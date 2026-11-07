// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 数值代入求值器（确定性，禁 eval）
//!
//! # 定位
//! 验证管线的定律代入步：把已知数值（命题主张 + 常量集）代入定律 formula
//! 的目标侧表达式，求出主张量的计算值。文法与 core/law_schema dimensional
//! 模块的公式侧文法**逐字一致**（`expr := factor (('*'|'/') factor)*`、
//! `factor := base ('^' ['-'] digits)?`、`base := identifier | number |
//! '(' expr ')'`）——禁扩展图灵面（无 `+`/函数调用/变量绑定）。
//!
//! # 确定性与有界性（DoS 面 fail-fast）
//! - 全程 f64 IEEE-754 确定性运算；每步算术后检查有限性，溢出/非有限即报错
//!   （不静默产生 inf/NaN 传播）；
//! - 幂次为整数（文法 `'-'? digits`），按平方法循环展开（checked，无平台
//!   libm 依赖）；
//! - 括号嵌套深度上限 [`MAX_PAREN_DEPTH`]，表达式长度上限
//!   [`MAX_EXPR_LEN`]，数值字面量幅值上限 [`MAX_LITERAL_MAGNITUDE`]；
//!   超限即显式报错，不放任失控递归或数值失控。

use std::collections::BTreeMap;

/// 表达式长度上限（超出 fail-fast，DoS 面硬上限）
pub const MAX_EXPR_LEN: usize = 512;

/// 括号嵌套深度上限（与 dimensional 模块同律）
pub const MAX_PAREN_DEPTH: usize = 64;

/// 单表达式标识符数上限（代入选集扫描的复杂度硬上限）
pub const MAX_IDENTS_PER_EXPR: usize = 32;

/// 数值字面量幅值上限（|literal| 超出 fail-fast）
pub const MAX_LITERAL_MAGNITUDE: f64 = 1e100;

/// 表达式中的标识符清单（出现序去重；超 [`MAX_IDENTS_PER_EXPR`] 报错）。
///
/// 用于代入前预检：表达式引用的符号必须全部可解析。
pub fn expression_identifiers(expr: &str) -> Result<Vec<String>, String> {
    let bytes = expr.as_bytes();
    let mut idents: Vec<String> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let name = expr[start..i].to_string();
            if !idents.contains(&name) {
                idents.push(name);
            }
            if idents.len() > MAX_IDENTS_PER_EXPR {
                return Err(format!("表达式标识符数超上限 {MAX_IDENTS_PER_EXPR}"));
            }
        } else {
            i += 1;
        }
    }
    Ok(idents)
}

struct NumParser<'a> {
    src: &'a str,
    pos: usize,
    depth: usize,
}

impl<'a> NumParser<'a> {
    fn new(src: &'a str) -> Result<Self, String> {
        if src.len() > MAX_EXPR_LEN {
            return Err(format!("表达式长度 {} 超上限 {MAX_EXPR_LEN}", src.len()));
        }
        Ok(NumParser {
            src,
            pos: 0,
            depth: 0,
        })
    }

    fn peek(&self) -> Option<u8> {
        self.src.as_bytes().get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.pos += 1;
        }
    }

    /// `expr := factor (('*' | '/') factor)*`
    fn parse_expr(
        &mut self,
        resolve: &mut dyn FnMut(&str) -> Result<f64, String>,
    ) -> Result<f64, String> {
        let mut acc = self.parse_factor(resolve)?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b'*') => {
                    self.pos += 1;
                    let rhs = self.parse_factor(resolve)?;
                    acc = checked_mul(acc, rhs)?;
                }
                Some(b'/') => {
                    self.pos += 1;
                    let rhs = self.parse_factor(resolve)?;
                    acc = checked_div(acc, rhs)?;
                }
                _ => return Ok(acc),
            }
        }
    }

    /// `factor := base ('^' ['-'] digits)?`
    fn parse_factor(
        &mut self,
        resolve: &mut dyn FnMut(&str) -> Result<f64, String>,
    ) -> Result<f64, String> {
        self.skip_ws();
        let base = self.parse_base(resolve)?;
        self.skip_ws();
        if self.peek() == Some(b'^') {
            self.pos += 1;
            let exp = self.parse_exponent()?;
            return checked_pow(base, exp);
        }
        Ok(base)
    }

    /// `base := 数值字面量 | 标识符（叶子解析）| '(' expr ')'`
    fn parse_base(
        &mut self,
        resolve: &mut dyn FnMut(&str) -> Result<f64, String>,
    ) -> Result<f64, String> {
        self.skip_ws();
        match self.peek() {
            Some(b) if b.is_ascii_digit() => self.consume_number(),
            Some(b) if b.is_ascii_alphabetic() || b == b'_' => {
                let start = self.pos;
                self.pos += 1;
                while matches!(self.peek(), Some(b) if b.is_ascii_alphanumeric() || b == b'_') {
                    self.pos += 1;
                }
                resolve(&self.src[start..self.pos])
            }
            Some(b'(') => self.parse_paren_subexpr(resolve),
            Some(b) if b.is_ascii() => Err(format!("非法记号 `{}`", b as char)),
            Some(_) => Err("非 ASCII 记号（仅允许 ASCII 标识符与运算符）".to_string()),
            None => Err("表达式意外结束".to_string()),
        }
    }

    /// 括号子表达式（整体作因子，可再接幂次）
    fn parse_paren_subexpr(
        &mut self,
        resolve: &mut dyn FnMut(&str) -> Result<f64, String>,
    ) -> Result<f64, String> {
        self.depth += 1;
        if self.depth > MAX_PAREN_DEPTH {
            return Err(format!("括号嵌套过深（上限 {MAX_PAREN_DEPTH} 层）"));
        }
        self.pos += 1; // 消费 '('
        let value = self.parse_expr(resolve)?;
        self.skip_ws();
        if self.peek() != Some(b')') {
            return Err("括号子表达式缺右括号 `)`".to_string());
        }
        self.pos += 1; // 消费 ')'
        self.depth -= 1;
        Ok(value)
    }

    /// `number := digits ('.' digits)?`（幅值上限内；无科学计数记法——
    /// 与 dimensional 公式侧文法逐字一致，数量级经字面量乘因子表达）
    fn consume_number(&mut self) -> Result<f64, String> {
        let start = self.pos;
        while matches!(self.peek(), Some(b) if b.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !matches!(self.peek(), Some(b) if b.is_ascii_digit()) {
                return Err("数值小数点后缺数字".to_string());
            }
            while matches!(self.peek(), Some(b) if b.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        let text = &self.src[start..self.pos];
        let value: f64 = text
            .parse()
            .map_err(|_| format!("数值字面量 `{text}` 不可解析"))?;
        if !value.is_finite() || value.abs() > MAX_LITERAL_MAGNITUDE {
            return Err(format!(
                "数值字面量 `{text}` 越界（|v| ≤ {MAX_LITERAL_MAGNITUDE:e}）"
            ));
        }
        Ok(value)
    }

    /// `exponent := ['-'] digits`（|exp| ≤ 127，与 dimensional 同律）
    fn parse_exponent(&mut self) -> Result<i32, String> {
        self.skip_ws();
        let negative = if self.peek() == Some(b'-') {
            self.pos += 1;
            true
        } else {
            false
        };
        let start = self.pos;
        while matches!(self.peek(), Some(b) if b.is_ascii_digit()) {
            self.pos += 1;
        }
        if start == self.pos {
            return Err("`^` 后缺整数幂次".to_string());
        }
        let digits = &self.src[start..self.pos];
        let magnitude: i32 = digits
            .parse()
            .map_err(|_| format!("幂次 `{digits}` 越界"))?;
        if magnitude > 127 {
            return Err(format!("幂次 `{digits}` 越界（|exp| ≤ 127）"));
        }
        Ok(if negative { -magnitude } else { magnitude })
    }
}

// ============================================================================
// checked 算术（溢出/非有限 → 显式报错，不静默传播 inf/NaN）
// ============================================================================

fn checked_mul(a: f64, b: f64) -> Result<f64, String> {
    let v = a * b;
    if !v.is_finite() || v.abs() > MAX_LITERAL_MAGNITUDE * 1e8 {
        return Err(format!("乘法结果非有限或越界: {a:e} * {b:e}"));
    }
    Ok(v)
}

fn checked_div(a: f64, b: f64) -> Result<f64, String> {
    if b == 0.0 {
        return Err(format!("除零: {a:e} / 0"));
    }
    let v = a / b;
    if !v.is_finite() || v.abs() > MAX_LITERAL_MAGNITUDE * 1e8 {
        return Err(format!("除法结果非有限或越界: {a:e} / {b:e}"));
    }
    Ok(v)
}

/// 整数幂（平方法循环展开，checked；负幂取倒数；无 libm 依赖，平台确定）
fn checked_pow(base: f64, exp: i32) -> Result<f64, String> {
    if base == 0.0 && exp < 0 {
        return Err("零的负幂次（除零）".to_string());
    }
    let negative = exp < 0;
    let mut e = exp.unsigned_abs();
    let mut b = base;
    let mut acc = 1.0f64;
    while e > 0 {
        if e & 1 == 1 {
            acc = checked_mul(acc, b)?;
        }
        e >>= 1;
        if e > 0 {
            b = checked_mul(b, b)?;
        }
    }
    if negative {
        checked_div(1.0, acc)
    } else {
        Ok(acc)
    }
}

// ============================================================================
// 公共 API
// ============================================================================

/// 求值数值表达式：符号经 `resolve` 代入（缺失/越界符号由调用方报因）。
///
/// 文法与 dimensional 公式侧逐字一致；尾部未解析内容、文法外记号（如 `+`）
/// 均显式报错（不猜测语义，保持确定性）。
pub fn eval_numeric(
    expr: &str,
    resolve: &mut dyn FnMut(&str) -> Result<f64, String>,
) -> Result<f64, String> {
    let mut parser = NumParser::new(expr)?;
    let value = parser.parse_expr(resolve)?;
    parser.skip_ws();
    if parser.pos != expr.len() {
        return Err(format!("表达式尾随未解析内容 `{}`", &expr[parser.pos..]));
    }
    Ok(value)
}

/// 判定表达式是否为「单个标识符」（直接代入形态：目标量独占一侧）。
///
/// 用于定律代入的 MVP 形态判定（无符号重排：目标侧必须是单符号裸形态）。
pub fn is_bare_identifier(expr: &str) -> bool {
    let trimmed = expr.trim();
    let bytes = trimmed.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    if !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_') {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

/// 符号取值表（law 代入的已知值：命题主张优先，常量集兜底）
pub type Substitution<'a> = BTreeMap<&'a str, f64>;

#[cfg(test)]
mod tests {
    use super::*;

    fn const_map<'a>(pairs: &'a [(&'a str, f64)]) -> Substitution<'a> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn eval_basic_arithmetic() {
        let sub = const_map(&[("m", 2.0), ("a", 3.0)]);
        let mut resolve = |name: &str| -> Result<f64, String> {
            sub.get(name)
                .copied()
                .ok_or_else(|| format!("符号 `{name}` 无已知值"))
        };
        assert_eq!(eval_numeric("m * a", &mut resolve).unwrap(), 6.0);
        assert_eq!(eval_numeric("(1/2)*m*a^2", &mut resolve).unwrap(), 9.0);
        assert_eq!(eval_numeric("m^3", &mut resolve).unwrap(), 8.0);
        assert_eq!(eval_numeric("m^-1", &mut resolve).unwrap(), 0.5);
        assert_eq!(eval_numeric("2.5", &mut resolve).unwrap(), 2.5);
        assert_eq!(eval_numeric("a / m", &mut resolve).unwrap(), 1.5);
    }

    #[test]
    fn eval_scientific_magnitude_via_literal_factor() {
        // G = 6.674e-11 形态在文法内表达为 6.674 * 10^-11（1e-11 拆因子）
        let mut resolve = |_: &str| -> Result<f64, String> { Err("无符号".to_string()) };
        let v = eval_numeric("6.674 * 10^-11", &mut resolve).unwrap();
        assert!((v - 6.674e-11).abs() < 1e-26, "v = {v:e}");
    }

    #[test]
    fn eval_rejects_bad_syntax_and_overflow() {
        // 语法面测试：符号恒可解析（值恒 1），使报错只来自文法面
        let mut resolve = |_: &str| -> Result<f64, String> { Ok(1.0) };
        assert!(eval_numeric("m + a", &mut resolve)
            .unwrap_err()
            .contains("尾随未解析内容"));
        assert!(eval_numeric("+m", &mut resolve)
            .unwrap_err()
            .contains("非法记号"));
        assert!(eval_numeric("m *", &mut resolve)
            .unwrap_err()
            .contains("意外结束"));
        assert!(eval_numeric("m^", &mut resolve)
            .unwrap_err()
            .contains("缺整数幂次"));
        assert!(eval_numeric("m^200", &mut resolve)
            .unwrap_err()
            .contains("越界"));
        // 科学计数记号在文法外（数量级经字面量乘因子表达）：尾随内容报错
        assert!(eval_numeric("0.5e3", &mut resolve)
            .unwrap_err()
            .contains("尾随未解析内容"));
        assert!(eval_numeric("1.", &mut resolve)
            .unwrap_err()
            .contains("小数点后缺数字"));

        let big = const_map(&[("x", 1e100), ("y", 1e100)]);
        let mut resolve_big = |name: &str| -> Result<f64, String> {
            big.get(name).copied().ok_or_else(|| "缺".to_string())
        };
        assert!(eval_numeric("x * y", &mut resolve_big)
            .unwrap_err()
            .contains("非有限或越界"));

        let zero = const_map(&[("z", 0.0), ("a", 1.0)]);
        let mut resolve_zero = |name: &str| -> Result<f64, String> {
            zero.get(name).copied().ok_or_else(|| "缺".to_string())
        };
        assert!(eval_numeric("a / z", &mut resolve_zero)
            .unwrap_err()
            .contains("除零"));
    }

    #[test]
    fn eval_enforces_hard_limits() {
        let mut resolve = |name: &str| -> Result<f64, String> {
            if name == "x" {
                Ok(1.0)
            } else {
                Err(format!("符号 `{name}` 无已知值"))
            }
        };
        let long = "x*".repeat(300) + "x";
        assert!(eval_numeric(&long, &mut resolve)
            .unwrap_err()
            .contains("超上限"));

        let deep = format!("{}x{}", "(".repeat(65), ")".repeat(65));
        assert!(eval_numeric(&deep, &mut resolve)
            .unwrap_err()
            .contains("嵌套过深"));

        let many: String = (0..40)
            .map(|i| format!("x{i}"))
            .collect::<Vec<_>>()
            .join("*");
        let err = expression_identifiers(&many).unwrap_err();
        assert!(err.contains("超上限"), "err = {err}");
    }

    #[test]
    fn expression_identifiers_dedup_and_order() {
        let idents = expression_identifiers("m * a * m * (a^2)").unwrap();
        assert_eq!(idents, vec!["m".to_string(), "a".to_string()]);
        assert!(is_bare_identifier("  F  "));
        assert!(is_bare_identifier("m1"));
        assert!(!is_bare_identifier("m * a"));
        assert!(!is_bare_identifier("2.0"));
        assert!(!is_bare_identifier(""));
        assert!(!is_bare_identifier("1m"));
    }
}

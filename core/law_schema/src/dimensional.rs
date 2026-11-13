// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! law_set 量纲一致性校验（能力层，可选启用）
//!
//! # 定位
//! 结构层（`check_formula_structure`）保证等式形态与符号声明完整性；本模块在其上
//! 增可选的量纲一致性校验：
//! - law 的 formula 引用符号**全部**有 si_units 且可解析 → 启用校验，
//!   等式两侧量纲不一致即硬拒收（报因含两侧量纲向量）；
//! - 任一符号缺 si_units 或单位表达式不可解析 → 跳过该校验并记 warning 级结果，
//!   不硬拒（兼容 G 类纯数学定律与自定义单位）。
//!
//! # 量纲模型（自制轻量代数，零外部依赖）
//! SI 七基本量纲 + 货币维的 i8 幂次向量 [M, L, T, I, Θ, N, J, C]
//! （质量/长度/时间/电流/热力学温度/物质的量/发光强度/货币——货币维为
//! SI 七量纲外的扩展基本量纲，异币种（CNY/USD/…）同归一维；币种间量值
//! 换算不归本层，本表只容量纲不容量值）。
//! - 乘除 = 分量加减；整数幂 = 分量倍乘（checked，越界按不可解析处理）；
//! - 数值常量因子量纲中性（量纲一）；
//! - 平面角/立体角按 SI 口径量纲一（rad/sr/deg 不引入辅助量纲）。
//!
//! # 表达式文法（formula 侧与 si_units 侧共用）
//! ```text
//! expr   := term (('+' | '-') term)*      ← 加减要求两侧操作分量纲一致
//! term   := factor (('*' | '/') factor)*
//! factor := ('-')* base ('^' ['-'] digits)?   ← 一元负号为纯记号面（[-v] = v）
//! base   := identifier | number | '(' expr ')'
//! ```
//! 括号子表达式按同一文法递归求值（可嵌套，深度上限 [`MAX_PAREN_DEPTH`]）；
//! 加减为量纲敏感运算：两侧操作分量纲一致方可加减（一致时结果量纲不变），
//! 不一致即量纲违例硬拒收（报因含两侧子表达式与其量纲向量渲染）——混加
//! （如米/元）不再落入「文法外记号跳过」的盲区。乘除照旧量纲代数；
//! 结构层已保证 formula 无判定形态运算符且引用符号已声明；本模块对文法外
//! 记号不猜测语义——按不可解析跳过，保持确定性。
//!
//! # 选型注记
//! 单位解析未引入 crates.io 单位库（`uom`/`pint` 系均为编译期类型层单位系统，
//! 无运行时单位表达式字符串解析能力，且会给 schema 门禁 crate 引入重依赖）；
//! 本模块以约三百行的确定性代数覆盖 si 基本量纲表达需求。

use std::collections::{BTreeMap, BTreeSet};

use super::Law;

// ============================================================================
// 基本量纲向量（SI 七量纲 + 货币维）
// ============================================================================

/// 基本量纲幂次向量 [M, L, T, I, Θ, N, J, C]（SI 七量纲 + 货币维 Currency）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DimensionVector(pub [i8; 8]);

/// 基本量纲的规范单位记号（渲染用，位序与向量分量一致；货币维渲染为
/// ASCII 记号 `currency`，与表达式文法的 ASCII 标识符口径一致）
const BASE_UNIT_SYMBOLS: [&str; 8] = ["kg", "m", "s", "A", "K", "mol", "cd", "currency"];

impl DimensionVector {
    /// 量纲一（全零向量）
    pub const DIMENSIONLESS: Self = Self([0; 8]);

    fn checked_add(self, other: Self) -> Option<Self> {
        let mut out = [0i8; 8];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = self.0[i].checked_add(other.0[i])?;
        }
        Some(Self(out))
    }

    fn checked_sub(self, other: Self) -> Option<Self> {
        let mut out = [0i8; 8];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = self.0[i].checked_sub(other.0[i])?;
        }
        Some(Self(out))
    }

    fn checked_pow(self, exp: i16) -> Option<Self> {
        let exp = i8::try_from(exp).ok()?;
        let mut out = [0i8; 8];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = self.0[i].checked_mul(exp)?;
        }
        Some(Self(out))
    }

    /// 规范渲染：非零分量展开为「基本单位记号^幂次」乘积（如 `kg*m*s^-2`）；
    /// 全零向量渲染为 `1`（量纲一）。渲染串即两侧量纲向量的报因形态。
    pub fn render(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for (i, sym) in BASE_UNIT_SYMBOLS.iter().enumerate() {
            match self.0[i] {
                0 => {}
                1 => parts.push((*sym).to_string()),
                e => parts.push(format!("{sym}^{e}")),
            }
        }
        if parts.is_empty() {
            "1".to_string()
        } else {
            parts.join("*")
        }
    }
}

// ============================================================================
// SI 单位表（记号 → 量纲向量；相干单位制口径，只容量纲不容量值换算）
// ============================================================================

/// SI 单位记号（含货币记号）→ 八基本量纲幂次。未知记号返回 `None`（调用方按「不可解析」
/// 跳过处理，warning 级不拒收）。记号区分大小写（SI 符号口径）。
fn unit_dimensions(name: &str) -> Option<DimensionVector> {
    let dims: [i8; 8] = match name {
        // 基本单位（m/kg/s/A/K/mol/cd）+ 常用全名别名
        "m" | "meter" | "metre" => [0, 1, 0, 0, 0, 0, 0, 0],
        "kg" | "kilogram" => [1, 0, 0, 0, 0, 0, 0, 0],
        "g" | "gram" | "gramme" => [1, 0, 0, 0, 0, 0, 0, 0],
        "s" | "second" | "sec" => [0, 0, 1, 0, 0, 0, 0, 0],
        "A" | "ampere" | "amp" => [0, 0, 0, 1, 0, 0, 0, 0],
        "K" | "kelvin" => [0, 0, 0, 0, 1, 0, 0, 0],
        "mol" | "mole" => [0, 0, 0, 0, 0, 1, 0, 0],
        "cd" | "candela" => [0, 0, 0, 0, 0, 0, 1, 0],
        // 相干导出单位
        "N" | "newton" => [1, 1, -2, 0, 0, 0, 0, 0],
        "Pa" | "pascal" => [1, -1, -2, 0, 0, 0, 0, 0],
        "J" | "joule" => [1, 2, -2, 0, 0, 0, 0, 0],
        "W" | "watt" => [1, 2, -3, 0, 0, 0, 0, 0],
        "Hz" | "hertz" => [0, 0, -1, 0, 0, 0, 0, 0],
        "C" | "coulomb" => [0, 0, 1, 1, 0, 0, 0, 0],
        "V" | "volt" => [1, 2, -3, -1, 0, 0, 0, 0],
        "F" | "farad" => [-1, -2, 4, 2, 0, 0, 0, 0],
        "ohm" => [1, 2, -3, -2, 0, 0, 0, 0],
        "S" | "siemens" => [-1, -2, 3, 2, 0, 0, 0, 0],
        "Wb" | "weber" => [1, 2, -2, -1, 0, 0, 0, 0],
        "T" | "tesla" => [1, 0, -2, -1, 0, 0, 0, 0],
        "H" | "henry" => [1, 2, -2, -2, 0, 0, 0, 0],
        "lm" | "lumen" => [0, 0, 0, 0, 0, 0, 1, 0],
        "lx" | "lux" => [0, -2, 0, 0, 0, 0, 1, 0],
        "Bq" | "becquerel" => [0, 0, -1, 0, 0, 0, 0, 0],
        "Gy" | "gray" => [0, 2, -2, 0, 0, 0, 0, 0],
        "Sv" | "sievert" => [0, 2, -2, 0, 0, 0, 0, 0],
        "kat" | "katal" => [0, 0, -1, 0, 0, 1, 0, 0],
        // 量纲一（SI 平面角/立体角口径）
        "rad" | "radian" | "sr" | "steradian" | "deg" | "degree" => [0; 8],
        // 常用接受单位（量纲与相干单位一致，不换算量值）
        "l" | "L" | "liter" | "litre" => [0, 3, 0, 0, 0, 0, 0, 0],
        "min" | "minute" | "h" | "hour" | "day" | "d" => [0, 0, 1, 0, 0, 0, 0, 0],
        "t" | "tonne" => [1, 0, 0, 0, 0, 0, 0, 0],
        "bar" | "atm" => [1, -1, -2, 0, 0, 0, 0, 0],
        "eV" | "electronvolt" => [1, 2, -2, 0, 0, 0, 0, 0],
        // 货币维（异币种同归一维；记号取常用 ISO 货币代码与英文/拼音别名，
        // 币种间量值换算不归本层——本表只容量纲不容量值，换算律管量值）
        "CNY" | "cny" | "yuan" | "RMB" | "rmb" => [0, 0, 0, 0, 0, 0, 0, 1],
        "USD" | "usd" | "dollar" | "dollars" => [0, 0, 0, 0, 0, 0, 0, 1],
        "EUR" | "eur" | "euro" => [0, 0, 0, 0, 0, 0, 0, 1],
        "GBP" | "gbp" | "pound" => [0, 0, 0, 0, 0, 0, 0, 1],
        "JPY" | "jpy" | "yen" => [0, 0, 0, 0, 0, 0, 0, 1],
        "HKD" | "hkd" | "CHF" | "chf" | "franc" | "money" | "currency" => [0, 0, 0, 0, 0, 0, 0, 1],
        _ => return None,
    };
    Some(DimensionVector(dims))
}

// ============================================================================
// 表达式求值（formula 侧与 si_units 侧共用文法）
// ============================================================================

/// 因子叶子解析：标识符 → 量纲向量；`Err` = 该标识符不可解析（含原因）
type LeafResolver<'a> = &'a mut dyn FnMut(&str) -> Result<DimensionVector, String>;

/// 括号嵌套深度上限（formula 为治理侧提交字符串：超出即显式报错，
/// 不放任失控递归打爆栈，保持 fail-fast 确定性口径）
const MAX_PAREN_DEPTH: usize = 64;

/// 加减量纲违例报因前缀（parse_expr Err 分派标记：check_law 据此前缀把
/// 该类失败归入 [`DimensionalVerdict::AdditiveMismatch`] 硬拒收，
/// 与「不可解析跳过」分野）
const ADDITIVE_MISMATCH_PREFIX: &str = "加减两侧操作分量纲不一致";

/// 函数自变量量纲违例报因前缀（严口径：白名单函数自变量须量纲一；
/// check_law 据此前缀把该类失败归入 [`DimensionalVerdict::Mismatch`]
/// 硬拒收，与「不可解析跳过」分野；law 级豁免键
/// `metadata.fn_arg_dimless_exempt` 置真时按量纲一处理，见 eval_side）
const FUNC_ARG_MISMATCH_PREFIX: &str = "函数自变量量纲违例";

/// 函数调用白名单（池实况扫定 2026-11-13：log 43 / sin 44 / cos 31 /
/// exp 13 / sqrt 11 / tan 4 / log2 2 / ln 1 / sum 1；未知函数名 fail-fast）。
/// 量纲规则见 `apply_fn`：除 `sum`（结果维 = 自变量维）外结果均为量纲一。
const FUNC_WHITELIST: [&str; 9] = [
    "log", "log2", "ln", "exp", "sin", "cos", "tan", "sqrt", "sum",
];

struct ExprParser<'a> {
    src: &'a str,
    pos: usize,
    /// 当前括号嵌套深度（parse_base 括号分支递归计数）
    depth: usize,
    /// 函数自变量量纲一豁免（law.metadata.fn_arg_dimless_exempt，缺省 false）
    fn_arg_exempt: bool,
}

impl<'a> ExprParser<'a> {
    fn new(src: &'a str) -> Self {
        ExprParser {
            src,
            pos: 0,
            depth: 0,
            fn_arg_exempt: false,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.src.as_bytes().get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.pos += 1;
        }
    }

    /// `expr := term (('+' | '-') term)*`——加减为量纲敏感运算：两侧操作分
    /// 量纲一致方可加减（一致时结果量纲不变），不一致即量纲违例（报因以
    /// [`ADDITIVE_MISMATCH_PREFIX`] 起头，check_law 分派为硬拒收）。
    fn parse_expr(&mut self, leaf: LeafResolver<'_>) -> Result<DimensionVector, String> {
        let start = self.pos;
        let acc = self.parse_term(leaf)?;
        loop {
            self.skip_ws();
            let op_pos = self.pos;
            match self.peek() {
                Some(b'+') | Some(b'-') => {
                    self.pos += 1;
                    self.skip_ws();
                    let rhs_start = self.pos;
                    let rhs = self.parse_term(leaf)?;
                    if rhs != acc {
                        let lhs_text = self.src[start..op_pos].trim().to_string();
                        let rhs_text = self.src[rhs_start..self.pos].trim().to_string();
                        return Err(format!(
                            "{ADDITIVE_MISMATCH_PREFIX}: `{lhs_text}` 展开为 {} 与 \
                             `{rhs_text}` 展开为 {}",
                            acc.render(),
                            rhs.render()
                        ));
                    }
                }
                _ => return Ok(acc),
            }
        }
    }

    /// `term := factor (('*' | '/') factor)*`
    fn parse_term(&mut self, leaf: LeafResolver<'_>) -> Result<DimensionVector, String> {
        let mut acc = self.parse_factor(leaf)?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b'*') => {
                    self.pos += 1;
                    let rhs = self.parse_factor(leaf)?;
                    acc = acc
                        .checked_add(rhs)
                        .ok_or_else(|| "量纲分量溢出（i8 越界）".to_string())?;
                }
                Some(b'/') => {
                    self.pos += 1;
                    let rhs = self.parse_factor(leaf)?;
                    acc = acc
                        .checked_sub(rhs)
                        .ok_or_else(|| "量纲分量溢出（i8 越界）".to_string())?;
                }
                _ => return Ok(acc),
            }
        }
    }

    /// `factor := ('-')* base ('^' exponent)?`——一元负号：纯记号面，量纲
    /// 向量关于取负对称（[-v] = v），消费记号不改维；连写负号（`--x`）同法。
    fn parse_factor(&mut self, leaf: LeafResolver<'_>) -> Result<DimensionVector, String> {
        self.skip_ws();
        while self.peek() == Some(b'-') {
            self.pos += 1;
            self.skip_ws();
        }
        let base = self.parse_base(leaf)?;
        self.skip_ws();
        if self.peek() == Some(b'^') {
            self.pos += 1;
            self.skip_ws();
            if self.peek() == Some(b'(') {
                // 记法扩展：量纲一底（如自然常数 e）的括号指数——指数须量纲
                // 一（严口径同函数自变量），结果量纲一。底非量纲一 = 符号指
                // 数（M1c 定性：量纲取决于指数运行时值，机验非确定性，文法
                // 不收，按不可解析跳过照录）。
                if base != DimensionVector::DIMENSIONLESS {
                    return Err(format!(
                        "非常数指数：底 `{}` 非量纲一，符号指数文法不支持",
                        base.render()
                    ));
                }
                let exp_dims = self.parse_paren_subexpr(leaf)?;
                if exp_dims != DimensionVector::DIMENSIONLESS {
                    return Err(format!(
                        "{FUNC_ARG_MISMATCH_PREFIX}: 括号指数展开为 {}，严口径要求量纲一",
                        exp_dims.render()
                    ));
                }
                return Ok(DimensionVector::DIMENSIONLESS);
            }
            let exp = self.parse_exponent()?;
            return base
                .checked_pow(exp)
                .ok_or_else(|| format!("幂次 {exp} 分量溢出（|exp| ≤ 127）"));
        }
        Ok(base)
    }

    /// 函数调用：`ident '(' expr (',' expr)* ')'`（白名单见
    /// [`FUNC_WHITELIST`]，实参数量按函数语义收紧）。实参括号纳入嵌套深度
    /// 计数，fail-fast 未知函数名。
    fn parse_fn_call(
        &mut self,
        name: &str,
        leaf: LeafResolver<'_>,
    ) -> Result<DimensionVector, String> {
        self.pos += 1; // 消费 '('
        self.depth += 1;
        if self.depth > MAX_PAREN_DEPTH {
            return Err(format!("括号嵌套过深（上限 {MAX_PAREN_DEPTH} 层）"));
        }
        let mut args: Vec<DimensionVector> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b')') {
            return Err(format!("函数 `{name}` 实参列表为空"));
        }
        loop {
            args.push(self.parse_expr(leaf)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b')') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(format!("函数 `{name}` 实参列表缺右括号 `)`")),
            }
        }
        self.depth -= 1;
        if args.len() != 1 {
            return Err(format!(
                "函数 `{name}` 实参数量 {} 不合规（白名单函数均为单参）",
                args.len()
            ));
        }
        self.apply_fn(name, &args)
    }

    /// 函数量纲规则（严口径）：
    /// - `log`/`log2`/`ln`/`exp`/`sin`/`cos`/`tan`：自变量须量纲一，结果量纲
    ///   一（物理惯例 [pX] = 1；违例经 law 级豁免键
    ///   `metadata.fn_arg_dimless_exempt` 置真时按量纲一处理）；
    /// - `sqrt`：自变量各分量须为偶数，结果 = 各分量半幂；
    /// - `sum`：Σ 结果维 = 自变量维（逐项同维性由单项求值代表，恒等返回）。
    fn apply_fn(
        &mut self,
        name: &str,
        args: &[DimensionVector],
    ) -> Result<DimensionVector, String> {
        let d = *args
            .first()
            .ok_or_else(|| format!("函数 `{name}` 缺实参"))?;
        match name {
            "sum" => Ok(d),
            "sqrt" => {
                let mut out = d.0;
                for slot in out.iter_mut() {
                    if *slot % 2 != 0 {
                        return Err(format!(
                            "{FUNC_ARG_MISMATCH_PREFIX}: 函数 `sqrt` 自变量展开为 {}，\
                             含奇幂分量无法取半幂",
                            d.render()
                        ));
                    }
                    *slot /= 2;
                }
                Ok(DimensionVector(out))
            }
            _ => {
                if !self.fn_arg_exempt && d != DimensionVector::DIMENSIONLESS {
                    return Err(format!(
                        "{FUNC_ARG_MISMATCH_PREFIX}: 函数 `{name}` 自变量展开为 {}，\
                         严口径要求量纲一",
                        d.render()
                    ));
                }
                Ok(DimensionVector::DIMENSIONLESS)
            }
        }
    }

    /// `base := 数值常量（量纲一）| 标识符（叶子解析）| '(' expr ')'（括号子表达式）`
    fn parse_base(&mut self, leaf: LeafResolver<'_>) -> Result<DimensionVector, String> {
        self.skip_ws();
        match self.peek() {
            Some(b) if b.is_ascii_digit() => {
                self.consume_number()?;
                Ok(DimensionVector::DIMENSIONLESS)
            }
            Some(b) if b.is_ascii_alphabetic() || b == b'_' => {
                let start = self.pos;
                self.pos += 1;
                while matches!(self.peek(), Some(b) if b.is_ascii_alphanumeric() || b == b'_') {
                    self.pos += 1;
                }
                let name = self.src[start..self.pos].to_string();
                self.skip_ws();
                if self.peek() == Some(b'(') && FUNC_WHITELIST.contains(&name.as_str()) {
                    return self.parse_fn_call(&name, leaf);
                }
                leaf(&name)
            }
            Some(b'(') => self.parse_paren_subexpr(leaf),
            Some(b) if b.is_ascii() => Err(format!("非法记号 `{}`", b as char)),
            Some(_) => Err("非 ASCII 记号（仅允许 ASCII 标识符与运算符）".to_string()),
            None => Err("表达式意外结束".to_string()),
        }
    }

    /// 括号子表达式：`'(' expr ')'`——括号内同一文法递归求值，天然支持嵌套；
    /// 整体作为一个因子（parse_factor 的 `^` 幂次可继续作用于其上）。
    fn parse_paren_subexpr(&mut self, leaf: LeafResolver<'_>) -> Result<DimensionVector, String> {
        self.depth += 1;
        if self.depth > MAX_PAREN_DEPTH {
            return Err(format!("括号嵌套过深（上限 {MAX_PAREN_DEPTH} 层）"));
        }
        self.pos += 1; // 消费 '('
        let dims = self.parse_expr(leaf)?;
        self.skip_ws();
        if self.peek() != Some(b')') {
            return Err("括号子表达式缺右括号 `)`".to_string());
        }
        self.pos += 1; // 消费 ')'
        self.depth -= 1;
        Ok(dims)
    }

    /// `number := digits ('.' digits)?`
    fn consume_number(&mut self) -> Result<(), String> {
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
        Ok(())
    }

    /// `exponent := ['-'] digits`（|exp| ≤ 127，i8 分量口径）
    fn parse_exponent(&mut self) -> Result<i16, String> {
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
        Ok(if negative {
            -magnitude as i16
        } else {
            magnitude as i16
        })
    }
}

/// 解析 si_units 单位表达式 → 基本量纲向量（SI 七量纲 + 货币维；如
/// `kg*m*s^-2`、`m*s^-1`、`joule*mol^-1*K^-1`、`CNY*s^-1`）。
/// 未知单位名/文法外记号/分量溢出均报错。
pub fn parse_unit_expr(expr: &str) -> Result<DimensionVector, String> {
    let mut parser = ExprParser::new(expr);
    let dims = parser.parse_expr(&mut |name| {
        unit_dimensions(name).ok_or_else(|| format!("未知单位 `{name}`"))
    })?;
    parser.skip_ws();
    if parser.pos != expr.len() {
        return Err(format!("表达式尾随未解析内容 `{}`", &expr[parser.pos..]));
    }
    Ok(dims)
}

/// 求值 formula 单侧表达式：符号量纲经 si_units 解析（带缓存，重复符号只解析一次）；
/// `fn_arg_exempt` = law 级函数自变量量纲一豁免（metadata.fn_arg_dimless_exempt）
fn eval_side(
    side: &str,
    si_units: &BTreeMap<String, String>,
    cache: &mut BTreeMap<String, DimensionVector>,
    fn_arg_exempt: bool,
) -> Result<DimensionVector, String> {
    let mut resolve = |name: &str| -> Result<DimensionVector, String> {
        if let Some(dims) = cache.get(name) {
            return Ok(*dims);
        }
        let expr = si_units
            .get(name)
            .ok_or_else(|| format!("符号 `{name}` 缺 si_units 声明"))?;
        let dims = parse_unit_expr(expr)
            .map_err(|e| format!("符号 `{name}` 的 si_units `{expr}` 不可解析: {e}"))?;
        cache.insert(name.to_string(), dims);
        Ok(dims)
    };
    let mut parser = ExprParser::new(side);
    parser.fn_arg_exempt = fn_arg_exempt;
    let dims = parser.parse_expr(&mut resolve)?;
    parser.skip_ws();
    if parser.pos != side.len() {
        return Err(format!("表达式尾随未解析内容 `{}`", &side[parser.pos..]));
    }
    Ok(dims)
}

// ============================================================================
// law 级量纲一致性校验（三态结论）
// ============================================================================

/// 量纲校验结论（能力层三态 + 加减违例四态）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DimensionalVerdict {
    /// 两侧量纲一致（含两侧均为量纲一）
    Consistent,
    /// 两侧量纲不一致——硬拒收（字段 = 两侧表达式与其规范量纲向量渲染）
    Mismatch { lhs: String, rhs: String },
    /// 加减两侧操作分量纲不一致（等式内部混加，如米/元）——硬拒收
    /// （字段 = 报因串，含两侧子表达式与其规范量纲向量渲染）
    AdditiveMismatch { report: String },
    /// 跳过：缺 si_units / 单位表达式不可解析 / 文法外记号（warning 级，不拒收）
    Skipped(String),
}

/// 单条定律的量纲一致性校验。
///
/// 启用条件 = formula 引用符号全部有 si_units（缺者整体跳过并给出原因，
/// 兼容 G 类纯数学定律）；两侧求值均成功才比较，任一侧不可解析即跳过。
/// 同维零特例：`metadata.same_dim_zero` 显式声明且 rhs 为严格字面单数字
/// `0` 时，零值量纲不设限（与 lhs 同纲），lhs 解析成功即 Consistent 纯态
/// （缺省 false = 现行双 eval 比对路径零变化）。
pub fn check_law(law: &Law) -> DimensionalVerdict {
    // 等式形态防御性重查（结构层已保证；不满足时结构层已报因，此处跳过）
    let Some((lhs_raw, rhs_raw)) = law.formula.split_once('=') else {
        return DimensionalVerdict::Skipped("formula 无 `=` 连接符".to_string());
    };
    if lhs_raw.contains('=') || rhs_raw.contains('=') {
        return DimensionalVerdict::Skipped("formula 含多个 `=` 连接符".to_string());
    }

    // 启用条件：formula 引用符号全部有 si_units（缺者列出后跳过，不硬拒）
    let missing: BTreeSet<String> = super::formula_identifiers(law.formula.as_str())
        .into_iter()
        .filter(|name| !law.si_units.contains_key(name.as_str()))
        .collect();
    if !missing.is_empty() {
        let names: Vec<String> = missing.into_iter().collect();
        return DimensionalVerdict::Skipped(format!(
            "符号 [{}] 缺 si_units 声明",
            names.join(", ")
        ));
    }

    let fn_arg_exempt = law
        .metadata
        .as_ref()
        .and_then(|m| m.fn_arg_dimless_exempt)
        .unwrap_or(false);
    let mut cache = BTreeMap::new();

    // 同维零特例（law.metadata.same_dim_zero 显式声明激活；缺省 false = 现行
    // 路径零改动）：rhs 侧严格字面单数字 `0` 时零值量纲不设限（物理上零与
    // lhs 同纲），lhs 解析成功即 Consistent 纯态，Err 走现行报因分派。
    // `0.0`/`-0`/常量求值零/零表达式均不激活（保守最小面）；rhs 非 `0` 时
    // 特例不生效，照现行双 eval 比对路径。
    let same_dim_zero = law
        .metadata
        .as_ref()
        .and_then(|m| m.same_dim_zero)
        .unwrap_or(false);
    if same_dim_zero && rhs_raw.trim() == "0" {
        return match eval_side(lhs_raw, &law.si_units, &mut cache, fn_arg_exempt) {
            Ok(_) => DimensionalVerdict::Consistent,
            Err(reason) if reason.starts_with(ADDITIVE_MISMATCH_PREFIX) => {
                DimensionalVerdict::AdditiveMismatch { report: reason }
            }
            Err(reason) if reason.starts_with(FUNC_ARG_MISMATCH_PREFIX) => {
                DimensionalVerdict::Mismatch {
                    lhs: reason,
                    rhs: "量纲一（函数自变量严口径）".to_string(),
                }
            }
            Err(reason) => DimensionalVerdict::Skipped(reason),
        };
    }

    let lhs = match eval_side(lhs_raw, &law.si_units, &mut cache, fn_arg_exempt) {
        Ok(dims) => dims,
        Err(reason) if reason.starts_with(ADDITIVE_MISMATCH_PREFIX) => {
            return DimensionalVerdict::AdditiveMismatch { report: reason };
        }
        Err(reason) if reason.starts_with(FUNC_ARG_MISMATCH_PREFIX) => {
            return DimensionalVerdict::Mismatch {
                lhs: reason,
                rhs: "量纲一（函数自变量严口径）".to_string(),
            };
        }
        Err(reason) => return DimensionalVerdict::Skipped(reason),
    };
    let rhs = match eval_side(rhs_raw, &law.si_units, &mut cache, fn_arg_exempt) {
        Ok(dims) => dims,
        Err(reason) if reason.starts_with(ADDITIVE_MISMATCH_PREFIX) => {
            return DimensionalVerdict::AdditiveMismatch { report: reason };
        }
        Err(reason) if reason.starts_with(FUNC_ARG_MISMATCH_PREFIX) => {
            return DimensionalVerdict::Mismatch {
                lhs: reason,
                rhs: "量纲一（函数自变量严口径）".to_string(),
            };
        }
        Err(reason) => return DimensionalVerdict::Skipped(reason),
    };

    if lhs == rhs {
        DimensionalVerdict::Consistent
    } else {
        DimensionalVerdict::Mismatch {
            lhs: format!("左侧 `{}` 展开为 {}", lhs_raw.trim(), lhs.render()),
            rhs: format!("右侧 `{}` 展开为 {}", rhs_raw.trim(), rhs.render()),
        }
    }
}

// ============================================================================
// 测试（向量代数 + 单位表达式解析 + 三态结论）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dims(v: [i8; 8]) -> DimensionVector {
        DimensionVector(v)
    }

    // ── 单位表达式解析 ──

    #[test]
    fn unit_expr_parses_to_base_vectors() {
        assert_eq!(
            parse_unit_expr("kg*m*s^-2").unwrap(),
            dims([1, 1, -2, 0, 0, 0, 0, 0])
        );
        assert_eq!(
            parse_unit_expr("m*s^-1").unwrap(),
            dims([0, 1, -1, 0, 0, 0, 0, 0])
        );
        assert_eq!(
            parse_unit_expr("m^3").unwrap(),
            dims([0, 3, 0, 0, 0, 0, 0, 0])
        );
        assert_eq!(
            parse_unit_expr("joule*mol^-1*K^-1").unwrap(),
            dims([1, 2, -2, 0, -1, -1, 0, 0])
        );
        assert_eq!(
            parse_unit_expr("newton").unwrap(),
            dims([1, 1, -2, 0, 0, 0, 0, 0])
        );
        assert_eq!(
            parse_unit_expr("kilogram").unwrap(),
            dims([1, 0, 0, 0, 0, 0, 0, 0])
        );
        assert_eq!(
            parse_unit_expr("kelvin").unwrap(),
            dims([0, 0, 0, 0, 1, 0, 0, 0])
        );
        assert_eq!(
            parse_unit_expr("pascal").unwrap(),
            dims([1, -1, -2, 0, 0, 0, 0, 0])
        );
        assert_eq!(
            parse_unit_expr("mol").unwrap(),
            dims([0, 0, 0, 0, 0, 1, 0, 0])
        );
        assert_eq!(
            parse_unit_expr("m/s").unwrap(),
            dims([0, 1, -1, 0, 0, 0, 0, 0])
        );
        assert_eq!(
            parse_unit_expr("1").unwrap(),
            DimensionVector::DIMENSIONLESS
        );
        assert_eq!(
            parse_unit_expr("0.5").unwrap(),
            DimensionVector::DIMENSIONLESS
        );
        assert_eq!(
            parse_unit_expr("rad").unwrap(),
            DimensionVector::DIMENSIONLESS
        );
    }

    #[test]
    fn unit_expr_parses_currency_dimension() {
        // 货币维：异币种同归一维（第 8 分量 = 货币幂次）
        let cny = parse_unit_expr("CNY").unwrap();
        let usd = parse_unit_expr("USD").unwrap();
        let yuan = parse_unit_expr("yuan").unwrap();
        assert_eq!(cny, usd);
        assert_eq!(usd, yuan);
        assert_eq!(cny, dims([0, 0, 0, 0, 0, 0, 0, 1]));
        // 组合表达式：货币流率（元/秒）
        assert_eq!(
            parse_unit_expr("CNY*s^-1").unwrap(),
            dims([0, 0, -1, 0, 0, 0, 0, 1])
        );
        // 乘积：单价 × 数量因子（量纲中性）仍为货币维
        assert_eq!(
            parse_unit_expr("2*yuan").unwrap(),
            dims([0, 0, 0, 0, 0, 0, 0, 1])
        );
    }

    #[test]
    fn unit_expr_whitespace_tolerant() {
        assert_eq!(
            parse_unit_expr(" kg * m * s^-2 ").unwrap(),
            dims([1, 1, -2, 0, 0, 0, 0, 0])
        );
    }

    #[test]
    fn unit_expr_rejects_unknown_names_and_bad_syntax() {
        assert!(parse_unit_expr("newtn").unwrap_err().contains("未知单位"));
        // 加减入文法：操作分量纲不等即量纲违例（m 长度 vs s 时间）
        assert!(parse_unit_expr("m + s")
            .unwrap_err()
            .contains("加减两侧操作分量纲不一致"));
        assert!(parse_unit_expr("+ m").unwrap_err().contains("非法记号"));
        assert!(parse_unit_expr("m^").unwrap_err().contains("缺整数幂次"));
        assert!(parse_unit_expr("m^200").unwrap_err().contains("越界"));
        assert!(parse_unit_expr("").unwrap_err().contains("意外结束"));
        assert!(parse_unit_expr("m *").unwrap_err().contains("意外结束"));
        assert!(parse_unit_expr("kg*m*s^-2extra")
            .unwrap_err()
            .contains("尾随未解析内容"));
        assert!(parse_unit_expr("kg·m")
            .unwrap_err()
            .contains("尾随未解析内容"));
        assert!(parse_unit_expr("0.")
            .unwrap_err()
            .contains("小数点后缺数字"));
    }

    // ── 括号子表达式 ──

    #[test]
    fn unit_expr_parenthesized_subexpressions() {
        // 括号整体作因子：可再接幂次
        assert_eq!(
            parse_unit_expr("(kg*m)^2").unwrap(),
            dims([2, 2, 0, 0, 0, 0, 0, 0])
        );
        // 括号内乘除混合
        assert_eq!(
            parse_unit_expr("kg*(m/s)").unwrap(),
            dims([1, 1, -1, 0, 0, 0, 0, 0])
        );
        // 括号内数值除法（量纲中性）
        assert_eq!(
            parse_unit_expr("(1/2)*kg").unwrap(),
            dims([1, 0, 0, 0, 0, 0, 0, 0])
        );
    }

    #[test]
    fn unit_expr_nested_parens() {
        assert_eq!(
            parse_unit_expr("(kg*(m/s))^2").unwrap(),
            dims([2, 2, -2, 0, 0, 0, 0, 0])
        );
        assert_eq!(
            parse_unit_expr("((kg))").unwrap(),
            dims([1, 0, 0, 0, 0, 0, 0, 0])
        );
    }

    #[test]
    fn unit_expr_paren_errors() {
        assert!(parse_unit_expr("(kg*m").unwrap_err().contains("缺右括号"));
        assert!(parse_unit_expr("kg)")
            .unwrap_err()
            .contains("尾随未解析内容"));
        let deep = format!("{}kg{}", "(".repeat(65), ")".repeat(65));
        assert!(parse_unit_expr(&deep).unwrap_err().contains("嵌套过深"));
    }

    #[test]
    fn check_law_parenthesized_formulas_consistent() {
        // 动能：括号内数值除法量纲中性，`(1/2)` 不改变量纲
        let kinetic = law_from_json(json!({
            "id": "kinetic", "formula": "E = (1/2)*m*v^2",
            "symbols": { "E": "动能", "m": "质量", "v": "速度" },
            "si_units": { "E": "kg*m^2*s^-2", "m": "kg", "v": "m*s^-1" },
            "max_version_depth": 1, "monotone_form": true
        }));
        assert_eq!(check_law(&kinetic), DimensionalVerdict::Consistent);

        // 万有引力：括号分子 `(m1*m2)` 作整体再除以 `r^2`
        let gravity = law_from_json(json!({
            "id": "gravity", "formula": "F = G*(m1*m2)/r^2",
            "symbols": { "F": "引力", "G": "引力常量", "m1": "质量一", "m2": "质量二", "r": "距离" },
            "si_units": { "F": "newton", "G": "m^3*kg^-1*s^-2", "m1": "kg", "m2": "kg", "r": "m" },
            "max_version_depth": 1, "monotone_form": true
        }));
        assert_eq!(check_law(&gravity), DimensionalVerdict::Consistent);

        // 嵌套括号：`((m*g)*h)/t` 与功率量纲一致
        let power = law_from_json(json!({
            "id": "power", "formula": "P = ((m*g)*h)/t",
            "symbols": { "P": "功率", "m": "质量", "g": "重力加速度", "h": "高度", "t": "时间" },
            "si_units": { "P": "watt", "m": "kg", "g": "m*s^-2", "h": "m", "t": "s" },
            "max_version_depth": 1, "monotone_form": true
        }));
        assert_eq!(check_law(&power), DimensionalVerdict::Consistent);
    }

    // ── 规范渲染 ──

    #[test]
    fn render_matches_base_symbol_order() {
        assert_eq!(dims([1, 1, -2, 0, 0, 0, 0, 0]).render(), "kg*m*s^-2");
        assert_eq!(dims([0, 3, 0, 0, 0, 0, 0, 0]).render(), "m^3");
        assert_eq!(DimensionVector::DIMENSIONLESS.render(), "1");
        assert_eq!(dims([0, 0, 0, 2, -1, 0, 0, 0]).render(), "A^2*K^-1");
        assert_eq!(dims([1, 0, -1, 0, 0, -1, 0, 0]).render(), "kg*s^-1*mol^-1");
        assert_eq!(dims([0, 0, 0, 0, 0, 0, 0, 1]).render(), "currency");
        assert_eq!(dims([0, 0, -1, 0, 0, 0, 0, 1]).render(), "s^-1*currency");
    }

    // ── checked 代数（溢出 → None → 按不可解析跳过）──

    #[test]
    fn checked_ops_overflow_to_none() {
        let big = dims([127, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(big.checked_add(big), None);
        assert_eq!(big.checked_pow(2), None);
        assert_eq!(
            dims([-128, 0, 0, 0, 0, 0, 0, 0]).checked_sub(dims([1, 0, 0, 0, 0, 0, 0, 0])),
            None
        );
    }

    // ── law 级三态结论 ──

    fn law_from_json(v: serde_json::Value) -> Law {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn check_law_consistent_mismatch_and_skip() {
        let consistent = law_from_json(json!({
            "id": "f_ma", "formula": "F = m * a",
            "symbols": { "F": "力", "m": "质量", "a": "加速度" },
            "si_units": { "F": "newton", "m": "kilogram", "a": "m*s^-2" },
            "max_version_depth": 1, "monotone_form": true
        }));
        assert_eq!(check_law(&consistent), DimensionalVerdict::Consistent);

        let mismatch = law_from_json(json!({
            "id": "f_mv", "formula": "F = m * v",
            "symbols": { "F": "力", "m": "质量", "v": "速度" },
            "si_units": { "F": "newton", "m": "kilogram", "v": "m*s^-1" },
            "max_version_depth": 1, "monotone_form": true
        }));
        match check_law(&mismatch) {
            DimensionalVerdict::Mismatch { lhs, rhs } => {
                assert!(lhs.contains("kg*m*s^-2"), "lhs = {lhs}");
                assert!(rhs.contains("m*s^-1"), "rhs = {rhs}");
            }
            other => panic!("应为 Mismatch: {other:?}"),
        }

        let skipped = law_from_json(json!({
            "id": "pure_math", "formula": "y = a * x",
            "symbols": { "y": "因变量", "a": "系数", "x": "自变量" },
            "si_units": {},
            "max_version_depth": 1, "monotone_form": true
        }));
        match check_law(&skipped) {
            DimensionalVerdict::Skipped(reason) => {
                assert!(reason.contains("缺 si_units"), "reason = {reason}");
                assert!(reason.contains('y'), "reason 须列全缺符号: {reason}");
            }
            other => panic!("应为 Skipped: {other:?}"),
        }
    }

    #[test]
    fn check_law_skips_on_unparseable_unit_and_bad_token() {
        let bad_unit = law_from_json(json!({
            "id": "typo", "formula": "F = m * a",
            "symbols": { "F": "力", "m": "质量", "a": "加速度" },
            "si_units": { "F": "newtn", "m": "kilogram", "a": "m*s^-2" },
            "max_version_depth": 1, "monotone_form": true
        }));
        match check_law(&bad_unit) {
            DimensionalVerdict::Skipped(reason) => {
                assert!(reason.contains("未知单位"), "reason = {reason}");
            }
            other => panic!("应为 Skipped: {other:?}"),
        }

        let bad_token = law_from_json(json!({
            "id": "plus", "formula": "y = a * +x",
            "symbols": { "y": "积", "a": "项一", "x": "项二" },
            "si_units": { "y": "m", "a": "m", "x": "m" },
            "max_version_depth": 1, "monotone_form": true
        }));
        match check_law(&bad_token) {
            DimensionalVerdict::Skipped(reason) => {
                assert!(reason.contains("非法记号"), "reason = {reason}");
            }
            other => panic!("应为 Skipped: {other:?}"),
        }

        let trailing = law_from_json(json!({
            "id": "adjacent", "formula": "y = a x",
            "symbols": { "y": "积", "a": "项一", "x": "项二" },
            "si_units": { "y": "m", "a": "m", "x": "m" },
            "max_version_depth": 1, "monotone_form": true
        }));
        match check_law(&trailing) {
            DimensionalVerdict::Skipped(reason) => {
                assert!(reason.contains("尾随未解析内容"), "reason = {reason}");
            }
            other => panic!("应为 Skipped: {other:?}"),
        }
    }

    #[test]
    fn check_law_additive_dimension_sensitivity() {
        // 加减一致：同维操作分量纲一致 → Consistent（加减不改变量纲）
        let ok = law_from_json(json!({
            "id": "sum", "formula": "y = a + x",
            "symbols": { "y": "和", "a": "项一", "x": "项二" },
            "si_units": { "y": "m", "a": "m", "x": "m" },
            "max_version_depth": 1, "monotone_form": true
        }));
        assert_eq!(check_law(&ok), DimensionalVerdict::Consistent);

        // 乘除与加减优先级：a + b*c 与 a 同维才通过（b*c 量纲代数先于加减比较）
        let prec = law_from_json(json!({
            "id": "energy", "formula": "E = m0 * c^2 + KE",
            "symbols": { "E": "总能量", "m0": "静质量", "c": "光速", "KE": "动能" },
            "si_units": { "E": "joule", "m0": "kilogram", "c": "m*s^-1", "KE": "joule" },
            "max_version_depth": 1, "monotone_form": true
        }));
        assert_eq!(check_law(&prec), DimensionalVerdict::Consistent);

        // 米/元混加：等式内部操作分量纲不等 → AdditiveMismatch 硬拒收
        // （报因含两侧子表达式与量纲向量渲染——混加盲区修复的直证）
        let mixed = law_from_json(json!({
            "id": "mix", "formula": "total = length + price",
            "symbols": { "total": "总额", "length": "长度", "price": "单价" },
            "si_units": { "total": "CNY", "length": "m", "price": "CNY*m^-1" },
            "max_version_depth": 1, "monotone_form": true
        }));
        match check_law(&mixed) {
            DimensionalVerdict::AdditiveMismatch { report } => {
                assert!(
                    report.contains("加减两侧操作分量纲不一致"),
                    "report = {report}"
                );
                assert!(report.contains("currency"), "report = {report}");
                assert!(report.contains("length"), "report = {report}");
            }
            other => panic!("应为 AdditiveMismatch: {other:?}"),
        }
    }

    #[test]
    fn unary_minus_is_pure_notation() {
        // 一元负号为纯记号面：[-v] = v，首位/连写/乘除链中均不改维
        assert!(parse_unit_expr("-kg*m*s^-2").is_ok());
        assert_eq!(
            parse_unit_expr("-kg*m*s^-2").unwrap(),
            parse_unit_expr("kg*m*s^-2").unwrap()
        );
        assert_eq!(
            parse_unit_expr("--kg*m*s^-2").unwrap(),
            parse_unit_expr("kg*m*s^-2").unwrap()
        );
        // 乘除链中部负号（此前在 parse_base 报「非法记号 -」）
        assert_eq!(
            parse_unit_expr("kg*-m*s^-2").unwrap(),
            parse_unit_expr("kg*m*s^-2").unwrap()
        );
        // 幂次负号共存不回归
        assert!(parse_unit_expr("-m*s^-2").is_ok());
    }

    #[test]
    fn check_law_negative_constant_side() {
        // 嵌负常数行解锁：负号不再阻塞求值，等式按量纲正常判定
        let law = law_from_json(json!({
            "id": "neg-const",
            "formula": "m = -2 * m0",
            "symbols": { "m": "", "m0": "" },
            "si_units": { "m": "kg", "m0": "kg" },
            "max_version_depth": 1, "monotone_form": true
        }));
        assert_eq!(check_law(&law), DimensionalVerdict::Consistent);

        // 负号不掩盖量纲矛盾：两侧不同维仍 Mismatch
        let law = law_from_json(json!({
            "id": "neg-const-mismatch",
            "formula": "E = -13.6 / n ^ 2",
            "symbols": { "E": "", "n": "" },
            "si_units": { "E": "eV", "n": "1" },
            "max_version_depth": 1, "monotone_form": true
        }));
        match check_law(&law) {
            DimensionalVerdict::Mismatch { .. } => {}
            other => panic!("应为 Mismatch: {other:?}"),
        }
    }

    #[test]
    fn function_call_whitelist_and_strict_dimensionless_args() {
        // 白名单函数严口径：自变量量纲一 → 结果量纲一
        let law = law_from_json(json!({
            "id": "fn-ok",
            "formula": "pH = -log(H)",
            "symbols": { "pH": "", "H": "" },
            "si_units": { "pH": "1", "H": "1", "log": "1" },
            "max_version_depth": 1, "monotone_form": true
        }));
        assert_eq!(check_law(&law), DimensionalVerdict::Consistent);

        // 严口径：自变量非量纲一 = Mismatch 硬校验（不再尾随报错跳过）
        let law = law_from_json(json!({
            "id": "fn-strict",
            "formula": "y = log(x)",
            "symbols": { "y": "", "x": "" },
            "si_units": { "y": "1", "x": "kg", "log": "1" },
            "max_version_depth": 1, "monotone_form": true
        }));
        match check_law(&law) {
            DimensionalVerdict::Mismatch { lhs, rhs } => {
                assert!(lhs.contains("函数自变量量纲违例"));
                assert!(rhs.contains("严口径"));
            }
            other => panic!("应为 Mismatch: {other:?}"),
        }

        // 未知函数名 fail-fast（不可解析 → 跳过照录）
        let law = law_from_json(json!({
            "id": "fn-unknown",
            "formula": "y = gamma_fn(x)",
            "symbols": { "y": "", "x": "" },
            "si_units": { "y": "1", "x": "1" },
            "max_version_depth": 1, "monotone_form": true
        }));
        assert!(matches!(check_law(&law), DimensionalVerdict::Skipped(_)));
    }

    #[test]
    fn fn_arg_dimless_exempt_key_overrides_strict_check() {
        // 豁免键显式声明：自变量非量纲一按量纲一处理（缺省不变式，引擎零硬编码）
        let law = law_from_json(json!({
            "id": "fn-exempt",
            "formula": "pH = -log(H)",
            "symbols": { "pH": "", "H": "" },
            "si_units": { "pH": "1", "H": "mol*m^-3", "log": "1" },
            "max_version_depth": 1, "monotone_form": true,
            "metadata": { "source": "test", "fn_arg_dimless_exempt": true }
        }));
        assert_eq!(check_law(&law), DimensionalVerdict::Consistent);
    }

    #[test]
    fn sqrt_even_power_and_sum_identity() {
        // sqrt 偶幂半幂：sqrt(kg^2*m^4) = kg*m^2
        let law = law_from_json(json!({
            "id": "sqrt-even",
            "formula": "s = sqrt(A)",
            "symbols": { "s": "", "A": "" },
            "si_units": { "s": "kg*m^2", "A": "kg^2*m^4", "sqrt": "1" },
            "max_version_depth": 1, "monotone_form": true
        }));
        assert_eq!(check_law(&law), DimensionalVerdict::Consistent);

        // sqrt 奇幂分量：硬校验 Mismatch
        let law = law_from_json(json!({
            "id": "sqrt-odd",
            "formula": "s = sqrt(A)",
            "symbols": { "s": "", "A": "" },
            "si_units": { "s": "kg", "A": "kg^3", "sqrt": "1" },
            "max_version_depth": 1, "monotone_form": true
        }));
        match check_law(&law) {
            DimensionalVerdict::Mismatch { lhs, .. } => {
                assert!(lhs.contains("奇幂分量"));
            }
            other => panic!("应为 Mismatch: {other:?}"),
        }

        // sum 恒等：Σ 结果维 = 自变量维
        let law = law_from_json(json!({
            "id": "sum-identity",
            "formula": "H = sum(p)",
            "symbols": { "H": "", "p": "" },
            "si_units": { "H": "1", "p": "1", "sum": "1" },
            "max_version_depth": 1, "monotone_form": true
        }));
        assert_eq!(check_law(&law), DimensionalVerdict::Consistent);
    }

    #[test]
    fn parenthesized_exponent_for_dimensionless_base() {
        // e^ 记法：量纲一底 + 括号指数（指数须量纲一，结果量纲一）
        let law = law_from_json(json!({
            "id": "exp-notation",
            "formula": "N = N0 * e^(-lambda * t)",
            "symbols": { "N": "", "N0": "", "e": "", "lambda": "", "t": "" },
            "si_units": { "N": "1", "N0": "1", "e": "1", "lambda": "s^-1", "t": "s" },
            "max_version_depth": 1, "monotone_form": true
        }));
        assert_eq!(check_law(&law), DimensionalVerdict::Consistent);

        // 底非量纲一 = 符号指数（M1c 定性）：不可解析跳过照录
        let law = law_from_json(json!({
            "id": "symbolic-exponent",
            "formula": "y = m ^ (k * t)",
            "symbols": { "y": "", "m": "", "k": "", "t": "" },
            "si_units": { "y": "1", "m": "kg", "k": "1", "t": "s" },
            "max_version_depth": 1, "monotone_form": true
        }));
        assert!(matches!(check_law(&law), DimensionalVerdict::Skipped(_)));
    }

    // ── 同维零特例（metadata.same_dim_zero 显式声明激活；缺省不变式三组）──

    #[test]
    fn same_dim_zero_declared_literal_zero_rhs_consistent() {
        // 声明 + rhs 严格字面 `0`：零值量纲不设限，lhs 解析成功即 Consistent 纯态
        for (id, sym, unit) in [
            ("eq-pressure", "dP", "pascal"),
            ("eq-angular", "dL", "kg*m^2*s^-1"),
            ("eq-force", "F", "newton"),
            ("eq-entropy", "deltaS", "joule*K^-1"),
        ] {
            let law = law_from_json(json!({
                "id": id, "formula": format!("{sym} = 0"),
                "symbols": { sym: "" },
                "si_units": { sym: unit },
                "max_version_depth": 1, "monotone_form": true,
                "metadata": { "source": "test", "same_dim_zero": true }
            }));
            assert_eq!(
                check_law(&law),
                DimensionalVerdict::Consistent,
                "声明+字面零 `{sym} = 0` 应判 Consistent"
            );
        }

        // 声明 + 字面零 + lhs 不可解析（si 拼写错）→ 现行报因路径照旧 Skipped
        let law = law_from_json(json!({
            "id": "eq-bad-unit", "formula": "F = 0",
            "symbols": { "F": "" },
            "si_units": { "F": "newtn" },
            "max_version_depth": 1, "monotone_form": true,
            "metadata": { "source": "test", "same_dim_zero": true }
        }));
        match check_law(&law) {
            DimensionalVerdict::Skipped(reason) => {
                assert!(reason.contains("未知单位"), "reason = {reason}");
            }
            other => panic!("lhs 不可解析应走现行 Skipped 路径: {other:?}"),
        }
    }

    #[test]
    fn same_dim_zero_declared_nonzero_rhs_current_path_unchanged() {
        // 声明在场但 rhs 非严格字面单数字 `0`：特例不生效，现行比对路径
        // （`0.0` 解析为量纲一 vs lhs 物理量纲 → Mismatch；零表达式同法）
        let law = law_from_json(json!({
            "id": "near-zero-literal", "formula": "F = 0.0",
            "symbols": { "F": "" },
            "si_units": { "F": "newton" },
            "max_version_depth": 1, "monotone_form": true,
            "metadata": { "source": "test", "same_dim_zero": true }
        }));
        match check_law(&law) {
            DimensionalVerdict::Mismatch { lhs, rhs } => {
                assert!(lhs.contains("kg*m*s^-2"), "lhs = {lhs}");
                assert!(rhs.contains("1"), "rhs = {rhs}");
            }
            other => panic!("`F = 0.0` 特例不激活应现行 Mismatch: {other:?}"),
        }

        let law = law_from_json(json!({
            "id": "zero-expr", "formula": "F = 0 * a",
            "symbols": { "F": "", "a": "" },
            "si_units": { "F": "newton", "a": "m*s^-2" },
            "max_version_depth": 1, "monotone_form": true,
            "metadata": { "source": "test", "same_dim_zero": true }
        }));
        match check_law(&law) {
            DimensionalVerdict::Mismatch { lhs, rhs } => {
                assert!(lhs.contains("kg*m*s^-2"), "lhs = {lhs}");
                assert!(rhs.contains("m*s^-2"), "rhs = {rhs}");
            }
            other => panic!("零表达式特例不激活应现行 Mismatch: {other:?}"),
        }

        // 声明在场 + rhs 非零 + 两侧同维：现行比对路径照常判 Consistent
        let law = law_from_json(json!({
            "id": "normal-ok", "formula": "F = m * a",
            "symbols": { "F": "", "m": "", "a": "" },
            "si_units": { "F": "newton", "m": "kilogram", "a": "m*s^-2" },
            "max_version_depth": 1, "monotone_form": true,
            "metadata": { "source": "test", "same_dim_zero": true }
        }));
        assert_eq!(check_law(&law), DimensionalVerdict::Consistent);
    }

    #[test]
    fn same_dim_zero_undeclared_zero_rhs_current_mismatch_unchanged() {
        // 未声明（缺省 false = 缺省不变式锚）：`X = 0` 照现行判定 Mismatch
        let law = law_from_json(json!({
            "id": "default-zero", "formula": "F = 0",
            "symbols": { "F": "" },
            "si_units": { "F": "newton" },
            "max_version_depth": 1, "monotone_form": true
        }));
        match check_law(&law) {
            DimensionalVerdict::Mismatch { lhs, rhs } => {
                assert!(lhs.contains("kg*m*s^-2"), "lhs = {lhs}");
                assert!(rhs.contains("1"), "rhs = {rhs}");
            }
            other => panic!("未声明应现行 Mismatch: {other:?}"),
        }

        // 旧 law_set 文档（metadata 无该键）反序列化兼容：same_dim_zero = None
        let law: Law = serde_json::from_value(json!({
            "id": "legacy", "formula": "F = m * a",
            "symbols": { "F": "", "m": "", "a": "" },
            "si_units": { "F": "newton", "m": "kilogram", "a": "m*s^-2" },
            "max_version_depth": 1, "monotone_form": true,
            "metadata": { "source": "legacy-doc" }
        }))
        .unwrap();
        assert_eq!(law.metadata.as_ref().and_then(|m| m.same_dim_zero), None);
        assert_eq!(check_law(&law), DimensionalVerdict::Consistent);
    }
}

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
//! SI 七基本量纲的 i8 幂次向量 [M, L, T, I, Θ, N, J]
//! （质量/长度/时间/电流/热力学温度/物质的量/发光强度）。
//! - 乘除 = 分量加减；整数幂 = 分量倍乘（checked，越界按不可解析处理）；
//! - 数值常量因子量纲中性（量纲一）；
//! - 平面角/立体角按 SI 口径量纲一（rad/sr/deg 不引入辅助量纲）。
//!
//! # 表达式文法（formula 侧与 si_units 侧共用）
//! ```text
//! expr   := factor (('*' | '/') factor)*
//! factor := base ('^' ['-'] digits)?
//! base   := identifier | number
//! ```
//! 结构层已保证 formula 无判定形态运算符且引用符号已声明；本模块对文法外
//! 记号（如 `+`、括号）不猜测语义——按不可解析跳过，保持确定性。
//!
//! # 选型注记
//! 单位解析未引入 crates.io 单位库（`uom`/`pint` 系均为编译期类型层单位系统，
//! 无运行时单位表达式字符串解析能力，且会给 schema 门禁 crate 引入重依赖）；
//! 本模块以约三百行的确定性代数覆盖 si 基本量纲表达需求。

use std::collections::{BTreeMap, BTreeSet};

use super::Law;

// ============================================================================
// 七基本量纲向量
// ============================================================================

/// 七基本量纲幂次向量 [M, L, T, I, Θ, N, J]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DimensionVector(pub [i8; 7]);

/// 基本量纲的规范单位记号（渲染用，位序与向量分量一致）
const BASE_UNIT_SYMBOLS: [&str; 7] = ["kg", "m", "s", "A", "K", "mol", "cd"];

impl DimensionVector {
    /// 量纲一（全零向量）
    pub const DIMENSIONLESS: Self = Self([0; 7]);

    fn checked_add(self, other: Self) -> Option<Self> {
        let mut out = [0i8; 7];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = self.0[i].checked_add(other.0[i])?;
        }
        Some(Self(out))
    }

    fn checked_sub(self, other: Self) -> Option<Self> {
        let mut out = [0i8; 7];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = self.0[i].checked_sub(other.0[i])?;
        }
        Some(Self(out))
    }

    fn checked_pow(self, exp: i16) -> Option<Self> {
        let exp = i8::try_from(exp).ok()?;
        let mut out = [0i8; 7];
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

/// SI 单位记号 → 七基本量纲幂次。未知记号返回 `None`（调用方按「不可解析」
/// 跳过处理，warning 级不拒收）。记号区分大小写（SI 符号口径）。
fn unit_dimensions(name: &str) -> Option<DimensionVector> {
    let dims: [i8; 7] = match name {
        // 基本单位（m/kg/s/A/K/mol/cd）+ 常用全名别名
        "m" | "meter" | "metre" => [0, 1, 0, 0, 0, 0, 0],
        "kg" | "kilogram" => [1, 0, 0, 0, 0, 0, 0],
        "g" | "gram" | "gramme" => [1, 0, 0, 0, 0, 0, 0],
        "s" | "second" | "sec" => [0, 0, 1, 0, 0, 0, 0],
        "A" | "ampere" | "amp" => [0, 0, 0, 1, 0, 0, 0],
        "K" | "kelvin" => [0, 0, 0, 0, 1, 0, 0],
        "mol" | "mole" => [0, 0, 0, 0, 0, 1, 0],
        "cd" | "candela" => [0, 0, 0, 0, 0, 0, 1],
        // 相干导出单位
        "N" | "newton" => [1, 1, -2, 0, 0, 0, 0],
        "Pa" | "pascal" => [1, -1, -2, 0, 0, 0, 0],
        "J" | "joule" => [1, 2, -2, 0, 0, 0, 0],
        "W" | "watt" => [1, 2, -3, 0, 0, 0, 0],
        "Hz" | "hertz" => [0, 0, -1, 0, 0, 0, 0],
        "C" | "coulomb" => [0, 0, 1, 1, 0, 0, 0],
        "V" | "volt" => [1, 2, -3, -1, 0, 0, 0],
        "F" | "farad" => [-1, -2, 4, 2, 0, 0, 0],
        "ohm" => [1, 2, -3, -2, 0, 0, 0],
        "S" | "siemens" => [-1, -2, 3, 2, 0, 0, 0],
        "Wb" | "weber" => [1, 2, -2, -1, 0, 0, 0],
        "T" | "tesla" => [1, 0, -2, -1, 0, 0, 0],
        "H" | "henry" => [1, 2, -2, -2, 0, 0, 0],
        "lm" | "lumen" => [0, 0, 0, 0, 0, 0, 1],
        "lx" | "lux" => [0, -2, 0, 0, 0, 0, 1],
        "Bq" | "becquerel" => [0, 0, -1, 0, 0, 0, 0],
        "Gy" | "gray" => [0, 2, -2, 0, 0, 0, 0],
        "Sv" | "sievert" => [0, 2, -2, 0, 0, 0, 0],
        "kat" | "katal" => [0, 0, -1, 0, 0, 1, 0],
        // 量纲一（SI 平面角/立体角口径）
        "rad" | "radian" | "sr" | "steradian" | "deg" | "degree" => [0; 7],
        // 常用接受单位（量纲与相干单位一致，不换算量值）
        "l" | "L" | "liter" | "litre" => [0, 3, 0, 0, 0, 0, 0],
        "min" | "minute" | "h" | "hour" | "day" | "d" => [0, 0, 1, 0, 0, 0, 0],
        "t" | "tonne" => [1, 0, 0, 0, 0, 0, 0],
        "bar" | "atm" => [1, -1, -2, 0, 0, 0, 0],
        "eV" | "electronvolt" => [1, 2, -2, 0, 0, 0, 0],
        _ => return None,
    };
    Some(DimensionVector(dims))
}

// ============================================================================
// 表达式求值（formula 侧与 si_units 侧共用文法）
// ============================================================================

/// 因子叶子解析：标识符 → 量纲向量；`Err` = 该标识符不可解析（含原因）
type LeafResolver<'a> = &'a mut dyn FnMut(&str) -> Result<DimensionVector, String>;

struct ExprParser<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> ExprParser<'a> {
    fn new(src: &'a str) -> Self {
        ExprParser { src, pos: 0 }
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
    fn parse_expr(&mut self, leaf: LeafResolver<'_>) -> Result<DimensionVector, String> {
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

    /// `factor := base ('^' exponent)?`
    fn parse_factor(&mut self, leaf: LeafResolver<'_>) -> Result<DimensionVector, String> {
        self.skip_ws();
        let base = self.parse_base(leaf)?;
        self.skip_ws();
        if self.peek() == Some(b'^') {
            self.pos += 1;
            let exp = self.parse_exponent()?;
            return base
                .checked_pow(exp)
                .ok_or_else(|| format!("幂次 {exp} 分量溢出（|exp| ≤ 127）"));
        }
        Ok(base)
    }

    /// `base := 数值常量（量纲一）| 标识符（叶子解析）`
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
                leaf(&self.src[start..self.pos])
            }
            Some(b) if b.is_ascii() => Err(format!("非法记号 `{}`", b as char)),
            Some(_) => Err("非 ASCII 记号（仅允许 ASCII 标识符与运算符）".to_string()),
            None => Err("表达式意外结束".to_string()),
        }
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

/// 解析 si_units 单位表达式 → 七基本量纲向量（如 `kg*m*s^-2`、`m*s^-1`、
/// `joule*mol^-1*K^-1`）。未知单位名/文法外记号/分量溢出均报错。
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

/// 求值 formula 单侧表达式：符号量纲经 si_units 解析（带缓存，重复符号只解析一次）
fn eval_side(
    side: &str,
    si_units: &BTreeMap<String, String>,
    cache: &mut BTreeMap<String, DimensionVector>,
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

/// 量纲校验结论（能力层三态）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DimensionalVerdict {
    /// 两侧量纲一致（含两侧均为量纲一）
    Consistent,
    /// 两侧量纲不一致——硬拒收（字段 = 两侧表达式与其规范量纲向量渲染）
    Mismatch { lhs: String, rhs: String },
    /// 跳过：缺 si_units / 单位表达式不可解析 / 文法外记号（warning 级，不拒收）
    Skipped(String),
}

/// 单条定律的量纲一致性校验。
///
/// 启用条件 = formula 引用符号全部有 si_units（缺者整体跳过并给出原因，
/// 兼容 G 类纯数学定律）；两侧求值均成功才比较，任一侧不可解析即跳过。
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

    let mut cache = BTreeMap::new();
    let lhs = match eval_side(lhs_raw, &law.si_units, &mut cache) {
        Ok(dims) => dims,
        Err(reason) => return DimensionalVerdict::Skipped(reason),
    };
    let rhs = match eval_side(rhs_raw, &law.si_units, &mut cache) {
        Ok(dims) => dims,
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

    fn dims(v: [i8; 7]) -> DimensionVector {
        DimensionVector(v)
    }

    // ── 单位表达式解析 ──

    #[test]
    fn unit_expr_parses_to_base_vectors() {
        assert_eq!(
            parse_unit_expr("kg*m*s^-2").unwrap(),
            dims([1, 1, -2, 0, 0, 0, 0])
        );
        assert_eq!(
            parse_unit_expr("m*s^-1").unwrap(),
            dims([0, 1, -1, 0, 0, 0, 0])
        );
        assert_eq!(parse_unit_expr("m^3").unwrap(), dims([0, 3, 0, 0, 0, 0, 0]));
        assert_eq!(
            parse_unit_expr("joule*mol^-1*K^-1").unwrap(),
            dims([1, 2, -2, 0, -1, -1, 0])
        );
        assert_eq!(
            parse_unit_expr("newton").unwrap(),
            dims([1, 1, -2, 0, 0, 0, 0])
        );
        assert_eq!(
            parse_unit_expr("kilogram").unwrap(),
            dims([1, 0, 0, 0, 0, 0, 0])
        );
        assert_eq!(
            parse_unit_expr("kelvin").unwrap(),
            dims([0, 0, 0, 0, 1, 0, 0])
        );
        assert_eq!(
            parse_unit_expr("pascal").unwrap(),
            dims([1, -1, -2, 0, 0, 0, 0])
        );
        assert_eq!(parse_unit_expr("mol").unwrap(), dims([0, 0, 0, 0, 0, 1, 0]));
        assert_eq!(
            parse_unit_expr("m/s").unwrap(),
            dims([0, 1, -1, 0, 0, 0, 0])
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
    fn unit_expr_whitespace_tolerant() {
        assert_eq!(
            parse_unit_expr(" kg * m * s^-2 ").unwrap(),
            dims([1, 1, -2, 0, 0, 0, 0])
        );
    }

    #[test]
    fn unit_expr_rejects_unknown_names_and_bad_syntax() {
        assert!(parse_unit_expr("newtn").unwrap_err().contains("未知单位"));
        assert!(parse_unit_expr("m + s")
            .unwrap_err()
            .contains("尾随未解析内容"));
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

    // ── 规范渲染 ──

    #[test]
    fn render_matches_base_symbol_order() {
        assert_eq!(dims([1, 1, -2, 0, 0, 0, 0]).render(), "kg*m*s^-2");
        assert_eq!(dims([0, 3, 0, 0, 0, 0, 0]).render(), "m^3");
        assert_eq!(DimensionVector::DIMENSIONLESS.render(), "1");
        assert_eq!(dims([0, 0, 0, 2, -1, 0, 0]).render(), "A^2*K^-1");
        assert_eq!(dims([1, 0, -1, 0, 0, -1, 0]).render(), "kg*s^-1*mol^-1");
    }

    // ── checked 代数（溢出 → None → 按不可解析跳过）──

    #[test]
    fn checked_ops_overflow_to_none() {
        let big = dims([127, 0, 0, 0, 0, 0, 0]);
        assert_eq!(big.checked_add(big), None);
        assert_eq!(big.checked_pow(2), None);
        assert_eq!(
            dims([-128, 0, 0, 0, 0, 0, 0]).checked_sub(dims([1, 0, 0, 0, 0, 0, 0])),
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
            "si_units": { "F": "newton", "m": "kilogram", "a": "m*s^-2" }
        }));
        assert_eq!(check_law(&consistent), DimensionalVerdict::Consistent);

        let mismatch = law_from_json(json!({
            "id": "f_mv", "formula": "F = m * v",
            "symbols": { "F": "力", "m": "质量", "v": "速度" },
            "si_units": { "F": "newton", "m": "kilogram", "v": "m*s^-1" }
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
            "si_units": {}
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
            "si_units": { "F": "newtn", "m": "kilogram", "a": "m*s^-2" }
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
            "si_units": { "y": "m", "a": "m", "x": "m" }
        }));
        match check_law(&bad_token) {
            DimensionalVerdict::Skipped(reason) => {
                assert!(reason.contains("非法记号"), "reason = {reason}");
            }
            other => panic!("应为 Skipped: {other:?}"),
        }

        let trailing = law_from_json(json!({
            "id": "plus", "formula": "y = a + x",
            "symbols": { "y": "和", "a": "项一", "x": "项二" },
            "si_units": { "y": "m", "a": "m", "x": "m" }
        }));
        match check_law(&trailing) {
            DimensionalVerdict::Skipped(reason) => {
                assert!(reason.contains("尾随未解析内容"), "reason = {reason}");
            }
            other => panic!("应为 Skipped: {other:?}"),
        }
    }
}

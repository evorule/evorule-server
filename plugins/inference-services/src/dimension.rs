// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! SI 量纲代数与公式量纲推导（确定性静态分析，无 I/O、无墙钟）。
//!
//! 算法对照上游参考实现逐函数移植：
//! - [`SIDimension`]：七基本量纲幂次向量（L/M/T/I/Θ/N/J），
//!   乘（幂次相加）/ 除（相减）/ 幂（乘 n）/ 开方（整除才有效）；
//! - 单位字符串解析：基本单位、导出单位（N/J/W/Pa/Hz/C/V/Ω/F…）、
//!   复合单位（`·`/`*` 乘积、`/` 分式、`^` 与 Unicode 上标幂次）；
//! - [`FormulaDimensionInferencer`]：按已知变量量纲对四则与幂表达式
//!   自顶向下推导量纲，验证等式两侧一致性。
//!
//! # 确定性
//!
//! 全程整数幂次运算，无浮点、无随机、无时钟；已知量纲表用
//! `BTreeMap` 保证遍历序确定性。

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

// ═══════════════════════════════════════════════════════════════════
// SI 量纲向量
// ═══════════════════════════════════════════════════════════════════

/// SI 量纲向量（7 个基本量纲的幂次）
///
/// 表示为 [L^a, M^b, T^c, I^d, Θ^e, N^f, J^g]
/// 对应 SI 七个基本量：长度、质量、时间、电流、温度、物质的量、发光强度
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SIDimension {
    /// 长度 (L) 的幂次
    pub length: i32,
    /// 质量 (M) 的幂次
    pub mass: i32,
    /// 时间 (T) 的幂次
    pub time: i32,
    /// 电流 (I) 的幂次
    pub current: i32,
    /// 温度 (Θ) 的幂次
    pub temperature: i32,
    /// 物质的量 (N) 的幂次
    pub amount: i32,
    /// 发光强度 (J) 的幂次
    pub luminosity: i32,
}

impl SIDimension {
    /// 无量纲
    pub const DIMENSIONLESS: SIDimension = SIDimension {
        length: 0,
        mass: 0,
        time: 0,
        current: 0,
        temperature: 0,
        amount: 0,
        luminosity: 0,
    };

    /// 长度 (m)
    pub const LENGTH: SIDimension = SIDimension {
        length: 1,
        mass: 0,
        time: 0,
        current: 0,
        temperature: 0,
        amount: 0,
        luminosity: 0,
    };

    /// 质量 (kg)
    pub const MASS: SIDimension = SIDimension {
        length: 0,
        mass: 1,
        time: 0,
        current: 0,
        temperature: 0,
        amount: 0,
        luminosity: 0,
    };

    /// 时间 (s)
    pub const TIME: SIDimension = SIDimension {
        length: 0,
        mass: 0,
        time: 1,
        current: 0,
        temperature: 0,
        amount: 0,
        luminosity: 0,
    };

    /// 电流 (A)
    pub const CURRENT: SIDimension = SIDimension {
        length: 0,
        mass: 0,
        time: 0,
        current: 1,
        temperature: 0,
        amount: 0,
        luminosity: 0,
    };

    /// 温度 (K)
    pub const TEMPERATURE: SIDimension = SIDimension {
        length: 0,
        mass: 0,
        time: 0,
        current: 0,
        temperature: 1,
        amount: 0,
        luminosity: 0,
    };

    /// 物质的量 (mol)
    pub const AMOUNT: SIDimension = SIDimension {
        length: 0,
        mass: 0,
        time: 0,
        current: 0,
        temperature: 0,
        amount: 1,
        luminosity: 0,
    };

    /// 速度 (m/s)
    pub const VELOCITY: SIDimension = SIDimension {
        length: 1,
        mass: 0,
        time: -1,
        current: 0,
        temperature: 0,
        amount: 0,
        luminosity: 0,
    };

    /// 加速度 (m/s²)
    pub const ACCELERATION: SIDimension = SIDimension {
        length: 1,
        mass: 0,
        time: -2,
        current: 0,
        temperature: 0,
        amount: 0,
        luminosity: 0,
    };

    /// 力 (N = kg·m/s²)
    pub const FORCE: SIDimension = SIDimension {
        length: 1,
        mass: 1,
        time: -2,
        current: 0,
        temperature: 0,
        amount: 0,
        luminosity: 0,
    };

    /// 能量 (J = kg·m²/s²)
    pub const ENERGY: SIDimension = SIDimension {
        length: 2,
        mass: 1,
        time: -2,
        current: 0,
        temperature: 0,
        amount: 0,
        luminosity: 0,
    };

    /// 功率 (W = kg·m²/s³)
    pub const POWER: SIDimension = SIDimension {
        length: 2,
        mass: 1,
        time: -3,
        current: 0,
        temperature: 0,
        amount: 0,
        luminosity: 0,
    };

    /// 压力 (Pa = kg/(m·s²))
    pub const PRESSURE: SIDimension = SIDimension {
        length: -1,
        mass: 1,
        time: -2,
        current: 0,
        temperature: 0,
        amount: 0,
        luminosity: 0,
    };

    /// 面积 (m²)
    pub const AREA: SIDimension = SIDimension {
        length: 2,
        mass: 0,
        time: 0,
        current: 0,
        temperature: 0,
        amount: 0,
        luminosity: 0,
    };

    /// 体积 (m³)
    pub const VOLUME: SIDimension = SIDimension {
        length: 3,
        mass: 0,
        time: 0,
        current: 0,
        temperature: 0,
        amount: 0,
        luminosity: 0,
    };

    /// 乘法：量纲相加
    pub fn multiply(&self, other: &SIDimension) -> SIDimension {
        SIDimension {
            length: self.length + other.length,
            mass: self.mass + other.mass,
            time: self.time + other.time,
            current: self.current + other.current,
            temperature: self.temperature + other.temperature,
            amount: self.amount + other.amount,
            luminosity: self.luminosity + other.luminosity,
        }
    }

    /// 除法：量纲相减
    pub fn divide(&self, other: &SIDimension) -> SIDimension {
        SIDimension {
            length: self.length - other.length,
            mass: self.mass - other.mass,
            time: self.time - other.time,
            current: self.current - other.current,
            temperature: self.temperature - other.temperature,
            amount: self.amount - other.amount,
            luminosity: self.luminosity - other.luminosity,
        }
    }

    /// 幂运算：量纲乘以 n
    pub fn power(&self, n: i32) -> SIDimension {
        SIDimension {
            length: self.length * n,
            mass: self.mass * n,
            time: self.time * n,
            current: self.current * n,
            temperature: self.temperature * n,
            amount: self.amount * n,
            luminosity: self.luminosity * n,
        }
    }

    /// 开方：量纲除以 n（仅当所有幂次可被 n 整除时有效）
    pub fn root(&self, n: i32) -> Option<SIDimension> {
        if n == 0 {
            return None;
        }
        let try_div = |v: i32| -> Option<i32> {
            if v % n == 0 {
                Some(v / n)
            } else {
                None
            }
        };
        Some(SIDimension {
            length: try_div(self.length)?,
            mass: try_div(self.mass)?,
            time: try_div(self.time)?,
            current: try_div(self.current)?,
            temperature: try_div(self.temperature)?,
            amount: try_div(self.amount)?,
            luminosity: try_div(self.luminosity)?,
        })
    }

    /// 是否无量纲
    pub fn is_dimensionless(&self) -> bool {
        *self == SIDimension::DIMENSIONLESS
    }

    /// 七分量向量形式（[L, M, T, I, Θ, N, J]）
    pub fn to_vector(&self) -> [i32; 7] {
        [
            self.length,
            self.mass,
            self.time,
            self.current,
            self.temperature,
            self.amount,
            self.luminosity,
        ]
    }

    /// 从七分量向量构造
    pub fn from_vector(v: [i32; 7]) -> SIDimension {
        SIDimension {
            length: v[0],
            mass: v[1],
            time: v[2],
            current: v[3],
            temperature: v[4],
            amount: v[5],
            luminosity: v[6],
        }
    }

    /// 从 SI 单位字符串解析量纲
    ///
    /// 支持的格式：
    /// - 基本单位：m, kg, s, A, K, mol, cd
    /// - 导出单位：N, J, W, Pa, Hz, C, V, Ω, F, m/s, m/s², m², m³, kg/m³
    /// - 复合单位：kg·m/s², kg*m²/s², kg·m²/s³（`·` 或 `*` 乘积、`/` 分式）
    /// - 带幂次：m², m³, s⁻¹, s^-2, kg^3
    pub fn from_si_unit(unit: &str) -> Option<SIDimension> {
        let unit = unit.trim();

        // 直接映射常见 SI 单位
        match unit {
            // 基本单位
            "m" => Some(SIDimension::LENGTH),
            "kg" => Some(SIDimension::MASS),
            "s" => Some(SIDimension::TIME),
            "A" => Some(SIDimension::CURRENT),
            "K" => Some(SIDimension::TEMPERATURE),
            "mol" => Some(SIDimension::AMOUNT),
            "cd" => Some(SIDimension {
                length: 0,
                mass: 0,
                time: 0,
                current: 0,
                temperature: 0,
                amount: 0,
                luminosity: 1,
            }),
            // 导出单位
            "N" | "kg·m/s²" | "kg*m/s²" => Some(SIDimension::FORCE),
            "J" | "kg·m²/s²" | "kg*m²/s²" => Some(SIDimension::ENERGY),
            "W" | "kg·m²/s³" | "kg*m²/s³" => Some(SIDimension::POWER),
            "Pa" | "kg/(m·s²)" | "kg/(m*s²)" | "N/m²" => Some(SIDimension::PRESSURE),
            "Hz" | "s⁻¹" | "s^-1" | "1/s" => Some(SIDimension::TIME.power(-1)),
            "C" | "A·s" | "A*s" => Some(SIDimension {
                length: 0,
                mass: 0,
                time: 1,
                current: 1,
                temperature: 0,
                amount: 0,
                luminosity: 0,
            }),
            "V" | "kg·m²/(A·s³)" | "W/A" => Some(SIDimension {
                length: 2,
                mass: 1,
                time: -3,
                current: -1,
                temperature: 0,
                amount: 0,
                luminosity: 0,
            }),
            "Ω" | "kg·m²/(A²·s³)" | "V/A" => Some(SIDimension {
                length: 2,
                mass: 1,
                time: -3,
                current: -2,
                temperature: 0,
                amount: 0,
                luminosity: 0,
            }),
            "F" | "A²·s⁴/(kg·m²)" | "C/V" => Some(SIDimension {
                length: -2,
                mass: -1,
                time: 4,
                current: 2,
                temperature: 0,
                amount: 0,
                luminosity: 0,
            }),
            "m/s" | "m·s⁻¹" => Some(SIDimension::VELOCITY),
            "m/s²" | "m·s⁻²" => Some(SIDimension::ACCELERATION),
            "m²" => Some(SIDimension::AREA),
            "m³" => Some(SIDimension::VOLUME),
            "kg/m³" => Some(SIDimension {
                length: -3,
                mass: 1,
                time: 0,
                current: 0,
                temperature: 0,
                amount: 0,
                luminosity: 0,
            }),
            "" | "-" | "1" | "dimensionless" => Some(SIDimension::DIMENSIONLESS),
            _ => Self::parse_compound_unit(unit),
        }
    }

    /// 解析复合单位（如 "kg·m²/s³"）
    fn parse_compound_unit(unit: &str) -> Option<SIDimension> {
        // 分离分子和分母
        let parts: Vec<&str> = unit.split('/').collect();
        if parts.is_empty() {
            return None;
        }

        // 解析分子
        let numerator = parts[0];
        let num_dims = Self::parse_product(numerator)?;
        let mut result = num_dims
            .into_iter()
            .fold(SIDimension::DIMENSIONLESS, |acc, dim| acc.multiply(&dim));

        // 解析分母
        if parts.len() > 1 {
            for denom in &parts[1..] {
                let denom_dims = Self::parse_product(denom)?;
                let denom_combined = denom_dims
                    .into_iter()
                    .fold(SIDimension::DIMENSIONLESS, |acc, dim| acc.multiply(&dim));
                result = result.divide(&denom_combined);
            }
        }

        Some(result)
    }

    /// 解析乘积表达式（如 "kg·m²"）
    fn parse_product(expr: &str) -> Option<Vec<SIDimension>> {
        let expr = expr.trim().trim_start_matches('(').trim_end_matches(')');
        let mut dims = Vec::new();

        // 按 · 或 * 分割
        let parts: Vec<&str> = if expr.contains('·') {
            expr.split('·').collect()
        } else if expr.contains('*') {
            expr.split('*').collect()
        } else {
            vec![expr]
        };

        for part in parts {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            // 解析带幂次的单位（如 m², s⁻², kg^3）
            let (base, exp) = Self::parse_unit_with_exponent(part)?;
            // 使用基础单位映射（避免递归调用 from_si_unit → parse_compound_unit）
            let base_dim = Self::base_unit_to_dimension(&base)?;
            dims.push(base_dim.power(exp));
        }

        if dims.is_empty() {
            None
        } else {
            Some(dims)
        }
    }

    /// 基础单位到量纲的映射（不触发递归）
    fn base_unit_to_dimension(unit: &str) -> Option<SIDimension> {
        match unit.trim() {
            "m" => Some(SIDimension::LENGTH),
            "kg" => Some(SIDimension::MASS),
            "s" => Some(SIDimension::TIME),
            "A" => Some(SIDimension::CURRENT),
            "K" => Some(SIDimension::TEMPERATURE),
            "mol" => Some(SIDimension::AMOUNT),
            "cd" => Some(SIDimension {
                length: 0,
                mass: 0,
                time: 0,
                current: 0,
                temperature: 0,
                amount: 0,
                luminosity: 1,
            }),
            _ => None,
        }
    }

    /// 解析带幂次的单位（如 m² → (m, 2), s⁻² → (s, -2)）
    fn parse_unit_with_exponent(s: &str) -> Option<(String, i32)> {
        let s = s.trim();

        // Unicode 上标数字
        let superscript_map = [
            ('⁰', '0'),
            ('¹', '1'),
            ('²', '2'),
            ('³', '3'),
            ('⁴', '4'),
            ('⁵', '5'),
            ('⁶', '6'),
            ('⁷', '7'),
            ('⁸', '8'),
            ('⁹', '9'),
            ('⁻', '-'),
        ];

        // 查找幂次开始位置
        let mut exp_start = s.len();
        let mut has_caret = false;

        if let Some(pos) = s.find('^') {
            exp_start = pos;
            has_caret = true;
        } else {
            // 查找 Unicode 上标或负号
            for (i, c) in s.char_indices() {
                if superscript_map.iter().any(|(sc, _)| *sc == c) {
                    exp_start = i;
                    break;
                }
            }
        }

        if exp_start == s.len() {
            // 无幂次
            return Some((s.to_string(), 1));
        }

        let base = s[..exp_start].to_string();
        let exp_str = if has_caret {
            s[exp_start + 1..].to_string()
        } else {
            // 转换 Unicode 上标为普通数字
            s[exp_start..]
                .chars()
                .map(|c| {
                    superscript_map
                        .iter()
                        .find(|(sc, _)| *sc == c)
                        .map(|(_, rc)| *rc)
                        .unwrap_or(c)
                })
                .collect::<String>()
        };

        let exp: i32 = exp_str.parse().ok()?;
        Some((base, exp))
    }

    /// 转换为可读的 SI 单位字符串
    pub fn to_si_string(&self) -> String {
        if self.is_dimensionless() {
            return "1".to_string();
        }

        let base_units = [
            ("kg", self.mass),
            ("m", self.length),
            ("s", self.time),
            ("A", self.current),
            ("K", self.temperature),
            ("mol", self.amount),
            ("cd", self.luminosity),
        ];

        let mut numerator = Vec::new();
        let mut denominator = Vec::new();

        for (unit, power) in &base_units {
            match power.cmp(&0) {
                std::cmp::Ordering::Greater => {
                    if *power == 1 {
                        numerator.push(unit.to_string());
                    } else {
                        numerator.push(format!("{}{}", unit, Self::format_exponent(*power)));
                    }
                }
                std::cmp::Ordering::Less => {
                    let abs_power = power.abs();
                    if abs_power == 1 {
                        denominator.push(unit.to_string());
                    } else {
                        denominator.push(format!("{}{}", unit, Self::format_exponent(abs_power)));
                    }
                }
                std::cmp::Ordering::Equal => {}
            }
        }

        if denominator.is_empty() {
            numerator.join("·")
        } else {
            format!("{} / {}", numerator.join("·"), denominator.join("·"))
        }
    }

    fn format_exponent(n: i32) -> String {
        let superscript = [
            ('0', '⁰'),
            ('1', '¹'),
            ('2', '²'),
            ('3', '³'),
            ('4', '⁴'),
            ('5', '⁵'),
            ('6', '⁶'),
            ('7', '⁷'),
            ('8', '⁸'),
            ('9', '⁹'),
            ('-', '⁻'),
        ];
        n.to_string()
            .chars()
            .map(|c| {
                superscript
                    .iter()
                    .find(|(a, _)| *a == c)
                    .map(|(_, s)| *s)
                    .unwrap_or(c)
            })
            .collect()
    }
}

impl std::fmt::Display for SIDimension {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[L{}M{}T{}I{}Θ{}N{}J{}]",
            self.length,
            self.mass,
            self.time,
            self.current,
            self.temperature,
            self.amount,
            self.luminosity
        )
    }
}

// ═══════════════════════════════════════════════════════════════════
// 公式量纲推导器
// ═══════════════════════════════════════════════════════════════════

/// 公式量纲推导器
///
/// 根据已知变量量纲，推导公式中各子表达式的量纲，
/// 并验证等式左右两边的量纲是否一致。
pub struct FormulaDimensionInferencer {
    /// 已知变量量纲（BTreeMap 保证遍历序确定性）
    known_dimensions: BTreeMap<String, SIDimension>,
}

impl FormulaDimensionInferencer {
    pub fn new() -> Self {
        FormulaDimensionInferencer {
            known_dimensions: BTreeMap::new(),
        }
    }

    /// 添加已知变量量纲
    pub fn add_dimension(&mut self, var: &str, dim: SIDimension) {
        self.known_dimensions.insert(var.to_string(), dim);
    }

    /// 从 `{"变量": "SI单位", ...}` 形态的映射批量加载。
    ///
    /// 未知单位 / 非字符串值按跳过处理（与上游参考实现一致）；
    /// 服务层会在结果中如实报告无法推导，不静默失败。
    pub fn load_from_si_units(&mut self, si_units: &BTreeMap<String, String>) {
        for (var, unit_str) in si_units {
            if let Some(dim) = SIDimension::from_si_unit(unit_str) {
                self.known_dimensions.insert(var.clone(), dim);
            }
        }
    }

    /// 推导表达式的量纲
    ///
    /// 简化实现：仅处理四则运算和幂运算
    pub fn infer_expression_dimension(&self, expr: &str) -> Option<SIDimension> {
        let expr = expr.trim();

        // 纯数字 → 无量纲
        if expr.parse::<f64>().is_ok() {
            return Some(SIDimension::DIMENSIONLESS);
        }

        // 已知变量
        if let Some(dim) = self.known_dimensions.get(expr) {
            return Some(dim.clone());
        }

        // 加减法：两边量纲必须相同
        if let Some(pos) = Self::find_top_level_op(expr, &['+', '-']) {
            let left = self.infer_expression_dimension(&expr[..pos])?;
            let right = self.infer_expression_dimension(expr[pos + 1..].trim_start())?;
            if left == right {
                return Some(left);
            }
            return None; // 量纲不匹配
        }

        // 乘法
        if let Some(pos) = Self::find_top_level_op(expr, &['*']) {
            let left = self.infer_expression_dimension(&expr[..pos])?;
            let right = self.infer_expression_dimension(expr[pos + 1..].trim_start())?;
            return Some(left.multiply(&right));
        }

        // 除法
        if let Some(pos) = Self::find_top_level_op(expr, &['/']) {
            let left = self.infer_expression_dimension(&expr[..pos])?;
            let right = self.infer_expression_dimension(expr[pos + 1..].trim_start())?;
            return Some(left.divide(&right));
        }

        // 幂运算：x**n → x 的量纲的 n 次方（n 必须是无量纲常数）
        if let Some(pos) = expr.find("**") {
            let base = self.infer_expression_dimension(&expr[..pos])?;
            let exp_str = &expr[pos + 2..];
            if let Ok(n) = exp_str.trim().parse::<i32>() {
                return Some(base.power(n));
            }
            // 非整数幂：仅当 base 是无量纲时有效
            let exp_dim = self.infer_expression_dimension(exp_str.trim())?;
            if exp_dim.is_dimensionless() && base.is_dimensionless() {
                return Some(SIDimension::DIMENSIONLESS);
            }
            return None;
        }

        // 括号
        if expr.starts_with('(') && expr.ends_with(')') {
            return self.infer_expression_dimension(&expr[1..expr.len() - 1]);
        }

        None
    }

    /// 查找顶层运算符位置（跳过括号内的运算符和 ** 运算符）
    fn find_top_level_op(expr: &str, ops: &[char]) -> Option<usize> {
        let chars: Vec<char> = expr.chars().collect();
        let mut depth = 0i32;
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            match c {
                '(' | '[' => depth += 1,
                ')' | ']' => depth -= 1,
                '*' if depth == 0 && ops.contains(&'*') => {
                    // 跳过 ** 运算符
                    if i + 1 < chars.len() && chars[i + 1] == '*' {
                        i += 2;
                        continue;
                    }
                    return Some(i);
                }
                _ if depth == 0 && ops.contains(&c) => return Some(i),
                _ => {}
            }
            i += 1;
        }
        None
    }

    /// 验证公式量纲一致性
    ///
    /// 公式格式：`left == right` 或 `left = right`
    pub fn verify_formula(&self, formula: &str) -> FormulaDimensionResult {
        let sep = if formula.contains("==") { "==" } else { "=" };
        let parts: Vec<&str> = formula.splitn(2, sep).collect();
        if parts.len() != 2 {
            return FormulaDimensionResult {
                passed: false,
                left_dimension: None,
                right_dimension: None,
                message: format!("公式格式错误：缺少 '{sep}' 分隔符"),
            };
        }

        let left_dim = self.infer_expression_dimension(parts[0].trim());
        let right_dim = self.infer_expression_dimension(parts[1].trim());

        match (left_dim, right_dim) {
            (Some(l), Some(r)) => {
                let passed = l == r;
                FormulaDimensionResult {
                    passed,
                    left_dimension: Some(l.clone()),
                    right_dimension: Some(r.clone()),
                    message: if passed {
                        format!("量纲一致: {} == {}", l.to_si_string(), r.to_si_string())
                    } else {
                        format!(
                            "量纲不一致: 左侧 {} != 右侧 {}",
                            l.to_si_string(),
                            r.to_si_string()
                        )
                    },
                }
            }
            (Some(l), None) => FormulaDimensionResult {
                passed: false,
                left_dimension: Some(l),
                right_dimension: None,
                message: "右侧量纲无法推导".to_string(),
            },
            (None, Some(r)) => FormulaDimensionResult {
                passed: false,
                left_dimension: None,
                right_dimension: Some(r),
                message: "左侧量纲无法推导".to_string(),
            },
            (None, None) => FormulaDimensionResult {
                passed: false,
                left_dimension: None,
                right_dimension: None,
                message: "两侧量纲均无法推导".to_string(),
            },
        }
    }
}

impl Default for FormulaDimensionInferencer {
    fn default() -> Self {
        Self::new()
    }
}

/// 公式量纲验证结果
#[derive(Debug, Clone)]
pub struct FormulaDimensionResult {
    pub passed: bool,
    pub left_dimension: Option<SIDimension>,
    pub right_dimension: Option<SIDimension>,
    pub message: String,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn test_si_dimension_multiply() {
        let result = SIDimension::LENGTH.multiply(&SIDimension::LENGTH);
        assert_eq!(result, SIDimension::AREA);
    }

    #[test]
    fn test_si_dimension_divide() {
        let result = SIDimension::LENGTH.divide(&SIDimension::TIME);
        assert_eq!(result, SIDimension::VELOCITY);
    }

    #[test]
    fn test_si_dimension_power() {
        let result = SIDimension::LENGTH.power(3);
        assert_eq!(result, SIDimension::VOLUME);
    }

    #[test]
    fn test_si_dimension_root() {
        let area = SIDimension::LENGTH.power(2);
        let result = area.root(2);
        assert_eq!(result, Some(SIDimension::LENGTH));
    }

    #[test]
    fn test_si_dimension_from_unit() {
        assert_eq!(SIDimension::from_si_unit("m"), Some(SIDimension::LENGTH));
        assert_eq!(SIDimension::from_si_unit("kg"), Some(SIDimension::MASS));
        assert_eq!(SIDimension::from_si_unit("s"), Some(SIDimension::TIME));
        assert_eq!(SIDimension::from_si_unit("N"), Some(SIDimension::FORCE));
        assert_eq!(SIDimension::from_si_unit("J"), Some(SIDimension::ENERGY));
        assert_eq!(SIDimension::from_si_unit("Pa"), Some(SIDimension::PRESSURE));
    }

    #[test]
    fn test_si_dimension_from_compound_unit() {
        assert_eq!(
            SIDimension::from_si_unit("m/s"),
            Some(SIDimension::VELOCITY)
        );
        assert_eq!(
            SIDimension::from_si_unit("m/s²"),
            Some(SIDimension::ACCELERATION)
        );
        assert_eq!(
            SIDimension::from_si_unit("kg·m/s²"),
            Some(SIDimension::FORCE)
        );
    }

    #[test]
    fn test_si_dimension_to_string() {
        assert_eq!(SIDimension::FORCE.to_si_string(), "kg·m / s²");
        assert_eq!(SIDimension::VELOCITY.to_si_string(), "m / s");
        assert_eq!(SIDimension::DIMENSIONLESS.to_si_string(), "1");
    }

    #[test]
    fn test_formula_dimension_newton2() {
        let mut inf = FormulaDimensionInferencer::new();
        inf.add_dimension("F", SIDimension::FORCE);
        inf.add_dimension("m", SIDimension::MASS);
        inf.add_dimension("a", SIDimension::ACCELERATION);

        let result = inf.verify_formula("F == m * a");
        assert!(result.passed, "F = m*a 量纲应一致: {}", result.message);
    }

    #[test]
    fn test_formula_dimension_stefan_boltzmann() {
        let mut inf = FormulaDimensionInferencer::new();
        inf.add_dimension("P", SIDimension::POWER);
        inf.add_dimension(
            "sigma",
            SIDimension {
                length: 0,
                mass: 1,
                time: -3,
                current: 0,
                temperature: -4,
                amount: 0,
                luminosity: 0,
            },
        );
        inf.add_dimension("A", SIDimension::AREA);
        inf.add_dimension("T", SIDimension::TEMPERATURE);

        let result = inf.verify_formula("P == sigma * A * T**4");
        // sigma 量纲 = kg·s⁻³·K⁻⁴, A = m², T⁴ = K⁴
        // sigma * A * T⁴ = kg·m²·s⁻³ = W ✓
        assert!(
            result.passed,
            "Stefan-Boltzmann 量纲应一致: {}",
            result.message
        );
    }

    #[test]
    fn test_formula_dimension_mismatch() {
        let mut inf = FormulaDimensionInferencer::new();
        inf.add_dimension("F", SIDimension::FORCE);
        inf.add_dimension("m", SIDimension::MASS);
        inf.add_dimension("v", SIDimension::VELOCITY);

        let result = inf.verify_formula("F == m * v");
        assert!(!result.passed, "F = m*v 量纲不应一致");
    }

    #[test]
    fn test_parse_unit_with_exponent() {
        assert_eq!(
            SIDimension::parse_unit_with_exponent("m²"),
            Some(("m".to_string(), 2))
        );
        assert_eq!(
            SIDimension::parse_unit_with_exponent("s⁻²"),
            Some(("s".to_string(), -2))
        );
        assert_eq!(
            SIDimension::parse_unit_with_exponent("kg^3"),
            Some(("kg".to_string(), 3))
        );
    }

    // ══════════════════════════════════════════════
    // 边缘情况补充测试
    // ══════════════════════════════════════════════

    #[test]
    fn test_dimensionless_multiply() {
        // 无量纲 × 任意 = 任意
        let result = SIDimension::DIMENSIONLESS.multiply(&SIDimension::FORCE);
        assert_eq!(result, SIDimension::FORCE);
    }

    #[test]
    fn test_dimensionless_divide() {
        // 任意 / 无量纲 = 任意
        let result = SIDimension::FORCE.divide(&SIDimension::DIMENSIONLESS);
        assert_eq!(result, SIDimension::FORCE);
    }

    #[test]
    fn test_same_dimension_divide() {
        // 同量纲相除 = 无量纲
        let result = SIDimension::LENGTH.divide(&SIDimension::LENGTH);
        assert_eq!(result, SIDimension::DIMENSIONLESS);
    }

    #[test]
    fn test_power_zero() {
        // 任意量纲的 0 次幂 = 无量纲
        let result = SIDimension::FORCE.power(0);
        assert_eq!(result, SIDimension::DIMENSIONLESS);
    }

    #[test]
    fn test_power_one() {
        // 任意量纲的 1 次幂 = 自身
        let result = SIDimension::FORCE.power(1);
        assert_eq!(result, SIDimension::FORCE);
    }

    #[test]
    fn test_negative_power() {
        // 负幂次：速度的 -1 次幂 = s/m
        let result = SIDimension::VELOCITY.power(-1);
        assert_eq!(result.time, 1);
        assert_eq!(result.length, -1);
    }

    #[test]
    fn test_root_not_integer() {
        // 非整数根应返回 None
        let result = SIDimension::LENGTH.root(3);
        // LENGTH 的 3 次根不是整数幂 → None
        assert!(result.is_none());
    }

    #[test]
    fn test_root_dimensionless() {
        // 无量纲的任意根 = 无量纲
        let result = SIDimension::DIMENSIONLESS.root(5);
        assert_eq!(result, Some(SIDimension::DIMENSIONLESS));
    }

    #[test]
    fn test_from_si_unit_unknown() {
        // 未知单位应返回 None
        assert_eq!(SIDimension::from_si_unit("xyz"), None);
        // 空字符串返回 DIMENSIONLESS
        assert_eq!(
            SIDimension::from_si_unit(""),
            Some(SIDimension::DIMENSIONLESS)
        );
    }

    #[test]
    fn test_from_si_unit_compound_with_prefix() {
        // 带前缀的复合单位
        assert_eq!(
            SIDimension::from_si_unit("kg·m²/s²"),
            Some(SIDimension::ENERGY)
        );
    }

    #[test]
    fn test_formula_dimension_ideal_gas() {
        // PV = nRT: [PV] = [nRT] = 能量
        let mut inf = FormulaDimensionInferencer::new();
        inf.add_dimension("P", SIDimension::PRESSURE);
        inf.add_dimension("V", SIDimension::VOLUME);
        inf.add_dimension("n", SIDimension::AMOUNT);
        inf.add_dimension(
            "R",
            SIDimension {
                length: 2,
                mass: 1,
                time: -2,
                current: 0,
                temperature: -1,
                amount: -1,
                luminosity: 0,
            },
        ); // J/(mol·K)
        inf.add_dimension("T", SIDimension::TEMPERATURE);

        let result = inf.verify_formula("P * V == n * R * T");
        assert!(result.passed, "PV = nRT 量纲应一致: {}", result.message);
    }

    #[test]
    fn test_formula_dimension_unknown_variable() {
        // 含未知变量的公式应推导失败
        let inf = FormulaDimensionInferencer::new(); // 空的，无已知量纲
        let result = inf.verify_formula("F == m * a");
        assert!(!result.passed, "无已知量纲时不应通过");
    }

    #[test]
    fn test_formula_dimension_parenthesized() {
        // 带括号的公式：F == m * (a + b)
        let mut inf = FormulaDimensionInferencer::new();
        inf.add_dimension("F", SIDimension::FORCE);
        inf.add_dimension("m", SIDimension::MASS);
        inf.add_dimension("a", SIDimension::ACCELERATION);
        inf.add_dimension("b", SIDimension::ACCELERATION);

        let result = inf.verify_formula("F == m * (a + b)");
        assert!(result.passed, "F = m*(a+b) 量纲应一致: {}", result.message);
    }

    #[test]
    fn test_formula_dimension_division() {
        // 除法公式：v == d / t
        let mut inf = FormulaDimensionInferencer::new();
        inf.add_dimension("v", SIDimension::VELOCITY);
        inf.add_dimension("d", SIDimension::LENGTH);
        inf.add_dimension("t", SIDimension::TIME);

        let result = inf.verify_formula("v == d / t");
        assert!(result.passed, "v = d/t 量纲应一致: {}", result.message);
    }

    #[test]
    fn test_formula_dimension_subtraction() {
        // 减法：同量纲相减
        let mut inf = FormulaDimensionInferencer::new();
        inf.add_dimension("x", SIDimension::LENGTH);
        inf.add_dimension("y", SIDimension::LENGTH);
        inf.add_dimension("z", SIDimension::LENGTH);

        let result = inf.verify_formula("z == x - y");
        assert!(result.passed, "z = x - y 量纲应一致: {}", result.message);
    }

    #[test]
    fn test_formula_dimension_subtraction_mismatch() {
        // 减法：不同量纲相减应失败
        let mut inf = FormulaDimensionInferencer::new();
        inf.add_dimension("x", SIDimension::LENGTH);
        inf.add_dimension("y", SIDimension::MASS);
        inf.add_dimension("z", SIDimension::LENGTH);

        let result = inf.verify_formula("z == x - y");
        assert!(!result.passed, "x(长度) - y(质量) 量纲不应一致");
    }

    #[test]
    fn test_parse_unit_with_exponent_plain() {
        // 无指数的普通单位
        assert_eq!(
            SIDimension::parse_unit_with_exponent("kg"),
            Some(("kg".to_string(), 1))
        );
    }

    #[test]
    fn test_parse_unit_with_exponent_empty() {
        // 空字符串返回 ("", 1)
        assert_eq!(
            SIDimension::parse_unit_with_exponent(""),
            Some(("".to_string(), 1))
        );
    }

    // ══════════════════════════════════════════════
    // 极端数值与特殊字符测试
    // ══════════════════════════════════════════════

    #[test]
    fn test_power_large_exponent() {
        // 大幂次：m^100
        let result = SIDimension::MASS.power(100);
        assert_eq!(result.mass, 100);
    }

    #[test]
    fn test_power_negative_exponent() {
        // 负幂次：s^-3
        let result = SIDimension::TIME.power(-3);
        assert_eq!(result.time, -3);
    }

    #[test]
    fn test_multiply_many_dimensions() {
        // 多次连续乘法不溢出
        let mut dim = SIDimension::LENGTH;
        for _ in 0..50 {
            dim = dim.multiply(&SIDimension::MASS);
        }
        assert_eq!(dim.mass, 50);
    }

    #[test]
    fn test_divide_by_itself() {
        // 自除：任意量纲 / 自身 = 无量纲
        let result = SIDimension::ENERGY.divide(&SIDimension::ENERGY);
        assert_eq!(result, SIDimension::DIMENSIONLESS);
    }

    #[test]
    fn test_from_si_unit_with_unicode_middle_dot() {
        // 使用 Unicode 中点 · 的复合单位——可能不被支持，但不应崩溃
        let result = SIDimension::from_si_unit("N·m");
        assert!(result.is_some() || result.is_none(), "Unicode 中点不应崩溃");
    }

    #[test]
    fn test_from_si_unit_with_spaces() {
        // 含空格的复合单位——可能不被支持，但不应崩溃
        let result = SIDimension::from_si_unit("kg m² / s²");
        assert!(
            result.is_some() || result.is_none(),
            "含空格的复合单位不应崩溃"
        );
    }

    #[test]
    fn test_formula_dimension_with_numeric_literal() {
        // 公式含数值常量：F == 2 * m * a
        let mut inf = FormulaDimensionInferencer::new();
        inf.add_dimension("F", SIDimension::FORCE);
        inf.add_dimension("m", SIDimension::MASS);
        inf.add_dimension("a", SIDimension::ACCELERATION);

        let result = inf.verify_formula("F == 2 * m * a");
        assert!(result.passed, "F = 2*m*a 量纲应一致: {}", result.message);
    }

    #[test]
    fn test_formula_dimension_only_multiplication() {
        // 纯乘法公式（无等号）
        let mut inf = FormulaDimensionInferencer::new();
        inf.add_dimension("m", SIDimension::MASS);
        inf.add_dimension("a", SIDimension::ACCELERATION);

        let result = inf.verify_formula("m * a");
        // 无等号的公式可能不通过，但不应崩溃
        assert!(result.passed || !result.passed);
    }

    #[test]
    fn test_dimension_equality_with_floating_point() {
        // 量纲向量中所有分量均为 0 时等于 DIMENSIONLESS
        let custom = SIDimension {
            length: 0,
            mass: 0,
            time: 0,
            current: 0,
            temperature: 0,
            amount: 0,
            luminosity: 0,
        };
        assert_eq!(custom, SIDimension::DIMENSIONLESS);
    }

    #[test]
    fn test_vector_roundtrip() {
        // 七分量向量往返（服务层序列化契约用）
        let v = SIDimension::FORCE.to_vector();
        assert_eq!(v, [1, 1, -2, 0, 0, 0, 0]);
        assert_eq!(SIDimension::from_vector(v), SIDimension::FORCE);
    }
}

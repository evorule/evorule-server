// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! law_set Schema 门禁 —— law_set v1.0 定稿 schema 的内嵌锚与确定性结构校验
//!
//! # 定位（与 core/rule_schema 同族对称）
//! - rule_schema 守治理判定语义（rule_set：condition/then 域判定），
//!   本 crate 守等式恒真语义（law_set：物理定律等式约束）——两 kind 分野。
//! - schema 源文件：`schemas/` 下三件（law_set / _meta / _shared）。
//!   law_set/v1.0 为定稿全文（单文件自洽，$defs/law 内嵌，无跨文件 $ref）；
//!   _meta/_shared 归 law_set 家族命名空间（$id 隔离，防与 rule_schema 全量件双写漂移）。
//!
//! # 校验两层
//! - **schema 层**：内嵌 law_set/v1.0.json 全文校验（jsonschema Draft202012）。
//! - **formula 结构层**（schema 表达不了的语义边界）：
//!   等式恒真语义以单个 `=` 连接符表达（编译期由消费侧重写为 `==`）；
//!   判定形态运算符（`==` 族）禁入——law_set 与 rule_set 的语义分野结构保证；
//!   formula 引用的标识符必须已在 symbols 声明（结构完整性）。
//!   量纲一致性校验属能力层（依赖单位解析选型），本 crate **不做**。
//!
//! # 正确性保证
//! - 构建期由 build.rs 校验三件合法性 + $id 自洽 + 定稿锚；运行期缓存校验器。
//! - jsonschema 0.21 默认内含 meta-schemas，meta-schema 自洽由测试锁定。

#![forbid(unsafe_code)]
// C5 (unwrap/expect/panic = deny) 仅约束生产代码;测试代码保留 unwrap 惯例
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::collections::{BTreeMap, BTreeSet};

use jsonschema::{Draft, Retrieve, UriRef, Validator};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ============================================================================
// 内嵌 Schema（SSOT：law_set 家族命名空间）
// ============================================================================

const LAW_SET_JSON: &str = include_str!("../schemas/law_set/v1.0.json");
const META_JSON: &str = include_str!("../schemas/_meta/v1.0.json");
const SHARED_JSON: &str = include_str!("../schemas/_shared/v1.0.json");

const LAW_SET_ID: &str = "https://evorule.org/schemas/law_set/v1.0.json";
const META_ID: &str = "https://evorule.org/schemas/law_set/_meta/v1.0.json";
const SHARED_ID: &str = "https://evorule.org/schemas/law_set/_shared/v1.0.json";

// ============================================================================
// serde 类型（law_set v1.0 文档模型）
// ============================================================================

/// 定律计算档位（schema tier enum；缺省 full）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// 轻档（线性代数形态即可求解）
    Light,
    /// 全档（缺省；含幂次/复合单位展开等完整语义）
    #[default]
    Full,
}

/// 定律条目的溯源元数据（source 必填，verified 可选）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LawMetadata {
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified: Option<String>,
}

/// 单条定律（law $def 的强类型镜像）
///
/// symbols/si_units 用 BTreeMap 保证序列化确定性（可回放/可审计）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Law {
    pub id: String,
    pub formula: String,
    pub symbols: BTreeMap<String, String>,
    /// symbol -> pint-parseable SI unit expression
    pub si_units: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub solvable: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub derivation: Vec<String>,
    #[serde(default)]
    pub tier: Tier,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<LawMetadata>,
}

/// law_set 文档顶层（治理骨架 + laws 数组）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LawSet {
    #[serde(rename = "$schema")]
    pub schema: String,
    pub kind: String,
    pub id: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    pub laws: Vec<Law>,
}

// ============================================================================
// 校验结果
// ============================================================================

/// 校验报告：valid + 结构化错误列表（含实例路径），供上层显示明确提示
#[derive(Debug, Clone, Serialize)]
pub struct SchemaReport {
    /// 是否通过
    pub valid: bool,
    /// 校验模式：law_set
    pub mode: &'static str,
    /// 错误列表（每条含实例路径 + 具体原因）
    pub errors: Vec<String>,
}

impl SchemaReport {
    fn ok(mode: &'static str) -> Self {
        SchemaReport {
            valid: true,
            mode,
            errors: Vec::new(),
        }
    }

    fn fail(mode: &'static str, errors: Vec<String>) -> Self {
        SchemaReport {
            valid: false,
            mode,
            errors,
        }
    }
}

// ============================================================================
// 内嵌 schema 解析器（跨文件 $ref 按 $id 解析，jsonschema Retrieve trait）
// ============================================================================

struct EmbeddedRetriever;

impl Retrieve for EmbeddedRetriever {
    fn retrieve(
        &self,
        uri: &UriRef<&str>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let id = uri.as_str();
        let raw = match id {
            LAW_SET_ID => LAW_SET_JSON,
            META_ID => META_JSON,
            SHARED_ID => SHARED_JSON,
            other => {
                return Err(format!("未知 evorule schema $id: {other}").into());
            }
        };
        serde_json::from_str(raw).map_err(|e| e.into())
    }
}

// ============================================================================
// 校验器缓存
// ============================================================================

struct Validators {
    law_set: Validator,
}

// build.rs 已在构建期保证三件 schema 合法且 $id 自洽，故此处的
// expect 是「静态输入必然成功」的不变量（非运行时错误路径）。
#[allow(clippy::expect_used)]
fn build_validators() -> Validators {
    let law_set: Value =
        serde_json::from_str(LAW_SET_JSON).expect("内嵌 law_set schema 非法（build.rs 已保证）");

    let mut opts = Validator::options();
    opts.with_draft(Draft::Draft202012);
    opts.with_retriever(EmbeddedRetriever);
    let law_set_v = opts
        .build(&law_set)
        .expect("构建 law_set 校验器失败（build.rs 已保证 schema 合法）");

    Validators { law_set: law_set_v }
}

static VALIDATORS: Lazy<Validators> = Lazy::new(build_validators);

// ============================================================================
// formula 结构校验（等式恒真语义的执行保证）
// ============================================================================

/// 判定形态运算符（condition 语义形态）——law_set 禁入
const BANNED_COMPARISON_OPS: [&str; 4] = ["==", "!=", "<=", ">="];

/// 提取 formula 中的标识符（连续 [A-Za-z_][A-Za-z0-9_]* 序列）
fn formula_identifiers(formula: &str) -> Vec<String> {
    let bytes = formula.as_bytes();
    let mut idents = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            idents.push(formula[start..i].to_string());
        } else {
            i += 1;
        }
    }
    idents
}

/// 单条定律的 formula 结构校验（错误累积入 errors）
///
/// - 判定形态运算符（`==` 族）禁入：law_set 与 rule_set 的语义分野结构保证。
///   命中后跳过等式计数检查（避免误导性报因），符号完整性照查。
/// - 恰好一个 `=` 连接符且两侧非空（等式恒真语义；编译期由消费侧重写为 `==`）。
/// - 引用的标识符必须已在 symbols 声明（结构完整性；
///   量纲一致性属能力层，本 crate 不做）。
fn check_formula_structure(idx: usize, law: &Law, errors: &mut Vec<String>) {
    let at = format!("laws[{idx}] (id={}): ", law.id);
    let f = law.formula.as_str();

    if let Some(op) = BANNED_COMPARISON_OPS.iter().find(|op| f.contains(*op)) {
        errors.push(format!(
            "{at}formula 含判定形态运算符 `{op}`（law_set 仅允许等式恒真语义：等式以单个 `=` 表达，禁止 condition 判定语义）"
        ));
    } else {
        match f.matches('=').count() {
            0 => errors.push(format!("{at}formula 缺等式连接符 `=`（非等式恒真语义）")),
            n if n >= 2 => errors.push(format!(
                "{at}formula 含 {n} 个 `=` 连接符（等式恒真语义要求恰好一个）"
            )),
            _ => {
                if let Some((lhs, rhs)) = f.split_once('=') {
                    if lhs.trim().is_empty() || rhs.trim().is_empty() {
                        errors.push(format!("{at}formula 等式两侧存在空表达式"));
                    }
                }
            }
        }
    }

    let undeclared: BTreeSet<String> = formula_identifiers(f)
        .into_iter()
        .filter(|s| !law.symbols.contains_key(s.as_str()))
        .collect();
    if !undeclared.is_empty() {
        let names: Vec<String> = undeclared.into_iter().collect();
        errors.push(format!(
            "{at}formula 引用未声明符号 [{}]（须在 symbols 声明）",
            names.join(", ")
        ));
    }
}

// ============================================================================
// 公共 API
// ============================================================================

/// 校验完整 law_set 文档（治理骨架 + laws 数组 + formula 结构层）。
///
/// 用于：law_set 部署件装载、API 提交、golden 验证。
pub fn validate_law_set(doc: &Value) -> SchemaReport {
    let mut errors: Vec<String> = match VALIDATORS.law_set.validate(doc) {
        Ok(()) => Vec::new(),
        Err(iter) => iter
            .map(|e| format!("{}: {}", e.instance_path, e))
            .collect(),
    };
    // formula 结构层（schema 表达不了的语义边界）：schema 校验失败条目的
    // formula 可能不可反序列化，逐条 best-effort；反序列化失败处 schema 层已报因。
    if let Some(laws) = doc.get("laws").and_then(|v| v.as_array()) {
        for (idx, item) in laws.iter().enumerate() {
            if let Ok(law) = serde_json::from_value::<Law>(item.clone()) {
                check_formula_structure(idx, &law, &mut errors);
            }
        }
    }
    if errors.is_empty() {
        SchemaReport::ok("law_set")
    } else {
        SchemaReport::fail("law_set", errors)
    }
}

// ============================================================================
// 测试（golden 四件 + 结构反例 + schema 自洽）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── golden 三件（G1/G2/G3：LawSet 反序列化 + validate 全过）──

    fn golden_newton() -> Value {
        json!({
            "$schema": "https://evorule.org/schemas/law_set/v1.0.json",
            "kind": "law_set",
            "id": "newton-second-law",
            "version": "1.0.0",
            "metadata": { "title": "Newton 第二定律" },
            "laws": [{
                "id": "newton_second",
                "formula": "F = m * a",
                "symbols": { "F": "合外力", "m": "质量", "a": "加速度" },
                "si_units": { "F": "newton", "m": "kilogram", "a": "m*s^-2" },
                "solvable": ["F", "m", "a"],
                "tier": "light",
                "metadata": { "source": "Newton, Principia (1687)" }
            }]
        })
    }

    fn golden_einstein() -> Value {
        json!({
            "$schema": "https://evorule.org/schemas/law_set/v1.0.json",
            "kind": "law_set",
            "id": "mass-energy-equivalence",
            "version": "1.0.0",
            "laws": [{
                "id": "einstein_mass_energy",
                "formula": "E = m * c^2",
                "symbols": { "E": "能量", "m": "质量", "c": "真空光速" },
                "si_units": { "E": "joule", "m": "kilogram", "c": "m*s^-1" },
                "solvable": ["E", "m"],
                "tier": "full"
            }]
        })
    }

    fn golden_ideal_gas() -> Value {
        json!({
            "$schema": "https://evorule.org/schemas/law_set/v1.0.json",
            "kind": "law_set",
            "id": "ideal-gas-law",
            "version": "1.0.0",
            "laws": [{
                "id": "ideal_gas",
                "formula": "P * V = n * R * T",
                "symbols": {
                    "P": "压强", "V": "体积", "n": "物质的量",
                    "R": "理想气体常数", "T": "热力学温度"
                },
                "si_units": {
                    "P": "pascal", "V": "m^3", "n": "mol",
                    "R": "joule*mol^-1*K^-1", "T": "kelvin"
                }
            }]
        })
    }

    #[test]
    fn golden_g1_newton_passes() {
        let doc = golden_newton();
        let lawset: LawSet =
            serde_json::from_value(doc.clone()).expect("G1 Newton 应可反序列化为 LawSet");
        assert_eq!(lawset.laws.len(), 1);
        assert_eq!(lawset.laws[0].tier, Tier::Light);
        assert_eq!(lawset.laws[0].solvable, vec!["F", "m", "a"]);
        let report = validate_law_set(&doc);
        assert!(report.valid, "G1 Newton 应通过: {:?}", report.errors);
    }

    #[test]
    fn golden_g2_einstein_passes() {
        let doc = golden_einstein();
        let lawset: LawSet =
            serde_json::from_value(doc.clone()).expect("G2 Einstein 应可反序列化为 LawSet");
        assert_eq!(lawset.laws[0].solvable, vec!["E", "m"]);
        assert_eq!(lawset.laws[0].tier, Tier::Full);
        let report = validate_law_set(&doc);
        assert!(report.valid, "G2 Einstein 应通过: {:?}", report.errors);
    }

    #[test]
    fn golden_g3_ideal_gas_passes_with_defaults() {
        let doc = golden_ideal_gas();
        let lawset: LawSet =
            serde_json::from_value(doc.clone()).expect("G3 理想气体应可反序列化为 LawSet");
        // 顶层 metadata 缺省 + tier 缺省 full（serde default 与 schema default 同语义）
        assert!(lawset.metadata.is_none());
        assert_eq!(lawset.laws[0].tier, Tier::Full);
        assert!(lawset.laws[0].solvable.is_empty());
        let report = validate_law_set(&doc);
        assert!(report.valid, "G3 理想气体应通过: {:?}", report.errors);
    }

    // ── G4 结构反例（拒收 + 报因）──

    #[test]
    fn golden_g4_undeclared_symbol_rejected_with_reason() {
        // 结构反例（07 档 G4 原样 F = m * v）：formula 引用未声明符号 v。
        // 量纲一致性属能力层，本 crate 在结构层拒收并报因。
        let mut doc = golden_newton();
        doc["laws"][0]["formula"] = json!("F = m * v");
        let report = validate_law_set(&doc);
        assert!(!report.valid, "G4 反例（未声明符号 v）应被拒");
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("未声明符号") && e.contains("v")),
            "报因须指明未声明符号 v: {:?}",
            report.errors
        );
    }

    // ── formula 结构层：判定语义禁入 + 等式结构边界 ──

    #[test]
    fn formula_comparison_operator_rejected() {
        // 判定形态等号（== 族）禁入：law_set 与 rule_set 的语义分野结构保证
        let mut doc = golden_newton();
        doc["laws"][0]["formula"] = json!("F == m * a");
        let report = validate_law_set(&doc);
        assert!(!report.valid, "formula 含 == 应被拒");
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("判定形态运算符") && e.contains("==")),
            "报因须指明判定形态运算符: {:?}",
            report.errors
        );
    }

    #[test]
    fn formula_missing_equals_rejected() {
        let mut doc = golden_newton();
        doc["laws"][0]["formula"] = json!("m * a");
        let report = validate_law_set(&doc);
        assert!(!report.valid, "formula 缺等式连接符应被拒");
        assert!(
            report.errors.iter().any(|e| e.contains("缺等式连接符")),
            "报因须指明缺等式连接符: {:?}",
            report.errors
        );
    }

    #[test]
    fn formula_multiple_equals_rejected() {
        let mut doc = golden_newton();
        doc["laws"][0]["formula"] = json!("F = m = a");
        let report = validate_law_set(&doc);
        assert!(!report.valid, "formula 多个等式连接符应被拒");
        assert!(
            report.errors.iter().any(|e| e.contains("恰好一个")),
            "报因须指明等式连接符数量越界: {:?}",
            report.errors
        );
    }

    #[test]
    fn formula_empty_side_rejected() {
        let mut doc = golden_newton();
        doc["laws"][0]["formula"] = json!("F = ");
        let report = validate_law_set(&doc);
        assert!(!report.valid, "formula 等式右侧空应被拒");
        assert!(
            report.errors.iter().any(|e| e.contains("空表达式")),
            "报因须指明空表达式: {:?}",
            report.errors
        );
    }

    // ── schema 层（定稿结构）──

    #[test]
    fn kind_mismatch_rejected() {
        let mut doc = golden_newton();
        doc["kind"] = json!("rule_set");
        let report = validate_law_set(&doc);
        assert!(!report.valid, "kind 非 law_set 应被拒");
    }

    #[test]
    fn law_unknown_field_rejected() {
        // law 条目 additionalProperties:false：未知键 fail-fast
        let mut doc = golden_newton();
        doc["laws"][0]["condition"] = json!({ "type": "eq" });
        let report = validate_law_set(&doc);
        assert!(!report.valid, "law 条目未知键 condition 应被拒");
        assert!(
            report.errors.iter().any(|e| e.contains("condition")),
            "报因须列明未知键名: {:?}",
            report.errors
        );
    }

    #[test]
    fn laws_over_limit_rejected() {
        // maxItems 128 定稿锚：129 条拒收
        let mut doc = golden_newton();
        let law = doc["laws"][0].clone();
        let mut many = Vec::new();
        for i in 0..129 {
            let mut one = law.clone();
            one["id"] = json!(format!("law_{i}"));
            one["formula"] = json!(format!("F{i} = m{i} * a{i}"));
            one["symbols"] = json!({
                (format!("F{i}")): "f",
                (format!("m{i}")): "m",
                (format!("a{i}")): "a"
            });
            one["si_units"] = json!({
                (format!("F{i}")): "newton",
                (format!("m{i}")): "kilogram",
                (format!("a{i}")): "m*s^-2"
            });
            many.push(one);
        }
        doc["laws"] = Value::Array(many);
        let report = validate_law_set(&doc);
        assert!(!report.valid, "laws 超 128 条应被拒");
    }

    #[test]
    fn symbols_below_min_rejected() {
        // minProperties 2：symbols 单符号拒收
        let mut doc = golden_newton();
        doc["laws"][0]["symbols"] = json!({ "F": "合外力" });
        let report = validate_law_set(&doc);
        assert!(!report.valid, "symbols 少于 2 项应被拒");
    }

    #[test]
    fn non_object_doc_rejected() {
        let report = validate_law_set(&json!([1, 2]));
        assert!(!report.valid, "顶层数组应被拒（必须 object）");
    }

    // ── schema 自洽（meta-schema 过）──

    #[test]
    fn law_set_schema_passes_meta_schema() {
        // 验收面：law_set/v1.0 定稿文档本身是合法 draft 2020-12 schema。
        // jsonschema 0.21 默认内含 meta-schemas，可直接构建 $ref 到 meta 的校验器。
        let schema: Value =
            serde_json::from_str(LAW_SET_JSON).expect("内嵌 law_set schema 必须可解析");
        let meta_ref: Value = json!({ "$ref": "https://json-schema.org/draft/2020-12/schema" });
        let mut opts = Validator::options();
        opts.with_draft(Draft::Draft202012);
        let meta_validator = opts.build(&meta_ref).expect("meta-schema 校验器构建失败");
        let errors: Vec<String> = match meta_validator.validate(&schema) {
            Ok(()) => Vec::new(),
            Err(iter) => iter
                .map(|e| format!("{}: {}", e.instance_path, e))
                .collect(),
        };
        assert!(
            errors.is_empty(),
            "law_set schema 应过 meta-schema: {errors:?}"
        );
    }

    // ── serde 确定性 ──

    #[test]
    fn lawset_serde_roundtrip_stable() {
        // BTreeMap + 固定字段序：序列化确定性（可回放/可审计）
        let doc = golden_newton();
        let lawset: LawSet = serde_json::from_value(doc.clone()).expect("反序列化");
        let round: LawSet =
            serde_json::from_value(serde_json::to_value(&lawset).expect("序列化")).expect("回读");
        assert_eq!(lawset, round, "LawSet serde 往返应等值");
    }
}

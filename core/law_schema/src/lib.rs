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
//! # 校验三层
//! - **schema 层**：内嵌 law_set/v1.0.json 全文校验（jsonschema Draft202012）。
//! - **formula 结构层**（schema 表达不了的语义边界）：
//!   等式恒真语义以单个 `=` 连接符表达（编译期由消费侧重写为 `==`）；
//!   判定形态运算符（`==` 族）禁入——law_set 与 rule_set 的语义分野结构保证；
//!   formula 引用的标识符必须已在 symbols 声明（结构完整性）。
//! - **量纲能力层**（`dimensional` 模块，可选启用）：law 引用符号全部有
//!   si_units 时校验等式两侧量纲一致性（不一致硬拒收并报两侧量纲向量）；
//!   缺 si_units / 单位不可解析的定律跳过并记 warning 级结果，不硬拒
//!   （兼容 G 类纯数学定律）。
//! - **域归属/跨域引用边层**：律式 `domains`（多域归属）与 `cross_refs`
//!   （跨域引用边，D2=A：注册时显式声明，禁自动发现）落点域必须在域目录
//!   注册，且引用边落点须属于目标律式的域归属、域内引用不得声明为跨域边。
//!
//! # 域目录嵌入副本
//! 治理侧 SSOT = evorule-rule `src/model/domain_catalog.v1.json`，
//! 本 crate 持嵌入副本，由 scripts/sync-domain-catalog.ps1 同步（CI 漂移检查 fail-closed）。
//!
//! # 正确性保证
//! - 构建期由 build.rs 校验三件合法性 + $id 自洽 + 定稿锚；运行期缓存校验器。
//! - jsonschema 0.21 默认内含 meta-schemas，meta-schema 自洽由测试锁定。

#![forbid(unsafe_code)]
// C5 (unwrap/expect/panic = deny) 仅约束生产代码;测试代码保留 unwrap 惯例
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::collections::{BTreeMap, BTreeSet};

pub mod dimensional;

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

/// 跨域引用边（D2=A：注册时显式声明，禁自动发现）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrossRef {
    /// 被引用律式 id（同集内）
    pub target: String,
    /// 引用边落点域（跨域声明）
    pub domain: String,
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
    /// 多域归属（域向量扩多域；缺省 = 单域 domain 字段语义不变）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub domains: Vec<String>,
    /// 跨域引用边显式声明（D2=A：注册时声明，禁自动发现）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cross_refs: Vec<CrossRef>,
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
// 域目录（嵌入副本；治理侧 SSOT = evorule-rule src/model/domain_catalog.v1.json）
// ============================================================================

/// 域目录条目（域 id -> 名称/描述）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainCatalogEntry {
    pub domain_id: String,
    pub name: String,
    pub description: String,
}

/// 域目录嵌入副本（治理侧 SSOT = evorule-rule `src/model/domain_catalog.v1.json`，
/// 由 scripts/sync-domain-catalog.ps1 同步，CI 域目录漂移检查 fail-closed）
const DOMAIN_CATALOG_JSON: &str = include_str!("domain_catalog.embedded.json");

// 重复 domain_id 属静态输入不变量破坏（嵌入副本经同步脚本自 SSOT 生成），
// 与 build_validators 同口径：expect fail-fast（非运行时错误路径）。
#[allow(clippy::expect_used)]
fn build_domain_catalog() -> BTreeMap<String, DomainCatalogEntry> {
    let raw: Value = serde_json::from_str(DOMAIN_CATALOG_JSON)
        .expect("域目录嵌入副本非法 JSON（静态输入不变量）");
    let entries: Vec<DomainCatalogEntry> =
        serde_json::from_value(raw.get("domains").cloned().unwrap_or(Value::Null))
            .expect("域目录嵌入副本缺 domains 数组或条目形态非法（静态输入不变量）");
    let mut map = BTreeMap::new();
    for entry in entries {
        let is_new = map.insert(entry.domain_id.clone(), entry).is_none();
        is_new
            .then_some(())
            .expect("域目录嵌入副本含重复 domain_id（静态输入不变量破坏）");
    }
    map
}

static DOMAIN_CATALOG: Lazy<BTreeMap<String, DomainCatalogEntry>> = Lazy::new(build_domain_catalog);

/// 域目录（嵌入副本单源；criticality 等下游复用本函数，禁止各自再持副本）
pub fn domain_catalog() -> &'static BTreeMap<String, DomainCatalogEntry> {
    &DOMAIN_CATALOG
}

/// 域 id 命名规约（pattern `^[a-z][a-z0-9_]*$` 的结构层镜像；schema 层已守
/// 命名规约，此处供 criticality 等下游做防御性复检与单源复用）
pub fn is_valid_domain_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    match bytes.first() {
        Some(b) if b.is_ascii_lowercase() => {}
        _ => return false,
    }
    bytes[1..]
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
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
    /// warning 级结果（量纲能力层跳过等；不影响 valid，不构成拒收）
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl SchemaReport {
    fn ok(mode: &'static str) -> Self {
        SchemaReport {
            valid: true,
            mode,
            errors: Vec::new(),
            warnings: Vec::new(),
        }
    }

    fn fail(mode: &'static str, errors: Vec<String>) -> Self {
        SchemaReport {
            valid: false,
            mode,
            errors,
            warnings: Vec::new(),
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
/// - 引用的标识符必须已在 symbols 声明（结构完整性）。
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
// 域归属/跨域引用边校验（D2=A：注册时显式声明，禁自动发现）
// ============================================================================

/// 单条定律的域归属校验：domains 逐个必须在域目录注册（schema 层已守命名
/// 规约，此处守注册在册性——目录为治理侧 SSOT 嵌入副本）
fn check_law_domains(idx: usize, law: &Law, errors: &mut Vec<String>) {
    if law.domains.is_empty() {
        return;
    }
    let catalog = domain_catalog();
    let at = format!("laws[{idx}] (id={}): ", law.id);
    for d in &law.domains {
        if !catalog.contains_key(d) {
            errors.push(format!("{at}domains 含未注册域 `{d}`（须先在域目录注册）"));
        }
    }
}

/// 单条定律的跨域引用边校验。
///
/// ① target 必须在同集 laws 内存在（否则硬错误报因含 target id）；
/// ② domain 必须在域目录注册（否则硬错误）；
/// ③ domain 必须属于目标律式的域归属（目标 domains 数组 ∪ 旧 domain 字段；
///    目标无任何域声明 → 硬错误「目标律式未声明域归属，跨域引用边落点不可判定」）；
/// ④ 域内引用不得声明为跨域引用边（本律式自身声明 domains 且落点 ∈ 自身 domains）。
///
/// laws_by_id = 全 set 已反序列化定律（id → 定律）；目标条目反序列化失败的
/// 情形 schema 层已报因，此处 best-effort（与 formula 结构层同口径）。
fn check_cross_refs(
    idx: usize,
    law: &Law,
    laws_by_id: &BTreeMap<&str, &Law>,
    errors: &mut Vec<String>,
) {
    if law.cross_refs.is_empty() {
        return;
    }
    let catalog = domain_catalog();
    let at = format!("laws[{idx}] (id={}): ", law.id);
    for edge in &law.cross_refs {
        let Some(target) = laws_by_id.get(edge.target.as_str()) else {
            errors.push(format!(
                "{at}跨域引用边 target `{}` 不在同集 laws 内（引用边仅指向同集律式，禁自动发现）",
                edge.target
            ));
            continue;
        };
        if !catalog.contains_key(&edge.domain) {
            errors.push(format!(
                "{at}跨域引用边 domain `{}` 未在域目录注册",
                edge.domain
            ));
            continue;
        }
        // ③ 目标律式的域归属（domains 数组 ∪ 旧 domain 字段）
        let mut attribution: Vec<&str> = target.domains.iter().map(String::as_str).collect();
        if let Some(d) = &target.domain {
            attribution.push(d);
        }
        if attribution.is_empty() {
            errors.push(format!(
                "{at}目标律式未声明域归属，跨域引用边落点不可判定：目标律式 `{}`（domains/domain 均缺）",
                edge.target
            ));
            continue;
        }
        if !attribution.contains(&edge.domain.as_str()) {
            errors.push(format!(
                "{at}跨域引用边 domain `{}` 不属于目标律式 `{}` 的域归属 [{}]（落点须为目标声明域之一）",
                edge.domain,
                edge.target,
                attribution.join(", ")
            ));
            continue;
        }
        // ④ 域内引用不得声明为跨域引用边
        if !law.domains.is_empty() && law.domains.contains(&edge.domain) {
            errors.push(format!(
                "{at}跨域引用边 domain `{}` 属于本律式自身 domains（域内引用不得声明为跨域引用边）",
                edge.domain
            ));
        }
    }
}

// ============================================================================
// 量纲能力层接线
// ============================================================================

/// 单条定律的量纲能力层接线：不一致 → 硬错误；跳过 → warning 级结果（不拒收）
fn apply_dimensional_verdict(
    idx: usize,
    law: &Law,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let at = format!("laws[{idx}] (id={}): ", law.id);
    match dimensional::check_law(law) {
        dimensional::DimensionalVerdict::Consistent => {}
        dimensional::DimensionalVerdict::Mismatch { lhs, rhs } => errors.push(format!(
            "{at}等式两侧量纲不一致（按 SI 七基本量纲展开）：{lhs}；{rhs}"
        )),
        dimensional::DimensionalVerdict::AdditiveMismatch { report } => errors.push(format!(
            "{at}等式内加减两侧操作分量纲不一致（量纲违例，硬拒收）：{report}"
        )),
        dimensional::DimensionalVerdict::Skipped(reason) => {
            warnings.push(format!("{at}量纲校验跳过（warning 级，不拒收）：{reason}"));
        }
    }
}

// ============================================================================
// 公共 API
// ============================================================================

/// 校验完整 law_set 文档（治理骨架 + laws 数组 + formula 结构层 + 量纲能力层）。
///
/// 用于：law_set 部署件装载、API 提交、golden 验证。
/// 量纲能力层为可选启用（law 引用符号全部有 si_units 才生效），
/// 跳过情况记入 `SchemaReport.warnings`（warning 级，不影响 valid）。
pub fn validate_law_set(doc: &Value) -> SchemaReport {
    let mut errors: Vec<String> = match VALIDATORS.law_set.validate(doc) {
        Ok(()) => Vec::new(),
        Err(iter) => iter
            .map(|e| format!("{}: {}", e.instance_path, e))
            .collect(),
    };
    // formula 结构层 + 域归属/跨域引用边层 + 量纲能力层（schema 表达不了的
    // 语义边界）：schema 校验失败条目的 formula 可能不可反序列化，逐条
    // best-effort；反序列化失败处 schema 层已报因。
    let mut warnings: Vec<String> = Vec::new();
    if let Some(laws) = doc.get("laws").and_then(|v| v.as_array()) {
        // 先收集全 set 已反序列化定律（id → 定律），供跨域引用边 target
        // 存在性与目标域归属判定检索。
        let parsed: Vec<(usize, Law)> = laws
            .iter()
            .enumerate()
            .filter_map(|(idx, item)| {
                serde_json::from_value::<Law>(item.clone())
                    .ok()
                    .map(|law| (idx, law))
            })
            .collect();
        let laws_by_id: BTreeMap<&str, &Law> = parsed
            .iter()
            .map(|(_, law)| (law.id.as_str(), law))
            .collect();
        for (idx, law) in &parsed {
            check_formula_structure(*idx, law, &mut errors);
            check_law_domains(*idx, law, &mut errors);
            check_cross_refs(*idx, law, &laws_by_id, &mut errors);
            apply_dimensional_verdict(*idx, law, &mut errors, &mut warnings);
        }
    }
    let mut report = if errors.is_empty() {
        SchemaReport::ok("law_set")
    } else {
        SchemaReport::fail("law_set", errors)
    };
    report.warnings = warnings;
    report
}

// ============================================================================
// 测试（golden 四件 + 量纲五件 + 结构反例 + schema 自洽）
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

    // ── 量纲能力层 golden（G5-G9：一致过/不一致拒收报两侧量纲/缺 si_units 跳过）──

    #[test]
    fn golden_dim_g5_mismatch_rejected_reporting_both_vectors() {
        // G5 反例（07 档 G4 量纲版）：F = m * v，v 已声明且带 si_units。
        // 结构层过、量纲层拒收，报因含两侧量纲向量（kg*m*s^-2 vs kg*m*s^-1）。
        let mut doc = golden_newton();
        doc["laws"][0]["formula"] = json!("F = m * v");
        doc["laws"][0]["symbols"] = json!({ "F": "合外力", "m": "质量", "v": "速度" });
        doc["laws"][0]["si_units"] = json!({
            "F": "newton", "m": "kilogram", "v": "m*s^-1"
        });
        let report = validate_law_set(&doc);
        assert!(!report.valid, "量纲不一致应硬拒收");
        let msg = report
            .errors
            .iter()
            .find(|e| e.contains("量纲不一致"))
            .unwrap_or_else(|| panic!("报因须指明量纲不一致: {:?}", report.errors));
        assert!(
            msg.contains("kg*m*s^-2") && msg.contains("m*s^-1"),
            "报因须含两侧量纲向量: {msg}"
        );
    }

    #[test]
    fn golden_dim_g10_currency_length_mix_rejected_reporting_both_vectors() {
        // G10（货币维反例）：米/元混加律式硬拒收——货币维使「元」类记号可解析，
        // 混加不再落入「未知单位跳过」的盲区，报因含两侧量纲向量（m vs currency）。
        let mut doc = golden_newton();
        doc["laws"][0]["formula"] = json!("total = length + price");
        doc["laws"][0]["symbols"] = json!({ "total": "总额", "length": "长度", "price": "单价" });
        doc["laws"][0]["si_units"] = json!({
            "total": "CNY", "length": "m", "price": "CNY*m^-1"
        });
        let report = validate_law_set(&doc);
        assert!(!report.valid, "米/元混加应硬拒收");
        let msg = report
            .errors
            .iter()
            .find(|e| e.contains("量纲不一致"))
            .unwrap_or_else(|| panic!("报因须指明量纲不一致: {:?}", report.errors));
        assert!(
            msg.contains("currency") && msg.contains("m"),
            "报因须含两侧量纲向量（m vs currency）: {msg}"
        );
    }

    #[test]
    fn golden_dim_g11_currency_same_dim_consistent_across_currencies() {
        // G11（货币维正例）：USD 与 CNY 记号异币种同归货币维——量纲层一致通过
        // （币种间量值换算不归量纲层，由换算律管量值）。
        let mut doc = golden_newton();
        doc["laws"][0]["formula"] = json!("total_usd = amount_cny * rate");
        doc["laws"][0]["symbols"] = json!({
            "total_usd": "美元总额", "amount_cny": "人民币金额", "rate": "汇率"
        });
        doc["laws"][0]["si_units"] = json!({
            "total_usd": "USD", "amount_cny": "CNY", "rate": "USD*CNY^-1"
        });
        let report = validate_law_set(&doc);
        assert!(
            report.valid,
            "异币种同货币维应判一致通过: {:?}",
            report.errors
        );
    }

    #[test]
    fn golden_dim_g6_missing_si_units_symbol_warns_but_not_rejected() {
        // G6：formula 引用符号缺 si_units → 跳过量纲校验记 warning，不硬拒
        // （G 类纯数学定律兼容；valid 保持 true）
        let mut doc = golden_newton();
        doc["laws"][0]["formula"] = json!("F = m * x");
        doc["laws"][0]["symbols"] = json!({ "F": "合外力", "m": "质量", "x": "任意标量" });
        let report = validate_law_set(&doc);
        assert!(
            report.valid,
            "缺 si_units 符号应跳过不拒收: {:?}",
            report.errors
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains('x') && w.contains("缺 si_units")),
            "warning 须指明缺 si_units 的符号: {:?}",
            report.warnings
        );
    }

    #[test]
    fn golden_dim_g7_pure_math_law_without_si_units_warns_not_rejected() {
        // G7：si_units 为空（纯数学定律）→ 全符号缺 si_units，整体跳过不拒收
        let mut doc = golden_newton();
        doc["laws"][0]["formula"] = json!("y = a * x");
        doc["laws"][0]["symbols"] = json!({ "y": "因变量", "a": "系数", "x": "自变量" });
        doc["laws"][0]["si_units"] = json!({});
        let report = validate_law_set(&doc);
        assert!(
            report.valid,
            "纯数学定律应跳过量纲校验不拒收: {:?}",
            report.errors
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("y") && w.contains("a") && w.contains("x")),
            "warning 须列出全部缺 si_units 符号: {:?}",
            report.warnings
        );
    }

    #[test]
    fn golden_dim_g8_division_and_number_literal_pass() {
        // G8：除法连接（v = d / t）+ 数值常量因子量纲中性（E = 0.5 * m * v^2）
        let mut doc = golden_newton();
        doc["laws"][0]["formula"] = json!("v = d / t");
        doc["laws"][0]["symbols"] = json!({ "v": "速度", "d": "距离", "t": "时间" });
        doc["laws"][0]["si_units"] = json!({ "v": "m*s^-1", "d": "m", "t": "s" });
        let report = validate_law_set(&doc);
        assert!(report.valid, "除法量纲一致应过: {:?}", report.errors);
        assert!(report.warnings.is_empty(), "不应有跳过 warning");

        let mut doc = golden_einstein();
        doc["laws"][0]["formula"] = json!("E = 0.5 * m * v^2");
        doc["laws"][0]["symbols"] = json!({ "E": "动能", "m": "质量", "v": "速度" });
        doc["laws"][0]["si_units"] = json!({ "E": "joule", "m": "kilogram", "v": "m*s^-1" });
        let report = validate_law_set(&doc);
        assert!(report.valid, "数值常量应量纲中性: {:?}", report.errors);
    }

    #[test]
    fn golden_dim_g9_unparseable_unit_name_warns_but_not_rejected() {
        // G9：si_units 单位名不可解析（拼写错误）→ 跳过记 warning，不硬拒
        let mut doc = golden_newton();
        doc["laws"][0]["si_units"]["F"] = json!("newtn");
        let report = validate_law_set(&doc);
        assert!(report.valid, "未知单位应跳过不拒收: {:?}", report.errors);
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("未知单位") && w.contains("newtn")),
            "warning 须指明不可解析单位: {:?}",
            report.warnings
        );
    }

    // ── 域归属/跨域引用边 golden（G12-G16：多域归属 + 引用边四判据 + law 自身域注册）──

    /// 跨域引用边的目标律式（默认声明 physical 域；测试按需改写）
    fn golden_target_law() -> Value {
        json!({
            "id": "grav_identity",
            "formula": "w = g * m",
            "symbols": { "w": "重量", "g": "重力加速度", "m": "质量" },
            "si_units": {},
            "domains": ["physical"]
        })
    }

    #[test]
    fn golden_g12_multidomain_with_legal_cross_refs_passes() {
        // G12 正例：多域归属 + 合法跨域引用边（落点 = 目标声明域，且不属于
        // 本律式自身 domains）。目标律式以旧单域 domain 字段声明归属
        // （③ 的旧字段归并路径同验）；目标纯数学形态，量纲层跳过记 warning。
        let mut doc = golden_newton();
        doc["laws"][0]["domains"] = json!(["physical", "engineering"]);
        doc["laws"][0]["cross_refs"] =
            json!([{ "target": "price_identity", "domain": "economic" }]);
        let mut target = golden_target_law();
        target["id"] = json!("price_identity");
        target
            .as_object_mut()
            .expect("target 应为 object")
            .remove("domains");
        target["domain"] = json!("economic");
        doc["laws"]
            .as_array_mut()
            .expect("laws 应为数组")
            .push(target);

        let lawset: LawSet =
            serde_json::from_value(doc.clone()).expect("G12 应可反序列化为 LawSet");
        assert_eq!(lawset.laws[0].domains, vec!["physical", "engineering"]);
        assert_eq!(
            lawset.laws[0].cross_refs,
            vec![CrossRef {
                target: "price_identity".to_string(),
                domain: "economic".to_string(),
            }]
        );
        assert_eq!(lawset.laws[1].domain.as_deref(), Some("economic"));
        let report = validate_law_set(&doc);
        assert!(
            report.valid,
            "G12 多域+合法跨域引用边应通过: {:?}",
            report.errors
        );
    }

    #[test]
    fn golden_g13_cross_ref_target_missing_rejected() {
        // G13 反例：target 不在同集 laws 内 → 拒收，报因含 target id
        let mut doc = golden_newton();
        doc["laws"][0]["domains"] = json!(["physical"]);
        doc["laws"][0]["cross_refs"] = json!([{ "target": "missing_law", "domain": "physical" }]);
        let report = validate_law_set(&doc);
        assert!(!report.valid, "G13 target 不存在应拒收");
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("不在同集 laws 内") && e.contains("missing_law")),
            "报因须含 target id: {:?}",
            report.errors
        );
    }

    #[test]
    fn golden_g14_cross_ref_domain_unregistered_rejected() {
        // G14 反例：引用边落点域未在域目录注册 → 拒收
        let mut doc = golden_newton();
        doc["laws"][0]["domains"] = json!(["physical"]);
        doc["laws"][0]["cross_refs"] =
            json!([{ "target": "grav_identity", "domain": "astrophysics" }]);
        doc["laws"]
            .as_array_mut()
            .expect("laws 应为数组")
            .push(golden_target_law());
        let report = validate_law_set(&doc);
        assert!(!report.valid, "G14 落点域未注册应拒收");
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("未在域目录注册") && e.contains("astrophysics")),
            "报因须指明未注册域: {:?}",
            report.errors
        );
    }

    #[test]
    fn golden_g15_intra_domain_edge_declared_cross_domain_rejected() {
        // G15 反例：落点域属于本律式自身 domains（域内引用）误声明为跨域引用边 → 拒收
        let mut doc = golden_newton();
        doc["laws"][0]["domains"] = json!(["physical"]);
        doc["laws"][0]["cross_refs"] = json!([{ "target": "grav_identity", "domain": "physical" }]);
        doc["laws"]
            .as_array_mut()
            .expect("laws 应为数组")
            .push(golden_target_law());
        let report = validate_law_set(&doc);
        assert!(!report.valid, "G15 域内引用误声明跨域应拒收");
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("域内引用不得声明为跨域引用边")),
            "报因须指明域内引用误声明: {:?}",
            report.errors
        );
    }

    #[test]
    fn golden_g16_law_domains_unregistered_rejected() {
        // G16 反例：law.domains 含未注册域 → 拒收（law 自身域归属注册在册性）
        let mut doc = golden_newton();
        doc["laws"][0]["domains"] = json!(["physical", "bogus_domain"]);
        let report = validate_law_set(&doc);
        assert!(!report.valid, "G16 law.domains 含未注册域应拒收");
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("domains 含未注册域") && e.contains("bogus_domain")),
            "报因须指明未注册域: {:?}",
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

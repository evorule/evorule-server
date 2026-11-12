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
//! - **有限性/单调形态层**：`max_version_depth`（版本链高度 h_p 有限性声明）
//!   必填且 ≥1；`monotone_form` 必填且值须为 true（非单调形态律式拒收）；
//!   formula 否定类构造（一元负号作用标识符/括号组、逻辑否定记号 `not`/`¬`）
//!   静态扫描命中且未声明 `time_anchor` → 拒收（时间锚定改写放行）。
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
    /// 函数自变量量纲一豁免（严口径例外声明；缺省 false = 严口径硬校验）。
    /// 仅经 law 面显式声明激活，引擎零硬编码行 id（缺省不变式口径）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fn_arg_dimless_exempt: Option<bool>,
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
    /// 版本链高度 h_p（有限性声明）：该律式版本链的最大深度，必填且 ≥1
    /// （serde 不加 default：缺字段即反序列化失败，schema 层 required 同向报因）
    pub max_version_depth: u32,
    /// 单调形态声明：值必须为 true（false 一律拒收，非单调形态律式
    /// 须以时间锚定改写重交）
    pub monotone_form: bool,
    /// 时间锚定改写声明锚（可选）：formula 命中否定类构造时必须非空
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_anchor: Option<String>,
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

/// 未验证条目（量纲第三态的结构化透出面）
///
/// 跳过 ≠ 通过：量纲能力层因 si_units 缺失/单位不可解析等原因未执行时，
/// 该条目不进入通过集，也不构成拒收，而以独立判定态「未验证」列明
/// （禁以 warning 静默埋没；见 law_set 装配呈现面同口径透出）。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct UnverifiedEntry {
    /// 条目路径（含 law id，形如 `laws[0] (id=newton_second): `）
    pub at: String,
    /// law id
    pub law_id: String,
    /// 未执行量纲校验的具体原因
    pub reason: String,
}

/// 校验报告：valid + 结构化错误列表（含实例路径），供上层显示明确提示
#[derive(Debug, Clone, Serialize)]
pub struct SchemaReport {
    /// 是否通过
    pub valid: bool,
    /// 校验模式：law_set
    pub mode: &'static str,
    /// 错误列表（每条含实例路径 + 具体原因）
    pub errors: Vec<String>,
    /// warning 级结果（不影响 valid，不构成拒收）
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// 未验证条目（量纲第三态：跳过 ≠ 通过 ≠ 拒收，独立判定态列明）
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unverified: Vec<UnverifiedEntry>,
}

impl SchemaReport {
    fn ok(mode: &'static str) -> Self {
        SchemaReport {
            valid: true,
            mode,
            errors: Vec::new(),
            warnings: Vec::new(),
            unverified: Vec::new(),
        }
    }

    fn fail(mode: &'static str, errors: Vec<String>) -> Self {
        SchemaReport {
            valid: false,
            mode,
            errors,
            warnings: Vec::new(),
            unverified: Vec::new(),
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
// 有限性声明 + 单调形态静态检查（formula 否定类构造扫描）
// ============================================================================

/// 否定类构造扫描命中（独立纯函数返回值，便于单测与报因回显）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegationHit {
    /// 命中形态：`unary_minus`（一元负号）或 `logical_not`（逻辑否定记号）
    pub kind: &'static str,
    /// 命中位置（formula 内字符偏移，0 起）
    pub pos: usize,
    /// 命中片段（含少量上下文，供报因回显）
    pub snippet: String,
}

/// 一元负号的前缀字符（减号前最近非空白字符必须是这些之一或行首）；
/// `a - b` 二元减法（前缀是标识符）与 `-3.14` 负数字面量（后随数字）不命中。
const UNARY_MINUS_PRECEDERS: [char; 9] = ['=', '(', '+', '*', '/', '^', '<', '>', ','];

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// 命中片段截取（前后各取少量上下文，越界端加省略号）
fn negation_snippet(chars: &[char], pos: usize, width: usize) -> String {
    let start = pos.saturating_sub(width);
    let end = (pos + width).min(chars.len());
    let mut s: String = chars[start..end].iter().collect();
    if start > 0 {
        s.insert(0, '…');
    }
    if end < chars.len() {
        s.push('…');
    }
    s
}

/// formula 否定类构造静态扫描（独立纯函数，便于单测）：
///
/// 两类命中形态——
/// ① 一元负号作用于标识符或括号组：减号前最近非空白字符是行首/运算符/分隔符
///    （`=(+*/^<>,`），且减号后（跳过空白）紧跟标识符字母或左括号；
///    `-3.14` 负数字面量与 `a - b` 二元减法不命中；
/// ② 逻辑否定记号：词边界 `not`、`¬`。
pub fn scan_negation_constructs(formula: &str) -> Vec<NegationHit> {
    let chars: Vec<char> = formula.chars().collect();
    let mut hits = Vec::new();
    for i in 0..chars.len() {
        if chars[i] == '-' {
            // 前瞻：减号后跳过空白须紧跟标识符字母或左括号
            let next = chars[i + 1..].iter().find(|c| !c.is_whitespace());
            let next_ok = next.is_some_and(|c| c.is_ascii_alphabetic() || *c == '(');
            if !next_ok {
                continue;
            }
            // 回溯：减号前最近非空白字符须为行首/运算符/分隔符
            let unary = chars[..i]
                .iter()
                .rev()
                .find(|c| !c.is_whitespace())
                .map(|c| UNARY_MINUS_PRECEDERS.contains(c))
                .unwrap_or(true);
            if unary {
                hits.push(NegationHit {
                    kind: "unary_minus",
                    pos: i,
                    snippet: negation_snippet(&chars, i, 8),
                });
            }
        } else if chars[i] == '¬' {
            hits.push(NegationHit {
                kind: "logical_not",
                pos: i,
                snippet: negation_snippet(&chars, i, 8),
            });
        } else if chars[i] == 'n'
            && chars[i..].len() >= 3
            && chars[i + 1] == 'o'
            && chars[i + 2] == 't'
        {
            let before_ok = i == 0 || !is_word_char(chars[i - 1]);
            let after_idx = i + 3;
            let after_ok = after_idx >= chars.len() || !is_word_char(chars[after_idx]);
            if before_ok && after_ok {
                hits.push(NegationHit {
                    kind: "logical_not",
                    pos: i,
                    snippet: negation_snippet(&chars, i, 8),
                });
            }
        }
    }
    hits
}

/// 单条定律的有限性校验：max_version_depth 须为整数且 ≥1。
/// 缺字段/非整数/负数形态由 schema 层 required + minimum 报因（serde 必填
/// 不加 default 同向拒收）；此处守反序列化成功条目的值域下界（0 拒收）。
fn check_law_finiteness(idx: usize, law: &Law, errors: &mut Vec<String>) {
    if law.max_version_depth == 0 {
        errors.push(format!(
            "laws[{idx}] (id={}): max_version_depth 须为整数且 ≥1（有限性声明：版本链高度下界为 1，当前 0）",
            law.id
        ));
    }
}

/// 单条定律的单调形态校验：
///
/// ① monotone_form 声明须为 true（false 一律拒收，中性技术语报因）；
/// ② formula 否定类构造静态扫描：命中且无 time_anchor → 拒收；
///    命中但 time_anchor 非空 → 放行（时间锚定改写）；未命中 → 放行。
fn check_monotone_form(idx: usize, law: &Law, errors: &mut Vec<String>) {
    let at = format!("laws[{idx}] (id={}): ", law.id);
    if !law.monotone_form {
        errors.push(format!(
            "{at}monotone_form=false：非单调形态律式拒收（须以时间锚定改写重交：改写 formula 消除否定类构造并声明 time_anchor 与 monotone_form=true）"
        ));
        return;
    }
    let hits = scan_negation_constructs(&law.formula);
    if hits.is_empty() {
        return;
    }
    let anchored = law
        .time_anchor
        .as_deref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if anchored {
        return;
    }
    for h in &hits {
        errors.push(format!(
            "{at}formula 含否定类构造（{kind}，位置 {pos}：`{snippet}`）；如属时间锚定改写请声明 time_anchor，否则拒收",
            kind = h.kind,
            pos = h.pos,
            snippet = h.snippet,
        ));
    }
}

// ============================================================================
// 量纲能力层接线
// ============================================================================

/// 单条定律的量纲能力层接线：不一致 → 硬错误；跳过 → 未验证第三态
/// （结构化列明，不拒收、不静默；跳过 ≠ 通过）
fn apply_dimensional_verdict(
    idx: usize,
    law: &Law,
    errors: &mut Vec<String>,
    unverified: &mut Vec<UnverifiedEntry>,
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
            unverified.push(UnverifiedEntry {
                at: at.trim_end().to_string(),
                law_id: law.id.clone(),
                reason,
            });
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
/// 跳过情况记入 `SchemaReport.unverified`（未验证第三态，独立判定态列明，
/// 不影响 valid、不静默）。
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
    let mut unverified: Vec<UnverifiedEntry> = Vec::new();
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
            check_law_finiteness(*idx, law, &mut errors);
            check_monotone_form(*idx, law, &mut errors);
            apply_dimensional_verdict(*idx, law, &mut errors, &mut unverified);
        }
    }
    let mut report = if errors.is_empty() {
        SchemaReport::ok("law_set")
    } else {
        SchemaReport::fail("law_set", errors)
    };
    report.unverified = unverified;
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
                "max_version_depth": 1,
                "monotone_form": true,
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
                "tier": "full",
                "max_version_depth": 1,
                "monotone_form": true
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
                },
                "max_version_depth": 1,
                "monotone_form": true
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
    fn golden_dim_g6_missing_si_units_symbol_unverified_but_not_rejected() {
        // G6：formula 引用符号缺 si_units → 跳过量纲校验记未验证第三态，不硬拒
        // （G 类纯数学定律兼容；valid 保持 true；跳过 ≠ 通过，独立判定态列明）
        let mut doc = golden_newton();
        doc["laws"][0]["formula"] = json!("F = m * x");
        doc["laws"][0]["symbols"] = json!({ "F": "合外力", "m": "质量", "x": "任意标量" });
        let report = validate_law_set(&doc);
        assert!(
            report.valid,
            "缺 si_units 符号应跳过不拒收: {:?}",
            report.errors
        );
        assert!(report.warnings.is_empty(), "跳过不得静默落 warning 面");
        assert_eq!(report.unverified.len(), 1, "应恰一条未验证条目");
        let entry = &report.unverified[0];
        assert_eq!(entry.law_id, "newton_second");
        assert!(
            entry.reason.contains('x') && entry.reason.contains("缺 si_units"),
            "未验证原因须指明缺 si_units 的符号: {:?}",
            entry.reason
        );
    }

    #[test]
    fn golden_dim_g7_pure_math_law_without_si_units_unverified_not_rejected() {
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
        assert_eq!(report.unverified.len(), 1);
        let entry = &report.unverified[0];
        assert!(
            entry.reason.contains("y") && entry.reason.contains("a") && entry.reason.contains("x"),
            "未验证原因须列出全部缺 si_units 符号: {:?}",
            entry.reason
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
        assert!(
            report.warnings.is_empty() && report.unverified.is_empty(),
            "不应有跳过或 warning"
        );

        let mut doc = golden_einstein();
        doc["laws"][0]["formula"] = json!("E = 0.5 * m * v^2");
        doc["laws"][0]["symbols"] = json!({ "E": "动能", "m": "质量", "v": "速度" });
        doc["laws"][0]["si_units"] = json!({ "E": "joule", "m": "kilogram", "v": "m*s^-1" });
        let report = validate_law_set(&doc);
        assert!(report.valid, "数值常量应量纲中性: {:?}", report.errors);
    }

    #[test]
    fn golden_dim_g9_unparseable_unit_name_unverified_but_not_rejected() {
        // G9：si_units 单位名不可解析（拼写错误）→ 跳过记未验证第三态，不硬拒
        let mut doc = golden_newton();
        doc["laws"][0]["si_units"]["F"] = json!("newtn");
        let report = validate_law_set(&doc);
        assert!(report.valid, "未知单位应跳过不拒收: {:?}", report.errors);
        assert!(report.warnings.is_empty(), "跳过不得静默落 warning 面");
        assert_eq!(report.unverified.len(), 1);
        let entry = &report.unverified[0];
        assert!(
            entry.reason.contains("未知单位") && entry.reason.contains("newtn"),
            "未验证原因须指明不可解析单位: {:?}",
            entry.reason
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
            "domains": ["physical"],
            "max_version_depth": 1,
            "monotone_form": true
        })
    }

    #[test]
    fn golden_g12_multidomain_with_legal_cross_refs_passes() {
        // G12 正例：多域归属 + 合法跨域引用边（落点 = 目标声明域，且不属于
        // 本律式自身 domains）。目标律式以旧单域 domain 字段声明归属
        // （③ 的旧字段归并路径同验）；目标纯数学形态，量纲层跳过记未验证第三态。
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

    // ── 有限性声明 + 单调形态 golden（G17-G21：必填/值域 + 否定类构造静态扫描）──

    #[test]
    fn golden_g17_missing_max_version_depth_rejected_with_field_path() {
        // G17 反例：缺 max_version_depth → 拒收，报因含字段定位
        let mut doc = golden_newton();
        doc["laws"][0]
            .as_object_mut()
            .expect("law 应为 object")
            .remove("max_version_depth");
        let report = validate_law_set(&doc);
        assert!(!report.valid, "G17 缺 max_version_depth 应拒收");
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("laws") && e.contains("max_version_depth")),
            "报因须含 laws 条目定位与字段名: {:?}",
            report.errors
        );
    }

    #[test]
    fn golden_g18_monotone_form_missing_or_false_rejected() {
        // G18 反例两支：monotone_form 缺失 → schema 层拒收；
        // monotone_form=false → 结构层拒收（中性技术语报因）
        let mut missing = golden_newton();
        missing["laws"][0]
            .as_object_mut()
            .expect("law 应为 object")
            .remove("monotone_form");
        let report = validate_law_set(&missing);
        assert!(!report.valid, "G18 缺 monotone_form 应拒收");
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("laws") && e.contains("monotone_form")),
            "报因须含条目定位与字段名: {:?}",
            report.errors
        );

        let mut false_form = golden_newton();
        false_form["laws"][0]["monotone_form"] = json!(false);
        let report = validate_law_set(&false_form);
        assert!(!report.valid, "G18 monotone_form=false 应拒收");
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("非单调形态律式拒收") && e.contains("time_anchor")),
            "报因须为中性技术语并指明时间锚定改写通路: {:?}",
            report.errors
        );
    }

    #[test]
    fn golden_g19_unary_minus_without_time_anchor_rejected() {
        // G19 反例：formula 含一元负号作用标识符且无 time_anchor → 拒收
        // （报因含否定类构造措辞与命中片段回显）
        let mut doc = golden_newton();
        doc["laws"][0]["formula"] = json!("v = -a * t");
        doc["laws"][0]["symbols"] = json!({ "v": "速度", "a": "加速度", "t": "时间" });
        doc["laws"][0]["si_units"] = json!({ "v": "m*s^-1", "a": "m*s^-2", "t": "s" });
        let report = validate_law_set(&doc);
        assert!(!report.valid, "G19 一元负号无 time_anchor 应拒收");
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("否定类构造") && e.contains("time_anchor") && e.contains("-a")),
            "报因须含否定类构造措辞、时间锚定指引与命中片段: {:?}",
            report.errors
        );
    }

    #[test]
    fn golden_g20_unary_minus_with_time_anchor_passes() {
        // G20 正例：同 formula + time_anchor 非空 → 时间锚定改写通过
        let mut doc = golden_newton();
        doc["laws"][0]["formula"] = json!("v = -a * t");
        doc["laws"][0]["symbols"] = json!({ "v": "速度", "a": "加速度", "t": "时间" });
        doc["laws"][0]["si_units"] = json!({ "v": "m*s^-1", "a": "m*s^-2", "t": "s" });
        doc["laws"][0]["time_anchor"] = json!("anchored at t0 = 0 (time-anchored rewrite)");
        let report = validate_law_set(&doc);
        assert!(report.valid, "G20 时间锚定改写应通过: {:?}", report.errors);

        // time_anchor 空白串不等价于声明在场（minLength 1 语义同向）：
        // 仅含空白 → 仍按未声明拒收
        let mut blank = doc.clone();
        blank["laws"][0]["time_anchor"] = json!("   ");
        let report = validate_law_set(&blank);
        assert!(!report.valid, "time_anchor 仅空白应视为未声明而拒收");
    }

    #[test]
    fn golden_g21_monotone_law_without_negation_passes() {
        // G21 正例：合法单调律式（无否定类构造）通过，且 serde 三新字段
        // 强类型往返一致（bool true / Option None 序列化省略）
        let doc = golden_ideal_gas();
        let lawset: LawSet =
            serde_json::from_value(doc.clone()).expect("G21 应可反序列化为 LawSet");
        assert_eq!(lawset.laws[0].max_version_depth, 1);
        assert!(lawset.laws[0].monotone_form);
        assert_eq!(lawset.laws[0].time_anchor, None);
        let report = validate_law_set(&doc);
        assert!(report.valid, "G21 合法单调律式应通过: {:?}", report.errors);
    }

    // ── 否定类构造扫描器单元面（独立纯函数边界形态）──

    #[test]
    fn negation_scan_unary_minus_forms() {
        // 命中：行首/运算符/分隔符前缀 + 标识符或括号后随
        for (formula, pos) in [
            ("-x = y", 0usize),
            ("a = -b * c", 4),
            ("F = m * (-g)", 9),
            ("a * -b = c", 4),
            ("a = - b", 4), // 减号后空白再标识符仍命中（`\s*` 形态）
        ] {
            let hits = scan_negation_constructs(formula);
            assert_eq!(hits.len(), 1, "formula `{formula}` 应恰一命中");
            assert_eq!(hits[0].kind, "unary_minus");
            assert_eq!(hits[0].pos, pos, "formula `{formula}` 命中位置");
            assert!(
                hits[0].snippet.contains('-'),
                "片段须回显减号: {:?}",
                hits[0]
            );
        }
    }

    #[test]
    fn negation_scan_non_hits() {
        // 不命中：负数字面量、二元减法、指数负幂、标识符内 not
        for formula in [
            "y = -3.14 * x",   // 负数字面量（后随数字）
            "a - b = c",       // 二元减法（前缀是标识符）
            "y = a ^ -2",      // 指数负幂（后随数字）
            "F = (a) - b",     // 括号组后二元减法（前缀是右括号）
            "s = t * (1/2)",   // 无减号
            "x = a and_not b", // `not` 前有词字符（非词边界）
            "y = a + x",       // 干净等式
        ] {
            let hits = scan_negation_constructs(formula);
            assert!(hits.is_empty(), "formula `{formula}` 不应命中: {:?}", hits);
        }
    }

    #[test]
    fn negation_scan_logical_not_forms() {
        // 词边界 not 与逻辑否定记号 ¬ 均命中
        let hits = scan_negation_constructs("y = not a");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, "logical_not");
        assert_eq!(hits[0].pos, 4);

        let hits = scan_negation_constructs("y = ¬a");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, "logical_not");

        // 语句内多命中逐个报（一元负号 + 词边界 not）
        let hits = scan_negation_constructs("y = -a and not b");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].kind, "unary_minus");
        assert_eq!(hits[1].kind, "logical_not");
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

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 验证注入面纯函数管线 —— 命题验证的四步判定（量纲对表/定律代入/常量对照/三态聚合）
//!
//! # 定位（与 core/law_schema 同构）
//! - law_schema 守 law_set 部署件的等式恒真语义；本 crate 承载验证注入：
//!   接收测量命题（主张值+SI 单位+不确定度），对已装配的定律与常量做
//!   确定性验证，产出三态判定与逐步证据。
//! - **纯函数管线**：不触文件/网络/进程，输入全部经参数注入（LawRef /
//!   ConstantRef 均为借用视图，由 server 侧装配面提供）——可回放、可审计、
//!   单请求内零状态（铁律 3：判定只读，只提案不改写）。
//! - **内嵌 schema 锚**（D2）：verify_request / constant_set 两件 schema
//!   内嵌并经 build.rs 构建期门禁；$id 归 criticality 命名空间。
//!
//! # 四步管线（10 号设计稿 §2）
//! 1. **量纲对表**：命题主张的 `(value, si_unit)` 与涉及定律符号/常量的
//!    si_units 跑同一量纲代数（复用 law_schema dimensional）——不一致即
//!    拒判（报两侧量纲向量，fail-fast，不猜语义）。
//! 2. **定律代入**：定位涉及定律（claims 符号 ∩ law.symbols），目标量
//!    单符号裸形态时把已知值（主张优先、常量兜底）代入另一侧求计算值，
//!    与主张比较（容差 = 声明不确定度 × k·σ 因子）。MVP 不做符号重排
//!    （目标侧非裸形态跳过并报因）。
//! 3. **常量对照**：主张符号命中唯一常量时做 k·σ 区间比较
//!    （σ = max(常量声明不确定度, 主张不确定度)；越界 = alert）。
//! 4. **三态聚合**（D4 词汇立法）：`stable`（全部相容）/ `alert`
//!    （常量越界但未矛盾）/ `contradiction`（定律链内互斥）。alert 态
//!    响应附结构化提案物料包（人工经治理链入闸，无自动回写通道，D5）。
//!
//! # DoS 面（对外端点，fail-fast 硬上限）
//! 表达式长度/括号深度/标识符数/数值幅值/主张条数/定律扫描集全部有上限
//! （[`eval`] 模块与本文常量），超限一律显式拒绝，不静默降级。

#![forbid(unsafe_code)]
// C5 (unwrap/expect/panic = deny) 仅约束生产代码;测试代码保留 unwrap 惯例
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod eval;

use std::collections::BTreeMap;

use evorule_law_schema::dimensional::{parse_unit_expr, DimensionVector};
use evorule_law_schema::Law;
use jsonschema::{Draft, Retrieve, UriRef, Validator};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ============================================================================
// 内嵌 Schema（SSOT：criticality 命名空间）
// ============================================================================

const VERIFY_REQUEST_JSON: &str = include_str!("../schemas/verify_request/v1.0.json");
const CONSTANT_SET_JSON: &str = include_str!("../schemas/constant_set/v1.0.json");

pub const VERIFY_REQUEST_ID: &str =
    "https://evorule.org/schemas/criticality/verify_request/v1.0.json";
pub const CONSTANT_SET_ID: &str = "https://evorule.org/schemas/criticality/constant_set/v1.0.json";

// ============================================================================
// 硬上限（DoS 面 fail-fast；schema 层之外的语义边界）
// ============================================================================

/// 主张数值幅值上限（|v| 超出拒收；覆盖天文到量子尺度，拒 inf/NaN）
pub const MAX_VALUE_MAGNITUDE: f64 = 1e100;

/// 单请求定律扫描集上限（涉及定律候选超限即拒收，防全量遍历失控）
pub const MAX_LAWS_PER_REQUEST: usize = 64;

/// k·σ 因子上限（schema maximum 100 的语义层镜像；缺省 2）
pub const DEFAULT_K_SIGMA: f64 = 2.0;
pub const MAX_K_SIGMA: f64 = 100.0;

// ============================================================================
// serde 类型（verify_request v1.0 文档模型）
// ============================================================================

/// 单条测量主张
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaimEntry {
    pub symbol: String,
    pub value: f64,
    pub si_unit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uncertainty: Option<f64>,
}

/// 测量上下文（溯源必填：铁律 2 无出处拒收）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerifyContext {
    pub source: String,
    pub measured_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub k_sigma: Option<f64>,
}

/// 验证命题（请求体强类型镜像）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerifyRequest {
    #[serde(rename = "$schema")]
    pub schema: String,
    pub subject: String,
    pub claims: Vec<ClaimEntry>,
    pub context: VerifyContext,
}

// ============================================================================
// serde 类型（constant_set v1.0 部署件文档模型；载荷字段与 rule 仓
// law_set-constant-v1.0 契约镜像：value + si_unit + metadata{source 必})
// ============================================================================

/// 常量溯源元数据（source 必填——铁律 2：无出处常量拒收）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConstantMetadata {
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uncertainty: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified: Option<String>,
}

/// 单条验证常量
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConstantEntry {
    pub id: String,
    pub value: f64,
    pub si_unit: String,
    pub metadata: ConstantMetadata,
}

/// constant_set 部署件文档顶层
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConstantSet {
    #[serde(rename = "$schema")]
    pub schema: String,
    pub kind: String,
    pub id: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    pub constants: Vec<ConstantEntry>,
}

// ============================================================================
// 校验结果（与 law_schema SchemaReport 同构）
// ============================================================================

/// 校验报告：valid + 结构化错误列表
#[derive(Debug, Clone, Serialize)]
pub struct SchemaReport {
    pub valid: bool,
    /// 校验模式：verify_request / constant_set
    pub mode: &'static str,
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
// 内嵌 schema 解析器（$id 自洽；build.rs 已保证两件合法）
// ============================================================================

struct EmbeddedRetriever;

impl Retrieve for EmbeddedRetriever {
    fn retrieve(
        &self,
        uri: &UriRef<&str>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let id = uri.as_str();
        let raw = match id {
            VERIFY_REQUEST_ID => VERIFY_REQUEST_JSON,
            CONSTANT_SET_ID => CONSTANT_SET_JSON,
            other => {
                return Err(format!("未知 evorule schema $id: {other}").into());
            }
        };
        serde_json::from_str(raw).map_err(|e| e.into())
    }
}

struct Validators {
    verify_request: Validator,
    constant_set: Validator,
}

#[allow(clippy::expect_used)]
fn compile_validator(doc: &Value) -> Validator {
    let mut opts = Validator::options();
    opts.with_draft(Draft::Draft202012);
    opts.with_retriever(EmbeddedRetriever);
    opts.build(doc)
        .expect("构建 criticality 校验器失败（build.rs 已保证内嵌 schema 合法）")
}

fn build_validators() -> Validators {
    #[allow(clippy::expect_used)]
    let parse_embedded = |raw: &'static str| -> Value {
        serde_json::from_str(raw).expect("内嵌 criticality schema 非法（build.rs 已保证）")
    };
    let verify_request = parse_embedded(VERIFY_REQUEST_JSON);
    let constant_set = parse_embedded(CONSTANT_SET_JSON);

    Validators {
        verify_request: compile_validator(&verify_request),
        constant_set: compile_validator(&constant_set),
    }
}

static VALIDATORS: Lazy<Validators> = Lazy::new(build_validators);

fn schema_errors(validator: &Validator, doc: &Value) -> Vec<String> {
    match validator.validate(doc) {
        Ok(()) => Vec::new(),
        Err(iter) => iter
            .map(|e| format!("{}: {}", e.instance_path, e))
            .collect(),
    }
}

// ============================================================================
// 数值边界（schema 表达不了的有限性/幅值语义）
// ============================================================================

fn check_finite_bounded(name: &str, v: f64, errors: &mut Vec<String>) {
    if !v.is_finite() {
        errors.push(format!("{name} 非有限数值（inf/NaN 拒收）"));
    } else if v.abs() > MAX_VALUE_MAGNITUDE {
        errors.push(format!("{name} 幅值越界（|v| ≤ {MAX_VALUE_MAGNITUDE:e}）"));
    }
}

fn check_k_sigma(k: f64, errors: &mut Vec<String>) {
    if !k.is_finite() || k <= 0.0 || k > MAX_K_SIGMA {
        errors.push(format!("k_sigma 越界（0 < k ≤ {MAX_K_SIGMA}）"));
    }
}

/// 解析常量声明的不确定度字符串（契约形态 = 数字字面量字符串）。
/// 调用方保证载荷已过 validate_constant_set（unparseable 已拒收）；
/// 此处失败返回 None（跳过对照并报因，不 panic）。
pub fn parse_uncertainty(text: &str) -> Option<f64> {
    let v: f64 = text.trim().parse().ok()?;
    if v.is_finite() && (0.0..=MAX_VALUE_MAGNITUDE).contains(&v) {
        Some(v)
    } else {
        None
    }
}

// ============================================================================
// 公共校验 API
// ============================================================================

/// 校验验证命题请求（schema 层 + 数值有限性/幅值 + 单位可解析 fail-fast）。
///
/// 单位不可解析直接拒收（命题是对外 API 输入，fail-fast 口径；与 law 臂
/// 部署件「跳过记警」不同——部署件是治理侧产物，命题是验证请求本体）。
pub fn validate_verify_request(doc: &Value) -> SchemaReport {
    let mut errors = schema_errors(&VALIDATORS.verify_request, doc);
    if let Ok(req) = serde_json::from_value::<VerifyRequest>(doc.clone()) {
        for claim in &req.claims {
            check_finite_bounded(
                &format!("主张 `{}` 的 value", claim.symbol),
                claim.value,
                &mut errors,
            );
            if let Some(u) = claim.uncertainty {
                check_finite_bounded(
                    &format!("主张 `{}` 的 uncertainty", claim.symbol),
                    u,
                    &mut errors,
                );
            }
            if let Err(e) = parse_unit_expr(&claim.si_unit) {
                errors.push(format!(
                    "主张 `{}` 的 si_unit `{}` 不可解析: {e}",
                    claim.symbol, claim.si_unit
                ));
            }
        }
        if let Some(k) = req.context.k_sigma {
            check_k_sigma(k, &mut errors);
        }
    }
    if errors.is_empty() {
        SchemaReport::ok("verify_request")
    } else {
        SchemaReport::fail("verify_request", errors)
    }
}

/// 校验常量集部署件（schema 层 + 数值有限性/幅值 + 单位与不确定度可解析）。
///
/// 部署件口径 fail-fast：无出处/单位不可解析/不确定度非数字字面量均拒收
/// （铁律 2 + 确定性装载）。
pub fn validate_constant_set(doc: &Value) -> SchemaReport {
    let mut errors = schema_errors(&VALIDATORS.constant_set, doc);
    if let Ok(set) = serde_json::from_value::<ConstantSet>(doc.clone()) {
        for c in &set.constants {
            check_finite_bounded(&format!("常量 `{}` 的 value", c.id), c.value, &mut errors);
            if let Err(e) = parse_unit_expr(&c.si_unit) {
                errors.push(format!(
                    "常量 `{}` 的 si_unit `{}` 不可解析: {e}",
                    c.id, c.si_unit
                ));
            }
            if let Some(u) = &c.metadata.uncertainty {
                if parse_uncertainty(u).is_none() {
                    errors.push(format!(
                        "常量 `{}` 的 uncertainty `{u}` 须为非负有限数字字面量",
                        c.id
                    ));
                }
            }
        }
    }
    if errors.is_empty() {
        SchemaReport::ok("constant_set")
    } else {
        SchemaReport::fail("constant_set", errors)
    }
}

// ============================================================================
// 管线输入视图（纯函数：定律/常量以借用视图注入，不依赖装配面类型）
// ============================================================================

/// 定律借用视图（dataset_id = 命名空间键，与 law_host 双键口径一致）
#[derive(Debug, Clone, Copy)]
pub struct LawRef<'a> {
    pub dataset_id: &'a str,
    pub law: &'a Law,
}

/// 常量借用视图
#[derive(Debug, Clone, Copy)]
pub struct ConstantRef<'a> {
    pub dataset_id: &'a str,
    pub constant: &'a ConstantEntry,
}

// ============================================================================
// 判定输出（D4 词汇立法：stable / alert / contradiction）
// ============================================================================

/// 三态判定词汇（criticality 域立法；与 04 号档 §3 一致）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// 全部相容
    Stable,
    /// 常量越界但未矛盾（附提案物料包）
    Alert,
    /// 定律链内互斥
    Contradiction,
}

/// 逐步结论
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepOutcome {
    /// 相容
    Consistent,
    /// 常量区间越界（alert 证据）
    OutOfBounds,
    /// 定律等式违背（contradiction 证据）
    Violated,
    /// 跳过（缺值/形态不符/无不确定度——记因不判定，不构成越界或矛盾）
    Skipped,
}

/// 单步证据
#[derive(Debug, Clone, Serialize)]
pub struct VerifyStep {
    /// 步序（1 起，生成序确定）
    pub step: usize,
    /// 步骤族：dimensional / law_substitution / constant_comparison
    pub kind: &'static str,
    /// 判定对象（主张符号 / 定律 id / 常量 id）
    pub target: String,
    pub outcome: StepOutcome,
    pub detail: String,
}

/// 提案物料包（alert/contradiction 态附出；人工经治理链入闸，无自动回写）
#[derive(Debug, Clone, Serialize)]
pub struct ProposalPackage {
    /// 提案族：constant_update / law_review
    pub kind: &'static str,
    pub dataset_id: String,
    pub entry_id: String,
    pub subject: String,
    /// 现值视图（current）与实测视图（measured）结构化并列
    pub payload: Value,
    pub reason: String,
}

/// 验证报告（响应体）
#[derive(Debug, Clone, Serialize)]
pub struct VerifyReport {
    pub verdict: Verdict,
    pub steps: Vec<VerifyStep>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub proposals: Vec<ProposalPackage>,
}

// ============================================================================
// 拒判错误（422 类：结构非法/量纲不一致/扫描越限——报因结构化）
// ============================================================================

/// 拒判错误（verify 返回 Err 的全部形态；HTTP 侧映射 422）
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VerifyError {
    /// schema/结构层拒收（含逐条报因）
    Schema { errors: Vec<String> },
    /// 量纲对表不一致——拒判并报两侧量纲向量（10 号稿验收 3）
    DimensionalMismatch {
        symbol: String,
        claim_dims: String,
        other_dims: String,
        against: String,
    },
    /// 涉及定律候选超扫描上限
    LawScanLimit { count: usize, limit: usize },
}

impl VerifyError {
    pub fn summary(&self) -> String {
        match self {
            VerifyError::Schema { errors } => {
                format!("命题结构拒收（{} 项）", errors.len())
            }
            VerifyError::DimensionalMismatch {
                symbol,
                claim_dims,
                other_dims,
                against,
            } => format!(
                "量纲不一致拒判: 主张 `{symbol}` 与 {against} 对表 `{claim_dims}` vs `{other_dims}`"
            ),
            VerifyError::LawScanLimit { count, limit } => {
                format!("涉及定律候选 {count} 条超扫描上限 {limit}")
            }
        }
    }
}

// ============================================================================
// 四步管线
// ============================================================================

/// 主张内部视图（值 + 量纲向量 + 不确定度）
struct ClaimView {
    value: f64,
    uncertainty: Option<f64>,
    dims: DimensionVector,
}

/// 常量符号全局匹配结论（唯一命中 / 跨库歧义 / 无）
enum ConstantMatch<'a> {
    Unique(&'a str, &'a ConstantEntry),
    Ambiguous(Vec<String>),
    NotFound,
}

/// 执行验证管线（纯函数；`laws`/`constants` 由调用方按确定性序注入）。
///
/// 步序：量纲对表 → 定律代入 → 常量对照 → 聚合。任何量纲不一致即拒判
/// （Err）；定律违背 → contradiction；常量越界 → alert；其余 → stable。
pub fn verify(
    req: &VerifyRequest,
    laws: &[LawRef<'_>],
    constants: &[ConstantRef<'_>],
) -> Result<VerifyReport, VerifyError> {
    structural_check(req)?;
    let k = req.context.k_sigma.unwrap_or(DEFAULT_K_SIGMA);
    let claims = build_claim_table(req)?;
    let const_index = build_const_index(constants);

    let mut steps: Vec<VerifyStep> = Vec::new();
    let mut proposals: Vec<ProposalPackage> = Vec::new();

    // ── 步① 量纲对表（不一致即拒判；一致记步）──
    dimensional_gate(req, &claims, laws, &const_index, &mut steps)?;

    // ── 步② 定律代入（目标量单符号裸形态；主张优先、常量兜底）──
    let any_violated = law_substitution(
        req,
        k,
        &claims,
        laws,
        &const_index,
        &mut steps,
        &mut proposals,
    )?;

    // ── 步③ 常量对照（唯一命中才对照；σ_eff = max(常量σ, 主张σ)）──
    let any_out_of_bounds =
        constant_comparison(req, k, &claims, &const_index, &mut steps, &mut proposals);

    // ── 步④ 三态聚合（矛盾 > 告警 > 稳定；跳过不构成越界或矛盾）──
    for (i, s) in steps.iter_mut().enumerate() {
        s.step = i + 1;
    }
    let verdict = if any_violated {
        Verdict::Contradiction
    } else if any_out_of_bounds {
        Verdict::Alert
    } else {
        Verdict::Stable
    };
    Ok(VerifyReport {
        verdict,
        steps,
        proposals,
    })
}

/// 结构层防御性重校（调用方可能绕过 validate 直调；fail-fast 不猜）
fn structural_check(req: &VerifyRequest) -> Result<(), VerifyError> {
    let mut struct_errors = Vec::new();
    for claim in &req.claims {
        check_finite_bounded(
            &format!("主张 `{}` 的 value", claim.symbol),
            claim.value,
            &mut struct_errors,
        );
        if let Some(u) = claim.uncertainty {
            check_finite_bounded(
                &format!("主张 `{}` 的 uncertainty", claim.symbol),
                u,
                &mut struct_errors,
            );
        }
        if let Err(e) = parse_unit_expr(&claim.si_unit) {
            struct_errors.push(format!(
                "主张 `{}` 的 si_unit `{}` 不可解析: {e}",
                claim.symbol, claim.si_unit
            ));
        }
    }
    if let Some(k) = req.context.k_sigma {
        check_k_sigma(k, &mut struct_errors);
    }
    if struct_errors.is_empty() {
        Ok(())
    } else {
        Err(VerifyError::Schema {
            errors: struct_errors,
        })
    }
}

/// 主张表（BTreeMap：同符号重复主张以末条为准，键序确定）
fn build_claim_table(req: &VerifyRequest) -> Result<BTreeMap<&str, ClaimView>, VerifyError> {
    let mut claims: BTreeMap<&str, ClaimView> = BTreeMap::new();
    for claim in &req.claims {
        let dims = parse_unit_expr(&claim.si_unit).map_err(|e| VerifyError::Schema {
            errors: vec![format!(
                "主张 `{}` 的 si_unit `{}` 不可解析: {e}",
                claim.symbol, claim.si_unit
            )],
        })?;
        claims.insert(
            claim.symbol.as_str(),
            ClaimView {
                value: claim.value,
                uncertainty: claim.uncertainty,
                dims,
            },
        );
    }
    Ok(claims)
}

/// 常量符号索引（id → 出现集；唯一命中才可对照，歧义报因）
fn build_const_index<'a>(
    constants: &'a [ConstantRef<'a>],
) -> BTreeMap<&'a str, Vec<&'a ConstantRef<'a>>> {
    let mut const_index: BTreeMap<&str, Vec<&ConstantRef<'_>>> = BTreeMap::new();
    for c in constants {
        const_index
            .entry(c.constant.id.as_str())
            .or_default()
            .push(c);
    }
    const_index
}

/// 常量符号全局解析（唯一命中 / 跨库歧义 / 无）
fn resolve_constant<'a>(
    const_index: &'a BTreeMap<&'a str, Vec<&'a ConstantRef<'a>>>,
    symbol: &str,
) -> ConstantMatch<'a> {
    match const_index.get(symbol) {
        None => ConstantMatch::NotFound,
        Some(v) if v.len() == 1 => ConstantMatch::Unique(v[0].dataset_id, v[0].constant),
        Some(many) => {
            ConstantMatch::Ambiguous(many.iter().map(|c| c.dataset_id.to_string()).collect())
        }
    }
}

/// 步① 量纲对表：主张单位与涉及定律符号/常量的单位对表，
/// 不一致即拒判（报两侧量纲向量）；一致记步，无可对表者跳过记因。
fn dimensional_gate(
    req: &VerifyRequest,
    claims: &BTreeMap<&str, ClaimView>,
    laws: &[LawRef<'_>],
    const_index: &BTreeMap<&str, Vec<&ConstantRef<'_>>>,
    steps: &mut Vec<VerifyStep>,
) -> Result<(), VerifyError> {
    for (symbol, view) in claims {
        let mut checked = 0usize;
        let mut mismatch: Option<VerifyError> = None;
        for lr in laws {
            let Some(unit_expr) = lr.law.si_units.get(*symbol) else {
                continue;
            };
            let Ok(law_dims) = parse_unit_expr(unit_expr) else {
                continue; // 定律侧单位不可解析 = law 臂既有跳过语义，不拒判
            };
            checked += 1;
            if law_dims != view.dims {
                mismatch = Some(VerifyError::DimensionalMismatch {
                    symbol: symbol.to_string(),
                    claim_dims: view.dims.render(),
                    other_dims: law_dims.render(),
                    against: format!("定律 {}（库 {}）", lr.law.id, lr.dataset_id),
                });
                break;
            }
        }
        if mismatch.is_none() {
            if let ConstantMatch::Unique(ds, c) = resolve_constant(const_index, symbol) {
                if let Ok(c_dims) = parse_unit_expr(&c.si_unit) {
                    checked += 1;
                    if c_dims != view.dims {
                        mismatch = Some(VerifyError::DimensionalMismatch {
                            symbol: symbol.to_string(),
                            claim_dims: view.dims.render(),
                            other_dims: c_dims.render(),
                            against: format!("常量 {}（库 {}）", c.id, ds),
                        });
                    }
                }
            }
        }
        match mismatch {
            Some(err) => return Err(err),
            None => steps.push(VerifyStep {
                step: steps.len() + 1,
                kind: "dimensional",
                target: symbol.to_string(),
                outcome: if checked > 0 {
                    StepOutcome::Consistent
                } else {
                    StepOutcome::Skipped
                },
                detail: if checked > 0 {
                    format!(
                        "主张量纲 `{}` 与 {checked} 处对表一致（单位 `{}`）",
                        view.dims.render(),
                        req.claims
                            .iter()
                            .find(|c| c.symbol == **symbol)
                            .map(|c| c.si_unit.as_str())
                            .unwrap_or("-")
                    )
                } else {
                    "无可对表的定律/常量单位声明（跳过）".to_string()
                },
            }),
        }
    }
    Ok(())
}

/// 步② 定律代入：涉及定律按 (dataset_id, law_id) 确定序扫描（上限内），
/// 目标量单符号裸形态时代入求值与主张容差比较。
/// 返回是否出现定律违背（contradiction 证据）。
fn law_substitution(
    req: &VerifyRequest,
    k: f64,
    claims: &BTreeMap<&str, ClaimView>,
    laws: &[LawRef<'_>],
    const_index: &BTreeMap<&str, Vec<&ConstantRef<'_>>>,
    steps: &mut Vec<VerifyStep>,
    proposals: &mut Vec<ProposalPackage>,
) -> Result<bool, VerifyError> {
    // 候选 = symbols 与主张相交的定律；序 = (dataset_id, law_id) 确定
    let mut candidates: Vec<&LawRef<'_>> = laws
        .iter()
        .filter(|lr| {
            lr.law
                .symbols
                .keys()
                .any(|s| claims.contains_key(s.as_str()))
        })
        .collect();
    candidates.sort_by(|a, b| (&a.dataset_id, &a.law.id).cmp(&(&b.dataset_id, &b.law.id)));
    if candidates.len() > MAX_LAWS_PER_REQUEST {
        return Err(VerifyError::LawScanLimit {
            count: candidates.len(),
            limit: MAX_LAWS_PER_REQUEST,
        });
    }

    let mut any_violated = false;
    for lr in &candidates {
        let law = lr.law;
        let Some((lhs_raw, rhs_raw)) = law.formula.split_once('=') else {
            steps.push(skip_step(
                "law_substitution",
                &law.id,
                "formula 无 `=` 连接符",
            ));
            continue;
        };
        // 目标量 = 定律符号与主张相交集中键序最小者（确定性）
        let Some(target) = law.symbols.keys().find(|s| claims.contains_key(s.as_str())) else {
            continue;
        };
        let target: &str = target.as_str();
        let view = &claims[target];

        // 裸形态判定（MVP 无符号重排）：目标量独占一侧
        let (expr, flipped) = if eval::is_bare_identifier(lhs_raw) && lhs_raw.trim() == target {
            (rhs_raw, false)
        } else if eval::is_bare_identifier(rhs_raw) && rhs_raw.trim() == target {
            (lhs_raw, true)
        } else {
            steps.push(skip_step(
                "law_substitution",
                &law.id,
                &format!("目标量 `{target}` 非单符号裸形态（MVP 不做符号重排，跳过）"),
            ));
            continue;
        };

        // 表达式标识符预检 + 代入表（主张优先，常量唯一命中兜底）
        let Ok(idents) = eval::expression_identifiers(expr) else {
            steps.push(skip_step(
                "law_substitution",
                &law.id,
                "表达式标识符数超上限",
            ));
            continue;
        };
        if idents.iter().any(|s| s == target) {
            steps.push(skip_step(
                "law_substitution",
                &law.id,
                &format!("目标量 `{target}` 出现于表达式侧（需自洽核验形态，MVP 跳过）"),
            ));
            continue;
        }
        let Some(sub) = build_substitution(&idents, target, claims, const_index) else {
            steps.push(skip_step(
                "law_substitution",
                &law.id,
                "表达式符号缺已知值（无主张值且无常量命中，或跨库同 id 歧义）",
            ));
            continue;
        };

        let mut resolve = |name: &str| -> Result<f64, String> {
            sub.get(name)
                .copied()
                .ok_or_else(|| format!("符号 `{name}` 无已知值"))
        };
        let computed = match eval::eval_numeric(expr, &mut resolve) {
            Ok(v) => v,
            Err(e) => {
                steps.push(skip_step(
                    "law_substitution",
                    &law.id,
                    &format!("代入求值失败: {e}"),
                ));
                continue;
            }
        };

        // 容差 = 声明不确定度 × k（无不确定度 → 跳过，不判定）
        let Some(sigma) = view.uncertainty else {
            steps.push(skip_step(
                "law_substitution",
                &law.id,
                &format!("主张 `{target}` 无不确定度声明，无法容差判定"),
            ));
            continue;
        };
        let tol = k * sigma;
        let delta = (computed - view.value).abs();
        let side = if flipped { "左" } else { "右" };
        let outcome = if delta <= tol {
            StepOutcome::Consistent
        } else {
            any_violated = true;
            StepOutcome::Violated
        };
        let comparison = if delta <= tol { "≤" } else { ">" };
        steps.push(VerifyStep {
            step: steps.len() + 1,
            kind: "law_substitution",
            target: format!("{}@{}", law.id, target),
            outcome,
            detail: format!(
                "代入{side}侧 `{}` 得 {computed:e}，主张 {value:e}，|Δ|={delta:e} {comparison} k·σ={tol:e}{suffix}",
                expr.trim(),
                value = view.value,
                suffix = if outcome == StepOutcome::Violated {
                    "（定律链内互斥）"
                } else {
                    ""
                }
            ),
        });
        if outcome == StepOutcome::Violated {
            proposals.push(ProposalPackage {
                kind: "law_review",
                dataset_id: lr.dataset_id.to_string(),
                entry_id: law.id.clone(),
                subject: req.subject.clone(),
                payload: serde_json::json!({
                    "formula": law.formula,
                    "target": target,
                    "claimed": view.value,
                    "computed": computed,
                    "tolerance": tol,
                    "substitution": sub,
                }),
                reason: "定律代入结果与主张超出容差（判定只提案：人工经治理链入闸核议）"
                    .to_string(),
            });
        }
    }
    Ok(any_violated)
}

/// 代入表构建：主张优先、常量唯一命中兜底；缺值/歧义返回 None（调用方跳过记因）
fn build_substitution(
    idents: &[String],
    target: &str,
    claims: &BTreeMap<&str, ClaimView>,
    const_index: &BTreeMap<&str, Vec<&ConstantRef<'_>>>,
) -> Option<BTreeMap<String, f64>> {
    let mut sub: BTreeMap<String, f64> = BTreeMap::new();
    for ident in idents {
        if ident == target {
            return None; // 目标量出现于表达式侧（需自洽核验形态，MVP 跳过）
        }
        if let Some(v) = claims.get(ident.as_str()) {
            sub.insert(ident.clone(), v.value);
            continue;
        }
        match resolve_constant(const_index, ident) {
            ConstantMatch::Unique(_, c) => {
                sub.insert(ident.clone(), c.value);
            }
            ConstantMatch::Ambiguous(_) | ConstantMatch::NotFound => return None,
        }
    }
    Some(sub)
}

/// 步③ 常量对照：主张符号唯一命中常量时做 k·σ 区间比较
/// （σ_eff = max(常量σ, 主张σ)）；越界记 alert 证据并附提案物料包。
/// 返回是否出现区间越界（alert 证据）。
fn constant_comparison(
    req: &VerifyRequest,
    k: f64,
    claims: &BTreeMap<&str, ClaimView>,
    const_index: &BTreeMap<&str, Vec<&ConstantRef<'_>>>,
    steps: &mut Vec<VerifyStep>,
    proposals: &mut Vec<ProposalPackage>,
) -> bool {
    let mut any_out_of_bounds = false;
    for (symbol, view) in claims {
        let (ds, c) = match resolve_constant(const_index, symbol) {
            ConstantMatch::Unique(ds, c) => (ds, c),
            ConstantMatch::Ambiguous(owners) => {
                steps.push(skip_step(
                    "constant_comparison",
                    symbol,
                    &format!(
                        "跨库同 id 歧义（{}），需指定命名空间后复验",
                        owners.join(", ")
                    ),
                ));
                continue;
            }
            ConstantMatch::NotFound => continue,
        };
        let sigma_eff = match (
            c.metadata
                .uncertainty
                .as_deref()
                .and_then(parse_uncertainty),
            view.uncertainty,
        ) {
            (Some(cs), Some(us)) => Some(cs.max(us)),
            (Some(cs), None) => Some(cs),
            (None, Some(us)) => Some(us),
            (None, None) => None,
        };
        let Some(sigma) = sigma_eff else {
            steps.push(skip_step(
                "constant_comparison",
                symbol,
                &format!("常量 `{}` 与主张均无不确定度声明，无法区间对照", c.id),
            ));
            continue;
        };
        let tol = k * sigma;
        let delta = (view.value - c.value).abs();
        let outcome = if delta <= tol {
            StepOutcome::Consistent
        } else {
            any_out_of_bounds = true;
            StepOutcome::OutOfBounds
        };
        let comparison = if delta <= tol { "≤" } else { ">" };
        steps.push(VerifyStep {
            step: steps.len() + 1,
            kind: "constant_comparison",
            target: format!("{}@{}", c.id, ds),
            outcome,
            detail: format!(
                "主张 {value:e} vs 常量 {ref:e}（σ={sigma:e}），|Δ|={delta:e} {comparison} k·σ={tol:e}{suffix}",
                value = view.value,
                ref = c.value,
                suffix = if outcome == StepOutcome::OutOfBounds {
                    "（区间越界）"
                } else {
                    ""
                }
            ),
        });
        if outcome == StepOutcome::OutOfBounds {
            proposals.push(ProposalPackage {
                kind: "constant_update",
                dataset_id: ds.to_string(),
                entry_id: c.id.clone(),
                subject: req.subject.clone(),
                payload: serde_json::json!({
                    "current": {
                        "value": c.value,
                        "si_unit": c.si_unit,
                        "metadata": c.metadata,
                    },
                    "measured": {
                        "value": view.value,
                        "si_unit": req
                            .claims
                            .iter()
                            .find(|x| x.symbol == **symbol)
                            .map(|x| x.si_unit.clone())
                            .unwrap_or_default(),
                        "uncertainty": view.uncertainty,
                        "source": req.context.source,
                        "measured_at": req.context.measured_at,
                        "method": req.context.method,
                    },
                    "k_sigma": k,
                }),
                reason: "常量对照越界（判定只提案：人工经治理链入闸核议）".to_string(),
            });
        }
    }
    any_out_of_bounds
}

fn skip_step(kind: &'static str, target: &str, reason: &str) -> VerifyStep {
    VerifyStep {
        step: 0, // 聚合前占位，插入时由调用方改序——见下
        kind,
        target: target.to_string(),
        outcome: StepOutcome::Skipped,
        detail: reason.to_string(),
    }
}

// ============================================================================
// 测试（golden 三态 + 量纲拒判 + 结构反例 + schema 自洽 + serde 确定性）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── golden 物料（与 law_schema/eval 侧同族：Newton + 万有引力 + 常量集）──

    fn newton_law() -> Law {
        serde_json::from_value(json!({
            "id": "newton_second",
            "formula": "F = m * a",
            "symbols": { "F": "合外力", "m": "质量", "a": "加速度" },
            "si_units": { "F": "newton", "m": "kilogram", "a": "m*s^-2" }
        }))
        .unwrap()
    }

    fn gravity_law() -> Law {
        serde_json::from_value(json!({
            "id": "universal_gravitation",
            "formula": "F = G * (m1 * m2) / r^2",
            "symbols": {
                "F": "引力", "G": "引力常量", "m1": "质量一", "m2": "质量二", "r": "距离"
            },
            "si_units": {
                "F": "newton", "G": "m^3*kg^-1*s^-2", "m1": "kilogram",
                "m2": "kilogram", "r": "m"
            }
        }))
        .unwrap()
    }

    fn big_g_constant(value: f64, uncertainty: &str) -> ConstantEntry {
        ConstantEntry {
            id: "G".to_string(),
            value,
            si_unit: "m^3*kg^-1*s^-2".to_string(),
            metadata: ConstantMetadata {
                source: "CODATA 2018".to_string(),
                uncertainty: Some(uncertainty.to_string()),
                verified: Some("2026-11-07".to_string()),
            },
        }
    }

    fn request(claims: Value, k_sigma: Option<f64>) -> VerifyRequest {
        let mut ctx = json!({
            "source": "bench recheck 2026-11-07",
            "measured_at": "2026-11-07T00:00:00Z"
        });
        if let Some(k) = k_sigma {
            ctx["k_sigma"] = json!(k);
        }
        serde_json::from_value(json!({
            "$schema": VERIFY_REQUEST_ID,
            "subject": "golden verification proposition",
            "claims": claims,
            "context": ctx
        }))
        .unwrap()
    }

    // ── golden 一：stable（G 值复测相容 + F=ma 代入相容）──

    #[test]
    fn golden_stable_constant_recheck_and_law_substitution() {
        let g = big_g_constant(6.674e-11, "0.0003e-11");
        let laws = [LawRef {
            dataset_id: "law-mechanics",
            law: &newton_law(),
        }];
        let constants = [ConstantRef {
            dataset_id: "const-fundamental",
            constant: &g,
        }];
        // G 主张在 k·σ 内（2×0.0003e-11 = 0.0006e-11 容差）
        let req = request(
            json!([{ "symbol": "G", "value": 6.6742e-11, "si_unit": "m^3*kg^-1*s^-2",
                     "uncertainty": 0.0001e-11 }]),
            None,
        );
        let report = verify(&req, &laws, &constants).unwrap();
        assert_eq!(report.verdict, Verdict::Stable);
        assert!(report.proposals.is_empty(), "stable 态零提案");
        assert!(
            report
                .steps
                .iter()
                .any(|s| s.kind == "constant_comparison" && s.outcome == StepOutcome::Consistent),
            "常量对照步应相容: {:?}",
            report.steps
        );

        // F=ma 代入相容（主张 F=10±0.5, m=2, a=5 → 计算 10）
        let req = request(
            json!([
                { "symbol": "F", "value": 10.0, "si_unit": "newton", "uncertainty": 0.5 },
                { "symbol": "m", "value": 2.0, "si_unit": "kilogram" },
                { "symbol": "a", "value": 5.0, "si_unit": "m*s^-2" }
            ]),
            None,
        );
        let report = verify(&req, &laws, &constants).unwrap();
        assert_eq!(report.verdict, Verdict::Stable);
        let sub_step = report
            .steps
            .iter()
            .find(|s| s.kind == "law_substitution")
            .unwrap();
        assert_eq!(sub_step.outcome, StepOutcome::Consistent);
        assert!(
            sub_step.detail.contains("≤ k·σ"),
            "计算值应入证据: {}",
            sub_step.detail
        );
    }

    // ── golden 二：alert（常量越界 k·σ + 提案物料包）──

    #[test]
    fn golden_alert_constant_out_of_bounds_with_proposal() {
        let g = big_g_constant(6.674e-11, "0.0003e-11");
        let constants = [ConstantRef {
            dataset_id: "const-fundamental",
            constant: &g,
        }];
        // 主张偏离 0.005e-11 >> 2σ=0.0006e-11 → alert
        let req = request(
            json!([{ "symbol": "G", "value": 6.679e-11, "si_unit": "m^3*kg^-1*s^-2",
                     "uncertainty": 0.0001e-11 }]),
            None,
        );
        let report = verify(&req, &[], &constants).unwrap();
        assert_eq!(report.verdict, Verdict::Alert);
        assert_eq!(report.proposals.len(), 1, "alert 态必附提案物料包");
        let p = &report.proposals[0];
        assert_eq!(p.kind, "constant_update");
        assert_eq!(p.dataset_id, "const-fundamental");
        assert_eq!(p.entry_id, "G");
        assert!(
            p.payload["current"]["value"].as_f64().unwrap() - 6.674e-11 < 1e-30,
            "现值视图应含常量现值"
        );
        assert!(
            p.payload["measured"]["source"]
                .as_str()
                .unwrap()
                .contains("bench"),
            "实测视图应含溯源"
        );
        let step = report
            .steps
            .iter()
            .find(|s| s.kind == "constant_comparison")
            .unwrap();
        assert_eq!(step.outcome, StepOutcome::OutOfBounds);
        assert!(
            step.detail.contains("> k·σ"),
            "越界证据应含比较式: {}",
            step.detail
        );
    }

    // ── golden 三：contradiction（与 F=ma 定律链矛盾）──

    #[test]
    fn golden_contradiction_law_chain_violated() {
        let laws = [LawRef {
            dataset_id: "law-mechanics",
            law: &newton_law(),
        }];
        // 主张 F=10±0.1, m=2, a=4 → 计算 8，|Δ|=2 >> 0.2 → contradiction
        let req = request(
            json!([
                { "symbol": "F", "value": 10.0, "si_unit": "newton", "uncertainty": 0.1 },
                { "symbol": "m", "value": 2.0, "si_unit": "kilogram" },
                { "symbol": "a", "value": 4.0, "si_unit": "m*s^-2" }
            ]),
            None,
        );
        let report = verify(&req, &laws, &[]).unwrap();
        assert_eq!(report.verdict, Verdict::Contradiction);
        let step = report
            .steps
            .iter()
            .find(|s| s.kind == "law_substitution")
            .unwrap();
        assert_eq!(step.outcome, StepOutcome::Violated);
        assert_eq!(report.proposals.len(), 1);
        assert_eq!(report.proposals[0].kind, "law_review");
    }

    // ── golden 四：万有引力代入（常量兜底 + 括号 + 幂次文法）──

    #[test]
    fn golden_gravity_substitution_with_constant_fallback() {
        let g = big_g_constant(6.674e-11, "0.0003e-11");
        let laws = [LawRef {
            dataset_id: "law-mechanics",
            law: &gravity_law(),
        }];
        let constants = [ConstantRef {
            dataset_id: "const-fundamental",
            constant: &g,
        }];
        // F = G*(m1*m2)/r^2：m1=m2=1kg, r=1m → F=G=6.674e-11 N
        let req = request(
            json!([
                { "symbol": "F", "value": 6.674e-11, "si_unit": "newton", "uncertainty": 1e-13 },
                { "symbol": "m1", "value": 1.0, "si_unit": "kilogram" },
                { "symbol": "m2", "value": 1.0, "si_unit": "kilogram" },
                { "symbol": "r", "value": 1.0, "si_unit": "m" }
            ]),
            None,
        );
        let report = verify(&req, &laws, &constants).unwrap();
        assert_eq!(report.verdict, Verdict::Stable, "{:?}", report.steps);
        let sub_step = report
            .steps
            .iter()
            .find(|s| s.kind == "law_substitution")
            .unwrap();
        assert_eq!(sub_step.outcome, StepOutcome::Consistent);
    }

    // ── 拒判：量纲不一致（报两侧量纲）──

    #[test]
    fn dimensional_mismatch_rejected_reporting_both_vectors() {
        let laws = [LawRef {
            dataset_id: "law-mechanics",
            law: &newton_law(),
        }];
        // 主张 a 的单位给 m*s^-1（速度量纲），与定律 m*s^-2 对表不一致
        let req = request(
            json!([
                { "symbol": "F", "value": 10.0, "si_unit": "newton", "uncertainty": 0.5 },
                { "symbol": "m", "value": 2.0, "si_unit": "kilogram" },
                { "symbol": "a", "value": 5.0, "si_unit": "m*s^-1" }
            ]),
            None,
        );
        let err = verify(&req, &laws, &[]).unwrap_err();
        match err {
            VerifyError::DimensionalMismatch {
                symbol,
                claim_dims,
                other_dims,
                ..
            } => {
                assert_eq!(symbol, "a");
                assert_eq!(claim_dims, "m*s^-1");
                assert_eq!(other_dims, "m*s^-2");
            }
            other => panic!("应为 DimensionalMismatch: {other:?}"),
        }
    }

    // ── 拒判：结构层（超限/单位不可解析）──

    #[test]
    fn structural_rejections() {
        // 数值越界
        let req = request(
            json!([{ "symbol": "x", "value": 1e200, "si_unit": "m" }]),
            None,
        );
        match verify(&req, &[], &[]).unwrap_err() {
            VerifyError::Schema { errors } => {
                assert!(errors.iter().any(|e| e.contains("幅值越界")), "{errors:?}");
            }
            other => panic!("应为 Schema 拒收: {other:?}"),
        }

        // validate_verify_request：单位不可解析 fail-fast
        let doc = json!({
            "$schema": VERIFY_REQUEST_ID,
            "subject": "s",
            "claims": [{ "symbol": "x", "value": 1.0, "si_unit": "newtn" }],
            "context": { "source": "src", "measured_at": "t" }
        });
        let report = validate_verify_request(&doc);
        assert!(!report.valid, "单位不可解析应拒收");
        assert!(
            report.errors.iter().any(|e| e.contains("不可解析")),
            "{:?}",
            report.errors
        );

        // k_sigma 越界
        let req = request(
            json!([{ "symbol": "x", "value": 1.0, "si_unit": "m" }]),
            Some(101.0),
        );
        assert!(matches!(
            verify(&req, &[], &[]),
            Err(VerifyError::Schema { .. })
        ));
    }

    // ── 定律扫描上限 + 跳过语义 ──

    #[test]
    fn law_scan_limit_and_skip_semantics() {
        // 65 条涉及定律 → LawScanLimit
        let laws: Vec<LawRef<'_>> = (0..65)
            .map(|i| {
                let law = serde_json::from_value::<Law>(json!({
                    "id": format!("law_{i}"),
                    "formula": "F = m * a",
                    "symbols": { "F": "力", "m": "质量", "a": "加速度" },
                    "si_units": { "F": "newton", "m": "kilogram", "a": "m*s^-2" }
                }))
                .unwrap();
                LawRef {
                    dataset_id: "ds",
                    law: Box::leak(Box::new(law)),
                }
            })
            .collect();
        let req = request(
            json!([{ "symbol": "F", "value": 1.0, "si_unit": "newton", "uncertainty": 0.1 }]),
            None,
        );
        match verify(&req, &laws, &[]).unwrap_err() {
            VerifyError::LawScanLimit { count, limit } => {
                assert_eq!(count, 65);
                assert_eq!(limit, MAX_LAWS_PER_REQUEST);
            }
            other => panic!("应为 LawScanLimit: {other:?}"),
        }

        // 目标非裸形态（P * V = n * R * T，主张 P）→ 跳过记因
        let ideal_gas = serde_json::from_value::<Law>(json!({
            "id": "ideal_gas",
            "formula": "P * V = n * R * T",
            "symbols": { "P": "压强", "V": "体积", "n": "物质的量", "R": "气体常数", "T": "温度" },
            "si_units": { "P": "pascal", "V": "m^3", "n": "mol", "R": "joule*mol^-1*K^-1", "T": "kelvin" }
        }))
        .unwrap();
        let laws = [LawRef {
            dataset_id: "law-thermo",
            law: &ideal_gas,
        }];
        let req = request(
            json!([{ "symbol": "P", "value": 100.0, "si_unit": "pascal", "uncertainty": 1.0 }]),
            None,
        );
        let report = verify(&req, &laws, &[]).unwrap();
        assert_eq!(report.verdict, Verdict::Stable, "跳过不构成矛盾");
        let step = report
            .steps
            .iter()
            .find(|s| s.kind == "law_substitution")
            .unwrap();
        assert_eq!(step.outcome, StepOutcome::Skipped);
        assert!(step.detail.contains("不做符号重排"), "{}", step.detail);
    }

    // ── 跨库同 id 常量歧义：跳过记因，不猜 ──

    #[test]
    fn ambiguous_constant_id_skips_with_reason() {
        let g1 = big_g_constant(6.674e-11, "0.0003e-11");
        let g2 = big_g_constant(6.680e-11, "0.0003e-11");
        let constants = [
            ConstantRef {
                dataset_id: "set-a",
                constant: &g1,
            },
            ConstantRef {
                dataset_id: "set-b",
                constant: &g2,
            },
        ];
        let req = request(
            json!([{ "symbol": "G", "value": 6.674e-11, "si_unit": "m^3*kg^-1*s^-2",
                     "uncertainty": 0.0001e-11 }]),
            None,
        );
        let report = verify(&req, &[], &constants).unwrap();
        assert_eq!(report.verdict, Verdict::Stable, "歧义跳过不判越界");
        let step = report
            .steps
            .iter()
            .find(|s| s.kind == "constant_comparison")
            .unwrap();
        assert_eq!(step.outcome, StepOutcome::Skipped);
        assert!(step.detail.contains("歧义"), "{}", step.detail);
    }

    // ── constant_set 校验（无出处拒收=铁律 2；不确定度非数字拒收）──

    #[test]
    fn constant_set_validation_rejections() {
        let valid = json!({
            "$schema": CONSTANT_SET_ID,
            "kind": "constant_set",
            "id": "fundamental",
            "version": "1.0.0",
            "constants": [{
                "id": "G", "value": 6.674e-11, "si_unit": "m^3*kg^-1*s^-2",
                "metadata": { "source": "CODATA 2018", "uncertainty": "0.0003e-11" }
            }]
        });
        assert!(validate_constant_set(&valid).valid);

        // 无出处 → 拒收
        let mut no_source = valid.clone();
        no_source["constants"][0]["metadata"] = json!({ "verified": "x" });
        let report = validate_constant_set(&no_source);
        assert!(!report.valid, "无出处常量应拒收（铁律 2）");
        assert!(
            report.errors.iter().any(|e| e.contains("source")),
            "{:?}",
            report.errors
        );

        // 不确定度非数字字面量 → 拒收
        let mut bad_sigma = valid.clone();
        bad_sigma["constants"][0]["metadata"]["uncertainty"] = json!("about right");
        let report = validate_constant_set(&bad_sigma);
        assert!(!report.valid, "不确定度非数字应拒收");
        assert!(
            report.errors.iter().any(|e| e.contains("数字字面量")),
            "{:?}",
            report.errors
        );

        // 未知键 → 拒收（additionalProperties:false）
        let mut unknown = valid.clone();
        unknown["constants"][0]["note"] = json!("extra");
        assert!(!validate_constant_set(&unknown).valid);

        // kind 漂移 → 拒收
        let mut kind_drift = valid.clone();
        kind_drift["kind"] = json!("law_set");
        assert!(!validate_constant_set(&kind_drift).valid);
    }

    // ── verify_request schema 自洽 + serde 确定性 ──

    #[test]
    fn verify_request_schema_passes_meta_schema() {
        let schema: Value = serde_json::from_str(VERIFY_REQUEST_JSON)
            .expect("内嵌 verify_request schema 必须可解析");
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
            "verify_request schema 应过 meta-schema: {errors:?}"
        );
    }

    #[test]
    fn verify_request_unknown_field_rejected() {
        let doc = json!({
            "$schema": VERIFY_REQUEST_ID,
            "subject": "s",
            "claims": [{ "symbol": "x", "value": 1.0, "si_unit": "m", "note": "extra" }],
            "context": { "source": "src", "measured_at": "t" }
        });
        assert!(!validate_verify_request(&doc).valid, "claim 未知键应拒收");

        let doc = json!({
            "$schema": VERIFY_REQUEST_ID,
            "subject": "s",
            "claims": [{ "symbol": "x", "value": 1.0, "si_unit": "m" }],
            "context": { "source": "src", "measured_at": "t" },
            "extra": 1
        });
        assert!(!validate_verify_request(&doc).valid, "顶层未知键应拒收");
    }

    #[test]
    fn report_serde_shape_stable() {
        // 判定词汇序列化形态立法锚：stable/alert/contradiction 小写下划线
        assert_eq!(
            serde_json::to_value(Verdict::Contradiction).unwrap(),
            json!("contradiction")
        );
        assert_eq!(
            serde_json::to_value(Verdict::Alert).unwrap(),
            json!("alert")
        );
        assert_eq!(
            serde_json::to_value(Verdict::Stable).unwrap(),
            json!("stable")
        );
        assert_eq!(
            serde_json::to_value(StepOutcome::OutOfBounds).unwrap(),
            json!("out_of_bounds")
        );
    }
}

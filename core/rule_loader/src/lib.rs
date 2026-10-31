// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! evorule 宪法装载唯一通道（[CON-L2-001]～[CON-L2-006]、[CON-S-001]～[CON-S-006]）
//!
//! # 定位（防御层收口；尽调结论已沉淀为宪法条文正本）
//!
//! 本 crate 是「规则集进入引擎」的**唯一装载函数** `load_rule_set`：
//! 宪法 schema 校验 → 剥壳 → 门禁检查 → provenance，四步固定顺序、不可拆、不可跳。
//! 生产 loader（server.rs 双 loader）与沙盒 loader（sandbox parse_draft_rules）
//! 一律改调本函数——消灭「第二条装载路径」（B-3 四缺陷 A/B2 的制度根源）。
//!
//! # 四步固定顺序（[CON-L2-002]）
//!
//! 1. **宪法 schema 校验**（`evorule_rule_schema::validate_rule_set` / `validate_transform_list`）
//! 2. **剥壳**（提取 `transform[]`；兼容带壳 rule_set 文档与裸 transform 数组两形态）
//! 3. **门禁检查**（[CON-L2-003] 三门不减为下限，venue=Sandbox 同守）：
//!    - tier 门：非 L2 携 enforce 拒（引擎级阻止权仅随治理链晋升的 L2 条目下发）
//!    - discipline 门：DC-01..09 形态纪律（evorule-discipline crate）
//!    - domain on_missing 门：domain 节点缺 on_missing 显式声明拒
//!    - 量纲登记门（随守恒不变量裁决立法增补）：transform 引用立法计算裁决指令
//!      （conservation_check）须携带 metadata.formula_registry 结构合法登记——
//!      「公式 → 量纲」登记为规则集的一部分，未登记拒装载
//! 4. **provenance**（[CON-S-003]）：来源指纹+门禁记录+宪法版本，可序列化入审计
//!
//! # 门-钥配套（硬币两面，[CON-X-002]）
//!
//! 每个拒绝都带四段式报错（[CON-K2-001]）：违什么 / 在哪 / 正本 / 正确样例。
//! 欠绝教学即遵循帮助——光拦不指路=逼人绕门。
//!
//! # serde 域 ↔ TCB 域转换
//!
//! `serde_to_tcb` 自 server.rs:4970 / platform_auth.rs:578 双份手写收编为唯一实现。

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use evorule_rule_schema::{validate_rule_set, validate_transform_list};
use evorule_tcb::JsonValue;
use serde::Serialize;
use serde_json::Value;

/// 宪法 schema 版本（与本 crate 依赖的 rule_schema 副本一致，provenance 用）
pub const CONSTITUTION_SCHEMA_VERSION: &str = "rule_set/v1.0";

/// 四段式报错导览入口（[CON-K1-002] 报错必指路）
pub const GUIDE_URL: &str = "https://evorule.org/schemas/docs/conformance-guide";

// ============================================================================
// 装载上下文与结果（[CON-S-001] API 签名）
// ============================================================================

/// 规则集来源（provenance 溯源用）
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LoadSource {
    /// 文件系统（生产 rules_dir / 宪法 core_eval.json）
    File { path: String },
    /// DB 草稿（沙盒 rule_versions）
    DbDraft {
        rule_version_ids: Vec<String>,
        ruleset_hash: String,
    },
    /// API 提交
    Api { caller: String },
}

/// 装载场地（[CON-L2-003]：三门不减——沙盒同守生产纪律）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Venue {
    Production,
    Sandbox,
}

/// 规则集组成语义（Q-1 定型：[CON-L2-006]，P-1 批文=合并）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Composition {
    Merged,
    Standalone,
}

/// L2 身份类别（tier 门与 domain 门豁免的判定依据，对齐 server 侧清单面/合并层
/// 的双向纪律：文件级按治理事实定类，业务条目归合并层条目级门）
///
/// - `None`：业务文件/沙盒草稿/API 提交——禁 enforce、禁声明 L2 tier、domain 门全守
/// - `Constraint`：治理链晋升的约束文件（rules_dir 根 `00_constraint_*`/`00_meta_*` 前缀，
///   前缀判定在调用方）——enforce 许可，**必须**声明 `metadata.tier ∈ {constraint, meta}`，
///   domain 门照守（生产豁免仅给宪法前缀，约束文件在合并列表 idx≥constitution_len）
/// - `Constitution`：宪法本体（core_eval.json / server_eval.json）——enforce 许可
///   （现宪法实无 enforce 条目）、eq/lt domain 声明门豁免（宪法内部组合由维护者负责）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum L2Class {
    None,
    Constraint,
    Constitution,
}

/// discipline 门 enforce 位的 env 值判定（单一事实源，server 测试桥同调本函数）。
/// R7/[CON-L2-004] fail-closed 收口：合法值域 = {缺省/空串, "warn", "enforce"}
/// （大小写不敏感）——**缺省/空串/"enforce" = `true`**（违规拒载；W1 观测期豁免
/// 已于 2026-10-14 收口：复审观测数据干净，统帅裁定提前切 enforce 缺省，见
/// [CON-L2-004] W1 收口注记与 SR-16）；显式 `"warn"` = `false`
/// （观测期回退口保留：一键降级 env 切 warn，无需发版）；**非法值 = panic 拒载**：
/// 门禁配置错误属部署事故，fail-closed 快速失败优于静默降级为观测态，
/// 报错携带修正指引。
pub fn discipline_enforce_from(value: Option<&str>) -> bool {
    match value {
        None | Some("") => true,
        Some(v) if v.eq_ignore_ascii_case("enforce") => true,
        Some(v) if v.eq_ignore_ascii_case("warn") => false,
        Some(other) => panic!(
            "EVORULE_DISCIPLINE_GATE 非法值 {other:?}（合法值: enforce | warn | 缺省/空串）。\
             [CON-L2-004] fail-closed：修正环境变量后重启（warn=纯观测，enforce=违规拒载）"
        ),
    }
}

/// 装载上下文
#[derive(Debug, Clone, Serialize)]
pub struct LoadContext {
    pub source: LoadSource,
    pub venue: Venue,
    pub composition: Composition,
    /// L2 身份（调用方依治理事实声明，provenance 留痕可审计）
    pub l2_class: L2Class,
    /// discipline 门 enforce 位（W1 观测期豁免已于 2026-10-14 收口：缺省 true=
    /// 违规拒载；env EVORULE_DISCIPLINE_GATE 显式 warn 时 false=放行+留痕，
    /// 为一键降级回退口）。
    /// env 为 warn/缺省/空串判定与渐进收口史见 `discipline_enforce_from`；
    /// 非法值 fail-closed（SR-13/R7）。
    pub discipline_enforce: bool,
}

impl LoadContext {
    /// discipline 门 enforce 位（从调用方环境取——单一事实源 env EVORULE_DISCIPLINE_GATE，
    /// 判定 SSOT = discipline_enforce_from）
    fn env_discipline_enforce() -> bool {
        discipline_enforce_from(std::env::var("EVORULE_DISCIPLINE_GATE").ok().as_deref())
    }

    /// 宪法本体装载（core_eval.json / server_eval.json）
    pub fn constitution(path: &str) -> Self {
        LoadContext {
            source: LoadSource::File {
                path: path.to_string(),
            },
            venue: Venue::Production,
            composition: Composition::Standalone,
            l2_class: L2Class::Constitution,
            discipline_enforce: Self::env_discipline_enforce(),
        }
    }

    /// rules_dir 约束文件装载（`00_constraint_*`/`00_meta_*` 前缀，调用方判定）
    pub fn constraint_file(path: &str) -> Self {
        LoadContext {
            source: LoadSource::File {
                path: path.to_string(),
            },
            venue: Venue::Production,
            composition: Composition::Standalone,
            l2_class: L2Class::Constraint,
            discipline_enforce: Self::env_discipline_enforce(),
        }
    }

    /// 普通业务规则文件装载
    pub fn business_file(path: &str) -> Self {
        LoadContext {
            source: LoadSource::File {
                path: path.to_string(),
            },
            venue: Venue::Production,
            composition: Composition::Standalone,
            l2_class: L2Class::None,
            discipline_enforce: Self::env_discipline_enforce(),
        }
    }

    /// 沙盒装载上下文（Q-1=合并语义默认）
    pub fn sandbox(source: LoadSource) -> Self {
        LoadContext {
            source,
            venue: Venue::Sandbox,
            composition: Composition::Merged,
            l2_class: L2Class::None,
            discipline_enforce: Self::env_discipline_enforce(),
        }
    }

    /// API 提交装载（在线提交规则）
    pub fn api(caller: &str) -> Self {
        LoadContext {
            source: LoadSource::Api {
                caller: caller.to_string(),
            },
            venue: Venue::Production,
            composition: Composition::Standalone,
            l2_class: L2Class::None,
            discipline_enforce: Self::env_discipline_enforce(),
        }
    }
}

/// 门禁通过记录（provenance 组成部分）
#[derive(Debug, Copy, Clone, Serialize)]
pub struct GatesPassed {
    pub tier_gate: bool,
    pub discipline_gate: bool,
    pub domain_on_missing_gate: bool,
    /// 量纲登记门（立法计算裁决指令引用须携带 metadata.formula_registry
    /// 结构合法登记；随守恒不变量裁决立法增补，三门不减为下限）
    pub dimension_registry_gate: bool,
}

/// 装载结果（[CON-S-001]：engine_rules + provenance）
///
/// 注：TCB JsonValue 不实现 serde Serialize（TCB 无 serde 依赖——确定性域不引 serde）；
/// 审计日志序列化 provenance（自带 Serialize），engine_rules 不序列化（引擎域原生）。
#[derive(Debug, Clone)]
pub struct LoadedRuleset {
    /// 剥壳后的引擎原生 transform 规则列表（TCB 域）
    pub engine_rules: Vec<JsonValue>,
    /// 溯源指纹
    pub provenance: Provenance,
}

/// 溯源指纹（[CON-S-003]）
#[derive(Debug, Clone, Serialize)]
pub struct Provenance {
    /// 来源类别标签（快速分流）
    pub source_kind: &'static str,
    /// 完整来源指纹（[CON-S-003] 来源指纹收编：文件路径 / DB 版本 ID 集+ruleset hash / API caller）
    pub source: LoadSource,
    pub venue: Venue,
    pub composition: Composition,
    pub l2_class: L2Class,
    /// 内容指纹（R4/[CON-S-003] 补课）：对装载输入文档的 blake3（`blake3:hex`，
    /// SSOT 经 evorule-hash，族 B 哈希纪律禁自写口径）。路径指纹描述「从哪来」，
    /// 内容指纹描述「装的是什么」——路径不变内容变/路径变内容同都可区分；
    /// 序列化取 serde_json 紧凑字节（Value 键序= BTreeMap，确定性）。
    pub content_hash: String,
    pub gates: GatesPassed,
    pub constitution_version: &'static str,
    /// 装载时间戳（unix 秒；确定性装载=时间戳仅入 provenance，不入规则语义）
    pub loaded_at_unix: u64,
}
// ============================================================================
// 四段式报错（[CON-K2-001]）
// ============================================================================

/// 四段式错误：违什么 / 在哪 / 正本 / 正确样例
#[derive(Debug, Clone, Serialize)]
pub struct FourPartError {
    /// ① 违什么
    pub violation: String,
    /// ② 在哪（JSON 路径）
    pub location: String,
    /// ③ 正本链接
    pub reference: String,
    /// ④ 最小正确样例（[CON-K2-002]）
    pub minimal_example: String,
}

impl std::fmt::Display for FourPartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[违宪] {}\n  位置：{}\n  正本：{}\n  正确形态：{}\n  导览：{GUIDE_URL}",
            self.violation, self.location, self.reference, self.minimal_example
        )
    }
}

/// 装载错误（[CON-S-002]）
#[derive(Debug, Clone, Serialize)]
pub enum ConstitutionError {
    SchemaViolation(FourPartError),
    ShellUnrecognized(FourPartError),
    TierGate(FourPartError),
    DisciplineGate(FourPartError),
    DomainOnMissingGate(FourPartError),
    DimensionRegistryGate(FourPartError),
    ProvenanceIncomplete(String),
}

impl std::fmt::Display for ConstitutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConstitutionError::SchemaViolation(e)
            | ConstitutionError::ShellUnrecognized(e)
            | ConstitutionError::TierGate(e)
            | ConstitutionError::DisciplineGate(e)
            | ConstitutionError::DomainOnMissingGate(e)
            | ConstitutionError::DimensionRegistryGate(e) => write!(f, "{e}"),
            ConstitutionError::ProvenanceIncomplete(s) => {
                write!(f, "[装载] provenance 不完整：{s}")
            }
        }
    }
}

impl std::error::Error for ConstitutionError {}

// ============================================================================
// 唯一装载函数（[CON-L2-001]～[CON-L2-004]）
// ============================================================================

/// 宪法装载唯一入口。四步固定顺序（[CON-L2-002]）不可拆不可跳；
/// fail-fast（[CON-L2-004]）：任何一步失败即整体失败，零部分装载、零降级。
///
/// # venue 不减免（[CON-L2-003]）
///
/// 存量核查收口后沙盒裁决铸 `sandbox:` 背书=治理行为；铸造信任者必须自守治理纪律，
/// 沙盒草稿与生产规则同守三门。
pub fn load_rule_set(doc: &Value, ctx: &LoadContext) -> Result<LoadedRuleset, ConstitutionError> {
    let (engine_rules, provenance) = load_one(doc, ctx, ctx.composition)?;
    Ok(LoadedRuleset {
        engine_rules,
        provenance,
    })
}

/// 合并装载部件（[CON-S-005] 装载门合并：部件文档+部件装载上下文成对提交）
#[derive(Debug, Clone)]
pub struct MergedPart {
    /// 部件文档（与单件装载同形态：带壳 rule_set / 部分壳 / 裸数组）
    pub doc: Value,
    /// 部件装载上下文（source/l2_class/venue 以部件自身治理事实声明；
    /// composition 字段在合并入口被强制为 Merged）
    pub ctx: LoadContext,
}

/// 合并装载的单部件结果（[CON-S-003] provenance 逐部件留痕）
#[derive(Debug, Clone, Serialize)]
pub struct PartOutcome {
    /// 部件溯源指纹（composition=Merged；content_hash 对部件文档计算）
    pub provenance: Provenance,
    /// 部件贡献的引擎规则条数（调用方 sources 标签展开依据）
    pub rule_count: usize,
}

/// 合并装载结果（[CON-S-005] 装载门合并）
#[derive(Debug, Clone)]
pub struct MergedLoaded {
    /// 全部件按传入序拼合的引擎规则（宪法在前=调用方传入序纪律）
    pub engine_rules: Vec<JsonValue>,
    /// 逐部件结果（与传入 parts 同序同长）
    pub parts: Vec<PartOutcome>,
}

/// 合并装载入口（[CON-S-005] 定稿：两层分工之「装载门合并」层）。
///
/// 生产/DB·API 路径的多部件合并**必须**经此入口：各部件以自身 ctx 重过
/// 四步门（与单件装载确定性等判），**函数内**按传入序拼合——禁止调用方
/// 拿单件结果自行 extend 拼装。任一部件门禁失败=整体失败（fail-fast，
/// 零部分装载），报错带部件序号标注。
///
/// 与「会话挂载合并」（SR-03，fork 时 core_eval_override 挂载面）分工：
/// 本入口管装载期合并；挂载期合并由 governance 侧实施（沙盒=全局规则集
/// +过门草稿全集，不在本函数管辖）。
pub fn load_rule_set_merged(parts: &[MergedPart]) -> Result<MergedLoaded, ConstitutionError> {
    // minItems=1：空部件清单=空规则集（transform minItems 同理拒）
    if parts.is_empty() {
        return Err(ConstitutionError::SchemaViolation(four_part(
            "合并装载部件清单为空（minItems=1）",
            "$（合并部件清单）",
            RULE_SET_URL,
            MINIMAL_TRANSFORM_RULE,
        )));
    }
    let mut engine_rules: Vec<JsonValue> = Vec::new();
    let mut outcomes: Vec<PartOutcome> = Vec::with_capacity(parts.len());
    for (i, part) in parts.iter().enumerate() {
        // 部件以自身 ctx 重过四步门（与单件装载同 ctx 同判=确定性等判），
        // composition 强制 Merged（合并语义由入口统一，部件 ctx 声明不生效）
        let (rules, provenance) =
            load_one(&part.doc, &part.ctx, Composition::Merged).map_err(|e| annotate_part(e, i))?;
        outcomes.push(PartOutcome {
            provenance,
            rule_count: rules.len(),
        });
        engine_rules.extend(rules);
    }
    Ok(MergedLoaded {
        engine_rules,
        parts: outcomes,
    })
}

/// 部件序号标注（报错定位：merged 报错必须可指涉到第几个部件）
fn annotate_part(e: ConstitutionError, idx: usize) -> ConstitutionError {
    let prefix = format!("合并装载部件 #{idx}: ");
    match e {
        ConstitutionError::SchemaViolation(mut f) => {
            f.violation.insert_str(0, &prefix);
            ConstitutionError::SchemaViolation(f)
        }
        ConstitutionError::ShellUnrecognized(mut f) => {
            f.violation.insert_str(0, &prefix);
            ConstitutionError::ShellUnrecognized(f)
        }
        ConstitutionError::TierGate(mut f) => {
            f.violation.insert_str(0, &prefix);
            ConstitutionError::TierGate(f)
        }
        ConstitutionError::DisciplineGate(mut f) => {
            f.violation.insert_str(0, &prefix);
            ConstitutionError::DisciplineGate(f)
        }
        ConstitutionError::DomainOnMissingGate(mut f) => {
            f.violation.insert_str(0, &prefix);
            ConstitutionError::DomainOnMissingGate(f)
        }
        ConstitutionError::DimensionRegistryGate(mut f) => {
            f.violation.insert_str(0, &prefix);
            ConstitutionError::DimensionRegistryGate(f)
        }
        other => other,
    }
}

/// 四步主体（[CON-L2-002]：版本协商→schema 校验→剥壳→三门→provenance）。
/// 组合语义由 `composition` 参数决定：单件=ctx.composition 透传；
/// 合并入口强制 Merged。结构性豁免=本 crate 唯一实现点（[CON-L4-001]）。
fn load_one(
    doc: &Value,
    ctx: &LoadContext,
    composition: Composition,
) -> Result<(Vec<JsonValue>, Provenance), ConstitutionError> {
    // ── 步骤 0：$schema 版本协商（[CON-S-004]）──
    // 带壳文档声明 $schema 时，必须命中本 crate 内嵌校验器认识的版本。
    // _meta pattern 只查 URL 形态（任意 v主.次 均过）——若无此门，v2.0 文档会
    // 按内嵌 v1.0 校验器静默通过（D4：版本协商缺位）。允许显式声明 v1.0；
    // 未来版本按此处白名单逐版登记，未登记版本 fail-fast。
    if let Some(schema_uri) = doc.get("$schema").and_then(|s| s.as_str()) {
        const KNOWN_VERSIONS: [&str; 1] = ["https://evorule.org/schemas/rule_set/v1.0.json"];
        if !KNOWN_VERSIONS.contains(&schema_uri) {
            return Err(ConstitutionError::SchemaViolation(four_part(
                &format!(
                    "$schema 版本未协商成功：声明 {schema_uri}，本装载器仅认识 {KNOWN_VERSIONS:?}（v2.0+ 文档须升级装载器后登记）"
                ),
                "$.$schema",
                RULE_SET_URL,
                MINIMAL_RULE_SET,
            )));
        }
    }

    // ── 步骤 1：宪法 schema 校验 ──
    // 分层校验策略（对齐设计分期——阶段一收口装载路径，不强制存量业务文件全壳迁移）：
    // - 裸 transform 数组 → transform_list 校验（数组语义）
    // - 带壳 rule_set（声明 $schema/kind/id/version/metadata）→ rule_set 全文严格校验
    // - 传统部分壳（rules_dir 业务/约束/bundle 条目现役形态：{metadata?,transform:[]}）
    //   → 仅校验 transform 数组（壳外字段仍受 tier 门管辖，不因无壳豁免）
    let report = if doc.is_array() {
        validate_transform_list(doc)
    } else if doc.get("transform").is_some() {
        if has_full_shell(doc) {
            validate_rule_set(doc)
        } else {
            validate_transform_list(doc.get("transform").expect("checked above"))
        }
    } else {
        return Err(ConstitutionError::ShellUnrecognized(four_part(
            "无法识别的规则集形态（既非 {transform:[]} 文档也非裸 transform 数组）",
            "$（文档根）",
            RULE_SET_URL,
            MINIMAL_RULE_SET,
        )));
    };
    if !report.valid {
        return Err(ConstitutionError::SchemaViolation(four_part(
            &format!("宪法 schema 校验失败（{}）", report.errors.join("; ")),
            "$（见各错误内路径）",
            RULE_SET_URL,
            MINIMAL_TRANSFORM_RULE,
        )));
    }

    // ── 步骤 2：剥壳（唯一剥壳点）──
    let transform_arr: Vec<Value> = doc
        .get("transform")
        .and_then(|v| v.as_array())
        .map(|a| a.to_vec())
        .or_else(|| doc.as_array().map(|a| a.to_vec()))
        .ok_or_else(|| {
            ConstitutionError::ShellUnrecognized(four_part(
                "剥壳失败：文档既非 {transform:[]} 也非裸数组",
                "$",
                RULE_SET_URL,
                MINIMAL_RULE_SET,
            ))
        })?;
    if transform_arr.is_empty() {
        return Err(ConstitutionError::SchemaViolation(four_part(
            "transform 数组为空（minItems=1）",
            "$.transform",
            RULE_SET_URL,
            MINIMAL_TRANSFORM_RULE,
        )));
    }

    // ── 步骤 3：门禁检查（tier/discipline/domain on_missing 三门不减 + 量纲登记门；venue 不减免）──
    tier_gate(&transform_arr, doc, ctx)?;
    discipline_gate(&transform_arr, ctx)?;
    domain_on_missing_gate(&transform_arr, ctx.l2_class)?;
    dimension_registry_gate(&transform_arr, doc)?;

    // ── 步骤 4：provenance ──
    // 内容指纹（R4/[CON-S-003]）：对输入文档字节算 blake3（evorule-hash SSOT）。
    // serde_json::Value 键序=BTreeMap（无 preserve_order），紧凑序列化确定性。
    let content_bytes = serde_json::to_vec(doc)
        .map_err(|e| ConstitutionError::ProvenanceIncomplete(format!("输入文档序列化失败: {e}")))?;
    let engine_rules: Vec<JsonValue> = transform_arr.into_iter().map(serde_to_tcb).collect();
    let provenance = Provenance {
        source_kind: source_kind(&ctx.source),
        source: ctx.source.clone(),
        venue: ctx.venue,
        composition,
        l2_class: ctx.l2_class,
        content_hash: evorule_hash::prefixed(&evorule_hash::digest(&content_bytes)),
        gates: GatesPassed {
            tier_gate: true,
            discipline_gate: true,
            domain_on_missing_gate: true,
            dimension_registry_gate: true,
        },
        constitution_version: CONSTITUTION_SCHEMA_VERSION,
        loaded_at_unix: now_unix(),
    };
    Ok((engine_rules, provenance))
}

/// 来源类别标签（provenance 留痕；完整 LoadSource 详情由调用方入审计日志）
fn source_kind(src: &LoadSource) -> &'static str {
    match src {
        LoadSource::File { .. } => "file",
        LoadSource::DbDraft { .. } => "db_draft",
        LoadSource::Api { .. } => "api",
    }
}

// ============================================================================
// 门实现（每个拒绝带四段式）
// ============================================================================

const RULE_SET_URL: &str = "https://evorule.org/schemas/rule_set/v1.0.json";
const MINIMAL_RULE_SET: &str = r#"{"$schema":"https://evorule.org/schemas/rule_set/v1.0.json","kind":"rule_set","id":"org.scope.name","version":"0.1.0","metadata":{"title":"…"},"transform":[{"type":"set","params":{"attr":"x","operation":"set","value":1}}]}"#;
const MINIMAL_TRANSFORM_RULE: &str =
    r#"{"type":"set","params":{"attr":"x","operation":"set","value":1}}"#;

fn four_part(
    violation: &str,
    location: &str,
    reference: &str,
    minimal_example: &str,
) -> FourPartError {
    FourPartError {
        violation: violation.to_string(),
        location: location.to_string(),
        reference: reference.to_string(),
        minimal_example: minimal_example.to_string(),
    }
}

/// 全壳判定：文档声明 rule_set v1.0 壳字段（$schema/kind/id/version/metadata 均在）
/// → 按 rule_set 全文严格校验；否则（传统部分壳 {metadata?,transform:[]}）只校验
/// transform 数组（阶段一分期：不强制存量业务文件全壳迁移，壳外字段仍受 tier 门管辖）。
fn has_full_shell(doc: &Value) -> bool {
    ["$schema", "kind", "id", "version", "metadata"]
        .iter()
        .all(|k| doc.get(*k).is_some())
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// tier 门（正向声明纪律+反向伪造拦截+enforce 限定；server 侧清单视图经
/// 单文件装载门复用本语义，零镜像实现）
///
/// - `L2Class::None`（业务/草稿/API）：禁止 enforce；禁止声明 `metadata.tier ∈ {constraint, meta}`
/// - `L2Class::Constraint`：**必须**声明 `metadata.tier ∈ {constraint, meta}`（防裸文件冒充）
/// - `L2Class::Constitution`：通过（宪法维护者自查）
fn tier_gate(
    transform_arr: &[Value],
    doc: &Value,
    ctx: &LoadContext,
) -> Result<(), ConstitutionError> {
    let tier = doc.pointer("/metadata/tier").and_then(|t| t.as_str());
    match ctx.l2_class {
        L2Class::Constitution => Ok(()),
        L2Class::Constraint => match tier {
            Some("constraint") | Some("meta") => Ok(()),
            other => Err(ConstitutionError::TierGate(four_part(
                &format!(
                    "约束文件缺少 metadata.tier=\"constraint\"（兼容旧值 \"meta\"）（实际 {other:?}）"
                ),
                "$.metadata.tier",
                "https://evorule.org/schemas/rule_set/v1.0.json",
                r#"{"metadata":{"tier":"constraint", …}}"#,
            ))),
        },
        L2Class::None => {
            if matches!(tier, Some("constraint") | Some("meta")) {
                return Err(ConstitutionError::TierGate(four_part(
                    "非元规则文件携带 L2 层级 tier（层级伪造）",
                    "$.metadata.tier",
                    "https://evorule.org/schemas/rule_set/v1.0.json",
                    r#"{"metadata":{"title":"…", …}}（业务文件不声明 tier；L2 约束经治理链晋升）"#,
                )));
            }
            let has_enforce = transform_arr.iter().any(|r| {
                r.get("type").and_then(|t| t.as_str()) == Some("enforce")
            });
            if has_enforce {
                return Err(ConstitutionError::TierGate(four_part(
                    "非 L2 规则集携带 enforce 强制原语（enforce 仅允许 L2 元规则；业务规则/沙盒草稿禁止私自获得引擎级阻止权）",
                    "$.transform[*]（type=enforce 条目）",
                    "https://evorule.org/schemas/_shared/v1.0.json#/$defs/transform_rule",
                    r#"{"type":"set","params":{"attr":"x","operation":"set","value":1}}（业务规则请用 set/push/branch/io_request；enforce 需经治理链晋升）"#,
                )));
            }
            Ok(())
        }
    }
}

/// discipline 门：DC-01..09 形态纪律（evorule-discipline crate）
///
/// enforce 位随 LoadContext（生产=env EVORULE_DISCIPLINE_GATE 单一事实源；
/// warn 期=W1 纯观测放行+违规面日志留痕，enforce 期=拒载）
fn discipline_gate(transform_arr: &[Value], ctx: &LoadContext) -> Result<(), ConstitutionError> {
    let tcb_rules: Vec<JsonValue> = transform_arr
        .iter()
        .map(|r| serde_to_tcb(r.clone()))
        .collect();
    match evorule_discipline::check(&tcb_rules) {
        Ok(report) if report.violations.is_empty() => Ok(()),
        Ok(report) => {
            let codes: Vec<String> = report.violations.iter().map(|v| v.code.clone()).collect();
            if !ctx.discipline_enforce {
                // 显式 warn（回退口）：放行+结构化留痕（缺省已收口为 enforce，SR-16）
                tracing::warn!(
                    target: "discipline_gate",
                    codes = %codes.join(","),
                    "discipline violations observed (warn mode, loaded as-is)"
                );
                return Ok(());
            }
            Err(ConstitutionError::DisciplineGate(four_part(
                &format!("L2 纪律门禁未过（DC 条款：{}）", codes.join(", ")),
                "$.transform（见各违规 path）",
                GUIDE_URL,
                MINIMAL_TRANSFORM_RULE,
            )))
        }
        Err(e) => Err(ConstitutionError::DisciplineGate(four_part(
            &format!("纪律门禁求值失败：{e}"),
            "$.transform",
            GUIDE_URL,
            MINIMAL_TRANSFORM_RULE,
        ))),
    }
}

/// domain on_missing 门：**eq/lt/in 域**三道子门 + in 域第 4 道 values 门（对齐生产实码 server.rs domain_gate_violations）
///
/// 仅对字面 eq/lt/in 域对象生效（对齐 2026-10-01 静默 false 根修方案 2' v4；
/// in 域为 design-19 vendored 增补）：
/// ① missing_declaration：缺 on_missing 显式声明
/// ② invalid_declaration：on_missing 非 error/unsat
/// ③ value_literal_ambiguous：value（eq/lt）或 values 字符串元素（in）像根段点路径
/// ④ values_declaration（仅 in）：values 缺失/非数组/空数组（design-19 R-3 fail-fast）
///
/// 动态域（`__` 字符串引用）静态不可判定，天然不命中；all/has_fields 等非 eq/lt/in
/// 域无 on_missing 语义，不检查——**不是**「全部 domain 都要 on_missing」。
///
/// **管辖面=L2 面**：宪法/约束文件条目行走此门（文件级）。业务文件
/// （L2Class::None）不在此拒——业务条目 domain 纪律由合并层条目级门
/// （`reject_undeclared_domain_rules`，宪法前缀豁免+fail-visible 明细）与
/// bundle 导入预判执行；文件级再拒会双门不同步+吞明细面。
fn domain_on_missing_gate(
    transform_arr: &[Value],
    l2_class: L2Class,
) -> Result<(), ConstitutionError> {
    if l2_class == L2Class::None {
        return Ok(()); // 业务面归合并层条目级门（明细可查），文件级不重复拒
    }
    for (i, rule) in transform_arr.iter().enumerate() {
        let hits = walk_domain_gate_violations(rule);
        if !hits.is_empty() {
            let details: Vec<String> = hits.iter().map(|h| h.detail.clone()).collect();
            return Err(ConstitutionError::DomainOnMissingGate(four_part(
                &format!("eq/lt/in 域声明门禁未过（{}）", details.join("; ")),
                &format!("$.transform[{i}]"),
                "https://evorule.org/schemas/_shared/v1.0.json#/$defs/domain",
                r#"{"type":"eq","path":"__exec__.instruction.params.x","value":1,"on_missing":"error"}"#,
            )));
        }
    }
    Ok(())
}

/// eq/lt/in 域单条门禁违规（门类+明细；门类取值与 server 侧 DomainViolation.door
/// 同一词汇表，双侧 parity 测试逐条全等锁定）
#[derive(Debug, Clone, PartialEq)]
pub struct DomainGateHit {
    /// 违规门类：`missing_declaration`（缺 on_missing 声明）/
    /// `invalid_declaration`（on_missing 非 error/unsat）/
    /// `value_literal_ambiguous`（value/values 元素呈根段点路径形态缺 __ 前缀）/
    /// `values_declaration`（仅 in：values 缺失/非数组/空数组）
    pub door: &'static str,
    /// 违规位置与说明（含 domain.path 与修复指引）
    pub detail: String,
}

/// 递归 walk 整棵规则树，收集 eq/lt/in 域对象的四道门违规（门禁层 API）。
///
/// 与 server 侧 domain_gate_violations 同语义同文案；根段点路径判定谓词
/// 直接调用 TCB `is_root_segment_dot_path` 唯一权威实现（装载期拒收与运行时
/// 归因共用，零镜像复算），双侧 parity 测试逐条全等锁定防漂移。
pub fn walk_domain_gate_violations(v: &Value) -> Vec<DomainGateHit> {
    let mut out = Vec::new();
    walk_eq_lt_domain(v, &mut out);
    out
}

fn walk_eq_lt_domain(v: &Value, out: &mut Vec<DomainGateHit>) {
    match v {
        Value::Array(items) => {
            for it in items {
                walk_eq_lt_domain(it, out);
            }
        }
        Value::Object(m) => {
            let ty = m.get("type").and_then(Value::as_str);
            if matches!(ty, Some("eq") | Some("lt") | Some("in")) {
                let ty = ty.unwrap_or("");
                let path_desc = m
                    .get("path")
                    .and_then(|p| p.as_str())
                    .map(|p| format!("path=\"{p}\""))
                    .unwrap_or_else(|| "path 缺失".to_string());
                match m.get("on_missing") {
                    None => out.push(DomainGateHit {
                        door: "missing_declaration",
                        detail: format!(
                            "{ty} 域 {path_desc} 缺 on_missing 显式声明（error=Missing 拒执行 / unsat=Missing 走 on_false+归因）"
                        ),
                    }),
                    Some(d) => {
                        if !matches!(d.as_str(), Some("error") | Some("unsat")) {
                            out.push(DomainGateHit {
                                door: "invalid_declaration",
                                detail: format!(
                                    "{ty} 域 {path_desc} on_missing 声明值非法（仅允许 error/unsat）"
                                ),
                            });
                        }
                    }
                }
                // ③ value_literal_ambiguous：value 呈根段点路径形态（缺 __ 前缀）
                if let Some(val) = m.get("value").and_then(|x| x.as_str()) {
                    if evorule_tcb::domain::is_root_segment_dot_path(val) {
                        out.push(DomainGateHit {
                            door: "value_literal_ambiguous",
                            detail: format!(
                                "{ty} 域 {path_desc} value=\"{val}\" 呈 exec 根段点路径形态但缺 __ 前缀（ValueLiteralAmbiguous 写作错误，运行时将被字面量化静默比对）"
                            ),
                        });
                    }
                }
                // ④ values_declaration（仅 in）：values 缺失/非数组/空数组
                if ty == "in" {
                    let values_arr = m.get("values").and_then(|v| v.as_array());
                    if m.get("values").is_none() {
                        out.push(DomainGateHit {
                            door: "values_declaration",
                            detail: format!(
                                "in 域 {path_desc} 缺 values 显式声明（集合成员判定必备）"
                            ),
                        });
                    } else if values_arr.is_none() {
                        out.push(DomainGateHit {
                            door: "values_declaration",
                            detail: format!(
                                "in 域 {path_desc} values 非数组（集合成员判定要求字面量数组）"
                            ),
                        });
                    } else if values_arr.is_some_and(|a| a.is_empty()) {
                        out.push(DomainGateHit {
                            door: "values_declaration",
                            detail: format!(
                                "in 域 {path_desc} values 为空数组（空集恒假，大概率写作错误——fail-fast 拒载）"
                            ),
                        });
                    }
                    if let Some(arr) = values_arr {
                        for el in arr.iter() {
                            if let Some(s) = el.as_str() {
                                if evorule_tcb::domain::is_root_segment_dot_path(s) {
                                    out.push(DomainGateHit {
                                        door: "value_literal_ambiguous",
                                        detail: format!(
                                            "in 域 {path_desc} values 元素 \"{s}\" 呈 exec 根段点路径形态但缺 __ 前缀（ValueLiteralAmbiguous 写作错误，运行时将被字面量化静默比对）"
                                        ),
                                    });
                                }
                            }
                        }
                    }
                }
            }
            // 递归所有子节点（嵌套 on_true/on_false/instructions/inner 等）
            for (_k, child) in m {
                walk_eq_lt_domain(child, out);
            }
        }
        _ => {}
    }
}

// ============================================================================
// 量纲登记门（立法计算裁决指令的装载期资格证明）
// ============================================================================

/// 立法计算裁决指令清单（裁决量的量纲语义由立法固定、可登记的指令类型；
/// 随立法逐版登记，与 $schema 版本白名单同构——装载器只认识它认识的立法）。
///
/// band_check 不入清单：其裁决量（value/lo/hi）由调用方场景携带，量纲随场景
/// 而定，无固定量纲可登记；conservation_check 的 drift_fp 由立法固定为无量纲
/// 相对漂移比（定点 Integer 直传），可登记；CAD 判据七指令（clearance/safety_
/// factor/stress/mass_budget/thermal_growth/tolerance_stack/interference）的
/// 裁决量由立法固定量纲向量（间隙裕度/热膨胀偏差/公差链累积 = 长度、安全系数
/// = 无量纲、应力裕量 = Pa、质量余量 = kg、干涉体积 = m³；µm/MPa/g 等为同
/// 量纲族内调用方 scale 约定，不改向量），可登记。
const LEGISLATED_MEASUREMENT_INSTRUCTIONS: &[&str] = &[
    "conservation_check",
    "clearance_check",
    "safety_factor_check",
    "stress_check",
    "mass_budget_check",
    "thermal_growth_check",
    "tolerance_stack_check",
    "interference_check",
];

/// SI 七基本量纲幂次向量长度（[L, M, T, I, Θ, N, J] 序，整数幂次）；
/// 与 inference 族量纲服务出参（七元整数组）同构。
const DIMENSION_VECTOR_LEN: usize = 7;

/// 量纲登记最小正确样例（四段式第④段）
const MINIMAL_DIMENSION_REGISTRY_ENTRY: &str = r#"{"metadata":{"formula_registry":[{"instruction_type":"conservation_check","formula":"|E_final − E_initial| / E_initial","dimensions":[0,0,0,0,0,0,0]}]}}"#;

/// 递归 walk 整棵规则树，收集立法计算裁决指令的 instruction 域引用
/// （`{"type":"instruction","instruction_type":…}` 节点），命中清单者入集
/// （BTreeSet 保序，报错确定性）。
fn walk_legislated_instruction_refs(v: &Value, out: &mut std::collections::BTreeSet<String>) {
    match v {
        Value::Array(items) => {
            for it in items {
                walk_legislated_instruction_refs(it, out);
            }
        }
        Value::Object(m) => {
            if m.get("type").and_then(Value::as_str) == Some("instruction") {
                if let Some(t) = m.get("instruction_type").and_then(Value::as_str) {
                    if LEGISLATED_MEASUREMENT_INSTRUCTIONS.contains(&t) {
                        out.insert(t.to_string());
                    }
                }
            }
            for (_k, child) in m {
                walk_legislated_instruction_refs(child, out);
            }
        }
        _ => {}
    }
}

/// 量纲登记门：transform 引用立法计算裁决指令时，文档必须携带
/// metadata.formula_registry 且对每个被引用指令有结构合法登记。
///
/// # 管辖面（venue 不减免）
///
/// 全 venue 全 L2Class——量纲登记不是治理层级问题，而是「使用立法计算裁决
/// 指令的资格证明」：谁用谁登记，宪法/约束/业务/沙盒草稿/API 提交同守。
/// 裸数组形态无处携带 metadata 登记，引用即拒（fail-closed）。
///
/// # 诚实边界
///
/// 本门是**结构校验**（登记存在、形态合法、覆盖全部引用），不做公式量纲
/// 推导的语义验证——公式右侧是否真具声明量纲属创作期职责（inference 族
/// 公式量纲一致性验证服务）；装载期结构门+创作期语义验证合成完整量纲通路
/// （确定性算力设计稿：装载时公式过门，「公式 → 量纲」登记为规则集的一部分）。
///
/// 登记条目合法形态：instruction_type 非空字符串 / formula 非空字符串 /
/// dimensions 恰好七元素整数数组（[L,M,T,I,Θ,N,J] 序 SI 七基本量纲幂次）；
/// instruction_type 登记唯一；登记集须覆盖全部被引用立法指令。
fn dimension_registry_gate(transform_arr: &[Value], doc: &Value) -> Result<(), ConstitutionError> {
    let mut referenced = std::collections::BTreeSet::new();
    for rule in transform_arr {
        walk_legislated_instruction_refs(rule, &mut referenced);
    }
    if referenced.is_empty() {
        return Ok(()); // 未引用立法计算裁决指令：门无事可做
    }
    let refs_desc: Vec<&str> = referenced.iter().map(String::as_str).collect();
    let Some(Value::Array(entries)) = doc.pointer("/metadata/formula_registry") else {
        return Err(ConstitutionError::DimensionRegistryGate(four_part(
            &format!(
                "规则集引用立法计算裁决指令 [{}]，但缺装载期量纲登记 \
                 （metadata.formula_registry 缺失或非数组）——「公式 → 量纲」登记 \
                 是规则集的一部分，未登记的立法裁决指令拒装载",
                refs_desc.join("、")
            ),
            "$.metadata.formula_registry（缺失）",
            GUIDE_URL,
            MINIMAL_DIMENSION_REGISTRY_ENTRY,
        )));
    };
    let mut registered: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for (i, entry) in entries.iter().enumerate() {
        let loc = format!("$.metadata.formula_registry[{i}]");
        let malformed = |why: String| {
            ConstitutionError::DimensionRegistryGate(four_part(
                &format!("量纲登记条目畸形：{why}"),
                &loc,
                GUIDE_URL,
                MINIMAL_DIMENSION_REGISTRY_ENTRY,
            ))
        };
        let Some(obj) = entry.as_object() else {
            return Err(malformed("条目须为对象".to_string()));
        };
        let Some(it) = obj.get("instruction_type").and_then(Value::as_str) else {
            return Err(malformed("instruction_type 缺失或非字符串".to_string()));
        };
        if it.is_empty() {
            return Err(malformed("instruction_type 为空串".to_string()));
        }
        let Some(f) = obj.get("formula").and_then(Value::as_str) else {
            return Err(malformed("formula 缺失或非字符串".to_string()));
        };
        if f.is_empty() {
            return Err(malformed(
                "formula 为空串（须写明被裁决量的公式）".to_string(),
            ));
        }
        let Some(dims) = obj.get("dimensions").and_then(Value::as_array) else {
            return Err(malformed("dimensions 缺失或非数组".to_string()));
        };
        if dims.len() != DIMENSION_VECTOR_LEN {
            return Err(malformed(format!(
                "dimensions 须为恰好 {DIMENSION_VECTOR_LEN} 元素的 [L,M,T,I,Θ,N,J] 幂次向量（实际 {} 元素）",
                dims.len()
            )));
        }
        for (j, d) in dims.iter().enumerate() {
            if !d.is_i64() {
                return Err(malformed(format!(
                    "dimensions[{j}] 非整数（七分量幂次须为整数）"
                )));
            }
        }
        if registered.contains_key(it) {
            return Err(malformed(format!(
                "instruction_type \"{it}\" 重复登记（登记须唯一）"
            )));
        }
        registered.insert(it, i);
    }
    for t in &referenced {
        if !registered.contains_key(t.as_str()) {
            let keys: Vec<&str> = registered.keys().copied().collect();
            return Err(ConstitutionError::DimensionRegistryGate(four_part(
                &format!(
                    "规则集引用立法计算裁决指令 \"{t}\"，但量纲登记未覆盖 \
                     （已登记 [{}]）——每个被引用的立法裁决指令都必须有登记",
                    keys.join("、")
                ),
                "$.metadata.formula_registry",
                GUIDE_URL,
                MINIMAL_DIMENSION_REGISTRY_ENTRY,
            )));
        }
    }
    Ok(())
}

// ============================================================================
// serde 域 → TCB 域转换（收编 server.rs:4970 / platform_auth.rs:578 双份手写）
// ============================================================================

/// serde_json::Value → evorule_tcb::JsonValue（唯一实现，仓内禁止再手写）
pub fn serde_to_tcb(v: serde_json::Value) -> JsonValue {
    match v {
        serde_json::Value::Null => JsonValue::Null,
        serde_json::Value::Bool(b) => JsonValue::Bool(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                JsonValue::Integer(i)
            } else {
                JsonValue::String(n.to_string().into())
            }
        }
        serde_json::Value::String(s) => JsonValue::String(s.into()),
        serde_json::Value::Array(arr) => {
            JsonValue::Array(arr.into_iter().map(serde_to_tcb).collect())
        }
        serde_json::Value::Object(obj) => {
            let mut map = std::collections::BTreeMap::new();
            for (k, val) in obj {
                map.insert(k, serde_to_tcb(val));
            }
            JsonValue::Object(map)
        }
    }
}

// ============================================================================
// 测试（[CON-S-001] 判据：三分支 source × 两 venue 覆盖）
// ============================================================================

// ============================================================================
// 测试（[CON-S-001] 判据：三分支 source × 两 venue 覆盖）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn valid_rule_set() -> Value {
        json!({
            "$schema": "https://evorule.org/schemas/rule_set/v1.0.json",
            "kind": "rule_set",
            "id": "com.evorule.test.sr01",
            "version": "0.1.0",
            "metadata": { "title": "SR-01 测试" },
            "transform": [
                { "type": "branch", "params": {
                    "domain": { "type": "all", "inner": [], "on_missing": "error" },
                    "on_true": [ { "type": "set", "params": { "attr": "x", "operation": "set", "value": 42 } } ],
                    "on_false": []
                }}
            ]
        })
    }

    #[test]
    fn production_file_constitution_passes() {
        let ctx = LoadContext::constitution("core_eval.json");
        let loaded = load_rule_set(&valid_rule_set(), &ctx).expect("宪法装载应通过");
        assert!(!loaded.engine_rules.is_empty());
        assert_eq!(loaded.provenance.constitution_version, "rule_set/v1.0");
        assert!(loaded.provenance.gates.tier_gate);
        assert_eq!(loaded.provenance.l2_class, L2Class::Constitution);
    }

    #[test]
    fn constraint_file_requires_tier_declaration() {
        // 正向：声明 tier=constraint 的约束文件过
        let mut doc = valid_rule_set();
        doc["metadata"]["tier"] = json!("constraint");
        let ctx = LoadContext::constraint_file("rules/00_constraint_x.json");
        load_rule_set(&doc, &ctx).expect("约束文件声明 tier 应过");

        // 反向 1：裸文件冒充约束文件（缺 tier 声明）拒
        let ctx = LoadContext::constraint_file("rules/00_constraint_bare.json");
        let err = load_rule_set(&valid_rule_set(), &ctx).expect_err("裸文件冒充约束应拒");
        assert!(matches!(err, ConstitutionError::TierGate(_)));

        // 反向 2：业务文件声明 L2 tier=层级伪造拒
        let mut forged = valid_rule_set();
        forged["metadata"]["tier"] = json!("constraint");
        let ctx = LoadContext::business_file("rules/business.json");
        let err = load_rule_set(&forged, &ctx).expect_err("业务文件伪造 tier 应拒");
        assert!(matches!(err, ConstitutionError::TierGate(_)));
        let msg = format!("{err}");
        assert!(msg.contains("层级伪造"));
    }

    #[test]
    fn sandbox_db_draft_passes_three_gates() {
        let ctx = LoadContext::sandbox(LoadSource::DbDraft {
            rule_version_ids: vec!["rv-1".into()],
            ruleset_hash: "abc".into(),
        });
        let loaded = load_rule_set(&valid_rule_set(), &ctx).expect("沙盒草稿（合法）应过三门");
        assert_eq!(loaded.provenance.venue, Venue::Sandbox);
        assert_eq!(loaded.provenance.composition, Composition::Merged);
    }

    #[test]
    fn sandbox_draft_with_enforce_rejected_by_tier_gate() {
        // B2 消解判据：沙盒草稿携 enforce 必被拒
        let doc = json!({
            "$schema": "https://evorule.org/schemas/rule_set/v1.0.json",
            "kind": "rule_set", "id": "x.y.z", "version": "0.1.0",
            "metadata": { "title": "B2 测试" },
            "transform": [
                { "type": "enforce", "params": { "domain": { "type": "all", "inner": [] }, "reason": "test governance bypass" } }
            ]
        });
        let ctx = LoadContext::sandbox(LoadSource::DbDraft {
            rule_version_ids: vec!["rv-2".into()],
            ruleset_hash: "abc".into(),
        });
        let err = load_rule_set(&doc, &ctx).expect_err("enforce 草稿应被 tier 门拒");
        assert!(matches!(err, ConstitutionError::TierGate(_)));
        // 四段式报错可读
        let msg = format!("{err}");
        assert!(msg.contains("enforce 仅允许"));
        assert!(msg.contains("正本"));
        assert!(msg.contains("正确形态"));
    }

    #[test]
    fn constitution_discipline_gate_enforce_rejects_and_warn_allows() {
        // 纪律门归属测试：生产 server 宪法 loader 的 enforce/warn 两态语义已收口
        // 至本 crate（DC-02 违规 = enforce 嵌入 branch 内——引擎级阻止权只允许
        // 顶层治理条目，嵌套即违规），本测试补齐此前缺失的门禁拒绝路径覆盖。
        let doc = json!({
            "$schema": "https://evorule.org/schemas/rule_set/v1.0.json",
            "kind": "rule_set", "id": "x.y.dc", "version": "0.1.0",
            "metadata": { "title": "L2 纪律门测试" },
            "transform": [
                { "type": "branch", "params": {
                    "domain": { "type": "all", "inner": [], "on_missing": "error" },
                    "on_true": [ { "type": "enforce", "params": { "domain": { "type": "all", "inner": [] }, "reason": "governance" } } ],
                    "on_false": []
                }}
            ]
        });

        // enforce 位（缺省，SSOT=discipline_enforce_from）：违规必须拒载，
        // 报错点明 L2 纪律门禁（对齐生产宪法 loader 拒载语义）
        let ctx = LoadContext::constitution("core_eval.json");
        let err = load_rule_set(&doc, &ctx).expect_err("宪法 DC-02 违规在 enforce 位必须拒载");
        assert!(matches!(err, ConstitutionError::DisciplineGate(_)));
        let msg = format!("{err}");
        assert!(
            msg.contains("L2 纪律门禁"),
            "拒载报错应点明 L2 纪律门禁: {msg}"
        );

        // warn 位（显式回退口）：放行+结构化留痕（W1 渐进口径）
        let mut ctx = LoadContext::constitution("core_eval.json");
        ctx.discipline_enforce = false;
        let loaded = load_rule_set(&doc, &ctx).expect("warn 位违规放行+留痕");
        assert!(!loaded.engine_rules.is_empty());
        assert!(loaded.provenance.gates.discipline_gate);
    }

    #[test]
    fn wrapped_without_transform_rejected() {
        let doc = json!({ "foo": 1 });
        let ctx = LoadContext::business_file("x.json");
        let err = load_rule_set(&doc, &ctx).expect_err("无法识别形态应被拒");
        assert!(matches!(err, ConstitutionError::ShellUnrecognized(_)));
    }

    #[test]
    fn bare_array_passes() {
        let doc = json!([
            { "type": "branch", "params": {
                "domain": { "type": "all", "inner": [] },
                "on_true": [ { "type": "set", "params": { "attr": "x", "operation": "set", "value": 1 } } ],
                "on_false": []
            }}
        ]);
        let ctx = LoadContext::api("test");
        let loaded = load_rule_set(&doc, &ctx).expect("裸数组应过");
        assert_eq!(loaded.engine_rules.len(), 1);
    }

    #[test]
    fn domain_missing_on_missing_rejected() {
        let doc = json!({
            "$schema": "https://evorule.org/schemas/rule_set/v1.0.json",
            "kind": "rule_set", "id": "x.y.w", "version": "0.1.0",
            "metadata": { "title": "on_missing 门测试", "tier": "constraint" },
            "transform": [
                { "type": "branch", "params": {
                    "domain": { "type": "eq", "path": "__exec__.instruction.params.x", "value": 1 },
                    "on_true": [], "on_false": []
                }}
            ]
        });
        // L2 面测试：constraint 文件 ctx（管辖面=L2 面；业务面归合并层条目级门）
        let ctx = LoadContext::constraint_file("00_constraint_x.json");
        let err = load_rule_set(&doc, &ctx).expect_err("domain 缺 on_missing 应被拒");
        assert!(matches!(err, ConstitutionError::DomainOnMissingGate(_)));
    }

    #[test]
    fn domain_gate_business_file_deferred_to_merge_layer() {
        // 业务面管辖裁定：业务文件缺 on_missing 不在文件级拒（合并层条目级门
        // + fail-visible 明细管辖），文件级放行——防双门不同步+吞明细面
        let doc = json!({
            "$schema": "https://evorule.org/schemas/rule_set/v1.0.json",
            "kind": "rule_set", "id": "x.y.w2", "version": "0.1.0",
            "metadata": { "title": "业务面归并测试" },
            "transform": [
                { "type": "branch", "params": {
                    "domain": { "type": "eq", "path": "payload.x", "value": 1 },
                    "on_true": [], "on_false": []
                }}
            ]
        });
        let ctx = LoadContext::business_file("x.json");
        let loaded = load_rule_set(&doc, &ctx).expect("业务面文件级应放行（归合并层）");
        assert_eq!(loaded.engine_rules.len(), 1);
    }

    // ===== design-19：in 域声明门禁（第 4 道 values 门 + 元素歧义）=====

    #[test]
    fn in_domain_legal_form_passes() {
        // 合法形态：path + values 字面量数组 + on_missing 显式声明 → 四步全过
        let doc = json!({
            "$schema": "https://evorule.org/schemas/rule_set/v1.0.json",
            "kind": "rule_set", "id": "x.y.in1", "version": "0.1.0",
            "metadata": { "title": "in 域合法形态", "tier": "constraint" },
            "transform": [
                { "type": "branch", "params": {
                    "domain": { "type": "in", "path": "__exec__.payload.model",
                                "values": ["M2.5", "M3"], "on_missing": "unsat" },
                    "on_true": [], "on_false": []
                }}
            ]
        });
        let ctx = LoadContext::constraint_file("00_constraint_in.json");
        let loaded = load_rule_set(&doc, &ctx).expect("in 域合法形态应过四步");
        assert!(loaded.provenance.gates.domain_on_missing_gate);
    }

    #[test]
    fn in_domain_values_element_ambiguous_rejected() {
        // values 字符串元素像根段点路径 → 字面量歧义拒载（门禁层，schema 不查元素语义）
        let doc = json!({
            "$schema": "https://evorule.org/schemas/rule_set/v1.0.json",
            "kind": "rule_set", "id": "x.y.in2", "version": "0.1.0",
            "metadata": { "title": "in 域元素歧义", "tier": "constraint" },
            "transform": [
                { "type": "branch", "params": {
                    "domain": { "type": "in", "path": "__exec__.payload.model",
                                "values": ["M2.5", "payload.x"], "on_missing": "unsat" },
                    "on_true": [], "on_false": []
                }}
            ]
        });
        let ctx = LoadContext::constraint_file("00_constraint_in.json");
        let err = load_rule_set(&doc, &ctx).expect_err("values 元素歧义应拒");
        assert!(matches!(err, ConstitutionError::DomainOnMissingGate(_)));
        let msg = format!("{err}");
        assert!(msg.contains("呈 exec 根段点路径形态"));
        assert!(msg.contains("正确形态"));
    }

    #[test]
    fn in_domain_missing_values_rejected_by_schema() {
        // values 缺失：schema 层（allOf in 分支 required）先拒——walk 门禁为纵深防御
        let doc = json!({
            "$schema": "https://evorule.org/schemas/rule_set/v1.0.json",
            "kind": "rule_set", "id": "x.y.in3", "version": "0.1.0",
            "metadata": { "title": "in 域缺 values", "tier": "constraint" },
            "transform": [
                { "type": "branch", "params": {
                    "domain": { "type": "in", "path": "__exec__.payload.model",
                                "on_missing": "unsat" },
                    "on_true": [], "on_false": []
                }}
            ]
        });
        let ctx = LoadContext::constraint_file("00_constraint_in.json");
        let err = load_rule_set(&doc, &ctx).expect_err("values 缺失应拒");
        assert!(matches!(err, ConstitutionError::SchemaViolation(_)));
    }

    #[test]
    fn in_domain_business_file_deferred_like_eq_lt() {
        // 业务面管辖裁定与 eq/lt 一致：业务文件不在文件级拒（归合并层条目级门）。
        // 注：schema 对业务文件同样生效（values 必备），故这里用「有 values 但
        // 元素歧义」形态验证——schema 过、walk 门禁业务面放行。
        let doc = json!({
            "$schema": "https://evorule.org/schemas/rule_set/v1.0.json",
            "kind": "rule_set", "id": "x.y.in4", "version": "0.1.0",
            "metadata": { "title": "in 域业务面归并" },
            "transform": [
                { "type": "branch", "params": {
                    "domain": { "type": "in", "path": "payload.model",
                                "values": ["M3", "payload.x"], "on_missing": "unsat" },
                    "on_true": [], "on_false": []
                }}
            ]
        });
        let ctx = LoadContext::business_file("x.json");
        let loaded = load_rule_set(&doc, &ctx).expect("业务面文件级应放行（归合并层）");
        assert_eq!(loaded.engine_rules.len(), 1);
    }

    #[test]
    fn in_walk_test_hook_covers_values_gate_details() {
        // 域门禁 walk：in 域第 4 道门三形态 + 元素歧义逐条明细（门类+明细结构化）
        let mk = |domain: Value| {
            json!({ "type": "branch", "params": {
                "domain": domain, "on_true": [], "on_false": []
            }})
        };
        let hits = walk_domain_gate_violations(&json!([
            mk(json!({ "type": "in", "path": "p", "on_missing": "unsat" })),
            mk(json!({ "type": "in", "path": "p", "values": "M3", "on_missing": "unsat" })),
            mk(json!({ "type": "in", "path": "p", "values": [], "on_missing": "unsat" })),
            mk(
                json!({ "type": "in", "path": "p", "values": ["payload.x"], "on_missing": "unsat" })
            ),
            mk(
                json!({ "type": "in", "path": "p", "values": ["M3", "M2.5"], "on_missing": "unsat" })
            ),
        ]));
        assert_eq!(hits.len(), 4, "四违规一合法: {hits:?}");
        assert_eq!(hits[0].door, "values_declaration");
        assert!(hits[0].detail.contains("缺 values 显式声明"));
        assert_eq!(hits[1].door, "values_declaration");
        assert!(hits[1].detail.contains("values 非数组"));
        assert_eq!(hits[2].door, "values_declaration");
        assert!(hits[2].detail.contains("values 为空数组"));
        assert_eq!(hits[3].door, "value_literal_ambiguous");
        assert!(hits[3].detail.contains("呈 exec 根段点路径形态"));
    }

    #[test]
    fn serde_to_tcb_object_uses_btreemap() {
        let v = json!({ "b": 1, "a": 2 });
        let t = serde_to_tcb(v);
        match t {
            JsonValue::Object(m) => {
                assert_eq!(m.len(), 2);
                // BTreeMap 有序：a 在前
                let keys: Vec<&String> = m.keys().collect();
                assert_eq!(keys[0], "a");
            }
            other => panic!("应是 Object，实际 {other:?}"),
        }
    }

    // ===== [CON-S-004] $schema 版本协商（D4 补实现）=====

    #[test]
    fn schema_version_negotiation_rejects_unknown_major() {
        // v2.0 文档（pattern 形态合法）必须被版本协商拒——不得按 v1.0 校验器静默通过
        let mut doc = valid_rule_set();
        doc["$schema"] = json!("https://evorule.org/schemas/rule_set/v2.0.json");
        let ctx = LoadContext::business_file("x.json");
        let err = load_rule_set(&doc, &ctx).expect_err("v2.0 未协商应拒");
        assert!(matches!(err, ConstitutionError::SchemaViolation(_)));
        let msg = format!("{err}");
        assert!(msg.contains("版本未协商"), "报错应说明版本协商失败: {msg}");
    }

    #[test]
    fn schema_version_negotiation_accepts_known_v1() {
        // 显式声明 v1.0（现役）过；不声明 $schema 的传统部分壳也过（分期兼容）
        let mut doc = valid_rule_set();
        doc["$schema"] = json!("https://evorule.org/schemas/rule_set/v1.0.json");
        let ctx = LoadContext::business_file("x.json");
        load_rule_set(&doc, &ctx).expect("v1.0 应协商通过");

        let mut partial = json!({ "transform": valid_rule_set()["transform"].clone() });
        partial["metadata"] = json!({ "title": "传统部分壳" });
        load_rule_set(&partial, &ctx).expect("无 $schema 部分壳应过（分期兼容）");
    }

    // ===== [CON-S-003] 来源指纹收编（D3）=====

    #[test]
    fn provenance_carries_full_source_fingerprint() {
        use serde_json as sj;
        // 文件源：路径指纹
        let ctx = LoadContext::constitution("tcb/core_eval.json");
        let loaded = load_rule_set(&valid_rule_set(), &ctx).unwrap();
        match &loaded.provenance.source {
            LoadSource::File { path } => assert_eq!(path, "tcb/core_eval.json"),
            other => panic!("应是 File 源，实际 {other:?}"),
        }
        // 序列化往返（[CON-S-003] 判据：provenance 可序列化入审计）
        let round = sj::to_value(&loaded.provenance).expect("provenance 应可序列化");
        assert_eq!(round["source"]["kind"], "file");
        assert_eq!(round["source"]["path"], "tcb/core_eval.json");
        assert_eq!(round["source_kind"], "file");

        // DB 草稿源：版本 ID 集+ruleset hash 指纹
        let ctx = LoadContext::sandbox(LoadSource::DbDraft {
            rule_version_ids: vec!["rv-9".into(), "rv-10".into()],
            ruleset_hash: "blake3:abcd".into(),
        });
        let loaded = load_rule_set(&valid_rule_set(), &ctx).unwrap();
        let round = sj::to_value(&loaded.provenance).unwrap();
        assert_eq!(round["source"]["kind"], "db_draft");
        assert_eq!(round["source"]["rule_version_ids"][0], "rv-9");
        assert_eq!(round["source"]["ruleset_hash"], "blake3:abcd");

        // API 源：caller 指纹
        let ctx = LoadContext::api("probe-client");
        let loaded = load_rule_set(&valid_rule_set(), &ctx).unwrap();
        let round = sj::to_value(&loaded.provenance).unwrap();
        assert_eq!(round["source"]["kind"], "api");
        assert_eq!(round["source"]["caller"], "probe-client");

        // 内容指纹（R4）：provenance 可序列化携带 content_hash
        let ctx = LoadContext::constitution("tcb/core_eval.json");
        let loaded = load_rule_set(&valid_rule_set(), &ctx).unwrap();
        let round = sj::to_value(&loaded.provenance).unwrap();
        assert!(
            round["content_hash"]
                .as_str()
                .unwrap_or("")
                .starts_with("blake3:"),
            "content_hash 应带 blake3: 前缀: {}",
            round["content_hash"]
        );
    }

    #[test]
    fn provenance_content_hash_deterministic_and_content_bound() {
        // R4/[CON-S-003] 判据：同文档两次装载 → 同内容指纹（确定性）；
        // 不同文档 → 不同指纹（内容绑定）；路径指纹与内容指纹分工独立。
        let loaded_a1 = load_rule_set(&valid_rule_set(), &LoadContext::business_file("a.json"))
            .expect("装载应通过");
        let loaded_a2 = load_rule_set(&valid_rule_set(), &LoadContext::business_file("b.json"))
            .expect("装载应通过");
        assert!(
            loaded_a1.provenance.content_hash.starts_with("blake3:"),
            "content_hash 应带 blake3: 前缀: {}",
            loaded_a1.provenance.content_hash
        );
        assert_eq!(
            loaded_a1.provenance.content_hash, loaded_a2.provenance.content_hash,
            "同内容不同路径 → 同内容指纹（内容绑定，路径另由 source 承载）"
        );
        // 不同内容 → 不同指纹
        let mut other = valid_rule_set();
        other["metadata"]["title"] = json!("不同内容");
        let loaded_other =
            load_rule_set(&other, &LoadContext::business_file("a.json")).expect("装载应通过");
        assert_ne!(
            loaded_a1.provenance.content_hash, loaded_other.provenance.content_hash,
            "不同内容 → 不同内容指纹"
        );
    }

    // ===== [CON-L2-004]/R7 W1 env fail-closed 判据 =====

    #[test]
    fn discipline_env_value_mapping() {
        // SR-16 收口后：缺省/空串（大小写不敏感）/enforce → true（违规拒载，W1
        // 观测期豁免已收口）；显式 warn（大小写不敏感）→ false（观测期回退口保留）
        assert!(discipline_enforce_from(None));
        assert!(discipline_enforce_from(Some("")));
        assert!(!discipline_enforce_from(Some("warn")));
        assert!(!discipline_enforce_from(Some("WARN")));
        assert!(discipline_enforce_from(Some("enforce")));
        assert!(discipline_enforce_from(Some("ENFORCE")));
    }

    #[test]
    #[should_panic(expected = "EVORULE_DISCIPLINE_GATE 非法值")]
    fn discipline_env_illegal_value_fails_closed() {
        // R7 fail-closed 判据：非法值 panic 拒载（不得静默降级为观测态）
        let _ = discipline_enforce_from(Some("yes"));
    }

    // ===== [CON-S-005] 装载门合并（R5：merged 入口判据）=====

    #[test]
    fn merged_loads_parts_in_order_with_outcomes() {
        // 宪法部件 + 业务部件按传入序拼合；逐部件 outcome（rule_count+provenance）
        let constitution_part = MergedPart {
            doc: valid_rule_set(),
            ctx: LoadContext::constitution("tcb/server_eval.json"),
        };
        let business_part = MergedPart {
            doc: json!([
                { "type": "set", "params": { "attr": "y", "operation": "set", "value": 1 } },
                { "type": "set", "params": { "attr": "z", "operation": "set", "value": 2 } }
            ]),
            ctx: LoadContext::business_file("rules/biz.json"),
        };
        let merged =
            load_rule_set_merged(&[constitution_part, business_part]).expect("合法部件合并应通过");
        // 拼合序 = 传入序（宪法在前）
        assert_eq!(merged.engine_rules.len(), 3, "宪法 1 条+业务 2 条");
        assert_eq!(merged.parts.len(), 2);
        assert_eq!(merged.parts[0].rule_count, 1);
        assert_eq!(merged.parts[1].rule_count, 2);
        // 逐部件 provenance：composition 强制 Merged，l2_class 保留部件自身身份
        assert_eq!(merged.parts[0].provenance.composition, Composition::Merged);
        assert_eq!(merged.parts[1].provenance.composition, Composition::Merged);
        assert_eq!(merged.parts[0].provenance.l2_class, L2Class::Constitution);
        assert_eq!(merged.parts[1].provenance.l2_class, L2Class::None);
        // 内容指纹逐部件可序列化（[CON-S-003]）
        assert!(merged.parts[0]
            .provenance
            .content_hash
            .starts_with("blake3:"));
        assert_ne!(
            merged.parts[0].provenance.content_hash, merged.parts[1].provenance.content_hash,
            "不同部件文档 → 不同内容指纹"
        );
    }

    #[test]
    fn merged_rejects_enforce_draft_part_after_constitution() {
        // [CON-S-005] 判据：草稿携 enforce 在合并后被 tier 门拒——
        // 合并路径不因「宪法部件合法」而放过后续部件的治理旁路
        let constitution_part = MergedPart {
            doc: valid_rule_set(),
            ctx: LoadContext::constitution("tcb/server_eval.json"),
        };
        let enforce_draft = MergedPart {
            doc: json!({
                "$schema": "https://evorule.org/schemas/rule_set/v1.0.json",
                "kind": "rule_set", "id": "x.y.e", "version": "0.1.0",
                "metadata": { "title": "合并路径 enforce 旁路" },
                "transform": [
                    { "type": "enforce", "params": { "domain": { "type": "all", "inner": [] }, "reason": "merge-path bypass" } }
                ]
            }),
            ctx: LoadContext::business_file("rules/evil.json"),
        };
        let err = load_rule_set_merged(&[constitution_part, enforce_draft])
            .expect_err("合并路径 enforce 草稿应被 tier 门拒");
        assert!(matches!(err, ConstitutionError::TierGate(_)));
        let msg = format!("{err}");
        assert!(msg.contains("合并装载部件 #1"), "报错应标注部件序号: {msg}");
        assert!(msg.contains("enforce 仅允许"));
    }

    #[test]
    fn merged_empty_parts_rejected() {
        // minItems=1：空部件清单 fail-fast（空规则集不得经合并通道成立）
        let err = load_rule_set_merged(&[]).expect_err("空部件清单应拒");
        assert!(matches!(err, ConstitutionError::SchemaViolation(_)));
        assert!(format!("{err}").contains("部件清单为空"));
    }

    // ===== 量纲登记门（立法计算裁决指令的装载期资格证明）=====

    /// 构造引用 conservation_check 的最小带壳规则集
    fn conservation_ref_rule_set() -> Value {
        json!({
            "$schema": "https://evorule.org/schemas/rule_set/v1.0.json",
            "kind": "rule_set", "id": "x.y.cons", "version": "0.1.0",
            "metadata": { "title": "守恒裁决引用测试" },
            "transform": [
                { "type": "branch", "params": {
                    "domain": { "type": "instruction", "instruction_type": "conservation_check" },
                    "on_true": [], "on_false": []
                }}
            ]
        })
    }

    #[test]
    fn dimension_registry_valid_entry_passes() {
        // 合法登记（七元整数幂次向量，无量纲相对漂移比）→ 四步全过且 provenance 留痕
        let mut doc = conservation_ref_rule_set();
        doc["metadata"]["formula_registry"] = json!([
            {
                "instruction_type": "conservation_check",
                "formula": "|E_final − E_initial| / E_initial",
                "dimensions": [0, 0, 0, 0, 0, 0, 0]
            }
        ]);
        let ctx = LoadContext::business_file("x.json");
        let loaded = load_rule_set(&doc, &ctx).expect("合法量纲登记应过四步");
        assert!(loaded.provenance.gates.dimension_registry_gate);
    }

    #[test]
    fn dimension_registry_missing_rejected() {
        // 带壳文档缺登记 → 拒（四段式可读）
        let doc = conservation_ref_rule_set();
        let ctx = LoadContext::business_file("x.json");
        let err = load_rule_set(&doc, &ctx).expect_err("引用立法裁决指令缺登记应拒");
        assert!(matches!(err, ConstitutionError::DimensionRegistryGate(_)));
        let msg = format!("{err}");
        assert!(msg.contains("量纲登记"), "报错应点明量纲登记: {msg}");
        assert!(msg.contains("正确形态"));

        // 裸数组形态（无 metadata 可载登记）同拒——venue 不减免，fail-closed
        let bare = doc["transform"].clone();
        let err = load_rule_set(&bare, &LoadContext::api("probe"))
            .expect_err("裸数组引用立法裁决指令应拒（无处登记）");
        assert!(matches!(err, ConstitutionError::DimensionRegistryGate(_)));

        // 不引用立法计算裁决指令的普通文档不受门管辖（无登记亦过）
        let ctx = LoadContext::business_file("x.json");
        load_rule_set(&valid_rule_set(), &ctx).expect("未引用立法裁决指令的文档不受量纲门管辖");
    }

    #[test]
    fn dimension_registry_malformed_dimensions_rejected() {
        let ctx = LoadContext::business_file("x.json");

        // 六元（少一分量）
        let mut doc = conservation_ref_rule_set();
        doc["metadata"]["formula_registry"] = json!([
            { "instruction_type": "conservation_check", "formula": "|ΔE|/E", "dimensions": [0, 0, 0, 0, 0, 0] }
        ]);
        let err = load_rule_set(&doc, &ctx).expect_err("六元 dimensions 应拒");
        assert!(matches!(err, ConstitutionError::DimensionRegistryGate(_)));
        assert!(
            format!("{err}").contains("恰好 7"),
            "报错应说明七元要求: {err}"
        );

        // 含浮点幂次（量纲幂次必须为整数）
        let mut doc = conservation_ref_rule_set();
        doc["metadata"]["formula_registry"] = json!([
            { "instruction_type": "conservation_check", "formula": "|ΔE|/E", "dimensions": [0, 0, 0, 0, 0, 0, 0.5] }
        ]);
        let err = load_rule_set(&doc, &ctx).expect_err("浮点幂次应拒");
        assert!(
            format!("{err}").contains("非整数"),
            "报错应说明整数要求: {err}"
        );
    }

    #[test]
    fn dimension_registry_bad_formula_and_duplicate_rejected() {
        let ctx = LoadContext::business_file("x.json");

        // formula 空串（登记须写明公式——「公式 → 量纲」登记为规则集的一部分）
        let mut doc = conservation_ref_rule_set();
        doc["metadata"]["formula_registry"] = json!([
            { "instruction_type": "conservation_check", "formula": "", "dimensions": [0, 0, 0, 0, 0, 0, 0] }
        ]);
        let err = load_rule_set(&doc, &ctx).expect_err("空 formula 应拒");
        assert!(format!("{err}").contains("formula 为空串"), "实际: {err}");

        // 同一 instruction_type 重复登记（登记须唯一）
        let mut doc = conservation_ref_rule_set();
        doc["metadata"]["formula_registry"] = json!([
            { "instruction_type": "conservation_check", "formula": "a", "dimensions": [0, 0, 0, 0, 0, 0, 0] },
            { "instruction_type": "conservation_check", "formula": "b", "dimensions": [0, 0, 0, 0, 0, 0, 0] }
        ]);
        let err = load_rule_set(&doc, &ctx).expect_err("重复登记应拒");
        assert!(format!("{err}").contains("重复登记"), "实际: {err}");

        // 登记存在但未覆盖被引用指令（登记了别的指令）→ 覆盖性拒
        let mut doc = conservation_ref_rule_set();
        doc["metadata"]["formula_registry"] = json!([
            { "instruction_type": "other_legislated_instruction", "formula": "x", "dimensions": [0, 0, 0, 0, 0, 0, 0] }
        ]);
        let err = load_rule_set(&doc, &ctx).expect_err("登记未覆盖引用应拒");
        let msg = format!("{err}");
        assert!(msg.contains("未覆盖"), "报错应说明覆盖性缺失: {msg}");
        assert!(
            msg.contains("conservation_check"),
            "报错应点名缺失的指令: {msg}"
        );
    }

    #[test]
    fn dimension_registry_cad_family_gate_coverage() {
        // CAD 判据七指令（v0.9.0 立法）同受量纲登记门管辖：
        // 引用任一未登记 → 拒且点名；七件全登记 → 过门。
        let cad_ref = |t: &str| {
            json!({
                "$schema": "https://evorule.org/schemas/rule_set/v1.0.json",
                "kind": "rule_set", "id": "x.y.cad", "version": "0.1.0",
                "metadata": { "title": "CAD 判据登记门测试" },
                "transform": [
                    { "type": "branch", "params": {
                        "domain": { "type": "instruction", "instruction_type": t },
                        "on_true": [], "on_false": []
                    }}
                ]
            })
        };
        let ctx = LoadContext::business_file("x.json");
        for t in [
            "clearance_check",
            "safety_factor_check",
            "stress_check",
            "mass_budget_check",
            "thermal_growth_check",
            "tolerance_stack_check",
            "interference_check",
        ] {
            let err = load_rule_set(&cad_ref(t), &ctx).expect_err("引用 CAD 判据指令缺登记应拒");
            assert!(matches!(err, ConstitutionError::DimensionRegistryGate(_)));
            assert!(
                format!("{err}").contains(t),
                "报错应点名缺失的指令 {t}: {err}"
            );
        }
        // 七件全登记 → 过门（登记集超集于引用集合法，覆盖性只查缺不查多）
        let mut doc = cad_ref("interference_check");
        doc["metadata"]["formula_registry"] = json!([
            { "instruction_type": "clearance_check", "formula": "g_min − d_min", "dimensions": [1, 0, 0, 0, 0, 0, 0] },
            { "instruction_type": "safety_factor_check", "formula": "σ_yield / σ_max", "dimensions": [0, 0, 0, 0, 0, 0, 0] },
            { "instruction_type": "stress_check", "formula": "σ_allow − σ_max", "dimensions": [-1, 1, -2, 0, 0, 0, 0] },
            { "instruction_type": "mass_budget_check", "formula": "W − Σmᵢ", "dimensions": [0, 1, 0, 0, 0, 0, 0] },
            { "instruction_type": "thermal_growth_check", "formula": "|ΔL_meas − α·L·ΔT|", "dimensions": [1, 0, 0, 0, 0, 0, 0] },
            { "instruction_type": "tolerance_stack_check", "formula": "√(ΣTᵢ²)", "dimensions": [1, 0, 0, 0, 0, 0, 0] },
            { "instruction_type": "interference_check", "formula": "V_intersecting", "dimensions": [3, 0, 0, 0, 0, 0, 0] }
        ]);
        let loaded = load_rule_set(&doc, &ctx).expect("CAD 判据全登记应过门");
        assert!(loaded.provenance.gates.dimension_registry_gate);
    }
}

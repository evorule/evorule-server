// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! evorule 宪法装载唯一通道（25 号档 [CON-L2-001]～[CON-L2-006]、[CON-S-001]～[CON-S-006]）
//!
//! # 定位（records/77 线1 防御层收口；22 号档尽调 → 25 号档条文正本）
//!
//! 本 crate 是「规则集进入引擎」的**唯一装载函数** `load_rule_set`：
//! 宪法 schema 校验 → 剥壳 → 三门 → provenance，四步固定顺序、不可拆、不可跳。
//! 生产 loader（server.rs 双 loader）与沙盒 loader（sandbox parse_draft_rules）
//! 一律改调本函数——消灭「第二条装载路径」（B-3 四缺陷 A/B2 的制度根源）。
//!
//! # 四步固定顺序（[CON-L2-002]）
//!
//! 1. **宪法 schema 校验**（`evorule_rule_schema::validate_rule_set` / `validate_transform_list`）
//! 2. **剥壳**（提取 `transform[]`；兼容带壳 rule_set 文档与裸 transform 数组两形态）
//! 3. **三门检查**（[CON-L2-003] 三门不减，venue=Sandbox 同守）：
//!    - tier 门：非 L2 携 enforce 拒（引擎级阻止权仅随治理链晋升的 L2 条目下发）
//!    - discipline 门：DC-01..09 形态纪律（evorule-discipline crate）
//!    - domain on_missing 门：domain 节点缺 on_missing 显式声明拒
//! 4. **provenance**（[CON-S-003]）：来源指纹+三门记录+宪法版本，可序列化入审计
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
    DbDraft { rule_version_ids: Vec<String>, ruleset_hash: String },
    /// API 提交
    Api { caller: String },
}

/// 装载场地（[CON-L2-003]：三门不减——沙盒同守生产纪律）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Venue { Production, Sandbox }

/// 规则集组成语义（Q-1 定型：[CON-L2-006]，P-1 批文=合并）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Composition { Merged, Standalone }

/// 装载上下文
#[derive(Debug, Clone, Serialize)]
pub struct LoadContext {
    pub source: LoadSource,
    pub venue: Venue,
    pub composition: Composition,
}

impl LoadContext {
    /// 沙盒装载上下文（Q-1=合并语义默认）
    pub fn sandbox(source: LoadSource) -> Self {
        LoadContext { source, venue: Venue::Sandbox, composition: Composition::Merged }
    }

    /// 生产装载上下文（宪法/业务规则文件）
    pub fn production(source: LoadSource) -> Self {
        LoadContext { source, venue: Venue::Production, composition: Composition::Standalone }
    }
}

/// 三门通过记录（provenance 组成部分）
#[derive(Debug, Clone, Serialize)]
pub struct GatesPassed {
    pub tier_gate: bool,
    pub discipline_gate: bool,
    pub domain_on_missing_gate: bool,
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
    pub source: LoadSource,
    pub venue: Venue,
    pub composition: Composition,
    pub gates: GatesPassed,
    pub constitution_version: String,
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
    ProvenanceIncomplete(String),
}

impl std::fmt::Display for ConstitutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConstitutionError::SchemaViolation(e)
            | ConstitutionError::ShellUnrecognized(e)
            | ConstitutionError::TierGate(e)
            | ConstitutionError::DisciplineGate(e)
            | ConstitutionError::DomainOnMissingGate(e) => write!(f, "{e}"),
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
/// O-431 后沙盒裁决铸 `sandbox:` 背书=治理行为；铸造信任者必须自守治理纪律，
/// 沙盒草稿与生产规则同守三门。
pub fn load_rule_set(doc: &Value, ctx: &LoadContext) -> Result<LoadedRuleset, ConstitutionError> {
    // ── 步骤 1：宪法 schema 校验 ──
    let report = if doc.get("transform").is_some() {
        validate_rule_set(doc)
    } else if doc.is_array() {
        validate_transform_list(doc)
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

    // ── 步骤 3：三门检查（venue 不减免）──
    tier_gate(&transform_arr, ctx)?;
    discipline_gate(&transform_arr)?;
    domain_on_missing_gate(&transform_arr)?;

    // ── 步骤 4：provenance ──
    let engine_rules: Vec<JsonValue> = transform_arr.into_iter().map(serde_to_tcb).collect();
    Ok(LoadedRuleset {
        engine_rules,
        provenance: Provenance {
            source: ctx.source.clone(),
            venue: ctx.venue,
            composition: ctx.composition,
            gates: GatesPassed { tier_gate: true, discipline_gate: true, domain_on_missing_gate: true },
            constitution_version: CONSTITUTION_SCHEMA_VERSION.to_string(),
            loaded_at_unix: now_unix(),
        },
    })
}

// ============================================================================
// 门实现（每个拒绝带四段式）
// ============================================================================

const RULE_SET_URL: &str = "https://evorule.org/schemas/rule_set/v1.0.json";
const MINIMAL_RULE_SET: &str = r#"{"$schema":"https://evorule.org/schemas/rule_set/v1.0.json","kind":"rule_set","id":"org.scope.name","version":"0.1.0","metadata":{"title":"…"},"transform":[{"type":"set","params":{"attr":"x","operation":"set","value":1}}]}"#;
const MINIMAL_TRANSFORM_RULE: &str = r#"{"type":"set","params":{"attr":"x","operation":"set","value":1}}"#;

fn four_part(violation: &str, location: &str, reference: &str, minimal_example: &str) -> FourPartError {
    FourPartError {
        violation: violation.to_string(),
        location: location.to_string(),
        reference: reference.to_string(),
        minimal_example: minimal_example.to_string(),
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// tier 门：非 L2 携 enforce 拒（B2 消解）。
///
/// L2 身份判定抽象为数据：File 来源+Standalone 组成=宪法/元规则文件面（治理链正本，
/// 生产装载的 core_eval.json / 00_constraint_*.json 走此分支）；DB 草稿/API 提交
/// 一律非 L2——沙盒草稿携 enforce=治理旁路，直接拒。
fn tier_gate(transform_arr: &[Value], ctx: &LoadContext) -> Result<(), ConstitutionError> {
    let is_constitution = matches!(&ctx.source, LoadSource::File { .. })
        && ctx.composition == Composition::Standalone;
    if is_constitution {
        return Ok(());
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

/// discipline 门：DC-01..09 形态纪律（evorule-discipline crate）
fn discipline_gate(transform_arr: &[Value]) -> Result<(), ConstitutionError> {
    let tcb_rules: Vec<JsonValue> =
        transform_arr.iter().map(|r| serde_to_tcb(r.clone())).collect();
    match evorule_discipline::check(&tcb_rules) {
        Ok(report) if report.violations.is_empty() => Ok(()),
        Ok(report) => {
            let codes: Vec<String> =
                report.violations.iter().map(|v| v.code.clone()).collect();
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

/// domain on_missing 门：**eq/lt 域**三道子门（对齐生产实码 server.rs:1590-1802）
///
/// 仅对字面 eq/lt 域对象生效（对齐 2026-10-01 静默 false 根修方案 2' v4）：
/// ① missing_declaration：缺 on_missing 显式声明
/// ② invalid_declaration：on_missing 非 error/unsat
/// ③ value_literal_ambiguous：value 字符串像根段点路径（历史写作错误形态）
///
/// 动态域（`__` 字符串引用）静态不可判定，天然不命中；all/has_fields 等非 eq/lt
/// 域无 on_missing 语义，不检查——**不是**「全部 domain 都要 on_missing」。
fn domain_on_missing_gate(transform_arr: &[Value]) -> Result<(), ConstitutionError> {
    for (i, rule) in transform_arr.iter().enumerate() {
        let mut violations: Vec<String> = Vec::new();
        walk_eq_lt_domain(rule, &mut violations);
        if !violations.is_empty() {
            return Err(ConstitutionError::DomainOnMissingGate(four_part(
                &format!("eq/lt 域声明门禁未过（{}）", violations.join("; ")),
                &format!("$.transform[{i}]"),
                "https://evorule.org/schemas/_shared/v1.0.json#/$defs/domain",
                r#"{"type":"eq","path":"__exec__.instruction.params.x","value":1,"on_missing":"error"}"#,
            )));
        }
    }
    Ok(())
}

/// 递归 walk 整棵规则树，收集 eq/lt 域对象的三道子门违规（对齐 server.rs domain_gate_violations）
fn walk_eq_lt_domain(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Array(items) => {
            for it in items {
                walk_eq_lt_domain(it, out);
            }
        }
        Value::Object(m) => {
            let ty = m.get("type").and_then(|t| t.as_str());
            if matches!(ty, Some("eq") | Some("lt")) {
                let ty = ty.unwrap_or("");
                let path_desc = m
                    .get("path")
                    .and_then(|p| p.as_str())
                    .map(|p| format!("path=\"{p}\""))
                    .unwrap_or_else(|| "path 缺失".to_string());
                match m.get("on_missing") {
                    None => out.push(format!(
                        "{ty} 域 {path_desc} 缺 on_missing 显式声明（error=Missing 拒执行 / unsat=Missing 走 on_false+归因）"
                    )),
                    Some(d) => {
                        if !matches!(d.as_str(), Some("error") | Some("unsat")) {
                            out.push(format!(
                                "{ty} 域 {path_desc} on_missing 声明值非法（仅允许 error/unsat）"
                            ));
                        }
                    }
                }
                // ③ value_literal_ambiguous：value 像根段点路径（如 "payload.x" 无 __ 前缀）
                if let Some(val) = m.get("value").and_then(|x| x.as_str()) {
                    if is_root_segment_dot_path_like(val) {
                        out.push(format!(
                            "{ty} 域 {path_desc} value 字面量歧义（\"{val}\" 像路径引用但缺 __ 前缀——若引用路径请写全，若字面量请重命名）"
                        ));
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

/// 「像根段点路径」的保守近似判定（生产实码用 TCB is_root_segment_dot_path；
/// 本 crate 走 serde 域，用同语义轻量实现：首段为 exec 已知根段+含 . 的字符串）
fn is_root_segment_dot_path_like(val: &str) -> bool {
    const ROOTS: [&str; 4] = ["instruction", "payload", "queue", "__exec__"];
    if val.contains("__") {
        return false; // 有 __ 前缀=显式路径引用，合法
    }
    let first = val.split('.').next().unwrap_or("");
    ROOTS.contains(&first) && val.contains('.')
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
        let ctx = LoadContext::production(LoadSource::File { path: "core_eval.json".into() });
        let loaded = load_rule_set(&valid_rule_set(), &ctx).expect("宪法装载应通过");
        assert!(!loaded.engine_rules.is_empty());
        assert_eq!(loaded.provenance.constitution_version, "rule_set/v1.0");
        assert!(loaded.provenance.gates.tier_gate);
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
    fn wrapped_without_transform_rejected() {
        let doc = json!({ "foo": 1 });
        let ctx = LoadContext::production(LoadSource::File { path: "x.json".into() });
        let err = load_rule_set(&doc, &ctx).expect_err("无法识别形态应被拒");
        assert!(matches!(err, ConstitutionError::ShellUnrecognized(_)));
    }

    #[test]
    fn bare_array_passes() {
        let doc = json!([
            { "type": "branch", "params": {
                "domain": { "type": "all", "inner": [], "on_missing": "error" },
                "on_true": [ { "type": "set", "params": { "attr": "x", "operation": "set", "value": 1 } } ],
                "on_false": []
            }}
        ]);
        let ctx = LoadContext::sandbox(LoadSource::Api { caller: "test".into() });
        let loaded = load_rule_set(&doc, &ctx).expect("裸数组应过");
        assert_eq!(loaded.engine_rules.len(), 1);
    }

    #[test]
    fn domain_missing_on_missing_rejected() {
        let doc = json!({
            "$schema": "https://evorule.org/schemas/rule_set/v1.0.json",
            "kind": "rule_set", "id": "x.y.w", "version": "0.1.0",
            "metadata": { "title": "on_missing 门测试" },
            "transform": [
                { "type": "branch", "params": {
                    "domain": { "type": "eq", "path": "__exec__.instruction.params.x", "value": 1 },
                    "on_true": [], "on_false": []
                }}
            ]
        });
        let ctx = LoadContext::production(LoadSource::File { path: "x.json".into() });
        let err = load_rule_set(&doc, &ctx).expect_err("domain 缺 on_missing 应被拒");
        assert!(matches!(err, ConstitutionError::DomainOnMissingGate(_)));
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
}

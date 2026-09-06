// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 规则 Schema 门禁 —— 基于固化的 evorule-system-rules v1.0 Schema 的确定性校验
//!
//! # 定位（records/77 边界与防御原则）
//! - TCB（evorule-tcb）不设防：只保证确定性执行，不保证用户规则正确性。
//! - **本 crate 是 evorule-server 侧的防御层**：在任何规则进入引擎前，
//!   用固化 Schema 拦截结构非法的规则并给出明确提示（线1）。
//! - Schema 源文件：`schemas/` 下三个文件（rule_set / _meta / _shared），
//!   与 evorule-system-rules 仓保持同步（跨仓一致性由 scripts/check_schema_sync.py 守护）。
//!
//! # 两种校验模式
//! - [validate_rule_set]：完整 rule_set 文档（5 标注字段 + transform[]），
//!   对应 rule_translate 输出 / hot_reload 加载 / API 提交的完整规则文件。
//! - [validate_transform_list]：裸 transform 数组（引擎 native 结构），
//!   对应 server 从规则文件抽取 transform 数组 / 裸数组入参。
//!
//! # 正确性保证
//! - 元指令/域类型/必填参数等全部由 Schema 表达（SSOT），不维护手写常量表。
//! - 构建期由 build.rs 校验三文件合法性 + $id 自洽；运行期缓存校验器。
//! - 真实文件合规性由 evorule-system-rules `_verify_schemas.py` 闭环验证。

#![forbid(unsafe_code)]
// C5 (unwrap/expect/panic = deny) 仅约束生产代码;测试代码保留 unwrap 惯例
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use jsonschema::{Draft, Retrieve, UriRef, Validator};
use once_cell::sync::Lazy;
use serde::Serialize;
use serde_json::Value;

// ============================================================================
// 内嵌 Schema（SSOT：与 evorule-system-rules/schemas 同步）
// ============================================================================

const RULE_SET_JSON: &str = include_str!("../schemas/rule_set/v1.0.json");
const META_JSON: &str = include_str!("../schemas/_meta/v1.0.json");
const SHARED_JSON: &str = include_str!("../schemas/_shared/v1.0.json");
const KNOWLEDGE_JSON: &str = include_str!("../schemas/knowledge/v1.0.json");

const RULE_SET_ID: &str = "https://evorule.org/schemas/rule_set/v1.0.json";
const META_ID: &str = "https://evorule.org/schemas/_meta/v1.0.json";
const SHARED_ID: &str = "https://evorule.org/schemas/_shared/v1.0.json";
const KNOWLEDGE_ID: &str = "https://evorule.org/schemas/knowledge/v1.0.json";

/// 最大 transform 规则数（与 TCB MAX_TRANSFORM_RULES 一致；schema maxItems 亦约束）
const MAX_TRANSFORM_RULES: usize = 64;

// ============================================================================
// 校验结果
// ============================================================================

/// 校验报告：valid + 结构化错误列表（含实例路径），供上层显示明确提示
#[derive(Debug, Clone, Serialize)]
pub struct SchemaReport {
    /// 是否通过
    pub valid: bool,
    /// 校验模式：rule_set / transform_list
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
            RULE_SET_ID => RULE_SET_JSON,
            META_ID => META_JSON,
            SHARED_ID => SHARED_JSON,
            KNOWLEDGE_ID => KNOWLEDGE_JSON,
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
    rule_set: Validator,
    transform_list: Validator,
    instruction: Validator,
    service_registry: Validator,
    knowledge: Validator,
}

// build.rs 已在构建期保证三个 schema 合法且 $id 自洽，故此处的
// expect 是「静态输入必然成功」的不变量（非运行时错误路径）。
#[allow(clippy::expect_used)]
fn build_validators() -> Validators {
    let rule_set: Value =
        serde_json::from_str(RULE_SET_JSON).expect("内嵌 rule_set schema 非法（build.rs 已保证）");
    let knowledge_schema: Value = serde_json::from_str(KNOWLEDGE_JSON)
        .expect("内嵌 knowledge schema 非法（build.rs 已保证）");
    let transform_list: Value = serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://evorule.org/schemas/rule_set/_transform_list.json",
        "title": "裸 transform 数组门禁（引擎 native 结构）",
        "type": "array",
        "items": { "$ref": "https://evorule.org/schemas/_shared/v1.0.json#/$defs/transform_rule" },
        "maxItems": 64,
        "minItems": 1
    });

    let mut rs_opts = Validator::options();
    rs_opts.with_draft(Draft::Draft202012);
    rs_opts.with_retriever(EmbeddedRetriever);
    let rule_set_v = rs_opts
        .build(&rule_set)
        .expect("构建 rule_set 校验器失败（build.rs 已保证 schema 合法）");

    let mut tl_opts = Validator::options();
    tl_opts.with_draft(Draft::Draft202012);
    tl_opts.with_retriever(EmbeddedRetriever);
    let transform_list_v = tl_opts
        .build(&transform_list)
        .expect("构建 transform_list 校验器失败（build.rs 已保证 schema 合法）");

    // 指令层校验器（submit_command / session_command 入口，Opt3）。
    // $ref 到 _shared 的 instruction $defs：控制流结构（set.attr 路径 /
    // sequence.instructions / conditional/while_loop 的 domain）由该 $defs 表达。
    let instruction_schema: Value = serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://evorule.org/schemas/rule_set/_instruction.json",
        "title": "单条指令门禁（指令层结构）",
        "$ref": "https://evorule.org/schemas/_shared/v1.0.json#/$defs/instruction"
    });
    let mut in_opts = Validator::options();
    in_opts.with_draft(Draft::Draft202012);
    in_opts.with_retriever(EmbeddedRetriever);
    let instruction_v = in_opts
        .build(&instruction_schema)
        .expect("构建 instruction 校验器失败（build.rs 已保证 schema 合法）");

    // service_registry 门禁（C9）：顶层 object，每个 value $ref _shared 的 service_entry
    // $defs（权威源 io_handlers/service_registry.rs ServiceEntry）。加载期拦截结构非法条目，
    // 与 parse_service_entry 的语义校验（scheme 白名单等）形成双层防御。
    let service_registry_schema: Value = serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://evorule.org/schemas/rule_set/_service_registry.json",
        "title": "service_registry.json 门禁（服务注册表顶层 object）",
        "type": "object",
        "additionalProperties": {
            "$ref": "https://evorule.org/schemas/_shared/v1.0.json#/$defs/service_entry"
        }
    });
    let mut sr_opts = Validator::options();
    sr_opts.with_draft(Draft::Draft202012);
    sr_opts.with_retriever(EmbeddedRetriever);
    let service_registry_v = sr_opts
        .build(&service_registry_schema)
        .expect("构建 service_registry 校验器失败（内嵌 schema 静态合法）");

    // knowledge 门禁（）：知识数据资产文档完整校验——双形态条目
    // （文档 content / 数据 payload+schema_ref）oneOf 互斥 + _meta 治理骨架。
    let mut kn_opts = Validator::options();
    kn_opts.with_draft(Draft::Draft202012);
    kn_opts.with_retriever(EmbeddedRetriever);
    let knowledge_v = kn_opts
        .build(&knowledge_schema)
        .expect("构建 knowledge 校验器失败（build.rs 已保证 schema 合法）");

    Validators {
        rule_set: rule_set_v,
        transform_list: transform_list_v,
        instruction: instruction_v,
        service_registry: service_registry_v,
        knowledge: knowledge_v,
    }
}

static VALIDATORS: Lazy<Validators> = Lazy::new(build_validators);

// ============================================================================
// 公共 API
// ============================================================================

/// 校验完整 rule_set 文档（5 标注字段 + transform[]）。
///
/// 用于：rule_translate 输出、hot_reload 加载、API 提交的完整规则文件。
pub fn validate_rule_set(doc: &Value) -> SchemaReport {
    collect_report(&VALIDATORS.rule_set, doc, "rule_set")
}

/// 校验裸 transform 数组（引擎 native 结构）。
///
/// 用于：server 从规则文件抽取 transform 数组 / 顶层数组入参。
/// 非数组或空数组视为非法（与 TCB 非空约束一致）。
pub fn validate_transform_list(transforms: &Value) -> SchemaReport {
    collect_report(&VALIDATORS.transform_list, transforms, "transform_list")
}

/// 从任意 rule 入参提取 transform 数组并校验：
/// - `{..., "transform": [...]}` → 校验 transform 数组
/// - `[...]` 顶层数组 → 校验该数组
/// - 其他 → 非法（无法识别）
pub fn validate_rule_input(input: &Value) -> SchemaReport {
    if let Some(arr) = input.get("transform").and_then(|v| v.as_array()) {
        validate_transform_list(&serde_json::Value::Array(arr.clone()))
    } else if input.is_array() {
        validate_transform_list(input)
    } else {
        SchemaReport::fail(
            "rule_input",
            vec!["无法识别：既不是 {transform:[]} 文档也不是 transform 数组".to_string()],
        )
    }
}

/// 校验 submit_command / session_command 提交的单条指令（线1 防御层，records/77，Opt3）。
///
/// 双层语言（records/75）分派：
/// - **元指令层类型**（set/push/branch/io_request/collect/merge）→ 按 `transform_rule` 严格
///   递归校验（含 domain 结构、path 语法、`__io_results__` 复数强制）——demos 08/012/013/014/016
///   的 `branch` 工作流走此路径，单数 `__io_result__` / 非法元指令在此被拦截。
/// - **指令层类型**（sequence/conditional/while_loop/noop/set 及业务类型）→ 按 `instruction` $defs
///   校验控制流结构（set.attr 路径 / sequence.instructions 数组 / conditional/while_loop 的 domain）。
///
/// 目的：把"提交照常、运行时才 PathResolutionFailed"提前到提交期即明确报错。
pub fn validate_command_instruction(instr: &Value) -> SchemaReport {
    if !instr.is_object() {
        return SchemaReport::fail(
            "command_instruction",
            vec!["指令必须是 JSON 对象（含 type 字段）".to_string()],
        );
    }
    let ty = instr.get("type").and_then(|v| v.as_str()).unwrap_or("");
    const META: &[&str] = &["set", "push", "branch", "io_request", "collect", "merge"];
    if META.contains(&ty) {
        validate_transform_list(&serde_json::Value::Array(vec![instr.clone()]))
    } else {
        collect_report(&VALIDATORS.instruction, instr, "command_instruction")
    }
}

/// 校验 service_registry.json 全文（C9：服务注册表加载期 schema 门禁）。
///
/// 顶层必须是 JSON object，每个条目按 `_shared` 的 `service_entry` $defs 校验
/// （url 必填、headers 值必须字符串、timeout_ms 非负整数等；未知字段开放以向前兼容）。
/// 与 io_handlers `parse_service_entry` 的语义校验（scheme 白名单等）形成双层防御。
pub fn validate_service_registry(doc: &Value) -> SchemaReport {
    collect_report(&VALIDATORS.service_registry, doc, "service_registry")
}

/// 校验知识数据资产文档（，knowledge/v1.0 完整校验）。
///
/// 双形态条目强制互斥：文档条目（content）与数据条目（payload + schema_ref）
/// 不得混于同一条目；治理骨架（kind=knowledge + _meta）与 rule_set 同源。
/// 用于：knowledge bundle 导入前的条目文档校验、治理侧知识数据集文档校验。
pub fn validate_knowledge(doc: &Value) -> SchemaReport {
    collect_report(&VALIDATORS.knowledge, doc, "knowledge")
}

fn collect_report(validator: &Validator, instance: &Value, mode: &'static str) -> SchemaReport {
    let mut errors: Vec<String> = match validator.validate(instance) {
        Ok(()) => Vec::new(),
        Err(iter) => iter
            .map(|e| format!("{}: {}", e.instance_path, e))
            .collect(),
    };
    // 引擎约束补充（schema 无法表达的边界，与 TCB 常量同源，SSOT 见 build.rs 注释）
    if mode == "transform_list" {
        if let Some(arr) = instance.as_array() {
            if arr.len() > MAX_TRANSFORM_RULES {
                errors.push(format!(
                    "transform 数量 {} 超过引擎上限 {}",
                    arr.len(),
                    MAX_TRANSFORM_RULES
                ));
            }
        }
    }
    if errors.is_empty() {
        SchemaReport::ok(mode)
    } else {
        SchemaReport::fail(mode, errors)
    }
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn rs(transform: Value) -> Value {
        serde_json::json!({
            "$schema": "https://evorule.org/schemas/rule_set/v1.0.json",
            "kind": "rule_set",
            "id": "com.evorule.test.x",
            "version": "0.1.0",
            "metadata": { "title": "测试" },
            "transform": transform
        })
    }

    #[test]
    fn valid_rule_set_passes() {
        let doc = rs(serde_json::json!([
            { "type": "branch", "params": {
                "domain": { "type": "all", "inner": [] },
                "on_true": [ { "type": "set", "params": { "attr": "x", "operation": "set", "value": 42 } } ]
            } }
        ]));
        let report = validate_rule_set(&doc);
        assert!(report.valid, "合法 rule_set 应通过: {:?}", report.errors);
    }

    #[test]
    fn set_missing_value_rejected() {
        // 引擎 exec_set 对缺失 value 报 MissingField
        let doc = rs(serde_json::json!([
            { "type": "set", "params": { "attr": "x", "operation": "set" } }
        ]));
        let report = validate_rule_set(&doc);
        assert!(!report.valid, "set 缺 value 应被拒");
        assert!(report.errors.iter().any(|e| e.contains("value")));
    }

    #[test]
    fn merge_tool_results_plural_accepted() {
        let doc = rs(serde_json::json!([
            { "type": "merge", "params": {
                "messages": "__exec__.payload.llm_response.messages",
                "tool_results": "__exec__.payload.service_results",
                "next_instruction": { "type": "call_external", "params": { "messages": "{{messages}}" } }
            } }
        ]));
        let report = validate_rule_set(&doc);
        assert!(
            report.valid,
            "merge 用 tool_results 复数应通过: {:?}",
            report.errors
        );
    }

    #[test]
    fn merge_without_tool_result_rejected() {
        let doc = rs(serde_json::json!([
            { "type": "merge", "params": {
                "messages": "__exec__.payload.llm_response.messages",
                "next_instruction": { "type": "noop" }
            } }
        ]));
        let report = validate_rule_set(&doc);
        assert!(
            !report.valid,
            "merge 无 tool_result 且无 tool_results 应被拒"
        );
    }

    #[test]
    fn unknown_transform_type_rejected() {
        let doc = rs(serde_json::json!([{ "type": "noop" }]));
        let report = validate_rule_set(&doc);
        assert!(
            !report.valid,
            "noop 是指令层类型，不得作为元指令层 transform 类型（P0-01）"
        );
    }

    #[test]
    fn domain_string_without_prefix_rejected() {
        let doc = rs(serde_json::json!([
            { "type": "branch", "params": { "domain": "payload.flag", "on_true": [] } }
        ]));
        let report = validate_rule_set(&doc);
        assert!(
            !report.valid,
            "domain 字符串无 __ 前缀应被拒（运行时报 MissingField）"
        );
    }

    #[test]
    fn transform_list_bare_array() {
        let arr = serde_json::json!([
            { "type": "set", "params": { "attr": "x", "operation": "set", "value": 1 } }
        ]);
        let report = validate_transform_list(&arr);
        assert!(report.valid, "裸 transform 数组应通过: {:?}", report.errors);
    }

    #[test]
    fn empty_transform_list_rejected() {
        let report = validate_transform_list(&serde_json::Value::Array(vec![]));
        assert!(!report.valid, "空 transform 数组应被拒");
    }

    #[test]
    fn rule_input_accepts_wrapped_and_bare() {
        let wrapped = serde_json::json!({ "transform": [
            { "type": "set", "params": { "attr": "x", "operation": "set", "value": 1 } }
        ]});
        assert!(validate_rule_input(&wrapped).valid);
        let bare = serde_json::json!([
            { "type": "set", "params": { "attr": "x", "operation": "set", "value": 1 } }
        ]);
        assert!(validate_rule_input(&bare).valid);
        let garbage = serde_json::json!({ "foo": 1 });
        assert!(!validate_rule_input(&garbage).valid);
    }

    // ===== Opt3：validate_command_instruction（submit_command 指令层门禁）=====

    #[test]
    fn cmd_business_instruction_passes() {
        // demos 实际业务指令（sampling_decider）→ 指令层宽松校验，应通过
        let instr = serde_json::json!({
            "type": "sampling_decider",
            "params": { "sampling_service": "sampling_service", "sample_interval": 5, "timestamp": 1719990000 }
        });
        let report = validate_command_instruction(&instr);
        assert!(report.valid, "业务指令应通过: {:?}", report.errors);
    }

    #[test]
    fn cmd_sequence_passes() {
        // demos 010.json 风格的 sequence 工作流（指令层）→ 应通过
        let instr = serde_json::json!({
            "type": "sequence",
            "params": {
                "instructions": [
                    { "type": "sampling_decider", "params": { "sample_interval": 5 } },
                    { "type": "conditional", "params": {
                        "domain": { "type": "eq", "path": "payload.audit.trigger_shadow", "value": true },
                        "then": { "type": "shadow_validate", "params": {} },
                        "else": { "type": "noop" }
                    } }
                ]
            }
        });
        let report = validate_command_instruction(&instr);
        assert!(report.valid, "sequence 工作流应通过: {:?}", report.errors);
    }

    #[test]
    fn cmd_while_loop_passes() {
        // demos 015.json 风格 while_loop（指令层）→ 应通过
        let instr = serde_json::json!({
            "type": "while_loop",
            "params": {
                "condition": { "type": "lt", "path": "payload.audit.evolution_count", "value": 3 },
                "body": [ { "type": "sampling_decider", "params": {} } ]
            }
        });
        let report = validate_command_instruction(&instr);
        assert!(report.valid, "while_loop 应通过: {:?}", report.errors);
    }

    #[test]
    fn cmd_branch_io_result_singular_rejected() {
        // demos 08.json 的 branch 工作流含单数 __io_result__ → 元指令层严格校验应被拒（Opt2）
        let instr = serde_json::json!({
            "type": "branch",
            "params": {
                "domain": { "type": "instruction", "instruction_type": "sampling_decider" },
                "on_true": [
                    { "type": "branch", "params": {
                        "domain": { "type": "exists", "path": "__exec__.payload.__io_result__" },
                        "on_true": [
                            { "type": "set", "params": { "attr": "audit.trigger_shadow", "operation": "set", "value": "__exec__.payload.__io_result__.trigger" } }
                        ],
                        "on_false": []
                    } }
                ]
            }
        });
        let report = validate_command_instruction(&instr);
        assert!(
            !report.valid,
            "单数 __io_result__ 应被拒: {:?}",
            report.errors
        );
        assert!(report.errors.iter().any(|e| e.contains("__io_result__")));
    }

    #[test]
    fn cmd_sequence_missing_instructions_rejected() {
        let instr = serde_json::json!({ "type": "sequence", "params": {} });
        let report = validate_command_instruction(&instr);
        assert!(
            !report.valid,
            "sequence 缺 instructions 应被拒: {:?}",
            report.errors
        );
    }

    #[test]
    fn cmd_set_bad_attr_path_rejected() {
        // 指令层 set.attr 路径语法（Opt1）
        let instr = serde_json::json!({ "type": "set", "params": { "attr": "payload.x.", "operation": "set", "value": 1 } });
        let report = validate_command_instruction(&instr);
        assert!(
            !report.valid,
            "set.attr 尾部空段应被拒: {:?}",
            report.errors
        );
    }

    #[test]
    fn cmd_non_object_rejected() {
        let report = validate_command_instruction(&serde_json::json!([1, 2]));
        assert!(!report.valid, "非对象指令应被拒");
    }

    #[test]
    fn cmd_set_meta_attr_path_ok() {
        // 元指令层 set（引擎原生）合法路径应通过
        let instr = serde_json::json!({ "type": "set", "params": { "attr": "system.running", "operation": "set", "value": false } });
        let report = validate_command_instruction(&instr);
        assert!(report.valid, "合法 set 指令应通过: {:?}", report.errors);
    }

    // ===== C9：validate_service_registry（服务注册表加载期门禁）=====

    #[test]
    fn service_registry_valid_passes() {
        let doc = serde_json::json!({
            "echo_svc": {
                "url": "http://127.0.0.1:5001/echo",
                "method": "POST",
                "headers": { "X-Source": "evorule" },
                "timeout_ms": 5000
            },
            "llm_advisor": { "url": "http://127.0.0.1:18081/v1", "version": "1.0.0" }
        });
        let report = validate_service_registry(&doc);
        assert!(report.valid, "合法注册表应通过: {:?}", report.errors);
    }

    #[test]
    fn service_registry_missing_url_rejected() {
        let doc = serde_json::json!({ "bad_svc": { "method": "POST" } });
        let report = validate_service_registry(&doc);
        assert!(!report.valid, "缺 url 应被拒");
    }

    #[test]
    fn service_registry_bad_headers_and_timeout_rejected() {
        // headers 值非字符串 + timeout_ms 负数（schema 层拦截，parse 层各报一条）
        let doc = serde_json::json!({
            "bad_svc": {
                "url": "http://127.0.0.1:5001/x",
                "headers": { "X-Auth": 12345 },
                "timeout_ms": -1
            }
        });
        let report = validate_service_registry(&doc);
        assert!(
            !report.valid,
            "headers 非字符串值 / timeout_ms 负数应被拒: {:?}",
            report.errors
        );
        assert!(report.errors.len() >= 2);
    }

    #[test]
    fn service_registry_non_object_rejected() {
        let report = validate_service_registry(&serde_json::json!([1, 2]));
        assert!(!report.valid, "顶层数组应被拒（必须 object）");
    }

    #[test]
    fn service_registry_unknown_fields_forward_compatible() {
        // 未知字段开放（additionalProperties: true）——向前兼容，不得拒绝
        let doc = serde_json::json!({
            "svc": { "url": "http://127.0.0.1:5001/x", "body_template": { "a": 1 } }
        });
        let report = validate_service_registry(&doc);
        assert!(report.valid, "未知字段应开放: {:?}", report.errors);
    }

    // ===== Q12 W5：validate_knowledge（知识数据资产文档门禁）=====

    fn kn_doc(entries: Value) -> Value {
        serde_json::json!({
            "$schema": "https://evorule.org/schemas/knowledge/v1.0.json",
            "kind": "knowledge",
            "id": "com.evorule.test.kn",
            "version": "0.1.0",
            "metadata": { "title": "测试数据资产" },
            "entries": entries
        })
    }

    #[test]
    fn knowledge_doc_and_data_entries_pass() {
        // 双形态可混排：文档条目（content）+ 数据条目（payload+schema_ref）
        let doc = kn_doc(serde_json::json!([
            { "id": "pitfall-001", "content": "能量漂移需先查碰撞恢复系数", "severity": "warning" },
            { "id": "scn-001", "payload": { "mass": 1.5 }, "schema_ref": "https://rpsm.example/schemas/body.json" }
        ]));
        let report = validate_knowledge(&doc);
        assert!(report.valid, "双形态条目应通过: {:?}", report.errors);
    }

    #[test]
    fn knowledge_entry_mixed_forms_rejected() {
        // 同条目同时携带 content 与 payload/schema_ref → oneOf 互斥拒绝
        let doc = kn_doc(serde_json::json!([
            { "id": "bad-001", "content": "x", "payload": { "a": 1 }, "schema_ref": "u" }
        ]));
        let report = validate_knowledge(&doc);
        assert!(!report.valid, "双形态混于单条目应被拒");
    }

    #[test]
    fn knowledge_data_entry_missing_schema_ref_rejected() {
        let doc = kn_doc(serde_json::json!([
            { "id": "bad-002", "payload": { "a": 1 } }
        ]));
        let report = validate_knowledge(&doc);
        assert!(!report.valid, "数据条目缺 schema_ref 应被拒");
    }

    #[test]
    fn knowledge_doc_missing_kind_rejected() {
        // _meta 治理骨架：kind 必填且必须为 knowledge
        let mut doc = kn_doc(serde_json::json!([{ "id": "d", "content": "x" }]));
        doc.as_object_mut()
            .unwrap()
            .insert("kind".into(), "rule_set".into());
        let report = validate_knowledge(&doc);
        assert!(!report.valid, "kind 非 knowledge 应被拒");
    }
}

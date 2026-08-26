// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 规则结构转译 (condition/action ↔ transform) — 界面升级 v1.0 阶段 A.2
//!
//! 设计依据: 实施文档_界面升级_v1.0.md §四 A.2 + 00_架构边界原则.md §七
//!
//! # 职责
//! 双模式编辑器需要 condition/action 人类可读视图与 evorule transform/branch
//! 元指令视图之间的互转。转译是**纯函数** (输入 JSON → 输出 JSON):
//! - 不写数据库
//! - 不调用 reactor
//! - 不进 evorule 仓
//!
//! # G1-G7 校验
//! 对齐 console 侧 `ruleValidator.ts` 的 7 条门禁 (L_console UX 预校验, 非权威)。
//! 核心仓 build.rs 是最终拦截者, 本层只做转译后的结构校验反馈。
//!
//! # 转译语义 (lossy)
//! - to_transform: condition+action → transform (生成结构, 末条补 all([]) 兜底)
//! - to_conditional: transform → condition+action (仅 set/branch(eq/lt/exists) 可回译;
//!   push/io_request/嵌套 branch/all/not/instruction 超出子集 → lossy=true + lost_items)

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{WorkspaceError, WorkspaceResult};

// =============================================================================
// 常量 (对齐 ruleValidator.ts) — UX 门禁口径, 非权威镜像
// =============================================================================
//
// # 口径声明 (P: G1-G7 为编辑器 UX 预校验, 非权威)
// 本层常量是"非权威 UX 预校验"口径, 对齐 console 侧 ruleValidator.ts,
// 与 evorule-governance 权威校验是"口径一致"关系, 而非"数值相同"关系。
// 权威拦截者: core 仓 build.rs + evorule-governance/src/rule_validation.rs
//   (MAX_TRANSFORM_RULES=64 / MAX_NESTING_DEPTH=64(对齐 TCB MAX_BRANCH_DEPTH, P2-02) /
//    MAX_IO_PARAMS_SIZE=10KB)。
//
// # 同步提醒 (方案 A: 显式锚定, 消静默漂移)
// 若核心仓调整校验语义(元指令集合 / 域类型 / 深度限制 / I/O 参数上限),
// 本层常量不会自动同步, 需人工核对。判定标准 = 边界输入下 UX 门禁与
// 权威校验的"放行/拒绝结论"一致, 而非常量数值相等。
// (自动报警见 evorule-server 对齐集成测试的规划, 尚未落地)

const VALID_META_INSTRUCTIONS: &[&str] =
    &["set", "push", "branch", "io_request", "collect", "merge"];
const VALID_DOMAIN_TYPES: &[&str] = &[
    "eq",
    "lt",
    "exists",
    "instruction",
    "all",
    "not",
    "has_fields",
];
// UX 门禁: 整体递归深度 ≤ 64。
// 注意与权威层 MAX_NESTING_DEPTH=64 (branch 嵌套层数, 对齐 TCB MAX_BRANCH_DEPTH, P2-02)
// 是不同量, 勿混; 仅需在边界输入下两者放行/拒绝结论一致即可。
const MAX_RECURSION_DEPTH: usize = 64;

// =============================================================================
// G1-G7 校验 (Rust 版, 对齐 console ruleValidator.ts)
// =============================================================================

/// 校验错误 (对齐 TS ValidationError)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationError {
    pub gate: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// 校验结果 (对齐 TS ValidationResult, UX 门禁 G1-G7, 非权威)
///
/// 命名加 `Ux` 前缀以与核心仓 (evorule-governance) 的权威 `ValidationResult`
/// 明确区分, 消除"两处 ValidationResult 语义不同"的漂移混淆。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UxValidationResult {
    pub valid: bool,
    pub errors: Vec<ValidationError>,
}

/// G1-G7 门禁校验入口
///
/// 入参为规则 JSON 文本 (期望含 `transform` 数组)。
pub fn validate_rule_json(json: &str) -> UxValidationResult {
    // G1: JSON 格式合法性
    let parsed: Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(e) => {
            return UxValidationResult {
                valid: false,
                errors: vec![ValidationError {
                    gate: "G1".into(),
                    message: format!("JSON 格式错误: {e}"),
                    path: None,
                }],
            };
        }
    };

    let mut errors: Vec<ValidationError> = Vec::new();

    // G2: 元指令类型合法性 (递归)
    if let Some(transform) = parsed.get("transform").and_then(|v| v.as_array()) {
        for (i, rule) in transform.iter().enumerate() {
            check_meta_instruction(rule, &mut errors, &format!("transform[{i}]"));
        }
    }

    // G3: I/O 双路径模式
    check_io_two_phase(&parsed, &mut errors);

    // G4: 域类型合法性
    check_domain_types(&parsed, &mut errors);

    // G5: 路径引用格式
    check_path_references(&parsed, &mut errors);

    // G6: 兜底规则存在
    check_fallback_rule(&parsed, &mut errors);

    // G7: 递归深度限制
    check_recursion_depth(&parsed, &mut errors);

    UxValidationResult {
        valid: errors.is_empty(),
        errors,
    }
}

/// G2: 元指令类型合法性 (set / push / branch / io_request)
fn check_meta_instruction(rule: &Value, errors: &mut Vec<ValidationError>, path: &str) {
    let Some(rule_obj) = rule.as_object() else {
        errors.push(ValidationError {
            gate: "G2".into(),
            message: format!("规则必须是对象，路径: {path}"),
            path: Some(path.into()),
        });
        return;
    };

    let type_val = rule_obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if !VALID_META_INSTRUCTIONS.contains(&type_val) {
        errors.push(ValidationError {
            gate: "G2".into(),
            message: format!(
                "无效的元指令类型: {type_val}，必须是 {} 之一",
                VALID_META_INSTRUCTIONS.join(", ")
            ),
            path: Some(path.into()),
        });
    }

    // 递归检查子指令 (on_true / on_false)
    if let Some(params) = rule_obj.get("params").and_then(|v| v.as_object()) {
        for branch_key in ["on_true", "on_false"] {
            if let Some(arr) = params.get(branch_key).and_then(|v| v.as_array()) {
                for (i, sub) in arr.iter().enumerate() {
                    check_meta_instruction(
                        sub,
                        errors,
                        &format!("{path}.params.{branch_key}[{i}]"),
                    );
                }
            }
        }
    }
}

/// G3: io_request 必须在 exists(__io_results__) 分支内（P1-03：复数，按 io_type 隔离）
fn check_io_two_phase(value: &Value, errors: &mut Vec<ValidationError>) {
    let Some(transform) = value.get("transform").and_then(|v| v.as_array()) else {
        return;
    };

    fn check_rule(rule: &Value, errors: &mut Vec<ValidationError>, path: &str, in_io: bool) {
        let Some(rule_obj) = rule.as_object() else {
            return;
        };
        let t = rule_obj.get("type").and_then(|v| v.as_str()).unwrap_or("");

        if t == "io_request" && !in_io {
            errors.push(ValidationError {
                gate: "G3".into(),
                message: format!("io_request 必须在 exists(__io_results__) 分支内，路径: {path}"),
                path: Some(path.into()),
            });
        }

        // 判断当前 branch 是否为 __io_results__ 检查
        //（TCB v0.3.1：结果按 io_type 隔离写入 __io_results__.{io_type}，
        //  规则引用用显式前缀 __exec__.payload.__io_results__.{io_type}，见 core_eval.json）
        let is_io_result_check = if t == "branch" {
            rule_obj
                .get("params")
                .and_then(|p| p.get("domain"))
                .and_then(|d| d.as_object())
                .map(|d| {
                    d.get("type").and_then(|v| v.as_str()) == Some("exists")
                        && d.get("path")
                            .and_then(|v| v.as_str())
                            .map(|p| p.starts_with("__exec__.payload.__io_results__."))
                            .unwrap_or(false)
                })
                .unwrap_or(false)
        } else {
            false
        };

        if let Some(params) = rule_obj.get("params").and_then(|v| v.as_object()) {
            for bk in ["on_true", "on_false"] {
                if let Some(arr) = params.get(bk).and_then(|v| v.as_array()) {
                    for (i, sub) in arr.iter().enumerate() {
                        check_rule(
                            sub,
                            errors,
                            &format!("{path}.params.{bk}[{i}]"),
                            is_io_result_check || in_io,
                        );
                    }
                }
            }
        }
    }

    for (i, rule) in transform.iter().enumerate() {
        check_rule(rule, errors, &format!("transform[{i}]"), false);
    }
}

/// G4: 域类型合法性 (eq / lt / exists / instruction / all / not)
fn check_domain_types(value: &Value, errors: &mut Vec<ValidationError>) {
    fn check_domain(domain: &Value, errors: &mut Vec<ValidationError>, path: &str) {
        let Some(dom_obj) = domain.as_object() else {
            return;
        };
        if let Some(t) = dom_obj.get("type").and_then(|v| v.as_str()) {
            if !VALID_DOMAIN_TYPES.contains(&t) {
                errors.push(ValidationError {
                    gate: "G4".into(),
                    message: format!(
                        "无效的域类型: {t}，必须是 {} 之一",
                        VALID_DOMAIN_TYPES.join(", ")
                    ),
                    path: Some(path.into()),
                });
            }
            // 嵌套域递归。P0-03：schema 用 inner（all=数组 / not=单域）；
            // 旧 domain/domains 是 P0-03 违规字段，一并检测以暴露迁移期脏数据。
            if let Some(inner) = dom_obj.get("inner") {
                match inner {
                    Value::Array(arr) => {
                        for (i, d) in arr.iter().enumerate() {
                            check_domain(d, errors, &format!("{path}.inner[{i}]"));
                        }
                    }
                    Value::Object(_) => check_domain(inner, errors, &format!("{path}.inner")),
                    _ => {}
                }
            }
            if let Some(arr) = dom_obj.get("domains").and_then(|v| v.as_array()) {
                for (i, d) in arr.iter().enumerate() {
                    check_domain(d, errors, &format!("{path}.domains[{i}]"));
                }
            }
            if let Some(d) = dom_obj.get("domain") {
                check_domain(d, errors, &format!("{path}.domain"));
            }
        }
    }

    fn check_rule(rule: &Value, errors: &mut Vec<ValidationError>, path: &str) {
        let Some(rule_obj) = rule.as_object() else {
            return;
        };
        if let Some(domain) = rule_obj.get("params").and_then(|p| p.get("domain")) {
            check_domain(domain, errors, &format!("{path}.params.domain"));
        }
        if let Some(params) = rule_obj.get("params").and_then(|v| v.as_object()) {
            for bk in ["on_true", "on_false"] {
                if let Some(arr) = params.get(bk).and_then(|v| v.as_array()) {
                    for (i, sub) in arr.iter().enumerate() {
                        check_rule(sub, errors, &format!("{path}.params.{bk}[{i}]"));
                    }
                }
            }
        }
    }

    if let Some(transform) = value.get("transform").and_then(|v| v.as_array()) {
        for (i, rule) in transform.iter().enumerate() {
            check_rule(rule, errors, &format!("transform[{i}]"));
        }
    }
}

/// G5: 路径引用格式 (__ 前缀必须符合 __exec__.payload.xxx 等)
fn check_path_references(value: &Value, errors: &mut Vec<ValidationError>) {
    fn check_paths(obj: &Value, errors: &mut Vec<ValidationError>, path: &str) {
        match obj {
            Value::Object(map) => {
                for (key, val) in map {
                    let child_path = format!("{path}.{key}");
                    if let Some(s) = val.as_str() {
                        if s.starts_with("__") {
                            // G5 白名单: payload(输入) / instruction(当前指令) / queue(队列) /
                            // io_results(I/O 结果, 复数按 io_type 隔离, P1-03) / result(规则输出标记,
                            // 业务动作 notify/approve/flag)
                            let ok = s.starts_with("__exec__.payload.")
                                || s.starts_with("__exec__.instruction.")
                                || s.starts_with("__exec__.queue")
                                || s.starts_with("__exec__.result.")
                                || s == "__io_results__"
                                || s.starts_with("__io_results__.");
                            if !ok {
                                errors.push(ValidationError {
                                    gate: "G5".into(),
                                    message: format!(
                                        "无效的路径引用格式: {s}，必须以 __exec__.payload. / __exec__.instruction. / __exec__.result. 开头"
                                    ),
                                    path: Some(child_path),
                                });
                            }
                        }
                    } else {
                        check_paths(val, errors, &child_path);
                    }
                }
            }
            Value::Array(arr) => {
                for (i, v) in arr.iter().enumerate() {
                    check_paths(v, errors, &format!("{path}[{i}]"));
                }
            }
            _ => {}
        }
    }
    check_paths(value, errors, "root");
}

/// G6: 兜底规则存在 (末条必须是 branch + all([]))
fn check_fallback_rule(value: &Value, errors: &mut Vec<ValidationError>) {
    let Some(transform) = value.get("transform").and_then(|v| v.as_array()) else {
        errors.push(ValidationError {
            gate: "G6".into(),
            message: "规则列表为空，缺少兜底规则".into(),
            path: Some("transform".into()),
        });
        return;
    };
    if transform.is_empty() {
        errors.push(ValidationError {
            gate: "G6".into(),
            message: "规则列表为空，缺少兜底规则".into(),
            path: Some("transform".into()),
        });
        return;
    }
    let last_idx = transform.len() - 1;
    let Some(last) = transform.get(last_idx).and_then(|v| v.as_object()) else {
        errors.push(ValidationError {
            gate: "G6".into(),
            message: "最后一条规则必须是 branch 类型的兜底规则".into(),
            path: Some(format!("transform[{last_idx}]")),
        });
        return;
    };
    if last.get("type").and_then(|v| v.as_str()) != Some("branch") {
        errors.push(ValidationError {
            gate: "G6".into(),
            message: "最后一条规则必须是 branch 类型的兜底规则".into(),
            path: Some(format!("transform[{last_idx}]")),
        });
        return;
    }
    let domain = last.get("params").and_then(|p| p.get("domain"));
    let valid = domain
        .and_then(|d| d.as_object())
        .map(|d| {
            d.get("type").and_then(|v| v.as_str()) == Some("all")
                && d.get("inner")
                    .and_then(|v| v.as_array())
                    .map(|a| a.is_empty())
                    .unwrap_or(false)
        })
        .unwrap_or(false);
    if !valid {
        errors.push(ValidationError {
            gate: "G6".into(),
            message: "兜底规则必须使用 all({inner: []}) 空域匹配所有未识别指令 (P0-03: 禁止 domain/domains)"
                .into(),
            path: Some(format!("transform[{last_idx}].params.domain")),
        });
    }
}

/// G7: 递归深度限制 (≤ 64)
fn check_recursion_depth(value: &Value, errors: &mut Vec<ValidationError>) {
    fn check_depth(obj: &Value, errors: &mut Vec<ValidationError>, depth: usize, path: &str) {
        if depth > MAX_RECURSION_DEPTH {
            errors.push(ValidationError {
                gate: "G7".into(),
                message: format!("递归深度超过 {MAX_RECURSION_DEPTH} 层，路径: {path}"),
                path: Some(path.into()),
            });
            return;
        }
        let Some(rule_obj) = obj.as_object() else {
            return;
        };
        if let Some(params) = rule_obj.get("params").and_then(|v| v.as_object()) {
            for bk in ["on_true", "on_false"] {
                if let Some(arr) = params.get(bk).and_then(|v| v.as_array()) {
                    for (i, sub) in arr.iter().enumerate() {
                        check_depth(sub, errors, depth + 1, &format!("{path}.params.{bk}[{i}]"));
                    }
                }
            }
        }
    }
    if let Some(transform) = value.get("transform").and_then(|v| v.as_array()) {
        for (i, rule) in transform.iter().enumerate() {
            check_depth(rule, errors, 0, &format!("transform[{i}]"));
        }
    }
}

// =============================================================================
// 转译 DTO (对齐 A.2 端点契约)
// =============================================================================

/// to_transform 请求: { condition, action_set, metadata }
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct TranslateToTransformRequest {
    /// condition 列表: [{field, op, value}]
    pub condition: Vec<Value>,
    /// action_set 列表: [{attr, operation, value}]
    pub action_set: Vec<Value>,
    /// 可选元数据: {name, description, tags}
    #[serde(default)]
    pub metadata: Option<Value>,
}

/// to_transform 响应
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct TranslateToTransformResponse {
    pub transform: Vec<Value>,
    pub g1_g7_pass: bool,
    pub warnings: Vec<String>,
}

/// to_conditional 请求: { transform }
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct TranslateToConditionalRequest {
    pub transform: Vec<Value>,
}

/// to_conditional 响应
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct TranslateToConditionalResponse {
    pub condition: Vec<Value>,
    pub action_set: Vec<Value>,
    pub lossy: bool,
    pub lost_items: Vec<String>,
}

// =============================================================================
// 转译实现
// =============================================================================

/// 根据业务比较符构建 evorule domain (决策1翻译表,见 08 方案 §4.1)
///
/// 翻译规则:
///   eq/lt/exists → 直接对应 {type, path, value}
///   gte(≥)       → not(lt(x))         非小于 = 大于等于
///   gt(>)        → not(all([lt,eq]))  非(小于且等于) = 大于
///   其他         → 原样透传 (G4 会拦截非法域类型)
///
/// P0-03: 嵌套一律用 `inner`（not=单域对象, all=域数组），禁止旧 `domain`/`domains` 字段，
/// 与固化 v1.0 Schema（records/70）及引擎 domain.rs 实现对齐。
fn build_condition_domain(op: &str, field: &str, value: Value) -> Value {
    match op {
        "eq" | "lt" | "exists" => serde_json::json!({
            "type": op, "path": field, "value": value
        }),
        "gte" => serde_json::json!({
            "type": "not",
            "inner": { "type": "lt", "path": field, "value": value }
        }),
        "gt" => serde_json::json!({
            "type": "not",
            "inner": {
                "type": "all",
                "inner": [
                    { "type": "lt", "path": field, "value": value.clone() },
                    { "type": "eq", "path": field, "value": value }
                ]
            }
        }),
        // 未知 op 透传 (G4/Schema 门禁拦截)
        _ => serde_json::json!({ "type": op, "path": field, "value": value }),
    }
}

/// to_transform: condition + action_set → transform
///
/// 转译规则:
/// - 每个 condition {field, op, value} → branch 指令 (domain={op, path=field, value}, on_true=[])
/// - 每个 action_set {attr, operation, value} → set 指令 (params={path=attr, operation, value})
/// - 末条自动补 all([]) 兜底 (满足 G6)
/// - 对生成的 transform 跑 G1-G7, warnings 收集错误信息
pub fn translate_to_transform(
    req: TranslateToTransformRequest,
) -> WorkspaceResult<TranslateToTransformResponse> {
    let mut transform: Vec<Value> = Vec::new();

    // condition → branch 指令
    // 业务比较符 gte/gt 翻译成 evorule 域类型组合 (决策1,见 08 方案 §4.1):
    //   gte(x) = not(lt(x))         — 非小于 = 大于等于
    //   gt(x)  = not(all([lt,eq]))  — 非(小于且等于) = 大于
    //   eq/lt/exists 直接对应
    for c in &req.condition {
        let field = c.get("field").and_then(|v| v.as_str()).unwrap_or("");
        let op = c.get("op").and_then(|v| v.as_str()).unwrap_or("eq");
        let value = c.get("value").cloned().unwrap_or(Value::Null);
        let domain = build_condition_domain(op, field, value);
        transform.push(serde_json::json!({
            "type": "branch",
            "params": {
                "domain": domain,
                "on_true": [],
                "on_false": []
            }
        }));
    }

    // action_set → set 指令
    // 注意: evorule core executor (tier0-tcb/src/executor.rs exec_set) 读取 params.attr
    // (不是 params.path), 且 BUILTIN_RULES 也用 params.attr — 此处对齐核心执行器约定。
    for a in &req.action_set {
        let attr = a.get("attr").and_then(|v| v.as_str()).unwrap_or("");
        let operation = a.get("operation").and_then(|v| v.as_str()).unwrap_or("set");
        let value = a.get("value").cloned().unwrap_or(Value::Null);
        transform.push(serde_json::json!({
            "type": "set",
            "params": {
                "attr": attr,
                "operation": operation,
                "value": value
            }
        }));
    }

    // 末条补 all({inner: []}) 兜底 (G6, P0-03: 禁止 domain/domains)
    transform.push(serde_json::json!({
        "type": "branch",
        "params": {
            "domain": { "type": "all", "inner": [] },
            "on_true": [],
            "on_false": []
        }
    }));

    // 跑 G1-G7 (UX 预校验, 非权威)
    let rule_obj =
        serde_json::json!({ "transform": transform.clone(), "metadata": req.metadata.clone() });
    let rule_text = serde_json::to_string(&rule_obj)
        .map_err(|e| WorkspaceError::internal(format!("serialize translate result: {e}")))?;
    let vr = validate_rule_json(&rule_text);
    let warnings: Vec<String> = vr
        .errors
        .iter()
        .map(|e| format!("[{}] {}", e.gate, e.message))
        .collect();

    // Schema 门禁（线1 权威拦截, records/77）：转译输出必须是引擎原生合法结构。
    // 不通过则整体拒绝返回——任何转译产物不得携带 Schema 非法结构流出本层
    // （防止"编辑器转译出来的规则跑不起来"的 P0-03 类问题扩散到上层应用）。
    let schema_report =
        evorule_rule_schema::validate_transform_list(&serde_json::Value::Array(transform.clone()));
    if !schema_report.valid {
        let detail = schema_report.errors.join("; ");
        return Err(WorkspaceError::invalid_input(format!(
            "转译输出未通过 Schema 门禁（引擎原生结构非法, 参见固化的 rule_set v1.0 Schema）：{detail}"
        )));
    }

    Ok(TranslateToTransformResponse {
        transform,
        g1_g7_pass: vr.valid,
        warnings,
    })
}

/// 识别 gt 模式: not(all([lt(path,value), eq(path,value)])) 且两子域 path/value 一致
/// 匹配则返回 (field, value), 否则 None
/// P0-03: all 的嵌套读 `inner` 数组。
fn extract_gt_pattern(all_domain: &serde_json::Map<String, Value>) -> Option<(String, Value)> {
    let inner = all_domain.get("inner").and_then(|v| v.as_array())?;
    if inner.len() != 2 {
        return None;
    }
    let d0 = inner[0].as_object()?;
    let d1 = inner[1].as_object()?;
    // 必须是 [lt, eq] 顺序 (translate_to_transform 的生成顺序)
    if d0.get("type").and_then(|v| v.as_str()) != Some("lt")
        || d1.get("type").and_then(|v| v.as_str()) != Some("eq")
    {
        return None;
    }
    let path0 = d0.get("path").and_then(|v| v.as_str())?;
    let path1 = d1.get("path").and_then(|v| v.as_str())?;
    if path0 != path1 {
        return None;
    }
    let value0 = d0.get("value")?;
    let value1 = d1.get("value")?;
    if value0 != value1 {
        return None;
    }
    Some((path0.to_string(), value0.clone()))
}

/// to_conditional: transform → condition + action_set (lossy)
///
/// 回译规则:
/// - branch (domain eq/lt/exists) → condition {field=path, op=domain.type, value=domain.value}
/// - set → action_set {attr=params.path, operation=params.operation, value=params.value}
/// - push / io_request / 嵌套 branch / all / not / instruction → 超出子集, lossy=true + lost_items
pub fn translate_to_conditional(
    req: TranslateToConditionalRequest,
) -> WorkspaceResult<TranslateToConditionalResponse> {
    let mut condition: Vec<Value> = Vec::new();
    let mut action_set: Vec<Value> = Vec::new();
    let mut lost_items: Vec<String> = Vec::new();

    for (i, rule) in req.transform.iter().enumerate() {
        let Some(rule_obj) = rule.as_object() else {
            lost_items.push(format!("transform[{i}]: 非对象"));
            continue;
        };
        let t = rule_obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match t {
            "branch" => {
                let domain = rule_obj.get("params").and_then(|p| p.get("domain"));
                let dom_type = domain
                    .and_then(|d| d.as_object())
                    .and_then(|d| d.get("type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                // 仅 eq/lt/exists 可回译; all(兜底)跳过; not/instruction 超出子集
                match dom_type {
                    "eq" | "lt" | "exists" => {
                        let dom = domain
                            .and_then(|d| d.as_object())
                            .cloned()
                            .unwrap_or_default();
                        let field = dom
                            .get("path")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let value = dom.get("value").cloned().unwrap_or(Value::Null);
                        condition.push(serde_json::json!({
                            "field": field,
                            "op": dom_type,
                            "value": value,
                        }));
                    }
                    "all" => {
                        // 兜底规则, 跳过 (不回译为 condition)
                    }
                    "not" => {
                        // 识别 gte/gt 模式 (决策1对称回译,见 08 方案 §4.1)
                        // P0-03: not 的嵌套读 `inner` 单域对象
                        let inner = domain
                            .and_then(|d| d.as_object())
                            .and_then(|d| d.get("inner"))
                            .and_then(|d| d.as_object());
                        if let Some(inner_obj) = inner {
                            let inner_type =
                                inner_obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
                            match inner_type {
                                "lt" => {
                                    // not(lt(x)) → gte
                                    let field = inner_obj
                                        .get("path")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let value =
                                        inner_obj.get("value").cloned().unwrap_or(Value::Null);
                                    condition.push(serde_json::json!({
                                        "field": field, "op": "gte", "value": value,
                                    }));
                                }
                                "all" => {
                                    // not(all([lt,eq])) → gt (需验证子域是 [lt(x), eq(x)] 同 path/value)
                                    if let Some((field, value)) = extract_gt_pattern(inner_obj) {
                                        condition.push(serde_json::json!({
                                            "field": field, "op": "gt", "value": value,
                                        }));
                                    } else {
                                        lost_items.push(format!("transform[{i}]: not(all(...)) 非标准 gt 模式, 超出回译子集"));
                                    }
                                }
                                _ => {
                                    lost_items.push(format!(
                                        "transform[{i}]: not({inner_type}...) 超出回译子集"
                                    ));
                                }
                            }
                        } else {
                            lost_items
                                .push(format!("transform[{i}]: not 缺少 inner 子域, 超出回译子集"));
                        }
                    }
                    _ => {
                        lost_items.push(format!(
                            "transform[{i}]: branch domain 类型 '{dom_type}' 超出回译子集"
                        ));
                    }
                }
                // 检查是否有嵌套子指令 (on_true 非空) → 超出子集
                let has_nested = rule_obj
                    .get("params")
                    .and_then(|p| p.as_object())
                    .map(|p| {
                        p.get("on_true")
                            .and_then(|v| v.as_array())
                            .map(|a| !a.is_empty())
                            .unwrap_or(false)
                            || p.get("on_false")
                                .and_then(|v| v.as_array())
                                .map(|a| !a.is_empty())
                                .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if has_nested {
                    lost_items.push(format!("transform[{i}]: 嵌套子指令超出回译子集"));
                }
            }
            "set" => {
                // 对齐 exec_set: 读取 params.attr (不是 params.path)
                let params = rule_obj
                    .get("params")
                    .and_then(|p| p.as_object())
                    .cloned()
                    .unwrap_or_default();
                let attr = params
                    .get("attr")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let operation = params
                    .get("operation")
                    .and_then(|v| v.as_str())
                    .unwrap_or("set")
                    .to_string();
                let value = params.get("value").cloned().unwrap_or(Value::Null);
                action_set.push(serde_json::json!({
                    "attr": attr,
                    "operation": operation,
                    "value": value,
                }));
            }
            "push" | "io_request" => {
                lost_items.push(format!("transform[{i}]: 元指令 '{t}' 超出回译子集"));
            }
            _ => {
                lost_items.push(format!("transform[{i}]: 未知元指令 '{t}'"));
            }
        }
    }

    let lossy = !lost_items.is_empty();
    Ok(TranslateToConditionalResponse {
        condition,
        action_set,
        lossy,
        lost_items,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn g1_invalid_json() {
        let r = validate_rule_json("not json at all }");
        assert!(!r.valid);
        assert_eq!(r.errors[0].gate, "G1");
    }

    #[test]
    fn g6_missing_fallback() {
        let json = r#"{"transform":[{"type":"set","params":{"path":"a","value":1}}]}"#;
        let r = validate_rule_json(json);
        assert!(r.errors.iter().any(|e| e.gate == "G6"));
    }

    #[test]
    fn to_transform_generates_fallback() {
        let req = TranslateToTransformRequest {
            condition: vec![serde_json::json!({"field":"x","op":"eq","value":1})],
            action_set: vec![serde_json::json!({"attr":"y","operation":"set","value":2})],
            metadata: None,
        };
        let resp = translate_to_transform(req).unwrap();
        assert!(resp.transform.len() >= 3); // 1 branch + 1 set + 1 fallback
                                            // 末条应是 all([]) 兜底
        let last = resp.transform.last().unwrap();
        assert_eq!(last.get("type").and_then(|v| v.as_str()), Some("branch"));
    }

    #[test]
    fn to_conditional_lossy_for_io_request() {
        // 对齐 exec_set: set 指令用 params.attr (不是 params.path)
        let req = TranslateToConditionalRequest {
            transform: vec![
                serde_json::json!({"type":"set","params":{"attr":"a","operation":"set","value":1}}),
                serde_json::json!({"type":"io_request","params":{}}),
            ],
        };
        let resp = translate_to_conditional(req).unwrap();
        assert!(resp.lossy);
        assert!(!resp.lost_items.is_empty());
        assert_eq!(resp.action_set.len(), 1);
    }

    // === 决策1测试: gte/gt 翻译 + 对称回译 (08 方案 §4.1) ===

    #[test]
    fn to_transform_gte_generates_not_lt() {
        let req = TranslateToTransformRequest {
            condition: vec![
                serde_json::json!({"field":"__exec__.payload.amount","op":"gte","value":10000}),
            ],
            action_set: vec![],
            metadata: None,
        };
        let resp = translate_to_transform(req).unwrap();
        // transform[0] 应是 branch + domain.type=not + domain.inner.type=lt (P0-03: inner)
        let dom = &resp.transform[0]["params"]["domain"];
        assert_eq!(dom["type"], "not");
        assert_eq!(dom["inner"]["type"], "lt");
        assert_eq!(dom["inner"]["path"], "__exec__.payload.amount");
        assert_eq!(dom["inner"]["value"], 10000);
        // G4 必须通过 (not/lt 都是合法域类型)
        assert!(resp.g1_g7_pass, "gte 翻译后应通过 G4: {:?}", resp.warnings);
    }

    #[test]
    fn to_transform_gt_generates_not_all_lt_eq() {
        let req = TranslateToTransformRequest {
            condition: vec![
                serde_json::json!({"field":"__exec__.payload.amount","op":"gt","value":10000}),
            ],
            action_set: vec![],
            metadata: None,
        };
        let resp = translate_to_transform(req).unwrap();
        let dom = &resp.transform[0]["params"]["domain"];
        assert_eq!(dom["type"], "not");
        assert_eq!(dom["inner"]["type"], "all");
        let sub = dom["inner"]["inner"].as_array().unwrap();
        assert_eq!(sub.len(), 2);
        assert_eq!(sub[0]["type"], "lt");
        assert_eq!(sub[1]["type"], "eq");
        assert!(resp.g1_g7_pass, "gt 翻译后应通过 G4: {:?}", resp.warnings);
    }

    #[test]
    fn to_conditional_symmetric_gte() {
        // gte → transform → conditional → 应回译成 gte (不 lossy)
        let req_fwd = TranslateToTransformRequest {
            condition: vec![
                serde_json::json!({"field":"__exec__.payload.amount","op":"gte","value":10000}),
            ],
            action_set: vec![],
            metadata: None,
        };
        let fwd = translate_to_transform(req_fwd).unwrap();
        let req_rev = TranslateToConditionalRequest {
            transform: fwd.transform,
        };
        let rev = translate_to_conditional(req_rev).unwrap();
        assert!(
            !rev.lossy,
            "gte 应对称回译, 不应 lossy: {:?}",
            rev.lost_items
        );
        assert_eq!(rev.condition.len(), 1);
        assert_eq!(rev.condition[0]["op"], "gte");
        assert_eq!(rev.condition[0]["field"], "__exec__.payload.amount");
        assert_eq!(rev.condition[0]["value"], 10000);
    }

    #[test]
    fn to_conditional_symmetric_gt() {
        let req_fwd = TranslateToTransformRequest {
            condition: vec![serde_json::json!({"field":"x","op":"gt","value":5})],
            action_set: vec![],
            metadata: None,
        };
        let fwd = translate_to_transform(req_fwd).unwrap();
        let rev = translate_to_conditional(TranslateToConditionalRequest {
            transform: fwd.transform,
        })
        .unwrap();
        assert!(!rev.lossy, "gt 应对称回译: {:?}", rev.lost_items);
        assert_eq!(rev.condition[0]["op"], "gt");
    }

    #[test]
    fn to_conditional_not_eq_is_lossy() {
        // not(eq(x)) 不是 gte/gt 模式, 应 lossy (P0-03: inner)
        let transform = vec![serde_json::json!({
            "type": "branch",
            "params": {"domain": {"type":"not","inner":{"type":"eq","path":"x","value":1}}, "on_true":[], "on_false":[]}
        })];
        let rev = translate_to_conditional(TranslateToConditionalRequest { transform }).unwrap();
        assert!(rev.lossy);
    }

    // === Schema 门禁（线1, records/77）===

    #[test]
    fn schema_gate_rejects_unknown_op() {
        // 未知比较符会透传成非法 domain type → Schema 门禁应整体拒绝
        let req = TranslateToTransformRequest {
            condition: vec![serde_json::json!({"field":"x","op":"between","value":1})],
            action_set: vec![],
            metadata: None,
        };
        let err = translate_to_transform(req).unwrap_err();
        assert!(
            err.to_string().contains("Schema 门禁"),
            "错误应指明 Schema 门禁: {err}"
        );
    }

    #[test]
    fn schema_gate_rejects_invalid_set_operation() {
        // set operation 超出枚举 (set/add/sub) → Schema 门禁拒绝
        let req = TranslateToTransformRequest {
            condition: vec![],
            action_set: vec![serde_json::json!({"attr":"x","operation":"multiply","value":2})],
            metadata: None,
        };
        let err = translate_to_transform(req).unwrap_err();
        assert!(err.to_string().contains("Schema 门禁"), "错误: {err}");
    }

    // === V1 验证: action_set value (含 role) 端到端保留 ===
    // 验证 businessRuleToTranslateInput 生成的 action_set {attr, operation, value:{role, action}}
    // 经 translate_to_transform → translate_to_conditional 后, value 字段(含 role) 完整保留。
    // 此测试用于排除 "动作角色 role 丢失" 的怀疑: 转译链上 value 是 opaque 透传, role 不会丢。
    // 同时验证 G5 已扩展白名单含 __exec__.result.* (业务动作路径合法)。

    #[test]
    fn action_set_value_with_role_preserved_end_to_end() {
        // 模拟 console businessRuleToTranslateInput 的输出
        let req = TranslateToTransformRequest {
            condition: vec![
                serde_json::json!({"field":"__exec__.payload.amount","op":"gte","value":10000}),
            ],
            action_set: vec![serde_json::json!({
                "attr": "__exec__.result.notify",
                "operation": "set",
                "value": { "role": "CFO", "action": "notify" }
            })],
            metadata: None,
        };
        let fwd = translate_to_transform(req).unwrap();
        // G1-G7 应全过 (含 G5: __exec__.result.* 已加入白名单)
        assert!(
            fwd.g1_g7_pass,
            "业务规则(gte + result.notify) 应通过 G1-G7: {:?}",
            fwd.warnings
        );
        // set 指令的 params.value 应完整保留 {role, action}
        let set_instr = fwd
            .transform
            .iter()
            .find(|r| r.get("type").and_then(|v| v.as_str()) == Some("set"))
            .expect("transform 应含 set 指令");
        // 对齐 exec_set: 用 params.attr (不是 params.path)
        assert_eq!(
            set_instr["params"]["attr"], "__exec__.result.notify",
            "set 指令应用 params.attr 对齐 reactor exec_set"
        );
        let value = &set_instr["params"]["value"];
        assert_eq!(value["role"], "CFO", "transform 阶段 value.role 应保留");
        assert_eq!(
            value["action"], "notify",
            "transform 阶段 value.action 应保留"
        );

        // 回译: translate_to_conditional 应将 set 还原为 action_set, value 完整
        let rev = translate_to_conditional(TranslateToConditionalRequest {
            transform: fwd.transform,
        })
        .unwrap();
        let action = rev
            .action_set
            .iter()
            .find(|a| a.get("attr").and_then(|v| v.as_str()) == Some("__exec__.result.notify"))
            .expect("回译应含 notify 动作");
        assert_eq!(action["value"]["role"], "CFO", "回译后 value.role 应保留");
        assert_eq!(
            action["value"]["action"], "notify",
            "回译后 value.action 应保留"
        );
        // 回译不应 lossy (set + branch(eq/lt/exists/not) 都在子集内)
        assert!(
            !rev.lossy,
            "动作 role 保留时不应 lossy: {:?}",
            rev.lost_items
        );
    }
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `rule_sandbox` 原生实现 —— 沙箱验证 LLM 生成的补丁规则。
//!
//! # SSOT 消除（关键差异 vs Python 基线）
//! Python 版手写了整套静态校验器（VALID_DOMAIN_TYPES / PATH_RE / 指令白名单等），
//! 与固化 schema 存在漂移风险。本实现**直接复用 `evorule-rule-schema` 的
//! `validate_transform_list`**：补丁规则必须是可热加载的 `transform_rule`
//! （type ∈ 6 元指令 set/push/branch/io_request/collect/merge，schema 权威源），
//! 与 submit_command 门禁同一校验器，彻底消除重复定义。
//! 注：Python 版额外放行 sequence/conditional/while_loop 等指令层类型作为补丁，
//! 但指令层非 transform（热加载后会 noop），本实现按 transform_rule 严格校验，更正确。

use evorule_tcb::JsonValue;

use crate::{obj, NativeService};

/// `rule_sandbox` 原生服务
pub struct RuleSandbox;

impl NativeService for RuleSandbox {
    fn execute(&self, args: &JsonValue) -> evorule_reactor::IoResult {
        let candidate = args
            .get("candidate_rule")
            .cloned()
            .unwrap_or(JsonValue::Null);

        let Some(rule) = extract_rule(&candidate) else {
            // 与 Python 基线完全一致：无法提取规则 → rejected
            return Ok(obj(vec![
                ("passed", JsonValue::Bool(false)),
                (
                    "errors",
                    JsonValue::Array(vec![JsonValue::string(
                        "无法从 candidate_rule 中提取规则 JSON（不是有效 JSON 或缺少 suggestion 字段）",
                    )]),
                ),
                ("warnings", JsonValue::Array(vec![])),
                ("verdict", JsonValue::string("rejected")),
                ("rule_summary", JsonValue::Null),
            ]));
        };

        // 复用 submit_command 同一门禁：补丁规则必须是可热加载的 transform_rule（6 元指令）
        let report = evorule_rule_schema::validate_transform_list(&serde_json::Value::Array(
            vec![jsonvalue_to_serde(&rule)],
        ));
        let errors: Vec<String> = report.errors.clone();
        let passed = errors.is_empty();

        Ok(obj(vec![
            ("passed", JsonValue::Bool(passed)),
            (
                "errors",
                JsonValue::Array(errors.into_iter().map(JsonValue::string).collect()),
            ),
            ("warnings", JsonValue::Array(vec![])),
            (
                "verdict",
                JsonValue::string(if passed { "approved" } else { "rejected" }),
            ),
            ("rule_summary", summarize_rule(&rule)),
        ]))
    }
}

// ============================================================================
// extract_rule —— 处理 llm_advisor 的 {suggestion, model} 包装（与 Python 一致）
// ============================================================================

fn extract_rule(candidate: &JsonValue) -> Option<JsonValue> {
    match candidate {
        JsonValue::Null => None,
        JsonValue::Object(m) => {
            // 情况1：llm_advisor 返回格式 {suggestion, model}
            if let Some(suggestion) = m.get("suggestion") {
                match suggestion {
                    JsonValue::String(s) => {
                        if let Some(parsed) = extract_json_object(s) {
                            return Some(parsed);
                        }
                        return None;
                    }
                    JsonValue::Object(_) => return Some(suggestion.clone()),
                    _ => {}
                }
            }
            // 情况2：本身是规则 dict（含 type）
            if m.contains_key("type") {
                return Some(candidate.clone());
            }
            None
        }
        // 情况3：JSON 字符串
        JsonValue::String(s) => extract_json_object(s),
        _ => None,
    }
}

/// 从字符串中提取 JSON 对象：先整体解析，失败则取第一个 `{` 到最后一个 `}` 之间解析
/// （对应 Python `json.loads` 失败后 `re.search(r"\{[\s\S]*\}", ...)` 的行为）。
fn extract_json_object(text: &str) -> Option<JsonValue> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
        if v.is_object() {
            return Some(serde_to_jsonvalue(v));
        }
    }
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    let slice = &text[start..=end];
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(slice) {
        if v.is_object() {
            return Some(serde_to_jsonvalue(v));
        }
    }
    None
}

/// 规则摘要（与 Python `summarize_rule` 一致）
fn summarize_rule(rule: &JsonValue) -> JsonValue {
    let params = rule
        .get("params")
        .cloned()
        .unwrap_or_else(JsonValue::empty_object);
    let domain = params
        .get("domain")
        .cloned()
        .unwrap_or(JsonValue::Null);
    let field_from_domain = |key: &str| match &domain {
        JsonValue::Object(m) => m.get(key).cloned().unwrap_or(JsonValue::Null),
        _ => JsonValue::Null,
    };
    let count = |key: &str| -> i64 {
        params
            .get(key)
            .and_then(|v| v.as_array())
            .map(|a| a.len() as i64)
            .unwrap_or(0)
    };
    obj(vec![
        ("type", rule.get("type").cloned().unwrap_or(JsonValue::Null)),
        ("domain_type", field_from_domain("type")),
        ("instruction_type", field_from_domain("instruction_type")),
        (
            "has_on_true",
            JsonValue::Bool(params.get("on_true").is_some()),
        ),
        (
            "has_on_false",
            JsonValue::Bool(params.get("on_false").is_some()),
        ),
        ("on_true_count", JsonValue::Integer(count("on_true"))),
        ("on_false_count", JsonValue::Integer(count("on_false"))),
    ])
}

// ============================================================================
// JsonValue ↔ serde_json::Value 转换（浮点按 TCB 约定转字符串）
// ============================================================================

fn jsonvalue_to_serde(v: &JsonValue) -> serde_json::Value {
    match v {
        JsonValue::Null => serde_json::Value::Null,
        JsonValue::Bool(b) => serde_json::Value::Bool(*b),
        JsonValue::Integer(i) => serde_json::Value::Number((*i).into()),
        JsonValue::String(s) => serde_json::Value::String(s.to_string()),
        JsonValue::Array(a) => {
            serde_json::Value::Array(a.iter().map(jsonvalue_to_serde).collect())
        }
        JsonValue::Object(m) => serde_json::Value::Object(
            m.iter()
                .map(|(k, v)| (k.clone(), jsonvalue_to_serde(v)))
                .collect(),
        ),
    }
}

fn serde_to_jsonvalue(v: serde_json::Value) -> JsonValue {
    match v {
        serde_json::Value::Null => JsonValue::Null,
        serde_json::Value::Bool(b) => JsonValue::Bool(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                JsonValue::Integer(i)
            } else {
                JsonValue::string(n.to_string())
            }
        }
        serde_json::Value::String(s) => JsonValue::string(s),
        serde_json::Value::Array(arr) => {
            JsonValue::Array(arr.into_iter().map(serde_to_jsonvalue).collect())
        }
        serde_json::Value::Object(o) => {
            let mut m = std::collections::BTreeMap::new();
            for (k, v) in o {
                m.insert(k, serde_to_jsonvalue(v));
            }
            JsonValue::Object(m)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::NativeService;

    #[test]
    fn test_extract_rule_from_llm_wrapper() {
        // llm_advisor 离线 mock 的 suggestion 不是有效 JSON → 无法提取 → None
        let candidate = JsonValue::object_from_pairs(&[
            (
                "suggestion",
                JsonValue::string("[Offline-Mock LLM, model=gpt-4o-mini]\n【建议步骤】\n..."),
            ),
            ("model", JsonValue::string("gpt-4o-mini")),
        ]);
        assert!(extract_rule(&candidate).is_none());
    }

    #[test]
    fn test_extract_rule_from_type_dict() {
        let candidate = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[("type", JsonValue::string("eq"))]),
                    ),
                    ("on_true", JsonValue::Array(vec![])),
                ]),
            ),
        ]);
        let rule = extract_rule(&candidate).expect("dict with type 应直接提取");
        assert_eq!(rule.get("type").and_then(|v| v.as_str()), Some("branch"));
    }

    #[test]
    fn test_sandbox_rejects_offline_mock() {
        // 端到端：离线 mock 建议无法提取 → passed=false（业务测试依赖此行为）
        let svc = RuleSandbox;
        let args = JsonValue::object_from_pairs(&[(
            "candidate_rule",
            JsonValue::object_from_pairs(&[
                (
                    "suggestion",
                    JsonValue::string("这不是 JSON 规则文本，无法解析。"),
                ),
                ("model", JsonValue::string("gpt-4o-mini")),
            ]),
        )]);
        let r = svc.execute(&args).unwrap();
        assert_eq!(r.get("passed").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(r.get("verdict").and_then(|v| v.as_str()), Some("rejected"));
    }

    #[test]
    fn test_sandbox_approves_valid_branch() {
        // 合法 branch 规则 → passed=true（复用 schema 校验器）
        let svc = RuleSandbox;
        let args = JsonValue::object_from_pairs(&[(
            "candidate_rule",
            JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("branch")),
                (
                    "params",
                    JsonValue::object_from_pairs(&[
                        (
                            "domain",
                            JsonValue::object_from_pairs(&[
                                ("type", JsonValue::string("eq")),
                                ("path", JsonValue::string("payload.x")),
                                ("value", JsonValue::Integer(1)),
                            ]),
                        ),
                        (
                            "on_true",
                            JsonValue::Array(vec![JsonValue::object_from_pairs(&[
                                ("type", JsonValue::string("set")),
                                (
                                    "params",
                                    JsonValue::object_from_pairs(&[
                                        ("attr", JsonValue::string("y")),
                                        ("operation", JsonValue::string("set")),
                                        ("value", JsonValue::Integer(2)),
                                    ]),
                                ),
                            ])]),
                        ),
                        ("on_false", JsonValue::Array(vec![])),
                    ]),
                ),
            ]),
        )]);
        let r = svc.execute(&args).unwrap();
        assert_eq!(
            r.get("passed").and_then(|v| v.as_bool()),
            Some(true),
            "合法 branch 应通过: {:?}",
            r.get("errors")
        );
        assert_eq!(r.get("verdict").and_then(|v| v.as_str()), Some("approved"));
    }

    #[test]
    fn test_sandbox_rejects_invalid_meta_instruction() {
        // 非法元指令（save_memory）→ schema 校验拒绝
        let svc = RuleSandbox;
        let args = JsonValue::object_from_pairs(&[(
            "candidate_rule",
            JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("save_memory")),
                ("params", JsonValue::empty_object()),
            ]),
        )]);
        let r = svc.execute(&args).unwrap();
        assert_eq!(r.get("passed").and_then(|v| v.as_bool()), Some(false));
    }
}

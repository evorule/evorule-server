// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `llm_advisor` 原生实现 —— 离线确定性 mock（Phase 3 归位 evo-agent 前的等价物）。
//!
//! 与 Python 基线一致：
//! - `_jinja_substitute` 复刻（`{{ key }}` 替换，值用 Python 兼容 JSON 序列化：`, `/`: ` 分隔、UTF-8 原样）
//! - 离线 mock 建议文本与 Python `_call_llm` 完全一致（含 prompt 前 200 字符截断）
//! - 返回 `{suggestion, model}`

use std::collections::BTreeMap;

use evorule_tcb::JsonValue;

use crate::{arg_str, obj, NativeService};

/// Python `json.dumps(v, ensure_ascii=False)` 兼容序列化（默认分隔符 `, ` 与 `: `）
fn py_json_dumps(v: &JsonValue) -> String {
    match v {
        JsonValue::Null => "null".to_string(),
        JsonValue::Bool(b) => {
            if *b {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        JsonValue::Integer(i) => format!("{i}"),
        JsonValue::String(s) => serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string()),
        JsonValue::Array(a) => {
            let items: Vec<String> = a.iter().map(py_json_dumps).collect();
            format!("[{}]", items.join(", "))
        }
        JsonValue::Object(m) => {
            let items: Vec<String> = m
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{}: {}",
                        serde_json::to_string(k).unwrap_or_else(|_| "\"\"".to_string()),
                        py_json_dumps(v)
                    )
                })
                .collect();
            format!("{{{}}}", items.join(", "))
        }
    }
}

/// Python `_jinja_substitute` 复刻：替换 `{{ key }}`（键两侧允许空白），缺失键保留原样。
fn jinja_substitute(template: &str, variables: &BTreeMap<String, JsonValue>) -> String {
    let chars: Vec<char> = template.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '{' && i + 1 < chars.len() && chars[i + 1] == '{' {
            // 找闭合 }}
            let mut end = None;
            let mut j = i + 2;
            while j + 1 < chars.len() {
                if chars[j] == '}' && chars[j + 1] == '}' {
                    end = Some(j);
                    break;
                }
                j += 1;
            }
            if let Some(end) = end {
                let inner: String = chars[i + 2..end].iter().collect();
                let key = inner.trim();
                match variables.get(key) {
                    Some(val) => out.push_str(&py_json_dumps(val)),
                    None => {
                        out.push_str("{{");
                        out.push_str(&inner);
                        out.push_str("}}");
                    }
                }
                i = end + 2;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// 离线确定性 mock 建议（与 Python `_call_llm` 离线分支一致）
fn offline_mock(prompt: &str, system: &str, model: &str) -> String {
    let truncated: String = prompt.chars().take(200).collect();
    format!(
        "[Offline-Mock LLM, model={model}]\n\
         【系统提示】{system}\n\
         【建议步骤】\n\
         1. 复核输入参数合法性（检查 pending_alert 字段）；\n\
         2. 检查求解器参数 tolerance 是否过于严格；\n\
         3. 切换 solver_type=TRAC-IK 或增大 max_iterations 重试。\n\
         【用户实际 prompt】\n\
         {truncated}..."
    )
}

/// `llm_advisor` 原生服务
pub struct LlmAdvisor;

impl NativeService for LlmAdvisor {
    fn execute(&self, args: &JsonValue) -> evorule_reactor::IoResult {
        let template = arg_str(
            args,
            "prompt_template",
            "告警: {{ alert }}, 输入: {{ snapshot }}，请生成 3 条中文排查建议。",
        );
        let alert = args.get("tpl_alert").cloned().unwrap_or(JsonValue::Null);
        let snapshot = args.get("tpl_snapshot").cloned().unwrap_or(JsonValue::Null);
        let mut variables = BTreeMap::new();
        variables.insert("alert".to_string(), alert);
        variables.insert("snapshot".to_string(), snapshot);
        let prompt = jinja_substitute(&template, &variables);

        let system = arg_str(args, "system", "你是一名资深的机器人系统可靠性工程师。");
        let model = arg_str(args, "model", "gpt-4o-mini");
        let suggestion = offline_mock(&prompt, &system, &model);

        Ok(obj(vec![
            ("suggestion", JsonValue::string(suggestion)),
            ("model", JsonValue::string(model)),
        ]))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::NativeService;

    #[test]
    fn test_py_json_dumps_object_matches_python() {
        // Python: json.dumps({"a": 1, "b": [1, 2], "c": null}, ensure_ascii=False)
        //       = '{"a": 1, "b": [1, 2], "c": null}'
        let v = JsonValue::object_from_pairs(&[
            ("a", JsonValue::Integer(1)),
            (
                "b",
                JsonValue::Array(vec![JsonValue::Integer(1), JsonValue::Integer(2)]),
            ),
            ("c", JsonValue::Null),
        ]);
        assert_eq!(py_json_dumps(&v), r#"{"a": 1, "b": [1, 2], "c": null}"#);
    }

    #[test]
    fn test_jinja_substitute_basic() {
        let template = "告警: {{ alert }}, 输入: {{ snapshot }}。";
        let mut vars = BTreeMap::new();
        vars.insert(
            "alert".to_string(),
            JsonValue::object_from_pairs(&[("error", JsonValue::string("出界"))]),
        );
        vars.insert("snapshot".to_string(), JsonValue::Integer(42));
        let out = jinja_substitute(template, &vars);
        assert_eq!(out, "告警: {\"error\": \"出界\"}, 输入: 42。");
    }

    #[test]
    fn test_offline_advise_shape() {
        let svc = LlmAdvisor;
        let args = JsonValue::object_from_pairs(&[
            ("model", JsonValue::string("gpt-4o-mini")),
            (
                "tpl_alert",
                JsonValue::object_from_pairs(&[("error", JsonValue::string("x"))]),
            ),
        ]);
        let r = svc.execute(&args).unwrap();
        let suggestion = r.get("suggestion").and_then(|v| v.as_str()).unwrap();
        assert!(suggestion.starts_with("[Offline-Mock LLM, model=gpt-4o-mini]"));
        assert!(suggestion.contains("【建议步骤】"));
        assert!(r.get("model").and_then(|v| v.as_str()) == Some("gpt-4o-mini"));
    }
}

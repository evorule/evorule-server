// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 插件服务实现 —— finance_config_get（读）+ finance_config_set（写，需审批）。
//!
//! HTTP 契约（外部插件包调用约定，与 server invoke/registry 管道一致）：
//! 请求 body = args（JSON 对象），响应 = 结果 JSON。进程外执行不影响
//! server 侧审计链（io_request/io_response fact 仍由 server 反应器记录）。

use serde_json::{json, Value};

use crate::store::ConfigStore;

/// finance_config_get —— 读单个配置键。
///
/// fail-fast: 键不存在返回显式错误（附相近键建议），不静默兜底。
pub fn config_get(store: &ConfigStore, args: &Value) -> Value {
    let key = args.get("key").and_then(|v| v.as_str()).unwrap_or("");
    if key.is_empty() {
        return json!({
            "exists": false,
            "error": "CONFIG_KEY_EMPTY",
            "hint": "finance_config_get 必须提供 args.key，例: config:limits.travel.max_amount"
        });
    }

    let cleaned_key = key.strip_prefix("config:").unwrap_or(key);

    match store.get(cleaned_key) {
        Some(value) => json!({
            "exists": true,
            "type": value_type(&value),
            "value": value,
            "source": format!("config:{cleaned_key}")
        }),
        None => {
            let suggestions = suggest_keys(cleaned_key, &store.all_keys());
            json!({
                "exists": false,
                "error": "CONFIG_KEY_NOT_FOUND",
                "key": format!("config:{cleaned_key}"),
                "suggestions": suggestions
            })
        }
    }
}

/// finance_config_set —— 创建配置变更提案（**不直接落库**）。
///
/// 关键约束: 只创建提案；落库须经管理面审批门（approve_proposal）。
pub fn config_set(store: &ConfigStore, args: &Value) -> Value {
    let key = args.get("key").and_then(|v| v.as_str()).unwrap_or("");
    if key.is_empty() {
        return json!({
            "success": false,
            "error": "CONFIG_KEY_EMPTY",
            "hint": "finance_config_set 必须提供 args.key 与 args.new_value"
        });
    }

    let Some(new_value) = args.get("new_value").cloned() else {
        return json!({
            "success": false,
            "error": "CONFIG_NEW_VALUE_MISSING",
            "hint": "finance_config_set 必须提供 args.new_value（任意 JSON）"
        });
    };

    let cleaned_key = key.strip_prefix("config:").unwrap_or(key);
    let reason = args
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("（未说明）");
    let proposed_by = args
        .get("proposed_by")
        .and_then(|v| v.as_str())
        .unwrap_or("system");

    match store.create_proposal(cleaned_key, new_value, reason, proposed_by) {
        Ok(proposal_id) => json!({
            "success": true,
            "proposal_id": proposal_id,
            "awaiting_approval": true,
            "message": format!("提案 {proposal_id} 已创建，等待财务人审批后落库")
        }),
        Err(e) => json!({
            "success": false,
            "error": "CONFIG_PROPOSAL_CREATE_FAILED",
            "detail": e
        }),
    }
}

fn value_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// 模糊匹配：从已有键列表中挑出与 key 前缀最接近的前 5 个。
fn suggest_keys(key: &str, all_keys: &[String]) -> Vec<String> {
    let lower = key.to_lowercase();
    let mut scored: Vec<(usize, &String)> = all_keys
        .iter()
        .filter_map(|k| {
            let kl = k.to_lowercase();
            let score = if kl == lower {
                0
            } else if kl.starts_with(&lower) || lower.starts_with(&kl) {
                1
            } else {
                2
            };
            if score <= 2 { Some((score, k)) } else { None }
        })
        .collect();
    scored.sort_by_key(|(s, _)| *s);
    scored.into_iter().take(5).map(|(_, k)| k.clone()).collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::store::ConfigStore;
    use std::path::PathBuf;

    fn temp_dir() -> PathBuf {
        let tid = format!("{:?}", std::thread::current().id());
        let dir = std::env::temp_dir().join(format!(
            "evorule-finance-config-svc-test-{}-{}",
            std::process::id(),
            tid
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_store() -> (PathBuf, ConfigStore) {
        let dir = temp_dir();
        let path = dir.join("finance-config.json");
        let store = ConfigStore::open(&path).unwrap();
        (path, store)
    }

    #[test]
    fn test_get_empty_key_returns_error() {
        let (_p, store) = make_store();
        let r = config_get(&store, &json!({}));
        assert_eq!(r["exists"], json!(false));
        assert_eq!(r["error"], json!("CONFIG_KEY_EMPTY"));
    }

    #[test]
    fn test_get_missing_key_returns_not_found_with_suggestions() {
        let (_p, store) = make_store();
        let pid = store
            .create_proposal("limits.travel.max_amount", json!(2000), "初始", "setup")
            .unwrap();
        store.approve_proposal(&pid, "setup").unwrap();

        let r = config_get(&store, &json!({"key": "config:limits.meal.per_day"}));
        assert_eq!(r["exists"], json!(false));
        assert_eq!(r["error"], json!("CONFIG_KEY_NOT_FOUND"));
        assert!(!r["suggestions"].as_array().unwrap().is_empty());
    }

    #[test]
    fn test_get_success_after_proposal_approved() {
        let (_p, store) = make_store();
        let pid = store
            .create_proposal("limits.travel.max_amount", json!(2000), "初始", "setup")
            .unwrap();
        store.approve_proposal(&pid, "setup").unwrap();

        let r = config_get(&store, &json!({"key": "config:limits.travel.max_amount"}));
        assert_eq!(r["exists"], json!(true));
        assert_eq!(r["type"], json!("integer"));
        assert_eq!(r["value"], json!(2000));
    }

    #[test]
    fn test_set_creates_proposal_only_does_not_write_value() {
        let (_p, store) = make_store();

        let r = config_set(
            &store,
            &json!({
                "key": "config:limits.travel.max_amount",
                "new_value": 3000,
                "reason": "测试",
                "proposed_by": "user_001"
            }),
        );
        assert_eq!(r["success"], json!(true));
        assert_eq!(r["awaiting_approval"], json!(true));
        let pid = r["proposal_id"].as_str().unwrap().to_string();

        let r2 = config_get(&store, &json!({"key": "config:limits.travel.max_amount"}));
        assert_eq!(r2["exists"], json!(false), "提案创建后值不落库（审批门成立）");

        store.approve_proposal(&pid, "finance_dir").unwrap();
        let r3 = config_get(&store, &json!({"key": "config:limits.travel.max_amount"}));
        assert_eq!(r3["exists"], json!(true));
        assert_eq!(r3["value"], json!(3000));
    }

    #[test]
    fn test_set_missing_new_value_returns_error() {
        let (_p, store) = make_store();
        let r = config_set(&store, &json!({"key": "config:x"}));
        assert_eq!(r["success"], json!(false));
        assert_eq!(r["error"], json!("CONFIG_NEW_VALUE_MISSING"));
    }

    #[test]
    fn test_suggest_keys_fuzzy_match() {
        let pool = vec![
            "limits.travel.max_amount".to_string(),
            "limits.meal.per_day".to_string(),
            "accounts.ledger.tax_rate".to_string(),
            "approvers.travel.chain".to_string(),
        ];
        let r = suggest_keys("limits.travel.max", &pool);
        assert!(r.contains(&"limits.travel.max_amount".to_string()));
    }
}

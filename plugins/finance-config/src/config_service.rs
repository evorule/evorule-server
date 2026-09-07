// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! finance-config 原生服务实现 —— config_get（读） + config_set（写，需审批）。

use evorule_plugin_kit::NativeService;
use evorule_reactor::IoResult;
use evorule_tcb::JsonValue;
use serde_json::Value;
use std::sync::Arc;

use crate::store::ConfigStore;

// ============================================================================
// 工具函数（与 demo-services lib.rs 同构）
// ============================================================================

pub(crate) fn obj(pairs: Vec<(&str, JsonValue)>) -> JsonValue {
    JsonValue::object_from_pairs(&pairs)
}

pub(crate) fn arg_str(v: &JsonValue, key: &str, default: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| default.to_string())
}

// ============================================================================
// finance_config_get —— 读路径
// ============================================================================

/// inance_config_get 原生服务：读单个配置键。
///
/// fail-fast（H 约束）: 键不存在返回显式错误，**不静默兜底**。
pub struct ConfigGetService {
    pub store: Arc<ConfigStore>,
}

impl NativeService for ConfigGetService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let key = arg_str(args, "key", "");
        if key.is_empty() {
            return Ok(obj(vec![
                ("exists", JsonValue::Bool(false)),
                ("error", JsonValue::string("CONFIG_KEY_EMPTY")),
                ("hint", JsonValue::string(
                    "finance_config_get 必须提供 args.key，例: config:limits.travel.max_amount",
                )),
            ]));
        }

        let cleaned_key = key
            .strip_prefix("config:")
            .unwrap_or(&key)
            .to_string();

        match self.store.get(&cleaned_key) {
            Some(value) => {
                let type_str = value_type(&value);
                Ok(obj(vec![
                    ("exists", JsonValue::Bool(true)),
                    ("type", JsonValue::string(type_str)),
                    ("value", json_to_tcb(&value)),
                    ("source", JsonValue::string(format!("config:{cleaned_key}"))),
                ]))
            }
            None => {
                let suggestions = suggest_keys(&cleaned_key, &self.store.all_keys());
                let sugg_list: Vec<JsonValue> = suggestions
                    .into_iter()
                    .map(JsonValue::string)
                    .collect();
                Ok(obj(vec![
                    ("exists", JsonValue::Bool(false)),
                    ("error", JsonValue::string("CONFIG_KEY_NOT_FOUND")),
                    ("key", JsonValue::string(format!("config:{cleaned_key}"))),
                    ("suggestions", JsonValue::Array(sugg_list)),
                ]))
            }
        }
    }
}

// ============================================================================
// finance_config_set —— 写路径（创建提案，不落库）
// ============================================================================

/// inance_config_set 原生服务：创建配置变更提案。
///
/// 关键约束: 本服务**只创建提案，不直接落库**。落库须经审批门（approve_proposal）。
pub struct ConfigSetService {
    pub store: Arc<ConfigStore>,
}

impl NativeService for ConfigSetService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let key = arg_str(args, "key", "");
        if key.is_empty() {
            return Ok(obj(vec![
                ("success", JsonValue::Bool(false)),
                ("error", JsonValue::string("CONFIG_KEY_EMPTY")),
                ("hint", JsonValue::string(
                    "finance_config_set 必须提供 args.key 与 args.new_value",
                )),
            ]));
        }

        let cleaned_key = key
            .strip_prefix("config:")
            .unwrap_or(&key)
            .to_string();

        let new_value_tcb = match args.get("new_value") {
            Some(v) => v.clone(),
            None => {
                return Ok(obj(vec![
                    ("success", JsonValue::Bool(false)),
                    ("error", JsonValue::string("CONFIG_NEW_VALUE_MISSING")),
                ]));
            }
        };

        let reason = arg_str(args, "reason", "（未说明）");
        let proposed_by = arg_str(args, "proposed_by", "system");

        let new_value_json = tcb_to_json(&new_value_tcb);

        let proposal_id = match self.store.create_proposal(
            &cleaned_key,
            new_value_json,
            &reason,
            &proposed_by,
        ) {
            Ok(id) => id,
            Err(e) => {
                return Ok(obj(vec![
                    ("success", JsonValue::Bool(false)),
                    ("error", JsonValue::string("CONFIG_PROPOSAL_CREATE_FAILED")),
                    ("detail", JsonValue::string(e)),
                ]));
            }
        };

        Ok(obj(vec![
            ("success", JsonValue::Bool(true)),
            ("proposal_id", JsonValue::string(proposal_id.clone())),
            ("awaiting_approval", JsonValue::Bool(true)),
            ("message", JsonValue::string(format!(
                "提案 {proposal_id} 已创建，等待财务人审批后落库"
            ))),
        ]))
    }
}

// ============================================================================
// JsonValue 转换辅助
// ============================================================================

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

/// serde_json::Value → TCB JsonValue（TCB 无 Float，浮点数转字符串）。
fn json_to_tcb(v: &Value) -> JsonValue {
    match v {
        Value::Null => JsonValue::Null,
        Value::Bool(b) => JsonValue::Bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                JsonValue::Integer(i)
            } else {
                JsonValue::string(n.to_string())
            }
        }
        Value::String(s) => JsonValue::string(s),
        Value::Array(arr) => {
            let list: Vec<JsonValue> = arr.iter().map(json_to_tcb).collect();
            JsonValue::Array(list)
        }
        Value::Object(obj) => {
            let pairs: Vec<(&str, JsonValue)> = obj
                .iter()
                .map(|(k, v)| (k.as_str(), json_to_tcb(v)))
                .collect();
            JsonValue::object_from_pairs(&pairs)
        }
    }
}

/// TCB JsonValue → serde_json::Value（反向）。
fn tcb_to_json(v: &JsonValue) -> Value {
    match v {
        JsonValue::Null => Value::Null,
        JsonValue::Bool(b) => Value::Bool(*b),
        JsonValue::Integer(i) => Value::Number((*i).into()),
        JsonValue::String(s) => Value::String(s.as_ref().to_string()),
        JsonValue::Array(arr) => Value::Array(arr.iter().map(tcb_to_json).collect()),
        JsonValue::Object(obj) => {
            let mut map = serde_json::Map::new();
            for (k, v) in obj {
                map.insert(k.clone(), tcb_to_json(v));
            }
            Value::Object(map)
        }
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

// ============================================================================
// NativeServiceDef 构造子（供 lib.rs 调用）
// ============================================================================

pub fn make_get(store: Arc<ConfigStore>) -> Arc<dyn NativeService> {
    Arc::new(ConfigGetService { store })
}

pub fn make_set(store: Arc<ConfigStore>) -> Arc<dyn NativeService> {
    Arc::new(ConfigSetService { store })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::store::ConfigStore;

    fn temp_dir() -> std::path::PathBuf {
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

    fn make_store() -> Arc<ConfigStore> {
        let dir = temp_dir();
        let path = dir.join("finance-config.json");
        Arc::new(ConfigStore::open(&path).unwrap())
    }

    fn params_of(args: JsonValue) -> JsonValue {
        JsonValue::object_from_pairs(&[("args", args)])
    }

    #[test]
    fn test_get_empty_key_returns_error() {
        let store = make_store();
        let svc = ConfigGetService { store };
        let r = svc.execute(&JsonValue::object_from_pairs(&[])).unwrap();
        assert_eq!(r.get("exists").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(
            r.get("error").and_then(|v| v.as_str()),
            Some("CONFIG_KEY_EMPTY")
        );
    }

    #[test]
    fn test_get_missing_key_returns_not_found_with_suggestions() {
        let store = make_store();
        let pid = store
            .create_proposal(
                "limits.travel.max_amount",
                Value::from(2000),
                "初始",
                "setup",
            )
            .unwrap();
        store.approve_proposal(&pid, "setup").unwrap();

        let svc = ConfigGetService { store: store.clone() };
        let args = JsonValue::object_from_pairs(&[(
            "key",
            JsonValue::string("config:limits.meal.per_day"),
        )]);
        let r = svc.execute(&args).unwrap();
        assert_eq!(r.get("exists").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(
            r.get("error").and_then(|v| v.as_str()),
            Some("CONFIG_KEY_NOT_FOUND")
        );
        let sugg = r
            .get("suggestions")
            .and_then(|v| v.as_array())
            .unwrap();
        assert!(!sugg.is_empty());
    }

    #[test]
    fn test_get_success_after_proposal_approved() {
        let store = make_store();
        let pid = store
            .create_proposal(
                "limits.travel.max_amount",
                Value::from(2000),
                "初始",
                "setup",
            )
            .unwrap();
        store.approve_proposal(&pid, "setup").unwrap();

        let svc = ConfigGetService { store: store.clone() };
        let args = JsonValue::object_from_pairs(&[(
            "key",
            JsonValue::string("config:limits.travel.max_amount"),
        )]);
        let r = svc.execute(&args).unwrap();
        assert_eq!(r.get("exists").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(r.get("type").and_then(|v| v.as_str()), Some("integer"));
        assert_eq!(r.get("value").and_then(|v| v.as_i64()), Some(2000));
    }

    #[test]
    fn test_set_creates_proposal_only_does_not_write_value() {
        let store = make_store();
        let svc = ConfigSetService { store: store.clone() };

        let args = JsonValue::object_from_pairs(&[
            ("key", JsonValue::string("config:limits.travel.max_amount")),
            ("new_value", JsonValue::Integer(3000)),
            ("reason", JsonValue::string("测试")),
            ("proposed_by", JsonValue::string("user_001")),
        ]);
        let r = svc.execute(&args).unwrap();
        assert_eq!(r.get("success").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            r.get("awaiting_approval").and_then(|v| v.as_bool()),
            Some(true)
        );
        let pid = r
            .get("proposal_id")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_string();

        let get_svc = ConfigGetService { store: store.clone() };
        let get_args = JsonValue::object_from_pairs(&[(
            "key",
            JsonValue::string("config:limits.travel.max_amount"),
        )]);
        let r2 = get_svc.execute(&get_args).unwrap();
        assert_eq!(r2.get("exists").and_then(|v| v.as_bool()), Some(false));

        store.approve_proposal(&pid, "finance_dir").unwrap();
        let r3 = get_svc.execute(&get_args).unwrap();
        assert_eq!(r3.get("exists").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(r3.get("value").and_then(|v| v.as_i64()), Some(3000));
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
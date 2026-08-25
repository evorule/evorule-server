// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `config_persist` 原生实现 —— 热加载补丁规则的 mock。
//!
//! 与 Python 基线一致：接收 `operation` 与 `rule`，返回 `{success, message, rule_type, persisted}`。
//! mock 不实际修改运行时配置（真实实现走 evorule-server 热加载 API，属后续阶段）。

use evorule_tcb::JsonValue;

use crate::{arg_str, obj, NativeService};

/// `config_persist` 原生服务
pub struct ConfigPersist;

impl NativeService for ConfigPersist {
    fn execute(&self, args: &JsonValue) -> evorule_reactor::IoResult {
        let operation = arg_str(args, "operation", "append_transform");
        let rule_type = match args.get("rule") {
            Some(JsonValue::Object(m)) => m
                .get("type")
                .cloned()
                .unwrap_or(JsonValue::Null),
            _ => JsonValue::Null,
        };
        Ok(obj(vec![
            ("success", JsonValue::Bool(true)),
            (
                "message",
                JsonValue::string(format!("操作 '{operation}' 已执行（mock）")),
            ),
            ("rule_type", rule_type),
            ("persisted", JsonValue::Bool(true)),
        ]))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::NativeService;

    #[test]
    fn test_persist_with_rule_type() {
        let svc = ConfigPersist;
        let args = JsonValue::object_from_pairs(&[
            ("operation", JsonValue::string("append_transform")),
            (
                "rule",
                JsonValue::object_from_pairs(&[("type", JsonValue::string("branch"))]),
            ),
        ]);
        let r = svc.execute(&args).unwrap();
        assert_eq!(r.get("success").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(r.get("rule_type").and_then(|v| v.as_str()), Some("branch"));
        assert_eq!(r.get("persisted").and_then(|v| v.as_bool()), Some(true));
    }

    #[test]
    fn test_persist_without_rule() {
        let svc = ConfigPersist;
        let args = JsonValue::object_from_pairs(&[(
            "operation",
            JsonValue::string("replace_all"),
        )]);
        let r = svc.execute(&args).unwrap();
        assert!(r.get("rule_type").map(|v| v.is_null()).unwrap_or(false));
        assert!(r.get("success").and_then(|v| v.as_bool()).unwrap());
    }
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! SG-2 路由策略资产加载（server 侧薄接缝）
//!
//! 资产 = rules_dir 根目录 [`ROUTE_POLICY_FILE`]（gitignore 管辖不进 commit，
//! 与 SG-5 规则资产同纪律；governance 侧 `RoutePolicy` 测试为 schema 权威）。
//!
//! # 装配语义（io_guard 特征表同形态）
//!
//! 缺失 / 解析失败 → `None`（SG-2 静态收紧层不装配，动态权限表权威不变，
//! 行为与 SG-1 之前一致）+ warn 每进程一次（透明性，禁止静默通过）。
//! 加载成功 → info 留痕规则数，deny/ask 闸门即生效（allow 不产生放行权）。
//!
//! 三处 gate 装配点（主干 main.rs + 会话级两处）经 [`shared_route_policy`]
//! 共享同一份进程级缓存（首次装配点触发读盘，部署资产不随 reload 变更）。

use std::path::Path;
use std::sync::{Arc, OnceLock};

use evorule_governance::permission::RoutePolicy;

/// 资产文件名（rules_dir 根目录正本）
pub const ROUTE_POLICY_FILE: &str = "00_constraint_route_policy.json";

static SHARED: OnceLock<Option<Arc<RoutePolicy>>> = OnceLock::new();
static MISSING_WARNED: OnceLock<()> = OnceLock::new();
static PARSE_WARNED: OnceLock<()> = OnceLock::new();

/// 进程级共享加载（首次调用读盘并缓存；三处 gate 装配点共用同一份）
pub fn shared_route_policy(rules_dir: &Path) -> Option<Arc<RoutePolicy>> {
    SHARED.get_or_init(|| load_route_policy(rules_dir)).clone()
}

/// 单次加载（公开以便测试；生产装配走 [`shared_route_policy`]）
pub fn load_route_policy(rules_dir: &Path) -> Option<Arc<RoutePolicy>> {
    let path = rules_dir.join(ROUTE_POLICY_FILE);
    let raw = match std::fs::read_to_string(&path) {
        Ok(r) => r,
        Err(_) => {
            if MISSING_WARNED.set(()).is_ok() {
                tracing::warn!(
                    target: "route_policy",
                    path = %path.display(),
                    "路由策略资产未部署，SG-2 静态收紧层不装配（动态权限表权威不变）"
                );
            }
            return None;
        }
    };
    match RoutePolicy::from_json_str(&raw) {
        Ok(p) => {
            tracing::info!(
                target: "route_policy",
                path = %path.display(),
                rules = p.rules.len(),
                "路由策略资产已加载（SG-2 静态收紧层：deny/ask 闸门生效；allow 不产生放行权）"
            );
            Some(Arc::new(p))
        }
        Err(e) => {
            if PARSE_WARNED.set(()).is_ok() {
                tracing::warn!(
                    target: "route_policy",
                    path = %path.display(),
                    error = %e,
                    "路由策略资产解析失败，SG-2 静态收紧层不装配（动态权限表权威不变）"
                );
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::fs;

    /// 唯一临时目录（进程内唯一 id 后缀）
    fn tmp_rules_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sg2_route_policy_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn valid_asset_loads_with_rules() {
        let dir = tmp_rules_dir("valid");
        fs::write(
            dir.join(ROUTE_POLICY_FILE),
            r#"{"version":1,"rules":[{"caller_role":"llm","io_type":"http_get","target":"*","decision":"deny"}]}"#,
        )
        .unwrap();
        let policy = load_route_policy(&dir).expect("合法资产必须加载成功");
        assert_eq!(policy.rules.len(), 1);
        assert_eq!(policy.rules[0].decision, evorule_governance::permission::RouteDecision::Deny);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_asset_returns_none() {
        let dir = tmp_rules_dir("missing");
        assert!(load_route_policy(&dir).is_none(), "未部署必须返回 None（不装配）");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_asset_returns_none() {
        let dir = tmp_rules_dir("corrupt");
        fs::write(dir.join(ROUTE_POLICY_FILE), "not json").unwrap();
        assert!(load_route_policy(&dir).is_none(), "解析失败必须返回 None（fail-closed 拒绝坏资产）");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn invalid_semantics_return_none() {
        let dir = tmp_rules_dir("invalid");
        // ask_fallback.unattended=allow = 静默放行 LLM，必须整体拒绝
        fs::write(
            dir.join(ROUTE_POLICY_FILE),
            r#"{"version":1,"ask_fallback":{"unattended":"allow"}}"#,
        )
        .unwrap();
        assert!(load_route_policy(&dir).is_none());
        fs::remove_dir_all(&dir).ok();
    }
}

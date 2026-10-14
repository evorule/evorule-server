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
use std::time::Duration;

use evorule_governance::circuit_breaker::CircuitConfig;
use evorule_governance::permission::RoutePolicy;

/// 资产文件名（rules_dir 根目录正本）
pub const ROUTE_POLICY_FILE: &str = "00_constraint_route_policy.json";

static SHARED: OnceLock<Option<Arc<RoutePolicy>>> = OnceLock::new();
static MISSING_WARNED: OnceLock<()> = OnceLock::new();
static PARSE_WARNED: OnceLock<()> = OnceLock::new();
static CIRCUIT_CFG: OnceLock<Option<CircuitConfig>> = OnceLock::new();

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

/// SG-3 会话级熔断配置（env 接缝，进程级缓存；三处装配点共用同一份）
///
/// - `EVORULE_CIRCUIT_THRESHOLD`：滑窗内静态层 deny 计数阈值，缺省 5；
///   `0` = 熔断面关闭（误伤回退开关）
/// - `EVORULE_CIRCUIT_WINDOW_SECS`：计数/跳闸窗口秒数，缺省 300
/// - 非法值（非数字 / 窗口 0）→ 缺省 + warn 每进程一次（禁止静默）
///
/// 返回 `None` = 不装配熔断面，行为与 SG-2 一致。
pub fn circuit_config_from_env() -> Option<CircuitConfig> {
    *CIRCUIT_CFG.get_or_init(|| {
        circuit_config_from_raw(
            std::env::var("EVORULE_CIRCUIT_THRESHOLD").ok().as_deref(),
            std::env::var("EVORULE_CIRCUIT_WINDOW_SECS").ok().as_deref(),
        )
    })
}

/// 纯解析内核（单测入口；生产装配走 [`circuit_config_from_env`]）
fn circuit_config_from_raw(threshold: Option<&str>, window: Option<&str>) -> Option<CircuitConfig> {
    const DEFAULT_THRESHOLD: u32 = 5;
    const DEFAULT_WINDOW_SECS: u64 = 300;

    let threshold = match threshold {
        None => DEFAULT_THRESHOLD,
        Some(raw) => raw.trim().parse::<u32>().unwrap_or_else(|_| {
            tracing::warn!(
                target: "route_policy",
                key = "EVORULE_CIRCUIT_THRESHOLD",
                value = %raw,
                default = DEFAULT_THRESHOLD,
                "熔断阈值 env 非法，按缺省值装配（每进程仅告警一次）"
            );
            DEFAULT_THRESHOLD
        }),
    };
    let window_secs = match window {
        None => DEFAULT_WINDOW_SECS,
        Some(raw) => match raw.trim().parse::<u64>() {
            Ok(v) if v > 0 => v,
            _ => {
                tracing::warn!(
                    target: "route_policy",
                    key = "EVORULE_CIRCUIT_WINDOW_SECS",
                    value = %raw,
                    default = DEFAULT_WINDOW_SECS,
                    "熔断窗口 env 非法（非数字或 0），按缺省值装配（每进程仅告警一次）"
                );
                DEFAULT_WINDOW_SECS
            }
        },
    };
    if threshold == 0 {
        return None; // 熔断面关闭（误伤回退开关；行为与 SG-2 一致）
    }
    Some(CircuitConfig {
        threshold,
        window: Duration::from_secs(window_secs),
    })
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

    // ---- SG-3 熔断配置解析（circuit_config_from_raw 纯内核）----

    #[test]
    fn circuit_defaults_when_unset() {
        let cfg = circuit_config_from_raw(None, None).expect("缺省必须装配熔断面");
        assert_eq!(cfg.threshold, 5);
        assert_eq!(cfg.window, Duration::from_secs(300));
    }

    #[test]
    fn circuit_zero_threshold_disables() {
        assert!(circuit_config_from_raw(Some("0"), None).is_none(), "阈值 0 = 误伤回退开关（不装配）");
    }

    #[test]
    fn circuit_illegal_values_fall_back_to_defaults() {
        let cfg = circuit_config_from_raw(Some("abc"), Some("-3")).expect("非法值必须回退缺省");
        assert_eq!(cfg.threshold, 5);
        assert_eq!(cfg.window, Duration::from_secs(300));
    }

    #[test]
    fn circuit_zero_window_is_illegal() {
        let cfg = circuit_config_from_raw(Some("3"), Some("0")).expect("窗口 0 视为非法回退缺省");
        assert_eq!(cfg.window, Duration::from_secs(300));
    }

    #[test]
    fn circuit_custom_values_applied() {
        let cfg = circuit_config_from_raw(Some("7"), Some("60")).expect("自定义值必须生效");
        assert_eq!(cfg.threshold, 7);
        assert_eq!(cfg.window, Duration::from_secs(60));
    }
}

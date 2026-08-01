// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 认证中间件（应用层）
//!
//! 提供基于 token 的简单认证，用于 HTTP API 访问控制。
//!
//! H6 架构合规整改：从 evorule-governance/src/api/auth.rs 迁移到应用层。
//! 移除了 `#[cfg(feature = "auth")]` 门控（应用层默认启用认证）。
//!
//! # P1-5 安全加固
//! - 使用 `subtle::ConstantTimeEq` 做恒定时间比较，防止时序攻击
//! - 支持 Token 轮换：`current_tokens` + `previous_tokens` 双 token 并存过渡
//! - `validate()` 遍历所有 token，不因匹配到就提前返回，避免枚举攻击

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use std::sync::Arc;
use subtle::ConstantTimeEq;

/// 认证配置
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// 当前合法 token 列表
    current_tokens: Arc<Vec<String>>,
    /// 上轮轮换前的 token 列表（过渡期仍可使用，用于无缝轮换）
    previous_tokens: Arc<Vec<String>>,
    /// 是否启用认证（false 时跳过检查）
    enabled: bool,
}

impl AuthConfig {
    /// 创建新认证配置
    ///
    /// - `tokens`：合法 token 列表（设为 current_tokens，previous_tokens 为空）
    /// - `enabled`：是否启用认证
    pub fn new(tokens: Vec<String>, enabled: bool) -> Self {
        Self {
            current_tokens: Arc::new(tokens),
            previous_tokens: Arc::new(Vec::new()),
            enabled,
        }
    }

    /// 禁用认证（开发模式）
    pub fn disabled() -> Self {
        Self {
            current_tokens: Arc::new(Vec::new()),
            previous_tokens: Arc::new(Vec::new()),
            enabled: false,
        }
    }

    /// 轮换 token：将 current_tokens 移入 previous_tokens，设置新的 current_tokens
    ///
    /// 轮换后，旧 token 在 `previous_tokens` 中仍可使用（过渡期），
    /// 客户端可在任意时间切换到新 token，实现无缝轮换。
    ///
    /// 再次轮换时，旧的 `previous_tokens` 会被丢弃（仅保留一轮过渡）。
    #[allow(dead_code)]
    pub fn rotate_tokens(&self, new_tokens: Vec<String>) -> Self {
        Self {
            current_tokens: Arc::new(new_tokens),
            previous_tokens: self.current_tokens.clone(),
            enabled: self.enabled,
        }
    }

    /// 恒定时间比较两个字符串是否相等
    ///
    /// 长度不同时仍执行比较以避免长度信息泄露（虽然 token 长度通常固定），
    /// 内容比较使用 `subtle::ConstantTimeEq` 确保恒定时间。
    fn ct_eq(a: &str, b: &str) -> bool {
        let a_bytes = a.as_bytes();
        let b_bytes = b.as_bytes();
        // 长度不同：比较 a 与自身（消耗相同时间），然后返回 false
        if a_bytes.len() != b_bytes.len() {
            let _ = a_bytes.ct_eq(a_bytes);
            return false;
        }
        bool::from(a_bytes.ct_eq(b_bytes))
    }

    /// 验证 token 是否合法
    ///
    /// 遍历 `current_tokens` 和 `previous_tokens` 中的所有 token，
    /// 使用恒定时间比较，且不因匹配到就提前返回（防止通过时序枚举有效 token）。
    pub fn validate(&self, token: &str) -> bool {
        if !self.enabled {
            return true;
        }
        let mut found = false;
        // 检查当前 token 列表（全部比较，不提前退出）
        for t in self.current_tokens.iter() {
            if Self::ct_eq(token, t) {
                found = true;
            }
        }
        // 检查上一轮 token 列表（全部比较，不提前退出）
        for t in self.previous_tokens.iter() {
            if Self::ct_eq(token, t) {
                found = true;
            }
        }
        found
    }
}

/// 从请求头提取 Bearer token
fn extract_bearer_token(req: &Request) -> Option<String> {
    req.headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string())
}

/// Axum 认证中间件
///
/// 从 `Authorization: Bearer <token>` 头提取 token，
/// 验证是否在合法 token 列表中。
pub async fn auth_middleware(
    State(auth_config): State<AuthConfig>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if !auth_config.enabled {
        return Ok(next.run(req).await);
    }

    let token = extract_bearer_token(&req).ok_or(StatusCode::UNAUTHORIZED)?;

    if auth_config.validate(&token) {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::panic, clippy::expect_used)]
    use super::*;

    #[test]
    fn test_disabled_auth_allows_all() {
        let config = AuthConfig::disabled();
        assert!(config.validate("anything"));
        assert!(config.validate(""));
    }

    #[test]
    fn test_enabled_auth_validates_token() {
        let config = AuthConfig::new(vec!["secret123".to_string()], true);
        assert!(config.validate("secret123"));
        assert!(!config.validate("wrong"));
        assert!(!config.validate(""));
    }

    #[test]
    fn test_enabled_with_empty_tokens_rejects_all() {
        let config = AuthConfig::new(vec![], true);
        assert!(!config.validate("anything"));
    }

    #[test]
    fn test_ct_eq_equal_strings() {
        assert!(AuthConfig::ct_eq("hello", "hello"));
        assert!(AuthConfig::ct_eq("", ""));
    }

    #[test]
    fn test_ct_eq_different_strings() {
        assert!(!AuthConfig::ct_eq("hello", "world"));
        assert!(!AuthConfig::ct_eq("hello", "hello!"));
        assert!(!AuthConfig::ct_eq("hello", ""));
    }

    #[test]
    fn test_multiple_tokens() {
        let config = AuthConfig::new(
            vec![
                "token_a".to_string(),
                "token_b".to_string(),
                "token_c".to_string(),
            ],
            true,
        );
        assert!(config.validate("token_a"));
        assert!(config.validate("token_b"));
        assert!(config.validate("token_c"));
        assert!(!config.validate("token_d"));
    }

    #[test]
    fn test_token_rotation_current_still_valid() {
        let config = AuthConfig::new(vec!["old_token".to_string()], true);
        let rotated = config.rotate_tokens(vec!["new_token".to_string()]);
        // 新 token 有效
        assert!(rotated.validate("new_token"));
        // 旧 token 仍在 previous_tokens 中有效（过渡期）
        assert!(rotated.validate("old_token"));
        // 无关 token 无效
        assert!(!rotated.validate("wrong_token"));
    }

    #[test]
    fn test_token_rotation_double_rotate_drops_oldest() {
        let config = AuthConfig::new(vec!["v1_token".to_string()], true);
        let rotated1 = config.rotate_tokens(vec!["v2_token".to_string()]);
        let rotated2 = rotated1.rotate_tokens(vec!["v3_token".to_string()]);

        // v3 是 current
        assert!(rotated2.validate("v3_token"));
        // v2 在 previous 中（v1 轮换前的 current）
        assert!(rotated2.validate("v2_token"));
        // v1 已被丢弃（仅保留一轮过渡）
        assert!(!rotated2.validate("v1_token"));
    }

    #[test]
    fn test_rotation_preserves_disabled_state() {
        let config = AuthConfig::disabled();
        let rotated = config.rotate_tokens(vec!["new_token".to_string()]);
        // 禁用状态下轮换后仍禁用
        assert!(rotated.validate("anything"));
        assert!(rotated.validate("new_token"));
    }
}

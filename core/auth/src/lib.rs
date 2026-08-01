// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 认证服务
//!
//! 提供 Bearer Token 的生成、验证、增删与列举。
//!
//! # 安全设计
//! - Token 比较一律走 `subtle::ConstantTimeEq`,避免 timing 侧信道。
//! - `list_tokens` 只返回**掩码** token(`TokenInfoMasked`),杜绝明文通过列举接口泄露
//!   (HTTP `/tokens` 端点本身未鉴权,返回明文即漏洞)。
//! - Mutex 采用 poison-tolerant 模式:某线程持锁 panic 时,后续线程仍可访问数据,
//!   避免认证服务因单点 panic 整体不可用。

#![forbid(unsafe_code)]
// C5 (unwrap/expect/panic = deny) 仅约束生产代码;测试代码保留 unwrap 惯例
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use subtle::ConstantTimeEq;
use tracing::info;

/// Token 掩码前缀保留长度
const MASK_PREFIX_LEN: usize = 8;
/// Token 掩码后缀保留长度
const MASK_SUFFIX_LEN: usize = 4;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenInfo {
    pub token: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthResponse {
    pub valid: bool,
    pub message: String,
    pub token_info: Option<TokenInfo>,
}

/// 掩码后的 Token 信息(用于列举接口,不暴露明文)
///
/// `token` 字段形如 `"a1b2c3d4..wxyz"`(前 8 + 后 4),其余字段保留。
/// 仅供 `list_tokens` 返回;需要明文 Token 的场景应通过 `generate_token` /
/// `add_token` 的返回值获取(调用方即持有者)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenInfoMasked {
    pub token: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub description: String,
}

impl TokenInfo {
    /// 返回掩码后的 token 字符串:前 8 位 + `..` + 后 4 位。
    ///
    /// Token 长度不足 `(PREFIX+SUFFIX)` 时退化为全 `*` 掩码,绝不泄露明文。
    fn masked_token(&self) -> String {
        let bytes = self.token.as_bytes();
        if bytes.len() < MASK_PREFIX_LEN + MASK_SUFFIX_LEN {
            return "*".repeat(bytes.len().max(1));
        }
        let prefix = &self.token[..MASK_PREFIX_LEN];
        let suffix = &self.token[self.token.len() - MASK_SUFFIX_LEN..];
        format!("{prefix}..{suffix}")
    }

    /// 转为掩码视图
    pub fn masked(&self) -> TokenInfoMasked {
        TokenInfoMasked {
            token: self.masked_token(),
            created_at: self.created_at,
            expires_at: self.expires_at,
            description: self.description.clone(),
        }
    }
}

pub struct AuthService {
    tokens: Arc<Mutex<Vec<TokenInfo>>>,
}

impl AuthService {
    pub fn new(tokens: Vec<TokenInfo>) -> Self {
        Self {
            tokens: Arc::new(Mutex::new(tokens)),
        }
    }

    pub fn validate_token(&self, token: &str) -> AuthResponse {
        // poison-tolerant: 持锁线程 panic 后仍继续服务,认证不应因单点 panic 全停
        let tokens = self.tokens.lock().unwrap_or_else(|e| e.into_inner());

        for token_info in tokens.iter() {
            let eq = token.as_bytes().ct_eq(token_info.token.as_bytes());
            if bool::from(eq) {
                if let Some(expires_at) = token_info.expires_at {
                    if expires_at < Utc::now() {
                        return AuthResponse {
                            valid: false,
                            message: "Token 已过期".to_string(),
                            token_info: None,
                        };
                    }
                }

                return AuthResponse {
                    valid: true,
                    message: "Token 验证成功".to_string(),
                    token_info: Some(token_info.clone()),
                };
            }
        }

        AuthResponse {
            valid: false,
            message: "无效的 Token".to_string(),
            token_info: None,
        }
    }

    pub fn add_token(&self, token: TokenInfo) -> Result<(), String> {
        let mut tokens = self.tokens.lock().unwrap_or_else(|e| e.into_inner());

        // 恒定时间比较:与 validate_token 保持一致,避免 timing 侧信道
        for existing in tokens.iter() {
            let eq = existing.token.as_bytes().ct_eq(token.token.as_bytes());
            if bool::from(eq) {
                return Err("Token 已存在".to_string());
            }
        }

        let description = token.description.clone();
        tokens.push(token);
        info!(token = %description, "添加新 Token");
        Ok(())
    }

    pub fn remove_token(&self, token: &str) -> bool {
        let mut tokens = self.tokens.lock().unwrap_or_else(|e| e.into_inner());
        let original_len = tokens.len();
        tokens.retain(|t| {
            let eq = t.token.as_bytes().ct_eq(token.as_bytes());
            !bool::from(eq)
        });
        let removed = tokens.len() < original_len;

        if removed {
            info!("删除 Token");
        }

        removed
    }

    /// 列举所有 Token(**掩码**版本)。
    ///
    /// 返回 `TokenInfoMasked`,token 字段为 `"<前8>..<后4>"` 形式。
    /// 列举接口不应暴露明文 —— 需要明文请走 `generate_token`/`add_token` 的返回值。
    pub fn list_tokens(&self) -> Vec<TokenInfoMasked> {
        let tokens = self.tokens.lock().unwrap_or_else(|e| e.into_inner());
        tokens.iter().map(TokenInfo::masked).collect()
    }

    pub fn generate_token(description: &str, expires_hours: Option<u64>) -> TokenInfo {
        let token = Self::random_token();
        let created_at = Utc::now();
        let expires_at = expires_hours.map(|h| created_at + chrono::Duration::hours(h as i64));

        TokenInfo {
            token,
            created_at,
            expires_at,
            description: description.to_string(),
        }
    }

    fn random_token() -> String {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let bytes: Vec<u8> = (0..32).map(|_| rng.gen()).collect();
        hex::encode(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::sync::Arc;
    use std::thread;

    fn make_token(token: &str, expires_at: Option<chrono::DateTime<Utc>>) -> TokenInfo {
        TokenInfo {
            token: token.to_string(),
            created_at: Utc::now(),
            expires_at,
            description: "test token".to_string(),
        }
    }

    // ===== validate_token =====

    #[test]
    fn validate_valid_token_succeeds() {
        let svc = AuthService::new(vec![make_token("secret-abc-123", None)]);
        let resp = svc.validate_token("secret-abc-123");
        assert!(resp.valid);
        assert_eq!(resp.message, "Token 验证成功");
        assert!(resp.token_info.is_some());
    }

    #[test]
    fn validate_expired_token_fails() {
        let past = Utc::now() - chrono::Duration::hours(1);
        let svc = AuthService::new(vec![make_token("expired-token", Some(past))]);
        let resp = svc.validate_token("expired-token");
        assert!(!resp.valid);
        assert_eq!(resp.message, "Token 已过期");
        assert!(resp.token_info.is_none());
    }

    #[test]
    fn validate_invalid_token_fails() {
        let svc = AuthService::new(vec![make_token("real-token", None)]);
        let resp = svc.validate_token("wrong-token");
        assert!(!resp.valid);
        assert_eq!(resp.message, "无效的 Token");
        assert!(resp.token_info.is_none());
    }

    #[test]
    fn validate_empty_store_fails() {
        let svc = AuthService::new(vec![]);
        let resp = svc.validate_token("anything");
        assert!(!resp.valid);
    }

    #[test]
    fn validate_non_expired_token_with_future_expiry_succeeds() {
        let future = Utc::now() + chrono::Duration::hours(1);
        let svc = AuthService::new(vec![make_token("future-token", Some(future))]);
        let resp = svc.validate_token("future-token");
        assert!(resp.valid);
    }

    // ===== add_token =====

    #[test]
    fn add_new_token_succeeds() {
        let svc = AuthService::new(vec![]);
        let result = svc.add_token(make_token("new-token", None));
        assert!(result.is_ok());
        assert!(svc.validate_token("new-token").valid);
    }

    #[test]
    fn add_duplicate_token_errors() {
        let svc = AuthService::new(vec![make_token("dup-token", None)]);
        let result = svc.add_token(make_token("dup-token", None));
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Token 已存在");
    }

    // ===== remove_token =====

    #[test]
    fn remove_existing_token_returns_true() {
        let svc = AuthService::new(vec![make_token("rm-token", None)]);
        assert!(svc.remove_token("rm-token"));
        // 删除后应验证失败
        assert!(!svc.validate_token("rm-token").valid);
    }

    #[test]
    fn remove_missing_token_returns_false() {
        let svc = AuthService::new(vec![make_token("keep-token", None)]);
        assert!(!svc.remove_token("no-such-token"));
        // 保留的 token 仍有效
        assert!(svc.validate_token("keep-token").valid);
    }

    // ===== list_tokens (掩码) =====

    #[test]
    fn list_tokens_returns_masked_not_plaintext() {
        let svc = AuthService::new(vec![make_token("a1b2c3d4e5f6g7h8i9j0k1l2m3n4o5p6", None)]);
        let listed = svc.list_tokens();
        assert_eq!(listed.len(), 1);
        let masked = &listed[0].token;
        // 前缀 + .. + 后缀
        assert!(masked.starts_with("a1b2c3d4"));
        assert!(masked.ends_with("o5p6"));
        assert!(masked.contains(".."));
        // 绝不能包含完整明文
        assert!(!masked.contains("e5f6g7h8i9j0k1l2m3n4"));
    }

    #[test]
    fn list_tokens_short_token_fully_masked() {
        // 短于 PREFIX+SUFFIX 的 token 退化为全 *
        let svc = AuthService::new(vec![make_token("abc", None)]);
        let listed = svc.list_tokens();
        assert_eq!(listed[0].token, "***");
    }

    #[test]
    fn list_tokens_preserves_metadata() {
        let future = Utc::now() + chrono::Duration::hours(2);
        let svc = AuthService::new(vec![make_token(
            "aaaaaaaa1111bbbb2222cccc3333dddd",
            Some(future),
        )]);
        let listed = svc.list_tokens();
        assert_eq!(listed[0].description, "test token");
        assert!(listed[0].expires_at.is_some());
    }

    // ===== generate_token =====

    #[test]
    fn generate_token_is_hex_64_chars() {
        let t = AuthService::generate_token("desc", None);
        // 32 bytes hex = 64 chars
        assert_eq!(t.token.len(), 64);
        assert!(t.token.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(t.expires_at.is_none());
    }

    #[test]
    fn generate_token_with_expiry_sets_future() {
        let t = AuthService::generate_token("desc", Some(24));
        let expiry = t.expires_at.expect("expiry should be set");
        assert!(expiry > Utc::now());
        // 大约 24 小时后(允许 1 分钟误差)
        let diff = expiry - Utc::now();
        assert!(diff.num_minutes() >= 1439 && diff.num_minutes() <= 1441);
    }

    #[test]
    fn generate_token_produces_unique_tokens() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..100 {
            let t = AuthService::generate_token("desc", None);
            assert!(seen.insert(t.token), "generated duplicate token");
        }
    }

    // ===== 并发安全 =====

    #[test]
    fn concurrent_add_and_validate_is_safe() {
        let svc = Arc::new(AuthService::new(vec![]));
        let mut handles = vec![];

        for i in 0..8 {
            let svc_clone = Arc::clone(&svc);
            handles.push(thread::spawn(move || {
                let token = format!("thread-token-{i}");
                let _ = svc_clone.add_token(make_token(&token, None));
                // 并发验证:自己加的应能验证通过
                assert!(svc_clone.validate_token(&token).valid);
            }));
        }

        for h in handles {
            h.join().expect("thread panicked");
        }

        // 最终应有 8 个 token
        assert_eq!(svc.list_tokens().len(), 8);
    }

    #[test]
    fn concurrent_validate_no_panic() {
        let svc = Arc::new(AuthService::new(vec![make_token("shared-token", None)]));
        let mut handles = vec![];

        for _ in 0..16 {
            let svc_clone = Arc::clone(&svc);
            handles.push(thread::spawn(move || {
                // 大量并发只读,不应 panic
                for _ in 0..100 {
                    let _ = svc_clone.validate_token("shared-token");
                }
            }));
        }

        for h in handles {
            h.join().expect("thread panicked");
        }
    }
}

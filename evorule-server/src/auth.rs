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
//! # 安全加固
//! - 使用 `subtle::ConstantTimeEq` 做恒定时间比较，防止时序攻击
//! - 支持 Token 轮换：`current_tokens` + `previous_tokens` 双 token 并存过渡
//! - `validate()` 遍历所有 token，不因匹配到就提前返回，避免枚举攻击
//!
//! UV-017 W2b：HTTP 中间件职责已移交 `api::platform_auth::unified_auth_middleware`
//! （双凭据：静态 token 或平台会话），本模块只保留凭据模型（AuthConfig /
//! CallerIdentity）与受保护域判定（requires_service_identity）。

use std::sync::Arc;
use subtle::ConstantTimeEq;
use tracing::warn;

/// 认证配置
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// 当前合法 token 列表
    current_tokens: Arc<Vec<String>>,
    /// 上轮轮换前的 token 列表（过渡期仍可使用，用于无缝轮换）
    previous_tokens: Arc<Vec<String>>,
    /// B5-server：受信服务管道 token 列表（service 身份，可写受保护域）
    ///
    /// 与 user token 权限差异仅在受保护域写入（`shared.*.stable.llm.*` /
    /// `stable.system.*`）：service 可写，user 拒绝。其余 API 权限等同。
    current_service_tokens: Arc<Vec<String>>,
    /// 是否启用认证（false 时跳过检查）
    enabled: bool,
}

/// B5-server：调用方身份（凭据分层）
///
/// 由 [`AuthConfig::identity`] 按凭据归属判定；认证禁用时中间件不注入
/// 身份，handler 侧按放行处理（开发模式语义不变）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallerIdentity {
    /// 普通 token（禁止写受保护域）
    User,
    /// 受信服务管道 token（evo-agent 等内部组件）
    Service,
}

impl AuthConfig {
    /// 创建新认证配置
    ///
    /// - `tokens`：合法 token 列表（设为 current_tokens，previous_tokens 为空）
    /// - `enabled`：是否启用认证
    ///
    /// N1 修复：过滤空字符串 token。`ct_eq("", "")` 返回 true，
    /// 若不过滤，攻击者发送 `Authorization: Bearer `（空 token）即可通过认证。
    pub fn new(tokens: Vec<String>, enabled: bool) -> Self {
        let filtered: Vec<String> = tokens.into_iter().filter(|t| !t.is_empty()).collect();
        if enabled && filtered.is_empty() {
            warn!("AuthConfig::new() 启用认证但无有效 token（全部为空或未提供），所有请求将被拒绝");
        }
        Self {
            current_tokens: Arc::new(filtered),
            previous_tokens: Arc::new(Vec::new()),
            current_service_tokens: Arc::new(Vec::new()),
            enabled,
        }
    }

    /// B5-server：设置受信服务管道 token（builder 风格）
    ///
    /// 空 token 过滤规则与 [`Self::new`] 一致。service token 列表可独立于
    /// user token 配置（如 `EVORULE_SERVICE_TOKEN` env）。
    pub fn with_service_tokens(mut self, tokens: Vec<String>) -> Self {
        let filtered: Vec<String> = tokens.into_iter().filter(|t| !t.is_empty()).collect();
        self.current_service_tokens = Arc::new(filtered);
        self
    }

    /// 禁用认证（开发模式）
    pub fn disabled() -> Self {
        Self {
            current_tokens: Arc::new(Vec::new()),
            previous_tokens: Arc::new(Vec::new()),
            current_service_tokens: Arc::new(Vec::new()),
            enabled: false,
        }
    }

    /// 是否启用认证（UV-017 W2b：统一中间件据此决定放行/校验）
    pub fn is_enabled(&self) -> bool {
        self.enabled
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
            current_service_tokens: self.current_service_tokens.clone(),
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

    /// 验证 token 是否合法（user token 或 service token 均可通过认证）
    ///
    /// 遍历所有 token 列表（user current/previous + service current），
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
        // B5-server：service token 同样可通过认证（身份区分在 identity()）
        for t in self.current_service_tokens.iter() {
            if Self::ct_eq(token, t) {
                found = true;
            }
        }
        found
    }

    /// B5-server：判定 token 的调用方身份
    ///
    /// service 列表优先匹配（token 同时出现在两个列表时按 Service 处理，
    /// 权限取并集语义）；未匹配到任何列表时返回 User（调用方必须先过
    /// [`Self::validate`]，此处仅做身份归类）。
    pub fn identity(&self, token: &str) -> CallerIdentity {
        if !self.enabled {
            return CallerIdentity::User;
        }
        for t in self.current_service_tokens.iter() {
            if Self::ct_eq(token, t) {
                return CallerIdentity::Service;
            }
        }
        CallerIdentity::User
    }
}

/// B5-server：路径是否属于受保护域（仅 service 身份可写）
///
/// 匹配任意 namespace 下 `stable.llm.*` / `stable.system.*` 段序列，
/// 如 `shared.{ns}.stable.llm.{model}.{key}`。`shared.` 前缀之外的路径
/// 不受保护（session 内部事实无跨会话伪造面）。
///
/// 注：`stable.llm` / `stable.system` 段序列在 shared 空间为保留段——
/// 用户自定义 key 不应包含该序列（准入拒绝时错误信息已给出指引）。
pub fn requires_service_identity(path: &str) -> bool {
    if !path.starts_with("shared.") {
        return false;
    }
    let segs: Vec<&str> = path.split('.').collect();
    segs.windows(2).any(|w| {
        w[0] == "stable" && (w[1] == "llm" || w[1] == "system")
    })
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

    // --- B5-server：service token / 身份解析 / 受保护域路径校验 ---

    #[test]
    fn test_service_token_passes_validate() {
        let config =
            AuthConfig::new(vec!["user_token".to_string()], true)
                .with_service_tokens(vec!["service_token".to_string()]);
        assert!(config.validate("user_token"));
        assert!(config.validate("service_token"));
        assert!(!config.validate("wrong"));
    }

    #[test]
    fn test_identity_user_for_regular_token() {
        let config =
            AuthConfig::new(vec!["user_token".to_string()], true)
                .with_service_tokens(vec!["service_token".to_string()]);
        assert_eq!(config.identity("user_token"), CallerIdentity::User);
        assert_eq!(config.identity("service_token"), CallerIdentity::Service);
        // service 列表优先：token 同时出现在两个列表时按 Service（权限并集语义）
        let overlap = AuthConfig::new(vec!["tok".to_string()], true)
            .with_service_tokens(vec!["tok".to_string()]);
        assert_eq!(overlap.identity("tok"), CallerIdentity::Service);
    }

    #[test]
    fn test_identity_disabled_returns_user() {
        let config = AuthConfig::disabled()
            .with_service_tokens(vec!["service_token".to_string()]);
        assert_eq!(config.identity("service_token"), CallerIdentity::User);
    }

    #[test]
    fn test_with_service_tokens_filters_empty() {
        let config = AuthConfig::new(vec!["user_token".to_string()], true)
            .with_service_tokens(vec![String::new(), "service_token".to_string()]);
        // 空 token 不进列表（与 new() 的 N1 修复规则一致）
        assert!(!config.validate(""));
        assert!(config.validate("service_token"));
    }

    #[test]
    fn test_rotation_preserves_service_tokens() {
        let config = AuthConfig::new(vec!["old".to_string()], true)
            .with_service_tokens(vec!["service_token".to_string()]);
        let rotated = config.rotate_tokens(vec!["new".to_string()]);
        assert!(rotated.validate("service_token"));
        assert_eq!(rotated.identity("service_token"), CallerIdentity::Service);
    }

    #[test]
    fn test_requires_service_identity_protected_paths() {
        // 任意 namespace 下的 stable.llm / stable.system 段序列均受保护
        assert!(requires_service_identity("shared.default.stable.llm.gpt-4o.summary"));
        assert!(requires_service_identity("shared.default.stable.system.pipeline"));
        assert!(requires_service_identity("shared.ns1.stable.llm.x"));
        // 深层嵌套也命中
        assert!(requires_service_identity("shared.default.a.stable.system.b.c"));
    }

    #[test]
    fn test_requires_service_identity_non_protected_paths() {
        // 非 shared 前缀不受保护（session 内部事实无跨会话伪造面）
        assert!(!requires_service_identity("stable.llm.gpt-4o.summary"));
        assert!(!requires_service_identity("session.stable.llm.x"));
        // shared 空间的用户自定义路径不受保护
        assert!(!requires_service_identity("shared.default.user.notes"));
        assert!(!requires_service_identity("shared.default.stable.user.notes"));
        // stable 后面不是 llm/system
        assert!(!requires_service_identity("shared.default.stable.public.x"));
        // 非段边界（stable.llmx 是单个段，不拆分匹配）
        assert!(!requires_service_identity("shared.default.stable.llmx"));
        assert!(!requires_service_identity("shared.default.stablellm.x"));
    }
}

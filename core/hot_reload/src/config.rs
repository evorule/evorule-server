// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 热重载配置

/// 热重载配置
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct HotReloadConfig {
    /// 监控的规则目录
    pub rules_dir: String,
    /// evorule-server 的地址
    pub evorule_server_url: String,
    /// 会话 ID（为空时创建新会话）
    pub session_id: Option<u64>,
    /// 轮询间隔（毫秒）
    pub poll_interval_ms: u64,
    /// 是否自动启动
    pub auto_start: bool,
}

impl Default for HotReloadConfig {
    fn default() -> Self {
        Self {
            rules_dir: "./rules".to_string(),
            evorule_server_url: "http://127.0.0.1:18080".to_string(),
            session_id: None,
            poll_interval_ms: 1000,
            auto_start: true,
        }
    }
}

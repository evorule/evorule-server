// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! U7 SSE session_switched 推送 — 滚动 session 切换通知
//!
//! 设计依据: PUBLISH_QUEUE_DESIGN.md §5 (P3)
//!           三层架构 §12.4 U7 决策
//!
//! # 设计
//! - 每个 session 有一个 broadcast channel
//! - SSE 订阅者 (GET /api/sessions/{id}/events) 在连接时订阅该 channel
//! - 滚动 session 切换时,向旧 session 的 channel 发送 session_switched 事件
//! - SSE handler 收到事件后推送给客户端,客户端据此切换到新 session
//!
//! # 与客户端的交互 (三层架构 §12.4 U7)
//! 客户端收到 session_switched 后:
//! 1. 关闭旧 EventSource (SSE)
//! 2. 用 new_session_id 订阅新 SSE
//! 3. 更新 productionStateStore.currentSessionId
//! 4. 触发 MonitorDashboard 重新渲染

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{broadcast, Mutex};
use tracing::{debug, info};

use crate::error::WorkspaceResult;

/// session_switched 事件 payload
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionSwitchedEvent {
    /// 固定 "session_switched"
    pub event_type: String,
    pub old_session_id: u64,
    pub new_session_id: u64,
    pub new_ruleset_version: i64,
    pub new_ruleset_hash: String,
    pub timestamp: String,
}

/// session_switched 广播器
///
/// 管理 session_id → broadcast::Sender 的映射。
/// SSE handler 在连接时调用 `register()` 获取 Receiver,
/// 滚动切换时调用 `broadcast_switched()` 向旧 session 的订阅者推送通知。
#[derive(Clone)]
pub struct SessionSwitchedBroadcaster {
    channels: Arc<Mutex<HashMap<u64, broadcast::Sender<SessionSwitchedEvent>>>>,
}

impl Default for SessionSwitchedBroadcaster {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionSwitchedBroadcaster {
    pub fn new() -> Self {
        Self {
            channels: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 为 session 注册广播通道 (SSE 连接时调用)
    ///
    /// 如果 session 已有通道,返回同一通道的新 Receiver;
    /// 否则创建新通道 (容量 16,足够缓冲多个 SSE 事件)。
    pub async fn register(&self, session_id: u64) -> broadcast::Receiver<SessionSwitchedEvent> {
        let mut channels = self.channels.lock().await;
        let sender = channels
            .entry(session_id)
            .or_insert_with(|| broadcast::channel(16).0)
            .clone();
        sender.subscribe()
    }

    /// 向旧 session 的 SSE 订阅者推送 session_switched 事件
    ///
    /// 如果旧 session 没有注册通道或没有活跃订阅者,仅记录 debug 日志 (非错误)。
    pub async fn broadcast_switched(
        &self,
        old_session_id: u64,
        new_session_id: u64,
        new_ruleset_version: i64,
        new_ruleset_hash: &str,
    ) -> WorkspaceResult<()> {
        let event = SessionSwitchedEvent {
            event_type: "session_switched".to_string(),
            old_session_id,
            new_session_id,
            new_ruleset_version,
            new_ruleset_hash: new_ruleset_hash.to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        let channels = self.channels.lock().await;
        if let Some(sender) = channels.get(&old_session_id) {
            match sender.send(event.clone()) {
                Ok(n) => {
                    info!(
                        old_session_id = old_session_id,
                        new_session_id = new_session_id,
                        subscribers_notified = n,
                        "session_switched event broadcasted"
                    );
                }
                Err(_) => {
                    debug!(
                        old_session_id = old_session_id,
                        "No active SSE subscribers for session_switched"
                    );
                }
            }
        } else {
            debug!(
                old_session_id = old_session_id,
                "No broadcast channel registered for session"
            );
        }

        Ok(())
    }

    /// 通知旧 session 已关闭 (清理通道)
    pub async fn notify_session_closed(&self, session_id: u64) {
        let mut channels = self.channels.lock().await;
        channels.remove(&session_id);
        debug!(session_id = session_id, "Broadcast channel cleaned up");
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[tokio::test]
    async fn test_broadcast_to_subscriber() {
        let switcher = SessionSwitchedBroadcaster::new();
        let mut rx = switcher.register(100).await;

        switcher
            .broadcast_switched(100, 200, 5, "hash123")
            .await
            .unwrap();

        let event = rx.recv().await.unwrap();
        assert_eq!(event.event_type, "session_switched");
        assert_eq!(event.old_session_id, 100);
        assert_eq!(event.new_session_id, 200);
        assert_eq!(event.new_ruleset_version, 5);
        assert_eq!(event.new_ruleset_hash, "hash123");
    }

    #[tokio::test]
    async fn test_broadcast_no_subscriber() {
        let switcher = SessionSwitchedBroadcaster::new();
        // 没有注册过,应该返回 Ok (非错误)
        let result = switcher.broadcast_switched(999, 1000, 1, "h").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_notify_session_closed() {
        let switcher = SessionSwitchedBroadcaster::new();
        let _rx = switcher.register(42).await;

        switcher.notify_session_closed(42).await;

        // 再次注册应该创建新通道
        let _rx2 = switcher.register(42).await;
    }

    #[tokio::test]
    async fn test_multiple_subscribers() {
        let switcher = SessionSwitchedBroadcaster::new();
        let mut rx1 = switcher.register(50).await;
        let mut rx2 = switcher.register(50).await;

        switcher
            .broadcast_switched(50, 60, 2, "hash")
            .await
            .unwrap();

        let e1 = rx1.recv().await.unwrap();
        let e2 = rx2.recv().await.unwrap();
        assert_eq!(e1.new_session_id, 60);
        assert_eq!(e2.new_session_id, 60);
    }
}

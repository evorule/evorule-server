// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 合成 IO 响应器 — Sandbox session 的 io_request 自动回调
//!
//! 设计依据: SANDBOX_ORCHESTRATION_DESIGN.md §4 (S2)
//!           三层架构 §3.6.3 (合成 IO 响应器)
//!
//! # 设计
//! - L1 Production: IoSubscriber 分发到真实 handler (DB/HTTP/Memory)
//! - L3 Sandbox: MockIoResponder 返回合成响应 (零生产 IO 风险)
//!
//! # P0 策略
//! 所有 io_request 返回成功合成响应,不调真实 DB/HTTP。
//! Draft 规则纯计算不触发 io_request 时,MockIoResponder 仅作占位。
//! 若触发 io_request,通过 HTTP 轮询 pending_io + 回调 io_response (P0 简化)。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::info;

/// 合成 IO 响应器
///
/// 监听 sandbox session 的 io_request,根据 io_type 返回合成响应。
/// P0 全合成数据 (Q2 决策),不接生产 IO。
pub struct MockIoResponder {
    session_id: u64,
    running: Arc<AtomicBool>,
    /// 合成响应规则 (io_type → 响应模板)
    response_templates: Arc<Mutex<HashMap<String, serde_json::Value>>>,
}

impl MockIoResponder {
    /// 创建新响应器,内置 6 种 io_type 的默认合成响应
    pub fn new(session_id: u64) -> Self {
        let mut templates = HashMap::new();

        // 覆盖 evorule-tcb 的 5 种 IoType + 通用 fallback
        templates.insert(
            "query_db".into(),
            serde_json::json!({"rows": [], "affected": 0, "status": "ok"}),
        );
        templates.insert(
            "call_service".into(),
            serde_json::json!({"result": "mock_success", "status": 200}),
        );
        templates.insert(
            "call_external".into(),
            serde_json::json!({"result": "mock_success", "status": 200}),
        );
        templates.insert(
            "http_get".into(),
            serde_json::json!({"result": "mock_success", "status": 200}),
        );
        templates.insert(
            "save_memory".into(),
            serde_json::json!({"value": null, "status": "ok"}),
        );

        Self {
            session_id,
            running: Arc::new(AtomicBool::new(false)),
            response_templates: Arc::new(Mutex::new(templates)),
        }
    }

    /// 启动合成 IO 响应器
    ///
    /// P0 简化方案: sandbox 测试不产生 io_request (Draft 规则纯计算),
    /// 或通过 HTTP 轮询 `/api/sessions/{id}/debug/pending_io` + 回调
    /// `/api/sessions/{id}/io_response`。
    /// 实际轮询逻辑在 evorule-server 集成时实现 (需访问 SessionApi 内部通道)。
    pub async fn start(&self) {
        self.running.store(true, Ordering::SeqCst);
        info!(
            session_id = self.session_id,
            "MockIoResponder started for sandbox session"
        );
    }

    /// 停止合成 IO 响应器
    pub async fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        info!(session_id = self.session_id, "MockIoResponder stopped");
    }

    /// 设置自定义合成响应 (按 io_type)
    pub async fn set_response(&self, io_type: &str, response: serde_json::Value) {
        self.response_templates
            .lock()
            .await
            .insert(io_type.to_string(), response);
    }

    /// 获取合成响应 (按 io_type,未配置时返回通用 mock)
    pub async fn get_response(&self, io_type: &str) -> serde_json::Value {
        self.response_templates
            .lock()
            .await
            .get(io_type)
            .cloned()
            .unwrap_or_else(|| serde_json::json!({"status": "mock", "result": null}))
    }

    /// 是否正在运行
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// 获取关联的 session_id
    pub fn session_id(&self) -> u64 {
        self.session_id
    }
}

/// 活跃 MockIoResponder 注册表 (session_id → responder)
///
/// 由 SandboxService 持有,管理多个并行沙盒的响应器生命周期。
pub type MockResponderRegistry = Arc<Mutex<HashMap<u64, MockIoResponder>>>;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[tokio::test]
    async fn test_default_responses() {
        let responder = MockIoResponder::new(42);
        assert_eq!(responder.session_id(), 42);
        assert!(!responder.is_running());

        responder.start().await;
        assert!(responder.is_running());

        let db_resp = responder.get_response("query_db").await;
        assert_eq!(db_resp["status"], "ok");

        let unknown_resp = responder.get_response("unknown_type").await;
        assert_eq!(unknown_resp["status"], "mock");

        responder.stop().await;
        assert!(!responder.is_running());
    }

    #[tokio::test]
    async fn test_custom_response() {
        let responder = MockIoResponder::new(1);
        responder
            .set_response(
                "query_db",
                serde_json::json!({"rows": [{"id": 1}], "custom": true}),
            )
            .await;

        let resp = responder.get_response("query_db").await;
        assert_eq!(resp["custom"], true);
        assert_eq!(resp["rows"][0]["id"], 1);
    }

    #[tokio::test]
    async fn test_registry() {
        let registry: MockResponderRegistry = Arc::new(Mutex::new(HashMap::new()));
        let r1 = MockIoResponder::new(100);
        let r2 = MockIoResponder::new(200);

        registry.lock().await.insert(100, r1);
        registry.lock().await.insert(200, r2);

        assert_eq!(registry.lock().await.len(), 2);

        let removed = registry.lock().await.remove(&100);
        assert!(removed.is_some());
        assert_eq!(registry.lock().await.len(), 1);
    }
}

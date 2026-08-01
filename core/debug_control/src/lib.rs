// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! EvoRule 调试控制 —— 基于 interrupt + rewind 的伪单步调试
//!
//! 从 evorule-reactor 移出的 debug_control 在应用层实现。
//! 采用方案 A：基于 `interrupt()` + FactsLog rewind 实现伪单步，
//! 不修改 tier1 核心。
//!
//! # 设计原理
//!
//! evorule 的反应器是事件驱动的（非持续运行），传统 pause/resume/step
//! 需要适配：
//! - **interrupt**：调用 evorule-server 的 `/interrupt` 端点，请求反应器退出
//! - **step（回放模式）**：基于 time_machine rewind，查看任意历史版本状态
//! - **watch**：轮询 evorule-server 状态，检测 version 变化并推送通知
//!
//! # 端点
//!
//! - `GET  /api/sessions/{id}/debug/status` — 调试状态聚合
//! - `POST /api/sessions/{id}/debug/interrupt` — 中断反应器
//! - `POST /api/sessions/{id}/debug/step` — 单步回放（body: `{"version": N}`）
//! - `GET  /api/sessions/{id}/debug/snapshot` — 当前快照（rewind 当前版本）
//! - `POST /api/sessions/{id}/debug/pause` — 暂停 watch 轮询
//! - `POST /api/sessions/{id}/debug/resume` — 恢复 watch 轮询
//! - `GET  /api/sessions/{id}/debug/watch?interval_ms=` — SSE 状态变化流
//!
//! # 设计说明
//!
//! - **伪单步语义**：evorule 反应器是事件驱动的，没有真正的"暂停/单步执行"，
//!   本模块通过 `interrupt` 中断当前事件循环 + `rewind` 回放任意历史版本，
//!   提供"伪单步"调试体验。
//! - **`get_snapshot` 双往返**：先调 `/state` 取当前 version，再调 `/rewind/{v}`
//!   取快照，共 2 次 HTTP 往返。未来若 evorule-server 提供 `/snapshot` 单端点
//!   可优化为 1 次。
//! - **watch 后台任务**：`watch_handler` 启动独立 tokio 任务轮询上游状态，
//!   客户端断开时 `tx.send` 失败自动退出。`pause`/`resume` 控制轮询循环是否
//!   跳过状态查询（不停止任务本身）。
//! - **mutex 毒化降级**：`paused`/`last_version` 用 `std::sync::Mutex` 保护，
//!   毒化时不 panic，降级访问受污染数据并记录 error 日志。

#![forbid(unsafe_code)]
// C5 (unwrap/expect/panic = deny) 仅约束生产代码;测试代码保留 unwrap 惯例
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use axum::{
    extract::Path,
    extract::Query,
    extract::State,
    http::StatusCode,
    response::sse::Event,
    response::{IntoResponse, Json, Sse},
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use tracing::info;

/// 调试状态聚合
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DebugStatus {
    pub session_id: u64,
    pub phase: String,
    pub version: u64,
    pub causal_depth: i64,
    pub is_finished: bool,
    pub invariant_violations: i64,
    pub pending_io_count: i64,
}

/// 单步回放请求
#[derive(Debug, Deserialize)]
pub struct StepRequest {
    pub version: u64,
}

/// 单步回放响应
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepResponse {
    pub session_id: u64,
    pub target_version: u64,
    pub actual_version: u64,
    pub payload: serde_json::Value,
    pub queue: Vec<serde_json::Value>,
}

/// watch 查询参数
#[derive(Debug, Deserialize)]
pub struct WatchQuery {
    #[serde(default = "default_interval")]
    pub interval_ms: u64,
}

fn default_interval() -> u64 {
    1000
}

/// 调试控制服务
#[derive(Clone)]
pub struct DebugControlService {
    evorule_server_url: Arc<String>,
    client: reqwest::Client,
    /// 会话暂停状态（session_id -> is_paused）
    paused: Arc<Mutex<HashMap<u64, bool>>>,
    /// 会话上次观察到的 version（用于 watch 变化检测）
    last_version: Arc<Mutex<HashMap<u64, u64>>>,
}

impl DebugControlService {
    pub fn new(evorule_server_url: String) -> Self {
        Self {
            evorule_server_url: Arc::new(evorule_server_url),
            client: reqwest::Client::new(),
            paused: Arc::new(Mutex::new(HashMap::new())),
            last_version: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 获取调试状态聚合
    pub async fn get_status(&self, session_id: u64) -> Result<DebugStatus, String> {
        let url = format!(
            "{}/api/sessions/{}/state",
            self.evorule_server_url, session_id
        );
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("请求 state 失败: {}", e))?;
        if !resp.status().is_success() {
            return Err(format!("state 端点返回 {}", resp.status()));
        }
        let state: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("解析 state 失败: {}", e))?;

        let reactor = state.get("reactor").ok_or("缺少 reactor 字段")?;
        let version = state.get("version").and_then(|v| v.as_u64()).unwrap_or(0);

        Ok(DebugStatus {
            session_id,
            phase: reactor
                .get("phase")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
            version,
            causal_depth: reactor
                .get("causal_depth")
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
            is_finished: reactor
                .get("phase")
                .and_then(|v| v.as_str())
                .map(|p| p == "finished")
                .unwrap_or(false),
            invariant_violations: reactor
                .get("invariant_violations")
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
            pending_io_count: reactor
                .get("pending_io_count")
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
        })
    }

    /// 中断反应器
    pub async fn interrupt(&self, session_id: u64) -> Result<(), String> {
        let url = format!(
            "{}/api/sessions/{}/interrupt",
            self.evorule_server_url, session_id
        );
        let resp = self
            .client
            .post(&url)
            .send()
            .await
            .map_err(|e| format!("请求 interrupt 失败: {}", e))?;
        if !resp.status().is_success() {
            return Err(format!("interrupt 端点返回 {}", resp.status()));
        }
        Ok(())
    }

    /// 单步回放（rewind 到指定版本）
    pub async fn step(&self, session_id: u64, target_version: u64) -> Result<StepResponse, String> {
        let url = format!(
            "{}/api/sessions/{}/rewind/{}",
            self.evorule_server_url, session_id, target_version
        );
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("请求 rewind 失败: {}", e))?;
        if !resp.status().is_success() {
            return Err(format!("rewind 端点返回 {}", resp.status()));
        }
        let snap: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("解析 rewind 失败: {}", e))?;

        Ok(StepResponse {
            session_id,
            target_version,
            actual_version: snap
                .get("version")
                .and_then(|v| v.as_u64())
                .unwrap_or(target_version),
            payload: snap
                .get("payload")
                .cloned()
                .unwrap_or(serde_json::json!({})),
            queue: snap
                .get("queue")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default(),
        })
    }

    /// 获取当前快照
    pub async fn get_snapshot(&self, session_id: u64) -> Result<serde_json::Value, String> {
        let status = self.get_status(session_id).await?;
        let step = self.step(session_id, status.version).await?;
        Ok(serde_json::json!({
            "session_id": step.session_id,
            "version": step.actual_version,
            "payload": step.payload,
            "queue": step.queue,
            "phase": status.phase,
            "is_finished": status.is_finished,
        }))
    }

    /// 检查会话是否暂停
    ///
    /// mutex 毒化时降级为未暂停（避免 panic 中断服务），并在日志中记录。
    pub fn is_paused(&self, session_id: u64) -> bool {
        self.lock_paused()
            .get(&session_id)
            .copied()
            .unwrap_or(false)
    }

    /// 设置暂停状态
    pub fn set_paused(&self, session_id: u64, paused: bool) {
        self.lock_paused().insert(session_id, paused);
    }

    /// 获取上次观察到的 version
    pub fn get_last_version(&self, session_id: u64) -> Option<u64> {
        self.lock_last_version().get(&session_id).copied()
    }

    /// 更新上次观察到的 version
    pub fn set_last_version(&self, session_id: u64, version: u64) {
        self.lock_last_version().insert(session_id, version);
    }

    /// 锁定 paused map，毒化时取回内部数据继续访问（不 panic）
    fn lock_paused(&self) -> std::sync::MutexGuard<'_, HashMap<u64, bool>> {
        match self.paused.lock() {
            Ok(g) => g,
            Err(p) => {
                tracing::error!("paused mutex 毒化,降级访问受污染数据");
                p.into_inner()
            }
        }
    }

    /// 锁定 last_version map，毒化时取回内部数据继续访问（不 panic）
    fn lock_last_version(&self) -> std::sync::MutexGuard<'_, HashMap<u64, u64>> {
        match self.last_version.lock() {
            Ok(g) => g,
            Err(p) => {
                tracing::error!("last_version mutex 毒化,降级访问受污染数据");
                p.into_inner()
            }
        }
    }
}

// ============ HTTP API ============

/// handler 错误类型：状态码 + 错误消息，避免 `String: IntoResponse` 默认 200。
type HandlerError = (StatusCode, String);

/// 把服务层 `Err(String)` 映射为 `500 Internal Server Error`
fn map_err(e: String) -> HandlerError {
    (StatusCode::INTERNAL_SERVER_ERROR, e)
}

async fn status_handler(
    State(svc): State<DebugControlService>,
    Path(session_id): Path<u64>,
) -> Result<Json<DebugStatus>, HandlerError> {
    let status = svc.get_status(session_id).await.map_err(map_err)?;
    Ok(Json(status))
}

async fn interrupt_handler(
    State(svc): State<DebugControlService>,
    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, HandlerError> {
    svc.interrupt(session_id).await.map_err(map_err)?;
    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "interrupted": true,
        "message": "反应器已请求中断",
    })))
}

async fn step_handler(
    State(svc): State<DebugControlService>,
    Path(session_id): Path<u64>,
    Json(req): Json<StepRequest>,
) -> Result<Json<StepResponse>, HandlerError> {
    let step = svc.step(session_id, req.version).await.map_err(map_err)?;
    Ok(Json(step))
}

async fn snapshot_handler(
    State(svc): State<DebugControlService>,
    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, HandlerError> {
    let snap = svc.get_snapshot(session_id).await.map_err(map_err)?;
    Ok(Json(snap))
}

async fn pause_handler(
    State(svc): State<DebugControlService>,
    Path(session_id): Path<u64>,
) -> Json<serde_json::Value> {
    svc.set_paused(session_id, true);
    Json(serde_json::json!({
        "session_id": session_id,
        "paused": true,
        "message": "调试轮询已暂停",
    }))
}

async fn resume_handler(
    State(svc): State<DebugControlService>,
    Path(session_id): Path<u64>,
) -> Json<serde_json::Value> {
    svc.set_paused(session_id, false);
    Json(serde_json::json!({
        "session_id": session_id,
        "paused": false,
        "message": "调试轮询已恢复",
    }))
}

/// SSE watch 端点：轮询状态变化并推送
async fn watch_handler(
    State(svc): State<DebugControlService>,
    Path(session_id): Path<u64>,
    Query(params): Query<WatchQuery>,
) -> impl IntoResponse {
    let interval_ms = params.interval_ms;
    let svc_clone = svc.clone();

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::io::Error>>(16);

    tokio::spawn(async move {
        loop {
            if svc_clone.is_paused(session_id) {
                tokio::time::sleep(tokio::time::Duration::from_millis(interval_ms)).await;
                continue;
            }

            match svc_clone.get_status(session_id).await {
                Ok(status) => {
                    let prev_version = svc_clone.get_last_version(session_id);
                    let changed = prev_version != Some(status.version);
                    svc_clone.set_last_version(session_id, status.version);

                    let event_data = serde_json::json!({
                        "session_id": status.session_id,
                        "version": status.version,
                        "phase": status.phase,
                        "causal_depth": status.causal_depth,
                        "is_finished": status.is_finished,
                        "changed": changed,
                    });

                    let event = Event::default()
                        .event("status")
                        .data(event_data.to_string());

                    if tx.send(Ok(event)).await.is_err() {
                        break;
                    }

                    if status.is_finished {
                        let end_event = Event::default()
                            .event("end")
                            .data(r#"{"message":"会话已结束"}"#);
                        let _ = tx.send(Ok(end_event)).await;
                        break;
                    }
                }
                Err(e) => {
                    let event = Event::default()
                        .event("error")
                        .data(serde_json::json!({"error": e}).to_string());
                    let _ = tx.send(Ok(event)).await;
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(interval_ms)).await;
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    Sse::new(stream)
}

/// 构建路由
pub fn build_router(service: DebugControlService) -> Router {
    Router::new()
        .route("/api/sessions/{id}/debug/status", get(status_handler))
        .route(
            "/api/sessions/{id}/debug/interrupt",
            post(interrupt_handler),
        )
        .route("/api/sessions/{id}/debug/step", post(step_handler))
        .route("/api/sessions/{id}/debug/snapshot", get(snapshot_handler))
        .route("/api/sessions/{id}/debug/pause", post(pause_handler))
        .route("/api/sessions/{id}/debug/resume", post(resume_handler))
        .route("/api/sessions/{id}/debug/watch", get(watch_handler))
        .with_state(service)
}

/// 启动 HTTP API 服务器
pub async fn run_server(service: DebugControlService, addr: &str) -> Result<(), String> {
    let app = build_router(service);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("绑定地址 {} 失败: {}", addr, e))?;
    info!("调试控制服务已启动 addr={}", addr);
    info!("端点:");
    const ENDPOINTS: &[&str] = &[
        "GET  /api/sessions/{id}/debug/status",
        "POST /api/sessions/{id}/debug/interrupt",
        "POST /api/sessions/{id}/debug/step",
        "GET  /api/sessions/{id}/debug/snapshot",
        "POST /api/sessions/{id}/debug/pause",
        "POST /api/sessions/{id}/debug/resume",
        "GET  /api/sessions/{id}/debug/watch?interval_ms=",
    ];
    for ep in ENDPOINTS {
        info!("  {ep}");
    }
    axum::serve(listener, app)
        .await
        .map_err(|e| format!("服务器错误: {}", e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    // ============ 纯状态测试（无 HTTP 依赖） ============

    /// 构造一个指向无效地址的 service（纯状态测试不走网络）
    fn new_service() -> DebugControlService {
        DebugControlService::new("http://127.0.0.1:1".to_string())
    }

    #[test]
    fn test_is_paused_default_false() {
        let svc = new_service();
        assert!(!svc.is_paused(1), "新会话默认未暂停");
    }

    #[test]
    fn test_set_paused_true() {
        let svc = new_service();
        svc.set_paused(1, true);
        assert!(svc.is_paused(1), "设置 true 后应暂停");
    }

    #[test]
    fn test_set_paused_false_after_true() {
        let svc = new_service();
        svc.set_paused(1, true);
        svc.set_paused(1, false);
        assert!(!svc.is_paused(1), "再设置 false 后应恢复");
    }

    #[test]
    fn test_paused_multi_session_isolation() {
        let svc = new_service();
        svc.set_paused(1, true);
        svc.set_paused(2, false);
        assert!(svc.is_paused(1), "session 1 暂停");
        assert!(!svc.is_paused(2), "session 2 未暂停");
        // 修改 session 2 不影响 session 1
        svc.set_paused(2, true);
        assert!(svc.is_paused(1), "session 1 仍暂停");
        assert!(svc.is_paused(2), "session 2 现在暂停");
    }

    #[test]
    fn test_get_last_version_default_none() {
        let svc = new_service();
        assert_eq!(svc.get_last_version(1), None, "新会话默认无 last_version");
    }

    #[test]
    fn test_set_last_version() {
        let svc = new_service();
        svc.set_last_version(1, 42);
        assert_eq!(svc.get_last_version(1), Some(42));
    }

    #[test]
    fn test_set_last_version_overwrite() {
        let svc = new_service();
        svc.set_last_version(1, 10);
        svc.set_last_version(1, 20);
        assert_eq!(svc.get_last_version(1), Some(20), "覆盖后应是最新值");
    }

    // ============ 服务层 HTTP 测试（mockito 模拟上游 evorule-server） ============

    /// 构造合法的 /state 响应体
    fn state_json_body() -> String {
        serde_json::json!({
            "version": 5,
            "reactor": {
                "phase": "running",
                "causal_depth": 3,
                "invariant_violations": 0,
                "pending_io_count": 2
            }
        })
        .to_string()
    }

    /// 构造合法的 /rewind/{v} 响应体
    fn rewind_json_body() -> String {
        serde_json::json!({
            "version": 5,
            "payload": {"amount": 100},
            "queue": [{"type": "io_request", "params": {"io_type": "http"}}]
        })
        .to_string()
    }

    #[tokio::test]
    async fn test_get_status_success() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/sessions/1/state")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(state_json_body())
            .create_async()
            .await;

        let svc = DebugControlService::new(server.url());
        let status = svc.get_status(1).await.expect("应成功");
        assert_eq!(status.session_id, 1);
        assert_eq!(status.version, 5);
        assert_eq!(status.phase, "running");
        assert_eq!(status.causal_depth, 3);
        assert!(!status.is_finished);
        assert_eq!(status.invariant_violations, 0);
        assert_eq!(status.pending_io_count, 2);
    }

    #[tokio::test]
    async fn test_get_status_missing_reactor_field() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/sessions/1/state")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(serde_json::json!({"version": 5}).to_string())
            .create_async()
            .await;

        let svc = DebugControlService::new(server.url());
        let err = svc.get_status(1).await.expect_err("缺少 reactor 应报错");
        assert!(err.contains("缺少 reactor 字段"), "实际错误: {err}");
    }

    #[tokio::test]
    async fn test_get_status_upstream_error() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/sessions/1/state")
            .with_status(500)
            .create_async()
            .await;

        let svc = DebugControlService::new(server.url());
        let err = svc.get_status(1).await.expect_err("上游 500 应报错");
        assert!(err.contains("state 端点返回"), "实际错误: {err}");
        assert!(err.contains("500"), "实际错误: {err}");
    }

    #[tokio::test]
    async fn test_get_status_network_error() {
        // 指向拒绝连接的地址 → 网络错误
        let svc = DebugControlService::new("http://127.0.0.1:1".to_string());
        let err = svc.get_status(1).await.expect_err("网络不可达应报错");
        assert!(err.contains("请求 state 失败"), "实际错误: {err}");
    }

    #[tokio::test]
    async fn test_get_status_finished_phase() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/sessions/1/state")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                serde_json::json!({
                    "version": 10,
                    "reactor": {"phase": "finished", "causal_depth": 0,
                                 "invariant_violations": 0, "pending_io_count": 0}
                })
                .to_string(),
            )
            .create_async()
            .await;

        let svc = DebugControlService::new(server.url());
        let status = svc.get_status(1).await.expect("应成功");
        assert!(
            status.is_finished,
            "phase=finished 时 is_finished 应为 true"
        );
    }

    #[tokio::test]
    async fn test_interrupt_success() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/api/sessions/1/interrupt")
            .with_status(200)
            .create_async()
            .await;

        let svc = DebugControlService::new(server.url());
        svc.interrupt(1).await.expect("interrupt 应成功");
    }

    #[tokio::test]
    async fn test_interrupt_upstream_error() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/api/sessions/1/interrupt")
            .with_status(409)
            .create_async()
            .await;

        let svc = DebugControlService::new(server.url());
        let err = svc.interrupt(1).await.expect_err("上游 409 应报错");
        assert!(err.contains("interrupt 端点返回"), "实际错误: {err}");
        assert!(err.contains("409"), "实际错误: {err}");
    }

    #[tokio::test]
    async fn test_step_success() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/sessions/1/rewind/5")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(rewind_json_body())
            .create_async()
            .await;

        let svc = DebugControlService::new(server.url());
        let step = svc.step(1, 5).await.expect("step 应成功");
        assert_eq!(step.session_id, 1);
        assert_eq!(step.target_version, 5);
        assert_eq!(step.actual_version, 5);
        assert_eq!(step.payload, serde_json::json!({"amount": 100}));
        assert_eq!(step.queue.len(), 1);
    }

    #[tokio::test]
    async fn test_step_missing_fields_uses_defaults() {
        let mut server = mockito::Server::new_async().await;
        // 缺少 payload 和 queue 字段 → 应使用默认值
        server
            .mock("GET", "/api/sessions/1/rewind/3")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(serde_json::json!({"version": 3}).to_string())
            .create_async()
            .await;

        let svc = DebugControlService::new(server.url());
        let step = svc.step(1, 3).await.expect("应成功（用默认值）");
        assert_eq!(step.actual_version, 3);
        assert_eq!(step.payload, serde_json::json!({}), "缺 payload 应为空对象");
        assert!(step.queue.is_empty(), "缺 queue 应为空数组");
    }

    #[tokio::test]
    async fn test_step_upstream_error() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/sessions/1/rewind/99")
            .with_status(404)
            .create_async()
            .await;

        let svc = DebugControlService::new(server.url());
        let err = svc.step(1, 99).await.expect_err("上游 404 应报错");
        assert!(err.contains("rewind 端点返回"), "实际错误: {err}");
    }

    #[tokio::test]
    async fn test_get_snapshot_success() {
        let mut server = mockito::Server::new_async().await;
        // get_snapshot 先调 /state 取 version, 再调 /rewind/{v} 取快照
        server
            .mock("GET", "/api/sessions/1/state")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(state_json_body())
            .create_async()
            .await;
        server
            .mock("GET", "/api/sessions/1/rewind/5")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(rewind_json_body())
            .create_async()
            .await;

        let svc = DebugControlService::new(server.url());
        let snap = svc.get_snapshot(1).await.expect("get_snapshot 应成功");
        assert_eq!(snap["session_id"], 1);
        assert_eq!(snap["version"], 5);
        assert_eq!(snap["payload"]["amount"], 100);
        assert_eq!(snap["phase"], "running");
        assert_eq!(snap["is_finished"], false);
    }

    // ============ handler 层 oneshot 测试 ============

    /// 辅助: 发送请求并返回 (status, body_text)
    async fn send_request(router: Router, req: Request<Body>) -> (StatusCode, String) {
        let response = router.oneshot(req).await.expect("oneshot failed");
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body collect failed")
            .to_bytes();
        (status, String::from_utf8_lossy(&body).to_string())
    }

    #[tokio::test]
    async fn test_handler_status_success() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/sessions/1/state")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(state_json_body())
            .create_async()
            .await;

        let svc = DebugControlService::new(server.url());
        let router = build_router(svc);

        let (status, body) = send_request(
            router,
            Request::builder()
                .method("GET")
                .uri("/api/sessions/1/debug/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"session_id\":1"));
        assert!(body.contains("\"phase\":\"running\""));
        assert!(body.contains("\"version\":5"));
    }

    #[tokio::test]
    async fn test_handler_status_upstream_error_returns_500() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/sessions/1/state")
            .with_status(500)
            .create_async()
            .await;

        let svc = DebugControlService::new(server.url());
        let router = build_router(svc);

        let (status, body) = send_request(
            router,
            Request::builder()
                .method("GET")
                .uri("/api/sessions/1/debug/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body.contains("state 端点返回"), "body={body}");
    }

    #[tokio::test]
    async fn test_handler_interrupt_success() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/api/sessions/1/interrupt")
            .with_status(200)
            .create_async()
            .await;

        let svc = DebugControlService::new(server.url());
        let router = build_router(svc);

        let (status, body) = send_request(
            router,
            Request::builder()
                .method("POST")
                .uri("/api/sessions/1/debug/interrupt")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"interrupted\":true"));
        assert!(body.contains("已请求中断"));
    }

    #[tokio::test]
    async fn test_handler_step_success() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/sessions/1/rewind/5")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(rewind_json_body())
            .create_async()
            .await;

        let svc = DebugControlService::new(server.url());
        let router = build_router(svc);

        let (status, body) = send_request(
            router,
            Request::builder()
                .method("POST")
                .uri("/api/sessions/1/debug/step")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!({"version": 5}).to_string()))
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"target_version\":5"));
        assert!(body.contains("\"actual_version\":5"));
        assert!(body.contains("\"amount\":100"));
    }

    #[tokio::test]
    async fn test_handler_step_invalid_body_returns_400() {
        let svc = new_service();
        let router = build_router(svc);

        // 非法 JSON body → axum Json 提取器返回 400 (而非 500)
        let (status, _body) = send_request(
            router,
            Request::builder()
                .method("POST")
                .uri("/api/sessions/1/debug/step")
                .header("content-type", "application/json")
                .body(Body::from("{bad json}"))
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_handler_step_upstream_error_returns_500() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/sessions/1/rewind/99")
            .with_status(404)
            .create_async()
            .await;

        let svc = DebugControlService::new(server.url());
        let router = build_router(svc);

        let (status, body) = send_request(
            router,
            Request::builder()
                .method("POST")
                .uri("/api/sessions/1/debug/step")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!({"version": 99}).to_string()))
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body.contains("rewind 端点返回"), "body={body}");
    }

    #[tokio::test]
    async fn test_handler_snapshot_success() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/sessions/1/state")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(state_json_body())
            .create_async()
            .await;
        server
            .mock("GET", "/api/sessions/1/rewind/5")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(rewind_json_body())
            .create_async()
            .await;

        let svc = DebugControlService::new(server.url());
        let router = build_router(svc);

        let (status, body) = send_request(
            router,
            Request::builder()
                .method("GET")
                .uri("/api/sessions/1/debug/snapshot")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"version\":5"));
        assert!(body.contains("\"amount\":100"));
        assert!(body.contains("\"phase\":\"running\""));
    }

    #[tokio::test]
    async fn test_handler_pause() {
        let svc = new_service();
        let router = build_router(svc.clone());

        let (status, body) = send_request(
            router,
            Request::builder()
                .method("POST")
                .uri("/api/sessions/1/debug/pause")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"paused\":true"));
        // 验证状态确实写入
        assert!(svc.is_paused(1));
    }

    #[tokio::test]
    async fn test_handler_resume() {
        let svc = new_service();
        svc.set_paused(1, true);
        let router = build_router(svc.clone());

        let (status, body) = send_request(
            router,
            Request::builder()
                .method("POST")
                .uri("/api/sessions/1/debug/resume")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"paused\":false"));
        // 验证状态确实清除
        assert!(!svc.is_paused(1));
    }

    #[tokio::test]
    async fn test_handler_watch_sse_stream() {
        let mut server = mockito::Server::new_async().await;
        // 返回 finished 状态 → watch 推送 status 事件后立即推送 end 事件并退出
        server
            .mock("GET", "/api/sessions/1/state")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                serde_json::json!({
                    "version": 10,
                    "reactor": {"phase": "finished", "causal_depth": 0,
                                 "invariant_violations": 0, "pending_io_count": 0}
                })
                .to_string(),
            )
            .create_async()
            .await;

        let svc = DebugControlService::new(server.url());
        let router = build_router(svc);

        let response = router
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/sessions/1/debug/watch?interval_ms=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("oneshot failed");

        assert_eq!(response.status(), StatusCode::OK);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body collect failed")
            .to_bytes();
        let body_str = String::from_utf8_lossy(&body);

        // SSE 流应包含 status 事件和 end 事件
        assert!(body_str.contains("event: status"), "body={body_str}");
        assert!(body_str.contains("\"version\":10"), "body={body_str}");
        assert!(body_str.contains("event: end"), "body={body_str}");
    }
}

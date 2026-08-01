// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Prometheus 指标收集服务
//!
//! 从 evorule-server 获取运行时指标，通过 HTTP 端点暴露给 Prometheus。
//! 可选从 semantic_invariants 服务拉取业务违规统计。
//!
//! # 设计
//! - 通过 HTTP API 定期从 evorule-server 拉取会话状态
//! - 可选从 semantic_invariants 服务拉取业务约束违规统计
//! - 使用 prometheus crate 注册指标到**自定义 registry**（非全局默认）
//! - 提供 `/metrics` 端点供 Prometheus 抓取
//!
//! # 指标说明
//! ## evorule-server 指标
//! - `evorule_sessions_total`：会话总数（Gauge）
//! - `evorule_sessions_active`：活跃会话数（Gauge）
//! - `evorule_session_causal_depth`：会话因果深度（GaugeVec, label: session_id）
//! - `evorule_session_version`：会话版本号（GaugeVec, label: session_id）
//! - `evorule_session_structural_invariant_violations`：不变量违反数（GaugeVec, label: session_id）
//! - `evorule_session_pending_io`：待处理 I/O 数（GaugeVec, label: session_id）
//! - `evorule_session_finished`：会话是否完成（GaugeVec, label: session_id）
//!
//! ## semantic_invariants 业务违规指标（配置 --semantic-invariants-url 后启用）
//! - `evorule_semantic_rules_total`：已注册约束规则总数（Gauge）
//! - `evorule_semantic_violations_total`：累计业务违规总数（Gauge）
//! - `evorule_semantic_violations_by_severity`：按严重级别的违规数（GaugeVec, label: severity）
//!
//! # 设计说明
//!
//! - **自定义 Registry**：每个 `MetricsService` 实例持有独立的 `prometheus::Registry`，
//!   而非全局默认 registry。这允许在测试中创建多个实例而不冲突。
//! - **poll_once 可测试**：核心收集逻辑提取为 `poll_once()`，可独立调用（不启动无限循环）。
//! - **mutex 毒化降级**：`session_metrics`/`server_metrics` 用 `std::sync::Mutex` 保护，
//!   毒化时不 panic，降级访问受污染数据并记录 error 日志。

#![forbid(unsafe_code)]
// C5 (unwrap/expect/panic = deny) 仅约束生产代码;测试代码保留 unwrap 惯例
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use axum::{extract::State, response::Json, routing::get, Router};
use prometheus::{Encoder, IntGauge, IntGaugeVec, Opts, Registry, TextEncoder};
use serde::Deserialize;
use std::sync::{Arc, Mutex};
use tokio::task;
use tracing::{info, warn};

/// 单个会话的指标快照
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct SessionMetrics {
    pub session_id: u64,
    pub causal_depth: i64,
    pub version: i64,
    pub invariant_violations: i64,
    pub pending_io_count: i64,
    pub current_phase: String,
    pub is_finished: bool,
}

/// 服务器级聚合指标
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct ServerMetrics {
    pub session_count: i64,
    pub active_sessions: i64,
}

/// 会话列表响应（/api/sessions 返回格式）
#[derive(Debug, Deserialize)]
struct SessionsResponse {
    sessions: Vec<u64>,
}

/// 会话状态响应（/api/sessions/{id}/state 返回格式）
#[derive(Debug, Deserialize)]
struct SessionStateResponse {
    version: i64,
    reactor: ReactorState,
}

/// 反应器状态
#[derive(Debug, Deserialize)]
struct ReactorState {
    causal_depth: i64,
    #[allow(dead_code)]
    current_step: i64,
    invariant_violations: i64,
    pending_io_count: i64,
    phase: String,
}

/// semantic_invariants /api/stats 响应
#[derive(Debug, Deserialize)]
struct SemanticStatsResponse {
    total_rules: i64,
    total_violations: i64,
    violations_by_severity: std::collections::HashMap<String, i64>,
    #[allow(dead_code)]
    sessions_with_violations: i64,
    #[allow(dead_code)]
    monitored_sessions: i64,
}

/// 所有 Prometheus 指标 + 自定义 registry 的集合（内部使用）
///
/// 将指标构建逻辑从 `MetricsService::new()` 中抽出，降低函数复杂度。
/// 每个实例持有独立的 `Registry`，允许多个 `MetricsService` 并存而不冲突。
#[derive(Clone)]
struct MetricsSet {
    registry: Arc<Registry>,
    // evorule-server 运行时指标
    sessions_total: IntGauge,
    sessions_active: IntGauge,
    session_causal_depth: IntGaugeVec,
    session_version: IntGaugeVec,
    session_structural_invariant_violations: IntGaugeVec,
    session_pending_io: IntGaugeVec,
    session_finished: IntGaugeVec,
    // semantic_invariants 业务违规指标
    semantic_rules_total: IntGauge,
    semantic_violations_total: IntGauge,
    semantic_violations_by_severity: IntGaugeVec,
}

impl MetricsSet {
    /// 创建服务器级 Gauge（带 evorule namespace）
    fn server_gauge(name: &str, help: &str) -> Result<IntGauge, String> {
        IntGauge::with_opts(Opts::new(name, help).namespace("evorule"))
            .map_err(|e| format!("创建 {name} 指标失败: {e}"))
    }

    /// 创建会话级 GaugeVec（带 session_id 标签）
    fn session_vec(name: &str, help: &str) -> Result<IntGaugeVec, String> {
        IntGaugeVec::new(Opts::new(name, help).namespace("evorule"), &["session_id"])
            .map_err(|e| format!("创建 {name} 指标失败: {e}"))
    }

    /// 创建所有 Prometheus 指标并注册到自定义 registry
    fn build() -> Result<Self, String> {
        let registry = Arc::new(Registry::new());

        let sessions_total = Self::server_gauge("sessions_total", "Total number of sessions")?;
        let sessions_active =
            Self::server_gauge("sessions_active", "Number of active (unfinished) sessions")?;

        let session_causal_depth =
            Self::session_vec("session_causal_depth", "Causal depth of the session")?;
        let session_version =
            Self::session_vec("session_version", "Current version of the session")?;
        let session_structural_invariant_violations = Self::session_vec(
            "session_structural_invariant_violations",
            "Number of structural invariant violations (tier1 reactor, not business constraints)",
        )?;
        let session_pending_io =
            Self::session_vec("session_pending_io", "Number of pending I/O operations")?;
        let session_finished = Self::session_vec(
            "session_finished",
            "Whether the session is finished (1) or not (0)",
        )?;

        let semantic_rules_total = Self::server_gauge(
            "semantic_rules_total",
            "Total registered semantic invariant rules",
        )?;
        let semantic_violations_total = Self::server_gauge(
            "semantic_violations_total",
            "Total business constraint violations",
        )?;
        let semantic_violations_by_severity = IntGaugeVec::new(
            Opts::new(
                "semantic_violations_by_severity",
                "Business constraint violations by severity level",
            )
            .namespace("evorule"),
            &["severity"],
        )
        .map_err(|e| format!("创建 semantic_violations_by_severity 指标失败: {e}"))?;

        // 批量注册到自定义 registry
        let collectors: Vec<Box<dyn prometheus::core::Collector>> = vec![
            Box::new(sessions_total.clone()),
            Box::new(sessions_active.clone()),
            Box::new(session_causal_depth.clone()),
            Box::new(session_version.clone()),
            Box::new(session_structural_invariant_violations.clone()),
            Box::new(session_pending_io.clone()),
            Box::new(session_finished.clone()),
            Box::new(semantic_rules_total.clone()),
            Box::new(semantic_violations_total.clone()),
            Box::new(semantic_violations_by_severity.clone()),
        ];
        for collector in collectors {
            registry
                .register(collector)
                .map_err(|e| format!("注册指标失败: {e}"))?;
        }

        // 初始化四个严重级别为 0（确保指标始终输出，即使无违规）
        for severity in &["critical", "high", "medium", "low"] {
            semantic_violations_by_severity
                .with_label_values(&[severity])
                .set(0);
        }

        Ok(Self {
            registry,
            sessions_total,
            sessions_active,
            session_causal_depth,
            session_version,
            session_structural_invariant_violations,
            session_pending_io,
            session_finished,
            semantic_rules_total,
            semantic_violations_total,
            semantic_violations_by_severity,
        })
    }
}

/// 指标收集服务
#[derive(Clone)]
pub struct MetricsService {
    metrics: MetricsSet,
    evorule_server_url: String,
    /// semantic_invariants 服务地址（可选，配置后启用业务违规指标）
    semantic_invariants_url: Option<String>,
    poll_interval_ms: u64,
    session_metrics: Arc<Mutex<Vec<SessionMetrics>>>,
    server_metrics: Arc<Mutex<ServerMetrics>>,
}

impl MetricsService {
    /// 创建新的指标服务并注册 Prometheus 指标到自定义 registry
    ///
    /// # 参数
    /// - `evorule_server_url`：evorule-server 的 HTTP 地址
    /// - `semantic_invariants_url`：semantic_invariants 服务地址（可选，配置后启用业务违规指标）
    /// - `poll_interval_ms`：轮询间隔（毫秒）
    pub fn new(
        evorule_server_url: String,
        semantic_invariants_url: Option<String>,
        poll_interval_ms: u64,
    ) -> Result<Self, String> {
        let metrics = MetricsSet::build()?;

        if semantic_invariants_url.is_some() {
            info!("业务违规指标已启用（将从 semantic_invariants 服务拉取）");
        }

        Ok(Self {
            metrics,
            evorule_server_url,
            semantic_invariants_url,
            poll_interval_ms,
            session_metrics: Arc::new(Mutex::new(Vec::new())),
            server_metrics: Arc::new(Mutex::new(ServerMetrics {
                session_count: 0,
                active_sessions: 0,
            })),
        })
    }

    /// 执行一次指标收集（可独立调用，不启动无限循环）
    ///
    /// 从 evorule-server 拉取会话列表和状态，更新 Prometheus 指标和内存快照。
    /// 错误不会中断整体流程，仅记录 warn 日志。
    pub async fn poll_once(&self, client: &reqwest::Client) {
        // 1. 获取会话列表
        let session_ids = self.fetch_session_ids(client).await;

        // 2. 对每个会话获取详细状态并更新 Prometheus 指标
        let mut new_session_metrics = Vec::with_capacity(session_ids.len());
        let mut active_count: i64 = 0;
        for sid in &session_ids {
            if let Some((metrics, is_active)) = self.fetch_and_record_session(client, sid).await {
                if is_active {
                    active_count += 1;
                }
                new_session_metrics.push(metrics);
            }
        }

        // 3. 清理已消失会话的指标标签（避免残留）
        self.cleanup_disappeared_labels(&session_ids);

        // 4. 更新服务器级指标 + 内存快照
        let total = session_ids.len() as i64;
        self.metrics.sessions_total.set(total);
        self.metrics.sessions_active.set(active_count);
        *self.lock_session_metrics() = new_session_metrics;
        {
            let mut sm = self.lock_server_metrics();
            sm.session_count = total;
            sm.active_sessions = active_count;
        }

        // 5. 拉取 semantic_invariants 业务违规统计（配置 URL 后启用）
        if let Some(ref si_url) = self.semantic_invariants_url {
            self.poll_semantic_stats(client, si_url).await;
        }

        info!(sessions = total, active = active_count, "指标已更新");
    }

    /// 从 evorule-server 获取会话 ID 列表；网络/解析错误降级为空列表
    async fn fetch_session_ids(&self, client: &reqwest::Client) -> Vec<u64> {
        let url = format!("{}/api/sessions", self.evorule_server_url);
        match client.get(&url).send().await {
            Ok(resp) => match resp.json::<SessionsResponse>().await {
                Ok(body) => body.sessions,
                Err(e) => {
                    warn!(error = %e, "解析会话列表失败");
                    Vec::new()
                }
            },
            Err(e) => {
                warn!(error = %e, "获取会话列表失败");
                Vec::new()
            }
        }
    }

    /// 获取单个会话状态，更新 Prometheus 会话级指标，返回 (快照, is_active)。
    /// 网络/解析错误返回 None（并记录 warn 日志）。
    async fn fetch_and_record_session(
        &self,
        client: &reqwest::Client,
        sid: &u64,
    ) -> Option<(SessionMetrics, bool)> {
        let url = format!("{}/api/sessions/{}/state", self.evorule_server_url, sid);
        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, session_id = sid, "获取会话状态失败");
                return None;
            }
        };
        let state = match resp.json::<SessionStateResponse>().await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, session_id = sid, "解析会话状态失败");
                return None;
            }
        };

        let is_finished = state.reactor.phase == "finished";
        let sid_str = sid.to_string();

        // 更新 Prometheus 会话级指标
        self.metrics
            .session_causal_depth
            .with_label_values(&[&sid_str])
            .set(state.reactor.causal_depth);
        self.metrics
            .session_version
            .with_label_values(&[&sid_str])
            .set(state.version);
        self.metrics
            .session_structural_invariant_violations
            .with_label_values(&[&sid_str])
            .set(state.reactor.invariant_violations);
        self.metrics
            .session_pending_io
            .with_label_values(&[&sid_str])
            .set(state.reactor.pending_io_count);
        self.metrics
            .session_finished
            .with_label_values(&[&sid_str])
            .set(if is_finished { 1 } else { 0 });

        Some((
            SessionMetrics {
                session_id: *sid,
                causal_depth: state.reactor.causal_depth,
                version: state.version,
                invariant_violations: state.reactor.invariant_violations,
                pending_io_count: state.reactor.pending_io_count,
                current_phase: state.reactor.phase,
                is_finished,
            },
            !is_finished,
        ))
    }

    /// 清理已消失会话的 Prometheus 标签（避免残留）
    fn cleanup_disappeared_labels(&self, current_ids: &[u64]) {
        let current: std::collections::HashSet<u64> = current_ids.iter().copied().collect();
        let prev = self.lock_session_metrics();
        for old in prev.iter() {
            if !current.contains(&old.session_id) {
                let sid_str = old.session_id.to_string();
                let m = &self.metrics;
                let _ = m.session_causal_depth.remove_label_values(&[&sid_str]);
                let _ = m.session_version.remove_label_values(&[&sid_str]);
                let _ = m
                    .session_structural_invariant_violations
                    .remove_label_values(&[&sid_str]);
                let _ = m.session_pending_io.remove_label_values(&[&sid_str]);
                let _ = m.session_finished.remove_label_values(&[&sid_str]);
            }
        }
    }

    /// 拉取 semantic_invariants 业务违规统计
    async fn poll_semantic_stats(&self, client: &reqwest::Client, si_url: &str) {
        let stats_url = format!("{}/api/stats", si_url);
        let resp = match client.get(&stats_url).send().await {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "拉取 semantic_invariants 统计失败");
                return;
            }
        };
        let stats = match resp.json::<SemanticStatsResponse>().await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "解析 semantic_invariants 统计失败");
                return;
            }
        };

        self.apply_semantic_stats(&stats);
        tracing::debug!(
            rules = stats.total_rules,
            violations = stats.total_violations,
            "semantic_invariants 统计已更新"
        );
    }

    /// 将 semantic_invariants 统计写入 Prometheus 指标
    fn apply_semantic_stats(&self, stats: &SemanticStatsResponse) {
        self.metrics.semantic_rules_total.set(stats.total_rules);
        self.metrics
            .semantic_violations_total
            .set(stats.total_violations);

        // 更新四个严重级别的违规计数（缺失的填 0）
        for severity in &["critical", "high", "medium", "low"] {
            let count = stats
                .violations_by_severity
                .get(*severity)
                .copied()
                .unwrap_or(0);
            self.metrics
                .semantic_violations_by_severity
                .with_label_values(&[severity])
                .set(count);
        }
    }

    /// 启动指标收集后台任务
    pub async fn start(&self) -> Result<(), String> {
        let svc = self.clone();
        let poll_interval_ms = self.poll_interval_ms;

        task::spawn(async move {
            let client = reqwest::Client::new();
            loop {
                svc.poll_once(&client).await;
                tokio::time::sleep(tokio::time::Duration::from_millis(poll_interval_ms)).await;
            }
        });

        info!("指标收集服务已启动");
        Ok(())
    }

    /// 获取指标文本（Prometheus 格式）
    pub fn get_metrics_text(&self) -> String {
        let metrics = self.metrics.registry.gather();
        let encoder = TextEncoder::new();
        let mut buffer = vec![];
        if encoder.encode(&metrics, &mut buffer).is_ok() {
            String::from_utf8_lossy(&buffer).to_string()
        } else {
            String::from("# Error encoding metrics")
        }
    }

    /// 获取会话指标快照
    pub fn get_session_metrics(&self) -> Vec<SessionMetrics> {
        self.lock_session_metrics().clone()
    }

    /// 获取服务器指标快照
    pub fn get_server_metrics(&self) -> ServerMetrics {
        self.lock_server_metrics().clone()
    }

    /// 锁定 session_metrics，毒化时取回内部数据继续访问（不 panic）
    fn lock_session_metrics(&self) -> std::sync::MutexGuard<'_, Vec<SessionMetrics>> {
        match self.session_metrics.lock() {
            Ok(g) => g,
            Err(p) => {
                tracing::error!("session_metrics mutex 毒化,降级访问受污染数据");
                p.into_inner()
            }
        }
    }

    /// 锁定 server_metrics，毒化时取回内部数据继续访问（不 panic）
    fn lock_server_metrics(&self) -> std::sync::MutexGuard<'_, ServerMetrics> {
        match self.server_metrics.lock() {
            Ok(g) => g,
            Err(p) => {
                tracing::error!("server_metrics mutex 毒化,降级访问受污染数据");
                p.into_inner()
            }
        }
    }
}

// ============ HTTP API ============

/// `GET /metrics` — Prometheus 文本格式
async fn metrics_handler(State(svc): State<MetricsService>) -> String {
    svc.get_metrics_text()
}

/// `GET /sessions` — 会话指标快照 JSON
async fn sessions_handler(State(svc): State<MetricsService>) -> Json<Vec<SessionMetrics>> {
    Json(svc.get_session_metrics())
}

/// `GET /status` — 服务器级聚合指标 JSON
async fn status_handler(State(svc): State<MetricsService>) -> Json<ServerMetrics> {
    Json(svc.get_server_metrics())
}

/// 构建路由（公开以便测试）
pub fn build_router(service: MetricsService) -> Router {
    Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/sessions", get(sessions_handler))
        .route("/status", get(status_handler))
        .with_state(service)
}

/// 启动 HTTP API 服务器
pub async fn run_server(service: MetricsService, addr: &str) -> Result<(), String> {
    let app = build_router(service);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("绑定地址 {addr} 失败: {e}"))?;
    info!("指标 HTTP API 已启动 addr={addr}");
    const ENDPOINTS: &[&str] = &[
        "GET /metrics   (Prometheus 文本格式)",
        "GET /sessions  (会话指标快照 JSON)",
        "GET /status    (服务器级聚合指标 JSON)",
    ];
    for ep in ENDPOINTS {
        info!("  {ep}");
    }
    axum::serve(listener, app)
        .await
        .map_err(|e| format!("服务器错误: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use mockito::Server;
    use tower::ServiceExt;

    // ============ 辅助构造 ============

    /// 构造最小参数的 service（无 semantic_invariants URL）
    fn new_service(server_url: String) -> MetricsService {
        MetricsService::new(server_url, None, 1000).expect("service 创建失败")
    }

    /// 构造带 semantic_invariants URL 的 service
    fn new_service_with_si(server_url: String, si_url: String) -> MetricsService {
        MetricsService::new(server_url, Some(si_url), 1000).expect("service 创建失败")
    }

    /// 构造一个 reactor state JSON（mockito 响应体片段）
    fn reactor_state_json(
        causal_depth: i64,
        version: i64,
        violations: i64,
        pending_io: i64,
        phase: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "version": version,
            "reactor": {
                "causal_depth": causal_depth,
                "current_step": 0,
                "invariant_violations": violations,
                "pending_io_count": pending_io,
                "phase": phase,
            }
        })
    }

    /// 辅助: 发送 oneshot 请求并返回 (status, body_text)
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

    // ============ new() 构造测试 ============

    #[test]
    fn test_new_basic() {
        let svc = new_service("http://127.0.0.1:18080".to_string());
        // 初始快照应为空
        assert!(svc.get_session_metrics().is_empty());
        let sm = svc.get_server_metrics();
        assert_eq!(sm.session_count, 0);
        assert_eq!(sm.active_sessions, 0);
    }

    #[test]
    fn test_new_with_semantic_url() {
        let svc = new_service_with_si(
            "http://127.0.0.1:18080".to_string(),
            "http://127.0.0.1:8082".to_string(),
        );
        // 即使配置了 SI URL，初始违规指标也应为 0（在 new 中预初始化）
        let text = svc.get_metrics_text();
        assert!(text.contains("evorule_semantic_rules_total"));
        assert!(text.contains("evorule_semantic_violations_total"));
        assert!(text.contains("evorule_semantic_violations_by_severity"));
    }

    #[test]
    fn test_new_zero_poll_interval() {
        // 边界：poll_interval_ms = 0 不应导致构造失败
        let svc = MetricsService::new("http://x".to_string(), None, 0)
            .expect("poll_interval=0 应允许构造");
        assert_eq!(svc.poll_interval_ms, 0);
    }

    #[test]
    fn test_multiple_instances_no_conflict() {
        // 自定义 registry 的关键验证：多个实例并存不应 panic 或冲突
        let _s1 = new_service("http://a".to_string());
        let _s2 = new_service("http://b".to_string());
        let _s3 = new_service_with_si("http://c".to_string(), "http://d".to_string());
        // 若走到这里即通过——全局 registry 会在此前 panic
    }

    // ============ get_metrics_text 初始输出测试 ============

    #[test]
    fn test_get_metrics_text_contains_all_metrics() {
        let svc = new_service("http://x".to_string());
        let text = svc.get_metrics_text();
        // 服务器级（IntGauge 即使为 0 也会输出）
        assert!(text.contains("evorule_sessions_total"));
        assert!(text.contains("evorule_sessions_active"));
        // 会话级 IntGaugeVec 在初始状态（无 label 被设置）不会出现在输出中；
        // 它们在 poll_once 接收到会话后才输出 —— 见 test_poll_once_active_session
        // 业务违规级（rules_total / violations_total 为 IntGauge，始终输出）
        assert!(text.contains("evorule_semantic_rules_total"));
        assert!(text.contains("evorule_semantic_violations_total"));
        // by_severity 在 new() 中预初始化了 4 个 severity label，所以始终输出
        assert!(text.contains("evorule_semantic_violations_by_severity"));
    }

    #[test]
    fn test_get_metrics_text_initial_zero() {
        let svc = new_service("http://x".to_string());
        let text = svc.get_metrics_text();
        // sessions_total 初始应为 0
        assert!(text.contains("evorule_sessions_total 0"));
        assert!(text.contains("evorule_sessions_active 0"));
        // 四个严重级别应都被预初始化为 0
        assert!(text.contains("evorule_semantic_violations_by_severity{severity=\"critical\"} 0"));
        assert!(text.contains("evorule_semantic_violations_by_severity{severity=\"high\"} 0"));
        assert!(text.contains("evorule_semantic_violations_by_severity{severity=\"medium\"} 0"));
        assert!(text.contains("evorule_semantic_violations_by_severity{severity=\"low\"} 0"));
    }

    // ============ get_session_metrics / get_server_metrics 初始测试 ============

    #[test]
    fn test_get_session_metrics_initial_empty() {
        let svc = new_service("http://x".to_string());
        assert!(svc.get_session_metrics().is_empty());
    }

    #[test]
    fn test_get_server_metrics_initial_zero() {
        let svc = new_service("http://x".to_string());
        let m = svc.get_server_metrics();
        assert_eq!(m.session_count, 0);
        assert_eq!(m.active_sessions, 0);
    }

    // ============ poll_once 测试（mockito）============

    #[tokio::test]
    async fn test_poll_once_empty_sessions() {
        let mut server = Server::new_async().await;
        let _m = server
            .mock("GET", "/api/sessions")
            .with_status(200)
            .with_body(r#"{"sessions":[]}"#)
            .create_async()
            .await;

        let svc = new_service(server.url());
        let client = reqwest::Client::new();
        svc.poll_once(&client).await;

        // 空列表：服务器级指标应为 0
        let sm = svc.get_server_metrics();
        assert_eq!(sm.session_count, 0);
        assert_eq!(sm.active_sessions, 0);
        assert!(svc.get_session_metrics().is_empty());
    }

    #[tokio::test]
    async fn test_poll_once_active_session() {
        let mut server = Server::new_async().await;
        server
            .mock("GET", "/api/sessions")
            .with_status(200)
            .with_body(r#"{"sessions":[100]}"#)
            .create_async()
            .await;
        server
            .mock("GET", "/api/sessions/100/state")
            .with_status(200)
            .with_body(reactor_state_json(5, 3, 0, 2, "running").to_string())
            .create_async()
            .await;

        let svc = new_service(server.url());
        let client = reqwest::Client::new();
        svc.poll_once(&client).await;

        let sm = svc.get_server_metrics();
        assert_eq!(sm.session_count, 1);
        assert_eq!(sm.active_sessions, 1, "running 会话应计为活跃");

        let sessions = svc.get_session_metrics();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, 100);
        assert_eq!(sessions[0].causal_depth, 5);
        assert_eq!(sessions[0].version, 3);
        assert_eq!(sessions[0].pending_io_count, 2);
        assert!(!sessions[0].is_finished);
        assert_eq!(sessions[0].current_phase, "running");

        // Prometheus 指标应反映会话状态
        let text = svc.get_metrics_text();
        assert!(text.contains("evorule_sessions_total 1"));
        assert!(text.contains("evorule_sessions_active 1"));
        assert!(text.contains(r#"evorule_session_causal_depth{session_id="100"} 5"#));
        assert!(text.contains(r#"evorule_session_version{session_id="100"} 3"#));
        assert!(text.contains(r#"evorule_session_pending_io{session_id="100"} 2"#));
        assert!(text.contains(r#"evorule_session_finished{session_id="100"} 0"#));
    }

    #[tokio::test]
    async fn test_poll_once_finished_session() {
        let mut server = Server::new_async().await;
        server
            .mock("GET", "/api/sessions")
            .with_status(200)
            .with_body(r#"{"sessions":[200]}"#)
            .create_async()
            .await;
        server
            .mock("GET", "/api/sessions/200/state")
            .with_status(200)
            .with_body(reactor_state_json(10, 7, 1, 0, "finished").to_string())
            .create_async()
            .await;

        let svc = new_service(server.url());
        let client = reqwest::Client::new();
        svc.poll_once(&client).await;

        let sm = svc.get_server_metrics();
        assert_eq!(sm.session_count, 1);
        assert_eq!(sm.active_sessions, 0, "finished 会话不应计为活跃");

        let sessions = svc.get_session_metrics();
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].is_finished);
        assert_eq!(sessions[0].invariant_violations, 1);

        let text = svc.get_metrics_text();
        assert!(text.contains("evorule_sessions_active 0"));
        assert!(text.contains(r#"evorule_session_finished{session_id="200"} 1"#));
        assert!(
            text.contains(r#"evorule_session_structural_invariant_violations{session_id="200"} 1"#)
        );
    }

    #[tokio::test]
    async fn test_poll_once_multiple_sessions_mixed() {
        let mut server = Server::new_async().await;
        server
            .mock("GET", "/api/sessions")
            .with_status(200)
            .with_body(r#"{"sessions":[1,2,3]}"#)
            .create_async()
            .await;
        server
            .mock("GET", "/api/sessions/1/state")
            .with_status(200)
            .with_body(reactor_state_json(1, 1, 0, 0, "running").to_string())
            .create_async()
            .await;
        server
            .mock("GET", "/api/sessions/2/state")
            .with_status(200)
            .with_body(reactor_state_json(2, 2, 0, 0, "finished").to_string())
            .create_async()
            .await;
        server
            .mock("GET", "/api/sessions/3/state")
            .with_status(200)
            .with_body(reactor_state_json(3, 3, 0, 1, "running").to_string())
            .create_async()
            .await;

        let svc = new_service(server.url());
        let client = reqwest::Client::new();
        svc.poll_once(&client).await;

        let sm = svc.get_server_metrics();
        assert_eq!(sm.session_count, 3);
        assert_eq!(sm.active_sessions, 2, "2 个 running + 1 个 finished");

        let sessions = svc.get_session_metrics();
        assert_eq!(sessions.len(), 3);
    }

    #[tokio::test]
    async fn test_poll_once_session_disappear_removes_labels() {
        // 第一次轮询：会话 1 存在
        let mut server = Server::new_async().await;
        let m1 = server
            .mock("GET", "/api/sessions")
            .with_status(200)
            .with_body(r#"{"sessions":[1]}"#)
            .create_async()
            .await;
        server
            .mock("GET", "/api/sessions/1/state")
            .with_status(200)
            .with_body(reactor_state_json(1, 1, 0, 0, "running").to_string())
            .create_async()
            .await;

        let svc = new_service(server.url());
        let client = reqwest::Client::new();
        svc.poll_once(&client).await;
        let text_after_first = svc.get_metrics_text();
        assert!(text_after_first.contains(r#"evorule_session_version{session_id="1"}"#));
        m1.remove();

        // 第二次轮询：会话 1 已消失
        server
            .mock("GET", "/api/sessions")
            .with_status(200)
            .with_body(r#"{"sessions":[]}"#)
            .create_async()
            .await;
        svc.poll_once(&client).await;

        let text_after_second = svc.get_metrics_text();
        assert!(
            !text_after_second.contains(r#"session_id="1""#),
            "已消失会话的标签应被清理"
        );
        let sm = svc.get_server_metrics();
        assert_eq!(sm.session_count, 0);
    }

    #[tokio::test]
    async fn test_poll_once_with_semantic_stats() {
        let mut server = Server::new_async().await;
        let mut si_server = Server::new_async().await;

        server
            .mock("GET", "/api/sessions")
            .with_status(200)
            .with_body(r#"{"sessions":[]}"#)
            .create_async()
            .await;
        si_server
            .mock("GET", "/api/stats")
            .with_status(200)
            .with_body(
                serde_json::json!({
                    "total_rules": 42,
                    "total_violations": 7,
                    "violations_by_severity": {
                        "critical": 1,
                        "high": 2,
                        "medium": 3,
                        "low": 1
                    },
                    "sessions_with_violations": 3,
                    "monitored_sessions": 10
                })
                .to_string(),
            )
            .create_async()
            .await;

        let svc = new_service_with_si(server.url(), si_server.url());
        let client = reqwest::Client::new();
        svc.poll_once(&client).await;

        let text = svc.get_metrics_text();
        assert!(text.contains("evorule_semantic_rules_total 42"));
        assert!(text.contains("evorule_semantic_violations_total 7"));
        assert!(text.contains(r#"evorule_semantic_violations_by_severity{severity="critical"} 1"#));
        assert!(text.contains(r#"evorule_semantic_violations_by_severity{severity="high"} 2"#));
        assert!(text.contains(r#"evorule_semantic_violations_by_severity{severity="medium"} 3"#));
        assert!(text.contains(r#"evorule_semantic_violations_by_severity{severity="low"} 1"#));
    }

    #[tokio::test]
    async fn test_poll_once_semantic_stats_partial_severity() {
        // SI 响应只包含部分严重级别——缺失的应被填 0
        let mut server = Server::new_async().await;
        let mut si_server = Server::new_async().await;

        server
            .mock("GET", "/api/sessions")
            .with_status(200)
            .with_body(r#"{"sessions":[]}"#)
            .create_async()
            .await;
        si_server
            .mock("GET", "/api/stats")
            .with_status(200)
            .with_body(
                serde_json::json!({
                    "total_rules": 5,
                    "total_violations": 2,
                    "violations_by_severity": {
                        "critical": 2
                    },
                    "sessions_with_violations": 0,
                    "monitored_sessions": 0
                })
                .to_string(),
            )
            .create_async()
            .await;

        let svc = new_service_with_si(server.url(), si_server.url());
        let client = reqwest::Client::new();
        svc.poll_once(&client).await;

        let text = svc.get_metrics_text();
        assert!(text.contains(r#"evorule_semantic_violations_by_severity{severity="critical"} 2"#));
        // 缺失的 high/medium/low 应为 0
        assert!(text.contains(r#"evorule_semantic_violations_by_severity{severity="high"} 0"#));
        assert!(text.contains(r#"evorule_semantic_violations_by_severity{severity="medium"} 0"#));
        assert!(text.contains(r#"evorule_semantic_violations_by_severity{severity="low"} 0"#));
    }

    #[tokio::test]
    async fn test_poll_once_network_error_does_not_panic() {
        // 指向不存在的端口——网络错误不应 panic，应降级为空列表
        let svc = new_service("http://127.0.0.1:1".to_string());
        let client = reqwest::Client::new();
        svc.poll_once(&client).await;

        let sm = svc.get_server_metrics();
        assert_eq!(sm.session_count, 0);
        assert_eq!(sm.active_sessions, 0);
    }

    #[tokio::test]
    async fn test_poll_once_invalid_sessions_json() {
        let mut server = Server::new_async().await;
        server
            .mock("GET", "/api/sessions")
            .with_status(200)
            .with_body(r#"not a json"#)
            .create_async()
            .await;

        let svc = new_service(server.url());
        let client = reqwest::Client::new();
        svc.poll_once(&client).await;

        // 解析失败应降级为空列表
        let sm = svc.get_server_metrics();
        assert_eq!(sm.session_count, 0);
    }

    #[tokio::test]
    async fn test_poll_once_invalid_state_json_skipped() {
        let mut server = Server::new_async().await;
        server
            .mock("GET", "/api/sessions")
            .with_status(200)
            .with_body(r#"{"sessions":[1,2]}"#)
            .create_async()
            .await;
        // 会话 1 的状态响应有效
        server
            .mock("GET", "/api/sessions/1/state")
            .with_status(200)
            .with_body(reactor_state_json(1, 1, 0, 0, "running").to_string())
            .create_async()
            .await;
        // 会话 2 的状态响应是无效 JSON——应被跳过，不影响会话 1
        server
            .mock("GET", "/api/sessions/2/state")
            .with_status(200)
            .with_body(r#"broken"#)
            .create_async()
            .await;

        let svc = new_service(server.url());
        let client = reqwest::Client::new();
        svc.poll_once(&client).await;

        // 会话计数仍为 2（基于列表），但只有会话 1 进入快照
        let sm = svc.get_server_metrics();
        assert_eq!(sm.session_count, 2);
        assert_eq!(sm.active_sessions, 1, "只有会话 1 解析成功且 active");

        let sessions = svc.get_session_metrics();
        assert_eq!(sessions.len(), 1, "无效状态的会话应被跳过");
        assert_eq!(sessions[0].session_id, 1);
    }

    #[tokio::test]
    async fn test_poll_once_semantic_network_error_keeps_previous() {
        // SI 服务网络错误：现有指标应保持（不被清零），错误仅记录 warn
        let mut server = Server::new_async().await;
        let mut si_server = Server::new_async().await;

        server
            .mock("GET", "/api/sessions")
            .with_status(200)
            .with_body(r#"{"sessions":[]}"#)
            .create_async()
            .await;
        let m_ok = si_server
            .mock("GET", "/api/stats")
            .with_status(200)
            .with_body(
                serde_json::json!({
                    "total_rules": 10,
                    "total_violations": 5,
                    "violations_by_severity": {"critical": 5},
                    "sessions_with_violations": 0,
                    "monitored_sessions": 0
                })
                .to_string(),
            )
            .create_async()
            .await;

        let svc = new_service_with_si(server.url(), si_server.url());
        let client = reqwest::Client::new();
        svc.poll_once(&client).await;
        m_ok.remove();

        // 第二次：SI 服务返回 500
        si_server
            .mock("GET", "/api/stats")
            .with_status(500)
            .create_async()
            .await;
        svc.poll_once(&client).await;

        let text = svc.get_metrics_text();
        // 500 错误时 resp.json() 失败——指标保持上次值
        assert!(text.contains("evorule_semantic_rules_total 10"));
        assert!(text.contains("evorule_semantic_violations_total 5"));
    }

    // ============ handler oneshot 测试 ============

    #[tokio::test]
    async fn test_handler_metrics_initial() {
        let svc = new_service("http://x".to_string());
        let router = build_router(svc);

        let (status, body) = send_request(
            router,
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("evorule_sessions_total"));
        assert!(body.contains("evorule_semantic_violations_by_severity"));
    }

    #[tokio::test]
    async fn test_handler_sessions_initial_empty() {
        let svc = new_service("http://x".to_string());
        let router = build_router(svc);

        let (status, body) = send_request(
            router,
            Request::builder()
                .uri("/sessions")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "[]", "初始 /sessions 应返回空数组");
    }

    #[tokio::test]
    async fn test_handler_status_initial_zero() {
        let svc = new_service("http://x".to_string());
        let router = build_router(svc);

        let (status, body) = send_request(
            router,
            Request::builder()
                .uri("/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body).expect("应返回有效 JSON");
        assert_eq!(v["session_count"], 0);
        assert_eq!(v["active_sessions"], 0);
    }

    #[tokio::test]
    async fn test_handler_metrics_after_poll() {
        let mut server = Server::new_async().await;
        server
            .mock("GET", "/api/sessions")
            .with_status(200)
            .with_body(r#"{"sessions":[42]}"#)
            .create_async()
            .await;
        server
            .mock("GET", "/api/sessions/42/state")
            .with_status(200)
            .with_body(reactor_state_json(8, 4, 0, 0, "running").to_string())
            .create_async()
            .await;

        let svc = new_service(server.url());
        let client = reqwest::Client::new();
        svc.poll_once(&client).await;

        let router = build_router(svc);
        let (status, body) = send_request(
            router,
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("evorule_sessions_total 1"));
        assert!(body.contains("evorule_sessions_active 1"));
        assert!(body.contains(r#"evorule_session_causal_depth{session_id="42"} 8"#));
    }

    #[tokio::test]
    async fn test_handler_sessions_after_poll() {
        let mut server = Server::new_async().await;
        server
            .mock("GET", "/api/sessions")
            .with_status(200)
            .with_body(r#"{"sessions":[7,8]}"#)
            .create_async()
            .await;
        server
            .mock("GET", "/api/sessions/7/state")
            .with_status(200)
            .with_body(reactor_state_json(1, 1, 0, 0, "running").to_string())
            .create_async()
            .await;
        server
            .mock("GET", "/api/sessions/8/state")
            .with_status(200)
            .with_body(reactor_state_json(2, 2, 0, 0, "finished").to_string())
            .create_async()
            .await;

        let svc = new_service(server.url());
        let client = reqwest::Client::new();
        svc.poll_once(&client).await;

        let router = build_router(svc);
        let (status, body) = send_request(
            router,
            Request::builder()
                .uri("/sessions")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body).expect("应返回有效 JSON 数组");
        let arr = v.as_array().expect("应是数组");
        assert_eq!(arr.len(), 2);
        // 验证字段完整
        assert_eq!(arr[0]["session_id"], 7);
        assert_eq!(arr[0]["is_finished"], false);
        assert_eq!(arr[1]["session_id"], 8);
        assert_eq!(arr[1]["is_finished"], true);
    }

    #[tokio::test]
    async fn test_handler_status_after_poll() {
        let mut server = Server::new_async().await;
        server
            .mock("GET", "/api/sessions")
            .with_status(200)
            .with_body(r#"{"sessions":[1,2,3]}"#)
            .create_async()
            .await;
        for sid in [1, 2, 3] {
            let phase = if sid == 2 { "finished" } else { "running" };
            let state_path = format!("/api/sessions/{sid}/state");
            server
                .mock("GET", state_path.as_str())
                .with_status(200)
                .with_body(reactor_state_json(sid as i64, sid as i64, 0, 0, phase).to_string())
                .create_async()
                .await;
        }

        let svc = new_service(server.url());
        let client = reqwest::Client::new();
        svc.poll_once(&client).await;

        let router = build_router(svc);
        let (status, body) = send_request(
            router,
            Request::builder()
                .uri("/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body).expect("应返回有效 JSON");
        assert_eq!(v["session_count"], 3);
        assert_eq!(v["active_sessions"], 2, "2 running + 1 finished");
    }

    #[tokio::test]
    async fn test_handler_unknown_route_returns_404() {
        let svc = new_service("http://x".to_string());
        let router = build_router(svc);

        let (status, _) = send_request(
            router,
            Request::builder()
                .uri("/nonexistent")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_handler_wrong_method_returns_405() {
        let svc = new_service("http://x".to_string());
        let router = build_router(svc);

        // /metrics 只支持 GET
        let (status, _) = send_request(
            router,
            Request::builder()
                .method("POST")
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Prometheus 指标实现（应用层）
//!
//! 实现 `evorule_governance::metrics::IoMetrics` trait，通过 Prometheus 收集指标。
//! 从 evorule-governance/src/metrics.rs 迁移而来（H6 架构合规整改）。
//!
//! # 指标列表
//! | 指标 | 类型 | 标签 | 说明 |
//! |------|------|------|------|
//! | `evorule_sessions_active` | Gauge | — | 当前活跃会话数 |
//! | `evorule_commands_total` | Counter | `type` | 命令提交总数（按指令类型） |
//! | `evorule_io_duration_seconds` | Histogram | `io_type` | I/O 调用耗时（按 I/O 类型） |
//! | `evorule_io_errors_total` | Counter | `io_type` | I/O 调用失败总数 |
//! | `evorule_facts_log_version` | Gauge | — | FactsLog 当前版本号 |
//! | `evorule_sse_connections_active` | Gauge | — | 当前活跃 SSE 连接数 |
//! | `evorule_http_requests_total` | Counter | `method`, `path`, `status` | HTTP 请求总数 |
//! | `evorule_sanitize_hits_total` | Counter | `rule` | L1 输入净化命中数（P5-A1，2026-08-27） |
//! | `evorule_auto_verify_failures_total` | Counter | — | 实时审计验证失败次数（P5-A2，2026-08-27） |
//! | `evorule_auto_verify_skips_total` | Counter | — | 自动审计验证跳过次数（P5-A2，2026-08-27） |

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use evorule_governance::metrics::IoMetrics;
use prometheus::{
    HistogramOpts, HistogramVec, IntCounterVec, IntGauge, Opts, Registry, TextEncoder,
};

/// 指标创建错误
#[derive(Debug)]
pub enum MetricsError {
    /// Gauge 指标创建失败
    GaugeCreation(String),
    /// Counter 指标创建失败
    CounterCreation(String),
    /// Histogram 指标创建失败
    HistogramCreation(String),
    /// 指标注册到 Registry 失败
    RegistryRegistration(String),
}

impl fmt::Display for MetricsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MetricsError::GaugeCreation(name) => {
                write!(f, "Failed to create gauge: {}", name)
            }
            MetricsError::CounterCreation(name) => {
                write!(f, "Failed to create counter: {}", name)
            }
            MetricsError::HistogramCreation(name) => {
                write!(f, "Failed to create histogram: {}", name)
            }
            MetricsError::RegistryRegistration(name) => {
                write!(f, "Failed to register metric: {}", name)
            }
        }
    }
}

impl std::error::Error for MetricsError {}

/// Prometheus 指标实现
///
/// 持有独立的 `Registry`（非全局），便于测试隔离。
/// 所有指标在 `new()` 时注册到 registry，之后通过 `render_as_text()` 输出 Prometheus 文本格式。
///
/// # H6 迁移说明
/// 原 `evorule_governance::metrics::Metrics` 的完整 Prometheus 实现已迁移到此处。
/// 核心层仅保留 `IoMetrics` trait 定义，通过依赖注入使用此实现。
pub struct PrometheusMetrics {
    registry: Registry,
    sessions_active: IntGauge,
    commands_total: IntCounterVec,
    io_duration_seconds: HistogramVec,
    io_errors_total: IntCounterVec,
    facts_log_version: IntGauge,
    sse_connections_active: IntGauge,
    http_requests_total: IntCounterVec,
    /// P5-A1：L1 净化命中计数（按 rule 标签）
    sanitize_hits_total: IntCounterVec,
    /// P5-A2：实时审计验证失败计数
    auto_verify_failures_total: prometheus::IntCounter,
    /// P5-A2：自动验证跳过计数
    auto_verify_skips_total: prometheus::IntCounter,
}

impl PrometheusMetrics {
    /// 创建并注册所有指标
    // 多指标注册 + 错误处理, 拆函数需共享 registry 状态。详见 GATE_REFERENCE.md §六(豁免索引)
    #[allow(clippy::too_many_lines)]
    pub fn new() -> Result<Self, MetricsError> {
        let registry = Registry::new();

        let sessions_active =
            IntGauge::new("evorule_sessions_active", "Current active sessions")
                .map_err(|_| MetricsError::GaugeCreation("evorule_sessions_active".to_string()))?;
        let commands_total = IntCounterVec::new(
            Opts::new(
                "evorule_commands_total",
                "Total commands submitted by instruction type",
            ),
            &["type"],
        )
        .map_err(|_| MetricsError::CounterCreation("evorule_commands_total".to_string()))?;
        let io_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "evorule_io_duration_seconds",
                "I/O call duration in seconds by io_type",
            )
            .buckets(vec![
                0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
            ]),
            &["io_type"],
        )
        .map_err(|_| MetricsError::HistogramCreation("evorule_io_duration_seconds".to_string()))?;
        let io_errors_total = IntCounterVec::new(
            Opts::new(
                "evorule_io_errors_total",
                "Total I/O call failures by io_type",
            ),
            &["io_type"],
        )
        .map_err(|_| MetricsError::CounterCreation("evorule_io_errors_total".to_string()))?;
        let facts_log_version =
            IntGauge::new("evorule_facts_log_version", "Current FactsLog version").map_err(
                |_| MetricsError::GaugeCreation("evorule_facts_log_version".to_string()),
            )?;
        let sse_connections_active = IntGauge::new(
            "evorule_sse_connections_active",
            "Current active SSE connections",
        )
        .map_err(|_| MetricsError::GaugeCreation("evorule_sse_connections_active".to_string()))?;
        let http_requests_total = IntCounterVec::new(
            Opts::new(
                "evorule_http_requests_total",
                "Total HTTP requests by method, path and status",
            ),
            &["method", "path", "status"],
        )
        .map_err(|_| MetricsError::CounterCreation("evorule_http_requests_total".to_string()))?;
        // P5-A1：净化命中按 rule 打标签，攻击态势可告警
        let sanitize_hits_total = IntCounterVec::new(
            Opts::new(
                "evorule_sanitize_hits_total",
                "L1 input sanitizer hits by rule name",
            ),
            &["rule"],
        )
        .map_err(|_| MetricsError::CounterCreation("evorule_sanitize_hits_total".to_string()))?;
        let auto_verify_failures_total = prometheus::IntCounter::new(
            "evorule_auto_verify_failures_total",
            "Realtime audit-chain verify failures (tamper indicator, alert on >0)",
        )
        .map_err(|_| {
            MetricsError::CounterCreation("evorule_auto_verify_failures_total".to_string())
        })?;
        let auto_verify_skips_total = prometheus::IntCounter::new(
            "evorule_auto_verify_skips_total",
            "Auto audit-verify skips (threshold/interval), measures real verification coverage",
        )
        .map_err(|_| {
            MetricsError::CounterCreation("evorule_auto_verify_skips_total".to_string())
        })?;

        registry
            .register(Box::new(sessions_active.clone()))
            .map_err(|_| {
                MetricsError::RegistryRegistration("evorule_sessions_active".to_string())
            })?;
        registry
            .register(Box::new(commands_total.clone()))
            .map_err(|_| {
                MetricsError::RegistryRegistration("evorule_commands_total".to_string())
            })?;
        registry
            .register(Box::new(io_duration_seconds.clone()))
            .map_err(|_| {
                MetricsError::RegistryRegistration("evorule_io_duration_seconds".to_string())
            })?;
        registry
            .register(Box::new(io_errors_total.clone()))
            .map_err(|_| {
                MetricsError::RegistryRegistration("evorule_io_errors_total".to_string())
            })?;
        registry
            .register(Box::new(facts_log_version.clone()))
            .map_err(|_| {
                MetricsError::RegistryRegistration("evorule_facts_log_version".to_string())
            })?;
        registry
            .register(Box::new(sse_connections_active.clone()))
            .map_err(|_| {
                MetricsError::RegistryRegistration("evorule_sse_connections_active".to_string())
            })?;
        registry
            .register(Box::new(http_requests_total.clone()))
            .map_err(|_| {
                MetricsError::RegistryRegistration("evorule_http_requests_total".to_string())
            })?;
        registry
            .register(Box::new(sanitize_hits_total.clone()))
            .map_err(|_| {
                MetricsError::RegistryRegistration("evorule_sanitize_hits_total".to_string())
            })?;
        registry
            .register(Box::new(auto_verify_failures_total.clone()))
            .map_err(|_| {
                MetricsError::RegistryRegistration("evorule_auto_verify_failures_total".to_string())
            })?;
        registry
            .register(Box::new(auto_verify_skips_total.clone()))
            .map_err(|_| {
                MetricsError::RegistryRegistration("evorule_auto_verify_skips_total".to_string())
            })?;

        Ok(Self {
            registry,
            sessions_active,
            commands_total,
            io_duration_seconds,
            io_errors_total,
            facts_log_version,
            sse_connections_active,
            http_requests_total,
            sanitize_hits_total,
            auto_verify_failures_total,
            auto_verify_skips_total,
        })
    }
}

impl IoMetrics for PrometheusMetrics {
    fn observe_io_duration(&self, io_type: &str, duration: Duration) {
        self.io_duration_seconds
            .with_label_values(&[io_type])
            .observe(duration.as_secs_f64());
    }

    fn inc_io_errors(&self, io_type: &str) {
        self.io_errors_total.with_label_values(&[io_type]).inc();
    }

    fn inc_sessions(&self) {
        self.sessions_active.inc();
    }

    fn dec_sessions(&self) {
        self.sessions_active.dec();
    }

    fn set_sessions(&self, n: i64) {
        self.sessions_active.set(n);
    }

    fn inc_commands(&self, instruction_type: &str) {
        self.commands_total
            .with_label_values(&[instruction_type])
            .inc();
    }

    fn set_facts_log_version(&self, version: u64) {
        self.facts_log_version.set(version as i64);
    }

    fn inc_sse_connections(&self) {
        self.sse_connections_active.inc();
    }

    fn dec_sse_connections(&self) {
        self.sse_connections_active.dec();
    }

    fn set_sse_connections(&self, n: i64) {
        self.sse_connections_active.set(n);
    }

    fn inc_http_requests(&self, method: &str, path: &str, status: &str) {
        self.http_requests_total
            .with_label_values(&[method, path, status])
            .inc();
    }

    fn inc_sanitize_hits(&self, rule: &str) {
        self.sanitize_hits_total.with_label_values(&[rule]).inc();
    }

    fn inc_auto_verify_failures(&self) {
        self.auto_verify_failures_total.inc();
    }

    fn inc_auto_verify_skips(&self) {
        self.auto_verify_skips_total.inc();
    }

    fn render_as_text(&self) -> String {
        let encoder = TextEncoder::new();
        let mfs = self.registry.gather();
        encoder
            .encode_to_string(&mfs)
            .unwrap_or_else(|e| format!("# encoding error: {e}"))
    }
}

/// 构造共享的 Prometheus 指标引用（供 `IoSubscriber::with_metrics()` 注入）
///
/// 返回 `SharedMetrics`（即 `Arc<dyn IoMetrics>`），可直接传给核心层的 `IoSubscriber`。
pub fn shared_prometheus_metrics() -> Result<Arc<PrometheusMetrics>, MetricsError> {
    Ok(Arc::new(PrometheusMetrics::new()?))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::panic, clippy::expect_used)]
    use super::*;
    use evorule_governance::metrics::IoMetrics;

    fn make_metrics() -> PrometheusMetrics {
        PrometheusMetrics::new().unwrap()
    }

    #[test]
    fn test_metrics_new_registers_all() {
        let m = make_metrics();
        m.inc_commands("init");
        m.observe_io_duration("call_external", Duration::from_secs(0));
        m.inc_io_errors("call_external");
        m.inc_http_requests("GET", "/", "200");

        let output = m.render_as_text();
        assert!(output.contains("evorule_sessions_active"));
        assert!(output.contains("evorule_commands_total"));
        assert!(output.contains("evorule_io_duration_seconds"));
        assert!(output.contains("evorule_io_errors_total"));
        assert!(output.contains("evorule_facts_log_version"));
        assert!(output.contains("evorule_sse_connections_active"));
        assert!(output.contains("evorule_http_requests_total"));
    }

    #[test]
    fn test_render_outputs_text_format() {
        let m = make_metrics();
        m.inc_sessions();
        m.inc_commands("increment");
        let output = m.render_as_text();
        assert!(output.contains("evorule_sessions_active"));
        assert!(output.contains("evorule_commands_total"));
        assert!(output.contains("1"));
    }

    #[test]
    fn test_sessions_gauge() {
        let m = make_metrics();
        m.inc_sessions();
        m.inc_sessions();
        m.dec_sessions();
        let output = m.render_as_text();
        assert!(output.contains("evorule_sessions_active 1"));
    }

    #[test]
    fn test_commands_counter_by_type() {
        let m = make_metrics();
        m.inc_commands("increment");
        m.inc_commands("increment");
        m.inc_commands("set");
        let output = m.render_as_text();
        assert!(output.contains("evorule_commands_total{type=\"increment\"} 2"));
        assert!(output.contains("evorule_commands_total{type=\"set\"} 1"));
    }

    #[test]
    fn test_io_duration_histogram() {
        let m = make_metrics();
        m.observe_io_duration("call_external", Duration::from_millis(150));
        m.observe_io_duration("call_external", Duration::from_millis(350));
        let output = m.render_as_text();
        assert!(output.contains("evorule_io_duration_seconds_bucket"));
        assert!(output.contains("evorule_io_duration_seconds_count"));
        assert!(output.contains("evorule_io_duration_seconds_sum"));
    }

    #[test]
    fn test_facts_log_version_gauge() {
        let m = make_metrics();
        m.set_facts_log_version(42);
        let output = m.render_as_text();
        assert!(output.contains("evorule_facts_log_version 42"));
    }

    #[test]
    fn test_sse_connections_gauge() {
        let m = make_metrics();
        m.inc_sse_connections();
        m.inc_sse_connections();
        m.set_sse_connections(5);
        let output = m.render_as_text();
        assert!(output.contains("evorule_sse_connections_active 5"));
    }

    #[test]
    fn test_http_requests_counter() {
        let m = make_metrics();
        m.inc_http_requests("GET", "/api/health", "200");
        m.inc_http_requests("POST", "/api/command", "200");
        m.inc_http_requests("GET", "/api/health", "200");
        let output = m.render_as_text();
        assert!(output.contains("method=\"GET\""));
        assert!(output.contains("path=\"/api/health\""));
        assert!(output.contains("status=\"200\""));
    }

    #[test]
    fn test_io_errors_counter() {
        let m = make_metrics();
        m.inc_io_errors("call_external");
        m.inc_io_errors("call_external");
        let output = m.render_as_text();
        assert!(output.contains("evorule_io_errors_total{io_type=\"call_external\"} 2"));
    }

    /// 验证 PrometheusMetrics 可以被作为 IoMetrics trait object 使用
    #[test]
    fn test_can_be_used_as_io_metrics_trait_object() {
        let m = make_metrics();
        let shared: Arc<dyn IoMetrics> = Arc::new(m);
        shared.inc_sessions();
        shared.inc_commands("test");
        shared.observe_io_duration("call_external", Duration::from_millis(100));
        // render_as_text 通过 trait object 调用
        let output = shared.render_as_text();
        assert!(output.contains("evorule_sessions_active 1"));
        assert!(output.contains("evorule_commands_total{type=\"test\"} 1"));
    }

    /// 验证 shared_prometheus_metrics() 构造函数正常工作
    #[test]
    fn test_shared_prometheus_metrics_constructor() {
        let m = shared_prometheus_metrics().unwrap();
        m.inc_sessions();
        let output = m.render_as_text();
        assert!(output.contains("evorule_sessions_active 1"));
    }

    /// P5-A1/A2 审计指标：sanit化命中按 rule 分桶、auto_verify 失败/跳过计数
    #[test]
    fn test_audit_counters_via_trait_object() {
        let m = make_metrics();
        let shared: Arc<dyn IoMetrics> = Arc::new(m);
        shared.inc_sanitize_hits("regex_injection");
        shared.inc_sanitize_hits("regex_injection");
        shared.inc_sanitize_hits("tool_prompt_block");
        shared.inc_auto_verify_failures();
        shared.inc_auto_verify_skips();
        let output = shared.render_as_text();
        assert!(output.contains(
            "evorule_sanitize_hits_total{rule=\"regex_injection\"} 2"
        ));
        assert!(output.contains(
            "evorule_sanitize_hits_total{rule=\"tool_prompt_block\"} 1"
        ));
        assert!(output.contains("evorule_auto_verify_failures_total 1"));
        assert!(output.contains("evorule_auto_verify_skips_total 1"));
    }
}

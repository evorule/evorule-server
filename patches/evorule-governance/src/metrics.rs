// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! I/O 指标收集接口（机制层 trait 定义）
//!
//! 本模块仅定义 `IoMetrics` trait 和默认的 `NoOpMetrics` 实现。
//! 具体的 Prometheus 实现由应用层提供（evorule-server 独立仓 evorule-server/src/metrics_impl.rs + core/metrics/）。
//!
//! # 设计理由
//! 指标收集是可观测性能力，属于应用层（AGENTS.md 边界判断表：
//! "加个 Prometheus 指标 → 放 evorule-application / evorule-server（可观测性是应用层）"）。
//! 核心层通过 trait 抽象暴露指标收集点，应用层通过依赖注入注入具体实现
//!（如 Prometheus、OpenTelemetry 等）。
//!
//! # H6 架构合规整改
//! 原 `metrics.rs` 直接依赖 `prometheus` crate，违反机制-策略分离原则。
//! 现重写为 trait 定义，移除 prometheus 依赖，消除 RUSTSEC-2024-0437 漏洞链路。
//!
//! # 指标列表（trait 暴露的收集点）
//! | 方法 | 对应原 Prometheus 指标 | 说明 |
//! |------|----------------------|------|
//! | `observe_io_duration` | `evorule_io_duration_seconds` | I/O 调用耗时（按 io_type） |
//! | `inc_io_errors` | `evorule_io_errors_total` | I/O 调用失败总数 |
//! | `inc_sessions` / `dec_sessions` / `set_sessions` | `evorule_sessions_active` | 当前活跃会话数 |
//! | `inc_commands` | `evorule_commands_total` | 命令提交总数（按指令类型） |
//! | `set_facts_log_version` | `evorule_facts_log_version` | FactsLog 当前版本号 |
//! | `inc_sse_connections` / `dec_sse_connections` / `set_sse_connections` | `evorule_sse_connections_active` | SSE 连接数 |
//! | `inc_http_requests` | `evorule_http_requests_total` | HTTP 请求总数 |
//! | `inc_sanitize_hits` | `evorule_sanitize_hits_total` | 输入净化命中数（P5-A1，2026-08-27） |
//! | `inc_auto_verify_failures` | `evorule_auto_verify_failures_total` | 实时审计验证失败次数（P5-A2，2026-08-27） |
//! | `inc_auto_verify_skips` | `evorule_auto_verify_skips_total` | 自动验证跳过次数（P5-A2，2026-08-27） |
//! | `render_as_text` | `/metrics` 端点输出 | 渲染为 Prometheus 文本格式 |

use std::sync::Arc;
use std::time::Duration;

/// I/O 指标收集 trait（机制层接口定义）
///
/// 核心层通过此 trait 暴露指标收集点，应用层负责具体实现。
/// 默认实现 `NoOpMetrics` 所有方法空转，不引入任何依赖。
///
/// # 使用方式
/// - 核心层默认使用 `NoOpMetrics`（通过 `noop_metrics()` 构造）
/// - 应用层通过 `IoSubscriber::with_metrics()` 注入 Prometheus 等具体实现
/// - 所有方法都提供默认实现（空转），便于 trait 扩展时保持向后兼容
pub trait IoMetrics: Send + Sync {
    /// 记录 I/O 调用耗时（按 io_type 打标签）
    ///
    /// # 参数
    /// - `io_type`：I/O 类型字符串（如 "call_external"、"call_service"）
    /// - `duration`：本次 I/O 调用耗时
    fn observe_io_duration(&self, _io_type: &str, _duration: Duration) {}

    /// I/O 错误计数 +1（按 io_type 打标签）
    fn inc_io_errors(&self, _io_type: &str) {}

    /// 会话数 +1（创建会话时调用）
    fn inc_sessions(&self) {}

    /// 会话数 -1（关闭会话时调用）
    fn dec_sessions(&self) {}

    /// 设置当前活跃会话数（批量同步时调用）
    fn set_sessions(&self, _n: i64) {}

    /// 命令计数 +1（按指令类型打标签）
    fn inc_commands(&self, _instruction_type: &str) {}

    /// 设置 FactsLog 当前版本号
    fn set_facts_log_version(&self, _version: u64) {}

    /// SSE 连接数 +1
    fn inc_sse_connections(&self) {}

    /// SSE 连接数 -1
    fn dec_sse_connections(&self) {}

    /// 设置当前 SSE 连接数（从计数器同步时调用）
    fn set_sse_connections(&self, _n: i64) {}

    /// HTTP 请求计数 +1（按 method/path/status 打标签）
    fn inc_http_requests(&self, _method: &str, _path: &str, _status: &str) {}

    /// 输入净化命中计数 +1（P5-A1：攻击态势可指标化）
    ///
    /// 按 rule 打标签（如 "role_override_ignore_previous"），使 L1 防线
    /// 的命中情况可被监控告警，而非仅 stderr 日志。
    fn inc_sanitize_hits(&self, _rule: &str) {}

    /// 实时审计验证失败计数 +1（P5-A2：审计链篡改探测结果可观测）
    fn inc_auto_verify_failures(&self) {}

    /// 自动审计验证跳过计数 +1（P5-A2：因阈值/间隔跳过时留痕）
    fn inc_auto_verify_skips(&self) {}

    /// 渲染为 Prometheus 文本格式（供 `/metrics` 端点返回）
    ///
    /// 默认返回空字符串。应用层若需暴露 `/metrics` 端点，
    /// 应在实现中覆写此方法返回 Prometheus 文本格式数据。
    fn render_as_text(&self) -> String {
        String::new()
    }
}

/// 默认空实现（所有操作空转，不引入任何依赖）
///
/// 核心层默认使用此实现。应用层通过 `IoSubscriber::with_metrics()`
/// 注入 Prometheus 等具体实现。
#[derive(Debug, Default, Clone)]
pub struct NoOpMetrics;

impl IoMetrics for NoOpMetrics {
    // 所有方法使用 trait 默认实现（空转），无需覆写
    // 显式覆写 observe_io_duration 和 inc_io_errors 以表达"这两个是核心收集点"的语义
    fn observe_io_duration(&self, _io_type: &str, _duration: Duration) {}
    fn inc_io_errors(&self, _io_type: &str) {}
}

/// 共享指标引用（Arc 包装，供 handler 和后台任务共享）
///
/// 使用 trait object（`dyn IoMetrics`），应用层可注入任意实现。
pub type SharedMetrics = Arc<dyn IoMetrics>;

/// 构造默认的 NoOpMetrics 共享引用
///
/// 核心层默认使用此函数构造指标收集器。
/// 应用层应使用 `PrometheusMetrics::new()` 构造具体实现。
pub fn noop_metrics() -> SharedMetrics {
    Arc::new(NoOpMetrics)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::panic, clippy::expect_used)]
    use super::*;

    #[test]
    fn test_noop_metrics_does_not_panic() {
        let m = NoOpMetrics;
        // 调用所有方法，验证不 panic
        m.observe_io_duration("call_external", Duration::from_millis(100));
        m.inc_io_errors("call_external");
        m.inc_sessions();
        m.dec_sessions();
        m.set_sessions(5);
        m.inc_commands("increment");
        m.set_facts_log_version(42);
        m.inc_sse_connections();
        m.dec_sse_connections();
        m.set_sse_connections(3);
        m.inc_http_requests("GET", "/api/health", "200");
    }

    #[test]
    fn test_noop_metrics_render_returns_empty() {
        let m = NoOpMetrics;
        assert_eq!(m.render_as_text(), "");
    }

    #[test]
    fn test_shared_metrics_via_trait_object() {
        // 验证 trait object 可以正常构造和分发
        let m: SharedMetrics = noop_metrics();
        m.observe_io_duration("test", Duration::from_secs(0));
        m.inc_io_errors("test");
        m.inc_sessions();
        m.set_facts_log_version(1);
        // render_as_text 默认返回空字符串
        assert_eq!(m.render_as_text(), "");
    }

    #[test]
    fn test_shared_metrics_clone_preserves_behavior() {
        // 验证 Arc<dyn IoMetrics> 可以 clone 且行为不变
        let m1: SharedMetrics = noop_metrics();
        let m2 = m1.clone();
        m1.observe_io_duration("a", Duration::from_millis(1));
        m2.observe_io_duration("b", Duration::from_millis(2));
        // NoOp 不 panic 即可
    }

    /// 辅助测试：验证自定义实现可以被注入到 SharedMetrics
    ///
    /// 使用共享的 `Arc<AtomicU32>` 计数器（而非 Arc::downcast），
    /// 因为 `Arc::downcast` 要求 trait object 是 `dyn Any + Send + Sync`，
    /// 而 `dyn IoMetrics` 不满足此约束。
    struct CountingMetrics {
        count: std::sync::Arc<std::sync::atomic::AtomicU32>,
    }

    impl IoMetrics for CountingMetrics {
        fn inc_sessions(&self) {
            self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[test]
    fn test_custom_metrics_can_be_injected() {
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let m: SharedMetrics = Arc::new(CountingMetrics {
            count: counter.clone(),
        });
        m.inc_sessions();
        m.inc_sessions();
        m.inc_sessions();
        // 验证自定义实现被正确调用（通过共享计数器，无需 downcast）
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 3);
    }
}

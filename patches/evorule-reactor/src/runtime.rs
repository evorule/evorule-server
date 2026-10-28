// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 运行时抽象层（Runtime trait 抽象·reactor 侧第一批）
//!
//! 目的：为"一套内核、多端形态"建立宿主能力缝——把时钟等宿主系统调用
//! 从反应器引擎中隔离到 trait 之后，使时间以数据形态进入纯逻辑层
//! （与 [`crate::pure`] 的"时间戳由调用方注入"设计对齐）。
//!
//! # 范围声明（诚实边界）
//!
//! - **时钟缝**：本批完整落地。引擎层 [`crate::reactor::Reactor`] 持有
//!   [`RuntimeClock`] 句柄；状态层提供 `_at` 变体（时间作参数），
//!   旧签名保留为兼容包装（内部仍取系统时钟，生产调用方迁移属后续批次）。
//! - **存储缝**：无需新建——[`crate::wal::FactWalStore`] trait +
//!   [`crate::wal::MemoryWalStore`]（无文件系统后端）+
//!   [`crate::facts_log::FactsLog::with_wal_store`] 注入点已构成完整缝，
//!   `WalWriter` 即其 native 文件系统后端。本批不重复发明。
//! - **字节等价铁律**：默认后端 [`SystemClock`] 行为与改造前的直接
//!   `Instant::now()` 逐字一致；native 行为等价由全量测试守护。
//! - Kani 形式化验证不受影响：纯层时间本就以参数注入
//!   （`register_io_request_pure` 先例），本批把同一纪律延伸到引擎层。

use std::sync::Arc;
use std::time::Instant;

/// 时钟抽象：反应器引擎经此获取当前时刻
///
/// 纯逻辑层不调用本 trait——时间以 `Instant` 参数流入
/// （见 `state` 模块的 `_at` 变体与 `pure` 模块的既有纪律）。
pub trait RuntimeClock: Send + Sync {
    /// 当前时刻
    fn now(&self) -> Instant;
}

/// 系统时钟（native 默认后端）
///
/// 行为与直接调用 `Instant::now()` 逐字一致（字节等价铁律）。
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl RuntimeClock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// 固定时钟（测试与确定性验证用）
///
/// 返回构造时给定的时刻，永不前进——使超时扫描等时间相关逻辑
/// 可确定性测试（无 sleep、无竞态）。
#[derive(Debug, Clone, Copy)]
pub struct FixedClock {
    instant: Instant,
}

impl FixedClock {
    /// 以指定时刻构造
    pub fn new(instant: Instant) -> Self {
        Self { instant }
    }
}

impl RuntimeClock for FixedClock {
    fn now(&self) -> Instant {
        self.instant
    }
}

/// 时钟句柄：引擎持有的 [`RuntimeClock`] 共享指针
///
/// 手工实现 [`std::fmt::Debug`]（dyn trait 无 Debug），
/// 以保持 [`crate::reactor::ReactorBuilder`] 的 `derive(Debug, Clone)` 不被破坏。
#[derive(Clone)]
pub struct ClockHandle(Arc<dyn RuntimeClock + Send + Sync>);

impl std::fmt::Debug for ClockHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClockHandle(<dyn RuntimeClock>)")
    }
}

impl ClockHandle {
    /// 包装任意时钟实现
    pub fn new(clock: Arc<dyn RuntimeClock + Send + Sync>) -> Self {
        Self(clock)
    }

    /// 以系统时钟构造（native 默认）
    pub fn system() -> Self {
        Self(Arc::new(SystemClock))
    }
}

impl RuntimeClock for ClockHandle {
    fn now(&self) -> Instant {
        self.0.now()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::fact::{FactId, IoType};
    use crate::state::ReactorState;
    use std::time::Duration;

    // 固定时钟：返回构造时刻，确定性验证的时间基座
    #[test]
    fn fixed_clock_returns_constructed_instant() {
        let base = Instant::now();
        let clock = FixedClock::new(base);
        assert_eq!(clock.now(), base);
        // 不随真实时间前进
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(clock.now(), base);
    }

    // 系统时钟：与真实时间单调一致（宽松断言：now 单调不减）
    #[test]
    fn system_clock_monotonic() {
        let clock = SystemClock;
        let a = clock.now();
        let b = clock.now();
        assert!(b >= a);
    }

    // 时钟缝端到端：FixedClock 驱动 scan_io_timeouts_at 的确定性判定
    // （超时扫描不依赖真实时间流逝——时间以数据进入，缝从引擎到判定全程贯通）
    #[test]
    fn fixed_clock_drives_deterministic_timeout_scan() {
        let mut state = ReactorState::new();
        let base = Instant::now();
        let clock = FixedClock::new(base);

        // 请求 A：在 base - 40s 发出（超 warn 30s，未超 error 60s）→ warn
        // 请求 B：在 base - 70s 发出（超 error 60s）→ error
        state.register_io_request_at(FactId(1), IoType::new("t"), base - Duration::from_secs(40));
        state.register_io_request_at(FactId(2), IoType::new("t"), base - Duration::from_secs(70));

        let (warn_ids, error_ids) = state.scan_io_timeouts_at(
            clock.now(),
            Duration::from_secs(30),
            Duration::from_secs(60),
        );
        assert_eq!(warn_ids, vec![FactId(1)]);
        assert_eq!(error_ids, vec![FactId(2)]);
    }

    // ClockHandle：透明转发
    #[test]
    fn clock_handle_forwards_to_inner_clock() {
        let base = Instant::now();
        let handle = ClockHandle::new(Arc::new(FixedClock::new(base)));
        assert_eq!(handle.now(), base);
        assert!(
            ClockHandle::system().now() <= Instant::now() + Duration::from_secs(1),
            "SystemClock 必须返回不晚于当前时刻的时间戳"
        );
    }
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 逻辑时钟（Lamport Clock）
//!
//! 用于跨组件事件排序，保证因果一致性。
//!
//! # 设计
//! 基于 Lamport 逻辑时钟算法：
//! - [`LogicalClock::tick`]：本地事件递增计数器并返回新值
//! - [`LogicalClock::merge`]：收到带时间戳的外部消息时，取 `max(local, other) + 1`
//!
//! 使用原子计数器实现，线程安全；[`Clone`] 后共享同一计数器实例。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// 逻辑时钟
///
/// 使用原子计数器实现，线程安全。
/// 每次 [`LogicalClock::tick`] 递增并返回新值。
/// 克隆后与原时钟共享同一计数器。
#[derive(Debug, Clone)]
pub struct LogicalClock {
    counter: Arc<AtomicU64>,
}

impl LogicalClock {
    /// 创建新时钟，初始值为 0
    ///
    /// # 示例
    ///
    /// > `no_run`：本示例仅作编译期用法演示。运行时行为由下方单元测试验证——
    /// > Windows 杀软/WDAC 会间歇性拦截批量 `cargo test --doc` 时刚编译的 doctest
    /// > 二进制（`os error 4551`，应用控制策略），为保证测试确定性，不在此执行。
    /// ```no_run
    /// use evorule_governance::LogicalClock;
    ///
    /// let clock = LogicalClock::new();
    /// assert_eq!(clock.current(), 0);
    ///
    /// // tick 单调递增
    /// let first = clock.tick();
    /// let second = clock.tick();
    /// assert_eq!(first, 1);
    /// assert_eq!(second, 2);
    ///
    /// // merge 取 max(local, other) + 1
    /// clock.merge(10);
    /// assert_eq!(clock.current(), 11);
    ///
    /// // Clone 后共享同一计数器
    /// let clone = clock.clone();
    /// clone.tick();
    /// assert_eq!(clock.current(), 12);
    /// ```
    pub fn new() -> Self {
        Self {
            counter: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 递增并返回新值
    ///
    /// 使用 `fetch_add` 原子递增；返回递增后的值（旧值 + 1）。
    pub fn tick(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// 获取当前值（不递增）
    pub fn current(&self) -> u64 {
        self.counter.load(Ordering::SeqCst)
    }

    /// 推进到目标值（终态为 `max(local, target)`，不额外 +1）
    ///
    /// 供 WAL 恢复路径 O(1) 对齐已见最大逻辑时间，替代逐次 tick 的 O(n) 循环
    /// （，report-002）。CAS 循环保证并发安全。
    pub fn advance_to(&self, target: u64) {
        loop {
            let current = self.counter.load(Ordering::SeqCst);
            if current >= target {
                break;
            }
            if self
                .counter
                .compare_exchange(current, target, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                break;
            }
        }
    }

    /// 合并外部时钟值（取 `max(local, other) + 1`）
    ///
    /// 使用 CAS 循环保证读-改-写的原子性，避免并发更新丢失。
    pub fn merge(&self, other: u64) {
        loop {
            let current = self.counter.load(Ordering::SeqCst);
            let new_val = current.max(other) + 1;
            if self
                .counter
                .compare_exchange(current, new_val, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                break;
            }
        }
    }
}

impl Default for LogicalClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::LogicalClock;

    #[test]
    fn new_starts_at_zero() {
        assert_eq!(LogicalClock::new().current(), 0);
    }

    #[test]
    fn tick_is_monotonic() {
        let clock = LogicalClock::new();
        assert_eq!(clock.tick(), 1);
        assert_eq!(clock.tick(), 2);
        assert_eq!(clock.tick(), 3);
    }

    #[test]
    fn merge_takes_max_plus_one() {
        let clock = LogicalClock::new();
        clock.merge(10);
        assert_eq!(clock.current(), 11);
        clock.merge(100);
        assert_eq!(clock.current(), 101);
        // 小于当前值时不回退：max(101, 1) + 1 = 102
        clock.merge(1);
        assert_eq!(clock.current(), 102);
    }

    #[test]
    fn clone_shares_counter() {
        let clock = LogicalClock::new();
        let clone = clock.clone();
        clone.tick();
        assert_eq!(clock.current(), 1);
    }

    #[test]
    fn advance_to_aligns_to_max_without_plus_one() {
        let clock = LogicalClock::new();
        // 空时钟推进到目标：终态 = target（与原 while-tick 循环终态一致，不 +1）
        clock.advance_to(42);
        assert_eq!(clock.current(), 42);
        // 目标小于当前值时不回退
        clock.advance_to(1);
        assert_eq!(clock.current(), 42);
        // 目标等于当前值时不变
        clock.advance_to(42);
        assert_eq!(clock.current(), 42);
        // 与 tick 混用：后续 tick 从当前值继续递增
        assert_eq!(clock.tick(), 43);
    }
}

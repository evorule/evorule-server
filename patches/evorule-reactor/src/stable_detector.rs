// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 稳定检测逻辑
//!
//! # 设计依据
//! 基于《02_反应式数据执行器》§3.2 和《04_树形结构》stable_detector.rs 定义：
//! - 队列为空 + 无待处理 I/O = 稳定
//! - 需已执行过至少一步（避免初始空状态误判）
//!
//! # 稳定条件
//! 1. `queue.is_empty()` — 所有指令已执行完毕
//! 2. `pending_io_count == 0` — 所有 I/O 请求已收到响应
//! 3. `steps > 0` — 已执行过至少一步（防止初始空队列误判）

/// 稳定检测器
///
/// 封装稳定判定逻辑，支持有状态（连续稳定计数）和无状态两种模式。
#[derive(Debug, Clone)]
pub struct StableDetector {
    /// 连续稳定计数（用于阈值判定，0 表示尚未稳定）
    stable_count: usize,

    /// 稳定阈值（连续 N 次稳定才判定为真正稳定）
    threshold: usize,
}

impl StableDetector {
    /// 创建新的稳定检测器，默认阈值 1
    pub fn new() -> Self {
        Self {
            stable_count: 0,
            threshold: 1,
        }
    }

    /// 设置稳定阈值（连续 N 次稳定才判定为真正稳定）
    pub fn with_threshold(threshold: usize) -> Self {
        Self {
            stable_count: 0,
            threshold: threshold.max(1),
        }
    }

    /// 检查当前状态是否满足稳定条件
    ///
    /// 无状态检查：仅判断队列与 I/O 状态，不考虑历史。
    pub fn is_stable(queue_len: usize, pending_io_count: usize) -> bool {
        queue_len == 0 && pending_io_count == 0
    }

    /// 有状态稳定检测：记录连续稳定次数
    ///
    /// 每次调用时，如果满足稳定条件则 `stable_count += 1`，否则重置为 0。
    /// 当 `stable_count >= threshold` 时返回 true。
    ///
    /// 注意：调用者应确保 `steps > 0` 后再调用此方法（避免初始空状态误判）。
    pub fn observe(&mut self, queue_len: usize, pending_io_count: usize) -> bool {
        if Self::is_stable(queue_len, pending_io_count) {
            self.stable_count = self.stable_count.saturating_add(1);
        } else {
            self.stable_count = 0;
        }
        self.stable_count >= self.threshold
    }

    /// 重置稳定计数器
    pub fn reset(&mut self) {
        self.stable_count = 0;
    }

    /// 返回当前连续稳定次数
    pub fn stable_count(&self) -> usize {
        self.stable_count
    }
}

impl Default for StableDetector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn test_is_stable_static() {
        // 队列空 + 无 I/O = 稳定
        assert!(StableDetector::is_stable(0, 0));

        // 队列非空 = 不稳定
        assert!(!StableDetector::is_stable(1, 0));

        // 有待处理 I/O = 不稳定
        assert!(!StableDetector::is_stable(0, 1));

        // 两者都有 = 不稳定
        assert!(!StableDetector::is_stable(2, 3));
    }

    #[test]
    fn test_observe_threshold_1() {
        let mut det = StableDetector::new(); // threshold=1

        // 第一次稳定 → 立即返回 true
        assert!(det.observe(0, 0));
        assert_eq!(det.stable_count(), 1);

        // 再次稳定
        assert!(det.observe(0, 0));
        assert_eq!(det.stable_count(), 2);

        // 变为不稳定 → 重置
        assert!(!det.observe(1, 0));
        assert_eq!(det.stable_count(), 0);

        // 再次稳定
        assert!(det.observe(0, 0));
        assert_eq!(det.stable_count(), 1);
    }

    #[test]
    fn test_observe_threshold_3() {
        let mut det = StableDetector::with_threshold(3);

        // 第 1 次稳定 → 未达阈值
        assert!(!det.observe(0, 0));
        assert_eq!(det.stable_count(), 1);

        // 第 2 次稳定 → 未达阈值
        assert!(!det.observe(0, 0));
        assert_eq!(det.stable_count(), 2);

        // 第 3 次稳定 → 达到阈值
        assert!(det.observe(0, 0));
        assert_eq!(det.stable_count(), 3);

        // 中间不稳定 → 重置
        assert!(!det.observe(0, 1));
        assert_eq!(det.stable_count(), 0);

        // 重新开始计数
        assert!(!det.observe(0, 0));
        assert_eq!(det.stable_count(), 1);
    }

    #[test]
    fn test_reset() {
        let mut det = StableDetector::with_threshold(2);
        det.observe(0, 0);
        det.observe(0, 0);
        assert_eq!(det.stable_count(), 2);

        det.reset();
        assert_eq!(det.stable_count(), 0);
    }

    #[test]
    fn test_threshold_minimum_1() {
        // threshold=0 应被规范化为 1
        let det = StableDetector::with_threshold(0);
        assert_eq!(det.threshold, 1);
    }
}

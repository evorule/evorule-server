// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 会话级熔断器（SG-3，family-②「白名单外锚点连发」）
//!
//! # 语义
//!
//! IoSubscriber 对**静态路由层 deny**（`route=deny` / `route=ask-fallback`，
//! 即 SG-2 白名单外锚点拒绝）做滑动窗口计数；窗口内计数达阈值 → **跳闸**：
//!
//! 1. 产生熔断信号指令 `set meta_security.circuit_breaker = {family, threshold,
//!    window_secs, count}`，由订阅者经 command 通道提交进会话——规则层
//!    `00_constraint_enforce_anchor.json` 第 3 条 enforce 拒链（Halted + 系统
//!    独占 Violation Fact）＝跳闸证据**确定性入链**（TCB 求值，同输入同输出）；
//! 2. 闸位进入跳闸态：后续 egress I/O 全量拒绝（error 串带 `circuit=tripped`
//!    段，与 default deny 可区分），经窗口时长后**自动复位**重新计数。
//!
//! # 状态纪律
//!
//! - 进程内存（hit_stats 同纪律：审计链 WAL 是全量权威源，本状态只承载
//!   运行时判定，不承载审计责任）；
//! - 每 [`crate::io_subscriber::IoSubscriber`] 实例独立持有（实例按会话级
//!   装配点构造，天然会话作用域）；
//! - 阈值/窗口由应用层经 [`CircuitConfig`] 注入（env 解析归应用层接缝，
//!   本 crate 保持纯机制）；阈值 0 或未装配 = 熔断面关闭。
//!
//! # 边界
//!
//! - 计数口径仅静态层 deny（`route` 命中）；动态权限表 deny 属 PermissionGate
//!   级留痕观察（威胁建模 §十一 D-3 拍板：其余违规不升熔断级）；
//! - 跳过谓词命中的 IoRequest（外部执行者应答）不经本闸（v1 边界，随误伤
//!   数据细化）。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use evorule_tcb::JsonValue;

/// 熔断信号指令的 attr 键（规则层 `00_constraint_enforce_anchor.json` 第 3 条
/// enforce 的匹配键；两端正本：本常量 × 规则资产，演化须同步）
pub const CIRCUIT_BREAKER_ATTR: &str = "meta_security.circuit_breaker";

/// 单会话熔断配置
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CircuitConfig {
    /// 滑动窗口内静态层 deny 计数阈值（达到即跳闸）
    pub threshold: u32,
    /// 计数窗口；跳闸后经此时长自动复位（窗口与跳闸时长同一语义）
    pub window: Duration,
}

/// 会话级熔断状态（每 IoSubscriber 实例独立持有）
#[derive(Debug)]
pub struct CircuitState {
    config: CircuitConfig,
    /// 滑动窗口内的 deny 时刻队列
    deny_times: VecDeque<Instant>,
    /// 跳闸截止时刻（`None` = 未跳闸；到期由 [`Self::is_tripped`] 自动复位）
    tripped_until: Option<Instant>,
}

impl CircuitState {
    /// 创建熔断状态
    pub fn new(config: CircuitConfig) -> Self {
        Self {
            config,
            deny_times: VecDeque::new(),
            tripped_until: None,
        }
    }

    /// 是否处于跳闸态（跳闸到期自动复位：清闸位、清计数，重新起窗）
    pub fn is_tripped(&mut self, now: Instant) -> bool {
        match self.tripped_until {
            Some(until) if now < until => true,
            Some(_) => {
                self.tripped_until = None;
                self.deny_times.clear();
                false
            }
            None => false,
        }
    }

    /// 记录一次静态层 deny。窗口内计数达阈值 → 跳闸并返回熔断信号 value
    /// （供调用方构造 `set meta_security.circuit_breaker` 指令提交会话）；
    /// 未达阈值返回 `None`。跳闸后计数清零，复位后重新起窗。
    pub fn record_deny(&mut self, now: Instant) -> Option<JsonValue> {
        if self.config.threshold == 0 {
            return None; // 熔断面关闭（阈值 0 = 禁用）
        }
        let cutoff = now.checked_sub(self.config.window);
        if let Some(cutoff) = cutoff {
            while let Some(&t) = self.deny_times.front() {
                if t < cutoff {
                    self.deny_times.pop_front();
                } else {
                    break;
                }
            }
        }
        self.deny_times.push_back(now);
        if (self.deny_times.len() as u32) < self.config.threshold {
            return None;
        }
        let count = self.deny_times.len() as u64;
        self.tripped_until = Some(now + self.config.window);
        self.deny_times.clear();
        Some(Self::signal_value(
            self.config.threshold as u64,
            self.config.window.as_secs(),
            count,
        ))
    }

    /// 熔断信号 value（规则层 enforce 拒链后的 Violation 归因即携带本对象）
    fn signal_value(threshold: u64, window_secs: u64, count: u64) -> JsonValue {
        JsonValue::object_from_pairs(&[
            ("family", JsonValue::from("off_allowlist_anchors")),
            ("threshold", JsonValue::from(threshold as i64)),
            ("window_secs", JsonValue::from(window_secs as i64)),
            ("count", JsonValue::from(count as i64)),
        ])
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn config(threshold: u32, window_secs: u64) -> CircuitConfig {
        CircuitConfig {
            threshold,
            window: Duration::from_secs(window_secs),
        }
    }

    #[test]
    fn below_threshold_returns_none() {
        let mut s = CircuitState::new(config(3, 60));
        let t0 = Instant::now();
        assert!(s.record_deny(t0).is_none());
        assert!(s.record_deny(t0 + Duration::from_secs(1)).is_none());
        assert!(!s.is_tripped(t0 + Duration::from_secs(2)));
    }

    #[test]
    fn threshold_trip_returns_signal_with_count() {
        let mut s = CircuitState::new(config(3, 60));
        let t0 = Instant::now();
        assert!(s.record_deny(t0).is_none());
        assert!(s.record_deny(t0 + Duration::from_secs(1)).is_none());
        let signal = s
            .record_deny(t0 + Duration::from_secs(2))
            .expect("第三次 deny 达阈值必须跳闸");
        // 信号 value 形状（与 00_constraint_enforce_anchor.json#2 匹配键正交，
        // 内容归因承载）
        let JsonValue::Object(ref map) = signal else {
            panic!("信号必须是对象");
        };
        assert_eq!(map.get("family").and_then(|v| v.as_str()), Some("off_allowlist_anchors"));
        assert_eq!(map.get("count").and_then(|v| v.as_i64()), Some(3));
        assert_eq!(map.get("threshold").and_then(|v| v.as_i64()), Some(3));
        assert_eq!(map.get("window_secs").and_then(|v| v.as_i64()), Some(60));
        assert!(s.is_tripped(t0 + Duration::from_secs(3)), "跳闸态内必须拒绝");
    }

    #[test]
    fn window_expiry_drops_old_denies() {
        let mut s = CircuitState::new(config(2, 60));
        let t0 = Instant::now();
        assert!(s.record_deny(t0).is_none());
        // 窗口外的一次 deny 不与 t0 累计
        assert!(s.record_deny(t0 + Duration::from_secs(61)).is_none());
        assert!(!s.is_tripped(t0 + Duration::from_secs(62)));
    }

    #[test]
    fn trip_auto_resets_after_window() {
        let mut s = CircuitState::new(config(1, 60));
        let t0 = Instant::now();
        assert!(s.record_deny(t0).is_some(), "阈值 1 首deny即跳闸");
        assert!(s.is_tripped(t0 + Duration::from_secs(1)));
        assert!(
            !s.is_tripped(t0 + Duration::from_secs(61)),
            "窗口过期必须自动复位"
        );
        // 复位后重新起窗：一次 deny 再次跳闸
        assert!(s.record_deny(t0 + Duration::from_secs(62)).is_some());
    }

    #[test]
    fn zero_threshold_disables_circuit() {
        let mut s = CircuitState::new(config(0, 60));
        let t0 = Instant::now();
        for i in 0..10 {
            assert!(s.record_deny(t0 + Duration::from_secs(i)).is_none());
        }
        assert!(!s.is_tripped(t0));
    }
}

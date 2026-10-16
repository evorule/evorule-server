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
//!
//! # 部署实测要点（SG-3 端到端实测方法论，2026-10-14 实测沉淀）
//!
//! 熔断面端到端实测需要真实 IoRequest 流经静态层 deny 闸，以下为实测
//! 路径与坑位（部署资产均为 gitignore 管辖，改测试态无 git 污染）：
//!
//! 1. **IoRequest 触发指令必须用 `call_service`，不能用 `http_get`**：
//!    `http_get`/`query_db`/`save_memory` 已非内置指令类型（宪法
//!    `server_eval.json` 头注），裸 `{"type":"io"}` 或 `{"type":"http_get"}`
//!    提交只会得到 `Instruction ignored by TCB`（宪法无规则匹配）。
//!    正确形态：`{"type":"call_service","params":{"service_name":"probe",
//!    "args":{}}}`（或带 `tool_name`），经宪法 branch → `io_request`
//!    元指令产生 `io_type=call_service` 的 IoRequest；
//! 2. **路由资产须临时切测试态**：部署资产 `rules_dir/
//!    00_constraint_route_policy.json` 缺省 `rules:[]` 零 deny（熔断永远
//!    不触发）——实测前加入形如 `{"caller_role":"llm","io_type":
//!    "call_service","target":"*","decision":"deny"}` 的规则，会话经
//!    `POST /api/sessions` body `{"caller_role":"llm"}` 声明角色；
//! 3. **改资产后必须重启**：`rules_dir` 合并装载发生在**启动/重载时**，
//!    非每会话读盘（路由策略另有 OnceLock 进程缓存，同样不热更）；
//!    启动须带本仓根目录为工作目录（宪法相对路径
//!    `./resources/server_eval.json`）；
//! 4. **连发与观察**：连发 call_service 至阈值（缺省 5），deny 不中断会话
//!    （version 照常推进，IoResponse error 回写）；取证经
//!    `GET /api/sessions/{id}/replay`——跳闸应有 `Violation(rule_index=18,
//!    cause=10000 段)` + 信号 value（family/threshold/window_secs/count）+
//!    `circuit=tripped` 全拒 IoResponse；
//! 5. **复位验证**：缺省窗口 300s 太久，实测重启时以
//!    `EVORULE_CIRCUIT_WINDOW_SECS=20` 缩短（部署 env 接缝，见 server 侧
//!    route_policy_loader）；窗口过期后新请求应恢复普通 deny（error 串无
//!    `tripped` 段）；
//! 6. **实测后必须还原**：路由资产还原净形态 + 不带 env 净态重启，并以
//!    A3 探针（`set meta_security.circuit_breaker` 信号 → 期待 `version=0`
//!    拒链）确认 enforce 三锚仍在装载位。

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

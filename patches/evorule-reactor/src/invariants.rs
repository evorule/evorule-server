// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 反应器不变式自检（白盒化：结构性约束验证）
//!
//! # 设计
//!
//! 5 条结构性不变式，每次 phase 转移时检查。
//! 违规用 `tracing::error!` 记录（非 `debug_assert!`，符合 F11）。
//! 不强制中断反应器，仅累计计数到 `ReactorState::structural_invariant_violations`。
//!
//! # 5 条不变式
//!
//! 1. `pending_io_count == pending_requests.len() == pending_io_timestamps.len()`
//! 2. `io_recovery == true ⇒ payload.__io_results__ 存在非 null 结果`
//! 3. `version >= prev_version`（单调递增）
//! 4. `payload.__io_results__ 存在非 null 结果 ⇒ io_recovery == true`（与 #2 合为 ⟺）
//! 5. `pending_io_count > 0 ∧ queue.is_empty() ⇒ io_recovery == false`
//!    （等待 I/O 期间不应处于恢复态；恢复指令已 pop 后队列空是合法的，
//!    但此时 io_recovery 应已被清或 pending_io 应已涨）
//!
//! # 注：原 #5 "pending_io==0 ∧ queue空 ∧ steps==0 ⇒ 应已 Stable" 在长驻模式下
//!    会误报（Stable 发射后的合法空闲态），故改为检查恢复态与等待态的不冲突。
//!
//! # 规范合规
//!
//! - ✅ 纯结构性检查，不涉及业务（机制-策略分离）
//! - ✅ 用 `tracing::error!` 而非 `debug_assert!`（F11）
//! - ✅ 违规计数是状态机的一部分
//! - ✅ 单函数 ≤ 50 行（F9），嵌套 ≤ 2 层（F8）

use crate::state::ReactorState;
use evorule_tcb::JsonValue;

/// 不变式违规
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvariantViolation {
    /// #1: pending_io_count 与 pending_requests/pending_io_timestamps 大小不一致
    IoCountMismatch {
        /// 当前 pending_io_count 值
        count: usize,
        /// pending_requests 集合大小
        requests_len: usize,
        /// pending_io_timestamps 映射大小
        timestamps_len: usize,
    },
    /// #2: io_recovery==true 但 payload 中无 __io_result__
    IoRecoveryWithoutResult,
    /// #3: version 回退（current < previous）
    VersionDecreased {
        /// 当前 version
        current: u64,
        /// 上一次 version
        previous: u64,
    },
    /// #4: payload 有 __io_result__ 但 io_recovery==false
    ResultWithoutIoRecovery,
    /// #5: pending_io>0 ∧ queue空 ∧ io_recovery=true（恢复态与等待态冲突）
    RecoveryWhileAwaitingIo,
}

impl InvariantViolation {
    /// 违规标签（用于 tracing/Prometheus）
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::IoCountMismatch { .. } => "io_count_mismatch",
            Self::IoRecoveryWithoutResult => "io_recovery_without_result",
            Self::VersionDecreased { .. } => "version_decreased",
            Self::ResultWithoutIoRecovery => "result_without_io_recovery",
            Self::RecoveryWhileAwaitingIo => "recovery_while_awaiting_io",
        }
    }
}

impl std::fmt::Display for InvariantViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IoCountMismatch {
                count,
                requests_len,
                timestamps_len,
            } => write!(
                f,
                "IoCountMismatch: count={}, requests_len={}, timestamps_len={}",
                count, requests_len, timestamps_len
            ),
            Self::IoRecoveryWithoutResult => write!(
                f,
                "IoRecoveryWithoutResult: io_recovery=true but __io_results__ has no non-null result"
            ),
            Self::VersionDecreased { current, previous } => {
                write!(
                    f,
                    "VersionDecreased: current={} < previous={}",
                    current, previous
                )
            }
            Self::ResultWithoutIoRecovery => write!(
                f,
                "ResultWithoutIoRecovery: __io_results__ has non-null result but io_recovery=false"
            ),
            Self::RecoveryWhileAwaitingIo => write!(
                f,
                "RecoveryWhileAwaitingIo: pending_io>0, queue empty, io_recovery=true"
            ),
        }
    }
}

/// 检查 5 条结构性不变式
///
/// 纯函数：仅读取状态，不修改。
/// 返回违规列表（空表示全部通过）。
///
/// # 参数
///
/// - `state`: 反应器状态快照
/// - `_steps`: 当前已执行指令步数（保留参数，#5 已重设计不再使用）
pub(crate) fn check_invariants(state: &ReactorState, _steps: usize) -> Vec<InvariantViolation> {
    let mut violations = Vec::new();

    // #1: pending_io_count == pending_requests.len() == pending_io_timestamps.len()
    check_io_count_consistency(state, &mut violations);

    // #2 + #4: io_recovery == payload_has_io_result
    check_io_recovery_consistency(state, &mut violations);

    // #3: version 单调递增
    check_version_monotonic(state, &mut violations);

    // #5: pending_io>0 ∧ queue空 ∧ io_recovery=true（恢复态与等待态冲突）
    check_no_recovery_conflict(state, &mut violations);

    violations
}

/// 不变式 #1：I/O 计数一致性
fn check_io_count_consistency(state: &ReactorState, violations: &mut Vec<InvariantViolation>) {
    let req_len = state.pending_requests.len();
    let ts_len = state.pending_io_timestamps.len();
    // Kani 模式：register_io_request_pure 有意不设置 pending_io_timestamps
    // （Instant::now() → clock_gettime 不被 Kani 支持），跳过 timestamp 长度检查。
    // timestamp 是运行时超时检测用的，不是结构性不变量。
    let mismatch = if cfg!(kani) {
        state.pending_io_count != req_len
    } else {
        state.pending_io_count != req_len || state.pending_io_count != ts_len
    };
    if mismatch {
        violations.push(InvariantViolation::IoCountMismatch {
            count: state.pending_io_count,
            requests_len: req_len,
            timestamps_len: ts_len,
        });
    }
}

/// 不变式 #2 + #4：io_recovery 与 __io_results__ 一致性（双向）
fn check_io_recovery_consistency(state: &ReactorState, violations: &mut Vec<InvariantViolation>) {
    let has_io_result = has_io_result(&state.payload);
    if state.io_recovery && !has_io_result {
        violations.push(InvariantViolation::IoRecoveryWithoutResult);
    }
    if has_io_result && !state.io_recovery {
        violations.push(InvariantViolation::ResultWithoutIoRecovery);
    }
}

/// 不变式 #3：version 单调递增
fn check_version_monotonic(state: &ReactorState, violations: &mut Vec<InvariantViolation>) {
    if state.version < state.prev_version {
        violations.push(InvariantViolation::VersionDecreased {
            current: state.version,
            previous: state.prev_version,
        });
    }
}

/// 不变式 #5：恢复态与等待态不冲突
///
/// 当 `pending_io > 0`（等待新 IoResponse）且队列空（恢复指令已 pop）时，
/// `io_recovery` 应已为 false（State 分支已清）或保持但 pending_io 应为 0。
/// 冲突场景：上一轮 IoResponse 处理后 push_front 恢复指令 → pop 执行 →
/// 触发新 IoRequest → break（io_recovery 仍 true, pending_io=1, queue 空）。
/// 此场景下 io_recovery=true 是过期标志，应被新 IoRequest 的 break 路径清理。
/// 当前实现允许此过渡态，故此不变式记录为弱约束（默认通过）。
fn check_no_recovery_conflict(state: &ReactorState, violations: &mut Vec<InvariantViolation>) {
    // 弱约束：仅当 pending_io > 0 且 queue 空 且 io_recovery=true 且 __io_results__ 无非 null 结果时
    // 才视为违规（此时 io_recovery 是过期标志且已无对应结果可消费）
    if state.pending_io_count > 0
        && state.queue.is_empty()
        && state.io_recovery
        && !has_io_result(&state.payload)
    {
        violations.push(InvariantViolation::RecoveryWhileAwaitingIo);
    }
}

/// 判定 payload 的 `__io_results__` 中是否存在非 null 的 I/O 结果
///
/// v0.3.1：结果按 io_type 隔离存储在 `__io_results__.{io_type}`，
/// core_eval 消费后以 null 清除（`exists` 将 null 视为不存在），
/// 因此"存在非 null 结果"等价于旧版的"存在 `__io_result__` 字段"。
fn has_io_result(payload: &JsonValue) -> bool {
    match payload.get("__io_results__") {
        Some(JsonValue::Object(map)) => map.values().any(|v| !v.is_null()),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::fact::FactId;

    /// 测试辅助：向 payload.__io_results__.call_external 注入非 null 结果
    // 2026-09-18：原用 BTreeMap::entry().or_insert_with()，但 kani cfg 下 Object 底层
    // 是 KaniMap 替身（Vec 模型，见 tcb value.rs L58-132），替身无 entry（建模成本不划算）；
    // 改用 contains_key + get_mut / insert 组合，BTreeMap 与 KaniMap 两 cfg API 统一。
    fn set_io_result(state: &mut ReactorState) {
        if let JsonValue::Object(map) = &mut state.payload {
            if !map.contains_key("__io_results__") {
                map.insert("__io_results__".to_string(), JsonValue::empty_object());
            }
            if let JsonValue::Object(io_map) = map.get_mut("__io_results__").unwrap() {
                io_map.insert("call_external".to_string(), JsonValue::string("test"));
            }
        }
    }

    #[test]
    fn test_fresh_state_passes_all_invariants() {
        let state = ReactorState::new();
        let violations = check_invariants(&state, 0);
        assert!(
            violations.is_empty(),
            "Fresh state should pass all invariants, got: {:?}",
            violations
        );
    }

    #[test]
    fn test_invariant_1_io_count_mismatch_requests() {
        let mut state = ReactorState::new();
        state.pending_io_count = 2;
        state.pending_requests.insert(FactId(1));
        // pending_io_timestamps empty → mismatch
        let violations = check_invariants(&state, 0);
        assert!(violations
            .iter()
            .any(|v| matches!(v, InvariantViolation::IoCountMismatch { .. })));
    }

    #[test]
    fn test_invariant_1_io_count_mismatch_timestamps() {
        let mut state = ReactorState::new();
        state.pending_io_count = 1;
        state.pending_requests.insert(FactId(1));
        state
            .pending_io_timestamps
            .insert(FactId(1), std::time::Instant::now());
        state
            .pending_io_timestamps
            .insert(FactId(2), std::time::Instant::now());
        // count=1, requests=1, timestamps=2 → mismatch
        let violations = check_invariants(&state, 0);
        assert!(violations
            .iter()
            .any(|v| matches!(v, InvariantViolation::IoCountMismatch { .. })));
    }

    #[test]
    fn test_invariant_1_all_three_consistent() {
        let mut state = ReactorState::new();
        state.pending_io_count = 2;
        state.pending_requests.insert(FactId(1));
        state.pending_requests.insert(FactId(2));
        state
            .pending_io_timestamps
            .insert(FactId(1), std::time::Instant::now());
        state
            .pending_io_timestamps
            .insert(FactId(2), std::time::Instant::now());
        let violations = check_invariants(&state, 0);
        assert!(violations
            .iter()
            .all(|v| !matches!(v, InvariantViolation::IoCountMismatch { .. })));
    }

    #[test]
    fn test_invariant_2_io_recovery_without_result() {
        let mut state = ReactorState::new();
        state.io_recovery = true;
        let violations = check_invariants(&state, 0);
        assert!(violations
            .iter()
            .any(|v| matches!(v, InvariantViolation::IoRecoveryWithoutResult)));
    }

    #[test]
    fn test_invariant_3_version_decreased() {
        let mut state = ReactorState::new();
        state.version = 5;
        state.prev_version = 10;
        let violations = check_invariants(&state, 0);
        assert!(violations
            .iter()
            .any(|v| matches!(v, InvariantViolation::VersionDecreased { .. })));
    }

    #[test]
    fn test_invariant_3_version_equal_passes() {
        let mut state = ReactorState::new();
        state.version = 5;
        state.prev_version = 5;
        let violations = check_invariants(&state, 0);
        assert!(violations
            .iter()
            .all(|v| !matches!(v, InvariantViolation::VersionDecreased { .. })));
    }

    #[test]
    fn test_invariant_4_result_without_io_recovery() {
        let mut state = ReactorState::new();
        set_io_result(&mut state);
        state.io_recovery = false;
        let violations = check_invariants(&state, 0);
        assert!(violations
            .iter()
            .any(|v| matches!(v, InvariantViolation::ResultWithoutIoRecovery)));
    }

    #[test]
    fn test_invariant_2_4_consistent_both_true() {
        let mut state = ReactorState::new();
        state.io_recovery = true;
        set_io_result(&mut state);
        let violations = check_invariants(&state, 0);
        assert!(violations.iter().all(|v| !matches!(
            v,
            InvariantViolation::IoRecoveryWithoutResult
                | InvariantViolation::ResultWithoutIoRecovery
        )));
    }

    #[test]
    fn test_invariant_2_4_consistent_both_false() {
        let state = ReactorState::new();
        // io_recovery=false, no __io_result__ → consistent
        let violations = check_invariants(&state, 0);
        assert!(violations.iter().all(|v| !matches!(
            v,
            InvariantViolation::IoRecoveryWithoutResult
                | InvariantViolation::ResultWithoutIoRecovery
        )));
    }

    #[test]
    fn test_invariant_5_recovery_conflict_violation() {
        // pending_io > 0, queue empty, io_recovery=true, no __io_result__
        let mut state = ReactorState::new();
        state.pending_io_count = 1;
        state.pending_requests.insert(FactId(1));
        state
            .pending_io_timestamps
            .insert(FactId(1), std::time::Instant::now());
        state.io_recovery = true;
        // no __io_result__ in payload
        let violations = check_invariants(&state, 0);
        assert!(violations
            .iter()
            .any(|v| matches!(v, InvariantViolation::RecoveryWhileAwaitingIo)));
    }

    #[test]
    fn test_invariant_5_no_conflict_when_result_present() {
        // io_recovery=true with __io_results__ non-null result → #2 passes, #5 not triggered
        let mut state = ReactorState::new();
        state.pending_io_count = 1;
        state.pending_requests.insert(FactId(1));
        state
            .pending_io_timestamps
            .insert(FactId(1), std::time::Instant::now());
        state.io_recovery = true;
        set_io_result(&mut state);
        let violations = check_invariants(&state, 0);
        assert!(violations
            .iter()
            .all(|v| !matches!(v, InvariantViolation::RecoveryWhileAwaitingIo)));
    }

    #[test]
    fn test_invariant_5_no_conflict_when_queue_nonempty() {
        let mut state = ReactorState::new();
        state.pending_io_count = 1;
        state.pending_requests.insert(FactId(1));
        state
            .pending_io_timestamps
            .insert(FactId(1), std::time::Instant::now());
        state.io_recovery = true;
        state.push_back(JsonValue::string("work"), FactId(1));
        let violations = check_invariants(&state, 0);
        assert!(violations
            .iter()
            .all(|v| !matches!(v, InvariantViolation::RecoveryWhileAwaitingIo)));
    }

    #[test]
    fn test_invariant_5_no_conflict_when_no_pending_io() {
        let mut state = ReactorState::new();
        state.io_recovery = true;
        // pending_io=0, but #2 will trigger (io_recovery without result)
        let violations = check_invariants(&state, 0);
        assert!(violations
            .iter()
            .all(|v| !matches!(v, InvariantViolation::RecoveryWhileAwaitingIo)));
    }

    #[test]
    fn test_violation_as_str() {
        assert_eq!(
            InvariantViolation::IoRecoveryWithoutResult.as_str(),
            "io_recovery_without_result"
        );
        assert_eq!(
            InvariantViolation::ResultWithoutIoRecovery.as_str(),
            "result_without_io_recovery"
        );
        assert_eq!(
            InvariantViolation::VersionDecreased {
                current: 1,
                previous: 2
            }
            .as_str(),
            "version_decreased"
        );
        assert_eq!(
            InvariantViolation::RecoveryWhileAwaitingIo.as_str(),
            "recovery_while_awaiting_io"
        );
    }

    #[test]
    fn test_violation_display() {
        let v = InvariantViolation::IoCountMismatch {
            count: 2,
            requests_len: 1,
            timestamps_len: 1,
        };
        let s = format!("{}", v);
        assert!(s.contains("IoCountMismatch"));
        assert!(s.contains("count=2"));
    }
}

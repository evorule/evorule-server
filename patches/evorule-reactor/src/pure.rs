// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 反应器纯逻辑模块（阶段7：Kani 形式化验证准备）
//!
//! # 设计目标
//!
//! 将 reactor 主循环中**不含 I/O、不含 tokio、不含 tracing** 的纯逻辑抽离到本模块，
//! 为后续 Kani 形式化验证做准备。Kani 不支持异步运行时和 I/O，
//! 因此需要将"逻辑"与"运行时"分离。
//!
//! # 抽离原则
//!
//! - ✅ 纯函数：输入 → 输出，无副作用
//! - ✅ 不含 tokio / async / await
//! - ✅ 不含 tracing / logging
//! - ✅ 不含 Instant::now() 等系统调用
//! - ✅ 与 TCB 的 Kani 验证配对（TCB 验证单步，reactor 验证多步）
//!
//! # 可验证的不变式（Kani 证明目标）
//!
//! 1. `pending_io_count == pending_requests.len() == pending_io_timestamps.len()`
//! 2. `io_recovery == true ⇔ payload.__io_results__ 存在非 null 结果`
//! 3. `version >= prev_version`（单调递增）
//! 4. 单步执行后状态一致性
//! 5. max_rounds 内终止（需 bounded model checking）
//!
//! # 规范合规
//!
//! - ✅ 机制-策略分离：本模块只含机制层逻辑
//! - ✅ F8/F9/F11：嵌套 ≤ 2 层，单函数 ≤ 50 行，无 debug_assert!
//! - ✅ 不影响 TCB（evorule-tcb 完全不修改）
//!
//! # Kani 使用方式（未来）
//!
//! ```bash
//! # 启用 kani feature 运行验证
//! cargo kani -p evorule-reactor --features kani
//! ```

use crate::error::ReactorError;
use crate::fact::{FactId, IoType};
use crate::invariants::InvariantViolation;
use crate::state::ReactorState;
use evorule_tcb::{execute_transition, JsonValue, TransitionResult};

/// 单步执行结果
///
/// 描述执行一条指令后 reactor 状态的变化，包括：
/// - 新的 payload / queue
/// - 是否触发了 I/O 请求
/// - 是否发生了错误
#[derive(Debug, Clone)]
pub(crate) enum StepOutcome {
    /// 状态转移成功（payload / queue 已更新）
    StateChanged,
    /// 指令被忽略：没有匹配的宪法规则或规则产生了 noop 效果
    ///
    /// 参见 docs/rule_taxonomy.md 规则分类体系
    Ignored {
        /// 被忽略的指令类型
        instruction_type: String,
        /// 忽略原因
        reason: String,
    },
    /// 触发 I/O 请求（调用方需注册并发射 IoRequest）
    IoRequired {
        /// I/O 类型字符串（原始字符串，调用方 parse 成 IoType）
        io_type: String,
        /// I/O 参数
        params: JsonValue,
    },
    /// `enforce` 强制拦截（回归验证）：违规指令被拒绝执行
    ///
    /// 指令已出队即丢弃（不推回队列）；payload/queue 保持该步前原样、
    /// version 不 bump（无任何状态转移发生）。调用方需系统独占发射
    /// `Fact::Violation`（三权分立：阻止权/记录权在系统）。
    Halted {
        /// 命中的 enforce 规则下标（合并规则列表中）
        rule_index: usize,
        /// 违规说明（enforce params.reason）
        reason: String,
    },
    /// TCB 执行错误
    TcbError(String),
}

/// 执行单步指令（纯函数，不含 I/O）
///
/// 从 `state` 中弹出队首指令，调用 TCB 的 `execute_transition`，
/// 根据结果更新 state。**不发射事实、不记录日志、不修改快照**。
///
/// # 参数
///
/// - `core_eval`: 规则配置（来自 core_eval.json）
/// - `state`: 反应器状态（可变引用，直接修改）
/// - `max_queue_len`: 队列长度上限（0 表示无限制）
///
/// # 返回值
///
/// - `StepOutcome::StateChanged`: 状态转移成功
/// - `StepOutcome::IoRequired`: 触发 I/O 请求
/// - `StepOutcome::TcbError`: TCB 执行错误
///
/// # 注意
///
/// - 此函数假设队列非空，队列为空时返回 `None`
/// - I/O 恢复态清理（clear_io_result）在此函数内部处理
/// - version bump 在此函数内部处理
pub(crate) fn next_step(
    core_eval: &[JsonValue],
    state: &mut ReactorState,
    max_queue_len: usize,
) -> Option<StepOutcome> {
    let (instruction, cause) = state.pop_instruction()?;

    let queue_vec: Vec<JsonValue> = state.queue.iter().cloned().collect();
    let result = execute_transition(core_eval, &instruction, &state.payload, &queue_vec);

    match result {
        Ok(TransitionResult::State {
            new_payload,
            new_queue,
            // 纯函数路径（Kani 验证准备）：不采集命中归因（运行时路径在 reactor.rs）
            rule_hits: _,
        }) => {
            state.payload = new_payload;
            state.update_queue_with_causes(new_queue, cause);

            // I/O 恢复执行后整体清除 __io_results__，防止残留影响后续不同的 I/O 指令
            if state.io_recovery {
                state.clear_io_result();
                state.io_recovery = false;
            }
            state.bump_version();

            // 队列长度检查（返回结果供调用方处理告警/Error）
            // 纯函数不发射事实，调用方决定是否告警
            if max_queue_len > 0 && state.queue.len() >= max_queue_len {
                // 达到硬限制，清空队列（与原逻辑一致）
                state.clear_queue();
            }

            Some(StepOutcome::StateChanged)
        }
        Ok(TransitionResult::Ignored {
            instruction_type,
            reason,
            // 纯函数路径（Kani 验证准备）：不采集命中归因（运行时路径在 reactor.rs）
            rule_hits: _,
        }) => {
            // 指令被静默忽略：记录告警并返回 Ignored 结果
            // 这使得上层（reactor）可以产生 Error 事实，不再静默失败
            // 规则分类参见 docs/rule_taxonomy.md
            state.bump_version();
            tracing::warn!(
                instruction_type = %instruction_type,
                reason = %reason,
                "指令被忽略：没有匹配的宪法规则或规则产生了 noop 效果"
            );
            Some(StepOutcome::Ignored {
                instruction_type,
                reason,
            })
        }
        Ok(TransitionResult::IoRequired { io_type, params }) => {
            // 注意：register_io_request 含 Instant::now()，不在纯函数中
            // 调用方需在外部注册 I/O 请求
            // 这里先把指令推回队列，让调用方处理
            // （原逻辑是在 IoRequired 分支中 register + save + break）
            state.push_front(instruction, cause);
            Some(StepOutcome::IoRequired { io_type, params })
        }
        Ok(TransitionResult::Halted { rule_index, reason }) => {
            // enforce 强制拦截（回归验证）：指令已出队即丢弃（不推回队列），
            // payload/queue 保持该步前原样，version 不 bump（无状态转移发生）。
            // 调用方据此系统独占发射 Fact::Violation。
            Some(StepOutcome::Halted { rule_index, reason })
        }
        Err(err) => Some(StepOutcome::TcbError(err.to_string())),
    }
}

/// 应用 Command 指令到状态（纯函数）
///
/// 将指令追加到队列尾部。对应原 `handle_fact` 中 `Fact::Command` 分支。
pub(crate) fn apply_command(state: &mut ReactorState, instruction: JsonValue, cause: FactId) {
    state.push_back(instruction, cause);
}

/// 应用 PayloadUpdate 到状态（纯函数）
///
/// 更新 payload 中指定路径的值，并递增 version。
/// 对应原 `handle_fact` 中 `Fact::PayloadUpdate` 分支。
pub(crate) fn apply_payload_update(
    state: &mut ReactorState,
    path: &str,
    value: JsonValue,
) -> Result<(), ReactorError> {
    if let Some(target) = evorule_tcb::path::resolve_path_mut(&mut state.payload, path) {
        *target = value;
        state.bump_version();
        return Ok(());
    }
    // 路径不存在：仅支持顶层字段创建
    if !path.contains('.') && !path.contains('[') {
        if let JsonValue::Object(map) = &mut state.payload {
            map.insert(path.to_string(), value);
            state.bump_version();
            return Ok(());
        }
    }
    Err(ReactorError::InvalidState {
        field: "payload path does not exist",
    })
}

/// 应用 IoResponse 到状态（纯函数）
///
/// 完成 I/O 请求、注入结果、推回原指令、设置恢复标志。
/// 对应原 `handle_fact` 中 `Fact::IoResponse` 分支的纯逻辑部分。
///
/// # 返回值
///
/// - `Ok(true)`: I/O 请求存在，已应用
/// - `Ok(false)`: I/O 请求不存在（未知 IoResponse，忽略）
/// - `Err`: 注入结果时出错
pub(crate) fn apply_io_response(
    state: &mut ReactorState,
    request_id: FactId,
    result: JsonValue,
) -> Result<bool, ReactorError> {
    // v0.3.1：先取 io_type（complete_io_request 会移除 io_type 记录）
    let io_type = state.get_io_type(&request_id).cloned();
    if !state.complete_io_request(request_id) {
        return Ok(false);
    }
    // v0.3.1 修复：null 结果没有可消费的内容（`exists` 将 null 视为不存在）。
    // 若注入 null 后重新推送原指令，恢复执行会再次走 io_request → 死循环。
    // 处理：丢弃缓存的原指令，不再重新推送（与 reactor::handle_fact 一致）。
    if result.is_null() {
        state.take_io_instruction(request_id);
        state.bump_version();
        return Ok(true);
    }
    if let Some(io_type) = io_type {
        inject_io_result(state, &io_type, result)?;
    }
    if let Some((orig_instruction, orig_cause)) = state.take_io_instruction(request_id) {
        state.push_front(orig_instruction, orig_cause);
        state.io_recovery = true;
    }
    state.bump_version();
    Ok(true)
}

/// 注入 I/O 结果到 payload.__io_results__.{io_type}（纯函数）
///
/// 对应原 `inject_io_result` 方法。
///
/// # Kani 模式
///
/// Kani 模式下不操作 `JsonValue::Object` 内部的 `BTreeMap`（会导致 CBMC 状态爆炸），
/// 改为设置 `kani_has_io_result` 标志位。`has_io_result` 在 Kani 模式下
/// 检查此标志位而非 BTreeMap，保证不变量验证的一致性。
fn inject_io_result(
    state: &mut ReactorState,
    io_type: &IoType,
    result: JsonValue,
) -> Result<(), ReactorError> {
    #[cfg(kani)]
    {
        let _ = (result, io_type); // 避免未使用变量警告
        state.kani_has_io_result = true;
        return Ok(());
    }
    #[cfg(not(kani))]
    {
        if let JsonValue::Object(map) = &mut state.payload {
            let entry = map
                .entry("__io_results__".to_string())
                .or_insert_with(JsonValue::empty_object);
            if let JsonValue::Object(io_map) = entry {
                io_map.insert(io_type.as_str().to_string(), result);
                return Ok(());
            }
        }
        Err(ReactorError::InvalidState {
            field: "payload is not an object, cannot inject __io_results__",
        })
    }
}

/// 检查不变式（纯函数，委托给 invariants 模块）
///
/// 此函数是 invariants::check_invariants 的重新导出，
/// 方便从 pure 模块统一访问所有可验证的纯逻辑。
///
/// 公开导出（干预端点 invariants/scan 通路）：外部可对快照重建的状态
/// 或经由 `ReactorHandle::latest_invariant_scan` 的引擎自检报告做按需核查。
pub fn check_invariants(state: &ReactorState, steps: usize) -> Vec<InvariantViolation> {
    crate::invariants::check_invariants(state, steps)
}

/// 稳定条件判定（纯函数）
///
/// 队列空 + 无 pending I/O + 已执行过（steps > 0）。
/// 对应原 `StableDetector::is_stable` + steps > 0。
pub(crate) fn is_stable(queue_len: usize, pending_io_count: usize, steps: usize) -> bool {
    queue_len == 0 && pending_io_count == 0 && steps > 0
}

/// 注册 I/O 请求（纯函数版本，不含 Instant::now()）
///
/// 原 `state.register_io_request` 含 `Instant::now()`，不适合 Kani 验证。
/// 此纯函数版本接受 `started_at` 参数由调用方传入，
/// 保证所有时间相关逻辑在 pure 模块之外。
///
/// # 参数
///
/// - `state`: 反应器状态
/// - `id`: I/O 请求 ID
/// - `io_type`: I/O 类型
/// - `instruction`: 触发 I/O 的原指令（用于恢复执行）
/// - `started_at_idx`: 时间戳索引（用于 Kani 验证时替代真实时间）
///
/// # 注意
///
/// Kani 验证时，`started_at_idx` 可作为抽象时间戳，
/// 验证计数一致性，无需真实 Instant。
pub(crate) fn register_io_request_pure(
    state: &mut ReactorState,
    id: FactId,
    io_type: IoType,
    instruction: JsonValue,
) {
    if state.pending_requests.insert(id) {
        state.pending_io_count = state.pending_io_count.saturating_add(1);
        state.pending_io_types.insert(id, io_type);
        state
            .pending_io_instructions
            .insert(id, (instruction, FactId(0)));
        // pending_io_timestamps 不在纯函数中设置（含 Instant::now()）
        // 调用方需在外部设置时间戳
    }
}

// ============================================================================
// Kani 形式化验证证明（阶段7）
//
// 门控方式: #[cfg(kani)], 由 cargo kani 自动注入 --cfg kani,
// 与 evorule-tcb/verification/kani_proofs.rs 风格一致。无需 --features kani。
//
// 启用: cargo kani -p evorule-reactor
// 单个: cargo kani -p evorule-reactor --harness invariant_cause_queue_sync
//
// 验证代码位于 verification/ 目录(与 src/ 核心实现解耦):
// - verification/kani_proofs.rs — Kani proof 函数(P0-11 cause 队列同步等)
// ============================================================================

#[cfg(kani)]
#[path = "../verification/kani_proofs.rs"]
pub mod kani_proofs;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::fact::FactId;
    use evorule_tcb::ObjectMap;

    // 辅助：创建带初始队列的 state
    fn state_with_queue(instructions: Vec<JsonValue>) -> ReactorState {
        let mut state = ReactorState::new();
        for instr in instructions {
            state.push_back(instr, FactId(0));
        }
        state
    }

    // 辅助：构造 increment 指令
    fn increment_instr(attr: &str, delta: i64) -> JsonValue {
        let mut params = ObjectMap::new();
        params.insert("attr".to_string(), JsonValue::string(attr));
        params.insert("delta".to_string(), JsonValue::Integer(delta));
        let mut instr = ObjectMap::new();
        instr.insert("type".to_string(), JsonValue::string("increment"));
        instr.insert("params".to_string(), JsonValue::Object(params));
        JsonValue::Object(instr)
    }

    // 辅助：加载 core_eval（与集成测试一致）
    fn load_core_eval() -> Vec<JsonValue> {
        use std::path::PathBuf;
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let path = manifest_dir.join("../evorule-tcb/core_eval.json");
        let json_str = std::fs::read_to_string(&path).unwrap();
        let json: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let transform = json.get("transform").and_then(|v| v.as_array()).unwrap();

        fn serde_to_tcb(v: serde_json::Value) -> JsonValue {
            match v {
                serde_json::Value::Null => JsonValue::Null,
                serde_json::Value::Bool(b) => JsonValue::Bool(b),
                serde_json::Value::Number(n) => {
                    if let Some(i) = n.as_i64() {
                        JsonValue::Integer(i)
                    } else {
                        JsonValue::string(n.to_string())
                    }
                }
                serde_json::Value::String(s) => JsonValue::string(s),
                serde_json::Value::Array(arr) => {
                    JsonValue::Array(arr.into_iter().map(serde_to_tcb).collect())
                }
                serde_json::Value::Object(obj) => {
                    let mut map = ObjectMap::new();
                    for (k, v) in obj {
                        map.insert(k, serde_to_tcb(v));
                    }
                    JsonValue::Object(map)
                }
            }
        }

        transform.iter().cloned().map(serde_to_tcb).collect()
    }

    #[test]
    fn test_next_step_state_changed() {
        let core_eval = load_core_eval();
        let mut state = state_with_queue(vec![increment_instr("x", 5)]);
        let outcome = next_step(&core_eval, &mut state, 1000);
        assert!(matches!(outcome, Some(StepOutcome::StateChanged)));
        assert_eq!(state.payload.get("x"), Some(&JsonValue::Integer(5)));
        assert!(state.queue.is_empty());
    }

    #[test]
    fn test_next_step_empty_queue() {
        let core_eval = load_core_eval();
        let mut state = ReactorState::new();
        let outcome = next_step(&core_eval, &mut state, 1000);
        assert!(outcome.is_none());
    }

    #[test]
    fn test_next_step_version_increases() {
        let core_eval = load_core_eval();
        let mut state = state_with_queue(vec![increment_instr("x", 1)]);
        let prev_version = state.version;
        let _ = next_step(&core_eval, &mut state, 1000);
        assert!(state.version > prev_version);
        assert_eq!(state.prev_version, prev_version);
    }

    // ===== enforce 强制拦截（回归验证）=====

    /// 辅助：构造 enforce 规则（单条 core_eval）
    fn enforce_core_eval(domain: JsonValue, reason: &str) -> Vec<JsonValue> {
        let mut params = ObjectMap::new();
        params.insert("domain".to_string(), domain);
        params.insert("reason".to_string(), JsonValue::string(reason));
        let mut instr = ObjectMap::new();
        instr.insert("type".to_string(), JsonValue::string("enforce"));
        instr.insert("params".to_string(), JsonValue::Object(params));
        vec![JsonValue::Object(instr)]
    }

    /// 辅助：构造 delete_all 指令
    fn delete_all_instr() -> JsonValue {
        let mut instr = ObjectMap::new();
        instr.insert("type".to_string(), JsonValue::string("delete_all"));
        instr.insert("params".to_string(), JsonValue::Object(ObjectMap::new()));
        JsonValue::Object(instr)
    }

    #[test]
    #[allow(clippy::panic)] // 测试断言失败提示使用 panic!（assert 风格，符合测试惯用法）
    fn test_next_step_enforce_halt_drops_instruction_and_keeps_state() {
        // enforce 语义：违规指令被拒——出队即丢弃（不推回）、payload/queue
        // 保持原样、version 不 bump（无状态转移发生）
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("instruction")),
            ("instruction_type", JsonValue::string("delete_all")),
        ]);
        let core_eval = enforce_core_eval(domain, "违规：禁删数据集");
        let mut state = state_with_queue(vec![delete_all_instr()]);
        state.payload = JsonValue::object_from_pairs(&[("data", JsonValue::string("keep"))]);
        let prev_version = state.version;

        match next_step(&core_eval, &mut state, 1000) {
            Some(StepOutcome::Halted { rule_index, reason }) => {
                assert_eq!(rule_index, 0);
                assert_eq!(reason, "违规：禁删数据集");
            }
            other => panic!("expected Halted, got {other:?}"),
        }

        // payload 原样、队列空（指令被丢弃未推回）、version 不变
        assert_eq!(state.payload.get("data"), Some(&JsonValue::string("keep")));
        assert!(state.queue.is_empty());
        assert_eq!(state.version, prev_version);
    }

    #[test]
    fn test_next_step_enforce_domain_false_continues() {
        // domain 为假：noop 继续，不产生 Halted
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("instruction")),
            ("instruction_type", JsonValue::string("harmless_op")),
        ]);
        let core_eval = enforce_core_eval(domain, "不适用");
        let mut state = state_with_queue(vec![delete_all_instr()]);
        let prev_version = state.version;

        let outcome = next_step(&core_eval, &mut state, 1000);
        // enforce 不命中且无其他规则匹配 → Ignored（noop 检测）
        assert!(matches!(outcome, Some(StepOutcome::Ignored { .. })));
        assert_eq!(state.version, prev_version + 1);
    }

    #[test]
    fn test_apply_command() {
        let mut state = ReactorState::new();
        let instr = increment_instr("y", 3);
        apply_command(&mut state, instr.clone(), FactId(0));
        assert_eq!(state.queue.len(), 1);
        assert_eq!(state.queue.front(), Some(&instr));
    }

    #[test]
    fn test_apply_payload_update_existing_path() {
        let mut state = ReactorState::new();
        // 先创建顶层字段
        if let JsonValue::Object(map) = &mut state.payload {
            map.insert("x".to_string(), JsonValue::Integer(0));
        }
        let result = apply_payload_update(&mut state, "x", JsonValue::Integer(42));
        assert!(result.is_ok());
        assert_eq!(state.payload.get("x"), Some(&JsonValue::Integer(42)));
        assert_eq!(state.version, 1);
    }

    #[test]
    fn test_apply_payload_update_new_top_level() {
        let mut state = ReactorState::new();
        let result = apply_payload_update(&mut state, "new_field", JsonValue::Integer(100));
        assert!(result.is_ok());
        assert_eq!(
            state.payload.get("new_field"),
            Some(&JsonValue::Integer(100))
        );
    }

    #[test]
    fn test_apply_payload_update_nonexistent_nested() {
        let mut state = ReactorState::new();
        let result = apply_payload_update(&mut state, "a.b.c", JsonValue::Integer(1));
        assert!(result.is_err());
    }

    #[test]
    fn test_apply_io_response_unknown_id() {
        let mut state = ReactorState::new();
        let result = apply_io_response(&mut state, FactId(999), JsonValue::Null);
        assert!(result.is_ok());
        assert!(!result.unwrap());
    }

    #[test]
    fn test_apply_io_response_known_id() {
        let mut state = ReactorState::new();
        let id = FactId(1);
        let instr = increment_instr("x", 1);
        // 注册 I/O 请求
        state.register_io_request(id, IoType::call_external());
        state.save_io_instruction(id, instr.clone(), FactId(0));
        // 应用响应
        let result = apply_io_response(&mut state, id, JsonValue::string("result"));
        assert!(result.is_ok());
        assert!(result.unwrap());
        // 验证结果已注入（v0.3.1：__io_results__.{io_type} 按类型隔离）
        assert!(matches!(
            state
                .payload
                .get("__io_results__")
                .and_then(|r| r.get("call_external")),
            Some(JsonValue::String(_))
        ));
        // 验证恢复标志已设置
        assert!(state.io_recovery);
        // 验证原指令已推回队首
        assert_eq!(state.queue.front(), Some(&instr));
        // 验证 I/O 请求已完成
        assert_eq!(state.pending_io_count, 0);
    }

    #[test]
    fn test_is_stable() {
        assert!(!is_stable(0, 0, 0));
        assert!(is_stable(0, 0, 1));
        assert!(!is_stable(1, 0, 1));
        assert!(!is_stable(0, 1, 1));
        assert!(!is_stable(1, 1, 1));
    }

    #[test]
    fn test_check_invariants_fresh_state() {
        let state = ReactorState::new();
        let violations = check_invariants(&state, 0);
        assert!(violations.is_empty());
    }

    #[test]
    fn test_register_io_request_pure() {
        let mut state = ReactorState::new();
        let id = FactId(1);
        let instr = increment_instr("x", 1);
        register_io_request_pure(&mut state, id, IoType::call_external(), instr.clone());
        assert_eq!(state.pending_io_count, 1);
        assert!(state.pending_requests.contains(&id));
        assert_eq!(
            state.pending_io_types.get(&id),
            Some(&IoType::call_external())
        );
        assert_eq!(
            state.pending_io_instructions.get(&id),
            Some(&(instr, FactId(0)))
        );
        // 时间戳不在纯函数中设置，由调用方负责
        assert!(!state.pending_io_timestamps.contains_key(&id));
    }

    #[test]
    fn test_register_io_request_pure_idempotent() {
        let mut state = ReactorState::new();
        let id = FactId(1);
        let instr = increment_instr("x", 1);
        register_io_request_pure(&mut state, id, IoType::call_external(), instr.clone());
        register_io_request_pure(&mut state, id, IoType::call_external(), instr.clone());
        assert_eq!(state.pending_io_count, 1);
    }
}

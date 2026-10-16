// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 状态转换核心 - 执行 `core_eval` transform 列表，产生新状态或 I/O 请求
//!
//! # 核心概念
//! - `core_eval`：transform 规则列表，由 `core_eval.json` 编译生成
//! - `instruction`：当前指令（由队列弹出）
//! - `payload`：业务状态
//! - `queue`：待执行指令队列
//!
//! # 设计原则
//! - 纯函数：相同输入 → 相同输出
//! - 永不 panic：所有错误返回 `TcbError`
//! - I/O 请求通过 `core_eval` 中的 `io_request` 元指令触发

use crate::domain::DomainAttribution;
use crate::error::TcbError;
use crate::executor::{
    execute_meta_instruction_budgeted, MetaInstructionResult, MAX_TOTAL_META_INSTRUCTIONS,
};
use crate::path::resolve_path;
use crate::value::{JsonValue, ObjectMap};
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// `core_eval` transform 规则数量上限
///
/// 终止性保证：防止恶意或误构造的超长 `core_eval` 导致
/// `execute_transition` 迭代时间不可控。
pub const MAX_TRANSFORM_RULES: usize = 64;

/// 单条 transform 规则的命中归因
///
/// 记录一次 `execute_transition` 中第 `index` 条规则（对输入列表的下标）
/// 是否**结构命中**。口径与字段语义见 [`executor` 模块]
/// （`execute_meta_instruction_budgeted` 的命中归因小节）：
/// 直接指令执行成功即命中；`branch` 所选分支存在且非空即命中。
///
/// 归因标识：引擎只产出列表下标（引擎无来源知识）；来源标签
/// （core_eval / rules/<相对路径>）由装载层按合并分段映射——
/// 见 server 侧 `RulesetLayout`。
#[derive(Debug, Clone, PartialEq)]
pub struct RuleHit {
    /// 规则在输入 `core_eval` 列表中的下标
    pub index: usize,
    /// 规则顶层指令类型（如 "branch"、"set"；缺失记 "unknown"）
    pub instr_type: String,
    /// 是否结构命中
    pub hit: bool,
    /// 域判定归因（branch/enforce 专属；其余指令类型为 `None`）
    ///
    /// 专项-20261001 方案 2' v4：eq/lt 域遇到状态侧缺失/不可比/引用歧义时，
    /// 二态投影压平为 false 的信息由本字段还原，供 FactsLog 落账与消费面
    /// 审计「为什么走了 on_false / 为什么没拦住」（含 on_missing 声明值）。
    /// - **R1 归因不回灌执行**：branch 走向 / enforce 命中仅由投影 bool +
    ///   规则文本 `on_missing` 静态声明决定，本字段不参与执行；
    /// - **R2 半成品纪律**：`IoRequired`/`Halted` 不携带 `rule_hits`，
    ///   中途归因不随半成品交付，以收敛后的重放结果为准。
    pub domain_attr: Option<DomainAttribution>,
}

/// 状态转换结果
#[derive(Debug, Clone, PartialEq)]
pub enum TransitionResult {
    /// 正常状态转换：新 payload 和新 queue
    State {
        /// 新的业务状态
        new_payload: JsonValue,
        /// 新的指令队列
        new_queue: Vec<JsonValue>,
        /// 本步各 transform 规则的命中归因（按执行顺序，与输入列表等长）
        rule_hits: Vec<RuleHit>,
    },
    /// I/O 请求：需要上层反应器执行 I/O
    ///
    /// # 重放契约（D11，v0.3.2 审计治理）
    ///
    /// `IoRequired` 是**纯信号**——返回值不携带新状态，调用方无法获得半成品 payload/queue。
    /// 首次执行中 core_eval 前序规则产生的状态修改**全部丢弃**（不提交），
    /// 反应器收到 I/O 结果后必须**从原始输入整体重放** `execute_transition`：
    ///
    /// 1. **纯信号语义** — 仅含 `io_type` 与 `params`（路径引用已解析为具体值）。
    /// 2. **部分修改丢弃** — 首次执行到 `io_request` 前的状态变更不返回；正确性由 D5（`io_request` 必须是叶子）保证。
    /// 3. **重放确定性** — 重放 = 以相同 `(core_eval, instruction, payload, queue)` 再次调用；TCB 纯函数性质保证路径一致。
    /// 4. **传播即停** — `IoRequired` 立即向上传播，其后 transform 规则不执行。
    ///
    /// **错误实现代价**：若反应器错误地持久化中间 payload 再增量续跑，
    /// `io_request` 之前的 `add` / `sub` 会被应用两次（state 污染）。
    /// 跨层协议详见 `TCB_SPEC.md` §四 D11。
    ///
    /// **命中归因**：`IoRequired` **不携带** `rule_hits`——中途
    /// 命中属于"半成品"（同状态修改一并丢弃），以收敛后的重放结果为准，
    /// 避免重放导致重复计数。归因只随 `State` / `Ignored` 收敛变体交付。
    IoRequired {
        /// I/O 类型（如 "call_external"、"query_db" 等）
        io_type: String,
        /// I/O 请求参数（路径引用已解析为具体值）
        params: JsonValue,
    },
    /// `enforce` 强制原语命中（回归验证）：违规指令被拒绝执行
    ///
    /// # 语义（与 `IoRequired` 同构的纯信号）
    ///
    /// - **传播即停** — 命中 enforce 规则后立即返回，剩余 transform 不执行；
    /// - **半成品丢弃** — 此前规则的状态修改全部丢弃（不返回 payload/queue），
    ///   违规动作不得"执行一半"；
    /// - **指令被拒** — 反应器据此不执行该指令、不推回队列（动作被阻止而非
    ///   仅留痕），并系统独占发射 `Fact::Violation` 全链留痕（三权分立：
    ///   阻止权/记录权在系统，LLM/规则零通道伪造）；
    /// - **归因** — 不携带 `rule_hits`（半成品纪律）；`rule_index` 为命中的
    ///   enforce 规则下标（由转换层填入），`reason` 透传 params.reason；
    /// - **重放确定性** — 相同输入必产生相同 `Halted`（纯函数性质）。
    Halted {
        /// 命中的 enforce 规则在输入 `core_eval` 列表中的下标
        rule_index: usize,
        /// 违规说明（enforce params.reason）
        reason: String,
    },
    /// 指令被忽略（没有匹配的 transform 规则，或规则产生 noop 效果）
    ///
    /// 显式暴露“静默失败”情况，方便上层（reactor/治理层）记录告警或产生 Error
    Ignored {
        /// 被忽略的指令类型（用于日志/审计）
        instruction_type: String,
        /// 说明：通常为 "no matching transform rule" 或 "rule matched but produced no effect"
        reason: String,
        /// 本步各 transform 规则的命中归因（静默失败时全为未命中，
        /// 是死规则检测的直接数据源）
        rule_hits: Vec<RuleHit>,
    },
}

/// 执行一步状态转换
///
/// # 参数
/// - `core_eval`：transform 规则列表（由 `core_eval.json` 编译）
/// - `instruction`：当前指令
/// - `payload`：当前业务状态
/// - `queue`：当前指令队列
///
/// # 返回
/// - `Ok(State { new_payload, new_queue })`：正常转换
/// - `Ok(IoRequired { io_type, params })`：`core_eval` 中遇到 `io_request` 元指令
/// - `Err(TcbError)`：执行错误
///
/// # 保证
/// - 纯函数（无副作用）
/// - 永不 panic（所有错误返回 `TcbError`）
/// - 确定性（相同输入 → 相同输出）
///
/// # Errors
///
/// - `TcbError::TooManyTransformRules`：`core_eval` 规则数超过 `MAX_TRANSFORM_RULES`（64）
/// - `TcbError::InvalidState`：状态结构异常（`__exec__.payload` 不存在）
/// - `TcbError::PathResolutionFailed`：`core_eval` 中路径解析失败
///
/// # 代码示例
///
/// ```
/// extern crate alloc;
/// use evorule_tcb::{JsonValue, execute_transition, TransitionResult};
/// use alloc::collections::BTreeMap;
///
/// // 业务 payload: { x: 10 }
/// let mut p = BTreeMap::new();
/// p.insert("x".to_string(), JsonValue::Integer(10));
/// let payload = JsonValue::object(p);
///
/// // 当前指令: noop
/// let mut instr = BTreeMap::new();
/// instr.insert("type".to_string(), JsonValue::string("noop"));
/// let instruction = JsonValue::object(instr);
///
/// // core_eval 规则: set(x, set, 42)
/// let mut set_params = BTreeMap::new();
/// set_params.insert("attr".to_string(), JsonValue::string("x"));
/// set_params.insert("operation".to_string(), JsonValue::string("set"));
/// set_params.insert("value".to_string(), JsonValue::Integer(42));
/// let mut set_rule = BTreeMap::new();
/// set_rule.insert("type".to_string(), JsonValue::string("set"));
/// set_rule.insert("params".to_string(), JsonValue::object(set_params));
/// let core_eval = vec![JsonValue::object(set_rule)];
///
/// // 执行
/// let result = execute_transition(&core_eval, &instruction, &payload, &[]);
/// match result {
///     Ok(TransitionResult::State { new_payload, .. }) => {
///         // x 应该被改成 42
///         assert_eq!(new_payload.get("x").and_then(|v| v.as_i64()), Some(42));
///     }
///     Ok(_) => panic!("expected State result"),
///     Err(e) => panic!("unexpected error: {:?}", e),
/// }
/// ```
// 119 行: 约束前置门(BUG-P0-005) + transform 主循环 + 归因合并必须单函数原子语义,
// 拆函数需共享 budget 与 rule_hits 中间状态; 豁免登记见 GATE_REFERENCE.md §6.3
#[allow(clippy::too_many_lines)]
pub fn execute_transition(
    core_eval: &[JsonValue],
    instruction: &JsonValue,
    payload: &JsonValue,
    queue: &[JsonValue],
) -> Result<TransitionResult, TcbError> {
    // 0. 终止性检查：core_eval 规则数不得超过 MAX_TRANSFORM_RULES
    if core_eval.len() > MAX_TRANSFORM_RULES {
        return Err(TcbError::TooManyTransformRules {
            limit: MAX_TRANSFORM_RULES,
            actual: core_eval.len(),
        });
    }

    // 1. 构造 __exec__ 上下文
    let exec_state = build_exec_state(instruction, payload, queue);

    // 2. 执行 core_eval transform 列表（整棵规则树共享单一执行预算，
    //    M6 终止性宽度防线：约束 branch 子指令列表的宽度）
    //    同步收集每条规则的结构命中（命中口径见 RuleHit / executor 文档）。
    let mut state = exec_state.clone();
    let mut budget = MAX_TOTAL_META_INSTRUCTIONS;
    let mut rule_hits: Vec<RuleHit> = Vec::with_capacity(core_eval.len());

    // 2.0 【约束前置门】enforce 约束先于任何状态变换 / IO 路由求值
    //     （2026-09-23 P0 修复：L2 约束层被 L1 规则遮蔽导致守卫静默失效）
    //
    //     根因：原实现按列表顺序单趟求值，任一条更早的规则产生 `IoRequired`
    //     即"传播即停"立即返回，其后的 `enforce` 规则**永不求值**。而 server
    //     的合并装载顺序是"L1 core_eval 在前、L2/L3 在后"（`load_merged_with_layout`），
    //     L1 覆盖 set/increment/call_external 等核心原语，凡 L1 已处理的指令类型，
    //     其上的 L2 enforce 约束全部成为死规则——动作未被拦且无 Violation 留痕。
    //
    //     语义裁决：**约束不是状态变换**。enforce 必须在所有 transform 之前求值：
    //     - 不可被更早规则的 `IoRequired` 早退遮蔽；
    //     - 判定上下文固定为**转换前输入状态**（exec_state），不随规则在列表中的
    //       位置或前序半成品状态漂移，杜绝"靠调整规则加载顺序绕过约束"这一通道。
    //
    //     求值顺序仍按列表下标升序（确定性保持）；命中首个 enforce 即返回
    //     `Halted { rule_index }`，**下标契约不变**（= 合并规则列表下标，
    //     hit-stats 归因与重放确定性依赖此契约）。
    //
    //     已知边界：本门只覆盖**顶层** `type == "enforce"` 规则；嵌在 branch
    //     子指令内的 enforce 仍受"传播即停"影响（残留风险，见 P0 bug 报告）。
    let mut enforce_hits: Vec<RuleHit> = Vec::new();
    for (index, rule) in core_eval.iter().enumerate() {
        let instr_type = rule
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        if instr_type != "enforce" {
            continue;
        }
        let mut hit = false;
        let mut domain_attr = None;
        let result = execute_meta_instruction_budgeted(
            rule,
            exec_state.clone(),
            0,
            &mut budget,
            &mut hit,
            &mut domain_attr,
        )?;
        enforce_hits.push(RuleHit {
            index,
            instr_type: instr_type.to_string(),
            hit,
            domain_attr,
        });
        if let MetaInstructionResult::Halted { reason } = result {
            // 半成品纪律同 IoRequired：不携带 rule_hits，状态修改随丢弃
            return Ok(TransitionResult::Halted {
                rule_index: index,
                reason,
            });
        }
    }

    for (index, transform_rule) in core_eval.iter().enumerate() {
        let instr_type = transform_rule
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        // 约束门已求值过的 enforce 不重复执行：预算与命中归因均只计一次
        if instr_type == "enforce" {
            continue;
        }
        let mut hit = false;
        let mut domain_attr = None;
        let result = execute_meta_instruction_budgeted(
            transform_rule,
            state,
            0,
            &mut budget,
            &mut hit,
            &mut domain_attr,
        )?;
        rule_hits.push(RuleHit {
            index,
            instr_type,
            hit,
            domain_attr,
        });

        match result {
            MetaInstructionResult::State(new_state) => state = new_state,
            // I/O 请求信号：立即返回，不继续执行后续 transform。
            // 不携带 rule_hits（D11 重放契约：中途命中随半成品状态一并丢弃，
            // 归因以收敛后的重放结果为准，避免重复计数）。
            MetaInstructionResult::IoRequired { io_type, params } => {
                return Ok(TransitionResult::IoRequired { io_type, params });
            }
            // enforce 强制原语命中（回归验证）：立即返回 Halted 纯信号，
            // 不继续执行后续 transform；此前状态修改随半成品一并丢弃，
            // 不携带 rule_hits（半成品纪律同 IoRequired）。
            // rule_index 由转换层填入（此处即为命中规则下标，确定性成立）。
            MetaInstructionResult::Halted { reason } => {
                return Ok(TransitionResult::Halted {
                    rule_index: index,
                    reason,
                });
            }
        }
    }

    // 2.1 归因合并：enforce 命中在约束门阶段收集，此处按下标归并回全序。
    //     对外口径不变：按 index 升序、与输入列表等长（IoRequired/Halted
    //     早退路径不携带 rule_hits 的既有契约不变）。
    rule_hits.extend(enforce_hits);
    rule_hits.sort_by_key(|h| h.index);

    // 3. 静默失败检测：检查是否存在匹配当前指令的宪法规则
    //    规则分类（参见 docs/rule_taxonomy.md）：
    //    - 宪法规则 (Constitution Rules): core_eval.json 中定义的规则
    //      * instruction domain: 精确匹配 instruction_type
    //      * all domain: 兜底机制，不视为业务规则
    //      * 动态 domain (eq/lt/gt 等): 通用规则，可处理任何指令
    //      * 直接规则 (set/increment 等): 检查 rule_type 是否匹配 instruction_type
    //
    //    检测逻辑：
    //    1. 优先检查是否有实际副作用（payload/queue 变化）
    //    2. 检查是否存在匹配当前指令的宪法规则
    //    3. 无副作用且无匹配规则 → 返回 Ignored
    let instruction_type = instruction
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    // 3.1 检查是否存在匹配当前指令的宪法规则
    let has_matching_constitution_rule = core_eval.iter().any(|rule| {
        if let Some(params) = rule.get("params").and_then(|p| p.as_object()) {
            if let Some(domain) = params.get("domain").and_then(|d| d.as_object()) {
                if let Some(domain_type) = domain.get("type").and_then(|t| t.as_str()) {
                    if domain_type == "instruction" {
                        // 精确匹配 instruction_type
                        if let Some(expected_type) =
                            domain.get("instruction_type").and_then(|t| t.as_str())
                        {
                            return expected_type == instruction_type;
                        }
                        return false;
                    } else if domain_type == "all" {
                        return false; // all 兜底不是业务规则
                    } else {
                        // 动态 domain（eq, lt, gt, ge, not, exists, has_fields 等）
                        // 视为通用规则，可处理任何指令
                        return true;
                    }
                }
            }
        } else if let Some(rule_type) = rule.get("type").and_then(|t| t.as_str()) {
            // 直接规则（如 set, increment），检查 rule_type 是否匹配 instruction_type
            if matches!(rule_type, "set" | "increment" | "decrement" | "branch") {
                return rule_type == instruction_type;
            }
        }
        false
    });

    // 4. 提取结果
    let new_payload = resolve_path(&state, "__exec__.payload")
        .cloned()
        .ok_or_else(|| TcbError::InvalidState {
            reason: "__exec__.payload not found in state after transform execution".to_string(),
        })?;

    let new_queue = resolve_path(&state, "__exec__.queue")
        .and_then(|v| v.as_array().map(<[JsonValue]>::to_vec))
        .ok_or_else(|| TcbError::InvalidState {
            reason: "__exec__.queue not found or not an array after transform execution"
                .to_string(),
        })?;

    // 5. 检查是否有实际副作用（优先）
    let has_side_effect = &new_payload != payload || new_queue != queue;

    // 6. 组合判断：
    //    - 规则匹配 + 有/无副作用 → 返回 State（合法操作）
    //    - 规则不匹配 + 有副作用 → 返回 State（不应发生，但保守处理）
    //    - 规则不匹配 + 无副作用 → 返回 Ignored（真正的指令未被处理）
    if !has_matching_constitution_rule && !has_side_effect {
        return Ok(TransitionResult::Ignored {
            instruction_type: instruction_type.to_string(),
            reason: "instruction not matched by any constitution rule".to_string(),
            rule_hits,
        });
    }

    Ok(TransitionResult::State {
        new_payload,
        new_queue,
        rule_hits,
    })
}

/// 构建 `__exec__` 上下文
fn build_exec_state(
    instruction: &JsonValue,
    payload: &JsonValue,
    queue: &[JsonValue],
) -> JsonValue {
    let mut exec = ObjectMap::new();
    exec.insert("instruction".to_string(), instruction.clone());
    exec.insert("payload".to_string(), payload.clone());
    exec.insert("queue".to_string(), JsonValue::array(queue.to_vec()));

    let mut root = ObjectMap::new();
    root.insert("__exec__".to_string(), JsonValue::Object(exec));
    JsonValue::Object(root)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    #![allow(clippy::panic)]
    #![allow(clippy::indexing_slicing)]

    use super::*;
    use crate::domain::{DomainOutcome, MissingReason};
    use crate::value::JsonValue;
    use alloc::vec;

    // ===== 辅助函数 =====

    fn make_payload(x: i64) -> JsonValue {
        JsonValue::object_from_pairs(&[("x", JsonValue::Integer(x))])
    }

    fn make_instruction(instr_type: &str, params: &[(&str, JsonValue)]) -> JsonValue {
        JsonValue::object_from_pairs(&[
            ("type", JsonValue::string(instr_type)),
            ("params", JsonValue::object_from_pairs(params)),
        ])
    }

    /// 构造 `call_llm` 流程测试用的 `core_eval`
    fn call_llm_core_eval() -> Vec<JsonValue> {
        vec![JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("instruction")),
                            ("instruction_type", JsonValue::string("call_llm")),
                        ]),
                    ),
                    (
                        "on_true",
                        JsonValue::array(vec![JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("branch")),
                            (
                                "params",
                                JsonValue::object_from_pairs(&[
                                    (
                                        "domain",
                                        JsonValue::object_from_pairs(&[
                                            ("type", JsonValue::string("exists")),
                                            (
                                                "path",
                                                JsonValue::string("__exec__.payload.__io_result__"),
                                            ),
                                        ]),
                                    ),
                                    (
                                        "on_true",
                                        JsonValue::array(vec![JsonValue::object_from_pairs(&[
                                            ("type", JsonValue::string("set")),
                                            (
                                                "params",
                                                JsonValue::object_from_pairs(&[
                                                    ("attr", JsonValue::string("llm_response")),
                                                    ("operation", JsonValue::string("set")),
                                                    (
                                                        "value",
                                                        JsonValue::string(
                                                            "__exec__.payload.__io_result__",
                                                        ),
                                                    ),
                                                ]),
                                            ),
                                        ])]),
                                    ),
                                    (
                                        "on_false",
                                        JsonValue::array(vec![JsonValue::object_from_pairs(&[
                                            ("type", JsonValue::string("io_request")),
                                            (
                                                "params",
                                                JsonValue::object_from_pairs(&[
                                                    ("io_type", JsonValue::string("call_llm")),
                                                    (
                                                        "prompt",
                                                        JsonValue::string(
                                                            "__exec__.instruction.params.prompt",
                                                        ),
                                                    ),
                                                ]),
                                            ),
                                        ])]),
                                    ),
                                ]),
                            ),
                        ])]),
                    ),
                ]),
            ),
        ])]
    }

    /// 构造多轮模拟测试用的 `core_eval`
    fn multi_round_core_eval() -> Vec<JsonValue> {
        vec![
            // spawn 映射：push 两个 increment
            JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("branch")),
                (
                    "params",
                    JsonValue::object_from_pairs(&[
                        (
                            "domain",
                            JsonValue::object_from_pairs(&[
                                ("type", JsonValue::string("instruction")),
                                ("instruction_type", JsonValue::string("spawn")),
                            ]),
                        ),
                        (
                            "on_true",
                            JsonValue::array(vec![JsonValue::object_from_pairs(&[
                                ("type", JsonValue::string("push")),
                                (
                                    "params",
                                    JsonValue::object_from_pairs(&[(
                                        "instructions",
                                        JsonValue::array(vec![
                                            make_instruction(
                                                "increment",
                                                &[("delta", JsonValue::Integer(5))],
                                            ),
                                            make_instruction(
                                                "increment",
                                                &[("delta", JsonValue::Integer(10))],
                                            ),
                                        ]),
                                    )]),
                                ),
                            ])]),
                        ),
                    ]),
                ),
            ]),
            // increment 映射：set x = x + delta
            JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("branch")),
                (
                    "params",
                    JsonValue::object_from_pairs(&[
                        (
                            "domain",
                            JsonValue::object_from_pairs(&[
                                ("type", JsonValue::string("instruction")),
                                ("instruction_type", JsonValue::string("increment")),
                            ]),
                        ),
                        (
                            "on_true",
                            JsonValue::array(vec![JsonValue::object_from_pairs(&[
                                ("type", JsonValue::string("set")),
                                (
                                    "params",
                                    JsonValue::object_from_pairs(&[
                                        ("attr", JsonValue::string("x")),
                                        ("operation", JsonValue::string("add")),
                                        (
                                            "value",
                                            JsonValue::string("__exec__.instruction.params.delta"),
                                        ),
                                    ]),
                                ),
                            ])]),
                        ),
                    ]),
                ),
            ]),
        ]
    }

    // ===== build_exec_state 测试 =====

    #[test]
    fn test_build_exec_state() {
        let payload = make_payload(42);
        let queue = vec![JsonValue::string("test")];
        let instruction = make_instruction("noop", &[]);
        let state = build_exec_state(&instruction, &payload, &queue);

        assert!(resolve_path(&state, "__exec__").is_some());
        assert!(resolve_path(&state, "__exec__.instruction").is_some());
        assert!(resolve_path(&state, "__exec__.payload").is_some());
        assert!(resolve_path(&state, "__exec__.queue").is_some());
        assert_eq!(
            resolve_path(&state, "__exec__.payload.x"),
            Some(&JsonValue::Integer(42))
        );
    }

    // ===== 正常状态转换测试 =====

    #[test]
    fn test_execute_transition_empty_core_eval() {
        let instruction = make_instruction("noop", &[]);
        let payload = make_payload(42);
        let core_eval: Vec<JsonValue> = vec![];

        let result = execute_transition(&core_eval, &instruction, &payload, &[]).unwrap();
        match result {
            TransitionResult::Ignored {
                instruction_type, ..
            } => {
                assert_eq!(instruction_type, "noop");
            }
            other => panic!("empty core_eval should return Ignored, got: {:?}", other),
        }
    }

    #[test]
    fn test_execute_transition_increment() {
        let instruction = make_instruction(
            "increment",
            &[
                ("attr", JsonValue::string("x")),
                ("delta", JsonValue::Integer(5)),
            ],
        );
        let payload = make_payload(10);

        let core_eval = vec![JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("instruction")),
                            ("instruction_type", JsonValue::string("increment")),
                        ]),
                    ),
                    (
                        "on_true",
                        JsonValue::array(vec![JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("set")),
                            (
                                "params",
                                JsonValue::object_from_pairs(&[
                                    ("attr", JsonValue::string("x")),
                                    ("operation", JsonValue::string("add")),
                                    (
                                        "value",
                                        JsonValue::string("__exec__.instruction.params.delta"),
                                    ),
                                ]),
                            ),
                        ])]),
                    ),
                ]),
            ),
        ])];

        let result = execute_transition(&core_eval, &instruction, &payload, &[]).unwrap();
        match result {
            TransitionResult::State { new_payload, .. } => {
                assert_eq!(new_payload.get("x"), Some(&JsonValue::Integer(15)));
            }
            _ => panic!("expected State"),
        }
    }

    #[test]
    fn test_execute_transition_multiple_transforms_sequential() {
        let instruction = make_instruction("noop", &[]);
        let payload = make_payload(0);

        let core_eval = vec![
            JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("set")),
                (
                    "params",
                    JsonValue::object_from_pairs(&[
                        ("attr", JsonValue::string("x")),
                        ("operation", JsonValue::string("set")),
                        ("value", JsonValue::Integer(10)),
                    ]),
                ),
            ]),
            JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("set")),
                (
                    "params",
                    JsonValue::object_from_pairs(&[
                        ("attr", JsonValue::string("y")),
                        ("operation", JsonValue::string("set")),
                        ("value", JsonValue::Integer(20)),
                    ]),
                ),
            ]),
        ];

        let result = execute_transition(&core_eval, &instruction, &payload, &[]).unwrap();
        match result {
            TransitionResult::State { new_payload, .. } => {
                assert_eq!(new_payload.get("x"), Some(&JsonValue::Integer(10)));
                assert_eq!(new_payload.get("y"), Some(&JsonValue::Integer(20)));
            }
            _ => panic!("expected State"),
        }
    }

    #[test]
    fn test_execute_transition_no_matching_transform_state_preserved() {
        let instruction = make_instruction("unknown_instr", &[]);
        let payload = make_payload(42);

        let core_eval = vec![JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("instruction")),
                            ("instruction_type", JsonValue::string("increment")),
                        ]),
                    ),
                    (
                        "on_true",
                        JsonValue::array(vec![JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("set")),
                            (
                                "params",
                                JsonValue::object_from_pairs(&[
                                    ("attr", JsonValue::string("x")),
                                    ("operation", JsonValue::string("set")),
                                    ("value", JsonValue::Integer(999)),
                                ]),
                            ),
                        ])]),
                    ),
                ]),
            ),
        ])];

        let result = execute_transition(&core_eval, &instruction, &payload, &[]).unwrap();
        match result {
            TransitionResult::Ignored {
                instruction_type, ..
            } => {
                assert_eq!(instruction_type, "unknown_instr");
            }
            other => panic!("expected Ignored, got: {:?}", other),
        }
    }

    // ===== push 测试 =====

    #[test]
    fn test_execute_transition_push_returns_new_queue() {
        let instruction = make_instruction("spawn", &[]);
        let payload = make_payload(0);

        let core_eval = vec![JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("instruction")),
                            ("instruction_type", JsonValue::string("spawn")),
                        ]),
                    ),
                    (
                        "on_true",
                        JsonValue::array(vec![JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("push")),
                            (
                                "params",
                                JsonValue::object_from_pairs(&[(
                                    "instructions",
                                    JsonValue::array(vec![
                                        make_instruction(
                                            "increment",
                                            &[("delta", JsonValue::Integer(1))],
                                        ),
                                        make_instruction(
                                            "increment",
                                            &[("delta", JsonValue::Integer(2))],
                                        ),
                                    ]),
                                )]),
                            ),
                        ])]),
                    ),
                ]),
            ),
        ])];

        let result = execute_transition(&core_eval, &instruction, &payload, &[]).unwrap();
        match result {
            TransitionResult::State { new_queue, .. } => {
                assert_eq!(new_queue.len(), 2);
                assert_eq!(
                    new_queue[0].get("type").and_then(|v| v.as_str()),
                    Some("increment")
                );
            }
            _ => panic!("expected State"),
        }
    }

    // ===== I/O 请求测试 =====

    #[test]
    fn test_execute_transition_io_request() {
        let instruction =
            make_instruction("call_llm", &[("prompt", JsonValue::string("Hello, LLM!"))]);
        let payload = make_payload(0);

        let core_eval = vec![JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("instruction")),
                            ("instruction_type", JsonValue::string("call_llm")),
                        ]),
                    ),
                    (
                        "on_true",
                        JsonValue::array(vec![JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("io_request")),
                            (
                                "params",
                                JsonValue::object_from_pairs(&[
                                    ("io_type", JsonValue::string("call_llm")),
                                    (
                                        "prompt",
                                        JsonValue::string("__exec__.instruction.params.prompt"),
                                    ),
                                ]),
                            ),
                        ])]),
                    ),
                ]),
            ),
        ])];

        let result = execute_transition(&core_eval, &instruction, &payload, &[]).unwrap();
        match result {
            TransitionResult::IoRequired { io_type, params } => {
                assert_eq!(io_type, "call_llm");
                assert_eq!(
                    params.get("prompt").and_then(|v| v.as_str()),
                    Some("Hello, LLM!")
                );
            }
            _ => panic!("expected IoRequired"),
        }
    }

    #[test]
    fn test_execute_transition_io_request_stops_subsequent_transforms() {
        let instruction = make_instruction("call_llm", &[("prompt", JsonValue::string("test"))]);
        let payload = make_payload(0);

        let core_eval = vec![
            JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("branch")),
                (
                    "params",
                    JsonValue::object_from_pairs(&[
                        (
                            "domain",
                            JsonValue::object_from_pairs(&[
                                ("type", JsonValue::string("instruction")),
                                ("instruction_type", JsonValue::string("call_llm")),
                            ]),
                        ),
                        (
                            "on_true",
                            JsonValue::array(vec![JsonValue::object_from_pairs(&[
                                ("type", JsonValue::string("io_request")),
                                (
                                    "params",
                                    JsonValue::object_from_pairs(&[
                                        ("io_type", JsonValue::string("call_llm")),
                                        ("prompt", JsonValue::string("test")),
                                    ]),
                                ),
                            ])]),
                        ),
                    ]),
                ),
            ]),
            JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("set")),
                (
                    "params",
                    JsonValue::object_from_pairs(&[
                        ("attr", JsonValue::string("x")),
                        ("operation", JsonValue::string("set")),
                        ("value", JsonValue::Integer(999)),
                    ]),
                ),
            ]),
        ];

        let result = execute_transition(&core_eval, &instruction, &payload, &[]).unwrap();
        assert!(matches!(result, TransitionResult::IoRequired { .. }));
    }

    #[test]
    fn test_execute_transition_non_io_instruction_no_io_request() {
        let instruction = make_instruction("increment", &[("delta", JsonValue::Integer(1))]);
        let payload = make_payload(5);

        let core_eval = vec![JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("instruction")),
                            ("instruction_type", JsonValue::string("increment")),
                        ]),
                    ),
                    (
                        "on_true",
                        JsonValue::array(vec![JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("set")),
                            (
                                "params",
                                JsonValue::object_from_pairs(&[
                                    ("attr", JsonValue::string("x")),
                                    ("operation", JsonValue::string("add")),
                                    ("value", JsonValue::Integer(1)),
                                ]),
                            ),
                        ])]),
                    ),
                ]),
            ),
        ])];

        let result = execute_transition(&core_eval, &instruction, &payload, &[]).unwrap();
        assert!(matches!(result, TransitionResult::State { .. }));
    }

    // ===== tool_trace 轨迹记录通道测试 =====

    /// `tool_trace` 规则（内联构造，不加载任何规则集文件；与部署面
    /// server_eval.json 的 tool_trace transform 同构：branch(tool_trace) →
    /// set(attr=params.attr, value=params.value)；该规则自 v0.6.0 起属
    /// 部署面规则集，主仓宪法已迁出应用层策略规则）
    fn tool_trace_rule() -> JsonValue {
        JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("instruction")),
                            ("instruction_type", JsonValue::string("tool_trace")),
                        ]),
                    ),
                    (
                        "on_true",
                        JsonValue::array(vec![JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("set")),
                            (
                                "params",
                                JsonValue::object_from_pairs(&[
                                    (
                                        "attr",
                                        JsonValue::string("__exec__.instruction.params.attr"),
                                    ),
                                    ("operation", JsonValue::string("set")),
                                    (
                                        "value",
                                        JsonValue::string("__exec__.instruction.params.value"),
                                    ),
                                ]),
                            ),
                        ])]),
                    ),
                ]),
            ),
        ])
    }

    /// tool_trace 指令（轨迹形态）：attr=meta_tool.tool_traces.<seq>（序号由
    /// 应用层控制实现 payload 累积），value=轨迹全文（tool_name/args/ok/duration/seq）
    /// TCB 零依赖（无 std），attr 由调用方以字面量给出。
    fn tool_trace_instr(attr: &'static str, seq: i64, tool: &str) -> JsonValue {
        make_instruction(
            "tool_trace",
            &[
                ("attr", JsonValue::string(attr)),
                (
                    "value",
                    JsonValue::object_from_pairs(&[
                        ("tool_name", JsonValue::string(tool)),
                        (
                            "args",
                            JsonValue::object_from_pairs(&[(
                                "command",
                                JsonValue::string("echo hi"),
                            )]),
                        ),
                        ("ok", JsonValue::Bool(true)),
                        ("duration_ms", JsonValue::Integer(12)),
                        ("seq", JsonValue::Integer(seq)),
                    ]),
                ),
            ],
        )
    }

    #[test]
    fn test_tool_trace_writes_value_into_payload() {
        // tool_trace 指令 → 部署面规则集 transform set 入 payload（meta_tool 缺失时中间路径自动创建）
        // 关键：payload 必须变化——noop 效果会被判 Ignored（反应器产生 Error 事实），
        // 本测试即 tool_trace「Ignored 陷阱」的回归验证。
        let instruction = tool_trace_instr("meta_tool.tool_traces.0", 0, "shell_exec");
        let payload = JsonValue::object_from_pairs(&[]);

        match execute_transition(&[tool_trace_rule()], &instruction, &payload, &[]).unwrap() {
            TransitionResult::State { new_payload, .. } => {
                let entry = new_payload
                    .get("meta_tool")
                    .and_then(|m| m.get("tool_traces"))
                    .and_then(|t| t.get("0"))
                    .and_then(|e| e.get("tool_name"))
                    .and_then(|v| v.as_str());
                assert_eq!(entry, Some("shell_exec"));
            }
            other => panic!("expected State with trace payload, got {:?}", other),
        }
    }

    #[test]
    fn test_tool_trace_accumulates_two_entries() {
        // 两条轨迹（seq 0/1）先后转换：第二条以第一条输出为输入，验证按序号累积互不覆盖
        let first = tool_trace_instr("meta_tool.tool_traces.0", 0, "file_read");
        let second = tool_trace_instr("meta_tool.tool_traces.1", 1, "shell_exec");
        let payload = JsonValue::object_from_pairs(&[]);

        let state1 = match execute_transition(&[tool_trace_rule()], &first, &payload, &[]).unwrap()
        {
            TransitionResult::State { new_payload, .. } => new_payload,
            other => panic!("first: expected State, got {:?}", other),
        };
        match execute_transition(&[tool_trace_rule()], &second, &state1, &[]).unwrap() {
            TransitionResult::State { new_payload, .. } => {
                let t0 = new_payload
                    .get("meta_tool")
                    .and_then(|m| m.get("tool_traces"))
                    .and_then(|t| t.get("0"))
                    .and_then(|e| e.get("tool_name"))
                    .and_then(|v| v.as_str());
                let t1 = new_payload
                    .get("meta_tool")
                    .and_then(|m| m.get("tool_traces"))
                    .and_then(|t| t.get("1"))
                    .and_then(|e| e.get("tool_name"))
                    .and_then(|v| v.as_str());
                assert_eq!(t0, Some("file_read"));
                assert_eq!(t1, Some("shell_exec"));
            }
            other => panic!("second: expected State, got {:?}", other),
        }
    }

    #[test]
    fn test_tool_trace_enforce_halts_on_violation() {
        // 约束前置门对 instruction_type=tool_trace 精确求值：违规轨迹在约束门拦截
        // （Halted→反应器派生 Violation 留痕），合法轨迹才落 StateTransition——
        // 两类留痕互补（部署面 server_eval.json constraints 契约）。
        let instruction = tool_trace_instr("meta_tool.tool_traces.0", 0, "shell_exec");
        let payload = JsonValue::object_from_pairs(&[]);
        let core_eval = vec![
            enforce_rule(
                JsonValue::object_from_pairs(&[
                    ("type", JsonValue::string("instruction")),
                    ("instruction_type", JsonValue::string("tool_trace")),
                ]),
                Some("违规：工具轨迹命中危险命令黑名单"),
            ),
            tool_trace_rule(),
        ];

        match execute_transition(&core_eval, &instruction, &payload, &[]).unwrap() {
            TransitionResult::Halted { rule_index, reason } => {
                assert_eq!(rule_index, 0);
                assert_eq!(reason, "违规：工具轨迹命中危险命令黑名单");
            }
            other => panic!("expected Halted, got {:?}", other),
        }
    }

    // ===== enforce 强制原语测试（回归验证）=====

    fn enforce_rule(domain: JsonValue, reason: Option<&str>) -> JsonValue {
        let mut params = vec![("domain", domain)];
        if let Some(r) = reason {
            params.push(("reason", JsonValue::string(r)));
        }
        JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("enforce")),
            ("params", JsonValue::object_from_pairs(&params)),
        ])
    }

    #[test]
    fn test_execute_transition_enforce_halts() {
        // 规则序列：set(改 x) → enforce(命中) → set(永不执行)
        // 预期：Halted{rule_index:1}；后续规则不执行（半成品不返回，
        // payload 丢弃由构造保证——Halted 不携带状态）
        let instruction = make_instruction("delete_all", &[]);
        let payload = make_payload(1);
        let core_eval = vec![
            JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("set")),
                (
                    "params",
                    JsonValue::object_from_pairs(&[
                        ("attr", JsonValue::string("x")),
                        ("operation", JsonValue::string("set")),
                        ("value", JsonValue::Integer(666)),
                    ]),
                ),
            ]),
            enforce_rule(
                JsonValue::object_from_pairs(&[
                    ("type", JsonValue::string("instruction")),
                    ("instruction_type", JsonValue::string("delete_all")),
                ]),
                Some("违规：禁删数据集"),
            ),
            JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("set")),
                (
                    "params",
                    JsonValue::object_from_pairs(&[
                        ("attr", JsonValue::string("x")),
                        ("operation", JsonValue::string("set")),
                        ("value", JsonValue::Integer(777)),
                    ]),
                ),
            ]),
        ];

        match execute_transition(&core_eval, &instruction, &payload, &[]).unwrap() {
            TransitionResult::Halted { rule_index, reason } => {
                assert_eq!(rule_index, 1);
                assert_eq!(reason, "违规：禁删数据集");
            }
            other => panic!("expected Halted, got {:?}", other),
        }
    }

    // ===== 约束前置门回归（2026-09-23 P0：L2 enforce 被 L1 IoRequired 遮蔽）=====

    /// L1 风格规则：`call_external` → `io_request`（命中即产生 IoRequired 早退）
    fn call_external_io_rule() -> JsonValue {
        JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("instruction")),
                            ("instruction_type", JsonValue::string("call_external")),
                        ]),
                    ),
                    (
                        "on_true",
                        JsonValue::array(vec![JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("io_request")),
                            (
                                "params",
                                JsonValue::object_from_pairs(&[(
                                    "io_type",
                                    JsonValue::string("call_external"),
                                )]),
                            ),
                        ])]),
                    ),
                ]),
            ),
        ])
    }

    /// L2 风格规则：模型白名单 `enforce`（顶层 `type=enforce`，治理链晋升写入）
    fn model_allowlist_rule(banned: &str) -> JsonValue {
        enforce_rule(
            JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("all")),
                (
                    "inner",
                    JsonValue::array(vec![
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("instruction")),
                            ("instruction_type", JsonValue::string("call_external")),
                        ]),
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("eq")),
                            ("path", JsonValue::string("instruction.params.model")),
                            ("value", JsonValue::string(banned)),
                        ]),
                    ]),
                ),
            ]),
            Some("模型不在白名单"),
        )
    }

    #[test]
    fn test_enforce_not_shadowed_by_earlier_io_required() {
        // 修复前：L1（branch→io_request）在 index 0 命中 → IoRequired 立即返回，
        // index 1 的 enforce 永不求值 → 违规指令拿到 IoRequest 而非被拦
        // （守卫静默失效：无 Violation 事实、调用方视为正常）。
        // 修复后：约束门前置求值 → Halted{rule_index:1}。
        let instruction = make_instruction(
            "call_external",
            &[("model", JsonValue::string("banned-model"))],
        );
        let payload = make_payload(0);
        let core_eval = vec![
            call_external_io_rule(),
            model_allowlist_rule("banned-model"),
        ];

        match execute_transition(&core_eval, &instruction, &payload, &[]).unwrap() {
            TransitionResult::Halted { rule_index, reason } => {
                assert_eq!(rule_index, 1, "rule_index 必须仍是合并列表中的原始下标");
                assert_eq!(reason, "模型不在白名单");
            }
            other => panic!("expected Halted（约束门前置）, got {:?}", other),
        }
    }

    #[test]
    fn test_enforce_pass_through_preserves_io_required() {
        // 反向回归：enforce 未命中时 IO 路由语义不变（仍为 IoRequired），
        // 证明修复只改变"违规"路径，不侵扰正常路径。
        let instruction = make_instruction(
            "call_external",
            &[("model", JsonValue::string("allowed-model"))],
        );
        let payload = make_payload(0);
        let core_eval = vec![
            call_external_io_rule(),
            model_allowlist_rule("banned-model"),
        ];

        match execute_transition(&core_eval, &instruction, &payload, &[]).unwrap() {
            TransitionResult::IoRequired { io_type, .. } => {
                assert_eq!(io_type, "call_external");
            }
            other => panic!("expected IoRequired, got {:?}", other),
        }
    }

    #[test]
    fn test_enforce_judged_on_pre_transform_input_state() {
        // 语义变更断言：约束判定基于**转换前输入状态**，前序 set 改写的 payload
        // 不得进入 enforce 判定——否则"调整规则顺序"即可操纵约束是否被满足。
        // 规则：set(x=1) → enforce(eq payload.x == 1 → 违规)
        // 修复前（enforce 在 set 之后、基于半成品求值）→ 命中 Halted；
        // 修复后（约束门基于输入状态 x=0）→ 不命中，正常收敛。
        let instruction = make_instruction("noop", &[]);
        let payload = make_payload(0);
        let core_eval = vec![
            JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("set")),
                (
                    "params",
                    JsonValue::object_from_pairs(&[
                        ("attr", JsonValue::string("x")),
                        ("operation", JsonValue::string("set")),
                        ("value", JsonValue::Integer(1)),
                    ]),
                ),
            ]),
            enforce_rule(
                JsonValue::object_from_pairs(&[
                    ("type", JsonValue::string("eq")),
                    ("path", JsonValue::string("payload.x")),
                    ("value", JsonValue::Integer(1)),
                ]),
                Some("x 被置 1"),
            ),
        ];

        match execute_transition(&core_eval, &instruction, &payload, &[]).unwrap() {
            TransitionResult::State {
                new_payload,
                rule_hits,
                ..
            } => {
                assert_eq!(new_payload.get("x").and_then(|v| v.as_i64()), Some(1));
                // 归因口径不变：按下标升序、与输入列表等长
                assert_eq!(rule_hits.len(), 2);
                assert_eq!(rule_hits[0].index, 0);
                assert_eq!(rule_hits[1].index, 1);
            }
            other => panic!("expected State（约束基于输入状态求值）, got {:?}", other),
        }
    }

    #[test]
    fn test_execute_transition_enforce_domain_false_continues() {
        // enforce domain 求值为假 → noop 继续，后续规则正常生效
        let instruction = make_instruction("increment", &[("delta", JsonValue::Integer(1))]);
        let payload = make_payload(5);
        let core_eval = vec![
            enforce_rule(
                JsonValue::object_from_pairs(&[
                    ("type", JsonValue::string("eq")),
                    ("path", JsonValue::string("payload.forbidden_mode")),
                    ("value", JsonValue::Bool(true)),
                ]),
                Some("禁用模式"),
            ),
            JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("branch")),
                (
                    "params",
                    JsonValue::object_from_pairs(&[
                        (
                            "domain",
                            JsonValue::object_from_pairs(&[
                                ("type", JsonValue::string("instruction")),
                                ("instruction_type", JsonValue::string("increment")),
                            ]),
                        ),
                        (
                            "on_true",
                            JsonValue::array(vec![JsonValue::object_from_pairs(&[
                                ("type", JsonValue::string("set")),
                                (
                                    "params",
                                    JsonValue::object_from_pairs(&[
                                        ("attr", JsonValue::string("x")),
                                        ("operation", JsonValue::string("add")),
                                        ("value", JsonValue::Integer(1)),
                                    ]),
                                ),
                            ])]),
                        ),
                    ]),
                ),
            ]),
        ];

        match execute_transition(&core_eval, &instruction, &payload, &[]).unwrap() {
            TransitionResult::State { new_payload, .. } => {
                assert_eq!(new_payload.get("x"), Some(&JsonValue::Integer(6)));
            }
            other => panic!("expected State, got {:?}", other),
        }
    }

    #[test]
    fn test_execute_transition_enforce_missing_reason_errors() {
        // domain 命中但缺 reason：结构错误显式报错（不允许静默强制）
        let instruction = make_instruction("delete_all", &[]);
        let core_eval = vec![enforce_rule(
            JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("instruction")),
                ("instruction_type", JsonValue::string("delete_all")),
            ]),
            None,
        )];

        let result = execute_transition(&core_eval, &instruction, &make_payload(0), &[]);
        assert!(
            matches!(&result, Err(TcbError::MissingField { field }) if field == "reason"),
            "expected MissingField(reason), got {:?}",
            result
        );
    }

    #[test]
    fn test_execute_transition_enforce_structural_domain_error() {
        // 未知域类型 = 规则结构错误：fail-fast 显式报错（同 branch 语义）
        let instruction = make_instruction("delete_all", &[]);
        let core_eval = vec![enforce_rule(
            JsonValue::object_from_pairs(&[("type", JsonValue::string("nonexistent_domain"))]),
            Some("r"),
        )];

        let result = execute_transition(&core_eval, &instruction, &make_payload(0), &[]);
        assert!(
            matches!(&result, Err(TcbError::UnknownDomainType { .. })),
            "expected UnknownDomainType, got {:?}",
            result
        );
    }

    // ===== 域判定归因透传（专项-20261001 方案 2' v4，R1/R2 纪律）=====

    #[test]
    fn test_execute_transition_branch_missing_projects_false_with_attr() {
        // eq 域路径缺失 + 缺省（on_missing 未声明 = unsat 兼容缺省）：
        // 走向 = 投影 false → on_false（R1：归因不回灌执行）；
        // 归因 = Missing(PathNotFound) 透传出 RuleHit（审计「为什么走 on_false」）
        let instruction = make_instruction("noop", &[]);
        let core_eval = vec![make_instruction(
            "branch",
            &[
                (
                    "domain",
                    JsonValue::object_from_pairs(&[
                        ("type", JsonValue::string("eq")),
                        ("path", JsonValue::string("__exec__.payload.missing")),
                        ("value", JsonValue::Integer(1)),
                    ]),
                ),
                ("on_true", JsonValue::array(vec![])),
                (
                    "on_false",
                    JsonValue::array(vec![make_instruction(
                        "set",
                        &[
                            ("attr", JsonValue::string("x")),
                            ("operation", JsonValue::string("set")),
                            ("value", JsonValue::Integer(9)),
                        ],
                    )]),
                ),
            ],
        )];

        match execute_transition(&core_eval, &instruction, &make_payload(0), &[]).unwrap() {
            TransitionResult::State {
                new_payload,
                rule_hits,
                ..
            } => {
                assert_eq!(new_payload.get("x"), Some(&JsonValue::Integer(9)));
                assert_eq!(rule_hits.len(), 1);
                assert!(rule_hits[0].hit, "on_false 非空 = 结构命中");
                assert_eq!(
                    rule_hits[0].domain_attr,
                    Some(DomainAttribution {
                        outcome: DomainOutcome::Missing(MissingReason::PathNotFound),
                        on_missing: None,
                    })
                );
            }
            other => panic!("expected State, got {:?}", other),
        }
    }

    #[test]
    fn test_execute_transition_branch_missing_on_missing_error_rejects() {
        // eq 域路径缺失 + on_missing=error：拒绝执行（MissingRejected），
        // 静默通道从根上铲除
        let instruction = make_instruction("noop", &[]);
        let core_eval = vec![make_instruction(
            "branch",
            &[
                (
                    "domain",
                    JsonValue::object_from_pairs(&[
                        ("type", JsonValue::string("eq")),
                        ("path", JsonValue::string("__exec__.payload.missing")),
                        ("value", JsonValue::Integer(1)),
                        ("on_missing", JsonValue::string("error")),
                    ]),
                ),
                ("on_true", JsonValue::array(vec![])),
                ("on_false", JsonValue::array(vec![])),
            ],
        )];

        let result = execute_transition(&core_eval, &instruction, &make_payload(0), &[]);
        assert!(
            matches!(&result, Err(TcbError::MissingRejected { detail }) if detail.contains("path_not_found")),
            "expected MissingRejected(path_not_found), got {:?}",
            result
        );
    }

    #[test]
    fn test_execute_transition_enforce_missing_unsat_no_halt_with_attr() {
        // enforce 域路径缺失 + 缺省：投影 false = 不命中不拦截（不 Halted）；
        // 归因 Missing(PathNotFound) 留痕——「为什么没拦住」的审计答案
        let instruction = make_instruction("risky_op", &[]);
        let core_eval = vec![enforce_rule(
            JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("eq")),
                ("path", JsonValue::string("__exec__.payload.missing")),
                ("value", JsonValue::Integer(1)),
            ]),
            Some("blocked"),
        )];

        match execute_transition(&core_eval, &instruction, &make_payload(0), &[]).unwrap() {
            TransitionResult::State { rule_hits, .. } => {
                assert_eq!(rule_hits.len(), 1);
                assert!(!rule_hits[0].hit);
                assert_eq!(
                    rule_hits[0].domain_attr,
                    Some(DomainAttribution {
                        outcome: DomainOutcome::Missing(MissingReason::PathNotFound),
                        on_missing: None,
                    })
                );
            }
            other => panic!("expected State, got {:?}", other),
        }
    }

    #[test]
    fn test_execute_transition_domain_attr_carries_declared_on_missing() {
        // 声明值透传：on_missing="unsat" 显式声明 → 归因含声明标签；
        // Missing 场景走 on_false（声明下的 false，非静默）
        let instruction = make_instruction("noop", &[]);
        let core_eval = vec![make_instruction(
            "branch",
            &[
                (
                    "domain",
                    JsonValue::object_from_pairs(&[
                        ("type", JsonValue::string("eq")),
                        ("path", JsonValue::string("__exec__.payload.missing")),
                        ("value", JsonValue::Integer(1)),
                        ("on_missing", JsonValue::string("unsat")),
                    ]),
                ),
                ("on_true", JsonValue::array(vec![])),
                ("on_false", JsonValue::array(vec![])),
            ],
        )];

        match execute_transition(&core_eval, &instruction, &make_payload(0), &[]).unwrap() {
            TransitionResult::State { rule_hits, .. } => {
                assert_eq!(rule_hits.len(), 1);
                assert_eq!(
                    rule_hits[0].domain_attr,
                    Some(DomainAttribution {
                        outcome: DomainOutcome::Missing(MissingReason::PathNotFound),
                        on_missing: Some("unsat"),
                    })
                );
            }
            other => panic!("expected State, got {:?}", other),
        }
    }

    #[test]
    fn test_execute_transition_enforce_missing_on_missing_error_rejects() {
        // enforce 域路径缺失 + on_missing=error：拒绝执行（MissingRejected）
        let instruction = make_instruction("risky_op", &[]);
        let core_eval = vec![enforce_rule(
            JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("eq")),
                ("path", JsonValue::string("__exec__.payload.missing")),
                ("value", JsonValue::Integer(1)),
                ("on_missing", JsonValue::string("error")),
            ]),
            Some("blocked"),
        )];

        let result = execute_transition(&core_eval, &instruction, &make_payload(0), &[]);
        assert!(
            matches!(&result, Err(TcbError::MissingRejected { detail }) if detail.contains("path_not_found")),
            "expected MissingRejected(path_not_found), got {:?}",
            result
        );
    }

    #[test]
    fn test_execute_transition_enforce_sat_halted_rule_hits_not_delivered() {
        // R2 半成品纪律：enforce 命中（Halted）不携带 rule_hits——
        // 归因不随半成品交付（返回变体层面无 rule_hits 字段，此处验证外层口径）
        let instruction = make_instruction("risky_op", &[]);
        let core_eval = vec![enforce_rule(
            JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("eq")),
                ("path", JsonValue::string("__exec__.payload.x")),
                ("value", JsonValue::Integer(1)),
            ]),
            Some("blocked"),
        )];

        match execute_transition(&core_eval, &instruction, &make_payload(1), &[]).unwrap() {
            TransitionResult::Halted { rule_index, reason } => {
                assert_eq!(rule_index, 0);
                assert_eq!(reason, "blocked");
            }
            other => panic!("expected Halted, got {:?}", other),
        }
    }

    #[test]
    fn test_execute_transition_non_domain_rule_has_no_domain_attr() {
        // 非域消费指令（set）：domain_attr = None（branch/enforce 专属归因）
        let instruction = make_instruction("set", &[]);
        let core_eval = vec![make_instruction(
            "set",
            &[
                ("attr", JsonValue::string("x")),
                ("operation", JsonValue::string("set")),
                ("value", JsonValue::Integer(1)),
            ],
        )];

        match execute_transition(&core_eval, &instruction, &make_payload(0), &[]).unwrap() {
            TransitionResult::State { rule_hits, .. } => {
                assert_eq!(rule_hits.len(), 1);
                assert!(rule_hits[0].hit);
                assert_eq!(rule_hits[0].domain_attr, None);
            }
            other => panic!("expected State, got {:?}", other),
        }
    }

    #[test]
    fn test_execute_transition_enforce_in_branch_propagates() {
        // branch 子指令内的 enforce 命中：传播即停，中断整个转换
        let instruction = make_instruction("risky_op", &[]);
        let core_eval = vec![JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("instruction")),
                            ("instruction_type", JsonValue::string("risky_op")),
                        ]),
                    ),
                    (
                        "on_true",
                        JsonValue::array(vec![
                            // 子指令 set 先改 payload，随后 enforce 命中——
                            // 半成品丢弃，整体 Halted
                            JsonValue::object_from_pairs(&[
                                ("type", JsonValue::string("set")),
                                (
                                    "params",
                                    JsonValue::object_from_pairs(&[
                                        ("attr", JsonValue::string("x")),
                                        ("operation", JsonValue::string("set")),
                                        ("value", JsonValue::Integer(1)),
                                    ]),
                                ),
                            ]),
                            enforce_rule(
                                JsonValue::object_from_pairs(&[
                                    ("type", JsonValue::string("eq")),
                                    ("path", JsonValue::string("payload.x")),
                                    ("value", JsonValue::Integer(1)),
                                ]),
                                Some("子指令拦截"),
                            ),
                        ]),
                    ),
                ]),
            ),
        ])];

        match execute_transition(&core_eval, &instruction, &make_payload(0), &[]).unwrap() {
            TransitionResult::Halted { rule_index, reason } => {
                assert_eq!(rule_index, 0);
                assert_eq!(reason, "子指令拦截");
            }
            other => panic!("expected Halted, got {:?}", other),
        }
    }

    #[test]
    fn test_execute_transition_enforce_deterministic() {
        // 重放确定性：相同输入 → 相同 Halted（纯函数性质）
        let instruction = make_instruction("delete_all", &[]);
        let payload = make_payload(1);
        let core_eval = vec![enforce_rule(
            JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("instruction")),
                ("instruction_type", JsonValue::string("delete_all")),
            ]),
            Some("r"),
        )];

        let a = execute_transition(&core_eval, &instruction, &payload, &[]).unwrap();
        let b = execute_transition(&core_eval, &instruction, &payload, &[]).unwrap();
        assert_eq!(a, b);
    }

    // ===== 端到端测试 =====

    #[test]
    fn test_end_to_end_call_llm_flow() {
        let instruction = make_instruction(
            "call_llm",
            &[("prompt", JsonValue::string("Summarize this text"))],
        );
        let core_eval = call_llm_core_eval();

        // === 第一阶段：首次执行，触发 io_request ===
        let payload_phase1 = make_payload(0);
        let result_phase1 =
            execute_transition(&core_eval, &instruction, &payload_phase1, &[]).unwrap();

        match &result_phase1 {
            TransitionResult::IoRequired { io_type, params } => {
                assert_eq!(io_type, "call_llm");
                assert_eq!(
                    params.get("prompt").and_then(|v| v.as_str()),
                    Some("Summarize this text")
                );
            }
            _ => panic!("phase 1: expected IoRequired"),
        }

        // === 第二阶段：模拟反应器注入 __io_result__ 后重新执行 ===
        let payload_phase2 = JsonValue::object_from_pairs(&[
            ("x", JsonValue::Integer(0)),
            ("__io_result__", JsonValue::string("Summary: Hello world")),
        ]);

        let result_phase2 =
            execute_transition(&core_eval, &instruction, &payload_phase2, &[]).unwrap();

        match result_phase2 {
            TransitionResult::State { new_payload, .. } => {
                assert_eq!(
                    new_payload.get("llm_response").and_then(|v| v.as_str()),
                    Some("Summary: Hello world")
                );
            }
            _ => panic!("phase 2: expected State"),
        }
    }

    #[test]
    fn test_execute_transition_set_with_subtraction() {
        let instruction = make_instruction("decrement", &[("delta", JsonValue::Integer(3))]);
        let payload = make_payload(10);

        let core_eval = vec![JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("instruction")),
                            ("instruction_type", JsonValue::string("decrement")),
                        ]),
                    ),
                    (
                        "on_true",
                        JsonValue::array(vec![JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("set")),
                            (
                                "params",
                                JsonValue::object_from_pairs(&[
                                    ("attr", JsonValue::string("x")),
                                    ("operation", JsonValue::string("sub")),
                                    (
                                        "value",
                                        JsonValue::string("__exec__.instruction.params.delta"),
                                    ),
                                ]),
                            ),
                        ])]),
                    ),
                ]),
            ),
        ])];

        let result = execute_transition(&core_eval, &instruction, &payload, &[]).unwrap();
        match result {
            TransitionResult::State { new_payload, .. } => {
                assert_eq!(new_payload.get("x"), Some(&JsonValue::Integer(7)));
            }
            _ => panic!("expected State"),
        }
    }

    // ===== MAX_TRANSFORM_RULES 测试 =====

    #[test]
    fn test_execute_transition_rejects_too_many_transform_rules() {
        let instruction = make_instruction("noop", &[]);
        let payload = make_payload(0);

        let catch_all = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("all")),
                            ("inner", JsonValue::empty_array()),
                        ]),
                    ),
                    ("on_true", JsonValue::array(vec![])),
                ]),
            ),
        ]);

        let core_eval: Vec<JsonValue> = (0..=MAX_TRANSFORM_RULES)
            .map(|_| catch_all.clone())
            .collect();

        assert!(core_eval.len() > MAX_TRANSFORM_RULES);

        let result = execute_transition(&core_eval, &instruction, &payload, &[]);
        assert!(matches!(
            result,
            Err(TcbError::TooManyTransformRules { .. })
        ));
    }

    #[test]
    fn test_execute_transition_accepts_exactly_max_transform_rules() {
        let instruction = make_instruction("noop", &[]);
        let payload = make_payload(42);

        let catch_all = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("all")),
                            ("inner", JsonValue::empty_array()),
                        ]),
                    ),
                    ("on_true", JsonValue::array(vec![])),
                ]),
            ),
        ]);

        let core_eval: Vec<JsonValue> = (0..MAX_TRANSFORM_RULES)
            .map(|_| catch_all.clone())
            .collect();

        assert_eq!(core_eval.len(), MAX_TRANSFORM_RULES);

        let result = execute_transition(&core_eval, &instruction, &payload, &[]).unwrap();
        match result {
            TransitionResult::Ignored {
                instruction_type, ..
            } => {
                assert_eq!(instruction_type, "noop");
            }
            other => panic!(
                "max rules with only all fallback should return Ignored, got: {:?}",
                other
            ),
        }
    }

    // ===== M6 元指令执行总数预算测试（transition 层接线） =====

    #[test]
    fn test_execute_transition_branch_width_budget_enforced() {
        // 单条 branch 规则（不违反 MAX_TRANSFORM_RULES=64），
        // 但 on_true 子指令宽度爆炸：branch(1) + 1024 子指令 = 1025 > 1024 预算
        let instruction = make_instruction("noop", &[]);
        let payload = make_payload(0);

        let sub = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("attr", JsonValue::string("x")),
                    ("operation", JsonValue::string("set")),
                    ("value", JsonValue::Integer(1)),
                ]),
            ),
        ]);

        let width = MAX_TOTAL_META_INSTRUCTIONS;
        let on_true: Vec<JsonValue> = (0..width).map(|_| sub.clone()).collect();

        let branch = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("all")),
                            ("inner", JsonValue::empty_array()),
                        ]),
                    ),
                    ("on_true", JsonValue::array(on_true)),
                ]),
            ),
        ]);

        // 顶层仅 1 条规则：规则数防线无法拦截宽度爆炸，由预算防线兜底
        let core_eval = vec![branch];

        let result = execute_transition(&core_eval, &instruction, &payload, &[]);
        assert!(matches!(
            result,
            Err(TcbError::TooManyExecutedInstructions { .. })
        ));
    }

    #[test]
    fn test_execute_transition_branch_width_within_budget_succeeds() {
        // 对照组：branch(1) + 1023 子指令 = 1024 = 恰好满额预算，应成功
        let instruction = make_instruction("noop", &[]);
        let payload = make_payload(0);

        let sub = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("attr", JsonValue::string("x")),
                    ("operation", JsonValue::string("set")),
                    ("value", JsonValue::Integer(1)),
                ]),
            ),
        ]);

        let width = MAX_TOTAL_META_INSTRUCTIONS - 1; // 1023：加上 branch 自身恰好 1024
        let on_true: Vec<JsonValue> = (0..width).map(|_| sub.clone()).collect();

        let branch = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("all")),
                            ("inner", JsonValue::empty_array()),
                        ]),
                    ),
                    ("on_true", JsonValue::array(on_true)),
                ]),
            ),
        ]);

        let core_eval = vec![branch];

        let result = execute_transition(&core_eval, &instruction, &payload, &[]).unwrap();
        match result {
            TransitionResult::State { new_payload, .. } => {
                // 1023 次 set(1)，最终 x = 1
                assert_eq!(new_payload.get("x"), Some(&JsonValue::Integer(1)));
            }
            _ => panic!("expected State"),
        }
    }

    // ===== 兜底规则测试 =====

    #[test]
    fn test_catch_all_rule_matches_unknown_instruction() {
        let instruction = make_instruction("unknown", &[]);
        let payload = make_payload(42);

        let core_eval = vec![JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("all")),
                            ("inner", JsonValue::empty_array()),
                        ]),
                    ),
                    ("on_true", JsonValue::array(vec![])),
                ]),
            ),
        ])];

        let result = execute_transition(&core_eval, &instruction, &payload, &[]).unwrap();
        match result {
            TransitionResult::Ignored {
                instruction_type, ..
            } => {
                assert_eq!(instruction_type, "unknown");
            }
            other => panic!("expected Ignored, got: {:?}", other),
        }
    }

    // ===== 多轮模拟测试 =====

    #[test]
    fn test_execute_transition_multi_round_simulation() {
        let core_eval = multi_round_core_eval();

        // === 第 1 轮：执行 spawn ===
        let spawn_instr = make_instruction("spawn", &[]);
        let payload_round1 = make_payload(0);

        let result_round1 =
            execute_transition(&core_eval, &spawn_instr, &payload_round1, &[]).unwrap();

        let (payload_round2, queue_round2) = match result_round1 {
            TransitionResult::State {
                new_payload,
                new_queue,
                ..
            } => (new_payload, new_queue),
            _ => panic!("round 1: expected State"),
        };

        assert_eq!(queue_round2.len(), 2);
        assert_eq!(payload_round2.get("x"), Some(&JsonValue::Integer(0)));

        // === 第 2 轮：执行第 1 个 increment (delta=5) ===
        let instr_round2 = &queue_round2[0];
        let queue_round2_remaining = &queue_round2[1..];

        let result_round2 = execute_transition(
            &core_eval,
            instr_round2,
            &payload_round2,
            queue_round2_remaining,
        )
        .unwrap();

        let TransitionResult::State {
            new_payload: payload_round3,
            new_queue: queue_round3,
            ..
        } = result_round2
        else {
            panic!("round 2: expected State")
        };

        assert_eq!(payload_round3.get("x"), Some(&JsonValue::Integer(5)));
        assert_eq!(queue_round3.len(), 1);

        // === 第 3 轮：执行第 2 个 increment (delta=10) ===
        let instr_round3 = &queue_round3[0];
        let result_round3 =
            execute_transition(&core_eval, instr_round3, &payload_round3, &[]).unwrap();

        match result_round3 {
            TransitionResult::State {
                new_payload,
                new_queue,
                ..
            } => {
                assert_eq!(new_payload.get("x"), Some(&JsonValue::Integer(15)));
                assert!(new_queue.is_empty());
            }
            _ => panic!("round 3: expected State"),
        }
    }

    // ===== while_loop 死循环防护专项单元测试（2026-08-18 显式警告改造回归） =====

    // 辅助函数：构造 domain 规则
    fn instruction_domain(instr_type: &str) -> JsonValue {
        JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("instruction")),
            ("instruction_type", JsonValue::string(instr_type)),
        ])
    }

    fn lt_domain(path: &str, value: i64) -> JsonValue {
        JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("lt")),
            ("path", JsonValue::string(path)),
            ("value", JsonValue::Integer(value)),
        ])
    }

    fn branch_rule(
        domain: JsonValue,
        on_true: Vec<JsonValue>,
        on_false: Vec<JsonValue>,
    ) -> JsonValue {
        JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("domain", domain),
                    ("on_true", JsonValue::array(on_true)),
                    ("on_false", JsonValue::array(on_false)),
                ]),
            ),
        ])
    }

    fn push_rule(instructions: Vec<JsonValue>) -> JsonValue {
        JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("push")),
            (
                "params",
                JsonValue::object_from_pairs(&[("instructions", JsonValue::array(instructions))]),
            ),
        ])
    }

    /// 专项回归：while_loop 指令在 condition 为 false 时不应被报告为 Ignored
    ///
    /// # 背景
    ///
    /// 在添加 `TransitionResult::Ignored` 机制后，while_loop 指令在 condition 为 false 时
    /// 不会执行 body（payload/queue 无变化），但这是合法的空操作，不应被误判为"指令被忽略"。
    ///
    /// # 测试场景
    ///
    /// 1. while_loop 指令被 `rule_while_loop` 匹配（has_matching_business_rule = true）
    /// 2. condition 为 false（counter >= 3）
    /// 3. 规则的 on_false 为空（payload/queue 无变化）
    /// 4. 期望返回 State（合法空操作），而非 Ignored
    #[test]
    // 102 行: while_loop 规则构造 + on_false 验证 + 多个不变量断言, 拆函数会让 fixture
    // 上下文散落, 不利于阅读; test fixture 故意复杂
    #[allow(clippy::too_many_lines)]
    fn test_while_loop_condition_false_returns_state_not_ignored() {
        // 构造 while_loop 规则
        let rule_while_loop = branch_rule(
            instruction_domain("while_loop"),
            vec![branch_rule(
                JsonValue::object_from_pairs(&[(
                    "domain",
                    lt_domain("__exec__.payload.counter", 3),
                )])
                .get("domain")
                .cloned()
                .unwrap_or_else(|| lt_domain("__exec__.payload.counter", 3)),
                vec![push_rule(vec![
                    JsonValue::object_from_pairs(&[(
                        "__exec__.instruction.params.body",
                        JsonValue::string("body_ref"),
                    )]),
                    JsonValue::object_from_pairs(&[(
                        "__exec__.instruction",
                        JsonValue::string("instr_ref"),
                    )]),
                ])],
                vec![],
            )],
            vec![],
        );

        // 构造 increment 规则（用于 while_loop body）
        let rule_increment = branch_rule(
            instruction_domain("increment"),
            vec![JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("set")),
                (
                    "params",
                    JsonValue::object_from_pairs(&[
                        (
                            "attr",
                            JsonValue::string("__exec__.instruction.params.attr"),
                        ),
                        ("operation", JsonValue::string("add")),
                        (
                            "value",
                            JsonValue::string("__exec__.instruction.params.delta"),
                        ),
                    ]),
                ),
            ])],
            vec![],
        );

        // 构造 all 兜底规则
        let rule_catch_all = branch_rule(
            JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("all")),
                ("inner", JsonValue::array(vec![])),
            ]),
            vec![],
            vec![],
        );

        let core_eval = vec![rule_increment, rule_while_loop, rule_catch_all];

        // 真实的 while_loop 指令结构: {type: "while_loop", params: {condition: ..., body: ...}}
        let actual_while_loop_instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("while_loop")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("condition", lt_domain("__exec__.payload.counter", 3)),
                    (
                        "body",
                        make_instruction(
                            "increment",
                            &[
                                ("attr", JsonValue::string("counter")),
                                ("delta", JsonValue::Integer(1)),
                            ],
                        ),
                    ),
                ]),
            ),
        ]);

        // counter = 5，条件 counter < 3 为 false
        let payload_counter_5 = JsonValue::object_from_pairs(&[("counter", JsonValue::Integer(5))]);

        // 执行 while_loop 指令
        let result = execute_transition(
            &core_eval,
            &actual_while_loop_instr,
            &payload_counter_5,
            &[],
        )
        .expect("执行应成功");

        // 期望返回 State（合法空操作），而非 Ignored
        match result {
            TransitionResult::State {
                new_payload,
                new_queue,
                ..
            } => {
                // payload 保持不变（counter 仍为 5，因为条件为 false 不执行 body）
                assert_eq!(new_payload.get("counter"), Some(&JsonValue::Integer(5)));
                // 队列保持不变
                assert!(new_queue.is_empty());
            }
            TransitionResult::Ignored {
                instruction_type,
                reason,
                ..
            } => {
                panic!(
                    "while_loop 指令不应被标记为 Ignored (type={}, reason={})。\n\
                     这是合法的空操作：while_loop 规则匹配了此指令（instruction domain 匹配），\n\
                     但 condition 为 false 时不执行 body。",
                    instruction_type, reason
                );
            }
            _ => panic!("应返回 State 或 Ignored"),
        }
    }

    /// 专项回归：while_loop 指令在 condition 为 true 时正常执行 body
    ///
    /// # 验证点
    ///
    /// 当 condition 为 true 时，while_loop 应该 push body 和自身到队列中，
    /// 以便下一轮继续执行循环。
    #[test]
    fn test_while_loop_condition_true_executes_body() {
        // 构造 while_loop 规则
        let rule_while_loop = branch_rule(
            instruction_domain("while_loop"),
            vec![branch_rule(
                lt_domain("__exec__.payload.counter", 3),
                vec![push_rule(vec![
                    JsonValue::object_from_pairs(&[(
                        "__exec__.instruction.params.body",
                        JsonValue::string("body_ref"),
                    )]),
                    JsonValue::object_from_pairs(&[(
                        "__exec__.instruction",
                        JsonValue::string("instr_ref"),
                    )]),
                ])],
                vec![],
            )],
            vec![],
        );

        let core_eval = vec![rule_while_loop];

        // while_loop 指令：condition = counter < 3，body = increment counter
        let while_loop_instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("while_loop")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("condition", lt_domain("__exec__.payload.counter", 3)),
                    (
                        "body",
                        make_instruction(
                            "increment",
                            &[
                                ("attr", JsonValue::string("counter")),
                                ("delta", JsonValue::Integer(1)),
                            ],
                        ),
                    ),
                ]),
            ),
        ]);

        // counter = 0，条件 counter < 3 为 true
        let payload_counter_0 = JsonValue::object_from_pairs(&[("counter", JsonValue::Integer(0))]);

        let result = execute_transition(&core_eval, &while_loop_instr, &payload_counter_0, &[])
            .expect("执行应成功");

        // 期望返回 State，且队列中有 body 和自身
        match result {
            TransitionResult::State { new_queue, .. } => {
                // 队列中应有 2 个指令：body 和 while_loop 自身
                assert_eq!(
                    new_queue.len(),
                    2,
                    "while_loop condition 为 true 时应 push body 和自身到队列"
                );
            }
            _ => panic!("while_loop condition 为 true 时应返回 State 并 push 指令"),
        }
    }

    /// 专项回归：while_loop 指令无匹配规则时应被报告为 Ignored
    ///
    /// # 验证点
    ///
    /// 当 core_eval 中没有匹配 while_loop 的规则时，
    /// while_loop 指令应被正确识别为 Ignored，便于审计和调试。
    #[test]
    fn test_while_loop_no_matching_rule_returns_ignored() {
        // 空的 core_eval（无规则）
        let core_eval: Vec<JsonValue> = vec![];

        // while_loop 指令
        let while_loop_instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("while_loop")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("condition", lt_domain("__exec__.payload.counter", 3)),
                    (
                        "body",
                        make_instruction(
                            "increment",
                            &[
                                ("attr", JsonValue::string("counter")),
                                ("delta", JsonValue::Integer(1)),
                            ],
                        ),
                    ),
                ]),
            ),
        ]);

        let payload = JsonValue::object_from_pairs(&[("counter", JsonValue::Integer(0))]);

        let result =
            execute_transition(&core_eval, &while_loop_instr, &payload, &[]).expect("执行应成功");

        // 期望返回 Ignored（无匹配规则）
        match result {
            TransitionResult::Ignored {
                instruction_type,
                reason,
                ..
            } => {
                assert_eq!(instruction_type, "while_loop");
                assert!(reason.contains("not matched"), "reason 应说明指令未被匹配");
            }
            TransitionResult::State { .. } => {
                panic!("无匹配规则的 while_loop 指令应返回 Ignored，而非 State");
            }
            _ => panic!("应返回 Ignored"),
        }
    }

    // ===== P0 级高风险边界测试（2026-08-18 补充） =====

    /// B3 场景：all 兜底规则不应被视为业务规则
    ///
    /// # 风险说明
    ///
    /// all([]) 是兜底机制，不应被误判为业务规则。
    /// 如果 all 兜底被误判为业务规则，真正的指令不匹配将被静默忽略。
    #[test]
    fn test_while_loop_all_fallback_rule_returns_ignored() {
        // 仅包含 all 兜底规则的 core_eval
        let rule_catch_all = branch_rule(
            JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("all")),
                ("inner", JsonValue::array(vec![])),
            ]),
            vec![],
            vec![],
        );

        let core_eval = vec![rule_catch_all];

        // while_loop 指令
        let while_loop_instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("while_loop")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("condition", lt_domain("__exec__.payload.counter", 3)),
                    (
                        "body",
                        make_instruction(
                            "increment",
                            &[
                                ("attr", JsonValue::string("counter")),
                                ("delta", JsonValue::Integer(1)),
                            ],
                        ),
                    ),
                ]),
            ),
        ]);

        let payload = JsonValue::object_from_pairs(&[("counter", JsonValue::Integer(0))]);

        let result =
            execute_transition(&core_eval, &while_loop_instr, &payload, &[]).expect("执行应成功");

        // 期望返回 Ignored（all 兜底不是业务规则）
        match result {
            TransitionResult::Ignored {
                instruction_type,
                reason,
                ..
            } => {
                assert_eq!(instruction_type, "while_loop");
                assert!(reason.contains("not matched"));
            }
            TransitionResult::State { .. } => {
                panic!(
                    "all 兜底规则不应被视为业务规则。\n\
                     如果此测试失败，说明 all 兜底被误判为业务规则，\n\
                     可能导致真正的指令不匹配被静默忽略。"
                );
            }
            _ => panic!("应返回 Ignored"),
        }
    }

    /// 语义验证：未知指令类型在"全部规则未命中 → 兜底规则无操作"下返回 Ignored
    ///
    /// # 背景
    ///
    /// 规则集为应用剧本形态的 I/O 循环组合（同名测试曾称 ReAct 宪法；
    /// 资产自 T8 迁出后，此类剧本不再属于核心仓），本用例守护的是
    /// transition 层第三态语义：所有规则都未产生效果且无副作用时返回 Ignored。
    ///
    /// # 边界澄清
    ///
    /// 元指令层未知类型走 executor 硬拒绝（UnknownMetaInstruction），
    /// 本用例针对的是指令层类型（如 query_db）未被任何 transform 规则命中的情形，
    /// 两者正交。
    ///
    /// # 验证点
    ///
    /// 未知指令类型应被正确识别为 Ignored，
    /// 即使存在动态 domain 规则的嵌套。
    #[test]
    fn test_unknown_instruction_unmatched_by_all_rules_returns_ignored() {
        // 构造简化的 I/O 循环规则集
        let rule_noop = branch_rule(instruction_domain("noop"), vec![], vec![]);

        let rule_catch_all = branch_rule(
            JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("all")),
                ("inner", JsonValue::array(vec![])),
            ]),
            vec![],
            vec![],
        );

        let rule_call_external = branch_rule(instruction_domain("call_external"), vec![], vec![]);

        let rule_call_service = branch_rule(instruction_domain("call_service"), vec![], vec![]);

        // 注意：rule_self_init 使用 all domain，不是动态 domain
        // 动态 domain 规则（exists, has_fields）嵌套在 on_true 分支中
        let core_eval = vec![
            rule_noop,
            rule_call_external,
            rule_call_service,
            rule_catch_all,
        ];

        // 未知指令类型（如旧版的 query_db）
        let unknown_instr = make_instruction("query_db", &[]);

        let payload = JsonValue::object_from_pairs(&[("x", JsonValue::Integer(42))]);

        let result =
            execute_transition(&core_eval, &unknown_instr, &payload, &[]).expect("执行应成功");

        // 验证返回 Ignored
        match result {
            TransitionResult::Ignored {
                instruction_type,
                reason,
                ..
            } => {
                assert_eq!(instruction_type, "query_db");
                assert!(reason.contains("constitution rule"));
            }
            TransitionResult::State { .. } => {
                panic!(
                    "未知指令类型应返回 Ignored，而非 State。\n\
                     这说明 Ignored 检测逻辑可能有问题——\n\
                     请检查是否有动态 domain 规则在顶层被误判为通用规则。"
                );
            }
            _ => panic!("应返回 Ignored 或 State"),
        }
    }

    /// B4 场景：动态 domain 规则（lt/eq）视为通用规则，可处理任何指令
    ///
    /// # 规则分类（docs/rule_taxonomy.md）
    ///
    /// 动态 domain（eq, lt, gt 等）基于 payload 内容决策，
    /// 视为通用规则，可处理任何指令。
    ///
    /// # 验证点
    ///
    /// lt 规则是通用规则，可以处理 while_loop 指令，
    /// 但由于 while_loop 指令本身没有副作用（只是条件检查），
    /// 如果 lt 条件不满足，可能会返回 State（合法空操作）。
    #[test]
    fn test_while_loop_dynamic_domain_rule_returns_state() {
        // 仅包含 lt 规则的 core_eval（动态 domain，通用规则）
        let rule_lt = branch_rule(lt_domain("__exec__.payload.counter", 3), vec![], vec![]);

        let core_eval = vec![rule_lt];

        // while_loop 指令
        let while_loop_instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("while_loop")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("condition", lt_domain("__exec__.payload.counter", 3)),
                    (
                        "body",
                        make_instruction(
                            "increment",
                            &[
                                ("attr", JsonValue::string("counter")),
                                ("delta", JsonValue::Integer(1)),
                            ],
                        ),
                    ),
                ]),
            ),
        ]);

        // counter = 0，lt 条件满足，但 on_true 为空，无副作用
        let payload = JsonValue::object_from_pairs(&[("counter", JsonValue::Integer(0))]);

        let result =
            execute_transition(&core_eval, &while_loop_instr, &payload, &[]).expect("执行应成功");

        // 动态 domain 规则是通用规则，可以处理 while_loop 指令
        match result {
            TransitionResult::State { .. } => {
                // ✅ 正确：动态 domain 规则是通用规则，匹配 while_loop 指令
                // 即使没有副作用，也应该返回 State（规则匹配了）
            }
            TransitionResult::Ignored { .. } => {
                panic!(
                    "动态 domain 规则（lt）是通用规则，应匹配 while_loop 指令。\n\
                     如果此测试失败，说明动态 domain 规则的匹配逻辑有问题。"
                );
            }
            _ => panic!("应返回 State 或 Ignored"),
        }
    }

    /// B5 场景：直接规则（set）不匹配 while_loop 指令，但会产生副作用
    ///
    /// # 规则分类（docs/rule_taxonomy.md）
    ///
    /// 直接规则（set, increment 等）只有在 rule_type 匹配 instruction_type 时
    /// 才视为宪法规则匹配。
    ///
    /// # 验证点
    ///
    /// set 规则不匹配 while_loop 指令，但会改变 payload（副作用），
    /// 因此返回 State（而非 Ignored）。这是正确的行为，因为确实发生了副作用。
    #[test]
    fn test_while_loop_direct_rule_mismatch_has_side_effect() {
        // 仅包含 set 直接规则的 core_eval（无 domain）
        let rule_set = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("attr", JsonValue::string("counter")),
                    ("operation", JsonValue::string("set")),
                    ("value", JsonValue::Integer(10)),
                ]),
            ),
        ]);

        let core_eval = vec![rule_set];

        // while_loop 指令
        let while_loop_instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("while_loop")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("condition", lt_domain("__exec__.payload.counter", 3)),
                    (
                        "body",
                        make_instruction(
                            "increment",
                            &[
                                ("attr", JsonValue::string("counter")),
                                ("delta", JsonValue::Integer(1)),
                            ],
                        ),
                    ),
                ]),
            ),
        ]);

        let payload = JsonValue::object_from_pairs(&[("counter", JsonValue::Integer(0))]);

        let result =
            execute_transition(&core_eval, &while_loop_instr, &payload, &[]).expect("执行应成功");

        // 改进后行为：set 规则不匹配 while_loop，但会改变 payload
        match result {
            TransitionResult::State { new_payload, .. } => {
                // ✅ 正确：虽然规则不匹配，但确实改变了 payload
                assert_eq!(
                    new_payload.get("counter"),
                    Some(&JsonValue::Integer(10)),
                    "set 规则应将 counter 设置为 10"
                );
            }
            TransitionResult::Ignored { .. } => {
                panic!(
                    "set 规则改变了 payload，不应返回 Ignored。\n\
                     直接规则虽然不匹配 while_loop 指令，但确实产生了副作用。"
                );
            }
            _ => panic!("应返回 State 或 Ignored"),
        }
    }

    /// B5b 场景：直接规则匹配时应正确识别
    ///
    /// # 验证点
    ///
    /// set 直接规则匹配 set 指令时，应正确识别为宪法规则匹配。
    #[test]
    fn test_set_direct_rule_matches_set_instruction() {
        // 仅包含 set 直接规则的 core_eval
        let rule_set = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("attr", JsonValue::string("counter")),
                    ("operation", JsonValue::string("set")),
                    ("value", JsonValue::Integer(10)),
                ]),
            ),
        ]);

        let core_eval = vec![rule_set];

        // set 指令
        let set_instr = make_instruction(
            "set",
            &[
                ("attr", JsonValue::string("counter")),
                ("operation", JsonValue::string("set")),
                ("value", JsonValue::Integer(10)),
            ],
        );

        let payload = JsonValue::object_from_pairs(&[("counter", JsonValue::Integer(0))]);

        let result = execute_transition(&core_eval, &set_instr, &payload, &[]).expect("执行应成功");

        // set 规则匹配 set 指令，且产生了副作用
        match result {
            TransitionResult::State { new_payload, .. } => {
                assert_eq!(
                    new_payload.get("counter"),
                    Some(&JsonValue::Integer(10)),
                    "set 规则应正确执行"
                );
            }
            _ => panic!("应返回 State"),
        }
    }

    /// E2 场景：规则匹配但 on_true 分支为空时仍应返回 State
    ///
    /// # 风险说明
    ///
    /// 当规则匹配但执行结果为空（on_true/on_false 都为空）时，
    /// 不应误判为 Ignored。这是合法的空操作场景。
    #[test]
    fn test_while_loop_matched_rule_empty_on_true_returns_state() {
        // while_loop 规则：on_true 和 on_false 都为空
        let rule_while_loop = branch_rule(
            instruction_domain("while_loop"),
            vec![], // on_true 为空
            vec![], // on_false 为空
        );

        let core_eval = vec![rule_while_loop];

        // while_loop 指令
        let while_loop_instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("while_loop")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("condition", lt_domain("__exec__.payload.counter", 3)),
                    (
                        "body",
                        make_instruction(
                            "increment",
                            &[
                                ("attr", JsonValue::string("counter")),
                                ("delta", JsonValue::Integer(1)),
                            ],
                        ),
                    ),
                ]),
            ),
        ]);

        let payload = JsonValue::object_from_pairs(&[("counter", JsonValue::Integer(0))]);

        let result =
            execute_transition(&core_eval, &while_loop_instr, &payload, &[]).expect("执行应成功");

        // 期望返回 State（规则匹配，即使无操作也应视为合法空操作）
        match result {
            TransitionResult::State {
                new_payload,
                new_queue,
                ..
            } => {
                // payload 和 queue 保持不变
                assert_eq!(new_payload.get("counter"), Some(&JsonValue::Integer(0)));
                assert!(new_queue.is_empty());
            }
            TransitionResult::Ignored {
                instruction_type,
                reason,
                ..
            } => {
                panic!(
                    "规则匹配但 on_true 为空时不应返回 Ignored。\n\
                     instruction_type: {}, reason: {}\n\
                     这表明空操作检测逻辑需要改进：应先检查规则是否匹配，\n\
                     而非仅依赖 payload/queue 是否变化。",
                    instruction_type, reason
                );
            }
            _ => panic!("应返回 State 或 Ignored"),
        }
    }

    // ===== P1 级补充测试（B1, B2, I1） =====

    /// B1: instruction domain 指向其他指令类型（increment 规则，while_loop 指令）
    /// 验证 instruction domain 的精确匹配逻辑
    #[test]
    fn test_while_loop_instruction_domain_mismatch_returns_ignored() {
        // 规则用 instruction("increment") domain，但指令是 while_loop
        let rule_increment = branch_rule(
            instruction_domain("increment"),
            vec![JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("set")),
                (
                    "params",
                    JsonValue::object_from_pairs(&[
                        ("attr", JsonValue::string("counter")),
                        ("value", JsonValue::Integer(1)),
                    ]),
                ),
            ])],
            vec![],
        );

        let core_eval = vec![rule_increment];

        // while_loop 指令
        let while_loop_instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("while_loop")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("condition", lt_domain("__exec__.payload.counter", 3)),
                    (
                        "body",
                        make_instruction(
                            "increment",
                            &[
                                ("attr", JsonValue::string("counter")),
                                ("delta", JsonValue::Integer(1)),
                            ],
                        ),
                    ),
                ]),
            ),
        ]);

        let payload = JsonValue::object_from_pairs(&[("counter", JsonValue::Integer(0))]);

        let result =
            execute_transition(&core_eval, &while_loop_instr, &payload, &[]).expect("执行应成功");

        // 期望返回 Ignored（instruction domain 精确匹配失败）
        match result {
            TransitionResult::Ignored {
                instruction_type,
                reason,
                ..
            } => {
                assert_eq!(instruction_type, "while_loop");
                assert!(
                    reason.contains("not matched"),
                    "reason should indicate no match, got: {}",
                    reason
                );
            }
            other => panic!(
                "期望返回 Ignored（instruction domain 不匹配），实际返回: {:?}",
                other
            ),
        }
    }

    /// B2: all 域 with 非空子域返回 false
    /// 验证边界条件：all 域中有子域返回 false → on_false 执行但无副作用 → Ignored
    #[test]
    fn test_while_loop_instruction_domain_missing_type_returns_ignored() {
        // all 域中有一个子域 (counter == 100)，由于 counter=0，子域返回 false
        // 因此 all 域整体返回 false，on_false 分支被选中
        // on_false 为空，无副作用，且 all 不是业务规则 → Ignored
        let rule_all = branch_rule(
            JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("all")),
                (
                    "inner",
                    JsonValue::array(vec![JsonValue::object_from_pairs(&[
                        ("type", JsonValue::string("eq")),
                        ("path", JsonValue::string("__exec__.payload.counter")),
                        ("value", JsonValue::Integer(100)),
                    ])]),
                ),
            ]),
            vec![JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("set")),
                (
                    "params",
                    JsonValue::object_from_pairs(&[
                        ("attr", JsonValue::string("counter")),
                        ("operation", JsonValue::string("set")),
                        ("value", JsonValue::Integer(1)),
                    ]),
                ),
            ])], // on_true: 有操作但不会被执行
            vec![], // on_false: 空（无副作用）
        );

        let core_eval = vec![rule_all];

        let while_loop_instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("while_loop")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("condition", lt_domain("__exec__.payload.counter", 3)),
                    (
                        "body",
                        make_instruction(
                            "increment",
                            &[
                                ("attr", JsonValue::string("counter")),
                                ("delta", JsonValue::Integer(1)),
                            ],
                        ),
                    ),
                ]),
            ),
        ]);

        let payload = JsonValue::object_from_pairs(&[("counter", JsonValue::Integer(0))]);

        let result =
            execute_transition(&core_eval, &while_loop_instr, &payload, &[]).expect("执行应成功");

        // 期望返回 Ignored（all 子域返回 false → on_false 空 → 无副作用 → Ignored）
        match result {
            TransitionResult::Ignored {
                instruction_type,
                reason,
                ..
            } => {
                assert_eq!(instruction_type, "while_loop");
                assert!(
                    reason.contains("not matched"),
                    "reason should indicate no match, got: {}",
                    reason
                );
            }
            other => panic!(
                "期望返回 Ignored（all 子域 false → on_false 空），实际返回: {:?}",
                other
            ),
        }
    }

    /// I1: 指令无 type 字段
    /// 验证 instruction_type 回退到 "unknown" 的逻辑
    #[test]
    fn test_while_loop_instruction_missing_type_returns_ignored() {
        let rule_while_loop = branch_rule(instruction_domain("while_loop"), vec![], vec![]);

        let core_eval = vec![rule_while_loop];

        // 指令结构缺少 type 字段
        let instr_no_type = JsonValue::object_from_pairs(&[(
            "params",
            JsonValue::object_from_pairs(&[
                ("condition", lt_domain("__exec__.payload.counter", 3)),
                (
                    "body",
                    make_instruction(
                        "increment",
                        &[
                            ("attr", JsonValue::string("counter")),
                            ("delta", JsonValue::Integer(1)),
                        ],
                    ),
                ),
            ]),
        )]);

        let payload = JsonValue::object_from_pairs(&[("counter", JsonValue::Integer(0))]);

        let result =
            execute_transition(&core_eval, &instr_no_type, &payload, &[]).expect("执行应成功");

        // 期望返回 Ignored（instruction_type 回退到 "unknown"，规则无匹配）
        match result {
            TransitionResult::Ignored {
                instruction_type,
                reason,
                ..
            } => {
                assert_eq!(instruction_type, "unknown");
                assert!(
                    reason.contains("not matched"),
                    "reason should indicate no match, got: {}",
                    reason
                );
            }
            other => panic!(
                "期望返回 Ignored（指令缺少 type 字段），实际返回: {:?}",
                other
            ),
        }
    }

    // ===== P2 级补充测试（B6, I2, E1） =====

    /// B6: 混合规则场景（第一条不匹配，第二条匹配）
    /// 验证多条规则时的匹配逻辑
    #[test]
    fn test_while_loop_mixed_rules_second_matches_returns_state() {
        // 第一条规则：increment domain（不匹配 while_loop）
        let rule_increment = branch_rule(instruction_domain("increment"), vec![], vec![]);

        // 第二条规则：while_loop domain（匹配）
        let rule_while_loop = branch_rule(instruction_domain("while_loop"), vec![], vec![]);

        let core_eval = vec![rule_increment, rule_while_loop];

        let while_loop_instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("while_loop")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("condition", lt_domain("__exec__.payload.counter", 3)),
                    (
                        "body",
                        make_instruction(
                            "increment",
                            &[
                                ("attr", JsonValue::string("counter")),
                                ("delta", JsonValue::Integer(1)),
                            ],
                        ),
                    ),
                ]),
            ),
        ]);

        let payload = JsonValue::object_from_pairs(&[("counter", JsonValue::Integer(0))]);

        let result =
            execute_transition(&core_eval, &while_loop_instr, &payload, &[]).expect("执行应成功");

        // 期望返回 State（第二条规则匹配）
        match result {
            TransitionResult::State { .. } => { /* OK */ }
            other => panic!(
                "期望返回 State（混合规则中第二条匹配），实际返回: {:?}",
                other
            ),
        }
    }

    /// I2: 指令 type 为 null
    /// 验证 type 为 null 时的处理
    #[test]
    fn test_while_loop_instruction_null_type_returns_ignored() {
        let rule_while_loop = branch_rule(instruction_domain("while_loop"), vec![], vec![]);

        let core_eval = vec![rule_while_loop];

        // 指令 type 为 null
        let instr_null_type = JsonValue::object_from_pairs(&[
            ("type", JsonValue::Null),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("condition", lt_domain("__exec__.payload.counter", 3)),
                    (
                        "body",
                        make_instruction(
                            "increment",
                            &[
                                ("attr", JsonValue::string("counter")),
                                ("delta", JsonValue::Integer(1)),
                            ],
                        ),
                    ),
                ]),
            ),
        ]);

        let payload = JsonValue::object_from_pairs(&[("counter", JsonValue::Integer(0))]);

        let result =
            execute_transition(&core_eval, &instr_null_type, &payload, &[]).expect("执行应成功");

        // 期望返回 Ignored（type 为 null 时 instruction_type 回退到 "unknown"）
        match result {
            TransitionResult::Ignored {
                instruction_type,
                reason,
                ..
            } => {
                assert_eq!(instruction_type, "unknown");
                assert!(
                    reason.contains("not matched"),
                    "reason should indicate no match, got: {}",
                    reason
                );
            }
            other => panic!("期望返回 Ignored（type 为 null 时），实际返回: {:?}", other),
        }
    }

    /// E1: on_false 分支有操作
    /// 验证 on_false 分支的执行：内层 branch 检查 condition，false 时执行 on_false
    #[test]
    fn test_while_loop_on_false_executes_operations() {
        // while_loop 规则：外层匹配 while_loop 指令，内层检查 condition
        // 当 condition=false (counter >= 3) 时，执行 on_false 中的 set 操作
        let rule_while_loop = branch_rule(
            instruction_domain("while_loop"),
            vec![branch_rule(
                lt_domain("__exec__.payload.counter", 3),
                vec![], // condition=true (on_true): 空操作
                vec![JsonValue::object_from_pairs(&[
                    ("type", JsonValue::string("set")),
                    (
                        "params",
                        JsonValue::object_from_pairs(&[
                            ("attr", JsonValue::string("flag")),
                            ("operation", JsonValue::string("set")),
                            ("value", JsonValue::Integer(1)),
                        ]),
                    ),
                ])], // condition=false (on_false): 设置 flag=1
            )],
            vec![],
        );

        let core_eval = vec![rule_while_loop];

        // while_loop 指令，condition=false（counter >= 3）
        let while_loop_instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("while_loop")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("condition", lt_domain("__exec__.payload.counter", 3)),
                    (
                        "body",
                        make_instruction(
                            "increment",
                            &[
                                ("attr", JsonValue::string("counter")),
                                ("delta", JsonValue::Integer(1)),
                            ],
                        ),
                    ),
                ]),
            ),
        ]);

        // counter 初始值为 5，condition (5 < 3) = false
        let payload = JsonValue::object_from_pairs(&[
            ("counter", JsonValue::Integer(5)),
            ("flag", JsonValue::Integer(0)),
        ]);

        let result =
            execute_transition(&core_eval, &while_loop_instr, &payload, &[]).expect("执行应成功");

        // 期望返回 State（内层 branch 的 on_false 分支被执行）
        match result {
            TransitionResult::State { new_payload, .. } => {
                // on_false 中的 set 操作应被执行，flag 变为 1
                assert_eq!(
                    new_payload.get("flag"),
                    Some(&JsonValue::Integer(1)),
                    "on_false 中的 set 操作应被执行，flag 应变为 1"
                );
            }
            other => panic!(
                "期望返回 State（on_false 分支有操作），实际返回: {:?}",
                other
            ),
        }
    }

    // ===== I/O 循环组合语义端到端测试（io_request 的语言层组合回归） =====
    // 规则集为内联构造的应用剧本形态：1) 计数器自初始化；2) call_external；3) call_service。
    // 核心仓最小评估集不再携带此类剧本（T8 迁出）；此模块守护的是元指令组合的执行语义，
    // 工具扇出/结果合并等循环编排职责在应用层 runner（collect/merge 原语已退役，历史批次）。
    // 嵌套子 mod（不写 `#[cfg(test)]`，继承父 mod 的 cfg(test)，
    // build.rs L1 门禁的 strip_test_mod 会把整个 mod tests 块一起剥掉）
    mod io_loop_e2e_tests {
        use super::*;
        use alloc::vec;

        fn s(v: &str) -> JsonValue {
            JsonValue::string(v)
        }
        fn iv(v: i64) -> JsonValue {
            JsonValue::Integer(v)
        }
        fn obj(pairs: &[(&str, JsonValue)]) -> JsonValue {
            JsonValue::object_from_pairs(pairs)
        }
        fn arr(v: Vec<JsonValue>) -> JsonValue {
            JsonValue::array(v)
        }

        fn instr_domain(t: &str) -> JsonValue {
            obj(&[("type", s("instruction")), ("instruction_type", s(t))])
        }
        fn exists_domain(path: &str) -> JsonValue {
            obj(&[("type", s("exists")), ("path", s(path))])
        }
        fn branch(
            domain: JsonValue,
            on_true: Vec<JsonValue>,
            on_false: Vec<JsonValue>,
        ) -> JsonValue {
            obj(&[
                ("type", s("branch")),
                (
                    "params",
                    obj(&[
                        ("domain", domain),
                        ("on_true", arr(on_true)),
                        ("on_false", arr(on_false)),
                    ]),
                ),
            ])
        }
        fn set_instr(attr: &str, op: &str, value: JsonValue) -> JsonValue {
            obj(&[
                ("type", s("set")),
                (
                    "params",
                    obj(&[("attr", s(attr)), ("operation", s(op)), ("value", value)]),
                ),
            ])
        }
        fn push_noop() -> JsonValue {
            obj(&[
                ("type", s("push")),
                (
                    "params",
                    obj(&[("instructions", arr(vec![obj(&[("type", s("noop"))])]))]),
                ),
            ])
        }

        /// 应用剧本式 I/O 循环规则集（与 io_loop 组合语义测试配套；工具扇出与
        // 结果合并的循环编排在应用层 runner，此处只守护 io_request 请求/消费组合）
        fn io_loop_rules() -> Vec<JsonValue> {
            // 1) 计数器自初始化（缺失时置 0，否则跳过）
            let self_init = branch(
                obj(&[
                    ("type", s("all")),
                    (
                        "inner",
                        arr(vec![
                            instr_domain("call_external"),
                            obj(&[
                                ("type", s("not")),
                                ("inner", exists_domain("__exec__.payload.react_iteration")),
                            ]),
                        ]),
                    ),
                ]),
                vec![set_instr("react_iteration", "set", iv(0))],
                vec![],
            );

            // 2) call_external：消费 LLM 结果（无 tool_calls 时 push noop 终止；
            //    工具扇出循环的编排职责在应用层 runner，不再使用 collect）
            let call_external = branch(
                instr_domain("call_external"),
                vec![branch(
                    exists_domain("__exec__.payload.__io_results__.call_external"),
                    vec![
                        set_instr(
                            "llm_response",
                            "set",
                            s("__exec__.payload.__io_results__.call_external"),
                        ),
                        branch(
                            exists_domain("__exec__.instruction.params.tools"),
                            vec![set_instr(
                                "tools",
                                "set",
                                s("__exec__.instruction.params.tools"),
                            )],
                            vec![],
                        ),
                        set_instr(
                            "__exec__.payload.__io_results__.call_external",
                            "set",
                            JsonValue::null(),
                        ),
                        push_noop(),
                    ],
                    vec![obj(&[
                        ("type", s("io_request")),
                        (
                            "params",
                            obj(&[
                                ("io_type", s("call_external")),
                                ("messages", s("__exec__.instruction.params.messages")),
                                ("tools", s("__exec__.instruction.params.tools")),
                            ]),
                        ),
                    ])],
                )],
                vec![],
            );

            // 3) call_service：消费工具结果（结果落 payload 后 push noop 终止；
            //    结果合并与下一条 LLM 调用的生成由应用层 runner 编排，不再使用 merge）
            let call_service = branch(
                instr_domain("call_service"),
                vec![branch(
                    exists_domain("__exec__.payload.__io_results__.call_service"),
                    vec![
                        set_instr(
                            "service_result",
                            "set",
                            s("__exec__.payload.__io_results__.call_service"),
                        ),
                        set_instr(
                            "__exec__.payload.__io_results__.call_service",
                            "set",
                            JsonValue::null(),
                        ),
                        push_noop(),
                    ],
                    vec![obj(&[
                        ("type", s("io_request")),
                        (
                            "params",
                            obj(&[
                                ("io_type", s("call_service")),
                                (
                                    "service_name",
                                    s("__exec__.instruction.params.service_name"),
                                ),
                                ("args", s("__exec__.instruction.params.args")),
                            ]),
                        ),
                    ])],
                )],
                vec![],
            );

            vec![self_init, call_external, call_service]
        }

        fn user_messages() -> JsonValue {
            arr(vec![obj(&[
                ("role", s("user")),
                ("content", s("What's the weather?")),
            ])])
        }
        fn tools_def() -> JsonValue {
            arr(vec![obj(&[("name", s("get_weather"))])])
        }
        fn call_external_instr(messages: JsonValue, tools: JsonValue) -> JsonValue {
            obj(&[
                ("type", s("call_external")),
                ("params", obj(&[("messages", messages), ("tools", tools)])),
            ])
        }
        fn call_service_instr() -> JsonValue {
            obj(&[
                ("type", s("call_service")),
                (
                    "params",
                    obj(&[
                        ("service_name", s("get_weather")),
                        ("args", obj(&[("city", s("Beijing"))])),
                    ]),
                ),
            ])
        }

        /// 轮次 1：无 I/O 结果 → 发起第一次 call_external（LLM）请求
        #[test]
        fn test_io_loop_round1_first_llm_io_request_fires() {
            let instruction = call_external_instr(user_messages(), tools_def());
            let payload = obj(&[]); // 空 payload：react_iteration 尚未初始化

            let result = execute_transition(&io_loop_rules(), &instruction, &payload, &[]).unwrap();
            match result {
                TransitionResult::IoRequired { io_type, params } => {
                    assert_eq!(io_type, "call_external");
                    assert_eq!(params.get("messages"), Some(&user_messages()));
                    assert_eq!(params.get("tools"), Some(&tools_def()));
                }
                _ => panic!("round 1: expected IoRequired"),
            }
        }

        /// 轮次 4：上一轮 I/O 结果已用 null 清除 → exists 判定不存在 → 发起第二次 LLM 请求。
        /// 回归：修复前 null 被视为"存在"，陈旧结果被消费，第二次 LLM 调用永远无法发起。
        #[test]
        fn test_io_loop_round4_second_llm_io_request_fires_after_null_clear() {
            let payload = obj(&[
                ("react_iteration", iv(1)),
                ("llm_response", obj(&[("messages", user_messages())])),
                ("tools", tools_def()),
                ("__io_results__", obj(&[("call_external", JsonValue::Null)])),
            ]);
            let instruction = call_external_instr(user_messages(), tools_def());

            let result = execute_transition(&io_loop_rules(), &instruction, &payload, &[]).unwrap();
            match result {
                TransitionResult::IoRequired { io_type, .. } => {
                    assert_eq!(io_type, "call_external");
                }
                TransitionResult::State { new_queue, .. } => {
                    panic!(
                        "round 4: expected IoRequired, got State with queue len {}",
                        new_queue.len()
                    );
                }
                TransitionResult::Ignored { .. } => {
                    panic!("round 4: expected IoRequired, got Ignored");
                }
                TransitionResult::Halted { rule_index, reason } => {
                    panic!(
                        "round 4: expected IoRequired, got Halted(rule_index={rule_index}, reason={reason})"
                    );
                }
            }
        }

        /// 迭代上限：call_service 消费轮结果落 payload 后 push noop 终止循环（react_iteration 保持不变）
        #[test]
        fn test_io_loop_iteration_cap_terminates_with_noop() {
            let payload = obj(&[
                ("react_iteration", iv(10)),
                ("llm_response", obj(&[("messages", user_messages())])),
                (
                    "__io_results__",
                    obj(&[("call_service", obj(&[("temperature", iv(25))]))]),
                ),
            ]);

            let result =
                execute_transition(&io_loop_rules(), &call_service_instr(), &payload, &[]).unwrap();
            let TransitionResult::State {
                new_payload,
                new_queue,
                ..
            } = result
            else {
                panic!("cap: expected State")
            };

            assert_eq!(new_queue.len(), 1);
            assert_eq!(
                new_queue[0].get("type").and_then(|v| v.as_str()),
                Some("noop")
            );
            // 循环终止：计数不再增长，也无 updated_messages
            assert_eq!(new_payload.get("react_iteration"), Some(&iv(10)));
            assert!(new_payload.get("updated_messages").is_none());
        }

        /// 终止轮：LLM 最终回答不含 tool_calls → push noop，且 react_iteration 被自动初始化
        #[test]
        fn test_io_loop_final_round_without_tool_calls_terminates() {
            let final_response = obj(&[("messages", user_messages())]); // 无 tool_calls
            let payload = obj(&[
                (
                    "__io_results__",
                    obj(&[("call_external", final_response.clone())]),
                ),
                // 故意不带 react_iteration：验证自初始化规则
            ]);
            let instruction = call_external_instr(user_messages(), tools_def());

            let result = execute_transition(&io_loop_rules(), &instruction, &payload, &[]).unwrap();
            let TransitionResult::State {
                new_payload,
                new_queue,
                ..
            } = result
            else {
                panic!("final: expected State")
            };

            assert_eq!(new_queue.len(), 1);
            assert_eq!(
                new_queue[0].get("type").and_then(|v| v.as_str()),
                Some("noop")
            );
            assert_eq!(new_payload.get("llm_response"), Some(&final_response));
            // 自初始化规则生效：react_iteration 从缺失变为 0
            assert_eq!(new_payload.get("react_iteration"), Some(&iv(0)));
        }
    }
}

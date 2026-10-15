// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 前向链仿真（合成文档迭代到不动点）。
//!
//! 在合成 `__exec__` 文档上迭代施加规则效果，直到不动点 / 步数上限 /
//! I/O 断流 / enforce 短路 / 状态振荡。算法底稿 = v4 `forward.rs`（已废弃），
//! 模型层全部按现行 TCB 重写：单步 = 列表序第一条**产生状态变化**的规则
//! （现行模型无 priority，列表序即语义序）；域求值与指令执行直接调用
//! crates.io `evorule-tcb` 0.8 公开原语（`execute_meta_instruction`），
//! TCB 零改动。
//!
//! # 与运行时语义的对齐点
//!
//! - **enforce 前置门**：现行 `execute_transition` 在一切状态变换前先求值
//!   全部顶层 `enforce`（P0-005 修复）；本仿真每步扫描同样先过 enforce 门
//!   （命中即 `halted` 短路），再按列表序扫非 enforce 规则。
//! - **io 传播即停**：`io_request` 产生信号即停（与 TCB 同语义），仿真侧
//!   无 I/O 面——`blocked_on_io` 诚实断流，不假装 I/O 结果。
//! - **值变化判据**：规则"适用"= 执行后文档发生值变化（结构命中但值不变
//!   的幂等重放不推进仿真——fixpoint 判据需要值收敛）。
//!
//! # 诚实边界
//!
//! 不动点 ≠ 真实系统行为：无 I/O 响应、无并发、无治理面干预；
//! 输出带 `simulation_scope` 声明。状态振荡（文档回归历史值）显式报告，
//! 不静默当作收敛。

#![forbid(unsafe_code)]

use std::collections::BTreeSet;

use evorule_tcb::executor::{execute_meta_instruction, MetaInstructionResult};
use evorule_tcb::JsonValue;

/// 仿真步数缺省值
pub const DEFAULT_MAX_STEPS: usize = 64;
/// 仿真步数硬上限
pub const MAX_STEPS_CAP: usize = 1000;

/// 仿真终止原因之外的透传：一步内规则的执行结局（内部用）
enum StepOutcome {
    /// 文档发生值变化，采纳新状态
    Changed(JsonValue),
    /// 执行到 I/O 请求——仿真无 I/O 面，诚实断流
    BlockedOnIo { io_type: String },
    /// enforce 命中，短路
    Halted { reason: String },
    /// 执行成功但文档无值变化（幂等 / 域未命中走空分支）
    Neutral,
}

/// 仿真报告
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulationReport {
    /// 终止原因：`fixpoint` / `max_steps` / `blocked_on_io` / `halted` / `oscillation`
    pub stopped_reason: &'static str,
    /// 实际执行步数
    pub steps: usize,
    /// 每步采纳的规则身份（`"{source}#{index}"`，按执行序）
    pub applied: Vec<String>,
    /// 终态文档（`__exec__` 内层：instruction/payload/queue）
    pub final_document: JsonValue,
    /// 是否到达不动点（`stopped_reason == "fixpoint"` 的等值 Convenience）
    pub reached_fixed_point: bool,
    /// I/O 断流明细：`(规则身份, io_type)`
    pub blocked_on_io: Option<(String, String)>,
    /// enforce 短路明细：`(规则身份, reason)`
    pub halted: Option<(String, String)>,
    /// 豁免桶规则身份（宪法段默认豁免；本服务仍执行它们，仅作透明标注）
    pub excluded: Vec<String>,
}

/// 构造 `__exec__` 执行状态（复刻 TCB `build_exec_state` 形态，该函数私有）
fn wrap_exec(document: &JsonValue) -> JsonValue {
    JsonValue::object_from_pairs(&[("__exec__", document.clone())])
}

/// 对单条规则在试用状态上执行，归类结局。
///
/// # Errors
///
/// TCB 执行错误（预算 / 深度 / 非法指令树）原样上抛，附规则身份。
fn try_rule(rule: &JsonValue, state: &JsonValue, rule_id: &str) -> Result<StepOutcome, String> {
    match execute_meta_instruction(rule, state.clone(), 0) {
        Ok(MetaInstructionResult::State(new_state)) => {
            if new_state == *state {
                Ok(StepOutcome::Neutral)
            } else {
                // TCB 返回的 State 自带 `__exec__` 外壳；仿真内部状态恒为
                // 内层文档（instruction/payload/queue），采纳前剥壳防双重包装
                let inner = new_state.get("__exec__").cloned().unwrap_or(new_state);
                Ok(StepOutcome::Changed(inner))
            }
        }
        Ok(MetaInstructionResult::IoRequired { io_type, .. }) => {
            Ok(StepOutcome::BlockedOnIo { io_type })
        }
        Ok(MetaInstructionResult::Halted { reason }) => Ok(StepOutcome::Halted { reason }),
        Err(e) => Err(format!(
            "规则 {rule_id} 执行失败: {e} — 自诊断指引: \
             仿真要求全部规则在合成文档上可执行（域求值 / 路径引用解析不得报错）"
        )),
    }
}

/// 执行前向链仿真（见模块文档）。
///
/// # Errors
///
/// - 规则数超 [`crate::rw_extract::MAX_RULES`]
/// - `max_steps` 超过硬上限
/// - 任一规则 TCB 执行错误
pub fn simulate(
    rules: &[JsonValue],
    sources: &[String],
    initial_document: &JsonValue,
    max_steps: usize,
) -> Result<SimulationReport, String> {
    if rules.len() != sources.len() {
        return Err(format!(
            "rules({}) 与 sources({}) 长度不一致 — 自诊断指引: 来源标签须与规则列表等长且按合并序对齐",
            rules.len(),
            sources.len()
        ));
    }
    if rules.len() > crate::rw_extract::MAX_RULES {
        return Err(format!(
            "规则数 {} 超过上限 {} — 自诊断指引: 与 TCB MAX_TRANSFORM_RULES 同口径",
            rules.len(),
            crate::rw_extract::MAX_RULES
        ));
    }
    if max_steps > MAX_STEPS_CAP {
        return Err(format!(
            "max_steps={max_steps} 超过硬上限 {MAX_STEPS_CAP} — 自诊断指引: \
             仿真是分析工具不是执行器，步数上限防止资源滥用"
        ));
    }

    let mut current = initial_document.clone();
    let mut applied: Vec<String> = Vec::new();
    let mut visited: BTreeSet<String> = BTreeSet::from([current.to_string()]);
    let excluded: Vec<String> = sources
        .iter()
        .enumerate()
        .filter(|(_, s)| *s == "core_eval")
        .map(|(i, s)| format!("{s}#{i}"))
        .collect();

    let mut steps = 0usize;
    loop {
        // ── enforce 前置门（对齐 execute_transition P0-005 语义）──
        for (index, rule) in rules.iter().enumerate() {
            let is_enforce = rule
                .get("type")
                .and_then(JsonValue::as_str)
                .is_some_and(|t| t == "enforce");
            if !is_enforce {
                continue;
            }
            let rule_id = format!("{}#{index}", sources[index]);
            match try_rule(rule, &wrap_exec(&current), &rule_id)? {
                StepOutcome::Halted { reason } => {
                    return Ok(SimulationReport {
                        stopped_reason: "halted",
                        steps,
                        applied,
                        final_document: current,
                        reached_fixed_point: false,
                        blocked_on_io: None,
                        halted: Some((rule_id, reason)),
                        excluded,
                    });
                }
                // enforce 门内：值变化不采纳（约束不是状态变换，对齐现行语义），
                // io / neutral 同样不在此短路（io 由 transform 段如实到达）
                _ => continue,
            }
        }

        // 步数上限检查（enforce 门之后，保证被 halt 的仿真不算步数）
        if steps >= max_steps {
            return Ok(SimulationReport {
                stopped_reason: "max_steps",
                steps,
                applied,
                final_document: current,
                reached_fixed_point: false,
                blocked_on_io: None,
                halted: None,
                excluded,
            });
        }

        // ── transform 段：列表序扫第一条产生值变化的规则 ──
        let mut scan_io: Option<(String, String)> = None;
        let mut adopted: Option<JsonValue> = None;
        for (index, rule) in rules.iter().enumerate() {
            let rule_id = format!("{}#{index}", sources[index]);
            match try_rule(rule, &wrap_exec(&current), &rule_id)? {
                StepOutcome::Changed(new_state) => {
                    adopted = Some(new_state);
                    applied.push(rule_id);
                    break;
                }
                StepOutcome::BlockedOnIo { io_type } => {
                    scan_io = Some((rule_id, io_type));
                    break;
                }
                StepOutcome::Halted { reason } => {
                    // 嵌套在 branch 内的 enforce 走这里（顶层 enforce 已在门内拦）
                    return Ok(SimulationReport {
                        stopped_reason: "halted",
                        steps,
                        applied,
                        final_document: current,
                        reached_fixed_point: false,
                        blocked_on_io: None,
                        halted: Some((rule_id, reason)),
                        excluded,
                    });
                }
                StepOutcome::Neutral => continue,
            }
        }

        match (adopted, scan_io) {
            (Some(new_state), _) => {
                steps += 1;
                let key = new_state.to_string();
                if visited.contains(&key) {
                    return Ok(SimulationReport {
                        stopped_reason: "oscillation",
                        steps,
                        applied,
                        final_document: new_state,
                        reached_fixed_point: false,
                        blocked_on_io: None,
                        halted: None,
                        excluded,
                    });
                }
                visited.insert(key);
                current = new_state;
            }
            (None, Some((rule_id, io_type))) => {
                return Ok(SimulationReport {
                    stopped_reason: "blocked_on_io",
                    steps,
                    applied,
                    final_document: current,
                    reached_fixed_point: false,
                    blocked_on_io: Some((rule_id, io_type)),
                    halted: None,
                    excluded,
                });
            }
            (None, None) => {
                return Ok(SimulationReport {
                    stopped_reason: "fixpoint",
                    steps,
                    applied,
                    final_document: current,
                    reached_fixed_point: true,
                    blocked_on_io: None,
                    halted: None,
                    excluded,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn obj(pairs: &[(&str, JsonValue)]) -> JsonValue {
        JsonValue::object_from_pairs(pairs)
    }

    fn set_rule(attr: &str, value: i64) -> JsonValue {
        obj(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                obj(&[
                    ("attr", JsonValue::string(attr)),
                    ("operation", JsonValue::string("set")),
                    ("value", JsonValue::Integer(value)),
                ]),
            ),
        ])
    }

    /// branch(exists 域) → on_true[set]
    fn cond_set(path: &str, attr: &str, value: i64) -> JsonValue {
        obj(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                obj(&[
                    (
                        "domain",
                        obj(&[
                            ("type", JsonValue::string("exists")),
                            ("path", JsonValue::string(path)),
                        ]),
                    ),
                    ("on_true", JsonValue::Array(vec![set_rule(attr, value)])),
                ]),
            ),
        ])
    }

    fn doc(payload_pairs: &[(&str, JsonValue)]) -> JsonValue {
        obj(&[
            ("instruction", obj(&[("type", JsonValue::string("test"))])),
            ("payload", obj(payload_pairs)),
            ("queue", JsonValue::Array(vec![])),
        ])
    }

    fn run(rules: &[JsonValue], doc: JsonValue, max_steps: usize) -> SimulationReport {
        let sources: Vec<String> = (0..rules.len())
            .map(|i| format!("rules/r{i}.json"))
            .collect();
        simulate(rules, &sources, &doc, max_steps).unwrap()
    }

    #[test]
    fn unconditional_set_reaches_fixpoint_in_one_step() {
        let rules = vec![set_rule("x", 42)];
        let report = run(&rules, doc(&[("x", JsonValue::Integer(0))]), 64);
        assert_eq!(report.stopped_reason, "fixpoint");
        assert!(report.reached_fixed_point);
        assert_eq!(report.steps, 1);
        assert_eq!(report.applied, vec!["rules/r0.json#0".to_string()]);
        assert_eq!(
            report
                .final_document
                .get("payload")
                .and_then(|p| p.get("x"))
                .and_then(JsonValue::as_i64),
            Some(42)
        );
    }

    #[test]
    fn idempotent_rule_is_neutral_immediate_fixpoint() {
        // set 同值：结构命中但值不变 → 不推进 → 0 步 fixpoint
        let rules = vec![set_rule("x", 7)];
        let report = run(&rules, doc(&[("x", JsonValue::Integer(7))]), 64);
        assert_eq!(report.stopped_reason, "fixpoint");
        assert_eq!(report.steps, 0);
        assert!(report.applied.is_empty());
    }

    #[test]
    fn conditional_chain_fires_then_fixpoints() {
        // r0: payload.flag 存在 → set payload.y=1；r1: payload.y 存在 → set payload.z=2
        let rules = vec![
            cond_set("__exec__.payload.flag", "y", 1),
            cond_set("__exec__.payload.y", "z", 2),
        ];
        let report = run(&rules, doc(&[("flag", JsonValue::Bool(true))]), 64);
        assert!(report.reached_fixed_point);
        assert_eq!(report.steps, 2);
        assert_eq!(
            report.applied,
            vec!["rules/r0.json#0".to_string(), "rules/r1.json#1".to_string()]
        );
    }

    #[test]
    fn oscillation_detected_not_silent() {
        // 自指 toggle 不可用纯 set 表达（set 只写常量），用两规则互写构造振荡：
        // r0: payload.a 存在 → 写 b=1（a 仍在）→ r1 无条件写 a=0 → a 消失条件仍成立?
        // 简化：increment 型振荡用 queue push 不可行——直接验证 visited 检测路径：
        // 规则 1 无条件 set x=1；规则 2 无条件 set x=0 → 循环振荡
        let rules = vec![set_rule("x", 1), set_rule("x", 0)];
        let report = run(&rules, doc(&[("x", JsonValue::Integer(0))]), 100);
        assert_eq!(report.stopped_reason, "oscillation");
        assert!(!report.reached_fixed_point);
    }

    #[test]
    fn max_steps_stops_simulation() {
        // increment 型无界步进：状态每次都新 → 振荡检测不触发 → 走到步数上限
        let rules = vec![obj(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                obj(&[
                    ("attr", JsonValue::string("counter")),
                    ("operation", JsonValue::string("add")),
                    ("value", JsonValue::Integer(1)),
                ]),
            ),
        ])];
        let report = run(&rules, doc(&[("counter", JsonValue::Integer(0))]), 5);
        assert_eq!(report.stopped_reason, "max_steps");
        assert_eq!(report.steps, 5);
        assert_eq!(
            report
                .final_document
                .get("payload")
                .and_then(|p| p.get("counter"))
                .and_then(JsonValue::as_i64),
            Some(5)
        );
    }

    #[test]
    fn io_request_blocks_honestly() {
        let rules = vec![obj(&[
            ("type", JsonValue::string("io_request")),
            (
                "params",
                obj(&[
                    ("io_type", JsonValue::string("call_service")),
                    ("params", obj(&[("service_name", JsonValue::string("svc"))])),
                ]),
            ),
        ])];
        let report = run(&rules, doc(&[]), 64);
        assert_eq!(report.stopped_reason, "blocked_on_io");
        let (id, io) = report.blocked_on_io.expect("io 断流明细");
        assert_eq!(id, "rules/r0.json#0");
        assert_eq!(io, "call_service");
    }

    #[test]
    fn top_level_enforce_halts_before_transforms() {
        let rules = vec![
            set_rule("x", 1),
            obj(&[
                ("type", JsonValue::string("enforce")),
                (
                    "params",
                    obj(&[
                        (
                            "domain",
                            obj(&[
                                ("type", JsonValue::string("eq")),
                                ("path", JsonValue::string("__exec__.payload.x")),
                                ("value", JsonValue::Integer(0)),
                            ]),
                        ),
                        ("reason", JsonValue::string("x must stay 0")),
                    ]),
                ),
            ]),
        ];
        let report = run(&rules, doc(&[("x", JsonValue::Integer(0))]), 64);
        assert_eq!(report.stopped_reason, "halted");
        let (id, reason) = report.halted.expect("halt 明细");
        assert_eq!(id, "rules/r1.json#1");
        assert_eq!(reason, "x must stay 0");
        // enforce 短路发生在 set 之前 → x 未被改
        assert_eq!(
            report
                .final_document
                .get("payload")
                .and_then(|p| p.get("x"))
                .and_then(JsonValue::as_i64),
            Some(0)
        );
    }

    #[test]
    fn determinism_two_runs_bit_identical() {
        let rules = vec![
            cond_set("__exec__.payload.flag", "y", 1),
            cond_set("__exec__.payload.y", "z", 2),
        ];
        let d = doc(&[("flag", JsonValue::Bool(true))]);
        let a = run(&rules, d.clone(), 64);
        let b = run(&rules, d, 64);
        assert_eq!(a, b);
    }

    #[test]
    fn oversized_steps_rejected() {
        let rules = vec![set_rule("x", 1)];
        let sources = vec!["rules/r.json".to_string()];
        assert!(simulate(&rules, &sources, &doc(&[]), MAX_STEPS_CAP + 1).is_err());
    }
}

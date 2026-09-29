// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 会话级 caller_role 解析器（O-179，B-3 接缝的应用层实现）
//!
//! # 机制（专项设计档 §三 方案 A′：声明随首因事实）
//!
//! 1. 声明落账：`POST /api/sessions` 可选声明 `caller_role`；命令入口对声明会话
//!    提交的每条 Command 在 instruction 根部注入 `__meta__.caller_role`
//!    （PR-1，随 WAL 落账 = 判定权威源）。
//! 2. 解析：本模块沿 `ctx.cause` 的 cause 链上溯至起点 `Fact::Command`，
//!    读其 `instruction.__meta__.caller_role`（缺失/非法 → Unknown）。
//! 3. 接线：两处 per-session `IoSubscriber` 构造点（`SessionOps::create_session`、
//!    `POST /api/sessions` 主路径）经 `session_caller_role_resolver` 注入
//!    `PermissionGate::with_caller_role_resolver`（governance 侧 B-3 接缝）。
//!
//! # 确定性（红线③核验，设计档 §四）
//!
//! resolver 为纯函数：输入 =（ctx.cause × 会话 WAL 事实 × 固定深度上限），
//! 无时钟/随机/外部可变状态。历史事实不可变（append-only），cause 链前缀
//! 不因并发追加改变 → 同输入必同角色，回放重算结果一致。
//!
//! # 围栏语义留痕（实施微调，对设计档 §三 3 的澄清）
//!
//! 设计档原文「按 v_trigger 围栏只看 ≤v_trigger 的事实」——实测 `ctx.v_trigger`
//! 是 gate.check 冻结的 **SharedFactsLog** 版本（gate.rs L87，D8 版本域），
//! 与会话私有 FactsLog 的版本**不同域**，字面比较不成立。实施以「cause 链
//! 可达性」实现自围栏：走查只沿 cause/request_id 指针向上游走，链上事实
//! 均早于 IoRequest 本身（判定时刻必然已落账且不可变），链外事实（并发
//! 追加的新命令等）永不参与解析。shared 侧版本围栏仍由 gate.check 的权限
//! 快照承担（snapshot_at）。语义等价：判定输入在判定时刻完全冻结。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use evorule_reactor::{CallerRole, Fact, FactId, FactsLog, IoCallContext};

/// cause 链走查固定深度上限（防环/超长链；达上限 → Unknown，fail-closed）
pub const MAX_CHAIN_DEPTH: usize = 64;

/// 构造会话级 resolver（捕获会话私有 FactsLog 克隆；governance B-3 接缝签名）
pub fn session_caller_role_resolver(
    facts_log: FactsLog,
) -> Arc<dyn Fn(&IoCallContext) -> CallerRole + Send + Sync> {
    Arc::new(move |ctx: &IoCallContext| resolve_caller_role(&facts_log, ctx))
}

/// 沿 cause 链上溯至起点 Command，读 `instruction.__meta__.caller_role`
///
/// # 走查规则（只向上游走，方向单一）
///
/// - `Fact::Command`：终点（无 cause 指针）——读声明；
/// - `StateTransition{cause}` / `IoRequest{cause}`：上溯 `cause`；
/// - `IoResponse{request_id}`：上溯 `request_id`（断点 1 修复的恢复链
///   IoResponse→IoRequest 通配，设计档 §三 3）；
/// - 其他事实类型 / 缺事实 / 断链 / 环 / 超深度上限 → `Unknown`（fail-closed）。
pub fn resolve_caller_role(log: &FactsLog, ctx: &IoCallContext) -> CallerRole {
    let history = log.history_with_versions();
    // FactId → 事实 索引（历史 append-only，本轮走查内快照一致）
    let mut by_id: HashMap<FactId, &Fact> = HashMap::with_capacity(history.len());
    for (_, fact) in &history {
        by_id.insert(fact.id(), fact);
    }
    walk_chain(&by_id, ctx.cause)
}

/// 从 `start` 沿链上溯（索引化，供 resolver 与单测共用）
fn walk_chain(by_id: &HashMap<FactId, &Fact>, start: FactId) -> CallerRole {
    let mut visited: HashSet<FactId> = HashSet::new();
    let mut current = start;

    for _ in 0..=MAX_CHAIN_DEPTH {
        if !visited.insert(current) {
            return CallerRole::Unknown; // 环 → fail-closed
        }
        let Some(fact) = by_id.get(&current) else {
            return CallerRole::Unknown; // 缺事实（断链）→ fail-closed
        };
        match fact {
            Fact::Command { instruction, .. } => {
                return instruction
                    .get("__meta__")
                    .and_then(|m| m.get("caller_role"))
                    .and_then(|v| v.as_str())
                    .map(CallerRole::from_str_opt)
                    .unwrap_or(CallerRole::Unknown);
            }
            Fact::StateTransition { cause, .. } => current = *cause,
            Fact::IoRequest { cause, .. } => current = *cause,
            Fact::IoResponse { request_id, .. } => current = *request_id,
            // 记录性/旁路事实不属业务 cause 链（TransitionTrace/Violation/Stable/
            // Error/PayloadUpdate）→ 出现在链上即结构异常，fail-closed
            _ => return CallerRole::Unknown,
        }
    }
    CallerRole::Unknown // 超深度上限 → fail-closed
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use evorule_tcb::JsonValue;

    /// O-179 resolver 单测矩阵（设计档 §七）：
    /// 1 声明 human/llm 链完整；2 未声明/非法值；3 IoResponse 恢复链；
    /// 4 深度上限/环/断链；5 链外事实不参与；6 gate 级判定覆盖。
    /// {"type":"call_service"} (+ 可选 __meta__.caller_role)
    fn instruction_with_role(role: Option<&str>) -> JsonValue {
        let mut obj = Vec::new();
        obj.push(("type", JsonValue::string("call_service")));
        if let Some(r) = role {
            obj.push((
                "__meta__",
                JsonValue::object_from_pairs(&[("caller_role", JsonValue::string(r))]),
            ));
        }
        JsonValue::object_from_pairs(&obj)
    }

    fn command(id: FactId, role: Option<&str>) -> Fact {
        Fact::Command {
            id,
            instruction: instruction_with_role(role),
        }
    }

    fn transition(id: FactId, cause: FactId) -> Fact {
        Fact::StateTransition {
            id,
            cause,
            new_payload: JsonValue::empty_object(),
            new_queue: Vec::new(),
        }
    }

    fn io_request(id: FactId, cause: FactId) -> Fact {
        Fact::IoRequest {
            id,
            cause,
            io_type: evorule_reactor::IoType::call_service(),
            params: JsonValue::empty_object(),
        }
    }

    fn io_response(id: FactId, request_id: FactId) -> Fact {
        Fact::IoResponse {
            id,
            request_id,
            result: JsonValue::empty_object(),
            error: None,
        }
    }

    /// 标准业务链：Command(声明) → ST → IoRequest，返回 IoRequest.id 作 ctx.cause
    fn chain(declared: Option<&str>) -> (FactsLog, FactId) {
        let log = FactsLog::new();
        let mut g = evorule_reactor::FactIdGenerator::new();
        let c = g.next_id();
        let st = g.next_id();
        let io = g.next_id();
        log.append(command(c, declared)).unwrap();
        log.append(transition(st, c)).unwrap();
        log.append(io_request(io, st)).unwrap();
        (log, io)
    }

    fn ctx_for(cause: FactId) -> IoCallContext {
        IoCallContext::new(cause, 0, None)
    }

    #[test]
    fn o179_res_1_declared_human_and_llm_resolved_along_chain() {
        let (log, io) = chain(Some("human"));
        assert_eq!(resolve_caller_role(&log, &ctx_for(io)), CallerRole::Human);

        let (log, io) = chain(Some("llm"));
        assert_eq!(resolve_caller_role(&log, &ctx_for(io)), CallerRole::Llm);
    }

    #[test]
    fn o179_res_2_undeclared_or_invalid_value_fails_closed() {
        // 未声明：Command 无 __meta__
        let (log, io) = chain(None);
        assert_eq!(resolve_caller_role(&log, &ctx_for(io)), CallerRole::Unknown);

        // 非法值：from_str_opt 兜底 Unknown
        let (log, io) = chain(Some("admin"));
        assert_eq!(resolve_caller_role(&log, &ctx_for(io)), CallerRole::Unknown);
    }

    #[test]
    fn o179_res_3_recovery_chain_via_io_response_still_reaches_command() {
        // 恢复链（断点 1 语义）：Cmd → ST → IoReq → IoResp → ST2 → IoReq2
        let log = FactsLog::new();
        let mut g = evorule_reactor::FactIdGenerator::new();
        let c = g.next_id();
        let st = g.next_id();
        let io1 = g.next_id();
        let resp = g.next_id();
        let st2 = g.next_id();
        let io2 = g.next_id();
        log.append(command(c, Some("human"))).unwrap();
        log.append(transition(st, c)).unwrap();
        log.append(io_request(io1, st)).unwrap();
        log.append(io_response(resp, io1)).unwrap();
        log.append(transition(st2, resp)).unwrap();
        log.append(io_request(io2, st2)).unwrap();

        assert_eq!(resolve_caller_role(&log, &ctx_for(io2)), CallerRole::Human);
    }

    #[test]
    fn o179_res_4_depth_cap_cycle_and_broken_chain_fail_closed() {
        // 深度上限：80 层 ST 链（> MAX_CHAIN_DEPTH）→ Unknown
        let log = FactsLog::new();
        let mut g = evorule_reactor::FactIdGenerator::new();
        let c = g.next_id();
        log.append(command(c, Some("human"))).unwrap();
        let mut prev = c;
        for _ in 0..80 {
            let id = g.next_id();
            log.append(transition(id, prev)).unwrap();
            prev = id;
        }
        assert_eq!(
            resolve_caller_role(&log, &ctx_for(prev)),
            CallerRole::Unknown
        );

        // 环：A→B→A
        let log = FactsLog::new();
        let mut g = evorule_reactor::FactIdGenerator::new();
        let a = g.next_id();
        let b = g.next_id();
        log.append(transition(a, b)).unwrap();
        log.append(transition(b, a)).unwrap();
        assert_eq!(resolve_caller_role(&log, &ctx_for(a)), CallerRole::Unknown);

        // 断链：cause 指向不存在事实
        let log = FactsLog::new();
        let missing = FactId(999_999);
        assert_eq!(
            resolve_caller_role(&log, &ctx_for(missing)),
            CallerRole::Unknown
        );
    }

    #[test]
    fn o179_res_5_off_chain_facts_do_not_participate() {
        // 同一 log 内两条独立链：目标链 human，另一链 llm —— 解析不串链
        let log = FactsLog::new();
        let mut g = evorule_reactor::FactIdGenerator::new();
        // human 链
        let c1 = g.next_id();
        let st1 = g.next_id();
        let io1 = g.next_id();
        log.append(command(c1, Some("human"))).unwrap();
        log.append(transition(st1, c1)).unwrap();
        log.append(io_request(io1, st1)).unwrap();
        // llm 链（链外事实）
        let c2 = g.next_id();
        let st2 = g.next_id();
        let io2 = g.next_id();
        log.append(command(c2, Some("llm"))).unwrap();
        log.append(transition(st2, c2)).unwrap();
        log.append(io_request(io2, st2)).unwrap();

        assert_eq!(resolve_caller_role(&log, &ctx_for(io1)), CallerRole::Human);
        assert_eq!(resolve_caller_role(&log, &ctx_for(io2)), CallerRole::Llm);
    }

    /// 矩阵 6（gate 级）：resolver 覆盖后判定——human 走默认策略 Allow、
    /// llm Deny、未知 Deny（空权限表走 DefaultPolicy 语义）。
    #[test]
    fn o179_res_6_gate_check_applies_resolver_verdict() {
        use evorule_governance::permission::PermissionGate;
        use evorule_governance::shared_facts_log::SharedFactsLog;

        let gate = PermissionGate::new(std::sync::Arc::new(SharedFactsLog::new()));

        // human 链 → Allow（DefaultPolicy.human=Allow，无条目匹配路径）
        let (log, io) = chain(Some("human"));
        let resolver = session_caller_role_resolver(log);
        let gate_h = gate.clone().with_caller_role_resolver(resolver);
        let mut ctx = ctx_for(io);
        assert_eq!(
            gate_h.check(&mut ctx, "call_service", None),
            evorule_governance::permission::Verdict::Allow
        );
        assert_eq!(ctx.caller_role, CallerRole::Human);

        // llm 链 → Deny（DefaultPolicy.llm=Deny）
        let (log, io) = chain(Some("llm"));
        let resolver = session_caller_role_resolver(log);
        let gate_l = gate.clone().with_caller_role_resolver(resolver);
        let mut ctx = ctx_for(io);
        assert_eq!(
            gate_l.check(&mut ctx, "call_service", None),
            evorule_governance::permission::Verdict::Deny
        );

        // 未声明 → Unknown → Deny（fail-closed 不回归的反向证据，单测级）
        let (log, io) = chain(None);
        let resolver = session_caller_role_resolver(log);
        let gate_u = gate.with_caller_role_resolver(resolver);
        let mut ctx = ctx_for(io);
        assert_eq!(
            gate_u.check(&mut ctx, "call_service", None),
            evorule_governance::permission::Verdict::Deny
        );
    }
}

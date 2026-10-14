// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 条件评估器 —— 复用 TCB `evaluate_domain`
//!
//! 将权限条目的 `conditions`（TCB domain 表达式）投影到一个由
//! [调用上下文](evorule_reactor::IoCallContext)和载荷构造的 exec_state 上求值。

use evorule_reactor::IoCallContext;
use evorule_tcb::domain::evaluate_domain;
use evorule_tcb::JsonValue;

use super::entry::PermissionError;

/// 权限条件评估器（无状态，直接复用 TCB 域求值器）
#[derive(Debug, Clone, Copy, Default)]
pub struct ConditionEvaluator;

impl ConditionEvaluator {
    /// 评估一个条件表达式
    ///
    /// # 参数
    /// - `conditions`: TCB domain 表达式（serde JSON）。例：
    ///   `{ "type": "all", "inner": [{ "type":"eq", "path":"__exec__.ctx.caller_role", "value":"human" }] }`
    /// - `ctx`: I/O 调用上下文（写入 exec_state 的 `__exec__.ctx` 命名空间）
    /// - `payload`: 本次 I/O 载荷（写入 exec_state 的 `__exec__.payload`）
    ///
    /// # 返回
    /// 空对象/空表达式视为"无约束"，返回 `Ok(true)`。
    pub fn evaluate(
        &self,
        conditions: &serde_json::Value,
        ctx: &IoCallContext,
        payload: &JsonValue,
    ) -> Result<bool, PermissionError> {
        if conditions.is_null()
            || (conditions.is_object() && conditions.as_object().is_some_and(|o| o.is_empty()))
        {
            return Ok(true);
        }
        let domain = super::serde_to_tcb(conditions);
        let exec_state = build_conditions_exec_state(ctx, payload);
        evaluate_domain(&domain, &exec_state).map_err(|e| PermissionError::Condition(e.to_string()))
    }
}

/// 构造供 `evaluate_domain` 使用的 exec_state
///
/// 结构为 `{ "__exec__": { "ctx": {...}, "payload": <payload> } }`，
/// 与 TCB 路径解析（自动补全 `__exec__.` 前缀）对齐，
/// 因此条件可写 `__exec__.ctx.caller_role` 或相对路径 `ctx.caller_role`。
fn build_conditions_exec_state(ctx: &IoCallContext, payload: &JsonValue) -> JsonValue {
    let mut exec = JsonValue::empty_object();
    let mut exec_inner = JsonValue::empty_object();

    let mut ctx_obj = JsonValue::empty_object();
    if let Some(c) = ctx_obj.as_object_mut() {
        c.insert("cause".to_string(), JsonValue::Integer(ctx.cause.0 as i64));
        c.insert(
            "v_trigger".to_string(),
            JsonValue::Integer(ctx.v_trigger as i64),
        );
        c.insert(
            "caller_role".to_string(),
            JsonValue::string(ctx.caller_role.as_str()),
        );
        if let Some(tenant) = &ctx.tenant_id {
            c.insert("tenant_id".to_string(), JsonValue::string(tenant.clone()));
        }
    }
    if let Some(inner) = exec_inner.as_object_mut() {
        inner.insert("ctx".to_string(), ctx_obj);
        inner.insert("payload".to_string(), payload.clone());
    }
    if let Some(root) = exec.as_object_mut() {
        root.insert("__exec__".to_string(), exec_inner);
    }
    exec
}

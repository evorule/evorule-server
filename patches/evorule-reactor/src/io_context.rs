// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! I/O 调用上下文 —— 权限判定与审计对齐的信息载体
//!
//! # 定位
//! 本模块提供两种**纯数据类型**（additive，不改动任何既有 trait/签名）：
//! - [`CallerRole`]：调用者角色（human / llm / unknown）
//! - [`IoCallContext`]：一次 I/O 调用的上下文快照（cause / v_trigger / caller_role ...）
//!
//! 它们首先被 governance 层权限判定（`PermissionTable::evaluate`）消费，
//! 后续将透传给每个 `IoHandler` 与 `IoDispatcher::dispatch`（详见实施文档 17）。
//!
//! # 版本语义（重要，见实施文档 17 §5.3）
//! `IoCallContext.v_trigger` 是**冻结版本**而非"当前版本"：
//! - 权限判定永远基于调用发起时冻结（处理该 IoRequest 时 FactsLog::version()）的快照；
//! - 权限变更只影响 v_trigger 更大的新调用，绝不影响已判定的旧调用；
//! - 回放重算必须用 v_trigger，否则破坏确定性。

use std::sync::Arc;

use crate::FactId;

/// 调用者角色 —— 调用时"代表谁"（区别于规则 `author`：author 表示"谁写的"）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallerRole {
    /// 人类规则发起的调用
    Human,
    /// LLM 规则发起的调用
    Llm,
    /// 无法确定（fail-closed 默认拒绝；白名单豁免见实施文档 17 §7）
    Unknown,
}

impl CallerRole {
    /// 稳定字符串表示（用于审计/记录与规则元数据解析）
    pub const fn as_str(self) -> &'static str {
        match self {
            CallerRole::Human => "human",
            CallerRole::Llm => "llm",
            CallerRole::Unknown => "unknown",
        }
    }

    /// 从字符串解析：`"human"`/`"llm"` → 对应角色，其余 → [`CallerRole::Unknown`]
    pub fn from_str_opt(s: &str) -> Self {
        match s {
            "human" => CallerRole::Human,
            "llm" => CallerRole::Llm,
            _ => CallerRole::Unknown,
        }
    }
}

impl core::fmt::Display for CallerRole {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// caller_role 解析器接缝（B-3）
///
/// 由应用层注入：给定一次 I/O 调用上下文，返回其调用者角色。缺省不注入时
/// `PermissionGate` 沿用 `ctx.caller_role`（默认 `Unknown` → 默认策略 fail-closed）。
/// 完整的逐 cause 链解析依赖会话私有 `FactsLog`（governance 侧 `PermissionGate` 只持有
/// `SharedFactsLog`，无法回溯会话事实流），故应在持有会话事实的应用层实现后注入本接缝；
/// 局限性与后续建议见实施文档 24 §8.3 B-3。
pub type CallerRoleResolver = Arc<dyn Fn(&IoCallContext) -> CallerRole + Send + Sync>;

/// I/O 调用上下文 —— 权限判定与审计对齐的信息载体
#[derive(Debug, Clone)]
pub struct IoCallContext {
    /// 触发本次 I/O 的源事实 ID（来自 `Fact::IoRequest.cause`）
    pub cause: FactId,
    /// 触发时刻的引擎版本号（v_trigger，用于严格回放权限投影）。
    /// 语义见模块级注释：这是**冻结版本**，权限判定基于该版本快照。
    pub v_trigger: u64,
    /// 调用者角色（沿 cause 链回溯得到）：human / llm / unknown
    pub caller_role: CallerRole,
    /// cause 链（预计算，或留空由权权限判定器惰性回溯）
    pub cause_chain: Vec<FactId>,
    /// 租户 ID（None = 全局会话），用于多租户 scope 过滤
    pub tenant_id: Option<String>,
}

impl IoCallContext {
    /// 便捷构造（默认未知调用者，后续由权权限判定器沿 cause 链解析 caller_role）
    pub fn new(cause: FactId, v_trigger: u64, tenant_id: Option<String>) -> Self {
        Self {
            cause,
            v_trigger,
            caller_role: CallerRole::Unknown,
            cause_chain: Vec::new(),
            tenant_id,
        }
    }
}

impl Default for IoCallContext {
    fn default() -> Self {
        Self {
            cause: FactId(0),
            v_trigger: 0,
            caller_role: CallerRole::Unknown,
            cause_chain: Vec::new(),
            tenant_id: None,
        }
    }
}

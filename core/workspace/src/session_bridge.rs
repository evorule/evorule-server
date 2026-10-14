// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! SessionOps trait — 桥接 evorule-governance 的 SessionManager
//!
//! 设计依据: WORKSPACE_CRATE_DESIGN.md §4
//!           SANDBOX_ORCHESTRATION_DESIGN.md §3.2 (扩展)
//!           PUBLISH_QUEUE_DESIGN.md §4 (reload_rules 扩展)
//!
//! # 目的
//! workspace crate 不直接依赖 evorule-governance (避免循环依赖 + 保持可独立测试)。
//! 通过 `SessionOps` trait 抽象会话操作,由 evorule-server 在应用层为 `SessionApi`
//! 实现此 trait,实现 workspace crate 与底层 SessionManager 的解耦。
//!
//! # 方法定义
//! ## 基础方法 (WORKSPACE_CRATE_DESIGN.md M5)
//! - [`create_session`] — 创建新会话,返回 session_id
//! - [`fork_session`] — 从父会话 fork 新会话
//! - [`close_session`] — 关闭会话
//! - [`list_sessions`] — 列出所有活跃会话 ID
//! - [`send_command`] — 向会话发送命令
//! - [`get_session_state`] — 获取会话当前状态快照
//!
//! ## 沙盒编排扩展 (SANDBOX_ORCHESTRATION_DESIGN.md §3.2)
//! - [`fork_session_with_rules`] — fork 并原子注入草稿规则集（规则进沙盒
//!   Reactor 规则集，权利面冲突 fail-fast 拒启）
//! - [`get_audit_report`] — 获取审计报告 (含 BLAKE3 链验证)
//! - [`get_audit_export`] — 获取审计链导出 (JSON 字符串)
//! - [`get_facts`] — 获取 Fact 列表 (测试报告统计)
//! - [`get_causal_chain`] — 获取因果链 (Fact 因果追溯)
//!
//! ## 发布队列扩展 (PUBLISH_QUEUE_DESIGN.md §4)
//! - [`reload_rules`] — 触发规则热重载 (SessionManager 内部 core_eval 更新)

use async_trait::async_trait;
use serde_json::Value;

use crate::error::{WorkspaceError, WorkspaceResult};

/// 草稿规则集治理指纹（R4/[CON-S-003] fork provenance 补真）
///
/// 由编排侧（start_sandbox）从 DB 治理事实构造：rule_versions 真实版本 ID
/// + compute_ruleset_hash 草稿集哈希——fork 实现侧逐条装载 ctx 据此携带，
/// 禁止再出现 `draft#i` 伪造标签+空 hash（provenance 必须与治理链对齐，
/// 沙盒裁决铸 `sandbox:` 背书=治理行为，铸造信任者必须留真指纹）。
#[derive(Debug, Clone)]
pub struct DraftRulesProvenance {
    /// 与 `extra_rules` 等长按序对应：每条草稿规则的治理版本 ID（rule_versions.id）
    pub rule_version_ids: Vec<String>,
    /// 草稿规则集 BLAKE3 哈希（`compute_ruleset_hash` 口径）
    pub ruleset_hash: String,
}

/// 会话操作抽象 trait
///
/// 由 evorule-server 中的 `SessionApi` 实现。
// clippy::double_must_use (clippy 1.99 新增): async_trait 宏为每个 trait 方法生成的
// `#[must_use]` 与 boxed Future 自身的 must_use 语义重复, 错误源自宏展开而非手写码,
// 整 trait 豁免。
#[allow(clippy::double_must_use)]
#[async_trait]
pub trait SessionOps: Send + Sync {
    // ===== 基础方法 (WORKSPACE_CRATE_DESIGN.md M5) =====

    /// 创建新会话,返回 session_id
    ///
    /// 实现应:
    /// 1. 调用 SessionManager::create_session()
    /// 2. 返回新分配的 session_id
    async fn create_session(&self) -> WorkspaceResult<u64>;

    /// 登记会话主体声明（可选能力，默认 no-op 零回归）
    ///
    /// 实现应把 (session_id, role) 登记进声明表（命令入口据此注入
    /// `__meta__.caller_role`，权限门按声明判定）。缺省实现不登记——
    /// 未声明会话 io 走 fail-closed 默认策略，与既有行为一致。
    async fn declare_caller_role(&self, _session_id: u64, _role: &str) -> WorkspaceResult<()> {
        Ok(())
    }

    /// 从父会话 fork 新会话,返回新 session_id
    ///
    /// 实现应调用 SessionManager::create_session_from_parent_at_version()
    async fn fork_session(&self, parent_session_id: u64) -> WorkspaceResult<u64>;

    /// 从父会话 fork 新会话并注入额外规则集（沙盒编排扩展）
    ///
    /// `extra_rules` 为草稿规则 JSON 数组（每条与规则文件同形态的单条规则）。
    /// 实现应原子完成：把草稿规则追加到当前全局规则集 → fork 父会话（新会话
    /// Reactor 持含草稿的规则集 clone）→ 回滚全局规则集——草稿规则仅进入
    /// 沙盒会话，生产与其他会话不受影响。
    ///
    /// 预检语义（fail-fast）：任一草稿规则的权利面（io_type 发射/消费）与
    /// 生产规则集或草稿集内部冲突时返回 `invalid_input` 错误并明示冲突明细，
    /// 不创建沙盒——避免冲突规则被静默剥除后测试建立在不完整规则集上
    /// （静默通过禁令）。
    ///
    /// 缺省实现返回「不支持」：仅沙盒编排链路需要此能力，其他桥接实现
    /// （发布/滚动/工作空间测试桩）无需关心。
    ///
    /// `provenance`（R4/[CON-S-003]）：草稿治理指纹——实现侧装载 ctx 必须据此
    /// 携带真实版本 ID+ruleset hash（长度必须与 `extra_rules` 一致，不一致
    /// fail-fast 拒启）。
    async fn fork_session_with_rules(
        &self,
        _parent_session_id: u64,
        _extra_rules: &[Value],
        _provenance: &DraftRulesProvenance,
    ) -> WorkspaceResult<u64> {
        Err(WorkspaceError::internal(
            "fork_session_with_rules not supported by this session bridge",
        ))
    }

    /// 检查会话是否存在
    ///
    /// 前置缺陷修复: production_state 持久化的 current_session_id 在 server 重启后可能失效
    /// (SessionManager 为内存态), 滚动发布前需校验旧生产 session 是否仍存活;
    /// 不存在时应回退为"首次发布"新建 session, 而非 fork 失败 404。
    async fn session_exists(&self, session_id: u64) -> bool;

    /// 关闭会话
    ///
    /// 实现应调用 SessionManager::close_session()
    async fn close_session(&self, session_id: u64) -> WorkspaceResult<()>;

    /// 列出当前所有活跃会话 ID
    ///
    /// 注意: SessionManager 不知道 workspace_id,所以这里返回全部活跃会话 ID,
    /// 由调用方(workspace_service)结合 db 中 sessions 表的 workspace_id 过滤。
    async fn list_sessions(&self) -> WorkspaceResult<Vec<u64>>;

    /// 向指定会话发送命令,返回 fact_id
    ///
    /// `command` 是原始 JSON 指令(如 `{"type":"noop"}`)
    async fn send_command(&self, session_id: u64, command: Value) -> WorkspaceResult<u64>;

    /// 获取会话当前状态快照
    ///
    /// 返回包含 payload / queue / version / reactor 信息的状态 JSON。
    async fn get_session_state(&self, session_id: u64) -> WorkspaceResult<Value>;

    // ===== 沙盒编排扩展 (SANDBOX_ORCHESTRATION_DESIGN.md §3.2) =====

    /// 获取审计报告 (含 BLAKE3 链验证结果)
    ///
    /// 返回 JSON,包含审计链长度、验证状态、Fact 统计等。
    async fn get_audit_report(&self, session_id: u64) -> WorkspaceResult<Value>;

    /// 获取审计链导出 (JSON 字符串)
    ///
    /// 返回完整的审计链 JSON,用于沙盒关闭时导出 test Fact。
    async fn get_audit_export(&self, session_id: u64) -> WorkspaceResult<String>;

    /// 获取 Fact 列表 (用于测试报告统计)
    ///
    /// 返回 session 的全部 Fact，**完整事实 JSON 形态**（`{"type","id",...}`
    /// 变体字段，与 Fact::to_json 对齐：Command 含 instruction、IoResponse 含
    /// result/error、Error 含 message、Violation 含 reason）。
    /// 注意区别于 get_audit_export（BLAKE3 审计链导出，fact_type/fact_id
    /// 元数据键名形态）——两者键名不同，消费方按本契约的 "type"/"id" 读取。
    async fn get_facts(&self, session_id: u64) -> WorkspaceResult<Vec<Value>>;

    /// 获取因果链 (某条 Fact 的因果追溯)
    ///
    /// 返回从根 Fact 到指定 Fact 的因果链 JSON。
    async fn get_causal_chain(&self, session_id: u64, fact_id: u64) -> WorkspaceResult<Value>;

    // ===== 发布队列扩展 (PUBLISH_QUEUE_DESIGN.md §4) =====

    /// 触发规则热重载
    ///
    /// 实现应调用 evorule-server 的 reload handler,
    /// 使 SessionManager 内部 core_eval 更新 (影响后续新创建的 session)。
    async fn reload_rules(&self) -> WorkspaceResult<()>;

    /// 显式刷新审计链 (缺口5 修复)
    ///
    /// 将 FactsLog 中尚未审计的 Fact 刷入 BLAKE3 哈希链。
    /// send_command 后应调用此方法确保审计链实时性 (缺口2)。
    /// 读方法 (get_audit_report 等) 内部也会兜底调用, 但显式 flush 语义更清晰。
    async fn flush_audit(&self, session_id: u64) -> WorkspaceResult<usize>;
}

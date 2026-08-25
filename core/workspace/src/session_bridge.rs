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
//! - [`get_audit_report`] — 获取审计报告 (含 BLAKE3 链验证)
//! - [`get_audit_export`] — 获取审计链导出 (JSON 字符串)
//! - [`get_facts`] — 获取 Fact 列表 (测试报告统计)
//! - [`get_causal_chain`] — 获取因果链 (Fact 因果追溯)
//!
//! ## 发布队列扩展 (PUBLISH_QUEUE_DESIGN.md §4)
//! - [`reload_rules`] — 触发规则热重载 (SessionManager 内部 core_eval 更新)

use async_trait::async_trait;
use serde_json::Value;

use crate::error::WorkspaceResult;

/// 会话操作抽象 trait
///
/// 由 evorule-server 中的 `SessionApi` 实现。
#[async_trait]
pub trait SessionOps: Send + Sync {
    // ===== 基础方法 (WORKSPACE_CRATE_DESIGN.md M5) =====

    /// 创建新会话,返回 session_id
    ///
    /// 实现应:
    /// 1. 调用 SessionManager::create_session()
    /// 2. 返回新分配的 session_id
    async fn create_session(&self) -> WorkspaceResult<u64>;

    /// 从父会话 fork 新会话,返回新 session_id
    ///
    /// 实现应调用 SessionManager::create_session_from_parent_at_version()
    async fn fork_session(&self, parent_session_id: u64) -> WorkspaceResult<u64>;

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
    /// 返回 session 的全部 Fact (JSON 数组)。
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

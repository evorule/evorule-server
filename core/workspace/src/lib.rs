// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! # evorule-workspace
//!
//! Workspace crate: 多租户工作空间 + 规则元数据管理 + 会话桥接 + 沙盒编排 + 发布队列。
//!
//! 设计依据:
//! - `WORKSPACE_CRATE_DESIGN.md` (P10 基础设施层)
//! - `SANDBOX_ORCHESTRATION_DESIGN.md` (Layer 3 沙盒编排)
//! - `PUBLISH_QUEUE_DESIGN.md` (发布工作流 + 滚动 session 热重载)
//!
//! # 模块结构
//! - [`error`] — 错误类型 + axum IntoResponse 实现
//! - [`models`] — 数据模型 + 状态机枚举 (workspaces/rules/sessions/sandbox/publish_queue/production)
//! - [`db`] — SQLite 连接 + schema 迁移 + 全部 CRUD
//! - [`session_bridge`] — SessionOps trait (桥接 SessionApi)
//! - [`workspace_service`] — Workspace 业务服务
//! - [`rule_meta_service`] — 规则元数据 + 状态机 + BLAKE3 哈希
//! - [`mock_io_responder`] — 沙盒合成 IO 响应器 (S2)
//! - [`test_report`] — 测试报告 schema + 生成 + BLAKE3 签名 (S3)
//! - [`session_switched`] — U7 SSE session_switched 广播 (P3)
//! - [`rolling_session`] — 滚动 session 热重载编排 (P2)
//! - [`sandbox_service`] — 沙盒编排主流程 (S1)
//! - [`publish_service`] — 发布队列 + 三级权限 + 状态机 (P1)
//! - [`api`] — HTTP handler + Router 构建
//!
//! # 安全约束
//! - `#![forbid(unsafe_code)]` (C4)
//! - 测试代码外禁止 `unwrap`/`expect`/`panic` (C5)

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod api;
pub mod bundle_land;
pub mod db;
pub mod error;
pub mod mock_io_responder;
pub mod models;
pub mod publish_service;
pub mod rolling_session;
pub mod rule_meta_service;
pub mod rule_translate;
pub mod sandbox_service;
pub mod session_bridge;
pub mod session_switched;
pub mod test_report;
pub mod verdict_service;
pub mod workspace_service;

// 顶层重导出 (供 evorule-server 直接 use)
// 注意: `WorkspaceState` 存在两个含义:
// - `api::WorkspaceState`: HTTP API 共享状态 (持有 Arc<WorkspaceService> + Arc<RuleMetaService> + ...)
// - `models::WorkspaceState`: 工作空间生命周期状态机枚举 (Active → Archived)
// 此处顶层导出 api::WorkspaceState (evorule-server AppState 字段使用);
// models 的状态机枚举通过 `evorule_workspace::models::WorkspaceState` 访问。
pub use api::{build_workspace_router, WorkspaceState};
pub use bundle_land::{
    land_bundle_atomically, land_knowledge_bundle_atomically, BundleManifest, EntryFileManifest,
};
pub use db::WorkspaceDb;
pub use error::{WorkspaceError, WorkspaceResult};
pub use models::{
    BundleImportRecord, ProductionAuditRecord, ProductionStateRecord, PublishQueueItem,
    PublishRole, PublishStatus, RuleRecord, RuleSessionBinding, RuleState, RuleVersionRecord,
    RuleVersionState, SandboxSession, SandboxStatus, SessionBindingState, SessionRecord,
    TestDatasetRecord, VerdictContractRecord, VersionClockMapRecord, WorkspaceMemberRecord,
    WorkspaceRecord,
};
pub use publish_service::PublishService;
pub use rolling_session::{RollingSessionService, RollingSwapResult};
pub use rule_meta_service::RuleMetaService;
pub use sandbox_service::SandboxService;
pub use session_bridge::SessionOps;
pub use session_switched::{SessionSwitchedBroadcaster, SessionSwitchedEvent};
pub use test_report::{TestReport, TestReportBuilder};
pub use verdict_service::VerdictService;
pub use workspace_service::WorkspaceService;

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! EvoRule 治理层 - I/O 订阅者、审计链、规则验证、时间机器
//!
//! # 定位
//! evorule-governance 是三层架构的最上层（纯机制层库）：
//! - 订阅 evorule-reactor 的 event broadcast 通道，过滤 `IoRequest` 事实
//! - 通过 IoDispatcher 框架分发 I/O（具体 Handler 实现由应用层注入，本 crate 仅定义机制）
//! - 基于 `FactsLog` 构建审计链（BLAKE3 哈希 + 逻辑时钟）
//! - 提供规则验证器（RuleValidator）+ 时间机器（TimeMachine: replay / rewind / fork / diff）
//! - 提供 SessionManager：会话管理器（机制层，管理反应器实例生命周期）
//!
//! **H5/H6 边界清理后不再包含**：HTTP API、SSE、Prometheus metrics、Bearer 认证、具体 I/O Handler 实现。
//! 上述应用层功能由应用层自行构建，本 crate 仅提供机制层接口。
//!
//! # 设计原则
//! - **不染指控制流**：治理层完全不知道 `conditional`/`while_loop`/`sequence` 的存在
//! - **I/O 外挂**：仅定义 IoHandler trait + IoDispatcher 框架（机制），零业务逻辑侵入；具体实现由应用层注入
//! - **事实总线**：所有组件通过 Fact 通信，无直接函数调用
//!
//! # 模块结构
//! - `io_handler` — IoHandler trait + IoResult（机制定义，应用层注入实现）
//! - `io_dispatcher` — Enum Dispatch（I/O 分发框架）
//! - `io_subscriber` — 订阅 IoRequest → 分发 → 回写 IoResponse
//! - `auditor` — 基于 FactsLog 的审计链
//! - `hash` — BLAKE3 哈希
//! - `clock` — 逻辑时钟
//! - `session` — 会话管理器（机制层，管理反应器实例生命周期）
//! - `rule_validation` — 规则 JSON Schema 验证器（tier0 core_eval.json）
//! - `time_machine` — 时间机器：replay / rewind / fork / diff
//! - `shared_facts_log` — 跨 session 共享 FactsLog 包装
//! - `metrics` — IoMetrics trait（机制层接口，Prometheus 实现由应用层提供）
// cluster 已移除（多 reactor 协作原语，应用层功能，后续由应用层实现）
// api, io_handlers, bin 目录已全部迁出本 crate（H5/H6 边界清理）

#![forbid(unsafe_code)]
#![deny(missing_docs)]

// H6: api 模块已迁出本 crate（机制-策略分离，核心层不再包含 HTTP API 代码）
pub mod anchor;
pub mod auditor;
pub mod circuit_breaker;
pub mod clock;
pub mod hash;
pub mod io_dispatcher;
pub mod io_handler;
// H5: io_handlers 具体实现已外迁（机制-策略分离）
// 具体 handler 实现(DbHandler/HttpHandler/MemoryHandler)不再属于本 crate
pub mod io_subscriber;
pub mod metrics;
pub mod permission;
// object_pool 已移除（性能优化，非核心功能）
pub mod rule_validation;
// H6: session 模块从 api/ 提升到顶层（机制层，管理反应器实例生命周期）
pub mod session;
// G-A1: 审计锚点签名（真实性/防抵赖），可选能力——未配置签名器时审计行为与旧版完全一致
pub mod shared_facts_log;
pub mod signing;
pub mod time_machine;

// 公开 API 重导出
// H6: AppState/GovernanceApi/GovernanceServer/SessionApi 已迁移到应用层（不再从核心导出）
pub use auditor::{Auditor, LoadError};
pub use clock::LogicalClock;
// cluster 已移除（多 reactor 协作原语，应用层功能）
pub use hash::content_hash;
pub use io_dispatcher::{IoDispatcher, IoDispatcherBuilder};
pub use io_handler::{IoHandler, IoResult};
pub use io_subscriber::IoSubscriber;
pub use metrics::{noop_metrics, IoMetrics, NoOpMetrics, SharedMetrics};
pub use permission::{
    ConditionEvaluator, DefaultPolicy, PermissionEntry, PermissionError, PermissionGate,
    PermissionState, PermissionTable, Verdict,
};
pub use session::{Session, SessionAnchorConfig, SessionError, SessionId, SessionManager};
// G-A1: 审计锚点签名（真实性/防抵赖）
pub use anchor::{
    signature_payload_v2, verify_anchor_chain, verify_binding, AnchorChainBuilder, AnchorError,
    AnchorProto, AuditAnchor, FactRange, ANCHOR_FORMAT, ANCHOR_GENESIS,
};
pub use signing::{verify_signature, AuditSigner, SignError};
// ObjectPool 已移除（性能优化，非核心功能）
pub use shared_facts_log::{SharedFact, SharedFactsLog};

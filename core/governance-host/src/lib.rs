// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! EvoRule 治理会话宿主 —— SessionManager + Auditor 服务端 fork
//!
//! # 定位
//!
//! 本 crate 是 evorule-governance 的**服务端能力宿主**（fork 收编）：
//! 从 governance 0.10.0 fork 出 `session`（SessionManager）与 `auditor`
//! （Auditor）两个模块，承接原 vendored patch 独有的三族服务端能力：
//!
//! - **G6 规则集锚定**：`ruleset_version` 一次性锚点写入审计链
//!   （`__ruleset__.version` 哨兵事实，FactId `u64::MAX - 1`）
//! - **rehydrate 崩溃复活**：同 sid WAL 重放复活会话
//!   （`rehydrate_session_from_wal`，FactId 从历史最大值续跑防重号）
//! - **EVID 导出面**：`AuditEntry` 携带 `fact_json` 原文 + `fact_json_stable`
//!   导出深比对（篡改信号 fail-fast）
//!
//! 其余 governance 子模块（io_subscriber/signing/metrics/rule_validation/
//! circuit_breaker/permission/time_machine/shared_facts_log 等）继续使用
//! 注册表正本 `evorule-governance`（0.10.0），本 crate 零重复。
//!
//! # 设计原则
//!
//! - fork 最小面：只收编 server 真实消费的 session/auditor 两模块
//! - 上游能力宿主：evorule-governance 仍是能力正本，本 crate 是服务端
//!   增量面的宿主（区别于 vendored patch 时代的「临时改写」定位）
//! - 事实总线：所有组件通过 Fact 通信，无直接函数调用
//!
//! # 模块结构
//! - `session` — SessionManager（含 G6 锚定 + rehydrate 复活族）
//! - `auditor` — Auditor（含 EVID fact_json 面 + 锚点持久化）

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod auditor;
pub mod session;

// 公开 API 重导出（与 evorule-governance 的 session/auditor 面同形，
// server 消费点按 evorule_governance_host:: 平移）
pub use auditor::{Auditor, LoadError};
pub use session::{Session, SessionAnchorConfig, SessionError, SessionId, SessionManager};

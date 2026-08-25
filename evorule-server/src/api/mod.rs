// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! API 服务模块（应用层）
//!
//! HTTP API + 认证 + 会话 HTTP 路由
//!
//! H6 架构合规整改：从 evorule-governance/src/api/ 迁移到应用层。
//! SessionManager 本身保留在 evorule_governance::session（机制层），
//! 此处仅包含 HTTP 路由处理（应用层策略）。

pub mod openapi;
pub mod server;
pub mod permissions;
pub mod bundles;

// H6: main.rs 直接从 `api::server::{...}` 导入所需类型,
// 此处不再 `pub use` 重导出（mod api 是 private 的,外部 crate 无法访问）。
// 如未来需要从 crate root 重导出,改为 `pub use server::{...};` 即可。

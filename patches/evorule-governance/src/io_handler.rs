// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! IoHandler trait —— H5 已下沉到 evorule-reactor
//!
//! # 迁移说明
//!
//! 此 trait 原定义于此文件,使用 `impl Future`(RPITIT)签名,非 object-safe。
//!
//! H5 架构债务修复中,trait 已下沉到 `evorule-reactor`(与 `IoType` 同层),
//! 并改用 `#[async_trait]` 使其 object-safe,支持 `Arc<dyn IoHandler>`。
//!
//! 此文件保留 re-export 以维持向后兼容:
//! 现有代码 `use evorule_governance::io_handler::IoHandler` 仍可用。
//!
//! 新代码推荐直接使用 `evorule_reactor::IoHandler`。

pub use evorule_reactor::{IoHandler, IoResult};

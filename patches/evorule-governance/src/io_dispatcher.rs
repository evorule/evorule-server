// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! I/O Dispatcher —— v0.2.0 已下沉到 evorule-reactor
//!
//! # 迁移说明
//!
//! 此模块原定义了独立的 `IoDispatcher` / `IoDispatcherBuilder` 结构体，与
//! `evorule-reactor` 中的实现重复（v0.1.x H5 下沉时遗留的副本）。
//!
//! v0.2.0 自定义 IoType 能力重构中，`IoDispatcher` 已在 `evorule-reactor` 统一实现，
//! 并新增 `contains()` / `known_types()` 方法（供加载期校验使用）。保留 governance
//! 独立副本会导致：
//! - `evorule_governance::IoDispatcher` ≠ `evorule_reactor::IoDispatcher`（不同类型）
//! - governance 用户缺少 `contains()` / `known_types()` 方法
//! - 混用两个 crate 的 IoDispatcher 产生类型不匹配编译错误
//!
//! 此文件改为 re-export，与 `io_handler.rs` 的迁移模式一致：
//! 现有代码 `use evorule_governance::IoDispatcher` 仍可用，且自动获得 reactor
//! 版本的全部方法（含 v0.2.0 新增的 `contains()` / `known_types()`）。
//!
//! 新代码推荐直接使用 `evorule_reactor::IoDispatcher`。

pub use evorule_reactor::{IoDispatcher, IoDispatcherBuilder};

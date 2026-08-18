// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! evorule-server 库层（供集成测试访问内部模块）
//!
//! 将 bin 的内部模块（api / auth / metrics_impl）暴露为 lib，
//! 使 `tests/` 下的集成测试能够构造 `AppState` 和 `Router` 进行
//! HTTP 层端到端联合测试。
//!
//! `main.rs` 仅保留配置解析、日志初始化、信号处理等入口逻辑，
//! 通过 `use evorule_server::...` 复用此处的模块。

#![forbid(unsafe_code)]
// C5 (unwrap/expect/panic = deny) 仅约束生产代码;测试代码保留 unwrap 惯例
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod api;
pub mod auth;
pub mod input_sanitizer;
pub mod metrics_impl;

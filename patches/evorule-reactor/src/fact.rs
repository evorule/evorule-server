// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 事实（Fact）定义 —— re-export 壳
//!
//! 实现已随 2026-10-18 SC3-a 方案乙迁入 [`evorule_audit_chain`]（no_std 审计链内核），
//! 本模块按既有三方 re-export 先例保持 `crate::fact` 路径与公开面零变化：
//! reactor / governance / cli 的既有引用无需任何改动。

pub use evorule_audit_chain::fact::*;

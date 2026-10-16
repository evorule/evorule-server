// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Kani proof 模块（仅 Kani 编译时启用）
//!
//! - `model`：结构化符号输入辅助（已知形状 + 符号叶子，控制状态展开成本）
//! - `kani_proofs`：P1-P21 证明清单（见 verification/kani-formal-verification-design.md §四）

#![cfg(kani)]

mod kani_proofs;
mod model;

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `constants.rs`：编译期锁定常量。
//! vendored 自 rpsm-demo `rpsm-core` v0.1.0(2026-09-01 快照),除本头注外逐行保持原实现。
//! 无任何 setter，不暴露 Arc 给外部；任何尝试经由 JSON/配置修改此值的操作
//! 都在 rpsm-hci 层被过滤丢弃。

/// 万有引力常数 (m^3 kg^-1 s^-2)
pub const G: f64 = 6.674_30e-11;

/// 真空光速 (m/s)
pub const C: f64 = 299_792_458.0;

/// 允许被外部配置覆盖的常数名集合。若配置尝试覆盖这些键，直接拒绝。
/// 实际值为空集合——G 与 C 一律不可被任何外部输入修改。
pub const LOCKED_CONSTANTS: &[&str] = &["G", "C"];

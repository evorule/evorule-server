// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 向量/四元数运算封装（vendored 自 rpsm-demo `rpsm-core` v0.1.0 快照）。
//! 自定义轻量 `Vec3`/`Quaternion`，避免为大场景引入重框架依赖。

pub mod quaternion;
pub mod vec3;

pub use quaternion::{integrate_orientation, Quaternion};
pub use vec3::Vec3;

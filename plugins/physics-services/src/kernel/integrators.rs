// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 数值积分器：辛欧拉（order 1）与速度 Verlet（order 2）。
//! vendored 自 rpsm-demo `rpsm-core` v0.1.0(2026-09-01 快照),逐行保持原实现。
//!
//! 二者均为**辛（symplectic）积分器**：对牛顿系统能量长期有界、不单调发散。
//! 速度 Verlet 为二阶，对变力（如 N 体引力）能量守恒显著优于辛欧拉——这是
//! `test_conservation_two_body_orbit` 验证的核心物理前提。
//!
//! 具体步进位于 `PhysicalKernel::tick_collision_optional`，按 `integrator_order`
//! 分派：辛欧拉为单遍（先速度后位置）；速度 Verlet 为两遍（先按旧加速度漂移
//! 位置、重算新位置处的力、再更新速度）。本 trait 承载「选择/标识」语义，由
//! `build_integrator` 在运行时构造，并随 HCI 热重载的 `integrator_order` 切换。

/// 积分器标识。具体步进由 `PhysicalKernel` 按 `order()` 分派。
pub trait Integrator {
    /// 阶数：1 = 辛欧拉（SymplecticEuler），2 = 速度 Verlet（VelocityVerlet）。
    fn order(&self) -> u8;
}

/// 辛欧拉（半隐式欧拉）积分器，对应 `integrator_order = 1`。
#[derive(Debug, Clone, Copy)]
pub struct SymplecticEuler;

impl Integrator for SymplecticEuler {
    fn order(&self) -> u8 {
        1
    }
}

/// 速度 Verlet 积分器，对应 `integrator_order = 2`。
#[derive(Debug, Clone, Copy)]
pub struct VelocityVerlet;

impl Integrator for VelocityVerlet {
    fn order(&self) -> u8 {
        2
    }
}

/// 依据积分器阶数构建积分器。
///
/// - `1` → 辛欧拉（半隐式欧拉）
/// - `2` → 速度 Verlet
///
/// 其它值返回错误，与 `rpsm-hci::load_config` 的校验口径一致（fail-fast）。
pub fn build_integrator(order: u8) -> Result<Box<dyn Integrator>, String> {
    match order {
        1 => Ok(Box::new(SymplecticEuler)),
        2 => Ok(Box::new(VelocityVerlet)),
        other => Err(format!("integrator_order 仅支持 1 或 2，收到 {other}")),
    }
}

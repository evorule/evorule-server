// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 单位四元数：表示刚体空间取向，用于确定性旋转积分。
//! vendored 自 rpsm-demo `rpsm-core` v0.1.0 快照,逐行保持原实现。

use super::Vec3;

/// 单位四元数 `(w, x, y, z)`，`w` 为标量部分。恒等 = `(1,0,0,0)`。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quaternion {
    pub w: f64,
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Default for Quaternion {
    fn default() -> Self {
        Self::identity()
    }
}

impl Quaternion {
    pub const fn identity() -> Self {
        Self {
            w: 1.0,
            x: 0.0,
            y: 0.0,
            z: 0.0,
        }
    }

    /// 纯四元数 `(0, v)`（由向量构造）。
    pub fn pure(v: Vec3) -> Self {
        Self {
            w: 0.0,
            x: v.x,
            y: v.y,
            z: v.z,
        }
    }

    pub fn length_squared(self) -> f64 {
        self.w * self.w + self.x * self.x + self.y * self.y + self.z * self.z
    }

    pub fn length(self) -> f64 {
        self.length_squared().sqrt()
    }

    /// 规范化到单位长度（四元数必须单位化才表示纯旋转）。
    pub fn normalize(self) -> Self {
        let l = self.length();
        if l <= f64::EPSILON {
            Self::identity()
        } else {
            self * (1.0 / l)
        }
    }

    /// 共轭（对单位四元数即其逆）：`(w,-x,-y,-z)`。用于求相对误差 `desired ⊗ q⁻¹`。
    pub fn conjugate(self) -> Self {
        Self {
            w: self.w,
            x: -self.x,
            y: -self.y,
            z: -self.z,
        }
    }

    /// 旋转向量 `θ·axis`（轴角缩放为向量）。恒等或向量部分近零时返回零向量。
    /// 用于把「两取向之差」转成可驱动 PD 复位力矩的三维误差向量。
    pub fn rotation_vector(self) -> Vec3 {
        let v = Vec3::new(self.x, self.y, self.z);
        let v2 = v.length();
        if v2 <= f64::EPSILON {
            return Vec3::zero();
        }
        // 轴角：θ = 2·atan2(|v|, w)，axis = v/|v|；旋转向量 = axis·θ。
        let theta = 2.0 * (v2 / self.w).atan();
        v * (theta / v2)
    }

    /// 用四元数旋转向量：`v' = q ⊗ (0,v) ⊗ q⁻¹`（单位四元数下 q⁻¹ = 共轭）。
    /// 约束投影求解需要：把局部铰接点/铰轴映射到世界系（`world = R·local`）。
    pub fn rotate_vec(self, v: Vec3) -> Vec3 {
        let qv = Vec3::new(self.x, self.y, self.z);
        let t = qv.cross(v) * 2.0;
        v + t * self.w + qv.cross(t)
    }
}

impl std::ops::Mul<f64> for Quaternion {
    type Output = Self;
    fn mul(self, s: f64) -> Self {
        Self {
            w: self.w * s,
            x: self.x * s,
            y: self.y * s,
            z: self.z * s,
        }
    }
}

impl std::ops::Add for Quaternion {
    type Output = Self;
    fn add(self, o: Self) -> Self {
        Self {
            w: self.w + o.w,
            x: self.x + o.x,
            y: self.y + o.y,
            z: self.z + o.z,
        }
    }
}

/// Hamilton 积：`self ⊗ other`（组合旋转）。
impl std::ops::Mul for Quaternion {
    type Output = Self;
    fn mul(self, o: Self) -> Self {
        Self {
            w: self.w * o.w - self.x * o.x - self.y * o.y - self.z * o.z,
            x: self.w * o.x + self.x * o.w + self.y * o.z - self.z * o.y,
            y: self.w * o.y - self.x * o.z + self.y * o.w + self.z * o.x,
            z: self.w * o.z + self.x * o.y - self.y * o.x + self.z * o.w,
        }
    }
}

/// 用角速度 `ω`（世界系）推进单位四元数一阶步长，并单位化。
/// 微分几何：`dq/dt = ½ (0,ω) ⊗ q`。给定确定性输入产出唯一结果。
pub fn integrate_orientation(q: Quaternion, omega: Vec3, dt: f64) -> Quaternion {
    let dq = Quaternion::pure(omega) * 0.5 * dt;
    (q + dq * q).normalize()
}

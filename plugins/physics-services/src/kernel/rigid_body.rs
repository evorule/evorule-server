// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 刚体属性与碰撞响应。
//! vendored 自 rpsm-demo `rpsm-core` v0.1.0(2026-09-01 快照);
//! 除本头注与导入路径调整(`crate::math` → `crate::kernel::math`)外逐行保持原实现。

use crate::kernel::math::{Quaternion, Vec3};

/// 刚体：质量、位置、速度、半径与每帧清零的力累加器。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RigidBody {
    pub mass: f64,
    pub pos: Vec3,
    pub vel: Vec3,
    pub force_accum: Vec3,
    /// 每帧清零的力矩累加器：刚性铰链约束力矩（锚点力臂力矩 + 轴对齐复位力矩）在此
    /// 累积，随后并入旋转积分（与注入外力矩、有界姿态 PD 一同生效）。与 `force_accum`
    /// 同生命周期：每帧开始由 `accumulate_forces` 清零、结束时归零以便快照。
    pub torque_accum: Vec3,
    /// 用于碰撞检测的半径（m）；默认 0.0（视作质点），仅开启半径几何的刚体启用碰撞。
    pub radius: f64,
    /// 线性空气阻力系数 b（N·s/m，`F_drag = -b·v`）；默认 0.0（无阻力）。
    ///
    /// 确定性物理模型：与重力/多体引力同在内核力合成，孪生内核克隆同一刚体故可原样
    /// 复现，属「被确定性建模」效应（账本可标 modeled）。默认 0 保证向后兼容。
    pub drag: f64,
    /// 平方空气阻力系数 c2（N·s²/m²，`F_drag = -c2·|v|·v`）；默认 0.0（无平方阻力）。
    ///
    /// 常规风阻一次/二次阻力分开：线性 `drag` 与平方 `drag_quadratic` 并存叠加。
    /// 自由落体终速 `v_t = √(m·g / c2)`，是确定性非保守力，孪生内核原样复现、账本可标 modeled。
    /// 默认 0 保证向后兼容。
    pub drag_quadratic: f64,
    /// 地面切向动力学摩擦系数 μ；默认 0.0（无摩擦）。
    ///
    /// 接触响应内生效：刚体触地且存在水平速度时，按 `μ·g·dt` 削减水平速度（封顶不反向）。
    /// 与竖直恢复系数碰撞同属确定性接触响应，孪生内核以同开关复现，账本可标 modeled。
    pub friction: f64,
    /// 弹簧-阻尼锚点：`Some(anchor)` 时挂线性弹簧-阻尼器 `F = -k·(p−a) − c·v`；
    /// `None` 表示无绑定（默认）。锚点一旦挂载即确定性力，孪生内核克隆复现。
    pub anchor: Option<Vec3>,
    /// 弹簧刚度 k（N/m）；仅当 `anchor` 为非 `None` 时生效。
    pub stiffness: f64,
    /// 阻尼系数 c（N·s/m）；仅当 `anchor` 为非 `None` 时生效（c=0 即纯弹簧）。
    pub damping: f64,
    /// 空间取向（单位四元数）；默认恒等。由角速度确定性积分推进。
    pub orientation: Quaternion,
    /// 角速度 ω（世界系，rad/s）；无外力矩时自由旋转守恒。
    pub angular_velocity: Vec3,
    /// 转动惯量（标量，各向同性，如球体 I=⅖·m·r²）。0 表示无旋转自由度。
    pub inertia: f64,
    /// 目标取向约束：`Some(target)` 时施加姿态复位力矩（旋转域弹簧），`None` 不约束取向。
    /// 与 `with_orientation_constraint` 配套。默认 `None`（自由旋转/外力矩向后兼容）。
    pub orient_target: Option<Quaternion>,
    /// 姿态误差力矩刚度 k_ang（N·m/rad）：`τ += +k_ang·err`，`err` 为拉向目标的矫正旋转。
    /// 仅当 `orient_target` 非 `None` 生效。
    pub angular_stiffness: f64,
    /// 角速度阻尼 c_ang（N·m·s/rad）：`τ += -c_ang·ω`。与 k_ang 配合成 PD 控制，收敛不发散。
    pub angular_damping: f64,
    /// 约束力矩饱和上界（N·m）：有界控制力矩核心，`|τ|` 钳制到该值防发散。`<=0` 视为无约束上限。
    pub max_torque: f64,
    /// 双势阱保守势（沿 X 轴）：`Some(w)` 时施加 `V(x) = w.a·(x² − w.m)²`，
    /// 外力 `F_x = −dV/dx = −4a·x·(x²−m)`。`None`（默认）无此项。
    ///
    /// **为何置于内核而非外力槽（诚实披露）**：外力槽为每帧注值的静态槽，速度 Verlet
    /// 第二次加速沿用注入值，使映射去辛（`det J = 1 − ½·a'·h² ≠ 1`），保守场能量会长期
    /// 漂移而非有界。作为内核级保守力（在 `accumulate_forces` 中按当前位置重算），
    /// velocity Verlet 能在新旧位置各求一次梯度，保持辛积分、能量近守恒（有界）——
    /// 这是「能量有界（守护验证器压力素材）」成立的机制前提。
    pub double_well: Option<DoubleWell>,
    /// 双体软铰关节（球铰中央力约束）：把本刚体与 `joint.body` 刚体的质心以
    /// 弹簧-阻尼连接，`F_i = u·(k·(d−L) − c·along)`（`u` 为自体质心→对端质心的
    /// 连线单位向量、`d` 为距离、`along=(v_i−v_j)·u`），等值反作用于对端。
    /// `stiffness>0` 为保守弹簧（势能 `½·k·(d−L)²` 并入总能量），`damping>0` 为
    /// 耗散项；`rest_length` 为自由长度。`None`（默认）无关节。
    ///
    /// **为何置于内核而非外力槽（同双势阱）**：软铰是多体**相互**约束，单靠外力槽
    /// 无法表达「等值反作用于对端」；且需在 velocity Verlet 新旧位置各求一次以保辛性。
    /// 作为内核级力（`accumulate_forces` 按当前位置重算），能量可证守恒（保守核心）。
    ///
    /// **边界（诚实披露）**：本 MVP 为「作用于质心的中央力」软铰，不产生力矩、
    /// 不断言刚性铰链（真机械臂肘关节需刚性转角约束 + 力矩耦合）——那是下一增量。
    pub joint: Option<Joint>,
    /// 刚性旋转铰链（revolute joint）：机械臂肘关节/运动链的刚性转角约束 + 力矩耦合。
    /// 见 [`HingeJoint`] 说明。`None`（默认）无刚性铰链。
    pub hinge: Option<HingeJoint>,
}

/// 双体软铰参数（球铰中央力约束，作用于两刚体质心连线）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Joint {
    /// 对端刚体在 `kernel.bodies` 中的索引（须存在，越界时约束被安全忽略）。
    pub body: usize,
    /// 弹簧劲度 k（N/m），0 视为仅有阻尼（保守势能随之为零）。
    pub stiffness: f64,
    /// 阻尼 c（N·s/m），0 为纯弹簧（保守、能量守恒可断言）。
    pub damping: f64,
    /// 自由长度 L（m），两点连线距离 d 偏离 L 时产生弹簧回复力。
    pub rest_length: f64,
}

/// 刚性旋转铰链（revolute joint，机械臂肘关节的刚性转角约束 + 力矩耦合）。
///
/// 与软铰（`Joint`，质心中央力弹簧）相对，本约束把**铰接点**（各刚体局部锚点）的
/// 世界坐标锁在一起（位置约束），并把**铰轴方向**对齐（角度约束），只允许两刚体绕
/// 铰轴相对转动——正是机械臂肘关节/运动链的语义。
///
/// 约束力/力矩按当前位置在 `accumulate_forces` 中重算（velocity Verlet 新旧各求一次）：
/// - **位置投影（保守）**：锚点误差 `e = a_j − a_i` 产生弹簧力 `F = k_p·e`（作用于
///   锚点，折算为质心平动 + 力矩 `τ = r×F`），势能 `½·k_p·|e|²` 入总能量；
/// - **角度投影（保守）**：铰轴错位 `n_i×n_j` 产生复位力矩 `τ_θ = k_θ·(n_i×n_j)`
///   （只对齐轴向、允许绕轴相对转动），势能 `½·k_θ·|n_i−n_j|²` 入总能量；
/// - **速度投影（耗散，可选）**：`damping>0` 时沿**相对角速度**施加速度投影阻尼
///   `τ = −c·(ω_i − ω_j)`（revolute 关节摩擦，只抵消绕铰轴相对转动这一自由 DOF，
///   确定性耗散），默认 0 = 纯约束（保守）。
///
/// **边界（诚实披露）**：本 MVP 为高刚度惩罚约束（penalty，非完全刚性投影），
/// 约束误差随 `k_p/k_θ` 增大而减小，数值上保持有界而非严格为零；`damping` 为关节
/// 摩擦的确定性模型，供 PLA 证伪注入非保守耗散。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HingeJoint {
    /// 对端：`Some(j)` 连接 `kernel.bodies[j]`；`None` 连接固定世界锚点 `anchor`。
    pub body: Option<usize>,
    /// 固定世界锚点（仅 `body=None` 时生效，世界坐标）。
    pub anchor: Vec3,
    /// 本刚体局部坐标系中的铰接点（相对质心偏移）。
    pub local_pivot: Vec3,
    /// 本刚体局部坐标系中的铰轴方向（单位向量）。
    pub local_axis: Vec3,
    /// 对端刚体局部坐标系中的铰接点（`body=Some` 时生效）。
    pub other_local_pivot: Vec3,
    /// 对端铰轴方向：`body=Some` 时为对端刚体局部坐标；`body=None` 时为固定世界方向。
    pub other_local_axis: Vec3,
    /// 位置约束劲度 k_p（N/m）：越高锚点越刚。
    pub position_stiffness: f64,
    /// 角度约束劲度 k_θ（N·m/rad）：越高铰轴越对齐。
    pub angular_stiffness: f64,
    /// 关节摩擦阻尼（N·s/m，约束方向速度投影的耗散项）：0 为纯约束（保守）。
    pub damping: f64,
}

/// 双势阱参数：`V(x) = a·(x² − m)²`（井底 x=±√m、V=0；中央势垒 x=0、高 a·m²）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DoubleWell {
    pub a: f64,
    pub m: f64,
}

impl RigidBody {
    pub fn new(mass: f64, pos: Vec3, vel: Vec3) -> Self {
        Self {
            mass,
            pos,
            vel,
            force_accum: Vec3::zero(),
            torque_accum: Vec3::zero(),
            radius: 0.0,
            drag: 0.0,
            drag_quadratic: 0.0,
            friction: 0.0,
            anchor: None,
            stiffness: 0.0,
            damping: 0.0,
            orientation: Quaternion::identity(),
            angular_velocity: Vec3::zero(),
            inertia: 0.0,
            orient_target: None,
            angular_stiffness: 0.0,
            angular_damping: 0.0,
            max_torque: 0.0,
            double_well: None,
            joint: None,
            hinge: None,
        }
    }

    /// 设置碰撞半径（构造后追加，保持 `new` 三参签名向后兼容）。
    pub fn with_radius(mut self, radius: f64) -> Self {
        self.radius = radius;
        self
    }

    /// 设置线性空气阻力系数（`F_drag = -b·v`，b≥0；构造后追加，向后兼容）。
    pub fn with_drag(mut self, drag: f64) -> Self {
        self.drag = drag.max(0.0);
        self
    }

    /// 设置平方空气阻力系数（`F_drag = -c2·|v|·v`，c2≥0；构造后追加，向后兼容）。
    /// 与线性 `drag` 可并存叠加，共同构成阻力合力。
    pub fn with_drag_quadratic(mut self, drag_quadratic: f64) -> Self {
        self.drag_quadratic = drag_quadratic.max(0.0);
        self
    }

    /// 设置地面切向动力学摩擦系数（μ≥0；构造后追加，向后兼容）。
    pub fn with_friction(mut self, friction: f64) -> Self {
        self.friction = friction.max(0.0);
        self
    }

    /// 挂载弹簧-阻尼器到锚点（k≥0，c≥0）；构造后追加，向后兼容。
    pub fn with_spring(mut self, anchor: Vec3, stiffness: f64, damping: f64) -> Self {
        self.anchor = Some(anchor);
        self.stiffness = stiffness.max(0.0);
        self.damping = damping.max(0.0);
        self
    }

    /// 挂载双体软铰关节：把本刚体与 `body` 刚体（索引）的质心以弹簧-阻尼连接。
    /// `F_i = u·(k·(d−L) − c·along)`，`u`=自体质心→对端质心连线单位向量、`d`=距离、
    /// `L`=自由长度、`along=(v_self−v_other)·u`；等值反作用于对端。
    /// 拉伸时回复力把两体质心拉拢（保守），阻尼项恒耗散。k≥0、c≥0，c=0 为纯弹簧
    /// （保守、能量可断言）。构造后追加，向后兼容。
    pub fn with_joint(
        mut self,
        body: usize,
        stiffness: f64,
        damping: f64,
        rest_length: f64,
    ) -> Self {
        self.joint = Some(Joint {
            body,
            stiffness: stiffness.max(0.0),
            damping: damping.max(0.0),
            rest_length: rest_length.max(0.0),
        });
        self
    }

    /// 挂载**双体刚性铰链**（运动链节）：本刚体与 `body` 刚体以旋转铰链相连。
    /// 位置约束把本刚体 `local_pivot` 世界点与对端 `other_local_pivot` 世界点锁在一起；
    /// 角度约束把本刚体 `local_axis` 与对端 `other_local_axis` 对齐（只允许绕轴相对转动）。
    /// 锚点误差 `e` 产生保守弹簧力 `F_i = k_p·e`、`F_j = −F_i`（作用于锚点并折算为质心
    /// 平动 + 力矩）；轴错位 `n_i×n_j` 产生保守复位力矩 `τ_i = k_θ·(n_i×n_j)`。
    /// `damping>0` 为关节摩擦（相对角速度阻尼 `τ=−c·(ω_i−ω_j)`，耗散项，PLA 证伪注入用），
    /// 默认 0 纯约束（保守）。k_p≥0、k_θ≥0、damping≥0。构造后追加，向后兼容。
    #[allow(clippy::too_many_arguments)] // 铰链几何/力学参数均为独立物理量，8 参为一个域内完整签名
    pub fn with_hinge(
        mut self,
        body: usize,
        local_pivot: Vec3,
        local_axis: Vec3,
        other_local_pivot: Vec3,
        other_local_axis: Vec3,
        kp: f64,
        ka: f64,
        damping: f64,
    ) -> Self {
        self.hinge = Some(HingeJoint {
            body: Some(body),
            anchor: Vec3::zero(),
            local_pivot,
            local_axis: local_axis.normalize(),
            other_local_pivot,
            other_local_axis: other_local_axis.normalize(),
            position_stiffness: kp.max(0.0),
            angular_stiffness: ka.max(0.0),
            damping: damping.max(0.0),
        });
        self
    }

    /// 挂载**固定世界铰链**（单摆）：本刚体铰接到固定世界锚点 `anchor`。
    /// 位置约束把本刚体 `local_pivot` 世界点与 `anchor` 锁在一起（保守弹簧力）；
    /// 角度约束（可选）把本刚体 `local_axis` 与固定世界方向 `axis` 对齐（`ka=0` 即不约束取向）。
    /// `damping>0` 为关节摩擦（相对角速度阻尼 `τ=−c·ω_i`，耗散项，PLA 证伪注入用）。
    /// 构造后追加，向后兼容。
    pub fn with_fixed_hinge(
        mut self,
        anchor: Vec3,
        local_pivot: Vec3,
        local_axis: Vec3,
        kp: f64,
        ka: f64,
        damping: f64,
    ) -> Self {
        self.hinge = Some(HingeJoint {
            body: None,
            anchor,
            local_pivot,
            local_axis: local_axis.normalize(),
            other_local_pivot: Vec3::zero(),
            // 固定世界方向：默认取构造时局部轴（保持初始取向，ka>0 才生效）。
            other_local_axis: if ka > 0.0 { local_axis.normalize() } else { Vec3::zero() },
            position_stiffness: kp.max(0.0),
            angular_stiffness: ka.max(0.0),
            damping: damping.max(0.0),
        });
        self
    }

    /// 挂载双势阱保守势（沿 X 轴）：`V(x)=a·(x²−m)²`，`F_x=−4a·x·(x²−m)`。
    /// 保守力、随位置实时重算（velocity Verlet 新旧各求一次），辛积分下能量近守恒。
    /// 构造后追加，向后兼容。
    pub fn with_double_well(mut self, a: f64, m: f64) -> Self {
        self.double_well = Some(DoubleWell { a: a.max(0.0), m: m.max(0.0) });
        self
    }

    /// 开启旋转自由度：设定初始角速度与转动惯量（各向同性标量，>0 才参与旋转积分）。
    /// 取向默认为恒等四元数，可由调用方随后覆盖。构造后追加，向后兼容。
    pub fn with_rotation(mut self, angular_velocity: Vec3, inertia: f64) -> Self {
        self.angular_velocity = angular_velocity;
        self.inertia = inertia.max(0.0);
        self
    }

    /// 挂载有界姿态约束（旋转域弹簧-阻尼 PD 控制）：把取向收敛到 `target` 而非发散。
    /// `τ = +k_ang·err − c_ang·ω`，其中 `err` 为「目标取向 ⊗ 当前取向⁻¹」的旋转向量；
    /// `|τ|` 钳制到 `max_torque`（`<=0` 表示无界）。k_ang=0 且 c_ang=0 时无约束力矩（退回自由旋转/外力矩）。构造后追加，向后兼容。
    pub fn with_orientation_constraint(
        mut self,
        target: Quaternion,
        stiffness: f64,
        damping: f64,
        max_torque: f64,
    ) -> Self {
        self.orient_target = Some(target);
        self.angular_stiffness = stiffness.max(0.0);
        self.angular_damping = damping.max(0.0);
        self.max_torque = max_torque;
        self
    }

    pub fn set_radius(&mut self, radius: f64) {
        self.radius = radius;
    }
}

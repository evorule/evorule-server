//! [evorule 移植注记] 本文件自 rpsm-demo `rpsm/tests/test_hinge.rs`(2026-09-01 快照)移植为 evorule-physics-services 集成测试:import 改路(rpsm_core → evorule_physics_services::kernel),测试逻辑逐行保真。
//! 双体刚性铰链（revolute joint，机械臂肘关节/运动链的刚性转角约束 + 力矩耦合）确定性验证。
//!
//! 模型：把**铰接锚点**世界坐标锁在一起（位置约束 k_p）、把**铰轴方向**对齐（角度约束
//! k_θ），只允许两刚体绕铰轴相对转动。约束力/力矩按当前位置在 `accumulate_forces` 中
//! 重算（velocity Verlet 新旧各求一次）→ 辛映射、能量近守恒。`damping` 为关节摩擦
//! （**相对角速度**阻尼 `τ=−c·(ω_i−ω_j)`，只抵消绕铰轴相对转动这一自由 DOF，确定性耗散，
//! PLA 证伪注入用）。
//!
//! 边界（诚实披露）：本 MVP 是高刚度**惩罚**约束（penalty，非完全刚性投影），约束误差
//! 随 k_p/k_θ 增大而减小、数值上保持**有界**而非严格为零；`damping` 为关节摩擦的确定性
//! 模型。相对软铰（`Joint`，质心中央力、无角度约束）的增量 = 锚点位置约束 + 铰轴角度约束。
//!
//! 覆盖：约束保持 / 确定性 / 单摆守恒 / 双摆能量有界 / 角度约束收敛（刚铰增量）/
//! 阻尼单调耗散 / 与软铰同刚度对比。

// 集成测试保留 unwrap/expect 惯例（C5 unwrap/expect/panic = deny 仅约束生产代码）
#![allow(clippy::unwrap_used, clippy::expect_used)]

use evorule_physics_services::kernel::{PhysicalKernel, Quaternion, RigidBody, Vec3};

const MASS: f64 = 1.0;
const I: f64 = 0.4; // 各向同性转动惯量（如球 I=⅖·m·r²）
const KP: f64 = 4000.0; // 位置约束劲度 N/m
const KA: f64 = 800.0; // 角度约束劲度 N·m/rad
const DT: f64 = 0.001;
const NO_GRAV: Vec3 = Vec3::zero();

/// 双体铰链系统：A 在原点静止、B 在 (1,0,0) 处以绕铰轴 z 的角速度 `omega_z` 自旋。
/// 铰轴均为 z 轴、锚点 ±0.5x（肘部语义），`damping` 为关节摩擦。返回内核。
fn two_body_hinge(omega_z: f64, damping: f64) -> PhysicalKernel {
    let mut k = PhysicalKernel::with_integrator(NO_GRAV, 2).expect("order 2");
    k.bodies.push(
        RigidBody::new(MASS, Vec3::zero(), Vec3::zero())
            .with_rotation(Vec3::zero(), I)
            .with_hinge(
                1,
                Vec3::new(0.5, 0.0, 0.0),
                Vec3::new(0.0, 0.0, 1.0),
                Vec3::new(-0.5, 0.0, 0.0),
                Vec3::new(0.0, 0.0, 1.0),
                KP,
                KA,
                damping,
            ),
    );
    k.bodies.push(
        RigidBody::new(MASS, Vec3::new(1.0, 0.0, 0.0), Vec3::zero())
            .with_rotation(Vec3::new(0.0, 0.0, omega_z), I),
    );
    k
}

/// 固定世界铰链单摆：质心 (0,1,0)、局部锚点 (0,0.5,0)、世界锚点 (0,1.5,0)、初速 (2,0,0)、
/// 重力 −Y、I 转动惯量。`damping` 为关节摩擦（0 纯保守）。
fn pendulum(damping: f64) -> PhysicalKernel {
    let mut k = PhysicalKernel::with_integrator(Vec3::new(0.0, -9.81, 0.0), 2).expect("order 2");
    k.bodies.push(
        RigidBody::new(MASS, Vec3::new(0.0, 1.0, 0.0), Vec3::new(2.0, 0.0, 0.0))
            .with_rotation(Vec3::zero(), I)
            .with_fixed_hinge(
                Vec3::new(0.0, 1.5, 0.0),
                Vec3::new(0.0, 0.5, 0.0),
                Vec3::new(0.0, 0.0, 1.0),
                KP,
                0.0,
                damping,
            ),
    );
    k
}

/// 绕 X 轴转 `theta` 的四元数（半角形式 `(cos θ/2, sin θ/2, 0, 0)`）。
fn rot_x(theta: f64) -> Quaternion {
    Quaternion {
        w: (theta / 2.0).cos(),
        x: (theta / 2.0).sin(),
        y: 0.0,
        z: 0.0,
    }
}

/// ①约束保持（位置 + 角度）：绕铰轴自旋 → 锚点分离有界（惩罚误差）+ 铰轴恒对齐。
#[test]
fn hinge_holds_anchor_and_axis() {
    let mut k = two_body_hinge(2.0, 0.0);
    let q1 = k.bodies[1].orientation;
    let mut max_anchor = 0.0_f64;
    let mut max_axis = 0.0_f64;
    let mut rotated = false;
    for _ in 0..20_000 {
        k.tick(DT);
        let b0 = &k.bodies[0];
        let b1 = &k.bodies[1];
        let a_i = b0.pos + b0.orientation.rotate_vec(Vec3::new(0.5, 0.0, 0.0));
        let a_j = b1.pos + b1.orientation.rotate_vec(Vec3::new(-0.5, 0.0, 0.0));
        max_anchor = max_anchor.max((a_j - a_i).length());
        let n_i = b0.orientation.rotate_vec(Vec3::new(0.0, 0.0, 1.0));
        let n_j = b1.orientation.rotate_vec(Vec3::new(0.0, 0.0, 1.0));
        max_axis = max_axis.max(n_i.cross(n_j).length());
        if b1.orientation != q1 {
            rotated = true;
        }
    }
    assert!(
        max_anchor < 2e-2,
        "锚点分离应有界（惩罚约束误差）：max_anchor={max_anchor}"
    );
    assert!(
        max_axis < 1e-6,
        "铰轴应恒对齐（只允许绕轴相对转动）：max_axis={max_axis}"
    );
    assert!(rotated, "应发生绕铰轴的相对转动（关节自由 DOF）");
}

/// ②确定性：同输入双跑逐位一致（内核级铰链克隆原样复现）。
#[test]
fn hinge_is_deterministic_bitwise() {
    let run = || {
        let mut k = two_body_hinge(2.0, 0.5);
        let mut trace = Vec::new();
        for _ in 0..10_000 {
            k.tick(DT);
            trace.push((
                k.bodies[0].pos,
                k.bodies[0].orientation,
                k.bodies[0].angular_velocity,
                k.bodies[1].pos,
                k.bodies[1].orientation,
                k.bodies[1].angular_velocity,
            ));
        }
        trace
    };
    let a = run();
    let b = run();
    for (x, y) in a.iter().zip(b.iter()) {
        assert!(*x == *y, "刚性铰链轨迹必须逐位确定");
    }
}

/// ③单摆守恒：固定世界铰链（位置约束，保守）+ 重力，无阻尼 → 能量近守恒（有界）。
#[test]
fn fixed_hinge_pendulum_conserves_energy() {
    let mut k = pendulum(0.0);
    let e0 = k.total_mechanical_energy();
    let mut max_dev = 0.0_f64;
    for _ in 0..30_000 {
        k.tick(DT);
        max_dev = max_dev.max((k.total_mechanical_energy() - e0).abs());
    }
    assert!(
        max_dev < e0 * 1e-3,
        "单摆能量应近守恒：e0={e0} 最大偏离 {max_dev}"
    );
}

/// ④双摆能量有界：重力下双摆（固定世界铰链 + 双体铰链）、无阻尼，能量长期有界、摆幅不发散。
#[test]
fn double_pendulum_energy_bounded() {
    let mut k = PhysicalKernel::with_integrator(Vec3::new(0.0, -9.81, 0.0), 2).expect("order 2");
    k.bodies.push(
        RigidBody::new(MASS, Vec3::new(0.0, 1.5, 0.0), Vec3::zero())
            .with_rotation(Vec3::zero(), I)
            .with_fixed_hinge(
                Vec3::new(0.0, 2.0, 0.0),
                Vec3::new(0.0, 0.5, 0.0),
                Vec3::new(0.0, 0.0, 1.0),
                KP,
                0.0,
                0.0,
            ),
    );
    k.bodies.push(
        RigidBody::new(MASS, Vec3::new(0.0, 0.5, 0.0), Vec3::new(3.0, 0.0, 0.0))
            .with_rotation(Vec3::zero(), I)
            .with_hinge(
                0,
                Vec3::new(0.0, 0.5, 0.0),
                Vec3::new(0.0, 0.0, 1.0),
                Vec3::new(0.0, -0.5, 0.0),
                Vec3::new(0.0, 0.0, 1.0),
                KP,
                KA,
                0.0,
            ),
    );
    let e0 = k.total_mechanical_energy();
    let mut max_e = f64::NEG_INFINITY;
    let mut min_e = f64::INFINITY;
    let mut max_x1 = 0.0_f64;
    for _ in 0..40_000 {
        k.tick(DT);
        let e = k.total_mechanical_energy();
        max_e = max_e.max(e);
        min_e = min_e.min(e);
        max_x1 = max_x1.max(k.bodies[1].pos.x.abs());
    }
    assert!(
        max_e - e0 < e0 * 1e-3,
        "能量上偏应有界：e0={e0} max_e={max_e}"
    );
    assert!(
        (min_e - e0).abs() < e0 * 1e-3,
        "能量下偏应有界：e0={e0} min_e={min_e}"
    );
    assert!(max_x1 < 2.0, "摆幅不应发散：max_x1={max_x1}");
}

/// ⑤角度约束收敛（刚铰相对软铰的增量）：初始铰轴错开 + 关节阻尼 → 轴被拉回对齐。
#[test]
fn hinge_axis_alignment_restores() {
    // 零重力、无初速：body1 取向绕 X 错开 0.5 rad → 铰轴 z 转到 yz 平面（错位 |n_i×n_j|≈0.479）。
    // 角度约束复位力矩（kθ）把轴拉回对齐、关节阻尼消除相对转动（软铰无此能力）。
    let mut k = PhysicalKernel::with_integrator(NO_GRAV, 2).expect("order 2");
    k.bodies.push(
        RigidBody::new(MASS, Vec3::zero(), Vec3::zero())
            .with_rotation(Vec3::zero(), I)
            .with_hinge(
                1,
                Vec3::new(0.5, 0.0, 0.0),
                Vec3::new(0.0, 0.0, 1.0),
                Vec3::new(-0.5, 0.0, 0.0),
                Vec3::new(0.0, 0.0, 1.0),
                KP,
                KA,
                3.0,
            ),
    );
    k.bodies.push(
        RigidBody::new(MASS, Vec3::new(1.0, 0.0, 0.0), Vec3::zero()).with_rotation(Vec3::zero(), I),
    );
    k.bodies[1].orientation = rot_x(0.5);
    let mut max_axis = 0.0_f64;
    let mut final_axis = f64::INFINITY;
    for _ in 0..30_000 {
        k.tick(DT);
        let b0 = &k.bodies[0];
        let b1 = &k.bodies[1];
        let n_i = b0.orientation.rotate_vec(Vec3::new(0.0, 0.0, 1.0));
        let n_j = b1.orientation.rotate_vec(Vec3::new(0.0, 0.0, 1.0));
        let mis = n_i.cross(n_j).length();
        max_axis = max_axis.max(mis);
        final_axis = mis;
    }
    assert!(
        final_axis < 1e-3,
        "角度约束 + 阻尼应把铰轴拉回对齐：final_axis={final_axis}"
    );
    assert!(
        max_axis < 1.0,
        "轴错位过程应有界（不过冲爆炸）：max_axis={max_axis}"
    );
}

/// ⑥阻尼单调耗散：固定铰链单摆 + 关节摩擦 → 能量单调不增（浮点噪声尺度内）且显著耗散。
#[test]
fn hinge_damping_dissipates_monotonic() {
    let mut k = pendulum(0.8);
    let e0 = k.total_mechanical_energy();
    let mut prev = f64::INFINITY;
    let mut max_inject = 0.0_f64;
    let mut final_e = f64::INFINITY;
    for _ in 0..40_000 {
        k.tick(DT);
        let e = k.total_mechanical_energy();
        max_inject = max_inject.max(e - prev);
        prev = e;
        final_e = e;
    }
    assert!(
        max_inject < 1e-3,
        "关节摩擦不得注入能量：max_inject={max_inject}"
    );
    assert!(
        final_e < e0 - 1.0,
        "关节摩擦应显著耗散：e0={e0} final_e={final_e}"
    );
}

/// ⑦与软铰同刚度对比（k=8000 重力下垂）：惩罚约束误差都应有界（不随长时下垂累积/发散），
/// 且刚铰额外保持铰轴对齐（软铰无角度约束，不具备此增量能力）。
#[test]
fn rigid_and_soft_constraints_stay_bounded() {
    let mut soft = PhysicalKernel::with_integrator(Vec3::new(0.0, -9.81, 0.0), 2).expect("order 2");
    soft.bodies.push(
        RigidBody::new(MASS, Vec3::new(0.0, 1.0, 0.0), Vec3::zero())
            .with_joint(1, 8000.0, 0.0, 1.0),
    );
    soft.bodies.push(RigidBody::new(
        MASS,
        Vec3::new(1.0, 1.0, 0.0),
        Vec3::new(1.5, 0.0, 0.0),
    ));

    let mut rigid =
        PhysicalKernel::with_integrator(Vec3::new(0.0, -9.81, 0.0), 2).expect("order 2");
    rigid.bodies.push(
        RigidBody::new(MASS, Vec3::new(0.0, 1.0, 0.0), Vec3::zero()).with_hinge(
            1,
            Vec3::new(0.5, 0.0, 0.0),
            Vec3::new(0.0, 0.0, 1.0),
            Vec3::new(-0.5, 0.0, 0.0),
            Vec3::new(0.0, 0.0, 1.0),
            8000.0,
            800.0,
            0.0,
        ),
    );
    rigid.bodies.push(RigidBody::new(
        MASS,
        Vec3::new(1.0, 1.0, 0.0),
        Vec3::new(1.5, 0.0, 0.0),
    ));

    let mut soft_err = 0.0_f64;
    let mut rigid_err = 0.0_f64;
    let mut axis_err = 0.0_f64;
    for _ in 0..20_000 {
        soft.tick(DT);
        soft_err = soft_err.max((soft.bodies[1].pos - soft.bodies[0].pos).length() - 1.0);
        rigid.tick(DT);
        let b0 = &rigid.bodies[0];
        let b1 = &rigid.bodies[1];
        let a_i = b0.pos + b0.orientation.rotate_vec(Vec3::new(0.5, 0.0, 0.0));
        let a_j = b1.pos + b1.orientation.rotate_vec(Vec3::new(-0.5, 0.0, 0.0));
        rigid_err = rigid_err.max((a_j - a_i).length());
        let n_i = b0.orientation.rotate_vec(Vec3::new(0.0, 0.0, 1.0));
        let n_j = b1.orientation.rotate_vec(Vec3::new(0.0, 0.0, 1.0));
        axis_err = axis_err.max(n_i.cross(n_j).length());
    }
    assert!(soft_err < 3e-2, "软铰约束误差应有界：soft_err={soft_err}");
    assert!(
        rigid_err < 3e-2,
        "刚铰约束误差应有界：rigid_err={rigid_err}"
    );
    assert!(axis_err < 1e-6, "刚铰铰轴应恒对齐：axis_err={axis_err}");
}

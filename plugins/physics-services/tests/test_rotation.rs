//! [evorule 移植注记] 本文件自 rpsm-demo `rpsm/tests/test_rotation.rs`(2026-09-01 快照)移植为 evorule-physics-services 集成测试:import 改路(rpsm_core → evorule_physics_services::kernel),测试逻辑逐行保真。
//! 刚体旋转自由度的确定性验证：四元数取向积分 + 转动动能守恒。
//!
//! 主线 B 重量级里程碑。当前无外力矩源，ω 自由旋转守恒；取向由
//! `integrate_orientation`（dq/dt = ½ (0,ω) ⊗ q）确定性推进并单位化，
//! 转动动能 ½·I·|ω|² 计入 `total_mechanical_energy`，使自由旋转系统能量守恒可断言。

// 集成测试保留 unwrap/expect 惯例（C5 unwrap/expect/panic = deny 仅约束生产代码）
#![allow(clippy::unwrap_used, clippy::expect_used)]

use evorule_physics_services::kernel::{PhysicalKernel, Quaternion, Vec3};

const I: f64 = 0.4; // 各向同性转动惯量（如 m=1,r=1 球体 I=⅖·m·r²）
const OMEGA: f64 = 2.0;

/// 自由旋转：逐位确定性双跑一致（含取向与转动动能）。
#[test]
fn free_rotation_is_deterministic() {
    let run = || -> Vec<(Quaternion, f64, f64)> {
        let mut k = PhysicalKernel::new(Vec3::zero());
        k.bodies.push(
            evorule_physics_services::kernel::RigidBody::new(1.0, Vec3::zero(), Vec3::zero())
                .with_rotation(Vec3::new(0.0, 0.0, OMEGA), I),
        );
        let mut trace = Vec::new();
        for _ in 0..2000 {
            k.tick(0.001);
            let b = &k.bodies[0];
            trace.push((
                b.orientation,
                b.angular_velocity.z,
                k.total_mechanical_energy(),
            ));
        }
        trace
    };
    let a = run();
    let c = run();
    assert_eq!(a.len(), c.len());
    for ((q1, w1, e1), (q2, w2, e2)) in a.iter().zip(c.iter()) {
        assert!(*q1 == *q2, "旋转仿真取向必须逐位确定");
        assert!(*w1 == *w2, "角速度必须逐位确定");
        assert!(*e1 == *e2, "能量必须逐位确定");
    }
}

/// 自由旋转能量守恒：无重力、无外力矩下，E = ½·I·ω² 恒定；且取向真实推进并被单位化。
#[test]
fn free_rotation_conserves_energy_and_advances_orientation() {
    let mut k = PhysicalKernel::new(Vec3::zero());
    k.bodies.push(
        evorule_physics_services::kernel::RigidBody::new(1.0, Vec3::zero(), Vec3::zero())
            .with_rotation(Vec3::new(0.0, 0.0, OMEGA), I),
    );
    let e0 = k.total_mechanical_energy();
    let expected = 0.5 * I * OMEGA * OMEGA; // 0.5·0.4·4 = 0.8
    assert!(
        (e0 - expected).abs() < 1e-12,
        "初态转动动能应为 ½·I·ω²={expected}，实际 {e0}"
    );

    let mut min_e = e0;
    let mut max_e = e0;
    // 取向应持续偏离恒等（真实在转）。
    let mut away_from_identity = false;
    for _ in 0..4000 {
        k.tick(0.001);
        let b = &k.bodies[0];
        let e = k.total_mechanical_energy();
        min_e = min_e.min(e);
        max_e = max_e.max(e);
        if b.orientation != Quaternion::identity() {
            away_from_identity = true;
        }
        // 取向保持单位长（数值归一化无漂移）。
        assert!(
            (b.orientation.length() - 1.0).abs() < 1e-9,
            "四元数必须保持单位长，实际 |q|={}",
            b.orientation.length()
        );
        // 角速度守恒（自由旋转、无外力矩）。
        assert!(
            (b.angular_velocity.z - OMEGA).abs() < 1e-9,
            "自由旋转角速度应守恒"
        );
    }
    assert!(
        min_e > expected * 0.9999 && max_e < expected * 1.0001,
        "自由旋转能量应守恒：E∈[{min_e},{max_e}] vs {expected}"
    );
    assert!(away_from_identity, "取向应真实推进而非停留恒等（ω≠0 时）");
}

/// 无旋转自由度（inertia=0）：取向保持恒等、转动动能不参与能量。
#[test]
fn zero_inertia_does_not_rotate() {
    let mut k = PhysicalKernel::new(Vec3::zero());
    k.bodies.push(
        evorule_physics_services::kernel::RigidBody::new(1.0, Vec3::zero(), Vec3::zero())
            .with_rotation(Vec3::new(0.0, 0.0, OMEGA), 0.0), // inertia=0 ⇒ 无旋转
    );
    let e0 = k.total_mechanical_energy();
    assert!((e0).abs() < 1e-12, "无旋转自由度时能量应为 0，实际 {e0}");
    for _ in 0..1000 {
        k.tick(0.001);
    }
    assert!(
        k.bodies[0].orientation == Quaternion::identity(),
        "inertia=0 不得推进取向"
    );
}

/// 恒定外力矩：从静止起旋转，角速度按 α=τ/I 单调增长，取向真实推进且保持单位长。
#[test]
fn constant_torque_accelerates_rotation() {
    let tau = 0.2; // 恒定外力矩 τ（沿 z）
    let dt = 0.001;
    let steps = 2000;
    let mut k = PhysicalKernel::new(Vec3::zero());
    k.bodies.push(
        evorule_physics_services::kernel::RigidBody::new(1.0, Vec3::zero(), Vec3::zero())
            .with_rotation(Vec3::zero(), I),
    );
    k.set_external_torque(0, Vec3::new(0.0, 0.0, tau));
    for _ in 0..steps {
        k.tick(dt);
    }
    // α = τ/I = 0.2/0.4 = 0.5；t = steps·dt = 2s ⇒ ω = α·t = 1.0。
    let w = k.bodies[0].angular_velocity.z;
    let expected_w = (tau / I) * steps as f64 * dt;
    assert!(
        (w - expected_w).abs() < 1e-9,
        "角速度应按 α=τ/I 增长：src 期望 {expected_w}，实际 {w}"
    );
    // 取向保持单位长且离开恒等（确实在转）。
    let q = k.bodies[0].orientation;
    assert!(
        (q.length() - 1.0).abs() < 1e-9,
        "取向须单位长 |q|={}",
        q.length()
    );
    assert!(q != Quaternion::identity(), "施加外力矩后取向应离开恒等");
}

/// 外力矩：逐位确定性双跑一致（含注有力矩的角速度与取向轨迹）。
#[test]
fn torque_is_deterministic() {
    let run = || -> Vec<(Vec3, Vec3, Quaternion)> {
        let mut k = PhysicalKernel::new(Vec3::zero());
        k.bodies.push(
            evorule_physics_services::kernel::RigidBody::new(1.0, Vec3::zero(), Vec3::zero())
                .with_rotation(Vec3::new(0.1, -0.2, 0.3), I),
        );
        k.set_external_torque(0, Vec3::new(0.0, 0.0, 0.4));
        let mut trace = Vec::new();
        for _ in 0..2000 {
            k.tick(0.001);
            let b = &k.bodies[0];
            trace.push((b.angular_velocity, b.angular_velocity, b.orientation));
        }
        trace
    };
    let a = run();
    let c = run();
    assert_eq!(a.len(), c.len());
    for ((w1, _, q1), (w2, _, q2)) in a.iter().zip(c.iter()) {
        assert!(*w1 == *w2, "力矩积分角速度必须逐位确定");
        assert!(*q1 == *q2, "力矩积分取向必须逐位确定");
    }
}

/// 零力矩即自由旋转守恒：ks 默认无需注入任何力矩时行为不变。
/// 由 `free_rotation_conserves_energy_and_advances_orientation` 覆盖，此处仅作关系断言：
/// 注入力矩后，把力矩清空会让角速度停止增长（不再继续被 α·dt 加速）。
#[test]
fn clearing_torque_stops_acceleration() {
    let mut k = PhysicalKernel::new(Vec3::zero());
    k.bodies.push(
        evorule_physics_services::kernel::RigidBody::new(1.0, Vec3::zero(), Vec3::zero())
            .with_rotation(Vec3::zero(), I),
    );
    let tau = Vec3::new(0.0, 0.0, 0.2);
    let dt = 0.001;
    // 先加速 500 步。
    k.set_external_torque(0, tau);
    for _ in 0..500 {
        k.tick(dt);
    }
    let w_at_clear = k.bodies[0].angular_velocity.z;
    // 清空力矩后再跑 500 步：角速度不再增长（自由旋转守恒）。
    k.clear_external_torques();
    for _ in 0..500 {
        k.tick(dt);
    }
    let w_after = k.bodies[0].angular_velocity.z;
    assert!(
        (w_after - w_at_clear).abs() < 1e-9,
        "清空力矩后角速度应停止增长：{w_at_clear} → {w_after}"
    );
}

/// 绕 z 轴旋转 `θ` 的轴角四元数（测试辅助，构造目标取向）。
fn rotate_z(theta: f64) -> Quaternion {
    Quaternion {
        w: (theta / 2.0).cos(),
        x: 0.0,
        y: 0.0,
        z: (theta / 2.0).sin(),
    }
}

/// 取向误差角（rad）：`target ⊗ 当前⁻¹` 的旋转向量长度。
fn orient_error(target: Quaternion, q: Quaternion) -> f64 {
    (target * q.conjugate()).rotation_vector().length()
}

/// 纯角速度阻尼（k_ang=0，仅 c_ang>0）：自由角速度被耗散收敛到 0，而不发散。
#[test]
fn pure_angular_damping_stops_spin() {
    let mut k = PhysicalKernel::new(Vec3::zero());
    k.bodies.push(
        evorule_physics_services::kernel::RigidBody::new(1.0, Vec3::zero(), Vec3::zero())
            .with_rotation(Vec3::new(0.0, 0.0, 10.0), I)
            // 目标取向 + 刚度 0 + 有阻尼：`τ = -c_ang·ω`，纯耗散转速。
            .with_orientation_constraint(Quaternion::identity(), 0.0, 5.0, 0.0),
    );
    let e0 = k.total_mechanical_energy();
    let mut min_e = e0;
    let mut max_e = e0;
    for _ in 0..20000 {
        k.tick(0.001);
        let b = &k.bodies[0];
        let e = k.total_mechanical_energy();
        min_e = min_e.min(e);
        max_e = max_e.max(e);
        assert!((b.orientation.length() - 1.0).abs() < 1e-9, "取向须单位长");
    }
    let b = &k.bodies[0];
    assert!(
        b.angular_velocity.length() < 0.01,
        "纯角速度阻尼应收敛到 0，实际 |ω|={}",
        b.angular_velocity.length()
    );
    // 单调耗散多发散验证：能量不增放大（max_e 不明显超过初值 ± 数值误差）。
    assert!(
        max_e <= e0 + 1e-9,
        "纯阻尼不得自增能量：max_e={max_e} vs e0={e0}"
    );
    assert!(min_e >= 0.0, "能量不为负（下界 0）");
}

/// 有界姿态约束（PD）：偏离目标 1rad 的刚体被收敛回目标，且取向保持单位长。
#[test]
fn orientation_constraint_resets_to_target() {
    let target = rotate_z(1.0);
    let mut k = PhysicalKernel::new(Vec3::zero());
    k.bodies.push(
        evorule_physics_services::kernel::RigidBody::new(1.0, Vec3::zero(), Vec3::zero())
            .with_rotation(Vec3::zero(), I)
            .with_orientation_constraint(target, 50.0, 20.0, 1.0e6), // 过阻尼，上限极大≈无界
    );
    for _ in 0..8000 {
        k.tick(0.001);
        assert!((k.bodies[0].orientation.length() - 1.0).abs() < 1e-9);
    }
    let b = &k.bodies[0];
    assert!(
        orient_error(target, b.orientation) < 0.05,
        "取向应收敛到目标，剩余误差 {} rad",
        orient_error(target, b.orientation)
    );
    assert!(
        b.angular_velocity.length() < 0.05,
        "收敛后角速度应趋零，实际 |ω|={}",
        b.angular_velocity.length()
    );
}

/// 有界控制力矩（有限 max_torque）：超大误差 + 极强刚度下，力矩被钳制防炮散。
/// 本测试专验「不发散」（有界 vs 无界即炮散）：追踪全程能量（此处=转动动能）与角速度
/// 断言始终有界、取向保持单位长、且被拉向目标（未停在背离 π 处）。
/// 精确收敛到目标由 `orientation_constraint_resets_to_target` 覆盖；饱和 bang-bang 不追求精确到位。
#[test]
fn bounded_constraint_is_stable() {
    let target = rotate_z(2.0);
    let mut k = PhysicalKernel::new(Vec3::zero());
    k.bodies.push(
        evorule_physics_services::kernel::RigidBody::new(1.0, Vec3::zero(), Vec3::zero())
            .with_rotation(Vec3::zero(), I)
            .with_orientation_constraint(target, 1.0e6, 20.0, 0.3), // 力矩上限 0.3
    );
    let mut max_w = 0.0f64;
    let mut max_e = 0.0f64;
    for _ in 0..20000 {
        k.tick(0.001);
        let b = &k.bodies[0];
        let e = k.total_mechanical_energy();
        max_w = max_w.max(b.angular_velocity.length());
        max_e = max_e.max(e);
        assert!((b.orientation.length() - 1.0).abs() < 1e-9, "取向须单位长");
    }
    let b = &k.bodies[0];
    let err = orient_error(target, b.orientation);
    // 有界：不因强弹簧+大误差炮散能量/角速度（有界力矩 vs 无界即炮散）。
    assert!(max_w < 5.0, "有界约束下角速度不得超过上限：max|ω|={max_w}");
    assert!(max_e < 3.0, "有界约束下能量不得发散：max Er={max_e}");
    // 向目标拉拢：最终误差显著小于初值 2.0，且未停在背离 π（3.14）处。
    assert!(
        err < 2.5,
        "有界约束控制应拉起向目标收敛且不炮散：最终误差 {err} rad"
    );
}

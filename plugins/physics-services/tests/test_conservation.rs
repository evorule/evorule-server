//! [evorule 移植注记] 本文件自 rpsm-demo `rpsm/tests/test_conservation.rs`(2026-09-01 快照)移植为 evorule-physics-services 集成测试:import 改路(rpsm_core → evorule_physics_services::kernel),测试逻辑逐行保真。
//! 能量守恒验证用例。
//!
//! 修复点：旧用例用 `G = 6.67430e-11` 直接当重力（m/s²），系统几乎不动，
//! 漂移天然 ≈ 0，属空跑，并未真正检验守恒。现改为：
//!  - 自由落体用真实尺度重力（≈ 9.8 m/s²），两种积分器均真正运动；
//!  - 双体引力轨道用内核锁定的真实 `G` 构造圆轨道，验证含多体势能的
//!    总机械能守恒（速度 Verlet 优于辛欧拉，体现积分器选择的意义）；
//!  - 新增「积分器选择真正生效」断言（同初值、变加速度场下两积分器轨迹不同）。
//!
//! 物理注记（避免误判）：
//!  - **速度 Verlet** 对均匀重力场因位置更新含 ½a·dt² 项，与能量差精确抵消，
//!    机械能误差为机器精度量级（严格守恒）。
//!  - **辛欧拉（半隐式）** 对均匀场每步损失 ½·g²·dt²，N 步累计漂移
//!    ≈ N·½·g²·dt²（本用例 ≈ 0.48），能量长期有界但**不严格守恒**——故该
//!    用例验证「有界且不发散」，而非零漂移。
//!  - 双体轨道为变力场（N 体引力），真二阶速度 Verlet 的相对能量漂移为 O(dt²)，
//!    长期有界（辛积分器特性），容差取 2% 已远宽于实际量级。

// 集成测试保留 unwrap/expect 惯例（C5 unwrap/expect/panic = deny 仅约束生产代码）
#![allow(clippy::unwrap_used, clippy::expect_used)]

use evorule_physics_services::kernel::{PhysicalKernel, RigidBody, Vec3};

/// 自由落体能量漂移绝对值（给定积分器阶数）。
/// 恒定重力场下：速度 Verlet 严格守恒（drift≈0），辛欧拉有界（drift≈N·½g²dt²）。
fn free_fall_drift(order: u8) -> f64 {
    let mut kernel =
        PhysicalKernel::with_integrator(Vec3::new(0.0, -9.806_65, 0.0), order).expect("order 合法");
    kernel.bodies.push(RigidBody::new(
        1.0,
        Vec3::new(0.0, 1000.0, 0.0),
        Vec3::zero(),
    ));
    let e0 = kernel.total_mechanical_energy();
    for _ in 0..10_000u32 {
        kernel.tick(0.001);
    }
    let e1 = kernel.total_mechanical_energy();
    (e0 - e1).abs()
}

#[test]
fn test_conservation_free_fall_sympletic_euler() {
    let drift = free_fall_drift(1);
    // 辛欧拉对均匀场不严格守恒：理论漂移 ≈ 10000·½·9.80665²·0.001² ≈ 0.48。
    // 此处验证「能量有界、不发散」——证明测试在真实运动而非空跑。
    assert!(
        drift < 1.0,
        "辛欧拉自由落体能量应长期有界（非严格守恒），漂移过大: |ΔE| = {drift:e}"
    );
}

#[test]
fn test_conservation_free_fall_velocity_verlet() {
    let drift = free_fall_drift(2);
    // 速度 Verlet 对均匀场精确守恒（二次项抵消），drift 应为机器精度量级。
    assert!(
        drift < 1e-6,
        "速度 Verlet 自由落体能量漂移过大: |ΔE| = {drift:e}"
    );
}

/// 双体圆轨道：用内核锁定的真实万有引力常数 `G` 构造，验证含多体势能的
/// 总机械能守恒。两等质量天体绕质心做圆轨道。使用真二阶速度 Verlet。
#[test]
fn test_conservation_two_body_orbit() {
    use evorule_physics_services::kernel::G;

    let m = 1e10_f64; // 质量 kg
    let r = 1.0_f64; // 间距 m
                     // 相对轨道速度 v = sqrt(G (m1+m2) / r)，各天体以 v/2 绕质心。
    let v_rel = (G * (2.0 * m) / r).sqrt();
    let v_body = 0.5 * v_rel;

    let mut kernel = PhysicalKernel::with_integrator(Vec3::zero(), 2).expect("order 合法");
    // 质心在原点：天体位于 (±r/2, 0, 0)，初速沿 y。
    kernel.bodies.push(RigidBody::new(
        m,
        Vec3::new(-r / 2.0, 0.0, 0.0),
        Vec3::new(0.0, v_body, 0.0),
    ));
    kernel.bodies.push(RigidBody::new(
        m,
        Vec3::new(r / 2.0, 0.0, 0.0),
        Vec3::new(0.0, -v_body, 0.0),
    ));

    let e0 = kernel.total_mechanical_energy();
    let dt = 0.001;
    for _ in 0..10_000u32 {
        kernel.tick(dt);
    }
    let e1 = kernel.total_mechanical_energy();
    let drift = (e0 - e1).abs();
    let rel = drift / e0.abs().max(1e-12);
    // 速度 Verlet 为二阶辛积分器，相对漂移长期有界（O(dt²)）。取 2% 容差，
    // 远宽于实际量级，仅用于防护数值异常（如爆破/发散）。
    assert!(
        rel < 2e-2,
        "双体轨道相对能量漂移过大: |ΔE|/|E0| = {rel:e} (|ΔE|={drift:e}, E0={e0:e})"
    );
}

/// 积分器选择必须真正生效：同初值、变加速度场（弹簧 -100x）下，两种积分器
/// 的轨迹应不同。若 `integrator_order` 仍是死配置，两轨迹会重合，本断言失败。
#[test]
fn test_integrator_selection_takes_effect() {
    let mut k1 = PhysicalKernel::with_integrator(Vec3::zero(), 1).unwrap();
    let mut k2 = PhysicalKernel::with_integrator(Vec3::zero(), 2).unwrap();
    for k in [&mut k1, &mut k2] {
        k.bodies
            .push(RigidBody::new(1.0, Vec3::new(0.1, 0.0, 0.0), Vec3::zero()));
    }
    for _ in 0..200u32 {
        for k in [&mut k1, &mut k2] {
            let x = k.bodies[0].pos.x;
            k.set_external_force(0, Vec3::new(-100.0 * x, 0.0, 0.0));
            k.tick(0.01);
        }
    }
    let diff = (k1.bodies[0].pos.x - k2.bodies[0].pos.x).abs();
    assert!(
        diff > 1e-6,
        "两种积分器轨迹应不同（order 必须真正生效），实际 diff={diff:e}"
    );
}

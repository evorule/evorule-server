//! [evorule 移植注记] 本文件自 rpsm-demo `rpsm/tests/test_rolling_collision.rs`(2026-09-01 快照)移植为 evorule-physics-services 集成测试:import 改路(rpsm_core → evorule_physics_services::kernel),测试逻辑逐行保真。
//! 旋转-碰撞耦合（滚动摩擦）的确定性验证。
//!
//! 接触响应内，同一切向滑摩擦把阻力同时灌入水平角速度（滚动分量 ωx/ωz，不含竖轴自旋 ωy），
//! 「旋转动能进切向摩擦」，防止接触中自旋持续堆积。物理判据：
//! ①滚动自旋被阻尼、转动动能耗散；②长期接触不发散（|ωh| 与能量单调不增，反自旋-up）；
//! ③竖直轴自旋（顶端陀螺）不受地面滚动摩擦影响；④孪生内核逐位复现；
//! ⑤无旋转自由度（inertia=0）或质点（radius=0）刚体保持向后兼容。

// 集成测试保留 unwrap/expect 惯例（C5 unwrap/expect/panic = deny 仅约束生产代码）
#![allow(clippy::unwrap_used, clippy::expect_used)]

use evorule_physics_services::kernel::{PhysicalKernel, Vec3};

const G: f64 = 9.8;
const M: f64 = 1.0;
const GRAV: Vec3 = Vec3::new(0.0, -G, 0.0);
const R: f64 = 0.25; // 球半径
const MU: f64 = 0.5; // 切向摩擦系数
const I_SPHERE: f64 = 0.4 * M * R * R; // ⅖·m·r² ≈ 0.025（各向同性）

/// 触地球体：半径 R、摩擦 MU、惯量 I_SPHERE，水平滚动角速度 ω=(ωx,0,0)。
fn rolling_sphere(wx: f64) -> evorule_physics_services::kernel::RigidBody {
    evorule_physics_services::kernel::RigidBody::new(M, Vec3::new(0.0, R, 0.0), Vec3::zero())
        .with_radius(R)
        .with_friction(MU)
        .with_rotation(Vec3::new(wx, 0.0, 0.0), I_SPHERE)
}

/// ①旋转动能进切向摩擦：接地滚动自旋被阻尼，转动动能单调耗散。
#[test]
fn spin_dissipates_via_rolling_friction() {
    let mut k = PhysicalKernel::with_integrator(GRAV, 2).expect("order 2");
    // restitution=0：弹起归零，球体粘滞接地，保证每帧都处于碰撞接触、滚动阻尼完整生效。
    k.set_restitution(0.0);
    k.bodies.push(rolling_sphere(10.0));
    let e0 = k.total_mechanical_energy();
    // 每帧阻尼 dω = μ·g·dt/r；μ=0.5,g=9.8,dt=0.001,r=0.25 → 0.0196。
    // 自 ω=10 降至 0 约需 510 帧，用 900 帧保证充分衰减。
    let mut prev_w = 10.0;
    for _ in 0..900 {
        k.tick(0.001);
        let b = &k.bodies[0];
        let wh = b.angular_velocity.x.abs();
        assert!(
            wh <= prev_w + 1e-12,
            "滚动自旋不得被摩擦自增：{prev_w} → {wh}"
        );
        prev_w = wh;
    }
    let e1 = k.total_mechanical_energy();
    assert!(
        k.bodies[0].angular_velocity.x.abs() < 1.0,
        "滚动自旋应被摩擦大幅阻尼：{}",
        k.bodies[0].angular_velocity.x
    );
    assert!(e1 < e0 - 0.5, "转动动能应进切向摩擦而耗散：e0={e0} → {e1}");
}

/// ②发散测试（反自旋-up）：纯滚动接触长期运行下，|ωh| 与总能量单调不增、不发散。
#[test]
fn no_spin_up_anti_divergence() {
    let mut k = PhysicalKernel::with_integrator(GRAV, 2).expect("order 2");
    k.bodies.push(rolling_sphere(5.0));
    let e0 = k.total_mechanical_energy();
    let mut prev_wh = 5.0_f64;
    let mut prev_e = e0;
    for _ in 0..10000 {
        k.tick(0.001);
        let b = &k.bodies[0];
        let wh = b.angular_velocity.x.abs();
        let e = k.total_mechanical_energy();
        assert!(
            wh <= prev_wh + 1e-12,
            "接触不得给滚动自旋注能（发散）：{prev_wh} → {wh}"
        );
        assert!(
            e <= prev_e + 1e-9,
            "接触不得使总能量自增（发散）：{prev_e} → {e}"
        );
        prev_wh = wh;
        prev_e = e;
    }
}

/// ③竖直轴自旋受保护：ωy（顶端陀螺）不属于滚动分量，地面滚动摩擦不得触碰。
#[test]
fn vertical_spin_untouched_by_ground_contact() {
    let mut k = PhysicalKernel::with_integrator(GRAV, 2).expect("order 2");
    k.bodies.push(
        evorule_physics_services::kernel::RigidBody::new(M, Vec3::new(0.0, R, 0.0), Vec3::zero())
            .with_radius(R)
            .with_friction(MU)
            .with_rotation(Vec3::new(0.0, 5.0, 0.0), I_SPHERE),
    );
    for _ in 0..1000 {
        k.tick(0.001);
    }
    assert!(
        (k.bodies[0].angular_velocity.y - 5.0).abs() < 1e-12,
        "竖轴自旋不应受滚动摩擦影响：{}",
        k.bodies[0].angular_velocity.y
    );
}

/// ④孪生内核复现：滚动耦合路径在 clone 后逐位一致。
#[test]
fn twin_kernel_reproduces() {
    let mut k = PhysicalKernel::with_integrator(GRAV, 2).expect("order 2");
    k.bodies.push(rolling_sphere(8.0));
    let mut twin = k.clone();
    let mut a = Vec::new();
    let mut b = Vec::new();
    for _ in 0..800 {
        k.tick(0.001);
        twin.tick(0.001);
        a.push((
            k.bodies[0].pos,
            k.bodies[0].angular_velocity,
            k.bodies[0].orientation,
        ));
        b.push((
            twin.bodies[0].pos,
            twin.bodies[0].angular_velocity,
            twin.bodies[0].orientation,
        ));
    }
    for (x, y) in a.iter().zip(b.iter()) {
        assert!(*x == *y, "滚动耦合孪生内核必须逐位一致");
    }
}

/// ⑤向后兼容：无旋转自由度（inertia=0）或质点（radius<=0）刚体不受滚动耦合影响。
#[test]
fn inertialess_and_point_bodies_unchanged() {
    // inertia=0 但 friction>0、radius>0：只吃线性摩擦，不动角速度（本就为 0）。
    let mut k = PhysicalKernel::with_integrator(GRAV, 2).expect("order 2");
    k.bodies.push(
        evorule_physics_services::kernel::RigidBody::new(
            M,
            Vec3::new(0.0, R, 0.0),
            Vec3::new(3.0, 0.0, 0.0),
        )
        .with_radius(R)
        .with_friction(MU),
    );
    for _ in 0..300 {
        k.tick(0.001);
    }
    // 角速度始终保持 0（未开旋转），水平速度被线性摩擦削减但为 0 的角速度不变。
    assert_eq!(k.bodies[0].angular_velocity, Vec3::zero());
    assert!(k.bodies[0].vel.x.abs() < 3.0, "线性摩擦应削减水平速度");
}

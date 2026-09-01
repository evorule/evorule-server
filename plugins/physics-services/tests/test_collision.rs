//! [evorule 移植注记] 本文件自 rpsm-demo `rpsm/tests/test_collision.rs`(2026-09-01 快照)移植为 evorule-physics-services 集成测试:import 改路(rpsm_core → evorule_physics_services::kernel),测试逻辑逐行保真。
//! [evorule 移植等效] 原用例 `test_hci_default_restitution_is_0_8` 依赖 rpsm_hci
//! 的 `HciConfig::default()`(面板缺省恢复系数 0.8,该 crate 未随内核 vendored),
//! 以内核缺省恢复系数 0.8 断言等效替代——二者共同构成「缺省 0.8」行为契约的
//! 内核侧验证面。
//! 阶段一：地面碰撞 + 恢复系数的行为验证。
//! 采用行为断言而非数值锁定：验证三种恢复系数下的宏观行为符合物理直觉。

// 集成测试保留 unwrap/expect 惯例（C5 unwrap/expect/panic = deny 仅约束生产代码）
#![allow(clippy::unwrap_used, clippy::expect_used)]

use evorule_physics_services::kernel::{PhysicalKernel, RigidBody, Vec3};

const G_EARTH: f64 = 9.80665;

/// 构造一个从高度 `height` 静止下落、带半径的刚体。
fn falling_ball(height: f64, radius: f64) -> PhysicalKernel {
    let mut kernel = PhysicalKernel::new(Vec3::new(0.0, -G_EARTH, 0.0));
    kernel
        .bodies
        .push(RigidBody::new(1.0, Vec3::new(0.0, height, 0.0), Vec3::zero()).with_radius(radius));
    kernel
}

/// 恢复系数 0.8：多次弹跳且高度递减，最终贴地（运动趋于停止）。
#[test]
fn test_restitution_0_8_bounces_then_settles() {
    let mut kernel = falling_ball(1.0, 0.05);
    kernel.set_restitution(0.8);

    // 60 s = 60_000 步 × 0.001 s，足够弹跳能量衰减并贴地。
    for _ in 0..60_000u32 {
        kernel.tick(0.001);
    }
    let body = &kernel.bodies[0];
    assert!(
        (body.pos.y - body.radius).abs() < 1e-6,
        "小球应最终贴地，pos.y={} radius={}",
        body.pos.y,
        body.radius
    );
    // 允许贴地微小振动（近 0.005 m/s 量级），验证不再产生明显弹跳。
    assert!(
        body.vel.y.abs() < 0.05,
        "小球应趋于静止，vel.y={}",
        body.vel.y
    );
}

/// 恢复系数 1.0（弹性碰撞）：机械能应基本守恒，小球持续弹跳不衰减。
#[test]
fn test_restitution_1_never_stops() {
    let mut kernel = falling_ball(1.0, 0.05);
    kernel.set_restitution(1.0);

    // 初始总机械能（静止于 1.0 m，含半径高度位移）。
    let e0 = kernel.total_mechanical_energy();
    let mut max_vy = 0.0_f64;
    for _ in 0..20_000u32 {
        kernel.tick(0.001);
        max_vy = max_vy.max(kernel.bodies[0].vel.y.abs());
    }
    let e = kernel.total_mechanical_energy();
    // 弹性：能量相对衰减不超过 5%（含碰撞位置归正引入的微小误差）。
    let rel = (e - e0).abs() / e0.max(1e-12);
    assert!(rel < 0.05, "弹性碰撞能量应基本守恒，相对偏差 {rel:.3e}");
    // 速度没有明显衰减，证明小球仍在持续弹跳。
    assert!(max_vy > 1.0, "弹性碰撞应保持大速度弹跳，max|vy|={max_vy}",);
}

/// 恢复系数 0.0（完全非弹性碰撞）：落地后立即静止。
#[test]
fn test_restitution_0_lands_and_stops() {
    let mut kernel = falling_ball(1.0, 0.05);
    kernel.set_restitution(0.0);

    for _ in 0..2_000u32 {
        kernel.tick(0.001);
    }
    let body = &kernel.bodies[0];
    assert!(
        (body.pos.y - body.radius).abs() < 1e-6,
        "完全非弹性应贴地，pos.y={}",
        body.pos.y
    );
    assert_eq!(body.vel.y, 0.0, "完全非弹性落地点速度应为 0");
}

/// 缺省恢复系数 0.8（移植等效）：内核 `PhysicalKernel::new` 的缺省恢复系数
/// 与 rpsm_hci 面板缺省值（未 vendored）一致，均为 0.8。
#[test]
fn test_kernel_default_restitution_is_0_8() {
    let kernel = PhysicalKernel::new(Vec3::new(0.0, -G_EARTH, 0.0));
    assert!((kernel.restitution() - 0.8).abs() < 1e-12);
}

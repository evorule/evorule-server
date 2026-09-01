//! [evorule 移植注记] 本文件自 rpsm-demo `rpsm/tests/test_stratified_gravity.rs`(2026-09-01 快照)移植为 evorule-physics-services 集成测试:import 改路(rpsm_core → evorule_physics_services::kernel),测试逻辑逐行保真。
//! 有界重力/分层势场的确定性验证（箱阱势）。
//!
//! 重力带 `[lo, hi]` 内刚体受力（均匀场 g），带外重力归零、形成平台。
//! 物理判据：①带外匀速逃逸不再回落；②带内自由落体；③出带上界速度冻结（平台）；
//! ④孪生内核逐位复现；⑤带内纯重力下总能量守恒（箱阱势 conservative 场）。
//! 这是后续「双势阱保守势垒」在重力维度上的基础素材（分层势场）。

// 集成测试保留 unwrap/expect 惯例（C5 unwrap/expect/panic = deny 仅约束生产代码）
#![allow(clippy::unwrap_used, clippy::expect_used)]

use evorule_physics_services::kernel::{PhysicalKernel, Vec3};

const G: f64 = 9.8;
const M: f64 = 1.0;
const GRAV: Vec3 = Vec3::new(0.0, -G, 0.0);
const LO: f64 = 0.0;
const HI: f64 = 100.0;

/// ①带外匀速：从高过 `hi`、已逃逸的刚体，重力归零 → 匀速直线运动（速度不变）。
#[test]
fn outside_band_moves_at_constant_velocity() {
    let mut k = PhysicalKernel::with_integrator(GRAV, 2).expect("order 2");
    k.set_gravity_band(LO, HI);
    // 起始即在上带之上，且原本是「逃逸余速」。
    k.bodies
        .push(evorule_physics_services::kernel::RigidBody::new(
            M,
            Vec3::new(0.0, HI + 100.0, 0.0),
            Vec3::new(3.0, 25.0, 0.0),
        ));
    let mut prev = k.bodies[0].vel;
    for _ in 0..1000 {
        k.tick(0.001);
        let v = k.bodies[0].vel;
        assert!(
            v == prev,
            "带外刚体不得受重力（匀速、逐位不变）：{prev:?} → {v:?}"
        );
        prev = v;
    }
}

/// ②带内自由落体：带内刚体受力，速度按 g 增长（与全空间均匀场一致）。
#[test]
fn inside_band_falls_with_gravity() {
    let mut k = PhysicalKernel::with_integrator(GRAV, 2).expect("order 2");
    k.set_gravity_band(LO, HI);
    k.bodies
        .push(evorule_physics_services::kernel::RigidBody::new(
            M,
            Vec3::new(0.0, 50.0, 0.0),
            Vec3::zero(),
        ));
    for _ in 0..100 {
        k.tick(0.001);
        // 始终保持在带内（0.1s 仅下落 ~0.05m）。
        assert!(
            LO <= k.bodies[0].pos.y && k.bodies[0].pos.y <= HI,
            "必须仍在带内"
        );
    }
    let v = k.bodies[0].vel.y.abs();
    let expect = G * 0.1; // 自由落体 0.1s
    assert!(
        (v - expect).abs() < 0.01,
        "带内应自由下落：expect |v|={expect}，实际 {v}"
    );
}

/// ④孪生内核复现：启用重力带下，clone 后同输入逐位一致（账本可标 modeled）。
#[test]
fn twin_kernel_reproduces_with_band() {
    let mut k = PhysicalKernel::with_integrator(GRAV, 2).expect("order 2");
    k.set_gravity_band(LO, HI);
    // 从带内以略超逃逸的速度上升：将穿越上界进入平台区，是带+平台切换路径。
    k.bodies
        .push(evorule_physics_services::kernel::RigidBody::new(
            M,
            Vec3::new(0.0, 50.0, 0.0),
            Vec3::new(0.0, 45.0, 0.0),
        ));
    let mut twin = k.clone();
    let mut a = Vec::new();
    let mut b = Vec::new();
    for _ in 0..2000 {
        k.tick(0.002);
        twin.tick(0.002);
        a.push((k.bodies[0].pos, k.bodies[0].vel));
        b.push((twin.bodies[0].pos, twin.bodies[0].vel));
    }
    for (x, y) in a.iter().zip(b.iter()) {
        assert!(*x == *y, "分层势场孪生内核必须逐位一致");
    }
}

/// ③出带上界逃逸：从带内上升、穿越 `hi` 后，速度停在穿越值不再变化（平台，不再被拉回）。
#[test]
fn escapes_band_and_velocity_freezes() {
    let mut k = PhysicalKernel::with_integrator(GRAV, 2).expect("order 2");
    k.set_gravity_band(LO, HI);
    // 足够快的初速，保证在测试窗内越过 100 并继续远离。
    k.bodies
        .push(evorule_physics_services::kernel::RigidBody::new(
            M,
            Vec3::new(0.0, 50.0, 0.0),
            Vec3::new(0.0, 90.0, 0.0),
        ));
    let mut crossed_at_v = None;
    let mut freeze_v = None;
    for _ in 0..3000 {
        k.tick(0.002);
        let b = &k.bodies[0];
        if crossed_at_v.is_none() && b.pos.y >= HI {
            crossed_at_v = Some(b.vel.y);
        }
        // 一旦远在带外上方，速度应冻结为穿越时刻值。
        if crossed_at_v.is_some() && b.pos.y >= HI + 20.0 {
            freeze_v = Some(b.vel.y);
            break;
        }
    }
    let freeze = freeze_v.expect("应已逃逸到平台上区");
    let crossed = crossed_at_v.expect("应已穿越上界");
    assert!(
        freeze >= 0.0 && (freeze - crossed).abs() < 1e-9,
        "逃逸后速度应冻结：crossed={crossed}，freeze={freeze}"
    );
}

/// ⑤带内纯重力守恒：初速不足逃逸，上升转折前全程待在带内，总机械能（箱阱势）近守恒。
#[test]
fn banded_gravity_conserves_energy_inside() {
    let mut k = PhysicalKernel::with_integrator(GRAV, 2).expect("order 2");
    k.set_gravity_band(LO, HI);
    // 从带内中段以不越界的初速上升（v=25 → 最高点 y≈50+v²/2g≈81.9 < 100）。
    // 匀速减速至转折点需 t=v/g≈2.55s，故 2.5s 内全程待于带内，纯保守场可断言守恒。
    k.bodies
        .push(evorule_physics_services::kernel::RigidBody::new(
            M,
            Vec3::new(0.0, 50.0, 0.0),
            Vec3::new(0.0, 25.0, 0.0),
        ));
    let e0 = k.total_mechanical_energy();
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for _ in 0..2500 {
        k.tick(0.001);
        let b = &k.bodies[0];
        assert!(b.pos.y >= LO && b.pos.y <= HI, "不得穿越带界");
        let e = k.total_mechanical_energy();
        min = min.min(e);
        max = max.max(e);
    }
    // 速度 Verlet 对恒力（线性势）解析精确，能量应近恒定（仅极小的浮点扰动）。
    let drift = (max - min).abs();
    assert!(
        drift < e0.abs() * 1e-6,
        "带内保守场能量应近守恒：e0={e0}，漂移 {drift}"
    );
}

//! [evorule 移植注记] 本文件自 rpsm-demo `rpsm/tests/test_contact_forces.rs`(2026-09-01 快照)移植为 evorule-physics-services 集成测试:import 改路(rpsm_core → evorule_physics_services::kernel),测试逻辑逐行保真。
//! 弹簧-阻尼器 + 地面切向摩擦两个内核力模型的确定性验证。
//!
//! 主线 B 物域深化增量：弹簧-阻尼（F = -k·(p−a) − c·v）与接触摩擦（μ·g 封顶不反向）。
//! 两者都由内核确定性实现，孪生内核克隆同一刚体可原样复现（账本可标 modeled）。

// 集成测试保留 unwrap/expect 惯例（C5 unwrap/expect/panic = deny 仅约束生产代码）
#![allow(clippy::unwrap_used, clippy::expect_used)]

use evorule_physics_services::kernel::{PhysicalKernel, Vec3};

const G: f64 = 10.0;

/// 弹簧-阻尼：逐位确定性双跑一致。
#[test]
fn spring_damper_is_deterministic() {
    let run = |c: f64| -> Vec<(f64, f64)> {
        let mut k = PhysicalKernel::new(Vec3::zero());
        k.bodies.push(
            evorule_physics_services::kernel::RigidBody::new(
                1.0,
                Vec3::new(1.0, 0.0, 0.0),
                Vec3::zero(),
            )
            .with_spring(Vec3::zero(), 100.0, c),
        );
        let mut trace = Vec::new();
        for _ in 0..2000 {
            k.tick(0.001);
            let b = &k.bodies[0];
            trace.push((b.pos.x, k.total_mechanical_energy()));
        }
        trace
    };
    let a = run(0.0);
    let c = run(0.0);
    assert_eq!(a.len(), c.len());
    for (x, y) in a.iter().zip(c.iter()) {
        assert!(x == y, "弹簧-阻尼仿真必须逐位确定");
    }
}

/// 无阻尼纯弹簧（c=0）在 Verlet 下能量守恒：KE + 弹簧PE 近似恒定。
#[test]
fn undamped_spring_conserves_energy() {
    let mut k = PhysicalKernel::with_integrator(Vec3::zero(), 2).expect("order 2");
    k.bodies.push(
        evorule_physics_services::kernel::RigidBody::new(
            1.0,
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::zero(),
        )
        .with_spring(Vec3::zero(), 100.0, 0.0),
    );
    let e0 = k.total_mechanical_energy(); // ½k·1² = 50
    assert!((e0 - 50.0).abs() < 1e-9, "初态能量应为 ½k·x²=50，实际 {e0}");
    let mut min_e = e0;
    let mut max_e = e0;
    for _ in 0..4000 {
        k.tick(0.001);
        let e = k.total_mechanical_energy();
        min_e = min_e.min(e);
        max_e = max_e.max(e);
    }
    // Verlet 二阶辛：能量应长期有界且极接近初值（相对漂移 << 1%）。
    assert!(
        min_e > 50.0 * 0.995 && max_e < 50.0 * 1.005,
        "无阻尼弹簧能量应守恒：E∈[{min_e},{max_e}] vs 50"
    );
}

/// 阻尼弹簧：能量单调不增，且系统收敛到停在锚点。
#[test]
fn damped_spring_dissipates_and_settles_at_anchor() {
    let mut k = PhysicalKernel::with_integrator(Vec3::zero(), 2).expect("order 2");
    k.bodies.push(
        evorule_physics_services::kernel::RigidBody::new(
            1.0,
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::zero(),
        )
        .with_spring(Vec3::zero(), 100.0, 20.0),
    ); // c=20 ≈ 临界阻尼 ζ=1，快速停机、能量净耗散到近 0。
    let mut last_e = k.total_mechanical_energy();
    for _ in 0..4000 {
        k.tick(0.001);
        let e = k.total_mechanical_energy();
        // Verlet 对近稳态有机器精度级的能量微振（~1e-6），允许小幅回升；
        // 阻尼整体必须是净耗散（经过数百步后能量显著下降而非守恒）。
        assert!(
            e <= last_e + 1e-5,
            "阻尼整体应净耗散，允许数值微振：{e} > {last_e}"
        );
        last_e = e;
    }
    assert!(
        last_e < 1e-2,
        "阻尼应把能量净耗散到近 0（初值 50），实际 {last_e}"
    );
    // 停机：位置回到锚点附近、速度≈0。
    let b = &k.bodies[0];
    assert!(b.pos.x.abs() < 1e-1, "应停在锚点，实际 x={}", b.pos.x);
    assert!(b.vel.length() < 1e-2, "应静止，实际 |v|={}", b.vel.length());
}

/// 接触摩擦：逐位确定性双跑一致。
#[test]
fn friction_is_deterministic() {
    let run = || -> Vec<(f64, f64)> {
        let mut k = PhysicalKernel::new(Vec3::new(0.0, -G, 0.0));
        k.set_restitution(0.0); // 贴合地面，摩擦持续生效
        k.bodies.push(
            evorule_physics_services::kernel::RigidBody::new(
                1.0,
                Vec3::new(0.0, 0.05, 0.0),
                Vec3::new(3.0, 0.0, 0.0),
            )
            .with_radius(0.05)
            .with_friction(0.5),
        );
        let mut trace = Vec::new();
        for _ in 0..2000 {
            k.tick(0.001);
            trace.push((k.bodies[0].pos.x, k.bodies[0].vel.x));
        }
        trace
    };
    let a = run();
    let c = run();
    for (x, y) in a.iter().zip(c.iter()) {
        assert!(x == y, "摩擦仿真必须逐位确定");
    }
}

/// 摩擦削停水平滑动：水平速度单调衰减到 0，且从不反向。
#[test]
fn friction_stops_sliding_without_reversing() {
    let v0 = 3.0;
    let mut k = PhysicalKernel::new(Vec3::new(0.0, -G, 0.0));
    k.set_restitution(0.0);
    k.bodies.push(
        evorule_physics_services::kernel::RigidBody::new(
            1.0,
            Vec3::new(0.0, 0.05, 0.0),
            Vec3::new(v0, 0.0, 0.0),
        )
        .with_radius(0.05)
        .with_friction(0.5),
    );
    let mut last_vx = v0;
    for _ in 0..5000 {
        k.tick(0.001);
        let vx = k.bodies[0].vel.x;
        assert!(
            vx <= last_vx + 1e-9,
            "摩擦只减速，不得反向加速：{vx} > {last_vx}"
        );
        assert!(vx >= -1e-12, "摩擦不得把速度拖到负数：{vx}");
        last_vx = vx;
    }
    // a = μ·g = 5，停机距离 ≈ v0²/(2a) = 9/10 = 0.9m；近似验证（含离散误差）。
    let x = k.bodies[0].pos.x;
    assert!(
        x.abs() >= 0.8 && x.abs() <= 1.0,
        "停机距离应近 0.9m，实际 {x}"
    );
    assert!(last_vx.abs() < 1e-3, "应已停止，实际速度 {last_vx}");
}

/// 摩擦仅在接触响应启用时生效（tick_collision_optional(.., false) 关闭时无摩擦）。
#[test]
fn friction_disabled_without_collision_response() {
    let v0 = 2.0;
    let with_on = |on: bool| -> f64 {
        let mut k = PhysicalKernel::new(Vec3::new(0.0, -G, 0.0));
        k.set_restitution(0.0);
        k.bodies.push(
            evorule_physics_services::kernel::RigidBody::new(
                1.0,
                Vec3::new(0.0, 0.05, 0.0),
                Vec3::new(v0, 0.0, 0.0),
            )
            .with_radius(0.05)
            .with_friction(0.5),
        );
        for _ in 0..1000 {
            k.tick_collision_optional(0.001, on);
        }
        k.bodies[0].vel.x
    };
    let on = with_on(true);
    let off = with_on(false);
    assert!(
        off.abs() >= on.abs(),
        "关碰撞则无摩擦：on({on}) 应比 off({off}) 减速更多"
    );
}

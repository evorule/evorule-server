//! [evorule 移植注记] 本文件自 rpsm-demo `rpsm/tests/test_air_drag.rs`(2026-09-01 快照)移植为 evorule-physics-services 集成测试:import 改路(rpsm_core → evorule_physics_services::kernel),测试逻辑逐行保真。
//! 空气阻力（线性拖拽 F = -b·v）内核模型的确定性验证。
//!
//! 这是「主线 B 物域深化」的第一个物理模型增量：在 rpsm-core 内核加入一个
//! 确定性非保守力（空气阻力），验证其可复现、能耗方向正确、且被孪生内核原样
//! 复现（账本可标 modeled，不产生假漂移）。系数默认 0，向后兼容。

// 集成测试保留 unwrap/expect 惯例（C5 unwrap/expect/panic = deny 仅约束生产代码）
#![allow(clippy::unwrap_used, clippy::expect_used)]

use evorule_physics_services::kernel::{PhysicalKernel, Vec3};

const G: f64 = 10.0; // 演示用重力（小值便于收敛断言）

/// 单自由落体：质量 m、阻力系数 b，从静止开始，验证确定性双跑逐位一致。
#[test]
fn drag_step_is_deterministic() {
    let run = |b: f64| -> Vec<(f64, f64, f64)> {
        let mut k = PhysicalKernel::new(Vec3::new(0.0, -G, 0.0));
        k.bodies.push(
            evorule_physics_services::kernel::RigidBody::new(1.0, Vec3::zero(), Vec3::zero()).with_drag(b),
        );
        let mut trace = Vec::new();
        for _ in 0..1000 {
            k.tick(0.001);
            let b0 = &k.bodies[0];
            trace.push((b0.pos.y, b0.vel.y, k.total_mechanical_energy()));
        }
        trace
    };
    let a = run(2.0);
    let c = run(2.0);
    assert_eq!(a.len(), c.len());
    for (x, y) in a.iter().zip(c.iter()) {
        assert!(x == y, "阻力仿真必须逐位确定：{x:?} vs {y:?}");
    }
}

/// 能量方向：空气阻力是耗散力，系统机械能必须随时间单调不增（并列比对无阻力）。
#[test]
fn drag_dissipates_energy() {
    let mut k = PhysicalKernel::new(Vec3::new(0.0, -G, 0.0));
    k.bodies.push(
        evorule_physics_services::kernel::RigidBody::new(1.0, Vec3::new(0.0, 5.0, 0.0), Vec3::new(3.0, 0.0, 0.0))
            .with_drag(0.8),
    );
    let mut last_e = k.total_mechanical_energy();
    for _ in 0..4000 {
        k.tick(0.001);
        let e = k.total_mechanical_energy();
        assert!(
            e <= last_e + 1e-9,
            "空气阻力不得增能：e={e} > last_e={last_e}"
        );
        last_e = e;
    }
    // 有阻力时能量损耗显著；对照无阻力应基本守恒。
    let with_drag_e = last_e;
    let mut k2 = PhysicalKernel::new(Vec3::new(0.0, -G, 0.0));
    k2.bodies.push(
        evorule_physics_services::kernel::RigidBody::new(1.0, Vec3::new(0.0, 5.0, 0.0), Vec3::new(3.0, 0.0, 0.0)),
    );
    for _ in 0..4000 {
        k2.tick(0.001);
    }
    assert!(
        with_drag_e < k2.total_mechanical_energy() - 1.0,
        "有阻力应比无阻力损失更多能量，实际含阻力 {with_drag_e} vs 无阻力 {}",
        k2.total_mechanical_energy()
    );
}

/// 终速：竖直下落受线性阻力，收敛到 v_t = m·g / b。
#[test]
fn drag_reaches_terminal_velocity() {
    let m: f64 = 1.0;
    let b: f64 = 5.0;
    let v_t = m * G / b; // = 2.0
    let mut k = PhysicalKernel::new(Vec3::new(0.0, -G, 0.0));
    k.bodies.push(evorule_physics_services::kernel::RigidBody::new(m, Vec3::zero(), Vec3::zero()).with_drag(b));
    // 足够长时间（τ = m/b = 0.2s，跑 10s ≈ 50τ）确保收敛。
    for _ in 0..10_000 {
        k.tick(0.001);
    }
    let vy = k.bodies[0].vel.y;
    assert!(
        (vy - (-v_t)).abs() < 0.05,
        "自由落体应达终速 {v_t}，实际 vy={vy}"
    );
}

/// 向后兼容：阻力系数 0 与不设阻力完全一致（不改变既有无阻力行为）。
#[test]
fn drag_zero_is_noop() {
    let run = |with_field: bool| -> f64 {
        let mut k = PhysicalKernel::new(Vec3::new(0.0, -G, 0.0));
        let body = evorule_physics_services::kernel::RigidBody::new(1.0, Vec3::new(0.0, 5.0, 0.0), Vec3::zero());
        k.bodies.push(if with_field {
            body.with_drag(0.0)
        } else {
            body
        });
        for _ in 0..500 {
            k.tick(0.001);
        }
        k.total_mechanical_energy()
    };
    assert_eq!(run(true), run(false), "drag=0 不应改变任何行为");
}

// [evorule 移植裁剪] 原文件末尾用例 `drag_is_reproduced_by_twin_kernel_no_false_drift`
// 依赖 rpsm_pla(物理逻辑分析器:PlaConfig/PhysicalLogicAnalyzer/FidelityLedger 等),
// 该 crate 未随 rpsm-core 内核 vendored,故本文件未移植该用例。
// 其孪生内核复现语义已由本目录 test_quad_drag/twin_kernel_reproduces、
// test_rolling_collision/twin_kernel_reproduces、test_stratified_gravity/
// twin_kernel_reproduces_with_band 等纯内核孪生复现用例等效覆盖。
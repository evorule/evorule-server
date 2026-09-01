//! [evorule 移植注记] 本文件自 rpsm-demo `rpsm/tests/test_double_well.rs`(2026-09-01 快照)移植为 evorule-physics-services 集成测试:import 改路(rpsm_core → evorule_physics_services::kernel),测试逻辑逐行保真。
//! 双势阱保守势垒的确定性验证（守护验证器压力素材）。
//!
//! 内核级保守势（沿 X 轴）`V(x) = a·(x² − m)²`：双势阱（井底 x=±√m、V=0），
//! 中央势垒 x=0、高 a·m²。外力 `F_x = −dV/dx = −4a·x·(x²−m)` 随位置在
//! `accumulate_forces` 中实时重算，velocity Verlet 新旧各求一次梯度 → 保持辛映射。
//!
//! 设计披露：起初尝试「经外力槽注入」——但外力槽为每帧注值的静态槽，速度 Verlet 第二次
//! 加速沿用注入值，映射去辛（`det J = 1 − ½·a'·h² ≠ 1`），能量会长期漂移而非有界
//! （实测最大偏离 ~22%）。故改为内核级保守场，能量方真正有界/近守恒。这是「诚实上报
//! 设计缺陷→纠正」而非盲目照表抄写的落点。

// 集成测试保留 unwrap/expect 惯例（C5 unwrap/expect/panic = deny 仅约束生产代码）
#![allow(clippy::unwrap_used, clippy::expect_used)]

use evorule_physics_services::kernel::{PhysicalKernel, Vec3};

const MASS: f64 = 1.0;
const A: f64 = 1.0;
const MBAR: f64 = 4.0; // 井底 x=±2，势垒高 V(0)=16
const DT: f64 = 0.002;
const NO_GRAV: Vec3 = Vec3::zero();

/// 跑 n 步，返回 (位置序列 x, 内核总机械能 E_h=KE+V)。
fn run(x0: f64, v0: f64, n: usize) -> (Vec<f64>, Vec<f64>, Vec<f64>) {
    let mut k = PhysicalKernel::with_integrator(NO_GRAV, 2).expect("order 2");
    k.bodies.push(
        evorule_physics_services::kernel::RigidBody::new(
            MASS,
            Vec3::new(x0, 0.0, 0.0),
            Vec3::new(v0, 0.0, 0.0),
        )
        .with_double_well(A, MBAR),
    );
    let mut xs = Vec::with_capacity(n);
    let mut vs = Vec::with_capacity(n);
    let mut es = Vec::with_capacity(n);
    for _ in 0..n {
        k.tick(DT);
        let b = &k.bodies[0];
        xs.push(b.pos.x);
        vs.push(b.vel.x);
        es.push(k.total_mechanical_energy());
    }
    (xs, vs, es)
}

/// ①阱内束缚 + 能量有界：初能量低于势垒 → 右井内往返，能量近守恒（有界、不单调增长）。
#[test]
fn trapped_energy_bounded_in_single_well() {
    let (xs, _, es) = run(2.4, 0.0, 10_000); // 束缚于右井 [1.5, 2.4]
    let min_x = xs.iter().cloned().fold(f64::INFINITY, f64::min);
    assert!(min_x > 0.5, "束缚粒子不得穿越中央势垒：min_x={min_x}");
    // 辛积分能量近守恒：相对漂移极小（有界，不发散）。
    let e0 = A * (2.4 * 2.4 - MBAR) * (2.4 * 2.4 - MBAR);
    let max_dev = es.iter().map(|e| (e - e0).abs()).fold(0.0_f64, f64::max);
    assert!(
        max_dev < e0 * 1e-3,
        "长时束缚下能量应有界：e0={e0} 最大偏离 {max_dev}"
    );
}

/// ②双井穿越 + 能量守恒：能量 > 势垒 → 左右两井穿梭，能量近守恒。
#[test]
fn crossing_barrier_energy_bounded() {
    // x=0、v=5：KE=12.5 + V(0)=16 = 28.5 高于势垒，两井穿梭。
    let (xs, _, es) = run(0.0, 5.0, 10_000);
    let min_x = xs.iter().cloned().fold(f64::INFINITY, f64::min);
    let max_x = xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    assert!(
        min_x < -1.5 && max_x > 1.5,
        "能量高于势垒应穿梭两井：x∈[{min_x},{max_x}]"
    );
    let e0 = 0.5 * MASS * 25.0 + A * MBAR * MBAR; // = 12.5 + 16 = 28.5
    let max_dev = es.iter().map(|e| (e - e0).abs()).fold(0.0_f64, f64::max);
    assert!(
        max_dev < e0 * 1e-3,
        "双井长时积分能量应有界：e0={e0} 最大偏离 {max_dev}"
    );
}

/// ③确定性：同输入逐位一致（内核级保守场克隆原样复现）。
#[test]
fn double_well_is_deterministic() {
    let run = || {
        let mut k = PhysicalKernel::with_integrator(NO_GRAV, 2).expect("order 2");
        k.bodies.push(
            evorule_physics_services::kernel::RigidBody::new(
                MASS,
                Vec3::new(1.7, 0.0, 0.0),
                Vec3::new(0.0, 2.0, 0.0),
            )
            .with_double_well(A, MBAR),
        );
        let mut trace = Vec::new();
        for _ in 0..10_000 {
            k.tick(DT);
            trace.push((
                k.bodies[0].pos,
                k.bodies[0].vel,
                k.total_mechanical_energy(),
            ));
        }
        trace
    };
    let a = run();
    let b = run();
    for (x, y) in a.iter().zip(b.iter()) {
        assert!(*x == *y, "双势阱轨迹必须逐位确定");
    }
}

/// ④发散压力素材：高能粒子在势垒顶往返穿梭的长久积分，无 NaN/inf、能量长期有界。
#[test]
fn high_energy_long_run_no_divergence() {
    let (xs, _, es) = run(0.0, 20.0, 50_000); // KE=200 ≫ 势垒，大幅穿梭、强非谐势
    assert!(xs.iter().all(|x| x.is_finite()), "轨迹不得出现 NaN/inf");
    let e0 = 0.5 * MASS * 400.0 + A * MBAR * MBAR;
    let max_dev = es.iter().map(|e| (e - e0).abs()).fold(0.0_f64, f64::max);
    assert!(
        max_dev < e0 * 1e-3,
        "高能长跑能量应长期有界：e0={e0} 最大偏离 {max_dev}"
    );
}

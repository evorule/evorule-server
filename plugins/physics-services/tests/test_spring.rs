//! [evorule 移植注记] 本文件自 rpsm-demo `rpsm/tests/test_spring.rs`(2026-09-01 快照)移植为 evorule-physics-services 集成测试:import 改路(rpsm_core → evorule_physics_services::kernel),测试逻辑逐行保真。
//! [evorule 移植等效] rpsm_dkel 的规则解析/注册表(RuleAST/RuleRegistry,DKEL 规则
//! 语言 `-100.0 * x` 的解析求值)未随内核 vendored,以同语义本地函数
//! `eval_spring_rule(x) = -100.0·x` 等效替代——规则语言自身的解析求值仍留
//! rpsm 侧验证;本文件验证的是「应用层经外力槽注入规则力 + 内核推进」链路。
//! 阶段二：DKEL 弹簧保守力通过外力槽由应用层注入。
//! 验证：水平弹簧振荡（无重力、无碰撞）下，系统能量（动能 + ½kx²）守恒，
//! 且球在规则驱动下越过平衡点往复振荡——证明外力注入确实参与了物理推进。

// 集成测试保留 unwrap/expect 惯例（C5 unwrap/expect/panic = deny 仅约束生产代码）
#![allow(clippy::unwrap_used, clippy::expect_used)]

use evorule_physics_services::kernel::{PhysicalKernel, RigidBody, Vec3};

/// 弹簧刚度由 DKEL 规则 `-100.0 * x` 给出（F = -k·x，k = 100）。
const K: f64 = 100.0;
const STEP: f64 = 0.001;

/// DKEL 规则 `ball_spring: "-100.0 * x"` 的同语义本地求值（移植等效）。
fn eval_spring_rule(x: f64) -> f64 {
    -100.0 * x
}

/// 给定 DKEL 注册表，计算当前刚体的系统总机械能（动能 + 弹簧势能）。
/// 注：本测试关闭重力与碰撞，故无需计入均匀场/多体势能。
fn system_energy(kernel: &PhysicalKernel) -> f64 {
    let body = &kernel.bodies[0];
    let ke = 0.5 * body.mass * body.vel.length_squared();
    let spring_pe = 0.5 * K * body.pos.x * body.pos.x;
    ke + spring_pe
}

#[test]
fn test_spring_dkel_oscil_lates_and_conserves_energy() {
    // 单变量弹簧规则 F = -100·x，位移基准取 x0 = 0（移植等效：本地函数求值）。

    // 无重力、质点（无碰撞），从 x=0.1 静止出发。
    let mut kernel = PhysicalKernel::new(Vec3::zero());
    kernel
        .bodies
        .push(RigidBody::new(1.0, Vec3::new(0.1, 0.0, 0.0), Vec3::zero()));

    let e0 = system_energy(&kernel);
    let mut min_x = f64::MAX;
    let mut max_rel_drift = 0.0_f64;

    // 20 s = 20_000 步；弹簧周期 T = 2π/√(k/m) ≈ 0.628 s，足够多次往复。
    for _ in 0..20_000u32 {
        let x = kernel.bodies[0].pos.x;
        let fx = eval_spring_rule(x);
        kernel.set_external_force(0, Vec3::new(fx, 0.0, 0.0));
        kernel.tick(STEP);

        let e = system_energy(&kernel);
        max_rel_drift = max_rel_drift.max((e - e0).abs() / e0.max(1e-12));
        min_x = min_x.min(kernel.bodies[0].pos.x);
    }

    // 越界越过平衡点：x 应变为负值，证明弹簧拉回后继续往复。
    assert!(
        min_x < -0.05,
        "弹簧应在越过平衡点后回到负侧，min_x={min_x}"
    );
    // 保守性：系统能量（含 ½kx²）相对漂移应很小（辛欧拉误差在多周期内通常 <1%）。
    assert!(
        max_rel_drift < 0.01,
        "保守弹簧系统能量漂移应极小，max|ΔE|/E0={max_rel_drift:.3e}"
    );
}

#[test]
fn test_external_force_cleared_between_unknown_triggers() {
    // 未开启弹簧时，无外力作用，球应保持静止。
    let mut kernel = PhysicalKernel::new(Vec3::zero());
    kernel
        .bodies
        .push(RigidBody::new(1.0, Vec3::new(0.1, 0.0, 0.0), Vec3::zero()));
    for _ in 0..1_000u32 {
        kernel.tick(STEP);
    }
    let body = &kernel.bodies[0];
    assert!(
        body.vel.length_squared() < 1e-18,
        "无外力应保持静止，速度平方={}",
        body.vel.length_squared()
    );
}
//! [evorule 移植注记] 本文件自 rpsm-demo `rpsm/tests/test_joint.rs`(2026-09-01 快照)移植为 evorule-physics-services 集成测试:import 改路(rpsm_core → evorule_physics_services::kernel),测试逻辑逐行保真。
//! 双体软铰（球铰中央力约束）的确定性验证。
//!
//! 模型：两刚体质心以弹簧-阻尼连接（作用于质心连线的**中央力**，等值反作用）——
//!   u = (p_j − p_i)/d；along = (v_i − v_j)·u；
//!   F_i = −u·(k·(d−L) + c·along)，F_j = −F_i。
//! 按当前位置在 `accumulate_forces` 中重算（velocity Verlet 新旧各求一次）→ 辛映射。
//!
//! 边界（诚实披露）：本 MVP 是「质心中央力」软铰，不产生力矩、不断言刚性铰链
//! （真机械臂肘关节需刚性转角约束 + 力矩耦合）——那是下一增量。本文件只验证
//! 软铰自身的守恒/耗散/确定性/动量/安全属性。

// 集成测试保留 unwrap/expect 惯例（C5 unwrap/expect/panic = deny 仅约束生产代码）
#![allow(clippy::unwrap_used, clippy::expect_used)]

use evorule_physics_services::kernel::{PhysicalKernel, RigidBody, Vec3};

const MASS: f64 = 1.0;
const K: f64 = 50.0; // 软铰劲度 N/m
const REST: f64 = 1.0; // 自由长度 m
const DT: f64 = 0.002;
const NO_GRAV: Vec3 = Vec3::zero();

/// 构建双体软铰系统：A 在原点静止，B 在 `d0` 处以 `vb` 初速沿 X 轴。
/// `c` 为软铰阻尼；纯弹簧用 c=0。返回内核。
fn two_body(c: f64, d0: f64, vb: f64) -> PhysicalKernel {
    let mut k = PhysicalKernel::with_integrator(NO_GRAV, 2).expect("order 2");
    k.bodies
        .push(RigidBody::new(MASS, Vec3::zero(), Vec3::zero()).with_joint(1, K, c, REST));
    k.bodies.push(RigidBody::new(
        MASS,
        Vec3::new(d0, 0.0, 0.0),
        Vec3::new(vb, 0.0, 0.0),
    ));
    k
}

/// ①纯弹簧保守：双体拉伸释放（无外力、无重力、无阻尼）→ 能量近守恒（有界、不单调增长）。
#[test]
fn pure_spring_conserves_energy() {
    let mut k = two_body(0.0, 1.3, 0.0); // 拉伸 0.3 释放
    let e0 = 0.5 * K * (1.3 - REST) * (1.3 - REST); // = ½·50·0.09 = 2.25
    let mut max_dev = 0.0_f64;
    for _ in 0..20_000 {
        k.tick(DT);
        let e = k.total_mechanical_energy();
        max_dev = max_dev.max((e - e0).abs());
    }
    assert!(
        max_dev < e0 * 1e-3,
        "纯弹簧双体长时积分能量应近守恒：e0={e0} 最大偏离 {max_dev}"
    );
}

/// ②阻尼耗散单调不增并收敛到自由长度：能量不注入、d 收敛到 L。
#[test]
fn damping_dissipates_monotonic_and_settles() {
    let mut k = two_body(2.0, 1.5, 0.0); // 拉伸 0.5 + 阻尼
    let mut prev = f64::INFINITY;
    let mut last_d = f64::INFINITY;
    let mut final_e = f64::INFINITY;
    for _ in 0..40_000 {
        k.tick(DT);
        let e = k.total_mechanical_energy();
        let d = (k.bodies[1].pos - k.bodies[0].pos).length();
        // 单调性放宽到浮点噪声尺度（辛积分步间有 ~1e-9 级回摆），但绝不注入可观能量。
        assert!(
            e <= prev + 1e-6,
            "阻尼耗散不得注入能量：e={e} 前一步 {prev}"
        );
        prev = e;
        last_d = d;
        final_e = e;
    }
    assert!(
        final_e < 1e-6,
        "长时阻尼应把能量耗散殆尽：final_e={final_e}"
    );
    assert!(
        (last_d - REST).abs() < 1e-3,
        "收敛到自由长度：d={last_d} L={REST}"
    );
}

/// ③平衡点静止：恰在自由长度、零速度 → 无净力、全程静止（仅浮点噪声级漂移）。
#[test]
fn rest_length_equilibrium_quiescent() {
    let mut k = two_body(0.0, REST, 0.0);
    for _ in 0..5000 {
        k.tick(DT);
    }
    let a = &k.bodies[0];
    let b = &k.bodies[1];
    assert!(
        a.pos.length() < 1e-9 && a.vel.length() < 1e-9,
        "A 应静止在原点附近：pos={:?} vel={:?}",
        a.pos,
        a.vel
    );
    assert!(
        (b.pos - Vec3::new(REST, 0.0, 0.0)).length() < 1e-9 && b.vel.length() < 1e-9,
        "B 应静止在自由长度处：pos={:?} vel={:?}",
        b.pos,
        b.vel
    );
}

/// ④确定性：同输入双跑逐位一致（内核级软铰克隆原样复现）。
#[test]
fn joint_is_deterministic_bitwise() {
    let run = || {
        let mut k = two_body(0.5, 1.2, 3.0);
        let mut trace = Vec::new();
        for _ in 0..10_000 {
            k.tick(DT);
            trace.push((
                k.bodies[0].pos,
                k.bodies[0].vel,
                k.bodies[1].pos,
                k.bodies[1].vel,
            ));
        }
        trace
    };
    let a = run();
    let b = run();
    for (x, y) in a.iter().zip(b.iter()) {
        assert!(*x == *y, "软铰轨迹必须逐位确定");
    }
}

/// ⑤牛顿第三定律：中央等值反作用力 → 无外力下总动量精确守恒。
#[test]
fn joint_conserves_total_momentum() {
    let mut k = two_body(0.3, 1.2, 2.0); // A 静止、B 初速 2 → 总动量 = 2·MASS
    let p0 = 2.0 * MASS;
    for _ in 0..20_000 {
        k.tick(DT);
        let p = k
            .bodies
            .iter()
            .map(|b| b.vel * b.mass)
            .fold(Vec3::zero(), |acc, v| {
                Vec3::new(acc.x + v.x, acc.y + v.y, acc.z + v.z)
            });
        assert!(
            (p.x - p0).abs() < 1e-12,
            "中央力应保总动量：p.x={} 期望 {p0}",
            p.x
        );
    }
}

/// ⑥回复有效性：拉伸释放后距离穿越自由长度做往返（证明回复力真实驱动）。
#[test]
fn relative_oscillation_about_rest_length() {
    let mut k = two_body(0.0, 1.5, 0.0); // 拉伸 0.5 释放（纯弹簧）
    let mut min_d = f64::INFINITY;
    let mut max_d = 0.0_f64;
    let mut n = 0;
    let mut prev_d = 1.5;
    for _ in 0..20_000 {
        k.tick(DT);
        let d = (k.bodies[1].pos - k.bodies[0].pos).length();
        min_d = min_d.min(d);
        max_d = max_d.max(d);
        // 穿越计数：距离从高于 L 变到低于 L（方向扫描）。
        if (prev_d - REST) > 0.0 && (d - REST) <= 0.0 {
            n += 1;
        }
        prev_d = d;
    }
    assert!(min_d < REST - 1e-3, "应越过自由长度：min_d={min_d}");
    assert!(
        (max_d - 1.5).abs() < 1e-2,
        "振幅应回到初始拉伸附近：max_d={max_d}"
    );
    assert!(n >= 2, "应发生多次简谐往返穿越：n={n}");
}

/// ⑦越界索引安全忽略：对端索引无效 → 不 panic、行为等同无约束自由体（能量恒为 KE）。
#[test]
fn out_of_bounds_index_safely_ignored() {
    let mut k = PhysicalKernel::with_integrator(NO_GRAV, 2).expect("order 2");
    k.bodies.push(
        RigidBody::new(MASS, Vec3::zero(), Vec3::new(3.0, 0.0, 0.0)).with_joint(9, K, 0.0, REST),
    );
    let e0 = 0.5 * MASS * 9.0;
    for _ in 0..5000 {
        k.tick(DT);
    }
    let e = k.total_mechanical_energy();
    assert!(
        (e - e0).abs() < 1e-12,
        "越界关节应等同自由体：e={e} e0={e0}"
    );
    assert_eq!(k.bodies[0].vel, Vec3::new(3.0, 0.0, 0.0), "速度应恒定");
}

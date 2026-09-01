//! [evorule 移植等效] 本文件自 rpsm-demo `rpsm/tests/test_constants_lock.rs`(2026-09-01 快照)
//! 移植为 evorule-physics-services 集成测试。
//!
//! 移植边界（诚实声明）：原用例的主体是 rpsm_hci 的
//! `load_constants_with_filter`(JSON 配置中锁定常量键的过滤拒绝)，该 crate 未随
//! rpsm-core 内核 vendored，过滤行为本身仍留 rpsm 侧验证。本文件移植其内核侧
//! 可验证面：物理常量 G/C 的编译期锁定值——这是「配置过滤」存在意义的前提，
//! 并确认内核不提供任何覆盖这些常量的运行时入口。
//!
//! 原 V1 验证语义：JSON 输入 `{"G": 6.0}` 应被过滤拒绝，物理内核 G 值不变。

// 集成测试保留 unwrap/expect 惯例（C5 unwrap/expect/panic = deny 仅约束生产代码）
#![allow(clippy::unwrap_used, clippy::expect_used)]

use evorule_physics_services::kernel::{C, G, PhysicalKernel};

/// 内核锁定的物理常量值不受任何配置影响。
#[test]
fn test_constants_lock_values_unchanged() {
    assert_eq!(G, 6.674_30e-11, "G 常量必须保持锁定值不变");
    assert_eq!(C, 299_792_458.0, "C 常量必须保持锁定值不变");
}

/// 内核构造后无任何覆盖锁定常量的通道：重力只能经 `set_gravity` 注入「均匀场
/// 方向/大小」，万有引力定律使用的 `G` 始终取编译期锁定值——构造双体系统，
/// 实测加速度与 `G·m/r²` 解析值一致（若 G 可被配置篡改，此断言即失效）。
#[test]
fn test_constants_lock_no_runtime_override_path() {
    let m = 1.0e12_f64;
    let d0 = 100.0_f64;
    let mut kernel = PhysicalKernel::new(chrono_free_zone());
    kernel.bodies.push(
        evorule_physics_services::kernel::RigidBody::new(m, vec3_at(-d0 / 2.0), zero_vel()),
    );
    kernel.bodies.push(
        evorule_physics_services::kernel::RigidBody::new(m, vec3_at(d0 / 2.0), zero_vel()),
    );

    // 单步积分后实测相对加速度量级 ≈ 2·G·m/r²（两体等质量相向加速，相对加速度
    // = 两者加速度之和；t=dt 内 Δv_rel = a_rel·dt；取绝对值比较量级）。
    let dt = 1e-3_f64;
    kernel.tick(dt);
    let v_rel = (kernel.bodies[1].vel.z - kernel.bodies[0].vel.z).abs();
    let a_measured = v_rel / dt;
    let a_expected = 2.0 * G * m / (d0 * d0);
    let rel = (a_measured - a_expected).abs() / a_expected;
    assert!(
        rel < 1e-9,
        "万有引力必须使用锁定 G：实测 {a_measured:e} vs 解析 {a_expected:e}（相对偏差 {rel:e}）"
    );
}

// —— 测试辅助（零均匀场构造，隔离多体引力单项）——

fn chrono_free_zone() -> evorule_physics_services::kernel::Vec3 {
    evorule_physics_services::kernel::Vec3::zero()
}

fn vec3_at(z: f64) -> evorule_physics_services::kernel::Vec3 {
    evorule_physics_services::kernel::Vec3::new(0.0, 0.0, z)
}

fn zero_vel() -> evorule_physics_services::kernel::Vec3 {
    evorule_physics_services::kernel::Vec3::zero()
}

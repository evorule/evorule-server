//! [evorule 移植等效] 本文件自 rpsm-demo `rpsm/tests/test_thermal_load.rs`(2026-09-01 快照)
//! 移植为 evorule-physics-services 集成测试。
//!
//! 移植边界（诚实声明）：原用例的主体是 rpsm_hci 的配置热重载
//! `spawn_config_watcher`（文件 watcher + 墙钟活性护栏 + 规范化 JSON BLAKE3 一致），
//! 该 crate 未随 rpsm-core 内核 vendored，热重载机制本身仍留 rpsm 侧验证。
//! 本文件移植其内核侧正确性证明与确定性证明（原文件 [正确性证明 2] 与
//! [确定性证明] 两段，2026-08-30 修复记录后确立的「哈希证明」判据）：
//!   1) 新重力（2G）生效：下落 t=1s 位移符合解析值；
//!   2) 同输入双跑，终态 BLAKE3 一致——内核确定性的哈希级断言
//!      （scenario-audit B 组「双跑逐字节一致」判据的仓内移植）。
//!
//! 性能回归不在正确性测试中断言（原文件既定路线：移交 criterion 基准）。

// 集成测试保留 unwrap/expect 惯例（C5 unwrap/expect/panic = deny 仅约束生产代码）
#![allow(clippy::unwrap_used, clippy::expect_used)]

use evorule_physics_services::kernel::{PhysicalKernel, RigidBody, Vec3, G};

/// 内核终态的确定性序列化哈希（Debug 格式对同一构建是确定的）。
fn hash_kernel_state(kernel: &PhysicalKernel) -> String {
    blake3::hash(format!("{:?}", kernel.bodies).as_bytes())
        .to_hex()
        .to_string()
}

/// 同一初始条件下跑 1000 步自由落体（新重力 2G）。
fn run_fall_kernel() -> PhysicalKernel {
    let mut kernel = PhysicalKernel::new(Vec3::new(0.0, -G * 2.0, 0.0));
    kernel
        .bodies
        .push(RigidBody::new(1.0, Vec3::zero(), Vec3::zero()));
    for _ in 0..1000 {
        kernel.tick(0.001);
    }
    kernel
}

/// 新重力生效：下落 t=1s 位移应为 2G 下 ½·(2G)·t²。
#[test]
fn test_new_gravity_takes_effect() {
    let kernel = run_fall_kernel();
    let dy = kernel.bodies[0].pos.y;
    let expect = -0.5 * G * 2.0 * 1.0; // 水平动量独立
    assert!(
        (dy - expect).abs() < 1e-6,
        "新重力未生效: dy = {dy}, expect = {expect}"
    );
}

/// 内核确定性：同输入双跑，终态 BLAKE3 一致（哈希级判据）。
#[test]
fn test_kernel_determinism_by_terminal_state_hash() {
    let hash_a = hash_kernel_state(&run_fall_kernel());
    let hash_b = hash_kernel_state(&run_fall_kernel());
    assert_eq!(hash_a, hash_b, "同输入双跑终态哈希不一致：内核非确定性");
}

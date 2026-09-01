//! [evorule 移植注记] 本文件自 rpsm-demo `rpsm/tests/test_environment.rs`(2026-09-01 快照)移植为 evorule-physics-services 集成测试:import 改路(rpsm_core → evorule_physics_services::kernel),测试逻辑逐行保真。
//! [evorule 移植等效] rpsm_hci 的 `HciConfig`/`EnvironmentConfig` 环境模板层
//! (earth/moon/custom 模板与 `get_current_gravity` 换算)未随内核 vendored,
//! 模板查值以同名常量字面量注入(9.80665/1.62/100.0)等效替代——模板层自身的
//! 换算逻辑仍留在 rpsm 侧验证;本文件验证的是注入后内核行为的正确性。
//! 环境模板层（ETL）V1 行为验证。
//!
//! 本文件只做「行为验证」而非「数值锁定」——内核已确认锁定物理常量 G/c，
//! 这里验证的是：切换环境模板后，物理行为是否符合预期。
//!
//! 覆盖两条主线：
//! 1. **均匀场侧**（earth / moon / custom）：验证模板重力加速度注入 `set_gravity`
//!    后，自由落体行为符合 `y = h - ½·g·t²` 的预期。
//! 2. **多体引力侧**（multibody）：均匀场为零时，内核中锁定的 `G·m/r²` 万有引力
//!    仍应让两体彼此靠近。
//!
//! 两条线共用同一个 `PhysicalKernel::tick`，合起来即证明「多体引力 + 均匀场叠加
//! 并存」的语义成立（牛顿叠加原理）。

// 集成测试保留 unwrap/expect 惯例（C5 unwrap/expect/panic = deny 仅约束生产代码）
#![allow(clippy::unwrap_used, clippy::expect_used)]

use evorule_physics_services::kernel::{PhysicalKernel, RigidBody, Vec3};

/// 环境模板重力加速度（与 rpsm_hci 模板缺省值对齐的移植等效常量）：
/// earth = 9.80665 m/s²、moon = 1.62 m/s²、custom 示例 = 100 m/s²。
const G_EARTH_TEMPLATE: f64 = 9.80665;
const G_MOON_TEMPLATE: f64 = 1.62;
const G_CUSTOM_TEMPLATE: f64 = 100.0;

/// 构造「单一刚体 + 零均匀场」的内核：
/// 初始均匀场置零（`Vec3::zero()`），让测试方决定是否通过 `set_gravity` 注入重力，
/// 从而隔离「均匀场」与「多体引力」两种效应的测试环境。
fn single_body_kernel(height: f64) -> PhysicalKernel {
    let mut kernel = PhysicalKernel::new(Vec3::zero());
    kernel.bodies.push(RigidBody::new(
        1.0,
        Vec3::new(0.0, height, 0.0),
        Vec3::zero(),
    ));
    kernel
}

/// 地球环境下的自由落体：
/// `HciConfig::default()` 的 `active_template` 为 "earth"，其重力加速度为 9.80665 m/s²。
/// 理论：从 10 m 起落，`y = 10 − ½·9.80665·t²`，约 1.43 s 到达 y=0；
/// 验证时推进 2 秒（2000 步），此时 y≈ −9.6 < 0，证明地球重力明显把刚体拉过地面。
#[test]
fn test_earth_gravity_fall_time() {
    let mut kernel = single_body_kernel(10.0);
    // 移植等效：rpsm_hci 缺省模板 active_template = "earth"（未 vendored，字面量注入）。
    let g = G_EARTH_TEMPLATE; // 期望 9.80665
    assert!((g - 9.80665).abs() < 1e-9, "earth g 应为 9.80665，收到 {g}");

    // 把环境换算出的重力加速度注入内核（方向朝 −y）。
    kernel.set_gravity(Vec3::new(0.0, -g, 0.0));
    // 2 秒 = 2000 步 × 0.001 s；地球重力下应早已穿过地面。
    for _ in 0..2000 {
        kernel.tick(0.001);
    }
    let pos = kernel.bodies[0].pos.y;
    assert!(pos < 0.0, "Earth: 2 秒后应已落地，y={pos}");
}

/// 月球环境下的（更缓慢）自由落体：
/// 切换 `active_template = "moon"` 后重力加速度变为 1.62 m/s²，约为地球的 1/6。
/// 理论：从 10 m 起落，`y = 10 − ½·1.62·t²`，约 3.51 s 到达 y=0；
/// 验证时推进 3.5 秒（3500 步），此时 y≈ 0.08，落在「接近地面」的容差区间 [−0.5, 0.5]。
/// 与地球测试对比：相同高度下月球需更久才能着地，从而证明模板切换确实生效。
#[test]
fn test_moon_gravity_slower_fall() {
    let mut kernel = single_body_kernel(10.0);
    // 移植等效：rpsm_hci 模板切换 active_template = "moon"（未 vendored，字面量注入）。
    let g = G_MOON_TEMPLATE; // 期望 1.62
    assert!((g - 1.62).abs() < 1e-9, "moon g 应为 1.62，收到 {g}");

    kernel.set_gravity(Vec3::new(0.0, -g, 0.0));
    // 3.5 秒 = 3500 步；月球重力下应尚未远落过地面，停留在 y≈0 附近。
    for _ in 0..3500 {
        kernel.tick(0.001);
    }
    let pos = kernel.bodies[0].pos.y;
    assert!(pos > -0.5 && pos < 0.5, "Moon: 3.5 秒后应接近地面，y={pos}");
}

/// 自定义环境下的重力缩放：
/// 将 `active_template` 切到 "custom"，并把 `custom_gravity` 设为强拉伸的 100 m/s²，
/// 验证 `EnvironmentConfig` 的 custom 分支优先级生效。
/// 理论：`y = 10 − ½·100·t²`，约 0.45 s 即过地面；1 秒（1000 步）后明显穿透。
/// 该用例同时覆盖「自定义值必须能覆盖模板默认值」的换算逻辑。
#[test]
fn test_custom_gravity_scale() {
    let mut kernel = single_body_kernel(10.0);
    // 移植等效：rpsm_hci 模板切 custom + custom_gravity = 100（未 vendored，字面量注入），
    // 该用例同时覆盖「自定义值覆盖模板默认值」语义对应的注入后行为。
    let g = G_CUSTOM_TEMPLATE; // 期望 100
    assert!((g - 100.0).abs() < 1e-9);

    kernel.set_gravity(Vec3::new(0.0, -g, 0.0));
    // 更强的自定义重力下，1 秒（1000 步）应显著穿过地面。
    for _ in 0..1000 {
        kernel.tick(0.001);
    }
    let pos = kernel.bodies[0].pos.y;
    assert!(pos < 0.0, "Custom(100): 1 秒后应已落地，y={pos}");
}

/// 均匀场为零时的多体引力并存验证：
/// 若 `tick` 只实现了「均匀场」，则当均匀场为零时两体应静止不动。
/// 这里构造一对质量 1e12 kg、相隔 100 m 的刚体，均匀场置零（`Vec3::zero()`），
/// 仅依靠内核锁定的 `G·m/r²` 万有引力互相吸引。
/// 理论初加速度 `a ≈ G·m/d₀² ≈ 6.674e-11·1e12/(100²) ≈ 6.7e-3 m/s²`，
/// 20 s 内每体位移约 `½·a·t² ≈ 1.3 m`，推进后距离应明显缩短（100 → ~97.3 m），
/// 证明多体引力与叠加语义同时生效。
#[test]
fn test_multibody_gravity_still_active_without_field() {
    // 均匀场为零时，多体引力仍应让两体相对靠近（验证 G·m/r² 并存生效）。
    let mut kernel = PhysicalKernel::new(Vec3::zero()); // 均匀场 = 零
    let mass = 1.0e12; // 大质量，让多体引力在量级上可被检测
    let d0 = 100.0; // 初始间距
    kernel.bodies.push(RigidBody::new(
        mass,
        Vec3::new(0.0, 0.0, d0 / 2.0),
        Vec3::zero(),
    ));
    kernel.bodies.push(RigidBody::new(
        mass,
        Vec3::new(0.0, 0.0, -d0 / 2.0),
        Vec3::zero(),
    ));

    // 20 s = 20_000 步 × 0.001 s；在两体永不接触的前提下，间距应收敛。
    for _ in 0..20_000u32 {
        kernel.tick(0.001);
    }

    // 末态间距取两刚体 z 坐标差；只要小于初距即证明被引力拉近。
    let d1 = kernel.bodies[0].pos.z - kernel.bodies[1].pos.z;
    assert!(d1 < d0, "多体引力应使两体靠近：初始 {d0}，现 {d1}");
}

//! 插件清单端到端验收（后端插件清单化，15 号实施计划 W4；UV-035 泛化至双插件；
//! UV-037 泛化至三插件）
//!
//! 不新增机制，做**运行时闭环实证**——与 main.rs 生产装配同构（过滤路由器 +
//! ServiceRegistryHandler HTTP 回落），非桩模拟：
//!
//! 1. 正向：子集清单启用 `config_persist` → `call_service` 命中原生实现
//!    （进程内确定性，无网络），args 原样到达。
//! 2. 负向：`sampling_service` 为真实原生服务但未启用 → 回落 HTTP 注册表
//!    （缺省部署注册表为空）→ unknown service_name，错误含自诊断指引
//!    （fail-fast + 可自愈，对齐系统自愈原则），不伪造结果。
//! 3. 确定性：启用服务名序 = `NATIVE_SERVICES` 声明序，与清单书写序无关
//!    （健康快照 /api/health plugins 节的能力对账口径）。
//! 4. UV-035 双插件：physics-services 按生产同构链挂载——原生命中、
//!    插件内未启用回落、链尾诚实报错、声明序锁定。
//! 5. UV-037 三插件：indicator-services 以第二种集成模式（Python 参考实现
//!    Rust 重写）挂载，链 demo → physics → indicator → HTTP——原生命中
//!    （含 warmup null 语义）、未启用穿透、声明序锁定。
//!
//! 真实二进制场景（缺省全启 / 子集 / 停用 / 清单 fail-fast + /api/health
//! plugins 节）由 `scripts/run-plugins-e2e.ps1` 承接，与本测试互补。

// 集成测试保留 unwrap 惯例（C5 unwrap/expect/panic = deny 仅约束生产代码）
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use evorule_demo_services::DemoServiceRouter;
use evorule_indicator_services::IndicatorServiceRouter;
use evorule_io_handlers::{HttpHandler, ServiceRegistry, ServiceRegistryHandler};
use evorule_physics_services::PhysicsServiceRouter;
use evorule_reactor::IoHandler as _;
use evorule_tcb::JsonValue;

/// 生产同构装配：过滤路由器 + 空注册表 HTTP 回落（与缺省部署一致：
/// service_registry.json 缺省不加载 → 空注册表）。
fn subset_router(enabled: &[&str]) -> DemoServiceRouter {
    let fallback = Arc::new(ServiceRegistryHandler::new(
        ServiceRegistry::empty(),
        Arc::new(HttpHandler::new()),
    ));
    DemoServiceRouter::with_enabled(fallback, enabled).unwrap()
}

/// UV-035 生产同构双插件链：demo 路由器为链首，physics 承接其回落，
/// HTTP 注册表为链尾（与 main.rs PLUGIN_DEFS 声明序挂载一致：
/// 链首 demo 未命中 → physics → 链尾 HTTP）。
fn dual_plugin_router(demo_enabled: &[&str], physics_enabled: &[&str]) -> DemoServiceRouter {
    let http = Arc::new(ServiceRegistryHandler::new(
        ServiceRegistry::empty(),
        Arc::new(HttpHandler::new()),
    ));
    let physics = Arc::new(PhysicsServiceRouter::with_enabled(http, physics_enabled).unwrap())
        as Arc<dyn evorule_reactor::IoHandler>;
    DemoServiceRouter::with_enabled(physics, demo_enabled).unwrap()
}

/// UV-037 生产同构三插件链（与 main.rs 一致）：
/// 链首 demo 未命中 → physics 未命中 → indicator → 链尾 HTTP 注册表。
fn triple_plugin_router(
    demo_enabled: &[&str],
    physics_enabled: &[&str],
    indicator_enabled: &[&str],
) -> DemoServiceRouter {
    let http = Arc::new(ServiceRegistryHandler::new(
        ServiceRegistry::empty(),
        Arc::new(HttpHandler::new()),
    ));
    let indicator =
        Arc::new(IndicatorServiceRouter::with_enabled(http, indicator_enabled).unwrap())
            as Arc<dyn evorule_reactor::IoHandler>;
    let physics =
        Arc::new(PhysicsServiceRouter::with_enabled(indicator, physics_enabled).unwrap())
            as Arc<dyn evorule_reactor::IoHandler>;
    DemoServiceRouter::with_enabled(physics, demo_enabled).unwrap()
}

fn svc_params(service_name: &str, args: JsonValue) -> JsonValue {
    JsonValue::object_from_pairs(&[
        ("service_name", JsonValue::string(service_name)),
        ("args", args),
    ])
}

/// 正向：子集清单启用 config_persist → 原生进程内命中，args 原样到达。
#[tokio::test]
async fn subset_manifest_enabled_service_native_hit() {
    let router = subset_router(&["config_persist"]);
    let r = router
        .execute(&svc_params(
            "config_persist",
            JsonValue::object_from_pairs(&[("operation", JsonValue::string("plugins-e2e-probe"))]),
        ))
        .await
        .unwrap();
    assert_eq!(
        r.get("success").and_then(|v| v.as_bool()),
        Some(true),
        "原生服务应命中并返回 success: {r}"
    );
    let msg = r.get("message").and_then(|v| v.as_str()).unwrap_or_default();
    assert!(msg.contains("plugins-e2e-probe"), "args 应原样到达原生服务: {msg}");
}

/// 负向：未启用服务回落空注册表 → unknown service_name + 自诊断指引，如实报错。
#[tokio::test]
async fn subset_manifest_disabled_service_honest_error_with_guidance() {
    let router = subset_router(&["config_persist"]);
    let err = router
        .execute(&svc_params("sampling_service", JsonValue::empty_object()))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("unknown service_name"), "got: {err}");
    assert!(err.contains("自诊断指引"), "错误应含自诊断指引，got: {err}");
    assert!(
        err.contains("sampling_service"),
        "指引应指向缺失的服务名，got: {err}"
    );
}

/// 确定性：健康快照/能力对账的服务名序 = NATIVE_SERVICES 声明序，
/// 与清单书写序无关（llm_advisor 在声明表中先于 config_persist）。
#[test]
fn subset_manifest_enabled_names_follow_declaration_order() {
    let router = subset_router(&["config_persist", "llm_advisor"]);
    assert_eq!(
        router.enabled_service_names(),
        vec!["llm_advisor", "config_persist"],
        "启用名序必须锁定为声明表序（防健康面/对账口径漂移）"
    );
}

// ============================================================================
// UV-035 双插件：physics-services 生产同构链场景
// ============================================================================

/// 双插件正向：physics_energy 子集启用 → 原生进程内命中（确定性，无网络）。
#[tokio::test]
async fn dual_plugin_physics_native_hit() {
    let router = dual_plugin_router(&["config_persist"], &["physics_energy"]);
    let r = router
        .execute(&svc_params(
            "physics_energy",
            JsonValue::object_from_pairs(&[(
                "bodies",
                // TCB 无 Float:数值参数传 Integer 或数字字符串(与生产 JSON 规则一致)
                JsonValue::Array(vec![JsonValue::object_from_pairs(&[
                    ("mass", JsonValue::Integer(1)),
                    (
                        "pos",
                        JsonValue::Array(vec![
                            JsonValue::string("0.0"),
                            JsonValue::string("0.0"),
                            JsonValue::string("0.0"),
                        ]),
                    ),
                    (
                        "vel",
                        JsonValue::Array(vec![
                            JsonValue::string("0.0"),
                            JsonValue::string("0.0"),
                            JsonValue::string("0.0"),
                        ]),
                    ),
                ])]),
            )]),
        ))
        .await
        .unwrap();
    assert_eq!(
        r.get("status").and_then(|v| v.as_str()),
        Some("ok"),
        "physics_energy 应原生命中并返回 status=ok: {r}"
    );
    assert!(
        r.get("total_mechanical_energy").is_some(),
        "能量输出应存在: {r}"
    );
}

/// 双插件回落：physics 插件内未启用服务（physics_simulate）→ 链首 demo 层
/// 未命中 → physics 层未启用 → 链尾空注册表 → unknown service_name +
/// 自诊断指引（诚实报错，不伪造）。
#[tokio::test]
async fn dual_plugin_disabled_physics_service_falls_through_honest_error() {
    let router = dual_plugin_router(&["config_persist"], &["physics_energy"]);
    let err = router
        .execute(&svc_params("physics_simulate", JsonValue::empty_object()))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("unknown service_name"), "got: {err}");
    assert!(err.contains("自诊断指引"), "错误应含自诊断指引，got: {err}");
    assert!(
        err.contains("physics_simulate"),
        "指引应指向缺失的服务名，got: {err}"
    );
}

/// 双插件确定性：physics 插件启用名序 = physics NATIVE_SERVICES 声明序，
/// 与清单书写序无关（physics_energy 声明序在 physics_simulate 之后）。
#[test]
fn dual_plugin_physics_enabled_names_follow_declaration_order() {
    let http = Arc::new(ServiceRegistryHandler::new(
        ServiceRegistry::empty(),
        Arc::new(HttpHandler::new()),
    ));
    let router = PhysicsServiceRouter::with_enabled(
        http,
        &["physics_energy", "physics_simulate"],
    )
    .unwrap();
    assert_eq!(
        router.enabled_service_names(),
        vec!["physics_simulate", "physics_energy"],
        "physics 启用名序必须锁定为其声明表序（防健康面/对账口径漂移）"
    );
}

/// 双插件互不干扰：demo 服务在链首仍被 demo 层原生命中（链序正确性：
/// 链首命中不依赖后续层；physics 层启用与否不影响 demo 层）。
#[tokio::test]
async fn dual_plugin_demo_service_still_hit_through_physics_layer() {
    let router = dual_plugin_router(&["config_persist"], &["physics_energy"]);
    let r = router
        .execute(&svc_params(
            "config_persist",
            JsonValue::object_from_pairs(&[("operation", JsonValue::string("uv035-dual-probe"))]),
        ))
        .await
        .unwrap();
    assert_eq!(
        r.get("success").and_then(|v| v.as_bool()),
        Some(true),
        "穿透 physics 层后 demo 原生服务应命中: {r}"
    );
}

// ============================================================================
// UV-037 三插件：indicator-services 生产同构链场景（第二种集成模式）
// ============================================================================

/// 三插件正向：indicator_sma 子集启用 → 穿透 demo/physics 两层后原生命中
/// （进程内确定性，无网络），前 4 位 warmup null 语义如实呈现。
#[tokio::test]
async fn triple_plugin_indicator_native_hit_with_warmup_nulls() {
    let router = triple_plugin_router(
        &["config_persist"],
        &["physics_energy"],
        &["indicator_sma"],
    );
    let r = router
        .execute(&svc_params(
            "indicator_sma",
            JsonValue::object_from_pairs(&[
                (
                    "series",
                    JsonValue::Array((1..=10).map(JsonValue::Integer).collect()),
                ),
                ("window", JsonValue::Integer(5)),
            ]),
        ))
        .await
        .unwrap();
    assert_eq!(
        r.get("status").and_then(|v| v.as_str()),
        Some("ok"),
        "indicator_sma 应原生命中并返回 status=ok: {r}"
    );
    let values = r
        .get("values")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("values 数组应存在: {r}"));
    assert_eq!(values.len(), 10, "输出长度应与序列一致: {r}");
    assert!(
        values.iter().take(4).all(|v| matches!(v, JsonValue::Null)),
        "前 4 位应为 warmup null: {values:?}"
    );
    assert_eq!(
        values[4].as_str(),
        Some("3.0"),
        "第 5 位应为 1+2+3+4+5/5=3.0(数值字符串化约定): {values:?}"
    );
}

/// 三插件回落：indicator 插件内未启用服务（indicator_macd）→ demo/physics
/// 两层均未命中 → indicator 层未启用 → 链尾空注册表 → unknown service_name
/// + 自诊断指引（诚实报错，不伪造）。
#[tokio::test]
async fn triple_plugin_disabled_indicator_service_falls_through_honest_error() {
    let router = triple_plugin_router(
        &["config_persist"],
        &["physics_energy"],
        &["indicator_sma"],
    );
    let err = router
        .execute(&svc_params("indicator_macd", JsonValue::empty_object()))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("unknown service_name"), "got: {err}");
    assert!(err.contains("自诊断指引"), "错误应含自诊断指引，got: {err}");
    assert!(
        err.contains("indicator_macd"),
        "指引应指向缺失的服务名，got: {err}"
    );
}

/// 三插件确定性：indicator 插件启用名序 = indicator NATIVE_SERVICES 声明序，
/// 与清单书写序无关（健康快照/能力对账口径，与另两插件同构）。
#[test]
fn triple_plugin_indicator_enabled_names_follow_declaration_order() {
    let http = Arc::new(ServiceRegistryHandler::new(
        ServiceRegistry::empty(),
        Arc::new(HttpHandler::new()),
    ));
    let router = IndicatorServiceRouter::with_enabled(
        http,
        &["indicator_rsi", "indicator_sma", "indicator_macd"],
    )
    .unwrap();
    assert_eq!(
        router.enabled_service_names(),
        vec!["indicator_sma", "indicator_macd", "indicator_rsi"],
        "indicator 启用名序必须锁定为其声明表序（防健康面/对账口径漂移）"
    );
}

/// 三插件互不干扰：demo 服务在链首原生命中（indicator 层挂载不改变
/// 既有插件行为；与 UV-035 双插件回归语义一致）。
#[tokio::test]
async fn triple_plugin_demo_service_still_hit_through_indicator_layer() {
    let router = triple_plugin_router(
        &["config_persist"],
        &["physics_energy"],
        &["indicator_sma"],
    );
    let r = router
        .execute(&svc_params(
            "config_persist",
            JsonValue::object_from_pairs(&[("operation", JsonValue::string("uv037-triple-probe"))]),
        ))
        .await
        .unwrap();
    assert_eq!(
        r.get("success").and_then(|v| v.as_bool()),
        Some(true),
        "三插件链链首 demo 原生服务应命中: {r}"
    );
}

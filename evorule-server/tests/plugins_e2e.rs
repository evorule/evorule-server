//! 插件清单端到端验收（后端插件清单化，15 号实施计划 W4）
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
//!
//! 真实二进制场景（缺省全启 / 子集 / 停用 / 清单 fail-fast + /api/health
//! plugins 节）由 `scripts/run-plugins-e2e.ps1` 承接，与本测试互补。

// 集成测试保留 unwrap 惯例（C5 unwrap/expect/panic = deny 仅约束生产代码）
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use evorule_demo_services::DemoServiceRouter;
use evorule_io_handlers::{HttpHandler, ServiceRegistry, ServiceRegistryHandler};
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

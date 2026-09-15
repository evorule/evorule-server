// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `evorule-wasm-host` —— WASM UDF 宿主进程。
//!
//! # 定位（77 号阶段 2 / ADR-0001）
//! 以 **external 插件**身份经 HTTP 接入 `evorule-server`：
//! server 侧 `call_service` → `service_registry` 查 URL → HTTP POST → 本进程执行 `.wasm`。
//!
//! **刻意进程外**（ADR-0001 决策 1）：`evorule-server` 二进制零膨胀、
//! 源码零改动，wasmtime 依赖树不进入 server 的 `Cargo.lock`。
//!
//! # 端点
//! | 方法 | 路径 | 说明 |
//! |---|---|---|
//! | GET | `/health` | 探活（57 号契约：2xx + JSON，否则判 Offline 并告警） |
//! | POST | `/services/{name}` | 执行 UDF，body = 入参 JSON；200 成功 / 404 未知 UDF / 422 执行失败 |
//!
//! **`/health` 的两档语义（探活可信度补强，见 `declaration` 模块）**：
//! - 声明与实载一致 → **200** → server 判 `online`；
//! - 可判定的不一致（0 个模块 / 声明了但未加载 / 加载了但未声明）→ **503**
//!   → server 判 `offline` 并记 `platform.event.plugin_offline` 告警。
//!
//! 为何必须落在状态码上：server 侧 `plugin_probe::classify_response` 只看
//! 「状态码是否 2xx」+「body 是否 JSON」，body 里写再多诊断也改变不了判定。
//! 若这里恒返 200，则「探活 online」只能证明**进程存活**，不能证明**服务可用**。
//! 无法对账（找不到 plugin.json）时维持 200 但 body 显式标注 `unavailable`——
//! 把"无法判定"当故障会制造假警（详见 `declaration` 模块头的取舍说明）。
//!
//! **路径为何是 `/services/{name}`（契约 v1.2 硬性）**：server 装载外部插件包时按
//! `base_url + /services/{name}` 派生路由（`main.rs::load_external_plugins`），
//! 是本进程唯一会被调用的 URI 形态。此处不提供第二形态（如 `/udf/{name}`）——
//! 双入口必然漂移，且未被调用的那个永远是死代码。
//!
//! **入参形态**：server 的 `service_registry` 把规则侧 `params.args` 序列化为
//! HTTP body（`{"amount":1000,...}`）并置 `Content-Type: application/json`，
//! 故 guest 收到的即原始入参 JSON，host 不包装、不解析。
//!
//! **调用出口语义**：非 2xx 会被 server 侧 `http_handler` 转为 `Err`（消息含
//! status + 响应体截断），因此本进程的 404/422 在规则侧表现为显式失败而非静默。
//!
//! # UDF ABI（guest 必须导出）
//! - `memory`：线性内存
//! - `alloc(len: i32) -> i32`：分配输入空间
//! - `udf(ptr: i32, len: i32) -> i64`：执行；返回 `(结果指针 << 32) | 结果长度`
//!
//! **失败约定**：出参 JSON 顶层若含 `error` 字段，host 视为 UDF 业务失败（→ 422）；
//! 否则 → 200。理由见 `call_udf` 内注释。
//!
//! # 配置（环境变量）
//! - `WASM_HOST_ADDR` 监听地址，默认 `127.0.0.1:9140`
//! - `WASM_HOST_DIR`  `.wasm` 目录，默认 `../wasm`（相对本包目录即 `plugins/wasm`）；
//!   **目录不存在即拒绝启动**（fail-fast，不再带 0 个 UDF 静默运行）
//! - `WASM_HOST_PLUGIN_JSON` 声明文件位置，缺省按
//!   `<WASM_HOST_DIR>/../wasm-host/plugin.json` 探测（不认 cwd，见 `declaration` 模块）
//! - `WASM_HOST_FUEL` fuel 预算，默认 `100_000_000`
//! - `WASM_HOST_MAX_MEM` 单模块内存上限字节，默认 `16777216`（16 MiB）

mod declaration;
mod engine;
mod registry;

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::engine::UdfRuntime;
use crate::registry::UdfRegistry;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let addr = std::env::var("WASM_HOST_ADDR").unwrap_or_else(|_| "127.0.0.1:9140".to_string());
    let dir = std::env::var("WASM_HOST_DIR").unwrap_or_else(|_| "../wasm".to_string());
    let fuel: u64 = std::env::var("WASM_HOST_FUEL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100_000_000);
    let max_mem: usize = std::env::var("WASM_HOST_MAX_MEM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(16 * 1024 * 1024);

    let runtime = match UdfRuntime::new(fuel, max_mem) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            eprintln!("evorule-wasm-host: 运行时初始化失败: {e}");
            std::process::exit(1);
        }
    };

    let registry = match UdfRegistry::load_dir(
        Arc::clone(&runtime),
        std::path::Path::new(&dir),
        std::env::var("WASM_HOST_PLUGIN_JSON").ok().as_deref(),
    ) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            eprintln!("evorule-wasm-host: UDF 加载失败（fail-fast）: {e}");
            std::process::exit(1);
        }
    };

    tracing::info!(
        "evorule-wasm-host: 已加载 {} 个 UDF: {:?}",
        registry.len(),
        registry.names()
    );

    // 声明对账：结论决定 /health 是 200 还是 503，故必须在启动期就喊清楚，
    // 不能等运维去 curl 才发现。
    {
        let rec = registry.reconciliation();
        match rec.state {
            crate::declaration::ReconciliationState::Ok => tracing::info!(
                "声明对账一致: plugin.json({}) × 实载 {} 个服务",
                rec.source.as_deref().unwrap_or("(未知来源)"),
                rec.loaded.len()
            ),
            crate::declaration::ReconciliationState::Degraded => tracing::error!(
                "声明对账不一致 → /health 将返回 503（server 侧会判 offline 并告警）: {}",
                rec.reason.as_deref().unwrap_or("(未给原因)")
            ),
            crate::declaration::ReconciliationState::Unavailable => tracing::warn!(
                "声明对账跳过（无法判定，不判 degraded）: {}",
                rec.reason.as_deref().unwrap_or("(未给原因)")
            ),
        }
    }

    let app = Router::new()
        .route("/health", get(health))
        .route("/services/{name}", post(call_udf))
        .with_state(registry);

    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("evorule-wasm-host: bind {addr} failed: {e}");
            std::process::exit(1);
        }
    };

    tracing::info!("evorule-wasm-host listening on {addr}");
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("evorule-wasm-host: serve failed: {e}");
        std::process::exit(1);
    }
}

/// 探活：`{base_url}/health` 必须 2xx + JSON（57 号 plugin_probe 契约）。
///
/// **状态码即结论**：`degraded` → 503（server 判 offline 并告警）；
/// `ok` / `unavailable` → 200（后者在 body 里显式标注"未对账"）。
/// `unavailable` 不判 503 的理由见 `declaration` 模块头（拒绝假警）。
async fn health(State(reg): State<Arc<UdfRegistry>>) -> (StatusCode, Json<Value>) {
    let rec = reg.reconciliation();
    let code = if rec.is_degraded() {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };
    let status = if rec.is_degraded() { "degraded" } else { "ok" };
    (
        code,
        Json(json!({
            "status": status,
            "service": "evorule-wasm-host",
            "modules": reg.len(),
            "udfs": reg.names(),
            // 探活可信度：可服务性对账（声明 × 实载）。字段含义见 declaration 模块。
            "reconciliation": rec.to_json(),
        })),
    )
}

/// 执行 UDF。body 原样透传给 guest（host 不解析入参，保持 ABI 中立）。
async fn call_udf(
    State(reg): State<Arc<UdfRegistry>>,
    Path(name): Path<String>,
    body: Bytes,
) -> (StatusCode, Json<Value>) {
    let module = match reg.module(&name) {
        Some(m) => m,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "ok": false, "error": format!("unknown udf: {name}") })),
            )
        }
    };

    let runtime = reg.runtime();
    let input = body.to_vec();

    // CPU 密集的 wasmtime 执行移出 async worker（T5）
    let outcome = tokio::task::spawn_blocking(move || runtime.call(&module, &input)).await;

    let out = match outcome {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(e)) => {
            // UDF 自身失败（trap / fuel 耗尽 / 内存越界）→ 422，让规则侧显式感知
            tracing::warn!("UDF `{name}` 执行失败: {e}");
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "ok": false, "udf": name, "error": e })),
            );
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "ok": false, "udf": name, "error": format!("阻塞任务失败: {e}") })),
            )
        }
    };

    match serde_json::from_slice::<Value>(&out) {
        Ok(value) => {
            // ABI 约定：出参 JSON **顶层含 `error` 字段** = UDF 业务失败。
            // 理由：host 无法区分"UDF 成功返回了一个 JSON"与"UDF 自己报错并写成 JSON"，
            // 若不约定，业务错误会被当成 200 成功透传给规则侧，治理语义失真。
            if let Some(e) = value.get("error") {
                tracing::warn!("UDF `{name}` 业务失败: {e}");
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({ "ok": false, "udf": name, "error": e })),
                );
            }
            (StatusCode::OK, Json(json!({ "ok": true, "value": value })))
        }
        Err(e) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "ok": false,
                "udf": name,
                "error": format!("UDF 返回值不是合法 JSON: {e}"),
            })),
        ),
    }
}

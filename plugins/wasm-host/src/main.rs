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
//! | POST | `/udf/{name}` | 执行 UDF，body = 入参 JSON；200 成功 / 404 未知 UDF / 422 执行失败 |
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
//! - `WASM_HOST_DIR`  `.wasm` 目录，默认 `../wasm`（相对本包目录即 `plugins/wasm`）
//! - `WASM_HOST_FUEL` fuel 预算，默认 `100_000_000`
//! - `WASM_HOST_MAX_MEM` 单模块内存上限字节，默认 `16777216`（16 MiB）

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

    let registry = match UdfRegistry::load_dir(Arc::clone(&runtime), std::path::Path::new(&dir)) {
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

    let app = Router::new()
        .route("/health", get(health))
        .route("/udf/{name}", post(call_udf))
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

/// 探活：`{base_url}/health` 必须 2xx + JSON（57 号 plugin_probe 契约）
async fn health(State(reg): State<Arc<UdfRegistry>>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "service": "evorule-wasm-host",
        "modules": reg.len(),
        "udfs": reg.names(),
    }))
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

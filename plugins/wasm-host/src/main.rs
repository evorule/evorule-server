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
//! # 探活契约（57 号 `api/plugin_probe.rs`）
//! `GET {base_url}/health` 必须返回 **2xx + JSON**，否则被判 `Offline` 并告警
//! （404/405 判 `NotImplemented` —— 不报警但不被发现，故本进程必须实现）。

use axum::{routing::get, Json, Router};
use serde_json::{json, Value};

/// 探活端点：2xx + JSON（契约见模块文档）。
///
/// `modules` 为已加载的 `.wasm` 数量（T2 接入真实加载后不再是常量 0）。
async fn health() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "service": "evorule-wasm-host",
        "modules": 0_usize,
    }))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let app = Router::new().route("/health", get(health));

    const ADDR: &str = "127.0.0.1:9140";
    let listener = match tokio::net::TcpListener::bind(ADDR).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("evorule-wasm-host: bind {ADDR} failed: {e}");
            std::process::exit(1);
        }
    };

    tracing::info!("evorule-wasm-host listening on {ADDR}");
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("evorule-wasm-host: serve failed: {e}");
        std::process::exit(1);
    }
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! flow-studio 外部插件包 —— 自持 HTTP 服务进程（契约 v1.1 §6 编译服务）。
//!
//! 形态：独立进程（finance-config 同模式），无状态（编译是纯函数，
//! 无数据目录）；装卸：pack.json `service.base_url` 指向本服务 + 重启 server。
//!
//! 路由（契约 v1.1 §6 信封）：
//!   POST /v1/compile   body: { "flow": <已解析 form_ref 的 flow> }
//!                      → 200 { "rule_draft": ..., "compiler_version": "..." }
//!                      → 400 { "error": "..." }（编译失败显式报错，不静默）
//!   GET  /health       存活探针
//!
//! 安全边界：编译是设计期操作（草稿生成），运行时执行链不经本服务；
//! 产物在 server 侧强制过 R2 等价性门禁（6 元指令白名单），本服务
//! 不具备触碰治理状态的任何路径（R3 draft-only）。

#![forbid(unsafe_code)]

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use serde_json::{json, Value};

use evorule_flow_studio_plugin::compile_flow;

#[derive(Clone, Copy)]
struct PluginState;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // 参数解析（轻量手写，插件包不引入 clap 依赖）
    let mut port: u16 = 9120;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => {
                port = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| {
                        eprintln!("[flow-studio] --port 需要一个端口号");
                        std::process::exit(2);
                    });
            }
            "--help" | "-h" => {
                println!("flow-studio plugin --port <9120>");
                println!("编译服务（契约 v1.1 §6）：POST /v1/compile；pack.json service.base_url 指向本服务");
                return;
            }
            other => {
                eprintln!("[flow-studio] 未知参数 {other}（--help 查看用法）");
                std::process::exit(2);
            }
        }
    }

    let state = PluginState;
    let app = axum::Router::new()
        .route("/health", get(health_handler))
        .route("/v1/compile", post(compile_handler))
        .with_state(state);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|e| {
            eprintln!("[flow-studio] ❌ tokio runtime 构建失败: {e}");
            std::process::exit(1);
        });
    runtime.block_on(async move {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .unwrap_or_else(|e| {
                eprintln!(
                    "[flow-studio] ❌ 端口绑定失败 {addr}: {e}。自诊断指引: \
                     ① 确认端口未被占用（netstat -ano | findstr {port}）; \
                     ② 换用 --port 指定其他端口并同步修改 pack.json service.base_url"
                );
                std::process::exit(1);
            });
        tracing::info!("[flow-studio] 编译服务已启动: {addr}（契约 v1.1 §6）");
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("[flow-studio] ❌ 服务运行失败: {e}");
            std::process::exit(1);
        }
    });
}

async fn health_handler(State(_s): State<PluginState>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "plugin": "flow-studio",
        "version": env!("CARGO_PKG_VERSION")
    }))
}

/// 编译入口（契约 v1.1 §6 信封）：body = { "flow": ... } → 编译纯函数。
/// 编译失败 = 400 + 结构化 error（显式错误，不静默）；成功 = 200 信封。
async fn compile_handler(State(_s): State<PluginState>, Json(body): Json<Value>) -> Response {
    let Some(flow) = body.get("flow") else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "请求体缺 'flow' 字段（契约 v1.1 §6 信封: { \"flow\": ... }）" })),
        )
            .into_response();
    };
    match compile_flow(flow) {
        Ok(draft) => (
            StatusCode::OK,
            Json(json!({
                "rule_draft": draft,
                "compiler_version": env!("CARGO_PKG_VERSION"),
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn app() -> axum::Router {
        axum::Router::new()
            .route("/health", get(health_handler))
            .route("/v1/compile", post(compile_handler))
            .with_state(PluginState)
    }

    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn compile_envelope_roundtrip() {
        let flow = serde_json::json!({
            "flow_id": "f1",
            "nodes": [
                { "node_id": "n1", "node_type": "start" },
                { "node_id": "n2", "node_type": "approval",
                  "params": { "role": "CFO", "prompt": "p" },
                  "form_ref_resolved": { "scene": "s", "field": "amount",
                                         "path": "__exec__.payload.amount" },
                  "threshold": 100 },
                { "node_id": "n3", "node_type": "end" }
            ],
            "edges": [ { "from": "n1", "to": "n2" }, { "from": "n2", "to": "n3", "guard": "approved" } ]
        });
        let resp = app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/compile")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::json!({ "flow": flow }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["compiler_version"], json!(env!("CARGO_PKG_VERSION")));
        assert_eq!(v["rule_draft"]["id"], json!("f1"));
        assert_eq!(v["rule_draft"]["transform"][0]["type"], json!("branch"));
    }

    #[tokio::test]
    async fn compile_missing_flow_field_is_400() {
        let resp = app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/compile")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let v = body_json(resp).await;
        assert!(v["error"].as_str().unwrap().contains("flow"), "got: {v}");
    }

    #[tokio::test]
    async fn compile_error_is_explicit_400() {
        let resp = app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/compile")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::json!({ "flow": { "flow_id": "f" } }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let v = body_json(resp).await;
        assert!(v["error"].as_str().unwrap().contains("nodes"), "got: {v}");
    }
}

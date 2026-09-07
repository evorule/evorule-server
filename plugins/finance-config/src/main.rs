// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! finance-config 外部插件包 —— 自持 HTTP 服务进程（首个外部插件包范本）。
//!
//! 形态：独立进程 + 自持数据目录（--data）+ plugin.json 清单（SSOT）；
//! 装卸：plugin_manifest.json 登记一行 + 重启 server，宿主零代码改动零重编。
//!
//! 路由（与 server invoke/registry 管道同契约）：
//!   POST /services/finance_config_get     body=args → 结果 JSON
//!   POST /services/finance_config_set     body=args → 提案创建结果（sensitive，不落库）
//!   GET  /health                          存活探针
//! 管理面（Bearer FINANCE_PLUGIN_ADMIN_TOKEN；未配置 token 时 503 拒绝，不静默裸奔）：
//!   GET  /admin/proposals                 待批提案列表
//!   POST /admin/proposals/{id}/approve    body: {"approver": "..."}
//!   POST /admin/proposals/{id}/reject     body: {"approver": "...", "reason": "..."}

#![forbid(unsafe_code)]

mod services;
mod store;

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use serde_json::{json, Value};

use crate::store::ConfigStore;

#[derive(Clone)]
struct PluginState {
    store: Arc<ConfigStore>,
    /// 管理面 token（None = 未配置 → 管理面 503 拒绝，fail-fast 不静默）
    admin_token: Option<String>,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // 参数解析（轻量手写，插件包不引入 clap 依赖）
    let mut port: u16 = 9110;
    let mut data_dir = PathBuf::from("data");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => {
                port = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| {
                        eprintln!("[finance-config] --port 需要一个端口号");
                        std::process::exit(2);
                    });
            }
            "--data" => {
                data_dir = PathBuf::from(args.next().unwrap_or_else(|| {
                    eprintln!("[finance-config] --data 需要一个目录路径");
                    std::process::exit(2);
                }));
            }
            "--help" | "-h" => {
                println!("finance-config plugin --port <9110> --data <./data>");
                println!("管理面 token: 环境变量 FINANCE_PLUGIN_ADMIN_TOKEN（未配置则管理面不可用）");
                return;
            }
            other => {
                eprintln!("[finance-config] 未知参数 {other}（--help 查看用法）");
                std::process::exit(2);
            }
        }
    }

    let store_path = data_dir.join("config_store.json");
    let store = match ConfigStore::open(&store_path) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("[finance-config] ❌ Store 初始化失败: {e}。自诊断指引: \
                ① 确认 --data 目录可写; \
                ② 检查 config_store.json 格式（JSON 损坏时删除后由系统重建）");
            std::process::exit(1);
        }
    };

    let admin_token = std::env::var("FINANCE_PLUGIN_ADMIN_TOKEN").ok();
    if admin_token.is_none() {
        tracing::warn!(
            "FINANCE_PLUGIN_ADMIN_TOKEN 未配置 — 管理面（提案审批）不可用（503）；\
             服务面 finance_config_get/set 不受影响"
        );
    }

    let state = PluginState { store, admin_token };
    let app = axum::Router::new()
        .route("/health", get(health_handler))
        .route(
            "/services/finance_config_get",
            post(get_service_handler),
        )
        .route(
            "/services/finance_config_set",
            post(set_service_handler),
        )
        .route("/admin/proposals", get(list_proposals_handler))
        .route(
            "/admin/proposals/{id}/approve",
            post(approve_proposal_handler),
        )
        .route(
            "/admin/proposals/{id}/reject",
            post(reject_proposal_handler),
        )
        .with_state(state);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime 构建失败");
    runtime.block_on(async move {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .unwrap_or_else(|e| {
                eprintln!(
                    "[finance-config] ❌ 端口绑定失败 {addr}: {e}。自诊断指引: \
                     ① 确认端口未被占用（netstat -ano | findstr {}）; \
                     ② 换用 --port 指定其他端口",
                    port
                );
                std::process::exit(1);
            });
        tracing::info!(
            "[finance-config] 插件服务已启动: {addr}（数据: {}）",
            store_path.display()
        );
        axum::serve(listener, app).await.expect("服务运行失败");
    });
}

async fn health_handler() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "plugin": "finance-config",
        "version": env!("CARGO_PKG_VERSION")
    }))
}

/// 服务面统一入口：body = args 原样透传，响应 = 结果 JSON。
/// 错误形态：HTTP 200 + 结构化错误字段（与原生插件 IoResult 语义一致，
/// 业务错误是服务的合法返回而非传输错误）。
async fn get_service_handler(
    State(state): State<PluginState>,
    Json(args): Json<Value>,
) -> Json<Value> {
    Json(services::config_get(&state.store, &args))
}

async fn set_service_handler(
    State(state): State<PluginState>,
    Json(args): Json<Value>,
) -> Json<Value> {
    Json(services::config_set(&state.store, &args))
}

/// 管理面 token 守卫：`Authorization: Bearer <token>`；未配置 token → 503。
fn check_admin(state: &PluginState, headers: &HeaderMap) -> Result<(), Response> {
    let Some(expected) = state.admin_token.as_ref() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": "管理面不可用: FINANCE_PLUGIN_ADMIN_TOKEN 未配置（fail-fast，不静默放行）"
            })),
        )
            .into_response());
    };
    let ok = headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|h| h.strip_prefix("Bearer ") == Some(expected.as_str()));
    if ok {
        Ok(())
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "管理面认证失败: 缺失或错误的 Bearer token" })),
        )
            .into_response())
    }
}

async fn list_proposals_handler(
    State(state): State<PluginState>,
    headers: HeaderMap,
) -> Result<Json<Value>, Response> {
    check_admin(&state, &headers)?;
    let proposals = state.store.pending_proposals();
    Ok(Json(json!({ "pending": proposals, "count": proposals.len() })))
}

async fn approve_proposal_handler(
    State(state): State<PluginState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, Response> {
    check_admin(&state, &headers)?;
    let approver = body
        .get("approver")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if approver.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "必须提供 body.approver（审批操作者身份，入审计）" })),
        )
            .into_response());
    }
    match state.store.approve_proposal(&id, approver) {
        Ok(()) => Ok(Json(json!({
            "success": true,
            "proposal_id": id,
            "approved_by": approver,
            "message": "提案已批准，配置值落库"
        }))),
        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e })),
        )
            .into_response()),
    }
}

async fn reject_proposal_handler(
    State(state): State<PluginState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, Response> {
    check_admin(&state, &headers)?;
    let approver = body
        .get("approver")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if approver.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "必须提供 body.approver（审批操作者身份，入审计）" })),
        )
            .into_response());
    }
    let _ = body.get("reason").and_then(|v| v.as_str());
    match state.store.reject_proposal(&id, approver) {
        Ok(()) => Ok(Json(json!({
            "success": true,
            "proposal_id": id,
            "approved_by": approver,
            "message": "提案已拒绝，配置值保持不变"
        }))),
        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e })),
        )
            .into_response()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn test_state() -> (PluginState, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "evorule-finance-plugin-http-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(ConfigStore::open(&dir.join("config_store.json")).unwrap());
        (
            PluginState {
                store,
                admin_token: Some("test-token".to_string()),
            },
            dir,
        )
    }

    fn app(state: PluginState) -> axum::Router {
        axum::Router::new()
            .route("/health", get(health_handler))
            .route("/services/finance_config_get", post(get_service_handler))
            .route("/services/finance_config_set", post(set_service_handler))
            .route("/admin/proposals", get(list_proposals_handler))
            .route("/admin/proposals/{id}/approve", post(approve_proposal_handler))
            .route("/admin/proposals/{id}/reject", post(reject_proposal_handler))
            .with_state(state)
    }

    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn service_get_contract_body_args_result_json() {
        let (state, _dir) = test_state();
        // 空参数 → 结构化错误（CONFIG_KEY_EMPTY），HTTP 200（业务错误非传输错误）
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/services/finance_config_get")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["error"], json!("CONFIG_KEY_EMPTY"));
    }

    #[tokio::test]
    async fn set_then_admin_approve_then_get_value_visible() {
        // 治理闭环 e2e（HTTP 层）: set→提案→管理面批准→get 回读成功值
        let (state, _dir) = test_state();
        let app = app(state.clone());

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/services/finance_config_set")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"key":"config:limits.travel.max_amount","new_value":2000,"reason":"测试","proposed_by":"tester"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["awaiting_approval"], json!(true));
        let pid = v["proposal_id"].as_str().unwrap().to_string();

        // 待批列表（带 token）
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/admin/proposals")
                    .header("Authorization", "Bearer test-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["count"], json!(1));
        assert_eq!(v["pending"][0]["key"], json!("limits.travel.max_amount"));

        // 审批落库
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/admin/proposals/{pid}/approve"))
                    .header("content-type", "application/json")
                    .header("Authorization", "Bearer test-token")
                    .body(Body::from(r#"{"approver":"finance_dir"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // get 回读成功值
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/services/finance_config_get")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"key":"config:limits.travel.max_amount"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let v = body_json(resp).await;
        assert_eq!(v["exists"], json!(true));
        assert_eq!(v["value"], json!(2000));
    }

    #[tokio::test]
    async fn admin_requires_token_and_rejects_wrong_token() {
        let (state, _dir) = test_state();
        let app = app(state);

        // 无 token → 401
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/proposals")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // 错 token → 401
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/admin/proposals")
                    .header("Authorization", "Bearer wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn admin_returns_503_when_token_unconfigured() {
        let (mut state, _dir) = test_state();
        state.admin_token = None;
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .uri("/admin/proposals")
                    .header("Authorization", "Bearer whatever")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}

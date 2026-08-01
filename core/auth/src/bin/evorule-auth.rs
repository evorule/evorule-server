// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 认证服务入口(独立 HTTP 代理)
//!
//! 暴露 4 个端点:
//! - `GET  /validate?token=xxx` — 验证 token
//! - `GET  /tokens`            — 列举 token(**掩码**,不含明文)
//! - `POST /tokens`            — 生成并**持久化** token
//! - `POST /tokens/generate`   — 仅生成 token(**不持久化**,供预览格式)
//!
//! # 已知设计缺口
//! `/tokens/generate` 与 `POST /tokens` 无法组合:前者返回的 token 无法回填到后者
//! (后者会重新生成)。如需"客户端自选 token + 持久化",应扩展 `POST /tokens`
//! 支持可选 `token` 字段。本审计不做该行为变更,仅记录。

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use axum::{extract::{State, Query}, routing::{get, post}, Json, Router};
use clap::Parser;
use evorule_auth::{AuthResponse, AuthService, TokenInfo};
use std::sync::Arc;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser, Debug)]
#[command(name = "evorule-auth")]
#[command(about = "认证代理服务", long_about = None)]
struct Args {
    #[arg(long, default_value = "8082")]
    api_port: u16,

    #[arg(long)]
    token: Vec<String>,
}

#[derive(serde::Deserialize)]
struct ValidateQuery {
    token: String,
}

#[derive(serde::Deserialize)]
struct AddTokenRequest {
    description: String,
    expires_hours: Option<u64>,
}

#[tokio::main]
async fn main() -> Result<(), String> {
    tracing_subscriber::registry()
        .with(fmt::layer().with_target(false))
        .init();

    let args = Args::parse();

    // 初始化 token 列表
    let mut tokens = Vec::new();
    for (i, token) in args.token.iter().enumerate() {
        tokens.push(TokenInfo {
            token: token.clone(),
            created_at: chrono::Utc::now(),
            expires_at: None,
            description: format!("命令行配置 token {}", i + 1),
        });
    }

    let service = Arc::new(AuthService::new(tokens));

    let app = Router::new()
        .route("/validate", get(validate_handler))
        .route("/tokens", get(list_tokens_handler))
        .route("/tokens", post(add_token_handler))
        .route("/tokens/generate", post(generate_token_handler))
        .with_state(service);

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], args.api_port));
    let listener = tokio::net::TcpListener::bind(&addr).await
        .map_err(|e| format!("绑定地址失败: {}", e))?;

    tracing::info!(port = args.api_port, "HTTP API 服务器已启动");
    tracing::info!("端点:");
    tracing::info!("  GET  http://{}:{}/validate?token=xxx", "127.0.0.1", args.api_port);
    tracing::info!("  GET  http://{}:{}/tokens", "127.0.0.1", args.api_port);
    tracing::info!("  POST http://{}:{}/tokens", "127.0.0.1", args.api_port);
    tracing::info!("  POST http://{}:{}/tokens/generate", "127.0.0.1", args.api_port);

    axum::serve(listener, app.into_make_service()).await
        .map_err(|e| format!("启动服务器失败: {}", e))?;

    Ok(())
}

/// `GET /validate?token=xxx` — 返回验证结果
async fn validate_handler(
    State(service): State<Arc<AuthService>>,
    Query(params): Query<ValidateQuery>,
) -> Json<AuthResponse> {
    Json(service.validate_token(&params.token))
}

/// `GET /tokens` — 返回所有 token 的**掩码**视图(不含明文)
async fn list_tokens_handler(State(service): State<Arc<AuthService>>) -> Json<Vec<evorule_auth::TokenInfoMasked>> {
    Json(service.list_tokens())
}

/// `POST /tokens` — 生成并持久化 token
async fn add_token_handler(
    State(service): State<Arc<AuthService>>,
    Json(req): Json<AddTokenRequest>,
) -> Json<AuthResponse> {
    let token = AuthService::generate_token(&req.description, req.expires_hours);
    match service.add_token(token.clone()) {
        Ok(_) => Json(AuthResponse {
            valid: true,
            message: "Token 添加成功".to_string(),
            token_info: Some(token),
        }),
        Err(e) => Json(AuthResponse {
            valid: false,
            message: e,
            token_info: None,
        }),
    }
}

/// `POST /tokens/generate` — 仅生成 token(**不持久化**,供预览)
async fn generate_token_handler(
    State(_service): State<Arc<AuthService>>,
    Json(req): Json<AddTokenRequest>,
) -> Json<TokenInfo> {
    Json(AuthService::generate_token(&req.description, req.expires_hours))
}

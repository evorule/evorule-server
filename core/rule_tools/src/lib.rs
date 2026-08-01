// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! EvoRule 规则工具 —— 验证器 + 安全分析
//!
//! 从 evorule-reactor 移出的 rule_validator 和 rule_safety 合并实现。
//! 纯静态分析，不依赖 tier1/tier2 运行时。
//!
//! # 指令类型白名单
//!
//! ## 元指令（tier0 核心）
//! - `set`: 设置属性（params: attr, operation, value）
//! - `push`: 推入队列（params: instructions）
//! - `branch`: 条件分支（params: domain, on_true, on_false）
//! - `io_request`: I/O 请求（params: io_type）
//!
//! ## 控制流指令（tier1）
//! - `sequence`: 顺序执行（params: instructions）
//! - `conditional`: 条件执行（params: domain, then, else）
//! - `while_loop`: 循环（params: domain, body）
//! - `call_rule`: 调用规则（params: rule）
//!
//! ## 域类型（条件）
//! - `eq`, `lt`, `gt`, `exists`, `instruction`, `all`, `not`
//!
//! # HTTP API
//!
//! - `POST /validate` (body: 规则 JSON 内容) — 验证规则结构
//! - `GET  /validate?file=path` — 验证指定文件（路径穿越被拒绝）
//! - `POST /safety` (body: 规则 JSON 内容) — 安全分析
//! - `GET  /safety?file=path` — 安全分析指定文件（路径穿越被拒绝）

#![forbid(unsafe_code)]
// C5 (unwrap/expect/panic = deny) 仅约束生产代码;测试代码保留 unwrap 惯例
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::post;
use axum::Router;
use serde::Deserialize;
use std::path::{Component, PathBuf};
use tracing::info;

pub mod safety;
pub mod validator;

pub use safety::{analyze_rule_safety, SafetyIssue, SafetyReport, SafetySeverity};
pub use validator::{validate_rule_json, ValidationReport, ValidationResult, ValidationSeverity};

// ============ HTTP API ============

/// 无状态 API 上下文（验证器/安全分析均为纯函数，无需共享状态）
#[derive(Clone)]
struct ApiState;

/// `GET ?file=` 查询参数
#[derive(Deserialize)]
struct FileQuery {
    file: String,
}

/// 校验文件路径，拒绝路径穿越（`..` 组件）
///
/// `GET /validate?file=../../etc/passwd` 类请求会被拒绝并返回 400。
/// 仅允许相对于当前工作目录的平铺路径。
fn validate_file_path(file: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(file);
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("路径不允许包含 '..'".to_string());
    }
    Ok(path)
}

/// `POST /validate` — body 为规则 JSON 内容
async fn validate_body(body: String) -> Json<ValidationReport> {
    let report = validate_rule_json(&body);
    Json(report)
}

/// `POST /safety` — body 为规则 JSON 内容
async fn safety_body(body: String) -> Json<SafetyReport> {
    let report = analyze_rule_safety(&body);
    Json(report)
}

/// `GET /validate?file=path`
async fn validate_file(
    State(_): State<ApiState>,
    Query(params): Query<FileQuery>,
) -> Result<Json<ValidationReport>, (StatusCode, String)> {
    let path = validate_file_path(&params.file).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let content = std::fs::read_to_string(&path)
        .map_err(|e| (StatusCode::NOT_FOUND, format!("读取文件失败: {e}")))?;
    let report = validate_rule_json(&content);
    Ok(Json(report))
}

/// `GET /safety?file=path`
async fn safety_file(
    State(_): State<ApiState>,
    Query(params): Query<FileQuery>,
) -> Result<Json<SafetyReport>, (StatusCode, String)> {
    let path = validate_file_path(&params.file).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let content = std::fs::read_to_string(&path)
        .map_err(|e| (StatusCode::NOT_FOUND, format!("读取文件失败: {e}")))?;
    let report = analyze_rule_safety(&content);
    Ok(Json(report))
}

/// 构建路由（公开以便测试）
pub fn build_router() -> Router {
    Router::new()
        .route("/validate", post(validate_body).get(validate_file))
        .route("/safety", post(safety_body).get(safety_file))
        .with_state(ApiState)
}

/// 启动 HTTP API 服务
#[allow(clippy::cognitive_complexity)]
pub async fn run_server(port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let app = build_router();
    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("规则工具 HTTP API 已启动 addr={addr}");
    let endpoints = [
        "POST /validate  (body: 规则 JSON)",
        "GET  /validate?file=path",
        "POST /safety    (body: 规则 JSON)",
        "GET  /safety?file=path",
    ];
    for ep in endpoints {
        info!("  {ep}");
    }
    Ok(axum::serve(listener, app).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    /// 辅助: 发送请求并返回 (status, body_text)
    async fn send_request(router: Router, req: Request<Body>) -> (StatusCode, String) {
        let response = router.oneshot(req).await.expect("oneshot failed");
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body collect failed")
            .to_bytes();
        let text = String::from_utf8_lossy(&body).to_string();
        (status, text)
    }

    fn valid_rule_body() -> String {
        serde_json::json!({
            "rule_id": "test-001",
            "rules": [{
                "name": "main",
                "instruction": {
                    "type": "set",
                    "params": {"attr": "x", "value": 1}
                }
            }]
        })
        .to_string()
    }

    // ===== POST /validate =====

    #[tokio::test]
    async fn handler_validate_body_success() {
        let app = build_router();
        let req = Request::builder()
            .method("POST")
            .uri("/validate")
            .header("content-type", "application/json")
            .body(Body::from(valid_rule_body()))
            .unwrap();
        let (status, body) = send_request(app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("test-001"));
        assert!(body.contains("\"valid\":true"));
    }

    #[tokio::test]
    async fn handler_validate_body_invalid_json() {
        let app = build_router();
        let req = Request::builder()
            .method("POST")
            .uri("/validate")
            .header("content-type", "application/json")
            .body(Body::from("{ bad json }"))
            .unwrap();
        let (status, body) = send_request(app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("JSON 解析失败"));
        assert!(body.contains("\"valid\":false"));
    }

    // ===== POST /safety =====

    #[tokio::test]
    async fn handler_safety_body_success() {
        let app = build_router();
        let req = Request::builder()
            .method("POST")
            .uri("/safety")
            .header("content-type", "application/json")
            .body(Body::from(valid_rule_body()))
            .unwrap();
        let (status, body) = send_request(app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"safe\":true"));
    }

    // ===== GET /validate?file= =====

    #[tokio::test]
    async fn handler_validate_file_traversal_rejected() {
        let app = build_router();
        let req = Request::builder()
            .method("GET")
            .uri("/validate?file=../../../etc/passwd")
            .body(Body::empty())
            .unwrap();
        let (status, body) = send_request(app, req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains(".."));
    }

    #[tokio::test]
    async fn handler_validate_file_not_found() {
        let app = build_router();
        let req = Request::builder()
            .method("GET")
            .uri("/validate?file=nonexistent_rule.json")
            .body(Body::empty())
            .unwrap();
        let (status, _body) = send_request(app, req).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn handler_safety_file_traversal_rejected() {
        let app = build_router();
        let req = Request::builder()
            .method("GET")
            .uri("/safety?file=../../secret")
            .body(Body::empty())
            .unwrap();
        let (status, _body) = send_request(app, req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}

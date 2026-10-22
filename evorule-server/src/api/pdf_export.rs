// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 服务端 PDF 导出端点（HTTP 薄壳）——渲染引擎正本已下沉 evorule-export-services
//! 插件（plugins/export-services/src/pdf.rs）。同一引擎双入口：
//! - 原生服务 `export_report_pdf`：落盘导出目录并返回 {path,bytes,blake3} 信封
//!   （agent/MCP/规则内消费面）；
//! - 本端点 `POST /api/export/pdf`：二进制直返不落盘（console PdfRenderer 消费面，
//!   契约对齐 export-renderers.ts）。

use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::Value;

use crate::api::server::AppState;

// 引擎与请求契约（插件侧 SSOT 重导出；main.rs 的 --pdf-font 经 set_font_override 注入）
pub use evorule_export_services::pdf::{render_pdf, set_font_override, PdfExportRequest};

/// 请求体上限（32MB）：console 可携带全量审计事实（raw_data），axum 默认 2MB 不够用。
/// PDF 是摘要型渲染，超限由客户端裁剪（includeRaw 关闭）或走 JSON 导出。
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

/// 构造 `/api/export/pdf` 路由（挂入受认证保护路由组；body 上限放宽到 32MB）
pub fn pdf_export_router() -> Router<AppState> {
    Router::new().route(
        "/api/export/pdf",
        post(pdf_export_handler).layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES)),
    )
}

/// 统一错误响应：`{ "success": false, "message": ... }`（与 permissions/marketplace 同形状）
fn err(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<Value>) {
    (
        status,
        Json(serde_json::json!({ "success": false, "message": message.into() })),
    )
}

/// `POST /api/export/pdf` → 服务端渲染 PDF
///
/// - 200：`application/pdf` 二进制
/// - 400：请求体非法 / 缺 content_type / 字体缺请求字符（带指引）
/// - 500：无可用中文字体 / 渲染内部错误（带自诊断指引）
#[utoipa::path(
    post,
    path = "/api/export/pdf",
    tag = "export",
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "渲染成功，返回 application/pdf 二进制"),
        (status = 400, description = "请求体非法 / 缺 content_type / 字体缺字符（message 带指引）", body = serde_json::Value),
        (status = 500, description = "无可用中文字体 / 渲染失败（message 带自诊断指引）", body = serde_json::Value)
    )
)]
async fn pdf_export_handler(
    payload: Result<Json<PdfExportRequest>, JsonRejection>,
) -> Result<Response, (StatusCode, Json<Value>)> {
    // Json rejection → 统一错误形状（不用 axum 默认文本，保持消费方可解析）
    let Json(req) =
        payload.map_err(|rej| err(StatusCode::BAD_REQUEST, format!("请求体解析失败: {rej}")))?;

    // 请求级可见性：server 无通用 access log，端点自带一行请求日志（消费方排障用）
    tracing::info!(
        "PDF 导出请求: content_type={}, raw_data={} 字节级数据, options={:?}",
        req.content_type,
        req.raw_data.is_some(),
        req.options
            .as_ref()
            .map(|o| o.pdf_title.is_some())
            .unwrap_or(false)
    );

    match render_pdf(&req) {
        Ok(bytes) => {
            tracing::info!("PDF 导出成功: {} 字节", bytes.len());
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/pdf")
                .body(Body::from(bytes))
                .map_err(|e| {
                    err(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("响应构造失败: {e}"),
                    )
                })?)
        }
        Err(msg) => {
            // 字体环境问题 → 500；请求内容问题（缺字符/缺 content_type）→ 400
            let status = if msg.contains("缺少 content_type")
                || msg.contains("缺少以下字符的字形")
                || msg.contains("渲染字体缺少")
            {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            tracing::warn!("PDF 导出失败（不静默）: {msg}");
            Err(err(status, msg))
        }
    }
}

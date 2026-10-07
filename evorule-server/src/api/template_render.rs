// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `POST /api/templates/render` —— 模板渲染 REST 消费面
//!
//! 与 NativeService `template_render` 共用同一渲染引擎 SSOT
//! （`evorule_template_services::render`），行为逐字节一致。
//!
//! - 纯函数无落库：不写会话/审计链（与 pack 模板 generate 同口径）；
//!   规则内消费走 `io_request(call_service, service_name=template_render)`
//!   （既有 IO 审计通路落账）。
//! - 请求体：`{ "template": "...", "context": {...}, "format": "text|markdown|json" }`
//!   - `template` 必填（非空，≤1MiB 引擎预算）；
//!   - `context` 可选对象（缺省空对象；数值域经 TCB 值域转换——浮点原样字符串化，
//!     与 HTTP 服务注册表回落同一转换 SSOT）；
//!   - `format` 可选 `text|markdown|json`（缺省 `text`；`json` 时产物合法性校验）。
//! - 成功：`{"success":true,"format":...,"content":...}`；
//!   失败：4xx `{"success":false,"message":...}`（与 permissions/marketplace 同形状）。

use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use evorule_io_handlers::service_registry::serde_to_json_value;
use evorule_tcb::JsonValue;
use evorule_template_services::render::{render_checked, Format};
use serde::Deserialize;
use serde_json::Value;

use crate::api::server::AppState;

/// 模板渲染请求体
#[derive(Deserialize)]
pub struct TemplateRenderRequest {
    template: Option<String>,
    context: Option<Value>,
    format: Option<String>,
}

/// 构造 `/api/templates/render` 路由（挂入受认证保护路由组）
pub fn template_render_router() -> Router<AppState> {
    Router::new().route("/api/templates/render", post(template_render_handler))
}

/// 统一错误响应：`{ "success": false, "message": ... }`（与 permissions/marketplace 同形状）
fn err(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<Value>) {
    (
        status,
        Json(serde_json::json!({ "success": false, "message": message.into() })),
    )
}

/// `POST /api/templates/render` → 确定性模板渲染
///
/// - 200：`{"success":true,"format":<声明值>,"content":<渲染文本>}`
/// - 400：请求体非法 / 模板语法错误 / 变量缺失 / 渲染预算超限（message 带自诊断指引）
#[utoipa::path(
    post,
    path = "/api/templates/render",
    tag = "templates",
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "渲染成功，返回声明的形态与渲染文本", body = serde_json::Value),
        (status = 400, description = "请求非法 / 模板语法错误 / 变量缺失 / 预算超限（message 带指引）", body = serde_json::Value)
    )
)]
pub async fn template_render_handler(
    Json(req): Json<TemplateRenderRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(template) = req.template.as_deref() else {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "缺 template 字段 — 需为字符串",
        ));
    };
    if template.is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "template 为空字符串 — 空模板无渲染语义（fail-fast）",
        ));
    }
    let context = match req.context {
        None | Some(Value::Null) => JsonValue::empty_object(),
        Some(v) if v.is_object() => serde_to_json_value(v),
        Some(_) => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "context 需为对象（缺省空对象）— 形如 {\"k\": v}",
            ));
        }
    };
    let format = match req.format.as_deref() {
        None | Some("") => Format::Text,
        Some(f) => Format::parse(f).map_err(|e| err(StatusCode::BAD_REQUEST, e))?,
    };
    match render_checked(template, &context, format) {
        Ok(rendered) => Ok(Json(serde_json::json!({
            "success": true,
            "format": rendered.format.as_str(),
            "content": rendered.content,
        }))),
        Err(e) => Err(err(StatusCode::BAD_REQUEST, e)),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    async fn call(body: Value) -> Result<Value, (StatusCode, Value)> {
        let req: TemplateRenderRequest =
            serde_json::from_value(body).expect("测试请求体须可反序列化");
        match template_render_handler(Json(req)).await {
            Ok(Json(v)) => Ok(v),
            Err((status, Json(v))) => Err((status, v)),
        }
    }

    #[tokio::test]
    async fn test_render_ok_text() {
        let v = call(serde_json::json!({
            "template": "你好, {{name}}!{% if vip %} VIP{% endif %}",
            "context": { "name": "张三", "vip": true }
        }))
        .await
        .unwrap();
        assert_eq!(v["success"], serde_json::json!(true));
        assert_eq!(v["format"], serde_json::json!("text"));
        assert_eq!(v["content"], serde_json::json!("你好, 张三! VIP"));
    }

    #[tokio::test]
    async fn test_render_ok_json_format_validated() {
        let v = call(serde_json::json!({
            "template": "{\"customer\": \"{{name}}\"}",
            "context": { "name": "李四" },
            "format": "json"
        }))
        .await
        .unwrap();
        assert_eq!(v["format"], serde_json::json!("json"));
        assert_eq!(v["content"], serde_json::json!("{\"customer\": \"李四\"}"));
    }

    #[tokio::test]
    async fn test_render_float_context_stringified_cross_face_consistency() {
        // 浮点经 TCB 值域转换（与 HTTP 服务注册表回落同一 SSOT）→ 原样字符串化
        let v = call(serde_json::json!({
            "template": "{{price}}",
            "context": { "price": 1.5 }
        }))
        .await
        .unwrap();
        assert_eq!(v["content"], serde_json::json!("1.5"));
    }

    #[tokio::test]
    async fn test_render_markdown_table() {
        let v = call(serde_json::json!({
            "template": "{% for o in orders %}| {{o.id}} | {{o.total}} |\n{% endfor %}",
            "context": { "orders": [ { "id": 1, "total": 30 }, { "id": 2, "total": 45 } ] },
            "format": "markdown"
        }))
        .await
        .unwrap();
        assert_eq!(v["format"], serde_json::json!("markdown"));
        assert_eq!(v["content"], serde_json::json!("| 1 | 30 |\n| 2 | 45 |\n"));
    }

    #[tokio::test]
    async fn test_render_error_contracts() {
        // 缺 template
        let (status, v) = call(serde_json::json!({})).await.unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(v["success"], serde_json::json!(false));
        assert!(v["message"].as_str().unwrap().contains("缺 template"));
        // 空 template
        let (status, v) = call(serde_json::json!({ "template": "" }))
            .await
            .unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(v["message"].as_str().unwrap().contains("为空"));
        // 非法 format（词表封闭）
        let (status, _) = call(serde_json::json!({ "template": "t", "format": "yaml" }))
            .await
            .unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        // context 非对象
        let (status, v) = call(serde_json::json!({ "template": "t", "context": [1] }))
            .await
            .unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(v["message"].as_str().unwrap().contains("context 需为对象"));
        // 模板渲染错误透传（变量缺失 fail-fast，位置诊断）
        let (_, v) = call(serde_json::json!({ "template": "x={{nope}}" }))
            .await
            .unwrap_err();
        let msg = v["message"].as_str().unwrap();
        assert!(msg.contains("变量未找到") && msg.contains("nope"), "{msg}");
        // json 形态产物校验失败 → 400
        let (_, v) = call(serde_json::json!({
            "template": "{\"k\": \"{{s}}\"}",
            "context": { "s": "a\"b" },
            "format": "json"
        }))
        .await
        .unwrap_err();
        assert!(v["message"].as_str().unwrap().contains("不是合法 JSON"));
    }
}

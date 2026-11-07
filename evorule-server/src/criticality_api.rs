// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 验证注入面对外端点 —— `POST /v1/criticality/verify`
//!
//! # 定位
//! 测量命题验证入口：接收 [`evorule_criticality::VerifyRequest`] 命题
//! （主张值 + SI 单位 + 不确定度 + 溯源上下文），以 law_host 装配的定律
//! 与常量集为参照执行纯函数验证管线，产出三态判定 + 逐步证据 + 提案
//! 物料包。挂 protected_routes（统一鉴权/限流/体限层之后）。
//!
//! # 安全面（10 号设计稿 §3 注入面纪律）
//! - **判定只读（铁律 3）**：handler 对装配面零写路径——law_host 为启动期
//!   构建的不可变共享态，本端点仅调用其只读查询；判定结果只出提案
//!   物料包（人工经治理链入闸），无任何自动回写通道。
//! - **结构 fail-fast**：schema 校验 + 数值有限性/幅值 + 单位可解析
//!   全部前置；量纲不一致/超限/非法结构一律 422 结构化报因，不静默。
//! - **DoS 硬上限**：表达式长度/括号深度/标识符数/数值幅值/主张条数/
//!   定律扫描集上限由 core/criticality 管线统一执行；传输面体限/限流
//!   由挂载层统一执法。
//! - **缺省不变式**：law_host 未配置 = 503 明示（不降级为空判定）。

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::api::server::AppState;
use evorule_criticality::{ConstantRef, LawRef, VerifyError};

/// 端点路由（挂 protected_routes：鉴权/限流/体限由装配层统一执法）
pub fn router() -> Router<AppState> {
    Router::new().route("/v1/criticality/verify", post(verify_handler))
}

/// 错误响应体（统一结构：code + message + detail）
fn error_body(code: &str, message: String, detail: Value) -> Value {
    json!({
        "error": {
            "code": code,
            "message": message,
            "detail": detail,
        }
    })
}

/// 拒判错误 → 422 响应（结构化报因：kind + 逐条 detail）
fn rejection(err: &VerifyError) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(error_body(
            "verify_rejected",
            err.summary(),
            serde_json::to_value(err).unwrap_or(Value::Null),
        )),
    )
        .into_response()
}

/// `POST /v1/criticality/verify`
///
/// 请求体 = verify_request/v1.0 命题文档；响应 = 三态判定报告
/// （{verdict, steps, proposals}）或结构化拒因。
async fn verify_handler(State(app): State<AppState>, Json(doc): Json<Value>) -> Response {
    // 装配前置：law_host 未配置 = 明示不可用（不降级为空判定）
    let Some(host) = app.law_host() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(error_body(
                "law_host_not_configured",
                "law_set 装配未配置：验证注入面需要 law 集与常量集装配后可用".to_string(),
                Value::Null,
            )),
        )
            .into_response();
    };

    // 结构层 fail-fast（schema + 有限性/幅值 + 单位可解析）
    let schema_report = evorule_criticality::validate_verify_request(&doc);
    if !schema_report.valid {
        let err = VerifyError::Schema {
            errors: schema_report.errors,
        };
        return rejection(&err);
    }
    let request: evorule_criticality::VerifyRequest = match serde_json::from_value(doc) {
        Ok(r) => r,
        Err(e) => {
            return rejection(&VerifyError::Schema {
                errors: vec![format!("命题反序列化失败: {e}")],
            });
        }
    };

    // 装配面注入（只读借用视图；dataset 序 = BTreeMap 序，确定性）
    let ds_ids = host.dataset_ids();
    let mut laws: Vec<LawRef<'_>> = Vec::new();
    for ds in &ds_ids {
        for law in host.laws_in(ds) {
            laws.push(LawRef {
                dataset_id: ds,
                law,
            });
        }
    }
    let constants: Vec<ConstantRef<'_>> = host
        .constant_refs()
        .into_iter()
        .map(|(dataset_id, constant)| ConstantRef {
            dataset_id,
            constant,
        })
        .collect();

    // 纯函数管线（判定只读：装配面零写路径；铁律 3 由 LawHost 不可变性保证）
    match evorule_criticality::verify(&request, &laws, &constants) {
        Ok(report) => (StatusCode::OK, Json(report)).into_response(),
        Err(err) => rejection(&err),
    }
}

// ============================================================================
// 测试（错误体形态；端点级行为测试在 api/server.rs tests，与既有
// 401/413 测试同模块共用测试装配面）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_body_shape_stable() {
        let body = error_body("verify_rejected", "拒绝原因".to_string(), json!({"k": 1}));
        assert_eq!(body["error"]["code"], "verify_rejected");
        assert_eq!(body["error"]["message"], "拒绝原因");
        assert_eq!(body["error"]["detail"]["k"], 1);
    }

    #[test]
    fn rejection_maps_to_422_with_structured_detail() {
        let err = VerifyError::LawScanLimit {
            count: 65,
            limit: 64,
        };
        let resp = rejection(&err);
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
}

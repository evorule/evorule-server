// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 权限管理 API —— `/api/permissions` 端点族（应用层策略）
//!
//! 基于治理层 [`evorule_governance::permission`] 的机制（`PermissionTable` / `PermissionEntry`），
//! 暴露：
//! - CRUD：`GET/POST /api/permissions`、`GET/PUT/DELETE /api/permissions/{id}`
//! - 审批流：`POST /api/permissions/{id}/submit`、`POST /api/permissions/{id}/review`
//! - 测试：`GET /api/permissions/version`、`POST /api/permissions/evaluate`
//!
//! 数据持久化在 `SharedFactsLog` 的 `shared.__permission__.entry.*` 路径下，
//! 每次写操作追加新版本，保证可审计回放。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::api::server::AppState;

use evorule_governance::permission::{PermissionEntry, PermissionState, PermissionTable, Verdict};
use evorule_governance::shared_facts_log::SharedFactsLog;
use evorule_reactor::{CallerRole, FactId, IoCallContext};
use serde::Deserialize;

/// 权限条目写入所用的来源会话 ID（0 = 系统/全局，非任何真实会话）
const GLOBAL_SESSION: u64 = 0;

/// 构造 `/api/permissions` 的路由（挂入受认证保护路由组）
pub fn permissions_router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/permissions",
            get(list_permissions).post(create_permission),
        )
        .route(
            "/api/permissions/{id}",
            get(get_permission)
                .put(update_permission)
                .delete(delete_permission),
        )
        .route("/api/permissions/version", get(permissions_version))
        .route("/api/permissions/evaluate", post(evaluate_permission))
        .route("/api/permissions/{id}/submit", post(submit_permission))
        .route("/api/permissions/{id}/review", post(review_permission))
}

/// 统一错误响应：`{ "success": false, "message": ... }`
fn err(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (
        status,
        Json(serde_json::json!({ "success": false, "message": message.into() })),
    )
}

/// 团结快照错误
fn snapshot(
    shared: &SharedFactsLog,
) -> Result<PermissionTable, (StatusCode, Json<serde_json::Value>)> {
    PermissionTable::snapshot_at(shared, shared.version())
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// `GET /api/permissions` → 列出全部权限条目（含当前版本号）
async fn list_permissions(
    State(shared): State<SharedFactsLog>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let table = snapshot(&shared)?;
    let entries = serde_json::to_value(table.entries()).unwrap_or(serde_json::Value::Null);
    Ok(Json(serde_json::json!({
        "success": true,
        "version": table.version(),
        "count": table.entries().len(),
        "entries": entries,
    })))
}

/// `GET /api/permissions/{id}` → 查询单条权限条目
async fn get_permission(
    State(shared): State<SharedFactsLog>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let table = snapshot(&shared)?;
    match table.get(&id) {
        Some(entry) => {
            let body = serde_json::to_value(entry).unwrap_or(serde_json::Value::Null);
            Ok(Json(serde_json::json!({ "success": true, "entry": body })))
        }
        None => Err(err(
            StatusCode::NOT_FOUND,
            format!("permission entry not found: {id}"),
        )),
    }
}

/// 校验主体字段合法（非任何主体必须有非空 id 或角色键）
fn validate_identity(req: &PermissionEntry) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if req.id.trim().is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "permission id must not be empty",
        ));
    }
    Ok(())
}

/// `POST /api/permissions` → 新建一条权限（强制 Draft 状态，id 冲突返回 409）
async fn create_permission(
    State(shared): State<SharedFactsLog>,
    Json(mut entry): Json<PermissionEntry>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    validate_identity(&entry)?;
    entry.state = PermissionState::Draft;
    entry.version = 0;

    let table = snapshot(&shared)?;
    if table.get(&entry.id).is_some() {
        return Err(err(
            StatusCode::CONFLICT,
            format!("duplicate permission id: {}", entry.id),
        ));
    }

    match PermissionTable::store_entry(&shared, &entry, GLOBAL_SESSION) {
        Ok(version) => Ok(Json(serde_json::json!({
            "success": true,
            "id": entry.id,
            "state": "draft",
            "version": version,
        }))),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

/// `PUT /api/permissions/{id}` → 全量替换一条权限（幂等：不存在则创建）
async fn update_permission(
    State(shared): State<SharedFactsLog>,
    Path(id): Path<String>,
    Json(mut entry): Json<PermissionEntry>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if entry.id != id {
        return Err(err(StatusCode::BAD_REQUEST, "path id and body id mismatch"));
    }
    validate_identity(&entry)?;
    // 覆盖式写入时若已是 Active，保持 Active（提交者可把规则直接置活，简化运营）
    let table = snapshot(&shared)?;
    if let Some(existing) = table.get(&entry.id) {
        entry.state = existing.state;
    }
    entry.version = 0;

    match PermissionTable::store_entry(&shared, &entry, GLOBAL_SESSION) {
        Ok(version) => Ok(Json(
            serde_json::json!({ "success": true, "id": id, "version": version }),
        )),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

/// `DELETE /api/permissions/{id}` → 删除（写墓碑，历史保留）
async fn delete_permission(
    State(shared): State<SharedFactsLog>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    match PermissionTable::remove(&shared, &id, GLOBAL_SESSION) {
        Ok(()) => Ok(Json(serde_json::json!({ "success": true, "id": id }))),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

/// `POST /api/permissions/{id}/submit` → 提交审批（Draft → Candidate）
async fn submit_permission(
    State(shared): State<SharedFactsLog>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let table = snapshot(&shared)?;
    let mut entry = table.get(&id).cloned().ok_or_else(|| {
        err(
            StatusCode::NOT_FOUND,
            format!("permission entry not found: {id}"),
        )
    })?;
    entry
        .submit()
        .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    entry.version = 0;

    match PermissionTable::store_entry(&shared, &entry, GLOBAL_SESSION) {
        Ok(version) => Ok(Json(
            serde_json::json!({ "success": true, "id": id, "state": "candidate", "version": version }),
        )),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

/// 审批请求体
#[derive(Debug, Deserialize)]
pub struct ReviewRequest {
    /// `true` = 批准（→ Active），`false` = 拒绝（→ Rejected）
    pub approve: bool,
}

/// `POST /api/permissions/{id}/review` → 审批裁决（Candidate → Active/Rejected）
async fn review_permission(
    State(shared): State<SharedFactsLog>,
    Path(id): Path<String>,
    Json(req): Json<ReviewRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let table = snapshot(&shared)?;
    let mut entry = table.get(&id).cloned().ok_or_else(|| {
        err(
            StatusCode::NOT_FOUND,
            format!("permission entry not found: {id}"),
        )
    })?;
    entry
        .review(req.approve)
        .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    entry.version = 0;

    let state = if req.approve { "active" } else { "rejected" };
    match PermissionTable::store_entry(&shared, &entry, GLOBAL_SESSION) {
        Ok(version) => Ok(Json(
            serde_json::json!({ "success": true, "id": id, "state": state, "version": version }),
        )),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

/// `GET /api/permissions/version` → 权限快照版本与条目数量
async fn permissions_version(
    State(shared): State<SharedFactsLog>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let table = snapshot(&shared)?;
    Ok(Json(serde_json::json!({
        "success": true,
        "version": table.version(),
        "count": table.entries().len(),
    })))
}

/// 判定测试请求体
#[derive(Debug, Deserialize)]
pub struct EvaluateRequest {
    /// 待判定的资源串
    pub resource: String,
    /// 动作（缺省 `*`）
    pub action: Option<String>,
    /// 调用者角色（`human`/`llm`/`unknown`，缺省 unknown）
    pub caller_role: Option<String>,
    /// 冻结版本（缺省使用当前共享版本）
    pub v_trigger: Option<u64>,
    /// 触发事实 ID（缺省 0）
    pub cause: Option<u64>,
    /// 租户 ID
    pub tenant_id: Option<String>,
}

/// `POST /api/permissions/evaluate` → 按给定上下文跑一次权限判定（只读）
async fn evaluate_permission(
    State(shared): State<SharedFactsLog>,
    Json(req): Json<EvaluateRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let ctx = IoCallContext {
        cause: FactId(req.cause.unwrap_or(0)),
        v_trigger: req.v_trigger.unwrap_or(shared.version()),
        caller_role: CallerRole::from_str_opt(req.caller_role.as_deref().unwrap_or("unknown")),
        cause_chain: Vec::new(),
        tenant_id: req.tenant_id,
    };

    let table = PermissionTable::snapshot_at(&shared, ctx.v_trigger)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let action = req.action.unwrap_or_else(|| "*".to_string());
    let verdict = table.evaluate(&ctx, &req.resource, &action, None);
    let label = match verdict {
        Verdict::Allow => "allow",
        Verdict::Deny => "deny",
        Verdict::Candidate => "candidate",
    };
    Ok(Json(serde_json::json!({
        "success": true,
        "caller_role": ctx.caller_role.as_str(),
        "resource": req.resource,
        "action": action,
        "v_trigger": ctx.v_trigger,
        "verdict": label,
    })))
}

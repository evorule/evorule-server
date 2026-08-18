// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! OpenAPI 单一真相源 (P2-1)
//!
//! 所有端点的 OpenAPI 3.1 规范由 [utoipa] 从代码生成（`#[utoipa::path]` +
//! `ToSchema`），运行时经 `GET /api/openapi.json` 导出。前端类型由此派生
//! （openapi-typescript），杜绝手写契约文档与代码漂移。
//!
//! - server.rs 端点在此聚合（[ApiDoc]）
//! - workspace 端点由 `evorule_workspace::api::workspace_openapi()` 独立聚合，
//!   在 [merged_openapi] 中运行时合并
//! - Swagger UI（`/api/docs`）仅通过 `--openapi-ui` 显式启用，默认关闭

use axum::response::Json;
use utoipa::OpenApi;

/// evorule-server 端点聚合（server.rs 全部 handler）
///
/// workspace 端点不在此列出——由 [merged_openapi] 运行时合并
/// `evorule_workspace::api::workspace_openapi()`。
#[derive(OpenApi)]
#[openapi(
    paths(
        // health 组
        crate::api::server::health,
        crate::api::server::liveness,
        crate::api::server::readiness,
        // governance 组（单反应器模式）
        crate::api::server::submit_command,
        crate::api::server::update_payload,
        crate::api::server::get_state,
        crate::api::server::get_audit,
        // sessions 组（多会话模式）
        crate::api::server::create_session,
        crate::api::server::list_sessions,
        crate::api::server::close_session,
        crate::api::server::session_metadata,
        crate::api::server::session_reap,
        crate::api::server::create_session_from_parent,
        crate::api::server::create_session_fork,
        crate::api::server::session_command,
        crate::api::server::session_state,
        crate::api::server::session_audit,
        crate::api::server::session_audit_verify,
        crate::api::server::session_causal_chain,
        crate::api::server::session_audit_export,
        crate::api::server::session_audit_import,
        crate::api::server::session_audit_export_compressed,
        crate::api::server::session_audit_import_compressed,
        crate::api::server::session_payload,
        crate::api::server::session_events,
        crate::api::server::session_io_response,
        crate::api::server::session_replay,
        crate::api::server::session_history,
        crate::api::server::session_rewind,
        crate::api::server::session_diff,
        crate::api::server::session_snapshot,
        crate::api::server::session_interrupt,
        crate::api::server::session_finished,
        crate::api::server::session_causal_depth,
        crate::api::server::session_invariants,
        crate::api::server::session_pending_io_count,
        crate::api::server::session_step,
        crate::api::server::session_auto_verify_get,
        crate::api::server::session_auto_verify_post,
        crate::api::server::session_abort,
        crate::api::server::debug_phase,
        crate::api::server::debug_queue,
        crate::api::server::debug_pending_io,
        // shared-facts 组
        crate::api::server::session_facts_by_prefix,
        crate::api::server::shared_facts_by_prefix,
        crate::api::server::shared_fact_source,
        crate::api::server::record_used_at_startup,
        crate::api::server::get_used_at_startup,
        crate::api::server::get_sessions_using_fact,
        crate::api::server::shared_facts_rollup,
        crate::api::server::shared_facts_version,
        // rules 组
        crate::api::server::validate_rules_handler,
        crate::api::server::reload_rules_handler,
        crate::api::server::get_rules,
        // openapi 元数据
        crate::api::openapi::openapi_json,
    ),
    components(schemas(
        // 通用
        crate::api::server::ApiResponse,
        crate::api::server::CommandRequest,
        crate::api::server::PayloadUpdateRequest,
        // 会话基础响应
        crate::api::server::SessionIdResponse,
        crate::api::server::SessionListResponse,
        crate::api::server::SessionForkResponse,
        crate::api::server::SessionMetadataResponse,
        crate::api::server::ReapResponse,
        // 状态快照
        crate::api::server::StateResponse,
        crate::api::server::SessionStateResponse,
        crate::api::server::ReactorStatus,
        // 审计
        crate::api::server::AuditResponse,
        crate::api::server::AuditVerifyResponse,
        crate::api::server::CausalChainEntry,
        crate::api::server::CausalChainResponse,
        crate::api::server::AuditImportResponse,
        // 时间旅行
        crate::api::server::RewindResponse,
        crate::api::server::DiffResponse,
        // 事实
        crate::api::server::SessionFact,
        crate::api::server::SharedFact,
        crate::api::server::UsedAtStartupResponse,
        crate::api::server::SessionsUsingFactResponse,
        // debug / 运行状态
        crate::api::server::DebugPhaseResponse,
        crate::api::server::DebugQueueResponse,
        crate::api::server::DebugPendingIoResponse,
        crate::api::server::InterruptResponse,
        crate::api::server::FinishedResponse,
        crate::api::server::CausalDepthResponse,
        crate::api::server::InvariantsResponse,
        crate::api::server::PendingIoCountResponse,
        crate::api::server::StepResponse,
        crate::api::server::SnapshotResponse,
        crate::api::server::AutoVerifyResponse,
        crate::api::server::AutoVerifyConfigureResponse,
        // 请求体
        crate::api::server::IoResponseRequest,
        crate::api::server::AutoVerifyRequest,
        crate::api::server::FactIdsRequest,
        crate::api::server::ValidateRulesRequest,
        crate::api::server::RulesReloadedResponse,
        crate::api::server::RulesResponse,
        crate::api::server::SharedFactsVersionResponse,
        // 查询参数
        crate::api::server::CreateSessionFromParentParams,
        crate::api::server::CreateSessionForkParams,
        crate::api::server::ReplayParams,
        crate::api::server::FactsByPrefixParams,
        crate::api::server::RewindParams,
        crate::api::server::DiffParams,
    )),
    info(
        title = "EvoRule Server API",
        description = "EvoRule 确定性执行引擎官方 HTTP API — 单一真相源（utoipa 代码生成，运行时导出）",
        version = env!("CARGO_PKG_VERSION"),
        license(name = "AGPL-3.0-or-later")
    )
)]
pub struct ApiDoc;

/// 合并后的完整规范（server 端点 + workspace 端点）
pub fn merged_openapi() -> utoipa::openapi::OpenApi {
    let mut spec = ApiDoc::openapi();
    spec.merge(evorule_workspace::api::workspace_openapi());
    spec
}

/// 导出 OpenAPI 规范（单一真相源）
///
/// 返回 JSON 文档，供前端 codegen（openapi-typescript）与人工查阅。
#[utoipa::path(
    get,
    path = "/api/openapi.json",
    tag = "openapi",
    responses(
        (status = 200, description = "OpenAPI 3.1 规范 JSON", body = serde_json::Value),
        (status = 500, description = "规范序列化失败")
    )
)]
pub async fn openapi_json() -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
    let spec = merged_openapi();
    serde_json::to_value(&spec).map(Json).map_err(|e| {
        tracing::error!(error = %e, "OpenAPI spec 序列化失败");
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    })
}

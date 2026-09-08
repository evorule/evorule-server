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
//! - workspace 端点由 `evorule_workspace::api::workspace_openapi` 独立聚合，
//!   在 [merged_openapi] 中运行时合并
//! - Swagger UI（`/api/docs`）仅通过 `--openapi-ui` 显式启用，默认关闭

use axum::response::Json;
use utoipa::OpenApi;

/// evorule-server 端点聚合（server.rs 全部 handler）
///
/// workspace 端点不在此列出——由 [merged_openapi] 运行时合并
/// `evorule_workspace::api::workspace_openapi`。
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
        // rules 命中统计组
        crate::api::server::hit_stats_handler,
        crate::api::server::hit_stats_rule_handler,
        // bundles 组（快照包导入 / T4 激活报告）
        crate::api::bundles::import_bundle_handler,
        crate::api::bundles::import_bundle_dry_run_handler,
        crate::api::bundles::active_bundles_handler,
        crate::api::bundles::list_bundle_imports_handler,
        // knowledge 组（段2 P1 执行侧数据面）
        crate::api::knowledge::knowledge_datasets_handler,
        crate::api::knowledge::knowledge_entries_handler,
        crate::api::knowledge::knowledge_entry_handler,
        // services / metrics 组（C5 能力对账 / Prometheus 抓取，补注册）
        crate::api::server::list_services_handler,
        crate::api::server::invoke_service_handler,
        crate::api::server::metrics_handler,
        // audit 档案与平台事件组
        crate::api::server::platform_events_handler,
        crate::api::server::archive_sessions,
        crate::api::server::archive_session_audit,
        // platform-auth 组
        crate::api::platform_auth::bootstrap,
        crate::api::platform_auth::login,
        crate::api::platform_auth::logout,
        crate::api::platform_auth::me,
        crate::api::platform_auth::auth_status,
        crate::api::platform_auth::change_password,
        crate::api::platform_auth::list_permissions,
        crate::api::platform_auth::list_users,
        crate::api::platform_auth::create_user,
        crate::api::platform_auth::update_user,
        crate::api::platform_auth::delete_user,
        crate::api::platform_auth::list_roles,
        crate::api::platform_auth::create_role,
        crate::api::platform_auth::update_role,
        crate::api::platform_auth::delete_role,
        // permissions 组（A-流 权限系统，9 端点，补注册）
        crate::api::permissions::list_permissions,
        crate::api::permissions::create_permission,
        crate::api::permissions::get_permission,
        crate::api::permissions::update_permission,
        crate::api::permissions::delete_permission,
        crate::api::permissions::submit_permission,
        crate::api::permissions::review_permission,
        crate::api::permissions::permissions_version,
        crate::api::permissions::evaluate_permission,
        // 平台应用凭据组（58 W2 签发/列表/吊销 + 59 W1 配额更新——
        // 58 W3 曾声称已入册,复核发现 paths() 实际遗漏,本批补登）
        crate::api::platform_auth::issue_app,
        crate::api::platform_auth::list_apps,
        crate::api::platform_auth::revoke_app,
        crate::api::platform_auth::update_app_quota,
        // 插件审批代理组（57 W2 三路由,同上补登）
        crate::api::server::plugin_admin_list_proposals,
        crate::api::server::plugin_admin_approve,
        crate::api::server::plugin_admin_reject,
        // 工作空间自助加入（handler 在 server.rs——evorule_workspace 路由族中
        // 唯一由本仓定义的端点,故在 paths() 登记;其余 workspace 端点走
        // merged_openapi 运行时合并）
        crate::api::server::workspace_join,
        // marketplace 组
        crate::api::marketplace::list_templates,
        crate::api::marketplace::upload_template,
        crate::api::marketplace::update_template_handler,
        crate::api::marketplace::download_template,
        crate::api::marketplace::delete_template_handler,
        // export 组
        crate::api::pdf_export::pdf_export_handler,
        // openapi 元数据
        crate::api::openapi::openapi_json,
    ),
    components(schemas(
        // 通用
        crate::api::server::ApiResponse,
        crate::api::server::HealthResponse,
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
    crate::api::server::RuleTierEntry,
        crate::api::server::SharedFactsVersionResponse,
        // bundles 组
        crate::api::bundles::ImportResponse,
        crate::api::bundles::ActiveBundlesResponse,
        crate::api::bundles::ActiveBundleInfo,
        // knowledge 组（段2 P1 执行侧数据面）
        crate::api::knowledge::KnowledgeDatasetsResponse,
        crate::api::knowledge::KnowledgeEntriesResponse,
        crate::knowledge_store::KnowledgeDatasetSummary,
        crate::knowledge_store::KnowledgeEntryRecord,
        // services 组（C5 能力对账）
        crate::api::server::BoundServiceInfo,
        // platform-auth 组请求体
        crate::api::platform_auth::CredentialsReq,
        crate::api::platform_auth::BootstrapReq,
        crate::api::platform_auth::ChangePasswordReq,
        crate::api::platform_auth::CreateUserReq,
        crate::api::platform_auth::UpdateUserReq,
        crate::api::platform_auth::CreateRoleReq,
        crate::api::platform_auth::UpdateRoleReq,
        // permissions 组请求体（A-流）
        crate::api::permissions::ReviewRequest,
        crate::api::permissions::EvaluateRequest,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 契约测试：openapi.json 必须覆盖全部已注册路由（防再漂移）。
    ///
    /// 端点清单与路由注册面同步维护：
    /// - server.rs `build_router` 的全部 `.route(...)` 调用
    ///   （public_routes / protected_routes / metrics_router / 条件挂载的 abort——
    ///   abort 路由默认不注册但文档恒注册，故同样纳入清单）
    /// - `platform_auth_router`（platform_auth.rs，挂入 public_routes）
    /// - `permissions_router`（permissions.rs，挂入 protected_routes）
    /// - workspace 端点族抽查（完整清单以 evorule-workspace 的
    ///   `workspace_openapi` 为准，此处按族抽代表路径）
    ///
    /// 新增 `.route` 的同步纪律：补 `#[utoipa::path]` 注解 → 在 [ApiDoc]
    /// `paths(...)` 注册 → 在本清单追加路径；缺一步本测试变红。
    #[test]
    fn test_openapi_covers_all_registered_paths() {
        let spec = merged_openapi();
        let paths = &spec.paths.paths;

        // ===== server.rs public_routes（免认证） =====
        let registered: &[&str] = &[
            "/api/health",
            "/api/health/liveness",
            "/api/health/readiness",
            "/api/rules/validate",
            "/api/openapi.json",
            "/api/services",
            // ===== platform_auth_router =====
            "/api/platform/auth/bootstrap",
            "/api/platform/auth/login",
            "/api/platform/auth/logout",
            "/api/platform/auth/me",
            "/api/platform/auth/status",
            "/api/platform/auth/change-password",
            "/api/platform/permissions",
            "/api/platform/users",
            "/api/platform/users/{username}",
            "/api/platform/roles",
            "/api/platform/roles/{name}",
            // ===== abort（--allow-abort 条件挂载，文档恒注册） =====
            "/api/sessions/{id}/abort",
            // ===== server.rs protected_routes（受认证保护） =====
            "/api/command",
            "/api/payload",
            "/api/state",
            "/api/audit",
            "/api/audit/platform-events",
            "/api/sessions",
            "/api/audit-archive/sessions",
            "/api/audit-archive/sessions/{id}/audit",
            "/api/sessions/from/{parent_id}",
            "/api/sessions/fork/{parent_id}",
            "/api/sessions/{id}",
            "/api/sessions/reap",
            "/api/sessions/{id}/command",
            "/api/sessions/{id}/state",
            "/api/sessions/{id}/audit",
            "/api/sessions/{id}/audit/verify",
            "/api/sessions/{id}/audit/export",
            "/api/sessions/{id}/audit/import",
            "/api/sessions/{id}/audit/export/compressed",
            "/api/sessions/{id}/audit/import/compressed",
            "/api/sessions/{id}/audit/causal/{fact_id}",
            "/api/sessions/{id}/payload",
            "/api/sessions/{id}/events",
            "/api/sessions/{id}/io_response",
            "/api/sessions/{id}/replay",
            "/api/sessions/{id}/history",
            "/api/sessions/{id}/rewind",
            "/api/sessions/{id}/diff",
            "/api/sessions/{id}/facts",
            "/api/shared/facts",
            "/api/shared/facts/{fact_id}/source",
            "/api/shared/facts/{fact_id}/used_by",
            "/api/shared/facts/version",
            "/api/shared/facts/rollup",
            "/api/sessions/{id}/used_at_startup",
            "/api/sessions/{id}/debug/phase",
            "/api/sessions/{id}/debug/queue",
            "/api/sessions/{id}/debug/pending_io",
            "/api/sessions/{id}/interrupt",
            "/api/sessions/{id}/finished",
            "/api/sessions/{id}/causal_depth",
            "/api/sessions/{id}/invariants",
            "/api/sessions/{id}/pending_io_count",
            "/api/sessions/{id}/step",
            "/api/sessions/{id}/snapshot",
            "/api/sessions/{id}/audit/auto_verify",
            "/api/rules/reload",
            "/api/rules",
            "/api/rules/hit-stats",
            "/api/rules/hit-stats/{rule_key}",
            "/api/bundles/import",
            "/api/bundles/import/dry-run",
            "/api/bundles/active",
            "/api/bundles/imports",
            "/api/knowledge",
            "/api/knowledge/{ds}/entries",
            "/api/knowledge/{ds}/entries/{entry_id}",
            // ===== permissions_router（A-流，挂入 protected_routes） =====
            "/api/permissions",
            "/api/permissions/{id}",
            "/api/permissions/version",
            "/api/permissions/evaluate",
            "/api/permissions/{id}/submit",
            "/api/permissions/{id}/review",
            // ===== metrics_router =====
            "/metrics",
            // ===== workspace 端点族抽查（evorule-workspace build_workspace_router） =====
            "/api/workspaces",
            "/api/workspaces/{id}",
            "/api/workspaces/{id}/members",
            "/api/workspaces/{id}/members/{user_id}",
            "/api/workspaces/{id}/members/join",
            "/api/workspaces/{id}/rules",
            "/api/workspaces/{id}/rules/{rule_id}",
            "/api/workspaces/{id}/rules/{rule_id}/versions",
            "/api/workspaces/{id}/sessions",
            "/api/workspaces/{id}/sandboxes",
            "/api/workspaces/{id}/test-datasets",
            "/api/publish/queue",
            "/api/publish/rollback",
            "/api/production/state",
            "/api/rules/translate/to_transform",
            "/api/workspaces/{id}/verdict_contracts",
            "/api/workspaces/{id}/verdict/evaluate",
            "/api/sessions/{id}/clock/lookup",
        ];

        for path in registered {
            assert!(
                paths.contains_key(*path),
                "openapi.json 漏报端点 {path}：补 #[utoipa::path] 注解并在 ApiDoc paths(...) 注册"
            );
        }

        // method 级抽查（utoipa 同路径多注解自动合并为单 PathItem）
        assert!(
            paths["/api/sessions"].get.is_some() && paths["/api/sessions"].post.is_some(),
            "/api/sessions 应同时有 GET/POST"
        );
        assert!(
            paths["/api/permissions/{id}"].put.is_some(),
            "/api/permissions/{{id}} 应为 PUT（与 permissions_router 注册一致）"
        );
        assert!(
            paths["/api/platform/users/{username}"].patch.is_some()
                && paths["/api/platform/users/{username}"].delete.is_some(),
            "/api/platform/users/{{username}} 应同时有 PATCH/DELETE"
        );
        assert!(
            paths["/api/platform/roles"].get.is_some()
                && paths["/api/platform/roles"].post.is_some(),
            "/api/platform/roles 应同时有 GET/POST"
        );
        assert!(
            paths["/api/services"].get.is_some(),
            "/api/services 应有 GET"
        );
        assert!(paths["/metrics"].get.is_some(), "/metrics 应有 GET");
    }
}

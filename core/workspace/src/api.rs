// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! HTTP API 层 — 32 个 handler + Router 构建
//!
//! 设计依据:
//! - WORKSPACE_CRATE_DESIGN.md §6 (Workspace + 规则 + 会话, 18 handler)
//! - SANDBOX_ORCHESTRATION_DESIGN.md §6 (沙盒编排 + 测试数据集, 7 handler)
//! - PUBLISH_QUEUE_DESIGN.md §6 (发布队列 + 生产状态/审计, 7 handler)
//!
//! # 路由总览
//! | Method | Path | Handler | 说明 |
//! |--------|------|---------|------|
//! | POST | /api/workspaces | create_workspace | 创建工作空间 |
//! | GET | /api/workspaces | list_workspaces | 列出工作空间 |
//! | GET | /api/workspaces/{id} | get_workspace | 获取工作空间 |
//! | PATCH | /api/workspaces/{id} | update_workspace | 更新工作空间 |
//! | DELETE | /api/workspaces/{id} | archive_workspace | 归档工作空间 |
//! | POST | /api/workspaces/{id}/members | add_member | 添加成员 |
//! | DELETE | /api/workspaces/{id}/members/{user_id} | remove_member | 移除成员 |
//! | GET | /api/workspaces/{id}/members | list_members | 列出成员 |
//! | POST | /api/workspaces/{id}/rules | create_rule | 创建规则 |
//! | GET | /api/workspaces/{id}/rules | list_rules | 列出规则 |
//! | GET | /api/workspaces/{id}/rules/{rule_id} | get_rule | 获取规则 |
//! | PATCH | /api/workspaces/{id}/rules/{rule_id} | update_rule_content | 更新规则内容 |
//! | POST | /api/workspaces/{id}/rules/{rule_id}/activate | activate_rule | 激活规则 |
//! | POST | /api/workspaces/{id}/rules/{rule_id}/submit | submit_rule | 提交候选 (Draft→Candidate) |
//! | POST | /api/workspaces/{id}/rules/{rule_id}/block | block_rule | 阻塞规则 |
//! | POST | /api/workspaces/{id}/rules/{rule_id}/archive | archive_rule | 归档规则 |
//! | POST | /api/workspaces/{id}/rules/{rule_id}/fork | fork_rule | fork 规则 |
//! | GET | /api/workspaces/{id}/rules/{rule_id}/versions | list_rule_versions_handler | 列出规则版本(含 content) |
//! | GET | /api/workspaces/{id}/rules/{rule_id}/versions/{version_id} | get_rule_version_handler | 获取规则指定版本 |
//! | POST | /api/workspaces/{id}/sessions | create_session | 创建会话 |
//! | GET | /api/workspaces/{id}/sessions | list_sessions | 列出会话 |
//! | POST | /api/workspaces/{id}/sandboxes | start_sandbox | 启动沙盒测试 |
//! | GET | /api/workspaces/{id}/sandboxes | list_sandboxes | 列出沙盒历史 |
//! | GET | /api/workspaces/{id}/sandboxes/{sandbox_id} | get_sandbox | 沙盒详情 |
//! | POST | /api/workspaces/{id}/sandboxes/{sandbox_id}/close | close_sandbox | 关闭沙盒 |
//! | GET | /api/workspaces/{id}/sandboxes/{sandbox_id}/report | get_sandbox_report | 测试报告 |
//! | POST | /api/workspaces/{id}/test-datasets | create_test_dataset | 创建数据集 |
//! | GET | /api/workspaces/{id}/test-datasets | list_test_datasets | 列出数据集 |
//! | POST | /api/publish/queue | submit_publish | 提交发布 (DepartmentHead) |
//! | GET | /api/publish/queue | list_publish_queue | 列出发布队列 |
//! | GET | /api/publish/queue/{queue_id} | get_publish_queue_item | 队列项详情 |
//! | POST | /api/publish/queue/{queue_id}/review | review_publish | 审批发布 (Admin) |
//! | POST | /api/publish/rollback | emergency_rollback | 紧急回滚 (Admin) |
//! | GET | /api/production/state | get_production_state | 生产状态 |
//! | GET | /api/production/audit | list_production_audit | 发布审计历史 |

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{delete, get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::Value;
use utoipa::OpenApi;

use crate::error::WorkspaceError;
use crate::models::{
    AddMemberRequest, CreateRuleRequest, CreateSessionRequest, CreateTestDatasetRequest,
    CreateWorkspaceRequest, ListPublishQueueQuery, ProductionAuditRecord, ProductionStateRecord,
    PublishQueueItem, PublishRole, PublishStatus, ReviewPublishRequest, RollbackRequest,
    RuleRecord, RuleVersionRecord, SandboxSession, SessionRecord, StartSandboxRequest,
    StartSandboxResponse, SubmitPublishRequest, TestDatasetRecord, UpdateRuleContentRequest,
    UpdateWorkspaceRequest, VerdictContractRecord, VersionClockMapRecord, WorkspaceMemberRecord,
    WorkspaceRecord,
};
use crate::publish_service::PublishService;
use crate::rule_meta_service::RuleMetaService;
use crate::rule_translate::{
    self, TranslateToConditionalRequest, TranslateToConditionalResponse,
    TranslateToTransformRequest, TranslateToTransformResponse,
};
use crate::sandbox_service::SandboxService;
use crate::session_switched::SessionSwitchedBroadcaster;
use crate::test_report::TestReport;
use crate::verdict_service::{
    CreateVerdictContractRequest, EvaluateVerdictRequest, EvaluateVerdictResult, LookupClockQuery,
    RecordClockRequest, UpdateVerdictContractRequest, VerdictService,
};
use crate::workspace_service::WorkspaceService;

/// Fork 规则请求体
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct ForkRuleRequest {
    pub new_name: String,
    pub created_by: String,
}

/// 列出工作空间的查询参数
#[derive(Debug, Deserialize, Default, utoipa::ToSchema)]
pub struct ListWorkspacesQuery {
    /// 按 owner 过滤 (可选)
    pub owner_id: Option<String>,
}

// =============================================================================
// 沙盒 + 发布队列 HTTP 包装请求 DTO
// =============================================================================
//
// P0 简化: 操作者身份 (user_id) 和发布角色 (PublishRole) 从请求体传入,
// 与现有 handler 一致 (如 CreateRuleRequest.created_by)。
// P1 接入 evorule-server 的 auth middleware 后, 改为从 Extension<AuthUser> 提取。

/// 启动沙盒测试的 HTTP 请求 (包装 StartSandboxRequest + 操作者)
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct StartSandboxHttpRequest {
    /// 沙盒负载 (规则版本 + 数据集)
    #[serde(flatten)]
    pub payload: StartSandboxRequest,
    /// 启动者用户 ID
    pub started_by: String,
}

/// 关闭沙盒的请求体
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CloseSandboxRequest {
    /// 关闭者用户 ID
    pub closed_by: String,
}

/// 沙盒列表/详情查询参数 (传递请求者身份以做成员校验)
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct SandboxRequesterQuery {
    /// 请求者用户 ID (用于 workspace 成员权限校验)
    pub requester: String,
}

/// 提交发布的 HTTP 请求 (包装 SubmitPublishRequest + 操作者 + 角色)
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct SubmitPublishHttpRequest {
    /// 发布负载
    #[serde(flatten)]
    pub payload: SubmitPublishRequest,
    /// 提交者用户 ID (科室主任)
    pub submitted_by: String,
    /// 发布角色 (doctor / department_head / admin)
    pub role: PublishRole,
}

/// 审批发布的 HTTP 请求 (包装 ReviewPublishRequest + 操作者 + 角色)
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct ReviewPublishHttpRequest {
    /// 审批负载
    #[serde(flatten)]
    pub payload: ReviewPublishRequest,
    /// 审批者用户 ID (信息科/院领导)
    pub reviewed_by: String,
    /// 发布角色 (必须为 admin)
    pub role: PublishRole,
}

/// 紧急回滚的 HTTP 请求 (包装 RollbackRequest + 操作者 + 角色)
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct RollbackHttpRequest {
    /// 回滚负载
    #[serde(flatten)]
    pub payload: RollbackRequest,
    /// 操作者用户 ID (信息科/院领导)
    pub operated_by: String,
    /// 发布角色 (必须为 admin)
    pub role: PublishRole,
}

/// 生产审计列表查询参数
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct ListProductionAuditQuery {
    /// 返回记录上限 (默认 50)
    #[serde(default = "default_audit_limit")]
    pub limit: i64,
}

/// 生产审计列表默认上限
fn default_audit_limit() -> i64 {
    50
}

/// 工作空间 API 共享状态
///
/// 通过 `Arc` 共享各业务服务,
/// 实现 `Clone` (Arc clone 廉价)。
/// 在 evorule-server 中通过 `FromRef<AppState>` 提取。
#[derive(Clone)]
pub struct WorkspaceState {
    /// 工作空间服务
    pub workspace_service: Arc<WorkspaceService>,
    /// 规则元数据服务
    pub rule_meta_service: Arc<RuleMetaService>,
    /// 沙盒编排服务 (SANDBOX_ORCHESTRATION_DESIGN.md §3)
    pub sandbox_service: Arc<SandboxService>,
    /// 发布队列服务 (PUBLISH_QUEUE_DESIGN.md §3)
    pub publish_service: Arc<PublishService>,
    /// session_switched SSE 广播器 (PUBLISH_QUEUE_DESIGN.md §5 U7)
    pub switcher: Arc<SessionSwitchedBroadcaster>,
    /// 判定契约 + wall-clock 旁路服务 (界面升级 v1.0 阶段 A.3/A.4)
    pub verdict_service: Arc<VerdictService>,
}

impl WorkspaceState {
    /// 创建新状态
    pub fn new(
        workspace_service: Arc<WorkspaceService>,
        rule_meta_service: Arc<RuleMetaService>,
        sandbox_service: Arc<SandboxService>,
        publish_service: Arc<PublishService>,
        switcher: Arc<SessionSwitchedBroadcaster>,
        verdict_service: Arc<VerdictService>,
    ) -> Self {
        Self {
            workspace_service,
            rule_meta_service,
            sandbox_service,
            publish_service,
            switcher,
            verdict_service,
        }
    }
}

/// workspace 端点的 OpenAPI 聚合（P2-1 单一真相源）
///
/// 全部 45 个 workspace handler 由 utoipa 从代码生成规范，
/// evorule-server 在运行时通过 [workspace_openapi] merge 到整体 spec
/// （`GET /api/openapi.json`）。
#[derive(utoipa::OpenApi)]
#[openapi(
    paths(
        // workspace 管理
        crate::api::create_workspace,
        crate::api::list_workspaces,
        crate::api::get_workspace,
        crate::api::update_workspace,
        crate::api::archive_workspace,
        // 成员管理
        crate::api::add_member,
        crate::api::remove_member,
        crate::api::list_members,
        // 规则管理
        crate::api::create_rule,
        crate::api::list_rules,
        crate::api::get_rule,
        crate::api::update_rule_content,
        crate::api::activate_rule,
        crate::api::submit_rule,
        crate::api::block_rule,
        crate::api::archive_rule,
        crate::api::fork_rule,
        crate::api::list_rule_versions_handler,
        crate::api::get_rule_version_handler,
        // 会话管理
        crate::api::create_session,
        crate::api::list_sessions,
        // 沙盒编排
        crate::api::start_sandbox,
        crate::api::list_sandboxes,
        crate::api::get_sandbox,
        crate::api::close_sandbox,
        crate::api::get_sandbox_report,
        // 测试数据集
        crate::api::create_test_dataset,
        crate::api::list_test_datasets,
        // 发布队列
        crate::api::submit_publish,
        crate::api::list_publish_queue,
        crate::api::get_publish_queue_item,
        crate::api::review_publish,
        crate::api::emergency_rollback,
        // 生产状态 + 审计
        crate::api::get_production_state,
        crate::api::list_production_audit,
        // 规则转译
        crate::api::translate_to_transform_handler,
        crate::api::translate_to_conditional_handler,
        // 判定契约
        crate::api::create_verdict_contract,
        crate::api::list_verdict_contracts,
        crate::api::get_verdict_contract,
        crate::api::update_verdict_contract,
        crate::api::delete_verdict_contract,
        crate::api::evaluate_verdict,
        // wall-clock 旁路
        crate::api::record_clock,
        crate::api::lookup_clock,
    ),
    components(schemas(
        // 记录模型
        crate::models::WorkspaceRecord,
        crate::models::WorkspaceMemberRecord,
        crate::models::RuleRecord,
        crate::models::RuleVersionRecord,
        crate::models::SessionRecord,
        crate::models::SandboxSession,
        crate::models::TestDatasetRecord,
        crate::models::PublishQueueItem,
        crate::models::ProductionStateRecord,
        crate::models::ProductionAuditRecord,
        crate::models::VerdictContractRecord,
        crate::models::VersionClockMapRecord,
        // 状态机枚举
        crate::models::WorkspaceState,
        crate::models::RuleState,
        crate::models::SessionBindingState,
        crate::models::RuleVersionState,
        crate::models::SandboxStatus,
        crate::models::PublishRole,
        crate::models::PublishStatus,
        // 请求 DTO
        crate::models::CreateWorkspaceRequest,
        crate::models::UpdateWorkspaceRequest,
        crate::models::AddMemberRequest,
        crate::models::CreateRuleRequest,
        crate::models::UpdateRuleContentRequest,
        crate::models::CreateSessionRequest,
        crate::models::StartSandboxRequest,
        crate::models::StartSandboxResponse,
        crate::models::CreateTestDatasetRequest,
        crate::models::SubmitPublishRequest,
        crate::models::ReviewPublishRequest,
        crate::models::RollbackRequest,
        crate::models::ListPublishQueueQuery,
        // api.rs 局部 DTO
        crate::api::ForkRuleRequest,
        crate::api::ListWorkspacesQuery,
        crate::api::StartSandboxHttpRequest,
        crate::api::CloseSandboxRequest,
        crate::api::SandboxRequesterQuery,
        crate::api::SubmitPublishHttpRequest,
        crate::api::ReviewPublishHttpRequest,
        crate::api::RollbackHttpRequest,
        crate::api::ListProductionAuditQuery,
        // 规则转译 DTO
        crate::rule_translate::TranslateToTransformRequest,
        crate::rule_translate::TranslateToTransformResponse,
        crate::rule_translate::TranslateToConditionalRequest,
        crate::rule_translate::TranslateToConditionalResponse,
        // 判定契约 + clock DTO
        crate::verdict_service::CreateVerdictContractRequest,
        crate::verdict_service::UpdateVerdictContractRequest,
        crate::verdict_service::EvaluateVerdictRequest,
        crate::verdict_service::EvaluateVerdictResult,
        crate::verdict_service::RecordClockRequest,
        crate::verdict_service::LookupClockQuery,
        // 测试报告 schema
        crate::test_report::TestReport,
        crate::test_report::TestSummary,
        crate::test_report::TestCaseResult,
        crate::test_report::CaseStatus,
        crate::test_report::TestAnomaly,
        crate::test_report::AuditInfo,
    )),
    info(
        title = "EvoRule Workspace API",
        description = "EvoRule workspace/规则/会话/沙盒/发布队列/判定契约 HTTP API — workspace 侧单一真相源（utoipa 代码生成，运行时由 evorule-server 合并）",
        version = env!("CARGO_PKG_VERSION"),
        license(name = "AGPL-3.0-or-later")
    )
)]
pub struct WorkspaceApiDoc;

/// 导出 workspace 端点 OpenAPI 规范，供 evorule-server 运行时合并
pub fn workspace_openapi() -> utoipa::openapi::OpenApi {
    WorkspaceApiDoc::openapi()
}

/// 构建工作空间路由
///
/// 泛型约束: `S` 必须能提取出 `WorkspaceState` (通过 `FromRef`)。
/// 在 evorule-server 中, `S = AppState`, `AppState` 实现 `FromRef<AppState> for WorkspaceState`。
pub fn build_workspace_router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    WorkspaceState: axum::extract::FromRef<S>,
{
    Router::new()
        // ===== Workspace 管理 =====
        .route(
            "/api/workspaces",
            post(create_workspace).get(list_workspaces),
        )
        .route(
            "/api/workspaces/{id}",
            get(get_workspace)
                .patch(update_workspace)
                .delete(archive_workspace),
        )
        // ===== 成员管理 =====
        .route(
            "/api/workspaces/{id}/members",
            post(add_member).get(list_members),
        )
        .route(
            "/api/workspaces/{id}/members/{user_id}",
            delete(remove_member),
        )
        // ===== 规则管理 =====
        .route(
            "/api/workspaces/{id}/rules",
            post(create_rule).get(list_rules),
        )
        .route(
            "/api/workspaces/{id}/rules/{rule_id}",
            get(get_rule).patch(update_rule_content),
        )
        .route(
            "/api/workspaces/{id}/rules/{rule_id}/activate",
            post(activate_rule),
        )
        .route(
            "/api/workspaces/{id}/rules/{rule_id}/submit",
            post(submit_rule),
        )
        .route(
            "/api/workspaces/{id}/rules/{rule_id}/block",
            post(block_rule),
        )
        .route(
            "/api/workspaces/{id}/rules/{rule_id}/archive",
            post(archive_rule),
        )
        .route("/api/workspaces/{id}/rules/{rule_id}/fork", post(fork_rule))
        // ===== 规则版本查询 (阶段 D 新增, 暴露 list_rule_versions / get_rule_version) =====
        .route(
            "/api/workspaces/{id}/rules/{rule_id}/versions",
            get(list_rule_versions_handler),
        )
        .route(
            "/api/workspaces/{id}/rules/{rule_id}/versions/{version_id}",
            get(get_rule_version_handler),
        )
        // ===== 会话管理 =====
        .route(
            "/api/workspaces/{id}/sessions",
            post(create_session).get(list_sessions),
        )
        // ===== 沙盒编排 (SANDBOX_ORCHESTRATION_DESIGN.md §6) =====
        .route(
            "/api/workspaces/{id}/sandboxes",
            post(start_sandbox).get(list_sandboxes),
        )
        .route(
            "/api/workspaces/{id}/sandboxes/{sandbox_id}",
            get(get_sandbox),
        )
        .route(
            "/api/workspaces/{id}/sandboxes/{sandbox_id}/close",
            post(close_sandbox),
        )
        .route(
            "/api/workspaces/{id}/sandboxes/{sandbox_id}/report",
            get(get_sandbox_report),
        )
        // ===== 测试数据集 (SANDBOX_ORCHESTRATION_DESIGN.md §3) =====
        .route(
            "/api/workspaces/{id}/test-datasets",
            post(create_test_dataset).get(list_test_datasets),
        )
        // ===== 发布队列 (PUBLISH_QUEUE_DESIGN.md §6) =====
        .route(
            "/api/publish/queue",
            post(submit_publish).get(list_publish_queue),
        )
        .route("/api/publish/queue/{queue_id}", get(get_publish_queue_item))
        .route("/api/publish/queue/{queue_id}/review", post(review_publish))
        .route("/api/publish/rollback", post(emergency_rollback))
        // ===== 生产状态 + 审计 (PUBLISH_QUEUE_DESIGN.md §6) =====
        .route("/api/production/state", get(get_production_state))
        .route("/api/production/audit", get(list_production_audit))
        // ===== 规则转译 (界面升级 v1.0 阶段 A.2, 纯函数) =====
        .route(
            "/api/rules/translate/to_transform",
            post(translate_to_transform_handler),
        )
        .route(
            "/api/rules/translate/to_conditional",
            post(translate_to_conditional_handler),
        )
        // ===== 判定契约 (界面升级 v1.0 阶段 A.3) =====
        .route(
            "/api/workspaces/{id}/verdict_contracts",
            post(create_verdict_contract).get(list_verdict_contracts),
        )
        .route(
            "/api/workspaces/{id}/verdict_contracts/{cid}",
            get(get_verdict_contract)
                .patch(update_verdict_contract)
                .delete(delete_verdict_contract),
        )
        .route(
            "/api/workspaces/{id}/verdict/evaluate",
            post(evaluate_verdict),
        )
        // ===== wall-clock 旁路 (界面升级 v1.0 阶段 A.4) =====
        // 旁路数据绝不进审计链哈希 (00 §六/§七)
        .route("/api/sessions/{id}/clock/record", post(record_clock))
        .route("/api/sessions/{id}/clock/lookup", get(lookup_clock))
}

// =============================================================================
// Workspace 管理 handler
// =============================================================================

#[utoipa::path(
    post,
    path = "/api/workspaces",
    tag = "workspace",
    request_body = CreateWorkspaceRequest,
    responses(
        (status = 201, description = "created", body = WorkspaceRecord),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// POST /api/workspaces — 创建工作空间
async fn create_workspace(
    State(state): State<WorkspaceState>,
    Json(req): Json<CreateWorkspaceRequest>,
) -> Result<(StatusCode, Json<WorkspaceRecord>), WorkspaceError> {
    let ws = state.workspace_service.create_workspace(req).await?;
    Ok((StatusCode::CREATED, Json(ws)))
}

#[utoipa::path(
    get,
    path = "/api/workspaces",
    tag = "workspace",
    params(
        ("owner_id" = Option<String>, Query, description = "按 owner 过滤 (可选)"),
    ),
    responses(
        (status = 200, description = "success", body = Vec<WorkspaceRecord>),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// GET /api/workspaces — 列出工作空间
async fn list_workspaces(
    State(state): State<WorkspaceState>,
    Query(params): Query<ListWorkspacesQuery>,
) -> Result<Json<Vec<WorkspaceRecord>>, WorkspaceError> {
    let list = state
        .workspace_service
        .list_workspaces(params.owner_id.as_deref())
        .await?;
    Ok(Json(list))
}

#[utoipa::path(
    get,
    path = "/api/workspaces/{id}",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
    ),
    responses(
        (status = 200, description = "success", body = WorkspaceRecord),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// GET /api/workspaces/{id} — 获取工作空间
async fn get_workspace(
    State(state): State<WorkspaceState>,
    Path(id): Path<String>,
) -> Result<Json<WorkspaceRecord>, WorkspaceError> {
    let ws = state.workspace_service.get_workspace(&id).await?;
    Ok(Json(ws))
}

#[utoipa::path(
    patch,
    path = "/api/workspaces/{id}",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
    ),
    request_body = UpdateWorkspaceRequest,
    responses(
        (status = 200, description = "success", body = WorkspaceRecord),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// PATCH /api/workspaces/{id} — 更新工作空间
async fn update_workspace(
    State(state): State<WorkspaceState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateWorkspaceRequest>,
) -> Result<Json<WorkspaceRecord>, WorkspaceError> {
    let ws = state.workspace_service.update_workspace(&id, req).await?;
    Ok(Json(ws))
}

#[utoipa::path(
    delete,
    path = "/api/workspaces/{id}",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
    ),
    responses(
        (status = 200, description = "success", body = WorkspaceRecord),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// DELETE /api/workspaces/{id} — 归档工作空间 (软删除)
async fn archive_workspace(
    State(state): State<WorkspaceState>,
    Path(id): Path<String>,
) -> Result<Json<WorkspaceRecord>, WorkspaceError> {
    let ws = state.workspace_service.archive_workspace(&id).await?;
    Ok(Json(ws))
}

// =============================================================================
// 成员管理 handler
// =============================================================================

#[utoipa::path(
    post,
    path = "/api/workspaces/{id}/members",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
    ),
    request_body = AddMemberRequest,
    responses(
        (status = 201, description = "created", body = WorkspaceMemberRecord),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// POST /api/workspaces/{id}/members — 添加成员
async fn add_member(
    State(state): State<WorkspaceState>,
    Path(workspace_id): Path<String>,
    Json(req): Json<AddMemberRequest>,
) -> Result<(StatusCode, Json<WorkspaceMemberRecord>), WorkspaceError> {
    let m = state
        .workspace_service
        .add_member(&workspace_id, &req.user_id, &req.role)
        .await?;
    Ok((StatusCode::CREATED, Json(m)))
}

#[utoipa::path(
    delete,
    path = "/api/workspaces/{id}/members/{user_id}",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
        ("user_id" = String, Path, description = "用户 ID"),
    ),
    responses(
        (status = 204, description = "deleted"),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// DELETE /api/workspaces/{id}/members/{user_id} — 移除成员
async fn remove_member(
    State(state): State<WorkspaceState>,
    Path((workspace_id, user_id)): Path<(String, String)>,
) -> Result<StatusCode, WorkspaceError> {
    state
        .workspace_service
        .remove_member(&workspace_id, &user_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get,
    path = "/api/workspaces/{id}/members",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
    ),
    responses(
        (status = 200, description = "success", body = Vec<WorkspaceMemberRecord>),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// GET /api/workspaces/{id}/members — 列出成员
async fn list_members(
    State(state): State<WorkspaceState>,
    Path(workspace_id): Path<String>,
) -> Result<Json<Vec<WorkspaceMemberRecord>>, WorkspaceError> {
    let members = state.workspace_service.list_members(&workspace_id).await?;
    Ok(Json(members))
}

// =============================================================================
// 规则管理 handler
// =============================================================================

#[utoipa::path(
    post,
    path = "/api/workspaces/{id}/rules",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
    ),
    request_body = CreateRuleRequest,
    responses(
        (status = 201, description = "created", body = RuleRecord),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// POST /api/workspaces/{id}/rules — 创建规则
async fn create_rule(
    State(state): State<WorkspaceState>,
    Path(workspace_id): Path<String>,
    Json(req): Json<CreateRuleRequest>,
) -> Result<(StatusCode, Json<RuleRecord>), WorkspaceError> {
    let rule = state
        .rule_meta_service
        .create_rule(&workspace_id, req)
        .await?;
    Ok((StatusCode::CREATED, Json(rule)))
}

#[utoipa::path(
    get,
    path = "/api/workspaces/{id}/rules",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
    ),
    responses(
        (status = 200, description = "success", body = Vec<RuleRecord>),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// GET /api/workspaces/{id}/rules — 列出规则
async fn list_rules(
    State(state): State<WorkspaceState>,
    Path(workspace_id): Path<String>,
) -> Result<Json<Vec<RuleRecord>>, WorkspaceError> {
    let rules = state.rule_meta_service.list_rules(&workspace_id).await?;
    Ok(Json(rules))
}

#[utoipa::path(
    get,
    path = "/api/workspaces/{id}/rules/{rule_id}",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
        ("rule_id" = String, Path, description = "规则 ID"),
    ),
    responses(
        (status = 200, description = "success", body = RuleRecord),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// GET /api/workspaces/{id}/rules/{rule_id} — 获取规则
async fn get_rule(
    State(state): State<WorkspaceState>,
    Path((workspace_id, rule_id)): Path<(String, String)>,
) -> Result<Json<RuleRecord>, WorkspaceError> {
    let rule = state
        .rule_meta_service
        .get_rule(&workspace_id, &rule_id)
        .await?;
    Ok(Json(rule))
}

#[utoipa::path(
    patch,
    path = "/api/workspaces/{id}/rules/{rule_id}",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
        ("rule_id" = String, Path, description = "规则 ID"),
    ),
    request_body = UpdateRuleContentRequest,
    responses(
        (status = 200, description = "success", body = RuleVersionRecord),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// PATCH /api/workspaces/{id}/rules/{rule_id} — 更新规则内容 (仅 Draft 状态)
async fn update_rule_content(
    State(state): State<WorkspaceState>,
    Path((workspace_id, rule_id)): Path<(String, String)>,
    Json(req): Json<UpdateRuleContentRequest>,
) -> Result<Json<RuleVersionRecord>, WorkspaceError> {
    let rv = state
        .rule_meta_service
        .update_rule_content(&workspace_id, &rule_id, req)
        .await?;
    Ok(Json(rv))
}

#[utoipa::path(
    post,
    path = "/api/workspaces/{id}/rules/{rule_id}/activate",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
        ("rule_id" = String, Path, description = "规则 ID"),
    ),
    responses(
        (status = 200, description = "success", body = RuleRecord),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// POST /api/workspaces/{id}/rules/{rule_id}/activate — 激活规则
async fn activate_rule(
    State(state): State<WorkspaceState>,
    Path((workspace_id, rule_id)): Path<(String, String)>,
) -> Result<Json<RuleRecord>, WorkspaceError> {
    let rule = state
        .rule_meta_service
        .activate_rule(&workspace_id, &rule_id)
        .await?;
    Ok(Json(rule))
}

#[utoipa::path(
    post,
    path = "/api/workspaces/{id}/rules/{rule_id}/submit",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
        ("rule_id" = String, Path, description = "规则 ID"),
    ),
    responses(
        (status = 200, description = "success", body = RuleRecord),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// POST /api/workspaces/{id}/rules/{rule_id}/submit — 提交候选 (Draft → Candidate)
///
/// 将 Draft 状态的规则提交为 Candidate, 进入发布队列待选状态。
/// (SANDBOX_ORCHESTRATION_DESIGN.md + PUBLISH_QUEUE_DESIGN.md 前置流程)
async fn submit_rule(
    State(state): State<WorkspaceState>,
    Path((workspace_id, rule_id)): Path<(String, String)>,
) -> Result<Json<RuleRecord>, WorkspaceError> {
    let rule = state
        .rule_meta_service
        .submit_rule(&workspace_id, &rule_id)
        .await?;
    Ok(Json(rule))
}

#[utoipa::path(
    post,
    path = "/api/workspaces/{id}/rules/{rule_id}/block",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
        ("rule_id" = String, Path, description = "规则 ID"),
    ),
    responses(
        (status = 200, description = "success", body = RuleRecord),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// POST /api/workspaces/{id}/rules/{rule_id}/block — 阻塞规则
async fn block_rule(
    State(state): State<WorkspaceState>,
    Path((workspace_id, rule_id)): Path<(String, String)>,
) -> Result<Json<RuleRecord>, WorkspaceError> {
    let rule = state
        .rule_meta_service
        .block_rule(&workspace_id, &rule_id)
        .await?;
    Ok(Json(rule))
}

#[utoipa::path(
    post,
    path = "/api/workspaces/{id}/rules/{rule_id}/archive",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
        ("rule_id" = String, Path, description = "规则 ID"),
    ),
    responses(
        (status = 200, description = "success", body = RuleRecord),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// POST /api/workspaces/{id}/rules/{rule_id}/archive — 归档规则
async fn archive_rule(
    State(state): State<WorkspaceState>,
    Path((workspace_id, rule_id)): Path<(String, String)>,
) -> Result<Json<RuleRecord>, WorkspaceError> {
    let rule = state
        .rule_meta_service
        .archive_rule(&workspace_id, &rule_id)
        .await?;
    Ok(Json(rule))
}

#[utoipa::path(
    post,
    path = "/api/workspaces/{id}/rules/{rule_id}/fork",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
        ("rule_id" = String, Path, description = "规则 ID"),
    ),
    request_body = ForkRuleRequest,
    responses(
        (status = 201, description = "created", body = RuleRecord),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// POST /api/workspaces/{id}/rules/{rule_id}/fork — fork 规则
async fn fork_rule(
    State(state): State<WorkspaceState>,
    Path((workspace_id, rule_id)): Path<(String, String)>,
    Json(req): Json<ForkRuleRequest>,
) -> Result<(StatusCode, Json<RuleRecord>), WorkspaceError> {
    let rule = state
        .rule_meta_service
        .fork_rule(&workspace_id, &rule_id, &req.new_name, &req.created_by)
        .await?;
    Ok((StatusCode::CREATED, Json(rule)))
}

#[utoipa::path(
    get,
    path = "/api/workspaces/{id}/rules/{rule_id}/versions",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
        ("rule_id" = String, Path, description = "规则 ID"),
    ),
    responses(
        (status = 200, description = "success", body = Vec<RuleVersionRecord>),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// GET /api/workspaces/{id}/rules/{rule_id}/versions — 列出规则全部版本(含 content)
///
/// 阶段 D: 暴露 `RuleMetaService::list_rule_versions` 为 HTTP 端点。
/// 返回 `Vec<RuleVersionRecord>`,按 version 降序(首条为 Current)。
/// console 编辑器据此懒加载规则 content。
async fn list_rule_versions_handler(
    State(state): State<WorkspaceState>,
    Path((workspace_id, rule_id)): Path<(String, String)>,
) -> Result<Json<Vec<RuleVersionRecord>>, WorkspaceError> {
    let list = state
        .rule_meta_service
        .list_rule_versions(&workspace_id, &rule_id)
        .await?;
    Ok(Json(list))
}

#[utoipa::path(
    get,
    path = "/api/workspaces/{id}/rules/{rule_id}/versions/{version_id}",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
        ("rule_id" = String, Path, description = "规则 ID"),
        ("version_id" = String, Path, description = "版本 ID"),
    ),
    responses(
        (status = 200, description = "success", body = RuleVersionRecord),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// GET /api/workspaces/{id}/rules/{rule_id}/versions/{version_id} — 获取规则指定版本
async fn get_rule_version_handler(
    State(state): State<WorkspaceState>,
    Path((workspace_id, rule_id, version_id)): Path<(String, String, String)>,
) -> Result<Json<RuleVersionRecord>, WorkspaceError> {
    let rv = state
        .rule_meta_service
        .get_rule_version(&workspace_id, &rule_id, &version_id)
        .await?;
    Ok(Json(rv))
}

// =============================================================================
// 会话管理 handler
// =============================================================================

#[utoipa::path(
    post,
    path = "/api/workspaces/{id}/sessions",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
    ),
    request_body = CreateSessionRequest,
    responses(
        (status = 201, description = "created", body = SessionRecord),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// POST /api/workspaces/{id}/sessions — 创建会话
async fn create_session(
    State(state): State<WorkspaceState>,
    Path(workspace_id): Path<String>,
    Json(req): Json<CreateSessionRequest>,
) -> Result<(StatusCode, Json<SessionRecord>), WorkspaceError> {
    let session = state
        .workspace_service
        .create_session(&workspace_id, req)
        .await?;
    Ok((StatusCode::CREATED, Json(session)))
}

#[utoipa::path(
    get,
    path = "/api/workspaces/{id}/sessions",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
    ),
    responses(
        (status = 200, description = "success", body = Vec<SessionRecord>),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// GET /api/workspaces/{id}/sessions — 列出会话
async fn list_sessions(
    State(state): State<WorkspaceState>,
    Path(workspace_id): Path<String>,
) -> Result<Json<Vec<SessionRecord>>, WorkspaceError> {
    let sessions = state.workspace_service.list_sessions(&workspace_id).await?;
    Ok(Json(sessions))
}

// =============================================================================
// 沙盒编排 handler (SANDBOX_ORCHESTRATION_DESIGN.md §6)
// =============================================================================
//
// 权限模型: 沙盒端点仅校验 workspace 成员身份 (不使用 Doctor/DepartmentHead/Admin
// 三级发布角色)。成员校验由 SandboxService 内部通过 is_workspace_member 完成。

#[utoipa::path(
    post,
    path = "/api/workspaces/{id}/sandboxes",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
    ),
    request_body = StartSandboxHttpRequest,
    responses(
        (status = 201, description = "created", body = StartSandboxResponse),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// POST /api/workspaces/{id}/sandboxes — 启动沙盒测试
async fn start_sandbox(
    State(state): State<WorkspaceState>,
    Path(workspace_id): Path<String>,
    Json(req): Json<StartSandboxHttpRequest>,
) -> Result<(StatusCode, Json<StartSandboxResponse>), WorkspaceError> {
    let resp = state
        .sandbox_service
        .start_sandbox(&workspace_id, req.payload, &req.started_by)
        .await?;
    Ok((StatusCode::CREATED, Json(resp)))
}

#[utoipa::path(
    get,
    path = "/api/workspaces/{id}/sandboxes",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
        ("requester" = String, Query, description = "请求者用户 ID (用于 workspace 成员权限校验)"),
    ),
    responses(
        (status = 200, description = "success", body = Vec<SandboxSession>),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// GET /api/workspaces/{id}/sandboxes — 列出沙盒测试历史
async fn list_sandboxes(
    State(state): State<WorkspaceState>,
    Path(workspace_id): Path<String>,
    Query(q): Query<SandboxRequesterQuery>,
) -> Result<Json<Vec<SandboxSession>>, WorkspaceError> {
    let list = state
        .sandbox_service
        .list_sandboxes(&workspace_id, &q.requester)
        .await?;
    Ok(Json(list))
}

#[utoipa::path(
    get,
    path = "/api/workspaces/{id}/sandboxes/{sandbox_id}",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
        ("sandbox_id" = i64, Path, description = "沙盒 ID"),
        ("requester" = String, Query, description = "请求者用户 ID (用于 workspace 成员权限校验)"),
    ),
    responses(
        (status = 200, description = "success", body = SandboxSession),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// GET /api/workspaces/{id}/sandboxes/{sandbox_id} — 查看沙盒详情
async fn get_sandbox(
    State(state): State<WorkspaceState>,
    Path((workspace_id, sandbox_id)): Path<(String, i64)>,
    Query(q): Query<SandboxRequesterQuery>,
) -> Result<Json<SandboxSession>, WorkspaceError> {
    let sandbox = state
        .sandbox_service
        .get_sandbox(&workspace_id, sandbox_id, &q.requester)
        .await?;
    Ok(Json(sandbox))
}

#[utoipa::path(
    post,
    path = "/api/workspaces/{id}/sandboxes/{sandbox_id}/close",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
        ("sandbox_id" = i64, Path, description = "沙盒 ID"),
    ),
    request_body = CloseSandboxRequest,
    responses(
        (status = 200, description = "关闭结果", body = serde_json::Value),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// POST /api/workspaces/{id}/sandboxes/{sandbox_id}/close — 关闭沙盒
async fn close_sandbox(
    State(state): State<WorkspaceState>,
    Path((workspace_id, sandbox_id)): Path<(String, i64)>,
    Json(req): Json<CloseSandboxRequest>,
) -> Result<Json<Value>, WorkspaceError> {
    let export_path = state
        .sandbox_service
        .close_sandbox(sandbox_id, &req.closed_by)
        .await?;
    Ok(Json(serde_json::json!({
        "sandbox_id": sandbox_id,
        "workspace_id": workspace_id,
        "status": "closed",
        "export_path": export_path,
    })))
}

#[utoipa::path(
    get,
    path = "/api/workspaces/{id}/sandboxes/{sandbox_id}/report",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
        ("sandbox_id" = i64, Path, description = "沙盒 ID"),
    ),
    responses(
        (status = 200, description = "success", body = TestReport),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// GET /api/workspaces/{id}/sandboxes/{sandbox_id}/report — 获取测试报告
async fn get_sandbox_report(
    State(state): State<WorkspaceState>,
    Path((_workspace_id, sandbox_id)): Path<(String, i64)>,
) -> Result<Json<TestReport>, WorkspaceError> {
    let report = state
        .sandbox_service
        .generate_test_report(sandbox_id)
        .await?;
    Ok(Json(report))
}

// =============================================================================
// 测试数据集 handler (SANDBOX_ORCHESTRATION_DESIGN.md §3)
// =============================================================================

#[utoipa::path(
    post,
    path = "/api/workspaces/{id}/test-datasets",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
    ),
    request_body = CreateTestDatasetRequest,
    responses(
        (status = 201, description = "created", body = TestDatasetRecord),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// POST /api/workspaces/{id}/test-datasets — 创建合成测试数据集
async fn create_test_dataset(
    State(state): State<WorkspaceState>,
    Path(workspace_id): Path<String>,
    Json(req): Json<CreateTestDatasetRequest>,
) -> Result<(StatusCode, Json<TestDatasetRecord>), WorkspaceError> {
    let dataset = state
        .sandbox_service
        .create_test_dataset(&workspace_id, req)
        .await?;
    Ok((StatusCode::CREATED, Json(dataset)))
}

#[utoipa::path(
    get,
    path = "/api/workspaces/{id}/test-datasets",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
    ),
    responses(
        (status = 200, description = "success", body = Vec<TestDatasetRecord>),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// GET /api/workspaces/{id}/test-datasets — 列出测试数据集
async fn list_test_datasets(
    State(state): State<WorkspaceState>,
    Path(workspace_id): Path<String>,
) -> Result<Json<Vec<TestDatasetRecord>>, WorkspaceError> {
    let list = state
        .sandbox_service
        .list_test_datasets(&workspace_id)
        .await?;
    Ok(Json(list))
}

// =============================================================================
// 发布队列 handler (PUBLISH_QUEUE_DESIGN.md §6)
// =============================================================================
//
// 三级权限 （决策）:
// - Doctor: 仅可编辑 Draft, 不可提交发布
// - DepartmentHead: 可提交到发布队列 (本科室 WS), 不可审批
// - Admin: 可审批发布 (全院) + 紧急回滚
//
// P0 简化: 角色从请求体 role 字段传入 (serde 自动反序列化 PublishRole)。
// P1 接入 auth middleware 后, 改为从 AuthUser claims 解析。

#[utoipa::path(
    post,
    path = "/api/publish/queue",
    tag = "workspace",
    request_body = SubmitPublishHttpRequest,
    responses(
        (status = 201, description = "created", body = PublishQueueItem),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// POST /api/publish/queue — 提交到发布队列 (DepartmentHead 权限)
async fn submit_publish(
    State(state): State<WorkspaceState>,
    Json(req): Json<SubmitPublishHttpRequest>,
) -> Result<(StatusCode, Json<PublishQueueItem>), WorkspaceError> {
    let item = state
        .publish_service
        .submit_publish(req.payload, &req.submitted_by, &req.role)
        .await?;
    Ok((StatusCode::CREATED, Json(item)))
}

#[utoipa::path(
    get,
    path = "/api/publish/queue",
    tag = "workspace",
    params(
        ("status" = Option<String>, Query, description = "按状态过滤 (pending/approved/published/rejected/cancelled)"),
    ),
    responses(
        (status = 200, description = "success", body = Vec<PublishQueueItem>),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// GET /api/publish/queue — 列出发布队列 (所有角色可查看)
async fn list_publish_queue(
    State(state): State<WorkspaceState>,
    Query(q): Query<ListPublishQueueQuery>,
) -> Result<Json<Vec<PublishQueueItem>>, WorkspaceError> {
    let status_filter = match q.status.as_deref() {
        Some(s) => Some(PublishStatus::from_str(s).ok_or_else(|| {
            WorkspaceError::invalid_input(format!(
                "invalid status filter: {s} (expected pending/approved/published/rejected/cancelled)"
            ))
        })?),
        None => None,
    };
    let list = state.publish_service.list_queue(status_filter).await?;
    Ok(Json(list))
}

#[utoipa::path(
    get,
    path = "/api/publish/queue/{queue_id}",
    tag = "workspace",
    params(
        ("queue_id" = i64, Path, description = "队列项 ID"),
    ),
    responses(
        (status = 200, description = "success", body = PublishQueueItem),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// GET /api/publish/queue/{queue_id} — 查看队列项详情 (所有角色可查看)
async fn get_publish_queue_item(
    State(state): State<WorkspaceState>,
    Path(queue_id): Path<i64>,
) -> Result<Json<PublishQueueItem>, WorkspaceError> {
    let item = state.publish_service.get_queue_item(queue_id).await?;
    Ok(Json(item))
}

#[utoipa::path(
    post,
    path = "/api/publish/queue/{queue_id}/review",
    tag = "workspace",
    params(
        ("queue_id" = i64, Path, description = "队列项 ID"),
    ),
    request_body = ReviewPublishHttpRequest,
    responses(
        (status = 200, description = "success", body = PublishQueueItem),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// POST /api/publish/queue/{queue_id}/review — 审批发布 (Admin 权限)
async fn review_publish(
    State(state): State<WorkspaceState>,
    Path(queue_id): Path<i64>,
    Json(req): Json<ReviewPublishHttpRequest>,
) -> Result<Json<PublishQueueItem>, WorkspaceError> {
    let item = state
        .publish_service
        .review_publish(queue_id, req.payload, &req.reviewed_by, &req.role)
        .await?;
    Ok(Json(item))
}

#[utoipa::path(
    post,
    path = "/api/publish/rollback",
    tag = "workspace",
    request_body = RollbackHttpRequest,
    responses(
        (status = 200, description = "回滚结果", body = serde_json::Value),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// POST /api/publish/rollback — 紧急回滚 (Admin 权限)
///
/// 版本号单调递增: 回滚到 target_version 的规则集, 但新版本号 = 当前版本 + 1 (不回退)。
async fn emergency_rollback(
    State(state): State<WorkspaceState>,
    Json(req): Json<RollbackHttpRequest>,
) -> Result<Json<Value>, WorkspaceError> {
    let target_version = req.payload.target_version;
    let new_version = state
        .publish_service
        .emergency_rollback(req.payload, &req.operated_by, &req.role)
        .await?;
    Ok(Json(serde_json::json!({
        "new_ruleset_version": new_version,
        "rolled_back_to": target_version,
        "message": format!(
            "Rolled back to v{target_version} (new version: v{new_version})"
        ),
    })))
}

#[utoipa::path(
    get,
    path = "/api/production/state",
    tag = "workspace",
    responses(
        (status = 200, description = "success", body = ProductionStateRecord),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// GET /api/production/state — 查询当前生产状态 (所有角色可查看)
async fn get_production_state(
    State(state): State<WorkspaceState>,
) -> Result<Json<ProductionStateRecord>, WorkspaceError> {
    let state_rec = state.publish_service.get_production_state().await?;
    Ok(Json(state_rec))
}

#[utoipa::path(
    get,
    path = "/api/production/audit",
    tag = "workspace",
    params(
        ("limit" = i64, Query, description = "返回记录上限 (默认 50)"),
    ),
    responses(
        (status = 200, description = "success", body = Vec<ProductionAuditRecord>),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// GET /api/production/audit — 查询发布审计历史 (所有角色可查看)
async fn list_production_audit(
    State(state): State<WorkspaceState>,
    Query(q): Query<ListProductionAuditQuery>,
) -> Result<Json<Vec<ProductionAuditRecord>>, WorkspaceError> {
    let list = state.publish_service.list_production_audit(q.limit).await?;
    Ok(Json(list))
}

// =============================================================================
// 规则转译 handler (界面升级 v1.0 阶段 A.2, 纯函数, 不读 db)
// =============================================================================

#[utoipa::path(
    post,
    path = "/api/rules/translate/to_transform",
    tag = "workspace",
    request_body = TranslateToTransformRequest,
    responses(
        (status = 200, description = "success", body = TranslateToTransformResponse),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// POST /api/rules/translate/to_transform
///
/// condition + action_set → transform (生成结构, 末条补 all([]) 兜底, 跑 G1-G7)
async fn translate_to_transform_handler(
    Json(req): Json<TranslateToTransformRequest>,
) -> Result<Json<TranslateToTransformResponse>, WorkspaceError> {
    let resp = rule_translate::translate_to_transform(req)?;
    Ok(Json(resp))
}

#[utoipa::path(
    post,
    path = "/api/rules/translate/to_conditional",
    tag = "workspace",
    request_body = TranslateToConditionalRequest,
    responses(
        (status = 200, description = "success", body = TranslateToConditionalResponse),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// POST /api/rules/translate/to_conditional
///
/// transform → condition + action_set (lossy: push/io_request/嵌套超出子集)
async fn translate_to_conditional_handler(
    Json(req): Json<TranslateToConditionalRequest>,
) -> Result<Json<TranslateToConditionalResponse>, WorkspaceError> {
    let resp = rule_translate::translate_to_conditional(req)?;
    Ok(Json(resp))
}

// =============================================================================
// 判定契约 handler (界面升级 v1.0 阶段 A.3, 应用层业务判定, 非确定性)
// =============================================================================

#[utoipa::path(
    post,
    path = "/api/workspaces/{id}/verdict_contracts",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
    ),
    request_body = CreateVerdictContractRequest,
    responses(
        (status = 201, description = "created", body = VerdictContractRecord),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// POST /api/workspaces/{id}/verdict_contracts — 创建判定契约
async fn create_verdict_contract(
    State(state): State<WorkspaceState>,
    Path(workspace_id): Path<String>,
    Json(req): Json<CreateVerdictContractRequest>,
) -> Result<(StatusCode, Json<VerdictContractRecord>), WorkspaceError> {
    let rec = state
        .verdict_service
        .create_contract(&workspace_id, req)
        .await?;
    Ok((StatusCode::CREATED, Json(rec)))
}

#[utoipa::path(
    get,
    path = "/api/workspaces/{id}/verdict_contracts",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
    ),
    responses(
        (status = 200, description = "success", body = Vec<VerdictContractRecord>),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// GET /api/workspaces/{id}/verdict_contracts — 列出判定契约
async fn list_verdict_contracts(
    State(state): State<WorkspaceState>,
    Path(workspace_id): Path<String>,
) -> Result<Json<Vec<VerdictContractRecord>>, WorkspaceError> {
    let list = state.verdict_service.list_contracts(&workspace_id).await?;
    Ok(Json(list))
}

#[utoipa::path(
    get,
    path = "/api/workspaces/{id}/verdict_contracts/{cid}",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
        ("cid" = i64, Path, description = "契约 ID"),
    ),
    responses(
        (status = 200, description = "success", body = VerdictContractRecord),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// GET /api/workspaces/{id}/verdict_contracts/{cid} — 获取单条契约
async fn get_verdict_contract(
    State(state): State<WorkspaceState>,
    Path((_workspace_id, cid)): Path<(String, i64)>,
) -> Result<Json<VerdictContractRecord>, WorkspaceError> {
    let rec = state.verdict_service.get_contract(cid).await?;
    Ok(Json(rec))
}

#[utoipa::path(
    patch,
    path = "/api/workspaces/{id}/verdict_contracts/{cid}",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
        ("cid" = i64, Path, description = "契约 ID"),
    ),
    request_body = UpdateVerdictContractRequest,
    responses(
        (status = 200, description = "success", body = VerdictContractRecord),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// PATCH /api/workspaces/{id}/verdict_contracts/{cid} — 更新契约 (patch)
async fn update_verdict_contract(
    State(state): State<WorkspaceState>,
    Path((_workspace_id, cid)): Path<(String, i64)>,
    Json(req): Json<UpdateVerdictContractRequest>,
) -> Result<Json<VerdictContractRecord>, WorkspaceError> {
    let rec = state.verdict_service.update_contract(cid, req).await?;
    Ok(Json(rec))
}

#[utoipa::path(
    delete,
    path = "/api/workspaces/{id}/verdict_contracts/{cid}",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
        ("cid" = i64, Path, description = "契约 ID"),
    ),
    responses(
        (status = 204, description = "deleted"),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// DELETE /api/workspaces/{id}/verdict_contracts/{cid} — 删除契约
async fn delete_verdict_contract(
    State(state): State<WorkspaceState>,
    Path((_workspace_id, cid)): Path<(String, i64)>,
) -> Result<StatusCode, WorkspaceError> {
    state.verdict_service.delete_contract(cid).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/api/workspaces/{id}/verdict/evaluate",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID"),
    ),
    request_body = EvaluateVerdictRequest,
    responses(
        (status = 200, description = "success", body = EvaluateVerdictResult),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// POST /api/workspaces/{id}/verdict/evaluate — 判定 (应用层, 非确定性)
///
/// 返回值含 note 确定性标注 (00 §七)。
async fn evaluate_verdict(
    State(state): State<WorkspaceState>,
    Path(workspace_id): Path<String>,
    Json(req): Json<EvaluateVerdictRequest>,
) -> Result<Json<EvaluateVerdictResult>, WorkspaceError> {
    let result = state.verdict_service.evaluate(&workspace_id, req).await?;
    Ok(Json(result))
}

// =============================================================================
// wall-clock 旁路 handler (界面升级 v1.0 阶段 A.4)
// =============================================================================

#[utoipa::path(
    post,
    path = "/api/sessions/{id}/clock/record",
    tag = "workspace",
    params(
        ("id" = i64, Path, description = "会话 ID"),
    ),
    request_body = RecordClockRequest,
    responses(
        (status = 200, description = "success", body = VersionClockMapRecord),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// POST /api/sessions/{id}/clock/record — 旁路记录 version → wall-clock
///
/// 供 server reactor 产生 Fact 时事务外旁路写入; 绝不进审计链哈希 (00 §六/§七)。
async fn record_clock(
    State(state): State<WorkspaceState>,
    Path(session_id): Path<i64>,
    Json(req): Json<RecordClockRequest>,
) -> Result<Json<VersionClockMapRecord>, WorkspaceError> {
    let rec = state
        .verdict_service
        .record_clock(
            session_id,
            req.version,
            &req.wall_clock,
            req.source.as_deref(),
        )
        .await?;
    Ok(Json(rec))
}

#[utoipa::path(
    get,
    path = "/api/sessions/{id}/clock/lookup",
    tag = "workspace",
    params(
        ("id" = i64, Path, description = "会话 ID"),
        ("from_version" = Option<i64>, Query, description = "起始版本"),
        ("to_version" = Option<i64>, Query, description = "结束版本"),
    ),
    responses(
        (status = 200, description = "success", body = Vec<VersionClockMapRecord>),
        (status = 400, description = "bad request"),
        (status = 404, description = "not found"),
    )
)]
/// GET /api/sessions/{id}/clock/lookup — 范围查询 version → wall-clock
async fn lookup_clock(
    State(state): State<WorkspaceState>,
    Path(session_id): Path<i64>,
    Query(q): Query<LookupClockQuery>,
) -> Result<Json<Vec<VersionClockMapRecord>>, WorkspaceError> {
    let list = state
        .verdict_service
        .lookup_clock(session_id, q.from_version, q.to_version)
        .await?;
    Ok(Json(list))
}

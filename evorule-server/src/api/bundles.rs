// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 快照包导入端点（·历史批次 集成契约 / 历史批次 bundles）
//!
//! - `POST /api/bundles/import`：6 项硬校验 + 逐条 Schema 门禁 + I/O 权利面重叠
//!   前置预判（重叠 → 400 规则对明细）+ 原子落盘 + 触发 reload；
//! - `POST /api/bundles/import/dry-run`：只跑校验链，不落盘不 reload。
//!
//! 框架层无 RBAC（D12：审批权威留在治理层 evorule-rule），此处仅要求有效 token（受保护路由）。
//! 校验失败一律 400 显式错误（不静默降级，设计文档 §9 / T0）。

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use evorule_bundle::VersionSelectionMode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

use crate::api::server::{ImportOutcome, RejectedDomainEntry, RejectedOverlapEntry, SessionApi};

/// 导入请求体（契约复刻治理侧 `ImportReq`：`{"bundle": DatasetBundle}`）
#[derive(Debug, Deserialize)]
pub struct ImportReq {
    pub bundle: evorule_bundle::DatasetBundle,
}

/// 落盘 manifest（`rules/bundles/{bundle_id}/bundle_manifest.json`，T3）
///
/// 记录单版本快照的运行配置元数据：版本语义（source_version/selection_mode/
/// resolved_version）+ 法规生效基准（law_ref.effective_from）+ 防篡改哈希 + 条目→文件映射。
/// 仅供溯源/运行配置读取，**不参与** loader 加载路径（loader 递归扫描条目 .json 原样加载）。
///
/// 审计⑥ 批 B（C5）: 类型下沉至 evorule-workspace（落盘 SSOT），
/// 发布链与外部导入通道共用一份落盘实现，此处 re-export 保持路径兼容。
pub use evorule_workspace::bundle_land::{BundleManifest, EntryFileManifest};

/// loader 扫描时排除的 manifest 文件名（约定 SSOT 在 evorule-bundle）
pub use evorule_bundle::BUNDLE_MANIFEST_FILE;

/// 导入成功响应（bundle 为单版本快照 → 导入即激活，T4 细化）
#[derive(Debug, Serialize, ToSchema)]
pub struct ImportResponse {
    pub imported: bool,
    pub bundle_id: String,
    pub dataset_id: String,
    pub activated_version: String,
    pub entry_count: usize,
    /// 硬失败原则：缺失服务已在校验链以显式错误拦截，成功导入即无缺失（设计文档 §9）
    pub missing_services: Vec<String>,
    /// reload 期 I/O 权利面防线拒载的重叠条目（导入前置预判已拦，正常为空数组；
    /// 非空=预判与 reload 之间 rules_dir 并发变更的半激活事实，显式到达调用方不静默）
    pub rejected_overlaps: Vec<RejectedOverlapEntry>,
    /// reload 期 domain 门禁拒载的违规条目（导入前置预判已拦，正常为空数组；
    /// 非空=预判与 reload 之间 rules_dir 并发变更的半激活事实，显式到达调用方不静默）
    pub rejected_domains: Vec<RejectedDomainEntry>,
}

/// 当前激活 bundle 信息（，来自 `bundle_manifest.json` 的精简视图）
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ActiveBundleInfo {
    pub bundle_id: String,
    pub dataset_id: String,
    pub source_version: String,
    /// `auto_by_effective_date` | `pinned`（evorule-bundle 枚举，schema 以字符串表达）
    #[schema(value_type = String)]
    pub selection_mode: VersionSelectionMode,
    /// pinned 已解析版本；auto 运行时按事件日期解析（None）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_version: Option<String>,
    /// law_ref.effective_from 基准（auto 模式的运行配置元数据）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_from: Option<String>,
    pub content_hash: String,
    pub entry_count: usize,
}

impl From<&BundleManifest> for ActiveBundleInfo {
    fn from(m: &BundleManifest) -> Self {
        ActiveBundleInfo {
            bundle_id: m.bundle_id.clone(),
            dataset_id: m.dataset_id.clone(),
            source_version: m.source_version.clone(),
            selection_mode: m.selection_mode,
            resolved_version: m.resolved_version.clone(),
            effective_from: m.effective_from.clone(),
            content_hash: m.content_hash.clone(),
            entry_count: m.entry_files.len(),
        }
    }
}

/// GET /api/bundles/active 响应
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ActiveBundlesResponse {
    pub bundles: Vec<ActiveBundleInfo>,
    pub count: usize,
}

/// POST /api/bundles/import —— 导入快照包并激活
///
/// 请求体为治理层 `DatasetBundle` 快照包 JSON（设计文档 §2）。6 项校验链任一失败
/// → 400 显式 `BundleError`（不静默）；成功 → 201 返回导入结果。
#[utoipa::path(
    post,
    path = "/api/bundles/import",
    tag = "bundles",
    request_body = serde_json::Value,
    responses(
        (status = 201, description = "导入成功并激活", body = ImportResponse),
        (status = 400, description = "校验/落盘失败（显式错误，不静默）", body = serde_json::Value),
        (status = 401, description = "未认证")
    )
)]
pub async fn import_bundle_handler(
    State(sessions): State<SessionApi>,
    Json(req): Json<ImportReq>,
) -> Result<(StatusCode, Json<ImportResponse>), (StatusCode, Json<Value>)> {
    let outcome = sessions
        .import_bundle(&req.bundle, false)
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": e, "imported": false })),
            )
        })?;
    let ImportOutcome {
        result,
        rejected_overlaps,
        rejected_domains,
    } = outcome;
    Ok((
        StatusCode::CREATED,
        Json(ImportResponse {
            imported: true,
            bundle_id: result.bundle_id,
            dataset_id: result.dataset_id,
            activated_version: result.source_version,
            entry_count: result.entry_count,
            missing_services: Vec::new(),
            rejected_overlaps,
            rejected_domains,
        }),
    ))
}

/// POST /api/bundles/import/dry-run —— 导入预检（校验链全跑，不落盘不 reload）
#[utoipa::path(
    post,
    path = "/api/bundles/import/dry-run",
    tag = "bundles",
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "预检通过", body = serde_json::Value),
        (status = 400, description = "预检失败（显式错误，不静默）", body = serde_json::Value),
        (status = 401, description = "未认证")
    )
)]
pub async fn import_bundle_dry_run_handler(
    State(sessions): State<SessionApi>,
    Json(req): Json<ImportReq>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let result = sessions
        .import_bundle(&req.bundle, true)
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": e, "valid": false })),
            )
        })?
        .result;
    Ok(Json(serde_json::json!({
        "valid": true,
        "bundle_id": result.bundle_id,
        "dataset_id": result.dataset_id,
        "source_version": result.source_version,
        "selection_mode": format!("{:?}", result.selection_mode),
        "resolved_version": result.resolved_version,
        "entry_count": result.entry_count,
        "verdict": format!("{:?}", result.verdict),
        "missing_services": [],
    })))
}

/// GET /api/bundles/active —— 报告当前激活的 bundle（版本语义）
///
/// 遍历 `rules/bundles/*/bundle_manifest.json` 返回各 dataset 当前激活快照
/// （bundle_id/dataset_id/source_version/selection_mode/resolved_version/effective_from/
/// content_hash/entry_count）。目录不存在 → 200 空列表；manifest 读取/解析失败
/// → **500 显式错误**（不静默掩盖激活状态）。
#[utoipa::path(
    get,
    path = "/api/bundles/active",
    tag = "bundles",
    responses(
        (status = 200, description = "当前激活的 bundle 列表（可能为空）", body = ActiveBundlesResponse),
        (status = 401, description = "未认证"),
        (status = 500, description = "manifest 读取/解析失败（显式错误）", body = serde_json::Value)
    )
)]
pub async fn active_bundles_handler(
    State(api): State<SessionApi>,
) -> Result<Json<ActiveBundlesResponse>, (StatusCode, Json<Value>)> {
    let manifests = api.active_bundles().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e })),
        )
    })?;
    let bundles: Vec<ActiveBundleInfo> = manifests.iter().map(ActiveBundleInfo::from).collect();
    let count = bundles.len();
    Ok(Json(ActiveBundlesResponse { bundles, count }))
}

/// GET /api/bundles/imports 响应 —— T5 bundle 导入溯源记录（管理元数据旁路）
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BundleImportsResponse {
    pub imports: Vec<evorule_workspace::BundleImportRecord>,
    pub count: usize,
}

/// GET /api/bundles/imports —— 查询 bundle 导入溯源历史
///
/// 读取 workspace 元数据库 `bundle_imports` 表（按导入时间倒序, `?limit=` 限制条数, 默认 100）。
/// 记录为**管理元数据**（imported_at 墙钟旁路），不参与 fact / 内容哈希 / 审计验证链。
/// workspace 元数据库未接线 → 200 空列表；查询失败 → **500 显式错误**（不静默掩盖审计缺失）。
#[utoipa::path(
    get,
    path = "/api/bundles/imports",
    tag = "bundles",
    params(
        ("limit" = Option<i64>, Query, description = "返回条数上限（默认 100, 最大 1000）")
    ),
    responses(
        (status = 200, description = "bundle 导入溯源记录（可能为空）", body = BundleImportsResponse),
        (status = 401, description = "未认证"),
        (status = 500, description = "溯源查询失败（显式错误）", body = serde_json::Value)
    )
)]
pub async fn list_bundle_imports_handler(
    State(api): State<SessionApi>,
    Query(q): Query<ListBundleImportsQuery>,
) -> Result<Json<BundleImportsResponse>, (StatusCode, Json<Value>)> {
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let imports = api.list_bundle_imports(limit).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e })),
        )
    })?;
    let count = imports.len();
    Ok(Json(BundleImportsResponse { imports, count }))
}

/// `?limit=` 查询参数
#[derive(Debug, Deserialize)]
pub struct ListBundleImportsQuery {
    #[serde(default)]
    pub limit: Option<i64>,
}

// =============================================================================
// 快照包导出端点 —— workspace 治理域带证据导出（部署链上游）
//
// - `POST /api/bundles/export`：把 workspace 的活规则集（state=Active）构造为
//   自洽 DatasetBundle（确定性 bundle_id + 全包哈希 + 证据如实携带 + 可选策略
//   快照入哈希链），供执行域 `bundle_import_dry_run` / `bundle_import` 消费，
//   与既有导入端点构成部署闭环（导出≠发布：不落盘、不激活、不 reload）。
//
// 数据源语义：dataset_id = workspace_id（与发布链 `dataset_id=workspace_id`
// 同口径，见 publish_service 构造）。workspace 域无历史版本快照链，仅支持
// `version="current"` 活规则集导出，历史版显式 400（不静默伪造）。
//
// 证据纪律（与治理侧 export_with_tests 双闸同口径）：verdict=pass 必带可追溯
// 标记（sandbox:<id> / human:<actor> 前缀），服务端 enforcement——客户端
// 纪律不构成校验；verdict=fail 无此要求（fail 即"未验证"，无伪造风险）。
// `sandbox:` 引用的存在性+报告一致性由 import 侧校验（双闸各管一层）。
// =============================================================================

/// 导出请求体（契约对齐治理侧 `ExportReq` 形状：dataset_id/version/tests/subset/recipe_snapshot）
#[derive(Debug, Deserialize)]
pub struct BundleExportReq {
    /// 数据集 ID（workspace 治理域语义 = workspace_id）
    pub dataset_id: String,
    /// 要导出的版本：仅支持 `current`（活规则集；缺省即 current）
    #[serde(default = "default_export_version")]
    pub version: String,
    /// 测试证据（闸门一产出随包携带；不新增治理存储模型）
    #[serde(default)]
    pub tests: evorule_bundle::BundleTests,
    /// 裁剪视图语法（可选）：`tag:core` / `domain:tax` / `ids:id1,id2`
    /// （多段以 `;` 分隔，交集语义；与治理侧同解析）
    #[serde(default)]
    pub subset: Option<String>,
    /// 策略快照（可选，opaque 载荷）：调用方策略资产在打包时刻的固化副本，
    /// 先入包再算全包哈希（随哈希链防篡改）；缺省 None 不序列化（字节兼容）
    #[serde(default)]
    pub recipe_snapshot: Option<evorule_bundle::RecipeSnapshot>,
}

fn default_export_version() -> String {
    "current".to_string()
}

/// 导出规则输入（规则元数据 + 当前版本内容，供导出核纯函数消费）
pub struct WorkspaceRuleInput {
    /// 规则 ID（ULID，跨导出稳定 → entry_id 稳定）
    pub rule_id: String,
    /// 规则名称（workspaces 内唯一，入 provenance）
    pub name: String,
    /// 规则内容（JSON 字符串，原样零转译入包）
    pub content: String,
}

/// 导出统一错误响应体
fn export_err(status: StatusCode, msg: String) -> (StatusCode, Json<Value>) {
    (status, Json(serde_json::json!({ "error": msg })))
}

/// workspace 治理域当前版导出核（纯函数，单测确定性入口）
///
/// 构造确定性（与发布链 build_publish_bundle 同族口径）：
/// - `entry_id` = `rule-{rule_id}`（ULID 稳定标识，同规则跨导出稳定）；
/// - `bundle_id` = `export-{条目内容哈希级联再哈希前 16 hex}`——同规则集同
///   bundle_id，执行域导入天然幂等（单激活替换语义）；
/// - 版本选择 `pinned` 到 `v1`（`Versioning::default` 链），无墙钟依赖；
///   `exported_at` 为管理元数据（墙钟旁路，与发布链 audit 同口径）；
/// - `data_dependencies = None`——workspace 规则为原生 JSON 规则文档，
///   无服务依赖声明面（与发布链 MVP 同边界）；
/// - `recipe_snapshot` 先入包再算全包哈希（evorule-bundle 哈希链覆盖可选字段）。
///
/// 失败显式（不静默）：空规则集 / 规则内容非法 JSON / 哈希前缀异常一律 Err。
pub fn export_workspace_bundle(
    dataset_id: &str,
    rules: &[WorkspaceRuleInput],
    tests: &evorule_bundle::BundleTests,
    recipe_snapshot: Option<evorule_bundle::RecipeSnapshot>,
    exported_at: &str,
) -> Result<evorule_bundle::DatasetBundle, String> {
    use evorule_bundle::{
        BundleAudit, BundleDatasetMeta, BundleEntry, Provenance, VersionSelection,
        VersionSelectionMode, Versioning, BUNDLE_SCHEMA_VERSION,
    };

    if rules.is_empty() {
        return Err(
            "导出拒绝：数据集无可导出条目（workspace 无 Active 规则；空包是部署事故面，显式失败不静默）"
                .to_string(),
        );
    }
    let mut entries = Vec::with_capacity(rules.len());
    let mut cascade = String::new();
    for r in rules {
        let body: Value = serde_json::from_str(r.content.trim()).map_err(|e| {
            format!(
                "规则 `{}` 内容非法（应为规则 JSON 文档，不静默）: {e}",
                r.name
            )
        })?;
        let content_hash = evorule_hash::digest(r.content.trim().as_bytes());
        cascade.push_str(&content_hash);
        entries.push(BundleEntry {
            entry_id: format!("rule-{}", r.rule_id),
            entry_kind: Default::default(),
            rule_body: body,
            schema_ref: None,
            provenance: Provenance {
                source: format!("workspace:{}/{}", r.rule_id, r.name),
                clause: None,
                document_id: None,
                effective_from: None,
                effective_to: None,
                last_verified: None,
                verified_by: None,
            },
            domain: "general".to_string(),
            tags: Vec::new(),
            dependencies: Vec::new(),
            // workspace 规则为规则条目，四治理字段不携带（None）
            knowledge_kind: None,
            trust_level: None,
            license_ref: None,
            execution_contract: None,
        });
    }
    let cascade_digest = evorule_hash::digest(cascade.as_bytes());
    let bundle_id = format!(
        "export-{}",
        cascade_digest.get(..16).unwrap_or(&cascade_digest)
    );

    let mut bundle = evorule_bundle::DatasetBundle {
        bundle_schema_version: BUNDLE_SCHEMA_VERSION.to_string(),
        bundle_id,
        dataset: BundleDatasetMeta {
            dataset_id: dataset_id.to_string(),
            name: format!("export:{dataset_id}"),
            tenant_id: "local".to_string(),
            instance_id: "evorule-server".to_string(),
            versioning: Versioning::default(),
            version_selection: Some(VersionSelection {
                mode: VersionSelectionMode::Pinned,
                pinned_version: Some("v1".to_string()),
                pinned_include_patch: None,
            }),
            law_ref: None,
            view_of: None,
            event_schemas: vec![],
        },
        entries,
        data_dependencies: None,
        recipe_snapshot,
        tests: tests.clone(),
        audit: BundleAudit {
            exported_at: exported_at.to_string(),
            exported_by: "evorule-server".to_string(),
            source_version: "v1".to_string(),
            content_hash: String::new(),
            hash_algo: "blake3".to_string(),
        },
    };
    bundle.audit.content_hash = bundle.compute_content_hash();
    Ok(bundle)
}

/// 裁剪视图语法解析（与治理侧同解析：`;` 分段交集；tag:/domain:/ids:；
/// 非法段显式错误不静默忽略；空裁剪结果由 BundleTrimmer 拒绝）
fn apply_trim_spec(
    bundle: &evorule_bundle::DatasetBundle,
    spec: &str,
    by: &str,
    at: &str,
) -> Result<evorule_bundle::DatasetBundle, String> {
    use evorule_bundle::BundleTrimmer;
    let mut view: Option<evorule_bundle::DatasetBundle> = None;
    for seg in spec.split(';') {
        let seg = seg.trim();
        if seg.is_empty() {
            continue;
        }
        let (kind, value) = seg.split_once(':').ok_or_else(|| {
            "subset 语法须为 tag:xxx / domain:xxx / ids:id1,id2（多段以 ; 分隔）".to_string()
        })?;
        let current = view.as_ref().unwrap_or(bundle);
        let applied = match kind {
            "tag" => BundleTrimmer::trim_by_filter(current, None, &[value], by, at),
            "domain" => BundleTrimmer::trim_by_filter(current, Some(value), &[], by, at),
            "ids" => {
                let keep: Vec<String> = value
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect();
                BundleTrimmer::trim_by_ids(current, &keep, by, at)
            }
            other => {
                return Err(format!(
                    "未知裁剪段 `{other}:`（合法 kind: tag / domain / ids）"
                ))
            }
        };
        view = Some(applied.map_err(|e| format!("裁剪视图构造失败: {e}"))?);
    }
    view.ok_or_else(|| "subset 为空裁剪表达式（无任何段）".to_string())
}

/// POST /api/bundles/export —— workspace 治理域带证据导出（部署链上游）
///
/// 把 workspace 活规则集构造为自洽 DatasetBundle 返回（不落盘不激活）；
/// 校验失败一律 400/404 显式错误（不静默）。受保护路由（有效 token）。
#[utoipa::path(
    post,
    path = "/api/bundles/export",
    tag = "bundles",
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "导出成功（自洽 DatasetBundle 快照包）", body = serde_json::Value),
        (status = 400, description = "版本/证据形状/规则内容/裁剪语法校验失败（显式错误，不静默）", body = serde_json::Value),
        (status = 401, description = "未认证"),
        (status = 404, description = "数据集（workspace）不存在", body = serde_json::Value)
    )
)]
pub async fn export_bundle_handler(
    State(ws): State<evorule_workspace::WorkspaceState>,
    Json(req): Json<BundleExportReq>,
) -> Result<Json<evorule_bundle::DatasetBundle>, (StatusCode, Json<Value>)> {
    // ① 版本门：workspace 域无历史快照链，仅当前版导出
    if req.version != "current" {
        return Err(export_err(
            StatusCode::BAD_REQUEST,
            format!(
                "版本 `{}` 不可导出：workspace 治理域无历史版本快照链，仅支持 version=\"current\"（活规则集导出）",
                req.version
            ),
        ));
    }
    // ② 证据形状校验（服务端 enforcement，与治理侧 export_with_tests 双闸同口径）
    if req.tests.verdict == evorule_bundle::TestVerdict::Pass {
        let traceable = !req.tests.subset.is_empty()
            && req
                .tests
                .subset
                .iter()
                .all(|s| s.starts_with("sandbox:") || s.starts_with("human:"));
        if !traceable {
            return Err(export_err(
                StatusCode::BAD_REQUEST,
                "证据校验失败:verdict=pass 的导出必须携带可追溯测试标记\
                 (tests.subset 须非空,且每项为 sandbox:<沙盒ID> 或 human:<操作者>)。\
                 机器背书请传入沙盒报告引用,人工背书请传 human:<操作者>\
                 (显式降级,导入侧可追溯);verdict=fail 无此要求"
                    .to_string(),
            ));
        }
    }
    // ③ 数据集存在性（dataset_id = workspace_id；存在性查询失败如实 404 不静默）
    ws.workspace_service
        .get_workspace(&req.dataset_id)
        .await
        .map_err(|e| {
            export_err(
                StatusCode::NOT_FOUND,
                format!(
                    "数据集不存在（workspace `{}` 查询失败: {e}）",
                    req.dataset_id
                ),
            )
        })?;
    // ④ 活规则集收集（state=Active 且有当前版本；内容读取失败显式 400）
    let all_rules = ws
        .rule_meta_service
        .list_rules(&req.dataset_id)
        .await
        .map_err(|e| export_err(StatusCode::BAD_REQUEST, format!("规则清单读取失败: {e}")))?;
    let mut inputs = Vec::new();
    for r in all_rules.iter().filter(|r| {
        r.state == evorule_workspace::RuleState::Active && r.current_version_id.is_some()
    }) {
        let vid = r.current_version_id.clone().unwrap_or_default();
        let ver = ws
            .rule_meta_service
            .get_rule_version(&req.dataset_id, &r.id, &vid)
            .await
            .map_err(|e| {
                export_err(
                    StatusCode::BAD_REQUEST,
                    format!("规则 `{}` 当前版本读取失败: {e}", r.name),
                )
            })?;
        inputs.push(WorkspaceRuleInput {
            rule_id: r.id.clone(),
            name: r.name.clone(),
            content: ver.content,
        });
    }
    // ⑤ 构造自洽 bundle（确定性 bundle_id；全包哈希含策略快照）
    let exported_at = chrono::Utc::now().to_rfc3339();
    let bundle = export_workspace_bundle(
        &req.dataset_id,
        &inputs,
        &req.tests,
        req.recipe_snapshot,
        &exported_at,
    )
    .map_err(|e| export_err(StatusCode::BAD_REQUEST, e))?;
    // ⑥ 裁剪视图（可选；交集语法，视图引用原版本链）
    let bundle = match req.subset.as_deref() {
        Some(spec) => apply_trim_spec(&bundle, spec, "evorule-server", &exported_at)
            .map_err(|e| export_err(StatusCode::BAD_REQUEST, e))?,
        None => bundle,
    };
    Ok(Json(bundle))
}

// 测试豁免 C5（unwrap/expect/panic）与 L2 clippy
#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use evorule_bundle::{
        BundleAudit, BundleDatasetMeta, BundleEntry, BundleTests, DataDependencies, DatasetBundle,
        EntryKind, LawRef, Provenance, ServiceDecl, SourceBinding, TestVerdict, VersionSelection,
        VersionSelectionMode, Versioning,
    };

    use super::*;

    /// 构造可导入的快照包（自动签名哈希）。auto 模式需 law_ref.effective_from 基准。
    fn valid_bundle(rule_body: Value) -> DatasetBundle {
        let mut b = DatasetBundle {
            bundle_schema_version: "1.0".into(),
            bundle_id: "bundle-ds-tax-2024-v1".into(),
            dataset: BundleDatasetMeta {
                dataset_id: "ds-tax-2024".into(),
                name: "2024 企业所得税合规规则集".into(),
                tenant_id: "org-evorule".into(),
                instance_id: "org-evorule".into(),
                versioning: Versioning::default(),
                version_selection: Some(VersionSelection {
                    mode: VersionSelectionMode::AutoByEffectiveDate,
                    pinned_version: None,
                    pinned_include_patch: None,
                }),
                law_ref: Some(LawRef {
                    document_id: "gov-tax-2023-001".into(),
                    law_version: None,
                    effective_from: Some("2024-01-01".into()),
                    effective_to: None,
                }),
                view_of: None,
                event_schemas: vec![],
            },
            entries: vec![BundleEntry {
                entry_id: "entry-tax-001".into(),
                entry_kind: EntryKind::Rule,
                rule_body,
                schema_ref: None,
                provenance: Provenance {
                    source: "《企业所得税法》".into(),
                    clause: None,
                    document_id: None,
                    effective_from: None,
                    effective_to: None,
                    last_verified: None,
                    verified_by: None,
                },
                domain: "tax".into(),
                tags: vec![],
                dependencies: vec![SourceBinding {
                    rule_ref: "transform[0]".into(),
                    service_name: "payroll_svc".into(),
                }],
                // 知识资产化 A 批：规则条目四治理字段不携带（None）
                knowledge_kind: None,
                trust_level: None,
                license_ref: None,
                execution_contract: None,
            }],
            data_dependencies: Some(DataDependencies {
                inputs: vec![],
                services: vec![ServiceDecl {
                    service_name: "payroll_svc".into(),
                    version: None,
                    io_contract: None,
                    sensitive: false,
                    description: None,
                    template: None,
                }],
            }),
            recipe_snapshot: None,
            tests: BundleTests {
                // B2: pass 必带可追溯标记(执行域 import 侧校验);
                // 测试意图=合法可导入包,人工背书形态
                subset: vec!["human:test-user".into()],
                fixtures: vec![],
                verdict: TestVerdict::Pass,
            },
            audit: BundleAudit {
                exported_at: "2026-08-24T12:00:00Z".into(),
                exported_by: "publisher-01".into(),
                source_version: "v1".into(),
                content_hash: String::new(),
                hash_algo: "blake3".into(),
            },
        };
        let hash = b.compute_content_hash();
        b.audit.content_hash = hash;
        b
    }

    fn schema_valid_body() -> Value {
        serde_json::json!({
            "transform": [
                { "type": "io_request", "params": { "io_type": "call_service", "service_name": "payroll_svc" } }
            ]
        })
    }

    /// 复用 valid_bundle 但替换 bundle_id / dataset_id（改后重签哈希）
    fn re_id_bundle(mut b: DatasetBundle, bundle_id: &str, dataset_id: &str) -> DatasetBundle {
        b.bundle_id = bundle_id.into();
        b.dataset.dataset_id = dataset_id.into();
        let hash = b.compute_content_hash();
        b.audit.content_hash = hash;
        b
    }

    /// 构造测试用 SessionApi（临时 core_eval.json + 临时 rules_dir）
    fn test_api(tmp: &tempfile::TempDir) -> SessionApi {
        let rules_dir = tmp.path().join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        let core_eval_path = tmp.path().join("core_eval.json");
        std::fs::write(
            &core_eval_path,
            // 修复(2026-09-01): SessionApi 启动 fail-fast 校验要求宪法
            // 必含 call_external 指令规则（LLM 审计桥平台契约），fixture 同步补入
            r#"{"transform":[
                {"type":"set","params":{"attr":"result","operation":"set","value":"ok"}},
                {"type":"branch","params":{"domain":{"type":"instruction","instruction_type":"call_external"},"on_true":[],"on_false":[]}}
            ]}"#,
        )
        .unwrap();
        SessionApi::new_with_full_config(
            Vec::new(),
            0,
            None,
            false,
            0,
            false,
            0,
            0,
            core_eval_path,
            rules_dir,
            evorule_governance::session::DEFAULT_IO_WARN_TIMEOUT.as_secs(),
            evorule_governance::session::DEFAULT_IO_ERROR_TIMEOUT.as_secs(),
        )
        .with_bound_services(["payroll_svc".to_string()])
    }

    #[tokio::test]
    async fn dry_run_validates_without_landing() {
        let tmp = tempfile::tempdir().unwrap();
        let api = test_api(&tmp);
        let bundle = valid_bundle(schema_valid_body());

        let r = api.import_bundle(&bundle, true).await.unwrap();
        assert_eq!(r.bundle_id, "bundle-ds-tax-2024-v1");
        assert_eq!(r.entry_count, 1);
        // 预检不落盘
        assert!(!tmp.path().join("rules/bundles").exists());
    }

    #[tokio::test]
    async fn import_lands_atomically_and_reloads() {
        let tmp = tempfile::tempdir().unwrap();
        let api = test_api(&tmp);
        let bundle = valid_bundle(schema_valid_body());

        let r = api.import_bundle(&bundle, false).await.unwrap();
        assert_eq!(r.entry_count, 1);
        // 落盘：rules/bundles/{bundle_id}/{entry_id}.json（rule_body 原样零转译）
        let entry_path = tmp
            .path()
            .join("rules/bundles/bundle-ds-tax-2024-v1/entry-tax-001.json");
        assert!(entry_path.is_file(), "条目未落盘: {}", entry_path.display());
        let doc: Value =
            serde_json::from_str(&std::fs::read_to_string(&entry_path).unwrap()).unwrap();
        assert_eq!(doc["transform"][0]["params"]["service_name"], "payroll_svc");
        // T3: manifest 落盘且字段正确
        let manifest_path = tmp
            .path()
            .join("rules/bundles/bundle-ds-tax-2024-v1/bundle_manifest.json");
        assert!(
            manifest_path.is_file(),
            "manifest 未落盘: {}",
            manifest_path.display()
        );
        let manifest: Value =
            serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
        assert_eq!(manifest["bundle_id"], "bundle-ds-tax-2024-v1");
        assert_eq!(manifest["dataset_id"], "ds-tax-2024");
        assert_eq!(manifest["source_version"], "v1");
        assert_eq!(manifest["selection_mode"], "auto_by_effective_date");
        assert_eq!(manifest["effective_from"], "2024-01-01");
        assert!(
            manifest["content_hash"]
                .as_str()
                .unwrap()
                .starts_with("blake3:"),
            "content_hash 应为 blake3: 前缀"
        );
        assert_eq!(manifest["entry_files"][0]["entry_id"], "entry-tax-001");
        assert_eq!(manifest["entry_files"][0]["file"], "entry-tax-001.json");
        // T3: loader 递归扫描 → reload 后 bundle 规则被实际加载（core_eval 2 + bundle 条目 1）
        assert_eq!(api.core_eval_len(), 3, "bundle 条目应随 reload 被加载");
        // 无半成品：无残留临时/备份目录
        assert!(!tmp
            .path()
            .join("rules/bundles/.bundle-ds-tax-2024-v1.tmp")
            .exists());
        assert!(!tmp
            .path()
            .join("rules/bundles/.bundle-ds-tax-2024-v1.bak")
            .exists());
        // reload 已触发（core_eval 非空）
        assert!(api.core_eval_len() >= 1);
    }

    // ============ 批次F: bundle 条目 reload blake3 复验 ============

    /// 批次F: 导入落盘 manifest 记录条目文件哈希;落盘内容被篡改 →
    /// reload 时该条目 fail-fast 拒载（其余规则不受影响,ERROR 不静默）。
    #[tokio::test]
    async fn bundle_entry_tamper_rejected_on_reload() {
        let tmp = tempfile::tempdir().unwrap();
        let api = test_api(&tmp);
        let bundle = valid_bundle(schema_valid_body());

        api.import_bundle(&bundle, false).await.unwrap();
        assert_eq!(
            api.core_eval_len(),
            3,
            "导入后 bundle 条目应被加载（宪法 2 + 条目 1）"
        );

        // 落盘 manifest 记录条目哈希（blake3:hex over 文件字节,SSOT 口径）
        let bundle_dir = tmp.path().join("rules/bundles/bundle-ds-tax-2024-v1");
        let entry_path = bundle_dir.join("entry-tax-001.json");
        let manifest: evorule_workspace::bundle_land::BundleManifest = serde_json::from_str(
            &std::fs::read_to_string(bundle_dir.join("bundle_manifest.json")).unwrap(),
        )
        .unwrap();
        let recorded = manifest.entry_files[0]
            .content_hash
            .as_deref()
            .expect("新导入 manifest 必须记录条目哈希");
        assert_eq!(
            recorded,
            evorule_workspace::bundle_land::entry_file_hash(&std::fs::read(&entry_path).unwrap()),
            "记录哈希必须等于落盘文件哈希"
        );

        // 篡改落盘条目（保持 Schema 合法,内容变化）→ reload 复验失配 → 拒载该条
        let mut tampered: Value =
            serde_json::from_str(&std::fs::read_to_string(&entry_path).unwrap()).unwrap();
        tampered["transform"][0]["params"]["service_name"] = Value::from("tampered_svc");
        std::fs::write(
            &entry_path,
            serde_json::to_string_pretty(&tampered).unwrap(),
        )
        .unwrap();
        api.reload_from_disk().await.unwrap();
        assert_eq!(
            api.core_eval_len(),
            2,
            "被篡改条目必须拒载（core_eval 2 = 仅宪法规则）"
        );
    }

    /// 批次F: 旧格式 manifest（条目无 content_hash 字段）→ 条目照常加载
    /// （防护不追溯存量,零迁移）。
    #[tokio::test]
    async fn bundle_legacy_manifest_without_entry_hashes_still_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let rules_dir = tmp.path().join("rules");
        let bdir = rules_dir.join("bundles").join("b-legacy");
        std::fs::create_dir_all(&bdir).unwrap();
        std::fs::write(
            bdir.join("e1.json"),
            r#"{"transform":[{"type":"io_request","params":{"io_type":"call_service","service_name":"payroll_svc"}}]}"#,
        )
        .unwrap();
        std::fs::write(
            bdir.join("bundle_manifest.json"),
            r#"{"bundle_id":"b-legacy","dataset_id":"ds-legacy","source_version":"v1",
                "selection_mode":"pinned","content_hash":"blake3:legacy",
                "entry_files":[{"entry_id":"e1","file":"e1.json"}]}"#,
        )
        .unwrap();
        let core_eval_path = tmp.path().join("core_eval.json");
        std::fs::write(
            &core_eval_path,
            r#"{"transform":[
                {"type":"set","params":{"attr":"result","operation":"set","value":"ok"}},
                {"type":"branch","params":{"domain":{"type":"instruction","instruction_type":"call_external"},"on_true":[],"on_false":[]}}
            ]}"#,
        )
        .unwrap();
        let merged = SessionApi::load_merged_transforms_from_fs(&core_eval_path, &rules_dir)
            .expect("legacy manifest 条目应照常加载");
        assert_eq!(merged.len(), 3, "宪法 2 + 旧 manifest 条目 1（不追溯拒载）");
    }

    /// 批次F: manifest 在但不可解析 → 无法证明条目未篡改 →
    /// fail-closed 拒载该目录全部条目（ERROR 留痕,不静默）。
    #[tokio::test]
    async fn bundle_manifest_illegal_refuses_whole_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let rules_dir = tmp.path().join("rules");
        let bdir = rules_dir.join("bundles").join("b-bad");
        std::fs::create_dir_all(&bdir).unwrap();
        std::fs::write(
            bdir.join("e1.json"),
            r#"{"transform":[{"type":"io_request","params":{"io_type":"call_service","service_name":"payroll_svc"}}]}"#,
        )
        .unwrap();
        std::fs::write(bdir.join("bundle_manifest.json"), "{not json").unwrap();
        let core_eval_path = tmp.path().join("core_eval.json");
        std::fs::write(
            &core_eval_path,
            r#"{"transform":[
                {"type":"set","params":{"attr":"result","operation":"set","value":"ok"}},
                {"type":"branch","params":{"domain":{"type":"instruction","instruction_type":"call_external"},"on_true":[],"on_false":[]}}
            ]}"#,
        )
        .unwrap();
        let merged = SessionApi::load_merged_transforms_from_fs(&core_eval_path, &rules_dir)
            .expect("复验拒载不构成装载失败（其余规则照常）");
        assert_eq!(
            merged.len(),
            2,
            "manifest 非法 → 该目录条目 fail-closed 拒载"
        );
    }

    #[tokio::test]
    async fn import_records_bundle_import_trace() {
        let tmp = tempfile::tempdir().unwrap();
        let ws_db = Arc::new(evorule_workspace::WorkspaceDb::in_memory().unwrap());
        let api = test_api(&tmp).with_workspace_db(ws_db);
        let bundle = valid_bundle(schema_valid_body());

        let r = api.import_bundle(&bundle, false).await.unwrap();
        assert_eq!(r.entry_count, 1);

        // T5: bundle 导入溯源已写入（bundle_imports 表, 管理元数据墙钟旁路）
        let records = api.list_bundle_imports(10).unwrap();
        assert_eq!(records.len(), 1);
        let rec = &records[0];
        assert_eq!(rec.bundle_id, "bundle-ds-tax-2024-v1");
        assert_eq!(rec.dataset_id, "ds-tax-2024");
        assert_eq!(rec.source_version, "v1");
        assert_eq!(rec.selection_mode, "auto_by_effective_date");
        assert_eq!(rec.resolved_version, None);
        assert!(
            rec.content_hash.starts_with("blake3:"),
            "content_hash 应为 blake3: 前缀"
        );
        assert_eq!(rec.entry_count, 1);
        // 溯源主体沿用治理侧导出者 exported_by（发布链发布者）
        assert_eq!(rec.imported_by, "publisher-01");
        // imported_at 为墙钟管理元数据（rfc3339 非空）
        assert!(!rec.imported_at.to_rfc3339().is_empty());
    }

    #[tokio::test]
    async fn dry_run_does_not_record_import_trace() {
        let tmp = tempfile::tempdir().unwrap();
        let ws_db = Arc::new(evorule_workspace::WorkspaceDb::in_memory().unwrap());
        let api = test_api(&tmp).with_workspace_db(ws_db);
        let bundle = valid_bundle(schema_valid_body());

        // 预检不落盘 → 不应写入溯源
        api.import_bundle(&bundle, true).await.unwrap();
        assert!(api.list_bundle_imports(10).unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_imports_empty_without_workspace_db() {
        // workspace 元数据库未接线 → 空列表（非错误, 未启用溯源）
        let tmp = tempfile::tempdir().unwrap();
        let api = test_api(&tmp);
        assert!(api.list_bundle_imports(10).unwrap().is_empty());
    }

    #[tokio::test]
    async fn tampered_bundle_rejected_no_landing() {
        let tmp = tempfile::tempdir().unwrap();
        let api = test_api(&tmp);
        let mut bundle = valid_bundle(schema_valid_body());
        // 篡改条目内容但不重签名 → 防篡改哈希校验失败
        bundle.entries[0].rule_body = serde_json::json!({ "transform": [{ "type": "io_request", "params": { "io_type": "call_service", "service_name": "hacked" } }] });

        let err = api.import_bundle(&bundle, false).await.unwrap_err();
        assert!(err.contains("校验失败"), "错误信息应显式: {err}");
        assert!(!tmp
            .path()
            .join("rules/bundles/bundle-ds-tax-2024-v1")
            .exists());
    }

    #[tokio::test]
    async fn schema_gate_rejects_invalid_rule_body() {
        let tmp = tempfile::tempdir().unwrap();
        let api = test_api(&tmp);
        // 符号三方一致需通过（合法 io_request 引用 payroll_svc），但第二个元指令
        // 类型不在 6 元指令枚举内 → 硬失败。Q12/审计⑤ SSOT 收口后拦截点前移：
        // BundleImporter::validate 内的 validate_rule_structure（与治理侧入库门禁同源）
        // 在第 ① 步即拒绝，server 侧第 ② 步 Schema 门禁仍保留（双层防御）。
        let mut bundle = valid_bundle(serde_json::json!({
            "transform": [
                { "type": "io_request", "params": { "io_type": "call_service", "service_name": "payroll_svc" } },
                { "type": "no_such_op", "params": {} }
            ]
        }));
        let hash = bundle.compute_content_hash();
        bundle.audit.content_hash = hash;

        let err = api.import_bundle(&bundle, false).await.unwrap_err();
        assert!(
            err.contains("不是元指令"),
            "应为元指令白名单硬失败（SSOT 第①步）: {err}"
        );
        assert!(!tmp
            .path()
            .join("rules/bundles/bundle-ds-tax-2024-v1")
            .exists());
    }

    #[tokio::test]
    async fn path_traversal_bundle_id_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let api = test_api(&tmp);
        let mut bundle = valid_bundle(schema_valid_body());
        bundle.bundle_id = "../evil".into();
        let hash = bundle.compute_content_hash();
        bundle.audit.content_hash = hash;

        let err = api.import_bundle(&bundle, false).await.unwrap_err();
        assert!(err.contains("路径穿越"), "应拒绝路径穿越: {err}");
        assert!(!Path::new(&tmp.path().join("evil")).exists());
    }

    #[tokio::test]
    async fn unbound_service_declared_rejected_explicitly() {
        // T6 阻断项 ①：bundle 声明执行侧未绑定的服务 → import 显式失败（不静默）
        let tmp = tempfile::tempdir().unwrap();
        let api = test_api(&tmp); // 默认绑定 = 原生 7 + payroll_svc
        let mut bundle = valid_bundle(schema_valid_body());
        // 三处符号保持一致（过 ① 步符号校验），但服务名不在执行侧绑定集内
        let svc = "not_bound_svc".to_string();
        bundle.data_dependencies.as_mut().unwrap().services[0].service_name = svc.clone();
        bundle.entries[0].dependencies[0].service_name = svc.clone();
        bundle.entries[0].rule_body = serde_json::json!({
            "transform": [
                { "type": "io_request", "params": { "io_type": "call_service", "service_name": svc } }
            ]
        });
        let hash = bundle.compute_content_hash();
        bundle.audit.content_hash = hash;

        let err = api.import_bundle(&bundle, false).await.unwrap_err();
        assert!(err.contains("未绑定"), "应为服务绑定核对显式失败: {err}");
        assert!(!tmp
            .path()
            .join("rules/bundles/bundle-ds-tax-2024-v1")
            .exists());
    }

    #[tokio::test]
    async fn sensitive_service_without_registry_binding_rejected() {
        // C6（02 方案层 3）：声明 sensitive=true 的服务必须注册表显式绑定
        // （service_registry.json，端点/凭据配置位），仅原生内嵌不满足。
        // 测试 api 未注入 registry_services → 显式失败（不静默）。
        let tmp = tempfile::tempdir().unwrap();
        let api = test_api(&tmp); // bound_services = 原生7 + payroll_svc；registry_services 空
        let mut bundle = valid_bundle(schema_valid_body());
        bundle.data_dependencies.as_mut().unwrap().services[0].sensitive = true;
        let hash = bundle.compute_content_hash();
        bundle.audit.content_hash = hash;

        let err = api.import_bundle(&bundle, false).await.unwrap_err();
        assert!(err.contains("sensitive"), "应为敏感服务核对显式失败: {err}");
        assert!(!tmp
            .path()
            .join("rules/bundles/bundle-ds-tax-2024-v1")
            .exists());
    }

    #[tokio::test]
    async fn sensitive_service_registry_bound_passes() {
        // C6 正例：sensitive 服务已在 service_registry 显式绑定 → 导入通过
        let tmp = tempfile::tempdir().unwrap();
        let metas = vec![evorule_io_handlers::ServiceMeta {
            name: "payroll_svc".into(),
            version: Some("1.2.0".into()),
            description: Some("payroll service".into()),
        }];
        let api = test_api(&tmp).with_registry_services(metas);
        let mut bundle = valid_bundle(schema_valid_body());
        bundle.data_dependencies.as_mut().unwrap().services[0].sensitive = true;
        let hash = bundle.compute_content_hash();
        bundle.audit.content_hash = hash;

        let r = api.import_bundle(&bundle, false).await.unwrap();
        assert_eq!(r.entry_count, 1);
        assert!(
            tmp.path()
                .join("rules/bundles/bundle-ds-tax-2024-v1")
                .exists(),
            "sensitive 服务已注册表绑定应可导入"
        );
    }

    #[tokio::test]
    async fn list_services_reports_native_plus_registry() {
        // C5：/api/services 能力对账 —— 原生叶子能力 + registry（带 version/description）
        let tmp = tempfile::tempdir().unwrap();
        let metas = vec![evorule_io_handlers::ServiceMeta {
            name: "payroll_svc".into(),
            version: Some("1.2.0".into()),
            description: Some("payroll service".into()),
        }];
        let api = test_api(&tmp).with_registry_services(metas);
        let out = crate::api::server::list_services_handler(axum::extract::State(api)).await;

        let mut natives: Vec<&str> = out
            .iter()
            .filter(|b| b.source == "native")
            .map(|b| b.name.as_str())
            .collect();
        natives.sort_unstable();
        let mut expected: Vec<&str> =
            evorule_demo_services::DemoServiceRouter::native_service_names().to_vec();
        expected.sort_unstable();
        assert_eq!(natives, expected, "原生叶子能力应全部上报且 version=1.0.0");
        assert!(out
            .iter()
            .all(|b| b.source == "native" || b.source == "registry"));

        let payroll = out
            .iter()
            .find(|b| b.name == "payroll_svc")
            .expect("registry 服务应上报");
        assert_eq!(payroll.source, "registry");
        assert_eq!(payroll.version.as_deref(), Some("1.2.0"));
        assert_eq!(payroll.description.as_deref(), Some("payroll service"));
    }

    /// 测试用服务链：echo 服务名（验证 invoke 直调复用链的接线语义，不依赖真实插件）
    #[derive(Default)]
    struct TestEchoChain;

    #[async_trait::async_trait]
    impl evorule_reactor::IoHandler for TestEchoChain {
        async fn execute(&self, _params: &evorule_tcb::JsonValue) -> evorule_reactor::IoResult {
            Ok(evorule_tcb::JsonValue::String("echo".into()))
        }
    }

    #[tokio::test]
    async fn list_services_native_injection_overrides_and_attaches_plugin() {
        // 对账泛化：with_native_services 替换 demo 兜底清单，逐服务携带 plugin
        // 归属/描述/敏感标记；registry 条目 plugin=None sensitive=false
        let tmp = tempfile::tempdir().unwrap();
        let infos = vec![
            crate::api::server::BoundServiceInfo {
                name: "physics_simulate".into(),
                source: "native".into(),
                version: Some("1.0.0".into()),
                description: Some("确定性物理仿真推进".into()),
                plugin: Some("physics-services".into()),
                sensitive: false,
                parameters: None,
            },
            crate::api::server::BoundServiceInfo {
                name: "finance_config_set".into(),
                source: "native".into(),
                version: Some("1.0.0".into()),
                description: Some("财务配置键写入".into()),
                plugin: Some("finance-config".into()),
                sensitive: true,
                parameters: None,
            },
        ];
        let metas = vec![evorule_io_handlers::ServiceMeta {
            name: "payroll_svc".into(),
            version: None,
            description: None,
        }];
        let api = test_api(&tmp)
            .with_native_services(infos)
            .with_registry_services(metas);
        let out = crate::api::server::list_services_handler(axum::extract::State(api)).await;

        let natives: Vec<&crate::api::server::BoundServiceInfo> =
            out.iter().filter(|b| b.source == "native").collect();
        assert_eq!(natives.len(), 2, "注入清单应整体替换 demo 兜底");
        assert!(!natives
            .iter()
            .any(|b| b.name == "inverse_kinematics_solver"));
        let physics = out.iter().find(|b| b.name == "physics_simulate").unwrap();
        assert_eq!(physics.plugin.as_deref(), Some("physics-services"));
        assert_eq!(physics.description.as_deref(), Some("确定性物理仿真推进"));
        assert!(!physics.sensitive);
        let finance = out.iter().find(|b| b.name == "finance_config_set").unwrap();
        assert_eq!(finance.plugin.as_deref(), Some("finance-config"));
        assert!(finance.sensitive, "声明表 sensitive 标记应透传对账清单");
        let payroll = out.iter().find(|b| b.name == "payroll_svc").unwrap();
        assert!(payroll.plugin.is_none() && !payroll.sensitive);
    }

    #[tokio::test]
    async fn invoke_service_executes_via_chain_with_guards() {
        // invoke 直调守卫语义：非敏感 200 走链执行；敏感 403；未知 404；无链 503
        let tmp = tempfile::tempdir().unwrap();
        let infos = vec![
            crate::api::server::BoundServiceInfo {
                name: "physics_simulate".into(),
                source: "native".into(),
                version: None,
                description: None,
                plugin: Some("physics-services".into()),
                sensitive: false,
                parameters: None,
            },
            crate::api::server::BoundServiceInfo {
                name: "finance_config_set".into(),
                source: "native".into(),
                version: None,
                description: None,
                plugin: Some("finance-config".into()),
                sensitive: true,
                parameters: None,
            },
        ];
        let api = test_api(&tmp)
            .with_native_services(infos)
            .with_service_chain(std::sync::Arc::new(TestEchoChain));

        // ① 非敏感服务 → 复用链执行，200 返回链结果
        let ok = crate::api::server::invoke_service_handler(
            axum::extract::State(api.clone()),
            axum::extract::Path("physics_simulate".to_string()),
            None,
            axum::Json(serde_json::json!({ "steps": 10 })),
        )
        .await
        .unwrap();
        assert_eq!(ok.0, serde_json::json!("echo"));

        // ② 敏感服务 → 403（直调=静默绕审批，禁止）
        let (status, body) = crate::api::server::invoke_service_handler(
            axum::extract::State(api.clone()),
            axum::extract::Path("finance_config_set".to_string()),
            None,
            axum::Json(serde_json::json!({})),
        )
        .await
        .unwrap_err();
        assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
        assert!(
            body.0.to_string().contains("禁止"),
            "403 文案应指明直调禁止"
        );

        // ③ 未知服务 → 404（附合法名指引）
        let (status, body) = crate::api::server::invoke_service_handler(
            axum::extract::State(api.clone()),
            axum::extract::Path("no_such_service".to_string()),
            None,
            axum::Json(serde_json::json!({})),
        )
        .await
        .unwrap_err();
        assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
        assert!(body.0.to_string().contains("unknown service"));

        // ④ 未装配服务链 → 503（服务名须在 bare api 的 demo 兜底清单内）
        let bare = test_api(&tmp);
        let (status, _) = crate::api::server::invoke_service_handler(
            axum::extract::State(bare),
            axum::extract::Path("config_persist".to_string()),
            None,
            axum::Json(serde_json::json!({})),
        )
        .await
        .unwrap_err();
        assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn single_activation_replaces_old_bundle_by_dataset() {
        let tmp = tempfile::tempdir().unwrap();
        let api = test_api(&tmp);
        // v1 导入
        api.import_bundle(&valid_bundle(schema_valid_body()), false)
            .await
            .unwrap();
        // 同 dataset 的 v2（不同 bundle_id）导入 → 单激活替换：v1 应被清理
        let v2 = re_id_bundle(
            valid_bundle(schema_valid_body()),
            "bundle-ds-tax-2024-v2",
            "ds-tax-2024",
        );
        api.import_bundle(&v2, false).await.unwrap();

        assert!(
            !tmp.path()
                .join("rules/bundles/bundle-ds-tax-2024-v1")
                .exists(),
            "同 dataset 旧 bundle 应被替换（单激活）"
        );
        assert!(
            tmp.path()
                .join("rules/bundles/bundle-ds-tax-2024-v2")
                .exists(),
            "新版本应就位"
        );
        // active 只报最新激活
        let active = api.active_bundles().unwrap();
        assert_eq!(active.len(), 1, "单激活：同 dataset 仅一个激活");
        assert_eq!(active[0].bundle_id, "bundle-ds-tax-2024-v2");
        // loader 只加载 v2 条目（core_eval 2 + v2 条目 1 = 3，v1 不残留）
        assert_eq!(api.core_eval_len(), 3, "旧 bundle 规则不应被加载");
    }

    #[tokio::test]
    async fn active_bundles_reports_imported_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let api = test_api(&tmp);
        api.import_bundle(&valid_bundle(schema_valid_body()), false)
            .await
            .unwrap();

        let active = api.active_bundles().unwrap();
        assert_eq!(active.len(), 1);
        let m = &active[0];
        assert_eq!(m.bundle_id, "bundle-ds-tax-2024-v1");
        assert_eq!(m.dataset_id, "ds-tax-2024");
        assert_eq!(m.source_version, "v1");
        assert_eq!(m.selection_mode, VersionSelectionMode::AutoByEffectiveDate);
        assert_eq!(m.effective_from.as_deref(), Some("2024-01-01"));
        assert!(
            m.content_hash.starts_with("blake3:"),
            "content_hash 应为 blake3: 前缀"
        );
        assert_eq!(m.entry_files.len(), 1);

        // 空目录 → 空列表（非错误）
        let empty_tmp = tempfile::tempdir().unwrap();
        let empty_api = test_api(&empty_tmp);
        assert!(empty_api.active_bundles().unwrap().is_empty());
    }

    #[tokio::test]
    async fn multiple_datasets_coexist_in_active() {
        let tmp = tempfile::tempdir().unwrap();
        let api = test_api(&tmp);
        api.import_bundle(&valid_bundle(schema_valid_body()), false)
            .await
            .unwrap();
        // b2 使用自有 io_type：同 io_type 跨 dataset 并存会被导入前置重叠预判显式拒
        // （发射/消费权利首声明独占——并存两 bundle 只允许首声明者持有同一 io_type）
        let med_body = serde_json::json!({
            "transform": [
                { "type": "io_request", "params": { "io_type": "med_probe", "service_name": "payroll_svc" } }
            ]
        });
        let b2 = re_id_bundle(
            valid_bundle(med_body),
            "bundle-ds-med-2024-v1",
            "ds-med-2024",
        );
        api.import_bundle(&b2, false).await.unwrap();

        let active = api.active_bundles().unwrap();
        assert_eq!(active.len(), 2, "不同 dataset 应并存");
        // 按 dataset_id 字典序稳定排序
        assert_eq!(active[0].dataset_id, "ds-med-2024");
        assert_eq!(active[1].dataset_id, "ds-tax-2024");
        assert!(tmp
            .path()
            .join("rules/bundles/bundle-ds-tax-2024-v1")
            .exists());
        assert!(tmp
            .path()
            .join("rules/bundles/bundle-ds-med-2024-v1")
            .exists());
    }

    // ============ I/O 权利面重叠导入前置预判 ============

    /// 部署面平台规则文件（rules_dir 直落，发射 call_service）
    fn platform_emit_file_body() -> Value {
        serde_json::json!({
            "transform": [
                { "type": "io_request", "params": { "io_type": "call_service", "service_name": "payroll_svc" } }
            ]
        })
    }

    /// 与装载集权利重叠的 bundle → 落盘前显式拒（明细归因到规则对，无半激活落盘）
    #[tokio::test]
    async fn import_overlap_with_loaded_rules_fails_fast() {
        let tmp = tempfile::tempdir().unwrap();
        let api = test_api(&tmp);
        // 部署面平台规则发射 call_service → reload 后进入装载集基线
        std::fs::write(
            tmp.path().join("rules/platform_io.json"),
            serde_json::to_string_pretty(&platform_emit_file_body()).unwrap(),
        )
        .unwrap();
        api.reload_from_disk().await.unwrap();
        assert_eq!(api.core_eval_len(), 3, "基线=宪法 2 + 平台规则 1");

        let err = api
            .import_bundle(&valid_bundle(schema_valid_body()), false)
            .await
            .unwrap_err();
        assert!(err.contains("I/O 权利面重叠"), "应为重叠前置拒绝: {err}");
        assert!(
            err.contains("platform_io.json"),
            "明细应归因到已装载规则: {err}"
        );
        assert!(err.contains("emit call_service"), "明细应含重叠面: {err}");
        assert!(
            !tmp.path()
                .join("rules/bundles/bundle-ds-tax-2024-v1")
                .exists(),
            "被拒 bundle 不得落盘（fail-fast，无半激活）"
        );
    }

    /// bundle 自有 io_type（flow_probe）不触平台保留权利面 → 正常导入且零拒载
    #[tokio::test]
    async fn import_own_io_type_complies() {
        let tmp = tempfile::tempdir().unwrap();
        let api = test_api(&tmp);
        let body = serde_json::json!({
            "transform": [
                { "type": "io_request", "params": { "io_type": "flow_probe", "service_name": "payroll_svc" } }
            ]
        });
        let outcome = api.import_bundle(&valid_bundle(body), false).await.unwrap();
        assert_eq!(outcome.entry_count, 1);
        assert!(
            outcome.rejected_overlaps.is_empty(),
            "合规导入 reload 期应零拒载"
        );
        assert!(tmp
            .path()
            .join("rules/bundles/bundle-ds-tax-2024-v1/entry-tax-001.json")
            .is_file());
    }

    /// 不同 dataset 的第二 bundle 声明同一发射权 → 并存即权利冲突 → 显式拒
    #[tokio::test]
    async fn import_second_bundle_same_io_type_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let api = test_api(&tmp);
        api.import_bundle(&valid_bundle(schema_valid_body()), false)
            .await
            .unwrap();
        let b2 = re_id_bundle(
            valid_bundle(schema_valid_body()),
            "bundle-ds-med-2024-v1",
            "ds-med-2024",
        );
        let err = api.import_bundle(&b2, false).await.unwrap_err();
        assert!(err.contains("I/O 权利面重叠"), "应为重叠前置拒绝: {err}");
        assert!(
            !tmp.path()
                .join("rules/bundles/bundle-ds-med-2024-v1")
                .exists(),
            "被拒 bundle 不得落盘"
        );
    }

    /// 替换形态不误伤：同 bundle_id 重导入 / 同 dataset 单激活换版，旧条目
    /// 权利声明将被本次原子落盘替换，不构成基线冲突
    #[tokio::test]
    async fn reimport_replacement_bundles_not_flagged_as_overlap() {
        let tmp = tempfile::tempdir().unwrap();
        let api = test_api(&tmp);
        api.import_bundle(&valid_bundle(schema_valid_body()), false)
            .await
            .unwrap();
        // 同 bundle_id 重导入（原子替换旧条目）
        api.import_bundle(&valid_bundle(schema_valid_body()), false)
            .await
            .unwrap();
        // 同 dataset 换版（v2 替换 v1，单激活）
        let v2 = re_id_bundle(
            valid_bundle(schema_valid_body()),
            "bundle-ds-tax-2024-v2",
            "ds-tax-2024",
        );
        api.import_bundle(&v2, false).await.unwrap();
        assert_eq!(api.core_eval_len(), 3, "宪法 2 + v2 条目 1（v1 已替换）");
    }

    /// handler 层两向：重叠 → 400 显式明细；合规自有 io_type → 201 且
    /// rejected_overlaps 为空
    #[tokio::test]
    async fn import_handler_overlaps_map_to_400_and_clean_to_201() {
        let tmp = tempfile::tempdir().unwrap();
        let api = test_api(&tmp);
        std::fs::write(
            tmp.path().join("rules/platform_io.json"),
            serde_json::to_string_pretty(&platform_emit_file_body()).unwrap(),
        )
        .unwrap();
        api.reload_from_disk().await.unwrap();
        let (status, Json(err_json)) = import_bundle_handler(
            axum::extract::State(api),
            Json(ImportReq {
                bundle: valid_bundle(schema_valid_body()),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert!(
            err_json["error"]
                .as_str()
                .unwrap()
                .contains("I/O 权利面重叠"),
            "400 明细应含重叠说明: {err_json}"
        );

        let tmp2 = tempfile::tempdir().unwrap();
        let api2 = test_api(&tmp2);
        let body = serde_json::json!({
            "transform": [
                { "type": "io_request", "params": { "io_type": "flow_probe", "service_name": "payroll_svc" } }
            ]
        });
        let (status, resp) = import_bundle_handler(
            axum::extract::State(api2),
            Json(ImportReq {
                bundle: valid_bundle(body),
            }),
        )
        .await
        .unwrap();
        assert_eq!(status, axum::http::StatusCode::CREATED);
        assert!(
            resp.0.rejected_overlaps.is_empty(),
            "合规导入响应 rejected_overlaps 应为空"
        );
    }

    #[tokio::test]
    async fn corrupted_manifest_errors_are_explicit() {
        let tmp = tempfile::tempdir().unwrap();
        let api = test_api(&tmp);
        api.import_bundle(&valid_bundle(schema_valid_body()), false)
            .await
            .unwrap();
        // 破坏 manifest → active 应显式 Err（不静默掩盖激活状态）
        let manifest_path = tmp
            .path()
            .join("rules/bundles/bundle-ds-tax-2024-v1/bundle_manifest.json");
        std::fs::write(&manifest_path, "{ not json").unwrap();

        let err = api.active_bundles().unwrap_err();
        assert!(err.contains("解析"), "应为显式解析错误: {err}");
    }

    // ===== 导出核（workspace 治理域带证据导出）=====

    use crate::api::bundles::{export_workspace_bundle, WorkspaceRuleInput};

    fn export_rule_input(id: &str, name: &str, content: &str) -> WorkspaceRuleInput {
        WorkspaceRuleInput {
            rule_id: id.to_string(),
            name: name.to_string(),
            content: content.to_string(),
        }
    }

    #[test]
    fn export_core_happy_path_self_consistent() {
        let rules = vec![
            export_rule_input("01JR00000001", "r-one", r#"{"kind":"rule","id":"one"}"#),
            export_rule_input("01JR00000002", "r-two", r#"{"kind":"rule","id":"two"}"#),
        ];
        let tests = BundleTests {
            subset: vec!["sandbox:sb-1".into()],
            fixtures: vec![],
            verdict: TestVerdict::Pass,
        };
        let snapshot = evorule_bundle::RecipeSnapshot {
            recipe_version: "memory-v1.0".into(),
            recipe: serde_json::json!({"focus": "tax"}),
            snapshot_at: "2026-10-08T00:00:00Z".into(),
        };
        let b = export_workspace_bundle(
            "ws-export-1",
            &rules,
            &tests,
            Some(snapshot),
            "2026-10-08T01:00:00Z",
        )
        .unwrap();
        assert_eq!(b.entries.len(), 2);
        assert_eq!(b.entries[0].entry_id, "rule-01JR00000001");
        assert_eq!(b.tests.verdict, TestVerdict::Pass);
        assert!(b.recipe_snapshot.is_some(), "快照应入包");
        assert!(b.bundle_id.starts_with("export-"), "{b:?}",);
        // 全包哈希自洽（含快照参与）
        assert!(
            b.verify_content_hash().is_ok(),
            "导出包哈希应自洽: {:?}",
            b.verify_content_hash()
        );
        // 确定性：同输入同 exported_at → 同 bundle_id 同哈希
        let b2 = export_workspace_bundle(
            "ws-export-1",
            &rules,
            &tests,
            b.recipe_snapshot.clone(),
            "2026-10-08T01:00:00Z",
        )
        .unwrap();
        assert_eq!(b.bundle_id, b2.bundle_id);
        assert_eq!(b.audit.content_hash, b2.audit.content_hash);
    }

    #[test]
    fn export_core_tamper_red() {
        let rules = vec![export_rule_input(
            "01JR00000003",
            "r-tamper",
            r#"{"kind":"rule","id":"t"}"#,
        )];
        let tests = BundleTests::unverified();
        let mut b =
            export_workspace_bundle("ws-t", &rules, &tests, None, "2026-10-08T01:00:00Z").unwrap();
        b.entries[0].rule_body = serde_json::json!({"kind":"rule","id":"tampered"});
        assert!(
            b.verify_content_hash().is_err(),
            "篡改条目后哈希应失配（防篡改红）"
        );
    }

    #[test]
    fn export_core_recipe_snapshot_in_hash_chain() {
        let rules = vec![export_rule_input(
            "01JR00000004",
            "r-snap",
            r#"{"kind":"rule","id":"s"}"#,
        )];
        let tests = BundleTests::unverified();
        let snap = evorule_bundle::RecipeSnapshot {
            recipe_version: "memory-v1.0".into(),
            recipe: serde_json::json!({"k": "v"}),
            snapshot_at: "2026-10-08T00:00:00Z".into(),
        };
        let with_snap =
            export_workspace_bundle("ws-s", &rules, &tests, Some(snap), "2026-10-08T01:00:00Z")
                .unwrap();
        let without_snap =
            export_workspace_bundle("ws-s", &rules, &tests, None, "2026-10-08T01:00:00Z").unwrap();
        assert_ne!(
            with_snap.audit.content_hash, without_snap.audit.content_hash,
            "快照应参与全包哈希（入链）"
        );
        let mut tampered = with_snap.clone();
        if let Some(s) = tampered.recipe_snapshot.as_mut() {
            s.recipe = serde_json::json!({"k": "forged"});
        }
        assert!(
            tampered.verify_content_hash().is_err(),
            "篡改快照后哈希应失配"
        );
    }

    #[test]
    fn export_core_empty_rules_and_bad_json_rejected() {
        let tests = BundleTests::unverified();
        // 空规则集显式拒绝（不静默出空包）
        assert!(export_workspace_bundle("ws-e", &[], &tests, None, "t").is_err());
        // 内容非 JSON 显式拒绝
        let bad = vec![export_rule_input("01JR00000005", "bad", "not-json{")];
        let err = export_workspace_bundle("ws-e", &bad, &tests, None, "t").unwrap_err();
        assert!(err.contains("非法"), "应显式报内容非法: {err}");
    }

    #[test]
    fn export_trim_spec_ids_view_with_view_of() {
        let rules = vec![
            export_rule_input("01JR00000006", "keep", r#"{"kind":"rule","id":"k"}"#),
            export_rule_input("01JR00000007", "drop", r#"{"kind":"rule","id":"d"}"#),
        ];
        let tests = BundleTests::unverified();
        let b = export_workspace_bundle("ws-trim", &rules, &tests, None, "2026-10-08T01:00:00Z")
            .unwrap();
        let keep_id = b.entries[0].entry_id.clone();
        let view = super::apply_trim_spec(
            &b,
            &format!("ids:{keep_id}"),
            "evorule-server",
            "2026-10-08T01:00:00Z",
        )
        .unwrap();
        assert_eq!(view.entries.len(), 1);
        assert_eq!(view.entries[0].entry_id, keep_id);
        assert_eq!(
            view.dataset.view_of.as_ref().unwrap().original_dataset_id,
            "ws-trim",
            "裁剪视图应引用原版本链"
        );
        assert!(view.verify_content_hash().is_ok(), "视图哈希应重算自洽");
        // 非法语法 / 未知段 / 空表达式显式红
        assert!(super::apply_trim_spec(&b, "bogus", "x", "t").is_err());
        assert!(super::apply_trim_spec(&b, "what:id1", "x", "t").is_err());
        assert!(super::apply_trim_spec(&b, "  ", "x", "t").is_err());
        // 空结果（ids 全不命中）由 Trimmer 显式拒
        assert!(super::apply_trim_spec(&b, "ids:nope", "x", "t").is_err());
    }
}

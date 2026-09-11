// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 快照包导入端点（· 36 号 集成契约 / 44 号 bundles）
//!
//! - `POST /api/bundles/import`：6 项硬校验 + 逐条 Schema 门禁 + 原子落盘 + 触发 reload；
//! - `POST /api/bundles/import/dry-run`：只跑校验链，不落盘不 reload。
//!
//! 框架层无 RBAC（D12：审批权威留在治理层 evorule-rule），此处仅要求有效 token（受保护路由）。
//! 校验失败一律 400 显式错误（不静默降级，35 号 §9 / T0）。

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use evorule_bundle::VersionSelectionMode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

use crate::api::server::SessionApi;

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
    /// 硬失败原则：缺失服务已在校验链以显式错误拦截，成功导入即无缺失（35 号 §9）
    pub missing_services: Vec<String>,
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
/// 请求体为治理层 `DatasetBundle` 快照包 JSON（36 号 §2）。6 项校验链任一失败
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
    let result = sessions
        .import_bundle(&req.bundle, false)
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": e, "imported": false })),
            )
        })?;
    Ok((
        StatusCode::CREATED,
        Json(ImportResponse {
            imported: true,
            bundle_id: result.bundle_id,
            dataset_id: result.dataset_id,
            activated_version: result.source_version,
            entry_count: result.entry_count,
            missing_services: Vec::new(),
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
        })?;
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
        let b2 = re_id_bundle(
            valid_bundle(schema_valid_body()),
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
}

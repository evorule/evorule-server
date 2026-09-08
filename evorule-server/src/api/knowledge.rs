// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 执行侧数据面端点（段2 P1 · D1 三端点）
//!
//! - `GET /api/knowledge`：已承载数据资产的数据集清单；
//! - `GET /api/knowledge/{ds}/entries?q=&domain=&tags=`：条目检索（与治理侧同语法，
//!   tags 逗号分隔任一命中）；
//! - `GET /api/knowledge/{ds}/entries/{entry_id}`：单条直取（payload 零转译原样）。
//!
//! 面向 SDK / 原生服务消费（P0 定案：console 治理中心走治理侧 API，不经此数据面）。
//! 权限：沿用受保护路由 token 中间件，无新增 RBAC（D2：执行侧 = 部署信任边界）。
//!
//! # fail-fast 口径
//! - knowledge 库加载失败（磁盘篡改/损坏）→ 数据面 **500 显式错误**，不静默返回空；
//! - 数据集未承载 → 条目端点 404（不静默空列表，区分"不存在"与"为空"）；
//! - 单条目不存在 → 404。错误格式与 bundles.rs 同构（`{"error": ...}`）。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

use crate::api::server::SessionApi;
use crate::knowledge_store::KnowledgeStore;
use crate::knowledge_store::{KnowledgeDatasetSummary, KnowledgeEntryRecord};

/// 数据集清单响应（S2）
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct KnowledgeDatasetsResponse {
    pub datasets: Vec<KnowledgeDatasetSummary>,
    pub count: usize,
}

/// 条目检索响应（S3）
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct KnowledgeEntriesResponse {
    pub dataset_id: String,
    pub entries: Vec<KnowledgeEntryRecord>,
    pub count: usize,
}

/// `?q=&domain=&tags=` 检索参数（S3：与治理侧同语法，tags 逗号分隔）
#[derive(Debug, Deserialize)]
pub struct KnowledgeSearchQuery {
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub domain: Option<String>,
    /// 逗号分隔标签（任一命中）
    #[serde(default)]
    pub tags: Option<String>,
}

/// knowledge 数据面统一错误（加载失败 500 / 未承载 404 / 条目不存在 404）
fn err_json(status: StatusCode, msg: String) -> (StatusCode, Json<Value>) {
    (status, Json(serde_json::json!({ "error": msg })))
}

/// knowledge 库可用性前置检查：加载失败 → 500 显式（数据面异常必须可见，不静默空列表）
fn ensure_knowledge_available(
    sessions: &SessionApi,
) -> Result<std::sync::Arc<KnowledgeStore>, (StatusCode, Json<Value>)> {
    if let Some(e) = sessions.knowledge_load_error() {
        return Err(err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("knowledge 数据资产库加载失败（数据面不可用）: {e}"),
        ));
    }
    Ok(sessions.knowledge_store())
}

/// GET /api/knowledge —— 已承载数据资产的数据集清单（S2）
#[utoipa::path(
    get,
    path = "/api/knowledge",
    tag = "knowledge",
    responses(
        (status = 200, description = "数据集清单（可能为空）", body = KnowledgeDatasetsResponse),
        (status = 401, description = "未认证"),
        (status = 500, description = "knowledge 库加载失败（显式错误，不静默）", body = serde_json::Value)
    )
)]
pub async fn knowledge_datasets_handler(
    State(sessions): State<SessionApi>,
) -> Result<Json<KnowledgeDatasetsResponse>, (StatusCode, Json<Value>)> {
    let store = ensure_knowledge_available(&sessions)?;
    let datasets = store.list_datasets();
    let count = datasets.len();
    Ok(Json(KnowledgeDatasetsResponse { datasets, count }))
}

/// GET /api/knowledge/{ds}/entries —— 数据集条目检索（S3）
#[utoipa::path(
    get,
    path = "/api/knowledge/{ds}/entries",
    tag = "knowledge",
    params(
        ("ds" = String, Path, description = "数据集 ID"),
        ("q" = Option<String>, Query, description = "包含匹配（entry_id/schema_ref/bundle_id/payload）"),
        ("domain" = Option<String>, Query, description = "领域精确匹配（忽略大小写）"),
        ("tags" = Option<String>, Query, description = "逗号分隔标签（任一命中）")
    ),
    responses(
        (status = 200, description = "条目列表（数据集已承载；可能为空）", body = KnowledgeEntriesResponse),
        (status = 404, description = "数据集未承载（显式，不静默空列表）", body = serde_json::Value),
        (status = 401, description = "未认证"),
        (status = 500, description = "knowledge 库加载失败（显式错误，不静默）", body = serde_json::Value)
    )
)]
pub async fn knowledge_entries_handler(
    State(sessions): State<SessionApi>,
    Path(ds): Path<String>,
    Query(q): Query<KnowledgeSearchQuery>,
) -> Result<Json<KnowledgeEntriesResponse>, (StatusCode, Json<Value>)> {
    let store = ensure_knowledge_available(&sessions)?;
    // 数据集未承载 → 404（区分"不存在"与"过滤后为空"；承载即至少 1 条目）
    if store.list_dataset(&ds).is_empty() {
        return Err(err_json(
            StatusCode::NOT_FOUND,
            format!("数据集 `{ds}` 未在执行侧承载（未导入或已被替换）"),
        ));
    }
    let tags: Vec<String> = q
        .tags
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    let entries = store
        .search(Some(&ds), q.q.as_deref(), q.domain.as_deref(), &tags)
        .into_iter()
        .cloned()
        .collect::<Vec<KnowledgeEntryRecord>>();
    let count = entries.len();
    Ok(Json(KnowledgeEntriesResponse {
        dataset_id: ds,
        entries,
        count,
    }))
}

/// GET /api/knowledge/{ds}/entries/{entry_id} —— 单条直取（S4）
#[utoipa::path(
    get,
    path = "/api/knowledge/{ds}/entries/{entry_id}",
    tag = "knowledge",
    params(
        ("ds" = String, Path, description = "数据集 ID"),
        ("entry_id" = String, Path, description = "条目 ID")
    ),
    responses(
        (status = 200, description = "条目记录（payload 零转译原样）", body = KnowledgeEntryRecord),
        (status = 404, description = "数据集或条目不存在", body = serde_json::Value),
        (status = 401, description = "未认证"),
        (status = 500, description = "knowledge 库加载失败（显式错误，不静默）", body = serde_json::Value)
    )
)]
pub async fn knowledge_entry_handler(
    State(sessions): State<SessionApi>,
    Path((ds, entry_id)): Path<(String, String)>,
) -> Result<Json<KnowledgeEntryRecord>, (StatusCode, Json<Value>)> {
    let store = ensure_knowledge_available(&sessions)?;
    store.get(&ds, &entry_id).cloned().map(Json).ok_or_else(|| {
        err_json(
            StatusCode::NOT_FOUND,
            format!("数据条目 `{ds}/{entry_id}` 不存在"),
        )
    })
}

// 测试豁免 C5（unwrap/expect/panic）与 L2 clippy
#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::server::SessionApi;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    /// 构造测试用 SessionApi（临时 core_eval.json + 临时 rules_dir）
    fn test_api(tmp: &tempfile::TempDir) -> SessionApi {
        let rules_dir = tmp.path().join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        let core_eval_path = tmp.path().join("core_eval.json");
        std::fs::write(
            &core_eval_path,
            r#"{"transform":[{"type":"set","params":{"attr":"result","operation":"set","value":"ok"}}]}"#,
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
    }

    /// 构造 knowledge bundle（哈希签名完整，可直接过 import_bundle 校验链）
    fn knowledge_bundle(
        bundle_id: &str,
        dataset_id: &str,
        entry_id: &str,
        schema_ref: &str,
        domain: &str,
        tags: &[&str],
        scenario_id: &str,
    ) -> evorule_bundle::DatasetBundle {
        use evorule_bundle::{
            BundleAudit, BundleDatasetMeta, BundleEntry, BundleTests, DatasetBundle, LawRef,
            Provenance, TestVerdict, VersionSelection, VersionSelectionMode, Versioning,
            BUNDLE_SCHEMA_VERSION,
        };
        let mut bundle = DatasetBundle {
            bundle_schema_version: BUNDLE_SCHEMA_VERSION.to_string(),
            bundle_id: bundle_id.into(),
            dataset: BundleDatasetMeta {
                dataset_id: dataset_id.into(),
                name: "Q12 段2 数据面测试集".into(),
                tenant_id: "org-evorule".into(),
                instance_id: "org-evorule".into(),
                versioning: Versioning::default(),
                version_selection: Some(VersionSelection {
                    mode: VersionSelectionMode::AutoByEffectiveDate,
                    pinned_version: None,
                    pinned_include_patch: None,
                }),
                law_ref: Some(LawRef {
                    document_id: "rpsm-scenarios".into(),
                    law_version: None,
                    effective_from: Some("2026-08-30".into()),
                    effective_to: None,
                }),
                view_of: None,
                event_schemas: vec![],
            },
            entries: vec![BundleEntry {
                entry_id: entry_id.into(),
                entry_kind: evorule_bundle::EntryKind::Knowledge,
                rule_body: serde_json::json!({
                    "scenario_id": scenario_id,
                    "gravity": [0.0, -9.81, 0.0],
                    "restitution": 1.0,
                    "bodies": [{"id": "particle-1"}]
                }),
                schema_ref: Some(schema_ref.into()),
                provenance: Provenance {
                    source: "rpsm 内置场景".into(),
                    clause: None,
                    document_id: None,
                    effective_from: None,
                    effective_to: None,
                    last_verified: None,
                    verified_by: None,
                },
                domain: domain.into(),
                tags: tags.iter().map(|s| s.to_string()).collect(),
                dependencies: vec![],
            }],
            data_dependencies: None,
            tests: BundleTests {
                // B2: pass 必带可追溯标记(执行域 import 侧校验);
                // 测试意图=合法可导入知识包,人工背书形态
                subset: vec!["human:q12-s2-itest".into()],
                fixtures: vec![],
                verdict: TestVerdict::Pass,
            },
            audit: BundleAudit {
                exported_at: "2026-08-30T00:00:00Z".into(),
                exported_by: "q12-s2-itest".into(),
                source_version: "v1".into(),
                content_hash: String::new(),
                hash_algo: "blake3".into(),
            },
        };
        bundle.audit.content_hash = bundle.compute_content_hash();
        bundle
    }

    /// 三端点 mini router（认证中间件在 build_router 全局统一挂载，此处不重复）
    fn knowledge_router(sessions: SessionApi) -> Router {
        Router::new()
            .route("/api/knowledge", get(knowledge_datasets_handler))
            .route(
                "/api/knowledge/{ds}/entries",
                get(knowledge_entries_handler),
            )
            .route(
                "/api/knowledge/{ds}/entries/{entry_id}",
                get(knowledge_entry_handler),
            )
            .with_state(sessions)
    }

    async fn oneshot_json(
        router: Router,
        method: &str,
        uri: &str,
    ) -> (StatusCode, serde_json::Value) {
        let request = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .body(axum::body::Body::empty())
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, body)
    }

    /// S6：三端点 200/404 + 过滤矩阵 + 导入后 refresh 即刻生效
    #[tokio::test]
    async fn knowledge_data_plane_endpoints_matrix() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = test_api(&tmp);

        // 领域 schema 注册（运维注入通道）
        let ddir = tmp.path().join("knowledge").join("domain_schemas");
        std::fs::create_dir_all(&ddir).unwrap();
        std::fs::write(
            ddir.join("scenario.json"),
            r#"{"$id":"https://rpsm.evorule.org/schemas/scenario/v1.0.json","type":"object"}"#,
        )
        .unwrap();

        let app = knowledge_router(sessions.clone());

        // 空库 → 200 空清单
        let (status, body) = oneshot_json(app.clone(), "GET", "/api/knowledge").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["count"], 0, "{body}");

        // 导入两个数据集各 1 条（domain/tags 区分），refresh 随导入自动触发
        sessions
            .import_bundle(
                &knowledge_bundle(
                    "bundle-ds-a-v1",
                    "ds-a",
                    "scn-001",
                    "https://rpsm.evorule.org/schemas/scenario/v1.0.json",
                    "physics",
                    &["spring", "demo"],
                    "spring-single-particle",
                ),
                false,
            )
            .await
            .expect("ds-a 导入应通过");
        sessions
            .import_bundle(
                &knowledge_bundle(
                    "bundle-ds-b-v1",
                    "ds-b",
                    "scn-002",
                    "https://rpsm.evorule.org/schemas/scenario/v1.0.json",
                    "chemistry",
                    &["demo"],
                    "chem-mix",
                ),
                false,
            )
            .await
            .expect("ds-b 导入应通过");

        // S2：数据集清单（导入后即刻生效，无需重启）
        let (status, body) = oneshot_json(app.clone(), "GET", "/api/knowledge").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["count"], 2, "{body}");
        assert_eq!(body["datasets"][0]["dataset_id"], "ds-a", "{body}");
        assert_eq!(body["datasets"][0]["entry_count"], 1, "{body}");
        assert_eq!(
            body["datasets"][0]["schema_refs"][0],
            "https://rpsm.evorule.org/schemas/scenario/v1.0.json",
            "{body}"
        );

        // S3：全量条目
        let (status, body) = oneshot_json(app.clone(), "GET", "/api/knowledge/ds-a/entries").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["count"], 1, "{body}");
        assert_eq!(body["entries"][0]["entry_id"], "scn-001", "{body}");
        // manifest 携带 domain/tags（段2 P1 新字段）
        assert_eq!(body["entries"][0]["domain"], "physics", "{body}");
        assert_eq!(body["entries"][0]["tags"][0], "spring", "{body}");

        // S3 过滤矩阵：domain 精确（忽略大小写）
        let (status, body) = oneshot_json(
            app.clone(),
            "GET",
            "/api/knowledge/ds-a/entries?domain=PHYSICS",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["count"], 1, "{body}");
        let (status, body) = oneshot_json(
            app.clone(),
            "GET",
            "/api/knowledge/ds-a/entries?domain=chem",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["count"], 0, "{body}");

        // S3 过滤矩阵：tags 任一命中
        let (status, body) = oneshot_json(
            app.clone(),
            "GET",
            "/api/knowledge/ds-a/entries?tags=spring",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["count"], 1, "{body}");

        // S3 过滤矩阵：q 包含匹配（payload 文本）
        let (status, body) =
            oneshot_json(app.clone(), "GET", "/api/knowledge/ds-a/entries?q=chem-mix").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["count"], 0, "{body}");
        let (status, body) = oneshot_json(
            app.clone(),
            "GET",
            "/api/knowledge/ds-a/entries?q=single-particle",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["count"], 1, "{body}");

        // S3：数据集未承载 → 404（不静默空列表）
        let (status, body) =
            oneshot_json(app.clone(), "GET", "/api/knowledge/no-such/entries").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

        // S4：单条直取 200 + 404
        let (status, body) =
            oneshot_json(app.clone(), "GET", "/api/knowledge/ds-a/entries/scn-001").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["payload"]["scenario_id"], "spring-single-particle",
            "{body}"
        );
        assert_eq!(body["bundle_id"], "bundle-ds-a-v1", "{body}");
        let (status, body) =
            oneshot_json(app.clone(), "GET", "/api/knowledge/ds-a/entries/nope").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert!(body["error"].as_str().is_some(), "{body}");
    }

    /// S6：knowledge 库加载失败 → 数据面 500 显式（不静默空列表）
    #[tokio::test]
    async fn knowledge_data_plane_load_error_is_explicit() {
        let tmp = tempfile::tempdir().unwrap();
        // 伪造损坏 bundle → 启动加载失败（数据面不可用）；须在 SessionApi 构造前就位
        let bdir = tmp
            .path()
            .join("knowledge")
            .join("bundles")
            .join("bundle-bad");
        std::fs::create_dir_all(&bdir).unwrap();
        std::fs::write(bdir.join("bundle_manifest.json"), "{ not json").unwrap();

        let sessions = test_api(&tmp);
        assert!(
            sessions.knowledge_load_error().is_some(),
            "损坏 manifest 应使启动加载显式失败"
        );

        let app = knowledge_router(sessions);
        let (status, body) = oneshot_json(app, "GET", "/api/knowledge").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
        assert!(
            body["error"].as_str().unwrap().contains("加载失败"),
            "错误应显式指向加载失败: {body}"
        );
    }
}

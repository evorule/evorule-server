//! 数据治理攻坚 总验收（历史批次第三节端到端演练，段B 完成标准）
//!
//! 单链全走真实 API（治理侧 axum router oneshot + 本地 mock evo-agent serve +
//! 执行侧真实 SessionApi + 本地 echo 服务），非桩模拟：
//!
//! ```text
//! 平台层引导(org-b + 成员指派, B1)
//!   → 双层租户隔离(org-a 用户不可见 org-b private 数据集)
//!   → 科学家建数据集(rule_engineer, org-b)
//!   → LLM 草稿(历史批次 /llm/ops/draft_rule → mock evo-agent; 仅 Draft)
//!   → 人工 gate two(条目 submit-candidate/approve + 数据集 lifecycle + 独立发布)
//!   → 执行侧拉包直跑(快照包 → SessionApi import → 自有 io_type acc_echo_v1 → echo 命中)
//!   → 审计回放(auth/lifecycle/llm 三审计链可追溯)
//!   → 全程: 裁剪视图引用版本链(view_of) + 查询表达式(search/entries)
//! ```
//!
//! 负向(独立测试): 凭据永不入库 —— 发布前 scan_credentials 静态扫描兜底拒绝。
//!
//! 关联: B1(租户) B2(三层绑定) B3(查询) B4(版本导出) B5(事件声明随包)。

// 集成测试保留 unwrap 惯例（C5 unwrap/expect/panic = deny 仅约束生产代码）
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use serde_json::{json, Value};
use tower::ServiceExt;

use evorule_io_handlers::{HttpHandler, ServiceMeta, ServiceRegistry, ServiceRegistryHandler};
use evorule_reactor::IoType;
use evorule_rule::model::dependency::{DataDependencies, ServiceDecl, SourceBinding};
use evorule_rule::{router, AppState, Role, RuleStore};
use evorule_server::api::server::SessionApi;
use evorule_workspace::SessionOps;

/// 服务名（三层绑定对齐锚：治理声明 = 条目绑定 = 执行侧注册表键）
const SVC: &str = "echo_svc_acc";

/// 规则条目体（与 binding_e2e 同构的完整 rule_set 文档：call_service 触发，
/// 自有 io_type acc_echo_v1 消费——bundle 禁发射/消费平台保留 io_type。
/// 发射+消费合并为**单条嵌套双路径规则**：装载防线 reject_overlapped_io_rules
/// 对同一 io_type 的权利面独占、跨条目重叠 fail-closed 拒载，单条持有则合法）
const RULE_BODY: &str = r#"{
    "$schema": "https://evorule.org/schemas/rule_set/v1.0.json",
    "kind": "rule_set",
    "id": "org.evorule.acc.e2e",
    "rule_id": "acc.e2e",
    "version": "0.1.0",
    "description": "总验收规则 —— call_service 触发后以自有 io_type acc_echo_v1 经 ServiceRegistry 路由到 echo_svc_acc，结果存入 payload.service_result（单条双路径：发射+消费同条持有 io 权利面）",
    "transform": [
        {
            "type": "branch",
            "params": {
                "domain": { "type": "instruction", "instruction_type": "call_service" },
                "on_true": [
                    {
                        "type": "branch",
                        "params": {
                            "domain": {
                                "type": "exists",
                                "path": "__exec__.payload.__io_results__.acc_echo_v1"
                            },
                            "on_true": [
                                {
                                    "type": "set",
                                    "params": {
                                        "attr": "service_result",
                                        "operation": "set",
                                        "value": "__exec__.payload.__io_results__.acc_echo_v1"
                                    }
                                },
                                {
                                    "type": "set",
                                    "params": {
                                        "attr": "__exec__.payload.__io_results__.acc_echo_v1",
                                        "operation": "set",
                                        "value": null
                                    }
                                },
                                { "type": "push", "params": { "instructions": [{ "type": "noop" }] } }
                            ],
                            "on_false": [
                                {
                                    "type": "io_request",
                                    "params": {
                                        "io_type": "acc_echo_v1",
                                        "service_name": "echo_svc_acc",
                                        "args": "__exec__.instruction.params.args"
                                    }
                                }
                            ]
                        }
                    }
                ],
                "on_false": []
            }
        }
    ]
}"#;

// ================= 通用工具 =================

/// 治理侧 API oneshot 调用（与 evorule-rule api tests 同口径）
async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (axum::http::StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    let req = builder
        .header("content-type", "application/json")
        .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap_or_default();
    let value = if text.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::Null)
    };
    (status, value)
}

/// 本地 mock evo-agent serve（POST /ops/draft_rule 返回 completed + 草稿规则体）
async fn spawn_mock_evo_agent() -> String {
    async fn draft_rule() -> axum::Json<Value> {
        axum::Json(json!({
            "operation": "draft_rule",
            "request_id": "mock-req-1",
            "status": "completed",
            "result": {
                "entry_id": "acc-rule-1",
                "rule_body": serde_json::from_str::<Value>(RULE_BODY).unwrap()
            },
            "llm_generated": { "model": "mock-1", "op": "draft_rule" }
        }))
    }
    let app = axum::Router::new().route("/ops/draft_rule", axum::routing::post(draft_rule));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// 本地 echo 服务（真实 HTTP 后端；POST body 原样回显）
async fn spawn_echo_service() -> String {
    async fn echo(axum::Json(body): axum::Json<Value>) -> axum::Json<Value> {
        axum::Json(json!({ "ok": true, "echo": body }))
    }
    let app = axum::Router::new().route("/echo", axum::routing::post(echo));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/echo")
}

/// 执行侧 SessionApi（与 binding_e2e 同口径：ServiceRegistryHandler + echo 注册表）
fn build_session_api(rules_dir: &std::path::Path, echo_url: &str) -> SessionApi {
    let core_eval_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../resources/server_eval.json");
    // 夹具包引用 sandbox:1 → in-memory 元数据库 seed 真实沙盒
    // （证据深度校验：存在性 + closed + 报告 failed==0 的放行面）
    let ws_db = std::sync::Arc::new(evorule_workspace::WorkspaceDb::in_memory().unwrap());
    let seed_sid = common::seed_pass_sandbox(&ws_db);
    assert_eq!(seed_sid, 1, "空库首个 seed 应得 sandbox:1(夹具引用约定)");
    let registry = ServiceRegistry::load_from_str(&format!(
        r#"{{ "{SVC}": {{ "url": "{echo_url}", "method": "POST" }} }}"#
    ))
    .unwrap();
    let http = Arc::new(HttpHandler::new_dev_allow_loopback());
    let svc_handler = Arc::new(ServiceRegistryHandler::new(registry.clone(), http));
    // bundle 自有 io_type 注册（对应 RULE_BODY 的 io_request/`__io_results__` 引用；
    // bundle 禁发射/消费平台保留 io_type，call_service 不对 bundle 通路开放）
    let dispatcher = evorule_governance::IoDispatcher::builder()
        .register(IoType::new("acc_echo_v1"), svc_handler.clone())
        .register(IoType::call_external(), svc_handler)
        .build();
    // 权限缺省 fail-closed 后（human 缺省 Deny，宽松=显式经账本声明），生产组装在
    // 启动期幂等播种 human×io:* 放行条目（main.rs 同款 id/结构）；本测试组装不经
    // 启动流程，按生产同口径补种——否则声明 human 的会话分发被缺省拒绝，
    // echo 回写断言只能等到超时。
    let shared_facts = evorule_governance::shared_facts_log::SharedFactsLog::new();
    {
        use evorule_governance::permission::{
            Effect, PermissionEntry, PermissionState, Resource, ResourceType, Subject, SubjectType,
        };
        let mut entry = PermissionEntry::new(
            "default-human-allow-io",
            Subject {
                subject_type: SubjectType::User,
                id: "human".to_string(),
            },
            Resource {
                resource_type: ResourceType::IoAction,
                path: "io:*".to_string(),
            },
            Effect::Allow,
        );
        entry.state = PermissionState::Active;
        entry.updated_by = "bootstrap".to_string();
        evorule_governance::permission::PermissionTable::store_entry(&shared_facts, &entry, 0)
            .expect("seed default-human-allow-io (same as production bootstrap)");
    }
    SessionApi::new_with_full_config(
        // 装载防线整改方案 C：构造传真实宪法（空 vec 会使导入期前置预判基线播种
        // zip 截断为空 → io 重叠预判 fail-open，已登记整改）
        common::load_constitution_transforms(&core_eval_path),
        100,
        None,
        false,
        100 * 1024 * 1024,
        false,
        1000,
        1,
        core_eval_path,
        rules_dir.to_path_buf(),
        evorule_governance_host::session::DEFAULT_IO_WARN_TIMEOUT.as_secs(),
        evorule_governance_host::session::DEFAULT_IO_ERROR_TIMEOUT.as_secs(),
        0,    // 0=引擎缺省 TTL
        None, // 锚点链：测试构造不启用
    )
    .with_workspace_db(ws_db)
    .with_dispatcher(dispatcher)
    .with_shared_facts(shared_facts)
    .with_bound_services([SVC.to_string()])
    .with_registry_services([ServiceMeta {
        name: SVC.to_string(),
        version: None,
        description: Some("本地 echo 服务（测试后端）".into()),
    }])
}

async fn wait_for_payload_key(
    api: &SessionApi,
    session_id: u64,
    key: &str,
    secs: u64,
) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while tokio::time::Instant::now() < deadline {
        let state = api.get_session_state(session_id).await.unwrap();
        if let Some(v) = state["payload"].get(key) {
            return Some(v.clone());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    None
}

/// 治理侧应用搭建：默认 org-a + mock evo-agent + 平台管理员 root（直接落库引导）
fn build_gov(tmp: &std::path::Path, llm_base_url: &str) -> (axum::Router, Arc<RuleStore>) {
    let gov_dir = tmp.join("governance");
    std::fs::create_dir_all(&gov_dir).unwrap();
    let store = RuleStore::open(gov_dir.join("db.sqlite").to_str().unwrap()).unwrap();
    store
        .ensure_default_tenant("org-a", "甲方组织", "inst-acc", "2026-08-31T00:00:00Z")
        .unwrap();
    store
        .ensure_default_org("org-a", "甲方组织", "2026-08-31T00:00:00Z")
        .unwrap();
    // 平台管理员引导（API 公共注册固定 rule_engineer，平台管理员只能落库创建）
    store.get_tenant("org-a").unwrap().expect("tenant 应已就绪");
    let auth = evorule_rule::AuthService::new("test-secret");
    auth.register(
        &store,
        "org-a",
        "root",
        "password123",
        Role::PlatformAdmin,
        0,
    )
    .unwrap();
    let state = AppState::new(store, "test-secret", "inst-acc", llm_base_url);
    (router(state.clone()), state.store.clone())
}

// ================= 总验收主链 =================

// multi_thread：LLM 代理内 ureq 为阻塞调用（历史批次同步主路径），单线程 runtime
// 会被 oneshot 处理器饿死 mock evo-agent 任务 → 挂死；多 worker 下阻塞仅占一个 worker。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// 总验收主链:治理→发布→导入→执行→审计单用例贯通,场景化测试不拆分
#[allow(clippy::too_many_lines)]
async fn acceptance_governance_to_execution_full_chain() {
    // 签名策略迁移档：本链路验证治理→执行语义（发布签名的 enforce 语义由
    // bundles 模块纯函数测试专测），e2e 语料为无签名包，显式走 warn 档。
    std::env::set_var("EVORULE_BUNDLE_SIGNATURE_POLICY", "warn");
    let tmp = tempfile::tempdir().unwrap();
    let llm = spawn_mock_evo_agent().await;
    let (app, store) = build_gov(tmp.path(), &llm);

    // ---------- 阶段 0：平台层引导（双层租户） ----------
    let (st, body) = send(
        &app,
        "POST",
        "/v1/auth/login",
        None,
        Some(json!({ "tenant_id": "org-a", "username": "root", "password": "password123" })),
    )
    .await;
    assert_eq!(st, axum::http::StatusCode::OK, "root 登录: {body}");
    let root = body["access_token"].as_str().unwrap().to_string();

    // 平台管理员创建乙方组织
    let (st, body) = send(
        &app,
        "POST",
        "/v1/orgs",
        Some(&root),
        Some(json!({ "org_id": "org-b", "name": "乙方服务组织" })),
    )
    .await;
    assert_eq!(st, axum::http::StatusCode::CREATED, "创建组织: {body}");

    // 注册成员（公共注册固定 rule_engineer，默认入 org-a）
    for (name, role_b) in [
        ("scientist", "rule_engineer"),
        ("approver-b", "approver"),
        ("admin-b", "admin"),
    ] {
        let (st, body) = send(
            &app,
            "POST",
            "/v1/auth/register",
            None,
            Some(json!({ "tenant_id": "org-a", "username": name, "password": "password123" })),
        )
        .await;
        assert_eq!(st, axum::http::StatusCode::CREATED, "注册 {name}: {body}");
        let uid = store
            .get_user_by_username_any(name)
            .unwrap()
            .expect("用户应存在")
            .user_id;
        // 平台管理员把成员指派进 org-b（授权变更入审计）
        let (st, body) = send(
            &app,
            "POST",
            "/v1/orgs/org-b/members",
            Some(&root),
            Some(json!({ "user_id": uid, "role": role_b })),
        )
        .await;
        assert_eq!(st, axum::http::StatusCode::CREATED, "指派 {name}: {body}");
    }

    // 各成员按 org-b 身份登录（跨 org 经成员关系解析）
    async fn login(app: axum::Router, u: &str) -> String {
        let (st, body) = send(
            &app,
            "POST",
            "/v1/auth/login",
            None,
            Some(json!({ "tenant_id": "org-b", "username": u, "password": "password123" })),
        )
        .await;
        assert_eq!(st, axum::http::StatusCode::OK, "登录 {u}: {body}");
        body["access_token"].as_str().unwrap().to_string()
    }
    let scientist = login(app.clone(), "scientist").await;
    let approver = login(app.clone(), "approver-b").await;
    let admin_b = login(app.clone(), "admin-b").await;

    // ---------- 阶段 0b：双层租户隔离（验收断言） ----------
    // org-a 侧注册一个 viewer，稍后验证其看不到 org-b 的 private 数据集
    {
        let (st, body) = send(
            &app,
            "POST",
            "/v1/auth/register",
            None,
            Some(
                json!({ "tenant_id": "org-a", "username": "viewer-a", "password": "password123" }),
            ),
        )
        .await;
        assert_eq!(st, axum::http::StatusCode::CREATED, "{body}");
        let uid = store
            .get_user_by_username_any("viewer-a")
            .unwrap()
            .unwrap()
            .user_id;
        let (st, _) = send(
            &app,
            "POST",
            "/v1/orgs/org-a/members",
            Some(&root),
            Some(json!({ "user_id": uid, "role": "viewer" })),
        )
        .await;
        assert_eq!(st, axum::http::StatusCode::CREATED);
    }
    let (st, body) = send(
        &app,
        "POST",
        "/v1/auth/login",
        None,
        Some(json!({ "tenant_id": "org-a", "username": "viewer-a", "password": "password123" })),
    )
    .await;
    assert_eq!(st, axum::http::StatusCode::OK, "{body}");
    let viewer_a = body["access_token"].as_str().unwrap().to_string();

    // ---------- 阶段 1：科学家建数据集（org-b，private） ----------
    let (st, body) = send(
        &app,
        "POST",
        "/v1/datasets",
        Some(&scientist),
        Some(json!({
            "dataset_id": "ds-acc",
            "name": "总验收规则集",
            "description": "历史批次第三节端到端演练",
            "domain": ["tax"],
            "visibility": "private",
            "law_ref": { "document_id": "acc-e2e", "effective_from": "2026-08-31" }
        })),
    )
    .await;
    assert_eq!(st, axum::http::StatusCode::CREATED, "建数据集: {body}");

    // 跨租户隔离：org-a viewer 不可见（private 非 Public+Published → 404）
    let (st, _) = send(&app, "GET", "/v1/datasets/ds-acc", Some(&viewer_a), None).await;
    assert_eq!(
        st,
        axum::http::StatusCode::NOT_FOUND,
        "跨 org private 数据集必须不可见"
    );

    // 服务目录注册（admin-b；目录存描述不存端点/凭据）+ 数据依赖声明（层 1）
    let (st, body) = send(
        &app,
        "POST",
        "/v1/services",
        Some(&admin_b),
        Some(json!({ "service_name": SVC, "sensitive": false, "description": "本地 echo 服务（测试后端）" })),
    )
    .await;
    assert_eq!(st, axum::http::StatusCode::CREATED, "注册服务: {body}");
    let (st, body) = send(
        &app,
        "PUT",
        "/v1/deps/datasets/ds-acc",
        Some(&scientist),
        Some(json!({
            "inputs": [],
            "services": [{
                "service_name": SVC, "version": null, "io_contract": null,
                "sensitive": false, "description": "本地 echo 服务（测试后端）", "template": null
            }]
        })),
    )
    .await;
    assert_eq!(st, axum::http::StatusCode::OK, "声明数据依赖: {body}");

    // ---------- 阶段 2：LLM 草稿（历史批次，仅 Draft） ----------
    let (st, body) = send(
        &app,
        "POST",
        "/v1/llm/ops/draft_rule",
        Some(&scientist),
        Some(json!({
            "model": "mock-1",
            "request_id": "req-acc-1",
            "params": { "prompt": "写一个调用 echo 服务的示例规则" }
        })),
    )
    .await;
    assert_eq!(st, axum::http::StatusCode::OK, "LLM 草稿: {body}");
    assert_eq!(
        body["status"], "completed",
        "mock evo-agent 应 completed: {body}"
    );

    // 草稿经人工确认入库（provenance 声明 LLM 溯源 + 人工确认），初始状态 Draft
    let (st, body) = send(
        &app,
        "POST",
        "/v1/entries",
        Some(&scientist),
        Some(json!({
            "dataset_id": "ds-acc",
            "entry_id": "acc-rule-1",
            "domain": "tax",
            "rule_body": serde_json::from_str::<Value>(RULE_BODY).unwrap(),
            "data_source_binding": [
                { "rule_ref": "transform[0].params.service_name", "service_name": SVC }
            ],
            "provenance": {
                "source": "LLM draft_rule 草稿（evo-agent serve，人工确认入库）",
                "clause": null, "document_id": null,
                "effective_from": null, "effective_to": null,
                "last_verified": null, "verified_by": null
            }
        })),
    )
    .await;
    assert_eq!(st, axum::http::StatusCode::CREATED, "条目入库: {body}");
    assert_eq!(body["status"], "Draft", "LLM 草稿只能停在 Draft: {body}");

    // gate two 负向：Draft 数据集直接发布必须被拒（Published 必须走独立审批端点 + 状态机）
    let (st, body) = send(
        &app,
        "POST",
        "/v1/datasets/ds-acc/publish",
        Some(&approver),
        Some(json!({ "confirm": true })),
    )
    .await;
    assert!(
        st.is_client_error(),
        "Draft 直接发布必须被拒（gate two），got {st}: {body}"
    );

    // ---------- 阶段 3：人工 gate two + 独立发布审批 ----------
    let (st, body) = send(
        &app,
        "POST",
        "/v1/entries/acc-rule-1/submit-candidate",
        Some(&scientist),
        Some(json!({ "sandbox_report_id": "rep-acc-1" })),
    )
    .await;
    assert_eq!(st, axum::http::StatusCode::OK, "闸门一送审: {body}");
    let (st, body) = send(
        &app,
        "POST",
        "/v1/entries/acc-rule-1/approve",
        Some(&approver),
        None,
    )
    .await;
    assert_eq!(st, axum::http::StatusCode::OK, "闸门二审批: {body}");

    let (st, body) = send(
        &app,
        "PATCH",
        "/v1/datasets/ds-acc/lifecycle",
        Some(&scientist),
        Some(json!({ "to": "candidate" })),
    )
    .await;
    assert_eq!(st, axum::http::StatusCode::OK, "数据集送审: {body}");
    let (st, body) = send(
        &app,
        "PATCH",
        "/v1/datasets/ds-acc/lifecycle",
        Some(&approver),
        Some(json!({ "to": "active" })),
    )
    .await;
    assert_eq!(st, axum::http::StatusCode::OK, "数据集生效: {body}");
    let (st, body) = send(
        &app,
        "POST",
        "/v1/datasets/ds-acc/publish",
        Some(&approver),
        Some(json!({ "confirm": true, "reason": "历史批次第三节总验收发布" })),
    )
    .await;
    assert_eq!(st, axum::http::StatusCode::OK, "独立发布审批: {body}");
    assert_eq!(body["lifecycle"]["status"], "Published", "{body}");

    // ---------- 阶段 4：导出 + 裁剪视图（引用版本链） + 查询表达式 ----------
    // 全量导出（无证据导出显式 unverified，不伪造 Pass）
    let (st, bundle) = send(
        &app,
        "GET",
        "/v1/bundles/datasets/ds-acc/versions/v1",
        Some(&scientist),
        None,
    )
    .await;
    assert_eq!(st, axum::http::StatusCode::OK, "导出: {bundle}");
    assert_eq!(
        bundle["data_dependencies"]["services"][0]["service_name"], SVC,
        "层 1 声明应随包流转"
    );

    // 裁剪视图：view_of 指向原版本（裁剪版=视图，非新数据集）
    let (st, trimmed) = send(
        &app,
        "GET",
        "/v1/bundles/datasets/ds-acc/versions/v1?subset=ids:acc-rule-1",
        Some(&scientist),
        None,
    )
    .await;
    assert_eq!(st, axum::http::StatusCode::OK, "裁剪导出: {trimmed}");
    assert_eq!(
        trimmed["dataset"]["view_of"]["original_dataset_id"], "ds-acc",
        "裁剪视图必须引用原版本链: {trimmed}"
    );
    assert_eq!(
        trimmed["dataset"]["view_of"]["view_of_version"], "v1",
        "{trimmed}"
    );

    // 查询表达式（domain 段筛选）
    let (st, found) = send(
        &app,
        "GET",
        "/v1/search/entries?dataset_id=ds-acc&domain=tax",
        Some(&scientist),
        None,
    )
    .await;
    assert_eq!(st, axum::http::StatusCode::OK, "查询: {found}");
    assert_eq!(found["items"].as_array().unwrap().len(), 1, "{found}");

    // 带闸门一证据导出（决策：执行侧导入要求 verdict=Pass，不默认 Pass；
    // 双闸：pass 必带 sandbox: 机器背书引用——执行侧 SessionApi 接 seed
    // 真实沙盒 #1（common::seed_pass_sandbox），深度校验放行面）
    let (st, bundle) = send(
        &app,
        "POST",
        "/v1/bundles/export",
        Some(&scientist),
        Some(json!({
            "dataset_id": "ds-acc",
            "version": "v1",
            "tests": { "subset": ["sandbox:1"], "fixtures": [], "verdict": "pass" }
        })),
    )
    .await;
    assert_eq!(st, axum::http::StatusCode::OK, "带证据导出: {bundle}");
    assert_eq!(bundle["tests"]["verdict"], "pass", "{bundle}");

    // ---------- 阶段 5：执行侧拉包直跑（三层绑定，B2 同口径） ----------
    let rules_dir = tmp.path().join("rules");
    let echo_url = spawn_echo_service().await;
    let api = build_session_api(&rules_dir, &echo_url);
    // 交付边界：快照包经序列化往返后导入
    let bundle_json = serde_json::to_string(&bundle).unwrap();
    let bundle: evorule_rule::DatasetBundle = serde_json::from_str(&bundle_json).unwrap();
    let result = api.import_bundle(&bundle, false).await.unwrap();
    // 装载防线整改方案 B：防线透传回归锚——rejected_overlaps 非空 = bundle 规则被
    // 装载防线静默剔除（整改前曾静默发生，此处显式断言不再无人消费）
    assert!(
        result.rejected_overlaps.is_empty(),
        "import 装载防线不应拒载条目: {:?}",
        result.rejected_overlaps
    );
    assert_eq!(result.dataset_id, "ds-acc");
    assert_eq!(result.entry_count, 1);
    let session_id = api.create_session().await.unwrap();
    // 会话主体声明：IoSubscriber 权限门 fail-closed（未声明 = Unknown = Deny，
    // 自动分发的 IoRequest 被"permission denied"错误响应消费且不回写 payload）；
    // 声明 human 走 default_policy.human = Allow，与生产声明流程同口径
    api.declare_caller_role(session_id, "human").await.unwrap();
    api.send_command(
        session_id,
        json!({ "type": "call_service", "params": { "args": { "msg": "acceptance-ping" } } }),
    )
    .await
    .unwrap();
    let service_result = wait_for_payload_key(&api, session_id, "service_result", 10)
        .await
        .expect("执行侧应命中 echo 服务并回写");
    assert_eq!(service_result["ok"], true, "{service_result}");
    assert_eq!(service_result["echo"]["msg"], "acceptance-ping");

    // ---------- 阶段 6：审计回放（三审计链可追溯；审计按 org 租户隔离） ----------
    // 授权/平台操作审计：org-a（root 视角）含 create_org；org-b（admin_b 视角）含 assign_role
    let (st, audits) = send(&app, "GET", "/v1/audits", Some(&root), None).await;
    assert_eq!(st, axum::http::StatusCode::OK);
    assert!(
        audits.to_string().contains("create_org"),
        "平台操作应入 org-a 审计: {audits}"
    );
    let (st, audits_b) = send(&app, "GET", "/v1/audits", Some(&admin_b), None).await;
    assert_eq!(st, axum::http::StatusCode::OK);
    assert!(
        audits_b.to_string().contains("assign_role"),
        "成员指派应入 org-b 审计: {audits_b}"
    );

    // 生命周期审计：org-b 视角可回放 Draft→Candidate→Active→Published 路径
    let (st, life) = send(&app, "GET", "/v1/audits/lifecycle", Some(&admin_b), None).await;
    assert_eq!(st, axum::http::StatusCode::OK);
    let text = life.to_string();
    assert!(
        text.contains("ds-acc"),
        "数据集迁移应入生命周期审计: {text}"
    );
    assert!(text.contains("Published"), "发布应入生命周期审计: {text}");

    // LLM 操作审计（设计文档 §8）：draft_rule completed 可溯源
    let (st, la) = send(&app, "GET", "/v1/llm/audits", Some(&root), None).await;
    assert_eq!(st, axum::http::StatusCode::OK);
    assert_eq!(
        la["items"][0]["operation"], "draft_rule",
        "LLM 草稿操作应入审计: {la}"
    );
    assert_eq!(la["items"][0]["status"], "completed", "{la}");

    // 条目状态历史（only-append 回放）
    let (st, hist) = send(
        &app,
        "GET",
        "/v1/entries/acc-rule-1/history",
        Some(&scientist),
        None,
    )
    .await;
    assert_eq!(st, axum::http::StatusCode::OK, "{hist}");
    let text = hist.to_string();
    assert!(
        text.contains("Candidate") && text.contains("Active"),
        "{text}"
    );
}

// ================= 负向：凭据永不入库（扫描兜底） =================

#[tokio::test]
async fn acceptance_credential_scan_blocks_entry_ingest() {
    let tmp = tempfile::tempdir().unwrap();
    let (app, _store) = build_gov(tmp.path(), "http://127.0.0.1:9");

    let (st, body) = send(
        &app,
        "POST",
        "/v1/auth/login",
        None,
        Some(json!({ "tenant_id": "org-a", "username": "root", "password": "password123" })),
    )
    .await;
    assert_eq!(st, axum::http::StatusCode::OK, "{body}");
    let root = body["access_token"].as_str().unwrap().to_string();

    let (st, body) = send(
        &app,
        "POST",
        "/v1/datasets",
        Some(&root),
        // law_ref:通过 前置校验(auto_by_effective_date 缺省模式需生效基准),
        // 让发布链抵达凭据扫描断言点(:此前 fixture 缺锚被前置校验先拦,
        // 断言"凭据扫描拦截"永不成立)
        Some(json!({
            "dataset_id": "ds-cred",
            "name": "含凭据数据集",
            "domain": ["tax"],
            "law_ref": { "document_id": "acc-cred-scan", "effective_from": "2026-01-01" }
        })),
    )
    .await;
    assert_eq!(st, axum::http::StatusCode::CREATED, "{body}");

    // 规则体夹带疑似凭据（静态扫描命中）—— 入库闸即拦（扫描前移后 fail-fast：
    // 凭据永不入规则资产库，设计文档 §6/§9-3；带凭据条目无法再到达发布，
    // 发布闸扫描转为对既有数据/旁路入库的纵深防御）
    let mut cred_body = serde_json::from_str::<Value>(RULE_BODY).unwrap();
    cred_body["description"] = json!("api_key=super-secret-123 泄漏测试");
    let (st, body) = send(
        &app,
        "POST",
        "/v1/entries",
        Some(&root),
        Some(json!({
            "dataset_id": "ds-cred",
            "entry_id": "cred-rule-1",
            "domain": "tax",
            "rule_body": cred_body
        })),
    )
    .await;
    assert_eq!(
        st,
        axum::http::StatusCode::BAD_REQUEST,
        "凭据条目入库即拒（fail-fast）: {body}"
    );
    let text = body.to_string();
    assert!(
        text.contains("凭据") || text.contains("CredentialScan"),
        "错误信息应指向凭据扫描: {text}"
    );
}

// DataDependencies/ServiceDecl 形状回归护栏（与治理侧声明端点同构）
#[test]
fn acceptance_decl_shapes_match_wire_contract() {
    let dd = DataDependencies {
        inputs: vec![],
        services: vec![ServiceDecl {
            service_name: SVC.into(),
            version: None,
            io_contract: None,
            sensitive: false,
            description: Some("形状护栏".into()),
            template: None,
        }],
    };
    let v = serde_json::to_value(&dd).unwrap();
    assert_eq!(v["services"][0]["service_name"], SVC);
    let _sb = SourceBinding {
        rule_ref: "transform[0].params.service_name".into(),
        service_name: SVC.into(),
    };
}

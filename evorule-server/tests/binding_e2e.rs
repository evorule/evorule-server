//! 三层绑定执行侧闭环端到端验收（数据治理攻坚 段B / B2，历史批次实施计划）
//!
//! 不新增机制，做**闭环实证**——真实跨仓 API 调用，非桩模拟：
//!
//! 层 1（治理侧声明）：evorule-rule 数据集 `data_dependencies` 声明 echo_svc_e2e；
//! 层 2（条目级绑定）：规则条目 `data_source_binding` 把 rule_body 内服务符号
//!                    绑定到 echo_svc_e2e（治理侧 add_entry 走符号三方一致门禁）；
//! 层 3（执行侧绑定）：server 侧 `service_registry`（内存注册表，形态与
//!                    service_registry.json 同源）把 echo_svc_e2e 翻译为 HTTP 端点。
//!
//! 全链：治理侧 4 步发布（create → add_entry → 状态迁移 → 发布审批）
//!   → `BundleExporter::export` 规则快照包（含 data_dependencies）
//!   → 序列化往返（模拟交付边界）
//!   → 执行侧 `import_bundle`（6 项校验链 + 逐条 Schema 门禁 + **第 8 项服务绑定核对**）
//!   → 落盘 rules_dir/bundles/ + reload（新会话使用新规则）
//!   → 创建 session 提交 call_service 指令 → bundle 规则以自有 io_type `acc_echo_v1`
//!     发 io_request（宪法约束：bundle 禁发射/消费平台保留 io_type，自有 io_type 为正道）
//!     → dispatcher 注册 `acc_echo_v1` → `ServiceRegistryHandler` 按 service_name
//!     路由到本测试拉起的真实本地 echo 服务 → 结果回写 payload。
//!
//! 负向：声明了但注册表漏配 → 运行时 unknown service_name，错误信息含自诊断指引
//! （fail-fast + 可自愈，对齐系统自愈原则）；会话不挂死、如实 Stable。

// 集成测试保留 unwrap 惯例（C5 unwrap/expect/panic = deny 仅约束生产代码）
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use evorule_io_handlers::{HttpHandler, ServiceMeta, ServiceRegistry, ServiceRegistryHandler};
use evorule_reactor::IoType;
use evorule_server::api::server::SessionApi;
use evorule_workspace::SessionOps;

use evorule_rule::model::dependency::{DataDependencies, ServiceDecl, SourceBinding};
use evorule_rule::model::{
    LawRef, Lifecycle, Meta, VersionSelection, VersionSelectionMode, Visibility,
};
use evorule_rule::{
    BundleTests, DatasetBundle, DatasetKind, LifecycleStatus, Provenance, RuleDataset, RuleEntry,
    RuleStore, TestVerdict,
};

/// 服务名（三层绑定的对齐锚：治理声明 = 条目绑定 = 执行侧注册表键）
const SVC: &str = "echo_svc_e2e";

/// 规则条目体：完整 rule_set 文档（与 rules/10_role13_demo.json 同构，
/// call_service 指令触发；io 通路走 bundle 自有 io_type `acc_echo_v1`——
/// bundle 禁发射/消费平台保留 io_type，自有 io_type 为正道，dispatcher 侧
/// 注册 `acc_echo_v1` 交给 ServiceRegistryHandler 落到 SVC）。
/// 发射+消费合并为**单条嵌套双路径规则**：装载防线 reject_overlapped_io_rules
/// 对同一 io_type 权利面独占、跨条目重叠 fail-closed 拒载，单条持有则合法。
/// 用 const 而非返回 &str 的函数——与 q12_data_asset_e2e.rs 同一纪律，防经验漂移。
const RULE_BODY: &str = r#"{
    "$schema": "https://evorule.org/schemas/rule_set/v1.0.json",
    "kind": "rule_set",
    "id": "org.evorule.binding.e2e",
    "rule_id": "binding.e2e",
    "version": "0.1.0",
    "description": "三层绑定端到端验收规则 —— 触发 call_service 后以自有 io_type acc_echo_v1 经 ServiceRegistry 路由到 echo_svc_e2e，结果存入 payload.service_result（单条双路径：发射+消费同条持有 io 权利面）",
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
                                {
                                    "type": "push",
                                    "params": {
                                        "instructions": [{ "type": "noop" }]
                                    }
                                }
                            ],
                            "on_false": [
                                {
                                    "type": "io_request",
                                    "params": {
                                        "io_type": "acc_echo_v1",
                                        "service_name": "echo_svc_e2e",
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

/// 本地 echo 服务（真实 HTTP 后端；POST body 原样回显）。
/// 返回监听地址，服务随 tokio task 存活到测试结束。
async fn spawn_echo_service() -> String {
    async fn echo(
        axum::Json(body): axum::Json<serde_json::Value>,
    ) -> axum::Json<serde_json::Value> {
        axum::Json(serde_json::json!({ "ok": true, "echo": body }))
    }
    let app = axum::Router::new().route("/echo", axum::routing::post(echo));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/echo")
}

/// 治理侧 4 步发布：create（声明数据依赖）→ add_entry（条目级绑定）
/// → 条目/数据集状态迁移 → 发布审批；导出规则快照包并做交付边界往返。
#[allow(clippy::too_many_lines)]
async fn governance_export_bundle(tmp: &std::path::Path) -> DatasetBundle {
    let gov_dir = tmp.join("governance");
    std::fs::create_dir_all(&gov_dir).unwrap();
    let store = RuleStore::open(gov_dir.join("db.sqlite").to_str().unwrap()).unwrap();

    // 层 1：数据集级数据依赖声明（IoContract 缺省；sensitive=false 免注册表 C6 强核对）
    let ds = RuleDataset {
        dataset_id: "ds-binding-e2e".into(),
        name: "三层绑定端到端规则集".into(),
        description: Some("B2 闭环实证：声明→绑定→执行侧注册表→io_request 命中".into()),
        dataset_kind: DatasetKind::RuleSet,
        domain: vec!["binding".into()],
        tags: vec!["e2e".into()],
        tenant_id: "org-evorule".into(),
        visibility: Visibility::Private,
        lifecycle: Lifecycle::default(), // Draft
        versioning: Default::default(),  // current = v1
        law_ref: Some(LawRef {
            document_id: "binding-e2e".into(),
            law_version: None,
            effective_from: Some("2026-08-31".into()),
            effective_to: None,
        }),
        version_selection: Some(VersionSelection {
            mode: VersionSelectionMode::AutoByEffectiveDate,
            pinned_version: None,
            pinned_include_patch: None,
        }),
        data_dependencies: Some(DataDependencies {
            inputs: vec![],
            services: vec![ServiceDecl {
                service_name: SVC.into(),
                version: None,
                io_contract: None,
                sensitive: false,
                description: Some("本地 echo 服务（测试后端）".into()),
                template: None,
            }],
        }),
        event_schemas: vec![],
        meta: Meta {
            created_at: "2026-08-31T00:00:00Z".into(),
            created_by: "governor".into(),
            updated_at: None,
            updated_by: None,
        },
    };
    store.create_dataset(&ds).unwrap();

    // 层 2：条目级绑定（rule_body 内服务符号 → SVC；add_entry 走符号三方一致门禁，
    // 服务未声明即拒绝——三层绑定的治理侧事前预检）
    let entry = RuleEntry {
        entry_id: "binding-rule".into(),
        dataset_id: "ds-binding-e2e".into(),
        version: 1,
        status: Some(LifecycleStatus::Draft),
        provenance: Provenance {
            source: "B2 端到端验收".into(),
            clause: None,
            document_id: None,
            effective_from: None,
            effective_to: None,
            last_verified: None,
            verified_by: None,
        },
        domain: "binding".into(),
        tags: vec![],
        data_source_binding: vec![SourceBinding {
            rule_ref: "transform[0].params.service_name".into(),
            service_name: SVC.into(),
        }],
        consumed_inputs: vec![],
        rule_body: serde_json::from_str(RULE_BODY).unwrap(),
        governance: None,
    };
    store.add_entry(&entry).unwrap();

    // 条目状态迁移（Draft→Candidate→Active，双闸门口径）
    store
        .transition_entry_status(
            "ds-binding-e2e",
            "binding-rule",
            LifecycleStatus::Candidate,
            "engineer",
            "t1",
            "评审通过",
        )
        .unwrap();
    store
        .transition_entry_status(
            "ds-binding-e2e",
            "binding-rule",
            LifecycleStatus::Active,
            "engineer",
            "t2",
            "生效",
        )
        .unwrap();

    // 数据集状态迁移 + 独立发布审批（Active→Published，二次确认语义）
    store
        .transition_dataset_status(
            "ds-binding-e2e",
            LifecycleStatus::Candidate,
            "approver",
            "送审",
            "t3",
        )
        .unwrap();
    store
        .transition_dataset_status(
            "ds-binding-e2e",
            LifecycleStatus::Active,
            "approver",
            "生效",
            "t4",
        )
        .unwrap();
    store
        .publish_dataset_with_cause(
            "ds-binding-e2e",
            "publisher",
            "t5",
            "B2 三层绑定端到端发布审批",
        )
        .unwrap();

    // 导出（含 data_dependencies 随包流转）+ 交付边界序列化往返
    let ds_now = store
        .get_dataset("ds-binding-e2e")
        .unwrap()
        .expect("数据集应存在");
    assert_eq!(ds_now.lifecycle.status, LifecycleStatus::Published);
    let entries = store.list_entries("ds-binding-e2e", None).unwrap();
    assert_eq!(entries.len(), 1);

    let tests = BundleTests {
        // 证据契约: verdict=pass 的导入必须携带 sandbox: 机器背书引用
        // (人工背书 human:<actor> 形态已下线;执行侧 SessionApi 接 seed
        // 真实沙盒 #1——common::seed_pass_sandbox 深度校验放行面)
        subset: vec!["sandbox:1".to_string()],
        fixtures: vec![],
        verdict: TestVerdict::Pass,
    };
    let bundle = evorule_rule::bundle::BundleExporter::export(
        &ds_now,
        &entries,
        &tests,
        "publisher",
        "2026-08-31T00:00:00Z",
        "instance-1",
        &std::collections::BTreeMap::new(),
        None, // recipe_snapshot：本 e2e 不携带策略快照（缺省不序列化字节兼容）
    );
    assert_eq!(bundle.bundle_id, "bundle-ds-binding-e2e-v1");
    let dd = bundle
        .data_dependencies
        .as_ref()
        .expect("导出包应携带数据依赖声明");
    assert_eq!(dd.services[0].service_name, SVC, "层 1 声明应随包流转");

    // 交付边界：快照包经序列化（文件/网络传输）后反序列化，内容与哈希签名不变
    let json = serde_json::to_string_pretty(&bundle).unwrap();
    let bundle: DatasetBundle = serde_json::from_str(&json).unwrap();
    assert_eq!(
        bundle.audit.content_hash,
        bundle.compute_content_hash(),
        "序列化往返不得破坏防篡改签名"
    );
    bundle
}

/// 执行侧 SessionApi：dispatcher 注册 ServiceRegistryHandler（service_name → 真实本地
/// echo 服务）；`with_bound_services`/`with_registry_services` 注入第 8 项核对所需绑定集。
/// `registry_present=false` 时注册表为空（负向：漏配场景）。
fn build_session_api(rules_dir: &std::path::Path, echo_url: Option<&str>) -> SessionApi {
    let core_eval_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../resources/server_eval.json");
    // 夹具包引用 sandbox:1 → in-memory 元数据库 seed 真实沙盒
    // （证据深度校验：存在性 + closed + 报告 failed==0 的放行面）
    let ws_db = std::sync::Arc::new(evorule_workspace::WorkspaceDb::in_memory().unwrap());
    let seed_sid = common::seed_pass_sandbox(&ws_db);
    assert_eq!(seed_sid, 1, "空库首个 seed 应得 sandbox:1(夹具引用约定)");

    let registry = match echo_url {
        Some(url) => ServiceRegistry::load_from_str(&format!(
            r#"{{ "{SVC}": {{ "url": "{url}", "method": "POST" }} }}"#
        ))
        .unwrap(),
        None => ServiceRegistry::empty(),
    };

    // 本地回环测试后端 → 开发模式 HttpHandler（放行 loopback，与 --allow-loopback 同语义）
    let http = Arc::new(HttpHandler::new_dev_allow_loopback());
    let svc_handler = Arc::new(ServiceRegistryHandler::new(registry.clone(), http));
    // bundle 自有 io_type 注册（对应 RULE_BODY 的 io_request/`__io_results__` 引用；
    // bundle 禁发射/消费平台保留 io_type，call_service 不对 bundle 通路开放）
    let dispatcher = evorule_governance::IoDispatcher::builder()
        .register(IoType::new("acc_echo_v1"), svc_handler.clone())
        .register(IoType::call_external(), svc_handler)
        .build();

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
        evorule_governance::session::DEFAULT_IO_WARN_TIMEOUT.as_secs(),
        evorule_governance::session::DEFAULT_IO_ERROR_TIMEOUT.as_secs(),
        0,    // 0=引擎缺省 TTL
        None, // 锚点链：测试构造不启用
    )
    .with_workspace_db(ws_db)
    .with_dispatcher(dispatcher)
    .with_bound_services([SVC.to_string()])
    .with_registry_services([ServiceMeta {
        name: SVC.to_string(),
        version: None,
        description: Some("本地 echo 服务（测试后端）".into()),
    }])
}

/// 轮询会话状态直到 payload 出现 `key`（或超时）。
async fn wait_for_payload_key(
    api: &SessionApi,
    session_id: u64,
    key: &str,
    secs: u64,
) -> Option<serde_json::Value> {
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

/// 正向：三层绑定全链命中 —— 治理声明 → 条目绑定 → 执行侧注册表 → io_request
/// 经真实 HTTP 命中本地 echo 服务 → 结果回写 payload.service_result。
#[tokio::test]
async fn binding_e2e_declared_template_registry_hit() {
    let tmp = tempfile::tempdir().unwrap();
    let rules_dir = tmp.path().join("rules");

    // 1. 治理侧发布 + 导出（层 1/层 2）
    let bundle = governance_export_bundle(tmp.path()).await;

    // 2. 执行侧：真实本地 echo 服务 + 注册表绑定（层 3）+ 导入
    let echo_url = spawn_echo_service().await;
    let api = build_session_api(&rules_dir, Some(&echo_url));
    let result = api.import_bundle(&bundle, false).await.unwrap();
    // 装载防线整改方案 B：防线透传回归锚——rejected_overlaps 非空 = bundle 规则被
    // 装载防线静默剔除（整改前曾静默发生，此处显式断言不再无人消费）
    assert!(
        result.rejected_overlaps.is_empty(),
        "import 装载防线不应拒载条目: {:?}",
        result.rejected_overlaps
    );
    assert_eq!(result.dataset_id, "ds-binding-e2e");
    assert_eq!(result.entry_count, 1);

    // 落盘断言：规则包落 rules_dir/bundles/（TCB 加载路径）
    let landed = rules_dir.join("bundles").join("bundle-ds-binding-e2e-v1");
    assert!(
        landed.join("bundle_manifest.json").is_file(),
        "manifest 应落盘"
    );
    assert!(landed.join("binding-rule.json").is_file(), "规则条目应落盘");

    // 3. 新会话使用落地规则（reload 后），提交 call_service 指令
    let session_id = api.create_session().await.unwrap();
    // 会话主体声明：IoSubscriber 权限门 fail-closed（未声明 = Unknown = Deny，
    // 自动分发的 IoRequest 会被"permission denied"错误响应消费且不回写 payload）；
    // 声明 human 走 default_policy.human = Allow，与生产声明流程同口径
    api.declare_caller_role(session_id, "human").await.unwrap();
    let instruction = serde_json::json!({
        "type": "call_service",
        "params": { "args": { "msg": "ping-binding-e2e" } }
    });
    api.send_command(session_id, instruction).await.unwrap();

    // 4. io_request 经 ServiceRegistryHandler → 真实 HTTP → 结果回写
    let service_result = wait_for_payload_key(&api, session_id, "service_result", 10)
        .await
        .expect("10s 内应回写 service_result（绑定命中）");
    assert_eq!(
        service_result["ok"], true,
        "echo 服务应命中并回显: {service_result}"
    );
    assert_eq!(
        service_result["echo"]["msg"], "ping-binding-e2e",
        "请求 args 应原样到达服务并回显"
    );
}

/// 负向：治理侧已声明（导入期核对通过）但执行侧注册表漏配 → 运行时
/// unknown service_name；错误信息含自诊断指引（fail-fast + 可自愈），会话不挂死。
#[tokio::test]
async fn binding_e2e_missing_registry_binding_reports_self_healing_guidance() {
    let tmp = tempfile::tempdir().unwrap();
    let rules_dir = tmp.path().join("rules");

    let bundle = governance_export_bundle(tmp.path()).await;

    // 导入期：with_bound_services 含 SVC（模拟"绑定核对集已登记"）；
    // 运行期：注册表为空（漏配 service_registry.json 条目）→ unknown service_name
    let api = build_session_api(&rules_dir, None);
    let result = api.import_bundle(&bundle, false).await.unwrap();
    // 装载防线整改方案 B：与正向同口径——负向只漏配运行时注册表，导入期装载面应干净
    assert!(
        result.rejected_overlaps.is_empty(),
        "import 装载防线不应拒载条目: {:?}",
        result.rejected_overlaps
    );
    assert_eq!(result.dataset_id, "ds-binding-e2e");

    let session_id = api.create_session().await.unwrap();
    // 与正向同口径：声明 human 让 IoRequest 真正进入分发路径（未声明会被
    // 权限门抢先拒绝，注册表漏配错误到不了执行面，负向覆盖失真）
    api.declare_caller_role(session_id, "human").await.unwrap();
    let instruction = serde_json::json!({
        "type": "call_service",
        "params": { "args": { "msg": "ping" } }
    });
    api.send_command(session_id, instruction).await.unwrap();

    // 运行时绑定缺失：io_request 如实失败（service_result = null 或缺席），会话 Stable 不挂死
    let service_result = wait_for_payload_key(&api, session_id, "service_result", 10).await;
    // 未回写也接受（错误被如实暴露，无伪造结果）
    if let Some(v) = service_result {
        assert!(v.is_null(), "绑定缺失时结果应为 null（如实失败），got: {v}");
    }

    // 绑定缺失的错误信息必须含自诊断指引（经公开 IoHandler::execute，与运行时同口径）
    let handler =
        ServiceRegistryHandler::new(ServiceRegistry::empty(), Arc::new(HttpHandler::new()));
    let params = evorule_tcb::JsonValue::object_from_pairs(&[(
        "service_name",
        evorule_tcb::JsonValue::string(SVC),
    )]);
    use evorule_reactor::IoHandler as _;
    let err = handler.execute(&params).await.unwrap_err().to_string();
    assert!(err.contains("unknown service_name"), "got: {err}");
    assert!(
        err.contains("自诊断指引"),
        "错误信息应含自诊断指引，got: {err}"
    );
    assert!(
        err.contains("--service-registry") && err.contains(SVC),
        "指引应指向 service_registry 绑定路径与服务名，got: {err}"
    );
}

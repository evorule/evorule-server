//! Q12 数据资产化端到端验收（专项验收门 5，方案 §6）
//!
//! 全链演示「数据资产一等公民」交付路径——真实跨仓 API 调用，非桩模拟：
//!
//! 1. 治理侧（`evorule-rule`）knowledge 数据集 4 步：
//!    create → add_entry(Draft，D3 领域 schema 强校验入库)
//!    → 条目状态迁移（Draft→Candidate→Active）
//!    → 数据集状态迁移 + 独立发布审批（Active→Published）
//! 2. 治理侧导出：`export_knowledge` → 规范 `DatasetBundle` → JSON 序列化往返
//!    （模拟真实交付边界：快照包文件/网络传输后再入库）
//! 3. 执行侧（`evorule-server`）：`import_bundle` 全校验链
//!    （6 项硬校验 + 领域 schema resolver 强校验 + 同质性门禁）
//! 4. 落盘 `{knowledge_dir}/bundles/`（与 rules_dir 物理隔离，数据不进 TCB 路径）
//! 5. `KnowledgeStore` 直读命中（模拟 rpsm 原生服务按 dataset_id/entry_id 消费）
//! 6. TCB 合并集不变（数据条目不进规则执行路径的负向断言）
//!
//! 本测试只覆盖 happy path 的完整性验收；门禁负向用例见
//! `src/api/server.rs` W6 专项与 evorule-bundle / evorule-rule 各自测试域。

// 集成测试保留 unwrap 惯例（C5 unwrap/expect/panic = deny 仅约束生产代码）
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use evorule_rule::model::{
    LawRef, Lifecycle, Meta, VersionSelection, VersionSelectionMode, Visibility,
};
use evorule_rule::{
    BundleTests, DatasetBundle, DatasetKind, KnowledgeEntry, LifecycleStatus, Provenance,
    RuleDataset, RuleStore, TestVerdict,
};
use evorule_server::api::server::SessionApi;

/// rpsm 形态场景领域 schema（测试替身：领域 schema 归 rpsm 仓所有，
/// 此处模拟治理侧与执行侧各注册一份同源 schema）。
/// 注意：用 const 而非返回 `&'static str` 的函数——server build.rs 门禁的
/// 花括号状态机不感知生命周期撇号（对 src/ 内嵌测试生效；tests/ 目录
/// 不在门禁扫描范围，但保持同一纪律，防经验漂移）。
const SCENARIO_SCHEMA: &str = r#"{
    "$schema": "https://json-schema.org/draft/2020-12/schema",
    "$id": "https://rpsm.evorule.org/schemas/scenario/v1.0.json",
    "type": "object",
    "required": ["scenario_id", "gravity", "bodies"],
    "properties": {
        "scenario_id": {"type": "string"},
        "gravity": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3},
        "restitution": {"type": "number", "minimum": 0, "maximum": 1},
        "bodies": {"type": "array", "items": {"type": "object"}}
    }
}"#;

#[tokio::test]
// 端到端全链场景用例(治理发布→执行直读),场景化测试不拆分
#[allow(clippy::too_many_lines)]
async fn q12_e2e_governance_publish_to_execution_direct_read() {
    // ================= 环境布局 =================
    // {tmp}/governance/          治理侧（SQLite + domain_schemas/）
    // {tmp}/rules                执行侧 rules_dir（父目录 = tmp → knowledge_dir = {tmp}/knowledge）
    // {tmp}/knowledge/           执行侧数据资产目录（bundles/ + domain_schemas/）
    let tmp = tempfile::tempdir().unwrap();
    let gov_dir = tmp.path().join("governance");
    let rules_dir = tmp.path().join("rules");
    std::fs::create_dir_all(gov_dir.join("domain_schemas")).unwrap();
    let exec_schemas = tmp.path().join("knowledge").join("domain_schemas");
    std::fs::create_dir_all(&exec_schemas).unwrap();
    std::fs::write(
        gov_dir.join("domain_schemas").join("rpsm-scenario.json"),
        SCENARIO_SCHEMA,
    )
    .unwrap();
    std::fs::write(exec_schemas.join("rpsm-scenario.json"), SCENARIO_SCHEMA).unwrap();

    // ================= 治理侧 4 步 =================
    let store = RuleStore::open(gov_dir.join("db.sqlite").to_str().unwrap()).unwrap();

    // 步骤 1a：创建 knowledge 数据集（类型创建时确定，创建后不可变更）
    let ds = RuleDataset {
        dataset_id: "ds-rpsm-assets".into(),
        name: "RPSM 物理场景数据资产".into(),
        description: Some("Q12 端到端演示：rpsm 场景 JSON 作为一等公民数据资产".into()),
        dataset_kind: DatasetKind::Knowledge,
        domain: vec!["rpsm".into()],
        tags: vec!["e2e".into()],
        tenant_id: "org-evorule".into(),
        visibility: Visibility::Private,
        lifecycle: Lifecycle::default(), // Draft
        versioning: Default::default(),  // current = v1
        law_ref: Some(LawRef {
            document_id: "rpsm-scenarios".into(),
            law_version: None,
            effective_from: Some("2026-08-30".into()),
            effective_to: None,
        }),
        version_selection: Some(VersionSelection {
            mode: VersionSelectionMode::AutoByEffectiveDate,
            pinned_version: None,
            pinned_include_patch: None,
        }),
        data_dependencies: None,
        event_schemas: vec![],
        meta: Meta {
            created_at: "2026-08-30T00:00:00Z".into(),
            created_by: "governor".into(),
            updated_at: None,
            updated_by: None,
        },
    };
    store.create_dataset(&ds).unwrap();

    // 步骤 1b：数据条目录入（Draft）——payload 过领域 schema 强校验，resolver 未命中即拒绝
    let scenario_payload = serde_json::json!({
        "scenario_id": "spring-single-particle",
        "gravity": [0.0, -9.81, 0.0],
        "restitution": 1.0,
        "bodies": [{"id": "particle-1", "mass": 1.5}]
    });
    let entry = KnowledgeEntry {
        entry_id: "scn-001".into(),
        dataset_id: "ds-rpsm-assets".into(),
        version: 1,
        status: Some(LifecycleStatus::Draft),
        provenance: Provenance {
            source: "RPSM 实验记录".into(),
            clause: None,
            document_id: None,
            effective_from: None,
            effective_to: None,
            last_verified: None,
            verified_by: None,
        },
        domain: "rpsm".into(),
        tags: vec![],
        payload: scenario_payload.clone(),
        schema_ref: "https://rpsm.evorule.org/schemas/scenario/v1.0.json".into(),
        governance: None,
    };
    store.add_knowledge_entry(&entry).unwrap();

    // 步骤 1c：条目状态迁移（Draft→Candidate→Active，双闸门口径）
    store
        .transition_knowledge_entry_status(
            "ds-rpsm-assets",
            "scn-001",
            LifecycleStatus::Candidate,
            "engineer",
            "t1",
            "评审通过",
        )
        .unwrap();
    store
        .transition_knowledge_entry_status(
            "ds-rpsm-assets",
            "scn-001",
            LifecycleStatus::Active,
            "engineer",
            "t2",
            "生效",
        )
        .unwrap();

    // 步骤 1d：数据集状态迁移 + 独立发布审批（Active→Published，二次确认语义）
    store
        .transition_dataset_status(
            "ds-rpsm-assets",
            LifecycleStatus::Candidate,
            "approver",
            "送审",
            "t3",
        )
        .unwrap();
    store
        .transition_dataset_status(
            "ds-rpsm-assets",
            LifecycleStatus::Active,
            "approver",
            "生效",
            "t4",
        )
        .unwrap();
    store
        .publish_dataset_with_cause("ds-rpsm-assets", "publisher", "t5", "Q12 端到端发布审批")
        .unwrap();

    // ================= 治理侧导出（真实交付格式往返） =================
    let ds_now = store
        .get_dataset("ds-rpsm-assets")
        .unwrap()
        .expect("数据集应存在");
    assert_eq!(ds_now.lifecycle.status, LifecycleStatus::Published);
    let entries = store
        .list_knowledge_entries("ds-rpsm-assets", None)
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].status, Some(LifecycleStatus::Active));

    let tests = BundleTests {
        subset: vec![],
        fixtures: vec![],
        verdict: TestVerdict::Pass,
    };
    let bundle = evorule_rule::bundle::BundleExporter::export_knowledge(
        &ds_now,
        &entries,
        &tests,
        "publisher",
        "2026-08-30T00:00:00Z",
        "instance-1",
        &BTreeMap::new(),
    );
    assert_eq!(bundle.bundle_id, "bundle-ds-rpsm-assets-v1");
    // 交付边界：快照包经序列化（文件/网络传输）后反序列化，内容与哈希签名不变
    let json = serde_json::to_string_pretty(&bundle).unwrap();
    let bundle: DatasetBundle = serde_json::from_str(&json).unwrap();
    assert_eq!(
        bundle.audit.content_hash,
        bundle.compute_content_hash(),
        "序列化往返不得破坏防篡改签名"
    );

    // ================= 执行侧导入 =================
    let core_eval_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../resources/server_eval.json");
    let sessions = SessionApi::new_with_full_config(
        vec![],
        100,
        None,
        false,
        100 * 1024 * 1024,
        false,
        1000,
        1,
        core_eval_path.clone(),
        rules_dir.clone(),
    );
    assert!(sessions.knowledge_load_error().is_none());
    // TCB 合并集基线（导入前）
    let baseline = SessionApi::load_merged_transforms_from_fs(&core_eval_path, &rules_dir)
        .expect("TCB 合并加载不应失败");

    let result = sessions.import_bundle(&bundle, false).await.unwrap();
    assert_eq!(result.dataset_id, "ds-rpsm-assets");
    assert_eq!(result.entry_count, 1);

    // ================= 落盘断言（物理隔离） =================
    let landed = tmp
        .path()
        .join("knowledge")
        .join("bundles")
        .join("bundle-ds-rpsm-assets-v1");
    assert!(
        landed.join("bundle_manifest.json").is_file(),
        "manifest 应落盘"
    );
    assert!(landed.join("scn-001.json").is_file(), "数据条目应落盘");
    assert!(
        !rules_dir.join("bundles").exists(),
        "数据包不得落入 rules_dir（TCB 加载路径物理隔离）"
    );

    // ================= KnowledgeStore 直读命中（W3 消费接口） =================
    // 模拟 rpsm 原生服务：按 (dataset_id, entry_id) 取场景 payload，零转译直读
    let store_snap = sessions.knowledge_store();
    let rec = store_snap
        .get("ds-rpsm-assets", "scn-001")
        .expect("数据条目应可直读");
    assert_eq!(rec.payload["scenario_id"], "spring-single-particle");
    assert_eq!(rec.payload["bodies"][0]["mass"], 1.5);
    assert_eq!(
        rec.schema_ref.as_deref(),
        Some("https://rpsm.evorule.org/schemas/scenario/v1.0.json")
    );
    assert_eq!(rec.bundle_id, "bundle-ds-rpsm-assets-v1");
    assert_eq!(rec.source_version, "v1");
    assert!(sessions.knowledge_load_error().is_none());

    // ================= TCB 合并集不变（负向断言） =================
    let merged = SessionApi::load_merged_transforms_from_fs(&core_eval_path, &rules_dir)
        .expect("TCB 合并加载不应被数据资产影响");
    assert_eq!(merged.len(), baseline.len(), "数据条目不得进入 TCB 合并集");
}

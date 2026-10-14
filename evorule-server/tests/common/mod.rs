//! 集成测试公共夹具。
//!
//! tests/ 集成测试无法访问 crate 内 `#[cfg(test)]` 模块
//! （`api::evidence_fixture`），故此处保留同构独立副本。

// 集成测试保留 unwrap 惯例（C5 unwrap/expect/panic = deny 仅约束生产代码）
#![allow(clippy::unwrap_used, clippy::expect_used)]

/// 进程内 seed 序号（报告文件名唯一化，多测试并行互不覆盖）
static SEED_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 在给定 workspace 元数据库中 seed 一个 closed+PASS 沙盒会话并落盘报告
/// 文件，返回沙盒 ID。in-memory 库自增 ID 从 1 起，空库首个 seed 即得
/// `sandbox:1`，与测试夹具包的引用约定一致。
///
/// 与 crate 内 `api::evidence_fixture::seed_pass_sandbox` 同构：为 import
/// 正路径证据深度校验（存在性 + closed + 报告 failed==0）提供放行面。
/// 报告落盘与 close_sandbox 生产落盘同目录（仓库忽略区
/// ./data/sandbox_reports），文件名带时间戳+序号+pid 保证唯一。
pub(crate) fn seed_pass_sandbox(ws_db: &std::sync::Arc<evorule_workspace::WorkspaceDb>) -> i64 {
    use std::sync::atomic::Ordering;

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let seq = SEED_SEQ.fetch_add(1, Ordering::Relaxed);
    let now_dt = ws_db.get_production_state().unwrap().updated_at;
    let ws_id = format!("ws-evidence-fixture-{ts}-{seq}-{}", std::process::id());
    ws_db
        .insert_workspace(&evorule_workspace::models::WorkspaceRecord {
            id: ws_id.clone(),
            name: "evidence-fixture".into(),
            owner_id: "test:evidence-fixture".into(),
            created_at: now_dt,
            updated_at: now_dt,
            archived_at: None,
            state: evorule_workspace::models::WorkspaceState::Active,
            description: None,
        })
        .unwrap();
    let report_dir = evorule_workspace::sandbox_report_dir()
        .to_string_lossy()
        .to_string();
    std::fs::create_dir_all(&report_dir).unwrap();
    let facts = format!(
        "audit_evidence_fixture_{ts}_{seq}_{}.json",
        std::process::id()
    );
    let sid = ws_db
        .insert_sandbox_session(
            None,
            &ws_id,
            1,
            Some("hash-evidence-fixture"),
            1,
            "test:evidence-fixture",
        )
        .unwrap();
    // 关闭态 + export_path 指向报告目录；报告文件与 generate_test_report
    // 落盘同构（report_<facts basename>.json），failed=0 使引用可作 pass 证据
    let export_path = format!("{report_dir}/{facts}");
    ws_db.close_sandbox_session(sid, &export_path).unwrap();
    std::fs::write(
        format!("{report_dir}/report_{facts}"),
        r#"{"summary": {"total_cases": 1, "passed": 1, "failed": 0, "skipped": 0}}"#,
    )
    .unwrap();
    sid
}

/// 读取 server_eval.json 的 transform 数组并转为 TCB JsonValue（构造期宪法规则集）
///
/// 装载防线整改方案 C：`SessionApi::new_with_full_config` 首参传空 vec 会使导入期
/// 前置预判（predict_import_io_overlaps）的基线播种 zip 截断为空——预判无
/// 权利面基线即 fail-open（已登记整改）。构造传真实 core_eval 消除失明前提。
///
/// 与 fault_recovery_test.rs 的 load_core_eval 同构（该文件直注评估集场景，
/// 此处供 SessionApi 构造场景共用）。
pub(crate) fn load_core_eval_transforms(
    core_eval_path: &std::path::Path,
) -> Vec<evorule_tcb::JsonValue> {
    let json_str = std::fs::read_to_string(core_eval_path)
        .unwrap_or_else(|e| panic!("Failed to read {}: {}", core_eval_path.display(), e));
    let json: serde_json::Value =
        serde_json::from_str(&json_str).expect("Failed to parse core_eval.json");
    // 剥壳+转换收编 rule_loader 唯一通道（[CON-L4-001] 测试夹具零手写语义实现）：
    // 宪法 ctx 过四步门（schema/三门），与生产装载同源
    let ctx = evorule_rule_loader::LoadContext::constitution(&core_eval_path.display().to_string());
    match evorule_rule_loader::load_rule_set(&json, &ctx) {
        Ok(loaded) => loaded.engine_rules,
        Err(e) => panic!("core_eval 夹具装载门禁未过（{}）: {e}", core_eval_path.display()),
    }
}

/// serde_json::Value → evorule_tcb::JsonValue：收编 rule_loader 唯一实现
/// （[CON-L4-001]；本地手写份已删，B 族哈希口径由 loader 键序测试保证）。
#[allow(unused_imports)]
use evorule_rule_loader::serde_to_tcb;

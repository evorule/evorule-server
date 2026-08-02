// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 会话级 governance 功能联合测试
//!
//! 通过 HTTP 层（`tower::ServiceExt::oneshot`）测试 evorule-server 与
//! evorule-governance 高级功能的端到端连通性：
//!
//! 1. **会话审计链** — audit → verify → export → import roundtrip
//! 2. **时间旅行 rewind** — 回退到早期版本，验证 payload 一致性
//! 3. **时间旅行 diff** — 对比两个版本的 payload 差异
//! 4. **会话 fork** — 父子会话状态独立性
//! 5. **因果链查询** — 追溯指定 Fact 的因果链
//!
//! 这些测试加载真实的 `core_eval.json`，
//! 使用 `set` 指令产生可验证的 payload 变更，覆盖 HTTP handler →
//! evorule-governance → evorule-reactor 的完整调用链。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use evorule_governance::auditor::Auditor;
use evorule_governance::metrics::SharedMetrics;
use evorule_governance::shared_facts_log::SharedFactsLog;
use evorule_reactor::Reactor;
use evorule_server::api::server::{AppState, GovernanceApi, GovernanceServer, SessionApi};
use evorule_server::metrics_impl::shared_prometheus_metrics;
use evorule_tcb::JsonValue;
use tower::ServiceExt;

// ===== 辅助函数 =====

/// `serde_json::Value` → `evorule_tcb::JsonValue`（与 main.rs / server.rs 中一致）
fn serde_to_tcb(v: serde_json::Value) -> JsonValue {
    match v {
        serde_json::Value::Null => JsonValue::Null,
        serde_json::Value::Bool(b) => JsonValue::Bool(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                JsonValue::Integer(i)
            } else {
                JsonValue::String(n.to_string())
            }
        }
        serde_json::Value::String(s) => JsonValue::String(s),
        serde_json::Value::Array(arr) => {
            JsonValue::Array(arr.into_iter().map(serde_to_tcb).collect())
        }
        serde_json::Value::Object(obj) => {
            let mut map = BTreeMap::new();
            for (k, val) in obj {
                map.insert(k, serde_to_tcb(val));
            }
            JsonValue::Object(map)
        }
    }
}

/// 加载 `core_eval.json`
fn load_core_eval() -> Vec<JsonValue> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let core_eval_path = manifest_dir.join("../../resources/core_eval.json");
    let json_str = std::fs::read_to_string(&core_eval_path).unwrap_or_else(|e| {
        panic!(
            "Failed to read core_eval.json at {}: {}",
            core_eval_path.display(),
            e
        )
    });
    let json: serde_json::Value =
        serde_json::from_str(&json_str).expect("Failed to parse core_eval.json");
    json.get("transform")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().cloned().map(serde_to_tcb).collect())
        .unwrap_or_default()
}

/// 构造测试用 `AppState`（使用真实 core_eval，纯内存模式无 WAL）
fn make_state() -> AppState {
    let core_eval = load_core_eval();

    let reactor = Reactor::builder(core_eval.clone()).max_rounds(100).build();
    let (tx, _rx, _event_tx, _handle, facts_log) = reactor.spawn();

    let auditor = Auditor::new(facts_log.clone());
    let governance = GovernanceApi::new(tx, facts_log, auditor);
    let sessions = SessionApi::new(core_eval, 100);
    let metrics: SharedMetrics = shared_prometheus_metrics().unwrap();
    let readiness = Arc::new(AtomicBool::new(true));
    let shared_facts = SharedFactsLog::new();

    AppState::new(governance, sessions, metrics, readiness, shared_facts)
}

/// 构造测试用 `Router`（bench 模式：无认证、无限速）
fn make_router(state: &AppState) -> axum::Router {
    GovernanceServer::bench(state.clone(), "0.0.0.0:0".to_string()).build_router()
}

/// 发送 oneshot 请求，返回 `(状态码, 响应体 JSON)`
///
/// 响应体非 JSON 时返回 `Value::Null`。
async fn send(
    state: &AppState,
    method: &str,
    uri: &str,
    body: Option<&str>,
) -> (axum::http::StatusCode, serde_json::Value) {
    let mut builder = axum::http::Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let request = if let Some(b) = body {
        builder.body(axum::body::Body::from(b.to_string())).unwrap()
    } else {
        builder.body(axum::body::Body::empty()).unwrap()
    };
    let response = make_router(state).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// 获取会话当前 FactsLog 版本号
async fn get_version(state: &AppState, session_id: u64) -> u64 {
    let (_, json) = send(
        state,
        "GET",
        &format!("/api/sessions/{session_id}/state"),
        None,
    )
    .await;
    json["version"].as_u64().unwrap_or(0)
}

/// 等待反应器处理完命令（版本号稳定后返回最终版本）
///
/// 提交命令后轮询 state 端点，直到版本号超过 `baseline` 且连续 2 次不变
/// （100ms 稳定窗口），确保 Command + StateTransition + Stable 全部入账。
async fn wait_for_processing(state: &AppState, session_id: u64, baseline: u64) -> u64 {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut last_version = baseline;
    let mut stable_count = 0u32;
    loop {
        let version = get_version(state, session_id).await;
        if version > last_version {
            last_version = version;
            stable_count = 0;
        } else {
            stable_count += 1;
            if stable_count >= 2 && version > baseline {
                return version;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("Timeout waiting for session {session_id} to stabilize above {baseline}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 提交 `set` 指令并等待处理完成，返回稳定后的版本号
async fn submit_set_and_wait(
    state: &AppState,
    session_id: u64,
    attr: &str,
    value: i64,
    baseline: u64,
) -> u64 {
    let body = format!(
        r#"{{"instruction":{{"type":"set","params":{{"attr":"{}","operation":"set","value":{}}}}}}}"#,
        attr, value
    );
    let (status, _) = send(
        state,
        "POST",
        &format!("/api/sessions/{session_id}/command"),
        Some(&body),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "set command should succeed"
    );
    wait_for_processing(state, session_id, baseline).await
}

/// 提交 `set` 指令并等待，返回 `(版本号, fact_id)`
async fn submit_set_get_fact_id(
    state: &AppState,
    session_id: u64,
    attr: &str,
    value: i64,
    baseline: u64,
) -> (u64, u64) {
    let body = format!(
        r#"{{"instruction":{{"type":"set","params":{{"attr":"{}","operation":"set","value":{}}}}}}}"#,
        attr, value
    );
    let (status, json) = send(
        state,
        "POST",
        &format!("/api/sessions/{session_id}/command"),
        Some(&body),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let fact_id = json["fact_id"].as_u64().unwrap();
    let version = wait_for_processing(state, session_id, baseline).await;
    (version, fact_id)
}

// ===== 测试用例 =====

/// 测试 1：会话审计链完整流程（audit → verify → export → import roundtrip）
///
/// 验证 evorule-governance `Auditor` 的 session 级方法经 HTTP 暴露后：
/// - 审计报告包含条目且字段重映射正确（`entry_count` → `fact_count`）
/// - 审计链验证通过
/// - export/import roundtrip 后审计链仍完整
#[tokio::test]
async fn test_session_audit_chain_flow() {
    let state = make_state();

    // 1. 创建会话
    let (status, json) = send(&state, "POST", "/api/sessions", None).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let session_id = json["session_id"].as_u64().unwrap();

    // 2. 提交命令产生审计条目
    let v0 = get_version(&state, session_id).await;
    let _ = submit_set_and_wait(&state, session_id, "counter", 1, v0).await;

    // 3. 查询审计报告（验证字段重映射 entry_count → fact_count）
    let (status, json) = send(
        &state,
        "GET",
        &format!("/api/sessions/{session_id}/audit"),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(json["session_id"], session_id);
    let fact_count = json["fact_count"].as_u64().unwrap_or(0);
    assert!(
        fact_count > 0,
        "审计链应有条目，实际 fact_count={fact_count}"
    );
    assert_eq!(json["verified"], true, "审计链应验证通过");
    assert!(json["entries"].is_array(), "entries 应为数组");

    // 4. 验证审计链完整性
    let (status, json) = send(
        &state,
        "GET",
        &format!("/api/sessions/{session_id}/audit/verify"),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(json["verified"], true);
    assert!(json["fact_count"].as_u64().unwrap_or(0) > 0);

    // 5. 导出审计链
    let (status, export) = send(
        &state,
        "GET",
        &format!("/api/sessions/{session_id}/audit/export"),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(export.is_object(), "导出应为 JSON 对象");

    // 6. 导入回同一会话（roundtrip 验证）
    let import_body = serde_json::to_string(&export).unwrap();
    let (status, json) = send(
        &state,
        "POST",
        &format!("/api/sessions/{session_id}/audit/import"),
        Some(&import_body),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(json["imported"], true, "导入应成功");
    assert_eq!(json["verify_ok"], true, "导入后审计链应验证通过");

    // 7. 导入后再次验证
    let (status, json) = send(
        &state,
        "GET",
        &format!("/api/sessions/{session_id}/audit/verify"),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(json["verified"], true);
}

/// 测试 2：时间旅行 rewind（回退到早期版本，验证 payload）
///
/// 验证 `evorule_governance::time_machine::rewind` 经 HTTP 暴露后
/// 能正确返回指定版本的 payload 快照。
#[tokio::test]
async fn test_session_rewind() {
    let state = make_state();

    // 1. 创建会话
    let (_, json) = send(&state, "POST", "/api/sessions", None).await;
    let session_id = json["session_id"].as_u64().unwrap();

    // 2. set counter=1 → v1
    let v0 = get_version(&state, session_id).await;
    let v1 = submit_set_and_wait(&state, session_id, "counter", 1, v0).await;

    // 3. set counter=42 → v2
    let v2 = submit_set_and_wait(&state, session_id, "counter", 42, v1).await;

    // 4. rewind 到 v1，counter 应为 1
    let (status, json) = send(
        &state,
        "GET",
        &format!("/api/sessions/{session_id}/rewind?version={v1}"),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(json["target_version"], v1);
    assert_eq!(
        json["payload"]["counter"].as_i64(),
        Some(1),
        "rewind 到 v1 时 counter 应为 1"
    );

    // 5. rewind 到 v2，counter 应为 42
    let (status, json) = send(
        &state,
        "GET",
        &format!("/api/sessions/{session_id}/rewind?version={v2}"),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(
        json["payload"]["counter"].as_i64(),
        Some(42),
        "rewind 到 v2 时 counter 应为 42"
    );

    // 6. rewind 到不存在的版本 → 400
    let (status, _) = send(
        &state,
        "GET",
        &format!("/api/sessions/{session_id}/rewind?version=999999"),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
}

/// 测试 3：时间旅行 diff（对比两个版本的 payload 差异）
///
/// 验证 `evorule_governance::time_machine::diff` 经 HTTP 暴露后
/// 能正确识别两个版本间变更的字段。
#[tokio::test]
async fn test_session_diff() {
    let state = make_state();

    let (_, json) = send(&state, "POST", "/api/sessions", None).await;
    let session_id = json["session_id"].as_u64().unwrap();

    // set counter=1 → v1, set counter=99 → v2
    let v0 = get_version(&state, session_id).await;
    let v1 = submit_set_and_wait(&state, session_id, "counter", 1, v0).await;
    let v2 = submit_set_and_wait(&state, session_id, "counter", 99, v1).await;

    // diff v1 → v2，counter 应在 changed 中
    let (status, json) = send(
        &state,
        "GET",
        &format!("/api/sessions/{session_id}/diff?a={v1}&b={v2}"),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(json["from_version"], v1);
    assert_eq!(json["to_version"], v2);

    // changed 是 Vec<(String, Value, Value)>，序列化为 [[field_name, old, new], ...]
    let changed = json["changed"].as_array().expect("changed 应为数组");
    let counter_changed = changed.iter().any(|entry| {
        entry
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_str())
            == Some("counter")
    });
    assert!(
        counter_changed,
        "diff changed 应包含 counter，实际: {changed:?}"
    );
    assert!(json["summary"].is_string(), "summary 应为字符串");
}

/// 测试 4：会话 fork（父子会话状态独立性）
///
/// 验证 `SessionManager::create_session_from_parent_at_version` 经 HTTP 暴露后：
/// - fork 能正确创建子会话
/// - 子会话的修改不影响父会话（状态隔离）
#[tokio::test]
async fn test_session_fork() {
    let state = make_state();

    // 1. 创建父会话 A
    let (_, json) = send(&state, "POST", "/api/sessions", None).await;
    let parent_id = json["session_id"].as_u64().unwrap();

    // 2. 父会话 set counter=1
    let v0 = get_version(&state, parent_id).await;
    let parent_v1 = submit_set_and_wait(&state, parent_id, "counter", 1, v0).await;

    // 3. fork 父会话 at parent_v1
    let (status, json) = send(
        &state,
        "POST",
        &format!("/api/sessions/fork/{parent_id}?version={parent_v1}"),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let child_id = json["session_id"].as_u64().unwrap();
    assert_eq!(json["parent_session_id"], parent_id);
    assert_eq!(json["forked_from_version"], parent_v1);

    // 4. 子会话 set counter=99
    let child_v0 = get_version(&state, child_id).await;
    let _ = submit_set_and_wait(&state, child_id, "counter", 99, child_v0).await;

    // 5. 父会话 counter 仍为 1（独立性验证）
    let (_, parent_state) = send(
        &state,
        "GET",
        &format!("/api/sessions/{parent_id}/state"),
        None,
    )
    .await;
    assert_eq!(
        parent_state["payload"]["counter"].as_i64(),
        Some(1),
        "父会话 counter 应保持 1（不受 fork 后子会话修改影响）"
    );

    // 6. 子会话 counter 为 99
    let (_, child_state) = send(
        &state,
        "GET",
        &format!("/api/sessions/{child_id}/state"),
        None,
    )
    .await;
    assert_eq!(
        child_state["payload"]["counter"].as_i64(),
        Some(99),
        "子会话 counter 应为 99"
    );
}

/// 测试 5：因果链查询
///
/// 验证 `session.causal_chain(FactId)` 经 HTTP 暴露后能正确返回
/// 指定 Fact 的因果链，包含 fact_id / fact_type / content_hash 等字段。
#[tokio::test]
async fn test_session_causal_chain() {
    let state = make_state();

    // 1. 创建会话
    let (_, json) = send(&state, "POST", "/api/sessions", None).await;
    let session_id = json["session_id"].as_u64().unwrap();

    // 2. 提交命令并获取 fact_id
    let v0 = get_version(&state, session_id).await;
    let (_, fact_id) = submit_set_get_fact_id(&state, session_id, "counter", 1, v0).await;

    // 3. 查询因果链
    let (status, json) = send(
        &state,
        "GET",
        &format!("/api/sessions/{session_id}/audit/causal/{fact_id}"),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(json["session_id"], session_id);
    assert_eq!(json["fact_id"], fact_id);

    let chain_length = json["chain_length"].as_u64().unwrap_or(0);
    assert!(
        chain_length > 0,
        "因果链应至少有 1 个条目，实际 chain_length={chain_length}"
    );

    // 链中应包含查询的 fact_id
    let chain = json["chain"].as_array().expect("chain 应为数组");
    let found = chain.iter().any(|e| e["fact_id"] == fact_id);
    assert!(found, "因果链应包含 fact_id={fact_id}");

    // 每个条目应有必要字段
    if let Some(first) = chain.first() {
        assert!(first["fact_type"].is_string(), "条目应有 fact_type 字段");
        assert!(
            first["content_hash"].is_string(),
            "条目应有 content_hash 字段"
        );
        assert!(
            first["logical_time"].is_number(),
            "条目应有 logical_time 字段"
        );
    }
}

/// 测试 6：规则热重载（验证 reload 后新会话使用新规则，旧会话保持不变）
///
/// 验证 `SessionApi::reload_from_disk` 经 HTTP `/api/rules/reload` 暴露后：
/// - 初始状态下规则数 = TCB 宪法规则数（rules_dir 为空）
/// - reload 前创建的旧会话在 reload 后仍可正常访问，状态保持不变（TCB 不可变语义）
/// - reload 后创建的新会话能正常工作
/// - 多次 reload 累积生效，规则数随业务规则文件增减而变化
/// - reload 响应包含正确的 `previous_rules` / `current_rules` 计数
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn test_session_rule_hot_reload() {
    use std::fs;
    use tempfile::TempDir;

    // ===== 1. 准备临时目录：core_eval.json + rules/ =====

    let tmp_dir = TempDir::new().expect("Failed to create temp dir");
    let tmp_path = tmp_dir.path();

    // 复制 TCB 宪法 core_eval.json 到临时目录
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let core_eval_source = manifest_dir.join("../../../evorule/evorule-tcb/core_eval.json");
    let core_eval_content = fs::read_to_string(&core_eval_source).unwrap_or_else(|e| {
        panic!(
            "Failed to read core_eval.json at {}: {}",
            core_eval_source.display(),
            e
        )
    });
    let core_eval_path = tmp_path.join("core_eval.json");
    fs::write(&core_eval_path, &core_eval_content).expect("Failed to write core_eval.json");

    // 创建空的 rules 目录（初始无业务规则）
    let rules_dir = tmp_path.join("rules");
    fs::create_dir(&rules_dir).expect("Failed to create rules dir");

    // ===== 2. 构造 AppState，使用自定义 core_eval_path 和 rules_dir =====

    let core_eval = load_core_eval();
    let tcb_len = core_eval.len();

    let reactor = Reactor::builder(core_eval.clone()).max_rounds(100).build();
    let (tx, _rx, _event_tx, _handle, facts_log) = reactor.spawn();
    let auditor = Auditor::new(facts_log.clone());
    let governance = GovernanceApi::new(tx, facts_log, auditor);

    let sessions = SessionApi::new_with_full_config(
        core_eval,
        100,
        None,              // wal_dir: 纯内存模式
        false,             // wal_fsync
        100 * 1024 * 1024, // max_wal_size_bytes
        false,             // auto_verify
        1000,              // auto_verify_threshold
        1,                 // auto_verify_interval
        core_eval_path.clone(),
        rules_dir.clone(),
    );

    let metrics: SharedMetrics = shared_prometheus_metrics().unwrap();
    let readiness = Arc::new(AtomicBool::new(true));
    let shared_facts = SharedFactsLog::new();
    let state = AppState::new(governance, sessions, metrics, readiness, shared_facts);

    // ===== 3. 创建旧会话（reload 前创建，使用初始规则集）=====

    let (_, json) = send(&state, "POST", "/api/sessions", None).await;
    assert_eq!(json["message"], "Session created");
    let session_old = json["session_id"].as_u64().unwrap();

    // 向旧会话提交命令，产生可验证的状态变更
    let v0_old = get_version(&state, session_old).await;
    let _ = submit_set_and_wait(&state, session_old, "counter", 42, v0_old).await;

    // ===== 4. 第一次热重载：向 rules_dir 添加 1 条业务规则 =====

    let rule_v1 = serde_json::json!({
        "transform": [
            {"type": "set", "params": {"attr": "rule_marker_v1", "operation": "set", "value": 100}}
        ]
    });
    fs::write(rules_dir.join("rule_v1.json"), rule_v1.to_string())
        .expect("Failed to write rule_v1.json");

    // 通过 HTTP POST /api/rules/reload 触发热重载
    let (status, json) = send(&state, "POST", "/api/rules/reload", Some("{}")).await;
    assert_eq!(status, axum::http::StatusCode::OK, "第一次热重载应成功");
    assert_eq!(json["reload_ok"], true, "reload_ok 应为 true");
    assert_eq!(
        json["previous_rules"].as_u64().unwrap() as usize,
        tcb_len,
        "previous_rules 应为 TCB 规则数（{tcb_len}）"
    );
    assert_eq!(
        json["current_rules"].as_u64().unwrap() as usize,
        tcb_len + 1,
        "current_rules 应为 TCB + 1 条业务规则"
    );

    // ===== 5. 创建新会话（reload 后创建，应使用更新后的规则集）=====

    let (_, json) = send(&state, "POST", "/api/sessions", None).await;
    assert_eq!(json["message"], "Session created");
    let session_new = json["session_id"].as_u64().unwrap();
    assert_ne!(session_new, session_old, "新会话 ID 应不同于旧会话");

    // 验证新会话能正常处理命令（新规则集已生效）
    let v0_new = get_version(&state, session_new).await;
    let _ = submit_set_and_wait(&state, session_new, "counter", 99, v0_new).await;

    let (_, state_new) = send(
        &state,
        "GET",
        &format!("/api/sessions/{session_new}/state"),
        None,
    )
    .await;
    assert_eq!(
        state_new["payload"]["counter"].as_i64(),
        Some(99),
        "新会话应能正常处理命令，counter 应为 99"
    );

    // ===== 6. 验证旧会话在 reload 后仍可访问且状态保持（TCB 不可变语义）=====

    let (_, state_old) = send(
        &state,
        "GET",
        &format!("/api/sessions/{session_old}/state"),
        None,
    )
    .await;
    assert_eq!(
        state_old["payload"]["counter"].as_i64(),
        Some(42),
        "旧会话应保持 reload 前的状态，counter 仍为 42（TCB 不可变语义）"
    );

    // ===== 7. 第二次热重载：再添加 1 条业务规则，验证累积生效 =====

    let rule_v2 = serde_json::json!({
        "transform": [
            {"type": "set", "params": {"attr": "rule_marker_v2", "operation": "set", "value": 200}}
        ]
    });
    fs::write(rules_dir.join("rule_v2.json"), rule_v2.to_string())
        .expect("Failed to write rule_v2.json");

    let (status, json) = send(&state, "POST", "/api/rules/reload", Some("{}")).await;
    assert_eq!(status, axum::http::StatusCode::OK, "第二次热重载应成功");
    assert_eq!(json["reload_ok"], true);
    assert_eq!(
        json["previous_rules"].as_u64().unwrap() as usize,
        tcb_len + 1,
        "previous_rules 应为第一次 reload 后的规则数"
    );
    assert_eq!(
        json["current_rules"].as_u64().unwrap() as usize,
        tcb_len + 2,
        "current_rules 应为 TCB + 2 条业务规则"
    );

    // ===== 8. 验证删除规则文件后 reload，规则数相应减少 =====

    fs::remove_file(rules_dir.join("rule_v1.json")).expect("Failed to remove rule_v1.json");

    let (status, json) = send(&state, "POST", "/api/rules/reload", Some("{}")).await;
    assert_eq!(status, axum::http::StatusCode::OK, "第三次热重载应成功");
    assert_eq!(
        json["current_rules"].as_u64().unwrap() as usize,
        tcb_len + 1,
        "删除 rule_v1.json 后，current_rules 应为 TCB + 1"
    );

    // ===== 9. 验证 reload 失败时旧规则保持不变（解析错误不破坏现有规则）=====

    // 写入无效 JSON 文件（应被 load_merged_transforms_from_fs 跳过，不影响其他规则）
    fs::write(rules_dir.join("invalid.json"), "{invalid json content}")
        .expect("Failed to write invalid.json");

    let (status, json) = send(&state, "POST", "/api/rules/reload", Some("{}")).await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "无效 JSON 文件应被跳过，reload 仍应成功"
    );
    assert_eq!(
        json["current_rules"].as_u64().unwrap() as usize,
        tcb_len + 1,
        "无效文件被跳过，current_rules 应保持 TCB + 1"
    );

    // TempDir 在作用域结束时自动清理
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
// 测试代码豁免 L2 clippy (L1 build.rs 门禁已守 panic-prone)。详见 GATE_REFERENCE.md §六(豁免索引)
#![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
//! H5 迁移:端到端集成测试(应用层)
//!
//! 验证端到端流程:反应器 → I/O 订阅者 → IoResponse 回写 → Stable
//! 使用 evorule-io-handlers 的 DbHandler/HttpHandler/MemoryHandler(应用层实现)。
//!
//! # H5 迁移背景
//!
//! 此测试文件从 `evorule-governance/tests/integration_test.rs` 迁出,
//! 因为它依赖具体 I/O handler 实现(策略层),不属于核心 evorule-governance。
//! 现归属 evorule-server crate(应用层入口)。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use evorule_governance::{
    clock::LogicalClock, hash, io_dispatcher::IoDispatcher, io_subscriber::IoSubscriber,
};
use evorule_governance_host::Auditor;
use evorule_io_handlers::{DbHandler, HttpHandler, MemoryHandler};
use evorule_reactor::{Fact, FactId, IoType, Reactor};
use evorule_tcb::JsonValue;
use tokio::time::timeout;

/// serde_json::Value → evorule_tcb::JsonValue：收编 rule_loader 唯一实现
/// （[CON-L4-001]；本地手写份已删）
use evorule_rule_loader::serde_to_tcb;

/// 从本仓 resources/server_eval.json 加载 transform 列表,并附加应用剧本兼容规则
///
/// T8 迁出后 core_eval.json 为最小评估集(原子+控制流+兜底)。本测试为验证
/// IoSubscriber/IoDispatcher 机制层行为,附加旧指令(save_memory/query_db/http_get)
/// 与 call_service 的应用剧本形态 transform 规则——运行宪法由消费方自持(属地原则)。
///
/// 测试辅助：全放行权限门（io:* × 任意主体 Allow）——权限门未装配的订阅者
/// 现按 fail-closed 拒绝分发，本 helper 供仅需 exercising 分发/重试/持久化
/// 语义的测试挂一个显式宽松门。
fn allow_all_gate() -> evorule_governance::permission::PermissionGate {
    use evorule_governance::permission::{
        Effect, PermissionEntry, PermissionState, Resource, ResourceType, Subject,
    };
    let log = std::sync::Arc::new(evorule_governance::shared_facts_log::SharedFactsLog::new());
    let mut entry = PermissionEntry::new(
        "test-allow-all",
        Subject::any(),
        Resource {
            resource_type: ResourceType::IoAction,
            path: "io:*".to_string(),
        },
        Effect::Allow,
    );
    entry.state = PermissionState::Active;
    evorule_governance::permission::PermissionTable::store_entry(&log, &entry, 0)
        .expect("seed allow-all gate");
    evorule_governance::permission::PermissionGate::new(log)
}

fn load_core_eval() -> Vec<JsonValue> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let core_eval_path = manifest_dir.join("../resources/server_eval.json");

    let json_str = std::fs::read_to_string(&core_eval_path)
        .unwrap_or_else(|e| panic!("Failed to read core_eval.json: {}", e));

    let json: serde_json::Value =
        serde_json::from_str(&json_str).expect("Failed to parse core_eval.json");

    // 剥壳+转换收编 rule_loader 唯一通道（[CON-L4-001] 夹具与生产装载同源过门）
    let ctx = evorule_rule_loader::LoadContext::constitution(&core_eval_path.display().to_string());
    let mut transforms: Vec<JsonValue> = match evorule_rule_loader::load_rule_set(&json, &ctx) {
        Ok(loaded) => loaded.engine_rules,
        Err(e) => panic!("server_eval 夹具装载门禁未过: {e}"),
    };

    // 附加兼容规则: save_memory → IoType::save_memory
    transforms.push(serde_to_tcb(serde_json::json!({
        "type": "branch",
        "params": {
            "domain": { "type": "instruction", "instruction_type": "save_memory" },
            "on_true": [
                {
                    "type": "branch",
                    "params": {
                        "domain": { "type": "exists", "path": "__exec__.payload.__io_results__.save_memory" },
                        "on_true": [
                            { "type": "set", "params": { "attr": "memory_result", "operation": "set", "value": "__exec__.payload.__io_results__.save_memory" } },
                            { "type": "set", "params": { "attr": "__exec__.payload.__io_results__.save_memory", "operation": "set", "value": null } }
                        ],
                        "on_false": [
                            { "type": "io_request", "params": { "io_type": "save_memory", "key": "__exec__.instruction.params.key", "value": "__exec__.instruction.params.value" } }
                        ]
                    }
                }
            ]
        }
    })));

    // 附加兼容规则: query_db → IoType::query_db
    transforms.push(serde_to_tcb(serde_json::json!({
        "type": "branch",
        "params": {
            "domain": { "type": "instruction", "instruction_type": "query_db" },
            "on_true": [
                {
                    "type": "branch",
                    "params": {
                        "domain": { "type": "exists", "path": "__exec__.payload.__io_results__.query_db" },
                        "on_true": [
                            { "type": "set", "params": { "attr": "db_result", "operation": "set", "value": "__exec__.payload.__io_results__.query_db" } },
                            { "type": "set", "params": { "attr": "__exec__.payload.__io_results__.query_db", "operation": "set", "value": null } }
                        ],
                        "on_false": [
                            { "type": "io_request", "params": { "io_type": "query_db", "query": "__exec__.instruction.params.query" } }
                        ]
                    }
                }
            ]
        }
    })));

    // 附加兼容规则: http_get → IoType::http_get
    transforms.push(serde_to_tcb(serde_json::json!({
        "type": "branch",
        "params": {
            "domain": { "type": "instruction", "instruction_type": "http_get" },
            "on_true": [
                {
                    "type": "branch",
                    "params": {
                        "domain": { "type": "exists", "path": "__exec__.payload.__io_results__.http_get" },
                        "on_true": [
                            { "type": "set", "params": { "attr": "http_result", "operation": "set", "value": "__exec__.payload.__io_results__.http_get" } },
                            { "type": "set", "params": { "attr": "__exec__.payload.__io_results__.http_get", "operation": "set", "value": null } }
                        ],
                        "on_false": [
                            { "type": "io_request", "params": { "io_type": "http_get", "url": "__exec__.instruction.params.url" } }
                        ]
                    }
                }
            ]
        }
    })));

    // 附加应用剧本规则: call_service 触发/消费(与 save_memory 等同构的最小形态)
    transforms.push(serde_to_tcb(serde_json::json!({
        "type": "branch",
        "params": {
            "domain": { "type": "instruction", "instruction_type": "call_service" },
            "on_true": [
                {
                    "type": "branch",
                    "params": {
                        "domain": { "type": "exists", "path": "__exec__.payload.__io_results__.call_service" },
                        "on_true": [
                            { "type": "set", "params": { "attr": "service_result", "operation": "set", "value": "__exec__.payload.__io_results__.call_service" } },
                            { "type": "set", "params": { "attr": "__exec__.payload.__io_results__.call_service", "operation": "set", "value": null } }
                        ],
                        "on_false": [
                            { "type": "io_request", "params": { "io_type": "call_service", "service_name": "__exec__.instruction.params.service_name", "args?": "__exec__.instruction.params.args" } }
                        ]
                    }
                }
            ]
        }
    })));

    transforms
}

/// 构造 call_service 指令
fn make_call_service_instruction(service_name: &str, args: &str) -> JsonValue {
    let mut params = BTreeMap::new();
    params.insert("service_name".to_string(), JsonValue::string(service_name));
    params.insert("args".to_string(), JsonValue::string(args));
    let mut instr = BTreeMap::new();
    instr.insert("type".to_string(), JsonValue::string("call_service"));
    instr.insert("params".to_string(), JsonValue::Object(params));
    JsonValue::Object(instr)
}

/// 构造 save_memory 指令
fn make_save_memory_instruction(key: &str, value: &str) -> JsonValue {
    let mut params = BTreeMap::new();
    params.insert("key".to_string(), JsonValue::string(key));
    params.insert("value".to_string(), JsonValue::string(value));
    let mut instr = BTreeMap::new();
    instr.insert("type".to_string(), JsonValue::string("save_memory"));
    instr.insert("params".to_string(), JsonValue::Object(params));
    JsonValue::Object(instr)
}

/// 构造 increment 指令
fn make_increment_instruction(attr: &str, delta: i64) -> JsonValue {
    let mut params = BTreeMap::new();
    params.insert("attr".to_string(), JsonValue::string(attr));
    params.insert("delta".to_string(), JsonValue::Integer(delta));
    let mut instr = BTreeMap::new();
    instr.insert("type".to_string(), JsonValue::string("increment"));
    instr.insert("params".to_string(), JsonValue::Object(params));
    JsonValue::Object(instr)
}

/// 创建测试用 IoDispatcher(H5: builder 模式 trait object 动态分发)
///
/// - DbHandler: SQLite 内存数据库
/// - HttpHandler: 默认 client(reqwest::Client 内部 Arc,多实例低开销)
/// - MemoryHandler: 临时目录
///
/// CALL_EXTERNAL/HTTP_GET/CALL_SERVICE 各创建独立 HttpHandler(保持原 dispatch 逻辑)。
async fn create_test_dispatcher(temp_dir: &std::path::Path) -> IoDispatcher {
    let db = DbHandler::connect("sqlite::memory:")
        .await
        .expect("Failed to connect to in-memory SQLite");
    let memory = MemoryHandler::new(temp_dir.to_path_buf());

    IoDispatcher::builder()
        .register(
            IoType::call_external(),
            Arc::new(HttpHandler::new()) as Arc<dyn evorule_reactor::IoHandler>,
        )
        .register(
            IoType::http_get(),
            Arc::new(HttpHandler::new()) as Arc<dyn evorule_reactor::IoHandler>,
        )
        .register(
            IoType::call_service(),
            Arc::new(HttpHandler::new()) as Arc<dyn evorule_reactor::IoHandler>,
        )
        .register(
            IoType::query_db(),
            Arc::new(db) as Arc<dyn evorule_reactor::IoHandler>,
        )
        .register(
            IoType::save_memory(),
            Arc::new(memory) as Arc<dyn evorule_reactor::IoHandler>,
        )
        .build()
}

/// 等待 Stable 事实，返回会话最终 payload（经 FactsLog 快照）
///
///：Stable 事实不再内嵌 final_snapshot（O(n²) 修复），
/// 最终状态由最近一条 StateTransition.new_payload 承担，经
/// `FactsLog::snapshot` 获取。
async fn wait_for_stable(
    rx: &mut evorule_reactor::EventReceiver,
    facts_log: &evorule_reactor::FactsLog,
) -> Option<JsonValue> {
    let stable = timeout(Duration::from_secs(10), async {
        loop {
            match rx.recv().await {
                Ok(fact) => match fact {
                    Fact::Stable { .. } => return Some(()),
                    Fact::Error { message, .. } => panic!("Reactor error: {}", message),
                    _ => {}
                },
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    eprintln!("Warning: lagged by {} events", n);
                }
            }
        }
    })
    .await
    .ok()
    .flatten();
    stable.map(|()| facts_log.snapshot().0)
}

// ===== 测试用例 =====

#[tokio::test]
async fn test_end_to_end_io_subscriber_with_save_memory() {
    let core_eval = load_core_eval();
    let temp_dir = std::env::temp_dir().join("tier2_test_io_subscriber");
    std::fs::create_dir_all(&temp_dir).ok();

    let dispatcher = create_test_dispatcher(&temp_dir).await;
    let subscriber = IoSubscriber::new(dispatcher).with_permission_gate(allow_all_gate());

    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, mut rx, event_tx, _handle, facts_log) = reactor.spawn();

    // 启动 I/O 订阅者
    let sub_rx = event_tx.subscribe();
    let sub_tx = tx.clone();
    tokio::spawn(async move {
        let _ = subscriber.run(sub_rx, sub_tx).await;
    });

    // 提交 save_memory 指令
    let instruction = make_save_memory_instruction("test_key", "hello memory");
    tx.send(Fact::Command {
        id: FactId(1),
        instruction,
    })
    .unwrap();

    // 等待 Stable
    let snapshot = wait_for_stable(&mut rx, &facts_log)
        .await
        .expect("Timed out waiting for Stable");

    // 验证 memory_result 业务字段被正确设置
    assert_eq!(
        snapshot.get("memory_result").and_then(|v| v.as_bool()),
        Some(true),
        "memory_result should be true"
    );

    // 验证 __io_results__ 已被清除（P1-03/v0.3.1：复数容器，按 io_type 隔离，
    // 消费后整体移除；单数 __io_result__ 已不存在，断言单数恒真无意义）
    assert!(
        snapshot.get("__io_results__").is_none(),
        "__io_results__ should be cleared after consumption"
    );

    // 验证文件实际被写入
    let file_path = temp_dir.join("test_key");
    let content = std::fs::read_to_string(&file_path).expect("File should exist");
    assert_eq!(content, "hello memory");

    // 清理
    std::fs::remove_dir_all(&temp_dir).ok();
}

#[tokio::test]
async fn test_end_to_end_save_memory_writes_file() {
    let core_eval = load_core_eval();
    let temp_dir = std::env::temp_dir().join("tier2_test_save_memory");
    std::fs::create_dir_all(&temp_dir).ok();

    let dispatcher = create_test_dispatcher(&temp_dir).await;
    let subscriber = IoSubscriber::new(dispatcher).with_permission_gate(allow_all_gate());

    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, mut rx, event_tx, _handle, facts_log) = reactor.spawn();

    // 启动 I/O 订阅者
    let sub_rx = event_tx.subscribe();
    let sub_tx = tx.clone();
    tokio::spawn(async move {
        let _ = subscriber.run(sub_rx, sub_tx).await;
    });

    // 提交 save_memory 指令
    let instruction = make_save_memory_instruction("test_file.txt", "hello memory");
    tx.send(Fact::Command {
        id: FactId(1),
        instruction,
    })
    .unwrap();

    // 等待 Stable
    let snapshot = wait_for_stable(&mut rx, &facts_log)
        .await
        .expect("Timed out waiting for Stable");

    // 验证 memory_result 业务字段
    assert_eq!(
        snapshot.get("memory_result").and_then(|v| v.as_bool()),
        Some(true),
        "memory_result should be true"
    );

    // 验证文件实际被写入
    let file_path = temp_dir.join("test_file.txt");
    let content = std::fs::read_to_string(&file_path).expect("File should exist");
    assert_eq!(content, "hello memory");

    // 清理
    std::fs::remove_dir_all(&temp_dir).ok();
}

#[tokio::test]
async fn test_io_subscriber_handles_errors() {
    let core_eval = load_core_eval();
    let temp_dir = std::env::temp_dir().join("tier2_test_io_error");
    std::fs::create_dir_all(&temp_dir).ok();

    let dispatcher = create_test_dispatcher(&temp_dir).await;
    let subscriber = IoSubscriber::new(dispatcher).with_permission_gate(allow_all_gate());

    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, mut rx, event_tx, _handle, facts_log) = reactor.spawn();

    // 启动 I/O 订阅者
    let sub_rx = event_tx.subscribe();
    let sub_tx = tx.clone();
    tokio::spawn(async move {
        let _ = subscriber.run(sub_rx, sub_tx).await;
    });

    // 提交 call_service(double, "not_a_number") — 会触发错误
    let instruction = make_call_service_instruction("double", "not_a_number");
    tx.send(Fact::Command {
        id: FactId(1),
        instruction,
    })
    .unwrap();

    // 等待 Stable（即使有错误也应有响应）
    let snapshot = wait_for_stable(&mut rx, &facts_log)
        .await
        .expect("Timed out waiting for Stable");

    // 验证 tool_result 被设置为 Null（因为 double 失败了）
    // 注意：IoResponse 的 error 字段被设置，但 result 为 Null
    assert!(
        snapshot.get("service_result") == Some(&JsonValue::Null)
            || snapshot.get("service_result").is_none(),
        "service_result should be Null or absent on error"
    );

    // 清理
    std::fs::remove_dir_all(&temp_dir).ok();
}

#[tokio::test]
async fn test_multiple_io_requests_sequence() {
    let core_eval = load_core_eval();
    let temp_dir = std::env::temp_dir().join("tier2_test_multi_io");
    std::fs::create_dir_all(&temp_dir).ok();

    let dispatcher = create_test_dispatcher(&temp_dir).await;
    let subscriber = IoSubscriber::new(dispatcher).with_permission_gate(allow_all_gate());

    let reactor = Reactor::builder(core_eval).max_rounds(200).build();
    let (tx, mut rx, event_tx, _handle, facts_log) = reactor.spawn();

    // 启动 I/O 订阅者
    let sub_rx = event_tx.subscribe();
    let sub_tx = tx.clone();
    tokio::spawn(async move {
        let _ = subscriber.run(sub_rx, sub_tx).await;
    });

    // 使用 sequence 指令打包两个 save_memory，避免反应器在第一个 Stable 后退出
    let first_save = make_save_memory_instruction("key1", "value1");
    let second_save = make_save_memory_instruction("key2", "value2");

    let mut seq_params = BTreeMap::new();
    seq_params.insert(
        "instructions".to_string(),
        JsonValue::Array(vec![first_save, second_save]),
    );
    let mut seq_instr = BTreeMap::new();
    seq_instr.insert("type".to_string(), JsonValue::string("sequence"));
    seq_instr.insert("params".to_string(), JsonValue::Object(seq_params));

    tx.send(Fact::Command {
        id: FactId(1),
        instruction: JsonValue::Object(seq_instr),
    })
    .unwrap();

    // 等待 Stable
    let snapshot = wait_for_stable(&mut rx, &facts_log)
        .await
        .expect("Timed out waiting for Stable");

    // 验证 memory_result 为 true（第二个 save_memory 的结果）
    assert_eq!(
        snapshot.get("memory_result").and_then(|v| v.as_bool()),
        Some(true),
        "memory_result should be true"
    );

    // 清理
    std::fs::remove_dir_all(&temp_dir).ok();
}

#[tokio::test]
async fn test_auditor_records_facts_log() {
    let core_eval = load_core_eval();
    let temp_dir = std::env::temp_dir().join("tier2_test_auditor");
    std::fs::create_dir_all(&temp_dir).ok();

    let dispatcher = create_test_dispatcher(&temp_dir).await;
    let subscriber = IoSubscriber::new(dispatcher).with_permission_gate(allow_all_gate());

    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, _rx, _event_tx, _handle, facts_log) = reactor.spawn();

    // 启动 I/O 订阅者
    let event_tx2 = _event_tx.clone();
    let sub_rx = event_tx2.subscribe();
    let sub_tx = tx.clone();
    tokio::spawn(async move {
        let _ = subscriber.run(sub_rx, sub_tx).await;
    });

    // 提交 increment 指令（不需要 I/O）
    tx.send(Fact::Command {
        id: FactId(1),
        instruction: make_increment_instruction("counter", 5),
    })
    .unwrap();

    // 等待反应器处理完成
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 创建审计器并审计
    let mut auditor = Auditor::new(facts_log);
    let count = auditor.audit_new();

    assert!(count > 0, "Auditor should have recorded facts");
    assert!(auditor.verify(), "Audit hash chain should be valid");

    // 验证审计条目
    let entries = auditor.entries();
    assert!(
        entries.iter().any(|e| e.fact_type == "Command"),
        "Should have Command fact in audit"
    );
    assert!(
        entries.iter().any(|e| e.fact_type == "Stable"),
        "Should have Stable fact in audit"
    );

    // 清理
    std::fs::remove_dir_all(&temp_dir).ok();
}

#[tokio::test]
async fn test_logical_clock_monotonic() {
    let clock = LogicalClock::new();

    let t1 = clock.tick();
    let t2 = clock.tick();
    let t3 = clock.tick();

    assert!(t1 < t2, "Clock should be monotonic: {} < {}", t1, t2);
    assert!(t2 < t3, "Clock should be monotonic: {} < {}", t2, t3);
    assert_eq!(clock.current(), t3);
}

#[tokio::test]
async fn test_logical_clock_merge() {
    let clock = LogicalClock::new();

    clock.tick(); // 1
    clock.tick(); // 2

    clock.merge(10); // max(2, 10) + 1 = 11

    let next = clock.tick(); // 12
    assert_eq!(next, 12, "After merge(10), next tick should be 12");
}

#[test]
fn test_hash_chain_verification() {
    let facts = vec![
        Fact::Command {
            id: FactId(1),
            instruction: JsonValue::string("test1"),
        },
        Fact::Stable {
            id: FactId(2),
            version: 1,
        },
    ];

    // P1-05：废弃 verify_hash_chain（恒 true、无断言力），改用 compute_chain_hash 真正验证：
    // ① 非空链哈希 ≠ genesis；② 确定性；③ 篡改任一事实后链哈希必须改变（防篡改断言力）。
    let chain = hash::compute_chain_hash(&facts).unwrap();
    assert_ne!(chain, "genesis", "非空链哈希不应等于 genesis");
    assert_eq!(
        hash::compute_chain_hash(&facts).unwrap(),
        chain,
        "链哈希应确定"
    );

    let mut tampered = facts.clone();
    tampered[0] = Fact::Command {
        id: FactId(1),
        instruction: JsonValue::string("tampered"),
    };
    assert_ne!(
        hash::compute_chain_hash(&tampered).unwrap(),
        chain,
        "篡改事实后链哈希应改变（防篡改）"
    );
}

#[test]
fn test_content_hash_deterministic() {
    let value = JsonValue::string("test content");

    let hash1 = hash::content_hash(&value).unwrap();
    let hash2 = hash::content_hash(&value).unwrap();

    assert_eq!(hash1, hash2, "Content hash should be deterministic");
    assert!(!hash1.is_empty(), "Hash should not be empty");
}

#[test]
fn test_content_hash_different_inputs() {
    let value1 = JsonValue::string("content1");
    let value2 = JsonValue::string("content2");

    let hash1 = hash::content_hash(&value1).unwrap();
    let hash2 = hash::content_hash(&value2).unwrap();

    assert_ne!(
        hash1, hash2,
        "Different inputs should produce different hashes"
    );
}

/// 构造 band_check 指令（Tier 1 立法 v0.7.0：纯判定+标签,开区间 lo<v<hi）
fn make_band_check_instruction(
    value: i64,
    lo: i64,
    hi: i64,
    in_value: &str,
    out_value: &str,
) -> JsonValue {
    let mut params = BTreeMap::new();
    params.insert("value".to_string(), JsonValue::Integer(value));
    params.insert("lo".to_string(), JsonValue::Integer(lo));
    params.insert("hi".to_string(), JsonValue::Integer(hi));
    params.insert("target".to_string(), JsonValue::string("verdict"));
    params.insert("in_value".to_string(), JsonValue::string(in_value));
    params.insert("out_value".to_string(), JsonValue::string(out_value));
    let mut instr = BTreeMap::new();
    instr.insert("type".to_string(), JsonValue::string("band_check"));
    instr.insert("params".to_string(), JsonValue::Object(params));
    JsonValue::Object(instr)
}

/// band_check 立法三例：带内 normal / 带外 slow / 开区间边界（v=lo 判带外）
#[tokio::test]
async fn test_band_check_legislation() {
    for (value, expected) in [(500i64, "normal"), (1500i64, "slow"), (0i64, "slow")] {
        let core_eval = load_core_eval();
        let reactor = Reactor::builder(core_eval).max_rounds(100).build();
        let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();

        let instruction = make_band_check_instruction(value, 0, 1000, "normal", "slow");
        tx.send(Fact::Command {
            id: FactId(1),
            instruction,
        })
        .unwrap();

        let snapshot = wait_for_stable(&mut rx, &facts_log)
            .await
            .expect("Timed out waiting for Stable");

        // 相对路径 target 写 payload.verdict；Stable 快照根即 payload 本体
        let verdict = snapshot.get("verdict").and_then(|v| v.as_str());
        assert_eq!(
            verdict,
            Some(expected),
            "value={value} 应判 verdict={expected}; snapshot={snapshot:?}"
        );
    }
}

/// 构造 externalization_violation 指令（Tier 1 立法 v0.8.0：外判违规事实记录通道，tool_trace 同构）
fn make_externalization_violation_instruction(phrase: &str) -> JsonValue {
    let mut value = BTreeMap::new();
    value.insert(
        "domain".to_string(),
        JsonValue::string("deterministic_compute"),
    );
    value.insert("phrase".to_string(), JsonValue::string(phrase));
    value.insert("request_id".to_string(), JsonValue::string("req-itest-1"));
    value.insert("mode".to_string(), JsonValue::string("observe"));
    let mut params = BTreeMap::new();
    params.insert(
        "attr".to_string(),
        JsonValue::string("meta_compliance.externalization_breach"),
    );
    params.insert("value".to_string(), JsonValue::Object(value));
    let mut instr = BTreeMap::new();
    instr.insert(
        "type".to_string(),
        JsonValue::string("externalization_violation"),
    );
    instr.insert("params".to_string(), JsonValue::Object(params));
    JsonValue::Object(instr)
}

/// externalization_violation 立法三例：违规落链 / 单槽纯覆盖（后写胜）/ 无关指令零波及
#[tokio::test]
async fn test_externalization_violation_legislation() {
    // 例 1：违规事实经新规则落 payload.meta_compliance.externalization_breach
    {
        let core_eval = load_core_eval();
        let reactor = Reactor::builder(core_eval).max_rounds(100).build();
        let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();
        tx.send(Fact::Command {
            id: FactId(1),
            instruction: make_externalization_violation_instruction("经计算"),
        })
        .unwrap();
        let snapshot = wait_for_stable(&mut rx, &facts_log)
            .await
            .expect("Timed out waiting for Stable");
        let breach = snapshot
            .get("meta_compliance")
            .and_then(|m| m.get("externalization_breach"));
        assert_eq!(
            breach
                .and_then(|b| b.get("domain"))
                .and_then(|d| d.as_str()),
            Some("deterministic_compute"),
            "违规事实应落 meta_compliance.externalization_breach; snapshot={snapshot:?}"
        );
        assert_eq!(
            breach
                .and_then(|b| b.get("phrase"))
                .and_then(|p| p.as_str()),
            Some("经计算")
        );
    }

    // 例 2：单槽纯覆盖——两次违规后写胜（历史明细在 Violation 账，payload 只留最新）
    {
        let core_eval = load_core_eval();
        let reactor = Reactor::builder(core_eval).max_rounds(100).build();
        let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();
        tx.send(Fact::Command {
            id: FactId(1),
            instruction: make_externalization_violation_instruction("经计算"),
        })
        .unwrap();
        tx.send(Fact::Command {
            id: FactId(2),
            instruction: make_externalization_violation_instruction("计算得出"),
        })
        .unwrap();
        let snapshot = wait_for_stable(&mut rx, &facts_log)
            .await
            .expect("Timed out waiting for Stable");
        assert_eq!(
            snapshot
                .get("meta_compliance")
                .and_then(|m| m.get("externalization_breach"))
                .and_then(|b| b.get("phrase"))
                .and_then(|p| p.as_str()),
            Some("计算得出"),
            "单槽纯覆盖应后写胜; snapshot={snapshot:?}"
        );
    }

    // 例 3：无关指令不触新规则（普通 set 不产生 meta_compliance 残留）
    {
        let core_eval = load_core_eval();
        let reactor = Reactor::builder(core_eval).max_rounds(100).build();
        let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();
        let mut params = BTreeMap::new();
        params.insert("attr".to_string(), JsonValue::string("plain_field"));
        params.insert("operation".to_string(), JsonValue::string("set"));
        params.insert("value".to_string(), JsonValue::Integer(1));
        let mut instr = BTreeMap::new();
        instr.insert("type".to_string(), JsonValue::string("set"));
        instr.insert("params".to_string(), JsonValue::Object(params));
        tx.send(Fact::Command {
            id: FactId(1),
            instruction: JsonValue::Object(instr),
        })
        .unwrap();
        let snapshot = wait_for_stable(&mut rx, &facts_log)
            .await
            .expect("Timed out waiting for Stable");
        assert_eq!(
            snapshot.get("plain_field").and_then(|v| v.as_i64()),
            Some(1)
        );
        assert!(
            snapshot.get("meta_compliance").is_none(),
            "无关指令不得触外判违规通道; snapshot={snapshot:?}"
        );
    }
}

/// 构造 conservation_check 指令（Tier 1 立法 v0.8.1：守恒不变量三态裁决——
/// 确定性物理服务的守恒测量事实按声明 scale 直传定点 Integer（drift_fp）,
/// 对照声明容差（bound_fp）裁决,规则层零数据变形;三态词汇由立法固定）
fn make_conservation_check_instruction(drift_fp: Option<i64>, bound_fp: i64) -> JsonValue {
    let mut params = BTreeMap::new();
    if let Some(d) = drift_fp {
        params.insert("drift_fp".to_string(), JsonValue::Integer(d));
    }
    params.insert("bound_fp".to_string(), JsonValue::Integer(bound_fp));
    params.insert(
        "target".to_string(),
        JsonValue::string("conservation_verdict"),
    );
    let mut instr = BTreeMap::new();
    instr.insert("type".to_string(), JsonValue::string("conservation_check"));
    instr.insert("params".to_string(), JsonValue::Object(params));
    JsonValue::Object(instr)
}

/// conservation_check 立法六例：pass / violation / 边界 eq（闭区间含等号）/
/// 篡改 bound（压到漂移之下）翻红 / 篡改 drift（抬到容差之上）翻红 /
/// 测量缺失 → undetermined（不适用≠通过,弃权显式留痕）
#[tokio::test]
async fn test_conservation_check_legislation() {
    for (drift_fp, bound_fp, expected) in [
        (Some(481_180i64), 500_000i64, "pass"),
        (Some(600_000i64), 500_000i64, "violation"),
        (Some(500_000i64), 500_000i64, "pass"),
        (Some(481_180i64), 400_000i64, "violation"),
        (Some(520_000i64), 500_000i64, "violation"),
        (None, 500_000i64, "undetermined"),
    ] {
        let core_eval = load_core_eval();
        let reactor = Reactor::builder(core_eval).max_rounds(100).build();
        let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();

        let instruction = make_conservation_check_instruction(drift_fp, bound_fp);
        tx.send(Fact::Command {
            id: FactId(1),
            instruction,
        })
        .unwrap();

        let snapshot = wait_for_stable(&mut rx, &facts_log)
            .await
            .expect("Timed out waiting for Stable");

        // 相对路径 target 写 payload.conservation_verdict;Stable 快照根即 payload 本体
        let verdict = snapshot
            .get("conservation_verdict")
            .and_then(|v| v.as_str());
        assert_eq!(
            verdict,
            Some(expected),
            "drift_fp={drift_fp:?} bound_fp={bound_fp} 应判 {expected}; snapshot={snapshot:?}"
        );
    }
}

/// 构造 CAD 判据指令（v0.9.0 立法：确定性测量事实三态裁决七件套——
/// 事实定点 i64 直传 + bound_fp 阈值，事实缺失经 exists 门显式分派
/// undetermined；违规方向两式：裕度类事实<bound_fp、偏差类 bound_fp<事实，
/// 边界等号一律 pass，规则层零数据变形）
fn make_cad_check_instruction(
    instruction_type: &str,
    fact_param: &str,
    fact_fp: Option<i64>,
    bound_fp: i64,
) -> JsonValue {
    let mut params = BTreeMap::new();
    if let Some(f) = fact_fp {
        params.insert(fact_param.to_string(), JsonValue::Integer(f));
    }
    params.insert("bound_fp".to_string(), JsonValue::Integer(bound_fp));
    params.insert("target".to_string(), JsonValue::string("cad_verdict"));
    let mut instr = BTreeMap::new();
    instr.insert("type".to_string(), JsonValue::string(instruction_type));
    instr.insert("params".to_string(), JsonValue::Object(params));
    JsonValue::Object(instr)
}

/// CAD 判据三态公用执行体：装载部署面规则集（过装载门禁）→ 发指令 →
/// 等 Stable → 回读 target（相对路径写 payload.cad_verdict，快照根即 payload）
async fn run_cad_verdict(instruction: JsonValue) -> String {
    let core_eval = load_core_eval();
    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();
    tx.send(Fact::Command {
        id: FactId(1),
        instruction,
    })
    .unwrap();
    let snapshot = wait_for_stable(&mut rx, &facts_log)
        .await
        .expect("Timed out waiting for Stable");
    snapshot
        .get("cad_verdict")
        .and_then(|v| v.as_str())
        .expect("cad_verdict 缺失——判据指令未被规则消费")
        .to_string()
}

/// clearance_check 立法五例（裕度类：事实<bound_fp → violation）：
/// 裕度为正 pass / 边界等号 pass（闭区间含等号）/ 裕度为负 violation /
/// 下限抬高尚不达标 violation / 测量缺失 → undetermined
#[tokio::test]
async fn test_clearance_check_legislation() {
    for (margin_fp, bound_fp, expected) in [
        (Some(5_000i64), 0i64, "pass"),
        (Some(0), 0, "pass"),
        (Some(-200), 0, "violation"),
        (Some(5_000), 8_000, "violation"),
        (None, 0, "undetermined"),
    ] {
        let verdict = run_cad_verdict(make_cad_check_instruction(
            "clearance_check",
            "margin_fp",
            margin_fp,
            bound_fp,
        ))
        .await;
        assert_eq!(
            verdict, expected,
            "margin_fp={margin_fp:?} bound_fp={bound_fp} 应判 {expected}"
        );
    }
}

/// safety_factor_check 立法四例（裕度类，定点 1e6 相对量）：
/// 系数达标 pass / 边界等号 pass / 系数不足 violation / 测量缺失 undetermined
#[tokio::test]
async fn test_safety_factor_check_legislation() {
    for (ratio_fp, bound_fp, expected) in [
        (Some(2_000_000i64), 1_500_000i64, "pass"),
        (Some(1_500_000), 1_500_000, "pass"),
        (Some(1_200_000), 1_500_000, "violation"),
        (None, 1_500_000, "undetermined"),
    ] {
        let verdict = run_cad_verdict(make_cad_check_instruction(
            "safety_factor_check",
            "ratio_fp",
            ratio_fp,
            bound_fp,
        ))
        .await;
        assert_eq!(
            verdict, expected,
            "ratio_fp={ratio_fp:?} bound_fp={bound_fp} 应判 {expected}"
        );
    }
}

/// stress_check 立法四例（符号判定类：bound_fp=0 约定，裕量<0 → violation）：
/// 裕量为正 pass / 恰好用尽（等号）pass / 超许用应力 violation / 测量缺失 undetermined
#[tokio::test]
async fn test_stress_check_legislation() {
    for (headroom_fp, bound_fp, expected) in [
        (Some(50i64), 0i64, "pass"),
        (Some(0), 0, "pass"),
        (Some(-10), 0, "violation"),
        (None, 0, "undetermined"),
    ] {
        let verdict = run_cad_verdict(make_cad_check_instruction(
            "stress_check",
            "headroom_fp",
            headroom_fp,
            bound_fp,
        ))
        .await;
        assert_eq!(
            verdict, expected,
            "headroom_fp={headroom_fp:?} bound_fp={bound_fp} 应判 {expected}"
        );
    }
}

/// mass_budget_check 立法四例（符号判定类：bound_fp=0 约定，余量<0 → violation）：
/// 预算内 pass / 恰好用尽 pass / 超重 violation / 测量缺失 undetermined
#[tokio::test]
async fn test_mass_budget_check_legislation() {
    for (margin_fp, bound_fp, expected) in [
        (Some(120i64), 0i64, "pass"),
        (Some(0), 0, "pass"),
        (Some(-45), 0, "violation"),
        (None, 0, "undetermined"),
    ] {
        let verdict = run_cad_verdict(make_cad_check_instruction(
            "mass_budget_check",
            "margin_fp",
            margin_fp,
            bound_fp,
        ))
        .await;
        assert_eq!(
            verdict, expected,
            "margin_fp={margin_fp:?} bound_fp={bound_fp} 应判 {expected}"
        );
    }
}

/// thermal_growth_check 立法四例（偏差类：bound_fp<事实 → violation）：
/// 偏差在容差内 pass / 边界等号 pass / 偏差越容差 violation / 测量缺失 undetermined
#[tokio::test]
async fn test_thermal_growth_check_legislation() {
    for (dev_fp, bound_fp, expected) in [
        (Some(12i64), 50i64, "pass"),
        (Some(50), 50, "pass"),
        (Some(51), 50, "violation"),
        (None, 50, "undetermined"),
    ] {
        let verdict = run_cad_verdict(make_cad_check_instruction(
            "thermal_growth_check",
            "dev_fp",
            dev_fp,
            bound_fp,
        ))
        .await;
        assert_eq!(
            verdict, expected,
            "dev_fp={dev_fp:?} bound_fp={bound_fp} 应判 {expected}"
        );
    }
}

/// tolerance_stack_check 立法四例（偏差类，RSS 累积 µm）：
/// 累积在界内 pass / 边界等号 pass / 累积越界 violation / 测量缺失 undetermined
#[tokio::test]
async fn test_tolerance_stack_check_legislation() {
    for (stack_fp, bound_fp, expected) in [
        (Some(38i64), 50i64, "pass"),
        (Some(50), 50, "pass"),
        (Some(62), 50, "violation"),
        (None, 50, "undetermined"),
    ] {
        let verdict = run_cad_verdict(make_cad_check_instruction(
            "tolerance_stack_check",
            "stack_fp",
            stack_fp,
            bound_fp,
        ))
        .await;
        assert_eq!(
            verdict, expected,
            "stack_fp={stack_fp:?} bound_fp={bound_fp} 应判 {expected}"
        );
    }
}

/// interference_check 立法四例（偏差类符号判定：bound_fp=0 约定，
/// 任何正干涉体积 → violation）：零干涉 pass / 正干涉体积 violation ×2 /
/// 测量缺失 undetermined
#[tokio::test]
async fn test_interference_check_legislation() {
    for (overlap_fp, bound_fp, expected) in [
        (Some(0i64), 0i64, "pass"),
        (Some(1), 0, "violation"),
        (Some(99_999), 0, "violation"),
        (None, 0, "undetermined"),
    ] {
        let verdict = run_cad_verdict(make_cad_check_instruction(
            "interference_check",
            "overlap_fp",
            overlap_fp,
            bound_fp,
        ))
        .await;
        assert_eq!(
            verdict, expected,
            "overlap_fp={overlap_fp:?} bound_fp={bound_fp} 应判 {expected}"
        );
    }
}

/// 构造 equilibrium_gate 指令（Tier 1 立法 v0.11.0：立法面引用特征 id，
/// coverage_ratio/net_intake_rate 用注册名，net_intake_rate 为判据①证据参数）
#[allow(clippy::too_many_arguments)]
fn make_equilibrium_gate_instruction(
    in_rate: i64,
    digest_rate: i64,
    net_intake_rate: i64,
    alert_free_windows: i64,
    coverage_ratio: i64,
    certified_streak: i64,
    alert_free_windows_required: i64,
    coverage_min: i64,
    certified_streak_min: i64,
) -> JsonValue {
    let mut params = BTreeMap::new();
    params.insert("in_rate".to_string(), JsonValue::Integer(in_rate));
    params.insert("digest_rate".to_string(), JsonValue::Integer(digest_rate));
    params.insert(
        "net_intake_rate".to_string(),
        JsonValue::Integer(net_intake_rate),
    );
    params.insert(
        "alert_free_windows".to_string(),
        JsonValue::Integer(alert_free_windows),
    );
    params.insert(
        "coverage_ratio".to_string(),
        JsonValue::Integer(coverage_ratio),
    );
    params.insert(
        "certified_streak".to_string(),
        JsonValue::Integer(certified_streak),
    );
    params.insert(
        "alert_free_windows_required".to_string(),
        JsonValue::Integer(alert_free_windows_required),
    );
    params.insert("coverage_min".to_string(), JsonValue::Integer(coverage_min));
    params.insert(
        "certified_streak_min".to_string(),
        JsonValue::Integer(certified_streak_min),
    );
    let mut instr = BTreeMap::new();
    instr.insert("type".to_string(), JsonValue::string("equilibrium_gate"));
    instr.insert("params".to_string(), JsonValue::Object(params));
    JsonValue::Object(instr)
}

/// gate 裁决读回（立法 v0.12.0 起主契约）：返回（判定原词, 归因, 契约 schema_version）。
async fn run_equilibrium_gate_verdict(
    instr: JsonValue,
) -> (Option<String>, Option<String>, Option<String>) {
    let core_eval = load_core_eval();
    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();
    tx.send(Fact::Command {
        id: FactId(1),
        instruction: instr,
    })
    .unwrap();
    let snapshot = wait_for_stable(&mut rx, &facts_log)
        .await
        .expect("Timed out waiting for Stable");
    (
        snapshot
            .get("equilibrium")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        snapshot
            .get("equilibrium_cause")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        snapshot
            .get("equilibrium_schema_version")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    )
}

/// equilibrium_gate 立法场景（v0.12.0 接管主契约）：黄金向量（等号边界/
/// 单判据失稳归因）+ 三态 undetermined（缺参弃权留痕，不再伪装失稳）
/// + schema_version 契约字段逐臂在写。v0.10.0 check 同构向量已并入本测试
///（check 规则整体退役，黄金向量由 gate 一并守护）。
#[tokio::test]
async fn test_equilibrium_gate_legislation() {
    // 场景 1：四判据全部恰在等号边界 -> stable（与 check 黄金向量同参同判）
    let (verdict, cause, schema) = run_equilibrium_gate_verdict(make_equilibrium_gate_instruction(
        100, 100, 0, 5, 90, 3, 5, 90, 3,
    ))
    .await;
    assert_eq!(
        verdict.as_deref(),
        Some("stable"),
        "四判据全等号边界应判 stable; cause={cause:?}"
    );
    assert_eq!(cause.as_deref(), Some("none"));
    assert_eq!(schema.as_deref(), Some("1"), "stable 臂应写 schema_version");

    // 场景 2：in_rate > digest_rate -> unstable + 同款归因（双跑一致）
    let (verdict, cause, schema) = run_equilibrium_gate_verdict(make_equilibrium_gate_instruction(
        120, 100, 20, 5, 90, 3, 5, 90, 3,
    ))
    .await;
    assert_eq!(verdict.as_deref(), Some("unstable"));
    assert_eq!(cause.as_deref(), Some("in_rate_exceeds_digest_rate"));
    assert_eq!(schema.as_deref(), Some("1"));

    // 场景 3：coverage_ratio 低于下限（特征 id 参数面）-> unstable + 归因
    let (verdict, cause, _) = run_equilibrium_gate_verdict(make_equilibrium_gate_instruction(
        100, 100, 0, 5, 800, 3, 5, 900, 3,
    ))
    .await;
    assert_eq!(verdict.as_deref(), Some("unstable"));
    assert_eq!(cause.as_deref(), Some("coverage_below_minimum"));

    // 场景 4：缺观测（去掉 coverage_ratio 参）-> undetermined 弃权留痕
    //（v0.11.0 语义改进：与 check 的 fail-closed unstable 差异面，勿混同）
    let mut instr = make_equilibrium_gate_instruction(100, 100, 0, 5, 90, 3, 5, 90, 3);
    if let JsonValue::Object(ref mut m) = instr {
        if let Some(JsonValue::Object(ref mut p)) = m.get_mut("params") {
            p.remove("coverage_ratio");
        }
    }
    let (verdict, cause, schema) = run_equilibrium_gate_verdict(instr).await;
    assert_eq!(verdict.as_deref(), Some("undetermined"));
    assert_eq!(cause.as_deref(), Some("missing_observation"));
    assert_eq!(
        schema.as_deref(),
        Some("1"),
        "undetermined 臂也应写 schema_version（fail-visible 留痕）"
    );
}

// ============================================================================
// 均衡观测演练（接线 e2e）：窗结算任务发射 + 规则层判定 + 读回视图
// ============================================================================
//
// 四场景一体（共享进程级窗计数即同一连续演练面，断言全部用相对口径）：
// A 缺观测 fail-closed → B 供给扰动失稳归因→消化补齐弛豫回稳 →
// C 警情注入→无警连计清零→连续 N 窗弛豫实测 → D 配额风暴反例。

use evorule_server::api::server::GovernanceApi;
use evorule_server::equilibrium_obs::{self, EquilibriumEvaluatorConfig, EquilibriumView};

/// 装配演练反应器：单反应器 + GovernanceApi（事件流后台排空防广播积压；
/// spawn 消费 reactor 自持运行任务，无需额外保活）。
fn spawn_drill_reactor() -> GovernanceApi {
    let core_eval = load_core_eval();
    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();
    tokio::spawn(async move {
        while let Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) = rx.recv().await
        {
            // drain: 防广播通道积压；Closed 即退出
        }
    });
    let auditor = Auditor::new(facts_log.clone());
    GovernanceApi::new(tx, facts_log, auditor)
}

/// 轮询等待视图满足条件（1s 窗 × 20s 上限；超时携带末值 panic 便于归因）。
async fn wait_for_view(what: &str, cond: impl Fn(&EquilibriumView) -> bool) -> EquilibriumView {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let v = equilibrium_obs::view();
        if cond(&v) {
            return v;
        }
        if std::time::Instant::now() >= deadline {
            panic!("equilibrium drill timeout waiting for {what}; last view = {v:?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn test_equilibrium_drill_end_to_end() {
    // ---- 场景 A：缺观测 fail-closed——零观测窗结算不可宣称稳态 ----
    let api = spawn_drill_reactor();
    let evaluator = equilibrium_obs::spawn_equilibrium_evaluator(
        api.clone(),
        EquilibriumEvaluatorConfig {
            window_secs: 1,
            alert_free_windows_required: 1,
            coverage_min: 900,
            certified_streak_min: 3,
        },
    );
    let v = wait_for_view("fail-closed verdict", |v| {
        v.stable == Some(false) && v.last_cause.as_deref().is_some_and(|c| c != "none")
    })
    .await;
    assert!(
        matches!(
            v.last_cause.as_deref(),
            Some("coverage_below_minimum") | Some("certified_streak_below_minimum")
        ),
        "缺观测应落在判据③/④ fail-closed 归因，got {v:?}"
    );
    evaluator.abort();

    // ---- 场景 B：供给扰动（供过于求）→ 失稳归因 → 消化补齐 → 弛豫回稳 ----
    let api = spawn_drill_reactor();
    let evaluator = equilibrium_obs::spawn_equilibrium_evaluator(
        api.clone(),
        EquilibriumEvaluatorConfig {
            window_secs: 1,
            alert_free_windows_required: 2,
            coverage_min: 0,
            certified_streak_min: 1,
        },
    );
    let eq = equilibrium_obs::shared();
    eq.record_coverage(1000);
    eq.record_certificate(true);
    for _ in 0..5 {
        eq.record_proposal();
    }
    let v = wait_for_view("supply-surge verdict", |v| {
        v.last_cause.as_deref() == Some("in_rate_exceeds_digest_rate")
    })
    .await;
    assert_eq!(v.stable, Some(false), "供给扰动应失稳，got {v:?}");
    for _ in 0..5 {
        eq.record_digested();
    }
    let v = wait_for_view("relaxed verdict", |v| v.stable == Some(true)).await;
    assert_eq!(
        v.last_cause.as_deref(),
        Some("none"),
        "弛豫后应归因 none，got {v:?}"
    );
    evaluator.abort();

    // ---- 场景 C：警情注入 → 无警连计清零 → 连续 3 窗弛豫回稳（弛豫窗数实测）----
    let api = spawn_drill_reactor();
    let evaluator = equilibrium_obs::spawn_equilibrium_evaluator(
        api.clone(),
        EquilibriumEvaluatorConfig {
            window_secs: 1,
            alert_free_windows_required: 3,
            coverage_min: 0,
            certified_streak_min: 1,
        },
    );
    equilibrium_obs::record_alert();
    equilibrium_obs::record_alert();
    let v = wait_for_view("alert verdict", |v| {
        v.last_cause.as_deref() == Some("alert_free_windows_below_required")
    })
    .await;
    assert_eq!(v.stable, Some(false), "警情窗应失稳清零无警连计，got {v:?}");
    let relaxed_at = std::time::Instant::now();
    let v = wait_for_view("alert-relaxed verdict", |v| v.stable == Some(true)).await;
    assert_eq!(v.last_cause.as_deref(), Some("none"));
    let relax_secs = relaxed_at.elapsed().as_secs_f64();
    eprintln!("弛豫实测：警情清零后 {relax_secs:.1}s 回稳（阈值 3 窗 x 1s 窗长）");
    assert!(
        (2.0..=4.5).contains(&relax_secs),
        "弛豫耗时应对齐 3 窗阈值（窗长 1s），实测 {relax_secs:.1}s"
    );
    evaluator.abort();

    // ---- 场景 D：配额风暴反例——限流降级生效 + 供给风暴后均衡回归 ----
    let quota =
        std::sync::Arc::new(evorule_server::tiered_adjudicator::AutoIntegrationQuota::new(3));
    let mut granted = 0;
    for _ in 0..8 {
        if quota.acquire() {
            granted += 1;
        } else {
            break;
        }
    }
    assert_eq!(granted, 3, "自动集成配额应在前 3 笔放行后降级");
    assert_eq!(quota.used(), 3);

    let api = spawn_drill_reactor();
    let evaluator = equilibrium_obs::spawn_equilibrium_evaluator(
        api.clone(),
        EquilibriumEvaluatorConfig {
            window_secs: 1,
            alert_free_windows_required: 1,
            coverage_min: 0,
            certified_streak_min: 1,
        },
    );
    let eq = equilibrium_obs::shared();
    for _ in 0..50 {
        eq.record_proposal();
    }
    let v = wait_for_view("storm verdict", |v| {
        v.last_cause.as_deref() == Some("in_rate_exceeds_digest_rate")
    })
    .await;
    assert_eq!(
        v.stable,
        Some(false),
        "供给风暴应呈现失稳（观测不拦截），got {v:?}"
    );
    for _ in 0..50 {
        eq.record_digested();
    }
    let v = wait_for_view("storm-relaxed verdict", |v| v.stable == Some(true)).await;
    assert_eq!(
        v.last_cause.as_deref(),
        Some("none"),
        "风暴消化后均衡应回归，got {v:?}"
    );
    evaluator.abort();
}

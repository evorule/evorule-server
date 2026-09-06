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
    auditor::Auditor, clock::LogicalClock, hash, io_dispatcher::IoDispatcher,
    io_subscriber::IoSubscriber,
};
use evorule_io_handlers::{DbHandler, HttpHandler, MemoryHandler};
use evorule_reactor::{Fact, FactId, IoType, Reactor};
use evorule_tcb::JsonValue;
use tokio::time::timeout;

/// 将 serde_json::Value 转换为 evorule_tcb::JsonValue
fn serde_to_tcb(v: serde_json::Value) -> JsonValue {
    match v {
        serde_json::Value::Null => JsonValue::Null,
        serde_json::Value::Bool(b) => JsonValue::Bool(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                JsonValue::Integer(i)
            } else {
                JsonValue::String(n.to_string().into())
            }
        }
        serde_json::Value::String(s) => JsonValue::String(s.into()),
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

/// 从本仓 resources/server_eval.json 加载 transform 列表,并附加应用剧本兼容规则
///
/// T8 迁出后 core_eval.json 为最小评估集(原子+控制流+兜底)。本测试为验证
/// IoSubscriber/IoDispatcher 机制层行为,附加旧指令(save_memory/query_db/http_get)
/// 与 call_service 的应用剧本形态 transform 规则——运行宪法由消费方自持(属地原则)。
fn load_core_eval() -> Vec<JsonValue> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let core_eval_path = manifest_dir.join("../resources/server_eval.json");

    let json_str = std::fs::read_to_string(&core_eval_path)
        .unwrap_or_else(|e| panic!("Failed to read core_eval.json: {}", e));

    let json: serde_json::Value =
        serde_json::from_str(&json_str).expect("Failed to parse core_eval.json");

    let mut transforms: Vec<JsonValue> = json
        .get("transform")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().cloned().map(serde_to_tcb).collect())
        .unwrap_or_default();

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
    let subscriber = IoSubscriber::new(dispatcher);

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
    let subscriber = IoSubscriber::new(dispatcher);

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
    let subscriber = IoSubscriber::new(dispatcher);

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
    let subscriber = IoSubscriber::new(dispatcher);

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
    let subscriber = IoSubscriber::new(dispatcher);

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

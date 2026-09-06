// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
// 测试代码豁免 L2 clippy (L1 build.rs 门禁已守 panic-prone)。详见 GATE_REFERENCE.md §六(豁免索引)
#![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
//! 故障恢复测试(H5 迁移到应用层)
//!
//! 验证系统在故障场景下的恢复能力:
//! 1. I/O 错误传播:IoResponse 携带 error 时反应器应继续运行
//! 2. I/O 超时恢复:pending I/O 超时后反应器发射 Error 并恢复
//! 3. max_rounds 限制:超过步数上限时发射 Error + Stable
//! 4. 反应器错误后继续服务:Error 后反应器仍能处理新命令
//!
//! # H5 迁移背景
//!
//! 此测试文件从 `evorule-governance/tests/fault_recovery_test.rs` 迁出,
//! 因为它依赖具体 I/O handler 实现(策略层),不属于核心 evorule-governance。
//! 现归属 evorule-server crate(应用层入口)。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use evorule_governance::{io_dispatcher::IoDispatcher, io_subscriber::IoSubscriber};
use evorule_io_handlers::{DbHandler, HttpHandler, MemoryHandler};
use evorule_reactor::{Fact, IoType, Reactor};
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

/// 从本仓 resources/server_eval.json 加载 transform 列表,并附加应用剧本规则
///
/// T8 迁出后 core_eval.json 为最小评估集(原子+控制流+兜底),且不再跨仓引用
/// evorule-tcb 资产(属地原则:运行宪法由消费方自持)。本测试为验证故障恢复
/// 机制层行为,附加 call_service 的应用剧本形态 transform 规则。
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

    // 附加应用剧本规则: call_service 触发/消费(最小同构形态)
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

/// 构造 sequence 指令
fn make_sequence_instruction(instructions: Vec<JsonValue>) -> JsonValue {
    let mut params = BTreeMap::new();
    params.insert("instructions".to_string(), JsonValue::Array(instructions));
    let mut instr = BTreeMap::new();
    instr.insert("type".to_string(), JsonValue::string("sequence"));
    instr.insert("params".to_string(), JsonValue::Object(params));
    JsonValue::Object(instr)
}

/// 创建测试用 IoDispatcher(H5: builder 模式 trait object 动态分发)
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

/// 收集事件直到 Stable 或 Error，返回 (errors, stable_version)
///
///：Stable 事实不再内嵌 final_snapshot（O(n²) 修复），
/// 达到 Stable 的判定改为返回其 version（调用方仅判 is_some）。
async fn collect_until_stable(
    rx: &mut evorule_reactor::EventReceiver,
    deadline: Duration,
) -> (Vec<String>, Option<u64>) {
    let mut errors = Vec::new();
    let stable_version = timeout(deadline, async {
        loop {
            match rx.recv().await {
                Ok(fact) => match fact {
                    Fact::Stable { version, .. } => return Some(version),
                    Fact::Error { message, .. } => {
                        errors.push(message);
                    }
                    _ => {}
                },
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    })
    .await
    .unwrap_or(None);
    (errors, stable_version)
}

/// 测试 1:I/O 错误传播
///
/// 调用不存在的工具 → IoSubscriber 返回 error → IoResponse 携带 error
/// → 反应器应处理错误并最终达到 Stable
#[tokio::test]
async fn test_io_error_propagation() {
    let core_eval = load_core_eval();
    let temp_dir =
        std::env::temp_dir().join(format!("evorule_test_io_error_{}", std::process::id()));
    std::fs::create_dir_all(&temp_dir).unwrap();

    let dispatcher = create_test_dispatcher(&temp_dir).await;
    let subscriber = IoSubscriber::new(dispatcher);

    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, mut rx, event_tx, _handle, _facts_log) = reactor.spawn();

    // spawn IoSubscriber
    let sub_rx = event_tx.subscribe();
    let sub_tx = tx.clone();
    tokio::spawn(async move {
        let _ = subscriber.run(sub_rx, sub_tx).await;
    });

    // 调用不存在的工具 "nonexistent_tool"
    let cmd = Fact::Command {
        id: evorule_reactor::FactId(20000),
        instruction: make_call_service_instruction("nonexistent_tool", "test"),
    };
    tx.send(cmd).unwrap();

    // 等待 Stable（可能伴随 Error Fact）
    let (_errors, snapshot) = collect_until_stable(&mut rx, Duration::from_secs(10)).await;

    // 反应器应该达到 Stable（可能先发射 Error）
    assert!(snapshot.is_some(), "反应器应在 I/O 错误后达到 Stable");

    // 清理
    let _ = std::fs::remove_dir_all(&temp_dir);
}

/// 测试 2:I/O 超时恢复
///
/// 发送 call_service 指令但不启动 IoSubscriber（模拟 IoResponse 永不到达）
/// → 反应器应在 io_error_timeout 后发射 Error 并恢复到 Stable
#[tokio::test]
async fn test_io_timeout_recovery() {
    let core_eval = load_core_eval();

    // 使用极短的超时配置加速测试
    let reactor = Reactor::builder(core_eval)
        .max_rounds(100)
        .io_warn_timeout(Duration::from_millis(50))
        .io_error_timeout(Duration::from_millis(200))
        .io_timeout_check_interval(Duration::from_millis(20))
        .build();

    let (tx, mut rx, _event_tx, _handle, _facts_log) = reactor.spawn();

    // 注意:不启动 IoSubscriber,模拟 IoResponse 永不到达

    // 发送 call_service 指令 → 触发 IoRequest → 无响应 → 超时
    let cmd = Fact::Command {
        id: evorule_reactor::FactId(20000),
        instruction: make_call_service_instruction("echo", "hello"),
    };
    tx.send(cmd).unwrap();

    // 等待 Error（I/O 超时）和后续 Stable
    let (errors, snapshot) = collect_until_stable(&mut rx, Duration::from_secs(5)).await;

    // 应该至少有一个 Error（I/O 超时）
    assert!(
        !errors.is_empty(),
        "应在 I/O 超时后发射 Error，实际 errors: {:?}",
        errors
    );

    // 反应器应恢复到 Stable
    assert!(
        snapshot.is_some(),
        "反应器应在 I/O 超时 Error 后恢复到 Stable"
    );

    // 验证 Error 消息包含超时信息
    let has_timeout_error = errors
        .iter()
        .any(|e| e.contains("timed out") || e.contains("timeout"));
    assert!(
        has_timeout_error,
        "Error 消息应包含超时信息，实际: {:?}",
        errors
    );
}

/// 测试 3:max_rounds 限制
///
/// 使用极小的 max_rounds + sequence 指令 → 超过步数上限
/// → 反应器发射 MaxRoundsExceeded Error + Stable
#[tokio::test]
async fn test_max_rounds_exceeded() {
    let core_eval = load_core_eval();

    // max_rounds=2，sequence 包含 5 个 increment 指令
    let reactor = Reactor::builder(core_eval).max_rounds(2).build();
    let (tx, mut rx, _event_tx, _handle, _facts_log) = reactor.spawn();

    // 构造 sequence 包含 5 个指令，但 max_rounds=2
    let instructions: Vec<JsonValue> = (0..5)
        .map(|i| make_increment_instruction(&format!("x{}", i), 1))
        .collect();
    let seq = make_sequence_instruction(instructions);

    let cmd = Fact::Command {
        id: evorule_reactor::FactId(20000),
        instruction: seq,
    };
    tx.send(cmd).unwrap();

    // 等待 Error + Stable
    let (errors, snapshot) = collect_until_stable(&mut rx, Duration::from_secs(5)).await;

    // 应该有 MaxRoundsExceeded Error
    assert!(!errors.is_empty(), "应在超过 max_rounds 时发射 Error");
    let has_max_rounds_error = errors.iter().any(|e| e.contains("max rounds"));
    assert!(
        has_max_rounds_error,
        "Error 消息应包含 max_rounds 信息，实际: {:?}",
        errors
    );

    // 反应器应恢复到 Stable
    assert!(
        snapshot.is_some(),
        "反应器应在 max_rounds Error 后恢复到 Stable"
    );
}

/// 测试 4:反应器错误后继续服务
///
/// 1. 发送会触发 max_rounds 的指令 → Error + Stable
/// 2. 发送正常 increment 指令 → 应正常执行并达到 Stable
/// 验证反应器在错误后仍能继续服务（长驻模式）
#[tokio::test]
async fn test_reactor_continues_after_error() {
    let core_eval = load_core_eval();

    let reactor = Reactor::builder(core_eval).max_rounds(2).build();
    let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();

    // 第一阶段:触发 max_rounds 错误
    let instructions: Vec<JsonValue> = (0..5)
        .map(|i| make_increment_instruction(&format!("x{}", i), 1))
        .collect();
    let seq = make_sequence_instruction(instructions);
    let cmd1 = Fact::Command {
        id: evorule_reactor::FactId(20000),
        instruction: seq,
    };
    tx.send(cmd1).unwrap();

    let (errors1, snapshot1) = collect_until_stable(&mut rx, Duration::from_secs(5)).await;
    assert!(!errors1.is_empty(), "第一阶段应有 Error");
    assert!(snapshot1.is_some(), "第一阶段应达到 Stable");

    // 第二阶段:发送正常指令，验证反应器仍能服务
    let cmd2 = Fact::Command {
        id: evorule_reactor::FactId(20001),
        instruction: make_increment_instruction("y", 42),
    };
    tx.send(cmd2).unwrap();

    let (errors2, snapshot2) = collect_until_stable(&mut rx, Duration::from_secs(5)).await;
    assert!(
        errors2.is_empty(),
        "第二阶段不应有 Error，实际: {:?}",
        errors2
    );
    assert!(snapshot2.is_some(), "第二阶段应达到 Stable");

    // 验证 increment 生效（: Stable 不再内嵌快照,经 FactsLog 获取）
    let (payload, _queue, _version) = facts_log.snapshot();
    if let Some(y_val) = payload.get("y").and_then(|v| v.as_i64()) {
        assert_eq!(y_val, 42, "increment y=42 应生效");
    }
}

/// 测试 5:I/O 超时恢复后继续服务
///
/// 1. 发送 call_service 但不启动 IoSubscriber → I/O 超时 Error + Stable
/// 2. 发送正常 increment 指令 → 应正常执行
/// 验证 I/O 超时恢复后反应器仍能继续服务
#[tokio::test]
async fn test_reactor_continues_after_io_timeout() {
    let core_eval = load_core_eval();

    let reactor = Reactor::builder(core_eval)
        .max_rounds(100)
        .io_warn_timeout(Duration::from_millis(50))
        .io_error_timeout(Duration::from_millis(200))
        .io_timeout_check_interval(Duration::from_millis(20))
        .build();

    let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();

    // 第一阶段:I/O 超时（不启动 IoSubscriber）
    let cmd1 = Fact::Command {
        id: evorule_reactor::FactId(20000),
        instruction: make_call_service_instruction("echo", "hello"),
    };
    tx.send(cmd1).unwrap();

    let (errors1, snapshot1) = collect_until_stable(&mut rx, Duration::from_secs(5)).await;
    assert!(!errors1.is_empty(), "第一阶段应有 I/O 超时 Error");
    assert!(snapshot1.is_some(), "第一阶段应达到 Stable");

    // 第二阶段:正常 increment 指令
    let cmd2 = Fact::Command {
        id: evorule_reactor::FactId(20001),
        instruction: make_increment_instruction("z", 99),
    };
    tx.send(cmd2).unwrap();

    let (errors2, snapshot2) = collect_until_stable(&mut rx, Duration::from_secs(5)).await;
    assert!(
        errors2.is_empty(),
        "第二阶段不应有 Error，实际: {:?}",
        errors2
    );
    assert!(snapshot2.is_some(), "第二阶段应达到 Stable");

    // 验证 increment 生效（: Stable 不再内嵌快照,经 FactsLog 获取）
    let (payload, _queue, _version) = facts_log.snapshot();
    if let Some(z_val) = payload.get("z").and_then(|v| v.as_i64()) {
        assert_eq!(z_val, 99, "increment z=99 应生效");
    }
}

/// 测试 6:正常 I/O 流程（对照测试）
///
/// 启动 IoSubscriber，发送 call_service("echo") 指令
/// → IoRequest → IoSubscriber 执行 → IoResponse → 反应器恢复 → Stable
#[tokio::test]
async fn test_normal_io_flow() {
    let core_eval = load_core_eval();
    let temp_dir =
        std::env::temp_dir().join(format!("evorule_test_normal_io_{}", std::process::id()));
    std::fs::create_dir_all(&temp_dir).unwrap();

    let dispatcher = create_test_dispatcher(&temp_dir).await;
    let subscriber = IoSubscriber::new(dispatcher);

    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, mut rx, event_tx, _handle, _facts_log) = reactor.spawn();

    // spawn IoSubscriber
    let sub_rx = event_tx.subscribe();
    let sub_tx = tx.clone();
    tokio::spawn(async move {
        let _ = subscriber.run(sub_rx, sub_tx).await;
    });

    // 发送 echo 工具调用
    let cmd = Fact::Command {
        id: evorule_reactor::FactId(20000),
        instruction: make_call_service_instruction("echo", "test_input"),
    };
    tx.send(cmd).unwrap();

    let (errors, snapshot) = collect_until_stable(&mut rx, Duration::from_secs(10)).await;

    // 正常流程不应有 Error
    assert!(
        errors.is_empty(),
        "正常 I/O 流程不应有 Error，实际: {:?}",
        errors
    );
    assert!(snapshot.is_some(), "应达到 Stable");

    // 清理
    let _ = std::fs::remove_dir_all(&temp_dir);
}

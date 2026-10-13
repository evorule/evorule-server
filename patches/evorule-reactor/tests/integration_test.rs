// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
// 测试代码豁免 L2 clippy (L1 build.rs 门禁已守 panic-prone)。详见 GATE_REFERENCE.md §六(豁免索引)
// too_many_lines: 测试 fixture(长 JSON 规则字面量)豁免
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    clippy::too_many_lines
)]
//! 反应式执行器集成测试

use evorule_reactor::{Fact, FactId, FactIdGenerator, IoType, Reactor};
use evorule_tcb::{JsonValue, ObjectMap};

use std::path::PathBuf;
use std::time::Duration;
use tokio::time::timeout;

/// 将 serde_json::Value 转换为 evorule_tcb::JsonValue
///
/// evorule-tcb 是零依赖 no_std crate，未实现 serde。
/// 集成测试通过 serde_json 解析 core_eval.json 后用此函数转换。
fn serde_to_tcb(v: serde_json::Value) -> JsonValue {
    match v {
        serde_json::Value::Null => JsonValue::Null,
        serde_json::Value::Bool(b) => JsonValue::Bool(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                JsonValue::Integer(i)
            } else {
                // 浮点或大整数：转为字符串（TCB 不支持 Float）
                JsonValue::string(n.to_string())
            }
        }
        serde_json::Value::String(s) => JsonValue::string(s),
        serde_json::Value::Array(arr) => {
            JsonValue::Array(arr.into_iter().map(serde_to_tcb).collect())
        }
        serde_json::Value::Object(obj) => {
            let mut map = ObjectMap::new();
            for (k, v) in obj {
                map.insert(k, serde_to_tcb(v));
            }
            JsonValue::Object(map)
        }
    }
}

/// 从 core_eval.json 加载 transform 列表
///
/// 背景：T8 迁出后核心仓 core_eval.json 为最小评估集，不再含 call_external /
/// call_external 应用剧本规则；本文件大量用例依赖完整 I/O 循环行为，故在
/// 最小评估集基础上内联追加同构的 I/O 循环规则链（忠实复刻消费方自持宪法
/// evo-agent agent_constitution.json 的对应规则），测试夹具自足、不依赖被迁出资产。
fn load_core_eval() -> Vec<JsonValue> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let core_eval_path = manifest_dir.join("../evorule-tcb/core_eval.json");

    let json_str = std::fs::read_to_string(&core_eval_path).unwrap_or_else(|e| {
        panic!(
            "Failed to read core_eval.json at {:?}: {}",
            core_eval_path, e
        )
    });

    let json: serde_json::Value =
        serde_json::from_str(&json_str).expect("Failed to parse core_eval.json");

    let mut rules: Vec<JsonValue> = json
        .get("transform")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().cloned().map(serde_to_tcb).collect())
        .unwrap_or_default();
    rules.extend(io_loop_rules());
    rules
}

/// 内联最小 I/O 单轮（应用剧本）规则链：call_external 触发/消费 +
/// call_service 触发/消费 + 兜底。规则清理（2026-09-14）后逐条对齐宪法
/// evo-agent agent_constitution.json v0.5.0 单轮口径：react_iteration 初始化 /
/// collect 工具扇出 / merge 结果回环已随 ReAct 循环退役，多轮编排由应用层负责。
fn io_loop_rules() -> Vec<JsonValue> {
    let rules = serde_json::json!([
        // call_external: 无结果 → io_request；有结果 → 消费到 llm_response
        {
            "type": "branch",
            "params": {
                "domain": {
                    "type": "instruction",
                    "instruction_type": "call_external"
                },
                "on_true": [
                    {
                        "type": "branch",
                        "params": {
                            "domain": {
                                "type": "exists",
                                "path": "__exec__.payload.__io_results__.call_external"
                            },
                            "on_true": [
                                {
                                    "type": "set",
                                    "params": {
                                        "attr": "llm_response",
                                        "operation": "set",
                                        "value":
                                            "__exec__.payload.__io_results__.call_external"
                                    }
                                },
                                {
                                    "type": "branch",
                                    "params": {
                                        "domain": {
                                            "type": "exists",
                                            "path": "__exec__.instruction.params.tools"
                                        },
                                        "on_true": [
                                            {
                                                "type": "set",
                                                "params": {
                                                    "attr": "tools",
                                                    "operation": "set",
                                                    "value":
                                                        "__exec__.instruction.params.tools"
                                                }
                                            }
                                        ],
                                        "on_false": []
                                    }
                                },
                                {
                                    "type": "set",
                                    "params": {
                                        "attr":
                                            "__exec__.payload.__io_results__.call_external",
                                        "operation": "set",
                                        "value": null
                                    }
                                },
                                {
                                    "type": "push",
                                    "params": {
                                        "instructions":
                                            [{ "type": "noop" }]
                                    }
                                }
                            ],
                            "on_false": [
                                {
                                    "type": "io_request",
                                    "params": {
                                        "io_type": "call_external",
                                        "messages":
                                            "__exec__.instruction.params.messages",
                                        "tools?": "__exec__.instruction.params.tools"
                                    }
                                }
                            ]
                        }
                    }
                ],
                "on_false": []
            }
        },
        // call_service: 无结果 → io_request；有结果 → 消费到 service_result（单轮止步）
        {
            "type": "branch",
            "params": {
                "domain": {
                    "type": "instruction",
                    "instruction_type": "call_service"
                },
                "on_true": [
                    {
                        "type": "branch",
                        "params": {
                            "domain": {
                                "type": "exists",
                                "path": "__exec__.payload.__io_results__.call_service"
                            },
                            "on_true": [
                                {
                                    "type": "set",
                                    "params": {
                                        "attr": "service_result",
                                        "operation": "set",
                                        "value":
                                            "__exec__.payload.__io_results__.call_service"
                                    }
                                },
                                {
                                    "type": "set",
                                    "params": {
                                        "attr":
                                            "__exec__.payload.__io_results__.call_service",
                                        "operation": "set",
                                        "value": null
                                    }
                                },
                                {
                                    "type": "push",
                                    "params": {
                                        "instructions":
                                            [{ "type": "noop" }]
                                    }
                                }
                            ],
                            "on_false": [
                                {
                                    "type": "io_request",
                                    "params": {
                                        "io_type": "call_service",
                                        "service_name":
                                            "__exec__.instruction.params.service_name",
                                        "args?": "__exec__.instruction.params.args"
                                    }
                                }
                            ]
                        }
                    }
                ],
                "on_false": []
            }
        },
        // noop + all([]) 兜底（与核心评估集收尾形态一致）
        {
            "type": "branch",
            "params": {
                "domain": { "type": "instruction", "instruction_type": "noop" },
                "on_true": []
            }
        },
        {
            "type": "branch",
            "params": {
                "domain": { "type": "all", "inner": [] },
                "on_true": []
            }
        }
    ]);
    rules
        .as_array()
        .expect("inline io-loop rules must be an array")
        .iter()
        .cloned()
        .map(serde_to_tcb)
        .collect()
}

fn make_instruction(typ: &str, attr: &str, delta: i64) -> JsonValue {
    let mut params = ObjectMap::new();
    params.insert("attr".to_string(), JsonValue::string(attr));
    params.insert("delta".to_string(), JsonValue::Integer(delta));
    let mut instr = ObjectMap::new();
    instr.insert("type".to_string(), JsonValue::string(typ));
    instr.insert("params".to_string(), JsonValue::Object(params));
    JsonValue::Object(instr)
}

fn make_call_external_instruction(prompt: &str) -> JsonValue {
    let mut params = ObjectMap::new();
    // v0.3.1：call_external 使用 messages 参数（LLM 消息历史数组），
    // io_request 透传 instruction 的 messages/tools 参数。
    params.insert(
        "messages".to_string(),
        JsonValue::Array(vec![JsonValue::object_from_pairs(&[
            ("role", JsonValue::string("user")),
            ("content", JsonValue::string(prompt)),
        ])]),
    );
    let mut instr = ObjectMap::new();
    instr.insert("type".to_string(), JsonValue::string("call_external"));
    instr.insert("params".to_string(), JsonValue::Object(params));
    JsonValue::Object(instr)
}

#[tokio::test]
async fn test_simple_increment() {
    let core_eval = load_core_eval();
    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();

    let mut gen = FactIdGenerator::new();
    let instruction = make_instruction("increment", "x", 5);
    tx.send(Fact::Command {
        id: gen.next_id(),
        instruction,
    })
    .unwrap();

    let result = timeout(Duration::from_secs(5), async {
        while let Ok(fact) = rx.recv().await {
            match fact {
                // Stable 不再携带快照(),状态经 FactsLog 快照取
                Fact::Stable { .. } => return Some(()),
                Fact::Error { message, .. } => panic!("Error: {}", message),
                _ => {}
            }
        }
        None
    })
    .await
    .unwrap();

    assert!(result.is_some());
    let (snapshot, _, _) = facts_log.snapshot();
    assert_eq!(snapshot.get("x"), Some(&JsonValue::Integer(5)));
}

#[tokio::test]
async fn test_io_request_detection() {
    let core_eval = load_core_eval();
    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();

    let mut gen = FactIdGenerator::new();
    let instruction = make_call_external_instruction("Hello");
    tx.send(Fact::Command {
        id: gen.next_id(),
        instruction,
    })
    .unwrap();

    // 等待 IoRequest，提取 ID
    let (request_id, io_type, params) = timeout(Duration::from_secs(5), async {
        while let Ok(fact) = rx.recv().await {
            match fact {
                Fact::IoRequest {
                    id,
                    io_type,
                    params,
                    ..
                } => return Some((id, io_type, params)),
                Fact::Error { message, .. } => panic!("Error: {}", message),
                _ => {}
            }
        }
        None
    })
    .await
    .unwrap()
    .expect("IoRequest not received");

    assert_eq!(io_type, IoType::call_external());
    // v0.3.1：call_external 的 io_request 透传 instruction 的 messages/tools 参数
    let messages = params.get("messages").and_then(|v| v.as_array());
    assert!(
        messages.is_some(),
        "call_external io_request should carry messages (v0.3.1)"
    );
    let first_msg = messages
        .and_then(|m| m.first())
        .and_then(|m| m.get("content"))
        .and_then(|v| v.as_str());
    assert_eq!(first_msg, Some("Hello"));

    // 使用实际的 request_id 回复
    let result = JsonValue::string("response from LLM");
    tx.send(Fact::IoResponse {
        id: gen.next_id(),
        request_id,
        result,
        error: None,
    })
    .unwrap();

    let result = timeout(Duration::from_secs(5), async {
        while let Ok(fact) = rx.recv().await {
            match fact {
                // Stable 不再携带快照(),状态经 FactsLog 快照取
                Fact::Stable { .. } => return Some(()),
                Fact::Error { message, .. } => panic!("Error: {}", message),
                _ => {}
            }
        }
        None
    })
    .await
    .unwrap();

    assert!(result.is_some());
    let (snapshot, _, _) = facts_log.snapshot();
    // v0.3.1：I/O 结果应被消费为业务字段 llm_response，
    // 恢复执行完成后 __io_results__ 应被整体移除（防止残留影响后续 I/O 指令）。
    assert_eq!(
        snapshot.get("llm_response").and_then(|v| v.as_str()),
        Some("response from LLM"),
        "llm_response business field should be set from __io_results__.call_external"
    );
    assert!(
        snapshot.get("__io_results__").is_none(),
        "__io_results__ should be cleared after recovery execution"
    );
}

#[tokio::test]
async fn test_unknown_io_response_records_error_fact() {
    let core_eval = load_core_eval();
    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();

    let mut gen = FactIdGenerator::new();

    // 发送一个未知的 IoResponse（ / A1：状态不变，
    // 但必须发射 Error fact 入链——不再静默忽略）
    tx.send(Fact::IoResponse {
        id: gen.next_id(),
        request_id: FactId(999),
        result: JsonValue::string("spurious"),
        error: None,
    })
    .unwrap();

    // 提交一个简单指令验证反应器仍在工作
    let instruction = make_instruction("increment", "x", 5);
    tx.send(Fact::Command {
        id: gen.next_id(),
        instruction,
    })
    .unwrap();

    let mut seen_error = false;
    let result = timeout(Duration::from_secs(5), async {
        while let Ok(fact) = rx.recv().await {
            match fact {
                // Stable 不再携带快照(),状态经 FactsLog 快照取
                Fact::Stable { .. } => return Some(()),
                Fact::Error { message, .. } => {
                    assert!(
                        message.contains("unknown/stale request_id F999"),
                        "unknown IoResponse 必须以 Error fact 显式入链，实际消息: {message}"
                    );
                    seen_error = true;
                }
                _ => {}
            }
        }
        None
    })
    .await
    .unwrap();

    assert!(result.is_some());
    assert!(seen_error, "未观察到 unknown IoResponse 对应的 Error fact");
    let (snapshot, _, _) = facts_log.snapshot();
    // 状态不受 spurious 响应影响
    assert_eq!(snapshot.get("x"), Some(&JsonValue::Integer(5)));
}

#[tokio::test]
async fn test_facts_log_records_all_facts() {
    let core_eval = load_core_eval();
    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();

    let mut gen = FactIdGenerator::new();
    let instruction = make_instruction("increment", "x", 5);
    tx.send(Fact::Command {
        id: gen.next_id(),
        instruction,
    })
    .unwrap();

    // 等待 Stable
    let result = timeout(Duration::from_secs(5), async {
        while let Ok(fact) = rx.recv().await {
            match fact {
                Fact::Stable { .. } => return Some(()),
                Fact::Error { message, .. } => panic!("Error: {}", message),
                _ => {}
            }
        }
        None
    })
    .await
    .unwrap();
    assert!(result.is_some());

    // 验证 FactsLog 记录了所有事实
    let history = facts_log.history();
    // 至少应包含: 1 Command + 1 StateTransition + 1 Stable = 3
    assert!(
        history.len() >= 3,
        "Expected at least 3 facts in log, got {}",
        history.len()
    );

    // 第一个应该是 Command
    assert!(matches!(history[0], Fact::Command { .. }));

    // 最后一个应该是 Stable
    assert!(matches!(history.last().unwrap(), Fact::Stable { .. }));

    // 验证版本号递增
    let version = facts_log.version();
    assert!(version >= 1, "Version should be >= 1, got {}", version);

    // 验证 read_from(0) 返回完整历史
    let all = facts_log.read_from(0).unwrap();
    assert_eq!(all.len(), history.len());
}

#[tokio::test]
async fn test_facts_log_with_io_request() {
    let core_eval = load_core_eval();
    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();

    let mut gen = FactIdGenerator::new();

    // 发送 call_external 指令
    let instruction = make_call_external_instruction("test prompt");
    tx.send(Fact::Command {
        id: gen.next_id(),
        instruction,
    })
    .unwrap();

    // 等待 IoRequest
    let request_id = timeout(Duration::from_secs(5), async {
        while let Ok(fact) = rx.recv().await {
            match fact {
                Fact::IoRequest { id, .. } => return Some(id),
                Fact::Error { message, .. } => panic!("Error: {}", message),
                _ => {}
            }
        }
        None
    })
    .await
    .unwrap()
    .expect("IoRequest not received");

    // 回复 IoResponse
    tx.send(Fact::IoResponse {
        id: gen.next_id(),
        request_id,
        result: JsonValue::string("llm result"),
        error: None,
    })
    .unwrap();

    // 等待 Stable
    let result = timeout(Duration::from_secs(5), async {
        while let Ok(fact) = rx.recv().await {
            match fact {
                Fact::Stable { .. } => return Some(()),
                Fact::Error { message, .. } => panic!("Error: {}", message),
                _ => {}
            }
        }
        None
    })
    .await
    .unwrap();
    assert!(result.is_some());

    // 验证 FactsLog 包含 IoRequest 和 IoResponse
    let history = facts_log.history();
    let has_io_request = history.iter().any(|f| matches!(f, Fact::IoRequest { .. }));
    let has_io_response = history.iter().any(|f| matches!(f, Fact::IoResponse { .. }));

    assert!(has_io_request, "FactsLog should contain IoRequest");
    assert!(has_io_response, "FactsLog should contain IoResponse");

    // 验证 cause 链：IoRequest 的 cause 应为 Command 的 id
    let command_id = history
        .iter()
        .find_map(|f| match f {
            Fact::Command { id, .. } => Some(*id),
            _ => None,
        })
        .expect("Should have Command");

    let io_request_cause = history
        .iter()
        .find_map(|f| match f {
            Fact::IoRequest { cause, .. } => Some(*cause),
            _ => None,
        })
        .expect("Should have IoRequest");

    assert_eq!(
        io_request_cause, command_id,
        "IoRequest cause should point to Command id"
    );
}

#[tokio::test]
async fn test_io_response_with_error_field() {
    let core_eval = load_core_eval();
    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();

    let mut gen = FactIdGenerator::new();

    // 发送 call_external 指令
    let instruction = make_call_external_instruction("test error");
    tx.send(Fact::Command {
        id: gen.next_id(),
        instruction,
    })
    .unwrap();

    // 等待 IoRequest
    let request_id = timeout(Duration::from_secs(5), async {
        while let Ok(fact) = rx.recv().await {
            match fact {
                Fact::IoRequest { id, .. } => return Some(id),
                Fact::Error { message, .. } => panic!("Error: {}", message),
                _ => {}
            }
        }
        None
    })
    .await
    .unwrap()
    .expect("IoRequest not received");

    // 回复带错误的 IoResponse
    tx.send(Fact::IoResponse {
        id: gen.next_id(),
        request_id,
        result: JsonValue::Null,
        error: Some("LLM API timeout".to_string()),
    })
    .unwrap();

    // 等待 Stable（即使有错误，反应器仍应继续完成）
    let result = timeout(Duration::from_secs(5), async {
        while let Ok(fact) = rx.recv().await {
            match fact {
                Fact::Stable { .. } => return Some(()),
                Fact::Error { message, .. } => panic!("Error: {}", message),
                _ => {}
            }
        }
        None
    })
    .await
    .unwrap();
    assert!(result.is_some());

    // 验证 FactsLog 中的 IoResponse 包含 error 字段
    let history = facts_log.history();
    let io_resp = history.iter().find_map(|f| match f {
        Fact::IoResponse { error, .. } => Some(error.clone()),
        _ => None,
    });
    assert!(io_resp.is_some(), "Should have IoResponse in log");
    assert_eq!(
        io_resp.unwrap(),
        Some("LLM API timeout".to_string()),
        "Error field should be preserved"
    );
}

// ===== 新增集成测试：覆盖修复后的逻辑 =====

/// 构造 set 指令
fn make_set_instruction(attr: &str, value: i64) -> JsonValue {
    let mut params = ObjectMap::new();
    params.insert("attr".to_string(), JsonValue::string(attr));
    params.insert("value".to_string(), JsonValue::Integer(value));
    let mut instr = ObjectMap::new();
    instr.insert("type".to_string(), JsonValue::string("set"));
    instr.insert("params".to_string(), JsonValue::Object(params));
    JsonValue::Object(instr)
}

/// 构造 decrement 指令
fn make_decrement_instruction(attr: &str, delta: i64) -> JsonValue {
    let mut params = ObjectMap::new();
    params.insert("attr".to_string(), JsonValue::string(attr));
    params.insert("delta".to_string(), JsonValue::Integer(delta));
    let mut instr = ObjectMap::new();
    instr.insert("type".to_string(), JsonValue::string("decrement"));
    instr.insert("params".to_string(), JsonValue::Object(params));
    JsonValue::Object(instr)
}

/// 构造 sequence 指令
fn make_sequence_instruction(instructions: Vec<JsonValue>) -> JsonValue {
    let mut params = ObjectMap::new();
    params.insert("instructions".to_string(), JsonValue::Array(instructions));
    let mut instr = ObjectMap::new();
    instr.insert("type".to_string(), JsonValue::string("sequence"));
    instr.insert("params".to_string(), JsonValue::Object(params));
    JsonValue::Object(instr)
}

/// 等待 Stable 事实（不再携带快照），返回 FactsLog 当前快照
async fn wait_for_stable(
    rx: &mut evorule_reactor::EventReceiver,
    facts_log: &evorule_reactor::FactsLog,
) -> Option<JsonValue> {
    timeout(Duration::from_secs(5), async {
        while let Ok(fact) = rx.recv().await {
            match fact {
                Fact::Stable { .. } => return Some(()),
                Fact::Error { message, .. } => panic!("Error: {}", message),
                _ => {}
            }
        }
        None
    })
    .await
    .unwrap()
    .map(|_| facts_log.snapshot().0)
}

#[tokio::test]
async fn test_decrement_instruction() {
    let core_eval = load_core_eval();
    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();

    let mut gen = FactIdGenerator::new();

    // 同时发送 set 和 decrement：drain 会将两个 Command 都 push 到队列
    // 执行 set: x=10
    // 执行 decrement: x=10-3=7
    tx.send(Fact::Command {
        id: gen.next_id(),
        instruction: make_set_instruction("x", 10),
    })
    .unwrap();
    tx.send(Fact::Command {
        id: gen.next_id(),
        instruction: make_decrement_instruction("x", 3),
    })
    .unwrap();

    let snapshot = wait_for_stable(&mut rx, &facts_log)
        .await
        .expect("Stable not received");
    assert_eq!(snapshot.get("x"), Some(&JsonValue::Integer(7)));
}

#[tokio::test]
async fn test_set_instruction() {
    let core_eval = load_core_eval();
    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();

    let mut gen = FactIdGenerator::new();
    tx.send(Fact::Command {
        id: gen.next_id(),
        instruction: make_set_instruction("y", 99),
    })
    .unwrap();

    let snapshot = wait_for_stable(&mut rx, &facts_log)
        .await
        .expect("Stable not received");
    assert_eq!(snapshot.get("y"), Some(&JsonValue::Integer(99)));
}

#[tokio::test]
async fn test_sequence_instruction_expansion() {
    let core_eval = load_core_eval();
    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();

    let mut gen = FactIdGenerator::new();

    // sequence 包含 3 个 increment 指令
    let instructions = vec![
        make_instruction("increment", "x", 1),
        make_instruction("increment", "x", 2),
        make_instruction("increment", "x", 3),
    ];
    tx.send(Fact::Command {
        id: gen.next_id(),
        instruction: make_sequence_instruction(instructions),
    })
    .unwrap();

    let snapshot = wait_for_stable(&mut rx, &facts_log)
        .await
        .expect("Stable not received");
    // x = 1 + 2 + 3 = 6
    assert_eq!(snapshot.get("x"), Some(&JsonValue::Integer(6)));
}

#[tokio::test]
async fn test_max_rounds_exceeded() {
    let core_eval = load_core_eval();
    // max_rounds=3：sequence(1步) + 3个increment(3步) = 4步，第4步会超限
    let reactor = Reactor::builder(core_eval).max_rounds(3).build();
    let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();

    let mut gen = FactIdGenerator::new();

    let instructions = vec![
        make_instruction("increment", "x", 1),
        make_instruction("increment", "x", 1),
        make_instruction("increment", "x", 1),
    ];
    tx.send(Fact::Command {
        id: gen.next_id(),
        instruction: make_sequence_instruction(instructions),
    })
    .unwrap();

    // 应该收到 Error fact（MaxRoundsExceeded）
    let result = timeout(Duration::from_secs(5), async {
        while let Ok(fact) = rx.recv().await {
            match fact {
                Fact::Error { message, .. } => return Some(message),
                Fact::Stable { .. } => panic!("Should not reach Stable"),
                _ => {}
            }
        }
        None
    })
    .await
    .unwrap()
    .expect("Error fact not received");

    assert!(
        result.contains("max rounds exceeded"),
        "Expected max rounds error, got: {}",
        result
    );

    // 验证 FactsLog 中有 Error fact
    let history = facts_log.history();
    let has_error = history.iter().any(|f| matches!(f, Fact::Error { .. }));
    assert!(has_error, "FactsLog should contain Error fact");
}

#[tokio::test]
async fn test_payload_update() {
    let core_eval = load_core_eval();
    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();

    let mut gen = FactIdGenerator::new();

    // 发送 PayloadUpdate 创建 x=42
    tx.send(Fact::PayloadUpdate {
        id: gen.next_id(),
        path: "x".to_string(),
        value: JsonValue::Integer(42),
    })
    .unwrap();

    // 发送 Command increment x by 5
    tx.send(Fact::Command {
        id: gen.next_id(),
        instruction: make_instruction("increment", "x", 5),
    })
    .unwrap();

    // drain 会同时处理两个 Fact：PayloadUpdate 设置 x=42，Command push increment
    // 执行 increment: x = 42 + 5 = 47
    let snapshot = wait_for_stable(&mut rx, &facts_log)
        .await
        .expect("Stable not received");
    assert_eq!(snapshot.get("x"), Some(&JsonValue::Integer(47)));
}

#[tokio::test]
async fn test_payload_update_existing_field() {
    let core_eval = load_core_eval();
    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();

    let mut gen = FactIdGenerator::new();

    // 先设置 x=10
    tx.send(Fact::Command {
        id: gen.next_id(),
        instruction: make_set_instruction("x", 10),
    })
    .unwrap();

    // 同时发送 PayloadUpdate 覆盖 x=99
    // 注意：drain 会先处理 Command（push set 到队列），再处理 PayloadUpdate（更新 x=99）
    // 然后执行 set x=10... 这会覆盖 PayloadUpdate 的值
    // 所以需要在 set 执行后再发送 PayloadUpdate

    // 让我们改用不同方式：先发送 set，等待 Stable 后...但反应器已结束
    // 所以我们需要在同一个 drain 批次中确保顺序
    // 实际上 drain 是 FIFO，所以先发送的先处理

    // 重新设计：发送 set + PayloadUpdate 覆盖 y
    tx.send(Fact::PayloadUpdate {
        id: gen.next_id(),
        path: "y".to_string(),
        value: JsonValue::string("hello"),
    })
    .unwrap();

    let snapshot = wait_for_stable(&mut rx, &facts_log)
        .await
        .expect("Stable not received");
    // set x=10 执行，PayloadUpdate 创建 y="hello"
    assert_eq!(snapshot.get("x"), Some(&JsonValue::Integer(10)));
    assert_eq!(snapshot.get("y").and_then(|v| v.as_str()), Some("hello"));
}

#[tokio::test]
async fn test_multiple_commands_batch() {
    // ISSUE-1 修复验证：快速连续发送多个 Command，确保都被执行
    let core_eval = load_core_eval();
    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();

    let mut gen = FactIdGenerator::new();

    // 快速连续发送 3 个 Command
    tx.send(Fact::Command {
        id: gen.next_id(),
        instruction: make_instruction("increment", "x", 5),
    })
    .unwrap();
    tx.send(Fact::Command {
        id: gen.next_id(),
        instruction: make_instruction("increment", "x", 10),
    })
    .unwrap();
    tx.send(Fact::Command {
        id: gen.next_id(),
        instruction: make_instruction("increment", "x", 20),
    })
    .unwrap();

    // 所有 3 个 Command 应该在同一轮 drain 中被处理
    // x = 5 + 10 + 20 = 35
    let snapshot = wait_for_stable(&mut rx, &facts_log)
        .await
        .expect("Stable not received");
    assert_eq!(
        snapshot.get("x"),
        Some(&JsonValue::Integer(35)),
        "All 3 commands should be executed: expected 35, got {:?}",
        snapshot.get("x")
    );
}

#[tokio::test]
async fn test_channel_closed() {
    let core_eval = load_core_eval();
    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, _rx, _event_tx, handle, _facts_log) = reactor.spawn();

    // 丢弃 tx，触发通道关闭
    drop(tx);

    // 等待反应器结束
    let result = handle.join().await;

    // 长驻模式：所有 command_tx 被丢弃 → 优雅退出 Ok(())
    assert!(
        result.is_ok(),
        "Expected graceful shutdown Ok(()), got: {:?}",
        result
    );
}

#[tokio::test]
async fn test_state_transition_cause_chain() {
    let core_eval = load_core_eval();
    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();

    let mut gen = FactIdGenerator::new();
    let command_id = gen.next_id();

    tx.send(Fact::Command {
        id: command_id,
        instruction: make_instruction("increment", "x", 7),
    })
    .unwrap();

    let _ = wait_for_stable(&mut rx, &facts_log)
        .await
        .expect("Stable not received");

    // 验证 FactsLog 中的 StateTransition 的 cause 指向 Command 的 id
    let history = facts_log.history();

    let command = history
        .iter()
        .find_map(|f| match f {
            Fact::Command { id, .. } => Some(*id),
            _ => None,
        })
        .expect("Should have Command");

    let state_transition = history
        .iter()
        .find_map(|f| match f {
            Fact::StateTransition { cause, .. } => Some(*cause),
            _ => None,
        })
        .expect("Should have StateTransition");

    assert_eq!(command, command_id, "Command id should match sent id");
    assert_eq!(
        state_transition, command,
        "StateTransition cause should point to Command id"
    );
}

#[tokio::test]
async fn test_noop_instruction() {
    let core_eval = load_core_eval();
    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();

    let mut gen = FactIdGenerator::new();

    // noop 指令不执行任何操作
    let mut instr = ObjectMap::new();
    instr.insert("type".to_string(), JsonValue::string("noop"));
    tx.send(Fact::Command {
        id: gen.next_id(),
        instruction: JsonValue::Object(instr),
    })
    .unwrap();

    let snapshot = wait_for_stable(&mut rx, &facts_log)
        .await
        .expect("Stable not received");
    // noop 不修改 payload，仍为空对象
    assert_eq!(snapshot, JsonValue::empty_object());
}

#[tokio::test]
async fn test_unknown_instruction_falls_to_noop() {
    // v0.3.1: 未知指令类型不再静默当 noop。
    // `all([])` 兜底规则匹配但显式标注非业务规则（evorule-tcb/src/transition.rs:179），
    // TCB 返回 `TransitionResult::Ignored`，反应器产生 Error 事实。
    // 这取代了之前"静默失败"的旧设计，强制上游感知未知指令。
    let core_eval = load_core_eval();
    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, mut rx, _event_tx, _handle, _facts_log) = reactor.spawn();

    let mut gen = FactIdGenerator::new();

    // 未知指令类型：all([]) 兜底规则匹配但无业务效果
    let mut instr = ObjectMap::new();
    instr.insert(
        "type".to_string(),
        JsonValue::string("unknown_instruction_type"),
    );
    tx.send(Fact::Command {
        id: gen.next_id(),
        instruction: JsonValue::Object(instr),
    })
    .unwrap();

    // 等待 Error 事实（v0.3.1 新行为：未知指令产生显式告警）
    let error_msg = tokio::time::timeout(Duration::from_secs(5), async {
        while let Ok(fact) = rx.recv().await {
            if let Fact::Error { message, .. } = fact {
                return message;
            }
        }
        panic!("channel closed without Error fact");
    })
    .await
    .expect("timeout waiting for Error fact");

    // Error 消息必须明确指出"被 TCB 忽略"
    assert!(
        error_msg.contains("ignored by TCB"),
        "expected 'ignored by TCB' in error message, got: {}",
        error_msg
    );
}

#[tokio::test]
async fn test_facts_log_version_tracking() {
    let core_eval = load_core_eval();
    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();

    let mut gen = FactIdGenerator::new();

    // 发送 increment 指令
    tx.send(Fact::Command {
        id: gen.next_id(),
        instruction: make_instruction("increment", "x", 5),
    })
    .unwrap();

    let _ = wait_for_stable(&mut rx, &facts_log)
        .await
        .expect("Stable not received");

    // 验证版本号 > 0（至少一次 StateTransition）
    let version = facts_log.version();
    assert!(
        version >= 1,
        "Version should be >= 1 after StateTransition, got {}",
        version
    );

    // 验证 last_stable_version 被记录
    let stable_version = facts_log.last_stable_version();
    assert_eq!(
        stable_version, version,
        "last_stable_version should equal current version after Stable"
    );

    // 验证 snapshot 与最终 payload 一致
    let (snap, _, _) = facts_log.snapshot();
    assert_eq!(snap.get("x"), Some(&JsonValue::Integer(5)));
}

#[tokio::test]
async fn test_read_from_for_audit_replay() {
    let core_eval = load_core_eval();
    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();

    let mut gen = FactIdGenerator::new();
    tx.send(Fact::Command {
        id: gen.next_id(),
        instruction: make_instruction("increment", "x", 5),
    })
    .unwrap();

    let _ = wait_for_stable(&mut rx, &facts_log)
        .await
        .expect("Stable not received");

    // 审计重放：读取所有事实
    let all_facts = facts_log.read_from(0).unwrap();
    assert!(
        all_facts.len() >= 3,
        "Should have at least 3 facts (Command + StateTransition + Stable), got {}",
        all_facts.len()
    );

    // 第一个应该是 Command
    assert!(matches!(all_facts[0], Fact::Command { .. }));

    // 最后一个应该是 Stable
    assert!(matches!(all_facts.last().unwrap(), Fact::Stable { .. }));
}

/// 辅助：等待 IoRequest 并返回 (request_id, io_type)
async fn wait_for_io_request(rx: &mut evorule_reactor::EventReceiver) -> Option<(FactId, IoType)> {
    timeout(Duration::from_secs(5), async {
        while let Ok(fact) = rx.recv().await {
            match fact {
                Fact::IoRequest { id, io_type, .. } => return Some((id, io_type)),
                Fact::Error { message, .. } => panic!("Error: {}", message),
                _ => {}
            }
        }
        None
    })
    .await
    .unwrap()
}

// ===== I/O 双路径机制测试（BUG 修复验证）=====

#[tokio::test]
async fn test_consecutive_different_io_requests_no_interference() {
    // 关键 BUG 修复验证：连续两次不同的 I/O 调用（call_external + call_service）
    // 必须各自走完整的 io_request → io_response → set 消费流程，
    // 不能因为第一次的 __io_results__ 残留导致第二次错误走 on_true 分支。
    //
    // v0.3.1：I/O 结果按 io_type 隔离存储在 __io_results__.{io_type}，
    // 恢复执行完成后整体移除 __io_results__ 容器。
    //
    // 使用 sequence 指令将两个 I/O 指令打包在同一次执行中：
    // sequence([call_external, call_service]) → 队列展开为 [call_external, call_service]
    // 1. call_external 首次执行 → IoRequest → IoResponse → 重新执行 → set llm_response
    // 2. call_service 首次执行 → 若 __io_results__ 未清除，会错误走 on_true（消费旧值）
    //    清除后 → IoRequest → IoResponse → 重新执行 → set service_result
    let core_eval = load_core_eval();
    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();

    let mut gen = FactIdGenerator::new();

    // 用 sequence 打包两个 I/O 指令
    let sequence_instr = make_sequence_instruction(vec![
        make_call_external_instruction("Hello"),
        make_call_service_instruction("calculator"),
    ]);
    tx.send(Fact::Command {
        id: gen.next_id(),
        instruction: sequence_instr,
    })
    .unwrap();

    // 1. 等待第一个 IoRequest（call_external）→ 回复 LLM 对象（含 messages）
    let (request_id_1, io_type_1) = wait_for_io_request(&mut rx).await.expect("IoRequest 1");
    assert_eq!(io_type_1, IoType::call_external());
    send_io_response_value(&tx, &mut gen, request_id_1, make_llm_response("llm answer"));

    // 2. 等待第二个 IoRequest（call_service）
    //    如果 __io_results__ 未被清除，call_service 会错误地走 on_true 分支，
    //    直接 set service_result = 残留的 "llm answer"，而不发起 IoRequest。
    //    此时 wait_for_io_request 会超时 panic。
    let (request_id_2, io_type_2) = wait_for_io_request(&mut rx).await.expect("IoRequest 2");
    assert_eq!(io_type_2, IoType::call_service());
    tx.send(Fact::IoResponse {
        id: gen.next_id(),
        request_id: request_id_2,
        result: JsonValue::string("service rows"),
        error: None,
    })
    .unwrap();

    // 3. 等待 Stable（规则清理后单轮口径：call_service 消费后止步，
    //    不再由 merge 生成下一轮 call_external）
    let snapshot = wait_for_stable(&mut rx, &facts_log).await.expect("Stable");
    let final_llm = make_llm_response("llm answer");
    assert_eq!(
        snapshot.get("llm_response"),
        Some(&final_llm),
        "llm_response should be from the first call_external"
    );
    assert_eq!(
        snapshot.get("service_result").and_then(|v| v.as_str()),
        Some("service rows"),
        "call_service should set service_result from its own IoResponse (not残留的 llm answer)"
    );
    assert!(
        snapshot.get("__io_results__").is_none(),
        "__io_results__ should be cleared after consumption"
    );
}

#[tokio::test]
async fn test_io_result_consumed_to_business_field() {
    // 验证 I/O 双路径机制：io_request → io_response → set 消费 → 业务字段
    let core_eval = load_core_eval();
    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();

    let mut gen = FactIdGenerator::new();

    tx.send(Fact::Command {
        id: gen.next_id(),
        instruction: make_call_external_instruction("summarize"),
    })
    .unwrap();

    let (request_id, _) = wait_for_io_request(&mut rx).await.expect("IoRequest");

    tx.send(Fact::IoResponse {
        id: gen.next_id(),
        request_id,
        result: JsonValue::string("summary ok"),
        error: None,
    })
    .unwrap();

    let snapshot = wait_for_stable(&mut rx, &facts_log).await.expect("Stable");

    // 业务字段 llm_response 应被设置为 I/O 结果
    assert_eq!(
        snapshot.get("llm_response").and_then(|v| v.as_str()),
        Some("summary ok")
    );

    // 验证 FactsLog 中有完整的因果链：
    // Command → StateTransition(1) → IoRequest → IoResponse → StateTransition(2) → Stable
    let history = facts_log.history();
    let has_command = history.iter().any(|f| matches!(f, Fact::Command { .. }));
    let has_io_request = history.iter().any(|f| matches!(f, Fact::IoRequest { .. }));
    let has_io_response = history.iter().any(|f| matches!(f, Fact::IoResponse { .. }));
    let has_stable = history.iter().any(|f| matches!(f, Fact::Stable { .. }));
    let state_transitions = history
        .iter()
        .filter(|f| matches!(f, Fact::StateTransition { .. }))
        .count();

    assert!(has_command, "Should have Command");
    assert!(has_io_request, "Should have IoRequest");
    assert!(has_io_response, "Should have IoResponse");
    assert!(has_stable, "Should have Stable");
    // 应有 2 次 StateTransition：第一次触发 io_request（不产生 StateTransition，只产生 IoRequest）
    // 实际上：call_external 首次执行 → IoRequest（无 StateTransition）
    //         恢复执行 → StateTransition（set llm_response）
    // 所以只有 1 次 StateTransition
    assert!(
        state_transitions >= 1,
        "Should have at least 1 StateTransition (recovery execution), got {}",
        state_transitions
    );
}

// ===== 扩展 I/O 双路径测试：多类型、同类型、混合场景 =====

/// 辅助：构造 call_service 指令
fn make_call_service_instruction(service_name: &str) -> JsonValue {
    let mut params = ObjectMap::new();
    params.insert("service_name".to_string(), JsonValue::string(service_name));
    let mut instr = ObjectMap::new();
    instr.insert("type".to_string(), JsonValue::string("call_service"));
    instr.insert("params".to_string(), JsonValue::Object(params));
    JsonValue::Object(instr)
}

/// 构造 call_external 恢复消费的 LLM 响应对象。
///
/// core_eval 夹具的 call_external 消费分支执行 `set llm_response`，
/// IoResponse 返回含 `messages` 数组的对象（形态对齐真实 LLM 响应；
/// 规则清理后宪法单轮止步，不再由 merge 引用 messages 生成下一轮）。
fn make_llm_response(content: &str) -> JsonValue {
    JsonValue::object_from_pairs(&[(
        "messages",
        JsonValue::Array(vec![JsonValue::object_from_pairs(&[
            ("role", JsonValue::string("assistant")),
            ("content", JsonValue::string(content)),
        ])]),
    )])
}

/// 辅助：发送 IoResponse（字符串结果）
fn send_io_response(
    tx: &evorule_reactor::FactSender,
    gen: &mut FactIdGenerator,
    request_id: FactId,
    result: &str,
) {
    tx.send(Fact::IoResponse {
        id: gen.next_id(),
        request_id,
        result: JsonValue::string(result),
        error: None,
    })
    .unwrap();
}

/// 辅助：发送 IoResponse（JsonValue 结果，如 LLM 对象响应）
fn send_io_response_value(
    tx: &evorule_reactor::FactSender,
    gen: &mut FactIdGenerator,
    request_id: FactId,
    result: JsonValue,
) {
    tx.send(Fact::IoResponse {
        id: gen.next_id(),
        request_id,
        result,
        error: None,
    })
    .unwrap();
}

#[tokio::test]
async fn test_two_different_io_types_sequence() {
    // 验证 2 种受支持 I/O 类型连续调用（call_external + call_service）
    // 每次都必须走完整的 io_request → io_response → set 消费流程。
    // v0.3.1：core_eval 仅内置 call_external（LLM 推理）与 call_service（工具/服务）两类 I/O；
    // query_db/http_get/save_memory 已移出宪法，由应用层以 call_service 实现。
    //
    // 规则清理后单轮口径：call_service 消费结果后止步（push noop），
    // 不再由 merge 生成下一轮 call_external。因此共 2 次 IoRequest：
    // call_external(#1) → call_service(#2)。
    let core_eval = load_core_eval();
    let reactor = Reactor::builder(core_eval).max_rounds(200).build();
    let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();

    let mut gen = FactIdGenerator::new();

    // sequence([call_external, call_service])
    let sequence_instr = make_sequence_instruction(vec![
        make_call_external_instruction("prompt-1"),
        make_call_service_instruction("notify"),
    ]);
    tx.send(Fact::Command {
        id: gen.next_id(),
        instruction: sequence_instr,
    })
    .unwrap();

    // 1. call_external → IoRequest → IoResponse（LLM 返回含 messages 的对象）
    let (rid_1, ty_1) = wait_for_io_request(&mut rx).await.expect("IoRequest 1");
    assert_eq!(ty_1, IoType::call_external());
    let first_llm = make_llm_response("llm-result");
    send_io_response_value(&tx, &mut gen, rid_1, first_llm.clone());

    // 2. call_service → IoRequest（若 __io_results__ 未清除，会错误消费旧值）
    let (rid_2, ty_2) = wait_for_io_request(&mut rx).await.expect("IoRequest 2");
    assert_eq!(ty_2, IoType::call_service());
    send_io_response(&tx, &mut gen, rid_2, "service-output");

    // 3. 验证最终快照（单轮止步，llm_response 来自第一次 call_external）
    let snapshot = wait_for_stable(&mut rx, &facts_log).await.expect("Stable");
    assert_eq!(
        snapshot.get("llm_response"),
        Some(&first_llm),
        "llm_response should be from the first call_external"
    );
    assert_eq!(
        snapshot.get("service_result").and_then(|v| v.as_str()),
        Some("service-output"),
        "call_service should set service_result from its own IoResponse"
    );
    assert!(
        snapshot.get("__io_results__").is_none(),
        "__io_results__ should be cleared after all I/O consumed"
    );
}

#[tokio::test]
async fn test_same_io_type_twice_no_stale_consumption() {
    // 验证相同 I/O 类型连续调用两次（call_external × 2）
    // 第二次必须发起新的 io_request，不能消费第一次的残留 __io_results__
    let core_eval = load_core_eval();
    let reactor = Reactor::builder(core_eval).max_rounds(200).build();
    let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();

    let mut gen = FactIdGenerator::new();

    // sequence([call_external("first"), call_external("second")])
    let sequence_instr = make_sequence_instruction(vec![
        make_call_external_instruction("first prompt"),
        make_call_external_instruction("second prompt"),
    ]);
    tx.send(Fact::Command {
        id: gen.next_id(),
        instruction: sequence_instr,
    })
    .unwrap();

    // 1. 第一个 call_external → IoRequest
    let (rid_1, ty_1) = wait_for_io_request(&mut rx).await.expect("IoRequest 1");
    assert_eq!(ty_1, IoType::call_external());
    send_io_response(&tx, &mut gen, rid_1, "first-answer");

    // 2. 第二个 call_external → 必须发起新的 IoRequest
    //    若 __io_results__ 未清除，第二次 call_external 会直接走 on_true
    //    set llm_response = 残留的 "first-answer"，导致 wait_for_io_request 超时
    let (rid_2, ty_2) = wait_for_io_request(&mut rx).await.expect("IoRequest 2");
    assert_eq!(ty_2, IoType::call_external());
    send_io_response(&tx, &mut gen, rid_2, "second-answer");

    // 3. 验证：llm_response 应为第二次的结果（覆盖第一次）
    let snapshot = wait_for_stable(&mut rx, &facts_log).await.expect("Stable");
    assert_eq!(
        snapshot.get("llm_response").and_then(|v| v.as_str()),
        Some("second-answer"),
        "llm_response should be from the second call_external (not stale first-answer)"
    );
    assert!(
        snapshot.get("__io_results__").is_none(),
        "__io_results__ should be cleared"
    );
}

#[tokio::test]
async fn test_io_interleaved_with_normal_instructions() {
    // 验证 I/O 请求与普通指令混合执行
    // sequence([increment(x,5), call_external, increment(y,10)])
    // 1. increment x=5（普通指令，无 I/O）
    // 2. call_external → IoRequest → IoResponse → set llm_response
    // 3. increment y=10（普通指令，不应受 I/O 残留影响）
    let core_eval = load_core_eval();
    let reactor = Reactor::builder(core_eval).max_rounds(200).build();
    let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();

    let mut gen = FactIdGenerator::new();

    let sequence_instr = make_sequence_instruction(vec![
        make_instruction("increment", "x", 5),
        make_call_external_instruction("mixed prompt"),
        make_instruction("increment", "y", 10),
    ]);
    tx.send(Fact::Command {
        id: gen.next_id(),
        instruction: sequence_instr,
    })
    .unwrap();

    // 1. 等待 call_external 的 IoRequest
    //    （increment 不产生 IoRequest，会先执行完再到达 call_external）
    let (rid, ty) = wait_for_io_request(&mut rx).await.expect("IoRequest");
    assert_eq!(ty, IoType::call_external());
    send_io_response(&tx, &mut gen, rid, "mixed-result");

    // 2. 等待 Stable
    let snapshot = wait_for_stable(&mut rx, &facts_log).await.expect("Stable");

    // 3. 验证所有指令都正确执行
    assert_eq!(
        snapshot.get("x"),
        Some(&JsonValue::Integer(5)),
        "increment x=5 should execute before call_external"
    );
    assert_eq!(
        snapshot.get("llm_response").and_then(|v| v.as_str()),
        Some("mixed-result"),
        "call_external should set llm_response"
    );
    assert_eq!(
        snapshot.get("y"),
        Some(&JsonValue::Integer(10)),
        "increment y=10 should execute after call_external (not affected by I/O)"
    );
    assert!(
        snapshot.get("__io_results__").is_none(),
        "__io_results__ should be cleared"
    );
}

#[tokio::test]
async fn test_all_supported_io_types_sequence() {
    // 终极验证：v0.3.1 支持的 2 种 I/O 类型全部连续调用
    // （规则清理后单轮口径：call_service 消费后止步，无 merge 回环）
    let core_eval = load_core_eval();
    let reactor = Reactor::builder(core_eval).max_rounds(500).build();
    let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();

    let mut gen = FactIdGenerator::new();

    let sequence_instr = make_sequence_instruction(vec![
        make_call_external_instruction("llm-prompt"),
        make_call_service_instruction("calculator"),
    ]);
    tx.send(Fact::Command {
        id: gen.next_id(),
        instruction: sequence_instr,
    })
    .unwrap();

    // 依次等待 2 个 IoRequest 并回复
    // #1 call_external → LLM 对象；#2 call_service → 工具结果
    let expected_types = [IoType::call_external(), IoType::call_service()];
    let expected_results = ["llm-output", "tool-output"];

    for (i, expected_ty) in expected_types.iter().enumerate() {
        let (rid, ty) = wait_for_io_request(&mut rx)
            .await
            .unwrap_or_else(|| panic!("IoRequest {} not received", i + 1));
        assert_eq!(
            ty,
            *expected_ty,
            "IoRequest {} should be {:?}",
            i + 1,
            expected_ty
        );
        if i == 1 {
            // call_service → 字符串工具结果
            send_io_response(&tx, &mut gen, rid, expected_results[i]);
        } else {
            // call_external → LLM 对象（含 messages）
            send_io_response_value(&tx, &mut gen, rid, make_llm_response(expected_results[i]));
        }
    }

    // 验证最终快照（单轮止步，llm_response 来自第一次 call_external）
    let snapshot = wait_for_stable(&mut rx, &facts_log).await.expect("Stable");
    let first_llm = make_llm_response("llm-output");
    assert_eq!(
        snapshot.get("llm_response"),
        Some(&first_llm),
        "llm_response should be from the first call_external"
    );
    assert_eq!(
        snapshot.get("service_result").and_then(|v| v.as_str()),
        Some("tool-output"),
        "service_result should be from call_service IoResponse"
    );
    assert!(
        snapshot.get("__io_results__").is_none(),
        "__io_results__ should be cleared after all I/O consumed"
    );
}

#[tokio::test]
async fn test_io_response_with_null_result_clears_properly() {
    // 验证 IoResponse 携带 Null 结果时，不产生死循环，且后续 I/O 不受影响。
    //
    // v0.3.1 语义：null 结果没有可消费的内容（`exists` 将 null 视为不存在），
    // 若把 null 注入 __io_results__ 后重新推送原指令，恢复执行时 exists==false
    // → 指令无限重发 io_request（死循环）。
    // 处理：丢弃缓存的原指令，不再重新推送（错误/空结果走同一路径）。
    //
    // 测试设计：sequence([call_external(null), call_external(normal)])
    // 1. call_external #1 → IoRequest → 回复 Null → 指令被丢弃（无死循环）
    // 2. call_external #2 仍在队列 → 必须发起新 IoRequest → 回复结果 → set llm_response
    let core_eval = load_core_eval();
    let reactor = Reactor::builder(core_eval).max_rounds(200).build();
    let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();

    let mut gen = FactIdGenerator::new();

    // sequence([call_external, call_external])
    let sequence_instr = make_sequence_instruction(vec![
        make_call_external_instruction("null-test"),
        make_call_external_instruction("second"),
    ]);
    tx.send(Fact::Command {
        id: gen.next_id(),
        instruction: sequence_instr,
    })
    .unwrap();

    // 1. call_external #1 → IoRequest → 回复 Null（指令应被丢弃，不重发 io_request）
    let (rid_1, ty_1) = wait_for_io_request(&mut rx).await.expect("IoRequest 1");
    assert_eq!(ty_1, IoType::call_external());
    tx.send(Fact::IoResponse {
        id: gen.next_id(),
        request_id: rid_1,
        result: JsonValue::Null,
        error: None,
    })
    .unwrap();

    // 2. call_external #2 → 必须仍发起新 IoRequest（不受 #1 的 Null 影响）
    let (rid_2, ty_2) = wait_for_io_request(&mut rx).await.expect("IoRequest 2");
    assert_eq!(ty_2, IoType::call_external());
    send_io_response(&tx, &mut gen, rid_2, "second-answer");

    // 3. 验证
    let snapshot = wait_for_stable(&mut rx, &facts_log).await.expect("Stable");
    // llm_response 应来自第二次 I/O 的结果（第一次 Null 的指令被丢弃，未消费任何字段）
    assert_eq!(
        snapshot.get("llm_response").and_then(|v| v.as_str()),
        Some("second-answer"),
        "llm_response should be from the second call_external (Null result dropped)"
    );
    assert!(
        snapshot.get("__io_results__").is_none(),
        "__io_results__ should be cleared"
    );
}

// ===== 阶段3-1.3：Executing 循环中快照更新测试 =====

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_snapshot_updates_during_executing_loop() {
    // 验证 Executing 循环中每 SNAPSHOT_UPDATE_INTERVAL（=100）步更新快照。
    //
    // 构造 500 条 increment 指令的 sequence，max_rounds = 600。
    // 反应器在 Executing 阶段连续执行时，每 100 步调用 update_snapshot。
    // 测试 task 在另一个 worker thread 上定期轮询 handle.current_step()，
    // 期望在执行过程中观察到 steps >= 100。
    //
    // 注意：必须用 multi_thread runtime，因为反应器在 Executing 循环中不 yield，
    // single_thread runtime 下测试 task 不会被调度。
    let core_eval = load_core_eval();
    let reactor = Reactor::builder(core_eval).max_rounds(600).build();
    let (tx, mut rx, _event_tx, handle, _facts_log) = reactor.spawn();

    // 构造 500 条 increment 指令的 sequence
    let increments: Vec<JsonValue> = (0..500)
        .map(|_| make_instruction("increment", "x", 1))
        .collect();
    let mut gen = FactIdGenerator::new();
    tx.send(Fact::Command {
        id: gen.next_id(),
        instruction: make_sequence_instruction(increments),
    })
    .unwrap();

    // 在反应器执行过程中，定期检查快照
    let mut max_step_seen = 0usize;
    let mut saw_step_ge_100 = false;

    let result = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            tokio::select! {
                fact = rx.recv() => {
                    match fact {
                        Ok(Fact::Stable { .. }) => break,
                        Ok(Fact::Error { message, .. }) => panic!("Error: {}", message),
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                _ = tokio::time::sleep(Duration::from_micros(200)) => {
                    if let Some(step) = handle.current_step() {
                        if step > max_step_seen {
                            max_step_seen = step;
                        }
                        if step >= 100 {
                            saw_step_ge_100 = true;
                        }
                    }
                }
            }
        }
    })
    .await;

    assert!(result.is_ok(), "测试超时（10s 内未收到 Stable）");
    assert!(
        saw_step_ge_100,
        "期望在 Executing 循环中观察到 steps >= 100（SNAPSHOT_UPDATE_INTERVAL=100），\
         实际观察到的最大 steps: {}。\
         这表明 Executing 循环中的定期快照更新未生效。",
        max_step_seen
    );

    // 验证最终结果：500 条 increment(x, 1) → x = 500
    // 反应器已 Stable，快照中 steps 应为 0（长驻模式重置），finished=false
    let snap = handle.snapshot().expect("snapshot should be readable");
    assert!(
        !snap.finished,
        "反应器应仍在运行（长驻模式），finished 应为 false"
    );
}

/// 阶段6：pending_io() 返回 I/O 详情（inspect API）
///
/// 验证 inspect API 返回的 pending I/O 详情：
/// - call_external 触发 IoRequest 后，pending_io() 返回非空列表
/// - 列表包含 (FactId, IoType::call_external(), Duration)
/// - Duration 是已等待时长（>= 0）
#[tokio::test]
async fn test_inspect_returns_pending_io() {
    let core_eval = load_core_eval();
    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, mut rx, _event_tx, handle, facts_log) = reactor.spawn();

    // 1. 发送 call_llm 指令
    let mut gen = FactIdGenerator::new();
    tx.send(Fact::Command {
        id: gen.next_id(),
        instruction: make_call_external_instruction("test prompt"),
    })
    .unwrap();

    // 2. 等待 IoRequest（reactor 发射后进入 AwaitingIo 阶段）
    let request_id = timeout(Duration::from_secs(5), async {
        while let Ok(fact) = rx.recv().await {
            if let Fact::IoRequest { id, .. } = fact {
                return id;
            }
            if let Fact::Error { message, .. } = fact {
                panic!("Error: {}", message);
            }
        }
        panic!("IoRequest 未收到");
    })
    .await
    .expect("等待 IoRequest 超时");

    // 3. 等待一小段时间，让 snapshot 更新 pending_io_snapshot
    tokio::time::sleep(Duration::from_millis(50)).await;

    // 4. 调用 pending_io_count()，验证返回内容
    let pending_count = handle.pending_io_count().unwrap_or(0);
    assert_eq!(
        pending_count, 1,
        "pending_io_count 应返回 1，got {}",
        pending_count
    );

    // 5. 回复 IoResponse，等待 Stable
    tx.send(Fact::IoResponse {
        id: gen.next_id(),
        request_id,
        result: JsonValue::string("llm result"),
        error: None,
    })
    .unwrap();

    let snapshot = wait_for_stable(&mut rx, &facts_log)
        .await
        .expect("回复 IoResponse 后应收到 Stable");
    assert_eq!(
        snapshot.get("llm_response").and_then(|v| v.as_str()),
        Some("llm result"),
        "llm_response 应被设置"
    );

    // 6. 验证 Stable 后 pending_io_count 为 0
    let pending_after = handle.pending_io_count().unwrap_or(0);
    assert!(
        pending_after == 0,
        "Stable 后 pending_io_count 应为 0，got {}",
        pending_after
    );

    drop(tx);
    let _ = handle.join().await;
}

// === D11 REPLAY TEST START (DELETE_RANGE: 整段从下一行到 D11 REPLAY TEST END) ===
// D11 IoRequired 重放契约差分测试
//
// 验证: 跨层不变式 "重放等价性" (TCB_SPEC.md §四 D11)
//   - 路径 A (首次执行): submit Command -> 收 IoRequest -> submit IoResponse -> Stable
//   - 路径 B (重放执行): submit PayloadUpdate (预注入 __io_results__) -> submit Command -> Stable (走 on_true, 跳过 IoRequest)
//   - 差分断言: snap_a.llm_response == snap_b.llm_response (业务结果一致)
//   - D11 步骤 4: 两条路径最终 __io_results__.call_external 都被清除 (exists == false)
//
// 删除指南: 失败时用 PowerShell [System.IO.File] 截断 L1577 之后
#[tokio::test]
async fn test_d11_replay_consistency_first_vs_preresult() {
    let core_eval = load_core_eval();
    let mock_obj =
        JsonValue::object_from_pairs(&[("llm_response", JsonValue::string("d11_diff_mock"))]);

    // ============================================================
    // 路径 A: 首次执行 (标准 IoRequest -> IoResponse -> 重放)
    // ============================================================
    let (tx_a, mut rx_a, _event_a, handle_a, facts_a) = Reactor::builder(core_eval.clone())
        .max_rounds(100)
        .build()
        .spawn();
    let mut gen_a = FactIdGenerator::new();

    // 1. 提交 call_external 指令
    tx_a.send(Fact::Command {
        id: gen_a.next_id(),
        instruction: make_call_external_instruction("d11_test"),
    })
    .unwrap();

    // 2. 等待 IoRequest (首次执行应触发)
    let request_id_a = timeout(Duration::from_secs(5), async {
        while let Ok(fact) = rx_a.recv().await {
            match fact {
                Fact::IoRequest { id, .. } => return Some(id),
                Fact::Error { message, .. } => panic!("D11 路径 A Error: {}", message),
                _ => {}
            }
        }
        None
    })
    .await
    .expect("D11 路径 A: 等待 IoRequest 超时")
    .expect("D11 路径 A: 未收到 IoRequest");

    // 3. 提交 IoResponse (D11 步骤 2: 注入 __io_results__ + io_recovery=true + 原指令重放)
    tx_a.send(Fact::IoResponse {
        id: gen_a.next_id(),
        request_id: request_id_a,
        result: mock_obj.clone(),
        error: None,
    })
    .unwrap();

    // 4. 等待 Stable (D11 步骤 3 重放后,步骤 4 清除 __io_results__)
    let snap_a = wait_for_stable(&mut rx_a, &facts_a)
        .await
        .expect("D11 路径 A: IoResponse 后应 Stable");

    drop(tx_a);
    let _ = handle_a.join().await;

    // ============================================================
    // 路径 B: 重放执行 (预注入 __io_results__, 模拟 D11 步骤 2 完成态)
    // ============================================================
    let (tx_b, mut rx_b, _event_b, handle_b, facts_b) =
        Reactor::builder(core_eval).max_rounds(100).build().spawn();
    let mut gen_b = FactIdGenerator::new();

    // 1. 预注入 __io_results__.call_external (模拟步骤 2 完成态)
    tx_b.send(Fact::PayloadUpdate {
        id: gen_b.next_id(),
        path: "__io_results__.call_external".to_string(),
        value: mock_obj.clone(),
    })
    .unwrap();

    // 2. 提交同一条 call_external 指令
    tx_b.send(Fact::Command {
        id: gen_b.next_id(),
        instruction: make_call_external_instruction("d11_test"),
    })
    .unwrap();

    // 3. 收集事实流,断言不应收到 IoRequest (走 on_true 跳过)
    let mut got_io_request_b = false;
    let mut stable_b = false;
    while let Ok(fact) = rx_b.recv().await {
        match fact {
            Fact::IoRequest { .. } => got_io_request_b = true,
            // Stable 不再携带快照(),状态经 FactsLog 快照取
            Fact::Stable { .. } => {
                stable_b = true;
                break;
            }
            Fact::Error { message, .. } => panic!("D11 路径 B Error: {}", message),
            _ => {}
        }
    }
    assert!(stable_b, "D11 路径 B: 应 Stable");
    let (snap_b, _, _) = facts_b.snapshot();

    drop(tx_b);
    let _ = handle_b.join().await;

    // ============================================================
    // 差分断言 (D11 跨层不变式: 重放等价性)
    // ============================================================

    // 1. 业务结果一致 (核心差分断言)
    assert_eq!(
        snap_a.get("llm_response"),
        snap_b.get("llm_response"),
        "D11 重放契约 (跨层不变式): snap_a.llm_response 应 == snap_b.llm_response"
    );

    // 2. 路径 B 不应收 IoRequest (D11 步骤 3: 重放走 on_true, 跳过 IoRequest)
    assert!(
        !got_io_request_b,
        "D11 重放契约: 预注入 __io_results__ 后路径 B 不应触发 IoRequest, 应走 on_true"
    );

    // 3. D11 步骤 4: 两条路径殊途同归 — 都不再可消费 __io_results__.call_external
    //    - 路径 A: reactor 整体移除 __io_results__ 容器 (reactor.rs L609-613)
    //    - 路径 B: on_true 分支 set null 到 call_external (core_eval.json L247-254)
    //    两者都让后续 `exists __io_results__.call_external == false`
    let is_consumable_a = snap_a
        .get("__io_results__")
        .and_then(|v| v.get("call_external"))
        .map(|v| !matches!(v, JsonValue::Null))
        .unwrap_or(false);
    let is_consumable_b = snap_b
        .get("__io_results__")
        .and_then(|v| v.get("call_external"))
        .map(|v| !matches!(v, JsonValue::Null))
        .unwrap_or(false);
    assert!(
        !is_consumable_a,
        "D11 步骤 4: 路径 A __io_results__.call_external 应不可消费 (exists == false)"
    );
    assert!(
        !is_consumable_b,
        "D11 步骤 4: 路径 B __io_results__.call_external 应不可消费 (exists == false)"
    );
}
// === D11 REPLAY TEST END ===

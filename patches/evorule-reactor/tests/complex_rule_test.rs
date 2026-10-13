// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 复杂 JSON 规则端到端测试
//!
//! 测试流程：
//! 1. 加载 JSON 规则文件（通过 serde_json 解析）
//! 2. 通过 reactor 执行指令

// 测试代码豁免 L2 clippy (L1 build.rs 门禁已守 panic-prone)。详见 GATE_REFERENCE.md §六(豁免索引)
#![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
//! 3. 检查审计链中的 Fact 记录
//! 4. 验证执行结果

use evorule_reactor::{Fact, FactIdGenerator, Reactor};
use evorule_tcb::{JsonValue, ObjectMap};
use std::time::Duration;
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

/// 从 JSON 文件加载 transform 规则
fn load_json_rule(json_str: &str) -> Vec<JsonValue> {
    let json: serde_json::Value =
        serde_json::from_str(json_str).expect("Failed to parse JSON rule");

    json.get("transform")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().cloned().map(serde_to_tcb).collect())
        .unwrap_or_default()
}

/// 测试场景 1: VIP 客户订单处理
#[tokio::test]
// 端到端测试场景, 161 行 fixture (规则 + 执行 + 断言)。详见 GATE_REFERENCE.md §六(豁免索引)
#[allow(clippy::too_many_lines)]
async fn test_vip_order_processing() {
    // JSON 规则：电商订单处理（遵循 TCB 的 I/O 两阶段模式）
    // v0.3.1：I/O 结果按类型隔离在 __io_results__.{io_type}
    // 阶段 1：检查 is_vip，如果存在且 __io_results__.call_service 不存在，则发起 I/O
    // 阶段 2：如果 __io_results__.call_service 存在，则消费结果
    let json_rule = r#"{
  "transform": [
    {
      "type": "branch",
      "params": {
        "domain": {
          "type": "instruction",
          "instruction_type": "process_order"
        },
        "on_true": [
          {
            "type": "branch",
            "params": {
              "domain": {
                "type": "exists",
                "path": "__exec__.payload.is_vip"
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
                          "attr": "discount",
                          "operation": "set",
                          "value": 10
                        }
                      },
                      {
                        "type": "set",
                        "params": {
                          "attr": "vip_notification",
                          "operation": "set",
                          "value": "__exec__.payload.__io_results__.call_service"
                        }
                      }
                    ],
                    "on_false": [
                      {
                        "type": "io_request",
                        "params": {
                          "io_type": "call_service",
                          "service_name": "notify_vip",
                          "prompt": "VIP 客户享受 9 折优惠"
                        }
                      }
                    ]
                  }
                }
              ],
              "on_false": [
                {
                  "type": "set",
                  "params": {
                    "attr": "discount",
                    "operation": "set",
                    "value": 0
                  }
                }
              ]
            }
          }
        ]
      }
    }
  ]
}"#;

    let core_eval = load_json_rule(json_rule);

    // 初始 payload：VIP 客户
    let _initial_payload = JsonValue::object_from_pairs(&[
        ("is_vip", JsonValue::Bool(true)),
        ("inventory", JsonValue::Integer(100)),
    ]);

    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();

    let mut gen = FactIdGenerator::new();

    // 设置初始状态
    tx.send(Fact::PayloadUpdate {
        id: gen.next_id(),
        path: "is_vip".to_string(),
        value: JsonValue::Bool(true),
    })
    .unwrap();
    tx.send(Fact::PayloadUpdate {
        id: gen.next_id(),
        path: "inventory".to_string(),
        value: JsonValue::Integer(100),
    })
    .unwrap();

    // 提交 VIP 客户订单指令
    let instruction = JsonValue::object_from_pairs(&[
        ("type", JsonValue::string("process_order")),
        (
            "params",
            JsonValue::object_from_pairs(&[
                ("order_id", JsonValue::string("ORD-001")),
                ("amount", JsonValue::Integer(1000)),
            ]),
        ),
    ]);

    tx.send(Fact::Command {
        id: gen.next_id(),
        instruction,
    })
    .unwrap();

    // 阶段 1：等待 IoRequest
    let request_id = timeout(Duration::from_secs(5), async {
        while let Ok(fact) = rx.recv().await {
            match fact {
                Fact::IoRequest {
                    id,
                    io_type,
                    params,
                    ..
                } => {
                    println!("I/O Request: {} - {:?}", io_type, params);
                    return Some(id);
                }
                Fact::Error { message, .. } => {
                    panic!("Error during execution: {}", message);
                }
                _ => {}
            }
        }
        None
    })
    .await
    .unwrap()
    .expect("IoRequest not received");

    // 阶段 2：发送 IoResponse，等待 Stable
    tx.send(Fact::IoResponse {
        id: gen.next_id(),
        request_id,
        result: JsonValue::string("VIP notification sent"),
        error: None,
    })
    .unwrap();

    let final_state = timeout(Duration::from_secs(5), async {
        while let Ok(fact) = rx.recv().await {
            match fact {
                // Stable 不再携带快照(),状态经 FactsLog 快照取
                Fact::Stable { .. } => return Some(facts_log.snapshot().0),
                Fact::Error { message, .. } => {
                    panic!("Error during execution: {}", message);
                }
                _ => {}
            }
        }
        None
    })
    .await
    .unwrap()
    .expect("Timeout waiting for Stable after IoResponse");

    // 检查 discount 是否设置为 10
    let discount = final_state
        .get("discount")
        .and_then(|v| v.as_i64())
        .expect("discount should exist");
    assert_eq!(discount, 10, "VIP customer should have 10% discount");

    // 检查审计链
    let facts = facts_log.history();
    println!("\n=== 审计链记录 ===");
    for (i, fact) in facts.iter().enumerate() {
        println!("Fact {}: {:?}", i, fact);
    }

    // 验证审计链中有 Command 和 IoRequest
    let has_command = facts.iter().any(|f| matches!(f, Fact::Command { .. }));
    let has_io_request = facts.iter().any(|f| matches!(f, Fact::IoRequest { .. }));
    let has_io_response = facts.iter().any(|f| matches!(f, Fact::IoResponse { .. }));

    assert!(has_command, "Should have Command fact");
    assert!(has_io_request, "Should have IoRequest fact");
    assert!(has_io_response, "Should have IoResponse fact");

    println!("\n✓ 审计链完整，包含 Command → IoRequest → IoResponse");
}

/// 测试场景 2: 普通客户订单处理
#[tokio::test]
async fn test_normal_order_processing() {
    let json_rule = r#"{
  "transform": [
    {
      "type": "branch",
      "params": {
        "domain": {
          "type": "instruction",
          "instruction_type": "process_order"
        },
        "on_true": [
          {
            "type": "branch",
            "params": {
              "domain": {
                "type": "exists",
                "path": "__exec__.payload.is_vip"
              },
              "on_true": [
                {
                  "type": "set",
                  "params": {
                    "attr": "discount",
                    "operation": "set",
                    "value": 10
                  }
                }
              ],
              "on_false": [
                {
                  "type": "set",
                  "params": {
                    "attr": "discount",
                    "operation": "set",
                    "value": 0
                  }
                }
              ]
            }
          }
        ]
      }
    }
  ]
}"#;

    let core_eval = load_json_rule(json_rule);
    let reactor = Reactor::builder(core_eval).max_rounds(100).build();
    let (tx, mut rx, _event_tx, _handle, facts_log) = reactor.spawn();

    let mut gen = FactIdGenerator::new();

    // 提交普通客户订单指令（无 is_vip 字段）
    let mut params = ObjectMap::new();
    params.insert("order_id".to_string(), JsonValue::string("ORD-002"));
    params.insert("amount".to_string(), JsonValue::Integer(500));

    let mut instr = ObjectMap::new();
    instr.insert("type".to_string(), JsonValue::string("process_order"));
    instr.insert("params".to_string(), JsonValue::Object(params));

    let instruction = JsonValue::Object(instr);

    tx.send(Fact::Command {
        id: gen.next_id(),
        instruction,
    })
    .unwrap();

    // 等待执行完成
    let result = timeout(Duration::from_secs(5), async {
        let mut final_state = None;
        while let Ok(fact) = rx.recv().await {
            match &fact {
                // Stable 不再携带快照(),状态经 FactsLog 快照取
                Fact::Stable { .. } => {
                    final_state = Some(facts_log.snapshot().0);
                    break;
                }
                Fact::Error { message, .. } => {
                    panic!("Error during execution: {}", message);
                }
                _ => {}
            }
        }
        final_state
    })
    .await
    .expect("Timeout waiting for execution");

    // 验证结果
    assert!(result.is_some(), "Should have final state");
    let final_state = result.unwrap();

    // 检查 discount 是否设置为 0（普通客户）
    let discount = final_state
        .get("discount")
        .and_then(|v| v.as_i64())
        .expect("discount should exist");
    assert_eq!(discount, 0, "Normal customer should have 0% discount");

    // 检查审计链
    let facts = facts_log.history();
    println!("\n=== 审计链记录 ===");
    for (i, fact) in facts.iter().enumerate() {
        println!("Fact {}: {:?}", i, fact);
    }

    println!("\n✓ 普通客户无折扣，审计链完整");
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
// 集成测试不受 build.rs 门禁扫描（仅扫描 src/），允许 unwrap/panic
#![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
//! 集成测试：从外部 crate 视角验证 evorule-tcb 公开 API
//!
//! 与单元测试互补，重点验证：
//! - 确定性保证（相同输入 → 相同输出）
//! - 状态隔离（无跨调用副作用）
//! - 错误类型可被外部 crate 匹配
//! - 跨模块集成（域评估 + 路径解析 + 状态转换）
//! - 完整 ReAct 循环（手工构造，与 core_eval.json v0.3.1 结构一致）
//!
//! 所有规则均手工构造 `JsonValue`（零 serde 依赖）。

use evorule_tcb::domain::evaluate_domain;
use evorule_tcb::path::resolve_path;
use evorule_tcb::{execute_transition, JsonValue, TcbError, TransitionResult};

// ============================================================================
// 辅助函数
// ============================================================================

fn s(v: &str) -> JsonValue {
    JsonValue::string(v)
}

fn iv(v: i64) -> JsonValue {
    JsonValue::Integer(v)
}

fn obj(pairs: &[(&str, JsonValue)]) -> JsonValue {
    JsonValue::object_from_pairs(pairs)
}

fn arr(v: Vec<JsonValue>) -> JsonValue {
    JsonValue::array(v)
}

fn make_instruction(instr_type: &str, params: &[(&str, JsonValue)]) -> JsonValue {
    obj(&[("type", s(instr_type)), ("params", obj(params))])
}

fn branch(domain: JsonValue, on_true: Vec<JsonValue>, on_false: Vec<JsonValue>) -> JsonValue {
    obj(&[
        ("type", s("branch")),
        (
            "params",
            obj(&[
                ("domain", domain),
                ("on_true", arr(on_true)),
                ("on_false", arr(on_false)),
            ]),
        ),
    ])
}

fn set_instr(attr: &str, op: &str, value: JsonValue) -> JsonValue {
    obj(&[
        ("type", s("set")),
        (
            "params",
            obj(&[("attr", s(attr)), ("operation", s(op)), ("value", value)]),
        ),
    ])
}

fn push_instr(instructions: Vec<JsonValue>) -> JsonValue {
    obj(&[
        ("type", s("push")),
        ("params", obj(&[("instructions", arr(instructions))])),
    ])
}

fn push_noop() -> JsonValue {
    push_instr(vec![obj(&[("type", s("noop"))])])
}

fn domain_instruction(t: &str) -> JsonValue {
    obj(&[("type", s("instruction")), ("instruction_type", s(t))])
}

fn domain_exists(path: &str) -> JsonValue {
    obj(&[("type", s("exists")), ("path", s(path))])
}

fn domain_lt(path: &str, value: i64) -> JsonValue {
    obj(&[("type", s("lt")), ("path", s(path)), ("value", iv(value))])
}

fn domain_not(inner: JsonValue) -> JsonValue {
    obj(&[("type", s("not")), ("inner", inner)])
}

fn domain_all(inner: Vec<JsonValue>) -> JsonValue {
    obj(&[("type", s("all")), ("inner", arr(inner))])
}

// ============================================================================
// ReAct 宪法规则构造（对应 core_eval.json v0.3.1 transform[6..12]）
// ============================================================================

/// 规则 1：react_iteration 自初始化（call_external 且 react_iteration 缺失时置 0）
fn rule_self_init() -> JsonValue {
    branch(
        domain_all(vec![
            domain_instruction("call_external"),
            domain_not(domain_exists("__exec__.payload.react_iteration")),
        ]),
        vec![set_instr("react_iteration", "set", iv(0))],
        vec![],
    )
}

/// 规则 2：call_external —— 消费 LLM 结果 / 发起 LLM 请求
fn rule_call_external() -> JsonValue {
    branch(
        domain_instruction("call_external"),
        vec![branch(
            domain_exists("__exec__.payload.__io_results__.call_external"),
            vec![
                set_instr(
                    "llm_response",
                    "set",
                    s("__exec__.payload.__io_results__.call_external"),
                ),
                // 持久化 tools（如果指令中有）
                branch(
                    domain_exists("__exec__.instruction.params.tools"),
                    vec![set_instr(
                        "tools",
                        "set",
                        s("__exec__.instruction.params.tools"),
                    )],
                    vec![],
                ),
                // 清除 I/O 结果
                set_instr(
                    "__exec__.payload.__io_results__.call_external",
                    "set",
                    JsonValue::Null,
                ),
                // 循环终止（工具扇出编排在应用层 runner，collect 已退役）
                push_noop(),
            ],
            vec![io_request_call_external()],
        )],
        vec![],
    )
}

/// call_external 的 io_request
fn io_request_call_external() -> JsonValue {
    obj(&[
        ("type", s("io_request")),
        (
            "params",
            obj(&[
                ("io_type", s("call_external")),
                ("messages", s("__exec__.instruction.params.messages")),
                ("tools?", s("__exec__.instruction.params.tools")),
            ]),
        ),
    ])
}

/// 规则 3：call_service —— 消费工具结果 / 发起工具请求
fn rule_call_service() -> JsonValue {
    branch(
        domain_instruction("call_service"),
        vec![branch(
            domain_exists("__exec__.payload.__io_results__.call_service"),
            vec![
                set_instr(
                    "service_result",
                    "set",
                    s("__exec__.payload.__io_results__.call_service"),
                ),
                set_instr(
                    "__exec__.payload.__io_results__.call_service",
                    "set",
                    JsonValue::Null,
                ),
                // 循环终止（结果合并与下一条 LLM 调用编排在应用层 runner，merge 已退役）
                push_noop(),
            ],
            vec![io_request_call_service()],
        )],
        vec![],
    )
}

/// call_service 的 io_request
fn io_request_call_service() -> JsonValue {
    obj(&[
        ("type", s("io_request")),
        (
            "params",
            obj(&[
                ("io_type", s("call_service")),
                (
                    "service_name",
                    s("__exec__.instruction.params.service_name"),
                ),
                ("args", s("__exec__.instruction.params.args")),
            ]),
        ),
    ])
}

/// 规则：noop 指令（空操作）
fn rule_noop() -> JsonValue {
    branch(domain_instruction("noop"), vec![], vec![])
}

/// 规则：兜底（所有未匹配的指令）
fn rule_catch_all() -> JsonValue {
    branch(domain_all(vec![]), vec![], vec![])
}

/// 构造完整的 ReAct 宪法规则列表
fn react_constitution() -> Vec<JsonValue> {
    vec![
        rule_self_init(),
        rule_call_external(),
        rule_call_service(),
        rule_noop(),
        rule_catch_all(),
    ]
}

// ============================================================================
// 测试辅助函数
// ============================================================================

fn make_payload(pairs: &[(&str, JsonValue)]) -> JsonValue {
    if pairs.is_empty() {
        JsonValue::empty_object()
    } else {
        obj(pairs)
    }
}

fn call_external_instr(messages: JsonValue) -> JsonValue {
    make_instruction("call_external", &[("messages", messages)])
}

fn call_service_instr(name: &str, args: JsonValue) -> JsonValue {
    make_instruction("call_service", &[("service_name", s(name)), ("args", args)])
}

fn user_messages() -> JsonValue {
    arr(vec![obj(&[
        ("role", s("user")),
        ("content", s("What's the weather?")),
    ])])
}

// ============================================================================
// 集成测试用例
// ============================================================================

// ── 确定性 ────────────────────────────────────────────────────────────────

/// 确定性保证：同一输入执行 10 次，结果必须完全相同
#[test]
fn test_determinism_repeated_calls() {
    let core_eval = react_constitution();
    let instruction = make_instruction("noop", &[]);
    let payload = make_payload(&[("x", iv(42))]);

    // 10 次调用，结果全部相同
    let mut results = Vec::new();
    for _ in 0..10 {
        let result = execute_transition(&core_eval, &instruction, &payload, &[]).unwrap();
        results.push(result);
    }

    // 两两比较
    for i in 1..results.len() {
        assert_eq!(
            results[0], results[i],
            "确定性违反：第 {} 次与第 0 次结果不同",
            i
        );
    }

    // 验证具体值
    match &results[0] {
        TransitionResult::State {
            new_payload,
            new_queue,
            ..
        } => {
            assert_eq!(new_payload.get("x"), Some(&iv(42)));
            assert!(new_queue.is_empty());
        }
        _ => panic!("noop 应返回 State"),
    }
}

// ── 状态隔离 ──────────────────────────────────────────────────────────────

/// 状态隔离：不同 payload 的独立调用互不干扰
#[test]
fn test_state_isolation_independent_calls() {
    let core_eval = react_constitution();
    let instruction = make_instruction("noop", &[]);

    // 两个独立调用，不同 payload
    let r1 = execute_transition(
        &core_eval,
        &instruction,
        &make_payload(&[("a", iv(1))]),
        &[],
    )
    .unwrap();
    let r2 = execute_transition(
        &core_eval,
        &instruction,
        &make_payload(&[("b", iv(2))]),
        &[],
    )
    .unwrap();

    match (&r1, &r2) {
        (
            TransitionResult::State {
                new_payload: p1, ..
            },
            TransitionResult::State {
                new_payload: p2, ..
            },
        ) => {
            assert_eq!(p1.get("a"), Some(&iv(1)));
            assert_eq!(p1.get("b"), None, "p1 不应有 b 字段");
            assert_eq!(p2.get("b"), Some(&iv(2)));
            assert_eq!(p2.get("a"), None, "p2 不应有 a 字段");
        }
        _ => panic!("两个调用都应返回 State"),
    }
}

// ── 错误类型匹配 ──────────────────────────────────────────────────────────

/// 外部 crate 可以匹配所有 TcbError 变体
#[test]
fn test_error_type_matching_from_external_crate() {
    // 1. MissingField
    let err = TcbError::MissingField {
        field: "test".to_string(),
    };
    match &err {
        TcbError::MissingField { field } => assert_eq!(field, "test"),
        _ => panic!("应匹配 MissingField"),
    }

    // 2. UnknownMetaInstruction
    let err = TcbError::UnknownMetaInstruction {
        meta_type: "foo".to_string(),
    };
    match &err {
        TcbError::UnknownMetaInstruction { meta_type } => assert_eq!(meta_type, "foo"),
        _ => panic!("应匹配 UnknownMetaInstruction"),
    }

    // 3. UnknownOperation
    let err = TcbError::UnknownOperation {
        operation: "multiply".to_string(),
    };
    match &err {
        TcbError::UnknownOperation { operation } => assert_eq!(operation, "multiply"),
        _ => panic!("应匹配 UnknownOperation"),
    }

    // 4. InvalidState
    let err = TcbError::InvalidState {
        reason: "bad state".to_string(),
    };
    match &err {
        TcbError::InvalidState { reason } => assert_eq!(reason, "bad state"),
        _ => panic!("应匹配 InvalidState"),
    }

    // 5. InvalidType
    let err = TcbError::InvalidType {
        expected: "integer",
        actual: "string",
        context: "value".to_string(),
    };
    match &err {
        TcbError::InvalidType {
            expected,
            actual,
            context,
        } => {
            assert_eq!(expected, &"integer");
            assert_eq!(actual, &"string");
            assert_eq!(context, "value");
        }
        _ => panic!("应匹配 InvalidType"),
    }

    // 6. PathResolutionFailed
    let err = TcbError::PathResolutionFailed {
        path: "a.b".to_string(),
        reason: "not found".to_string(),
    };
    match &err {
        TcbError::PathResolutionFailed { path, reason } => {
            assert_eq!(path, "a.b");
            assert_eq!(reason, "not found");
        }
        _ => panic!("应匹配 PathResolutionFailed"),
    }

    // 7. NestingTooDeep
    let err = TcbError::NestingTooDeep { limit: 64 };
    match &err {
        TcbError::NestingTooDeep { limit } => assert_eq!(*limit, 64),
        _ => panic!("应匹配 NestingTooDeep"),
    }

    // 8. IntegerOverflow
    let err = TcbError::IntegerOverflow {
        operation: "add".to_string(),
        left: i64::MAX,
        right: 1,
    };
    match &err {
        TcbError::IntegerOverflow {
            operation,
            left,
            right,
        } => {
            assert_eq!(operation, "add");
            assert_eq!(*left, i64::MAX);
            assert_eq!(*right, 1);
        }
        _ => panic!("应匹配 IntegerOverflow"),
    }

    // 9. TooManyTransformRules
    let err = TcbError::TooManyTransformRules {
        limit: 64,
        actual: 100,
    };
    match &err {
        TcbError::TooManyTransformRules { limit, actual } => {
            assert_eq!(*limit, 64);
            assert_eq!(*actual, 100);
        }
        _ => panic!("应匹配 TooManyTransformRules"),
    }
}

// ── 域评估 + 路径解析 + 状态转换 跨模块集成 ──────────────────────────────

/// 跨模块集成：构造包含 domain 检测的 branch 规则，验证 domain → path → transition 链路
#[test]
fn test_domain_path_transition_integration() {
    // 构造一条规则：如果 payload.x == 10，则 set x = 20
    let rule = branch(
        obj(&[
            ("type", s("eq")),
            ("path", s("__exec__.payload.x")),
            ("value", iv(10)),
        ]),
        vec![set_instr("x", "set", iv(20))],
        vec![],
    );
    let core_eval = vec![rule];

    // 条件满足：x == 10 → x 变为 20
    let result = execute_transition(
        &core_eval,
        &make_instruction("noop", &[]),
        &make_payload(&[("x", iv(10))]),
        &[],
    )
    .unwrap();
    match result {
        TransitionResult::State { new_payload, .. } => {
            assert_eq!(new_payload.get("x"), Some(&iv(20)));
        }
        _ => panic!("应返回 State"),
    }

    // 条件不满足：x == 5 → x 保持不变
    let result = execute_transition(
        &core_eval,
        &make_instruction("noop", &[]),
        &make_payload(&[("x", iv(5))]),
        &[],
    )
    .unwrap();
    match result {
        TransitionResult::State { new_payload, .. } => {
            assert_eq!(new_payload.get("x"), Some(&iv(5)));
        }
        _ => panic!("应返回 State"),
    }
}

// ── ReAct 循环：第一步（call_external → IoRequired） ─────────────────────

/// ReAct 第 1 步：首次 call_external 无 I/O 结果 → 发起 io_request
#[test]
fn test_react_round1_io_request_fires() {
    let core_eval = react_constitution();
    let instruction = call_external_instr(user_messages());
    let payload = make_payload(&[]); // 空 payload，react_iteration 未初始化

    let result = execute_transition(&core_eval, &instruction, &payload, &[]).unwrap();
    match result {
        TransitionResult::IoRequired { io_type, params } => {
            assert_eq!(io_type, "call_external");
            assert_eq!(params.get("messages"), Some(&user_messages()));
            // tools 未提供 → 应不存在
            assert!(params.get("tools").is_none());
        }
        _ => panic!("round 1: 应返回 IoRequired"),
    }
}

// ── ReAct 循环：第三步（call_service → IoRequired） ─────────────────────

/// ReAct 第 3 步：call_service 无 I/O 结果 → 发起 io_request
#[test]
fn test_react_round3_call_service_io_request() {
    let core_eval = react_constitution();
    let payload = make_payload(&[
        ("react_iteration", iv(0)),
        ("llm_response", obj(&[("messages", user_messages())])),
    ]);
    let instruction = call_service_instr("get_weather", obj(&[("city", s("Beijing"))]));

    let result = execute_transition(&core_eval, &instruction, &payload, &[]).unwrap();
    match result {
        TransitionResult::IoRequired { io_type, params } => {
            assert_eq!(io_type, "call_service");
            assert_eq!(
                params.get("service_name").and_then(|v| v.as_str()),
                Some("get_weather")
            );
            assert_eq!(
                params
                    .get("args")
                    .and_then(|v| v.get("city"))
                    .and_then(|v| v.as_str()),
                Some("Beijing")
            );
        }
        _ => panic!("round 3: 应返回 IoRequired"),
    }
}

// ── ReAct 循环：迭代上限 ──────────────────────────────────────────────────

/// call_service 消费轮结果落 payload 后 push noop 终止循环（react_iteration 保持不变）
#[test]
fn test_react_iteration_cap_terminates_with_noop() {
    let core_eval = react_constitution();
    let payload = make_payload(&[
        ("react_iteration", iv(10)),
        ("llm_response", obj(&[("messages", user_messages())])),
        (
            "__io_results__",
            obj(&[("call_service", obj(&[("temperature", iv(25))]))]),
        ),
    ]);
    let instruction = call_service_instr("get_weather", JsonValue::empty_object());

    let result = execute_transition(&core_eval, &instruction, &payload, &[]).unwrap();
    let TransitionResult::State {
        new_payload,
        new_queue,
        rule_hits: _,
    } = result
    else {
        panic!("cap: 应返回 State")
    };

    // 队列只有 1 条 noop（循环终止）
    assert_eq!(new_queue.len(), 1);
    assert_eq!(
        new_queue[0].get("type").and_then(|v| v.as_str()),
        Some("noop")
    );
    // 计数不再增长
    assert_eq!(new_payload.get("react_iteration"), Some(&iv(10)));
    // 无 updated_messages
    assert!(new_payload.get("updated_messages").is_none());
}

// ── I/O 结果 null 清除后 exists 为 false ──────────────────────────────────

/// JSON null 视为"已清除"：exists 必须返回 false，否则陈旧结果被反复消费
#[test]
fn test_io_result_null_cleared_exists_returns_false() {
    let core_eval = react_constitution();
    // 模拟上一轮已消费 I/O 结果并用 null 清除
    let payload = make_payload(&[
        ("react_iteration", iv(1)),
        ("llm_response", obj(&[("messages", user_messages())])),
        ("__io_results__", obj(&[("call_external", JsonValue::Null)])),
    ]);
    let instruction = call_external_instr(user_messages());

    // null 视为不存在 → 应发起新的 io_request（而非消费陈旧结果）
    let result = execute_transition(&core_eval, &instruction, &payload, &[]).unwrap();
    match result {
        TransitionResult::IoRequired { io_type, .. } => {
            assert_eq!(io_type, "call_external");
        }
        TransitionResult::State { new_queue, .. } => {
            panic!(
                "null 清除后应发起第二轮 IoRequired，但得到 State (queue len={})",
                new_queue.len()
            );
        }
        TransitionResult::Ignored { .. } => {
            panic!("null 清除后应发起第二轮 IoRequired，但得到 Ignored");
        }
        TransitionResult::Halted { rule_index, reason } => {
            panic!("null 清除后应发起第二轮 IoRequired，但得到 Halted(rule_index={rule_index}, reason={reason})");
        }
    }
}

// ── 整数溢出传播 ──────────────────────────────────────────────────────────

/// 整数溢出通过 execute_transition 正确传播
#[test]
fn test_integer_overflow_propagates_through_execute_transition() {
    // 构造 set(add) 指令：向 x 加上 i64::MAX，再尝试 +1 → 溢出
    let instruction = make_instruction("increment", &[("attr", s("x")), ("delta", iv(1))]);
    let payload = make_payload(&[("x", iv(i64::MAX))]);

    // set 规则：匹配 increment 指令 → add
    let rule = branch(
        domain_instruction("increment"),
        vec![set_instr(
            "__exec__.instruction.params.attr",
            "add",
            s("__exec__.instruction.params.delta"),
        )],
        vec![],
    );
    let core_eval = vec![rule, rule_catch_all()];

    let result = execute_transition(&core_eval, &instruction, &payload, &[]);
    match result {
        Err(TcbError::IntegerOverflow { operation, .. }) => {
            assert_eq!(operation, "add");
        }
        other => panic!("应返回 IntegerOverflow，但得到: {:?}", other),
    }
}

// ── 未知指令类型 → Ignored 显式提醒 ──────────────────────────────────────

/// 未知指令类型落入 all([]) 兜底规则，TCB 显式返回 Ignored 而非静默忽略
#[test]
fn test_unknown_instruction_falls_to_catch_all() {
    let core_eval = vec![rule_noop(), rule_catch_all()];
    let instruction = make_instruction("unknown_instruction", &[]);
    let payload = make_payload(&[("x", iv(42))]);

    let result = execute_transition(&core_eval, &instruction, &payload, &[]).unwrap();
    match result {
        TransitionResult::Ignored {
            instruction_type,
            reason,
            ..
        } => {
            assert_eq!(instruction_type, "unknown_instruction");
            assert!(reason.contains("not matched"));
        }
        _ => panic!("未知指令应返回 Ignored，实际返回: {:?}", result),
    }
}

// ── 空 tool_calls → push noop ────────────────────────────────────────────

/// LLM 返回不含 tool_calls 的响应 → 消费后 push noop 终止
#[test]
fn test_no_tool_calls_terminates_via_noop() {
    let core_eval = react_constitution();
    let llm_response = obj(&[("messages", user_messages())]); // 无 tool_calls
    let payload = make_payload(&[
        ("react_iteration", iv(0)),
        (
            "__io_results__",
            obj(&[("call_external", llm_response.clone())]),
        ),
    ]);
    let instruction = call_external_instr(user_messages());

    let result = execute_transition(&core_eval, &instruction, &payload, &[]).unwrap();
    let TransitionResult::State {
        new_payload,
        new_queue,
        rule_hits: _,
    } = result
    else {
        panic!("应返回 State")
    };

    // 无 call_service，只有 noop
    assert_eq!(new_queue.len(), 1);
    assert_eq!(
        new_queue[0].get("type").and_then(|v| v.as_str()),
        Some("noop")
    );
    // llm_response 已消费（不含 tool_calls 字段，但整个对象被保存）
    assert_eq!(new_payload.get("llm_response"), Some(&llm_response));
}

// ── 空 tool_calls 数组 → push noop ───────────────────────────────────────

/// tool_calls 为空数组时消费轮同样 push noop 终止
#[test]
fn test_empty_tool_calls_array_triggers_noop() {
    let core_eval = react_constitution();
    let llm_response = obj(&[
        ("tool_calls", arr(vec![])), // 空数组
        ("messages", user_messages()),
    ]);
    let payload = make_payload(&[
        ("react_iteration", iv(0)),
        (
            "__io_results__",
            obj(&[("call_external", llm_response.clone())]),
        ),
    ]);
    let instruction = call_external_instr(user_messages());

    let result = execute_transition(&core_eval, &instruction, &payload, &[]).unwrap();
    let TransitionResult::State { new_queue, .. } = result else {
        panic!("应返回 State")
    };

    // 空 tool_calls 不生成 call_service，只 push noop（collect 已退役，消费轮统一终止）
    assert_eq!(new_queue.len(), 1);
    assert_eq!(
        new_queue[0].get("type").and_then(|v| v.as_str()),
        Some("noop")
    );
}

// ── 空队列输入 ────────────────────────────────────────────────────────────

/// 空队列作为输入，noop 指令返回空队列
#[test]
fn test_empty_queue_input() {
    let core_eval = react_constitution();
    let instruction = make_instruction("noop", &[]);
    let payload = make_payload(&[("x", iv(1))]);

    let result = execute_transition(&core_eval, &instruction, &payload, &[]).unwrap();
    match result {
        TransitionResult::State {
            new_payload,
            new_queue,
            ..
        } => {
            assert_eq!(new_payload.get("x"), Some(&iv(1)));
            assert!(new_queue.is_empty(), "空队列输入应返回空队列");
        }
        _ => panic!("应返回 State"),
    }
}

// ── 非空队列输入，push 指令附加到前端 ─────────────────────────────────────

/// push 指令将新指令插入队列前端，保留原有队列
#[test]
fn test_push_appends_to_front_of_existing_queue() {
    let existing_queue = vec![make_instruction("decrement", &[("delta", iv(1))])];

    // 构造规则：push increment 到队列前端
    let rule = branch(
        domain_instruction("spawn"),
        vec![push_instr(vec![make_instruction(
            "increment",
            &[("delta", iv(5))],
        )])],
        vec![],
    );
    let core_eval = vec![rule, rule_catch_all()];
    let instruction = make_instruction("spawn", &[]);
    let payload = make_payload(&[("x", iv(0))]);

    let result = execute_transition(&core_eval, &instruction, &payload, &existing_queue).unwrap();
    match result {
        TransitionResult::State { new_queue, .. } => {
            // 新队列 = [increment(5), decrement(1)]
            assert_eq!(new_queue.len(), 2);
            assert_eq!(
                new_queue[0].get("type").and_then(|v| v.as_str()),
                Some("increment")
            );
            assert_eq!(
                new_queue[1].get("type").and_then(|v| v.as_str()),
                Some("decrement")
            );
        }
        _ => panic!("应返回 State"),
    }
}

// ── 域评估：not(not(exists)) 双否定 ──────────────────────────────────────

/// 双否定域评估：not(not(exists(...))) == exists(...)
#[test]
fn test_domain_double_negation() {
    let state = obj(&[(
        "__exec__",
        obj(&[("payload", obj(&[("flag", JsonValue::Bool(true))]))]),
    )]);

    // exists: true
    let exists = domain_exists("__exec__.payload.flag");
    assert!(evaluate_domain(&exists, &state).unwrap());

    // not(exists): false
    let not_exists = domain_not(domain_exists("__exec__.payload.flag"));
    assert!(!evaluate_domain(&not_exists, &state).unwrap());

    // not(not(exists)): true（双否定还原）
    let double_not = domain_not(domain_not(domain_exists("__exec__.payload.flag")));
    assert!(evaluate_domain(&double_not, &state).unwrap());
}

// ── 路径解析：非对象/非数组路径返回 None ──────────────────────────────────

/// 对非对象类型进行字段访问 → None（不 panic）
#[test]
fn test_path_resolve_on_non_object_returns_none() {
    let state = obj(&[("value", iv(42))]);

    // value 是整数，对其访问子字段应返回 None
    let result = resolve_path(&state, "value.nested");
    assert!(result.is_none(), "整数不应有子字段");

    // missing 路径返回 None
    let result = resolve_path(&state, "missing.field");
    assert!(result.is_none());
}

// ── 多规则链：set → increment → decrement 顺序执行 ────────────────────────

/// 模拟多条规则顺序执行：set x=10 → increment x+=5 → decrement x-=3
#[test]
fn test_rule_chain_sequential_execution() {
    let rules = vec![
        branch(
            domain_instruction("set"),
            vec![set_instr(
                "__exec__.instruction.params.attr",
                "set",
                s("__exec__.instruction.params.value"),
            )],
            vec![],
        ),
        branch(
            domain_instruction("increment"),
            vec![set_instr(
                "__exec__.instruction.params.attr",
                "add",
                s("__exec__.instruction.params.delta"),
            )],
            vec![],
        ),
        branch(
            domain_instruction("decrement"),
            vec![set_instr(
                "__exec__.instruction.params.attr",
                "sub",
                s("__exec__.instruction.params.delta"),
            )],
            vec![],
        ),
        rule_catch_all(),
    ];

    // 第 1 步：set x=10
    let r1 = execute_transition(
        &rules,
        &make_instruction("set", &[("attr", s("x")), ("value", iv(10))]),
        &make_payload(&[]),
        &[],
    )
    .unwrap();
    let (p1, q1) = match r1 {
        TransitionResult::State {
            new_payload,
            new_queue,
            ..
        } => (new_payload, new_queue),
        _ => panic!("step 1: 应返回 State"),
    };
    assert_eq!(p1.get("x"), Some(&iv(10)));

    // 第 2 步：increment x += 5
    let r2 = execute_transition(
        &rules,
        &make_instruction("increment", &[("attr", s("x")), ("delta", iv(5))]),
        &p1,
        &q1,
    )
    .unwrap();
    let (p2, q2) = match r2 {
        TransitionResult::State {
            new_payload,
            new_queue,
            ..
        } => (new_payload, new_queue),
        _ => panic!("step 2: 应返回 State"),
    };
    assert_eq!(p2.get("x"), Some(&iv(15)));

    // 第 3 步：decrement x -= 3
    let r3 = execute_transition(
        &rules,
        &make_instruction("decrement", &[("attr", s("x")), ("delta", iv(3))]),
        &p2,
        &q2,
    )
    .unwrap();
    let (p3, _q3) = match r3 {
        TransitionResult::State {
            new_payload,
            new_queue,
            ..
        } => (new_payload, new_queue),
        _ => panic!("step 3: 应返回 State"),
    };
    assert_eq!(p3.get("x"), Some(&iv(12)));
}

// ── while_loop 死循环防护（2026-08-17 审计修复根因的专项回归）───────────

/// 带步数上限的队列驱动多步执行（模拟反应器主循环）
///
/// 循环 pop 队首指令调用 `execute_transition`，直到队列排空、触发 I/O、
/// 执行错误或达到步数上限。返回 `(已执行步数, 最终 payload, 剩余队列)`。
///
/// 步数上限是本辅助函数的核心价值：死循环场景（队列永不排空）下
/// 测试快速返回观察死循环形态，而非无限挂起拖垮测试套件——对应
/// reactor 层 `max_rounds` 防线与差分测试 `simulate_execution` 上限。
fn run_queue_with_step_limit(
    core_eval: &[JsonValue],
    initial_payload: JsonValue,
    initial_queue: Vec<JsonValue>,
    max_steps: usize,
) -> (usize, JsonValue, Vec<JsonValue>) {
    let mut payload = initial_payload;
    let mut queue = initial_queue;
    let mut steps = 0usize;
    while steps < max_steps {
        if queue.is_empty() {
            return (steps, payload, queue);
        }
        let instruction = queue.remove(0);
        steps += 1;
        match execute_transition(core_eval, &instruction, &payload, &queue) {
            Ok(TransitionResult::State {
                new_payload,
                new_queue,
                ..
            }) => {
                payload = new_payload;
                queue = new_queue;
            }
            Ok(TransitionResult::Ignored {
                instruction_type,
                reason,
                ..
            }) => {
                panic!(
                    "Step {}: 指令类型 '{}' 被忽略: {}",
                    steps, instruction_type, reason
                );
            }
            // I/O 触发、enforce 拦截或错误：停止推进（与反应器行为一致：
            // Halted 时指令被拒，payload/queue 保持进入该步前的原样）
            Ok(TransitionResult::IoRequired { .. })
            | Ok(TransitionResult::Halted { .. })
            | Err(_) => return (steps, payload, queue),
        }
    }
    (steps, payload, queue)
}

/// 专项回归：while_loop body 误用 `set + operation:add` 导致死循环
///
/// # 根因（2026-08-17 差分测试挂起事故）
///
/// 宪法业务 `set` 规则硬编码元指令 `operation="set"`（覆盖语义），
/// 业务指令 params 中的 `operation` 字段（如 `add`）被静默忽略。
/// 误用 `set+add` 做 while_loop body 累加 → 计数器每轮被覆盖回常量
/// → `lt` 条件永真 → push `[body, while_loop自身]` 无限循环，
/// 队列永不排空（无上限的多步模拟曾因此永久挂起）。
///
/// # 固化的三道防线
///
/// 1. **语义防线**：业务 `set` 的 `operation` 字段确实被忽略（覆盖语义）。
///    若未来让 set 尊重该字段，本测试失败——提醒这是宪法语义变更，
///    需同步 core_eval.json constraints 与版本说明。
/// 2. **死循环形态**：`set+add` body 在步数上限内队列始终非空、
///    计数器恒为常量（用上限截断观察，测试本身不挂起）。
/// 3. **对照组**：`increment` body（宪法规定的加法指令）正常终止，
///    计数器收敛到边界值——同一 condition 下两种 body 的行为差异
///    即覆盖语义与累加语义的差异。
#[test]
fn test_while_loop_set_add_body_deadloop_regression() {
    // 宪法规则子集（与 core_eval.json v0.3.1 对应规则结构一致）
    let rule_increment = branch(
        domain_instruction("increment"),
        vec![set_instr(
            "__exec__.instruction.params.attr",
            "add",
            s("__exec__.instruction.params.delta"),
        )],
        vec![],
    );
    let rule_business_set = branch(
        domain_instruction("set"),
        vec![set_instr(
            "__exec__.instruction.params.attr",
            // 硬编码 "set"：业务指令的 operation 字段被忽略（根因所在）
            "set",
            s("__exec__.instruction.params.value"),
        )],
        vec![],
    );
    let rule_while_loop = branch(
        domain_instruction("while_loop"),
        vec![branch(
            s("__exec__.instruction.params.condition"),
            vec![push_instr(vec![
                s("__exec__.instruction.params.body"),
                s("__exec__.instruction"),
            ])],
            vec![],
        )],
        vec![],
    );
    let core_eval = vec![
        rule_increment,
        rule_business_set,
        rule_while_loop,
        rule_catch_all(),
    ];

    // condition: counter < 3
    let condition = domain_lt("__exec__.payload.counter", 3);

    // —— 防线 1：业务 set 的 operation:"add" 被忽略（覆盖语义）——
    // counter=5 --set(value=1)--> 1；若 set 尊重 add 则应为 6
    let set_add_body = make_instruction(
        "set",
        &[
            ("attr", s("counter")),
            ("operation", s("add")),
            ("value", iv(1)),
        ],
    );
    let r = execute_transition(
        &core_eval,
        &set_add_body,
        &make_payload(&[("counter", iv(5))]),
        &[],
    )
    .unwrap();
    match r {
        TransitionResult::State { new_payload, .. } => {
            assert_eq!(
                new_payload.get("counter"),
                Some(&iv(1)),
                "业务 set 必须是覆盖语义：operation 字段被宪法忽略（core_eval.json constraints）"
            );
        }
        _ => panic!("set 指令应成功执行"),
    }

    // —— 防线 2：set+add 作为 while_loop body → 死循环形态 ——
    let deadloop_instr = make_instruction(
        "while_loop",
        &[("condition", condition.clone()), ("body", set_add_body)],
    );
    let (steps, payload, queue) = run_queue_with_step_limit(
        &core_eval,
        make_payload(&[("counter", iv(0))]),
        vec![deadloop_instr],
        1000,
    );
    assert_eq!(steps, 1000, "死循环场景必须耗尽步数上限（队列永不排空）");
    assert!(!queue.is_empty(), "死循环形态：达到上限后队列仍非空");
    assert_eq!(
        payload.get("counter"),
        Some(&iv(1)),
        "计数器被覆盖回常量（不增长）——死循环根因的形态固化"
    );

    // —— 防线 3：对照组，increment body 正常终止 ——
    let inc_body = make_instruction("increment", &[("attr", s("counter")), ("delta", iv(1))]);
    let good_instr = make_instruction(
        "while_loop",
        &[("condition", condition), ("body", inc_body)],
    );
    let (steps, payload, queue) = run_queue_with_step_limit(
        &core_eval,
        make_payload(&[("counter", iv(0))]),
        vec![good_instr],
        1000,
    );
    assert!(queue.is_empty(), "increment body：队列排空（正常终止）");
    assert!(steps < 1000, "increment body：步数应远小于上限");
    assert_eq!(
        payload.get("counter"),
        Some(&iv(3)),
        "counter 从 0 递增到 3（counter < 3 不再满足，循环退出）"
    );
}

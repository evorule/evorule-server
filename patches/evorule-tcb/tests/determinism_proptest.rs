// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! evorule-tcb 确定性属性测试（proptest）
//!
//! TCB 是确定性计算内核，核心承诺：**相同输入 → 相同输出**。
//! 本文件用随机输入（随机 payload / 随机指令 / 随机 core_eval 规则）
//! 对 `execute_transition` 进行大量采样验证：
//!
//! 1. **确定性**：同一输入重复执行多次，输出必须字节级一致
//! 2. **纯函数**：执行不修改输入（输入在调用前后保持不变）
//! 3. **永不 panic**：任意合法输入下 `execute_transition` 不 panic
//!
//! # 运行方式
//! ```bash
//! cargo test -p evorule-tcb --test determinism_proptest
//! ```

extern crate alloc;

use alloc::vec::Vec;

use evorule_tcb::{execute_transition, JsonValue, ObjectMap};
use proptest::prelude::*;

// ============================================================================
// 随机 JsonValue 生成器
// ============================================================================

/// 随机 JsonValue（限制深度/宽度，保证生成快速且覆盖各类类型）
fn json_value_strategy() -> impl Strategy<Value = JsonValue> {
    let leaf: BoxedStrategy<JsonValue> = prop_oneof![
        1 => Just(JsonValue::Null),
        1 => Just(JsonValue::Bool(true)),
        1 => Just(JsonValue::Bool(false)),
        1 => (-1000i64..1000i64).prop_map(JsonValue::Integer),
        1 => ".*".prop_map(|s| JsonValue::string(s.to_string())),
    ]
    .boxed();

    leaf.prop_recursive(3, 20, 10, |inner| {
        prop_oneof![
            // 数组
            1 => proptest::collection::vec(inner.clone(), 0..4).prop_map(JsonValue::Array),
            // 对象（键为简单字符串）
            1 => proptest::collection::vec((".*".prop_map(|s| s.to_string()), inner.clone()), 0..4)
                .prop_map(|pairs| {
                    let mut map = ObjectMap::new();
                    for (k, v) in pairs {
                        map.insert(k, v);
                    }
                    JsonValue::object(map)
                }),
        ]
        .boxed()
    })
}

/// 随机 payload：总是对象（__exec__ 上下文需要）
fn payload_strategy() -> impl Strategy<Value = JsonValue> {
    proptest::collection::vec(
        (".*".prop_map(|s| s.to_string()), json_value_strategy()),
        0..8,
    )
    .prop_map(|pairs| {
        let mut map = ObjectMap::new();
        for (k, v) in pairs {
            map.insert(k, v);
        }
        JsonValue::object(map)
    })
}

/// 随机指令：各种合法/边界指令类型
fn instruction_strategy() -> impl Strategy<Value = JsonValue> {
    prop_oneof![
        // noop
        1 => Just(JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("noop")),
        ])),
        // set（attr 固定为 "x"，值随机）
        1 => (".*".prop_map(|s| s.to_string()), json_value_strategy()).prop_map(|(attr, value)| {
            JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("set")),
                ("params", JsonValue::object_from_pairs(&[
                    ("attr", JsonValue::string(attr)),
                    ("operation", JsonValue::string("set")),
                    ("value", value),
                ])),
            ])
        }),
        // 未知指令类型（验证 all([]) 兜底）
        1 => Just(JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("unknown_instruction_xyz")),
        ])),
    ]
    .boxed()
}

/// 随机 core_eval：一组规则（noop / set / branch）
fn core_eval_strategy() -> impl Strategy<Value = Vec<JsonValue>> {
    proptest::collection::vec(
        prop_oneof![
            // noop 规则
            1 => Just(JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("noop")),
            ])),
            // set 规则：无条件 set(x, set, 随机值)
            1 => json_value_strategy().prop_map(|value| {
                JsonValue::object_from_pairs(&[
                    ("type", JsonValue::string("set")),
                    ("params", JsonValue::object_from_pairs(&[
                        ("attr", JsonValue::string("x")),
                        ("operation", JsonValue::string("set")),
                        ("value", value),
                    ])),
                ])
            }),
            // branch：all([]) 无条件 true → on_true 执行 set(x)
            1 => Just(JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("branch")),
                ("params", JsonValue::object_from_pairs(&[
                    ("domain", JsonValue::object_from_pairs(&[
                        ("type", JsonValue::string("all")),
                    ])),
                    ("on_true", JsonValue::Array(vec![JsonValue::object_from_pairs(&[
                        ("type", JsonValue::string("set")),
                        ("params", JsonValue::object_from_pairs(&[
                            ("attr", JsonValue::string("y")),
                            ("operation", JsonValue::string("set")),
                            ("value", JsonValue::Integer(1)),
                        ])),
                    ])])),
                    ("on_false", JsonValue::Array(vec![])),
                ])),
            ])),
        ]
        .boxed(),
        0..8,
    )
}

// ============================================================================
// 属性 1: 确定性 —— 相同输入 → 相同输出
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn deterministic_same_input_same_output(
        payload in payload_strategy(),
        instruction in instruction_strategy(),
        core_eval in core_eval_strategy(),
    ) {
        // 同一输入执行两次，结果必须完全一致
        let r1 = execute_transition(&core_eval, &instruction, &payload, &[]);
        let r2 = execute_transition(&core_eval, &instruction, &payload, &[]);

        match (r1, r2) {
            (Ok(a), Ok(b)) => {
                match (a, b) {
                    (evorule_tcb::TransitionResult::State { new_payload: p1, new_queue: q1, rule_hits: h1 },
                     evorule_tcb::TransitionResult::State { new_payload: p2, new_queue: q2, rule_hits: h2 }) => {
                        assert_eq!(p1, p2, "确定性被破坏: 相同输入产生不同 payload");
                        assert_eq!(q1, q2, "确定性被破坏: 相同输入产生不同 queue");
                        assert_eq!(h1, h2, "确定性被破坏: 相同输入产生不同命中归因");
                    }
                    (evorule_tcb::TransitionResult::Ignored { instruction_type: t1, reason: r1, rule_hits: h1 },
                     evorule_tcb::TransitionResult::Ignored { instruction_type: t2, reason: r2, rule_hits: h2 }) => {
                        assert_eq!(t1, t2, "确定性被破坏: 不同 instruction_type");
                        assert_eq!(r1, r2, "确定性被破坏: 不同 reason");
                        assert_eq!(h1, h2, "确定性被破坏: 相同输入产生不同命中归因");
                    }
                    (evorule_tcb::TransitionResult::IoRequired { io_type: t1, params: rp1 },
                     evorule_tcb::TransitionResult::IoRequired { io_type: t2, params: rp2 }) => {
                        assert_eq!(t1, t2, "确定性被破坏: 不同 io_type");
                        assert_eq!(rp1, rp2, "确定性被破坏: 不同 io params");
                    }
                    (evorule_tcb::TransitionResult::Halted { rule_index: i1, reason: s1 },
                     evorule_tcb::TransitionResult::Halted { rule_index: i2, reason: s2 }) => {
                        assert_eq!(i1, i2, "确定性被破坏: 不同 enforce 命中下标");
                        assert_eq!(s1, s2, "确定性被破坏: 不同 violation reason");
                    }
                    // 两个结果类型不同
                    _ => {
                        unreachable!("相同输入产生不同结果类型");
                    }
                }
            }
            (Err(a), Err(b)) => {
                assert_eq!(a, b, "确定性被破坏: 相同输入产生不同错误");
            }
            _ => {
                unreachable!("相同输入一个成功一个失败: Ok vs Err");
            }
        }
    }
}

// ============================================================================
// 属性 2: 纯函数 —— 执行不修改输入
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn pure_function_does_not_mutate_input(
        payload in payload_strategy(),
        instruction in instruction_strategy(),
        core_eval in core_eval_strategy(),
    ) {
        // 克隆输入快照
        let payload_snapshot = payload.clone();
        let instruction_snapshot = instruction.clone();
        let core_eval_snapshot = core_eval.clone();

        // 执行（无论成功/失败）
        let _ = execute_transition(&core_eval, &instruction, &payload, &[]);

        // 执行后输入必须保持不变
        assert_eq!(payload, payload_snapshot, "纯函数被破坏: execute_transition 修改了 payload");
        assert_eq!(instruction, instruction_snapshot, "纯函数被破坏: execute_transition 修改了 instruction");
        assert_eq!(core_eval, core_eval_snapshot, "纯函数被破坏: execute_transition 修改了 core_eval");
    }
}

// ============================================================================
// 属性 3: 永不 panic —— 任意输入执行不 panic（proptest 断言失败即 panic）
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn never_panics_on_valid_input(
        payload in payload_strategy(),
        instruction in instruction_strategy(),
        core_eval in core_eval_strategy(),
        queue in proptest::collection::vec(instruction_strategy(), 0..4),
    ) {
        // 执行不应 panic（返回 Ok/Err 均可，但不能崩溃）
        let _ = execute_transition(&core_eval, &instruction, &payload, &queue);
    }
}

// ============================================================================
// 属性 4: 队列传播 —— State 结果的 new_queue 必须是合法数组
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn state_result_queue_is_array(
        payload in payload_strategy(),
        instruction in instruction_strategy(),
        core_eval in core_eval_strategy(),
    ) {
        if let Ok(evorule_tcb::TransitionResult::State { new_queue, .. }) =
            execute_transition(&core_eval, &instruction, &payload, &[])
        {
            // new_queue 是 Vec<JsonValue>，天然是数组；验证其中的元素是合法 JsonValue
            for item in &new_queue {
                let _: &JsonValue = item;
            }
        }
    }
}

// ============================================================================
// 属性 5: 多轮串联 —— 连续执行 State 结果仍保持确定性
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn sequential_transitions_deterministic(
        payload in payload_strategy(),
        instruction in instruction_strategy(),
        core_eval in core_eval_strategy(),
    ) {
        // 执行两轮
        let mut run1_payload = payload.clone();
        let mut run2_payload = payload.clone();

        for _ in 0..3 {
            let r1 = execute_transition(&core_eval, &instruction, &run1_payload, &[]);
            let r2 = execute_transition(&core_eval, &instruction, &run2_payload, &[]);
            match (r1, r2) {
                (Ok(evorule_tcb::TransitionResult::State { new_payload: p1, .. }),
                 Ok(evorule_tcb::TransitionResult::State { new_payload: p2, .. })) => {
                    assert_eq!(p1, p2, "串联执行失去确定性");
                    run1_payload = p1;
                    run2_payload = p2;
                }
                _ => break, // IoRequired/Err 时停止串联
            }
        }
    }
}

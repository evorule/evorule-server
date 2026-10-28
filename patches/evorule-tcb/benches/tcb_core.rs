// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 性能基准 — TCB 核心热路径
//!
//! 041 v2.0：建立 TCB 核心性能基线。
//! 覆盖 execute_transition、路径解析、domain 评估等核心热路径。
//!
//! # 运行
//! ```bash
//! cargo bench -p evorule-tcb --bench tcb_core
//! ```
#![allow(clippy::unwrap_used, clippy::expect_used)]

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use evorule_tcb::{execute_transition, JsonValue, TransitionResult};

// ===== 辅助函数 =====

/// 构造 increment 指令
fn make_instruction(attr: &str, delta: i64) -> JsonValue {
    JsonValue::object_from_pairs(&[
        ("type", JsonValue::string("increment")),
        (
            "params",
            JsonValue::object_from_pairs(&[
                ("attr", JsonValue::string(attr)),
                ("delta", JsonValue::Integer(delta)),
            ]),
        ),
    ])
}

/// 构造 set 指令
fn make_set_instruction(attr: &str, value: i64) -> JsonValue {
    JsonValue::object_from_pairs(&[
        ("type", JsonValue::string("set")),
        (
            "params",
            JsonValue::object_from_pairs(&[
                ("attr", JsonValue::string(attr)),
                ("value", JsonValue::Integer(value)),
            ]),
        ),
    ])
}

/// 构造 noop 指令
fn make_noop_instruction() -> JsonValue {
    JsonValue::object_from_pairs(&[("type", JsonValue::string("noop"))])
}

/// 构造简单 payload
fn make_payload() -> JsonValue {
    JsonValue::object_from_pairs(&[
        ("x", JsonValue::Integer(10)),
        ("y", JsonValue::Integer(20)),
        ("counter", JsonValue::Integer(0)),
    ])
}

/// 构造 core_eval 规则列表（包含 increment/decrement/set/noop 规则）
fn make_core_eval() -> Vec<JsonValue> {
    vec![
        // increment 规则
        JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("instruction")),
                            ("instruction_type", JsonValue::string("increment")),
                        ]),
                    ),
                    (
                        "on_true",
                        JsonValue::array(vec![JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("set")),
                            (
                                "params",
                                JsonValue::object_from_pairs(&[
                                    (
                                        "attr",
                                        JsonValue::string("__exec__.instruction.params.attr"),
                                    ),
                                    ("operation", JsonValue::string("add")),
                                    (
                                        "value",
                                        JsonValue::string("__exec__.instruction.params.delta"),
                                    ),
                                ]),
                            ),
                        ])]),
                    ),
                ]),
            ),
        ]),
        // set 规则
        JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("instruction")),
                            ("instruction_type", JsonValue::string("set")),
                        ]),
                    ),
                    (
                        "on_true",
                        JsonValue::array(vec![JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("set")),
                            (
                                "params",
                                JsonValue::object_from_pairs(&[
                                    (
                                        "attr",
                                        JsonValue::string("__exec__.instruction.params.attr"),
                                    ),
                                    ("operation", JsonValue::string("set")),
                                    (
                                        "value",
                                        JsonValue::string("__exec__.instruction.params.value"),
                                    ),
                                ]),
                            ),
                        ])]),
                    ),
                ]),
            ),
        ]),
        // noop 规则
        JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("instruction")),
                            ("instruction_type", JsonValue::string("noop")),
                        ]),
                    ),
                    ("on_true", JsonValue::array(vec![])),
                ]),
            ),
        ]),
    ]
}

// ===== Benchmark 函数 =====

/// Benchmark 1: execute_transition - increment 指令
/// 核心热路径：执行 increment 指令
fn bench_execute_transition_increment(c: &mut Criterion) {
    let core_eval = make_core_eval();
    let instruction = make_instruction("x", 5);
    let payload = make_payload();

    c.bench_function("tcb/execute_transition_increment", |b| {
        b.iter(|| {
            let result = execute_transition(
                black_box(&core_eval),
                black_box(&instruction),
                black_box(&payload),
                black_box(&[]),
            );
            let _ = black_box(result);
        });
    });
}

/// Benchmark 2: execute_transition - set 指令
/// 核心热路径：执行 set 指令
fn bench_execute_transition_set(c: &mut Criterion) {
    let core_eval = make_core_eval();
    let instruction = make_set_instruction("x", 42);
    let payload = make_payload();

    c.bench_function("tcb/execute_transition_set", |b| {
        b.iter(|| {
            let result = execute_transition(
                black_box(&core_eval),
                black_box(&instruction),
                black_box(&payload),
                black_box(&[]),
            );
            let _ = black_box(result);
        });
    });
}

/// Benchmark 3: execute_transition - noop 指令
/// 最轻量的指令执行
fn bench_execute_transition_noop(c: &mut Criterion) {
    let core_eval = make_core_eval();
    let instruction = make_noop_instruction();
    let payload = make_payload();

    c.bench_function("tcb/execute_transition_noop", |b| {
        b.iter(|| {
            let result = execute_transition(
                black_box(&core_eval),
                black_box(&instruction),
                black_box(&payload),
                black_box(&[]),
            );
            let _ = black_box(result);
        });
    });
}

/// Benchmark 4: execute_transition - 1000 次 increment 循环
/// 模拟连续操作场景
fn bench_execute_transition_1000_increments(c: &mut Criterion) {
    let core_eval = make_core_eval();
    let instruction = make_instruction("counter", 1);
    let initial_payload = make_payload();

    c.bench_function("tcb/execute_transition_1000_increments", |b| {
        b.iter(|| {
            let mut payload = initial_payload.clone();
            for _ in 0..1000 {
                let result = execute_transition(&core_eval, &instruction, &payload, &[]);
                match result {
                    Ok(TransitionResult::State { new_payload, .. }) => {
                        payload = new_payload;
                    }
                    _ => break,
                }
            }
            black_box(payload);
        });
    });
}

/// Benchmark 5: JsonValue 构造性能
/// 数据结构基础操作
fn bench_jsonvalue_construction(c: &mut Criterion) {
    c.bench_function("tcb/jsonvalue_construction", |b| {
        b.iter(|| {
            let value = JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("test")),
                ("value", JsonValue::Integer(42)),
                ("flag", JsonValue::Bool(true)),
                ("name", JsonValue::string("hello")),
                (
                    "nested",
                    JsonValue::object_from_pairs(&[
                        ("a", JsonValue::Integer(1)),
                        ("b", JsonValue::Integer(2)),
                    ]),
                ),
            ]);
            black_box(value);
        });
    });
}

/// Benchmark 6: JsonValue 字段访问性能
/// 路径解析基础操作
fn bench_jsonvalue_field_access(c: &mut Criterion) {
    let value = JsonValue::object_from_pairs(&[
        ("x", JsonValue::Integer(10)),
        ("y", JsonValue::Integer(20)),
        (
            "nested",
            JsonValue::object_from_pairs(&[
                ("a", JsonValue::Integer(100)),
                ("b", JsonValue::Integer(200)),
            ]),
        ),
    ]);

    c.bench_function("tcb/jsonvalue_field_access", |b| {
        b.iter(|| {
            let x = value.get("x").and_then(|v| v.as_i64());
            let y = value.get("y").and_then(|v| v.as_i64());
            let nested_a = value
                .get("nested")
                .and_then(|v| v.get("a"))
                .and_then(|v| v.as_i64());
            black_box((x, y, nested_a));
        });
    });
}

/// Benchmark 7: core_eval 规则匹配性能
/// 规则遍历与匹配
fn bench_rule_matching(c: &mut Criterion) {
    let core_eval = make_core_eval();
    let instruction = make_instruction("x", 1);
    let payload = make_payload();

    c.bench_function("tcb/rule_matching", |b| {
        b.iter(|| {
            // 测量规则匹配开销（实际执行 increment 指令触发规则匹配）
            let result = execute_transition(
                black_box(&core_eval),
                black_box(&instruction),
                black_box(&payload),
                black_box(&[]),
            );
            // 只关心是否成功匹配
            let is_state = matches!(result, Ok(TransitionResult::State { .. }));
            black_box(is_state);
        });
    });
}

/// Benchmark 8: 队列操作性能
/// 指令队列处理
fn bench_queue_operations(c: &mut Criterion) {
    let core_eval = make_core_eval();
    let instruction = make_set_instruction("x", 100);
    let payload = make_payload();

    // 模拟有队列的场景
    let queue = vec![
        make_instruction("x", 1),
        make_instruction("y", 2),
        make_set_instruction("counter", 0),
    ];

    c.bench_function("tcb/queue_operations", |b| {
        b.iter(|| {
            let result = execute_transition(
                black_box(&core_eval),
                black_box(&instruction),
                black_box(&payload),
                black_box(&queue),
            );
            let _ = black_box(result);
        });
    });
}

criterion_group!(
    benches,
    bench_execute_transition_increment,
    bench_execute_transition_set,
    bench_execute_transition_noop,
    bench_execute_transition_1000_increments,
    bench_jsonvalue_construction,
    bench_jsonvalue_field_access,
    bench_rule_matching,
    bench_queue_operations,
);
criterion_main!(benches);

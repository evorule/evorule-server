// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 载体同一性 · 差分验证 harness（DEV-6 证据形态 B · T1/T2）
//!
//! 在生产载体（`cfg(not(kani))`：真实 `BTreeMap`、`MAX_DOMAIN_DEPTH=64`）上，
//! 以有界代表域实证「Kani 替身上验证的属性同样成立于生产载体」，弥补
//! [build-config-comparison.md](../verification/carrier-identity/build-config-comparison.md)
//! 所列 D1–D3（T1）与 D4（T2）的行为级偏差。
//!
//! 载体说明：tests/ 仅在 `cargo test` 下编译；`kani` cfg 仅由 Kani 工具链注入，
//! stable `cargo test` 天然不进入 Kani 翻译层，无需额外 cfg 旗标。
//!
//! 界声明（R2）：T1 键数 ≤ 64（对齐 D1 替身验证键量级），值字符串长度 ≤ 16；
//! T2 嵌套深度 ≤ 65（恰好跨过生产上界 64 的两侧）。

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    clippy::iter_kv_map
)]

use evorule_tcb::domain::evaluate_domain;
use evorule_tcb::{JsonValue, ObjectMap};

// ===== T1：JsonValue / ObjectMap（对应 D1–D3）=====

/// `BTreeMap` 后端在乱序插入下迭代序确定（字典序），两次独立构造完全一致。
/// 实证 D1 论证「`ObjectMap` 迭代序与 `BTreeMap` 一致」在生产载体成立。
#[test]
fn t1a_objectmap_iter_order_deterministic() {
    let keys: Vec<String> = (0..64).map(|i| format!("k{i:03}")).collect();

    let build = || {
        let mut obj = ObjectMap::new();
        // 逆序插入：迭代序只能由 BTreeMap 字典序保证，与插入序无关
        for (n, key) in keys.iter().enumerate().rev() {
            obj.insert(key.clone(), JsonValue::string(format!("v{n:03}")));
        }
        obj
    };

    let a = build();
    let b = build();

    // ⚠️ 不可改用 .keys()：`cargo kani --tests` 在 kani cfg 下 ObjectMap=KaniMap（无 keys 方法），
    // iter().map 取 key 为三闸（stable test / clippy / kani）兼容写法；iter_kv_map 由文件头 allow 豁免。
    let seq_a: Vec<&String> = a.iter().map(|(k, _)| k).collect();
    let seq_b: Vec<&String> = b.iter().map(|(k, _)| k).collect();
    assert_eq!(seq_a, seq_b, "两次独立构造的迭代序必须一致（确定性）");
    assert_eq!(
        seq_a,
        keys.iter().collect::<Vec<_>>(),
        "迭代序必须为字典序（BTreeMap 生产语义）"
    );
}

/// `JsonValue` object 构造 → 访问往返 no-panic 且值稳定；
/// 嵌套对象（含 null）访问正常（对应 D2/D3 的 null 路径）。
#[test]
fn t1b_jsonvalue_construct_access_roundtrip() {
    let mut inner = JsonValue::empty_object();
    inner.insert("flag".to_string(), JsonValue::Integer(1));
    inner.insert("empty".to_string(), JsonValue::Null);

    let mut root = JsonValue::empty_object();
    root.insert("name".to_string(), JsonValue::string("probe"));
    root.insert("inner".to_string(), inner);

    assert_eq!(root.get("name"), Some(&JsonValue::string("probe")));
    let inner_back = root.get("inner").expect("inner 必须可读回");
    assert_eq!(inner_back.get("flag"), Some(&JsonValue::Integer(1)));
    assert_eq!(inner_back.get("empty"), Some(&JsonValue::Null));
    assert_eq!(root.get("missing"), None, "缺失键必须返回 None 而非 panic");
}

// ===== T2：域求值（对应 D4）=====

/// 构造执行状态 `{"__exec__": {"instruction": {"type": "noop"}, "payload": {"x": 10}}}`
fn exec_state() -> JsonValue {
    let mut payload = ObjectMap::new();
    payload.insert("x".to_string(), JsonValue::Integer(10));
    let mut instruction = ObjectMap::new();
    instruction.insert("type".to_string(), JsonValue::string("noop"));
    let mut exec = ObjectMap::new();
    exec.insert("payload".to_string(), JsonValue::Object(payload));
    exec.insert("instruction".to_string(), JsonValue::Object(instruction));
    let mut root = ObjectMap::new();
    root.insert("__exec__".to_string(), JsonValue::Object(exec));
    JsonValue::Object(root)
}

fn eq_x10() -> JsonValue {
    JsonValue::object_from_pairs(&[
        ("type", JsonValue::string("eq")),
        ("path", JsonValue::string("__exec__.payload.x")),
        ("value", JsonValue::Integer(10)),
    ])
}

/// 深度 ≤ 64（生产上界）内有界、no-panic、结果正确。
/// 直接实证 D4 边界化所要求的「深度 64 下域求值仍成立」。
#[test]
fn t2a_domain_eval_bounded_depth64_ok() {
    let state = exec_state();
    // 恰好 64 层 not 包裹（最内 eq 处于 depth=64，不超界）
    let mut domain = eq_x10();
    for _ in 0..64 {
        domain =
            JsonValue::object_from_pairs(&[("type", JsonValue::string("not")), ("inner", domain)]);
    }
    // 64 层 not 为偶数次取反：eq(x,10)=true → 结果仍 true，且必须是 Ok 而非 Err/panic
    assert!(
        evaluate_domain(&domain, &state).expect("深度 64 不得超界报错"),
        "64 层 not（偶数次取反）后应仍为 true"
    );
}

/// 深度 > 64 显式拒绝（`NestingTooDeep`），绝不静默求值。
#[test]
fn t2b_domain_eval_over_limit_explicit_reject() {
    let state = exec_state();
    let mut domain = eq_x10();
    for _ in 0..65 {
        domain =
            JsonValue::object_from_pairs(&[("type", JsonValue::string("not")), ("inner", domain)]);
    }
    assert!(
        matches!(
            evaluate_domain(&domain, &state),
            Err(evorule_tcb::TcbError::NestingTooDeep { limit: 64 })
        ),
        "深度 65 必须显式拒绝（fail-closed），不得静默通过"
    );
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 载体同一性 · 差分验证 harness（DEV-6 证据形态 B · T5/T6）
//!
//! 在生产载体（真实 `blake3` 哈希、真实 `RwLock` 并发、完整 append 路径）上，
//! 实证被验证属性同样成立，弥补 D11（T5）与 D12/D13（T6）的行为级偏差。
//!
//! 载体说明：tests/ 仅在 `cargo test` 下编译，不进入 Kani 翻译层。
//!
//! 界声明（R2）：T5 样本 ≤ 8、字符串长度 ≤ 64；T6 append 次数 ≤ 8（单线程，
//! 并发数据竞争验证由既有审计链测试覆盖，本 harness 不重复）。

#![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]

use evorule_reactor::{chain_step, content_hash, fact_hash};
use evorule_reactor::{Fact, FactId, FactsLog};
use evorule_tcb::JsonValue;

// ===== T5：哈希与链构造（对应 D11）=====

/// `content_hash` 确定性（同输入同输出）+ 区分性（不同输入不同输出）。
/// 区分性是「篡改必破坏链」的结构基础。
#[test]
fn t5a_content_hash_deterministic_and_discriminating() {
    let long = "x".repeat(64);
    let samples = ["", "a", "genesis", long.as_str()];
    for s in samples {
        let h1 = content_hash(&JsonValue::string(s)).expect("合法输入不得报错");
        let h2 = content_hash(&JsonValue::string(s)).expect("合法输入不得报错");
        assert_eq!(h1, h2, "content_hash 必须确定性：输入 {s:?}");
    }
    // 区分性：逐对不同输入产生不同哈希
    for i in 0..samples.len() {
        for j in (i + 1)..samples.len() {
            let hi = content_hash(&JsonValue::string(samples[i])).expect("ok");
            let hj = content_hash(&JsonValue::string(samples[j])).expect("ok");
            assert_ne!(hi, hj, "不同输入 {i}/{j} 必须产生不同哈希");
        }
    }
}

/// `chain_step` 单射性（前链不同或内容不同 → 链哈希不同）+ 幂等。
/// 实证「链构造单射」结构性质在 blake3 生产载体成立（替身用简化哈希验证过同一命题）。
#[test]
fn t5b_chain_step_injective_and_idempotent() {
    let c = content_hash(&JsonValue::string("payload")).expect("ok");
    // 前链单射：h1 ≠ h2 ⇒ chain_step(h1,c) ≠ chain_step(h2,c)
    let h1 = chain_step("hash-A", &c);
    let h2 = chain_step("hash-B", &c);
    assert_ne!(h1, h2, "前链不同必须产生不同链哈希");
    // 内容单射：c1 ≠ c2 ⇒ chain_step(h,c1) ≠ chain_step(h,c2)
    let c2 = content_hash(&JsonValue::string("tampered")).expect("ok");
    assert_ne!(h1, chain_step("hash-A", &c2), "内容不同必须产生不同链哈希");
    // 幂等（确定性）
    assert_eq!(h1, chain_step("hash-A", &c));
}

/// T5c：`fact_hash` 对 PayloadUpdate 确定性 + 篡改必变。
#[test]
fn t5c_fact_hash_deterministic_tamper_detected() {
    let mk = |v: i64| Fact::PayloadUpdate {
        id: FactId(1),
        path: "shared.probe".to_string(),
        value: JsonValue::Integer(v),
    };
    let a1 = fact_hash(&mk(1)).expect("合法 Fact 不得报错");
    let a2 = fact_hash(&mk(1)).expect("合法 Fact 不得报错");
    let b = fact_hash(&mk(2)).expect("合法 Fact 不得报错");
    assert_eq!(a1, a2, "fact_hash 必须确定性");
    assert_ne!(a1, b, "内容被篡改必须改变事实哈希");
}

// ===== T6：FactsLog append 完整路径（对应 D12/D13）=====

fn payload(path: &str, v: i64) -> Fact {
    Fact::PayloadUpdate {
        id: FactId(v as u64),
        path: path.to_string(),
        value: JsonValue::Integer(v),
    }
}

/// 生产载体（真实 `RwLock` + 完整 append：哈希链更新 + 内存状态）下
/// version 单调递增、快照与内存一致（D13 替身跳过的路径在生产成立）。
#[test]
fn t6a_append_version_monotonic_and_snapshot_consistent() {
    let log = FactsLog::new();
    let mut prev = log.version();
    for i in 1..=8 {
        let v = log
            .append(payload("shared.probe", i))
            .expect("append 不得失败");
        assert_eq!(v, prev + 1, "version 必须严格 +1 递增");
        prev = v;
        let (_, _, snap_v) = log.snapshot();
        assert_eq!(snap_v, v, "快照 version 必须与 append 返回值一致");
    }
    assert_eq!(log.version(), 8);
}

/// append-only 历史——同 path 多次 append 保留全部事实（无就地改写）。
#[test]
fn t6b_append_only_history_preserved() {
    let log = FactsLog::new();
    for i in 1..=4 {
        log.append(payload("shared.probe", i))
            .expect("append 不得失败");
    }
    let facts = log.facts_by_path_prefix("shared.probe");
    assert_eq!(
        facts.len(),
        4,
        "账本必须保留全部 4 条历史事实（append-only，非就地改写）"
    );
    // 历史顺序与写入一致（生产载体上迭代序确定）
    let values: Vec<i64> = facts
        .iter()
        .map(|(_, f)| match f {
            Fact::PayloadUpdate { value, .. } => match value {
                JsonValue::Integer(n) => *n,
                _ => panic!("应全部为 Integer"),
            },
            _ => panic!("应全部为 PayloadUpdate"),
        })
        .collect();
    assert_eq!(values, vec![1, 2, 3, 4], "历史序必须与写入序一致");
}

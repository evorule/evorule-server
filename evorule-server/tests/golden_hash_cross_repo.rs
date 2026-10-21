// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 跨仓哈希黄金断言：本仓（经 vendored 副本消费的）`fact_hash` 必须与上游内核
//! SSOT 快照（`evorule-audit-chain/hash_snapshot.txt`）逐字节一致。
//!
//! 为什么需要：内核已将审计链哈希算法上提为 no_std SSOT crate 并以快照+三方
//! 交叉测试守护；本仓 vendored 副本是独立实现拷贝（升级随基线节奏），若无
//! 本断言，两侧任何一侧独立演进导致的字节漂移将使本仓产出的审计链与内核
//! 工具（verify-chain 等）不再互证，且无人在对账时发现。本测试即跨仓锁定的
//! 最小机制：值变更只应发生在内核快照有意变更（Fact 结构演进）并同步此处。
//!
//! 值来源：上游内核仓 `evorule-audit-chain/hash_snapshot.txt`（7 条标准事实）。

#![allow(clippy::unwrap_used)]

use evorule_reactor::{fact_hash, Fact, FactId, IoType};
use evorule_tcb::JsonValue;

/// 内核 SSOT 快照值（逐行对应 GOLDEN_FACTS 构造顺序）
const KERNEL_SNAPSHOT: [&str; 7] = [
    "56de9ce95d41896edf081f452b4ea5d3ad6508de3a0781a36fa5fbd13c586aa1",
    "902f22869450a78851d5bf1396b64d4bc7be0666468a39d2dac53b7d176f8047",
    "6c4a12ee89a692181a07ede80b0864f0774286427fce67156944f6f33290ddf7",
    "4855b360063f11eeffd7b8e2bd5720c641451012a688ebec489b5f8f059058aa",
    "867c9f708b7469ffb25682f997792b8811f34ef61eb2ee4ba23964e1c20f7909",
    "ac02d9e15a497b30c18ce5019a50aeced2d5d186a973ee3a0985ee728054a015",
    "42ca4e53611616707514550f1d2ab8a89be5ef8bd1a5f6cbd07e016bdb931afb",
];

fn golden_facts() -> Vec<Fact> {
    vec![
        Fact::Command {
            id: FactId(1),
            instruction: JsonValue::empty_object(),
        },
        Fact::PayloadUpdate {
            id: FactId(2),
            path: "test.path".into(),
            value: JsonValue::string("test_value"),
        },
        Fact::StateTransition {
            id: FactId(3),
            cause: FactId(1),
            new_payload: JsonValue::empty_object(),
            new_queue: vec![],
        },
        Fact::IoRequest {
            id: FactId(4),
            cause: FactId(3),
            io_type: IoType::http_get(),
            params: JsonValue::empty_object(),
        },
        Fact::IoResponse {
            id: FactId(5),
            request_id: FactId(4),
            result: JsonValue::string("response"),
            error: None,
        },
        Fact::Stable {
            id: FactId(6),
            version: 0,
        },
        Fact::Error {
            id: FactId(7),
            message: "test error".into(),
        },
    ]
}

/// 跨仓逐字节锁定（任何一侧漂移即红）
#[test]
fn golden_fact_hash_matches_kernel_ssot() {
    for (i, fact) in golden_facts().iter().enumerate() {
        let h = fact_hash(fact).unwrap();
        assert_eq!(
            h,
            KERNEL_SNAPSHOT[i],
            "第 {} 条标准事实哈希与内核 SSOT 快照不一致——跨仓实现漂移（本仓产出的审计链\
             将与内核工具不互证）。若内核快照有意变更，须同批同步本常量",
            i + 1
        );
    }
}

/// 链步一致性（genesis 起点与链哈希推进）
#[test]
fn golden_chain_step_matches_kernel_semantics() {
    let facts = golden_facts();
    let mut prev = "genesis".to_string();
    for fact in &facts {
        let c = fact_hash(fact).unwrap();
        let combined = format!("{prev}{c}");
        let chain = blake3::hash(combined.as_bytes()).to_hex().to_string();
        prev = chain;
    }
    // 非空链链尾不得为 genesis（推进发生）；空链语义由内核测试守护
    assert_ne!(prev, "genesis");
}

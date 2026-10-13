// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 性能基准 — FactsLog append 吞吐（内存模式）
//!
//! 041 v2.0 A7：建立审计链 append 性能基线。
//! 每次 append 含 BLAKE3 哈希链计算（content_hash + chain_hash），
//! 是反应器每步执行的核心写入路径。
//!
//! # 运行
//! ```bash
//! cargo bench -p evorule-reactor --bench facts_log_append
//! ```
#![allow(clippy::unwrap_used, clippy::expect_used)]

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use evorule_reactor::{Fact, FactId, FactsLog};
use evorule_tcb::JsonValue;

fn bench_facts_log_append_1000(c: &mut Criterion) {
    let instruction = JsonValue::object_from_pairs(&[
        ("type", JsonValue::string("increment")),
        (
            "params",
            JsonValue::object_from_pairs(&[
                ("attr", JsonValue::string("x")),
                ("delta", JsonValue::Integer(1)),
            ]),
        ),
    ]);

    c.bench_function("facts_log/append_1000_memory", |b| {
        b.iter(|| {
            let log = FactsLog::new();
            for i in 0..1000 {
                let fact = Fact::Command {
                    id: FactId(i),
                    instruction: instruction.clone(),
                };
                let _ = log.append(black_box(fact));
            }
        });
    });
}

criterion_group!(benches, bench_facts_log_append_1000);
criterion_main!(benches);

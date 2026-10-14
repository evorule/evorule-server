// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 性能基准 — 审计链 audit_new + verify
//!
//! 041 v2.0 A8：建立治理层审计链性能基线。
//! - `audit_new`：从 FactsLog 拉取新增事实，计算哈希链条目
//! - `verify`：O(n) 遍历整条哈希链校验完整性
//!
//! # 运行
//! ```bash
//! cargo bench -p evorule-governance --bench audit_chain
//! ```
#![allow(clippy::unwrap_used, clippy::expect_used)]

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use evorule_governance::Auditor;
use evorule_reactor::{Fact, FactId, FactsLog};
use evorule_tcb::JsonValue;

/// 向 FactsLog 填充 n 条 Command 事实
fn fill_facts(log: &FactsLog, n: u64) {
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
    for i in 0..n {
        let _ = log.append(Fact::Command {
            id: FactId(i),
            instruction: instruction.clone(),
        });
    }
}

/// audit_new 100 条事实的开销（含哈希链条目构造）
fn bench_audit_new_100(c: &mut Criterion) {
    c.bench_function("auditor/audit_new_100_facts", |b| {
        b.iter(|| {
            let log = FactsLog::new();
            fill_facts(&log, 100);
            let mut auditor = Auditor::new(log);
            let n = auditor.audit_new();
            black_box(n);
        });
    });
}

/// verify 1000 条审计条目的开销（O(n) 哈希链校验）
fn bench_audit_verify_1000(c: &mut Criterion) {
    // 预构造 1000 条审计链（bench 外一次性准备）
    let log = FactsLog::new();
    fill_facts(&log, 1000);
    let mut auditor = Auditor::new(log);
    auditor.audit_new();

    c.bench_function("auditor/verify_1000_entries", |b| {
        b.iter(|| {
            let ok = auditor.verify();
            black_box(ok);
        });
    });
}

criterion_group!(benches, bench_audit_new_100, bench_audit_verify_1000);
criterion_main!(benches);

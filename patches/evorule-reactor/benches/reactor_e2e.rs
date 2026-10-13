// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 性能基准 — reactor 主循环端到端
//!
//! 041 v2.0 A7：建立 reactor 主循环性能基线。
//! 覆盖 spawn → 提交命令 → 接收 Stable 事件 → 优雅退出的完整路径，
//! 反映真实反应器调度 + tier0 执行 + FactsLog append + 事件广播的开销。
//!
//! # 运行
//! ```bash
//! cargo bench -p evorule-reactor --bench reactor_e2e
//! ```
#![allow(clippy::unwrap_used, clippy::expect_used)]

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use evorule_reactor::{Fact, FactId, Reactor};
use evorule_tcb::JsonValue;

/// 构造 increment 宪法规则
fn make_core_eval() -> Vec<JsonValue> {
    vec![JsonValue::object_from_pairs(&[
        ("type", JsonValue::string("increment")),
        (
            "params",
            JsonValue::object_from_pairs(&[
                ("attr", JsonValue::string("x")),
                ("delta", JsonValue::Integer(1)),
            ]),
        ),
    ])]
}

/// 构造 increment 指令
fn make_increment(delta: i64) -> JsonValue {
    JsonValue::object_from_pairs(&[
        ("type", JsonValue::string("increment")),
        (
            "params",
            JsonValue::object_from_pairs(&[
                ("attr", JsonValue::string("x")),
                ("delta", JsonValue::Integer(delta)),
            ]),
        ),
    ])
}

/// 单命令端到端：spawn 反应器 → 发 1 条 increment → 等 Stable → 退出
fn bench_reactor_single_command(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let core_eval = make_core_eval();

    c.bench_function("reactor/e2e_single_command", |b| {
        b.iter(|| {
            rt.block_on(async {
                let reactor = Reactor::builder(core_eval.clone()).max_rounds(1000).build();
                let (cmd_tx, mut event_rx, _event_tx, handle, _facts_log) = reactor.spawn();

                cmd_tx
                    .send(Fact::Command {
                        id: FactId(1),
                        instruction: make_increment(1),
                    })
                    .expect("send command");

                // 等待 Stable 事件（主循环处理完成的标志）
                while let Ok(fact) = event_rx.recv().await {
                    if matches!(fact, Fact::Stable { .. }) {
                        break;
                    }
                }

                drop(cmd_tx);
                let _ = handle.join().await;
            });
        });
    });
}

/// 100 命令端到端：连续提交 100 条命令，等最终 Stable
fn bench_reactor_100_commands(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let core_eval = make_core_eval();

    c.bench_function("reactor/e2e_100_commands", |b| {
        b.iter(|| {
            rt.block_on(async {
                let reactor = Reactor::builder(core_eval.clone())
                    .max_rounds(10000)
                    .build();
                let (cmd_tx, mut event_rx, _event_tx, handle, facts_log) = reactor.spawn();

                for i in 1..=100 {
                    cmd_tx
                        .send(Fact::Command {
                            id: FactId(i),
                            instruction: make_increment(1),
                        })
                        .expect("send command");
                }

                // 长运行模式：队列空后 emit Stable
                while let Ok(fact) = event_rx.recv().await {
                    if matches!(fact, Fact::Stable { .. }) {
                        break;
                    }
                }

                // 确认 100 条命令都已入账（防止优化器消除）
                let history_len = facts_log.history().len();
                black_box(history_len);

                drop(cmd_tx);
                let _ = handle.join().await;
            });
        });
    });
}

criterion_group!(
    benches,
    bench_reactor_single_command,
    bench_reactor_100_commands
);
criterion_main!(benches);

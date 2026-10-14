// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 差分测试:审计链版本一致性(P0-9)+ 时间机器 vs FactsLog 重建一致性(P0-10)
//!
//! # 位置说明
//!
//! 本文件位于 `verification/` 目录(形式化验证专属目录),与 `src/` 核心实现解耦:
//! - 不受 `build.rs` 编译时门禁约束(仅扫 `src/` 目录)
//! - 通过 `Cargo.toml` 的 `[[test]]` 目标指向,作为集成测试编译
//!
//! # 验证目标
//!
//! ## P0-9: `diff_version_consistency`
//!
//! `facts_log.version() == reactor.snapshot().version`
//!
//! 验证审计链(FactsLog)的版本号与反应器内部状态版本号一致。
//! 如果两者分叉,会导致时间旅行调试器看到的版本与实际执行版本不匹配。
//!
//! ## P0-10: `diff_rewind_vs_factslog`
//!
//! `rewind(facts, N).snapshot == FactsLog 重放到版本 N 的快照`
//!
//! 验证时间机器(纯函数重建)与 FactsLog(命令式追加)产生相同的历史快照。
//! 两条独立路径达到同一版本,互为交叉验证。
//!
//! # 语义等价规约(差分测试的形式化契约)
//!
//! - **P0-9**: 对任意指令序列 `Cmds` 与初始 payload `S`:
//!   `facts_log.version() == reactor.snapshot().version`
//!   (审计链视角与反应器内部视角的版本号必须一致,不得分叉)。
//! - **P0-10**: 对任意事实序列 `Facts` 与目标版本 `N`:
//!   `rewind(Facts, N).snapshot == FactsLog 重放前 N 个事实后的 snapshot`
//!   (两条独立重建路径产生的版本号与 payload 均须结构全等)。
//!
//! # 跑法
//!
//! ```bash
//! cargo test --package evorule-governance --test differential_test
//! PROPTEST_CASES=1000 cargo test --package evorule-governance --test differential_test
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use evorule_governance::time_machine::rewind;
use evorule_reactor::{Fact, FactId, FactsLog, Reactor};
use evorule_tcb::JsonValue;
use proptest::prelude::*;
use proptest::test_runner::FileFailurePersistence;

// =============================================================================
// 辅助:JsonValue ↔ serde_json::Value 转换(用于比较)
// =============================================================================

/// 将 evorule_tcb::JsonValue 转换为 serde_json::Value(用于与 RewindSnapshot 比较)
fn tcb_to_serde(v: &JsonValue) -> serde_json::Value {
    match v {
        JsonValue::Null => serde_json::Value::Null,
        JsonValue::Bool(b) => serde_json::Value::Bool(*b),
        JsonValue::Integer(i) => serde_json::Value::Number((*i).into()),
        JsonValue::String(s) => serde_json::Value::String(s.to_string()),
        JsonValue::Array(arr) => serde_json::Value::Array(arr.iter().map(tcb_to_serde).collect()),
        JsonValue::Object(map) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in map.iter() {
                obj.insert(k.clone(), tcb_to_serde(v));
            }
            serde_json::Value::Object(obj)
        }
    }
}

// =============================================================================
// 辅助:构造 Fact
// =============================================================================

fn payload_update_fact(id: u64, path: &str, value: i64) -> Fact {
    Fact::PayloadUpdate {
        id: FactId(id),
        path: path.to_string(),
        value: JsonValue::Integer(value),
    }
}

fn state_transition_fact(id: u64, cause: u64, payload_x: i64, payload_y: i64) -> Fact {
    Fact::StateTransition {
        id: FactId(id),
        cause: FactId(cause),
        new_payload: JsonValue::object_from_pairs(&[
            ("x", JsonValue::Integer(payload_x)),
            ("y", JsonValue::Integer(payload_y)),
        ]),
        new_queue: vec![],
    }
}

// =============================================================================
// proptest 配置
// =============================================================================

fn proptest_config() -> ProptestConfig {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(256);
    ProptestConfig {
        cases,
        failure_persistence: Some(Box::new(FileFailurePersistence::Off)),
        ..ProptestConfig::default()
    }
}

// =============================================================================
// P0-9: facts_log.version() == reactor.snapshot().version
// =============================================================================

proptest! {
    #![proptest_config(proptest_config())]

    /// P0-9: 单条 Command 后,审计链版本与反应器内部版本一致
    ///
    /// 差分对:
    /// - A: FactsLog::version()(审计链视角)
    /// - B: ReactorHandle::snapshot().version(反应器内部视角)
    ///
    /// 发送一条 Command → StateTransition 后,两者版本都应为 1。
    #[test]
    fn diff_version_consistency_single_command(
        initial_x in -1_000_000i64..1_000_000,
        new_value in -1_000_000i64..1_000_000,
    ) {
        let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
        rt.block_on(async move {
            // 空核心规则集 —— execute_transition 返回 State(payload 不变)
            let core_eval: Vec<JsonValue> = vec![];
            let payload = JsonValue::object_from_pairs(&[
                ("x", JsonValue::Integer(initial_x)),
                ("y", JsonValue::Integer(0)),
            ]);
            let facts_log = FactsLog::with_initial_payload(payload);
            let reactor = Reactor::builder(core_eval)
                .max_rounds(100)
                .facts_log(facts_log)
                .build();
            let (tx, mut rx, _event_tx, handle, log) = reactor.spawn();

            // 发送 set 指令(触发 StateTransition)
            let instruction = JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("set")),
                ("params", JsonValue::object_from_pairs(&[
                    ("attr", JsonValue::string("x")),
                    ("operation", JsonValue::string("set")),
                    ("value", JsonValue::Integer(new_value)),
                ])),
            ]);
            // FactSender 是 UnboundedSender,send 是同步方法
            tx.send(Fact::Command {
                id: FactId(1),
                instruction,
            }).expect("send command failed");

            // 等待 Stable(确保 StateTransition 已被 FactsLog 记录)
            let mut got_transition = false;
            while let Ok(fact) = rx.recv().await {
                match fact {
                    Fact::StateTransition { .. } => {
                        got_transition = true;
                    }
                    Fact::Stable { .. } => break,
                    Fact::Error { .. } => break,
                    _ => {}
                }
            }

            handle.abort();

            // 核心断言:如果发生了 StateTransition,版本号必须一致
            if got_transition {
                let reactor_version = handle.snapshot()
                    .expect("reactor snapshot should exist")
                    .version;
                let log_version = log.version();
                prop_assert_eq!(
                    reactor_version, log_version,
                    "version mismatch: reactor={} vs facts_log={} (after 1 StateTransition)",
                    reactor_version, log_version,
                );
            }

            Ok(())
        })?;
    }

    /// P0-9: 多条 Command 后,审计链版本与反应器内部版本一致
    #[test]
    fn diff_version_consistency_multi_command(
        n_commands in 1usize..10,
    ) {
        let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
        rt.block_on(async move {
            let core_eval: Vec<JsonValue> = vec![];
            let payload = JsonValue::object_from_pairs(&[
                ("x", JsonValue::Integer(0)),
            ]);
            let facts_log = FactsLog::with_initial_payload(payload);
            let reactor = Reactor::builder(core_eval)
                .max_rounds(1000)
                .facts_log(facts_log)
                .build();
            let (tx, mut rx, _event_tx, handle, log) = reactor.spawn();

            // 发送 n_commands 条 noop 指令
            for i in 0..n_commands {
                let instruction = JsonValue::object_from_pairs(&[
                    ("type", JsonValue::string("noop")),
                ]);
                tx.send(Fact::Command {
                    id: FactId(i as u64 + 1),
                    instruction,
                }).expect("send command failed");

                // 等待这条命令的 Stable
                while let Ok(fact) = rx.recv().await {
                    if matches!(fact, Fact::Stable { .. } | Fact::Error { .. }) {
                        break;
                    }
                }
            }

            handle.abort();

            // 比较版本号
            let reactor_version = handle.snapshot()
                .expect("reactor snapshot should exist")
                .version;
            let log_version = log.version();
            prop_assert_eq!(
                reactor_version, log_version,
                "version mismatch after {} commands: reactor={} vs facts_log={}",
                n_commands, reactor_version, log_version,
            );

            Ok(())
        })?;
    }
}

// =============================================================================
// P0-10: rewind(facts, N) == FactsLog 重放到版本 N
// =============================================================================

proptest! {
    #![proptest_config(proptest_config())]

    /// P0-10: PayloadUpdate 序列 —— rewind 与 FactsLog 重放产生相同快照
    ///
    /// 差分对:
    /// - A: rewind(&all_facts, target_version) → RewindSnapshot
    /// - B: FactsLog 重放前 target_version 个事实 → snapshot()
    ///
    /// 两条独立路径重建同一版本的状态,互为交叉验证。
    #[test]
    fn diff_rewind_vs_factslog_payload_update(
        // 1..8 个 PayloadUpdate 事实,每个更新 "x" 字段
        values in proptest::collection::vec(-1000i64..1000, 1..8),
        // 目标索引(选择中间某个版本做差分)
        target_idx in 0usize..7,
    ) {
        // 生成 PayloadUpdate 事实序列
        let facts: Vec<Fact> = values.iter().enumerate().map(|(i, &v)| {
            payload_update_fact(i as u64 + 1, "x", v)
        }).collect();

        let target_idx = target_idx.min(facts.len() - 1);
        let target_facts = &facts[..=target_idx];

        // 路径 A: rewind
        // rewind 需要 target_version,PayloadUpdate 每个递增 version,
        // 所以 target_idx 个事实后 version = target_idx + 1
        let target_version = (target_idx + 1) as u64;
        let rewind_snapshot = rewind(&facts, target_version);
        prop_assert!(
            rewind_snapshot.is_some(),
            "rewind should return Some for valid target_version={} (facts={})",
            target_version, facts.len()
        );
        let rs = rewind_snapshot.unwrap();

        // 路径 B: FactsLog 重放到 target_version
        let log_b = FactsLog::with_initial_payload(JsonValue::empty_object());
        for fact in target_facts {
            log_b.append(fact.clone()).expect("append failed");
        }
        let (b_payload, _b_queue, b_version) = log_b.snapshot();

        // 比较版本号
        prop_assert_eq!(
            rs.version, b_version,
            "version mismatch: rewind={} vs facts_log={}",
            rs.version, b_version,
        );

        // 比较 payload
        let b_payload_serde = tcb_to_serde(&b_payload);
        prop_assert_eq!(
            &rs.payload, &b_payload_serde,
            "payload mismatch at version {}",
            target_version,
        );

        // 验证 x 字段值正确
        let expected_x = values[target_idx];
        prop_assert_eq!(
            rs.payload.get("x").and_then(|v| v.as_i64()),
            Some(expected_x),
            "rewind x should be last update value"
        );
    }

    /// P0-10: StateTransition 序列 —— rewind 与 FactsLog 重放产生相同快照
    ///
    /// StateTransition 替换整个 payload(而非合并),测试不同的重建路径。
    #[test]
    fn diff_rewind_vs_factslog_state_transition(
        // 1..6 个 (x, y) 对,每个生成一个 StateTransition
        pairs in proptest::collection::vec(
            (-1000i64..1000, -1000i64..1000),
            1..6,
        ),
        target_idx in 0usize..5,
    ) {
        // 生成 StateTransition 事实序列
        let facts: Vec<Fact> = pairs.iter().enumerate().map(|(i, &(x, y))| {
            state_transition_fact(i as u64 + 1, i as u64, x, y)
        }).collect();

        let target_idx = target_idx.min(facts.len() - 1);
        let target_facts = &facts[..=target_idx];

        // 路径 A: rewind
        let target_version = (target_idx + 1) as u64;
        let rewind_snapshot = rewind(&facts, target_version);
        prop_assert!(
            rewind_snapshot.is_some(),
            "rewind should return Some for StateTransition sequence"
        );
        let rs = rewind_snapshot.unwrap();

        // 路径 B: FactsLog 重放
        let log_b = FactsLog::with_initial_payload(JsonValue::empty_object());
        for fact in target_facts {
            log_b.append(fact.clone()).expect("append failed");
        }
        let (b_payload, _b_queue, b_version) = log_b.snapshot();

        // 比较版本号
        prop_assert_eq!(
            rs.version, b_version,
            "version mismatch (StateTransition): rewind={} vs facts_log={}",
            rs.version, b_version,
        );

        // 比较 payload
        let b_payload_serde = tcb_to_serde(&b_payload);
        prop_assert_eq!(
            &rs.payload, &b_payload_serde,
            "payload mismatch (StateTransition) at version {}",
            target_version,
        );

        // 验证 x/y 字段值正确(应为最后一次 StateTransition 的值)
        let (expected_x, expected_y) = pairs[target_idx];
        prop_assert_eq!(
            rs.payload.get("x").and_then(|v| v.as_i64()),
            Some(expected_x),
            "rewind x should match last StateTransition"
        );
        prop_assert_eq!(
            rs.payload.get("y").and_then(|v| v.as_i64()),
            Some(expected_y),
            "rewind y should match last StateTransition"
        );
    }

    /// P0-10: 混合事实序列(PayloadUpdate + StateTransition)
    ///
    /// 验证 rewind 在混合事实类型下仍与 FactsLog 一致。
    #[test]
    fn diff_rewind_vs_factslog_mixed(
        // 先一个 StateTransition 设定初始状态,然后多个 PayloadUpdate 修改
        initial_x in -500i64..500,
        initial_y in -500i64..500,
        // 每个更新选择 "x" 或 "y" 字段
        updates in proptest::collection::vec(
            (-500i64..500, proptest::sample::select(vec!["x".to_string(), "y".to_string()])),
            1..5,
        ),
        target_idx in 0usize..5,
    ) {
        let mut facts: Vec<Fact> = vec![
            state_transition_fact(1, 0, initial_x, initial_y),
        ];
        for (i, &(v, ref path)) in updates.iter().enumerate() {
            facts.push(payload_update_fact(i as u64 + 2, path, v));
        }

        let total_facts = facts.len();
        let target_idx = target_idx.min(total_facts - 1);
        let target_facts = &facts[..=target_idx];

        // target_version: 1 (StateTransition) + target_idx (PayloadUpdate 数)
        let target_version = (target_idx + 1) as u64;

        // 路径 A: rewind
        let rewind_snapshot = rewind(&facts, target_version);
        prop_assert!(rewind_snapshot.is_some(), "rewind should return Some for mixed sequence");
        let rs = rewind_snapshot.unwrap();

        // 路径 B: FactsLog 重放
        let log_b = FactsLog::with_initial_payload(JsonValue::empty_object());
        for fact in target_facts {
            log_b.append(fact.clone()).expect("append failed");
        }
        let (b_payload, _b_queue, b_version) = log_b.snapshot();

        // 比较版本号
        prop_assert_eq!(
            rs.version, b_version,
            "version mismatch (mixed): rewind={} vs facts_log={}",
            rs.version, b_version,
        );

        // 比较 payload
        let b_payload_serde = tcb_to_serde(&b_payload);
        prop_assert_eq!(
            &rs.payload, &b_payload_serde,
            "payload mismatch (mixed) at version {}",
            target_version,
        );
    }
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.

//! vendored 副本 Fact 契约黄金向量：自证闸门 + 重生成器
//!
//! （背景）本目录为 vendored 补丁副本，与主仓 SSOT（evorule-audit-chain）无
//! 依赖关系，单方漂移不在任何一侧仓内测试暴露。跨仓对账物 = 冻结黄金向量
//! `tests/fixtures/ssot_fact_contract_golden.json`（本目录生成、双仓共享）：
//! - 本文件 assert 测试：副本 live 输出 == 向量（副本侧棘轮，server 仓 CI 强阻塞）；
//! - 对向闸门（evorule-integration-tests tests/ssot_fact_contract_gate.rs）经
//!   include_str! 读同一向量：SSOT live 输出 == 向量（SSOT 侧棘轮）。
//! 契约有意演进时：在本目录重生成向量，双仓同批提交留痕。
//!
//! （覆盖面）9 个 Fact 变体 + u64::MAX-1 哨兵 id + u64 值通道共 11 样本。
//! 断言面仅哈希契约（fact_to_stable_json / fact_hash，无 feature 门控）——
//! WAL 展示面（fact_to_json/fact_from_json，persistence 门控）不在对账物内：
//! 其 serde_to_tcb 回程对超 i64 范围 u64 有意转字符串（0.8.4 基线既有行为，
//! 主仓 0.9.0 同），往返非无损，已按存量问题单独立项，不入本闸断言面。
//!
//! （独立自测 tcb 源）本目录空 workspace 表下 tcb 按纯 version req 解析注册表
//! 0.8.4（缺 u64 全域通道）；[patch.crates-io] 已重定向至仓内 vendored 副本。

use evorule_reactor::{fact_hash, fact_to_stable_json, Fact, FactId, IoType};
use evorule_tcb::JsonValue;

const FIXTURE_REL: &str = "tests/fixtures/ssot_fact_contract_golden.json";

/// 全量对账样本（label + 副本侧事实）——与 SSOT 侧对向闸门逐字段同构
fn all_samples() -> Vec<(&'static str, Fact)> {
    let noop = || JsonValue::object_from_pairs(&[("type", JsonValue::string("noop"))]);
    let empty = || JsonValue::object_from_pairs(&[]);
    vec![
        (
            "Command",
            Fact::Command {
                id: FactId(1),
                instruction: noop(),
            },
        ),
        (
            "PayloadUpdate(sentinel)",
            Fact::PayloadUpdate {
                id: FactId(u64::MAX - 1),
                path: "__ruleset__.version".into(),
                value: JsonValue::String("c7b6fab66f0ea645".into()),
            },
        ),
        (
            "PayloadUpdate(u64-value)",
            Fact::PayloadUpdate {
                id: FactId(11),
                path: "__ruleset__.anchor".into(),
                value: JsonValue::u64(u64::MAX - 3),
            },
        ),
        (
            "StateTransition(sentinel-cause)",
            Fact::StateTransition {
                id: FactId(u64::MAX - 2),
                cause: FactId(1),
                new_payload: empty(),
                new_queue: vec![],
            },
        ),
        (
            "IoRequest",
            Fact::IoRequest {
                id: FactId(4),
                cause: FactId(1),
                io_type: IoType::call_external(),
                params: empty(),
            },
        ),
        (
            "IoResponse(ok)",
            Fact::IoResponse {
                id: FactId(5),
                request_id: FactId(4),
                result: empty(),
                error: None,
            },
        ),
        (
            "IoResponse(error)",
            Fact::IoResponse {
                id: FactId(6),
                request_id: FactId(4),
                result: empty(),
                error: Some("io failed".into()),
            },
        ),
        (
            "Stable",
            Fact::Stable {
                id: FactId(7),
                version: 3,
            },
        ),
        (
            "Error",
            Fact::Error {
                id: FactId(8),
                message: "timeout".into(),
            },
        ),
        (
            "TransitionTrace",
            Fact::TransitionTrace {
                id: FactId(9),
                cause: FactId(u64::MAX - 2),
                rule_hits: vec![],
            },
        ),
        (
            "Violation",
            Fact::Violation {
                id: FactId(10),
                cause: FactId(1),
                rule_index: 2,
                reason: "enforced".into(),
                instruction: empty(),
            },
        ),
    ]
}

fn stable_compact(fact: &Fact) -> String {
    let v = fact_to_stable_json(fact).expect("稳定序列化不得失败");
    serde_json::to_string(&v).expect("紧凑序列化不得失败")
}

fn fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_REL)
}

/// 副本侧棘轮：live 输出 == 冻结向量（稳定序列化 + content_hash）
#[test]
fn fact_contract_matches_frozen_golden_fixture() {
    let raw = std::fs::read_to_string(fixture_path()).expect("黄金向量文件缺失（先运行重生成器）");
    let fx: serde_json::Value = serde_json::from_str(&raw).expect("黄金向量非法 JSON");
    let arr = fx["samples"].as_array().expect("黄金向量缺 samples 数组");
    let samples = all_samples();
    assert_eq!(
        samples.len(),
        11,
        "构造面样本数变化须同步黄金向量与对向闸门（9 变体 + 哨兵 id + u64 值通道 = 11）"
    );

    for (label, fact) in &samples {
        let entry = arr
            .iter()
            .find(|s| s["label"].as_str() == Some(*label))
            .unwrap_or_else(|| panic!("黄金向量缺样本 {label}（向量须与构造面同步）"));

        assert_eq!(
            stable_compact(fact),
            entry["stable_json"]
                .as_str()
                .unwrap_or_else(|| panic!("样本 {label}: 缺 stable_json")),
            "样本 {label}: 副本稳定序列化漂移（须重生成向量并双仓留痕）"
        );
        assert_eq!(
            fact_hash(fact).expect("fact_hash 不得失败"),
            entry["fact_hash"]
                .as_str()
                .unwrap_or_else(|| panic!("样本 {label}: 缺 fact_hash")),
            "样本 {label}: 副本 content_hash 漂移"
        );
    }

    // 反向同步：向量不得含构造面之外的样本
    assert_eq!(
        arr.len(),
        samples.len(),
        "黄金向量样本数与构造面不同步（对向闸门可能已先行演进）"
    );
}

/// 黄金向量重生成器（契约有意演进时显式执行；输出覆写 fixtures 文件，随批提交双仓留痕）
#[test]
#[ignore = "重生成器：cargo test --test ssot_fact_contract_gate --features persistence regenerate_golden_fixture -- --ignored --nocapture"]
fn regenerate_golden_fixture() {
    let samples: Vec<serde_json::Value> = all_samples()
        .into_iter()
        .map(|(label, fact)| {
            serde_json::json!({
                "label": label,
                "stable_json": stable_compact(&fact),
                "fact_hash": fact_hash(&fact).expect("fact_hash 不得失败"),
            })
        })
        .collect();
    let doc = serde_json::json!({
        "contract": "Fact stable serialization golden: fact_to_stable_json compact string + fact_hash hex, per sample",
        "regenerate": "in patches/evorule-reactor: cargo test --test ssot_fact_contract_gate --features persistence regenerate_golden_fixture -- --ignored --nocapture; mirror gate: evorule-integration-tests/tests/ssot_fact_contract_gate.rs",
        "samples": samples,
    });
    let pretty = serde_json::to_string_pretty(&doc).expect("向量序列化不得失败");
    let path = fixture_path();
    std::fs::create_dir_all(path.parent().expect("向量路径必有父目录")).expect("建目录不得失败");
    std::fs::write(&path, pretty + "\n").expect("写向量不得失败");
    println!("golden fixture written: {}", path.display());
}

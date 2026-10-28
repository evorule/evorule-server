// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.

//! 审计导出黄金向量：自证闸门 + 重生成器（EVID 移动端/第三方离线验证器基座）
//!
//! （背景）`Auditor::export()` 是审计链对外披露的唯一形态。EVID 契约下每条目
//! 内嵌 `fact_json_stable`（content_hash 的原字节输入），第三方（含移动端验证器）
//! 仅凭导出物即可逐条重算 content_hash + 链折叠，无需 Fact 类型知识、无需网络。
//! 导出格式一旦漂移，离线验证器全线失配——故冻结整份导出产物为黄金向量。
//!
//! （对账物）冻结向量 `tests/fixtures/audit_export_golden.json`：本目录生成、
//! 双仓共享：
//! - 本文件 assert 测试：live export == 向量（server 侧棘轮，server 仓 CI 强阻塞）；
//! - 对向闸门（evorule-integration-tests tests/audit_export_contract_gate.rs）经
//!   include_str! 读同一向量：SSOT 侧（evorule-audit-chain）活体复算 == 向量
//!   （SSOT 侧棘轮，bundle 仓 cross-repo-gates 跑）。
//!
//! 导出格式有意演进时：在本目录重生成向量，双仓同批提交留痕。
//!
//! （样本面）与 Fact 契约闸（patches/evorule-reactor/tests/ssot_fact_contract_gate.rs）
//! 同构 11 样本：9 个 Fact 变体 + u64::MAX-1 哨兵 id + u64 值通道，依序入链——
//! 导出物因此同时覆盖全部变体的 stable 序列化面与完整链折叠面。

use evorule_governance_host::Auditor;
use evorule_reactor::{Fact, FactId, FactsLog, IoType};
use evorule_tcb::JsonValue;

const FIXTURE_REL: &str = "tests/fixtures/audit_export_golden.json";

/// 全量对账样本（label + 事实）——与 Fact 契约闸构造面逐字段同构
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

/// 固定样本序列构建审计器（内存 FactsLog，顺序与向量 entries 一一对应）
fn build_golden_auditor() -> (Auditor, Vec<(&'static str, Fact)>) {
    let samples = all_samples();
    let log = FactsLog::new();
    for (_, fact) in &samples {
        log.append(fact.clone()).expect("样本事实入链不得失败");
    }
    let mut auditor = Auditor::new(log);
    auditor.audit_new();
    assert_eq!(
        auditor.entries().len(),
        samples.len(),
        "审计条目数与样本数不一致（样本构造面或去重游标漂移）"
    );
    (auditor, samples)
}

fn fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_REL)
}

/// server 侧棘轮：live 导出 == 冻结向量（整份导出产物全等）
#[test]
fn audit_export_matches_frozen_golden_fixture() {
    let (auditor, samples) = build_golden_auditor();
    let export_str = auditor.export().expect("导出序列化不得失败");
    let live: serde_json::Value = serde_json::from_str(&export_str).expect("导出须为合法 JSON");

    let raw = std::fs::read_to_string(fixture_path()).expect("黄金向量文件缺失（先运行重生成器）");
    let fx: serde_json::Value = serde_json::from_str(&raw).expect("黄金向量非法 JSON");
    let frozen = &fx["export"];

    // 结构性断言（失败时可读定位，先于全等比对）
    assert_eq!(live["version"], "1.0", "导出版本漂移");
    assert_eq!(
        live["entry_count"].as_u64(),
        Some(samples.len() as u64),
        "条目数与样本构造面不同步"
    );
    let entries = live["entries"].as_array().expect("导出缺 entries 数组");
    for entry in entries {
        assert!(
            entry["fact_json_stable"].is_string(),
            "条目 fact_id={} 缺 fact_json_stable（EVID 契约断裂）",
            entry["fact_id"]
        );
    }

    // 整份导出产物全等（含 last_hash/每条目全字段/字段值字节级一致）
    assert_eq!(
        &live, frozen,
        "导出产物与冻结黄金向量漂移（导出格式或哈希面演进——须重生成向量并双仓同批留痕）"
    );

    // 移动端判定预演：零 Fact 类型知识，仅凭导出物完成逐条重算 + 链折叠。
    // 折叠语义与 verify() 一致：条目 prev_hash 是**先验**链接（须等于当前链头），
    // chain_step(prev, content) 的结果即该条目链哈希（= 下一条目的先验 prev）。
    let mut prev = String::from("genesis");
    for entry in entries {
        assert_eq!(
            entry["prev_hash"].as_str().expect("缺 prev_hash"),
            prev,
            "fact_id={} 链接断裂",
            entry["fact_id"]
        );
        prev = evorule_governance::hash::chain_step(
            prev.as_str(),
            entry["content_hash"].as_str().expect("缺 content_hash"),
        );
    }
    assert_eq!(prev, frozen["last_hash"], "链折叠终值与 last_hash 不一致");
}

/// 黄金向量重生成器（导出格式有意演进时显式执行；输出覆写 fixtures 文件，随批提交双仓留痕）
#[test]
#[ignore = "重生成器：cargo test --manifest-path patches/evorule-governance/Cargo.toml --test audit_export_golden regenerate_export_golden_fixture -- --ignored --nocapture"]
fn regenerate_export_golden_fixture() {
    let (auditor, samples) = build_golden_auditor();
    let export_str = auditor.export().expect("导出序列化不得失败");
    let export: serde_json::Value = serde_json::from_str(&export_str).expect("导出须为合法 JSON");

    let labels: Vec<serde_json::Value> = samples
        .iter()
        .map(|(label, _)| serde_json::Value::String((*label).into()))
        .collect();
    let doc = serde_json::json!({
        "contract": "Audit export golden: auditor.export() payload frozen verbatim; per-entry fact_json_stable is the verbatim byte input of content_hash (blake3), chain folded from genesis equals last_hash",
        "regenerate": "in patches/evorule-governance: cargo test --test audit_export_golden regenerate_export_golden_fixture -- --ignored --nocapture; mirror gate: evorule-integration-tests/tests/audit_export_contract_gate.rs (SSOT live recompute vs this fixture)",
        "sample_labels": labels,
        "export": export,
    });
    let pretty = serde_json::to_string_pretty(&doc).expect("向量序列化不得失败");
    let path = fixture_path();
    std::fs::create_dir_all(path.parent().expect("向量路径必有父目录")).expect("建目录不得失败");
    std::fs::write(&path, pretty + "\n").expect("写向量不得失败");
    println!("golden fixture written: {}", path.display());
}

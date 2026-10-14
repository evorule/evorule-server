// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 端到端审计链集成测试（两套 WAL 合并验证）
//!
//! # 测试目标
//! 验证两套 WAL 合并方案的端到端完整性：
//! 1. **tier1 写 WAL**：`FactsLog::append` 自动计算哈希链并写入 WAL（含 `content_hash`/`prev_hash`/`chain_hash`）
//! 2. **tier2 加载验证**：`Auditor::load_from_tier1_wal` 读取 tier1 WAL 并逐条验证哈希链
//! 3. **篡改检测**：对 Fact 内容、`prev_hash`、`chain_hash` 的任何篡改都能被检测
//! 4. **跨层一致性**：tier1 `FactsLog::last_hash()` 与 tier2 `Auditor::last_hash` 字节级一致
//! 5. **CLI 兼容性**：WAL 文件可被 `evorule_reactor::read_wal_with_hash` 读取（CLI `verify-chain` 使用同一接口）
//!
//! # 单一真相源验证
//! 哈希算法的唯一实现在 `evorule_reactor::hash`，tier2 通过 re-export 调用。
//! 本测试通过对比 tier1 和 tier2 的 `last_hash` 验证单一真相源原则。

#![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use evorule_governance::{auditor::Auditor, hash};
use evorule_reactor::{read_wal_with_hash, Fact, FactId, FactsLog, IoType, WalRecord};
use evorule_tcb::JsonValue;
use tempfile::TempDir;

/// 构造一个简单的 Command Fact
fn make_command(id: u64, instruction_type: &str) -> Fact {
    let mut params = BTreeMap::new();
    params.insert("attr".to_string(), JsonValue::string("x"));
    params.insert("delta".to_string(), JsonValue::Integer(1));
    let mut instr = BTreeMap::new();
    instr.insert("type".to_string(), JsonValue::string(instruction_type));
    instr.insert("params".to_string(), JsonValue::Object(params));
    Fact::Command {
        id: FactId(id),
        instruction: JsonValue::Object(instr),
    }
}

/// 构造一个 StateTransition Fact
fn make_state_transition(id: u64, cause: u64, payload_val: i64) -> Fact {
    let mut payload = BTreeMap::new();
    payload.insert("x".to_string(), JsonValue::Integer(payload_val));
    Fact::StateTransition {
        id: FactId(id),
        cause: FactId(cause),
        new_payload: JsonValue::Object(payload),
        new_queue: vec![],
    }
}

/// 构造一个 Stable Fact
fn make_stable(id: u64, version: u64) -> Fact {
    Fact::Stable {
        id: FactId(id),
        version,
    }
}

/// 构造一个 IoRequest Fact
fn make_io_request(id: u64, cause: u64) -> Fact {
    let mut params = BTreeMap::new();
    params.insert("url".to_string(), JsonValue::string("http://example.com"));
    Fact::IoRequest {
        id: FactId(id),
        cause: FactId(cause),
        io_type: IoType::http_get(),
        params: JsonValue::Object(params),
    }
}

/// 构造一个 IoResponse Fact
fn make_io_response(id: u64, request_id: u64) -> Fact {
    Fact::IoResponse {
        id: FactId(id),
        request_id: FactId(request_id),
        result: JsonValue::string("ok"),
        error: None,
    }
}

/// 构造一个完整的事实序列（覆盖 7 种 Fact 变体中的 5 种常用类型）
fn build_fact_sequence() -> Vec<Fact> {
    vec![
        make_command(1, "increment"),
        make_state_transition(2, 1, 1),
        make_command(3, "increment"),
        make_state_transition(4, 3, 2),
        make_io_request(5, 4),
        make_io_response(6, 5),
        make_stable(7, 2), // id=7，于版本 2 稳定
    ]
}

/// 读取 WAL 文件所有行（原始文本），用于篡改测试
fn read_wal_lines(path: &PathBuf) -> Vec<String> {
    let content = fs::read_to_string(path).expect("WAL file should be readable");
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect()
}

/// 将 WAL 行写回文件
fn write_wal_lines(path: &PathBuf, lines: &[String]) {
    let content = lines.join("\n") + "\n";
    fs::write(path, content).expect("WAL file should be writable");
}

// ============================================================================
// 测试 1：tier1 FactsLog 写入的 WAL 包含哈希字段
// ============================================================================
#[test]
fn test_tier1_wal_contains_hash_fields() {
    let tmp = TempDir::new().expect("failed to create temp dir");
    let wal_path = tmp.path().join("hash_chain_test.wal");

    // 创建带 WAL 的 FactsLog
    let facts_log = FactsLog::with_wal(&wal_path).expect("failed to create FactsLog with WAL");
    let facts = build_fact_sequence();

    // 追加所有事实
    for fact in &facts {
        facts_log
            .append(fact.clone())
            .expect("append should succeed");
    }

    // 读取 WAL 文件并验证包含哈希字段
    let records = read_wal_with_hash(&wal_path).expect("WAL should be readable");
    assert_eq!(
        records.len(),
        facts.len(),
        "WAL record count should match fact count"
    );

    // 每条记录都应包含完整的哈希字段
    for (i, record) in records.iter().enumerate() {
        assert!(
            record.content_hash.is_some(),
            "record[{}] should have content_hash",
            i
        );
        assert!(
            record.prev_hash.is_some(),
            "record[{}] should have prev_hash",
            i
        );
        assert!(
            record.chain_hash.is_some(),
            "record[{}] should have chain_hash",
            i
        );
    }

    // 首条记录的 prev_hash 应为 "genesis"
    assert_eq!(
        records[0].prev_hash.as_deref(),
        Some("genesis"),
        "first record's prev_hash should be 'genesis'"
    );

    println!(
        "[OK] tier1 WAL contains hash fields for all {} records",
        records.len()
    );
}

// ============================================================================
// 测试 2：tier2 Auditor 从 tier1 WAL 加载并验证哈希链成功
// ============================================================================
#[test]
fn test_tier2_loads_and_verifies_tier1_wal() {
    let tmp = TempDir::new().expect("failed to create temp dir");
    let wal_path = tmp.path().join("cross_layer_test.wal");

    // tier1: 写 WAL
    let facts_log = FactsLog::with_wal(&wal_path).expect("failed to create FactsLog with WAL");
    let facts = build_fact_sequence();
    for fact in &facts {
        facts_log
            .append(fact.clone())
            .expect("append should succeed");
    }
    let tier1_last_hash = facts_log.last_hash();

    // tier2: 从 tier1 WAL 加载并验证
    let mut auditor = Auditor::new(FactsLog::new());
    auditor
        .load_from_tier1_wal(&wal_path)
        .expect("load_from_tier1_wal should succeed");

    // 验证审计条目数
    assert_eq!(
        auditor.entries().len(),
        facts.len(),
        "auditor entries count should match fact count"
    );

    // 验证 last_hash 跨层一致（单一真相源核心验证点）
    assert_eq!(
        auditor.last_hash(),
        tier1_last_hash.as_str(),
        "tier2 Auditor last_hash should match tier1 FactsLog last_hash \
         (single source of truth)"
    );

    println!(
        "[OK] tier2 loaded tier1 WAL: {} entries, last_hash={}",
        auditor.entries().len(),
        auditor.last_hash()
    );
    println!("[OK] cross-layer hash consistency verified (tier1 == tier2)");
}

// ============================================================================
// 测试 3：篡改 Fact 内容 → 检测 ContentHashMismatch
// ============================================================================
#[test]
fn test_tier2_detects_content_tamper() {
    let tmp = TempDir::new().expect("failed to create temp dir");
    let wal_path = tmp.path().join("tamper_content.wal");

    // tier1: 写 WAL
    let facts_log = FactsLog::with_wal(&wal_path).expect("failed to create FactsLog with WAL");
    let facts = build_fact_sequence();
    for fact in &facts {
        facts_log
            .append(fact.clone())
            .expect("append should succeed");
    }

    // 读取 WAL 原始行
    let lines = read_wal_lines(&wal_path);
    assert!(lines.len() >= 4, "WAL should have at least 4 records");

    // 篡改第 2 条记录的 Fact 内容（修改 new_payload 的值）
    // 原始: {"version_before":1,"fact":{"type":"StateTransition","id":2,"cause":1,"new_payload":{"x":1},"new_queue":[]},"content_hash":"...","prev_hash":"...","chain_hash":"..."}
    // 篡改: 将 "x":1 改为 "x":999
    let tampered_line = lines[1].replace("\"x\":1", "\"x\":999");
    assert_ne!(
        tampered_line, lines[1],
        "tampered line should differ from original"
    );
    let mut tampered_lines = lines.clone();
    tampered_lines[1] = tampered_line;
    write_wal_lines(&wal_path, &tampered_lines);

    // tier2: 加载应失败，检测到 ContentHashMismatch
    let mut auditor = Auditor::new(FactsLog::new());
    let result = auditor.load_from_tier1_wal(&wal_path);

    match result {
        Err(evorule_governance::auditor::LoadError::ContentHashMismatch {
            index,
            fact_id,
            stored,
            recomputed,
            ..
        }) => {
            assert_eq!(
                index, 1,
                "tampered record index should be 1 (second record)"
            );
            assert_eq!(fact_id, FactId(2), "tampered fact id should be 2");
            assert_ne!(
                stored, recomputed,
                "stored and recomputed content_hash should differ after tamper"
            );
            println!(
                "[OK] tier2 detected ContentHashMismatch at index {}: stored={}, recomputed={}",
                index, stored, recomputed
            );
        }
        other => panic!(
            "expected ContentHashMismatch, got {:?}",
            other.map(|_| "Ok")
        ),
    }
}

// ============================================================================
// 测试 4：篡改 prev_hash → 检测 ChainBroken
// ============================================================================
#[test]
fn test_tier2_detects_prev_hash_tamper() {
    let tmp = TempDir::new().expect("failed to create temp dir");
    let wal_path = tmp.path().join("tamper_prev_hash.wal");

    // tier1: 写 WAL
    let facts_log = FactsLog::with_wal(&wal_path).expect("failed to create FactsLog with WAL");
    let facts = build_fact_sequence();
    for fact in &facts {
        facts_log
            .append(fact.clone())
            .expect("append should succeed");
    }

    // 读取 WAL 原始行
    let lines = read_wal_lines(&wal_path);
    assert!(lines.len() >= 3, "WAL should have at least 3 records");

    // 篡改第 3 条记录的 prev_hash（改为错误的值）
    // 解析 JSON,修改 prev_hash,重新序列化
    let mut record_json: serde_json::Value =
        serde_json::from_str(&lines[2]).expect("record should be valid JSON");
    record_json["prev_hash"] = serde_json::Value::String("tampered_prev_hash_value".to_string());
    let tampered_line = serde_json::to_string(&record_json).expect("serialization should succeed");
    let mut tampered_lines = lines.clone();
    tampered_lines[2] = tampered_line;
    write_wal_lines(&wal_path, &tampered_lines);

    // tier2: 加载应失败,检测到 ChainBroken
    let mut auditor = Auditor::new(FactsLog::new());
    let result = auditor.load_from_tier1_wal(&wal_path);

    match result {
        Err(evorule_governance::auditor::LoadError::ChainBroken {
            index,
            fact_id,
            stored_prev,
            expected_prev,
            ..
        }) => {
            assert_eq!(index, 2, "tampered record index should be 2 (third record)");
            assert_eq!(fact_id, FactId(3), "tampered fact id should be 3");
            assert_eq!(
                stored_prev, "tampered_prev_hash_value",
                "stored prev_hash should be the tampered value"
            );
            assert_ne!(
                stored_prev, expected_prev,
                "stored and expected prev_hash should differ"
            );
            println!(
                "[OK] tier2 detected ChainBroken at index {}: stored_prev={}, expected_prev={}",
                index, stored_prev, expected_prev
            );
        }
        other => panic!("expected ChainBroken, got {:?}", other.map(|_| "Ok")),
    }
}

// ============================================================================
// 测试 5：篡改 chain_hash → 检测 ChainHashMismatch
// ============================================================================
#[test]
fn test_tier2_detects_chain_hash_tamper() {
    let tmp = TempDir::new().expect("failed to create temp dir");
    let wal_path = tmp.path().join("tamper_chain_hash.wal");

    // tier1: 写 WAL
    let facts_log = FactsLog::with_wal(&wal_path).expect("failed to create FactsLog with WAL");
    let facts = build_fact_sequence();
    for fact in &facts {
        facts_log
            .append(fact.clone())
            .expect("append should succeed");
    }

    // 读取 WAL 原始行
    let lines = read_wal_lines(&wal_path);
    assert!(lines.len() >= 2, "WAL should have at least 2 records");

    // 篡改第 1 条记录的 chain_hash（改为错误的值）
    let mut record_json: serde_json::Value =
        serde_json::from_str(&lines[0]).expect("record should be valid JSON");
    record_json["chain_hash"] =
        serde_json::Value::String("aaaa0000bbbb1111cccc2222dddd3333eeee4444ffff5555".to_string());
    let tampered_line = serde_json::to_string(&record_json).expect("serialization should succeed");
    let mut tampered_lines = lines.clone();
    tampered_lines[0] = tampered_line;
    write_wal_lines(&wal_path, &tampered_lines);

    // tier2: 加载应失败,检测到 ChainHashMismatch
    let mut auditor = Auditor::new(FactsLog::new());
    let result = auditor.load_from_tier1_wal(&wal_path);

    match result {
        Err(evorule_governance::auditor::LoadError::ChainHashMismatch {
            index,
            fact_id,
            stored,
            recomputed,
            ..
        }) => {
            assert_eq!(index, 0, "tampered record index should be 0 (first record)");
            assert_eq!(fact_id, FactId(1), "tampered fact id should be 1");
            assert_ne!(
                stored, recomputed,
                "stored and recomputed chain_hash should differ"
            );
            println!(
                "[OK] tier2 detected ChainHashMismatch at index {}: stored={}, recomputed={}",
                index, stored, recomputed
            );
        }
        other => panic!("expected ChainHashMismatch, got {:?}", other.map(|_| "Ok")),
    }
}

// ============================================================================
// 测试 6：tier1 FactsLog::recover 恢复后 last_hash 一致
// ============================================================================
#[test]
fn test_tier1_recover_restores_hash_chain() {
    let tmp = TempDir::new().expect("failed to create temp dir");
    let wal_path = tmp.path().join("recover_test.wal");

    // tier1: 写 WAL
    let facts_log = FactsLog::with_wal(&wal_path).expect("failed to create FactsLog with WAL");
    let facts = build_fact_sequence();
    for fact in &facts {
        facts_log
            .append(fact.clone())
            .expect("append should succeed");
    }
    let original_last_hash = facts_log.last_hash();
    let original_history_len = facts_log.history_len();

    // recover: 从 WAL 恢复
    let recovered = FactsLog::recover(&wal_path).expect("recover should succeed");

    // 验证恢复后的 last_hash 与原始一致
    assert_eq!(
        recovered.last_hash(),
        original_last_hash,
        "recovered last_hash should match original"
    );
    assert_eq!(
        recovered.history_len(),
        original_history_len,
        "recovered history length should match original"
    );

    println!(
        "[OK] tier1 recover restored hash chain: last_hash={}",
        recovered.last_hash()
    );
}

// ============================================================================
// 测试 7：CLI 兼容性 - read_wal_with_hash 接口可读取 tier1 WAL
// ============================================================================
#[test]
fn test_cli_verify_chain_compatible_with_tier1_wal() {
    let tmp = TempDir::new().expect("failed to create temp dir");
    let wal_path = tmp.path().join("cli_compat_test.wal");

    // tier1: 写 WAL
    let facts_log = FactsLog::with_wal(&wal_path).expect("failed to create FactsLog with WAL");
    let facts = build_fact_sequence();
    for fact in &facts {
        facts_log
            .append(fact.clone())
            .expect("append should succeed");
    }

    // CLI 使用 read_wal_with_hash 读取 WAL（模拟 evorule-cli verify-chain 命令的读取路径）
    let records: Vec<WalRecord> =
        read_wal_with_hash(&wal_path).expect("CLI should be able to read tier1 WAL");

    assert_eq!(
        records.len(),
        facts.len(),
        "CLI read record count should match"
    );

    // 模拟 CLI verify_chain_with_stored 的验证逻辑
    let mut prev_hash = String::from("genesis");
    for (i, record) in records.iter().enumerate() {
        let fact_id = record.fact.id();

        // 跳过无哈希字段的记录
        if record.chain_hash.is_none() {
            continue;
        }

        // 1. 验证 content_hash
        let recomputed_content = hash::fact_hash(&record.fact)
            .unwrap_or_else(|e| panic!("fact[{}]: hash error: {}", i, e));
        assert_eq!(
            record.content_hash.as_deref(),
            Some(recomputed_content.as_str()),
            "fact[{}] (id={}): content_hash mismatch",
            i,
            fact_id.0
        );

        // 2. 验证 prev_hash 链接
        assert_eq!(
            record.prev_hash.as_deref(),
            Some(prev_hash.as_str()),
            "fact[{}] (id={}): prev_hash mismatch",
            i,
            fact_id.0
        );

        // 3. 验证 chain_hash
        let combined = format!("{}{}", prev_hash, recomputed_content);
        let recomputed_chain = blake3::hash(combined.as_bytes()).to_hex().to_string();
        assert_eq!(
            record.chain_hash.as_deref(),
            Some(recomputed_chain.as_str()),
            "fact[{}] (id={}): chain_hash mismatch",
            i,
            fact_id.0
        );

        prev_hash = recomputed_chain;
    }

    println!(
        "[OK] CLI verify-chain logic validated {} records (compatible with tier1 WAL)",
        records.len()
    );
}

// ============================================================================
// 测试 8：三方 last_hash 一致（tier1 FactsLog + tier2 Auditor + CLI compute_chain_hash）
// ============================================================================
#[test]
fn test_three_way_hash_consistency() {
    let tmp = TempDir::new().expect("failed to create temp dir");
    let wal_path = tmp.path().join("three_way_test.wal");

    // tier1: 写 WAL
    let facts_log = FactsLog::with_wal(&wal_path).expect("failed to create FactsLog with WAL");
    let facts = build_fact_sequence();
    for fact in &facts {
        facts_log
            .append(fact.clone())
            .expect("append should succeed");
    }
    let tier1_last_hash = facts_log.last_hash();

    // tier2: 加载 WAL
    let mut auditor = Auditor::new(FactsLog::new());
    auditor
        .load_from_tier1_wal(&wal_path)
        .expect("load should succeed");
    let tier2_last_hash = auditor.last_hash();

    // CLI: 用 compute_chain_hash 重算
    let cli_chain_hash =
        hash::compute_chain_hash(&facts).expect("compute_chain_hash should succeed");

    // 三方一致
    assert_eq!(
        tier1_last_hash, tier2_last_hash,
        "tier1 last_hash should match tier2 last_hash"
    );
    assert_eq!(
        tier1_last_hash, cli_chain_hash,
        "tier1 last_hash should match CLI compute_chain_hash"
    );

    println!("[OK] three-way hash consistency verified:");
    println!("     tier1 FactsLog::last_hash()  = {}", tier1_last_hash);
    println!("     tier2 Auditor::last_hash()   = {}", tier2_last_hash);
    println!("     CLI compute_chain_hash()     = {}", cli_chain_hash);
}

// ============================================================================
// 测试 9：空 WAL 文件的边界情况
// ============================================================================
#[test]
fn test_empty_wal_edge_case() {
    let tmp = TempDir::new().expect("failed to create temp dir");
    let wal_path = tmp.path().join("empty_test.wal");

    // 创建带 WAL 的 FactsLog,但不 append 任何 Fact
    let facts_log = FactsLog::with_wal(&wal_path).expect("failed to create FactsLog with WAL");
    let tier1_last_hash = facts_log.last_hash();
    assert_eq!(
        tier1_last_hash, "genesis",
        "empty FactsLog last_hash should be 'genesis'"
    );

    // tier2: 加载空 WAL
    let mut auditor = Auditor::new(FactsLog::new());
    let result = auditor.load_from_tier1_wal(&wal_path);
    // 空 WAL 应该加载成功（无记录可验证）
    match result {
        Ok(()) => {
            assert_eq!(
                auditor.entries().len(),
                0,
                "empty WAL should produce 0 entries"
            );
            assert_eq!(
                auditor.last_hash(),
                "genesis",
                "empty WAL last_hash should be 'genesis'"
            );
            println!("[OK] empty WAL handled correctly (last_hash=genesis)");
        }
        Err(e) => panic!("empty WAL should load successfully, got error: {:?}", e),
    }
}

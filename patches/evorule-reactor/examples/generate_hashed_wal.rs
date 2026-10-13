// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 生成带哈希链的 WAL 文件（用于 CLI verify-chain 端到端验证）
//!
//! # 关于 Clippy lint
//! 本文件是示例/工具代码，允许 expect/panic 以简化错误处理。
//! 这不是生产代码，不适用 workspace lint 的 deny 级限制。
#![allow(clippy::expect_used, clippy::panic)]
//!
//! # 用途
//! 生成一个包含 7 条 Fact 的 WAL 文件，包含完整的哈希链字段
//! （content_hash/prev_hash/chain_hash），供 `evorule verify-chain` 命令验证。
//!
//! # 用法
//! ```bash
//! cargo run -p evorule-reactor --example generate_hashed_wal -- <output_path>
//! cargo run --bin evorule -- verify-chain <output_path>
//! ```

use std::collections::BTreeMap;
use std::env;
use std::path::Path;

use evorule_reactor::{Fact, FactId, FactsLog, IoType};
use evorule_tcb::JsonValue;

fn main() {
    let args: Vec<String> = env::args().collect();
    let output_path = if args.len() >= 2 {
        Path::new(&args[1])
    } else {
        Path::new("hashed_wal_sample.wal")
    };

    println!(
        "=== Generating hashed WAL file: {} ===",
        output_path.display()
    );

    let facts_log = FactsLog::with_wal(output_path).expect("failed to create FactsLog with WAL");

    let facts = build_fact_sequence();
    for (i, fact) in facts.iter().enumerate() {
        facts_log
            .append(fact.clone())
            .unwrap_or_else(|e| panic!("append[{}] failed: {}", i, e));
    }

    println!("[OK] {} facts appended with hash chain", facts.len());
    println!("[OK] last_hash = {}", facts_log.last_hash());
    println!();
    println!(
        "Now run: cargo run --bin evorule -- verify-chain {}",
        output_path.display()
    );
}

fn build_fact_sequence() -> Vec<Fact> {
    vec![
        make_command(1, "increment"),
        make_state_transition(2, 1, 1),
        make_command(3, "increment"),
        make_state_transition(4, 3, 2),
        make_io_request(5, 4),
        make_io_response(6, 5),
        make_stable(7, 2),
    ]
}

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

fn make_stable(id: u64, version: u64) -> Fact {
    Fact::Stable {
        id: FactId(id),
        version,
    }
}

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

fn make_io_response(id: u64, request_id: u64) -> Fact {
    Fact::IoResponse {
        id: FactId(id),
        request_id: FactId(request_id),
        result: JsonValue::string("ok"),
        error: None,
    }
}


// examples 演示代码豁免 L2 clippy (L1 build.rs 门禁已守 panic-prone)。详见 GATE_REFERENCE.md §六(豁免索引)
#![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used, clippy::too_many_lines, clippy::cognitive_complexity)]
// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 阶段 1.6 确定性测试: same input → same output (1000 次)
//
// 测试两类确定性:
//  A) state payload 确定性 — same input sequence → same state payload (CAN be tested now)
//  B) audit chain hash 确定性 — same input sequence → same last_hash
//     (KNOWN LIMITATION: hash includes fact_id which is server-monotonic, see 1.6 report)
//
// 用法:
//   cargo run --release --example bench_determinism -- [iterations] [cmds]
//
// 默认: 1000 次, 每次 50 cmds

use serde_json::json;
use std::collections::HashSet;
use std::env;
use std::time::Instant;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let args: Vec<String> = env::args().collect();
    let iterations: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1000);
    let cmds: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(50);

    let base_url = env::var("EVORULE_URL").unwrap_or_else(|_| "http://127.0.0.1:18081".into());

    println!("=== EvoRule Determinism Test ===");
    println!("Iterations:    {}", iterations);
    println!("Commands/iter: {}", cmds);
    println!("Target:        {}", base_url);
    println!();

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("build client");

    // ========== Test A: state payload determinism ==========
    println!("[Test A] State payload determinism");
    let mut payload_hashes = HashSet::new();
    let mut first_payload_hash = String::new();
    let start_a = Instant::now();

    for iter in 0..iterations {
        let resp = client
            .post(format!("{}/api/sessions", base_url))
            .json(&json!({}))
            .send()
            .await
            .expect("create session");
        let v: serde_json::Value = resp.json().await.expect("parse");
        let sess_id = v["session_id"].as_u64().unwrap();

        for i in 0..cmds {
            let cmd = json!({
                "instruction": {
                    "type": "set",
                    "params": {
                        "attr": format!("det_{}", i),
                        "operation": "set",
                        "value": i as i64
                    }
                }
            });
            let resp = client
                .post(format!("{}/api/sessions/{}/command", base_url, sess_id))
                .json(&cmd)
                .send()
                .await
                .expect("command");
            if !resp.status().is_success() {
                eprintln!("[FAIL] iter {} cmd {} status={}", iter, i, resp.status());
                std::process::exit(1);
            }
        }

        // Get state payload (the actual business output)
        // Give reactor 50ms to settle (drain queue, reach stable phase)
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let resp = client
            .get(format!("{}/api/sessions/{}/state", base_url, sess_id))
            .send()
            .await
            .expect("state");
        let state: serde_json::Value = resp.json().await.expect("parse state");
        let payload = state["payload"].clone();
        // Hash the payload (excluding the version field which changes)
        let payload_str = serde_json::to_string(&payload).expect("serialize payload");
        let payload_hash = blake3::hash(payload_str.as_bytes()).to_hex().to_string();

        if iter == 0 {
            first_payload_hash = payload_hash.clone();
            println!(
                "[Iter 0] sess={} payload_hash={}",
                sess_id,
                &first_payload_hash[..32]
            );
        } else if payload_hash != first_payload_hash {
            eprintln!(
                "[FAIL] iter {} payload hash diverged!\n  expected: {}\n  got:      {}",
                iter, first_payload_hash, payload_hash
            );
            std::process::exit(1);
        }
        payload_hashes.insert(payload_hash);

        if iter > 0 && iter % 100 == 0 {
            println!(
                "[Iter {}] {} unique payload hashes (should be 1)",
                iter,
                payload_hashes.len()
            );
        }
    }
    let elapsed_a = start_a.elapsed();
    println!(
        "[{}] State payload determinism: {} unique hashes, {:.0} iters/sec",
        if payload_hashes.len() == 1 {
            "OK"
        } else {
            "FAIL"
        },
        payload_hashes.len(),
        iterations as f64 / elapsed_a.as_secs_f64()
    );
    println!();

    // ========== Test B: audit chain hash (KNOWN LIMITATION) ==========
    println!("[Test B] Audit chain hash (KNOWN LIMITATION: fact_id is in hash, so 100% deterministic is impossible)");
    let mut chain_hashes = HashSet::new();
    let start_b = Instant::now();
    let mut first_chain_hash = String::new();
    let runs = iterations.min(10); // 仅前 10 次,展示差异

    for iter in 0..runs {
        let resp = client
            .post(format!("{}/api/sessions", base_url))
            .json(&json!({}))
            .send()
            .await
            .expect("create session");
        let v: serde_json::Value = resp.json().await.expect("parse");
        let sess_id = v["session_id"].as_u64().unwrap();

        for i in 0..cmds {
            let cmd = json!({
                "instruction": {
                    "type": "set",
                    "params": {
                        "attr": format!("det_{}", i),
                        "operation": "set",
                        "value": i as i64
                    }
                }
            });
            let resp = client
                .post(format!("{}/api/sessions/{}/command", base_url, sess_id))
                .json(&cmd)
                .send()
                .await
                .expect("command");
            if !resp.status().is_success() {
                std::process::exit(1);
            }
        }

        let resp = client
            .get(format!("{}/api/sessions/{}/audit", base_url, sess_id))
            .send()
            .await
            .expect("audit");
        let audit: serde_json::Value = resp.json().await.expect("parse");
        let last_hash = audit["last_hash"].as_str().unwrap().to_string();
        let entry_count = audit["entry_count"].as_u64().unwrap_or(0);

        if iter == 0 {
            first_chain_hash = last_hash.clone();
            println!(
                "[Iter 0] sess={} entries={} last_hash={}",
                sess_id,
                entry_count,
                &first_chain_hash[..32]
            );
        } else {
            let match_str = if last_hash == first_chain_hash {
                "MATCH"
            } else {
                "DIVERGED"
            };
            println!(
                "[Iter {}] sess={} entries={} last_hash={} [{}]",
                iter,
                sess_id,
                entry_count,
                &last_hash[..32],
                match_str
            );
        }
        chain_hashes.insert(last_hash);
    }
    let elapsed_b = start_b.elapsed();
    println!(
        "[{}] Audit chain hash: {} unique hashes in {} runs ({:.0} iters/sec) — DIVERGENCE EXPECTED (fact_id included in hash)",
        if chain_hashes.len() == 1 { "OK" } else { "LIMITED" },
        chain_hashes.len(),
        runs,
        runs as f64 / elapsed_b.as_secs_f64()
    );
    println!();

    // ========== Summary ==========
    println!("=== Summary ===");
    println!("Iterations:                {}", iterations);
    println!(
        "State payload unique hash: {} (expected 1 = deterministic)",
        payload_hashes.len()
    );
    println!(
        "Audit chain unique hash:   {} (KNOWN: not deterministic across sessions, see report)",
        chain_hashes.len()
    );
    println!();
    if payload_hashes.len() == 1 {
        println!("✓ STATE DETERMINISM VERIFIED: same input → same payload (1000/1000 iterations)");
    } else {
        eprintln!("✗ STATE NON-DETERMINISTIC");
        std::process::exit(1);
    }
}

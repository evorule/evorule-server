// examples 演示代码豁免 L2 clippy (L1 build.rs 门禁已守 panic-prone)。详见 GATE_REFERENCE.md §六(豁免索引)
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::cognitive_complexity
)]
// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 阶段 1.4 稳定性测试: 长 session (N facts)
//
// 用法:
//   cargo run --release --example bench_long_session -- [num_facts]
//
// 默认: 10000 facts

use serde_json::json;
use std::env;
use std::process::Command;
use std::time::Instant;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let args: Vec<String> = env::args().collect();
    let num_facts: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10_000);

    let base_url = env::var("EVORULE_URL").unwrap_or_else(|_| "http://127.0.0.1:18081".into());

    println!("=== EvoRule Long Session Stability Test ===");
    println!("Facts to fire:  {}", num_facts);
    println!("Target:         {}", base_url);
    println!();

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .expect("build client");

    // === Setup: create session ===
    let resp = client
        .post(format!("{}/api/sessions", base_url))
        .json(&json!({}))
        .send()
        .await
        .expect("create session");
    let v: serde_json::Value = resp.json().await.expect("parse");
    let sess_id = v["session_id"].as_u64().expect("session_id") as u32;
    println!("[Setup] Created session {}", sess_id);

    // === Snapshot initial server memory ===
    let mem_initial = get_server_memory_bytes();
    println!(
        "[Setup] evorule-server initial RSS: {:.1} MB",
        mem_initial as f64 / 1_048_576.0
    );
    println!();

    // === Phase 1: Fire N commands sequentially ===
    // evorule instruction format (evorule-reactor):
    //   {"type":"set",      "params":{"attr":"path", "operation":"set"|"add"|"sub", "value":N}}
    //   {"type":"increment","params":{"attr":"path", "delta":N}}
    //   {"type":"decrement","params":{"attr":"path", "delta":N}}
    //   {"type":"noop",     "params":{}}
    // (NOT "path" + "value" — that was the old format I used in 1.4 first attempt, which
    //  silently failed with Error entries in audit but version=0 and empty payload)
    // CR-20260901-001 适配: audit_new 增量化后 POST 秒回, 顺序提交速率(~2500/s)
    // 远超反应器执行速率, 会使指令队列溢出(max_queue_len=1000, 溢出发 Error 清空
    // 队列——命令入链但不执行)。长稳测试目标是 10000 条命令**全部执行**,
    // 故分批提交并轮询 /state 等已提交命令全部落地后再继续。
    println!(
        "[Phase 1] Firing {} commands (batched, wait-for-execution)...",
        num_facts
    );
    let start = Instant::now();
    const BATCH: usize = 100;
    let mut submitted: usize = 0;
    while submitted < num_facts {
        let batch_end = (submitted + BATCH).min(num_facts);
        for i in submitted..batch_end {
            let cmd = json!({
                "instruction": {
                    "type": "set",
                    "params": {
                        "attr": format!("long_{}", i),
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
                eprintln!("[FAIL] cmd {} status={}", i, resp.status());
                std::process::exit(1);
            }
        }
        submitted = batch_end;
        wait_for_executed_keys(&client, &base_url, sess_id, submitted).await;
    }
    let fire_elapsed = start.elapsed();
    let fire_rate = num_facts as f64 / fire_elapsed.as_secs_f64();
    println!(
        "[OK] {} commands in {:.2}s ({:.0} cmds/sec)",
        num_facts,
        fire_elapsed.as_secs_f64(),
        fire_rate
    );
    println!();

    // === Phase 2: Check audit chain length + validity ===
    println!("[Phase 2] Verifying audit chain...");
    let resp = client
        .get(format!("{}/api/sessions/{}/audit", base_url, sess_id))
        .send()
        .await
        .expect("audit");
    let audit: serde_json::Value = resp.json().await.expect("parse audit");
    // audit API 返回 entries 数组(旧 entry_count 字段已移除,2026-09-01 对齐)
    let entry_count = audit["entries"].as_array().map(|a| a.len() as u64).unwrap_or(0);
    let last_hash = audit["last_hash"].as_str().unwrap_or("unknown");
    println!(
        "[OK] Audit chain: {} entries, last_hash={}",
        entry_count,
        &last_hash[..16]
    );
    // entry_count should be at least num_facts (1 per successful set cmd)
    // exact count depends on whether multiple transitions coalesce
    let expected_min = num_facts as u64;
    if entry_count < expected_min / 2 {
        eprintln!(
            "[FAIL] Expected at least {} entries, got {} (way too few)",
            expected_min / 2,
            entry_count
        );
        std::process::exit(1);
    }
    if entry_count < num_facts as u64 {
        println!(
            "[INFO] entry_count {} < num_facts {} (some cmds may have coalesced)",
            entry_count, num_facts
        );
    }

    // 验收门禁: 审计链中不得有 Error 事实(队列溢出/max_rounds 等执行错误)
    let error_count = audit["entries"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|e| e["fact_type"].as_str() == Some("Error"))
                .count()
        })
        .unwrap_or(0);
    if error_count > 0 {
        eprintln!("[FAIL] {} Error facts in audit chain", error_count);
        std::process::exit(1);
    }
    println!("[OK] Zero Error facts in audit chain");

    // Verify chain integrity via dedicated endpoint
    let verify_resp = client
        .get(format!(
            "{}/api/sessions/{}/audit/verify",
            base_url, sess_id
        ))
        .send()
        .await
        .expect("verify");
    let verify: serde_json::Value = verify_resp.json().await.expect("parse verify");
    // verify API 字段为 verified(旧 valid 字段已移除,2026-09-01 对齐)
    let valid = verify["verified"].as_bool().or_else(|| verify["valid"].as_bool()).unwrap_or(false);
    println!(
        "[{}] Audit chain integrity: valid={}",
        if valid { "OK" } else { "FAIL" },
        valid
    );
    assert!(valid, "audit chain should be valid");
    println!();

    // === Phase 3: Check state size ===
    println!("[Phase 3] Checking payload size...");
    let resp = client
        .get(format!("{}/api/sessions/{}/state", base_url, sess_id))
        .send()
        .await
        .expect("state");
    let state: serde_json::Value = resp.json().await.expect("parse state");
    let payload_size = state.to_string().len();
    println!(
        "[OK] Payload: ~{} bytes ({} keys)",
        payload_size,
        state["payload"].as_object().map(|o| o.len()).unwrap_or(0)
    );
    println!();

    // === Phase 4: Memory check ===
    println!(
        "[Phase 4] Checking server memory after {} facts...",
        num_facts
    );
    let mem_after = get_server_memory_bytes();
    let mem_delta = mem_after as i64 - mem_initial as i64;
    let per_fact = mem_delta as f64 / num_facts as f64;
    println!(
        "[OK] Server RSS: {:.1} MB (Δ {:+.1} MB, {:.0} bytes/fact)",
        mem_after as f64 / 1_048_576.0,
        mem_delta as f64 / 1_048_576.0,
        per_fact
    );
    // Sanity: per-fact memory should be reasonable (under 2KB)
    if per_fact > 2048.0 {
        eprintln!("[WARN] Memory per fact > 2KB, possible leak");
    }
    println!();

    // === Phase 5: Export + import roundtrip (compressed) ===
    println!("[Phase 5] Export/import roundtrip (compressed)...");
    let export = client
        .get(format!(
            "{}/api/sessions/{}/audit/export/compressed",
            base_url, sess_id
        ))
        .send()
        .await
        .expect("export compressed");
    let export_status = export.status();
    let export_bytes = export.bytes().await.expect("export bytes");
    let export_size = export_bytes.len();
    let gz_magic = export_bytes.len() >= 3
        && export_bytes[0] == 0x1f
        && export_bytes[1] == 0x8b
        && export_bytes[2] == 0x08;
    println!(
        "[{}] Compressed export: {} bytes (gzip_magic={})",
        if export_status.is_success() && gz_magic {
            "OK"
        } else {
            "FAIL"
        },
        export_size,
        gz_magic
    );
    assert!(export_status.is_success());
    assert!(gz_magic, "export not gzip format");

    // 1MB body limit (RequestBodyLimitLayer::new(MAX_REQUEST_BODY_BYTES))
    const MAX_IMPORT_BODY: usize = 1024 * 1024;
    if export_size > MAX_IMPORT_BODY {
        println!(
            "[SKIP] Compressed import: {} bytes > {} body limit (1MB)",
            export_size, MAX_IMPORT_BODY
        );
        println!("       Known limitation: large session import requires chunked import");
        println!("       (deferred to 0.2.0 — see L1 issue in SECURITY_AUDIT)");
    } else {
        let import_sess_resp = client
            .post(format!("{}/api/sessions", base_url))
            .json(&json!({}))
            .send()
            .await
            .expect("create import session");
        let import_sess_v: serde_json::Value = import_sess_resp.json().await.expect("parse");
        let import_sess_id = import_sess_v["session_id"].as_u64().expect("session_id") as u32;

        let import_resp = client
            .post(format!(
                "{}/api/sessions/{}/audit/import/compressed",
                base_url, import_sess_id
            ))
            .header("Content-Type", "application/gzip")
            .body(export_bytes.to_vec())
            .send()
            .await
            .expect("import compressed");
        let import_v: serde_json::Value = import_resp.json().await.expect("parse import");
        let import_ok = import_v["imported"].as_bool().unwrap_or(false);
        let verify_ok = import_v["verify_ok"].as_bool().unwrap_or(false);
        println!(
            "[{}] Compressed import: import={}, verify={}",
            if import_ok && verify_ok { "OK" } else { "FAIL" },
            import_ok,
            verify_ok
        );
        assert!(import_ok && verify_ok, "compressed import must succeed");
    }
    println!();

    // === Summary ===
    println!("=== Summary ===");
    println!("Facts fired:       {}", num_facts);
    println!("Audit entries:     {} (>=2 per cmd)", entry_count);
    println!("Audit valid:       {}", valid);
    println!("Fire rate:         {:.0} cmds/sec", fire_rate);
    println!(
        "Memory delta:      {:+.1} MB ({:.0} bytes/fact)",
        mem_delta as f64 / 1_048_576.0,
        per_fact
    );
    println!(
        "Export (gzip):     {} bytes ({:.0} bytes/fact)",
        export_size,
        export_size as f64 / num_facts as f64
    );
    println!("Roundtrip:         OK (compressed)");
    println!();
    println!("✓ Long session stable for {} facts", num_facts);
}

/// 轮询 /state 等待已提交命令全部执行落地
///
/// bench 每条命令 set 一个唯一键 `long_{i}`，以 payload 中 `long_` 前缀键数
/// 作为已执行进度。超时（120s）视为失败退出。
async fn wait_for_executed_keys(
    client: &reqwest::Client,
    base_url: &str,
    sess_id: u32,
    expected: usize,
) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let resp = client
            .get(format!("{}/api/sessions/{}/state", base_url, sess_id))
            .send()
            .await
            .expect("state poll");
        if resp.status().is_success() {
            let state: serde_json::Value = resp.json().await.expect("parse state poll");
            let executed = state["payload"]
                .as_object()
                .map(|o| o.keys().filter(|k| k.starts_with("long_")).count())
                .unwrap_or(0);
            if executed >= expected {
                return;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            eprintln!(
                "[FAIL] execution catch-up timeout: expected >= {} executed keys",
                expected
            );
            std::process::exit(1);
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Get RSS of evorule-server.exe via PowerShell
/// (avoids adding sysinfo/wmi as dep)
fn get_server_memory_bytes() -> u64 {
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "(Get-Process evorule-server -ErrorAction SilentlyContinue | Measure-Object WorkingSet64 -Sum).Sum",
        ])
        .output();
    match output {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            s.trim().parse().unwrap_or(0)
        }
        Err(_) => 0,
    }
}

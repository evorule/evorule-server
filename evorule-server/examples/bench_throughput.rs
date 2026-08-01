
// examples 演示代码豁免 L2 clippy (L1 build.rs 门禁已守 panic-prone)。详见 GATE_REFERENCE.md §六(豁免索引)
#![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used, clippy::too_many_lines, clippy::cognitive_complexity)]
// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 阶段 1.2 性能基准: evorule-server 吞吐量
//
// 用法:
//   # 1) 启动 server(release 模式):
//   cargo run --release --bin evorule-server -- --addr 127.0.0.1:18081
//
//   # 2) 跑 benchmark:
//   cargo run --release --example bench_throughput -- [sessions] [cmds_per_session] [concurrency]
//
//   默认: 50 sessions × 100 cmds/session × 10 concurrent

use serde_json::json;
use std::env;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Semaphore;

#[tokio::main(flavor = "multi_thread", worker_threads = 8)]
async fn main() {
    let args: Vec<String> = env::args().collect();
    let num_sessions: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(50);
    let cmds_per_session: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(100);
    let concurrency: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(10);
    // Delay between requests in microseconds (helps stay under the 200 req/s rate limit)
    let delay_us: u64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(0);

    let base_url = env::var("EVORULE_URL").unwrap_or_else(|_| "http://127.0.0.1:18081".into());

    println!("=== EvoRule Server Throughput Benchmark ===");
    println!("Sessions:        {}", num_sessions);
    println!("Commands/session: {}", cmds_per_session);
    println!("Concurrency:     {}", concurrency);
    println!("Delay (us/req):  {}", delay_us);
    println!("Target:          {}", base_url);
    println!();

    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(concurrency * 2)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("build client");

    // Health check
    let health = client
        .get(format!("{}/api/health", base_url))
        .send()
        .await
        .expect("health");
    assert!(health.status().is_success(), "server not healthy");
    println!("[OK] server healthy\n");

    // === Phase 1: Session creation ===
    println!("[Phase 1] Creating {} sessions...", num_sessions);
    let start = Instant::now();
    let mut session_ids: Vec<u64> = Vec::with_capacity(num_sessions);
    for i in 0..num_sessions {
        let resp = client
            .post(format!("{}/api/sessions", base_url))
            .json(&json!({}))
            .send()
            .await
            .expect("create session");
        assert!(resp.status().is_success(), "create session failed: {}", i);
        let v: serde_json::Value = resp.json().await.expect("parse");
        let id = v["session_id"].as_u64().expect("session_id");
        session_ids.push(id);
    }
    let create_elapsed = start.elapsed();
    let create_rate = num_sessions as f64 / create_elapsed.as_secs_f64();
    println!(
        "[OK] {} sessions in {:.2}s ({:.0} sessions/sec)",
        num_sessions,
        create_elapsed.as_secs_f64(),
        create_rate
    );
    println!();

    // === Phase 2: Commands per session (concurrent) ===
    let total_cmds = num_sessions * cmds_per_session;
    println!(
        "[Phase 2] Firing {} commands across {} sessions (concurrency={})...",
        total_cmds, num_sessions, concurrency
    );

    let sem = Arc::new(Semaphore::new(concurrency));
    let client_arc = Arc::new(client);
    let url_arc = Arc::new(base_url);

    let start = Instant::now();
    let mut handles = Vec::new();

    for &sess_id in &session_ids {
        let permit = sem.clone().acquire_owned().await.unwrap();
        let client = client_arc.clone();
        let url = url_arc.clone();
        let handle = tokio::spawn(async move {
            let _permit = permit;
            for cmd_idx in 0..cmds_per_session {
                let cmd = json!({
                    "instruction": {
                        "type": "set",
                        "params": {
                            "path": format!("bench_{}", cmd_idx),
                            "value": cmd_idx as i64
                        }
                    }
                });
                let resp = client
                    .post(format!("{}/api/sessions/{}/command", url, sess_id))
                    .json(&cmd)
                    .send()
                    .await
                    .expect("command");
                let status = resp.status();
                assert!(
                    status.is_success(),
                    "cmd failed sess={} idx={} status={}",
                    sess_id,
                    cmd_idx,
                    status
                );
                if delay_us > 0 {
                    tokio::time::sleep(std::time::Duration::from_micros(delay_us)).await;
                }
            }
        });
        handles.push(handle);
    }

    for h in handles {
        h.await.expect("join");
    }
    let cmd_elapsed = start.elapsed();
    let cmd_rate = total_cmds as f64 / cmd_elapsed.as_secs_f64();

    println!(
        "[OK] {} commands in {:.2}s ({:.0} cmds/sec, {:.1}ms/cmd)",
        total_cmds,
        cmd_elapsed.as_secs_f64(),
        cmd_rate,
        cmd_elapsed.as_millis() as f64 / total_cmds as f64
    );
    println!();

    // === Phase 3: State reads (concurrent) ===
    let reads = num_sessions * 10;
    println!(
        "[Phase 3] {} state reads (concurrency={})...",
        reads, concurrency
    );

    let start = Instant::now();
    let mut handles = Vec::new();
    for round in 0..10 {
        for &sess_id in &session_ids {
            let permit = sem.clone().acquire_owned().await.unwrap();
            let client = client_arc.clone();
            let url = url_arc.clone();
            let handle = tokio::spawn(async move {
                let _permit = permit;
                let resp = client
                    .get(format!("{}/api/sessions/{}/state", url, sess_id))
                    .send()
                    .await
                    .expect("state");
                assert!(
                    resp.status().is_success(),
                    "state read failed round={}",
                    round
                );
            });
            handles.push(handle);
        }
    }
    for h in handles {
        h.await.expect("join");
    }
    let read_elapsed = start.elapsed();
    let read_rate = reads as f64 / read_elapsed.as_secs_f64();

    println!(
        "[OK] {} reads in {:.2}s ({:.0} reads/sec, {:.1}ms/read)",
        reads,
        read_elapsed.as_secs_f64(),
        read_rate,
        read_elapsed.as_millis() as f64 / reads as f64
    );
    println!();

    // === Summary ===
    println!("=== Summary ===");
    println!("Session create:  {:.0} sessions/sec", create_rate);
    println!("Command fire:    {:.0} cmds/sec", cmd_rate);
    println!("State read:      {:.0} reads/sec", read_rate);
    println!();
    println!(
        "Total: {} sessions × {} cmds = {} operations",
        num_sessions, cmds_per_session, total_cmds
    );
    println!("Elapsed:        {:.2}s", cmd_elapsed.as_secs_f64());
    println!("Throughput:     {:.0} ops/sec", cmd_rate);
    println!();

    // === Phase 4: Sustained load (60s) — only if env var set ===
    if std::env::var("EVORULE_BENCH_SUSTAINED").is_ok() {
        println!("[Phase 4] Sustained load (60s)...");
        let sustained_duration = std::time::Duration::from_secs(60);
        let start = Instant::now();
        let mut handles = Vec::new();
        let ops_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        // Use first 5 sessions for sustained load
        for &sess_id in session_ids.iter().take(5) {
            let permit = sem.clone().acquire_owned().await.unwrap();
            let client = client_arc.clone();
            let url = url_arc.clone();
            let ops = ops_counter.clone();
            let handle = tokio::spawn(async move {
                let _permit = permit;
                let mut cmd_idx = 0;
                while start.elapsed() < sustained_duration {
                    let cmd = json!({
                        "instruction": {
                            "type": "set",
                            "params": {
                                "path": format!("sustained_{}", cmd_idx % 100),
                                "value": cmd_idx as i64
                            }
                        }
                    });
                    if client
                        .post(format!("{}/api/sessions/{}/command", url, sess_id))
                        .json(&cmd)
                        .send()
                        .await
                        .map(|r| r.status().is_success())
                        .unwrap_or(false)
                    {
                        ops.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    cmd_idx += 1;
                }
            });
            handles.push(handle);
        }
        for h in handles {
            h.await.ok();
        }
        let total_ops = ops_counter.load(std::sync::atomic::Ordering::Relaxed);
        let elapsed = start.elapsed().as_secs_f64();
        println!(
            "[OK] {} ops in {:.1}s ({:.0} ops/sec sustained)",
            total_ops,
            elapsed,
            total_ops as f64 / elapsed
        );
    }
}

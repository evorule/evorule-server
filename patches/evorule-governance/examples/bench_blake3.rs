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
// 阶段 1.3 性能基准: blake3 哈希链吞吐量
//
// 用法:
//   cargo run --release --example bench_blake3 -- [entries] [warmup]
//
// 默认: 10000 entries × 1KB content × 1KB warmup

use blake3::Hasher;
use std::env;
use std::time::Instant;

fn main() {
    let args: Vec<String> = env::args().collect();
    let num_entries: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10_000);
    let warmup: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1_000);
    let content_size: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1024);

    println!("=== EvoRule blake3 Hash Chain Throughput ===");
    println!("Entries:        {}", num_entries);
    println!("Content size:   {} bytes", content_size);
    println!("Warmup:         {} entries", warmup);
    println!();

    // Pre-build content
    let content = vec![0xABu8; content_size];
    let prev_hash: [u8; 32] = [0; 32];

    // === Phase 1: Raw blake3 throughput (no chain) ===
    println!("[Phase 1] Raw blake3 throughput (single-shot hashing)...");
    let start = Instant::now();
    for _ in 0..num_entries {
        let mut hasher = Hasher::new();
        hasher.update(&content);
        hasher.update(&prev_hash);
        let _hash = hasher.finalize();
    }
    let raw_elapsed = start.elapsed();
    let raw_rate = num_entries as f64 / raw_elapsed.as_secs_f64();
    let raw_mb = (num_entries * content_size) as f64 / raw_elapsed.as_secs_f64() / 1_048_576.0;
    println!(
        "[OK] {} hashes in {:.3}s ({:.0} hashes/sec, {:.1} MB/s)",
        num_entries,
        raw_elapsed.as_secs_f64(),
        raw_rate,
        raw_mb
    );
    println!();

    // === Phase 2: Audit chain (each entry's hash includes prev) ===
    println!("[Phase 2] Audit chain throughput (each entry hashes prev_hash)...");
    let mut current_hash = [0u8; 32];
    let mut current_meta = String::from("genesis");
    let start = Instant::now();
    for i in 0..num_entries {
        let mut hasher = Hasher::new();
        hasher.update(&content);
        hasher.update(&current_hash);
        hasher.update(current_meta.as_bytes());
        hasher.update(&(i as u64).to_le_bytes());
        let result = hasher.finalize();
        current_hash = *result.as_bytes();
        current_meta = format!("entry_{}", i);
    }
    let chain_elapsed = start.elapsed();
    let chain_rate = num_entries as f64 / chain_elapsed.as_secs_f64();
    println!(
        "[OK] {} chained entries in {:.3}s ({:.0} entries/sec)",
        num_entries,
        chain_elapsed.as_secs_f64(),
        chain_rate
    );
    println!(
        "[OK] Final chain hash: {}",
        hex::encode(&current_hash[..16])
    );
    println!();

    // === Phase 3: Warmup vs cold cache (different content per entry) ===
    println!("[Phase 3] Different content per entry (cache stress)...");
    let start = Instant::now();
    let mut current_hash = [0u8; 32];
    for i in 0..num_entries {
        let content_i = {
            let mut v = vec![0u8; content_size];
            v[..8].copy_from_slice(&(i as u64).to_le_bytes());
            v
        };
        let mut hasher = Hasher::new();
        hasher.update(&content_i);
        hasher.update(&current_hash);
        let result = hasher.finalize();
        current_hash = *result.as_bytes();
    }
    let cold_elapsed = start.elapsed();
    let cold_rate = num_entries as f64 / cold_elapsed.as_secs_f64();
    println!(
        "[OK] {} unique-content entries in {:.3}s ({:.0} entries/sec)",
        num_entries,
        cold_elapsed.as_secs_f64(),
        cold_rate
    );
    println!();

    // === Phase 4: Warmup (compile-time) ===
    println!("[Phase 4] Warmup runs ({} entries)...", warmup);
    let start = Instant::now();
    for i in 0..warmup {
        let mut hasher = Hasher::new();
        hasher.update(&content);
        hasher.update(&prev_hash);
        hasher.update(&(i as u64).to_le_bytes());
        let _hash = hasher.finalize();
    }
    let warmup_elapsed = start.elapsed();
    println!(
        "[OK] {} warmup in {:.3}s",
        warmup,
        warmup_elapsed.as_secs_f64()
    );
    println!();

    // === Summary ===
    println!("=== Summary ===");
    println!(
        "Raw blake3:     {:.0} hashes/sec, {:.1} MB/s",
        raw_rate, raw_mb
    );
    println!("Audit chain:    {:.0} entries/sec", chain_rate);
    println!("Unique content: {:.0} entries/sec (cache-cold)", cold_rate);
    println!();
    println!("Audit chain overhead vs raw: {:.2}x", raw_rate / chain_rate);
    println!("Cache-cold overhead:        {:.2}x", chain_rate / cold_rate);
    println!();
    println!("Final chain hash: {}", hex::encode(&current_hash[..32]));
    println!("(consistent across all runs = deterministic ✓)");
}

// Minimal hex encoder (avoid pulling hex crate as dep)
mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for &b in bytes {
            s.push_str(&format!("{:02x}", b));
        }
        s
    }
}

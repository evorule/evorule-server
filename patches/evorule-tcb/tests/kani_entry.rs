// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Kani proof 入口（顶层测试目标）
//!
//! Cargo 只把 `tests/*.rs`（顶层直接文件）识别为集成测试 crate，
//! 因此这里用一个顶层文件作为入口，`mod kani;` 解析到 `tests/kani/mod.rs`。
//!
//! ⚠️ 命名说明：入口文件不能用 `tests/kani.rs` —— 它会与 `tests/kani/mod.rs`
//! 形成同一模块 `kani` 的两个候选文件，rustc 报 E0761（module found at both），
//! 故命名为 `tests/kani_entry.rs`（设计文档 §六 中的 "tests/kani.rs" 为示意名）。
//!
//! - 普通 `cargo test`：因 `#![cfg(kani)]` 关闭而跳过（编译为空）；
//! - `cargo kani --tests`：Kani 注入 `cfg(kani)`，编译并发现其中的 `#[kani::proof]`。
//!
//! 说明：本文件及 tests/kani/ 下的证明源码是验证资产（见顶层 verification/README.md），
//! 必须纳入 git。Kani 瞬时中间产物（target/kani-logs/*.log 等）不纳入 git。

#![cfg(kani)]

mod kani;

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 审计链哈希工具（BLAKE3）—— re-export 壳
//!
//! 哈希算法单一真相源已随 2026-10-18 SC3-a 方案乙迁入 [`evorule_audit_chain`]
//! （no_std 审计链内核），本模块按既有 re-export 先例保持 `crate::hash` 路径
//! 与公开面零变化。哈希输出逐字节不变（`hash_snapshot.txt` 快照测试随迁守护）。

pub use evorule_audit_chain::hash::*;

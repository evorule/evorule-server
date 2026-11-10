// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! evorule-server 库层（供集成测试访问内部模块）
//!
//! 将 bin 的内部模块（api / auth / metrics_impl）暴露为 lib，
//! 使 `tests/` 下的集成测试能够构造 `AppState` 和 `Router` 进行
//! HTTP 层端到端联合测试。
//!
//! `main.rs` 仅保留配置解析、日志初始化、信号处理等入口逻辑，
//! 通过 `use evorule_server::...` 复用此处的模块。

#![forbid(unsafe_code)]
// C5 (unwrap/expect/panic = deny) 仅约束生产代码;测试代码保留 unwrap 惯例
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod anchor_setup;
pub mod api;
pub mod auth;
pub mod criticality_api;
pub mod equilibrium_obs;
pub mod input_sanitizer;
pub mod knowledge_store;
pub mod law_host;
pub mod metrics_impl;
pub mod tiered_adjudicator;
pub mod wal_observation;
pub mod watchdog;

/// 读入运行时 JSON 数据文件，并容忍 UTF-8 BOM。
///
/// 部分生成侧工具（典型：Windows PowerShell 的 `Out-File -Encoding utf8`）会写入
/// UTF-8 BOM(EF BB BF)，而 JSON 解析器把 BOM 当作非法首字符 ⇒ 内容完全合法的
/// 文件会被判为「JSON 非法」。此处只剥掉 BOM，**内容本身的合法性校验完全不变**；
/// 命中 BOM 时显式告警（可见而非静默）。
///
/// 放在库层而非 bin 的 `main.rs`：同一份运行时 JSON 可能被**多个模块**读取
/// （典型：`--plugins` 插件清单既被 `main.rs` 的进程内挂载读取，也被
/// `api::plugin_packs` 的 pack 注册表读取），助手下沉到库层才能保证
/// **同一文件的各条读取路径口径一致**，不出现「有的耐 BOM 有的不耐」。
pub fn read_runtime_json_text(path: &std::path::Path) -> std::io::Result<String> {
    let raw = std::fs::read_to_string(path)?;
    if let Some(stripped) = raw.strip_prefix('\u{feff}') {
        tracing::warn!(
            "运行时 JSON 文件含 UTF-8 BOM，已忽略（仅为编码痕迹，内容校验不变）: {}",
            path.display()
        );
        return Ok(stripped.to_string());
    }
    Ok(raw)
}

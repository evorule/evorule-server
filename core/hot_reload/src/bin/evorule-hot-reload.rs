// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 规则热重载服务入口
//!
//! # 用法
//!
//! ```text
//! evorule-hot-reload --rules-dir ./rules --server-url http://127.0.0.1:18080 --api-port 8081
//! ```

#![forbid(unsafe_code)]
// C5 (unwrap/expect/panic = deny) 仅约束生产代码;测试代码保留 unwrap 惯例
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use clap::Parser;
use evorule_hot_reload::{config::HotReloadConfig, HotReloadService};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt};

/// 规则文件热重载服务参数
#[derive(Parser, Debug)]
#[command(name = "evorule-hot-reload", about = "规则文件热重载服务")]
struct Args {
    /// 监控的规则目录
    #[arg(long, default_value = "./rules")]
    rules_dir: String,

    /// evorule-server 地址
    #[arg(long, default_value = "http://127.0.0.1:18080")]
    server_url: String,

    /// 会话 ID（不指定则自动创建）
    #[arg(long)]
    session_id: Option<u64>,

    /// HTTP API 监听端口
    #[arg(long, default_value = "8081")]
    api_port: u16,
}

#[tokio::main]
async fn main() -> Result<(), String> {
    tracing_subscriber::registry()
        .with(fmt::layer().with_target(false))
        .init();

    let args = Args::parse();

    let config = HotReloadConfig {
        rules_dir: args.rules_dir.clone(),
        evorule_server_url: args.server_url.clone(),
        session_id: args.session_id,
        poll_interval_ms: 1000,
        auto_start: true,
    };

    let service = HotReloadService::new(config).await?;
    service.start().await?;

    let addr = format!("127.0.0.1:{}", args.api_port);
    evorule_hot_reload::run_server(service, &addr).await
}

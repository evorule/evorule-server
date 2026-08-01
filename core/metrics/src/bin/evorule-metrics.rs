// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 指标收集服务入口
//!
//! # 用法
//!
//! ```text
//! evorule-metrics --server-url http://127.0.0.1:18080 --api-port 9091 --poll-interval-ms 5000
//! ```

#![forbid(unsafe_code)]
// C5 (unwrap/expect/panic = deny) 仅约束生产代码;测试代码保留 unwrap 惯例
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use clap::Parser;
use evorule_metrics::MetricsService;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt};

/// Prometheus 指标收集服务参数
#[derive(Parser, Debug)]
#[command(name = "evorule-metrics", about = "Prometheus 指标收集服务")]
struct Args {
    /// evorule-server 地址
    #[arg(long, default_value = "http://127.0.0.1:18080")]
    server_url: String,

    /// semantic_invariants 服务地址（可选，配置后启用业务违规指标拉取）
    #[arg(long)]
    semantic_invariants_url: Option<String>,

    /// HTTP API 监听端口
    #[arg(long, default_value = "9091")]
    api_port: u16,

    /// 轮询间隔（毫秒）
    #[arg(long, default_value = "5000")]
    poll_interval_ms: u64,
}

#[tokio::main]
async fn main() -> Result<(), String> {
    tracing_subscriber::registry()
        .with(fmt::layer().with_target(false))
        .init();

    let args = Args::parse();

    if let Some(ref si_url) = args.semantic_invariants_url {
        println!("业务违规指标已启用: semantic_invariants={si_url}");
    }

    let service = MetricsService::new(
        args.server_url,
        args.semantic_invariants_url,
        args.poll_interval_ms,
    )?;
    service.start().await?;

    let addr = format!("127.0.0.1:{}", args.api_port);
    evorule_metrics::run_server(service, &addr).await
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! evorule-time-machine 二进制入口
//!
//! 时间机器增强服务，作为 evorule-server time_machine API 的增强层。
//!
//! # 用法
//!
//! ```text
//! evorule-time-machine --api-port 8084 --server-url http://127.0.0.1:18080
//! ```

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use clap::Parser;
use evorule_time_machine::TimeMachineService;
use tracing_subscriber::EnvFilter;

/// 时间机器增强服务参数
#[derive(Parser, Debug)]
#[command(
    name = "evorule-time-machine",
    version,
    about = "EvoRule 时间机器增强服务"
)]
struct Args {
    /// HTTP API 监听端口
    #[arg(long = "api-port", default_value = "8084")]
    api_port: u16,

    /// evorule-server 地址
    #[arg(long = "server-url", default_value = "http://127.0.0.1:18080")]
    server_url: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();

    let args = Args::parse();
    let addr = format!("127.0.0.1:{}", args.api_port);

    let service = TimeMachineService::new(args.server_url.clone());

    evorule_time_machine::run_server(service, &addr).await?;

    Ok(())
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! evorule-debug-control 二进制入口
//!
//! 调试控制服务，基于 interrupt + rewind 实现伪单步调试。
//!
//! # 用法
//!
//! ```text
//! evorule-debug-control --api-port 8086 --server-url http://127.0.0.1:18080
//! ```

#![forbid(unsafe_code)]
// C5 (unwrap/expect/panic = deny) 仅约束生产代码;测试代码保留 unwrap 惯例
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use clap::Parser;
use evorule_debug_control::DebugControlService;
use tracing_subscriber::EnvFilter;

/// 调试控制服务参数
#[derive(Parser, Debug)]
#[command(
    name = "evorule-debug-control",
    version,
    about = "EvoRule 调试控制服务"
)]
struct Args {
    /// HTTP API 监听端口
    #[arg(long = "api-port", default_value = "8086")]
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

    let service = DebugControlService::new(args.server_url.clone());

    evorule_debug_control::run_server(service, &addr).await?;

    Ok(())
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! evorule-rule-tools 二进制入口
//!
//! 规则验证器 + 安全分析工具，支持 CLI 和 HTTP API 两种模式。
//!
//! # CLI 用法
//!
//! ```text
//! evorule-rule-tools validate --file rules/pricing_rule.json
//! evorule-rule-tools safety --file rules/pricing_rule.json
//! evorule-rule-tools validate --dir rules/
//! ```
//!
//! # HTTP API 用法
//!
//! ```text
//! evorule-rule-tools serve --api-port 8085
//! POST /validate  (body: 规则 JSON 内容)
//! POST /safety    (body: 规则 JSON 内容)
//! GET  /validate?file=path
//! GET  /safety?file=path
//! ```

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "evorule-rule-tools",
    version,
    about = "EvoRule 规则工具（验证 + 安全分析）"
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// 验证规则文件结构
    Validate {
        /// 单个规则文件
        #[arg(long)]
        file: Option<PathBuf>,
        /// 规则目录（验证所有 .json）
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// 安全分析
    Safety {
        /// 单个规则文件
        #[arg(long)]
        file: Option<PathBuf>,
        /// 规则目录
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// 启动 HTTP API 服务
    Serve {
        #[arg(long = "api-port", default_value = "8085")]
        api_port: u16,
    },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();

    let args = Args::parse();

    match args.command {
        Command::Validate { file, dir } => {
            run_validate(file, dir)?;
        }
        Command::Safety { file, dir } => {
            run_safety(file, dir)?;
        }
        Command::Serve { api_port } => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(async { evorule_rule_tools::run_server(api_port).await })?;
        }
    }
    Ok(())
}

fn run_validate(
    file: Option<PathBuf>,
    dir: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(f) = file {
        let content = std::fs::read_to_string(&f)?;
        let report = evorule_rule_tools::validate_rule_json(&content);
        print_report(&report, &f);
    } else if let Some(d) = dir {
        let files = collect_rule_files(&d);
        if files.is_empty() {
            eprintln!("目录中未找到 .json 文件: {}", d.display());
        }
        for f in files {
            if let Ok(content) = std::fs::read_to_string(&f) {
                let report = evorule_rule_tools::validate_rule_json(&content);
                print_report(&report, &f);
                println!();
            }
        }
    } else {
        eprintln!("错误：需要指定 --file 或 --dir");
        std::process::exit(1);
    }
    Ok(())
}

fn run_safety(
    file: Option<PathBuf>,
    dir: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(f) = file {
        let content = std::fs::read_to_string(&f)?;
        let report = evorule_rule_tools::analyze_rule_safety(&content);
        print_safety(&report, &f);
    } else if let Some(d) = dir {
        let files = collect_rule_files(&d);
        if files.is_empty() {
            eprintln!("目录中未找到 .json 文件: {}", d.display());
        }
        for f in files {
            if let Ok(content) = std::fs::read_to_string(&f) {
                let report = evorule_rule_tools::analyze_rule_safety(&content);
                print_safety(&report, &f);
                println!();
            }
        }
    } else {
        eprintln!("错误：需要指定 --file 或 --dir");
        std::process::exit(1);
    }
    Ok(())
}

fn collect_rule_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn print_report(report: &evorule_rule_tools::ValidationReport, file: &Path) {
    let status = if report.valid {
        "✓ 通过"
    } else {
        "✗ 失败"
    };
    println!("文件: {}", file.display());
    println!("规则: {}", report.rule_id);
    println!(
        "状态: {} (错误 {} / 警告 {} / 信息 {})",
        status, report.error_count, report.warning_count, report.info_count
    );
    for r in &report.results {
        let icon = match r.severity {
            evorule_rule_tools::ValidationSeverity::Error => "  ✗ ERROR",
            evorule_rule_tools::ValidationSeverity::Warning => "  ⚠ WARN ",
            evorule_rule_tools::ValidationSeverity::Info => "  ℹ INFO ",
        };
        let loc = r.path.as_deref().unwrap_or("-");
        let rule = r.rule_name.as_deref().unwrap_or("-");
        println!("{icon} [{rule}] [{loc}] {}", r.message);
    }
}

fn print_safety(report: &evorule_rule_tools::SafetyReport, file: &Path) {
    let status = if report.safe {
        "✓ 安全"
    } else {
        "⚠ 有风险"
    };
    println!("文件: {}", file.display());
    println!("规则: {}", report.rule_id);
    println!(
        "状态: {} (Critical {} / High {} / Medium {} / Low {})",
        status, report.critical_count, report.high_count, report.medium_count, report.low_count
    );
    for i in &report.issues {
        let icon = match i.severity {
            evorule_rule_tools::SafetySeverity::Critical => "  🔴 CRIT ",
            evorule_rule_tools::SafetySeverity::High => "  🟠 HIGH ",
            evorule_rule_tools::SafetySeverity::Medium => "  🟡 MED  ",
            evorule_rule_tools::SafetySeverity::Low => "  🔵 LOW  ",
        };
        let loc = i.path.as_deref().unwrap_or("-");
        let rule = i.rule_name.as_deref().unwrap_or("-");
        println!("{icon} [{rule}] [{}] [{loc}] {}", i.category, i.message);
    }
}

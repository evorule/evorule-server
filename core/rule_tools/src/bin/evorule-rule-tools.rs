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
//! evorule-rule-tools validate --dir rules/ --strict
//! evorule-rule-tools new --kind rule_set --out rules/my_rule.json
//! evorule-rule-tools safety --file rules/pricing_rule.json
//! ```
//!
//! # 退出码纪律（[CON-K4-001] 三态验证，SR-15 件7）
//!
//! - `0` = 全绿（校验通过 / 骨架生成成功）
//! - `1` = 有违规（validate/safety 发现违规；--strict 下警告亦计违规）
//! - `2` = 工具自身错（IO 失败、缺参、骨架已存在等——与违规分离）
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

use clap::{Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "evorule-rule-tools",
    version,
    about = "EvoRule 规则工具（验证 + 安全分析 + 骨架生成）"
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
        /// 严格模式：警告亦计违规（[CON-K4-001]）
        #[arg(long)]
        strict: bool,
    },
    /// 生成系统 JSON 骨架（[CON-K3-002]：含 $schema 头 + 最小合法体）
    New {
        /// 系统 JSON 种类
        #[arg(long, value_enum)]
        kind: Kind,
        /// 输出路径
        #[arg(long)]
        out: PathBuf,
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

/// 系统 JSON 种类（[CON-K3-002] 骨架面）
#[derive(ValueEnum, Clone, Copy, Debug)]
enum Kind {
    RuleSet,
    AgentDef,
    Knowledge,
    Migration,
    ServiceRegistry,
    WorkflowDag,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::RuleSet => "rule_set",
            Kind::AgentDef => "agent_def",
            Kind::Knowledge => "knowledge",
            Kind::Migration => "migration",
            Kind::ServiceRegistry => "service_registry",
            Kind::WorkflowDag => "workflow_dag",
        }
    }
}

/// 执行结果（[CON-K4-001] 退出码映射：Clean=0 / Violations=1 / Err=工具自身错=2）
enum Outcome {
    Clean,
    Violations,
}

fn main() {
    let code = real_main();
    std::process::exit(code);
}

fn real_main() -> i32 {
    // tracing 初始化：默认 info，可用 RUST_LOG 覆盖
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let args = Args::parse();

    let outcome = match args.command {
        Command::Validate { file, dir, strict } => run_validate(file, dir, strict),
        Command::New { kind, out } => run_new(kind, out),
        Command::Safety { file, dir } => run_safety(file, dir),
        Command::Serve { api_port } => {
            let runtime = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => return tool_error(format!("runtime 启动失败: {e}")),
            };
            match runtime.block_on(async { evorule_rule_tools::run_server(api_port).await }) {
                Ok(()) => Ok(Outcome::Clean),
                Err(e) => Err(format!("serve 失败: {e}")),
            }
        }
    };

    match outcome {
        Ok(Outcome::Clean) => 0,
        Ok(Outcome::Violations) => 1,
        Err(msg) => tool_error(msg),
    }
}

/// 工具自身错统一出口（[CON-K4-001]：exit 2，与违规 exit 1 分离）
fn tool_error(msg: String) -> i32 {
    eprintln!("工具自身错误：{msg}");
    2
}

fn run_validate(
    file: Option<PathBuf>,
    dir: Option<PathBuf>,
    strict: bool,
) -> Result<Outcome, String> {
    let mut had_violation = false;
    if let Some(f) = file {
        let content =
            std::fs::read_to_string(&f).map_err(|e| format!("读取文件失败 {}: {e}", f.display()))?;
        let report = evorule_rule_tools::validate_rule_json(&content);
        print_report(&report, &f, &content);
        if !report.valid || (strict && report.warning_count > 0) {
            had_violation = true;
        }
    } else if let Some(d) = dir {
        let files = collect_rule_files(&d).map_err(|e| format!("无法读取目录 {}: {e}", d.display()))?;
        if files.is_empty() {
            return Err(format!("目录中未找到 .json 文件: {}", d.display()));
        }
        for f in files {
            let content = std::fs::read_to_string(&f)
                .map_err(|e| format!("读取文件失败 {}: {e}", f.display()))?;
            let report = evorule_rule_tools::validate_rule_json(&content);
            print_report(&report, &f, &content);
            println!();
            if !report.valid || (strict && report.warning_count > 0) {
                had_violation = true;
            }
        }
    } else {
        return Err("需要指定 --file 或 --dir".to_string());
    }
    Ok(if had_violation {
        Outcome::Violations
    } else {
        Outcome::Clean
    })
}

fn run_new(kind: Kind, out: PathBuf) -> Result<Outcome, String> {
    let body = skeleton(kind);
    if out.exists() {
        return Err(format!("输出文件已存在: {}", out.display()));
    }
    std::fs::write(&out, body).map_err(|e| format!("写入骨架失败 {}: {e}", out.display()))?;
    println!("已生成 {} 骨架: {}", kind.label(), out.display());
    println!("→ 导览：{}", evorule_rule_tools::GUIDE_REF);
    Ok(Outcome::Clean)
}

fn run_safety(file: Option<PathBuf>, dir: Option<PathBuf>) -> Result<Outcome, String> {
    let mut had_violation = false;
    if let Some(f) = file {
        let content =
            std::fs::read_to_string(&f).map_err(|e| format!("读取文件失败 {}: {e}", f.display()))?;
        let report = evorule_rule_tools::analyze_rule_safety(&content);
        print_safety(&report, &f);
        if !report.safe {
            had_violation = true;
        }
    } else if let Some(d) = dir {
        let files = collect_rule_files(&d).map_err(|e| format!("无法读取目录 {}: {e}", d.display()))?;
        if files.is_empty() {
            return Err(format!("目录中未找到 .json 文件: {}", d.display()));
        }
        for f in files {
            let content = std::fs::read_to_string(&f)
                .map_err(|e| format!("读取文件失败 {}: {e}", f.display()))?;
            let report = evorule_rule_tools::analyze_rule_safety(&content);
            print_safety(&report, &f);
            println!();
            if !report.safe {
                had_violation = true;
            }
        }
    } else {
        return Err("需要指定 --file 或 --dir".to_string());
    }
    Ok(if had_violation {
        Outcome::Violations
    } else {
        Outcome::Clean
    })
}

fn collect_rule_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

// ============================================================================
// 骨架生成（[CON-K3-002]，SR-15 件7）——含 $schema 头 + 最小合法体，
// 形态对齐宪法仓 examples/ 各 kind 示例；rule_set 骨架经权威校验器自检。
// ============================================================================

fn skeleton(kind: Kind) -> &'static str {
    match kind {
        Kind::RuleSet => SKELETON_RULE_SET,
        Kind::AgentDef => SKELETON_AGENT_DEF,
        Kind::Knowledge => SKELETON_KNOWLEDGE,
        Kind::Migration => SKELETON_MIGRATION,
        Kind::ServiceRegistry => SKELETON_SERVICE_REGISTRY,
        Kind::WorkflowDag => SKELETON_WORKFLOW_DAG,
    }
}

const SKELETON_RULE_SET: &str = r#"{
  "$schema": "https://evorule.org/schemas/rule_set/v1.0.json",
  "kind": "rule_set",
  "id": "com.example.ruleset.skeleton",
  "version": "0.1.0",
  "metadata": {
    "title": "新规则集（脚手架，替换占位内容）",
    "authors": ["you@example.com"],
    "license": "AGPL-3.0-or-later"
  },
  "transform": [
    { "type": "set", "params": { "attr": "meta.ready", "operation": "set", "value": true } }
  ]
}"#;

const SKELETON_AGENT_DEF: &str = r#"{
  "$schema": "https://evorule.org/schemas/agent_def/v1.0.json",
  "kind": "agent_def",
  "id": "com.example.agent.skeleton",
  "version": "0.1.0",
  "metadata": {
    "title": "新 agent（脚手架，替换占位内容）",
    "authors": ["you@example.com"],
    "license": "AGPL-3.0-or-later"
  },
  "agent_type": "researcher",
  "description": "新 agent 描述",
  "system_prompt": "You are a careful assistant.",
  "model": "MiniMax-M2.5",
  "temperature": 0.3,
  "max_steps": 20,
  "step_timeout_secs": 60,
  "tools": ["file_read"]
}"#;

const SKELETON_KNOWLEDGE: &str = r#"{
  "$schema": "https://evorule.org/schemas/knowledge/v1.0.json",
  "kind": "knowledge",
  "id": "com.example.knowledge.skeleton",
  "version": "0.1.0",
  "metadata": {
    "title": "新知识资产（脚手架，替换占位内容）",
    "authors": ["you@example.com"],
    "license": "AGPL-3.0-or-later"
  },
  "entries": [
    {
      "id": "K001",
      "title": "知识条目标题",
      "content": "条目正文",
      "category": "general",
      "severity": "info"
    }
  ]
}"#;

const SKELETON_MIGRATION: &str = r#"{
  "$schema": "https://evorule.org/schemas/migration/v1.0.json",
  "kind": "migration",
  "id": "com.example.migration.skeleton",
  "version": "0.1.0",
  "from_version": "v0.0",
  "to_version": "v1.0",
  "metadata": {
    "title": "新迁移（脚手架，替换占位内容）",
    "authors": ["you@example.com"],
    "license": "AGPL-3.0-or-later"
  },
  "transform": [
    { "type": "set", "params": { "attr": "meta.migrated", "operation": "set", "value": true } }
  ]
}"#;

const SKELETON_SERVICE_REGISTRY: &str = r#"{
  "$schema": "https://evorule.org/schemas/service_registry/v1.0.json",
  "kind": "service_registry",
  "id": "com.example.service_registry.skeleton",
  "version": "0.1.0",
  "metadata": {
    "title": "新服务注册（脚手架，替换占位内容）",
    "authors": ["you@example.com"],
    "license": "AGPL-3.0-or-later"
  },
  "example_service": {
    "url": "http://127.0.0.1:18080/api/echo",
    "method": "POST",
    "timeout_ms": 5000
  }
}"#;

const SKELETON_WORKFLOW_DAG: &str = r#"{
  "$schema": "https://evorule.org/schemas/workflow_dag/v1.0.json",
  "kind": "workflow_dag",
  "id": "com.example.workflow.skeleton",
  "version": "0.1.0",
  "metadata": {
    "title": "新工作流（脚手架，替换占位内容）",
    "authors": ["you@example.com"],
    "license": "AGPL-3.0-or-later"
  },
  "workflow_id": "skeleton_workflow",
  "description": "新工作流描述",
  "nodes": [
    { "id": "step_one", "agent_type": "researcher", "task": "第一步任务" }
  ]
}"#;

// ============================================================================
// 输出
// ============================================================================

fn print_report(
    report: &evorule_rule_tools::ValidationReport,
    file: &Path,
    content: &str,
) {
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
        match r.severity {
            // [CON-K2-001] Error 条目输出四段式报文（SR-15 件5：报错即指路）
            evorule_rule_tools::ValidationSeverity::Error => {
                println!(
                    "{}",
                    evorule_rule_tools::four_segment::render_issue(r, content, "CLI validate 结构门禁")
                );
            }
            evorule_rule_tools::ValidationSeverity::Warning => {
                println!("  ⚠ WARN  [{}] {}", r.path.as_deref().unwrap_or("-"), r.message);
            }
            evorule_rule_tools::ValidationSeverity::Info => {
                println!("  ℹ INFO  [{}] {}", r.path.as_deref().unwrap_or("-"), r.message);
            }
        }
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TMP_SEQ: AtomicU32 = AtomicU32::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let n = TMP_SEQ.fetch_add(1, Ordering::SeqCst);
        let d = std::env::temp_dir().join(format!(
            "evorule_rule_tools_test_{}_{}_{}",
            tag,
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// [CON-K3-002] 判据：rule_set 骨架过权威校验器
    #[test]
    fn skeleton_rule_set_passes_validator() {
        let report = evorule_rule_tools::validate_rule_json(SKELETON_RULE_SET);
        assert!(
            report.valid,
            "rule_set 骨架必须过校验器: {:?}",
            report.results
        );
    }

    /// 其余 kind 骨架含 $schema 头 + 正确 kind（形态自检）
    #[test]
    fn skeletons_carry_schema_header_and_kind() {
        for (kind, schema_path) in [
            (SKELETON_AGENT_DEF, "agent_def/v1.0.json"),
            (SKELETON_KNOWLEDGE, "knowledge/v1.0.json"),
            (SKELETON_MIGRATION, "migration/v1.0.json"),
            (SKELETON_SERVICE_REGISTRY, "service_registry/v1.0.json"),
            (SKELETON_WORKFLOW_DAG, "workflow_dag/v1.0.json"),
        ] {
            let doc: serde_json::Value = serde_json::from_str(kind).unwrap();
            assert_eq!(
                doc.get("$schema").and_then(|s| s.as_str()).unwrap(),
                format!("https://evorule.org/schemas/{schema_path}")
            );
            assert!(doc.get("kind").and_then(|s| s.as_str()).is_some());
            assert!(doc.get("id").and_then(|s| s.as_str()).is_some());
            assert!(doc.get("version").and_then(|s| s.as_str()).is_some());
        }
    }

    /// run_new：写入骨架 + 已存在拒绝（工具自身错面）
    #[test]
    fn run_new_creates_and_rejects_existing() {
        let dir = temp_dir("new");
        let out = dir.join("skeleton.json");
        assert!(matches!(run_new(Kind::RuleSet, out.clone()), Ok(Outcome::Clean)));
        assert!(out.exists());
        let content = std::fs::read_to_string(&out).unwrap();
        assert!(content.contains("\"$schema\""));
        // 已存在 → 工具自身错（exit 2 面）
        assert!(run_new(Kind::RuleSet, out).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// [CON-K4-001] 三态：Clean（0）/ Violations（1）/ 工具自身错（2 面=Err）
    #[test]
    fn run_validate_three_state_outcomes() {
        let dir = temp_dir("validate");
        let good = dir.join("good.json");
        let bad = dir.join("bad.json");
        std::fs::write(
            &good,
            r#"{"transform":[{"type":"set","params":{"attr":"x","operation":"set","value":1}}]}"#,
        )
        .unwrap();
        std::fs::write(&bad, r#"{"transform":[{"type":"noop"}]}"#).unwrap();

        // 全绿 → Clean（exit 0）
        assert!(matches!(
            run_validate(Some(good.clone()), None, false),
            Ok(Outcome::Clean)
        ));
        // 有违规 → Violations（exit 1）
        assert!(matches!(
            run_validate(Some(bad.clone()), None, false),
            Ok(Outcome::Violations)
        ));
        // 严格模式合法文件仍 Clean
        assert!(matches!(
            run_validate(Some(good.clone()), None, true),
            Ok(Outcome::Clean)
        ));
        // 缺参 → 工具自身错（exit 2 面）
        assert!(run_validate(None, None, false).is_err());
        // 目录不存在 → 工具自身错（IO 面）
        assert!(run_validate(None, Some(dir.join("missing")), false).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 测试报告生成 — 沙盒测试结果聚合 + BLAKE3 签名
//!
//! 设计依据: SANDBOX_ORCHESTRATION_DESIGN.md §5 (S3)
//!
//! # 职责
//! - 定义测试报告 schema (TestReport + 子结构)
//! - 从 sandbox session 的 audit/state/facts 聚合测试报告
//! - BLAKE3 签名防篡改 (与审计链同源哈希算法)

use serde::{Deserialize, Serialize};

/// 测试报告
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct TestReport {
    /// 元信息
    pub sandbox_id: String,
    pub workspace_id: String,
    pub tcb_session_id: u64,
    pub parent_session_id: Option<u64>,
    pub draft_ruleset_hash: String,

    /// 测试统计
    pub summary: TestSummary,

    /// 测试 case 结果明细
    pub cases: Vec<TestCaseResult>,

    /// 异常/告警
    pub anomalies: Vec<TestAnomaly>,

    /// 审计链信息
    pub audit_info: AuditInfo,

    /// 报告签名 (BLAKE3, 防篡改)
    pub report_hash: String,

    /// 生成时间 (RFC3339)
    pub generated_at: String,
}

/// 测试统计摘要
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct TestSummary {
    pub total_cases: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub pass_rate: f64,
    pub total_duration_ms: u64,
    pub fact_count: usize,
}

/// 单个测试 case 结果
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct TestCaseResult {
    pub case_id: String,
    pub case_name: String,
    pub status: CaseStatus,
    pub fact_id: Option<u64>,
    pub error_message: Option<String>,
    pub duration_ms: u64,
}

/// case 状态
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CaseStatus {
    Passed,
    Failed,
    Skipped,
}

/// 测试异常
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct TestAnomaly {
    pub anomaly_type: String,
    pub description: String,
    pub fact_id: Option<u64>,
    pub severity: String,
}

/// 审计链信息
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AuditInfo {
    pub audit_chain_length: usize,
    pub audit_chain_verified: bool,
    pub audit_export_path: Option<String>,
}

/// 按 Command 事实边界归组。
///
/// 每条 `Command` 事实开启一个新组，其后随附事实（StateTransition/
/// TransitionTrace/IoResponse/Error/Violation/Stable 等）归属当前组；
/// 首条 Command 之前的前导散事实单独成组（command=None，按启动噪声处置）。
/// 返回 (组首 Command 事实, 随附事实列表)，按链序排列。
fn group_facts_by_command(
    facts: &[serde_json::Value],
) -> Vec<(Option<&serde_json::Value>, Vec<&serde_json::Value>)> {
    let mut groups: Vec<(Option<&serde_json::Value>, Vec<&serde_json::Value>)> = Vec::new();
    for fact in facts {
        if fact.get("type").and_then(|v| v.as_str()) == Some("Command") {
            groups.push((Some(fact), Vec::new()));
        } else if let Some(last) = groups.last_mut() {
            last.1.push(fact);
        } else {
            groups.push((None, vec![fact]));
        }
    }
    groups
}

/// 用例组识别：仅宪法锚定的沙盒用例指令形态计为测试用例（[CON-X-003]：
/// `is_sandbox_case_instruction` 读 `_shared` `sandbox_case_instruction` $def，
/// eval_* 家族）；规则装载（transform）/未知指令类型/缺失 instruction 归启动
/// 噪声组（白名单口径：未知形态宁可降为噪声留痕，不冒充用例）。
/// 历史注：2026-10-14 前锚废弃字面量 `type=="command"`（注入器改「用例即指令
/// 本体」后该形态从未出现——现网用例全部误归噪声的活 bug，本批修复）。
fn is_case_group(command: Option<&serde_json::Value>) -> bool {
    command
        .and_then(|c| c.get("instruction"))
        .map(evorule_rule_schema::is_sandbox_case_instruction)
        .unwrap_or(false)
}

/// 事实级失败判定：返回失败说明（None = 非失败事实）。
///
/// 判定面（Failed 判定扩面整改）：
/// - `Error`（TCB 错误/Ignored/max_rounds/队列溢出/超时）→ message；
/// - `Violation`（enforce 强制拦截）→ reason；
/// - 携带非空 error 的 `IoResponse`（下游 I/O 失败）→ error 文本。
///   其余事实（Command/StateTransition/Stable/TransitionTrace 等）不构成失败。
fn fact_failure_message(fact: &serde_json::Value) -> Option<String> {
    match fact.get("type").and_then(|v| v.as_str()) {
        Some("Error") => Some(
            fact.get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("TCB error")
                .to_string(),
        ),
        Some("Violation") => Some(
            fact.get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("enforce violation")
                .to_string(),
        ),
        Some("IoResponse")
            if !fact
                .get("error")
                .map(serde_json::Value::is_null)
                .unwrap_or(true) =>
        {
            Some(
                fact.get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("I/O error response")
                    .to_string(),
            )
        }
        _ => None,
    }
}

/// 测试报告构建器
pub struct TestReportBuilder {
    sandbox_id: Option<String>,
    workspace_id: Option<String>,
    tcb_session_id: Option<u64>,
    parent_session_id: Option<u64>,
    draft_ruleset_hash: Option<String>,
    state: Option<serde_json::Value>,
    audit: Option<serde_json::Value>,
    facts: Option<Vec<serde_json::Value>>,
    /// 测试 case 显式名（按**用例组序**对应，非原始事实序）。
    /// 来源 = test_dataset cases_json 各元素的 `name` 字段（可选）；
    /// 缺失/为空/越界时报告回退默认命名 "Case #N"。
    /// 对应假设: 第 i 个用例组（instruction.type=="command" 的 Command 组）
    /// ↔ cases_json[i]（用例按序注入、按序成组）；错位只影响显示名，
    /// 不影响 status 判定（判定基于组内事实类型）。
    case_names: Option<Vec<String>>,
}

impl Default for TestReportBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl TestReportBuilder {
    pub fn new() -> Self {
        Self {
            sandbox_id: None,
            workspace_id: None,
            tcb_session_id: None,
            parent_session_id: None,
            draft_ruleset_hash: None,
            state: None,
            audit: None,
            facts: None,
            case_names: None,
        }
    }

    pub fn sandbox_id(mut self, id: impl Into<String>) -> Self {
        self.sandbox_id = Some(id.into());
        self
    }

    pub fn workspace_id(mut self, id: impl Into<String>) -> Self {
        self.workspace_id = Some(id.into());
        self
    }

    pub fn tcb_session_id(mut self, id: u64) -> Self {
        self.tcb_session_id = Some(id);
        self
    }

    pub fn parent_session_id(mut self, id: Option<u64>) -> Self {
        self.parent_session_id = id;
        self
    }

    pub fn draft_ruleset_hash(mut self, hash: impl Into<String>) -> Self {
        self.draft_ruleset_hash = Some(hash.into());
        self
    }

    pub fn state(mut self, state: serde_json::Value) -> Self {
        self.state = Some(state);
        self
    }

    pub fn audit(mut self, audit: serde_json::Value) -> Self {
        self.audit = Some(audit);
        self
    }

    pub fn facts(mut self, facts: Vec<serde_json::Value>) -> Self {
        self.facts = Some(facts);
        self
    }

    /// 注入 case 显式名（按序对应 facts）
    pub fn case_names(mut self, names: Vec<String>) -> Self {
        self.case_names = Some(names);
        self
    }

    /// 构建测试报告 (计算统计 + BLAKE3 签名)
    ///
    /// 判定语义（沙盒报告 P0 修复：判定归真）：
    /// - **按 Command 事实边界归组**：一条用例指令在事实链上展开为多条事实
    ///   （Command + StateTransition/TransitionTrace/IoResponse/Error...），
    ///   1:1 转写会让 total_cases=事实条数且全部错位；
    /// - **用例组白名单**：仅 `instruction.type == "command"`（沙盒用例注入
    ///   包装形态，见 sandbox_service.inject_test_cases）的组计为用例；
    ///   规则装载（transform）/未知指令类型/前导散事实归启动噪声组，
    ///   不计入用例统计；
    /// - **Failed 扩面**：组内任一 Error/Violation/带 error 的 IoResponse 即
    ///   Failed（此前仅 Error/Exception 字面判定，error IoResponse 与
    ///   Violation 恒漏判，叠加 facts 形态错配实际恒 Passed）；
    /// - **启动噪声不静默**：噪声组中的失败事实转写为 TestAnomaly(warning) 留痕；
    /// - duration_ms/skipped 恒 0：真实执行度量属真用例执行模型专项
    ///   （候选改进项，未实施）。
    pub fn build(self) -> TestReport {
        let facts = self.facts.unwrap_or_default();
        let fact_count = facts.len();

        let case_names = self.case_names.unwrap_or_default();

        let mut anomalies: Vec<TestAnomaly> = Vec::new();
        let mut cases: Vec<TestCaseResult> = Vec::new();

        for (command, attached) in group_facts_by_command(&facts) {
            // 组内失败事实: (事实引用, 失败说明)
            let failures: Vec<(&serde_json::Value, String)> = attached
                .into_iter()
                .chain(command)
                .filter_map(|f| fact_failure_message(f).map(|m| (f, m)))
                .collect();

            if !is_case_group(command) {
                // 启动噪声组：失败事实大声留痕（不静默），不计入用例统计
                for (fact, message) in failures {
                    anomalies.push(TestAnomaly {
                        anomaly_type: "startup_noise".to_string(),
                        description: message,
                        fact_id: fact.get("id").and_then(|v| v.as_u64()),
                        severity: "warning".to_string(),
                    });
                }
                continue;
            }

            let case_idx = cases.len();
            let status = if failures.is_empty() {
                CaseStatus::Passed
            } else {
                CaseStatus::Failed
            };
            // case 显式名优先(dataset cases_json 的 name 字段，按用例序对齐),
            // 缺失/空/越界回退默认命名
            let case_name = case_names
                .get(case_idx)
                .filter(|n| !n.trim().is_empty())
                .cloned()
                .unwrap_or_else(|| format!("Case #{}", case_idx + 1));

            cases.push(TestCaseResult {
                case_id: format!("case-{}", case_idx + 1),
                case_name,
                status,
                fact_id: command.and_then(|c| c.get("id")).and_then(|v| v.as_u64()),
                error_message: failures.first().map(|(_, m)| m.clone()),
                duration_ms: 0,
            });
        }

        let passed = cases
            .iter()
            .filter(|c| c.status == CaseStatus::Passed)
            .count();
        let failed = cases
            .iter()
            .filter(|c| c.status == CaseStatus::Failed)
            .count();
        let total = cases.len();
        let pass_rate = if total > 0 {
            passed as f64 / total as f64
        } else {
            0.0
        };

        let audit = self.audit.unwrap_or_default();
        // 字段名与 evorule-governance auditor.report() 对齐:
        // auditor 返回 "entry_count" (非 "chain_length"), 详见 auditor.rs:479
        let audit_chain_length = audit
            .get("entry_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let audit_chain_verified = audit
            .get("verified")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let summary = TestSummary {
            total_cases: total,
            passed,
            failed,
            skipped: 0,
            pass_rate,
            total_duration_ms: 0,
            fact_count,
        };

        let mut report = TestReport {
            sandbox_id: self.sandbox_id.unwrap_or_default(),
            workspace_id: self.workspace_id.unwrap_or_default(),
            tcb_session_id: self.tcb_session_id.unwrap_or(0),
            parent_session_id: self.parent_session_id,
            draft_ruleset_hash: self.draft_ruleset_hash.unwrap_or_default(),
            summary,
            cases,
            anomalies,
            audit_info: AuditInfo {
                audit_chain_length,
                audit_chain_verified,
                audit_export_path: None,
            },
            report_hash: String::new(),
            generated_at: chrono::Utc::now().to_rfc3339(),
        };

        // BLAKE3 签名 (防篡改) — 审计⑥ C3: 哈希实现统一走 evorule-hash (输入字节不变)
        let report_json = serde_json::to_string(&report).unwrap_or_default();
        report.report_hash = evorule_hash::digest(report_json.as_bytes());

        report
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// 构造沙盒用例注入形态的 Command 事实
    /// (instruction.type == "eval_*"，宪法 sandbox_case_instruction 锚，
    ///  见 sandbox_service.inject_test_cases 与 [CON-X-003])
    fn case_command(id: u64) -> serde_json::Value {
        serde_json::json!({
            "type": "Command",
            "id": id,
            "instruction": {"type": "eval_hello", "payload": {"k": "v"}},
        })
    }

    /// 构造规则装载形态的 Command 事实
    /// (instruction.type == "transform", 见 sandbox_service.load_rules_into_sandbox)
    fn transform_command(id: u64) -> serde_json::Value {
        serde_json::json!({
            "type": "Command",
            "id": id,
            "instruction": {"type": "transform", "payload": {}},
        })
    }

    /// 废弃形态回归锁：旧 "command" 字面量（历史白名单口径）在宪法锚下必须
    /// 归噪声组——防锚回退（[CON-X-003] 判据：双写扫描零命中 + 形态演进有锁）
    #[test]
    fn legacy_command_literal_is_noise_not_case() {
        let legacy = serde_json::json!({
            "type": "Command",
            "id": 1,
            "instruction": {"type": "command", "payload": {}},
        });
        assert!(!is_case_group(Some(&legacy)));
    }

    #[test]
    fn test_report_generation() {
        // 生产实况形态: 1 组规则装载(transform) + 2 条用例
        // (各展开为 Command + 随附事实); transform 组因宪法无对应规则产生 Error 噪声
        let facts = vec![
            transform_command(1),
            serde_json::json!({"id": 2, "type": "Error", "message": "no rule matched"}),
            case_command(3),
            serde_json::json!({"id": 4, "type": "StateTransition"}),
            case_command(5),
            serde_json::json!({"id": 6, "type": "IoResponse", "result": "ok"}),
        ];

        let report = TestReportBuilder::new()
            .sandbox_id("sbx-1")
            .workspace_id("ws-test")
            .tcb_session_id(100)
            .draft_ruleset_hash("abc123")
            .state(serde_json::json!({}))
            .audit(serde_json::json!({"entry_count": 6, "verified": true}))
            .facts(facts)
            .build();

        // 仅 command 组计为用例 (transform 噪声组不计入): total=2 非 6
        assert_eq!(report.summary.total_cases, 2);
        assert_eq!(report.summary.passed, 2);
        assert_eq!(report.summary.failed, 0);
        assert!((report.summary.pass_rate - 1.0).abs() < 1e-9);
        assert_eq!(report.summary.fact_count, 6);
        assert!(!report.report_hash.is_empty());
        assert_eq!(report.audit_info.audit_chain_length, 6);
        assert!(report.audit_info.audit_chain_verified);
        // 启动噪声不静默: transform 组的 Error → startup_noise anomaly 留痕
        assert_eq!(report.anomalies.len(), 1);
        assert_eq!(report.anomalies[0].anomaly_type, "startup_noise");
        assert_eq!(report.anomalies[0].severity, "warning");
        assert_eq!(report.anomalies[0].fact_id, Some(2));
        assert_eq!(report.anomalies[0].description, "no rule matched");
        // fact_id 取各组 Command 的 id
        assert_eq!(report.cases[0].fact_id, Some(3));
        assert_eq!(report.cases[1].fact_id, Some(5));
    }

    #[test]
    fn test_case_failure_error_fact() {
        // 用例组内附 Error 事实 → Failed, error_message 取 message
        let facts = vec![
            case_command(1),
            serde_json::json!({"id": 2, "type": "Error", "message": "rule failed"}),
        ];
        let report = TestReportBuilder::new().facts(facts).build();

        assert_eq!(report.summary.total_cases, 1);
        assert_eq!(report.summary.passed, 0);
        assert_eq!(report.summary.failed, 1);
        assert_eq!(report.cases[0].status, CaseStatus::Failed);
        assert_eq!(
            report.cases[0].error_message.as_deref(),
            Some("rule failed")
        );
        // 组内失败计入用例判定, 不产生 anomaly (anomaly 只记噪声组)
        assert!(report.anomalies.is_empty());
    }

    #[test]
    fn test_case_failure_error_io_response() {
        // Failed 扩面: 携带非空 error 的 IoResponse → Failed
        let facts = vec![
            case_command(1),
            serde_json::json!({"id": 2, "type": "IoResponse", "error": "timeout"}),
        ];
        let report = TestReportBuilder::new().facts(facts).build();

        assert_eq!(report.summary.failed, 1);
        assert_eq!(report.cases[0].status, CaseStatus::Failed);
        assert_eq!(report.cases[0].error_message.as_deref(), Some("timeout"));
    }

    #[test]
    fn test_case_failure_violation() {
        // Failed 扩面: Violation (enforce 拦截) → Failed
        let facts = vec![
            case_command(1),
            serde_json::json!({"id": 2, "type": "Violation", "reason": "budget exceeded"}),
        ];
        let report = TestReportBuilder::new().facts(facts).build();

        assert_eq!(report.summary.failed, 1);
        assert_eq!(report.cases[0].status, CaseStatus::Failed);
        assert_eq!(
            report.cases[0].error_message.as_deref(),
            Some("budget exceeded")
        );
    }

    #[test]
    fn test_leading_loose_facts_are_noise() {
        // 首条 Command 之前的前导散事实 → 噪声组, 不冒充用例
        let facts = vec![
            serde_json::json!({"id": 1, "type": "StateTransition"}),
            serde_json::json!({"id": 2, "type": "Error", "message": "orphan"}),
            case_command(3),
        ];
        let report = TestReportBuilder::new().facts(facts).build();

        assert_eq!(report.summary.total_cases, 1);
        assert_eq!(report.summary.passed, 1);
        assert_eq!(report.anomalies.len(), 1);
        assert_eq!(report.anomalies[0].fact_id, Some(2));
        assert_eq!(report.anomalies[0].description, "orphan");
    }

    #[test]
    fn test_empty_facts() {
        // 空事实链 (0 用例沙盒): total=0/failed=0, 不再假阴性, 无 anomaly
        let report = TestReportBuilder::new()
            .sandbox_id("sbx-empty")
            .workspace_id("ws-empty")
            .tcb_session_id(1)
            .build();

        assert_eq!(report.summary.total_cases, 0);
        assert_eq!(report.summary.pass_rate, 0.0);
        assert!(report.cases.is_empty());
        assert!(report.anomalies.is_empty());
        assert!(!report.report_hash.is_empty());
    }

    /// case 显式名按**用例组序**对齐 (非原始事实序);
    /// 缺名/空名/越界回退默认命名 "Case #N", 判定语义不变。
    #[test]
    fn test_case_names_preferred_with_fallback() {
        let facts = vec![
            case_command(1),
            serde_json::json!({"id": 2, "type": "StateTransition"}),
            case_command(3),
            case_command(4),
            serde_json::json!({"id": 5, "type": "Error", "message": "boom"}),
        ];

        let report = TestReportBuilder::new()
            .facts(facts)
            .case_names(vec![
                "登录超时校验".to_string(),
                "   ".to_string(), // 空白名 → 回退默认
            ]) // 第 3 个用例组越界 → 回退默认
            .build();

        assert_eq!(report.cases[0].case_name, "登录超时校验");
        assert_eq!(report.cases[1].case_name, "Case #2");
        assert_eq!(report.cases[2].case_name, "Case #3");
        // 判定: 前两用例 passed, 第 3 用例组附 Error → failed
        assert_eq!(report.summary.passed, 2);
        assert_eq!(report.summary.failed, 1);
        assert_eq!(report.cases[2].error_message.as_deref(), Some("boom"));
    }
}

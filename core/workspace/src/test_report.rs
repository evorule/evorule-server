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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestCaseResult {
    pub case_id: String,
    pub case_name: String,
    pub status: CaseStatus,
    pub fact_id: Option<u64>,
    pub error_message: Option<String>,
    pub duration_ms: u64,
}

/// case 状态
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CaseStatus {
    Passed,
    Failed,
    Skipped,
}

/// 测试异常
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestAnomaly {
    pub anomaly_type: String,
    pub description: String,
    pub fact_id: Option<u64>,
    pub severity: String,
}

/// 审计链信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditInfo {
    pub audit_chain_length: usize,
    pub audit_chain_verified: bool,
    pub audit_export_path: Option<String>,
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

    /// 构建测试报告 (计算统计 + BLAKE3 签名)
    ///
    /// P0 简化判定: Error/Exception 类型 Fact = failed, 其余 = passed
    /// P1 增强: 按 case_id 分组, 聚合 pass/fail
    pub fn build(self) -> TestReport {
        let facts = self.facts.unwrap_or_default();
        let fact_count = facts.len();

        let cases: Vec<TestCaseResult> = facts
            .iter()
            .enumerate()
            .map(|(i, fact)| {
                let fact_id = fact.get("id").and_then(|v| v.as_u64());
                let fact_type = fact
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");

                let status = if fact_type == "Error" || fact_type == "Exception" {
                    CaseStatus::Failed
                } else {
                    CaseStatus::Passed
                };

                TestCaseResult {
                    case_id: format!("case-{}", i + 1),
                    case_name: format!("Fact #{} ({})", fact_id.unwrap_or(i as u64), fact_type),
                    status,
                    fact_id,
                    error_message: if status == CaseStatus::Failed {
                        Some(fact.to_string())
                    } else {
                        None
                    },
                    duration_ms: 0,
                }
            })
            .collect();

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
            anomalies: Vec::new(),
            audit_info: AuditInfo {
                audit_chain_length,
                audit_chain_verified,
                audit_export_path: None,
            },
            report_hash: String::new(),
            generated_at: chrono::Utc::now().to_rfc3339(),
        };

        // BLAKE3 签名 (防篡改)
        let report_json = serde_json::to_string(&report).unwrap_or_default();
        let hash = blake3::hash(report_json.as_bytes());
        report.report_hash = hash.to_hex().to_string();

        report
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn test_report_generation() {
        let facts = vec![
            serde_json::json!({"id": 1, "type": "Command"}),
            serde_json::json!({"id": 2, "type": "StateTransition"}),
            serde_json::json!({"id": 3, "type": "Error", "message": "rule failed"}),
        ];

        let report = TestReportBuilder::new()
            .sandbox_id("sbx-1")
            .workspace_id("ws-test")
            .tcb_session_id(100)
            .draft_ruleset_hash("abc123")
            .state(serde_json::json!({}))
            .audit(serde_json::json!({"entry_count": 3, "verified": true}))
            .facts(facts)
            .build();

        assert_eq!(report.summary.total_cases, 3);
        assert_eq!(report.summary.passed, 2);
        assert_eq!(report.summary.failed, 1);
        assert!((report.summary.pass_rate - 0.667).abs() < 0.01);
        assert!(!report.report_hash.is_empty());
        assert_eq!(report.audit_info.audit_chain_length, 3);
        assert!(report.audit_info.audit_chain_verified);
    }

    #[test]
    fn test_empty_facts() {
        let report = TestReportBuilder::new()
            .sandbox_id("sbx-empty")
            .workspace_id("ws-empty")
            .tcb_session_id(1)
            .build();

        assert_eq!(report.summary.total_cases, 0);
        assert_eq!(report.summary.pass_rate, 0.0);
        assert!(!report.report_hash.is_empty());
    }
}

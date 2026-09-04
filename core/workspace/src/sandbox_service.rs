// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 沙盒编排服务 — 三层架构 Layer 3 (Sandbox Sessions)
//!
//! 设计依据: SANDBOX_ORCHESTRATION_DESIGN.md §3 (S1)
//!
//! # 职责
//! 组合 evorule-server 已有的 fork API + 合成数据加载 + 测试报告生成,
//! 形成"一键启动沙盒测试"的完整流程:
//!
//! 1. 校验 workspace 成员权限
//! 2. 计算 Draft 规则集 BLAKE3 哈希
//! 3. Fork Production session → 新 sandbox session
//! 4. 记录 sandbox_sessions 表
//! 5. 逐条加载 Draft 规则到 sandbox (send_command transform)
//! 6. 逐条注入合成测试数据 (send_command)
//! 7. 启动 MockIoResponder (自动回调 io_request)
//! 8. 关闭时导出 test Fact + 生成测试报告
//!
//! # 关键决策
//! - S1: 编排模块位于 workspace crate (依赖 workspace 表)
//! - S2: MockIoResponder 全合成数据 (P0, Q2 决策)
//! - S4: test Fact 通过 audit/export 导出,不另建表

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::db::WorkspaceDb;
use crate::error::{WorkspaceError, WorkspaceResult};
use crate::mock_io_responder::MockIoResponder;
use crate::models::{
    RuleVersionRecord, SandboxSession, SandboxStatus, StartSandboxRequest, StartSandboxResponse,
    TestDatasetRecord,
};
use crate::session_bridge::SessionOps;
use crate::test_report::{TestReport, TestReportBuilder};

/// 沙盒测试报告导出目录
const SANDBOX_REPORT_DIR: &str = "./data/sandbox_reports";

/// 沙盒编排服务
///
/// 持有 `Arc<WorkspaceDb>` + `Arc<dyn SessionOps>`。
/// 通过 `Arc<Mutex<HashMap>>` 管理多个并行沙盒的 MockIoResponder 生命周期。
pub struct SandboxService {
    db: Arc<WorkspaceDb>,
    session_ops: Arc<dyn SessionOps>,
    /// 活跃沙盒的 MockIoResponder 句柄 (tcb_session_id → responder)
    mock_responders: Arc<Mutex<HashMap<u64, MockIoResponder>>>,
}

impl SandboxService {
    /// 创建新服务实例
    pub fn new(db: Arc<WorkspaceDb>, session_ops: Arc<dyn SessionOps>) -> Self {
        Self {
            db,
            session_ops,
            mock_responders: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 启动沙盒测试 (完整编排流程)
    ///
    /// 流程 (SANDBOX_ORCHESTRATION_DESIGN.md §3.1):
    /// 1. 校验 Workspace 成员权限
    /// 2. 查询规则版本 + 校验所属 workspace
    /// 3. 计算 Draft 规则集 BLAKE3 哈希
    /// 4. 获取当前 Production session_id (从 production_state 表)
    /// 5. Fork Production session → 新 sandbox session
    /// 6. 记录 sandbox_sessions 表
    /// 7. 逐条加载 Draft 规则到 sandbox (send_command transform)
    /// 8. 逐条注入合成测试数据 (send_command)
    /// 9. 启动 MockIoResponder
    pub async fn start_sandbox(
        &self,
        workspace_id: &str,
        req: StartSandboxRequest,
        started_by: &str,
    ) -> WorkspaceResult<StartSandboxResponse> {
        // 1. 校验成员权限
        self.validate_member(workspace_id, started_by)?;

        // 2. 查询规则版本 + 校验所属 workspace + 计算 Draft 规则集 BLAKE3 哈希
        let rule_versions = self.validate_rule_versions(workspace_id, &req)?;
        let draft_hash = compute_ruleset_hash(&rule_versions);

        // 4. 获取 Production session_id + 5. Fork → 新 sandbox session
        let parent_session_id = self.get_production_parent_session()?;
        let tcb_session_id = self.session_ops.fork_session(parent_session_id).await?;
        info!(
            sandbox_session_id = tcb_session_id,
            parent_session_id = parent_session_id,
            workspace_id = %workspace_id,
            "Sandbox session forked from production"
        );

        // 6. 记录 sandbox_sessions 表
        let sandbox_id = self.db.insert_sandbox_session(
            Some(tcb_session_id as i64),
            workspace_id,
            parent_session_id as i64,
            Some(&draft_hash),
            req.test_dataset_id,
            started_by,
        )?;

        // 7. 逐条加载 Draft 规则到 sandbox (send_command transform)
        self.load_rules_into_sandbox(tcb_session_id, &rule_versions)
            .await?;

        // 8. 注入合成测试数据
        let test_cases = self
            .inject_test_cases(tcb_session_id, workspace_id, &req)
            .await?;

        // 9. 启动 MockIoResponder
        let responder = MockIoResponder::new(tcb_session_id);
        responder.start().await;
        self.mock_responders
            .lock()
            .await
            .insert(tcb_session_id, responder);

        // 缺口3: 记录 sandbox_started 生命周期节点到 production_audit
        // 沙盒从生产 session fork, 记录启动时的生产版本上下文, 便于追溯
        // "哪个生产版本被用来做沙盒测试"。
        self.record_sandbox_started(workspace_id, started_by, &draft_hash, tcb_session_id)?;

        Ok(StartSandboxResponse {
            sandbox_id,
            tcb_session_id,
            draft_ruleset_hash: draft_hash,
            test_case_count: test_cases.len(),
        })
    }

    /// 校验成员权限
    fn validate_member(&self, workspace_id: &str, started_by: &str) -> WorkspaceResult<()> {
        if !self.db.is_workspace_member(workspace_id, started_by)? {
            return Err(WorkspaceError::forbidden(format!(
                "user {started_by} is not a member of workspace {workspace_id}"
            )));
        }
        Ok(())
    }

    /// 查询规则版本 + 校验非空 + 校验每个规则版本所属 workspace
    fn validate_rule_versions(
        &self,
        workspace_id: &str,
        req: &StartSandboxRequest,
    ) -> WorkspaceResult<Vec<RuleVersionRecord>> {
        if req.rule_version_ids.is_empty() {
            return Err(WorkspaceError::invalid_input(
                "rule_version_ids must not be empty",
            ));
        }

        let rule_versions = self.db.get_rule_versions_by_ids(&req.rule_version_ids)?;
        if rule_versions.len() != req.rule_version_ids.len() {
            let found: Vec<&str> = rule_versions.iter().map(|rv| rv.id.as_str()).collect();
            return Err(WorkspaceError::not_found(
                "rule_version",
                format!(
                    "requested {} but found {} (ids: {:?})",
                    req.rule_version_ids.len(),
                    rule_versions.len(),
                    found
                ),
            ));
        }

        for rv in &rule_versions {
            let rule = self.db.get_rule(&rv.rule_id)?;
            if rule.workspace_id != workspace_id {
                return Err(WorkspaceError::forbidden(format!(
                    "rule {} (version {}) does not belong to workspace {}",
                    rule.id, rv.id, workspace_id
                )));
            }
        }
        Ok(rule_versions)
    }

    /// 获取当前 production session_id (作为 sandbox 的 parent)
    fn get_production_parent_session(&self) -> WorkspaceResult<u64> {
        self.db
            .get_production_state()?
            .current_session_id
            .ok_or_else(|| {
                WorkspaceError::internal("production session not initialized (cannot fork sandbox)")
            })
            .map(|id| id as u64)
    }

    /// 逐条加载 Draft 规则到 sandbox (send_command transform), 失败时关闭 session 并清理
    async fn load_rules_into_sandbox(
        &self,
        tcb_session_id: u64,
        rule_versions: &[RuleVersionRecord],
    ) -> WorkspaceResult<()> {
        for rv in rule_versions {
            let rule_content: Value = serde_json::from_str(&rv.content).map_err(|e| {
                WorkspaceError::internal(format!(
                    "rule_version {} content is not valid JSON: {e}",
                    rv.id
                ))
            })?;
            let instruction = serde_json::json!({
                "type": "transform",
                "payload": rule_content,
            });
            if let Err(e) = self
                .session_ops
                .send_command(tcb_session_id, instruction)
                .await
            {
                // 加载失败,关闭已 fork 的 session 并清理 db 记录
                warn!(
                    error = %e,
                    rule_version_id = %rv.id,
                    "Failed to load rule into sandbox, cleaning up"
                );
                let _ = self.session_ops.close_session(tcb_session_id).await;
                return Err(e);
            }
        }
        info!(
            count = rule_versions.len(),
            sandbox_session_id = tcb_session_id,
            "Draft rules loaded into sandbox"
        );
        Ok(())
    }

    /// 逐条注入合成测试数据, 返回解析后的 test_cases
    async fn inject_test_cases(
        &self,
        tcb_session_id: u64,
        workspace_id: &str,
        req: &StartSandboxRequest,
    ) -> WorkspaceResult<Vec<Value>> {
        let dataset = self
            .db
            .get_test_dataset(req.test_dataset_id)?
            .ok_or_else(|| {
                WorkspaceError::not_found("test_dataset", req.test_dataset_id.to_string())
            })?;

        // 校验数据集归属 (共享数据集 workspace_id IS NULL 或属于该 workspace)
        if let Some(ds_ws) = &dataset.workspace_id {
            if ds_ws != workspace_id {
                return Err(WorkspaceError::forbidden(format!(
                    "test_dataset {} does not belong to workspace {}",
                    dataset.id, workspace_id
                )));
            }
        }

        let test_cases: Vec<Value> = serde_json::from_str(&dataset.cases_json).map_err(|e| {
            WorkspaceError::internal(format!(
                "test_dataset cases_json is not valid JSON array: {e}"
            ))
        })?;

        for case in &test_cases {
            let instruction = serde_json::json!({
                "type": "command",
                "payload": case,
            });
            self.session_ops
                .send_command(tcb_session_id, instruction)
                .await?;
        }
        info!(
            count = test_cases.len(),
            sandbox_session_id = tcb_session_id,
            "Test cases injected into sandbox"
        );
        Ok(test_cases)
    }

    /// 缺口3: 记录 sandbox_started 生命周期节点到 production_audit
    fn record_sandbox_started(
        &self,
        workspace_id: &str,
        started_by: &str,
        draft_hash: &str,
        tcb_session_id: u64,
    ) -> WorkspaceResult<()> {
        let prod_state = self.db.get_production_state()?;
        let source_ws_ids = serde_json::json!([workspace_id]).to_string();
        self.db.insert_production_audit(
            "sandbox_started",
            prod_state.ruleset_version,
            None,
            draft_hash,
            tcb_session_id as i64,
            &source_ws_ids,
            started_by,
            None,
            None,
            None,
        )?;
        Ok(())
    }

    /// 关闭沙盒 (导出 test Fact + 更新状态 + 关闭 session)
    ///
    /// 流程:
    /// 1. 查询沙盒记录,校验状态为 Running
    /// 2. 校验成员权限
    /// 3. 停止 MockIoResponder
    /// 4. 导出 test Fact (audit/export → JSON 文件)
    /// 5. 关闭 session
    /// 6. 更新 sandbox_sessions 表 (status=closed)
    pub async fn close_sandbox(&self, sandbox_id: i64, closed_by: &str) -> WorkspaceResult<String> {
        let sandbox = self
            .db
            .get_sandbox_session(sandbox_id)?
            .ok_or_else(|| WorkspaceError::not_found("sandbox", sandbox_id.to_string()))?;

        if sandbox.status != SandboxStatus::Running {
            return Err(WorkspaceError::InvalidStateTransition {
                from: sandbox.status.as_str().to_string(),
                to: SandboxStatus::Closed.as_str().to_string(),
            });
        }

        // 校验成员权限
        if !self
            .db
            .is_workspace_member(&sandbox.workspace_id, closed_by)?
        {
            return Err(WorkspaceError::forbidden(format!(
                "user {closed_by} is not a member of workspace {}",
                sandbox.workspace_id
            )));
        }

        let tcb_session_id = sandbox.tcb_session_id.unwrap_or(0) as u64;

        // 停止 MockIoResponder
        if let Some(responder) = self.mock_responders.lock().await.remove(&tcb_session_id) {
            responder.stop().await;
        }

        // 导出 test Fact (通过 audit/export API, 存为 JSON 文件)
        let export_path = format!(
            "{}/sandbox_{}_{}.json",
            SANDBOX_REPORT_DIR,
            sandbox_id,
            chrono::Utc::now().timestamp()
        );
        let audit_data = self.session_ops.get_audit_export(tcb_session_id).await?;
        std::fs::create_dir_all(SANDBOX_REPORT_DIR).map_err(|e| {
            WorkspaceError::internal(format!("create sandbox_report dir failed: {e}"))
        })?;
        std::fs::write(&export_path, &audit_data)
            .map_err(|e| WorkspaceError::internal(format!("write sandbox export failed: {e}")))?;

        // UV-072: 关闭前(session 仍活)生成完整 TestReport 并落盘。
        // 此前仅导出 fact 链文件,summary 报告未持久化 → 关闭后
        // generate_test_report 实时取数 404 "session not found",
        // 机器证据回填与"查看报告"功能全断。
        // 报告文件与 facts 文件同目录同时间戳配对:report_sandbox_{id}_{ts}.json
        // (generate_test_report 关闭态按 export_path 推导本路径读取)。
        let report_path = format!("report_{}", export_path.rsplit('/').next().unwrap_or_default());
        let report_path = format!("{}/{}", SANDBOX_REPORT_DIR, report_path);
        {
            let state_val = self.session_ops.get_session_state(tcb_session_id).await?;
            let audit_val = self.session_ops.get_audit_report(tcb_session_id).await?;
            let facts_val = self.session_ops.get_facts(tcb_session_id).await?;
            let report = TestReportBuilder::new()
                .sandbox_id(sandbox_id.to_string())
                .workspace_id(sandbox.workspace_id.clone())
                .tcb_session_id(tcb_session_id)
                .parent_session_id(Some(sandbox.parent_session_id as u64))
                .draft_ruleset_hash(sandbox.draft_ruleset_hash.clone().unwrap_or_default())
                .state(state_val)
                .audit(audit_val)
                .facts(facts_val)
                .build();
            let report_json = serde_json::to_string_pretty(&report).map_err(|e| {
                WorkspaceError::internal(format!("serialize test report failed: {e}"))
            })?;
            std::fs::write(&report_path, report_json).map_err(|e| {
                WorkspaceError::internal(format!(
                    "write sandbox test report failed: {e} (path: {report_path})"
                ))
            })?;
            info!(
                sandbox_id = sandbox_id,
                report_path = %report_path,
                verdict_failed = report.summary.failed,
                "UV-072: sandbox test report persisted before close"
            );
        }

        // 关闭 session (尽力清理,失败仅告警)
        if let Err(e) = self.session_ops.close_session(tcb_session_id).await {
            warn!(
                error = %e,
                sandbox_id = sandbox_id,
                tcb_session_id = tcb_session_id,
                "Failed to close sandbox session (marking closed in db anyway)"
            );
        }

        // 更新 sandbox_sessions 表
        self.db.close_sandbox_session(sandbox_id, &export_path)?;

        // 缺口3: 记录 sandbox_closed 生命周期节点 (含测试报告路径)
        // 关键: test_report_paths 填入 export_path, 使生产审计可追溯到沙盒测试报告。
        // 与缺口4 (publish 时关联 test_report_paths) 形成完整闭环。
        let prod_state = self.db.get_production_state()?;
        let source_ws_ids = serde_json::json!([sandbox.workspace_id]).to_string();
        self.db.insert_production_audit(
            "sandbox_closed",
            prod_state.ruleset_version,
            None,
            sandbox.draft_ruleset_hash.as_deref().unwrap_or(""),
            tcb_session_id as i64,
            &source_ws_ids,
            closed_by,
            None,
            Some(&export_path),
            None,
        )?;

        info!(
            sandbox_id = sandbox_id,
            tcb_session_id = tcb_session_id,
            export_path = %export_path,
            "Sandbox closed and test facts exported"
        );

        Ok(export_path)
    }

    /// 生成测试报告 (从 sandbox session 的 audit + state + facts 聚合)
    ///
    /// 报告包含 BLAKE3 签名 (防篡改),可附带在发布队列项中供审批者查阅。
    ///
    /// UV-072: running 沙盒实时聚合(现状);closed 沙盒从 close 时持久化的
    /// 报告文件读取(与 facts 导出同目录同时间戳配对:report_sandbox_{id}_{ts}.json,
    /// 按 sandbox.export_path 推导)。文件缺失时显式报错含自诊断指引,
    /// 不静默不伪造。
    pub async fn generate_test_report(&self, sandbox_id: i64) -> WorkspaceResult<TestReport> {
        let sandbox = self
            .db
            .get_sandbox_session(sandbox_id)?
            .ok_or_else(|| WorkspaceError::not_found("sandbox", sandbox_id.to_string()))?;

        if sandbox.status == SandboxStatus::Closed {
            let export_path = sandbox.export_path.as_deref().ok_or_else(|| {
                WorkspaceError::not_found(
                    "sandbox report (closed without export_path — 数据异常:关闭时未导出,\
                     无法回溯报告;请重跑沙盒测试)",
                    sandbox_id.to_string(),
                )
            })?;
            let file_name = export_path.rsplit('/').next().unwrap_or_default();
            let report_path = format!("{}/report_{}", SANDBOX_REPORT_DIR, file_name);
            let content = std::fs::read_to_string(&report_path).map_err(|_| {
                WorkspaceError::not_found(
                    "sandbox report file",
                    format!(
                        "{report_path} (沙盒已关闭且报告文件缺失:可能被清理或属 UV-072 \
                         修复前关闭的历史沙盒;请重跑沙盒测试以生成报告)"
                    ),
                )
            })?;
            let report: TestReport = serde_json::from_str(&content).map_err(|e| {
                WorkspaceError::internal(format!(
                    "sandbox report file corrupted: {e} (path: {report_path})"
                ))
            })?;
            return Ok(report);
        }

        let tcb_session_id = sandbox.tcb_session_id.unwrap_or(0) as u64;

        // 获取 session 状态快照
        let state = self.session_ops.get_session_state(tcb_session_id).await;

        // 获取审计报告 (含 BLAKE3 链验证)
        let audit = self.session_ops.get_audit_report(tcb_session_id).await;

        // 获取 Fact 列表 (用于统计 pass/fail)
        let facts = self.session_ops.get_facts(tcb_session_id).await;

        // 审计/状态/事实必须真实可查, 不得用空值兜底掩盖取数失败 (静默通过治理)
        // 若 session 已关闭导致取数失败, 显式报错而非伪造空报告 (防止报告被误判为"无事实/未验证")
        let state_val = state?;
        let audit_val = audit?;
        let facts_val = facts?;

        // 构建测试报告
        let report = TestReportBuilder::new()
            .sandbox_id(sandbox_id.to_string())
            .workspace_id(sandbox.workspace_id.clone())
            .tcb_session_id(tcb_session_id)
            .parent_session_id(Some(sandbox.parent_session_id as u64))
            .draft_ruleset_hash(sandbox.draft_ruleset_hash.clone().unwrap_or_default())
            .state(state_val)
            .audit(audit_val)
            .facts(facts_val)
            .build();

        Ok(report)
    }

    /// 列出 Workspace 的沙盒测试历史
    pub async fn list_sandboxes(
        &self,
        workspace_id: &str,
        requester: &str,
    ) -> WorkspaceResult<Vec<SandboxSession>> {
        if !self.db.is_workspace_member(workspace_id, requester)? {
            return Err(WorkspaceError::forbidden(format!(
                "user {requester} is not a member of workspace {workspace_id}"
            )));
        }
        self.db.list_sandbox_sessions(workspace_id)
    }

    /// 获取单个沙盒详情
    pub async fn get_sandbox(
        &self,
        workspace_id: &str,
        sandbox_id: i64,
        requester: &str,
    ) -> WorkspaceResult<SandboxSession> {
        if !self.db.is_workspace_member(workspace_id, requester)? {
            return Err(WorkspaceError::forbidden(format!(
                "user {requester} is not a member of workspace {workspace_id}"
            )));
        }
        let sandbox = self
            .db
            .get_sandbox_session(sandbox_id)?
            .ok_or_else(|| WorkspaceError::not_found("sandbox", sandbox_id.to_string()))?;
        if sandbox.workspace_id != workspace_id {
            return Err(WorkspaceError::not_found("sandbox", sandbox_id.to_string()));
        }
        Ok(sandbox)
    }

    /// 创建测试数据集
    pub async fn create_test_dataset(
        &self,
        workspace_id: &str,
        req: crate::models::CreateTestDatasetRequest,
    ) -> WorkspaceResult<TestDatasetRecord> {
        // 校验 workspace 存在
        self.db.get_workspace(workspace_id)?;

        // 解析 cases_json 并计算 case_count
        let cases: Vec<Value> = serde_json::from_str(&req.cases_json).map_err(|e| {
            WorkspaceError::invalid_input(format!("cases_json is not a JSON array: {e}"))
        })?;
        let case_count = cases.len() as i64;

        // 校验 workspace_id 一致性 (如果指定了 workspace_id)
        if let Some(ref ws_id) = req.workspace_id {
            if ws_id != workspace_id {
                return Err(WorkspaceError::invalid_input(
                    "workspace_id in request body does not match path",
                ));
            }
        }

        let id = self.db.insert_test_dataset(
            &req.name,
            // 路径 workspace_id 为权威(REST 语义):数据集归属由 URL 决定;
            // 请求体字段仅作上方一致性校验,不参与落库。
            // UV-071:修复误用 req.workspace_id(缺省 NULL)导致
            // "创建成功但列表按 workspace 过滤永远不可见"。
            Some(workspace_id),
            &req.cases_json,
            case_count,
            &req.created_by,
            req.description.as_deref(),
        )?;

        self.db
            .get_test_dataset(id)?
            .ok_or_else(|| WorkspaceError::internal("test_dataset just inserted but not found"))
    }

    /// 列出 workspace 的测试数据集
    pub async fn list_test_datasets(
        &self,
        workspace_id: &str,
    ) -> WorkspaceResult<Vec<TestDatasetRecord>> {
        // 校验 workspace 存在
        self.db.get_workspace(workspace_id)?;
        self.db.list_test_datasets(workspace_id)
    }
}

/// 计算规则版本集的 BLAKE3 哈希
///
/// 将所有规则版本的内容按 id 排序后拼接,计算 BLAKE3。
/// 排序保证相同规则集产生相同哈希 (与顺序无关)。
fn compute_ruleset_hash(versions: &[crate::models::RuleVersionRecord]) -> String {
    let mut sorted: Vec<&crate::models::RuleVersionRecord> = versions.iter().collect();
    sorted.sort_by(|a, b| a.id.cmp(&b.id));

    // 审计⑥ C3: 哈希实现统一走 evorule-hash; 输入字节构造保持不变 (id + '\n' + content_hash + '\n')
    let mut buf: Vec<u8> = Vec::new();
    for rv in &sorted {
        buf.extend_from_slice(rv.id.as_bytes());
        buf.extend_from_slice(b"\n");
        buf.extend_from_slice(rv.content_hash.as_bytes());
        buf.extend_from_slice(b"\n");
    }
    evorule_hash::digest(&buf)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::db::WorkspaceDb;
    use crate::models::{
        CreateRuleRequest, CreateWorkspaceRequest, RuleVersionRecord, RuleVersionState,
    };
    use crate::rule_meta_service::RuleMetaService;
    use crate::workspace_service::WorkspaceService;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// 测试用 SessionOps 桩 (记录 send_command 调用)
    struct MockSessionOps {
        next_id: AtomicU64,
        commands_sent: Mutex<Vec<(u64, Value)>>,
    }

    use std::sync::Mutex;

    impl MockSessionOps {
        fn new(start: u64) -> Self {
            Self {
                next_id: AtomicU64::new(start),
                commands_sent: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl SessionOps for MockSessionOps {
        async fn create_session(&self) -> WorkspaceResult<u64> {
            Ok(self.next_id.fetch_add(1, Ordering::SeqCst))
        }
        async fn fork_session(&self, _parent: u64) -> WorkspaceResult<u64> {
            Ok(self.next_id.fetch_add(1, Ordering::SeqCst))
        }
        async fn session_exists(&self, id: u64) -> bool {
            id < self.next_id.load(Ordering::SeqCst)
        }
        async fn close_session(&self, _id: u64) -> WorkspaceResult<()> {
            Ok(())
        }
        async fn list_sessions(&self) -> WorkspaceResult<Vec<u64>> {
            Ok(Vec::new())
        }
        async fn send_command(&self, id: u64, cmd: Value) -> WorkspaceResult<u64> {
            self.commands_sent.lock().unwrap().push((id, cmd));
            Ok(0)
        }
        async fn get_session_state(&self, id: u64) -> WorkspaceResult<Value> {
            Ok(serde_json::json!({"session_id": id, "status": "running"}))
        }
        async fn get_audit_report(&self, _id: u64) -> WorkspaceResult<Value> {
            Ok(serde_json::json!({"entry_count": 5, "verified": true}))
        }
        async fn get_audit_export(&self, _id: u64) -> WorkspaceResult<String> {
            Ok(r#"[{"id":1,"type":"Command"},{"id":2,"type":"StateTransition"}]"#.to_string())
        }
        async fn get_facts(&self, _id: u64) -> WorkspaceResult<Vec<Value>> {
            Ok(vec![
                serde_json::json!({"id": 1, "type": "Command"}),
                serde_json::json!({"id": 2, "type": "StateTransition"}),
            ])
        }
        async fn get_causal_chain(&self, _id: u64, _fact_id: u64) -> WorkspaceResult<Value> {
            Ok(serde_json::json!([]))
        }
        async fn reload_rules(&self) -> WorkspaceResult<()> {
            Ok(())
        }
        async fn flush_audit(&self, _id: u64) -> WorkspaceResult<usize> {
            Ok(0)
        }
    }

    /// 构建测试用 SandboxService + 依赖
    fn make_services() -> (
        SandboxService,
        Arc<WorkspaceDb>,
        Arc<MockSessionOps>,
        Arc<WorkspaceService>,
    ) {
        let db = Arc::new(WorkspaceDb::in_memory().unwrap());
        let mock_ops = Arc::new(MockSessionOps::new(500));
        let ops: Arc<dyn SessionOps> = mock_ops.clone();
        let ws_svc = Arc::new(WorkspaceService::new(db.clone(), ops.clone()));
        let sandbox_svc = SandboxService::new(db.clone(), ops);
        // 初始化 production_state (设置初始 session_id)
        db.update_production_state(100, 0, "init_hash", "system")
            .unwrap();
        (sandbox_svc, db, mock_ops, ws_svc)
    }

    async fn create_workspace(svc: &Arc<WorkspaceService>, owner: &str) -> String {
        svc.create_workspace(CreateWorkspaceRequest {
            name: "test-ws".to_string(),
            owner_id: owner.to_string(),
            description: None,
        })
        .await
        .unwrap()
        .id
    }

    async fn create_rule(rule_svc: &RuleMetaService, ws_id: &str, name: &str) -> RuleVersionRecord {
        let rule = rule_svc
            .create_rule(
                ws_id,
                CreateRuleRequest {
                    name: name.to_string(),
                    content: r#"{"transform":[{"type":"noop"}]}"#.to_string(),
                    created_by: "owner-1".to_string(),
                    description: None,
                },
            )
            .await
            .unwrap();
        // 获取当前版本
        let versions = rule_svc.list_rule_versions(ws_id, &rule.id).await.unwrap();
        versions
            .into_iter()
            .find(|v| v.state == RuleVersionState::Current)
            .unwrap()
    }

    #[tokio::test]
    async fn test_start_and_close_sandbox() {
        let (sandbox_svc, db, mock_ops, ws_svc) = make_services();
        let ws_id = create_workspace(&ws_svc, "owner-1").await;
        let rule_svc = Arc::new(RuleMetaService::new(db.clone()));
        let rv = create_rule(&rule_svc, &ws_id, "rule-1").await;

        // 创建测试数据集
        let dataset_id = db
            .insert_test_dataset(
                "ds-1",
                Some(&ws_id),
                r#"[{"event":"test1"},{"event":"test2"}]"#,
                2,
                "owner-1",
                None,
            )
            .unwrap();

        // 启动沙盒
        let resp = sandbox_svc
            .start_sandbox(
                &ws_id,
                StartSandboxRequest {
                    rule_version_ids: vec![rv.id.clone()],
                    test_dataset_id: dataset_id,
                    parent_version: None,
                },
                "owner-1",
            )
            .await
            .unwrap();

        assert!(resp.sandbox_id > 0);
        assert_eq!(resp.tcb_session_id, 500); // MockSessionOps 从 500 开始
        assert!(!resp.draft_ruleset_hash.is_empty());
        assert_eq!(resp.test_case_count, 2);

        // send_command 被调用: 1 次 transform + 2 次 command = 3 次
        {
            let commands = mock_ops.commands_sent.lock().unwrap();
            assert_eq!(commands.len(), 3);
            assert_eq!(commands[0].1["type"], "transform");
            assert_eq!(commands[1].1["type"], "command");
            assert_eq!(commands[2].1["type"], "command");
        }

        // 沙盒记录存在且状态为 running
        let sandbox = db.get_sandbox_session(resp.sandbox_id).unwrap().unwrap();
        assert_eq!(sandbox.status, SandboxStatus::Running);
        assert_eq!(sandbox.parent_session_id, 100);

        // 关闭沙盒
        let export_path = sandbox_svc
            .close_sandbox(resp.sandbox_id, "owner-1")
            .await
            .unwrap();
        assert!(export_path.contains(&format!("sandbox_{}", resp.sandbox_id)));

        // 状态已更新为 closed
        let sandbox = db.get_sandbox_session(resp.sandbox_id).unwrap().unwrap();
        assert_eq!(sandbox.status, SandboxStatus::Closed);
        assert!(sandbox.export_path.is_some());
    }

    #[tokio::test]
    async fn test_start_sandbox_permission_denied() {
        let (sandbox_svc, _db, _mock_ops, ws_svc) = make_services();
        let ws_id = create_workspace(&ws_svc, "owner-1").await;

        // 非 workspace 成员启动沙盒
        let result = sandbox_svc
            .start_sandbox(
                &ws_id,
                StartSandboxRequest {
                    rule_version_ids: vec!["rv-1".to_string()],
                    test_dataset_id: 1,
                    parent_version: None,
                },
                "intruder",
            )
            .await;
        assert!(matches!(result, Err(WorkspaceError::Forbidden(_))));
    }

    #[tokio::test]
    async fn test_generate_test_report() {
        let (sandbox_svc, db, _mock_ops, ws_svc) = make_services();
        let ws_id = create_workspace(&ws_svc, "owner-1").await;
        let rule_svc = Arc::new(RuleMetaService::new(db.clone()));
        let rv = create_rule(&rule_svc, &ws_id, "rule-1").await;

        let dataset_id = db
            .insert_test_dataset(
                "ds-1",
                Some(&ws_id),
                r#"[{"event":"test1"}]"#,
                1,
                "owner-1",
                None,
            )
            .unwrap();

        let resp = sandbox_svc
            .start_sandbox(
                &ws_id,
                StartSandboxRequest {
                    rule_version_ids: vec![rv.id.clone()],
                    test_dataset_id: dataset_id,
                    parent_version: None,
                },
                "owner-1",
            )
            .await
            .unwrap();

        let report = sandbox_svc
            .generate_test_report(resp.sandbox_id)
            .await
            .unwrap();

        assert_eq!(report.workspace_id, ws_id);
        assert_eq!(report.tcb_session_id, 500);
        assert!(!report.report_hash.is_empty());
        assert_eq!(report.audit_info.audit_chain_length, 5);
        assert!(report.audit_info.audit_chain_verified);
    }

    #[test]
    fn test_compute_ruleset_hash_deterministic() {
        let now = chrono::Utc::now();
        let rv1 = RuleVersionRecord {
            id: "rv-1".to_string(),
            rule_id: "r-1".to_string(),
            version: 1,
            content_hash: "hash-1".to_string(),
            content: "{}".to_string(),
            created_at: now,
            state: RuleVersionState::Current,
            created_by: "u".to_string(),
        };
        let rv2 = RuleVersionRecord {
            id: "rv-2".to_string(),
            ..rv1.clone()
        };

        // 相同输入 → 相同哈希 (与顺序无关)
        let h1 = compute_ruleset_hash(&[rv1.clone(), rv2.clone()]);
        let h2 = compute_ruleset_hash(&[rv2.clone(), rv1.clone()]);
        assert_eq!(h1, h2);

        // 不同输入 → 不同哈希
        let h3 = compute_ruleset_hash(&[rv1]);
        assert_ne!(h1, h3);
    }
}

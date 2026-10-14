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
//! 3. Fork Production session → 新 sandbox session，**fork 时原子注入
//!    Draft 规则集**（草稿规则仅进沙盒 Reactor；权利面冲突 fail-fast 拒启）
//! 4. 记录 sandbox_sessions 表
//! 5. 逐条注入合成测试数据（用例即指令本体，与生产 /api/command 同形态）
//! 6. 启动 MockIoResponder (自动回调 io_request)
//! 7. 关闭时导出 test Fact + 生成测试报告
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

/// 沙盒测试报告导出目录（相对默认值；实际路径由 [`sandbox_report_dir`] 解析）。
/// pub: 执行域 import 侧证据一致性校验需按同一规则推导报告路径
/// (report_<facts 文件 basename>.json,与 generate_test_report 关闭态同口径)
pub const SANDBOX_REPORT_DIR: &str = "./data/sandbox_reports";

/// 沙盒报告目录解析（2026-09-26 E9 补缺：消除相对路径对进程 cwd 的隐式依赖，
/// 修复"server 重启后 cwd 变化 → 旧报告不可寻址 → 闸门一误拒"运维事故级断链）。
/// - 显式配置：环境变量 `EVORULE_SANDBOX_REPORT_DIR`（非空即用，相对值基于启动 cwd 解析）；
/// - 默认：基于**启动时 cwd** 解析为绝对路径（进程内稳定；跨重启同一启动目录
///   可寻址——报告目录不再随 cwd 漂移）。部署契约：以固定 cwd 启动 server。
pub fn sandbox_report_dir() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("EVORULE_SANDBOX_REPORT_DIR") {
        if !dir.is_empty() {
            return std::path::PathBuf::from(dir);
        }
    }
    std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join(SANDBOX_REPORT_DIR)
}

/// 从数据集 cases_json 提取测试 case 显式名（按序对应用例组，报告据此命名）。
/// dataset 缺失/解析失败时回退空（报告回退默认命名 "Case #N"，如实降级不阻塞）。
/// 活 session 报告（generate_test_report）与关闭态报告共用，保证两出口命名口径一致。
fn extract_case_names(cases_json: &str) -> Vec<String> {
    serde_json::from_str::<Vec<Value>>(cases_json)
        .map(|cases| {
            cases
                .iter()
                .map(|c| {
                    c.get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default()
}

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
    /// 2. 查询规则版本 + 校验 + 计算 Draft 规则集 BLAKE3 哈希
    /// 3. 解析草稿规则 JSON（fail-fast）
    /// 4. 获取 Production session_id + fork 并原子注入草稿规则集
    ///    （权利面冲突拒启；fork 后事实链空探针）
    /// 5. 记录 sandbox_sessions 表
    /// 6. 注入合成测试数据（用例即指令本体）
    /// 7. 启动 MockIoResponder
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

        // 3. 解析草稿规则 JSON（逐条与规则文件同形态；解析失败 fail-fast）
        let draft_rules = self.parse_draft_rules(&rule_versions)?;

        // 4. 获取 Production session_id
        //    5. fork 并注入草稿规则集（原子序列：草稿规则仅进沙盒 Reactor；
        //    权利面冲突 fail-fast 拒启，此时尚未落 db，无孤儿记录）
        let parent_session_id = self.get_production_parent_session()?;
        // R4/[CON-S-003]：草稿治理指纹=真实 DB 治理事实（版本 ID 逐条对应
        // draft_rules 同序 + compute_ruleset_hash 草稿集哈希），穿透 fork 实现
        // 侧装载 ctx——禁止 provenance 伪造（"draft#i"+空 hash 已退役）
        let provenance = crate::session_bridge::DraftRulesProvenance {
            rule_version_ids: rule_versions.iter().map(|rv| rv.id.clone()).collect(),
            ruleset_hash: draft_hash.clone(),
        };
        let tcb_session_id = self
            .session_ops
            .fork_session_with_rules(parent_session_id, &draft_rules, &provenance)
            .await?;
        info!(
            sandbox_session_id = tcb_session_id,
            parent_session_id = parent_session_id,
            workspace_id = %workspace_id,
            draft_rule_count = draft_rules.len(),
            "Sandbox session forked from production with draft rules injected"
        );

        // 探针（断链复发探测器）：fork 继承的是状态快照而非事实历史，
        // 沙盒事实链应从零起步。非空 = fork 语义漂移（如未来 fork 开始复制
        // 事实历史），报告按 Command 归组将错位——fail-fast 暴露，不静默。
        let initial_facts = self.session_ops.get_facts(tcb_session_id).await?;
        if !initial_facts.is_empty() {
            let _ = self.session_ops.close_session(tcb_session_id).await;
            return Err(WorkspaceError::internal(format!(
                "fork 语义漂移: 沙盒事实链非空（{} 条初始事实），测试报告判定面将错位",
                initial_facts.len()
            )));
        }

        // 6. 记录 sandbox_sessions 表
        let sandbox_id = self.db.insert_sandbox_session(
            Some(tcb_session_id as i64),
            workspace_id,
            parent_session_id as i64,
            Some(&draft_hash),
            req.test_dataset_id,
            started_by,
        )?;

        // 7.（已并入步骤 5）草稿规则经 fork 注入沙盒规则集——不再用
        //    transform 包装指令搬运（宪法无对应规则，恒 Ignored→Error，
        //    草稿从未真正生效的机制断链已修）

        // 8. 注入合成测试数据（用例即指令本体）
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
                "您不是工作空间 {workspace_id} 的成员,无法执行此操作;请联系工作空间所有者将您加入成员,或创建自己的工作空间 (user={started_by})"
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

    /// 解析草稿规则 JSON（每条与规则文件同形态的单条规则）
    ///
    /// 解析失败 fail-fast。草稿规则经 [`SessionOps::fork_session_with_rules`]
    /// 在 fork 时注入沙盒规则集（原 send_command transform 包装指令搬运路径
    /// 已废除——宪法无对应规则致恒 Ignored→Error，草稿从未真正生效）。
    fn parse_draft_rules(
        &self,
        rule_versions: &[RuleVersionRecord],
    ) -> WorkspaceResult<Vec<Value>> {
        let mut rules = Vec::with_capacity(rule_versions.len());
        for rv in rule_versions {
            let rule_content: Value = serde_json::from_str(&rv.content).map_err(|e| {
                WorkspaceError::internal(format!(
                    "rule_version {} content is not valid JSON: {e}",
                    rv.id
                ))
            })?;
            rules.push(rule_content);
        }
        Ok(rules)
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

        for (idx, case) in test_cases.iter().enumerate() {
            // 用例即指令本体：与生产 POST /api/command 同形态、同 TCB 消费路径
            // （原 {"type":"command"} 包装是本服务发明的形态，无任何宪法/草稿
            // 规则匹配，恒 Ignored→Error——用例从未真实执行的断链面之一）。
            // 校验 fail-fast（[CON-X-003] 同锚）：用例类型必须命中宪法
            // sandbox_case_instruction 锚（eval_* 家族）——非对象/非 eval_* 形态
            // 无法被用例报告正确归组（is_case_group 同锚判定），启动期拦截而非
            // 留到报告里变成 startup_noise 假阴。
            // name/expected 等元数据字段随指令一并发送（TCB 对根部额外键零消费）。
            let valid = case.is_object()
                && evorule_rule_schema::is_sandbox_case_instruction(case);
            if !valid {
                return Err(WorkspaceError::invalid_input(format!(
                    "test_dataset {} 用例 #{} 必须是宪法锚定的用例指令对象（type 命中 {} $def，如 eval_hello）",
                    dataset.id, idx, evorule_rule_schema::SANDBOX_CASE_DEF_NAME
                )));
            }
            self.session_ops
                .send_command(tcb_session_id, case.clone())
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

        // : 认知复杂度拆分 — 校验/导出/报告三个语义块提为独立方法,
        // 编排逻辑留在本函数(此前 31/25 触发 clippy cognitive_complexity)
        self.validate_sandbox_close(&sandbox, closed_by)?;

        let tcb_session_id = sandbox.tcb_session_id.unwrap_or(0) as u64;

        // 停止 MockIoResponder
        if let Some(responder) = self.mock_responders.lock().await.remove(&tcb_session_id) {
            responder.stop().await;
        }

        // 导出 test Fact (通过 audit/export API, 存为 JSON 文件)
        let export_path = self
            .export_sandbox_facts(sandbox_id, tcb_session_id)
            .await?;

        // : 关闭前(session 仍活)生成完整 TestReport 并落盘。
        // 此前仅导出 fact 链文件,summary 报告未持久化 → 关闭后
        // generate_test_report 实时取数 404 "session not found",
        // 机器证据回填与"查看报告"功能全断。
        // 报告文件与 facts 文件同目录同时间戳配对:report_sandbox_{id}_{ts}.json
        // (generate_test_report 关闭态按 export_path 推导本路径读取)。
        self.persist_test_report(&sandbox, tcb_session_id, &export_path)
            .await?;

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

    /// close_sandbox 前置校验: 状态必须 Running + 操作者必须是工作区成员
    fn validate_sandbox_close(
        &self,
        sandbox: &SandboxSession,
        closed_by: &str,
    ) -> WorkspaceResult<()> {
        if sandbox.status != SandboxStatus::Running {
            return Err(WorkspaceError::InvalidStateTransition {
                from: sandbox.status.as_str().to_string(),
                to: SandboxStatus::Closed.as_str().to_string(),
            });
        }

        if !self
            .db
            .is_workspace_member(&sandbox.workspace_id, closed_by)?
        {
            return Err(WorkspaceError::forbidden(format!(
                "您不是工作空间 {} 的成员,无法关闭沙盒;请联系工作空间所有者将您加入成员 (user={closed_by})",
                sandbox.workspace_id
            )));
        }
        Ok(())
    }

    /// 导出 session 审计 fact 链到沙盒报告目录（[`sandbox_report_dir`]），返回导出文件路径
    async fn export_sandbox_facts(
        &self,
        sandbox_id: i64,
        tcb_session_id: u64,
    ) -> WorkspaceResult<String> {
        let dir = sandbox_report_dir();
        let export_path = dir.join(format!(
            "sandbox_{}_{}.json",
            sandbox_id,
            chrono::Utc::now().timestamp()
        ));
        let audit_data = self.session_ops.get_audit_export(tcb_session_id).await?;
        std::fs::create_dir_all(&dir).map_err(|e| {
            WorkspaceError::internal(format!("create sandbox_report dir failed: {e}"))
        })?;
        std::fs::write(&export_path, &audit_data)
            .map_err(|e| WorkspaceError::internal(format!("write sandbox export failed: {e}")))?;
        Ok(export_path.to_string_lossy().to_string())
    }

    /// : 生成完整 TestReport 并落盘 (与 facts 文件同目录同时间戳配对)
    async fn persist_test_report(
        &self,
        sandbox: &SandboxSession,
        tcb_session_id: u64,
        export_path: &str,
    ) -> WorkspaceResult<()> {
        let sandbox_id = sandbox.id;
        let report_path = sandbox_report_dir()
            .join(format!(
                "report_{}",
                export_path.rsplit(['/', '\\']).next().unwrap_or_default()
            ))
            .to_string_lossy()
            .to_string();
        let state_val = self.session_ops.get_session_state(tcb_session_id).await?;
        let audit_val = self.session_ops.get_audit_report(tcb_session_id).await?;
        let facts_val = self.session_ops.get_facts(tcb_session_id).await?;

        // 提取测试 case 显式名(按序对应 facts, 报告据此命名不再 unknown)。
        let case_names = self
            .db
            .get_test_dataset(sandbox.test_dataset_id)
            .ok()
            .flatten()
            .map(|ds| extract_case_names(&ds.cases_json))
            .unwrap_or_default();

        let report = TestReportBuilder::new()
            .sandbox_id(sandbox_id.to_string())
            .workspace_id(sandbox.workspace_id.clone())
            .tcb_session_id(tcb_session_id)
            .parent_session_id(Some(sandbox.parent_session_id as u64))
            .draft_ruleset_hash(sandbox.draft_ruleset_hash.clone().unwrap_or_default())
            .state(state_val)
            .audit(audit_val)
            .facts(facts_val)
            .case_names(case_names)
            .build();
        let report_json = serde_json::to_string_pretty(&report)
            .map_err(|e| WorkspaceError::internal(format!("serialize test report failed: {e}")))?;
        std::fs::write(&report_path, report_json).map_err(|e| {
            WorkspaceError::internal(format!(
                "write sandbox test report failed: {e} (path: {report_path})"
            ))
        })?;
        info!(
            sandbox_id = sandbox_id,
            report_path = %report_path,
            verdict_failed = report.summary.failed,
            ": sandbox test report persisted before close"
        );
        Ok(())
    }

    /// 生成测试报告 (从 sandbox session 的 audit + state + facts 聚合)
    ///
    /// 报告包含 BLAKE3 签名 (防篡改),可附带在发布队列项中供审批者查阅。
    ///
    /// : running 沙盒实时聚合(现状);closed 沙盒从 close 时持久化的
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
            let file_name = export_path.rsplit(['/', '\\']).next().unwrap_or_default();
            let report_path = sandbox_report_dir()
                .join(format!("report_{file_name}"))
                .to_string_lossy()
                .to_string();
            let content = std::fs::read_to_string(&report_path).map_err(|_| {
                WorkspaceError::not_found(
                    "sandbox report file",
                    format!(
                        "{report_path} (沙盒已关闭且报告文件缺失:可能被清理或属 \
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
        // 用例显式名接线：活 session 报告与关闭态报告同口径（此前仅关闭态
        // 接线，GET report 恒回退 "Case #N"，按名判据/审计可读性断链）。
        let case_names = self
            .db
            .get_test_dataset(sandbox.test_dataset_id)
            .ok()
            .flatten()
            .map(|ds| extract_case_names(&ds.cases_json))
            .unwrap_or_default();
        let report = TestReportBuilder::new()
            .sandbox_id(sandbox_id.to_string())
            .workspace_id(sandbox.workspace_id.clone())
            .tcb_session_id(tcb_session_id)
            .parent_session_id(Some(sandbox.parent_session_id as u64))
            .draft_ruleset_hash(sandbox.draft_ruleset_hash.clone().unwrap_or_default())
            .state(state_val)
            .audit(audit_val)
            .facts(facts_val)
            .case_names(case_names)
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
                "您不是工作空间 {workspace_id} 的成员,无法执行此操作;请联系工作空间所有者将您加入成员,或创建自己的工作空间 (user={requester})"
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
                "您不是工作空间 {workspace_id} 的成员,无法执行此操作;请联系工作空间所有者将您加入成员,或创建自己的工作空间 (user={requester})"
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

        // [CON-L1-001] 写入门禁：每个用例必须命中宪法 sandbox_case_instruction
        // 锚（[CON-X-003]，eval_* 家族）。与 inject_test_cases 的 fail-fast 同锚
        // 同法——创建期拦截脏数据集落库，而非留到沙盒启动才炸（旁路写入=拒，
        // 无降级通道）。
        for (idx, case) in cases.iter().enumerate() {
            if !case.is_object() || !evorule_rule_schema::is_sandbox_case_instruction(case) {
                return Err(WorkspaceError::invalid_input(format!(
                    "cases_json[{idx}] 必须是宪法锚定的用例指令对象（type 命中 {} $def，如 eval_hello）",
                    evorule_rule_schema::SANDBOX_CASE_DEF_NAME
                )));
            }
        }

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
            // :修复误用 req.workspace_id(缺省 NULL)导致
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

    /// 测试用 SessionOps 桩 (记录 send_command/fork 注入调用)
    struct MockSessionOps {
        next_id: AtomicU64,
        commands_sent: Mutex<Vec<(u64, Value)>>,
        /// fork_session_with_rules 注入的 (session_id, 草稿规则数组)
        rules_injected: Mutex<Vec<(u64, Vec<Value>)>>,
    }

    use std::sync::Mutex;

    impl MockSessionOps {
        fn new(start: u64) -> Self {
            Self {
                next_id: AtomicU64::new(start),
                commands_sent: Mutex::new(Vec::new()),
                rules_injected: Mutex::new(Vec::new()),
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
        async fn fork_session_with_rules(
            &self,
            _parent: u64,
            extra_rules: &[Value],
            _provenance: &crate::session_bridge::DraftRulesProvenance,
        ) -> WorkspaceResult<u64> {
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            self.rules_injected
                .lock()
                .unwrap()
                .push((id, extra_rules.to_vec()));
            // 契约：fork 后事实链为空（状态快照非事实历史）
            Ok(id)
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
        async fn get_facts(&self, id: u64) -> WorkspaceResult<Vec<Value>> {
            // 模拟真实契约：fork 后未注入指令前事实链为空（探针依赖此语义），
            // 指令注入后按沙盒报告判定整改确立的完整事实形态返回样例
            let has_commands = self
                .commands_sent
                .lock()
                .unwrap()
                .iter()
                .any(|(sid, _)| *sid == id);
            if has_commands {
                Ok(vec![
                    serde_json::json!({
                        "id": 1, "type": "Command",
                        "instruction": {"type": "eval_increment", "params": {}},
                    }),
                    serde_json::json!({"id": 2, "type": "StateTransition"}),
                ])
            } else {
                Ok(Vec::new())
            }
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
                    content: r#"{"transform":[{"type":"set","params":{"attr":"__exec__.payload.x","operation":"set","value":1}}]}"#.to_string(),
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

        // 创建测试数据集（用例即指令本体：type 命中宪法 sandbox_case_instruction
        // 锚（eval_* 家族）；name 元数据字段随指令发送，报告按用例组序命名）
        let dataset_id = db
            .insert_test_dataset(
                "ds-1",
                Some(&ws_id),
                r#"[{"name":"用例1","type":"eval_increment","params":{}},{"name":"用例2","type":"eval_increment","params":{}}]"#,
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

        // 草稿规则经 fork 注入（不再走 send_command transform 搬运）
        {
            let injected = mock_ops.rules_injected.lock().unwrap();
            assert_eq!(injected.len(), 1, "fork_session_with_rules 应恰好调用一次");
            assert_eq!(injected[0].0, 500);
            assert_eq!(injected[0].1.len(), 1, "1 条草稿规则应注入沙盒规则集");
        }

        // send_command 仅用例注入: 2 次直发（无 transform/command 包装指令）
        {
            let commands = mock_ops.commands_sent.lock().unwrap();
            assert_eq!(commands.len(), 2);
            assert_eq!(commands[0].1["type"], "eval_increment");
            assert_eq!(commands[1].1["type"], "eval_increment");
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

    /// [CON-L1-001] 旁路写入拒绝：create_test_dataset 落库期宪法锚门禁——
    /// 非 eval_* 形态（含旧 command 字面量）与非法 JSON 数组元素一律拒，
    /// 无降级通道（创建期拦截，不留到沙盒启动延迟爆）。
    #[tokio::test]
    async fn test_create_dataset_rejects_non_anchored_cases() {
        let (sandbox_svc, _db, _mock_ops, ws_svc) = make_services();
        let ws_id = create_workspace(&ws_svc, "owner-1").await;

        use crate::models::CreateTestDatasetRequest;
        // 旧字面量形态（历史白名单口径，宪法无此指令类型）
        let legacy = sandbox_svc
            .create_test_dataset(
                &ws_id,
                CreateTestDatasetRequest {
                    name: "legacy-form".into(),
                    cases_json: r#"[{"name":"c1","type":"command","params":{}}]"#.into(),
                    created_by: "owner-1".into(),
                    workspace_id: None,
                    description: None,
                },
            )
            .await;
        assert!(
            matches!(&legacy, Err(WorkspaceError::InvalidInput(m)) if m.contains("sandbox_case_instruction")),
            "legacy literal must be rejected at write gate: {legacy:?}"
        );

        // 非对象元素
        let junk = sandbox_svc
            .create_test_dataset(
                &ws_id,
                CreateTestDatasetRequest {
                    name: "junk".into(),
                    cases_json: r#"["not-an-object"]"#.into(),
                    created_by: "owner-1".into(),
                    workspace_id: None,
                    description: None,
                },
            )
            .await;
        assert!(matches!(&junk, Err(WorkspaceError::InvalidInput(_))));

        // 宪法锚形态过门（对照：合法 eval_* 落库成功）
        let ok = sandbox_svc
            .create_test_dataset(
                &ws_id,
                CreateTestDatasetRequest {
                    name: "anchored".into(),
                    cases_json: r#"[{"name":"c1","type":"eval_hello","params":{}}]"#.into(),
                    created_by: "owner-1".into(),
                    workspace_id: None,
                    description: None,
                },
            )
            .await;
        assert!(ok.is_ok(), "anchored eval_* form must pass: {ok:?}");
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
                r#"[{"name":"用例1","type":"eval_increment","params":{}}]"#,
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
        // 用例显式名接线（活 session 报告与关闭态同口径）：
        // dataset 单条用例 name="用例1"，mock facts 单 Command 组（eval_* 锚命中）
        assert_eq!(report.cases.len(), 1, "单用例应成单用例组");
        assert_eq!(report.cases[0].case_name, "用例1");
    }

    #[test]
    fn test_extract_case_names() {
        // 有名/缺名/坏 JSON 三形态
        assert_eq!(
            extract_case_names(r#"[{"name":"a","type":"eval_x"},{"type":"eval_y"}]"#),
            vec!["a".to_string(), String::new()]
        );
        assert_eq!(extract_case_names("not json"), Vec::<String>::new());
        assert_eq!(extract_case_names("[]"), Vec::<String>::new());
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

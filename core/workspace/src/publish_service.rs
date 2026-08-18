// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 发布队列服务 — 三层架构 §7 发布工作流
//!
//! 设计依据: PUBLISH_QUEUE_DESIGN.md §3 (P1)
//!
//! # 职责
//! - 发布队列 CRUD + 三级权限校验 (PublishRole)
//! - 状态机: Pending → Approved → Published / Rejected / Cancelled
//! - 审批通过后触发滚动 session 热重载 (委托 RollingSessionService)
//! - 紧急回滚 (用旧规则集快照 + 新版本号, 版本号只增不减)
//!
//! # 三级权限 (Q3 决策)
//! | 角色 | 提交 | 审批 | 回滚 |
//! |------|------|------|------|
//! | Doctor | ❌ | ❌ | ❌ |
//! | DepartmentHead | ✅ (本科室 WS) | ❌ | ❌ |
//! | Admin | ❌ | ✅ (全院) | ✅ |

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::Mutex;
use tracing::info;

use crate::db::WorkspaceDb;
use crate::error::{WorkspaceError, WorkspaceResult};
use crate::models::{
    ProductionAuditRecord, ProductionStateRecord, PublishQueueItem, PublishRole, PublishStatus,
    ReviewPublishRequest, RollbackRequest, SubmitPublishRequest,
};
use crate::rolling_session::RollingSessionService;

/// 发布队列服务
///
/// 持有 `Arc<WorkspaceDb>` + `Arc<dyn SessionOps>` + `RollingSessionService`。
/// 通过 `tokio::sync::Mutex` 全局发布锁串行化发布动作 (P2 决策)。
pub struct PublishService {
    db: Arc<WorkspaceDb>,
    rolling_session: RollingSessionService,
    /// 全局发布锁 (同时只允许一个发布动作执行)
    publish_lock: Arc<Mutex<()>>,
}

impl PublishService {
    /// 创建新服务实例
    pub fn new(db: Arc<WorkspaceDb>, rolling_session: RollingSessionService) -> Self {
        Self {
            db,
            rolling_session,
            publish_lock: Arc::new(Mutex::new(())),
        }
    }

    /// 提交到发布队列 (科室主任权限)
    ///
    /// 流程:
    /// 1. 校验角色 (仅 DepartmentHead)
    /// 2. 校验规则版本存在 + 所属 workspace + 规则状态为 Candidate
    /// 3. 计算规则集 BLAKE3 哈希
    /// 4. 序列化规则集 (JSON 数组)
    /// 5. 插入 publish_queue 表 (status=pending)
    pub async fn submit_publish(
        &self,
        req: SubmitPublishRequest,
        submitted_by: &str,
        role: &PublishRole,
    ) -> WorkspaceResult<PublishQueueItem> {
        // 1. 权限校验
        if !role.can_submit_publish() {
            return Err(WorkspaceError::forbidden(format!(
                "role {:?} cannot submit to publish queue (requires DepartmentHead)",
                role
            )));
        }

        // 校验提交者是 workspace 成员
        if !self
            .db
            .is_workspace_member(&req.workspace_id, submitted_by)?
        {
            return Err(WorkspaceError::forbidden(format!(
                "user {submitted_by} is not a member of workspace {}",
                req.workspace_id
            )));
        }

        if req.rule_version_ids.is_empty() {
            return Err(WorkspaceError::invalid_input(
                "rule_version_ids must not be empty",
            ));
        }

        // 2. 查询规则版本 + 校验所属 workspace + 状态为 Candidate
        let rule_versions = self.db.get_rule_versions_by_ids(&req.rule_version_ids)?;
        if rule_versions.len() != req.rule_version_ids.len() {
            return Err(WorkspaceError::not_found(
                "rule_version",
                format!(
                    "requested {} but found {}",
                    req.rule_version_ids.len(),
                    rule_versions.len()
                ),
            ));
        }

        let mut rules_json: Vec<Value> = Vec::with_capacity(rule_versions.len());
        for rv in &rule_versions {
            // 校验规则属于该 workspace
            let rule = self.db.get_rule(&rv.rule_id)?;
            if rule.workspace_id != req.workspace_id {
                return Err(WorkspaceError::forbidden(format!(
                    "rule {} (version {}) does not belong to workspace {}",
                    rule.id, rv.id, req.workspace_id
                )));
            }
            // 校验规则状态为 Candidate (待发布)
            // 设计文档称 final_candidate, 对应当前 RuleState::Candidate
            use crate::models::RuleState;
            if rule.state != RuleState::Candidate {
                return Err(WorkspaceError::InvalidStateTransition {
                    from: rule.state.as_str().to_string(),
                    to: "published".to_string(),
                });
            }
            // 解析规则内容
            let content: Value = serde_json::from_str(&rv.content).map_err(|e| {
                WorkspaceError::internal(format!(
                    "rule_version {} content is not valid JSON: {e}",
                    rv.id
                ))
            })?;
            rules_json.push(content);
        }

        // 3. 计算规则集 BLAKE3 哈希
        let ruleset_hash = compute_publish_ruleset_hash(&rules_json);

        // 4. 序列化规则集
        let final_candidate_rules = serde_json::to_string(&rules_json)?;

        // 5. 插入 publish_queue
        let id = self.db.insert_publish_queue_item(
            &req.workspace_id,
            &final_candidate_rules,
            &ruleset_hash,
            req.test_report_sandbox_id,
            submitted_by,
            req.description.as_deref(),
        )?;

        info!(
            queue_id = id,
            workspace_id = %req.workspace_id,
            rule_count = rules_json.len(),
            ruleset_hash = %ruleset_hash,
            "Publish request submitted to queue"
        );

        // 缺口3: 记录 publish_submitted 生命周期节点到 production_audit
        // 完整发布审计链: publish_submitted → publish_reviewed → ruleset_published
        // 生命周期事件不改变版本号, ruleset_version 记录提交时的当前生产版本 (上下文)。
        // get_production_audit_by_version 已过滤此类事件, 不会干扰回滚快照加载。
        let prod_state = self.db.get_production_state()?;
        let source_ws_ids = serde_json::json!([req.workspace_id]).to_string();
        self.db.insert_production_audit(
            "publish_submitted",
            prod_state.ruleset_version,
            None,
            &ruleset_hash,
            prod_state.current_session_id.unwrap_or(0),
            &source_ws_ids,
            submitted_by,
            None,
            None, // test_report_paths (提交阶段沙盒可能未关闭, 路径在发布时由 execute_publish 关联)
            None, // ruleset_snapshot (生命周期事件, 无快照)
        )?;

        self.db.get_publish_queue_item(id)?.ok_or_else(|| {
            WorkspaceError::internal("publish_queue item just inserted but not found")
        })
    }

    /// 列出发布队列 (按状态过滤)
    ///
    /// 所有角色可查看队列 (但只有 Admin 可审批)。
    pub async fn list_queue(
        &self,
        status_filter: Option<PublishStatus>,
    ) -> WorkspaceResult<Vec<PublishQueueItem>> {
        self.db.list_publish_queue(status_filter)
    }

    /// 获取单个队列项详情
    pub async fn get_queue_item(&self, id: i64) -> WorkspaceResult<PublishQueueItem> {
        self.db
            .get_publish_queue_item(id)?
            .ok_or_else(|| WorkspaceError::not_found("publish_queue", id.to_string()))
    }

    /// 审批发布 (信息科/院领导权限)
    ///
    /// 流程:
    /// 1. 校验角色 (仅 Admin)
    /// 2. 校验队列项状态 (必须 pending)
    /// 3. approved → 更新状态 + 触发滚动 session 热重载 + 标记 published
    ///    rejected → 更新状态 + 通知 Workspace
    pub async fn review_publish(
        &self,
        queue_id: i64,
        req: ReviewPublishRequest,
        reviewed_by: &str,
        role: &PublishRole,
    ) -> WorkspaceResult<PublishQueueItem> {
        // 1. 权限校验
        if !role.can_review_publish() {
            return Err(WorkspaceError::forbidden(format!(
                "role {:?} cannot review publish queue (requires Admin)",
                role
            )));
        }

        let item = self
            .db
            .get_publish_queue_item(queue_id)?
            .ok_or_else(|| WorkspaceError::not_found("publish_queue", queue_id.to_string()))?;

        if item.status != PublishStatus::Pending {
            return Err(WorkspaceError::InvalidStateTransition {
                from: item.status.as_str().to_string(),
                to: "reviewed".to_string(),
            });
        }

        // 缺口3: 审批前获取当前生产状态, 用于记录 publish_reviewed 生命周期节点
        // (approved: 在 execute_publish 改版本前记录; rejected: 记录驳回决定)
        let prod_state = self.db.get_production_state()?;
        let source_ws_ids = serde_json::json!([item.workspace_id]).to_string();

        match req.decision.as_str() {
            "approved" => {
                // 更新队列状态为 approved
                self.db.update_publish_queue_status(
                    queue_id,
                    PublishStatus::Approved,
                    reviewed_by,
                    req.comment.as_deref(),
                )?;

                // 缺口3: 记录 publish_reviewed (approved) 生命周期节点
                // 在 execute_publish 改变版本前记录, ruleset_version 为当前生产版本。
                let review_reason = format!(
                    "decision=approved, comment={}",
                    req.comment.as_deref().unwrap_or("")
                );
                self.db.insert_production_audit(
                    "publish_reviewed",
                    prod_state.ruleset_version,
                    None,
                    &item.ruleset_hash,
                    prod_state.current_session_id.unwrap_or(0),
                    &source_ws_ids,
                    reviewed_by,
                    Some(&review_reason),
                    None,
                    None,
                )?;

                // 触发滚动 session 热重载 (加全局发布锁)
                let published_version = self.execute_publish(queue_id, reviewed_by).await?;

                // 标记队列为 published
                self.db.complete_publish(queue_id, published_version)?;

                info!(
                    queue_id = queue_id,
                    published_version = published_version,
                    "Publish completed: ruleset rolled out to production"
                );
            }
            "rejected" => {
                self.db.update_publish_queue_status(
                    queue_id,
                    PublishStatus::Rejected,
                    reviewed_by,
                    req.comment.as_deref(),
                )?;

                // 缺口3: 记录 publish_reviewed (rejected) 生命周期节点
                let review_reason = format!(
                    "decision=rejected, comment={}",
                    req.comment.as_deref().unwrap_or("")
                );
                self.db.insert_production_audit(
                    "publish_reviewed",
                    prod_state.ruleset_version,
                    None,
                    &item.ruleset_hash,
                    prod_state.current_session_id.unwrap_or(0),
                    &source_ws_ids,
                    reviewed_by,
                    Some(&review_reason),
                    None,
                    None,
                )?;

                info!(
                    queue_id = queue_id,
                    comment = ?req.comment,
                    "Publish request rejected"
                );
            }
            other => {
                return Err(WorkspaceError::invalid_input(format!(
                    "invalid decision: {other} (expected 'approved' or 'rejected')"
                )));
            }
        }

        self.db
            .get_publish_queue_item(queue_id)?
            .ok_or_else(|| WorkspaceError::not_found("publish_queue", queue_id.to_string()))
    }

    /// 执行发布 (滚动 session 热重载)
    ///
    /// 获取全局发布锁 → 调用 RollingSessionService.rolling_swap
    async fn execute_publish(&self, queue_id: i64, published_by: &str) -> WorkspaceResult<i64> {
        let _lock = self.publish_lock.lock().await;

        let item = self
            .db
            .get_publish_queue_item(queue_id)?
            .ok_or_else(|| WorkspaceError::not_found("publish_queue", queue_id.to_string()))?;

        // 解析规则集
        let rules: Vec<Value> = serde_json::from_str(&item.final_candidate_rules).map_err(|e| {
            WorkspaceError::internal(format!("parse final_candidate_rules failed: {e}"))
        })?;

        // 缺口4 修复: 关联沙盒测试报告路径到 production_audit
        // 若提交时关联了 test_report_sandbox_id, 从 sandbox_sessions.export_path 查得报告路径,
        // 写入 production_audit.test_report_paths (SANDBOX_ORCHESTRATION_DESIGN.md §5.3)。
        let test_report_paths: Option<String> = match item.test_report_sandbox_id {
            Some(sandbox_id) => self
                .db
                .get_sandbox_session(sandbox_id)?
                .and_then(|s| s.export_path),
            None => None,
        };

        // 执行滚动 session 热重载
        let result = self
            .rolling_session
            .rolling_swap(
                &rules,
                &item.ruleset_hash,
                &item.workspace_id,
                published_by,
                None, // 正常发布无回滚原因
                test_report_paths.as_deref(),
            )
            .await?;

        Ok(result.new_ruleset_version)
    }

    /// 紧急回滚 (信息科/院领导权限)
    ///
    /// 回滚 = 用旧版本 ruleset_snapshot 的规则集 + 新版本号 (版本号只增不减)。
    /// 通过 production_audit.ruleset_snapshot 加载目标版本的规则集。
    pub async fn emergency_rollback(
        &self,
        req: RollbackRequest,
        operated_by: &str,
        role: &PublishRole,
    ) -> WorkspaceResult<i64> {
        // 1. 权限校验
        if !role.can_rollback() {
            return Err(WorkspaceError::forbidden(format!(
                "role {:?} cannot rollback (requires Admin)",
                role
            )));
        }

        // 2. 查找目标版本的规则集快照
        let target_audit = self
            .db
            .get_production_audit_by_version(req.target_version)?
            .ok_or_else(|| {
                WorkspaceError::not_found(
                    "production_audit (version)",
                    req.target_version.to_string(),
                )
            })?;

        // 3. 从快照加载规则集
        let snapshot_str = target_audit.ruleset_snapshot.as_deref().ok_or_else(|| {
            WorkspaceError::internal(format!(
                "production_audit version {} has no ruleset_snapshot (cannot rollback)",
                req.target_version
            ))
        })?;
        let rules: Vec<Value> = serde_json::from_str(snapshot_str)
            .map_err(|e| WorkspaceError::internal(format!("parse ruleset_snapshot failed: {e}")))?;

        if rules.is_empty() {
            return Err(WorkspaceError::invalid_input(format!(
                "ruleset_snapshot for version {} is empty (cannot rollback to empty ruleset)",
                req.target_version
            )));
        }

        // 4. 加全局发布锁 → 执行滚动 session 切换 (用旧规则集)
        let _lock = self.publish_lock.lock().await;

        let result = self
            .rolling_session
            .rolling_swap(
                &rules,
                &target_audit.ruleset_hash,
                "rollback",
                operated_by,
                Some(&req.reason),
                None, // 回滚不产生新的测试报告
            )
            .await?;

        info!(
            target_version = req.target_version,
            new_version = result.new_ruleset_version,
            operated_by = operated_by,
            reason = %req.reason,
            "Emergency rollback completed"
        );

        Ok(result.new_ruleset_version)
    }

    /// 获取当前生产状态
    pub async fn get_production_state(&self) -> WorkspaceResult<ProductionStateRecord> {
        self.db.get_production_state()
    }

    /// 列出生产审计记录
    pub async fn list_production_audit(
        &self,
        limit: i64,
    ) -> WorkspaceResult<Vec<ProductionAuditRecord>> {
        self.db.list_production_audit(limit)
    }
}

/// 计算发布规则集的 BLAKE3 哈希
///
/// 将规则 JSON 内容拼接 (按序列化字符串顺序),计算 BLAKE3。
/// 与 sandbox_service 的 compute_ruleset_hash 不同:
/// - sandbox 按 rule_version_id + content_hash 计算 (沙盒阶段规则版本固定)
/// - publish 按规则内容 JSON 计算 (发布阶段规则内容固定)
fn compute_publish_ruleset_hash(rules: &[Value]) -> String {
    let mut hasher = blake3::Hasher::new();
    for rule in rules {
        // 使用规范化的 JSON 字符串参与哈希
        let rule_str = serde_json::to_string(rule).unwrap_or_default();
        hasher.update(rule_str.as_bytes());
        hasher.update(b"\n");
    }
    hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::db::WorkspaceDb;
    use crate::models::{CreateRuleRequest, CreateWorkspaceRequest, RuleVersionState};
    use crate::rolling_session::RollingSessionService;
    use crate::session_bridge::SessionOps;
    use crate::session_switched::SessionSwitchedBroadcaster;
    use crate::workspace_service::WorkspaceService;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct MockSessionOps {
        next_id: AtomicU64,
    }
    impl MockSessionOps {
        fn new(start: u64) -> Self {
            Self {
                next_id: AtomicU64::new(start),
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
        async fn close_session(&self, _id: u64) -> WorkspaceResult<()> {
            Ok(())
        }
        async fn list_sessions(&self) -> WorkspaceResult<Vec<u64>> {
            Ok(Vec::new())
        }
        async fn send_command(&self, _id: u64, _cmd: Value) -> WorkspaceResult<u64> {
            Ok(0)
        }
        async fn get_session_state(&self, id: u64) -> WorkspaceResult<Value> {
            Ok(serde_json::json!({"session_id": id}))
        }
        async fn get_audit_report(&self, _id: u64) -> WorkspaceResult<Value> {
            Ok(serde_json::json!({"entry_count": 0, "verified": true}))
        }
        async fn get_audit_export(&self, _id: u64) -> WorkspaceResult<String> {
            Ok("[]".to_string())
        }
        async fn get_facts(&self, _id: u64) -> WorkspaceResult<Vec<Value>> {
            Ok(Vec::new())
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

    /// 构建测试用 PublishService + 依赖
    async fn make_services() -> (
        PublishService,
        Arc<WorkspaceDb>,
        Arc<RuleMetaServiceHandle>,
        String,
    ) {
        let db = Arc::new(WorkspaceDb::in_memory().unwrap());
        let ops: Arc<dyn SessionOps> = Arc::new(MockSessionOps::new(1000));
        let switcher = SessionSwitchedBroadcaster::new();
        let rolling = RollingSessionService::new(db.clone(), ops.clone(), switcher);
        let publish_svc = PublishService::new(db.clone(), rolling);
        let ws_svc = Arc::new(WorkspaceService::new(db.clone(), ops));
        // 初始化 production_state
        db.update_production_state(100, 0, "init_hash", "system")
            .unwrap();
        // 创建 workspace
        let ws_id = ws_svc
            .create_workspace(CreateWorkspaceRequest {
                name: "test-ws".to_string(),
                owner_id: "head-1".to_string(),
                description: None,
            })
            .await
            .unwrap()
            .id;
        let rule_svc = Arc::new(RuleMetaServiceHandle {
            inner: Arc::new(RuleMetaService::new(db.clone())),
        });
        (publish_svc, db, rule_svc, ws_id)
    }

    /// 测试辅助: 包装 RuleMetaService 以便共享
    struct RuleMetaServiceHandle {
        inner: Arc<RuleMetaService>,
    }

    /// 创建 Candidate 规则并返回其当前版本 ID
    async fn make_candidate_rule(
        rule_svc: &RuleMetaService,
        _db: &WorkspaceDb,
        ws_id: &str,
        name: &str,
    ) -> String {
        let rule = rule_svc
            .create_rule(
                ws_id,
                CreateRuleRequest {
                    name: name.to_string(),
                    content: r#"{"transform":[{"type":"noop"}]}"#.to_string(),
                    created_by: "head-1".to_string(),
                    description: None,
                },
            )
            .await
            .unwrap();
        // Draft → Candidate
        rule_svc.submit_rule(ws_id, &rule.id).await.unwrap();
        // 获取当前版本 ID
        let versions = rule_svc.list_rule_versions(ws_id, &rule.id).await.unwrap();
        versions
            .into_iter()
            .find(|v| v.state == RuleVersionState::Current)
            .unwrap()
            .id
    }

    use crate::rule_meta_service::RuleMetaService;

    #[tokio::test]
    async fn test_submit_publish_permission_denied_for_doctor() {
        let (publish_svc, _db, _rule_svc, ws_id) = make_services().await;

        let result = publish_svc
            .submit_publish(
                SubmitPublishRequest {
                    workspace_id: ws_id,
                    rule_version_ids: vec!["rv-1".to_string()],
                    test_report_sandbox_id: None,
                    description: None,
                },
                "doctor-1",
                &PublishRole::Doctor,
            )
            .await;
        assert!(matches!(result, Err(WorkspaceError::Forbidden(_))));
    }

    #[tokio::test]
    async fn test_submit_publish_by_department_head() {
        let (publish_svc, db, rule_svc_handle, ws_id) = make_services().await;
        let rv_id = make_candidate_rule(&rule_svc_handle.inner, &db, &ws_id, "rule-1").await;

        let item = publish_svc
            .submit_publish(
                SubmitPublishRequest {
                    workspace_id: ws_id.clone(),
                    rule_version_ids: vec![rv_id],
                    test_report_sandbox_id: None,
                    description: Some("内科规则发布".to_string()),
                },
                "head-1",
                &PublishRole::DepartmentHead,
            )
            .await
            .unwrap();

        assert_eq!(item.workspace_id, ws_id);
        assert_eq!(item.status, PublishStatus::Pending);
        assert_eq!(item.submitted_by, "head-1");
        assert!(!item.ruleset_hash.is_empty());
        assert!(item.final_candidate_rules.contains("noop"));
    }

    #[tokio::test]
    async fn test_review_publish_full_flow() {
        let (publish_svc, db, rule_svc_handle, ws_id) = make_services().await;
        let rv_id = make_candidate_rule(&rule_svc_handle.inner, &db, &ws_id, "rule-1").await;

        // 科室主任提交
        let item = publish_svc
            .submit_publish(
                SubmitPublishRequest {
                    workspace_id: ws_id.clone(),
                    rule_version_ids: vec![rv_id],
                    test_report_sandbox_id: None,
                    description: None,
                },
                "head-1",
                &PublishRole::DepartmentHead,
            )
            .await
            .unwrap();

        // 信息科审批通过
        let published = publish_svc
            .review_publish(
                item.id,
                ReviewPublishRequest {
                    decision: "approved".to_string(),
                    comment: Some("通过".to_string()),
                },
                "admin-1",
                &PublishRole::Admin,
            )
            .await
            .unwrap();

        assert_eq!(published.status, PublishStatus::Published);
        assert!(published.published_version.is_some());
        assert_eq!(published.published_version.unwrap(), 1); // 0 → 1

        // production_state 已更新
        let state = db.get_production_state().unwrap();
        assert_eq!(state.ruleset_version, 1);
        assert_eq!(state.current_session_id, Some(1000)); // MockSessionOps 从 1000 开始

        // production_audit 已记录
        let audit = db.get_production_audit_by_version(1).unwrap().unwrap();
        assert_eq!(audit.event_type, "ruleset_published");
        assert!(audit.ruleset_snapshot.is_some());
    }

    #[tokio::test]
    async fn test_review_publish_rejected() {
        let (publish_svc, _db, rule_svc_handle, ws_id) = make_services().await;
        let rv_id = make_candidate_rule(&rule_svc_handle.inner, &_db, &ws_id, "rule-1").await;

        let item = publish_svc
            .submit_publish(
                SubmitPublishRequest {
                    workspace_id: ws_id,
                    rule_version_ids: vec![rv_id],
                    test_report_sandbox_id: None,
                    description: None,
                },
                "head-1",
                &PublishRole::DepartmentHead,
            )
            .await
            .unwrap();

        let rejected = publish_svc
            .review_publish(
                item.id,
                ReviewPublishRequest {
                    decision: "rejected".to_string(),
                    comment: Some("规则有冲突".to_string()),
                },
                "admin-1",
                &PublishRole::Admin,
            )
            .await
            .unwrap();

        assert_eq!(rejected.status, PublishStatus::Rejected);
        assert_eq!(rejected.review_comment.as_deref(), Some("规则有冲突"));
    }

    #[tokio::test]
    async fn test_review_permission_denied_for_head() {
        let (publish_svc, _db, _rule_svc, _ws_id) = make_services().await;

        // 科室主任尝试审批 → Forbidden
        let result = publish_svc
            .review_publish(
                999,
                ReviewPublishRequest {
                    decision: "approved".to_string(),
                    comment: None,
                },
                "head-1",
                &PublishRole::DepartmentHead,
            )
            .await;
        assert!(matches!(result, Err(WorkspaceError::Forbidden(_))));
    }

    #[tokio::test]
    async fn test_emergency_rollback_version_monotonic() {
        let (publish_svc, db, rule_svc_handle, ws_id) = make_services().await;
        let rv_id = make_candidate_rule(&rule_svc_handle.inner, &db, &ws_id, "rule-1").await;

        // v1: 发布
        let item = publish_svc
            .submit_publish(
                SubmitPublishRequest {
                    workspace_id: ws_id.clone(),
                    rule_version_ids: vec![rv_id.clone()],
                    test_report_sandbox_id: None,
                    description: None,
                },
                "head-1",
                &PublishRole::DepartmentHead,
            )
            .await
            .unwrap();
        publish_svc
            .review_publish(
                item.id,
                ReviewPublishRequest {
                    decision: "approved".to_string(),
                    comment: None,
                },
                "admin-1",
                &PublishRole::Admin,
            )
            .await
            .unwrap();

        // v2: 再发布一次 (相同规则)
        let item2 = publish_svc
            .submit_publish(
                SubmitPublishRequest {
                    workspace_id: ws_id.clone(),
                    rule_version_ids: vec![rv_id],
                    test_report_sandbox_id: None,
                    description: None,
                },
                "head-1",
                &PublishRole::DepartmentHead,
            )
            .await
            .unwrap();
        publish_svc
            .review_publish(
                item2.id,
                ReviewPublishRequest {
                    decision: "approved".to_string(),
                    comment: None,
                },
                "admin-1",
                &PublishRole::Admin,
            )
            .await
            .unwrap();

        // 现在版本是 v2, 回滚到 v1 → 新版本应为 v3 (不回退)
        let new_version = publish_svc
            .emergency_rollback(
                RollbackRequest {
                    target_version: 1,
                    reason: "v2 误触发".to_string(),
                },
                "admin-1",
                &PublishRole::Admin,
            )
            .await
            .unwrap();

        assert_eq!(new_version, 3); // 版本号单调递增, 不回退到 1

        // 回滚审计已记录
        let audit = db.get_production_audit_by_version(3).unwrap().unwrap();
        assert_eq!(audit.event_type, "ruleset_rollback");
        assert_eq!(audit.reason.as_deref(), Some("v2 误触发"));
    }

    #[tokio::test]
    async fn test_rollback_permission_denied_for_head() {
        let (publish_svc, _db, _rule_svc, _ws_id) = make_services().await;

        let result = publish_svc
            .emergency_rollback(
                RollbackRequest {
                    target_version: 1,
                    reason: "test".to_string(),
                },
                "head-1",
                &PublishRole::DepartmentHead,
            )
            .await;
        assert!(matches!(result, Err(WorkspaceError::Forbidden(_))));
    }

    #[test]
    fn test_compute_publish_ruleset_hash_deterministic() {
        let r1 = serde_json::json!({"key": "a"});
        let r2 = serde_json::json!({"key": "b"});

        let h1 = compute_publish_ruleset_hash(&[r1.clone(), r2.clone()]);
        let h2 = compute_publish_ruleset_hash(&[r1, r2]);
        assert_eq!(h1, h2);

        let h3 = compute_publish_ruleset_hash(&[serde_json::json!({"key": "a"})]);
        assert_ne!(h1, h3);
    }
}

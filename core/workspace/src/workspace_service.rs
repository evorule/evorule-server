// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Workspace 业务服务层
//!
//! 设计依据: WORKSPACE_CRATE_DESIGN.md §4
//!
//! 职责:
//! - 编排 db 层 CRUD + 状态机校验
//! - 通过 SessionOps trait 与底层 SessionManager 交互
//! - 维护 workspace / member / session 的一致性
//!
//! # 一致性约束
//! - 创建 workspace 时自动添加 owner 成员
//! - 创建 session 时校验 workspace / rule 状态,并建立 rule_session_binding
//! - 关闭 session 时关闭所有相关 binding

use std::sync::Arc;

use chrono::Utc;

use crate::db::WorkspaceDb;
use crate::error::{WorkspaceError, WorkspaceResult};
use crate::models::{
    BundleImportRecord, CreateSessionRequest, CreateWorkspaceRequest, MemberRole,
    RuleSessionBinding, RuleState, SessionBindingState, SessionRecord, UpdateWorkspaceRequest,
    WorkspaceMemberRecord, WorkspaceRecord, WorkspaceState,
};
use crate::session_bridge::SessionOps;

/// Workspace 业务服务
///
/// 持有 `Arc<WorkspaceDb>` (数据库) 和 `Arc<dyn SessionOps>` (会话桥接)。
/// 通过 `Arc` 共享,可在多个 axum handler 间克隆。
pub struct WorkspaceService {
    db: Arc<WorkspaceDb>,
    session_ops: Arc<dyn SessionOps>,
}

impl WorkspaceService {
    /// 创建新服务实例
    pub fn new(db: Arc<WorkspaceDb>, session_ops: Arc<dyn SessionOps>) -> Self {
        Self { db, session_ops }
    }

    /// 获取 db 引用 (供 RuleMetaService 共享同一 db)
    pub fn db(&self) -> &Arc<WorkspaceDb> {
        &self.db
    }

    // ========================================================================
    // Workspace CRUD
    // ========================================================================

    /// 创建工作空间
    ///
    /// 流程:
    /// 1. 生成 ULID
    /// 2. 插入 workspaces 表
    /// 3. 自动添加 owner 成员 (role=owner)
    pub async fn create_workspace(
        &self,
        req: CreateWorkspaceRequest,
    ) -> WorkspaceResult<WorkspaceRecord> {
        if req.name.trim().is_empty() {
            return Err(WorkspaceError::invalid_input("name must not be empty"));
        }
        if req.owner_id.trim().is_empty() {
            return Err(WorkspaceError::invalid_input("owner_id must not be empty"));
        }

        let now = Utc::now();
        let ws = WorkspaceRecord {
            id: ulid::Ulid::new().to_string(),
            name: req.name,
            owner_id: req.owner_id.clone(),
            created_at: now,
            updated_at: now,
            archived_at: None,
            state: WorkspaceState::Active,
            description: req.description,
        };

        // 插入 workspace
        self.db.insert_workspace(&ws)?;

        // 自动添加 owner 成员
        self.db
            .insert_member(&ws.id, &req.owner_id, MemberRole::Owner.as_str())?;

        tracing::info!(
            workspace_id = %ws.id,
            owner = %ws.owner_id,
            "workspace created"
        );
        Ok(ws)
    }

    /// 获取工作空间
    pub async fn get_workspace(&self, id: &str) -> WorkspaceResult<WorkspaceRecord> {
        self.db.get_workspace(id)
    }

    /// 列出工作空间
    ///
    /// `owner_id` 为 Some 时仅返回该 owner 的 workspace,为 None 时返回全部。
    pub async fn list_workspaces(
        &self,
        owner_id: Option<&str>,
    ) -> WorkspaceResult<Vec<WorkspaceRecord>> {
        self.db.list_workspaces(owner_id)
    }

    /// 更新工作空间 (仅 Active 状态可更新)
    pub async fn update_workspace(
        &self,
        id: &str,
        req: UpdateWorkspaceRequest,
    ) -> WorkspaceResult<WorkspaceRecord> {
        let ws = self.db.get_workspace(id)?;
        if ws.state != WorkspaceState::Active {
            return Err(WorkspaceError::InvalidStateTransition {
                from: ws.state.as_str().to_string(),
                to: "updated".to_string(),
            });
        }
        self.db
            .update_workspace(id, req.name.as_deref(), req.description.as_deref())
    }

    /// 归档工作空间
    ///
    /// 归档后 workspace 进入只读状态,不可创建新规则/会话。
    pub async fn archive_workspace(&self, id: &str) -> WorkspaceResult<WorkspaceRecord> {
        let ws = self.db.get_workspace(id)?;
        if !ws.state.can_transition_to(WorkspaceState::Archived) {
            return Err(WorkspaceError::InvalidStateTransition {
                from: ws.state.as_str().to_string(),
                to: WorkspaceState::Archived.as_str().to_string(),
            });
        }
        let archived = self.db.archive_workspace(id)?;
        tracing::info!(workspace_id = %id, "workspace archived");
        Ok(archived)
    }

    // ========================================================================
    // 成员管理
    // ========================================================================

    /// 添加成员
    ///
    /// 校验:
    /// - workspace 存在且 Active
    /// - 成员尚未存在 (UNIQUE 约束)
    pub async fn add_member(
        &self,
        workspace_id: &str,
        user_id: &str,
        role: &str,
    ) -> WorkspaceResult<WorkspaceMemberRecord> {
        let ws = self.db.get_workspace(workspace_id)?;
        if ws.state != WorkspaceState::Active {
            return Err(WorkspaceError::InvalidStateTransition {
                from: ws.state.as_str().to_string(),
                to: "member_added".to_string(),
            });
        }
        if user_id == ws.owner_id {
            return Err(WorkspaceError::invalid_input(
                "owner already exists as member",
            ));
        }
        self.db.insert_member(workspace_id, user_id, role)
    }

    /// 移除成员 (不能移除 owner)
    pub async fn remove_member(&self, workspace_id: &str, user_id: &str) -> WorkspaceResult<()> {
        self.db.delete_member(workspace_id, user_id)
    }

    /// 自由加入(幂等):非成员则以 viewer 角色加入,已是成员(含 owner)直接成功。
    ///
    /// 设计(用户裁定方案 A——自由加入):
    /// - 角色固定 viewer 最小权限:沙盒族端点只校验「是成员」不查角色,
    ///   viewer 即可解锁全部沙盒操作;admin/editor 的提权仍走 add_member
    ///   显式授权路径,join 不放大权限。
    /// - 幂等语义:重复点击「加入」不报错,返回是否本次实际加入。
    /// - 身份由 server 层统一认证中间件注入(AuthedActor),不信任前端自报。
    pub async fn join_workspace(&self, workspace_id: &str, user_id: &str) -> WorkspaceResult<bool> {
        let ws = self.db.get_workspace(workspace_id)?;
        if ws.state != WorkspaceState::Active {
            return Err(WorkspaceError::InvalidStateTransition {
                from: ws.state.as_str().to_string(),
                to: "member_joined".to_string(),
            });
        }
        // owner 天然是成员(创建时自动加入),重复 join 幂等收敛
        if user_id == ws.owner_id {
            return Ok(false);
        }
        if self.db.is_workspace_member(workspace_id, user_id)? {
            return Ok(false);
        }
        self.db.insert_member(workspace_id, user_id, "viewer")?;
        tracing::info!(workspace_id = %workspace_id, user_id = %user_id, "workspace member joined (self-service)");
        Ok(true)
    }

    /// 列出成员
    pub async fn list_members(
        &self,
        workspace_id: &str,
    ) -> WorkspaceResult<Vec<WorkspaceMemberRecord>> {
        // 先校验 workspace 存在 (返回 404 而非空列表)
        self.db.get_workspace(workspace_id)?;
        self.db.list_members(workspace_id)
    }

    // ========================================================================
    // 会话管理
    // ========================================================================

    /// 创建会话
    ///
    /// 流程:
    /// 1. 校验 workspace 存在且 Active
    /// 2. 如果指定 rule_id:
    ///    a. 校验 rule 存在且状态为 Active
    ///    b. 解析 rule_version_id (指定则用指定的,否则用 current_version_id)
    /// 3. 调用 session_ops.create_session 获取 session_id
    /// 4. 在 db 中记录 session
    /// 5. 如果有 rule_version_id,创建 binding
    pub async fn create_session(
        &self,
        workspace_id: &str,
        req: CreateSessionRequest,
    ) -> WorkspaceResult<SessionRecord> {
        // 1. 校验 workspace
        let ws = self.db.get_workspace(workspace_id)?;
        if ws.state != WorkspaceState::Active {
            return Err(WorkspaceError::InvalidStateTransition {
                from: ws.state.as_str().to_string(),
                to: "session_created".to_string(),
            });
        }

        // 2. 校验 rule (如果指定)
        let mut bound_rule_id: Option<String> = None;
        let mut bound_rule_version_id: Option<String> = None;
        if let Some(rule_id) = &req.rule_id {
            let rule = self.db.get_rule(rule_id)?;
            if rule.workspace_id != workspace_id {
                return Err(WorkspaceError::invalid_input(format!(
                    "rule {rule_id} does not belong to workspace {workspace_id}"
                )));
            }
            if rule.state != RuleState::Active {
                return Err(WorkspaceError::InvalidStateTransition {
                    from: rule.state.as_str().to_string(),
                    to: "session_bound".to_string(),
                });
            }
            bound_rule_id = Some(rule.id.clone());

            // 解析 rule_version_id
            let rv_id = if let Some(v) = &req.rule_version_id {
                // 校验指定版本存在且属于此 rule
                let rv = self.db.get_rule_version(v)?;
                if rv.rule_id != rule.id {
                    return Err(WorkspaceError::invalid_input(format!(
                        "rule_version {v} does not belong to rule {rule_id}"
                    )));
                }
                rv.id
            } else if let Some(cv_id) = &rule.current_version_id {
                // 用 current_version
                cv_id.clone()
            } else {
                return Err(WorkspaceError::invalid_input(format!(
                    "rule {rule_id} has no current_version; specify rule_version_id"
                )));
            };
            bound_rule_version_id = Some(rv_id);
        }

        // 3. 调用 session_ops 创建底层会话
        let session_id = self.session_ops.create_session().await?;

        // 4. 记录到 db
        let now = Utc::now();
        let session = SessionRecord {
            id: session_id,
            workspace_id: workspace_id.to_string(),
            rule_id: bound_rule_id.clone(),
            rule_version_id: bound_rule_version_id.clone(),
            created_at: now,
            closed_at: None,
            created_by: req.created_by,
        };
        // 如果 insert_session 失败,需要回滚 session_ops 创建的会话
        if let Err(e) = self.db.insert_session(&session) {
            // 尽力回滚 (忽略关闭错误)
            let _ = self.session_ops.close_session(session_id).await;
            return Err(e);
        }

        // 5. 创建 binding (如果有 rule_version)
        if let Some(rv_id) = &bound_rule_version_id {
            let binding = RuleSessionBinding {
                id: ulid::Ulid::new().to_string(),
                rule_version_id: rv_id.clone(),
                session_id,
                workspace_id: workspace_id.to_string(),
                bound_at: now,
                unbound_at: None,
                state: SessionBindingState::Bound,
            };
            if let Err(e) = self.db.insert_binding(&binding) {
                tracing::warn!(
                    error = %e,
                    session_id,
                    "failed to create rule_session_binding (session still created)"
                );
            }
        }

        tracing::info!(
            workspace_id = %workspace_id,
            session_id,
            rule_id = ?bound_rule_id,
            "session created"
        );
        Ok(session)
    }

    /// 列出 workspace 下的会话
    pub async fn list_sessions(&self, workspace_id: &str) -> WorkspaceResult<Vec<SessionRecord>> {
        // 校验 workspace 存在
        self.db.get_workspace(workspace_id)?;
        self.db.list_sessions_by_workspace(workspace_id)
    }

    /// 关闭会话
    ///
    /// 流程:
    /// 1. 从 db 查询 session,校验存在且未关闭
    /// 2. 调用 session_ops.close_session 关闭底层会话
    /// 3. 在 db 中标记 session 关闭
    /// 4. 关闭所有相关 binding
    pub async fn close_session(&self, session_id: u64) -> WorkspaceResult<SessionRecord> {
        // 1. 查询 session
        let session = self.db.get_session(session_id)?;
        if session.closed_at.is_some() {
            return Err(WorkspaceError::InvalidStateTransition {
                from: "closed".to_string(),
                to: "closed".to_string(),
            });
        }

        // 2. 关闭底层会话
        if let Err(e) = self.session_ops.close_session(session_id).await {
            tracing::warn!(
                error = %e,
                session_id,
                "session_ops.close_session failed (marking closed in db anyway)"
            );
        }

        // 3. db 中标记关闭
        let closed = self.db.close_session(session_id)?;

        // 4. 关闭相关 binding
        self.close_session_bindings(session_id)?;

        tracing::info!(session_id, "session closed");
        Ok(closed)
    }

    /// 关闭 session 下所有相关 binding (幂等, 单个失败仅告警)
    fn close_session_bindings(&self, session_id: u64) -> WorkspaceResult<()> {
        let bindings = self.db.list_bindings_by_session(session_id)?;
        for b in bindings {
            if b.state == SessionBindingState::Bound {
                if let Err(e) = self.db.close_binding(&b.id) {
                    tracing::warn!(
                        error = %e,
                        binding_id = %b.id,
                        "failed to close binding"
                    );
                }
            }
        }
        Ok(())
    }

    /// 记录一次 bundle 导入溯源
    ///
    /// 委托 db 层写入 `bundle_imports`；`imported_at` 由 db 层以墙钟生成 (管理元数据, 旁路)。
    #[allow(clippy::too_many_arguments)]
    pub fn record_bundle_import(
        &self,
        bundle_id: &str,
        dataset_id: &str,
        source_version: &str,
        selection_mode: &str,
        resolved_version: Option<&str>,
        content_hash: &str,
        entry_count: i64,
        imported_by: &str,
    ) -> WorkspaceResult<i64> {
        self.db.insert_bundle_import(
            bundle_id,
            dataset_id,
            source_version,
            selection_mode,
            resolved_version,
            content_hash,
            entry_count,
            imported_by,
        )
    }

    /// 列出 bundle 导入溯源记录 (按导入时间倒序, 限制条数)
    pub fn list_bundle_imports(&self, limit: i64) -> WorkspaceResult<Vec<BundleImportRecord>> {
        self.db.list_bundle_imports(limit)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::db::WorkspaceDb;
    use async_trait::async_trait;
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    /// 测试用 SessionOps 桩实现
    struct MockSessionOps {
        next_id: AtomicU64,
        closed: Mutex<Vec<u64>>,
        commands: Mutex<Vec<(u64, Value)>>,
    }

    impl MockSessionOps {
        fn new() -> Self {
            Self {
                next_id: AtomicU64::new(1000),
                closed: Mutex::new(Vec::new()),
                commands: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl SessionOps for MockSessionOps {
        async fn create_session(&self) -> WorkspaceResult<u64> {
            Ok(self.next_id.fetch_add(1, Ordering::SeqCst))
        }
        async fn fork_session(&self, parent: u64) -> WorkspaceResult<u64> {
            // 模拟 fork: 分配新 id
            let _ = parent;
            Ok(self.next_id.fetch_add(1, Ordering::SeqCst))
        }
        async fn session_exists(&self, id: u64) -> bool {
            id < self.next_id.load(Ordering::SeqCst) && !self.closed.lock().unwrap().contains(&id)
        }
        async fn close_session(&self, id: u64) -> WorkspaceResult<()> {
            self.closed.lock().unwrap().push(id);
            Ok(())
        }
        async fn list_sessions(&self) -> WorkspaceResult<Vec<u64>> {
            Ok(Vec::new())
        }
        async fn send_command(&self, id: u64, cmd: Value) -> WorkspaceResult<u64> {
            self.commands.lock().unwrap().push((id, cmd));
            Ok(42)
        }
        async fn get_session_state(&self, _id: u64) -> WorkspaceResult<Value> {
            Ok(json!({"payload": {}, "version": 0}))
        }
        async fn get_audit_report(&self, _id: u64) -> WorkspaceResult<Value> {
            Ok(json!({"entry_count": 0, "verified": true}))
        }
        async fn get_audit_export(&self, _id: u64) -> WorkspaceResult<String> {
            Ok("[]".to_string())
        }
        async fn get_facts(&self, _id: u64) -> WorkspaceResult<Vec<Value>> {
            Ok(Vec::new())
        }
        async fn get_causal_chain(&self, _id: u64, _fact_id: u64) -> WorkspaceResult<Value> {
            Ok(json!([]))
        }
        async fn reload_rules(&self) -> WorkspaceResult<()> {
            Ok(())
        }
        async fn flush_audit(&self, _id: u64) -> WorkspaceResult<usize> {
            Ok(0)
        }
    }

    fn make_service() -> (WorkspaceService, std::sync::Arc<MockSessionOps>) {
        let db = Arc::new(WorkspaceDb::in_memory().unwrap());
        let ops = Arc::new(MockSessionOps::new());
        let svc = WorkspaceService::new(db, ops.clone());
        (svc, ops)
    }

    fn make_workspace_sync(svc: &WorkspaceService, name: &str, owner: &str) -> WorkspaceRecord {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(svc.create_workspace(CreateWorkspaceRequest {
            name: name.to_string(),
            owner_id: owner.to_string(),
            description: None,
        }))
        .unwrap()
    }

    #[test]
    fn test_create_workspace_adds_owner_member() {
        let (svc, _ops) = make_service();
        let ws = make_workspace_sync(&svc, "team", "user-1");

        let rt = tokio::runtime::Runtime::new().unwrap();
        let members = rt.block_on(svc.list_members(&ws.id)).unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].user_id, "user-1");
        assert_eq!(members[0].role, "owner");
    }

    #[test]
    fn test_create_workspace_rejects_empty_name() {
        let (svc, _ops) = make_service();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(svc.create_workspace(CreateWorkspaceRequest {
                name: "".to_string(),
                owner_id: "user-1".to_string(),
                description: None,
            }))
            .unwrap_err();
        assert!(matches!(err, WorkspaceError::InvalidInput(_)));
    }

    #[test]
    fn test_archive_workspace() {
        let (svc, _ops) = make_service();
        let ws = make_workspace_sync(&svc, "team", "user-1");

        let rt = tokio::runtime::Runtime::new().unwrap();
        let archived = rt.block_on(svc.archive_workspace(&ws.id)).unwrap();
        assert_eq!(archived.state, WorkspaceState::Archived);

        // 归档后再归档应失败
        let err = rt.block_on(svc.archive_workspace(&ws.id)).unwrap_err();
        assert!(matches!(err, WorkspaceError::InvalidStateTransition { .. }));
    }

    #[test]
    fn test_add_and_remove_member() {
        let (svc, _ops) = make_service();
        let ws = make_workspace_sync(&svc, "team", "owner-1");

        let rt = tokio::runtime::Runtime::new().unwrap();
        let m = rt
            .block_on(svc.add_member(&ws.id, "user-2", "editor"))
            .unwrap();
        assert_eq!(m.role, "editor");

        // 不能重复添加
        let err = rt
            .block_on(svc.add_member(&ws.id, "user-2", "editor"))
            .unwrap_err();
        assert!(matches!(err, WorkspaceError::AlreadyExists { .. }));

        // 不能添加 owner 为成员
        let err = rt
            .block_on(svc.add_member(&ws.id, "owner-1", "editor"))
            .unwrap_err();
        assert!(matches!(err, WorkspaceError::InvalidInput(_)));

        // 移除成员
        rt.block_on(svc.remove_member(&ws.id, "user-2")).unwrap();

        // 不能移除 owner
        let err = rt
            .block_on(svc.remove_member(&ws.id, "owner-1"))
            .unwrap_err();
        assert!(matches!(err, WorkspaceError::InvalidInput(_)));
    }

    /// 自由加入锁定:幂等(viewer 最小权限,重复 join 收敛,owner 天然成员)
    #[test]
    fn test_join_workspace_idempotent_viewer() {
        let (svc, _ops) = make_service();
        let ws = make_workspace_sync(&svc, "team", "owner-1");

        let rt = tokio::runtime::Runtime::new().unwrap();

        // 首次加入:joined=true,角色 viewer
        let joined = rt.block_on(svc.join_workspace(&ws.id, "user-2")).unwrap();
        assert!(joined);
        let members = rt.block_on(svc.list_members(&ws.id)).unwrap();
        let m = members.iter().find(|m| m.user_id == "user-2").unwrap();
        assert_eq!(m.role, "viewer");

        // 重复加入:幂等收敛(joined=false,不撞 UNIQUE 报错)
        let joined_again = rt.block_on(svc.join_workspace(&ws.id, "user-2")).unwrap();
        assert!(!joined_again);

        // owner join:天然成员,幂等 false
        let owner_join = rt.block_on(svc.join_workspace(&ws.id, "owner-1")).unwrap();
        assert!(!owner_join);
    }

    #[test]
    fn test_create_session_without_rule() {
        let (svc, _ops) = make_service();
        let ws = make_workspace_sync(&svc, "team", "owner-1");

        let rt = tokio::runtime::Runtime::new().unwrap();
        let session = rt
            .block_on(svc.create_session(
                &ws.id,
                CreateSessionRequest {
                    rule_id: None,
                    rule_version_id: None,
                    created_by: "owner-1".to_string(),
                },
            ))
            .unwrap();

        assert_eq!(session.workspace_id, ws.id);
        assert!(session.rule_id.is_none());
        assert!(session.rule_version_id.is_none());

        // list
        let list = rt.block_on(svc.list_sessions(&ws.id)).unwrap();
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn test_close_session() {
        let (svc, ops) = make_service();
        let ws = make_workspace_sync(&svc, "team", "owner-1");

        let rt = tokio::runtime::Runtime::new().unwrap();
        let session = rt
            .block_on(svc.create_session(
                &ws.id,
                CreateSessionRequest {
                    rule_id: None,
                    rule_version_id: None,
                    created_by: "owner-1".to_string(),
                },
            ))
            .unwrap();

        let closed = rt.block_on(svc.close_session(session.id)).unwrap();
        assert!(closed.closed_at.is_some());

        // 验证 session_ops.close_session 被调用
        let closed_ids = ops.closed.lock().unwrap();
        assert!(closed_ids.contains(&session.id));

        // 重复关闭应失败
        drop(closed_ids);
        let err = rt.block_on(svc.close_session(session.id)).unwrap_err();
        assert!(matches!(err, WorkspaceError::InvalidStateTransition { .. }));
    }

    #[test]
    fn test_archived_workspace_rejects_session() {
        let (svc, _ops) = make_service();
        let ws = make_workspace_sync(&svc, "team", "owner-1");

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(svc.archive_workspace(&ws.id)).unwrap();

        let err = rt
            .block_on(svc.create_session(
                &ws.id,
                CreateSessionRequest {
                    rule_id: None,
                    rule_version_id: None,
                    created_by: "owner-1".to_string(),
                },
            ))
            .unwrap_err();
        assert!(matches!(err, WorkspaceError::InvalidStateTransition { .. }));
    }
}

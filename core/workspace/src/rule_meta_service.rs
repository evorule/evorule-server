// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 规则元数据服务层
//!
//! 设计依据: WORKSPACE_CRATE_DESIGN.md §5
//!
//! 职责:
//! - 规则 CRUD + 版本管理
//! - 状态机校验 (Draft → Candidate → Active ↔ Blocked; * → Archived)
//! - 规则内容 BLAKE3 哈希 (用于去重和审计)
//! - 规则 fork (基于现有规则创建副本)

use std::sync::Arc;

use chrono::Utc;

use crate::db::WorkspaceDb;
use crate::error::{WorkspaceError, WorkspaceResult};
use crate::models::{
    CreateRuleRequest, RuleRecord, RuleState, RuleVersionRecord, RuleVersionState,
    UpdateRuleContentRequest, WorkspaceState,
};

/// 规则元数据服务
pub struct RuleMetaService {
    db: Arc<WorkspaceDb>,
}

impl RuleMetaService {
    /// 创建新服务实例
    pub fn new(db: Arc<WorkspaceDb>) -> Self {
        Self { db }
    }

    /// 计算规则内容的 BLAKE3 哈希
    ///
    /// 用于:
    /// - 版本去重 (相同内容哈希相同)
    /// - 审计追踪 (内容不可篡改)
    fn compute_content_hash(content: &str) -> String {
        let hash = blake3::hash(content.as_bytes());
        hash.to_hex().to_string()
    }

    /// 校验规则内容是否为合法 JSON
    fn validate_content(content: &str) -> WorkspaceResult<()> {
        serde_json::from_str::<serde_json::Value>(content)
            .map_err(|e| WorkspaceError::invalid_input(format!("invalid rule JSON: {e}")))?;
        Ok(())
    }

    // ========================================================================
    // 规则 CRUD
    // ========================================================================

    /// 创建规则
    ///
    /// 流程:
    /// 1. 校验 workspace 存在且 Active
    /// 2. 校验 rule 内容是合法 JSON
    /// 3. 计算 BLAKE3 哈希
    /// 4. 插入 rules 表 (state=Draft)
    /// 5. 插入 rule_versions 表 (version=1, state=Current)
    /// 6. 更新 rules.current_version_id
    pub async fn create_rule(
        &self,
        workspace_id: &str,
        req: CreateRuleRequest,
    ) -> WorkspaceResult<RuleRecord> {
        if req.name.trim().is_empty() {
            return Err(WorkspaceError::invalid_input("rule name must not be empty"));
        }
        if req.created_by.trim().is_empty() {
            return Err(WorkspaceError::invalid_input(
                "created_by must not be empty",
            ));
        }

        // 1. 校验 workspace
        let ws = self.db.get_workspace(workspace_id)?;
        if ws.state != WorkspaceState::Active {
            return Err(WorkspaceError::InvalidStateTransition {
                from: ws.state.as_str().to_string(),
                to: "rule_created".to_string(),
            });
        }

        // 2. 校验内容
        Self::validate_content(&req.content)?;

        // 3. 计算哈希
        let content_hash = Self::compute_content_hash(&req.content);

        // 4. 插入 rule
        let now = Utc::now();
        let rule_id = ulid::Ulid::new().to_string();
        let rule = RuleRecord {
            id: rule_id.clone(),
            workspace_id: workspace_id.to_string(),
            name: req.name,
            current_version_id: None, // 稍后更新
            state: RuleState::Draft,
            created_at: now,
            updated_at: now,
            archived_at: None,
            description: req.description,
            created_by: req.created_by.clone(),
            metadata: "{}".to_string(),
        };
        self.db.insert_rule(&rule)?;

        // 5. 插入初始版本 (version 1)
        let version_id = ulid::Ulid::new().to_string();
        let rv = RuleVersionRecord {
            id: version_id.clone(),
            rule_id: rule_id.clone(),
            version: 1,
            content_hash,
            content: req.content,
            created_at: now,
            state: RuleVersionState::Current,
            created_by: req.created_by,
        };
        self.db.insert_rule_version(&rv)?;

        // 6. 更新 current_version_id
        self.db.update_rule_current_version(&rule_id, &version_id)?;

        // 重新查询返回完整记录
        let rule = self.db.get_rule(&rule_id)?;
        tracing::info!(
            workspace_id = %workspace_id,
            rule_id = %rule.id,
            "rule created"
        );
        Ok(rule)
    }

    /// 获取规则
    ///
    /// 校验 rule 属于指定 workspace。
    pub async fn get_rule(&self, workspace_id: &str, rule_id: &str) -> WorkspaceResult<RuleRecord> {
        let rule = self.db.get_rule(rule_id)?;
        if rule.workspace_id != workspace_id {
            return Err(WorkspaceError::not_found("rule", rule_id));
        }
        Ok(rule)
    }

    /// 列出 workspace 下的规则
    pub async fn list_rules(&self, workspace_id: &str) -> WorkspaceResult<Vec<RuleRecord>> {
        // 校验 workspace 存在
        self.db.get_workspace(workspace_id)?;
        self.db.list_rules(workspace_id)
    }

    /// 更新规则内容 (仅 Draft 状态允许)
    ///
    /// 流程:
    /// 1. 校验 rule 存在 + workspace 一致 + 状态为 Draft
    /// 2. 校验新内容是合法 JSON
    /// 3. 计算新哈希
    /// 4. 旧版本标记为 superseded
    /// 5. 插入新版本 (version+1, state=Current)
    /// 6. 更新 rules.current_version_id
    pub async fn update_rule_content(
        &self,
        workspace_id: &str,
        rule_id: &str,
        req: UpdateRuleContentRequest,
    ) -> WorkspaceResult<RuleVersionRecord> {
        // 1. 校验
        let rule = self.get_rule(workspace_id, rule_id).await?;
        if !rule.state.is_editable() {
            return Err(WorkspaceError::InvalidStateTransition {
                from: rule.state.as_str().to_string(),
                to: "content_updated".to_string(),
            });
        }

        // 2. 校验内容
        Self::validate_content(&req.content)?;

        // 3. 计算哈希
        let new_hash = Self::compute_content_hash(&req.content);

        // 4. 旧版本标记 superseded
        if let Some(old_v_id) = &rule.current_version_id {
            self.db.mark_version_superseded(old_v_id)?;
        }

        // 5. 插入新版本
        let next_version = self.db.get_next_version_number(rule_id)?;
        let now = Utc::now();
        let new_version_id = ulid::Ulid::new().to_string();
        let new_rv = RuleVersionRecord {
            id: new_version_id.clone(),
            rule_id: rule_id.to_string(),
            version: next_version,
            content_hash: new_hash,
            content: req.content,
            created_at: now,
            state: RuleVersionState::Current,
            created_by: req.updated_by,
        };
        self.db.insert_rule_version(&new_rv)?;

        // 6. 更新 current_version_id
        self.db
            .update_rule_current_version(rule_id, &new_version_id)?;

        tracing::info!(
            rule_id = %rule_id,
            version = next_version,
            "rule content updated"
        );
        Ok(new_rv)
    }

    // ========================================================================
    // 状态机操作
    // ========================================================================

    /// 激活规则 (便捷方法)
    ///
    /// 根据当前状态自动执行迁移:
    /// - Draft → Candidate → Active (两步)
    /// - Candidate → Active (一步)
    /// - Blocked → Active (一步)
    /// - Active → 幂等返回
    /// - Archived → 报错
    pub async fn activate_rule(
        &self,
        workspace_id: &str,
        rule_id: &str,
    ) -> WorkspaceResult<RuleRecord> {
        let rule = self.get_rule(workspace_id, rule_id).await?;
        match rule.state {
            RuleState::Draft => {
                // Draft → Candidate
                let candidate = self.transition_state(rule_id, RuleState::Candidate)?;
                // Candidate → Active
                self.transition_state(rule_id, RuleState::Active)
                    .or(Ok(candidate))
            }
            RuleState::Candidate => self.transition_state(rule_id, RuleState::Active),
            RuleState::Blocked => self.transition_state(rule_id, RuleState::Active),
            RuleState::Active => Ok(rule),
            RuleState::Archived => Err(WorkspaceError::InvalidStateTransition {
                from: rule.state.as_str().to_string(),
                to: RuleState::Active.as_str().to_string(),
            }),
        }
    }

    /// 提交候选 (Draft → Candidate)
    pub async fn submit_rule(
        &self,
        workspace_id: &str,
        rule_id: &str,
    ) -> WorkspaceResult<RuleRecord> {
        let rule = self.get_rule(workspace_id, rule_id).await?;
        if !rule.state.can_transition_to(RuleState::Candidate) {
            return Err(WorkspaceError::InvalidStateTransition {
                from: rule.state.as_str().to_string(),
                to: RuleState::Candidate.as_str().to_string(),
            });
        }
        self.transition_state(rule_id, RuleState::Candidate)
    }

    /// 阻塞规则 (Active → Blocked)
    pub async fn block_rule(
        &self,
        workspace_id: &str,
        rule_id: &str,
    ) -> WorkspaceResult<RuleRecord> {
        let rule = self.get_rule(workspace_id, rule_id).await?;
        if !rule.state.can_transition_to(RuleState::Blocked) {
            return Err(WorkspaceError::InvalidStateTransition {
                from: rule.state.as_str().to_string(),
                to: RuleState::Blocked.as_str().to_string(),
            });
        }
        self.transition_state(rule_id, RuleState::Blocked)
    }

    /// 归档规则 (* → Archived)
    pub async fn archive_rule(
        &self,
        workspace_id: &str,
        rule_id: &str,
    ) -> WorkspaceResult<RuleRecord> {
        let rule = self.get_rule(workspace_id, rule_id).await?;
        if !rule.state.can_transition_to(RuleState::Archived) {
            return Err(WorkspaceError::InvalidStateTransition {
                from: rule.state.as_str().to_string(),
                to: RuleState::Archived.as_str().to_string(),
            });
        }
        let archived = self.db.archive_rule(rule_id)?;
        tracing::info!(rule_id = %rule_id, "rule archived");
        Ok(archived)
    }

    /// 内部状态迁移辅助
    fn transition_state(&self, rule_id: &str, target: RuleState) -> WorkspaceResult<RuleRecord> {
        self.db.update_rule_state(rule_id, target)
    }

    // ========================================================================
    // 规则版本查询
    // ========================================================================

    /// 列出规则的所有版本 (按版本号降序)
    pub async fn list_rule_versions(
        &self,
        workspace_id: &str,
        rule_id: &str,
    ) -> WorkspaceResult<Vec<RuleVersionRecord>> {
        // 校验 rule 属于指定 workspace
        self.get_rule(workspace_id, rule_id).await?;
        self.db.list_rule_versions(rule_id)
    }

    /// 获取规则的指定版本
    pub async fn get_rule_version(
        &self,
        workspace_id: &str,
        rule_id: &str,
        version_id: &str,
    ) -> WorkspaceResult<RuleVersionRecord> {
        // 校验 rule 属于指定 workspace
        self.get_rule(workspace_id, rule_id).await?;
        let rv = self.db.get_rule_version(version_id)?;
        if rv.rule_id != rule_id {
            return Err(WorkspaceError::not_found("rule_version", version_id));
        }
        Ok(rv)
    }

    // ========================================================================
    // 规则 Fork
    // ========================================================================

    /// Fork 规则
    ///
    /// 基于现有规则创建新规则:
    /// - 新规则 name = new_name
    /// - 复制源规则当前版本内容
    /// - 新规则状态 = Draft
    /// - 新规则 version = 1 (独立版本历史)
    pub async fn fork_rule(
        &self,
        workspace_id: &str,
        source_rule_id: &str,
        new_name: &str,
        created_by: &str,
    ) -> WorkspaceResult<RuleRecord> {
        if new_name.trim().is_empty() {
            return Err(WorkspaceError::invalid_input("new_name must not be empty"));
        }

        // 校验源规则
        let source = self.get_rule(workspace_id, source_rule_id).await?;

        // 获取源规则当前版本内容
        let source_version = self
            .db
            .get_current_version(source_rule_id)?
            .ok_or_else(|| WorkspaceError::internal("source rule has no current version"))?;

        // 创建新规则 (复用 create_rule 逻辑)
        let new_rule = self
            .create_rule(
                workspace_id,
                CreateRuleRequest {
                    name: new_name.to_string(),
                    content: source_version.content,
                    created_by: created_by.to_string(),
                    description: source.description.clone(),
                },
            )
            .await?;

        tracing::info!(
            source_rule_id = %source_rule_id,
            new_rule_id = %new_rule.id,
            "rule forked"
        );
        Ok(new_rule)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::db::WorkspaceDb;
    use crate::models::CreateWorkspaceRequest;
    use crate::workspace_service::WorkspaceService;

    fn make_services() -> (RuleMetaService, WorkspaceService) {
        let db = Arc::new(WorkspaceDb::in_memory().unwrap());
        // 测试用 SessionOps 桩
        struct StubOps;
        #[async_trait::async_trait]
        impl crate::session_bridge::SessionOps for StubOps {
            async fn create_session(&self) -> WorkspaceResult<u64> {
                Ok(1)
            }
            async fn fork_session(&self, _: u64) -> WorkspaceResult<u64> {
                Ok(2)
            }
            async fn session_exists(&self, _: u64) -> bool {
                true
            }
            async fn close_session(&self, _: u64) -> WorkspaceResult<()> {
                Ok(())
            }
            async fn list_sessions(&self) -> WorkspaceResult<Vec<u64>> {
                Ok(vec![])
            }
            async fn send_command(&self, _: u64, _: serde_json::Value) -> WorkspaceResult<u64> {
                Ok(0)
            }
            async fn get_session_state(&self, _: u64) -> WorkspaceResult<serde_json::Value> {
                Ok(serde_json::json!({}))
            }
            async fn get_audit_report(&self, _: u64) -> WorkspaceResult<serde_json::Value> {
                Ok(serde_json::json!({"entry_count": 0, "verified": true}))
            }
            async fn get_audit_export(&self, _: u64) -> WorkspaceResult<String> {
                Ok("[]".to_string())
            }
            async fn get_facts(&self, _: u64) -> WorkspaceResult<Vec<serde_json::Value>> {
                Ok(Vec::new())
            }
            async fn get_causal_chain(&self, _: u64, _: u64) -> WorkspaceResult<serde_json::Value> {
                Ok(serde_json::json!([]))
            }
            async fn reload_rules(&self) -> WorkspaceResult<()> {
                Ok(())
            }
            async fn flush_audit(&self, _: u64) -> WorkspaceResult<usize> {
                Ok(0)
            }
        }
        let ops: Arc<dyn crate::session_bridge::SessionOps> = Arc::new(StubOps);
        let ws_svc = WorkspaceService::new(db.clone(), ops);
        let rule_svc = RuleMetaService::new(db);
        (rule_svc, ws_svc)
    }

    fn make_workspace(ws_svc: &WorkspaceService) -> String {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ws = rt
            .block_on(ws_svc.create_workspace(CreateWorkspaceRequest {
                name: "team".to_string(),
                owner_id: "owner-1".to_string(),
                description: None,
            }))
            .unwrap();
        ws.id
    }

    fn make_rule(rule_svc: &RuleMetaService, ws_id: &str, name: &str) -> RuleRecord {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(rule_svc.create_rule(
            ws_id,
            CreateRuleRequest {
                name: name.to_string(),
                content: r#"{"transform":[{"type":"noop"}]}"#.to_string(),
                created_by: "owner-1".to_string(),
                description: None,
            },
        ))
        .unwrap()
    }

    #[test]
    fn test_create_rule_initial_version() {
        let (rule_svc, ws_svc) = make_services();
        let ws_id = make_workspace(&ws_svc);
        let rule = make_rule(&rule_svc, &ws_id, "rule-1");

        assert_eq!(rule.state, RuleState::Draft);
        assert!(rule.current_version_id.is_some());

        // 验证版本
        let rt = tokio::runtime::Runtime::new().unwrap();
        let versions = rt
            .block_on(rule_svc.list_rule_versions(&ws_id, &rule.id))
            .unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].version, 1);
        assert_eq!(versions[0].state, RuleVersionState::Current);
        assert!(!versions[0].content_hash.is_empty());
    }

    #[test]
    fn test_blake3_hash_deterministic() {
        let content = r#"{"transform":[{"type":"noop"}]}"#;
        let h1 = RuleMetaService::compute_content_hash(content);
        let h2 = RuleMetaService::compute_content_hash(content);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // BLAKE3 hex = 32 bytes = 64 chars

        // 不同内容不同哈希
        let h3 = RuleMetaService::compute_content_hash(r#"{"transform":[]}"#);
        assert_ne!(h1, h3);
    }

    #[test]
    fn test_update_rule_content_creates_new_version() {
        let (rule_svc, ws_svc) = make_services();
        let ws_id = make_workspace(&ws_svc);
        let rule = make_rule(&rule_svc, &ws_id, "rule-1");

        let rt = tokio::runtime::Runtime::new().unwrap();
        let new_content = r#"{"transform":[{"type":"increment","params":{"attr":"x","delta":1}}]}"#;
        let new_rv = rt
            .block_on(rule_svc.update_rule_content(
                &ws_id,
                &rule.id,
                UpdateRuleContentRequest {
                    content: new_content.to_string(),
                    updated_by: "owner-1".to_string(),
                },
            ))
            .unwrap();

        assert_eq!(new_rv.version, 2);
        assert_eq!(new_rv.state, RuleVersionState::Current);

        // 旧版本应被 superseded
        let versions = rt
            .block_on(rule_svc.list_rule_versions(&ws_id, &rule.id))
            .unwrap();
        assert_eq!(versions.len(), 2);
        let v1 = versions.iter().find(|v| v.version == 1).unwrap();
        assert_eq!(v1.state, RuleVersionState::Superseded);

        // rule 的 current_version_id 应更新
        let rule_after = rt.block_on(rule_svc.get_rule(&ws_id, &rule.id)).unwrap();
        assert_eq!(
            rule_after.current_version_id.as_deref(),
            Some(new_rv.id.as_str())
        );
    }

    #[test]
    fn test_update_rule_rejects_non_draft() {
        let (rule_svc, ws_svc) = make_services();
        let ws_id = make_workspace(&ws_svc);
        let rule = make_rule(&rule_svc, &ws_id, "rule-1");

        let rt = tokio::runtime::Runtime::new().unwrap();
        // 激活规则
        rt.block_on(rule_svc.activate_rule(&ws_id, &rule.id))
            .unwrap();

        // Active 状态不允许更新
        let err = rt
            .block_on(rule_svc.update_rule_content(
                &ws_id,
                &rule.id,
                UpdateRuleContentRequest {
                    content: r#"{"transform":[]}"#.to_string(),
                    updated_by: "owner-1".to_string(),
                },
            ))
            .unwrap_err();
        assert!(matches!(err, WorkspaceError::InvalidStateTransition { .. }));
    }

    #[test]
    fn test_activate_rule_from_draft() {
        let (rule_svc, ws_svc) = make_services();
        let ws_id = make_workspace(&ws_svc);
        let rule = make_rule(&rule_svc, &ws_id, "rule-1");

        let rt = tokio::runtime::Runtime::new().unwrap();
        let active = rt
            .block_on(rule_svc.activate_rule(&ws_id, &rule.id))
            .unwrap();
        assert_eq!(active.state, RuleState::Active);

        // 幂等: 再次激活
        let active2 = rt
            .block_on(rule_svc.activate_rule(&ws_id, &rule.id))
            .unwrap();
        assert_eq!(active2.state, RuleState::Active);
    }

    #[test]
    fn test_block_and_unblock_rule() {
        let (rule_svc, ws_svc) = make_services();
        let ws_id = make_workspace(&ws_svc);
        let rule = make_rule(&rule_svc, &ws_id, "rule-1");

        let rt = tokio::runtime::Runtime::new().unwrap();
        // 先激活
        rt.block_on(rule_svc.activate_rule(&ws_id, &rule.id))
            .unwrap();

        // 阻塞
        let blocked = rt.block_on(rule_svc.block_rule(&ws_id, &rule.id)).unwrap();
        assert_eq!(blocked.state, RuleState::Blocked);

        // 恢复 (activate 会从 Blocked -> Active)
        let active = rt
            .block_on(rule_svc.activate_rule(&ws_id, &rule.id))
            .unwrap();
        assert_eq!(active.state, RuleState::Active);
    }

    #[test]
    fn test_archive_rule() {
        let (rule_svc, ws_svc) = make_services();
        let ws_id = make_workspace(&ws_svc);
        let rule = make_rule(&rule_svc, &ws_id, "rule-1");

        let rt = tokio::runtime::Runtime::new().unwrap();
        let archived = rt
            .block_on(rule_svc.archive_rule(&ws_id, &rule.id))
            .unwrap();
        assert_eq!(archived.state, RuleState::Archived);

        // 归档后不能激活
        let err = rt
            .block_on(rule_svc.activate_rule(&ws_id, &rule.id))
            .unwrap_err();
        assert!(matches!(err, WorkspaceError::InvalidStateTransition { .. }));
    }

    #[test]
    fn test_fork_rule() {
        let (rule_svc, ws_svc) = make_services();
        let ws_id = make_workspace(&ws_svc);
        let rule = make_rule(&rule_svc, &ws_id, "rule-1");

        let rt = tokio::runtime::Runtime::new().unwrap();
        let forked = rt
            .block_on(rule_svc.fork_rule(&ws_id, &rule.id, "rule-1-fork", "user-2"))
            .unwrap();

        assert_eq!(forked.name, "rule-1-fork");
        assert_eq!(forked.state, RuleState::Draft);
        assert_ne!(forked.id, rule.id);

        // forked 规则应有独立的 version 1
        let versions = rt
            .block_on(rule_svc.list_rule_versions(&ws_id, &forked.id))
            .unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].version, 1);

        // 内容应与源规则当前版本相同
        let source_versions = rt
            .block_on(rule_svc.list_rule_versions(&ws_id, &rule.id))
            .unwrap();
        let source_current = source_versions
            .iter()
            .find(|v| v.state == RuleVersionState::Current)
            .unwrap();
        assert_eq!(versions[0].content, source_current.content);
        assert_eq!(versions[0].content_hash, source_current.content_hash);
    }

    #[test]
    fn test_invalid_rule_content_rejected() {
        let (rule_svc, ws_svc) = make_services();
        let ws_id = make_workspace(&ws_svc);

        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(rule_svc.create_rule(
                &ws_id,
                CreateRuleRequest {
                    name: "bad".to_string(),
                    content: "not valid json".to_string(),
                    created_by: "owner-1".to_string(),
                    description: None,
                },
            ))
            .unwrap_err();
        assert!(matches!(err, WorkspaceError::InvalidInput(_)));
    }

    #[test]
    fn test_rule_workspace_isolation() {
        let (rule_svc, ws_svc) = make_services();
        let rt = tokio::runtime::Runtime::new().unwrap();

        let ws1 = rt
            .block_on(ws_svc.create_workspace(CreateWorkspaceRequest {
                name: "team-1".to_string(),
                owner_id: "owner-1".to_string(),
                description: None,
            }))
            .unwrap();
        let ws2 = rt
            .block_on(ws_svc.create_workspace(CreateWorkspaceRequest {
                name: "team-2".to_string(),
                owner_id: "owner-1".to_string(),
                description: None,
            }))
            .unwrap();

        let rule = make_rule(&rule_svc, &ws1.id, "rule-1");

        // 从 ws2 访问 ws1 的规则应失败
        let err = rt
            .block_on(rule_svc.get_rule(&ws2.id, &rule.id))
            .unwrap_err();
        assert!(matches!(err, WorkspaceError::NotFound { .. }));
    }
}

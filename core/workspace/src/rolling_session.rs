// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 滚动 session 热重载编排 — 三层架构 §3.3
//!
//! 设计依据: PUBLISH_QUEUE_DESIGN.md §4 (P2)
//!
//! # 核心机制
//! evorule-tcb 的 TCB 不可变语义: `replace_core_eval` 只影响**新创建**的 session,
//! 已存在的 session 不会中途换规则。因此热重载采用滚动 session 方案:
//!
//! 1. `reload_rules()` → SessionManager 内部 core_eval 更新
//! 2. `fork_session(old_id)` → Fork 旧 session (新 session 用新 core_eval)
//! 3. 切换 `production_state.current_session_id` → 新 session_id
//! 4. 向旧 session 的 SSE 订阅者推送 `session_switched` 事件 (U7)
//! 5. 旧 session drain 完在途 Fact 后 close
//!
//! 全程不中断正在处理的 Fact,版本号单调递增,审计链可追溯。

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use serde_json::Value;
use tracing::{info, warn};

use crate::db::WorkspaceDb;
use crate::error::{WorkspaceError, WorkspaceResult};
use crate::session_bridge::SessionOps;
use crate::session_switched::SessionSwitchedBroadcaster;

/// 旧 session drain 超时 (P4 决策: 30s, 与 evorule-server GRACEFUL_SHUTDOWN_TIMEOUT 一致)
const DRAIN_TIMEOUT_SECS: u64 = 30;

/// 滚动 session 切换结果
#[derive(Debug, Clone)]
pub struct RollingSwapResult {
    /// 新的 production session_id (fork 后)
    pub new_session_id: u64,
    /// 新的规则集版本号 (单调递增)
    pub new_ruleset_version: i64,
    /// 新规则集 BLAKE3 哈希
    pub new_ruleset_hash: String,
}

/// 导出生产 session 的 BLAKE3 审计链到磁盘 (缺口5 修复)
///
/// 与沙盒 close 时导出测试报告同构 (sandbox_service::close_sandbox)。
/// 在 rolling_swap 关闭旧生产 session 前调用, 确保 fact 级审计链不随 session 销毁而丢失。
///
/// # 背景
/// evorule-governance 的 SessionManager 在内存模式下 (wal_dir=None) 关闭 session 时,
/// FactsLog 和 Auditor 仅加入 pending_recycle 列表等待复用, BLAKE3 哈希链不落盘。
/// 这导致生产 session 被 rolling swap 替换后, 其 fact 级审计链永久丢失 — 对合规
/// 审计层 (EU AI Act Article 12) 不可接受。
///
/// # 导出路径
/// `./data/production_audits/session_{id}_{timestamp}.json`
///
/// # 参数
/// - `session_ops`: SessionOps 引用 (用于 flush + export)
/// - `session_id`: 待关闭的生产 session ID
///
/// # 返回
/// - `Some(path)`: 导出成功, 返回文件路径
/// - `None`: 导出失败 (已记录 warning 日志, 不阻塞 session 关闭)
pub async fn export_production_audit_chain(
    session_ops: &Arc<dyn SessionOps>,
    session_id: u64,
) -> Option<String> {
    // 1. flush 审计链 (确保 reactor 已处理的 fact 全部上链)
    if let Err(e) = session_ops.flush_audit(session_id).await {
        warn!(
            session_id,
            error = %e,
            "Failed to flush audit before exporting production session audit chain (缺口5)"
        );
        return None;
    }

    // 2. 导出完整审计链 JSON (含 entries / entry_count / last_hash / verified)
    let audit_json = match session_ops.get_audit_export(session_id).await {
        Ok(json) => json,
        Err(e) => {
            warn!(
                session_id,
                error = %e,
                "Failed to export production session audit chain (缺口5)"
            );
            return None;
        }
    };

    // 3. 写入磁盘 (./data/production_audits/session_{id}_{timestamp}.json)
    let dir = std::path::Path::new("./data/production_audits");
    if let Err(e) = std::fs::create_dir_all(dir) {
        warn!(
            session_id,
            error = %e,
            "Failed to create production_audits directory (缺口5)"
        );
        return None;
    }

    let timestamp = Utc::now().timestamp();
    let path = dir.join(format!("session_{session_id}_{timestamp}.json"));

    match std::fs::write(&path, &audit_json) {
        Ok(()) => {
            let path_str = path.to_string_lossy().to_string();
            info!(
                session_id,
                path = %path_str,
                "Production session audit chain exported (缺口5: BLAKE3 链持久化)"
            );
            Some(path_str)
        }
        Err(e) => {
            warn!(
                session_id,
                error = %e,
                "Failed to write production session audit chain to file (缺口5)"
            );
            None
        }
    }
}

/// 滚动 session 热重载服务
///
/// 编排 reload → fork → switch → broadcast → drain 完整流程。
/// 持有 `Arc<WorkspaceDb>` + `Arc<dyn SessionOps>` + `SessionSwitchedBroadcaster`。
pub struct RollingSessionService {
    db: Arc<WorkspaceDb>,
    session_ops: Arc<dyn SessionOps>,
    /// session_switched 广播器 (U7 决策)
    switcher: SessionSwitchedBroadcaster,
    /// 旧 session drain 超时
    drain_timeout: Duration,
}

impl RollingSessionService {
    /// 创建新服务实例
    pub fn new(
        db: Arc<WorkspaceDb>,
        session_ops: Arc<dyn SessionOps>,
        switcher: SessionSwitchedBroadcaster,
    ) -> Self {
        Self {
            db,
            session_ops,
            switcher,
            drain_timeout: Duration::from_secs(DRAIN_TIMEOUT_SECS),
        }
    }

    /// 滚动 session 切换 (核心编排)
    ///
    /// 三层架构 §3.3 完整流程:
    /// 1. `reload_rules()` → SessionManager 内部 core_eval 更新
    /// 2. `fork_session(old_id)` → Fork 旧 session (新 session 用新 core_eval)
    /// 3. 切换 `production_state.current_session_id` (原子更新)
    /// 4. 打新 ruleset_version (单调递增)
    /// 5. 写 production_audit 表 (含 ruleset_snapshot 快照)
    /// 6. 向旧 session 的 SSE 订阅者推送 session_switched 事件 (U7)
    /// 7. 旧 session drain 完在途 Fact 后 close (异步, 不阻塞返回)
    ///
    /// # 参数
    /// - `rules`: 待发布的规则集 (JSON 数组,用于快照审计)
    /// - `ruleset_hash`: 规则集 BLAKE3 哈希
    /// - `source_workspace_id`: 来源工作空间 ID (审计追溯)
    /// - `operated_by`: 操作者用户 ID
    /// - `reason`: 回滚原因 (正常发布为 None, 回滚时填充)
    /// - `test_report_paths`: 关联的沙盒测试报告文件路径 (缺口4: SANDBOX_ORCHESTRATION_DESIGN.md §5.3
    ///   要求 production_audit.test_report_paths 存报告路径用于追溯; 正常发布时由
    ///   publish_service::execute_publish 从 queue.test_report_sandbox_id 查得, 回滚时为 None)
    pub async fn rolling_swap(
        &self,
        rules: &[Value],
        ruleset_hash: &str,
        source_workspace_id: &str,
        operated_by: &str,
        reason: Option<&str>,
        test_report_paths: Option<&str>,
    ) -> WorkspaceResult<RollingSwapResult> {
        let start_time = Instant::now();

        // 获取当前生产状态
        let current_state = self.db.get_production_state()?;

        // 首次发布: current_session_id 为 NULL → 创建新 session (无需 fork)
        // 后续发布: current_session_id 非 NULL → fork 旧 session (滚动热重载)
        let is_first_publish = current_state.current_session_id.is_none();

        // Step 1: reload_rules (SessionManager 内部 core_eval 更新)
        // 注意: 规则文件写入由 evorule-server 的 reload handler 处理 (扫描 rules_dir)。
        // 本服务通过 SessionOps 触发 reload,实际规则内容已通过外部写入 rules_dir。
        self.session_ops.reload_rules().await?;
        info!("Rules reloaded into SessionManager core_eval");

        let new_session_id = if is_first_publish {
            // 首次发布: 创建全新 session (无旧 session 可 fork)
            let sid = self.session_ops.create_session().await?;
            info!(
                new_session_id = sid,
                "First publish: created new production session (no old session to fork)"
            );
            sid
        } else {
            // 后续发布: Fork 旧 session (新 session 用新 core_eval, 继承 payload 状态)
            let old_session_id = current_state
                .current_session_id
                .ok_or_else(|| {
                    WorkspaceError::internal(
                        "current_session_id is None despite is_first_publish=false (invariant violated)",
                    )
                })? as u64;
            info!(
                old_session_id = old_session_id,
                rule_count = rules.len(),
                ruleset_hash = ruleset_hash,
                "Starting rolling session swap (fork from existing production session)"
            );
            let sid = self.session_ops.fork_session(old_session_id).await?;
            info!(
                old_session_id = old_session_id,
                new_session_id = sid,
                "New production session forked (inherits payload state, uses new core_eval)"
            );

            // Step 6: 向旧 session 的 SSE 订阅者推送 session_switched 事件 (U7)
            // (仅后续发布需要, 首次发布无旧 session)
            let new_ruleset_version_for_broadcast = current_state.ruleset_version + 1;
            self.switcher
                .broadcast_switched(
                    old_session_id,
                    sid,
                    new_ruleset_version_for_broadcast,
                    ruleset_hash,
                )
                .await?;
            info!(
                old_session_id = old_session_id,
                new_session_id = sid,
                "session_switched SSE event pushed to old session subscribers"
            );

            // Step 7: 旧 session drain (异步, 等待在途 Fact 处理完, 超时强制关闭)
            // P0 简化: 直接等待固定超时后关闭 (P1 增强: 轮询判断是否处理完)
            // 缺口5 修复: 关闭前导出生产 session 的 BLAKE3 审计链到磁盘
            // (与沙盒 close 时导出测试报告同构, 确保生产 session 的 fact 级审计链不丢失)
            let session_ops = self.session_ops.clone();
            let switcher = self.switcher.clone();
            let db = self.db.clone();
            let timeout = self.drain_timeout;
            let closed_ruleset_version = current_state.ruleset_version;
            let closed_ruleset_hash = current_state.ruleset_hash.clone().unwrap_or_default();
            let source_ws_id = source_workspace_id.to_string();
            tokio::spawn(async move {
                tokio::time::sleep(timeout).await;

                // 缺口5: 关闭前 flush + 导出审计链到磁盘
                let audit_export_path =
                    export_production_audit_chain(&session_ops, old_session_id).await;

                match session_ops.close_session(old_session_id).await {
                    Ok(()) => {
                        info!(
                            old_session_id = old_session_id,
                            audit_export_path = ?audit_export_path,
                            "Old production session closed after drain"
                        );
                        // 缺口5: 记录 session_closed 生命周期节点到 production_audit
                        // test_report_paths 字段在此复用为审计链导出路径
                        // (event_type=session_closed 区分语义, 非沙盒测试报告)
                        if let Some(path) = &audit_export_path {
                            let source_ws_ids =
                                serde_json::json!([source_ws_id]).to_string();
                            if let Err(e) = db.insert_production_audit(
                                "session_closed",
                                closed_ruleset_version,
                                None,
                                &closed_ruleset_hash,
                                old_session_id as i64,
                                &source_ws_ids,
                                "system",
                                Some("rolling_swap_drain_complete"),
                                Some(path),
                                None,
                            ) {
                                warn!(
                                    old_session_id = old_session_id,
                                    error = %e,
                                    "Failed to record session_closed audit event (缺口5)"
                                );
                            }
                        }
                    }
                    Err(e) => {
                        warn!(
                            old_session_id = old_session_id,
                            error = %e,
                            "Failed to close old production session after drain timeout"
                        );
                    }
                }
                // 通知广播器旧 session 已关闭 (清理订阅通道)
                switcher.notify_session_closed(old_session_id).await;
            });

            sid
        };

        // Step 3: 计算新版本号 (单调递增)
        let new_ruleset_version = current_state.ruleset_version + 1;

        // Step 4: 原子更新 production_state
        self.db.update_production_state(
            new_session_id as i64,
            new_ruleset_version,
            ruleset_hash,
            operated_by,
        )?;
        info!(
            new_session_id = new_session_id,
            new_ruleset_version = new_ruleset_version,
            is_first_publish = is_first_publish,
            "production_state updated"
        );

        // Step 5: 写 production_audit 表 (含 ruleset_snapshot 快照, 用于回滚)
        let source_ws_ids = serde_json::json!([source_workspace_id]).to_string();
        let ruleset_snapshot = serde_json::to_string(rules).unwrap_or_else(|_| "[]".to_string());
        self.db.insert_production_audit(
            if reason.is_some() {
                "ruleset_rollback"
            } else {
                "ruleset_published"
            },
            new_ruleset_version,
            Some(current_state.ruleset_version),
            ruleset_hash,
            new_session_id as i64,
            &source_ws_ids,
            operated_by,
            reason,
            // 缺口4 修复: 关联沙盒测试报告路径 (SANDBOX_ORCHESTRATION_DESIGN.md §5.3)
            // 正常发布时为 publish_service::execute_publish 查得的 sandbox export_path;
            // 回滚时为 None (回滚不产生新的测试报告)。
            test_report_paths,
            Some(&ruleset_snapshot),
        )?;

        info!(
            new_session_id = new_session_id,
            new_ruleset_version = new_ruleset_version,
            is_first_publish = is_first_publish,
            elapsed_ms = start_time.elapsed().as_millis(),
            "Rolling session swap completed"
        );

        Ok(RollingSwapResult {
            new_session_id,
            new_ruleset_version,
            new_ruleset_hash: ruleset_hash.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::db::WorkspaceDb;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    /// 测试用 SessionOps 桩: 通过共享 Arc 记录 reload/close 调用
    struct MockSessionOps {
        next_id: AtomicU64,
        /// 共享的 reload 计数器 (测试可直接读取)
        reload_counter: Arc<AtomicU64>,
        closed: Mutex<Vec<u64>>,
    }

    impl MockSessionOps {
        fn new(start_id: u64) -> (Self, Arc<AtomicU64>) {
            let reload_counter = Arc::new(AtomicU64::new(0));
            let ops = Self {
                next_id: AtomicU64::new(start_id),
                reload_counter: reload_counter.clone(),
                closed: Mutex::new(Vec::new()),
            };
            (ops, reload_counter)
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
        async fn close_session(&self, id: u64) -> WorkspaceResult<()> {
            self.closed.lock().unwrap().push(id);
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
            self.reload_counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn flush_audit(&self, _id: u64) -> WorkspaceResult<usize> {
            Ok(0)
        }
    }

    fn make_db_with_production_session(session_id: i64) -> Arc<WorkspaceDb> {
        let db = Arc::new(WorkspaceDb::in_memory().unwrap());
        // 直接更新 production_state 设置初始 session_id
        db.update_production_state(session_id, 0, "initial_hash", "system")
            .unwrap();
        db
    }

    #[tokio::test]
    async fn test_rolling_swap_increments_version() {
        let db = make_db_with_production_session(100);
        let (mock, reload_counter) = MockSessionOps::new(200);
        let ops: Arc<dyn SessionOps> = Arc::new(mock);
        let switcher = SessionSwitchedBroadcaster::new();
        let svc = RollingSessionService::new(db.clone(), ops, switcher);

        let rules = vec![serde_json::json!({"type": "noop"})];
        let result = svc
            .rolling_swap(&rules, "hash_v1", "ws-test", "admin-1", None, None)
            .await
            .unwrap();

        // 版本号递增: 0 → 1
        assert_eq!(result.new_ruleset_version, 1);
        assert_eq!(result.new_session_id, 200);
        assert_eq!(result.new_ruleset_hash, "hash_v1");

        // production_state 已更新
        let state = db.get_production_state().unwrap();
        assert_eq!(state.ruleset_version, 1);
        assert_eq!(state.current_session_id, Some(200));

        // reload 被调用一次
        assert_eq!(reload_counter.load(Ordering::SeqCst), 1);

        // production_audit 已记录 (含 ruleset_snapshot)
        let audit = db.get_production_audit_by_version(1).unwrap();
        assert!(audit.is_some());
        let audit = audit.unwrap();
        assert_eq!(audit.event_type, "ruleset_published");
        assert_eq!(audit.previous_version, Some(0));
        assert!(audit.ruleset_snapshot.is_some());
        assert!(audit.ruleset_snapshot.as_deref().unwrap().contains("noop"));
    }

    #[tokio::test]
    async fn test_rolling_swap_rollback_records_audit() {
        let db = make_db_with_production_session(100);
        let (mock, _reload_counter) = MockSessionOps::new(200);
        let ops: Arc<dyn SessionOps> = Arc::new(mock);
        let switcher = SessionSwitchedBroadcaster::new();
        let svc = RollingSessionService::new(db.clone(), ops, switcher);

        let rules = vec![serde_json::json!({"type": "rollback_rule"})];
        let result = svc
            .rolling_swap(&rules, "old_hash", "rollback", "admin-1", Some("误触发"), None)
            .await
            .unwrap();

        // 回滚也递增版本号 (不回退)
        assert_eq!(result.new_ruleset_version, 1);

        let audit = db.get_production_audit_by_version(1).unwrap().unwrap();
        assert_eq!(audit.event_type, "ruleset_rollback");
        assert_eq!(audit.reason.as_deref(), Some("误触发"));
    }

    #[tokio::test]
    async fn test_rolling_swap_first_publish_creates_session() {
        // 首次发布: production_state.current_session_id = NULL
        // 应该 create_session 而非 fork_session
        let db = Arc::new(WorkspaceDb::in_memory().unwrap());
        // 不调用 update_production_state, 保持 current_session_id = NULL
        let (mock, _reload_counter) = MockSessionOps::new(500);
        let ops: Arc<dyn SessionOps> = Arc::new(mock);
        let switcher = SessionSwitchedBroadcaster::new();
        let svc = RollingSessionService::new(db.clone(), ops, switcher);

        let rules = vec![serde_json::json!({"type": "set", "params": {"attr": "x"}})];
        let result = svc
            .rolling_swap(&rules, "first_hash", "ws-first", "admin-1", None, None)
            .await
            .unwrap();

        // 首次发布: 版本 0 → 1, session_id = 500 (create_session 返回)
        assert_eq!(result.new_ruleset_version, 1);
        assert_eq!(result.new_session_id, 500);

        // production_state 已更新
        let state = db.get_production_state().unwrap();
        assert_eq!(state.ruleset_version, 1);
        assert_eq!(state.current_session_id, Some(500));

        // production_audit 已记录
        let audit = db.get_production_audit_by_version(1).unwrap().unwrap();
        assert_eq!(audit.event_type, "ruleset_published");
        assert_eq!(audit.previous_version, Some(0));
    }
}

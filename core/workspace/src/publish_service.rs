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
//! - 发布链闭环 (审计⑥ 批 B C1+C5): 审批通过 → 闸门一证据检查 → Schema 门禁 →
//!   构造 DatasetBundle + BundleImporter 校验 → 原子落盘 rules_dir → 滚动 session 热重载
//! - 紧急回滚 (用旧规则集快照 + 新版本号, 版本号只增不减)
//!
//! # 三级权限 （决策）
//! | 角色 | 提交 | 审批 | 回滚 |
//! |------|------|------|------|
//! | Doctor | ❌ | ❌ | ❌ |
//! | DepartmentHead | ✅ (本科室 WS) | ❌ | ❌ |
//! | Admin | ❌ | ✅ (全院) | ✅ |

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::Mutex;
use tracing::info;

use crate::db::WorkspaceDb;
use crate::error::{WorkspaceError, WorkspaceResult};
use crate::models::{
    ProductionAuditRecord, ProductionStateRecord, PublishKind, PublishQueueItem, PublishRole,
    PublishStatus, ReviewPublishRequest, RollbackRequest, SubmitPublishRequest,
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
    /// 业务规则目录 (审计⑥ 批 B C5: 发布产物 DatasetBundle 落盘目标)
    rules_dir: PathBuf,
}

impl PublishService {
    /// 创建新服务实例
    ///
    /// `rules_dir` 为业务规则目录（与 server 启动加载的 rules_dir 同源），
    /// 发布时规范 DatasetBundle 原子落盘至 `{rules_dir}/bundles/{bundle_id}/`，
    /// 后续热重载/重启加载自然生效。
    pub fn new(
        db: Arc<WorkspaceDb>,
        rolling_session: RollingSessionService,
        rules_dir: PathBuf,
    ) -> Self {
        Self {
            db,
            rolling_session,
            publish_lock: Arc::new(Mutex::new(())),
            rules_dir,
        }
    }

    /// 提交到发布队列 (科室主任/管理员权限; UV-151: Admin 亦可提交)
    ///
    /// 流程:
    /// 1. 校验角色 (DepartmentHead 或 Admin)
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
                "role {:?} cannot submit to publish queue (requires DepartmentHead or Admin)",
                role
            )));
        }

        // 校验提交者是 workspace 成员
        if !self
            .db
            .is_workspace_member(&req.workspace_id, submitted_by)?
        {
            return Err(WorkspaceError::forbidden(format!(
                "您不是工作空间 {} 的成员,无法提交发布;请联系工作空间所有者将您加入成员 (user={submitted_by})",
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
            // 前置缺陷修复: 校验提交的版本是该规则的当前版本。
            // 原实现未校验, 可提交被后续版本覆盖的旧版本 (Superseded) 内容, 造成发布过期规则。
            if rule.current_version_id.as_deref() != Some(rv.id.as_str()) {
                return Err(WorkspaceError::invalid_input(format!(
                    "rule {} version {} is not the current version (current: {}) — publish only the current candidate version",
                    rule.id,
                    rv.id,
                    rule.current_version_id.as_deref().unwrap_or("none")
                )));
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

        // UV-145 W3: 元规则晋升校验 (kind=meta_promotion 时转写产物前置校验,
        // 防落盘后 loader 拒载的废文件入库; 与 loader tier_gate/schema 门禁同口径)
        let meta_rule_content: Option<String> = match req.kind {
            PublishKind::Normal => None,
            PublishKind::MetaPromotion => {
                let raw = req.meta_rule_content.as_deref().ok_or_else(|| {
                    WorkspaceError::invalid_input(
                        "kind=meta_promotion 要求 meta_rule_content (转写后的元规则 JSON)",
                    )
                })?;
                validate_meta_rule_content(raw)?;
                // 服务端权威预填晋升溯源: promoted_from 锚定来源规则版本,
                // 审批落盘时再填 promoted_by/promoted_at/zero_alarm_window (防客户端伪造)
                let mut meta: Value = serde_json::from_str(raw).map_err(|e| {
                    WorkspaceError::internal(format!("reparse meta_rule_content failed: {e}"))
                })?;
                let promoted_from = rule_versions
                    .iter()
                    .map(|rv| format!("rule_version:{}", rv.id))
                    .collect::<Vec<_>>()
                    .join(",");
                if let Some(obj) = meta.as_object_mut() {
                    let md = obj
                        .entry("metadata")
                        .or_insert_with(|| serde_json::json!({}));
                    if let Some(md_obj) = md.as_object_mut() {
                        md_obj.insert("promoted_from".to_string(), Value::String(promoted_from));
                    }
                }
                Some(serde_json::to_string(&meta)?)
            }
        };

        // 5. 插入 publish_queue
        let id = self.db.insert_publish_queue_item(
            &req.workspace_id,
            &final_candidate_rules,
            &ruleset_hash,
            req.test_report_sandbox_id,
            submitted_by,
            req.description.as_deref(),
            req.kind,
            meta_rule_content.as_deref(),
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
                // 前置缺陷修复: 发布期间队列保持 pending, 若发布失败队列仍为 pending 可重试,
                // 不残留孤儿 approved 状态 (原实现先置 approved 再发布, 失败无法恢复)。
                // meta_promotion 分流见 execute_meta_promotion (返回值为业务版本上下文, 未推进)。
                let published_version = self.execute_publish(queue_id, reviewed_by).await?;

                // 发布成功 → 标记队列为 published (审批人/意见随 complete_publish 一并落库)
                self.db.complete_publish(
                    queue_id,
                    published_version,
                    reviewed_by,
                    req.comment.as_deref(),
                )?;

                if item.kind == PublishKind::MetaPromotion {
                    info!(
                        queue_id = queue_id,
                        "Meta promotion completed: L2 file landed, business ruleset version unchanged"
                    );
                } else {
                    info!(
                        queue_id = queue_id,
                        published_version = published_version,
                        "Publish completed: ruleset rolled out to production"
                    );
                }
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

    /// 执行发布 (发布链闭环 + 滚动 session 热重载)
    ///
    /// 获取全局发布锁后按队列项类型分流:
    /// - Normal: 业务规则 DatasetBundle 落盘 bundles/ + 滚动 session 版本推进
    /// - MetaPromotion (UV-145 W3): 元规则 00_meta_ 文件落盘 rules_dir 根目录,
    ///   不推业务版本, 审计 meta_promoted
    ///
    /// 任一步骤失败 → 发布失败 (队列保持 pending 可重试), 杜绝绕过。
    async fn execute_publish(&self, queue_id: i64, published_by: &str) -> WorkspaceResult<i64> {
        let _lock = self.publish_lock.lock().await;

        let item = self
            .db
            .get_publish_queue_item(queue_id)?
            .ok_or_else(|| WorkspaceError::not_found("publish_queue", queue_id.to_string()))?;

        match item.kind {
            PublishKind::MetaPromotion => self.execute_meta_promotion(&item, published_by).await,
            PublishKind::Normal => self.execute_normal_publish(&item, published_by).await,
        }
    }

    /// 闸门一证据检查 (fail-closed, 与 B2 import 侧同口径)
    ///
    /// 存在性 + closed + 报告文件可解析 + summary.failed == 0,
    /// 任一不满足即 Err (队列保持 pending 可重试)。
    /// 通过时返回关联的测试报告路径 (缺口4: 写入 production_audit.test_report_paths)。
    fn verify_gate_one_evidence(&self, item: &PublishQueueItem) -> WorkspaceResult<Option<String>> {
        let sandbox_id = item.test_report_sandbox_id.ok_or_else(|| {
            WorkspaceError::invalid_input(format!(
                "发布被拒绝（闸门一证据缺失）: 队列项 {} 未关联已完成的沙盒测试。\
                 请先在沙盒中验证规则集，提交发布时携带 test_report_sandbox_id 后重试",
                item.id
            ))
        })?;
        let sb = self.db.get_sandbox_session(sandbox_id)?.ok_or_else(|| {
            WorkspaceError::invalid_input(format!(
                "发布被拒绝（闸门一证据缺失）: 队列项 {} 关联的沙盒 #{sandbox_id} 不存在",
                item.id
            ))
        })?;
        if sb.status != crate::SandboxStatus::Closed {
            return Err(WorkspaceError::invalid_input(format!(
                "发布被拒绝（闸门一证据无效）: 沙盒 #{sandbox_id} 状态为 {:?}\
                 （非 closed，测试未完成），不得作为发布证据。\
                 请在测试工作台完成沙盒测试并关闭出报告后重试",
                sb.status
            )));
        }
        // 报告一致性: 与 close_sandbox 落盘同口径推导 report_<basename>.json
        let export_path = sb.export_path.as_deref().ok_or_else(|| {
            WorkspaceError::invalid_input(format!(
                "发布被拒绝（闸门一证据无效）: 沙盒 #{sandbox_id} 已关闭但无报告\
                 导出路径（数据异常）。请重跑沙盒测试"
            ))
        })?;
        let file_name = export_path.rsplit('/').next().unwrap_or_default();
        let report_path = format!("{}/report_{}", crate::SANDBOX_REPORT_DIR, file_name);
        let content = std::fs::read_to_string(&report_path).map_err(|_| {
            WorkspaceError::invalid_input(format!(
                "发布被拒绝（闸门一证据无效）: 沙盒 #{sandbox_id} 报告文件缺失\
                 （{report_path}）。请重跑沙盒测试"
            ))
        })?;
        let report: Value = serde_json::from_str(&content).map_err(|e| {
            WorkspaceError::invalid_input(format!(
                "发布被拒绝（闸门一证据无效）: 沙盒 #{sandbox_id} 报告文件损坏: {e}"
            ))
        })?;
        let failed = report
            .pointer("/summary/failed")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| {
                WorkspaceError::invalid_input(format!(
                    "发布被拒绝（闸门一证据无效）: 沙盒 #{sandbox_id} 报告缺少\
                     summary.failed 字段（结构异常）"
                ))
            })?;
        if failed != 0 {
            return Err(WorkspaceError::invalid_input(format!(
                "发布被拒绝（闸门一 FAIL 报告）: 沙盒 #{sandbox_id} 测试报告有\
                 {failed} 个失败用例，不得作为发布证据。\
                 请修复规则后在测试工作台重新验证"
            )));
        }
        Ok(Some(report_path))
    }

    /// 执行普通业务规则发布 (发布链闭环 + 滚动 session 热重载)
    ///
    /// 1. 闸门一证据检查 （决策: 未验证不得默认 Pass）
    /// 2. 逐条 Schema 门禁 (与外部导入通道 import_bundle 第 7 项同级硬失败)
    /// 3. 构造规范 DatasetBundle + BundleImporter::validate (6 项硬校验)
    /// 4. 原子落盘 rules_dir (C5, 补 H4 缺失的写盘)
    /// 5. 滚动 session 热重载 (rolling_swap 内部 reload 从 rules_dir 重扫,
    ///    故落盘必须在前, 新会话才真正运行新规则)
    async fn execute_normal_publish(
        &self,
        item: &PublishQueueItem,
        published_by: &str,
    ) -> WorkspaceResult<i64> {
        // 解析规则集
        let rules: Vec<Value> = serde_json::from_str(&item.final_candidate_rules).map_err(|e| {
            WorkspaceError::internal(format!("parse final_candidate_rules failed: {e}"))
        })?;

        // ===== 发布链闭环 (审计⑥ 批 B C1+C5) =====

        // 新版本号 (与 rolling_swap 内部计算口径一致: current + 1)
        let prod_state = self.db.get_production_state()?;
        let new_version = prod_state.ruleset_version + 1;

        // 1. 闸门一证据检查 （决策: 未跑真实沙箱验证的发布不得默认 Pass）
        // : 升级为与 B2(import 侧)同口径——存在性 + closed + 报告一致性,
        // 一律 fail-closed。旧实现仅查会话存在性(is_some),FAIL 报告/未关闭沙盒均可
        // 过闸门,且 build_publish_bundle 的 verdict 硬编码 Pass——假 pass 证据落盘。
        let test_report_paths = self.verify_gate_one_evidence(item)?;

        // 2. 逐条 Schema 门禁 (硬失败, 防 loader fail-soft 静默跳过非法规则)
        for (i, rule) in rules.iter().enumerate() {
            let report = evorule_rule_schema::validate_rule_input(rule);
            if !report.valid {
                return Err(WorkspaceError::invalid_input(format!(
                    "规则 #{i} 未通过 Schema 门禁（引擎原生结构非法）: {}",
                    report.errors.join("; ")
                )));
            }
        }

        // 3. 构造规范 DatasetBundle + BundleImporter::validate (6 项硬校验)
        // 发布队列 MVP 仅规则包（rule 条目不消费领域 schema，resolver 恒未命中即可）
        let no_domain_schema = |_uri: &str| None;
        let bundle = build_publish_bundle(&rules, item, new_version, published_by);
        let import_result = evorule_bundle::BundleImporter::validate(&bundle, &no_domain_schema)
            .map_err(|e| WorkspaceError::internal(format!("发布校验失败（不落盘不生效）: {e}")))?;

        // 4. 原子落盘 rules_dir (失败则发布失败, 队列保持 pending 可重试)
        crate::bundle_land::land_bundle_atomically(&self.rules_dir, &bundle, &import_result)
            .map_err(|e| {
                WorkspaceError::internal(format!("发布落盘失败（不生效，可重试）: {e}"))
            })?;

        // 5. 执行滚动 session 热重载 (内部 reload 从 rules_dir 读到刚落盘的 bundle)
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

    /// 执行元规则晋升落盘 (UV-145 W3 晋升通道核心)
    ///
    /// 与普通发布 ([`Self::execute_normal_publish`]) 的差异:
    /// 1. 产物为 L2 元规则文件 `rules_dir/00_meta_promoted_{hash16}.json`
    ///    (根目录直置, 非 bundles/ 子目录), 同内容同名幂等, 原子写 (tmp+rename);
    /// 2. 不 fork session / 不推业务 ruleset_version (元规则不进业务版本序列),
    ///    仅触发 reload (会话程序为创建时快照, 新会话生效、存量会话不受影响);
    /// 3. 审计 event_type=meta_promoted, ruleset_snapshot 存落盘内容 (版本回溯可查);
    /// 4. 溯源字段 (promoted_by/promoted_at/zero_alarm_window) 服务端权威填充,
    ///    覆盖客户端同名字段 (防伪造); promoted_from 在提交时已锚定来源规则版本。
    async fn execute_meta_promotion(
        &self,
        item: &PublishQueueItem,
        published_by: &str,
    ) -> WorkspaceResult<i64> {
        // 1. 闸门一证据检查 (零报警证据, 与普通发布同口径 fail-closed)
        let test_report_paths = self.verify_gate_one_evidence(item)?;

        // 2. 解析转写产物 + 防御性复核 (提交时已校验, 此处防库内篡改后落盘)
        let raw = item.meta_rule_content.as_deref().ok_or_else(|| {
            WorkspaceError::internal("meta_promotion queue item missing meta_rule_content")
        })?;
        let mut meta: Value = serde_json::from_str(raw).map_err(|e| {
            WorkspaceError::internal(format!("parse meta_rule_content failed: {e}"))
        })?;
        validate_meta_rule_content(raw)?;

        // 3. 服务端权威填充晋升溯源 (覆盖客户端同名字段)
        if let Some(obj) = meta.as_object_mut() {
            let md = obj
                .entry("metadata")
                .or_insert_with(|| serde_json::json!({}));
            if let Some(md_obj) = md.as_object_mut() {
                md_obj.insert(
                    "promoted_by".to_string(),
                    Value::String(published_by.to_string()),
                );
                md_obj.insert(
                    "promoted_at".to_string(),
                    Value::String(chrono::Utc::now().to_rfc3339()),
                );
                md_obj.insert(
                    "zero_alarm_window".to_string(),
                    Value::String(format!(
                        "sandbox:{}",
                        item.test_report_sandbox_id.unwrap_or(0)
                    )),
                );
            }
        }
        let content_str = serde_json::to_string_pretty(&meta)?;

        // 4. 原子落盘 L2 元规则文件 (tmp + rename, 同内容同名幂等)
        let hash = evorule_hash::digest(content_str.as_bytes());
        let hash_prefix = hash.get(..16).unwrap_or(&hash);
        let file_name = format!("00_meta_promoted_{hash_prefix}.json");
        let final_path = self.rules_dir.join(&file_name);
        let tmp_path = self.rules_dir.join(format!("{file_name}.tmp"));
        std::fs::write(&tmp_path, &content_str).map_err(|e| {
            WorkspaceError::internal(format!("写元规则临时文件失败 (不生效，可重试): {e}"))
        })?;
        std::fs::rename(&tmp_path, &final_path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp_path);
            WorkspaceError::internal(format!("元规则原子落盘失败 (不生效，可重试): {e}"))
        })?;

        // 5. 审计 meta_promoted (ruleset_snapshot 存落盘内容, 回溯可查;
        //    event_type 不在回滚快照过滤集 ruleset_published/ruleset_rollback 内,
        //    不干扰紧急回滚链)
        let prod_state = self.db.get_production_state()?;
        let source_ws_ids = serde_json::json!([item.workspace_id]).to_string();
        self.db.insert_production_audit(
            "meta_promoted",
            prod_state.ruleset_version, // 上下文版本, 不推进
            None,
            &hash,
            prod_state.current_session_id.unwrap_or(0),
            &source_ws_ids,
            published_by,
            Some(&format!(
                "meta promotion: {file_name} (publish_queue #{})",
                item.id
            )),
            test_report_paths.as_deref(),
            Some(&content_str),
        )?;

        // 6. 重载规则 (新会话携带新元规则; 不 fork / 不推业务版本)
        self.rolling_session.reload_rules().await?;

        info!(
            queue_id = item.id,
            meta_file = %file_name,
            content_hash = %hash,
            published_by = published_by,
            "Meta rule promoted: L2 file landed and rules reloaded"
        );

        // 返回当前业务版本号 (仅作 complete_publish 的上下文填充, 未推进)
        Ok(prod_state.ruleset_version)
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
    // 审计⑥ C3: 哈希实现统一走 evorule-hash; 输入字节构造保持不变 (逐条 JSON 串 + '\n')
    let mut buf: Vec<u8> = Vec::new();
    for rule in rules {
        // 使用规范化的 JSON 字符串参与哈希
        let rule_str = serde_json::to_string(rule).unwrap_or_default();
        buf.extend_from_slice(rule_str.as_bytes());
        buf.extend_from_slice(b"\n");
    }
    evorule_hash::digest(&buf)
}

/// 校验转写后的元规则内容 (UV-145 W3, 提交与落盘两侧共用单一权威实现)
///
/// 与 loader 层级门禁 (`tier_gate_reason`) / Schema 门禁 (`passes_schema_gate`) 同口径:
/// 1. 合法 JSON 对象;
/// 2. `metadata.tier == "meta"` (L2 正向门禁——防裸文件冒充元规则);
/// 3. `metadata.title` 非空 (元规则可观测性, W2 前馈摘要依赖标题);
/// 4. `transform` 数组存在且逐条通过 `validate_transform_list`
///    (防落盘后 loader 拒载的废文件入库)。
fn validate_meta_rule_content(raw: &str) -> WorkspaceResult<()> {
    let meta: Value = serde_json::from_str(raw)
        .map_err(|e| WorkspaceError::invalid_input(format!("meta_rule_content 非法 JSON: {e}")))?;
    if !meta.is_object() {
        return Err(WorkspaceError::invalid_input(
            "meta_rule_content 必须是 JSON 对象 (含 metadata + transform)",
        ));
    }
    let tier = meta.pointer("/metadata/tier").and_then(|t| t.as_str());
    if tier != Some("meta") {
        return Err(WorkspaceError::invalid_input(format!(
            "meta_rule_content 必须声明 metadata.tier=\"meta\" (实际 {tier:?})"
        )));
    }
    let title = meta
        .pointer("/metadata/title")
        .and_then(|t| t.as_str())
        .unwrap_or("");
    if title.trim().is_empty() {
        return Err(WorkspaceError::invalid_input(
            "meta_rule_content 必须声明非空 metadata.title",
        ));
    }
    let transforms = meta
        .get("transform")
        .and_then(|t| t.as_array())
        .ok_or_else(|| {
            WorkspaceError::invalid_input("meta_rule_content 必须包含 transform 数组")
        })?;
    let report = evorule_rule_schema::validate_transform_list(&Value::Array(transforms.clone()));
    if !report.valid {
        return Err(WorkspaceError::invalid_input(format!(
            "meta_rule_content 未通过 Schema 门禁（引擎原生结构非法）: {}",
            report.errors.join("; ")
        )));
    }
    Ok(())
}

/// 由发布队列项构造规范 DatasetBundle (审计⑥ 批 B C5)
///
/// 构造是确定性的:
/// - `bundle_id` = `publish-{ruleset_hash 前 16 hex}` — 同规则集同 bundle_id (重试幂等落盘替换);
/// - `entry_id` = `rule-{序号}-{内容哈希前 12 hex}` — 确定性 + 同 bundle 内防碰撞;
/// - 版本选择 `pinned` 到 `v1` (Versioning::default 链), 无墙钟依赖;
/// - `tests.verdict = Pass` 仅在闸门一证据检查通过后才会被调用 (execute_publish 前置);
/// - `data_dependencies = None` — 治理侧数据依赖声明属 dataset 资产范畴,
///   MVP 发布链规则集为原生 JSON 规则数组, 无服务依赖声明 (符号三方一致校验自然通过)。
///
/// 全包哈希在构造末尾计算 (`compute_content_hash`), 保证 BundleImporter 防篡改校验通过。
fn build_publish_bundle(
    rules: &[Value],
    item: &PublishQueueItem,
    new_version: i64,
    published_by: &str,
) -> evorule_bundle::DatasetBundle {
    use evorule_bundle::{
        BundleAudit, BundleDatasetMeta, BundleEntry, BundleTests, Provenance, VersionSelection,
        VersionSelectionMode, Versioning, BUNDLE_SCHEMA_VERSION,
    };

    let hash_prefix = item.ruleset_hash.get(..16).unwrap_or(&item.ruleset_hash);
    let bundle_id = format!("publish-{hash_prefix}");

    let entries: Vec<BundleEntry> = rules
        .iter()
        .enumerate()
        .map(|(i, rule)| {
            let rule_str = serde_json::to_string(rule).unwrap_or_default();
            let content_hash = evorule_hash::digest(rule_str.as_bytes());
            let hash_prefix = content_hash.get(..12).unwrap_or(&content_hash);
            BundleEntry {
                entry_id: format!("rule-{i:02}-{hash_prefix}"),
                entry_kind: Default::default(),
                rule_body: rule.clone(),
                schema_ref: None,
                provenance: Provenance {
                    source: format!("publish_queue#{}", item.id),
                    clause: None,
                    document_id: None,
                    effective_from: None,
                    effective_to: None,
                    last_verified: None,
                    verified_by: Some(published_by.to_string()),
                },
                domain: "general".to_string(),
                tags: Vec::new(),
                dependencies: Vec::new(),
            }
        })
        .collect();

    let mut bundle = evorule_bundle::DatasetBundle {
        bundle_schema_version: BUNDLE_SCHEMA_VERSION.to_string(),
        bundle_id,
        dataset: BundleDatasetMeta {
            dataset_id: item.workspace_id.clone(),
            name: format!("publish:{}:{}", item.workspace_id, item.id),
            tenant_id: "local".to_string(),
            instance_id: "evorule-server".to_string(),
            versioning: Versioning::default(),
            version_selection: Some(VersionSelection {
                mode: VersionSelectionMode::Pinned,
                pinned_version: Some("v1".to_string()),
                pinned_include_patch: None,
            }),
            law_ref: None,
            view_of: None,
            event_schemas: vec![],
        },
        entries,
        data_dependencies: None,
        tests: BundleTests {
            // : 证据如实携带——闸门一已验证沙盒 closed + 报告 failed==0,
            // subset 携带 sandbox:<id> 可追溯标记(旧实现硬编码空 subset + Pass,
            // 属假证据形态;verdict 现为闸门一验证后的派生值,非无条件 Pass)
            subset: item
                .test_report_sandbox_id
                .map(|sid| vec![format!("sandbox:{sid}")])
                .unwrap_or_default(),
            fixtures: Vec::new(),
            verdict: evorule_bundle::TestVerdict::Pass,
        },
        audit: BundleAudit {
            // exported_at 为管理元数据 (墙钟旁路): 参与 bundle 自身防篡改哈希,
            // 但不渗入 fact / 审计验证链 (与 bundle_imports 溯源同口径)
            exported_at: chrono::Utc::now().to_rfc3339(),
            exported_by: published_by.to_string(),
            source_version: format!("v{new_version}"),
            content_hash: String::new(),
            hash_algo: "blake3".to_string(),
        },
    };
    bundle.audit.content_hash = bundle.compute_content_hash();
    bundle
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::db::WorkspaceDb;
    use crate::models::{
        CreateRuleRequest, CreateWorkspaceRequest, RuleVersionState, UpdateRuleContentRequest,
    };
    use crate::rolling_session::RollingSessionService;
    use crate::session_bridge::SessionOps;
    use crate::session_switched::SessionSwitchedBroadcaster;
    use crate::workspace_service::WorkspaceService;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Mutex;

    struct MockSessionOps {
        next_id: AtomicU64,
        /// 注入 reload_rules 失败 (用于测试发布失败时队列保持 pending 可重试)
        reload_fail: AtomicBool,
        /// reload_rules 调用计数 (meta_promotion 流程断言 reload 被触发)
        reload_count: AtomicU64,
        /// 已创建的 session id (session_exists 依据)
        created: Mutex<std::collections::HashSet<u64>>,
    }
    impl MockSessionOps {
        fn new(start: u64) -> Self {
            Self {
                next_id: AtomicU64::new(start),
                reload_fail: AtomicBool::new(false),
                reload_count: AtomicU64::new(0),
                created: Mutex::new(std::collections::HashSet::new()),
            }
        }
    }
    #[async_trait]
    impl SessionOps for MockSessionOps {
        async fn create_session(&self) -> WorkspaceResult<u64> {
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            self.created.lock().unwrap().insert(id);
            Ok(id)
        }
        async fn fork_session(&self, _parent: u64) -> WorkspaceResult<u64> {
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            self.created.lock().unwrap().insert(id);
            Ok(id)
        }
        async fn session_exists(&self, session_id: u64) -> bool {
            self.created.lock().unwrap().contains(&session_id)
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
            self.reload_count.fetch_add(1, Ordering::SeqCst);
            if self.reload_fail.load(Ordering::SeqCst) {
                return Err(WorkspaceError::internal(
                    "mock reload_rules failed (injected)",
                ));
            }
            Ok(())
        }
        async fn flush_audit(&self, _id: u64) -> WorkspaceResult<usize> {
            Ok(0)
        }
    }

    /// 构建测试用 PublishService + 依赖
    ///
    /// 返回 `Arc<MockSessionOps>` 句柄 (测试可注入 reload_rules 失败以模拟发布失败)
    /// 与临时 rules_dir (TempDir guard, 供发布链落盘断言)。
    async fn make_services() -> (
        PublishService,
        Arc<WorkspaceDb>,
        Arc<RuleMetaServiceHandle>,
        String,
        Arc<MockSessionOps>,
        tempfile::TempDir,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let rules_dir = tmp.path().join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        let db = Arc::new(WorkspaceDb::in_memory().unwrap());
        let ops = Arc::new(MockSessionOps::new(1000));
        let dyn_ops: Arc<dyn SessionOps> = ops.clone();
        let switcher = SessionSwitchedBroadcaster::new();
        let rolling = RollingSessionService::new(db.clone(), dyn_ops.clone(), switcher);
        let publish_svc = PublishService::new(db.clone(), rolling, rules_dir);
        let ws_svc = Arc::new(WorkspaceService::new(db.clone(), dyn_ops));
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
        (publish_svc, db, rule_svc, ws_id, ops, tmp)
    }

    /// 测试辅助: 创建已关闭 (closed) 的沙盒会话, 提供闸门一证据
    fn make_sandbox_evidence(db: &WorkspaceDb, ws_id: &str) -> i64 {
        // : 闸门一升级后须 closed + PASS 报告文件——报告名加原子序号
        // 防并行测试写同名文件互相覆盖;export_path basename 与报告文件名对应
        // (与 close_sandbox/generate_test_report 落盘口径一致)
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let export_path = format!("./data/test_reports/report-{n}.json");
        let sid = db
            .insert_sandbox_session(None, ws_id, 100, None, 1, "head-1")
            .unwrap();
        db.close_sandbox_session(sid, &export_path).unwrap();
        std::fs::create_dir_all(crate::SANDBOX_REPORT_DIR).unwrap();
        std::fs::write(
            format!("{}/report_report-{n}.json", crate::SANDBOX_REPORT_DIR),
            r#"{"summary": {"total_cases": 1, "passed": 1, "failed": 0, "skipped": 0}}"#,
        )
        .unwrap();
        sid
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
        // 合法元指令 (set): 发布链 Schema 门禁 (审计⑥ 批 B C1) 会拦截非法结构,
        // 测试规则必须通过 evorule_rule_schema::validate_rule_input
        let content = r#"{"transform":[{"type":"set","params":{"attr":"result","operation":"set","value":"ok"}}]}"#;
        let rule = rule_svc
            .create_rule(
                ws_id,
                CreateRuleRequest {
                    name: name.to_string(),
                    content: content.to_string(),
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
        let (publish_svc, _db, _rule_svc, ws_id, _ops, _tmp) = make_services().await;

        let result = publish_svc
            .submit_publish(
                SubmitPublishRequest {
                    workspace_id: ws_id,
                    rule_version_ids: vec!["rv-1".to_string()],
                    test_report_sandbox_id: None,
                    description: None,
                    kind: PublishKind::Normal,
                    meta_rule_content: None,
                },
                "doctor-1",
                &PublishRole::Doctor,
            )
            .await;
        assert!(matches!(result, Err(WorkspaceError::Forbidden(_))));
    }

    #[tokio::test]
    async fn test_submit_publish_by_department_head() {
        let (publish_svc, db, rule_svc_handle, ws_id, _ops, _tmp) = make_services().await;
        let rv_id = make_candidate_rule(&rule_svc_handle.inner, &db, &ws_id, "rule-1").await;

        let item = publish_svc
            .submit_publish(
                SubmitPublishRequest {
                    workspace_id: ws_id.clone(),
                    rule_version_ids: vec![rv_id],
                    test_report_sandbox_id: None,
                    description: Some("内科规则发布".to_string()),
                    kind: PublishKind::Normal,
                    meta_rule_content: None,
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
        assert!(item.final_candidate_rules.contains("\"attr\":\"result\""));
    }

    #[tokio::test]
    async fn test_review_publish_full_flow() {
        let (publish_svc, db, rule_svc_handle, ws_id, _ops, tmp) = make_services().await;
        let rv_id = make_candidate_rule(&rule_svc_handle.inner, &db, &ws_id, "rule-1").await;
        let sandbox_id = make_sandbox_evidence(&db, &ws_id);

        // 科室主任提交 (携带闸门一证据)
        let item = publish_svc
            .submit_publish(
                SubmitPublishRequest {
                    workspace_id: ws_id.clone(),
                    rule_version_ids: vec![rv_id],
                    test_report_sandbox_id: Some(sandbox_id),
                    description: None,
                    kind: PublishKind::Normal,
                    meta_rule_content: None,
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

        // 审计⑥ 批 B C5: 发布链闭环 — DatasetBundle 已落盘 rules_dir
        let bundles_dir = tmp.path().join("rules/bundles");
        let entries = std::fs::read_dir(&bundles_dir).unwrap().count();
        assert_eq!(entries, 1, "应恰好落盘一个 bundle 目录");
        let bundle_dir = std::fs::read_dir(&bundles_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(
            bundle_dir
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("publish-"))
                .unwrap_or(false),
            "bundle_id 应为 publish-{{hash}} 形式: {:?}",
            bundle_dir
        );
        // manifest 落盘且 dataset_id = workspace_id
        let manifest_raw =
            std::fs::read_to_string(bundle_dir.join(evorule_bundle::BUNDLE_MANIFEST_FILE)).unwrap();
        let manifest: serde_json::Value = serde_json::from_str(&manifest_raw).unwrap();
        assert_eq!(manifest["dataset_id"], ws_id);
        assert_eq!(manifest["source_version"], "v1");
        // 规则条目落盘 (rule_body 原样零转译)
        let rule_files: Vec<_> = std::fs::read_dir(&bundle_dir)
            .unwrap()
            .filter_map(|e| e.unwrap().file_name().into_string().ok())
            .filter(|n| n.ends_with(".json") && n != evorule_bundle::BUNDLE_MANIFEST_FILE)
            .collect();
        assert_eq!(rule_files.len(), 1, "应落盘恰好一个规则条目文件");
    }

    #[tokio::test]
    async fn test_review_publish_rejected() {
        let (publish_svc, _db, rule_svc_handle, ws_id, _ops, _tmp) = make_services().await;
        let rv_id = make_candidate_rule(&rule_svc_handle.inner, &_db, &ws_id, "rule-1").await;

        let item = publish_svc
            .submit_publish(
                SubmitPublishRequest {
                    workspace_id: ws_id,
                    rule_version_ids: vec![rv_id],
                    test_report_sandbox_id: None,
                    description: None,
                    kind: PublishKind::Normal,
                    meta_rule_content: None,
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
        let (publish_svc, _db, _rule_svc, _ws_id, _ops, _tmp) = make_services().await;

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
        let (publish_svc, db, rule_svc_handle, ws_id, _ops, _tmp) = make_services().await;
        let rv_id = make_candidate_rule(&rule_svc_handle.inner, &db, &ws_id, "rule-1").await;
        let sandbox_id = make_sandbox_evidence(&db, &ws_id);

        // v1: 发布
        let item = publish_svc
            .submit_publish(
                SubmitPublishRequest {
                    workspace_id: ws_id.clone(),
                    rule_version_ids: vec![rv_id.clone()],
                    test_report_sandbox_id: Some(sandbox_id),
                    description: None,
                    kind: PublishKind::Normal,
                    meta_rule_content: None,
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
                    test_report_sandbox_id: Some(sandbox_id),
                    description: None,
                    kind: PublishKind::Normal,
                    meta_rule_content: None,
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
        let (publish_svc, _db, _rule_svc, _ws_id, _ops, _tmp) = make_services().await;

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

    #[tokio::test]
    async fn test_review_publish_publish_failure_keeps_queue_pending() {
        // 前置缺陷修复回归: 发布失败时队列项必须保持 pending (可重试),
        // 不残留孤儿 approved 状态 (原实现先置 approved 再发布, 失败永久卡死)。
        let (publish_svc, db, rule_svc_handle, ws_id, ops, _tmp) = make_services().await;
        let rv_id = make_candidate_rule(&rule_svc_handle.inner, &db, &ws_id, "rule-1").await;
        let sandbox_id = make_sandbox_evidence(&db, &ws_id);

        let item = publish_svc
            .submit_publish(
                SubmitPublishRequest {
                    workspace_id: ws_id.clone(),
                    rule_version_ids: vec![rv_id],
                    test_report_sandbox_id: Some(sandbox_id),
                    description: None,
                    kind: PublishKind::Normal,
                    meta_rule_content: None,
                },
                "head-1",
                &PublishRole::DepartmentHead,
            )
            .await
            .unwrap();

        // 注入 reload_rules 失败 → execute_publish (rolling_swap) 返回 Err
        ops.reload_fail.store(true, Ordering::SeqCst);

        let result = publish_svc
            .review_publish(
                item.id,
                ReviewPublishRequest {
                    decision: "approved".to_string(),
                    comment: Some("通过".to_string()),
                },
                "admin-1",
                &PublishRole::Admin,
            )
            .await;
        assert!(result.is_err());

        // 队列项保持 pending (可重试), 不被孤儿 approved 卡死
        let after = db.get_publish_queue_item(item.id).unwrap().unwrap();
        assert_eq!(after.status, PublishStatus::Pending);

        // production_state 未被改动 (发布未生效)
        let state = db.get_production_state().unwrap();
        assert_eq!(state.ruleset_version, 0);

        // 恢复后重试 → 发布成功
        ops.reload_fail.store(false, Ordering::SeqCst);
        let published = publish_svc
            .review_publish(
                item.id,
                ReviewPublishRequest {
                    decision: "approved".to_string(),
                    comment: Some("重试通过".to_string()),
                },
                "admin-1",
                &PublishRole::Admin,
            )
            .await
            .unwrap();
        assert_eq!(published.status, PublishStatus::Published);
        assert_eq!(published.published_version, Some(1));
        assert_eq!(published.reviewed_by.as_deref(), Some("admin-1"));
        assert_eq!(published.review_comment.as_deref(), Some("重试通过"));
    }

    #[tokio::test]
    async fn test_submit_publish_rejects_stale_version() {
        // 前置缺陷修复回归: 只能发布规则的当前版本, 禁止发布被覆盖的旧版本 (Superseded)。
        let (publish_svc, _db, rule_svc_handle, ws_id, _ops, _tmp) = make_services().await;
        let rule_svc = &rule_svc_handle.inner;

        // 建规则 (Draft, v1 为当前版本)
        let rule = rule_svc
            .create_rule(
                &ws_id,
                CreateRuleRequest {
                    name: "rule-stale".to_string(),
                    content: r#"{"transform":[{"type":"set","params":{"attr":"result","operation":"set","value":"ok"}}]}"#.to_string(),
                    created_by: "head-1".to_string(),
                    description: None,
                },
            )
            .await
            .unwrap();
        let v1_id = rule.current_version_id.clone().unwrap();

        // 更新内容 → 生成 v2, v1 被 superseded
        rule_svc
            .update_rule_content(
                &ws_id,
                &rule.id,
                UpdateRuleContentRequest {
                    content: r#"{"transform":[{"type":"set","params":{"attr":"result","operation":"set","value":"ok"}},{"type":"set","params":{"attr":"extra","operation":"set","value":1}}]}"#.to_string(),
                    updated_by: "head-1".to_string(),
                },
            )
            .await
            .unwrap();
        let versions = rule_svc.list_rule_versions(&ws_id, &rule.id).await.unwrap();
        let v2_id = versions
            .iter()
            .find(|v| v.state == RuleVersionState::Current)
            .unwrap()
            .id
            .clone();

        // 提交为 Candidate
        rule_svc.submit_rule(&ws_id, &rule.id).await.unwrap();

        // 提交旧版本 v1 → 拒绝
        let result = publish_svc
            .submit_publish(
                SubmitPublishRequest {
                    workspace_id: ws_id.clone(),
                    rule_version_ids: vec![v1_id],
                    test_report_sandbox_id: None,
                    description: None,
                    kind: PublishKind::Normal,
                    meta_rule_content: None,
                },
                "head-1",
                &PublishRole::DepartmentHead,
            )
            .await;
        assert!(matches!(result, Err(WorkspaceError::InvalidInput(_))));

        // 提交当前版本 v2 → 成功
        let item = publish_svc
            .submit_publish(
                SubmitPublishRequest {
                    workspace_id: ws_id,
                    rule_version_ids: vec![v2_id],
                    test_report_sandbox_id: None,
                    description: None,
                    kind: PublishKind::Normal,
                    meta_rule_content: None,
                },
                "head-1",
                &PublishRole::DepartmentHead,
            )
            .await
            .unwrap();
        assert_eq!(item.status, PublishStatus::Pending);
    }

    #[tokio::test]
    async fn test_publish_requires_sandbox_evidence() {
        // 审计⑥ 批 B C1: 闸门一证据检查 （决策: 未验证不得默认 Pass） —
        // 未关联沙盒测试的发布必须被拒绝, 不落盘不生效, 队列保持 pending 可重试。
        let (publish_svc, db, rule_svc_handle, ws_id, _ops, tmp) = make_services().await;
        let rv_id = make_candidate_rule(&rule_svc_handle.inner, &db, &ws_id, "rule-1").await;

        // 提交时不携带 test_report_sandbox_id
        let item = publish_svc
            .submit_publish(
                SubmitPublishRequest {
                    workspace_id: ws_id.clone(),
                    rule_version_ids: vec![rv_id],
                    test_report_sandbox_id: None,
                    description: None,
                    kind: PublishKind::Normal,
                    meta_rule_content: None,
                },
                "head-1",
                &PublishRole::DepartmentHead,
            )
            .await
            .unwrap();

        let result = publish_svc
            .review_publish(
                item.id,
                ReviewPublishRequest {
                    decision: "approved".to_string(),
                    comment: None,
                },
                "admin-1",
                &PublishRole::Admin,
            )
            .await;
        assert!(matches!(result, Err(WorkspaceError::InvalidInput(msg)) if msg.contains("闸门一")));

        // 队列保持 pending (补齐证据后可重试)
        let after = db.get_publish_queue_item(item.id).unwrap().unwrap();
        assert_eq!(after.status, PublishStatus::Pending);

        // production_state 未改动, rules_dir 无落盘 (发布未生效)
        let state = db.get_production_state().unwrap();
        assert_eq!(state.ruleset_version, 0);
        assert!(!tmp.path().join("rules/bundles").exists());
    }

    /// 测试辅助: 创建已关闭沙盒, 可选写入指定内容的报告文件
    ///
    /// `report_json = None` → 不写报告文件 (模拟报告缺失);
    /// 报告名含进程 ID + 原子序号: 进程内序号防并行测试同名覆盖,
    /// 进程 ID 防跨测试运行的残留同名文件干扰 (如"缺失"场景误读上次
    /// 运行残留的损坏报告)。与 make_sandbox_evidence 落盘口径一致
    /// (report_ + export_path basename)。
    fn make_closed_sandbox(db: &WorkspaceDb, ws_id: &str, report_json: Option<&str>) -> i64 {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let tag = format!("closed-{}-{n}", std::process::id());
        let export_path = format!("./data/test_reports/{tag}.json");
        let sid = db
            .insert_sandbox_session(None, ws_id, 100, None, 1, "head-1")
            .unwrap();
        db.close_sandbox_session(sid, &export_path).unwrap();
        if let Some(json) = report_json {
            std::fs::create_dir_all(crate::SANDBOX_REPORT_DIR).unwrap();
            std::fs::write(
                format!("{}/report_{tag}.json", crate::SANDBOX_REPORT_DIR),
                json,
            )
            .unwrap();
        }
        sid
    }

    /// 测试辅助: 携带指定沙盒证据提交发布并审批通过, 断言被闸门一拒绝
    ///
    /// 同时断言 fail-closed 副作用: 队列保持 pending 可重试、版本不推进。
    async fn expect_gate_one_rejection(
        publish_svc: &PublishService,
        db: &WorkspaceDb,
        rule_svc_handle: &RuleMetaServiceHandle,
        ws_id: &str,
        sandbox_id: i64,
        expect_msg: &str,
    ) {
        let rv_id = {
            // 原子序号生成唯一规则名: 同一测试可能多次触发本辅助 (UNIQUE 约束)
            static RULE_SEQ: AtomicU64 = AtomicU64::new(0);
            let n = RULE_SEQ.fetch_add(1, Ordering::SeqCst);
            make_candidate_rule(&rule_svc_handle.inner, db, ws_id, &format!("rule-gate-{n}")).await
        };
        let item = publish_svc
            .submit_publish(
                SubmitPublishRequest {
                    workspace_id: ws_id.to_string(),
                    rule_version_ids: vec![rv_id],
                    test_report_sandbox_id: Some(sandbox_id),
                    description: None,
                    kind: PublishKind::Normal,
                    meta_rule_content: None,
                },
                "head-1",
                &PublishRole::DepartmentHead,
            )
            .await
            .unwrap();
        let result = publish_svc
            .review_publish(
                item.id,
                ReviewPublishRequest {
                    decision: "approved".to_string(),
                    comment: None,
                },
                "admin-1",
                &PublishRole::Admin,
            )
            .await;
        match &result {
            Err(WorkspaceError::InvalidInput(msg)) => assert!(
                msg.contains(expect_msg),
                "闸门一错误消息应含 {expect_msg:?}, 实际: {msg}"
            ),
            other => panic!("应被闸门一拒绝(InvalidInput), 实际: {other:?}"),
        }
        // fail-closed: 队列保持 pending (补齐证据后可重试), 版本不推进
        let after = db.get_publish_queue_item(item.id).unwrap().unwrap();
        assert_eq!(after.status, PublishStatus::Pending);
        assert_eq!(db.get_production_state().unwrap().ruleset_version, 0);
    }

    #[tokio::test]
    async fn test_publish_rejects_running_sandbox() {
        // : 未关闭沙盒 (running, 测试未完成) 不得作为发布证据
        let (publish_svc, db, rule_svc_handle, ws_id, _ops, _tmp) = make_services().await;
        let sid = db
            .insert_sandbox_session(None, &ws_id, 100, None, 1, "head-1")
            .unwrap(); // 不 close → running
        expect_gate_one_rejection(
            &publish_svc,
            &db,
            &rule_svc_handle,
            &ws_id,
            sid,
            "非 closed",
        )
        .await;
    }

    #[tokio::test]
    async fn test_publish_rejects_fail_report() {
        // : FAIL 报告 (有失败用例) 不得作为发布证据
        let (publish_svc, db, rule_svc_handle, ws_id, _ops, _tmp) = make_services().await;
        let sid = make_closed_sandbox(
            &db,
            &ws_id,
            Some(r#"{"summary": {"total_cases": 2, "passed": 1, "failed": 1, "skipped": 0}}"#),
        );
        expect_gate_one_rejection(&publish_svc, &db, &rule_svc_handle, &ws_id, sid, "失败用例")
            .await;
    }

    #[tokio::test]
    async fn test_publish_rejects_missing_report_file() {
        // : 已关闭但报告文件缺失 (被清理/修复前历史沙盒) → fail-closed 拒发布
        let (publish_svc, db, rule_svc_handle, ws_id, _ops, _tmp) = make_services().await;
        let sid = make_closed_sandbox(&db, &ws_id, None);
        expect_gate_one_rejection(
            &publish_svc,
            &db,
            &rule_svc_handle,
            &ws_id,
            sid,
            "报告文件缺失",
        )
        .await;
    }

    #[tokio::test]
    async fn test_publish_rejects_malformed_report() {
        // : 报告损坏 (非法 JSON) / 结构异常 (缺 summary.failed 字段) → fail-closed 拒发布
        let (publish_svc, db, rule_svc_handle, ws_id, _ops, _tmp) = make_services().await;
        let corrupted = make_closed_sandbox(&db, &ws_id, Some("not-a-json{{{"));
        expect_gate_one_rejection(
            &publish_svc,
            &db,
            &rule_svc_handle,
            &ws_id,
            corrupted,
            "报告文件损坏",
        )
        .await;
        let no_field = make_closed_sandbox(&db, &ws_id, Some(r#"{"summary": {"total_cases": 1}}"#));
        expect_gate_one_rejection(
            &publish_svc,
            &db,
            &rule_svc_handle,
            &ws_id,
            no_field,
            "结构异常",
        )
        .await;
    }

    #[test]
    fn test_build_publish_bundle_carries_sandbox_evidence() {
        // : 发布 bundle 证据如实携带——tests.subset 含 sandbox:<id> 可追溯标记,
        // verdict 为闸门一验证后的派生 Pass (旧实现硬编码空 subset + 无条件 Pass, 属假证据形态)。
        // 注: 落盘的 bundle_manifest.json 为 BundleManifest 结构, 不含 tests 字段;
        // tests 证据在 DatasetBundle 内存形态中由 BundleImporter 校验链消费。
        let make_item = |sandbox_id: Option<i64>| PublishQueueItem {
            id: 1,
            workspace_id: "ws-1".to_string(),
            final_candidate_rules: "[]".to_string(),
            ruleset_hash: "hash".to_string(),
            test_report_sandbox_id: sandbox_id,
            submitted_by: "head-1".to_string(),
            submitted_at: chrono::Utc::now(),
            reviewed_by: None,
            reviewed_at: None,
            review_comment: None,
            published_version: None,
            published_at: None,
            status: PublishStatus::Pending,
            description: None,
            kind: PublishKind::Normal,
            meta_rule_content: None,
        };
        let rules = vec![serde_json::json!({"key": "a"})];

        // 携带沙盒证据 → subset 如实携带可追溯标记
        let bundle = build_publish_bundle(&rules, &make_item(Some(42)), 1, "admin-1");
        assert_eq!(bundle.tests.subset, vec!["sandbox:42".to_string()]);
        assert_eq!(bundle.tests.verdict, evorule_bundle::TestVerdict::Pass);

        // 无沙盒证据 → subset 为空 (该形态由闸门一拦截, 走不到 build_publish_bundle)
        let bundle_no_ev = build_publish_bundle(&rules, &make_item(None), 1, "admin-1");
        assert!(bundle_no_ev.tests.subset.is_empty());
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

    // ===== UV-145 W3: 元规则晋升通道 =====

    /// 测试辅助: 构造元规则转写产物 JSON 字符串
    ///
    /// `tier = None` → 不输出 tier 字段 (测正向门禁缺失形态)
    fn make_meta_content(tier: Option<&str>) -> String {
        let tier_field = match tier {
            Some(t) => format!(r#""tier":"{t}","#),
            None => String::new(),
        };
        format!(
            r#"{{"kind":"rule_set","metadata":{{{tier_field}"title":"禁改守卫状态哨兵"}},"transform":[{{"type":"set","params":{{"attr":"result","operation":"set","value":"ok"}}}}]}}"#
        )
    }

    /// 测试辅助: 携带指定参数提交元规则晋升
    #[allow(clippy::too_many_arguments)]
    async fn submit_meta_promotion(
        publish_svc: &PublishService,
        ws_id: &str,
        rv_id: String,
        sandbox_id: Option<i64>,
        meta_content: Option<String>,
    ) -> WorkspaceResult<PublishQueueItem> {
        publish_svc
            .submit_publish(
                SubmitPublishRequest {
                    workspace_id: ws_id.to_string(),
                    rule_version_ids: vec![rv_id],
                    test_report_sandbox_id: sandbox_id,
                    description: None,
                    kind: PublishKind::MetaPromotion,
                    meta_rule_content: meta_content,
                },
                "head-1",
                &PublishRole::DepartmentHead,
            )
            .await
    }

    #[tokio::test]
    async fn test_submit_meta_promotion_requires_content() {
        let (publish_svc, _db, rule_svc_handle, ws_id, _ops, _tmp) = make_services().await;
        let rv_id = make_candidate_rule(&rule_svc_handle.inner, &_db, &ws_id, "rule-1").await;

        let result = submit_meta_promotion(&publish_svc, &ws_id, rv_id, None, None).await;
        assert!(
            matches!(result, Err(WorkspaceError::InvalidInput(ref msg)) if msg.contains("meta_rule_content")),
            "缺转写产物应被拒: {result:?}"
        );
    }

    #[tokio::test]
    async fn test_submit_meta_promotion_rejects_missing_tier() {
        let (publish_svc, _db, rule_svc_handle, ws_id, _ops, _tmp) = make_services().await;
        let rv_id = make_candidate_rule(&rule_svc_handle.inner, &_db, &ws_id, "rule-1").await;

        // tier 字段缺失 (正向门禁: L2 必须声明 metadata.tier=meta)
        let result = submit_meta_promotion(
            &publish_svc,
            &ws_id,
            rv_id.clone(),
            None,
            Some(make_meta_content(None)),
        )
        .await;
        assert!(
            matches!(result, Err(WorkspaceError::InvalidInput(ref msg)) if msg.contains("tier")),
            "缺 tier 声明应被拒: {result:?}"
        );

        // tier 非法值
        let result = submit_meta_promotion(
            &publish_svc,
            &ws_id,
            rv_id,
            None,
            Some(make_meta_content(Some("business"))),
        )
        .await;
        assert!(matches!(result, Err(WorkspaceError::InvalidInput(_))));
    }

    #[tokio::test]
    async fn test_submit_meta_promotion_rejects_invalid_transform() {
        let (publish_svc, _db, rule_svc_handle, ws_id, _ops, _tmp) = make_services().await;
        let rv_id = make_candidate_rule(&rule_svc_handle.inner, &_db, &ws_id, "rule-1").await;

        // transform 含非法元指令 → Schema 门禁拒绝
        let bad = make_meta_content(Some("meta"))
            .replace(r#""type":"set""#, r#""type":"no_such_instruction""#);
        let result = submit_meta_promotion(&publish_svc, &ws_id, rv_id, None, Some(bad)).await;
        assert!(
            matches!(result, Err(WorkspaceError::InvalidInput(ref msg)) if msg.contains("Schema 门禁")),
            "非法 transform 应被拒: {result:?}"
        );
    }

    #[tokio::test]
    async fn test_meta_promotion_requires_sandbox_evidence() {
        // 晋升同普通发布走闸门一 (零报警证据, fail-closed)
        let (publish_svc, db, rule_svc_handle, ws_id, _ops, tmp) = make_services().await;
        let rv_id = make_candidate_rule(&rule_svc_handle.inner, &db, &ws_id, "rule-1").await;

        let item = submit_meta_promotion(
            &publish_svc,
            &ws_id,
            rv_id,
            None,
            Some(make_meta_content(Some("meta"))),
        )
        .await
        .unwrap();

        let result = publish_svc
            .review_publish(
                item.id,
                ReviewPublishRequest {
                    decision: "approved".to_string(),
                    comment: None,
                },
                "admin-1",
                &PublishRole::Admin,
            )
            .await;
        assert!(
            matches!(result, Err(WorkspaceError::InvalidInput(ref msg)) if msg.contains("闸门一")),
            "无证据晋升应被闸门一拒绝: {result:?}"
        );

        // fail-closed: 队列保持 pending 可重试, rules_dir 无落盘
        let after = db.get_publish_queue_item(item.id).unwrap().unwrap();
        assert_eq!(after.status, PublishStatus::Pending);
        let rules_dir = tmp.path().join("rules");
        let no_meta = std::fs::read_dir(&rules_dir)
            .unwrap()
            .filter_map(|e| e.unwrap().file_name().into_string().ok())
            .all(|n| !n.starts_with("00_meta_promoted_"));
        assert!(no_meta, "未审批通过前不得落盘 00_meta_ 文件");
    }

    #[tokio::test]
    async fn test_meta_promotion_full_flow() {
        // W3 验收: ①提名→审批→落盘全链留痕 ②非审批路径无法写 00_meta_
        // ③版本回溯可查 (audit.ruleset_snapshot); 业务 ruleset_version 不推进。
        let (publish_svc, db, rule_svc_handle, ws_id, ops, tmp) = make_services().await;
        let rv_id = make_candidate_rule(&rule_svc_handle.inner, &db, &ws_id, "rule-1").await;
        let sandbox_id = make_sandbox_evidence(&db, &ws_id);

        // ② 权限前置: Doctor 无法提交晋升 (写盘仅审批链可达)
        let doctor_result = publish_svc
            .submit_publish(
                SubmitPublishRequest {
                    workspace_id: ws_id.clone(),
                    rule_version_ids: vec![rv_id.clone()],
                    test_report_sandbox_id: Some(sandbox_id),
                    description: None,
                    kind: PublishKind::MetaPromotion,
                    meta_rule_content: Some(make_meta_content(Some("meta"))),
                },
                "doctor-1",
                &PublishRole::Doctor,
            )
            .await;
        assert!(matches!(doctor_result, Err(WorkspaceError::Forbidden(_))));

        // 提交晋升 (转写产物 + 闸门一证据)
        let item = submit_meta_promotion(
            &publish_svc,
            &ws_id,
            rv_id.clone(),
            Some(sandbox_id),
            Some(make_meta_content(Some("meta"))),
        )
        .await
        .unwrap();
        assert_eq!(item.kind, PublishKind::MetaPromotion);

        // 审批通过 → 落盘
        let published = publish_svc
            .review_publish(
                item.id,
                ReviewPublishRequest {
                    decision: "approved".to_string(),
                    comment: Some("哨兵观察期满,同意晋升".to_string()),
                },
                "admin-1",
                &PublishRole::Admin,
            )
            .await
            .unwrap();
        assert_eq!(published.status, PublishStatus::Published);

        // ① 00_meta_promoted_{hash}.json 落盘 rules_dir 根目录 + 溯源完整
        let rules_dir = tmp.path().join("rules");
        let meta_files: Vec<String> = std::fs::read_dir(&rules_dir)
            .unwrap()
            .filter_map(|e| e.unwrap().file_name().into_string().ok())
            .filter(|n| n.starts_with("00_meta_promoted_") && n.ends_with(".json"))
            .collect();
        assert_eq!(meta_files.len(), 1, "应恰好落盘一个元规则文件");
        let landed: Value =
            serde_json::from_str(&std::fs::read_to_string(rules_dir.join(&meta_files[0])).unwrap())
                .unwrap();
        assert_eq!(
            landed.pointer("/metadata/tier").and_then(|v| v.as_str()),
            Some("meta")
        );
        assert_eq!(
            landed.pointer("/metadata/title").and_then(|v| v.as_str()),
            Some("禁改守卫状态哨兵")
        );
        assert_eq!(
            landed
                .pointer("/metadata/promoted_by")
                .and_then(|v| v.as_str()),
            Some("admin-1"),
            "promoted_by 须服务端权威填充"
        );
        assert_eq!(
            landed
                .pointer("/metadata/promoted_from")
                .and_then(|v| v.as_str()),
            Some(format!("rule_version:{rv_id}").as_str()),
            "promoted_from 须锚定来源规则版本"
        );
        assert_eq!(
            landed
                .pointer("/metadata/zero_alarm_window")
                .and_then(|v| v.as_str()),
            Some(format!("sandbox:{sandbox_id}").as_str()),
            "零报警证据须可追溯"
        );

        // ③ 审计 meta_promoted + ruleset_snapshot 回溯
        let audits = db.list_production_audit(20).unwrap();
        let meta_audit = audits
            .iter()
            .find(|a| a.event_type == "meta_promoted")
            .expect("meta_promoted 审计事件应存在");
        let snap: Value =
            serde_json::from_str(meta_audit.ruleset_snapshot.as_deref().expect("快照应存在"))
                .unwrap();
        assert_eq!(
            snap.pointer("/metadata/promoted_by")
                .and_then(|v| v.as_str()),
            Some("admin-1")
        );

        // 业务版本不推进 + 不落 bundle (元规则不进业务版本序列)
        assert_eq!(db.get_production_state().unwrap().ruleset_version, 0);
        assert!(!tmp.path().join("rules/bundles").exists());

        // reload 被触发 (新会话携带新元规则)
        assert_eq!(ops.reload_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_meta_promotion_rejected_no_landing() {
        // W3 验收②: 驳回路径不落盘
        let (publish_svc, _db, rule_svc_handle, ws_id, _ops, tmp) = make_services().await;
        let rv_id = make_candidate_rule(&rule_svc_handle.inner, &_db, &ws_id, "rule-1").await;

        let item = submit_meta_promotion(
            &publish_svc,
            &ws_id,
            rv_id,
            None,
            Some(make_meta_content(Some("meta"))),
        )
        .await
        .unwrap();

        let rejected = publish_svc
            .review_publish(
                item.id,
                ReviewPublishRequest {
                    decision: "rejected".to_string(),
                    comment: Some("暂不晋升".to_string()),
                },
                "admin-1",
                &PublishRole::Admin,
            )
            .await
            .unwrap();
        assert_eq!(rejected.status, PublishStatus::Rejected);

        let rules_dir = tmp.path().join("rules");
        let no_meta = std::fs::read_dir(&rules_dir)
            .unwrap()
            .filter_map(|e| e.unwrap().file_name().into_string().ok())
            .all(|n| !n.starts_with("00_meta_promoted_"));
        assert!(no_meta, "驳回后不得落盘 00_meta_ 文件");
    }
}

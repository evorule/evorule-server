// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 数据模型 + 状态机枚举
//!
//! 设计依据: WORKSPACE_CRATE_DESIGN.md §3
//!
//! # 7 张表
//! | 表名 | Model | 用途 |
//! |------|-------|------|
//! | `workspaces` | [`WorkspaceRecord`] | 工作空间主表 |
//! | `workspace_members` | [`WorkspaceMemberRecord`] | 成员关系 |
//! | `rules` | [`RuleRecord`] | 规则元数据 |
//! | `rule_versions` | [`RuleVersionRecord`] | 规则版本内容 |
//! | `sessions` | [`SessionRecord`] | 会话记录 |
//! | `rule_session_bindings` | [`RuleSessionBinding`] | 规则会话绑定 |
//! | `schema_migrations` | (内部) | schema 版本追踪 |
//!
//! # 4 个状态机
//! - [`WorkspaceState`]: Active → Archived
//! - [`RuleState`]: Draft → Candidate → Active ↔ Blocked; * → Archived
//! - [`SessionBindingState`]: Bound → Closed
//! - [`RuleVersionState`]: Current → Superseded
//!
//! 状态机枚举的 `from_str` 是有意设计的**有损解析器**（返回 `Option<Self>`，
//! 未知字符串 → `None`，配合 db 层 `unwrap_or(默认值)` 做容错回读），
//! 与 `std::str::FromStr`（返回 `Result`）语义不同、非 trait 实现，
//! 故豁免 `clippy::should_implement_trait`（避免为规避 lint 而改名 30+ 调用点）。
#![allow(clippy::should_implement_trait)]

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// =============================================================================
// 状态机枚举
// =============================================================================

/// 工作空间状态机
///
/// Active → Archived (单向)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceState {
    /// 活跃 (可读写)
    Active,
    /// 已归档 (只读, 不可创建新规则/会话)
    Archived,
}

impl WorkspaceState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Archived => "archived",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "active" => Some(Self::Active),
            "archived" => Some(Self::Archived),
            _ => None,
        }
    }

    /// 校验状态迁移合法性
    ///
    /// 仅允许: Active → Archived
    pub fn can_transition_to(&self, target: Self) -> bool {
        matches!((self, target), (Self::Active, Self::Archived))
    }
}

/// 规则状态机
///
/// 合法迁移:
/// - Draft → Candidate (提交候选)
/// - Candidate → Active (审核通过)
/// - Candidate → Draft (退回草稿)
/// - Active → Blocked (临时阻塞)
/// - Blocked → Active (恢复)
/// - Draft/Candidate/Active/Blocked → Archived (归档)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuleState {
    /// 草稿 (可编辑)
    Draft,
    /// 候选 (待审核, 不可编辑)
    Candidate,
    /// 活跃 (已激活, 不可编辑)
    Active,
    /// 已阻塞 (临时禁用)
    Blocked,
    /// 已归档 (只读)
    Archived,
}

impl RuleState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Candidate => "candidate",
            Self::Active => "active",
            Self::Blocked => "blocked",
            Self::Archived => "archived",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "draft" => Some(Self::Draft),
            "candidate" => Some(Self::Candidate),
            "active" => Some(Self::Active),
            "blocked" => Some(Self::Blocked),
            "archived" => Some(Self::Archived),
            _ => None,
        }
    }

    /// 校验状态迁移合法性
    pub fn can_transition_to(&self, target: Self) -> bool {
        matches!(
            (self, target),
            (Self::Draft, Self::Candidate)
                | (Self::Candidate, Self::Active)
                | (Self::Candidate, Self::Draft)
                | (Self::Active, Self::Blocked)
                | (Self::Blocked, Self::Active)
                | (Self::Draft, Self::Archived)
                | (Self::Candidate, Self::Archived)
                | (Self::Active, Self::Archived)
                | (Self::Blocked, Self::Archived)
        )
    }

    /// 是否允许编辑内容 (仅 Draft 状态允许)
    pub fn is_editable(&self) -> bool {
        matches!(self, Self::Draft)
    }
}

/// 会话-规则绑定状态机
///
/// Bound → Closed (单向)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionBindingState {
    /// 绑定中 (会话活跃)
    Bound,
    /// 已关闭 (会话已结束)
    Closed,
}

impl SessionBindingState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Bound => "bound",
            Self::Closed => "closed",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "bound" => Some(Self::Bound),
            "closed" => Some(Self::Closed),
            _ => None,
        }
    }

    pub fn can_transition_to(&self, target: Self) -> bool {
        matches!((self, target), (Self::Bound, Self::Closed))
    }
}

/// 规则版本状态机
///
/// Current → Superseded (单向, 新版本激活时旧版本自动 superseded)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuleVersionState {
    /// 当前版本 (活跃)
    Current,
    /// 已被取代
    Superseded,
}

impl RuleVersionState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Superseded => "superseded",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "current" => Some(Self::Current),
            "superseded" => Some(Self::Superseded),
            _ => None,
        }
    }

    pub fn can_transition_to(&self, target: Self) -> bool {
        matches!((self, target), (Self::Current, Self::Superseded))
    }
}

// =============================================================================
// 数据模型 (7 张表)
// =============================================================================

/// 工作空间记录 (workspaces 表)
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct WorkspaceRecord {
    /// 工作空间 ID (ULID, 字典序可排序)
    pub id: String,
    /// 工作空间名称
    pub name: String,
    /// 所有者用户 ID
    pub owner_id: String,
    /// 创建时间 (UTC)
    pub created_at: DateTime<Utc>,
    /// 最后更新时间
    pub updated_at: DateTime<Utc>,
    /// 归档时间 (未归档时为 None)
    pub archived_at: Option<DateTime<Utc>>,
    /// 当前状态
    pub state: WorkspaceState,
    /// 描述 (可选)
    pub description: Option<String>,
}

/// 工作空间成员记录 (workspace_members 表)
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct WorkspaceMemberRecord {
    /// 所属工作空间 ID
    pub workspace_id: String,
    /// 成员用户 ID
    pub user_id: String,
    /// 成员角色 (owner/admin/editor/viewer)
    pub role: String,
    /// 加入时间
    pub joined_at: DateTime<Utc>,
}

/// 成员角色枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberRole {
    Owner,
    Admin,
    Editor,
    Viewer,
}

impl MemberRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Admin => "admin",
            Self::Editor => "editor",
            Self::Viewer => "viewer",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "owner" => Some(Self::Owner),
            "admin" => Some(Self::Admin),
            "editor" => Some(Self::Editor),
            "viewer" => Some(Self::Viewer),
            _ => None,
        }
    }

    /// 是否拥有写权限 (owner/admin/editor)
    pub fn can_write(&self) -> bool {
        matches!(self, Self::Owner | Self::Admin | Self::Editor)
    }

    /// 是否拥有管理权限 (owner/admin)
    pub fn can_admin(&self) -> bool {
        matches!(self, Self::Owner | Self::Admin)
    }
}

/// 规则记录 (rules 表)
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RuleRecord {
    /// 规则 ID (ULID)
    pub id: String,
    /// 所属工作空间 ID
    pub workspace_id: String,
    /// 规则名称 (工作空间内唯一)
    pub name: String,
    /// 当前活跃版本 ID (Draft 状态时为 None)
    pub current_version_id: Option<String>,
    /// 当前状态
    pub state: RuleState,
    /// 创建时间
    pub created_at: DateTime<Utc>,
    /// 最后更新时间
    pub updated_at: DateTime<Utc>,
    /// 归档时间
    pub archived_at: Option<DateTime<Utc>>,
    /// 规则描述
    pub description: Option<String>,
    /// 创建者用户 ID
    pub created_by: String,
    /// 扩展元数据 (JSON 字符串), 用于双模式编辑器、来源、标签等可扩展字段
    ///
    /// v3 schema 新增列, 永远是合法 JSON OBJECT 文本; 空时为 "{}"。
    pub metadata: String,
}

/// 规则版本记录 (rule_versions 表)
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RuleVersionRecord {
    /// 版本 ID (ULID)
    pub id: String,
    /// 所属规则 ID
    pub rule_id: String,
    /// 版本号 (从 1 递增)
    pub version: u64,
    /// 内容哈希 (BLAKE3, 用于去重和审计)
    pub content_hash: String,
    /// 规则内容 (JSON 字符串, 存储原始规则 JSON)
    pub content: String,
    /// 创建时间
    pub created_at: DateTime<Utc>,
    /// 版本状态
    pub state: RuleVersionState,
    /// 创建者用户 ID
    pub created_by: String,
}

/// 会话记录 (sessions 表)
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SessionRecord {
    /// 会话 ID (与 evorule-governance SessionManager 的 session_id 对应)
    pub id: u64,
    /// 所属工作空间 ID
    pub workspace_id: String,
    /// 绑定的规则 ID (可选, 支持无规则会话)
    pub rule_id: Option<String>,
    /// 绑定的规则版本 ID
    pub rule_version_id: Option<String>,
    /// 创建时间
    pub created_at: DateTime<Utc>,
    /// 关闭时间
    pub closed_at: Option<DateTime<Utc>>,
    /// 创建者用户 ID
    pub created_by: String,
}

/// 规则-会话绑定记录 (rule_session_bindings 表)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleSessionBinding {
    /// 绑定 ID (ULID)
    pub id: String,
    /// 规则版本 ID
    pub rule_version_id: String,
    /// 会话 ID
    pub session_id: u64,
    /// 工作空间 ID (冗余, 便于查询)
    pub workspace_id: String,
    /// 绑定时间
    pub bound_at: DateTime<Utc>,
    /// 解绑时间
    pub unbound_at: Option<DateTime<Utc>>,
    /// 绑定状态
    pub state: SessionBindingState,
}

// =============================================================================
// 沙盒会话 (sandbox_sessions 表) — SANDBOX_ORCHESTRATION_DESIGN.md §3
// =============================================================================

/// 沙盒状态机
///
/// Running → Closed (单向,关闭后不可恢复)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SandboxStatus {
    /// 运行中 (sandbox session 活跃,可注入测试数据)
    Running,
    /// 已关闭 (test Fact 已导出,session 已关闭)
    Closed,
}

impl SandboxStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Closed => "closed",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "running" => Some(Self::Running),
            "closed" => Some(Self::Closed),
            _ => None,
        }
    }

    pub fn can_transition_to(&self, target: Self) -> bool {
        matches!((self, target), (Self::Running, Self::Closed))
    }
}

/// 沙盒会话记录 (sandbox_sessions 表)
///
/// 每个 sandbox session 从 Production session fork 而来,
/// 加载 Workspace 的 Draft 规则 + 合成测试数据集,隔离运行测试。
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SandboxSession {
    /// 应用层沙盒 ID (自增)
    pub id: i64,
    /// 所属工作空间 ID
    pub workspace_id: String,
    /// TCB 层 session_id (由 SessionManager fork 分配)
    pub tcb_session_id: Option<i64>,
    /// fork 来源的 Production session_id
    pub parent_session_id: i64,
    /// 本次测试的 Draft 规则集 BLAKE3 哈希
    pub draft_ruleset_hash: Option<String>,
    /// 使用的合成数据集 ID
    pub test_dataset_id: i64,
    /// 沙盒状态
    pub status: SandboxStatus,
    /// 启动时间
    pub started_at: DateTime<Utc>,
    /// 关闭时间
    pub closed_at: Option<DateTime<Utc>>,
    /// 启动者用户 ID
    pub started_by: String,
    /// 测试 Fact 导出路径 (关闭时填充)
    pub export_path: Option<String>,
}

// =============================================================================
// 测试数据集 (test_datasets 表) — SANDBOX_ORCHESTRATION_DESIGN.md §3
// =============================================================================

/// 合成测试数据集
///
/// 存储沙盒测试用的合成数据 (P0 全合成,Q2 决策)。
/// cases_json 为 JSON 数组,每个元素是一条测试 case。
/// case 可选携带 `name`(字符串): 沙盒报告据此逐条命名(不再 "Fact #N (unknown)"),
/// 缺失时报告回退默认命名。期望断言(expected)按测试工作台配置面推进,本层不扩展。
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct TestDatasetRecord {
    /// 数据集 ID (自增)
    pub id: i64,
    /// 数据集名称
    pub name: String,
    /// 所属工作空间 ID (None = 全院共享)
    pub workspace_id: Option<String>,
    /// 测试 case 列表 (JSON 数组字符串)
    pub cases_json: String,
    /// case 数量 (冗余,便于查询)
    pub case_count: i64,
    /// 创建时间
    pub created_at: DateTime<Utc>,
    /// 创建者
    pub created_by: String,
    /// 描述
    pub description: Option<String>,
}

// =============================================================================
// 发布队列 (publish_queue 表) — PUBLISH_QUEUE_DESIGN.md §3
// =============================================================================

/// 发布角色 (P0 三级硬编码, P1 接入 P08 协作工作流的角色模型)
///
/// 三层架构 §7 三级发布权限 （决策）:
/// - Doctor: 仅可编辑 Draft, 不可提交发布
/// - DepartmentHead: 可提交到发布队列 (本科室 WS), 不可审批
/// - Admin: 可审批发布 (全院) + 紧急回滚
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PublishRole {
    /// 普通医生 (可编辑 Draft, 不可提交发布)
    Doctor,
    /// 科室主任 (可提交到发布队列, 不可审批)
    DepartmentHead,
    /// 信息科/院领导 (可审批发布 + 紧急回滚)
    Admin,
}

impl PublishRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Doctor => "doctor",
            Self::DepartmentHead => "department_head",
            Self::Admin => "admin",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "doctor" => Some(Self::Doctor),
            "department_head" => Some(Self::DepartmentHead),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }

    /// 是否可提交到发布队列 (DepartmentHead 提交; Admin 亦可——平台管理员需提交+审批双权)
    pub fn can_submit_publish(&self) -> bool {
        matches!(self, Self::DepartmentHead | Self::Admin)
    }

    /// 是否可审批发布 (仅 Admin)
    pub fn can_review_publish(&self) -> bool {
        matches!(self, Self::Admin)
    }

    /// 是否可紧急回滚 (仅 Admin)
    pub fn can_rollback(&self) -> bool {
        matches!(self, Self::Admin)
    }
}

/// 发布队列状态机
///
/// Pending → Approved → Published (正常流程)
/// Pending → Rejected (驳回)
/// Pending → Cancelled (取消)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PublishStatus {
    /// 待审批 (科室主任已提交,等待信息科审批)
    Pending,
    /// 已批准 (信息科批准,正在/已完成滚动发布)
    Approved,
    /// 已发布 (滚动 session 切换完成)
    Published,
    /// 已驳回 (信息科驳回,回 Workspace 修改)
    Rejected,
    /// 已取消 (提交者主动取消)
    Cancelled,
}

impl PublishStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Published => "published",
            Self::Rejected => "rejected",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "approved" => Some(Self::Approved),
            "published" => Some(Self::Published),
            "rejected" => Some(Self::Rejected),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// 校验状态迁移合法性
    pub fn can_transition_to(&self, target: Self) -> bool {
        matches!(
            (self, target),
            (Self::Pending, Self::Approved)
                | (Self::Pending, Self::Rejected)
                | (Self::Pending, Self::Cancelled)
                | (Self::Approved, Self::Published)
                | (Self::Approved, Self::Cancelled)
        )
    }
}

/// 发布队列项类型 (UV-145 W3 元规则晋升通道)
///
/// - Normal: 普通业务规则发布 (走 DatasetBundle 落盘 rules_dir/bundles/)
/// - MetaPromotion: 业务规则 → L2 元规则晋升 (转写产物原子落盘 rules_dir 根目录
///   `00_meta_promoted_*.json`, 不推业务 ruleset 版本, 审计 event=meta_promoted)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PublishKind {
    /// 普通业务规则发布 (默认, 存量行为不变)
    #[default]
    Normal,
    /// 元规则晋升 (L3 业务规则 → L2 元规则, 仅治理链审批可落盘)
    MetaPromotion,
}

impl PublishKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::MetaPromotion => "meta_promotion",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "normal" => Some(Self::Normal),
            "meta_promotion" => Some(Self::MetaPromotion),
            _ => None,
        }
    }
}

/// 发布队列记录 (publish_queue 表)
///
/// 三级权限审批工作流:
/// 1. 科室主任 (DepartmentHead) 提交 → status=Pending
/// 2. 信息科/院领导 (Admin) 审批 → Approved / Rejected
/// 3. Approved 后触发滚动 session 热重载 → Published
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PublishQueueItem {
    /// 队列项 ID (自增)
    pub id: i64,
    /// 来源工作空间 ID
    pub workspace_id: String,
    /// 待发布的规则集 (JSON 数组字符串,包含 final_candidate 规则内容)
    pub final_candidate_rules: String,
    /// 规则集 BLAKE3 哈希 (防篡改 + 去重)
    pub ruleset_hash: String,
    /// 附带的测试报告 sandbox_id (可选)
    pub test_report_sandbox_id: Option<i64>,
    /// 提交者用户 ID (科室主任)
    pub submitted_by: String,
    /// 提交时间
    pub submitted_at: DateTime<Utc>,
    /// 审批者用户 ID (信息科/院领导)
    pub reviewed_by: Option<String>,
    /// 审批时间
    pub reviewed_at: Option<DateTime<Utc>>,
    /// 审批意见
    pub review_comment: Option<String>,
    /// 发布版本号 (Published 后填充)
    pub published_version: Option<i64>,
    /// 发布时间
    pub published_at: Option<DateTime<Utc>>,
    /// 当前状态
    pub status: PublishStatus,
    /// 发布说明
    pub description: Option<String>,
    /// 队列项类型 (normal=普通发布 / meta_promotion=元规则晋升; UV-145 W3)
    pub kind: PublishKind,
    /// 转写后的元规则内容 (JSON 字符串, 仅 meta_promotion 时非空)
    ///
    /// 与 final_candidate_rules (业务规则原文, 溯源锚点) 分离存储:
    /// 前者回答"晋升自什么", 后者回答"落盘什么"。
    pub meta_rule_content: Option<String>,
}

// =============================================================================
// 生产状态 (production_state 表) — PUBLISH_QUEUE_DESIGN.md §4
// =============================================================================

/// 生产状态记录 (production_state 表,单行表)
///
/// 记录当前生产环境的活跃 session + 规则集版本。
/// 每次滚动发布后原子更新,版本号单调递增。
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ProductionStateRecord {
    /// 固定 ID = 1 (单行表)
    pub id: i64,
    /// 当前活跃的 production session_id
    pub current_session_id: Option<i64>,
    /// 当前规则集版本号 (单调递增,初始 0)
    pub ruleset_version: i64,
    /// 当前规则集 BLAKE3 哈希
    pub ruleset_hash: Option<String>,
    /// 最后操作者
    pub last_operated_by: Option<String>,
    /// 最后更新时间
    pub updated_at: DateTime<Utc>,
}

// =============================================================================
// 生产审计 (production_audit 表) — PUBLISH_QUEUE_DESIGN.md §8
// =============================================================================

/// 生产审计记录 (production_audit 表)
///
/// 记录每次发布/回滚事件,与 tcb BLAKE3 链互补:
/// - tcb 链: Fact 级完整性 (物理不可篡改)
/// - production_audit: 版本级可审计性 (逻辑不可篡改)
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ProductionAuditRecord {
    /// 审计记录 ID (自增)
    pub id: i64,
    /// 事件类型 (ruleset_published / ruleset_rollback)
    pub event_type: String,
    /// 本次发布的规则集版本号
    pub ruleset_version: i64,
    /// 上一版本号 (None = 初始发布)
    pub previous_version: Option<i64>,
    /// 规则集 BLAKE3 哈希
    pub ruleset_hash: String,
    /// 关联的 TCB session_id
    pub tcb_session_id: i64,
    /// 来源工作空间 ID 列表 (JSON 数组字符串)
    pub source_workspace_ids: String,
    /// 操作者
    pub operated_by: String,
    /// 操作时间
    pub operated_at: DateTime<Utc>,
    /// 回滚原因 (仅 event_type=ruleset_rollback 时填充)
    pub reason: Option<String>,
    /// 附带的测试报告路径列表 (JSON 数组字符串)
    pub test_report_paths: Option<String>,
    /// 规则集快照 (JSON 数组字符串, 发布时的完整规则内容, 用于回滚)
    ///
    /// 存储发布时刻的规则内容快照,使每次 production_audit 记录自包含,
    /// 回滚时可直接加载,无需依赖 rules 表的当前状态 (规则可能已被后续发布覆盖)。
    pub ruleset_snapshot: Option<String>,
}

// =============================================================================

/// bundle 导入溯源记录 (bundle_imports 表, T5)
///
/// 记录执行侧导入治理层快照包的历史。确定性硬约束 (00_架构边界原则.md §七):
/// bundle_id / source_version 为逻辑标识可入溯源元数据; imported_at 为**管理元数据**
/// (墙钟旁路), 绝不渗入 fact / 内容哈希 / 审计验证链 (审计链哈希不受此记录影响)。
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct BundleImportRecord {
    /// 记录 ID (自增)
    pub id: i64,
    /// 快照包 ID
    pub bundle_id: String,
    /// 数据集 ID
    pub dataset_id: String,
    /// 快照源版本 (v1 / v2 / v2.p1)
    pub source_version: String,
    /// 版本选择模式 (`auto_by_effective_date` | `pinned`, 与 evorule-bundle 枚举 snake_case 一致)
    pub selection_mode: String,
    /// pinned 已解析版本 (None = auto 模式)
    pub resolved_version: Option<String>,
    /// 快照全包防篡改哈希 (blake3)
    pub content_hash: String,
    /// 条目数
    pub entry_count: i64,
    /// 导入时间 (管理元数据, 墙钟旁路)
    pub imported_at: DateTime<Utc>,
    /// 溯源主体: 治理侧导出者 exported_by (发布链发布者), fallback "system"
    pub imported_by: String,
}

// =============================================================================
// 判定契约 (verdict_contracts 表) — 界面升级 v1.0 阶段 A.1
// =============================================================================

/// 判定契约记录 (verdict_contracts 表)
///
/// 字段对齐 实施文档 A.1 / A.3 端点契约:
/// - workspace 级配置, 条件集合 field/op/value → verdict
/// - `is_default=true` 表示该 workspace 的默认契约 (每 workspace 至多一条)
/// - 绝不进入审计链哈希 (公共层旁路, 00 §七)
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct VerdictContractRecord {
    /// 自增主键
    pub id: i64,
    /// 所属工作空间
    pub workspace_id: String,
    /// 契约名称 (如 "default", "fraud-team-b")
    pub name: String,
    /// 版本号 (同一 (workspace_id, name) 单调递增)
    pub version: i64,
    /// 条件集合 JSON: [{field, op, value, verdict}]
    pub rules_json: String,
    /// 是否为该 workspace 的默认契约
    pub is_default: bool,
    /// 创建者
    pub created_by: String,
    /// 创建时间
    pub created_at: DateTime<Utc>,
    /// 最后更新时间
    pub updated_at: DateTime<Utc>,
}

// =============================================================================
// wall-clock 版本时钟映射 (version_clock_map 表) — 界面升级 v1.0 阶段 A.1
// =============================================================================

/// 版本 ↔ 墙钟 旁路映射 (version_clock_map 表)
///
/// 设计约束 (00_架构边界原则 §六/§七):
/// - 仅做索引, 绝不写入审计链或参与哈希
/// - 不进入 evorule 仓 / TCB Fact (Fact 无 wall-clock)
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct VersionClockMapRecord {
    /// 自增主键
    pub id: i64,
    /// 关联会话 ID (FK→sessions)
    pub session_id: i64,
    /// 逻辑版本号
    pub version: i64,
    /// 版本对应的墙钟时间 (RFC3339)
    pub wall_clock: DateTime<Utc>,
    /// 来源标记 (默认 'ttd_sidecar')
    pub source: String,
}

// =============================================================================
// API 请求/响应 DTO
// =============================================================================

/// 创建工作空间请求
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateWorkspaceRequest {
    pub name: String,
    pub owner_id: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// 更新工作空间请求
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdateWorkspaceRequest {
    pub name: Option<String>,
    pub description: Option<String>,
}

/// 添加成员请求
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct AddMemberRequest {
    pub user_id: String,
    pub role: String,
}

/// 创建规则请求
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateRuleRequest {
    pub name: String,
    /// 初始内容 (JSON 字符串)
    pub content: String,
    pub created_by: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// 更新规则内容请求 (仅 Draft 状态允许)
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdateRuleContentRequest {
    /// 新内容 (JSON 字符串)
    pub content: String,
    pub updated_by: String,
}

/// 创建会话请求
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateSessionRequest {
    /// 绑定的规则 ID (可选)
    pub rule_id: Option<String>,
    /// 指定规则版本 ID (可选, 不指定则用 current_version)
    pub rule_version_id: Option<String>,
    pub created_by: String,
}

// =============================================================================
// 沙盒 + 发布队列 API DTO (SANDBOX_ORCHESTRATION_DESIGN.md §6 + PUBLISH_QUEUE_DESIGN.md §6)
// =============================================================================

/// 启动沙盒测试请求
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct StartSandboxRequest {
    /// 要测试的规则版本 ID 列表 (从 rules 表查 Draft/Candidate 状态的当前版本)
    pub rule_version_ids: Vec<String>,
    /// 合成数据集 ID (从 test_datasets 表查)
    pub test_dataset_id: i64,
    /// 可选:指定 fork 的 production session 版本 (None = 最新)
    #[serde(default)]
    pub parent_version: Option<u64>,
}

/// 启动沙盒测试响应
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct StartSandboxResponse {
    /// 应用层 sandbox_sessions.id
    pub sandbox_id: i64,
    /// SessionManager 的 session_id
    pub tcb_session_id: u64,
    /// 本次测试的 Draft 规则集 BLAKE3
    pub draft_ruleset_hash: String,
    /// 注入的测试 case 数量
    pub test_case_count: usize,
}

/// 创建测试数据集请求
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateTestDatasetRequest {
    pub name: String,
    /// 测试 case 列表 (JSON 数组字符串)
    pub cases_json: String,
    pub created_by: String,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

/// 提交发布请求
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct SubmitPublishRequest {
    pub workspace_id: String,
    /// 待发布的规则版本 ID 列表 (必须全部为 Candidate 状态; meta_promotion 时作为晋升溯源来源)
    pub rule_version_ids: Vec<String>,
    /// 附带的测试报告 sandbox_id (可选)
    #[serde(default)]
    pub test_report_sandbox_id: Option<i64>,
    /// 发布说明
    #[serde(default)]
    pub description: Option<String>,
    /// 队列项类型 (缺省 normal; UV-145 W3 元规则晋升通道)
    #[serde(default)]
    pub kind: PublishKind,
    /// 转写后的元规则内容 (JSON 字符串; 仅 kind=meta_promotion 时必填)
    ///
    /// 结构须含 metadata.tier="meta" + metadata.title + transform 数组,
    /// promoted_by/promoted_at/promoted_from/zero_alarm_window 溯源字段由服务端
    /// 审批链权威填充, 客户端提供的同名字段被覆盖 (防伪造溯源)。
    #[serde(default)]
    pub meta_rule_content: Option<String>,
}

/// 审批请求
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct ReviewPublishRequest {
    /// approved / rejected
    pub decision: String,
    #[serde(default)]
    pub comment: Option<String>,
}

/// 紧急回滚请求
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct RollbackRequest {
    pub target_version: i64,
    pub reason: String,
}

/// 列出发布队列的查询参数
#[derive(Debug, Deserialize, Default, utoipa::ToSchema)]
pub struct ListPublishQueueQuery {
    /// 按状态过滤 (pending/approved/published/rejected/cancelled)
    pub status: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_workspace_state_transitions() {
        assert!(WorkspaceState::Active.can_transition_to(WorkspaceState::Archived));
        assert!(!WorkspaceState::Archived.can_transition_to(WorkspaceState::Active));
        assert!(!WorkspaceState::Active.can_transition_to(WorkspaceState::Active));
    }

    #[test]
    fn test_rule_state_transitions() {
        assert!(RuleState::Draft.can_transition_to(RuleState::Candidate));
        assert!(RuleState::Candidate.can_transition_to(RuleState::Active));
        assert!(RuleState::Candidate.can_transition_to(RuleState::Draft));
        assert!(RuleState::Active.can_transition_to(RuleState::Blocked));
        assert!(RuleState::Blocked.can_transition_to(RuleState::Active));
        assert!(RuleState::Active.can_transition_to(RuleState::Archived));
        assert!(RuleState::Draft.can_transition_to(RuleState::Archived));
        assert!(RuleState::Candidate.can_transition_to(RuleState::Archived));
        assert!(RuleState::Blocked.can_transition_to(RuleState::Archived));
        assert!(!RuleState::Archived.can_transition_to(RuleState::Draft));
        assert!(!RuleState::Archived.can_transition_to(RuleState::Active));
        assert!(!RuleState::Draft.can_transition_to(RuleState::Active));
        assert!(!RuleState::Active.can_transition_to(RuleState::Draft));
    }

    #[test]
    fn test_rule_state_editable() {
        assert!(RuleState::Draft.is_editable());
        assert!(!RuleState::Candidate.is_editable());
        assert!(!RuleState::Active.is_editable());
        assert!(!RuleState::Blocked.is_editable());
        assert!(!RuleState::Archived.is_editable());
    }

    #[test]
    fn test_session_binding_state_transitions() {
        assert!(SessionBindingState::Bound.can_transition_to(SessionBindingState::Closed));
        assert!(!SessionBindingState::Closed.can_transition_to(SessionBindingState::Bound));
    }

    #[test]
    fn test_rule_version_state_transitions() {
        assert!(RuleVersionState::Current.can_transition_to(RuleVersionState::Superseded));
        assert!(!RuleVersionState::Superseded.can_transition_to(RuleVersionState::Current));
    }

    #[test]
    fn test_member_role_permissions() {
        assert!(MemberRole::Owner.can_write());
        assert!(MemberRole::Admin.can_write());
        assert!(MemberRole::Editor.can_write());
        assert!(!MemberRole::Viewer.can_write());

        assert!(MemberRole::Owner.can_admin());
        assert!(MemberRole::Admin.can_admin());
        assert!(!MemberRole::Editor.can_admin());
        assert!(!MemberRole::Viewer.can_admin());
    }

    #[test]
    fn test_state_roundtrip_serialization() {
        let states = vec![WorkspaceState::Active, WorkspaceState::Archived];
        for s in states {
            let json = serde_json::to_string(&s).unwrap_or_default();
            let back: WorkspaceState =
                serde_json::from_str(&json).unwrap_or(WorkspaceState::Active);
            assert_eq!(s, back);
        }

        let rule_states = vec![
            RuleState::Draft,
            RuleState::Candidate,
            RuleState::Active,
            RuleState::Blocked,
            RuleState::Archived,
        ];
        for s in rule_states {
            let json = serde_json::to_string(&s).unwrap_or_default();
            let back: RuleState = serde_json::from_str(&json).unwrap_or(RuleState::Draft);
            assert_eq!(s, back);
        }
    }

    #[test]
    fn test_state_from_str() {
        assert_eq!(
            WorkspaceState::from_str("active"),
            Some(WorkspaceState::Active)
        );
        assert_eq!(
            WorkspaceState::from_str("archived"),
            Some(WorkspaceState::Archived)
        );
        assert_eq!(WorkspaceState::from_str("invalid"), None);

        assert_eq!(RuleState::from_str("draft"), Some(RuleState::Draft));
        assert_eq!(RuleState::from_str("candidate"), Some(RuleState::Candidate));
        assert_eq!(RuleState::from_str("active"), Some(RuleState::Active));
        assert_eq!(RuleState::from_str("blocked"), Some(RuleState::Blocked));
        assert_eq!(RuleState::from_str("archived"), Some(RuleState::Archived));
        assert_eq!(RuleState::from_str("invalid"), None);
    }

    #[test]
    fn test_sandbox_status_transitions() {
        assert!(SandboxStatus::Running.can_transition_to(SandboxStatus::Closed));
        assert!(!SandboxStatus::Closed.can_transition_to(SandboxStatus::Running));
        assert!(!SandboxStatus::Running.can_transition_to(SandboxStatus::Running));
    }

    #[test]
    fn test_publish_status_transitions() {
        // 正常流程
        assert!(PublishStatus::Pending.can_transition_to(PublishStatus::Approved));
        assert!(PublishStatus::Approved.can_transition_to(PublishStatus::Published));
        // 驳回 / 取消
        assert!(PublishStatus::Pending.can_transition_to(PublishStatus::Rejected));
        assert!(PublishStatus::Pending.can_transition_to(PublishStatus::Cancelled));
        assert!(PublishStatus::Approved.can_transition_to(PublishStatus::Cancelled));
        // 非法迁移
        assert!(!PublishStatus::Published.can_transition_to(PublishStatus::Pending));
        assert!(!PublishStatus::Rejected.can_transition_to(PublishStatus::Approved));
        assert!(!PublishStatus::Pending.can_transition_to(PublishStatus::Published));
    }

    #[test]
    fn test_new_status_roundtrip_serialization() {
        assert_eq!(
            serde_json::to_string(&SandboxStatus::Running).unwrap_or_default(),
            "\"running\""
        );
        assert_eq!(
            serde_json::to_string(&SandboxStatus::Closed).unwrap_or_default(),
            "\"closed\""
        );
        assert_eq!(
            serde_json::to_string(&PublishStatus::Pending).unwrap_or_default(),
            "\"pending\""
        );
        assert_eq!(
            serde_json::to_string(&PublishStatus::Published).unwrap_or_default(),
            "\"published\""
        );
    }

    #[test]
    fn test_new_status_from_str() {
        assert_eq!(
            SandboxStatus::from_str("running"),
            Some(SandboxStatus::Running)
        );
        assert_eq!(
            SandboxStatus::from_str("closed"),
            Some(SandboxStatus::Closed)
        );
        assert_eq!(SandboxStatus::from_str("invalid"), None);

        assert_eq!(
            PublishStatus::from_str("pending"),
            Some(PublishStatus::Pending)
        );
        assert_eq!(
            PublishStatus::from_str("approved"),
            Some(PublishStatus::Approved)
        );
        assert_eq!(
            PublishStatus::from_str("published"),
            Some(PublishStatus::Published)
        );
        assert_eq!(
            PublishStatus::from_str("rejected"),
            Some(PublishStatus::Rejected)
        );
        assert_eq!(
            PublishStatus::from_str("cancelled"),
            Some(PublishStatus::Cancelled)
        );
        assert_eq!(PublishStatus::from_str("invalid"), None);
    }

    #[test]
    fn test_publish_role_permissions() {
        // Doctor: 不可提交/审批/回滚
        assert!(!PublishRole::Doctor.can_submit_publish());
        assert!(!PublishRole::Doctor.can_review_publish());
        assert!(!PublishRole::Doctor.can_rollback());

        // DepartmentHead: 可提交, 不可审批/回滚
        assert!(PublishRole::DepartmentHead.can_submit_publish());
        assert!(!PublishRole::DepartmentHead.can_review_publish());
        assert!(!PublishRole::DepartmentHead.can_rollback());

        // Admin: 可提交 + 审批 + 回滚 (平台管理员需提交+审批双权)
        assert!(PublishRole::Admin.can_submit_publish());
        assert!(PublishRole::Admin.can_review_publish());
        assert!(PublishRole::Admin.can_rollback());
    }

    #[test]
    fn test_publish_role_roundtrip() {
        for role in [
            PublishRole::Doctor,
            PublishRole::DepartmentHead,
            PublishRole::Admin,
        ] {
            let json = serde_json::to_string(&role).unwrap_or_default();
            let back: PublishRole = serde_json::from_str(&json).unwrap_or(PublishRole::Doctor);
            assert_eq!(role, back);
        }
        assert_eq!(PublishRole::from_str("admin"), Some(PublishRole::Admin));
        assert_eq!(PublishRole::from_str("invalid"), None);
    }
}

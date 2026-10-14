// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 权限条目数据模型
//!
//! 定义 [`PermissionEntry`] 及其组成类型，schema 与实施文档 18 对齐。
//! 本模块为纯数据模型（serde 驱动），不包含任何判定逻辑。

use serde::{Deserialize, Serialize};

/// 权限条目生命周期状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PermissionState {
    /// 草稿：创建者编辑中，尚未提交审批
    #[default]
    Draft,
    /// 候选：已提交，等待审批人裁决
    Candidate,
    /// 激活：已通过，进入判定集合
    Active,
    /// 已拒绝｜已作废（从有效判定集合移除）
    Rejected,
}

impl PermissionState {
    /// 是否处于"有效判定"状态（只有 `Active` 参与判定）
    pub const fn is_active(self) -> bool {
        matches!(self, PermissionState::Active)
    }

    /// 是否处于"待审批"状态（`Candidate`）
    pub const fn is_candidate(self) -> bool {
        matches!(self, PermissionState::Candidate)
    }
}

/// 判定效果：允许或拒绝
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    /// 允许
    #[default]
    Allow,
    /// 拒绝
    Deny,
}

/// 主体类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SubjectType {
    /// 具名用户
    User,
    /// 角色
    Role,
    /// 规则文件（author）
    Rule,
    /// LLM 智能体
    LlmAgent,
    /// 任意主体（通配）
    #[default]
    Any,
}

/// 权限主体：谁被允许/拒绝
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Subject {
    /// 主体类型
    #[serde(default)]
    pub subject_type: SubjectType,
    /// 主体标识符（`type = Any` 时为空串）
    #[serde(default)]
    pub id: String,
}

impl Subject {
    /// 通配主体（匹配任意调用者）
    pub fn any() -> Self {
        Self {
            subject_type: SubjectType::Any,
            id: String::new(),
        }
    }

    /// 人类规则作者主体
    pub fn human(name: impl Into<String>) -> Self {
        Self {
            subject_type: SubjectType::User,
            id: name.into(),
        }
    }
}

/// 资源类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResourceType {
    /// 事实（state/queue/shared 路径）
    Fact,
    /// I/O 动作
    IoAction,
    /// HTTP API
    Api,
    /// 共享事实域
    #[default]
    Shared,
}

/// 权限资源：要保护什么
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Resource {
    /// 资源类型
    #[serde(default)]
    pub resource_type: ResourceType,
    /// 资源匹配模式（`*` 结尾为前缀通配），例如 `shared.*` / `db.users.*`
    #[serde(default)]
    pub path: String,
}

/// 作用域：限定生效的租户范围
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Scope {
    /// 生效租户 ID；`None` = 全局生效
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
}

/// 权限条目 —— 一条 subject→resource→action 的授权/拒绝声明
///
/// 数据模型与实施文档 18 对齐。作为纯数据存于 `SharedFactsLog` 下的 TCB JSON，
/// 由 [`super::PermissionTable`] 重建快照后判定。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionEntry {
    /// 条目 ID（即存储路径 `shared.__permission__.entry.<id>` 的 `<id>` 部分）
    pub id: String,
    /// 版本号（读取时以最新全局版本覆盖，用于回放对齐）
    #[serde(default)]
    pub version: u64,
    /// 生命周期状态
    #[serde(default)]
    pub state: PermissionState,
    /// 主体
    #[serde(default)]
    pub subject: Subject,
    /// 资源
    #[serde(default)]
    pub resource: Resource,
    /// 动作标识（对应实际 I/O 指令 type / API 名称）
    #[serde(default = "default_action")]
    pub action: String,
    /// 判定效果
    #[serde(default)]
    pub effect: Effect,
    /// 条件表达式（可选，交给 TCB `evaluate_domain`）：结构见 [`super::condition`]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conditions: Option<serde_json::Value>,
    /// 作用域
    #[serde(default)]
    pub scope: Scope,
    /// 触发来源（审批/审计追根溯源）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
    /// 最近修改者
    #[serde(default = "default_updated_by")]
    pub updated_by: String,
}

/// `action` 字段默认值（`*` = 任意动作）
fn default_action() -> String {
    "*".to_string()
}

/// `updated_by` 字段默认值
fn default_updated_by() -> String {
    "unknown".to_string()
}

impl PermissionEntry {
    /// 构造一个新条目（版本恒为 1，状态恒为 `Draft`，动作通配）
    pub fn new(
        id: impl Into<String>,
        subject: Subject,
        resource: Resource,
        effect: Effect,
    ) -> Self {
        Self {
            id: id.into(),
            version: 1,
            state: PermissionState::Draft,
            subject,
            resource,
            action: default_action(),
            effect,
            conditions: None,
            scope: Scope { tenant_id: None },
            cause: None,
            updated_by: default_updated_by(),
        }
    }

    /// 进入候选（待审批）状态
    pub fn submit(&mut self) -> Result<(), PermissionError> {
        self.state = match self.state {
            PermissionState::Draft => PermissionState::Candidate,
            other => {
                return Err(PermissionError::InvalidState(format!(
                    "only Draft can be submitted, current = {other:?}"
                )))
            }
        };
        Ok(())
    }

    /// 审批裁决：同意则激活，拒绝则作废
    pub fn review(&mut self, approve: bool) -> Result<(), PermissionError> {
        match self.state {
            PermissionState::Candidate => {
                self.state = if approve {
                    PermissionState::Active
                } else {
                    PermissionState::Rejected
                };
                Ok(())
            }
            other => Err(PermissionError::InvalidState(format!(
                "only Candidate can be reviewed, current = {other:?}"
            ))),
        }
    }
}

/// 权限子系统错误
#[derive(Debug, thiserror::Error)]
pub enum PermissionError {
    /// 条目不存在
    #[error("permission entry not found: {0}")]
    NotFound(String),
    /// 状态非法
    #[error("invalid permission state: {0}")]
    InvalidState(String),
    /// 条件评估失败
    #[error("condition evaluation failed: {0}")]
    Condition(String),
    /// 序列化/反序列化失败
    #[error("permission serialization error: {0}")]
    Json(#[from] serde_json::Error),
    /// 存储层错误
    #[error("permission store error: {0}")]
    Store(String),
}

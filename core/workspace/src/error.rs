// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 错误类型 + axum IntoResponse 实现
//!
//! 设计依据: WORKSPACE_CRATE_DESIGN.md §1
//!
//! # 状态码映射
//! | 变体 | HTTP Status | 语义 |
//! |------|-------------|------|
//! | `NotFound` | 404 | 资源不存在 |
//! | `AlreadyExists` | 409 | 资源已存在 (冲突) |
//! | `InvalidStateTransition` | 409 | 状态机非法迁移 |
//! | `InvalidInput` | 400 | 请求参数错误 |
//! | `Unauthorized` | 401 | 未授权 |
//! | `DatabaseError` | 500 | SQLite 错误 |
//! | `Internal` | 500 | 其他内部错误 |

use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde_json::json;
use thiserror::Error;

/// Workspace crate 统一错误类型
///
/// 注意: `NotFound` 采用 `{ resource, id }` 结构体变体,
/// 与 SANDBOX_ORCHESTRATION_DESIGN.md / PUBLISH_QUEUE_DESIGN.md 一致。
#[derive(Debug, Error)]
pub enum WorkspaceError {
    /// 资源不存在 (404)
    #[error("{resource} not found: {id}")]
    NotFound { resource: String, id: String },

    /// 资源已存在 (409)
    #[error("{resource} already exists: {id}")]
    AlreadyExists { resource: String, id: String },

    /// 状态机非法迁移 (409)
    #[error("invalid state transition: {from} -> {to}")]
    InvalidStateTransition { from: String, to: String },

    /// 请求参数错误 (400)
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// 未授权 (401)
    #[error("unauthorized")]
    Unauthorized,

    /// 禁止访问 (403) — 权限不足
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// SQLite 数据库错误 (500)
    #[error("database error: {0}")]
    DatabaseError(String),

    /// 其他内部错误 (500)
    #[error("internal error: {0}")]
    Internal(String),
}

impl WorkspaceError {
    /// 便捷构造: NotFound(resource, id)
    pub fn not_found(resource: impl Into<String>, id: impl Into<String>) -> Self {
        Self::NotFound {
            resource: resource.into(),
            id: id.into(),
        }
    }

    /// 便捷构造: AlreadyExists(resource, id)
    pub fn already_exists(resource: impl Into<String>, id: impl Into<String>) -> Self {
        Self::AlreadyExists {
            resource: resource.into(),
            id: id.into(),
        }
    }

    /// 便捷构造: InvalidInput
    pub fn invalid_input(msg: impl Into<String>) -> Self {
        Self::InvalidInput(msg.into())
    }

    /// 便捷构造: Internal
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }

    /// 便捷构造: Forbidden
    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self::Forbidden(msg.into())
    }
}

/// 从 rusqlite::Error 转换
///
/// - `SQLITE_CONSTRAINT_UNIQUE` → `AlreadyExists`
/// - `SQLITE_MISUSE` / 解析错误 → `InvalidInput`
/// - 其他 → `DatabaseError`
impl From<rusqlite::Error> for WorkspaceError {
    fn from(e: rusqlite::Error) -> Self {
        use rusqlite::ErrorCode;
        match e {
            rusqlite::Error::SqliteFailure(err, ref msg) => match err.code {
                ErrorCode::ConstraintViolation => {
                    // UNIQUE 约束违反 → 资源已存在
                    let detail = msg.clone().unwrap_or_default();
                    WorkspaceError::AlreadyExists {
                        resource: "resource".to_string(),
                        id: detail,
                    }
                }
                _ => WorkspaceError::DatabaseError(e.to_string()),
            },
            rusqlite::Error::QueryReturnedNoRows => WorkspaceError::NotFound {
                resource: "resource".to_string(),
                id: "unknown".to_string(),
            },
            _ => WorkspaceError::DatabaseError(e.to_string()),
        }
    }
}

/// 从 serde_json::Error 转换 (序列化/反序列化失败)
impl From<serde_json::Error> for WorkspaceError {
    fn from(e: serde_json::Error) -> Self {
        WorkspaceError::InvalidInput(format!("JSON error: {e}"))
    }
}

/// 从 std::io::Error 转换 (DB 文件操作)
impl From<std::io::Error> for WorkspaceError {
    fn from(e: std::io::Error) -> Self {
        WorkspaceError::Internal(format!("IO error: {e}"))
    }
}

impl IntoResponse for WorkspaceError {
    fn into_response(self) -> Response {
        let (status, error_msg) = match &self {
            WorkspaceError::NotFound { resource, id } => {
                (StatusCode::NOT_FOUND, format!("{resource} not found: {id}"))
            }
            WorkspaceError::AlreadyExists { resource, id } => (
                StatusCode::CONFLICT,
                format!("{resource} already exists: {id}"),
            ),
            WorkspaceError::InvalidStateTransition { from, to } => (
                StatusCode::CONFLICT,
                format!("invalid state transition: {from} -> {to}"),
            ),
            WorkspaceError::InvalidInput(msg) => {
                (StatusCode::BAD_REQUEST, format!("invalid input: {msg}"))
            }
            WorkspaceError::Unauthorized => {
                (StatusCode::UNAUTHORIZED, "unauthorized".to_string())
            }
            WorkspaceError::Forbidden(msg) => {
                (StatusCode::FORBIDDEN, format!("forbidden: {msg}"))
            }
            WorkspaceError::DatabaseError(msg) => {
                tracing::error!(error = %msg, "workspace database error");
                (StatusCode::INTERNAL_SERVER_ERROR, "database error".to_string())
            }
            WorkspaceError::Internal(msg) => {
                tracing::error!(error = %msg, "workspace internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_string(),
                )
            }
        };

        // 数据库/内部错误不向客户端泄露细节,只返回通用消息
        let body = Json(json!({
            "error": error_msg,
            "code": status.as_u16(),
        }));
        (status, body).into_response()
    }
}

/// Result 别名
pub type WorkspaceResult<T> = Result<T, WorkspaceError>;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn test_not_found_construction() {
        let e = WorkspaceError::not_found("workspace", "ws_123");
        assert!(matches!(
            e,
            WorkspaceError::NotFound { resource, id }
                if resource == "workspace" && id == "ws_123"
        ));
    }

    #[test]
    fn test_from_rusqlite_constraint() {
        let e = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
            Some("UNIQUE constraint failed".to_string()),
        );
        let we: WorkspaceError = e.into();
        assert!(matches!(we, WorkspaceError::AlreadyExists { .. }));
    }

    #[test]
    fn test_from_serde_json_error() {
        let e: serde_json::Error = serde_json::from_str::<serde_json::Value>("bad json").unwrap_err();
        let we: WorkspaceError = e.into();
        assert!(matches!(we, WorkspaceError::InvalidInput(_)));
    }

    #[test]
    fn test_display_messages() {
        let e = WorkspaceError::not_found("rule", "r_1");
        assert!(e.to_string().contains("rule"));
        assert!(e.to_string().contains("r_1"));
    }
}

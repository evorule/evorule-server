// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! SQLite 数据访问层
//!
//! 设计依据: WORKSPACE_CRATE_DESIGN.md §2
//!
//! # 并发模型
//! 使用 `std::sync::Mutex<Connection>` 保护 SQLite 连接。
//! SQLite 单写多读,`Mutex` 串行化写操作,避免 `SQLITE_BUSY`。
//!
//! # Schema 版本
//! 通过 `schema_migrations` 表追踪已应用的迁移版本。
//! 当前版本: 4。

use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};

use crate::error::{WorkspaceError, WorkspaceResult};
use crate::models::{
    BundleImportRecord, MemberRole, ProductionAuditRecord, ProductionStateRecord,
    PublishQueueItem, PublishStatus, RuleRecord, RuleSessionBinding, RuleState, RuleVersionRecord,
    RuleVersionState, SandboxSession, SandboxStatus, SessionBindingState, SessionRecord,
    TestDatasetRecord, VerdictContractRecord, VersionClockMapRecord, WorkspaceMemberRecord,
    WorkspaceRecord, WorkspaceState,
};

/// 当前 schema 版本
const SCHEMA_VERSION: u32 = 4;

/// SQLite 数据库封装
///
/// 通过 `std::sync::Mutex<Connection>` 串行化访问。
/// 在 async 上下文中使用时,锁不应跨 `.await` 持有。
pub struct WorkspaceDb {
    conn: Mutex<Connection>,
}

impl WorkspaceDb {
    /// 打开文件数据库
    ///
    /// - 启用 WAL 模式(提高并发读性能)
    /// - 启用外键约束
    /// - 自动执行 schema 迁移
    pub fn open<P: AsRef<Path>>(path: P) -> WorkspaceResult<Self> {
        let conn = Connection::open(path)
            .map_err(|e| WorkspaceError::DatabaseError(format!("open failed: {e}")))?;
        // WAL 模式
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| WorkspaceError::DatabaseError(format!("pragma journal_mode: {e}")))?;
        // 外键约束
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| WorkspaceError::DatabaseError(format!("pragma foreign_keys: {e}")))?;
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.migrate()?;
        tracing::info!(version = SCHEMA_VERSION, "workspace db migrated");
        Ok(db)
    }

    /// 内存数据库(测试用)
    pub fn in_memory() -> WorkspaceResult<Self> {
        let conn = Connection::open_in_memory()
            .map_err(|e| WorkspaceError::DatabaseError(format!("open_in_memory: {e}")))?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| WorkspaceError::DatabaseError(format!("pragma foreign_keys: {e}")))?;
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.migrate()?;
        Ok(db)
    }

    /// 锁定连接(内部辅助)
    fn lock(&self) -> WorkspaceResult<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|e| WorkspaceError::Internal(format!("db mutex poisoned: {e}")))
    }

    /// 执行 schema 迁移
    fn migrate(&self) -> WorkspaceResult<()> {
        let conn = self.lock()?;

        create_schema_migrations_table(&conn)?;
        let current = current_schema_version(&conn)?;
        if current >= SCHEMA_VERSION {
            return Ok(());
        }

        migrate_v1(&conn)?;
        migrate_v2(&conn)?;
        migrate_v3(&conn)?;
        migrate_v4(&conn)?;
        Ok(())
    }

    /// 获取当前 schema 版本
    pub fn schema_version(&self) -> WorkspaceResult<u32> {
        let conn = self.lock()?;
        let v: Option<i64> = conn
            .query_row(
                "SELECT version FROM schema_migrations ORDER BY version DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| WorkspaceError::DatabaseError(format!("schema_version query: {e}")))?
            .flatten();
        Ok(v.unwrap_or(0) as u32)
    }
}

/// 创建 schema_migrations 元数据表 (必须最先执行)
fn create_schema_migrations_table(conn: &Connection) -> WorkspaceResult<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            applied_at TEXT NOT NULL
        );",
    )
    .map_err(|e| WorkspaceError::DatabaseError(format!("create schema_migrations: {e}")))
}

/// 查询已应用的最高 schema 版本
fn current_schema_version(conn: &Connection) -> WorkspaceResult<u32> {
    let v: Option<u32> = conn
        .query_row(
            "SELECT version FROM schema_migrations ORDER BY version DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| WorkspaceError::DatabaseError(format!("query schema version: {e}")))?
        .flatten();
    Ok(v.unwrap_or(0))
}

/// 记录已应用的迁移版本
fn record_migration(conn: &Connection, version: u32) -> WorkspaceResult<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT OR REPLACE INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
        params![version, now],
    )
    .map_err(|e| WorkspaceError::DatabaseError(format!("record migration v{version}: {e}")))?;
    Ok(())
}

/// v1 迁移: 创建全部基础表
fn migrate_v1(conn: &Connection) -> WorkspaceResult<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS workspaces (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            owner_id TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            archived_at TEXT,
            state TEXT NOT NULL CHECK(state IN ('active', 'archived')),
            description TEXT
        );

        CREATE TABLE IF NOT EXISTS workspace_members (
            workspace_id TEXT NOT NULL REFERENCES workspaces(id),
            user_id TEXT NOT NULL,
            role TEXT NOT NULL CHECK(role IN ('owner', 'admin', 'editor', 'viewer')),
            joined_at TEXT NOT NULL,
            PRIMARY KEY (workspace_id, user_id)
        );

        CREATE TABLE IF NOT EXISTS rules (
            id TEXT PRIMARY KEY,
            workspace_id TEXT NOT NULL REFERENCES workspaces(id),
            name TEXT NOT NULL,
            current_version_id TEXT,
            state TEXT NOT NULL CHECK(state IN ('draft', 'candidate', 'active', 'blocked', 'archived')),
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            archived_at TEXT,
            description TEXT,
            created_by TEXT NOT NULL,
            UNIQUE(workspace_id, name)
        );

        CREATE TABLE IF NOT EXISTS rule_versions (
            id TEXT PRIMARY KEY,
            rule_id TEXT NOT NULL REFERENCES rules(id),
            version INTEGER NOT NULL,
            content_hash TEXT NOT NULL,
            content TEXT NOT NULL,
            created_at TEXT NOT NULL,
            state TEXT NOT NULL CHECK(state IN ('current', 'superseded')),
            created_by TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS sessions (
            id INTEGER PRIMARY KEY,
            workspace_id TEXT NOT NULL REFERENCES workspaces(id),
            rule_id TEXT REFERENCES rules(id),
            rule_version_id TEXT REFERENCES rule_versions(id),
            created_at TEXT NOT NULL,
            closed_at TEXT,
            created_by TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS rule_session_bindings (
            id TEXT PRIMARY KEY,
            rule_version_id TEXT NOT NULL REFERENCES rule_versions(id),
            session_id INTEGER NOT NULL REFERENCES sessions(id),
            workspace_id TEXT NOT NULL REFERENCES workspaces(id),
            bound_at TEXT NOT NULL,
            unbound_at TEXT,
            state TEXT NOT NULL CHECK(state IN ('bound', 'closed'))
        );

        CREATE INDEX IF NOT EXISTS idx_rules_workspace ON rules(workspace_id);
        CREATE INDEX IF NOT EXISTS idx_rule_versions_rule ON rule_versions(rule_id);
        CREATE INDEX IF NOT EXISTS idx_sessions_workspace ON sessions(workspace_id);
        CREATE INDEX IF NOT EXISTS idx_bindings_session ON rule_session_bindings(session_id);
        CREATE INDEX IF NOT EXISTS idx_bindings_rule_version ON rule_session_bindings(rule_version_id);
        CREATE INDEX IF NOT EXISTS idx_members_workspace ON workspace_members(workspace_id);
        ",
    )
    .map_err(|e| WorkspaceError::DatabaseError(format!("migrate v1: {e}")))?;
    record_migration(conn, 1)
}

/// v2 迁移: 沙盒 + 发布队列 + 生产状态/审计 + 测试数据集
/// (SANDBOX_ORCHESTRATION_DESIGN.md §3 + PUBLISH_QUEUE_DESIGN.md §3/§4/§8)
fn migrate_v2(conn: &Connection) -> WorkspaceResult<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS sandbox_sessions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            workspace_id TEXT NOT NULL REFERENCES workspaces(id),
            tcb_session_id INTEGER,
            parent_session_id INTEGER NOT NULL,
            draft_ruleset_hash TEXT,
            test_dataset_id INTEGER NOT NULL,
            status TEXT NOT NULL CHECK(status IN ('running', 'closed')),
            started_at TEXT NOT NULL,
            closed_at TEXT,
            started_by TEXT NOT NULL,
            export_path TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_sandbox_workspace ON sandbox_sessions(workspace_id);
        CREATE INDEX IF NOT EXISTS idx_sandbox_status ON sandbox_sessions(status);

        CREATE TABLE IF NOT EXISTS test_datasets (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            workspace_id TEXT REFERENCES workspaces(id),
            cases_json TEXT NOT NULL,
            case_count INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL,
            created_by TEXT NOT NULL,
            description TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_test_datasets_workspace ON test_datasets(workspace_id);

        CREATE TABLE IF NOT EXISTS publish_queue (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            workspace_id TEXT NOT NULL REFERENCES workspaces(id),
            final_candidate_rules TEXT NOT NULL,
            ruleset_hash TEXT NOT NULL,
            test_report_sandbox_id INTEGER,
            submitted_by TEXT NOT NULL,
            submitted_at TEXT NOT NULL,
            reviewed_by TEXT,
            reviewed_at TEXT,
            review_comment TEXT,
            published_version INTEGER,
            published_at TEXT,
            status TEXT NOT NULL CHECK(status IN ('pending', 'approved', 'published', 'rejected', 'cancelled')),
            description TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_publish_queue_status ON publish_queue(status);
        CREATE INDEX IF NOT EXISTS idx_publish_queue_workspace ON publish_queue(workspace_id);
        CREATE INDEX IF NOT EXISTS idx_publish_queue_submitted_at ON publish_queue(submitted_at);

        CREATE TABLE IF NOT EXISTS production_state (
            id INTEGER PRIMARY KEY CHECK(id = 1),
            current_session_id INTEGER,
            ruleset_version INTEGER NOT NULL DEFAULT 0,
            ruleset_hash TEXT,
            last_operated_by TEXT,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS production_audit (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            event_type TEXT NOT NULL,
            ruleset_version INTEGER NOT NULL,
            previous_version INTEGER,
            ruleset_hash TEXT NOT NULL,
            tcb_session_id INTEGER NOT NULL,
            source_workspace_ids TEXT NOT NULL,
            operated_by TEXT NOT NULL,
            operated_at TEXT NOT NULL,
            reason TEXT,
            test_report_paths TEXT,
            ruleset_snapshot TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_production_audit_version ON production_audit(ruleset_version);
        CREATE INDEX IF NOT EXISTS idx_production_audit_event ON production_audit(event_type);

        -- 初始化 production_state 单行记录 (如果不存在)
        INSERT OR IGNORE INTO production_state (id, current_session_id, ruleset_version, ruleset_hash, last_operated_by, updated_at)
        VALUES (1, NULL, 0, NULL, NULL, '1970-01-01T00:00:00Z');
        ",
    )
    .map_err(|e| WorkspaceError::DatabaseError(format!("migrate v2: {e}")))?;
    record_migration(conn, 2)
}

/// v3 迁移: 判定契约 + wall-clock 旁路映射 + rules.metadata 扩展列
/// (实施文档_界面升级_v1.0.md §四 阶段 A.1)
///
/// 设计约束 (00_架构边界原则.md §七):
///   - verdict_contracts / version_clock_map 属于公共层旁路数据, 绝不进入审计链哈希
///   - version_clock_map 仅版本→时间索引, 不写入 evorule 仓/TCB Fact
fn migrate_v3(conn: &Connection) -> WorkspaceResult<()> {
    conn.execute_batch(
        "
        -- 判定契约表 (workspace 级配置): 条件集合 field/op/value → verdict
        -- 字段对齐 实施文档 A.1 / A.3 端点契约
        CREATE TABLE IF NOT EXISTS verdict_contracts (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            workspace_id TEXT NOT NULL REFERENCES workspaces(id),
            name TEXT NOT NULL,
            version INTEGER NOT NULL,
            -- 条件集合 JSON: [{field, op, value, verdict}]
            rules_json TEXT NOT NULL,
            -- 是否为该 workspace 的默认契约 (每 workspace 至多一条 is_default=1)
            is_default INTEGER NOT NULL DEFAULT 0 CHECK(is_default IN (0, 1)),
            created_by TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            UNIQUE(workspace_id, name, version)
        );

        CREATE INDEX IF NOT EXISTS idx_verdict_contracts_workspace
            ON verdict_contracts(workspace_id);
        CREATE INDEX IF NOT EXISTS idx_verdict_contracts_default
            ON verdict_contracts(workspace_id) WHERE is_default = 1;

        -- wall-clock 旁路映射表: 逻辑版本号 → wall-clock
        -- 绝不写入审计链或参与哈希, 仅应用层查询使用 (00 §六 Fact 无 wall-clock)
        CREATE TABLE IF NOT EXISTS version_clock_map (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id INTEGER NOT NULL REFERENCES sessions(id),
            version INTEGER NOT NULL,
            wall_clock TEXT NOT NULL,
            source TEXT NOT NULL DEFAULT 'ttd_sidecar',
            UNIQUE(session_id, version)
        );

        CREATE INDEX IF NOT EXISTS idx_version_clock_session
            ON version_clock_map(session_id);
        CREATE INDEX IF NOT EXISTS idx_version_clock_version
            ON version_clock_map(session_id, version);

        -- rules 表新增 metadata JSON 扩展列 (用于双模式编辑器、来源、标签等可扩展元数据)
        -- 对既有历史记录使用默认值 '{}', 不破坏现有结构
        ALTER TABLE rules ADD COLUMN metadata TEXT NOT NULL DEFAULT '{}';
        ",
    )
    .map_err(|e| WorkspaceError::DatabaseError(format!("migrate v3: {e}")))?;
    record_migration(conn, 3)
}

/// v4 迁移: bundle 导入溯源 (bundle_imports 表, T5)
///
/// 设计约束 (00_架构边界原则.md §七): bundle_id/source_version 为逻辑标识可入溯源元数据;
/// imported_at 为管理元数据 (墙钟旁路), 绝不渗入 fact / 内容哈希 / 审计验证链。
fn migrate_v4(conn: &Connection) -> WorkspaceResult<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS bundle_imports (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            bundle_id TEXT NOT NULL,
            dataset_id TEXT NOT NULL,
            source_version TEXT NOT NULL,
            selection_mode TEXT NOT NULL,
            resolved_version TEXT,
            content_hash TEXT NOT NULL,
            entry_count INTEGER NOT NULL,
            imported_at TEXT NOT NULL,
            imported_by TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_bundle_imports_dataset ON bundle_imports(dataset_id);
        CREATE INDEX IF NOT EXISTS idx_bundle_imports_bundle ON bundle_imports(bundle_id);
        CREATE INDEX IF NOT EXISTS idx_bundle_imports_imported_at ON bundle_imports(imported_at);
        ",
    )
    .map_err(|e| WorkspaceError::DatabaseError(format!("migrate v4: {e}")))?;
    record_migration(conn, 4)
}

// =============================================================================
// workspaces 表 CRUD
// =============================================================================

impl WorkspaceDb {
    pub fn insert_workspace(&self, ws: &WorkspaceRecord) -> WorkspaceResult<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO workspaces
                (id, name, owner_id, created_at, updated_at, archived_at, state, description)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                ws.id,
                ws.name,
                ws.owner_id,
                ws.created_at.to_rfc3339(),
                ws.updated_at.to_rfc3339(),
                ws.archived_at.map(|t| t.to_rfc3339()),
                ws.state.as_str(),
                ws.description,
            ],
        )
        .map_err(WorkspaceError::from)?;
        Ok(())
    }

    pub fn get_workspace(&self, id: &str) -> WorkspaceResult<WorkspaceRecord> {
        let conn = self.lock()?;
        let ws = conn
            .query_row(
                "SELECT id, name, owner_id, created_at, updated_at, archived_at, state, description
                 FROM workspaces WHERE id = ?1",
                params![id],
                row_to_workspace,
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => WorkspaceError::not_found("workspace", id),
                other => WorkspaceError::from(other),
            })?;
        Ok(ws)
    }

    pub fn list_workspaces(&self, owner_id: Option<&str>) -> WorkspaceResult<Vec<WorkspaceRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, name, owner_id, created_at, updated_at, archived_at, state, description
                 FROM workspaces
                 WHERE (?1 IS NULL OR owner_id = ?1)
                 ORDER BY created_at DESC",
            )
            .map_err(WorkspaceError::from)?;
        let rows = stmt
            .query_map(params![owner_id], row_to_workspace)
            .map_err(WorkspaceError::from)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(WorkspaceError::from)?);
        }
        Ok(out)
    }

    pub fn update_workspace(
        &self,
        id: &str,
        name: Option<&str>,
        description: Option<&str>,
    ) -> WorkspaceResult<WorkspaceRecord> {
        {
            let conn = self.lock()?;
            let now = Utc::now().to_rfc3339();
            // 动态构建 UPDATE (仅更新非 None 字段)
            if name.is_some() || description.is_some() {
                let mut sets: Vec<&str> = Vec::new();
                let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
                if let Some(n) = name {
                    sets.push("name = ?");
                    params_vec.push(Box::new(n.to_string()));
                }
                if let Some(d) = description {
                    sets.push("description = ?");
                    params_vec.push(Box::new(d.to_string()));
                }
                sets.push("updated_at = ?");
                params_vec.push(Box::new(now.clone()));
                let sql = format!("UPDATE workspaces SET {} WHERE id = ?", sets.join(", "));
                params_vec.push(Box::new(id.to_string()));
                let param_refs: Vec<&dyn rusqlite::ToSql> =
                    params_vec.iter().map(|p| p.as_ref()).collect();
                let affected = conn
                    .execute(&sql, param_refs.as_slice())
                    .map_err(WorkspaceError::from)?;
                if affected == 0 {
                    return Err(WorkspaceError::not_found("workspace", id));
                }
            }
        }
        self.get_workspace(id)
    }

    pub fn archive_workspace(&self, id: &str) -> WorkspaceResult<WorkspaceRecord> {
        {
            let conn = self.lock()?;
            let now = Utc::now().to_rfc3339();
            let affected = conn
                .execute(
                    "UPDATE workspaces
                     SET state = 'archived', archived_at = ?1, updated_at = ?1
                     WHERE id = ?2 AND state = 'active'",
                    params![now, id],
                )
                .map_err(WorkspaceError::from)?;
            if affected == 0 {
                // 检查 workspace 是否存在
                let exists: bool = conn
                    .query_row(
                        "SELECT 1 FROM workspaces WHERE id = ?1",
                        params![id],
                        |_| Ok(true),
                    )
                    .optional()
                    .map_err(WorkspaceError::from)?
                    .unwrap_or(false);
                if !exists {
                    return Err(WorkspaceError::not_found("workspace", id));
                }
                return Err(WorkspaceError::InvalidStateTransition {
                    from: "archived".to_string(),
                    to: "archived".to_string(),
                });
            }
        }
        self.get_workspace(id)
    }

    #[allow(dead_code)]
    pub fn delete_workspace(&self, id: &str) -> WorkspaceResult<()> {
        let conn = self.lock()?;
        let affected = conn
            .execute("DELETE FROM workspaces WHERE id = ?1", params![id])
            .map_err(WorkspaceError::from)?;
        if affected == 0 {
            return Err(WorkspaceError::not_found("workspace", id));
        }
        Ok(())
    }
}

// =============================================================================
// workspace_members 表 CRUD
// =============================================================================

impl WorkspaceDb {
    pub fn insert_member(
        &self,
        workspace_id: &str,
        user_id: &str,
        role: &str,
    ) -> WorkspaceResult<WorkspaceMemberRecord> {
        // 校验 role 合法性
        if MemberRole::from_str(role).is_none() {
            return Err(WorkspaceError::invalid_input(format!(
                "invalid role: {role}"
            )));
        }
        let joined_at = Utc::now();
        {
            let conn = self.lock()?;
            conn.execute(
                "INSERT INTO workspace_members (workspace_id, user_id, role, joined_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![workspace_id, user_id, role, joined_at.to_rfc3339()],
            )
            .map_err(WorkspaceError::from)?;
        }
        Ok(WorkspaceMemberRecord {
            workspace_id: workspace_id.to_string(),
            user_id: user_id.to_string(),
            role: role.to_string(),
            joined_at,
        })
    }

    pub fn get_member(
        &self,
        workspace_id: &str,
        user_id: &str,
    ) -> WorkspaceResult<WorkspaceMemberRecord> {
        let conn = self.lock()?;
        let m = conn
            .query_row(
                "SELECT workspace_id, user_id, role, joined_at
                 FROM workspace_members
                 WHERE workspace_id = ?1 AND user_id = ?2",
                params![workspace_id, user_id],
                |row| {
                    Ok(WorkspaceMemberRecord {
                        workspace_id: row.get(0)?,
                        user_id: row.get(1)?,
                        role: row.get(2)?,
                        joined_at: parse_dt(row.get(3)?),
                    })
                },
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    WorkspaceError::not_found("member", format!("{workspace_id}/{user_id}"))
                }
                other => WorkspaceError::from(other),
            })?;
        Ok(m)
    }

    pub fn list_members(&self, workspace_id: &str) -> WorkspaceResult<Vec<WorkspaceMemberRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT workspace_id, user_id, role, joined_at
                 FROM workspace_members
                 WHERE workspace_id = ?1
                 ORDER BY joined_at ASC",
            )
            .map_err(WorkspaceError::from)?;
        let rows = stmt
            .query_map(params![workspace_id], |row| {
                Ok(WorkspaceMemberRecord {
                    workspace_id: row.get(0)?,
                    user_id: row.get(1)?,
                    role: row.get(2)?,
                    joined_at: parse_dt(row.get(3)?),
                })
            })
            .map_err(WorkspaceError::from)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(WorkspaceError::from)?);
        }
        Ok(out)
    }

    pub fn delete_member(&self, workspace_id: &str, user_id: &str) -> WorkspaceResult<()> {
        // 注意: std::sync::Mutex 不可重入,锁必须在调用 get_member 前释放,
        // 否则 get_member 内部 self.lock() 会死锁。
        let affected = {
            let conn = self.lock()?;
            conn.execute(
                "DELETE FROM workspace_members
                 WHERE workspace_id = ?1 AND user_id = ?2 AND role != 'owner'",
                params![workspace_id, user_id],
            )
            .map_err(WorkspaceError::from)?
        };
        if affected == 0 {
            // 锁已释放,安全调用 get_member 检查不存在 vs owner
            let member = self.get_member(workspace_id, user_id)?;
            if member.role == "owner" {
                return Err(WorkspaceError::invalid_input(
                    "cannot remove owner from workspace",
                ));
            }
            return Err(WorkspaceError::not_found(
                "member",
                format!("{workspace_id}/{user_id}"),
            ));
        }
        Ok(())
    }
}

// =============================================================================
// rules 表 CRUD
// =============================================================================

impl WorkspaceDb {
    pub fn insert_rule(&self, rule: &RuleRecord) -> WorkspaceResult<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO rules
                (id, workspace_id, name, current_version_id, state,
                 created_at, updated_at, archived_at, description, created_by,
                 metadata)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                rule.id,
                rule.workspace_id,
                rule.name,
                rule.current_version_id,
                rule.state.as_str(),
                rule.created_at.to_rfc3339(),
                rule.updated_at.to_rfc3339(),
                rule.archived_at.map(|t| t.to_rfc3339()),
                rule.description,
                rule.created_by,
                rule.metadata,
            ],
        )
        .map_err(WorkspaceError::from)?;
        Ok(())
    }

    pub fn get_rule(&self, id: &str) -> WorkspaceResult<RuleRecord> {
        let conn = self.lock()?;
        let r = conn
            .query_row(
                "SELECT id, workspace_id, name, current_version_id, state,
                        created_at, updated_at, archived_at, description, created_by,
                        metadata
                 FROM rules WHERE id = ?1",
                params![id],
                row_to_rule,
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => WorkspaceError::not_found("rule", id),
                other => WorkspaceError::from(other),
            })?;
        Ok(r)
    }

    pub fn get_rule_by_name(&self, workspace_id: &str, name: &str) -> WorkspaceResult<RuleRecord> {
        let conn = self.lock()?;
        let r = conn
            .query_row(
                "SELECT id, workspace_id, name, current_version_id, state,
                        created_at, updated_at, archived_at, description, created_by,
                        metadata
                 FROM rules WHERE workspace_id = ?1 AND name = ?2",
                params![workspace_id, name],
                row_to_rule,
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    WorkspaceError::not_found("rule", format!("{workspace_id}/{name}"))
                }
                other => WorkspaceError::from(other),
            })?;
        Ok(r)
    }

    pub fn list_rules(&self, workspace_id: &str) -> WorkspaceResult<Vec<RuleRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, workspace_id, name, current_version_id, state,
                        created_at, updated_at, archived_at, description, created_by,
                        metadata
                 FROM rules
                 WHERE workspace_id = ?1
                 ORDER BY created_at DESC",
            )
            .map_err(WorkspaceError::from)?;
        let rows = stmt
            .query_map(params![workspace_id], row_to_rule)
            .map_err(WorkspaceError::from)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(WorkspaceError::from)?);
        }
        Ok(out)
    }

    pub fn update_rule_state(&self, id: &str, new_state: RuleState) -> WorkspaceResult<RuleRecord> {
        {
            let conn = self.lock()?;
            let now = Utc::now().to_rfc3339();
            let affected = conn
                .execute(
                    "UPDATE rules SET state = ?1, updated_at = ?2 WHERE id = ?3",
                    params![new_state.as_str(), now, id],
                )
                .map_err(WorkspaceError::from)?;
            if affected == 0 {
                return Err(WorkspaceError::not_found("rule", id));
            }
        }
        self.get_rule(id)
    }

    pub fn update_rule_current_version(
        &self,
        rule_id: &str,
        version_id: &str,
    ) -> WorkspaceResult<()> {
        let conn = self.lock()?;
        let now = Utc::now().to_rfc3339();
        let affected = conn
            .execute(
                "UPDATE rules SET current_version_id = ?1, updated_at = ?2 WHERE id = ?3",
                params![version_id, now, rule_id],
            )
            .map_err(WorkspaceError::from)?;
        if affected == 0 {
            return Err(WorkspaceError::not_found("rule", rule_id));
        }
        Ok(())
    }

    pub fn archive_rule(&self, id: &str) -> WorkspaceResult<RuleRecord> {
        {
            let conn = self.lock()?;
            let now = Utc::now().to_rfc3339();
            let affected = conn
                .execute(
                    "UPDATE rules
                     SET state = 'archived', archived_at = ?1, updated_at = ?1
                     WHERE id = ?2 AND state != 'archived'",
                    params![now, id],
                )
                .map_err(WorkspaceError::from)?;
            if affected == 0 {
                let exists: bool = conn
                    .query_row("SELECT 1 FROM rules WHERE id = ?1", params![id], |_| {
                        Ok(true)
                    })
                    .optional()
                    .map_err(WorkspaceError::from)?
                    .unwrap_or(false);
                if !exists {
                    return Err(WorkspaceError::not_found("rule", id));
                }
                return Err(WorkspaceError::InvalidStateTransition {
                    from: "archived".to_string(),
                    to: "archived".to_string(),
                });
            }
        }
        self.get_rule(id)
    }
}

// =============================================================================
// rule_versions 表 CRUD
// =============================================================================

impl WorkspaceDb {
    pub fn insert_rule_version(&self, rv: &RuleVersionRecord) -> WorkspaceResult<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO rule_versions
                (id, rule_id, version, content_hash, content, created_at, state, created_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                rv.id,
                rv.rule_id,
                rv.version as i64,
                rv.content_hash,
                rv.content,
                rv.created_at.to_rfc3339(),
                rv.state.as_str(),
                rv.created_by,
            ],
        )
        .map_err(WorkspaceError::from)?;
        Ok(())
    }

    pub fn get_rule_version(&self, id: &str) -> WorkspaceResult<RuleVersionRecord> {
        let conn = self.lock()?;
        let rv = conn
            .query_row(
                "SELECT id, rule_id, version, content_hash, content, created_at, state, created_by
                 FROM rule_versions WHERE id = ?1",
                params![id],
                row_to_rule_version,
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    WorkspaceError::not_found("rule_version", id)
                }
                other => WorkspaceError::from(other),
            })?;
        Ok(rv)
    }

    pub fn list_rule_versions(&self, rule_id: &str) -> WorkspaceResult<Vec<RuleVersionRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, rule_id, version, content_hash, content, created_at, state, created_by
                 FROM rule_versions
                 WHERE rule_id = ?1
                 ORDER BY version DESC",
            )
            .map_err(WorkspaceError::from)?;
        let rows = stmt
            .query_map(params![rule_id], row_to_rule_version)
            .map_err(WorkspaceError::from)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(WorkspaceError::from)?);
        }
        Ok(out)
    }

    pub fn get_current_version(&self, rule_id: &str) -> WorkspaceResult<Option<RuleVersionRecord>> {
        let conn = self.lock()?;
        let rv = conn
            .query_row(
                "SELECT rv.id, rv.rule_id, rv.version, rv.content_hash, rv.content,
                        rv.created_at, rv.state, rv.created_by
                 FROM rule_versions rv
                 JOIN rules r ON r.current_version_id = rv.id
                 WHERE r.id = ?1",
                params![rule_id],
                row_to_rule_version,
            )
            .optional()
            .map_err(WorkspaceError::from)?;
        Ok(rv)
    }

    /// 获取下一个版本号 (max(version) + 1, 无版本时返回 1)
    pub fn get_next_version_number(&self, rule_id: &str) -> WorkspaceResult<u64> {
        let conn = self.lock()?;
        let max: Option<i64> = conn
            .query_row(
                "SELECT MAX(version) FROM rule_versions WHERE rule_id = ?1",
                params![rule_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(WorkspaceError::from)?
            .flatten();
        Ok(max.unwrap_or(0) as u64 + 1)
    }

    pub fn mark_version_superseded(&self, id: &str) -> WorkspaceResult<()> {
        let conn = self.lock()?;
        let affected = conn
            .execute(
                "UPDATE rule_versions SET state = 'superseded' WHERE id = ?1 AND state = 'current'",
                params![id],
            )
            .map_err(WorkspaceError::from)?;
        if affected == 0 {
            return Err(WorkspaceError::not_found("rule_version (current)", id));
        }
        Ok(())
    }
}

// =============================================================================
// sessions 表 CRUD
// =============================================================================

impl WorkspaceDb {
    pub fn insert_session(&self, session: &SessionRecord) -> WorkspaceResult<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO sessions
                (id, workspace_id, rule_id, rule_version_id, created_at, closed_at, created_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session.id as i64,
                session.workspace_id,
                session.rule_id,
                session.rule_version_id,
                session.created_at.to_rfc3339(),
                session.closed_at.map(|t| t.to_rfc3339()),
                session.created_by,
            ],
        )
        .map_err(WorkspaceError::from)?;
        Ok(())
    }

    pub fn get_session(&self, id: u64) -> WorkspaceResult<SessionRecord> {
        let conn = self.lock()?;
        let s = conn
            .query_row(
                "SELECT id, workspace_id, rule_id, rule_version_id,
                        created_at, closed_at, created_by
                 FROM sessions WHERE id = ?1",
                params![id as i64],
                |row| {
                    Ok(SessionRecord {
                        id: row.get::<_, i64>(0)? as u64,
                        workspace_id: row.get(1)?,
                        rule_id: row.get(2)?,
                        rule_version_id: row.get(3)?,
                        created_at: parse_dt(row.get(4)?),
                        closed_at: row.get::<_, Option<String>>(5)?.map(parse_dt),
                        created_by: row.get(6)?,
                    })
                },
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    WorkspaceError::not_found("session", id.to_string())
                }
                other => WorkspaceError::from(other),
            })?;
        Ok(s)
    }

    pub fn list_sessions_by_workspace(
        &self,
        workspace_id: &str,
    ) -> WorkspaceResult<Vec<SessionRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, workspace_id, rule_id, rule_version_id,
                        created_at, closed_at, created_by
                 FROM sessions
                 WHERE workspace_id = ?1
                 ORDER BY created_at DESC",
            )
            .map_err(WorkspaceError::from)?;
        let rows = stmt
            .query_map(params![workspace_id], |row| {
                Ok(SessionRecord {
                    id: row.get::<_, i64>(0)? as u64,
                    workspace_id: row.get(1)?,
                    rule_id: row.get(2)?,
                    rule_version_id: row.get(3)?,
                    created_at: parse_dt(row.get(4)?),
                    closed_at: row.get::<_, Option<String>>(5)?.map(parse_dt),
                    created_by: row.get(6)?,
                })
            })
            .map_err(WorkspaceError::from)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(WorkspaceError::from)?);
        }
        Ok(out)
    }

    pub fn close_session(&self, id: u64) -> WorkspaceResult<SessionRecord> {
        {
            let conn = self.lock()?;
            let now = Utc::now().to_rfc3339();
            let affected = conn
                .execute(
                    "UPDATE sessions SET closed_at = ?1 WHERE id = ?2 AND closed_at IS NULL",
                    params![now, id as i64],
                )
                .map_err(WorkspaceError::from)?;
            if affected == 0 {
                let exists: bool = conn
                    .query_row(
                        "SELECT 1 FROM sessions WHERE id = ?1",
                        params![id as i64],
                        |_| Ok(true),
                    )
                    .optional()
                    .map_err(WorkspaceError::from)?
                    .unwrap_or(false);
                if !exists {
                    return Err(WorkspaceError::not_found("session", id.to_string()));
                }
                return Err(WorkspaceError::InvalidStateTransition {
                    from: "closed".to_string(),
                    to: "closed".to_string(),
                });
            }
        }
        self.get_session(id)
    }
}

// =============================================================================
// rule_session_bindings 表 CRUD
// =============================================================================

impl WorkspaceDb {
    pub fn insert_binding(&self, binding: &RuleSessionBinding) -> WorkspaceResult<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO rule_session_bindings
                (id, rule_version_id, session_id, workspace_id, bound_at, unbound_at, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                binding.id,
                binding.rule_version_id,
                binding.session_id as i64,
                binding.workspace_id,
                binding.bound_at.to_rfc3339(),
                binding.unbound_at.map(|t| t.to_rfc3339()),
                binding.state.as_str(),
            ],
        )
        .map_err(WorkspaceError::from)?;
        Ok(())
    }

    pub fn list_bindings_by_session(
        &self,
        session_id: u64,
    ) -> WorkspaceResult<Vec<RuleSessionBinding>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, rule_version_id, session_id, workspace_id,
                        bound_at, unbound_at, state
                 FROM rule_session_bindings
                 WHERE session_id = ?1
                 ORDER BY bound_at ASC",
            )
            .map_err(WorkspaceError::from)?;
        let rows = stmt
            .query_map(params![session_id as i64], row_to_binding)
            .map_err(WorkspaceError::from)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(WorkspaceError::from)?);
        }
        Ok(out)
    }

    pub fn list_bindings_by_rule_version(
        &self,
        rule_version_id: &str,
    ) -> WorkspaceResult<Vec<RuleSessionBinding>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, rule_version_id, session_id, workspace_id,
                        bound_at, unbound_at, state
                 FROM rule_session_bindings
                 WHERE rule_version_id = ?1
                 ORDER BY bound_at ASC",
            )
            .map_err(WorkspaceError::from)?;
        let rows = stmt
            .query_map(params![rule_version_id], row_to_binding)
            .map_err(WorkspaceError::from)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(WorkspaceError::from)?);
        }
        Ok(out)
    }

    pub fn close_binding(&self, id: &str) -> WorkspaceResult<RuleSessionBinding> {
        // 1. 查询当前 binding 并校验状态 (必须为 Bound 才能关闭)
        let mut binding = {
            let conn = self.lock()?;
            conn.query_row(
                "SELECT id, rule_version_id, session_id, workspace_id,
                        bound_at, unbound_at, state
                 FROM rule_session_bindings WHERE id = ?1",
                params![id],
                row_to_binding,
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => WorkspaceError::not_found("binding", id),
                other => WorkspaceError::from(other),
            })?
        };
        if binding.state != SessionBindingState::Bound {
            return Err(WorkspaceError::InvalidStateTransition {
                from: binding.state.as_str().to_string(),
                to: "closed".to_string(),
            });
        }
        // 2. 更新状态为 closed,设置 unbound_at
        let now = Utc::now().to_rfc3339();
        {
            let conn = self.lock()?;
            conn.execute(
                "UPDATE rule_session_bindings SET state = 'closed', unbound_at = ?1 WHERE id = ?2",
                params![now, id],
            )
            .map_err(WorkspaceError::from)?;
        }
        // 3. 就地更新内存对象并返回 (避免重复查询)
        binding.state = SessionBindingState::Closed;
        binding.unbound_at = Some(parse_dt(now));
        Ok(binding)
    }
}

// =============================================================================
// workspace_members 辅助查询
// =============================================================================

impl WorkspaceDb {
    /// 校验用户是否为 workspace 成员 (含 owner)
    ///
    /// 用于沙盒/发布权限校验: 必须是 workspace 成员才能操作。
    pub fn is_workspace_member(&self, workspace_id: &str, user_id: &str) -> WorkspaceResult<bool> {
        let conn = self.lock()?;
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM workspace_members
                 WHERE workspace_id = ?1 AND user_id = ?2",
                params![workspace_id, user_id],
                |_| Ok(true),
            )
            .optional()
            .map_err(WorkspaceError::from)?
            .unwrap_or(false);
        Ok(exists)
    }
}

// =============================================================================
// rule_versions 批量查询 (沙盒/发布需要)
// =============================================================================

impl WorkspaceDb {
    /// 按 ID 列表批量查询规则版本
    ///
    /// 返回顺序不保证与输入一致 (按数据库返回顺序)。
    /// 用于沙盒加载 Draft 规则 + 发布时校验 final_candidate 规则。
    pub fn get_rule_versions_by_ids(
        &self,
        version_ids: &[String],
    ) -> WorkspaceResult<Vec<RuleVersionRecord>> {
        if version_ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.lock()?;
        // 构建 IN (?, ?, ...) 占位符
        let placeholders: Vec<String> = (0..version_ids.len()).map(|_| "?".to_string()).collect();
        let sql = format!(
            "SELECT id, rule_id, version, content_hash, content, created_at, state, created_by
             FROM rule_versions WHERE id IN ({})",
            placeholders.join(", ")
        );
        let params_vec: Vec<&dyn rusqlite::ToSql> = version_ids
            .iter()
            .map(|s| s as &dyn rusqlite::ToSql)
            .collect();
        let mut stmt = conn.prepare(&sql).map_err(WorkspaceError::from)?;
        let rows = stmt
            .query_map(params_vec.as_slice(), row_to_rule_version)
            .map_err(WorkspaceError::from)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(WorkspaceError::from)?);
        }
        Ok(out)
    }

    /// 按规则 ID 列表批量查询规则记录 (校验规则状态时使用)
    pub fn get_rules_by_ids(&self, rule_ids: &[String]) -> WorkspaceResult<Vec<RuleRecord>> {
        if rule_ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.lock()?;
        let placeholders: Vec<String> = (0..rule_ids.len()).map(|_| "?".to_string()).collect();
        let sql = format!(
            "SELECT id, workspace_id, name, current_version_id, state,
                    created_at, updated_at, archived_at, description, created_by,
                    metadata
             FROM rules WHERE id IN ({})",
            placeholders.join(", ")
        );
        let params_vec: Vec<&dyn rusqlite::ToSql> =
            rule_ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let mut stmt = conn.prepare(&sql).map_err(WorkspaceError::from)?;
        let rows = stmt
            .query_map(params_vec.as_slice(), row_to_rule)
            .map_err(WorkspaceError::from)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(WorkspaceError::from)?);
        }
        Ok(out)
    }

    /// 读取或写入规则的 metadata (JSON 字符串)。
    ///
    /// 这是 A.3 双模式编辑器、B 判定契约等扩展功能所需的列。
    /// 调用方应保证传入的 metadata 是合法 JSON OBJECT 字符串。
    pub fn update_rule_metadata(
        &self,
        rule_id: &str,
        metadata: &str,
    ) -> WorkspaceResult<RuleRecord> {
        {
            let conn = self.lock()?;
            let now = Utc::now().to_rfc3339();
            let affected = conn
                .execute(
                    "UPDATE rules SET metadata = ?1, updated_at = ?2 WHERE id = ?3",
                    params![metadata, now, rule_id],
                )
                .map_err(WorkspaceError::from)?;
            if affected == 0 {
                return Err(WorkspaceError::not_found("rule", rule_id));
            }
        }
        self.get_rule(rule_id)
    }
}

// =============================================================================
// sandbox_sessions 表 CRUD — SANDBOX_ORCHESTRATION_DESIGN.md §3
// =============================================================================

impl WorkspaceDb {
    /// 插入沙盒会话记录
    ///
    /// 返回自增 id。
    pub fn insert_sandbox_session(
        &self,
        tcb_session_id: Option<i64>,
        workspace_id: &str,
        parent_session_id: i64,
        draft_ruleset_hash: Option<&str>,
        test_dataset_id: i64,
        started_by: &str,
    ) -> WorkspaceResult<i64> {
        let conn = self.lock()?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO sandbox_sessions
                (workspace_id, tcb_session_id, parent_session_id, draft_ruleset_hash,
                 test_dataset_id, status, started_at, started_by)
             VALUES (?1, ?2, ?3, ?4, ?5, 'running', ?6, ?7)",
            params![
                workspace_id,
                tcb_session_id,
                parent_session_id,
                draft_ruleset_hash,
                test_dataset_id,
                now,
                started_by,
            ],
        )
        .map_err(WorkspaceError::from)?;
        Ok(conn.last_insert_rowid())
    }

    /// 获取沙盒会话
    pub fn get_sandbox_session(&self, id: i64) -> WorkspaceResult<Option<SandboxSession>> {
        let conn = self.lock()?;
        let s = conn
            .query_row(
                "SELECT id, workspace_id, tcb_session_id, parent_session_id, draft_ruleset_hash,
                        test_dataset_id, status, started_at, closed_at, started_by, export_path
                 FROM sandbox_sessions WHERE id = ?1",
                params![id],
                row_to_sandbox,
            )
            .optional()
            .map_err(WorkspaceError::from)?;
        Ok(s)
    }

    /// 列出 workspace 的沙盒会话 (按启动时间降序)
    pub fn list_sandbox_sessions(
        &self,
        workspace_id: &str,
    ) -> WorkspaceResult<Vec<SandboxSession>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, workspace_id, tcb_session_id, parent_session_id, draft_ruleset_hash,
                        test_dataset_id, status, started_at, closed_at, started_by, export_path
                 FROM sandbox_sessions
                 WHERE workspace_id = ?1
                 ORDER BY started_at DESC",
            )
            .map_err(WorkspaceError::from)?;
        let rows = stmt
            .query_map(params![workspace_id], row_to_sandbox)
            .map_err(WorkspaceError::from)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(WorkspaceError::from)?);
        }
        Ok(out)
    }

    /// 关闭沙盒会话 (status → closed, 填充 closed_at + export_path)
    pub fn close_sandbox_session(&self, id: i64, export_path: &str) -> WorkspaceResult<()> {
        let conn = self.lock()?;
        let now = Utc::now().to_rfc3339();
        let affected = conn
            .execute(
                "UPDATE sandbox_sessions
                 SET status = 'closed', closed_at = ?1, export_path = ?2
                 WHERE id = ?3 AND status = 'running'",
                params![now, export_path, id],
            )
            .map_err(WorkspaceError::from)?;
        if affected == 0 {
            return Err(WorkspaceError::not_found(
                "sandbox (running)",
                id.to_string(),
            ));
        }
        Ok(())
    }
}

// =============================================================================
// test_datasets 表 CRUD — SANDBOX_ORCHESTRATION_DESIGN.md §3
// =============================================================================

impl WorkspaceDb {
    /// 插入测试数据集
    ///
    /// `cases_json` 应为 JSON 数组字符串。case_count 由调用方计算并传入。
    pub fn insert_test_dataset(
        &self,
        name: &str,
        workspace_id: Option<&str>,
        cases_json: &str,
        case_count: i64,
        created_by: &str,
        description: Option<&str>,
    ) -> WorkspaceResult<i64> {
        let conn = self.lock()?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO test_datasets
                (name, workspace_id, cases_json, case_count, created_at, created_by, description)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                name,
                workspace_id,
                cases_json,
                case_count,
                now,
                created_by,
                description
            ],
        )
        .map_err(WorkspaceError::from)?;
        Ok(conn.last_insert_rowid())
    }

    /// 获取测试数据集
    pub fn get_test_dataset(&self, id: i64) -> WorkspaceResult<Option<TestDatasetRecord>> {
        let conn = self.lock()?;
        let d = conn
            .query_row(
                "SELECT id, name, workspace_id, cases_json, case_count,
                        created_at, created_by, description
                 FROM test_datasets WHERE id = ?1",
                params![id],
                row_to_test_dataset,
            )
            .optional()
            .map_err(WorkspaceError::from)?;
        Ok(d)
    }

    /// 列出 workspace 的测试数据集 (含共享数据集 workspace_id IS NULL)
    pub fn list_test_datasets(
        &self,
        workspace_id: &str,
    ) -> WorkspaceResult<Vec<TestDatasetRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, name, workspace_id, cases_json, case_count,
                        created_at, created_by, description
                 FROM test_datasets
                 WHERE workspace_id = ?1 OR workspace_id IS NULL
                 ORDER BY created_at DESC",
            )
            .map_err(WorkspaceError::from)?;
        let rows = stmt
            .query_map(params![workspace_id], row_to_test_dataset)
            .map_err(WorkspaceError::from)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(WorkspaceError::from)?);
        }
        Ok(out)
    }
}

// =============================================================================
// production_state 表 CRUD — PUBLISH_QUEUE_DESIGN.md §4
// =============================================================================

impl WorkspaceDb {
    /// 获取生产状态 (单行表, id=1)
    pub fn get_production_state(&self) -> WorkspaceResult<ProductionStateRecord> {
        let conn = self.lock()?;
        let state = conn
            .query_row(
                "SELECT id, current_session_id, ruleset_version, ruleset_hash,
                        last_operated_by, updated_at
                 FROM production_state WHERE id = 1",
                [],
                row_to_production_state,
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    WorkspaceError::internal("production_state row not initialized")
                }
                other => WorkspaceError::from(other),
            })?;
        Ok(state)
    }

    /// 原子更新生产状态 (current_session_id + ruleset_version + ruleset_hash)
    ///
    /// 版本号由调用方计算 (current.ruleset_version + 1),保证单调递增。
    pub fn update_production_state(
        &self,
        new_session_id: i64,
        new_ruleset_version: i64,
        new_ruleset_hash: &str,
        operated_by: &str,
    ) -> WorkspaceResult<()> {
        let conn = self.lock()?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE production_state
             SET current_session_id = ?1,
                 ruleset_version = ?2,
                 ruleset_hash = ?3,
                 last_operated_by = ?4,
                 updated_at = ?5
             WHERE id = 1",
            params![
                new_session_id,
                new_ruleset_version,
                new_ruleset_hash,
                operated_by,
                now
            ],
        )
        .map_err(WorkspaceError::from)?;
        Ok(())
    }
}

// =============================================================================
// production_audit 表 CRUD — PUBLISH_QUEUE_DESIGN.md §8
// =============================================================================

impl WorkspaceDb {
    /// 插入生产审计记录
    ///
    /// `ruleset_snapshot` 为发布时的规则集 JSON 数组字符串 (用于回滚)。
    #[allow(clippy::too_many_arguments)]
    pub fn insert_production_audit(
        &self,
        event_type: &str,
        ruleset_version: i64,
        previous_version: Option<i64>,
        ruleset_hash: &str,
        tcb_session_id: i64,
        source_workspace_ids: &str,
        operated_by: &str,
        reason: Option<&str>,
        test_report_paths: Option<&str>,
        ruleset_snapshot: Option<&str>,
    ) -> WorkspaceResult<()> {
        let conn = self.lock()?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO production_audit
                (event_type, ruleset_version, previous_version, ruleset_hash,
                 tcb_session_id, source_workspace_ids, operated_by, operated_at,
                 reason, test_report_paths, ruleset_snapshot)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                event_type,
                ruleset_version,
                previous_version,
                ruleset_hash,
                tcb_session_id,
                source_workspace_ids,
                operated_by,
                now,
                reason,
                test_report_paths,
                ruleset_snapshot,
            ],
        )
        .map_err(WorkspaceError::from)?;
        Ok(())
    }

    /// 按版本号查询生产审计记录 (用于回滚时加载旧规则集快照)
    ///
    /// 仅返回版本变更事件 (ruleset_published / ruleset_rollback),
    /// 过滤掉生命周期事件 (publish_submitted / sandbox_started 等),
    /// 避免非版本变更记录干扰回滚快照加载。
    pub fn get_production_audit_by_version(
        &self,
        version: i64,
    ) -> WorkspaceResult<Option<ProductionAuditRecord>> {
        let conn = self.lock()?;
        let r = conn
            .query_row(
                "SELECT id, event_type, ruleset_version, previous_version, ruleset_hash,
                        tcb_session_id, source_workspace_ids, operated_by, operated_at,
                        reason, test_report_paths, ruleset_snapshot
                 FROM production_audit
                 WHERE ruleset_version = ?1
                   AND event_type IN ('ruleset_published', 'ruleset_rollback')
                 ORDER BY id DESC LIMIT 1",
                params![version],
                row_to_production_audit,
            )
            .optional()
            .map_err(WorkspaceError::from)?;
        Ok(r)
    }

    /// 列出生产审计记录 (按版本号降序, 限制条数)
    pub fn list_production_audit(&self, limit: i64) -> WorkspaceResult<Vec<ProductionAuditRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, event_type, ruleset_version, previous_version, ruleset_hash,
                        tcb_session_id, source_workspace_ids, operated_by, operated_at,
                        reason, test_report_paths, ruleset_snapshot
                 FROM production_audit
                 ORDER BY ruleset_version DESC, id DESC
                 LIMIT ?1",
            )
            .map_err(WorkspaceError::from)?;
        let rows = stmt
            .query_map(params![limit], row_to_production_audit)
            .map_err(WorkspaceError::from)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(WorkspaceError::from)?);
        }
        Ok(out)
    }
}

// =============================================================================
// bundle_imports 表 CRUD — T5 审计溯源
// =============================================================================

impl WorkspaceDb {
    /// 记录一次 bundle 导入溯源 (T5)
    ///
    /// 返回自增 id。`imported_at` 由本方法以墙钟生成 (管理元数据, 旁路),
    /// 不参与 fact / 内容哈希 / 审计验证链。
    #[allow(clippy::too_many_arguments)]
    pub fn insert_bundle_import(
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
        let conn = self.lock()?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO bundle_imports
                (bundle_id, dataset_id, source_version, selection_mode, resolved_version,
                 content_hash, entry_count, imported_at, imported_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                bundle_id,
                dataset_id,
                source_version,
                selection_mode,
                resolved_version,
                content_hash,
                entry_count,
                now,
                imported_by,
            ],
        )
        .map_err(WorkspaceError::from)?;
        Ok(conn.last_insert_rowid())
    }

    /// 列出 bundle 导入溯源记录 (按导入时间倒序, 限制条数)
    pub fn list_bundle_imports(&self, limit: i64) -> WorkspaceResult<Vec<BundleImportRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, bundle_id, dataset_id, source_version, selection_mode,
                        resolved_version, content_hash, entry_count, imported_at, imported_by
                 FROM bundle_imports
                 ORDER BY imported_at DESC, id DESC
                 LIMIT ?1",
            )
            .map_err(WorkspaceError::from)?;
        let rows = stmt
            .query_map(params![limit], row_to_bundle_import)
            .map_err(WorkspaceError::from)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(WorkspaceError::from)?);
        }
        Ok(out)
    }
}

/// bundle_imports 行映射
fn row_to_bundle_import(row: &rusqlite::Row<'_>) -> rusqlite::Result<BundleImportRecord> {
    Ok(BundleImportRecord {
        id: row.get(0)?,
        bundle_id: row.get(1)?,
        dataset_id: row.get(2)?,
        source_version: row.get(3)?,
        selection_mode: row.get(4)?,
        resolved_version: row.get(5)?,
        content_hash: row.get(6)?,
        entry_count: row.get(7)?,
        imported_at: parse_dt(row.get(8)?),
        imported_by: row.get(9)?,
    })
}

impl WorkspaceDb {
    /// 插入发布队列项 (status=pending)
    ///
    /// 返回自增 id。
    #[allow(clippy::too_many_arguments)]
    pub fn insert_publish_queue_item(
        &self,
        workspace_id: &str,
        final_candidate_rules: &str,
        ruleset_hash: &str,
        test_report_sandbox_id: Option<i64>,
        submitted_by: &str,
        description: Option<&str>,
    ) -> WorkspaceResult<i64> {
        let conn = self.lock()?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO publish_queue
                (workspace_id, final_candidate_rules, ruleset_hash, test_report_sandbox_id,
                 submitted_by, submitted_at, status, description)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7)",
            params![
                workspace_id,
                final_candidate_rules,
                ruleset_hash,
                test_report_sandbox_id,
                submitted_by,
                now,
                description,
            ],
        )
        .map_err(WorkspaceError::from)?;
        Ok(conn.last_insert_rowid())
    }

    /// 获取发布队列项
    pub fn get_publish_queue_item(&self, id: i64) -> WorkspaceResult<Option<PublishQueueItem>> {
        let conn = self.lock()?;
        let item = conn
            .query_row(
                "SELECT id, workspace_id, final_candidate_rules, ruleset_hash,
                        test_report_sandbox_id, submitted_by, submitted_at,
                        reviewed_by, reviewed_at, review_comment,
                        published_version, published_at, status, description
                 FROM publish_queue WHERE id = ?1",
                params![id],
                row_to_publish_queue,
            )
            .optional()
            .map_err(WorkspaceError::from)?;
        Ok(item)
    }

    /// 列出发布队列 (按状态过滤, 按提交时间升序 FIFO)
    pub fn list_publish_queue(
        &self,
        status_filter: Option<PublishStatus>,
    ) -> WorkspaceResult<Vec<PublishQueueItem>> {
        let conn = self.lock()?;
        let status_str = status_filter.map(|s| s.as_str());
        let mut stmt = conn
            .prepare(
                "SELECT id, workspace_id, final_candidate_rules, ruleset_hash,
                        test_report_sandbox_id, submitted_by, submitted_at,
                        reviewed_by, reviewed_at, review_comment,
                        published_version, published_at, status, description
                 FROM publish_queue
                 WHERE (?1 IS NULL OR status = ?1)
                 ORDER BY submitted_at ASC",
            )
            .map_err(WorkspaceError::from)?;
        let rows = stmt
            .query_map(params![status_str], row_to_publish_queue)
            .map_err(WorkspaceError::from)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(WorkspaceError::from)?);
        }
        Ok(out)
    }

    /// 更新队列项审批状态 (approved/rejected/cancelled)
    pub fn update_publish_queue_status(
        &self,
        id: i64,
        new_status: PublishStatus,
        reviewed_by: &str,
        comment: Option<&str>,
    ) -> WorkspaceResult<()> {
        let conn = self.lock()?;
        let now = Utc::now().to_rfc3339();
        let affected = conn
            .execute(
                "UPDATE publish_queue
                 SET status = ?1, reviewed_by = ?2, reviewed_at = ?3, review_comment = ?4
                 WHERE id = ?5",
                params![new_status.as_str(), reviewed_by, now, comment, id],
            )
            .map_err(WorkspaceError::from)?;
        if affected == 0 {
            return Err(WorkspaceError::not_found("publish_queue", id.to_string()));
        }
        Ok(())
    }

    /// 标记队列项已发布 (status=pending → published, 填充版本号/审批人/审批时间/审批意见)
    ///
    /// 前置缺陷修复: 原实现先置 status=approved 再执行滚动发布, 发布失败会残留孤儿 approved
    /// 状态 (无法重试、无法恢复)。改为仅在发布成功后才把 pending 直接置为 published,
    /// 失败时队列保持 pending 可重试。审批人/意见随发布成功一并落库。
    pub fn complete_publish(
        &self,
        id: i64,
        published_version: i64,
        reviewed_by: &str,
        review_comment: Option<&str>,
    ) -> WorkspaceResult<()> {
        let conn = self.lock()?;
        let now = Utc::now().to_rfc3339();
        let affected = conn
            .execute(
                "UPDATE publish_queue
                 SET status = 'published', published_version = ?1, published_at = ?2,
                     reviewed_by = ?3, reviewed_at = ?4, review_comment = ?5
                 WHERE id = ?6 AND status = 'pending'",
                params![
                    published_version,
                    now,
                    reviewed_by,
                    now,
                    review_comment,
                    id
                ],
            )
            .map_err(WorkspaceError::from)?;
        if affected == 0 {
            return Err(WorkspaceError::not_found(
                "publish_queue (pending)",
                id.to_string(),
            ));
        }
        Ok(())
    }
}

// =============================================================================
// verdict_contracts 表 CRUD — 界面升级 v1.0 阶段 A.1/A.3
// =============================================================================

impl WorkspaceDb {
    /// 创建判定契约
    ///
    /// 版本号自动取 (workspace_id, name) 下 max(version)+1。
    /// 若 is_default=true, 先清除该 workspace 旧默认标记 (保证至多一条默认)。
    pub fn insert_verdict_contract(
        &self,
        workspace_id: &str,
        name: &str,
        rules_json: &str,
        is_default: bool,
        created_by: &str,
    ) -> WorkspaceResult<VerdictContractRecord> {
        let conn = self.lock()?;
        let now = Utc::now().to_rfc3339();

        // 校验 rules_json 是合法 JSON
        serde_json::from_str::<serde_json::Value>(rules_json)
            .map_err(|e| WorkspaceError::invalid_input(format!("invalid rules_json: {e}")))?;

        // 清除旧默认 (事务内)
        if is_default {
            conn.execute(
                "UPDATE verdict_contracts SET is_default = 0, updated_at = ?1
                 WHERE workspace_id = ?2 AND is_default = 1",
                params![now, workspace_id],
            )
            .map_err(WorkspaceError::from)?;
        }

        // 计算下一版本号
        let max_version: Option<i64> = conn
            .query_row(
                "SELECT MAX(version) FROM verdict_contracts WHERE workspace_id = ?1 AND name = ?2",
                params![workspace_id, name],
                |row| row.get(0),
            )
            .optional()
            .map_err(WorkspaceError::from)?
            .flatten();
        let version = max_version.unwrap_or(0) + 1;

        conn.execute(
            "INSERT INTO verdict_contracts
                (workspace_id, name, version, rules_json, is_default, created_by, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
            params![
                workspace_id,
                name,
                version,
                rules_json,
                is_default as i64,
                created_by,
                now,
            ],
        )
        .map_err(WorkspaceError::from)?;
        let id = conn.last_insert_rowid();
        drop(conn);
        self.get_verdict_contract(id)
    }

    /// 获取判定契约
    pub fn get_verdict_contract(&self, id: i64) -> WorkspaceResult<VerdictContractRecord> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT id, workspace_id, name, version, rules_json, is_default,
                    created_by, created_at, updated_at
             FROM verdict_contracts WHERE id = ?1",
            params![id],
            row_to_verdict_contract,
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => {
                WorkspaceError::not_found("verdict_contract", id.to_string())
            }
            other => WorkspaceError::from(other),
        })
    }

    /// 列出 workspace 的判定契约
    pub fn list_verdict_contracts(
        &self,
        workspace_id: &str,
    ) -> WorkspaceResult<Vec<VerdictContractRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, workspace_id, name, version, rules_json, is_default,
                        created_by, created_at, updated_at
                 FROM verdict_contracts
                 WHERE workspace_id = ?1
                 ORDER BY name ASC, version DESC",
            )
            .map_err(WorkspaceError::from)?;
        let rows = stmt
            .query_map(params![workspace_id], row_to_verdict_contract)
            .map_err(WorkspaceError::from)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(WorkspaceError::from)?);
        }
        Ok(out)
    }

    /// 获取 workspace 的默认契约 (is_default=1)
    pub fn get_default_verdict_contract(
        &self,
        workspace_id: &str,
    ) -> WorkspaceResult<Option<VerdictContractRecord>> {
        let conn = self.lock()?;
        let r = conn
            .query_row(
                "SELECT id, workspace_id, name, version, rules_json, is_default,
                        created_by, created_at, updated_at
                 FROM verdict_contracts
                 WHERE workspace_id = ?1 AND is_default = 1
                 LIMIT 1",
                params![workspace_id],
                row_to_verdict_contract,
            )
            .optional()
            .map_err(WorkspaceError::from)?;
        Ok(r)
    }

    /// 更新判定契约 (rules_json / is_default / name)
    pub fn update_verdict_contract(
        &self,
        id: i64,
        rules_json: Option<&str>,
        is_default: Option<bool>,
        name: Option<&str>,
    ) -> WorkspaceResult<VerdictContractRecord> {
        // 先取现有记录 (确定 workspace_id 用于清默认)
        let existing = self.get_verdict_contract(id)?;
        {
            let conn = self.lock()?;
            let now = Utc::now().to_rfc3339();

            if let Some(default) = is_default {
                if default {
                    conn.execute(
                        "UPDATE verdict_contracts SET is_default = 0, updated_at = ?1
                         WHERE workspace_id = ?2 AND is_default = 1 AND id != ?3",
                        params![now, existing.workspace_id, id],
                    )
                    .map_err(WorkspaceError::from)?;
                }
            }

            if let Some(rj) = rules_json {
                serde_json::from_str::<serde_json::Value>(rj).map_err(|e| {
                    WorkspaceError::invalid_input(format!("invalid rules_json: {e}"))
                })?;
                conn.execute(
                    "UPDATE verdict_contracts SET rules_json = ?1, updated_at = ?2 WHERE id = ?3",
                    params![rj, now, id],
                )
                .map_err(WorkspaceError::from)?;
            }
            if let Some(d) = is_default {
                conn.execute(
                    "UPDATE verdict_contracts SET is_default = ?1, updated_at = ?2 WHERE id = ?3",
                    params![d as i64, now, id],
                )
                .map_err(WorkspaceError::from)?;
            }
            if let Some(n) = name {
                conn.execute(
                    "UPDATE verdict_contracts SET name = ?1, updated_at = ?2 WHERE id = ?3",
                    params![n, now, id],
                )
                .map_err(WorkspaceError::from)?;
            }
        }
        self.get_verdict_contract(id)
    }

    /// 删除判定契约
    pub fn delete_verdict_contract(&self, id: i64) -> WorkspaceResult<()> {
        let conn = self.lock()?;
        let affected = conn
            .execute("DELETE FROM verdict_contracts WHERE id = ?1", params![id])
            .map_err(WorkspaceError::from)?;
        if affected == 0 {
            return Err(WorkspaceError::not_found(
                "verdict_contract",
                id.to_string(),
            ));
        }
        Ok(())
    }
}

// =============================================================================
// version_clock_map 表 CRUD — 界面升级 v1.0 阶段 A.1/A.4
// =============================================================================

impl WorkspaceDb {
    /// 旁路记录 version → wall-clock (INSERT OR REPLACE, 事务外旁路写)
    ///
    /// 设计约束: 绝不进审计链哈希 (00 §六/§七)。
    pub fn record_version_clock(
        &self,
        session_id: i64,
        version: i64,
        wall_clock: &str,
        source: Option<&str>,
    ) -> WorkspaceResult<VersionClockMapRecord> {
        let conn = self.lock()?;
        let src = source.unwrap_or("ttd_sidecar");
        conn.execute(
            "INSERT INTO version_clock_map (session_id, version, wall_clock, source)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(session_id, version) DO UPDATE SET wall_clock = ?3, source = ?4",
            params![session_id, version, wall_clock, src],
        )
        .map_err(WorkspaceError::from)?;
        // 回读保证返回 wall_clock 解析一致
        conn.query_row(
            "SELECT id, session_id, version, wall_clock, source
             FROM version_clock_map WHERE session_id = ?1 AND version = ?2",
            params![session_id, version],
            row_to_version_clock,
        )
        .map_err(|e| WorkspaceError::DatabaseError(format!("record_version_clock readback: {e}")))
    }

    /// 范围查询 [from_version, to_version] 的 wall-clock
    pub fn lookup_version_clock_range(
        &self,
        session_id: i64,
        from_version: Option<i64>,
        to_version: Option<i64>,
    ) -> WorkspaceResult<Vec<VersionClockMapRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, session_id, version, wall_clock, source
                 FROM version_clock_map
                 WHERE session_id = ?1
                   AND (?2 IS NULL OR version >= ?2)
                   AND (?3 IS NULL OR version <= ?3)
                 ORDER BY version ASC",
            )
            .map_err(WorkspaceError::from)?;
        let rows = stmt
            .query_map(
                params![session_id, from_version, to_version],
                row_to_version_clock,
            )
            .map_err(WorkspaceError::from)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(WorkspaceError::from)?);
        }
        Ok(out)
    }
}

// =============================================================================
// 行映射辅助函数
// =============================================================================

/// 解析 RFC3339 时间字符串
fn parse_dt(s: String) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(&s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

/// workspaces 表行映射
fn row_to_workspace(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkspaceRecord> {
    let state_str: String = row.get(6)?;
    let state = WorkspaceState::from_str(&state_str).unwrap_or(WorkspaceState::Active);
    Ok(WorkspaceRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        owner_id: row.get(2)?,
        created_at: parse_dt(row.get(3)?),
        updated_at: parse_dt(row.get(4)?),
        archived_at: row.get::<_, Option<String>>(5)?.map(parse_dt),
        state,
        description: row.get(7)?,
    })
}

/// rules 表行映射
fn row_to_rule(row: &rusqlite::Row<'_>) -> rusqlite::Result<RuleRecord> {
    let state_str: String = row.get(4)?;
    let state = RuleState::from_str(&state_str).unwrap_or(RuleState::Draft);
    // metadata 为 v3 新增列; 老代码路径不 SELECT 此列时, 安全兜底为 "{}"。
    let metadata: String = row.get(10).unwrap_or_else(|_| "{}".to_string());
    Ok(RuleRecord {
        id: row.get(0)?,
        workspace_id: row.get(1)?,
        name: row.get(2)?,
        current_version_id: row.get(3)?,
        state,
        created_at: parse_dt(row.get(5)?),
        updated_at: parse_dt(row.get(6)?),
        archived_at: row.get::<_, Option<String>>(7)?.map(parse_dt),
        description: row.get(8)?,
        created_by: row.get(9)?,
        metadata,
    })
}

/// rule_versions 表行映射
fn row_to_rule_version(row: &rusqlite::Row<'_>) -> rusqlite::Result<RuleVersionRecord> {
    let state_str: String = row.get(6)?;
    let state = RuleVersionState::from_str(&state_str).unwrap_or(RuleVersionState::Current);
    Ok(RuleVersionRecord {
        id: row.get(0)?,
        rule_id: row.get(1)?,
        version: row.get::<_, i64>(2)? as u64,
        content_hash: row.get(3)?,
        content: row.get(4)?,
        created_at: parse_dt(row.get(5)?),
        state,
        created_by: row.get(7)?,
    })
}

/// rule_session_bindings 表行映射
fn row_to_binding(row: &rusqlite::Row<'_>) -> rusqlite::Result<RuleSessionBinding> {
    let state_str: String = row.get(6)?;
    let state = SessionBindingState::from_str(&state_str).unwrap_or(SessionBindingState::Bound);
    Ok(RuleSessionBinding {
        id: row.get(0)?,
        rule_version_id: row.get(1)?,
        session_id: row.get::<_, i64>(2)? as u64,
        workspace_id: row.get(3)?,
        bound_at: parse_dt(row.get(4)?),
        unbound_at: row.get::<_, Option<String>>(5)?.map(parse_dt),
        state,
    })
}

/// sandbox_sessions 表行映射
fn row_to_sandbox(row: &rusqlite::Row<'_>) -> rusqlite::Result<SandboxSession> {
    let status_str: String = row.get(6)?;
    let status = SandboxStatus::from_str(&status_str).unwrap_or(SandboxStatus::Running);
    Ok(SandboxSession {
        id: row.get(0)?,
        workspace_id: row.get(1)?,
        tcb_session_id: row.get(2)?,
        parent_session_id: row.get(3)?,
        draft_ruleset_hash: row.get(4)?,
        test_dataset_id: row.get(5)?,
        status,
        started_at: parse_dt(row.get(7)?),
        closed_at: row.get::<_, Option<String>>(8)?.map(parse_dt),
        started_by: row.get(9)?,
        export_path: row.get(10)?,
    })
}

/// test_datasets 表行映射
fn row_to_test_dataset(row: &rusqlite::Row<'_>) -> rusqlite::Result<TestDatasetRecord> {
    Ok(TestDatasetRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        workspace_id: row.get(2)?,
        cases_json: row.get(3)?,
        case_count: row.get(4)?,
        created_at: parse_dt(row.get(5)?),
        created_by: row.get(6)?,
        description: row.get(7)?,
    })
}

/// verdict_contracts 表行映射 (阶段 A.1/A.3)
fn row_to_verdict_contract(row: &rusqlite::Row<'_>) -> rusqlite::Result<VerdictContractRecord> {
    let is_default: i64 = row.get(5)?;
    Ok(VerdictContractRecord {
        id: row.get(0)?,
        workspace_id: row.get(1)?,
        name: row.get(2)?,
        version: row.get(3)?,
        rules_json: row.get(4)?,
        is_default: is_default != 0,
        created_by: row.get(6)?,
        created_at: parse_dt(row.get(7)?),
        updated_at: parse_dt(row.get(8)?),
    })
}

/// version_clock_map 表行映射 (阶段 A.1/A.4)
fn row_to_version_clock(row: &rusqlite::Row<'_>) -> rusqlite::Result<VersionClockMapRecord> {
    Ok(VersionClockMapRecord {
        id: row.get(0)?,
        session_id: row.get(1)?,
        version: row.get(2)?,
        wall_clock: parse_dt(row.get(3)?),
        source: row.get(4)?,
    })
}

/// production_state 表行映射
fn row_to_production_state(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProductionStateRecord> {
    Ok(ProductionStateRecord {
        id: row.get(0)?,
        current_session_id: row.get(1)?,
        ruleset_version: row.get(2)?,
        ruleset_hash: row.get(3)?,
        last_operated_by: row.get(4)?,
        updated_at: parse_dt(row.get(5)?),
    })
}

/// production_audit 表行映射
fn row_to_production_audit(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProductionAuditRecord> {
    Ok(ProductionAuditRecord {
        id: row.get(0)?,
        event_type: row.get(1)?,
        ruleset_version: row.get(2)?,
        previous_version: row.get(3)?,
        ruleset_hash: row.get(4)?,
        tcb_session_id: row.get(5)?,
        source_workspace_ids: row.get(6)?,
        operated_by: row.get(7)?,
        operated_at: parse_dt(row.get(8)?),
        reason: row.get(9)?,
        test_report_paths: row.get(10)?,
        ruleset_snapshot: row.get(11)?,
    })
}

/// publish_queue 表行映射
///
/// 列顺序 (0-based):
/// 0=id 1=workspace_id 2=final_candidate_rules 3=ruleset_hash
/// 4=test_report_sandbox_id 5=submitted_by 6=submitted_at
/// 7=reviewed_by 8=reviewed_at 9=review_comment
/// 10=published_version 11=published_at 12=status 13=description
fn row_to_publish_queue(row: &rusqlite::Row<'_>) -> rusqlite::Result<PublishQueueItem> {
    let status_str: String = row.get(12)?;
    let status = PublishStatus::from_str(&status_str).unwrap_or(PublishStatus::Pending);
    Ok(PublishQueueItem {
        id: row.get(0)?,
        workspace_id: row.get(1)?,
        final_candidate_rules: row.get(2)?,
        ruleset_hash: row.get(3)?,
        test_report_sandbox_id: row.get(4)?,
        submitted_by: row.get(5)?,
        submitted_at: parse_dt(row.get(6)?),
        reviewed_by: row.get(7)?,
        reviewed_at: row.get::<_, Option<String>>(8)?.map(parse_dt),
        review_comment: row.get(9)?,
        published_version: row.get(10)?,
        published_at: row.get::<_, Option<String>>(11)?.map(parse_dt),
        status,
        description: row.get(13)?,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::models::{CreateWorkspaceRequest, MemberRole};

    fn make_db() -> WorkspaceDb {
        WorkspaceDb::in_memory().expect("in_memory db should succeed")
    }

    fn make_workspace(db: &WorkspaceDb, name: &str, owner: &str) -> WorkspaceRecord {
        let now = Utc::now();
        let ws = WorkspaceRecord {
            id: ulid::Ulid::new().to_string(),
            name: name.to_string(),
            owner_id: owner.to_string(),
            created_at: now,
            updated_at: now,
            archived_at: None,
            state: WorkspaceState::Active,
            description: None,
        };
        db.insert_workspace(&ws).expect("insert workspace");
        // 自动添加 owner 为成员
        db.insert_member(&ws.id, owner, MemberRole::Owner.as_str())
            .expect("insert owner member");
        ws
    }

    #[test]
    fn test_schema_version_after_migrate() {
        let db = make_db();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn test_workspace_crud() {
        let db = make_db();
        let ws = make_workspace(&db, "team-alpha", "user-1");
        assert_eq!(ws.state, WorkspaceState::Active);

        // get
        let fetched = db.get_workspace(&ws.id).unwrap();
        assert_eq!(fetched.name, "team-alpha");

        // list
        let list = db.list_workspaces(None).unwrap();
        assert_eq!(list.len(), 1);

        // list by owner
        let list_mine = db.list_workspaces(Some("user-1")).unwrap();
        assert_eq!(list_mine.len(), 1);
        let list_other = db.list_workspaces(Some("user-2")).unwrap();
        assert_eq!(list_other.len(), 0);

        // update
        let updated = db
            .update_workspace(&ws.id, Some("team-alpha-v2"), Some("desc"))
            .unwrap();
        assert_eq!(updated.name, "team-alpha-v2");
        assert_eq!(updated.description.as_deref(), Some("desc"));

        // archive
        let archived = db.archive_workspace(&ws.id).unwrap();
        assert_eq!(archived.state, WorkspaceState::Archived);
        assert!(archived.archived_at.is_some());
    }

    #[test]
    fn test_workspace_not_found() {
        let db = make_db();
        let err = db.get_workspace("nonexistent").unwrap_err();
        assert!(matches!(err, WorkspaceError::NotFound { .. }));
    }

    #[test]
    fn test_member_crud() {
        let db = make_db();
        let ws = make_workspace(&db, "team", "owner-1");

        // 添加成员
        let m = db
            .insert_member(&ws.id, "user-2", MemberRole::Editor.as_str())
            .unwrap();
        assert_eq!(m.role, "editor");

        // list
        let members = db.list_members(&ws.id).unwrap();
        assert_eq!(members.len(), 2);

        // 删除非 owner 成员
        db.delete_member(&ws.id, "user-2").unwrap();
        let members = db.list_members(&ws.id).unwrap();
        assert_eq!(members.len(), 1);

        // 不能删除 owner
        let err = db.delete_member(&ws.id, "owner-1").unwrap_err();
        assert!(matches!(err, WorkspaceError::InvalidInput(_)));
    }

    #[test]
    fn test_invalid_role_rejected() {
        let db = make_db();
        let ws = make_workspace(&db, "team", "owner-1");
        let err = db.insert_member(&ws.id, "user-2", "superuser").unwrap_err();
        assert!(matches!(err, WorkspaceError::InvalidInput(_)));
    }

    #[test]
    fn test_rule_crud_and_state_machine() {
        let db = make_db();
        let ws = make_workspace(&db, "team", "owner-1");

        let now = Utc::now();
        let rule = RuleRecord {
            id: ulid::Ulid::new().to_string(),
            workspace_id: ws.id.clone(),
            name: "rule-1".to_string(),
            current_version_id: None,
            state: RuleState::Draft,
            created_at: now,
            updated_at: now,
            archived_at: None,
            description: None,
            created_by: "owner-1".to_string(),
            metadata: "{}".to_string(),
        };
        db.insert_rule(&rule).unwrap();

        // get
        let fetched = db.get_rule(&rule.id).unwrap();
        assert_eq!(fetched.name, "rule-1");
        assert_eq!(fetched.state, RuleState::Draft);

        // get by name
        let by_name = db.get_rule_by_name(&ws.id, "rule-1").unwrap();
        assert_eq!(by_name.id, rule.id);

        // 状态机: Draft -> Candidate -> Active
        let candidate = db
            .update_rule_state(&rule.id, RuleState::Candidate)
            .unwrap();
        assert_eq!(candidate.state, RuleState::Candidate);
        let active = db.update_rule_state(&rule.id, RuleState::Active).unwrap();
        assert_eq!(active.state, RuleState::Active);

        // list
        let list = db.list_rules(&ws.id).unwrap();
        assert_eq!(list.len(), 1);

        // archive
        let archived = db.archive_rule(&rule.id).unwrap();
        assert_eq!(archived.state, RuleState::Archived);
    }

    #[test]
    fn test_rule_unique_name_per_workspace() {
        let db = make_db();
        let ws = make_workspace(&db, "team", "owner-1");
        let now = Utc::now();

        let rule1 = RuleRecord {
            id: ulid::Ulid::new().to_string(),
            workspace_id: ws.id.clone(),
            name: "rule-1".to_string(),
            current_version_id: None,
            state: RuleState::Draft,
            created_at: now,
            updated_at: now,
            archived_at: None,
            description: None,
            created_by: "owner-1".to_string(),
            metadata: "{}".to_string(),
        };
        db.insert_rule(&rule1).unwrap();

        // 同名规则在相同 workspace 应失败
        let rule2 = RuleRecord {
            id: ulid::Ulid::new().to_string(),
            workspace_id: ws.id.clone(),
            name: "rule-1".to_string(),
            ..rule1.clone()
        };
        let err = db.insert_rule(&rule2).unwrap_err();
        assert!(matches!(err, WorkspaceError::AlreadyExists { .. }));

        // 不同 workspace 同名规则应成功
        let ws2 = make_workspace(&db, "team-2", "owner-1");
        let rule3 = RuleRecord {
            id: ulid::Ulid::new().to_string(),
            workspace_id: ws2.id.clone(),
            name: "rule-1".to_string(),
            ..rule1.clone()
        };
        db.insert_rule(&rule3).unwrap();
    }

    #[test]
    fn test_rule_version_crud() {
        let db = make_db();
        let ws = make_workspace(&db, "team", "owner-1");
        let now = Utc::now();

        let rule = RuleRecord {
            id: ulid::Ulid::new().to_string(),
            workspace_id: ws.id.clone(),
            name: "rule-1".to_string(),
            current_version_id: None,
            state: RuleState::Draft,
            created_at: now,
            updated_at: now,
            archived_at: None,
            description: None,
            created_by: "owner-1".to_string(),
            metadata: "{}".to_string(),
        };
        db.insert_rule(&rule).unwrap();

        // 版本号递增
        let v1_num = db.get_next_version_number(&rule.id).unwrap();
        assert_eq!(v1_num, 1);

        let rv1 = RuleVersionRecord {
            id: ulid::Ulid::new().to_string(),
            rule_id: rule.id.clone(),
            version: 1,
            content_hash: "hash-1".to_string(),
            content: r#"{"transform":[{"type":"noop"}]}"#.to_string(),
            created_at: now,
            state: RuleVersionState::Current,
            created_by: "owner-1".to_string(),
        };
        db.insert_rule_version(&rv1).unwrap();

        let v2_num = db.get_next_version_number(&rule.id).unwrap();
        assert_eq!(v2_num, 2);

        // 设置 current_version
        db.update_rule_current_version(&rule.id, &rv1.id).unwrap();
        let current = db.get_current_version(&rule.id).unwrap();
        assert!(current.is_some());
        assert_eq!(current.unwrap().id, rv1.id);

        // 标记 superseded
        db.mark_version_superseded(&rv1.id).unwrap();
        let rv1_after = db.get_rule_version(&rv1.id).unwrap();
        assert_eq!(rv1_after.state, RuleVersionState::Superseded);

        // list
        let list = db.list_rule_versions(&rule.id).unwrap();
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn test_session_crud() {
        let db = make_db();
        let ws = make_workspace(&db, "team", "owner-1");
        let now = Utc::now();

        let session = SessionRecord {
            id: 100,
            workspace_id: ws.id.clone(),
            rule_id: None,
            rule_version_id: None,
            created_at: now,
            closed_at: None,
            created_by: "owner-1".to_string(),
        };
        db.insert_session(&session).unwrap();

        // get
        let fetched = db.get_session(100).unwrap();
        assert_eq!(fetched.workspace_id, ws.id);

        // list by workspace
        let list = db.list_sessions_by_workspace(&ws.id).unwrap();
        assert_eq!(list.len(), 1);

        // close
        let closed = db.close_session(100).unwrap();
        assert!(closed.closed_at.is_some());
    }

    #[test]
    fn test_binding_crud() {
        let db = make_db();
        let ws = make_workspace(&db, "team", "owner-1");
        let now = Utc::now();

        // 先创建 rule + version + session
        let rule = RuleRecord {
            id: ulid::Ulid::new().to_string(),
            workspace_id: ws.id.clone(),
            name: "rule-1".to_string(),
            current_version_id: None,
            state: RuleState::Draft,
            created_at: now,
            updated_at: now,
            archived_at: None,
            description: None,
            created_by: "owner-1".to_string(),
            metadata: "{}".to_string(),
        };
        db.insert_rule(&rule).unwrap();

        let rv = RuleVersionRecord {
            id: ulid::Ulid::new().to_string(),
            rule_id: rule.id.clone(),
            version: 1,
            content_hash: "hash".to_string(),
            content: "{}".to_string(),
            created_at: now,
            state: RuleVersionState::Current,
            created_by: "owner-1".to_string(),
        };
        db.insert_rule_version(&rv).unwrap();

        let session = SessionRecord {
            id: 200,
            workspace_id: ws.id.clone(),
            rule_id: Some(rule.id.clone()),
            rule_version_id: Some(rv.id.clone()),
            created_at: now,
            closed_at: None,
            created_by: "owner-1".to_string(),
        };
        db.insert_session(&session).unwrap();

        // 创建 binding
        let binding = RuleSessionBinding {
            id: ulid::Ulid::new().to_string(),
            rule_version_id: rv.id.clone(),
            session_id: 200,
            workspace_id: ws.id.clone(),
            bound_at: now,
            unbound_at: None,
            state: SessionBindingState::Bound,
        };
        db.insert_binding(&binding).unwrap();

        // list by session
        let by_session = db.list_bindings_by_session(200).unwrap();
        assert_eq!(by_session.len(), 1);

        // list by rule_version
        let by_rv = db.list_bindings_by_rule_version(&rv.id).unwrap();
        assert_eq!(by_rv.len(), 1);

        // close binding
        let closed = db.close_binding(&binding.id).unwrap();
        assert_eq!(closed.state, SessionBindingState::Closed);
        assert!(closed.unbound_at.is_some());
    }

    #[test]
    fn test_workspace_isolation() {
        // 不同 workspace 的数据相互隔离
        let db = make_db();
        let ws1 = make_workspace(&db, "team-1", "owner-1");
        let ws2 = make_workspace(&db, "team-2", "owner-1");
        let now = Utc::now();

        // ws1 创建 rule
        let rule1 = RuleRecord {
            id: ulid::Ulid::new().to_string(),
            workspace_id: ws1.id.clone(),
            name: "rule".to_string(),
            current_version_id: None,
            state: RuleState::Draft,
            created_at: now,
            updated_at: now,
            archived_at: None,
            description: None,
            created_by: "owner-1".to_string(),
            metadata: "{}".to_string(),
        };
        db.insert_rule(&rule1).unwrap();

        // ws2 创建同名 rule (应成功,因为 workspace 不同)
        let rule2 = RuleRecord {
            id: ulid::Ulid::new().to_string(),
            workspace_id: ws2.id.clone(),
            name: "rule".to_string(),
            ..rule1.clone()
        };
        db.insert_rule(&rule2).unwrap();

        // list 隔离
        let ws1_rules = db.list_rules(&ws1.id).unwrap();
        assert_eq!(ws1_rules.len(), 1);
        assert_eq!(ws1_rules[0].id, rule1.id);

        let ws2_rules = db.list_rules(&ws2.id).unwrap();
        assert_eq!(ws2_rules.len(), 1);
        assert_eq!(ws2_rules[0].id, rule2.id);
    }

    #[test]
    fn test_deserialize_create_workspace_request() {
        let json = r#"{"name":"test","owner_id":"user-1","description":"d"}"#;
        let req: CreateWorkspaceRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.name, "test");
        assert_eq!(req.owner_id, "user-1");
        assert_eq!(req.description.as_deref(), Some("d"));

        // description 可选
        let json2 = r#"{"name":"test","owner_id":"user-1"}"#;
        let req2: CreateWorkspaceRequest = serde_json::from_str(json2).unwrap();
        assert!(req2.description.is_none());
    }
}

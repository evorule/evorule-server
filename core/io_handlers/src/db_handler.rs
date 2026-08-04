// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! Database I/O Handler —— 基于 `sqlx` 接入 SQLite。
//!
//! 执行 SQL 语句并将结果转换为 `JsonValue`：
//! - `SELECT` 等查询语句返回 `JsonValue::Array`（每行是一个 `JsonValue::Object`）。
//! - `INSERT`/`UPDATE`/`DELETE` 等非查询语句返回受影响行数 `JsonValue::Integer`。
//!
//! 参数通过 `?` 占位符绑定，避免 SQL 注入。

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use evorule_reactor::{IoHandler, IoResult};
use evorule_tcb::JsonValue;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteQueryResult,
    SqliteRow,
};
use sqlx::{Column, Row};

/// 单次 DB 查询超时（DB 5s）
const DB_TIMEOUT: Duration = Duration::from_secs(5);

/// 连接池最大连接数
const MAX_CONNECTIONS: u32 = 4;

/// 连接获取超时（避免等待过长）
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// SQLite 处理器
///
/// 持有 `sqlx::SqlitePool` 连接池，执行 SQL 查询。
///
/// # 安全注意
/// `execute` 接收的 `query` 字段是任意 SQL 文本，会原样提交给 SQLite 执行
/// （参数通过 `?` 占位符绑定，可防 SQL 注入）。但规则编写者可通过 IoRequest
/// 执行任意 DDL/DML（包括 `DROP TABLE`）。调用方应确保 `core_eval.json` 规则
/// 来源可信，并在最小权限的数据库上运行本 handler。
pub struct DbHandler {
    pool: SqlitePool,
}

impl DbHandler {
    /// 异步初始化连接池。
    ///
    /// 等价于 [`DbHandler::connect`]，传入完整的数据库 URL（如 `sqlite::memory:`
    /// 或 `sqlite://path/to/db.sqlite`）。
    pub async fn new(database_url: String) -> Result<Self, sqlx::Error> {
        Self::connect(&database_url).await
    }

    /// 异步连接数据库并创建连接池。
    ///
    /// # 参数
    /// - `database_url`: SQLite 连接字符串（如 `sqlite://./data/demo.sqlite`
    ///   或 `sqlite::memory:`）。
    ///
    /// # 错误
    /// `database_url` 解析失败时返回 `sqlx::Error`，**不会静默回退到内存库**。
    /// 旧实现用 `unwrap_or_else(|_| SqliteConnectOptions::new())` 在解析失败时
    /// 回退到 `sqlite::memory:`，错误的 URL 会悄悄连到内存库，写入的数据进程
    /// 退出即丢失，且不同连接看到不同数据（memory 库 per-connection）。
    pub async fn connect(database_url: &str) -> Result<Self, sqlx::Error> {
        let options: SqliteConnectOptions = database_url
            .parse::<SqliteConnectOptions>()?
            .create_if_missing(true)
            .busy_timeout(Duration::from_secs(2))
            .foreign_keys(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(MAX_CONNECTIONS)
            .acquire_timeout(CONNECT_TIMEOUT)
            .connect_with(options)
            .await?;

        Ok(Self { pool })
    }

    /// 通过文件路径异步连接 SQLite，自动创建不存在的文件。
    ///
    /// 在 Windows 上避免 URL 反斜杠解析问题，推荐使用此方法。
    ///
    /// # 参数
    /// - `path`: 数据库文件路径（如 `./data/demo.sqlite`）。
    pub async fn connect_file(path: impl AsRef<Path>) -> Result<Self, sqlx::Error> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .busy_timeout(Duration::from_secs(2))
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal);

        let pool = SqlitePoolOptions::new()
            .max_connections(MAX_CONNECTIONS)
            .acquire_timeout(CONNECT_TIMEOUT)
            .connect_with(options)
            .await?;

        Ok(Self { pool })
    }
}

#[async_trait]
impl IoHandler for DbHandler {
    async fn execute(&self, params: &JsonValue) -> IoResult {
        // 提取 SQL（必需）
        let query_str = params
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required param: query".to_string())?;

        // 构建查询并绑定参数（可选，数组形式）
        let mut query = sqlx::query(query_str);
        if let Some(args) = params.get("params").and_then(|v| v.as_array()) {
            for arg in args {
                query = match arg {
                    JsonValue::Integer(i) => query.bind(*i),
                    JsonValue::String(s) => query.bind(s.clone()),
                    JsonValue::Bool(b) => query.bind(*b),
                    JsonValue::Null => query.bind(Option::<i64>::None),
                    // 复合类型序列化为 JSON 文本
                    JsonValue::Array(_) | JsonValue::Object(_) => query.bind(arg.to_string()),
                };
            }
        }

        // 判断语句是否会返回行：SELECT/WITH/VALUES/PRAGMA/EXPLAIN 首词，
        // 或任意位置含 RETURNING 子句（INSERT/UPDATE/DELETE ... RETURNING）。
        // 旧实现只识别 SELECT 开头，CTE/RETURNING/注释开头会误判为非查询，
        // 走 execute() 返回 rows_affected，查询结果被丢弃。
        let is_query = is_query_statement(query_str);

        if is_query {
            // 5s 超时，防止 DB 卡住导致会话僵死
            let rows: Vec<SqliteRow> =
                tokio::time::timeout(DB_TIMEOUT, query.fetch_all(&self.pool))
                    .await
                    .map_err(|_| format!("db query timed out after {}s", DB_TIMEOUT.as_secs()))?
                    .map_err(|e| e.to_string())?;
            let arr: Vec<JsonValue> = rows.iter().map(row_to_json).collect();
            Ok(JsonValue::Array(arr))
        } else {
            let result: SqliteQueryResult =
                tokio::time::timeout(DB_TIMEOUT, query.execute(&self.pool))
                    .await
                    .map_err(|_| format!("db execute timed out after {}s", DB_TIMEOUT.as_secs()))?
                    .map_err(|e| e.to_string())?;
            Ok(JsonValue::Integer(result.rows_affected() as i64))
        }
    }
}

/// 将一行 `SqliteRow` 转换为 `JsonValue::Object`。
///
/// 按列类型依次尝试 i64 / bool / String / f64 解码，
/// 无法解码的列回退为 `JsonValue::Null`。
fn row_to_json(row: &SqliteRow) -> JsonValue {
    let mut obj: BTreeMap<String, JsonValue> = BTreeMap::new();
    for (idx, col) in row.columns().iter().enumerate() {
        let name = col.name().to_string();
        let value = if let Ok(Some(i)) = row.try_get::<Option<i64>, _>(idx) {
            JsonValue::Integer(i)
        } else if let Ok(Some(b)) = row.try_get::<Option<bool>, _>(idx) {
            JsonValue::Bool(b)
        } else if let Ok(Some(s)) = row.try_get::<Option<String>, _>(idx) {
            JsonValue::String(s)
        } else if let Ok(Some(f)) = row.try_get::<Option<f64>, _>(idx) {
            // JsonValue 无浮点类型，用字符串保留值。
            // 注：Rust 的 f64 Display 不是最短 round-trip 表示，
            // 如 0.1+0.2 会得到 "0.30000000000000004"。
            // 需要精确浮点的调用方应在 SQL 层用 ROUND()/printf() 控制位数。
            JsonValue::String(f.to_string())
        } else {
            JsonValue::Null
        };
        obj.insert(name, value);
    }
    JsonValue::Object(obj)
}

/// 判断 SQL 语句是否会返回结果行。
///
/// 识别以下情况：
/// - 首词为 `SELECT`/`WITH`/`VALUES`/`PRAGMA`/`EXPLAIN`（先跳过前导注释与空白）
/// - 语句包含 `RETURNING` 子句（SQLite 3.35+ 的 `INSERT/UPDATE/DELETE ... RETURNING`）
///
/// 非查询语句（无 RETURNING 的 `INSERT`/`UPDATE`/`DELETE`/`CREATE`/`DROP`/`ALTER`
/// 等）返回 false，调用方走 `query.execute()` 拿 `rows_affected`。
///
/// 旧实现 `trim_start().starts_with("SELECT")` 会把 CTE (`WITH ... SELECT`)、
/// `RETURNING` 子句、注释开头 (`-- note\nSELECT ...`) 误判为非查询，导致
/// 查询结果被丢弃。本函数修复这些情况。
fn is_query_statement(sql: &str) -> bool {
    let stmt = strip_leading_sql_comments(sql);
    let upper = stmt.to_ascii_uppercase();
    let first_word = upper.split_whitespace().next().unwrap_or("");
    let is_query_first = matches!(
        first_word,
        "SELECT" | "WITH" | "VALUES" | "PRAGMA" | "EXPLAIN"
    );
    // RETURNING 子句：INSERT/UPDATE/DELETE ... RETURNING 会返回行。
    // 用 " RETURNING "（前后空格）降低误判，列名恰好叫 RETURNING 极罕见。
    let has_returning = upper.contains(" RETURNING ");
    is_query_first || has_returning
}

/// 跳过 SQL 前导注释（`--` 行注释与 `/* */` 块注释）与空白。
///
/// SQLite 允许语句以注释开头，旧实现的 `trim_start()` 无法跳过注释，
/// 导致 `-- note\nSELECT ...` 被误判。本函数循环跳过前导空白与注释，
/// 返回第一条实际 SQL 的起始位置。
fn strip_leading_sql_comments(sql: &str) -> &str {
    let mut s = sql;
    loop {
        s = s.trim_start();
        if s.starts_with("--") {
            // 行注释：跳到下一个换行
            match s.find('\n') {
                Some(idx) => s = &s[idx + 1..],
                None => return "",
            }
        } else if s.starts_with("/*") {
            // 块注释：跳到 */
            match s.find("*/") {
                Some(idx) => s = &s[idx + 2..],
                None => return "",
            }
        } else {
            return s;
        }
    }
}

// ============================================================================
// SQL 语句模板白名单机制 —— StatementWhitelist + WhitelistedDbHandler
//
// 机制-策略分离：SQL 模板是"策略"（运维/业务写在 JSON 配置里），模板名 + 参数
// 是"机制"（IoHandler 只允许调用白名单模板，不允许直接执行任意 SQL）。
//
// statement_whitelist.json 格式：
// {
// "find_user_by_id": {
// "sql": "SELECT id, name FROM users WHERE id = ?",
// "param_names": ["id"],
// "description": "按用户 ID 查询（可选，仅做文档）"
// }
// }
// ============================================================================

/// 单个 SQL 白名单条目
#[derive(Debug, Clone)]
pub struct StatementEntry {
    pub sql: String,
    pub param_names: Vec<String>,
}

/// SQL 白名单集合
#[derive(Debug, Clone, Default)]
pub struct StatementWhitelist {
    entries: std::collections::BTreeMap<String, StatementEntry>,
}

impl StatementWhitelist {
    pub fn empty() -> Self {
        Self {
            entries: std::collections::BTreeMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&StatementEntry> {
        self.entries.get(name)
    }

    /// 从 JSON 文件加载；文件不存在或为空路径返回空集合（不报错）。
    pub fn load_from_file(path: Option<&std::path::Path>) -> Result<Self, String> {
        let Some(p) = path else {
            return Ok(Self::empty());
        };
        if !p.exists() {
            tracing::warn!(
                "statement_whitelist file not found: {} — QUERY_DB 调用会全部拒绝",
                p.display()
            );
            return Ok(Self::empty());
        }
        let raw = std::fs::read_to_string(p)
            .map_err(|e| format!("read statement_whitelist {} failed: {}", p.display(), e))?;
        Self::load_from_str(&raw)
    }

    /// 从 JSON 字符串解析
    pub fn load_from_str(json: &str) -> Result<Self, String> {
        let val: serde_json::Value = serde_json::from_str(json)
            .map_err(|e| format!("parse statement_whitelist json failed: {e}"))?;
        let obj = val
            .as_object()
            .ok_or_else(|| "statement_whitelist 顶层必须是 JSON object".to_string())?;
        let mut entries = std::collections::BTreeMap::new();
        for (name, v) in obj {
            let entry_obj = v
                .as_object()
                .ok_or_else(|| format!("statement '{}' 必须是 JSON object", name))?;
            let sql = entry_obj
                .get("sql")
                .and_then(|x| x.as_str())
                .ok_or_else(|| format!("statement '{}' missing required 'sql' field", name))?
                .to_string();
            // sql 必须是单语句：禁止分号拼接多条（防止一条模板里藏 DROP 等）
            if has_multiple_statements(&sql) {
                return Err(format!(
                    "statement '{}' contains multiple SQL statements (semicolons forbidden)",
                    name
                ));
            }
            let param_names: Vec<String> = entry_obj
                .get("param_names")
                .and_then(|x| x.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            entries.insert(name.clone(), StatementEntry { sql, param_names });
        }
        Ok(Self { entries })
    }
}

/// 判断一段 SQL 是否包含多条语句（以 ; 分隔，跳过字符串字面量内的分号）。
///
/// 用极简状态机扫描，避免引入 sqlparser 依赖。误报（字符串里的; 未被正确跳过）
/// 只会拒绝合法模板（fail-open → fail-closed），是安全偏好。
fn has_multiple_statements(mut sql: &str) -> bool {
    sql = strip_leading_sql_comments(sql);
    let mut in_single = false; // '...'
    let mut in_double = false; // "..."（SQLite 标识符引号）
    let mut in_backtick = false; // `...`（MySQL 风格，SQLite 也支持）
    let mut found_semicolon_at = None::<usize>;
    for (i, ch) in sql.char_indices() {
        match ch {
            '\'' if !in_double && !in_backtick => in_single = !in_single,
            '"' if !in_single && !in_backtick => in_double = !in_double,
            '`' if !in_single && !in_double => in_backtick = !in_backtick,
            ';' if !in_single && !in_double && !in_backtick => {
                found_semicolon_at = Some(i);
                break;
            }
            _ => {}
        }
    }
    let Some(idx) = found_semicolon_at else {
        return false;
    };
    // 分号之后如果除空白和末尾注释还有内容，算多语句
    let tail = &sql[idx + 1..];
    let stripped = strip_leading_sql_comments(tail);
    !stripped.trim().is_empty()
}

/// 白名单包装器：禁止直接传 `query` 字段，只允许 `name` 字段引用白名单模板。
///
/// IoParams 协议：
/// ```json
/// {
/// "name": "find_user_by_id",
/// "params": [123]        // 数组（按位置绑定）或 { "id": 123 }（按名字绑定，模板需声明 param_names）
/// }
/// ```
pub struct WhitelistedDbHandler {
    inner: DbHandler,
    whitelist: StatementWhitelist,
}

impl WhitelistedDbHandler {
    pub fn new(inner: DbHandler, whitelist: StatementWhitelist) -> Self {
        Self { inner, whitelist }
    }

    /// 解析最终传给 DbHandler 的 params：{ query: String, params: Array }
    fn resolve(&self, params: &JsonValue) -> Result<JsonValue, String> {
        if self.whitelist.is_empty() {
            return Err("QUERY_DB disabled: no statement_whitelist loaded; \
                 set --statement-whitelist <path> to enable"
                .to_string());
        }
        // 显式禁止 params.query（防止绕过白名单）
        if params.get("query").is_some() {
            return Err(
                "QUERY_DB: direct 'query' field is forbidden; use 'name' to reference a whitelisted template"
                    .to_string(),
            );
        }
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "QUERY_DB missing required param: 'name' (template name)".to_string())?;
        let entry = self.whitelist.get(name).ok_or_else(|| {
            format!(
                "QUERY_DB: template name '{name}' is not whitelisted; \
                 check statement_whitelist.json"
            )
        })?;

        // 解析 params：数组或对象（对象需 param_names 映射）
        let bound: Vec<JsonValue> = match params.get("params") {
            None => Vec::new(),
            Some(JsonValue::Array(arr)) => arr.clone(),
            Some(JsonValue::Object(obj)) => {
                if entry.param_names.is_empty() {
                    return Err(format!(
                        "QUERY_DB template '{name}' has no param_names declared, \
                         but params was passed as an object (use array form instead)"
                    ));
                }
                let mut arr = Vec::with_capacity(entry.param_names.len());
                for pn in &entry.param_names {
                    let v = obj.get(pn).cloned().unwrap_or(JsonValue::Null);
                    arr.push(v);
                }
                arr
            }
            Some(_) => {
                return Err("QUERY_DB 'params' must be an array or object".to_string());
            }
        };

        // 组装 DbHandler::execute 需要的形式
        let mut out = std::collections::BTreeMap::<String, JsonValue>::new();
        out.insert("query".into(), JsonValue::string(entry.sql.as_str()));
        out.insert("params".into(), JsonValue::Array(bound));
        Ok(JsonValue::Object(out))
    }
}

#[async_trait]
impl IoHandler for WhitelistedDbHandler {
    async fn execute(&self, params: &JsonValue) -> IoResult {
        let resolved = self.resolve(params)?;
        self.inner.execute(&resolved).await
    }
}

#[cfg(test)]
mod whitelist_tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::panic, clippy::expect_used)]
    use super::*;

    #[test]
    fn test_load_from_str_basic() {
        let json = r#"{
            "find": {"sql": "SELECT * FROM t WHERE id = ?", "param_names": ["id"]},
            "list": {"sql": "SELECT * FROM t"}
        }"#;
        let w = StatementWhitelist::load_from_str(json).unwrap();
        assert_eq!(w.len(), 2);
        let find = w.get("find").unwrap();
        assert_eq!(find.param_names, vec!["id".to_string()]);
        assert!(find.sql.contains("WHERE id = ?"));
        let list = w.get("list").unwrap();
        assert!(list.param_names.is_empty());
    }

    #[test]
    fn test_empty_whitelist_blocks() {
        let db_path =
            std::env::temp_dir().join(format!("evorule_wl_empty_{}.sqlite", std::process::id()));
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let db = DbHandler::connect_file(&db_path).await.unwrap();
            let w = WhitelistedDbHandler::new(db, StatementWhitelist::empty());
            let params = JsonValue::object_from_pairs(&[("query", JsonValue::string("SELECT 1"))]);
            let err = w.execute(&params).await.unwrap_err();
            assert!(err.contains("no statement_whitelist loaded"));
        });
    }

    #[test]
    fn test_resolve_array_params() {
        let w = StatementWhitelist::load_from_str(
            r#"{"f":{"sql":"SELECT * FROM t WHERE id = ?","param_names":["id"]}}"#,
        )
        .unwrap();
        // 先构造一个临时 WhitelistedDbHandler 用于 resolve
        // 这里直接 new 一个空内存 DB 就行
        let db_path =
            std::env::temp_dir().join(format!("evorule_wl_resolve_{}.sqlite", std::process::id()));
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let db = DbHandler::connect_file(&db_path).await.unwrap();
            let wl = WhitelistedDbHandler::new(db, w);
            let params = JsonValue::object_from_pairs(&[
                ("name", JsonValue::string("f")),
                ("params", JsonValue::array(vec![JsonValue::Integer(42)])),
            ]);
            let resolved = wl.resolve(&params).unwrap();
            assert_eq!(
                resolved.get("query").and_then(|v| v.as_str()),
                Some("SELECT * FROM t WHERE id = ?")
            );
            let arr = resolved.get("params").and_then(|v| v.as_array()).unwrap();
            assert_eq!(arr.len(), 1);
            assert_eq!(arr[0].as_i64(), Some(42));
        });
    }

    #[test]
    fn test_resolve_object_params_by_name() {
        let w = StatementWhitelist::load_from_str(
            r#"{"f":{"sql":"SELECT * FROM t WHERE a = ? AND b = ?","param_names":["b","a"]}}"#,
        )
        .unwrap();
        let db_path =
            std::env::temp_dir().join(format!("evorule_wl_obj_{}.sqlite", std::process::id()));
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let db = DbHandler::connect_file(&db_path).await.unwrap();
            let wl = WhitelistedDbHandler::new(db, w);
            // 传对象，b=22, a=11 — 按 param_names 顺序 [b, a] 得到 [22, 11]
            let params = JsonValue::object_from_pairs(&[
                ("name", JsonValue::string("f")),
                (
                    "params",
                    JsonValue::object_from_pairs(&[
                        ("a", JsonValue::Integer(11)),
                        ("b", JsonValue::Integer(22)),
                    ]),
                ),
            ]);
            let resolved = wl.resolve(&params).unwrap();
            let arr = resolved.get("params").and_then(|v| v.as_array()).unwrap();
            assert_eq!(arr[0].as_i64(), Some(22)); // b
            assert_eq!(arr[1].as_i64(), Some(11)); // a
        });
    }

    #[test]
    fn test_resolve_rejects_direct_query_field() {
        let w = StatementWhitelist::load_from_str(r#"{"f":{"sql":"SELECT 1"}}"#).unwrap();
        let db_path =
            std::env::temp_dir().join(format!("evorule_wl_bypass_{}.sqlite", std::process::id()));
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let db = DbHandler::connect_file(&db_path).await.unwrap();
            let wl = WhitelistedDbHandler::new(db, w);
            let params = JsonValue::object_from_pairs(&[
                ("name", JsonValue::string("f")),
                ("query", JsonValue::string("DROP TABLE t")),
            ]);
            let err = wl.execute(&params).await.unwrap_err();
            assert!(err.contains("direct 'query' field is forbidden"));
        });
    }

    #[test]
    fn test_has_multiple_statements_detected() {
        assert!(has_multiple_statements("SELECT 1; SELECT 2"));
        assert!(has_multiple_statements(
            "INSERT INTO t(a) VALUES (1); DROP TABLE t"
        ));
        // 注释+空白在分号后的也算多语句
        assert!(has_multiple_statements("SELECT 1; -- trailing\nSELECT 2"));
    }

    #[test]
    fn test_has_multiple_statements_semicolon_in_string_ignored() {
        // 分号在字符串字面量里不算多语句
        assert!(!has_multiple_statements("SELECT 'a;b' AS x"));
        assert!(!has_multiple_statements("INSERT INTO t VALUES (\";\")"));
    }

    #[test]
    fn test_has_multiple_statements_single_trailing_semicolon_allowed() {
        // 末尾只有一个 ;，后面全空白/注释 → 不算多语句
        assert!(!has_multiple_statements("SELECT 1;"));
        assert!(!has_multiple_statements("SELECT 1; -- done"));
        assert!(!has_multiple_statements("SELECT 1\n;\n"));
    }

    #[test]
    fn test_multiple_statements_blocked_on_load() {
        let err = StatementWhitelist::load_from_str(r#"{"bad":{"sql":"SELECT 1; DROP TABLE t"}}"#)
            .unwrap_err();
        assert!(err.contains("multiple SQL statements"));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used, clippy::panic)]
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// 生成唯一的临时数据库文件路径（位于系统临时目录，测试残留可接受）。
    static DB_COUNTER: AtomicU64 = AtomicU64::new(0);
    fn temp_db_path() -> PathBuf {
        let n = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "evorule_io_test_{}_{}.sqlite",
            std::process::id(),
            n
        ))
    }

    /// 构造 IoHandler params 对象的辅助函数
    fn params(pairs: &[(&str, JsonValue)]) -> JsonValue {
        JsonValue::object_from_pairs(pairs)
    }

    #[test]
    fn test_is_query_statement_select() {
        assert!(is_query_statement("SELECT * FROM t"));
        assert!(is_query_statement("  select 1"));
        assert!(is_query_statement("SELECT * FROM t WHERE x = 'RETURNING'"));
    }

    #[test]
    fn test_is_query_statement_cte_and_returning() {
        // CTE：旧实现误判为非查询
        assert!(is_query_statement(
            "WITH cte AS (SELECT 1) SELECT * FROM cte"
        ));
        // RETURNING：旧实现误判为非查询
        assert!(is_query_statement(
            "INSERT INTO t(a) VALUES (1) RETURNING a"
        ));
        assert!(is_query_statement("DELETE FROM t WHERE a = 1 RETURNING a"));
    }

    #[test]
    fn test_is_query_statement_other_query_kinds() {
        assert!(is_query_statement("VALUES (1), (2)"));
        assert!(is_query_statement("PRAGMA table_info(t)"));
        assert!(is_query_statement("EXPLAIN SELECT * FROM t"));
    }

    #[test]
    fn test_is_query_statement_leading_comments() {
        // 注释开头：旧实现误判
        assert!(is_query_statement("-- note\nSELECT 1"));
        assert!(is_query_statement("/* block */ SELECT 1"));
        assert!(is_query_statement(
            "/* a */ /* b */\n-- c\nWITH x AS (SELECT 1) SELECT * FROM x"
        ));
    }

    #[test]
    fn test_is_query_statement_non_query() {
        assert!(!is_query_statement("INSERT INTO t(a) VALUES (1)"));
        assert!(!is_query_statement("UPDATE t SET a = 1"));
        assert!(!is_query_statement("DELETE FROM t"));
        assert!(!is_query_statement("CREATE TABLE t(a INTEGER)"));
        assert!(!is_query_statement("DROP TABLE t"));
        // RETURNING 作为字符串字面量时仍会命中（已知限制，列名/字面量
        // 含 RETURNING 极罕见，fetch_all 对非查询语句返回空也不致错）。
    }

    #[test]
    fn test_strip_leading_sql_comments() {
        assert_eq!(strip_leading_sql_comments("SELECT 1"), "SELECT 1");
        assert_eq!(strip_leading_sql_comments("  \n SELECT 1"), "SELECT 1");
        assert_eq!(strip_leading_sql_comments("-- c\nSELECT 1"), "SELECT 1");
        assert_eq!(strip_leading_sql_comments("/* c */SELECT 1"), "SELECT 1");
        // 未闭合块注释
        assert_eq!(strip_leading_sql_comments("/* unclosed"), "");
        // 全注释
        assert_eq!(strip_leading_sql_comments("-- only comment"), "");
    }

    // ===== DbHandler::execute 集成测试（SQLite 临时文件）=====

    #[tokio::test]
    async fn test_db_create_insert_select_roundtrip() {
        let path = temp_db_path();
        let handler = DbHandler::connect_file(&path).await.unwrap();

        // 建表（非查询，返回 rows_affected = 0）
        let r = handler
            .execute(&params(&[(
                "query",
                JsonValue::string("CREATE TABLE t(a INTEGER, b TEXT)"),
            )]))
            .await
            .unwrap();
        assert_eq!(r.as_i64(), Some(0));

        // 插入
        let r = handler
            .execute(&params(&[(
                "query",
                JsonValue::string("INSERT INTO t(a, b) VALUES (1, 'hello')"),
            )]))
            .await
            .unwrap();
        assert_eq!(r.as_i64(), Some(1));

        // 查询
        let r = handler
            .execute(&params(&[(
                "query",
                JsonValue::string("SELECT a, b FROM t"),
            )]))
            .await
            .unwrap();
        let arr = r.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].get("a").and_then(|v| v.as_i64()), Some(1));
        assert_eq!(arr[0].get("b").and_then(|v| v.as_str()), Some("hello"));
    }

    #[tokio::test]
    async fn test_db_select_with_params_binding() {
        let path = temp_db_path();
        let handler = DbHandler::connect_file(&path).await.unwrap();
        handler
            .execute(&params(&[(
                "query",
                JsonValue::string("CREATE TABLE t(a INTEGER)"),
            )]))
            .await
            .unwrap();
        for i in 1..=3 {
            handler
                .execute(&params(&[
                    ("query", JsonValue::string("INSERT INTO t(a) VALUES (?)")),
                    ("params", JsonValue::array(vec![JsonValue::Integer(i)])),
                ]))
                .await
                .unwrap();
        }
        // 参数绑定查询 a > 1
        let r = handler
            .execute(&params(&[
                (
                    "query",
                    JsonValue::string("SELECT a FROM t WHERE a > ? ORDER BY a"),
                ),
                ("params", JsonValue::array(vec![JsonValue::Integer(1)])),
            ]))
            .await
            .unwrap();
        let arr = r.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0].get("a").and_then(|v| v.as_i64()), Some(2));
        assert_eq!(arr[1].get("a").and_then(|v| v.as_i64()), Some(3));
    }

    #[tokio::test]
    async fn test_db_cte_query_returns_rows() {
        // regression for Bug-1: WITH ... SELECT 旧实现误判为非查询，丢失结果
        let path = temp_db_path();
        let handler = DbHandler::connect_file(&path).await.unwrap();
        let r = handler
            .execute(&params(&[(
                "query",
                JsonValue::string("WITH cte AS (SELECT 1 AS x) SELECT x FROM cte"),
            )]))
            .await
            .unwrap();
        let arr = r.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].get("x").and_then(|v| v.as_i64()), Some(1));
    }

    #[tokio::test]
    async fn test_db_returning_clause_returns_rows() {
        // regression for Bug-1: INSERT ... RETURNING 旧实现误判为非查询，丢失结果
        let path = temp_db_path();
        let handler = DbHandler::connect_file(&path).await.unwrap();
        handler
            .execute(&params(&[(
                "query",
                JsonValue::string("CREATE TABLE t(id INTEGER PRIMARY KEY, a TEXT)"),
            )]))
            .await
            .unwrap();
        let r = handler
            .execute(&params(&[(
                "query",
                JsonValue::string("INSERT INTO t(a) VALUES ('hi') RETURNING id, a"),
            )]))
            .await
            .unwrap();
        let arr = r.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].get("id").and_then(|v| v.as_i64()), Some(1));
        assert_eq!(arr[0].get("a").and_then(|v| v.as_str()), Some("hi"));
    }

    #[tokio::test]
    async fn test_db_leading_comment_select() {
        // regression for Bug-1: 注释开头 旧实现 trim_start() 无法跳过注释，误判
        let path = temp_db_path();
        let handler = DbHandler::connect_file(&path).await.unwrap();
        let r = handler
            .execute(&params(&[(
                "query",
                JsonValue::string("-- this is a comment\nSELECT 42 AS answer"),
            )]))
            .await
            .unwrap();
        let arr = r.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].get("answer").and_then(|v| v.as_i64()), Some(42));
    }

    #[tokio::test]
    async fn test_db_non_query_returns_rows_affected() {
        let path = temp_db_path();
        let handler = DbHandler::connect_file(&path).await.unwrap();
        handler
            .execute(&params(&[(
                "query",
                JsonValue::string("CREATE TABLE t(a INTEGER)"),
            )]))
            .await
            .unwrap();
        handler
            .execute(&params(&[(
                "query",
                JsonValue::string("INSERT INTO t(a) VALUES (10), (20), (30)"),
            )]))
            .await
            .unwrap();
        let r = handler
            .execute(&params(&[(
                "query",
                JsonValue::string("UPDATE t SET a = a + 1"),
            )]))
            .await
            .unwrap();
        assert_eq!(r.as_i64(), Some(3)); // 3 行受影响
    }

    #[tokio::test]
    async fn test_db_missing_query_param() {
        let path = temp_db_path();
        let handler = DbHandler::connect_file(&path).await.unwrap();
        let r = handler
            .execute(&params(&[("not_query", JsonValue::string("SELECT 1"))]))
            .await;
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("missing required param: query"));
    }

    #[tokio::test]
    async fn test_db_sql_syntax_error() {
        let path = temp_db_path();
        let handler = DbHandler::connect_file(&path).await.unwrap();
        let r = handler
            .execute(&params(&[(
                "query",
                JsonValue::string("SELECT FROM nonexistent_table"),
            )]))
            .await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn test_db_connect_invalid_url_returns_error() {
        // regression for Bug-2: 旧实现用 unwrap_or_else(|_| SqliteConnectOptions::new())
        // 在 URL 解析失败时静默回退到 sqlite::memory:。
        // "sqlite://x?mode=invalid" 使 from_str 返回 Err（unknown value for `mode`），
        // 旧实现会回退到内存库（数据丢失风险），新实现必须返回错误。
        let r = DbHandler::connect("sqlite://x?mode=invalid").await;
        assert!(
            r.is_err(),
            "invalid mode param must return error, not fallback to memory db"
        );
    }

    #[tokio::test]
    async fn test_db_connect_memory_url_succeeds() {
        // 正向测试：合法的内存库 URL 应该成功（确认 Bug-2 修复未误伤合法 URL）
        let r = DbHandler::connect("sqlite::memory:").await;
        assert!(r.is_ok(), "sqlite::memory: should connect successfully");
    }
}

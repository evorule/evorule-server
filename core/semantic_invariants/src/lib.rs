// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! EvoRule 语义不变式引擎 —— JSON 驱动的业务约束检查
//!
//! 从 evorule-reactor 移出的 semantic_invariants 在应用层实现。
//! tier1 保留结构性不变式（物理一致性），语义不变式（业务逻辑）
//! 在本模块独立运行，不侵入核心。
//!
//! # 设计
//!
//! - 接收 JSON 格式的语义约束规则
//! - 对当前会话状态检查业务约束（通过 evorule-server `/state` 端点获取 live payload）
//! - 违规时记录并可通过 `GET /violations` 查询
//! - `GET /api/stats` 提供全局违规聚合（供 metrics 服务拉取）
//!
//! # 约束规则格式
//!
//! ```json
//! {
//!   "invariants": [
//!     {
//!       "name": "amount_non_negative",
//!       "description": "金额不能为负",
//!       "path": "payload.amount",
//!       "operator": "gte",
//!       "value": 0,
//!       "severity": "critical"
//!     }
//!   ]
//! }
//! ```
//!
//! # 支持的操作符
//!
//! - `eq`：等于
//! - `ne`：不等于
//! - `gt`：大于
//! - `lt`：小于
//! - `gte`：大于等于
//! - `lte`：小于等于
//! - `exists`：字段存在
//! - `not_exists`：字段不存在
//! - `contains`：包含（字符串/数组）
//! - `ne_path`：不等于另一个路径的值
//!
//! # 端点
//!
//! - `POST   /api/sessions/{id}/invariants` — 添加约束规则
//! - `GET    /api/sessions/{id}/invariants` — 列出约束规则
//! - `DELETE /api/sessions/{id}/invariants/{name}` — 删除约束规则
//! - `GET    /api/sessions/{id}/violations` — 获取违规记录
//! - `POST   /api/sessions/{id}/check` — 立即检查当前状态
//! - `GET    /api/stats` — 全局违规统计（供 metrics 服务拉取）
//!
//! # 设计说明
//!
//! - **live payload**：`fetch_session_state` 使用 `/state` 端点返回的 live payload
//!   （含 pending PayloadUpdate），而非 rewind 快照（会遗漏待处理变更）。
//! - **mutex 毒化降级**：`rules`/`violations` 用 `std::sync::Mutex` 保护，
//!   毒化时不 panic，降级访问受污染数据并记录 error 日志。
//! - **未实现**：`watch` SSE 违规告警流端点尚未实现，未来增强项。

#![forbid(unsafe_code)]
// C5 (unwrap/expect/panic = deny) 仅约束生产代码;测试代码保留 unwrap 惯例
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use axum::{
    extract::Path,
    extract::State,
    http::StatusCode,
    response::Json,
    routing::{delete, get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use tracing::info;

/// 约束严重级别
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Critical,
    High,
    Medium,
    Low,
}

/// 比较操作符
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operator {
    Eq,
    Ne,
    Gt,
    Lt,
    Gte,
    Lte,
    Exists,
    NotExists,
    Contains,
    /// 不等于另一个路径的值
    NePath,
}

/// 语义约束规则
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvariantRule {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// JSON 路径（如 `payload.amount`）
    pub path: String,
    pub operator: Operator,
    /// 比较值（exists/not_exists 不需要）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
    /// 用于 ne_path 操作符的第二个路径
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path2: Option<String>,
    #[serde(default = "default_severity")]
    pub severity: Severity,
}

fn default_severity() -> Severity {
    Severity::High
}

/// 约束规则集合
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvariantSet {
    pub invariants: Vec<InvariantRule>,
}

/// 违规记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Violation {
    pub rule_name: String,
    pub description: String,
    pub severity: Severity,
    pub session_id: u64,
    pub version: u64,
    pub path: String,
    pub actual_value: serde_json::Value,
    pub expected_value: Option<serde_json::Value>,
    pub timestamp: String,
}

/// 检查结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    pub session_id: u64,
    pub version: u64,
    pub checked_rules: usize,
    pub violations: Vec<Violation>,
    pub passed: bool,
}

/// 违规统计（供 metrics 服务拉取）
///
/// 聚合所有会话的约束规则与违规记录，输出可用于 Prometheus 监控的汇总数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViolationStats {
    /// 已注册的约束规则总数（所有会话合计）
    pub total_rules: usize,
    /// 累计违规总数（所有会话合计）
    pub total_violations: usize,
    /// 按严重级别分组的违规数（key: critical/high/medium/low）
    pub violations_by_severity: BTreeMap<String, usize>,
    /// 有违规记录的会话数
    pub sessions_with_violations: usize,
    /// 被监控的会话数（已注册规则的会话数）
    pub monitored_sessions: usize,
}

/// 语义不变式服务
#[derive(Clone)]
pub struct SemanticInvariantService {
    evorule_server_url: Arc<String>,
    client: reqwest::Client,
    /// 会话约束规则（session_id -> Vec<InvariantRule>）
    rules: Arc<Mutex<HashMap<u64, Vec<InvariantRule>>>>,
    /// 违规记录（session_id -> Vec<Violation>）
    violations: Arc<Mutex<HashMap<u64, Vec<Violation>>>>,
}

impl SemanticInvariantService {
    pub fn new(evorule_server_url: String) -> Self {
        Self {
            evorule_server_url: Arc::new(evorule_server_url),
            client: reqwest::Client::new(),
            rules: Arc::new(Mutex::new(HashMap::new())),
            violations: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 添加约束规则
    pub fn add_rules(&self, session_id: u64, rule_set: InvariantSet) -> usize {
        let mut rules = self.lock_rules();
        let entry = rules.entry(session_id).or_default();
        let count = rule_set.invariants.len();
        entry.extend(rule_set.invariants);
        count
    }

    /// 获取约束规则
    pub fn get_rules(&self, session_id: u64) -> Vec<InvariantRule> {
        self.lock_rules()
            .get(&session_id)
            .cloned()
            .unwrap_or_default()
    }

    /// 删除约束规则
    pub fn remove_rule(&self, session_id: u64, name: &str) -> bool {
        let mut rules = self.lock_rules();
        if let Some(entry) = rules.get_mut(&session_id) {
            let before = entry.len();
            entry.retain(|r| r.name != name);
            return entry.len() < before;
        }
        false
    }

    /// 获取违规记录
    pub fn get_violations(&self, session_id: u64) -> Vec<Violation> {
        self.lock_violations()
            .get(&session_id)
            .cloned()
            .unwrap_or_default()
    }

    /// 获取全局违规统计（供 metrics 服务拉取）
    ///
    /// 聚合所有会话的约束规则与违规记录，返回可序列化的统计快照。
    /// metrics 服务通过 `GET /api/stats` 拉取此数据并转为 Prometheus 指标。
    pub fn get_stats(&self) -> ViolationStats {
        let rules = self.lock_rules();
        let violations = self.lock_violations();

        let total_rules: usize = rules.values().map(|v| v.len()).sum();
        let monitored_sessions = rules.len();

        // 初始化四个严重级别计数器
        let mut violations_by_severity = BTreeMap::new();
        violations_by_severity.insert("critical".to_string(), 0usize);
        violations_by_severity.insert("high".to_string(), 0);
        violations_by_severity.insert("medium".to_string(), 0);
        violations_by_severity.insert("low".to_string(), 0);

        let mut total_violations = 0usize;
        let mut sessions_with_violations = 0usize;

        for session_violations in violations.values() {
            if !session_violations.is_empty() {
                sessions_with_violations += 1;
            }
            for v in session_violations {
                total_violations += 1;
                let severity_str = match v.severity {
                    Severity::Critical => "critical",
                    Severity::High => "high",
                    Severity::Medium => "medium",
                    Severity::Low => "low",
                };
                *violations_by_severity
                    .entry(severity_str.to_string())
                    .or_insert(0) += 1;
            }
        }

        ViolationStats {
            total_rules,
            total_violations,
            violations_by_severity,
            sessions_with_violations,
            monitored_sessions,
        }
    }

    /// 获取会话当前状态（从 evorule-server）
    ///
    /// 使用 state 端点返回的 live payload（包含尚未触发 StateTransition 的
    /// pending PayloadUpdate），而非 rewind 快照（rewind 只反映已落盘的
    /// StateTransition，会遗漏待处理的 payload 变更）。
    async fn fetch_session_state(
        &self,
        session_id: u64,
    ) -> Result<(u64, serde_json::Value), String> {
        let url = format!(
            "{}/api/sessions/{}/state",
            self.evorule_server_url, session_id
        );
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("请求 state 失败: {}", e))?;
        if !resp.status().is_success() {
            return Err(format!("state 端点返回 {}", resp.status()));
        }
        let state: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("解析 state 失败: {}", e))?;

        let version = state.get("version").and_then(|v| v.as_u64()).unwrap_or(0);

        // state 端点直接返回 live payload（含 pending PayloadUpdate）
        let payload = state
            .get("payload")
            .cloned()
            .unwrap_or(serde_json::json!({}));

        let full_state = serde_json::json!({
            "version": version,
            "payload": payload,
            "phase": state.get("reactor").and_then(|r| r.get("phase")).cloned(),
        });

        Ok((version, full_state))
    }

    /// 检查单个约束规则
    fn check_rule(rule: &InvariantRule, state: &serde_json::Value) -> Option<Violation> {
        let value_at_path = get_path(state, &rule.path);
        let version = state.get("version").and_then(|v| v.as_u64()).unwrap_or(0);
        let timestamp = chrono_now();

        let violated = match &rule.operator {
            Operator::Exists => value_at_path.is_none(),
            Operator::NotExists => value_at_path.is_some(),
            Operator::Eq => {
                value_at_path.as_ref()
                    != Some(&rule.value.clone().unwrap_or(serde_json::Value::Null))
            }
            Operator::Ne => {
                value_at_path.as_ref()
                    == Some(&rule.value.clone().unwrap_or(serde_json::Value::Null))
            }
            // 比较类操作符: violated = 约束不成立
            // Gte(>=) 违规当 actual < expected; Gt(>) 违规当 actual <= expected
            // Lte(<=) 违规当 actual > expected; Lt(<) 违规当 actual >= expected
            // 当值不存在或非数字时,无法判定,视为未违规(返回 false)
            Operator::Gt => compare_numbers(&value_at_path, &rule.value, |a, b| a <= b),
            Operator::Lt => compare_numbers(&value_at_path, &rule.value, |a, b| a >= b),
            Operator::Gte => compare_numbers(&value_at_path, &rule.value, |a, b| a < b),
            Operator::Lte => compare_numbers(&value_at_path, &rule.value, |a, b| a > b),
            Operator::Contains => {
                let val = value_at_path.as_ref();
                let target = rule.value.as_ref();
                match (val, target) {
                    (Some(serde_json::Value::String(s)), Some(serde_json::Value::String(t))) => {
                        !s.contains(t)
                    }
                    (Some(serde_json::Value::Array(arr)), Some(t)) => !arr.contains(t),
                    _ => true,
                }
            }
            Operator::NePath => {
                let val_a = get_path(state, &rule.path);
                let val_b = rule.path2.as_ref().and_then(|p| get_path(state, p));
                val_a == val_b
            }
        };

        if violated {
            Some(Violation {
                rule_name: rule.name.clone(),
                description: rule.description.clone(),
                severity: rule.severity.clone(),
                session_id: 0, // 由调用者填充
                version,
                path: rule.path.clone(),
                actual_value: value_at_path.unwrap_or(serde_json::Value::Null),
                expected_value: rule.value.clone(),
                timestamp,
            })
        } else {
            None
        }
    }

    /// 检查会话的当前状态
    pub async fn check_session(&self, session_id: u64) -> Result<CheckResult, String> {
        let (version, state) = self.fetch_session_state(session_id).await?;

        let rules = self.get_rules(session_id);
        let mut violations = Vec::new();

        for rule in &rules {
            if let Some(mut v) = Self::check_rule(rule, &state) {
                v.session_id = session_id;
                violations.push(v);
            }
        }

        let passed = violations.is_empty();
        let checked = rules.len();

        // 记录违规
        if !violations.is_empty() {
            let mut store = self.lock_violations();
            store
                .entry(session_id)
                .or_default()
                .extend(violations.clone());
        }

        Ok(CheckResult {
            session_id,
            version,
            checked_rules: checked,
            violations,
            passed,
        })
    }

    /// 锁定 rules map，毒化时取回内部数据继续访问（不 panic）
    fn lock_rules(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Vec<InvariantRule>>> {
        match self.rules.lock() {
            Ok(g) => g,
            Err(p) => {
                tracing::error!("rules mutex 毒化,降级访问受污染数据");
                p.into_inner()
            }
        }
    }

    /// 锁定 violations map，毒化时取回内部数据继续访问（不 panic）
    fn lock_violations(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Vec<Violation>>> {
        match self.violations.lock() {
            Ok(g) => g,
            Err(p) => {
                tracing::error!("violations mutex 毒化,降级访问受污染数据");
                p.into_inner()
            }
        }
    }
}

/// 从 JSON 中按路径取值（如 `payload.amount`）
fn get_path(state: &serde_json::Value, path: &str) -> Option<serde_json::Value> {
    let mut current = state;
    for part in path.split('.') {
        current = current.get(part)?;
    }
    Some(current.clone())
}

/// 比较两个数字
fn compare_numbers(
    actual: &Option<serde_json::Value>,
    expected: &Option<serde_json::Value>,
    cmp: impl Fn(f64, f64) -> bool,
) -> bool {
    let a = actual.as_ref().and_then(|v| v.as_f64());
    let b = expected.as_ref().and_then(|v| v.as_f64());
    match (a, b) {
        (Some(a), Some(b)) => cmp(a, b),
        _ => false,
    }
}

/// 获取当前时间戳（简单实现，不依赖 chrono）
fn chrono_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("epoch:{}", secs)
}

// ============ HTTP API ============

async fn add_rules_handler(
    State(svc): State<SemanticInvariantService>,
    Path(session_id): Path<u64>,
    Json(rule_set): Json<InvariantSet>,
) -> Json<serde_json::Value> {
    let count = svc.add_rules(session_id, rule_set);
    Json(serde_json::json!({
        "session_id": session_id,
        "added": count,
        "message": format!("已添加 {} 条约束规则", count),
    }))
}

async fn get_rules_handler(
    State(svc): State<SemanticInvariantService>,
    Path(session_id): Path<u64>,
) -> Json<serde_json::Value> {
    let rules = svc.get_rules(session_id);
    Json(serde_json::json!({
        "session_id": session_id,
        "count": rules.len(),
        "invariants": rules,
    }))
}

async fn delete_rule_handler(
    State(svc): State<SemanticInvariantService>,
    Path((session_id, name)): Path<(u64, String)>,
) -> Json<serde_json::Value> {
    let deleted = svc.remove_rule(session_id, &name);
    Json(serde_json::json!({
        "session_id": session_id,
        "rule_name": name,
        "deleted": deleted,
    }))
}

async fn get_violations_handler(
    State(svc): State<SemanticInvariantService>,
    Path(session_id): Path<u64>,
) -> Json<serde_json::Value> {
    let violations = svc.get_violations(session_id);
    Json(serde_json::json!({
        "session_id": session_id,
        "count": violations.len(),
        "violations": violations,
    }))
}

/// handler 错误类型：状态码 + 错误消息，避免 `String: IntoResponse` 默认 200。
type HandlerError = (StatusCode, String);

/// 把服务层 `Err(String)` 映射为 `500 Internal Server Error`
fn map_err(e: String) -> HandlerError {
    (StatusCode::INTERNAL_SERVER_ERROR, e)
}

async fn check_handler(
    State(svc): State<SemanticInvariantService>,
    Path(session_id): Path<u64>,
) -> Result<Json<CheckResult>, HandlerError> {
    let result = svc.check_session(session_id).await.map_err(map_err)?;
    Ok(Json(result))
}

/// 全局违规统计端点（供 metrics 服务拉取）
async fn stats_handler(State(svc): State<SemanticInvariantService>) -> Json<ViolationStats> {
    Json(svc.get_stats())
}

/// 构建路由
pub fn build_router(service: SemanticInvariantService) -> Router {
    Router::new()
        .route(
            "/api/sessions/{id}/invariants",
            post(add_rules_handler).get(get_rules_handler),
        )
        .route(
            "/api/sessions/{id}/invariants/{name}",
            delete(delete_rule_handler),
        )
        .route("/api/sessions/{id}/violations", get(get_violations_handler))
        .route("/api/sessions/{id}/check", post(check_handler))
        .route("/api/stats", get(stats_handler))
        .with_state(service)
}

/// 启动 HTTP API 服务器
pub async fn run_server(service: SemanticInvariantService, addr: &str) -> Result<(), String> {
    let app = build_router(service);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("绑定地址 {} 失败: {}", addr, e))?;
    info!("语义不变式服务已启动 addr={}", addr);
    info!("端点:");
    const ENDPOINTS: &[&str] = &[
        "POST   /api/sessions/{id}/invariants  (添加约束规则)",
        "GET    /api/sessions/{id}/invariants   (列出约束规则)",
        "DELETE /api/sessions/{id}/invariants/{name}",
        "GET    /api/sessions/{id}/violations    (获取违规记录)",
        "POST   /api/sessions/{id}/check         (立即检查)",
        "GET    /api/stats                         (全局违规统计,供 metrics 拉取)",
    ];
    for ep in ENDPOINTS {
        info!("  {ep}");
    }
    axum::serve(listener, app)
        .await
        .map_err(|e| format!("服务器错误: {}", e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    // ============ 辅助构造 ============

    /// 构造测试用 state JSON
    fn sample_state() -> serde_json::Value {
        serde_json::json!({
            "version": 5,
            "payload": {
                "amount": 100,
                "name": "hello world",
                "tags": ["a", "b", "c"],
                "active": true
            }
        })
    }

    /// 构造一条规则
    fn rule(name: &str, path: &str, op: Operator) -> InvariantRule {
        InvariantRule {
            name: name.to_string(),
            description: format!("测试规则 {name}"),
            path: path.to_string(),
            operator: op,
            value: None,
            path2: None,
            severity: Severity::High,
        }
    }

    /// 构造一条带 value 的规则
    fn rule_with_value(
        name: &str,
        path: &str,
        op: Operator,
        value: serde_json::Value,
    ) -> InvariantRule {
        let mut r = rule(name, path, op);
        r.value = Some(value);
        r
    }

    /// 指向无效地址的 service（纯状态测试不走网络）
    fn new_service() -> SemanticInvariantService {
        SemanticInvariantService::new("http://127.0.0.1:1".to_string())
    }

    /// 构造 evorule-server /state 端点的 mock 响应体
    fn state_response_body() -> String {
        serde_json::json!({
            "version": 5,
            "payload": {"amount": 100, "name": "hello world", "tags": ["a", "b", "c"]},
            "reactor": {"phase": "running"}
        })
        .to_string()
    }

    // ============ get_path 纯函数测试 ============

    #[test]
    fn test_get_path_nested() {
        let state = sample_state();
        assert_eq!(
            get_path(&state, "payload.amount"),
            Some(serde_json::json!(100))
        );
    }

    #[test]
    fn test_get_path_missing_intermediate() {
        let state = sample_state();
        assert_eq!(get_path(&state, "nonexistent.amount"), None);
    }

    #[test]
    fn test_get_path_missing_final() {
        let state = sample_state();
        assert_eq!(get_path(&state, "payload.nonexistent"), None);
    }

    #[test]
    fn test_get_path_root_level() {
        let state = sample_state();
        assert_eq!(get_path(&state, "version"), Some(serde_json::json!(5)));
    }

    // ============ compare_numbers 纯函数测试 ============

    #[test]
    fn test_compare_numbers_both_present() {
        let actual = Some(serde_json::json!(10));
        let expected = Some(serde_json::json!(20));
        // 10 < 20 → true
        assert!(compare_numbers(&actual, &expected, |a, b| a < b));
        // 10 > 20 → false
        assert!(!compare_numbers(&actual, &expected, |a, b| a > b));
    }

    #[test]
    fn test_compare_numbers_actual_missing() {
        let expected = Some(serde_json::json!(20));
        // actual 缺失 → false（无法判定）
        assert!(!compare_numbers(&None, &expected, |a, b| a < b));
    }

    #[test]
    fn test_compare_numbers_non_numeric() {
        let actual = Some(serde_json::json!("not a number"));
        let expected = Some(serde_json::json!(20));
        assert!(!compare_numbers(&actual, &expected, |a, b| a < b));
    }

    // ============ check_rule 各操作符测试 ============

    #[test]
    fn test_check_rule_exists_violated_when_missing() {
        let r = rule("exists_check", "payload.nonexistent", Operator::Exists);
        let state = sample_state();
        let v = SemanticInvariantService::check_rule(&r, &state);
        assert!(v.is_some(), "路径不存在 → Exists 违规");
    }

    #[test]
    fn test_check_rule_exists_not_violated_when_present() {
        let r = rule("exists_check", "payload.amount", Operator::Exists);
        let state = sample_state();
        let v = SemanticInvariantService::check_rule(&r, &state);
        assert!(v.is_none(), "路径存在 → Exists 未违规");
    }

    #[test]
    fn test_check_rule_not_exists_violated_when_present() {
        let r = rule("not_exists_check", "payload.amount", Operator::NotExists);
        let state = sample_state();
        let v = SemanticInvariantService::check_rule(&r, &state);
        assert!(v.is_some(), "路径存在 → NotExists 违规");
    }

    #[test]
    fn test_check_rule_not_exists_not_violated_when_missing() {
        let r = rule(
            "not_exists_check",
            "payload.nonexistent",
            Operator::NotExists,
        );
        let state = sample_state();
        let v = SemanticInvariantService::check_rule(&r, &state);
        assert!(v.is_none(), "路径不存在 → NotExists 未违规");
    }

    #[test]
    fn test_check_rule_eq_not_violated_when_equal() {
        let r = rule_with_value(
            "eq_check",
            "payload.amount",
            Operator::Eq,
            serde_json::json!(100),
        );
        let state = sample_state();
        let v = SemanticInvariantService::check_rule(&r, &state);
        assert!(v.is_none(), "值相等 → Eq 未违规");
    }

    #[test]
    fn test_check_rule_eq_violated_when_different() {
        let r = rule_with_value(
            "eq_check",
            "payload.amount",
            Operator::Eq,
            serde_json::json!(200),
        );
        let state = sample_state();
        let v = SemanticInvariantService::check_rule(&r, &state);
        assert!(v.is_some(), "值不等 → Eq 违规");
    }

    #[test]
    fn test_check_rule_ne_violated_when_equal() {
        let r = rule_with_value(
            "ne_check",
            "payload.amount",
            Operator::Ne,
            serde_json::json!(100),
        );
        let state = sample_state();
        let v = SemanticInvariantService::check_rule(&r, &state);
        assert!(v.is_some(), "值相等 → Ne 违规");
    }

    #[test]
    fn test_check_rule_ne_not_violated_when_different() {
        let r = rule_with_value(
            "ne_check",
            "payload.amount",
            Operator::Ne,
            serde_json::json!(200),
        );
        let state = sample_state();
        let v = SemanticInvariantService::check_rule(&r, &state);
        assert!(v.is_none(), "值不等 → Ne 未违规");
    }

    #[test]
    fn test_check_rule_gt_not_violated_when_greater() {
        // actual=100 > expected=50 → 约束成立 → 未违规
        let r = rule_with_value(
            "gt_check",
            "payload.amount",
            Operator::Gt,
            serde_json::json!(50),
        );
        let state = sample_state();
        let v = SemanticInvariantService::check_rule(&r, &state);
        assert!(v.is_none(), "100 > 50 → Gt 未违规");
    }

    #[test]
    fn test_check_rule_gt_violated_when_equal() {
        // actual=100, expected=100, 100 <= 100 → 违规
        let r = rule_with_value(
            "gt_check",
            "payload.amount",
            Operator::Gt,
            serde_json::json!(100),
        );
        let state = sample_state();
        let v = SemanticInvariantService::check_rule(&r, &state);
        assert!(v.is_some(), "100 == 100 → Gt 违规（不大于）");
    }

    #[test]
    fn test_check_rule_lt_not_violated_when_less() {
        // actual=100 < expected=200 → 约束成立 → 未违规
        let r = rule_with_value(
            "lt_check",
            "payload.amount",
            Operator::Lt,
            serde_json::json!(200),
        );
        let state = sample_state();
        let v = SemanticInvariantService::check_rule(&r, &state);
        assert!(v.is_none(), "100 < 200 → Lt 未违规");
    }

    #[test]
    fn test_check_rule_lt_violated_when_equal() {
        let r = rule_with_value(
            "lt_check",
            "payload.amount",
            Operator::Lt,
            serde_json::json!(100),
        );
        let state = sample_state();
        let v = SemanticInvariantService::check_rule(&r, &state);
        assert!(v.is_some(), "100 == 100 → Lt 违规（不小于）");
    }

    #[test]
    fn test_check_rule_gte_not_violated_when_equal() {
        let r = rule_with_value(
            "gte_check",
            "payload.amount",
            Operator::Gte,
            serde_json::json!(100),
        );
        let state = sample_state();
        let v = SemanticInvariantService::check_rule(&r, &state);
        assert!(v.is_none(), "100 >= 100 → Gte 未违规");
    }

    #[test]
    fn test_check_rule_gte_violated_when_less() {
        let r = rule_with_value(
            "gte_check",
            "payload.amount",
            Operator::Gte,
            serde_json::json!(200),
        );
        let state = sample_state();
        let v = SemanticInvariantService::check_rule(&r, &state);
        assert!(v.is_some(), "100 < 200 → Gte 违规");
    }

    #[test]
    fn test_check_rule_lte_not_violated_when_equal() {
        let r = rule_with_value(
            "lte_check",
            "payload.amount",
            Operator::Lte,
            serde_json::json!(100),
        );
        let state = sample_state();
        let v = SemanticInvariantService::check_rule(&r, &state);
        assert!(v.is_none(), "100 <= 100 → Lte 未违规");
    }

    #[test]
    fn test_check_rule_lte_violated_when_greater() {
        let r = rule_with_value(
            "lte_check",
            "payload.amount",
            Operator::Lte,
            serde_json::json!(50),
        );
        let state = sample_state();
        let v = SemanticInvariantService::check_rule(&r, &state);
        assert!(v.is_some(), "100 > 50 → Lte 违规");
    }

    #[test]
    fn test_check_rule_contains_string_not_violated() {
        let r = rule_with_value(
            "contains_str",
            "payload.name",
            Operator::Contains,
            serde_json::json!("hello"),
        );
        let state = sample_state();
        let v = SemanticInvariantService::check_rule(&r, &state);
        assert!(v.is_none(), "\"hello world\" 包含 \"hello\" → 未违规");
    }

    #[test]
    fn test_check_rule_contains_string_violated() {
        let r = rule_with_value(
            "contains_str",
            "payload.name",
            Operator::Contains,
            serde_json::json!("xyz"),
        );
        let state = sample_state();
        let v = SemanticInvariantService::check_rule(&r, &state);
        assert!(v.is_some(), "\"hello world\" 不包含 \"xyz\" → 违规");
    }

    #[test]
    fn test_check_rule_contains_array_not_violated() {
        let r = rule_with_value(
            "contains_arr",
            "payload.tags",
            Operator::Contains,
            serde_json::json!("a"),
        );
        let state = sample_state();
        let v = SemanticInvariantService::check_rule(&r, &state);
        assert!(v.is_none(), "tags 包含 \"a\" → 未违规");
    }

    #[test]
    fn test_check_rule_contains_array_violated() {
        let r = rule_with_value(
            "contains_arr",
            "payload.tags",
            Operator::Contains,
            serde_json::json!("z"),
        );
        let state = sample_state();
        let v = SemanticInvariantService::check_rule(&r, &state);
        assert!(v.is_some(), "tags 不包含 \"z\" → 违规");
    }

    #[test]
    fn test_check_rule_ne_path_violated_when_equal() {
        let mut r = rule("ne_path_check", "payload.amount", Operator::NePath);
        r.path2 = Some("payload.amount".to_string());
        let state = sample_state();
        let v = SemanticInvariantService::check_rule(&r, &state);
        assert!(v.is_some(), "两个路径值相等 → NePath 违规");
    }

    #[test]
    fn test_check_rule_ne_path_not_violated_when_different() {
        let mut r = rule("ne_path_check", "payload.amount", Operator::NePath);
        r.path2 = Some("payload.version".to_string());
        let state = sample_state();
        let v = SemanticInvariantService::check_rule(&r, &state);
        assert!(v.is_none(), "两个路径值不等 → NePath 未违规");
    }

    #[test]
    fn test_check_rule_violation_includes_correct_fields() {
        let r = rule_with_value(
            "field_check",
            "payload.amount",
            Operator::Gte,
            serde_json::json!(200),
        );
        let state = sample_state();
        let v = SemanticInvariantService::check_rule(&r, &state).expect("应违规");
        assert_eq!(v.rule_name, "field_check");
        assert_eq!(v.path, "payload.amount");
        assert_eq!(v.actual_value, serde_json::json!(100));
        assert_eq!(v.expected_value, Some(serde_json::json!(200)));
        assert_eq!(v.version, 5);
        assert_eq!(v.severity, Severity::High);
        assert_eq!(v.session_id, 0, "session_id 由调用者填充,初始为 0");
    }

    // ============ 服务层状态方法测试 ============

    #[test]
    fn test_add_rules_new_session() {
        let svc = new_service();
        let rule_set = InvariantSet {
            invariants: vec![
                rule("rule_a", "payload.x", Operator::Exists),
                rule("rule_b", "payload.y", Operator::Exists),
            ],
        };
        let count = svc.add_rules(1, rule_set);
        assert_eq!(count, 2);
        assert_eq!(svc.get_rules(1).len(), 2);
    }

    #[test]
    fn test_add_rules_append_to_existing() {
        let svc = new_service();
        svc.add_rules(
            1,
            InvariantSet {
                invariants: vec![rule("rule_a", "payload.x", Operator::Exists)],
            },
        );
        svc.add_rules(
            1,
            InvariantSet {
                invariants: vec![rule("rule_b", "payload.y", Operator::Exists)],
            },
        );
        let rules = svc.get_rules(1);
        assert_eq!(rules.len(), 2, "追加后应有 2 条规则");
    }

    #[test]
    fn test_get_rules_nonexistent_session() {
        let svc = new_service();
        assert!(svc.get_rules(999).is_empty(), "不存在的会话返回空数组");
    }

    #[test]
    fn test_remove_rule_existing() {
        let svc = new_service();
        svc.add_rules(
            1,
            InvariantSet {
                invariants: vec![rule("rule_a", "payload.x", Operator::Exists)],
            },
        );
        assert!(svc.remove_rule(1, "rule_a"), "删除存在的规则应返回 true");
        assert!(svc.get_rules(1).is_empty(), "删除后规则列表为空");
    }

    #[test]
    fn test_remove_rule_nonexistent() {
        let svc = new_service();
        svc.add_rules(
            1,
            InvariantSet {
                invariants: vec![rule("rule_a", "payload.x", Operator::Exists)],
            },
        );
        assert!(
            !svc.remove_rule(1, "rule_b"),
            "删除不存在的规则应返回 false"
        );
    }

    #[test]
    fn test_remove_rule_nonexistent_session() {
        let svc = new_service();
        assert!(!svc.remove_rule(999, "rule_a"), "不存在的会话返回 false");
    }

    #[test]
    fn test_get_violations_empty() {
        let svc = new_service();
        assert!(svc.get_violations(1).is_empty(), "无违规记录返回空数组");
    }

    #[test]
    fn test_get_stats_empty() {
        let svc = new_service();
        let stats = svc.get_stats();
        assert_eq!(stats.total_rules, 0);
        assert_eq!(stats.total_violations, 0);
        assert_eq!(stats.monitored_sessions, 0);
        assert_eq!(stats.sessions_with_violations, 0);
        assert_eq!(stats.violations_by_severity["critical"], 0);
        assert_eq!(stats.violations_by_severity["high"], 0);
    }

    #[test]
    fn test_get_stats_with_rules() {
        let svc = new_service();
        svc.add_rules(
            1,
            InvariantSet {
                invariants: vec![
                    rule("r1", "payload.x", Operator::Exists),
                    rule("r2", "payload.y", Operator::Exists),
                ],
            },
        );
        svc.add_rules(
            2,
            InvariantSet {
                invariants: vec![rule("r3", "payload.z", Operator::Exists)],
            },
        );
        let stats = svc.get_stats();
        assert_eq!(stats.total_rules, 3);
        assert_eq!(stats.monitored_sessions, 2);
        assert_eq!(stats.total_violations, 0, "无违规时 total_violations=0");
    }

    // ============ 服务层 HTTP 测试（mockito） ============

    #[tokio::test]
    async fn test_check_session_with_violation() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/sessions/1/state")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(state_response_body())
            .create_async()
            .await;

        let svc = SemanticInvariantService::new(server.url());
        // amount=100, 约束 amount >= 200 → 违规
        svc.add_rules(
            1,
            InvariantSet {
                invariants: vec![rule_with_value(
                    "amount_gte_200",
                    "payload.amount",
                    Operator::Gte,
                    serde_json::json!(200),
                )],
            },
        );

        let result = svc.check_session(1).await.expect("check 应成功");
        assert_eq!(result.session_id, 1);
        assert_eq!(result.version, 5);
        assert_eq!(result.checked_rules, 1);
        assert!(!result.passed, "应检测到违规");
        assert_eq!(result.violations.len(), 1);
        assert_eq!(result.violations[0].rule_name, "amount_gte_200");
        assert_eq!(result.violations[0].session_id, 1, "session_id 应被填充");

        // 违规应被记录到 store
        let stored = svc.get_violations(1);
        assert_eq!(stored.len(), 1, "违规应已记录");
    }

    #[tokio::test]
    async fn test_check_session_no_violation() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/sessions/1/state")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(state_response_body())
            .create_async()
            .await;

        let svc = SemanticInvariantService::new(server.url());
        // amount=100, 约束 amount >= 50 → 未违规
        svc.add_rules(
            1,
            InvariantSet {
                invariants: vec![rule_with_value(
                    "amount_gte_50",
                    "payload.amount",
                    Operator::Gte,
                    serde_json::json!(50),
                )],
            },
        );

        let result = svc.check_session(1).await.expect("check 应成功");
        assert!(result.passed, "不应有违规");
        assert!(result.violations.is_empty());
        // 无违规时不记录
        assert!(svc.get_violations(1).is_empty());
    }

    #[tokio::test]
    async fn test_check_session_upstream_error() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/sessions/1/state")
            .with_status(500)
            .create_async()
            .await;

        let svc = SemanticInvariantService::new(server.url());
        let err = svc.check_session(1).await.expect_err("上游 500 应报错");
        assert!(err.contains("state 端点返回"), "实际错误: {err}");
    }

    #[tokio::test]
    async fn test_check_session_network_error() {
        let svc = SemanticInvariantService::new("http://127.0.0.1:1".to_string());
        let err = svc.check_session(1).await.expect_err("网络不可达应报错");
        assert!(err.contains("请求 state 失败"), "实际错误: {err}");
    }

    // ============ handler 层 oneshot 测试 ============

    /// 辅助: 发送请求并返回 (status, body_text)
    async fn send_request(router: Router, req: Request<Body>) -> (StatusCode, String) {
        let response = router.oneshot(req).await.expect("oneshot failed");
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body collect failed")
            .to_bytes();
        (status, String::from_utf8_lossy(&body).to_string())
    }

    #[tokio::test]
    async fn test_handler_add_rules_success() {
        let svc = new_service();
        let router = build_router(svc);

        let body = serde_json::json!({
            "invariants": [{
                "name": "test_rule",
                "path": "payload.amount",
                "operator": "gte",
                "value": 0,
                "severity": "critical"
            }]
        })
        .to_string();

        let (status, body) = send_request(
            router,
            Request::builder()
                .method("POST")
                .uri("/api/sessions/1/invariants")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"added\":1"));
    }

    #[tokio::test]
    async fn test_handler_get_rules_success() {
        let svc = new_service();
        svc.add_rules(
            1,
            InvariantSet {
                invariants: vec![rule("rule_a", "payload.x", Operator::Exists)],
            },
        );
        let router = build_router(svc);

        let (status, body) = send_request(
            router,
            Request::builder()
                .method("GET")
                .uri("/api/sessions/1/invariants")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"count\":1"));
        assert!(body.contains("rule_a"));
    }

    #[tokio::test]
    async fn test_handler_delete_rule_success() {
        let svc = new_service();
        svc.add_rules(
            1,
            InvariantSet {
                invariants: vec![rule("rule_a", "payload.x", Operator::Exists)],
            },
        );
        let router = build_router(svc);

        let (status, body) = send_request(
            router,
            Request::builder()
                .method("DELETE")
                .uri("/api/sessions/1/invariants/rule_a")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"deleted\":true"));
    }

    #[tokio::test]
    async fn test_handler_delete_rule_not_found() {
        let svc = new_service();
        let router = build_router(svc);

        let (status, body) = send_request(
            router,
            Request::builder()
                .method("DELETE")
                .uri("/api/sessions/1/invariants/nonexistent")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"deleted\":false"));
    }

    #[tokio::test]
    async fn test_handler_get_violations_success() {
        let svc = new_service();
        let router = build_router(svc);

        let (status, body) = send_request(
            router,
            Request::builder()
                .method("GET")
                .uri("/api/sessions/1/violations")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"count\":0"));
    }

    #[tokio::test]
    async fn test_handler_check_success() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/sessions/1/state")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(state_response_body())
            .create_async()
            .await;

        let svc = SemanticInvariantService::new(server.url());
        svc.add_rules(
            1,
            InvariantSet {
                invariants: vec![rule_with_value(
                    "amount_gte_50",
                    "payload.amount",
                    Operator::Gte,
                    serde_json::json!(50),
                )],
            },
        );
        let router = build_router(svc);

        let (status, body) = send_request(
            router,
            Request::builder()
                .method("POST")
                .uri("/api/sessions/1/check")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"passed\":true"));
        assert!(body.contains("\"checked_rules\":1"));
    }

    #[tokio::test]
    async fn test_handler_check_upstream_error_returns_500() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/sessions/1/state")
            .with_status(500)
            .create_async()
            .await;

        let svc = SemanticInvariantService::new(server.url());
        let router = build_router(svc);

        let (status, body) = send_request(
            router,
            Request::builder()
                .method("POST")
                .uri("/api/sessions/1/check")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body.contains("state 端点返回"), "body={body}");
    }

    #[tokio::test]
    async fn test_handler_stats_success() {
        let svc = new_service();
        svc.add_rules(
            1,
            InvariantSet {
                invariants: vec![rule("r1", "payload.x", Operator::Exists)],
            },
        );
        let router = build_router(svc);

        let (status, body) = send_request(
            router,
            Request::builder()
                .method("GET")
                .uri("/api/stats")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"total_rules\":1"));
        assert!(body.contains("\"monitored_sessions\":1"));
    }

    #[tokio::test]
    async fn test_handler_add_rules_invalid_body_returns_400() {
        let svc = new_service();
        let router = build_router(svc);

        let (status, _body) = send_request(
            router,
            Request::builder()
                .method("POST")
                .uri("/api/sessions/1/invariants")
                .header("content-type", "application/json")
                .body(Body::from("{bad json}"))
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}

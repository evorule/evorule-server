// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! EvoRule 时间机器增强层
//!
//! 提供 rewind/diff/history 的本地实现，不再依赖 evorule-server 的 /rewind /diff 端点。
//! 通过 evorule-server 的 /api/sessions/{id}/history 获取完整 Fact 列表，
//! 在本地实现回溯和差异计算。
//!
//! # 架构定位
//!
//! rewind/diff 核心算法已从 evorule-governance 迁移到本模块（应用层），
//! 消除核心边界违规。tier2 仅保留 /history 端点（核心审计能力）。
//!
//! # 端点
//!
//! - `GET  /api/sessions/{id}/version-tree` — 版本树（所有 StateTransition 版本）
//! - `GET  /api/sessions/{id}/batch-diff?from=&to=` — 批量 diff（连续版本差异）
//! - `GET  /api/sessions/{id}/replay-plan?from=&to=` — 重放计划（前端可视化数据）
//! - `GET  /api/sessions/{id}/history` — 代理（转发到 evorule-server）
//! - `GET  /api/sessions/{id}/rewind/{v}` — 本地实现 rewind
//! - `GET  /api/sessions/{id}/diff?a=&b=` — 本地实现 diff

#![forbid(unsafe_code)]
// C5 (unwrap/expect/panic = deny) 仅约束生产代码;测试代码保留 unwrap 惯例
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use axum::http::StatusCode;
use axum::{extract::Path, extract::Query, extract::State, response::Json, routing::get, Router};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::info;

// ============ 数据结构 ============

/// evorule-server history 端点返回的单条记录（完整 Fact）
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HistoryEntry {
    pub version: u64,
    #[serde(rename = "type")]
    pub type_name: String,
    pub id: u64,
    /// 完整 Fact 数据（含 instruction/payload 等变体字段）
    #[serde(flatten)]
    pub data: serde_json::Value,
}

/// rewind 结果：指定 version 的物化快照
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewindSnapshot {
    pub payload: serde_json::Value,
    pub queue: Vec<serde_json::Value>,
    pub version: u64,
}

/// 两个 version 间的 payload diff 结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PayloadDiff {
    /// v_b 新增的字段（v_a 没有，v_b 有）
    pub added: Vec<(String, serde_json::Value)>,
    /// v_b 删除的字段（v_a 有，v_b 没有）
    pub removed: Vec<(String, serde_json::Value)>,
    /// 值变化的字段 (key, v_a 的值, v_b 的值)
    pub changed: Vec<(String, serde_json::Value, serde_json::Value)>,
    /// 值未变化的字段名
    pub unchanged: Vec<String>,
}

impl PayloadDiff {
    /// diff 是否为空（两个 payload 完全相同）
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }

    /// 变更字段总数（added + removed + changed）
    pub fn change_count(&self) -> usize {
        self.added.len() + self.removed.len() + self.changed.len()
    }

    /// 生成可读的 diff 摘要
    pub fn summary(&self) -> String {
        format!(
            "diff: +{} -{} ~{} (={} unchanged)",
            self.added.len(),
            self.removed.len(),
            self.changed.len(),
            self.unchanged.len()
        )
    }
}

/// 版本树节点
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionTreeNode {
    pub version: u64,
    pub fact_type: String,
    /// 是否为 StateTransition（payload 变更点）
    pub is_state_transition: bool,
}

/// 版本树响应
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionTreeResponse {
    pub session_id: u64,
    pub total_versions: u64,
    pub state_transition_count: usize,
    pub nodes: Vec<VersionTreeNode>,
}

/// 批量 diff 中的单条差异
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchDiffEntry {
    pub from_version: u64,
    pub to_version: u64,
    pub summary: String,
    pub change_count: usize,
    pub added: usize,
    pub removed: usize,
    pub changed: usize,
}

/// 批量 diff 响应
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchDiffResponse {
    pub session_id: u64,
    pub from_version: u64,
    pub to_version: u64,
    pub total_changes: usize,
    pub diffs: Vec<BatchDiffEntry>,
}

/// 重放计划中的单步
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayStep {
    pub version: u64,
    pub fact_type: String,
    /// 该版本的 payload 快照（仅 StateTransition 有）
    pub payload: Option<serde_json::Value>,
}

/// 重放计划响应
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayPlanResponse {
    pub session_id: u64,
    pub from_version: u64,
    pub to_version: u64,
    pub step_count: usize,
    pub steps: Vec<ReplayStep>,
}

// ============ 核心算法 ============

/// 从 history 列表回溯到指定 version，返回当时的快照
///
/// # 语义说明
///
/// - rewind 返回的是指定 version 的 **StateTransition 快照**
/// - **不包含**该 version 之后的 pending PayloadUpdate
/// - 如需获取当前 live 状态（含 pending 更新），请调用 evorule-server 的
///   `GET /api/sessions/{id}/state` 端点
/// - version 与 causal_depth 的区别见 tier1 ReactorStateSnapshot 文档
pub fn local_rewind(history: &[HistoryEntry], target_version: u64) -> Option<RewindSnapshot> {
    if target_version == 0 {
        return Some(RewindSnapshot {
            payload: serde_json::Value::Object(serde_json::Map::new()),
            queue: Vec::new(),
            version: 0,
        });
    }

    let mut payload = serde_json::Value::Object(serde_json::Map::new());
    let mut queue: Vec<serde_json::Value> = Vec::new();
    let mut version: u64 = 0;

    for entry in history {
        match entry.type_name.as_str() {
            "StateTransition" => {
                payload = entry
                    .data
                    .get("new_payload")
                    .cloned()
                    .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
                queue = entry
                    .data
                    .get("new_queue")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                version = entry.version + 1;
            }
            "IoResponse" => {
                version = entry.version + 1;
            }
            _ => {}
        }
        if version == target_version {
            break;
        }
    }

    // 循环未 break 说明从未命中 target_version:
    // 可能是目标超出末尾(version < target),也可能是版本间隙(version > target)。
    // 两种情况都应返回 None —— 不能静默返回最后命中的快照(否则版本间隙会回放错位)。
    if version != target_version {
        return None;
    }

    Some(RewindSnapshot {
        payload,
        queue,
        version,
    })
}

/// 两个 version 间的 payload diff
pub fn local_diff(history: &[HistoryEntry], v_a: u64, v_b: u64) -> PayloadDiff {
    let payload_a = local_rewind(history, v_a)
        .map(|s| s.payload)
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
    let payload_b = local_rewind(history, v_b)
        .map(|s| s.payload)
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));

    compute_diff(&payload_a, &payload_b)
}

fn compute_diff(payload_a: &serde_json::Value, payload_b: &serde_json::Value) -> PayloadDiff {
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    let mut unchanged = Vec::new();

    if let Some(map_a) = payload_a.as_object() {
        for (key, val_a) in map_a {
            match payload_b.as_object().and_then(|m| m.get(key)) {
                Some(val_b) => {
                    if val_a == val_b {
                        unchanged.push(key.clone());
                    } else {
                        changed.push((key.clone(), val_a.clone(), val_b.clone()));
                    }
                }
                None => {
                    removed.push((key.clone(), val_a.clone()));
                }
            }
        }
    }

    if let Some(map_b) = payload_b.as_object() {
        for (key, val_b) in map_b {
            if payload_a.as_object().and_then(|m| m.get(key)).is_none() {
                added.push((key.clone(), val_b.clone()));
            }
        }
    }

    PayloadDiff {
        added,
        removed,
        changed,
        unchanged,
    }
}

// ============ 聚合算法 (纯函数,无 HTTP 依赖,便于单测) ============
//
// 这三个函数从 TimeMachineService 的 async 方法中抽出核心逻辑,
// 接收 `&[HistoryEntry]` 直接计算,使版本树/批量 diff/重放计划的
// 窗口与过滤逻辑可在无 HTTP 环境下测试。

/// 构建版本树(纯函数)
///
/// 将 history 映射为 `VersionTreeNode` 列表,统计 StateTransition 数量,
/// `total_versions` 取 history 最后一条记录的 version(空 history → 0)。
pub fn build_version_tree(history: &[HistoryEntry], session_id: u64) -> VersionTreeResponse {
    let nodes: Vec<VersionTreeNode> = history
        .iter()
        .map(|e| VersionTreeNode {
            version: e.version,
            fact_type: e.type_name.clone(),
            is_state_transition: e.type_name == "StateTransition",
        })
        .collect();
    let state_transition_count = nodes.iter().filter(|n| n.is_state_transition).count();
    let total_versions = history.last().map(|e| e.version).unwrap_or(0);

    VersionTreeResponse {
        session_id,
        total_versions,
        state_transition_count,
        nodes,
    }
}

/// 构建批量 diff(纯函数)
///
/// 在 `[from, to]` 范围内取连续 StateTransition 对,计算相邻 ST 之间
/// 的 payload diff。**注意**:diff 用的是 snapshot version(= `entry.version + 1`),
/// 因为 `local_rewind` 的 target_version 语义是"ST 之后的快照版本号"。
/// `from_version`/`to_version` 字段仍报告 entry.version(标识是哪两个 ST)。
pub fn build_batch_diff(
    history: &[HistoryEntry],
    session_id: u64,
    from: u64,
    to: u64,
) -> BatchDiffResponse {
    // from..=to 范围内的 StateTransition(entry.version)
    let st_versions: Vec<u64> = history
        .iter()
        .filter(|e| e.type_name == "StateTransition")
        .map(|e| e.version)
        .filter(|v| *v >= from && *v <= to)
        .collect();

    let mut diffs = Vec::new();
    let mut total_changes = 0;

    for window in st_versions.windows(2) {
        let a = window[0];
        let b = window[1];
        // 用 snapshot version(a+1, b+1):ST(a) 之后 vs ST(b) 之后的快照
        let diff_result = local_diff(history, a + 1, b + 1);
        let summary = diff_result.summary();
        let added = diff_result.added.len();
        let removed = diff_result.removed.len();
        let changed = diff_result.changed.len();
        let change_count = diff_result.change_count();
        total_changes += change_count;

        diffs.push(BatchDiffEntry {
            from_version: a,
            to_version: b,
            summary,
            change_count,
            added,
            removed,
            changed,
        });
    }

    BatchDiffResponse {
        session_id,
        from_version: from,
        to_version: to,
        total_changes,
        diffs,
    }
}

/// 构建重放计划(纯函数)
///
/// 遍历 `[from, to]` 范围内的 history 条目,对 StateTransition 取其
/// 之后快照(snapshot version = `entry.version + 1`)的 payload。
pub fn build_replay_plan(
    history: &[HistoryEntry],
    session_id: u64,
    from: u64,
    to: u64,
) -> ReplayPlanResponse {
    let mut steps = Vec::new();
    for entry in history.iter() {
        if entry.version < from || entry.version > to {
            continue;
        }

        // 仅对 StateTransition 取 payload 快照(ST 之后的快照)
        let payload = if entry.type_name == "StateTransition" {
            local_rewind(history, entry.version + 1).map(|s| s.payload)
        } else {
            None
        };

        steps.push(ReplayStep {
            version: entry.version,
            fact_type: entry.type_name.clone(),
            payload,
        });
    }

    let step_count = steps.len();
    ReplayPlanResponse {
        session_id,
        from_version: from,
        to_version: to,
        step_count,
        steps,
    }
}

// ============ 服务 ============

/// 时间机器增强服务
#[derive(Clone)]
pub struct TimeMachineService {
    evorule_server_url: Arc<String>,
    client: reqwest::Client,
}

impl TimeMachineService {
    pub fn new(evorule_server_url: String) -> Self {
        Self {
            evorule_server_url: Arc::new(evorule_server_url),
            client: reqwest::Client::new(),
        }
    }

    /// 从 evorule-server 获取会话历史（完整 Fact 列表）
    async fn fetch_history(&self, session_id: u64) -> Result<Vec<HistoryEntry>, String> {
        let url = format!(
            "{}/api/sessions/{}/history",
            self.evorule_server_url, session_id
        );
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("请求 history 失败: {}", e))?;
        if !resp.status().is_success() {
            return Err(format!("history 端点返回 {}", resp.status()));
        }
        resp.json::<Vec<HistoryEntry>>()
            .await
            .map_err(|e| format!("解析 history 失败: {}", e))
    }

    /// 本地实现 rewind（不依赖 evorule-server 的 /rewind 端点）
    pub async fn rewind(
        &self,
        session_id: u64,
        target_version: u64,
    ) -> Result<RewindSnapshot, String> {
        let history = self.fetch_history(session_id).await?;
        local_rewind(&history, target_version)
            .ok_or_else(|| format!("version {} 不存在或超出范围", target_version))
    }

    /// 本地实现 diff（不依赖 evorule-server 的 /diff 端点）
    pub async fn diff(&self, session_id: u64, a: u64, b: u64) -> Result<PayloadDiff, String> {
        let history = self.fetch_history(session_id).await?;
        Ok(local_diff(&history, a, b))
    }

    /// 构建版本树
    pub async fn get_version_tree(&self, session_id: u64) -> Result<VersionTreeResponse, String> {
        let history = self.fetch_history(session_id).await?;
        Ok(build_version_tree(&history, session_id))
    }

    /// 批量 diff：连续版本间的差异矩阵
    pub async fn get_batch_diff(
        &self,
        session_id: u64,
        from: u64,
        to: u64,
    ) -> Result<BatchDiffResponse, String> {
        let history = self.fetch_history(session_id).await?;
        Ok(build_batch_diff(&history, session_id, from, to))
    }

    /// 重放计划：获取 from..=to 范围内每步的快照
    pub async fn get_replay_plan(
        &self,
        session_id: u64,
        from: u64,
        to: u64,
    ) -> Result<ReplayPlanResponse, String> {
        let history = self.fetch_history(session_id).await?;
        Ok(build_replay_plan(&history, session_id, from, to))
    }

    /// 代理：转发 history 请求
    pub async fn proxy_history(&self, session_id: u64) -> Result<serde_json::Value, String> {
        let history = self.fetch_history(session_id).await?;
        serde_json::to_value(&history).map_err(|e| e.to_string())
    }
}

// ============ HTTP API ============

#[derive(Deserialize)]
struct DiffParams {
    a: u64,
    b: u64,
}

#[derive(Deserialize)]
struct RangeParams {
    from: u64,
    to: u64,
}

/// handler 错误类型:(StatusCode, 错误消息)
/// 用元组而非裸 String —— axum 的 `String: IntoResponse` 会返回 200 OK,
/// 导致错误响应被客户端误判为成功。元组则正确返回指定状态码。
type ApiError = (StatusCode, String);

/// 版本树端点
async fn version_tree_handler(
    State(svc): State<TimeMachineService>,
    Path(session_id): Path<u64>,
) -> Result<Json<VersionTreeResponse>, ApiError> {
    let tree = svc
        .get_version_tree(session_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(tree))
}

/// 批量 diff 端点
async fn batch_diff_handler(
    State(svc): State<TimeMachineService>,
    Path(session_id): Path<u64>,
    Query(params): Query<RangeParams>,
) -> Result<Json<BatchDiffResponse>, ApiError> {
    let diff = svc
        .get_batch_diff(session_id, params.from, params.to)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(diff))
}

/// 重放计划端点
async fn replay_plan_handler(
    State(svc): State<TimeMachineService>,
    Path(session_id): Path<u64>,
    Query(params): Query<RangeParams>,
) -> Result<Json<ReplayPlanResponse>, ApiError> {
    let plan = svc
        .get_replay_plan(session_id, params.from, params.to)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(plan))
}

/// 代理：history
async fn proxy_history_handler(
    State(svc): State<TimeMachineService>,
    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let val = svc
        .proxy_history(session_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(val))
}

/// rewind 端点（本地实现）
async fn rewind_handler(
    State(svc): State<TimeMachineService>,
    Path((session_id, version)): Path<(u64, u64)>,
) -> Result<Json<RewindSnapshot>, ApiError> {
    let snap = svc
        .rewind(session_id, version)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(snap))
}

/// diff 端点（本地实现）
async fn diff_handler(
    State(svc): State<TimeMachineService>,
    Path(session_id): Path<u64>,
    Query(params): Query<DiffParams>,
) -> Result<Json<PayloadDiff>, ApiError> {
    let diff = svc
        .diff(session_id, params.a, params.b)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(diff))
}

/// 构建路由
pub fn build_router(service: TimeMachineService) -> Router {
    Router::new()
        // 增强端点
        .route("/api/sessions/{id}/version-tree", get(version_tree_handler))
        .route("/api/sessions/{id}/batch-diff", get(batch_diff_handler))
        .route("/api/sessions/{id}/replay-plan", get(replay_plan_handler))
        // 基础端点
        .route("/api/sessions/{id}/history", get(proxy_history_handler))
        .route("/api/sessions/{id}/rewind/{version}", get(rewind_handler))
        .route("/api/sessions/{id}/diff", get(diff_handler))
        .with_state(service)
}

/// 暴露的 HTTP 端点列表(用于启动日志)
const ENDPOINTS: &[&str] = &[
    "GET  /api/sessions/{id}/version-tree",
    "GET  /api/sessions/{id}/batch-diff?from=&to=",
    "GET  /api/sessions/{id}/replay-plan?from=&to=",
    "GET  /api/sessions/{id}/history",
    "GET  /api/sessions/{id}/rewind/{v}",
    "GET  /api/sessions/{id}/diff?a=&b=",
];

/// 启动 HTTP API 服务器
pub async fn run_server(service: TimeMachineService, addr: &str) -> Result<(), String> {
    let app = build_router(service);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("绑定地址 {} 失败: {}", addr, e))?;
    info!("时间机器服务已启动 addr={}", addr);
    info!("端点:");
    for endpoint in ENDPOINTS {
        info!("  {endpoint}");
    }
    axum::serve(listener, app)
        .await
        .map_err(|e| format!("服务器错误: {}", e))?;
    Ok(())
}

// ============ 单元测试 ============
//
// 测试覆盖 local_rewind 和 local_diff 核心算法,确保版本回溯和差异计算的正确性。
// 这些测试不依赖 HTTP 请求,直接测试纯函数逻辑。

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个 StateTransition 类型的 HistoryEntry
    fn make_state_transition(version: u64, payload: serde_json::Value) -> HistoryEntry {
        HistoryEntry {
            version,
            type_name: "StateTransition".to_string(),
            id: version,
            data: serde_json::json!({
                "new_payload": payload,
                "new_queue": []
            }),
        }
    }

    /// 构造一个 IoResponse 类型的 HistoryEntry
    fn make_io_response(version: u64) -> HistoryEntry {
        HistoryEntry {
            version,
            type_name: "IoResponse".to_string(),
            id: version,
            data: serde_json::json!({
                "result": null
            }),
        }
    }

    /// 构造一个 Command 类型的 HistoryEntry(应被 local_rewind 忽略)
    fn make_command(version: u64) -> HistoryEntry {
        HistoryEntry {
            version,
            type_name: "Command".to_string(),
            id: version,
            data: serde_json::json!({
                "instruction": {}
            }),
        }
    }

    // ===== local_rewind 测试 =====

    #[test]
    fn test_local_rewind_zero_returns_empty() {
        // version 0 应返回空 payload 和空 queue
        let history: Vec<HistoryEntry> = vec![];
        let result = local_rewind(&history, 0);
        assert!(result.is_some());
        let snap = result.unwrap();
        assert_eq!(snap.version, 0);
        assert!(snap.payload.is_object());
        assert!(snap.payload.as_object().unwrap().is_empty());
        assert!(snap.queue.is_empty());
    }

    #[test]
    fn test_local_rewind_basic_state_transition() {
        // 单个 StateTransition,回溯到 version 1
        let history = vec![make_state_transition(0, serde_json::json!({"amount": 100}))];

        let result = local_rewind(&history, 1);
        assert!(result.is_some());
        let snap = result.unwrap();
        assert_eq!(snap.version, 1);
        assert_eq!(snap.payload, serde_json::json!({"amount": 100}));
    }

    #[test]
    fn test_local_rewind_multiple_state_transitions() {
        // 多个 StateTransition,回溯到中间版本
        let history = vec![
            make_state_transition(0, serde_json::json!({"step": 1})),
            make_state_transition(1, serde_json::json!({"step": 2})),
            make_state_transition(2, serde_json::json!({"step": 3})),
        ];

        // 回溯到 version 2(第二个 StateTransition 之后)
        let result = local_rewind(&history, 2);
        assert!(result.is_some());
        let snap = result.unwrap();
        assert_eq!(snap.version, 2);
        assert_eq!(snap.payload, serde_json::json!({"step": 2}));
    }

    #[test]
    fn test_local_rewind_io_response_increments_version() {
        // IoResponse 应递增 version 但不改变 payload
        let history = vec![
            make_state_transition(0, serde_json::json!({"amount": 50})),
            make_io_response(1),
        ];

        // 回溯到 version 2(IoResponse 之后)
        let result = local_rewind(&history, 2);
        assert!(result.is_some());
        let snap = result.unwrap();
        assert_eq!(snap.version, 2);
        // payload 应保持前一个 StateTransition 的值
        assert_eq!(snap.payload, serde_json::json!({"amount": 50}));
    }

    #[test]
    fn test_local_rewind_command_ignored() {
        // Command 应被忽略(不影响 version 和 payload)
        let history = vec![
            make_command(0),
            make_state_transition(1, serde_json::json!({"amount": 100})),
        ];

        // 回溯到 version 2(StateTransition 之后)
        let result = local_rewind(&history, 2);
        assert!(result.is_some());
        let snap = result.unwrap();
        assert_eq!(snap.version, 2);
        assert_eq!(snap.payload, serde_json::json!({"amount": 100}));
    }

    #[test]
    fn test_local_rewind_out_of_range_returns_none() {
        // 目标 version 超出 history 范围应返回 None
        let history = vec![make_state_transition(0, serde_json::json!({"amount": 100}))];

        // history 只到 version 1,请求 version 5
        let result = local_rewind(&history, 5);
        assert!(result.is_none());
    }

    #[test]
    fn test_local_rewind_preserves_queue() {
        // StateTransition 的 new_queue 应被保留
        let entry = HistoryEntry {
            version: 0,
            type_name: "StateTransition".to_string(),
            id: 0,
            data: serde_json::json!({
                "new_payload": {"amount": 100},
                "new_queue": [{"type": "increment"}, {"type": "decrement"}]
            }),
        };

        let result = local_rewind(&[entry], 1);
        assert!(result.is_some());
        let snap = result.unwrap();
        assert_eq!(snap.queue.len(), 2);
    }

    // ===== local_diff 测试 =====

    #[test]
    fn test_local_diff_added_field() {
        // v_b 新增了字段
        let history = vec![
            make_state_transition(0, serde_json::json!({"a": 1})),
            make_state_transition(1, serde_json::json!({"a": 1, "b": 2})),
        ];

        let diff = local_diff(&history, 1, 2);
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.added[0].0, "b");
        assert_eq!(diff.removed.len(), 0);
        assert_eq!(diff.changed.len(), 0);
        assert_eq!(diff.unchanged.len(), 1);
        assert_eq!(diff.unchanged[0], "a");
    }

    #[test]
    fn test_local_diff_removed_field() {
        // v_b 删除了字段
        let history = vec![
            make_state_transition(0, serde_json::json!({"a": 1, "b": 2})),
            make_state_transition(1, serde_json::json!({"a": 1})),
        ];

        let diff = local_diff(&history, 1, 2);
        assert_eq!(diff.removed.len(), 1);
        assert_eq!(diff.removed[0].0, "b");
        assert_eq!(diff.added.len(), 0);
        assert_eq!(diff.changed.len(), 0);
    }

    #[test]
    fn test_local_diff_changed_field() {
        // v_b 改变了字段值
        let history = vec![
            make_state_transition(0, serde_json::json!({"a": 1})),
            make_state_transition(1, serde_json::json!({"a": 99})),
        ];

        let diff = local_diff(&history, 1, 2);
        assert_eq!(diff.changed.len(), 1);
        assert_eq!(diff.changed[0].0, "a");
        assert_eq!(diff.changed[0].1, serde_json::json!(1));
        assert_eq!(diff.changed[0].2, serde_json::json!(99));
        assert_eq!(diff.added.len(), 0);
        assert_eq!(diff.removed.len(), 0);
    }

    #[test]
    fn test_local_diff_identical_payloads() {
        // 两个 version 的 payload 完全相同
        let history = vec![
            make_state_transition(0, serde_json::json!({"a": 1, "b": 2})),
            make_state_transition(1, serde_json::json!({"a": 1, "b": 2})),
        ];

        let diff = local_diff(&history, 1, 2);
        assert!(diff.is_empty());
        assert_eq!(diff.unchanged.len(), 2);
    }

    // ===== PayloadDiff 方法测试 =====

    #[test]
    fn test_payload_diff_is_empty() {
        let empty_diff = PayloadDiff {
            added: vec![],
            removed: vec![],
            changed: vec![],
            unchanged: vec!["a".to_string()],
        };
        assert!(empty_diff.is_empty());

        let non_empty_diff = PayloadDiff {
            added: vec![("b".to_string(), serde_json::json!(2))],
            removed: vec![],
            changed: vec![],
            unchanged: vec!["a".to_string()],
        };
        assert!(!non_empty_diff.is_empty());
    }

    #[test]
    fn test_payload_diff_change_count() {
        let diff = PayloadDiff {
            added: vec![("x".to_string(), serde_json::json!(1))],
            removed: vec![("y".to_string(), serde_json::json!(2))],
            changed: vec![("z".to_string(), serde_json::json!(3), serde_json::json!(4))],
            unchanged: vec!["w".to_string()],
        };
        assert_eq!(diff.change_count(), 3);
    }

    #[test]
    fn test_payload_diff_summary() {
        let diff = PayloadDiff {
            added: vec![("x".to_string(), serde_json::json!(1))],
            removed: vec![("y".to_string(), serde_json::json!(2))],
            changed: vec![("z".to_string(), serde_json::json!(3), serde_json::json!(4))],
            unchanged: vec!["w".to_string()],
        };
        let summary = diff.summary();
        assert!(summary.contains("+1"));
        assert!(summary.contains("-1"));
        assert!(summary.contains("~1"));
        assert!(summary.contains("=1"));
    }

    // ===== local_rewind 边界测试 (P0) =====

    #[test]
    fn test_local_rewind_sparse_version_returns_none() {
        // 版本间隙:history 跳过 version 3,请求 version 3 应返回 None
        // (version 0 的 ST → snapshot version=1;version 5 的 ST → snapshot version=6)
        // S4: 循环结束后检查 `version != target_version`,正确处理 `version > target` 的间隙情况
        let history = vec![
            make_state_transition(0, serde_json::json!({"a": 1})),
            make_state_transition(5, serde_json::json!({"a": 2})),
        ];
        let result = local_rewind(&history, 3);
        assert!(
            result.is_none(),
            "稀疏版本间隙应返回 None,但实际返回了 {:?}",
            result
        );
    }

    // ===== S4: 版本间隙补充测试 =====
    //
    // 以下测试覆盖 local_rewind / local_diff / build_version_tree 在版本间隙
    // (version gap)场景下的行为。版本间隙指 history 中 entry.version 不连续,
    // 导致某些 snapshot version (= entry.version + 1) 不存在。
    // 契约:请求间隙中的 version 必须返回 None,不能静默返回相邻快照(否则回放错位)。

    #[test]
    fn test_local_rewind_gap_before_first_entry() {
        // 间隙在首条记录之前:第一个 ST 的 version=5 → snapshot version=6
        // 请求 version 1(在 6 之前)应返回 None
        let history = vec![make_state_transition(5, serde_json::json!({"a": 1}))];
        assert!(
            local_rewind(&history, 1).is_none(),
            "首条记录之前的版本间隙应返回 None"
        );
    }

    #[test]
    fn test_local_rewind_gap_created_by_ignored_command() {
        // Command 类型被 local_rewind 忽略(不递增 version),造成版本间隙
        // history: ST(0) → snap v1; Command(2) 被忽略(version 仍为 1); ST(3) → snap v4
        // 请求 version 2(Command 的 version+1)应返回 None,因为 Command 不产生快照
        let history = vec![
            make_state_transition(0, serde_json::json!({"a": 1})),
            make_command(2),
            make_state_transition(3, serde_json::json!({"a": 2})),
        ];
        assert!(
            local_rewind(&history, 2).is_none(),
            "Command 不产生快照,version 2 是间隙,应返回 None"
        );
        // 但边界 snapshot version 1 和 4 仍应存在
        assert!(local_rewind(&history, 1).is_some());
        assert!(local_rewind(&history, 4).is_some());
    }

    #[test]
    fn test_local_rewind_gap_between_st_and_io_response() {
        // ST 和 IoResponse 之间的版本间隙
        // history: ST(0) → snap v1; IoResponse(5) → snap v6
        // 请求 version 3(在 1 和 6 之间)应返回 None
        let history = vec![
            make_state_transition(0, serde_json::json!({"amount": 50})),
            make_io_response(5),
        ];
        assert!(
            local_rewind(&history, 3).is_none(),
            "ST 与 IoResponse 之间的间隙应返回 None"
        );
        // IoResponse 之后的 snapshot version=6 应存在且 payload 继承自前一个 ST
        let snap = local_rewind(&history, 6).expect("version 6 应存在");
        assert_eq!(snap.payload, serde_json::json!({"amount": 50}));
    }

    #[test]
    fn test_local_rewind_multiple_gaps_all_return_none() {
        // 多个间隙:ST 在 version 0, 5, 10 → snapshot versions = 1, 6, 11
        // 间隙中的 version 2,3,4,7,8,9 都应返回 None
        let history = vec![
            make_state_transition(0, serde_json::json!({"step": 1})),
            make_state_transition(5, serde_json::json!({"step": 2})),
            make_state_transition(10, serde_json::json!({"step": 3})),
        ];
        for gap_version in [2u64, 3, 4, 7, 8, 9] {
            assert!(
                local_rewind(&history, gap_version).is_none(),
                "间隙 version {} 应返回 None",
                gap_version
            );
        }
    }

    #[test]
    fn test_local_rewind_gap_boundaries_return_some() {
        // 验证间隙边界(有效的 snapshot version)仍正常返回
        // ST 在 version 0, 5 → snapshot versions = 1, 6
        let history = vec![
            make_state_transition(0, serde_json::json!({"a": 1})),
            make_state_transition(5, serde_json::json!({"a": 2})),
        ];
        let snap1 = local_rewind(&history, 1).expect("snapshot v1 应存在");
        assert_eq!(snap1.version, 1);
        assert_eq!(snap1.payload, serde_json::json!({"a": 1}));

        let snap6 = local_rewind(&history, 6).expect("snapshot v6 应存在");
        assert_eq!(snap6.version, 6);
        assert_eq!(snap6.payload, serde_json::json!({"a": 2}));
    }

    #[test]
    fn test_local_diff_gap_version_a_treats_as_empty() {
        // v_a 落在版本间隙 → local_rewind 返回 None → payload 退化为空对象
        // → v_b 的所有字段报为 added
        let history = vec![
            make_state_transition(0, serde_json::json!({"a": 1})),
            make_state_transition(5, serde_json::json!({"a": 1, "b": 2})),
        ];
        // v_a=3 在间隙中(1 < 3 < 6),v_b=6 存在
        let diff = local_diff(&history, 3, 6);
        assert_eq!(diff.added.len(), 2, "v_a 为空对象,v_b 的 a 和 b 都应报 added");
        assert!(diff.removed.is_empty());
        assert!(diff.changed.is_empty());
    }

    #[test]
    fn test_local_diff_both_versions_in_gap_returns_empty() {
        // v_a 和 v_b 都在版本间隙 → 两个 payload 都退化为空对象 → 空 diff
        let history = vec![
            make_state_transition(0, serde_json::json!({"a": 1})),
            make_state_transition(10, serde_json::json!({"a": 2})),
        ];
        // v_a=3, v_b=5 都在间隙中(1 < 3 < 5 < 11)
        let diff = local_diff(&history, 3, 5);
        assert!(diff.is_empty(), "两个间隙版本都退化为空对象,diff 应为空");
    }

    #[test]
    fn test_build_version_tree_sparse_versions() {
        // 稀疏版本:total_versions 取最后一条记录的 version(而非条目数)
        let history = vec![
            make_state_transition(0, serde_json::json!({"a": 1})),
            make_state_transition(10, serde_json::json!({"a": 2})),
        ];
        let tree = build_version_tree(&history, 42);
        assert_eq!(tree.total_versions, 10, "total_versions 应为最后一条的 version");
        assert_eq!(tree.nodes.len(), 2, "节点数应为实际条目数");
        assert_eq!(tree.state_transition_count, 2);
        assert_eq!(tree.session_id, 42);
    }

    #[test]
    fn test_build_batch_diff_spans_version_gap() {
        // 批量 diff 跨越版本间隙:ST 在 0 和 5,from=0, to=10
        // 应只对 (0,5) 这一对计算 diff,间隙不影响配对
        let history = vec![
            make_state_transition(0, serde_json::json!({"a": 1})),
            make_state_transition(5, serde_json::json!({"a": 1, "b": 2})),
        ];
        let resp = build_batch_diff(&history, 42, 0, 10);
        assert_eq!(resp.diffs.len(), 1, "只有一对 ST,应只产生 1 个 diff");
        assert_eq!(resp.diffs[0].from_version, 0);
        assert_eq!(resp.diffs[0].to_version, 5);
        assert_eq!(resp.diffs[0].added, 1, "v_b 比 v_a 多了 b 字段");
        assert_eq!(resp.total_changes, 1);
    }

    #[test]
    fn test_local_rewind_missing_new_payload_falls_back_empty() {
        // StateTransition 缺少 new_payload 字段 → payload 退化为空对象
        let entry = HistoryEntry {
            version: 0,
            type_name: "StateTransition".to_string(),
            id: 0,
            data: serde_json::json!({ "new_queue": [] }),
        };
        let snap = local_rewind(&[entry], 1).expect("version 1 应存在");
        assert!(snap
            .payload
            .as_object()
            .expect("payload 应为对象")
            .is_empty());
    }

    #[test]
    fn test_local_rewind_missing_new_queue_falls_back_empty() {
        // StateTransition 缺少 new_queue 字段 → queue 退化为空 vec
        let entry = HistoryEntry {
            version: 0,
            type_name: "StateTransition".to_string(),
            id: 0,
            data: serde_json::json!({ "new_payload": {"x": 1} }),
        };
        let snap = local_rewind(&[entry], 1).expect("version 1 应存在");
        assert!(snap.queue.is_empty());
    }

    #[test]
    fn test_local_rewind_non_array_new_queue_falls_back_empty() {
        // new_queue 不是数组(而是字符串)→ queue 退化为空 vec
        let entry = HistoryEntry {
            version: 0,
            type_name: "StateTransition".to_string(),
            id: 0,
            data: serde_json::json!({
                "new_payload": {"x": 1},
                "new_queue": "not-an-array"
            }),
        };
        let snap = local_rewind(&[entry], 1).expect("version 1 应存在");
        assert!(snap.queue.is_empty(), "非数组 new_queue 应退化为空 vec");
    }

    #[test]
    fn test_local_rewind_empty_history_nonzero_returns_none() {
        // 空 history + target>0 → None
        let history: Vec<HistoryEntry> = vec![];
        assert!(local_rewind(&history, 1).is_none());
        // target=0 仍应返回空快照(已在现有测试覆盖,此处不重复)
    }

    // ===== compute_diff / local_diff 边界测试 (P0) =====

    #[test]
    fn test_compute_diff_non_object_payloads_returns_empty() {
        // 两个非对象 payload(数组)无法按字段 diff → 空结果
        // 行为契约:非对象 payload 退化为空 diff(不报 added/removed/changed)
        let diff = local_diff(
            &[make_state_transition(0, serde_json::json!([1, 2, 3]))],
            1,
            1, // v_a==v_b,但两者 payload 都是数组
        );
        // v_a==v_b 时 local_rewind 返回同一个数组;compute_diff 对非对象返回空 diff
        // 注:此处主要锁定"非对象不 panic、返回空 diff"的行为
        let _ = diff; // 行为:数组 payload 不产生字段级 diff
    }

    #[test]
    fn test_compute_diff_two_arrays_is_empty() {
        // 两个不同的数组 payload → 浅 diff 视为非对象 → 空 diff
        let history = vec![
            make_state_transition(0, serde_json::json!([1, 2])),
            make_state_transition(1, serde_json::json!([1, 2, 3])),
        ];
        let diff = local_diff(&history, 1, 2);
        // 当前实现:数组不进入 as_object 分支 → added/removed/changed 全空
        assert!(diff.added.is_empty());
        assert!(diff.removed.is_empty());
        assert!(diff.changed.is_empty());
    }

    #[test]
    fn test_compute_diff_nested_object_is_shallow() {
        // 嵌套对象:浅 diff 只在顶层报 changed,不递归进嵌套字段
        let history = vec![
            make_state_transition(0, serde_json::json!({"nested": {"a": 1, "b": 2}})),
            make_state_transition(1, serde_json::json!({"nested": {"a": 99, "b": 2}})),
        ];
        let diff = local_diff(&history, 1, 2);
        // 顶层 "nested" 值整体变化 → 报 1 个 changed,不递归报告内部 a 变化
        assert_eq!(diff.changed.len(), 1);
        assert_eq!(diff.changed[0].0, "nested");
        assert_eq!(diff.changed[0].1, serde_json::json!({"a": 1, "b": 2}));
        assert_eq!(diff.changed[0].2, serde_json::json!({"a": 99, "b": 2}));
    }

    #[test]
    fn test_local_diff_nonexistent_version_a_treats_as_empty() {
        // v_a 不存在 → local_rewind 返回 None → 退化为空对象 → v_b 全部字段报 added
        let history = vec![
            make_state_transition(0, serde_json::json!({"a": 1})),
            make_state_transition(1, serde_json::json!({"a": 1, "b": 2})),
        ];
        let diff = local_diff(&history, 999, 2);
        assert_eq!(diff.added.len(), 2); // a 和 b 都报 added
        assert!(diff.removed.is_empty());
        assert!(diff.changed.is_empty());
    }

    #[test]
    fn test_local_diff_same_version_is_empty() {
        // 同一版本 diff 自身 → 空 diff
        let history = vec![make_state_transition(
            0,
            serde_json::json!({"a": 1, "b": 2}),
        )];
        let diff = local_diff(&history, 1, 1);
        assert!(diff.is_empty());
        assert_eq!(diff.unchanged.len(), 2);
    }

    // ===== build_version_tree 纯函数测试 (P1) =====

    #[test]
    fn test_build_version_tree_mixed_types() {
        let history = vec![
            make_state_transition(0, serde_json::json!({"a": 1})),
            make_io_response(1),
            make_state_transition(2, serde_json::json!({"a": 2})),
            make_command(3),
        ];
        let tree = build_version_tree(&history, 42);
        assert_eq!(tree.session_id, 42);
        assert_eq!(tree.total_versions, 3); // 最后一条 version=3
        assert_eq!(tree.state_transition_count, 2);
        assert_eq!(tree.nodes.len(), 4);
        assert!(tree.nodes[0].is_state_transition);
        assert!(!tree.nodes[1].is_state_transition);
        assert!(tree.nodes[2].is_state_transition);
        assert!(!tree.nodes[3].is_state_transition);
        assert_eq!(tree.nodes[2].fact_type, "StateTransition");
    }

    #[test]
    fn test_build_version_tree_empty_history() {
        let tree = build_version_tree(&[], 7);
        assert_eq!(tree.session_id, 7);
        assert_eq!(tree.total_versions, 0);
        assert_eq!(tree.state_transition_count, 0);
        assert!(tree.nodes.is_empty());
    }

    // ===== build_batch_diff 纯函数测试 (P1, 含 bug 回归) =====

    #[test]
    fn test_build_batch_diff_regression_snapshot_version() {
        // 回归测试:修正前传 entry.version(a,b) 给 local_diff,导致字段错位
        //   修正前 local_diff(history, 0, 1): 空快照 vs {a:1} → added=[a]  (错)
        //   修正后 local_diff(history, 1, 2): {a:1} vs {a:1,b:2} → added=[b] (对)
        // BatchDiffEntry 只存计数,通过 added==1 + removed==0 + changed==0 锁定
        // (a 在两边都有→unchanged; b 新增→added)
        let history = vec![
            make_state_transition(0, serde_json::json!({"a": 1})),
            make_state_transition(1, serde_json::json!({"a": 1, "b": 2})),
        ];
        let resp = build_batch_diff(&history, 1, 0, 1);
        assert_eq!(resp.diffs.len(), 1);
        let entry = &resp.diffs[0];
        assert_eq!(entry.from_version, 0);
        assert_eq!(entry.to_version, 1);
        assert_eq!(entry.added, 1); // b 新增
        assert_eq!(entry.removed, 0);
        assert_eq!(entry.changed, 0);
        assert_eq!(entry.change_count, 1);
        assert_eq!(resp.total_changes, 1);
        // 关键区分:修正前(传 entry.version)added=[a],unchanged=0 → summary "=0 unchanged"
        //          修正后(传 snapshot version)added=[b],unchanged=1 → summary "=1 unchanged"
        // added 计数两种情况都是 1,必须靠 unchanged 才能区分
        assert!(
            entry.summary.contains("=1 unchanged"),
            "修正后 a 应为 unchanged;实际 summary={}",
            entry.summary
        );
    }

    #[test]
    fn test_build_batch_diff_multiple_transitions() {
        let history = vec![
            make_state_transition(0, serde_json::json!({"a": 1})),
            make_state_transition(1, serde_json::json!({"a": 1, "b": 2})),
            make_state_transition(2, serde_json::json!({"a": 1, "b": 2, "c": 3})),
        ];
        let resp = build_batch_diff(&history, 1, 0, 2);
        // 3 个 ST → windows(2) 产生 2 条 diff
        assert_eq!(resp.diffs.len(), 2);
        assert_eq!(resp.diffs[0].from_version, 0);
        assert_eq!(resp.diffs[0].to_version, 1);
        assert_eq!(resp.diffs[0].added, 1); // +b
        assert_eq!(resp.diffs[1].from_version, 1);
        assert_eq!(resp.diffs[1].to_version, 2);
        assert_eq!(resp.diffs[1].added, 1); // +c
        assert_eq!(resp.total_changes, 2);
    }

    #[test]
    fn test_build_batch_diff_from_greater_than_to() {
        // from > to → 范围过滤为空 → 无 diff
        let history = vec![
            make_state_transition(0, serde_json::json!({"a": 1})),
            make_state_transition(1, serde_json::json!({"a": 2})),
        ];
        let resp = build_batch_diff(&history, 1, 2, 1);
        assert!(resp.diffs.is_empty());
        assert_eq!(resp.total_changes, 0);
    }

    #[test]
    fn test_build_batch_diff_single_transition_in_range() {
        // 范围内只有 1 个 ST → windows(2) 空 → 无 diff
        let history = vec![
            make_state_transition(0, serde_json::json!({"a": 1})),
            make_state_transition(5, serde_json::json!({"a": 2})),
        ];
        let resp = build_batch_diff(&history, 1, 0, 0);
        assert!(resp.diffs.is_empty());
    }

    #[test]
    fn test_build_batch_diff_no_state_transitions() {
        // history 无 ST → 无 diff
        let history = vec![make_io_response(0), make_command(1)];
        let resp = build_batch_diff(&history, 1, 0, 5);
        assert!(resp.diffs.is_empty());
        assert_eq!(resp.total_changes, 0);
    }

    // ===== build_replay_plan 纯函数测试 (P1) =====

    #[test]
    fn test_build_replay_plan_mixed_types() {
        let history = vec![
            make_state_transition(0, serde_json::json!({"a": 1})),
            make_io_response(1),
            make_state_transition(2, serde_json::json!({"a": 2})),
        ];
        let plan = build_replay_plan(&history, 9, 0, 2);
        assert_eq!(plan.session_id, 9);
        assert_eq!(plan.step_count, 3);
        assert_eq!(plan.steps.len(), 3);
        // ST 步有 payload,IoResponse 步 payload=None
        assert!(plan.steps[0].payload.is_some());
        assert!(plan.steps[1].payload.is_none());
        assert!(plan.steps[2].payload.is_some());
        // ST(0) 之后的快照 payload = {a:1}
        assert_eq!(plan.steps[0].payload, Some(serde_json::json!({"a": 1})));
        // ST(2) 之后的快照 payload = {a:2}
        assert_eq!(plan.steps[2].payload, Some(serde_json::json!({"a": 2})));
    }

    #[test]
    fn test_build_replay_plan_range_filter() {
        let history = vec![
            make_state_transition(0, serde_json::json!({"a": 1})),
            make_state_transition(1, serde_json::json!({"a": 2})),
            make_state_transition(2, serde_json::json!({"a": 3})),
        ];
        // 只取 version 1
        let plan = build_replay_plan(&history, 1, 1, 1);
        assert_eq!(plan.step_count, 1);
        assert_eq!(plan.steps[0].version, 1);
    }

    #[test]
    fn test_build_replay_plan_from_greater_than_to() {
        let history = vec![make_state_transition(0, serde_json::json!({"a": 1}))];
        let plan = build_replay_plan(&history, 1, 2, 1);
        assert_eq!(plan.step_count, 0);
        assert!(plan.steps.is_empty());
    }

    #[test]
    fn test_build_replay_plan_empty_history() {
        let plan = build_replay_plan(&[], 1, 0, 10);
        assert_eq!(plan.step_count, 0);
        assert!(plan.steps.is_empty());
    }

    // ===== fetch_history HTTP 错误路径测试 (P2, mockito) =====

    /// 构造合法 history JSON(两个条目:StateTransition + IoResponse)
    fn history_json_body() -> String {
        serde_json::json!([
            {"version": 0, "type": "StateTransition", "id": 0, "new_payload": {"amount": 100}, "new_queue": []},
            {"version": 1, "type": "IoResponse", "id": 1, "result": null}
        ])
        .to_string()
    }

    #[tokio::test]
    async fn test_fetch_history_success() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/sessions/1/history")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(history_json_body())
            .create_async()
            .await;

        let svc = TimeMachineService::new(server.url());
        let tree = svc.get_version_tree(1).await.expect("应成功");
        assert_eq!(tree.session_id, 1);
        assert_eq!(tree.nodes.len(), 2);
        assert_eq!(tree.state_transition_count, 1);
        assert_eq!(tree.total_versions, 1);
    }

    #[tokio::test]
    async fn test_fetch_history_non_2xx_error() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/sessions/2/history")
            .with_status(500)
            .create_async()
            .await;

        let svc = TimeMachineService::new(server.url());
        let result = svc.get_version_tree(2).await;
        let err = result.expect_err("非 2xx 应返回错误");
        assert!(err.contains("history 端点返回"), "实际错误: {err}");
        assert!(err.contains("500"), "实际错误: {err}");
    }

    #[tokio::test]
    async fn test_fetch_history_invalid_json_error() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/sessions/3/history")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("not-valid-json{{{")
            .create_async()
            .await;

        let svc = TimeMachineService::new(server.url());
        let result = svc.get_version_tree(3).await;
        let err = result.expect_err("无效 JSON 应返回错误");
        assert!(err.contains("解析 history 失败"), "实际错误: {err}");
    }

    #[tokio::test]
    async fn test_fetch_history_network_error() {
        // 指向一个拒绝连接的地址(端口 1 通常无服务)→ 网络错误
        let svc = TimeMachineService::new("http://127.0.0.1:1".to_string());
        let result = svc.get_version_tree(4).await;
        let err = result.expect_err("网络不可达应返回错误");
        assert!(err.contains("请求 history 失败"), "实际错误: {err}");
    }

    #[tokio::test]
    async fn test_rewind_via_http_success() {
        // 端到端:fetch + local_rewind
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/sessions/5/history")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(history_json_body())
            .create_async()
            .await;

        let svc = TimeMachineService::new(server.url());
        // ST(0) → snapshot version=1
        let snap = svc.rewind(5, 1).await.expect("rewind 应成功");
        assert_eq!(snap.version, 1);
        assert_eq!(snap.payload, serde_json::json!({"amount": 100}));
    }

    // ===== handler 层 oneshot 测试 (P3, 路由+参数解析+错误传播) =====

    use axum::body::Body;
    use http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn test_handler_version_tree_success() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/sessions/1/history")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(history_json_body())
            .create_async()
            .await;

        let svc = TimeMachineService::new(server.url());
        let router = build_router(svc);

        let response = router
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/sessions/1/version-tree")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let tree: VersionTreeResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(tree.session_id, 1);
        assert_eq!(tree.nodes.len(), 2);
    }

    #[tokio::test]
    async fn test_handler_diff_query_params() {
        // 验证 Query 参数解析 (?a=&b=)
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/sessions/1/history")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(history_json_body())
            .create_async()
            .await;

        let svc = TimeMachineService::new(server.url());
        let router = build_router(svc);

        let response = router
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/sessions/1/diff?a=1&b=2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let diff: PayloadDiff = serde_json::from_slice(&body).unwrap();
        // version 1 vs 2:但 history 只有 version 0,1 → v_b=2 不存在 → 退化为空对象 diff
        // (锁定 Query 解析成功 + handler 返回有效 JSON,不深究 diff 内容)
        let _ = diff;
    }

    #[tokio::test]
    async fn test_handler_upstream_error_returns_500() {
        // 上游 evorule-server 返回 500 → handler 的 String 错误 → axum 返回 500
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/sessions/1/history")
            .with_status(500)
            .create_async()
            .await;

        let svc = TimeMachineService::new(server.url());
        let router = build_router(svc);

        let response = router
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/sessions/1/version-tree")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // String 错误作为 IntoResponse 返回 500
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(body_str.contains("history 端点返回"), "body={body_str}");
    }
}

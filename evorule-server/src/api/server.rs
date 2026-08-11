// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! HTTP API 服务（axum）
//!
//! 提供外部访问接口，支持通过 HTTP 提交命令、查询状态、获取审计报告。
//!
//! # API 路由
//! - `POST /api/command` — 提交命令到反应器
//! - `POST /api/payload` — 更新 payload 字段
//! - `GET /api/state` — 获取当前状态快照
//! - `GET /api/audit` — 获取审计报告
//! - `POST /api/reload` — 手动触发配置热重载
//! - `GET /api/health` — 健康检查

use crate::auth::AuthConfig;
use crate::input_sanitizer::InputSanitizer;
use axum::http::Method;
use evorule_governance::auditor::Auditor;
use evorule_governance::metrics::SharedMetrics;
use evorule_governance::session;
use evorule_governance::shared_facts_log::SharedFactsLog;
use evorule_governance::{IoDispatcher, IoSubscriber};
use evorule_reactor::{Fact, FactId, FactSender, FactsLog};
use evorule_tcb::JsonValue;
use evorule_workspace::api::WorkspaceState;
use serde::Deserialize;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::Mutex;

/// 就绪标志（优雅退出时设为 false，readiness 端点返回 503）
pub type ReadinessFlag = Arc<AtomicBool>;

/// Governance API 共享状态
///
/// 持有反应器的 command 通道发送端和 FactsLog 引用，
/// 供 axum handler 共享访问。
#[derive(Clone)]
pub struct GovernanceApi {
    /// command 通道发送端（提交 Fact 到反应器）
    command_tx: FactSender,
    /// FactsLog 克隆（读取状态和历史）
    facts_log: FactsLog,
    /// 审计器（`Arc<Mutex>` 保护，因为需要可变操作）
    auditor: Arc<Mutex<Auditor>>,
    /// ID 生成器偏移
    next_id: Arc<std::sync::atomic::AtomicU64>,
}

impl GovernanceApi {
    /// 创建新 API 状态
    pub fn new(command_tx: FactSender, facts_log: FactsLog, auditor: Auditor) -> Self {
        Self {
            command_tx,
            facts_log,
            auditor: Arc::new(Mutex::new(auditor)),
            next_id: Arc::new(std::sync::atomic::AtomicU64::new(20000)),
        }
    }

    /// 生成下一个 FactId
    fn next_id(&self) -> FactId {
        FactId(
            self.next_id
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        )
    }

    /// 提交命令到反应器
    pub fn send_command(&self, instruction: JsonValue) -> Result<FactId, String> {
        let id = self.next_id();
        self.command_tx
            .send(Fact::Command { id, instruction })
            .map_err(|_| "Command channel closed".to_string())?;
        Ok(id)
    }

    /// 提交 PayloadUpdate 到反应器
    pub fn send_payload_update(&self, path: String, value: JsonValue) -> Result<FactId, String> {
        let id = self.next_id();
        self.command_tx
            .send(Fact::PayloadUpdate { id, path, value })
            .map_err(|_| "Command channel closed".to_string())?;
        Ok(id)
    }

    /// 获取当前状态快照
    pub fn snapshot(&self) -> (JsonValue, Vec<JsonValue>, u64) {
        self.facts_log.snapshot()
    }

    /// 获取 FactsLog 引用
    pub fn facts_log(&self) -> &FactsLog {
        &self.facts_log
    }

    /// 审计新增事实
    pub async fn audit_new(&self) -> usize {
        let mut auditor = self.auditor.lock().await;
        auditor.audit_new()
    }

    /// 获取审计报告
    pub async fn audit_report(&self) -> String {
        let auditor = self.auditor.lock().await;
        auditor.report()
    }

    /// 获取审计条目数
    pub async fn audit_entry_count(&self) -> usize {
        let auditor = self.auditor.lock().await;
        auditor.entries().len()
    }

    /// 验证审计链完整性
    pub async fn audit_verify(&self) -> bool {
        let auditor = self.auditor.lock().await;
        auditor.verify()
    }

    /// 获取审计器引用（用于高级操作）
    pub fn auditor(&self) -> Arc<Mutex<Auditor>> {
        self.auditor.clone()
    }
}

/// 全局 SSE 连接数上限（防止连接耗尽）
const MAX_SSE_CONNECTIONS: u64 = 100;

/// SSE 心跳间隔（每 15s 发送 `: ping` 保持连接活跃）
const SSE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// SSE 连接最大空闲时长（10 分钟无事件自动关闭）
const SSE_MAX_IDLE: Duration = Duration::from_secs(600);

/// HTTP 请求体大小上限（1MB，防止超大请求体攻击）
const MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024;

/// HTTP 并发请求数上限（1000 并发，防止连接耗尽）
const MAX_CONCURRENCY: usize = 1000;

/// 会话管理 API 共享状态
///
/// 持有 `SessionManager`（`Arc<Mutex>` 保护），管理多个独立反应器实例。
/// 每个会话拥有独立的 state、FactsLog、command/event 通道，配合长驻模式持续服务。
#[derive(Clone)]
pub struct SessionApi {
    /// 会话管理器
    sessions: Arc<Mutex<session::SessionManager>>,
    /// API 层 FactId 计数器（从 30000 起，避免与反应器自身 ID 冲突）
    next_id: Arc<std::sync::atomic::AtomicU64>,
    /// 当前活跃 SSE 连接数（全局计数器，限制 MAX_SSE_CONNECTIONS）
    sse_connections: Arc<AtomicU64>,
    /// 已加载的核心规则（core_eval）
    /// reload 时会更新此处 + SessionManager 内部 core_eval
    core_eval: Arc<std::sync::RwLock<Arc<Vec<JsonValue>>>>,
    /// core_eval.json 路径（TCB 宪法路径，reload 时重新读取）
    core_eval_path: std::path::PathBuf,
    /// rules_dir 路径（业务规则目录，reload 时重扫描）
    rules_dir: std::path::PathBuf,
    /// I/O 分发器（clone 给每个新 session 的 IoSubscriber，共享底层 handler）
    /// None 时 session 的 IoRequest 无人处理（纯计算场景）
    dispatcher: Option<IoDispatcher>,
}

impl SessionApi {
    /// 创建会话管理 API
    ///
    /// # 参数
    /// - `core_eval`：transform 规则列表（用于创建每个会话的反应器）
    /// - `max_rounds`：每个反应器的最大指令执行步数
    pub fn new(core_eval: Vec<JsonValue>, max_rounds: usize) -> Self {
        Self::new_with_fsync(core_eval, max_rounds, false)
    }

    /// 创建会话管理 API（支持 fsync 配置，P02）
    ///
    /// # 参数
    /// - `core_eval`：transform 规则列表（用于创建每个会话的反应器）
    /// - `max_rounds`：每个反应器的最大指令执行步数
    /// - `wal_fsync`：是否在每次 WAL 写入后执行 fsync
    pub fn new_with_fsync(core_eval: Vec<JsonValue>, max_rounds: usize, wal_fsync: bool) -> Self {
        Self::new_with_wal_options(core_eval, max_rounds, None, wal_fsync, 100 * 1024 * 1024)
    }

    /// 创建会话管理 API（支持完整 WAL 配置，P03）
    ///
    /// # 参数
    /// - `core_eval`：transform 规则列表（用于创建每个会话的反应器）
    /// - `max_rounds`：每个反应器的最大指令执行步数
    /// - `wal_dir`：WAL 文件存储目录（为 None 时使用纯内存模式）
    /// - `wal_fsync`：是否在每次 WAL 写入后执行 fsync
    /// - `max_wal_size_bytes`：单个 WAL 文件最大大小（0 表示不轮换）
    pub fn new_with_wal_options(
        core_eval: Vec<JsonValue>,
        max_rounds: usize,
        wal_dir: Option<std::path::PathBuf>,
        wal_fsync: bool,
        max_wal_size_bytes: u64,
    ) -> Self {
        Self::new_with_full_config(
            core_eval,
            max_rounds,
            wal_dir,
            wal_fsync,
            max_wal_size_bytes,
            false,
            1000,
            1,
            std::path::PathBuf::from("./resources/core_eval.json"),
            std::path::PathBuf::from("./rules"),
        )
    }

    /// 创建会话管理 API（支持完整配置，P06）
    ///
    /// # 参数
    /// - `core_eval`：transform 规则列表（用于创建每个会话的反应器）
    /// - `max_rounds`：每个反应器的最大指令执行步数
    /// - `wal_dir`：WAL 文件存储目录（为 None 时使用纯内存模式）
    /// - `wal_fsync`：是否在每次 WAL 写入后执行 fsync
    /// - `max_wal_size_bytes`：单个 WAL 文件最大大小（0 表示不轮换）
    /// - `auto_verify`：是否启用审计链实时验证
    /// - `auto_verify_threshold`：自动验证阈值（0 表示不限制）
    /// - `auto_verify_interval`：自动验证间隔（1 表示每次都验证）
    /// - `core_eval_path`：TCB 宪法 core_eval.json 路径（reload 时重读取）
    /// - `rules_dir`：业务规则目录（reload 时重扫描）
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_full_config(
        core_eval: Vec<JsonValue>,
        max_rounds: usize,
        wal_dir: Option<std::path::PathBuf>,
        wal_fsync: bool,
        max_wal_size_bytes: u64,
        auto_verify: bool,
        auto_verify_threshold: usize,
        auto_verify_interval: usize,
        core_eval_path: std::path::PathBuf,
        rules_dir: std::path::PathBuf,
    ) -> Self {
        let ce_cloned = core_eval.clone();
        let sessions = Arc::new(Mutex::new(
            session::SessionManager::with_limits_and_wal_and_auto_verify(
                core_eval,
                max_rounds,
                session::DEFAULT_MAX_SESSIONS,
                session::DEFAULT_SESSION_TTL,
                wal_dir,
                session::DEFAULT_SHARD_COUNT,
                wal_fsync,
                max_wal_size_bytes,
                auto_verify,
                auto_verify_threshold,
                auto_verify_interval,
            ),
        ));
        Self {
            sessions: sessions.clone(),
            next_id: Arc::new(std::sync::atomic::AtomicU64::new(30000)),
            sse_connections: Arc::new(AtomicU64::new(0)),
            core_eval: Arc::new(std::sync::RwLock::new(Arc::new(ce_cloned))),
            core_eval_path,
            rules_dir,
            dispatcher: None,
        }
    }

    /// 注入 I/O 分发器（builder 模式）
    ///
    /// 注入后，每个新创建的 session 会自动 spawn 一个 IoSubscriber，
    /// 将 session reactor 的 IoRequest 分发到注册的 handler。
    pub fn with_dispatcher(mut self, dispatcher: IoDispatcher) -> Self {
        self.dispatcher = Some(dispatcher);
        self
    }

    /// 获取已加载的核心规则（core_eval）只读引用
    ///
    /// 供 Portal API 查询当前加载的 transform 规则列表。
    /// reload 后此方法返回新的规则。
    pub fn core_eval(&self) -> Arc<Vec<JsonValue>> {
        let guard = match self.core_eval.read() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.clone()
    }

    /// 返回当前已加载 transform 规则数量
    pub fn core_eval_len(&self) -> usize {
        self.core_eval().len()
    }

    /// 生成下一个 FactId
    fn next_id(&self) -> FactId {
        FactId(
            self.next_id
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        )
    }

    /// 尝试获取一个 SSE 连接配额
    ///
    /// 成功返回 `SseConnectionGuard`，连接关闭时自动释放配额。
    /// 超过 `MAX_SSE_CONNECTIONS` 上限返回 `None`。
    fn try_acquire_sse(&self) -> Option<SseConnectionGuard> {
        let current = self.sse_connections.load(Ordering::SeqCst);
        if current >= MAX_SSE_CONNECTIONS {
            tracing::warn!(
                current,
                max = MAX_SSE_CONNECTIONS,
                "SSE 连接数已达上限，拒绝新连接"
            );
            return None;
        }
        let new_val = self.sse_connections.fetch_add(1, Ordering::SeqCst);
        if new_val >= MAX_SSE_CONNECTIONS {
            // 并发竞争回退
            self.sse_connections.fetch_sub(1, Ordering::SeqCst);
            tracing::warn!(
                current = new_val,
                max = MAX_SSE_CONNECTIONS,
                "SSE 连接数已达上限（并发竞争回退）"
            );
            return None;
        }
        tracing::debug!(
            active = new_val + 1,
            max = MAX_SSE_CONNECTIONS,
            "SSE 连接已建立"
        );
        Some(SseConnectionGuard {
            counter: self.sse_connections.clone(),
        })
    }

    /// 返回当前活跃 SSE 连接数（用于监控/测试）
    pub fn sse_connection_count(&self) -> u64 {
        self.sse_connections.load(Ordering::SeqCst)
    }

    /// 返回当前活跃会话数（语义修正）
    ///
    /// 与 `sse_connection_count()` 的区别：
    /// - `sse_connection_count`：SSE 连接数（一个会话可能无 SSE 或多 SSE）
    /// - `active_session_count`：真实活跃会话数（SessionManager 内部 atomic 计数）
    ///
    /// Portal summary 应使用此方法而非 SSE 连接数。
    pub async fn active_session_count(&self) -> u64 {
        let mgr = self.sessions.lock().await;
        mgr.len() as u64
    }

    /// 启动后台 reaper 任务，定期清理过期和已结束的会话
    ///
    /// 应在服务器启动时调用一次。清理间隔为 5 分钟。
    pub fn start_reaper(&self) {
        let sessions = self.sessions.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(evorule_governance::session::REAPER_INTERVAL);
            interval.tick().await; // 跳过第一次立即触发
            loop {
                interval.tick().await;
                let reaped = {
                    let mgr = sessions.lock().await;
                    mgr.reap_all()
                };
                if reaped > 0 {
                    tracing::info!(
                        reaped_count = reaped,
                        "Background reaper cleaned up expired/finished sessions"
                    );
                }
            }
        });
    }

    /// 重新从磁盘加载 TCB 宪法（core_eval.json）+ 业务规则（rules_dir）。
    ///
    /// 原子替换内部 `core_eval` 缓存和 SessionManager 内部用于新会话的 core_eval。
    /// 已存在会话的反应器**不会**中途换规则（保证 TCB 不可变语义）。
    ///
    /// # 返回
    /// - `Ok((old_len, new_len))`：旧规则数和新规则数
    /// - `Err(String)`：读取/解析失败（失败时旧规则保持不变）
    pub async fn reload_from_disk(&self) -> Result<(usize, usize), String> {
        let new_transforms =
            Self::load_merged_transforms_from_fs(&self.core_eval_path, &self.rules_dir)?;
        let new_len = new_transforms.len();

        // 1. 替换 SessionManager 内部 core_eval（新会话使用新规则）
        let old_len_mgr;
        {
            let mut mgr = self.sessions.lock().await;
            let old = mgr.replace_core_eval(new_transforms.clone());
            old_len_mgr = old.len();
        }

        // 2. 替换 SessionApi 自己的缓存
        let old_len_cache;
        {
            let mut w = match self.core_eval.write() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            old_len_cache = w.len();
            *w = Arc::new(new_transforms);
        }
        tracing::info!(
            old_len = old_len_cache,
            new_len,
            "session rules reloaded from disk"
        );
        Ok((old_len_mgr.max(old_len_cache), new_len))
    }

    /// 从文件系统合并加载：TCB 宪法 core_eval.json（在前）+ rules_dir/*.json（在后，按文件名字典序）
    pub fn load_merged_transforms_from_fs(
        core_eval_path: &std::path::Path,
        rules_dir: &std::path::Path,
    ) -> Result<Vec<JsonValue>, String> {
        let mut tcb = Self::load_core_eval_transforms(core_eval_path)?;
        tcb.extend(Self::load_rules_dir_transforms(rules_dir));
        Ok(tcb)
    }

    /// 加载 TCB 宪法 core_eval.json 的 transform 数组。
    ///
    /// 要求文件存在、可解析、`transform` 字段非空，任一不满足返回 Err。
    fn load_core_eval_transforms(
        core_eval_path: &std::path::Path,
    ) -> Result<Vec<JsonValue>, String> {
        let tcb_raw = std::fs::read_to_string(core_eval_path).map_err(|e| {
            format!(
                "读取 core_eval.json 失败 {}: {}",
                core_eval_path.display(),
                e
            )
        })?;
        let tcb_json: serde_json::Value = serde_json::from_str(&tcb_raw)
            .map_err(|e| format!("解析 core_eval.json 失败: {}", e))?;
        let tcb: Vec<JsonValue> = tcb_json
            .get("transform")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().cloned().map(serde_to_tcb).collect())
            .ok_or_else(|| {
                format!(
                    "core_eval.json {} 没有 transform 数组字段",
                    core_eval_path.display()
                )
            })?;
        if tcb.is_empty() {
            return Err(format!(
                "core_eval.json {} 的 transform 数组为空",
                core_eval_path.display()
            ));
        }
        Ok(tcb)
    }

    /// 扫描业务规则目录，按文件名字典序加载所有 *.json 的 transform 数组。
    ///
    /// 目录不存在或读取失败时返回空 Vec（不报错）；单个文件解析失败时
    /// warn 日志并跳过该文件（fail-soft，保证热重载可用性）。
    fn load_rules_dir_transforms(rules_dir: &std::path::Path) -> Vec<JsonValue> {
        if !rules_dir.exists() {
            return Vec::new();
        }
        let Ok(read_dir) = std::fs::read_dir(rules_dir) else {
            return Vec::new();
        };
        let mut entries: Vec<std::path::PathBuf> = Vec::new();
        for entry in read_dir.flatten() {
            let p = entry.path();
            if p.extension().map(|e| e == "json").unwrap_or(false) && p.is_file() {
                entries.push(p);
            }
        }
        entries.sort_by(|a, b| {
            a.file_name()
                .unwrap_or_default()
                .cmp(b.file_name().unwrap_or_default())
        });
        let mut out: Vec<JsonValue> = Vec::new();
        for p in entries {
            if let Some(extra) = Self::parse_rule_file(&p) {
                out.extend(extra);
            }
        }
        out
    }

    /// 读取并解析单个业务规则文件，返回其 transform 数组。
    ///
    /// 文件读取/解析失败或格式不符时 warn 并返回 None（fail-soft，保证热重载可用性）。
    fn parse_rule_file(p: &std::path::Path) -> Option<Vec<JsonValue>> {
        let raw = match std::fs::read_to_string(p) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("读取业务规则文件 {} 失败: {}，已跳过", p.display(), e);
                return None;
            }
        };
        let json: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("解析业务规则文件 {} 失败: {}，已跳过", p.display(), e);
                return None;
            }
        };
        if let Some(arr) = json.get("transform").and_then(|x| x.as_array()) {
            Some(arr.iter().cloned().map(serde_to_tcb).collect())
        } else if let Some(arr) = json.as_array() {
            Some(arr.iter().cloned().map(serde_to_tcb).collect())
        } else {
            tracing::warn!(
                "业务规则文件 {} 既不是 {{transform:[]}} 也不是 transform 数组，已跳过",
                p.display()
            );
            None
        }
    }
}

// =============================================================================
// WC-10: SessionOps for SessionApi — 桥接 evorule-workspace 与 SessionManager
// =============================================================================
// 设计依据: WORKSPACE_CRATE_DESIGN.md §4 (SessionOps trait)
// workspace crate 通过 SessionOps trait 抽象会话操作,此处为 SessionApi 实现该 trait,
// 使 WorkspaceService 能够调用底层 SessionManager 的方法。

#[async_trait::async_trait]
impl evorule_workspace::SessionOps for SessionApi {
    async fn create_session(&self) -> evorule_workspace::WorkspaceResult<u64> {
        let result = {
            let sessions = self.sessions.lock().await;
            sessions.create_session()
        };
        match result {
            Ok(id) => {
                // 为新 session 的 reactor spawn IoSubscriber (复用 create_session handler 逻辑)
                if let Some(ref dispatcher) = self.dispatcher {
                    let sessions = self.sessions.lock().await;
                    if let Some(session) = sessions.get_session(id) {
                        let event_rx = session.event_tx.subscribe();
                        let command_tx = session.command_tx.clone();
                        let subscriber =
                            IoSubscriber::new(dispatcher.clone());
                        tokio::spawn(async move {
                            if let Err(e) = subscriber.run(event_rx, command_tx).await {
                                tracing::error!(
                                    session_id = id,
                                    error = %e,
                                    "Workspace SessionOps: IoSubscriber 异常退出"
                                );
                            }
                        });
                    }
                }
                Ok(id)
            }
            Err(e) => Err(evorule_workspace::WorkspaceError::internal(format!(
                "create_session failed: {e:?}"
            ))),
        }
    }

    async fn fork_session(
        &self,
        parent_session_id: u64,
    ) -> evorule_workspace::WorkspaceResult<u64> {
        let result = {
            let sessions = self.sessions.lock().await;
            sessions.create_session_from_parent_at_version(parent_session_id, None)
        };
        match result {
            Ok(id) => Ok(id),
            Err(evorule_governance::session::SessionError::NotFound { id }) => {
                Err(evorule_workspace::WorkspaceError::not_found(
                    "session",
                    id.to_string(),
                ))
            }
            Err(evorule_governance::session::SessionError::LimitExceeded { current, max }) => {
                Err(evorule_workspace::WorkspaceError::internal(format!(
                    "session limit exceeded: {current}/{max}"
                )))
            }
            Err(e) => Err(evorule_workspace::WorkspaceError::internal(format!(
                "fork_session failed: {e:?}"
            ))),
        }
    }

    async fn close_session(
        &self,
        session_id: u64,
    ) -> evorule_workspace::WorkspaceResult<()> {
        let result = {
            let sessions = self.sessions.lock().await;
            sessions.close_session(session_id)
        };
        match result {
            Ok(_) => Ok(()),
            Err(_) => Err(evorule_workspace::WorkspaceError::not_found(
                "session",
                session_id.to_string(),
            )),
        }
    }

    async fn list_sessions(&self) -> evorule_workspace::WorkspaceResult<Vec<u64>> {
        let sessions = self.sessions.lock().await;
        Ok(sessions.list_sessions())
    }

    async fn send_command(
        &self,
        session_id: u64,
        command: serde_json::Value,
    ) -> evorule_workspace::WorkspaceResult<u64> {
        let id = self.next_id();
        let instruction = serde_to_tcb(command);

        let sessions = self.sessions.lock().await;
        sessions.touch_session(session_id);
        let session = sessions.get_session(session_id).ok_or_else(|| {
            evorule_workspace::WorkspaceError::not_found("session", session_id.to_string())
        })?;

        session
            .command_tx
            .send(Fact::Command { id, instruction })
            .map_err(|_| {
                evorule_workspace::WorkspaceError::internal(
                    "command channel closed (reactor exited)",
                )
            })?;
        // 缺口2 修复: send_command 后实时刷新审计链
        // 将 FactsLog 中已处理但尚未审计的 Fact 刷入 BLAKE3 哈希链。
        // reactor 异步处理, 此处刷新已处理的 fact; 未处理的由读方法兜底刷新。
        session.audit_new();
        Ok(id.0)
    }

    async fn get_session_state(
        &self,
        session_id: u64,
    ) -> evorule_workspace::WorkspaceResult<serde_json::Value> {
        let sessions = self.sessions.lock().await;
        sessions.touch_session(session_id);
        let session = sessions.get_session(session_id).ok_or_else(|| {
            evorule_workspace::WorkspaceError::not_found("session", session_id.to_string())
        })?;

        let (payload, queue, version) = session.facts_log.snapshot();

        let mut obj = serde_json::Map::new();
        obj.insert("payload".to_string(), tcb_to_serde(&payload));
        obj.insert(
            "queue".to_string(),
            serde_json::Value::Array(queue.iter().map(tcb_to_serde).collect()),
        );
        obj.insert(
            "version".to_string(),
            serde_json::Value::Number(version.into()),
        );
        Ok(serde_json::Value::Object(obj))
    }

    // ===== 沙盒编排扩展 (SANDBOX_ORCHESTRATION_DESIGN.md §3.2) =====

    /// 获取审计报告 (含 BLAKE3 链验证结果)
    ///
    /// 返回 JSON,包含审计链长度、验证状态、Fact 统计等。
    async fn get_audit_report(
        &self,
        session_id: u64,
    ) -> evorule_workspace::WorkspaceResult<serde_json::Value> {
        let sessions = self.sessions.lock().await;
        sessions.touch_session(session_id);
        let session = sessions.get_session(session_id).ok_or_else(|| {
            evorule_workspace::WorkspaceError::not_found("session", session_id.to_string())
        })?;

        // 兜底刷新: send_command 后已实时刷新, 此处确保 reactor 异步处理的 fact 也进链
        let _new_count = session.audit_new();
        let report_str = session.audit_report();
        let report: serde_json::Value = serde_json::from_str(&report_str)
            .map_err(|e| {
                evorule_workspace::WorkspaceError::internal(format!("audit parse failed: {e}"))
            })?;

        // 附加验证状态 (report 字段已含 last_hash/entry_count,补充 verified)
        let mut enriched = report;
        if let serde_json::Value::Object(ref mut map) = enriched {
            map.insert(
                "verified".into(),
                serde_json::json!(session.audit_verify()),
            );
            map.insert("session_id".into(), serde_json::json!(session_id));
        }
        Ok(enriched)
    }

    /// 获取审计链导出 (JSON 字符串)
    ///
    /// 返回完整的审计链 JSON,用于沙盒关闭时导出 test Fact。
    async fn get_audit_export(
        &self,
        session_id: u64,
    ) -> evorule_workspace::WorkspaceResult<String> {
        let sessions = self.sessions.lock().await;
        sessions.touch_session(session_id);
        let session = sessions.get_session(session_id).ok_or_else(|| {
            evorule_workspace::WorkspaceError::not_found("session", session_id.to_string())
        })?;

        // 兜底刷新: 确保 reactor 异步处理的 fact 已进 BLAKE3 链后再导出
        let _new_count = session.audit_new();
        Ok(session.audit_export())
    }

    /// 获取 Fact 列表 (用于测试报告统计)
    ///
    /// 从审计链导出中解析出 Fact 列表。
    async fn get_facts(
        &self,
        session_id: u64,
    ) -> evorule_workspace::WorkspaceResult<Vec<serde_json::Value>> {
        let export_str = self.get_audit_export(session_id).await?;
        let export: serde_json::Value = serde_json::from_str(&export_str)
            .map_err(|e| {
                evorule_workspace::WorkspaceError::internal(format!("audit export parse: {e}"))
            })?;

        // 审计导出包含 entries 数组,每条 entry 有 fact_id/fact_type/content_hash 等
        // 提取 entries 作为 Fact 列表
        let entries = export
            .get("entries")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(entries)
    }

    /// 获取因果链 (某条 Fact 的因果追溯)
    ///
    /// 返回从根 Fact 到指定 Fact 的因果链 JSON。
    async fn get_causal_chain(
        &self,
        session_id: u64,
        fact_id: u64,
    ) -> evorule_workspace::WorkspaceResult<serde_json::Value> {
        use evorule_reactor::FactId;

        let sessions = self.sessions.lock().await;
        sessions.touch_session(session_id);
        let session = sessions.get_session(session_id).ok_or_else(|| {
            evorule_workspace::WorkspaceError::not_found("session", session_id.to_string())
        })?;

        // 兜底刷新: 确保 fact 已进审计链后再追溯因果
        let _new_count = session.audit_new();
        let chain = session.causal_chain(FactId(fact_id));

        let entries: Vec<serde_json::Value> = chain
            .iter()
            .map(|e| {
                serde_json::json!({
                    "fact_id": e.fact_id.0,
                    "fact_type": e.fact_type,
                    "logical_time": e.logical_time,
                    "content_hash": e.content_hash,
                    "prev_hash": e.prev_hash,
                    "cause": e.cause.map(|c| c.0),
                })
            })
            .collect();

        Ok(serde_json::json!({
            "session_id": session_id,
            "fact_id": fact_id,
            "chain_length": entries.len(),
            "chain": entries,
        }))
    }

    // ===== 发布队列扩展 (PUBLISH_QUEUE_DESIGN.md §4) =====

    /// 触发规则热重载
    ///
    /// 调用 reload_from_disk,使 SessionManager 内部 core_eval 更新
    /// (影响后续新创建的 session,已存在 session 不受影响 — TCB 不可变语义)。
    async fn reload_rules(&self) -> evorule_workspace::WorkspaceResult<()> {
        self.reload_from_disk()
            .await
            .map(|_| ())
            .map_err(|e| {
                evorule_workspace::WorkspaceError::internal(format!("reload_rules failed: {e}"))
            })
    }

    /// 显式刷新审计链 (缺口5 修复)
    ///
    /// 将 FactsLog 中尚未审计的 Fact 刷入 BLAKE3 哈希链, 返回本次新增条目数。
    /// send_command 后应调用此方法确保审计链实时性。
    async fn flush_audit(
        &self,
        session_id: u64,
    ) -> evorule_workspace::WorkspaceResult<usize> {
        let sessions = self.sessions.lock().await;
        sessions.touch_session(session_id);
        let session = sessions.get_session(session_id).ok_or_else(|| {
            evorule_workspace::WorkspaceError::not_found("session", session_id.to_string())
        })?;
        Ok(session.audit_new())
    }
}

/// SSE 连接配额守卫
///
/// RAII 模式：Drop 时自动减少全局 SSE 连接计数器，
/// 确保连接断开后配额被正确释放。
pub struct SseConnectionGuard {
    counter: Arc<AtomicU64>,
}

impl Drop for SseConnectionGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
        tracing::debug!("SSE 连接配额已释放");
    }
}

/// SSE 指标守卫
///
/// RAII 模式：Drop 时自动减少 SSE 连接指标。
/// 在 `session_events` 的 `stream!` 内部持有，stream 结束时自动释放。
struct SseMetricsGuard(SharedMetrics);

impl Drop for SseMetricsGuard {
    fn drop(&mut self) {
        self.0.dec_sse_connections();
    }
}

/// 应用全局状态（合并 GovernanceApi + SessionApi + AgentManager + Metrics + Readiness + Workspace）
///
/// 通过 axum `FromRef` 模式，handler 可按需提取子状态：
/// - `State<GovernanceApi>` — 单反应器模式路由
/// - `State<SessionApi>` — 多会话模式路由
/// - `State<SharedMetrics>` — Prometheus 指标
/// - `State<ReadinessFlag>` — 就绪标志
/// - `State<WorkspaceState>` — 工作空间 + 规则元数据 (P10)
/// - `State<Arc<InputSanitizer>>` — Phase 1 第一层输入净化（Prompt 注入防御）
#[derive(Clone)]
pub struct AppState {
    /// 单反应器 API（向后兼容）
    governance: GovernanceApi,
    /// 多会话 API
    sessions: SessionApi,
    /// Prometheus 指标
    metrics: SharedMetrics,
    /// 就绪标志（优雅退出时设为 false）
    readiness: ReadinessFlag,
    /// 跨会话共享事实存储
    shared_facts: SharedFactsLog,
    /// 工作空间状态 (P10: 多租户工作空间 + 规则元数据)
    workspace: WorkspaceState,
    /// Phase 1: 输入净化器（HTTP 入口 Prompt 注入防御，静默改写）
    sanitizer: Arc<InputSanitizer>,
}

impl AppState {
    /// 创建应用全局状态
    pub fn new(
        governance: GovernanceApi,
        sessions: SessionApi,
        metrics: SharedMetrics,
        readiness: ReadinessFlag,
        shared_facts: SharedFactsLog,
        workspace: WorkspaceState,
        sanitizer: Arc<InputSanitizer>,
    ) -> Self {
        Self {
            governance,
            sessions,
            metrics,
            readiness,
            shared_facts,
            workspace,
            sanitizer,
        }
    }
}

impl FromRef<AppState> for GovernanceApi {
    fn from_ref(state: &AppState) -> Self {
        state.governance.clone()
    }
}

impl FromRef<AppState> for SessionApi {
    fn from_ref(state: &AppState) -> Self {
        state.sessions.clone()
    }
}

impl FromRef<AppState> for SharedMetrics {
    fn from_ref(state: &AppState) -> Self {
        state.metrics.clone()
    }
}

impl FromRef<AppState> for ReadinessFlag {
    fn from_ref(state: &AppState) -> Self {
        state.readiness.clone()
    }
}

impl FromRef<AppState> for SharedFactsLog {
    fn from_ref(state: &AppState) -> Self {
        state.shared_facts.clone()
    }
}

impl FromRef<AppState> for WorkspaceState {
    fn from_ref(state: &AppState) -> Self {
        state.workspace.clone()
    }
}

impl FromRef<AppState> for Arc<InputSanitizer> {
    fn from_ref(state: &AppState) -> Self {
        state.sanitizer.clone()
    }
}

/// HTTP API 请求体
#[derive(Debug, serde::Deserialize)]
pub struct CommandRequest {
    /// 指令 JSON
    pub instruction: serde_json::Value,
}

/// HTTP API 响应
#[derive(Debug, serde::Serialize)]
pub struct ApiResponse {
    /// 是否成功
    pub success: bool,
    /// 消息
    pub message: String,
    /// Fact ID（如适用）
    pub fact_id: Option<u64>,
}

/// PayloadUpdate 请求体
#[derive(Debug, serde::Deserialize)]
pub struct PayloadUpdateRequest {
    /// 字段路径
    pub path: String,
    /// 字段值
    pub value: serde_json::Value,
}

/// 将 serde_json::Value 转换为 evorule_tcb::JsonValue
fn serde_to_tcb(v: serde_json::Value) -> JsonValue {
    match v {
        serde_json::Value::Null => JsonValue::Null,
        serde_json::Value::Bool(b) => JsonValue::Bool(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                JsonValue::Integer(i)
            } else {
                JsonValue::String(n.to_string())
            }
        }
        serde_json::Value::String(s) => JsonValue::String(s),
        serde_json::Value::Array(arr) => {
            JsonValue::Array(arr.into_iter().map(serde_to_tcb).collect())
        }
        serde_json::Value::Object(obj) => {
            let mut map = std::collections::BTreeMap::new();
            for (k, val) in obj {
                map.insert(k, serde_to_tcb(val));
            }
            JsonValue::Object(map)
        }
    }
}

/// 将 evorule_tcb::JsonValue 转换为 serde_json::Value
fn tcb_to_serde(v: &JsonValue) -> serde_json::Value {
    match v {
        JsonValue::Null => serde_json::Value::Null,
        JsonValue::Bool(b) => serde_json::Value::Bool(*b),
        JsonValue::Integer(i) => serde_json::Value::Number((*i).into()),
        JsonValue::String(s) => serde_json::Value::String(s.clone()),
        JsonValue::Array(arr) => serde_json::Value::Array(arr.iter().map(tcb_to_serde).collect()),
        JsonValue::Object(map) => {
            let mut obj = serde_json::Map::new();
            for (k, val) in map {
                obj.insert(k.clone(), tcb_to_serde(val));
            }
            serde_json::Value::Object(obj)
        }
    }
}

// portal 已移除（应用层功能）
use async_stream::stream;
use axum::body::Bytes;
use axum::extract::{FromRef, Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use futures_core::Stream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::broadcast;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;

/// 将 Fact 序列化为 SSE 事件 data 字段（JSON 字符串）
///
/// 格式：`{"type":"Command","id":1,"instruction":{...}}`
pub fn fact_to_sse_data(fact: &Fact) -> String {
    let mut obj = serde_json::Map::new();
    match fact {
        Fact::Command { id, instruction } => {
            obj.insert("type".into(), serde_json::Value::String("Command".into()));
            obj.insert("id".into(), serde_json::Value::Number(id.0.into()));
            obj.insert("instruction".into(), tcb_to_serde(instruction));
        }
        Fact::StateTransition {
            id,
            cause,
            new_payload,
            new_queue,
        } => {
            obj.insert(
                "type".into(),
                serde_json::Value::String("StateTransition".into()),
            );
            obj.insert("id".into(), serde_json::Value::Number(id.0.into()));
            obj.insert("cause".into(), serde_json::Value::Number(cause.0.into()));
            obj.insert("new_payload".into(), tcb_to_serde(new_payload));
            obj.insert(
                "new_queue".into(),
                serde_json::Value::Array(new_queue.iter().map(tcb_to_serde).collect()),
            );
        }
        Fact::IoRequest {
            id,
            cause,
            io_type,
            params,
        } => {
            obj.insert("type".into(), serde_json::Value::String("IoRequest".into()));
            obj.insert("id".into(), serde_json::Value::Number(id.0.into()));
            obj.insert("cause".into(), serde_json::Value::Number(cause.0.into()));
            obj.insert(
                "io_type".into(),
                serde_json::Value::String(io_type.to_string()),
            );
            obj.insert("params".into(), tcb_to_serde(params));
        }
        Fact::IoResponse {
            id,
            request_id,
            result,
            error,
        } => {
            obj.insert(
                "type".into(),
                serde_json::Value::String("IoResponse".into()),
            );
            obj.insert("id".into(), serde_json::Value::Number(id.0.into()));
            obj.insert(
                "request_id".into(),
                serde_json::Value::Number(request_id.0.into()),
            );
            obj.insert("result".into(), tcb_to_serde(result));
            if let Some(err) = error {
                obj.insert("error".into(), serde_json::Value::String(err.clone()));
            }
        }
        Fact::Stable { id, final_snapshot } => {
            obj.insert("type".into(), serde_json::Value::String("Stable".into()));
            obj.insert("id".into(), serde_json::Value::Number(id.0.into()));
            obj.insert("final_snapshot".into(), tcb_to_serde(final_snapshot));
        }
        Fact::PayloadUpdate { id, path, value } => {
            obj.insert(
                "type".into(),
                serde_json::Value::String("PayloadUpdate".into()),
            );
            obj.insert("id".into(), serde_json::Value::Number(id.0.into()));
            obj.insert("path".into(), serde_json::Value::String(path.clone()));
            obj.insert("value".into(), tcb_to_serde(value));
        }
        Fact::Error { id, message } => {
            obj.insert("type".into(), serde_json::Value::String("Error".into()));
            obj.insert("id".into(), serde_json::Value::Number(id.0.into()));
            obj.insert("message".into(), serde_json::Value::String(message.clone()));
        }
    }
    serde_json::Value::Object(obj).to_string()
}

/// 健康检查 handler（向后兼容，等价于 liveness）
async fn health() -> Json<ApiResponse> {
    Json(ApiResponse {
        success: true,
        message: "ok".to_string(),
        fact_id: None,
    })
}

/// Liveness 探针（进程存活检查）
///
/// `GET /api/health/liveness` → 始终返回 200，只要进程在运行就算存活。
/// Kubernetes livenessProbe 用此端点判断是否需要重启容器。
async fn liveness() -> Json<ApiResponse> {
    Json(ApiResponse {
        success: true,
        message: "alive".to_string(),
        fact_id: None,
    })
}

/// Readiness 探针（就绪检查）
///
/// `GET /api/health/readiness` → readiness flag 为 true 时返回 200，否则 503。
/// 优雅退出时 flag 设为 false，负载均衡器将流量切走。
async fn readiness(State(flag): State<ReadinessFlag>) -> Result<Json<ApiResponse>, StatusCode> {
    if flag.load(std::sync::atomic::Ordering::SeqCst) {
        Ok(Json(ApiResponse {
            success: true,
            message: "ready".to_string(),
            fact_id: None,
        }))
    } else {
        Err(StatusCode::SERVICE_UNAVAILABLE)
    }
}

/// Prometheus 指标端点
///
/// `GET /metrics` → 返回 Prometheus 文本格式指标数据。
/// 此端点免认证（Prometheus scraper 通常不携带 token），但仍受速率限制和并发限制保护。
async fn metrics_handler(State(metrics): State<SharedMetrics>) -> String {
    metrics.render_as_text()
}

/// 认证中间件包装器
///
/// H6: 应用层始终启用认证（auth.rs 已迁移到应用层，无 cfg 门控）
async fn auth_middleware_wrapper(
    State(auth_config): State<crate::auth::AuthConfig>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, axum::http::StatusCode> {
    crate::auth::auth_middleware(State(auth_config), req, next).await
}

/// HTTP 请求计数中间件（N3：接入 http_requests_total 指标）
///
/// 用 method + 归一化 path + status 作为 label。
/// path 归一化：把纯数字段替换为 `{id}`，避免 /api/sessions/42/command 与
/// /api/sessions/43/command 产生不同 label 导致 Prometheus 基数爆炸。
async fn http_metrics_middleware(
    State(metrics): State<SharedMetrics>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let response = next.run(req).await;
    let status_code = response.status();
    let normalized = normalize_path_for_metrics(&path);
    metrics.inc_http_requests(method.as_str(), &normalized, status_code.as_str());
    response
}

/// 归一化 path 用于 metrics label，防止基数爆炸
///
/// 把纯数字段替换为 `{id}`：
/// - `/api/sessions/42/command` → `/api/sessions/{id}/command`
/// - `/api/sessions/42/audit/100` → `/api/sessions/{id}/audit/{id}`
fn normalize_path_for_metrics(path: &str) -> String {
    path.split('/')
        .map(|seg| {
            if !seg.is_empty() && seg.chars().all(|c| c.is_ascii_digit()) {
                "{id}"
            } else {
                seg
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// 提交命令 handler
async fn submit_command(
    State(api): State<GovernanceApi>,
    State(metrics): State<SharedMetrics>,
    State(sanitizer): State<Arc<InputSanitizer>>,
    Json(req): Json<CommandRequest>,
) -> Result<Json<ApiResponse>, StatusCode> {
    // Phase 1: 第一层输入净化（静默改写 Prompt 注入内容）
    let (instruction_value, sanitize_report) = sanitizer.sanitize_value(&req.instruction);
    if sanitize_report.has_hits() {
        tracing::warn!(
            hits = ?sanitize_report.unique_hits(),
            hit_count = sanitize_report.hit_count(),
            "submit_command 输入净化命中（已静默改写）"
        );
    }

    // 按指令类型计数
    {
        let cmd_type = instruction_value
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        metrics.inc_commands(cmd_type);
    }

    let instruction = serde_to_tcb(instruction_value);
    match api.send_command(instruction) {
        Ok(id) => Ok(Json(ApiResponse {
            success: true,
            message: "Command submitted".to_string(),
            fact_id: Some(id.0),
        })),
        Err(msg) => Ok(Json(ApiResponse {
            success: false,
            message: msg,
            fact_id: None,
        })),
    }
}

/// PayloadUpdate handler
async fn update_payload(
    State(api): State<GovernanceApi>,
    Json(req): Json<PayloadUpdateRequest>,
) -> Result<Json<ApiResponse>, StatusCode> {
    let value = serde_to_tcb(req.value);
    match api.send_payload_update(req.path, value) {
        Ok(id) => Ok(Json(ApiResponse {
            success: true,
            message: "PayloadUpdate submitted".to_string(),
            fact_id: Some(id.0),
        })),
        Err(msg) => Ok(Json(ApiResponse {
            success: false,
            message: msg,
            fact_id: None,
        })),
    }
}

/// 获取状态快照 handler
async fn get_state(State(api): State<GovernanceApi>) -> Json<serde_json::Value> {
    let (payload, queue, version) = api.snapshot();

    let mut obj = serde_json::Map::new();
    obj.insert("payload".to_string(), tcb_to_serde(&payload));
    obj.insert(
        "queue".to_string(),
        serde_json::Value::Array(queue.iter().map(tcb_to_serde).collect()),
    );
    obj.insert(
        "version".to_string(),
        serde_json::Value::Number(version.into()),
    );

    // 同步审计
    api.audit_new().await;

    Json(serde_json::Value::Object(obj))
}

/// 获取审计报告 handler
async fn get_audit(State(api): State<GovernanceApi>) -> Json<serde_json::Value> {
    api.audit_new().await;
    let report = api.audit_report().await;

    match serde_json::from_str::<serde_json::Value>(&report) {
        Ok(json) => Json(json),
        Err(_) => Json(serde_json::Value::String(report)),
    }
}

// ===== 会话管理路由（多反应器实例模式）=====

/// 创建会话 handler
///
/// `POST /api/sessions` → 创建新的长驻反应器实例，返回 session_id
/// 超过最大会话数时返回 429 Too Many Requests
async fn create_session(
    State(api): State<SessionApi>,
    State(metrics): State<SharedMetrics>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let result = {
        let sessions = api.sessions.lock().await;
        sessions.create_session()
    };
    match result {
        Ok(id) => {
            metrics.inc_sessions(); // 会话数 +1

            // 为新 session 的 reactor spawn IoSubscriber
            // 没有 IoSubscriber 时，session 的 IoRequest 会 60s 超时
            if let Some(ref dispatcher) = api.dispatcher {
                let sessions = api.sessions.lock().await;
                if let Some(session) = sessions.get_session(id) {
                    let event_rx = session.event_tx.subscribe();
                    let command_tx = session.command_tx.clone();
                    let subscriber =
                        IoSubscriber::new(dispatcher.clone()).with_metrics(metrics.clone());
                    tokio::spawn(async move {
                        if let Err(e) = subscriber.run(event_rx, command_tx).await {
                            tracing::error!(
                                session_id = id,
                                error = %e,
                                "Session IoSubscriber 异常退出"
                            );
                        }
                    });
                    tracing::info!(session_id = id, "IoSubscriber 已为 session 启动");
                }
            }

            Ok(Json(serde_json::json!({
                "session_id": id,
                "message": "Session created"
            })))
        }
        Err(evorule_governance::session::SessionError::LimitExceeded { current, max }) => {
            tracing::warn!(current, max, "Session creation rejected: limit exceeded");
            Err(StatusCode::TOO_MANY_REQUESTS)
        }
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// 列出所有会话 handler
///
/// `GET /api/sessions` → 返回所有活跃会话 ID
async fn list_sessions(
    State(api): State<SessionApi>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let list = {
        let sessions = api.sessions.lock().await;
        sessions.list_sessions()
    };
    Ok(Json(serde_json::json!({
        "sessions": list
    })))
}

/// 关闭会话 handler
///
/// `DELETE /api/sessions/:id` → 关闭指定会话，反应器优雅退出
async fn close_session(
    State(api): State<SessionApi>,
    State(metrics): State<SharedMetrics>,
    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let result = {
        let sessions = api.sessions.lock().await;
        sessions.close_session(session_id)
    };
    match result {
        Ok(_) => {
            metrics.dec_sessions(); // 会话数 -1
            Ok(Json(serde_json::json!({
                "session_id": session_id,
                "message": "Session closed"
            })))
        }
        Err(_) => Err(StatusCode::NOT_FOUND),
    }
}

/// 从父会话创建子会话 handler
///
/// `POST /api/sessions/from/:parent_id` → 基于父会话创建新会话，
/// 记录跨会话因果关系（父会话 ID + 初始内容哈希）
#[derive(serde::Deserialize)]
struct CreateSessionFromParentParams {
    version: Option<u64>,
}

async fn create_session_from_parent(
    State(api): State<SessionApi>,
    State(metrics): State<SharedMetrics>,
    Path(parent_id): Path<u64>,
    Query(params): Query<CreateSessionFromParentParams>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let result = {
        let sessions = api.sessions.lock().await;
        sessions.create_session_from_parent_at_version(parent_id, params.version)
    };
    match result {
        Ok(id) => {
            metrics.inc_sessions();
            Ok(Json(serde_json::json!({
                "session_id": id,
                "parent_session_id": parent_id,
                "message": "Session created from parent",
                "forked_from_version": params.version
            })))
        }
        Err(evorule_governance::session::SessionError::NotFound { id }) => {
            tracing::warn!(parent_id = id, "Parent session not found");
            Err(StatusCode::NOT_FOUND)
        }
        Err(evorule_governance::session::SessionError::LimitExceeded { current, max }) => {
            tracing::warn!(current, max, "Session creation rejected: limit exceeded");
            Err(StatusCode::TOO_MANY_REQUESTS)
        }
        Err(evorule_governance::session::SessionError::InvalidVersion { version }) => {
            tracing::warn!(version, "Invalid version for session fork");
            Err(StatusCode::BAD_REQUEST)
        }
    }
}

#[derive(serde::Deserialize)]
struct CreateSessionForkParams {
    version: Option<u64>,
}

async fn create_session_fork(
    State(api): State<SessionApi>,
    State(metrics): State<SharedMetrics>,
    Path(parent_id): Path<u64>,
    Query(params): Query<CreateSessionForkParams>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let version = params.version.ok_or(StatusCode::BAD_REQUEST)?;
    let result = {
        let sessions = api.sessions.lock().await;
        sessions.create_session_from_parent_at_version(parent_id, Some(version))
    };
    match result {
        Ok(id) => {
            metrics.inc_sessions();
            Ok(Json(serde_json::json!({
                "session_id": id,
                "parent_session_id": parent_id,
                "forked_from_version": version,
                "message": "Session forked from parent at specified version"
            })))
        }
        Err(evorule_governance::session::SessionError::NotFound { id }) => {
            tracing::warn!(parent_id = id, "Parent session not found for fork");
            Err(StatusCode::NOT_FOUND)
        }
        Err(evorule_governance::session::SessionError::LimitExceeded { current, max }) => {
            tracing::warn!(current, max, "Session fork rejected: limit exceeded");
            Err(StatusCode::TOO_MANY_REQUESTS)
        }
        Err(evorule_governance::session::SessionError::InvalidVersion { version }) => {
            tracing::warn!(version, "Invalid version for session fork");
            Err(StatusCode::BAD_REQUEST)
        }
    }
}

/// 会话命令提交 handler
///
/// `POST /api/sessions/:id/command` → 提交命令到指定会话的反应器
async fn session_command(
    State(api): State<SessionApi>,
    State(metrics): State<SharedMetrics>,
    State(sanitizer): State<Arc<InputSanitizer>>,
    Path(session_id): Path<u64>,
    Json(req): Json<CommandRequest>,
) -> Result<Json<ApiResponse>, StatusCode> {
    // Phase 1: 第一层输入净化（静默改写 Prompt 注入内容）
    let (instruction_value, sanitize_report) = sanitizer.sanitize_value(&req.instruction);
    if sanitize_report.has_hits() {
        tracing::warn!(
            hits = ?sanitize_report.unique_hits(),
            hit_count = sanitize_report.hit_count(),
            session_id = session_id,
            "session_command 输入净化命中（已静默改写）"
        );
    }

    // 按指令类型计数
    {
        let cmd_type = instruction_value
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        metrics.inc_commands(cmd_type);
    }

    let id = api.next_id();
    let instruction = serde_to_tcb(instruction_value);

    let sessions = api.sessions.lock().await;
    sessions.touch_session(session_id);
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    match session.command_tx.send(Fact::Command { id, instruction }) {
        Ok(()) => {
            // 缺口2 修复: 命令提交后实时刷新审计链 (BLAKE3 哈希链)
            session.audit_new();
            Ok(Json(ApiResponse {
                success: true,
                message: "Command submitted".to_string(),
                fact_id: Some(id.0),
            }))
        }
        Err(_) => Ok(Json(ApiResponse {
            success: false,
            message: "Command channel closed (reactor exited)".to_string(),
            fact_id: None,
        })),
    }
}

/// 会话状态查询 handler
///
/// `GET /api/sessions/:id/state` → 返回指定会话的状态快照
async fn session_state(
    State(api): State<SessionApi>,
    State(metrics): State<SharedMetrics>,
    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;
    sessions.touch_session(session_id);
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    let (payload, queue, version) = session.facts_log.snapshot();

    metrics.set_facts_log_version(version);

    let mut reactor_obj = serde_json::Map::new();
    reactor_obj.insert(
        "phase".into(),
        session
            .handle
            .current_phase()
            .map(|p| serde_json::Value::String(p.as_str().to_string()))
            .unwrap_or(serde_json::Value::Null),
    );
    reactor_obj.insert(
        "causal_depth".into(),
        session
            .handle
            .causal_depth()
            .map(|d| serde_json::Value::Number(d.into()))
            .unwrap_or(serde_json::Value::Null),
    );
    reactor_obj.insert(
        "structural_invariant_violations".into(),
        serde_json::Value::Number(session.handle.structural_invariant_violations().into()),
    );
    reactor_obj.insert(
        "pending_io_count".into(),
        session
            .handle
            .pending_io_count()
            .map(|c| serde_json::Value::Number(c.into()))
            .unwrap_or(serde_json::Value::Null),
    );
    reactor_obj.insert(
        "current_step".into(),
        session
            .handle
            .current_step()
            .map(|s| serde_json::Value::Number(s.into()))
            .unwrap_or(serde_json::Value::Null),
    );

    let mut obj = serde_json::Map::new();
    obj.insert("payload".into(), tcb_to_serde(&payload));
    obj.insert(
        "queue".into(),
        serde_json::Value::Array(queue.iter().map(tcb_to_serde).collect()),
    );
    obj.insert("version".into(), serde_json::Value::Number(version.into()));
    obj.insert("reactor".into(), serde_json::Value::Object(reactor_obj));

    Ok(Json(serde_json::Value::Object(obj)))
}

/// 会话审计报告 handler
///
/// `GET /api/sessions/:id/audit` → 返回指定会话的审计报告
async fn session_audit(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;
    sessions.touch_session(session_id);
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    // 先审计新事实
    let _new_count = session.audit_new();

    let report_str = session.audit_report();
    let report: serde_json::Value = serde_json::from_str(&report_str)
        .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));

    // 重新映射字段名以符合 API 文档规范
    let mut normalized = serde_json::Map::new();
    normalized.insert("session_id".into(), serde_json::json!(session_id));
    normalized.insert(
        "fact_count".into(),
        report
            .get("entry_count")
            .cloned()
            .unwrap_or(serde_json::json!(0)),
    );
    normalized.insert(
        "last_hash".into(),
        report
            .get("last_hash")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    );
    normalized.insert("verified".into(), serde_json::json!(session.audit_verify()));
    normalized.insert(
        "entries".into(),
        report
            .get("entries")
            .cloned()
            .unwrap_or(serde_json::json!([])),
    );

    Ok(Json(serde_json::Value::Object(normalized)))
}

/// 会话审计链验证 handler
///
/// `GET /api/sessions/:id/audit/verify` → 验证指定会话的审计链完整性
async fn session_audit_verify(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;
    sessions.touch_session(session_id);
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    let _new_count = session.audit_new();
    let valid = session.audit_verify();

    // 获取审计报告以提取更多信息
    let report_str = session.audit_report();
    let report: serde_json::Value = serde_json::from_str(&report_str)
        .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));

    Ok(Json(serde_json::json!({
        "verified": valid,
        "session_id": session_id,
        "fact_count": report.get("entry_count").cloned().unwrap_or(serde_json::Value::Null),
        "last_hash": report.get("last_hash").cloned().unwrap_or(serde_json::Value::Null),
    })))
}

/// 会话因果链查询 handler
///
/// `GET /api/sessions/:id/audit/causal/:fact_id` → 追溯指定 Fact 的因果链
async fn session_causal_chain(
    State(api): State<SessionApi>,
    Path((session_id, fact_id)): Path<(u64, u64)>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    use evorule_reactor::FactId;

    let sessions = api.sessions.lock().await;
    sessions.touch_session(session_id);
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    let _new_count = session.audit_new();
    let chain = session.causal_chain(FactId(fact_id));

    let entries: Vec<serde_json::Value> = chain
        .iter()
        .map(|e| {
            serde_json::json!({
                "fact_id": e.fact_id.0,
                "fact_type": e.fact_type,
                "logical_time": e.logical_time,
                "content_hash": e.content_hash,
                "prev_hash": e.prev_hash,
                "cause": e.cause.map(|c| c.0),
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "fact_id": fact_id,
        "chain_length": entries.len(),
        "chain": entries,
    })))
}

/// 会话审计链导出 handler（P04）
///
/// `GET /api/sessions/:id/audit/export` → 导出指定会话的审计链为 JSON
///
/// 返回包含完整哈希链的审计数据，可用于跨实例迁移、离线分析或备份。
/// 导出是只读操作，使用 GET 方法语义更合适。
async fn session_audit_export(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;
    sessions.touch_session(session_id);
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    // 先审计新事实，确保导出包含最新条目
    let _new_count = session.audit_new();

    let export_str = session.audit_export();
    let export: serde_json::Value = serde_json::from_str(&export_str)
        .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));

    Ok(Json(export))
}

/// 会话审计链导入 handler（P04）
///
/// `POST /api/sessions/:id/audit/import` → 导入外部审计链数据
///
/// **安全注意事项**：
/// 1. 导入操作会**完全覆盖**当前会话的审计链，具有破坏性
/// 2. 应仅允许管理员或授权用户调用此接口
/// 3. 建议在调用前验证导入数据的来源和完整性
/// 4. 导入后会自动调用 `verify()` 验证审计链完整性
///
/// # 返回
/// - `200 OK`：导入成功且审计链验证通过
/// - `202 Accepted`：导入成功但审计链验证失败（数据可能已损坏）
/// - `400 Bad Request`：JSON 解析失败或字段缺失
/// - `404 Not Found`：会话不存在
async fn session_audit_import(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
    Json(data): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;
    sessions.touch_session(session_id);
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    let json_str = serde_json::to_string(&data).map_err(|_| StatusCode::BAD_REQUEST)?;
    let (import_ok, verify_ok) = session.audit_import(&json_str);

    if !import_ok {
        return Err(StatusCode::BAD_REQUEST);
    }

    let status = if verify_ok { "ok" } else { "verify_failed" };
    if !verify_ok {
        tracing::warn!(
            session_id = session_id,
            "session_audit_import: 导入成功但审计链验证失败"
        );
    }

    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "imported": import_ok,
        "verify_ok": verify_ok,
        "status": status,
    })))
}

/// 会话审计链压缩导出 handler（P05）
///
/// `GET /api/sessions/:id/audit/export/compressed` → 返回 gzip 压缩的审计链
///
/// 返回 `application/gzip` 二进制数据，体积通常为 JSON 格式的 5-10%。
/// 适用于网络传输受限或大批量迁移场景。
async fn session_audit_export_compressed(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
) -> Result<Response, StatusCode> {
    let sessions = api.sessions.lock().await;
    sessions.touch_session(session_id);
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    // 先审计新事实，确保导出包含最新条目
    let _new_count = session.audit_new();

    let compressed = session.audit_export_compressed();
    if compressed.is_empty() {
        tracing::warn!(
            session_id = session_id,
            "session_audit_export_compressed: 压缩失败"
        );
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/gzip"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"audit_chain.json.gz\"",
            ),
        ],
        compressed,
    )
        .into_response())
}

/// 会话审计链压缩导入 handler（P05）
///
/// `POST /api/sessions/:id/audit/import/compressed` → 导入 gzip 压缩的审计链
///
/// 请求体为 gzip 二进制数据（`Content-Type: application/gzip`）。
/// 解压后等价于 [`session_audit_import`]，导入成功后自动调用 `verify()`。
///
/// **安全注意事项** 与 [`session_audit_import`] 相同。
async fn session_audit_import_compressed(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;
    sessions.touch_session(session_id);
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    if body.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let (import_ok, verify_ok) = session.audit_import_compressed(&body);

    if !import_ok {
        return Err(StatusCode::BAD_REQUEST);
    }

    let status = if verify_ok { "ok" } else { "verify_failed" };
    if !verify_ok {
        tracing::warn!(
            session_id = session_id,
            "session_audit_import_compressed: 导入成功但审计链验证失败"
        );
    }

    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "imported": import_ok,
        "verify_ok": verify_ok,
        "status": status,
        "format": "gzip",
    })))
}

/// 会话 PayloadUpdate handler
///
/// `POST /api/sessions/:id/payload` → 更新指定会话的 payload 字段
async fn session_payload(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
    Json(req): Json<PayloadUpdateRequest>,
) -> Result<Json<ApiResponse>, StatusCode> {
    let id = api.next_id();
    let value = serde_to_tcb(req.value);

    let sessions = api.sessions.lock().await;
    sessions.touch_session(session_id);
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    match session.command_tx.send(Fact::PayloadUpdate {
        id,
        path: req.path,
        value,
    }) {
        Ok(()) => Ok(Json(ApiResponse {
            success: true,
            message: "PayloadUpdate submitted".to_string(),
            fact_id: Some(id.0),
        })),
        Err(_) => Ok(Json(ApiResponse {
            success: false,
            message: "Command channel closed (reactor exited)".to_string(),
            fact_id: None,
        })),
    }
}

/// SSE 事件流 handler
///
/// `GET /api/sessions/:id/events` → 订阅指定会话的 event broadcast 通道，
/// 将 Fact 事件流式推送给客户端（text/event-stream）。
///
/// 事件格式：`data: {"type":"Command","id":1,"instruction":{...}}`
///
/// 连接保持直到：
/// - 客户端断开连接
/// - 会话被关闭（反应器退出，broadcast 通道关闭）
/// - 空闲超时（10 分钟无事件）
///
/// # 安全措施
/// - 全局 SSE 连接数限制（`MAX_SSE_CONNECTIONS=100`），超限返回 503
/// - 心跳（每 15s 发 `: ping`，防止代理/防火墙超时断开）
/// - 空闲超时（10 分钟无实际事件自动关闭，心跳不计入）
async fn session_events(
    State(api): State<SessionApi>,
    State(metrics): State<SharedMetrics>,
    Path(session_id): Path<u64>,
) -> Result<Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>>, StatusCode> {
    // 获取 SSE 连接配额，超限返回 503
    let sse_guard = api
        .try_acquire_sse()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    // SSE 连接指标 +1（stream 结束时通过 SseMetricsGuard 自动 -1）
    metrics.inc_sse_connections();

    // 从 SessionManager 获取 event 通道接收端
    let mut event_rx = {
        let sessions = api.sessions.lock().await;
        sessions.touch_session(session_id);
        let session = sessions
            .get_session(session_id)
            .ok_or(StatusCode::NOT_FOUND)?;
        session.event_tx.subscribe()
    };

    // 创建异步流：从 broadcast 接收 Fact，转换为 SSE Event
    // 心跳 + 空闲超时
    let stream = stream! {
        // 持有 SSE 连接配额守卫，stream 结束时自动释放
        let _guard = sse_guard;
        // 持有 metrics 守卫，stream 结束时自动 dec_sse_connections
        let _metrics_guard = SseMetricsGuard(metrics);

        let mut heartbeat = tokio::time::interval(SSE_HEARTBEAT_INTERVAL);
        heartbeat.tick().await; // 跳过第一次立即触发
        let mut last_event_time = tokio::time::Instant::now();

        loop {
            let idle_deadline = last_event_time + SSE_MAX_IDLE;
            tokio::select! {
                // 心跳：定期发送 : ping 保持连接活跃
                _ = heartbeat.tick() => {
                    yield Ok::<_, std::convert::Infallible>(
                        Event::default().comment("ping")
                    );
                }
                // 事件接收
                recv_result = event_rx.recv() => {
                    match recv_result {
                        Ok(fact) => {
                            last_event_time = tokio::time::Instant::now();
                            let data = fact_to_sse_data(&fact);
                            yield Ok::<_, std::convert::Infallible>(
                                Event::default().data(data)
                            );
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            tracing::debug!(
                                session_id,
                                "SSE stream closed: broadcast channel closed"
                            );
                            break;
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(
                                session_id,
                                dropped = n,
                                "SSE stream lagged, events dropped"
                            );
                            continue;
                        }
                    }
                }
                // 空闲超时：10 分钟无实际事件自动关闭（心跳不重置计时）
                _ = tokio::time::sleep_until(idle_deadline) => {
                    tracing::info!(
                        session_id,
                        idle_secs = SSE_MAX_IDLE.as_secs(),
                        "SSE 连接空闲超时，自动关闭"
                    );
                    break;
                }
            }
        }
    };

    Ok(Sse::new(stream))
}

#[derive(serde::Deserialize)]
struct ReplayParams {
    from: Option<u64>,
    to: Option<u64>,
}

#[derive(serde::Deserialize)]
struct FactsByPrefixParams {
    prefix: Option<String>,
}

// JoinRequest/BroadcastRequest/default_exclude_source 已删除（cluster 模块已移除）

/// IoResponse 请求体（外部提交）
#[derive(serde::Deserialize)]
struct IoResponseRequest {
    /// 对应的 IoRequest ID
    request_id: u64,
    /// I/O 执行结果
    result: serde_json::Value,
    /// I/O 错误信息（可选）
    error: Option<String>,
}

async fn session_replay(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
    Query(params): Query<ReplayParams>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    let from = params.from.unwrap_or(0);
    let to = params.to.unwrap_or_else(|| session.facts_log.version());

    let all_facts = session.facts_log.history_with_versions();
    let result: Vec<_> = all_facts
        .into_iter()
        .filter(|(v, _)| *v >= from && *v <= to)
        .map(|(version, fact)| {
            let mut json = fact.to_json();
            json.insert("version".to_string(), JsonValue::integer(version as i64));
            tcb_to_serde(&json)
        })
        .collect();

    Ok(Json(serde_json::Value::Array(result)))
}

async fn session_history(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    let all_facts = session.facts_log.history_with_versions();
    let result: Vec<_> = all_facts
        .into_iter()
        .map(|(version, fact)| {
            let mut json = fact.to_json();
            json.insert("version".to_string(), JsonValue::integer(version as i64));
            tcb_to_serde(&json)
        })
        .collect();

    Ok(Json(serde_json::Value::Array(result)))
}

// rewind/diff 端点（时间旅行调试）
#[derive(Deserialize)]
struct RewindParams {
    version: u64,
}

#[derive(Deserialize)]
struct DiffParams {
    a: u64,
    b: u64,
}

async fn session_rewind(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
    Query(params): Query<RewindParams>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    let facts = session.facts_log.history();
    let snapshot = evorule_governance::time_machine::rewind(&facts, params.version);

    match snapshot {
        Some(snap) => Ok(Json(serde_json::json!({
            "session_id": session_id,
            "target_version": params.version,
            "payload": snap.payload,
            "queue": snap.queue,
            "actual_version": snap.version,
        }))),
        None => Err(StatusCode::BAD_REQUEST),
    }
}

async fn session_diff(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
    Query(params): Query<DiffParams>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    let facts = session.facts_log.history();
    let diff = evorule_governance::time_machine::diff(&facts, params.a, params.b);

    // 契约对齐(S1 修复,2026-08-03):
    //   ttd 修复 2 + SPEC §1.1 规定 diff 返回 { items: [...] },元素为元组:
    //     - added   → [key, value]      (2 元组)
    //     - changed → [key, old, new]   (3 元组)
    //   D1-B 扩展契约:removed 不并入 items(items 契约只支持 added/changed 语义),
    //   单独作为 removed 字段返回 [[key, value], ...]。
    //   之前返回 { added, removed, changed, unchanged, summary } 违背既定契约,已修正。
    let mut items: Vec<serde_json::Value> =
        Vec::with_capacity(diff.added.len() + diff.changed.len());
    for (k, v) in &diff.added {
        items.push(serde_json::json!([k, v]));
    }
    for (k, old, new) in &diff.changed {
        items.push(serde_json::json!([k, old, new]));
    }
    let removed: Vec<serde_json::Value> = diff
        .removed
        .iter()
        .map(|(k, v)| serde_json::json!([k, v]))
        .collect();

    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "from_version": params.a,
        "to_version": params.b,
        "items": items,
        "removed": removed,
        "summary": diff.summary(),
    })))
}

async fn session_facts_by_prefix(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
    Query(params): Query<FactsByPrefixParams>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    let prefix = params.prefix.unwrap_or_default();
    let facts = session.facts_log.facts_by_path_prefix(&prefix);

    // D-S3 修复(2026-08-03):非 PayloadUpdate 的 fact 跳过(filter),不再返回空对象 {}。
    //   facts 端点语义是"按 path prefix 的 payload 更新索引",只返回 PayloadUpdate。
    let result: Vec<_> = facts
        .into_iter()
        .filter_map(|(version, fact)| {
            if let Fact::PayloadUpdate { id, path, value } = fact {
                Some(serde_json::json!({
                    "fact_id": id.0,
                    "version": version,
                    "path": path,
                    "value": tcb_to_serde(&value),
                }))
            } else {
                None
            }
        })
        .collect();

    Ok(Json(serde_json::Value::Array(result)))
}

async fn shared_facts_by_prefix(
    State(shared_facts): State<SharedFactsLog>,
    Query(params): Query<FactsByPrefixParams>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let prefix = params.prefix.unwrap_or_default();
    let facts = shared_facts.facts_by_path_prefix(&prefix);

    let result: Vec<_> = facts
        .into_iter()
        .map(|sf| {
            serde_json::json!({
                "fact_id": sf.fact_id.0,
                "path": sf.path,
                "value": tcb_to_serde(&sf.value),
                "source_session_id": sf.source_session_id,
                "version": sf.version,
            })
        })
        .collect();

    Ok(Json(serde_json::Value::Array(result)))
}

async fn shared_fact_source(
    State(shared_facts): State<SharedFactsLog>,
    Path(fact_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let fact = shared_facts
        .fact_by_id(FactId(fact_id))
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(serde_json::json!({
        "fact_id": fact.fact_id.0,
        "path": fact.path,
        "value": tcb_to_serde(&fact.value),
        "source_session_id": fact.source_session_id,
        "version": fact.version,
    })))
}

async fn record_used_at_startup(
    State(shared_facts): State<SharedFactsLog>,
    Path(session_id): Path<u64>,
    Json(req): Json<serde_json::Value>,
) -> Result<Json<ApiResponse>, StatusCode> {
    let fact_ids: Vec<FactId> = req
        .get("fact_ids")
        .and_then(|v| v.as_array())
        .ok_or(StatusCode::BAD_REQUEST)?
        .iter()
        .filter_map(|v| v.as_u64())
        .map(FactId)
        .collect();

    shared_facts.record_used_at_startup(session_id, &fact_ids);

    tracing::info!(
        session_id,
        fact_count = fact_ids.len(),
        "Recorded used_at_startup"
    );
    Ok(Json(ApiResponse {
        success: true,
        message: "used_at_startup recorded".to_string(),
        fact_id: None,
    }))
}

async fn get_used_at_startup(
    State(shared_facts): State<SharedFactsLog>,
    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let fact_ids = shared_facts
        .get_used_at_startup(session_id)
        .unwrap_or_default();

    let result: Vec<_> = fact_ids.into_iter().map(|f| f.0).collect();
    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "fact_ids": result,
    })))
}

async fn get_sessions_using_fact(
    State(shared_facts): State<SharedFactsLog>,
    Path(fact_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = shared_facts.get_sessions_using_fact(FactId(fact_id));
    Ok(Json(serde_json::json!({
        "fact_id": fact_id,
        "sessions": sessions,
    })))
}

async fn debug_phase(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    let phase = session
        .handle
        .current_phase()
        .map(|p| serde_json::Value::String(p.as_str().to_string()))
        .unwrap_or(serde_json::Value::Null);

    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "phase": phase,
    })))
}

async fn debug_queue(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;
    let _session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "queue": serde_json::Value::Array(vec![]),
    })))
}

async fn debug_pending_io(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    let pending_count = session.handle.pending_io_count().unwrap_or(0);

    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "pending_io_count": pending_count,
        "pending_io": serde_json::Value::Array(vec![]),
    })))
}

async fn session_interrupt(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    session.handle.interrupt();

    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "success": true,
        "message": "Interrupt requested, reactor will respond at next checkpoint",
    })))
}

/// 检查会话是否已结束
async fn session_finished(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "finished": session.is_finished(),
    })))
}

/// 获取因果链深度
async fn session_causal_depth(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "causal_depth": session.causal_depth(),
    })))
}

/// 获取不变式违规计数
async fn session_invariants(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "structural_invariant_violations": session.structural_invariant_violations(),
    })))
}

/// 获取待处理 I/O 数量
async fn session_pending_io_count(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "pending_io_count": session.pending_io_count(),
    })))
}

/// 获取当前执行步数
async fn session_step(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "current_step": session.current_step(),
    })))
}

/// 获取完整状态快照
async fn session_snapshot(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    match session.snapshot() {
        Some(snap) => Ok(Json(serde_json::json!({
            "session_id": session_id,
            "finished": snap.finished,
            "phase": format!("{:?}", snap.phase),
            "version": snap.version,
            "steps": snap.steps,
            "pending_io_count": snap.pending_io_count,
            "structural_invariant_violations": snap.structural_invariant_violations,
        }))),
        None => Ok(Json(serde_json::json!({
            "session_id": session_id,
            "error": "Failed to get snapshot (reactor finished or lock poisoned)",
        }))),
    }
}

/// 查询审计链自动验证状态
async fn session_auto_verify_get(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "auto_verify": session.is_auto_verify_enabled(),
    })))
}

/// 设置审计链自动验证配置
async fn session_auto_verify_post(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
    Json(req): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    let enabled = req
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let threshold = req.get("threshold").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let interval = req.get("interval").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
    session.set_auto_verify(enabled, threshold, interval);

    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "success": true,
        "auto_verify": enabled,
        "threshold": threshold,
        "interval": interval,
        "message": format!("Auto-verify {}", if enabled { "enabled" } else { "disabled" }),
    })))
}

/// IoResponse 外部提交 handler
///
/// `POST /api/sessions/:id/io_response` → 外部应用提交 IoResponse，
/// 允许外部系统通过 HTTP API 异步返回 I/O 执行结果。
///
/// 请求体格式：
/// ```json
/// {
/// "request_id": 123,
/// "result": {"content": "response data"},
/// "error": null
/// }
/// ```
async fn session_io_response(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
    Json(req): Json<IoResponseRequest>,
) -> Result<Json<ApiResponse>, StatusCode> {
    let id = api.next_id();
    let request_id = evorule_reactor::FactId(req.request_id);
    let result = serde_to_tcb(req.result);

    let sessions = api.sessions.lock().await;
    sessions.touch_session(session_id);
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    match session.command_tx.send(Fact::IoResponse {
        id,
        request_id,
        result,
        error: req.error,
    }) {
        Ok(()) => {
            tracing::info!(
                session_id,
                request_id = req.request_id,
                "IoResponse submitted externally"
            );
            Ok(Json(ApiResponse {
                success: true,
                message: "IoResponse submitted".to_string(),
                fact_id: Some(id.0),
            }))
        }
        Err(_) => Ok(Json(ApiResponse {
            success: false,
            message: "Command channel closed (reactor exited)".to_string(),
            fact_id: None,
        })),
    }
}

// cluster 相关 handler 已移除（多 reactor 协作原语，应用层功能）

/// 治理层 HTTP 服务器
///
/// 支持两套路由：
/// - 单反应器模式（`/api/command`、`/api/state` 等，向后兼容）
/// - 多会话模式（`/api/sessions/*`，配合长驻反应器和 SSE 事件流）
pub struct GovernanceServer {
    state: AppState,
    auth: AuthConfig,
    /// 监听地址（保留用于诊断/显示，实际绑定在 main.rs 中完成）
    #[allow(dead_code)]
    addr: String,
    /// 速率限制：每 IP 持续速率（req/s）。0 表示禁用限速。
    rate_limit_per_sec: u64,
    /// 速率限制：令牌桶容量（突发上限）
    rate_limit_burst: u32,
    /// CORS 允许的 Origin 白名单
    ///
    /// - 空列表：只允许同源请求（`AllowOrigin::default()` 不允许任何跨域）
    /// - 非空列表：只允许列表中的 Origin 通过。列表元素示例：`"http://localhost:3000"`
    allowed_origins: Arc<Vec<String>>,
    /// S2：/metrics 端点是否需要认证（默认 false，Prometheus scraper 通常不带 token）
    metrics_requires_auth: bool,
}

impl GovernanceServer {
    /// 创建新服务器
    ///
    /// # 参数
    /// - `state`：应用全局状态（合并 GovernanceApi + SessionApi）
    /// - `auth`：认证配置
    /// - `addr`：监听地址（如 "0.0.0.0:8080"）
    /// - `rate_limit_per_sec`：每 IP 持续速率（req/s），`0` = 禁用
    /// - `rate_limit_burst`：突发上限（令牌桶容量）
    /// - `allowed_origins`：CORS 允许的 Origin 白名单（空=严格同源，非空=白名单）
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        state: AppState,
        auth: AuthConfig,
        addr: String,
        rate_limit_per_sec: u64,
        rate_limit_burst: u32,
        allowed_origins: Vec<String>,
        metrics_requires_auth: bool,
    ) -> Self {
        Self {
            state,
            auth,
            addr,
            rate_limit_per_sec,
            rate_limit_burst,
            allowed_origins: Arc::new(allowed_origins),
            metrics_requires_auth,
        }
    }

    /// 创建禁用认证的开发服务器（保留默认限速 200 req/s）
    #[allow(dead_code)]
    pub fn dev(state: AppState, addr: String) -> Self {
        Self::new(
            state,
            AuthConfig::disabled(),
            addr,
            1,
            200,
            vec![
                "http://localhost:3000".to_string(),
                "https://localhost:3000".to_string(),
                "http://127.0.0.1:3000".to_string(),
            ],
            false,
        )
    }

    /// 创建禁用认证 + 禁用限速的基准测试服务器（仅用于 benchmarks）
    #[allow(dead_code)]
    pub fn bench(state: AppState, addr: String) -> Self {
        // per_sec=0 触发 build_router() 完全跳过 GovernorLayer（真正禁用限速）
        Self::new(state, AuthConfig::disabled(), addr, 0, 0, vec![], false)
    }

    /// 构建路由（公开，供 bin 自定义启动流程使用）
    ///
    /// # 安全层（从内到外）
    /// 1. `auth_middleware` — Bearer token 认证
    /// 2. `RequestBodyLimitLayer` — 请求体大小限制（1MB）
    /// 3. `ConcurrencyLimitLayer` — 并发连接数限制（1000）
    /// 4. `CorsLayer` — CORS 预检处理
    /// 5. `GovernorLayer` — 速率限制（每 IP `rate_limit_burst / rate_limit_per_sec` req/s）
    ///
    /// # 注意
    /// `GovernorLayer` 依赖 `ConnectInfo<SocketAddr>` 提取客户端 IP，
    /// 因此 bin 启动时必须使用 `into_make_service_with_connect_info::<SocketAddr>()`。
    ///
    /// # tower-governor 参数语义
    /// `per_second` 是令牌桶补充周期（秒），每周期补充 `burst_size` 个令牌。
    /// 持续速率 = burst_size / per_second（req/s）。
    /// burst_size 同时是桶的最大容量（突发上限）。
    // axum Router 多 route 集中配置, 拆函数需共享 AppState。详见 GATE_REFERENCE.md §六(豁免索引)
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
    pub fn build_router(&self) -> Router {
        let auth = self.auth.clone();

        // 速率限制配置（令牌桶：每 period 秒补充 burst 个令牌）
        // rate_limit_per_sec == 0 表示完全禁用限速（不添加 GovernorLayer）
        // 修复：之前用 (1, 1_000_000) 模拟"无限速"，但 GovernorConfigBuilder::finish()
        // 可能 fallback 到 GovernorConfig::default()（默认低限速），导致 --no-rate-limit
        // 实际仍触发 429。现在通过 resolve_governor_config() 条件性返回 None 来跳过 GovernorLayer。
        //
        // 注意：GovernorLayer 不能存入 Option<GovernorLayer> 变量（其 M/RespBody 泛型
        // 只能在 .layer() 调用时通过 Layer trait 约束推断），因此采用 match 分支。

        // 公开路由（免认证）— health/liveness/readiness/metrics
        // /api/rules/validate 从 protected_routes 移到 public_routes（仅读 JSON，不改状态）
        // /api/rules/reload 是运营操作，但仍放在 public_routes 里（由部署环境的
        // 网络层防火墙限制访问来源，evorule-server 作为框架层不做 RBAC 强制）
        // H6: 显式指定 Router 状态类型为 AppState，消除 SharedMetrics 的 FromRef 歧义
        //（Arc<dyn IoMetrics> 既满足 FromRef<AppState>，又满足 blanket impl FromRef<T> for T: Clone）
        let public_routes = Router::<AppState>::new()
            .route("/api/health", get(health))
            .route("/api/health/liveness", get(liveness))
            .route("/api/health/readiness", get(readiness))
            .route("/api/rules/validate", post(validate_rules_handler));

        // 受保护路由（需认证）
        let protected_routes = Router::new()
            // 单反应器模式路由（向后兼容）
            .route("/api/command", post(submit_command))
            .route("/api/payload", post(update_payload))
            .route("/api/state", get(get_state))
            .route("/api/audit", get(get_audit))
            // 多会话模式路由
            .route("/api/sessions", post(create_session).get(list_sessions))
            .route(
                "/api/sessions/from/{parent_id}",
                post(create_session_from_parent),
            )
            .route("/api/sessions/fork/{parent_id}", post(create_session_fork))
            .route("/api/sessions/{id}", delete(close_session))
            .route("/api/sessions/{id}/command", post(session_command))
            .route("/api/sessions/{id}/state", get(session_state))
            .route("/api/sessions/{id}/audit", get(session_audit))
            .route("/api/sessions/{id}/audit/verify", get(session_audit_verify))
            .route("/api/sessions/{id}/audit/export", get(session_audit_export))
            .route(
                "/api/sessions/{id}/audit/import",
                post(session_audit_import),
            )
            .route(
                "/api/sessions/{id}/audit/export/compressed",
                get(session_audit_export_compressed),
            )
            .route(
                "/api/sessions/{id}/audit/import/compressed",
                post(session_audit_import_compressed),
            )
            .route(
                "/api/sessions/{id}/audit/causal/{fact_id}",
                get(session_causal_chain),
            )
            .route("/api/sessions/{id}/payload", post(session_payload))
            .route("/api/sessions/{id}/events", get(session_events))
            .route("/api/sessions/{id}/io_response", post(session_io_response))
            // 治理层演进 API（回放、时间旅行、集群协作）
            .route("/api/sessions/{id}/replay", get(session_replay))
            .route("/api/sessions/{id}/history", get(session_history))
            .route("/api/sessions/{id}/rewind", get(session_rewind))
            .route("/api/sessions/{id}/diff", get(session_diff))
            .route("/api/sessions/{id}/facts", get(session_facts_by_prefix))
            .route("/api/shared/facts", get(shared_facts_by_prefix))
            .route(
                "/api/shared/facts/{fact_id}/source",
                get(shared_fact_source),
            )
            .route(
                "/api/shared/facts/{fact_id}/used_by",
                get(get_sessions_using_fact),
            )
            .route(
                "/api/sessions/{id}/used_at_startup",
                post(record_used_at_startup),
            )
            .route(
                "/api/sessions/{id}/used_at_startup",
                get(get_used_at_startup),
            )
            .route("/api/sessions/{id}/debug/phase", get(debug_phase))
            .route("/api/sessions/{id}/debug/queue", get(debug_queue))
            .route("/api/sessions/{id}/debug/pending_io", get(debug_pending_io))
            .route("/api/sessions/{id}/interrupt", post(session_interrupt))
            .route("/api/sessions/{id}/finished", get(session_finished))
            .route("/api/sessions/{id}/causal_depth", get(session_causal_depth))
            .route("/api/sessions/{id}/invariants", get(session_invariants))
            .route(
                "/api/sessions/{id}/pending_io_count",
                get(session_pending_io_count),
            )
            .route("/api/sessions/{id}/step", get(session_step))
            .route("/api/sessions/{id}/snapshot", get(session_snapshot))
            .route(
                "/api/sessions/{id}/audit/auto_verify",
                get(session_auto_verify_get),
            )
            .route(
                "/api/sessions/{id}/audit/auto_verify",
                post(session_auto_verify_post),
            )
            // B2 修复：reload 从 public_routes 移到 protected_routes。
            // 该端点会重新加载 core_eval.json + rules_dir，是运营操作，
            // 未认证用户不应触发（DoS 风险 + rules_dir 可写时注入恶意规则）。
            .route("/api/rules/reload", post(reload_rules_handler))
            // P10: 工作空间 + 规则元数据路由 (18 个端点, 受认证保护)
            .merge(evorule_workspace::build_workspace_router())
            // rewind/diff 已移至 application/core/time_machine（本地实现）
            .layer(axum::middleware::from_fn_with_state(
                auth,
                auth_middleware_wrapper,
            ));

        // CORS 白名单（从 self.allowed_origins 构建）
        //
        // 策略：
        // - 列表非空 → 只允许列表中的 Origin（精确匹配）
        // - 列表为空 → 严格拒绝跨域（不允许任何外部 Origin）
        let origins_cloned: Vec<String> = (*self.allowed_origins).clone();
        // S3：检测通配符 origin，warn 提示浏览器兼容性问题
        if origins_cloned.iter().any(|o| o == "*") {
            tracing::warn!(
                "CORS 配置包含通配符 '*'。结合 allow_credentials(true)，\
                 浏览器会拒绝此响应（CORS 规范禁止通配符 + credentials）。\
                 请使用精确 Origin 列表（如 https://app.example.com）替代。"
            );
        }
        let cors = if origins_cloned.is_empty() {
            // 严格同源模式：不暴露任何 CORS 响应头，浏览器自动拒绝跨域。
            // 仍显式声明 methods/headers 以防空 Origin 的边缘场景。
            CorsLayer::new()
                .allow_methods([
                    Method::GET,
                    Method::POST,
                    Method::PUT,
                    Method::DELETE,
                    Method::OPTIONS,
                ])
                .allow_headers([
                    axum::http::header::CONTENT_TYPE,
                    axum::http::header::AUTHORIZATION,
                    axum::http::header::ACCEPT,
                ])
        } else {
            let mut header_vals: Vec<axum::http::HeaderValue> = Vec::new();
            for o in &origins_cloned {
                match axum::http::HeaderValue::from_str(o) {
                    Ok(v) => header_vals.push(v),
                    Err(e) => {
                        tracing::warn!("跳过无效 CORS Origin: {} → {}", o, e);
                    }
                }
            }
            CorsLayer::new()
                .allow_methods([
                    Method::GET,
                    Method::POST,
                    Method::PUT,
                    Method::DELETE,
                    Method::OPTIONS,
                ])
                .allow_headers([
                    axum::http::header::CONTENT_TYPE,
                    axum::http::header::AUTHORIZATION,
                    axum::http::header::ACCEPT,
                ])
                .allow_origin(header_vals)
                .allow_credentials(true)
        };

        // 合并路由 + 全局安全层（从内到外：body limit → concurrency → cors → rate limit）
        // 修复：当 resolve_governor_config() 返回 None 时，完全跳过 GovernorLayer（真正禁用限速）
        // S2：/metrics 根据 metrics_requires_auth 决定是否需要认证
        // 独立构建 metrics_router，避免改动 public/protected 路由分组的结构
        let metrics_router = Router::<AppState>::new().route("/metrics", get(metrics_handler));
        let metrics_router = if self.metrics_requires_auth {
            metrics_router.layer(axum::middleware::from_fn_with_state(
                self.auth.clone(),
                auth_middleware_wrapper,
            ))
        } else {
            metrics_router
        };

        let router = Router::new()
            .merge(public_routes)
            .merge(protected_routes)
            .merge(metrics_router)
            .layer(axum::middleware::from_fn_with_state(
                self.state.metrics.clone(),
                http_metrics_middleware,
            ))
            .layer(RequestBodyLimitLayer::new(MAX_REQUEST_BODY_BYTES))
            .layer(tower::limit::ConcurrencyLimitLayer::new(MAX_CONCURRENCY))
            .layer(cors);

        match resolve_governor_config(self.rate_limit_per_sec, self.rate_limit_burst) {
            None => {
                tracing::info!("速率限制已禁用（--no-rate-limit / per_sec=0）");
                router.with_state(self.state.clone())
            }
            Some(cfg) => {
                tracing::info!(
                    "速率限制已启用：{} req/s（burst={}）",
                    self.rate_limit_burst as u64 / self.rate_limit_per_sec,
                    self.rate_limit_burst
                );
                router
                    .layer(tower_governor::GovernorLayer::new(cfg))
                    .with_state(self.state.clone())
            }
        }
    }

    /// 启动 HTTP 服务器
    ///
    /// 使用 `into_make_service_with_connect_info::<SocketAddr>()` 注入客户端 IP，
    /// 以支持 `GovernorLayer`（速率限制）的按 IP 限流。
    #[allow(dead_code)]
    pub async fn serve(self) -> Result<(), std::io::Error> {
        // H6: 此方法为预留 API（main.rs 使用 build_router() + axum::serve 自行启动以支持优雅退出）
        let router = self.build_router();
        let listener = tokio::net::TcpListener::bind(&self.addr).await?;
        tracing::info!("Governance HTTP server listening on {}", self.addr);
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
    }
}

// ====================================================================
// 规则校验 API
// ====================================================================

/// 请求体：待校验的规则 JSON
#[derive(serde::Deserialize)]
struct ValidateRulesRequest {
    /// 规则 JSON 字符串（支持三种格式：{transform:[...]}/[...]/{...}）
    rules: String,
}

/// 规则校验端点
///
/// 接收 JSON 规则字符串，返回静态验证 + 安全分析结果。
///
/// # 请求格式
/// ```json
/// {"rules": "{\"transform\":[{\"type\":\"noop\"}]}"}
/// ```
///
/// # 响应格式
/// ```json
/// {
/// "passed": true,
/// "static_validation": { "checks": [...], "error_count": 0, "warn_count": 0 },
/// "security_analysis": { "checks": [...], "risk_count": 0, "risk_level": "low" },
/// "summary": { "total_transforms": 1, "total_errors": 0, "total_warnings": 0, "total_risks": 0 }
/// }
/// ```
async fn validate_rules_handler(
    Json(req): Json<ValidateRulesRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)> {
    match evorule_governance::rule_validation::validate_rules_from_json(&req.rules) {
        Ok(result) => {
            let json = serde_json::to_value(&result).unwrap_or_default();
            let status = if result.passed {
                StatusCode::OK
            } else {
                StatusCode::UNPROCESSABLE_ENTITY
            };
            Ok((status, Json(json)))
        }
        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": e,
                "passed": false
            })),
        )),
    }
}

// ============================================================================
// POST /api/rules/reload — 重新从磁盘加载 TCB 宪法 + 业务规则
// ============================================================================

/// 规则热重载响应
#[derive(Debug, serde::Serialize)]
struct RulesReloadedResponse {
    reload_ok: bool,
    previous_rules: usize,
    current_rules: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// 规则热重载 handler
///
/// - 请求：空（`POST /api/rules/reload`，可传空 body `{}`）
/// - 成功：`200 {"reload_ok":true,"previous_rules":N,"current_rules":M}`
/// - 失败：`500 {"reload_ok":false,"previous_rules":N,"current_rules":N,"error":"..."}`（旧规则保留）
async fn reload_rules_handler(
    State(sessions): State<SessionApi>,
) -> Result<(StatusCode, Json<RulesReloadedResponse>), (StatusCode, Json<RulesReloadedResponse>)> {
    let old_len = sessions.core_eval_len();
    match sessions.reload_from_disk().await {
        Ok((old, new_len)) => Ok((
            StatusCode::OK,
            Json(RulesReloadedResponse {
                reload_ok: true,
                previous_rules: old,
                current_rules: new_len,
                error: None,
            }),
        )),
        Err(e) => {
            tracing::error!("rules reload failed: {}", e);
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(RulesReloadedResponse {
                    reload_ok: false,
                    previous_rules: old_len,
                    current_rules: old_len,
                    error: Some(e),
                }),
            ))
        }
    }
}

/// 速率限制配置决策（纯函数，可单元测试）
///
/// 根据 `per_sec` 和 `burst` 参数构造 `GovernorConfig`，决定是否启用限速。
///
/// # 参数
/// - `per_sec`：令牌桶补充周期（秒）。`0` 表示禁用限速。
/// - `burst`：令牌桶容量（突发上限）。
///
/// # 返回
/// - `None`：禁用限速（调用方不应添加 `GovernorLayer`）
/// - `Some(cfg)`：启用限速，使用返回的配置构造 `GovernorLayer`
///
/// # 设计理由
/// `GovernorConfigBuilder::finish()` 可能返回 `None`，旧代码用
/// `unwrap_or_else(GovernorConfig::default)` fallback，但 `default()` 的限速值
/// 很低，会导致 `--no-rate-limit` 名义禁用、实际仍强限速的 bug。
/// 抽取为独立函数后，`per_sec == 0` 路径直接返回 `None`，彻底绕过 fallback 陷阱。
pub fn resolve_governor_config(
    per_sec: u64,
    burst: u32,
) -> Option<
    tower_governor::governor::GovernorConfig<
        tower_governor::key_extractor::PeerIpKeyExtractor,
        governor::middleware::NoOpMiddleware,
    >,
> {
    if per_sec == 0 {
        return None;
    }
    tower_governor::governor::GovernorConfigBuilder::default()
        .per_second(per_sec)
        .burst_size(burst)
        .finish()
        .or_else(|| {
            tracing::warn!(
                "GovernorConfigBuilder::finish() 返回 None，fallback 到 GovernorConfig::default()"
            );
            Some(tower_governor::governor::GovernorConfig::default())
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::panic, clippy::expect_used)]
    use super::*;

    // ===== resolve_governor_config 单元测试 =====
    // 覆盖 030 文档记录的 --no-rate-limit 修复场景

    #[test]
    fn test_resolve_governor_config_disabled_when_per_sec_zero() {
        // 本次 bug 的核心场景：--no-rate-limit → per_sec=0 → 必须返回 None
        let result = resolve_governor_config(0, 200);
        assert!(
            result.is_none(),
            "per_sec=0 必须返回 None（禁用限速），实际返回: {result:?}"
        );
    }

    #[test]
    fn test_resolve_governor_config_disabled_when_both_zero() {
        // bench() 路径：per_sec=0, burst=0 → 必须返回 None
        let result = resolve_governor_config(0, 0);
        assert!(
            result.is_none(),
            "per_sec=0 且 burst=0 必须返回 None，实际返回: {result:?}"
        );
    }

    #[test]
    fn test_resolve_governor_config_enabled_normal() {
        // 默认配置：per_sec=1, burst=200 → 必须返回 Some
        let result = resolve_governor_config(1, 200);
        assert!(
            result.is_some(),
            "per_sec=1, burst=200 必须返回 Some（启用限速）"
        );
    }

    #[test]
    fn test_resolve_governor_config_enabled_small_burst() {
        // 边界值：per_sec=1, burst=1 → 仍应返回 Some（最小有效限速配置）
        let result = resolve_governor_config(1, 1);
        assert!(
            result.is_some(),
            "per_sec=1, burst=1 必须返回 Some（最小有效限速配置）"
        );
    }

    // ===== 既有单元测试 =====

    #[test]
    fn test_serde_to_tcb_roundtrip() {
        let original = serde_json::json!({
            "name": "test",
            "count": 42,
            "active": true,
            "items": [1, 2, 3]
        });
        let tcb = serde_to_tcb(original.clone());
        let back = tcb_to_serde(&tcb);
        assert_eq!(back, original);
    }

    #[test]
    fn test_tcb_to_serde_null() {
        let tcb = JsonValue::Null;
        let serde = tcb_to_serde(&tcb);
        assert!(serde.is_null());
    }

    #[test]
    fn test_tcb_to_serde_nested() {
        let mut inner = std::collections::BTreeMap::new();
        inner.insert("key".to_string(), JsonValue::String("value".to_string()));
        let tcb = JsonValue::Object(inner);

        let mut expected = serde_json::Map::new();
        expected.insert(
            "key".to_string(),
            serde_json::Value::String("value".to_string()),
        );

        let serde = tcb_to_serde(&tcb);
        assert_eq!(serde, serde_json::Value::Object(expected));
    }

    #[test]
    fn test_fact_to_sse_data_command() {
        let mut params = std::collections::BTreeMap::new();
        params.insert("attr".to_string(), JsonValue::string("x"));
        params.insert("delta".to_string(), JsonValue::Integer(5));
        let mut instr = std::collections::BTreeMap::new();
        instr.insert("type".to_string(), JsonValue::string("increment"));
        instr.insert("params".to_string(), JsonValue::Object(params));

        let fact = Fact::Command {
            id: FactId(1),
            instruction: JsonValue::Object(instr),
        };
        let json: serde_json::Value = serde_json::from_str(&fact_to_sse_data(&fact)).unwrap();
        assert_eq!(json["type"], "Command");
        assert_eq!(json["id"], 1);
        assert_eq!(json["instruction"]["type"], "increment");
    }

    #[test]
    fn test_fact_to_sse_data_stable() {
        let mut payload = std::collections::BTreeMap::new();
        payload.insert("x".to_string(), JsonValue::Integer(5));
        let fact = Fact::Stable {
            id: FactId(10),
            final_snapshot: JsonValue::Object(payload),
        };
        let json: serde_json::Value = serde_json::from_str(&fact_to_sse_data(&fact)).unwrap();
        assert_eq!(json["type"], "Stable");
        assert_eq!(json["id"], 10);
        assert_eq!(json["final_snapshot"]["x"], 5);
    }

    #[test]
    fn test_fact_to_sse_data_error() {
        let fact = Fact::Error {
            id: FactId(99),
            message: "max rounds exceeded".to_string(),
        };
        let json: serde_json::Value = serde_json::from_str(&fact_to_sse_data(&fact)).unwrap();
        assert_eq!(json["type"], "Error");
        assert_eq!(json["id"], 99);
        assert_eq!(json["message"], "max rounds exceeded");
    }

    #[test]
    fn test_fact_to_sse_data_io_request() {
        use evorule_reactor::IoType;
        let fact = Fact::IoRequest {
            id: FactId(3),
            cause: FactId(1),
            io_type: IoType::call_external(),
            params: JsonValue::Null,
        };
        let json: serde_json::Value = serde_json::from_str(&fact_to_sse_data(&fact)).unwrap();
        assert_eq!(json["type"], "IoRequest");
        assert_eq!(json["id"], 3);
        assert_eq!(json["cause"], 1);
        assert_eq!(json["io_type"], "call_external");
    }

    #[test]
    fn test_fact_to_sse_data_payload_update() {
        let fact = Fact::PayloadUpdate {
            id: FactId(5),
            path: "x".to_string(),
            value: JsonValue::Integer(42),
        };
        let json: serde_json::Value = serde_json::from_str(&fact_to_sse_data(&fact)).unwrap();
        assert_eq!(json["type"], "PayloadUpdate");
        assert_eq!(json["id"], 5);
        assert_eq!(json["path"], "x");
        assert_eq!(json["value"], 42);
    }

    // ====================================================================
    // /api/rules/validate 端点测试
    // ====================================================================
    // 直接调用 validate_rules_handler,覆盖所有异常分支:
    // - 400 BAD_REQUEST: JSON 解析失败
    // - 422 UNPROCESSABLE_ENTITY: 静态验证失败
    // - 200 OK: 验证通过(含 warn 级别不阻断的情况)

    /// 辅助函数: 调用 handler 并返回 (状态码, 响应体)
    async fn call_validate(
        rules: &str,
    ) -> Result<(StatusCode, serde_json::Value), (StatusCode, serde_json::Value)> {
        let req = ValidateRulesRequest {
            rules: rules.to_string(),
        };
        match validate_rules_handler(Json(req)).await {
            Ok((status, json)) => Ok((status, json.0)),
            Err((status, json)) => Err((status, json.0)),
        }
    }

    // --- 400 BAD_REQUEST: JSON 解析失败 ---

    #[tokio::test]
    async fn test_validate_invalid_json_syntax() {
        // 无效 JSON 语法(花括号不匹配)
        let result = call_validate(r#"{"transform":[invalid}"#).await;
        assert!(result.is_err(), "无效 JSON 语法应返回 Err(400)");
        let (status, body) = result.unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["passed"], false);
        assert!(body["error"].as_str().unwrap().contains("JSON 解析失败"));
    }

    #[tokio::test]
    async fn test_validate_empty_string() {
        // 空字符串不是合法 JSON
        let result = call_validate("").await;
        assert!(result.is_err());
        let (status, _) = result.unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_validate_json_null() {
        // JSON null 既不是对象也不是数组
        let result = call_validate("null").await;
        assert!(result.is_err());
        let (status, body) = result.unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("对象或数组"));
    }

    #[tokio::test]
    async fn test_validate_json_primitive() {
        // JSON 原始值(数字)不是对象或数组
        let result = call_validate("12345").await;
        assert!(result.is_err());
        let (status, _) = result.unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_validate_json_string() {
        // JSON 字符串不是对象或数组
        let result = call_validate(r#""hello""#).await;
        assert!(result.is_err());
        let (status, _) = result.unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_validate_json_boolean() {
        // JSON 布尔值不是对象或数组
        let result = call_validate("true").await;
        assert!(result.is_err());
        let (status, _) = result.unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    // --- 422 UNPROCESSABLE_ENTITY: 静态验证失败 ---

    #[tokio::test]
    async fn test_validate_empty_transform_array() {
        // transform 数组为空
        let (status, body) = call_validate(r#"{"transform":[]}"#).await.unwrap();
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["passed"], false);
        // 应有 "non_empty" 校验项报错
        let checks = &body["static_validation"]["checks"];
        assert!(checks
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "non_empty" && c["passed"] == false));
    }

    #[tokio::test]
    async fn test_validate_missing_type_field() {
        // transform 缺少 type 字段
        let (status, body) = call_validate(r#"{"transform":[{"params":{"attr":"x"}}]}"#)
            .await
            .unwrap();
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["passed"], false);
        assert!(body["summary"]["total_errors"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn test_validate_unknown_type() {
        // type 不在白名单中
        let (status, body) = call_validate(r#"{"transform":[{"type":"unknown_type"}]}"#)
            .await
            .unwrap();
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["passed"], false);
        // 应有 "type_valid" 校验项报错
        let checks = &body["static_validation"]["checks"];
        assert!(checks
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "type_valid" && c["passed"] == false));
    }

    #[tokio::test]
    async fn test_validate_set_missing_required_params() {
        // set 指令缺少必填参数 (需要 attr/operation/value,只提供 attr)
        let (status, body) =
            call_validate(r#"{"transform":[{"type":"set","params":{"attr":"x"}}]}"#)
                .await
                .unwrap();
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["passed"], false);
        // params_complete 应报错
        let checks = &body["static_validation"]["checks"];
        assert!(checks
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "params_complete" && c["level"] == "error"));
    }

    #[tokio::test]
    async fn test_validate_branch_missing_domain() {
        // branch 指令缺少必填参数 domain
        let (status, body) = call_validate(r#"{"transform":[{"type":"branch"}]}"#)
            .await
            .unwrap();
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["passed"], false);
    }

    #[tokio::test]
    async fn test_validate_io_request_missing_io_type() {
        // io_request 指令缺少必填参数 io_type
        let (status, body) = call_validate(r#"{"transform":[{"type":"io_request"}]}"#)
            .await
            .unwrap();
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["passed"], false);
    }

    #[tokio::test]
    async fn test_validate_push_missing_instructions() {
        // push 指令缺少必填参数 instructions
        let (status, body) = call_validate(r#"{"transform":[{"type":"push"}]}"#)
            .await
            .unwrap();
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["passed"], false);
    }

    #[tokio::test]
    async fn test_validate_increment_missing_delta() {
        // increment 指令缺少必填参数 delta
        let (status, body) =
            call_validate(r#"{"transform":[{"type":"increment","params":{"attr":"x"}}]}"#)
                .await
                .unwrap();
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["passed"], false);
    }

    #[tokio::test]
    async fn test_validate_io_request_params_too_large() {
        // io_request 的 params 超过 10KB 上限
        // 构造一个约 11KB 的大字符串作为 io_type 的值
        let large_value = "x".repeat(11000);
        let rules = serde_json::json!({
            "transform": [{
                "type": "io_request",
                "params": {
                    "io_type": "call_service",
                    "data": large_value
                }
            }]
        })
        .to_string();
        let (status, body) = call_validate(&rules).await.unwrap();
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["passed"], false);
        // params_complete 应报 params 过大
        let checks = &body["static_validation"]["checks"];
        assert!(checks
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "params_complete" && c["level"] == "error"));
    }

    #[tokio::test]
    async fn test_validate_transform_count_exceeds_limit() {
        // transform 数量超过 64 条上限
        // 用 noop 填充 65 条
        let transforms: Vec<serde_json::Value> = (0..65)
            .map(|_| serde_json::json!({"type": "noop"}))
            .collect();
        let rules = serde_json::json!({"transform": transforms}).to_string();
        let (status, body) = call_validate(&rules).await.unwrap();
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["passed"], false);
        // transform_count_limit 应报错
        let checks = &body["static_validation"]["checks"];
        assert!(checks
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "transform_count_limit" && c["passed"] == false));
    }

    // --- 200 OK: 验证通过 ---

    #[tokio::test]
    async fn test_validate_valid_noop() {
        // 合法 noop 指令
        let (status, body) = call_validate(r#"{"transform":[{"type":"noop"}]}"#)
            .await
            .unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["passed"], true);
        assert_eq!(body["summary"]["total_errors"], 0);
        assert_eq!(body["summary"]["total_transforms"], 1);
    }

    #[tokio::test]
    async fn test_validate_valid_set() {
        // 合法 set 指令
        let (status, body) = call_validate(
            r#"{"transform":[{"type":"set","params":{"attr":"x","operation":"set","value":1}}]}"#,
        )
        .await
        .unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["passed"], true);
        assert_eq!(body["summary"]["total_errors"], 0);
    }

    #[tokio::test]
    async fn test_validate_valid_branch() {
        // 合法 branch 指令(含 on_true/on_false)
        let (status, body) = call_validate(
            r#"{"transform":[{"type":"branch","params":{"domain":"check","on_true":[{"type":"noop"}],"on_false":[{"type":"noop"}]}}]}"#,
        )
        .await
        .unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["passed"], true);
    }

    #[tokio::test]
    async fn test_validate_valid_io_request() {
        // 合法 io_request 指令
        let (status, body) = call_validate(
            r#"{"transform":[{"type":"io_request","params":{"io_type":"call_service"}}]}"#,
        )
        .await
        .unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["passed"], true);
    }

    #[tokio::test]
    async fn test_validate_single_transform_object() {
        // 单条 transform 对象(非数组,非标准 transform 包装)
        let (status, body) = call_validate(r#"{"type":"noop"}"#).await.unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["passed"], true);
        assert_eq!(body["summary"]["total_transforms"], 1);
    }

    #[tokio::test]
    async fn test_validate_top_level_array() {
        // 顶层数组格式
        let (status, body) = call_validate(r#"[{"type":"noop"},{"type":"noop"}]"#)
            .await
            .unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["passed"], true);
        assert_eq!(body["summary"]["total_transforms"], 2);
    }

    #[tokio::test]
    async fn test_validate_invalid_operation_warn_not_blocking() {
        // set 的 operation 不在合法值中 → warn 级别,不阻断 passed
        let (status, body) = call_validate(
            r#"{"transform":[{"type":"set","params":{"attr":"x","operation":"invalid_op","value":1}}]}"#,
        )
        .await
        .unwrap();
        assert_eq!(status, StatusCode::OK, "warn 不阻断,应返回 200");
        assert_eq!(body["passed"], true);
        // 但应有 warn 计数
        assert!(
            body["static_validation"]["warn_count"].as_u64().unwrap() > 0,
            "应有 warn 级别校验项"
        );
    }

    #[tokio::test]
    async fn test_validate_multiple_errors() {
        // 多条规则各有不同错误,验证 error 计数正确
        let (status, body) = call_validate(
            r#"{"transform":[
                {"type":"unknown_type"},
                {"params":{}},
                {"type":"set","params":{"attr":"x"}}
            ]}"#,
        )
        .await
        .unwrap();
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["passed"], false);
        // 至少 3 个 error(unknown type + missing type + missing params)
        assert!(
            body["summary"]["total_errors"].as_u64().unwrap() >= 3,
            "应有至少 3 个 error"
        );
    }

    #[tokio::test]
    async fn test_validate_response_structure() {
        // 验证响应体的完整结构(所有必需字段都存在)
        let (status, body) = call_validate(r#"{"transform":[{"type":"noop"}]}"#)
            .await
            .unwrap();
        assert_eq!(status, StatusCode::OK);
        // 顶层字段
        assert!(body["passed"].is_boolean());
        assert!(body["static_validation"].is_object());
        assert!(body["security_analysis"].is_object());
        assert!(body["summary"].is_object());
        // static_validation 子字段
        assert!(body["static_validation"]["checks"].is_array());
        assert!(body["static_validation"]["error_count"].is_u64());
        assert!(body["static_validation"]["warn_count"].is_u64());
        // security_analysis 子字段
        assert!(body["security_analysis"]["checks"].is_array());
        assert!(body["security_analysis"]["risk_count"].is_u64());
        assert!(body["security_analysis"]["risk_level"].is_string());
        // summary 子字段
        assert!(body["summary"]["total_transforms"].is_u64());
        assert!(body["summary"]["total_errors"].is_u64());
        assert!(body["summary"]["total_warnings"].is_u64());
        assert!(body["summary"]["total_risks"].is_u64());
    }

    #[tokio::test]
    async fn test_validate_error_response_structure() {
        // 验证错误响应体结构(400 分支)
        let result = call_validate("not_json").await;
        assert!(result.is_err());
        let (_, body) = result.unwrap_err();
        assert_eq!(body["passed"], false);
        assert!(body["error"].is_string());
    }

    // ====================================================================
    // Handler oneshot 测试（通过真实 build_router 验证端到端）
    // ====================================================================
    // 使用 GovernanceServer::bench() 构建无认证、无限速的路由，
    // 通过 tower::ServiceExt::oneshot 发送请求并检查响应。
    // 覆盖路由注册、FromRef 状态提取、中间件链、handler 响应格式。

    use crate::metrics_impl::shared_prometheus_metrics;
    use evorule_reactor::Reactor;
    use tower::ServiceExt;

    /// 构造测试用 AppState（最小依赖，无需加载 core_eval.json）
    ///
    /// 返回 `(AppState, ReadinessFlag)`，flag 可用于测试 readiness 端点的两种状态。
    ///
    /// 内部 spawn 的反应器在 rx/event_tx/handle drop 后仍会运行（孤儿任务）：
    /// - ReactorHandle 无 Drop impl，丢弃不会 abort 任务
    /// - emit_fact 对无接收者的 broadcast send 已优雅处理（debug 日志，不 panic）
    /// - 孤儿任务在 `#[tokio::test]` 运行时 drop 时被自动取消
    fn make_test_state() -> (AppState, ReadinessFlag) {
        let mut instr = std::collections::BTreeMap::new();
        instr.insert("type".to_string(), JsonValue::string("noop"));
        let core_eval = vec![JsonValue::Object(instr)];

        let reactor = Reactor::builder(core_eval.clone()).max_rounds(100).build();
        let (tx, _rx, _event_tx, _handle, facts_log) = reactor.spawn();

        let auditor = Auditor::new(facts_log.clone());
        let governance = GovernanceApi::new(tx, facts_log, auditor);
        let sessions = SessionApi::new(core_eval, 100);
        let metrics: SharedMetrics = shared_prometheus_metrics().unwrap();
        let readiness: ReadinessFlag = Arc::new(AtomicBool::new(true));
        let shared_facts = SharedFactsLog::new();

        // P10: 构造测试用 WorkspaceState (内存 SQLite + 桥接到 sessions)
        let ws_db = Arc::new(evorule_workspace::WorkspaceDb::in_memory().unwrap());
        let session_ops: Arc<dyn evorule_workspace::SessionOps> = Arc::new(sessions.clone());
        let ws_service = Arc::new(evorule_workspace::WorkspaceService::new(
            ws_db.clone(),
            session_ops.clone(),
        ));
        let rule_meta_service = Arc::new(evorule_workspace::RuleMetaService::new(ws_db.clone()));
        // 构造沙盒/发布/滚动 session 服务 (SANDBOX_ORCHESTRATION_DESIGN.md §3, PUBLISH_QUEUE_DESIGN.md §3)
        let switcher = evorule_workspace::SessionSwitchedBroadcaster::new();
        let sandbox_service = Arc::new(evorule_workspace::SandboxService::new(
            ws_db.clone(),
            session_ops.clone(),
        ));
        let rolling_session = evorule_workspace::RollingSessionService::new(
            ws_db.clone(),
            session_ops,
            switcher.clone(),
        );
        let publish_service = Arc::new(evorule_workspace::PublishService::new(
            ws_db.clone(),
            rolling_session,
        ));
        let verdict_service =
            Arc::new(evorule_workspace::VerdictService::new(ws_db.clone()));
        let workspace_state = evorule_workspace::WorkspaceState::new(
            ws_service,
            rule_meta_service,
            sandbox_service,
            publish_service,
            Arc::new(switcher),
            verdict_service,
        );

        (
            AppState::new(
                governance,
                sessions,
                metrics,
                readiness.clone(),
                shared_facts,
                workspace_state,
                Arc::new(InputSanitizer::with_default_rules()),
            ),
            readiness,
        )
    }

    /// 构造测试用 Router（bench 模式：无认证、无限速，适合 oneshot）
    fn make_test_router(state: &AppState) -> Router {
        GovernanceServer::bench(state.clone(), "0.0.0.0:0".to_string()).build_router()
    }

    /// 发送 oneshot JSON 请求并返回 (状态码, 响应体 JSON)
    ///
    /// 响应体非 JSON 时返回 `Value::Null`（如 `/metrics` 返回纯文本）。
    async fn oneshot_json(
        router: Router,
        method: &str,
        uri: &str,
        body: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let mut builder = axum::http::Request::builder().method(method).uri(uri);
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        let request = if let Some(b) = body {
            builder.body(axum::body::Body::from(b.to_string())).unwrap()
        } else {
            builder.body(axum::body::Body::empty()).unwrap()
        };
        let response = router.oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    // --- 公开端点（免认证） ---

    #[tokio::test]
    async fn test_health_oneshot() {
        let (state, _) = make_test_state();
        let (status, json) =
            oneshot_json(make_test_router(&state), "GET", "/api/health", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["success"], true);
        assert_eq!(json["message"], "ok");
    }

    #[tokio::test]
    async fn test_liveness_oneshot() {
        let (state, _) = make_test_state();
        let (status, json) = oneshot_json(
            make_test_router(&state),
            "GET",
            "/api/health/liveness",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["message"], "alive");
    }

    #[tokio::test]
    async fn test_readiness_ready_oneshot() {
        let (state, _) = make_test_state();
        let (status, json) = oneshot_json(
            make_test_router(&state),
            "GET",
            "/api/health/readiness",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["message"], "ready");
    }

    #[tokio::test]
    async fn test_readiness_not_ready_oneshot() {
        // 模拟优雅退出：readiness flag 设为 false → 503
        let (state, flag) = make_test_state();
        flag.store(false, std::sync::atomic::Ordering::SeqCst);
        let request = axum::http::Request::builder()
            .method("GET")
            .uri("/api/health/readiness")
            .body(axum::body::Body::empty())
            .unwrap();
        let response = make_test_router(&state).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_metrics_endpoint_oneshot() {
        let (state, _) = make_test_state();
        // 先提交一条命令触发 commands_total 计数器
        // （IntCounterVec 是惰性创建子指标，未调用 inc_commands 时 TextEncoder 不输出该指标）
        let body = r#"{"instruction":{"type":"noop"}}"#;
        let _ = oneshot_json(make_test_router(&state), "POST", "/api/command", Some(body)).await;
        // SharedMetrics 通过 Arc 共享，第二次 oneshot 看到同一份指标
        let request = axum::http::Request::builder()
            .method("GET")
            .uri("/metrics")
            .body(axum::body::Body::empty())
            .unwrap();
        let response = make_test_router(&state).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("evorule_sessions_active"));
        assert!(text.contains("evorule_commands_total"));
        assert!(text.contains("noop"), "应包含命令类型标签 type=\"noop\"");
    }

    // --- 会话管理端点 ---

    #[tokio::test]
    async fn test_list_sessions_empty_oneshot() {
        let (state, _) = make_test_state();
        let (status, json) =
            oneshot_json(make_test_router(&state), "GET", "/api/sessions", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["sessions"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn test_create_session_oneshot() {
        let (state, _) = make_test_state();
        let (status, json) =
            oneshot_json(make_test_router(&state), "POST", "/api/sessions", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(json["session_id"].is_number());
        assert_eq!(json["message"], "Session created");
    }

    #[tokio::test]
    async fn test_close_nonexistent_session_404() {
        let (state, _) = make_test_state();
        let (status, _) = oneshot_json(
            make_test_router(&state),
            "DELETE",
            "/api/sessions/9999",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_session_state_nonexistent_404() {
        let (state, _) = make_test_state();
        let (status, _) = oneshot_json(
            make_test_router(&state),
            "GET",
            "/api/sessions/9999/state",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_session_command_nonexistent_404() {
        let (state, _) = make_test_state();
        let body = r#"{"instruction":{"type":"noop"}}"#;
        let (status, _) = oneshot_json(
            make_test_router(&state),
            "POST",
            "/api/sessions/9999/command",
            Some(body),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_session_full_flow_oneshot() {
        let (state, _) = make_test_state();

        // 1. 创建会话
        let (status, json) =
            oneshot_json(make_test_router(&state), "POST", "/api/sessions", None).await;
        assert_eq!(status, StatusCode::OK);
        let session_id = json["session_id"].as_u64().unwrap();

        // 2. 查询会话状态（含 reactor 子对象）
        let uri = format!("/api/sessions/{session_id}/state");
        let (status, json) = oneshot_json(make_test_router(&state), "GET", &uri, None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(json["payload"].is_object());
        assert!(json["version"].is_number());
        assert!(json["reactor"].is_object());

        // 3. 提交命令到会话
        let uri = format!("/api/sessions/{session_id}/command");
        let body = r#"{"instruction":{"type":"noop"}}"#;
        let (status, json) = oneshot_json(make_test_router(&state), "POST", &uri, Some(body)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["success"], true);

        // 4. 关闭会话
        let uri = format!("/api/sessions/{session_id}");
        let (status, json) = oneshot_json(make_test_router(&state), "DELETE", &uri, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["message"], "Session closed");
    }

    // --- 单反应器端点（向后兼容） ---

    #[tokio::test]
    async fn test_submit_command_oneshot() {
        let (state, _) = make_test_state();
        let body = r#"{"instruction":{"type":"noop"}}"#;
        let (status, json) =
            oneshot_json(make_test_router(&state), "POST", "/api/command", Some(body)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["success"], true);
        assert!(json["fact_id"].is_number());
    }

    #[tokio::test]
    async fn test_get_state_oneshot() {
        let (state, _) = make_test_state();
        let (status, json) =
            oneshot_json(make_test_router(&state), "GET", "/api/state", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(json["payload"].is_object());
        assert!(json["version"].is_number());
    }

    #[tokio::test]
    async fn test_get_audit_oneshot() {
        let (state, _) = make_test_state();
        let (status, json) =
            oneshot_json(make_test_router(&state), "GET", "/api/audit", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(json.is_object());
    }

    // --- 规则校验端点（通过路由，含中间件链） ---

    #[tokio::test]
    async fn test_validate_rules_via_router_oneshot() {
        let (state, _) = make_test_state();
        let body = r#"{"rules":"{\"transform\":[{\"type\":\"noop\"}]}"}"#;
        let (status, json) = oneshot_json(
            make_test_router(&state),
            "POST",
            "/api/rules/validate",
            Some(body),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["passed"], true);
    }

    // --- 认证中间件 ---

    #[tokio::test]
    async fn test_auth_blocks_unauthorized_oneshot() {
        let (state, _) = make_test_state();
        let auth = AuthConfig::new(vec!["secret_token".to_string()], true);
        let router = GovernanceServer::new(
            state,
            auth,
            "0.0.0.0:0".to_string(),
            0,
            0,
            vec![],
            // S2：测试中 /metrics 无需认证
            false,
        )
        .build_router();
        // 受保护路由未带 token → 401
        let (status, _) = oneshot_json(router, "GET", "/api/sessions", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_auth_passes_with_valid_token_oneshot() {
        let (state, _) = make_test_state();
        let auth = AuthConfig::new(vec!["secret_token".to_string()], true);
        let router = GovernanceServer::new(
            state,
            auth,
            "0.0.0.0:0".to_string(),
            0,
            0,
            vec![],
            // S2：测试中 /metrics 无需认证
            false,
        )
        .build_router();
        let request = axum::http::Request::builder()
            .method("GET")
            .uri("/api/sessions")
            .header("authorization", "Bearer secret_token")
            .body(axum::body::Body::empty())
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_public_routes_skip_auth_oneshot() {
        let (state, _) = make_test_state();
        let auth = AuthConfig::new(vec!["secret_token".to_string()], true);
        let router = GovernanceServer::new(
            state,
            auth,
            "0.0.0.0:0".to_string(),
            0,
            0,
            vec![],
            // S2：测试中 /metrics 无需认证
            false,
        )
        .build_router();
        // /api/health 是公开路由，即使启用认证也无需 token
        let (status, _) = oneshot_json(router, "GET", "/api/health", None).await;
        assert_eq!(status, StatusCode::OK);
    }
}

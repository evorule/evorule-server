// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 会话管理器 - 多反应器实例隔离
//!
//! 每个会话对应一个独立的长驻反应器实例，拥有独立的 state、FactsLog、
//! command 通道和 event 通道。SessionManager 负责会话的创建、查找和销毁。
//!
//! # 设计
//! - `SessionId`：基于 u64 的唯一标识符
//! - `Session`：持有反应器的 command_tx、facts_log、event_tx、handle
//! - `SessionManager`：持有分片 `BTreeMap<SessionId, Session>` 和共享的 core_eval 配置
//!
//! # 长驻模式配合
//! 反应器在 Stable 后不退出，持续等待下一命令。会话销毁时（`close_session`），
//! 丢弃 command_tx 触发反应器优雅退出。
//!
//! # 性能优化
//! - 分片 BTreeMap：将会话分散到多个分片，减少锁竞争
//! - AtomicU64：无锁分配 session_id，提升并发创建性能

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use evorule_reactor::{
    fact_hash, EventSender, Fact, FactId, FactSender, FactsLog, FactsLogError, Reactor,
    ReactorHandle, WalRecord,
};
use evorule_tcb::JsonValue;

use crate::auditor::{AuditEntry, Auditor};
// ObjectPool 已移除（性能优化，非核心功能）

/// 会话初始内容哈希口径（审计⑥ C2: 消除 TCB Display 与审计链 serde_json 紧凑口径的分叉）
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ContentHashScheme {
    /// 旧口径: `blake3(TCB Display 字符串)`（带空格,历史 fork 会话）
    #[default]
    #[allow(dead_code)]
    TcbDisplay,
    /// 当前口径: `blake3(serde_json 紧凑序列化)` — 与审计链 `hash::content_hash` 同源
    AuditChainCompact,
}

/// 计算 fork 会话的初始内容哈希
///
/// 优先使用审计链口径（`evorule_reactor::hash::content_hash`,serde_json 紧凑序列化）,
/// 使跨会话因果锚与审计链内容哈希可互证;转换失败时 fallback 旧 Display 口径并告警
/// （确定性引擎内该分支实际不可达,tcb_to_serde 覆盖全部 JsonValue 变体）。
fn compute_initial_hash(payload: &JsonValue) -> (String, ContentHashScheme) {
    match evorule_reactor::content_hash(payload) {
        Ok(h) => (h, ContentHashScheme::AuditChainCompact),
        Err(e) => {
            tracing::warn!(error = %e, "审计链口径内容哈希失败, fallback 到旧 Display 口径");
            (
                blake3::hash(payload.to_string().as_bytes())
                    .to_hex()
                    .to_string(),
                ContentHashScheme::TcbDisplay,
            )
        }
    }
}

/// 默认最大会话数
pub const DEFAULT_MAX_SESSIONS: usize = 1000;
/// 默认会话 TTL（30 分钟无活动自动过期）
pub const DEFAULT_SESSION_TTL: Duration = Duration::from_secs(30 * 60);
/// 后台 reaper 清理间隔（5 分钟）
pub const REAPER_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// 默认分片数（16 片，平衡并发度和内存开销）
pub const DEFAULT_SHARD_COUNT: usize = 16;
/// pending I/O 超时警告阈值缺省（30s，与引擎 ReactorBuilder 缺省一致）
pub const DEFAULT_IO_WARN_TIMEOUT: Duration = Duration::from_secs(30);
/// pending I/O 超时错误阈值缺省（60s，与引擎 ReactorBuilder 缺省一致）
///
/// 多轮编排语义下（core_eval T8：`io_request→io_response` 窗口承载整个
/// agent ReAct 循环），分钟级窗口需要部署方按任务墙钟预算显式放宽，
/// 经 `SessionManager::with_io_timeouts` 贯通。
pub const DEFAULT_IO_ERROR_TIMEOUT: Duration = Duration::from_secs(60);
/// pending I/O 超时扫描间隔缺省（5s，与引擎常量一致；测试可缩短）
pub const DEFAULT_IO_TIMEOUT_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// 会话 ID
pub type SessionId = u64;

/// 会话 - 持有一个独立反应器实例的句柄
#[derive(Clone)]
pub struct Session {
    /// command 通道发送端（提交 Fact 到反应器）
    pub command_tx: FactSender,
    /// FactsLog 克隆（读取状态和历史）
    pub facts_log: FactsLog,
    /// event 通道发送端（可 `subscribe()` 创建接收者）
    pub event_tx: EventSender,
    /// 反应器任务句柄
    pub handle: Arc<ReactorHandle>,
    /// 会话审计器（每个会话独立的哈希链）
    pub auditor: Arc<std::sync::Mutex<Auditor>>,
    /// 父会话 ID（用于跨会话因果追溯）
    pub parent_session_id: Option<SessionId>,
    /// 初始内容哈希（基于父会话最终状态或初始 payload 计算）
    /// 用于跨会话因果链的完整性校验
    pub initial_content_hash: Option<String>,
    /// 初始内容哈希口径（审计⑥ C2,验证方按口径分派验证算法）
    pub content_hash_scheme: ContentHashScheme,
    created_at: Arc<Instant>,
    last_activity_ms: Arc<AtomicU64>,
}

impl Session {
    /// 检查反应器是否已结束
    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    /// 强制中止反应器任务
    pub fn abort(&self) {
        self.handle.abort();
    }

    /// 获取当前执行阶段
    pub fn current_phase(&self) -> Option<evorule_reactor::ReactorPhase> {
        self.handle.current_phase()
    }

    /// 获取因果链深度（version 号）
    pub fn causal_depth(&self) -> Option<usize> {
        self.handle.causal_depth()
    }

    /// 获取不变式违规累计计数
    pub fn structural_invariant_violations(&self) -> u64 {
        self.handle.structural_invariant_violations()
    }

    /// 获取待响应的 I/O 请求数量
    pub fn pending_io_count(&self) -> Option<usize> {
        self.handle.pending_io_count()
    }

    /// 获取当前已执行指令步数
    pub fn current_step(&self) -> Option<usize> {
        self.handle.current_step()
    }

    /// 获取完整状态快照
    pub fn snapshot(&self) -> Option<evorule_reactor::ReactorStateSnapshot> {
        self.handle.snapshot()
    }

    /// 设置审计链自动验证配置
    pub fn set_auto_verify(&self, enabled: bool, threshold: usize, interval: usize) {
        if let Ok(mut auditor) = self.auditor.lock() {
            auditor.set_auto_verify(enabled, threshold, interval);
        }
    }

    /// 查询审计链自动验证状态
    pub fn is_auto_verify_enabled(&self) -> bool {
        if let Ok(auditor) = self.auditor.lock() {
            auditor.is_auto_verify_enabled()
        } else {
            false
        }
    }

    /// 立即封一个审计锚点（G-A2；会话结束封条/手动强制封）
    ///
    /// 未启用锚点（SessionManager 未配 anchor_config）时返回 Err。
    pub fn seal_anchor_now(
        &self,
    ) -> Result<crate::anchor::AuditAnchor, crate::anchor::AnchorError> {
        match self.auditor.lock() {
            Ok(mut auditor) => auditor.seal_anchor_now(),
            Err(_) => Err(crate::anchor::AnchorError::Hash(
                "审计器锁中毒，无法封锚点".to_string(),
            )),
        }
    }

    /// 已封锚点序列快照（G-A2；空切片=未启用或尚未封条）
    pub fn anchors(&self) -> Vec<crate::anchor::AuditAnchor> {
        match self.auditor.lock() {
            Ok(auditor) => auditor.anchors().to_vec(),
            Err(_) => Vec::new(),
        }
    }

    /// 验证锚点链+锚点与事实链绑定（G-A2 验证入口）
    pub fn verify_anchors(
        &self,
        keys: &std::collections::BTreeMap<String, String>,
    ) -> Result<(), String> {
        match self.auditor.lock() {
            Ok(auditor) => auditor.verify_anchors(keys),
            Err(_) => Err("审计器锁中毒，无法验证锚点".to_string()),
        }
    }

    /// 获取父会话 ID（用于跨会话因果追溯）
    pub fn parent_session_id(&self) -> Option<SessionId> {
        self.parent_session_id
    }

    /// 获取初始内容哈希（基于父会话状态派生）
    ///
    /// 若该会话是从父会话派生的，则返回基于父会话 payload 快照
    /// 计算的内容哈希，用于跨会话因果链完整性校验。
    pub fn initial_content_hash(&self) -> Option<&str> {
        self.initial_content_hash.as_deref()
    }

    /// 获取初始内容哈希口径（审计⑥ C2）
    pub fn content_hash_scheme(&self) -> ContentHashScheme {
        self.content_hash_scheme
    }

    /// 获取最后活动时间
    pub fn last_activity(&self) -> Instant {
        let ms = self.last_activity_ms.load(Ordering::Relaxed);
        *self.created_at + Duration::from_millis(ms)
    }

    /// 更新最后活动时间
    pub fn touch(&self) {
        let now = Instant::now();
        let ms = (now - *self.created_at).as_millis() as u64;
        self.last_activity_ms.store(ms, Ordering::Relaxed);
    }

    /// 审计新增事实（同步最新 facts 到审计链）
    ///
    /// 返回本次新增的审计条目数量。
    pub fn audit_new(&self) -> usize {
        if let Ok(mut auditor) = self.auditor.lock() {
            auditor.audit_new()
        } else {
            0
        }
    }

    /// 获取审计报告（JSON 字符串）
    ///
    /// mutex 中毒或序列化失败时返回 `Err`（不静默退化为 `"{}"`，
    /// 防止审计数据被误判为"空"）。
    pub fn audit_report(&self) -> Result<String, String> {
        let auditor = self
            .auditor
            .lock()
            .map_err(|_| "auditor mutex poisoned".to_string())?;
        auditor.report().map_err(|e| e.to_string())
    }

    /// 验证审计链完整性
    pub fn audit_verify(&self) -> bool {
        if let Ok(auditor) = self.auditor.lock() {
            auditor.verify()
        } else {
            false
        }
    }

    /// 导出审计链为 JSON 字符串（P04）
    ///
    /// 返回包含 `version`、`last_hash`、`last_audited_version`、
    /// `entry_count` 和 `entries` 数组的 JSON 字符串。
    /// 可用于跨实例迁移、离线分析或备份。
    ///
    /// mutex 中毒或序列化失败时返回 `Err`（不静默退化为 `"{}"`）。
    pub fn audit_export(&self) -> Result<String, String> {
        let auditor = self
            .auditor
            .lock()
            .map_err(|_| "auditor mutex poisoned".to_string())?;
        auditor.export().map_err(|e| e.to_string())
    }

    /// 从 JSON 字符串导入审计链（P04）
    ///
    /// 覆盖当前审计状态。导入成功后自动调用 `verify()` 校验完整性，
    /// 返回 `(import_ok, verify_ok)`。
    ///
    /// # 安全说明
    /// 调用方应确保数据来自可信来源，导入操作具有破坏性。
    pub fn audit_import(&self, json_str: &str) -> (bool, bool) {
        if let Ok(mut auditor) = self.auditor.lock() {
            match auditor.import_and_verify(json_str) {
                Ok(verify_ok) => (true, verify_ok),
                Err(e) => {
                    tracing::warn!(error = %e, "audit_import: 导入失败");
                    (false, false)
                }
            }
        } else {
            (false, false)
        }
    }

    /// 导出压缩的审计链（P05）
    ///
    /// 返回 gzip 压缩字节流，可用于减少网络传输或存储空间。
    /// 失败时返回 `Vec::new()`（调用方应视为错误）。
    pub fn audit_export_compressed(&self) -> Vec<u8> {
        if let Ok(auditor) = self.auditor.lock() {
            match auditor.export_compressed() {
                Ok(data) => data,
                Err(e) => {
                    tracing::warn!(error = %e, "audit_export_compressed: 压缩失败");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        }
    }

    /// 从压缩数据导入审计链（P05）
    ///
    /// 解压 gzip 字节流并导入。导入成功后自动调用 `verify()` 校验完整性，
    /// 返回 `(import_ok, verify_ok)`。
    pub fn audit_import_compressed(&self, compressed: &[u8]) -> (bool, bool) {
        if let Ok(mut auditor) = self.auditor.lock() {
            match auditor.import_compressed_and_verify(compressed) {
                Ok(verify_ok) => (true, verify_ok),
                Err(e) => {
                    tracing::warn!(error = %e, "audit_import_compressed: 导入失败");
                    (false, false)
                }
            }
        } else {
            (false, false)
        }
    }

    /// 获取因果链（从指定 FactId 追溯）
    pub fn causal_chain(&self, fact_id: FactId) -> Vec<AuditEntry> {
        if let Ok(auditor) = self.auditor.lock() {
            auditor.causal_chain(fact_id)
        } else {
            Vec::new()
        }
    }
}

/// 会话管理器
///
/// 管理多个反应器会话，每个会话拥有独立的 state 和通道。
/// 使用分片 BTreeMap 减少锁竞争，支持高并发访问。
pub struct SessionManager {
    /// core_eval 配置（用于创建新反应器，每次 clone）
    core_eval: Vec<JsonValue>,
    /// 最大轮次
    max_rounds: usize,
    /// 会话表分片（每片独立锁）
    shards: Vec<Arc<Mutex<BTreeMap<SessionId, Session>>>>,
    /// 下一个会话 ID（无锁分配）
    next_session_id: AtomicU64,
    /// 最大会话数
    max_sessions: usize,
    /// 会话 TTL（无活动超时）
    session_ttl: Duration,
    /// WAL 文件存储目录（为 None 时使用纯内存模式）
    wal_dir: Option<PathBuf>,
    /// WAL fsync 开关（P02）
    ///
    /// 启用后在每次 WAL 写入后执行 `sync_all()`，确保断电时数据不丢失。
    /// 性能开销较大，默认禁用。
    wal_fsync: bool,
    /// WAL 文件最大大小（字节，P03）
    ///
    /// 达到此大小后自动轮换文件（0 表示不轮换）。
    /// 默认值为 100MB。
    max_wal_size_bytes: u64,
    /// 是否启用审计链实时验证（P06）
    ///
    /// 启用后在每次 audit_new 后自动调用 verify()，及时发现数据篡改。
    /// 性能开销为 O(n)，建议大条目数场景配合阈值和间隔使用。
    auto_verify: bool,
    /// 自动验证阈值（P06）
    ///
    /// 当审计条目数超过此阈值时，跳过自动验证。0 表示不限制。
    auto_verify_threshold: usize,
    /// 自动验证间隔（P06）
    ///
    /// 每 N 次 audit_new 执行一次自动验证。1 表示每次都验证。
    auto_verify_interval: usize,
    /// pending I/O 超时警告阈值（贯通至本管理器创建的每个会话反应器）
    io_warn_timeout: Duration,
    /// pending I/O 超时错误阈值（超时发射 `Fact::Error` 并移除请求，贯通至每个会话反应器）
    io_error_timeout: Duration,
    /// pending I/O 超时扫描间隔（贯通至每个会话反应器；测试可缩短）
    io_timeout_check_interval: Duration,
    /// 当前总会话数（乐观计数）
    count: AtomicU64,
    /// 待回收的 FactsLog 列表（会话关闭后等待反应器退出）
    pending_recycle: Mutex<Vec<FactsLog>>,
    /// 审计锚点链配置（G-A2；None=锚点关，行为与旧版逐位一致）
    ///
    /// 配置后本管理器创建的每个会话审计器都会启用签名锚点链。
    /// 密钥材料不落审计资产——builder 在此持有 signer（内存态），
    /// 每会话从配置派生独立 builder（私钥种子注入，不入 WAL）。
    anchor_config: Option<SessionAnchorConfig>,
}

/// 会话审计锚点链配置（G-A2）
///
/// 由部署面构造（server 启动时注入），`SessionManager` 据此为每个
/// 新会话启用 [`crate::anchor::AnchorChainBuilder`]。
/// **默认 None**：不配置则锚点机制完全关闭（兼容判据 J-T6）。
#[derive(Debug, Clone)]
pub struct SessionAnchorConfig {
    /// ed25519 私钥种子（32 字节；由部署面 secret 管理注入，不入审计资产/WAL）
    pub signer_seed: [u8; 32],
    /// 密钥标识（轮换支持：验证方按 key_id 选公钥）
    pub key_id: String,
    /// 引擎/部署标识（跨引擎链冒充检出，如 "evorule-server/0.9.1"）
    pub engine_id: String,
    /// 自动封条间隔（每 N 条新事实封一个锚点；0=仅手动 seal）
    pub interval: usize,
}

/// 安全获取 Mutex 锁，处理 PoisonError
fn lock_mutex<T>(
    mutex: &Mutex<T>,
) -> Result<std::sync::MutexGuard<'_, T>, PoisonError<std::sync::MutexGuard<'_, T>>> {
    mutex.lock()
}

impl SessionManager {
    /// 创建会话管理器（使用默认限制：最多 1000 会话，TTL 30 分钟，纯内存模式）
    ///
    /// # 参数
    /// - `core_eval`：transform 规则列表（用于创建每个会话的反应器）
    /// - `max_rounds`：每个反应器的最大指令执行步数
    pub fn new(core_eval: Vec<JsonValue>, max_rounds: usize) -> Self {
        Self::with_limits_and_wal(
            core_eval,
            max_rounds,
            DEFAULT_MAX_SESSIONS,
            DEFAULT_SESSION_TTL,
            None,
            DEFAULT_SHARD_COUNT,
        )
    }

    /// 创建会话管理器并指定资源限制（纯内存模式）
    ///
    /// # 参数
    /// - `core_eval`：transform 规则列表
    /// - `max_rounds`：每个反应器的最大指令执行步数
    /// - `max_sessions`：最大并发会话数
    /// - `session_ttl`：会话无活动超时时间
    pub fn with_limits(
        core_eval: Vec<JsonValue>,
        max_rounds: usize,
        max_sessions: usize,
        session_ttl: Duration,
    ) -> Self {
        Self::with_limits_and_wal(
            core_eval,
            max_rounds,
            max_sessions,
            session_ttl,
            None,
            DEFAULT_SHARD_COUNT,
        )
    }

    /// 创建会话管理器并指定资源限制和 WAL 目录
    ///
    /// # 参数
    /// - `core_eval`：transform 规则列表
    /// - `max_rounds`：每个反应器的最大指令执行步数
    /// - `max_sessions`：最大并发会话数
    /// - `session_ttl`：会话无活动超时时间
    /// - `wal_dir`：WAL 文件存储目录（为 None 时使用纯内存模式）
    /// - `shard_count`：分片数（推荐 16-64）
    pub fn with_limits_and_wal(
        core_eval: Vec<JsonValue>,
        max_rounds: usize,
        max_sessions: usize,
        session_ttl: Duration,
        wal_dir: Option<PathBuf>,
        shard_count: usize,
    ) -> Self {
        Self::with_limits_and_wal_and_fsync(
            core_eval,
            max_rounds,
            max_sessions,
            session_ttl,
            wal_dir,
            shard_count,
            false,
        )
    }

    /// 创建会话管理器并指定资源限制、WAL 目录和 fsync 选项（P02）
    ///
    /// # 参数
    /// - `core_eval`：transform 规则列表
    /// - `max_rounds`：每个反应器的最大指令执行步数
    /// - `max_sessions`：最大并发会话数
    /// - `session_ttl`：会话无活动超时时间
    /// - `wal_dir`：WAL 文件存储目录（为 None 时使用纯内存模式）
    /// - `shard_count`：分片数（推荐 16-64）
    /// - `wal_fsync`：是否在每次 WAL 写入后执行 fsync（确保断电时数据不丢失）
    pub fn with_limits_and_wal_and_fsync(
        core_eval: Vec<JsonValue>,
        max_rounds: usize,
        max_sessions: usize,
        session_ttl: Duration,
        wal_dir: Option<PathBuf>,
        shard_count: usize,
        wal_fsync: bool,
    ) -> Self {
        Self::with_limits_and_wal_full(
            core_eval,
            max_rounds,
            max_sessions,
            session_ttl,
            wal_dir,
            shard_count,
            wal_fsync,
            100 * 1024 * 1024,
        )
    }

    /// 创建会话管理器并指定资源限制、WAL 目录、fsync 和轮换选项（P03）
    ///
    /// # 参数
    /// - `core_eval`：transform 规则列表
    /// - `max_rounds`：每个反应器的最大指令执行步数
    /// - `max_sessions`：最大并发会话数
    /// - `session_ttl`：会话无活动超时时间
    /// - `wal_dir`：WAL 文件存储目录（为 None 时使用纯内存模式）
    /// - `shard_count`：分片数（推荐 16-64）
    /// - `wal_fsync`：是否在每次 WAL 写入后执行 fsync（确保断电时数据不丢失）
    /// - `max_wal_size_bytes`：单个 WAL 文件最大大小（0 表示不轮换）
    #[allow(clippy::too_many_arguments)]
    pub fn with_limits_and_wal_full(
        core_eval: Vec<JsonValue>,
        max_rounds: usize,
        max_sessions: usize,
        session_ttl: Duration,
        wal_dir: Option<PathBuf>,
        shard_count: usize,
        wal_fsync: bool,
        max_wal_size_bytes: u64,
    ) -> Self {
        Self::with_limits_and_wal_and_auto_verify(
            core_eval,
            max_rounds,
            max_sessions,
            session_ttl,
            wal_dir,
            shard_count,
            wal_fsync,
            max_wal_size_bytes,
            false,
            1000,
            1,
        )
    }

    /// 创建会话管理器并指定完整配置（P06）
    ///
    /// # 参数
    /// - `core_eval`：transform 规则列表
    /// - `max_rounds`：每个反应器的最大指令执行步数
    /// - `max_sessions`：最大并发会话数
    /// - `session_ttl`：会话无活动超时时间
    /// - `wal_dir`：WAL 文件存储目录（为 None 时使用纯内存模式）
    /// - `shard_count`：分片数（推荐 16-64）
    /// - `wal_fsync`：是否在每次 WAL 写入后执行 fsync
    /// - `max_wal_size_bytes`：单个 WAL 文件最大大小（0 表示不轮换）
    /// - `auto_verify`：是否启用审计链实时验证
    /// - `auto_verify_threshold`：自动验证阈值（0 表示不限制）
    /// - `auto_verify_interval`：自动验证间隔（1 表示每次都验证）
    #[allow(clippy::too_many_arguments)]
    pub fn with_limits_and_wal_and_auto_verify(
        core_eval: Vec<JsonValue>,
        max_rounds: usize,
        max_sessions: usize,
        session_ttl: Duration,
        wal_dir: Option<PathBuf>,
        shard_count: usize,
        wal_fsync: bool,
        max_wal_size_bytes: u64,
        auto_verify: bool,
        auto_verify_threshold: usize,
        auto_verify_interval: usize,
    ) -> Self {
        let shards = (0..shard_count)
            .map(|_| Arc::new(Mutex::new(BTreeMap::new())))
            .collect();

        // AUDIT-A3 修复（2026-08-27）：配置 wal_dir 时扫描既有 session_*.wal
        // 恢复会话 ID 计数器。否则重启后 next_session_id 归 1，首个新会话会以
        // truncate 模式覆盖已存在的 session_1.wal，静默清除上一轮审计链。
        let initial_session_id = wal_dir
            .as_deref()
            .map(Self::scan_max_session_id)
            .unwrap_or(0)
            + 1;

        Self {
            core_eval,
            max_rounds,
            shards,
            next_session_id: AtomicU64::new(initial_session_id),
            max_sessions,
            session_ttl,
            wal_dir,
            wal_fsync,
            max_wal_size_bytes,
            auto_verify,
            auto_verify_threshold,
            auto_verify_interval: if auto_verify_interval == 0 {
                1
            } else {
                auto_verify_interval
            },
            io_warn_timeout: DEFAULT_IO_WARN_TIMEOUT,
            io_error_timeout: DEFAULT_IO_ERROR_TIMEOUT,
            io_timeout_check_interval: DEFAULT_IO_TIMEOUT_CHECK_INTERVAL,
            count: AtomicU64::new(0),
            pending_recycle: Mutex::new(Vec::new()),
            anchor_config: None,
        }
    }

    /// 启用审计锚点链（G-A2，builder 模式）
    ///
    /// 配置后本管理器**后续创建**的每个会话审计器启用签名锚点链；
    /// 已存在会话不受影响。不调用本方法=锚点关（与旧版行为逐位一致）。
    pub fn with_anchor_config(mut self, config: SessionAnchorConfig) -> Self {
        self.anchor_config = Some(config);
        self
    }

    /// 从锚点配置派生单会话审计器（私钥种子注入 signer，session_id 入锚点载荷）
    fn auditor_from_config(&self, facts_log: FactsLog) -> Auditor {
        let auditor = Auditor::new_with_auto_verify(
            facts_log,
            self.auto_verify,
            self.auto_verify_threshold,
            self.auto_verify_interval,
        );
        match &self.anchor_config {
            Some(cfg) => auditor.with_anchor(
                crate::signing::AuditSigner::from_bytes(cfg.signer_seed),
                cfg.key_id.clone(),
                cfg.engine_id.clone(),
                format!("session-{}", self.next_session_id.load(Ordering::Relaxed)),
                cfg.interval,
            ),
            None => auditor,
        }
    }

    /// 覆盖 pending I/O 超时预算（贯通至本管理器后续创建的每个会话反应器）
    ///
    /// `None` 项保持引擎缺省（warn 30s / error 60s / 扫描 5s），缺省行为
    /// 与未调用本方法逐位一致。部署方按任务墙钟预算放宽 error 阈值
    /// （多轮编排窗口可远超单发调用秒级假设）。
    pub fn with_io_timeouts(
        mut self,
        io_warn_timeout: Option<Duration>,
        io_error_timeout: Option<Duration>,
        io_timeout_check_interval: Option<Duration>,
    ) -> Self {
        if let Some(d) = io_warn_timeout {
            self.io_warn_timeout = d;
        }
        if let Some(d) = io_error_timeout {
            self.io_error_timeout = d;
        }
        if let Some(d) = io_timeout_check_interval {
            self.io_timeout_check_interval = d;
        }
        self
    }

    /// 扫描 WAL 目录，返回既有会话 WAL 文件的最大序号（无文件时为 0）
    ///
    /// 仅匹配 `session_{n}.wal` 精确命名；轮换分片 `session_{n}.wal.{seq}`
    /// 与共享事实 `shared_facts.wal` 均不会被误判。
    fn scan_max_session_id(wal_dir: &std::path::Path) -> u64 {
        let mut max = 0u64;
        let Ok(entries) = std::fs::read_dir(wal_dir) else {
            tracing::warn!(
                dir = %wal_dir.display(),
                "无法读取 WAL 目录，会话 ID 计数器从 1 开始（若该目录曾有会话，旧 WAL 可能被覆盖）"
            );
            return 0;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if let Some(num) = name
                .strip_prefix("session_")
                .and_then(|s| s.strip_suffix(".wal"))
                .and_then(|s| s.parse::<u64>().ok())
            {
                max = max.max(num);
            }
        }
        if max > 0 {
            tracing::info!(max_session_wal_id = max, "已从 WAL 目录恢复会话 ID 计数器");
        }
        max
    }

    /// 获取分片索引
    fn get_shard_idx(&self, id: SessionId) -> usize {
        (id as usize) % self.shards.len()
    }

    /// 获取分片
    fn get_shard(&self, id: SessionId) -> &Arc<Mutex<BTreeMap<SessionId, Session>>> {
        &self.shards[self.get_shard_idx(id)]
    }

    /// 原子预留会话名额（ / B4）
    ///
    /// 以 CAS 自旋替代"先 load 检查、后 fetch_add"的两步模式——原模式下检查
    /// 与递增之间存在窗口（其间含 create_facts_log/spawn 等慢操作），高并发
    /// 并发创建可突破 `max_sessions`（TOCTOU）。
    ///
    /// # 返回
    /// - `Ok(())`：成功预留 1 个名额（调用方创建失败时必须调用
    ///   [`Self::release_session_slot`] 归还）
    /// - `Err(SessionError::LimitExceeded)`：已达上限
    fn reserve_session_slot(&self) -> Result<(), SessionError> {
        loop {
            let current = self.count.load(Ordering::Relaxed);
            if current >= self.max_sessions as u64 {
                tracing::warn!(
                    current,
                    max = self.max_sessions,
                    "Session creation rejected: limit exceeded"
                );
                return Err(SessionError::LimitExceeded {
                    current: current as usize,
                    max: self.max_sessions,
                });
            }
            if self
                .count
                .compare_exchange(current, current + 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return Ok(());
            }
            // CAS 失败 = 并发竞争，重读重试
        }
    }

    /// 归还 1 个会话名额（与 [`Self::reserve_session_slot`] 配对）
    fn release_session_slot(&self) {
        self.count.fetch_sub(1, Ordering::Relaxed);
    }

    /// 创建新会话
    ///
    /// spawn 一个新的长驻反应器实例，分配唯一 SessionId。
    ///
    /// # 返回
    /// - `Ok(SessionId)`：新会话的 SessionId
    /// - `Err(SessionError::LimitExceeded)`：超过最大会话数限制
    /// - `Err(SessionError::WalUnavailable)`：WAL 不可用，拒绝创建（不允许审计链残缺的会话）
    pub fn create_session(&self) -> Result<SessionId, SessionError> {
        // B4：先原子预留名额，再做慢操作（create_facts_log/spawn）；失败路径必须归还
        self.reserve_session_slot()?;

        let session_id = self.next_session_id.fetch_add(1, Ordering::Relaxed);

        let facts_log = match self.create_facts_log(session_id) {
            Ok(fl) => fl,
            Err(source) => {
                self.release_session_slot();
                tracing::error!(
                    session_id,
                    error = %source,
                    "拒绝创建会话：WAL 不可用（确定性引擎不允许无审计链的会话）"
                );
                return Err(SessionError::WalUnavailable { session_id, source });
            }
        };

        let reactor = Reactor::builder(self.core_eval.clone())
            .max_rounds(self.max_rounds)
            .io_warn_timeout(self.io_warn_timeout)
            .io_error_timeout(self.io_error_timeout)
            .io_timeout_check_interval(self.io_timeout_check_interval)
            .facts_log(facts_log)
            .build();
        let (command_tx, _event_rx, event_tx, handle, facts_log) = reactor.spawn();

        let shard = self.get_shard(session_id);
        let mut shard_guard = match lock_mutex(shard) {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!("SessionManager shard mutex poisoned, recovering");
                e.into_inner()
            }
        };

        let new_count = self.count.load(Ordering::Relaxed);

        tracing::info!(
            session_id,
            active = new_count,
            max = self.max_sessions,
            "Session created (long-running reactor spawned)"
        );

        let auditor = Arc::new(std::sync::Mutex::new(
            self.auditor_from_config(facts_log.clone()),
        ));

        shard_guard.insert(
            session_id,
            Session {
                command_tx,
                facts_log,
                event_tx,
                handle: Arc::new(handle),
                auditor,
                parent_session_id: None,
                initial_content_hash: None,
                content_hash_scheme: ContentHashScheme::default(),
                created_at: Arc::new(Instant::now()),
                last_activity_ms: Arc::new(AtomicU64::new(0)),
            },
        );

        Ok(session_id)
    }

    /// 从父会话派生新会话（跨会话因果追溯）
    ///
    /// 创建一个新会话，并将其与 `parent_id` 关联。
    /// 新会话的 `initial_content_hash` 基于父会话当前的 payload 快照计算，
    /// 形成跨会话的因果链，用于审计追溯。
    ///
    /// 父会话不需要处于结束状态——可以在任何时刻派生子会话。
    pub fn create_session_from_parent(
        &self,
        parent_id: SessionId,
    ) -> Result<SessionId, SessionError> {
        self.create_session_from_parent_at_version(parent_id, None)
    }

    /// 基于父会话的指定版本创建新会话
    ///
    /// 如果指定了 `version`，则从父会话在该版本的快照派生子会话；
    /// 如果未指定，则使用父会话的当前状态（同 `create_session_from_parent`）。
    ///
    /// # 参数
    ///
    /// - `parent_id`: 父会话 ID
    /// - `version`: 可选的历史版本号（0 = 初始空状态）
    ///
    /// # 返回
    ///
    /// - `Ok(session_id)`: 创建成功，返回新会话 ID
    /// - `Err(SessionError::NotFound)`: 父会话不存在
    /// - `Err(SessionError::LimitExceeded)`: 超过最大会话数
    /// - `Err(SessionError::InvalidVersion)`: 指定的版本号无效
    /// - `Err(SessionError::WalUnavailable)`: WAL 不可用，拒绝派生（不允许审计链残缺的会话）
    pub fn create_session_from_parent_at_version(
        &self,
        parent_id: SessionId,
        version: Option<u64>,
    ) -> Result<SessionId, SessionError> {
        self.create_session_from_parent_at_version_with_core_eval(parent_id, version, None)
    }

    /// 基于父会话指定版本创建新会话，并允许挂载会话专属规则集
    ///
    /// fork 语义与 [`SessionManager::create_session_from_parent_at_version`]
    /// 完全一致（状态快照继承、事实链全新起步、审计链/WAL 建链一致），唯一差异：
    /// `core_eval_override` 为 `Some` 时，新会话 Reactor 挂载给定规则集而非当前
    /// 全局规则集——用于沙盒编排等「派生会话需要与生产不同规则集」的编排场景；
    /// 不修改全局规则集，不影响任何其他会话。`None` 与
    /// [`create_session_from_parent_at_version`] 完全一致。
    ///
    /// # 参数
    ///
    /// - `parent_id`: 父会话 ID
    /// - `version`: 可选的历史版本号（0 = 初始空状态）
    /// - `core_eval_override`: 会话专属规则集（`None` = 当前全局规则集）
    ///
    /// # 返回 / 错误
    ///
    /// 与 [`SessionManager::create_session_from_parent_at_version`] 相同。
    pub fn create_session_from_parent_at_version_with_core_eval(
        &self,
        parent_id: SessionId,
        version: Option<u64>,
        core_eval_override: Option<Vec<JsonValue>>,
    ) -> Result<SessionId, SessionError> {
        let parent = self
            .get_session(parent_id)
            .ok_or(SessionError::NotFound { id: parent_id })?;

        // 审计⑥ C2: 锚哈希统一到审计链口径（带口径标记,旧验证方按 scheme 分派）
        let (initial_payload, initial_version) = match version {
            Some(v) => rewind_payload(&parent.facts_log, v)
                .map(|payload| (payload, v))
                .ok_or(SessionError::InvalidVersion { version: v })?,
            None => {
                let (payload, _, version) = parent.facts_log.snapshot();
                (payload, version)
            }
        };

        self.create_session_from_initial_state_inner(
            Some(parent_id),
            initial_payload,
            initial_version,
            core_eval_override,
        )
    }

    /// 从给定初始状态创建新会话（fork-from-archive 入口）
    ///
    /// 内存父会话（经 `rewind_payload`）与磁盘归档链（经全链哈希校验 + 重放）
    /// 算出的初始状态都经此入口落地；两条 fork 路径的统一落点为
    /// [`SessionManager::create_session_from_initial_state_inner`]。
    /// 需要会话专属规则集的变体走
    /// [`SessionManager::create_session_from_parent_at_version_with_core_eval`]。
    ///
    /// # 参数 / 返回
    ///
    /// 与 [`SessionManager::create_session_from_initial_state_inner`] 相同。
    pub fn create_session_from_initial_state(
        &self,
        parent_id: Option<SessionId>,
        initial_payload: JsonValue,
        initial_version: u64,
    ) -> Result<SessionId, SessionError> {
        self.create_session_from_initial_state_inner(
            parent_id,
            initial_payload,
            initial_version,
            None,
        )
    }

    /// fork-from-archive 与会话专属规则集 fork 的统一落点
    ///
    /// [`SessionManager::create_session_from_initial_state`] 与
    /// [`SessionManager::create_session_from_parent_at_version_with_core_eval`]
    /// 共享同一套建链/注册逻辑：预留名额 → 分配 ID → WAL 版 FactsLog（fail-closed）
    /// → spawn reactor → 注册 shard。
    ///
    /// # 参数
    ///
    /// - `parent_id`: 因果父会话 ID（None = 不记录因果链接，仅建会话）
    /// - `initial_payload`: 初始 payload
    /// - `initial_version`: 初始版本号（新会话审计链从此续起）
    /// - `core_eval`: 新会话 Reactor 规则集；`None` = 当前全局规则集 clone
    fn create_session_from_initial_state_inner(
        &self,
        parent_id: Option<SessionId>,
        initial_payload: JsonValue,
        initial_version: u64,
        core_eval: Option<Vec<JsonValue>>,
    ) -> Result<SessionId, SessionError> {
        // B4：先原子预留名额，再做慢操作（create_facts_log/spawn）；失败路径必须归还
        self.reserve_session_slot()?;

        let session_id = self.next_session_id.fetch_add(1, Ordering::Relaxed);

        // 审计⑥ C2: 锚哈希统一到审计链口径（带口径标记,旧验证方按 scheme 分派）
        let (initial_content_hash, content_hash_scheme) = compute_initial_hash(&initial_payload);

        let facts_log = match self.create_facts_log(session_id) {
            Ok(fl) => fl,
            Err(source) => {
                self.release_session_slot();
                tracing::error!(
                    session_id,
                    error = %source,
                    "拒绝派生会话：WAL 不可用（确定性引擎不允许无审计链的会话）"
                );
                return Err(SessionError::WalUnavailable { session_id, source });
            }
        };
        facts_log.set_initial_state(initial_payload, initial_version);

        let reactor = Reactor::builder(core_eval.unwrap_or_else(|| self.core_eval.clone()))
            .max_rounds(self.max_rounds)
            .io_warn_timeout(self.io_warn_timeout)
            .io_error_timeout(self.io_error_timeout)
            .io_timeout_check_interval(self.io_timeout_check_interval)
            .facts_log(facts_log)
            .build();
        let (command_tx, _event_rx, event_tx, handle, facts_log) = reactor.spawn();

        let shard = self.get_shard(session_id);
        let mut shard_guard = match lock_mutex(shard) {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!("SessionManager shard mutex poisoned, recovering");
                e.into_inner()
            }
        };

        let new_count = self.count.load(Ordering::Relaxed);

        tracing::info!(
            session_id,
            parent_id,
            active = new_count,
            max = self.max_sessions,
            "Session created from parent (cross-session causality)"
        );

        let auditor = Arc::new(std::sync::Mutex::new(
            self.auditor_from_config(facts_log.clone()),
        ));

        shard_guard.insert(
            session_id,
            Session {
                command_tx,
                facts_log,
                event_tx,
                handle: Arc::new(handle),
                auditor,
                parent_session_id: parent_id,
                initial_content_hash: Some(initial_content_hash),
                content_hash_scheme,
                created_at: Arc::new(Instant::now()),
                last_activity_ms: Arc::new(AtomicU64::new(0)),
            },
        );

        Ok(session_id)
    }

    /// 返回当前已加载的 transform 规则列表（只读）
    ///
    /// 用于 Portal API、reload API 返回当前生效的 transform 长度/内容。
    pub fn core_eval(&self) -> &[JsonValue] {
        &self.core_eval
    }

    /// 原子替换用于创建**新**会话的 transform 规则列表。
    ///
    /// 已经存在的会话（已 spawn 的 Reactor）其 core_eval 已被 clone 进 Reactor，
    /// 不受此调用影响，保证确定性/TCB 不可变语义。
    ///
    /// 返回旧的 core_eval（调用方可按需记录）。
    pub fn replace_core_eval(&mut self, new_core_eval: Vec<JsonValue>) -> Vec<JsonValue> {
        let old = std::mem::replace(&mut self.core_eval, new_core_eval);
        tracing::info!(
            old_rules = old.len(),
            new_rules = self.core_eval.len(),
            "SessionManager core_eval replaced (only new sessions will use new rules)"
        );
        old
    }

    /// 更新会话的最后活动时间（每次访问会话时调用）
    pub fn touch_session(&self, id: SessionId) {
        let shard = self.get_shard(id);
        let mut shard_guard = match lock_mutex(shard) {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!("SessionManager shard mutex poisoned, recovering");
                e.into_inner()
            }
        };
        if let Some(session) = shard_guard.get_mut(&id) {
            session.touch();
        }
    }

    /// 获取会话引用
    pub fn get_session(&self, id: SessionId) -> Option<Session> {
        let shard = self.get_shard(id);
        let shard_guard = match lock_mutex(shard) {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!("SessionManager shard mutex poisoned, recovering");
                e.into_inner()
            }
        };
        shard_guard.get(&id).cloned()
    }

    /// 关闭会话
    ///
    /// 取出会话并丢弃 command_tx，触发反应器优雅退出。
    /// 反应器在检测到通道关闭后返回 `Ok(())`。
    ///
    /// # 返回
    /// - `Ok(Arc<ReactorHandle>)`：会话的 handle，调用方可 `await` 确认反应器已退出
    /// - `Err(SessionError::NotFound)`：会话不存在
    pub fn close_session(&self, id: SessionId) -> Result<Arc<ReactorHandle>, SessionError> {
        let shard = self.get_shard(id);
        let mut shard_guard = match lock_mutex(shard) {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!("SessionManager shard mutex poisoned, recovering");
                e.into_inner()
            }
        };
        let session = shard_guard
            .remove(&id)
            .ok_or(SessionError::NotFound { id })?;

        self.count.fetch_sub(1, Ordering::Relaxed);

        // 内存模式下，将 FactsLog 加入待回收列表（等待反应器退出后复用）
        if self.wal_dir.is_none() {
            if let Ok(mut pending) = self.pending_recycle.lock() {
                pending.push(session.facts_log.clone());
            }
        }

        tracing::info!("Session {} closing (command_tx dropped)", id);
        Ok(session.handle)
    }

    /// 列出所有活跃会话 ID
    pub fn list_sessions(&self) -> Vec<SessionId> {
        let mut ids = Vec::new();
        for shard in &self.shards {
            let shard_guard = match lock_mutex(shard) {
                Ok(g) => g,
                Err(e) => {
                    tracing::warn!("SessionManager shard mutex poisoned, recovering");
                    e.into_inner()
                }
            };
            for id in shard_guard.keys() {
                ids.push(*id);
            }
        }
        ids.sort();
        ids
    }

    /// 清理已结束的会话
    ///
    /// 移除所有 `is_finished()` 为真的会话，并尝试回收 FactsLog 到对象池。
    /// 同时清理待回收列表中可复用的 FactsLog。
    /// 返回被清理的会话数量。
    pub fn reap_finished(&self) -> usize {
        // 先清理待回收列表
        self.reclaim_pending();

        let mut reaped = 0;
        for shard in &self.shards {
            let mut shard_guard = match lock_mutex(shard) {
                Ok(g) => g,
                Err(e) => {
                    tracing::warn!("SessionManager shard mutex poisoned, recovering");
                    e.into_inner()
                }
            };

            // 收集已结束会话的 ID
            let finished_ids: Vec<SessionId> = shard_guard
                .iter()
                .filter(|(_, session)| session.is_finished())
                .map(|(id, _)| *id)
                .collect();

            // 移除已完成的会话
            for id in finished_ids {
                if let Some(_session) = shard_guard.remove(&id) {
                    tracing::debug!("Session {} reaped (reactor finished)", id);
                    reaped += 1;
                }
            }
        }
        if reaped > 0 {
            self.count.fetch_sub(reaped as u64, Ordering::Relaxed);
        }
        reaped
    }

    /// 清理过期的会话（TTL 过期）
    ///
    /// 移除所有 `last_activity` 距今超过 `session_ttl` 的会话。
    /// 返回被清理的会话数量。
    pub fn reap_expired(&self) -> usize {
        let now = Instant::now();
        let mut reaped = 0;
        for shard in &self.shards {
            let mut shard_guard = match lock_mutex(shard) {
                Ok(g) => g,
                Err(e) => {
                    tracing::warn!("SessionManager shard mutex poisoned, recovering");
                    e.into_inner()
                }
            };
            let before = shard_guard.len();
            shard_guard.retain(|id, session| {
                let elapsed = now.duration_since(session.last_activity());
                if elapsed > self.session_ttl {
                    tracing::info!(
                        session_id = id,
                        elapsed_secs = elapsed.as_secs(),
                        ttl_secs = self.session_ttl.as_secs(),
                        "Session expired (TTL reached)"
                    );
                    false
                } else {
                    true
                }
            });
            reaped += before - shard_guard.len();
        }
        if reaped > 0 {
            self.count.fetch_sub(reaped as u64, Ordering::Relaxed);
        }
        reaped
    }

    /// 清理所有可回收的会话（已结束 + 已过期）
    pub fn reap_all(&self) -> usize {
        let finished = self.reap_finished();
        let expired = self.reap_expired();
        finished + expired
    }

    /// 活跃会话数（乐观计数）
    pub fn len(&self) -> usize {
        self.count.load(Ordering::Relaxed) as usize
    }

    /// 是否无会话
    pub fn is_empty(&self) -> bool {
        self.count.load(Ordering::Relaxed) == 0
    }

    /// 创建 FactsLog（根据配置选择内存模式或 WAL 模式）
    ///
    /// 配置了 `wal_dir` 时创建带 WAL 的 FactsLog；创建失败返回 `Err`
    /// （调用方必须拒绝创建会话，禁止静默降级为内存模式——
    /// 确定性引擎不允许产生审计链残缺的会话）。
    /// 未配置 `wal_dir` 时为纯内存模式（显式配置，非降级）。
    fn create_facts_log(&self, session_id: SessionId) -> Result<FactsLog, FactsLogError> {
        if let Some(ref wal_dir) = self.wal_dir {
            let wal_path = wal_dir.join(format!("session_{}.wal", session_id));
            match FactsLog::with_wal_options(&wal_path, self.max_wal_size_bytes, self.wal_fsync) {
                Ok(facts_log) => {
                    tracing::debug!(session_id, wal_path = %wal_path.display(), fsync = self.wal_fsync, max_wal_size_bytes = self.max_wal_size_bytes, "FactsLog created with WAL");
                    Ok(facts_log)
                }
                Err(e) => Err(e),
            }
        } else {
            Ok(FactsLog::new())
        }
    }

    /// 清理待回收列表（对象池已移除，仅清理已完成的）
    fn reclaim_pending(&self) {
        if let Ok(mut pending) = self.pending_recycle.lock() {
            if pending.is_empty() {
                return;
            }
            let still_pending: Vec<FactsLog> =
                pending.drain(..).filter(|fl| !fl.is_reusable()).collect();
            *pending = still_pending;
        }
    }
}

/// 会话错误
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// 会话不存在
    #[error("Session {id} not found")]
    NotFound {
        /// 不存在的会话 ID
        id: SessionId,
    },
    /// 超过最大会话数限制
    #[error("Session limit exceeded: {current}/{max}")]
    LimitExceeded {
        /// 当前会话数
        current: usize,
        /// 最大会话数
        max: usize,
    },
    /// 指定的版本号无效
    #[error("Invalid version {version}")]
    InvalidVersion {
        /// 无效的版本号
        version: u64,
    },
    /// WAL 不可用，拒绝创建会话
    ///
    /// 确定性引擎不允许存在无审计链的会话：WAL 创建失败时宁可不服务，
    /// 也绝不静默降级为内存模式（否则该会话事实不落盘、审计链永久残缺）。
    #[error("WAL unavailable, session creation rejected: session {session_id}: {source}")]
    WalUnavailable {
        /// 目标会话 ID
        session_id: SessionId,
        /// 底层 WAL 错误
        #[source]
        source: FactsLogError,
    },
    /// 归档 WAL 链校验失败，拒绝 fork-from-archive
    ///
    /// fail-closed 口径：审计链校验不过（内容哈希/链哈希不匹配、旧格式无哈希、
    /// 内容损坏）的归档，绝不能作为新会话的父状态——否则篡改数据可经
    /// fork"洗白"进新链，破坏"可证明"承诺。
    #[error("Archive WAL integrity check failed: {reason}")]
    ArchiveCorrupted {
        /// 失败原因
        reason: String,
    },
}

/// 从 FactsLog 回溯到指定 version，返回当时的 payload 快照
///
/// 用于 fork_session 时获取父会话的历史版本状态。
/// 与 time_machine 模块同源同语义（同 apply_payload_update 更新规则）；
/// time_machine 仍在 governance 内提供 rewind/diff 公开 API，fork 经此
/// 轻量入口复用 FactsLog 增量遍历。
///
/// 断点 9 修复：添加 PayloadUpdate 分支，正确应用路径更新并递增 version。
/// 原实现忽略 PayloadUpdate（`_ => {}`），导致 fork_session 获取的初始状态
/// 缺少 PayloadUpdate 变更，且 version 计数不一致。
fn rewind_payload(facts_log: &FactsLog, target_version: u64) -> Option<JsonValue> {
    if target_version == 0 {
        return Some(JsonValue::empty_object());
    }

    let history = facts_log.history_with_versions();
    let mut payload = JsonValue::empty_object();
    let mut version: u64 = 0;

    for (version_before, fact) in history {
        match &fact {
            Fact::StateTransition { new_payload, .. } => {
                payload = new_payload.clone();
                version = version_before + 1;
            }
            Fact::IoResponse { .. } => {
                version = version_before + 1;
            }
            // 断点 9 修复：PayloadUpdate 必须应用路径更新并递增 version
            Fact::PayloadUpdate { path, value, .. } => {
                crate::time_machine::apply_payload_update(&mut payload, path, value.clone());
                version = version_before + 1;
            }
            _ => {}
        }
        if version == target_version {
            break;
        }
    }

    if version < target_version {
        return None;
    }
    Some(payload)
}

/// 从归档 WAL 记录重建 fork 初始状态（fork-from-archive 的核心）
///
/// 数据源是磁盘上的归档 WAL 记录（`read_wal_with_hash` 产物，只读）而非内存
/// FactsLog，重放语义与 [`rewind_payload`] 完全同源（StateTransition 整体替换 /
/// IoResponse 递增 / PayloadUpdate 经 `apply_payload_update` 路径更新）。
///
/// fail-closed：先做全链哈希校验，再做重放——
/// 1. `content_hash` 必须与 `fact_hash(fact)` 重算一致（与写入侧 `FactsLog::append`
///    同一单一真相源）；
/// 2. `chain_hash = blake3(prev_hash + content_hash)` 逐条续链，prev 从 `"genesis"` 起
///    （与审计档案 `rebuild_chain` 同一口径）；
/// 3. 存在旧格式（无哈希）记录 → `ArchiveCorrupted`（无法证明未被篡改）。
///
/// 任一条不过即拒绝派生，防篡改数据经 fork "洗白"进新链。
///
/// # 参数
///
/// - `records`: 归档 WAL 的全部记录（含轮换分片，按序拼接）
/// - `target_version`: `None` = 回放到链尾；`Some(0)` = 空状态；超出链尾 → `InvalidVersion`
///
/// # 返回
///
/// `(payload, version)`——fork 新会话的初始状态
pub fn payload_from_wal_records(
    records: &[WalRecord],
    target_version: Option<u64>,
) -> Result<(JsonValue, u64), SessionError> {
    // —— 1. 全链哈希校验（fail-closed，先于任何重放）——
    let mut prev_hash = String::from("genesis");
    for rec in records {
        let content_hash = fact_hash(&rec.fact).map_err(|e| SessionError::ArchiveCorrupted {
            reason: format!("content hash failure at fact {}: {e}", rec.fact.id().0),
        })?;
        match &rec.content_hash {
            Some(stored) if stored != &content_hash => {
                return Err(SessionError::ArchiveCorrupted {
                    reason: format!("content_hash mismatch at fact {}", rec.fact.id().0),
                });
            }
            // content_hash 与 chain_hash 由写入侧成对落盘；缺哈希 = 旧格式，拒收
            None => {
                return Err(SessionError::ArchiveCorrupted {
                    reason: format!(
                        "legacy (hashless) record at fact {} — cannot prove integrity",
                        rec.fact.id().0
                    ),
                });
            }
            Some(_) => {}
        }
        let chain_hash = blake3::hash(format!("{prev_hash}{content_hash}").as_bytes())
            .to_hex()
            .to_string();
        if let Some(stored_chain) = &rec.chain_hash {
            if stored_chain != &chain_hash {
                return Err(SessionError::ArchiveCorrupted {
                    reason: format!("chain_hash mismatch at fact {}", rec.fact.id().0),
                });
            }
        }
        prev_hash = chain_hash;
    }

    // —— 2. 重放 payload（与 rewind_payload 同语义）——
    if target_version == Some(0) {
        return Ok((JsonValue::empty_object(), 0));
    }
    let mut payload = JsonValue::empty_object();
    let mut version: u64 = 0;
    for rec in records {
        let version_before = rec.version_before;
        match &rec.fact {
            Fact::StateTransition { new_payload, .. } => {
                payload = new_payload.clone();
                version = version_before + 1;
            }
            Fact::IoResponse { .. } => {
                version = version_before + 1;
            }
            Fact::PayloadUpdate { path, value, .. } => {
                crate::time_machine::apply_payload_update(&mut payload, path, value.clone());
                version = version_before + 1;
            }
            _ => {}
        }
        if Some(version) == target_version {
            break;
        }
    }

    match target_version {
        None => Ok((payload, version)),
        Some(t) if version >= t => Ok((payload, version)),
        Some(t) => Err(SessionError::InvalidVersion { version: t }),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::panic, clippy::expect_used)]
    use super::*;
    use evorule_reactor::Fact;
    use std::collections::BTreeMap;

    /// AUDIT-A3 回归：重启后从 wal_dir 恢复会话 ID 计数器，
    /// 新会话不得以 truncate 覆盖既有审计链。
    #[tokio::test]
    async fn test_session_id_counter_restored_from_wal_dir() {
        let dir = std::env::temp_dir().join(format!("evorule_audit_a3_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // 模拟上一轮运行留下的 WAL：最大序号 7，含轮换分片与共享事实干扰项
        std::fs::write(dir.join("session_7.wal"), b"old-audit-chain-7").unwrap();
        std::fs::write(dir.join("session_3.wal"), b"old-audit-chain-3").unwrap();
        std::fs::write(dir.join("session_7.wal.2"), b"rotated-shard").unwrap();
        std::fs::write(dir.join("shared_facts.wal"), b"shared").unwrap();

        let core_eval = make_core_eval();
        let mgr = SessionManager::with_limits_and_wal_and_auto_verify(
            core_eval,
            100,
            100,
            Duration::from_secs(3600),
            Some(dir.clone()),
            4,
            false,
            0,
            false,
            1000,
            1,
        );

        // 首个新会话必须是 8（= max+1），不能复用 1~7 中任何一个
        let id = mgr.create_session().unwrap();
        assert_eq!(id, 8, "重启后首个会话 ID 必须接续既有 WAL 最大序号");

        // 旧审计链文件必须原样保留
        assert_eq!(
            std::fs::read(dir.join("session_7.wal")).unwrap(),
            b"old-audit-chain-7",
            "既有 WAL 被覆盖：AUDIT-A3 回归失败"
        );

        // 模拟二次重启：计数器应继续接续
        drop(mgr);
        let core_eval2 = make_core_eval();
        let mgr2 = SessionManager::with_limits_and_wal_and_auto_verify(
            core_eval2,
            100,
            100,
            Duration::from_secs(3600),
            Some(dir.clone()),
            4,
            false,
            0,
            false,
            1000,
            1,
        );
        let id2 = mgr2.create_session().unwrap();
        assert_eq!(id2, 9, "二次重启后计数器必须继续接续");

        let _handle = mgr2.close_session(id2).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    fn make_core_eval() -> Vec<JsonValue> {
        let mut params = BTreeMap::new();
        params.insert("attr".to_string(), JsonValue::string("x"));
        params.insert("delta".to_string(), JsonValue::Integer(1));
        let mut instr = BTreeMap::new();
        instr.insert("type".to_string(), JsonValue::string("increment"));
        instr.insert("params".to_string(), JsonValue::Object(params));
        vec![JsonValue::Object(instr)]
    }

    /// B4 回归（）：max_sessions 限额在高并发下不可突破。
    /// 原实现 check（load）与 reserve（fetch_add）分离，其间含 spawn 慢操作，
    /// 并发创建可越过限额（TOCTOU）。现 CAS 预留，N 线程 × max=1 恰成功 1 个。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_max_sessions_concurrent_creation_cannot_exceed() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc as StdArc;

        let core_eval = make_core_eval();
        // 注意：new() 第二参数是 max_rounds；B4 专项需显式 with_limits 设 max_sessions=1
        let mgr = StdArc::new(SessionManager::with_limits(
            core_eval,
            100,
            1,
            Duration::from_secs(3600),
        ));

        let ok = StdArc::new(AtomicUsize::new(0));
        let exceeded = StdArc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let mgr = mgr.clone();
            let ok = ok.clone();
            let exceeded = exceeded.clone();
            handles.push(tokio::task::spawn(async move {
                for _ in 0..25 {
                    match mgr.create_session() {
                        Ok(_) => {
                            ok.fetch_add(1, Ordering::SeqCst);
                        }
                        Err(SessionError::LimitExceeded { .. }) => {
                            exceeded.fetch_add(1, Ordering::SeqCst);
                        }
                        Err(e) => panic!("unexpected error: {e}"),
                    }
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        // 恰好成功 1 个（计数与在册一致），其余全部被限额拒绝
        assert_eq!(
            ok.load(Ordering::SeqCst),
            1,
            "max=1 时并发 200 次创建恰成功 1 次（TOCTOU 修复实证）"
        );
        assert_eq!(exceeded.load(Ordering::SeqCst), 199);
        assert_eq!(mgr.len(), 1);
    }

    #[tokio::test]
    async fn test_create_and_get_session() {
        let core_eval = make_core_eval();
        let mgr = SessionManager::new(core_eval, 100);

        let id1 = mgr.create_session().unwrap();
        let id2 = mgr.create_session().unwrap();
        assert_ne!(id1, id2);
        assert_eq!(mgr.len(), 2);

        assert!(mgr.get_session(id1).is_some());
        assert!(mgr.get_session(id2).is_some());
        assert!(mgr.get_session(999).is_none());
    }

    #[tokio::test]
    async fn test_close_session() {
        let core_eval = make_core_eval();
        let mgr = SessionManager::new(core_eval, 100);

        let id = mgr.create_session().unwrap();
        assert_eq!(mgr.len(), 1);

        let handle = mgr.close_session(id).unwrap();
        assert_eq!(mgr.len(), 0);

        assert!(mgr.get_session(id).is_none());

        assert!(matches!(
            mgr.close_session(id),
            Err(SessionError::NotFound { .. })
        ));

        drop(handle);
    }

    #[tokio::test]
    async fn test_list_sessions() {
        let core_eval = make_core_eval();
        let mgr = SessionManager::new(core_eval, 100);

        assert!(mgr.list_sessions().is_empty());

        let id1 = mgr.create_session().unwrap();
        let id2 = mgr.create_session().unwrap();

        let mut list = mgr.list_sessions();
        list.sort();
        assert_eq!(list, vec![id1, id2]);
    }

    #[tokio::test]
    async fn test_session_command_works() {
        let core_eval = make_core_eval();
        let mgr = SessionManager::new(core_eval, 100);

        let id = mgr.create_session().unwrap();
        let session = mgr.get_session(id).unwrap();

        let mut params = BTreeMap::new();
        params.insert("attr".to_string(), JsonValue::string("x"));
        params.insert("delta".to_string(), JsonValue::Integer(5));
        let mut instr = BTreeMap::new();
        instr.insert("type".to_string(), JsonValue::string("increment"));
        instr.insert("params".to_string(), JsonValue::Object(params));

        session
            .command_tx
            .send(Fact::Command {
                id: evorule_reactor::FactId(1),
                instruction: JsonValue::Object(instr),
            })
            .unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let history = session.facts_log.history();
        assert!(
            history.len() >= 2,
            "Expected at least 2 facts, got {}",
            history.len()
        );
    }

    #[tokio::test]
    async fn test_close_session_triggers_reactor_exit() {
        let core_eval = make_core_eval();
        let mgr = SessionManager::new(core_eval, 100);

        let id = mgr.create_session().unwrap();
        let handle = mgr.close_session(id).unwrap();

        let handle_inner = Arc::try_unwrap(handle).unwrap_or_else(|_| {
            panic!("Expected single reference to handle");
        });
        let result = handle_inner.join().await;
        assert!(result.is_ok(), "Expected graceful Ok(())");
    }

    #[tokio::test]
    async fn test_is_empty_and_len() {
        let core_eval = make_core_eval();
        let mgr = SessionManager::new(core_eval, 100);

        assert!(mgr.is_empty());
        assert_eq!(mgr.len(), 0);

        mgr.create_session().unwrap();
        assert!(!mgr.is_empty());
        assert_eq!(mgr.len(), 1);
    }

    #[tokio::test]
    async fn test_session_limit_exceeded() {
        let core_eval = make_core_eval();
        let mgr = SessionManager::with_limits(core_eval, 100, 2, Duration::from_secs(3600));

        let id1 = mgr.create_session().unwrap();
        let id2 = mgr.create_session().unwrap();
        assert_eq!(mgr.len(), 2);

        let result = mgr.create_session();
        assert!(matches!(
            result,
            Err(SessionError::LimitExceeded { current: 2, max: 2 })
        ));
        assert_eq!(mgr.len(), 2);

        let _handle = mgr.close_session(id1).unwrap();
        let id3 = mgr.create_session().unwrap();
        assert_eq!(mgr.len(), 2);
        assert!(mgr.get_session(id3).is_some());

        let _ = mgr.close_session(id2);
        let _ = mgr.close_session(id3);
    }

    #[tokio::test]
    async fn test_reap_expired() {
        let core_eval = make_core_eval();
        let mgr = SessionManager::with_limits(core_eval, 100, 100, Duration::from_millis(100));

        let id1 = mgr.create_session().unwrap();
        let id2 = mgr.create_session().unwrap();
        assert_eq!(mgr.len(), 2);

        tokio::time::sleep(Duration::from_millis(150)).await;

        let reaped = mgr.reap_expired();
        assert_eq!(reaped, 2);
        assert_eq!(mgr.len(), 0);
        assert!(mgr.get_session(id1).is_none());
        assert!(mgr.get_session(id2).is_none());
    }

    #[tokio::test]
    async fn test_touch_session_prevents_expiry() {
        let core_eval = make_core_eval();
        let mgr = SessionManager::with_limits(core_eval, 100, 100, Duration::from_millis(100));

        let id = mgr.create_session().unwrap();

        tokio::time::sleep(Duration::from_millis(60)).await;
        mgr.touch_session(id);

        tokio::time::sleep(Duration::from_millis(60)).await;
        let reaped = mgr.reap_expired();
        assert_eq!(reaped, 0);
        assert!(mgr.get_session(id).is_some());

        tokio::time::sleep(Duration::from_millis(120)).await;
        let reaped = mgr.reap_expired();
        assert_eq!(reaped, 1);
    }

    #[tokio::test]
    async fn test_sharding_distribution() {
        let core_eval = make_core_eval();
        let mgr = SessionManager::with_limits_and_wal(
            core_eval,
            100,
            100,
            Duration::from_secs(3600),
            None,
            4,
        );

        for _ in 0..100 {
            mgr.create_session().unwrap();
        }
        assert_eq!(mgr.len(), 100);

        let mut counts = [0; 4];
        for shard in &mgr.shards {
            let shard_guard = match lock_mutex(shard) {
                Ok(g) => g,
                Err(e) => e.into_inner(),
            };
            for id in shard_guard.keys() {
                let idx = *id as usize % 4;
                counts[idx] += 1;
            }
        }

        assert!(
            counts.iter().all(|&c| c > 0),
            "All shards should have sessions"
        );
        assert!(
            counts.iter().all(|&c| c < 50),
            "No shard should have too many sessions"
        );
    }

    // object_pool 相关测试已移除（性能优化功能已删除）

    #[tokio::test]
    async fn test_cross_session_causality_e2e() {
        let core_eval = make_core_eval();
        let mgr = SessionManager::new(core_eval, 100);

        // 1. 创建父会话
        let parent_id = mgr.create_session().unwrap();
        let parent = mgr.get_session(parent_id).unwrap();
        assert!(parent.parent_session_id().is_none());
        assert!(parent.initial_content_hash().is_none());

        // 2. 给父会话发送命令，触发状态变化（payload 不再是空的）
        let mut params = BTreeMap::new();
        params.insert("attr".to_string(), JsonValue::string("x"));
        params.insert("delta".to_string(), JsonValue::Integer(42));
        let mut instr = BTreeMap::new();
        instr.insert("type".to_string(), JsonValue::string("increment"));
        instr.insert("params".to_string(), JsonValue::Object(params));

        parent
            .command_tx
            .send(Fact::Command {
                id: evorule_reactor::FactId(1),
                instruction: JsonValue::Object(instr),
            })
            .unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // 3. 记录父会话当前 payload 哈希，作为预期值（审计⑥ C2: 审计链口径）
        let (parent_payload, _, _) = parent.facts_log.snapshot();
        let expected_hash = evorule_reactor::content_hash(&parent_payload).unwrap();

        // 4. 从父会话派生子会话
        let child_id = mgr.create_session_from_parent(parent_id).unwrap();
        let child = mgr.get_session(child_id).unwrap();

        // 5. 验证子会话的父会话 ID 和初始内容哈希
        assert_eq!(child.parent_session_id(), Some(parent_id));
        assert_eq!(child.initial_content_hash(), Some(expected_hash.as_str()));
        assert_eq!(
            child.content_hash_scheme(),
            ContentHashScheme::AuditChainCompact
        );
        assert_ne!(child_id, parent_id);
        assert_eq!(mgr.len(), 2);

        // 6. 给子会话也发命令，验证子会话独立运行
        let mut params2 = BTreeMap::new();
        params2.insert("attr".to_string(), JsonValue::string("x"));
        params2.insert("delta".to_string(), JsonValue::Integer(10));
        let mut instr2 = BTreeMap::new();
        instr2.insert("type".to_string(), JsonValue::string("increment"));
        instr2.insert("params".to_string(), JsonValue::Object(params2));

        child
            .command_tx
            .send(Fact::Command {
                id: evorule_reactor::FactId(1),
                instruction: JsonValue::Object(instr2),
            })
            .unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // 子会话应有自己独立的 facts 历史
        let child_history = child.facts_log.history();
        assert!(child_history.len() >= 2);

        // 7. 再从子会话派生孙会话，验证多级因果链
        let grandchild_id = mgr.create_session_from_parent(child_id).unwrap();
        let grandchild = mgr.get_session(grandchild_id).unwrap();

        assert_eq!(grandchild.parent_session_id(), Some(child_id));
        assert!(grandchild.initial_content_hash().is_some());
        // 孙会话的初始内容哈希应等于子会话创建时的 payload 快照哈希
        // （子会话刚创建还没执行命令时，状态等于初始状态）
        let (child_payload, _, _) = child.facts_log.snapshot();
        let child_hash = evorule_reactor::content_hash(&child_payload).unwrap();
        assert_eq!(grandchild.initial_content_hash(), Some(child_hash.as_str()));

        assert_eq!(mgr.len(), 3);

        // 8. 验证：从不存在的父会话创建应失败
        let result = mgr.create_session_from_parent(999_999);
        assert!(matches!(
            result,
            Err(SessionError::NotFound { id: 999_999 })
        ));

        // 9. 清理
        let _ = mgr.close_session(grandchild_id);
        let _ = mgr.close_session(child_id);
        let _ = mgr.close_session(parent_id);
    }

    #[tokio::test]
    async fn test_cross_session_causality_initial_state() {
        // 验证从刚创建（未执行任何命令）的父会话派生子会话
        let core_eval = make_core_eval();
        let mgr = SessionManager::new(core_eval, 100);

        let parent_id = mgr.create_session().unwrap();
        let parent = mgr.get_session(parent_id).unwrap();

        // 父会话刚创建，payload 是初始状态
        let (parent_payload, _, _) = parent.facts_log.snapshot();
        let expected_hash = blake3::hash(parent_payload.to_string().as_bytes())
            .to_hex()
            .to_string();

        let child_id = mgr.create_session_from_parent(parent_id).unwrap();
        let child = mgr.get_session(child_id).unwrap();

        assert_eq!(child.parent_session_id(), Some(parent_id));
        assert_eq!(child.initial_content_hash(), Some(expected_hash.as_str()));

        let _ = mgr.close_session(child_id);
        let _ = mgr.close_session(parent_id);
    }

    /// fork-from-archive：payload_from_wal_records 往返 + 篡改检出 + 版本边界
    #[tokio::test]
    async fn test_payload_from_wal_records_roundtrip_and_tamper() {
        use evorule_reactor::{read_wal_with_hash, tcb_to_serde};

        let dir = std::env::temp_dir().join(format!("evorule_fork_archive_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wal_path = dir.join("session_99.wal");

        // 1. 写一条带哈希链的 WAL（与生产 FactsLog::append 写入侧同源）
        let log = FactsLog::with_wal(&wal_path).unwrap();
        log.append(Fact::PayloadUpdate {
            id: FactId(1),
            path: "risk".to_string(),
            value: JsonValue::Integer(3),
        })
        .unwrap();
        log.append(Fact::PayloadUpdate {
            id: FactId(2),
            path: "amount".to_string(),
            value: JsonValue::Integer(50000),
        })
        .unwrap();
        let mut payload_map = BTreeMap::new();
        payload_map.insert("risk".to_string(), JsonValue::Integer(3));
        payload_map.insert("amount".to_string(), JsonValue::Integer(50000));
        let final_payload = JsonValue::Object(payload_map);
        log.append(Fact::StateTransition {
            id: FactId(3),
            cause: FactId(2),
            new_payload: final_payload.clone(),
            new_queue: Vec::new(),
        })
        .unwrap();
        drop(log);

        // 2. 读回归档记录，重放 payload
        let records = read_wal_with_hash(&wal_path).unwrap();
        assert_eq!(records.len(), 3);

        // 链尾：version 3，payload = StateTransition 的 new_payload
        let (payload, version) = payload_from_wal_records(&records, None).unwrap();
        assert_eq!(version, 3);
        assert_eq!(tcb_to_serde(&payload), tcb_to_serde(&final_payload));

        // Some(2)：重放到第二条 PayloadUpdate 之后
        let (payload2, version2) = payload_from_wal_records(&records, Some(2)).unwrap();
        assert_eq!(version2, 2);
        assert_eq!(
            tcb_to_serde(&payload2),
            serde_json::json!({"risk": 3, "amount": 50000})
        );

        // Some(0)：空状态
        let (empty, v0) = payload_from_wal_records(&records, Some(0)).unwrap();
        assert_eq!(v0, 0);
        assert!(tcb_to_serde(&empty).as_object().unwrap().is_empty());

        // 超界：InvalidVersion
        assert!(matches!(
            payload_from_wal_records(&records, Some(99)),
            Err(SessionError::InvalidVersion { version: 99 })
        ));

        // 3. 篡改检出：改第一条记录的 fact 内容 → content_hash 不匹配
        let mut tampered = records.clone();
        tampered[0].fact = Fact::PayloadUpdate {
            id: FactId(1),
            path: "risk".to_string(),
            value: JsonValue::Integer(999),
        };
        assert!(matches!(
            payload_from_wal_records(&tampered, None),
            Err(SessionError::ArchiveCorrupted { .. })
        ));

        // 4. 旧格式（无哈希）记录拒收
        let mut legacy = records.clone();
        legacy[1].content_hash = None;
        legacy[1].chain_hash = None;
        assert!(matches!(
            payload_from_wal_records(&legacy, None),
            Err(SessionError::ArchiveCorrupted { .. })
        ));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// fork-from-archive：create_session_from_initial_state 统一落点
    /// （父链接、初始哈希口径、子会话从初始状态独立续跑）
    #[tokio::test]
    async fn test_create_session_from_initial_state() {
        use evorule_reactor::tcb_to_serde;
        let dir = std::env::temp_dir().join(format!("evorule_fork_state_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // core_eval 必须是合法 meta 指令树：set x += 5（increment 不是合法 meta 指令）
        let mut set_params = BTreeMap::new();
        set_params.insert("attr".to_string(), JsonValue::string("x"));
        set_params.insert("operation".to_string(), JsonValue::string("add"));
        set_params.insert("value".to_string(), JsonValue::Integer(5));
        let mut set_instr = BTreeMap::new();
        set_instr.insert("type".to_string(), JsonValue::string("set"));
        set_instr.insert("params".to_string(), JsonValue::Object(set_params));
        let core_eval = vec![JsonValue::Object(set_instr)];

        let mgr = SessionManager::with_limits_and_wal_and_auto_verify(
            core_eval,
            100,
            100,
            Duration::from_secs(3600),
            Some(dir.clone()),
            4,
            false,
            0,
            false,
            1000,
            1,
        );

        // 模拟从归档链重放出的初始状态（x=10，对应版本 5；父会话 42 已不在内存）
        let mut payload = BTreeMap::new();
        payload.insert("x".to_string(), JsonValue::Integer(10));
        let child_id = mgr
            .create_session_from_initial_state(Some(42), JsonValue::Object(payload), 5)
            .unwrap();
        let child = mgr.get_session(child_id).unwrap();

        // 因果父链接与初始哈希口径（与内存 fork 同一落点）
        assert_eq!(child.parent_session_id(), Some(42));
        assert!(child.initial_content_hash().is_some());
        assert_eq!(
            child.content_hash_scheme(),
            ContentHashScheme::AuditChainCompact
        );

        // 子会话从初始状态续跑：任意命令触发 core_eval（set x += 5）→ x = 15
        let mut instr = BTreeMap::new();
        instr.insert("type".to_string(), JsonValue::string("tick"));
        child
            .command_tx
            .send(Fact::Command {
                id: evorule_reactor::FactId(1),
                instruction: JsonValue::Object(instr),
            })
            .unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

        let (snap, _, snap_version) = child.facts_log.snapshot();
        assert_eq!(
            tcb_to_serde(&snap)["x"],
            serde_json::json!(15),
            "子会话必须从 fork 初始状态 x=10 续跑（core_eval: x += 5）"
        );
        assert!(snap_version >= 6, "子会话版本应从初始版本 5 续起");
        assert!(child.facts_log.history().len() >= 2, "子会话审计链独立落盘");

        let _ = mgr.close_session(child_id);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 会话专属规则集 fork（沙盒编排消费面）：with_core_eval 只影响 fork 出的
    /// 会话，全局规则集不被修改，其他会话不受影响
    #[tokio::test]
    async fn test_create_session_from_parent_with_core_eval_override() {
        use evorule_reactor::tcb_to_serde;
        // 全局规则集为空：override 规则只应进 override 会话
        let mgr = SessionManager::new(vec![], 100);

        let root = mgr.create_session().unwrap();

        // override 规则集：set x += 5（meta 指令树，任意命令触发）
        let mut set_params = BTreeMap::new();
        set_params.insert("attr".to_string(), JsonValue::string("x"));
        set_params.insert("operation".to_string(), JsonValue::string("add"));
        set_params.insert("value".to_string(), JsonValue::Integer(5));
        let mut set_instr = BTreeMap::new();
        set_instr.insert("type".to_string(), JsonValue::string("set"));
        set_instr.insert("params".to_string(), JsonValue::Object(set_params));
        let override_rules = vec![JsonValue::Object(set_instr)];

        let sandbox = mgr
            .create_session_from_parent_at_version_with_core_eval(root, None, Some(override_rules))
            .unwrap();
        // 对照组：常规 fork（无 override）→ 使用全局规则集（空集）
        let plain = mgr
            .create_session_from_parent_at_version(root, None)
            .unwrap();

        let mut instr = BTreeMap::new();
        instr.insert("type".to_string(), JsonValue::string("tick"));
        let send_tick = |id: u64| {
            mgr.get_session(id)
                .unwrap()
                .command_tx
                .send(Fact::Command {
                    id: evorule_reactor::FactId(1),
                    instruction: JsonValue::Object(instr.clone()),
                })
                .unwrap();
        };
        send_tick(sandbox);
        send_tick(plain);
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

        let (sb_snap, _, _) = mgr.get_session(sandbox).unwrap().facts_log.snapshot();
        assert_eq!(
            tcb_to_serde(&sb_snap)["x"],
            serde_json::json!(5),
            "会话专属规则集生效：set x += 5 仅在 fork 会话执行"
        );
        let (pl_snap, _, _) = mgr.get_session(plain).unwrap().facts_log.snapshot();
        assert!(
            tcb_to_serde(&pl_snap).get("x").is_none(),
            "非 override fork 使用全局空规则集，命令不被 override 规则执行"
        );

        // 全局规则集不被修改：其后新建的常规会话仍不受影响
        let after = mgr.create_session().unwrap();
        send_tick(after);
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
        let (af_snap, _, _) = mgr.get_session(after).unwrap().facts_log.snapshot();
        assert!(
            tcb_to_serde(&af_snap).get("x").is_none(),
            "全局规则集不被 override 修改（后续会话仍用空集）"
        );

        for id in [root, sandbox, plain, after] {
            let _ = mgr.close_session(id);
        }
    }

    // ===== pending I/O 超时预算贯通 =====

    /// io_request 元指令 core_eval：任意命令触发一次 pending I/O（不自动响应）
    fn make_io_core_eval() -> Vec<JsonValue> {
        let mut io_params = BTreeMap::new();
        io_params.insert("io_type".to_string(), JsonValue::string("slow_call"));
        let mut instr = BTreeMap::new();
        instr.insert("type".to_string(), JsonValue::string("io_request"));
        instr.insert("params".to_string(), JsonValue::Object(io_params));
        vec![JsonValue::Object(instr)]
    }

    /// 向会话发送一条触发 pending I/O 的命令，并在时限内等待超时 `Fact::Error`
    async fn expect_timeout_error(
        mgr: &SessionManager,
        session_id: SessionId,
        deadline: Duration,
    ) -> String {
        let session = mgr.get_session(session_id).unwrap();
        let mut rx = session.event_tx.subscribe();

        let mut instr = BTreeMap::new();
        instr.insert("type".to_string(), JsonValue::string("tick"));
        session
            .command_tx
            .send(Fact::Command {
                id: evorule_reactor::FactId(1),
                instruction: JsonValue::Object(instr),
            })
            .unwrap();

        let start = tokio::time::Instant::now();
        let mut saw_io_request = false;
        loop {
            assert!(
                start.elapsed() < deadline,
                "时限内未收到超时 Fact::Error（io_request 已见={saw_io_request}）"
            );
            match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                Ok(Ok(Fact::IoRequest { .. })) => saw_io_request = true,
                Ok(Ok(Fact::Error { message, .. })) => {
                    assert!(
                        message.contains("timed out"),
                        "首个 Fact::Error 必须是超时错误而非其他失败: {message}"
                    );
                    assert!(saw_io_request, "超时 Error 前必须先登记 pending I/O");
                    return message;
                }
                Ok(Ok(_)) => {}
                _ => {}
            }
        }
    }

    /// 自定义超时预算贯通实证（create_session 路径）：error 阈值 150ms 时，
    /// 未响应的 pending I/O 在 2s 内触发超时 Fact::Error（引擎 fail-fast
    /// 语义经 SessionManager 贯通到达会话事件面）。
    #[tokio::test]
    async fn test_io_timeouts_custom_error_threshold_fires_fast() {
        let core_eval = make_io_core_eval();
        let mgr = SessionManager::new(core_eval, 100).with_io_timeouts(
            Some(Duration::from_millis(50)),
            Some(Duration::from_millis(150)),
            Some(Duration::from_millis(50)),
        );
        let id = mgr.create_session().unwrap();
        let msg = expect_timeout_error(&mgr, id, Duration::from_secs(2)).await;
        assert!(msg.contains("after"), "超时消息应携带实际预算: {msg}");
        let _ = mgr.close_session(id);
    }

    /// 自定义超时预算贯通实证（派生会话路径）：derive 会话的反应器
    /// 同样吃 SessionManager 的超时预算（第二处 builder 接线）。
    #[tokio::test]
    async fn test_io_timeouts_pass_through_to_derived_session() {
        let core_eval = make_io_core_eval();
        let mgr = SessionManager::new(core_eval, 100).with_io_timeouts(
            Some(Duration::from_millis(50)),
            Some(Duration::from_millis(150)),
            Some(Duration::from_millis(50)),
        );
        let parent_id = mgr.create_session().unwrap();
        let child_id = mgr.create_session_from_parent(parent_id).unwrap();
        expect_timeout_error(&mgr, child_id, Duration::from_secs(2)).await;
        let _ = mgr.close_session(child_id);
        let _ = mgr.close_session(parent_id);
    }

    // ===== G-A2 锚点穿线端到端 =====

    fn anchor_cfg(interval: usize) -> super::SessionAnchorConfig {
        super::SessionAnchorConfig {
            signer_seed: [7u8; 32],
            key_id: "k1".to_string(),
            engine_id: "evorule-test/0.8".to_string(),
            interval,
        }
    }

    #[tokio::test]
    async fn test_session_anchor_end_to_end() {
        // 穿线：配置锚点的 SessionManager → 新会话审计器自动启锚 → 封条+验证全绿
        let mgr = super::SessionManager::new(vec![], 10).with_anchor_config(anchor_cfg(2));
        let sid = mgr.create_session().map_err(|e| format!("{e:?}")).unwrap();
        let session = mgr.get_session(sid).unwrap();
        // 未有事实时手动封一个空区间锚点（封条语义）
        let anchor = session
            .seal_anchor_now()
            .map_err(|e| format!("{e:?}"))
            .unwrap();
        assert_eq!(anchor.seq, 0);
        assert_eq!(anchor.fact_range.hi, 0);
        // verify_anchors：结构+签名+绑定全绿
        let mut keys = std::collections::BTreeMap::new();
        keys.insert(
            "k1".to_string(),
            crate::signing::AuditSigner::from_bytes([7u8; 32]).verifying_hex(),
        );
        assert!(
            session.verify_anchors(&keys).is_ok(),
            "空链锚点验证必须通过"
        );
    }

    #[tokio::test]
    async fn test_session_anchor_default_off() {
        // 未配 anchor_config：锚点关——seal 返回 Err（对齐 J-T6 兼容判据）
        let mgr = super::SessionManager::new(vec![], 10);
        let sid = mgr.create_session().map_err(|e| format!("{e:?}")).unwrap();
        let session = mgr.get_session(sid).unwrap();
        assert!(
            session.seal_anchor_now().is_err(),
            "未启用锚点时 seal 必须 Err"
        );
        assert!(session.anchors().is_empty());
    }

    #[test]
    fn test_io_timeouts_defaults_unchanged() {
        assert_eq!(DEFAULT_IO_WARN_TIMEOUT, Duration::from_secs(30));
        assert_eq!(DEFAULT_IO_ERROR_TIMEOUT, Duration::from_secs(60));
        assert_eq!(DEFAULT_IO_TIMEOUT_CHECK_INTERVAL, Duration::from_secs(5));

        let mgr = SessionManager::new(make_core_eval(), 100);
        assert_eq!(mgr.io_warn_timeout, Duration::from_secs(30));
        assert_eq!(mgr.io_error_timeout, Duration::from_secs(60));
        assert_eq!(mgr.io_timeout_check_interval, Duration::from_secs(5));

        let mgr2 = SessionManager::new(make_core_eval(), 100).with_io_timeouts(None, None, None);
        assert_eq!(mgr2.io_warn_timeout, Duration::from_secs(30));
        assert_eq!(mgr2.io_error_timeout, Duration::from_secs(60));
        assert_eq!(mgr2.io_timeout_check_interval, Duration::from_secs(5));

        let mgr3 = SessionManager::new(make_core_eval(), 100).with_io_timeouts(
            None,
            Some(Duration::from_secs(3600)),
            None,
        );
        assert_eq!(mgr3.io_warn_timeout, Duration::from_secs(30));
        assert_eq!(mgr3.io_error_timeout, Duration::from_secs(3600));
        assert_eq!(mgr3.io_timeout_check_interval, Duration::from_secs(5));
    }
}

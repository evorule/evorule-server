// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Append-Only 事实审计链
//!
//! # 设计依据
//! 基于《02_反应式数据执行器》§2.2，FactsLog 是系统的唯一真相存储：
//! - 所有 Fact 追加到不可变历史（`history`）
//! - 当前物化快照由最近的 StateTransition 确定
//! - 单调递增版本号（StateTransition / IoResponse 时 +1）
//! - 提供审计重放接口 `read_from()`
//!
//! # 并发模型
//! 使用 `std::sync::RwLock`（非 `tokio::sync::RwLock`），因为：
//! - 写操作极快（push + 字段更新），不跨 await 持锁
//! - 反应器是唯一写入者，竞争极低
//! - 读取者（审计器）可同步获取快照

use crate::fact::{Fact, FactId};
#[cfg(feature = "persistence")]
use crate::wal::{FactWalStore, WalWriter, DEFAULT_MAX_WAL_SIZE_BYTES};
use evorule_tcb::path::resolve_path_mut;
use evorule_tcb::JsonValue;
#[cfg(kani)]
use std::cell::RefCell;
use std::collections::BTreeMap;
#[cfg(feature = "persistence")]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
#[cfg(not(kani))]
use std::sync::RwLock;

/// FactsLog 错误类型
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FactsLogError {
    /// 版本号溢出
    VersionOverflow,
    /// 哈希链计算失败（两套 WAL 合并：哈希链提升到 tier1）
    ///
    /// 携带错误描述字符串。哈希计算是纯函数，失败通常意味着 Fact 序列化异常。
    /// 此变体不依赖 persistence feature，因为哈希链在纯内存模式下也需维护
    /// （`last_hash` 字段始终存在）。
    HashError(String),
    /// 读取起点落在压缩点之前（A-3 `compact()` 后内存前缀已丢弃）
    ///
    /// 与"该版本前本就没有事实"严格区分：后者返回**空 Vec**，前者返回本变体。
    /// 二者此前共用空 Vec，导致"历史被压实丢弃"静默伪装成"历史上什么都没发生"
    /// （审计校验因此漏检，见 `SharedFactsLog::verify_causal_consistency` 的已知边界）。
    ///
    /// 取回压缩点前的完整历史只能基于 WAL 离线重放，内存投影不再持有。
    CompactedRead {
        /// 调用方请求读取的起始版本
        from_version: u64,
        /// 压缩点版本（= 压缩时的 `last_stable_version`）
        compacted_version: u64,
        /// 已因压缩从内存投影丢弃的事实条数
        compacted_count: usize,
    },
    #[cfg(feature = "persistence")]
    /// WAL 写入或读取失败（P0-1）
    ///
    /// 携带错误描述字符串。WAL 写失败时内存状态尚未更新，调用方可决定
    /// 是否终止反应器（避免内存与磁盘状态分叉）。
    WalError(String),
}

impl core::fmt::Display for FactsLogError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FactsLogError::VersionOverflow => write!(f, "facts log version overflow"),
            FactsLogError::HashError(msg) => write!(f, "facts log hash error: {msg}"),
            FactsLogError::CompactedRead {
                from_version,
                compacted_version,
                compacted_count,
            } => write!(
                f,
                "facts log read from version {from_version} precedes compaction point \
                 {compacted_version} ({compacted_count} facts dropped from memory projection; \
                 replay from WAL for the full prefix)"
            ),
            #[cfg(feature = "persistence")]
            FactsLogError::WalError(msg) => write!(f, "facts log WAL error: {msg}"),
        }
    }
}

impl std::error::Error for FactsLogError {}

/// 压缩快照（A-3：手动调用 compact() 后生成）
///
/// 记录压缩点处的完整状态，使 history 中该版本之前的事实可安全丢弃。
/// 审计链完整性由 WAL 文件保证（WAL 保留全量记录）。
///
/// `snapshot`/`queue`/`last_hash` 字段当前仅写入（供未来从压缩点恢复状态/审计链时读取），
/// 故标 `allow(dead_code)` 避免误报。
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct CompactedSnapshot {
    /// 压缩点版本号（= 压缩时的 last_stable_version）
    version: u64,
    /// 压缩点处的状态快照
    snapshot: JsonValue,
    /// 压缩点处的指令队列
    queue: Vec<JsonValue>,
    /// 压缩点处的哈希链尾（从压缩点恢复后可继续验证审计链）
    last_hash: String,
    /// 已丢弃的事实数量
    compacted_count: usize,
}

/// 内部可变状态
struct FactsLogInner {
    /// Append-Only 历史，元组为 (追加前的版本号, Fact)
    ///
    /// 版本号用于 `read_from()` 审计重放：返回所有 `version_before >= from_version` 的事实。
    history: Vec<(u64, Fact)>,

    /// 当前物化快照（由最近的 StateTransition 确定）
    current_snapshot: JsonValue,

    /// 当前指令队列（由最近的 StateTransition 确定）
    current_queue: Vec<JsonValue>,

    /// 单调递增版本号（StateTransition / IoResponse 时 +1）
    version: u64,

    /// 最后稳定时的版本（Stable 事实时记录）
    last_stable_version: u64,

    /// 审计链末尾哈希（两套 WAL 合并：哈希链提升到 tier1）
    ///
    /// 初始为 `"genesis"`，每次 `append` 时更新为新的链哈希。
    /// 用于 WAL 持久化和 CLI 验证。
    last_hash: String,

    #[cfg(feature = "persistence")]
    /// 可选的 WAL 存储后端（P0-1；起为可替换 trait 对象）
    ///
    /// - `Some`：`append()` 时先 write-ahead 写后端再更新内存
    /// - `None`：纯内存模式（兼容旧 API，如 `new()` / `with_initial_payload()`）
    ///
    /// 默认后端 = 文件 `WalWriter`（`recover*` 恢复后挂载，行为不变）；
    /// 自定义后端经 `with_wal_store` 挂载（如 `MemoryWalStore`）。
    /// `recover()` 重放期间临时为 `None`，重放完成后挂载以继续追加。
    wal: Option<Box<dyn FactWalStore>>,

    #[cfg(feature = "persistence")]
    /// 是否在 WAL flush 后执行 fsync（P02）
    ///
    /// 启用后在每次 WAL 写入后执行 `sync_all()`，确保断电时数据不丢失。
    /// 性能开销较大，默认禁用。
    fsync_on_flush: bool,

    #[cfg(feature = "persistence")]
    /// WAL 文件最大大小（字节，P03）
    ///
    /// 达到此大小后自动轮换文件（0 表示不轮换）。
    /// 默认值为 `DEFAULT_MAX_WAL_SIZE_BYTES`（100MB）。
    max_wal_size_bytes: u64,

    /// 索引 1：版本号 → history 首次出现的下标（A-3：加速 read_from，O(log n) 定位）
    version_index: BTreeMap<u64, usize>,

    /// 索引 2：FactId → history 下标（A-3：加速因果链查询）
    fact_id_index: BTreeMap<FactId, usize>,

    /// 索引 3：完整 path → history 下标列表（A-3：加速 facts_by_path_prefix）
    path_index: BTreeMap<String, Vec<usize>>,

    /// 压缩快照（A-3：手动调用 compact() 后填充，None = 未压缩）
    compacted_snapshot: Option<CompactedSnapshot>,
    /// fork/派生基态锚定事实的 id 分配器（M-4；独立于常规 FactId 流）
    next_anchor_id: u64,
}

/// 将一条 Fact 的效果应用到内存状态（快照/队列/版本/last_stable）
///
/// [`FactsLog::append`]（进程内追加）与 WAL 重放（`replay_wal_records` /
/// `recover_with_options`）的**单一事实应用点**：两条路径的版本推进与
/// 快照更新语义由本函数构造性保证一致。历史教训：双份实现曾漂移导致
/// WAL 重放不推进版本（与 append 不一致），此后收敛为单点。
///
/// 版本规则：StateTransition / IoResponse / PayloadUpdate 推进 +1；
/// Stable 记录 last_stable；Command / IoRequest / Error / TransitionTrace /
/// Violation 为记录性或触发事实，不改快照不推进版本。
///
/// Kani 模式：简化为仅 version / last_stable 推进（跳过 FixedMap/Vec
/// 路径的快照更新——CBMC 状态爆炸；哈希链正确性由既有 proof 覆盖，
/// 快照路径不在证明范围）。
fn apply_fact_to_state(inner: &mut FactsLogInner, fact: &Fact) -> Result<(), FactsLogError> {
    #[cfg(kani)]
    {
        match fact {
            Fact::StateTransition { .. } | Fact::IoResponse { .. } | Fact::PayloadUpdate { .. } => {
                inner.version = inner
                    .version
                    .checked_add(1)
                    .ok_or(FactsLogError::VersionOverflow)?;
            }
            Fact::Stable { .. } => {
                inner.last_stable_version = inner.version;
            }
            // 显式匹配剩余变体（门禁禁止 Fact match 通配符 _）
            Fact::Command { .. }
            | Fact::IoRequest { .. }
            | Fact::Error { .. }
            | Fact::TransitionTrace { .. }
            | Fact::Violation { .. } => {}
        }
        return Ok(());
    }
    #[cfg(not(kani))]
    match fact {
        Fact::StateTransition {
            new_payload,
            new_queue,
            ..
        } => {
            inner.current_snapshot = new_payload.clone();
            inner.current_queue = new_queue.clone();
            inner.version = inner
                .version
                .checked_add(1)
                .ok_or(FactsLogError::VersionOverflow)?;
        }
        Fact::IoResponse { .. } => {
            inner.version = inner
                .version
                .checked_add(1)
                .ok_or(FactsLogError::VersionOverflow)?;
        }
        Fact::Stable { .. } => {
            inner.last_stable_version = inner.version;
        }
        Fact::PayloadUpdate { path, value, .. } => {
            if let Some(target) = resolve_path_mut(&mut inner.current_snapshot, path) {
                *target = value.clone();
            } else {
                let parts: Vec<&str> = path.split('.').collect();
                if !parts.is_empty() {
                    if parts.len() == 1 && !path.contains('[') {
                        if let JsonValue::Object(map) = &mut inner.current_snapshot {
                            map.insert(path.clone(), value.clone());
                        }
                    } else {
                        let mut current = &mut inner.current_snapshot;
                        for (i, &part) in parts.iter().enumerate() {
                            if i == parts.len() - 1 {
                                if let JsonValue::Object(map) = current {
                                    map.insert(part.to_string(), value.clone());
                                }
                            } else if let JsonValue::Object(map) = current {
                                if !map.contains_key(part) {
                                    map.insert(part.to_string(), JsonValue::empty_object());
                                }
                                if let Some(next) = map.get_mut(part) {
                                    current = next;
                                } else {
                                    break;
                                }
                            } else {
                                break;
                            }
                        }
                    }
                }
            }
            inner.version = inner
                .version
                .checked_add(1)
                .ok_or(FactsLogError::VersionOverflow)?;
        }
        // 记录性事实与触发事实：不修改快照，版本号不变
        Fact::Command { .. } | Fact::IoRequest { .. } | Fact::Error { .. } => {}
        Fact::TransitionTrace { .. } => {
            // 记录性事实，不修改快照，版本号不变（与 Command 同类）
        }
        Fact::Violation { .. } => {
            // 违规拦截事实：记录性事实，不修改快照，版本号不变
            // （违规动作被拒，payload/queue 保持原样，与 TransitionTrace 同类）
        }
    }
    Ok(())
}

/// 内部锁包装器
///
/// Kani 模式下用 `RefCell` 替代 `RwLock`，避免 futex 同步原语导致 CBMC 状态爆炸
/// （Kani 对 `RwLock::read`/`write` 建模为 futex_wait 系统调用，522 次路径展开
/// 导致 `proof_fact_log_append_monotonic` 超时）。Kani proof 是单线程的，无需同步，
/// `RefCell::borrow`/`borrow_mut` 足够且建模高效。
///
/// 非 Kani 模式保持 `RwLock` 语义不变（多读取者并发）。
#[cfg(not(kani))]
struct FactsLogLock(RwLock<FactsLogInner>);
#[cfg(kani)]
struct FactsLogLock(RefCell<FactsLogInner>);

// SAFETY: Kani proof 是单线程的，RefCell 不会被并发访问。
// `tokio::spawn`（reactor.rs）要求 future 为 Send，而 `Arc<RefCell>` 因 RefCell
// 的 !Sync 导致 !Send。Kani 模式下 reactor.rs 的 spawn 代码不会被实际执行
// （Kani 只验证指定 harness），此 impl 仅用于满足编译期 Send 检查。
#[cfg(kani)]
#[allow(unsafe_code)]
unsafe impl Sync for FactsLogLock {}

#[cfg(not(kani))]
impl FactsLogLock {
    fn new(inner: FactsLogInner) -> Self {
        Self(RwLock::new(inner))
    }
    fn read(&self) -> std::sync::RwLockReadGuard<'_, FactsLogInner> {
        self.0.read().unwrap_or_else(|e| e.into_inner())
    }
    fn write(&self) -> std::sync::RwLockWriteGuard<'_, FactsLogInner> {
        self.0.write().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(kani)]
impl FactsLogLock {
    fn new(inner: FactsLogInner) -> Self {
        Self(RefCell::new(inner))
    }
    fn read(&self) -> std::cell::Ref<'_, FactsLogInner> {
        self.0.borrow()
    }
    fn write(&self) -> std::cell::RefMut<'_, FactsLogInner> {
        self.0.borrow_mut()
    }
}

/// 从 WAL 记录序列重放构建纯内存 FactsLog（无文件系统、无写句柄挂载）
///
/// `FactsLog::recover_with_options` 的纯核心，也是离线重放/验证的共用入口：
/// 事实应用语义与进程内 [`FactsLog::append`] 由 [`apply_fact_to_state`] 单点
/// 保证一致。哈希链恢复口径与挂载版 recover 一致：新格式（含 chain_hash
/// 字段）取尾条 chain_hash；旧格式或混合格式全链重算。
#[cfg(feature = "persistence")]
pub fn replay_wal_records(records: &[crate::wal::WalRecord]) -> Result<FactsLog, FactsLogError> {
    let log = FactsLog::new();
    {
        let mut inner = log.inner.write();

        // 跟踪是否有任何记录带哈希字段
        let mut has_hash_records = false;

        for record in records {
            let version_before = record.version_before;
            let fact = &record.fact;

            if record.chain_hash.is_some() {
                has_hash_records = true;
            }

            inner.history.push((version_before, fact.clone()));
            // A-3：重建索引
            let idx = inner.history.len() - 1;
            inner.version_index.entry(version_before).or_insert(idx);
            inner.fact_id_index.insert(fact.id(), idx);
            if let Fact::PayloadUpdate { path, .. } = fact {
                inner.path_index.entry(path.clone()).or_default().push(idx);
            }
            apply_fact_to_state(&mut inner, fact)?;
        }

        // 恢复审计链哈希：新格式取尾条 chain_hash；旧格式/混合格式全链重算
        if has_hash_records {
            if let Some(last_record) = records.last() {
                if let Some(chain_hash) = &last_record.chain_hash {
                    inner.last_hash = chain_hash.clone();
                } else {
                    let facts: Vec<Fact> = records.iter().map(|r| r.fact.clone()).collect();
                    inner.last_hash = crate::hash::compute_chain_hash(&facts)
                        .map_err(|e| FactsLogError::WalError(format!("hash recover error: {e}")))?;
                }
            }
        } else {
            let facts: Vec<Fact> = records.iter().map(|r| r.fact.clone()).collect();
            inner.last_hash = crate::hash::compute_chain_hash(&facts)
                .map_err(|e| FactsLogError::WalError(format!("hash recover error: {e}")))?;
        }
    }
    Ok(log)
}

/// Append-Only 事实审计链
///
/// 所有组件共享同一个 `FactsLog` 实例（通过 `Arc` 克隆）。
/// 反应器是唯一写入者，审计器/治理层是读取者。
#[derive(Clone)]
pub struct FactsLog {
    inner: Arc<FactsLogLock>,
    /// WAL 写入连续失败计数（修复，2026-08-27）
    ///
    /// - `append()` 中 WAL 写失败时递增、成功时清零
    /// - 达到 `WAL_FAIL_TERMINATE_THRESHOLD`（3）后，后续每次失败调用
    ///   `on_wal_failure_exhausted` 回调通知 reactor 终止会话
    /// - AtomicU64 而非放 FactsLogInner：重试路径需要避免长时间持写锁
    #[cfg(feature = "persistence")]
    wal_fail_count: Arc<AtomicU64>,
    /// 连续失败达阈值后的升级回调（由 reactor 设置，用于终止会话并
    /// 发射面向用户的 Error fact）
    #[cfg(feature = "persistence")]
    on_wal_failure_exhausted: WalFailureExhaustedCallback,
}

/// WAL 连续失败升级回调类型
#[cfg(feature = "persistence")]
type WalFailureExhaustedCallback = Arc<std::sync::Mutex<Option<Box<dyn Fn(&str) + Send + Sync>>>>;

#[cfg(feature = "persistence")]
/// WAL 写入连续失败终止阈值（用户决策：方案 b，2026-08-27）
pub const WAL_FAIL_TERMINATE_THRESHOLD: u64 = 3;

#[cfg(feature = "persistence")]
/// 生成面向用户的 WAL 故障自助诊断信息（方案 b 用户指导要求）
///
/// 根因均为部署环境问题而非代码缺陷；信息按可能性排序给出操作指引，
/// 使运维/用户无需阅读源码即可自行恢复。
fn wal_failure_guidance(error_msg: &str) -> String {
    format!(
        "审计日志(WAL)写入已连续 {} 次失败，本会话已被终止以保证可回放性不被静默破坏。\
错误详情: {error_msg}。\
该故障源于磁盘/权限等部署环境问题，请按以下顺序排查：
1. 检查磁盘空间是否已满（df -h / Windows 资源管理器查看 WAL 所在盘剩余空间），清理后重启服务即可自动恢复；
2. 检查进程对 WAL 目录是否有写权限（目录可能被迁移或 ACL 变更）；
3. 若 WAL 位于网络盘/NAS，检查挂载是否掉线；
4. 排查后重启 evorule-server；recover 会从已有 WAL 完整重建状态，不丢数据。
注意：本会话中断前的事实已在最后成功写入的 WAL 记录内，重启后可通过 /replay 审计。",
        WAL_FAIL_TERMINATE_THRESHOLD
    )
}

impl std::fmt::Debug for FactsLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.read();
        let mut s = f.debug_struct("FactsLog");
        s.field("version", &inner.version)
            .field("history_len", &inner.history.len());
        #[cfg(feature = "persistence")]
        {
            s.field("has_wal", &inner.wal.is_some());
        }
        s.finish()
    }
}

impl FactsLog {
    /// 统一构造（初始化 WAL 失败计数器与升级回调）
    fn with_inner(inner: FactsLogInner) -> Self {
        Self {
            inner: Arc::new(FactsLogLock::new(inner)),
            #[cfg(feature = "persistence")]
            wal_fail_count: Arc::new(AtomicU64::new(0)),
            #[cfg(feature = "persistence")]
            on_wal_failure_exhausted: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// 创建空的 FactsLog（初始版本为 0，payload 为空对象，无 WAL）
    pub fn new() -> Self {
        Self::with_inner(FactsLogInner {
            history: Vec::new(),
            current_snapshot: JsonValue::empty_object(),
            current_queue: Vec::new(),
            version: 0,
            last_stable_version: 0,
            last_hash: String::from("genesis"),
            #[cfg(feature = "persistence")]
            wal: None,
            #[cfg(feature = "persistence")]
            fsync_on_flush: false,
            #[cfg(feature = "persistence")]
            max_wal_size_bytes: DEFAULT_MAX_WAL_SIZE_BYTES,
            version_index: BTreeMap::new(),
            fact_id_index: BTreeMap::new(),
            path_index: BTreeMap::new(),
            compacted_snapshot: None,
            next_anchor_id: 1_000_000_000,
        })
    }

    /// 创建空的 FactsLog 并设置初始 payload
    pub fn with_initial_payload(payload: JsonValue) -> Self {
        let log = Self::new();
        {
            let mut inner = log.inner.write();
            inner.current_snapshot = payload;
        }
        log
    }

    #[cfg(feature = "persistence")]
    /// 注册 WAL 连续失败达阈值的升级回调（方案 b）
    ///
    /// 回调在 `append()` 持有写锁之外被调用，参数为面向用户的
    /// 自助诊断信息（含恢复指导）。reactor 用它终止会话并发射 Error fact。
    /// 首次 `append` 前设置；重复调用覆盖旧回调。
    pub fn set_on_wal_failure_exhausted<F>(&self, callback: F)
    where
        F: Fn(&str) + Send + Sync + 'static,
    {
        if let Ok(mut guard) = self.on_wal_failure_exhausted.lock() {
            *guard = Some(Box::new(callback));
        }
    }

    #[cfg(feature = "persistence")]
    /// 当前 WAL 连续失败计数（监控/测试用）
    pub fn wal_fail_count(&self) -> u64 {
        self.wal_fail_count.load(Ordering::SeqCst)
    }

    /// 设置初始状态（用于 fork 场景）
    ///
    /// 设置初始 payload 和版本号，但不增加版本计数。
    /// 这用于从父会话 fork 时继承状态。
    pub fn set_initial_state(&self, payload: JsonValue, version: u64) {
        let mut inner = self.inner.write();
        // M-4：fork/派生基态 digest 锚定入链（血统可审：篡改基态即链失配；
        // 全文经 digest 引用不膨胀链——由快照 API 消息，信息零丢失）
        let digest = crate::hash::content_hash(&payload).unwrap_or_default();
        let anchor_value = JsonValue::object_from_pairs(&[
            ("digest", JsonValue::string(digest)),
            ("version", JsonValue::integer(version as i64)),
        ]);
        let anchor_id = FactId(inner.next_anchor_id);
        inner.next_anchor_id = inner.next_anchor_id.saturating_add(1);
        let fact = Fact::PayloadUpdate {
            id: anchor_id,
            path: "__fork__.initial_state".to_string(),
            value: anchor_value,
        };
        // 先写 WAL 再改内存（write-ahead 语义）
        drop(inner);
        let _ = self.append(fact); // best-effort：失败由调用方 WAL 机制处理
        let mut inner = self.inner.write();
        inner.current_snapshot = payload;
        inner.version = version;
        inner.last_stable_version = version;
    }

    #[cfg(feature = "persistence")]
    /// 创建带 WAL 持久化的 FactsLog（P0-1）
    ///
    /// 全新启动场景：truncate 已有 WAL 文件，从空状态开始。
    /// 后续所有 `append()` 调用都会先 write-ahead 写入 WAL 再更新内存。
    ///
    /// # 错误
    /// - `WalError`：WAL 文件创建/打开失败
    pub fn with_wal<P: AsRef<std::path::Path>>(path: P) -> Result<Self, FactsLogError> {
        Self::with_wal_and_fsync(path, false)
    }

    #[cfg(feature = "persistence")]
    /// 创建带 WAL 持久化和 fsync 的 FactsLog（P02）
    ///
    /// 与 `with_wal` 相同，但启用 fsync 确保断电时数据不丢失。
    ///
    /// # 参数
    /// - `path`: WAL 文件路径
    /// - `fsync`: 是否在每次 flush 后执行 fsync
    ///
    /// # 错误
    /// - `WalError`：WAL 文件创建/打开失败
    pub fn with_wal_and_fsync<P: AsRef<std::path::Path>>(
        path: P,
        fsync: bool,
    ) -> Result<Self, FactsLogError> {
        Self::with_wal_options(path, DEFAULT_MAX_WAL_SIZE_BYTES, fsync)
    }

    #[cfg(feature = "persistence")]
    /// 创建带 WAL 持久化、轮换和 fsync 的 FactsLog（P03）
    ///
    /// # 参数
    /// - `path`: WAL 文件路径
    /// - `max_wal_size_bytes`: 单个 WAL 文件最大大小（0 表示不轮换）
    /// - `fsync`: 是否在每次 flush 后执行 fsync
    ///
    /// # 错误
    /// - `WalError`：WAL 文件创建/打开失败
    pub fn with_wal_options<P: AsRef<std::path::Path>>(
        path: P,
        max_wal_size_bytes: u64,
        fsync: bool,
    ) -> Result<Self, FactsLogError> {
        let wal = WalWriter::create_with_options(path, max_wal_size_bytes, fsync)
            .map_err(|e| FactsLogError::WalError(e.to_string()))?;
        Ok(Self::with_inner(FactsLogInner {
            history: Vec::new(),
            current_snapshot: JsonValue::empty_object(),
            current_queue: Vec::new(),
            version: 0,
            last_stable_version: 0,
            last_hash: String::from("genesis"),
            wal: Some(Box::new(wal)),
            fsync_on_flush: fsync,
            max_wal_size_bytes,
            version_index: BTreeMap::new(),
            fact_id_index: BTreeMap::new(),
            path_index: BTreeMap::new(),
            compacted_snapshot: None,
            next_anchor_id: 1_000_000_000,
        }))
    }

    #[cfg(feature = "persistence")]
    /// 创建挂载自定义存储后端的 FactsLog（存储层 trait 抽象）
    ///
    /// 与 `with_wal*` 系列同语义（write-ahead：`append()` 先写后端再更新内存），
    /// 但后端由调用方注入——如 [`crate::wal::MemoryWalStore`]（无文件系统/
    /// 嵌入式/测试）或第三方实现。哈希链计算与内存推进逻辑与文件后端完全一致。
    ///
    /// # 注意
    /// `Box` 会取得后端所有权；需要事后检视记录的后端（如
    /// `MemoryWalStore`）应利用其共享句柄语义（先 `clone` 再注入）。
    /// 本构造器不改变 `new()/recover*` 的任何既有行为。
    pub fn with_wal_store(store: Box<dyn FactWalStore>) -> Self {
        Self::with_inner(FactsLogInner {
            history: Vec::new(),
            current_snapshot: JsonValue::empty_object(),
            current_queue: Vec::new(),
            version: 0,
            last_stable_version: 0,
            last_hash: String::from("genesis"),
            wal: Some(store),
            fsync_on_flush: false,
            max_wal_size_bytes: DEFAULT_MAX_WAL_SIZE_BYTES,
            version_index: BTreeMap::new(),
            fact_id_index: BTreeMap::new(),
            path_index: BTreeMap::new(),
            compacted_snapshot: None,
            next_anchor_id: 1_000_000_000,
        })
    }

    #[cfg(feature = "persistence")]
    /// 从 WAL 恢复 FactsLog（P0-1）
    ///
    /// # 恢复流程
    /// 1. 读取 WAL 文件所有 (version_before, Fact) 记录
    /// 2. 重放事实到内存状态（重放期间 WAL 未挂载，不重复写入磁盘）
    /// 3. 重放完成后以 `append` 模式挂载 WAL，继续追加新事实
    ///
    /// # 错误
    /// - `WalError`：WAL 读取失败或重放完成后挂载失败
    /// - `VersionOverflow`：重放过程中版本号溢出
    pub fn recover<P: AsRef<std::path::Path>>(path: P) -> Result<Self, FactsLogError> {
        Self::recover_with_fsync(path, false)
    }

    #[cfg(feature = "persistence")]
    /// 从 WAL 恢复 FactsLog 并指定 fsync 选项（P02）
    ///
    /// # 恢复流程
    /// 1. 读取 WAL 文件所有 (version_before, Fact) 记录
    /// 2. 重放事实到内存状态（重放期间 WAL 未挂载，不重复写入磁盘）
    /// 3. 重放完成后以 `append` 模式挂载 WAL，继续追加新事实
    ///
    /// # 参数
    /// - `path`: WAL 文件路径
    /// - `fsync`: 是否在每次 flush 后执行 fsync
    ///
    /// # 错误
    /// - `WalError`：WAL 读取失败或重放完成后挂载失败
    /// - `VersionOverflow`：重放过程中版本号溢出
    pub fn recover_with_fsync<P: AsRef<std::path::Path>>(
        path: P,
        fsync: bool,
    ) -> Result<Self, FactsLogError> {
        Self::recover_with_options(path, DEFAULT_MAX_WAL_SIZE_BYTES, fsync)
    }

    #[cfg(feature = "persistence")]
    /// 从 WAL 恢复 FactsLog 并指定轮换和 fsync 选项（P03）
    ///
    /// # 恢复流程
    /// 1. 读取 WAL 文件所有 WalRecord 记录（支持多文件轮换，自动识别新旧格式）
    /// 2. 重放事实到内存状态（重放期间 WAL 未挂载，不重复写入磁盘）
    /// 3. 恢复审计链哈希（新格式直接读取，旧格式重新计算）
    /// 4. 重放完成后以 `append` 模式挂载 WAL，继续追加新事实
    ///
    /// # 哈希链恢复策略（两套 WAL 合并）
    /// - 新格式（有 chain_hash）：直接使用存储的 chain_hash 恢复 last_hash
    /// - 旧格式（无 chain_hash）：重放后重新计算 last_hash
    ///
    /// # 参数
    /// - `path`: WAL 文件路径
    /// - `max_wal_size_bytes`: 单个 WAL 文件最大大小（0 表示不轮换）
    /// - `fsync`: 是否在每次 flush 后执行 fsync
    ///
    /// # 错误
    /// - `WalError`：WAL 读取失败或重放完成后挂载失败
    /// - `VersionOverflow`：重放过程中版本号溢出
    ///
    /// 薄壳三段：read（含轮换分片合并）→ [`replay_wal_records`]（纯重放）
    /// → mount（append 模式挂载）。事实应用与哈希恢复语义在
    /// replay_wal_records / apply_fact_to_state 单点维护。
    pub fn recover_with_options<P: AsRef<std::path::Path>>(
        path: P,
        max_wal_size_bytes: u64,
        fsync: bool,
    ) -> Result<Self, FactsLogError> {
        use crate::wal::read_wal_with_hash;

        let records =
            read_wal_with_hash(&path).map_err(|e| FactsLogError::WalError(e.to_string()))?;
        let log = replay_wal_records(&records)?;
        {
            let mut inner = log.inner.write();
            // 重放完成，挂载 WAL 继续追加
            let wal = WalWriter::append_with_options(path, max_wal_size_bytes, fsync)
                .map_err(|e| FactsLogError::WalError(e.to_string()))?;
            inner.wal = Some(Box::new(wal));
            inner.fsync_on_flush = fsync;
            inner.max_wal_size_bytes = max_wal_size_bytes;
        }
        Ok(log)
    }

    /// 追加事实，返回追加后的版本号
    ///
    /// # 版本规则
    /// - `StateTransition`：更新快照与队列，version += 1
    /// - `IoResponse`：version += 1（快照由反应器通过后续 StateTransition 更新）
    /// - `PayloadUpdate`：更新快照，version += 1（断点 11 修复：与 reactor bump_version 对齐）
    /// - `Command` / `IoRequest`：版本不变（触发反应器计算）
    /// - `Stable`：记录 last_stable_version
    /// - `Error`：版本不变
    ///
    /// # 哈希链（两套 WAL 合并）
    /// 每次 append 计算审计链哈希：
    /// - `content_hash = blake3(fact_to_stable_json(fact))`
    /// - `chain_hash = blake3(prev_hash + content_hash)`
    /// - 更新 `last_hash = chain_hash`
    ///
    /// # WAL 持久化（P0-1）
    /// 若挂载了 WAL，则先 write-ahead 写入磁盘（含哈希字段）并 flush，再更新内存状态。
    /// WAL 写失败时内存尚未更新，返回 `WalError` 让调用方决定是否终止反应器，
    /// 避免内存与磁盘状态分叉。
    // WAL write-ahead 原子序（哈希链计算 → 先写磁盘 → 后更新内存）必须保持
    // 单函数；事实应用细节已提取至 apply_fact_to_state（与重放路径共用单点）
    pub fn append(&self, fact: Fact) -> Result<u64, FactsLogError> {
        let mut inner = self.inner.write();

        let version_before = inner.version;

        // === Kani 模式：简化 append ===
        // 事实应用走 apply_fact_to_state 的 Kani 简化分支（跳过 FixedMap/Vec/
        // String 复杂路径——CBMC 状态爆炸/OOM；只保留结构性不变量：history
        // push、version 递增、last_stable 更新；跳过 last_hash 更新与快照更新，
        // 哈希链正确性由既有 proof 覆盖）。
        #[cfg(kani)]
        {
            apply_fact_to_state(&mut inner, &fact)?;
            inner.history.push((version_before, fact));
            return Ok(inner.version);
        }

        // === 非 Kani 模式：完整实现 ===

        // 计算哈希链（两套 WAL 合并：哈希链提升到 tier1）
        // 注意：此段代码不依赖 persistence feature，因为 last_hash 字段始终存在，
        // 即使纯内存模式也需维护哈希链（用于 tier2 Auditor 跨层一致性）。
        let content_hash = crate::hash::fact_hash(&fact)
            .map_err(|e| FactsLogError::HashError(format!("hash error: {e}")))?;
        let prev_hash = inner.last_hash.clone();
        let chain_hash = crate::hash::chain_step(&prev_hash, &content_hash);

        #[cfg(feature = "persistence")]
        {
            // P0-1: WAL write-ahead —— 内存更新前先写磁盘 + flush（含哈希字段）
            if let Some(wal) = inner.wal.as_mut() {
                match wal.append_record_with_hash(
                    version_before,
                    &fact,
                    &content_hash,
                    &prev_hash,
                    &chain_hash,
                ) {
                    Ok(()) => {
                        // 成功 → 清零连续失败计数（方案 b：瞬时故障自愈不累计）
                        self.wal_fail_count.store(0, Ordering::SeqCst);
                    }
                    Err(e) => {
                        let fail_count = self.wal_fail_count.fetch_add(1, Ordering::SeqCst) + 1;
                        if fail_count >= WAL_FAIL_TERMINATE_THRESHOLD {
                            // W1 方案 b：连续失败达阈值 → 触发升级回调（reactor
                            // 据此终止会话并向用户发射含自助指导的 Error fact）。
                            // 阈值后每次失败都重复触发，保证最终一定能终止。
                            let guidance = wal_failure_guidance(&e.to_string());
                            tracing::error!(
                                fail_count,
                                "WAL write failed {} times consecutively; escalating session termination",
                                WAL_FAIL_TERMINATE_THRESHOLD
                            );
                            drop(inner); // 释放写锁再回调，回调可能需要访问 FactsLog
                            if let Ok(guard) = self.on_wal_failure_exhausted.lock() {
                                if let Some(cb) = guard.as_ref() {
                                    cb(&guidance);
                                }
                            }
                            return Err(FactsLogError::WalError(guidance));
                        }
                        return Err(FactsLogError::WalError(format!(
                            "WAL write failed (consecutive #{fail_count}/{}): {e}",
                            WAL_FAIL_TERMINATE_THRESHOLD
                        )));
                    }
                }
            }
        }

        // 更新哈希链末尾
        inner.last_hash = chain_hash;

        // 更新内存状态（事实应用单点：与 WAL 重放路径语义由构造保证一致）
        apply_fact_to_state(&mut inner, &fact)?;

        // 记录索引信息（fact 即将被 move 到 history）
        let fact_id = fact.id();
        let path_opt = match &fact {
            Fact::PayloadUpdate { path, .. } => Some(path.clone()),
            _ => None,
        };

        // 推入历史（fact 已 match 完，move 即可，无需 clone）
        inner.history.push((version_before, fact));

        // A-3：维护索引（O(log n) 插入）
        let idx = inner.history.len() - 1;
        inner.version_index.entry(version_before).or_insert(idx);
        inner.fact_id_index.insert(fact_id, idx);
        if let Some(path) = path_opt {
            inner.path_index.entry(path).or_default().push(idx);
        }

        Ok(inner.version)
    }

    /// 读取当前快照 (payload, queue, version)
    pub fn snapshot(&self) -> (JsonValue, Vec<JsonValue>, u64) {
        let inner = self.inner.read();
        (
            inner.current_snapshot.clone(),
            inner.current_queue.clone(),
            inner.version,
        )
    }

    /// 读取从指定版本之后的所有事实（用于审计/重放）
    ///
    /// 返回所有 `version_before >= from_version` 的事实。
    /// 如果 `from_version` 为 0，返回完整历史。
    ///
    /// # 压缩点语义（F6；P1-F4/B3 已于 2026-09-30 提升为类型级强制）
    ///
    /// 实例运行期间发生过压缩（compact）时，压缩点之前的历史已从内存投影
    /// 丢弃。此时本方法返回 [`Err(FactsLogError::CompactedRead)`]，
    /// **绝不返回空 Vec** —— 空 Vec 专用于"该版本前本就没有事实"。
    ///
    /// > **变更说明**：此前本方法在压缩点前返回空 Vec，使"被压实丢弃"与
    /// > "本来就没有"在 API 上不可区分，审计校验因此漏检（见
    /// > `SharedFactsLog::verify_causal_consistency` 的已知边界）。区分责任
    /// > 现由类型系统承担，不再依赖调用方主动查 [`Self::compacted_info`]。
    ///
    /// 需要压缩点前的完整前缀历史时，必须基于 WAL 文件离线重放。
    ///
    /// # 错误
    /// - [`FactsLogError::CompactedRead`]：`from_version` 早于压缩点版本
    pub fn read_from(&self, from_version: u64) -> Result<Vec<Fact>, FactsLogError> {
        let inner = self.inner.read();
        // A-3：压缩点之前的事实已丢弃 —— 显式报错，不再伪装成空历史
        if let Some(ref compacted) = inner.compacted_snapshot {
            if from_version < compacted.version {
                return Err(FactsLogError::CompactedRead {
                    from_version,
                    compacted_version: compacted.version,
                    compacted_count: compacted.compacted_count,
                });
            }
        }
        // A-3：用 version_index 加速定位起始下标（O(log n) 替代 O(n) 遍历）
        let start = inner
            .version_index
            .range(from_version..)
            .next()
            .map(|(_, &idx)| idx)
            .unwrap_or(inner.history.len());
        Ok(inner
            .history
            .get(start..)
            .unwrap_or(&[])
            .iter()
            .map(|(_, f)| f.clone())
            .collect())
    }

    /// 返回当前版本号
    pub fn version(&self) -> u64 {
        self.inner.read().version
    }

    /// 返回最后稳定版本号
    pub fn last_stable_version(&self) -> u64 {
        self.inner.read().last_stable_version
    }

    /// 返回审计链末尾哈希（两套 WAL 合并）
    ///
    /// 初始为 `"genesis"`，每次 `append` 后更新为新的链哈希。
    /// 用于：
    /// - tier2 Auditor 读取哈希链状态
    /// - CLI 验证哈希链完整性
    /// - 跨会话审计链衔接
    pub fn last_hash(&self) -> String {
        self.inner.read().last_hash.clone()
    }

    /// 返回历史记录数量
    pub fn history_len(&self) -> usize {
        self.inner.read().history.len()
    }

    /// 返回完整历史（用于全量审计）
    pub fn history(&self) -> Vec<Fact> {
        let inner = self.inner.read();
        inner.history.iter().map(|(_, f)| f.clone()).collect()
    }

    /// 从指定下标起增量遍历历史事实（零 clone，锁内回调）
    ///
    /// 用于 tier2 Auditor 的增量审计（）：调用方以自持游标
    /// （如已审计条目数）为起点，只遍历尾部新事实，避免 [`Self::history`]
    /// 每次全量 clone——长驻会话下全量 clone 构成 O(n²) CPU 瓶颈
    /// （实测 ~1500 命令时每命令近 GB 级内存复制）。
    ///
    /// # 约束
    /// - 回调在内部读锁持有期间执行，回调内**不得**再访问本 `FactsLog`
    ///   （重入将死锁）
    /// - `start` 不小于当前历史长度时返回 0（与切片越界空语义对齐）
    /// - 实例运行期间发生过 compact 时，历史前缀已从内存投影丢弃，
    ///   持久游标可能越过历史头部——调用方语义与 [`Self::read_from`]
    ///   的压缩注意事项相同
    ///
    /// # 参数
    /// - `start`：起始下标（含）
    /// - `f`：回调，参数为 `(version_before, &Fact)`
    ///
    /// # 返回值
    /// 实际遍历的事实条数。
    pub fn for_each_fact_from(&self, start: usize, mut f: impl FnMut(u64, &Fact)) -> usize {
        let inner = self.inner.read();
        let tail = match inner.history.get(start..) {
            Some(t) => t,
            None => return 0,
        };
        for (vb, fact) in tail.iter() {
            f(*vb, fact);
        }
        tail.len()
    }

    /// 返回带版本号的完整历史（阶段5：时间机器 rewind/diff/replay 使用）
    ///
    /// 每个元素为 `(version_before, Fact)`，其中 `version_before` 是该 Fact
    /// 追加前的版本号。`StateTransition` / `IoResponse` 追加后 version = version_before + 1。
    ///
    /// 与 `history()` 的区别：保留版本号信息，供时间机器按版本范围过滤。
    pub fn history_with_versions(&self) -> Vec<(u64, Fact)> {
        let inner = self.inner.read();
        inner.history.iter().map(|(v, f)| (*v, f.clone())).collect()
    }

    /// 返回最后 N 条带版本号的历史（P2-8：避免全量 clone）
    ///
    /// 与 `history_with_versions()` 的区别：只 clone 最后 `n` 条 Fact，
    /// 而非全量 clone。当 history 很大（万级 fact）时，复杂度从 O(全量) 降到 O(n)。
    ///
    /// 用于 Portal API 的 recent_triggers 等只需最近 N 条的场景。
    /// 若 `n >= history.len()`，返回全部历史（等价于 `history_with_versions()`）。
    pub fn history_last_with_versions(&self, n: usize) -> Vec<(u64, Fact)> {
        let inner = self.inner.read();
        let history = &inner.history;
        let start = history.len().saturating_sub(n);
        history
            .get(start..)
            .unwrap_or(&[])
            .iter()
            .map(|(v, f)| (*v, f.clone()))
            .collect()
    }

    /// 按 path 前缀查询 PayloadUpdate Fact（P0-1）
    ///
    /// 返回所有 `PayloadUpdate.path` 以指定前缀开头的事实。用于 evo-agent 的 auto_recall
    /// 机制，按命名空间前缀（如 `agent_researcher.shared.research_notes`）查询历史记忆。
    ///
    /// # 复杂度
    /// O(log n + k)，k 为匹配的事实数。通过 path_index 索引加速（A-3）。
    ///
    /// # 示例
    /// ```
    /// use evorule_reactor::FactsLog;
    /// let log = FactsLog::new();
    /// let facts = log.facts_by_path_prefix("agent_researcher.shared");
    /// // 返回所有 path 以 "agent_researcher.shared" 开头的 PayloadUpdate
    /// ```
    pub fn facts_by_path_prefix(&self, prefix: &str) -> Vec<(u64, Fact)> {
        let inner = self.inner.read();
        // A-3：用 path_index 加速（BTreeMap range 按字典序，前缀连续）
        let mut result = Vec::new();
        for (path, indices) in inner.path_index.range(prefix.to_string()..) {
            if !path.starts_with(prefix) {
                break; // 字典序连续，遇到不匹配即可终止
            }
            for &idx in indices {
                if let Some((v, f)) = inner.history.get(idx) {
                    result.push((*v, f.clone()));
                }
            }
        }
        result
    }

    /// 重置 FactsLog 到初始状态（用于对象池复用）
    ///
    /// 清空历史记录、快照和队列，重置版本号。
    /// 保留已分配的 Vec 容量（`clear()` 而非重新创建），减少内存重分配。
    /// WAL 写入器会被丢弃（重置为 `None`），仅适用于内存模式复用。
    ///
    /// # 安全性
    /// 调用方必须确保此时没有其他线程正在访问此 FactsLog
    /// （即 `is_reusable()` 返回 `true`）。
    pub fn reset(&self) {
        let mut inner = self.inner.write();
        inner.history.clear();
        inner.current_snapshot = JsonValue::empty_object();
        inner.current_queue.clear();
        inner.version = 0;
        inner.last_stable_version = 0;
        inner.last_hash = String::from("genesis");
        // A-3：清空索引和压缩快照
        inner.version_index.clear();
        inner.fact_id_index.clear();
        inner.path_index.clear();
        inner.compacted_snapshot = None;
        #[cfg(feature = "persistence")]
        {
            inner.wal = None;
        }
    }

    /// 压缩历史（A-3：手动触发）
    ///
    /// 将 `last_stable_version` 之前的事实折叠为快照，从内存 history 中丢弃。
    /// 压缩后：
    /// - `history` 只保留压缩点之后的事实
    /// - `compacted_snapshot` 记录压缩点状态（version/snapshot/queue/last_hash）
    /// - 三个索引同步重建（下标偏移修正）
    /// - WAL 文件保留全量记录（审计链完整性由 WAL 保证，不由内存保证）
    ///
    /// 压缩后 `read_from(v)` 当 v < 压缩点版本时返回 `Err(CompactedRead)`
    /// （2026-09-30 起：不再是空 Vec，避免与"该版本前本无事实"混淆）。
    ///
    /// # 返回值
    /// 压缩率（0.0~1.0），如 0.6 表示 60% 体积缩减。无可压缩事实时返回 0.0。
    pub fn compact(&self) -> f64 {
        let mut inner = self.inner.write();

        // 找到 last_stable_version 之后的第一个事实的下标（分界点）
        let split_point = inner
            .history
            .iter()
            .position(|(v, _)| *v > inner.last_stable_version)
            .unwrap_or(inner.history.len());

        if split_point == 0 {
            return 0.0; // 无可压缩事实
        }

        let compacted_count = split_point;
        let total_before = inner.history.len();

        // 保存压缩点状态
        inner.compacted_snapshot = Some(CompactedSnapshot {
            version: inner.last_stable_version,
            snapshot: inner.current_snapshot.clone(),
            queue: inner.current_queue.clone(),
            last_hash: inner.last_hash.clone(),
            compacted_count,
        });

        // 丢弃压缩点之前的事实
        inner.history.drain(0..split_point);

        // 重建索引：临时取出 history 避免借用冲突
        let history = std::mem::take(&mut inner.history);
        inner.version_index.clear();
        inner.fact_id_index.clear();
        inner.path_index.clear();
        for (new_idx, (version_before, fact)) in history.iter().enumerate() {
            inner
                .version_index
                .entry(*version_before)
                .or_insert(new_idx);
            inner.fact_id_index.insert(fact.id(), new_idx);
            if let Fact::PayloadUpdate { path, .. } = fact {
                inner
                    .path_index
                    .entry(path.clone())
                    .or_default()
                    .push(new_idx);
            }
        }
        inner.history = history; // 放回

        compacted_count as f64 / total_before as f64
    }

    /// 查询压缩快照信息（A-3：用于外部检查压缩状态）
    ///
    /// 返回 `(compacted_version, compacted_count)`：
    /// - `None`：未压缩
    /// - `Some((version, count))`：已压缩，version 为压缩点版本，count 为已丢弃事实数
    pub fn compacted_info(&self) -> Option<(u64, usize)> {
        let inner = self.inner.read();
        inner
            .compacted_snapshot
            .as_ref()
            .map(|c| (c.version, c.compacted_count))
    }

    /// 检查 FactsLog 是否可安全复用
    ///
    /// 当 Arc 强引用计数为 1 时（仅当前持有者），表示反应器已释放其引用，
    /// 可以安全重置并回收到对象池。
    pub fn is_reusable(&self) -> bool {
        Arc::strong_count(&self.inner) == 1
    }

    /// 轮换 WAL 文件（干预原语 wal.force_rotate 通路）
    ///
    /// 文件后端：切新分片（序号 +1 开新文件，旧分片保持完整——纯追加切换，
    /// 崩溃安全，不影响链内容与可回放性），返回 `Ok(true)`；
    /// 纯内存模式 / 无可轮换后端：`Ok(false)`。轮换不经过 `fact` 通道、
    /// 不产生事实——它是存储层操作，与审计链内容无关。
    #[cfg(feature = "persistence")]
    pub fn rotate_wal(&self) -> Result<bool, FactsLogError> {
        let mut inner = self.inner.write();
        match inner.wal.as_deref_mut() {
            Some(store) => store.rotate().map_err(|e| FactsLogError::WalError(e.to_string())),
            None => Ok(false),
        }
    }
}

impl Default for FactsLog {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::panic, clippy::expect_used, clippy::indexing_slicing)]
    use super::*;
    use crate::fact::{FactId, IoType};

    /// 纯重放入口直测：append 路径与 replay_wal_records 路径的终态一致性
    /// （快照/队列/版本/链尾哈希/历史长度/稳定版本六元组全等）
    #[cfg(feature = "persistence")]
    #[test]
    fn test_replay_wal_records_matches_append_path() {
        use crate::wal::MemoryWalStore;

        let store = MemoryWalStore::new();
        let log = FactsLog::with_wal_store(Box::new(store.clone()));
        let facts = vec![
            Fact::Command {
                id: FactId(1),
                instruction: JsonValue::empty_object(),
            },
            Fact::StateTransition {
                id: FactId(2),
                cause: FactId(1),
                new_payload: JsonValue::object_from_pairs(&[("x", JsonValue::Integer(1))]),
                new_queue: vec![],
            },
            Fact::PayloadUpdate {
                id: FactId(3),
                path: "y.z".into(),
                value: JsonValue::string("v"),
            },
            Fact::Stable {
                id: FactId(4),
                version: 2,
            },
        ];
        for f in facts {
            log.append(f).expect("append ok");
        }
        let (snap_a, queue_a, ver_a) = log.snapshot();
        let hash_a = log.last_hash();

        let records = store.records();
        let replayed = replay_wal_records(&records).unwrap();
        let (snap_b, queue_b, ver_b) = replayed.snapshot();
        assert_eq!(ver_a, ver_b, "版本推进必须一致");
        assert_eq!(snap_a, snap_b, "快照必须一致");
        assert_eq!(queue_a, queue_b, "队列必须一致");
        assert_eq!(hash_a, replayed.last_hash(), "链尾哈希必须一致");
        assert_eq!(log.history_len(), replayed.history_len());
        assert_eq!(replayed.last_stable_version(), 2);
    }

    /// 空记录与旧格式（无哈希字段）记录的重放边界：genesis 链尾 / 全链重算
    #[cfg(feature = "persistence")]
    #[test]
    fn test_replay_wal_records_empty_and_legacy() {
        use crate::wal::WalRecord;

        let empty = replay_wal_records(&[]).unwrap();
        assert_eq!(empty.last_hash(), "genesis");
        assert_eq!(empty.version(), 0);
        assert_eq!(empty.history_len(), 0);

        let legacy_fact = Fact::Command {
            id: FactId(1),
            instruction: JsonValue::empty_object(),
        };
        let legacy = vec![WalRecord {
            version_before: 0,
            fact: legacy_fact.clone(),
            content_hash: None,
            prev_hash: None,
            chain_hash: None,
        }];
        let log = replay_wal_records(&legacy).unwrap();
        assert_eq!(
            log.last_hash(),
            crate::hash::compute_chain_hash(&[legacy_fact]).unwrap(),
            "旧格式（无哈希字段）必须全链重算"
        );
        assert_eq!(log.history_len(), 1);
    }

    #[test]
    fn test_new_facts_log() {
        let log = FactsLog::new();
        let (payload, queue, version) = log.snapshot();
        assert_eq!(version, 0);
        assert_eq!(payload, JsonValue::empty_object());
        assert!(queue.is_empty());
        assert_eq!(log.history_len(), 0);
        assert_eq!(log.last_stable_version(), 0);
    }

    #[test]
    fn test_append_command_no_version_change() {
        let log = FactsLog::new();
        let v = log
            .append(Fact::Command {
                id: FactId(1),
                instruction: JsonValue::empty_object(),
            })
            .unwrap();
        assert_eq!(v, 0); // Command 不改变版本
        assert_eq!(log.history_len(), 1);
    }

    #[test]
    fn test_append_state_transition_increments_version() {
        let log = FactsLog::new();

        let payload = JsonValue::object_from_pairs(&[("x", JsonValue::Integer(42))]);
        let v = log
            .append(Fact::StateTransition {
                id: FactId(1),
                cause: FactId(0),
                new_payload: payload.clone(),
                new_queue: vec![],
            })
            .unwrap();
        assert_eq!(v, 1);

        let (snap, queue, version) = log.snapshot();
        assert_eq!(version, 1);
        assert_eq!(snap, payload);
        assert!(queue.is_empty());
    }

    #[test]
    fn test_append_io_response_increments_version() {
        let log = FactsLog::new();
        let v = log
            .append(Fact::IoResponse {
                id: FactId(1),
                request_id: FactId(2),
                result: JsonValue::string("ok"),
                error: None,
            })
            .unwrap();
        assert_eq!(v, 1);
    }

    #[test]
    fn test_append_stable_records_last_stable() {
        let log = FactsLog::new();

        // 先产生一次 StateTransition 使 version=1
        log.append(Fact::StateTransition {
            id: FactId(1),
            cause: FactId(0),
            new_payload: JsonValue::empty_object(),
            new_queue: vec![],
        })
        .unwrap();
        assert_eq!(log.version(), 1);

        // 再追加 Stable
        log.append(Fact::Stable {
            id: FactId(2),
            version: 0,
        })
        .unwrap();
        assert_eq!(log.last_stable_version(), 1);
    }

    #[test]
    fn test_read_from() {
        let log = FactsLog::new();

        // version 0: Command (不改变版本)
        log.append(Fact::Command {
            id: FactId(1),
            instruction: JsonValue::empty_object(),
        })
        .unwrap();
        // version 0 → 1: StateTransition
        log.append(Fact::StateTransition {
            id: FactId(2),
            cause: FactId(1),
            new_payload: JsonValue::empty_object(),
            new_queue: vec![],
        })
        .unwrap();
        // version 1: IoRequest (不改变版本)
        log.append(Fact::IoRequest {
            id: FactId(3),
            cause: FactId(2),
            io_type: IoType::call_external(),
            params: JsonValue::empty_object(),
        })
        .unwrap();
        // version 1 → 2: IoResponse
        log.append(Fact::IoResponse {
            id: FactId(4),
            request_id: FactId(3),
            result: JsonValue::string("resp"),
            error: None,
        })
        .unwrap();

        // 全量读取
        let all = log.read_from(0).unwrap();
        assert_eq!(all.len(), 4);

        // 从 version 1 开始读（包含 version_before >= 1 的事实）
        let from_v1 = log.read_from(1).unwrap();
        // version_before 分别是: 0, 0, 1, 1
        // >= 1 的有: IoRequest(version_before=1), IoResponse(version_before=1)
        assert_eq!(from_v1.len(), 2);
        assert_eq!(from_v1[0].id(), FactId(3));
        assert_eq!(from_v1[1].id(), FactId(4));

        // 从 version 2 开始读
        let from_v2 = log.read_from(2).unwrap();
        assert!(from_v2.is_empty());
    }

    #[test]
    fn test_clone_shares_state() {
        let log = FactsLog::new();
        let log2 = log.clone();

        log.append(Fact::Command {
            id: FactId(1),
            instruction: JsonValue::empty_object(),
        })
        .unwrap();

        // 克隆共享同一内部状态
        assert_eq!(log2.history_len(), 1);
    }

    #[test]
    fn test_with_initial_payload() {
        let payload = JsonValue::object_from_pairs(&[("init", JsonValue::Integer(42))]);
        let log = FactsLog::with_initial_payload(payload.clone());

        let (snap, queue, version) = log.snapshot();
        assert_eq!(version, 0); // 初始 payload 不改变版本
        assert_eq!(snap, payload);
        assert!(queue.is_empty());
        assert_eq!(log.history_len(), 0); // 未追加任何事实
    }

    #[test]
    fn test_snapshot_with_non_empty_queue() {
        let log = FactsLog::new();

        let payload = JsonValue::object_from_pairs(&[("x", JsonValue::Integer(1))]);
        let queue = vec![JsonValue::string("instr1"), JsonValue::string("instr2")];

        log.append(Fact::StateTransition {
            id: FactId(1),
            cause: FactId(0),
            new_payload: payload.clone(),
            new_queue: queue.clone(),
        })
        .unwrap();

        let (snap, q, version) = log.snapshot();
        assert_eq!(version, 1);
        assert_eq!(snap, payload);
        assert_eq!(q, queue);
    }

    #[test]
    fn test_payload_update_increments_version() {
        // 断点 11 修复：PayloadUpdate 现在递增 version（与 reactor bump_version 对齐）
        let log = FactsLog::new();
        let v0 = log.version();

        let v = log
            .append(Fact::PayloadUpdate {
                id: FactId(1),
                path: "x".to_string(),
                value: JsonValue::Integer(42),
            })
            .unwrap();
        assert_eq!(v, v0 + 1); // 版本递增
        assert_eq!(log.version(), 1);
        assert_eq!(log.history_len(), 1);
    }

    #[test]
    fn test_io_request_does_not_change_version() {
        let log = FactsLog::new();
        let v = log
            .append(Fact::IoRequest {
                id: FactId(1),
                cause: FactId(0),
                io_type: IoType::call_external(),
                params: JsonValue::empty_object(),
            })
            .unwrap();
        assert_eq!(v, 0); // 版本不变
        assert_eq!(log.version(), 0);
    }

    #[test]
    fn test_error_does_not_change_version() {
        let log = FactsLog::new();
        let v = log
            .append(Fact::Error {
                id: FactId(1),
                message: "test error".to_string(),
            })
            .unwrap();
        assert_eq!(v, 0); // 版本不变
        assert_eq!(log.version(), 0);
    }

    #[test]
    fn test_version_sequence() {
        let log = FactsLog::new();

        // Command: 版本不变 (0)
        log.append(Fact::Command {
            id: FactId(1),
            instruction: JsonValue::empty_object(),
        })
        .unwrap();
        assert_eq!(log.version(), 0);

        // StateTransition: 版本 +1 (1)
        log.append(Fact::StateTransition {
            id: FactId(2),
            cause: FactId(1),
            new_payload: JsonValue::empty_object(),
            new_queue: vec![],
        })
        .unwrap();
        assert_eq!(log.version(), 1);

        // IoRequest: 版本不变 (1)
        log.append(Fact::IoRequest {
            id: FactId(3),
            cause: FactId(2),
            io_type: IoType::call_external(),
            params: JsonValue::empty_object(),
        })
        .unwrap();
        assert_eq!(log.version(), 1);

        // IoResponse: 版本 +1 (2)
        log.append(Fact::IoResponse {
            id: FactId(4),
            request_id: FactId(3),
            result: JsonValue::string("resp"),
            error: None,
        })
        .unwrap();
        assert_eq!(log.version(), 2);

        // Stable: 记录 last_stable_version = 2，版本不变
        log.append(Fact::Stable {
            id: FactId(5),
            version: 0,
        })
        .unwrap();
        assert_eq!(log.version(), 2);
        assert_eq!(log.last_stable_version(), 2);
    }

    #[test]
    fn test_read_from_with_state_transition() {
        // 验证 read_from 正确过滤 StateTransition 的版本
        let log = FactsLog::new();

        // version 0: Command
        log.append(Fact::Command {
            id: FactId(1),
            instruction: JsonValue::empty_object(),
        })
        .unwrap();

        // version 0 → 1: StateTransition
        log.append(Fact::StateTransition {
            id: FactId(2),
            cause: FactId(1),
            new_payload: JsonValue::empty_object(),
            new_queue: vec![],
        })
        .unwrap();

        // version 1 → 2: another StateTransition
        log.append(Fact::StateTransition {
            id: FactId(3),
            cause: FactId(2),
            new_payload: JsonValue::empty_object(),
            new_queue: vec![],
        })
        .unwrap();

        // read_from(0): 全部 3 条
        assert_eq!(log.read_from(0).unwrap().len(), 3);

        // read_from(1): version_before >= 1，即第二条 StateTransition (version_before=1)
        let from_v1 = log.read_from(1).unwrap();
        assert_eq!(from_v1.len(), 1);
        assert_eq!(from_v1[0].id(), FactId(3));

        // read_from(2): 空
        assert!(log.read_from(2).unwrap().is_empty());
    }

    #[test]
    fn test_history_preserves_order() {
        let log = FactsLog::new();
        let ids = [FactId(1), FactId(2), FactId(3), FactId(4)];

        for &id in &ids {
            log.append(Fact::Command {
                id,
                instruction: JsonValue::empty_object(),
            })
            .unwrap();
        }

        let history = log.history();
        assert_eq!(history.len(), 4);
        for (i, fact) in history.iter().enumerate() {
            assert_eq!(fact.id(), ids[i]);
        }
    }

    // === P0-1 facts_by_path_prefix 测试 ===

    #[test]
    fn test_facts_by_path_prefix_empty_history() {
        let log = FactsLog::new();
        let result = log.facts_by_path_prefix("any_prefix");
        assert!(result.is_empty());
    }

    #[test]
    fn test_facts_by_path_prefix_no_matches() {
        let log = FactsLog::new();
        log.append(Fact::PayloadUpdate {
            id: FactId(1),
            path: "agent_other.shared.note".to_string(),
            value: JsonValue::string("hello"),
        })
        .unwrap();

        let result = log.facts_by_path_prefix("agent_researcher");
        assert!(result.is_empty());
    }

    #[test]
    fn test_facts_by_path_prefix_single_match() {
        let log = FactsLog::new();
        log.append(Fact::PayloadUpdate {
            id: FactId(1),
            path: "agent_researcher.shared.note".to_string(),
            value: JsonValue::string("hello"),
        })
        .unwrap();

        let result = log.facts_by_path_prefix("agent_researcher.shared");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].1.id(), FactId(1));
    }

    #[test]
    fn test_facts_by_path_prefix_multiple_matches() {
        let log = FactsLog::new();
        log.append(Fact::PayloadUpdate {
            id: FactId(1),
            path: "agent_researcher.shared.note1".to_string(),
            value: JsonValue::string("v1"),
        })
        .unwrap();
        log.append(Fact::PayloadUpdate {
            id: FactId(2),
            path: "agent_researcher.shared.note2".to_string(),
            value: JsonValue::string("v2"),
        })
        .unwrap();
        log.append(Fact::PayloadUpdate {
            id: FactId(3),
            path: "agent_other.shared.note3".to_string(),
            value: JsonValue::string("v3"),
        })
        .unwrap();

        let result = log.facts_by_path_prefix("agent_researcher.shared");
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].1.id(), FactId(1));
        assert_eq!(result[1].1.id(), FactId(2));
    }

    #[test]
    fn test_facts_by_path_prefix_prefix_boundary() {
        let log = FactsLog::new();
        log.append(Fact::PayloadUpdate {
            id: FactId(1),
            path: "agent_researcher_shared.note".to_string(),
            value: JsonValue::string("v1"),
        })
        .unwrap();
        log.append(Fact::PayloadUpdate {
            id: FactId(2),
            path: "agent_researcher.shared.note".to_string(),
            value: JsonValue::string("v2"),
        })
        .unwrap();

        let result = log.facts_by_path_prefix("agent_researcher.");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].1.id(), FactId(2));
    }

    #[test]
    fn test_facts_by_path_prefix_only_payload_update() {
        let log = FactsLog::new();
        log.append(Fact::Command {
            id: FactId(1),
            instruction: JsonValue::empty_object(),
        })
        .unwrap();
        log.append(Fact::PayloadUpdate {
            id: FactId(2),
            path: "agent_researcher.shared.note".to_string(),
            value: JsonValue::string("v1"),
        })
        .unwrap();
        log.append(Fact::StateTransition {
            id: FactId(3),
            cause: FactId(1),
            new_payload: JsonValue::empty_object(),
            new_queue: vec![],
        })
        .unwrap();

        let result = log.facts_by_path_prefix("agent_researcher");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].1.id(), FactId(2));
    }

    // === P0-1 WAL 持久化测试 ===

    #[cfg_attr(not(feature = "persistence"), allow(dead_code))]
    fn temp_wal_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "evorule_factslog_test_{name}_{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn test_facts_log_error_wal_error_display() {
        let e = FactsLogError::WalError("disk full".into());
        assert!(format!("{e}").contains("disk full"));
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn test_with_wal_creates_empty_log() {
        let path = temp_wal_path("with_wal_empty");
        let log = FactsLog::with_wal(&path).unwrap();
        // 全新启动：版本 0、空历史
        assert_eq!(log.version(), 0);
        assert_eq!(log.history_len(), 0);

        // 文件已创建（可能为空，因为尚未 append）
        assert!(std::fs::metadata(&path).is_ok());

        let _ = std::fs::remove_file(&path);
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn test_rotate_wal_memory_mode_returns_false() {
        let log = FactsLog::new();
        assert!(
            !log.rotate_wal().unwrap(),
            "纯内存模式（无 WAL 后端）轮换必须返回 false"
        );
    }

    #[test]
    fn test_rotate_wal_file_backend_rotates_shard() {
        let path = temp_wal_path("rotate_shard");

        let log = FactsLog::with_wal(&path).unwrap();
        log.append(Fact::Command {
            id: FactId(1),
            instruction: JsonValue::object_from_pairs(&[("type", JsonValue::string("increment"))]),
        })
        .unwrap();
        assert!(log.rotate_wal().unwrap(), "文件后端轮换必须返回 true");
        // 轮换后继续追加：新分片续写，旧分片保持完整；全量重放内容不丢
        log.append(Fact::Command {
            id: FactId(2),
            instruction: JsonValue::object_from_pairs(&[("type", JsonValue::string("increment"))]),
        })
        .unwrap();
        drop(log);

        let recovered = FactsLog::recover(&path).unwrap();
        assert_eq!(recovered.history().len(), 2, "轮换后旧分片内容必须可重放");
    }

    #[test]
    fn test_wal_persists_facts_across_drop() {
        let path = temp_wal_path("persist_drop");

        // 1. 创建带 WAL 的 FactsLog，写入若干事实
        let log = FactsLog::with_wal(&path).unwrap();
        log.append(Fact::Command {
            id: FactId(1),
            instruction: JsonValue::object_from_pairs(&[("type", JsonValue::string("increment"))]),
        })
        .unwrap();
        log.append(Fact::StateTransition {
            id: FactId(2),
            cause: FactId(1),
            new_payload: JsonValue::object_from_pairs(&[("x", JsonValue::Integer(42))]),
            new_queue: vec![],
        })
        .unwrap();
        log.append(Fact::Stable {
            id: FactId(3),
            version: 1,
        })
        .unwrap();

        let (snap_before, _, ver_before) = log.snapshot();
        let hist_before = log.history();
        assert_eq!(ver_before, 1);
        assert_eq!(hist_before.len(), 3);

        // 2. 模拟进程崩溃：丢弃 log
        drop(log);

        // 3. 从 WAL 恢复
        let recovered = FactsLog::recover(&path).unwrap();

        // 4. 验证状态一致
        let (snap_after, _, ver_after) = recovered.snapshot();
        let hist_after = recovered.history();
        assert_eq!(ver_after, ver_before, "version should match after recovery");
        assert_eq!(
            snap_after, snap_before,
            "snapshot should match after recovery"
        );
        assert_eq!(
            hist_after.len(),
            hist_before.len(),
            "history length should match"
        );
        for (i, (a, b)) in hist_before.iter().zip(hist_after.iter()).enumerate() {
            assert_eq!(a, b, "fact {i} should match after recovery");
        }
        assert_eq!(recovered.last_stable_version(), 1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    #[cfg(feature = "persistence")]
    fn test_recovered_log_can_continue_appending() {
        let path = temp_wal_path("continue_append");

        // 第一次生命周期：写 2 条事实
        let log = FactsLog::with_wal(&path).unwrap();
        log.append(Fact::Command {
            id: FactId(1),
            instruction: JsonValue::empty_object(),
        })
        .unwrap();
        log.append(Fact::Stable {
            id: FactId(2),
            version: 0,
        })
        .unwrap();
        drop(log);

        // 第二次生命周期：恢复 + 继续写
        let recovered = FactsLog::recover(&path).unwrap();
        assert_eq!(recovered.history_len(), 2);
        recovered
            .append(Fact::Error {
                id: FactId(3),
                message: "post-recovery".into(),
            })
            .unwrap();
        assert_eq!(recovered.history_len(), 3);
        drop(recovered);

        // 第三次生命周期：再次恢复，验证追加已持久化
        let recovered2 = FactsLog::recover(&path).unwrap();
        assert_eq!(recovered2.history_len(), 3);
        let history = recovered2.history();
        assert_eq!(history[2].id(), FactId(3));

        let _ = std::fs::remove_file(&path);
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn test_recover_nonexistent_wal_returns_error() {
        let path = temp_wal_path("nonexistent");
        let result = FactsLog::recover(&path);
        match result {
            Err(FactsLogError::WalError(msg)) => {
                // OK，预期错误
                assert!(!msg.is_empty());
            }
            Err(other) => panic!("expected WalError, got other error: {other:?}"),
            Ok(_) => panic!("expected WalError, got Ok"),
        }
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn test_wal_disabled_when_using_new() {
        // 通过 new() 创建的 FactsLog 不挂载 WAL，append 不应触发磁盘 I/O
        let log = FactsLog::new();
        log.append(Fact::Command {
            id: FactId(1),
            instruction: JsonValue::empty_object(),
        })
        .unwrap();
        assert_eq!(log.history_len(), 1);
        // 无需清理文件 —— 没有创建任何文件
    }

    /// W1 方案 b 回归（2026-08-27）：
    /// WAL 持续写失败 → 连续失败计数递增 → 达阈值后返回含自助指导的错误
    /// 并触发升级回调。
    ///
    /// 失败构造方式（跨平台稳定）：设置极小的 max_wal_size_bytes 触发文件
    /// 轮换，并把轮换目标路径 `wal.1.jsonl` 预先替换为同名目录——
    /// OpenOptions::open 对目录必失败，之后每次 append 都在 rotate 处报错。
    #[cfg(feature = "persistence")]
    #[test]
    fn test_wal_consecutive_failure_escalates_with_guidance() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc as StdArc;

        let base = std::env::temp_dir().join(format!("evorule_w1_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let wal_path = base.join("wal.jsonl");

        // max=1 字节：首条 append 后 current_size 超阈值，
        // 第二条 append 将触发 rotate → 打开 wal.1.jsonl 失败
        let log = FactsLog::with_wal_options(&wal_path, 1, false).unwrap();
        log.append(Fact::Command {
            id: FactId(1),
            instruction: JsonValue::empty_object(),
        })
        .unwrap(); // 基线写入成功（写后触发轮换条件）

        // 预置多个轮换目标为目录 → 持续 WalError。
        // 注意：rotate 内部先 file_sequence += 1 再 open_file，单次 open
        // 失败后下一次会跳到新路径"意外自愈"，因此预置足够多的目录
        // 覆盖阈值次数的连续失败。
        for seq in 1..=(WAL_FAIL_TERMINATE_THRESHOLD + 3) {
            std::fs::create_dir(base.join(format!("wal.{seq}.jsonl"))).unwrap();
        }

        let escalated = StdArc::new(AtomicUsize::new(0));
        let escalated_cb = escalated.clone();
        log.set_on_wal_failure_exhausted(move |guidance| {
            escalated_cb.fetch_add(1, Ordering::SeqCst);
            assert!(
                guidance.contains("磁盘") && guidance.contains("权限"),
                "guidance must contain self-service recovery hints"
            );
        });

        // 连续失败 WAL_FAIL_TERMINATE_THRESHOLD 次：
        for i in 1..=WAL_FAIL_TERMINATE_THRESHOLD {
            let result = log.append(Fact::Command {
                id: FactId(i + 10),
                instruction: JsonValue::empty_object(),
            });
            assert!(
                result.is_err(),
                "attempt {i} should fail on dir-as-rotate-target"
            );
            let err_msg = format!("{}", result.unwrap_err());
            if i < WAL_FAIL_TERMINATE_THRESHOLD {
                assert!(
                    err_msg.contains("#"),
                    "pre-threshold error carries consecutive count, got: {err_msg}"
                );
            } else {
                assert!(
                    err_msg.contains("自助诊断") || err_msg.contains("排查"),
                    "escalated error should carry user guidance, got: {err_msg}"
                );
            }
        }
        assert_eq!(
            escalated.load(Ordering::SeqCst),
            1,
            "callback should fire exactly once at threshold"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn test_wal_recovery_preserves_all_seven_fact_variants() {
        let path = temp_wal_path("all_variants");

        let log = FactsLog::with_wal(&path).unwrap();
        // 7 种 Fact 变体各写一条
        log.append(Fact::Command {
            id: FactId(1),
            instruction: JsonValue::object_from_pairs(&[("a", JsonValue::Integer(1))]),
        })
        .unwrap();
        log.append(Fact::PayloadUpdate {
            id: FactId(2),
            path: "x.y".into(),
            value: JsonValue::String("v".into()),
        })
        .unwrap();
        log.append(Fact::StateTransition {
            id: FactId(3),
            cause: FactId(1),
            new_payload: JsonValue::object_from_pairs(&[("x", JsonValue::Integer(5))]),
            new_queue: vec![JsonValue::String("q1".into())],
        })
        .unwrap();
        log.append(Fact::IoRequest {
            id: FactId(4),
            cause: FactId(3),
            io_type: IoType::call_external(),
            params: JsonValue::object_from_pairs(&[("prompt", JsonValue::String("hi".into()))]),
        })
        .unwrap();
        log.append(Fact::IoResponse {
            id: FactId(5),
            request_id: FactId(4),
            result: JsonValue::String("resp".into()),
            error: None,
        })
        .unwrap();
        log.append(Fact::Stable {
            id: FactId(6),
            version: 2,
        })
        .unwrap();
        log.append(Fact::Error {
            id: FactId(7),
            message: "all variants tested".into(),
        })
        .unwrap();

        let original_history = log.history();
        let (original_snap, original_queue, original_ver) = log.snapshot();
        let original_last_stable = log.last_stable_version();
        drop(log);

        let recovered = FactsLog::recover(&path).unwrap();
        let recovered_history = recovered.history();
        let (recovered_snap, recovered_queue, recovered_ver) = recovered.snapshot();
        let recovered_last_stable = recovered.last_stable_version();

        assert_eq!(recovered_history.len(), original_history.len());
        for (i, (a, b)) in original_history
            .iter()
            .zip(recovered_history.iter())
            .enumerate()
        {
            assert_eq!(a, b, "fact {i} mismatch after recovery");
        }
        assert_eq!(recovered_snap, original_snap);
        assert_eq!(recovered_queue, original_queue);
        assert_eq!(recovered_ver, original_ver);
        assert_eq!(recovered_last_stable, original_last_stable);

        let _ = std::fs::remove_file(&path);
    }

    // === P02 WAL fsync 测试 ===

    #[cfg(feature = "persistence")]
    #[test]
    fn test_with_wal_and_fsync_creates_empty_log() {
        let path = temp_wal_path("with_wal_fsync_empty");
        let log = FactsLog::with_wal_and_fsync(&path, true).unwrap();
        assert_eq!(log.version(), 0);
        assert_eq!(log.history_len(), 0);
        assert!(std::fs::metadata(&path).is_ok());
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn test_wal_fsync_persists_facts_across_drop() {
        let path = temp_wal_path("fsync_persist_drop");

        let log = FactsLog::with_wal_and_fsync(&path, true).unwrap();
        log.append(Fact::Command {
            id: FactId(1),
            instruction: JsonValue::object_from_pairs(&[("type", JsonValue::string("increment"))]),
        })
        .unwrap();
        log.append(Fact::StateTransition {
            id: FactId(2),
            cause: FactId(1),
            new_payload: JsonValue::object_from_pairs(&[("x", JsonValue::Integer(42))]),
            new_queue: vec![],
        })
        .unwrap();

        let (snap_before, _, ver_before) = log.snapshot();
        let hist_before = log.history();
        assert_eq!(ver_before, 1);
        assert_eq!(hist_before.len(), 2);

        drop(log);

        let recovered = FactsLog::recover_with_fsync(&path, true).unwrap();
        let (snap_after, _, ver_after) = recovered.snapshot();
        let hist_after = recovered.history();

        assert_eq!(ver_after, ver_before);
        assert_eq!(snap_after, snap_before);
        assert_eq!(hist_after.len(), hist_before.len());

        let _ = std::fs::remove_file(&path);
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn test_recover_with_fsync_can_continue_appending() {
        let path = temp_wal_path("fsync_continue_append");

        let log = FactsLog::with_wal_and_fsync(&path, true).unwrap();
        log.append(Fact::Command {
            id: FactId(1),
            instruction: JsonValue::empty_object(),
        })
        .unwrap();
        drop(log);

        let recovered = FactsLog::recover_with_fsync(&path, true).unwrap();
        assert_eq!(recovered.history_len(), 1);
        recovered
            .append(Fact::Error {
                id: FactId(2),
                message: "post-recovery with fsync".into(),
            })
            .unwrap();
        assert_eq!(recovered.history_len(), 2);
        drop(recovered);

        let recovered2 = FactsLog::recover(&path).unwrap();
        assert_eq!(recovered2.history_len(), 2);
        let history = recovered2.history();
        assert_eq!(history[1].id(), FactId(2));

        let _ = std::fs::remove_file(&path);
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn test_fsync_false_is_default() {
        let path = temp_wal_path("fsync_default");

        let log = FactsLog::with_wal(&path).unwrap();
        log.append(Fact::Command {
            id: FactId(1),
            instruction: JsonValue::empty_object(),
        })
        .unwrap();
        drop(log);

        let recovered = FactsLog::recover(&path).unwrap();
        assert_eq!(recovered.history_len(), 1);

        let _ = std::fs::remove_file(&path);
    }

    // === P03 WAL 文件轮换测试 ===

    #[cfg(feature = "persistence")]
    #[test]
    fn test_wal_rotation_creates_multiple_files() {
        let path = temp_wal_path("rotation_create");

        let log = FactsLog::with_wal_options(&path, 100, false).unwrap();

        for i in 0..10 {
            log.append(Fact::Command {
                id: FactId(i as u64 + 1),
                instruction: JsonValue::object_from_pairs(&[(
                    "data",
                    JsonValue::string("x".repeat(50)),
                )]),
            })
            .unwrap();
        }
        assert_eq!(log.history_len(), 10);

        drop(log);

        let file_stem = path.file_stem().unwrap().to_string_lossy().to_string();
        let files = std::fs::read_dir(path.parent().unwrap()).unwrap();
        let wal_files: Vec<_> = files
            .filter_map(|e| {
                let e = e.unwrap();
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with(&file_stem) {
                    Some(name)
                } else {
                    None
                }
            })
            .collect();

        assert!(
            wal_files.len() > 1,
            "Expected multiple WAL files, got {:?}",
            wal_files
        );

        for f in &wal_files {
            let fp = path.parent().unwrap().join(f);
            let _ = std::fs::remove_file(&fp);
        }
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn test_wal_rotation_recover_reads_all_files() {
        let path = temp_wal_path("rotation_recover");

        let log = FactsLog::with_wal_options(&path, 100, false).unwrap();

        for i in 0..20 {
            log.append(Fact::Command {
                id: FactId(i as u64 + 1),
                instruction: JsonValue::object_from_pairs(&[(
                    "data",
                    JsonValue::string("x".repeat(30)),
                )]),
            })
            .unwrap();
        }
        let history_before = log.history();
        assert_eq!(history_before.len(), 20);

        drop(log);

        let recovered = FactsLog::recover(&path).unwrap();
        let history_after = recovered.history();
        assert_eq!(history_after.len(), 20);

        for i in 0..20 {
            assert_eq!(history_after[i].id(), history_before[i].id());
        }

        let file_stem = path.file_stem().unwrap().to_string_lossy().to_string();
        let files = std::fs::read_dir(path.parent().unwrap()).unwrap();
        for e in files {
            let e = e.unwrap();
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with(&file_stem) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn test_wal_rotation_default_size() {
        let path = temp_wal_path("rotation_default");

        let log = FactsLog::with_wal(&path).unwrap();
        log.append(Fact::Command {
            id: FactId(1),
            instruction: JsonValue::empty_object(),
        })
        .unwrap();
        assert_eq!(log.history_len(), 1);

        drop(log);

        let recovered = FactsLog::recover(&path).unwrap();
        assert_eq!(recovered.history_len(), 1);

        let _ = std::fs::remove_file(&path);
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn test_wal_rotation_zero_disables_rotation() {
        let path = temp_wal_path("rotation_zero");

        let log = FactsLog::with_wal_options(&path, 0, false).unwrap();

        for i in 0..100 {
            log.append(Fact::Command {
                id: FactId(i as u64 + 1),
                instruction: JsonValue::object_from_pairs(&[(
                    "data",
                    JsonValue::string("x".repeat(100)),
                )]),
            })
            .unwrap();
        }
        assert_eq!(log.history_len(), 100);

        drop(log);

        let file_stem = path.file_stem().unwrap().to_string_lossy().to_string();
        let files = std::fs::read_dir(path.parent().unwrap()).unwrap();
        let wal_files: Vec<_> = files
            .filter_map(|e| {
                let e = e.unwrap();
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with(&file_stem) {
                    Some(name)
                } else {
                    None
                }
            })
            .collect();

        assert_eq!(
            wal_files.len(),
            1,
            "Expected single WAL file when rotation disabled, got {:?}",
            wal_files
        );

        let recovered = FactsLog::recover(&path).unwrap();
        assert_eq!(recovered.history_len(), 100);

        let _ = std::fs::remove_file(&path);
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn test_wal_rotation_after_recovery() {
        let path = temp_wal_path("rotation_after_recovery");

        let log = FactsLog::with_wal_options(&path, 100, false).unwrap();
        log.append(Fact::Command {
            id: FactId(1),
            instruction: JsonValue::string("initial"),
        })
        .unwrap();
        drop(log);

        let recovered = FactsLog::recover_with_options(&path, 100, false).unwrap();
        assert_eq!(recovered.history_len(), 1);

        for i in 0..20 {
            recovered
                .append(Fact::Command {
                    id: FactId(i as u64 + 2),
                    instruction: JsonValue::object_from_pairs(&[(
                        "data",
                        JsonValue::string("x".repeat(50)),
                    )]),
                })
                .unwrap();
        }
        assert_eq!(recovered.history_len(), 21);

        drop(recovered);

        let recovered2 = FactsLog::recover(&path).unwrap();
        assert_eq!(recovered2.history_len(), 21);

        let file_stem = path.file_stem().unwrap().to_string_lossy().to_string();
        let files = std::fs::read_dir(path.parent().unwrap()).unwrap();
        for e in files {
            let e = e.unwrap();
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with(&file_stem) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    // ===== 存储层 trait 抽象测试 =====

    #[cfg(feature = "persistence")]
    #[test]
    fn test_memory_wal_store_roundtrip_preserves_hash_fields() {
        use crate::wal::{FactWalStore, MemoryWalStore};

        let mut store = MemoryWalStore::new();
        assert!(store.is_empty());

        store
            .append_record_with_hash(
                0,
                &Fact::Command {
                    id: FactId(1),
                    instruction: JsonValue::empty_object(),
                },
                "c1",
                "genesis",
                "h1",
            )
            .unwrap();
        store
            .append_record_with_hash(
                1,
                &Fact::Error {
                    id: FactId(2),
                    message: "e".into(),
                },
                "c2",
                "h1",
                "h2",
            )
            .unwrap();
        assert_eq!(store.len(), 2);

        let records = store.into_records();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].version_before, 0);
        assert_eq!(records[0].content_hash.as_deref(), Some("c1"));
        assert_eq!(records[0].prev_hash.as_deref(), Some("genesis"));
        assert_eq!(records[0].chain_hash.as_deref(), Some("h1"));
        assert_eq!(records[1].version_before, 1);
        assert_eq!(records[1].chain_hash.as_deref(), Some("h2"));
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn test_with_wal_store_memory_backend_same_hash_chain_as_pure_memory() {
        use crate::wal::MemoryWalStore;

        // 同一事实序列:纯内存模式 vs 内存后端模式,哈希链必须一致
        let store = MemoryWalStore::new();
        let probe = store.clone();
        let log = FactsLog::with_wal_store(Box::new(store));

        let log_pure = FactsLog::new();
        let mut expected_vb: Vec<u64> = Vec::new();
        let mut cur_version: u64 = 0;
        for i in 0..5u64 {
            let fact = if i % 2 == 0 {
                Fact::PayloadUpdate {
                    id: FactId(i + 1),
                    path: format!("k{i}"),
                    value: JsonValue::Integer(i as i64),
                }
            } else {
                Fact::Command {
                    id: FactId(i + 1),
                    instruction: JsonValue::empty_object(),
                }
            };
            // 版本规则:PayloadUpdate +1,Command 不变(facts_log append 契约)
            expected_vb.push(cur_version);
            if matches!(fact, Fact::PayloadUpdate { .. }) {
                cur_version += 1;
            }
            log.append(fact.clone()).unwrap();
            log_pure.append(fact).unwrap();
        }

        // 版本与历史一致
        assert_eq!(log.version(), log_pure.version());
        assert_eq!(log.history_len(), log_pure.history_len());
        // 哈希链一致(通过 FactsLog 公开访问器;探测句柄记录数一致)
        assert_eq!(log.last_hash(), log_pure.last_hash());
        assert_eq!(probe.len(), 5);
        // write-ahead 语义:后端记录与内存 history 一一同相(version_before 对齐、均带哈希)
        let records = probe.records();
        let hist = log.history();
        assert_eq!(records.len(), hist.len());
        for (r, vb) in records.iter().zip(expected_vb.iter()) {
            assert_eq!(r.version_before, *vb);
            assert!(r.has_hash());
        }
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn test_file_backend_via_trait_unchanged_roundtrip() {
        // 默认文件后端经 trait 对象分发,行为与直挂 WalWriter 一致(回归锁)
        let path = temp_wal_path("reg026_file_trait");
        let log = FactsLog::with_wal(&path).unwrap();
        log.append(Fact::PayloadUpdate {
            id: FactId(1),
            path: "a".into(),
            value: JsonValue::Integer(7),
        })
        .unwrap();
        let expected_last = log.last_hash();
        drop(log);

        let recovered = FactsLog::recover(&path).unwrap();
        assert_eq!(recovered.history_len(), 1);
        assert_eq!(recovered.last_hash(), expected_last);
        let _ = std::fs::remove_file(&path);
    }

    // ===== A-3 索引与压缩测试 =====

    #[test]
    fn test_a3_version_index_accelerates_read_from() {
        let log = FactsLog::new();
        // 注入 50 轮（每轮 Command + StateTransition = 2 条），version 达到 50
        for i in 0..50u64 {
            log.append(Fact::Command {
                id: FactId(i * 2 + 1),
                instruction: JsonValue::empty_object(),
            })
            .unwrap();
            log.append(Fact::StateTransition {
                id: FactId(i * 2 + 2),
                cause: FactId(i * 2 + 1),
                new_payload: JsonValue::object_from_pairs(&[(
                    "count",
                    JsonValue::Integer(i as i64),
                )]),
                new_queue: vec![],
            })
            .unwrap();
        }
        assert_eq!(log.history_len(), 100);
        assert_eq!(log.version(), 50);

        // read_from(25) 应返回 version_before >= 25 的所有事实
        let facts = log.read_from(25).unwrap();
        // 版本 25~49 各有 2 条（Command + StateTransition），共 25 版 * 2 条 = 50 条
        assert_eq!(facts.len(), 50);

        // read_from(0) 返回全部 100 条
        let all = log.read_from(0).unwrap();
        assert_eq!(all.len(), 100);
    }

    #[test]
    fn test_a3_path_index_accelerates_facts_by_path_prefix() {
        let log = FactsLog::new();
        // 注入不同 path 的 PayloadUpdate
        for i in 0..20u64 {
            log.append(Fact::PayloadUpdate {
                id: FactId(i + 1),
                path: format!("agent.shared.notes_{i}"),
                value: JsonValue::Integer(i as i64),
            })
            .unwrap();
        }
        for i in 0..10u64 {
            log.append(Fact::PayloadUpdate {
                id: FactId(i + 21),
                path: format!("agent.memory.fact_{i}"),
                value: JsonValue::Integer(i as i64),
            })
            .unwrap();
        }

        // 查询前缀 "agent.shared" 应返回 20 条
        assert_eq!(log.facts_by_path_prefix("agent.shared").len(), 20);
        // 查询前缀 "agent.memory" 应返回 10 条
        assert_eq!(log.facts_by_path_prefix("agent.memory").len(), 10);
        // 查询前缀 "agent" 应返回全部 30 条
        assert_eq!(log.facts_by_path_prefix("agent").len(), 30);
        // 查询不存在的前缀应返回 0 条
        assert_eq!(log.facts_by_path_prefix("nonexistent").len(), 0);
    }

    #[test]
    fn test_a3_compact_reduces_history_size() {
        let log = FactsLog::new();

        // 注入 1666 轮（每轮 Command + StateTransition + Stable = 3 条 = 4998 条）
        // version 达到 1666, last_stable_version = 1666
        for i in 0..1666u64 {
            log.append(Fact::Command {
                id: FactId(i * 3 + 1),
                instruction: JsonValue::empty_object(),
            })
            .unwrap();
            log.append(Fact::StateTransition {
                id: FactId(i * 3 + 2),
                cause: FactId(i * 3 + 1),
                new_payload: JsonValue::object_from_pairs(&[(
                    "count",
                    JsonValue::Integer(i as i64),
                )]),
                new_queue: vec![],
            })
            .unwrap();
            log.append(Fact::Stable {
                id: FactId(i * 3 + 3),
                version: 0,
            })
            .unwrap();
        }
        // version=1666, last_stable_version=1666

        // 再加 2 条：1 StateTransition 提升 version 到 1667 + 1 Command
        // 这 2 条的 version_before 分别为 1666 和 1667
        log.append(Fact::StateTransition {
            id: FactId(4999),
            cause: FactId(4998),
            new_payload: JsonValue::object_from_pairs(&[("final", JsonValue::bool(true))]),
            new_queue: vec![],
        })
        .unwrap(); // version_before=1666, version→1667
        log.append(Fact::Command {
            id: FactId(5000),
            instruction: JsonValue::empty_object(),
        })
        .unwrap(); // version_before=1667

        assert_eq!(log.history_len(), 5000);

        // 压缩前状态快照
        let (snapshot_before, queue_before, version_before) = log.snapshot();
        let last_hash_before = log.last_hash();

        // 执行压缩
        let ratio = log.compact();

        // split_point=4999（第 5000 条 version_before=1667 > 1666）
        // compacted_count=4999, ratio=4999/5000≈0.9998 >> 0.4
        assert!(ratio >= 0.4, "压缩率 {ratio:.4} 应 >= 0.4 (40%)");

        // 压缩后 history 只保留 1 条
        assert_eq!(log.history_len(), 1);

        // 压缩后快照/队列/版本/哈希不变
        let (snapshot_after, queue_after, version_after) = log.snapshot();
        assert_eq!(snapshot_after, snapshot_before);
        assert_eq!(queue_after, queue_before);
        assert_eq!(version_after, version_before);
        assert_eq!(log.last_hash(), last_hash_before);

        // compacted_info 应有值
        let (compacted_version, compacted_count) = log.compacted_info().expect("应有压缩快照");
        assert_eq!(compacted_count, 4999);
        assert_eq!(compacted_version, 1666);
    }

    #[test]
    fn test_a3_compact_read_from_before_compaction_point_returns_explicit_error() {
        let log = FactsLog::new();

        // 注入 10 轮（每轮 Command + StateTransition + Stable = 3 条）
        for i in 0..10u64 {
            log.append(Fact::Command {
                id: FactId(i * 3 + 1),
                instruction: JsonValue::empty_object(),
            })
            .unwrap();
            log.append(Fact::StateTransition {
                id: FactId(i * 3 + 2),
                cause: FactId(i * 3 + 1),
                new_payload: JsonValue::object_from_pairs(&[("v", JsonValue::Integer(i as i64))]),
                new_queue: vec![],
            })
            .unwrap();
            log.append(Fact::Stable {
                id: FactId(i * 3 + 3),
                version: 0,
            })
            .unwrap();
        }
        // version=10, last_stable_version=10

        // 加 2 条使压缩点之后有数据
        log.append(Fact::StateTransition {
            id: FactId(31),
            cause: FactId(30),
            new_payload: JsonValue::object_from_pairs(&[("v", JsonValue::Integer(10))]),
            new_queue: vec![],
        })
        .unwrap(); // version_before=10, version→11
        log.append(Fact::Command {
            id: FactId(32),
            instruction: JsonValue::empty_object(),
        })
        .unwrap(); // version_before=11

        // 压缩前 read_from(5) 返回非空
        assert!(!log.read_from(5).unwrap().is_empty());

        // 执行压缩
        log.compact();

        // 压缩后 read_from(5) 必须返回显式错误，而**不是**空 Vec：
        // 空 Vec 曾使"被压实丢弃"与"本无历史"在 API 上不可分辨（P1-F4/B3）。
        match log.read_from(5) {
            Err(FactsLogError::CompactedRead {
                from_version,
                compacted_version,
                compacted_count,
            }) => {
                assert_eq!(from_version, 5);
                assert_eq!(compacted_version, 10, "压缩点版本应为 last_stable_version");
                assert!(compacted_count > 0, "应报告被压实丢弃的事实条数");
            }
            other => panic!("压缩点前读取应返回 CompactedRead，实际得到 {other:?}"),
        }

        // 压缩点之后读取仍然正常
        assert_eq!(
            log.read_from(11).unwrap().len(),
            1,
            "压缩后 read_from(11) 应返回 1 条"
        );

        // 关键区分：未压缩实例读"尚无事实的版本"仍是空 Vec，而非错误
        let fresh = FactsLog::new();
        assert!(
            fresh.read_from(0).unwrap().is_empty(),
            "真空历史应返回空 Vec，而非 CompactedRead —— 二者不得再混淆"
        );
    }
}

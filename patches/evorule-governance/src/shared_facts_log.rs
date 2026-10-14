// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 跨会话共享事实存储
//!
//! # 设计依据
//! 基于文档 19（auto_recall 记忆机制）和 22（补齐清单），SharedFactsLog 是跨会话共享事实的全局存储。
//! 每个 session 写入 `shared.*` 命名空间的 PayloadUpdate 时，同步写入 SharedFactsLog，
//! 其他 session 可以从中读取。
//!
//! # 并发模型
//! 使用 `std::sync::RwLock`，因为写操作极快（push + 字段更新），不跨 await 持锁。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use evorule_reactor::{Fact, FactId, FactsLog, FactsLogError};
use evorule_tcb::JsonValue;
use serde::{Deserialize, Serialize};

/// 共享事实元数据
///
/// 每条共享事实除了原始 Fact 的内容外，还包含来源会话 ID 等元数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedFact {
    /// 事实 ID
    pub fact_id: FactId,
    /// 事实路径
    pub path: String,
    /// 事实值
    pub value: JsonValue,
    /// 来源会话 ID
    pub source_session_id: u64,
    /// 版本号
    pub version: u64,
    /// 来源会话侧的源头 fact_id（N6 链路统一，R10）
    ///
    /// 同一逻辑记忆存在两个账本节点：会话侧（session WAL 哈希链上的
    /// `Fact::PayloadUpdate`）与共享侧（本账本）。本字段记录共享侧条目
    /// 对应的会话侧源头事实 ID，使跨链溯源可出示双侧证据。
    ///
    /// 语义边界：`fact_id` 是**链内节点地址**（会话链/共享链各自独立编址），
    /// 逻辑身份由 `path` 承载——两个 fact_id 不是"同一 ID 的分裂"，而是
    /// 两个审计事件（"会话写入 X" 与 "X 被广播进共享账本"）的节点地址。
    ///
    /// `None` 表示：经旧签名 `append` 写入（无 origin 通道），或该条目
    /// 先于 R10 存在（metadata 中无映射）。溯源索引随 metadata 持久化，
    /// 丢失语义与 `fact_sources` 一致（辅助索引，WAL 为权威）。
    pub origin_fact_id: Option<u64>,
}

/// 共享事实元数据持久化容器
///
/// 用于将 `SharedFactsLog` 的内存元数据（`fact_sources`、`next_fact_id`、`rolled_up`、
/// `used_at_startup`）持久化到独立文件，弥补 `FactsLog` 的 WAL 只记录 Fact 本身、
/// 不记录元数据的设计限制。
///
/// 序列化为 JSON 文件，通过 `write_metadata_atomic` 原子写入（write tmp + rename）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SharedFactsMetadata {
    /// 下一个 fact_id（自增计数器）
    pub next_fact_id: u64,
    /// FactId(u64) → source_session_id 映射
    pub fact_sources: BTreeMap<u64, u64>,
    /// FactId(u64) → 会话侧源头 fact_id 映射（N6 链路统一，R10）
    ///
    /// 仅存 `Some` 项；`#[serde(default)]` 保证旧 metadata 文件（无此字段）
    /// 反序列化不报错——溯源索引是辅助数据，缺失只损失跨链出处，不损审计链。
    #[serde(default)]
    pub fact_origins: BTreeMap<u64, u64>,
    /// session_id → 启动时使用的 fact_id 列表
    pub used_at_startup: BTreeMap<u64, Vec<u64>>,
    /// 已标记为 rollup 的 fact_id 集合（`facts_by_path_prefix` 查询时过滤）
    pub rolled_up: BTreeSet<u64>,
}

/// `mark_as_rollup` 的显式结果（R11 静默失败修复）
///
/// 修复前返回 `()`——调用方无法感知"标记了不存在的 id"这一静默失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollupOutcome {
    /// 实际存在的、成功标记（或已标记，幂等）的 id 数量
    pub marked: usize,
    /// 共享账本中不存在的 id 清单（典型原因：误用 payload 侧 fact_id，
    /// 与共享表 ID 空间不通用）。调用方必须显式处理，不得忽略。
    pub unknown: Vec<FactId>,
}

/// 跨会话共享事实存储
///
/// 作为跨会话共享事实的全局存储，支持按路径前缀查询。
#[derive(Debug, Clone)]
pub struct SharedFactsLog {
    inner: Arc<RwLock<SharedFactsLogInner>>,
}

struct SharedFactsLogInner {
    facts_log: FactsLog,
    next_fact_id: u64,
    fact_sources: BTreeMap<FactId, u64>,
    /// FactId → 会话侧源头 fact_id（N6 链路统一，R10；仅存 Some 项）
    fact_origins: BTreeMap<FactId, u64>,
    used_at_startup: BTreeMap<u64, Vec<FactId>>,
    /// 已标记为 rollup 的 fact_id 集合（`facts_by_path_prefix` 过滤，`fact_by_id` 不过滤）
    rolled_up: BTreeSet<FactId>,
    /// metadata 文件路径（`Some` 时写操作后持久化；`None` 时纯内存）
    metadata_path: Option<PathBuf>,
}

impl std::fmt::Debug for SharedFactsLogInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedFactsLogInner")
            .field("facts_log", &self.facts_log)
            .field("next_fact_id", &self.next_fact_id)
            .field("fact_sources_len", &self.fact_sources.len())
            .finish()
    }
}

impl SharedFactsLogInner {
    /// 持久化 metadata 到文件（须持有写锁调用）
    ///
    /// 统一持久化契约（P4-DESIGN ADR，2026-08-27）：
    /// 所有修改 `fact_sources` / `used_at_startup` / `rolled_up` / `next_fact_id`
    /// 的写操作完成后必须调用本方法。策略为 best-effort——写入失败只记 warn
    /// 日志、不阻塞调用方（metadata 是 WAL 事实历史的辅助索引，WAL 本身
    /// 才是权威数据源；重启后 recover 以 WAL 为准重建事实，metadata 缺失
    /// 仅损失消费关系/rollup 标记等治理元数据）。
    ///
    /// 已覆盖的调用方：`append`、`record_used_at_startup`、`mark_as_rollup`。
    fn persist_metadata_locked(&self) {
        if let Some(ref meta_path) = self.metadata_path {
            if let Err(e) = write_metadata_atomic(meta_path, self) {
                tracing::warn!("SharedFactsLog metadata persist failed: {e}");
            }
        }
    }
}

impl SharedFactsLog {
    /// 创建空的 SharedFactsLog（无 WAL，纯内存）
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(SharedFactsLogInner {
                facts_log: FactsLog::new(),
                next_fact_id: 1,
                fact_sources: BTreeMap::new(),
                fact_origins: BTreeMap::new(),
                used_at_startup: BTreeMap::new(),
                rolled_up: BTreeSet::new(),
                metadata_path: None,
            })),
        }
    }

    /// 创建带 WAL 持久化的 SharedFactsLog（不恢复历史，向后兼容）
    ///
    /// 注意：此方法只挂载 WAL writer，不从 WAL 恢复历史。
    /// 如需完整恢复（fact 历史 + 元数据），请使用 [`recover`](Self::recover)。
    pub fn with_wal<P: AsRef<Path>>(path: P) -> Result<Self, FactsLogError> {
        let facts_log = FactsLog::with_wal(path)?;
        Ok(Self {
            inner: Arc::new(RwLock::new(SharedFactsLogInner {
                facts_log,
                next_fact_id: 1,
                fact_sources: BTreeMap::new(),
                fact_origins: BTreeMap::new(),
                used_at_startup: BTreeMap::new(),
                rolled_up: BTreeSet::new(),
                metadata_path: None,
            })),
        })
    }

    /// 从 WAL + metadata 文件完整恢复 SharedFactsLog
    ///
    /// 与 [`with_wal`](Self::with_wal) 不同，此方法会：
    /// 1. 调用 `FactsLog::recover(wal_path)` 从 WAL 重放 fact 历史
    /// 2. 读取 metadata 文件恢复 `fact_sources`、`next_fact_id`、`rolled_up`、`used_at_startup`
    ///
    /// # 参数
    /// - `wal_path`: WAL 文件路径
    /// - `metadata_path`: metadata 文件路径（不存在时使用默认值）
    ///
    /// # 错误
    /// - WAL 恢复失败返回 `FactsLogError`
    /// - metadata 文件读取失败（文件不存在除外）返回 `FactsLogError`
    pub fn recover<P1: AsRef<Path>, P2: AsRef<Path>>(
        wal_path: P1,
        metadata_path: P2,
    ) -> Result<Self, FactsLogError> {
        // WAL 文件不存在时用 with_wal 创建新的，存在时用 recover 恢复
        let facts_log = if wal_path.as_ref().exists() {
            FactsLog::recover(&wal_path)?
        } else {
            FactsLog::with_wal(&wal_path)?
        };

        // P4-D：清理上次写入中断留下的孤儿 tmp 文件（write 与 rename 之间
        // 崩溃/断电会产生）；不影响正确性——正式文件从未被触碰——但残留
        // 会随时间堆积，且下次原子写入会被直接覆盖，无需保留。
        let tmp_path = metadata_path.as_ref().with_extension("tmp");
        if tmp_path.exists() {
            match std::fs::remove_file(&tmp_path) {
                Ok(()) => tracing::info!(tmp = %tmp_path.display(), "已清理孤儿 metadata.tmp"),
                Err(e) => {
                    tracing::warn!(tmp = %tmp_path.display(), error = %e, "清理孤儿 metadata.tmp 失败")
                }
            }
        }

        // 读取 metadata 文件（不存在时使用默认值）
        let metadata = match std::fs::read_to_string(&metadata_path) {
            Ok(content) => serde_json::from_str::<SharedFactsMetadata>(&content)
                .map_err(|e| FactsLogError::WalError(format!("metadata parse error: {e}")))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => SharedFactsMetadata::default(),
            Err(e) => return Err(FactsLogError::WalError(format!("metadata read error: {e}"))),
        };

        // 转换 u64 → FactId
        let fact_sources: BTreeMap<FactId, u64> = metadata
            .fact_sources
            .into_iter()
            .map(|(k, v)| (FactId(k), v))
            .collect();

        // R10：溯源索引恢复（旧 metadata 无此字段时为空 map，语义 = origin 未知）
        let fact_origins: BTreeMap<FactId, u64> = metadata
            .fact_origins
            .into_iter()
            .map(|(k, v)| (FactId(k), v))
            .collect();

        let used_at_startup: BTreeMap<u64, Vec<FactId>> = metadata
            .used_at_startup
            .into_iter()
            .map(|(k, v)| (k, v.into_iter().map(FactId).collect()))
            .collect();

        let rolled_up: BTreeSet<FactId> = metadata.rolled_up.into_iter().map(FactId).collect();

        // F1（audit-chain 专项 2026-08-28）：孤立 fact 结构性检测（P1-F2 兑现）。
        // 崩溃窗口（append 三步分离：fact 先落 WAL、fact_sources 后写 metadata）
        // 会产生"WAL 中有 PayloadUpdate、fact_sources 无映射"的孤立 fact——
        // 该段审计链不可归因。不自动伪造映射（source 无法从 fact 内容恢复，
        // 伪造即撒谎，与 P4-O2 消费关系单向信任原则一致），只检测 + 显式告警。
        let orphans = match detect_orphan_facts(&facts_log, &fact_sources) {
            Ok(v) => v,
            Err(e) => {
                // 恢复阶段的 facts_log 未经压实，本不应发生；一旦发生即说明
                // 集合不完整，此时"未发现孤立"是假结论——必须显式告警而非静默通过。
                tracing::warn!(
                    error = %e,
                    "孤立 fact 检测未完成：事实集合不完整（压实点前历史已从内存投影丢弃），\
                     无法确认审计链中是否存在不可归因段"
                );
                Vec::new()
            }
        };
        if !orphans.is_empty() {
            for id in &orphans {
                tracing::warn!(
                    fact_id = id,
                    "孤立共享 fact：WAL 中存在但 fact_sources 缺失（崩溃窗口产物，因果映射永久丢失，该段审计链不可归因）"
                );
            }
            tracing::warn!(
                count = orphans.len(),
                "孤立共享 fact 检测汇总：恢复的审计链中存在不可归因段，建议人工核对 WAL 与 metadata 文件"
            );
        }

        Ok(Self {
            inner: Arc::new(RwLock::new(SharedFactsLogInner {
                facts_log,
                next_fact_id: metadata.next_fact_id.max(1),
                fact_sources,
                fact_origins,
                used_at_startup,
                rolled_up,
                metadata_path: Some(metadata_path.as_ref().to_path_buf()),
            })),
        })
    }

    /// 因果一致性校验：返回孤立 fact_id 列表（压实场景下显式报错）
    ///
    /// 孤立 fact = WAL 历史中存在该 `Fact::PayloadUpdate`，但 `fact_sources`
    /// 无对应映射（崩溃窗口产物，因果映射永久丢失）。
    ///
    /// - [`recover`](Self::recover) 完成时会自动检测并逐条 `warn!`；
    /// - 本方法供运行期健康检查 / 外部审计工具调用。
    ///
    /// # 错误
    /// 若实例运行期间发生过压缩（compact），压缩点前的 fact 已从内存投影
    /// 丢弃，`read_from(0)` 拿不到完整集合——本方法返回
    /// [`Err(FactsLogError::CompactedRead)`]，**不静默漏检**：一个静默返回
    /// 空列表的审计方法，会把"没查全"冒充成"查过没问题"。
    /// 精确的全量校验应基于 WAL 离线重放工具。
    pub fn verify_causal_consistency(&self) -> Result<Vec<u64>, FactsLogError> {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        detect_orphan_facts(&inner.facts_log, &inner.fact_sources)
    }

    /// 追加共享事实
    ///
    /// # 参数
    /// - `path`: 事实路径（通常以 `shared.` 开头）
    /// - `value`: 事实值
    /// - `source_session_id`: 来源会话 ID，用于跨会话因果追溯
    ///
    /// # 返回
    /// 追加后的版本号
    pub fn append(
        &self,
        path: &str,
        value: JsonValue,
        source_session_id: u64,
    ) -> Result<u64, FactsLogError> {
        self.append_inner(path, value, source_session_id, None)
    }

    /// 追加共享事实并携带会话侧源头 fact_id（N6 链路统一，R10）
    ///
    /// 与 [`append`](Self::append) 唯一区别：共享侧条目记录其会话侧源头
    /// 事实 ID，使 `/{id}/source` 可出示双侧证据。`origin_fact_id: None`
    /// 时行为与 `append` 完全一致。
    ///
    /// # 参数
    /// - `origin_fact_id`: 会话侧源头 fact_id。调用方须保证该事实已确定
    ///   入链（server 路由先 `command_tx.send` 成功后广播，origin 永不悬空）
    ///
    /// # 返回
    /// 追加后的版本号
    pub fn append_with_origin(
        &self,
        path: &str,
        value: JsonValue,
        source_session_id: u64,
        origin_fact_id: Option<u64>,
    ) -> Result<u64, FactsLogError> {
        self.append_inner(path, value, source_session_id, origin_fact_id)
    }

    /// 两个公开 append 入口的共同实现（私有）
    fn append_inner(
        &self,
        path: &str,
        value: JsonValue,
        source_session_id: u64,
        origin_fact_id: Option<u64>,
    ) -> Result<u64, FactsLogError> {
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());

        let fact_id = FactId(inner.next_fact_id);
        inner.next_fact_id = inner
            .next_fact_id
            .checked_add(1)
            .ok_or(FactsLogError::VersionOverflow)?;

        let fact = Fact::PayloadUpdate {
            id: fact_id,
            path: path.to_string(),
            value,
        };

        let version = inner.facts_log.append(fact)?;
        inner.fact_sources.insert(fact_id, source_session_id);
        if let Some(origin) = origin_fact_id {
            inner.fact_origins.insert(fact_id, origin);
        }

        // 持久化 metadata（best-effort：失败只记日志，不阻塞 append）
        inner.persist_metadata_locked();

        Ok(version)
    }

    /// 查询指定事实的会话侧源头 fact_id（N6 链路统一，R10）
    ///
    /// # 返回
    /// 会话侧源头 fact_id；无 origin 通道（旧签名写入/历史条目）时返回 `None`
    pub fn origin_fact_id(&self, fact_id: FactId) -> Option<u64> {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        inner.fact_origins.get(&fact_id).copied()
    }

    /// 按路径前缀查询共享事实
    ///
    /// 返回所有路径以指定前缀开头的共享事实。
    /// 已标记为 rollup 的 fact 会被过滤（不显示在列表中）。
    /// 如需查询单个已 rollup 的 fact，使用 [`fact_by_id`](Self::fact_by_id)（不过滤）。
    pub fn facts_by_path_prefix(&self, prefix: &str) -> Vec<SharedFact> {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());

        inner
            .facts_log
            .facts_by_path_prefix(prefix)
            .into_iter()
            .filter_map(|(version, fact)| {
                // 不变式「只有未 rollup 的 PayloadUpdate 才进入结果」在此**一次性**编码。
                // 原实现拆成 filter + map 两次匹配 PayloadUpdate，第二次匹配只能靠
                // `unreachable` 宏兜底——一旦 Fact 新增变体或上游过滤条件变更，
                // 该兜底即成为真实 panic 点。改为单次匹配后无 panic 路径。
                match fact {
                    // 过滤已 rollup 的 fact（列表查询不显示旧摘要）
                    Fact::PayloadUpdate { id, path, value } if !inner.rolled_up.contains(&id) => {
                        Some(SharedFact {
                            fact_id: id,
                            path,
                            value,
                            source_session_id: *inner.fact_sources.get(&id).unwrap_or(&0),
                            version,
                            origin_fact_id: inner.fact_origins.get(&id).copied(),
                        })
                    }
                    _ => None,
                }
            })
            .collect()
    }

    /// 查询指定事实的来源会话 ID
    ///
    /// # 参数
    /// - `fact_id`: 事实 ID
    ///
    /// # 返回
    /// 来源会话 ID，如果找不到则返回 None
    pub fn source_session_id(&self, fact_id: FactId) -> Option<u64> {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        inner.fact_sources.get(&fact_id).copied()
    }

    /// 按事实 ID 查询完整事实信息
    ///
    /// # 参数
    /// - `fact_id`: 事实 ID
    ///
    /// # 返回
    /// 完整的共享事实信息，如果找不到则返回 None
    pub fn fact_by_id(&self, fact_id: FactId) -> Option<SharedFact> {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());

        let source_session_id = *inner.fact_sources.get(&fact_id)?;

        for (version, fact) in inner.facts_log.history_with_versions() {
            if let Fact::PayloadUpdate { id, path, value } = fact {
                if id == fact_id {
                    return Some(SharedFact {
                        fact_id: id,
                        path,
                        value,
                        source_session_id,
                        version,
                        origin_fact_id: inner.fact_origins.get(&id).copied(),
                    });
                }
            }
        }

        None
    }

    /// 返回当前版本号
    pub fn version(&self) -> u64 {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        inner.facts_log.version()
    }

    /// 返回历史记录数量
    pub fn history_len(&self) -> usize {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        inner.facts_log.history_len()
    }

    /// 记录会话启动时使用的事实 ID
    ///
    /// 用于追踪共享事实的消费关系，支持跨会话因果追溯。
    /// 如果设置了 `metadata_path`，状态会持久化到文件（原子写入），
    /// 确保重启后消费关系链不丢失。
    ///
    /// # 参数
    /// - `session_id`: 会话 ID
    /// - `fact_ids`: 该会话启动时使用的事实 ID 列表
    pub fn record_used_at_startup(&self, session_id: u64, fact_ids: &[FactId]) {
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        inner.used_at_startup.insert(session_id, fact_ids.to_vec());
        inner.persist_metadata_locked();
    }

    /// 查询会话启动时使用的事实 ID
    pub fn get_used_at_startup(&self, session_id: u64) -> Option<Vec<FactId>> {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        inner.used_at_startup.get(&session_id).cloned()
    }

    /// 查询使用指定事实的所有会话
    pub fn get_sessions_using_fact(&self, fact_id: FactId) -> Vec<u64> {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        inner
            .used_at_startup
            .iter()
            .filter(|(_, facts)| facts.contains(&fact_id))
            .map(|(session_id, _)| *session_id)
            .collect()
    }

    /// 标记 fact 为已 rollup
    ///
    /// 被标记的 fact 在 `facts_by_path_prefix` 查询中被过滤（不再显示在列表中），
    /// 但 `fact_by_id` 仍可查到（保持审计可追溯）。
    /// 如果设置了 `metadata_path`，状态会持久化到文件（原子写入）。
    ///
    /// # 参数
    /// - `fact_ids`: 要标记为 rollup 的 fact ID 列表
    ///
    /// # 返回
    /// [`RollupOutcome`]：实际标记数 + 共享账本中不存在的 id 清单。
    ///
    /// # R11（静默失败修复）
    /// 修复前：不存在的 id 也被无条件插入 `rolled_up` 并返回 `()`——调用方
    /// （server 端点）无法区分"标记成功"与"标记了不存在的 id"。典型事故：
    /// 调用方误用写入响应里的 payload 侧 fact_id（与共享表 ID 空间不通用），
    /// 得到 HTTP 200 但实际什么都没标记 → 旧摘要永不失效 → 反复 rollup 膨胀。
    /// 现在：不存在的 id **不入** `rolled_up`（不给账本塞垃圾标记），
    /// 并在返回值中显式列出，由调用方决定如何上报。
    pub fn mark_as_rollup(&self, fact_ids: &[FactId]) -> RollupOutcome {
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let mut marked = 0usize;
        let mut unknown = Vec::new();
        for id in fact_ids {
            // 存在性锚点与 fact_by_id 一致：fact_sources 表
            if inner.fact_sources.contains_key(id) {
                inner.rolled_up.insert(*id);
                marked += 1;
            } else {
                unknown.push(*id);
            }
        }
        // 持久化 metadata（best-effort）
        inner.persist_metadata_locked();
        RollupOutcome { marked, unknown }
    }

    /// 查询某 fact 是否已标记为 rollup（诊断/审计用途，R11 新增）
    pub fn is_rolled_up(&self, fact_id: &FactId) -> bool {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        inner.rolled_up.contains(fact_id)
    }

    /// 重置 SharedFactsLog 到初始状态
    ///
    /// # 安全性边界（F6，audit-chain 专项 2026-08-28 标注）
    ///
    /// **仅限纯内存实例调用**。底层 [`FactsLog::reset`](evorule_reactor::FactsLog::reset)
    /// 只丢弃内存投影并将 WAL 写入器置为 `None`，**不会删除磁盘上的 WAL 文件**。
    /// 若实例配置了 `metadata_path`（含 WAL 持久化），调用本方法后：
    /// - 已落盘的旧事实在重启重放时会**全部复活**；
    /// - metadata（`fact_sources` 等）虽同步清空，但与复活的事实形成新的
    ///   孤儿状态，破坏审计链完整性。
    ///
    /// 需要彻底清空持久化实例时，必须先删除 WAL 与 metadata 文件再重建实例，
    /// 而不是调用本方法。
    pub fn reset(&self) {
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        inner.facts_log.reset();
        inner.next_fact_id = 1;
        inner.fact_sources.clear();
        inner.used_at_startup.clear();
        inner.rolled_up.clear();
    }
}

/// 原子写入 metadata 文件（write tmp + rename，防断电损坏）
///
/// 将 `SharedFactsLogInner` 的元数据序列化为 JSON，先写入临时文件再 rename，
/// 确保断电时不会产生损坏的 metadata 文件。
fn write_metadata_atomic(path: &Path, inner: &SharedFactsLogInner) -> Result<(), FactsLogError> {
    let metadata = SharedFactsMetadata {
        next_fact_id: inner.next_fact_id,
        fact_sources: inner.fact_sources.iter().map(|(k, v)| (k.0, *v)).collect(),
        fact_origins: inner.fact_origins.iter().map(|(k, v)| (k.0, *v)).collect(),
        used_at_startup: inner
            .used_at_startup
            .iter()
            .map(|(k, v)| (*k, v.iter().map(|f| f.0).collect()))
            .collect(),
        rolled_up: inner.rolled_up.iter().map(|f| f.0).collect(),
    };

    let json = serde_json::to_string(&metadata)
        .map_err(|e| FactsLogError::WalError(format!("metadata serialize error: {e}")))?;

    let tmp_path = path.with_extension("tmp");
    std::fs::write(&tmp_path, json)
        .map_err(|e| FactsLogError::WalError(format!("metadata write error: {e}")))?;
    std::fs::rename(&tmp_path, path)
        .map_err(|e| FactsLogError::WalError(format!("metadata rename error: {e}")))?;

    Ok(())
}

impl Default for SharedFactsLog {
    fn default() -> Self {
        Self::new()
    }
}

/// 孤立 fact 检测：WAL 历史中的 PayloadUpdate 集合 − fact_sources 键集
///
/// 只认 `Fact::PayloadUpdate`（SharedFactsLog.append 的唯一产物形态）。
///
/// # 错误
/// 若 facts_log 发生过压缩，压缩点前的历史已从内存投影丢弃，本函数无法
/// 取得完整集合，返回 [`FactsLogError::CompactedRead`]——**不返回空列表**，
/// 以免"校验不完整"被误读为"未发现孤立"。
fn detect_orphan_facts(
    facts_log: &FactsLog,
    fact_sources: &BTreeMap<FactId, u64>,
) -> Result<Vec<u64>, FactsLogError> {
    let wal_fact_ids: BTreeSet<u64> = facts_log
        .read_from(0)?
        .iter()
        .filter_map(|f| match f {
            Fact::PayloadUpdate { id, .. } => Some(id.0),
            _ => None,
        })
        .collect();
    Ok(wal_fact_ids
        .iter()
        .filter(|id| !fact_sources.contains_key(&FactId(**id)))
        .copied()
        .collect())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::panic, clippy::expect_used)]
    use super::*;

    #[test]
    fn test_shared_facts_log_new() {
        let log = SharedFactsLog::new();
        assert_eq!(log.version(), 0);
        assert_eq!(log.history_len(), 0);
    }

    #[test]
    fn test_shared_facts_log_append() {
        let log = SharedFactsLog::new();

        let version = log
            .append("shared.research.note1", JsonValue::string("hello"), 100)
            .unwrap();

        assert_eq!(version, 1); // 断点 11: PayloadUpdate 现在递增 version
        assert_eq!(log.history_len(), 1);
    }

    #[test]
    fn test_shared_facts_log_facts_by_path_prefix() {
        let log = SharedFactsLog::new();

        log.append("shared.research.note1", JsonValue::string("v1"), 100)
            .unwrap();
        log.append("shared.research.note2", JsonValue::string("v2"), 100)
            .unwrap();
        log.append("shared.other.data", JsonValue::string("v3"), 200)
            .unwrap();

        let result = log.facts_by_path_prefix("shared.research");
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].path, "shared.research.note1");
        assert_eq!(result[1].path, "shared.research.note2");
        assert_eq!(result[0].source_session_id, 100);
        assert_eq!(result[1].source_session_id, 100);
    }

    #[test]
    fn test_shared_facts_log_facts_by_path_prefix_no_matches() {
        let log = SharedFactsLog::new();

        log.append("shared.research.note1", JsonValue::string("v1"), 100)
            .unwrap();

        let result = log.facts_by_path_prefix("shared.other");
        assert!(result.is_empty());
    }

    #[test]
    fn test_shared_facts_log_facts_by_path_prefix_empty() {
        let log = SharedFactsLog::new();

        let result = log.facts_by_path_prefix("any_prefix");
        assert!(result.is_empty());
    }

    #[test]
    fn test_shared_facts_log_source_session_id() {
        let log = SharedFactsLog::new();

        log.append("shared.note", JsonValue::string("v1"), 100)
            .unwrap();

        let result = log.facts_by_path_prefix("shared");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].source_session_id, 100);
    }

    #[test]
    fn test_shared_facts_log_reset() {
        let log = SharedFactsLog::new();

        log.append("shared.note", JsonValue::string("v1"), 100)
            .unwrap();
        assert_eq!(log.history_len(), 1);

        log.reset();
        assert_eq!(log.history_len(), 0);
        assert_eq!(log.version(), 0);
    }

    // ===== L-3: mark_as_rollup + recover 测试 =====

    #[test]
    fn test_mark_as_rollup_filters_from_prefix_query() {
        let log = SharedFactsLog::new();

        log.append(
            "shared.ns.sessions.s1.summary",
            JsonValue::string("s1"),
            100,
        )
        .unwrap();
        log.append(
            "shared.ns.sessions.s2.summary",
            JsonValue::string("s2"),
            100,
        )
        .unwrap();
        log.append(
            "shared.ns.sessions.s3.summary",
            JsonValue::string("s3"),
            100,
        )
        .unwrap();

        // 标记 s1 为 rollup
        log.mark_as_rollup(&[FactId(1)]);

        // 列表查询：s1 被过滤
        let result = log.facts_by_path_prefix("shared.ns.sessions.");
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].fact_id, FactId(2));
        assert_eq!(result[1].fact_id, FactId(3));
    }

    #[test]
    fn test_mark_as_rollup_fact_by_id_still_visible() {
        let log = SharedFactsLog::new();

        log.append(
            "shared.ns.sessions.s1.summary",
            JsonValue::string("s1"),
            100,
        )
        .unwrap();

        log.mark_as_rollup(&[FactId(1)]);

        // fact_by_id 不过滤 rolled_up（审计可追溯）
        let fact = log.fact_by_id(FactId(1));
        assert!(fact.is_some());
        assert_eq!(fact.unwrap().path, "shared.ns.sessions.s1.summary");
    }

    #[test]
    fn test_mark_as_rollup_unknown_ids_reported_not_marked() {
        // R11：不存在的 id 不得静默入 rolled_up，必须显式上报
        let log = SharedFactsLog::new();

        log.append(
            "shared.ns.sessions.s1.summary",
            JsonValue::string("s1"),
            100,
        )
        .unwrap();

        // 混合批次：1 存在、999/1000 不存在（payload 侧 id 误用的典型形态）
        let outcome = log.mark_as_rollup(&[FactId(1), FactId(999), FactId(1000)]);
        assert_eq!(outcome.marked, 1, "只有存在的 id 被标记");
        assert_eq!(
            outcome.unknown,
            vec![FactId(999), FactId(1000)],
            "不存在的 id 必须逐个上报"
        );

        // 不存在的 id 不进 rolled_up（不给账本塞垃圾标记）
        assert!(!log.is_rolled_up(&FactId(999)));
        assert!(log.is_rolled_up(&FactId(1)));

        // 幂等：已标记的 id 再次标记 = marked，不算 unknown
        let again = log.mark_as_rollup(&[FactId(1)]);
        assert_eq!(again.marked, 1);
        assert!(again.unknown.is_empty());
    }

    #[test]
    fn test_recover_restores_facts_and_metadata() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let wal_path = dir.path().join("shared.wal");
        let meta_path = dir.path().join("shared.json");

        // 第一次创建：写入数据 + rollup
        {
            let log = SharedFactsLog::recover(&wal_path, &meta_path).unwrap();
            log.append("shared.ns.stable.key1", JsonValue::string("v1"), 100)
                .unwrap();
            log.append(
                "shared.ns.sessions.s1.summary",
                JsonValue::string("s1"),
                100,
            )
            .unwrap();
            log.mark_as_rollup(&[FactId(2)]); // rollup s1 summary
        }

        // 第二次创建：从 WAL + metadata 恢复
        {
            let log = SharedFactsLog::recover(&wal_path, &meta_path).unwrap();
            assert_eq!(log.history_len(), 2); // fact 历史恢复
            assert_eq!(log.version(), 2);

            // fact_sources 恢复
            assert_eq!(log.source_session_id(FactId(1)), Some(100));

            // rolled_up 恢复：prefix 查询过滤了 FactId(2)
            let result = log.facts_by_path_prefix("shared.ns.");
            assert_eq!(result.len(), 1);
            assert_eq!(result[0].fact_id, FactId(1));

            // fact_by_id 仍可查到 rolled_up 的 fact
            assert!(log.fact_by_id(FactId(2)).is_some());

            // next_fact_id 恢复：下一次 append 从 3 开始
            let v = log
                .append("shared.ns.stable.key2", JsonValue::string("v2"), 200)
                .unwrap();
            assert_eq!(v, 3);
        }
    }

    #[test]
    fn test_recover_metadata_not_found_uses_defaults() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let wal_path = dir.path().join("shared.wal");
        let meta_path = dir.path().join("nonexistent.json");

        // metadata 文件不存在 → 使用默认值，不报错
        let log = SharedFactsLog::recover(&wal_path, &meta_path).unwrap();
        assert_eq!(log.history_len(), 0);
        assert_eq!(log.version(), 0);
    }

    /// P4-BUG 回归：record_used_at_startup 必须持久化，
    /// 重启（recover）后消费关系链不得丢失。
    #[test]
    fn test_record_used_at_startup_persists_across_recover() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let wal_path = dir.path().join("shared.wal");
        let meta_path = dir.path().join("shared_meta.json");

        {
            let log = SharedFactsLog::recover(&wal_path, &meta_path).unwrap();
            log.append("shared.ns.key", JsonValue::string("v1"), 100)
                .unwrap();
            log.record_used_at_startup(42, &[FactId(1), FactId(2)]);
            log.record_used_at_startup(43, &[FactId(1)]);
        } // drop：模拟进程退出

        // 模拟重启恢复
        let log2 = SharedFactsLog::recover(&wal_path, &meta_path).unwrap();
        assert_eq!(
            log2.get_used_at_startup(42),
            Some(vec![FactId(1), FactId(2)]),
            "重启后 used_at_startup(42) 丢失：P4-BUG 回归失败"
        );
        assert_eq!(log2.get_used_at_startup(43), Some(vec![FactId(1)]));
        assert_eq!(
            log2.get_sessions_using_fact(FactId(2)),
            vec![42],
            "重启后消费关系查询结果不完整"
        );
    }

    /// P4-D 回归：recover 时清理孤儿 metadata.tmp。
    #[test]
    fn test_recover_removes_orphan_tmp_file() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let wal_path = dir.path().join("shared.wal");
        let meta_path = dir.path().join("shared_meta.json");

        let tmp_path = meta_path.with_extension("tmp");
        std::fs::write(&tmp_path, b"{orphan").unwrap();

        let _log = SharedFactsLog::recover(&wal_path, &meta_path).unwrap();
        assert!(
            !tmp_path.exists(),
            "孤儿 metadata.tmp 未被 recover 清理：P4-D 回归失败"
        );
    }

    /// F1 回归（audit-chain 专项 2026-08-28）：崩溃窗口产生的孤立 fact
    /// （WAL 有 PayloadUpdate、fact_sources 缺映射）必须被检出并可查询。
    /// 模拟方式：正常 append 两条后 drop，再手写一份"只有一条映射"的
    /// metadata（模拟 fact_sources 写入前进程崩溃）。
    #[test]
    fn test_recover_detects_orphan_facts_from_crash_window() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let wal_path = dir.path().join("shared.wal");
        let meta_path = dir.path().join("shared_meta.json");

        {
            let log = SharedFactsLog::recover(&wal_path, &meta_path).unwrap();
            log.append("shared.ns.a", JsonValue::string("v1"), 100)
                .unwrap();
            log.append("shared.ns.b", JsonValue::string("v2"), 101)
                .unwrap();
        } // drop：模拟进程退出（此时两条映射都已持久化）

        // 模拟崩溃窗口：metadata 只含 fact_id=1 的映射（fact_id=2 的映射丢失）
        let degraded = serde_json::json!({
            "fact_sources": {"1": 100},
            "next_fact_id": 2,
            "rolled_up": [],
            "used_at_startup": {}
        });
        std::fs::write(&meta_path, degraded.to_string()).unwrap();

        let log2 = SharedFactsLog::recover(&wal_path, &meta_path).unwrap();
        let orphans = log2.verify_causal_consistency().unwrap();
        assert_eq!(
            orphans,
            vec![2],
            "崩溃窗口产生的孤立 fact 未被检出：F1 回归失败"
        );
    }

    /// F1 对照：完整 metadata（无崩溃窗口）时不得误报。
    #[test]
    fn test_verify_causal_consistency_no_false_positive_when_consistent() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let wal_path = dir.path().join("shared.wal");
        let meta_path = dir.path().join("shared_meta.json");

        let log = SharedFactsLog::recover(&wal_path, &meta_path).unwrap();
        log.append("shared.ns.a", JsonValue::string("v1"), 100)
            .unwrap();
        log.append("shared.ns.b", JsonValue::string("v2"), 101)
            .unwrap();

        assert!(
            log.verify_causal_consistency().unwrap().is_empty(),
            "一致的 WAL+metadata 被误报为孤立：F1 对照测试失败"
        );
    }

    // ===== R10：origin_fact_id 跨链溯源（N6） =====

    /// append_with_origin → 前缀查询 / fact_by_id / 独立查询三出口均携带 origin
    #[test]
    fn test_append_with_origin_visible_in_all_query_paths() {
        let log = SharedFactsLog::new();

        log.append_with_origin("shared.ns.key", JsonValue::string("v1"), 100, Some(53))
            .unwrap();

        let facts = log.facts_by_path_prefix("shared.ns.");
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].origin_fact_id, Some(53), "前缀查询应携带 origin");

        let by_id = log.fact_by_id(FactId(1)).unwrap();
        assert_eq!(by_id.origin_fact_id, Some(53), "fact_by_id 应携带 origin");

        assert_eq!(log.origin_fact_id(FactId(1)), Some(53), "独立查询应命中");
        assert_eq!(log.origin_fact_id(FactId(999)), None, "未知 id 应返回 None");
    }

    /// 旧签名 append（无 origin 通道）→ origin 恒 None（向后兼容语义）
    #[test]
    fn test_append_without_origin_is_none() {
        let log = SharedFactsLog::new();

        log.append("shared.ns.key", JsonValue::string("v1"), 100)
            .unwrap();

        let facts = log.facts_by_path_prefix("shared.ns.");
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].origin_fact_id, None, "旧签名写入不得伪造 origin");
        assert_eq!(log.origin_fact_id(FactId(1)), None);
    }

    /// origin 随 metadata 持久化，recover 后可查（与 fact_sources 同级语义）
    #[test]
    fn test_origin_persists_across_recover() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let wal_path = dir.path().join("shared.wal");
        let meta_path = dir.path().join("shared_meta.json");

        {
            let log = SharedFactsLog::recover(&wal_path, &meta_path).unwrap();
            log.append_with_origin("shared.ns.key", JsonValue::string("v1"), 100, Some(53))
                .unwrap();
        } // drop：模拟进程退出

        let log2 = SharedFactsLog::recover(&wal_path, &meta_path).unwrap();
        assert_eq!(
            log2.origin_fact_id(FactId(1)),
            Some(53),
            "重启后 origin 丢失：R10 持久化回归失败"
        );
        let facts = log2.facts_by_path_prefix("shared.ns.");
        assert_eq!(facts[0].origin_fact_id, Some(53));
    }

    /// 旧 metadata 文件（无 fact_origins 字段）→ 恢复零报错、origin=None
    /// （serde(default) 是升级兼容的硬前提：升级前数据不得因新字段拒绝加载）
    #[test]
    fn test_legacy_metadata_without_origins_recovers_cleanly() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let wal_path = dir.path().join("shared.wal");
        let meta_path = dir.path().join("shared_meta.json");

        // 先正常写入产生 WAL
        {
            let log = SharedFactsLog::recover(&wal_path, &meta_path).unwrap();
            log.append("shared.ns.key", JsonValue::string("v1"), 100)
                .unwrap();
        }
        // 手写 R10 之前形态的 metadata（无 fact_origins 字段）
        let legacy = serde_json::json!({
            "fact_sources": {"1": 100},
            "next_fact_id": 2,
            "rolled_up": [],
            "used_at_startup": {}
        });
        std::fs::write(&meta_path, legacy.to_string()).unwrap();

        let log2 = SharedFactsLog::recover(&wal_path, &meta_path).unwrap();
        assert_eq!(
            log2.origin_fact_id(FactId(1)),
            None,
            "旧 metadata 恢复后 origin 应为 None"
        );
        // 事实与来源不受影响
        assert_eq!(log2.source_session_id(FactId(1)), Some(100));
        assert_eq!(log2.history_len(), 1);
    }
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Write-Ahead Log (WAL) —— 事实审计链的磁盘持久化
//!
//! # 设计依据
//! P0-1（原始问题）：FactsLog 原本仅内存存储，进程崩溃即丢失。
//! WAL 在每次 `append` 之前先将 Fact 序列化为 JSONL 写入磁盘并 flush，
//! 保证进程崩溃/重启/OOM 后可通过 `recover` 重放事实恢复状态。
//!
//! # WAL 格式（JSONL，每行一条记录）
//!
//! ## 新格式（v2，含哈希链）
//! ```json
//! {"version_before": 0, "fact": {"type": "Command", "id": 1, "instruction": {...}},
//!  "content_hash": "a3f5e8...", "prev_hash": "genesis", "chain_hash": "b7c9d1..."}
//! ```
//!
//! ## 旧格式（v1，无哈希，向后兼容）
//! ```json
//! {"version_before": 0, "fact": {"type": "Command", "id": 1, "instruction": {...}}}
//! ```
//!
//! 读取时自动识别格式：有 `chain_hash` 字段 → 新格式，无 → 旧格式。
//!
//! # 持久化级别
//! - `flush`：保证进程崩溃/重启/OOM 不丢数据（P0 风险场景）
//! - `fsync`：保证断电不丢数据（P1+ 优化项，P0 暂不实现，权衡 fsync 的性能损耗）
//!
//! # 序列化策略
//! 不使用 `#[derive(Serialize/Deserialize)]`，原因有二：
//! 1. evorule-tcb 是 `no_std` + 零依赖 crate，不能引入 serde 派生宏
//! 2. 孤儿规则禁止在 tier1 为 tier0 类型实现外部 trait
//!
//! 改用 free function 桥接：`tcb_to_serde`/`serde_to_tcb` 转换
//! `JsonValue ↔ serde_json::Value`，再由 `fact_to_json`/`fact_from_json`
//! 手动处理 7 种 Fact 变体的序列化。
//!
//! # 恢复流程
//! 1. `read_wal(path)` 读取所有 (version_before, Fact) 记录
//! 2. `FactsLog::recover(path)` 重放事实到内存状态（WAL 未挂载，不重复写）
//! 3. 重放完成后以 `WalWriter::append` 模式挂载 WAL，继续追加新事实

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use evorule_tcb::JsonValue;

use crate::fact::{Fact, FactId, IoType, TraceDomainAttribution, TraceHit};

/// WAL 错误类型
#[derive(Debug)]
pub enum WalError {
    /// 底层 I/O 错误（文件读写失败）
    Io(std::io::Error),
    /// JSON 序列化/反序列化错误
    Json(serde_json::Error),
    /// Fact 反序列化失败（未知类型名/缺字段/字段类型不匹配）
    InvalidFact(String),
}

impl core::fmt::Display for WalError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            WalError::Io(e) => write!(f, "WAL I/O error: {e}"),
            WalError::Json(e) => write!(f, "WAL JSON error: {e}"),
            WalError::InvalidFact(msg) => write!(f, "WAL invalid fact: {msg}"),
        }
    }
}

impl std::error::Error for WalError {}

impl From<std::io::Error> for WalError {
    fn from(e: std::io::Error) -> Self {
        WalError::Io(e)
    }
}

impl From<serde_json::Error> for WalError {
    fn from(e: serde_json::Error) -> Self {
        WalError::Json(e)
    }
}

/// WAL 记录（带可选哈希字段）
///
/// 两套 WAL 合并后的统一记录格式。
/// 哈希字段为 `Option`，兼容旧格式（无哈希）和新格式（有哈希）。
///
/// # 字段说明
/// - `version_before`: 追加前的版本号
/// - `fact`: 事实内容
/// - `content_hash`: Fact 的 BLAKE3 哈希（新格式有，旧格式无）
/// - `prev_hash`: 前一条记录的链哈希（新格式有，旧格式无）
/// - `chain_hash`: 当前链哈希 `blake3(prev_hash + content_hash)`（新格式有，旧格式无）
#[derive(Debug, Clone)]
pub struct WalRecord {
    /// 追加前的版本号
    pub version_before: u64,
    /// 事实内容
    pub fact: Fact,
    /// Fact 的 BLAKE3 哈希（新格式有，旧格式无）
    pub content_hash: Option<String>,
    /// 前一条记录的链哈希（新格式有，旧格式无）
    pub prev_hash: Option<String>,
    /// 当前链哈希（新格式有，旧格式无）
    pub chain_hash: Option<String>,
}

impl WalRecord {
    /// 判断是否为带哈希的新格式记录
    pub fn has_hash(&self) -> bool {
        self.chain_hash.is_some()
    }
}

/// 事实 WAL 存储后端契约（存储层 trait 抽象）
///
/// write-ahead 语义：`FactsLog::append` 在更新内存状态**之前**调用本方法，
/// 实现 `Ok` 返回即承诺记录已落（后端自行承担崩溃/断电不丢语义）。
///
/// 默认实现 = [`WalWriter`]（文件后端，行为与历史上 WAL 直挂完全一致）；
/// 备选实现 = [`MemoryWalStore`]（纯内存，嵌入式/测试/离线场景）。
/// 第三方可实现本 trait 接入自有存储（SQLite/远程等）。
///
/// 契约要点：
/// - 哈希链字段（content_hash/prev_hash/chain_hash）必须原样保存，不得重算或丢弃；
///   回放/审计一致性依赖"写入即所见"
/// - 同一实例上记录的追加顺序即调用顺序（append-only，不提供改删）
pub trait FactWalStore: Send + Sync {
    /// 追加一条带哈希链的记录（write-ahead：内存更新前调用）
    fn append_record_with_hash(
        &mut self,
        version_before: u64,
        fact: &Fact,
        content_hash: &str,
        prev_hash: &str,
        chain_hash: &str,
    ) -> Result<(), WalError>;

    /// 轮换底层存储（干预原语：文件后端切新分片）
    ///
    /// 返回 `Ok(true)` = 实际发生轮换；`Ok(false)` = 本后端无可轮换面
    /// （内存后端等）。默认无操作。
    fn rotate(&mut self) -> Result<bool, WalError> {
        let _ = self;
        Ok(false)
    }
}

impl FactWalStore for WalWriter {
    fn append_record_with_hash(
        &mut self,
        version_before: u64,
        fact: &Fact,
        content_hash: &str,
        prev_hash: &str,
        chain_hash: &str,
    ) -> Result<(), WalError> {
        // 纯委托：文件后端行为与历史实现逐字节一致（默认实现行为不变）
        WalWriter::append_record_with_hash(
            self,
            version_before,
            fact,
            content_hash,
            prev_hash,
            chain_hash,
        )
    }

    fn rotate(&mut self) -> Result<bool, WalError> {
        // 纯委托：切新分片，旧分片保持完整（崩溃安全）
        WalWriter::rotate(self).map(|()| true)
    }
}

/// 纯内存事实 WAL 后端（：离线/嵌入式/测试受益项）
///
/// - 记录保存在进程内存（`Arc<Mutex<Vec<WalRecord>>>`），哈希链字段原样保存；
/// - 进程退出即失（与 FactsLog 纯内存模式同语义），适用于无文件系统、
///   短命嵌入式会话、以及需要检视 WAL 内容的单测；
/// - `Clone` 为共享句柄语义：clone 与原实例看到同一条记录序列，
///   调用方在把实例交给 `FactsLog::with_wal_store` 前先保留一个 clone
///   即可事后检视（`records()` / `into_records()`）。
#[derive(Debug, Clone, Default)]
pub struct MemoryWalStore {
    records: std::sync::Arc<std::sync::Mutex<Vec<WalRecord>>>,
}

impl MemoryWalStore {
    /// 创建空内存后端
    pub fn new() -> Self {
        Self::default()
    }

    /// 已追加的记录数
    pub fn len(&self) -> usize {
        self.records.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// 是否为空
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 当前全部记录快照（顺序 = 追加顺序；后续追加不反映在返回值中）
    pub fn records(&self) -> Vec<WalRecord> {
        self.records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 取出全部已追加记录（消耗共享句柄；顺序 = 追加顺序）
    pub fn into_records(self) -> Vec<WalRecord> {
        match std::sync::Arc::try_unwrap(self.records) {
            Ok(mutex) => mutex.into_inner().unwrap_or_else(|e| e.into_inner()),
            Err(arc) => {
                let mut guard = arc.lock().unwrap_or_else(|e| e.into_inner());
                guard.drain(..).collect()
            }
        }
    }
}

impl FactWalStore for MemoryWalStore {
    fn append_record_with_hash(
        &mut self,
        version_before: u64,
        fact: &Fact,
        content_hash: &str,
        prev_hash: &str,
        chain_hash: &str,
    ) -> Result<(), WalError> {
        self.records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(WalRecord {
                version_before,
                fact: fact.clone(),
                content_hash: Some(content_hash.to_string()),
                prev_hash: Some(prev_hash.to_string()),
                chain_hash: Some(chain_hash.to_string()),
            });
        Ok(())
    }
}

/// tier0 `JsonValue` → `serde_json::Value`
///
/// 直接映射，因 tier0 无 Float 类型，故整数/字符串/布尔/null/数组/对象一一对应。
pub fn tcb_to_serde(v: &JsonValue) -> serde_json::Value {
    match v {
        JsonValue::Null => serde_json::Value::Null,
        JsonValue::Bool(b) => serde_json::Value::Bool(*b),
        JsonValue::Integer(i) => serde_json::Value::Number((*i).into()),
        // u64 全域通道（v0.9.0）：serde Number 原生无损承载，不经 i64 cast
        JsonValue::U64(v) => serde_json::Value::Number((*v).into()),
        JsonValue::String(s) => serde_json::Value::String(s.to_string()),
        JsonValue::Array(arr) => serde_json::Value::Array(arr.iter().map(tcb_to_serde).collect()),
        JsonValue::Object(map) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in map.iter() {
                obj.insert(k.clone(), tcb_to_serde(v));
            }
            serde_json::Value::Object(obj)
        }
    }
}

/// `serde_json::Value` → tier0 `JsonValue`
///
/// # Float 处理
/// tier0 无 Float 类型（形式化验证约束）。遇到 serde 的浮点数时，
/// 转为字符串保留精度（`n.to_string()`），避免静默截断。
/// u64 超 i64 范围时走 `U64` 变体无损承载（与 `tcb_to_serde` 正程对称）。
pub fn serde_to_tcb(v: &serde_json::Value) -> JsonValue {
    match v {
        serde_json::Value::Null => JsonValue::Null,
        serde_json::Value::Bool(b) => JsonValue::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                JsonValue::Integer(i)
            } else if let Some(u) = n.as_u64() {
                // u64 全域通道回程：`as_i64` 未命中说明该值超 i64 范围，
                // 走 `U64` 变体无损承载（转字符串会破坏 WAL 往返保形，
                // 导致回放重算 fact_hash 失配）
                JsonValue::U64(u)
            } else {
                // 浮点数：tier0 无 Float，转字符串保留精度
                JsonValue::string(n.to_string())
            }
        }
        serde_json::Value::String(s) => JsonValue::string(s),
        serde_json::Value::Array(arr) => JsonValue::Array(arr.iter().map(serde_to_tcb).collect()),
        serde_json::Value::Object(map) => {
            let mut obj = BTreeMap::new();
            for (k, v) in map.iter() {
                obj.insert(k.clone(), serde_to_tcb(v));
            }
            JsonValue::Object(obj)
        }
    }
}

/// 域归因条目 → JSON（None 字段省略）
///
/// 与 `hash.rs` 的同名私有函数保持同构：WAL 持久化与哈希链序列化
/// 必须产出一致形状，否则回放重算哈希将失配。
fn trace_attr_to_json(attr: &TraceDomainAttribution) -> serde_json::Value {
    let mut attr_obj = serde_json::Map::new();
    attr_obj.insert(
        "outcome".into(),
        serde_json::Value::String(attr.outcome.clone()),
    );
    if let Some(reason) = &attr.missing_reason {
        attr_obj.insert(
            "missing_reason".into(),
            serde_json::Value::String(reason.clone()),
        );
    }
    if let Some(policy) = &attr.on_missing {
        attr_obj.insert(
            "on_missing".into(),
            serde_json::Value::String(policy.clone()),
        );
    }
    serde_json::Value::Object(attr_obj)
}

/// `Fact` → `serde_json::Value`
///
/// 每个变体序列化为带 `type` 鉴别字段的 JSON 对象，例如：
/// ```json
/// {"type": "Command", "id": 1, "instruction": {...}}
/// ```
// 注：match 全部 Fact 变体的序列化分支，有意保持单一函数便于对账字段映射。
#[allow(clippy::too_many_lines)]
pub fn fact_to_json(fact: &Fact) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    match fact {
        Fact::Command { id, instruction } => {
            obj.insert("type".into(), serde_json::Value::String("Command".into()));
            obj.insert("id".into(), serde_json::Value::Number(id.0.into()));
            obj.insert("instruction".into(), tcb_to_serde(instruction));
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
                serde_json::Value::String(io_type.as_str().into()),
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
            obj.insert(
                "error".into(),
                match error {
                    Some(msg) => serde_json::Value::String(msg.clone()),
                    None => serde_json::Value::Null,
                },
            );
        }
        Fact::Stable { id, version } => {
            obj.insert("type".into(), serde_json::Value::String("Stable".into()));
            obj.insert("id".into(), serde_json::Value::Number(id.0.into()));
            obj.insert(
                "version".into(),
                serde_json::Value::Number((*version).into()),
            );
        }
        Fact::Error { id, message } => {
            obj.insert("type".into(), serde_json::Value::String("Error".into()));
            obj.insert("id".into(), serde_json::Value::Number(id.0.into()));
            obj.insert("message".into(), serde_json::Value::String(message.clone()));
        }
        Fact::TransitionTrace {
            id,
            cause,
            rule_hits,
        } => {
            obj.insert(
                "type".into(),
                serde_json::Value::String("TransitionTrace".into()),
            );
            obj.insert("id".into(), serde_json::Value::Number(id.0.into()));
            obj.insert("cause".into(), serde_json::Value::Number(cause.0.into()));
            let hits: Vec<serde_json::Value> = rule_hits
                .iter()
                .map(|h| {
                    // domain_attr 仅在 Some 时写入键（None 省略）：
                    // 存量 WAL 记录无该键，解析侧 absent → None 兼容旧格式。
                    let mut hit_obj = serde_json::Map::new();
                    hit_obj.insert("index".into(), serde_json::json!(h.index));
                    hit_obj.insert(
                        "instr_type".into(),
                        serde_json::Value::String(h.instr_type.clone()),
                    );
                    hit_obj.insert("hit".into(), serde_json::Value::Bool(h.hit));
                    if let Some(attr) = &h.domain_attr {
                        hit_obj.insert("domain_attr".into(), trace_attr_to_json(attr));
                    }
                    serde_json::Value::Object(hit_obj)
                })
                .collect();
            obj.insert("rule_hits".into(), serde_json::Value::Array(hits));
        }
        Fact::Violation {
            id,
            cause,
            rule_index,
            reason,
            instruction,
        } => {
            obj.insert("type".into(), serde_json::Value::String("Violation".into()));
            obj.insert("id".into(), serde_json::Value::Number(id.0.into()));
            obj.insert("cause".into(), serde_json::Value::Number(cause.0.into()));
            obj.insert(
                "rule_index".into(),
                serde_json::Value::Number((*rule_index).into()),
            );
            obj.insert("reason".into(), serde_json::Value::String(reason.clone()));
            obj.insert("instruction".into(), tcb_to_serde(instruction));
        }
    }
    serde_json::Value::Object(obj)
}

/// `serde_json::Value` → `Fact`
///
/// 反序列化带 `type` 鉴别字段的 JSON 对象。任何字段缺失或类型不匹配
/// 均返回 `WalError::InvalidFact`，调用方可决定跳过该行或中止恢复。
// 7 种 Fact 变体扁平 match, 拆函数需共享中间变量。详见 GATE_REFERENCE.md §六(豁免索引)
#[allow(clippy::too_many_lines)]
pub fn fact_from_json(v: &serde_json::Value) -> Result<Fact, WalError> {
    let obj = v
        .as_object()
        .ok_or_else(|| WalError::InvalidFact("fact is not an object".into()))?;
    let type_str = obj
        .get("type")
        .and_then(|t| t.as_str())
        .ok_or_else(|| WalError::InvalidFact("missing 'type' field".into()))?;
    // FactId 为 u64 全域：G6 规则集锚定事实用 u64::MAX-1 哨兵 id（session.rs
    // 落链 __ruleset__.version），as_i64 对 u64 高位返回 None 会误报
    // missing/invalid 'id'——必须按无符号解析。
    let id_raw = obj
        .get("id")
        .and_then(|i| i.as_u64())
        .ok_or_else(|| WalError::InvalidFact("missing/invalid 'id' field".into()))?;
    let id = FactId(id_raw);

    match type_str {
        "Command" => {
            let instruction = obj
                .get("instruction")
                .ok_or_else(|| WalError::InvalidFact("Command missing 'instruction'".into()))?;
            Ok(Fact::Command {
                id,
                instruction: serde_to_tcb(instruction),
            })
        }
        "PayloadUpdate" => {
            let path = obj
                .get("path")
                .and_then(|p| p.as_str())
                .ok_or_else(|| WalError::InvalidFact("PayloadUpdate missing 'path'".into()))?;
            let value = obj
                .get("value")
                .ok_or_else(|| WalError::InvalidFact("PayloadUpdate missing 'value'".into()))?;
            Ok(Fact::PayloadUpdate {
                id,
                path: path.into(),
                value: serde_to_tcb(value),
            })
        }
        "StateTransition" => {
            let cause_raw = obj
                .get("cause")
                .and_then(|c| c.as_u64())
                .ok_or_else(|| WalError::InvalidFact("StateTransition missing 'cause'".into()))?;
            let new_payload = obj.get("new_payload").ok_or_else(|| {
                WalError::InvalidFact("StateTransition missing 'new_payload'".into())
            })?;
            let new_queue_arr =
                obj.get("new_queue")
                    .and_then(|q| q.as_array())
                    .ok_or_else(|| {
                        WalError::InvalidFact("StateTransition missing 'new_queue'".into())
                    })?;
            let new_queue: Vec<JsonValue> = new_queue_arr.iter().map(serde_to_tcb).collect();
            Ok(Fact::StateTransition {
                id,
                cause: FactId(cause_raw),
                new_payload: serde_to_tcb(new_payload),
                new_queue,
            })
        }
        "IoRequest" => {
            let cause_raw = obj
                .get("cause")
                .and_then(|c| c.as_u64())
                .ok_or_else(|| WalError::InvalidFact("IoRequest missing 'cause'".into()))?;
            let io_type_str = obj
                .get("io_type")
                .and_then(|t| t.as_str())
                .ok_or_else(|| WalError::InvalidFact("IoRequest missing 'io_type'".into()))?;
            // v0.2.0：io_type 透传不校验（崩溃恢复含自定义 io_type 的 WAL 必须可恢复）
            let io_type = IoType::new(io_type_str);
            let params = obj
                .get("params")
                .ok_or_else(|| WalError::InvalidFact("IoRequest missing 'params'".into()))?;
            Ok(Fact::IoRequest {
                id,
                cause: FactId(cause_raw),
                io_type,
                params: serde_to_tcb(params),
            })
        }
        "IoResponse" => {
            let request_id_raw = obj
                .get("request_id")
                .and_then(|r| r.as_u64())
                .ok_or_else(|| WalError::InvalidFact("IoResponse missing 'request_id'".into()))?;
            let result = obj
                .get("result")
                .ok_or_else(|| WalError::InvalidFact("IoResponse missing 'result'".into()))?;
            let error = match obj.get("error") {
                Some(serde_json::Value::Null) | None => None,
                Some(serde_json::Value::String(s)) => Some(s.clone()),
                Some(_) => {
                    return Err(WalError::InvalidFact(
                        "IoResponse 'error' must be string or null".into(),
                    ))
                }
            };
            Ok(Fact::IoResponse {
                id,
                request_id: FactId(request_id_raw),
                result: serde_to_tcb(result),
                error,
            })
        }
        "Stable" => {
            // 新格式读取 version;旧格式(≤0.3.x 含 final_snapshot)缺失时先返回 0,
            // 由调用方(read_wal_file_with_hash)以外层 version_before 兜底。
            // 旧格式的 final_snapshot 字段若存在则忽略——recover 对 Stable 仅记
            // last_stable_version 不读内容,状态重建由 StateTransition.new_payload
            // 承担(见)。
            let version = obj.get("version").and_then(|v| v.as_u64()).unwrap_or(0);
            Ok(Fact::Stable { id, version })
        }
        "Error" => {
            let message = obj
                .get("message")
                .and_then(|m| m.as_str())
                .ok_or_else(|| WalError::InvalidFact("Error missing 'message'".into()))?;
            Ok(Fact::Error {
                id,
                message: message.into(),
            })
        }
        "TransitionTrace" => {
            let cause_raw = obj
                .get("cause")
                .and_then(|c| c.as_u64())
                .ok_or_else(|| WalError::InvalidFact("TransitionTrace missing 'cause'".into()))?;
            let hits_raw = obj
                .get("rule_hits")
                .and_then(|h| h.as_array())
                .ok_or_else(|| {
                    WalError::InvalidFact("TransitionTrace missing 'rule_hits'".into())
                })?;
            let mut rule_hits = Vec::with_capacity(hits_raw.len());
            for h in hits_raw {
                let index = h.get("index").and_then(|v| v.as_u64()).ok_or_else(|| {
                    WalError::InvalidFact("TransitionTrace hit missing 'index'".into())
                })?;
                let instr_type = h
                    .get("instr_type")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        WalError::InvalidFact("TransitionTrace hit missing 'instr_type'".into())
                    })?;
                let hit = h.get("hit").and_then(|v| v.as_bool()).ok_or_else(|| {
                    WalError::InvalidFact("TransitionTrace hit missing 'hit'".into())
                })?;
                // domain_attr 可选（absent → None，兼容存量 WAL 无归因记录）；
                // 键存在但结构不合法则拒收（哈希链回放依赖忠实解析，不静默丢归因）。
                let domain_attr = match h.get("domain_attr") {
                    None => None,
                    Some(attr) => {
                        let outcome =
                            attr.get("outcome")
                                .and_then(|v| v.as_str())
                                .ok_or_else(|| {
                                    WalError::InvalidFact(
                                        "TransitionTrace domain_attr missing 'outcome'".into(),
                                    )
                                })?;
                        let missing_reason = attr
                            .get("missing_reason")
                            .map(|v| {
                                v.as_str().map(str::to_string).ok_or_else(|| {
                                    WalError::InvalidFact(
                                        "TransitionTrace domain_attr 'missing_reason' not a string"
                                            .into(),
                                    )
                                })
                            })
                            .transpose()?;
                        let on_missing = attr
                            .get("on_missing")
                            .map(|v| {
                                v.as_str().map(str::to_string).ok_or_else(|| {
                                    WalError::InvalidFact(
                                        "TransitionTrace domain_attr 'on_missing' not a string"
                                            .into(),
                                    )
                                })
                            })
                            .transpose()?;
                        Some(TraceDomainAttribution {
                            outcome: outcome.into(),
                            missing_reason,
                            on_missing,
                        })
                    }
                };
                rule_hits.push(TraceHit {
                    index,
                    instr_type: instr_type.into(),
                    hit,
                    domain_attr,
                });
            }
            Ok(Fact::TransitionTrace {
                id,
                cause: FactId(cause_raw),
                rule_hits,
            })
        }
        "Violation" => {
            let cause_raw = obj
                .get("cause")
                .and_then(|c| c.as_u64())
                .ok_or_else(|| WalError::InvalidFact("Violation missing 'cause'".into()))?;
            let rule_index = obj
                .get("rule_index")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| WalError::InvalidFact("Violation missing 'rule_index'".into()))?;
            let reason = obj
                .get("reason")
                .and_then(|v| v.as_str())
                .ok_or_else(|| WalError::InvalidFact("Violation missing 'reason'".into()))?;
            let instruction = obj
                .get("instruction")
                .ok_or_else(|| WalError::InvalidFact("Violation missing 'instruction'".into()))?;
            Ok(Fact::Violation {
                id,
                cause: FactId(cause_raw),
                rule_index,
                reason: reason.into(),
                instruction: serde_to_tcb(instruction),
            })
        }
        other => Err(WalError::InvalidFact(format!("unknown fact type: {other}"))),
    }
}

/// WAL 文件轮换策略默认值（P03）
pub const DEFAULT_MAX_WAL_SIZE_BYTES: u64 = 100 * 1024 * 1024;

/// WAL 写入器
///
/// 持有 `BufWriter<File>`，每次 `append` 后立即 `flush`，
/// 保证 write-ahead 语义（进程崩溃时磁盘已落盘）。
///
/// # fsync 支持（P02）
/// 可选的 fsync 支持，启用后在每次 flush 后执行 `sync_all()`，
/// 确保断电时数据不丢失（性能开销较大，默认禁用）。
///
/// # 文件轮换（P03）
/// 支持按大小自动轮换，单个文件超过 `max_size_bytes` 时自动创建新文件，
/// 文件名格式为 `session_N.wal`（主文件）和 `session_N.wal.1`、`session_N.wal.2` 等（轮换文件）。
pub struct WalWriter {
    writer: BufWriter<File>,
    path: PathBuf,
    max_size_bytes: u64,
    current_size_bytes: u64,
    fsync_on_flush: bool,
    file_sequence: u64,
}

impl WalWriter {
    fn build_rotated_path(path: &Path, sequence: u64) -> PathBuf {
        if sequence == 0 {
            path.to_path_buf()
        } else {
            let mut p = path.to_path_buf();
            let ext = p
                .extension()
                .map(|e| e.to_string_lossy().to_string())
                .unwrap_or_default();
            let stem = p
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if ext.is_empty() {
                p.set_file_name(format!("{}.{}", stem, sequence));
            } else {
                p.set_file_name(format!("{}.{}.{}", stem, sequence, ext));
            }
            p
        }
    }

    fn open_file(path: &Path, sequence: u64, truncate: bool) -> Result<(File, PathBuf), WalError> {
        let rotated_path = Self::build_rotated_path(path, sequence);
        let mut options = OpenOptions::new();
        options.create(true).write(true);
        if truncate {
            options.truncate(true);
        } else {
            options.append(true);
        }
        let file = options.open(&rotated_path)?;
        Ok((file, rotated_path))
    }

    /// 创建新 WAL 文件（truncate 已有文件）
    ///
    /// 用于 `FactsLog::with_wal` 全新启动场景。
    pub fn create<P: AsRef<Path>>(path: P) -> Result<Self, WalError> {
        Self::create_with_options(path, DEFAULT_MAX_WAL_SIZE_BYTES, false)
    }

    /// 创建新 WAL 文件并指定 fsync 选项（P02）
    ///
    /// # 参数
    /// - `path`: WAL 文件路径
    /// - `fsync`: 是否在每次 flush 后执行 fsync
    pub fn create_with_fsync<P: AsRef<Path>>(path: P, fsync: bool) -> Result<Self, WalError> {
        Self::create_with_options(path, DEFAULT_MAX_WAL_SIZE_BYTES, fsync)
    }

    /// 创建新 WAL 文件并指定轮换和 fsync 选项（P03）
    ///
    /// # 参数
    /// - `path`: WAL 文件路径
    /// - `max_size_bytes`: 单个文件最大大小，达到后自动轮换（0 表示不轮换）
    /// - `fsync`: 是否在每次 flush 后执行 fsync
    pub fn create_with_options<P: AsRef<Path>>(
        path: P,
        max_size_bytes: u64,
        fsync: bool,
    ) -> Result<Self, WalError> {
        let path_buf = path.as_ref().to_path_buf();
        let (file, _) = Self::open_file(&path_buf, 0, true)?;
        Ok(Self {
            writer: BufWriter::new(file),
            path: path_buf,
            max_size_bytes,
            current_size_bytes: 0,
            fsync_on_flush: fsync,
            file_sequence: 0,
        })
    }

    /// 以追加模式打开 WAL 文件
    ///
    /// 用于 `FactsLog::recover` 后继续写入：文件已存在则追加，
    /// 不存在则创建（recover 空文件场景）。
    pub fn append<P: AsRef<Path>>(path: P) -> Result<Self, WalError> {
        Self::append_with_options(path, DEFAULT_MAX_WAL_SIZE_BYTES, false)
    }

    /// 以追加模式打开 WAL 文件并指定 fsync 选项（P02）
    ///
    /// # 参数
    /// - `path`: WAL 文件路径
    /// - `fsync`: 是否在每次 flush 后执行 fsync
    pub fn append_with_fsync<P: AsRef<Path>>(path: P, fsync: bool) -> Result<Self, WalError> {
        Self::append_with_options(path, DEFAULT_MAX_WAL_SIZE_BYTES, fsync)
    }

    /// 以追加模式打开 WAL 文件并指定轮换和 fsync 选项（P03）
    ///
    /// # 参数
    /// - `path`: WAL 文件路径
    /// - `max_size_bytes`: 单个文件最大大小，达到后自动轮换（0 表示不轮换）
    /// - `fsync`: 是否在每次 flush 后执行 fsync
    pub fn append_with_options<P: AsRef<Path>>(
        path: P,
        max_size_bytes: u64,
        fsync: bool,
    ) -> Result<Self, WalError> {
        let path_buf = path.as_ref().to_path_buf();

        let mut sequence = 0;
        loop {
            let rotated_path = Self::build_rotated_path(&path_buf, sequence);
            if !rotated_path.exists() {
                sequence = sequence.saturating_sub(1);
                break;
            }
            sequence += 1;
        }

        let (file, _) = Self::open_file(&path_buf, sequence, false)?;

        let current_size = if sequence == 0 && path_buf.exists() {
            std::fs::metadata(&path_buf).map(|m| m.len()).unwrap_or(0)
        } else {
            0
        };

        Ok(Self {
            writer: BufWriter::new(file),
            path: path_buf,
            max_size_bytes,
            current_size_bytes: current_size,
            fsync_on_flush: fsync,
            file_sequence: sequence,
        })
    }

    /// 轮换到新分片文件（干预原语 wal.force_rotate 通路）
    ///
    /// 纯追加式切换：序号 +1 并打开新文件，旧分片保持完整——崩溃安全，
    /// 不影响链内容与可回放性（recover 按序读全部分片）。
    pub fn rotate(&mut self) -> Result<(), WalError> {
        self.file_sequence += 1;
        let (file, _) = Self::open_file(&self.path, self.file_sequence, true)?;
        self.writer = BufWriter::new(file);
        self.current_size_bytes = 0;
        Ok(())
    }

    /// 追加一条记录：`{"version_before": N, "fact": {...}}`
    ///
    /// 调用后立即 `flush`，若启用 fsync 则额外执行 `sync_all()`，
    /// 保证进程崩溃和断电时数据不丢失。
    ///
    /// 若启用文件轮换（`max_size_bytes > 0`），当当前文件大小超过阈值时自动创建新文件。
    pub fn append_record(&mut self, version_before: u64, fact: &Fact) -> Result<(), WalError> {
        let mut record = serde_json::Map::new();
        record.insert(
            "version_before".into(),
            serde_json::Value::Number(version_before.into()),
        );
        record.insert("fact".into(), fact_to_json(fact));
        let line = serde_json::to_string(&serde_json::Value::Object(record))?;

        let line_bytes = line.len() as u64 + 1;

        if self.max_size_bytes > 0
            && self.current_size_bytes > 0
            && self.current_size_bytes + line_bytes > self.max_size_bytes
        {
            self.rotate()?;
        }

        writeln!(self.writer, "{line}")?;
        self.writer.flush()?;

        if self.fsync_on_flush {
            self.writer.get_mut().sync_all()?;
        }

        self.current_size_bytes += line_bytes;

        Ok(())
    }

    /// 追加一条带哈希链的记录（两套 WAL 合并：新格式 v2）
    ///
    /// 写入格式：
    /// ```json
    /// {"version_before": N, "fact": {...},
    ///  "content_hash": "...", "prev_hash": "...", "chain_hash": "..."}
    /// ```
    ///
    /// # 参数
    /// - `version_before`: 追加前的版本号
    /// - `fact`: 事实内容
    /// - `content_hash`: Fact 的 BLAKE3 哈希
    /// - `prev_hash`: 前一条记录的链哈希（首条为 `"genesis"`）
    /// - `chain_hash`: 当前链哈希 `blake3(prev_hash + content_hash)`
    pub fn append_record_with_hash(
        &mut self,
        version_before: u64,
        fact: &Fact,
        content_hash: &str,
        prev_hash: &str,
        chain_hash: &str,
    ) -> Result<(), WalError> {
        let mut record = serde_json::Map::new();
        record.insert(
            "version_before".into(),
            serde_json::Value::Number(version_before.into()),
        );
        record.insert("fact".into(), fact_to_json(fact));
        record.insert(
            "content_hash".into(),
            serde_json::Value::String(content_hash.into()),
        );
        record.insert(
            "prev_hash".into(),
            serde_json::Value::String(prev_hash.into()),
        );
        record.insert(
            "chain_hash".into(),
            serde_json::Value::String(chain_hash.into()),
        );
        let line = serde_json::to_string(&serde_json::Value::Object(record))?;

        let line_bytes = line.len() as u64 + 1;

        if self.max_size_bytes > 0
            && self.current_size_bytes > 0
            && self.current_size_bytes + line_bytes > self.max_size_bytes
        {
            self.rotate()?;
        }

        writeln!(self.writer, "{line}")?;
        self.writer.flush()?;

        if self.fsync_on_flush {
            self.writer.get_mut().sync_all()?;
        }

        self.current_size_bytes += line_bytes;

        Ok(())
    }
}

fn read_wal_file_with_hash<P: AsRef<Path>>(
    path: P,
    base_line_no: usize,
) -> Result<Vec<WalRecord>, WalError> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut records = Vec::new();
    for (line_idx, line) in reader.lines().enumerate() {
        let line_no = base_line_no + line_idx;
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(&line)
            .map_err(|e| WalError::InvalidFact(format!("line {line_no}: JSON parse error: {e}")))?;
        let obj = value
            .as_object()
            .ok_or_else(|| WalError::InvalidFact(format!("line {line_no}: not an object")))?;
        let version_before = obj
            .get("version_before")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| {
                WalError::InvalidFact(format!("line {line_no}: missing version_before"))
            })?;
        let fact_value = obj
            .get("fact")
            .ok_or_else(|| WalError::InvalidFact(format!("line {line_no}: missing fact")))?;
        let fact = fact_from_json(fact_value)?;
        // 旧格式容错():≤0.3.x 的 Stable 无 version 字段,
        // 以外层 version_before 兜底(状态重建不读此字段,仅供审计/展示)
        let fact = match fact {
            Fact::Stable { id, version: 0 } if fact_value.get("version").is_none() => {
                Fact::Stable {
                    id,
                    version: version_before,
                }
            }
            f => f,
        };

        // 可选哈希字段（新格式有，旧格式无）
        let content_hash = obj
            .get("content_hash")
            .and_then(|v| v.as_str())
            .map(String::from);
        let prev_hash = obj
            .get("prev_hash")
            .and_then(|v| v.as_str())
            .map(String::from);
        let chain_hash = obj
            .get("chain_hash")
            .and_then(|v| v.as_str())
            .map(String::from);

        records.push(WalRecord {
            version_before,
            fact,
            content_hash,
            prev_hash,
            chain_hash,
        });
    }
    Ok(records)
}

fn read_wal_file<P: AsRef<Path>>(
    path: P,
    base_line_no: usize,
) -> Result<Vec<(u64, Fact)>, WalError> {
    let records = read_wal_file_with_hash(path, base_line_no)?;
    Ok(records
        .into_iter()
        .map(|r| (r.version_before, r.fact))
        .collect())
}

/// 读取 WAL 文件，返回所有 (version_before, Fact) 记录
///
/// 支持单文件和多文件轮换格式（P03）：
/// - 单文件：`session_N.wal`
/// - 多文件：`session_N.wal`、`session_N.wal.1`、`session_N.wal.2` 等
///
/// 用于 `FactsLog::recover`：读取 → 重放事实（不写 WAL）→ 挂载 WAL 继续追加。
///
/// # 向后兼容性（P03）
/// 完全兼容旧版单文件 WAL：
/// - 自动检测：读取时先检查主文件（无序列号后缀）是否存在
/// - 顺序读取：按序列号从小到大依次读取轮换文件
/// - 无缝升级：启用轮换后，新建的 WAL 会自动使用轮换策略，旧 WAL 文件不受影响
/// - 降级支持：若只存在主文件（旧版格式），行为与旧版完全一致
///
/// # 错误处理
/// - 文件不存在 → `WalError::Io`
/// - 任意行 JSON 解析失败 → `WalError::InvalidFact`（带行号）
/// - 空行自动跳过
pub fn read_wal<P: AsRef<Path>>(path: P) -> Result<Vec<(u64, Fact)>, WalError> {
    let path_buf = path.as_ref().to_path_buf();
    let mut records = Vec::new();
    let mut line_no = 0;
    let mut found_any_file = false;

    if path_buf.exists() {
        records.extend(read_wal_file(&path_buf, line_no)?);
        line_no += std::fs::read_to_string(&path_buf)
            .map(|s| s.lines().count())
            .unwrap_or(0);
        found_any_file = true;
    }

    let mut sequence = 1;
    loop {
        let rotated_path = {
            let mut p = path_buf.clone();
            let ext = p
                .extension()
                .map(|e| e.to_string_lossy().to_string())
                .unwrap_or_default();
            let stem = p
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if ext.is_empty() {
                p.set_file_name(format!("{}.{}", stem, sequence));
            } else {
                p.set_file_name(format!("{}.{}.{}", stem, sequence, ext));
            }
            p
        };

        if !rotated_path.exists() {
            break;
        }

        records.extend(read_wal_file(&rotated_path, line_no)?);
        line_no += std::fs::read_to_string(&rotated_path)
            .map(|s| s.lines().count())
            .unwrap_or(0);
        found_any_file = true;
        sequence += 1;
    }

    if !found_any_file {
        return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "WAL file not found").into());
    }

    Ok(records)
}

/// 读取 WAL 文件，返回所有 `WalRecord`（带可选哈希字段）
///
/// 两套 WAL 合并后的统一读取函数。
/// 自动识别新旧格式：
/// - 有 `chain_hash` 字段 → 新格式，`content_hash`/`prev_hash`/`chain_hash` 有值
/// - 无 `chain_hash` 字段 → 旧格式，`content_hash`/`prev_hash`/`chain_hash` 为 `None`
///
/// # 参数
/// - `path`: WAL 文件路径（支持轮换文件）
///
/// # 返回值
/// - `Ok(Vec<WalRecord>)`: 所有记录（带可选哈希字段）
/// - `Err(WalError)`: 文件不存在或解析失败
///
/// # 用途
/// - CLI `verify-chain` 命令：验证哈希链完整性
/// - tier2 `Auditor::load_from_wal_verified`：恢复时验证审计链
/// - `FactsLog::recover`：重放事实恢复状态
pub fn read_wal_with_hash<P: AsRef<Path>>(path: P) -> Result<Vec<WalRecord>, WalError> {
    let path_buf = path.as_ref().to_path_buf();
    let mut records = Vec::new();
    let mut line_no = 0;
    let mut found_any_file = false;

    if path_buf.exists() {
        records.extend(read_wal_file_with_hash(&path_buf, line_no)?);
        line_no += std::fs::read_to_string(&path_buf)
            .map(|s| s.lines().count())
            .unwrap_or(0);
        found_any_file = true;
    }

    let mut sequence = 1;
    loop {
        let rotated_path = {
            let mut p = path_buf.clone();
            let ext = p
                .extension()
                .map(|e| e.to_string_lossy().to_string())
                .unwrap_or_default();
            let stem = p
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if ext.is_empty() {
                p.set_file_name(format!("{}.{}", stem, sequence));
            } else {
                p.set_file_name(format!("{}.{}.{}", stem, sequence, ext));
            }
            p
        };

        if !rotated_path.exists() {
            break;
        }

        records.extend(read_wal_file_with_hash(&rotated_path, line_no)?);
        line_no += std::fs::read_to_string(&rotated_path)
            .map(|s| s.lines().count())
            .unwrap_or(0);
        found_any_file = true;
        sequence += 1;
    }

    if !found_any_file {
        return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "WAL file not found").into());
    }

    Ok(records)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::panic, clippy::expect_used)]
    use super::*;
    use crate::fact::{Fact, FactId, IoType};
    use evorule_tcb::JsonValue;

    // === tcb_to_serde / serde_to_tcb 单元测试 ===

    #[test]
    fn test_tcb_serde_null_roundtrip() {
        let v = JsonValue::Null;
        let s = tcb_to_serde(&v);
        assert_eq!(s, serde_json::Value::Null);
        assert_eq!(serde_to_tcb(&s), v);
    }

    #[test]
    fn test_tcb_serde_bool_roundtrip() {
        for b in [true, false] {
            let v = JsonValue::Bool(b);
            let s = tcb_to_serde(&v);
            assert_eq!(s, serde_json::Value::Bool(b));
            assert_eq!(serde_to_tcb(&s), v);
        }
    }

    #[test]
    fn test_tcb_serde_integer_roundtrip() {
        for i in [0i64, 1, -1, i64::MAX, i64::MIN, 42, -99] {
            let v = JsonValue::Integer(i);
            let s = tcb_to_serde(&v);
            assert_eq!(s.as_i64(), Some(i));
            assert_eq!(serde_to_tcb(&s), v);
        }
    }

    #[test]
    fn test_tcb_serde_string_roundtrip() {
        let v = JsonValue::String("hello 世界".into());
        let s = tcb_to_serde(&v);
        assert_eq!(s, serde_json::Value::String("hello 世界".into()));
        assert_eq!(serde_to_tcb(&s), v);
    }

    #[test]
    fn test_tcb_serde_array_roundtrip() {
        let v = JsonValue::Array(vec![
            JsonValue::Null,
            JsonValue::Integer(1),
            JsonValue::String("x".into()),
        ]);
        let s = tcb_to_serde(&v);
        assert!(s.is_array());
        assert_eq!(serde_to_tcb(&s), v);
    }

    #[test]
    fn test_tcb_serde_object_roundtrip() {
        let v = JsonValue::object_from_pairs(&[
            ("a", JsonValue::Integer(1)),
            ("b", JsonValue::String("y".into())),
            ("c", JsonValue::Array(vec![JsonValue::Bool(true)])),
        ]);
        let s = tcb_to_serde(&v);
        assert!(s.is_object());
        assert_eq!(serde_to_tcb(&s), v);
    }

    #[test]
    fn test_serde_to_tcb_float_becomes_string() {
        // tier0 无 Float，浮点数应转为字符串保留精度
        let s: serde_json::Value = serde_json::from_str("3.14").unwrap();
        let tcb = serde_to_tcb(&s);
        match tcb {
            JsonValue::String(ref _str) => {}
            ref other => panic!("expected String for float, got {other:?}"),
        }
    }

    #[test]
    fn test_tcb_serde_u64_roundtrip() {
        // u64 全域通道（v0.9.0）：超 i64 范围值正程/回程双向无损。
        // （边界）serde_json Number 无 tag：i64 范围内的 U64 经 serde 往返后
        // as_i64 命中退化为 Integer（数值无损、哈希面不经 tcb 不受累），
        // U64 形态保形承诺仅覆盖超 i64 范围值。
        for u in [u64::MAX, u64::MAX - 3, i64::MAX as u64 + 1] {
            let v = JsonValue::U64(u);
            let s = tcb_to_serde(&v);
            assert_eq!(s.as_u64(), Some(u));
            assert_eq!(serde_to_tcb(&s), v);
        }
    }

    #[test]
    fn test_serde_to_tcb_u64_overflow_keeps_u64() {
        // u64 超 i64 范围走 U64 变体无损承载（转字符串会破坏 WAL 往返保形）
        let s: serde_json::Value = serde_json::from_str(&format!("{}", u64::MAX)).unwrap();
        let tcb = serde_to_tcb(&s);
        assert_eq!(tcb, JsonValue::U64(u64::MAX));
    }

    #[test]
    fn test_payload_update_u64_value_wal_roundtrip_hash_stable() {
        // WAL 展示面往返保形：U64 值经 fact_to_json → fact_from_json 后形态
        // 与哈希不变（超 i64 范围 u64 场景的回放确定性）
        let fact = Fact::PayloadUpdate {
            id: FactId(42),
            path: "__ruleset__.anchor".into(),
            value: JsonValue::U64(u64::MAX - 3),
        };
        let json = fact_to_json(&fact);
        let restored = fact_from_json(&json).expect("roundtrip should succeed");
        assert_eq!(restored, fact);
        // 回放重算 content_hash 与落链值一致（哈希输入 fact_to_stable_json）
        let h1 = crate::fact_hash(&fact).expect("fact_hash should succeed");
        let h2 = crate::fact_hash(&restored).expect("fact_hash should succeed");
        assert_eq!(h1, h2);
    }

    // === fact_to_json / fact_from_json 7 种变体往返测试 ===

    fn assert_fact_roundtrip(fact: &Fact) {
        let json = fact_to_json(fact);
        let restored = fact_from_json(&json).expect("roundtrip should succeed");
        assert_eq!(restored, *fact);
    }

    #[test]
    fn test_fact_command_roundtrip() {
        let fact = Fact::Command {
            id: FactId(1),
            instruction: JsonValue::object_from_pairs(&[
                ("type", JsonValue::String("increment".into())),
                (
                    "params",
                    JsonValue::object_from_pairs(&[("x", JsonValue::Integer(5))]),
                ),
            ]),
        };
        assert_fact_roundtrip(&fact);
    }

    #[test]
    fn test_fact_payload_update_roundtrip() {
        let fact = Fact::PayloadUpdate {
            id: FactId(7),
            path: "user.profile.name".into(),
            value: JsonValue::String("alice".into()),
        };
        assert_fact_roundtrip(&fact);
    }

    #[test]
    fn test_fact_state_transition_roundtrip() {
        let fact = Fact::StateTransition {
            id: FactId(2),
            cause: FactId(1),
            new_payload: JsonValue::object_from_pairs(&[("x", JsonValue::Integer(42))]),
            new_queue: vec![JsonValue::String("instr1".into()), JsonValue::Integer(99)],
        };
        assert_fact_roundtrip(&fact);
    }

    #[test]
    fn test_fact_io_request_roundtrip() {
        for io_type in [
            IoType::call_external(),
            IoType::query_db(),
            IoType::http_get(),
            IoType::save_memory(),
            IoType::call_service(),
        ] {
            let fact = Fact::IoRequest {
                id: FactId(3),
                cause: FactId(2),
                io_type,
                params: JsonValue::object_from_pairs(&[("prompt", JsonValue::String("hi".into()))]),
            };
            assert_fact_roundtrip(&fact);
        }
    }

    #[test]
    fn test_fact_io_response_success_roundtrip() {
        let fact = Fact::IoResponse {
            id: FactId(4),
            request_id: FactId(3),
            result: JsonValue::String("llm reply".into()),
            error: None,
        };
        assert_fact_roundtrip(&fact);
    }

    #[test]
    fn test_fact_io_response_error_roundtrip() {
        let fact = Fact::IoResponse {
            id: FactId(5),
            request_id: FactId(3),
            result: JsonValue::Null,
            error: Some("timeout after 30s".into()),
        };
        assert_fact_roundtrip(&fact);
    }

    #[test]
    fn test_fact_stable_roundtrip() {
        let fact = Fact::Stable {
            id: FactId(6),
            version: 7,
        };
        assert_fact_roundtrip(&fact);
    }

    #[test]
    fn test_fact_error_roundtrip() {
        let fact = Fact::Error {
            id: FactId(8),
            message: "max rounds exceeded".into(),
        };
        assert_fact_roundtrip(&fact);
    }

    #[test]
    fn test_fact_violation_roundtrip() {
        // 回归验证：Violation 事实（enforce 拦截记录）WAL 往返
        let fact = Fact::Violation {
            id: FactId(10),
            cause: FactId(3),
            rule_index: 2,
            reason: "违规：禁删数据集".into(),
            instruction: JsonValue::object_from_pairs(&[
                ("type", JsonValue::String("delete_all".into())),
                (
                    "params",
                    JsonValue::object_from_pairs(&[("scope", JsonValue::String("all".into()))]),
                ),
            ]),
        };
        assert_fact_roundtrip(&fact);
    }

    #[test]
    fn test_fact_transition_trace_without_attr_roundtrip() {
        // 无归因（存量形态）：domain_attr 为 None，WAL 往返后仍为 None
        let fact = Fact::TransitionTrace {
            id: FactId(11),
            cause: FactId(2),
            rule_hits: vec![
                TraceHit {
                    index: 0,
                    instr_type: "set".into(),
                    hit: true,
                    domain_attr: None,
                },
                TraceHit {
                    index: 1,
                    instr_type: "branch".into(),
                    hit: false,
                    domain_attr: None,
                },
            ],
        };
        assert_fact_roundtrip(&fact);
    }

    #[test]
    fn test_fact_transition_trace_with_attr_roundtrip() {
        // 三态归因落账：missing 分类 + on_missing 声明值随 WAL 往返保真（专项-20261001）
        let fact = Fact::TransitionTrace {
            id: FactId(12),
            cause: FactId(2),
            rule_hits: vec![TraceHit {
                index: 1,
                instr_type: "branch".into(),
                hit: false,
                domain_attr: Some(TraceDomainAttribution {
                    outcome: "missing".into(),
                    missing_reason: Some("path_not_found".into()),
                    on_missing: Some("unsat".into()),
                }),
            }],
        };
        assert_fact_roundtrip(&fact);
    }

    #[test]
    fn test_transition_trace_legacy_without_domain_attr_key() {
        // 存量 WAL 兼容：无 domain_attr 键的记录解析为 None（absent → None）
        let json = serde_json::json!({
            "type": "TransitionTrace",
            "id": 5,
            "cause": 2,
            "rule_hits": [{"index": 0, "instr_type": "branch", "hit": false}]
        });
        let restored = fact_from_json(&json).expect("旧格式 TransitionTrace 必须可解析");
        match restored {
            Fact::TransitionTrace { rule_hits, .. } => {
                assert_eq!(rule_hits.len(), 1);
                assert!(
                    rule_hits
                        .first()
                        .and_then(|h| h.domain_attr.as_ref())
                        .is_none(),
                    "absent domain_attr 必须解析为 None"
                );
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn test_transition_trace_malformed_domain_attr_rejected() {
        // 不静默丢归因：domain_attr 键存在但缺 outcome → 拒收（fail-fast）
        let json = serde_json::json!({
            "type": "TransitionTrace",
            "id": 5,
            "cause": 2,
            "rule_hits": [{
                "index": 0,
                "instr_type": "branch",
                "hit": false,
                "domain_attr": {"missing_reason": "path_not_found"}
            }]
        });
        let result = fact_from_json(&json);
        assert!(
            matches!(result, Err(WalError::InvalidFact(_))),
            "结构不合法的 domain_attr 必须拒收，不得静默丢归因"
        );
    }

    #[test]
    fn test_fact_from_json_unknown_type() {
        let json = serde_json::json!({"type": "Unknown", "id": 1});
        let result = fact_from_json(&json);
        assert!(matches!(result, Err(WalError::InvalidFact(_))));
    }

    #[test]
    fn test_fact_from_json_missing_type() {
        let json = serde_json::json!({"id": 1});
        let result = fact_from_json(&json);
        assert!(matches!(result, Err(WalError::InvalidFact(_))));
    }

    // === u64 全域 FactId 哨兵解析专项（G6 规则集锚定事实回归）===

    /// G6 规则集锚定事实以 u64::MAX-1 哨兵 id 落链（session.rs
    /// `__ruleset__.version`）。此前 as_i64 解析对 u64 高位返回 None，
    /// WAL 回读报「missing/invalid 'id' field」并连锁拒绝复活——
    /// u64 全域 id/cause/request_id 必须可往返。
    #[test]
    fn test_fact_sentinel_id_roundtrip() {
        let fact = Fact::PayloadUpdate {
            id: FactId(u64::MAX - 1),
            path: "__ruleset__.version".into(),
            value: JsonValue::String("c7b6fab66f0ea645".into()),
        };
        assert_fact_roundtrip(&fact);
    }

    #[test]
    fn test_read_wal_sentinel_id_end_to_end() {
        // 真机 319B 引导 WAL 形态：单条哨兵 id PayloadUpdate 记录
        let path = temp_wal_path("sentinel_id");
        let mut w = WalWriter::create(&path).unwrap();
        w.append_record(
            0,
            &Fact::PayloadUpdate {
                id: FactId(u64::MAX - 1),
                path: "__ruleset__.version".into(),
                value: JsonValue::String("c7b6fab66f0ea645".into()),
            },
        )
        .unwrap();
        drop(w);

        let records = read_wal(&path).expect("哨兵 id WAL 必须可读（不得报 InvalidFact）");
        assert_eq!(records.len(), 1);
        match &records[0].1 {
            Fact::PayloadUpdate { id, .. } => assert_eq!(id.0, u64::MAX - 1),
            other => panic!("expected PayloadUpdate, got {other:?}"),
        }

        let _ = std::fs::remove_file(&path);
    }

    // === 旧格式 WAL(≤0.3.x 含 final_snapshot)兼容专项 ===

    /// 旧格式(≤0.3.x)Stable 事实内嵌 final_snapshot 全量快照、无 version 字段。
    /// 新代码读旧 WAL: 忽略 final_snapshot,version 以外层 version_before 兜底。
    /// 验收门禁 4.3 第 7 项()。
    #[test]
    fn test_read_legacy_wal_stable_with_final_snapshot() {
        let path = temp_wal_path("legacy_stable");
        let lines = [
            r#"{"version_before":0,"fact":{"type":"Command","id":1,"instruction":{"type":"increment","params":{"attr":"x","delta":1}}}}"#,
            r#"{"version_before":0,"fact":{"type":"StateTransition","id":2,"cause":1,"new_payload":{"x":1},"new_queue":[]}}"#,
            r#"{"version_before":1,"fact":{"type":"Stable","id":3,"final_snapshot":{"x":1}}}"#,
        ];
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();

        let records = read_wal(&path).expect("旧格式 WAL(含 final_snapshot)必须可读");
        assert_eq!(records.len(), 3);

        match records.get(2).map(|(_, f)| f) {
            Some(Fact::Stable { id, version }) => {
                assert_eq!(id.0, 3);
                assert_eq!(
                    *version, 1,
                    "旧格式缺失 version 字段时应以外层 version_before 兜底"
                );
            }
            other => panic!("expected Stable, got {other:?}"),
        }

        let _ = std::fs::remove_file(&path);
    }

    /// 新格式 Stable 含显式 version 字段时,不得被外层 version_before 覆盖
    /// (与新写入的 WAL 往返一致性,防兜底逻辑误伤新格式)。
    #[test]
    fn test_read_new_wal_stable_version_preserved() {
        let path = temp_wal_path("new_stable");
        let lines = [r#"{"version_before":1,"fact":{"type":"Stable","id":3,"version":7}}"#];
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();

        let records = read_wal(&path).expect("新格式 WAL 必须可读");
        match records.first().map(|(_, f)| f) {
            Some(Fact::Stable { id, version }) => {
                assert_eq!(id.0, 3);
                assert_eq!(*version, 7, "新格式显式 version 不得被 version_before 覆盖");
            }
            other => panic!("expected Stable, got {other:?}"),
        }

        let _ = std::fs::remove_file(&path);
    }

    // === WalWriter + read_wal 集成测试 ===

    fn temp_wal_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "evorule_wal_test_{name}_{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn test_wal_create_append_read_roundtrip() {
        let path = temp_wal_path("roundtrip");
        let mut writer = WalWriter::create(&path).unwrap();
        let facts = vec![
            (
                0u64,
                Fact::Command {
                    id: FactId(1),
                    instruction: JsonValue::object_from_pairs(&[(
                        "type",
                        JsonValue::String("increment".into()),
                    )]),
                },
            ),
            (
                0u64,
                Fact::StateTransition {
                    id: FactId(2),
                    cause: FactId(1),
                    new_payload: JsonValue::object_from_pairs(&[("x", JsonValue::Integer(5))]),
                    new_queue: vec![],
                },
            ),
            (
                1u64,
                Fact::Stable {
                    id: FactId(3),
                    version: 1,
                },
            ),
        ];
        for (vb, f) in &facts {
            writer.append_record(*vb, f).unwrap();
        }
        drop(writer);

        let records = read_wal(&path).unwrap();
        assert_eq!(records.len(), facts.len());
        for (i, ((vb_expected, f_expected), (vb_actual, f_actual))) in
            facts.iter().zip(records.iter()).enumerate()
        {
            assert_eq!(vb_actual, vb_expected, "version_before mismatch at {i}");
            assert_eq!(f_actual, f_expected, "fact mismatch at {i}");
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_wal_append_mode_continues_existing_file() {
        let path = temp_wal_path("append_mode");

        // 第一次：create + 写 2 条
        let mut w1 = WalWriter::create(&path).unwrap();
        w1.append_record(
            0,
            &Fact::Command {
                id: FactId(1),
                instruction: JsonValue::empty_object(),
            },
        )
        .unwrap();
        w1.append_record(
            0,
            &Fact::Stable {
                id: FactId(2),
                version: 1,
            },
        )
        .unwrap();
        drop(w1);

        // 第二次：append 模式继续写 1 条
        let mut w2 = WalWriter::append(&path).unwrap();
        w2.append_record(
            0,
            &Fact::Error {
                id: FactId(3),
                message: "test".into(),
            },
        )
        .unwrap();
        drop(w2);

        // 读取应得 3 条
        let records = read_wal(&path).unwrap();
        assert_eq!(records.len(), 3);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_read_wal_skips_blank_lines() {
        let path = temp_wal_path("blank_lines");
        let mut w = WalWriter::create(&path).unwrap();
        w.append_record(
            0,
            &Fact::Command {
                id: FactId(1),
                instruction: JsonValue::empty_object(),
            },
        )
        .unwrap();
        drop(w);

        // 手动追加空行
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(f).unwrap();
        writeln!(f, "   ").unwrap();
        drop(f);

        let records = read_wal(&path).unwrap();
        assert_eq!(records.len(), 1, "blank lines should be skipped");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_read_wal_nonexistent_file_returns_io_error() {
        let path = temp_wal_path("nonexistent");
        let result = read_wal(&path);
        assert!(matches!(result, Err(WalError::Io(_))));
    }

    #[test]
    fn test_wal_error_display() {
        let e = WalError::InvalidFact("bad fact".into());
        assert!(format!("{e}").contains("bad fact"));

        let e = WalError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "missing"));
        assert!(format!("{e}").contains("missing"));
    }
}

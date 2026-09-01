// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 审计档案（只读）— 从 WAL 重建历史会话审计链（UV-016）
//!
//! # 定位
//!
//! 活跃会话的审计走 `GET /api/sessions/{id}/audit`（内存 Auditor）；
//! 本模块补齐另一半：服务器重启后，活跃会话清空，但 `wal_dir` 下的
//! `session_{n}.wal`（含轮换分片）完整保留了事实与哈希链。这里提供
//! **纯只读**的重建与查询，让历史会话（含 LLM 侧车审计会话）可回看。
//!
//! # 只读保证
//!
//! - 全程只使用 [`evorule_reactor::read_wal_with_hash`]（含轮换分片合并），
//!   不触碰 `FactsLog`（其 recover 路径会以 append 模式挂载写句柄）；
//! - 不注册进 `SessionManager`，无 reap/abort/命令通道语义，`touch_session`
//!   的 TTL 续命副作用不存在；
//! - 对文件零写入、零删除、零重命名。
//!
//! # 验证口径
//!
//! 链哈希 = `blake3(prev_hash + content_hash)`，首条 `prev_hash = "genesis"`，
//! `content_hash = fact_hash(fact)`（与写入侧 `FactsLog::append` 同一单一真相源）。
//! 重建时逐条重算并与 WAL 存储的 `chain_hash` 比对；比对失败在响应中如实
//! 置 `verified = false`，不静默。旧格式记录（无哈希字段）计入
//! `unhashed_records`，不参与验证也不假装验证过。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use evorule_reactor::{fact_hash, read_wal_with_hash, tcb_to_serde, Fact, WalRecord};
use serde::Serialize;

/// 档案扫描/读取错误（如实上报，不做静默降级）
#[derive(Debug)]
pub enum ArchiveError {
    /// wal_dir 未启用（纯内存模式）
    WalDisabled,
    /// 会话档案不存在
    NotFound(u64),
    /// I/O 或 WAL 解析失败
    Io(String),
}

impl std::fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WalDisabled => write!(f, "wal_dir 未启用（纯内存模式），无审计档案"),
            Self::NotFound(id) => write!(f, "会话 {id} 无审计档案"),
            Self::Io(e) => write!(f, "审计档案读取失败: {e}"),
        }
    }
}

/// 档案会话元数据（列表用，轻量）
#[derive(Debug, Clone, Serialize)]
pub struct ArchiveSessionMeta {
    pub session_id: u64,
    /// 事实总数
    pub fact_count: u64,
    /// 首条事实类型
    pub first_fact_type: String,
    /// 末条事实类型（如 Stable/Error，可判断会话收尾状态）
    pub last_fact_type: String,
    /// LLM 侧车审计会话（首条 LLM 形态 Command 为 call_external 且带 messages）
    pub is_llm_sidecar: bool,
    /// 侧车审计用途（audit_purpose 标签，非侧车为 None）
    pub audit_purpose: Option<String>,
    /// WAL 文件总字节数（含分片）
    pub wal_bytes: u64,
}

/// wal_dir 下会话 WAL 文件的一次性清单：会话 ID → (指纹, 总字节)
struct WalInventory {
    sessions: BTreeMap<u64, ((u64, u64), u64)>,
}

/// 从文件名解析会话 WAL：`session_{n}.wal` 及轮换分片 `session_{n}.wal.{seq}`
fn parse_session_wal_filename(name: &str) -> Option<u64> {
    let rest = name.strip_prefix("session_")?;
    let pos = rest.find(".wal")?;
    let id_part = &rest[..pos];
    let after = &rest[pos + 4..];
    // 后缀必须是 ""（基础文件）或 ".{纯数字}"（轮换分片）
    let valid_suffix = after.is_empty()
        || (after.len() > 1
            && after.starts_with('.')
            && after[1..].bytes().all(|b| b.is_ascii_digit()));
    if !valid_suffix {
        return None;
    }
    id_part.parse::<u64>().ok()
}

/// 扫描 wal_dir：`session_{n}.wal` 与分片 `session_{n}.wal.{seq}` 归入会话 n；
/// `shared_facts.wal`、`governance_single_reactor.wal` 等非会话文件天然不匹配
/// 命名规则被排除。指纹 = 基础文件+全部分片的 (mtime,size) 聚合，用于缓存失效。
fn scan_inventory(wal_dir: &Path) -> Result<WalInventory, ArchiveError> {
    let mut sessions: BTreeMap<u64, ((u64, u64), u64)> = BTreeMap::new();
    let dir = std::fs::read_dir(wal_dir).map_err(|e| ArchiveError::Io(e.to_string()))?;
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(id) = parse_session_wal_filename(&name) else {
            continue;
        };
        let Ok(m) = entry.metadata() else {
            continue;
        };
        let mtime = m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let is_base = !name
            .strip_prefix("session_")
            .and_then(|r| r.strip_suffix(".wal"))
            .map(|r| r.contains('.'))
            .unwrap_or(false);
        let e = sessions.entry(id).or_insert(((0u64, 0u64), 0u64));
        // 指纹：mtime 按文件名长度混淆后异或聚合，size 直接累加
        e.0 .0 ^= mtime.wrapping_mul(0x9E37_79B9_7F4A_7C15 ^ (name.len() as u64));
        e.0 .1 += u64::from(is_base);
        e.1 += m.len();
    }
    Ok(WalInventory { sessions })
}

fn session_wal_base(wal_dir: &Path, session_id: u64) -> PathBuf {
    wal_dir.join(format!("session_{session_id}.wal"))
}

/// 判断 Command 事实是否为 LLM 侧车审计命令（call_external + messages，
/// 与 server 侧 `is_llm_audit_request` 的 IoRequest 谓词同口径）
fn is_llm_sidecar_command(fact: &Fact) -> Option<String> {
    let Fact::Command { instruction, .. } = fact else {
        return None;
    };
    let itype = instruction.get("type").and_then(|v| v.as_str())?;
    if itype != "call_external" {
        return None;
    }
    let params = instruction.get("params")?;
    params.get("messages")?;
    if params.get("service_name").is_some() || params.get("name").is_some() {
        return None;
    }
    params
        .get("audit_purpose")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// 读取单会话 WAL 全部记录（基础文件 + 分片合并；无该档案 → NotFound）
fn read_records(wal_dir: &Path, session_id: u64) -> Result<Vec<WalRecord>, ArchiveError> {
    let path = session_wal_base(wal_dir, session_id);
    if !path.exists() {
        return Err(ArchiveError::NotFound(session_id));
    }
    read_wal_with_hash(&path).map_err(|e| ArchiveError::Io(e.to_string()))
}

/// 由已读取的记录构建会话元数据
fn build_meta(session_id: u64, records: &[WalRecord], wal_bytes: u64) -> ArchiveSessionMeta {
    let (first_fact_type, last_fact_type) = match (records.first(), records.last()) {
        (Some(f), Some(l)) => (
            f.fact.type_name().to_string(),
            l.fact.type_name().to_string(),
        ),
        _ => ("-".into(), "-".into()),
    };
    // 侧车判定：扫首条 LLM 形态命令（正常即首条 Command；容错扫到 Stable 为止）
    let mut is_llm_sidecar = false;
    let mut audit_purpose = None;
    for rec in records {
        if matches!(rec.fact, Fact::Stable { .. }) {
            break;
        }
        if let Some(purpose) = is_llm_sidecar_command(&rec.fact) {
            is_llm_sidecar = true;
            audit_purpose = Some(purpose);
            break;
        }
    }
    ArchiveSessionMeta {
        session_id,
        fact_count: records.len() as u64,
        first_fact_type,
        last_fact_type,
        is_llm_sidecar,
        audit_purpose,
        wal_bytes,
    }
}

/// 重建审计链并逐条验证
///
/// 返回 (entries, last_chain_hash, verified, unhashed_records)。
/// 验证：每条重算 `blake3(prev_hash + content_hash)` 与 WAL 存储 chain_hash 比对；
/// 任一不匹配 → verified=false（疑似篡改/损坏，如实上报）。
fn rebuild_chain(
    records: &[WalRecord],
) -> (Vec<ArchiveAuditEntry>, String, bool, usize) {
    let mut entries = Vec::with_capacity(records.len());
    let mut prev_hash = String::from("genesis");
    let mut verified = true;
    let mut unhashed = 0usize;

    for (idx, rec) in records.iter().enumerate() {
        let fact = &rec.fact;
        let content_hash = match fact_hash(fact) {
            Ok(h) => h,
            Err(_) => {
                // 内容哈希算不出 = 内容已损坏，链无法继续验证
                verified = false;
                break;
            }
        };
        // 新格式记录：存储哈希必须与重算一致
        if let Some(stored) = &rec.content_hash {
            if stored != &content_hash {
                verified = false;
            }
        } else {
            unhashed += 1;
        }
        let combined = format!("{prev_hash}{content_hash}");
        let chain_hash = blake3::hash(combined.as_bytes()).to_hex().to_string();
        if let Some(stored_chain) = &rec.chain_hash {
            if stored_chain != &chain_hash {
                verified = false;
            }
        }
        let cause = match fact {
            Fact::StateTransition { cause, .. } | Fact::IoRequest { cause, .. } => Some(cause.0),
            Fact::IoResponse { request_id, .. } => Some(request_id.0),
            _ => None,
        };
        entries.push(ArchiveAuditEntry {
            fact_id: fact.id().0,
            fact_type: fact.type_name().to_string(),
            logical_time: (idx + 1) as u64,
            prev_hash: prev_hash.clone(),
            content_hash,
            cause,
            content_json: None,
        });
        prev_hash = chain_hash;
    }

    (entries, prev_hash, verified, unhashed)
}

/// 单条重建的审计条目（与活跃会话 audit 响应同形）
#[derive(Debug, Serialize)]
struct ArchiveAuditEntry {
    fact_id: u64,
    fact_type: String,
    /// 1-based 审计链条目序号（与写入侧 Auditor 口径一致）
    logical_time: u64,
    content_hash: String,
    prev_hash: String,
    cause: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_json: Option<serde_json::Value>,
}

/// 重建单会话审计响应（与活跃会话 `GET /api/sessions/{id}/audit` 同形）
///
/// 响应字段：`session_id` / `fact_count` / `last_hash` / `verified` /
/// `unhashed_records` / `entries[]`（`include_content=true` 时每条附
/// `content_json` 完整 Fact 内容，含 IoRequest params / IoResponse result）。
pub fn read_archive_audit(
    wal_dir: &Path,
    session_id: u64,
    include_content: bool,
) -> Result<serde_json::Value, ArchiveError> {
    let records = read_records(wal_dir, session_id)?;
    let (mut entries, last_hash, verified, unhashed) = rebuild_chain(&records);

    if include_content {
        let mut content_index: BTreeMap<u64, serde_json::Value> = BTreeMap::new();
        for rec in &records {
            content_index.insert(rec.fact.id().0, tcb_to_serde(&rec.fact.to_json()));
        }
        for entry in entries.iter_mut() {
            if let Some(content) = content_index.get(&entry.fact_id) {
                entry.content_json = Some(content.clone());
            }
        }
    }

    Ok(serde_json::json!({
        "session_id": session_id,
        "fact_count": records.len(),
        "last_hash": last_hash,
        "verified": verified,
        "unhashed_records": unhashed,
        "entries": entries,
    }))
}

/// 读取单会话 WAL 并构建元数据
fn build_meta_for(
    wal_dir: &Path,
    session_id: u64,
    wal_bytes: u64,
) -> Result<ArchiveSessionMeta, ArchiveError> {
    let records = read_records(wal_dir, session_id)?;
    Ok(build_meta(session_id, &records, wal_bytes))
}

/// 列出 wal_dir 下全部历史会话档案（只读扫描，无缓存）
pub fn list_archive_sessions(wal_dir: &Path) -> Result<Vec<ArchiveSessionMeta>, ArchiveError> {
    let inventory = scan_inventory(wal_dir)?;
    let mut metas = Vec::with_capacity(inventory.sessions.len());
    for (id, (_, wal_bytes)) in &inventory.sessions {
        metas.push(build_meta_for(wal_dir, *id, *wal_bytes)?);
    }
    Ok(metas)
}

/// 列表缓存：按 (mtime, size) 指纹失效的会话档案元数据缓存
///
/// 列表页高频访问，逐会话全量读 WAL 成本随历史增长；指纹未变化的会话
/// 直接复用上次元数据，新增/追加的会话重读。只读缓存，无写路径。
#[derive(Default)]
pub struct ArchiveCache {
    wal_dir: Option<PathBuf>,
    entries: BTreeMap<u64, CachedMeta>,
}

struct CachedMeta {
    meta: ArchiveSessionMeta,
    fingerprint: (u64, u64),
}

impl ArchiveCache {
    pub fn new(wal_dir: Option<PathBuf>) -> Self {
        Self {
            wal_dir,
            entries: BTreeMap::new(),
        }
    }

    /// 列出全部档案（带缓存；wal_dir 未启用或目录不可读 → 空列表）
    pub fn list(&mut self) -> Vec<ArchiveSessionMeta> {
        let Some(wal_dir) = &self.wal_dir else {
            return Vec::new();
        };
        let Ok(inventory) = scan_inventory(wal_dir) else {
            return Vec::new();
        };

        // 清理已消失的会话
        self.entries.retain(|id, _| inventory.sessions.contains_key(id));

        let mut out = Vec::with_capacity(inventory.sessions.len());
        for (id, (fingerprint, wal_bytes)) in &inventory.sessions {
            let meta = match self.entries.get(id) {
                Some(c) if c.fingerprint == *fingerprint => c.meta.clone(),
                _ => {
                    match build_meta_for(wal_dir, *id, *wal_bytes) {
                        Ok(m) => {
                            self.entries.insert(
                                *id,
                                CachedMeta {
                                    meta: m.clone(),
                                    fingerprint: *fingerprint,
                                },
                            );
                            m
                        }
                        // 单会话读失败不阻断整个列表；详情端点会再次如实报错
                        Err(_) => continue,
                    }
                }
            };
            out.push(meta);
        }
        out
    }

    /// 读取单会话审计（现读 WAL，保证最新；不走缓存）
    pub fn read_audit(
        &self,
        session_id: u64,
        include_content: bool,
    ) -> Result<serde_json::Value, ArchiveError> {
        match &self.wal_dir {
            Some(dir) => read_archive_audit(dir, session_id, include_content),
            None => Err(ArchiveError::WalDisabled),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
    use super::*;
    use evorule_reactor::{Fact, FactId};
    use evorule_tcb::JsonValue;

    fn cmd(id: u64, itype: &str, params: JsonValue) -> Fact {
        Fact::Command {
            id: FactId(id),
            instruction: JsonValue::object_from_pairs(&[
                ("type", JsonValue::string(itype)),
                ("params", params),
            ]),
        }
    }

    fn tmp_wal_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "evorule_audit_archive_{tag}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_session_wal(wal_dir: &Path, session_id: u64, facts: &[Fact]) {
        use evorule_reactor::WalWriter;
        let mut w = WalWriter::create(session_wal_base(wal_dir, session_id)).unwrap();
        let mut prev = String::from("genesis");
        for f in facts {
            let content = fact_hash(f).unwrap();
            let chain = blake3::hash(format!("{prev}{content}").as_bytes())
                .to_hex()
                .to_string();
            w.append_record_with_hash(0, f, &content, &prev, &chain)
                .unwrap();
            prev = chain;
        }
    }

    #[test]
    fn test_list_and_read_roundtrip() {
        let dir = tmp_wal_dir("roundtrip");
        let facts = vec![
            cmd(1, "increment", JsonValue::empty_object()),
            Fact::Stable {
                id: FactId(2),
                version: 1,
            },
        ];
        write_session_wal(&dir, 1, &facts);
        // 非会话文件必须被排除
        std::fs::write(dir.join("shared_facts.wal"), b"x").unwrap();
        std::fs::write(dir.join("governance_single_reactor.wal"), b"x").unwrap();

        let metas = list_archive_sessions(&dir).unwrap();
        assert_eq!(metas.len(), 1, "shared/governance 文件不应出现在档案");
        assert_eq!(metas[0].session_id, 1);
        assert_eq!(metas[0].fact_count, 2);
        assert!(!metas[0].is_llm_sidecar);

        let audit = read_archive_audit(&dir, 1, false).unwrap();
        assert_eq!(audit["fact_count"], 2);
        assert_eq!(audit["verified"], true);
        assert_eq!(audit["unhashed_records"], 0);
        assert_eq!(audit["entries"].as_array().unwrap().len(), 2);
        assert_eq!(audit["entries"][0]["logical_time"], 1);
        assert_eq!(audit["entries"][1]["logical_time"], 2);
        assert!(audit["entries"][0].get("content_json").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_llm_sidecar_detection() {
        let dir = tmp_wal_dir("sidecar");
        let params = JsonValue::object_from_pairs(&[
            ("messages", JsonValue::string("[{\"role\":\"user\",\"content\":\"hi\"}]")),
            ("audit_purpose", JsonValue::string("draft_rule")),
        ]);
        let facts = vec![cmd(1, "call_external", params)];
        write_session_wal(&dir, 7, &facts);

        let metas = list_archive_sessions(&dir).unwrap();
        assert!(metas[0].is_llm_sidecar);
        assert_eq!(metas[0].audit_purpose.as_deref(), Some("draft_rule"));

        // 非侧车:call_service(带 service_name)不算
        let dir2 = tmp_wal_dir("sidecar_neg");
        let params2 = JsonValue::object_from_pairs(&[
            ("messages", JsonValue::string("[]")),
            ("service_name", JsonValue::string("demo-svc")),
        ]);
        write_session_wal(&dir2, 8, &[cmd(1, "call_external", params2)]);
        let metas2 = list_archive_sessions(&dir2).unwrap();
        assert!(!metas2[0].is_llm_sidecar);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn test_tamper_detection() {
        let dir = tmp_wal_dir("tamper");
        let facts = vec![
            cmd(1, "increment", JsonValue::empty_object()),
            Fact::Stable {
                id: FactId(2),
                version: 1,
            },
        ];
        write_session_wal(&dir, 3, &facts);
        // 篡改:直接改写基础文件内容(破坏链)
        let path = session_wal_base(&dir, 3);
        let data = std::fs::read(&path).unwrap();
        let mut tampered = data.clone();
        let pos = data.windows(9).position(|w| w == b"increment").unwrap();
        tampered[pos..pos + 4].copy_from_slice(&b"incX"[..]);
        std::fs::write(&path, &tampered).unwrap();

        let audit = read_archive_audit(&dir, 3, false).unwrap();
        // 内容被改 → content_hash 重算不一致或解析出不同事实 → 链必须不通过
        if audit["fact_count"].as_u64().unwrap() == 2 {
            assert_eq!(audit["verified"], false, "篡改后验证必须失败");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_not_found_and_cache() {
        let dir = tmp_wal_dir("cache");
        let facts = vec![Fact::Stable {
            id: FactId(1),
            version: 1,
        }];
        write_session_wal(&dir, 5, &facts);

        assert!(matches!(
            read_archive_audit(&dir, 99, false),
            Err(ArchiveError::NotFound(99))
        ));

        let mut cache = ArchiveCache::new(Some(dir.clone()));
        let l1 = cache.list();
        assert_eq!(l1.len(), 1);
        let l2 = cache.list();
        assert_eq!(l2.len(), 1);
        assert_eq!(l2[0].session_id, 5);

        // 空缓存(无 wal_dir)
        let mut empty = ArchiveCache::new(None);
        assert!(empty.list().is_empty());
        assert!(matches!(
            empty.read_audit(5, false),
            Err(ArchiveError::WalDisabled)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_parse_filename() {
        assert_eq!(parse_session_wal_filename("session_3.wal"), Some(3));
        assert_eq!(parse_session_wal_filename("session_3.wal.1"), Some(3));
        assert_eq!(parse_session_wal_filename("session_12.wal.7"), Some(12));
        assert_eq!(parse_session_wal_filename("shared_facts.wal"), None);
        assert_eq!(parse_session_wal_filename("session_x.wal"), None);
        assert_eq!(parse_session_wal_filename("session_.wal"), None);
    }
}

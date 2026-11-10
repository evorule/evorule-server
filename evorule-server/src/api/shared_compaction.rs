// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 共享分区记忆值物理释放/版本压实
//!
//! 共享事实账（`SharedFactsLog`：WAL + metadata）同 path 覆写为追加新版本，
//! 旧版本与已 rollup 记忆值永久滞留盘面。本模块提供按 path 压实只留最新版 +
//! 旧版归档的物理释放面：
//!
//! - **保留/释放判定**：keep-latest + 权限豁免面（`shared.__permission__.entry.*`
//!   与 `shared.__permission__.default_policy` 全版本保留，权限时点语义与墓碑
//!   正确性红线）+ rolled_up 全释放（含同 path 旧版）。
//! - **归档先于一切改写**：archive 模式存原文 + digest，discard 模式仅存根
//!   （digest 可验「曾存在且未篡改」，原文不可恢复 = 真物理释放）；归档写出
//!   失败放弃本轮压实（原文件零触）。
//! - **重建互换（stop-world = 启动期）**：tmp WAL 按原相对顺序重编号连续重建
//!   （0..k-1），meta 先行互换；`next_fact_id` 续接 → id 空间零复用，跨 epoch
//!   version 数值不连续为显式文档化边界（物理释放的固有代价）。
//! - **缺省不变式**：无 marker 零动作；释放数 = 0 零改写；无自动阈值、无后台
//!   线程。纯内存模式（未配置 wal_dir）拒绝。
//!
//! 机制关键事实（以 evorule-governance 0.13 `shared_facts_log.rs` 为准）：
//! - 重编号铁证：`inner.version` = 重放中自增型事实条数，与间隙无关；保留原始
//!   版本号 + 丢弃中段会让压实后新写条目版本号小于旧条目 → `snapshot_at`
//!   「同条目取最高版本」判定旧条目复活压过新条目（权限状态倒退）。故必须
//!   重编号，压实后新 append 严格大于全部保留条目。
//! - `SharedFactsMetadata` 反序列化时 `next_fact_id`/`fact_sources`/
//!   `used_at_startup`/`rolled_up` 四字段无 `serde(default)`——bootstrap 最小
//!   meta 必须四键全量写出，否则 recover 拒收。
//! - 孤儿 fact（WAL 有、fact_sources 无）随保留面携带（source_session_id=0），
//!   报告 `orphans_carried` 显式留痕——不伪造归属、不丢载荷。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use axum::Extension;
use evorule_governance::shared_facts_log::{SharedFact, SharedFactsLog, SharedFactsMetadata};
use evorule_reactor::FactId;
use serde::{Deserialize, Serialize};

use crate::auth::CallerIdentity;

/// marker 文件名（wal_dir 下）：POST 端点写入，启动期执行位读取
pub const MARKER_FILE: &str = "shared_facts_compaction.request.json";
/// 最后一轮压实报告文件名（wal_dir 下）
pub const REPORT_FILE: &str = "shared_facts_compaction.last_report.json";
/// 归档文件名（wal_dir 下，append 模式永不重写）
pub const ARCHIVE_FILE: &str = "shared_facts_archive.jsonl";
/// 共享事实 WAL 文件名（与 main 启动恢复路径同源）
pub const SHARED_WAL_FILE: &str = "shared_facts.wal";
/// 共享事实 metadata 文件名（与 main 启动恢复路径同源）
pub const SHARED_META_FILE: &str = "shared_facts_meta.json";

/// 权限条目路径前缀（豁免面：全版本保留，永不压实）
/// 与 evorule-governance `PermissionTable::ENTRY_PREFIX` 字面量同源
const PERMISSION_ENTRY_PREFIX: &str = "shared.__permission__.entry.";
/// 默认策略路径（豁免面：精确匹配）
/// 与 evorule-governance `PermissionTable::DEFAULT_POLICY_PATH` 字面量同源
const PERMISSION_DEFAULT_POLICY_PATH: &str = "shared.__permission__.default_policy";
/// tmp 重建文件后缀
const TMP_SUFFIX: &str = ".compact-tmp";
/// 互换备份后缀（保留一轮，下次压实覆写）
const BACKUP_SUFFIX: &str = ".pre-compact";

/// 压实模式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionMode {
    /// 归档：释放事实原文 + 存根全量入档（存储责任移交用户）
    Archive,
    /// 丢弃：仅存根（digest 可验「曾存在且未篡改」，原文不可恢复）
    Discard,
}

impl CompactionMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Archive => "archive",
            Self::Discard => "discard",
        }
    }
}

/// POST 端点请求体
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionRequestBody {
    pub mode: CompactionMode,
}

/// 压实报告（成功路径 + 零动作路径共用；写入 REPORT_FILE 供 GET 端点呈现）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompactionReport {
    /// 本轮是否执行了重建互换（false = 无 marker 或释放数 0 零动作）
    #[serde(default)]
    pub executed: bool,
    /// 请求的模式（无 marker 时 None）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// 释放事实数
    #[serde(default)]
    pub released: u64,
    /// 保留事实数
    #[serde(default)]
    pub kept: u64,
    /// 随保留面携带的孤立 fact 数（不伪造归属、不丢载荷）
    #[serde(default)]
    pub orphans_carried: u64,
    #[serde(default)]
    pub bytes_before: u64,
    #[serde(default)]
    pub bytes_after: u64,
    /// 压实前版本号（= 旧账本条数）
    #[serde(default)]
    pub version_before: u64,
    /// 压实后版本号（= 保留条数；跨 epoch 数值回落为显式文档化边界）
    #[serde(default)]
    pub version_after: u64,
    #[serde(default)]
    pub next_fact_id_before: u64,
    #[serde(default)]
    pub next_fact_id_after: u64,
    /// 失败原因（报告随失败写入时携带）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 压实失败（fail-visible：放弃时原文件零触）
#[derive(Debug)]
pub struct CompactionError(String);

impl std::fmt::Display for CompactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "shared facts compaction failed: {}", self.0)
    }
}

impl std::error::Error for CompactionError {}

fn is_exempt(path: &str) -> bool {
    path.starts_with(PERMISSION_ENTRY_PREFIX) || path == PERMISSION_DEFAULT_POLICY_PATH
}

fn file_size(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

fn append_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut name = p.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(suffix);
    p.with_file_name(name)
}

fn remove_if_exists(p: &Path) -> Result<(), CompactionError> {
    if p.exists() {
        std::fs::remove_file(p)
            .map_err(|e| CompactionError(format!("remove {} failed: {e}", p.display())))?;
    }
    Ok(())
}

/// 读旧 metadata（不存在 = 默认值，与 recover 语义一致）
fn read_metadata(meta: &Path) -> Result<SharedFactsMetadata, CompactionError> {
    match std::fs::read_to_string(meta) {
        Ok(content) => serde_json::from_str(&content)
            .map_err(|e| CompactionError(format!("metadata parse failed: {e}"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(SharedFactsMetadata::default()),
        Err(e) => Err(CompactionError(format!("metadata read failed: {e}"))),
    }
}

/// 写 bootstrap 最小 metadata：`next_fact_id` 续接旧空间，余默认。
/// 四个无 `serde(default)` 的字段必须全量写出（见模块文档）。
fn write_bootstrap_meta(meta: &Path, next_fact_id: u64) -> Result<(), CompactionError> {
    let payload = serde_json::json!({
        "next_fact_id": next_fact_id,
        "fact_sources": {},
        "used_at_startup": {},
        "rolled_up": [],
    });
    std::fs::write(meta, payload.to_string())
        .map_err(|e| CompactionError(format!("bootstrap metadata write failed: {e}")))
}

/// 保留/释放判定结果
struct ReleaseDecision {
    /// 保留集（按原相对顺序 = version 升序）
    kept: Vec<SharedFact>,
    /// 释放集（version 升序）
    released: Vec<SharedFact>,
    /// 随保留面携带的孤立 fact 数
    orphans_carried: u64,
}

/// 保留/释放判定（纯函数，只读旧实例）
///
/// 枚举全部 = `facts_by_path_prefix("")`（未 rollup 全量，含孤儿）+ 旧 meta
/// rolled_up 清单（`fact_by_id` 逐项补齐——rolled_up 项必在 fact_sources，
/// 与孤儿不可能并存）。
fn decide(old: &SharedFactsLog, old_meta: &SharedFactsMetadata) -> ReleaseDecision {
    let mut all: Vec<SharedFact> = old.facts_by_path_prefix("");
    for id in &old_meta.rolled_up {
        if let Some(f) = old.fact_by_id(FactId(*id)) {
            all.push(f);
        }
    }
    // version 升序 = 原相对顺序（append 单口写入，version 随记录序自增）
    all.sort_by_key(|f| f.version);

    // 每 path 最高版本（全量口径：live + rolled_up）
    let mut latest: BTreeMap<String, u64> = BTreeMap::new();
    for f in &all {
        latest
            .entry(f.path.clone())
            .and_modify(|v| {
                if f.version > *v {
                    *v = f.version;
                }
            })
            .or_insert(f.version);
    }

    let mut kept = Vec::new();
    let mut released = Vec::new();
    let mut orphans_carried = 0u64;
    for f in all {
        let orphan = old.source_session_id(f.fact_id).is_none();
        if orphan {
            orphans_carried += 1;
        }
        if is_exempt(&f.path) {
            // 豁免面：全版本保留（优先级最高，rolled_up/孤儿一律不压）
            kept.push(f);
        } else if old_meta.rolled_up.contains(&f.fact_id.0) {
            // 释放面①：rolled_up 全部
            released.push(f);
        } else if orphan {
            // 孤儿随保留面携带：不伪造归属、不丢载荷，报告留痕
            kept.push(f);
        } else if latest.get(&f.path).copied() == Some(f.version) {
            // 释放面②的对偶：非豁免 path 最新版本保留
            kept.push(f);
        } else {
            // 释放面②：非豁免 path 的非最新版本
            released.push(f);
        }
    }
    ReleaseDecision {
        kept,
        released,
        orphans_carried,
    }
}

/// 归档追加（append 模式永不重写；每轮前置 manifest 行 + 每释放事实一行存根）
fn append_archive(
    archive: &Path,
    mode: CompactionMode,
    released: &[SharedFact],
    bytes_before: u64,
) -> Result<(), CompactionError> {
    use std::io::Write;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(archive)
        .map_err(|e| {
            CompactionError(format!(
                "archive open failed (compaction aborted, originals untouched): {e}"
            ))
        })?;
    let manifest = serde_json::json!({
        "record": "manifest",
        "mode": mode.as_str(),
        "timestamp": ts,
        "released": released.len(),
        "bytes_before": bytes_before,
    });
    writeln!(file, "{manifest}")
        .map_err(|e| CompactionError(format!("archive manifest write failed: {e}")))?;
    for f in released {
        // JsonValue 不实现 Serialize：先转 serde 形态再算规范摘要
        // （evorule-hash 族 B 哈希纪律，禁自写 blake3；prefixed 自描述前缀）
        let value_ser = evorule_reactor::tcb_to_serde(&f.value);
        let digest = evorule_hash::prefixed(&evorule_hash::json_digest(&value_ser));
        let mut stub = serde_json::json!({
            "record": "fact",
            "fact_id": f.fact_id.0,
            "path": f.path,
            "source_session_id": f.source_session_id,
            "origin_fact_id": f.origin_fact_id,
            "version": f.version,
            "value_len": value_ser.to_string().len(),
            "value_digest": digest,
        });
        if mode == CompactionMode::Archive {
            stub["value"] = value_ser;
        }
        writeln!(file, "{stub}")
            .map_err(|e| CompactionError(format!("archive stub write failed: {e}")))?;
    }
    file.flush()
        .map_err(|e| CompactionError(format!("archive flush failed: {e}")))?;
    Ok(())
}

fn cleanup_tmp(tmp_wal: &Path, tmp_meta: &Path) {
    let _ = std::fs::remove_file(tmp_wal);
    let _ = std::fs::remove_file(tmp_meta);
}

/// 压实主入口：判定 → 归档 → tmp 重建 → 复验 → 互换。
///
/// 任一步失败放弃本轮：归档失败原文件零触；重建/复验失败删除 tmp 原文件零触；
/// 互换中途失败备份（`*.pre-compact`）在位，恢复程序 = 改名回滚。
/// 释放数 = 0 → 零动作（不建 tmp 不互换），报告零值。
pub fn compact_shared_facts(
    wal: &Path,
    meta: &Path,
    archive: &Path,
    mode: CompactionMode,
) -> Result<CompactionReport, CompactionError> {
    let old_meta = read_metadata(meta)?;
    let bytes_before = file_size(wal);

    // 判定（recover 含全链验链：失败 fail-visible 放弃）
    let (kept, released, orphans_carried, version_before, next_before) = {
        let old = SharedFactsLog::recover(wal, meta)
            .map_err(|e| CompactionError(format!("shared facts recover failed: {e}")))?;
        let d = decide(&old, &old_meta);
        (
            d.kept,
            d.released,
            d.orphans_carried,
            old.version(),
            old_meta.next_fact_id.max(1),
        )
    }; // old 实例在此 drop：释放旧 WAL 文件句柄（Windows 互换前置条件）

    let mut report = CompactionReport {
        mode: Some(mode.as_str().to_string()),
        released: released.len() as u64,
        kept: kept.len() as u64,
        orphans_carried,
        bytes_before,
        version_before,
        next_fact_id_before: next_before,
        ..Default::default()
    };

    if released.is_empty() {
        // 零动作：不建 tmp 不互换不归档，报告零值
        return Ok(report);
    }

    // 归档先于一切改写：失败放弃本轮（原文件零触）
    append_archive(archive, mode, &released, bytes_before)?;

    // tmp 重建路径（残留清理：上次互换中断可能遗留）
    let tmp_wal = append_suffix(wal, TMP_SUFFIX);
    let tmp_meta = append_suffix(meta, TMP_SUFFIX);
    remove_if_exists(&tmp_wal)?;
    remove_if_exists(&tmp_meta)?;

    write_bootstrap_meta(&tmp_meta, next_before)?;

    // 重编号重建：按原相对顺序连续 append，新 id 自旧 next 起零复用；
    // 孤儿携带（source_session_id=0，判定时已定）；origin 随条目保真。
    let mut id_map: BTreeMap<u64, u64> = BTreeMap::new();
    {
        let tmp_log = SharedFactsLog::recover(&tmp_wal, &tmp_meta).map_err(|e| {
            cleanup_tmp(&tmp_wal, &tmp_meta);
            CompactionError(format!("tmp ledger bootstrap failed: {e}"))
        })?;
        for (i, f) in kept.iter().enumerate() {
            tmp_log
                .append_with_origin(
                    &f.path,
                    f.value.clone(),
                    f.source_session_id,
                    f.origin_fact_id,
                )
                .map_err(|e| {
                    cleanup_tmp(&tmp_wal, &tmp_meta);
                    CompactionError(format!("tmp ledger rebuild append failed: {e}"))
                })?;
            id_map.insert(f.fact_id.0, next_before + i as u64);
        }
        // 消费关系重建：已释放 id 的引用自然消落（辅助索引，WAL 权威口径）
        for (sid, ids) in &old_meta.used_at_startup {
            let mapped: Vec<FactId> = ids
                .iter()
                .filter_map(|id| id_map.get(id).copied())
                .map(FactId)
                .collect();
            if !mapped.is_empty() {
                tmp_log.record_used_at_startup(*sid, &mapped);
            }
        }
    } // tmp_log drop：metadata 自动持久化、WAL writer 关闭

    // 复验（独立 recover：验链 + 计数 + 逐条采样 + 权限快照对口）；任一不过删除 tmp 放弃
    {
        let check = SharedFactsLog::recover(&tmp_wal, &tmp_meta).map_err(|e| {
            cleanup_tmp(&tmp_wal, &tmp_meta);
            CompactionError(format!("tmp ledger verify recover failed: {e}"))
        })?;
        if check.version() != kept.len() as u64 || check.history_len() != kept.len() {
            cleanup_tmp(&tmp_wal, &tmp_meta);
            return Err(CompactionError(format!(
                "tmp ledger verify failed: version/history mismatch ({} vs {})",
                check.version(),
                kept.len()
            )));
        }
        for (i, f) in kept.iter().enumerate() {
            match check.fact_by_id(FactId(next_before + i as u64)) {
                Some(g) if g.path == f.path && g.value == f.value => {}
                _ => {
                    cleanup_tmp(&tmp_wal, &tmp_meta);
                    return Err(CompactionError(format!(
                        "tmp ledger verify failed: fact sample mismatch at index {i}"
                    )));
                }
            }
        }
        // 权限快照对口：fence = version()+1（全量现状语义，严格 < 围栏 off-by-one）
        let mut expected_entries: BTreeMap<String, (u64, bool)> = BTreeMap::new();
        for f in &kept {
            if let Some(id) = f.path.strip_prefix(PERMISSION_ENTRY_PREFIX) {
                let tombstone = match evorule_reactor::tcb_to_serde(&f.value) {
                    serde_json::Value::Object(map) => map
                        .get("__deleted")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    _ => false,
                };
                match expected_entries.get(id) {
                    Some((v, _)) if *v > f.version => {}
                    _ => {
                        expected_entries.insert(id.to_string(), (f.version, tombstone));
                    }
                }
            }
        }
        let table = evorule_governance::permission::PermissionTable::snapshot_at(
            &check,
            check.version() + 1,
        )
        .map_err(|e| {
            cleanup_tmp(&tmp_wal, &tmp_meta);
            CompactionError(format!("tmp ledger verify failed: snapshot_at: {e}"))
        })?;
        for (id, (_, tombstone)) in &expected_entries {
            if *tombstone {
                continue;
            }
            if table.get(id).is_none() {
                cleanup_tmp(&tmp_wal, &tmp_meta);
                return Err(CompactionError(format!(
                    "tmp ledger verify failed: permission entry '{id}' missing in snapshot"
                )));
            }
        }
    }

    // 互换序（meta 先行）：备份先清（Windows rename 目标存在会失败；备份保留一轮）
    let meta_backup = append_suffix(meta, BACKUP_SUFFIX);
    let wal_backup = append_suffix(wal, BACKUP_SUFFIX);
    remove_if_exists(&meta_backup)?;
    remove_if_exists(&wal_backup)?;
    std::fs::rename(meta, &meta_backup)
        .map_err(|e| CompactionError(format!("swap meta->backup failed: {e}")))?;
    std::fs::rename(&tmp_meta, meta)
        .map_err(|e| CompactionError(format!("swap tmp_meta->meta failed: {e}")))?;
    std::fs::rename(wal, &wal_backup)
        .map_err(|e| CompactionError(format!("swap wal->backup failed: {e}")))?;
    std::fs::rename(&tmp_wal, wal)
        .map_err(|e| CompactionError(format!("swap tmp_wal->wal failed: {e}")))?;

    report.executed = true;
    report.bytes_after = file_size(wal);
    report.version_after = kept.len() as u64;
    report.next_fact_id_after = next_before + kept.len() as u64;
    Ok(report)
}

/// 启动期执行位：marker 在位才执行（缺省零动作），成功清 marker + 写报告。
///
/// 失败返回 Err（调用方告警、marker 保留、下轮重试；启动不阻——随后的
/// 恢复验链仍是独立 fail-closed 闸）。
pub fn run_requested_compaction(wal_dir: &Path) -> Result<CompactionReport, CompactionError> {
    let marker = wal_dir.join(MARKER_FILE);
    if !marker.exists() {
        return Ok(CompactionReport::default());
    }
    let raw = std::fs::read_to_string(&marker)
        .map_err(|e| CompactionError(format!("marker read failed: {e}")))?;
    let req: CompactionRequestBody = serde_json::from_str(&raw)
        .map_err(|e| CompactionError(format!("marker parse failed: {e}")))?;
    let report = compact_shared_facts(
        &wal_dir.join(SHARED_WAL_FILE),
        &wal_dir.join(SHARED_META_FILE),
        &wal_dir.join(ARCHIVE_FILE),
        req.mode,
    )?;
    // 成功（含零动作）清 marker + 写报告
    std::fs::remove_file(&marker)
        .map_err(|e| CompactionError(format!("marker remove failed: {e}")))?;
    write_report(wal_dir, &report)?;
    Ok(report)
}

/// 写报告文件（GET 端点呈现面）
fn write_report(wal_dir: &Path, report: &CompactionReport) -> Result<(), CompactionError> {
    let json = serde_json::to_string_pretty(report)
        .map_err(|e| CompactionError(format!("report serialize failed: {e}")))?;
    std::fs::write(wal_dir.join(REPORT_FILE), json)
        .map_err(|e| CompactionError(format!("report write failed: {e}")))
}

/// 写压实请求 marker（POST 端点 IO；独立函数便于单测）
fn write_compaction_request(wal_dir: &Path, mode: CompactionMode) -> Result<(), CompactionError> {
    let payload = serde_json::json!({ "mode": mode.as_str() });
    std::fs::write(wal_dir.join(MARKER_FILE), payload.to_string())
        .map_err(|e| CompactionError(format!("marker write failed: {e}")))
}

/// 共享 WAL 目录（启动期由 main 注入一次；None = 纯内存模式，端点拒绝）
static SHARED_WAL_DIR: OnceLock<PathBuf> = OnceLock::new();

/// 启动期注入共享 WAL 目录（仿插件健康快照全局 setter 先例；仅 wal_dir 配置分支调用）
pub fn set_shared_wal_dir(dir: PathBuf) {
    let _ = SHARED_WAL_DIR.set(dir);
}

fn admin_err(status: StatusCode, msg: String) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({ "error": msg })))
}

/// `POST /api/admin/shared/compaction` → 登记共享分区压实请求（下一轮重启生效）
///
/// 人工治理动作：自动化凭据拒绝 + 平台管理员硬校验（与 WAL checkpoint 同面）。
/// 写 marker 于 wal_dir；纯内存模式（未配置 wal_dir）拒绝并说明。
/// 落账 platform.event（shared_compaction_scheduled）。
#[utoipa::path(
    post,
    path = "/api/admin/shared/compaction",
    tag = "admin",
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "已登记（下一轮重启生效）；请求体 {\"mode\":\"archive\"|\"discard\"}", body = serde_json::Value),
        (status = 400, description = "mode 非法 / 纯内存模式无共享分区"),
        (status = 401, description = "未认证"),
        (status = 403, description = "非管理员或自动化凭据"),
        (status = 500, description = "marker 写入失败")
    )
)]
pub async fn admin_shared_compaction(
    State(shared): State<SharedFactsLog>,
    headers: HeaderMap,
    identity: Option<Extension<CallerIdentity>>,
    Json(payload): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    // 人工治理动作，自动化凭据拒绝（与 WAL checkpoint 同款前置）
    if matches!(
        identity,
        Some(Extension(CallerIdentity::Service)) | Some(Extension(CallerIdentity::App))
    ) {
        return Err(admin_err(
            StatusCode::FORBIDDEN,
            "自动化凭据禁止登记共享分区压实（人工治理动作）".to_string(),
        ));
    }
    let username = crate::api::platform_auth::require_administrator(&shared, &headers)?;

    let mode = match payload.get("mode").and_then(|m| m.as_str()) {
        Some("archive") => CompactionMode::Archive,
        Some("discard") => CompactionMode::Discard,
        _ => {
            return Err(admin_err(
                StatusCode::BAD_REQUEST,
                "mode 必须为 archive 或 discard".to_string(),
            ))
        }
    };
    let Some(wal_dir) = SHARED_WAL_DIR.get() else {
        return Err(admin_err(
            StatusCode::BAD_REQUEST,
            "纯内存模式（未配置 wal_dir），无共享分区可压实".to_string(),
        ));
    };
    write_compaction_request(wal_dir, mode)
        .map_err(|e| admin_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    crate::api::platform_auth::append_platform_event(
        &shared,
        "shared_compaction_scheduled",
        serde_json::json!({ "mode": mode.as_str(), "scheduled_by": username }),
    );
    Ok(Json(serde_json::json!({
        "scheduled": true,
        "effective": "next_restart",
        "mode": mode.as_str(),
    })))
}

/// `GET /api/admin/shared/compaction` → 最后一轮压实报告
///
/// 管理员限定（报告含释放面路径与 digest 摘要）。无报告（未执行过）404。
#[utoipa::path(
    get,
    path = "/api/admin/shared/compaction",
    tag = "admin",
    responses(
        (status = 200, description = "最后一轮压实报告", body = serde_json::Value),
        (status = 401, description = "未认证"),
        (status = 403, description = "非管理员或自动化凭据"),
        (status = 404, description = "纯内存模式或尚无报告")
    )
)]
pub async fn get_shared_compaction_report(
    State(shared): State<SharedFactsLog>,
    headers: HeaderMap,
    identity: Option<Extension<CallerIdentity>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if matches!(
        identity,
        Some(Extension(CallerIdentity::Service)) | Some(Extension(CallerIdentity::App))
    ) {
        return Err(admin_err(
            StatusCode::FORBIDDEN,
            "自动化凭据禁止读取共享分区压实报告（人工治理动作）".to_string(),
        ));
    }
    crate::api::platform_auth::require_administrator(&shared, &headers)?;
    let Some(wal_dir) = SHARED_WAL_DIR.get() else {
        return Err(admin_err(
            StatusCode::NOT_FOUND,
            "纯内存模式（未配置 wal_dir），无压实报告".to_string(),
        ));
    };
    let raw = match std::fs::read_to_string(wal_dir.join(REPORT_FILE)) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(admin_err(
                StatusCode::NOT_FOUND,
                "尚无压实报告（未执行过压实）".to_string(),
            ))
        }
        Err(e) => {
            return Err(admin_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("report read failed: {e}"),
            ))
        }
    };
    let report: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
        admin_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("report parse failed: {e}"),
        )
    })?;
    Ok(Json(report))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used, clippy::panic)]
    use super::*;
    use evorule_tcb::JsonValue;

    fn jv(s: &str) -> JsonValue {
        JsonValue::string(s)
    }

    /// 搭一份旧账本：返回 (wal, meta, archive)。
    /// 内容（version 序）：
    /// - id1 shared.ns.note = "a1"（v1，非最新 → 释放）
    /// - id2 shared.ns.note = "a2"（v2，最新 → 保留）
    /// - id3 shared.session.s1.summary = "s1"（v3，mark rollup → 释放）
    /// - id4 shared.__permission__.entry.p1 = PermissionEntry(human/Allow)
    ///   （v4，豁免 → 保留；append_with_origin 带 origin=9 验溯源保真）
    /// - id5 shared.__permission__.default_policy = "deny"（v5，豁免 → 保留）
    /// - id6 shared.ns.keep = "k1"（v6，单版本最新 → 保留）
    fn build_ledger(dir: &Path, name: &str) -> (PathBuf, PathBuf, PathBuf) {
        use evorule_governance::permission::entry::{
            Effect, PermissionEntry, PermissionState, Resource, ResourceType, Subject, SubjectType,
        };
        let wal = dir.join(format!("{name}.wal"));
        let meta = dir.join(format!("{name}_meta.json"));
        let archive = dir.join(format!("{name}_archive.jsonl"));
        let old = SharedFactsLog::recover(&wal, &meta).unwrap();
        old.append("shared.ns.note", jv("a1"), 100).unwrap();
        old.append("shared.ns.note", jv("a2"), 100).unwrap();
        old.append("shared.session.s1.summary", jv("s1"), 100)
            .unwrap();
        // 权限条目值必须是合法 PermissionEntry JSON（复验 snapshot_at 会逐条反序列化）
        let mut entry = PermissionEntry::new(
            "p1",
            Subject {
                subject_type: SubjectType::User,
                id: "human".to_string(),
            },
            Resource {
                resource_type: ResourceType::IoAction,
                path: "io:*".to_string(),
            },
            Effect::Allow,
        );
        entry.state = PermissionState::Active;
        let entry_value = evorule_reactor::serde_to_tcb(&serde_json::to_value(&entry).unwrap());
        old.append_with_origin("shared.__permission__.entry.p1", entry_value, 0, Some(9))
            .unwrap();
        old.append("shared.__permission__.default_policy", jv("deny"), 0)
            .unwrap();
        old.append("shared.ns.keep", jv("k1"), 101).unwrap();
        old.mark_as_rollup(&[FactId(3)]);
        // 消费关系：session 42 用了 id2（保留）与 id3（释放 → 引用消落）
        old.record_used_at_startup(42, &[FactId(2), FactId(3)]);
        (wal, meta, archive)
    }

    /// 验收锚 1+4：保留面正确 + id 空间续接零复用 + used_at_startup 消落
    #[test]
    fn keep_latest_exempt_rollup_release_with_id_continuity() {
        let dir = tempfile::tempdir().unwrap();
        let (wal, meta, archive) = build_ledger(dir.path(), "sf");

        let report = compact_shared_facts(&wal, &meta, &archive, CompactionMode::Discard).unwrap();
        assert!(report.executed);
        assert_eq!(report.released, 2);
        assert_eq!(report.kept, 4);
        assert_eq!(report.version_before, 6);
        assert_eq!(report.version_after, 4);
        assert_eq!(report.next_fact_id_before, 7);
        assert_eq!(report.next_fact_id_after, 11);

        let fresh = SharedFactsLog::recover(&wal, &meta).unwrap();
        assert_eq!(fresh.version(), 4);
        assert_eq!(fresh.history_len(), 4);
        assert_eq!(fresh.facts_by_path_prefix("").len(), 4);

        // 重编号映射：保留集按 version 升序 → 新 id 7,8,9,10
        // 保留面 source/origin 对口
        let a2 = fresh.fact_by_id(FactId(7)).unwrap();
        assert_eq!(a2.path, "shared.ns.note");
        assert_eq!(a2.value, jv("a2"));
        assert_eq!(a2.source_session_id, 100);
        let perm = fresh.fact_by_id(FactId(8)).unwrap();
        assert_eq!(perm.path, "shared.__permission__.entry.p1");
        assert_eq!(perm.origin_fact_id, Some(9), "origin 随条目保真");
        assert!(fresh
            .fact_by_id(FactId(9))
            .unwrap()
            .path
            .ends_with("default_policy"));
        assert_eq!(fresh.fact_by_id(FactId(10)).unwrap().value, jv("k1"));

        // 释放项不可读（fact_sources 无映射）
        assert!(fresh.fact_by_id(FactId(1)).is_none());
        assert!(fresh.fact_by_id(FactId(3)).is_none());

        // id 空间零复用：新 append 的 fact_id = 旧 next（7+4=11）；
        // append 返回值 = 写入后版本（计数口径），history 查询的 version 字段 =
        // version_before（写入前版本，0 起索引）——新条目 version_before=4
        // 严格大于全部保留条目（≤3）
        let v_new = fresh.append("shared.ns.new", jv("n1"), 200).unwrap();
        assert_eq!(
            v_new, 5,
            "重编号后新写版本（写入后口径）严格大于全部保留条目"
        );
        let new_fact = &fresh.facts_by_path_prefix("shared.ns.new")[0];
        assert_eq!(new_fact.fact_id, FactId(11));
        assert_eq!(new_fact.version, 4);

        // used_at_startup 重建：已释放 id3 引用消落，id2 重映射为 7
        assert_eq!(
            fresh.get_used_at_startup(42),
            Some(vec![FactId(7)]),
            "释放 id 的消费引用应消落，保留 id 应重映射"
        );
    }

    /// 验收锚 2：archive 模式含原文 + digest 对口；discard 模式零原文泄漏
    #[test]
    fn archive_modes_value_visibility_and_digest() {
        let dir = tempfile::tempdir().unwrap();
        let (wal, meta, archive) = build_ledger(dir.path(), "sf");
        compact_shared_facts(&wal, &meta, &archive, CompactionMode::Archive).unwrap();

        let content = std::fs::read_to_string(&archive).unwrap();
        // manifest 前置
        assert!(content.contains("\"record\":\"manifest\""), "{content}");
        assert!(content.contains("\"mode\":\"archive\""), "{content}");
        // archive 模式：释放事实原文在档 + digest 对口
        assert!(content.contains("\"value\":\"a1\""), "{content}");
        let a1_digest = evorule_hash::prefixed(&evorule_hash::json_digest(
            &evorule_reactor::tcb_to_serde(&jv("a1")),
        ));
        assert!(content.contains(&a1_digest), "{content}");
        // 存根字段齐备（version 字段 = version_before 口径：id1→0、id3→2）
        assert!(content.contains("\"fact_id\":1"), "{content}");
        assert!(content.contains("\"fact_id\":3"), "{content}");
        assert!(content.contains("\"version\":0"), "{content}");
        assert!(content.contains("\"version\":2"), "{content}");

        // discard 模式：零原文泄漏
        let dir2 = tempfile::tempdir().unwrap();
        let (wal2, meta2, archive2) = build_ledger(dir2.path(), "sf");
        compact_shared_facts(&wal2, &meta2, &archive2, CompactionMode::Discard).unwrap();
        let content2 = std::fs::read_to_string(&archive2).unwrap();
        assert!(
            !content2.contains("a1"),
            "discard 存根不得含原文: {content2}"
        );
        assert!(
            !content2.contains("s1\""),
            "discard 存根不得含原文: {content2}"
        );
        assert!(content2.contains("value_digest"), "{content2}");
        assert!(content2.contains("\"mode\":\"discard\""), "{content2}");
    }

    /// 验收锚 2：归档失败放弃压实且原文件字节不变
    #[test]
    fn archive_failure_aborts_with_originals_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let (wal, meta, archive) = build_ledger(dir.path(), "sf");
        let wal_bytes = std::fs::read(&wal).unwrap();
        let meta_bytes = std::fs::read(&meta).unwrap();

        // 归档落点预置为目录 → open 必败
        std::fs::create_dir(&archive).unwrap();
        let err = compact_shared_facts(&wal, &meta, &archive, CompactionMode::Discard).unwrap_err();
        assert!(
            err.to_string().contains("archive"),
            "错误应定位归档失败: {err}"
        );

        // 原文件字节不变 + 无 tmp 残留
        assert_eq!(std::fs::read(&wal).unwrap(), wal_bytes, "WAL 字节必须不变");
        assert_eq!(
            std::fs::read(&meta).unwrap(),
            meta_bytes,
            "metadata 字节必须不变"
        );
        let entries: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
            .collect();
        assert!(
            !entries.iter().any(|n| n.ends_with(TMP_SUFFIX)),
            "不得有 tmp 残留: {entries:?}"
        );
    }

    /// 验收锚 6：缺省零发生——无 marker 零动作；释放数 0 零改写（marker 仍清除）
    #[test]
    fn default_no_action_on_missing_marker_and_zero_release() {
        // 无 marker：零动作，不创建 WAL
        let dir = tempfile::tempdir().unwrap();
        let report = run_requested_compaction(dir.path()).unwrap();
        assert!(!report.executed);
        assert!(
            !dir.path().join(SHARED_WAL_FILE).exists(),
            "无 marker 不得创建 WAL"
        );

        // 释放数 0：零改写（单版本 + 豁免面全保留）
        let dir2 = tempfile::tempdir().unwrap();
        let wal = dir2.path().join(SHARED_WAL_FILE);
        let meta = dir2.path().join(SHARED_META_FILE);
        let old = SharedFactsLog::recover(&wal, &meta).unwrap();
        old.append("shared.ns.only", jv("v1"), 100).unwrap();
        old.append("shared.__permission__.entry.p", jv("perm"), 0)
            .unwrap();
        drop(old);
        let wal_bytes = std::fs::read(&wal).unwrap();
        let meta_bytes = std::fs::read(&meta).unwrap();

        write_compaction_request(dir2.path(), CompactionMode::Archive).unwrap();
        let report = run_requested_compaction(dir2.path()).unwrap();
        assert!(!report.executed, "释放数 0 应零动作");
        assert_eq!(report.released, 0);
        assert_eq!(std::fs::read(&wal).unwrap(), wal_bytes, "WAL 字节必须不变");
        assert_eq!(
            std::fs::read(&meta).unwrap(),
            meta_bytes,
            "metadata 字节必须不变"
        );
        assert!(
            !dir2.path().join(MARKER_FILE).exists(),
            "零动作也算成功路径，marker 应清除"
        );
        assert!(dir2.path().join(REPORT_FILE).exists(), "零动作报告应落盘");
        // 零动作不写归档
        assert!(!dir2.path().join(ARCHIVE_FILE).exists());
    }

    /// 验收锚 3：版本守卫红线——重编号后新写权限条目胜过旧条目 + 围栏语义不破
    #[test]
    fn renumbered_ledger_new_permission_entry_wins_and_fence_holds() {
        use evorule_governance::permission::entry::{
            Effect, PermissionEntry, PermissionState, Resource, ResourceType, Subject, SubjectType,
        };
        use evorule_governance::permission::PermissionTable;

        let dir = tempfile::tempdir().unwrap();
        let wal = dir.path().join("sf.wal");
        let meta = dir.path().join("sf_meta.json");
        let archive = dir.path().join("sf_archive.jsonl");

        let entry = |subject: &str, effect: Effect| {
            let mut e = PermissionEntry::new(
                "P",
                Subject {
                    subject_type: SubjectType::User,
                    id: subject.to_string(),
                },
                Resource {
                    resource_type: ResourceType::IoAction,
                    path: "io:*".to_string(),
                },
                effect,
            );
            e.state = PermissionState::Active;
            e
        };

        // 旧账本：9 条非豁免旧版本（全释放）+ 权限条目 P 在 v10（高版本）
        {
            let old = SharedFactsLog::recover(&wal, &meta).unwrap();
            for i in 1..=9 {
                old.append("shared.f.x", jv("old"), 100).unwrap();
                let _ = i;
            }
            PermissionTable::store_entry(&old, &entry("human", Effect::Allow), 0).unwrap();
        }
        // 旧语义下 v10 时点的 P = human/Allow
        {
            let old = SharedFactsLog::recover(&wal, &meta).unwrap();
            let t = PermissionTable::snapshot_at(&old, old.version() + 1).unwrap();
            assert_eq!(t.get("P").unwrap().subject.id, "human");
        }

        // 压实：f.x 旧版本 8 条释放（留最新 1 条）、P 重编号至低位
        // （若保留原始版本号，新写条目版本将小于 P 的 v10 → 红线）
        let report = compact_shared_facts(&wal, &meta, &archive, CompactionMode::Discard).unwrap();
        assert_eq!(report.released, 8);
        assert_eq!(report.version_after, 2);

        let fresh = SharedFactsLog::recover(&wal, &meta).unwrap();
        assert_eq!(fresh.version(), 2);

        // 压实后新写权限条目（同 id 覆写）→ version_before=2 > P 的 version_before=1
        PermissionTable::store_entry(&fresh, &entry("human2", Effect::Deny), 0).unwrap();
        assert_eq!(fresh.version(), 3);

        // 围栏语义（严格 <）：fence = version() = 3 → 含 version_before < 3 全部，
        // 最高版本胜出 = 新条目
        let t_now = PermissionTable::snapshot_at(&fresh, fresh.version()).unwrap();
        let won = t_now.get("P").unwrap();
        assert_eq!(
            won.subject.id, "human2",
            "重编号后新写权限条目必须胜过旧条目（最高版本胜出）"
        );
        assert_eq!(won.effect, Effect::Deny);
        // fence = version()-1 = 2 → 排除 version_before=2 的新条目，仍是旧条目
        // （off-by-one 语义：version_before == v_shared 属于下一时点）
        let t_prev = PermissionTable::snapshot_at(&fresh, fresh.version() - 1).unwrap();
        assert_eq!(
            t_prev.get("P").unwrap().subject.id,
            "human",
            "fence 排除新条目（version_before == fence）时必须回落到旧条目"
        );
    }

    /// 验收锚 1 补：孤儿 fact 随保留面携带并显式留痕（不伪造归属、不丢载荷）
    #[test]
    fn orphan_facts_carried_and_reported() {
        let dir = tempfile::tempdir().unwrap();
        let wal = dir.path().join("sf.wal");
        let meta = dir.path().join("sf_meta.json");
        let archive = dir.path().join("sf_archive.jsonl");

        {
            let old = SharedFactsLog::recover(&wal, &meta).unwrap();
            old.append("shared.ns.a", jv("a1"), 100).unwrap();
            old.append("shared.ns.b", jv("b1"), 101).unwrap();
            old.append("shared.ns.c", jv("c1"), 100).unwrap();
            old.append("shared.ns.c", jv("c2"), 100).unwrap();
        }
        // 模拟崩溃窗口：metadata 丢 id2 映射 → id2 成孤儿（1/3/4 映射在位）
        let degraded = serde_json::json!({
            "next_fact_id": 5,
            "fact_sources": {"1": 100, "3": 100, "4": 100},
            "used_at_startup": {},
            "rolled_up": [],
        });
        std::fs::write(&meta, degraded.to_string()).unwrap();

        let report = compact_shared_facts(&wal, &meta, &archive, CompactionMode::Discard).unwrap();
        assert!(report.executed);
        assert_eq!(report.orphans_carried, 1, "孤儿必须显式留痕");
        assert_eq!(report.released, 1, "c1 非最新 → 释放");
        assert_eq!(report.kept, 3);

        let fresh = SharedFactsLog::recover(&wal, &meta).unwrap();
        assert_eq!(fresh.version(), 3);
        // 载荷零丢失；携带归属 = 0（不伪造归属）
        let carried = fresh.fact_by_id(FactId(6)).unwrap();
        assert_eq!(carried.path, "shared.ns.b");
        assert_eq!(carried.value, jv("b1"));
        assert_eq!(carried.source_session_id, 0);
        assert_eq!(fresh.fact_by_id(FactId(5)).unwrap().value, jv("a1"));
        assert_eq!(fresh.fact_by_id(FactId(7)).unwrap().value, jv("c2"));
    }

    /// 验收锚 5：崩溃中间态两形态 recover 不 panic、无静默错配
    #[test]
    fn crash_middle_states_recover_without_panic_or_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let wal = dir.path().join("sf.wal");
        let meta = dir.path().join("sf_meta.json");

        // 中间态 A：meta 先行互换完成、WAL 未换 → 旧 WAL + 新 meta
        // （fact_sources 空 + next 续接旧空间）→ 孤儿告警 fail-visible，
        // id 空间零重叠 ⇒ 无静默错配
        {
            let old = SharedFactsLog::recover(&wal, &meta).unwrap();
            old.append("shared.ns.a", jv("a1"), 100).unwrap();
            old.append("shared.ns.b", jv("b1"), 101).unwrap();
        }
        let mid_meta = serde_json::json!({
            "next_fact_id": 3,
            "fact_sources": {},
            "used_at_startup": {},
            "rolled_up": [],
        });
        std::fs::write(&meta, mid_meta.to_string()).unwrap();
        {
            let log = SharedFactsLog::recover(&wal, &meta).unwrap();
            let orphans = log.verify_causal_consistency().unwrap();
            assert_eq!(orphans, vec![1, 2], "中间态 A 必须显式检出不可归因段");
        }

        // 中间态 B：WAL 缺位窗口（meta 在位、WAL 已改名走）→ 空账本可活，
        // 读面降级不 panic（id 空间零重叠 ⇒ 后续 append 不与旧 id 碰撞）
        let wal_moved = dir.path().join("sf.wal.pre-compact");
        std::fs::rename(&wal, &wal_moved).unwrap();
        let log2 = SharedFactsLog::recover(&wal, &meta).unwrap();
        assert_eq!(log2.version(), 0);
        assert!(log2.facts_by_path_prefix("").is_empty());
        log2.append("shared.ns.c", jv("c1"), 1).unwrap();
        let c1 = &log2.facts_by_path_prefix("shared.ns.c")[0];
        assert_eq!(c1.fact_id, FactId(3), "id 续接旧空间，与旧账本零重叠");
    }

    /// 验收锚 7：marker 执行位——archive/discard 模式 marker 解析与执行串联
    #[test]
    fn marker_runner_executes_requested_mode() {
        let dir = tempfile::tempdir().unwrap();
        let (wal, meta, _archive) = build_ledger(dir.path(), "sf");
        // 改名进执行位约定的标准文件名
        let std_wal = dir.path().join(SHARED_WAL_FILE);
        let std_meta = dir.path().join(SHARED_META_FILE);
        std::fs::rename(&wal, &std_wal).unwrap();
        std::fs::rename(&meta, &std_meta).unwrap();

        write_compaction_request(dir.path(), CompactionMode::Discard).unwrap();
        let report = run_requested_compaction(dir.path()).unwrap();
        assert!(report.executed);
        assert_eq!(report.released, 2);
        assert_eq!(report.mode.as_deref(), Some("discard"));
        // 成功清 marker + 写报告
        assert!(!dir.path().join(MARKER_FILE).exists());
        assert!(dir.path().join(REPORT_FILE).exists());
        // 归档按 discard 模式落盘
        let archive_content = std::fs::read_to_string(dir.path().join(ARCHIVE_FILE)).unwrap();
        assert!(archive_content.contains("\"mode\":\"discard\""));

        // 报告可回读且与返回值一致
        let persisted: CompactionReport =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join(REPORT_FILE)).unwrap())
                .unwrap();
        assert!(persisted.executed);
        assert_eq!(persisted.kept, 4);
    }
}

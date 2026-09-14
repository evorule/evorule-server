// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 快照包原子落盘（SSOT 单一实现）
//!
//! 审计⑥ 批 B（C5）: 落盘逻辑从 evorule-server 下沉至本 crate —— 发布链
//! （[`crate::publish_service::PublishService`]）与外部导入通道
//! （evorule-server `SessionApi::import_bundle`）共用一份实现，
//! 消除两份 land 实现漂移的通道（与 evorule-bundle 的 SSOT 原则一致）。
//!
//! # 落盘布局（数据资产化：物理隔离）
//! - 规则包：`{rules_dir}/bundles/{bundle_id}/{entry_id}.json`（rule_body 原样零转译）
//!   + `{rules_dir}/bundles/{bundle_id}/bundle_manifest.json`
//!     （版本语义/法规基准/哈希/条目→文件映射）；
//! - 数据包：`{knowledge_dir}/bundles/{bundle_id}/{entry_id}.json`（payload 原样）
//!   + 同构 manifest（条目映射含 schema_ref，D3）——与 rules_dir **物理隔离**，
//!     TCB loader 扫描路径天然不触碰数据文件（，blocker 消除）。
//!
//! # 原子性
//! - 临时目录写入 → rename 就位，写入失败清理临时目录，无半成品；
//! - 同 dataset 旧 bundle 单激活替换，rename 失败回滚恢复旧版。

use std::path::{Path, PathBuf};

use evorule_bundle::{DatasetBundle, ImportResult};
use serde::{Deserialize, Serialize};

use evorule_bundle::VersionSelectionMode;

/// 落盘 manifest（`{rules_dir}/bundles/{bundle_id}/bundle_manifest.json`，T3）
///
/// 记录单版本快照的运行配置元数据：版本语义（source_version/selection_mode/
/// resolved_version）+ 法规生效基准（law_ref.effective_from）+ 防篡改哈希 + 条目→文件映射。
/// 仅供溯源/运行配置读取，**不参与** loader 加载路径（loader 递归扫描条目 .json 原样加载）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleManifest {
    pub bundle_id: String,
    pub dataset_id: String,
    pub source_version: String,
    pub selection_mode: VersionSelectionMode,
    /// pinned 已解析版本；auto 运行时按事件日期解析（None）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_version: Option<String>,
    /// law_ref.effective_from 基准（auto 模式的运行配置元数据，T4 细化）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_from: Option<String>,
    /// 全包防篡改哈希（blake3）
    pub content_hash: String,
    /// 落盘完整性哈希（存量豁免清零 L2，2026-09-15）: 对 manifest 自身（除本字段）
    /// 的紧凑 serde 序列化（struct 声明序=确定性）经 evorule-hash 做 blake3（`blake3:` 前缀）。
    /// 落盘时计算写入；复验侧从盘面重算比对——**当前包完整性可离线复验**。
    /// 与 `content_hash`（导入时全包溯源哈希，对内存 DatasetBundle 计算，盘面不可重算）
    /// 分工不混用。条目字节经 `entry_files[].content_hash` 传递覆盖。
    /// 旧 manifest 缺字段 → None（serde default），复验跳过（零迁移，与批次F 条目哈希同策略）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landed_content_hash: Option<String>,
    /// 条目 → 落盘文件映射（`{entry_id}.json`）
    pub entry_files: Vec<EntryFileManifest>,
}

/// manifest 中的单条目文件映射
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryFileManifest {
    pub entry_id: String,
    pub file: String,
    /// UV-183 批次F（reload 防篡改）: 条目文件内容哈希（`blake3:hex`，SSOT
    /// 经 evorule-hash）。导入落盘时对文件字节计算；reload 时复验，失配
    /// fail-fast 拒载该条目。旧 manifest 缺字段 → None（serde default），
    /// 复验跳过——防护只对新增哈希生效，存量零迁移。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    /// Knowledge 条目：领域 JSON Schema 引用（D3）；Rule 条目省略（None 不序列化）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_ref: Option<String>,
    /// Knowledge 条目：领域分类（段2 P1 执行侧数据面过滤；Rule 条目省略）。
    /// serde default 兼容旧 manifest（缺字段 → None，过滤不命中但不报错）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Knowledge 条目：标签（段2 P1 执行侧数据面过滤；空省略）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// 条目文件哈希口径（UV-183 批次F，SSOT）：`blake3:hex` over 文件字节，
/// 统一经 evorule-hash（族 B 哈希纪律，禁自写 blake3）。落盘写入与 reload
/// 复验两侧共用同一函数，防口径漂移。
pub fn entry_file_hash(bytes: &[u8]) -> String {
    evorule_hash::prefixed(&evorule_hash::digest(bytes))
}

/// UV-183 批次F（reload 防篡改）: bundle 目录条目文件 blake3 复验。
///
/// 对 manifest.entry_files 中带 content_hash 的条目逐一重算落盘文件哈希并
/// 比对，返回失配条目文件名列表（含 manifest 记录但磁盘缺失的条目）。
/// 未记录哈希的条目（旧 manifest，零迁移）跳过。返回空 = 全部通过。
pub fn verify_bundle_entry_hashes(dir: &Path, manifest: &BundleManifest) -> Vec<String> {
    let mut mismatched = Vec::new();
    for e in &manifest.entry_files {
        let Some(expect) = e.content_hash.as_deref() else {
            continue;
        };
        let actual = std::fs::read(dir.join(&e.file))
            .map(|bytes| entry_file_hash(&bytes))
            .unwrap_or_default();
        if actual != expect {
            mismatched.push(e.file.clone());
        }
    }
    mismatched
}

impl BundleManifest {
    /// 落盘完整性哈希口径（L2，SSOT 经 evorule-hash，族 B 纪律禁自写 blake3）：
    /// 对排除 `landed_content_hash` 自身的 manifest 紧凑 serde 序列化
    /// （struct 声明序=确定性；`json_digest` 内含 BTreeMap 键序规范）做 blake3。
    /// 条目字节经 `entry_files[].content_hash` 传递覆盖 → 本哈希即「当前包完整性」。
    /// 与落盘写入、复验两侧共用同一实现，防口径漂移。
    pub fn compute_landed_hash(&self) -> String {
        let mut canonical = self.clone();
        canonical.landed_content_hash = None;
        evorule_hash::prefixed(&evorule_hash::json_digest(&canonical))
    }
}

/// 存量豁免清零 L2（2026-09-15）: bundle 目录落盘完整性复验。
///
/// 从盘面读取 bundle_manifest.json，重算 `landed_content_hash` 并与记录值比对：
/// - manifest 未记录该字段（旧存量，零迁移）→ `Ok(())`（跳过，不追溯）；
/// - 记录值 ≠ 重算值 → `Err`（调用方 fail-closed 处置，如拒载该 bundle 全部条目）。
/// 口径与落盘侧 `compute_landed_hash` 同源。
pub fn verify_bundle_landed_hash(dir: &Path) -> Result<(), String> {
    let raw = std::fs::read_to_string(dir.join(evorule_bundle::BUNDLE_MANIFEST_FILE))
        .map_err(|e| format!("manifest 读取失败: {e}"))?;
    let manifest: BundleManifest =
        serde_json::from_str(&raw).map_err(|e| format!("manifest 解析失败: {e}"))?;
    verify_landed_hash_recorded(&manifest)
}

/// 对已解析 manifest 做落盘完整性比对（调用方已持有解析结果时免二次读盘）。
pub fn verify_landed_hash_recorded(manifest: &BundleManifest) -> Result<(), String> {
    let Some(recorded) = manifest.landed_content_hash.as_deref() else {
        return Ok(()); // 旧存量零迁移
    };
    let actual = manifest.compute_landed_hash();
    if actual == recorded {
        Ok(())
    } else {
        Err(format!(
            "落盘完整性失配（疑似篡改/半成品）: 记录 {recorded} ≠ 重算 {actual}"
        ))
    }
}

/// 原子落盘：`{rules_dir}/bundles/{bundle_id}/{entry_id}.json`（rule_body 原样零转译）
/// + `bundle_manifest.json`（版本语义/法规基准/哈希/条目→文件映射）。
///
/// - 同 dataset 再次导入 → 替换旧 bundle 目录（单激活替换语义，T4 细化）；
/// - 写入失败 → 清理临时目录，无半成品；
/// - rename 失败 → 回滚恢复旧版。
pub fn land_bundle_atomically(
    rules_dir: &Path,
    bundle: &DatasetBundle,
    result: &ImportResult,
) -> Result<(), String> {
    land_bundle_core(&rules_dir.join("bundles"), bundle, result, false)
}

/// 原子落盘（数据资产通道）：`{knowledge_dir}/bundles/{bundle_id}/{entry_id}.json`
/// （`rule_body` 字段承载领域 payload，零转译）+ 同构 `bundle_manifest.json`
/// （条目映射含 `schema_ref`）。
///
/// 与 rules_dir **物理隔离**：TCB loader（load_rules_dir_transforms）只扫 rules_dir，
/// 数据文件不在其扫描路径上，天然不进 TCB 合并集——**零改动消除 blocker**。
/// 原子性/单激活替换语义与规则落盘完全同构（同一份核心实现）。
pub fn land_knowledge_bundle_atomically(
    knowledge_dir: &Path,
    bundle: &DatasetBundle,
    result: &ImportResult,
) -> Result<(), String> {
    land_bundle_core(&knowledge_dir.join("bundles"), bundle, result, true)
}

/// 落盘核心（规则/数据共用，防逻辑漂移）：
/// `with_schema_ref = true` 时 manifest 条目映射携带领域 schema 引用（knowledge 包）。
fn land_bundle_core(
    base: &Path,
    bundle: &DatasetBundle,
    result: &ImportResult,
    with_schema_ref: bool,
) -> Result<(), String> {
    if bundle.bundle_id.is_empty()
        || bundle.bundle_id.contains(['/', '\\'])
        || bundle.bundle_id.contains("..")
    {
        return Err(format!(
            "非法 bundle_id `{}`（拒绝路径穿越）",
            bundle.bundle_id
        ));
    }

    let target = base.join(&bundle.bundle_id);
    let tmp = base.join(format!(".{}.tmp", bundle.bundle_id));
    let backup = base.join(format!(".{}.bak", bundle.bundle_id));

    // 清理上次残留的临时/备份目录
    let _ = std::fs::remove_dir_all(&tmp);
    let _ = std::fs::remove_dir_all(&backup);
    std::fs::create_dir_all(&tmp).map_err(|e| format!("创建临时目录失败: {e}"))?;

    // 写入条目（任一失败 → 清理临时目录，不留半成品）
    let write_result = (|| -> Result<(), String> {
        // UV-183 批次F: 逐条记录落盘文件哈希（与 entries 同序,reload 复验基线）
        let mut entry_hashes: Vec<String> = Vec::with_capacity(bundle.entries.len());
        for entry in &bundle.entries {
            if entry.entry_id.is_empty()
                || entry.entry_id.contains(['/', '\\'])
                || entry.entry_id.contains("..")
            {
                return Err(format!(
                    "条目 `{}` 含非法路径字符（拒绝写入）",
                    entry.entry_id
                ));
            }
            let doc = serde_json::to_string_pretty(&entry.rule_body)
                .map_err(|e| format!("序列化条目 `{}` 失败: {e}", entry.entry_id))?;
            std::fs::write(tmp.join(format!("{}.json", entry.entry_id)), doc.as_bytes())
                .map_err(|e| format!("写入条目 `{}` 失败: {e}", entry.entry_id))?;
            entry_hashes.push(entry_file_hash(doc.as_bytes()));
        }
        // T3: 写 manifest（与条目同目录，随原子替换一并落盘）
        let manifest = BundleManifest {
            bundle_id: bundle.bundle_id.clone(),
            dataset_id: result.dataset_id.clone(),
            source_version: result.source_version.clone(),
            selection_mode: result.selection_mode,
            resolved_version: result.resolved_version.clone(),
            effective_from: bundle
                .dataset
                .law_ref
                .as_ref()
                .and_then(|l| l.effective_from.clone()),
            content_hash: bundle.audit.content_hash.clone(),
            // L2: 先 None 构造，再计算落盘完整性哈希（口径需排除本字段自身）
            landed_content_hash: None,
            entry_files: bundle
                .entries
                .iter()
                .zip(entry_hashes.iter())
                .map(|(e, h)| EntryFileManifest {
                    entry_id: e.entry_id.clone(),
                    file: format!("{}.json", e.entry_id),
                    // UV-183 批次F: reload 复验基线（blake3:hex over 落盘字节）
                    content_hash: Some(h.clone()),
                    schema_ref: if with_schema_ref {
                        Some(e.schema_ref.clone().unwrap_or_default())
                    } else {
                        None
                    },
                    // Q12 段2 P1：knowledge 条目携带 domain/tags，供执行侧数据面
                    // （/api/knowledge）与治理侧同语法过滤；rule 条目不携带（loader 不读）。
                    domain: if with_schema_ref {
                        Some(e.domain.clone())
                    } else {
                        None
                    },
                    tags: if with_schema_ref {
                        e.tags.clone()
                    } else {
                        Vec::new()
                    },
                })
                .collect(),
        };
        // L2（存量豁免清零）: 落盘完整性哈希（落盘写入与复验共用 compute_landed_hash 口径）
        let mut manifest = manifest;
        manifest.landed_content_hash = Some(manifest.compute_landed_hash());
        let manifest_json = serde_json::to_string_pretty(&manifest)
            .map_err(|e| format!("序列化 bundle_manifest.json 失败: {e}"))?;
        std::fs::write(
            tmp.join(evorule_bundle::BUNDLE_MANIFEST_FILE),
            manifest_json,
        )
        .map_err(|e| format!("写入 bundle_manifest.json 失败: {e}"))?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }

    // T4: 收集同 dataset 的旧 bundle 目录（不同 bundle_id），单激活替换
    let stale_dirs = find_same_dataset_stale_dirs(base, &result.dataset_id, &bundle.bundle_id);

    // 原子替换：旧版先移走为备份，新版本 rename 就位后清理备份；任一失败回滚
    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new(); // (原路径, 备份路径)
                                                         // 1) 同 bundle_id 旧目录
    if target.exists() {
        std::fs::rename(&target, &backup).map_err(|e| {
            let _ = std::fs::remove_dir_all(&tmp);
            format!("移走旧版 bundle 目录失败: {e}")
        })?;
        moved.push((target.clone(), backup.clone()));
    }
    // 2) 同 dataset 的其它旧 bundle 目录（单激活替换：仅最新激活）
    for (i, stale) in stale_dirs.iter().enumerate() {
        let stale_bak = base.join(format!(".stale.{}.{}", bundle.bundle_id, i));
        let _ = std::fs::remove_dir_all(&stale_bak);
        if let Err(e) = std::fs::rename(stale, &stale_bak) {
            rollback_bundle_moves(&moved);
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(format!(
                "移走同 dataset 旧 bundle `{}` 失败（已回滚）: {e}",
                stale.display()
            ));
        }
        moved.push((stale.clone(), stale_bak));
    }
    // 3) 新版本就位
    if let Err(e) = std::fs::rename(&tmp, &target) {
        rollback_bundle_moves(&moved);
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(format!("原子落盘 rename 失败（已回滚）: {e}"));
    }
    // 4) 清理备份（隐藏目录：loader 已跳过 `.` 前缀，残留不污染加载）
    for (_, bak) in &moved {
        let _ = std::fs::remove_dir_all(bak);
    }

    tracing::info!(
        bundle_id = %bundle.bundle_id,
        dataset_id = %result.dataset_id,
        entry_count = bundle.entries.len(),
        replaced_stale = stale_dirs.len(),
        "bundle 已原子落盘（单激活替换）"
    );
    Ok(())
}

/// 找出 base 下与指定 dataset 相同（且 bundle_id 不同）的旧 bundle 目录（单激活）。
///
/// 跳过隐藏目录（临时/备份目录）与不含 manifest 的目录；manifest 读取失败静默跳过。
fn find_same_dataset_stale_dirs(base: &Path, dataset_id: &str, bundle_id: &str) -> Vec<PathBuf> {
    let Ok(read_dir) = std::fs::read_dir(base) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in read_dir.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with('.') {
            continue; // 临时/备份目录
        }
        let manifest_path = p.join(evorule_bundle::BUNDLE_MANIFEST_FILE);
        if !manifest_path.is_file() {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        let Ok(m) = serde_json::from_str::<BundleManifest>(&raw) else {
            continue;
        };
        if m.dataset_id == dataset_id && m.bundle_id != bundle_id {
            out.push(p);
        }
    }
    out
}

/// 回滚 bundle 落盘的目录移动：按逆序把备份目录还原到原路径
fn rollback_bundle_moves(moved: &[(PathBuf, PathBuf)]) {
    for (orig, bak) in moved.iter().rev() {
        let _ = std::fs::rename(bak, orig);
    }
}

#[cfg(test)]
mod landed_hash_tests {
    use super::*;

    /// 测试返回 Result 以规避 clippy unwrap/expect/panic deny（仓内门禁 C5）
    type TestResult = Result<(), String>;

    fn sample_manifest() -> BundleManifest {
        BundleManifest {
            bundle_id: "bundle-t1-v1".to_string(),
            dataset_id: "ds-t1".to_string(),
            source_version: "1.0.0".to_string(),
            selection_mode: VersionSelectionMode::Pinned,
            resolved_version: None,
            effective_from: None,
            content_hash: "blake3:1111111111111111111111111111111111111111111111111111111111111111"
                .to_string(),
            landed_content_hash: None,
            entry_files: vec![EntryFileManifest {
                entry_id: "e1".to_string(),
                file: "e1.json".to_string(),
                content_hash: Some(
                    "blake3:2222222222222222222222222222222222222222222222222222222222222222"
                        .to_string(),
                ),
                schema_ref: None,
                domain: None,
                tags: Vec::new(),
            }],
        }
    }

    #[test]
    fn 落盘哈希_盘面往返重算一致() -> TestResult {
        let mut m = sample_manifest();
        m.landed_content_hash = Some(m.compute_landed_hash());
        // 模拟盘面：序列化 → 解析 → 重算（口径必须对 serde 往返稳定）
        let raw = serde_json::to_string(&m).map_err(|e| e.to_string())?;
        let parsed: BundleManifest = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        assert_eq!(parsed.landed_content_hash, m.landed_content_hash);
        verify_landed_hash_recorded(&parsed)
    }

    #[test]
    fn 落盘哈希_篡改任意字段_失配() {
        let mut m = sample_manifest();
        m.landed_content_hash = Some(m.compute_landed_hash());
        m.source_version = "9.9.9".to_string(); // 篡改 manifest 记录
        assert!(verify_landed_hash_recorded(&m).is_err());
        m.source_version = "1.0.0".to_string();
        m.entry_files.clear(); // 篡改条目映射（含条目哈希同改的绕过路径）
        assert!(verify_landed_hash_recorded(&m).is_err());
    }

    #[test]
    fn 落盘哈希_旧存量缺字段_跳过() {
        let m = sample_manifest(); // landed_content_hash: None（零迁移）
        assert!(verify_landed_hash_recorded(&m).is_ok());
    }
}

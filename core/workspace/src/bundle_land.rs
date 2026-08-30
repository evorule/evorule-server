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
//! # 落盘布局（Q12 数据资产化：物理隔离）
//! - 规则包：`{rules_dir}/bundles/{bundle_id}/{entry_id}.json`（rule_body 原样零转译）
//!   + `{rules_dir}/bundles/{bundle_id}/bundle_manifest.json`
//!   （T3: 版本语义/法规基准/哈希/条目→文件映射）；
//! - 数据包：`{knowledge_dir}/bundles/{bundle_id}/{entry_id}.json`（payload 原样）
//!   + 同构 manifest（条目映射含 schema_ref，D3）——与 rules_dir **物理隔离**，
//!   TCB loader 扫描路径天然不触碰数据文件（Q12 W1，blocker 消除）。
//!
//! # 原子性
//! - 临时目录写入 → rename 就位，写入失败清理临时目录，无半成品；
//! - 同 dataset 旧 bundle 单激活替换（T4），rename 失败回滚恢复旧版。

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
    /// 条目 → 落盘文件映射（`{entry_id}.json`）
    pub entry_files: Vec<EntryFileManifest>,
}

/// manifest 中的单条目文件映射
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryFileManifest {
    pub entry_id: String,
    pub file: String,
    /// Knowledge 条目：领域 JSON Schema 引用（Q12 D3）；Rule 条目省略（None 不序列化）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_ref: Option<String>,
    /// Knowledge 条目：领域分类（Q12 段2 P1 执行侧数据面过滤；Rule 条目省略）。
    /// serde default 兼容旧 manifest（缺字段 → None，过滤不命中但不报错）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Knowledge 条目：标签（Q12 段2 P1 执行侧数据面过滤；空省略）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// 原子落盘：`{rules_dir}/bundles/{bundle_id}/{entry_id}.json`（rule_body 原样零转译）
/// + `bundle_manifest.json`（T3：版本语义/法规基准/哈希/条目→文件映射）。
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

/// 原子落盘（Q12 W1 数据资产通道）：`{knowledge_dir}/bundles/{bundle_id}/{entry_id}.json`
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
            std::fs::write(tmp.join(format!("{}.json", entry.entry_id)), doc)
                .map_err(|e| format!("写入条目 `{}` 失败: {e}", entry.entry_id))?;
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
            entry_files: bundle
                .entries
                .iter()
                .map(|e| EntryFileManifest {
                    entry_id: e.entry_id.clone(),
                    file: format!("{}.json", e.entry_id),
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
        let manifest_json = serde_json::to_string_pretty(&manifest)
            .map_err(|e| format!("序列化 bundle_manifest.json 失败: {e}"))?;
        std::fs::write(tmp.join(evorule_bundle::BUNDLE_MANIFEST_FILE), manifest_json)
            .map_err(|e| format!("写入 bundle_manifest.json 失败: {e}"))?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }

    // T4: 收集同 dataset 的旧 bundle 目录（不同 bundle_id），单激活替换
    let stale_dirs = find_same_dataset_stale_dirs(&base, &result.dataset_id, &bundle.bundle_id);

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

/// 找出 base 下与指定 dataset 相同（且 bundle_id 不同）的旧 bundle 目录（T4 单激活）。
///
/// 跳过隐藏目录（临时/备份目录）与不含 manifest 的目录；manifest 读取失败静默跳过。
fn find_same_dataset_stale_dirs(
    base: &Path,
    dataset_id: &str,
    bundle_id: &str,
) -> Vec<PathBuf> {
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

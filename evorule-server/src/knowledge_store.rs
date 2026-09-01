// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! KnowledgeStore —— 执行侧数据资产库（Q12 W2/W3）
//!
//! 数据资产与规则资产**物理隔离**的执行侧消费通道：
//! - 落盘布局由 `evorule_workspace::bundle_land::land_knowledge_bundle_atomically`
//!   保证：`{knowledge_dir}/bundles/{bundle_id}/{entry_id}.json`（payload 原样零转译）
//!   + 同构 `bundle_manifest.json`（条目映射含 schema_ref）；
//! - 本模块在**启动/导入时**扫描该布局，构建进程内 BTreeMap 索引
//!   （`(dataset_id, entry_id)` → payload + schema_ref + 溯源），供原生服务
//!   （IoHandler 侧，如 RPSM）按 dataset_id/entry_id 直读——MVP 不做网络数据面；
//! - **load_rules_dir_transforms 零改动**：knowledge 目录与 rules_dir 物理隔离，
//!   TCB 加载路径天然不触碰数据文件（Q12 blocker 消除方式）。
//!
//! # fail-fast 口径
//! - 加载时 manifest 缺失/非法、条目文件缺失/非法 JSON → **显式 Err**（不静默跳过）：
//!   数据资产的完整性问题是磁盘篡改/损坏信号，掩盖即违背确定性原则；
//! - knowledge 目录不存在 → 空库（非错误：执行侧可以只跑规则不承载数据资产）；
//! - 领域 schema 解析（`lookup_domain_schema`）未命中 → 返回 None（调用方门禁显式拒绝）。
//!
//! # 溯源边界
//! manifest 的 content_hash / source_version 为管理元数据（墙钟旁路），
//! 不渗入 fact / 审计验证链（与 bundle_imports 溯源表同口径）。

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;
use utoipa::ToSchema;

/// 单条数据资产记录（进程内索引项，W3 直读单元；Q12 段2 P1 兼作数据面响应组件）
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct KnowledgeEntryRecord {
    pub dataset_id: String,
    pub entry_id: String,
    /// 领域结构化数据本体（落盘 `{entry_id}.json` 的原样内容，零转译）
    pub payload: serde_json::Value,
    /// 领域 JSON Schema 引用（D3；manifest 条目映射携带）
    pub schema_ref: Option<String>,
    /// 来源 bundle（溯源）
    pub bundle_id: String,
    /// 治理侧源版本（溯源）
    pub source_version: String,
    /// 领域分类（Q12 段2 P1：manifest 携带，数据面过滤用；旧 manifest → 空）
    pub domain: String,
    /// 标签（Q12 段2 P1：manifest 携带，数据面过滤用；旧 manifest → 空）
    pub tags: Vec<String>,
}

/// 执行侧数据资产库（进程内，BTreeMap 确定性索引）
#[derive(Debug, Default)]
pub struct KnowledgeStore {
    /// (dataset_id, entry_id) → 记录；BTreeMap 保证遍历顺序确定性
    entries: BTreeMap<(String, String), KnowledgeEntryRecord>,
    /// 已加载 bundle 数（可观测性）
    bundle_count: usize,
}

impl KnowledgeStore {
    /// 从磁盘加载：扫描 `{knowledge_dir}/bundles/*/bundle_manifest.json`。
    ///
    /// - 目录不存在 → 空库（Ok，执行侧可无数据资产运行）；
    /// - 任一 bundle manifest 非法 / 条目文件缺失或非法 JSON → 显式 Err（fail-fast，
    ///   错误信息含具体文件路径，供运维自愈定位）。
    pub fn load_from_disk(knowledge_dir: &Path) -> Result<Self, String> {
        let bundles_dir = knowledge_dir.join("bundles");
        if !bundles_dir.exists() {
            return Ok(Self::default());
        }

        let mut entries = BTreeMap::new();
        let mut bundle_count = 0usize;

        let read_dir =
            std::fs::read_dir(&bundles_dir).map_err(|e| format!("读取知识目录失败: {e}"))?;
        for item in read_dir.flatten() {
            let p = item.path();
            if !p.is_dir() {
                continue;
            }
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') {
                continue; // 临时/备份目录（落盘原子性残留，非激活 bundle）
            }
            let manifest_path = p.join(evorule_bundle::BUNDLE_MANIFEST_FILE);
            if !manifest_path.is_file() {
                return Err(format!(
                    "知识 bundle 目录 `{}` 缺少 bundle_manifest.json（疑似损坏或半成品）: 修复步骤：1) 核对治理侧导出的 bundle 是否完整复制；2) 删除该目录后经 /api/bundles/import 重新导入",
                    p.display()
                ));
            }
            let raw = std::fs::read_to_string(&manifest_path)
                .map_err(|e| format!("读取 manifest `{}` 失败: {e}", manifest_path.display()))?;
            let manifest: evorule_workspace::BundleManifest =
                serde_json::from_str(&raw).map_err(|e| {
                    format!(
                        "解析 manifest `{}` 失败（磁盘篡改或版本不兼容）: {e}",
                        manifest_path.display()
                    )
                })?;

            for ef in &manifest.entry_files {
                let entry_path = p.join(&ef.file);
                let entry_raw = std::fs::read_to_string(&entry_path).map_err(|e| {
                    format!(
                        "读取数据条目 `{}` 失败（manifest 声明与磁盘不一致）: {e}",
                        entry_path.display()
                    )
                })?;
                let payload: serde_json::Value = serde_json::from_str(&entry_raw).map_err(|e| {
                    format!(
                        "解析数据条目 `{}` 失败（非合法 JSON）: {e}",
                        entry_path.display()
                    )
                })?;
                let key = (manifest.dataset_id.clone(), ef.entry_id.clone());
                // 单 bundle 内 dataset_id+entry_id 唯一（bundle 门禁保证）；重复 = manifest 损坏
                if entries.contains_key(&key) {
                    return Err(format!(
                        "数据条目 `{}/{}` 在 bundle `{}` 内重复声明（manifest 损坏）",
                        manifest.dataset_id, ef.entry_id, manifest.bundle_id
                    ));
                }
                entries.insert(
                    key,
                    KnowledgeEntryRecord {
                        dataset_id: manifest.dataset_id.clone(),
                        entry_id: ef.entry_id.clone(),
                        payload,
                        schema_ref: ef.schema_ref.clone(),
                        bundle_id: manifest.bundle_id.clone(),
                        source_version: manifest.source_version.clone(),
                        domain: ef.domain.clone().unwrap_or_default(),
                        tags: ef.tags.clone(),
                    },
                );
            }
            bundle_count += 1;
        }

        Ok(Self {
            entries,
            bundle_count,
        })
    }

    /// 直读单条数据资产（W3：原生服务按 dataset_id/entry_id 取 payload）
    pub fn get(&self, dataset_id: &str, entry_id: &str) -> Option<&KnowledgeEntryRecord> {
        self.entries
            .get(&(dataset_id.to_string(), entry_id.to_string()))
    }

    /// 列出某数据集的全部数据条目（BTreeMap 序，确定性）
    pub fn list_dataset(&self, dataset_id: &str) -> Vec<&KnowledgeEntryRecord> {
        self.entries
            .range(
                (dataset_id.to_string(), String::new())
                    ..=(dataset_id.to_string(), "\u{10FFFF}".to_string()),
            )
            .map(|(_, v)| v)
            .collect()
    }

    /// 已加载条目总数
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 已加载 bundle 数
    pub fn bundle_count(&self) -> usize {
        self.bundle_count
    }

    /// 数据集级清单（Q12 段2 P1/S1：`GET /api/knowledge` 数据源）
    ///
    /// 按 dataset_id 聚合（BTreeMap 序，确定性）：来源 bundle 集合、条目数、
    /// schema_ref 集合。执行侧单激活语义下同 dataset 通常仅一个 bundle，
    /// 聚合口径容忍多 bundle 并存（历史/手工落盘布局）。
    pub fn list_datasets(&self) -> Vec<KnowledgeDatasetSummary> {
        let mut acc: BTreeMap<String, KnowledgeDatasetSummary> = BTreeMap::new();
        for rec in self.entries.values() {
            let s = acc
                .entry(rec.dataset_id.clone())
                .or_insert_with(|| KnowledgeDatasetSummary {
                    dataset_id: rec.dataset_id.clone(),
                    bundle_ids: Vec::new(),
                    entry_count: 0,
                    schema_refs: Vec::new(),
                });
            if !s.bundle_ids.contains(&rec.bundle_id) {
                s.bundle_ids.push(rec.bundle_id.clone());
            }
            s.entry_count += 1;
            if let Some(sr) = &rec.schema_ref {
                if !s.schema_refs.contains(sr) {
                    s.schema_refs.push(sr.clone());
                }
            }
        }
        acc.into_values().collect()
    }

    /// 进程内过滤检索（Q12 段2 P1/S1：`GET /api/knowledge/{ds}/entries` 数据源）
    ///
    /// 过滤语义与治理侧 `search_knowledge_entries` 同口径：
    /// - `dataset_id`：Some 时仅该数据集（None = 全库）；
    /// - `domain`：精确匹配（忽略 ASCII 大小写）；
    /// - `tags`：任一命中即保留（OR 语义，空列表不过滤）；
    /// - `q`：entry_id / schema_ref / bundle_id / payload 文本拼接小写包含匹配。
    ///
    /// 数据量级小（执行侧数据面），线性扫描 + BTreeMap 确定性序，不做索引。
    pub fn search(
        &self,
        dataset_id: Option<&str>,
        q: Option<&str>,
        domain: Option<&str>,
        tags: &[String],
    ) -> Vec<&KnowledgeEntryRecord> {
        let lower_q = q.map(|s| s.to_lowercase());
        let mut out = Vec::new();
        for rec in self.entries.values() {
            if let Some(ds) = dataset_id {
                if rec.dataset_id != ds {
                    continue;
                }
            }
            if let Some(d) = domain {
                if !rec.domain.eq_ignore_ascii_case(d) {
                    continue;
                }
            }
            if !tags.is_empty() && !tags.iter().any(|t| rec.tags.contains(t)) {
                continue;
            }
            if let Some(lq) = &lower_q {
                let hay = format!(
                    "{} {} {} {}",
                    rec.entry_id,
                    rec.schema_ref.as_deref().unwrap_or(""),
                    rec.bundle_id,
                    rec.payload
                )
                .to_lowercase();
                if !hay.contains(lq) {
                    continue;
                }
            }
            out.push(rec);
        }
        out
    }
}

/// 数据集级清单项（Q12 段2 P1/S1：`GET /api/knowledge` 响应元素）
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct KnowledgeDatasetSummary {
    pub dataset_id: String,
    /// 来源 bundle 集合（单激活语义下通常 1 个）
    pub bundle_ids: Vec<String>,
    pub entry_count: usize,
    /// 该数据集条目引用的领域 schema 集合（去重，BTreeMap 序）
    pub schema_refs: Vec<String>,
}

/// 领域 schema 解析（Q12 D3 执行侧注入点）：
/// 扫描 `{knowledge_dir}/domain_schemas/*.json`，以 schema `$id`（缺省文件名）为键索引。
/// 未命中返回 None（调用方门禁显式拒绝，不静默放行）。
///
/// 领域 schema 归领域仓所有（如 rpsm 场景 schema 由 rpsm 仓提供），执行侧由运维把
/// 领域 schema 文件放入该目录完成"宿主注入"。注意 `$id` 必须是合法 URI
/// （jsonschema 校验器强制，见 evorule-rule 同款约束说明）。
pub fn lookup_domain_schema_in(knowledge_dir: &Path, uri: &str) -> Option<serde_json::Value> {
    let dir = knowledge_dir.join("domain_schemas");
    let read_dir = std::fs::read_dir(&dir).ok()?;
    for item in read_dir.flatten() {
        let p = item.path();
        if p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let key = v
            .get("$id")
            .and_then(|i| i.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| {
                p.file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
        if key == uri {
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kn_bundle(dir: &Path, bundle_id: &str, dataset_id: &str, entry_id: &str, schema_ref: &str) {
        let bdir = dir.join("bundles").join(bundle_id);
        std::fs::create_dir_all(&bdir).unwrap();
        std::fs::write(bdir.join(format!("{entry_id}.json")), r#"{"mass":1.5}"#).unwrap();
        let manifest = serde_json::json!({
            "bundle_id": bundle_id,
            "dataset_id": dataset_id,
            "source_version": "v1",
            "selection_mode": "auto_by_effective_date",
            "content_hash": "blake3:test",
            "entry_files": [
                { "entry_id": entry_id, "file": format!("{entry_id}.json"), "schema_ref": schema_ref }
            ]
        });
        std::fs::write(
            bdir.join("bundle_manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn load_roundtrip_and_direct_read() {
        let tmp = tempfile::tempdir().unwrap();
        kn_bundle(
            tmp.path(),
            "bundle-ds-a-v1",
            "ds-rpsm-assets",
            "scn-001",
            "https://rpsm.example/schemas/body.json",
        );
        let store = KnowledgeStore::load_from_disk(tmp.path()).unwrap();
        assert_eq!(store.bundle_count(), 1);
        assert_eq!(store.len(), 1);
        let rec = store.get("ds-rpsm-assets", "scn-001").unwrap();
        assert_eq!(rec.payload, serde_json::json!({ "mass": 1.5 }));
        assert_eq!(
            rec.schema_ref.as_deref(),
            Some("https://rpsm.example/schemas/body.json")
        );
        assert_eq!(rec.bundle_id, "bundle-ds-a-v1");
        assert_eq!(store.list_dataset("ds-rpsm-assets").len(), 1);
        assert!(store.list_dataset("no-such").is_empty());
        assert!(store.get("ds-rpsm-assets", "nope").is_none());
    }

    #[test]
    fn missing_dir_yields_empty_store() {
        let tmp = tempfile::tempdir().unwrap();
        let store = KnowledgeStore::load_from_disk(&tmp.path().join("nope")).unwrap();
        assert!(store.is_empty());
    }

    #[test]
    fn corrupt_manifest_fails_fast() {
        let tmp = tempfile::tempdir().unwrap();
        let bdir = tmp.path().join("bundles").join("bundle-bad");
        std::fs::create_dir_all(&bdir).unwrap();
        std::fs::write(bdir.join("bundle_manifest.json"), "{ not json").unwrap();
        let err = KnowledgeStore::load_from_disk(tmp.path()).unwrap_err();
        assert!(
            err.contains("bundle_manifest.json"),
            "报错应含文件路径: {err}"
        );
    }

    #[test]
    fn missing_entry_file_fails_fast() {
        let tmp = tempfile::tempdir().unwrap();
        let bdir = tmp.path().join("bundles").join("bundle-x");
        std::fs::create_dir_all(&bdir).unwrap();
        let manifest = serde_json::json!({
            "bundle_id": "bundle-x",
            "dataset_id": "ds",
            "source_version": "v1",
            "selection_mode": "auto_by_effective_date",
            "content_hash": "blake3:test",
            "entry_files": [ { "entry_id": "gone", "file": "gone.json" } ]
        });
        std::fs::write(
            bdir.join("bundle_manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let err = KnowledgeStore::load_from_disk(tmp.path()).unwrap_err();
        assert!(err.contains("gone.json"), "报错应指向缺失条目: {err}");
    }

    #[test]
    fn domain_schema_lookup_by_id() {
        let tmp = tempfile::tempdir().unwrap();
        let ddir = tmp.path().join("domain_schemas");
        std::fs::create_dir_all(&ddir).unwrap();
        std::fs::write(
            ddir.join("body.json"),
            r#"{"$id":"https://rpsm.example/schemas/body.json","type":"object"}"#,
        )
        .unwrap();
        let hit = lookup_domain_schema_in(tmp.path(), "https://rpsm.example/schemas/body.json");
        assert!(hit.is_some());
        assert!(lookup_domain_schema_in(tmp.path(), "https://nope").is_none());
    }

    /// Q12 段2 P1/S1：list_datasets 聚合 + search 过滤矩阵（domain/tags/q 组合）
    #[test]
    fn list_datasets_and_search_filter_matrix() {
        let tmp = tempfile::tempdir().unwrap();
        // ds-a: domain=physics, tags=[spring,demo]
        kn_bundle(
            tmp.path(),
            "bundle-ds-a-v1",
            "ds-a",
            "scn-001",
            "https://rpsm.example/schemas/body.json",
        );
        // 给 ds-a 的 manifest 手工加 domain/tags（kn_bundle 不带，模拟新落盘格式）
        let ma = tmp
            .path()
            .join("bundles")
            .join("bundle-ds-a-v1")
            .join("bundle_manifest.json");
        let mut v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&ma).unwrap()).unwrap();
        v["entry_files"][0]["domain"] = serde_json::json!("physics");
        v["entry_files"][0]["tags"] = serde_json::json!(["spring", "demo"]);
        std::fs::write(&ma, serde_json::to_string_pretty(&v).unwrap()).unwrap();

        // ds-b: 旧格式 manifest（无 domain/tags 字段）→ 默认空
        kn_bundle(
            tmp.path(),
            "bundle-ds-b-v1",
            "ds-b",
            "mix-002",
            "https://x/schema.json",
        );

        let store = KnowledgeStore::load_from_disk(tmp.path()).unwrap();
        assert_eq!(store.len(), 2);

        // list_datasets：BTreeMap 序聚合
        let dss = store.list_datasets();
        assert_eq!(dss.len(), 2);
        assert_eq!(dss[0].dataset_id, "ds-a");
        assert_eq!(dss[0].bundle_ids, vec!["bundle-ds-a-v1"]);
        assert_eq!(dss[0].entry_count, 1);
        assert_eq!(
            dss[0].schema_refs,
            vec!["https://rpsm.example/schemas/body.json"]
        );

        // search：domain 命中 / 忽略大小写 / 不命中
        assert_eq!(
            store.search(Some("ds-a"), None, Some("physics"), &[]).len(),
            1
        );
        assert_eq!(
            store.search(Some("ds-a"), None, Some("PHYSICS"), &[]).len(),
            1
        );
        assert_eq!(store.search(Some("ds-a"), None, Some("chem"), &[]).len(), 0);

        // search：tags 任一命中（OR）
        assert_eq!(
            store
                .search(Some("ds-a"), None, None, &["demo".to_string()])
                .len(),
            1
        );
        assert_eq!(
            store
                .search(Some("ds-a"), None, None, &["nope".to_string()])
                .len(),
            0
        );

        // search：q 包含匹配（payload 文本）
        assert_eq!(store.search(Some("ds-a"), Some("mass"), None, &[]).len(), 1);
        assert_eq!(
            store.search(Some("ds-a"), Some("no-hit"), None, &[]).len(),
            0
        );

        // search：跨数据集过滤（ds-b 旧格式 domain 为空 → 不过滤维度下仍可见）
        assert_eq!(store.search(Some("ds-b"), None, None, &[]).len(), 1);
        assert_eq!(store.search(None, None, None, &[]).len(), 2);
    }
}

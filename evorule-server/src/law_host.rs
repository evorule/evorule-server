// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! law_set 装配模块 —— 独立部署件的运行时装配与逐域查询面
//!
//! # 定位
//! - law_set 为独立部署件，不经 rule_loader 装载路径（`core/rule_loader`
//!   lib.rs 明示「law_set 为独立部署件」）；本模块是 server 侧 law_set 的
//!   唯一运行时装配与查询面，消费 [`evorule_law_schema::validate_law_set`]
//!   （schema 三层：结构 + formula 恒真语义 + 量纲能力层）。
//! - 装配形态（MVP）：启动时扫描本地部署目录逐集装载；远端源（rule serve
//!   URL + dataset_id）为后续配置面扩展，本模块接口已按 dataset_id 命名空间
//!   预留，不阻塞远端登记。
//! - 逐集隔离：一库校验失败不阻断他库装载，装配报告逐集列明报因
//!   （fail-visible，拒绝静默降级）。
//! - 查询键 = `(dataset_id, law_id)` 双键：跨库同 id 允许（命名空间 =
//!   dataset_id），仅告警不拦截（判定只提案不改写）。

#![forbid(unsafe_code)]
// C5 (unwrap/expect/panic = deny) 仅约束生产代码;测试代码保留 unwrap 惯例
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use evorule_law_schema::{validate_law_set, Law, LawSet};
use serde::Serialize;

// ============================================================================
// 装配结论（呈现面：启动日志与伴跑 status 投影同源）
// ============================================================================

/// 单集装载结论（valid/rejected 两态统一结构；逐集列明，不静默）
#[derive(Debug, Clone, Serialize)]
pub struct LawSetStatus {
    /// 集标识（本地目录形态=文件名去 `.json` 后缀；远端源形态=登记 dataset_id），
    /// 作为本机 law 命名空间键（跨库同 id 的隔离边界）
    pub dataset_id: String,
    /// 来源路径（本地目录形态；远端形态=登记源描述）
    pub source: String,
    /// 是否通过全层校验（schema + formula 结构层 + 量纲能力层）
    pub valid: bool,
    /// law 条数（valid 集=实装条数；拒收集=可解析条数或 0）
    pub law_count: usize,
    /// warning 级结果数（量纲跳过等；不构成拒收）
    pub warning_count: usize,
    /// 错误数（>0 即拒收）
    pub error_count: usize,
    /// 逐条报因（拒收集列明；valid 集为空）
    pub errors: Vec<String>,
    /// 文档 blake3 锚（evorule-hash json_digest，族 B 哈希纪律；不可解析时 None）
    pub content_digest: Option<String>,
}

impl LawSetStatus {
    fn invalid(dataset_id: String, source: String, errors: Vec<String>) -> Self {
        LawSetStatus {
            dataset_id,
            source,
            valid: false,
            law_count: 0,
            warning_count: 0,
            error_count: errors.len(),
            errors,
            content_digest: None,
        }
    }
}

/// 目录装载报告：loaded（双键注册入查询面）+ rejected（隔离拒收，逐集报因）
#[derive(Debug, Default)]
pub struct LawHostLoadReport {
    pub loaded: Vec<LawSetStatus>,
    pub rejected: Vec<LawSetStatus>,
    /// 目录级失败（目录不存在/不可读等）。配置面已显式声明该目录，
    /// 装配失败必须可见（fail-visible），由启动报告列明，不静默吞掉。
    pub dir_error: Option<String>,
}

impl LawHostLoadReport {
    /// 全集结论迭代器（loaded + rejected，逐集列明口径）
    pub fn statuses(&self) -> impl Iterator<Item = &LawSetStatus> {
        self.loaded.iter().chain(self.rejected.iter())
    }

    pub fn loaded_count(&self) -> usize {
        self.loaded.len()
    }

    pub fn rejected_count(&self) -> usize {
        self.rejected.len()
    }
}

// ============================================================================
// 装配完成态（查询面）
// ============================================================================

#[derive(Debug)]
struct LoadedSet {
    status: LawSetStatus,
    laws: BTreeMap<String, Law>,
}

/// law_set 运行时装配与查询面（启动期构建一次，不可变共享）
///
/// 仅 valid 集注册入查询面；拒收集只存在于报告与 status 呈现面。
#[derive(Debug, Default)]
pub struct LawHost {
    sets: BTreeMap<String, LoadedSet>,
    /// 跨库同 law_id 告警（允许并存不拦截；判定只提案不改写）
    cross_set_id_warnings: Vec<String>,
}

impl LawHost {
    /// 扫描部署目录逐集装载（逐集隔离：单集失败不影响他集）
    ///
    /// 仅取目录直置 `*.json` 文件（不递归）；文件排序确定性（可回放）。
    /// 返回装配完成态与逐集报告。
    pub fn load_dir(dir: &Path) -> (LawHost, LawHostLoadReport) {
        let mut report = LawHostLoadReport::default();
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                report.dir_error = Some(format!("{}: {e}", dir.display()));
                return (LawHost::default(), report);
            }
        };

        // 收集 *.json 路径并排序（readdir 序不确定，装配序必须可回放）
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_file() && p.extension().map(|x| x == "json").unwrap_or(false))
            .collect();
        paths.sort();

        let mut host = LawHost::default();
        for path in paths {
            let dataset_id = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let source = path.display().to_string();
            let raw = match std::fs::read_to_string(&path) {
                Ok(r) => r,
                Err(e) => {
                    report.rejected.push(LawSetStatus::invalid(
                        dataset_id,
                        source,
                        vec![format!("读取失败: {e}")],
                    ));
                    continue;
                }
            };
            let doc: serde_json::Value = match serde_json::from_str(&raw) {
                Ok(d) => d,
                Err(e) => {
                    report.rejected.push(LawSetStatus::invalid(
                        dataset_id,
                        source,
                        vec![format!("JSON 解析失败: {e}")],
                    ));
                    continue;
                }
            };
            let digested = evorule_hash::try_json_digest(&doc).ok();
            let law_set: Option<LawSet> = serde_json::from_value(doc.clone()).ok();
            let law_count_declared = doc
                .get("laws")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let r = validate_law_set(&doc);
            let status = LawSetStatus {
                dataset_id: dataset_id.clone(),
                source: source.clone(),
                valid: r.valid,
                law_count: law_count_declared,
                warning_count: r.warnings.len(),
                error_count: r.errors.len(),
                errors: r.errors,
                content_digest: digested,
            };
            if !status.valid {
                report.rejected.push(status);
                continue;
            }
            // valid 集：按 law_id 建域内索引（集内 id 唯一由 schema/结构层保证）
            let mut laws = BTreeMap::new();
            if let Some(ls) = law_set {
                for law in ls.laws {
                    laws.insert(law.id.clone(), law);
                }
            }
            report.loaded.push(status.clone());
            host.sets.insert(dataset_id, LoadedSet { status, laws });
        }

        host.cross_set_id_warnings = Self::compute_cross_set_id_warnings(&host.sets);
        (host, report)
    }

    /// 跨库同 law_id 检测（告警不拦截：命名空间=dataset_id，双键不撞）
    fn compute_cross_set_id_warnings(sets: &BTreeMap<String, LoadedSet>) -> Vec<String> {
        // law_id -> 出现的 dataset_id 列表（BTreeMap 保证报告序确定性）
        let mut seen: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (ds, set) in sets {
            for law_id in set.laws.keys() {
                seen.entry(law_id).or_default().push(ds);
            }
        }
        seen.into_iter()
            .filter(|(_, owners)| owners.len() > 1)
            .map(|(law_id, owners)| {
                format!("law id `{law_id}` 跨 {n} 库同 id（命名空间隔离允许并存，仅告警不拦截）：{owners}", n = owners.len(), owners = owners.join(", "))
            })
            .collect()
    }

    /// 双键查询（dataset_id + law_id；跨库同 id 各归各库，不撞）
    pub fn lookup(&self, dataset_id: &str, law_id: &str) -> Option<&Law> {
        self.sets.get(dataset_id)?.laws.get(law_id)
    }

    /// 逐域查询（dataset_id 域内全部 law，id 序确定性）
    pub fn laws_in(&self, dataset_id: &str) -> Vec<&Law> {
        match self.sets.get(dataset_id) {
            Some(s) => s.laws.values().collect(),
            None => Vec::new(),
        }
    }

    /// 已注册集标识（BTreeMap 序确定性）
    pub fn dataset_ids(&self) -> Vec<&str> {
        self.sets.keys().map(|k| k.as_str()).collect()
    }

    /// 已注册集的呈现结论（status 投影同源；仅 valid 集）
    pub fn statuses(&self) -> Vec<LawSetStatus> {
        self.sets.values().map(|s| s.status.clone()).collect()
    }

    /// 跨库同 id 告警（装配期判定；启动报告列明）
    pub fn cross_set_id_warnings(&self) -> &[String] {
        &self.cross_set_id_warnings
    }
}

// ============================================================================
// 测试（装配/隔离/双键/真机门控）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn newton_doc() -> serde_json::Value {
        json!({
            "$schema": "https://evorule.org/schemas/law_set/v1.0.json",
            "kind": "law_set",
            "id": "newton-second-law",
            "version": "1.0.0",
            "laws": [{
                "id": "newton_second",
                "formula": "F = m * a",
                "symbols": { "F": "合外力", "m": "质量", "a": "加速度" },
                "si_units": { "F": "newton", "m": "kilogram", "a": "m*s^-2" },
                "solvable": ["F", "m", "a"],
                "tier": "light",
                "metadata": { "source": "Newton, Principia (1687)" }
            }]
        })
    }

    fn ideal_gas_doc() -> serde_json::Value {
        json!({
            "$schema": "https://evorule.org/schemas/law_set/v1.0.json",
            "kind": "law_set",
            "id": "ideal-gas-law",
            "version": "1.0.0",
            "laws": [{
                "id": "ideal_gas",
                "formula": "P * V = n * R * T",
                "symbols": {
                    "P": "压强", "V": "体积", "n": "物质的量",
                    "R": "理想气体常数", "T": "热力学温度"
                },
                "si_units": {
                    "P": "pascal", "V": "m^3", "n": "mol",
                    "R": "joule*mol^-1*K^-1", "T": "kelvin"
                }
            }]
        })
    }

    fn write_set(dir: &Path, name: &str, doc: &serde_json::Value) {
        std::fs::write(
            dir.join(format!("{name}.json")),
            serde_json::to_string_pretty(doc).unwrap(),
        )
        .unwrap();
    }

    // ── 多集装配 + 双键查询（验收 2/3 语义）──

    #[test]
    fn multi_set_load_and_dual_key_lookup() {
        let tmp = tempfile::tempdir().unwrap();
        write_set(tmp.path(), "law-mechanics", &newton_doc());
        write_set(tmp.path(), "law-thermo", &ideal_gas_doc());

        let (host, report) = LawHost::load_dir(tmp.path());
        assert_eq!(
            report.loaded_count(),
            2,
            "两集应全装载: {:?}",
            report.rejected
        );
        assert_eq!(report.rejected_count(), 0);
        assert!(report.dir_error.is_none());

        // 逐域可查（按域索引注册）
        assert_eq!(host.dataset_ids(), vec!["law-mechanics", "law-thermo"]);
        let mech = host.laws_in("law-mechanics");
        assert_eq!(mech.len(), 1);
        assert_eq!(mech[0].id, "newton_second");
        let thermo = host.laws_in("law-thermo");
        assert_eq!(thermo.len(), 1);
        assert_eq!(thermo[0].id, "ideal_gas");

        // 双键查询：同 law_id 在不同库各归各库
        assert!(host.lookup("law-mechanics", "newton_second").is_some());
        assert!(host.lookup("law-mechanics", "ideal_gas").is_none());
        assert!(host.lookup("law-thermo", "ideal_gas").is_some());
        assert!(host.lookup("law-thermo", "newton_second").is_none());

        // 呈现结论与查询面同源
        let statuses = host.statuses();
        assert_eq!(statuses.len(), 2);
        assert!(statuses.iter().all(|s| s.valid));
        assert!(statuses.iter().all(|s| s.content_digest.is_some()));
    }

    // ── 跨库同 id：允许并存 + 告警 + 双键不撞（D3 语义）──

    #[test]
    fn cross_set_same_law_id_allowed_with_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let mut a = newton_doc();
        a["laws"][0]["id"] = json!("shared_id");
        let mut b = ideal_gas_doc();
        b["laws"][0]["id"] = json!("shared_id");
        write_set(tmp.path(), "set-a", &a);
        write_set(tmp.path(), "set-b", &b);

        let (host, report) = LawHost::load_dir(tmp.path());
        assert_eq!(report.loaded_count(), 2, "跨库同 id 不拦截");
        assert_eq!(report.rejected_count(), 0);

        let warnings = host.cross_set_id_warnings();
        assert_eq!(warnings.len(), 1, "同 id 应恰产一条告警: {warnings:?}");
        assert!(warnings[0].contains("shared_id") && warnings[0].contains("仅告警"));

        // 双键各归各库（同 id 不撞）
        let a_law = host.lookup("set-a", "shared_id").unwrap();
        let b_law = host.lookup("set-b", "shared_id").unwrap();
        assert_ne!(a_law.formula, b_law.formula, "两库同名 law 各自独立");
    }

    // ── 逐集隔离：坏库拒收报因 + 他库零牵连（验收 4 语义）──

    #[test]
    fn bad_set_isolated_good_set_unaffected() {
        let tmp = tempfile::tempdir().unwrap();
        // 坏条目 1：formula 引用未声明符号（结构层拒收）
        let mut bad1 = newton_doc();
        bad1["laws"][0]["formula"] = json!("F = m * v");
        // 坏条目 2：量纲不一致（能力层拒收）
        let mut bad2 = ideal_gas_doc();
        bad2["laws"][0]["si_units"]["T"] = json!("second");
        // 坏集 3：schema 违例（缺 si_units 必填键）
        let mut bad3 = newton_doc();
        bad3["laws"][0].as_object_mut().unwrap().remove("si_units");
        write_set(tmp.path(), "bad-structure", &bad1);
        write_set(tmp.path(), "bad-dimensional", &bad2);
        write_set(tmp.path(), "bad-schema", &bad3);
        write_set(tmp.path(), "good-set", &ideal_gas_doc());
        // 坏集 4：非 JSON 垃圾文件
        std::fs::write(tmp.path().join("bad-garbage.json"), "{not json").unwrap();

        let (host, report) = LawHost::load_dir(tmp.path());
        assert_eq!(report.loaded_count(), 1, "仅健康集装载");
        assert_eq!(report.rejected_count(), 4, "四坏集逐集隔离拒收");
        assert_eq!(host.dataset_ids(), vec!["good-set"]);

        // 逐集报因可辨（拒收原因逐集列明，不静默）
        for st in &report.rejected {
            assert!(
                !st.valid && st.error_count > 0,
                "{} 应有报因: {:?}",
                st.dataset_id,
                st.errors
            );
        }
        let structure = report
            .rejected
            .iter()
            .find(|s| s.dataset_id == "bad-structure")
            .unwrap();
        assert!(structure.errors.iter().any(|e| e.contains("未声明符号")));
        let dim = report
            .rejected
            .iter()
            .find(|s| s.dataset_id == "bad-dimensional")
            .unwrap();
        assert!(dim.errors.iter().any(|e| e.contains("量纲不一致")));
        let schema = report
            .rejected
            .iter()
            .find(|s| s.dataset_id == "bad-schema")
            .unwrap();
        assert!(schema.errors.iter().any(|e| e.contains("si_units")));
        let garbage = report
            .rejected
            .iter()
            .find(|s| s.dataset_id == "bad-garbage")
            .unwrap();
        assert!(garbage.errors.iter().any(|e| e.contains("JSON 解析失败")));

        // 他库零牵连：健康集双键可查
        assert!(host.lookup("good-set", "ideal_gas").is_some());
    }

    // ── 目录级 fail-visible ──

    #[test]
    fn missing_dir_reported_not_silent() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("no-such-dir");
        let (host, report) = LawHost::load_dir(&missing);
        assert_eq!(host.dataset_ids().len(), 0);
        assert_eq!(report.loaded_count() + report.rejected_count(), 0);
        assert!(report.dir_error.is_some(), "目录不存在必须可见");
        let err = report.dir_error.unwrap();
        assert!(err.contains("no-such-dir"));
    }

    #[test]
    fn empty_dir_loads_zero_sets() {
        let tmp = tempfile::tempdir().unwrap();
        let (host, report) = LawHost::load_dir(tmp.path());
        assert_eq!(report.loaded_count(), 0);
        assert_eq!(report.rejected_count(), 0);
        assert!(report.dir_error.is_none());
        assert_eq!(host.dataset_ids().len(), 0);
    }

    // ── 真机门控测试（env 开门才跑；门关空跑不计时，取证须报开门耗时差）──
    //
    // EVORULE_LAW_HOST_E2E_DIR=<law_set 部署目录> 时对真机部署件装载取证
    // （两库并存 + 逐集 valid + 双键可查）。门关时跳过（报绿不算验证）。

    #[test]
    fn evmachine_deployment_dir_loads_all_sets() {
        let dir = match std::env::var("EVORULE_LAW_HOST_E2E_DIR") {
            Ok(d) if !d.is_empty() => d,
            _ => {
                eprintln!(
                    "SKIP evmachine_deployment_dir_loads_all_sets（门关：EVORULE_LAW_HOST_E2E_DIR 未配置）"
                );
                return;
            }
        };
        let (host, report) = LawHost::load_dir(Path::new(&dir));
        assert!(
            report.dir_error.is_none(),
            "真机目录必须可读: {:?}",
            report.dir_error
        );
        assert!(
            report.loaded_count() >= 2,
            "真机目录应含 ≥2 库（力学+热学）: loaded={} rejected={:?}",
            report.loaded_count(),
            report.rejected
        );
        assert!(
            report.rejected.is_empty(),
            "真机部署件不应有拒收集: {:?}",
            report.rejected
        );
        for st in &report.loaded {
            assert!(st.valid, "{} 应 valid: {:?}", st.dataset_id, st.errors);
            assert!(
                st.error_count == 0 && st.warning_count == 0,
                "真机集应零错零警: {:?}",
                st
            );
            assert!(st.content_digest.is_some(), "blake3 锚应存在");
        }
        // 逐域可查：每集至少 1 条 law
        for ds in host.dataset_ids() {
            assert!(!host.laws_in(ds).is_empty(), "{ds} 域内应可查");
        }

        // 跨库同 id 双键取证（真机物料驱动）：取一份真机部署件复制为探针集
        // （dataset_id 改缀但 law id 原样），同 id 跨库并存应告警不拦截，
        // 双键查询各归各库（同 id 不撞）。
        let probe_tmp = tempfile::tempdir().unwrap();
        let first = &report.loaded[0].dataset_id;
        let first_raw =
            std::fs::read_to_string(Path::new(&dir).join(format!("{first}.json"))).unwrap();
        let probe_doc: serde_json::Value = serde_json::from_str(&first_raw).unwrap();
        let probe_law_id = probe_doc["laws"][0]["id"].as_str().unwrap().to_string();
        let probe_formula = probe_doc["laws"][0]["formula"]
            .as_str()
            .unwrap()
            .to_string();
        std::fs::write(probe_tmp.path().join(format!("{first}.json")), &first_raw).unwrap();
        std::fs::write(
            probe_tmp.path().join("probe-dup-id.json"),
            serde_json::to_string_pretty(&probe_doc).unwrap(),
        )
        .unwrap();
        let (probe_host, probe_report) = LawHost::load_dir(probe_tmp.path());
        assert_eq!(
            probe_report.loaded_count(),
            2,
            "同 id 探针集应并存装载: {:?}",
            probe_report.rejected
        );
        assert!(
            probe_host
                .cross_set_id_warnings()
                .iter()
                .any(|w| w.contains(&probe_law_id)),
            "跨库同 id 应告警: {:?}",
            probe_host.cross_set_id_warnings()
        );
        let origin = probe_host.lookup(first, &probe_law_id).unwrap();
        let dup = probe_host.lookup("probe-dup-id", &probe_law_id).unwrap();
        assert_eq!(
            origin.formula, probe_formula,
            "原集双键应查到原物料 formula"
        );
        assert_eq!(
            dup.formula, probe_formula,
            "探针集双键应查到同 formula（同 id 不同命名空间并存不撞）"
        );
    }
}

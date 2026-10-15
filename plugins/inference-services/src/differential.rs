// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 规则集差分（读写集结构化 diff，一阶行为近似）。
//!
//! 对两个规则集各跑一次静态读写集提取（复用 [`crate::rw_extract::extract`]），
//! 输出集合级结构化差异：新增 / 消失的写路径、读路径、I/O 边；
//! 来源标签一致且等长时附逐下标规则体 diff。
//!
//! # 保守方向声明（本服务的判定口径）
//!
//! - 写路径不同 ⇒ 行为**必**不同（写是效果的必要载体）；
//! - 读路径 / I/O 边不同 ⇒ 行为**可能**不同（多报方向，不漏报）；
//! - 读写集全同 ⇒ **不做任何等价断言**（静态面一致不蕴含行为一致，
//!   行为级判定走等价探针服务）；
//! - 任一侧存在动态写目标 / 动态域规则：静态面不完整，报告显式标注
//!   （undetermined 不静默）。

#![forbid(unsafe_code)]

use std::collections::BTreeSet;

use evorule_tcb::JsonValue;

use crate::rw_extract::{self, RuleRw};

/// I/O 边的规范键：`"{io_type}/{service_name}"`，`service_name` 静态不可知时为 `*`。
fn io_key(io: &rw_extract::IoEdge) -> String {
    let service = io.service_name.as_deref().unwrap_or("*");
    format!("{}/{}", io.io_type, service)
}

/// 集合级差异桶
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SetDiff {
    /// 仅第二套规则集写的路径（规范化形态）
    pub added_writes: Vec<String>,
    /// 仅第一套规则集写的路径
    pub removed_writes: Vec<String>,
    /// 仅第二套规则集读的路径
    pub added_reads: Vec<String>,
    /// 仅第一套规则集读的路径
    pub removed_reads: Vec<String>,
    /// 仅第二套规则集持有的 I/O 边（规范键形态）
    pub added_ios: Vec<String>,
    /// 仅第一套规则集持有的 I/O 边
    pub removed_ios: Vec<String>,
}

/// 逐下标规则体差异（仅来源标签一致且等长时计算）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDiff {
    /// 合并列表下标
    pub index: usize,
    /// 来源标签（两侧一致才有意义）
    pub source: String,
}

/// 差分报告
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DifferentialReport {
    /// 集合级读写/I-O 差异
    pub set_diff: SetDiff,
    /// 是否做了逐下标 diff（来源标签一致且等长）
    pub aligned: bool,
    /// 规则体不同的下标（`aligned` 时有；逐下标，升序）
    pub changed_indices: Vec<IndexDiff>,
    /// 第一套规则集中带动态写目标的规则数
    pub set1_dynamic_writes: usize,
    /// 第二套规则集中带动态写目标的规则数
    pub set2_dynamic_writes: usize,
    /// 第一套规则集中域为路径引用（静态不可判定）的规则数
    pub set1_dynamic_domains: usize,
    /// 第二套规则集中域为路径引用的规则数
    pub set2_dynamic_domains: usize,
    /// 保守方向注记（确定性生成）
    pub note: String,
}

fn union_writes(rws: &[RuleRw]) -> BTreeSet<String> {
    rws.iter().flat_map(|r| r.writes.iter().cloned()).collect()
}

fn union_reads(rws: &[RuleRw]) -> BTreeSet<String> {
    rws.iter().flat_map(|r| r.reads.iter().cloned()).collect()
}

fn union_ios(rws: &[RuleRw]) -> BTreeSet<String> {
    rws.iter().flat_map(|r| r.ios.iter().map(io_key)).collect()
}

fn set_diff_strs(a: &BTreeSet<String>, b: &BTreeSet<String>) -> (Vec<String>, Vec<String>) {
    let added: Vec<String> = b.difference(a).cloned().collect();
    let removed: Vec<String> = a.difference(b).cloned().collect();
    (added, removed)
}

/// 执行规则集差分（见模块文档）。
///
/// # Errors
///
/// 透传读写集提取的全部错误（规则/来源长度不一致、规则数超限、
/// 指令树深度超限）。
pub fn diff_rulesets(
    rules1: &[JsonValue],
    sources1: &[String],
    rules2: &[JsonValue],
    sources2: &[String],
    include_constitution: bool,
) -> Result<DifferentialReport, String> {
    let rws1 = rw_extract::extract(rules1, sources1, include_constitution)?;
    let rws2 = rw_extract::extract(rules2, sources2, include_constitution)?;

    let (added_writes, removed_writes) = set_diff_strs(&union_writes(&rws1), &union_writes(&rws2));
    let (added_reads, removed_reads) = set_diff_strs(&union_reads(&rws1), &union_reads(&rws2));
    let (added_ios, removed_ios) = set_diff_strs(&union_ios(&rws1), &union_ios(&rws2));

    let aligned = rules1.len() == rules2.len() && sources1 == sources2;
    let changed_indices: Vec<IndexDiff> = if aligned {
        rules1
            .iter()
            .zip(rules2.iter())
            .enumerate()
            .filter_map(|(i, (a, b))| {
                (a != b).then_some(IndexDiff {
                    index: i,
                    source: sources1[i].clone(),
                })
            })
            .collect()
    } else {
        Vec::new()
    };

    let set1_dynamic_writes = rws1.iter().filter(|r| r.write_dyn).count();
    let set2_dynamic_writes = rws2.iter().filter(|r| r.write_dyn).count();
    let set1_dynamic_domains = rws1.iter().filter(|r| r.domain_dyn).count();
    let set2_dynamic_domains = rws2.iter().filter(|r| r.domain_dyn).count();

    let note = build_note(
        &added_writes,
        &removed_writes,
        &added_reads,
        &removed_reads,
        &added_ios,
        &removed_ios,
        set1_dynamic_writes + set2_dynamic_writes,
        set1_dynamic_domains + set2_dynamic_domains,
    );

    Ok(DifferentialReport {
        set_diff: SetDiff {
            added_writes,
            removed_writes,
            added_reads,
            removed_reads,
            added_ios,
            removed_ios,
        },
        aligned,
        changed_indices,
        set1_dynamic_writes,
        set2_dynamic_writes,
        set1_dynamic_domains,
        set2_dynamic_domains,
        note,
    })
}

/// 保守方向注记（确定性生成）。
#[allow(clippy::too_many_arguments)]
fn build_note(
    added_writes: &[String],
    removed_writes: &[String],
    added_reads: &[String],
    removed_reads: &[String],
    added_ios: &[String],
    removed_ios: &[String],
    dynamic_writes: usize,
    dynamic_domains: usize,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !added_writes.is_empty() || !removed_writes.is_empty() {
        parts.push("写路径存在差异: 行为必然不同".to_string());
    } else if !added_reads.is_empty()
        || !removed_reads.is_empty()
        || !added_ios.is_empty()
        || !removed_ios.is_empty()
    {
        parts.push("读路径或 I/O 边存在差异: 行为可能不同(保守多报,不漏报)".to_string());
    } else {
        parts.push("静态读写面全同: 不做任何等价断言(行为级判定走等价探针服务)".to_string());
    }
    if dynamic_writes > 0 {
        parts.push(format!(
            "{dynamic_writes} 条动态写目标规则: 写位置运行时才定形,静态面不完整"
        ));
    }
    if dynamic_domains > 0 {
        parts.push(format!(
            "{dynamic_domains} 条动态域规则: 域可达性静态不可判定,差异面可能被低估"
        ));
    }
    parts.join("；")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn set_rule(attr: &str, value: i64) -> JsonValue {
        JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("attr", JsonValue::string(attr)),
                    ("operation", JsonValue::string("set")),
                    ("value", JsonValue::Integer(value)),
                ]),
            ),
        ])
    }

    fn io_rule(io_type: &str) -> JsonValue {
        JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("exists")),
                            ("path", JsonValue::string("__exec__.payload.flag")),
                        ]),
                    ),
                    (
                        "on_true",
                        JsonValue::Array(vec![JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("io_request")),
                            (
                                "params",
                                JsonValue::object_from_pairs(&[(
                                    "io_type",
                                    JsonValue::string(io_type),
                                )]),
                            ),
                        ])]),
                    ),
                ]),
            ),
        ])
    }

    fn srcs(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("rules/r{i}.json")).collect()
    }

    #[test]
    fn write_changed_implies_behavior_differs() {
        let rules1 = vec![set_rule("x", 1)];
        let rules2 = vec![set_rule("y", 1)];
        let src = srcs(1);
        let report = diff_rulesets(&rules1, &src, &rules2, &src, false).unwrap();
        assert!(report
            .set_diff
            .added_writes
            .contains(&"payload.y".to_string()));
        assert!(report
            .set_diff
            .removed_writes
            .contains(&"payload.x".to_string()));
        assert!(report.note.contains("必然不同"));
        assert!(report.aligned);
        assert_eq!(report.changed_indices.len(), 1);
    }

    #[test]
    fn identical_sets_refuse_equivalence_claim() {
        let rules = vec![set_rule("x", 1)];
        let src = srcs(1);
        let report = diff_rulesets(&rules, &src, &rules, &src, false).unwrap();
        assert!(report.set_diff.added_writes.is_empty());
        assert!(report.set_diff.removed_writes.is_empty());
        assert!(report.note.contains("不做任何等价断言"));
        assert!(report.changed_indices.is_empty());
    }

    #[test]
    fn io_edge_diff_detected() {
        let rules1 = vec![io_rule("call_external")];
        let rules2 = vec![io_rule("call_service")];
        let src = srcs(1);
        let report = diff_rulesets(&rules1, &src, &rules2, &src, false).unwrap();
        assert!(report
            .set_diff
            .added_ios
            .iter()
            .any(|k| k.starts_with("call_service/")));
        assert!(report
            .set_diff
            .removed_ios
            .iter()
            .any(|k| k.starts_with("call_external/")));
        assert!(report.note.contains("可能不同"));
    }

    #[test]
    fn unaligned_sets_skip_index_diff() {
        let rules1 = vec![set_rule("x", 1)];
        let rules2 = vec![set_rule("x", 1), set_rule("y", 2)];
        let src1 = srcs(1);
        let src2 = srcs(2);
        let report = diff_rulesets(&rules1, &src1, &rules2, &src2, false).unwrap();
        assert!(!report.aligned);
        assert!(report.changed_indices.is_empty());
        assert!(report
            .set_diff
            .added_writes
            .contains(&"payload.y".to_string()));
    }

    #[test]
    fn dynamic_write_flagged_not_silent() {
        // 动态写:attr 为状态路径引用(宪法 increment 形态)
        let dyn_rule = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "attr",
                        JsonValue::string("__exec__.instruction.params.attr"),
                    ),
                    ("operation", JsonValue::string("add")),
                    ("value", JsonValue::Integer(1)),
                ]),
            ),
        ]);
        let rules1 = vec![dyn_rule.clone()];
        let rules2 = vec![dyn_rule];
        let src = srcs(1);
        let report = diff_rulesets(&rules1, &src, &rules2, &src, false).unwrap();
        assert_eq!(report.set1_dynamic_writes, 1);
        assert_eq!(report.set2_dynamic_writes, 1);
        assert!(report.note.contains("静态面不完整"));
    }
}

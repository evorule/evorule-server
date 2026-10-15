// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 反事实差异分析（双前向仿真 + 终态可观测面 diff）。
//!
//! 同一规则集分别以原始输入文档与假设输入文档各跑一次前向链仿真
//! （复用 [`crate::forward_sim::simulate`]，enforce 前置门 / 不动点 /
//! io 诚实断流语义全量对齐），对比两次终态在**可观测面**上的差异，
//! 并给出由结构差异机械生成的确定性解释。
//!
//! # 可观测面定义
//!
//! 终态文档展平为叶路径 → 值后，剔除任何路径段以 `__` 开头的内部面
//! （如 `__io_results__`）；其余业务路径参与 diff。空对象 / 空数组
//! 视为叶值保留（结构在场信息不丢）。控制面差异（终止原因 / I/O 断流 /
//! enforce 短路是否发生）不与值面混算，单独进解释。
//!
//! # 诚实边界
//!
//! - 仿真边界同前向链仿真：无 I/O 响应 / 无并发 / 无治理面干预，
//!   不动点 ≠ 真实系统行为。
//! - 只对比终态，不做步级归因（哪条规则在哪一步引入差异）——
//!   属于升级方向（需前向仿真暴露步级快照）。
//! - 差异条目数有硬上限，超出截断并显式标注（分析工具不是全量导出器）。

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use evorule_tcb::JsonValue;

use crate::forward_sim::{self, SimulationReport};

/// 可观测面差异条目硬上限（超出截断并显式标注）
pub const MAX_DIFF_ENTRIES: usize = 128;

/// 终态文档展平为叶路径 → 值（对象键 `.` 连接，数组下标 `[i]`）。
///
/// 任何路径段以 `__` 开头视为内部面，不进可观测面；空对象 / 空数组
/// 作为叶值保留在展平结果中（结构在场信息不丢）。
pub fn flatten_observable(document: &JsonValue) -> BTreeMap<String, JsonValue> {
    let mut out = BTreeMap::new();
    flatten_walk(document, String::new(), &mut out);
    out
}

fn is_internal_segment(segment: &str) -> bool {
    segment.starts_with("__")
}

fn flatten_walk(value: &JsonValue, prefix: String, out: &mut BTreeMap<String, JsonValue>) {
    match value {
        JsonValue::Object(map) => {
            if map.is_empty() {
                out.insert(prefix, value.clone());
                return;
            }
            for (key, child) in map {
                if is_internal_segment(key) {
                    continue;
                }
                let next = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten_walk(child, next, out);
            }
        }
        JsonValue::Array(items) => {
            if items.is_empty() {
                out.insert(prefix, value.clone());
                return;
            }
            for (i, child) in items.iter().enumerate() {
                let next = format!("{prefix}[{i}]");
                flatten_walk(child, next, out);
            }
        }
        other => {
            out.insert(prefix, other.clone());
        }
    }
}

/// 单条可观测面差异
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathDiff {
    /// 业务叶路径（展平形态）
    pub path: String,
    /// `added` / `removed` / `changed`（相对原始输入侧）
    pub kind: &'static str,
    /// 原始输入侧值（`added` 时无）
    pub original_value: Option<JsonValue>,
    /// 假设输入侧值（`removed` 时无）
    pub hypothetical_value: Option<JsonValue>,
}

/// 两份可观测面 map 的差异（路径字典序，确定性）。
///
/// 返回 `(差异列表, 是否因条目上限截断)`。
pub fn diff_observable(
    original: &BTreeMap<String, JsonValue>,
    hypothetical: &BTreeMap<String, JsonValue>,
) -> (Vec<PathDiff>, bool) {
    let mut diffs = Vec::new();
    let mut truncated = false;
    for (path, value) in original {
        if diffs.len() >= MAX_DIFF_ENTRIES {
            truncated = true;
            break;
        }
        match hypothetical.get(path) {
            None => diffs.push(PathDiff {
                path: path.clone(),
                kind: "removed",
                original_value: Some(value.clone()),
                hypothetical_value: None,
            }),
            Some(hv) if hv != value => diffs.push(PathDiff {
                path: path.clone(),
                kind: "changed",
                original_value: Some(value.clone()),
                hypothetical_value: Some(hv.clone()),
            }),
            Some(_) => {}
        }
    }
    if !truncated {
        for (path, value) in hypothetical {
            if original.contains_key(path) {
                continue;
            }
            if diffs.len() >= MAX_DIFF_ENTRIES {
                truncated = true;
                break;
            }
            diffs.push(PathDiff {
                path: path.clone(),
                kind: "added",
                original_value: None,
                hypothetical_value: Some(value.clone()),
            });
        }
    }
    (diffs, truncated)
}

/// 反事实分析报告
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CounterfactualReport {
    /// 原始输入侧仿真报告
    pub original: SimulationReport,
    /// 假设输入侧仿真报告
    pub hypothetical: SimulationReport,
    /// 可观测面差异（路径字典序）
    pub diffs: Vec<PathDiff>,
    /// 差异是否因条目上限截断
    pub truncated: bool,
    /// 确定性解释（由结构差异机械生成，无自然语言推断）
    pub explanation: String,
}

/// 执行反事实分析（见模块文档）。
///
/// # Errors
///
/// 透传前向链仿真的全部错误（规则/来源长度不一致、规则数超限、
/// 步数超硬上限、规则执行失败）。
pub fn analyze(
    rules: &[JsonValue],
    sources: &[String],
    original_document: &JsonValue,
    hypothetical_document: &JsonValue,
    max_steps: usize,
) -> Result<CounterfactualReport, String> {
    let original = forward_sim::simulate(rules, sources, original_document, max_steps)?;
    let hypothetical = forward_sim::simulate(rules, sources, hypothetical_document, max_steps)?;
    let (diffs, truncated) = diff_observable(
        &flatten_observable(&original.final_document),
        &flatten_observable(&hypothetical.final_document),
    );
    let explanation = explain(&original, &hypothetical, &diffs, truncated);
    Ok(CounterfactualReport {
        original,
        hypothetical,
        diffs,
        truncated,
        explanation,
    })
}

/// 控制面签名是否一致（终止原因 / I/O 断流 / enforce 短路）
fn control_equal(a: &SimulationReport, b: &SimulationReport) -> bool {
    a.stopped_reason == b.stopped_reason
        && a.blocked_on_io == b.blocked_on_io
        && a.halted == b.halted
}

/// 由结构差异机械生成解释（确定性：同输入必得同文本）。
fn explain(
    original: &SimulationReport,
    hypothetical: &SimulationReport,
    diffs: &[PathDiff],
    truncated: bool,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    match (&original.blocked_on_io, &hypothetical.blocked_on_io) {
        (Some((o_id, o_io)), Some((h_id, h_io))) if o_id != h_id || o_io != h_io => {
            parts.push(format!(
                "控制面差异: 两侧 I/O 断流点不同(原始 {o_id}/{o_io} vs 假设 {h_id}/{h_io})"
            ));
        }
        (Some(_), None) | (None, Some(_)) => {
            parts
                .push("控制面差异: I/O 断流发生面不同(一侧触发 io_request,一侧未触发)".to_string());
        }
        _ => {}
    }
    if original.halted.is_some() != hypothetical.halted.is_some() {
        parts.push("控制面差异: enforce 短路发生面不同".to_string());
    }
    if original.stopped_reason != hypothetical.stopped_reason {
        parts.push(format!(
            "终止原因不同: {} vs {}",
            original.stopped_reason, hypothetical.stopped_reason
        ));
    }
    if truncated {
        parts.push(format!(
            "可观测面差异超过 {MAX_DIFF_ENTRIES} 条,已截断(其余差异未列出)"
        ));
    }
    if diffs.is_empty() {
        if control_equal(original, hypothetical) {
            parts.push("可观测面与控制面均无差异: 输入变化未传播".to_string());
        } else {
            parts.push("可观测面值无差异(控制面差异见上)".to_string());
        }
    } else {
        parts.push(format!("可观测面差异 {} 条(详见 diffs)", diffs.len()));
    }
    parts.join("；")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn flatten_skips_internal_segments() {
        let doc = JsonValue::object_from_pairs(&[
            (
                "payload",
                JsonValue::object_from_pairs(&[("a", JsonValue::Integer(1))]),
            ),
            (
                "__io_results__",
                JsonValue::object_from_pairs(&[("call_external", JsonValue::Integer(9))]),
            ),
        ]);
        let flat = flatten_observable(&doc);
        assert_eq!(flat.len(), 1);
        assert!(flat.contains_key("payload.a"));
        assert!(!flat.keys().any(|k| k.contains("__")));
    }

    #[test]
    fn flatten_keeps_empty_containers() {
        let doc = JsonValue::object_from_pairs(&[
            ("empty_obj", JsonValue::object_from_pairs(&[])),
            ("empty_arr", JsonValue::Array(vec![])),
        ]);
        let flat = flatten_observable(&doc);
        assert_eq!(flat.len(), 2);
        assert!(flat.contains_key("empty_obj"));
        assert!(flat.contains_key("empty_arr"));
    }

    #[test]
    fn flatten_array_indices() {
        let doc = JsonValue::object_from_pairs(&[(
            "queue",
            JsonValue::Array(vec![JsonValue::Integer(1), JsonValue::Integer(2)]),
        )]);
        let flat = flatten_observable(&doc);
        assert!(flat.contains_key("queue[0]"));
        assert!(flat.contains_key("queue[1]"));
    }

    #[test]
    fn diff_kinds_and_order() {
        let original = flatten_observable(&JsonValue::object_from_pairs(&[
            ("a", JsonValue::Integer(1)),
            ("b", JsonValue::string("x")),
            ("c", JsonValue::Bool(true)),
        ]));
        let hypothetical = flatten_observable(&JsonValue::object_from_pairs(&[
            ("a", JsonValue::Integer(2)),
            ("c", JsonValue::Bool(true)),
            ("d", JsonValue::string("new")),
        ]));
        let (diffs, truncated) = diff_observable(&original, &hypothetical);
        assert!(!truncated);
        let kinds: Vec<(&str, &str)> = diffs.iter().map(|d| (d.path.as_str(), d.kind)).collect();
        assert_eq!(
            kinds,
            vec![("a", "changed"), ("b", "removed"), ("d", "added")]
        );
    }

    #[test]
    fn diff_truncation_capped() {
        let mut map1 = std::collections::BTreeMap::new();
        let mut map2 = std::collections::BTreeMap::new();
        for i in 0..200 {
            map1.insert(format!("p{i}"), JsonValue::Integer(1));
            map2.insert(format!("p{i}"), JsonValue::Integer(2));
        }
        let original = flatten_observable(&JsonValue::Object(map1));
        let hypothetical = flatten_observable(&JsonValue::Object(map2));
        let (diffs, truncated) = diff_observable(&original, &hypothetical);
        assert!(truncated);
        assert_eq!(diffs.len(), MAX_DIFF_ENTRIES);
    }

    #[test]
    fn analyze_no_propagation_explanation() {
        // 常量覆盖规则:无条件把 payload.seed 写成 1 —— 初始差异被覆写,
        // 终态可观测面收敛
        let rule = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("attr", JsonValue::string("seed")),
                    ("operation", JsonValue::string("set")),
                    ("value", JsonValue::Integer(1)),
                ]),
            ),
        ]);
        let sources = vec!["rules/t.json".to_string()];
        let mk_doc = |seed: i64| {
            JsonValue::object_from_pairs(&[
                (
                    "payload",
                    JsonValue::object_from_pairs(&[("seed", JsonValue::Integer(seed))]),
                ),
                ("instruction", JsonValue::object_from_pairs(&[])),
                ("queue", JsonValue::Array(vec![])),
            ])
        };
        let report = analyze(&[rule], &sources, &mk_doc(5), &mk_doc(9), 16).unwrap();
        assert!(report.diffs.is_empty());
        assert!(!report.truncated);
        assert!(report.explanation.contains("均无差异"));
        assert_eq!(report.original.stopped_reason, "fixpoint");
    }

    #[test]
    fn analyze_propagating_change() {
        // 条件规则:存在 payload.flag 才写 payload.out=1 ——
        // 有 flag 一侧多出 out,无 flag 一侧保持
        let rule = JsonValue::object_from_pairs(&[
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
                            ("type", JsonValue::string("set")),
                            (
                                "params",
                                JsonValue::object_from_pairs(&[
                                    ("attr", JsonValue::string("out")),
                                    ("operation", JsonValue::string("set")),
                                    ("value", JsonValue::Integer(1)),
                                ]),
                            ),
                        ])]),
                    ),
                ]),
            ),
        ]);
        let sources = vec!["rules/t.json".to_string()];
        let doc_with = JsonValue::object_from_pairs(&[
            (
                "payload",
                JsonValue::object_from_pairs(&[("flag", JsonValue::Bool(true))]),
            ),
            ("instruction", JsonValue::object_from_pairs(&[])),
            ("queue", JsonValue::Array(vec![])),
        ]);
        let doc_without = JsonValue::object_from_pairs(&[
            ("payload", JsonValue::object_from_pairs(&[])),
            ("instruction", JsonValue::object_from_pairs(&[])),
            ("queue", JsonValue::Array(vec![])),
        ]);
        let report = analyze(&[rule], &sources, &doc_with, &doc_without, 16).unwrap();
        let mut paths: Vec<&str> = report.diffs.iter().map(|d| d.path.as_str()).collect();
        paths.sort_unstable();
        // 空容器按叶值保留:doc_without 的 payload 为空对象 → "payload" 叶差异
        assert_eq!(paths, vec!["payload", "payload.flag", "payload.out"]);
        assert!(report
            .diffs
            .iter()
            .any(|d| d.path == "payload.out" && d.kind == "removed"));
        assert!(report.explanation.contains("3 条"));
    }
}

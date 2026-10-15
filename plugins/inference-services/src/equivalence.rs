// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 等价探针（结构比较 + 有限探针行为证据）。
//!
//! 两个规则集的等价性分两级判定：
//!
//! ① **结构级**：规则列表与来源标签逐元素相等（列表序即语义序，
//!    不做重排归一）。结构全同 ⇒ 同一确定性执行语义下行为必然全同，
//!    直接判 `structurally_identical`，探针无需运行。
//! ② **行为级**：在调用方提供的探针输入集上，两侧各跑一次前向链仿真，
//!    逐探针对比终态可观测面与控制面（终止原因 / I/O 断流 / enforce 短路）。
//!
//! # 诚实边界（本服务的语义核心）
//!
//! - `probe_equivalent` **≠ 数学证明**：只断言"在给定 N 个探针上未发现
//!   差异"，不断言全输入域等价。探针集由调用方提供，覆盖面责任在探针
//!   提供方。
//! - 结构不同 + 探针全同 → `probe_equivalent`（证据语义）；
//!   结构不同 + 未提供探针 → `structural_difference_unprobed`
//!   （不断言等价，也不断言不等价）。
//! - 行为等价的严格判定需要 SMT 等形式化工具，本服务不做该声明。
//! - 探针数有硬上限（防资源滥用）。

#![forbid(unsafe_code)]

use evorule_tcb::JsonValue;

use crate::counterfactual::flatten_observable;
use crate::forward_sim::{self, SimulationReport};

/// 探针数量硬上限
pub const MAX_PROBES: usize = 32;

/// 首个行为反例
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Counterexample {
    /// 反例探针下标（探针输入数组中的位置）
    pub probe_index: usize,
    /// 差异类别摘要（确定性生成）
    pub summary: String,
}

/// 等价探针报告
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EquivalenceReport {
    /// `structurally_identical` / `probe_equivalent` / `differed` /
    /// `structural_difference_unprobed`
    pub verdict: &'static str,
    /// 结构级是否全同
    pub structural_identical: bool,
    /// 实际运行的探针数（结构全同短路时为 0）
    pub probes_run: usize,
    /// 首个行为反例（`differed` 时有）
    pub counterexample: Option<Counterexample>,
    /// 语义注记（确定性生成；`probe_equivalent` 必附证据语义声明）
    pub note: String,
}

/// 执行等价探针（见模块文档）。
///
/// # Errors
///
/// - 任一侧规则/来源长度不一致、规则数超限、步数超硬上限
/// - 任一规则 TCB 执行错误（透传前向链仿真错误）
/// - 探针数超 [`MAX_PROBES`] 或探针形态非法（须为对象）
pub fn compare_rulesets(
    rules1: &[JsonValue],
    sources1: &[String],
    rules2: &[JsonValue],
    sources2: &[String],
    probe_inputs: &[JsonValue],
) -> Result<EquivalenceReport, String> {
    if probe_inputs.len() > MAX_PROBES {
        return Err(format!(
            "探针数 {} 超过上限 {MAX_PROBES} — 自诊断指引: 探针是证据不是普查,\
             覆盖面靠探针选择而非数量堆叠",
            probe_inputs.len()
        ));
    }
    for (i, probe) in probe_inputs.iter().enumerate() {
        if !matches!(probe, JsonValue::Object(_)) {
            return Err(format!(
                "探针 #{i} 须为对象 — 自诊断指引: 探针输入是合成 __exec__ 文档,\
                 形如 {{\"instruction\":..., \"payload\":..., \"queue\":[...]}}"
            ));
        }
    }

    let structural_identical = rules1 == rules2 && sources1 == sources2;
    if structural_identical {
        return Ok(EquivalenceReport {
            verdict: "structurally_identical",
            structural_identical: true,
            probes_run: 0,
            counterexample: None,
            note: "结构全同: 同一确定性执行语义下行为必然全同,探针未运行(无需)".to_string(),
        });
    }

    for (i, probe) in probe_inputs.iter().enumerate() {
        let a = forward_sim::simulate(rules1, sources1, probe, forward_sim::DEFAULT_MAX_STEPS)?;
        let b = forward_sim::simulate(rules2, sources2, probe, forward_sim::DEFAULT_MAX_STEPS)?;
        if let Some(summary) = probe_difference(&a, &b) {
            return Ok(EquivalenceReport {
                verdict: "differed",
                structural_identical: false,
                probes_run: i + 1,
                counterexample: Some(Counterexample {
                    probe_index: i,
                    summary,
                }),
                note: "结构不同且探针发现行为差异;首个反例见 counterexample".to_string(),
            });
        }
    }

    if probe_inputs.is_empty() {
        Ok(EquivalenceReport {
            verdict: "structural_difference_unprobed",
            structural_identical: false,
            probes_run: 0,
            counterexample: None,
            note: "结构不同且未提供探针: 不能断言行为不同,也不能断言等价".to_string(),
        })
    } else {
        Ok(EquivalenceReport {
            verdict: "probe_equivalent",
            structural_identical: false,
            probes_run: probe_inputs.len(),
            counterexample: None,
            note: format!(
                "在 {} 个探针上未发现差异;这是证据语义而非数学证明,\
                 全输入域等价性不在本服务声明范围内",
                probe_inputs.len()
            ),
        })
    }
}

/// 单探针行为差异检测：可观测面值 → 控制面（终止原因 / I/O 断流 / enforce 短路）。
fn probe_difference(a: &SimulationReport, b: &SimulationReport) -> Option<String> {
    if flatten_observable(&a.final_document) != flatten_observable(&b.final_document) {
        return Some("终态可观测面值差异".to_string());
    }
    if a.stopped_reason != b.stopped_reason {
        return Some(format!(
            "终止原因差异: {} vs {}",
            a.stopped_reason, b.stopped_reason
        ));
    }
    if a.blocked_on_io != b.blocked_on_io {
        return Some("I/O 断流面差异".to_string());
    }
    if a.halted != b.halted {
        return Some("enforce 短路面差异".to_string());
    }
    None
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

    fn branch_set(flag_path: &str, attr: &str, value: i64) -> JsonValue {
        JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("exists")),
                            ("path", JsonValue::string(flag_path)),
                        ]),
                    ),
                    ("on_true", JsonValue::Array(vec![set_rule(attr, value)])),
                ]),
            ),
        ])
    }

    fn doc(flag: bool) -> JsonValue {
        let payload = if flag {
            JsonValue::object_from_pairs(&[("flag", JsonValue::Bool(true))])
        } else {
            JsonValue::object_from_pairs(&[])
        };
        JsonValue::object_from_pairs(&[
            ("payload", payload),
            ("instruction", JsonValue::object_from_pairs(&[])),
            ("queue", JsonValue::Array(vec![])),
        ])
    }

    fn srcs(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("rules/r{i}.json")).collect()
    }

    #[test]
    fn structurally_identical_short_circuits() {
        let rules = vec![set_rule("x", 1)];
        let src = srcs(1);
        let report =
            compare_rulesets(&rules, &src, &rules, &src, &[doc(true), doc(false)]).unwrap();
        assert_eq!(report.verdict, "structurally_identical");
        assert_eq!(report.probes_run, 0, "结构全同无需探针");
    }

    #[test]
    fn differed_with_counterexample() {
        // 常量值不同:探针一跑即分
        let rules1 = vec![set_rule("x", 1)];
        let rules2 = vec![set_rule("x", 2)];
        let src = srcs(1);
        let report = compare_rulesets(&rules1, &src, &rules2, &src, &[doc(true)]).unwrap();
        assert_eq!(report.verdict, "differed");
        let cx = report.counterexample.expect("有反例");
        assert_eq!(cx.probe_index, 0);
        assert!(cx.summary.contains("可观测面"));
    }

    #[test]
    fn probe_equivalent_is_evidence_not_proof() {
        // 行为不同但探针打不到:两侧分别条件写在不同 flag 上,
        // 探针不带任何 flag → 两侧都中性 → 探针全同(证据语义)
        let rules1 = vec![branch_set("__exec__.payload.flag", "y", 1)];
        let rules2 = vec![branch_set("__exec__.payload.other", "x", 1)];
        let src = srcs(1);
        let report = compare_rulesets(&rules1, &src, &rules2, &src, &[doc(false)]).unwrap();
        assert_eq!(report.verdict, "probe_equivalent");
        assert_eq!(report.probes_run, 1);
        assert!(report.note.contains("证据语义"));
        // 同一对规则集,换打到差异的探针 → differed
        let mut hit = doc(false);
        if let Some(JsonValue::Object(payload)) = hit.get_mut("payload") {
            payload.insert("flag".to_string(), JsonValue::Bool(true));
        }
        let report2 = compare_rulesets(&rules1, &src, &rules2, &src, &[doc(false), hit]).unwrap();
        assert_eq!(report2.verdict, "differed");
        assert_eq!(report2.counterexample.expect("有反例").probe_index, 1);
    }

    #[test]
    fn unprobed_structural_difference() {
        let rules1 = vec![set_rule("x", 1)];
        let rules2 = vec![set_rule("x", 2)];
        let src = srcs(1);
        let report = compare_rulesets(&rules1, &src, &rules2, &src, &[]).unwrap();
        assert_eq!(report.verdict, "structural_difference_unprobed");
        assert!(report.counterexample.is_none());
    }

    #[test]
    fn probe_cap_enforced() {
        let rules1 = vec![set_rule("x", 1)];
        let rules2 = vec![set_rule("x", 2)];
        let src = srcs(1);
        let probes: Vec<JsonValue> = (0..=MAX_PROBES).map(|_| doc(false)).collect();
        let err = compare_rulesets(&rules1, &src, &rules2, &src, &probes).unwrap_err();
        assert!(err.contains("超过上限"));
    }

    #[test]
    fn probe_shape_validated() {
        let rules1 = vec![set_rule("x", 1)];
        let rules2 = vec![set_rule("x", 2)];
        let src = srcs(1);
        let err =
            compare_rulesets(&rules1, &src, &rules2, &src, &[JsonValue::Integer(1)]).unwrap_err();
        assert!(err.contains("须为对象"));
    }
}

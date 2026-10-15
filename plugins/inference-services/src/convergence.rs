// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 收敛检测（112 阶段 3，R-2 裁定口径：与 cycle_detect 去重）。
//!
//! 算法底稿 = v4 `convergence.rs` 的五检查框架；现行模型下的取舍：
//! - **保留**：① 自修改条件（no-termination 静态代理：规则写自身域读路径）
//!   ② 步数上界估计（静态无环 → 最长链 = 最少遍历趟数上界）。
//! - **不做**：v4 的 PriorityStarvation。列表序语义下"前序写 + 后序域读
//!   同路径"是普通顺序数据依赖（后序本就该看到前序的写），独立饥饿检查
//!   会把一切正常链误报为风险——饥饿语义由 conflict 的 shadow_risk
//!   （顺序遮蔽）承担，本模块不重复、不重算（实施期发现，替代 R-2 初稿
//!   的"列表序饥饿重定义"方案）。
//! - **不重复实现**：self-loop / cycle 两项与 [`crate::cycle`] 完全同图
//!   同判定——本模块内部直接调用 [`crate::cycle::detect_cycles`]，输出
//!   引用其计数，不另立环类型学。
//! - **剥除**：PriorityStarvation（现行模型无 priority 字段，已按上重定义）。
//!
//! # 诚实边界
//!
//! "收敛"指静态可判定子集：存在保守层问题（conservative cycles /
//! 动态写）时 `converges=false` 且 `undetermined=true`——静态判不了
//! 的一律显式输出，不静默给绿。

#![forbid(unsafe_code)]

use crate::cycle::detect_cycles;
use crate::rw_extract::RuleRw;

/// 收敛问题
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvergenceIssue {
    /// `self_modifying` / `list_order_starvation` / `static_cycle` /
    /// `conservative_cycle`
    pub issue_type: &'static str,
    /// 涉事规则身份（环问题为空串，环成员在 description 里列全）
    pub rule_id: String,
    pub description: String,
    /// true = 保守层发现（静态判定不了，非确定性风险）
    pub conservative: bool,
}

/// 收敛报告
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvergenceReport {
    /// 静态可判定子集上无问题（保守层有问题时为 false 且 undetermined=true）
    pub converges: bool,
    /// 保守层问题存在 → 静态无法下结论
    pub undetermined: bool,
    /// 最少遍历趟数上界（静态无环时 = 静态边图最长链；有环 / 保守环 → None）
    pub estimated_passes: Option<usize>,
    pub issues: Vec<ConvergenceIssue>,
    /// 豁免桶规则身份（不参与判定，透明透出）
    pub excluded: Vec<String>,
}

/// 收敛检测（见模块文档）。
///
/// # Errors
///
/// 环检测内部超限（规则数 / 邻接表规模）时上抛。
pub fn check(rws: &[RuleRw]) -> Result<ConvergenceReport, String> {
    let excluded: Vec<String> = rws
        .iter()
        .filter(|rw| rw.excluded)
        .map(|rw| rw.id.clone())
        .collect();
    let live: Vec<&RuleRw> = rws.iter().filter(|rw| !rw.excluded).collect();
    let mut issues: Vec<ConvergenceIssue> = Vec::new();
    let mut undetermined = false;

    // ① 自修改条件（no-termination 静态代理）：写集 ∩ 自身域读集 ≠ ∅
    for rw in &live {
        let overlap: Vec<&String> = rw.writes.iter().filter(|w| rw.reads.contains(*w)).collect();
        if !overlap.is_empty() {
            issues.push(ConvergenceIssue {
                issue_type: "self_modifying",
                rule_id: rw.id.clone(),
                description: format!(
                    "规则 {} 写自身域读路径 {:?} — 反复施加可能永不满足不动点",
                    rw.id, overlap
                ),
                conservative: false,
            });
        }
    }

    // ② 环（复用 cycle_detect，不另立类型学）
    let cycle_report = detect_cycles(rws)?;
    for c in &cycle_report.static_cycles {
        issues.push(ConvergenceIssue {
            issue_type: "static_cycle",
            rule_id: String::new(),
            description: format!("静态环: {}", c.join(" -> ")),
            conservative: false,
        });
    }
    for c in &cycle_report.conservative_cycles {
        undetermined = true;
        issues.push(ConvergenceIssue {
            issue_type: "conservative_cycle",
            rule_id: String::new(),
            description: format!("保守环（动态边，静态判不了终止性）: {}", c.join(" -> ")),
            conservative: true,
        });
    }

    // ③ 动态写 → 终止性静态不可判定
    for rw in &live {
        if rw.write_dyn {
            undetermined = true;
            issues.push(ConvergenceIssue {
                issue_type: "conservative_cycle", // 归入保守类（动态写目标 = 图形状未知）
                rule_id: rw.id.clone(),
                description: format!("规则 {} 动态写目标 — 静态图不完整，终止性不可判定", rw.id),
                conservative: true,
            });
        }
    }

    // ⑤ 步数上界：静态无环时 = 静态边图最长链（DAG 最长路径，边 = write→read）；
    //    有静态环或存在保守层问题时 None（链长无意义 / 不可判定）
    let estimated_passes = if undetermined || !cycle_report.static_cycles.is_empty() {
        None
    } else {
        Some(longest_chain(&live))
    };

    let converges = issues.is_empty();
    Ok(ConvergenceReport {
        converges,
        undetermined,
        estimated_passes,
        issues,
        excluded,
    })
}

/// 静态边图（a→b 当 a 的写 ∈ b 的读，仅 Static 边）上的最长链（节点数计）。
fn longest_chain(live: &[&RuleRw]) -> usize {
    let n = live.len();
    // adj[i] = 后继集合
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, a) in live.iter().enumerate() {
        for (j, b) in live.iter().enumerate() {
            if i == j {
                continue;
            }
            if a.writes.iter().any(|w| b.reads.contains(w)) {
                adj[i].push(j);
            }
        }
    }
    // DAG 最长路径：记忆化 DFS（此分支仅在无静态环时到达；保守兜底 = 深度上限）
    let mut memo: Vec<Option<usize>> = vec![None; n];
    let mut on_stack = vec![false; n];
    fn dfs(
        i: usize,
        adj: &[Vec<usize>],
        memo: &mut [Option<usize>],
        on_stack: &mut [bool],
    ) -> usize {
        if let Some(v) = memo[i] {
            return v;
        }
        if on_stack[i] {
            return 1; // 环兜底（不应到达：调用方已过滤静态环）
        }
        on_stack[i] = true;
        let mut best = 1;
        for &j in &adj[i] {
            best = best.max(1 + dfs(j, adj, memo, on_stack));
        }
        on_stack[i] = false;
        memo[i] = Some(best);
        best
    }
    (0..n)
        .map(|i| dfs(i, &adj, &mut memo, &mut on_stack))
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::rw_extract::extract;
    use evorule_tcb::JsonValue;

    fn obj(pairs: &[(&str, JsonValue)]) -> JsonValue {
        JsonValue::object_from_pairs(pairs)
    }

    fn set_rule(attr: &str, value: i64) -> JsonValue {
        obj(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                obj(&[
                    ("attr", JsonValue::string(attr)),
                    ("value", JsonValue::Integer(value)),
                ]),
            ),
        ])
    }

    /// branch(exists/eq 域) → on_true[set]
    fn cond_set(path: &str, attr: &str, value: i64) -> JsonValue {
        obj(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                obj(&[
                    (
                        "domain",
                        obj(&[
                            ("type", JsonValue::string("exists")),
                            ("path", JsonValue::string(path)),
                        ]),
                    ),
                    ("on_true", JsonValue::Array(vec![set_rule(attr, value)])),
                ]),
            ),
        ])
    }

    fn rws_of(rules: &[JsonValue]) -> Vec<RuleRw> {
        let sources: Vec<String> = (0..rules.len())
            .map(|i| format!("rules/r{i}.json"))
            .collect();
        extract(rules, &sources, false).unwrap()
    }

    #[test]
    fn clean_ruleset_converges_with_pass_bound() {
        // 链：r0 写 x，r1 读 x 写 y（无环无自修改）
        let rules = vec![set_rule("x", 1), cond_set("__exec__.payload.x", "y", 2)];
        let report = check(&rws_of(&rules)).unwrap();
        assert!(report.converges);
        assert!(!report.undetermined);
        assert_eq!(report.estimated_passes, Some(2));
        assert!(report.issues.is_empty());
    }

    #[test]
    fn self_modifying_detected() {
        // 无条件 set 读自身域? set 无域 → 用 branch: 域读 x，写 x
        let rules = vec![cond_set("__exec__.payload.x", "x", 1)];
        let report = check(&rws_of(&rules)).unwrap();
        assert!(!report.converges);
        assert!(report
            .issues
            .iter()
            .any(|i| i.issue_type == "self_modifying" && !i.conservative));
    }

    #[test]
    fn static_cycle_reused_from_cycle_module() {
        // r0 读 y 写 x；r1 读 x 写 y → 静态环（cycle_detect 同源判定）
        let rules = vec![
            cond_set("__exec__.payload.y", "x", 1),
            cond_set("__exec__.payload.x", "y", 2),
        ];
        let report = check(&rws_of(&rules)).unwrap();
        assert!(!report.converges);
        assert!(report
            .issues
            .iter()
            .any(|i| i.issue_type == "static_cycle" && !i.conservative));
        assert_eq!(report.estimated_passes, None);
    }

    #[test]
    fn dynamic_write_makes_termination_undetermined() {
        let dyn_rule = obj(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                obj(&[
                    (
                        "attr",
                        JsonValue::string("__exec__.instruction.params.attr"),
                    ),
                    ("value", JsonValue::Integer(1)),
                ]),
            ),
        ]);
        let report = check(&rws_of(&[dyn_rule])).unwrap();
        assert!(report.undetermined);
        assert_eq!(report.estimated_passes, None);
        assert!(report
            .issues
            .iter()
            .any(|i| i.issue_type == "conservative_cycle" && i.conservative));
    }

    #[test]
    fn excluded_bucket_listed_not_judged() {
        let rules = vec![set_rule("x", 1)];
        let rws = extract(&rules, &["core_eval".to_string()], false).unwrap();
        let report = check(&rws).unwrap();
        assert_eq!(report.excluded, vec!["core_eval#0".to_string()]);
        assert!(report.converges);
    }

    #[test]
    fn determinism_two_runs_identical() {
        let rules = vec![set_rule("x", 1), cond_set("__exec__.payload.x", "y", 2)];
        let a = check(&rws_of(&rules)).unwrap();
        let b = check(&rws_of(&rules)).unwrap();
        assert_eq!(a, b);
    }
}

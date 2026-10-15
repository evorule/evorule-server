// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 后向链规划（112 阶段 3，复用阶段 2 rw_extract 底座）。
//!
//! 给定目标写路径与初始可用路径集，反推"哪些规则以何顺序执行能产出目标、
//! 哪些输入必须外部提供"。算法底稿 = v4 `backward.rs`（已废弃）的生产者
//! 搜索框架；模型层全部按现行模型重写：规则身份 = `(source, index)`、
//! 生产者索引 = rw_extract 静态写集、搜索防环复用 on-path 集合。
//!
//! # 诚实边界
//!
//! - 可达性 = **静态路径存在性**，非运行时保证；
//! - 动态写目标（`write_dyn`）规则可作生产者但整体结论降级 `conservative`；
//! - 豁免桶规则（宪法段）不作为生产者（自引用形态，计入即是假能力），
//!   显式列入 `excluded` 透出。

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};

use crate::rw_extract::RuleRw;

/// 搜索深度缺省值
pub const DEFAULT_MAX_DEPTH: usize = 8;
/// 搜索深度硬上限
pub const MAX_DEPTH_CAP: usize = 24;

/// 规划结果
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanReport {
    /// 目标路径是否可由规则集产出（或已在可用集内）
    pub achievable: bool,
    /// 规则执行序（生产者在前、目标规则在后；`"{source}#{index}"`）
    pub rule_sequence: Vec<String>,
    /// 无法由规则集产出、需外部提供的路径（排序去重）
    pub required_inputs: Vec<String>,
    /// 搜索深度（规则链最长递归层）
    pub depth: usize,
    /// 目标路径
    pub target_path: String,
    /// 结论是否保守（链上含动态写生产者 → 可达性非严格保证）
    pub conservative: bool,
    /// 豁免桶规则（未参与生产者搜索，透明透出）
    pub excluded: Vec<String>,
    /// 不可达时的说明
    pub reason: String,
}

/// 生产者索引：静态写路径 → 写该路径的非豁免规则下标列表（按合并序）
fn producer_index(rws: &[RuleRw]) -> BTreeMap<&str, Vec<usize>> {
    let mut idx: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, rw) in rws.iter().enumerate() {
        if rw.excluded {
            continue;
        }
        for w in &rw.writes {
            idx.entry(w.as_str()).or_default().push(i);
        }
    }
    idx
}

/// 递归规划：返回 `(sequence, missing_inputs, depth, conservative)`。
///
/// `available` = 已满足路径集（初始可用 ∪ 已在链上产出的路径）；
/// `on_path` = 递归栈上的目标路径（防环）。
#[allow(clippy::too_many_arguments)]
fn plan_recursive(
    target: &str,
    rws: &[RuleRw],
    producers: &BTreeMap<&str, Vec<usize>>,
    available: &mut BTreeSet<String>,
    required: &mut BTreeSet<String>,
    on_path: &mut BTreeSet<String>,
    sequence: &mut Vec<usize>,
    conservative: &mut bool,
    depth: usize,
    max_depth: usize,
) -> (bool, usize) {
    if available.contains(target) {
        return (true, depth);
    }
    if depth >= max_depth {
        return (false, depth);
    }
    if !on_path.insert(target.to_string()) {
        // 路径已在递归栈上 → 环；该分支不可达（真环由 cycle_detect 独立报告）
        return (false, depth);
    }

    // 无生产者 → 外部必需输入（v4 语义：标记为必需输入，链仍可达）
    let producer_indices: &[usize] = match producers.get(target) {
        Some(v) if !v.is_empty() => v,
        _ => {
            required.insert(target.to_string());
            available.insert(target.to_string());
            on_path.remove(target);
            return (true, depth);
        }
    };

    let mut best: Option<(Vec<usize>, BTreeSet<String>, usize, bool)> = None;
    {
        let indices = producer_indices;
        for &ri in indices {
            let rw = &rws[ri];
            if rw.write_dyn {
                // 动态写目标：名义上写一切路径，作生产者使结论降级
                *conservative = true;
            }
            let mut sub_available = available.clone();
            let mut sub_seq = sequence.clone();
            let mut sub_conservative = *conservative;
            let mut ok = true;
            let mut deepest = depth + 1;

            // 该规则的全部域读 + 路径引用读都必须可得（域读与值引用统一按 rw.reads）
            for need in &rw.reads {
                if need.starts_with("queue[")
                    || need.starts_with("instruction.")
                    || need.starts_with("io:")
                {
                    // 执行上下文自带段 / I/O 通道读不视为外部文档输入
                    continue;
                }
                if sub_available.contains(need) {
                    continue;
                }
                let (sat, d) = plan_recursive(
                    need,
                    rws,
                    producers,
                    &mut sub_available,
                    required,
                    on_path,
                    &mut sub_seq,
                    &mut sub_conservative,
                    depth + 1,
                    max_depth,
                );
                deepest = deepest.max(d);
                if !sat {
                    ok = false;
                    break;
                }
                // 递归成功后才标记已满足（失败分支的 sub_available 随局部克隆丢弃）
            }
            if ok {
                sub_seq.push(ri);
                sub_available.insert(target.to_string());
                // 择优：链短者优先；并列取序列字典序最小（确定性）
                let better = match &best {
                    None => true,
                    Some((bseq, _, bd, _)) => {
                        sub_seq.len() < bseq.len() || (sub_seq.len() == bseq.len() && deepest < *bd)
                    }
                };
                if better {
                    best = Some((sub_seq, sub_available, deepest, sub_conservative));
                }
            }
        }
    }

    on_path.remove(target);
    match best {
        Some((seq, _av, d, cons)) => {
            *sequence = seq;
            *conservative = cons;
            (true, d)
        }
        None => (false, depth),
    }
}

/// 后向链规划（见模块文档）。
pub fn plan(
    rws: &[RuleRw],
    target_path: &str,
    available_paths: &BTreeSet<String>,
    max_depth: usize,
) -> PlanReport {
    let excluded: Vec<String> = rws
        .iter()
        .filter(|rw| rw.excluded)
        .map(|rw| rw.id.clone())
        .collect();
    let producers = producer_index(rws);

    // 目标本身已可用 → 平凡可达
    if available_paths.contains(target_path) {
        return PlanReport {
            achievable: true,
            target_path: target_path.to_string(),
            reason: "目标路径已在可用集内".to_string(),
            excluded,
            ..PlanReport::default()
        };
    }

    let mut available = available_paths.clone();
    let mut on_path = BTreeSet::new();
    let mut sequence: Vec<usize> = Vec::new();
    let mut conservative = false;
    let mut required: BTreeSet<String> = BTreeSet::new();
    let (ok, depth) = plan_recursive(
        target_path,
        rws,
        &producers,
        &mut available,
        &mut required,
        &mut on_path,
        &mut sequence,
        &mut conservative,
        0,
        max_depth,
    );

    if !ok {
        // 收集缺失输入：沿失败链无法给出精确清单，诚实输出不可达 + 建议看 producers
        return PlanReport {
            achievable: false,
            rule_sequence: Vec::new(),
            target_path: target_path.to_string(),
            conservative,
            excluded,
            reason: format!(
                "目标路径在静态生产者图上不可达（深度上限 {max_depth} 内无生产链；\
                 动态写/豁免段/环均计入不可达方向）"
            ),
            ..PlanReport::default()
        };
    }

    // 深度 = 链长；sequence 已按"生产者在前、目标规则在后"收集
    let rule_sequence: Vec<String> = sequence.iter().map(|&ri| rws[ri].id.clone()).collect();
    PlanReport {
        achievable: true,
        rule_sequence,
        // 无生产者的路径 = 外部必需输入（链上规则产出的不在此列）
        required_inputs: required.into_iter().collect(),
        depth,
        target_path: target_path.to_string(),
        conservative,
        excluded,
        reason: "静态生产链可达（含外部必需输入）".to_string(),
    }
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

    /// branch(eq 域) → on_true[set]
    fn cond_set(domain_path: &str, domain_val: &str, attr: &str, value: i64) -> JsonValue {
        obj(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                obj(&[
                    (
                        "domain",
                        obj(&[
                            ("type", JsonValue::string("eq")),
                            ("path", JsonValue::string(domain_path)),
                            ("value", JsonValue::string(domain_val)),
                        ]),
                    ),
                    (
                        "on_true",
                        JsonValue::Array(vec![obj(&[
                            ("type", JsonValue::string("set")),
                            (
                                "params",
                                obj(&[
                                    ("attr", JsonValue::string(attr)),
                                    ("value", JsonValue::Integer(value)),
                                ]),
                            ),
                        ])]),
                    ),
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
    fn direct_producer_chain_in_order() {
        // r0 产 a；r1 读 a 产 b；目标 b
        let rules = vec![
            cond_set("__exec__.payload.mode", "m", "a", 1),
            cond_set("__exec__.payload.a", "v", "b", 2),
        ];
        let report = plan(
            &rws_of(&rules),
            "payload.b",
            &BTreeSet::new(),
            DEFAULT_MAX_DEPTH,
        );
        assert!(report.achievable);
        assert_eq!(
            report.rule_sequence,
            vec!["rules/r0.json#0".to_string(), "rules/r1.json#1".to_string()]
        );
        assert!(!report.conservative);
    }

    #[test]
    fn target_already_available_is_trivial() {
        let rules = vec![cond_set("__exec__.payload.mode", "m", "b", 1)];
        let mut avail = BTreeSet::new();
        avail.insert("payload.b".to_string());
        let report = plan(&rws_of(&rules), "payload.b", &avail, DEFAULT_MAX_DEPTH);
        assert!(report.achievable);
        assert!(report.rule_sequence.is_empty());
    }

    #[test]
    fn no_producer_becomes_required_input() {
        // v4 语义：无生产者路径 = 外部必需输入，链仍可达（achievable + required_inputs）
        let rules = vec![cond_set("__exec__.payload.mode", "m", "a", 1)];
        let report = plan(
            &rws_of(&rules),
            "payload.ghost",
            &BTreeSet::new(),
            DEFAULT_MAX_DEPTH,
        );
        assert!(report.achievable);
        assert!(report.rule_sequence.is_empty());
        assert_eq!(report.required_inputs, vec!["payload.ghost".to_string()]);
    }

    #[test]
    fn dynamic_producer_downgrades_to_conservative() {
        // 动态写：attr 为路径引用（写位置运行时定形）——静态写集只含
        // attr 表达式本身，不匹配任何具体 payload 路径（诚实：不假装知道写哪）
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
        // 对具体 payload 路径：动态写者不构成生产者 → 必需输入
        let r1 = plan(
            &rws_of(std::slice::from_ref(&dyn_rule)),
            "payload.b",
            &BTreeSet::new(),
            DEFAULT_MAX_DEPTH,
        );
        assert!(r1.achievable);
        assert!(!r1.conservative);
        assert_eq!(r1.required_inputs, vec!["payload.b".to_string()]);
        // 对写表达式路径本身：动态写者是生产者 → 结论降级 conservative
        let r2 = plan(
            &rws_of(&[dyn_rule]),
            "instruction.params.attr",
            &BTreeSet::new(),
            DEFAULT_MAX_DEPTH,
        );
        assert!(r2.achievable);
        assert!(r2.conservative);
    }

    #[test]
    fn depth_cap_limits_search() {
        // 链长 6：r_i 读 p_i 写 p_{i+1}
        let mut rules = Vec::new();
        for i in 0..6 {
            rules.push(cond_set(
                &format!("__exec__.payload.p{i}"),
                "v",
                &format!("p{}", i + 1),
                i as i64,
            ));
        }
        let report = plan(&rws_of(&rules), "payload.p6", &BTreeSet::new(), 4);
        // 深度上限 4 < 链长 6 → 不可达（保守方向：不假装可达）
        assert!(!report.achievable);
    }

    #[test]
    fn cycle_in_chain_not_infinite() {
        // r0 读 b 写 a；r1 读 a 写 b；目标 a：递归在 on_path 防环下终止
        let rules = vec![
            cond_set("__exec__.payload.b", "v", "a", 1),
            cond_set("__exec__.payload.a", "v", "b", 2),
        ];
        let report = plan(
            &rws_of(&rules),
            "payload.a",
            &BTreeSet::new(),
            DEFAULT_MAX_DEPTH,
        );
        // 互为生产者 → 链不终止 → 不可达（环由 cycle_detect 报告）
        assert!(!report.achievable);
    }
}

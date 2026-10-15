// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 规则集内环路检测（112 号阶段 2，设计文档 §三）。
//!
//! # 算法骨架
//!
//! 保真自 v4 `CycleDetector`（Kahn 入度表 + dependents 表 + DFS 环路径提取），
//! 输入换成 [`crate::rw_extract::RuleRw`] 流——**算法底稿 + 测试预言分离**，
//! 模型层零照搬（v4-TCB 类型化 API → 现行规范化路径集）。
//!
//! # 建图（现行语义）
//!
//! - **静态边** a→b：`writes(a) ∩ reads(b) ≠ ∅`（a 的产出是 b 的输入）。
//! - **保守边**（单向多报，绝不静默漏，裁定 R-3）：
//!   1. `write_dyn(a)` 且（b 读 `instruction.*` 或 b 也是动态写/动态域）——
//!      a 的写位置运行时才定形，可能与 b 的指令参数读发生数据相关；
//!   2. `domain_dyn(b)` 且 a 写 `instruction.*`——b 的域条件运行时从指令
//!      参数解析，a 对指令树的写可能改变 b 的可达性。
//! - **分桶**：`excluded = true`（宪法段默认豁免）的规则不参与建图，
//!   在报告中显式列出（不静默）。
//!
//! # 与 workflow.rs 的关系
//!
//! workflow.rs 的 Kahn 是 workflow 节点图（另一层）；本模块是**规则集内
//! 规则图**，不同对象、不复用、不冲突。TCB 零改动——环检测是治理面
//! 静态分析能力，不进纯计算内核。
//!
//! # 确定性
//!
//! 全程 `BTreeMap`/`BTreeSet` + 字典序遍历；环去重用规范形（旋转至最小 id
//! 居首，正向/反向取字典序较小者）；环发现数硬上限 1000（确定性截断）。

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};

use crate::rw_extract::RuleRw;

/// 环发现数硬上限（确定性截断；到达即停止 DFS，不 panic）
const MAX_CYCLES: usize = 1000;

/// 边类别
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EdgeKind {
    /// 纯静态读写交集
    Static,
    /// 含动态标注的保守边（多报方向）
    Conservative,
}

/// 环路检测报告（字段名对齐 v4 测试预言，conservative 分层为现行新增）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CycleReport {
    /// 是否存在静态环（主口径；保守环不计入本字段）
    pub has_cycles: bool,
    /// 静态环数量
    pub cycle_count: usize,
    /// 静态环列表（每环为规则 id 列表，规范形）
    pub static_cycles: Vec<Vec<String>>,
    /// 保守环列表（含保守边，需人工复核）
    pub conservative_cycles: Vec<Vec<String>>,
    /// 拓扑序（无环前缀有效；有环时为被剔除环节点后的可行序）
    pub topological_order: Vec<String>,
    /// 不在任何环中的规则
    pub safe_rules: Vec<String>,
    /// 位于环中的规则（含保守环成员）
    pub unsafe_rules: Vec<String>,
    /// 豁免桶规则 id 列表（显式标注，不静默）
    pub excluded: Vec<String>,
}

/// 环路检测入口。
///
/// # Errors
///
/// 内部不变量被破坏时返回 `Err`（正常输入不会发生；防御性不 panic）。
pub fn detect_cycles(rws: &[RuleRw]) -> Result<CycleReport, String> {
    // ── 分桶 ──
    let active: Vec<&RuleRw> = rws.iter().filter(|r| !r.excluded).collect();
    let excluded: Vec<String> = rws
        .iter()
        .filter(|r| r.excluded)
        .map(|r| r.id.clone())
        .collect();

    // ── 建图：adjacency[src] = {dst: kind}（同类取更优 Static）──
    let mut adjacency: BTreeMap<String, BTreeMap<String, EdgeKind>> = BTreeMap::new();
    for r in &active {
        adjacency.entry(r.id.clone()).or_default();
    }
    for a in &active {
        for b in &active {
            if a.id == b.id {
                continue;
            }
            let kind = edge_kind(a, b);
            if let Some(k) = kind {
                let slot = adjacency
                    .get_mut(&a.id)
                    .ok_or("内部不变量破坏: adjacency 缺项")?;
                let entry = slot.entry(b.id.clone()).or_insert(k);
                if *entry == EdgeKind::Conservative && k == EdgeKind::Static {
                    *entry = EdgeKind::Static;
                }
            }
        }
    }

    // ── Kahn（入度表 + dependents，全边参与；确定性字典序）──
    let mut in_degree: BTreeMap<String, usize> = BTreeMap::new();
    let mut dependents: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (src, dsts) in &adjacency {
        in_degree.entry(src.clone()).or_insert(0);
        for dst in dsts.keys() {
            *in_degree.entry(dst.clone()).or_insert(0) += 1;
            dependents.entry(src.clone()).or_default().push(dst.clone());
        }
    }
    let mut ready: BTreeSet<String> = in_degree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(id, _)| id.clone())
        .collect();
    let mut topo_order: Vec<String> = Vec::new();
    while let Some(id) = ready.iter().next().cloned() {
        ready.remove(&id);
        topo_order.push(id.clone());
        if let Some(deps) = dependents.get(&id) {
            for d in deps {
                if let Some(deg) = in_degree.get_mut(d) {
                    *deg -= 1;
                    if *deg == 0 {
                        ready.insert(d.clone());
                    }
                }
            }
        }
    }

    let cycle_nodes: Vec<String> = adjacency
        .keys()
        .filter(|id| !topo_order.contains(*id))
        .cloned()
        .collect();

    if cycle_nodes.is_empty() {
        return Ok(CycleReport {
            has_cycles: false,
            cycle_count: 0,
            static_cycles: Vec::new(),
            conservative_cycles: Vec::new(),
            topological_order: topo_order,
            safe_rules: adjacency.keys().cloned().collect(),
            unsafe_rules: Vec::new(),
            excluded,
        });
    }

    // ── 环路径提取（DFS，规范形去重，按边类别分层）──
    let cycle_set: BTreeSet<&String> = cycle_nodes.iter().collect();
    let mut found: BTreeSet<Vec<String>> = BTreeSet::new();
    let mut kinds: BTreeMap<Vec<String>, EdgeKind> = BTreeMap::new();
    for start in &cycle_nodes {
        let mut path: Vec<String> = vec![start.clone()];
        let mut on_path: BTreeSet<String> = BTreeSet::from([start.clone()]);
        dfs_cycles(
            start,
            start,
            &cycle_set,
            &adjacency,
            &mut on_path,
            &mut path,
            &mut found,
            &mut kinds,
        );
        if found.len() >= MAX_CYCLES {
            break;
        }
    }

    let mut static_cycles = Vec::new();
    let mut conservative_cycles = Vec::new();
    for (cycle, kind) in &kinds {
        if *kind == EdgeKind::Static {
            static_cycles.push(cycle.clone());
        } else {
            conservative_cycles.push(cycle.clone());
        }
    }

    Ok(CycleReport {
        has_cycles: !static_cycles.is_empty(),
        cycle_count: static_cycles.len(),
        static_cycles,
        conservative_cycles,
        topological_order: topo_order,
        safe_rules: adjacency
            .keys()
            .filter(|id| !cycle_set.contains(*id))
            .cloned()
            .collect(),
        unsafe_rules: cycle_nodes,
        excluded,
    })
}

/// 边判定：a→b 的类别（None = 无边）
fn edge_kind(a: &RuleRw, b: &RuleRw) -> Option<EdgeKind> {
    // 静态边：写读交集；但动态写表达式空间（instruction.*）的交集降级为
    // 保守边（R-3 细化，2026-10-15 真实语料回归发现）：form-2 动态写的
    // 写集条目是 attr 表达式本身（如 instruction.params.attr），不是真实
    // payload 位置——部署面自持的 increment/decrement/set 桥接剧本规则
    // （与宪法同源自引用形态）在此空间互相成边会产生假静态环。
    let mut static_hit = false;
    for w in &a.writes {
        if b.reads.contains(w) {
            if a.write_dyn && w.starts_with("instruction.") {
                return Some(EdgeKind::Conservative);
            }
            static_hit = true;
        }
    }
    if static_hit {
        return Some(EdgeKind::Static);
    }
    // 保守边 1：a 动态写 + b 触碰指令参数读 / 动态写 / 动态域
    if a.write_dyn
        && (b.reads.iter().any(|r| r.starts_with("instruction.")) || b.write_dyn || b.domain_dyn)
    {
        return Some(EdgeKind::Conservative);
    }
    // 保守边 2：b 动态域 + a 写指令树（b 的域条件运行时从指令参数解析）
    if b.domain_dyn && a.writes.iter().any(|w| w.starts_with("instruction.")) {
        return Some(EdgeKind::Conservative);
    }
    None
}

/// DFS 找环（v4 find_cycle_paths 骨架 + 规范形去重 + 边类别聚合）
#[allow(clippy::too_many_arguments)]
fn dfs_cycles(
    current: &str,
    start: &str,
    cycle_set: &BTreeSet<&String>,
    adjacency: &BTreeMap<String, BTreeMap<String, EdgeKind>>,
    on_path: &mut BTreeSet<String>,
    path: &mut Vec<String>,
    found: &mut BTreeSet<Vec<String>>,
    kinds: &mut BTreeMap<Vec<String>, EdgeKind>,
) {
    if found.len() >= MAX_CYCLES {
        return;
    }
    let Some(dsts) = adjacency.get(current) else {
        return;
    };
    for next in dsts.keys() {
        if !cycle_set.contains(next) {
            continue;
        }
        if next == start {
            // 找到一环：path 即环（start 开头）
            let canon = canonical_cycle(path);
            let worst = if path.windows(2).any(|w| {
                adjacency
                    .get(&w[0].to_string())
                    .and_then(|m| m.get(&w[1].to_string()))
                    == Some(&EdgeKind::Conservative)
            }) {
                EdgeKind::Conservative
            } else {
                EdgeKind::Static
            };
            // 同一环既可能被 static 边发现也可能被 conservative 边发现——
            // 取更弱类别（Conservative 优先保留，多报方向）
            if found.insert(canon.clone()) || worst == EdgeKind::Conservative {
                let entry = kinds.entry(canon).or_insert(worst);
                if worst == EdgeKind::Conservative {
                    *entry = EdgeKind::Conservative;
                }
            }
            continue;
        }
        if on_path.contains(next) {
            continue;
        }
        on_path.insert(next.clone());
        path.push(next.clone());
        dfs_cycles(
            next, start, cycle_set, adjacency, on_path, path, found, kinds,
        );
        path.pop();
        on_path.remove(next);
    }
}

/// 环规范形：旋转至最小 id 居首；与反向环取字典序较小者（去重）
fn canonical_cycle(path: &[String]) -> Vec<String> {
    let n = path.len();
    let mut best: Option<Vec<String>> = None;
    for offset in 0..n {
        let mut rotated: Vec<String> = Vec::with_capacity(n);
        for i in 0..n {
            // 环上旋转取元素；n ≤ 64 且 path 非空，下标恒在界内
            let idx = (offset + i) % n;
            rotated.push(path[idx].clone());
        }
        let mut reversed: Vec<String> = rotated.iter().rev().cloned().collect();
        reversed.rotate_left(1);
        for candidate in [rotated, reversed] {
            let is_better = match best.as_ref() {
                None => true,
                Some(b) => candidate < *b,
            };
            if is_better {
                best = Some(candidate);
            }
        }
    }
    best.unwrap_or_default()
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

    fn set_with_guard(attr: &str, guard_path: &str, value: i64) -> JsonValue {
        obj(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                obj(&[
                    (
                        "domain",
                        obj(&[
                            ("type", JsonValue::string("exists")),
                            ("path", JsonValue::string(guard_path)),
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
    fn acyclic_rules_yield_topological_order() {
        // r0 写 payload.a；r1 读 payload.a 写 payload.b；r2 读 payload.b
        let rules = vec![
            set_rule("a", 1),
            set_with_guard("b", "__exec__.payload.a", 2),
            set_with_guard("c", "__exec__.payload.b", 3),
        ];
        let report = detect_cycles(&rws_of(&rules)).unwrap();
        assert!(!report.has_cycles);
        assert_eq!(report.cycle_count, 0);
        assert_eq!(report.topological_order.len(), 3);
        assert!(report.unsafe_rules.is_empty());
        assert_eq!(report.safe_rules.len(), 3);
        // 拓扑序确定性：r0 必在 r1 前，r1 必在 r2 前
        let pos = |id: &str| {
            report
                .topological_order
                .iter()
                .position(|x| x == id)
                .expect("id 在拓扑序中")
        };
        assert!(pos("rules/r0.json#0") < pos("rules/r1.json#1"));
        assert!(pos("rules/r1.json#1") < pos("rules/r2.json#2"));
    }

    #[test]
    fn two_node_write_read_cycle_detected() {
        // r0: 存在 payload.b 时写 payload.a；r1: 存在 payload.a 时写 payload.b
        // → r0→r1（a 是 b 的输入）与 r1→r0（b 是 a 的输入）构成静态环
        let rules = vec![
            set_with_guard("a", "__exec__.payload.b", 1),
            set_with_guard("b", "__exec__.payload.a", 2),
        ];
        let report = detect_cycles(&rws_of(&rules)).unwrap();
        assert!(report.has_cycles);
        assert_eq!(report.cycle_count, 1);
        assert_eq!(report.static_cycles.len(), 1);
        let cycle = &report.static_cycles[0];
        assert_eq!(cycle.len(), 2);
        assert_eq!(report.unsafe_rules.len(), 2);
        assert!(report.safe_rules.is_empty());
    }

    #[test]
    fn excluded_bucket_not_in_graph() {
        // 宪法形态动态写规则在豁免桶 → 不参与建图，excluded 显式列出
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
        let rws = extract(
            std::slice::from_ref(&dyn_rule),
            &["core_eval".to_string()],
            false,
        )
        .unwrap();
        let report = detect_cycles(&rws).unwrap();
        assert!(!report.has_cycles);
        assert_eq!(report.excluded, vec!["core_eval#0".to_string()]);
    }

    #[test]
    fn dynamic_write_creates_conservative_edge() {
        // r0 动态写（attr 引用 instruction.params.attr）+ 读 instruction.params.attr
        // r1 读 instruction.params.attr → 保守边 r0→r1
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
        let reader = obj(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                obj(&[
                    (
                        "domain",
                        obj(&[
                            ("type", JsonValue::string("exists")),
                            (
                                "path",
                                JsonValue::string("__exec__.instruction.params.attr"),
                            ),
                        ]),
                    ),
                    ("on_true", JsonValue::Array(vec![])),
                ]),
            ),
        ]);
        let rws = rws_of(&[dyn_rule, reader]);
        let report = detect_cycles(&rws).unwrap();
        // 保守边 r0→r1 不构成环（r1 无回边），无静态环
        assert!(!report.has_cycles);
        assert!(report.static_cycles.is_empty());
        assert_eq!(report.topological_order.len(), 2);
    }

    #[test]
    fn canonical_cycle_dedupes_rotation_and_reverse() {
        let a = "rules/r0.json#0".to_string();
        let b = "rules/r1.json#1".to_string();
        let c = "rules/r2.json#2".to_string();
        let c1 = canonical_cycle(&[a.clone(), b.clone(), c.clone()]);
        let c2 = canonical_cycle(&[b.clone(), c.clone(), a.clone()]);
        let c3 = canonical_cycle(&[a.clone(), c.clone(), b.clone()]);
        assert_eq!(c1, c2, "旋转等价须去重");
        assert_eq!(c1, c3, "反向等价须去重");
        assert_eq!(c1[0], a, "最小 id 居首");
    }
}

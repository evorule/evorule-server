// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 规则集内冲突检测（112 号阶段 2，设计文档 §四，裁定 R-1）。
//!
//! # 类型学（v4 五类按现行语义重写，零照搬）
//!
//! | 类型 | 来源 | 现行判定 |
//! |------|------|----------|
//! | `contradictory_constants` | v4 ContradictoryTransforms | 同写路径 + 域可能同时满足 + 两侧均为静态常量且不等 |
//! | `shadow_risk` | v4 SamePriority 语义失效后重定义 | 同写路径 + 域可能同时满足 + 非常量矛盾——现行无优先级，**列表顺序即优先级**（后写覆盖前写） |
//! | `write_contention` | v4 ResourceContention | 同写路径 + 域可能同时满足 + 至少一侧为动态写/路径引用值——结果静态不可定形 |
//! | `complementary_domain_pair` | v4 MutuallyExclusiveDomains 保守版 | 同写路径 + 域不重叠 + 一侧域为 not(X) 镜像而另一侧为 X |
//! | `circular_write` | v4 CircularDependency 改由 cycle 服务供给 | 同静态环成员对 + 同写路径 |
//! | `io_result_contention` | **现行特有**（v4 无对应） | 同 `io_type` 的多生产者（`__io_results__.{T}` 按类型隔离，多生产者顺序混用会串台） |
//!
//! # 与 DC-05 的边界（不重叠声明）
//!
//! DC-05 管装载期 `enforce` 约束前置门遮蔽；本模块管 set 层写覆盖与
//! 顺序敏感（装载后/CI 静态分析）。只产出报告，**不做任何拦截**——
//! 拦截权归治理面。
//!
//! # 域重叠判定（静态保守）
//!
//! 仅在**可证伪**方向判否：两侧 `instruction` 域约束集互斥 → 不重叠；
//! 其余（含动态域、无约束域、路径读域）一律保守判可能重叠（多报方向，
//! 裁定 R-3）。路径读域不用于判否——不同属性的域条件可同时满足。

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use crate::cycle::{detect_cycles, CycleReport};
use crate::rw_extract::RuleRw;

/// 单条冲突
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// 规则 1（合并列表中靠前者）
    pub rule_id_1: String,
    /// 规则 2（合并列表中靠后者）
    pub rule_id_2: String,
    /// 冲突类型（见模块文档类型学）
    pub conflict_type: String,
    /// 描述（含证据）
    pub description: String,
    /// 是否保守判定（含动态标注；多报方向）
    pub conservative: bool,
}

/// 冲突检测报告
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConflictReport {
    /// 是否存在冲突
    pub has_conflicts: bool,
    /// 冲突总数
    pub conflict_count: usize,
    /// 冲突列表（按 (i, j, 类型) 字典序确定性排列）
    pub conflicts: Vec<Conflict>,
    /// 豁免桶规则 id 列表（显式标注，不静默）
    pub excluded: Vec<String>,
}

/// 冲突类型常量（与模块文档类型学一致）
pub const T_CONTRADICTORY: &str = "contradictory_constants";
pub const T_SHADOW: &str = "shadow_risk";
pub const T_CONTENTION: &str = "write_contention";
pub const T_COMPLEMENTARY: &str = "complementary_domain_pair";
pub const T_CIRCULAR: &str = "circular_write";
pub const T_IO: &str = "io_result_contention";

/// 冲突检测入口。
///
/// 内部先跑一次环检测（`circular_write` 需要 static cycle 成员对）。
///
/// # Errors
///
/// 内部环检测失败时透传 `Err`。
pub fn detect_conflicts(rws: &[RuleRw]) -> Result<ConflictReport, String> {
    let active: Vec<&RuleRw> = rws.iter().filter(|r| !r.excluded).collect();
    let excluded: Vec<String> = rws
        .iter()
        .filter(|r| r.excluded)
        .map(|r| r.id.clone())
        .collect();

    let cycle_report: CycleReport = detect_cycles(rws)?;
    let static_cycle_members: Vec<[String; 2]> = cycle_report
        .static_cycles
        .iter()
        .flat_map(|c| {
            // 环内相邻对（含首尾回边）→ 无序对集合
            (0..c.len()).map(move |k| {
                let x = &c[k];
                let y = &c[(k + 1) % c.len()];
                if x <= y {
                    [x.clone(), y.clone()]
                } else {
                    [y.clone(), x.clone()]
                }
            })
        })
        .collect();

    let mut conflicts: Vec<Conflict> = Vec::new();

    // ── 写路径配对检测（O(n²)，n ≤ MAX_TRANSFORM_RULES=64 有上界保护）──
    for i in 0..active.len() {
        for j in (i + 1)..active.len() {
            let a = active[i];
            let b = active[j];
            let (id1, id2) = (a.id.as_str(), b.id.as_str());
            let overlap = domains_may_overlap(a, b);

            // 动态写：与对方任意静态写都可能落到同一路径（保守）
            if (a.write_dyn || b.write_dyn) && overlap {
                conflicts.push(Conflict {
                    rule_id_1: id1.to_string(),
                    rule_id_2: id2.to_string(),
                    conflict_type: T_CONTENTION.to_string(),
                    description: format!(
                        "规则 {id1} 或 {id2} 存在动态写目标（attr 运行时才定形），\
                         与对方静态写可能落到同一路径——写位置静态不可判定"
                    ),
                    conservative: true,
                });
                continue;
            }

            // 共享写路径逐条判定（字典序确定性）
            let shared: Vec<&String> = a.writes.iter().filter(|w| b.writes.contains(*w)).collect();
            for w in shared {
                // circular_write 优先（环内写路径最凶）
                if static_cycle_members.iter().any(|pair| {
                    (pair[0] == id1 && pair[1] == id2) || (pair[0] == id2 && pair[1] == id1)
                }) {
                    conflicts.push(Conflict {
                        rule_id_1: id1.to_string(),
                        rule_id_2: id2.to_string(),
                        conflict_type: T_CIRCULAR.to_string(),
                        description: format!(
                            "规则 {id1} 与 {id2} 同处静态环且都写 '{w}'——循环写依赖"
                        ),
                        conservative: false,
                    });
                    continue;
                }
                if !overlap {
                    // 域可证不重叠：仅互补域对仍值得报告（作者可能以为互斥）
                    if is_complementary(a, b) {
                        conflicts.push(Conflict {
                            rule_id_1: id1.to_string(),
                            rule_id_2: id2.to_string(),
                            conflict_type: T_COMPLEMENTARY.to_string(),
                            description: format!(
                                "规则 {id1} 与 {id2} 都写 '{w}'，域呈 not 镜像互补——\
                                 若作者意图是互斥分支请确认无第三态遗漏"
                            ),
                            conservative: false,
                        });
                    }
                    continue;
                }
                let va = a.const_writes.get(w);
                let vb = b.const_writes.get(w);
                match (va, vb) {
                    (Some(x), Some(y)) if x != y => {
                        conflicts.push(Conflict {
                            rule_id_1: id1.to_string(),
                            rule_id_2: id2.to_string(),
                            conflict_type: T_CONTRADICTORY.to_string(),
                            description: format!(
                                "规则 {id1} 与 {id2} 域可能同时满足且都写 '{w}' \
                                 但赋予不同常量: {x} vs {y}"
                            ),
                            conservative: false,
                        });
                    }
                    (Some(x), Some(y)) if x == y => {
                        // 同常量：顺序无关，良性，不报
                    }
                    _ => {
                        // 至少一侧为路径引用值：结果运行时才定形 → 顺序敏感
                        conflicts.push(Conflict {
                            rule_id_1: id1.to_string(),
                            rule_id_2: id2.to_string(),
                            conflict_type: T_SHADOW.to_string(),
                            description: format!(
                                "规则 {id1} 与 {id2} 域可能同时满足且都写 '{w}'——\
                                 现行无优先级字段，列表靠后者（{id2}）覆盖靠前者；\
                                 若非预期顺序敏感行为请用显式分支隔离"
                            ),
                            conservative: false,
                        });
                    }
                }
            }
        }
    }

    // ── I/O 结果通道多生产者（现行特有）──
    let mut producers: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for r in &active {
        for edge in &r.ios {
            producers
                .entry(edge.io_type.as_str())
                .or_default()
                .push(r.id.as_str());
        }
    }
    for (io_type, ids) in &producers {
        if ids.len() < 2 {
            continue;
        }
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                conflicts.push(Conflict {
                    rule_id_1: ids[i].to_string(),
                    rule_id_2: ids[j].to_string(),
                    conflict_type: T_IO.to_string(),
                    description: format!(
                        "规则 {} 与 {} 都发出 io_request('{io_type}')——\
                         __io_results__.{io_type} 按类型隔离，多生产者顺序混用会互相覆盖结果",
                        ids[i], ids[j]
                    ),
                    conservative: false,
                });
            }
        }
    }

    conflicts.sort_by(|x, y| {
        (&x.rule_id_1, &x.rule_id_2, &x.conflict_type).cmp(&(
            &y.rule_id_1,
            &y.rule_id_2,
            &y.conflict_type,
        ))
    });
    let count = conflicts.len();
    Ok(ConflictReport {
        has_conflicts: count > 0,
        conflict_count: count,
        conflicts,
        excluded,
    })
}

/// 域可能同时满足判定（静态保守：仅可证伪方向判否）
fn domains_may_overlap(a: &RuleRw, b: &RuleRw) -> bool {
    if a.domain_dyn || b.domain_dyn {
        return true;
    }
    if a.domain_universal || b.domain_universal {
        return true;
    }
    if !a.instr_types.is_empty() && !b.instr_types.is_empty() {
        // 两侧都有指令类型约束：互斥即可证不重叠
        if a.instr_types.is_disjoint(&b.instr_types) {
            return false;
        }
    }
    // 其余保守判可能重叠
    true
}

/// 互补域对判定：一侧 not(X) 镜像、另一侧含 X
fn is_complementary(a: &RuleRw, b: &RuleRw) -> bool {
    !a.not_domains.is_disjoint(&b.pos_domains) || !b.not_domains.is_disjoint(&a.pos_domains)
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

    fn set_node(attr: &str, value: i64) -> JsonValue {
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
    fn branch_write(domain: JsonValue, attr: &str, value: i64) -> JsonValue {
        obj(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                obj(&[
                    ("domain", domain),
                    ("on_true", JsonValue::Array(vec![set_node(attr, value)])),
                ]),
            ),
        ])
    }

    fn eq_domain(path: &str, eq_val: &str) -> JsonValue {
        obj(&[
            ("type", JsonValue::string("eq")),
            ("path", JsonValue::string(path)),
            ("value", JsonValue::string(eq_val)),
        ])
    }

    fn instr_domain(t: &str) -> JsonValue {
        obj(&[
            ("type", JsonValue::string("instruction")),
            ("instruction_type", JsonValue::string(t)),
        ])
    }

    fn io_request_rule(io_type: &str, marker: i64) -> JsonValue {
        obj(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                obj(&[
                    ("domain", instr_domain("call_service")),
                    (
                        "on_false",
                        JsonValue::Array(vec![obj(&[
                            ("type", JsonValue::string("io_request")),
                            (
                                "params",
                                obj(&[
                                    ("io_type", JsonValue::string(io_type)),
                                    ("marker", JsonValue::Integer(marker)),
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

    fn types_of(report: &ConflictReport) -> Vec<&str> {
        report
            .conflicts
            .iter()
            .map(|c| c.conflict_type.as_str())
            .collect()
    }

    #[test]
    fn contradictory_constants_detected() {
        // 域都读 payload.mode（可同时满足），都写 payload.x，常量不等
        let rules = vec![
            branch_write(eq_domain("__exec__.payload.mode", "a"), "x", 1),
            branch_write(eq_domain("__exec__.payload.mode", "a"), "x", 2),
        ];
        let report = detect_conflicts(&rws_of(&rules)).unwrap();
        assert!(report.has_conflicts);
        assert!(types_of(&report).contains(&T_CONTRADICTORY));
    }

    #[test]
    fn same_constant_is_benign() {
        let rules = vec![
            branch_write(eq_domain("__exec__.payload.mode", "a"), "x", 1),
            branch_write(eq_domain("__exec__.payload.mode", "a"), "x", 1),
        ];
        let report = detect_conflicts(&rws_of(&rules)).unwrap();
        assert!(!report.has_conflicts);
    }

    #[test]
    fn disjoint_instruction_domains_not_overlapping() {
        // 指令类型互斥（increment vs decrement）→ 可证不重叠
        let r1 = branch_write(instr_domain("increment"), "x", 1);
        let r2 = branch_write(instr_domain("decrement"), "x", 2);
        let report = detect_conflicts(&rws_of(&[r1, r2])).unwrap();
        assert!(!report.has_conflicts);
    }

    #[test]
    fn shadow_risk_when_values_are_path_refs() {
        let mk = |value_ref: &str| {
            obj(&[
                ("type", JsonValue::string("branch")),
                (
                    "params",
                    obj(&[
                        ("domain", eq_domain("__exec__.payload.mode", "a")),
                        (
                            "on_true",
                            JsonValue::Array(vec![obj(&[
                                ("type", JsonValue::string("set")),
                                (
                                    "params",
                                    obj(&[
                                        ("attr", JsonValue::string("x")),
                                        ("value", JsonValue::string(value_ref)),
                                    ]),
                                ),
                            ])]),
                        ),
                    ]),
                ),
            ])
        };
        let rules = vec![mk("__exec__.payload.a"), mk("__exec__.payload.b")];
        let report = detect_conflicts(&rws_of(&rules)).unwrap();
        assert!(types_of(&report).contains(&T_SHADOW));
    }

    #[test]
    fn io_result_contention_multi_producer() {
        let report = detect_conflicts(&rws_of(&[
            io_request_rule("call_service", 1),
            io_request_rule("call_service", 2),
        ]))
        .unwrap();
        assert!(types_of(&report).contains(&T_IO));
    }

    #[test]
    fn circular_write_requires_cycle_membership() {
        // r0 存在 b 写 a；r1 存在 a 写 b → 静态环，但共享写路径为空
        // → circular_write 不出现（诚实边界：环边来自写读交集，非写写交集）
        let rules = vec![
            branch_write(
                obj(&[
                    ("type", JsonValue::string("exists")),
                    ("path", JsonValue::string("__exec__.payload.b")),
                ]),
                "a",
                1,
            ),
            branch_write(
                obj(&[
                    ("type", JsonValue::string("exists")),
                    ("path", JsonValue::string("__exec__.payload.a")),
                ]),
                "b",
                2,
            ),
        ];
        let report = detect_conflicts(&rws_of(&rules)).unwrap();
        assert!(!types_of(&report).contains(&T_CIRCULAR));
    }

    #[test]
    fn excluded_bucket_listed_not_silent() {
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
        let report = detect_conflicts(&rws).unwrap();
        assert_eq!(report.excluded, vec!["core_eval#0".to_string()]);
        assert!(!report.has_conflicts);
    }

    #[test]
    fn dynamic_write_flags_conservative_contention() {
        // r0 动态写（无约束域 → universal）+ r1 静态写 payload.x
        // → write_contention(conservative)
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
        let static_rule = branch_write(
            obj(&[
                ("type", JsonValue::string("exists")),
                ("path", JsonValue::string("__exec__.payload.mode")),
            ]),
            "x",
            5,
        );
        let report = detect_conflicts(&rws_of(&[dyn_rule, static_rule])).unwrap();
        let hit = report
            .conflicts
            .iter()
            .find(|c| c.conflict_type == T_CONTENTION)
            .expect("应报 write_contention");
        assert!(hit.conservative);
    }
}

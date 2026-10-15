// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 规则效果预测（112 阶段 3，R-3 裁定：rw_extract 之上的分类薄层）。
//!
//! 算法底稿 = v4 `effect_predict.rs`（影响分级思想）；模型层全部按现行
//! 模型重写：读写集直接消费阶段 2 [`crate::rw_extract`] 输出，不重复
//! 实现。v4 的 State 注入（`__effect_prediction__` / `__audit_trace`）
//! 与伪置信度（priority×特异性启发式乘积）不移植——插件零状态副作用，
//! 审计归执行器与治理面。
//!
//! # 影响分级（impact）
//!
//! `none`（无读无写无 I/O）< `read_only`（只读）< `write_few`（写 ≤2 路径）
//! < `write_many` < `io_touching`（含 I/O 出边，独立性最高的一档）。

#![forbid(unsafe_code)]

use crate::rw_extract::RuleRw;

/// 影响等级（序：none < read_only < write_few < write_many < io_touching）
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ImpactLevel {
    None,
    ReadOnly,
    WriteFew,
    WriteMany,
    IoTouching,
}

impl ImpactLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            ImpactLevel::None => "none",
            ImpactLevel::ReadOnly => "read_only",
            ImpactLevel::WriteFew => "write_few",
            ImpactLevel::WriteMany => "write_many",
            ImpactLevel::IoTouching => "io_touching",
        }
    }
}

/// 单规则效果预测
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleEffect {
    /// 规则身份 `"{source}#{index}"`
    pub id: String,
    pub impact: ImpactLevel,
    /// 预测读路径（规范串，排序透出）
    pub reads: Vec<String>,
    /// 预测写路径（含动态写表达式本身——诚实标注，不做猜测展开）
    pub writes: Vec<String>,
    /// 警告：动态域 / 动态写 / I/O 出边 / 豁免桶
    pub warnings: Vec<String>,
}

/// 逐规则效果预测（输入即 rw_extract 输出）。
pub fn predict(rws: &[RuleRw]) -> Vec<RuleEffect> {
    rws.iter()
        .map(|rw| {
            let mut warnings = Vec::new();
            if rw.excluded {
                warnings.push("excluded_bucket: 宪法段（默认豁免分析）".to_string());
            }
            if rw.domain_dyn {
                warnings.push("dynamic_domain: 域为路径引用，运行时才可判定".to_string());
            }
            if rw.write_dyn {
                warnings.push("dynamic_write: 写位置运行时才定形".to_string());
            }
            for e in &rw.ios {
                warnings.push(format!("io_edge: {} 出边（I/O 面）", e.io_type));
            }
            if rw.domain_universal {
                warnings.push("universal_domain: 无约束域，与一切输入可能相关".to_string());
            }

            let impact = if !rw.ios.is_empty() {
                ImpactLevel::IoTouching
            } else if rw.writes.is_empty() {
                if rw.reads.is_empty() {
                    ImpactLevel::None
                } else {
                    ImpactLevel::ReadOnly
                }
            } else if rw.writes.len() <= 2 {
                ImpactLevel::WriteFew
            } else {
                ImpactLevel::WriteMany
            };

            RuleEffect {
                id: rw.id.clone(),
                impact,
                reads: rw.reads.iter().cloned().collect(),
                writes: rw.writes.iter().cloned().collect(),
                warnings,
            }
        })
        .collect()
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

    fn rws_of(rules: &[JsonValue]) -> Vec<RuleRw> {
        let sources: Vec<String> = (0..rules.len())
            .map(|i| format!("rules/r{i}.json"))
            .collect();
        extract(rules, &sources, false).unwrap()
    }

    #[test]
    fn write_few_and_write_many_graded() {
        let rules = vec![set_rule("a", 1), set_rule("b", 1)];
        let effects = predict(&rws_of(&rules));
        assert_eq!(effects[0].impact, ImpactLevel::WriteFew);
        // 两条 set 在同一条规则树里 = 写 2 路径（仍 write_few）
        let both = obj(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                obj(&[
                    (
                        "domain",
                        obj(&[
                            ("type", JsonValue::string("exists")),
                            ("path", JsonValue::string("__exec__.payload.m")),
                        ]),
                    ),
                    (
                        "on_true",
                        JsonValue::Array(vec![
                            set_rule("a", 1),
                            set_rule("b", 1),
                            set_rule("c", 1),
                        ]),
                    ),
                ]),
            ),
        ]);
        let effects2 = predict(&rws_of(&[both]));
        assert_eq!(effects2[0].impact, ImpactLevel::WriteMany);
    }

    #[test]
    fn io_rule_is_io_touching_with_warning() {
        let io_rule = obj(&[
            ("type", JsonValue::string("io_request")),
            (
                "params",
                obj(&[("io_type", JsonValue::string("call_external"))]),
            ),
        ]);
        let effects = predict(&rws_of(&[io_rule]));
        assert_eq!(effects[0].impact, ImpactLevel::IoTouching);
        assert!(effects[0]
            .warnings
            .iter()
            .any(|w| w.starts_with("io_edge:")));
    }

    #[test]
    fn dynamic_flags_surface_as_warnings() {
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
        let effects = predict(&rws_of(&[dyn_rule]));
        assert!(effects[0]
            .warnings
            .iter()
            .any(|w| w.starts_with("dynamic_write:")));
    }

    #[test]
    fn excluded_bucket_flagged_not_silent() {
        let rule = set_rule("a", 1);
        let rws = extract(
            std::slice::from_ref(&rule),
            &["core_eval".to_string()],
            false,
        )
        .unwrap();
        let effects = predict(&rws);
        assert!(effects[0]
            .warnings
            .iter()
            .any(|w| w.starts_with("excluded_bucket:")));
    }
}

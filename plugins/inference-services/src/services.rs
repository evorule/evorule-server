// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 原生服务实现：量纲检查三个服务（确定性静态分析，JSON 入 / JSON 出）。
//!
//! # 服务契约
//!
//! - `inference_dimension_from_unit`：
//!   入参 `{"units": ["m/s", "kg·m/s²", ...]}` →
//!   出参 `{"results": [{"input","ok":true,"dimension":[L,M,T,I,Θ,N,J],"si_string"} |
//!                       {"input","ok":false}]}`
//! - `inference_dimension_compare_units`：
//!   入参 `{"pairs": [["N", "kg·m/s²"], ...]}` →
//!   出参 `{"results": [{"left","right","compatible":bool,"left_dimension":[...]|null,
//!                       "right_dimension":[...]|null}]}`（compatible = 量纲向量相等）
//! - `inference_dimension_verify_formula`：
//!   入参 `{"si_units": {"F": "N", ...}, "formulas": ["F == m*a", ...]}` →
//!   出参 `{"results": [{"formula","passed":bool,"left_dimension":[...]|null,
//!                       "right_dimension":[...]|null,"message"}]}`
//!   （`si_units` 中未知单位 / 非字符串值按跳过处理，公式侧如实报「无法推导」）
//!
//! # 确定性
//!
//! 全程整数幂次与字符串运算，无浮点数值输出、无时钟、无随机——
//! 同输入两次执行结果逐位一致。

#![forbid(unsafe_code)]

use evorule_plugin_kit::NativeService;
use evorule_reactor::IoResult;
use evorule_tcb::JsonValue;

use crate::dimension::{FormulaDimensionInferencer, SIDimension};

/// 七分量向量 → JsonValue 数组（[L, M, T, I, Θ, N, J]，逐位 Integer）
fn dim_to_json(dim: &SIDimension) -> JsonValue {
    JsonValue::Array(
        dim.to_vector()
            .iter()
            .map(|p| JsonValue::Integer(i64::from(*p)))
            .collect(),
    )
}

/// 从 args 提取字符串数组（缺字段 / 非数组 → Err，fail-fast 附指引）
fn string_array_arg<'a>(args: &'a JsonValue, key: &str) -> Result<Vec<&'a str>, String> {
    let arr = args.get(key).ok_or_else(|| {
        format!(
            "缺少必填参数 {key}（形如 {{\"{key}\": [\"m/s\", ...]}}）— \
             自诊断指引: 入参为 JSON 对象, {key} 须为字符串数组"
        )
    })?;
    let items = match arr {
        JsonValue::Array(items) => items,
        _ => {
            return Err(format!(
                "参数 {key} 须为字符串数组 — 自诊断指引: 检查入参类型"
            ))
        }
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match item {
            JsonValue::String(s) => out.push(s.as_ref()),
            _ => {
                return Err(format!(
                    "参数 {key} 中存在非字符串元素 — 自诊断指引: \
                     所有单位必须以字符串书写（如 \"m/s\"、\"kg·m/s²\"）"
                ))
            }
        }
    }
    Ok(out)
}

/// 从 args 提取 `{"var": "unit", ...}` 字符串映射（缺字段 → 空映射，宽松：
/// 公式侧对未知变量会如实报「无法推导」）
fn si_units_arg(args: &JsonValue) -> BTreeMapAlias {
    let mut out = BTreeMapAlias::new();
    if let Some(JsonValue::Object(map)) = args.get("si_units") {
        for (k, v) in map.iter() {
            if let JsonValue::String(s) = v {
                out.insert(k.clone(), s.to_string());
            }
        }
    }
    out
}

type BTreeMapAlias = std::collections::BTreeMap<String, String>;

/// 从 args 提取字符串二元组数组
fn pair_array_arg<'a>(args: &'a JsonValue, key: &str) -> Result<Vec<(&'a str, &'a str)>, String> {
    let arr = args.get(key).ok_or_else(|| {
        format!(
            "缺少必填参数 {key}（形如 {{\"{key}\": [[\"N\", \"kg·m/s²\"], ...]}}）— \
             自诊断指引: {key} 须为二元字符串数组的数组"
        )
    })?;
    let items = match arr {
        JsonValue::Array(items) => items,
        _ => return Err(format!("参数 {key} 须为数组 — 自诊断指引: 检查入参类型")),
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match item {
            JsonValue::Array(pair) if pair.len() == 2 => {
                let l = match &pair[0] {
                    JsonValue::String(s) => s.as_ref(),
                    _ => {
                        return Err(format!(
                            "参数 {key} 的二元组存在非字符串元素 — 自诊断指引: \
                             单位必须以字符串书写"
                        ))
                    }
                };
                let r = match &pair[1] {
                    JsonValue::String(s) => s.as_ref(),
                    _ => {
                        return Err(format!(
                            "参数 {key} 的二元组存在非字符串元素 — 自诊断指引: \
                             单位必须以字符串书写"
                        ))
                    }
                };
                out.push((l, r));
            }
            _ => {
                return Err(format!(
                    "参数 {key} 中存在非二元组元素 — 自诊断指引: \
                     每项须为 [左单位, 右单位] 二元数组"
                ))
            }
        }
    }
    Ok(out)
}

// ── inference_dimension_from_unit ──

/// 单位字符串 → 量纲向量解析服务
pub struct DimensionFromUnit;

impl NativeService for DimensionFromUnit {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let units = string_array_arg(args, "units")?;
        let results: Vec<JsonValue> = units
            .iter()
            .map(|u| match SIDimension::from_si_unit(u) {
                Some(dim) => JsonValue::object_from_pairs(&[
                    ("input", JsonValue::string(*u)),
                    ("ok", JsonValue::Bool(true)),
                    ("dimension", dim_to_json(&dim)),
                    ("si_string", JsonValue::string(dim.to_si_string())),
                ]),
                None => JsonValue::object_from_pairs(&[
                    ("input", JsonValue::string(*u)),
                    ("ok", JsonValue::Bool(false)),
                ]),
            })
            .collect();
        Ok(JsonValue::object_from_pairs(&[(
            "results",
            JsonValue::Array(results),
        )]))
    }
}

// ── inference_dimension_compare_units ──

/// 两单位量纲等价性比较服务（compatible = 七分量向量逐位相等）
pub struct DimensionCompareUnits;

impl NativeService for DimensionCompareUnits {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let pairs = pair_array_arg(args, "pairs")?;
        let results: Vec<JsonValue> = pairs
            .iter()
            .map(|(l, r)| {
                let ld = SIDimension::from_si_unit(l);
                let rd = SIDimension::from_si_unit(r);
                let compatible = matches!((&ld, &rd), (Some(a), Some(b)) if a == b);
                JsonValue::object_from_pairs(&[
                    ("left", JsonValue::string(*l)),
                    ("right", JsonValue::string(*r)),
                    ("compatible", JsonValue::Bool(compatible)),
                    (
                        "left_dimension",
                        ld.as_ref().map(dim_to_json).unwrap_or(JsonValue::Null),
                    ),
                    (
                        "right_dimension",
                        rd.as_ref().map(dim_to_json).unwrap_or(JsonValue::Null),
                    ),
                ])
            })
            .collect();
        Ok(JsonValue::object_from_pairs(&[(
            "results",
            JsonValue::Array(results),
        )]))
    }
}

// ── inference_dimension_verify_formula ──

/// 公式量纲一致性验证服务
pub struct DimensionVerifyFormula;

impl NativeService for DimensionVerifyFormula {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let formulas = string_array_arg(args, "formulas")?;
        let si_units = si_units_arg(args);
        let mut inf = FormulaDimensionInferencer::new();
        inf.load_from_si_units(&si_units);
        let results: Vec<JsonValue> = formulas
            .iter()
            .map(|f| {
                let r = inf.verify_formula(f);
                JsonValue::object_from_pairs(&[
                    ("formula", JsonValue::string(*f)),
                    ("passed", JsonValue::Bool(r.passed)),
                    (
                        "left_dimension",
                        r.left_dimension
                            .as_ref()
                            .map(dim_to_json)
                            .unwrap_or(JsonValue::Null),
                    ),
                    (
                        "right_dimension",
                        r.right_dimension
                            .as_ref()
                            .map(dim_to_json)
                            .unwrap_or(JsonValue::Null),
                    ),
                    ("message", JsonValue::string(r.message)),
                ])
            })
            .collect();
        Ok(JsonValue::object_from_pairs(&[(
            "results",
            JsonValue::Array(results),
        )]))
    }
}

// ── 构造子（声明表 make 用） ──

/// 声明表构造子：单位解析服务
pub fn mk_from_unit() -> std::sync::Arc<dyn NativeService> {
    std::sync::Arc::new(DimensionFromUnit)
}

/// 声明表构造子：量纲比较服务
pub fn mk_compare() -> std::sync::Arc<dyn NativeService> {
    std::sync::Arc::new(DimensionCompareUnits)
}

/// 声明表构造子：公式验证服务
pub fn mk_verify_formula() -> std::sync::Arc<dyn NativeService> {
    std::sync::Arc::new(DimensionVerifyFormula)
}

// ============================================================================
// 规则静态分析三服务（112 阶段 2，设计文档 §五）
// ============================================================================

use std::collections::BTreeSet;

use crate::backward::plan as backward_plan;
use crate::backward::{DEFAULT_MAX_DEPTH, MAX_DEPTH_CAP};
use crate::conflict::{detect_conflicts, ConflictReport};
use crate::convergence::check as convergence_check;
use crate::cycle::{detect_cycles, CycleReport};
use crate::effect::predict as effect_predict;
use crate::forward_sim::{simulate as forward_simulate, DEFAULT_MAX_STEPS, MAX_STEPS_CAP};
use crate::rw_extract::{extract, RuleRw};
type AvailSet = std::collections::BTreeSet<String>;

/// 公共入参：`{"rules": [...], "sources": [...], "include_constitution"?: bool}`
struct RulesetArgs<'a> {
    rules: &'a [JsonValue],
    sources: Vec<String>,
    include_constitution: bool,
}

/// 解析规则集公共入参（fail-fast 附指引）
fn ruleset_args<'a>(args: &'a JsonValue) -> Result<RulesetArgs<'a>, String> {
    let rules = match args.get("rules") {
        Some(JsonValue::Array(items)) => items,
        Some(_) => {
            return Err(
                "参数 rules 须为数组 — 自诊断指引: 规则集为 core_eval 形态 JSON 数组\
                 （合并列表 + 来源标签等长对齐）"
                    .to_string(),
            )
        }
        None => {
            return Err("缺少必填参数 rules — 自诊断指引: 形如 {\"rules\": [...], \
                 \"sources\": [\"core_eval\", \"server_eval\", \"rules/x.json\", ...]}"
                .to_string())
        }
    };
    let sources = match args.get("sources") {
        Some(JsonValue::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    JsonValue::String(s) => out.push(s.to_string()),
                    _ => {
                        return Err("参数 sources 须为字符串数组 — 自诊断指引: 来源标签形如 \
                             \"core_eval\" / \"server_eval\" / \"rules/<相对路径>\""
                            .to_string())
                    }
                }
            }
            out
        }
        Some(_) => return Err("参数 sources 须为字符串数组".to_string()),
        None => {
            return Err(
                "缺少必填参数 sources — 自诊断指引: 与 rules 等长的来源标签数组\
                 （对齐 RulesetLayout 合并序：core_eval 在前 + rules_dir 字典序）"
                    .to_string(),
            )
        }
    };
    let include_constitution = match args.get("include_constitution") {
        Some(JsonValue::Bool(b)) => *b,
        Some(_) => {
            return Err(
                "参数 include_constitution 须为布尔值 — 自诊断指引: 缺省 false（宪法段豁免）"
                    .to_string(),
            )
        }
        None => false,
    };
    Ok(RulesetArgs {
        rules,
        sources,
        include_constitution,
    })
}

/// RuleRw → JsonValue（诚实标注位全量透出）
fn rw_to_json(rw: &RuleRw) -> JsonValue {
    let set_json = |s: &BTreeSet<String>| {
        JsonValue::Array(s.iter().map(|v| JsonValue::string(v.clone())).collect())
    };
    let const_pairs: Vec<JsonValue> = rw
        .const_writes
        .iter()
        .map(|(k, v)| {
            JsonValue::object_from_pairs(&[
                ("path", JsonValue::string(k.clone())),
                ("literal", JsonValue::string(v.clone())),
            ])
        })
        .collect();
    let io_pairs: Vec<JsonValue> = rw
        .ios
        .iter()
        .map(|e| {
            JsonValue::object_from_pairs(&[
                ("io_type", JsonValue::string(e.io_type.clone())),
                (
                    "service_name",
                    match &e.service_name {
                        Some(s) => JsonValue::string(s.clone()),
                        None => JsonValue::Null,
                    },
                ),
            ])
        })
        .collect();
    JsonValue::object_from_pairs(&[
        ("id", JsonValue::string(rw.id.clone())),
        ("source", JsonValue::string(rw.source.clone())),
        (
            "index",
            JsonValue::Integer(i64::try_from(rw.index).unwrap_or(-1)),
        ),
        ("reads", set_json(&rw.reads)),
        ("writes", set_json(&rw.writes)),
        ("const_writes", JsonValue::Array(const_pairs)),
        ("ios", JsonValue::Array(io_pairs)),
        ("instr_types", set_json(&rw.instr_types)),
        ("domain_dyn", JsonValue::Bool(rw.domain_dyn)),
        ("write_dyn", JsonValue::Bool(rw.write_dyn)),
        ("domain_universal", JsonValue::Bool(rw.domain_universal)),
        ("excluded", JsonValue::Bool(rw.excluded)),
    ])
}

fn cycles_to_json(report: &CycleReport) -> JsonValue {
    let cycle_list = |cycles: &[Vec<String>]| {
        JsonValue::Array(
            cycles
                .iter()
                .map(|c| {
                    JsonValue::Array(c.iter().map(|id| JsonValue::string(id.clone())).collect())
                })
                .collect(),
        )
    };
    JsonValue::object_from_pairs(&[
        ("has_cycles", JsonValue::Bool(report.has_cycles)),
        (
            "cycle_count",
            JsonValue::Integer(i64::try_from(report.cycle_count).unwrap_or(-1)),
        ),
        ("static_cycles", cycle_list(&report.static_cycles)),
        (
            "conservative_cycles",
            cycle_list(&report.conservative_cycles),
        ),
        (
            "topological_order",
            JsonValue::Array(
                report
                    .topological_order
                    .iter()
                    .map(|id| JsonValue::string(id.clone()))
                    .collect(),
            ),
        ),
        (
            "safe_rules",
            JsonValue::Array(
                report
                    .safe_rules
                    .iter()
                    .map(|id| JsonValue::string(id.clone()))
                    .collect(),
            ),
        ),
        (
            "unsafe_rules",
            JsonValue::Array(
                report
                    .unsafe_rules
                    .iter()
                    .map(|id| JsonValue::string(id.clone()))
                    .collect(),
            ),
        ),
        (
            "excluded",
            JsonValue::Array(
                report
                    .excluded
                    .iter()
                    .map(|id| JsonValue::string(id.clone()))
                    .collect(),
            ),
        ),
    ])
}

fn conflicts_to_json(report: &ConflictReport) -> JsonValue {
    let items: Vec<JsonValue> = report
        .conflicts
        .iter()
        .map(|c| {
            JsonValue::object_from_pairs(&[
                ("rule_id_1", JsonValue::string(c.rule_id_1.clone())),
                ("rule_id_2", JsonValue::string(c.rule_id_2.clone())),
                ("conflict_type", JsonValue::string(c.conflict_type.clone())),
                ("description", JsonValue::string(c.description.clone())),
                ("conservative", JsonValue::Bool(c.conservative)),
            ])
        })
        .collect();
    JsonValue::object_from_pairs(&[
        ("has_conflicts", JsonValue::Bool(report.has_conflicts)),
        (
            "conflict_count",
            JsonValue::Integer(i64::try_from(report.conflict_count).unwrap_or(-1)),
        ),
        ("conflicts", JsonValue::Array(items)),
        (
            "excluded",
            JsonValue::Array(
                report
                    .excluded
                    .iter()
                    .map(|id| JsonValue::string(id.clone()))
                    .collect(),
            ),
        ),
    ])
}

// ── inference_rw_extract ──

/// 规则读写集静态提取服务（环/冲突检测的共享底座，调试与 CI 直连用）
pub struct RuleRwExtract;

impl NativeService for RuleRwExtract {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let parsed = ruleset_args(args)?;
        let rws = extract(parsed.rules, &parsed.sources, parsed.include_constitution)?;
        let items: Vec<JsonValue> = rws.iter().map(rw_to_json).collect();
        Ok(JsonValue::object_from_pairs(&[(
            "results",
            JsonValue::Array(items),
        )]))
    }
}

// ── inference_cycle_detect ──

/// 规则集内环路检测服务（静态/保守分层，宪法段默认豁免显式标注）
pub struct RuleCycleDetect;

impl NativeService for RuleCycleDetect {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let parsed = ruleset_args(args)?;
        let rws = extract(parsed.rules, &parsed.sources, parsed.include_constitution)?;
        let report = detect_cycles(&rws)?;
        Ok(cycles_to_json(&report))
    }
}

// ── inference_conflict_detect ──

/// 规则集内冲突检测服务（六类型学，R-1 裁定口径）
pub struct RuleConflictDetect;

impl NativeService for RuleConflictDetect {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let parsed = ruleset_args(args)?;
        let rws = extract(parsed.rules, &parsed.sources, parsed.include_constitution)?;
        let report = detect_conflicts(&rws)?;
        Ok(conflicts_to_json(&report))
    }
}

/// 声明表构造子：读写集提取服务
pub fn mk_rw_extract() -> std::sync::Arc<dyn NativeService> {
    std::sync::Arc::new(RuleRwExtract)
}

/// 声明表构造子：环路检测服务
pub fn mk_cycle_detect() -> std::sync::Arc<dyn NativeService> {
    std::sync::Arc::new(RuleCycleDetect)
}

/// 声明表构造子：冲突检测服务
pub fn mk_conflict_detect() -> std::sync::Arc<dyn NativeService> {
    std::sync::Arc::new(RuleConflictDetect)
}

// ============================================================================
// 规则推理四服务（112 阶段 3，设计文档 §二，R-1~R-3 裁定口径）
// ============================================================================

/// 前向链仿真服务：合成 `__exec__` 文档上迭代到不动点（R-1 语义）
pub struct RuleForwardSimulate;

impl NativeService for RuleForwardSimulate {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let parsed = ruleset_args(args)?;
        let max_steps = match args.get("max_steps") {
            Some(JsonValue::Integer(n)) if *n >= 0 => {
                let n = usize::try_from(*n).unwrap_or(MAX_STEPS_CAP);
                if n > MAX_STEPS_CAP {
                    return Err(format!(
                        "max_steps={n} 超过硬上限 {MAX_STEPS_CAP} — 自诊断指引: \
                         仿真是分析工具不是执行器"
                    ));
                }
                n
            }
            Some(JsonValue::Integer(_)) => return Err("参数 max_steps 须为非负整数".to_string()),
            Some(_) => return Err("参数 max_steps 须为整数".to_string()),
            None => DEFAULT_MAX_STEPS,
        };
        let document = match args.get("initial_document") {
            Some(d @ JsonValue::Object(_)) => d,
            Some(_) => {
                return Err("参数 initial_document 须为对象 — 自诊断指引: 形如 \
                     {\"instruction\": {...}, \"payload\": {...}, \"queue\": [...]}"
                    .to_string())
            }
            None => {
                return Err(
                    "缺少必填参数 initial_document — 自诊断指引: 合成 __exec__ 文档，\
                     形如 {\"instruction\": {...}, \"payload\": {...}, \"queue\": [...]}"
                        .to_string(),
                )
            }
        };
        let report = forward_simulate(parsed.rules, &parsed.sources, document, max_steps)?;
        Ok(JsonValue::object_from_pairs(&[
            ("stopped_reason", JsonValue::string(report.stopped_reason)),
            (
                "steps",
                JsonValue::Integer(i64::try_from(report.steps).unwrap_or(-1)),
            ),
            (
                "applied",
                JsonValue::Array(
                    report
                        .applied
                        .iter()
                        .map(|id| JsonValue::string(id.clone()))
                        .collect(),
                ),
            ),
            ("final_document", report.final_document.clone()),
            (
                "reached_fixed_point",
                JsonValue::Bool(report.reached_fixed_point),
            ),
            (
                "blocked_on_io",
                match &report.blocked_on_io {
                    Some((id, io)) => JsonValue::object_from_pairs(&[
                        ("rule_id", JsonValue::string(id.clone())),
                        ("io_type", JsonValue::string(io.clone())),
                    ]),
                    None => JsonValue::Null,
                },
            ),
            (
                "halted",
                match &report.halted {
                    Some((id, reason)) => JsonValue::object_from_pairs(&[
                        ("rule_id", JsonValue::string(id.clone())),
                        ("reason", JsonValue::string(reason.clone())),
                    ]),
                    None => JsonValue::Null,
                },
            ),
            (
                "excluded",
                JsonValue::Array(
                    report
                        .excluded
                        .iter()
                        .map(|id| JsonValue::string(id.clone()))
                        .collect(),
                ),
            ),
            (
                "simulation_scope",
                JsonValue::string(
                    "合成文档仿真:无 I/O 响应/无并发/无治理面干预,不动点≠真实系统行为",
                ),
            ),
        ]))
    }
}

/// 后向链规划服务：目标路径反推生产者链（复用 rw_extract）
pub struct RuleBackwardPlan;

impl NativeService for RuleBackwardPlan {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let parsed = ruleset_args(args)?;
        let target = match args.get("target_path") {
            Some(JsonValue::String(s)) => s.to_string(),
            Some(_) => {
                return Err("参数 target_path 须为字符串 — 自诊断指引: 如 \"payload.x\"".to_string())
            }
            None => {
                return Err(
                    "缺少必填参数 target_path — 自诊断指引: 目标写路径(规范化形态,如 payload.x)"
                        .to_string(),
                )
            }
        };
        let max_depth = match args.get("max_depth") {
            Some(JsonValue::Integer(n)) if *n >= 0 => {
                let n = usize::try_from(*n).unwrap_or(MAX_DEPTH_CAP);
                if n > MAX_DEPTH_CAP {
                    return Err(format!("max_depth={n} 超过硬上限 {MAX_DEPTH_CAP}"));
                }
                n
            }
            Some(JsonValue::Integer(_)) => return Err("参数 max_depth 须为非负整数".to_string()),
            Some(_) => return Err("参数 max_depth 须为整数".to_string()),
            None => DEFAULT_MAX_DEPTH,
        };
        let mut available: AvailSet = AvailSet::new();
        if let Some(JsonValue::Array(items)) = args.get("available_paths") {
            for item in items {
                match item {
                    JsonValue::String(s) => {
                        available.insert(s.to_string());
                    }
                    _ => {
                        return Err(
                            "参数 available_paths 须为字符串数组 — 自诊断指引: 初始可用路径集"
                                .to_string(),
                        )
                    }
                }
            }
        }
        let rws = extract(parsed.rules, &parsed.sources, parsed.include_constitution)?;
        let report = backward_plan(&rws, &target, &available, max_depth);
        Ok(JsonValue::object_from_pairs(&[
            ("achievable", JsonValue::Bool(report.achievable)),
            (
                "rule_sequence",
                JsonValue::Array(
                    report
                        .rule_sequence
                        .iter()
                        .map(|id| JsonValue::string(id.clone()))
                        .collect(),
                ),
            ),
            (
                "required_inputs",
                JsonValue::Array(
                    report
                        .required_inputs
                        .iter()
                        .map(|p| JsonValue::string(p.clone()))
                        .collect(),
                ),
            ),
            (
                "depth",
                JsonValue::Integer(i64::try_from(report.depth).unwrap_or(-1)),
            ),
            ("target_path", JsonValue::string(report.target_path.clone())),
            ("conservative", JsonValue::Bool(report.conservative)),
            (
                "excluded",
                JsonValue::Array(
                    report
                        .excluded
                        .iter()
                        .map(|id| JsonValue::string(id.clone()))
                        .collect(),
                ),
            ),
            ("reason", JsonValue::string(report.reason.clone())),
        ]))
    }
}

/// 效果预测服务：rw_extract 之上的影响分级薄层（R-3）
pub struct RuleEffectPredict;

impl NativeService for RuleEffectPredict {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let parsed = ruleset_args(args)?;
        let rws = extract(parsed.rules, &parsed.sources, parsed.include_constitution)?;
        let effects = effect_predict(&rws);
        let items: Vec<JsonValue> = effects
            .iter()
            .map(|e| {
                JsonValue::object_from_pairs(&[
                    ("id", JsonValue::string(e.id.clone())),
                    ("impact", JsonValue::string(e.impact.as_str())),
                    (
                        "reads",
                        JsonValue::Array(
                            e.reads
                                .iter()
                                .map(|p| JsonValue::string(p.clone()))
                                .collect(),
                        ),
                    ),
                    (
                        "writes",
                        JsonValue::Array(
                            e.writes
                                .iter()
                                .map(|p| JsonValue::string(p.clone()))
                                .collect(),
                        ),
                    ),
                    (
                        "warnings",
                        JsonValue::Array(
                            e.warnings
                                .iter()
                                .map(|w| JsonValue::string(w.clone()))
                                .collect(),
                        ),
                    ),
                ])
            })
            .collect();
        Ok(JsonValue::object_from_pairs(&[(
            "results",
            JsonValue::Array(items),
        )]))
    }
}

/// 收敛检测服务：去重后口径（环复用 cycle_detect，饥饿交由 shadow_risk）
pub struct RuleConvergenceCheck;

impl NativeService for RuleConvergenceCheck {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let parsed = ruleset_args(args)?;
        let rws = extract(parsed.rules, &parsed.sources, parsed.include_constitution)?;
        let report = convergence_check(&rws)?;
        let items: Vec<JsonValue> = report
            .issues
            .iter()
            .map(|i| {
                JsonValue::object_from_pairs(&[
                    ("issue_type", JsonValue::string(i.issue_type)),
                    ("rule_id", JsonValue::string(i.rule_id.clone())),
                    ("description", JsonValue::string(i.description.clone())),
                    ("conservative", JsonValue::Bool(i.conservative)),
                ])
            })
            .collect();
        Ok(JsonValue::object_from_pairs(&[
            ("converges", JsonValue::Bool(report.converges)),
            ("undetermined", JsonValue::Bool(report.undetermined)),
            (
                "estimated_passes",
                match report.estimated_passes {
                    Some(n) => JsonValue::Integer(i64::try_from(n).unwrap_or(-1)),
                    None => JsonValue::Null,
                },
            ),
            ("issues", JsonValue::Array(items)),
            (
                "excluded",
                JsonValue::Array(
                    report
                        .excluded
                        .iter()
                        .map(|id| JsonValue::string(id.clone()))
                        .collect(),
                ),
            ),
        ]))
    }
}

/// 声明表构造子：前向链仿真服务
pub fn mk_forward_simulate() -> std::sync::Arc<dyn NativeService> {
    std::sync::Arc::new(RuleForwardSimulate)
}

/// 声明表构造子：后向链规划服务
pub fn mk_backward_plan() -> std::sync::Arc<dyn NativeService> {
    std::sync::Arc::new(RuleBackwardPlan)
}

/// 声明表构造子：效果预测服务
pub fn mk_effect_predict() -> std::sync::Arc<dyn NativeService> {
    std::sync::Arc::new(RuleEffectPredict)
}

/// 声明表构造子：收敛检测服务
pub fn mk_convergence_check() -> std::sync::Arc<dyn NativeService> {
    std::sync::Arc::new(RuleConvergenceCheck)
}

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

/// 从 args 解析可选 `max_steps`（缺省 [`DEFAULT_MAX_STEPS`]；类型 / 上限校验 fail-fast）。
fn parse_max_steps(args: &JsonValue) -> Result<usize, String> {
    match args.get("max_steps") {
        Some(JsonValue::Integer(n)) if *n >= 0 => {
            let n = usize::try_from(*n).unwrap_or(MAX_STEPS_CAP);
            if n > MAX_STEPS_CAP {
                return Err(format!(
                    "max_steps={n} 超过硬上限 {MAX_STEPS_CAP} — 自诊断指引: \
                     仿真是分析工具不是执行器"
                ));
            }
            Ok(n)
        }
        Some(JsonValue::Integer(_)) => Err("参数 max_steps 须为非负整数".to_string()),
        Some(_) => Err("参数 max_steps 须为整数".to_string()),
        None => Ok(DEFAULT_MAX_STEPS),
    }
}

/// 从 args 提取对象形态的合成文档参数（缺字段 / 非对象 → Err，fail-fast 附指引）。
fn doc_arg<'a>(args: &'a JsonValue, key: &str) -> Result<&'a JsonValue, String> {
    match args.get(key) {
        Some(d @ JsonValue::Object(_)) => Ok(d),
        Some(_) => Err(format!(
            "参数 {key} 须为对象 — 自诊断指引: 合成 __exec__ 文档,形如 \
             {{\"instruction\": {{...}}, \"payload\": {{...}}, \"queue\": [...]}}"
        )),
        None => Err(format!(
            "缺少必填参数 {key} — 自诊断指引: 合成 __exec__ 文档,形如 \
             {{\"instruction\": {{...}}, \"payload\": {{...}}, \"queue\": [...]}}"
        )),
    }
}

/// 从 args 解析一组命名规则集（`{rules_key}` / `{sources_key}`），来源标签克隆持有。
fn parse_named_ruleset<'a>(
    args: &'a JsonValue,
    rules_key: &str,
    sources_key: &str,
) -> Result<(&'a [JsonValue], Vec<String>), String> {
    let rules = match args.get(rules_key) {
        Some(JsonValue::Array(items)) => items,
        Some(_) => {
            return Err(format!(
                "参数 {rules_key} 须为数组 — 自诊断指引: 规则集为合并列表形态 JSON 数组"
            ))
        }
        None => {
            return Err(format!(
                "缺少必填参数 {rules_key} — 自诊断指引: 形如 {{\"{rules_key}\": [...], \
                 \"{sources_key}\": [\"core_eval\", \"server_eval\", \"rules/x.json\", ...]}}"
            ))
        }
    };
    let sources = match args.get(sources_key) {
        Some(JsonValue::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    JsonValue::String(s) => out.push(s.to_string()),
                    _ => {
                        return Err(format!(
                            "参数 {sources_key} 须为字符串数组 — 自诊断指引: 来源标签形如 \
                             \"core_eval\" / \"server_eval\" / \"rules/<相对路径>\""
                        ))
                    }
                }
            }
            out
        }
        Some(_) => return Err(format!("参数 {sources_key} 须为字符串数组")),
        None => {
            return Err(format!(
                "缺少必填参数 {sources_key} — 自诊断指引: 与 {rules_key} 等长的来源标签数组\
                 （对齐 RulesetLayout 合并序：core_eval 在前 + rules_dir 字典序）"
            ))
        }
    };
    if rules.len() != sources.len() {
        return Err(format!(
            "{rules_key}({}) 与 {sources_key}({}) 长度不一致 — 自诊断指引: \
             来源标签须与规则列表等长且按合并序对齐",
            rules.len(),
            sources.len()
        ));
    }
    Ok((rules, sources))
}

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
        Ok(JsonValue::object_from_pairs(&[
            ("status", JsonValue::string("determined")),
            ("determinism", JsonValue::string("bitwise_replayable")),
            ("results", JsonValue::Array(results)),
        ]))
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
        Ok(JsonValue::object_from_pairs(&[
            ("status", JsonValue::string("determined")),
            ("determinism", JsonValue::string("bitwise_replayable")),
            ("results", JsonValue::Array(results)),
        ]))
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
        Ok(JsonValue::object_from_pairs(&[
            ("status", JsonValue::string("determined")),
            ("determinism", JsonValue::string("bitwise_replayable")),
            ("results", JsonValue::Array(results)),
        ]))
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
// 规则静态分析三服务（设计文档 §五）
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
    let (rules, sources) = parse_named_ruleset(args, "rules", "sources")?;
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
        // 统一信封 v1:静态/保守分层均有确定结论,无 undetermined 路径
        ("status", JsonValue::string("determined")),
        ("determinism", JsonValue::string("bitwise_replayable")),
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
        // 统一信封 v1:六类型学冲突判定均有确定结论,无 undetermined 路径
        ("status", JsonValue::string("determined")),
        ("determinism", JsonValue::string("bitwise_replayable")),
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
        Ok(JsonValue::object_from_pairs(&[
            ("status", JsonValue::string("determined")),
            ("determinism", JsonValue::string("bitwise_replayable")),
            ("results", JsonValue::Array(items)),
        ]))
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

/// 规则集内冲突检测服务（六类型学）
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
// 规则推理四服务（设计文档 §二）
// ============================================================================

/// 前向链仿真服务：合成 `__exec__` 文档上迭代到不动点
pub struct RuleForwardSimulate;

impl NativeService for RuleForwardSimulate {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let parsed = ruleset_args(args)?;
        let max_steps = parse_max_steps(args)?;
        let document = doc_arg(args, "initial_document")?;
        let report = forward_simulate(parsed.rules, &parsed.sources, document, max_steps)?;
        Ok(JsonValue::object_from_pairs(&[
            ("status", JsonValue::string("determined")),
            ("determinism", JsonValue::string("bitwise_replayable")),
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
        // 空/纯空白目标路径 fail-fast：归一后为空串，落入「无生产者」分支会回
        // achievable=true + required_inputs=[""] —— 与「须外部提供」语义不符，误导调用方。
        if target.trim().is_empty() {
            return Err(
                "参数 target_path 不得为空 — 自诊断指引: 目标写路径(如 payload.x)".to_string(),
            );
        }
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
            ("status", JsonValue::string("determined")),
            ("determinism", JsonValue::string("bitwise_replayable")),
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
            (
                "target_path_input",
                JsonValue::string(report.target_path_input.clone()),
            ),
            (
                "produced_by_rules",
                JsonValue::Bool(report.produced_by_rules),
            ),
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

/// 效果预测服务：rw_extract 之上的影响分级薄层
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
        Ok(JsonValue::object_from_pairs(&[
            ("status", JsonValue::string("determined")),
            ("determinism", JsonValue::string("bitwise_replayable")),
            ("results", JsonValue::Array(items)),
        ]))
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
        // 统一信封 v1:status 与既有 undetermined 字段恒等约束
        // (undetermined=true ⇔ status="undetermined"),既有字段原位保留;
        // reason 仅 undetermined 时携带(静态分析不可判定子集)
        let mut out = vec![
            (
                "status",
                if report.undetermined {
                    JsonValue::string("undetermined")
                } else {
                    JsonValue::string("determined")
                },
            ),
            ("determinism", JsonValue::string("bitwise_replayable")),
        ];
        if report.undetermined {
            out.push((
                "reason",
                JsonValue::string("unresolvable_by_static_analysis"),
            ));
        }
        out.extend([
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
        ]);
        Ok(JsonValue::object_from_pairs(&out))
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

// ============================================================================
// 规则推理三服务（阶段 4：反事实 / 等价探针 / 差分）
// ============================================================================

/// 仿真控制面摘要（终止原因 + 步数 + I/O 断流 + enforce 短路）
fn sim_brief(report: &crate::forward_sim::SimulationReport) -> JsonValue {
    let io_pair = |io: &Option<(String, String)>| match io {
        Some((id, t)) => JsonValue::object_from_pairs(&[
            ("rule_id", JsonValue::string(id.clone())),
            ("io_type", JsonValue::string(t.clone())),
        ]),
        None => JsonValue::Null,
    };
    let halt_pair = |h: &Option<(String, String)>| match h {
        Some((id, reason)) => JsonValue::object_from_pairs(&[
            ("rule_id", JsonValue::string(id.clone())),
            ("reason", JsonValue::string(reason.clone())),
        ]),
        None => JsonValue::Null,
    };
    JsonValue::object_from_pairs(&[
        ("stopped_reason", JsonValue::string(report.stopped_reason)),
        (
            "steps",
            JsonValue::Integer(i64::try_from(report.steps).unwrap_or(-1)),
        ),
        ("blocked_on_io", io_pair(&report.blocked_on_io)),
        ("halted", halt_pair(&report.halted)),
    ])
}

/// 反事实差异分析服务：双前向仿真 + 终态可观测面 diff
pub struct RuleCounterfactualAnalyze;

impl NativeService for RuleCounterfactualAnalyze {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let parsed = ruleset_args(args)?;
        let max_steps = parse_max_steps(args)?;
        let original_document = doc_arg(args, "original_document")?;
        let hypothetical_document = doc_arg(args, "hypothetical_document")?;
        let report = crate::counterfactual::analyze(
            parsed.rules,
            &parsed.sources,
            original_document,
            hypothetical_document,
            max_steps,
        )?;
        let diffs: Vec<JsonValue> = report
            .diffs
            .iter()
            .map(|d| {
                JsonValue::object_from_pairs(&[
                    ("path", JsonValue::string(d.path.clone())),
                    ("kind", JsonValue::string(d.kind)),
                    (
                        "original_value",
                        d.original_value.clone().unwrap_or(JsonValue::Null),
                    ),
                    (
                        "hypothetical_value",
                        d.hypothetical_value.clone().unwrap_or(JsonValue::Null),
                    ),
                ])
            })
            .collect();
        Ok(JsonValue::object_from_pairs(&[
            ("status", JsonValue::string("determined")),
            ("determinism", JsonValue::string("bitwise_replayable")),
            ("original", sim_brief(&report.original)),
            ("hypothetical", sim_brief(&report.hypothetical)),
            ("diffs", JsonValue::Array(diffs)),
            ("truncated", JsonValue::Bool(report.truncated)),
            ("explanation", JsonValue::string(report.explanation)),
            (
                "simulation_scope",
                JsonValue::string(
                    "同前向链仿真:无 I/O 响应/无并发/无治理面干预,不动点≠真实系统行为;只对比终态,无步级归因",
                ),
            ),
        ]))
    }
}

/// 等价探针服务：结构比较 + 有限探针行为证据（probe_equivalent ≠ 数学证明）
pub struct RuleEquivalenceProbe;

impl NativeService for RuleEquivalenceProbe {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let (rules1, sources1) = parse_named_ruleset(args, "rules1", "sources1")?;
        let (rules2, sources2) = parse_named_ruleset(args, "rules2", "sources2")?;
        if args.get("max_steps").is_some() {
            return Err(
                "本服务不接受 max_steps — 自诊断指引: 探针仿真统一使用缺省步数上限\
                 （防止单探针步数爆炸掩盖差异面）"
                    .to_string(),
            );
        }
        let probe_inputs: &[JsonValue] =
            match args.get("probe_inputs") {
                Some(JsonValue::Array(items)) => items,
                Some(_) => return Err(
                    "参数 probe_inputs 须为数组 — 自诊断指引: 探针输入是合成 __exec__ 文档数组,\
                     缺省视为空(只做结构比较)"
                        .to_string(),
                ),
                None => &[],
            };
        let report = crate::equivalence::compare_rulesets(
            rules1,
            &sources1,
            rules2,
            &sources2,
            probe_inputs,
        )?;
        let counterexample = match &report.counterexample {
            Some(cx) => JsonValue::object_from_pairs(&[
                (
                    "probe_index",
                    JsonValue::Integer(i64::try_from(cx.probe_index).unwrap_or(-1)),
                ),
                ("summary", JsonValue::string(cx.summary.clone())),
            ]),
            None => JsonValue::Null,
        };
        Ok(JsonValue::object_from_pairs(&[
            ("status", JsonValue::string("determined")),
            ("determinism", JsonValue::string("bitwise_replayable")),
            ("verdict", JsonValue::string(report.verdict)),
            (
                "structural_identical",
                JsonValue::Bool(report.structural_identical),
            ),
            (
                "probes_run",
                JsonValue::Integer(i64::try_from(report.probes_run).unwrap_or(-1)),
            ),
            ("counterexample", counterexample),
            ("note", JsonValue::string(report.note)),
        ]))
    }
}

/// 规则集差分服务：读写集结构化 diff（保守方向声明）
pub struct RuleDifferentialDiff;

impl NativeService for RuleDifferentialDiff {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let (rules1, sources1) = parse_named_ruleset(args, "rules1", "sources1")?;
        let (rules2, sources2) = parse_named_ruleset(args, "rules2", "sources2")?;
        let include_constitution =
            match args.get("include_constitution") {
                Some(JsonValue::Bool(b)) => *b,
                Some(_) => return Err(
                    "参数 include_constitution 须为布尔值 — 自诊断指引: 缺省 false（宪法段豁免）"
                        .to_string(),
                ),
                None => false,
            };
        let report = crate::differential::diff_rulesets(
            rules1,
            &sources1,
            rules2,
            &sources2,
            include_constitution,
        )?;
        let str_set = |v: &[String]| {
            JsonValue::Array(v.iter().map(|s| JsonValue::string(s.clone())).collect())
        };
        let indices: Vec<JsonValue> = report
            .changed_indices
            .iter()
            .map(|d| {
                JsonValue::object_from_pairs(&[
                    (
                        "index",
                        JsonValue::Integer(i64::try_from(d.index).unwrap_or(-1)),
                    ),
                    ("source", JsonValue::string(d.source.clone())),
                ])
            })
            .collect();
        Ok(JsonValue::object_from_pairs(&[
            ("status", JsonValue::string("determined")),
            ("determinism", JsonValue::string("bitwise_replayable")),
            ("added_writes", str_set(&report.set_diff.added_writes)),
            ("removed_writes", str_set(&report.set_diff.removed_writes)),
            ("added_reads", str_set(&report.set_diff.added_reads)),
            ("removed_reads", str_set(&report.set_diff.removed_reads)),
            ("added_ios", str_set(&report.set_diff.added_ios)),
            ("removed_ios", str_set(&report.set_diff.removed_ios)),
            ("aligned", JsonValue::Bool(report.aligned)),
            ("changed_indices", JsonValue::Array(indices)),
            (
                "set1_dynamic_writes",
                JsonValue::Integer(i64::try_from(report.set1_dynamic_writes).unwrap_or(-1)),
            ),
            (
                "set2_dynamic_writes",
                JsonValue::Integer(i64::try_from(report.set2_dynamic_writes).unwrap_or(-1)),
            ),
            (
                "set1_dynamic_domains",
                JsonValue::Integer(i64::try_from(report.set1_dynamic_domains).unwrap_or(-1)),
            ),
            (
                "set2_dynamic_domains",
                JsonValue::Integer(i64::try_from(report.set2_dynamic_domains).unwrap_or(-1)),
            ),
            ("note", JsonValue::string(report.note)),
        ]))
    }
}

/// 声明表构造子：反事实差异分析服务
pub fn mk_counterfactual_analyze() -> std::sync::Arc<dyn NativeService> {
    std::sync::Arc::new(RuleCounterfactualAnalyze)
}

/// 声明表构造子：等价探针服务
pub fn mk_equivalence_probe() -> std::sync::Arc<dyn NativeService> {
    std::sync::Arc::new(RuleEquivalenceProbe)
}

/// 声明表构造子：规则集差分服务
pub fn mk_differential_diff() -> std::sync::Arc<dyn NativeService> {
    std::sync::Arc::new(RuleDifferentialDiff)
}

/// 反事实参数扫描服务：参数路径逐值双仿真，每点差异面 + 汇总
/// （counterfactual_analyze 的批量形态，语义完全复用）
pub struct RuleCounterfactualSweep;

impl NativeService for RuleCounterfactualSweep {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let parsed = ruleset_args(args)?;
        let max_steps = parse_max_steps(args)?;
        let base_document = doc_arg(args, "document")?;
        let param_path = args
            .get("param_path")
            .and_then(JsonValue::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                "缺少必填参数 param_path — 自诊断指引: 扫描的参数点路径,形如 \
                 \"payload.threshold\"（点分隔）"
                    .to_string()
            })?;
        let values: &[JsonValue] = args
            .get("values")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| {
                "缺少必填参数 values — 自诊断指引: 值序列由调用方显式给出（数字/字符串皆可，\
                 原样拼入文档，零类型强转），如 [1, 5, 10]"
                    .to_string()
            })?;
        let report = crate::sweep::sweep(
            parsed.rules,
            &parsed.sources,
            base_document,
            param_path,
            values,
            max_steps,
        )?;
        let points: Vec<JsonValue> = report
            .points
            .iter()
            .map(|p| {
                JsonValue::object_from_pairs(&[
                    ("value", p.value.clone()),
                    (
                        "diff_count",
                        JsonValue::Integer(i64::try_from(p.diff_count).unwrap_or(-1)),
                    ),
                    (
                        "paths",
                        JsonValue::Array(
                            p.paths
                                .iter()
                                .map(|s| JsonValue::string(s.clone()))
                                .collect(),
                        ),
                    ),
                    (
                        "base_stopped_reason",
                        JsonValue::string(p.base_stopped_reason),
                    ),
                    ("truncated", JsonValue::Bool(p.truncated)),
                ])
            })
            .collect();
        Ok(JsonValue::object_from_pairs(&[
            ("status", JsonValue::string("determined")),
            ("determinism", JsonValue::string("bitwise_replayable")),
            ("param_path", JsonValue::string(report.param_path)),
            ("points", JsonValue::Array(points)),
            (
                "points_total",
                JsonValue::Integer(i64::try_from(report.points.len()).unwrap_or(-1)),
            ),
            (
                "points_with_diffs",
                JsonValue::Integer(i64::try_from(report.points_with_diffs).unwrap_or(-1)),
            ),
            (
                "max_diff_count",
                JsonValue::Integer(i64::try_from(report.max_diff_count).unwrap_or(-1)),
            ),
            (
                "note",
                JsonValue::string(
                    "逐点双仿真diff,结果按输入序独立输出,不做插值或趋势推断;值序列由调用方显式给出",
                ),
            ),
        ]))
    }
}

/// 声明表构造子：反事实参数扫描服务
pub fn mk_counterfactual_sweep() -> std::sync::Arc<dyn NativeService> {
    std::sync::Arc::new(RuleCounterfactualSweep)
}

// ============================================================================
// 测试：统一信封保留键（科学计算服务面规范 v1，对齐批守卫）
// ============================================================================

#[cfg(test)]
mod envelope_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use evorule_tcb::JsonValue;

    fn set_rule(attr: &str, value: i64) -> JsonValue {
        JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("attr", JsonValue::string(attr)),
                    ("value", JsonValue::Integer(value)),
                ]),
            ),
        ])
    }

    fn dynamic_write_rule() -> JsonValue {
        // 动态写:静态分析不可判定 → convergence undetermined=true
        JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "attr",
                        JsonValue::string("__exec__.instruction.params.attr"),
                    ),
                    ("value", JsonValue::Integer(1)),
                ]),
            ),
        ])
    }

    fn ruleset(rules: Vec<JsonValue>) -> JsonValue {
        let n = rules.len();
        JsonValue::object_from_pairs(&[
            ("rules", JsonValue::Array(rules)),
            (
                "sources",
                JsonValue::Array(
                    (0..n)
                        .map(|i| JsonValue::string(format!("rules/r{i}.json")))
                        .collect::<Vec<_>>(),
                ),
            ),
        ])
    }

    #[test]
    fn envelope_convergence_status_identity_with_undetermined_field() {
        // 干净规则集:status=determined,无 reason,既有字段原位
        let out = RuleConvergenceCheck
            .execute(&ruleset(vec![set_rule("x", 1)]))
            .unwrap();
        assert_eq!(
            out.get("status").and_then(JsonValue::as_str),
            Some("determined")
        );
        assert_eq!(
            out.get("undetermined").and_then(JsonValue::as_bool),
            Some(false)
        );
        assert_eq!(
            out.get("determinism").and_then(JsonValue::as_str),
            Some("bitwise_replayable")
        );
        assert!(out.get("reason").is_none(), "determined 时不得携带 reason");
        assert!(out.get("converges").is_some() && out.get("issues").is_some());

        // 动态写:undetermined=true ⇔ status=undetermined + reason(恒等约束守卫)
        let out = RuleConvergenceCheck
            .execute(&ruleset(vec![dynamic_write_rule()]))
            .unwrap();
        assert_eq!(
            out.get("status").and_then(JsonValue::as_str),
            Some("undetermined")
        );
        assert_eq!(
            out.get("undetermined").and_then(JsonValue::as_bool),
            Some(true)
        );
        assert_eq!(
            out.get("reason").and_then(JsonValue::as_str),
            Some("unresolvable_by_static_analysis")
        );
    }

    #[test]
    fn envelope_plain_services_have_reserved_keys() {
        // 量纲族(纯逻辑域):status 恒 determined + determinism 保留键
        let args = JsonValue::object_from_pairs(&[(
            "units",
            JsonValue::Array(vec![JsonValue::string("m"), JsonValue::string("m/s")]),
        )]);
        let out = DimensionFromUnit.execute(&args).unwrap();
        assert_eq!(
            out.get("status").and_then(JsonValue::as_str),
            Some("determined")
        );
        assert_eq!(
            out.get("determinism").and_then(JsonValue::as_str),
            Some("bitwise_replayable")
        );
        assert!(out.get("reason").is_none());

        // 环检测(规则静态分析域):同上
        let out = RuleCycleDetect
            .execute(&ruleset(vec![set_rule("x", 1)]))
            .unwrap();
        assert_eq!(
            out.get("status").and_then(JsonValue::as_str),
            Some("determined")
        );
        assert_eq!(
            out.get("determinism").and_then(JsonValue::as_str),
            Some("bitwise_replayable")
        );
    }
}

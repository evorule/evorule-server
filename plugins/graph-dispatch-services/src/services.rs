// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 原生服务实现：常数修订冲击传播分析（确定性静态分析，JSON 入 / JSON 出）。
//!
//! # 服务契约：`graph_shock_propagate`
//!
//! 入参 `{"rule_set": {…}, "revision": {…}, "adjudications": […]}`：
//! - `rule_set`：规则集对象（含 `metadata.formula_registry` 与可选
//!   `metadata.constants`）——原样交依赖图构建器物化，构建错误透传；
//! - `revision`：修订提案 `{constant_id, new_value, new_uncertainty?, source, verified}`——
//!   出处元数据强制（source 非空 + verified 恰为 true，CODATA 级修订不例外）；
//!   `new_value` 修订对象须为图中已声明常数；
//! - `adjudications`：历史裁决清单（元素 {instruction_type, state}），调用方从
//!   审计链 lifecycle 取数后传入，服务不做生命周期过滤。
//!
//! 出参（确定性键序）：
//! `{"affected_instructions":[{instruction_type,depth,via}…按指令排序],
//!   "adjudications_invalidated":[原始条目…保序],
//!   "domains_touched":[[L,M,T,I,Θ,N,J]…字典序去重],
//!   "graph_digest":"blake3:hex","input_digest":"blake3:hex",
//!   "new_value":修订值原样回显,"propagation_depth":u64,
//!   "regime_invalidated":[指令…排序],"shocked_constant":id,
//!   "unreferenced_constants":[常数…字典序]}`
//!
//! `unreferenced_constants` = 声明集（metadata.constants）与引用集（引用边）
//! 差集——已声明但零指令登记引用的常数对冲击传播零半径（地震仪不可见），
//! 本字段使该静默态有痕可观察；与修订对象无关，逐次 invoke 均报告。
//!
//! # 传播语义（五步，全确定性）
//!
//! ① 引用闭包：引用边命中修订常数的全部指令 = 直接受影响（depth=1, via=reference）；
//! ② regime 复查：直接受影响指令在常数域界声明存在时查 new_value——
//!    闭区间 [domain_lo, domain_hi] 之外 → regime 失效；未声明域界 → 只受影响不失效；
//! ③ 量纲耦合层间传播：沿耦合边自「直接失效集」BFS 至不动点——被波及指令
//!    入失效集且入受影响集（via=coupling，depth=BFS 层数+1）；直接失效集为空则不传播；
//! ④ 已裁命题反推：裁决清单中 instruction_type ∈ 失效集的条目原样列入 invalidated；
//! ⑤ propagation_depth = 受影响集最大 depth（无受影响 = 0）。
//!
//! # 确定性
//!
//! 全程纯函数：无时钟、无随机、零 I/O；同输入两次执行输出逐位一致。
//! input_digest 锚定三输入（篡改即红锚：任一输入变 → digest 变 → 复算不一致）。
//! 注：TCB 域小数以字符串形态承载（引擎惯例，serde_to_tcb 同律）——
//! `new_value`/`new_uncertainty` 接受整数与可解析为有限小数的数字字符串。

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;

use evorule_plugin_kit::NativeService;
use evorule_reactor::IoResult;
use evorule_tcb::JsonValue;

use evorule_rule_loader::dep_graph::{build_dependency_graph, DependencyGraph};

/// 受影响深度标记：引用闭包（修订常数被直接引用）
const VIA_REFERENCE: &str = "reference";
/// 受影响深度标记：量纲耦合层间传播
const VIA_COUPLING: &str = "coupling";

/// TCB 域 → serde 域转换（服务边界专用；TCB 无 serde 依赖，跨域交接在此收口）。
///
/// 与仓内 serde_to_tcb 逆语义对齐：Integer/U64 原样成数，字符串原样成串
/// （TCB 域小数以字符串承载的惯例形态在 serde 域保持字符串——由服务层数值
/// 解析器按需解释，转换本身零语义猜测）。
fn tcb_to_serde(v: &JsonValue) -> serde_json::Value {
    match v {
        JsonValue::Null => serde_json::Value::Null,
        JsonValue::Bool(b) => serde_json::Value::Bool(*b),
        JsonValue::Integer(i) => serde_json::Value::Number((*i).into()),
        JsonValue::U64(u) => serde_json::Value::Number((*u).into()),
        JsonValue::String(s) => serde_json::Value::String(s.to_string()),
        JsonValue::Array(items) => {
            serde_json::Value::Array(items.iter().map(tcb_to_serde).collect())
        }
        JsonValue::Object(map) => {
            let mut obj = serde_json::Map::new();
            for (k, val) in map.iter() {
                obj.insert(k.clone(), tcb_to_serde(val));
            }
            serde_json::Value::Object(obj)
        }
    }
}

// ============================================================================
// 入参解析（缺任一必选键 → fail-visible Err 带自诊断指引）
// ============================================================================

/// 从 args 提取对象形态必选参数（缺失 / 非对象 → Err）。
fn object_arg<'a>(args: &'a JsonValue, key: &str) -> Result<&'a JsonValue, String> {
    match args.get(key) {
        Some(v @ JsonValue::Object(_)) => Ok(v),
        Some(_) => Err(format!(
            "参数 {key} 须为对象 — 自诊断指引: 见服务契约（rule_set 为规则集对象, \
             revision 为修订提案对象）"
        )),
        None => Err(format!(
            "缺少必填参数 {key} — 自诊断指引: 入参形如 \
             {{\"rule_set\": {{…}}, \"revision\": {{…}}, \"adjudications\": [...]}}"
        )),
    }
}

/// 提取对象内非空字符串字段（修订提案出处元数据同律：缺失/空串 → Err）。
fn non_empty_str_field(obj: &JsonValue, key: &str, why: &str) -> Result<String, String> {
    match obj.get(key) {
        Some(JsonValue::String(s)) if !s.is_empty() => Ok(s.to_string()),
        Some(JsonValue::String(_)) => Err(format!(
            "revision.{key} 为空串 — {why} — 自诊断指引: 补齐出处元数据后重试"
        )),
        Some(_) => Err(format!("revision.{key} 须为字符串 — {why}")),
        None => Err(format!(
            "revision 缺 {key} — {why} — 自诊断指引: 补齐出处元数据后重试"
        )),
    }
}

/// 提取数值字段（整数 / u64 / 可解析为有限小数的数字字符串——TCB 域小数
/// 以字符串承载的引擎惯例；其余形态 → Err）。
fn numeric_field(obj: &JsonValue, key: &str) -> Result<f64, String> {
    let v = obj
        .get(key)
        .ok_or_else(|| format!("revision 缺 {key}（修订后取值必填）"))?;
    let n = match v {
        JsonValue::Integer(i) => *i as f64,
        JsonValue::U64(u) => *u as f64,
        JsonValue::String(s) => s.parse::<f64>().map_err(|_| {
            format!(
                "revision.{key}=\"{s}\" 不是合法数字 — 自诊断指引: \
                     TCB 域小数以字符串承载（如 \"0.5\"），须可解析为有限小数"
            )
        })?,
        _ => {
            return Err(format!(
                "revision.{key} 须为数字（整数或数字字符串）— 自诊断指引: \
                 修订取值不接受布尔/数组/对象/空值"
            ));
        }
    };
    if !n.is_finite() {
        return Err(format!("revision.{key} 须为有限数字（NaN/无穷拒收）"));
    }
    Ok(n)
}

/// 解析修订提案（出处元数据强制：CODATA 级修订不例外）。
fn parse_revision(revision: &JsonValue) -> Result<(String, f64), String> {
    let why = "修订提案强制出处元数据（可溯源性），缺出处/未验证的修订不接受";
    let constant_id = non_empty_str_field(revision, "constant_id", why)?;
    let new_value = numeric_field(revision, "new_value")?;
    // new_uncertainty 可选：存在则须为合法数字（不参与传播计算，仅入 input_digest）
    if revision.get("new_uncertainty").is_some() {
        numeric_field(revision, "new_uncertainty")?;
    }
    non_empty_str_field(revision, "source", why)?;
    match revision.get("verified") {
        Some(JsonValue::Bool(true)) => {}
        Some(_) => {
            return Err(format!(
                "revision.verified 须恰为 true — {why} — 自诊断指引: \
                 修订提案须先过验证再提交传播分析"
            ));
        }
        None => {
            return Err(format!(
                "revision 缺 verified — {why} — 自诊断指引: 补齐验证位后重试"
            ));
        }
    }
    Ok((constant_id, new_value))
}

/// 解析历史裁决清单（元素 {instruction_type, state} 均非空字符串；保序保留原始条目）。
fn parse_adjudications(args: &JsonValue) -> Result<Vec<(String, JsonValue)>, String> {
    let items = match args.get("adjudications") {
        Some(JsonValue::Array(items)) => items,
        Some(_) => {
            return Err("参数 adjudications 须为数组 — 自诊断指引: 元素形如 \
                 {\"instruction_type\": \"…\", \"state\": \"…\"}"
                .to_string())
        }
        None => {
            return Err("缺少必填参数 adjudications — 自诊断指引: 历史裁决清单, \
                 元素形如 {\"instruction_type\": \"…\", \"state\": \"…\"}"
                .to_string())
        }
    };
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let obj = match item {
            JsonValue::Object(_) => item,
            _ => {
                return Err(format!(
                    "adjudications[{i}] 须为对象 — 自诊断指引: 元素形如 \
                     {{\"instruction_type\": \"…\", \"state\": \"…\"}}"
                ))
            }
        };
        let instruction_type = match obj.get("instruction_type") {
            Some(JsonValue::String(s)) if !s.is_empty() => s.to_string(),
            _ => {
                return Err(format!(
                    "adjudications[{i}].instruction_type 须为非空字符串 — 自诊断指引: \
                     裁决条目须指明被裁决的指令类型"
                ))
            }
        };
        if !matches!(obj.get("state"), Some(JsonValue::String(s)) if !s.is_empty()) {
            return Err(format!(
                "adjudications[{i}].state 须为非空字符串 — 自诊断指引: \
                 裁决条目须指明裁决状态（调用方从审计链 lifecycle 取数）"
            ));
        }
        out.push((instruction_type, item.clone()));
    }
    Ok(out)
}

// ============================================================================
// 冲击传播计算（五步全确定性）
// ============================================================================

/// 冲击传播结果（确定性结构：全部集合有序）
struct ShockOutcome {
    /// 受影响指令（(instruction_type, depth, via)，按指令字典序）
    affected: Vec<(String, u64, &'static str)>,
    /// regime 失效指令（字典序）
    regime_invalidated: Vec<String>,
    /// 已裁命题反推命中条目（原始条目保序）
    adjudications_invalidated: Vec<JsonValue>,
    /// 失效指令量纲去重清单（字典序）
    domains_touched: Vec<[i64; 7]>,
    /// 受影响集最大深度（无受影响 = 0）
    propagation_depth: u64,
}

/// 五步传播推演（引用闭包 → regime 复查 → 耦合 BFS → 命题反推 → 深度汇总）。
fn compute_shock(
    graph: &DependencyGraph,
    constant_id: &str,
    new_value: f64,
    adjudications: &[(String, JsonValue)],
) -> ShockOutcome {
    // ① 引用闭包：引用边命中修订常数的全部指令 = 直接受影响（depth=1）
    let direct: BTreeSet<&str> = graph
        .reference_edges
        .iter()
        .filter(|e| e.constant == constant_id)
        .map(|e| e.instruction.as_str())
        .collect();

    // ② regime 复查：域界声明存在时查 new_value（闭区间外 → 失效；未声明 → 不失效）。
    //    判定仅依赖常数域界与 new_value，对全部直接受影响指令同判。
    let mut invalidated: BTreeSet<String> = BTreeSet::new();
    if let Some(c) = graph
        .constants
        .iter()
        .find(|c| c.constant_id == constant_id)
    {
        for inst in &direct {
            let outside = c.domain_lo.is_some_and(|lo| new_value < lo)
                || c.domain_hi.is_some_and(|hi| new_value > hi);
            if outside {
                invalidated.insert((*inst).to_string());
            }
        }
    }

    // 受影响集（引用闭包先行，BFS 波及按层数补入；键序 = 指令字典序）
    let mut affected: BTreeMap<String, (u64, &'static str)> = direct
        .iter()
        .map(|s| ((*s).to_string(), (1u64, VIA_REFERENCE)))
        .collect();

    // ③ 量纲耦合层间传播：自直接失效集 BFS 至不动点（直接失效集为空则不传播）
    let mut visited: BTreeSet<String> = invalidated.clone();
    let mut frontier: Vec<String> = invalidated.iter().cloned().collect();
    let mut layer: u64 = 0;
    while !frontier.is_empty() {
        layer += 1;
        let mut next: Vec<String> = Vec::new();
        for node in &frontier {
            for edge in &graph.coupling_edges {
                let neighbor = if edge.a == *node {
                    Some(&edge.b)
                } else if edge.b == *node {
                    Some(&edge.a)
                } else {
                    None
                };
                let Some(n) = neighbor else { continue };
                if visited.insert(n.clone()) {
                    // 被波及指令：入失效集且入受影响集（depth = BFS 层数 + 1）
                    invalidated.insert(n.clone());
                    let depth = layer + 1;
                    affected.entry(n.clone()).or_insert((depth, VIA_COUPLING));
                    next.push(n.clone());
                }
            }
        }
        next.sort();
        frontier = next;
    }

    // ④ 已裁命题反推：裁决清单中指令命中失效集的条目原样保留（保序）
    let adjudications_invalidated: Vec<JsonValue> = adjudications
        .iter()
        .filter(|(t, _)| invalidated.contains(t))
        .map(|(_, item)| item.clone())
        .collect();

    // ⑤ 深度汇总 + 失效指令量纲去重（字典序）
    let propagation_depth = affected.values().map(|(d, _)| *d).max().unwrap_or(0);
    let mut domains: BTreeSet<[i64; 7]> = BTreeSet::new();
    for inst in &invalidated {
        if let Some(node) = graph
            .instructions
            .iter()
            .find(|n| &n.instruction_type == inst)
        {
            domains.insert(node.dimensions);
        }
    }

    ShockOutcome {
        affected: affected
            .into_iter()
            .map(|(t, (d, via))| (t, d, via))
            .collect(),
        regime_invalidated: invalidated.into_iter().collect(),
        adjudications_invalidated,
        domains_touched: domains.into_iter().collect(),
        propagation_depth,
    }
}

// ============================================================================
// 服务实例
// ============================================================================

/// 常数修订冲击传播分析服务（纯函数，零内部状态）
pub struct GraphShockPropagate;

impl NativeService for GraphShockPropagate {
    fn execute(&self, args: &JsonValue) -> IoResult {
        // ── 入参解析（缺任一必选键 → fail-visible）──
        let rule_set = object_arg(args, "rule_set")?;
        let revision = object_arg(args, "revision")?;
        let (constant_id, new_value) = parse_revision(revision)?;
        let adjudications = parse_adjudications(args)?;

        // ── 依赖图物化（构建错误透传；serde 域交接）──
        let rule_set_serde = tcb_to_serde(rule_set);
        let graph = build_dependency_graph(&rule_set_serde).map_err(|e| e.to_string())?;

        // 修订对象必须命中已声明常数
        if !graph.constants.iter().any(|c| c.constant_id == constant_id) {
            return Err(format!(
                "修订常数 \"{constant_id}\" 未在 rule_set metadata.constants 声明 — \
                 自诊断指引: 修订对象必须是依赖图已声明的常数（先声明后修订）"
            ));
        }

        // ── input_digest：三输入规范化哈希（篡改即红锚）──
        let revision_serde = tcb_to_serde(revision);
        let adjudications_serde: Vec<serde_json::Value> = adjudications
            .iter()
            .map(|(_, item)| tcb_to_serde(item))
            .collect();
        let input_payload = serde_json::json!({
            "rule_set": rule_set_serde,
            "revision": revision_serde,
            "adjudications": adjudications_serde,
        });
        let input_hex = evorule_hash::try_json_digest(&input_payload)
            .map_err(|e| format!("输入序列化失败: {e}"))?;

        // ── 传播推演 + 出参（确定性键序：BTreeMap 自动排序）──
        let outcome = compute_shock(&graph, &constant_id, new_value, &adjudications);
        let affected_json: Vec<JsonValue> = outcome
            .affected
            .iter()
            .map(|(t, d, via)| {
                JsonValue::object_from_pairs(&[
                    ("instruction_type", JsonValue::string(t.as_str())),
                    (
                        "depth",
                        JsonValue::Integer(i64::try_from(*d).unwrap_or(i64::MAX)),
                    ),
                    ("via", JsonValue::string(*via)),
                ])
            })
            .collect();
        let domains_json: Vec<JsonValue> = outcome
            .domains_touched
            .iter()
            .map(|dims| JsonValue::Array(dims.iter().map(|p| JsonValue::Integer(*p)).collect()))
            .collect();
        let raw_new_value = revision
            .get("new_value")
            .cloned()
            .unwrap_or(JsonValue::Null);
        // 可见性面：声明集 − 引用集（零引用常数对冲击传播零半径，逐次点名）
        let referenced: BTreeSet<&str> = graph
            .reference_edges
            .iter()
            .map(|e| e.constant.as_str())
            .collect();
        let unreferenced: Vec<JsonValue> = graph
            .constants
            .iter()
            .map(|c| c.constant_id.as_str())
            .filter(|id| !referenced.contains(id))
            .map(JsonValue::string)
            .collect();
        Ok(JsonValue::object_from_pairs(&[
            ("affected_instructions", JsonValue::Array(affected_json)),
            (
                "adjudications_invalidated",
                JsonValue::Array(outcome.adjudications_invalidated),
            ),
            ("domains_touched", JsonValue::Array(domains_json)),
            ("graph_digest", JsonValue::string(graph.digest)),
            (
                "input_digest",
                JsonValue::string(evorule_hash::prefixed(&input_hex)),
            ),
            ("new_value", raw_new_value),
            (
                "propagation_depth",
                JsonValue::Integer(i64::try_from(outcome.propagation_depth).unwrap_or(i64::MAX)),
            ),
            (
                "regime_invalidated",
                JsonValue::Array(
                    outcome
                        .regime_invalidated
                        .iter()
                        .map(|s| JsonValue::string(s.as_str()))
                        .collect(),
                ),
            ),
            ("shocked_constant", JsonValue::string(constant_id)),
            ("unreferenced_constants", JsonValue::Array(unreferenced)),
        ]))
    }
}

/// 声明表构造子：常数修订冲击传播服务
pub fn mk_graph_shock_propagate() -> Arc<dyn NativeService> {
    Arc::new(GraphShockPropagate)
}

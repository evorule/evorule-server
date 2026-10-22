// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 规则读写集静态提取器（静态推理服务核心，设计文档 §二）。
//!
//! # 定位
//!
//! 把合并规则列表（core_eval 形态 JSON）静态提取为每条规则的规范化读写集
//! + I/O 出边 + 诚实标注位，供环检测（`cycle`）与冲突检测（`conflict`）消费。
//!
//! 纯函数、零 I/O、永不 panic（超限显式 `Err`），与 TCB 同纪律。
//!
//! # 语义对齐（全部经现行 TCB 源码核实，零 v4 模型照搬）
//!
//! - **路径规范化**：剥 `__exec__.` 前缀（可重复）；`payload.__io_results__.T`
//!   与 `__io_results__.T` 统一映射为 `io:T`（I/O 结果按类型隔离的专用空间）。
//! - **读侧**：域 `path` 字段统一走 `__exec__.` 自动补全语义；
//!   `eq`/`lt` 的 `value` 字段按 `domain.rs::resolve_value_reference_outcome`
//!   语义判定——`__` 开头字符串 = 路径引用；根段点分形态（instruction/payload/
//!   queue 首段）= 引用歧义，运行时显式报错，静态侧保守记读。
//! - **写侧**（`executor.rs::execute_set` attr 三形态）：
//!   1. 相对形态（如 `user.name`）→ 相对 `__exec__.payload`，写路径 = `payload.user.name`；
//!   2. 状态路径引用（如 `__exec__.instruction.params.attr`）→ **动态写目标**：
//!      记读 + 置 `write_dyn`，写集附 attr 表达式本身（图可见性用）；
//!   3. 显式全形式（`__exec__.payload.X`）→ 剥前缀后为静态写目标。
//! - **`instruction` 域**：零路径读，记入 `instr_types`（域重叠判定的唯一
//!   可证伪依据）。
//! - **`io_request`**：记 `IoEdge{io_type, service_name}`；除 `io_type` 外
//!   的参数按路径引用或字面量解析（`?` 后缀可选参数语义不影响读集）。
//! - **域本身为路径引用**（宪法 conditional/while_loop 实锤形态）→ 记读 +
//!   置 `domain_dyn`（域可达性静态不可判定）。
//!
//! # 分桶
//!
//! `source == "core_eval"` 的宪法规则默认进入豁免桶（`excluded = true`）——
//! 宪法是自引用形态（读/写 `instruction.params.attr`、域动态），静态图上
//! 必然假环；其动态性是设计特征不是缺陷。豁免显式标注，不静默；
//! `include_constitution = true` 可参数化包含。

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};

use evorule_tcb::JsonValue;

/// 指令树遍历深度上限（与 TCB `MAX_DOMAIN_DEPTH` 同量级防线；超限显式 Err）
pub const MAX_TREE_DEPTH: usize = 64;

/// 规则数上限（与 TCB `MAX_TRANSFORM_RULES` 同口径；超限显式 Err）
pub const MAX_RULES: usize = 64;

/// I/O 出边：一次 `io_request` 的类型与（静态可提取时的）服务名
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct IoEdge {
    /// `params.io_type`（如 "call_external"、"call_service"）
    pub io_type: String,
    /// `params.service_name` 静态字面量时可提取；路径引用 / 缺失 → `None`
    pub service_name: Option<String>,
}

/// 单条规则的静态读写集
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleRw {
    /// 规则身份：`"{source}#{index}"`（对齐 RulesetLayout 的 (source, index) 契约）
    pub id: String,
    /// 来源标签（"core_eval" / "server_eval" / "rules/<相对路径>"）
    pub source: String,
    /// 合并列表下标
    pub index: usize,
    /// 规范化读路径集（payload.* / instruction.* / queue[*] / io:T）
    pub reads: BTreeSet<String>,
    /// 规范化写路径集；动态写目标时附 attr 表达式本身（如图可见性）
    pub writes: BTreeSet<String>,
    /// 静态常量写：路径 → 字面量规范化串（矛盾写检测材料）
    pub const_writes: BTreeMap<String, String>,
    /// I/O 出边
    pub ios: BTreeSet<IoEdge>,
    /// 指令类型约束集（`instruction` 域声明；域重叠判定的可证伪依据）
    pub instr_types: BTreeSet<String>,
    /// not 包裹的域子树规范串（互补域对检测材料）
    pub not_domains: BTreeSet<String>,
    /// 非 not 的域对象规范串（互补域对检测材料）
    pub pos_domains: BTreeSet<String>,
    /// 域本身是路径引用（域可达性静态不可判定）
    pub domain_dyn: bool,
    /// 存在动态写目标（写位置运行时才定形）
    pub write_dyn: bool,
    /// 域无任何路径读且无指令约束（如 `all([])` 兜底）→ 与一切域可能重叠
    pub domain_universal: bool,
    /// 豁免桶（宪法段默认 true；显式包含时 false）
    pub excluded: bool,
}

/// 提取全部规则的读写集。
///
/// # Errors
///
/// - `rules` 与 `sources` 长度不一致
/// - 规则数超过 [`MAX_RULES`]
/// - 任一指令树深度超过 [`MAX_TREE_DEPTH`]
pub fn extract(
    rules: &[JsonValue],
    sources: &[String],
    include_constitution: bool,
) -> Result<Vec<RuleRw>, String> {
    if rules.len() != sources.len() {
        return Err(format!(
            "rules({}) 与 sources({}) 长度不一致 — 自诊断指引: 来源标签须与规则列表等长且按合并序对齐",
            rules.len(),
            sources.len()
        ));
    }
    if rules.len() > MAX_RULES {
        return Err(format!(
            "规则数 {} 超过上限 {MAX_RULES} — 自诊断指引: 与 TCB MAX_TRANSFORM_RULES 同口径,超限规则集无法通过 execute_transition",
            rules.len()
        ));
    }
    let mut out = Vec::with_capacity(rules.len());
    for (index, rule) in rules.iter().enumerate() {
        let source = &sources[index];
        let excluded = !include_constitution && source == "core_eval";
        let mut acc = Acc::default();
        walk_instruction(rule, 0, &mut acc)?;
        out.push(RuleRw {
            id: format!("{source}#{index}"),
            source: source.clone(),
            index,
            reads: acc.reads,
            writes: acc.writes,
            const_writes: acc.const_writes,
            ios: acc.ios,
            instr_types: acc.instr_types,
            not_domains: acc.not_domains,
            pos_domains: acc.pos_domains,
            domain_dyn: acc.domain_dyn,
            write_dyn: acc.write_dyn,
            domain_universal: !acc.saw_constraining_domain,
            excluded,
        });
    }
    Ok(out)
}

// ============================================================================
// 内部累加器与遍历
// ============================================================================

#[derive(Default)]
struct Acc {
    reads: BTreeSet<String>,
    writes: BTreeSet<String>,
    const_writes: BTreeMap<String, String>,
    ios: BTreeSet<IoEdge>,
    instr_types: BTreeSet<String>,
    not_domains: BTreeSet<String>,
    pos_domains: BTreeSet<String>,
    domain_dyn: bool,
    write_dyn: bool,
    saw_constraining_domain: bool,
}

/// 遍历单条指令树节点
fn walk_instruction(node: &JsonValue, depth: usize, acc: &mut Acc) -> Result<(), String> {
    if depth > MAX_TREE_DEPTH {
        return Err(format!(
            "指令树深度超过上限 {MAX_TREE_DEPTH} — 自诊断指引: 与 TCB 域/分支深度防线同口径,拒绝提取异常嵌套规则"
        ));
    }
    let ty = node.get("type").and_then(JsonValue::as_str).unwrap_or("");
    let params = node.get("params");
    match ty {
        "branch" => {
            if let Some(p) = params {
                if let Some(d) = p.get("domain") {
                    walk_domain(d, depth + 1, acc)?;
                }
                for key in ["on_true", "on_false"] {
                    if let Some(list) = p.get(key) {
                        walk_instruction_list(list, depth + 1, acc)?;
                    }
                }
            }
        }
        "enforce" => {
            // 约束前置门：域可静态或路径引用；enforce 无写副作用
            if let Some(p) = params {
                if let Some(d) = p.get("domain") {
                    walk_domain(d, depth + 1, acc)?;
                }
            }
        }
        "set" => {
            if let Some(p) = params {
                walk_set(p, acc);
            }
        }
        "push" => {
            if let Some(p) = params {
                if let Some(instructions) = p.get("instructions") {
                    match instructions {
                        JsonValue::Array(items) => {
                            for item in items {
                                walk_instruction(item, depth + 1, acc)?;
                            }
                        }
                        JsonValue::String(s)
                            // 路径引用形态（宪法 sequence/conditional/while_loop 实锤）：
                            // 子指令树运行时才定形 → 记读
                            if is_path_ref(s) => {
                                acc.reads.insert(normalize_path(s));
                            }
                        _ => {}
                    }
                }
            }
        }
        "io_request" => {
            if let Some(p) = params {
                let io_type = p
                    .get("io_type")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("unknown");
                // service_name：静态字面量可提取；路径引用 → 记读 + None
                let mut service_name = None;
                if let Some(JsonValue::String(s)) = p.get("service_name") {
                    if is_path_ref(s) {
                        acc.reads.insert(normalize_path(s));
                    } else {
                        service_name = Some(s.to_string());
                    }
                }
                // 除 io_type 外的参数：`__` 开头字符串按路径引用记读
                if let Some(obj) = p.as_object() {
                    for (key, value) in obj.iter() {
                        if key == "io_type" || key == "service_name" {
                            continue;
                        }
                        if let JsonValue::String(s) = value {
                            if is_path_ref(s) {
                                acc.reads.insert(normalize_path(s));
                            }
                        }
                    }
                }
                acc.ios.insert(IoEdge {
                    io_type: io_type.to_string(),
                    service_name,
                });
            }
        }
        // 未知类型：TCB 运行时 Ignored；静态侧不展开、不误报
        _ => {}
    }
    Ok(())
}

/// 遍历指令列表（on_true / on_false / push 字面量数组）
fn walk_instruction_list(list: &JsonValue, depth: usize, acc: &mut Acc) -> Result<(), String> {
    if let JsonValue::Array(items) = list {
        for item in items {
            walk_instruction(item, depth, acc)?;
        }
    }
    Ok(())
}

/// 遍历域节点（读侧）
fn walk_domain(domain: &JsonValue, depth: usize, acc: &mut Acc) -> Result<(), String> {
    if depth > MAX_TREE_DEPTH {
        return Err(format!(
            "域嵌套深度超过上限 {MAX_TREE_DEPTH} — 自诊断指引: 与 TCB MAX_DOMAIN_DEPTH 同口径"
        ));
    }
    match domain {
        // 域本身是路径引用（宪法 conditional/while_loop 实锤形态）
        JsonValue::String(s) => {
            acc.reads.insert(normalize_path(s));
            acc.domain_dyn = true;
        }
        JsonValue::Object(_) => {
            let ty = domain.get("type").and_then(JsonValue::as_str).unwrap_or("");
            match ty {
                "eq" | "lt" => {
                    if let Some(path) = domain.get("path").and_then(JsonValue::as_str) {
                        acc.reads.insert(normalize_path(path));
                        acc.saw_constraining_domain = true;
                    }
                    if let Some(JsonValue::String(s)) = domain.get("value") {
                        if is_path_ref(s) {
                            acc.reads.insert(normalize_path(s));
                        }
                    }
                    acc.pos_domains.insert(domain.to_string());
                }
                "exists" => {
                    if let Some(path) = domain.get("path").and_then(JsonValue::as_str) {
                        acc.reads.insert(normalize_path(path));
                        acc.saw_constraining_domain = true;
                    }
                    acc.pos_domains.insert(domain.to_string());
                }
                "has_fields" => {
                    if let Some(path) = domain.get("path").and_then(JsonValue::as_str) {
                        acc.reads.insert(normalize_path(path));
                        acc.saw_constraining_domain = true;
                    }
                    // fields 是字段名清单不是路径，不入读集
                    acc.pos_domains.insert(domain.to_string());
                }
                "instruction" => {
                    if let Some(t) = domain.get("instruction_type").and_then(JsonValue::as_str) {
                        acc.instr_types.insert(t.to_string());
                        acc.saw_constraining_domain = true;
                    }
                    acc.pos_domains.insert(domain.to_string());
                }
                "all" | "not" => {
                    if let Some(inner) = domain.get("inner") {
                        let canon = inner.to_string();
                        if ty == "not" {
                            acc.not_domains.insert(canon);
                        }
                        // all 递归展开；not 亦递归（其内层路径仍是读）
                        walk_domain(inner, depth + 1, acc)?;
                    }
                }
                // 未知域类型：TCB 运行时显式报错；静态侧不展开不误报
                _ => {}
            }
        }
        _ => {}
    }
    Ok(())
}

/// 处理 set 节点（写侧 + 值读侧）
fn walk_set(params: &JsonValue, acc: &mut Acc) {
    // ── attr 三形态（对齐 executor.rs::execute_set 语义）──
    let attr_raw = params.get("attr").and_then(JsonValue::as_str);
    let mut static_write: Option<String> = None;
    if let Some(raw) = attr_raw {
        if let Some(rest) = raw.strip_prefix("__exec__.payload.") {
            // 形态 3：显式 payload 全形式 → 静态写目标
            static_write = Some(payload_rel_to_canonical(rest));
        } else if raw.starts_with("__") {
            // 形态 2：状态路径引用 → 动态写目标
            acc.reads.insert(normalize_path(raw));
            acc.write_dyn = true;
            acc.writes.insert(normalize_path(raw));
        } else {
            // 形态 1：payload 相对路径 → 静态写目标
            static_write = Some(payload_rel_to_canonical(raw));
        }
    }
    // ── value：路径引用记读；字面量记常量写 ──
    let mut const_repr: Option<String> = None;
    if let Some(value) = params.get("value") {
        match value {
            JsonValue::String(s) => {
                if is_path_ref(s) {
                    acc.reads.insert(normalize_path(s));
                } else {
                    const_repr = Some(value.to_string());
                }
            }
            // 非字符串一律字面量（TCB 值解析仅对字符串做引用判定）
            _ => const_repr = Some(value.to_string()),
        }
    }
    if let (Some(w), Some(c)) = (static_write.clone(), const_repr.clone()) {
        acc.const_writes.insert(w, c);
    }
    if let Some(w) = static_write {
        acc.writes.insert(w);
    }
}

// ============================================================================
// 路径规范化与引用判定（对齐 TCB path/domain/executor 语义）
// ============================================================================

/// 剥 `__exec__.` 前缀（可重复）后归一；`__io_results__` 空间映射为 `io:`
pub fn normalize_path(raw: &str) -> String {
    let mut s = raw;
    while let Some(rest) = s.strip_prefix("__exec__.") {
        s = rest;
    }
    if let Some(rest) = s.strip_prefix("payload.__io_results__.") {
        return format!("io:{rest}");
    }
    if let Some(rest) = s.strip_prefix("__io_results__.") {
        return format!("io:{rest}");
    }
    s.to_string()
}

/// payload 相对路径 → 规范化状态路径（写侧形态 1/3 共用）
pub fn payload_rel_to_canonical(rel: &str) -> String {
    if let Some(rest) = rel.strip_prefix("__io_results__.") {
        return format!("io:{rest}");
    }
    format!("payload.{rel}")
}

/// 目标路径规范化（查询侧入口，与写侧形态 1/3 对齐同一套归一）。
///
/// 语义：
/// - 裸相对路径（`x`）→ `payload.x`（写侧相对形态同解）
/// - `__exec__.payload.x` → `payload.x`（剥前缀）
/// - 已带根段（`payload.*` / `instruction.*` / `queue[*]` / `io:*`）→ 原样保留
/// - 空串 → 空串（调用方须自行 fail-fast 拒绝）
///
/// 存在理由：生产者索引的键是**写侧规范化后**的路径（`payload.y`），
/// 而查询侧目标路径此前原样入索引——`y` 与 `payload.y` 会得到不同结论
/// （前者查无生产者 → 落「外部必需输入」分支回 achievable=true，
/// 后者命中生产者 → 走生产链搜索）。同一目标两种写法两种结论 = 口径不自洽。
/// 归一后等价写法必得等价结论。
pub fn canonical_target_path(raw: &str) -> String {
    let s = normalize_path(raw);
    let t = s.trim();
    if t.is_empty() {
        return String::new();
    }
    // 已带根段 → 原样保留（裸相对路径不属此列）
    if t.starts_with("payload.")
        || t.starts_with("payload[")
        || t.starts_with("instruction.")
        || t.starts_with("instruction[")
        || t.starts_with("queue.")
        || t.starts_with("queue[")
        || t.starts_with("io:")
    {
        return t.to_string();
    }
    // 其余（裸相对路径）→ payload 相对路径（与写侧形态 1 同解）
    format!("payload.{t}")
}

/// 字符串值是否为路径引用（对齐 resolve_value_reference_outcome 语义）：
/// `__` 开头 = 显式引用；根段点分形态（instruction/payload/queue 首段）=
/// 引用歧义（运行时报错），静态侧保守按引用记读
fn is_path_ref(s: &str) -> bool {
    if s.starts_with("__") {
        return true;
    }
    match s.split_once('.') {
        Some((first, _)) => {
            // 首段可能带数组下标后缀（如 queue[0]）——取 '[' 前的根段名
            let mut root = first;
            if let Some(idx) = root.find('[') {
                root = &root[..idx];
            }
            matches!(root, "instruction" | "payload" | "queue")
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn obj(pairs: &[(&str, JsonValue)]) -> JsonValue {
        JsonValue::object_from_pairs(pairs)
    }

    fn extract_one(rule: &JsonValue) -> RuleRw {
        let rws = extract(
            std::slice::from_ref(rule),
            &["rules/test.json".to_string()],
            false,
        )
        .unwrap();
        rws.into_iter().next().expect("恰一条")
    }

    #[test]
    fn normalize_strips_exec_prefix_and_maps_io() {
        assert_eq!(normalize_path("__exec__.payload.x"), "payload.x");
        assert_eq!(normalize_path("__exec__.__exec__.payload.x"), "payload.x");
        assert_eq!(
            normalize_path("__exec__.payload.__io_results__.call_external"),
            "io:call_external"
        );
        assert_eq!(normalize_path("__io_results__.query_db"), "io:query_db");
        assert_eq!(normalize_path("payload.x"), "payload.x");
        assert_eq!(normalize_path("queue[0].type"), "queue[0].type");
    }

    #[test]
    fn set_static_write_and_literal_value() {
        // 写 payload.x 常量 42；域读 payload.x
        let rule = obj(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                obj(&[
                    (
                        "domain",
                        obj(&[
                            ("type", JsonValue::string("eq")),
                            ("path", JsonValue::string("__exec__.payload.flag")),
                            ("value", JsonValue::string("on")),
                        ]),
                    ),
                    (
                        "on_true",
                        JsonValue::Array(vec![obj(&[
                            ("type", JsonValue::string("set")),
                            (
                                "params",
                                obj(&[
                                    ("attr", JsonValue::string("x")),
                                    ("operation", JsonValue::string("set")),
                                    ("value", JsonValue::Integer(42)),
                                ]),
                            ),
                        ])]),
                    ),
                ]),
            ),
        ]);
        let rw = extract_one(&rule);
        assert!(rw.reads.contains("payload.flag"));
        assert!(rw.writes.contains("payload.x"));
        assert_eq!(rw.const_writes.get("payload.x").expect("常量写"), "42");
        assert!(!rw.write_dyn);
        assert!(rw.instr_types.is_empty());
    }

    #[test]
    fn set_dynamic_attr_is_write_dyn_and_read() {
        // 宪法 increment 规则形态：attr = __exec__.instruction.params.attr
        let rule = obj(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                obj(&[
                    (
                        "attr",
                        JsonValue::string("__exec__.instruction.params.attr"),
                    ),
                    ("operation", JsonValue::string("add")),
                    (
                        "value",
                        JsonValue::string("__exec__.instruction.params.delta"),
                    ),
                ]),
            ),
        ]);
        let rw = extract_one(&rule);
        assert!(rw.write_dyn);
        assert!(rw.reads.contains("instruction.params.attr"));
        assert!(rw.reads.contains("instruction.params.delta"));
        assert!(rw.writes.contains("instruction.params.attr"));
        assert!(rw.const_writes.is_empty());
    }

    #[test]
    fn instruction_domain_records_instr_type() {
        let rule = obj(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                obj(&[
                    (
                        "domain",
                        obj(&[
                            ("type", JsonValue::string("instruction")),
                            ("instruction_type", JsonValue::string("increment")),
                        ]),
                    ),
                    ("on_true", JsonValue::Array(vec![])),
                ]),
            ),
        ]);
        let rw = extract_one(&rule);
        assert!(rw.instr_types.contains("increment"));
        assert!(rw.reads.is_empty());
        assert!(!rw.domain_universal);
    }

    #[test]
    fn all_empty_domain_is_universal() {
        // 宪法兜底规则形态：all([]) 无约束 → 与一切重叠
        let rule = obj(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                obj(&[
                    (
                        "domain",
                        obj(&[
                            ("type", JsonValue::string("all")),
                            ("inner", JsonValue::Array(vec![])),
                        ]),
                    ),
                    ("on_true", JsonValue::Array(vec![])),
                ]),
            ),
        ]);
        let rw = extract_one(&rule);
        assert!(rw.domain_universal);
    }

    #[test]
    fn dynamic_domain_is_flagged() {
        // 宪法 conditional 内层 branch：domain 本身是路径引用
        let rule = obj(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                obj(&[
                    (
                        "domain",
                        JsonValue::string("__exec__.instruction.params.domain"),
                    ),
                    ("on_true", JsonValue::Array(vec![])),
                ]),
            ),
        ]);
        let rw = extract_one(&rule);
        assert!(rw.domain_dyn);
        assert!(rw.reads.contains("instruction.params.domain"));
    }

    #[test]
    fn io_request_extracts_edge_and_static_service_name() {
        let rule = obj(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                obj(&[
                    (
                        "domain",
                        obj(&[
                            ("type", JsonValue::string("instruction")),
                            ("instruction_type", JsonValue::string("call_service")),
                        ]),
                    ),
                    (
                        "on_false",
                        JsonValue::Array(vec![obj(&[
                            ("type", JsonValue::string("io_request")),
                            (
                                "params",
                                obj(&[
                                    ("io_type", JsonValue::string("call_service")),
                                    ("service_name", JsonValue::string("indicator_sma")),
                                    ("series", JsonValue::string("__exec__.payload.series")),
                                ]),
                            ),
                        ])]),
                    ),
                ]),
            ),
        ]);
        let rw = extract_one(&rule);
        let edge = rw.ios.iter().next().expect("恰一条 IoEdge");
        assert_eq!(edge.io_type, "call_service");
        assert_eq!(edge.service_name.as_deref(), Some("indicator_sma"));
        assert!(rw.reads.contains("payload.series"));
    }

    #[test]
    fn push_literal_recurses_and_push_ref_reads() {
        // push 字面量数组 → 递归内部 set；push 路径引用 → 记读
        let inner_set = obj(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                obj(&[
                    ("attr", JsonValue::string("y")),
                    ("value", JsonValue::Integer(1)),
                ]),
            ),
        ]);
        let push_node = obj(&[
            ("type", JsonValue::string("push")),
            (
                "params",
                obj(&[("instructions", JsonValue::Array(vec![inner_set]))]),
            ),
        ]);
        let rule = obj(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                obj(&[
                    (
                        "domain",
                        obj(&[
                            ("type", JsonValue::string("all")),
                            ("inner", JsonValue::Array(vec![])),
                        ]),
                    ),
                    ("on_true", JsonValue::Array(vec![push_node])),
                ]),
            ),
        ]);
        let rw = extract_one(&rule);
        assert!(rw.writes.contains("payload.y"));
        // while_loop 形态：instructions 为路径引用
        let rule2 = obj(&[
            ("type", JsonValue::string("push")),
            (
                "params",
                obj(&[(
                    "instructions",
                    JsonValue::string("__exec__.instruction.params.body"),
                )]),
            ),
        ]);
        let rw2 = extract_one(&rule2);
        assert!(rw2.reads.contains("instruction.params.body"));
    }

    #[test]
    fn constitution_bucket_excluded_by_default() {
        let rule = obj(&[("type", JsonValue::string("set"))]);
        let rws = extract(
            std::slice::from_ref(&rule),
            &["core_eval".to_string()],
            false,
        )
        .unwrap();
        assert!(rws[0].excluded);
        let rws2 = extract(
            std::slice::from_ref(&rule),
            &["core_eval".to_string()],
            true,
        )
        .unwrap();
        assert!(!rws2[0].excluded);
    }

    #[test]
    fn length_mismatch_and_overlimit_fail_fast() {
        let rule = obj(&[("type", JsonValue::string("set"))]);
        let err = extract(
            std::slice::from_ref(&rule),
            &["a".into(), "b".into()],
            false,
        )
        .unwrap_err();
        assert!(err.contains("长度不一致"));
        let rules: Vec<JsonValue> = (0..=MAX_RULES).map(|_| rule.clone()).collect();
        let sources: Vec<String> = (0..=MAX_RULES).map(|_| "rules/x.json".into()).collect();
        let err2 = extract(&rules, &sources, false).unwrap_err();
        assert!(err2.contains("超过上限"));
    }

    #[test]
    fn path_ref_classification_matches_tcb_semantics() {
        assert!(is_path_ref("__exec__.payload.x"));
        assert!(is_path_ref("payload.x"));
        assert!(is_path_ref("instruction.params.attr"));
        assert!(is_path_ref("queue[0].type"));
        assert!(!is_path_ref("on"));
        assert!(!is_path_ref("42"));
        assert!(!is_path_ref("plain"));
    }
}

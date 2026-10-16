// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 域类型评估器 - 8 种基本域类型
//!
//! # 支持的域类型
//! - `eq`：相等比较
//! - `lt`：小于比较
//! - `in`：集合成员判定（vendored 增补，design-19）
//! - `exists`：路径存在性检查
//! - `instruction`：指令类型匹配
//! - `all`：所有子域为真（空列表 = 真）
//! - `not`：子域取反
//! - `has_fields`：对象字段存在性与非空检查
//!
//! # 派生域类型（由 `core_eval.json` 组合实现）
//! - `gt` = `all([not(lt), not(eq)])`
//! - `or` = `not(all([not(a), not(b)]))`
//! - `le` = `not(gt)`
//! - `ge` = `not(lt)`
//! - `ne` = `not(eq)`

use crate::error::TcbError;
use crate::executor::json_type_name;
use crate::path::resolve_exec_path;
use crate::value::JsonValue;
use alloc::format;
use alloc::string::ToString;

/// 域评估最大递归深度
///
/// 终止性保证：嵌套 `all`/`not` 组合的递归深度上限。
/// 与 `executor::MAX_BRANCH_DEPTH`、`transition::MAX_TRANSFORM_RULES`
/// 共同构成单次状态转换的终止性防线。
///
/// # Kani 验证模型（CR-20260913-002）
///
/// Kani 构建下取 4：`evaluate_domain_outcome_inner` 的 `all`/`not` 分支递归调用
/// 自身，CBMC 无条件编码整棵递归调用树（每层扇出 2），64 层 = 2^64 节点
/// 不可收敛（2026-09-13 六轮二分探针定位：同逻辑去递归版 35s PASS，
/// 真递归版 150s 超时，与运行时输入无关）。4 层 = 2^5 个评估实例，深度
/// 保护分支（`depth > MAX_DOMAIN_DEPTH` 返回 `Err(NestingTooDeep)`）让
/// 递归树有界终止，无需 unwind 截断。属性语义：kani 构建验证「深度限制
/// 为 4 的域评估器」never-panic 的完备性；生产构建保持 64 零改动。
#[cfg(not(kani))]
pub const MAX_DOMAIN_DEPTH: usize = 64;
/// Kani 验证模型值：见上方主文档（CR-20260913-002，递归树有界化）。
#[cfg(kani)]
pub const MAX_DOMAIN_DEPTH: usize = 1;

/// 解析 domain 中的 path 字段，支持自动补全 `__exec__.` 前缀
///
/// 路径解析规则（统一走 `path::resolve_exec_path`）：
/// 1. `__exec__.` 开头: strip 后直接解析
/// 2. 其他: 自动补全 `__exec__.` 前缀后解析
///
/// 这样用户编写 conditional/while_loop 的 domain 时，可以写 `payload.x`、
/// `instruction.type`、`queue[0].type` 等相对路径，
/// 而不需要知道 `__exec__` 内部上下文结构。
fn resolve_domain_path<'a>(exec_state: &'a JsonValue, path: &str) -> Option<&'a JsonValue> {
    resolve_exec_path(exec_state, path)
}

// ===== 三态域判定本体（专项-20261001 方案 2' v4）=====

/// 域判定三态结果
///
/// 「真实比对为假」「路径不存在」「类型不可比」「value 引用歧义」四种
/// 情形在二态输出上不可区分（信息丢失）——三态结果还原被压平的输出，
/// Missing 的处理由规则文本的 `on_missing` 显式声明决定（`error` = 拒绝
/// 执行 / `unsat` = 走 on_false + 归因），静默通道从根上铲除。
#[derive(Debug, Clone, PartialEq)]
pub enum DomainOutcome {
    /// 路径存在且比较成立
    Sat,
    /// 路径存在且比较不成立（真实为假）
    Unsat,
    /// 状态侧缺失/不可比/引用歧义（处理策略见 `on_missing` 声明）
    Missing(MissingReason),
}

/// Missing 的归因分类
#[derive(Debug, Clone, PartialEq)]
pub enum MissingReason {
    /// 比较路径（或 `__` 引用路径）在执行状态中不存在
    PathNotFound,
    /// 两侧值均存在但类型不可比（`lt` 非 i64 等）
    Incomparable,
    /// value 为根段点分形态字符串（像路径引用但缺 `__` 前缀，写作错误）
    ValueLiteralAmbiguous,
}

impl MissingReason {
    /// 归因标签（审计落账/错误 detail 用，版本化锁定）
    pub fn label(&self) -> &'static str {
        match self {
            MissingReason::PathNotFound => "path_not_found",
            MissingReason::Incomparable => "incomparable",
            MissingReason::ValueLiteralAmbiguous => "value_literal_ambiguous",
        }
    }
}

impl DomainOutcome {
    /// 二态投影：Missing → false（兼容包装投影，与改前行为一致）
    pub fn to_bool(&self) -> bool {
        matches!(self, DomainOutcome::Sat)
    }

    /// 三态标签（审计落账用，版本化锁定）
    pub fn label(&self) -> &'static str {
        match self {
            DomainOutcome::Sat => "sat",
            DomainOutcome::Unsat => "unsat",
            DomainOutcome::Missing(_) => "missing",
        }
    }

    /// Missing 归因分类标签（非 Missing 返回 None）
    pub fn missing_reason(&self) -> Option<&'static str> {
        match self {
            DomainOutcome::Missing(reason) => Some(reason.label()),
            _ => None,
        }
    }

    fn from_bool(b: bool) -> Self {
        if b {
            DomainOutcome::Sat
        } else {
            DomainOutcome::Unsat
        }
    }
}

/// 域判定归因记录（归因透传，专项-20261001 方案 2' v4 T3）
///
/// 由 branch/enforce 消费点在求值成功后产出，随 `RuleHit` 交付 FactsLog
/// 落账与消费面审计（含 on_missing 声明值）。
///
/// **R1 归因不回灌执行**：本记录仅作审计观测位，branch 走向 / enforce 命中
/// 均在求值时由投影 bool + 规则静态声明决定，本记录永不参与执行决策。
#[derive(Debug, Clone, PartialEq)]
pub struct DomainAttribution {
    /// 三态判定结果
    pub outcome: DomainOutcome,
    /// 规则文本的 `on_missing` 声明值（`"error"` / `"unsat"`）；
    /// `None` = 未声明（存量兼容缺省 unsat——装载面新规则已强制显式声明）
    pub on_missing: Option<&'static str>,
}

/// ValueLiteralAmbiguous 判定规则版本（版本化锁定，防跨版本归因漂移）
///
/// 判定口径：value 字符串匹配「TCB exec 根段开头的点分形态」
/// `^(instruction|payload|queue)(\.[A-Za-z0-9_]+)+$` → 写作错误
/// （像路径引用但缺 `__` 前缀）。根段名单与 `resolve_exec_path`
/// 相对路径自动补全的根命名空间一致。
/// 存量实证（专项盘点）：历史实测旧形态 `instruction.params.milestone_target`
/// 精确命中；`meta_workflow.phase` 等合法符号常量字面量零误伤。
pub const VALUE_LITERAL_AMBIGUOUS_RULE: &str = "root-segment-dot-path.v1";

/// value 字符串是否为「TCB exec 根段开头的点分形态」（写作错误判定谓词）
///
/// [`VALUE_LITERAL_AMBIGUOUS_RULE`]（root-segment-dot-path.v1）口径的唯一权威
/// 实现：装载期拒收（server）与运行时归因（本仓）共用本谓词，零镜像复算。
pub fn is_root_segment_dot_path(s: &str) -> bool {
    let mut parts = s.split('.');
    match parts.next() {
        Some("instruction") | Some("payload") | Some("queue") => {}
        _ => return false,
    }
    let mut seg_count = 0usize;
    for seg in parts {
        seg_count += 1;
        if seg.is_empty() || !seg.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return false;
        }
    }
    seg_count >= 1
}

/// `on_missing` 声明（eq/lt 域可选字段；缺省 = 兼容缺省 unsat）
#[derive(Debug, Clone, PartialEq)]
enum OnMissingPolicy {
    /// Missing → Err 拒绝执行（显式报错，分支不执行）
    Error,
    /// Missing → 走 on_false + 归因（显式声明下的 false，非静默）
    Unsat,
}

impl OnMissingPolicy {
    /// 声明标签（审计落账用，与规则文本字面量一致）
    fn label(&self) -> &'static str {
        match self {
            OnMissingPolicy::Error => "error",
            OnMissingPolicy::Unsat => "unsat",
        }
    }
}

/// 解析 eq/lt 域的 `on_missing` 声明；缺省 None（运行时兼容缺省 unsat，
/// 仅防旧数据直灌——装载面对新装载规则强制显式声明）
fn parse_on_missing(domain: &JsonValue) -> Result<Option<OnMissingPolicy>, TcbError> {
    match domain.get("on_missing") {
        None => Ok(None),
        Some(v) => match v.as_str() {
            Some("error") => Ok(Some(OnMissingPolicy::Error)),
            Some("unsat") => Ok(Some(OnMissingPolicy::Unsat)),
            _ => Err(TcbError::InvalidType {
                expected: "\"error\" | \"unsat\"",
                actual: json_type_name(v),
                context: "on_missing".to_string(),
            }),
        },
    }
}

/// Missing 按声明分派：`error` → `Err(MissingRejected)`（拒绝执行，走
/// 结构侧错误通道全链留痕）；`unsat`/缺省 → `Ok(Missing)`（消费点投影
/// false + 归因落账）
fn dispatch_missing(
    policy: Option<OnMissingPolicy>,
    reason: MissingReason,
    context: &str,
) -> Result<DomainOutcome, TcbError> {
    match policy {
        Some(OnMissingPolicy::Error) => Err(TcbError::MissingRejected {
            detail: format!("{}: {}", reason.label(), context),
        }),
        _ => Ok(DomainOutcome::Missing(reason)),
    }
}

fn value_context(value: &JsonValue) -> &str {
    value.as_str().unwrap_or("?")
}

/// 解析 `eq`/`lt` 的 `value` 字段（三态版）：`__` 开头字符串视为路径引用
/// （解析失败 → `PathNotFound`）；根段点分形态 → `ValueLiteralAmbiguous`；
/// 其余为字面值。
fn resolve_value_reference_outcome(
    value: &JsonValue,
    exec_state: &JsonValue,
) -> Result<JsonValue, MissingReason> {
    match value {
        JsonValue::String(s) => {
            if s.starts_with("__") {
                resolve_domain_path(exec_state, s)
                    .cloned()
                    .ok_or(MissingReason::PathNotFound)
            } else if is_root_segment_dot_path(s) {
                Err(MissingReason::ValueLiteralAmbiguous)
            } else {
                Ok(value.clone())
            }
        }
        _ => Ok(value.clone()),
    }
}

/// 评估域条件，返回布尔值
///
/// # 支持的域类型
/// - `eq`：相等比较
/// - `lt`：小于比较（仅 i64）
/// - `in`：集合成员判定（vendored 增补，design-19）
/// - `exists`：路径存在性检查（null 视为已清除 = 不存在）
/// - `instruction`：指令类型匹配
/// - `all`：所有子域为真（空列表 = 真）
/// - `not`：子域取反
/// - `has_fields`：对象字段存在性与非空检查
///
/// # 错误语义（结构 vs 状态，统一决策表）
///
/// | 情形 | 返回 | 例子 |
/// |------|------|------|
/// | 域对象缺必需字段 / 字段类型错误 | `Err(MissingField/InvalidType)` | `eq` 缺 `value`；`on_missing` 非法值 |
/// | 未知域类型 | `Err(UnknownDomainType)` | `type: "e"`（拼错的 eq） |
/// | `has_fields.fields` 为空数组 | `Err(InvalidType)` | 无意义结构 |
/// | 嵌套深度超 `MAX_DOMAIN_DEPTH` | `Err(NestingTooDeep)` | 65 层 `not` |
/// | 路径在状态中不存在 | `Ok(false)`（三态投影） | `eq` 的 path 未就位 |
/// | 值不可比较（非整数等） | `Ok(false)`（三态投影） | `lt` 对字符串 |
/// | value 根段点分形态（引用歧义） | `Ok(false)`（三态投影） | `value: "instruction.params.x"` |
/// | `all` 的 `inner` 为空数组 | `Ok(true)` | 真空真（逻辑学标准约定） |
///
/// 该决策表确立单一原则：**规则结构错误显式报错（fail-fast），
/// 业务状态缺失静默求值（fail-closed）**。特别地，未知域类型不再
/// 静默求值为 false——否则经 `not` 包裹后反转为 true，fail-closed
/// 退化为 fail-open。
///
/// **三态归因（专项-20261001）**：本函数是 [`evaluate_domain_outcome`]
/// 的二态兼容包装（Missing → false），状态侧三情形（路径缺失/不可比/
/// 引用歧义）在 outcome 上可归因，行为与改前逐情形一致；对带
/// `on_missing: "error"` 声明的输入，Missing → `Err(MissingRejected)`
/// 拒绝执行。
///
/// # 路径约定
/// - `path` 字段支持相对路径（自动补全 `__exec__.` 前缀）
/// - `eq`/`lt` 的 `value` 字段若为 `__` 开头字符串则视为路径引用
///   （与执行器 `__` 保留命名空间约定一致）
///
/// # 示例
///
/// ```
/// extern crate alloc;
/// use evorule_tcb::JsonValue;
/// use evorule_tcb::domain::evaluate_domain;
/// use alloc::collections::BTreeMap;
///
/// // 构造 exec_state: { __exec__: { payload: { x: 10 } } }
/// let mut payload = BTreeMap::new();
/// payload.insert("x".to_string(), JsonValue::Integer(10));
/// let mut exec_inner = BTreeMap::new();
/// exec_inner.insert("payload".to_string(), JsonValue::object(payload));
/// let mut root = BTreeMap::new();
/// root.insert("__exec__".to_string(), JsonValue::object(exec_inner));
/// let state = JsonValue::object(root);
///
/// // eq: __exec__.payload.x == 10
/// let eq = JsonValue::object_from_pairs(&[
///     ("type", JsonValue::string("eq")),
///     ("path", JsonValue::string("__exec__.payload.x")),
///     ("value", JsonValue::Integer(10)),
/// ]);
/// assert_eq!(evaluate_domain(&eq, &state), Ok(true));
/// ```
///
/// # Errors
///
/// 见上方决策表：域结构错误返回 `TcbError`，业务状态缺失返回 `Ok(false)`。
pub fn evaluate_domain(domain: &JsonValue, exec_state: &JsonValue) -> Result<bool, TcbError> {
    evaluate_domain_outcome_inner(domain, exec_state, 0).map(|o| o.to_bool())
}

/// 三态域评估（专项-20261001 方案 2' v4 主入口）
///
/// eq/lt 三态求值 + `on_missing` 声明分派（`error` →
/// `Err(MissingRejected)` / `unsat` → `Ok(Missing)` 由消费点投影 false
/// + 归因落账）。
///
/// all/not 三值同构短路；exists/instruction/has_fields
/// 为存在性检查本体（二态，无 Missing）。
pub fn evaluate_domain_outcome(
    domain: &JsonValue,
    exec_state: &JsonValue,
) -> Result<DomainOutcome, TcbError> {
    evaluate_domain_outcome_inner(domain, exec_state, 0)
}

/// 读取规则文本的 `on_missing` 声明值（归因透传用，T3）
///
/// 返回声明标签（`"error"` / `"unsat"`）；未声明返回 `None`。与运行时
/// 求值共用同一解析函数（单一权威，零镜像复算）；非法声明值返回 `None`
/// ——此类域无法通过求值（结构错误通道显式报错），本助手只服务已求值
/// 成功的域对象。
pub fn declared_on_missing(domain: &JsonValue) -> Option<&'static str> {
    parse_on_missing(domain).ok().flatten().map(|p| p.label())
}

/// 域评估内部实现（带递归深度限制，三态版）
fn evaluate_domain_outcome_inner(
    domain: &JsonValue,
    exec_state: &JsonValue,
    depth: usize,
) -> Result<DomainOutcome, TcbError> {
    if depth > MAX_DOMAIN_DEPTH {
        return Err(TcbError::NestingTooDeep {
            limit: MAX_DOMAIN_DEPTH,
        });
    }

    let domain_type = get_str_field(domain, "type")?;

    match domain_type {
        "eq" => evaluate_eq_outcome(domain, exec_state),
        "lt" => evaluate_lt_outcome(domain, exec_state),
        "in" => evaluate_in_outcome(domain, exec_state),
        "exists" => evaluate_exists(domain, exec_state).map(DomainOutcome::from_bool),
        "instruction" => evaluate_instruction_eq(domain, exec_state).map(DomainOutcome::from_bool),
        "all" => evaluate_all_outcome(domain, exec_state, depth),
        "not" => evaluate_not_outcome(domain, exec_state, depth),
        "has_fields" => evaluate_has_fields(domain, exec_state).map(DomainOutcome::from_bool),
        other => Err(TcbError::UnknownDomainType {
            domain_type: other.to_string(),
        }),
    }
}

/// 读取域对象的必需字符串字段
///
/// 缺失 → `MissingField`；存在但非字符串 → `InvalidType`。
fn get_str_field<'a>(domain: &'a JsonValue, field: &'static str) -> Result<&'a str, TcbError> {
    let value = domain.get(field).ok_or_else(|| TcbError::MissingField {
        field: field.to_string(),
    })?;
    value.as_str().ok_or_else(|| TcbError::InvalidType {
        expected: "string",
        actual: json_type_name(value),
        context: field.to_string(),
    })
}

/// Eq 三态求值：路径值 == 目标值
///
/// `value` 支持 `__` 开头路径引用（跨字段相等比较）。
/// 路径不存在 → `Missing(PathNotFound)`；`__` 引用解析失败 → 对应
/// Missing 归因；根段点分形态 value → `Missing(ValueLiteralAmbiguous)`。
/// Missing 按 `on_missing` 声明分派（error → Err 拒绝执行 / unsat、
/// 缺省 → Missing 由消费点投影 false）。
fn evaluate_eq_outcome(
    domain: &JsonValue,
    exec_state: &JsonValue,
) -> Result<DomainOutcome, TcbError> {
    let policy = parse_on_missing(domain)?;
    let path = get_str_field(domain, "path")?;
    let value = domain.get("value").ok_or_else(|| TcbError::MissingField {
        field: "value".to_string(),
    })?;
    let target = match resolve_value_reference_outcome(value, exec_state) {
        Ok(t) => t,
        Err(reason) => return dispatch_missing(policy, reason, value_context(value)),
    };
    match resolve_domain_path(exec_state, path) {
        Some(actual) => Ok(if actual == &target {
            DomainOutcome::Sat
        } else {
            DomainOutcome::Unsat
        }),
        None => dispatch_missing(policy, MissingReason::PathNotFound, path),
    }
}

/// Eq 二态兼容包装（旧签名，供未迁移调用方与回归基准；Missing → false）
pub fn evaluate_eq(domain: &JsonValue, exec_state: &JsonValue) -> Result<bool, TcbError> {
    evaluate_eq_outcome(domain, exec_state).map(|o| o.to_bool())
}

/// Lt 三态求值：路径值 < 目标值（仅 i64）
///
/// 路径不存在 → `Missing(PathNotFound)`；value 引用失败 → 对应归因；
/// 两侧均存在但任一侧非 i64 → `Missing(Incomparable)`。
/// Missing 按 `on_missing` 声明分派。
fn evaluate_lt_outcome(
    domain: &JsonValue,
    exec_state: &JsonValue,
) -> Result<DomainOutcome, TcbError> {
    let policy = parse_on_missing(domain)?;
    let path = get_str_field(domain, "path")?;
    let value = domain.get("value").ok_or_else(|| TcbError::MissingField {
        field: "value".to_string(),
    })?;
    let target = match resolve_value_reference_outcome(value, exec_state) {
        Ok(t) => t,
        Err(reason) => return dispatch_missing(policy, reason, value_context(value)),
    };
    let actual = match resolve_domain_path(exec_state, path) {
        Some(a) => a.clone(),
        None => return dispatch_missing(policy, MissingReason::PathNotFound, path),
    };
    match (actual.as_i64(), target.as_i64()) {
        (Some(a), Some(t)) => Ok(if a < t {
            DomainOutcome::Sat
        } else {
            DomainOutcome::Unsat
        }),
        _ => dispatch_missing(policy, MissingReason::Incomparable, path),
    }
}

/// In 三态求值：路径值 ∈ `values` 集合（vendored 增补，design-19 R-2/R-3）
///
/// `values` 为纯字面量数组（元素不做 `__` 引用解析）；字符串元素命中
/// 根段点分形态 → `Missing(ValueLiteralAmbiguous)`（与 eq 的 value 同族
/// 同谓词 [`is_root_segment_dot_path`]，防字面量化静默比对）。空集 →
/// `Unsat`（数学语义：不属于空集合恒假；装载面另行拒收空集写作错误）。
/// 路径不存在 → `Missing(PathNotFound)` 按 `on_missing` 分派；
/// `values` 缺失/非数组 → 结构侧 `Err`（fail-fast）。
fn evaluate_in_outcome(
    domain: &JsonValue,
    exec_state: &JsonValue,
) -> Result<DomainOutcome, TcbError> {
    let policy = parse_on_missing(domain)?;
    let path = get_str_field(domain, "path")?;
    let values = domain.get("values").ok_or_else(|| TcbError::MissingField {
        field: "values".to_string(),
    })?;
    let arr = values.as_array().ok_or_else(|| TcbError::InvalidType {
        expected: "array",
        actual: json_type_name(values),
        context: "in.values".to_string(),
    })?;
    for item in arr.iter() {
        if let JsonValue::String(s) = item {
            if is_root_segment_dot_path(s) {
                return dispatch_missing(policy, MissingReason::ValueLiteralAmbiguous, s);
            }
        }
    }
    let actual = match resolve_domain_path(exec_state, path) {
        Some(a) => a,
        None => return dispatch_missing(policy, MissingReason::PathNotFound, path),
    };
    Ok(if arr.iter().any(|v| v == actual) {
        DomainOutcome::Sat
    } else {
        DomainOutcome::Unsat
    })
}

/// Exists：路径存在且值非 null
///
/// JSON `null` 视为"已清除/不存在"——`core_eval` 用 `set ... = null`
/// 清除 `__io_results__` 后，后续 `exists` 检查必须返回 `false`，
/// 否则陈旧结果会被反复消费、新的 `io_request` 永远无法发起。
fn evaluate_exists(domain: &JsonValue, exec_state: &JsonValue) -> Result<bool, TcbError> {
    let path = get_str_field(domain, "path")?;
    Ok(match resolve_domain_path(exec_state, path) {
        Some(JsonValue::Null) | None => false,
        Some(_) => true,
    })
}

/// InstructionEq：当前指令类型匹配
///
/// 当前指令在状态中缺失 type → `Ok(false)`（状态侧）；
/// 域对象缺失 `instruction_type` → `Err`（结构侧）。
fn evaluate_instruction_eq(domain: &JsonValue, exec_state: &JsonValue) -> Result<bool, TcbError> {
    let instr_type = get_str_field(domain, "instruction_type")?;
    let current = exec_state
        .get("__exec__")
        .and_then(|e| e.get("instruction"))
        .and_then(|i| i.get("type"))
        .and_then(|v| v.as_str());

    Ok(current == Some(instr_type))
}

/// All 三态求值：所有子域为真（空列表 = Sat，真空真约定）
///
/// 三值同构短路：遇 `Unsat` 短路返回、遇 `Missing` 短路返回——与改前
/// bool 版 false 短路逐情形同构（Missing 投影 false 后行为一致，后续
/// 子域不求值，结构错误暴露边界不变）。缺 `inner` 或非数组 → `Err`。
fn evaluate_all_outcome(
    domain: &JsonValue,
    exec_state: &JsonValue,
    depth: usize,
) -> Result<DomainOutcome, TcbError> {
    let inner = domain.get("inner").ok_or_else(|| TcbError::MissingField {
        field: "inner".to_string(),
    })?;
    let arr = inner.as_array().ok_or_else(|| TcbError::InvalidType {
        expected: "array",
        actual: json_type_name(inner),
        context: "all.inner".to_string(),
    })?;

    for sub_domain in arr {
        match evaluate_domain_outcome_inner(sub_domain, exec_state, depth + 1)? {
            DomainOutcome::Sat => continue,
            other => return Ok(other),
        }
    }
    Ok(DomainOutcome::Sat)
}

/// Not 三态求值：Missing 原样传播（归因保留），Sat/Unsat 取反。
/// 缺 `inner` → `Err`（结构侧）。
///
/// 语义修正（vs 改前 bool 版）：改前 not(Missing)=not(false)=true——
/// 「路径缺失」经 not 反转为「真」（fail-open，派生 gt/ge 同源受累）；
/// 三态版 Missing 原样传播、投影 false。见 tests::test_not_missing_propagates 留痕。
fn evaluate_not_outcome(
    domain: &JsonValue,
    exec_state: &JsonValue,
    depth: usize,
) -> Result<DomainOutcome, TcbError> {
    let inner = domain.get("inner").ok_or_else(|| TcbError::MissingField {
        field: "inner".to_string(),
    })?;
    Ok(
        match evaluate_domain_outcome_inner(inner, exec_state, depth + 1)? {
            DomainOutcome::Sat => DomainOutcome::Unsat,
            DomainOutcome::Unsat => DomainOutcome::Sat,
            DomainOutcome::Missing(m) => DomainOutcome::Missing(m),
        },
    )
}

/// HasFields：检查对象是否包含指定的非空字段
///
/// # 参数
/// - `path`：要检查的对象路径
/// - `fields`：要检查的字段名列表（数组）
///
/// # 行为
/// - 结构侧（`Err`）：缺 `path`/`fields`、`fields` 非数组或为空数组、
///   元素非字符串——这些是规则作者的书写错误，必须显式暴露
/// - 状态侧（`Ok(false)`）：目标对象未就位、目标非对象、
///   字段缺失、字段为 null、数组字段为空
fn evaluate_has_fields(domain: &JsonValue, exec_state: &JsonValue) -> Result<bool, TcbError> {
    let path = get_str_field(domain, "path")?;

    let fields_val = domain.get("fields").ok_or_else(|| TcbError::MissingField {
        field: "fields".to_string(),
    })?;
    let fields = fields_val.as_array().ok_or_else(|| TcbError::InvalidType {
        expected: "array",
        actual: json_type_name(fields_val),
        context: "has_fields.fields".to_string(),
    })?;
    if fields.is_empty() {
        return Err(TcbError::InvalidType {
            expected: "non-empty array",
            actual: "empty array",
            context: "has_fields.fields".to_string(),
        });
    }

    // 状态侧：目标对象未就位
    let target = match resolve_domain_path(exec_state, path) {
        Some(t) => t,
        None => return Ok(false),
    };

    // 状态侧：目标不是对象
    let obj = match target.as_object() {
        Some(o) => o,
        None => return Ok(false),
    };

    for field_value in fields {
        let field_name = field_value.as_str().ok_or_else(|| TcbError::InvalidType {
            expected: "string",
            actual: json_type_name(field_value),
            context: "has_fields.fields element".to_string(),
        })?;

        // 状态侧：字段必须存在
        let value = match obj.get(field_name) {
            Some(v) => v,
            None => return Ok(false),
        };

        // 如果是数组，必须非空
        if let Some(arr) = value.as_array() {
            if arr.is_empty() {
                return Ok(false);
            }
        }

        // 如果是 null，视为不存在
        if value.is_null() {
            return Ok(false);
        }

        // 其他类型（bool, integer, string, object）只要存在就视为有效
    }

    Ok(true)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    #![allow(clippy::panic)]
    #![allow(clippy::indexing_slicing)]

    use super::*;
    use crate::error::TcbError;
    use crate::value::{JsonValue, ObjectMap};
    use alloc::string::ToString;
    use alloc::vec;

    // ===== 辅助函数 =====

    /// 测试辅助：断言求值成功并返回布尔值。
    /// 结构错误会 panic 暴露（语义用例不应产生 Err）。
    fn eval_ok(domain: &JsonValue, state: &JsonValue) -> bool {
        match evaluate_domain(domain, state) {
            Ok(b) => b,
            Err(e) => panic!("evaluate_domain returned Err: {:?}", e),
        }
    }

    fn make_exec_state(instruction_type: &str, payload: JsonValue) -> JsonValue {
        let mut exec = ObjectMap::new();
        exec.insert(
            "instruction".to_string(),
            make_instruction(instruction_type),
        );
        exec.insert("payload".to_string(), payload);
        let mut root = ObjectMap::new();
        root.insert("__exec__".to_string(), JsonValue::Object(exec));
        JsonValue::Object(root)
    }

    fn make_payload(x: i64) -> JsonValue {
        let mut map = ObjectMap::new();
        map.insert("x".to_string(), JsonValue::Integer(x));
        JsonValue::Object(map)
    }

    fn make_instruction(instr_type: &str) -> JsonValue {
        let mut map = ObjectMap::new();
        map.insert("type".to_string(), JsonValue::string(instr_type));
        JsonValue::Object(map)
    }

    // ===== eq 测试 =====

    #[test]
    fn test_eq_true() {
        let state = make_exec_state("noop", make_payload(10));
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("eq")),
            ("path", JsonValue::string("__exec__.payload.x")),
            ("value", JsonValue::Integer(10)),
        ]);
        assert!(eval_ok(&domain, &state));
    }

    #[test]
    fn test_eq_false() {
        let state = make_exec_state("noop", make_payload(10));
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("eq")),
            ("path", JsonValue::string("__exec__.payload.x")),
            ("value", JsonValue::Integer(20)),
        ]);
        assert!(!eval_ok(&domain, &state));
    }

    #[test]
    fn test_eq_string_comparison() {
        let mut payload = ObjectMap::new();
        payload.insert("name".to_string(), JsonValue::string("hello"));
        let payload = JsonValue::Object(payload);
        let state = make_exec_state("noop", payload);

        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("eq")),
            ("path", JsonValue::string("__exec__.payload.name")),
            ("value", JsonValue::string("hello")),
        ]);
        assert!(eval_ok(&domain, &state));

        let domain_false = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("eq")),
            ("path", JsonValue::string("__exec__.payload.name")),
            ("value", JsonValue::string("world")),
        ]);
        assert!(!eval_ok(&domain_false, &state));
    }

    #[test]
    fn test_eq_missing_path_is_error() {
        let state = make_exec_state("noop", make_payload(10));
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("eq")),
            ("value", JsonValue::Integer(10)),
        ]);
        // M5：缺 path 是规则结构错误，必须显式报错
        assert!(matches!(
            evaluate_domain(&domain, &state),
            Err(TcbError::MissingField { field }) if field == "path"
        ));
    }

    #[test]
    fn test_eq_missing_value_is_error() {
        let state = make_exec_state("noop", make_payload(10));
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("eq")),
            ("path", JsonValue::string("__exec__.payload.x")),
        ]);
        assert!(matches!(
            evaluate_domain(&domain, &state),
            Err(TcbError::MissingField { field }) if field == "value"
        ));
    }

    #[test]
    fn test_eq_resolve_failed_returns_false() {
        let state = make_exec_state("noop", make_payload(10));
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("eq")),
            ("path", JsonValue::string("__exec__.payload.missing")),
            ("value", JsonValue::Integer(42)),
        ]);
        assert!(!eval_ok(&domain, &state));
    }

    // ===== lt 测试 =====

    #[test]
    fn test_lt_true() {
        let state = make_exec_state("noop", make_payload(10));
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("lt")),
            ("path", JsonValue::string("__exec__.payload.x")),
            ("value", JsonValue::Integer(20)),
        ]);
        assert!(eval_ok(&domain, &state));
    }

    #[test]
    fn test_lt_false() {
        let state = make_exec_state("noop", make_payload(10));
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("lt")),
            ("path", JsonValue::string("__exec__.payload.x")),
            ("value", JsonValue::Integer(5)),
        ]);
        assert!(!eval_ok(&domain, &state));
    }

    #[test]
    fn test_lt_equal_false() {
        let state = make_exec_state("noop", make_payload(10));
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("lt")),
            ("path", JsonValue::string("__exec__.payload.x")),
            ("value", JsonValue::Integer(10)),
        ]);
        assert!(!eval_ok(&domain, &state));
    }

    #[test]
    fn test_lt_missing_path_is_error() {
        let state = make_exec_state("noop", make_payload(10));
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("lt")),
            ("value", JsonValue::Integer(20)),
        ]);
        assert!(matches!(
            evaluate_domain(&domain, &state),
            Err(TcbError::MissingField { field }) if field == "path"
        ));
    }

    #[test]
    fn test_lt_resolve_failed_returns_false() {
        let state = make_exec_state("noop", make_payload(10));
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("lt")),
            ("path", JsonValue::string("__exec__.payload.missing")),
            ("value", JsonValue::Integer(20)),
        ]);
        assert!(!eval_ok(&domain, &state));
    }

    #[test]
    fn test_lt_non_integer_returns_false() {
        let mut payload = ObjectMap::new();
        payload.insert("name".to_string(), JsonValue::string("hello"));
        let payload = JsonValue::Object(payload);
        let state = make_exec_state("noop", payload);

        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("lt")),
            ("path", JsonValue::string("__exec__.payload.name")),
            ("value", JsonValue::Integer(10)),
        ]);
        assert!(!eval_ok(&domain, &state));
    }

    // ===== in 测试（vendored 增补，design-19）=====

    fn make_in_domain(values: JsonValue) -> JsonValue {
        JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("in")),
            ("path", JsonValue::string("__exec__.payload.x")),
            ("values", values),
            ("on_missing", JsonValue::string("unsat")),
        ])
    }

    fn make_string_array(items: &[&str]) -> JsonValue {
        let v: alloc::vec::Vec<JsonValue> =
            items.iter().map(|s| JsonValue::string(*s)).collect();
        JsonValue::Array(v)
    }

    #[test]
    fn test_in_member_sat() {
        let state = make_exec_state("noop", JsonValue::object_from_pairs(&[
            ("x", JsonValue::string("MiniMax-M3")),
        ]));
        let domain = make_in_domain(make_string_array(&["MiniMax-M2.5", "MiniMax-M3"]));
        assert!(eval_ok(&domain, &state));
    }

    #[test]
    fn test_in_not_member_unsat() {
        let state = make_exec_state("noop", JsonValue::object_from_pairs(&[
            ("x", JsonValue::string("other-model")),
        ]));
        let domain = make_in_domain(make_string_array(&["MiniMax-M2.5", "MiniMax-M3"]));
        assert!(!eval_ok(&domain, &state));
    }

    #[test]
    fn test_in_integer_member() {
        let state = make_exec_state("noop", make_payload(2));
        let domain = make_in_domain(JsonValue::Array(vec![
            JsonValue::Integer(1),
            JsonValue::Integer(2),
        ]));
        assert!(eval_ok(&domain, &state));
    }

    #[test]
    fn test_in_empty_values_is_unsat() {
        let state = make_exec_state("noop", make_payload(1));
        let domain = make_in_domain(JsonValue::Array(vec![]));
        assert!(!eval_ok(&domain, &state));
    }

    #[test]
    fn test_in_path_missing_projects_false() {
        let state = make_exec_state("noop", make_payload(1));
        // 无 on_missing（缺省兼容 unsat）+ 路径缺失 → Missing 投影 false
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("in")),
            ("path", JsonValue::string("__exec__.payload.missing")),
            ("values", make_string_array(&["a"])),
        ]);
        assert!(!eval_ok(&domain, &state));
    }

    #[test]
    fn test_in_on_missing_error_rejects() {
        let state = make_exec_state("noop", make_payload(1));
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("in")),
            ("path", JsonValue::string("__exec__.payload.missing")),
            ("values", make_string_array(&["a"])),
            ("on_missing", JsonValue::string("error")),
        ]);
        assert!(matches!(
            evaluate_domain(&domain, &state),
            Err(TcbError::MissingRejected { .. })
        ));
    }

    #[test]
    fn test_in_missing_values_is_structural_error() {
        let state = make_exec_state("noop", make_payload(1));
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("in")),
            ("path", JsonValue::string("__exec__.payload.x")),
        ]);
        assert!(matches!(
            evaluate_domain(&domain, &state),
            Err(TcbError::MissingField { field }) if field == "values"
        ));
    }

    #[test]
    fn test_in_values_not_array_is_structural_error() {
        let state = make_exec_state("noop", make_payload(1));
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("in")),
            ("path", JsonValue::string("__exec__.payload.x")),
            ("values", JsonValue::string("a")),
        ]);
        assert!(matches!(
            evaluate_domain(&domain, &state),
            Err(TcbError::InvalidType { context, .. }) if context == "in.values"
        ));
    }

    #[test]
    fn test_in_root_segment_element_is_ambiguous() {
        let state = make_exec_state("noop", JsonValue::object_from_pairs(&[
            ("x", JsonValue::string("instruction.params.model")),
        ]));
        let domain = make_in_domain(make_string_array(&["instruction.params.model"]));
        assert!(!eval_ok(&domain, &state));
    }

    #[test]
    fn test_in_outcome_deterministic() {
        let state = make_exec_state("noop", JsonValue::object_from_pairs(&[
            ("x", JsonValue::string("MiniMax-M2.5")),
        ]));
        let domain = make_in_domain(make_string_array(&["MiniMax-M2.5", "MiniMax-M3"]));
        let a = evaluate_domain_outcome(&domain, &state);
        let b = evaluate_domain_outcome(&domain, &state);
        assert_eq!(a, b);
        assert!(matches!(a, Ok(DomainOutcome::Sat)));
    }

    // ===== exists 测试 =====

    #[test]
    fn test_exists_true() {
        let state = make_exec_state("noop", make_payload(10));
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("exists")),
            ("path", JsonValue::string("__exec__.payload.x")),
        ]);
        assert!(eval_ok(&domain, &state));
    }

    #[test]
    fn test_exists_false() {
        let state = make_exec_state("noop", make_payload(10));
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("exists")),
            ("path", JsonValue::string("__exec__.payload.missing")),
        ]);
        assert!(!eval_ok(&domain, &state));
    }

    #[test]
    fn test_exists_missing_path_is_error() {
        let state = make_exec_state("noop", make_payload(10));
        let domain = JsonValue::object_from_pairs(&[("type", JsonValue::string("exists"))]);
        assert!(matches!(
            evaluate_domain(&domain, &state),
            Err(TcbError::MissingField { field }) if field == "path"
        ));
    }

    /// null 值视为"已清除"：I/O 结果被 set 为 null 后，exists 必须返回 false，
    /// 否则 ReAct 循环中陈旧结果会被反复消费、新 io_request 无法发起。
    #[test]
    fn test_exists_null_value_returns_false() {
        let mut payload = ObjectMap::new();
        payload.insert("cleared".to_string(), JsonValue::Null);
        payload.insert("live".to_string(), JsonValue::Integer(1));
        let payload = JsonValue::Object(payload);
        let state = make_exec_state("noop", payload);

        let cleared = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("exists")),
            ("path", JsonValue::string("__exec__.payload.cleared")),
        ]);
        assert!(!eval_ok(&cleared, &state));

        let live = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("exists")),
            ("path", JsonValue::string("__exec__.payload.live")),
        ]);
        assert!(eval_ok(&live, &state));
    }

    // ===== instruction 测试 =====

    #[test]
    fn test_instruction_eq_true() {
        let state = make_exec_state("increment", make_payload(0));
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("instruction")),
            ("instruction_type", JsonValue::string("increment")),
        ]);
        assert!(eval_ok(&domain, &state));
    }

    #[test]
    fn test_instruction_eq_false() {
        let state = make_exec_state("increment", make_payload(0));
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("instruction")),
            ("instruction_type", JsonValue::string("decrement")),
        ]);
        assert!(!eval_ok(&domain, &state));
    }

    #[test]
    fn test_instruction_eq_missing_current_returns_false() {
        let root = ObjectMap::new();
        let state = JsonValue::Object(root);
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("instruction")),
            ("instruction_type", JsonValue::string("set")),
        ]);
        assert!(!eval_ok(&domain, &state));
    }

    /// Contract test: `make_instruction("xxx")` 必须产出 `{"type": "xxx"}` 形态。
    /// 锁住 helper 的输出 shape，防止以后误改签名。
    #[test]
    fn test_make_instruction_shape() {
        let instr = make_instruction("noop");
        let mut expected_map = ObjectMap::new();
        expected_map.insert("type".to_string(), JsonValue::string("noop"));
        assert_eq!(instr, JsonValue::Object(expected_map));

        // 空字符串也按字面值处理（不做特殊语义）
        let empty_instr = make_instruction("");
        assert_eq!(empty_instr.get("type").and_then(|v| v.as_str()), Some(""));
    }

    /// 用 `make_instruction` helper 构造 state 中的 `__exec__.instruction`，
    /// 跑 `evaluate_instruction_eq` 的 happy path，
    /// 证明 helper 输出与 evaluate 链路兼容。
    #[test]
    fn test_instruction_eq_using_make_instruction_helper() {
        // 用 helper 构造 instruction 并嵌入 state
        let mut exec_inner = ObjectMap::new();
        exec_inner.insert("instruction".to_string(), make_instruction("branch"));
        let mut root = ObjectMap::new();
        root.insert("__exec__".to_string(), JsonValue::Object(exec_inner));
        let state = JsonValue::Object(root);

        // 匹配路径：domain 期望 "branch"，state 当前 instruction type 是 "branch" → true
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("instruction")),
            ("instruction_type", JsonValue::string("branch")),
        ]);
        assert!(eval_ok(&domain, &state));

        // 不匹配 → false
        let mismatch = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("instruction")),
            ("instruction_type", JsonValue::string("set")),
        ]);
        assert!(!eval_ok(&mismatch, &state));
    }

    // ===== all 测试 =====

    #[test]
    fn test_all_true() {
        let state = make_exec_state("noop", make_payload(10));
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("all")),
            (
                "inner",
                JsonValue::array(vec![JsonValue::object_from_pairs(&[
                    ("type", JsonValue::string("eq")),
                    ("path", JsonValue::string("__exec__.payload.x")),
                    ("value", JsonValue::Integer(10)),
                ])]),
            ),
        ]);
        assert!(eval_ok(&domain, &state));
    }

    #[test]
    fn test_all_false() {
        let state = make_exec_state("noop", make_payload(10));
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("all")),
            (
                "inner",
                JsonValue::array(vec![JsonValue::object_from_pairs(&[
                    ("type", JsonValue::string("eq")),
                    ("path", JsonValue::string("__exec__.payload.x")),
                    ("value", JsonValue::Integer(20)),
                ])]),
            ),
        ]);
        assert!(!eval_ok(&domain, &state));
    }

    #[test]
    fn test_all_multiple_conditions() {
        let state = make_exec_state("noop", make_payload(10));
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("all")),
            (
                "inner",
                JsonValue::array(vec![
                    JsonValue::object_from_pairs(&[
                        ("type", JsonValue::string("eq")),
                        ("path", JsonValue::string("__exec__.payload.x")),
                        ("value", JsonValue::Integer(10)),
                    ]),
                    JsonValue::object_from_pairs(&[
                        ("type", JsonValue::string("lt")),
                        ("path", JsonValue::string("__exec__.payload.x")),
                        ("value", JsonValue::Integer(20)),
                    ]),
                ]),
            ),
        ]);
        assert!(eval_ok(&domain, &state));
    }

    #[test]
    fn test_all_empty_list_is_true() {
        let state = make_exec_state("noop", make_payload(0));
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("all")),
            ("inner", JsonValue::empty_array()),
        ]);
        assert!(eval_ok(&domain, &state));
    }

    #[test]
    fn test_all_no_inner_is_error() {
        let state = make_exec_state("noop", make_payload(0));
        let domain = JsonValue::object_from_pairs(&[("type", JsonValue::string("all"))]);
        // L3：缺 inner 是结构错误；空数组才是合法的真空真
        assert!(matches!(
            evaluate_domain(&domain, &state),
            Err(TcbError::MissingField { field }) if field == "inner"
        ));
    }

    #[test]
    fn test_all_inner_not_array_is_error() {
        let state = make_exec_state("noop", make_payload(0));
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("all")),
            ("inner", JsonValue::Integer(42)),
        ]);
        assert!(matches!(
            evaluate_domain(&domain, &state),
            Err(TcbError::InvalidType { context, .. }) if context == "all.inner"
        ));
    }

    // ===== not 测试 =====

    #[test]
    fn test_not_true() {
        let state = make_exec_state("noop", make_payload(10));
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("not")),
            (
                "inner",
                JsonValue::object_from_pairs(&[
                    ("type", JsonValue::string("eq")),
                    ("path", JsonValue::string("__exec__.payload.x")),
                    ("value", JsonValue::Integer(20)),
                ]),
            ),
        ]);
        assert!(eval_ok(&domain, &state));
    }

    #[test]
    fn test_not_false() {
        let state = make_exec_state("noop", make_payload(10));
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("not")),
            (
                "inner",
                JsonValue::object_from_pairs(&[
                    ("type", JsonValue::string("eq")),
                    ("path", JsonValue::string("__exec__.payload.x")),
                    ("value", JsonValue::Integer(10)),
                ]),
            ),
        ]);
        assert!(!eval_ok(&domain, &state));
    }

    #[test]
    fn test_not_no_inner_is_error() {
        let state = make_exec_state("noop", make_payload(0));
        let domain = JsonValue::object_from_pairs(&[("type", JsonValue::string("not"))]);
        // L3：缺 inner 是结构错误（此前 Not(空)=true 与 fail-open 风险同源）
        assert!(matches!(
            evaluate_domain(&domain, &state),
            Err(TcbError::MissingField { field }) if field == "inner"
        ));
    }

    // ===== 嵌套测试 =====

    #[test]
    fn test_nested_not_all() {
        let state = make_exec_state("noop", make_payload(10));
        // not(all([eq(x,10)])) = false
        let inner_all = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("all")),
            (
                "inner",
                JsonValue::array(vec![JsonValue::object_from_pairs(&[
                    ("type", JsonValue::string("eq")),
                    ("path", JsonValue::string("__exec__.payload.x")),
                    ("value", JsonValue::Integer(10)),
                ])]),
            ),
        ]);
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("not")),
            ("inner", inner_all),
        ]);
        assert!(!eval_ok(&domain, &state));
    }

    #[test]
    fn test_deep_nesting_within_limit() {
        let state = make_exec_state("noop", make_payload(10));

        let mut domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("eq")),
            ("path", JsonValue::string("__exec__.payload.x")),
            ("value", JsonValue::Integer(10)),
        ]);

        // 嵌套 30 层 Not（偶数层 = 原值 = true）
        for _ in 0..30 {
            domain = JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("not")),
                ("inner", domain),
            ]);
        }
        assert!(eval_ok(&domain, &state));

        // 再加一层 Not（奇数层 = 取反 = false）
        domain =
            JsonValue::object_from_pairs(&[("type", JsonValue::string("not")), ("inner", domain)]);
        assert!(!eval_ok(&domain, &state));
    }

    #[test]
    fn test_exceeds_max_depth_is_error() {
        let state = make_exec_state("noop", make_payload(10));

        let mut domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("eq")),
            ("path", JsonValue::string("__exec__.payload.x")),
            ("value", JsonValue::Integer(10)),
        ]);

        // 嵌套 65 层 Not（超过 MAX_DOMAIN_DEPTH = 64）
        for _ in 0..65 {
            domain = JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("not")),
                ("inner", domain),
            ]);
        }

        // 超深是结构错误（终止性防线），显式报错而非静默 false
        assert!(matches!(
            evaluate_domain(&domain, &state),
            Err(TcbError::NestingTooDeep { limit: 64 })
        ));
    }

    // ===== 派生域类型测试（组合） =====

    #[test]
    fn test_derived_ne() {
        // Ne(a,b) = Not(Eq(a,b))
        let state = make_exec_state("noop", make_payload(10));

        // 10 != 20 → true
        let ne_true = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("not")),
            (
                "inner",
                JsonValue::object_from_pairs(&[
                    ("type", JsonValue::string("eq")),
                    ("path", JsonValue::string("__exec__.payload.x")),
                    ("value", JsonValue::Integer(20)),
                ]),
            ),
        ]);
        assert!(eval_ok(&ne_true, &state));

        // 10 != 10 → false
        let ne_false = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("not")),
            (
                "inner",
                JsonValue::object_from_pairs(&[
                    ("type", JsonValue::string("eq")),
                    ("path", JsonValue::string("__exec__.payload.x")),
                    ("value", JsonValue::Integer(10)),
                ]),
            ),
        ]);
        assert!(!eval_ok(&ne_false, &state));
    }

    #[test]
    fn test_derived_gt() {
        // Gt(a,b) = All([Not(Lt(a,b)), Not(Eq(a,b))])
        let state = make_exec_state("noop", make_payload(10));

        // 10 > 5 → true
        let gt_true = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("all")),
            (
                "inner",
                JsonValue::array(vec![
                    JsonValue::object_from_pairs(&[
                        ("type", JsonValue::string("not")),
                        (
                            "inner",
                            JsonValue::object_from_pairs(&[
                                ("type", JsonValue::string("lt")),
                                ("path", JsonValue::string("__exec__.payload.x")),
                                ("value", JsonValue::Integer(5)),
                            ]),
                        ),
                    ]),
                    JsonValue::object_from_pairs(&[
                        ("type", JsonValue::string("not")),
                        (
                            "inner",
                            JsonValue::object_from_pairs(&[
                                ("type", JsonValue::string("eq")),
                                ("path", JsonValue::string("__exec__.payload.x")),
                                ("value", JsonValue::Integer(5)),
                            ]),
                        ),
                    ]),
                ]),
            ),
        ]);
        assert!(eval_ok(&gt_true, &state));

        // 10 > 10 → false
        let gt_false = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("all")),
            (
                "inner",
                JsonValue::array(vec![
                    JsonValue::object_from_pairs(&[
                        ("type", JsonValue::string("not")),
                        (
                            "inner",
                            JsonValue::object_from_pairs(&[
                                ("type", JsonValue::string("lt")),
                                ("path", JsonValue::string("__exec__.payload.x")),
                                ("value", JsonValue::Integer(10)),
                            ]),
                        ),
                    ]),
                    JsonValue::object_from_pairs(&[
                        ("type", JsonValue::string("not")),
                        (
                            "inner",
                            JsonValue::object_from_pairs(&[
                                ("type", JsonValue::string("eq")),
                                ("path", JsonValue::string("__exec__.payload.x")),
                                ("value", JsonValue::Integer(10)),
                            ]),
                        ),
                    ]),
                ]),
            ),
        ]);
        assert!(!eval_ok(&gt_false, &state));
    }

    #[test]
    fn test_derived_ge() {
        // Ge(a,b) = Not(Lt(a,b))
        let state = make_exec_state("noop", make_payload(10));

        // 10 >= 5 → true
        let ge_true = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("not")),
            (
                "inner",
                JsonValue::object_from_pairs(&[
                    ("type", JsonValue::string("lt")),
                    ("path", JsonValue::string("__exec__.payload.x")),
                    ("value", JsonValue::Integer(5)),
                ]),
            ),
        ]);
        assert!(eval_ok(&ge_true, &state));

        // 10 >= 10 → true
        let ge_equal = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("not")),
            (
                "inner",
                JsonValue::object_from_pairs(&[
                    ("type", JsonValue::string("lt")),
                    ("path", JsonValue::string("__exec__.payload.x")),
                    ("value", JsonValue::Integer(10)),
                ]),
            ),
        ]);
        assert!(eval_ok(&ge_equal, &state));

        // 10 >= 20 → false
        let ge_false = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("not")),
            (
                "inner",
                JsonValue::object_from_pairs(&[
                    ("type", JsonValue::string("lt")),
                    ("path", JsonValue::string("__exec__.payload.x")),
                    ("value", JsonValue::Integer(20)),
                ]),
            ),
        ]);
        assert!(!eval_ok(&ge_false, &state));
    }

    #[test]
    fn test_derived_le() {
        // Le(a,b) = Not(Gt(a,b))
        let state = make_exec_state("noop", make_payload(10));

        let gt_domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("all")),
            (
                "inner",
                JsonValue::array(vec![
                    JsonValue::object_from_pairs(&[
                        ("type", JsonValue::string("not")),
                        (
                            "inner",
                            JsonValue::object_from_pairs(&[
                                ("type", JsonValue::string("lt")),
                                ("path", JsonValue::string("__exec__.payload.x")),
                                ("value", JsonValue::Integer(20)),
                            ]),
                        ),
                    ]),
                    JsonValue::object_from_pairs(&[
                        ("type", JsonValue::string("not")),
                        (
                            "inner",
                            JsonValue::object_from_pairs(&[
                                ("type", JsonValue::string("eq")),
                                ("path", JsonValue::string("__exec__.payload.x")),
                                ("value", JsonValue::Integer(20)),
                            ]),
                        ),
                    ]),
                ]),
            ),
        ]);

        // 10 <= 20 → Not(Gt(10,20)) → true
        let le_true = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("not")),
            ("inner", gt_domain),
        ]);
        assert!(eval_ok(&le_true, &state));
    }

    #[test]
    fn test_derived_or() {
        // Or(a,b) = Not(All([Not(a), Not(b)]))
        let state = make_exec_state("noop", make_payload(10));

        // 10 == 10 OR 10 == 20 → true
        let or_true = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("not")),
            (
                "inner",
                JsonValue::object_from_pairs(&[
                    ("type", JsonValue::string("all")),
                    (
                        "inner",
                        JsonValue::array(vec![
                            JsonValue::object_from_pairs(&[
                                ("type", JsonValue::string("not")),
                                (
                                    "inner",
                                    JsonValue::object_from_pairs(&[
                                        ("type", JsonValue::string("eq")),
                                        ("path", JsonValue::string("__exec__.payload.x")),
                                        ("value", JsonValue::Integer(10)),
                                    ]),
                                ),
                            ]),
                            JsonValue::object_from_pairs(&[
                                ("type", JsonValue::string("not")),
                                (
                                    "inner",
                                    JsonValue::object_from_pairs(&[
                                        ("type", JsonValue::string("eq")),
                                        ("path", JsonValue::string("__exec__.payload.x")),
                                        ("value", JsonValue::Integer(20)),
                                    ]),
                                ),
                            ]),
                        ]),
                    ),
                ]),
            ),
        ]);
        assert!(eval_ok(&or_true, &state));

        // 15 == 10 OR 15 == 20 → false
        let state2 = make_exec_state("noop", make_payload(15));
        assert!(!eval_ok(&or_true, &state2));
    }

    // ===== 自动补全路径前缀测试 =====

    #[test]
    fn test_payload_prefix_auto_complete() {
        let state = make_exec_state("noop", make_payload(10));

        // payload.x → __exec__.payload.x
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("eq")),
            ("path", JsonValue::string("payload.x")),
            ("value", JsonValue::Integer(10)),
        ]);
        assert!(eval_ok(&domain, &state));
    }

    #[test]
    fn test_instruction_prefix_auto_complete() {
        let state = make_exec_state("increment", make_payload(0));

        // instruction.type → __exec__.instruction.type
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("exists")),
            ("path", JsonValue::string("instruction.type")),
        ]);
        assert!(eval_ok(&domain, &state));
    }

    #[test]
    fn test_full_path_still_works() {
        let state = make_exec_state("noop", make_payload(10));

        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("eq")),
            ("path", JsonValue::string("__exec__.payload.x")),
            ("value", JsonValue::Integer(10)),
        ]);
        assert!(eval_ok(&domain, &state));
    }

    // ===== 未知域类型测试 =====

    #[test]
    fn test_unknown_domain_type_is_error() {
        let state = make_exec_state("noop", make_payload(0));
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("unknown_domain")),
            ("path", JsonValue::string("__exec__.payload.x")),
        ]);
        // M5：未知域类型显式报错（此前静默 false 经 not 反转为 true = fail-open）
        assert!(matches!(
            evaluate_domain(&domain, &state),
            Err(TcbError::UnknownDomainType { domain_type }) if domain_type == "unknown_domain"
        ));
    }

    /// M5 核心回归：not(未知类型) 必须报错，
    /// 绝不允许静默 false → not 反转 → fail-open 返回 true。
    #[test]
    fn test_not_unknown_type_is_error_not_true() {
        let state = make_exec_state("noop", make_payload(0));
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("not")),
            (
                "inner",
                JsonValue::object_from_pairs(&[
                    ("type", JsonValue::string("e")), // 拼错的 eq
                    ("path", JsonValue::string("__exec__.payload.x")),
                    ("value", JsonValue::Integer(10)),
                ]),
            ),
        ]);
        assert!(matches!(
            evaluate_domain(&domain, &state),
            Err(TcbError::UnknownDomainType { domain_type }) if domain_type == "e"
        ));
    }

    /// 非对象域（如字符串）是结构错误
    #[test]
    fn test_non_object_domain_is_error() {
        let state = make_exec_state("noop", make_payload(0));
        let domain = JsonValue::string("not a domain");
        assert!(matches!(
            evaluate_domain(&domain, &state),
            Err(TcbError::MissingField { field }) if field == "type"
        ));
    }

    /// 域 type 字段存在但非字符串 → InvalidType
    #[test]
    fn test_domain_type_not_string_is_error() {
        let state = make_exec_state("noop", make_payload(0));
        let domain = JsonValue::object_from_pairs(&[("type", JsonValue::Integer(42))]);
        assert!(matches!(
            evaluate_domain(&domain, &state),
            Err(TcbError::InvalidType { context, .. }) if context == "type"
        ));
    }

    // ===== 数组索引路径测试 =====

    #[test]
    fn test_eq_with_array_index_path() {
        let mut item = ObjectMap::new();
        item.insert("value".to_string(), JsonValue::Integer(42));
        let items = JsonValue::array(vec![JsonValue::Object(item)]);
        let mut payload = ObjectMap::new();
        payload.insert("items".to_string(), items);
        let payload = JsonValue::Object(payload);
        let state = make_exec_state("noop", payload);

        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("eq")),
            ("path", JsonValue::string("__exec__.payload.items[0].value")),
            ("value", JsonValue::Integer(42)),
        ]);
        assert!(eval_ok(&domain, &state));
    }

    // ===== L4：eq/lt 的 value 路径引用测试 =====

    #[test]
    fn test_eq_value_as_path_reference() {
        // 跨字段相等：payload.x == payload.expected
        let mut payload = ObjectMap::new();
        payload.insert("x".to_string(), JsonValue::Integer(10));
        payload.insert("expected".to_string(), JsonValue::Integer(10));
        let payload = JsonValue::Object(payload);
        let state = make_exec_state("noop", payload);

        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("eq")),
            ("path", JsonValue::string("__exec__.payload.x")),
            ("value", JsonValue::string("__exec__.payload.expected")),
        ]);
        assert!(eval_ok(&domain, &state));
    }

    #[test]
    fn test_eq_value_as_path_reference_mismatch() {
        let mut payload = ObjectMap::new();
        payload.insert("x".to_string(), JsonValue::Integer(10));
        payload.insert("expected".to_string(), JsonValue::Integer(20));
        let payload = JsonValue::Object(payload);
        let state = make_exec_state("noop", payload);

        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("eq")),
            ("path", JsonValue::string("__exec__.payload.x")),
            ("value", JsonValue::string("__exec__.payload.expected")),
        ]);
        assert!(!eval_ok(&domain, &state));
    }

    #[test]
    fn test_eq_value_unresolvable_path_reference_is_false() {
        // 引用路径不存在 → 状态侧 false（且不会把 null 当作可匹配值）
        let state = make_exec_state("noop", make_payload(10));
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("eq")),
            ("path", JsonValue::string("__exec__.payload.x")),
            ("value", JsonValue::string("__exec__.payload.missing")),
        ]);
        assert!(!eval_ok(&domain, &state));
    }

    #[test]
    fn test_lt_value_as_path_reference() {
        // payload.x < payload.limit
        let mut payload = ObjectMap::new();
        payload.insert("x".to_string(), JsonValue::Integer(10));
        payload.insert("limit".to_string(), JsonValue::Integer(20));
        let payload = JsonValue::Object(payload);
        let state = make_exec_state("noop", payload);

        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("lt")),
            ("path", JsonValue::string("__exec__.payload.x")),
            ("value", JsonValue::string("__exec__.payload.limit")),
        ]);
        assert!(eval_ok(&domain, &state));
    }

    // ===== has_fields 结构/状态测试 =====

    #[test]
    fn test_has_fields_all_present_true() {
        let mut target = ObjectMap::new();
        target.insert("a".to_string(), JsonValue::Integer(1));
        target.insert(
            "b".to_string(),
            JsonValue::array(vec![JsonValue::Integer(2)]),
        );
        let mut payload = ObjectMap::new();
        payload.insert("obj".to_string(), JsonValue::Object(target));
        let payload = JsonValue::Object(payload);
        let state = make_exec_state("noop", payload);

        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("has_fields")),
            ("path", JsonValue::string("__exec__.payload.obj")),
            (
                "fields",
                JsonValue::array(vec![JsonValue::string("a"), JsonValue::string("b")]),
            ),
        ]);
        assert!(eval_ok(&domain, &state));
    }

    #[test]
    fn test_has_fields_missing_field_false() {
        // 状态侧：字段缺失 → false（不报错）
        let mut target = ObjectMap::new();
        target.insert("a".to_string(), JsonValue::Integer(1));
        let mut payload = ObjectMap::new();
        payload.insert("obj".to_string(), JsonValue::Object(target));
        let payload = JsonValue::Object(payload);
        let state = make_exec_state("noop", payload);

        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("has_fields")),
            ("path", JsonValue::string("__exec__.payload.obj")),
            (
                "fields",
                JsonValue::array(vec![JsonValue::string("a"), JsonValue::string("c")]),
            ),
        ]);
        assert!(!eval_ok(&domain, &state));
    }

    #[test]
    fn test_has_fields_target_not_object_false() {
        // 状态侧：目标非对象 → false
        let mut payload = ObjectMap::new();
        payload.insert("obj".to_string(), JsonValue::Integer(42));
        let payload = JsonValue::Object(payload);
        let state = make_exec_state("noop", payload);

        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("has_fields")),
            ("path", JsonValue::string("__exec__.payload.obj")),
            ("fields", JsonValue::array(vec![JsonValue::string("a")])),
        ]);
        assert!(!eval_ok(&domain, &state));
    }

    #[test]
    fn test_has_fields_target_missing_false() {
        // 状态侧：目标路径不存在 → false（对象尚未就位）
        let state = make_exec_state("noop", make_payload(0));
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("has_fields")),
            ("path", JsonValue::string("__exec__.payload.obj")),
            ("fields", JsonValue::array(vec![JsonValue::string("a")])),
        ]);
        assert!(!eval_ok(&domain, &state));
    }

    #[test]
    fn test_has_fields_empty_fields_is_error() {
        // 结构侧：空 fields 无意义 → 报错（L3：不再是静默 false）
        let state = make_exec_state("noop", make_payload(0));
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("has_fields")),
            ("path", JsonValue::string("__exec__.payload.obj")),
            ("fields", JsonValue::empty_array()),
        ]);
        assert!(matches!(
            evaluate_domain(&domain, &state),
            Err(TcbError::InvalidType { context, .. }) if context == "has_fields.fields"
        ));
    }

    #[test]
    fn test_has_fields_missing_fields_is_error() {
        let state = make_exec_state("noop", make_payload(0));
        let domain = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("has_fields")),
            ("path", JsonValue::string("__exec__.payload.obj")),
        ]);
        assert!(matches!(
            evaluate_domain(&domain, &state),
            Err(TcbError::MissingField { field }) if field == "fields"
        ));
    }

    #[test]
    fn test_has_fields_null_or_empty_array_field_false() {
        // 状态侧：null 字段与空数组字段视为不存在
        let mut target = ObjectMap::new();
        target.insert("n".to_string(), JsonValue::Null);
        target.insert("e".to_string(), JsonValue::empty_array());
        target.insert("ok".to_string(), JsonValue::string("v"));
        let mut payload = ObjectMap::new();
        payload.insert("obj".to_string(), JsonValue::Object(target));
        let payload = JsonValue::Object(payload);
        let state = make_exec_state("noop", payload);

        let with_null = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("has_fields")),
            ("path", JsonValue::string("__exec__.payload.obj")),
            ("fields", JsonValue::array(vec![JsonValue::string("n")])),
        ]);
        assert!(!eval_ok(&with_null, &state));

        let with_empty = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("has_fields")),
            ("path", JsonValue::string("__exec__.payload.obj")),
            ("fields", JsonValue::array(vec![JsonValue::string("e")])),
        ]);
        assert!(!eval_ok(&with_empty, &state));

        let with_ok = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("has_fields")),
            ("path", JsonValue::string("__exec__.payload.obj")),
            ("fields", JsonValue::array(vec![JsonValue::string("ok")])),
        ]);
        assert!(eval_ok(&with_ok, &state));
    }

    // ===== 三态域判定测试（设计 v4：包装一致性/声明执行语义/确定性幂等素材）=====

    /// 测试辅助：构造带 on_missing 声明的 eq 域
    fn eq_with_missing(path: &str, value: JsonValue, on_missing: Option<&str>) -> JsonValue {
        let mut map = ObjectMap::new();
        map.insert("type".to_string(), JsonValue::string("eq"));
        map.insert("path".to_string(), JsonValue::string(path));
        map.insert("value".to_string(), value);
        if let Some(om) = on_missing {
            map.insert("on_missing".to_string(), JsonValue::string(om));
        }
        JsonValue::Object(map)
    }

    /// 测试辅助：构造带 on_missing 声明的 lt 域
    fn lt_with_missing(path: &str, value: JsonValue, on_missing: Option<&str>) -> JsonValue {
        let mut map = ObjectMap::new();
        map.insert("type".to_string(), JsonValue::string("lt"));
        map.insert("path".to_string(), JsonValue::string(path));
        map.insert("value".to_string(), value);
        if let Some(om) = on_missing {
            map.insert("on_missing".to_string(), JsonValue::string(om));
        }
        JsonValue::Object(map)
    }

    #[test]
    fn test_outcome_eq_sat_unsat_with_declaration() {
        let state = make_exec_state("noop", make_payload(10));
        let mk = |v: i64, om: Option<&str>| {
            eq_with_missing("__exec__.payload.x", JsonValue::Integer(v), om)
        };
        assert_eq!(
            evaluate_domain_outcome(&mk(10, None), &state).unwrap(),
            DomainOutcome::Sat
        );
        assert_eq!(
            evaluate_domain_outcome(&mk(20, None), &state).unwrap(),
            DomainOutcome::Unsat
        );
        assert_eq!(
            evaluate_domain_outcome(&mk(20, Some("unsat")), &state).unwrap(),
            DomainOutcome::Unsat
        );
    }

    /// 声明执行语义·前半：eq 路径缺失的三种声明走向
    #[test]
    fn test_outcome_eq_path_missing_on_missing_policies() {
        let state = make_exec_state("noop", make_payload(10));

        // 缺省（无声明，存量兼容缺省 unsat）→ Missing(PathNotFound)，投影 false
        let d_default = eq_with_missing("__exec__.payload.missing", JsonValue::Integer(1), None);
        assert_eq!(
            evaluate_domain_outcome(&d_default, &state).unwrap(),
            DomainOutcome::Missing(MissingReason::PathNotFound)
        );
        assert!(!evaluate_domain(&d_default, &state).unwrap());

        // 显式 unsat → Missing + 归因（显式声明下的 false，非静默）
        let d_unsat = eq_with_missing(
            "__exec__.payload.missing",
            JsonValue::Integer(1),
            Some("unsat"),
        );
        assert_eq!(
            evaluate_domain_outcome(&d_unsat, &state).unwrap(),
            DomainOutcome::Missing(MissingReason::PathNotFound)
        );

        // 显式 error → Err(MissingRejected) 拒绝执行（detail 含归因标签与路径）
        let d_error = eq_with_missing(
            "__exec__.payload.missing",
            JsonValue::Integer(1),
            Some("error"),
        );
        assert!(matches!(
            evaluate_domain_outcome(&d_error, &state),
            Err(TcbError::MissingRejected { detail })
                if detail == "path_not_found: __exec__.payload.missing"
        ));
    }

    /// 声明执行语义·后半：lt 类型不可比（Incomparable）两种声明走向
    #[test]
    fn test_outcome_lt_incomparable_policies() {
        let mut p = ObjectMap::new();
        p.insert("name".to_string(), JsonValue::string("hello"));
        let state = make_exec_state("noop", JsonValue::Object(p));

        let d_unsat = lt_with_missing(
            "__exec__.payload.name",
            JsonValue::Integer(0),
            Some("unsat"),
        );
        assert_eq!(
            evaluate_domain_outcome(&d_unsat, &state).unwrap(),
            DomainOutcome::Missing(MissingReason::Incomparable)
        );
        // 显式声明下的 false：投影与改前一致
        assert!(!evaluate_domain(&d_unsat, &state).unwrap());

        let d_error = lt_with_missing(
            "__exec__.payload.name",
            JsonValue::Integer(0),
            Some("error"),
        );
        assert!(matches!(
            evaluate_domain_outcome(&d_error, &state),
            Err(TcbError::MissingRejected { detail }) if detail.starts_with("incomparable:")
        ));
    }

    /// 实测素材：历史实测复现形态（eq value 无前缀字面量）——
    /// unsat 声明 → 归因 Missing(ValueLiteralAmbiguous)；
    /// error 声明 → Err（ValueLiteralAmbiguous 一律从严）
    #[test]
    fn test_outcome_value_literal_ambiguous_policies() {
        let state = make_exec_state("noop", make_payload(10));

        let d_unsat = eq_with_missing(
            "__exec__.payload.x",
            JsonValue::string("instruction.params.milestone_target"),
            Some("unsat"),
        );
        assert_eq!(
            evaluate_domain_outcome(&d_unsat, &state).unwrap(),
            DomainOutcome::Missing(MissingReason::ValueLiteralAmbiguous)
        );
        assert!(!evaluate_domain(&d_unsat, &state).unwrap());

        let d_error = eq_with_missing(
            "__exec__.payload.x",
            JsonValue::string("instruction.params.milestone_target"),
            Some("error"),
        );
        assert!(matches!(
            evaluate_domain_outcome(&d_error, &state),
            Err(TcbError::MissingRejected { detail })
                if detail == "value_literal_ambiguous: instruction.params.milestone_target"
        ));
    }

    /// ValueLiteralAmbiguous 判定口径（root-segment-dot-path.v1）正负例锚定：
    /// 正例=历史实测旧形态（eq value 无前缀）；负例=专项盘点 10 处合法字面量（零误伤）
    #[test]
    fn test_value_literal_ambiguous_rule_cases() {
        // 正例：根段点分形态（像路径引用但缺 __ 前缀，写作错误）
        assert!(is_root_segment_dot_path(
            "instruction.params.milestone_target"
        ));
        assert!(is_root_segment_dot_path("payload.x"));
        assert!(is_root_segment_dot_path("queue.front"));

        // 负例：专项盘点合法符号常量/模型名字面量（零误伤锚定）
        assert!(!is_root_segment_dot_path("meta_tool.pending_target_scope"));
        assert!(!is_root_segment_dot_path("meta_workflow.phase"));
        assert!(!is_root_segment_dot_path("meta_tool.pending_tool_intent"));
        assert!(!is_root_segment_dot_path("meta_signal.node_done"));
        assert!(!is_root_segment_dot_path("MiniMax-M2.5"));

        // 边界：单段（无点）不是路径形态；空段/非法字符/大写根段保守当字面量
        assert!(!is_root_segment_dot_path("instruction"));
        assert!(!is_root_segment_dot_path("instruction..x"));
        assert!(!is_root_segment_dot_path("instruction.params.x "));
        assert!(!is_root_segment_dot_path("Instruction.params.x"));
        // __ 前缀走引用分支，不进歧义判定
        assert!(!is_root_segment_dot_path("__exec__.payload.x"));
    }

    /// 版本化锁定锚：判定口径变更必须换版本号（防跨版本归因漂移）
    #[test]
    fn test_value_literal_ambiguous_rule_versioned() {
        assert_eq!(VALUE_LITERAL_AMBIGUOUS_RULE, "root-segment-dot-path.v1");
    }

    /// 包装一致性：无 on_missing 声明输入下，outcome 二态投影
    /// 与改前 bool 实现逐情形一致（Missing → false）
    #[test]
    fn test_wrapper_consistency_missing_projects_false() {
        let state = make_exec_state("noop", make_payload(10));

        // 路径缺失 → Missing → false
        let d1 = eq_with_missing("__exec__.payload.missing", JsonValue::Integer(42), None);
        assert!(!evaluate_domain(&d1, &state).unwrap());
        assert_eq!(
            evaluate_domain_outcome(&d1, &state).unwrap(),
            DomainOutcome::Missing(MissingReason::PathNotFound)
        );

        // 引用路径缺失 → PathNotFound → false
        let d2 = eq_with_missing(
            "__exec__.payload.x",
            JsonValue::string("__exec__.payload.missing"),
            None,
        );
        assert!(!evaluate_domain(&d2, &state).unwrap());
        assert_eq!(
            evaluate_domain_outcome(&d2, &state).unwrap(),
            DomainOutcome::Missing(MissingReason::PathNotFound)
        );

        // 不可比 → Incomparable → false
        let mut p = ObjectMap::new();
        p.insert("name".to_string(), JsonValue::string("hello"));
        let state2 = make_exec_state("noop", JsonValue::Object(p));
        let d3 = lt_with_missing("__exec__.payload.name", JsonValue::Integer(0), None);
        assert!(!evaluate_domain(&d3, &state2).unwrap());
        assert_eq!(
            evaluate_domain_outcome(&d3, &state2).unwrap(),
            DomainOutcome::Missing(MissingReason::Incomparable)
        );

        // 引用歧义 → ValueLiteralAmbiguous → false
        let d4 = eq_with_missing(
            "__exec__.payload.x",
            JsonValue::string("instruction.params.milestone_target"),
            None,
        );
        assert!(!evaluate_domain(&d4, &state).unwrap());
        assert_eq!(
            evaluate_domain_outcome(&d4, &state).unwrap(),
            DomainOutcome::Missing(MissingReason::ValueLiteralAmbiguous)
        );
    }

    /// 包装一致性边界锚：all([Missing, 结构错误])——Missing 短路不暴露
    /// 后续结构错误，与改前 bool 版 false 短路边界一致
    #[test]
    fn test_all_missing_short_circuit_boundary_preserved() {
        let state = make_exec_state("noop", make_payload(10));
        let missing_eq = eq_with_missing("__exec__.payload.missing", JsonValue::Integer(1), None);
        let structural_err = JsonValue::object_from_pairs(&[("type", JsonValue::string("eq"))]); // 缺 path/value
        let all = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("all")),
            ("inner", JsonValue::array(vec![missing_eq, structural_err])),
        ]);
        // 改前：false（false 短路，不报错）；改后：Missing 投影 false（不报错）
        assert!(!evaluate_domain(&all, &state).unwrap());
        assert!(matches!(
            evaluate_domain_outcome(&all, &state),
            Ok(DomainOutcome::Missing(_))
        ));
    }

    /// not 对 Missing 原样传播（归因不被取反吞掉）。
    ///
    /// **语义修正留痕（vs 改前）**：改前 bool 版 not(Missing)=not(false)=true
    /// ——「路径缺失」经 not 反转为「真」（fail-open）；三态版 Missing
    /// 原样传播，投影 false。这是归因显式化的必然语义（Missing 被 not
    /// 吞掉=归因丢失=静默通道借 not 还魂），属根修语义修复，见实施
    /// 留痕「not 包裹比较域存量影响面核查」。
    #[test]
    fn test_not_missing_propagates() {
        let state = make_exec_state("noop", make_payload(10));
        let missing_eq = eq_with_missing("__exec__.payload.missing", JsonValue::Integer(1), None);
        let not = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("not")),
            ("inner", missing_eq),
        ]);
        assert!(matches!(
            evaluate_domain_outcome(&not, &state),
            Ok(DomainOutcome::Missing(MissingReason::PathNotFound))
        ));
        // Missing 投影 false（不再被 not 反转为 true）
        assert!(!evaluate_domain(&not, &state).unwrap());
    }

    /// 确定性幂等：同输入重复求值 N 次 outcome 逐次全等
    #[test]
    fn test_outcome_deterministic_idempotent() {
        let state = make_exec_state("noop", make_payload(10));
        let domain = eq_with_missing(
            "__exec__.payload.missing",
            JsonValue::Integer(1),
            Some("unsat"),
        );
        let first = evaluate_domain_outcome(&domain, &state);
        for _ in 0..10 {
            assert_eq!(evaluate_domain_outcome(&domain, &state), first);
        }
    }

    /// on_missing 非法值 = 结构错误（TCB 无警告只有报错，"warn" 等不设警告通道）
    #[test]
    fn test_on_missing_invalid_value_is_structural_error() {
        let state = make_exec_state("noop", make_payload(10));
        let mut map = ObjectMap::new();
        map.insert("type".to_string(), JsonValue::string("eq"));
        map.insert("path".to_string(), JsonValue::string("__exec__.payload.x"));
        map.insert("value".to_string(), JsonValue::Integer(10));
        map.insert("on_missing".to_string(), JsonValue::string("warn"));
        let domain = JsonValue::Object(map);
        assert!(matches!(
            evaluate_domain(&domain, &state),
            Err(TcbError::InvalidType { context, .. }) if context == "on_missing"
        ));
    }
}

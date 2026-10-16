// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 元指令执行器 - 5 个元指令 + `io_request` 信号 + `enforce` 强制原语
//!
//! # 元指令列表
//! - `set`：修改 payload 字段
//! - `push`：推指令到队列前端
//! - `branch`：条件执行子指令列表
//! - `io_request`：产生 I/O 请求信号（不修改状态）
//! - `enforce`：元规则强制原语（回归验证）——params.domain 匹配即返回 Halted 信号
//!
//! # 设计原则
//! `io_request` 是"半元指令"——在执行器中硬编码识别，但行为完全由 JSON 参数驱动。
//! 它不修改任何状态，仅返回 `MetaInstructionResult::IoRequired` 信号。

use crate::domain::{declared_on_missing, evaluate_domain_outcome, DomainAttribution};
use crate::error::TcbError;
use crate::path::{parse_path_segments, resolve_path, resolve_path_mut, PathSegment};
use crate::value::{JsonValue, ObjectMap};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// 元指令执行器的最大嵌套深度
pub const MAX_BRANCH_DEPTH: usize = 64;

/// 单次状态转换允许执行的元指令总数上限（终止性宽度防线，M6）
///
/// 与 `MAX_BRANCH_DEPTH`（深度防线）、`MAX_TRANSFORM_RULES`（规则数防线）、
/// `MAX_DOMAIN_DEPTH`（域递归防线）共同构成完整的终止性保证：
/// 深度限制无法约束**宽度**（单条 branch 的子指令列表长度无上界），
/// 本上限补齐该缺口——任意 core_eval 的执行路径指令总数有界。
///
/// 取值依据：`MAX_TRANSFORM_RULES`(64) × 单规则平均指令数(≤16) = 1024，
/// 覆盖 ReAct 宪法的实际需求（每条规则 < 15 条指令）并留有余量。
pub const MAX_TOTAL_META_INSTRUCTIONS: usize = 1024;

/// 合法 core_eval 元指令类型权威清单（SSOT / C2）
///
/// 与下方 `execute_meta_instruction_budgeted` 的 dispatch 分支一一对应。
/// 消费方（如 evorule-cli validate 的规则白名单）**必须引用本常量**，
/// 禁止自行硬编码副本——防止 tcb 新增元指令时消费方误报合法规则。
///
/// # 漂移防线
/// - 本表 → dispatch：`test_meta_instruction_types_ssot` 逐类型断言
///   dispatch 不返回 `UnknownMetaInstruction`；
/// - dispatch → 本表：新增 dispatch 分支时须同步更新本表并登记变更
///   （该方向不可由枚举自动穷尽，靠 CR 门禁约束）。
pub const META_INSTRUCTION_TYPES: &[&str] = &["branch", "set", "push", "io_request", "enforce"];

/// 元指令执行结果
#[derive(Debug, Clone, PartialEq)]
pub enum MetaInstructionResult {
    /// 正常执行，返回更新后的状态
    State(JsonValue),
    /// I/O 请求信号（立即传播，不继续执行后续指令）
    IoRequired {
        /// I/O 类型（如 "call_external"、"query_db"）
        io_type: String,
        /// I/O 请求参数（路径引用已解析为具体值）
        params: JsonValue,
    },
    /// 强制中断信号（`enforce` 原语命中，回归验证）
    ///
    /// 纯信号：立即向上传播，不携带状态；状态转换层据此返回
    /// `TransitionResult::Halted`（违规指令被拒，此前状态修改随
    /// 半成品纪律一并丢弃）。与 `IoRequired` 同为传播即停语义。
    Halted {
        /// 违规说明（enforce params.reason，供 Violation 事实审计回显）
        reason: String,
    },
}

/// 执行一条元指令，返回执行结果
///
/// 独立入口：每次调用获得满额执行预算（`MAX_TOTAL_META_INSTRUCTIONS`）。
/// 状态转换层（`execute_transition`）使用 [`execute_meta_instruction_budgeted`]
/// 在整棵规则树上共享单一预算。
pub fn execute_meta_instruction(
    instr: &JsonValue,
    state: JsonValue,
    depth: usize,
) -> Result<MetaInstructionResult, TcbError> {
    let mut budget = MAX_TOTAL_META_INSTRUCTIONS;
    let mut hit = false;
    let mut domain_attr = None;
    execute_meta_instruction_budgeted(instr, state, depth, &mut budget, &mut hit, &mut domain_attr)
}

/// 执行一条元指令（受共享预算约束，M6 终止性宽度防线）
///
/// 每执行一条指令（含 branch 的子指令递归）消耗 1 个预算单位；
/// 预算耗尽返回 `TooManyExecutedInstructions`。
///
/// # 命中归因（结构命中口径）
///
/// `hit_out` 在执行成功时写入该条指令是否**结构命中**：
/// - 直接指令（set/push）：执行成功即命中；
/// - `io_request`：产生信号即命中；
/// - `branch`：所选分支（on_true/on_false）**存在且非空**即命中——
///   空数组或缺失分支不命中（无效果路径）。
///
/// 口径为**结构命中**而非副作用命中：与值是否实际变化无关（幂等重放
/// 如 `set` 同值仍算命中），判定稳定且零状态对比开销。调用方在
/// `Err` 返回时不得采信 `hit_out`（未定义）。递归子指令的命中
/// 不计入顶层规则（命中粒度 = 顶层规则级，见 `TransitionResult`）。
///
/// # 域判定归因（专项-20261001 方案 2' v4，R1/R2 纪律）
///
/// `domain_attr_out` 在 branch/enforce 的域求值成功后写入三态归因
/// （`DomainAttribution`：`DomainOutcome` 含 Missing 分类 + `on_missing`
/// 声明值）；其余指令类型不写入（保持 `None`）。
/// - **R1 归因不回灌执行**：branch 走向仅由投影 bool（`Sat → true`，其余
///   `→ false`）+ 规则文本 `on_missing` 静态声明决定；归因仅作审计用途。
/// - **R2 半成品纪律**：`IoRequired`/`Halted` 不携带 `rule_hits`（转换层
///   既有契约），中途归因同样不随半成品交付，以收敛后的重放结果为准。
/// - 调用方在 `Err` 返回时不得采信 `domain_attr_out`（未定义）。
/// - 递归子指令的域归因不计入顶层规则（同命中粒度：顶层规则级）。
pub(crate) fn execute_meta_instruction_budgeted(
    instr: &JsonValue,
    state: JsonValue,
    depth: usize,
    budget: &mut usize,
    hit_out: &mut bool,
    domain_attr_out: &mut Option<DomainAttribution>,
) -> Result<MetaInstructionResult, TcbError> {
    // 预算检查：先扣减后执行（失败指令同样计入，防止用错误路径消耗无界资源）
    if let Some(next) = budget.checked_sub(1) {
        *budget = next;
    } else {
        return Err(TcbError::TooManyExecutedInstructions {
            limit: MAX_TOTAL_META_INSTRUCTIONS,
        });
    }

    let instr_type = instr
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or(TcbError::MissingField {
            field: "type".to_string(),
        })?;

    match instr_type {
        "set" => exec_set(instr, state)
            .map(MetaInstructionResult::State)
            .inspect(|_| *hit_out = true),
        "push" => exec_push(instr, state)
            .map(MetaInstructionResult::State)
            .inspect(|_| *hit_out = true),
        "branch" => exec_branch(instr, state, depth, budget, hit_out, domain_attr_out),
        "io_request" => exec_io_request(instr, state).inspect(|_| *hit_out = true),
        "enforce" => exec_enforce(instr, state, domain_attr_out).inspect(|result| {
            // 结构命中口径：domain 求值为真（Halted 信号产生）即命中；
            // 求值为假（noop 继续）不命中。
            if matches!(result, MetaInstructionResult::Halted { .. }) {
                *hit_out = true;
            }
        }),
        _ => Err(TcbError::UnknownMetaInstruction {
            meta_type: instr_type.to_string(),
        }),
    }
}

#[cfg(test)]
mod executor_ssot_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    /// SSOT 漂移防线（ / C2）：
    /// `META_INSTRUCTION_TYPES` 中每个类型都必须被 dispatch 实际处理
    /// （不得返回 `UnknownMetaInstruction`）——防止白名单比执行器更严，
    /// 导致消费方（evorule-cli validate）误报合法规则。
    #[test]
    fn test_meta_instruction_types_ssot() {
        assert_eq!(META_INSTRUCTION_TYPES.len(), 5);
        for t in META_INSTRUCTION_TYPES {
            let instr = JsonValue::object_from_pairs(&[("type", JsonValue::string(*t))]);
            if let Err(TcbError::UnknownMetaInstruction { .. }) =
                execute_meta_instruction(&instr, JsonValue::Null, 0)
            {
                panic!("META_INSTRUCTION_TYPES 含未实现类型 '{t}'——白名单与 dispatch 漂移");
            }
            // Ok 或其他错误（如缺字段路径）均不算白名单漂移
        }
    }
}

/// 解析路径或字面值
///
/// 如果值是 `__` 开头的字符串，则视为路径并解析；
/// 否则作为字面值返回。
fn resolve_path_or_literal(
    state: &JsonValue,
    val: Option<&JsonValue>,
) -> Result<JsonValue, TcbError> {
    match val {
        Some(JsonValue::String(s)) if s.starts_with("__") => resolve_path(state, s)
            .cloned()
            .ok_or_else(|| TcbError::PathResolutionFailed {
                path: s.to_string(),
                reason: "path not found".to_string(),
            }),
        Some(v) => Ok(v.clone()),
        None => Err(TcbError::MissingField {
            field: "value".to_string(),
        }),
    }
}

/// 返回 `JsonValue` 的小写类型名，用于错误诊断。
pub(crate) fn json_type_name(v: &JsonValue) -> &'static str {
    match v {
        JsonValue::Null => "null",
        JsonValue::Bool(_) => "boolean",
        JsonValue::Integer(_) => "integer",
        JsonValue::String(_) => "string",
        JsonValue::Array(_) => "array",
        JsonValue::Object(_) => "object",
    }
}

/// set 元指令：修改 payload 字段（attr 支持数组索引，v0.3.2 起）
///
/// attr 的三种写法：
/// 1. 普通 payload 字段路径（如 `"x"`、`"a.b"`、`"items[0].done"`）：相对
///    `__exec__.payload` 解析，语法与 domain 读取路径一致（点号 + `[N]` 索引）
/// 2. 状态路径引用（如 `"__exec__.instruction.params.attr"`）：先解析为字符串，
///    再作为 payload 字段路径（间接寻址）
/// 3. 显式 payload 相对路径（如 `"__exec__.payload.__io_results__.call_external"`）：
///    前缀 `__exec__.payload.` 之后的后缀直接作为 payload 内相对路径，不做状态解析。
///    用于引用 payload 内以 `__` 开头的字段（否则写法 2 会把它误判为状态根路径）
///
/// # 索引写入语义（显式优先）
/// - 中间对象段缺失/null：自动创建空对象（auto-vivification，既有行为）
/// - 索引段：目标数组**必须已存在**（不隐式创建，数组长度无法从索引推断）；
///   索引越界报错（不隐式追加，追加须由 push 显式完成）
//
// 注：本函数有意保持单一函数承载完整路径解析语义分支（三种 attr 写法 +
// payload 前缀守卫 + 索引写入语义），便于审计路径解析行为；行数超出 clippy
// 默认阈值（105/100），按 clippy 官方建议用函数级 allow 豁免。
#[allow(clippy::too_many_lines)]
fn exec_set(instr: &JsonValue, mut state: JsonValue) -> Result<JsonValue, TcbError> {
    let params = instr.get("params").ok_or(TcbError::MissingField {
        field: "params".to_string(),
    })?;

    // attr 支持路径引用；同时支持显式 payload 相对路径（见函数文档注释）
    let attr_raw = params.get("attr").ok_or(TcbError::MissingField {
        field: "attr".to_string(),
    })?;

    let resolve_attr_from_state = || -> Result<String, TcbError> {
        let attr_value = resolve_path_or_literal(&state, Some(attr_raw))?;
        attr_value
            .as_str()
            .map(ToString::to_string)
            .ok_or_else(|| TcbError::InvalidType {
                expected: "string",
                actual: json_type_name(&attr_value),
                context: "attr".to_string(),
            })
    };

    // 写侧 attr 禁止 payload. 前缀（方案 a，引擎级运行时守卫）：
    // attr 相对 __exec__.payload 解析，带 payload. 前缀会双重嵌套写入
    // payload.payload.*（静默失败——不报错但写到错误位置，读者永远 miss，
    // 实证：种子元规则哨兵自读不到自身状态标记恒违规）。
    // schema 门禁（payload_attr_path）在加载/提交期已拒；此处兜底覆盖
    // 状态引用间接产生的前缀与绕过门禁的执行入口。
    // 豁免：__exec__.payload. 显式全形式剥离后的后缀不再守卫——作者显式锚定
    // payload 根（如 __exec__.payload.payload.deep 写 payload 内同名子字段）
    // 是意图明确的合法写法；schema 侧 not.pattern 只匹配裸串，与此一致。
    let attr: String = match attr_raw.as_str() {
        Some(raw) => match raw.strip_prefix("__exec__.payload.") {
            Some(suffix) => suffix.to_string(),
            None => {
                let resolved = resolve_attr_from_state()?;
                if resolved.starts_with("payload.") {
                    return Err(TcbError::PathResolutionFailed {
                        path: resolved,
                        reason: "写侧 attr 禁止以 payload. 开头：attr 相对 __exec__.payload \
                                 解析，带前缀会双重嵌套写入 payload.payload.*。请直接写 \
                                 payload 内相对路径（如 meta_guard.cleared_for_move），\
                                 真需写 payload 内同名子字段时用 __exec__.payload. 显式全形式"
                            .to_string(),
                    });
                }
                resolved
            }
        },
        None => {
            let resolved = resolve_attr_from_state()?;
            if resolved.starts_with("payload.") {
                return Err(TcbError::PathResolutionFailed {
                    path: resolved,
                    reason: "写侧 attr 禁止以 payload. 开头：attr 相对 __exec__.payload 解析，\
                             带前缀会双重嵌套写入 payload.payload.*。请直接写 \
                             payload 内相对路径（如 meta_guard.cleared_for_move），\
                             真需写 payload 内同名子字段时用 __exec__.payload. 显式全形式"
                        .to_string(),
                });
            }
            resolved
        }
    };

    let operation =
        params
            .get("operation")
            .and_then(|v| v.as_str())
            .ok_or(TcbError::MissingField {
                field: "operation".to_string(),
            })?;

    let value = resolve_path_or_literal(&state, params.get("value"))?;

    // 解析 attr 路径为段序列（支持字段访问与数组索引，语法与 domain 读取路径一致）
    let segments = parse_path_segments(&attr).ok_or_else(|| TcbError::PathResolutionFailed {
        path: attr.to_string(),
        reason: "invalid path syntax (empty segment or malformed index)".to_string(),
    })?;
    if segments.is_empty() {
        return Err(TcbError::PathResolutionFailed {
            path: attr.to_string(),
            reason: "empty path".to_string(),
        });
    }

    // 获取 __exec__.payload 根节点
    let payload = resolve_path_mut(&mut state, "__exec__.payload").ok_or_else(|| {
        TcbError::PathResolutionFailed {
            path: "__exec__.payload".to_string(),
            reason: "payload not found in state".to_string(),
        }
    })?;

    let slot = descend_to_write_slot(payload, &segments, &attr)?;

    let current = match &slot {
        WriteSlot::Field(obj, name) => obj
            .get(name.as_str())
            .cloned()
            // null ≡ 已清除/不存在 ≡ 算术起点 0（L7：与缺失字段、
            // domain.exists 的 null 哲学对齐；operation=set 时 current 被丢弃，不受影响）
            .map(|v| match v {
                JsonValue::Null => JsonValue::Integer(0),
                other => other,
            })
            .unwrap_or(JsonValue::Integer(0)),
        WriteSlot::Index(arr, idx) => {
            arr.get(*idx)
                .cloned()
                .ok_or_else(|| TcbError::PathResolutionFailed {
                    path: attr.to_string(),
                    reason: "array slot vanished".to_string(),
                })?
        }
    };

    let new_value = apply_set_operation(operation, current, value)?;

    match slot {
        WriteSlot::Field(obj, name) => {
            obj.insert(name, new_value);
        }
        WriteSlot::Index(arr, idx) => {
            if let Some(element) = arr.get_mut(idx) {
                *element = new_value;
            }
        }
    }
    Ok(state)
}

/// 应用 set 运算：`set` 覆盖、`add`/`sub` checked 算术（溢出报 `IntegerOverflow`）
fn apply_set_operation(
    operation: &str,
    current: JsonValue,
    value: JsonValue,
) -> Result<JsonValue, TcbError> {
    match operation {
        "set" => Ok(value),
        "add" | "sub" => {
            let cur = current.as_i64().ok_or_else(|| TcbError::InvalidType {
                expected: "integer",
                actual: json_type_name(&current),
                context: format!("{operation} left operand"),
            })?;
            let val = value.as_i64().ok_or_else(|| TcbError::InvalidType {
                expected: "integer",
                actual: json_type_name(&value),
                context: format!("{operation} right operand"),
            })?;
            let result = if operation == "add" {
                cur.checked_add(val)
            } else {
                cur.checked_sub(val)
            }
            .ok_or_else(|| TcbError::IntegerOverflow {
                operation: operation.to_string(),
                left: cur,
                right: val,
            })?;
            Ok(JsonValue::Integer(result))
        }
        op => Err(TcbError::UnknownOperation {
            operation: op.to_string(),
        }),
    }
}

/// attr 下降后的可写位置：对象字段或数组元素槽位
enum WriteSlot<'a> {
    /// 对象字段（`&mut ObjectMap`，字段名）
    Field(&'a mut ObjectMap, String),
    /// 数组元素槽位（`&mut [JsonValue]`，已验证索引）
    Index(&'a mut [JsonValue], usize),
}

/// 沿段序列下降到可写位置（支持数组索引写入）
///
/// # 下降语义（结构错误显式报错）
///
/// - **中间字段段**：缺失或为 null 时自动创建空对象（auto-vivification，
///   与既有行为一致）；存在但非对象 → 报错。
///   例外：若下一段是索引段，则缺失/null **不**创建占位对象而是直接报错
///   （数组长度无法从索引推断，隐式创建会制造语义陷阱）。
/// - **索引段**：目标数组**必须已存在**（缺失/null → 报错，禁止隐式创建）；
///   索引越界（`>= len`）→ 报错（禁止稀疏数组与隐式追加，追加须由
///   push 等显式机制完成）；存在但非数组 → 报错。
/// - **末段**：返回可写槽位（字段或元素）。
fn descend_to_write_slot<'a>(
    payload: &'a mut JsonValue,
    segments: &[PathSegment],
    attr: &str,
) -> Result<WriteSlot<'a>, TcbError> {
    let mut current = payload;
    let Some(last) = segments.len().checked_sub(1) else {
        return Err(TcbError::PathResolutionFailed {
            path: attr.to_string(),
            reason: "empty path".to_string(),
        });
    };

    for (i, seg) in segments.iter().enumerate() {
        let is_last = i == last;
        match seg {
            PathSegment::Field(name) => {
                match descend_field_segment(current, name, is_last, segments, i, attr)? {
                    DescendStep::Slot(slot) => return Ok(slot),
                    DescendStep::Next(next) => current = next,
                }
            }
            PathSegment::Index(opt_field, idx) => {
                match descend_index_segment(current, opt_field, *idx, is_last, attr)? {
                    DescendStep::Slot(slot) => return Ok(slot),
                    DescendStep::Next(next) => current = next,
                }
            }
        }
    }

    Err(TcbError::PathResolutionFailed {
        path: attr.to_string(),
        reason: "empty path".to_string(),
    })
}

/// 单段下降的结果：到达可写槽位，或取得继续下降的下一节点
enum DescendStep<'a> {
    /// 末段：返回可写槽位
    Slot(WriteSlot<'a>),
    /// 中间段：继续下降的下一节点
    Next(&'a mut JsonValue),
}

/// 处理字段段下降（auto-vivification 规则见 `descend_to_write_slot` 文档）
fn descend_field_segment<'a>(
    current: &'a mut JsonValue,
    name: &str,
    is_last: bool,
    segments: &[PathSegment],
    i: usize,
    attr: &str,
) -> Result<DescendStep<'a>, TcbError> {
    if !matches!(current, JsonValue::Object(_)) {
        let ty = json_type_name(current);
        return Err(TcbError::PathResolutionFailed {
            path: attr.to_string(),
            reason: format!("node at segment '{}' is {}, expected object", name, ty),
        });
    }
    let obj = current
        .as_object_mut()
        .ok_or_else(|| TcbError::PathResolutionFailed {
            path: attr.to_string(),
            reason: "failed to enter object node".to_string(),
        })?;
    if is_last {
        return Ok(DescendStep::Slot(WriteSlot::Field(obj, name.to_string())));
    }
    let next_is_index = matches!(segments.get(i + 1), Some(PathSegment::Index(..)));
    match obj.get_mut(name) {
        None | Some(JsonValue::Null) => {
            if next_is_index {
                return Err(TcbError::PathResolutionFailed {
                    path: attr.to_string(),
                    reason: format!(
                        "array '{}' not found; set cannot auto-create arrays \
                         (create via push first)",
                        name
                    ),
                });
            }
            obj.insert(name.to_string(), JsonValue::empty_object());
        }
        Some(JsonValue::Object(_)) => {}
        Some(other) => {
            let ty = json_type_name(other);
            return Err(TcbError::PathResolutionFailed {
                path: attr.to_string(),
                reason: format!("intermediate segment '{}' is {}, expected object", name, ty),
            });
        }
    }
    let next = obj
        .get_mut(name)
        .ok_or_else(|| TcbError::PathResolutionFailed {
            path: attr.to_string(),
            reason: "failed to descend into intermediate object".to_string(),
        })?;
    Ok(DescendStep::Next(next))
}

/// 处理索引段下降（数组必须已存在、越界报错，规则见 `descend_to_write_slot` 文档）
fn descend_index_segment<'a>(
    current: &'a mut JsonValue,
    opt_field: &Option<String>,
    idx: usize,
    is_last: bool,
    attr: &str,
) -> Result<DescendStep<'a>, TcbError> {
    let target: &mut JsonValue = if let Some(f) = opt_field {
        if !matches!(current, JsonValue::Object(_)) {
            return Err(TcbError::PathResolutionFailed {
                path: attr.to_string(),
                reason: format!(
                    "node before array '{}' is {}, expected object",
                    f,
                    json_type_name(current)
                ),
            });
        }
        let obj = current
            .as_object_mut()
            .ok_or_else(|| TcbError::PathResolutionFailed {
                path: attr.to_string(),
                reason: "failed to enter object node".to_string(),
            })?;
        match obj.get_mut(f.as_str()) {
            None | Some(JsonValue::Null) => {
                return Err(TcbError::PathResolutionFailed {
                    path: attr.to_string(),
                    reason: format!(
                        "array '{}' not found; set cannot auto-create arrays \
                         (create via push first)",
                        f
                    ),
                });
            }
            Some(v) => v,
        }
    } else {
        current
    };
    let arr = target
        .as_array_mut()
        .ok_or_else(|| TcbError::PathResolutionFailed {
            path: attr.to_string(),
            reason: format!(
                "segment '{}' is not an array",
                opt_field.clone().unwrap_or_default()
            ),
        })?;
    if idx >= arr.len() {
        return Err(TcbError::PathResolutionFailed {
            path: attr.to_string(),
            reason: format!("index {} out of bounds (array length {})", idx, arr.len()),
        });
    }
    if is_last {
        return Ok(DescendStep::Slot(WriteSlot::Index(arr, idx)));
    }
    let next = arr
        .get_mut(idx)
        .ok_or_else(|| TcbError::PathResolutionFailed {
            path: attr.to_string(),
            reason: "failed to descend into array element".to_string(),
        })?;
    Ok(DescendStep::Next(next))
}

/// 解析 instructions 列表，支持数组元素中的路径引用
///
/// 顶层值如果是路径引用字符串，先解析为数组；
/// 然后遍历数组元素，如果元素是 `__` 开头的字符串，解析为路径引用。
/// 路径引用解析为数组时自动展平。
fn resolve_instructions_list(
    state: &JsonValue,
    val: Option<&JsonValue>,
) -> Result<Vec<JsonValue>, TcbError> {
    let val = resolve_path_or_literal(state, val)?;
    let arr = val.as_array().ok_or_else(|| TcbError::InvalidType {
        expected: "array",
        actual: json_type_name(&val),
        context: "instructions".to_string(),
    })?;

    let mut result = Vec::new();
    for item in arr {
        match item {
            JsonValue::String(s) if s.starts_with("__") => {
                let mut resolved = resolve_path(state, s).cloned().ok_or_else(|| {
                    TcbError::PathResolutionFailed {
                        path: s.to_string(),
                        reason: "path not found".to_string(),
                    }
                })?;
                // mem::take: whole-value move out via &mut borrow (partial move-out
                // is rejected for types with Drop impl, e.g. cfg(kani) no-op Drop).
                if let JsonValue::Array(inner) = &mut resolved {
                    result.extend(core::mem::take(inner));
                } else {
                    result.push(resolved);
                }
            }
            _ => result.push(item.clone()),
        }
    }
    Ok(result)
}

/// push 元指令：推指令到队列前端
///
/// 空指令列表视为 no-op，不返回错误。
fn exec_push(instr: &JsonValue, mut state: JsonValue) -> Result<JsonValue, TcbError> {
    let params = instr.get("params").ok_or(TcbError::MissingField {
        field: "params".to_string(),
    })?;

    let instructions = resolve_instructions_list(&state, params.get("instructions"))?;

    // 空指令列表 = no-op
    if instructions.is_empty() {
        return Ok(state);
    }

    // 获取当前队列切片
    let queue_slice = resolve_path_mut(&mut state, "__exec__.queue")
        .and_then(|v| v.as_array_mut())
        .ok_or_else(|| TcbError::InvalidState {
            reason: "__exec__.queue is missing or not an array".to_string(),
        })?;

    // 构建新队列：新指令在前，旧队列在后
    let mut new_queue = Vec::with_capacity(instructions.len() + queue_slice.len());
    new_queue.extend(instructions);
    new_queue.extend_from_slice(queue_slice);

    // 替换回 state
    let queue =
        resolve_path_mut(&mut state, "__exec__.queue").ok_or_else(|| TcbError::InvalidState {
            reason: "__exec__.queue disappeared".to_string(),
        })?;

    *queue = JsonValue::Array(new_queue);

    Ok(state)
}

/// branch 元指令：条件执行子指令列表
///
/// 如果子指令返回 `IoRequired`，立即传播信号，不继续执行后续子指令。
/// 子指令递归共享外层执行预算（终止性宽度防线）。
fn exec_branch(
    instr: &JsonValue,
    mut state: JsonValue,
    depth: usize,
    budget: &mut usize,
    hit_out: &mut bool,
    domain_attr_out: &mut Option<DomainAttribution>,
) -> Result<MetaInstructionResult, TcbError> {
    if depth >= MAX_BRANCH_DEPTH {
        return Err(TcbError::NestingTooDeep {
            limit: MAX_BRANCH_DEPTH,
        });
    }

    let params = instr.get("params").ok_or(TcbError::MissingField {
        field: "params".to_string(),
    })?;

    let domain = resolve_path_or_literal(&state, params.get("domain"))?;
    // 域结构错误（未知类型/缺字段/超深/结构错误）显式报错，不在 TCB 层静默求值；
    // on_missing=error 声明的 Missing 同样显式报错（MissingRejected，专项-20261001）
    let outcome = evaluate_domain_outcome(&domain, &state)?;

    // R1 归因不回灌执行：走向仅由投影 bool 决定，三态归因（含声明值）仅作审计透传
    let branch_key = if outcome.to_bool() {
        "on_true"
    } else {
        "on_false"
    };
    *domain_attr_out = Some(DomainAttribution {
        outcome,
        on_missing: declared_on_missing(&domain),
    });

    let branch_instrs = params.get(branch_key).and_then(|v| v.as_array());

    if let Some(instrs) = branch_instrs {
        // 结构命中：所选分支存在且非空即命中；子指令命中不细记
        // （命中粒度 = 顶层规则级）。空数组/缺失分支 = 无效果路径，不命中。
        if !instrs.is_empty() {
            *hit_out = true;
        }
        for sub_instr in instrs {
            let mut sub_hit = false;
            // 子指令的域归因不细记（同命中粒度：顶层规则级），落本地变量丢弃
            let mut sub_attr = None;
            let result = execute_meta_instruction_budgeted(
                sub_instr,
                state,
                depth + 1,
                budget,
                &mut sub_hit,
                &mut sub_attr,
            )?;
            match result {
                MetaInstructionResult::State(new_state) => state = new_state,
                io_required @ MetaInstructionResult::IoRequired { .. } => return Ok(io_required),
                // enforce 信号传播即停（回归验证）：branch 子指令内的 enforce
                // 命中同样中断整个转换（半成品纪律与顶层一致）
                halted @ MetaInstructionResult::Halted { .. } => return Ok(halted),
            }
        }
    }

    Ok(MetaInstructionResult::State(state))
}

/// `enforce` 元指令：元规则强制原语（回归验证，纯机制不存策略）
///
/// # 参数（schema 门禁同口径：domain/reason 必填）
/// - `domain`：域结构（7 基础域类型，与 branch 的 domain 同构；支持
///   `__` 路径引用动态域，与 branch 范式一致）
/// - `reason`：违规说明字符串——命中时随 Halted 信号透传，供反应器
///   系统独占发射的 Violation 事实审计回显（必填：无 reason 的拦截
///   不可审计，不允许静默强制）
///
/// # 语义
/// - domain 求值 `Ok(true)` → `Halted { reason }`：状态转换层立即中断
///   剩余 transform，违规指令被拒（此前状态修改随半成品一并丢弃）；
/// - domain 求值 `Ok(false)` → `State(state)` noop 继续（不命中）；
/// - domain 结构错误（未知类型/缺字段/超深）→ 显式 `TcbError`
///   （fail-fast，与 branch 同语义：规则结构错误不允许静默求值）。
///
/// # 层级边界（引擎无层级概念）
/// 本原语不感知 tier——"enforce 仅允许出现在 L2 元规则文件"由 server
/// 侧装载门禁强制（见 evorule-server tier gate / schema 门禁）。
fn exec_enforce(
    instr: &JsonValue,
    state: JsonValue,
    domain_attr_out: &mut Option<DomainAttribution>,
) -> Result<MetaInstructionResult, TcbError> {
    let params = instr.get("params").ok_or(TcbError::MissingField {
        field: "params".to_string(),
    })?;

    let domain = resolve_path_or_literal(&state, params.get("domain"))?;
    // 域结构错误（未知类型/缺字段/超深/结构错误）显式报错，不在 TCB 层静默求值；
    // on_missing=error 声明的 Missing 同样显式报错（MissingRejected，专项-20261001）
    let outcome = evaluate_domain_outcome(&domain, &state)?;

    // R1 归因不回灌执行：命中与否仅由投影 bool 决定，三态归因（含声明值）仅作审计透传
    // （Unsat/Missing 不命中也要留归因——「为什么没拦住」的审计答案）
    let domain_satisfied = outcome.to_bool();
    *domain_attr_out = Some(DomainAttribution {
        outcome,
        on_missing: declared_on_missing(&domain),
    });

    if !domain_satisfied {
        return Ok(MetaInstructionResult::State(state));
    }

    let reason = params
        .get("reason")
        .and_then(|v| v.as_str())
        .ok_or(TcbError::MissingField {
            field: "reason".to_string(),
        })?;

    Ok(MetaInstructionResult::Halted {
        reason: reason.to_string(),
    })
}

/// `io_request` 元指令：产生 I/O 请求信号（不修改状态）
///
/// # 参数可选性（显式声明）
///
/// - 键名**不带** `?` 后缀（如 `"messages"`）：必选参数。路径引用解析失败
///   立即报错 `PathResolutionFailed`——拼写错误的路径在根因处暴露，
///   不再静默吞掉。
/// - 键名**带** `?` 后缀（如 `"tools?"`）：可选参数。路径引用解析失败时
///   跳过该参数（业务性缺省），请求参数使用去掉 `?` 的键名（`"tools"`）。
///
/// 「可选」必须显式声明而非由解析失败隐式推断（审计决策）：
/// 静默跳过会把拼写错误伪装成「参数未提供」，错误在远离根因的
/// 上游（外部系统行为异常）才暴露。
fn exec_io_request(instr: &JsonValue, state: JsonValue) -> Result<MetaInstructionResult, TcbError> {
    let params = instr.get("params").ok_or(TcbError::MissingField {
        field: "params".to_string(),
    })?;

    let io_type = params
        .get("io_type")
        .and_then(|v| v.as_str())
        .ok_or(TcbError::MissingField {
            field: "io_type".to_string(),
        })?
        .to_string();

    // 构造请求参数：解析所有路径引用
    // 可选参数必须以 '?' 后缀显式声明；必选参数解析失败立即报错
    let mut request_params = ObjectMap::new();
    if let Some(obj) = params.as_object() {
        for (key, value) in obj.iter() {
            if key == "io_type" {
                continue;
            }
            let (param_name, optional) = match key.strip_suffix('?') {
                Some(stripped) => (stripped, true),
                None => (key.as_str(), false),
            };
            match resolve_path_or_literal(&state, Some(value)) {
                Ok(resolved) => {
                    request_params.insert(param_name.to_string(), resolved);
                }
                // 显式声明的可选参数：路径不存在时跳过（业务性缺省）
                Err(TcbError::PathResolutionFailed { .. }) if optional => {}
                // 必选参数路径解析失败：显式报错，不静默吞掉拼写错误
                Err(e) => return Err(e),
            }
        }
    }

    Ok(MetaInstructionResult::IoRequired {
        io_type,
        params: JsonValue::Object(request_params),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    #![allow(clippy::panic)]
    #![allow(clippy::indexing_slicing)]

    use super::*;
    use crate::domain::DomainOutcome;
    use crate::value::{JsonValue, ObjectMap};
    use alloc::string::ToString;
    use alloc::vec;
    use alloc::vec::Vec;

    // ===== 辅助函数 =====

    fn make_exec_state(
        instruction_type: &str,
        payload: JsonValue,
        queue: Vec<JsonValue>,
    ) -> JsonValue {
        let mut exec = ObjectMap::new();
        exec.insert(
            "instruction".to_string(),
            make_instruction(instruction_type, &[]),
        );
        exec.insert("payload".to_string(), payload);
        exec.insert("queue".to_string(), JsonValue::Array(queue));
        let mut root = ObjectMap::new();
        root.insert("__exec__".to_string(), JsonValue::Object(exec));
        JsonValue::Object(root)
    }

    fn make_exec_state_with_instruction(
        instruction: JsonValue,
        payload: JsonValue,
        queue: Vec<JsonValue>,
    ) -> JsonValue {
        let mut exec = ObjectMap::new();
        exec.insert("instruction".to_string(), instruction);
        exec.insert("payload".to_string(), payload);
        exec.insert("queue".to_string(), JsonValue::Array(queue));
        let mut root = ObjectMap::new();
        root.insert("__exec__".to_string(), JsonValue::Object(exec));
        JsonValue::Object(root)
    }

    fn make_payload(x: i64) -> JsonValue {
        let mut map = ObjectMap::new();
        map.insert("x".to_string(), JsonValue::Integer(x));
        JsonValue::Object(map)
    }

    fn make_instruction(instr_type: &str, params: &[(&str, JsonValue)]) -> JsonValue {
        JsonValue::object_from_pairs(&[
            ("type", JsonValue::string(instr_type)),
            ("params", JsonValue::object_from_pairs(params)),
        ])
    }

    fn make_instruction_simple(instr_type: &str, value: i64) -> JsonValue {
        make_instruction(instr_type, &[("value", JsonValue::Integer(value))])
    }

    // ===== set 测试 =====

    #[test]
    fn test_set_add() {
        let state = make_exec_state("increment", make_payload(5), vec![]);
        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("attr", JsonValue::string("x")),
                    ("operation", JsonValue::string("add")),
                    ("value", JsonValue::Integer(3)),
                ]),
            ),
        ]);

        let result = exec_set(&instr, state).unwrap();
        let payload = resolve_path(&result, "__exec__.payload").unwrap();
        assert_eq!(payload.get("x"), Some(&JsonValue::Integer(8)));
    }

    #[test]
    fn test_set_sub() {
        let state = make_exec_state("decrement", make_payload(10), vec![]);
        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("attr", JsonValue::string("x")),
                    ("operation", JsonValue::string("sub")),
                    ("value", JsonValue::Integer(3)),
                ]),
            ),
        ]);

        let result = exec_set(&instr, state).unwrap();
        let payload = resolve_path(&result, "__exec__.payload").unwrap();
        assert_eq!(payload.get("x"), Some(&JsonValue::Integer(7)));
    }

    #[test]
    fn test_set_operation_set_string_value() {
        let state = make_exec_state("set", make_payload(0), vec![]);
        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("attr", JsonValue::string("name")),
                    ("operation", JsonValue::string("set")),
                    ("value", JsonValue::string("hello")),
                ]),
            ),
        ]);

        let result = exec_set(&instr, state).unwrap();
        let value = resolve_path(&result, "__exec__.payload.name").unwrap();
        assert_eq!(value, &JsonValue::string("hello"));
    }

    // ===== 写侧 attr payload. 前缀守卫测试（方案 a）=====

    /// 错误形态：attr 带 payload. 前缀 → 显式报错（拒绝双重嵌套静默写歪）
    #[test]
    fn test_set_payload_prefix_attr_rejected() {
        let state = make_exec_state("set", make_payload(0), vec![]);
        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("attr", JsonValue::string("payload.meta_guard.cleared")),
                    ("operation", JsonValue::string("set")),
                    ("value", JsonValue::Bool(true)),
                ]),
            ),
        ]);

        let err = exec_set(&instr, state).unwrap_err();
        match err {
            TcbError::PathResolutionFailed { path, reason } => {
                assert_eq!(path, "payload.meta_guard.cleared");
                assert!(
                    reason.contains("payload.") && reason.contains("双重嵌套"),
                    "错误原因应含双重嵌套指引: {reason}"
                );
            }
            other => panic!("expected PathResolutionFailed, got: {other:?}"),
        }
    }

    /// 状态引用间接产生的 payload. 前缀同样被拒（守卫作用于解析后的最终 attr）
    #[test]
    fn test_set_state_ref_attr_resolving_to_payload_prefix_rejected() {
        let mut params = ObjectMap::new();
        params.insert("target".to_string(), JsonValue::string("payload.x"));
        let instruction = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            ("params", JsonValue::Object(params)),
        ]);
        let state = make_exec_state_with_instruction(instruction, make_payload(0), vec![]);
        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "attr",
                        JsonValue::string("__exec__.instruction.params.target"),
                    ),
                    ("operation", JsonValue::string("set")),
                    ("value", JsonValue::Integer(1)),
                ]),
            ),
        ]);

        let err = exec_set(&instr, state).unwrap_err();
        assert!(
            matches!(err, TcbError::PathResolutionFailed { ref path, .. } if path == "payload.x"),
            "运行时解析出 payload. 前缀 attr 应被拒: {err:?}"
        );
    }

    /// 显式全形式 __exec__.payload.x 放行（剥离前缀后写 payload.x）
    #[test]
    fn test_set_exec_payload_full_form_ok() {
        let state = make_exec_state("set", make_payload(0), vec![]);
        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("attr", JsonValue::string("__exec__.payload.x")),
                    ("operation", JsonValue::string("set")),
                    ("value", JsonValue::Integer(9)),
                ]),
            ),
        ]);

        let result = exec_set(&instr, state).unwrap();
        let payload = resolve_path(&result, "__exec__.payload").unwrap();
        assert_eq!(payload.get("x"), Some(&JsonValue::Integer(9)));
    }

    /// 显式全形式 __exec__.payload.payload.deep 是写 payload.payload.* 的唯一合法途径
    #[test]
    fn test_set_exec_payload_nested_full_form_ok() {
        let state = make_exec_state("set", make_payload(0), vec![]);
        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("attr", JsonValue::string("__exec__.payload.payload.deep")),
                    ("operation", JsonValue::string("set")),
                    ("value", JsonValue::Integer(1)),
                ]),
            ),
        ]);

        let result = exec_set(&instr, state).unwrap();
        let nested = resolve_path(&result, "__exec__.payload.payload.deep").unwrap();
        assert_eq!(nested, &JsonValue::Integer(1));
    }

    /// 裸简写嵌套路径（种子元规则正确形态）不受守卫影响
    #[test]
    fn test_set_bare_shorthand_nested_path_ok() {
        let state = make_exec_state("set", make_payload(0), vec![]);
        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("attr", JsonValue::string("meta_guard.cleared_for_move")),
                    ("operation", JsonValue::string("set")),
                    ("value", JsonValue::Bool(true)),
                ]),
            ),
        ]);

        let result = exec_set(&instr, state).unwrap();
        let flag = resolve_path(&result, "__exec__.payload.meta_guard.cleared_for_move").unwrap();
        assert_eq!(flag, &JsonValue::Bool(true));
    }

    #[test]
    fn test_set_type_safety() {
        let state = make_exec_state("set", make_payload(5), vec![]);
        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("attr", JsonValue::string("x")),
                    ("operation", JsonValue::string("add")),
                    ("value", JsonValue::string("hello")),
                ]),
            ),
        ]);

        let result = exec_set(&instr, state);
        assert!(matches!(result, Err(TcbError::InvalidType { .. })));
    }

    #[test]
    fn test_set_add_overflow() {
        let state = make_exec_state("set", make_payload(i64::MAX), vec![]);
        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("attr", JsonValue::string("x")),
                    ("operation", JsonValue::string("add")),
                    ("value", JsonValue::Integer(1)),
                ]),
            ),
        ]);

        let result = exec_set(&instr, state);
        assert!(matches!(result, Err(TcbError::IntegerOverflow { .. })));
    }

    #[test]
    fn test_set_sub_overflow() {
        let state = make_exec_state("set", make_payload(i64::MIN), vec![]);
        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("attr", JsonValue::string("x")),
                    ("operation", JsonValue::string("sub")),
                    ("value", JsonValue::Integer(1)),
                ]),
            ),
        ]);

        let result = exec_set(&instr, state);
        assert!(matches!(result, Err(TcbError::IntegerOverflow { .. })));
    }

    #[test]
    fn test_set_nested_attr_path() {
        let mut inner = ObjectMap::new();
        inner.insert("b".to_string(), JsonValue::Integer(1));
        let mut payload = ObjectMap::new();
        payload.insert("a".to_string(), JsonValue::Object(inner));
        let payload = JsonValue::Object(payload);

        let state = make_exec_state("set", payload, vec![]);
        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("attr", JsonValue::string("a.b")),
                    ("operation", JsonValue::string("add")),
                    ("value", JsonValue::Integer(10)),
                ]),
            ),
        ]);

        let result = exec_set(&instr, state).unwrap();
        let value = resolve_path(&result, "__exec__.payload.a.b").unwrap();
        assert_eq!(value, &JsonValue::Integer(11));
    }

    #[test]
    fn test_set_nested_attr_create_field() {
        let mut inner = ObjectMap::new();
        inner.insert("existing".to_string(), JsonValue::Integer(100));
        let mut payload = ObjectMap::new();
        payload.insert("a".to_string(), JsonValue::Object(inner));
        let payload = JsonValue::Object(payload);

        let state = make_exec_state("set", payload, vec![]);
        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("attr", JsonValue::string("a.new_field")),
                    ("operation", JsonValue::string("set")),
                    ("value", JsonValue::Integer(42)),
                ]),
            ),
        ]);

        let result = exec_set(&instr, state).unwrap();
        let value = resolve_path(&result, "__exec__.payload.a.new_field").unwrap();
        assert_eq!(value, &JsonValue::Integer(42));
    }

    // ── M2：attr 数组索引写入 ────────────────────────────────────────────

    fn make_payload_with_items() -> JsonValue {
        // payload.items = [ { done: false }, { done: false } ]
        let item0 = JsonValue::object_from_pairs(&[("done", JsonValue::Bool(false))]);
        let item1 = JsonValue::object_from_pairs(&[("done", JsonValue::Bool(false))]);
        JsonValue::object_from_pairs(&[("items", JsonValue::array(vec![item0, item1]))])
    }

    fn set_instr(attr: &str, op: &str, value: JsonValue) -> JsonValue {
        JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("attr", JsonValue::string(attr)),
                    ("operation", JsonValue::string(op)),
                    ("value", value),
                ]),
            ),
        ])
    }

    #[test]
    fn test_set_array_index_field_write() {
        // items[0].done：既有数组元素的字段写入（domain 读取同款路径语法）
        let state = make_exec_state("set", make_payload_with_items(), vec![]);
        let result = exec_set(
            &set_instr("items[0].done", "set", JsonValue::Bool(true)),
            state,
        )
        .unwrap();
        assert_eq!(
            resolve_path(&result, "__exec__.payload.items[0].done"),
            Some(&JsonValue::Bool(true))
        );
        // 其余元素不受影响
        assert_eq!(
            resolve_path(&result, "__exec__.payload.items[1].done"),
            Some(&JsonValue::Bool(false))
        );
    }

    #[test]
    fn test_set_array_index_element_write() {
        // items[1]：末段索引，替换整个数组元素
        let state = make_exec_state("set", make_payload_with_items(), vec![]);
        let replacement = JsonValue::object_from_pairs(&[("done", JsonValue::Bool(true))]);
        let result = exec_set(&set_instr("items[1]", "set", replacement), state).unwrap();
        assert_eq!(
            resolve_path(&result, "__exec__.payload.items[1].done"),
            Some(&JsonValue::Bool(true))
        );
        assert_eq!(
            resolve_path(&result, "__exec__.payload.items[0].done"),
            Some(&JsonValue::Bool(false))
        );
    }

    #[test]
    fn test_set_array_index_add_operation() {
        // items[0].count = 5 + 3：索引槽位上的算术运算
        let item0 = JsonValue::object_from_pairs(&[("count", JsonValue::Integer(5))]);
        let payload = JsonValue::object_from_pairs(&[("items", JsonValue::array(vec![item0]))]);
        let state = make_exec_state("set", payload, vec![]);
        let result = exec_set(
            &set_instr("items[0].count", "add", JsonValue::Integer(3)),
            state,
        )
        .unwrap();
        assert_eq!(
            resolve_path(&result, "__exec__.payload.items[0].count"),
            Some(&JsonValue::Integer(8))
        );
    }

    #[test]
    fn test_set_array_missing_errors_no_auto_create() {
        // 数组不存在 → 报错（数组长度无法从索引推断，禁止隐式创建）
        let payload = JsonValue::empty_object();
        let state = make_exec_state("set", payload, vec![]);
        let result = exec_set(
            &set_instr("items[0].done", "set", JsonValue::Bool(true)),
            state,
        );
        match result {
            Err(TcbError::PathResolutionFailed { path, reason }) => {
                assert!(path.contains("items[0].done"));
                assert!(
                    reason.contains("cannot auto-create arrays"),
                    "reason: {reason}"
                );
            }
            other => panic!("expected PathResolutionFailed, got {:?}", other),
        }
    }

    #[test]
    fn test_set_array_index_out_of_bounds_errors() {
        // 索引越界 → 报错（禁止稀疏数组与隐式追加）
        let state = make_exec_state("set", make_payload_with_items(), vec![]);
        let result = exec_set(
            &set_instr("items[2].done", "set", JsonValue::Bool(true)),
            state,
        );
        match result {
            Err(TcbError::PathResolutionFailed { path, reason }) => {
                assert!(path.contains("items[2].done"));
                assert!(reason.contains("out of bounds"), "reason: {reason}");
            }
            other => panic!("expected PathResolutionFailed, got {:?}", other),
        }
    }

    #[test]
    fn test_set_array_index_no_malformed_field_created() {
        // M2 回归核心：旧实现会把 "items[0]" 当字段名创建畸形键；
        // 新实现要么成功写入元素、要么显式报错，绝不创建 "items[0]" 字面键
        let state = make_exec_state("set", make_payload_with_items(), vec![]);
        let result = exec_set(
            &set_instr("items[0].done", "set", JsonValue::Bool(true)),
            state,
        )
        .unwrap();
        let items = resolve_path(&result, "__exec__.payload.items").unwrap();
        let arr = items.as_array().unwrap();
        assert_eq!(arr.len(), 2, "数组长度不变（无追加）");
        // 确认没有 "items[0]" 畸形键出现在 payload 顶层
        let payload_obj = resolve_path(&result, "__exec__.payload").unwrap();
        assert!(payload_obj.get("items[0]").is_none(), "不得创建畸形字段名");
        assert!(payload_obj.get("items[1]").is_none(), "不得创建畸形字段名");
    }

    #[test]
    fn test_set_array_index_on_non_array_errors() {
        // 字段存在但非数组 → 显式报错
        let payload = JsonValue::object_from_pairs(&[("items", JsonValue::Integer(42))]);
        let state = make_exec_state("set", payload, vec![]);
        let result = exec_set(&set_instr("items[0]", "set", JsonValue::Bool(true)), state);
        match result {
            Err(TcbError::PathResolutionFailed { path, reason }) => {
                assert!(path.contains("items[0]"));
                assert!(reason.contains("not an array"), "reason: {reason}");
            }
            other => panic!("expected PathResolutionFailed, got {:?}", other),
        }
    }

    #[test]
    fn test_set_arithmetic_null_field_treated_as_zero() {
        // L7 回归：null ≡ 已清除/不存在 ≡ 算术起点 0，
        // add/sub 不得对 null 字段报 InvalidType（与缺失字段行为对齐）
        let payload = JsonValue::object_from_pairs(&[("x", JsonValue::Null)]);
        let state = make_exec_state("set", payload, vec![]);

        let result = exec_set(&set_instr("x", "add", JsonValue::Integer(5)), state).unwrap();
        assert_eq!(
            resolve_path(&result, "__exec__.payload.x").unwrap(),
            &JsonValue::Integer(5)
        );
    }

    #[test]
    fn test_set_arithmetic_missing_field_treated_as_zero() {
        // 对照组：缺失字段 add 从 0 起算（既有行为，防止 L7 修改引入回归）
        let state = make_exec_state("set", JsonValue::empty_object(), vec![]);
        let result = exec_set(&set_instr("x", "sub", JsonValue::Integer(3)), state).unwrap();
        assert_eq!(
            resolve_path(&result, "__exec__.payload.x").unwrap(),
            &JsonValue::Integer(-3)
        );
    }

    #[test]
    fn test_set_attr_with_path_reference_from_instruction() {
        let instruction = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("increment")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("attr", JsonValue::string("x")),
                    ("delta", JsonValue::Integer(5)),
                ]),
            ),
        ]);
        let state = make_exec_state_with_instruction(instruction, make_payload(10), vec![]);

        let set_instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
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

        let result = exec_set(&set_instr, state).unwrap();
        let x = resolve_path(&result, "__exec__.payload.x").unwrap();
        assert_eq!(x, &JsonValue::Integer(15));
    }

    #[test]
    fn test_set_nested_attr_through_null_intermediate() {
        let mut payload = ObjectMap::new();
        let mut audit = ObjectMap::new();
        audit.insert("evolve_request".to_string(), JsonValue::Null);
        payload.insert("audit".to_string(), JsonValue::Object(audit));
        let state = make_exec_state_with_instruction(
            JsonValue::object_from_pairs(&[("type", JsonValue::string("evolution_scanner"))]),
            JsonValue::Object(payload),
            vec![],
        );

        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("attr", JsonValue::string("audit.evolve_request.reason")),
                    ("operation", JsonValue::string("set")),
                    ("value", JsonValue::string("test reason")),
                ]),
            ),
        ]);

        let result = exec_set(&instr, state).unwrap();
        let reason = resolve_path(&result, "__exec__.payload.audit.evolve_request.reason");
        assert_eq!(reason.and_then(|v| v.as_str()), Some("test reason"));
    }

    #[test]
    fn test_set_nested_attr_through_scalar_intermediate_errors() {
        let mut payload = ObjectMap::new();
        let mut audit = ObjectMap::new();
        audit.insert("evolve_request".to_string(), JsonValue::Integer(42));
        payload.insert("audit".to_string(), JsonValue::Object(audit));
        let state = make_exec_state_with_instruction(
            JsonValue::object_from_pairs(&[("type", JsonValue::string("evolution_scanner"))]),
            JsonValue::Object(payload),
            vec![],
        );

        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("attr", JsonValue::string("audit.evolve_request.reason")),
                    ("operation", JsonValue::string("set")),
                    ("value", JsonValue::string("test reason")),
                ]),
            ),
        ]);

        let result = exec_set(&instr, state);
        match result {
            Err(TcbError::PathResolutionFailed { path, reason }) => {
                assert!(path.contains("audit.evolve_request.reason"));
                assert!(reason.contains("integer"));
                assert!(reason.contains("object"));
            }
            other => panic!("expected PathResolutionFailed, got {:?}", other),
        }
    }

    #[test]
    fn test_set_missing_attr_field() {
        let state = make_exec_state("set", make_payload(0), vec![]);
        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("operation", JsonValue::string("set")),
                    ("value", JsonValue::Integer(1)),
                ]),
            ),
        ]);

        let result = exec_set(&instr, state);
        assert!(matches!(result, Err(TcbError::MissingField { field }) if field == "attr"));
    }

    #[test]
    fn test_set_missing_value_field() {
        let state = make_exec_state("set", make_payload(0), vec![]);
        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("attr", JsonValue::string("x")),
                    ("operation", JsonValue::string("set")),
                ]),
            ),
        ]);

        let result = exec_set(&instr, state);
        assert!(matches!(result, Err(TcbError::MissingField { field }) if field == "value"));
    }

    #[test]
    fn test_set_unknown_operation() {
        let state = make_exec_state("set", make_payload(10), vec![]);
        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("attr", JsonValue::string("x")),
                    ("operation", JsonValue::string("multiply")),
                    ("value", JsonValue::Integer(2)),
                ]),
            ),
        ]);

        let result = exec_set(&instr, state);
        assert!(
            matches!(result, Err(TcbError::UnknownOperation { operation }) if operation == "multiply")
        );
    }

    // ===== push 测试 =====

    #[test]
    fn test_push_basic() {
        let state = make_exec_state("sequence", make_payload(0), vec![]);
        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("push")),
            (
                "params",
                JsonValue::object_from_pairs(&[(
                    "instructions",
                    JsonValue::array(vec![JsonValue::object_from_pairs(&[
                        ("type", JsonValue::string("increment")),
                        (
                            "params",
                            JsonValue::object_from_pairs(&[
                                ("attr", JsonValue::string("x")),
                                ("delta", JsonValue::Integer(1)),
                            ]),
                        ),
                    ])]),
                )]),
            ),
        ]);

        let result = exec_push(&instr, state).unwrap();
        let queue = resolve_path(&result, "__exec__.queue").unwrap();
        let arr = queue.as_array().unwrap();
        assert_eq!(arr.len(), 1);
    }

    #[test]
    fn test_push_prepends_to_existing_queue() {
        let existing = make_instruction_simple("old_op", 0);
        let state = make_exec_state("sequence", make_payload(0), vec![existing]);
        let new_instr = make_instruction_simple("new_op", 1);
        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("push")),
            (
                "params",
                JsonValue::object_from_pairs(&[(
                    "instructions",
                    JsonValue::array(vec![new_instr]),
                )]),
            ),
        ]);

        let result = exec_push(&instr, state).unwrap();
        let queue = resolve_path(&result, "__exec__.queue").unwrap();
        let arr = queue.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0].get("type").and_then(|v| v.as_str()), Some("new_op"));
        assert_eq!(arr[1].get("type").and_then(|v| v.as_str()), Some("old_op"));
    }

    #[test]
    fn test_push_empty_list_is_noop() {
        let existing = make_instruction_simple("old_op", 0);
        let state = make_exec_state("sequence", make_payload(0), vec![existing]);
        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("push")),
            (
                "params",
                JsonValue::object_from_pairs(&[("instructions", JsonValue::array(vec![]))]),
            ),
        ]);

        let result = exec_push(&instr, state).unwrap();
        let queue = resolve_path(&result, "__exec__.queue").unwrap();
        let arr = queue.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].get("type").and_then(|v| v.as_str()), Some("old_op"));
    }

    #[test]
    fn test_push_with_path_reference_elements() {
        let then_instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("attr", JsonValue::string("x")),
                    ("operation", JsonValue::string("set")),
                    ("value", JsonValue::Integer(42)),
                ]),
            ),
        ]);
        let instruction = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("conditional")),
            (
                "params",
                JsonValue::object_from_pairs(&[("then", then_instr)]),
            ),
        ]);
        let state = make_exec_state_with_instruction(instruction, make_payload(0), vec![]);

        let push_instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("push")),
            (
                "params",
                JsonValue::object_from_pairs(&[(
                    "instructions",
                    JsonValue::array(vec![JsonValue::string("__exec__.instruction.params.then")]),
                )]),
            ),
        ]);

        let result = exec_push(&push_instr, state).unwrap();
        let queue = resolve_path(&result, "__exec__.queue").unwrap();
        let arr = queue.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].get("type").and_then(|v| v.as_str()), Some("set"));
    }

    #[test]
    fn test_push_with_array_body_flattened() {
        let body_array = JsonValue::array(vec![
            JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("increment")),
                (
                    "params",
                    JsonValue::object_from_pairs(&[
                        ("attr", JsonValue::string("x")),
                        ("delta", JsonValue::Integer(1)),
                    ]),
                ),
            ]),
            JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("set")),
                (
                    "params",
                    JsonValue::object_from_pairs(&[
                        ("attr", JsonValue::string("y")),
                        ("operation", JsonValue::string("set")),
                        ("value", JsonValue::Integer(42)),
                    ]),
                ),
            ]),
        ]);
        let while_instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("while_loop")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("body", body_array),
                    (
                        "condition",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("lt")),
                            ("path", JsonValue::string("__exec__.payload.x")),
                            ("value", JsonValue::Integer(10)),
                        ]),
                    ),
                ]),
            ),
        ]);
        let state = make_exec_state_with_instruction(while_instr, make_payload(0), vec![]);

        let push_instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("push")),
            (
                "params",
                JsonValue::object_from_pairs(&[(
                    "instructions",
                    JsonValue::array(vec![
                        JsonValue::string("__exec__.instruction.params.body"),
                        JsonValue::string("__exec__.instruction"),
                    ]),
                )]),
            ),
        ]);

        let result = exec_push(&push_instr, state).unwrap();
        let queue = resolve_path(&result, "__exec__.queue").unwrap();
        let arr = queue.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(
            arr[0].get("type").and_then(|v| v.as_str()),
            Some("increment")
        );
        assert_eq!(arr[1].get("type").and_then(|v| v.as_str()), Some("set"));
        assert_eq!(
            arr[2].get("type").and_then(|v| v.as_str()),
            Some("while_loop")
        );
    }

    // ===== branch 测试 =====

    #[test]
    fn test_branch_true() {
        let state = make_exec_state("branch", make_payload(10), vec![]);
        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("eq")),
                            ("path", JsonValue::string("__exec__.payload.x")),
                            ("value", JsonValue::Integer(10)),
                        ]),
                    ),
                    (
                        "on_true",
                        JsonValue::array(vec![JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("set")),
                            (
                                "params",
                                JsonValue::object_from_pairs(&[
                                    ("attr", JsonValue::string("x")),
                                    ("operation", JsonValue::string("set")),
                                    ("value", JsonValue::Integer(1)),
                                ]),
                            ),
                        ])]),
                    ),
                ]),
            ),
        ]);

        let result = execute_meta_instruction(&instr, state, 0).unwrap();
        match result {
            MetaInstructionResult::State(new_state) => {
                let x = resolve_path(&new_state, "__exec__.payload.x").unwrap();
                assert_eq!(x, &JsonValue::Integer(1));
            }
            _ => panic!("expected State"),
        }
    }

    #[test]
    fn test_branch_false() {
        let state = make_exec_state("branch", make_payload(5), vec![]);
        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("eq")),
                            ("path", JsonValue::string("__exec__.payload.x")),
                            ("value", JsonValue::Integer(10)),
                        ]),
                    ),
                    (
                        "on_false",
                        JsonValue::array(vec![JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("set")),
                            (
                                "params",
                                JsonValue::object_from_pairs(&[
                                    ("attr", JsonValue::string("x")),
                                    ("operation", JsonValue::string("set")),
                                    ("value", JsonValue::Integer(2)),
                                ]),
                            ),
                        ])]),
                    ),
                ]),
            ),
        ]);

        let result = execute_meta_instruction(&instr, state, 0).unwrap();
        match result {
            MetaInstructionResult::State(new_state) => {
                let x = resolve_path(&new_state, "__exec__.payload.x").unwrap();
                assert_eq!(x, &JsonValue::Integer(2));
            }
            _ => panic!("expected State"),
        }
    }

    #[test]
    fn test_branch_with_both_branches() {
        let state = make_exec_state("branch", make_payload(5), vec![]);
        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("eq")),
                            ("path", JsonValue::string("__exec__.payload.x")),
                            ("value", JsonValue::Integer(10)),
                        ]),
                    ),
                    (
                        "on_true",
                        JsonValue::array(vec![JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("set")),
                            (
                                "params",
                                JsonValue::object_from_pairs(&[
                                    ("attr", JsonValue::string("x")),
                                    ("operation", JsonValue::string("set")),
                                    ("value", JsonValue::Integer(1)),
                                ]),
                            ),
                        ])]),
                    ),
                    (
                        "on_false",
                        JsonValue::array(vec![JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("set")),
                            (
                                "params",
                                JsonValue::object_from_pairs(&[
                                    ("attr", JsonValue::string("x")),
                                    ("operation", JsonValue::string("set")),
                                    ("value", JsonValue::Integer(2)),
                                ]),
                            ),
                        ])]),
                    ),
                ]),
            ),
        ]);

        let result = execute_meta_instruction(&instr, state, 0).unwrap();
        match result {
            MetaInstructionResult::State(new_state) => {
                let x = resolve_path(&new_state, "__exec__.payload.x").unwrap();
                assert_eq!(x, &JsonValue::Integer(2));
            }
            _ => panic!("expected State"),
        }
    }

    #[test]
    fn test_branch_no_matching_branch_returns_state_unchanged() {
        let state = make_exec_state("branch", make_payload(5), vec![]);
        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("eq")),
                            ("path", JsonValue::string("__exec__.payload.x")),
                            ("value", JsonValue::Integer(10)),
                        ]),
                    ),
                    (
                        "on_true",
                        JsonValue::array(vec![JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("set")),
                            (
                                "params",
                                JsonValue::object_from_pairs(&[
                                    ("attr", JsonValue::string("x")),
                                    ("operation", JsonValue::string("set")),
                                    ("value", JsonValue::Integer(1)),
                                ]),
                            ),
                        ])]),
                    ),
                ]),
            ),
        ]);

        let result = execute_meta_instruction(&instr, state.clone(), 0).unwrap();
        match result {
            MetaInstructionResult::State(new_state) => {
                let x = resolve_path(&new_state, "__exec__.payload.x").unwrap();
                assert_eq!(x, &JsonValue::Integer(5));
            }
            _ => panic!("expected State"),
        }
    }

    #[test]
    fn test_branch_depth_limit() {
        let state = make_exec_state("branch", make_payload(0), vec![]);
        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("eq")),
                            ("path", JsonValue::string("__exec__.payload.x")),
                            ("value", JsonValue::Integer(0)),
                        ]),
                    ),
                    ("on_true", JsonValue::array(vec![])),
                ]),
            ),
        ]);

        // depth = 63 (MAX-1) 应该可以执行（预算充足）
        let mut budget = MAX_TOTAL_META_INSTRUCTIONS;
        let mut hit = false;
        let result = exec_branch(&instr, state.clone(), 63, &mut budget, &mut hit, &mut None);
        assert!(result.is_ok());

        // depth = 64 (MAX) 应该返回 NestingTooDeep（深度检查先于预算扣减）
        let mut budget2 = MAX_TOTAL_META_INSTRUCTIONS;
        let mut hit2 = false;
        let result = exec_branch(
            &instr,
            state,
            MAX_BRANCH_DEPTH,
            &mut budget2,
            &mut hit2,
            &mut None,
        );
        assert!(matches!(result, Err(TcbError::NestingTooDeep { .. })));
    }

    // ===== M6 元指令执行总数预算测试 =====

    #[test]
    fn test_budget_zero_returns_error() {
        let state = make_exec_state("set", make_payload(0), vec![]);
        let instr = make_instruction(
            "set",
            &[
                ("attr", JsonValue::string("x")),
                ("operation", JsonValue::string("set")),
                ("value", JsonValue::Integer(1)),
            ],
        );

        let mut budget = 0usize;
        let mut hit = false;
        let result =
            execute_meta_instruction_budgeted(&instr, state, 0, &mut budget, &mut hit, &mut None);
        assert!(
            matches!(result, Err(TcbError::TooManyExecutedInstructions { limit }) if limit == MAX_TOTAL_META_INSTRUCTIONS)
        );
    }

    #[test]
    fn test_budget_single_instruction_consumes_one() {
        let state = make_exec_state("set", make_payload(0), vec![]);
        let instr = make_instruction(
            "set",
            &[
                ("attr", JsonValue::string("x")),
                ("operation", JsonValue::string("set")),
                ("value", JsonValue::Integer(7)),
            ],
        );

        let mut budget = 1usize;
        let mut hit = false;
        let result =
            execute_meta_instruction_budgeted(&instr, state, 0, &mut budget, &mut hit, &mut None)
                .unwrap();
        assert!(matches!(result, MetaInstructionResult::State(_)));
        // 单条指令恰好耗尽预算
        assert_eq!(budget, 0);
    }

    #[test]
    fn test_branch_sub_instructions_share_budget() {
        // branch(1) + 2 个子指令 = 共 3 个预算单位
        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("eq")),
                            ("path", JsonValue::string("__exec__.payload.x")),
                            ("value", JsonValue::Integer(0)),
                        ]),
                    ),
                    (
                        "on_true",
                        JsonValue::array(vec![
                            make_instruction(
                                "set",
                                &[
                                    ("attr", JsonValue::string("x")),
                                    ("operation", JsonValue::string("set")),
                                    ("value", JsonValue::Integer(1)),
                                ],
                            ),
                            make_instruction(
                                "set",
                                &[
                                    ("attr", JsonValue::string("x")),
                                    ("operation", JsonValue::string("set")),
                                    ("value", JsonValue::Integer(2)),
                                ],
                            ),
                        ]),
                    ),
                ]),
            ),
        ]);

        // 预算 = 2（不足 3）：应报错，不允许部分执行后静默成功
        let state = make_exec_state("branch", make_payload(0), vec![]);
        let mut budget = 2usize;
        let mut hit = false;
        let result =
            execute_meta_instruction_budgeted(&instr, state, 0, &mut budget, &mut hit, &mut None);
        assert!(matches!(
            result,
            Err(TcbError::TooManyExecutedInstructions { .. })
        ));

        // 预算 = 3（恰好）：应成功
        let state = make_exec_state("branch", make_payload(0), vec![]);
        let mut budget = 3usize;
        let mut hit2 = false;
        let result =
            execute_meta_instruction_budgeted(&instr, state, 0, &mut budget, &mut hit2, &mut None)
                .unwrap();
        match result {
            MetaInstructionResult::State(new_state) => {
                let x = resolve_path(&new_state, "__exec__.payload.x").unwrap();
                assert_eq!(x, &JsonValue::Integer(2));
            }
            _ => panic!("expected State"),
        }
        assert_eq!(budget, 0);
    }

    #[test]
    fn test_budget_error_reports_limit_constant() {
        let state = make_exec_state("set", make_payload(0), vec![]);
        let instr = make_instruction(
            "set",
            &[
                ("attr", JsonValue::string("x")),
                ("operation", JsonValue::string("set")),
                ("value", JsonValue::Integer(1)),
            ],
        );

        let mut budget = 0usize;
        let mut hit = false;
        let err =
            execute_meta_instruction_budgeted(&instr, state, 0, &mut budget, &mut hit, &mut None)
                .unwrap_err();
        match err {
            TcbError::TooManyExecutedInstructions { limit } => {
                assert_eq!(limit, MAX_TOTAL_META_INSTRUCTIONS);
                assert_eq!(limit, 1024);
            }
            other => panic!("unexpected error: {:?}", other),
        }
    }

    // ===== io_request 测试 =====

    #[test]
    fn test_io_request_basic() {
        let state = make_exec_state("call_external", make_payload(0), vec![]);
        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("io_request")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("io_type", JsonValue::string("call_external")),
                    ("prompt", JsonValue::string("Hello")),
                ]),
            ),
        ]);

        let result = exec_io_request(&instr, state).unwrap();
        match result {
            MetaInstructionResult::IoRequired { io_type, params } => {
                assert_eq!(io_type, "call_external");
                assert_eq!(params.get("prompt").and_then(|v| v.as_str()), Some("Hello"));
                assert!(params.get("io_type").is_none());
            }
            _ => panic!("expected IoRequired"),
        }
    }

    #[test]
    fn test_io_request_path_resolution() {
        let instruction = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("call_external")),
            (
                "params",
                JsonValue::object_from_pairs(&[("prompt", JsonValue::string("Summarize this"))]),
            ),
        ]);
        let state = make_exec_state_with_instruction(instruction, make_payload(0), vec![]);

        let io_instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("io_request")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("io_type", JsonValue::string("call_external")),
                    (
                        "prompt",
                        JsonValue::string("__exec__.instruction.params.prompt"),
                    ),
                ]),
            ),
        ]);

        let result = exec_io_request(&io_instr, state).unwrap();
        match result {
            MetaInstructionResult::IoRequired { io_type, params } => {
                assert_eq!(io_type, "call_external");
                assert_eq!(
                    params.get("prompt").and_then(|v| v.as_str()),
                    Some("Summarize this")
                );
            }
            _ => panic!("expected IoRequired"),
        }
    }

    #[test]
    fn test_io_request_does_not_modify_state() {
        let state = make_exec_state("call_external", make_payload(42), vec![]);
        let state_snapshot = state.clone();

        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("io_request")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("io_type", JsonValue::string("call_external")),
                    ("prompt", JsonValue::string("test")),
                ]),
            ),
        ]);

        let result = exec_io_request(&instr, state).unwrap();
        assert!(matches!(result, MetaInstructionResult::IoRequired { .. }));
        assert_eq!(state_snapshot, state_snapshot);
    }

    #[test]
    fn test_io_request_optional_params_skipped_when_path_not_found() {
        let instruction = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("call_external")),
            (
                "params",
                JsonValue::object_from_pairs(&[("prompt", JsonValue::string("hello"))]),
            ),
        ]);
        let state = make_exec_state_with_instruction(instruction, make_payload(0), vec![]);

        let io_instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("io_request")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("io_type", JsonValue::string("call_external")),
                    (
                        "prompt",
                        JsonValue::string("__exec__.instruction.params.prompt"),
                    ),
                    (
                        "temperature?",
                        JsonValue::string("__exec__.instruction.params.temperature"),
                    ),
                    (
                        "max_tokens?",
                        JsonValue::string("__exec__.instruction.params.max_tokens"),
                    ),
                ]),
            ),
        ]);

        let result = exec_io_request(&io_instr, state).unwrap();
        match result {
            MetaInstructionResult::IoRequired { io_type, params } => {
                assert_eq!(io_type, "call_external");
                assert_eq!(params.get("prompt").and_then(|v| v.as_str()), Some("hello"));
                assert!(params.get("temperature").is_none());
                assert!(params.get("max_tokens").is_none());
            }
            _ => panic!("expected IoRequired"),
        }
    }

    #[test]
    fn test_io_request_required_param_path_failure_errors() {
        let instruction = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("call_external")),
            (
                "params",
                JsonValue::object_from_pairs(&[("prompt", JsonValue::string("hello"))]),
            ),
        ]);
        let state = make_exec_state_with_instruction(instruction, make_payload(0), vec![]);

        // messaegs 是拼写错误的必选参数（正确应为 messages）：
        // 必须显式报错，不得静默跳过
        let io_instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("io_request")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("io_type", JsonValue::string("call_external")),
                    (
                        "messaegs",
                        JsonValue::string("__exec__.instruction.params.messaegs"),
                    ),
                ]),
            ),
        ]);

        let result = exec_io_request(&io_instr, state);
        assert!(matches!(
            result,
            Err(TcbError::PathResolutionFailed { ref path, .. }) if path == "__exec__.instruction.params.messaegs"
        ));
    }

    #[test]
    fn test_io_request_optional_param_key_stripped_when_present() {
        // 带 '?' 后缀的可选参数在路径存在时：正常包含，且请求键名去掉 '?'
        let instruction = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("call_external")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("prompt", JsonValue::string("hello")),
                    ("temperature", JsonValue::Integer(7)),
                ]),
            ),
        ]);
        let state = make_exec_state_with_instruction(instruction, make_payload(0), vec![]);

        let io_instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("io_request")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("io_type", JsonValue::string("call_external")),
                    (
                        "prompt",
                        JsonValue::string("__exec__.instruction.params.prompt"),
                    ),
                    (
                        "temperature?",
                        JsonValue::string("__exec__.instruction.params.temperature"),
                    ),
                ]),
            ),
        ]);

        let result = exec_io_request(&io_instr, state).unwrap();
        match result {
            MetaInstructionResult::IoRequired { params, .. } => {
                assert_eq!(params.get("prompt").and_then(|v| v.as_str()), Some("hello"));
                assert_eq!(params.get("temperature"), Some(&JsonValue::Integer(7)));
                // 键名不带 '?' 后缀
                assert!(params.get("temperature?").is_none());
            }
            _ => panic!("expected IoRequired"),
        }
    }

    #[test]
    fn test_io_request_missing_io_type() {
        let state = make_exec_state("call_external", make_payload(0), vec![]);
        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("io_request")),
            (
                "params",
                JsonValue::object_from_pairs(&[("prompt", JsonValue::string("test"))]),
            ),
        ]);

        let result = exec_io_request(&instr, state);
        assert!(matches!(result, Err(TcbError::MissingField { field }) if field == "io_type"));
    }

    #[test]
    fn test_io_request_missing_params() {
        let state = make_exec_state("call_external", make_payload(0), vec![]);
        let instr = JsonValue::object_from_pairs(&[("type", JsonValue::string("io_request"))]);

        let result = exec_io_request(&instr, state);
        assert!(matches!(result, Err(TcbError::MissingField { field }) if field == "params"));
    }

    #[test]
    fn test_io_request_in_branch_propagates() {
        let state = make_exec_state("call_external", make_payload(10), vec![]);
        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("eq")),
                            ("path", JsonValue::string("__exec__.payload.x")),
                            ("value", JsonValue::Integer(10)),
                        ]),
                    ),
                    (
                        "on_true",
                        JsonValue::array(vec![JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("io_request")),
                            (
                                "params",
                                JsonValue::object_from_pairs(&[
                                    ("io_type", JsonValue::string("call_external")),
                                    ("prompt", JsonValue::string("hi")),
                                ]),
                            ),
                        ])]),
                    ),
                ]),
            ),
        ]);

        let result = execute_meta_instruction(&instr, state, 0).unwrap();
        match result {
            MetaInstructionResult::IoRequired { io_type, .. } => {
                assert_eq!(io_type, "call_external");
            }
            _ => panic!("expected IoRequired"),
        }
    }

    #[test]
    fn test_io_request_stops_subsequent_instructions() {
        let state = make_exec_state("call_external", make_payload(10), vec![]);
        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("eq")),
                            ("path", JsonValue::string("__exec__.payload.x")),
                            ("value", JsonValue::Integer(10)),
                        ]),
                    ),
                    (
                        "on_true",
                        JsonValue::array(vec![
                            JsonValue::object_from_pairs(&[
                                ("type", JsonValue::string("io_request")),
                                (
                                    "params",
                                    JsonValue::object_from_pairs(&[
                                        ("io_type", JsonValue::string("call_external")),
                                        ("prompt", JsonValue::string("hi")),
                                    ]),
                                ),
                            ]),
                            JsonValue::object_from_pairs(&[
                                ("type", JsonValue::string("set")),
                                (
                                    "params",
                                    JsonValue::object_from_pairs(&[
                                        ("attr", JsonValue::string("x")),
                                        ("operation", JsonValue::string("set")),
                                        ("value", JsonValue::Integer(999)),
                                    ]),
                                ),
                            ]),
                        ]),
                    ),
                ]),
            ),
        ]);

        let result = execute_meta_instruction(&instr, state, 0).unwrap();
        match result {
            MetaInstructionResult::IoRequired { io_type, .. } => {
                assert_eq!(io_type, "call_external");
            }
            _ => panic!("expected IoRequired"),
        }
    }

    #[test]
    fn test_io_request_in_on_false_propagates() {
        let state = make_exec_state("fallback", make_payload(5), vec![]);
        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("eq")),
                            ("path", JsonValue::string("__exec__.payload.x")),
                            ("value", JsonValue::Integer(100)),
                        ]),
                    ),
                    ("on_true", JsonValue::array(vec![])),
                    (
                        "on_false",
                        JsonValue::array(vec![JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("io_request")),
                            (
                                "params",
                                JsonValue::object_from_pairs(&[
                                    ("io_type", JsonValue::string("fallback_handler")),
                                    ("reason", JsonValue::string("primary_failed")),
                                ]),
                            ),
                        ])]),
                    ),
                ]),
            ),
        ]);

        let result = execute_meta_instruction(&instr, state, 0).unwrap();
        match result {
            MetaInstructionResult::IoRequired { io_type, params } => {
                assert_eq!(io_type, "fallback_handler");
                assert_eq!(
                    params.get("reason").and_then(|v| v.as_str()),
                    Some("primary_failed")
                );
            }
            _ => panic!("expected IoRequired"),
        }
    }

    #[test]
    fn test_io_request_nested_two_layers_propagates() {
        let state = make_exec_state("call_external", make_payload(10), vec![]);
        let inner_branch = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("eq")),
                            ("path", JsonValue::string("__exec__.payload.x")),
                            ("value", JsonValue::Integer(10)),
                        ]),
                    ),
                    (
                        "on_true",
                        JsonValue::array(vec![JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("io_request")),
                            (
                                "params",
                                JsonValue::object_from_pairs(&[
                                    ("io_type", JsonValue::string("call_external")),
                                    ("prompt", JsonValue::string("nested")),
                                ]),
                            ),
                        ])]),
                    ),
                ]),
            ),
        ]);
        let outer_branch = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("eq")),
                            ("path", JsonValue::string("__exec__.payload.x")),
                            ("value", JsonValue::Integer(10)),
                        ]),
                    ),
                    ("on_true", JsonValue::array(vec![inner_branch])),
                ]),
            ),
        ]);

        let result = execute_meta_instruction(&outer_branch, state, 0).unwrap();
        match result {
            MetaInstructionResult::IoRequired { io_type, params } => {
                assert_eq!(io_type, "call_external");
                assert_eq!(
                    params.get("prompt").and_then(|v| v.as_str()),
                    Some("nested")
                );
            }
            _ => panic!("expected IoRequired"),
        }
    }

    // ===== 未知元指令测试 =====

    #[test]
    fn test_unknown_meta_instruction() {
        let state = make_exec_state("unknown", make_payload(0), vec![]);
        let instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("unknown_op")),
            ("params", JsonValue::object_from_pairs(&[])),
        ]);

        let result = execute_meta_instruction(&instr, state, 0);
        assert!(
            matches!(result, Err(TcbError::UnknownMetaInstruction { meta_type }) if meta_type == "unknown_op")
        );
    }

    #[test]
    fn test_missing_type_field() {
        let state = make_exec_state("noop", make_payload(0), vec![]);
        let instr = JsonValue::object_from_pairs(&[("params", JsonValue::object_from_pairs(&[]))]);

        let result = execute_meta_instruction(&instr, state, 0);
        assert!(matches!(result, Err(TcbError::MissingField { field }) if field == "type"));
    }

    // ===== 字面量指令测试 =====

    #[test]
    fn test_push_with_literal_object() {
        let state = make_exec_state("while_loop", make_payload(0), vec![]);
        let push_instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("push")),
            (
                "params",
                JsonValue::object_from_pairs(&[(
                    "instructions",
                    JsonValue::array(vec![JsonValue::object_from_pairs(&[(
                        "type",
                        JsonValue::string("noop"),
                    )])]),
                )]),
            ),
        ]);

        let result = exec_push(&push_instr, state).unwrap();
        let queue = resolve_path(&result, "__exec__.queue").unwrap();
        let arr = queue.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].get("type").and_then(|v| v.as_str()), Some("noop"));
    }

    #[test]
    fn test_io_request_literal_params_always_included() {
        let state = make_exec_state("call_external", make_payload(0), vec![]);
        let io_instr = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("io_request")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("io_type", JsonValue::string("call_external")),
                    ("prompt", JsonValue::string("fixed_prompt")),
                    ("timeout_ms", JsonValue::Integer(5000)),
                ]),
            ),
        ]);

        let result = exec_io_request(&io_instr, state).unwrap();
        match result {
            MetaInstructionResult::IoRequired { params, .. } => {
                assert_eq!(
                    params.get("prompt").and_then(|v| v.as_str()),
                    Some("fixed_prompt")
                );
                assert_eq!(params.get("timeout_ms"), Some(&JsonValue::Integer(5000)));
            }
            _ => panic!("expected IoRequired"),
        }
    }

    mod react_tests {
        use super::*;
        use crate::value::{JsonValue, ObjectMap};
        use alloc::vec;

        /// 构建包含 tool_calls 的 llm_response 状态
        fn make_state_with_tool_calls() -> JsonValue {
            let tool_call1 = JsonValue::object_from_pairs(&[
                ("name", JsonValue::string("get_weather")),
                (
                    "args",
                    JsonValue::object_from_pairs(&[("city", JsonValue::string("Beijing"))]),
                ),
            ]);
            let tool_call2 = JsonValue::object_from_pairs(&[
                ("name", JsonValue::string("get_time")),
                ("args", JsonValue::object_from_pairs(&[])),
            ]);

            let tool_calls = JsonValue::array(vec![tool_call1, tool_call2]);
            let mut llm_response = ObjectMap::new();
            llm_response.insert("tool_calls".to_string(), tool_calls);
            // 添加消息历史
            let messages = JsonValue::array(vec![JsonValue::object_from_pairs(&[
                ("role", JsonValue::string("user")),
                ("content", JsonValue::string("What's the weather?")),
            ])]);
            llm_response.insert("messages".to_string(), messages);

            let mut payload = ObjectMap::new();
            payload.insert("llm_response".to_string(), JsonValue::Object(llm_response));

            let mut exec = ObjectMap::new();
            exec.insert("payload".to_string(), JsonValue::Object(payload));
            exec.insert("queue".to_string(), JsonValue::empty_array());

            let mut root = ObjectMap::new();
            root.insert("__exec__".to_string(), JsonValue::Object(exec));
            JsonValue::Object(root)
        }

        #[test]
        fn test_has_fields_with_tool_calls() {
            let state = make_state_with_tool_calls();
            let domain = JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("has_fields")),
                ("path", JsonValue::string("__exec__.payload.llm_response")),
                (
                    "fields",
                    JsonValue::array(vec![JsonValue::string("tool_calls")]),
                ),
            ]);
            assert_eq!(
                evaluate_domain_outcome(&domain, &state).unwrap(),
                DomainOutcome::Sat
            );
        }

        #[test]
        fn test_has_fields_missing_field() {
            let state = make_state_with_tool_calls();
            let domain = JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("has_fields")),
                ("path", JsonValue::string("__exec__.payload.llm_response")),
                (
                    "fields",
                    JsonValue::array(vec![JsonValue::string("missing_field")]),
                ),
            ]);
            assert_eq!(
                evaluate_domain_outcome(&domain, &state).unwrap(),
                DomainOutcome::Unsat
            );
        }

        #[test]
        fn test_has_fields_empty_tool_calls_returns_false() {
            let mut llm_response = ObjectMap::new();
            llm_response.insert("tool_calls".to_string(), JsonValue::empty_array());
            let mut payload = ObjectMap::new();
            payload.insert("llm_response".to_string(), JsonValue::Object(llm_response));
            let mut exec = ObjectMap::new();
            exec.insert("payload".to_string(), JsonValue::Object(payload));
            let mut root = ObjectMap::new();
            root.insert("__exec__".to_string(), JsonValue::Object(exec));
            let state = JsonValue::Object(root);

            let domain = JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("has_fields")),
                ("path", JsonValue::string("__exec__.payload.llm_response")),
                (
                    "fields",
                    JsonValue::array(vec![JsonValue::string("tool_calls")]),
                ),
            ]);
            assert_eq!(
                evaluate_domain_outcome(&domain, &state).unwrap(),
                DomainOutcome::Unsat
            );
        }
    }
}

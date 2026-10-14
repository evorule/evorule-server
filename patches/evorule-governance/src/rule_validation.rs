// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 规则校验 API —— 静态验证 + 安全分析
//!
//! # 设计依据
//!
//! 本模块为 evorule 核心对外暴露的规则校验接口，供：
//! - **evorule-application/panels/rule-editor**：可视化编辑器在保存前实时校验
//! - **evorule-cli**：`evorule validate` 子命令（未来可桥接到此 API）
//! - **嵌入式场景**：设备通过 HTTP API 校验规则，无需集成 CLI
//!
//! # 机制-策略分离
//!
//! 本模块**只验证机制层约束**，不涉及业务语义：
//! - ✅ 结构完整性：type 字段存在、必填参数完备
//! - ✅ 类型合法性：指令类型在白名单内
//! - ✅ 安全风险：循环深度、I/O 频率、payload 增长边界
//! - ❌ 不验证业务规则（如"库存扣减不能为负"）—— 那是 semantic_invariants 的职责
//!
//! # 参考
//! - `evorule-cli/src/commands/validate.rs`：CLI 侧的元指令白名单校验
//! - `evorule-tcb/src/executor.rs`：`execute_meta_instruction` 的合法指令类型
//! - `evorule-reactor/src/特别规范.md`：机制-策略分离原则

use evorule_tcb::JsonValue;
use serde::Serialize;

// ============================================================================
// 元指令白名单
// ============================================================================

/// 合法元指令类型白名单（P0-01：对齐 executor.rs 元指令白名单）
///
/// 来源：evorule-tcb/src/executor.rs::META_INSTRUCTION_TYPES，
/// 4 种：set / push / branch / io_request（enforce 由 tier 门禁控制不入选；
/// collect/merge 已退役，规则清理方案 2026-09-14）。
/// noop/increment/decrement 是**指令层（instruction）**类型，不是元指令层，不得混入本白名单
/// （双层语言框架，records/75；P0-01 修前曾误混，导致假阳性/假阴性）。
/// 不含 G8 禁止词（conditional/while_loop/sequence），故无需 build.rs 豁免。
const VALID_TRANSFORM_TYPES: &[&str] = &["branch", "set", "push", "io_request"];

/// `branch` 指令的必填参数
const BRANCH_REQUIRED: &[&str] = &["domain"];
/// `set` 指令的必填参数
const SET_REQUIRED: &[&str] = &["attr", "operation", "value"];
/// `push` 指令的必填参数
const PUSH_REQUIRED: &[&str] = &["instructions"];
/// `io_request` 指令的必填参数
const IO_REQUEST_REQUIRED: &[&str] = &["io_type"];

/// `set` 指令的合法 operation 值
const VALID_OPERATIONS: &[&str] = &["set", "add", "sub"];

// ============================================================================
// 安全分析阈值
// ============================================================================

/// 最大 transform 规则数量（与 transition.rs 的 MAX_TRANSFORM_RULES 一致）
const MAX_TRANSFORM_RULES: usize = 64;
/// 最大嵌套深度（branch 内的 branch 等）
///
/// P2-02：对齐 TCB MAX_BRANCH_DEPTH=64（executor.rs L28），避免校验器比引擎更严
/// 造成假阳性（SSOT：同一上限不得多处不同定义）。防御性建议上限见 check_recursive_nesting。
const MAX_NESTING_DEPTH: usize = 64;
/// 单个 io_request 内最大参数大小（字节）
const MAX_IO_PARAMS_SIZE: usize = 1024 * 10; // 10KB
/// 安全分析中检测到的最大循环深度阈值
const INFINITE_LOOP_DEPTH_THRESHOLD: usize = 5;

// ============================================================================
// 返回类型
// ============================================================================

/// 校验结果
#[derive(Debug, Clone, Serialize)]
pub struct ValidationResult {
    /// 是否通过（所有校验项均无 error）
    pub passed: bool,
    /// 静态校验项
    pub static_validation: StaticValidation,
    /// 安全分析项
    pub security_analysis: SecurityAnalysis,
    /// 汇总
    pub summary: ValidationSummary,
}

/// 静态校验结果
#[derive(Debug, Clone, Serialize)]
pub struct StaticValidation {
    /// 校验项列表
    pub checks: Vec<ValidationCheck>,
    /// 错误计数
    pub error_count: usize,
    /// 警告计数
    pub warn_count: usize,
}

/// 单条校验项
#[derive(Debug, Clone, Serialize)]
pub struct ValidationCheck {
    /// 校验项名称
    pub name: &'static str,
    /// 是否通过
    pub passed: bool,
    /// 严重级别：error / warn / info
    pub level: &'static str,
    /// 详细描述
    pub message: String,
    /// 关联的 transform 索引（-1 表示全局）
    pub transform_index: i32,
}

/// 安全分析结果
#[derive(Debug, Clone, Serialize)]
pub struct SecurityAnalysis {
    /// 分析项列表
    pub checks: Vec<ValidationCheck>,
    /// 风险计数
    pub risk_count: usize,
    /// 整体风险等级：low / medium / high
    pub risk_level: &'static str,
}

/// 校验汇总
#[derive(Debug, Clone, Serialize)]
pub struct ValidationSummary {
    /// 总规则数
    pub total_transforms: usize,
    /// 总错误数
    pub total_errors: usize,
    /// 总警告数
    pub total_warnings: usize,
    /// 总风险数
    pub total_risks: usize,
}

// ============================================================================
// 入口函数
// ============================================================================

/// 执行完整的规则校验（静态验证 + 安全分析）
///
/// # 参数
/// - `transforms`: 规则 transform 列表（从 core_eval.json 或规则目录加载）
///
/// # 返回值
/// 包含全部校验项和汇总的 `ValidationResult`。
///
/// # 示例
/// ```
/// use evorule_governance::rule_validation::validate_rules;
/// use evorule_tcb::JsonValue;
///
/// // 一条合法规则：无条件 set(x, 42)
/// let rule = JsonValue::object_from_pairs(&[
///     ("type", JsonValue::string("set")),
///     ("params", JsonValue::object_from_pairs(&[
///         ("attr", JsonValue::string("x")),
///         ("operation", JsonValue::string("set")),
///         ("value", JsonValue::Integer(42)),
///     ])),
/// ]);
///
/// let result = validate_rules(&[rule]);
/// assert!(result.passed, "合法规则应通过校验");
/// assert_eq!(result.summary.total_transforms, 1);
/// assert_eq!(result.summary.total_errors, 0);
/// ```
pub fn validate_rules(transforms: &[JsonValue]) -> ValidationResult {
    // === 阶段一：静态验证 ===
    let static_checks = perform_static_validation(transforms);
    let static_error_count = static_checks.iter().filter(|c| c.level == "error").count();
    let static_warn_count = static_checks.iter().filter(|c| c.level == "warn").count();

    // 如果静态验证有 error，安全分析仍然执行（但可能不完整）
    // 安全分析需要结构正确的规则才能做有意义的分析

    // === 阶段二：安全分析 ===
    let security_checks = perform_security_analysis(transforms);
    let risk_count = security_checks
        .iter()
        .filter(|c| c.level == "error" || c.level == "warn")
        .count();

    // === 计算汇总 ===
    let total_errors = static_error_count;
    let total_warnings =
        static_warn_count + security_checks.iter().filter(|c| c.level == "warn").count();
    let total_risks = risk_count;

    let risk_level = if total_errors > 0 || total_risks > 3 {
        "high"
    } else if total_warnings > 0 {
        "medium"
    } else {
        "low"
    };

    // 静态验证的 error 才导致 passed=false（安全分析 warn 不阻断）
    let passed = static_error_count == 0;

    ValidationResult {
        passed,
        static_validation: StaticValidation {
            checks: static_checks,
            error_count: static_error_count,
            warn_count: static_warn_count,
        },
        security_analysis: SecurityAnalysis {
            checks: security_checks,
            risk_count,
            risk_level,
        },
        summary: ValidationSummary {
            total_transforms: transforms.len(),
            total_errors,
            total_warnings,
            total_risks,
        },
    }
}

// ============================================================================
// 阶段一：静态验证
// ============================================================================

/// 执行静态结构验证
fn perform_static_validation(transforms: &[JsonValue]) -> Vec<ValidationCheck> {
    let mut checks = Vec::new();

    // 1. 全局：transform 数量检查
    checks.push(check_transform_count(transforms));

    if transforms.is_empty() {
        checks.push(ValidationCheck {
            name: "non_empty",
            passed: false,
            level: "error",
            message: "Transform 列表为空，至少需要一条规则".to_string(),
            transform_index: -1,
        });
        return checks;
    }

    for (i, t) in transforms.iter().enumerate() {
        let idx = i as i32;

        // 2. type 字段存在性
        checks.push(check_type_exists(t, idx));

        // 如果 type 存在，检查合法性
        if let Some(type_str) = t.get("type").and_then(|v| v.as_str()) {
            checks.push(check_type_valid(type_str, idx));

            // 3. 参数完备性（仅在 type 合法时检查）
            checks.push(check_params_complete(t, type_str, idx));
        }

        // 4. 递归检查嵌套结构（branch 的 on_true/on_false）
        if let Some(type_str) = t.get("type").and_then(|v| v.as_str()) {
            if type_str == "branch" {
                checks.extend(check_branch_nested(t, idx, 0));
            }
        }
    }

    checks
}

/// 检查 transform 数量是否在限制内
fn check_transform_count(transforms: &[JsonValue]) -> ValidationCheck {
    let passed = transforms.len() <= MAX_TRANSFORM_RULES;
    ValidationCheck {
        name: "transform_count_limit",
        passed,
        level: if passed { "info" } else { "error" },
        message: format!(
            "Transform 数量: {} (上限: {}, {})",
            transforms.len(),
            MAX_TRANSFORM_RULES,
            if passed { "合规" } else { "超出限制" }
        ),
        transform_index: -1,
    }
}

/// 检查 type 字段是否存在
fn check_type_exists(t: &JsonValue, idx: i32) -> ValidationCheck {
    let has_type = t.get("type").and_then(|v| v.as_str()).is_some();
    ValidationCheck {
        name: "type_exists",
        passed: has_type,
        level: if has_type { "info" } else { "error" },
        message: if has_type {
            format!("transform[{}] 的 type 字段存在", idx)
        } else {
            format!("transform[{}] 缺少 'type' 字段", idx)
        },
        transform_index: idx,
    }
}

/// 检查 type 是否在白名单内
fn check_type_valid(type_str: &str, idx: i32) -> ValidationCheck {
    let valid = VALID_TRANSFORM_TYPES.contains(&type_str);
    ValidationCheck {
        name: "type_valid",
        passed: valid,
        level: if valid { "info" } else { "error" },
        message: if valid {
            format!("transform[{}] 的 type '{}' 是合法元指令", idx, type_str)
        } else {
            format!(
                "transform[{}] 的 type '{}' 不在白名单中 (合法值: {})",
                idx,
                type_str,
                VALID_TRANSFORM_TYPES.join(", ")
            )
        },
        transform_index: idx,
    }
}

/// 检查参数完备性
fn check_params_complete(t: &JsonValue, type_str: &str, idx: i32) -> ValidationCheck {
    let required = match type_str {
        "branch" => BRANCH_REQUIRED,
        "set" => SET_REQUIRED,
        "push" => PUSH_REQUIRED,
        "io_request" => IO_REQUEST_REQUIRED,
        _ => {
            return ValidationCheck {
                name: "params_complete",
                passed: true,
                level: "info",
                message: format!("transform[{}] 的 type '{}' 无需校验参数", idx, type_str),
                transform_index: idx,
            }
        }
    };

    let mut missing: Vec<&str> = Vec::new();
    let mut extra_checks: Vec<String> = Vec::new();

    for param in required {
        if t.get("params").and_then(|p| p.get(param)).is_none() {
            missing.push(param);
        }
    }

    // 额外检查：set 的 operation 值（P1-02：非法 operation 提升为 error 阻断，
    // 因 TCB exec_set 对未知 operation 硬失败 UnknownOperation，校验层不得降级为 warn）
    let mut operation_invalid = false;
    if type_str == "set" && missing.is_empty() {
        if let Some(op) = t
            .get("params")
            .and_then(|p| p.get("operation"))
            .and_then(|v| v.as_str())
        {
            if !VALID_OPERATIONS.contains(&op) {
                operation_invalid = true;
                extra_checks.push(format!(
                    "operation '{}' 不在合法值中 ({})",
                    op,
                    VALID_OPERATIONS.join(", ")
                ));
            }
        }
    }

    // 额外检查：io_request 的 params 大小
    // 安全风险: params 过大可能导致 OOM/DoS,设为 error 级别(阻断验证)
    let mut params_too_large = false;
    if type_str == "io_request" && missing.is_empty() {
        if let Some(params) = t.get("params") {
            let params_str = format!("{:?}", params);
            if params_str.len() > MAX_IO_PARAMS_SIZE {
                extra_checks.push(format!(
                    "io_request 参数过大: {} 字节 (上限: {} 字节)",
                    params_str.len(),
                    MAX_IO_PARAMS_SIZE
                ));
                params_too_large = true;
            }
        }
    }

    let passed = missing.is_empty() && extra_checks.is_empty();
    let level = if passed {
        "info"
    } else if !missing.is_empty() || params_too_large || operation_invalid {
        // 缺失必填 / params 过大 / operation 非法 → error 级别（阻断）
        "error"
    } else {
        // 其他额外检查 → warn 级别
        "warn"
    };

    let mut msg_parts: Vec<String> = Vec::new();
    if !missing.is_empty() {
        msg_parts.push(format!("缺少必填参数: {}", missing.join(", ")));
    }
    msg_parts.extend(extra_checks);

    ValidationCheck {
        name: "params_complete",
        passed,
        level,
        message: if msg_parts.is_empty() {
            format!("transform[{}] 的 params 参数完备", idx)
        } else {
            format!("transform[{}]: {}", idx, msg_parts.join("; "))
        },
        transform_index: idx,
    }
}

/// 递归检查 branch 的嵌套结构
fn check_branch_nested(t: &JsonValue, idx: i32, depth: usize) -> Vec<ValidationCheck> {
    let mut checks = Vec::new();

    if depth > MAX_NESTING_DEPTH {
        checks.push(ValidationCheck {
            name: "nesting_depth",
            passed: false,
            level: "error",
            message: format!("transform[{}] 的嵌套深度超过 {} 层", idx, MAX_NESTING_DEPTH),
            transform_index: idx,
        });
        return checks;
    }

    // 检查 on_true
    if let Some(on_true) = t
        .get("params")
        .and_then(|p| p.get("on_true"))
        .and_then(|v| v.as_array())
    {
        for child in on_true {
            if let Some(child_type) = child.get("type").and_then(|v| v.as_str()) {
                if child_type == "branch" {
                    checks.extend(check_branch_nested(child, idx, depth + 1));
                }
            }
        }
    }

    // 检查 on_false
    if let Some(on_false) = t
        .get("params")
        .and_then(|p| p.get("on_false"))
        .and_then(|v| v.as_array())
    {
        for child in on_false {
            if let Some(child_type) = child.get("type").and_then(|v| v.as_str()) {
                if child_type == "branch" {
                    checks.extend(check_branch_nested(child, idx, depth + 1));
                }
            }
        }
    }

    checks
}

// ============================================================================
// 阶段二：安全分析
// ============================================================================

/// 执行安全分析
fn perform_security_analysis(transforms: &[JsonValue]) -> Vec<ValidationCheck> {
    vec![
        // 1. 无限循环检测：while_loop 在 domain 中，但 body 没有状态变更
        check_infinite_loop_risk(transforms),
        // 2. 递归嵌套风险：branch 嵌套过深
        check_recursive_nesting(transforms),
        // 3. 无界 I/O 检测：io_request 在循环内
        check_unbounded_io(transforms),
        // 4. payload 无界增长检测：重复 push 且无清理
        check_payload_growth(transforms),
        // 5. 交叉引用检测：domain 引用自身
        check_self_reference(transforms),
    ]
}

/// 检测无限循环风险
///
/// 检查 while_loop 类型的 domain 中是否包含条件判定，
/// 以及 body 是否包含状态变更指令。
/// B9（report-002）：递归遍历 branch 的 on_true/on_false 子节点，
/// 嵌套在 branch 内的 while_loop / 状态变更不再漏检。
/// 会话状态全局共享——任意层级的 set 均可为任意层级的
/// while_loop 提供终止条件，故信号跨层级累加。
fn check_infinite_loop_risk(transforms: &[JsonValue]) -> ValidationCheck {
    let mut has_while_loop = false;
    let mut has_state_change = false;
    collect_loop_risk_signals(transforms, &mut has_while_loop, &mut has_state_change);

    let risk = has_while_loop && !has_state_change;
    ValidationCheck {
        name: "infinite_loop",
        passed: !risk,
        level: if risk { "warn" } else { "info" },
        message: if risk {
            "检测到 while_loop 但未找到状态变更指令 (set)，可能导致无限循环".to_string()
        } else if has_while_loop {
            "while_loop 存在配套的状态变更指令，循环可终止".to_string()
        } else {
            "未检测到 while_loop 指令".to_string()
        },
        transform_index: -1,
    }
}

/// 递归收集 while_loop 与状态变更信号（含 branch 子节点）
fn collect_loop_risk_signals(
    transforms: &[JsonValue],
    has_while_loop: &mut bool,
    has_state_change: &mut bool,
) {
    for t in transforms {
        if let Some(type_str) = t.get("type").and_then(|v| v.as_str()) {
            if type_str == "branch" {
                // 检查 domain 是否引用控制流指令
                // 使用 ControlFlowType::parse 而非字面量比较，满足 G8 门禁
                if let Some(domain) = t.get("params").and_then(|p| p.get("domain")) {
                    if let Some(inner_type) = domain.get("type").and_then(|v| v.as_str()) {
                        if inner_type == "instruction" {
                            if let Some(inst_type) =
                                domain.get("instruction_type").and_then(|v| v.as_str())
                            {
                                if evorule_reactor::ControlFlowType::parse(inst_type)
                                    == Some(evorule_reactor::ControlFlowType::WhileLoop)
                                {
                                    *has_while_loop = true;
                                }
                            }
                        }
                    }
                }
                // B9：递归 branch 子节点 on_true / on_false
                if let Some(params) = t.get("params") {
                    for key in ["on_true", "on_false"] {
                        if let Some(sub) = params.get(key).and_then(|v| v.as_array()) {
                            collect_loop_risk_signals(sub, has_while_loop, has_state_change);
                        }
                    }
                }
            }

            // 检查是否有状态变更指令（P0-01：元指令层无 increment/decrement，
            // 状态变更由 set 承担；指令层 increment/decrement 不在 transform 层）
            if type_str == "set" {
                *has_state_change = true;
            }
        }
    }
}

/// 检测递归嵌套风险
fn check_recursive_nesting(transforms: &[JsonValue]) -> ValidationCheck {
    let mut max_depth = 0usize;

    for t in transforms.iter() {
        if let Some(type_str) = t.get("type").and_then(|v| v.as_str()) {
            if type_str == "branch" {
                let depth = measure_nesting_depth(t, 0);
                max_depth = max_depth.max(depth);
            }
        }
    }

    let risk = max_depth >= INFINITE_LOOP_DEPTH_THRESHOLD;
    ValidationCheck {
        name: "recursive_nesting",
        passed: !risk,
        level: if risk { "warn" } else { "info" },
        message: format!(
            "最大嵌套深度: {} 层{}",
            max_depth,
            if risk {
                format!(" (超过阈值 {}，建议拆分)", INFINITE_LOOP_DEPTH_THRESHOLD)
            } else {
                String::new()
            }
        ),
        transform_index: -1,
    }
}

/// 测量 branch 的嵌套深度
fn measure_nesting_depth(t: &JsonValue, depth: usize) -> usize {
    let mut max_child_depth = depth;

    for branch_key in &["on_true", "on_false"] {
        if let Some(children) = t
            .get("params")
            .and_then(|p| p.get(branch_key))
            .and_then(|v| v.as_array())
        {
            for child in children {
                if let Some(child_type) = child.get("type").and_then(|v| v.as_str()) {
                    if child_type == "branch" {
                        let child_depth = measure_nesting_depth(child, depth + 1);
                        max_child_depth = max_child_depth.max(child_depth);
                    }
                }
            }
        }
    }

    max_child_depth
}

/// 检测无界 I/O 风险
///
/// 检查 while_loop 内部是否包含 io_request（可能导致无限制的 I/O 调用）
fn check_unbounded_io(transforms: &[JsonValue]) -> ValidationCheck {
    let mut unbounded_io_count = 0usize;

    for t in transforms {
        if let Some(type_str) = t.get("type").and_then(|v| v.as_str()) {
            if type_str == "branch" {
                unbounded_io_count += count_io_in_loop_body(t);
            }
        }
    }

    let risk = unbounded_io_count > 0;
    ValidationCheck {
        name: "unbounded_io",
        passed: !risk,
        level: if risk { "warn" } else { "info" },
        message: if risk {
            format!(
                "检测到 {} 处 io_request 可能在循环体内执行，可能导致无限制 I/O 调用",
                unbounded_io_count
            )
        } else {
            "未检测到循环体内的 io_request".to_string()
        },
        transform_index: -1,
    }
}

/// 统计 branch 体内 io_request 的数量（简化版：仅检查直接子节点）
fn count_io_in_loop_body(t: &JsonValue) -> usize {
    let mut count = 0usize;

    for branch_key in &["on_true", "on_false"] {
        if let Some(children) = t
            .get("params")
            .and_then(|p| p.get(branch_key))
            .and_then(|v| v.as_array())
        {
            for child in children {
                if let Some(child_type) = child.get("type").and_then(|v| v.as_str()) {
                    match child_type {
                        "io_request" => count += 1,
                        "branch" => count += count_io_in_loop_body(child),
                        _ => {}
                    }
                }
            }
        }
    }

    count
}

/// 检测 payload 无界增长风险
///
/// 检查 push 指令是否可能无限追加数据到 payload
fn check_payload_growth(transforms: &[JsonValue]) -> ValidationCheck {
    let mut push_count = 0usize;
    let mut has_cleanup = false;

    for t in transforms {
        if let Some(type_str) = t.get("type").and_then(|v| v.as_str()) {
            if type_str == "push" {
                push_count += 1;
            }
            // 检查是否有清理操作（set 到空数组/对象）
            if type_str == "set" {
                if let Some(value) = t.get("params").and_then(|p| p.get("value")) {
                    if value.is_array() || value.is_null() {
                        has_cleanup = true;
                    }
                }
            }
        }
    }

    let risk = push_count > 3 && !has_cleanup;
    ValidationCheck {
        name: "payload_growth",
        passed: !risk,
        level: if risk { "warn" } else { "info" },
        message: if risk {
            format!(
                "检测到 {} 处 push 指令但未找到清理操作，可能导致 payload 无界增长",
                push_count
            )
        } else {
            format!(
                "push 指令 {} 处{}",
                push_count,
                if has_cleanup {
                    "，有配套清理操作"
                } else {
                    ""
                }
            )
        },
        transform_index: -1,
    }
}

/// 检测自引用风险
///
/// 检查 domain 是否引用自身（如 instruction_type 等于自身 type）
fn check_self_reference(transforms: &[JsonValue]) -> ValidationCheck {
    let mut self_refs: Vec<String> = Vec::new();

    for (i, t) in transforms.iter().enumerate() {
        if let Some(type_str) = t.get("type").and_then(|v| v.as_str()) {
            if type_str == "branch" {
                if let Some(domain) = t.get("params").and_then(|p| p.get("domain")) {
                    if let Some(inner_type) = domain.get("type").and_then(|v| v.as_str()) {
                        if inner_type == "instruction" {
                            if let Some(inst_type) =
                                domain.get("instruction_type").and_then(|v| v.as_str())
                            {
                                // 检查 on_true 是否包含与 instruction_type 同类型的指令
                                if let Some(on_true) = t
                                    .get("params")
                                    .and_then(|p| p.get("on_true"))
                                    .and_then(|v| v.as_array())
                                {
                                    for child in on_true {
                                        if let Some(child_type) =
                                            child.get("type").and_then(|v| v.as_str())
                                        {
                                            if child_type == inst_type || child_type == "branch" {
                                                self_refs.push(format!(
                                                    "transform[{}] 匹配 '{}' 但体内包含同类型指令",
                                                    i, inst_type
                                                ));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let risk = !self_refs.is_empty();
    ValidationCheck {
        name: "self_reference",
        passed: !risk,
        level: if risk { "warn" } else { "info" },
        message: if risk {
            format!("检测到自引用风险: {}", self_refs.join("; "))
        } else {
            "未检测到自引用".to_string()
        },
        transform_index: -1,
    }
}

// ============================================================================
// 对外 API 函数（供 server.rs 路由调用）
// ============================================================================

/// 从 JSON 字符串解析并校验规则
///
/// # 参数
/// - `json_str`: JSON 字符串，可以是以下格式：
///   - `{"transform": [...]}` — 标准 core_eval 格式
///   - `[{...}, {...}]` — 顶层数组
///   - `{...}` — 单条 transform
///
/// # 返回值
/// 校验结果，包含静态验证和安全分析。
///
/// # 示例
/// ```
/// use evorule_governance::rule_validation::validate_rules_from_json;
///
/// // 标准 core_eval 格式（含 transform 数组）
/// let json = r#"{
///   "transform": [
///     {
///       "type": "branch",
///       "params": {
///         "domain": {"type": "all", "inner": []},
///         "on_true": [
///           {"type": "set", "params": {"attr": "x", "operation": "set", "value": 42}}
///         ]
///       }
///     }
///   ]
/// }"#;
///
/// let result = validate_rules_from_json(json).expect("JSON 应可解析");
/// assert!(result.passed);
///
/// // 非法 JSON 应返回 Err
/// assert!(validate_rules_from_json("{not valid json}").is_err());
/// ```
pub fn validate_rules_from_json(json_str: &str) -> Result<ValidationResult, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(json_str).map_err(|e| format!("JSON 解析失败: {}", e))?;

    let transforms = extract_transforms(&parsed)?;

    // 转换为 tier0 JsonValue
    let tcb_transforms: Vec<JsonValue> = transforms
        .iter()
        .map(evorule_reactor::serde_to_tcb)
        .collect();

    Ok(validate_rules(&tcb_transforms))
}

/// 从 serde_json::Value 中提取 transform 列表
fn extract_transforms(json: &serde_json::Value) -> Result<Vec<serde_json::Value>, String> {
    match json {
        serde_json::Value::Object(map) => {
            if let Some(arr) = map.get("transform").and_then(|v| v.as_array()) {
                Ok(arr.clone())
            } else if let Some(arr) = map.get("transforms").and_then(|v| v.as_array()) {
                Ok(arr.clone())
            } else {
                // 单对象视为单条 transform
                Ok(vec![json.clone()])
            }
        }
        serde_json::Value::Array(arr) => Ok(arr.clone()),
        _ => Err("JSON 必须是对象或数组".to_string()),
    }
}

// ============================================================================
// 辅助函数：JSON 序列化（用于 HTTP 响应）
// ============================================================================

/// 将校验结果序列化为 JSON 字符串
pub fn validation_result_to_json(result: &ValidationResult) -> String {
    serde_json::to_string_pretty(result).unwrap_or_else(|_| "{}".to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    // ====================================================================
    // 静态验证测试
    // ====================================================================

    #[test]
    fn test_validate_valid_rules() {
        let transforms = vec![
            json_to_tcb(r#"{"type":"set","params":{"attr":"x","operation":"set","value":1}}"#),
            json_to_tcb(r#"{"type":"set","params":{"attr":"x","operation":"set","value":0}}"#),
            json_to_tcb(
                r#"{"type":"branch","params":{"domain":{"type":"instruction","instruction_type":"increment"},"on_true":[{"type":"set","params":{"attr":"x","operation":"set","value":1}}]}}"#,
            ),
        ];

        let result = validate_rules(&transforms);
        assert!(result.passed, "Valid rules should pass");
        assert_eq!(result.static_validation.error_count, 0);
    }

    #[test]
    fn test_validate_empty_rules() {
        let transforms: Vec<JsonValue> = vec![];
        let result = validate_rules(&transforms);
        assert!(!result.passed, "Empty rules should fail");
        assert!(result.static_validation.error_count > 0);
    }

    #[test]
    fn test_validate_unknown_type() {
        let transforms = vec![json_to_tcb(r#"{"type":"unknown_type"}"#)];
        let result = validate_rules(&transforms);
        assert!(!result.passed, "Unknown type should fail");
        // 应该有 type_valid 错误
        assert!(result.static_validation.error_count >= 1);
    }

    #[test]
    fn test_validate_missing_type() {
        let transforms = vec![json_to_tcb(r#"{"params":{"attr":"x"}}"#)];
        let result = validate_rules(&transforms);
        assert!(!result.passed, "Missing type should fail");
    }

    #[test]
    fn test_validate_missing_params() {
        let transforms = vec![json_to_tcb(r#"{"type":"set"}"#)];
        let result = validate_rules(&transforms);
        assert!(!result.passed, "Missing params should fail");
        assert!(result.static_validation.error_count >= 1);
    }

    #[test]
    fn test_validate_invalid_operation() {
        let transforms = vec![json_to_tcb(
            r#"{"type":"set","params":{"attr":"x","operation":"invalid_op","value":0}}"#,
        )];
        let result = validate_rules(&transforms);
        // P1-02：非法 operation 提升为 error，阻断 passed
        //（TCB exec_set 对未知 operation 硬失败 UnknownOperation，校验层不得降级为 warn）
        assert!(
            !result.passed,
            "Invalid operation should be error-level and block"
        );
        let params_check = result
            .static_validation
            .checks
            .iter()
            .find(|c| c.name == "params_complete");
        assert!(params_check.is_some());
        assert_eq!(params_check.unwrap().level, "error");
    }

    // ====================================================================
    // 安全分析测试
    // ====================================================================

    #[test]
    fn test_security_infinite_loop_detection() {
        // while_loop 但没有状态变更指令
        //（P0-01：on_true 用合法元指令 push——非状态变更，保持"无状态变更"检测场景）
        let transforms = vec![json_to_tcb(
            r#"{"type":"branch","params":{"domain":{"type":"instruction","instruction_type":"while_loop"},"on_true":[{"type":"push","params":{"instructions":[]}}]}}"#,
        )];
        let result = validate_rules(&transforms);
        // 应该有 infinite_loop 警告
        let infinite_loop = result
            .security_analysis
            .checks
            .iter()
            .find(|c| c.name == "infinite_loop");
        assert!(infinite_loop.is_some());
        assert!(!infinite_loop.unwrap().passed);
    }

    #[test]
    fn test_security_infinite_loop_nested_branch_detection() {
        // B9（report-002）：嵌套在 branch 子节点内的 while_loop 不再漏检
        // 外层 branch domain 为 comparison（非 while_loop），while_loop 藏在
        // on_false 子数组的内层 branch domain 中——旧实现只扫顶层会漏检
        let v = serde_json::json!({
            "type": "branch",
            "params": {
                "domain": {"type": "comparison", "field": "x", "operator": "gt", "value": 0},
                "on_true": [{"type": "push", "params": {"instructions": []}}],
                "on_false": [
                    {"type": "branch", "params": {
                        "domain": {"type": "instruction", "instruction_type": "while_loop"},
                        "on_true": [{"type": "push", "params": {"instructions": []}}],
                    }},
                ],
            },
        });
        let transforms = vec![evorule_reactor::serde_to_tcb(&v)];
        let result = validate_rules(&transforms);
        let infinite_loop = result
            .security_analysis
            .checks
            .iter()
            .find(|c| c.name == "infinite_loop");
        assert!(infinite_loop.is_some(), "嵌套 while_loop 仍须产出检查项");
        assert!(
            !infinite_loop.unwrap().passed,
            "嵌套 branch 内的 while_loop 无状态变更必须告警"
        );
    }

    #[test]
    fn test_security_infinite_loop_nested_state_change_covers() {
        // B9 补充：嵌套 while_loop 存在时，任意层级的状态变更（含嵌套层级）
        // 均提供终止条件 → 不告警
        let v = serde_json::json!({
            "type": "branch",
            "params": {
                "domain": {"type": "comparison", "field": "x", "operator": "gt", "value": 0},
                "on_false": [
                    {"type": "branch", "params": {
                        "domain": {"type": "instruction", "instruction_type": "while_loop"},
                        "on_true": [{"type": "set", "params": {"attr": "i", "operation": "set", "value": 1}}],
                    }},
                ],
            },
        });
        let transforms = vec![evorule_reactor::serde_to_tcb(&v)];
        let result = validate_rules(&transforms);
        let infinite_loop = result
            .security_analysis
            .checks
            .iter()
            .find(|c| c.name == "infinite_loop");
        assert!(infinite_loop.is_some());
        assert!(
            infinite_loop.unwrap().passed,
            "嵌套层级的状态变更应覆盖嵌套 while_loop"
        );
    }

    #[test]
    fn test_security_unbounded_io() {
        // while_loop + io_request 在体内
        let transforms = vec![json_to_tcb(
            r#"{"type":"branch","params":{"domain":{"type":"instruction","instruction_type":"while_loop"},"on_true":[{"type":"io_request","params":{"io_type":"http_get"}}]}}"#,
        )];
        let result = validate_rules(&transforms);
        let unbounded_io = result
            .security_analysis
            .checks
            .iter()
            .find(|c| c.name == "unbounded_io");
        assert!(unbounded_io.is_some());
        assert!(!unbounded_io.unwrap().passed);
    }

    #[test]
    fn test_security_payload_growth() {
        // 多个 push 但无清理
        let transforms = vec![
            json_to_tcb(r#"{"type":"push","params":{"instructions":[]}}"#),
            json_to_tcb(r#"{"type":"push","params":{"instructions":[]}}"#),
            json_to_tcb(r#"{"type":"push","params":{"instructions":[]}}"#),
            json_to_tcb(r#"{"type":"push","params":{"instructions":[]}}"#),
        ];
        let result = validate_rules(&transforms);
        let payload_growth = result
            .security_analysis
            .checks
            .iter()
            .find(|c| c.name == "payload_growth");
        assert!(payload_growth.is_some());
        assert!(!payload_growth.unwrap().passed);
    }

    #[test]
    fn test_security_cleanup_detected() {
        // push 有配套清理就不会告警
        let transforms = vec![
            json_to_tcb(r#"{"type":"push","params":{"instructions":[]}}"#),
            json_to_tcb(r#"{"type":"push","params":{"instructions":[]}}"#),
            json_to_tcb(r#"{"type":"push","params":{"instructions":[]}}"#),
            json_to_tcb(r#"{"type":"push","params":{"instructions":[]}}"#),
            json_to_tcb(
                r#"{"type":"set","params":{"attr":"queue","operation":"set","value":null}}"#,
            ),
        ];
        let result = validate_rules(&transforms);
        let payload_growth = result
            .security_analysis
            .checks
            .iter()
            .find(|c| c.name == "payload_growth");
        assert!(payload_growth.is_some());
        assert!(payload_growth.unwrap().passed);
    }

    // ====================================================================
    // 辅助函数
    // ====================================================================

    fn json_to_tcb(json_str: &str) -> JsonValue {
        let v: serde_json::Value = serde_json::from_str(json_str).unwrap();
        evorule_reactor::serde_to_tcb(&v)
    }

    #[test]
    fn test_validate_rules_from_json() {
        let json = r#"{"transform":[{"type":"set","params":{"attr":"x","operation":"set","value":1}},{"type":"set","params":{"attr":"x","operation":"set","value":0}}]}"#;
        let result = validate_rules_from_json(json).unwrap();
        assert!(result.passed);
    }

    #[test]
    fn test_validate_rules_from_json_array() {
        let json = r#"[{"type":"set","params":{"attr":"x","operation":"set","value":1}},{"type":"set","params":{"attr":"x","operation":"set","value":0}}]"#;
        let result = validate_rules_from_json(json).unwrap();
        assert!(result.passed);
    }

    #[test]
    fn test_validate_rules_from_json_invalid() {
        let result = validate_rules_from_json("not json");
        assert!(result.is_err());
    }

    #[test]
    fn test_transform_count_limit() {
        // 超过 64 条规则应该报错
        let mut transforms = Vec::new();
        for _ in 0..65 {
            transforms.push(json_to_tcb(
                r#"{"type":"set","params":{"attr":"x","operation":"set","value":1}}"#,
            ));
        }
        let result = validate_rules(&transforms);
        let count_check = result
            .static_validation
            .checks
            .iter()
            .find(|c| c.name == "transform_count_limit");
        assert!(count_check.is_some());
        assert!(!count_check.unwrap().passed);
    }
}

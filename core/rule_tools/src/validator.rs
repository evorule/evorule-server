// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 规则验证器 —— 静态结构验证
//!
//! 检查规则 JSON 是否符合 schema、指令类型是否合法、参数是否完备。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

/// 验证严重级别
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationSeverity {
    Error,
    Warning,
    Info,
}

/// 单条验证结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationResult {
    pub severity: ValidationSeverity,
    pub message: String,
    /// 规则名（如适用）
    pub rule_name: Option<String>,
    /// JSON 路径（如 `rules[0].instruction.params.attr`）
    pub path: Option<String>,
}

/// 验证报告
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationReport {
    pub valid: bool,
    pub rule_id: String,
    pub error_count: usize,
    pub warning_count: usize,
    pub info_count: usize,
    pub results: Vec<ValidationResult>,
}

/// 已知的元指令类型
const META_INSTRUCTIONS: &[&str] = &["set", "push", "branch", "io_request"];

/// 已知的控制流指令类型
const CONTROL_FLOW_INSTRUCTIONS: &[&str] = &["sequence", "conditional", "while_loop", "call_rule"];

/// 已知的域类型
const DOMAIN_TYPES: &[&str] = &["eq", "lt", "gt", "exists", "instruction", "all", "not"];

/// 已知的 set 操作类型
const SET_OPERATIONS: &[&str] = &["set", "add", "sub"];

/// 验证规则 JSON 文件内容
pub fn validate_rule_json(content: &str) -> ValidationReport {
    let root: Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(e) => {
            return ValidationReport {
                valid: false,
                rule_id: "<unknown>".to_string(),
                error_count: 1,
                warning_count: 0,
                info_count: 0,
                results: vec![ValidationResult {
                    severity: ValidationSeverity::Error,
                    message: format!("JSON 解析失败: {}", e),
                    rule_name: None,
                    path: None,
                }],
            };
        }
    };

    let mut results = Vec::new();
    let rule_id = root
        .get("rule_id")
        .and_then(|v| v.as_str())
        .unwrap_or("<unknown>")
        .to_string();

    // 1. 检查顶层必需字段
    check_top_level_fields(&root, &mut results);

    // 2. 检查 rules 数组
    if let Some(rules) = root.get("rules").and_then(|v| v.as_array()) {
        for (i, rule) in rules.iter().enumerate() {
            let rule_name = rule
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("<unnamed>")
                .to_string();
            check_rule_definition(rule, &rule_name, i, &mut results);
        }
    }

    // 3. 检查 rule_id 唯一性（在同一文件内）
    check_rule_name_uniqueness(&root, &mut results);

    // 4. 检查 call_rule 引用是否存在
    check_rule_references(&root, &mut results);

    let error_count = results
        .iter()
        .filter(|r| r.severity == ValidationSeverity::Error)
        .count();
    let warning_count = results
        .iter()
        .filter(|r| r.severity == ValidationSeverity::Warning)
        .count();
    let info_count = results
        .iter()
        .filter(|r| r.severity == ValidationSeverity::Info)
        .count();

    ValidationReport {
        valid: error_count == 0,
        rule_id,
        error_count,
        warning_count,
        info_count,
        results,
    }
}

/// 检查顶层必需字段
fn check_top_level_fields(root: &Value, results: &mut Vec<ValidationResult>) {
    if root.get("rule_id").is_none() {
        results.push(ValidationResult {
            severity: ValidationSeverity::Error,
            message: "缺少必需字段 'rule_id'".to_string(),
            rule_name: None,
            path: Some("$".to_string()),
        });
    }

    if root.get("rules").is_none() {
        results.push(ValidationResult {
            severity: ValidationSeverity::Warning,
            message: "缺少 'rules' 字段（无规则定义）".to_string(),
            rule_name: None,
            path: Some("$".to_string()),
        });
    } else if !root.get("rules").map(|v| v.is_array()).unwrap_or(false) {
        results.push(ValidationResult {
            severity: ValidationSeverity::Error,
            message: "'rules' 字段必须是数组".to_string(),
            rule_name: None,
            path: Some("$.rules".to_string()),
        });
    }

    if root.get("version").is_none() {
        results.push(ValidationResult {
            severity: ValidationSeverity::Info,
            message: "建议添加 'version' 字段".to_string(),
            rule_name: None,
            path: Some("$".to_string()),
        });
    }
}

/// 检查单个规则定义
fn check_rule_definition(
    rule: &Value,
    rule_name: &str,
    index: usize,
    results: &mut Vec<ValidationResult>,
) {
    let base_path = format!("$.rules[{}]", index);

    // 检查 name 字段
    if rule.get("name").is_none() {
        results.push(ValidationResult {
            severity: ValidationSeverity::Error,
            message: "规则缺少 'name' 字段".to_string(),
            rule_name: Some(rule_name.to_string()),
            path: Some(format!("{}.name", base_path)),
        });
    }

    // 检查 instruction 字段
    let instruction = match rule.get("instruction") {
        Some(i) => i,
        None => {
            results.push(ValidationResult {
                severity: ValidationSeverity::Error,
                message: "规则缺少 'instruction' 字段".to_string(),
                rule_name: Some(rule_name.to_string()),
                path: Some(format!("{}.instruction", base_path)),
            });
            return;
        }
    };

    // 递归检查指令
    check_instruction(
        instruction,
        rule_name,
        &format!("{}.instruction", base_path),
        results,
    );
}

/// 递归检查指令结构
fn check_instruction(
    instr: &Value,
    rule_name: &str,
    path: &str,
    results: &mut Vec<ValidationResult>,
) {
    let instr_type = match instr.get("type").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => {
            results.push(ValidationResult {
                severity: ValidationSeverity::Error,
                message: "指令缺少 'type' 字段".to_string(),
                rule_name: Some(rule_name.to_string()),
                path: Some(format!("{}.type", path)),
            });
            return;
        }
    };

    // 检查指令类型是否合法
    let known = META_INSTRUCTIONS
        .iter()
        .chain(CONTROL_FLOW_INSTRUCTIONS.iter())
        .any(|&t| t == instr_type);
    if !known {
        results.push(ValidationResult {
            severity: ValidationSeverity::Error,
            message: format!("未知的指令类型 '{}'", instr_type),
            rule_name: Some(rule_name.to_string()),
            path: Some(format!("{}.type", path)),
        });
        return;
    }

    // 检查 params 字段
    let params = instr.get("params");
    if params.is_none() && instr_type != "call_rule" {
        results.push(ValidationResult {
            severity: ValidationSeverity::Warning,
            message: format!("指令 '{}' 缺少 'params' 字段", instr_type),
            rule_name: Some(rule_name.to_string()),
            path: Some(format!("{}.params", path)),
        });
        return;
    }

    // 按指令类型检查必需参数
    match instr_type {
        "set" => check_set_params(params, rule_name, path, results),
        "push" | "sequence" => check_sequence_params(params, rule_name, path, results),
        "branch" | "conditional" => check_branch_params(params, rule_name, path, results),
        "while_loop" => check_while_params(params, rule_name, path, results),
        "call_rule" => check_call_rule_params(params, rule_name, path, results),
        "io_request" => check_io_request_params(params, rule_name, path, results),
        _ => {}
    }
}

fn check_set_params(
    params: Option<&Value>,
    rule_name: &str,
    path: &str,
    results: &mut Vec<ValidationResult>,
) {
    let p = match params {
        Some(v) => v,
        None => return,
    };
    if p.get("attr").is_none() {
        results.push(ValidationResult {
            severity: ValidationSeverity::Error,
            message: "set 指令缺少 'attr' 参数".to_string(),
            rule_name: Some(rule_name.to_string()),
            path: Some(format!("{}.params.attr", path)),
        });
    }
    if p.get("value").is_none() {
        results.push(ValidationResult {
            severity: ValidationSeverity::Error,
            message: "set 指令缺少 'value' 参数".to_string(),
            rule_name: Some(rule_name.to_string()),
            path: Some(format!("{}.params.value", path)),
        });
    }
    // 检查 operation 是否合法
    if let Some(op) = p.get("operation").and_then(|v| v.as_str()) {
        if !SET_OPERATIONS.contains(&op) {
            results.push(ValidationResult {
                severity: ValidationSeverity::Error,
                message: format!("未知的 set 操作 '{}'", op),
                rule_name: Some(rule_name.to_string()),
                path: Some(format!("{}.params.operation", path)),
            });
        }
    }
}

fn check_sequence_params(
    params: Option<&Value>,
    rule_name: &str,
    path: &str,
    results: &mut Vec<ValidationResult>,
) {
    let p = match params {
        Some(v) => v,
        None => return,
    };
    match p.get("instructions") {
        Some(Value::Array(arr)) => {
            for (i, sub) in arr.iter().enumerate() {
                check_instruction(
                    sub,
                    rule_name,
                    &format!("{}.params.instructions[{}]", path, i),
                    results,
                );
            }
        }
        Some(_) => {
            results.push(ValidationResult {
                severity: ValidationSeverity::Error,
                message: "'instructions' 必须是数组".to_string(),
                rule_name: Some(rule_name.to_string()),
                path: Some(format!("{}.params.instructions", path)),
            });
        }
        None => {
            results.push(ValidationResult {
                severity: ValidationSeverity::Error,
                message: "缺少 'instructions' 参数".to_string(),
                rule_name: Some(rule_name.to_string()),
                path: Some(format!("{}.params.instructions", path)),
            });
        }
    }
}

fn check_branch_params(
    params: Option<&Value>,
    rule_name: &str,
    path: &str,
    results: &mut Vec<ValidationResult>,
) {
    let p = match params {
        Some(v) => v,
        None => return,
    };

    // 检查 domain
    if let Some(domain) = p.get("domain") {
        check_domain(
            domain,
            rule_name,
            &format!("{}.params.domain", path),
            results,
        );
    } else {
        results.push(ValidationResult {
            severity: ValidationSeverity::Error,
            message: "缺少 'domain' 参数".to_string(),
            rule_name: Some(rule_name.to_string()),
            path: Some(format!("{}.params.domain", path)),
        });
    }

    // 检查 then/on_true
    let then_key = if p.get("then").is_some() {
        "then"
    } else {
        "on_true"
    };
    if let Some(then) = p.get(then_key) {
        check_instruction(
            then,
            rule_name,
            &format!("{}.params.{}", path, then_key),
            results,
        );
    }

    // else/on_false 是可选的
    let else_key = if p.get("else").is_some() {
        "else"
    } else {
        "on_false"
    };
    if let Some(els) = p.get(else_key) {
        check_instruction(
            els,
            rule_name,
            &format!("{}.params.{}", path, else_key),
            results,
        );
    }
}

fn check_while_params(
    params: Option<&Value>,
    rule_name: &str,
    path: &str,
    results: &mut Vec<ValidationResult>,
) {
    let p = match params {
        Some(v) => v,
        None => return,
    };

    if let Some(domain) = p.get("domain") {
        check_domain(
            domain,
            rule_name,
            &format!("{}.params.domain", path),
            results,
        );
    } else {
        results.push(ValidationResult {
            severity: ValidationSeverity::Error,
            message: "while_loop 缺少 'domain' 参数".to_string(),
            rule_name: Some(rule_name.to_string()),
            path: Some(format!("{}.params.domain", path)),
        });
    }

    if let Some(body) = p.get("body") {
        check_instruction(body, rule_name, &format!("{}.params.body", path), results);
    } else {
        results.push(ValidationResult {
            severity: ValidationSeverity::Error,
            message: "while_loop 缺少 'body' 参数".to_string(),
            rule_name: Some(rule_name.to_string()),
            path: Some(format!("{}.params.body", path)),
        });
    }
}

fn check_call_rule_params(
    params: Option<&Value>,
    rule_name: &str,
    path: &str,
    results: &mut Vec<ValidationResult>,
) {
    let p = match params {
        Some(v) => v,
        None => {
            results.push(ValidationResult {
                severity: ValidationSeverity::Error,
                message: "call_rule 缺少 'params' 字段".to_string(),
                rule_name: Some(rule_name.to_string()),
                path: Some(format!("{}.params", path)),
            });
            return;
        }
    };
    if p.get("rule").is_none() {
        results.push(ValidationResult {
            severity: ValidationSeverity::Error,
            message: "call_rule 缺少 'rule' 参数".to_string(),
            rule_name: Some(rule_name.to_string()),
            path: Some(format!("{}.params.rule", path)),
        });
    }
}

fn check_io_request_params(
    params: Option<&Value>,
    rule_name: &str,
    path: &str,
    results: &mut Vec<ValidationResult>,
) {
    let p = match params {
        Some(v) => v,
        None => return,
    };
    if p.get("io_type").is_none() {
        results.push(ValidationResult {
            severity: ValidationSeverity::Error,
            message: "io_request 缺少 'io_type' 参数".to_string(),
            rule_name: Some(rule_name.to_string()),
            path: Some(format!("{}.params.io_type", path)),
        });
    }
}

/// 检查域（条件）结构
fn check_domain(domain: &Value, rule_name: &str, path: &str, results: &mut Vec<ValidationResult>) {
    let dtype = match domain.get("type").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => {
            results.push(ValidationResult {
                severity: ValidationSeverity::Error,
                message: "domain 缺少 'type' 字段".to_string(),
                rule_name: Some(rule_name.to_string()),
                path: Some(format!("{}.type", path)),
            });
            return;
        }
    };

    if !DOMAIN_TYPES.contains(&dtype) {
        results.push(ValidationResult {
            severity: ValidationSeverity::Error,
            message: format!("未知的域类型 '{}'", dtype),
            rule_name: Some(rule_name.to_string()),
            path: Some(format!("{}.type", path)),
        });
        return;
    }

    // eq/lt/gt 需要 left 和 right
    if matches!(dtype, "eq" | "lt" | "gt") {
        if domain.get("left").is_none() {
            results.push(ValidationResult {
                severity: ValidationSeverity::Error,
                message: format!("域 '{}' 缺少 'left' 参数", dtype),
                rule_name: Some(rule_name.to_string()),
                path: Some(format!("{}.left", path)),
            });
        }
        if domain.get("right").is_none() {
            results.push(ValidationResult {
                severity: ValidationSeverity::Error,
                message: format!("域 '{}' 缺少 'right' 参数", dtype),
                rule_name: Some(rule_name.to_string()),
                path: Some(format!("{}.right", path)),
            });
        }
    }

    // all/not 需要嵌套 domain
    if matches!(dtype, "all" | "not") {
        if let Some(sub) = domain.get("domain") {
            check_domain(sub, rule_name, &format!("{}.domain", path), results);
        } else if dtype == "not" {
            results.push(ValidationResult {
                severity: ValidationSeverity::Error,
                message: "域 'not' 缺少 'domain' 参数".to_string(),
                rule_name: Some(rule_name.to_string()),
                path: Some(format!("{}.domain", path)),
            });
        }
    }

    // all 可以有 domains 数组
    if dtype == "all" {
        if let Some(arr) = domain.get("domains").and_then(|v| v.as_array()) {
            for (i, sub) in arr.iter().enumerate() {
                check_domain(sub, rule_name, &format!("{}.domains[{}]", path, i), results);
            }
        }
    }
}

/// 检查规则名唯一性
fn check_rule_name_uniqueness(root: &Value, results: &mut Vec<ValidationResult>) {
    let mut seen = HashSet::new();
    let mut duplicates = HashSet::new();

    if let Some(rules) = root.get("rules").and_then(|v| v.as_array()) {
        for rule in rules {
            if let Some(name) = rule.get("name").and_then(|v| v.as_str()) {
                if !seen.insert(name.to_string()) {
                    duplicates.insert(name.to_string());
                }
            }
        }
    }

    for dup in duplicates {
        results.push(ValidationResult {
            severity: ValidationSeverity::Error,
            message: format!("规则名 '{}' 重复定义", dup),
            rule_name: Some(dup),
            path: Some("$.rules".to_string()),
        });
    }
}

/// 检查 call_rule 引用的规则是否存在
fn check_rule_references(root: &Value, results: &mut Vec<ValidationResult>) {
    let mut defined_rules = HashSet::new();
    let mut references = Vec::new();

    if let Some(rules) = root.get("rules").and_then(|v| v.as_array()) {
        for (i, rule) in rules.iter().enumerate() {
            if let Some(name) = rule.get("name").and_then(|v| v.as_str()) {
                defined_rules.insert(name.to_string());
            }
            // 收集 call_rule 引用（带 JSON 路径）
            if let Some(instr) = rule.get("instruction") {
                let base_path = format!("$.rules[{i}].instruction");
                collect_call_rule_refs(instr, &base_path, &mut references);
            }
        }
    }

    for (ref_name, path) in references {
        if !defined_rules.contains(&ref_name) {
            results.push(ValidationResult {
                severity: ValidationSeverity::Error,
                message: format!("call_rule 引用了未定义的规则 '{}'", ref_name),
                rule_name: None,
                path: Some(path),
            });
        }
    }
}

/// 递归收集 call_rule 引用（携带 JSON 路径，便于定位）
fn collect_call_rule_refs(instr: &Value, path: &str, refs: &mut Vec<(String, String)>) {
    let instr_type = match instr.get("type").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return,
    };

    if instr_type == "call_rule" {
        if let Some(name) = instr
            .get("params")
            .and_then(|p| p.get("rule"))
            .and_then(|v| v.as_str())
        {
            refs.push((name.to_string(), format!("{path}.params.rule")));
        }
    }

    // 递归子指令
    if let Some(params) = instr.get("params") {
        if let Some(arr) = params.get("instructions").and_then(|v| v.as_array()) {
            for (i, sub) in arr.iter().enumerate() {
                collect_call_rule_refs(sub, &format!("{path}.params.instructions[{i}]"), refs);
            }
        }
        if let Some(then) = params.get("then").or_else(|| params.get("on_true")) {
            let key = if params.get("then").is_some() {
                "then"
            } else {
                "on_true"
            };
            collect_call_rule_refs(then, &format!("{path}.params.{key}"), refs);
        }
        if let Some(els) = params.get("else").or_else(|| params.get("on_false")) {
            let key = if params.get("else").is_some() {
                "else"
            } else {
                "on_false"
            };
            collect_call_rule_refs(els, &format!("{path}.params.{key}"), refs);
        }
        if let Some(body) = params.get("body") {
            collect_call_rule_refs(body, &format!("{path}.params.body"), refs);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构建一个结构完整、可通过验证的规则 JSON 字符串
    fn valid_rule_json() -> String {
        r#"{
            "rule_id": "test-001",
            "version": "1.0",
            "rules": [
                {
                    "name": "main",
                    "instruction": {
                        "type": "sequence",
                        "params": {
                            "instructions": [
                                {
                                    "type": "set",
                                    "params": { "attr": "price", "operation": "set", "value": 100 }
                                },
                                {
                                    "type": "conditional",
                                    "params": {
                                        "domain": { "type": "eq", "left": "price", "right": 100 },
                                        "then": { "type": "push", "params": { "instructions": [] } }
                                    }
                                }
                            ]
                        }
                    }
                },
                {
                    "name": "helper",
                    "instruction": { "type": "call_rule", "params": { "rule": "main" } }
                }
            ]
        }"#
        .to_string()
    }

    // ===== happy path =====

    #[test]
    fn validate_valid_rule() {
        let report = validate_rule_json(&valid_rule_json());
        assert!(
            report.valid,
            "errors: {:?}",
            report
                .results
                .iter()
                .filter(|r| r.severity == ValidationSeverity::Error)
                .collect::<Vec<_>>()
        );
        assert_eq!(report.rule_id, "test-001");
        assert_eq!(report.error_count, 0);
    }

    // ===== JSON 解析 =====

    #[test]
    fn validate_invalid_json() {
        let report = validate_rule_json("{ not valid json }");
        assert!(!report.valid);
        assert_eq!(report.error_count, 1);
        assert!(report.results[0].message.contains("JSON 解析失败"));
    }

    // ===== 顶层字段 =====

    #[test]
    fn validate_missing_rule_id() {
        let json = r#"{"rules": []}"#;
        let report = validate_rule_json(json);
        assert!(!report.valid);
        assert!(report.results.iter().any(|r| r.message.contains("rule_id")));
    }

    #[test]
    fn validate_missing_rules_field_warns() {
        let json = r#"{"rule_id": "x"}"#;
        let report = validate_rule_json(json);
        // 缺少 rules → Warning, 不是 Error
        assert!(report.warning_count > 0);
        assert_eq!(report.error_count, 0);
        assert!(report.valid);
    }

    #[test]
    fn validate_rules_not_array() {
        let json = r#"{"rule_id": "x", "rules": "not-array"}"#;
        let report = validate_rule_json(json);
        assert!(!report.valid);
        assert!(report
            .results
            .iter()
            .any(|r| r.message.contains("必须是数组")));
    }

    #[test]
    fn validate_missing_version_is_info() {
        let json = r#"{"rule_id": "x", "rules": []}"#;
        let report = validate_rule_json(json);
        assert!(report.info_count > 0);
    }

    // ===== 规则定义 =====

    #[test]
    fn validate_missing_name() {
        let json = r#"{"rule_id": "x", "rules": [{"instruction": {"type": "set", "params": {"attr": "a", "value": 1}}}]}"#;
        let report = validate_rule_json(json);
        assert!(!report.valid);
        assert!(report.results.iter().any(|r| r.message.contains("name")));
    }

    #[test]
    fn validate_missing_instruction() {
        let json = r#"{"rule_id": "x", "rules": [{"name": "r"}]}"#;
        let report = validate_rule_json(json);
        assert!(!report.valid);
        assert!(report
            .results
            .iter()
            .any(|r| r.message.contains("instruction")));
    }

    // ===== 指令类型 =====

    #[test]
    fn validate_unknown_instruction_type() {
        let json = r#"{"rule_id": "x", "rules": [{"name": "r", "instruction": {"type": "unknown_type", "params": {}}}]}"#;
        let report = validate_rule_json(json);
        assert!(!report.valid);
        assert!(report
            .results
            .iter()
            .any(|r| r.message.contains("未知的指令类型")));
    }

    #[test]
    fn validate_instruction_missing_type() {
        let json = r#"{"rule_id": "x", "rules": [{"name": "r", "instruction": {"params": {}}}]}"#;
        let report = validate_rule_json(json);
        assert!(!report.valid);
        assert!(report.results.iter().any(|r| r.message.contains("type")));
    }

    // ===== set 指令 =====

    #[test]
    fn validate_set_missing_attr() {
        let json = r#"{"rule_id": "x", "rules": [{"name": "r", "instruction": {"type": "set", "params": {"value": 1}}}]}"#;
        let report = validate_rule_json(json);
        assert!(report.results.iter().any(|r| r.message.contains("attr")));
    }

    #[test]
    fn validate_set_missing_value() {
        let json = r#"{"rule_id": "x", "rules": [{"name": "r", "instruction": {"type": "set", "params": {"attr": "a"}}}]}"#;
        let report = validate_rule_json(json);
        assert!(report.results.iter().any(|r| r.message.contains("value")));
    }

    #[test]
    fn validate_set_invalid_operation() {
        let json = r#"{"rule_id": "x", "rules": [{"name": "r", "instruction": {"type": "set", "params": {"attr": "a", "value": 1, "operation": "multiply"}}}]}"#;
        let report = validate_rule_json(json);
        assert!(report
            .results
            .iter()
            .any(|r| r.message.contains("未知的 set 操作")));
    }

    #[test]
    fn validate_set_valid_operations() {
        for op in &["set", "add", "sub"] {
            let json = serde_json::json!({
                "rule_id": "x",
                "rules": [{
                    "name": "r",
                    "instruction": {
                        "type": "set",
                        "params": {"attr": "a", "value": 1, "operation": op}
                    }
                }]
            })
            .to_string();
            let report = validate_rule_json(&json);
            assert!(
                !report
                    .results
                    .iter()
                    .any(|r| r.severity == ValidationSeverity::Error && r.message.contains("操作")),
                "op={op}"
            );
        }
    }

    // ===== sequence / push =====

    #[test]
    fn validate_sequence_missing_instructions() {
        let json = r#"{"rule_id": "x", "rules": [{"name": "r", "instruction": {"type": "sequence", "params": {}}}]}"#;
        let report = validate_rule_json(json);
        assert!(report
            .results
            .iter()
            .any(|r| r.message.contains("instructions")));
    }

    #[test]
    fn validate_sequence_instructions_not_array() {
        let json = r#"{"rule_id": "x", "rules": [{"name": "r", "instruction": {"type": "sequence", "params": {"instructions": "not-array"}}}]}"#;
        let report = validate_rule_json(json);
        assert!(report
            .results
            .iter()
            .any(|r| r.message.contains("必须是数组")));
    }

    // ===== branch / conditional =====

    #[test]
    fn validate_branch_missing_domain() {
        let json = r#"{"rule_id": "x", "rules": [{"name": "r", "instruction": {"type": "conditional", "params": {"then": {"type": "set", "params": {"attr": "a", "value": 1}}}}}]}"#;
        let report = validate_rule_json(json);
        assert!(report.results.iter().any(|r| r.message.contains("domain")));
    }

    #[test]
    fn validate_conditional_with_then_else() {
        let json = r#"{"rule_id": "x", "rules": [{"name": "r", "instruction": {"type": "conditional", "params": {"domain": {"type": "eq", "left": "a", "right": 1}, "then": {"type": "set", "params": {"attr": "a", "value": 1}}, "else": {"type": "set", "params": {"attr": "a", "value": 2}}}}}]}"#;
        let report = validate_rule_json(json);
        assert!(report.valid, "errors: {:?}", report.results);
    }

    #[test]
    fn validate_branch_with_on_true_on_false() {
        let json = r#"{"rule_id": "x", "rules": [{"name": "r", "instruction": {"type": "branch", "params": {"domain": {"type": "eq", "left": "a", "right": 1}, "on_true": {"type": "set", "params": {"attr": "a", "value": 1}}, "on_false": {"type": "set", "params": {"attr": "a", "value": 2}}}}}]}"#;
        let report = validate_rule_json(json);
        assert!(report.valid, "errors: {:?}", report.results);
    }

    // ===== while_loop =====

    #[test]
    fn validate_while_missing_domain() {
        let json = serde_json::json!({
            "rule_id": "x",
            "rules": [{
                "name": "r",
                "instruction": {
                    "type": "while_loop",
                    "params": {
                        "body": {"type": "set", "params": {"attr": "a", "value": 1}}
                    }
                }
            }]
        })
        .to_string();
        let report = validate_rule_json(&json);
        assert!(report.results.iter().any(|r| r.message.contains("domain")));
    }

    #[test]
    fn validate_while_missing_body() {
        let json = serde_json::json!({
            "rule_id": "x",
            "rules": [{
                "name": "r",
                "instruction": {
                    "type": "while_loop",
                    "params": {
                        "domain": {"type": "eq", "left": "a", "right": 1}
                    }
                }
            }]
        })
        .to_string();
        let report = validate_rule_json(&json);
        assert!(report.results.iter().any(|r| r.message.contains("body")));
    }

    // ===== call_rule =====

    #[test]
    fn validate_call_rule_missing_rule_param() {
        let json = r#"{"rule_id": "x", "rules": [{"name": "r", "instruction": {"type": "call_rule", "params": {}}}]}"#;
        let report = validate_rule_json(json);
        assert!(report.results.iter().any(|r| r.message.contains("rule")));
    }

    #[test]
    fn validate_call_rule_missing_params() {
        let json =
            r#"{"rule_id": "x", "rules": [{"name": "r", "instruction": {"type": "call_rule"}}]}"#;
        let report = validate_rule_json(json);
        assert!(report.results.iter().any(|r| r.message.contains("params")));
    }

    // ===== io_request =====

    #[test]
    fn validate_io_request_missing_io_type() {
        let json = r#"{"rule_id": "x", "rules": [{"name": "r", "instruction": {"type": "io_request", "params": {}}}]}"#;
        let report = validate_rule_json(json);
        assert!(report.results.iter().any(|r| r.message.contains("io_type")));
    }

    // ===== 域类型 =====

    #[test]
    fn validate_unknown_domain_type() {
        let json = r#"{"rule_id": "x", "rules": [{"name": "r", "instruction": {"type": "conditional", "params": {"domain": {"type": "unknown_domain", "left": "a", "right": 1}}}}]}"#;
        let report = validate_rule_json(json);
        assert!(report
            .results
            .iter()
            .any(|r| r.message.contains("未知的域类型")));
    }

    #[test]
    fn validate_domain_eq_missing_left() {
        let json = r#"{"rule_id": "x", "rules": [{"name": "r", "instruction": {"type": "conditional", "params": {"domain": {"type": "eq", "right": 1}}}}]}"#;
        let report = validate_rule_json(json);
        assert!(report.results.iter().any(|r| r.message.contains("left")));
    }

    #[test]
    fn validate_domain_not_missing_domain() {
        let json = r#"{"rule_id": "x", "rules": [{"name": "r", "instruction": {"type": "conditional", "params": {"domain": {"type": "not"}}}}]}"#;
        let report = validate_rule_json(json);
        assert!(report.results.iter().any(|r| r.message.contains("domain")));
    }

    #[test]
    fn validate_domain_all_with_domains_array() {
        let json = r#"{"rule_id": "x", "rules": [{"name": "r", "instruction": {"type": "conditional", "params": {"domain": {"type": "all", "domains": [{"type": "eq", "left": "a", "right": 1}, {"type": "exists", "left": "b"}]}}}}]}"#;
        let report = validate_rule_json(json);
        assert!(report.valid, "errors: {:?}", report.results);
    }

    // ===== 唯一性 & 引用 =====

    #[test]
    fn validate_duplicate_rule_names() {
        let json = r#"{"rule_id": "x", "rules": [{"name": "dup", "instruction": {"type": "set", "params": {"attr": "a", "value": 1}}}, {"name": "dup", "instruction": {"type": "set", "params": {"attr": "b", "value": 2}}}]}"#;
        let report = validate_rule_json(json);
        assert!(report.results.iter().any(|r| r.message.contains("重复")));
    }

    #[test]
    fn validate_call_rule_undefined_ref() {
        let json = r#"{"rule_id": "x", "rules": [{"name": "r", "instruction": {"type": "call_rule", "params": {"rule": "nonexistent"}}}]}"#;
        let report = validate_rule_json(json);
        assert!(report
            .results
            .iter()
            .any(|r| r.message.contains("未定义") && r.message.contains("nonexistent")));
    }

    #[test]
    fn validate_call_rule_ref_reports_actual_path() {
        // 回归测试: 旧实现路径恒为 "<call_rule>", 应报告实际 JSON 路径
        let json = r#"{"rule_id": "x", "rules": [{"name": "r", "instruction": {"type": "sequence", "params": {"instructions": [{"type": "call_rule", "params": {"rule": "missing"}}]}}}]}"#;
        let report = validate_rule_json(json);
        let ref_result = report
            .results
            .iter()
            .find(|r| r.message.contains("missing"));
        assert!(ref_result.is_some(), "应报告 undefined ref");
        let path = ref_result.unwrap().path.as_deref().unwrap_or("");
        assert!(path.contains("$.rules[0].instruction"), "path={path}");
        assert!(path.contains("instructions[0]"), "path={path}");
        assert!(path.contains("params.rule"), "path={path}");
        assert!(!path.contains("<call_rule>"), "不应是占位符, path={path}");
    }

    #[test]
    fn validate_call_rule_valid_ref_no_error() {
        let json = r#"{"rule_id": "x", "rules": [{"name": "a", "instruction": {"type": "call_rule", "params": {"rule": "b"}}}, {"name": "b", "instruction": {"type": "set", "params": {"attr": "x", "value": 1}}}]}"#;
        let report = validate_rule_json(json);
        assert!(!report
            .results
            .iter()
            .any(|r| r.severity == ValidationSeverity::Error && r.message.contains("未定义")));
    }

    // ===== 嵌套 =====

    #[test]
    fn validate_deeply_nested_instructions() {
        // 3 层 sequence 嵌套
        let json = r#"{"rule_id": "x", "rules": [{"name": "r", "instruction": {"type": "sequence", "params": {"instructions": [{"type": "sequence", "params": {"instructions": [{"type": "sequence", "params": {"instructions": [{"type": "set", "params": {"attr": "a", "value": 1}}]}}]}}]}}}]}"#;
        let report = validate_rule_json(json);
        assert!(report.valid, "errors: {:?}", report.results);
    }
}

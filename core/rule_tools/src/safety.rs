// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 规则安全分析 —— 静态风险评估
//!
//! 检测规则的潜在风险：
//! - 死循环（while_loop 无终止条件）
//! - 无限 I/O（io_request 在循环中）
//! - 未受限 payload 增长（push 在循环中）
//! - 递归 call_rule（自递归 / 间接循环）
//! - 深度嵌套（超过阈值）

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// 安全严重级别
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetySeverity {
    Critical,
    High,
    Medium,
    Low,
}

/// 单条安全问题
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyIssue {
    pub severity: SafetySeverity,
    pub category: String,
    pub message: String,
    pub rule_name: Option<String>,
    pub path: Option<String>,
}

/// 安全报告
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyReport {
    pub safe: bool,
    pub rule_id: String,
    pub critical_count: usize,
    pub high_count: usize,
    pub medium_count: usize,
    pub low_count: usize,
    pub issues: Vec<SafetyIssue>,
}

/// 最大嵌套深度阈值
const MAX_NESTING_DEPTH: usize = 10;

/// 分析规则 JSON 的安全风险
pub fn analyze_rule_safety(content: &str) -> SafetyReport {
    let root: Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(_) => {
            return SafetyReport {
                safe: false,
                rule_id: "<unknown>".to_string(),
                critical_count: 1,
                high_count: 0,
                medium_count: 0,
                low_count: 0,
                issues: vec![SafetyIssue {
                    severity: SafetySeverity::Critical,
                    category: "parse_error".to_string(),
                    message: "JSON 解析失败，无法分析".to_string(),
                    rule_name: None,
                    path: None,
                }],
            };
        }
    };

    let mut issues = Vec::new();
    let rule_id = root
        .get("rule_id")
        .and_then(|v| v.as_str())
        .unwrap_or("<unknown>")
        .to_string();

    // 收集所有规则定义
    let mut defined_rules = HashSet::new();
    if let Some(rules) = root.get("rules").and_then(|v| v.as_array()) {
        for rule in rules {
            if let Some(name) = rule.get("name").and_then(|v| v.as_str()) {
                defined_rules.insert(name.to_string());
            }
        }
    }

    // 分析每条规则
    if let Some(rules) = root.get("rules").and_then(|v| v.as_array()) {
        for (i, rule) in rules.iter().enumerate() {
            let rule_name = rule
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("<unnamed>")
                .to_string();
            let base_path = format!("$.rules[{i}]");

            if let Some(instr) = rule.get("instruction") {
                // 检查 while_loop 风险
                check_while_loop_risks(
                    instr,
                    &rule_name,
                    &format!("{base_path}.instruction"),
                    &mut issues,
                );

                // 检查嵌套深度
                check_nesting_depth(
                    instr,
                    &rule_name,
                    &format!("{base_path}.instruction"),
                    0,
                    &mut issues,
                );

                // 检查 call_rule 自递归与未定义引用
                check_call_rule_recursion(
                    instr,
                    &rule_name,
                    &format!("{base_path}.instruction"),
                    &defined_rules,
                    &mut issues,
                );
            }
        }
    }

    // 检查间接循环 call_rule（A→B→A）
    // 依赖完整 call graph，需在遍历所有规则之后进行。
    check_call_graph_cycles(&root, &mut issues);

    let critical_count = issues
        .iter()
        .filter(|i| i.severity == SafetySeverity::Critical)
        .count();
    let high_count = issues
        .iter()
        .filter(|i| i.severity == SafetySeverity::High)
        .count();
    let medium_count = issues
        .iter()
        .filter(|i| i.severity == SafetySeverity::Medium)
        .count();
    let low_count = issues
        .iter()
        .filter(|i| i.severity == SafetySeverity::Low)
        .count();

    SafetyReport {
        safe: critical_count == 0 && high_count == 0,
        rule_id,
        critical_count,
        high_count,
        medium_count,
        low_count,
        issues,
    }
}

/// 检查 while_loop 相关风险
fn check_while_loop_risks(
    instr: &Value,
    rule_name: &str,
    path: &str,
    issues: &mut Vec<SafetyIssue>,
) {
    let instr_type = match instr.get("type").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return,
    };

    if instr_type == "while_loop" {
        // 检查 body 中是否有 io_request（可能导致无限 I/O）
        if let Some(body) = instr.get("params").and_then(|p| p.get("body")) {
            if contains_instruction_type(body, "io_request") {
                issues.push(SafetyIssue {
                    severity: SafetySeverity::High,
                    category: "infinite_io".to_string(),
                    message: "while_loop 体中包含 io_request，可能导致无限 I/O".to_string(),
                    rule_name: Some(rule_name.to_string()),
                    path: Some(path.to_string()),
                });
            }

            // 检查 body 中是否有 push（可能导致队列无限增长）
            if contains_instruction_type(body, "push") {
                issues.push(SafetyIssue {
                    severity: SafetySeverity::Medium,
                    category: "unbounded_growth".to_string(),
                    message: "while_loop 体中包含 push，可能导致队列无限增长".to_string(),
                    rule_name: Some(rule_name.to_string()),
                    path: Some(path.to_string()),
                });
            }
        }

        // 检查 domain 是否为常量 true（可能死循环）
        if let Some(domain) = instr.get("params").and_then(|p| p.get("domain")) {
            if is_constant_true_domain(domain) {
                issues.push(SafetyIssue {
                    severity: SafetySeverity::Critical,
                    category: "dead_loop".to_string(),
                    message: "while_loop 的 domain 恒为真，将导致死循环".to_string(),
                    rule_name: Some(rule_name.to_string()),
                    path: Some(format!("{path}.params.domain")),
                });
            }
        } else {
            issues.push(SafetyIssue {
                severity: SafetySeverity::High,
                category: "missing_condition".to_string(),
                message: "while_loop 缺少 domain 条件".to_string(),
                rule_name: Some(rule_name.to_string()),
                path: Some(format!("{path}.params.domain")),
            });
        }
    }

    // 递归检查子指令
    if let Some(params) = instr.get("params") {
        for key in &["then", "on_true", "else", "on_false", "body"] {
            if let Some(sub) = params.get(key) {
                check_while_loop_risks(sub, rule_name, &format!("{path}.params.{key}"), issues);
            }
        }
        if let Some(arr) = params.get("instructions").and_then(|v| v.as_array()) {
            for (i, sub) in arr.iter().enumerate() {
                check_while_loop_risks(
                    sub,
                    rule_name,
                    &format!("{path}.params.instructions[{i}]"),
                    issues,
                );
            }
        }
    }
}

/// 检查嵌套深度
fn check_nesting_depth(
    instr: &Value,
    rule_name: &str,
    path: &str,
    depth: usize,
    issues: &mut Vec<SafetyIssue>,
) {
    if depth > MAX_NESTING_DEPTH {
        issues.push(SafetyIssue {
            severity: SafetySeverity::Medium,
            category: "deep_nesting".to_string(),
            message: format!("指令嵌套深度 {depth} 超过阈值 {MAX_NESTING_DEPTH}"),
            rule_name: Some(rule_name.to_string()),
            path: Some(path.to_string()),
        });
        return;
    }

    // 指令缺少 type 字段则无法判断类型，直接返回
    let instr_type = match instr.get("type").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return,
    };

    // call_rule 是引用而非嵌套，不增加深度
    if instr_type == "call_rule" {
        return;
    }

    // 递归子指令并增加深度
    if let Some(params) = instr.get("params") {
        for key in &["then", "on_true", "else", "on_false", "body"] {
            if let Some(sub) = params.get(key) {
                check_nesting_depth(
                    sub,
                    rule_name,
                    &format!("{path}.params.{key}"),
                    depth + 1,
                    issues,
                );
            }
        }

        if let Some(arr) = params.get("instructions").and_then(|v| v.as_array()) {
            for (i, sub) in arr.iter().enumerate() {
                check_nesting_depth(
                    sub,
                    rule_name,
                    &format!("{path}.params.instructions[{i}]"),
                    depth + 1,
                    issues,
                );
            }
        }
    }
}

/// 检查 call_rule 自递归与未定义引用
///
/// 仅检测**直接**自递归（rule A 的 instruction 中 call_rule A）和
/// 引用未定义规则。间接循环（A→B→A）由 `check_call_graph_cycles` 负责。
fn check_call_rule_recursion(
    instr: &Value,
    current_rule: &str,
    path: &str,
    defined_rules: &HashSet<String>,
    issues: &mut Vec<SafetyIssue>,
) {
    let instr_type = match instr.get("type").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return,
    };

    if instr_type == "call_rule" {
        if let Some(target) = instr
            .get("params")
            .and_then(|p| p.get("rule"))
            .and_then(|v| v.as_str())
        {
            // 检查自引用
            if target == current_rule {
                issues.push(SafetyIssue {
                    severity: SafetySeverity::Critical,
                    category: "self_recursion".to_string(),
                    message: format!("规则 '{current_rule}' 直接递归调用自身"),
                    rule_name: Some(current_rule.to_string()),
                    path: Some(path.to_string()),
                });
                return;
            }

            // 检查引用未定义的规则
            if !defined_rules.contains(target) {
                issues.push(SafetyIssue {
                    severity: SafetySeverity::Medium,
                    category: "undefined_reference".to_string(),
                    message: format!("call_rule 引用未定义的规则 '{target}'"),
                    rule_name: Some(current_rule.to_string()),
                    path: Some(path.to_string()),
                });
            }
        }
    }

    // 递归子指令
    if let Some(params) = instr.get("params") {
        for key in &["then", "on_true", "else", "on_false", "body"] {
            if let Some(sub) = params.get(key) {
                check_call_rule_recursion(
                    sub,
                    current_rule,
                    &format!("{path}.params.{key}"),
                    defined_rules,
                    issues,
                );
            }
        }
        if let Some(arr) = params.get("instructions").and_then(|v| v.as_array()) {
            for (i, sub) in arr.iter().enumerate() {
                check_call_rule_recursion(
                    sub,
                    current_rule,
                    &format!("{path}.params.instructions[{i}]"),
                    defined_rules,
                    issues,
                );
            }
        }
    }
}

/// 构建调用图并检测间接循环 call_rule（A→B→A）
///
/// 对每条规则做 DFS，检查是否存在从自身出发又回到自身的调用链。
/// 直接自递归已由 `check_call_rule_recursion` 检测，此处跳过。
fn check_call_graph_cycles(root: &Value, issues: &mut Vec<SafetyIssue>) {
    // 构建 call graph: rule_name → 被调用的规则名列表
    let mut call_graph: HashMap<String, Vec<String>> = HashMap::new();
    if let Some(rules) = root.get("rules").and_then(|v| v.as_array()) {
        for rule in rules {
            let name = rule
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("<unnamed>")
                .to_string();
            let mut calls = Vec::new();
            if let Some(instr) = rule.get("instruction") {
                collect_call_rule_names(instr, &mut calls);
            }
            call_graph.entry(name).or_default().extend(calls);
        }
    }

    // 对每条规则，检查是否能通过 call_rule 链回到自身
    for start in call_graph.keys() {
        let mut visited = HashSet::new();
        if can_reach_self(&call_graph, start, start, &mut visited) {
            issues.push(SafetyIssue {
                severity: SafetySeverity::High,
                category: "circular_recursion".to_string(),
                message: format!("规则 '{start}' 存在循环 call_rule 引用（可通过调用链回到自身）"),
                rule_name: Some(start.clone()),
                path: None,
            });
        }
    }
}

/// 递归收集指令树中所有 call_rule 的目标规则名
fn collect_call_rule_names(instr: &Value, calls: &mut Vec<String>) {
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
            calls.push(name.to_string());
        }
    }

    if let Some(params) = instr.get("params") {
        for key in &["then", "on_true", "else", "on_false", "body"] {
            if let Some(sub) = params.get(key) {
                collect_call_rule_names(sub, calls);
            }
        }
        if let Some(arr) = params.get("instructions").and_then(|v| v.as_array()) {
            for sub in arr {
                collect_call_rule_names(sub, calls);
            }
        }
    }
}

/// 检查从 `current` 出发是否可达 `target`（排除 direct self-loop: current==target 的首跳）
///
/// `visited` 防止搜索过程中无限循环（自身就是循环的图也会终止）。
fn can_reach_self(
    graph: &HashMap<String, Vec<String>>,
    target: &str,
    current: &str,
    visited: &mut HashSet<String>,
) -> bool {
    let Some(callees) = graph.get(current) else {
        return false;
    };
    for callee in callees {
        // 首跳不判定 direct self-loop（current==target 时 callee==target 是自递归，
        // 已由 check_call_rule_recursion 以 Critical 级别报告）
        if callee == target && current != target {
            return true;
        }
        if visited.insert(callee.clone()) && can_reach_self(graph, target, callee, visited) {
            return true;
        }
    }
    false
}

/// 检查指令树中是否包含指定类型的指令
fn contains_instruction_type(instr: &Value, target_type: &str) -> bool {
    let instr_type = match instr.get("type").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return false,
    };

    if instr_type == target_type {
        return true;
    }

    if let Some(params) = instr.get("params") {
        for key in &["then", "on_true", "else", "on_false", "body"] {
            if let Some(sub) = params.get(key) {
                if contains_instruction_type(sub, target_type) {
                    return true;
                }
            }
        }
        if let Some(arr) = params.get("instructions").and_then(|v| v.as_array()) {
            for sub in arr {
                if contains_instruction_type(sub, target_type) {
                    return true;
                }
            }
        }
    }

    false
}

/// 检查 domain 是否恒为真
///
/// 目前仅检测最明显的常量真模式：`eq` 且 `left == right`。
/// 不覆盖 `all` 空数组等边缘场景——保守策略，宁可漏报不可误报。
fn is_constant_true_domain(domain: &Value) -> bool {
    if let Some(dtype) = domain.get("type").and_then(|v| v.as_str()) {
        if dtype == "eq" {
            // eq 比较两个常量
            let left = domain.get("left");
            let right = domain.get("right");
            if left.is_some() && right.is_some() && left == right {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== happy path =====

    #[test]
    fn safety_safe_rule() {
        let json = serde_json::json!({
            "rule_id": "safe-001",
            "rules": [{
                "name": "main",
                "instruction": {
                    "type": "sequence",
                    "params": {
                        "instructions": [
                            {"type": "set", "params": {"attr": "x", "value": 1}},
                            {"type": "io_request", "params": {"io_type": "http"}}
                        ]
                    }
                }
            }]
        })
        .to_string();
        let report = analyze_rule_safety(&json);
        assert!(report.safe, "issues: {:?}", report.issues);
        assert_eq!(report.rule_id, "safe-001");
    }

    #[test]
    fn safety_empty_rules_is_safe() {
        let json = r#"{"rule_id": "empty", "rules": []}"#;
        let report = analyze_rule_safety(json);
        assert!(report.safe);
    }

    // ===== parse error =====

    #[test]
    fn safety_parse_error() {
        let report = analyze_rule_safety("{ invalid json }");
        assert!(!report.safe);
        assert_eq!(report.critical_count, 1);
        assert_eq!(report.issues[0].category, "parse_error");
    }

    // ===== while_loop 风险 =====

    #[test]
    fn safety_while_io_request_in_body() {
        let json = serde_json::json!({
            "rule_id": "r1",
            "rules": [{
                "name": "loop",
                "instruction": {
                    "type": "while_loop",
                    "params": {
                        "domain": {"type": "eq", "left": "x", "right": 1},
                        "body": {"type": "io_request", "params": {"io_type": "http"}}
                    }
                }
            }]
        })
        .to_string();
        let report = analyze_rule_safety(&json);
        assert!(report
            .issues
            .iter()
            .any(|i| i.category == "infinite_io" && i.severity == SafetySeverity::High));
    }

    #[test]
    fn safety_while_push_in_body() {
        let json = serde_json::json!({
            "rule_id": "r1",
            "rules": [{
                "name": "loop",
                "instruction": {
                    "type": "while_loop",
                    "params": {
                        "domain": {"type": "eq", "left": "x", "right": 1},
                        "body": {"type": "push", "params": {"instructions": []}}
                    }
                }
            }]
        })
        .to_string();
        let report = analyze_rule_safety(&json);
        assert!(report
            .issues
            .iter()
            .any(|i| i.category == "unbounded_growth" && i.severity == SafetySeverity::Medium));
    }

    #[test]
    fn safety_while_constant_true_domain() {
        let json = serde_json::json!({
            "rule_id": "r1",
            "rules": [{
                "name": "loop",
                "instruction": {
                    "type": "while_loop",
                    "params": {
                        "domain": {"type": "eq", "left": 1, "right": 1},
                        "body": {"type": "set", "params": {"attr": "x", "value": 1}}
                    }
                }
            }]
        })
        .to_string();
        let report = analyze_rule_safety(&json);
        assert!(!report.safe);
        assert!(report
            .issues
            .iter()
            .any(|i| i.category == "dead_loop" && i.severity == SafetySeverity::Critical));
    }

    #[test]
    fn safety_while_missing_domain() {
        let json = serde_json::json!({
            "rule_id": "r1",
            "rules": [{
                "name": "loop",
                "instruction": {
                    "type": "while_loop",
                    "params": {
                        "body": {"type": "set", "params": {"attr": "x", "value": 1}}
                    }
                }
            }]
        })
        .to_string();
        let report = analyze_rule_safety(&json);
        assert!(!report.safe);
        assert!(report
            .issues
            .iter()
            .any(|i| i.category == "missing_condition" && i.severity == SafetySeverity::High));
    }

    #[test]
    fn safety_io_request_outside_while_is_safe() {
        let json = serde_json::json!({
            "rule_id": "r1",
            "rules": [{
                "name": "main",
                "instruction": {
                    "type": "sequence",
                    "params": {
                        "instructions": [
                            {"type": "io_request", "params": {"io_type": "http"}},
                            {"type": "push", "params": {"instructions": []}}
                        ]
                    }
                }
            }]
        })
        .to_string();
        let report = analyze_rule_safety(&json);
        assert!(report.safe, "issues: {:?}", report.issues);
    }

    // ===== 嵌套深度 =====

    #[test]
    fn safety_deep_nesting_warns() {
        // 构建超过 MAX_NESTING_DEPTH (10) 层嵌套的 sequence
        let mut inner = serde_json::json!({"type": "set", "params": {"attr": "x", "value": 1}});
        for _ in 0..12 {
            inner = serde_json::json!({
                "type": "sequence",
                "params": {"instructions": [inner]}
            });
        }
        let json = serde_json::json!({
            "rule_id": "deep",
            "rules": [{"name": "r", "instruction": inner}]
        })
        .to_string();
        let report = analyze_rule_safety(&json);
        assert!(report
            .issues
            .iter()
            .any(|i| i.category == "deep_nesting" && i.severity == SafetySeverity::Medium));
    }

    #[test]
    fn safety_shallow_nesting_no_warning() {
        let json = serde_json::json!({
            "rule_id": "shallow",
            "rules": [{
                "name": "r",
                "instruction": {
                    "type": "sequence",
                    "params": {
                        "instructions": [
                            {"type": "sequence", "params": {"instructions": [
                                {"type": "set", "params": {"attr": "x", "value": 1}}
                            ]}}
                        ]
                    }
                }
            }]
        })
        .to_string();
        let report = analyze_rule_safety(&json);
        assert!(!report.issues.iter().any(|i| i.category == "deep_nesting"));
    }

    // ===== call_rule 递归 =====

    #[test]
    fn safety_self_recursion_detected() {
        let json = serde_json::json!({
            "rule_id": "rec",
            "rules": [{
                "name": "r",
                "instruction": {"type": "call_rule", "params": {"rule": "r"}}
            }]
        })
        .to_string();
        let report = analyze_rule_safety(&json);
        assert!(!report.safe);
        assert!(report
            .issues
            .iter()
            .any(|i| i.category == "self_recursion" && i.severity == SafetySeverity::Critical));
    }

    #[test]
    fn safety_circular_recursion_detected() {
        // 回归测试: 旧实现 visited 集合从未填充, 间接循环 (A→B→A) 无法被检测
        let json = serde_json::json!({
            "rule_id": "cycle",
            "rules": [
                {"name": "a", "instruction": {"type": "call_rule", "params": {"rule": "b"}}},
                {"name": "b", "instruction": {"type": "call_rule", "params": {"rule": "a"}}}
            ]
        })
        .to_string();
        let report = analyze_rule_safety(&json);
        assert!(
            !report.safe,
            "应检测到间接循环递归, issues: {:?}",
            report.issues
        );
        assert!(report
            .issues
            .iter()
            .any(|i| i.category == "circular_recursion" && i.severity == SafetySeverity::High));
    }

    #[test]
    fn safety_circular_recursion_three_way() {
        // A→B→C→A 三节点循环
        let json = serde_json::json!({
            "rule_id": "cycle3",
            "rules": [
                {"name": "a", "instruction": {"type": "call_rule", "params": {"rule": "b"}}},
                {"name": "b", "instruction": {"type": "call_rule", "params": {"rule": "c"}}},
                {"name": "c", "instruction": {"type": "call_rule", "params": {"rule": "a"}}}
            ]
        })
        .to_string();
        let report = analyze_rule_safety(&json);
        assert!(
            !report.safe,
            "应检测到三节点循环, issues: {:?}",
            report.issues
        );
        assert!(report
            .issues
            .iter()
            .any(|i| i.category == "circular_recursion"));
    }

    #[test]
    fn safety_no_false_positive_on_call_chain() {
        // A→B→C 非循环链, 不应报告 circular_recursion
        let json = serde_json::json!({
            "rule_id": "chain",
            "rules": [
                {"name": "a", "instruction": {"type": "call_rule", "params": {"rule": "b"}}},
                {"name": "b", "instruction": {"type": "call_rule", "params": {"rule": "c"}}},
                {"name": "c", "instruction": {"type": "set", "params": {"attr": "x", "value": 1}}}
            ]
        })
        .to_string();
        let report = analyze_rule_safety(&json);
        assert!(
            !report
                .issues
                .iter()
                .any(|i| i.category == "circular_recursion"),
            "不应误报循环, issues: {:?}",
            report.issues
        );
    }

    #[test]
    fn safety_undefined_call_rule_reference() {
        let json = serde_json::json!({
            "rule_id": "undef",
            "rules": [{
                "name": "r",
                "instruction": {"type": "call_rule", "params": {"rule": "nonexistent"}}
            }]
        })
        .to_string();
        let report = analyze_rule_safety(&json);
        assert!(report
            .issues
            .iter()
            .any(|i| i.category == "undefined_reference" && i.severity == SafetySeverity::Medium));
    }

    #[test]
    fn safety_nested_while_in_sequence() {
        // while_loop 嵌套在 sequence 中, 仍应检测到风险
        let json = serde_json::json!({
            "rule_id": "nested",
            "rules": [{
                "name": "r",
                "instruction": {
                    "type": "sequence",
                    "params": {
                        "instructions": [{
                            "type": "while_loop",
                            "params": {
                                "domain": {"type": "eq", "left": 1, "right": 1},
                                "body": {"type": "io_request", "params": {"io_type": "http"}}
                            }
                        }]
                    }
                }
            }]
        })
        .to_string();
        let report = analyze_rule_safety(&json);
        assert!(
            report.issues.iter().any(|i| i.category == "dead_loop"),
            "应检测到死循环"
        );
        assert!(
            report.issues.iter().any(|i| i.category == "infinite_io"),
            "应检测到无限 I/O"
        );
    }
}

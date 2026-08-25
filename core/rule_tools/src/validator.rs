// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 规则验证器 —— Schema 驱动校验（线1 防御层）
//!
//! 以固化的 evorule-system-rules v1.0 Schema 为权威基准（SSOT），
//! 校验规则 JSON 是否符合引擎原生结构（transform[]）。
//!
//! # 定位
//! - TCB（evorule-tcb）不设防：只保证确定性执行，不保证用户规则正确性。
//! - 本模块是 evorule-server 侧的防御层（records/77 边界与防御原则），
//!   用固化 Schema 拦截结构非法的规则并给出明确提示。
//! - Schema 校验委托 `evorule_rule_schema` crate（内嵌 schemas/ 三个文件，
//!   与 evorule-system-rules 仓同步，跨仓一致性由 scripts/check_schema_sync.py 守护）。
//!
//! # 兼容输入形态
//! - `{ "transform": [...] }` — 完整 rule_set / core_eval 文档
//! - `[...]` — 裸 transform 数组
//! - `{...}` — 单条 transform 对象

use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    /// JSON 路径（如 `transform[0].params.attr`）
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

/// 验证规则 JSON 文件内容（Schema 门禁，权威基准）
///
/// 内部实现：解析 JSON → `evorule_rule_schema::validate_rule_input` →
/// 将 SchemaReport 错误转为 `ValidationResult`（Error 级别）。
pub fn validate_rule_json(content: &str) -> ValidationReport {
    // 1. JSON 解析
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
                    message: format!("JSON 解析失败: {e}"),
                    rule_name: None,
                    path: None,
                }],
            };
        }
    };

    // 2. 提取规则标识（rule_set 文档用 id，旧格式兼容 rule_id）
    let rule_id = root
        .get("id")
        .or_else(|| root.get("rule_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("<unknown>")
        .to_string();

    // 3. Schema 门禁（权威基准，线1 拦截）
    // 兼容单条 transform 对象（文档声明的输入形态 `{...}`）：与 server API 一致，
    // 包装为 transform 数组再校验；其余形态交给 validate_rule_input。
    let schema_report = if root.is_object() && root.get("transform").is_none() {
        evorule_rule_schema::validate_transform_list(&serde_json::Value::Array(vec![root.clone()]))
    } else {
        evorule_rule_schema::validate_rule_input(&root)
    };

    // 4. 转换为统一报告结构
    let results: Vec<ValidationResult> = if schema_report.valid {
        Vec::new()
    } else {
        schema_report
            .errors
            .iter()
            .map(|err| {
                // SchemaReport 错误已含实例路径（如 "/transform/0"），
                // 拆分出 path 前缀与原因供展示。
                let (path, message) = match err.split_once(": ") {
                    Some((p, m)) => (Some(p.to_string()), m.to_string()),
                    None => (None, err.clone()),
                };
                ValidationResult {
                    severity: ValidationSeverity::Error,
                    message,
                    rule_name: None,
                    path,
                }
            })
            .collect()
    };

    let error_count = results
        .iter()
        .filter(|r| r.severity == ValidationSeverity::Error)
        .count();

    ValidationReport {
        valid: error_count == 0,
        rule_id,
        error_count,
        warning_count: 0,
        info_count: 0,
        results,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构建一个结构完整、可通过 Schema 门禁的引擎原生规则 JSON 字符串
    fn valid_rule_json() -> String {
        r#"{
            "id": "com.evorule.test.x",
            "version": "0.1.0",
            "transform": [
                {
                    "type": "set",
                    "params": { "attr": "x", "operation": "set", "value": 42 }
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
        assert_eq!(report.rule_id, "com.evorule.test.x");
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

    // ===== 输入形态兼容 =====

    #[test]
    fn validate_bare_transform_array() {
        let json = r#"[{"type":"set","params":{"attr":"x","operation":"set","value":1}}]"#;
        let report = validate_rule_json(json);
        assert!(report.valid, "裸 transform 数组应通过: {:?}", report.results);
    }

    #[test]
    fn validate_single_transform_object() {
        // 单对象视为单条 transform（与 governance extract_transforms 一致）
        let json = r#"{"type":"set","params":{"attr":"x","operation":"set","value":1}}"#;
        let report = validate_rule_json(json);
        assert!(report.valid, "单条 transform 应通过: {:?}", report.results);
    }

    #[test]
    fn validate_non_object_non_array_rejected() {
        // 数字既不是对象也不是数组 → 无法识别
        let report = validate_rule_json("12345");
        assert!(!report.valid);
        assert!(!report.results.is_empty());
    }

    // ===== 引擎原生结构（transform[]） =====

    #[test]
    fn validate_empty_transform_array() {
        let json = r#"{"transform":[]}"#;
        let report = validate_rule_json(json);
        assert!(!report.valid, "空 transform 数组应被拒（TCB 非空约束）");
    }

    #[test]
    fn validate_unknown_transform_type() {
        // noop 是指令层类型，不是元指令层 transform 类型（P0-01）
        let json = r#"{"transform":[{"type":"noop"}]}"#;
        let report = validate_rule_json(json);
        assert!(!report.valid);
        assert!(report.results.iter().any(|r| r.message.contains("noop")));
    }

    #[test]
    fn validate_transform_missing_type() {
        let json = r#"{"transform":[{"params":{"attr":"x"}}]}"#;
        let report = validate_rule_json(json);
        assert!(!report.valid);
        assert!(report.results.iter().any(|r| r.message.contains("type")));
    }

    #[test]
    fn validate_set_missing_value() {
        // 引擎 exec_set 对缺失 value 报 MissingField
        let json = r#"{"transform":[{"type":"set","params":{"attr":"x","operation":"set"}}]}"#;
        let report = validate_rule_json(json);
        assert!(!report.valid);
        assert!(report.results.iter().any(|r| r.message.contains("value")));
    }

    #[test]
    fn validate_set_missing_attr() {
        let json =
            r#"{"transform":[{"type":"set","params":{"operation":"set","value":1}}]}"#;
        let report = validate_rule_json(json);
        assert!(!report.valid);
        assert!(report.results.iter().any(|r| r.message.contains("attr")));
    }

    #[test]
    fn validate_branch_missing_domain() {
        // branch 缺少必填参数 domain（保留 params 以让 Schema 报出精确字段错误）
        let json = r#"{"transform":[{"type":"branch","params":{"on_true":[]}}]}"#;
        let report = validate_rule_json(json);
        assert!(!report.valid);
        assert!(report.results.iter().any(|r| r.message.contains("domain")));
    }

    #[test]
    fn validate_branch_missing_on_true() {
        // branch 必填 domain + on_true
        let json = r#"{"transform":[{"type":"branch","params":{"domain":{"type":"all","inner":[]}}}]}"#;
        let report = validate_rule_json(json);
        assert!(!report.valid);
        assert!(report.results.iter().any(|r| r.message.contains("on_true")));
    }

    #[test]
    fn validate_io_request_missing_io_type() {
        // io_request 缺少必填参数 io_type（保留 params 以让 Schema 报出精确字段错误）
        let json = r#"{"transform":[{"type":"io_request","params":{}}]}"#;
        let report = validate_rule_json(json);
        assert!(!report.valid);
        assert!(report.results.iter().any(|r| r.message.contains("io_type")));
    }

    #[test]
    fn validate_push_missing_instructions() {
        // push 缺少必填参数 instructions（保留 params 以让 Schema 报出精确字段错误）
        let json = r#"{"transform":[{"type":"push","params":{}}]}"#;
        let report = validate_rule_json(json);
        assert!(!report.valid);
        assert!(report
            .results
            .iter()
            .any(|r| r.message.contains("instructions")));
    }

    #[test]
    fn validate_domain_string_without_prefix_rejected() {
        // domain 字符串无 __ 前缀应被拒（运行时报 MissingField）
        let json = r#"{"transform":[{"type":"branch","params":{"domain":"payload.flag","on_true":[]}}]}"#;
        let report = validate_rule_json(json);
        assert!(!report.valid);
    }

    #[test]
    fn validate_nested_branch_valid() {
        // 嵌套 branch（on_true 数组内再套 branch）应通过
        let json = r#"{
            "transform": [{
                "type": "branch",
                "params": {
                    "domain": { "type": "all", "inner": [] },
                    "on_true": [{
                        "type": "branch",
                        "params": {
                            "domain": { "type": "all", "inner": [] },
                            "on_true": [
                                { "type": "set", "params": { "attr": "x", "operation": "set", "value": 1 } }
                            ]
                        }
                    }]
                }
            }]
        }"#;
        let report = validate_rule_json(json);
        assert!(report.valid, "嵌套 branch 应通过: {:?}", report.results);
    }

    #[test]
    fn validate_domain_uses_inner_not_domains() {
        // P0-03: domain 嵌套一律用 inner，禁止 domain/domains 字段
        let json = r#"{"transform":[{"type":"branch","params":{"domain":{"type":"all","domains":[]},"on_true":[]}}]}"#;
        let report = validate_rule_json(json);
        assert!(
            !report.valid,
            "all 缺 inner（用了已废弃的 domains）应被拒: {:?}",
            report.results
        );
    }

    #[test]
    fn validate_results_report_path() {
        // SchemaReport 错误应带实例路径（/transform/0/...）
        let json = r#"{"transform":[{"type":"set","params":{"attr":"x","operation":"set"}}]}"#;
        let report = validate_rule_json(json);
        assert!(!report.valid);
        let result = &report.results[0];
        assert!(result.path.is_some(), "应有实例路径: {:?}", result);
    }
}

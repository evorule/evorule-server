// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 四段式报错 formatter（[CON-K2-001]/[CON-K2-002]/[CON-K1-002]，SR-15 件5）
//!
//! 报文形态（25 号档 §3.2 规格）：
//!
//! ```text
//! [宪法校验失败 / <门标签>]
//! ① 违什么：<validator 报告 + schema 关键字>
//! ② 在哪：<JSON 路径>（第 N 行附近；尽力定位，缺省 -）
//! ③ 正本：https://evorule.org/schemas/...
//! ④ 正确形态（最小可过）：
//!    {...}
//! → 导览：<仓内路径> | <权威源 URL>
//! ```
//!
//! [CON-K2-002] 最小样例表：按错误类别取最小可过形态（与宪法仓
//! acceptance/golden/ 金样例同源对齐，不另养副本）；表内全部样例经
//! `validate_rule_json` 自检（模板测试在场，缺任一段=红灯）。
//!
//! 消费面（三处，[CON-L1-002] 同源）：db.rs 持久层写入门 /
//! rule_meta_service validate_content / CLI validate。

use crate::validator::{ValidationReport, ValidationResult, ValidationSeverity};

/// [CON-K1-002] 导览常量：仓内路径 + Gitee 权威源 URL
pub const GUIDE_REF: &str =
    "docs/guide.md（规则作者节）| https://gitee.com/evorule/evorule-system-rules#docs";

/// ③ 正本链接基准（rule_set v1.0 = 本校验器权威基准 SSOT）
pub const SCHEMA_REFERENCE_URL: &str = "https://evorule.org/schemas/rule_set/v1.0.json";

/// _shared def 锚链接基准（domain/instruction 等共享定义所在）
const SHARED_DEFS_URL: &str = "https://evorule.org/schemas/_shared/v1.0.json";

// —— [CON-K2-002] 最小可过样例表（形态=引擎原生 transform 条目）——
const MINIMAL_SET: &str = r#"{"type":"set","params":{"attr":"x","operation":"set","value":1}}"#;
const MINIMAL_BRANCH: &str =
    r#"{"type":"branch","params":{"domain":{"type":"all","inner":[]},"on_true":[]}}"#;
const MINIMAL_PUSH: &str =
    r#"{"type":"push","params":{"instructions":[{"type":"set","params":{"attr":"x","operation":"set","value":1}}]}}"#;
const MINIMAL_IO: &str = r#"{"type":"io_request","params":{"io_type":"http_get"}}"#;

/// 渲染整份报告的四段式报文（取首条 Error；报告合法时返回 None）
pub fn render_gate(report: &ValidationReport, content: &str, gate_label: &str) -> Option<String> {
    if report.valid {
        return None;
    }
    report
        .results
        .iter()
        .find(|r| r.severity == ValidationSeverity::Error)
        .map(|r| render_issue(r, content, gate_label))
}

/// 渲染单条验证结果的四段式报文
///
/// `gate_label`：门标签（如 "CON-L1-001 持久层写入门"）。
pub fn render_issue(issue: &ValidationResult, content: &str, gate_label: &str) -> String {
    let message = &issue.message;
    let keyword = schema_keyword(message);
    let location = match &issue.path {
        Some(p) => pointer_to_jsonpath(p),
        None => "$".to_string(),
    };
    let line_disp = match approx_line(content, message, issue.path.as_deref()) {
        Some(n) => format!("第 {n} 行附近"),
        None => "-".to_string(),
    };
    let reference = reference_url(message, issue.path.as_deref());
    let example = minimal_example(message);
    format!(
        "[宪法校验失败 / {gate_label}]\n\
         ① 违什么：{message}（schema: {keyword}）\n\
         ② 在哪：{location}（{line_disp}）\n\
         ③ 正本：{reference}\n\
         ④ 正确形态（最小可过）：\n   {example}\n\
         → 导览：{GUIDE_REF}"
    )
}

/// ① schema 关键字归类（从 validator 报错文本尽力提取，缺省 type）
fn schema_keyword(message: &str) -> &'static str {
    if message.contains("is a required property") || message.contains("missing field") {
        "required"
    } else if message.contains("unknown variant") {
        "enum"
    } else if message.contains("pattern") {
        "pattern"
    } else if message.contains("oneOf") || message.contains("anyOf") {
        "oneOf"
    } else if message.contains("minItems") {
        "minItems"
    } else if message.contains("additional") {
        "additionalProperties"
    } else {
        "type"
    }
}

/// ② JSON Pointer → JSONPath 展示形态（"/0/params" → "$[0].params"；
/// 空段跳过，"/" → "$"）
fn pointer_to_jsonpath(pointer: &str) -> String {
    let mut out = String::from("$");
    for seg in pointer.split('/').skip(1) {
        if seg.is_empty() {
            continue;
        }
        match seg.parse::<usize>() {
            Ok(idx) => out.push_str(&format!("[{idx}]")),
            Err(_) => {
                out.push('.');
                out.push_str(seg);
            }
        }
    }
    out
}

/// ② 行号近似：从报错/路径提取字段名 token，在内容文本中找首个 `"token"` 出现行。
/// 尽力定位（缺省 None → 展示 "-"），不做精确映射。
fn approx_line(content: &str, message: &str, path: Option<&str>) -> Option<usize> {
    let token = required_field(message)
        .or_else(|| backtick_field(message))
        .or_else(|| last_alpha_segment(path))?;
    let needle = format!("\"{token}\"");
    content.lines().position(|l| l.contains(&needle)).map(|i| i + 1)
}

/// required 措辞字段名（`"attr" is a required property` → "attr"）
fn required_field(message: &str) -> Option<String> {
    let rest = message.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// 报错文本中反引号包裹的字段名（如 missing field `attr` → "attr"）
fn backtick_field(message: &str) -> Option<String> {
    let mut parts = message.split('`');
    parts.next()?;
    parts.next().map(|s| s.to_string())
}

/// 路径末端字母段（"/transform/0/params" → "params"）
fn last_alpha_segment(path: Option<&str>) -> Option<String> {
    let last = path?.rsplit('/').next()?;
    if last.is_empty() || last.chars().next()?.is_ascii_digit() {
        return None;
    }
    Some(last.to_string())
}

/// ③ 正本链接：报错涉共享 def 时锚 $defs（导览直达最小面），否则回 rule_set 正本
fn reference_url(message: &str, path: Option<&str>) -> String {
    let hay = match path {
        Some(p) => format!("{message} {p}"),
        None => message.to_string(),
    };
    if hay.contains("\"domain\"") || hay.contains("/domain") {
        format!("{SHARED_DEFS_URL}#/$defs/domain")
    } else if hay.contains("instruction") {
        format!("{SHARED_DEFS_URL}#/$defs/instruction")
    } else {
        SCHEMA_REFERENCE_URL.to_string()
    }
}

/// ④ 最小可过样例（[CON-K2-002] 按错误类别取表；缺省=set 最小形态）
fn minimal_example(message: &str) -> &'static str {
    // 权威措辞形态：`"X" is a required property`
    if let Some(field) = required_field(message) {
        return match field.as_str() {
            "domain" | "on_true" => MINIMAL_BRANCH,
            "instructions" => MINIMAL_PUSH,
            "io_type" => MINIMAL_IO,
            _ => MINIMAL_SET,
        };
    }
    // 兜底（反引号措辞形态：missing field `X`）
    if message.contains("missing field") {
        if message.contains("`domain`") || message.contains("`on_true`") {
            return MINIMAL_BRANCH;
        }
        if message.contains("`instructions`") {
            return MINIMAL_PUSH;
        }
        if message.contains("`io_type`") {
            return MINIMAL_IO;
        }
    }
    MINIMAL_SET
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const GATE: &str = "CON-L1-001 持久层写入门";

    /// [CON-K2-002] 质量门：最小样例表全部条目必须真实可过校验器
    #[test]
    fn minimal_examples_all_pass_validator() {
        for (name, example) in [
            ("MINIMAL_SET", MINIMAL_SET),
            ("MINIMAL_BRANCH", MINIMAL_BRANCH),
            ("MINIMAL_PUSH", MINIMAL_PUSH),
            ("MINIMAL_IO", MINIMAL_IO),
        ] {
            let report = crate::validator::validate_rule_json(example);
            assert!(
                report.valid,
                "{name} 必须通过权威校验器（K2-002 最小可过承诺）: {:?}",
                report.results
            );
        }
    }

    /// 四段+导览全段在场（缺任一段=红灯，[CON-K2-001]/[CON-K1-002]）
    #[test]
    fn render_issue_contains_all_segments() {
        let content = r#"{"transform":[{"type":"set","params":{"operation":"set","value":1}}]}"#;
        let report = crate::validator::validate_rule_json(content);
        assert!(!report.valid);
        let text = render_issue(&report.results[0], content, GATE);
        assert!(text.contains("[宪法校验失败 / CON-L1-001 持久层写入门]"), "{text}");
        assert!(text.contains("① 违什么："), "{text}");
        assert!(text.contains("② 在哪："), "{text}");
        assert!(text.contains("③ 正本：https://evorule.org/schemas/"), "{text}");
        assert!(text.contains("④ 正确形态（最小可过）："), "{text}");
        assert!(text.contains("→ 导览：docs/guide.md"), "{text}");
        assert!(text.contains("https://gitee.com/evorule/evorule-system-rules"), "{text}");
        // missing field `attr` → required 关键字 + set 最小样例
        assert!(text.contains("schema: required"), "{text}");
        assert!(text.contains(MINIMAL_SET), "{text}");
    }

    /// JSON 解析失败形态：路径缺省=$、行号缺省=-，四段仍然齐备
    #[test]
    fn render_gate_parse_failure_has_four_segments() {
        let report = crate::validator::validate_rule_json("not-json{{");
        let text = render_gate(&report, "not-json{{", GATE).expect("invalid report must render");
        assert!(text.contains("② 在哪：$（-）"), "{text}");
        assert!(text.contains("④ 正确形态"), "{text}");
        assert!(text.contains("→ 导览："), "{text}");
    }

    /// branch 缺 domain → branch 最小样例 + domain def 锚链接
    #[test]
    fn render_gate_branch_missing_domain() {
        let content = r#"{"transform":[{"type":"branch","params":{"on_true":[]}}]}"#;
        let report = crate::validator::validate_rule_json(content);
        let text = render_gate(&report, content, GATE).expect("invalid report must render");
        assert!(text.contains("③ 正本：https://evorule.org/schemas/_shared/v1.0.json#/$defs/domain"), "{text}");
        assert!(text.contains(MINIMAL_BRANCH), "{text}");
    }

    /// render_gate：合法报告返回 None；invalid 报告取首条 Error
    #[test]
    fn render_gate_none_when_valid() {
        let content = r#"{"transform":[{"type":"set","params":{"attr":"x","operation":"set","value":1}}]}"#;
        let report = crate::validator::validate_rule_json(content);
        assert!(report.valid);
        assert!(render_gate(&report, content, GATE).is_none());
    }

    /// Pointer → JSONPath 转换（数组段与字段段混合）
    #[test]
    fn pointer_to_jsonpath_mixed() {
        assert_eq!(pointer_to_jsonpath("/transform/0/params"), "$.transform[0].params");
        assert_eq!(pointer_to_jsonpath("/transform/12"), "$.transform[12]");
        assert_eq!(pointer_to_jsonpath("/"), "$");
    }
}

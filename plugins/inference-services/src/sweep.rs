//! 反事实参数扫描（counterfactual_analyze 的批量形态）
//!
//! 语义：给定基准文档与参数路径，对调用方提供的值序列**逐值**构造
//! 假设文档（原样拼接 `JsonValue`，零类型强转——数字/字符串/布尔皆可扫），
//! 每点跑一次与 [`crate::counterfactual::analyze`] 完全同语义的双仿真 diff。
//!
//! 诚实边界：
//! - 值序列由调用方显式给出（**不做**自动区间展开——步进语义与浮点
//!   边界由调用方负责，扫描器不替调用方发明取值空间）；
//! - 每点结果独立、按输入序输出，逐点 `diff_count` 不做任何插值或
//!   趋势推断；"越界定位"= 有差异点的清单，非数学断言。

use crate::counterfactual::{self};
use evorule_tcb::JsonValue;

/// 单点扫描结果
#[derive(Debug, Clone)]
pub struct SweepPoint {
    /// 扫描值（调用方 JsonValue 原样回传）
    pub value: JsonValue,
    /// 该点与基准的终态可观测面差异数
    pub diff_count: usize,
    /// 差异路径清单（截断语义随逐点 diff 的截断声明）
    pub paths: Vec<String>,
    /// 原始侧终止原因（同基准复跑，逐点携带供对账）
    pub base_stopped_reason: &'static str,
    /// 该点可观测面 diff 是否截断
    pub truncated: bool,
}

/// 扫描报告
#[derive(Debug)]
pub struct SweepReport {
    pub param_path: String,
    pub points: Vec<SweepPoint>,
    /// 有差异的点数（"越界定位"= 本清单的来源）
    pub points_with_diffs: usize,
    pub max_diff_count: usize,
}

/// 值数硬上限（逐点双仿真，上限防滥用；超限诚实拒绝）
pub const MAX_POINTS: usize = 256;

/// 在文档副本中按点路径替换叶值（中间节点缺失/非对象 → fail-fast，
/// 不发明结构——替换必须发生在既有结构上）。
pub fn set_path(base: &JsonValue, path: &str, value: JsonValue) -> Result<JsonValue, String> {
    let segments: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return Err(
            "param_path 为空 — 自诊断指引: 形如 \"payload.threshold\"（点分隔）".to_string(),
        );
    }
    let mut root = base.clone();
    let mut cur = &mut root;
    let last = segments.len() - 1;
    for (i, seg) in segments.iter().enumerate() {
        let obj = match cur {
            JsonValue::Object(map) => map,
            other => {
                let got = match other {
                    JsonValue::Array(_) => "数组",
                    JsonValue::String(_) => "字符串",
                    JsonValue::Integer(_) => "整数",
                    JsonValue::Bool(_) => "布尔",
                    JsonValue::Null => "null",
                    JsonValue::Object(_) => unreachable!("上一分支已覆盖"),
                };
                return Err(format!(
                    "param_path 第 {i} 段 `{seg}` 落在非对象节点上 — 自诊断指引: \
                     替换路径须逐段命中既有对象;实得{got}节点"
                ));
            }
        };
        if i == last {
            obj.insert((*seg).to_string(), value);
            return Ok(root);
        }
        cur = obj.get_mut(*seg).ok_or_else(|| {
            format!(
                "param_path 第 {i} 段 `{seg}` 在基准文档中不存在 — 自诊断指引: \
                 替换路径须命中既有结构,不发明节点"
            )
        })?;
    }
    unreachable!("segments 非空时必在循环内返回")
}

/// 参数路径扫描主入口
///
/// # Errors
///
/// 透传 analyze 的全部错误 + 参数路径/值序列的解析错误。
pub fn sweep(
    rules: &[JsonValue],
    sources: &[String],
    base_document: &JsonValue,
    param_path: &str,
    values: &[JsonValue],
    max_steps: usize,
) -> Result<SweepReport, String> {
    if values.is_empty() {
        return Err("values 不可为空 — 自诊断指引: 值序列由调用方显式给出,至少 1 点".to_string());
    }
    if values.len() > MAX_POINTS {
        return Err(format!(
            "values={}/{MAX_POINTS} 超上限 — 自诊断指引: 每点一次双仿真,超限请分批",
            values.len()
        ));
    }
    // 基准先跑通再逐点（fail-fast 提前：规则集问题不在扫到一半才炸）
    let mut points = Vec::with_capacity(values.len());
    let mut points_with_diffs = 0usize;
    let mut max_diff_count = 0usize;
    for (i, value) in values.iter().enumerate() {
        let hypothetical = set_path(base_document, param_path, value.clone())
            .map_err(|e| format!("第 {i} 点构造假设文档失败: {e}"))?;
        let report =
            counterfactual::analyze(rules, sources, base_document, &hypothetical, max_steps)?;
        let diff_count = report.diffs.len();
        if diff_count > 0 {
            points_with_diffs += 1;
        }
        max_diff_count = max_diff_count.max(diff_count);
        points.push(SweepPoint {
            value: value.clone(),
            diff_count,
            paths: report.diffs.iter().map(|d| d.path.clone()).collect(),
            base_stopped_reason: report.original.stopped_reason,
            truncated: report.truncated,
        });
    }
    Ok(SweepReport {
        param_path: param_path.to_string(),
        points,
        points_with_diffs,
        max_diff_count,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn copy_rule() -> JsonValue {
        // 复制规则:把 payload.threshold 写到 payload.out（value 走 __ 路径引用）
        JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("attr", JsonValue::string("out")),
                    ("operation", JsonValue::string("set")),
                    ("value", JsonValue::string("__exec__.payload.threshold")),
                ]),
            ),
        ])
    }

    fn base_doc(threshold: i64) -> JsonValue {
        JsonValue::object_from_pairs(&[
            (
                "payload",
                JsonValue::object_from_pairs(&[("threshold", JsonValue::Integer(threshold))]),
            ),
            ("instruction", JsonValue::object_from_pairs(&[])),
            ("queue", JsonValue::Array(vec![])),
        ])
    }

    #[test]
    fn sweep_reports_per_point_diffs() {
        let rules = [copy_rule()];
        let sources = ["rules/t.json".to_string()];
        let base = base_doc(1);
        let values = vec![
            JsonValue::Integer(1),
            JsonValue::Integer(5),
            JsonValue::Integer(10),
        ];
        let report = sweep(&rules, &sources, &base, "payload.threshold", &values, 16).unwrap();
        assert_eq!(report.points.len(), 3);
        assert_eq!(report.points_with_diffs, 2, "v=1 与基准相同 → 零差异");
        // 诚实语义:扫的参数本身也在可观测面上——每点 diff = 参数自身 + 传播面
        assert_eq!(report.max_diff_count, 2);
        assert_eq!(report.points[0].diff_count, 0);
        assert_eq!(report.points[1].diff_count, 2);
        assert_eq!(
            report.points[1].paths,
            vec!["payload.out", "payload.threshold"]
        );
        assert_eq!(report.points[0].base_stopped_reason, "fixpoint");
    }

    #[test]
    fn sweep_accepts_non_integer_values_verbatim() {
        // 值原样拼接:字符串也可扫(结构 diff 不关心类型)
        let rules = [copy_rule()];
        let sources = ["rules/t.json".to_string()];
        let base = base_doc(1);
        let values = vec![JsonValue::string("high")];
        let report = sweep(&rules, &sources, &base, "payload.threshold", &values, 16).unwrap();
        assert_eq!(report.points_with_diffs, 1);
        assert_eq!(
            report.points[0].paths,
            vec!["payload.out", "payload.threshold"]
        );
    }

    #[test]
    fn set_path_fails_fast_on_missing_node() {
        let base = base_doc(1);
        let err = set_path(&base, "payload.nested.deeper", JsonValue::Integer(1)).unwrap_err();
        assert!(err.contains("不存在"), "中间节点缺失必须显式拒绝: {err}");
        let err = set_path(&base, "", JsonValue::Integer(1)).unwrap_err();
        assert!(err.contains("param_path 为空"));
    }

    #[test]
    fn sweep_rejects_empty_and_oversized_values() {
        let rules = [copy_rule()];
        let sources = ["rules/t.json".to_string()];
        let base = base_doc(1);
        let err = sweep(&rules, &sources, &base, "payload.threshold", &[], 16).unwrap_err();
        assert!(err.contains("不可为空"));
        let many: Vec<JsonValue> = (0..=super::MAX_POINTS as i64)
            .map(JsonValue::Integer)
            .collect();
        let err = sweep(&rules, &sources, &base, "payload.threshold", &many, 16).unwrap_err();
        assert!(err.contains("超上限"));
    }
}

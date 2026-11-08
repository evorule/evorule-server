// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 服务壳层：统一信封 v1 + 参数解析守卫 + 4 个判据服务实现。

use std::sync::Arc;

use evorule_plugin_kit::NativeService;
use evorule_reactor::IoResult;
use evorule_tcb::JsonValue;

use crate::criteria::{approval_density_json, behavior_entropy_json, era_mixing_sentinel_json};
use crate::era::split_era_json;
use crate::scope::out_of_scope_writes_json;

/// 事件数组入参资源守卫上限（超限 fail-visible 拒解，不静默降级）。
pub const MAX_EVENTS: usize = 200_000;
/// 滑动窗宽守卫上限。
pub const MAX_WINDOW: i64 = 10_000;

/// 数值出参:整值出 Integer,带小数出字符串(值形态与判据正本一致)。
pub fn num_json(v: f64) -> JsonValue {
    if v.fract() == 0.0 && v.abs() <= 9.0e15 {
        JsonValue::Integer(v as i64)
    } else {
        JsonValue::string(format!("{v}"))
    }
}

pub fn opt_num_json(v: Option<f64>) -> JsonValue {
    v.map(num_json).unwrap_or(JsonValue::Null)
}

pub fn opt_i64_json(v: Option<i64>) -> JsonValue {
    v.map(JsonValue::Integer).unwrap_or(JsonValue::Null)
}

/// 整值 f64 格式化（与判据正本 f-string 对整数的打印形态一致）。
pub fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() <= 9.0e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// 信封头（统一信封 v1 保留键）。
fn envelope_pairs(extra: Vec<(&'static str, JsonValue)>) -> Vec<(&'static str, JsonValue)> {
    let mut pairs = vec![
        ("status", JsonValue::string("determined")),
        ("determinism", JsonValue::string("bitwise_replayable")),
    ];
    pairs.extend(extra);
    pairs
}

/// 解析 events 入参（对象数组 + 资源守卫）。
fn parse_events(args: &JsonValue) -> Result<&[JsonValue], String> {
    let arr = args
        .get("events")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| {
            "缺少参数 events（事件对象数组）。自诊断指引:入参须为 {\"events\": [事件对象, ...]},\
             事件对象即 CLI run --json / WS 帧同构面或 journal NDJSON 行的 JSON 对象"
                .to_string()
        })?;
    if arr.len() > MAX_EVENTS {
        return Err(format!(
            "events 超资源守卫上限 {MAX_EVENTS} 条(实到 {} 条),拒绝处理;自诊断指引:\
             分段送入或由宿主侧预切分",
            arr.len()
        ));
    }
    Ok(arr)
}

/// 解析可选 window 入参(缺省 10,1..=10000)。
fn parse_window(args: &JsonValue) -> Result<usize, String> {
    match args.get("window") {
        None | Some(JsonValue::Null) => Ok(10),
        Some(v) => {
            let w = v
                .as_i64()
                .ok_or_else(|| "window 须为整数。自诊断指引:1..=10000".to_string())?;
            if !(1..=MAX_WINDOW).contains(&w) {
                return Err(format!(
                    "window 越界 {w}(允许 1..={MAX_WINDOW});自诊断指引:窗宽决定\
                     熵序列步长,过窄过宽都稀释判据面"
                ));
            }
            Ok(w as usize)
        }
    }
}

/// 泛型判据服务：入参解析 → 纯函数计算 → 信封组装。
struct CriteriaService<F>
where
    F: Fn(&[JsonValue], &JsonValue) -> Result<JsonValue, String>,
{
    name: &'static str,
    compute: F,
}

impl<F> NativeService for CriteriaService<F>
where
    F: Fn(&[JsonValue], &JsonValue) -> Result<JsonValue, String> + Send + Sync,
{
    fn execute(&self, args: &JsonValue) -> IoResult {
        let events = parse_events(args)?;
        let body = (self.compute)(events, args)?;
        Ok(JsonValue::object_from_pairs(&envelope_pairs(vec![
            ("service_name", JsonValue::string(self.name)),
            ("result", body),
        ])))
    }
}

/// M2 面:行为熵窗口序列 + 前后半程 JS 散度。
pub fn mk_behavior_entropy() -> Arc<dyn NativeService> {
    Arc::new(CriteriaService {
        name: "trace_behavior_entropy",
        compute: |events, args| {
            let window = parse_window(args)?;
            Ok(behavior_entropy_json(events, window))
        },
    })
}

/// M3 面:审批密度/结论分布/流级错误/帧对账未闭合清单。
pub fn mk_approval_density() -> Arc<dyn NativeService> {
    Arc::new(CriteriaService {
        name: "trace_approval_density",
        compute: |events, _args| Ok(approval_density_json(events)),
    })
}

/// 分代切分索引(J1∨J2 边界,J3 旁证)。
pub fn mk_era_split() -> Arc<dyn NativeService> {
    Arc::new(CriteriaService {
        name: "trace_era_split",
        compute: |events, _args| Ok(split_era_json(events)),
    })
}

/// 时代混杂哨兵(ts 跨代 + 模型混桶证据面)。
pub fn mk_era_mixing_sentinel() -> Arc<dyn NativeService> {
    Arc::new(CriteriaService {
        name: "trace_era_mixing_sentinel",
        compute: |events, _args| Ok(era_mixing_sentinel_json(events)),
    })
}

/// M4 面:写类工具路径集合 + scope 区间判定(范围外写入检出)。
pub fn mk_out_of_scope_writes() -> Arc<dyn NativeService> {
    Arc::new(CriteriaService {
        name: "trace_out_of_scope_writes",
        compute: |events, args| out_of_scope_writes_json(events, args),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    // ---- 夹具构造 helper（与判据正本黄金向量生成器的 fixtures 逐字段同构）----

    fn ev(pairs: &[(&str, JsonValue)]) -> JsonValue {
        JsonValue::object_from_pairs(pairs)
    }
    fn s(v: &str) -> JsonValue {
        JsonValue::string(v)
    }
    fn tc(name: &str) -> JsonValue {
        ev(&[("type", s("ToolCall")), ("name", s(name))])
    }
    fn tc_ts(name: &str, ts: i64) -> JsonValue {
        ev(&[
            ("type", s("ToolCall")),
            ("name", s(name)),
            ("ts", JsonValue::Integer(ts)),
        ])
    }
    fn chained_row(seq: i64, ts: i64, prev: &str) -> JsonValue {
        ev(&[
            ("seq", JsonValue::Integer(seq)),
            ("ts", JsonValue::Integer(ts)),
            ("content_hash", s(&format!("h{seq}"))),
            ("prev_hash", s(prev)),
            ("chain_hash", s(&format!("c{seq}"))),
        ])
    }

    /// 夹具 A:12 次 ToolCall + 审批对×2 + 流级错误×2 + 孤儿 ToolResult
    fn fixture_a() -> Vec<JsonValue> {
        let names = [
            "read", "read", "grep", "write", "todo", "read", "grep", "read", "write", "read",
            "grep", "read",
        ];
        let mut events: Vec<JsonValue> = names.iter().map(|n| tc(n)).collect();
        events.push(ev(&[
            ("type", s("ApprovalRequired")),
            ("proposal_id", s("p1")),
            ("tool_name", s("shell_exec")),
        ]));
        events.push(ev(&[
            ("type", s("ApprovalResult")),
            ("proposal_id", s("p1")),
            ("approved", JsonValue::Bool(true)),
        ]));
        events.push(ev(&[
            ("type", s("ApprovalRequired")),
            ("proposal_id", s("p2")),
            ("tool_name", s("file_write")),
        ]));
        events.push(ev(&[
            ("type", s("ApprovalResult")),
            ("proposal_id", s("p2")),
            ("approved", JsonValue::Bool(false)),
            ("auto_rejected", JsonValue::Bool(true)),
        ]));
        events.push(ev(&[
            ("type", s("Error")),
            ("error", s("provider timeout")),
        ]));
        events.push(ev(&[("type", s("Error")), ("error", s("bad gateway"))]));
        events.push(ev(&[("type", s("ToolResult")), ("name", s("orphan"))]));
        events
    }

    // ---- M2 面 ----

    /// 黄金向量(判据正本对账):窗口 4,步长 2,熵序列 5 点,半程散度 0.1038
    #[test]
    fn test_m2_window4_golden() {
        let events = fixture_a();
        let args = ev(&[
            ("events", JsonValue::array(events.clone())),
            ("window", JsonValue::Integer(4)),
        ]);
        let svc = mk_behavior_entropy();
        let out = svc.execute(&args).expect("黄金向量夹具必须成功");
        let body = out.get("result").expect("信封 result");
        assert_eq!(body.get("window").and_then(JsonValue::as_i64), Some(4));
        let series = body
            .get("entropy_series")
            .and_then(JsonValue::as_array)
            .expect("entropy_series");
        assert_eq!(series.len(), 5, "start_call 应为 1,3,5,7,9");
        let starts: Vec<i64> = series
            .iter()
            .map(|p| p.get("start_call").and_then(JsonValue::as_i64).unwrap())
            .collect();
        assert_eq!(starts, vec![1, 3, 5, 7, 9]);
        let ents: Vec<&str> = series
            .iter()
            .map(|p| p.get("entropy").and_then(JsonValue::as_str).unwrap())
            .collect();
        assert_eq!(
            ents,
            vec!["1.5000", "2.0000", "1.5000", "1.5000", "1.5000"],
            "熵 4 位定点字符串(与判据正本 round(x,4) 值相等)"
        );
        assert_eq!(
            body.get("js_divergence_halves").and_then(JsonValue::as_str),
            Some("0.1038")
        );
        // 正本形态锚:键集与 m2_behavior_entropy 输出逐键一致(多键/缺键即偏)
        let mut keys = body
            .as_object()
            .map(|o| o.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        keys.sort();
        assert_eq!(
            keys,
            vec!["entropy_series", "js_divergence_halves", "window"]
        );
    }

    /// 缺省窗宽 10(与分析器 argparse 缺省一致)
    #[test]
    fn test_m2_default_window10_golden() {
        let events = fixture_a();
        let svc = mk_behavior_entropy();
        let out = svc
            .execute(&ev(&[("events", JsonValue::array(events))]))
            .expect("缺省窗宽应成功");
        let body = out.get("result").unwrap();
        assert_eq!(body.get("window").and_then(JsonValue::as_i64), Some(10));
        let series = body
            .get("entropy_series")
            .and_then(JsonValue::as_array)
            .unwrap();
        assert_eq!(series.len(), 1);
        assert_eq!(
            series[0].get("entropy").and_then(JsonValue::as_str),
            Some("1.7610")
        );
    }

    /// 手算向量:4 名等频窗口熵 = log₂4 = 2.0;同分布半程散度 = 0
    #[test]
    fn test_hand_computed_uniform_entropy_and_zero_js() {
        let events: Vec<JsonValue> = ["alpha", "beta", "gamma", "delta"]
            .iter()
            .map(|n| tc(n))
            .collect();
        let svc = mk_behavior_entropy();
        let out = svc
            .execute(&ev(&[
                ("events", JsonValue::array(events.clone())),
                ("window", JsonValue::Integer(4)),
            ]))
            .unwrap();
        let body = out.get("result").unwrap();
        assert_eq!(
            body.get("entropy_series")
                .and_then(JsonValue::as_array)
                .unwrap()[0]
                .get("entropy")
                .and_then(JsonValue::as_str),
            Some("2.0000")
        );
        // 同分布:前后半程各 [alpha, beta](交错)→ JS 散度 0;另证:不相交
        // 分布 [alpha,alpha|beta,beta] → JS 散度 1(两分布支撑集不相交时
        // JS=log₂2=1,手算向量)
        let events2: Vec<JsonValue> = ["alpha", "beta", "alpha", "beta"]
            .iter()
            .map(|n| tc(n))
            .collect();
        let out2 = svc
            .execute(&ev(&[("events", JsonValue::array(events2))]))
            .unwrap();
        assert_eq!(
            out2.get("result")
                .unwrap()
                .get("js_divergence_halves")
                .and_then(JsonValue::as_str),
            Some("0.0000")
        );
        let events3: Vec<JsonValue> = ["alpha", "alpha", "beta", "beta"]
            .iter()
            .map(|n| tc(n))
            .collect();
        let out3 = svc
            .execute(&ev(&[("events", JsonValue::array(events3))]))
            .unwrap();
        assert_eq!(
            out3.get("result")
                .unwrap()
                .get("js_divergence_halves")
                .and_then(JsonValue::as_str),
            Some("1.0000")
        );
    }

    // ---- M3 面 ----

    /// 黄金向量:审批密度/结论分布/auto_rejected/流级错误
    #[test]
    fn test_m3_golden() {
        let events = fixture_a();
        let svc = mk_approval_density();
        let out = svc
            .execute(&ev(&[("events", JsonValue::array(events))]))
            .unwrap();
        let body = out.get("result").unwrap();
        assert_eq!(
            body.get("approval_requests").and_then(JsonValue::as_i64),
            Some(2)
        );
        assert_eq!(
            body.get("approval_results").and_then(JsonValue::as_i64),
            Some(2)
        );
        let verdicts = body.get("verdicts").unwrap();
        assert_eq!(
            verdicts.get("approved").and_then(JsonValue::as_i64),
            Some(1)
        );
        assert_eq!(
            verdicts.get("rejected").and_then(JsonValue::as_i64),
            Some(1)
        );
        assert_eq!(
            body.get("auto_rejected").and_then(JsonValue::as_i64),
            Some(1)
        );
        assert_eq!(
            body.get("stream_errors").and_then(JsonValue::as_i64),
            Some(2)
        );
        let msgs = body
            .get("error_messages")
            .and_then(JsonValue::as_array)
            .unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].as_str(), Some("provider timeout"));
    }

    /// 正本形态锚:M3 键集与 m3_constraint_adherence 输出逐键一致(六键,
    /// 不掺帧对账面——frame_pairing 属正本独立函数,候后批范畴)
    #[test]
    fn test_m3_key_set_matches_reference() {
        let events = fixture_a();
        let svc = mk_approval_density();
        let out = svc
            .execute(&ev(&[("events", JsonValue::array(events))]))
            .unwrap();
        let body = out.get("result").unwrap();
        let mut keys = body
            .as_object()
            .map(|o| o.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "approval_requests",
                "approval_results",
                "auto_rejected",
                "error_messages",
                "stream_errors",
                "verdicts"
            ]
        );
    }

    // ---- 哨兵 ----

    /// 夹具 A 无 ts 无 react → 哨兵不触发
    #[test]
    fn test_sentinel_quiet_on_plain_stream() {
        let events = fixture_a();
        let svc = mk_era_mixing_sentinel();
        let out = svc
            .execute(&ev(&[("events", JsonValue::array(events))]))
            .unwrap();
        let body = out.get("result").unwrap();
        assert_eq!(body.get("mixed").and_then(JsonValue::as_bool), Some(false));
    }

    /// 黄金向量:ts 跨代(25h 间隔) + react 主循环模型混桶 → 双证据面
    #[test]
    fn test_sentinel_golden_two_evidence_faces() {
        let events = vec![
            tc_ts("read", 1000),
            tc_ts("grep", 2000),
            ev(&[
                ("purpose", s("react")),
                ("model", s("model-a")),
                ("ts", JsonValue::Integer(3000)),
            ]),
            ev(&[
                ("purpose", s("react")),
                ("model", s("model-b")),
                ("ts", JsonValue::Integer(90003000)),
            ]),
            tc_ts("write", 90004000),
        ];
        let svc = mk_era_mixing_sentinel();
        let out = svc
            .execute(&ev(&[("events", JsonValue::array(events))]))
            .unwrap();
        let body = out.get("result").unwrap();
        assert_eq!(body.get("mixed").and_then(JsonValue::as_bool), Some(true));
        let evidence = body.get("evidence").and_then(JsonValue::as_array).unwrap();
        assert_eq!(evidence.len(), 2, "ts 跨代 + 模型混桶两条证据");
        assert_eq!(
            evidence[0].as_str(),
            Some("ts_gap>86400000ms: 3000 -> 90003000")
        );
        assert_eq!(
            evidence[1].as_str(),
            Some("react 主循环模型混桶: model-a → model-b")
        );
        let models = body
            .get("react_models")
            .and_then(JsonValue::as_array)
            .unwrap();
        assert_eq!(models[0].as_str(), Some("model-a"));
        assert_eq!(models[1].as_str(), Some("model-b"));
    }

    // ---- 分代切分 ----

    /// 黄金向量:旧账 2 行 → 链化 3 行(J1@行3) → J2 跨代 1 行 = 3 代
    #[test]
    fn test_era_split_three_eras_golden() {
        let events = vec![
            ev(&[
                ("seq", JsonValue::Integer(1)),
                ("ts", JsonValue::Integer(100)),
                ("type", s("LlmDone")),
                ("content", s("x")),
            ]),
            ev(&[
                ("seq", JsonValue::Integer(2)),
                ("ts", JsonValue::Integer(200)),
                ("type", s("ToolCall")),
                ("name", s("read")),
            ]),
            chained_row(3, 300, "genesis"),
            chained_row(4, 400, "h3"),
            chained_row(5, 500, "h4"),
            chained_row(6, 500 + 90000000, "h5"),
        ];
        let svc = mk_era_split();
        let out = svc
            .execute(&ev(&[("events", JsonValue::array(events))]))
            .unwrap();
        let body = out.get("result").unwrap();
        let eras = body.get("eras").and_then(JsonValue::as_array).unwrap();
        assert_eq!(eras.len(), 3, "旧账/链化代/J2 跨代 = 3 代");
        // 代 1:旧账,无链键
        let e1 = &eras[0];
        assert_eq!(
            e1.get("seq_range").and_then(JsonValue::as_array).unwrap()[0].as_i64(),
            Some(1)
        );
        assert_eq!(
            e1.get("line_range").and_then(JsonValue::as_array).unwrap()[1].as_i64(),
            Some(2)
        );
        let sum1 = e1.get("summary").unwrap();
        assert_eq!(sum1.get("rows").and_then(JsonValue::as_i64), Some(2));
        assert_eq!(
            sum1.get("chain_hash_rows").and_then(JsonValue::as_i64),
            Some(0)
        );
        let span1 = sum1
            .get("ts_span_ms")
            .and_then(JsonValue::as_array)
            .unwrap();
        assert_eq!(span1[0].as_i64(), Some(100));
        assert_eq!(span1[1].as_i64(), Some(200));
        // 代 2:J1 翻转,链化行 3 条,genesis 1
        let e2 = &eras[1];
        let det2 = e2.get("detected_at").unwrap();
        let hits2 = det2
            .get("criteria_hit")
            .and_then(JsonValue::as_array)
            .unwrap();
        assert_eq!(hits2[0].as_str(), Some("J1_format_flip"));
        assert_eq!(
            det2.get("j1_format").and_then(JsonValue::as_str),
            Some("链形态翻转@行3:前代 chained=False,本代 chained=True")
        );
        let sum2 = e2.get("summary").unwrap();
        assert_eq!(
            sum2.get("chain_hash_rows").and_then(JsonValue::as_i64),
            Some(3)
        );
        assert_eq!(
            sum2.get("chain_genesis_rows").and_then(JsonValue::as_i64),
            Some(1)
        );
        // 代 3:J2 跨代,单行
        let e3 = &eras[2];
        let det3 = e3.get("detected_at").unwrap();
        assert_eq!(
            det3.get("j2_ts_gap").and_then(JsonValue::as_str),
            Some("@行6:间隔 90000000ms")
        );
        let sum3 = e3.get("summary").unwrap();
        let span3 = sum3
            .get("ts_span_ms")
            .and_then(JsonValue::as_array)
            .unwrap();
        assert_eq!(span3[0].as_i64(), Some(90000500));
        assert_eq!(span3[1].as_i64(), Some(90000500));
        assert_eq!(body.get("mixed").and_then(JsonValue::as_bool), Some(true));
        let sc = body.get("split_criteria").unwrap();
        assert_eq!(
            sc.get("era_gap_ms").and_then(JsonValue::as_i64),
            Some(86400000)
        );
    }

    /// 单一链化代无边界 → 1 代,mixed=false
    #[test]
    fn test_era_split_single_chained_era() {
        let events: Vec<JsonValue> = (1..=4)
            .map(|i| chained_row(i, i * 100, if i == 1 { "genesis" } else { "h" }))
            .collect();
        let svc = mk_era_split();
        let out = svc
            .execute(&ev(&[("events", JsonValue::array(events))]))
            .unwrap();
        let body = out.get("result").unwrap();
        let eras = body.get("eras").and_then(JsonValue::as_array).unwrap();
        assert_eq!(eras.len(), 1);
        assert_eq!(body.get("mixed").and_then(JsonValue::as_bool), Some(false));
        assert_eq!(
            eras[0]
                .get("summary")
                .unwrap()
                .get("chain_genesis_rows")
                .and_then(JsonValue::as_i64),
            Some(1)
        );
    }

    // ---- fail-visible 守卫 ----

    #[test]
    fn test_missing_events_fails_visible_with_guidance() {
        let svc = mk_behavior_entropy();
        let err = svc.execute(&ev(&[])).expect_err("缺 events 必须报错");
        assert!(err.contains("events"), "错误应含指引: {err}");
    }

    #[test]
    fn test_window_out_of_range_fails_visible() {
        let events = vec![tc("a")];
        let svc = mk_behavior_entropy();
        let err = svc
            .execute(&ev(&[
                ("events", JsonValue::array(events)),
                ("window", JsonValue::Integer(0)),
            ]))
            .expect_err("window=0 必须报错");
        assert!(err.contains("window"), "错误应含指引: {err}");
    }

    #[test]
    fn test_non_numeric_window_fails_visible() {
        let events = vec![tc("a")];
        let svc = mk_behavior_entropy();
        let err = svc
            .execute(&ev(&[
                ("events", JsonValue::array(events)),
                ("window", s("wide")),
            ]))
            .expect_err("非整数 window 必须报错");
        assert!(err.contains("整数"), "错误应含指引: {err}");
    }

    // ---- 信封 ----

    #[test]
    fn test_envelope_status_and_determinism_keys() {
        let events = vec![tc("a")];
        let svc = mk_approval_density();
        let out = svc
            .execute(&ev(&[("events", JsonValue::array(events))]))
            .unwrap();
        assert_eq!(
            out.get("status").and_then(JsonValue::as_str),
            Some("determined")
        );
        assert_eq!(
            out.get("determinism").and_then(JsonValue::as_str),
            Some("bitwise_replayable")
        );
        assert_eq!(
            out.get("service_name").and_then(JsonValue::as_str),
            Some("trace_approval_density")
        );
    }
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 事件流判据实现：行为熵（M2 面）、审批密度与帧对账（M3 面）、时代混杂哨兵。
//!
//! 判据公式与离线分析器工具同源（同输入同输出对账口径）；求和序与键序
//! 全部确定性（首现序），小数出参走 4 位定点字符串。

use std::collections::VecDeque;

use evorule_tcb::JsonValue;

use crate::era::ERA_GAP_MS;
use crate::services::fmt_num;

// ============================================================================
// 通用读取与数值口径
// ============================================================================

/// 顶层键优先、缺省回落 `payload` 同名键（与判据正本
/// `ev.get(key, payload.get(key))` 口径一致：顶层键在场（含 null）即取之）。
pub fn get_top_or_payload<'a>(ev: &'a JsonValue, key: &str) -> Option<&'a JsonValue> {
    match ev.get(key) {
        Some(v) => Some(v),
        None => ev.get("payload").and_then(|p| p.get(key)),
    }
}

/// 数值字段 → f64（整数/无符号整数；布尔不计入数值口径）。
pub fn as_f64(v: &JsonValue) -> Option<f64> {
    match v {
        JsonValue::Integer(i) => Some(*i as f64),
        JsonValue::U64(u) => Some(*u as f64),
        _ => None,
    }
}

/// 数值字段 → i64（整数/无符号整数，越界返回 None）。
pub fn as_i64(v: &JsonValue) -> Option<i64> {
    match v {
        JsonValue::Integer(i) => Some(*i),
        JsonValue::U64(u) => i64::try_from(*u).ok(),
        _ => None,
    }
}

/// 真值口径（与判据正本 Python truthy 一致：空串/零/空容器皆假）。
fn truthy(v: &JsonValue) -> bool {
    match v {
        JsonValue::Null => false,
        JsonValue::Bool(b) => *b,
        JsonValue::Integer(i) => *i != 0,
        JsonValue::U64(u) => *u != 0,
        JsonValue::String(s) => !s.is_empty(),
        JsonValue::Array(a) => !a.is_empty(),
        JsonValue::Object(_) => true,
    }
}

/// 4 位定点字符串（与判据正本 round(x, 4) 语义一致：对二进制值正确舍入、
/// ties-to-even；出参统一 scale=4 定点字符串约定）。
pub fn fmt4(x: f64) -> String {
    format!("{x:.4}")
}

/// 首现序计数器（迭代序 = 键首现序，与判据正本 Counter 的 dict 插入序一致）。
#[derive(Default)]
pub struct Counter {
    entries: Vec<(String, usize)>,
}

impl Counter {
    pub fn add(&mut self, k: &str) {
        if let Some(e) = self.entries.iter_mut().find(|(name, _)| name == k) {
            e.1 += 1;
        } else {
            self.entries.push((k.to_string(), 1));
        }
    }
    pub fn get(&self, k: &str) -> usize {
        self.entries
            .iter()
            .find(|(name, _)| name == k)
            .map(|(_, c)| *c)
            .unwrap_or(0)
    }
    pub fn total(&self) -> usize {
        self.entries.iter().map(|(_, c)| c).sum()
    }
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(name, _)| name.as_str())
    }
}

// ============================================================================
// M2 面：行为熵窗口序列 + 前后半程 JS 散度
// ============================================================================

fn shannon_entropy(c: &Counter) -> f64 {
    let total = c.total();
    if total == 0 {
        return 0.0;
    }
    let h: f64 = c
        .entries
        .iter()
        .map(|(_, count)| {
            let p = *count as f64 / total as f64;
            p * p.log2()
        })
        .sum();
    -h
}

/// JS 散度（log2 域）：并集键序 = 前半程首现序接后半程新键。
fn js_divergence(a: &Counter, b: &Counter) -> f64 {
    let ta = a.total();
    let tb = b.total();
    if ta == 0 || tb == 0 {
        return 0.0;
    }
    let mut keys: Vec<String> = a.keys().map(str::to_string).collect();
    for k in b.keys() {
        if a.get(k) == 0 {
            keys.push(k.to_string());
        }
    }
    let pa = |k: &str| a.get(k) as f64 / ta as f64;
    let pb = |k: &str| b.get(k) as f64 / tb as f64;
    let kl = |p: &dyn Fn(&str) -> f64| -> f64 {
        keys.iter()
            .filter_map(|k| {
                let pk = p(k);
                if pk > 0.0 {
                    let m = (pa(k) + pb(k)) / 2.0;
                    Some(pk * (pk / m).log2())
                } else {
                    None
                }
            })
            .sum()
    };
    0.5 * kl(&pa) + 0.5 * kl(&pb)
}

/// ToolCall 到达序工具名序列（name 缺失计 "?"，与正本一致）。
pub fn tool_call_names(events: &[JsonValue]) -> Vec<&str> {
    events
        .iter()
        .filter(|ev| ev.get("type").and_then(JsonValue::as_str) == Some("ToolCall"))
        .map(|ev| ev.get("name").and_then(JsonValue::as_str).unwrap_or("?"))
        .collect()
}

/// M2 输出：窗口熵序列 + 前后半程散度（4 位定点字符串出参）。
pub fn behavior_entropy_json(events: &[JsonValue], window: usize) -> JsonValue {
    let names = tool_call_names(events);
    let step = std::cmp::max(1, window / 2);
    let end = if names.len() >= window {
        names.len() - window + 1
    } else {
        0
    };
    let mut series: Vec<JsonValue> = Vec::new();
    let mut i = 0usize;
    while i < end {
        let mut c = Counter::default();
        for name in &names[i..i + window] {
            c.add(name);
        }
        series.push(JsonValue::object_from_pairs(&[
            ("start_call", JsonValue::Integer(i as i64 + 1)),
            ("entropy", JsonValue::string(fmt4(shannon_entropy(&c)))),
        ]));
        i += step;
    }
    let half = names.len() / 2;
    let js = if half > 0 {
        let mut a = Counter::default();
        for name in &names[..half] {
            a.add(name);
        }
        let mut b = Counter::default();
        for name in &names[half..] {
            b.add(name);
        }
        js_divergence(&a, &b)
    } else {
        0.0
    };
    JsonValue::object_from_pairs(&[
        ("window", JsonValue::Integer(window as i64)),
        ("entropy_series", JsonValue::array(series)),
        ("js_divergence_halves", JsonValue::string(fmt4(js))),
        ("tool_call_count", JsonValue::Integer(names.len() as i64)),
    ])
}

// ============================================================================
// M3 面：审批密度 + 结论分布 + 流级错误 + 帧对账未闭合清单
// ============================================================================

/// M3 输出：审批密度/结论分布/auto_rejected/流级 Error/错误消息样本
/// + 帧对账未闭合清单（对账面也是 M3 观测面的一部分）。
pub fn approval_density_json(events: &[JsonValue]) -> JsonValue {
    let mut approvals = 0usize;
    let mut results = 0usize;
    let mut errors = 0usize;
    let mut verdicts = Counter::default();
    let mut auto_rejected = 0usize;
    let mut error_messages: Vec<JsonValue> = Vec::new();
    let mut call_ids: VecDeque<&str> = VecDeque::new();
    let mut approval_ids: VecDeque<String> = VecDeque::new();
    let mut unclosed_tools: Vec<JsonValue> = Vec::new();
    let mut unclosed_approval_results: Vec<JsonValue> = Vec::new();
    for ev in events {
        let t = ev.get("type").and_then(JsonValue::as_str);
        match t {
            Some("ToolCall") => {
                let name = ev.get("name").and_then(JsonValue::as_str).unwrap_or("?");
                call_ids.push_back(name);
            }
            Some("ToolResult") => {
                if call_ids.pop_front().is_none() {
                    unclosed_tools.push(JsonValue::string("result_without_call"));
                }
            }
            Some("ApprovalRequired") => {
                approvals += 1;
                // 与正本一致:proposal_id falsy(缺省/空串/null)时回落
                // tool_name,再缺省计 "?"
                let id = match ev.get("proposal_id") {
                    Some(JsonValue::String(s)) if !s.is_empty() => s.to_string(),
                    _ => match ev.get("tool_name") {
                        Some(JsonValue::String(s)) if !s.is_empty() => s.to_string(),
                        _ => "?".to_string(),
                    },
                };
                approval_ids.push_back(id);
            }
            Some("ApprovalResult") => {
                results += 1;
                if approval_ids.pop_front().is_none() {
                    unclosed_approval_results.push(JsonValue::string("result_without_request"));
                }
                match ev.get("approved") {
                    Some(v) => {
                        verdicts.add(if truthy(v) { "approved" } else { "rejected" });
                    }
                    None => verdicts.add("rejected"),
                }
                if ev.get("auto_rejected").map(truthy).unwrap_or(false) {
                    auto_rejected += 1;
                }
            }
            Some("Error") => {
                errors += 1;
                if error_messages.len() < 10 {
                    let msg = match ev.get("error") {
                        Some(JsonValue::String(s)) => JsonValue::string(s.clone()),
                        Some(JsonValue::Integer(i)) => JsonValue::Integer(*i),
                        Some(JsonValue::U64(u)) => JsonValue::U64(*u),
                        Some(JsonValue::Bool(b)) => JsonValue::Bool(*b),
                        _ => JsonValue::Null,
                    };
                    error_messages.push(msg);
                }
            }
            _ => {}
        }
    }
    let verdict_obj = JsonValue::object_from_pairs(
        &verdicts
            .keys()
            .map(|k| (k, JsonValue::Integer(verdicts.get(k) as i64)))
            .collect::<Vec<_>>(),
    );
    let str_list = |items: VecDeque<&str>| {
        JsonValue::array(items.into_iter().map(JsonValue::string).collect())
    };
    let str_list_owned = |items: VecDeque<String>| {
        JsonValue::array(items.into_iter().map(JsonValue::string).collect())
    };
    JsonValue::object_from_pairs(&[
        ("approval_requests", JsonValue::Integer(approvals as i64)),
        ("approval_results", JsonValue::Integer(results as i64)),
        ("verdicts", verdict_obj),
        ("auto_rejected", JsonValue::Integer(auto_rejected as i64)),
        ("stream_errors", JsonValue::Integer(errors as i64)),
        ("error_messages", JsonValue::array(error_messages)),
        ("unclosed_tool_calls", str_list(call_ids)),
        ("unclosed_tool_results", JsonValue::array(unclosed_tools)),
        ("unclosed_approvals", str_list_owned(approval_ids)),
        (
            "unclosed_approval_results",
            JsonValue::array(unclosed_approval_results),
        ),
    ])
}

// ============================================================================
// 时代混杂哨兵
// ============================================================================

/// 哨兵输出：mixed/证据面/react 主循环模型列表（混档判读无效化门前置）。
pub fn era_mixing_sentinel_json(events: &[JsonValue]) -> JsonValue {
    let mut evidence: Vec<JsonValue> = Vec::new();
    let mut models: Vec<String> = Vec::new();
    let mut last_ts: Option<f64> = None;
    for ev in events {
        let ts = get_top_or_payload(ev, "ts").and_then(as_f64);
        if let Some(t) = ts {
            if let Some(l) = last_ts {
                if t - l > ERA_GAP_MS {
                    evidence.push(JsonValue::string(format!(
                        "ts_gap>{}ms: {} -> {}",
                        ERA_GAP_MS as i64,
                        fmt_num(l),
                        fmt_num(t)
                    )));
                }
            }
            last_ts = Some(t);
        }
        let purpose = get_top_or_payload(ev, "purpose").and_then(JsonValue::as_str);
        let model = get_top_or_payload(ev, "model").and_then(JsonValue::as_str);
        if purpose == Some("react") {
            if let Some(m) = model {
                if !m.is_empty() && !models.iter().any(|x| x == m) {
                    models.push(m.to_string());
                }
            }
        }
    }
    if models.len() > 1 {
        evidence.push(JsonValue::string(format!(
            "react 主循环模型混桶: {}",
            models.join(" → ")
        )));
    }
    let mixed = !evidence.is_empty();
    JsonValue::object_from_pairs(&[
        ("mixed", JsonValue::Bool(mixed)),
        ("evidence", JsonValue::array(evidence)),
        (
            "react_models",
            JsonValue::array(
                models
                    .iter()
                    .map(|s| JsonValue::string(s.clone()))
                    .collect(),
            ),
        ),
    ])
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 事件流分代切分（只读，零改写输入）：三重正交判据，边界取 J1∨J2 命中点。
//!
//! - J1 格式代（主判据，恒可靠）：行级链哈希字段组（[`CHAIN_KEYS`]）在场
//!   与否——链化行三键齐在场，旧账无；逐行判定，状态翻转即界；
//! - J2 时序代（主判据）：相邻行 ts 间隔 > [`ERA_GAP_MS`]（24h）断点；
//! - J3 身份代（辅证，不单独成界）：react 主循环 model 档位翻转；
//!   sidecar 用途不入检，只作边界快照旁证留痕。
//!
//! 返回分代索引（不写文件、不改输入）：逐代 era/seq_range/line_range/
//! detected_at（判据命中快照）/summary。line_range/seq_range 皆 1-based
//! 闭区间。

use evorule_tcb::JsonValue;

use crate::criteria::{as_f64, as_i64, get_top_or_payload};
use crate::services::{fmt_num, opt_i64_json, opt_num_json};

/// 链哈希字段组（J1 格式代判据；与 journal 链化行编码对码：链化行三键齐插，
/// 旧账无任何一键；任一键在场即判「链化形态」）
pub const CHAIN_KEYS: [&str; 3] = ["content_hash", "prev_hash", "chain_hash"];
/// journal 链首行 prev_hash 锚值（新链起点旁证）
pub const CHAIN_GENESIS: &str = "genesis";
/// 时代混杂 ts 间隔阈值：相邻事件 ts 间隔超过该值（24h）= 跨代断点
pub const ERA_GAP_MS: f64 = 24.0 * 3600.0 * 1000.0;

/// 行级时代特征提取（纯读取）。
struct RowFeatures {
    seq: Option<i64>,
    ts: Option<f64>,
    chained: bool,
    genesis_start: bool,
    temp_present: bool,
    react_model: Option<String>,
}

fn row_features(ev: &JsonValue) -> RowFeatures {
    let purpose = get_top_or_payload(ev, "purpose")
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    let model = get_top_or_payload(ev, "model")
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    let ts = get_top_or_payload(ev, "ts").and_then(as_f64);
    let chained = CHAIN_KEYS.iter().any(|k| ev.get(k).is_some());
    let genesis_start = ev
        .get("prev_hash")
        .and_then(JsonValue::as_str)
        .map(|s| s == CHAIN_GENESIS)
        .unwrap_or(false);
    let temp_present = ev
        .get("payload")
        .map(|p| p.get("temperature").is_some())
        .unwrap_or(false);
    let react_model = if purpose.as_deref() == Some("react") {
        model
    } else {
        None
    };
    RowFeatures {
        seq: ev.get("seq").and_then(as_i64),
        ts,
        chained,
        genesis_start,
        temp_present,
        react_model,
    }
}

/// 逐代累积状态（闭代前）。
struct EraState {
    era: usize,
    chained: bool,
    start_line: usize,
    start_seq: Option<i64>,
    last_seq: Option<i64>,
    last_ts: Option<f64>,
    ts_min: Option<f64>,
    ts_max: Option<f64>,
    rows: usize,
    chained_rows: usize,
    temp_rows: usize,
    genesis_rows: usize,
    react_models: Vec<String>,
    criteria: Vec<String>,
    prev_last_ts: Option<f64>,
}

/// 闭代完成结构（补行尾坐标与边界量化明细，与判据正本闭代步骤对码）。
struct ClosedEra {
    era: usize,
    chained: bool,
    start_line: usize,
    start_seq: Option<i64>,
    last_seq: Option<i64>,
    ts_min: Option<f64>,
    ts_max: Option<f64>,
    rows: usize,
    chained_rows: usize,
    temp_rows: usize,
    genesis_rows: usize,
    react_models: Vec<String>,
    criteria: Vec<String>,
    gap_ms: Option<f64>,
    end_line: usize,
}

fn finish_era(c: EraState, end_line: usize) -> ClosedEra {
    // last_seq 口径与判据正本一致：取 0/缺省时回退 start_seq（0/None 视为未落）
    let last_seq = match c.last_seq {
        Some(s) if s != 0 => Some(s),
        _ => c.start_seq,
    };
    let gap_ms = match (c.ts_min, c.prev_last_ts) {
        (Some(lo), Some(prev)) => Some(lo - prev),
        _ => None,
    };
    ClosedEra {
        era: c.era,
        chained: c.chained,
        start_line: c.start_line,
        start_seq: c.start_seq,
        last_seq,
        ts_min: c.ts_min,
        ts_max: c.ts_max,
        rows: c.rows,
        chained_rows: c.chained_rows,
        temp_rows: c.temp_rows,
        genesis_rows: c.genesis_rows,
        react_models: c.react_models,
        criteria: c.criteria,
        gap_ms,
        end_line,
    }
}

fn split_era_impl(events: &[JsonValue]) -> Vec<ClosedEra> {
    let mut closed: Vec<ClosedEra> = Vec::new();
    let mut cur: Option<EraState> = None;
    let mut prev_ts: Option<f64> = None;
    for (i, ev) in events.iter().enumerate() {
        let feat = row_features(ev);
        let numeric_ts = feat.ts;
        let mut reasons: Vec<String> = Vec::new();
        if let Some(c) = &cur {
            if feat.chained != c.chained {
                reasons.push("J1_format_flip".to_string());
            }
            if let (Some(t), Some(last)) = (numeric_ts, c.last_ts) {
                if t - last > ERA_GAP_MS {
                    reasons.push(format!("J2_ts_gap>{}ms", ERA_GAP_MS as i64));
                }
            }
        }
        if cur.is_none() || !reasons.is_empty() {
            if let Some(c) = cur.take() {
                closed.push(finish_era(c, i));
            }
            cur = Some(EraState {
                era: closed.len() + 1,
                chained: feat.chained,
                start_line: i + 1,
                start_seq: feat.seq,
                last_seq: None,
                last_ts: numeric_ts,
                ts_min: numeric_ts,
                ts_max: numeric_ts,
                rows: 0,
                chained_rows: 0,
                temp_rows: 0,
                genesis_rows: 0,
                react_models: Vec::new(),
                criteria: if reasons.is_empty() {
                    vec!["first_era".to_string()]
                } else {
                    reasons
                },
                prev_last_ts: prev_ts,
            });
        }
        let c = match cur.as_mut() {
            Some(c) => c,
            // 不可达:上方分支保证 cur 已建(首行或换代均建新代)
            None => continue,
        };
        c.rows += 1;
        if numeric_ts.is_some() {
            c.last_ts = numeric_ts;
        }
        if feat.seq.is_some() {
            c.last_seq = feat.seq;
        }
        if let Some(t) = numeric_ts {
            c.ts_min = Some(c.ts_min.map_or(t, |m: f64| m.min(t)));
            c.ts_max = Some(c.ts_max.map_or(t, |m: f64| m.max(t)));
        }
        if feat.chained {
            c.chained_rows += 1;
        }
        if feat.genesis_start {
            c.genesis_rows += 1;
        }
        if feat.temp_present {
            c.temp_rows += 1;
        }
        if let Some(m) = &feat.react_model {
            if !m.is_empty() && !c.react_models.contains(m) {
                c.react_models.push(m.clone());
            }
        }
        if numeric_ts.is_some() {
            prev_ts = numeric_ts;
        }
    }
    if let Some(c) = cur.take() {
        closed.push(finish_era(c, events.len()));
    }
    closed
}

/// Python list 字面量形态:['a', 'b']（判据快照文案与正本一致）。
fn python_list(items: &[String]) -> String {
    format!(
        "[{}]",
        items
            .iter()
            .map(|s| format!("'{}'", s))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// 分代切分 → JSON 索引（只读；不产出切片文件，落盘归宿主工具面）。
pub fn split_era_json(events: &[JsonValue]) -> JsonValue {
    let closed = split_era_impl(events);
    let mixed = closed.len() > 1;
    let eras: Vec<JsonValue> = closed
        .iter()
        .enumerate()
        .map(|(idx, e)| {
            let prev = if idx > 0 {
                Some(&closed[idx - 1])
            } else {
                None
            };
            let hit_j1 = e.criteria.iter().any(|c| c.starts_with("J1"));
            let hit_j2 = e.criteria.iter().any(|c| c.starts_with("J2"));
            let j1_format = if hit_j1 {
                let prev_chained = prev.map(|p| p.chained_rows > 0).unwrap_or(false);
                // 判据快照文案与正本一致:Python bool 打印形态(False/True 首字母大写)
                let fmt_bool = |b: bool| if b { "True" } else { "False" };
                JsonValue::string(format!(
                    "链形态翻转@行{}:前代 chained={},本代 chained={}",
                    e.start_line,
                    fmt_bool(prev_chained),
                    fmt_bool(e.chained)
                ))
            } else {
                JsonValue::Null
            };
            let j2_ts_gap = if hit_j2 {
                match e.gap_ms {
                    Some(g) => {
                        JsonValue::string(format!("@行{}:间隔 {}ms", e.start_line, fmt_num(g)))
                    }
                    None => JsonValue::Null,
                }
            } else {
                JsonValue::Null
            };
            let j3_model = if !e.react_models.is_empty()
                || prev.map(|p| !p.react_models.is_empty()).unwrap_or(false)
            {
                let prev_models = prev.map(|p| p.react_models.clone()).unwrap_or_default();
                JsonValue::string(format!(
                    "前代 react models={} → 本代 models={}",
                    python_list(&prev_models),
                    python_list(&e.react_models)
                ))
            } else {
                JsonValue::Null
            };
            // 正本口径:ts_min 在场才输出 [ts_min, ts_max],否则 null
            let ts_span = if e.ts_min.is_some() {
                JsonValue::array(vec![opt_num_json(e.ts_min), opt_num_json(e.ts_max)])
            } else {
                JsonValue::Null
            };
            JsonValue::object_from_pairs(&[
                ("era", JsonValue::Integer(e.era as i64)),
                (
                    "seq_range",
                    JsonValue::array(vec![opt_i64_json(e.start_seq), opt_i64_json(e.last_seq)]),
                ),
                (
                    "line_range",
                    JsonValue::array(vec![
                        JsonValue::Integer(e.start_line as i64),
                        JsonValue::Integer(e.end_line as i64),
                    ]),
                ),
                (
                    "detected_at",
                    JsonValue::object_from_pairs(&[
                        (
                            "criteria_hit",
                            JsonValue::array(
                                e.criteria
                                    .iter()
                                    .map(|s| JsonValue::string(s.clone()))
                                    .collect(),
                            ),
                        ),
                        ("j1_format", j1_format),
                        ("j2_ts_gap", j2_ts_gap),
                        ("j3_model", j3_model),
                    ]),
                ),
                (
                    "summary",
                    JsonValue::object_from_pairs(&[
                        ("rows", JsonValue::Integer(e.rows as i64)),
                        ("ts_span_ms", ts_span),
                        (
                            "react_models",
                            JsonValue::array(
                                e.react_models
                                    .iter()
                                    .map(|s| JsonValue::string(s.clone()))
                                    .collect(),
                            ),
                        ),
                        ("chain_hash_rows", JsonValue::Integer(e.chained_rows as i64)),
                        (
                            "temperature_key_rows",
                            JsonValue::Integer(e.temp_rows as i64),
                        ),
                        (
                            "chain_genesis_rows",
                            JsonValue::Integer(e.genesis_rows as i64),
                        ),
                    ]),
                ),
            ])
        })
        .collect();
    JsonValue::object_from_pairs(&[
        ("eras", JsonValue::array(eras)),
        (
            "split_criteria",
            JsonValue::object_from_pairs(&[
                (
                    // 披露中性化:正本 rule 文案带内部档号注记,公共仓剥离(对账驱动同口径剥离)
                    "rule",
                    JsonValue::string("边界=J1∨J2 命中点;J3 身份代仅旁证不单独成界"),
                ),
                (
                    "chain_keys",
                    JsonValue::array(CHAIN_KEYS.iter().map(|k| JsonValue::string(*k)).collect()),
                ),
                ("era_gap_ms", JsonValue::Integer(ERA_GAP_MS as i64)),
            ]),
        ),
        ("mixed", JsonValue::Bool(mixed)),
    ])
}

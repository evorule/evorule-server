// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 判据对账工具（本地验证用）：读取 NDJSON/journal JSONL 事件文件，逐行
//! 解析为事件对象后调用本家族判据服务，打印统一信封输出（Display 序列化）。
//!
//! 用法：
//! ```text
//! cargo run -p evorule-trace-audit-services --example reconcile -- <events.ndjson> [era|sentinel|entropy:N]
//! ```
//! 判据名缺省依次输出 era_split 与 sentinel 两段结果；`entropy:N` 追加
//! 窗宽 N 的行为熵输出。与小数出参：浮点数按整值→Integer、带小数→
//! 字符串的口径转换（与判据正本 f-string 打印形态一致）。

use std::collections::BTreeMap;

use evorule_tcb::JsonValue;

fn convert(v: &serde_json::Value) -> JsonValue {
    match v {
        serde_json::Value::Null => JsonValue::Null,
        serde_json::Value::Bool(b) => JsonValue::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                JsonValue::Integer(i)
            } else if let Some(u) = n.as_u64() {
                JsonValue::U64(u)
            } else if let Some(f) = n.as_f64() {
                if f.fract() == 0.0 && f.abs() <= 9.0e15 {
                    JsonValue::Integer(f as i64)
                } else {
                    JsonValue::String(std::borrow::Cow::Owned(format!("{f}")))
                }
            } else {
                JsonValue::Null
            }
        }
        serde_json::Value::String(s) => JsonValue::String(std::borrow::Cow::Owned(s.clone())),
        serde_json::Value::Array(a) => JsonValue::Array(a.iter().map(convert).collect()),
        serde_json::Value::Object(o) => JsonValue::Object(
            o.iter()
                .map(|(k, v)| (k.clone(), convert(v)))
                .collect::<BTreeMap<_, _>>(),
        ),
    }
}

fn envelope(service: &str, body: JsonValue) -> String {
    JsonValue::object_from_pairs(&[
        ("service_name", JsonValue::string(service)),
        ("result", body),
    ])
    .to_string()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!(
            "用法: reconcile <events.ndjson> [era|density|sentinel|entropy:N|oos:SCOPE|BASE]"
        );
        std::process::exit(2);
    }
    let path = &args[1];
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: 事件文件不可读: {e}");
            std::process::exit(2);
        }
    };
    // 与正本 read_events 口径一致:坏行 fail-visible(计数告警,不静默吞)
    let mut bad = 0usize;
    let events: Vec<JsonValue> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| match serde_json::from_str::<serde_json::Value>(l) {
            Ok(v) => Some(convert(&v)),
            Err(_) => {
                bad += 1;
                None
            }
        })
        .collect();
    if bad > 0 {
        eprintln!("warn: {bad} 行 JSON 解析失败(已跳过,与正本 read_events 口径一致)");
    }
    let events_arg = JsonValue::array(events);
    let mut ran = false;
    for mode in args.iter().skip(2) {
        if let Some(w) = mode.strip_prefix("entropy:") {
            let window = match w.parse::<i64>() {
                Ok(n) => n,
                Err(_) => {
                    eprintln!("error: entropy 窗宽须整数: {w}");
                    std::process::exit(2);
                }
            };
            let svc = evorule_trace_audit_services::services::mk_behavior_entropy();
            let input = JsonValue::object_from_pairs(&[
                ("events", events_arg.clone()),
                ("window", JsonValue::Integer(window)),
            ]);
            match svc.execute(&input) {
                Ok(out) => println!("{}", envelope("trace_behavior_entropy", out)),
                Err(e) => println!("{{\"error\":\"{e}\"}}"),
            }
            ran = true;
        } else if mode == "era" {
            let svc = evorule_trace_audit_services::services::mk_era_split();
            let input = JsonValue::object_from_pairs(&[("events", events_arg.clone())]);
            match svc.execute(&input) {
                Ok(out) => println!("{}", envelope("trace_era_split", out)),
                Err(e) => println!("{{\"error\":\"{e}\"}}"),
            }
            ran = true;
        } else if mode == "density" {
            let svc = evorule_trace_audit_services::services::mk_approval_density();
            let input = JsonValue::object_from_pairs(&[("events", events_arg.clone())]);
            match svc.execute(&input) {
                Ok(out) => println!("{}", envelope("trace_approval_density", out)),
                Err(e) => println!("{{\"error\":\"{e}\"}}"),
            }
            ran = true;
        } else if mode == "sentinel" {
            let svc = evorule_trace_audit_services::services::mk_era_mixing_sentinel();
            let input = JsonValue::object_from_pairs(&[("events", events_arg.clone())]);
            match svc.execute(&input) {
                Ok(out) => println!("{}", envelope("trace_era_mixing_sentinel", out)),
                Err(e) => println!("{{\"error\":\"{e}\"}}"),
            }
            ran = true;
        } else if let Some(spec) = mode.strip_prefix("oos:") {
            // M4 面：oos:SCOPE 或 oos:SCOPE|BASE（对账口径=分析器在 BASE=CWD
            // 下以 --scope SCOPE 运行的形态；单值时 SCOPE=BASE）
            let (scope_s, base_s) = match spec.split_once('|') {
                Some((sc, b)) => (sc, b),
                None => (spec, spec),
            };
            let svc = evorule_trace_audit_services::services::mk_out_of_scope_writes();
            let input = JsonValue::object_from_pairs(&[
                ("events", events_arg.clone()),
                ("scope", JsonValue::string(scope_s)),
                ("base_dir", JsonValue::string(base_s)),
            ]);
            match svc.execute(&input) {
                Ok(out) => println!("{}", envelope("trace_out_of_scope_writes", out)),
                Err(e) => println!("{{\"error\":\"{e}\"}}"),
            }
            ran = true;
        }
    }
    if !ran {
        for (name, svc) in [
            (
                "trace_era_split",
                evorule_trace_audit_services::services::mk_era_split(),
            ),
            (
                "trace_era_mixing_sentinel",
                evorule_trace_audit_services::services::mk_era_mixing_sentinel(),
            ),
            (
                "trace_out_of_scope_writes",
                evorule_trace_audit_services::services::mk_out_of_scope_writes(),
            ),
        ] {
            let input = JsonValue::object_from_pairs(&[("events", events_arg.clone())]);
            match svc.execute(&input) {
                Ok(out) => println!("{}", envelope(name, out)),
                Err(e) => println!("{{\"error\":\"{e}\"}}"),
            }
        }
    }
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 服务实现层（骨架批：探针服务）。
//!
//! 信封 v1：成功出参保留键 `status`=determined + `determinism`=
//! bitwise_replayable 打头；error 走框架 `Err(String)` 带自诊断指引。

use std::sync::Arc;

use evorule_plugin_kit::NativeService;
use evorule_reactor::IoResult;
use evorule_tcb::JsonValue;

fn envelope(pairs: &[(&str, JsonValue)]) -> JsonValue {
    let mut all: Vec<(&str, JsonValue)> = vec![
        ("status", JsonValue::string("determined")),
        ("determinism", JsonValue::string("bitwise_replayable")),
    ];
    all.extend_from_slice(pairs);
    JsonValue::object_from_pairs(&all)
}

/// 探针服务：回显可选 `echo` 字符串，恒返固定信封结构（同步 execute，
/// 纯计算零出网——与全族确定性纪律一致）。
struct VideoSkeletonProbe;

impl NativeService for VideoSkeletonProbe {
    fn execute(&self, args: &JsonValue) -> IoResult {
        // 参数形态宽松：echo 可缺省（骨架探针无必填业务参数）
        let echo = args.get("echo").and_then(|v| v.as_str()).unwrap_or("");
        Ok(envelope(&[
            ("service", JsonValue::string("video_service_skeleton_probe")),
            ("echo", JsonValue::string(echo)),
        ]))
    }
}

/// 声明表构造子：骨架探针
pub fn mk_skeleton_probe() -> Arc<dyn NativeService> {
    Arc::new(VideoSkeletonProbe)
}

// ============================================================================
// 字幕时间轴一致性校验（video_timeline_consistency_check）
// ============================================================================

/// 时间轴一致性判定：镜头时间轴 + 字幕 cue 列表，全部整数毫秒比较。
/// 判定项：cue 越出所属镜头(cue_out_of_shot)、同 shot 内 cue 重叠(cue_overlap,
/// overlap_policy=allow 时跳过)、相邻镜头缝隙/重叠(gap/shot_overlap)、
/// 超出总时长(overflow_total,未给 total_duration 则跳过)。
struct TimelineConsistencyCheck;

fn req_i64(obj: &evorule_tcb::ObjectMap, key: &str, ctx: &str) -> Result<i64, String> {
    // tcb 0.13: 整数分 Integer(i64)/U64(u64) 两形态（跨形态 PartialEq 故意不等）。
    // 归一受理两形态的正整数毫秒；U64 超 i64::MAX 仍拒（判定语义不变：合法毫秒域内才受理）。
    obj.get(key)
        .and_then(|v| match v {
            JsonValue::Integer(n) => Some(*n),
            JsonValue::U64(u) => i64::try_from(*u).ok(),
            _ => None,
        })
        .ok_or_else(|| format!("{ctx} 缺整数毫秒字段 {key} — 自诊断指引: 时间量一律整数毫秒"))
}

fn req_str<'a>(obj: &'a evorule_tcb::ObjectMap, key: &str, ctx: &str) -> Result<&'a str, String> {
    obj.get(key)
        .and_then(JsonValue::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{ctx} 缺非空字符串字段 {key}"))
}

fn parse_shots(args: &JsonValue) -> Result<Vec<(String, i64, i64)>, String> {
    let arr = args
        .get("shots")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| {
            "缺少必填参数 shots — 自诊断指引: 数组,每项 {id,start,end},整数毫秒".to_string()
        })?;
    if arr.is_empty() {
        return Err("shots 须为非空数组".to_string());
    }
    let mut out = Vec::with_capacity(arr.len());
    for (i, v) in arr.iter().enumerate() {
        let ctx = format!("shots[{i}]");
        let obj = v
            .as_object()
            .ok_or_else(|| format!("{ctx} 须为对象 — 自诊断指引: {{\"id\",\"start\",\"end\"}}"))?;
        let id = req_str(obj, "id", &ctx)?.to_string();
        let start = req_i64(obj, "start", &ctx)?;
        let end = req_i64(obj, "end", &ctx)?;
        if start >= end {
            return Err(format!(
                "{ctx}({id}) 须 start < end（start={start}, end={end}）"
            ));
        }
        out.push((id, start, end));
    }
    Ok(out)
}

fn parse_cues(args: &JsonValue) -> Result<Vec<(String, String, i64, i64)>, String> {
    let arr = args
        .get("cues")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| {
            "缺少必填参数 cues — 自诊断指引: 数组,每项 {id,shot_id,start,end,text}".to_string()
        })?;
    if arr.is_empty() {
        return Err("cues 须为非空数组".to_string());
    }
    let mut out = Vec::with_capacity(arr.len());
    for (i, v) in arr.iter().enumerate() {
        let ctx = format!("cues[{i}]");
        let obj = v.as_object().ok_or_else(|| {
            format!(
                "{ctx} 须为对象 — 自诊断指引: {{\"id\",\"shot_id\",\"start\",\"end\",\"text\"}}"
            )
        })?;
        let id = req_str(obj, "id", &ctx)?.to_string();
        let shot_id = req_str(obj, "shot_id", &ctx)?.to_string();
        let start = req_i64(obj, "start", &ctx)?;
        let end = req_i64(obj, "end", &ctx)?;
        if start >= end {
            return Err(format!(
                "{ctx}({id}) 须 start < end（start={start}, end={end}）"
            ));
        }
        out.push((id, shot_id, start, end));
    }
    Ok(out)
}

impl NativeService for TimelineConsistencyCheck {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let shots = parse_shots(args)?;
        let cues = parse_cues(args)?;
        let total_duration = args.get("total_duration").and_then(|v| match v {
        JsonValue::Integer(n) => Some(*n),
        JsonValue::U64(u) => i64::try_from(*u).ok(),
        _ => None,
    });
        let overlap_forbid = match args.get("overlap_policy") {
            None | Some(JsonValue::Null) => true,
            Some(v) => match v.as_str() {
                Some("forbid") => true,
                Some("allow") => false,
                other => {
                    return Err(format!(
                        "overlap_policy={other:?} 越界 — 自诊断指引: \"allow\" | \"forbid\"（缺省 forbid）"
                    ))
                }
            },
        };

        let mut violations: Vec<(&str, String, String)> = Vec::new();

        // shot 索引（id → (start,end)；重复 id 视为输入缺陷 fail-fast）
        let mut shot_map: std::collections::BTreeMap<&str, (i64, i64)> =
            std::collections::BTreeMap::new();
        for (id, start, end) in &shots {
            if shot_map.insert(id.as_str(), (*start, *end)).is_some() {
                return Err(format!("shots 含重复 id {id} — 自诊断指引: 镜头 id 须唯一"));
            }
        }

        // ① cue 越出所属镜头
        for (cid, sid, cs, ce) in &cues {
            let (ss, se) = shot_map.get(sid.as_str())
                .ok_or_else(|| format!("cue {cid} 的 shot_id {sid} 在 shots 中不存在 — 自诊断指引: shot_id 须引用已声明镜头"))?;
            if *cs < *ss || *ce > *se {
                violations.push((
                    "cue_out_of_shot",
                    cid.clone(),
                    format!("cue [{cs},{ce}) 越出镜头 {sid} [{ss},{se})"),
                ));
            }
        }

        // ② 同 shot 内 cue 重叠（同序逐对比较；overlap_policy=allow 时跳过）
        if overlap_forbid {
            for (cid, sid, cs, ce) in &cues {
                for (cid2, sid2, cs2, ce2) in &cues {
                    if sid == sid2 && cid < cid2 && *cs < *ce2 && *cs2 < *ce {
                        violations.push((
                            "cue_overlap",
                            cid.clone(),
                            format!("与 cue {cid2} 在镜头 {sid} 内时间重叠"),
                        ));
                    }
                }
            }
        }

        // ③ 相邻镜头缝隙/重叠（按 start 排序后检查连续性）
        let mut ordered: Vec<&(String, i64, i64)> = shots.iter().collect();
        ordered.sort_by_key(|(_, s, _)| *s);
        for pair in ordered.windows(2) {
            let (id_a, _, end_a) = (&pair[0].0, pair[0].1, pair[0].2);
            let (id_b, start_b, _) = (&pair[1].0, pair[1].1, pair[1].2);
            if start_b > end_a {
                violations.push((
                    "gap",
                    id_b.clone(),
                    format!(
                        "镜头 {id_b} 起点落后前一镜头 {id_a} 结束点 {} 毫秒",
                        start_b - end_a
                    ),
                ));
            } else if start_b < end_a {
                violations.push((
                    "shot_overlap",
                    id_b.clone(),
                    format!("镜头 {id_b} 与前一镜头 {id_a} 时间重叠"),
                ));
            }
        }

        // ④ 超出总时长（未给 total_duration 则跳过）
        if let Some(total) = total_duration {
            if total < 0 {
                return Err(format!("total_duration={total} 须非负整数毫秒"));
            }
            for (id, _, end) in &shots {
                if *end > total {
                    violations.push((
                        "overflow_total",
                        id.clone(),
                        format!("镜头 {id} 结束点超总时长 {} 毫秒", end - total),
                    ));
                }
            }
            for (cid, _, _, ce) in &cues {
                if *ce > total {
                    violations.push((
                        "overflow_total",
                        cid.clone(),
                        format!("cue {cid} 结束点超总时长 {} 毫秒", ce - total),
                    ));
                }
            }
        }

        let all_clean = violations.is_empty();
        let violations_arr: Vec<JsonValue> = violations
            .into_iter()
            .map(|(ty, eid, detail)| {
                JsonValue::object_from_pairs(&[
                    ("type", JsonValue::string(ty)),
                    ("element_id", JsonValue::string(eid)),
                    ("detail", JsonValue::string(detail)),
                ])
            })
            .collect();
        Ok(envelope(&[
            (
                "service",
                JsonValue::string("video_timeline_consistency_check"),
            ),
            ("violations", JsonValue::Array(violations_arr)),
            ("all_clean", JsonValue::Bool(all_clean)),
        ]))
    }
}

/// 声明表构造子：字幕时间轴一致性校验
pub fn mk_timeline_consistency_check() -> Arc<dyn NativeService> {
    Arc::new(TimelineConsistencyCheck)
}

// ============================================================================
// 字幕时间轴一致性校验测试（golden：正例 + 越界/重叠/缝隙负例 + 双跑位元一致）
// ============================================================================

#[cfg(test)]
mod timeline_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn shot(id: &str, start: i64, end: i64) -> JsonValue {
        JsonValue::object_from_pairs(&[
            ("id", JsonValue::string(id)),
            ("start", JsonValue::Integer(start)),
            ("end", JsonValue::Integer(end)),
        ])
    }

    fn cue(id: &str, shot_id: &str, start: i64, end: i64) -> JsonValue {
        JsonValue::object_from_pairs(&[
            ("id", JsonValue::string(id)),
            ("shot_id", JsonValue::string(shot_id)),
            ("start", JsonValue::Integer(start)),
            ("end", JsonValue::Integer(end)),
            ("text", JsonValue::string("字幕")),
        ])
    }

    fn args(shots: Vec<JsonValue>, cues: Vec<JsonValue>) -> JsonValue {
        JsonValue::object_from_pairs(&[
            ("shots", JsonValue::Array(shots)),
            ("cues", JsonValue::Array(cues)),
        ])
    }

    fn run(a: &JsonValue) -> JsonValue {
        TimelineConsistencyCheck.execute(a).unwrap()
    }

    #[test]
    fn timeline_golden_positive() {
        // 1 镜头 1 cue 完全落内 → all_clean
        let a = args(vec![shot("s1", 0, 5000)], vec![cue("c1", "s1", 100, 900)]);
        let out = run(&a);
        assert_eq!(
            out.get("all_clean").and_then(JsonValue::as_bool),
            Some(true)
        );
        assert_eq!(
            out.get("violations")
                .and_then(JsonValue::as_array)
                .map(|v| v.len()),
            Some(0)
        );
        assert_eq!(
            out.get("status").and_then(JsonValue::as_str),
            Some("determined")
        );
    }

    #[test]
    fn timeline_negative_out_of_shot() {
        // cue 越出镜头右界
        let a = args(vec![shot("s1", 0, 5000)], vec![cue("c1", "s1", 100, 6000)]);
        let out = run(&a);
        assert_eq!(
            out.get("all_clean").and_then(JsonValue::as_bool),
            Some(false)
        );
        let v = out.get("violations").and_then(JsonValue::as_array).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(
            v[0].get("type").and_then(JsonValue::as_str),
            Some("cue_out_of_shot")
        );
        assert_eq!(
            v[0].get("element_id").and_then(JsonValue::as_str),
            Some("c1")
        );
    }

    #[test]
    fn timeline_negative_overlap_and_gap() {
        // 两 cue 同 shot 重叠 + 两镜头缝隙
        let a = args(
            vec![shot("s1", 0, 3000), shot("s2", 3500, 6000)],
            vec![
                cue("c1", "s1", 100, 2000),
                cue("c2", "s1", 1500, 2500),
                cue("c3", "s2", 3600, 4500),
            ],
        );
        let out = run(&a);
        let v = out.get("violations").and_then(JsonValue::as_array).unwrap();
        let types: Vec<&str> = v
            .iter()
            .filter_map(|x| x.get("type").and_then(JsonValue::as_str))
            .collect();
        assert!(
            types.contains(&"cue_overlap"),
            "须含 cue_overlap: {types:?}"
        );
        assert!(types.contains(&"gap"), "须含 gap: {types:?}");
        // overlap_policy=allow 时重叠项消失
        let mut a2 = a;
        if let JsonValue::Object(ref mut m) = a2 {
            m.insert("overlap_policy".to_string(), JsonValue::string("allow"));
        }
        let out2 = run(&a2);
        let v2 = out2
            .get("violations")
            .and_then(JsonValue::as_array)
            .unwrap();
        let types2: Vec<&str> = v2
            .iter()
            .filter_map(|x| x.get("type").and_then(JsonValue::as_str))
            .collect();
        assert!(
            !types2.contains(&"cue_overlap"),
            "allow 下不得含 cue_overlap"
        );
        assert!(types2.contains(&"gap"), "allow 仍须含 gap");
    }

    #[test]
    fn timeline_double_run_identical_and_overflow() {
        // 双跑位元一致 + overflow_total（显式 total_duration）
        let a = args(vec![shot("s1", 0, 6000)], vec![cue("c1", "s1", 100, 7000)]);
        let mut a2 = a.clone();
        if let JsonValue::Object(ref mut m) = a2 {
            m.insert("total_duration".to_string(), JsonValue::Integer(5000));
        }
        let out = run(&a2);
        let out_b = run(&a2);
        assert_eq!(out, out_b, "同参双跑须位元一致");
        let v = out.get("violations").and_then(JsonValue::as_array).unwrap();
        let types: Vec<&str> = v
            .iter()
            .filter_map(|x| x.get("type").and_then(JsonValue::as_str))
            .collect();
        assert!(
            types.iter().filter(|t| **t == "overflow_total").count() == 2,
            "镜头+cue 各一条 overflow_total: {types:?}"
        );
        // 未给 total_duration → 跳过 overflow 判定（cue 越镜仍报 cue_out_of_shot，非 overflow 项）
        let out_none = run(&a);
        let v_none = out_none
            .get("violations")
            .and_then(JsonValue::as_array)
            .unwrap();
        let types_none: Vec<&str> = v_none
            .iter()
            .filter_map(|x| x.get("type").and_then(JsonValue::as_str))
            .collect();
        assert!(
            types_none.iter().all(|t| *t == "cue_out_of_shot"),
            "无 total 时仅报越镜: {types_none:?}"
        );
    }

    #[test]
    fn timeline_rejects_bad_input() {
        // shot_id 引用不存在镜头 → Err（fail-fast 不产 violations）
        let a = args(
            vec![shot("s1", 0, 1000)],
            vec![cue("c1", "ghost", 100, 500)],
        );
        assert!(TimelineConsistencyCheck.execute(&a).is_err());
        // 非法 overlap_policy
        let mut a2 = args(vec![shot("s1", 0, 1000)], vec![cue("c1", "s1", 100, 500)]);
        if let JsonValue::Object(ref mut m) = a2 {
            m.insert("overlap_policy".to_string(), JsonValue::string("maybe"));
        }
        assert!(TimelineConsistencyCheck.execute(&a2).is_err());
        // shots 空数组
        let a3 = args(vec![], vec![cue("c1", "s1", 100, 500)]);
        assert!(TimelineConsistencyCheck.execute(&a3).is_err());
    }
}

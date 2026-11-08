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
// 颜色对比度判定（color_contrast_ratio_eval）— V3′ 判定核
// ============================================================================

/// 颜色对比度判定核：前景/背景 sRGB 三元组（0..255 十进制字符串）→
/// WCAG 2.x 相对亮度 + 对比度比值，判定是否达显式阈值。
///
/// 确定性实现说明（本核判定语义与 11 档 v2.0 契约一致）：
/// - 入参全为结构化字符串数字（通道整数串、阈值十进制有理数串），
///   零文件路径/零采样策略/零感知语义；
/// - WCAG 线性化含 2.4 次幂（超越函数），纯有理数域无封闭解；本核以
///   **256 点离线冻结常量表**实现（生成式即 WCAG 公式、half-up 取整、
///   运行时零浮点零近似）——同参双跑位元一致由整数查表语义保证；
/// - 出参 ratio/lum 为定点字符串（4/6 位小数），复核用中间量与主量
///   同源；金标向量断言到字符串位（数学精确向量 0/1/21 全位断言，
///   超越函数近似向量仅断言判定面与位元一致，不断言近似尾数）。
struct ColorContrastRatioEval;

/// 亮度定点标度（1e9）与线性化整数实现
const LUM_SCALE: i128 = 1_000_000_000;

/// WCAG 2.x sRGB 线性化常量表（256 点，LUM_SCALE=1e9 标度，half-up 取整）。
/// 生成式：c<=10 → (c/255)/12.92；c>=11 → ((c/255+0.055)/1.055)^2.4。
/// 数学精确锚（c=0→0、c=255→1000000000、黑白对比度恰 21）已离线验证；
/// 生成脚本用 f64 参照系算表后冻结——运行时零浮点零近似零超越函数。
const WCAG_LINEAR: [i128; 256] = [
    0, 303527, 607054, 910581, 1214108, 1517635, 1821162, 2124689, 2428216, 2731743, 3035270,
    3346536, 3676507, 4024717, 4391442, 4776953, 5181517, 5605392, 6048833, 6512091, 6995410,
    7499032, 8023193, 8568126, 9134059, 9721217, 10329823, 10960094, 11612245, 12286488, 12983032,
    13702083, 14443844, 15208514, 15996293, 16807376, 17641954, 18500220, 19382361, 20288563,
    21219010, 22173885, 23153366, 24157632, 25186860, 26241222, 27320892, 28426040, 29556834,
    30713444, 31896033, 33104767, 34339807, 35601315, 36889450, 38204372, 39546235, 40915197,
    42311411, 43735029, 45186204, 46665086, 48171824, 49706566, 51269458, 52860647, 54480276,
    56128490, 57805430, 59511238, 61246054, 63010018, 64803267, 66625939, 68478170, 70360096,
    72271851, 74213568, 76185381, 78187422, 80219820, 82282707, 84376212, 86500462, 88655586,
    90841711, 93058963, 95307467, 97587347, 99898728, 102241733, 104616484, 107023103, 109461711,
    111932428, 114435374, 116970668, 119538428, 122138772, 124771818, 127437680, 130136477,
    132868322, 135633330, 138431615, 141263291, 144128471, 147027266, 149959790, 152926152,
    155926464, 158960835, 162029376, 165132195, 168269400, 171441101, 174647404, 177888416,
    181164244, 184474995, 187820772, 191201683, 194617830, 198069320, 201556254, 205078736,
    208636870, 212230757, 215860500, 219526200, 223227957, 226965874, 230740049, 234550582,
    238397574, 242281122, 246201327, 250158285, 254152094, 258182853, 262250658, 266355605,
    270497791, 274677312, 278894263, 283148740, 287440838, 291770650, 296138271, 300543794,
    304987314, 309468923, 313988713, 318546778, 323143209, 327778098, 332451536, 337163615,
    341914425, 346704056, 351532600, 356400144, 361306780, 366252596, 371237680, 376262123,
    381326011, 386429434, 391572478, 396755231, 401977780, 407240212, 412542613, 417885071,
    423267670, 428690497, 434153636, 439657174, 445201195, 450785783, 456411023, 462077000,
    467783796, 473531496, 479320183, 485149940, 491020850, 496932995, 502886458, 508881321,
    514917665, 520995573, 527115126, 533276404, 539479489, 545724461, 552011402, 558340390,
    564711506, 571124829, 577580440, 584078418, 590618841, 597201788, 603827339, 610495571,
    617206562, 623960392, 630757136, 637596874, 644479682, 651405637, 658374817, 665387298,
    672443157, 679542470, 686685312, 693871761, 701101892, 708375780, 715693501, 723055129,
    730460740, 737910409, 745404210, 752942217, 760524505, 768151147, 775822218, 783537792,
    791297940, 799102738, 806952258, 814846572, 822785754, 830769877, 838799012, 846873232,
    854992608, 863157213, 871367119, 879622397, 887923118, 896269353, 904661174, 913098652,
    921581856, 930110858, 938685728, 947306537, 955973353, 964686248, 973445290, 982250550,
    991102097, 1000000000,
];

/// 通道线性化 = 常量表查表（解析层已限 0..255，get 越界分支纯防御）
fn wcag_linear_channel(c: i64) -> i128 {
    let idx = usize::try_from(c).ok().unwrap_or(usize::MAX);
    *WCAG_LINEAR
        .get(idx)
        .unwrap_or(WCAG_LINEAR.last().unwrap_or(&0))
}

/// WCAG 相对亮度：L = 0.2126R + 0.7152G + 0.0722B（LUM_SCALE 标度）
fn wcag_relative_luminance(rgb: &[i64; 3]) -> i128 {
    let r = wcag_linear_channel(rgb[0]);
    let g = wcag_linear_channel(rgb[1]);
    let b = wcag_linear_channel(rgb[2]);
    (2126 * r + 7152 * g + 722 * b + 5000) / 10000
}

/// 定点串格式化：v 为 1e9 标度非负定点，出 `ip.frac`（frac 位宽=out_decimals，
/// half-up 取整）。要求 out_decimals <= 9。
fn fmt_lum_scaled(v: i128, out_decimals: u32) -> String {
    let div = 10i128.pow(9 - out_decimals);
    let q = (v + div / 2) / div;
    let unit = 10i128.pow(out_decimals);
    format!(
        "{}.{:0width$}",
        q / unit,
        q % unit,
        width = out_decimals as usize
    )
}

/// 十进制有理数字符串解析（非负定点）：返回 (值, 标度)。
/// 限形：整数段 <=12 位、小数段 <=12 位、纯数字+至多一个小数点。
fn parse_rational_dec(raw: &str, ctx: &str) -> Result<(i128, i128), String> {
    let s = raw.trim();
    if s.is_empty() || s.len() > 25 {
        return Err(format!(
            "{ctx} 非法有理数字符串 {raw:?} — 自诊断指引: 十进制形如 \"4.5\"/\"0.125\"（长度<=25）"
        ));
    }
    let (ip, fp) = match s.split_once('.') {
        None => (s, ""),
        Some((a, b)) => (a, b),
    };
    if (ip.is_empty() && fp.is_empty())
        || ip.len() > 12
        || fp.len() > 12
        || !ip.bytes().all(|b| b.is_ascii_digit())
        || !fp.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(format!(
            "{ctx} 非法有理数字符串 {raw:?} — 自诊断指引: 十进制形如 \"4.5\"（整数/小数各<=12 位纯数字）"
        ));
    }
    let mut v: i128 = 0;
    for b in ip.bytes() {
        v = v * 10 + (b - b'0') as i128;
    }
    let mut scale: i128 = 1;
    for b in fp.bytes() {
        v = v * 10 + (b - b'0') as i128;
        scale *= 10;
    }
    Ok((v, scale))
}

/// 跨标度比较：a/sa ? b/sb（纯整数交叉相乘，无除法无浮点）
fn cmp_scaled(a: i128, sa: i128, b: i128, sb: i128) -> std::cmp::Ordering {
    (a * sb).cmp(&(b * sa))
}

fn parse_srgb_channel(v: &JsonValue, ctx: &str) -> Result<i64, String> {
    let s = v
        .as_str()
        .ok_or_else(|| format!("{ctx} 须为字符串数字 — 自诊断指引: 0..255 十进制整数字符串"))?;
    let t = s.trim();
    if t.is_empty() || t.len() > 3 || !t.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "{ctx} 非法通道值 {s:?} — 自诊断指引: 0..255 十进制整数字符串"
        ));
    }
    let mut n: i64 = 0;
    for b in t.bytes() {
        n = n * 10 + (b - b'0') as i64;
    }
    if n > 255 {
        return Err(format!("{ctx}={n} 越界 — 自诊断指引: 0..255"));
    }
    Ok(n)
}

fn parse_srgb_triple(arr: &[JsonValue], base: &str) -> Result<[i64; 3], String> {
    if arr.len() != 3 {
        return Err(format!(
            "{base} 须恰 3 通道 [r,g,b] — 自诊断指引: sRGB 三元组,各通道 0..255 字符串"
        ));
    }
    let mut out = [0i64; 3];
    for (i, v) in arr.iter().enumerate() {
        out[i] = parse_srgb_channel(v, &format!("{base}[{i}]"))?;
    }
    Ok(out)
}

impl NativeService for ColorContrastRatioEval {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let fg = args
            .get("fg")
            .and_then(JsonValue::as_array)
            .ok_or("缺少必填参数 fg — 自诊断指引: [r,g,b] 各通道 0..255 字符串")?;
        let bg = args
            .get("bg")
            .and_then(JsonValue::as_array)
            .ok_or("缺少必填参数 bg — 自诊断指引: [r,g,b] 各通道 0..255 字符串")?;
        let fgv = parse_srgb_triple(fg, "fg")?;
        let bgv = parse_srgb_triple(bg, "bg")?;
        let (thr_v, thr_s) = match args.get("threshold") {
            None | Some(JsonValue::Null) => (45i128, 10i128), // 缺省 "4.5"
            Some(v) => {
                let s = v.as_str().ok_or(
                    "threshold 须为字符串有理数 — 自诊断指引: \"4.5\"（整数/小数各<=12 位）",
                )?;
                let p = parse_rational_dec(s, "threshold")?;
                if p.0 < 0 {
                    return Err(
                        "threshold 须非负 — 自诊断指引: 对比度阈值正值如 \"4.5\"".to_string()
                    );
                }
                p
            }
        };

        let fg_lum = wcag_relative_luminance(&fgv);
        let bg_lum = wcag_relative_luminance(&bgv);
        // 比值 = (L亮+0.05)/(L暗+0.05)（0.05 = 5e7 @1e9 标度）
        let (hi, lo) = if fg_lum >= bg_lum {
            (fg_lum, bg_lum)
        } else {
            (bg_lum, fg_lum)
        };
        let n = hi + 50_000_000i128;
        let d = lo + 50_000_000i128;
        let ratio_scaled = (n * LUM_SCALE + d / 2) / d; // 1e9 标度
                                                        // pass = ratio >= threshold（跨标度整数比较，同浮点无关）
        let pass = cmp_scaled(ratio_scaled, LUM_SCALE, thr_v, thr_s) != std::cmp::Ordering::Less;

        Ok(envelope(&[
            ("service", JsonValue::string("color_contrast_ratio_eval")),
            ("ratio", JsonValue::string(fmt_lum_scaled(ratio_scaled, 4))),
            ("pass", JsonValue::Bool(pass)),
            ("fg_lum", JsonValue::string(fmt_lum_scaled(fg_lum, 6))),
            ("bg_lum", JsonValue::string(fmt_lum_scaled(bg_lum, 6))),
            (
                "threshold",
                JsonValue::string(fmt_lum_scaled(thr_v * LUM_SCALE / thr_s, 4)),
            ),
        ]))
    }
}

/// 声明表构造子：颜色对比度判定
pub fn mk_color_contrast_ratio_eval() -> Arc<dyn NativeService> {
    Arc::new(ColorContrastRatioEval)
}

// ============================================================================
// 帧序列异常事件判定（video_frame_sequence_eval）— V4′ 判定核
// ============================================================================

/// 帧序列异常事件判定核：逐帧平均亮度摘要序列（引擎侧抽帧统计产物，
/// 字符串十进制 0..255，帧号=数组下标）→ 黑帧/闪帧/静止段三类事件。
///
/// 判定语义（阈值全部显式入参，缺省值为契约层标注）：
/// - 黑帧：亮度 <= black_threshold（缺省 "16"）；
/// - 闪帧：相邻帧亮度差绝对值 >= flash_delta（缺省 "60"）；
/// - 静止段：连续相等亮度段长度 >= still_frames 帧（缺省 24，以帧计）。
///
/// 确定性实现：亮度统一归一 1e4 标度定点（超 4 位小数 half-up 归入，
/// 契约层标注），阈值跨标度整数交叉比较，零浮点零墙钟。
struct VideoFrameSequenceEval;

/// 亮度定点标度（1e4）
const SEQ_SCALE: i128 = 10_000;

/// 有理数 (v,scale) 归一到 SEQ_SCALE（half-up，v>=0）；越 1e4 标度精度即舍入
fn rescale_to_seq(v: i128, scale: i128) -> i64 {
    if scale >= SEQ_SCALE {
        let d = scale / SEQ_SCALE;
        let q = v / d;
        let r = v % d;
        let q2 = if r * 2 >= d { q + 1 } else { q };
        q2 as i64
    } else {
        (v * (SEQ_SCALE / scale)) as i64
    }
}

/// SEQ_SCALE 标度值格式化（4 位小数）
fn fmt_seq(v: i64) -> String {
    let v = v as i128;
    format!("{}.{:04}", v / SEQ_SCALE, v % SEQ_SCALE)
}

fn parse_lum_frame(v: &JsonValue, ctx: &str) -> Result<i64, String> {
    let s = v.as_str().ok_or_else(|| {
        format!("{ctx} 须为字符串十进制数 — 自诊断指引: 0..255 如 \"128\"/\"12.5\"")
    })?;
    let (val, scale) = parse_rational_dec(s, ctx)?;
    if val < 0 {
        return Err(format!("{ctx}={s} 越界 — 自诊断指引: 0..255"));
    }
    let n = rescale_to_seq(val, scale);
    if n > 2_550_000 {
        return Err(format!("{ctx}={s} 越界 — 自诊断指引: 0..255"));
    }
    Ok(n)
}

impl NativeService for VideoFrameSequenceEval {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let lums = args
            .get("lums")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| {
                "缺少必填参数 lums — 自诊断指引: 逐帧平均亮度字符串数组,帧号=数组下标".to_string()
            })?;
        if lums.is_empty() {
            return Err("lums 须为非空数组 — 自诊断指引: 至少 1 帧亮度摘要".to_string());
        }
        let mut vals: Vec<i64> = Vec::with_capacity(lums.len());
        for (i, v) in lums.iter().enumerate() {
            vals.push(parse_lum_frame(v, &format!("lums[{i}]"))?);
        }
        let (black_v, black_s) = match args.get("black_threshold") {
            None | Some(JsonValue::Null) => (160_000i128, 10_000i128), // 缺省 "16"
            Some(v) => {
                let s = v.as_str().ok_or_else(|| {
                    "black_threshold 须为字符串有理数 — 自诊断指引: \"16\"".to_string()
                })?;
                let p = parse_rational_dec(s, "black_threshold")?;
                if p.0 < 0 {
                    return Err("black_threshold 须非负 — 自诊断指引: 如 \"16\"".to_string());
                }
                p
            }
        };
        let (flash_v, flash_s) = match args.get("flash_delta") {
            None | Some(JsonValue::Null) => (600_000i128, 10_000i128), // 缺省 "60"
            Some(v) => {
                let s = v.as_str().ok_or_else(|| {
                    "flash_delta 须为字符串有理数 — 自诊断指引: \"60\"".to_string()
                })?;
                let p = parse_rational_dec(s, "flash_delta")?;
                if p.0 < 0 {
                    return Err("flash_delta 须非负 — 自诊断指引: 如 \"60\"".to_string());
                }
                p
            }
        };
        let still_min: i64 = match args.get("still_frames") {
            None | Some(JsonValue::Null) => 24,
            Some(v) => {
                let n = match v {
                    JsonValue::Integer(n) => Some(*n),
                    JsonValue::U64(u) => i64::try_from(*u).ok(),
                    _ => None,
                }
                .ok_or_else(|| {
                    "still_frames 须为正整数帧数 — 自诊断指引: 如 24（以帧计,时间换算由引擎做）"
                        .to_string()
                })?;
                if n <= 0 {
                    return Err(format!("still_frames={n} 须正整数帧数"));
                }
                n
            }
        };

        // ① 黑帧（帧号升序）
        let black_frames: Vec<i64> = vals
            .iter()
            .enumerate()
            .filter(|(_, v)| {
                cmp_scaled(**v as i128, SEQ_SCALE, black_v, black_s) != std::cmp::Ordering::Greater
            })
            .map(|(i, _)| i as i64)
            .collect();

        // ② 闪帧（相邻帧差绝对值 >= flash_delta；事件帧号=右帧下标）
        let mut flash_events: Vec<JsonValue> = Vec::new();
        for (k, w) in vals.windows(2).enumerate() {
            let (a, b) = (w[0], w[1]);
            let diff = (b as i128 - a as i128).abs();
            if cmp_scaled(diff, SEQ_SCALE, flash_v, flash_s) != std::cmp::Ordering::Less {
                flash_events.push(JsonValue::object_from_pairs(&[
                    ("frame", JsonValue::Integer(k as i64 + 1)),
                    ("from", JsonValue::string(fmt_seq(a))),
                    ("to", JsonValue::string(fmt_seq(b))),
                ]));
            }
        }

        // ③ 静止段（连续相等亮度 >= still_min 帧）
        let mut still_spans: Vec<JsonValue> = Vec::new();
        let mut run_start = 0usize;
        for i in 1..=vals.len() {
            if i == vals.len() || vals[i] != vals[run_start] {
                let len = i - run_start;
                if len >= still_min as usize {
                    still_spans.push(JsonValue::object_from_pairs(&[
                        ("start_frame", JsonValue::Integer(run_start as i64)),
                        ("end_frame", JsonValue::Integer((i - 1) as i64)),
                        ("frames", JsonValue::Integer(len as i64)),
                    ]));
                }
                run_start = i;
            }
        }

        let counts = JsonValue::object_from_pairs(&[
            (
                "black_frames",
                JsonValue::Integer(black_frames.len() as i64),
            ),
            (
                "flash_events",
                JsonValue::Integer(flash_events.len() as i64),
            ),
            ("still_spans", JsonValue::Integer(still_spans.len() as i64)),
        ]);
        Ok(envelope(&[
            ("service", JsonValue::string("video_frame_sequence_eval")),
            (
                "black_frames",
                JsonValue::Array(black_frames.into_iter().map(JsonValue::Integer).collect()),
            ),
            ("flash_events", JsonValue::Array(flash_events)),
            ("still_spans", JsonValue::Array(still_spans)),
            ("counts", counts),
        ]))
    }
}

/// 声明表构造子：帧序列异常事件判定
pub fn mk_video_frame_sequence_eval() -> Arc<dyn NativeService> {
    Arc::new(VideoFrameSequenceEval)
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

// ============================================================================
// 颜色对比度判定测试（golden：精确锚全位断言 + 判定面负例 + 双跑位元一致）
// ============================================================================

#[cfg(test)]
mod contrast_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn rgb_triplet(r: &str, g: &str, b: &str) -> JsonValue {
        JsonValue::Array(vec![
            JsonValue::string(r),
            JsonValue::string(g),
            JsonValue::string(b),
        ])
    }

    fn args(fg: JsonValue, bg: JsonValue, thr: Option<&str>) -> JsonValue {
        let mut pairs: Vec<(&str, JsonValue)> = vec![("fg", fg), ("bg", bg)];
        if let Some(t) = thr {
            pairs.push(("threshold", JsonValue::string(t)));
        }
        JsonValue::object_from_pairs(&pairs)
    }

    fn run(a: &JsonValue) -> JsonValue {
        ColorContrastRatioEval.execute(a).unwrap()
    }

    #[test]
    fn contrast_exact_anchors() {
        // 数学精确锚（与常量表生成式一致性互相锁定）:
        // 黑/白 → ratio 恰 21.0000；白/黑同值；全同色 → 1.0000
        let bw = args(
            rgb_triplet("255", "255", "255"),
            rgb_triplet("0", "0", "0"),
            None,
        );
        let out = run(&bw);
        assert_eq!(
            out.get("ratio").and_then(JsonValue::as_str),
            Some("21.0000")
        );
        assert_eq!(out.get("pass").and_then(JsonValue::as_bool), Some(true));
        assert_eq!(
            out.get("fg_lum").and_then(JsonValue::as_str),
            Some("1.000000")
        );
        assert_eq!(
            out.get("bg_lum").and_then(JsonValue::as_str),
            Some("0.000000")
        );

        let wb = args(
            rgb_triplet("0", "0", "0"),
            rgb_triplet("255", "255", "255"),
            None,
        );
        let out2 = run(&wb);
        // fg/bg 互换: ratio 恒(分子恒亮者)、pass 恒；fg_lum/bg_lum 应互换
        assert_eq!(out2.get("ratio"), out.get("ratio"));
        assert_eq!(out2.get("pass"), out.get("pass"));
        assert_eq!(
            out2.get("fg_lum").and_then(JsonValue::as_str),
            Some("0.000000")
        );
        assert_eq!(
            out2.get("bg_lum").and_then(JsonValue::as_str),
            Some("1.000000")
        );

        // 全同色 → 1.0000, 低于任何阈值 → fail
        let same = args(
            rgb_triplet("128", "128", "128"),
            rgb_triplet("128", "128", "128"),
            None,
        );
        let out3 = run(&same);
        assert_eq!(
            out3.get("ratio").and_then(JsonValue::as_str),
            Some("1.0000")
        );
        assert_eq!(out3.get("pass").and_then(JsonValue::as_bool), Some(false));
    }

    #[test]
    fn contrast_threshold_boundary_and_negative() {
        // 中灰参照: WCAG 灰(#808080) 亮度≈0.21586 → 对黑比值=(0.21586+0.05)/0.05≈5.3172
        let g = args(
            rgb_triplet("128", "128", "128"),
            rgb_triplet("0", "0", "0"),
            None,
        );
        let out = run(&g);
        let ratio: f64 = out
            .get("ratio")
            .and_then(JsonValue::as_str)
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            (ratio - 5.3172).abs() < 0.001,
            "灰/黑 ratio≈5.3172: {ratio}"
        );

        // threshold 边界: ratio≈5.3172 阈值 "5.317" → pass；"5.318" → fail
        let lo = args(
            rgb_triplet("128", "128", "128"),
            rgb_triplet("0", "0", "0"),
            Some("5.317"),
        );
        assert_eq!(
            run(&lo).get("pass").and_then(JsonValue::as_bool),
            Some(true)
        );
        let hi = args(
            rgb_triplet("128", "128", "128"),
            rgb_triplet("0", "0", "0"),
            Some("5.318"),
        );
        assert_eq!(
            run(&hi).get("pass").and_then(JsonValue::as_bool),
            Some(false)
        );

        // 缺省 threshold = "4.5": 黑白过, 灰黑(5.3172)也过
        let bw = args(
            rgb_triplet("255", "255", "255"),
            rgb_triplet("0", "0", "0"),
            None,
        );
        assert_eq!(
            run(&bw).get("pass").and_then(JsonValue::as_bool),
            Some(true)
        );
        assert_eq!(run(&g).get("pass").and_then(JsonValue::as_bool), Some(true));
        // 但更低对比组合(深灰 40/黑)不过: 亮度≈0.021→(0.021+0.05)/0.05≈1.43
        let dark = args(
            rgb_triplet("40", "40", "40"),
            rgb_triplet("0", "0", "0"),
            None,
        );
        assert_eq!(
            run(&dark).get("pass").and_then(JsonValue::as_bool),
            Some(false)
        );
    }

    #[test]
    fn contrast_double_run_identical() {
        let a = args(
            rgb_triplet("200", "30", "90"),
            rgb_triplet("12", "200", "44"),
            Some("3.0"),
        );
        let x = run(&a);
        let y = run(&a);
        assert_eq!(x, y, "同参双跑须位元一致");
        // 字段齐全
        for k in ["ratio", "pass", "fg_lum", "bg_lum", "threshold", "status"] {
            assert!(x.get(k).is_some(), "出参缺 {k}");
        }
    }

    #[test]
    fn contrast_rejects_bad_input() {
        // 通道越界/非法形
        let bad_ch = args(
            rgb_triplet("256", "0", "0"),
            rgb_triplet("0", "0", "0"),
            None,
        );
        assert!(ColorContrastRatioEval.execute(&bad_ch).is_err());
        let bad_form = args(
            rgb_triplet("12.5", "0", "0"),
            rgb_triplet("0", "0", "0"),
            None,
        );
        assert!(ColorContrastRatioEval.execute(&bad_form).is_err());
        // 通道数不对
        let arr2 = JsonValue::Array(vec![JsonValue::string("0"), JsonValue::string("0")]);
        let bad_n = args(arr2, rgb_triplet("0", "0", "0"), None);
        assert!(ColorContrastRatioEval.execute(&bad_n).is_err());
        // 缺 fg / 缺 bg
        let only_bg = JsonValue::object_from_pairs(&[("bg", rgb_triplet("0", "0", "0"))]);
        assert!(ColorContrastRatioEval.execute(&only_bg).is_err());
        // 负阈值/非法阈值
        let neg = args(
            rgb_triplet("0", "0", "0"),
            rgb_triplet("255", "255", "255"),
            Some("-1"),
        );
        assert!(ColorContrastRatioEval.execute(&neg).is_err());
        let junk = args(
            rgb_triplet("0", "0", "0"),
            rgb_triplet("255", "255", "255"),
            Some("abc"),
        );
        assert!(ColorContrastRatioEval.execute(&junk).is_err());
    }
}

// ============================================================================
// 帧序列事件判定测试（golden：黑帧/闪帧/静止段正负例 + 双跑位元一致 + 越界拒）
// ============================================================================

#[cfg(test)]
mod frame_seq_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn args(lums: Vec<&str>) -> JsonValue {
        JsonValue::object_from_pairs(&[(
            "lums",
            JsonValue::Array(lums.into_iter().map(JsonValue::string).collect()),
        )])
    }

    fn args_with(mut a: JsonValue, key: &str, v: JsonValue) -> JsonValue {
        if let JsonValue::Object(ref mut m) = a {
            m.insert(key.to_string(), v);
        }
        a
    }

    fn run(a: &JsonValue) -> JsonValue {
        VideoFrameSequenceEval.execute(a).unwrap()
    }

    #[test]
    fn seq_golden_black_flash_still() {
        // 合成序列: 恒黑3帧(0,0,0) + 跳变120(闪) + 平台150×2 + 跳变40(闪) + 恒24×25(静)
        let mut lums: Vec<&str> = vec!["0", "0", "0"];
        lums.push("120"); // 闪事件帧3 (0→120)
        lums.extend_from_slice(&["150", "150"]); // 150-120=30 不闪
        lums.push("40"); // 闪事件帧6 (150→40, Δ110)
        lums.extend(std::iter::repeat_n("24", 25));
        let a = args(lums);
        let out = run(&a);

        let black: Vec<i64> = out
            .get("black_frames")
            .and_then(JsonValue::as_array)
            .unwrap()
            .iter()
            .filter_map(|v| v.as_i64())
            .collect();
        assert_eq!(black, vec![0, 1, 2], "黑帧帧号: {black:?}");

        let flashes: Vec<i64> = out
            .get("flash_events")
            .and_then(JsonValue::as_array)
            .unwrap()
            .iter()
            .filter_map(|v| v.get("frame").and_then(|f| f.as_i64()))
            .collect();
        assert_eq!(flashes, vec![3, 6], "闪帧事件帧号: {flashes:?}");

        let spans: Vec<(i64, i64, i64)> = out
            .get("still_spans")
            .and_then(JsonValue::as_array)
            .unwrap()
            .iter()
            .filter_map(|v| {
                Some((
                    v.get("start_frame")?.as_i64()?,
                    v.get("end_frame")?.as_i64()?,
                    v.get("frames")?.as_i64()?,
                ))
            })
            .collect();
        assert_eq!(spans, vec![(7, 31, 25)], "静止段: {spans:?}");

        let counts = out.get("counts").expect("counts");
        assert_eq!(counts.get("black_frames").and_then(|v| v.as_i64()), Some(3));
        assert_eq!(counts.get("flash_events").and_then(|v| v.as_i64()), Some(2));
        assert_eq!(counts.get("still_spans").and_then(|v| v.as_i64()), Some(1));
        assert_eq!(
            out.get("status").and_then(JsonValue::as_str),
            Some("determined")
        );
    }

    #[test]
    fn seq_thresholds_explicit_and_boundary() {
        // 显式阈值: black=10 → 只有严格 ≤10 的帧算黑（10.0001 不算）
        let a = args_with(
            args(vec!["10", "10.0001", "11", "9.9999"]),
            "black_threshold",
            JsonValue::string("10"),
        );
        let out = run(&a);
        let black: Vec<i64> = out
            .get("black_frames")
            .and_then(JsonValue::as_array)
            .unwrap()
            .iter()
            .filter_map(|v| v.as_i64())
            .collect();
        assert_eq!(
            black,
            vec![0, 3],
            "black<=10 边界(10.0001>10 不算): {black:?}"
        );

        // flash_delta 边界: Δ=60.00 恰达缺省 60 → 算闪
        let b = args(vec!["0", "60"]);
        let flashes = run(&b)
            .get("flash_events")
            .and_then(JsonValue::as_array)
            .unwrap()
            .len();
        assert_eq!(flashes, 1, "Δ=60 恰达缺省阈值须算闪");

        // still_frames=3 显式: 恒值 2 帧段不算, 3 帧段算
        let c = args_with(
            args(vec!["50", "50", "51", "51", "51", "60"]),
            "still_frames",
            JsonValue::Integer(3),
        );
        let spans: Vec<(i64, i64, i64)> = run(&c)
            .get("still_spans")
            .and_then(JsonValue::as_array)
            .unwrap()
            .iter()
            .filter_map(|v| {
                Some((
                    v.get("start_frame")?.as_i64()?,
                    v.get("end_frame")?.as_i64()?,
                    v.get("frames")?.as_i64()?,
                ))
            })
            .collect();
        assert_eq!(spans, vec![(2, 4, 3)], "still_frames=3(下标制): {spans:?}");
    }

    #[test]
    fn seq_double_run_identical_and_envelope() {
        let a = args(vec!["0", "200", "0", "200", "0"]);
        let x = run(&a);
        let y = run(&a);
        assert_eq!(x, y, "同参双跑须位元一致");
        assert_eq!(
            x.get("status").and_then(JsonValue::as_str),
            Some("determined")
        );
        assert_eq!(
            x.get("determinism").and_then(JsonValue::as_str),
            Some("bitwise_replayable")
        );
        // from/to 字符串形态
        let ev = x.get("flash_events").and_then(JsonValue::as_array).unwrap();
        assert_eq!(
            ev[0].get("from").and_then(JsonValue::as_str),
            Some("0.0000"),
            "from 须定点字符串"
        );
        assert_eq!(
            ev[0].get("to").and_then(JsonValue::as_str),
            Some("200.0000")
        );
    }

    #[test]
    fn seq_rejects_bad_input() {
        // 空数组
        assert!(VideoFrameSequenceEval.execute(&args(vec![])).is_err());
        // 缺 lums
        assert!(VideoFrameSequenceEval
            .execute(&JsonValue::object_from_pairs(&[]))
            .is_err());
        // 越界 >255
        assert!(VideoFrameSequenceEval
            .execute(&args(vec!["255.1"]))
            .is_err());
        // 非法形态（数字而非字符串）
        let num_form = JsonValue::object_from_pairs(&[(
            "lums",
            JsonValue::Array(vec![JsonValue::Integer(128)]),
        )]);
        assert!(VideoFrameSequenceEval.execute(&num_form).is_err());
        // 非正 still_frames
        let a = args_with(args(vec!["0"]), "still_frames", JsonValue::Integer(0));
        assert!(VideoFrameSequenceEval.execute(&a).is_err());
        // 负阈值
        let b = args_with(args(vec!["0"]), "black_threshold", JsonValue::string("-5"));
        assert!(VideoFrameSequenceEval.execute(&b).is_err());
        // 非法 flash_delta
        let c = args_with(args(vec!["0"]), "flash_delta", JsonValue::string("x"));
        assert!(VideoFrameSequenceEval.execute(&c).is_err());
    }
}
// ============================================================================
// 主体轨迹守恒判定（video_track_consistency_eval）— V5' 判定核
// ============================================================================

/// 主体轨迹守恒判定核：参考 bbox + 逐帧主体 bbox 序列（引擎侧感知检出/
/// 跟踪产物结构化喂入）→ 判定每帧位移/尺度漂移是否越限。
///
/// 判定语义（宿主侧视频能力需求方案 V5' 契约）：
/// - 位移 = 帧框中心 − 参考框中心，按 metric（`l2` 缺省/`linf`）取值，
///   超 `max_translation`（px）即违例 `translation_exceeded`；
/// - 尺度漂移 = |帧框面积 − 参考框面积| / 参考框面积，超
///   `max_scale_delta` 即违例 `scale_exceeded`；
/// - 参考框由引擎侧显式给出（感知禁令：无 auto_from_first_frame 模式）。
///
/// 确定性实现：
/// - 坐标限形：整数段 <=7 位 + 小数段 <=6 位纯数字（非负）→ 1e6 定点
///   整**精确**表示（零舍入零浮点）；max_scale_delta 整数段 <=2 位；
/// - l2 判定面 = dx^2+dy^2 与 limit^2 纯整数精确比较（零开方）；
///   显示值 = 确定性整数 Newton isqrt（floor，纯整数迭代）；
/// - 尺度判定 = 交叉相乘整比较；限形下峰值积 <=1e35 << i128 上限
///   （1.7e38），零溢出；
/// - 帧号非负整数必填唯一，判定按帧号升序（输入序不敏感）。
struct VideoTrackConsistencyEval;

/// 轨迹核定点标度（1e6，坐标小数 <=6 位 → 全量精确）
const TRK_SCALE: i128 = 1_000_000;

/// 坐标/阈值有理数解析（非负）：整数段 <=max_i 位、小数段 <=max_f 位。
/// 返回 TRK_SCALE 精确定点值（scale 整除 1e6，零舍入）。
fn parse_trk_rational(raw: &str, ctx: &str, max_i: usize, max_f: usize) -> Result<i128, String> {
    let s = raw.trim();
    let (ip, fp) = match s.split_once('.') {
        None => (s, ""),
        Some((a, b)) => (a, b),
    };
    if s.is_empty()
        || (ip.is_empty() && fp.is_empty())
        || ip.len() > max_i
        || fp.len() > max_f
        || !ip.bytes().all(|b| b.is_ascii_digit())
        || !fp.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(format!(
            "{ctx} 非法数值 {raw:?} — 自诊断指引: 十进制形如 \"100.5\"（整数 <={max_i} 位 + 小数 <={max_f} 位纯数字,非负）"
        ));
    }
    let mut v: i128 = 0;
    for b in ip.bytes() {
        v = v * 10 + (b - b'0') as i128;
    }
    let mut scale: i128 = 1;
    for b in fp.bytes() {
        v = v * 10 + (b - b'0') as i128;
        scale *= 10;
    }
    Ok(v * (TRK_SCALE / scale))
}

/// bbox 解析：{x,y,w,h} 字符串有理数（非负；w/h 须 >0）；返回 1e6 定点 [x,y,w,h]
fn parse_trk_bbox(v: &JsonValue, ctx: &str) -> Result<[i128; 4], String> {
    let obj = v
        .as_object()
        .ok_or_else(|| format!("{ctx} 须为对象 {{x,y,w,h}} — 自诊断指引: 四字段字符串有理数"))?;
    let mut out = [0i128; 4];
    for (k, idx) in [("x", 0usize), ("y", 1), ("w", 2), ("h", 3)] {
        let s = obj
            .get(k)
            .and_then(JsonValue::as_str)
            .ok_or_else(|| format!("{ctx}.{k} 须为字符串有理数 — 自诊断指引: 如 \"120.5\""))?;
        out[idx] = parse_trk_rational(s, &format!("{ctx}.{k}"), 7, 6)?;
    }
    if out[2] <= 0 || out[3] <= 0 {
        return Err(format!(
            "{ctx} 宽高须 >0 — 自诊断指引: w/h 正值（面积零/负无法作尺度基准）"
        ));
    }
    Ok(out)
}

/// 帧号解析：非负整数（数值形入参,契约层标注）
fn parse_trk_frame(v: &JsonValue) -> Result<i64, String> {
    let n = match v {
        JsonValue::Integer(n) => Some(*n),
        JsonValue::U64(u) => i64::try_from(*u).ok(),
        _ => None,
    }
    .ok_or_else(|| "tracks[].frame 须为非负整数 — 自诊断指引: 帧号必填（数值形）".to_string())?;
    if n < 0 {
        return Err(format!("tracks[].frame={n} 须非负"));
    }
    Ok(n)
}

/// 确定性整数 Newton isqrt（floor(sqrt(n))，纯整数迭代零浮点）
fn isqrt(n: i128) -> i128 {
    if n < 2 {
        return n;
    }
    let mut x: i128 = 1 << (128 - (n as u128).leading_zeros() as usize).div_ceil(2);
    loop {
        let y = (x + n / x) / 2;
        if y >= x {
            break;
        }
        x = y;
    }
    x
}

/// 1e6 定点值 → 精确 6 位小数字符串（值本身 1e6 精确,零舍入）
fn fmt_trk(v: i128) -> String {
    format!("{}.{:06}", v / TRK_SCALE, v % TRK_SCALE)
}

/// 1e4 标度值 → 4 位小数字符串（判定精确,显示截断尾数）
fn fmt_trk4(v: i128) -> String {
    format!("{}.{:04}", v / 10_000, v % 10_000)
}

impl NativeService for VideoTrackConsistencyEval {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let reference = args.get("reference").ok_or(
            "缺少必填参数 reference — 自诊断指引: {x,y,w,h} 字符串有理数（引擎侧感知检出显式给出）",
        )?;
        let tracks = args
            .get("tracks")
            .and_then(JsonValue::as_array)
            .ok_or("缺少必填参数 tracks — 自诊断指引: 逐帧数组 [{frame, bbox},...],帧号必填")?;
        if tracks.is_empty() {
            return Err("tracks 须为非空数组 — 自诊断指引: 至少 1 帧主体 bbox".to_string());
        }
        let metric = match args.get("metric") {
            None | Some(JsonValue::Null) => "l2",
            Some(v) => v
                .as_str()
                .ok_or("metric 须为字符串 \"l2\" 或 \"linf\" — 自诊断指引: 缺省 l2")?,
        };
        if metric != "l2" && metric != "linf" {
            return Err(format!(
                "metric={metric} 未知 — 自诊断指引: 仅 \"l2\"（欧氏）或 \"linf\"（切比雪夫）,缺省 l2"
            ));
        }
        let max_t_raw = args
            .get("max_translation")
            .and_then(JsonValue::as_str)
            .ok_or("缺少必填参数 max_translation — 自诊断指引: 位移上限 px,字符串如 \"40\"")?;
        let max_t = parse_trk_rational(max_t_raw, "max_translation", 7, 6)?;
        let max_s_raw = args
            .get("max_scale_delta")
            .and_then(JsonValue::as_str)
            .ok_or("缺少必填参数 max_scale_delta — 自诊断指引: 尺度漂移上限,字符串如 \"0.2\"")?;
        let max_s = parse_trk_rational(max_s_raw, "max_scale_delta", 2, 6)?;

        let reference = parse_trk_bbox(reference, "reference")?;
        let ref_cx = reference[0] + reference[2] / 2;
        let ref_cy = reference[1] + reference[3] / 2;
        let ref_area = reference[2] * reference[3];

        // 解析全部帧（帧号唯一性守卫+按帧号升序判定,输入序不敏感）
        let mut frames: Vec<(i64, [i128; 4])> = Vec::with_capacity(tracks.len());
        for (i, t) in tracks.iter().enumerate() {
            let frame = parse_trk_frame(
                t.get("frame")
                    .ok_or_else(|| format!("tracks[{i}] 缺 frame — 自诊断指引: 帧号必填"))?,
            )
            .map_err(|e| format!("tracks[{i}] {e}"))?;
            let bbox = parse_trk_bbox(
                t.get("bbox")
                    .ok_or_else(|| format!("tracks[{i}] 缺 bbox — 自诊断指引: {{x,y,w,h}}"))?,
                &format!("tracks[{i}].bbox"),
            )?;
            frames.push((frame, bbox));
        }
        frames.sort_by_key(|(f, _)| *f);
        for w in frames.windows(2) {
            if w[0].0 == w[1].0 {
                return Err(format!(
                    "tracks 帧号重复: {} — 自诊断指引: 每帧恰一条,重复条目判据歧义故拒绝",
                    w[0].0
                ));
            }
        }

        // 逐帧判定
        let mut per_frame: Vec<JsonValue> = Vec::with_capacity(frames.len());
        let mut violations: Vec<JsonValue> = Vec::new();
        let mut max_trans_obs: i128 = 0;
        let mut max_scale_obs: i128 = 0;
        for (frame, bbox) in &frames {
            let cx = bbox[0] + bbox[2] / 2;
            let cy = bbox[1] + bbox[3] / 2;
            let dx = (cx - ref_cx).abs();
            let dy = (cy - ref_cy).abs();
            // 位移：linf=max(|dx|,|dy|)；l2=sqrt(dx^2+dy^2)（判定面=平方整比较）
            let (trans, trans_exceeded) = match metric {
                "linf" => {
                    let v = dx.max(dy);
                    (v, v > max_t)
                }
                _ => {
                    let sq = dx * dx + dy * dy;
                    (isqrt(sq), sq > max_t * max_t)
                }
            };
            if trans > max_trans_obs {
                max_trans_obs = trans;
            }
            // 尺度漂移：比较 drift > max_s <=> dn*1e6 > max_s*ref_area（交叉乘,精确）
            let area = bbox[2] * bbox[3];
            let dn = (area - ref_area).abs();
            let scale_exceeded = dn * TRK_SCALE > max_s * ref_area;
            let drift = dn * 10_000 / ref_area; // 1e4 标度显示值（截断尾数）
            if drift > max_scale_obs {
                max_scale_obs = drift;
            }
            if trans_exceeded {
                violations.push(JsonValue::object_from_pairs(&[
                    ("frame", JsonValue::Integer(*frame)),
                    ("kind", JsonValue::string("translation_exceeded")),
                    ("value", JsonValue::string(fmt_trk(trans))),
                    ("limit", JsonValue::string(fmt_trk(max_t))),
                ]));
            }
            if scale_exceeded {
                violations.push(JsonValue::object_from_pairs(&[
                    ("frame", JsonValue::Integer(*frame)),
                    ("kind", JsonValue::string("scale_exceeded")),
                    ("value", JsonValue::string(fmt_trk4(drift))),
                    (
                        "limit",
                        JsonValue::string(fmt_trk4(max_s * 10_000 / TRK_SCALE)),
                    ),
                ]));
            }
            per_frame.push(JsonValue::object_from_pairs(&[
                ("frame", JsonValue::Integer(*frame)),
                ("translation", JsonValue::string(fmt_trk(trans))),
                ("scale_drift", JsonValue::string(fmt_trk4(drift))),
            ]));
        }

        let all_clean = violations.is_empty();
        Ok(envelope(&[
            ("service", JsonValue::string("video_track_consistency_eval")),
            ("all_clean", JsonValue::Bool(all_clean)),
            ("per_frame", JsonValue::Array(per_frame)),
            ("violations", JsonValue::Array(violations)),
            (
                "max_translation_observed",
                JsonValue::string(fmt_trk(max_trans_obs)),
            ),
            (
                "max_scale_observed",
                JsonValue::string(fmt_trk4(max_scale_obs)),
            ),
        ]))
    }
}

/// 声明表构造子：主体轨迹守恒判定
pub fn mk_video_track_consistency_eval() -> Arc<dyn NativeService> {
    Arc::new(VideoTrackConsistencyEval)
}
// ============================================================================
// 轨迹守恒判定测试（golden：匀速合法/单帧跳变/尺度渐变 + 边界 + 双跑一致）
// ============================================================================

#[cfg(test)]
mod track_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn bbox(x: &str, y: &str, w: &str, h: &str) -> JsonValue {
        JsonValue::object_from_pairs(&[
            ("x", JsonValue::string(x)),
            ("y", JsonValue::string(y)),
            ("w", JsonValue::string(w)),
            ("h", JsonValue::string(h)),
        ])
    }

    fn trk(frame: i64, b: JsonValue) -> JsonValue {
        JsonValue::object_from_pairs(&[("frame", JsonValue::Integer(frame)), ("bbox", b)])
    }

    fn args(
        reference: JsonValue,
        tracks: Vec<JsonValue>,
        max_t: &str,
        max_s: &str,
        metric: Option<&str>,
    ) -> JsonValue {
        let mut pairs: Vec<(&str, JsonValue)> = vec![
            ("reference", reference),
            ("tracks", JsonValue::Array(tracks)),
            ("max_translation", JsonValue::string(max_t)),
            ("max_scale_delta", JsonValue::string(max_s)),
        ];
        if let Some(m) = metric {
            pairs.push(("metric", JsonValue::string(m)));
        }
        JsonValue::object_from_pairs(&pairs)
    }

    fn run(a: &JsonValue) -> JsonValue {
        VideoTrackConsistencyEval.execute(a).unwrap()
    }

    #[test]
    fn track_uniform_clean_golden() {
        // 匀速合法轨迹: 帧框恒等于参考框 → 全清洁
        let r = bbox("100", "100", "40", "20");
        let a = args(
            r.clone(),
            vec![trk(0, r.clone()), trk(1, r.clone()), trk(2, r.clone())],
            "40",
            "0.2",
            None,
        );
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
            out.get("max_translation_observed")
                .and_then(JsonValue::as_str),
            Some("0.000000")
        );
        assert_eq!(
            out.get("max_scale_observed").and_then(JsonValue::as_str),
            Some("0.0000")
        );
        let pf = out.get("per_frame").and_then(JsonValue::as_array).unwrap();
        assert_eq!(pf.len(), 3);
        assert_eq!(
            pf[0].get("translation").and_then(JsonValue::as_str),
            Some("0.000000")
        );
        assert_eq!(
            pf[0].get("scale_drift").and_then(JsonValue::as_str),
            Some("0.0000")
        );
        assert_eq!(
            out.get("status").and_then(JsonValue::as_str),
            Some("determined")
        );
    }

    #[test]
    fn track_jump_violation_golden() {
        // 单帧跳变: 帧 5 中心横移 80px > 40px 上限 → translation_exceeded
        let r = bbox("100", "100", "40", "20");
        let a = args(
            r.clone(),
            vec![trk(0, r.clone()), trk(5, bbox("180", "100", "40", "20"))],
            "40",
            "0.2",
            None,
        );
        let out = run(&a);
        assert_eq!(
            out.get("all_clean").and_then(JsonValue::as_bool),
            Some(false)
        );
        let v = out.get("violations").and_then(JsonValue::as_array).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].get("frame").and_then(JsonValue::as_i64), Some(5));
        assert_eq!(
            v[0].get("kind").and_then(JsonValue::as_str),
            Some("translation_exceeded")
        );
        assert_eq!(
            v[0].get("value").and_then(JsonValue::as_str),
            Some("80.000000")
        );
        assert_eq!(
            v[0].get("limit").and_then(JsonValue::as_str),
            Some("40.000000")
        );
        assert_eq!(
            out.get("max_translation_observed")
                .and_then(JsonValue::as_str),
            Some("80.000000")
        );
    }

    #[test]
    fn track_scale_gradient_violation_golden() {
        // 尺度渐变: 100x100 → 110x110, 漂移 21% > 20% 上限 → scale_exceeded
        let r = bbox("0", "0", "100", "100");
        let a = args(
            r.clone(),
            vec![trk(0, r.clone()), trk(1, bbox("0", "0", "110", "110"))],
            "500",
            "0.2",
            None,
        );
        let out = run(&a);
        let v = out.get("violations").and_then(JsonValue::as_array).unwrap();
        assert_eq!(v.len(), 1, "{v:?}");
        assert_eq!(
            v[0].get("kind").and_then(JsonValue::as_str),
            Some("scale_exceeded")
        );
        assert_eq!(
            v[0].get("value").and_then(JsonValue::as_str),
            Some("0.2100")
        );
        assert_eq!(
            v[0].get("limit").and_then(JsonValue::as_str),
            Some("0.2000")
        );
        assert_eq!(
            out.get("max_scale_observed").and_then(JsonValue::as_str),
            Some("0.2100")
        );
    }

    #[test]
    fn track_boundary_strict_and_metrics() {
        // 边界: 漂移恰等于上限不算超（严格大于）; 超一丝即超
        let r = bbox("0", "0", "10", "10");
        let exact = args(
            r.clone(),
            vec![trk(0, bbox("0", "0", "10", "12"))],
            "500",
            "0.2",
            None,
        );
        assert_eq!(
            run(&exact).get("all_clean").and_then(JsonValue::as_bool),
            Some(true)
        );
        let over = args(
            r.clone(),
            vec![trk(0, bbox("0", "0", "10", "12.000001"))],
            "500",
            "0.2",
            None,
        );
        assert_eq!(
            run(&over).get("all_clean").and_then(JsonValue::as_bool),
            Some(false)
        );

        // l2/linf: 3-4-5 三角精确锚 — 位移 (30,40): l2=50, linf=40
        let r2 = bbox("0", "0", "10", "10");
        let mv = bbox("30", "40", "10", "10");
        let l2 = args(
            r2.clone(),
            vec![trk(0, mv.clone())],
            "100",
            "0.5",
            Some("l2"),
        );
        assert_eq!(
            run(&l2)
                .get("per_frame")
                .and_then(JsonValue::as_array)
                .unwrap()[0]
                .get("translation")
                .and_then(JsonValue::as_str),
            Some("50.000000")
        );
        let linf = args(r2, vec![trk(0, mv)], "100", "0.5", Some("linf"));
        assert_eq!(
            run(&linf)
                .get("per_frame")
                .and_then(JsonValue::as_array)
                .unwrap()[0]
                .get("translation")
                .and_then(JsonValue::as_str),
            Some("40.000000")
        );
    }

    #[test]
    fn track_order_insensitive_and_double_run() {
        // 乱序入参 → 按帧号升序判定; 双跑位元一致
        let r = bbox("0", "0", "10", "10");
        let a = args(
            r.clone(),
            vec![trk(7, r.clone()), trk(2, r.clone()), trk(4, r.clone())],
            "40",
            "0.2",
            None,
        );
        let x = run(&a);
        let y = run(&a);
        assert_eq!(x, y, "同参双跑须位元一致");
        let pf = x.get("per_frame").and_then(JsonValue::as_array).unwrap();
        let frames: Vec<i64> = pf
            .iter()
            .filter_map(|v| v.get("frame").and_then(JsonValue::as_i64))
            .collect();
        assert_eq!(frames, vec![2, 4, 7], "须按帧号升序: {frames:?}");
    }

    #[test]
    fn track_rejects_bad_input() {
        let r = bbox("0", "0", "10", "10");
        // 空 tracks
        assert!(VideoTrackConsistencyEval
            .execute(&args(r.clone(), vec![], "40", "0.2", None))
            .is_err());
        // 缺必填
        assert!(VideoTrackConsistencyEval
            .execute(&JsonValue::object_from_pairs(&[("reference", r.clone())]))
            .is_err());
        // 帧号重复
        let dup = args(
            r.clone(),
            vec![trk(3, r.clone()), trk(3, r.clone())],
            "40",
            "0.2",
            None,
        );
        assert!(VideoTrackConsistencyEval.execute(&dup).is_err());
        // 负帧号
        let negf = args(r.clone(), vec![trk(-1, r.clone())], "40", "0.2", None);
        assert!(VideoTrackConsistencyEval.execute(&negf).is_err());
        // 零宽 bbox
        let zw = args(
            r.clone(),
            vec![trk(0, bbox("0", "0", "0", "10"))],
            "40",
            "0.2",
            None,
        );
        assert!(VideoTrackConsistencyEval.execute(&zw).is_err());
        // 未知 metric
        let badm = args(r.clone(), vec![trk(0, r.clone())], "40", "0.2", Some("l1"));
        assert!(VideoTrackConsistencyEval.execute(&badm).is_err());
        // 负值坐标
        let negc = args(
            bbox("-1", "0", "10", "10"),
            vec![trk(0, r.clone())],
            "40",
            "0.2",
            None,
        );
        assert!(VideoTrackConsistencyEval.execute(&negc).is_err());
        // 超 7 位整数段
        let bigc = args(
            bbox("12345678", "0", "10", "10"),
            vec![trk(0, r.clone())],
            "40",
            "0.2",
            None,
        );
        assert!(VideoTrackConsistencyEval.execute(&bigc).is_err());
    }
}

// ============================================================================
// QC 守恒谓词引擎三原语（qc_count_check / qc_domain_check / qc_temporal_check）
// — B7a 批次（统一守恒谓词引擎：结构计数+比例域校验+帧间守恒，规则表参数化）
// ============================================================================

/// QC 守恒核定点标度（复用 1e6 轨迹核标度；比值/域界/差值全量精确）
const QC_SCALE: i128 = 1_000_000;

/// qc 域数值解析（允许负值，域校验需要）：整数段 <=max_i 位、小数段 <=max_f 位
/// 纯数字（可选前导 -）。返回 QC_SCALE 精确定点值（scale 整除 1e6，零舍入）。
fn parse_qc_rational(raw: &str, ctx: &str, max_i: usize, max_f: usize) -> Result<i128, String> {
    let s = raw.trim();
    let (neg, body) = match s.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, s),
    };
    let (ip, fp) = match body.split_once('.') {
        None => (body, ""),
        Some((a, b)) => (a, b),
    };
    if body.is_empty()
        || (ip.is_empty() && fp.is_empty())
        || ip.len() > max_i
        || fp.len() > max_f
        || !ip.bytes().all(|b| b.is_ascii_digit())
        || !fp.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(format!(
            "{ctx} 非法数值 {raw:?} — 自诊断指引: 十进制形如 \"100.5\" 或 \"-0.25\"（整数 <={max_i} 位 + 小数 <={max_f} 位纯数字）"
        ));
    }
    let mut v: i128 = 0;
    for b in ip.bytes() {
        v = v * 10 + (b - b'0') as i128;
    }
    let mut scale: i128 = 1;
    for b in fp.bytes() {
        v = v * 10 + (b - b'0') as i128;
        scale *= 10;
    }
    let scaled = v * (QC_SCALE / scale);
    Ok(if neg { -scaled } else { scaled })
}

/// 1e6 标度值 → 6 位小数字符串
fn fmt_qc(v: i128) -> String {
    format!("{}.{:06}", v / QC_SCALE, v % QC_SCALE)
}

/// 守恒违规条目（规则 ID+有理数证据直喂台账）
fn qc_violation(rule_id: &str, kind: &str, detail: String) -> JsonValue {
    JsonValue::object_from_pairs(&[
        ("rule_id", JsonValue::string(rule_id)),
        ("kind", JsonValue::string(kind)),
        ("detail", JsonValue::string(detail)),
    ])
}

/// 从规则对象取字符串参数（可选，缺省回落引擎缺省值）
fn qc_rule_str<'a>(rule: &'a evorule_tcb::ObjectMap, key: &str) -> Option<&'a str> {
    rule.get(key).and_then(JsonValue::as_str)
}

// ----------------------------------------------------------------------------
// 原语一：qc_count_check — 结构计数判定
// ----------------------------------------------------------------------------

/// 结构计数判定核：对象计数与规则表期望计数比对。
///
/// 判定语义（01-启发录 §七 三件套之「结构计数」）：
/// - 输入 `counts`: {部件名: 计数} 映射（提取侧结构化喂入，整数）；
/// - 规则表逐条：`part`（部件名）+ `expect`（期望计数，必填整数）
///   + 可选 `min`/`max`（期望为区间时以区间为准，单值忽略）；
/// - 计数落在 [min,max]（闭区间，缺省 min=max=expect）即合规；
///   否则违例 `count_mismatch`（规则 ID+期望+实际证据）。
///
/// 确定性实现：计数=整数直读；区间端点整数；零浮点零近似。
struct QcCountCheck;

impl NativeService for QcCountCheck {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let counts = args.get("counts").and_then(JsonValue::as_object).ok_or(
            "缺少必填参数 counts — 自诊断指引: {{部件名: 计数}} 对象,如 {{\"fingers\": 5}}",
        )?;
        let rules = args
            .get("rules")
            .and_then(JsonValue::as_array)
            .ok_or("缺少必填参数 rules — 自诊断指引: 规则表数组,每项 {{rule_id, part, expect}} 或 {{rule_id, part, min, max}}")?;
        if rules.is_empty() {
            return Err("rules 须为非空数组 — 自诊断指引: 至少 1 条计数规则".to_string());
        }
        let mut violations: Vec<JsonValue> = Vec::new();
        for (i, r) in rules.iter().enumerate() {
            let obj = r.as_object().ok_or(format!(
                "rules[{i}] 须为规则对象 — 自诊断指引: {{rule_id, part, expect}}"
            ))?;
            let rule_id = req_str(obj, "rule_id", &format!("rules[{i}].rule_id"))?;
            let part = req_str(obj, "part", &format!("rules[{i}].part"))?;
            let actual = counts
                .get(part)
                .and_then(|v| match v {
                    JsonValue::Integer(n) => Some(*n),
                    JsonValue::U64(u) => i64::try_from(*u).ok(),
                    _ => None,
                })
                .ok_or(format!(
                    "rules[{i}] 部件 {part:?} 在 counts 中缺失或非整数 — 自诊断指引: counts 键须覆盖规则表全部 part"
                ))?;
            let get_i64 = |key: &str| -> Result<Option<i64>, String> {
                match obj.get(key) {
                    None | Some(JsonValue::Null) => Ok(None),
                    Some(JsonValue::Integer(n)) => Ok(Some(*n)),
                    Some(JsonValue::U64(u)) => i64::try_from(*u).map(Some).map_err(|_| {
                        format!("rules[{i}].{key}={u} 超出整数域 — 自诊断指引: 计数域为 i64 整数")
                    }),
                    Some(_) => Err(format!(
                        "rules[{i}].{key} 须为整数 — 自诊断指引: 计数域为整数,如 {{expect: 5}}"
                    )),
                }
            };
            let (lo, hi) = match (get_i64("min")?, get_i64("max")?, get_i64("expect")?) {
                (Some(lo), Some(hi), _) => {
                    if lo > hi {
                        return Err(format!(
                            "rules[{i}] min={lo} > max={hi} — 自诊断指引: 区间须 lo<=hi"
                        ));
                    }
                    (lo, hi)
                }
                (Some(_), None, _) | (None, Some(_), _) => {
                    return Err(format!(
                        "rules[{i}] min/max 须成对给出 — 自诊断指引: 区间规则同时给 min 与 max,单值规则用 expect"
                    ));
                }
                (None, None, Some(expect)) => (expect, expect),
                (None, None, None) => {
                    return Err(format!(
                        "rules[{i}] 缺 expect（或 min/max 区间）— 自诊断指引: 单值 {{expect}} 或区间 {{min,max}}"
                    ));
                }
            };
            if actual < lo || actual > hi {
                violations.push(qc_violation(
                    rule_id,
                    "count_mismatch",
                    format!("part={part:?} 期望 [{lo},{hi}] 实际 {actual}"),
                ));
            }
        }
        let all_clean = violations.is_empty();
        Ok(envelope(&[
            ("service", JsonValue::string("qc_count_check")),
            ("all_clean", JsonValue::Bool(all_clean)),
            ("violations", JsonValue::Array(violations)),
        ]))
    }
}

/// 声明表构造子：结构计数判定
pub fn mk_qc_count_check() -> Arc<dyn NativeService> {
    Arc::new(QcCountCheck)
}

// ----------------------------------------------------------------------------
// 原语二：qc_domain_check — 比例/数值域校验
// ----------------------------------------------------------------------------

/// 比例/数值域校验核：测量值落入规则域区间判定（骨比域/角度域/结构比域同构）。
///
/// 判定语义（01-启发录 §七 三件套之「比例/角度域校验」）：
/// - 输入 `measurements`: {测量名: 字符串有理数} 映射（提取侧喂入）；
/// - 规则表逐条：`name`（测量名）+ `min`/`max`（域界，字符串有理数，
///   闭区间，两者至少给一）+ 可选 `unit`（仅入证据，不参与判定）；
///   域界带解剖学文献锚的由规则 ID 承载溯源；
/// - 测量值出域即违例 `domain_violation`（规则 ID+域界+实际证据）；
/// - 判定面=1e6 定点整比较（零浮点；界整除 1e6 精确）。
struct QcDomainCheck;

impl NativeService for QcDomainCheck {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let measurements = args
            .get("measurements")
            .and_then(JsonValue::as_object)
            .ok_or("缺少必填参数 measurements — 自诊断指引: {{测量名: 字符串有理数}},如 {{\"bone_ratio\": \"0.65\"}}")?;
        let rules = args.get("rules").and_then(JsonValue::as_array).ok_or(
            "缺少必填参数 rules — 自诊断指引: 规则表数组,每项 {{rule_id, name, min, max}}",
        )?;
        if rules.is_empty() {
            return Err("rules 须为非空数组 — 自诊断指引: 至少 1 条域规则".to_string());
        }
        let mut violations: Vec<JsonValue> = Vec::new();
        for (i, r) in rules.iter().enumerate() {
            let obj = r.as_object().ok_or(format!(
                "rules[{i}] 须为规则对象 — 自诊断指引: {{rule_id, name, min, max}}"
            ))?;
            let rule_id = req_str(obj, "rule_id", &format!("rules[{i}].rule_id"))?;
            let name = req_str(obj, "name", &format!("rules[{i}].name"))?;
            let raw_min = qc_rule_str(obj, "min");
            let raw_max = qc_rule_str(obj, "max");
            if raw_min.is_none() && raw_max.is_none() {
                return Err(format!(
                    "rules[{i}] min/max 至少给一 — 自诊断指引: 域规则 {rule_id:?} 须给 min 或 max（可开区间）"
                ));
            }
            let min = raw_min
                .map(|s| parse_qc_rational(s, &format!("rules[{i}].min"), 7, 6))
                .transpose()?;
            let max = raw_max
                .map(|s| parse_qc_rational(s, &format!("rules[{i}].max"), 7, 6))
                .transpose()?;
            if let (Some(lo), Some(hi)) = (min, max) {
                if lo > hi {
                    return Err(format!(
                        "rules[{i}] min={lo} > max={hi} — 自诊断指引: 域须 lo<=hi"
                    ));
                }
            }
            let raw = measurements
                .get(name)
                .and_then(JsonValue::as_str)
                .ok_or(format!(
                    "rules[{i}] 测量 {name:?} 在 measurements 中缺失或非字符串 — 自诊断指引: measurements 键须覆盖规则表全部 name"
                ))?;
            let actual = parse_qc_rational(raw, &format!("measurements.{name}"), 7, 6)?;
            let below = matches!(min, Some(lo) if actual < lo);
            let above = matches!(max, Some(hi) if actual > hi);
            if below || above {
                let bound = if below {
                    format!("下界 {}", fmt_qc(min.unwrap_or(0)))
                } else {
                    format!("上界 {}", fmt_qc(max.unwrap_or(0)))
                };
                violations.push(qc_violation(
                    rule_id,
                    "domain_violation",
                    format!("measure={name:?} 出{bound},实际 {}", fmt_qc(actual)),
                ));
            }
        }
        let all_clean = violations.is_empty();
        Ok(envelope(&[
            ("service", JsonValue::string("qc_domain_check")),
            ("all_clean", JsonValue::Bool(all_clean)),
            ("violations", JsonValue::Array(violations)),
        ]))
    }
}

/// 声明表构造子：比例/数值域校验
pub fn mk_qc_domain_check() -> Arc<dyn NativeService> {
    Arc::new(QcDomainCheck)
}

// ----------------------------------------------------------------------------
// 原语三：qc_temporal_check — 帧间守恒判定
// ----------------------------------------------------------------------------

/// 帧间守恒判定核：逐帧观测序列的帧间差值越限判定（位移连续/面积连续/
/// 计数守恒同构——差值泛化到任意数值观测）。
///
/// 判定语义（01-启发录 §七 三件套之「帧间守恒」）：
/// - 输入 `frames`: [{frame: 非负整数唯一, values: {观测名: 字符串有理数}}]
///   （按帧号升序判定,输入序不敏感）；
/// - 规则表逐条：`name`（观测名）+ `max_delta`（相邻帧差值上限，字符串
///   有理数非负）——相邻帧 |Δ| 超限即违例 `delta_exceeded`
///   （规则 ID+帧号+观测名+实际差值证据）；
/// - 首帧无前帧不判定（无基线）；复用轨迹核纪律：帧号重复拒绝；
/// - 判定面=1e6 定点整比较零浮点；差值有向绝对值。
struct QcTemporalCheck;

impl NativeService for QcTemporalCheck {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let frames_in = args
            .get("frames")
            .and_then(JsonValue::as_array)
            .ok_or("缺少必填参数 frames — 自诊断指引: [{frame, values},...],frame 非负整数唯一")?;
        if frames_in.is_empty() {
            return Err(
                "frames 须为非空数组 — 自诊断指引: 至少 1 帧（首帧无前帧不判定）".to_string(),
            );
        }
        let rules = args.get("rules").and_then(JsonValue::as_array).ok_or(
            "缺少必填参数 rules — 自诊断指引: 规则表数组,每项 {{rule_id, name, max_delta}}",
        )?;
        if rules.is_empty() {
            return Err("rules 须为非空数组 — 自诊断指引: 至少 1 条守恒规则".to_string());
        }
        // 解析规则表（观测名+上限定点）
        let mut limits: Vec<(&str, &str, i128)> = Vec::with_capacity(rules.len());
        for (i, r) in rules.iter().enumerate() {
            let obj = r.as_object().ok_or(format!(
                "rules[{i}] 须为规则对象 — 自诊断指引: {{rule_id, name, max_delta}}"
            ))?;
            let rule_id = req_str(obj, "rule_id", &format!("rules[{i}].rule_id"))?;
            let name = req_str(obj, "name", &format!("rules[{i}].name"))?;
            let md = qc_rule_str(obj, "max_delta").ok_or(format!(
                "rules[{i}] 缺 max_delta — 自诊断指引: 相邻帧差值上限,字符串如 \"40\""
            ))?;
            let lim = parse_qc_rational(md, &format!("rules[{i}].max_delta"), 7, 6)?;
            if lim < 0 {
                return Err(format!(
                    "rules[{i}].max_delta={md:?} 须非负 — 自诊断指引: 差值上限为幅度非有向量"
                ));
            }
            limits.push((rule_id, name, lim));
        }
        // 解析全部帧（帧号唯一性守卫+按帧号升序判定）
        let mut frames: Vec<(i64, &evorule_tcb::ObjectMap)> = Vec::with_capacity(frames_in.len());
        for (i, f) in frames_in.iter().enumerate() {
            let obj = f
                .as_object()
                .ok_or(format!("frames[{i}] 须为对象 {{frame, values}}"))?;
            let frame = parse_trk_frame(
                obj.get("frame")
                    .ok_or_else(|| format!("frames[{i}] 缺 frame — 自诊断指引: 帧号必填"))?,
            )
            .map_err(|e| format!("frames[{i}] {e}"))?;
            frames.push((frame, obj));
        }
        frames.sort_by_key(|(f, _)| *f);
        for w in frames.windows(2) {
            if w[0].0 == w[1].0 {
                return Err(format!(
                    "frames 帧号重复: {} — 自诊断指引: 每帧恰一条,重复条目判据歧义故拒绝",
                    w[0].0
                ));
            }
        }
        // 逐规则逐相邻帧判定
        let mut violations: Vec<JsonValue> = Vec::new();
        for (rule_id, name, lim) in &limits {
            for w in frames.windows(2) {
                let (fa, va) = &w[0];
                let (fb, vb) = &w[1];
                let get_val = |o: &evorule_tcb::ObjectMap, tag: String| -> Result<i128, String> {
                    let raw = o
                        .get("values")
                        .and_then(JsonValue::as_object)
                        .and_then(|m| m.get(*name))
                        .and_then(JsonValue::as_str)
                        .ok_or(format!(
                            "帧 {tag} 缺观测 {name:?} — 自诊断指引: values 键须覆盖规则表全部观测名"
                        ))?;
                    parse_qc_rational(raw, &format!("frames[{tag}].values.{name}"), 7, 6)
                };
                let a = get_val(va, fa.to_string())?;
                let b = get_val(vb, fb.to_string())?;
                let delta = (b - a).abs();
                if delta > *lim {
                    violations.push(qc_violation(
                        rule_id,
                        "delta_exceeded",
                        format!(
                            "obs={name:?} 帧 {fa}->{fb} 差值 {} 超限 {}",
                            fmt_qc(delta),
                            fmt_qc(*lim)
                        ),
                    ));
                }
            }
        }
        let all_clean = violations.is_empty();
        Ok(envelope(&[
            ("service", JsonValue::string("qc_temporal_check")),
            ("all_clean", JsonValue::Bool(all_clean)),
            ("violations", JsonValue::Array(violations)),
        ]))
    }
}

/// 声明表构造子：帧间守恒判定
pub fn mk_qc_temporal_check() -> Arc<dyn NativeService> {
    Arc::new(QcTemporalCheck)
}

// ============================================================================
// QC 守恒三原语测试（golden：计数单值/区间/域出界/帧间跳变/边界恰等/双跑一致/拒收面）
// ============================================================================

#[cfg(test)]
mod qc_conservation_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn obj(pairs: &[(&str, JsonValue)]) -> JsonValue {
        JsonValue::object_from_pairs(pairs)
    }

    fn run_count(a: &JsonValue) -> JsonValue {
        QcCountCheck.execute(a).unwrap()
    }

    fn run_domain(a: &JsonValue) -> JsonValue {
        QcDomainCheck.execute(a).unwrap()
    }

    fn run_temporal(a: &JsonValue) -> JsonValue {
        QcTemporalCheck.execute(a).unwrap()
    }

    // ---- qc_count_check ----

    #[test]
    fn count_single_expect_clean_golden() {
        // 手部五指单值期望：实际 5 == expect 5 → 全清洁
        let a = obj(&[
            ("counts", obj(&[("fingers", JsonValue::Integer(5))])),
            (
                "rules",
                JsonValue::Array(vec![obj(&[
                    ("rule_id", JsonValue::string("hand.finger_count")),
                    ("part", JsonValue::string("fingers")),
                    ("expect", JsonValue::Integer(5)),
                ])]),
            ),
        ]);
        let out = run_count(&a);
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
    fn count_interval_and_violation_golden() {
        // 螺丝数区间 [3,5]：实际 4 合规；实际 8 违例且证据逐字
        let rule = obj(&[
            ("rule_id", JsonValue::string("device.screw_count")),
            ("part", JsonValue::string("screws")),
            ("min", JsonValue::Integer(3)),
            ("max", JsonValue::Integer(5)),
        ]);
        let mk = |n: i64| {
            obj(&[
                ("counts", obj(&[("screws", JsonValue::Integer(n))])),
                ("rules", JsonValue::Array(vec![rule.clone()])),
            ])
        };
        let ok = run_count(&mk(4));
        assert_eq!(ok.get("all_clean").and_then(JsonValue::as_bool), Some(true));
        let bad = run_count(&mk(8));
        assert_eq!(
            bad.get("all_clean").and_then(JsonValue::as_bool),
            Some(false)
        );
        let vs = bad.get("violations").and_then(JsonValue::as_array).unwrap();
        assert_eq!(vs.len(), 1);
        assert_eq!(
            vs[0].get("rule_id").and_then(JsonValue::as_str),
            Some("device.screw_count")
        );
        assert_eq!(
            vs[0].get("kind").and_then(JsonValue::as_str),
            Some("count_mismatch")
        );
        assert_eq!(
            vs[0].get("detail").and_then(JsonValue::as_str),
            Some("part=\"screws\" 期望 [3,5] 实际 8")
        );
    }

    #[test]
    fn count_rejects_min_without_max_and_missing_part() {
        // min 单独给 → 拒；part 不在 counts → 拒
        let a = obj(&[
            ("counts", obj(&[("fingers", JsonValue::Integer(5))])),
            (
                "rules",
                JsonValue::Array(vec![obj(&[
                    ("rule_id", JsonValue::string("x")),
                    ("part", JsonValue::string("fingers")),
                    ("min", JsonValue::Integer(1)),
                ])]),
            ),
        ]);
        assert!(QcCountCheck.execute(&a).is_err());
        let b = obj(&[
            ("counts", obj(&[("fingers", JsonValue::Integer(5))])),
            (
                "rules",
                JsonValue::Array(vec![obj(&[
                    ("rule_id", JsonValue::string("x")),
                    ("part", JsonValue::string("thumbs")),
                    ("expect", JsonValue::Integer(1)),
                ])]),
            ),
        ]);
        assert!(QcCountCheck.execute(&b).is_err());
    }

    // ---- qc_domain_check ----

    #[test]
    fn domain_bone_ratio_clean_golden() {
        // 示指近:中骨比域 [0.55,0.75]：实际 0.65 → 合规（文献锚经规则 ID 溯源）
        let a = obj(&[
            (
                "measurements",
                obj(&[("index_proximal_ratio", JsonValue::string("0.65"))]),
            ),
            (
                "rules",
                JsonValue::Array(vec![obj(&[
                    (
                        "rule_id",
                        JsonValue::string("hand.bone_ratio.index_prox_mid"),
                    ),
                    ("name", JsonValue::string("index_proximal_ratio")),
                    ("min", JsonValue::string("0.55")),
                    ("max", JsonValue::string("0.75")),
                ])]),
            ),
        ]);
        let out = run_domain(&a);
        assert_eq!(
            out.get("all_clean").and_then(JsonValue::as_bool),
            Some(true)
        );
        assert_eq!(
            out.get("status").and_then(JsonValue::as_str),
            Some("determined")
        );
    }

    #[test]
    fn domain_open_interval_violation_golden() {
        // 角度域仅给上界（开下界）：实际 45.0 > 上界 10.0 → 违例证据逐字
        let a = obj(&[
            (
                "measurements",
                obj(&[("dip_hyperextension_deg", JsonValue::string("45.0"))]),
            ),
            (
                "rules",
                JsonValue::Array(vec![obj(&[
                    ("rule_id", JsonValue::string("hand.angle.dip_overextension")),
                    ("name", JsonValue::string("dip_hyperextension_deg")),
                    ("max", JsonValue::string("10")),
                ])]),
            ),
        ]);
        let out = run_domain(&a);
        assert_eq!(
            out.get("all_clean").and_then(JsonValue::as_bool),
            Some(false)
        );
        let vs = out.get("violations").and_then(JsonValue::as_array).unwrap();
        assert_eq!(vs.len(), 1);
        assert_eq!(
            vs[0].get("kind").and_then(JsonValue::as_str),
            Some("domain_violation")
        );
        assert_eq!(
            vs[0].get("detail").and_then(JsonValue::as_str),
            Some("measure=\"dip_hyperextension_deg\" 出上界 10.000000,实际 45.000000")
        );
    }

    #[test]
    fn domain_boundary_exact_and_negative_measurement() {
        // 边界恰等=合规（闭区间语义）；负值测量合法（温度类观测）
        let a = obj(&[
            ("measurements", obj(&[("v", JsonValue::string("0.75"))])),
            (
                "rules",
                JsonValue::Array(vec![obj(&[
                    ("rule_id", JsonValue::string("x")),
                    ("name", JsonValue::string("v")),
                    ("max", JsonValue::string("0.75")),
                ])]),
            ),
        ]);
        assert_eq!(
            run_domain(&a).get("all_clean").and_then(JsonValue::as_bool),
            Some(true)
        );
        let b = obj(&[
            ("measurements", obj(&[("t", JsonValue::string("-12.5"))])),
            (
                "rules",
                JsonValue::Array(vec![obj(&[
                    ("rule_id", JsonValue::string("y")),
                    ("name", JsonValue::string("t")),
                    ("min", JsonValue::string("-100")),
                    ("max", JsonValue::string("100")),
                ])]),
            ),
        ]);
        assert_eq!(
            run_domain(&b).get("all_clean").and_then(JsonValue::as_bool),
            Some(true)
        );
    }

    #[test]
    fn domain_rejects_no_bounds_and_bad_scale() {
        // min/max 全缺 → 拒；7 位小数超限形 → 拒
        let a = obj(&[
            ("measurements", obj(&[("v", JsonValue::string("1"))])),
            (
                "rules",
                JsonValue::Array(vec![obj(&[
                    ("rule_id", JsonValue::string("x")),
                    ("name", JsonValue::string("v")),
                ])]),
            ),
        ]);
        assert!(QcDomainCheck.execute(&a).is_err());
        let b = obj(&[
            (
                "measurements",
                obj(&[("v", JsonValue::string("0.1234567"))]),
            ),
            (
                "rules",
                JsonValue::Array(vec![obj(&[
                    ("rule_id", JsonValue::string("x")),
                    ("name", JsonValue::string("v")),
                    ("max", JsonValue::string("1")),
                ])]),
            ),
        ]);
        assert!(QcDomainCheck.execute(&b).is_err());
    }

    // ---- qc_temporal_check ----

    #[test]
    fn temporal_uniform_clean_golden() {
        // 位移守恒：x 帧间匀速步进 10px <= 上限 40 → 全清洁
        let f = |n: i64, x: i128| {
            obj(&[
                ("frame", JsonValue::Integer(n)),
                (
                    "values",
                    obj(&[("center_x", JsonValue::string(format!("{x}.0")))]),
                ),
            ])
        };
        let a = obj(&[
            (
                "frames",
                JsonValue::Array(vec![f(0, 100), f(1, 110), f(2, 120)]),
            ),
            (
                "rules",
                JsonValue::Array(vec![obj(&[
                    ("rule_id", JsonValue::string("track.displacement")),
                    ("name", JsonValue::string("center_x")),
                    ("max_delta", JsonValue::string("40")),
                ])]),
            ),
        ]);
        let out = run_temporal(&a);
        assert_eq!(
            out.get("all_clean").and_then(JsonValue::as_bool),
            Some(true)
        );
        assert_eq!(
            out.get("status").and_then(JsonValue::as_str),
            Some("determined")
        );
    }

    #[test]
    fn temporal_jump_violation_and_frame_order_golden() {
        // 单帧跳变 80 > 40 → 违例；乱序入参按帧号升序判定（输入序不敏感）
        let f = |n: i64, x: &str| {
            obj(&[
                ("frame", JsonValue::Integer(n)),
                ("values", obj(&[("center_x", JsonValue::string(x))])),
            ])
        };
        let a = obj(&[
            (
                "frames",
                JsonValue::Array(vec![f(5, "180.0"), f(0, "100.0")]),
            ),
            (
                "rules",
                JsonValue::Array(vec![obj(&[
                    ("rule_id", JsonValue::string("track.displacement")),
                    ("name", JsonValue::string("center_x")),
                    ("max_delta", JsonValue::string("40")),
                ])]),
            ),
        ]);
        let out = run_temporal(&a);
        assert_eq!(
            out.get("all_clean").and_then(JsonValue::as_bool),
            Some(false)
        );
        let vs = out.get("violations").and_then(JsonValue::as_array).unwrap();
        assert_eq!(vs.len(), 1);
        assert_eq!(
            vs[0].get("kind").and_then(JsonValue::as_str),
            Some("delta_exceeded")
        );
        assert_eq!(
            vs[0].get("detail").and_then(JsonValue::as_str),
            Some("obs=\"center_x\" 帧 0->5 差值 80.000000 超限 40.000000")
        );
    }

    #[test]
    fn temporal_negative_delta_uses_abs_and_boundary_exact() {
        // 负向跳变取绝对值判定（100->50 差值 50 越限）；差值恰等上限=合规（严格大于语义）
        let f = |n: i64, x: &str| {
            obj(&[
                ("frame", JsonValue::Integer(n)),
                ("values", obj(&[("v", JsonValue::string(x))])),
            ])
        };
        let rule = obj(&[
            ("rule_id", JsonValue::string("r")),
            ("name", JsonValue::string("v")),
            ("max_delta", JsonValue::string("40")),
        ]);
        let neg = obj(&[
            (
                "frames",
                JsonValue::Array(vec![f(0, "100.0"), f(1, "50.0")]),
            ),
            ("rules", JsonValue::Array(vec![rule.clone()])),
        ]);
        assert_eq!(
            run_temporal(&neg)
                .get("all_clean")
                .and_then(JsonValue::as_bool),
            Some(false)
        );
        let exact = obj(&[
            (
                "frames",
                JsonValue::Array(vec![f(0, "100.0"), f(1, "140.0")]),
            ),
            ("rules", JsonValue::Array(vec![rule])),
        ]);
        assert_eq!(
            run_temporal(&exact)
                .get("all_clean")
                .and_then(JsonValue::as_bool),
            Some(true)
        );
    }

    #[test]
    fn temporal_double_run_identical_and_rejections() {
        // 双跑位元一致 + 拒收面：重复帧号/缺观测/负 max_delta/空 frames
        let f = |n: i64, x: &str| {
            obj(&[
                ("frame", JsonValue::Integer(n)),
                ("values", obj(&[("v", JsonValue::string(x))])),
            ])
        };
        let a = obj(&[
            ("frames", JsonValue::Array(vec![f(0, "1.0"), f(1, "2.0")])),
            (
                "rules",
                JsonValue::Array(vec![obj(&[
                    ("rule_id", JsonValue::string("r")),
                    ("name", JsonValue::string("v")),
                    ("max_delta", JsonValue::string("5")),
                ])]),
            ),
        ]);
        let x = run_temporal(&a);
        let y = run_temporal(&a);
        assert_eq!(x, y, "同参双跑位元一致");

        let dup = obj(&[
            ("frames", JsonValue::Array(vec![f(1, "1.0"), f(1, "2.0")])),
            ("rules", JsonValue::Array(vec![])),
        ]);
        assert!(QcTemporalCheck.execute(&dup).is_err());

        let empty = obj(&[
            ("frames", JsonValue::Array(vec![])),
            ("rules", JsonValue::Array(vec![])),
        ]);
        assert!(QcTemporalCheck.execute(&empty).is_err());

        let neglim = obj(&[
            ("frames", JsonValue::Array(vec![f(0, "1.0"), f(1, "2.0")])),
            (
                "rules",
                JsonValue::Array(vec![obj(&[
                    ("rule_id", JsonValue::string("r")),
                    ("name", JsonValue::string("v")),
                    ("max_delta", JsonValue::string("-1")),
                ])]),
            ),
        ]);
        assert!(QcTemporalCheck.execute(&neglim).is_err());

        let missing = obj(&[
            (
                "frames",
                JsonValue::Array(vec![
                    obj(&[
                        ("frame", JsonValue::Integer(0)),
                        ("values", obj(&[("w", JsonValue::string("1.0"))])),
                    ]),
                    f(1, "2.0"),
                ]),
            ),
            (
                "rules",
                JsonValue::Array(vec![obj(&[
                    ("rule_id", JsonValue::string("r")),
                    ("name", JsonValue::string("v")),
                    ("max_delta", JsonValue::string("5")),
                ])]),
            ),
        ]);
        assert!(QcTemporalCheck.execute(&missing).is_err());
    }
}

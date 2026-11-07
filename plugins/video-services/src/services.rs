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

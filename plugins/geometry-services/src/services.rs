//! 几何三服务的参数解析与执行（collision / clearance / occlusion）
//!
//! 共用输入形态：`parts` 数组，元素 = `{"id","shape","z"?}`；
//! 绘制序 = 数组下标（先绘在下、后绘在上，画家算法）。
//! `z` 为可选声明层（同层不判遮挡序，由绘制序决定覆盖）。

use std::sync::Arc;

use evorule_reactor::IoResult;
use evorule_tcb::JsonValue;

use evorule_plugin_kit::NativeService;

use crate::geometry::{
    num_f64, parse_shape, shapes_dist, Pt, Shape, EPS,
};

/// 零件数硬上限（两两判定 O(n²)，上限防滥用；超限诚实拒绝）
pub const MAX_PARTS: usize = 4096;

/// f64 → JsonValue 字符串（`{:?}` 最短往返，与 indicator-services 同款边界）
pub(crate) fn float_str(f: f64) -> JsonValue {
    JsonValue::string(format!("{f:?}"))
}

/// 零件（绘制序 = 解析序下标）
struct Part {
    id: String,
    shape: Shape,
    z: Option<f64>,
}

/// 解析并校验 parts：id 非空且唯一、shape 合法、z 可选数值
fn parse_parts(args: &JsonValue) -> Result<Vec<Part>, String> {
    let arr = args
        .get("parts")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| {
            "缺少必填参数 parts — 自诊断指引: 形如 [{\"id\":\"base\",\"shape\":{\
             \"type\":\"bbox\",\"x\":0,\"y\":0,\"w\":10,\"h\":5}}, ...]，绘制序 = 数组下标"
                .to_string()
        })?;
    if arr.is_empty() {
        return Err("parts 不可为空 — 自诊断指引: 至少 1 个零件才能构成判定对".to_string());
    }
    if arr.len() > MAX_PARTS {
        return Err(format!(
            "parts={}/{MAX_PARTS} 超上限 — 自诊断指引: 两两判定 O(n²)，超限请分批",
            arr.len()
        ));
    }
    let mut out = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let obj = item.as_object().ok_or_else(|| {
            format!("parts[{i}] 须为对象 — 自诊断指引: {{\"id\",\"shape\",\"z\"?}}")
        })?;
        let id = obj
            .get("id")
            .and_then(JsonValue::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("parts[{i}] 缺非空 id — 自诊断指引: id 为零件唯一标识"))?;
        let shape_v = obj
            .get("shape")
            .ok_or_else(|| format!("parts[{i}]({id}) 缺 shape — 自诊断指引: 见 shape 三形态"))?;
        let shape = parse_shape(shape_v).map_err(|e| format!("parts[{i}]({id}) 形状非法: {e}"))?;
        let z = match obj.get("z") {
            None | Some(JsonValue::Null) => None,
            Some(v) => Some(num_f64(v).ok_or_else(|| {
                format!("parts[{i}]({id}) z 须为合法数值层 — 自诊断指引: z 声明逻辑层高")
            })?),
        };
        out.push(Part {
            id: id.to_string(),
            shape,
            z,
        });
    }
    let mut ids: Vec<&str> = out.iter().map(|p| p.id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.len() != out.len() {
        return Err("parts 存在重复 id — 自诊断指引: id 唯一是配对报告可定位的前提".to_string());
    }
    Ok(out)
}

// ============================================================================
// 三服务
// ============================================================================

/// 内部执行核心：两两距离全表 + 按阈值产出违规清单
fn pairwise(args: &JsonValue, mode: Mode) -> Result<JsonValue, String> {
    let parts = parse_parts(args)?;
    let limit = match mode {
        Mode::Collision => EPS,
        Mode::Clearance => {
            let v = args.get("min_clearance").and_then(num_f64).ok_or_else(|| {
                "缺少必填参数 min_clearance — 自诊断指引: 间隙下限（数值，与坐标同单位）"
                    .to_string()
            })?;
            if v < 0.0 {
                return Err(format!("min_clearance 须非负 — 自诊断指引: 实得 {v}"));
            }
            v
        }
        Mode::Occlusion => {
            if parts.iter().any(|p| p.z.is_none()) {
                let missing: Vec<&str> = parts
                    .iter()
                    .filter(|p| p.z.is_none())
                    .map(|p| p.id.as_str())
                    .collect();
                return Err(format!(
                    "遮挡序检查要求全部零件声明 z — 自诊断指引: 缺失方 = [{}]",
                    missing.join(", ")
                ));
            }
            0.0
        }
    };
    let n = parts.len();
    let mut violations: Vec<JsonValue> = Vec::new();
    let mut min_gap = f64::INFINITY;
    let mut overlapping_pairs = 0usize;
    for i in 0..n {
        for j in (i + 1)..n {
            let (a, b) = (&parts[i], &parts[j]);
            let gap = shapes_dist(&a.shape, &b.shape);
            if gap <= EPS {
                overlapping_pairs += 1;
            }
            if gap < min_gap {
                min_gap = gap;
            }
            let hit = match mode {
                // 碰撞:触即算(距离 ≤ EPS)
                Mode::Collision => gap <= EPS,
                // 间隙:宽容方向 — gap ≥ 限值 − EPS 视为达标(容差语义见模块头声明)
                Mode::Clearance => gap < limit - EPS,
                // 遮挡:重叠对中,后绘者 z 更低 = 违规(同层不算)
                Mode::Occlusion => {
                    gap <= EPS
                        && match (a.z, b.z) {
                            (Some(za), Some(zb)) => zb < za - EPS,
                            _ => false,
                        }
                }
            };
            if hit {
                let mut fields = vec![
                    ("a", JsonValue::string(a.id.clone())),
                    ("b", JsonValue::string(b.id.clone())),
                ];
                fields.push(match mode {
                    Mode::Collision => ("gap", float_str(gap)),
                    Mode::Clearance => ("gap", float_str(gap)),
                    Mode::Occlusion => (
                        "z_earlier_later",
                        JsonValue::string(format!(
                            "{:?}/{:?}",
                            a.z.unwrap_or_default(),
                            b.z.unwrap_or_default()
                        )),
                    ),
                });
                if mode == Mode::Occlusion {
                    fields.push(("order_earlier", JsonValue::Integer(i as i64)));
                    fields.push(("order_later", JsonValue::Integer(j as i64)));
                }
                violations.push(JsonValue::object_from_pairs(&fields));
            }
        }
    }
    let checked_pairs = n * (n - 1) / 2;
    // 统一信封规范 v1（加性保留键）：既有字段全部原位保留，
    // 仅新增 status / determinism 两个保留键。
    // status 恒为 "determined"：容差方向是声明即契约（碰撞触即算、间隙内缩
    // EPS），每个可解析输入都得到唯一确定判定，本族无 undetermined 路径
    // （S2 登记实测修正）。
    // determinism = "corpus_locked"：f64 域，双跑位元一致 + 语料回归锁定。
    // error 路径维持框架 Err(String) 通道（IoResult 契约），不入成功信封。
    let mut out = vec![
        ("status", JsonValue::string("determined")),
        ("determinism", JsonValue::string("corpus_locked")),
        ("parts", JsonValue::Integer(n as i64)),
        ("checked_pairs", JsonValue::Integer(checked_pairs as i64)),
        ("violations", JsonValue::Array(violations.clone())),
        ("all_clean", JsonValue::Bool(violations.is_empty())),
        (
            "min_gap",
            if min_gap.is_finite() {
                float_str(min_gap)
            } else {
                JsonValue::Null
            },
        ),
    ];
    if mode == Mode::Occlusion {
        out.push((
            "overlapping_pairs",
            JsonValue::Integer(overlapping_pairs as i64),
        ));
    }
    Ok(JsonValue::object_from_pairs(&out))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Collision,
    Clearance,
    Occlusion,
}

// ============================================================================
// NativeService 薄壳
// ============================================================================

macro_rules! geometry_service {
    ($name:ident, $desc:expr, $mode:expr) => {
        pub struct $name;

        impl NativeService for $name {
            fn execute(&self, args: &JsonValue) -> IoResult {
                pairwise(args, $mode)
            }
        }
    };
}

geometry_service!(GeometryCollisionCheck, "几何碰撞检测", Mode::Collision);
geometry_service!(GeometryClearanceCheck, "几何间隙检查", Mode::Clearance);
geometry_service!(GeometryOcclusionCheck, "几何遮挡序检查", Mode::Occlusion);

/// 声明表构造子：碰撞检测
pub fn mk_collision_check() -> Arc<dyn NativeService> {
    Arc::new(GeometryCollisionCheck)
}

/// 声明表构造子：间隙检查
pub fn mk_clearance_check() -> Arc<dyn NativeService> {
    Arc::new(GeometryClearanceCheck)
}

/// 声明表构造子：遮挡序检查
pub fn mk_occlusion_check() -> Arc<dyn NativeService> {
    Arc::new(GeometryOcclusionCheck)
}

// ============================================================================
// 等距图样三服务（iso_project / tube_route / figure_audit）
// ============================================================================

use crate::figure::{
    bend_info, fit_transform, iso_project_pt, len3, prism_prism_gap_lb, prism_tube_gap_lb,
    parse_pt3, sub3, supported_by, tube_tube_gap, BendInfo, Fit, Prism, Pt3, TubeSeg, MAX_POINTS,
    MAX_WAYPOINTS,
};

/// 信封保留键（统一信封规范 v1）
fn envelope() -> Vec<(&'static str, JsonValue)> {
    vec![
        ("status", JsonValue::string("determined")),
        ("determinism", JsonValue::string("corpus_locked")),
    ]
}

/// 角度参数解析：声明区间即契约
fn parse_angle(args: &JsonValue, key: &str, default: f64, lo: f64, hi: f64, lo_eq: bool) -> Result<f64, String> {
    match args.get(key) {
        None | Some(JsonValue::Null) => Ok(default),
        Some(v) => {
            let d = num_f64(v).ok_or_else(|| {
                format!("{key} 须为合法数值角度 — 自诊断指引: 拒绝 NaN/Inf")
            })?;
            let ok = if lo_eq { d >= lo && d < hi } else { d > lo && d < hi };
            if ok {
                Ok(d)
            } else {
                Err(format!(
                    "{key}={d} 超出声明区间 — 自诊断指引: 合法区间见服务说明"
                ))
            }
        }
    }
}

fn pair_points(points: &[JsonValue], ctx: &str) -> Result<Vec<Pt3>, String> {
    points.iter().enumerate().map(|(i, v)| parse_pt3(v, &format!("{ctx}[{i}]"))).collect()
}

// ---- geometry_iso_project ----

pub struct GeometryIsoProject;

impl NativeService for GeometryIsoProject {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let pts_v = args
            .get("points")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| {
                "缺少必填参数 points — 自诊断指引: 形如 [[x,y,z],...]（≥1 点）".to_string()
            })?;
        if pts_v.is_empty() || pts_v.len() > MAX_POINTS {
            return Err(format!(
                "points 数量 {}/{MAX_POINTS} 越界 — 自诊断指引: ≥1 且 ≤{MAX_POINTS}",
                pts_v.len()
            ));
        }
        let pts = pair_points(pts_v, "points")?;
        let az = parse_angle(args, "azimuth_deg", 45.0, 0.0, 360.0, true)?;
        let el = parse_angle(args, "elevation_deg", 30.0, 0.0, 90.0, false)?;
        let raw: Vec<Pt> = pts.iter().map(|p| iso_project_pt(*p, az, el)).collect();
        let (scale, ox, oy) = match args.get("fit") {
            None | Some(JsonValue::Null) => (1.0, 0.0, 0.0),
            Some(f) => {
                let obj = f
                    .as_object()
                    .ok_or_else(|| "fit 须为对象 — 自诊断指引: {\"width\",\"height\",\"pad\"?}".to_string())?;
                let num = |k: &str| -> Result<f64, String> {
                    obj.get(k)
                        .and_then(num_f64)
                        .ok_or_else(|| format!("fit 缺合法数值字段 {k}"))
                };
                let (width, height) = (num("width")?, num("height")?);
                let pad = match obj.get("pad") {
                    None | Some(JsonValue::Null) => 0.0,
                    Some(v) => num_f64(v)
                        .ok_or_else(|| "fit.pad 须为合法数值".to_string())?,
                };
                if width <= 0.0 || height <= 0.0 {
                    return Err("fit 视口宽高须为正".to_string());
                }
                if pad < 0.0 || 2.0 * pad >= width.min(height) {
                    return Err(format!(
                        "fit.pad={pad} 非法 — 自诊断指引: pad ≥ 0 且 2·pad < min(width,height)"
                    ));
                }
                fit_transform(
                    &raw,
                    &Fit {
                        width,
                        height,
                        pad,
                    },
                )
            }
        };
        let projected: Vec<JsonValue> = raw
            .iter()
            .map(|p| {
                JsonValue::Array(vec![
                    crate::figure::fnum(p.x * scale + ox),
                    crate::figure::fnum(p.y * scale + oy),
                ])
            })
            .collect();
        let mut xs: Vec<f64> = raw.iter().map(|p| p.x).collect();
        let mut ys: Vec<f64> = raw.iter().map(|p| p.y).collect();
        xs.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        ys.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let (minx, miny) = (xs[0], ys[0]);
        let (maxx, maxy) = (xs[xs.len() - 1], ys[ys.len() - 1]);
        let mut out = envelope();
        out.push(("points", JsonValue::Integer(pts.len() as i64)));
        out.push(("projected", JsonValue::Array(projected)));
        out.push(("scale", crate::figure::fnum(scale)));
        out.push(("offset_x", crate::figure::fnum(ox)));
        out.push(("offset_y", crate::figure::fnum(oy)));
        out.push((
            "bbox_in",
            JsonValue::Array(vec![
                crate::figure::fnum(minx),
                crate::figure::fnum(miny),
                crate::figure::fnum(maxx),
                crate::figure::fnum(maxy),
            ]),
        ));
        Ok(JsonValue::object_from_pairs(&out))
    }
}

pub fn mk_iso_project() -> Arc<dyn NativeService> {
    Arc::new(GeometryIsoProject)
}

// ---- geometry_tube_route ----

pub struct GeometryTubeRoute;

impl NativeService for GeometryTubeRoute {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let wps_v = args
            .get("waypoints")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| {
                "缺少必填参数 waypoints — 自诊断指引: 形如 [[x,y,z],...]（≥2 点）".to_string()
            })?;
        if wps_v.len() < 2 || wps_v.len() > MAX_WAYPOINTS {
            return Err(format!(
                "waypoints 数量 {}/{MAX_WAYPOINTS} 越界 — 自诊断指引: ≥2 且 ≤{MAX_WAYPOINTS}",
                wps_v.len()
            ));
        }
        let wps = pair_points(wps_v, "waypoints")?;
        let min_r = match args.get("min_bend_radius") {
            None | Some(JsonValue::Null) => None,
            Some(v) => {
                let r = num_f64(v)
                    .ok_or_else(|| "min_bend_radius 须为合法数值 — 自诊断指引: ≥0".to_string())?;
                if r < 0.0 {
                    return Err(format!("min_bend_radius={r} 须非负"));
                }
                Some(r)
            }
        };
        let n = wps.len();
        let mut seg_len: Vec<f64> = Vec::with_capacity(n - 1);
        for i in 0..(n - 1) {
            let l = len3(sub3(wps[i + 1], wps[i]));
            if l <= EPS {
                return Err(format!(
                    "waypoints[{i}]→[{}] 零长段 — 自诊断指引: 相邻转折点须距离 > {EPS}",
                    i + 1
                ));
            }
            seg_len.push(l);
        }
        // 转角与切线需求（有限弯径时）
        let mut turns: Vec<JsonValue> = Vec::new();
        let mut tangents: Vec<f64> = Vec::new();
        let mut violations: Vec<JsonValue> = Vec::new();
        for i in 1..(n - 1) {
            let din = sub3(wps[i], wps[i - 1]);
            let dout = sub3(wps[i + 1], wps[i]);
            let lin = len3(din);
            let lout = len3(dout);
            let info: BendInfo = bend_info(
                Pt3 {
                    x: din.x / lin,
                    y: din.y / lin,
                    z: din.z / lin,
                },
                Pt3 {
                    x: dout.x / lout,
                    y: dout.y / lout,
                    z: dout.z / lout,
                },
                min_r,
            );
            if min_r.is_some() && info.tangent_len.is_infinite() {
                violations.push(JsonValue::object_from_pairs(&[
                    ("kind", JsonValue::string("bend_unroutable")),
                    ("waypoint", JsonValue::Integer(i as i64)),
                    ("turn_deg", crate::figure::fnum(info.turn_deg)),
                ]));
            }
            tangents.push(info.tangent_len);
            turns.push(JsonValue::object_from_pairs(&[
                ("waypoint", JsonValue::Integer(i as i64)),
                ("turn_deg", crate::figure::fnum(info.turn_deg)),
                (
                    "tangent_len",
                    if info.tangent_len.is_finite() {
                        crate::figure::fnum(info.tangent_len)
                    } else {
                        JsonValue::Null
                    },
                ),
            ]));
        }
        // 弯径占用量按段核算：leg i 两端转角切线之和 ≤ 段长（声明即契约）
        if min_r.is_some() {
            for i in 0..(n - 1) {
                let mut used = 0.0;
                if i > 0 {
                    used += tangents[i - 1];
                }
                if i < tangents.len() {
                    used += tangents[i];
                }
                if used.is_finite() && used > seg_len[i] + EPS {
                    violations.push(JsonValue::object_from_pairs(&[
                        ("kind", JsonValue::string("bend_exceeds_leg")),
                        ("leg", JsonValue::Integer(i as i64)),
                        ("required", crate::figure::fnum(used)),
                        ("available", crate::figure::fnum(seg_len[i])),
                    ]));
                }
            }
        }
        let total: f64 = seg_len.iter().sum();
        let mut out = envelope();
        out.push(("waypoints", JsonValue::Integer(n as i64)));
        out.push(("segments", JsonValue::Integer((n - 1) as i64)));
        out.push(("total_length", crate::figure::fnum(total)));
        out.push(("turns", JsonValue::Array(turns)));
        out.push(("violations", JsonValue::Array(violations.clone())));
        out.push(("all_clean", JsonValue::Bool(violations.is_empty())));
        Ok(JsonValue::object_from_pairs(&out))
    }
}

pub fn mk_tube_route() -> Arc<dyn NativeService> {
    Arc::new(GeometryTubeRoute)
}

// ---- geometry_figure_audit ----

/// 场景实体：棱柱或管路
enum Solid {
    Prism { id: String, prism: Prism },
    Tube { id: String, segs: Vec<TubeSeg> },
}

impl Solid {
    fn id(&self) -> &str {
        match self {
            Solid::Prism { id, .. } => id,
            Solid::Tube { id, .. } => id,
        }
    }
}

pub struct GeometryFigureAudit;

impl GeometryFigureAudit {
    fn parse_solids(args: &JsonValue) -> Result<Vec<Solid>, String> {
        let arr = args
            .get("solids")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| {
                "缺少必填参数 solids — 自诊断指引: [{\"id\",\"kind\":\"prism\"|\"tube\",...}]".to_string()
            })?;
        if arr.is_empty() || arr.len() > MAX_PARTS {
            return Err(format!(
                "solids 数量 {}/{MAX_PARTS} 越界 — 自诊断指引: ≥1 且 ≤{MAX_PARTS}",
                arr.len()
            ));
        }
        let mut out = Vec::with_capacity(arr.len());
        let mut total_segs = 0usize;
        for (i, item) in arr.iter().enumerate() {
            let obj = item
                .as_object()
                .ok_or_else(|| format!("solids[{i}] 须为对象"))?;
            let id = obj
                .get("id")
                .and_then(JsonValue::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| format!("solids[{i}] 缺非空 id"))?;
            let kind = obj
                .get("kind")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| format!("solids[{i}]({id}) 缺 kind — 自诊断指引: kind ∈ prism | tube"))?;
            match kind {
                "prism" => {
                    let pts_v = obj
                        .get("points")
                        .and_then(JsonValue::as_array)
                        .ok_or_else(|| format!("solids[{i}]({id}) 棱柱缺 points — 自诊断指引: [[x,y],...] ≥3 点"))?;
                    if pts_v.len() < 3 {
                        return Err(format!("solids[{i}]({id}) 棱柱顶点数须 ≥3 — 实得 {}", pts_v.len()));
                    }
                    let mut poly = Vec::with_capacity(pts_v.len());
                    for (j, pv) in pts_v.iter().enumerate() {
                        let pair = pv
                            .as_array()
                            .filter(|a| a.len() == 2)
                            .ok_or_else(|| format!("solids[{i}]({id}) 第 {j} 顶点须为 [x,y] 数对"))?;
                        let (x, y) = (
                            num_f64(&pair[0]).ok_or_else(|| format!("solids[{i}]({id}) 第 {j} 顶点 x 非法"))?,
                            num_f64(&pair[1]).ok_or_else(|| format!("solids[{i}]({id}) 第 {j} 顶点 y 非法"))?,
                        );
                        poly.push(Pt { x, y });
                    }
                    let base = obj
                        .get("base")
                        .and_then(num_f64)
                        .ok_or_else(|| format!("solids[{i}]({id}) 缺 base"))?;
                    let top = obj
                        .get("top")
                        .and_then(num_f64)
                        .ok_or_else(|| format!("solids[{i}]({id}) 缺 top"))?;
                    if top - base <= EPS {
                        return Err(format!(
                            "solids[{i}]({id}) 高度退化 — 自诊断指引: top−base 须 > {EPS}"
                        ));
                    }
                    out.push(Solid::Prism {
                        id: id.to_string(),
                        prism: Prism { poly, base, top },
                    });
                }
                "tube" => {
                    let wps_v = obj
                        .get("waypoints")
                        .and_then(JsonValue::as_array)
                        .ok_or_else(|| format!("solids[{i}]({id}) 管路缺 waypoints"))?;
                    if wps_v.len() < 2 || wps_v.len() > MAX_WAYPOINTS {
                        return Err(format!(
                            "solids[{i}]({id}) waypoints 数量 {} 越界 — 须 ≥2 且 ≤{MAX_WAYPOINTS}",
                            wps_v.len()
                        ));
                    }
                    let wps = pair_points(wps_v, &format!("solids[{i}]({id}).waypoints"))?;
                    let r = obj
                        .get("radius")
                        .and_then(num_f64)
                        .ok_or_else(|| format!("solids[{i}]({id}) 缺 radius"))?;
                    if r < 0.0 {
                        return Err(format!("solids[{i}]({id}) radius={r} 须非负"));
                    }
                    let mut segs = Vec::with_capacity(wps.len() - 1);
                    for k in 0..(wps.len() - 1) {
                        if len3(sub3(wps[k + 1], wps[k])) <= EPS {
                            return Err(format!(
                                "solids[{i}]({id}) 第 {k} 段零长 — 相邻转折点须距离 > {EPS}"
                            ));
                        }
                        segs.push(TubeSeg {
                            a: wps[k],
                            b: wps[k + 1],
                            r,
                        });
                    }
                    total_segs += segs.len();
                    out.push(Solid::Tube {
                        id: id.to_string(),
                        segs,
                    });
                }
                other => {
                    return Err(format!(
                        "solids[{i}]({id}) kind 未知: {other} — 自诊断指引: kind ∈ prism | tube"
                    ))
                }
            }
        }
        if total_segs > MAX_POINTS {
            return Err(format!(
                "管段总数 {total_segs}/{MAX_POINTS} 超限 — 自诊断指引: 两两核算量随段数平方增长，超限请分批"
            ));
        }
        let mut ids: Vec<&str> = out.iter().map(|s| s.id()).collect();
        ids.sort_unstable();
        ids.dedup();
        if ids.len() != out.len() {
            return Err("solids 存在重复 id".to_string());
        }
        Ok(out)
    }

    fn solid_gap(a: &Solid, b: &Solid) -> f64 {
        match (a, b) {
            (Solid::Prism { prism: pa, .. }, Solid::Prism { prism: pb, .. }) => {
                prism_prism_gap_lb(pa, pb)
            }
            (Solid::Prism { prism, .. }, Solid::Tube { segs, .. })
            | (Solid::Tube { segs, .. }, Solid::Prism { prism, .. }) => segs
                .iter()
                .map(|s| prism_tube_gap_lb(prism, s))
                .fold(f64::INFINITY, f64::min),
            (Solid::Tube { segs: sa, .. }, Solid::Tube { segs: sb, .. }) => sa
                .iter()
                .flat_map(|x| sb.iter().map(move |y| (x, y)))
                .map(|(x, y)| tube_tube_gap(x, y))
                .fold(f64::INFINITY, f64::min),
        }
    }
}

impl NativeService for GeometryFigureAudit {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let solids = Self::parse_solids(args)?;
        let min_clearance = match args.get("min_clearance") {
            None | Some(JsonValue::Null) => None,
            Some(v) => {
                let c = num_f64(v)
                    .ok_or_else(|| "min_clearance 须为合法数值 — 自诊断指引: ≥0".to_string())?;
                if c < 0.0 {
                    return Err(format!("min_clearance={c} 须非负"));
                }
                Some(c)
            }
        };
        let require_support = matches!(args.get("require_support"), Some(JsonValue::Bool(true)));
        let n = solids.len();
        let mut violations: Vec<JsonValue> = Vec::new();
        let mut min_gap = f64::INFINITY;
        for i in 0..n {
            for j in (i + 1)..n {
                let gap = Self::solid_gap(&solids[i], &solids[j]);
                if gap < min_gap {
                    min_gap = gap;
                }
                let hit = match min_clearance {
                    None => gap <= EPS,
                    Some(c) => gap < c - EPS,
                };
                if hit {
                    violations.push(JsonValue::object_from_pairs(&[
                        ("kind", JsonValue::string("insufficient_gap")),
                        ("ids", JsonValue::Array(vec![
                            JsonValue::string(solids[i].id().to_string()),
                            JsonValue::string(solids[j].id().to_string()),
                        ])),
                        ("gap", crate::figure::fnum(gap)),
                    ]));
                }
            }
        }
        if require_support {
            for s in &solids {
                if let Solid::Tube { id, segs } = s {
                    for (end_name, pt) in [
                        ("start", segs[0].a),
                        ("end", segs[segs.len() - 1].b),
                    ] {
                        let ok = solids.iter().any(|other| match other {
                            Solid::Prism { prism, .. } => supported_by(pt, prism),
                            Solid::Tube { .. } => false,
                        });
                        if !ok {
                            violations.push(JsonValue::object_from_pairs(&[
                                ("kind", JsonValue::string("unsupported_end")),
                                ("id", JsonValue::string(id.clone())),
                                ("end", JsonValue::string(end_name)),
                            ]));
                        }
                    }
                }
            }
        }
        let mut out = envelope();
        out.push(("solids", JsonValue::Integer(n as i64)));
        out.push(("pairs_checked", JsonValue::Integer((n * (n - 1) / 2) as i64)));
        out.push(("violations", JsonValue::Array(violations.clone())));
        out.push(("all_clean", JsonValue::Bool(violations.is_empty())));
        out.push((
            "min_gap_lb",
            if min_gap.is_finite() {
                crate::figure::fnum(min_gap)
            } else {
                JsonValue::Null
            },
        ));
        out.push(("gap_semantics", JsonValue::string("lower_bound")));
        Ok(JsonValue::object_from_pairs(&out))
    }
}

pub fn mk_figure_audit() -> Arc<dyn NativeService> {
    Arc::new(GeometryFigureAudit)
}

// ============================================================================
// 等距图样三服务测试
// ============================================================================

#[cfg(test)]
mod figure_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn pt3a(x: i64, y: i64, z: i64) -> JsonValue {
        JsonValue::Array(vec![
            JsonValue::Integer(x),
            JsonValue::Integer(y),
            JsonValue::Integer(z),
        ])
    }

    #[test]
    fn iso_project_envelope_and_golden() {
        let args = JsonValue::object_from_pairs(&[(
            "points",
            JsonValue::Array(vec![pt3a(0, 0, 0), pt3a(10, 0, 0), pt3a(0, 0, 10)]),
        )]);
        let out = GeometryIsoProject.execute(&args).unwrap();
        assert_eq!(
            out.get("status").and_then(JsonValue::as_str),
            Some("determined")
        );
        assert_eq!(
            out.get("determinism").and_then(JsonValue::as_str),
            Some("corpus_locked")
        );
        let proj = out.get("projected").and_then(JsonValue::as_array).unwrap();
        assert_eq!(proj.len(), 3);
        // 第三点 z=10 抬升 → 屏幕位于原点上方（y 更小）
        let y0 = proj[0].as_array().unwrap()[1].as_str().unwrap();
        let y2 = proj[2].as_array().unwrap()[1].as_str().unwrap();
        assert!(y2.parse::<f64>().unwrap() < y0.parse::<f64>().unwrap());
    }

    #[test]
    fn iso_project_rejects_bad_angle_and_fit() {
        let args = JsonValue::object_from_pairs(&[
            ("points", JsonValue::Array(vec![pt3a(0, 0, 0)])),
            ("elevation_deg", JsonValue::Integer(95)),
        ]);
        assert!(GeometryIsoProject.execute(&args).is_err());
        let args = JsonValue::object_from_pairs(&[
            ("points", JsonValue::Array(vec![pt3a(0, 0, 0)])),
            (
                "fit",
                JsonValue::object_from_pairs(&[
                    ("width", JsonValue::Integer(100)),
                    ("height", JsonValue::Integer(0)),
                ]),
            ),
        ]);
        assert!(GeometryIsoProject.execute(&args).is_err());
    }

    #[test]
    fn tube_route_right_angle_with_tight_leg_violates() {
        // 直角弯 R=10：切线需求 10 > 段长 8 → bend_exceeds_leg
        // （R=5 时切线 5 ≤ 单腿 8，合法不违规——占用量=段两端切线之和）
        let args = JsonValue::object_from_pairs(&[
            (
                "waypoints",
                JsonValue::Array(vec![pt3a(0, 0, 0), pt3a(8, 0, 0), pt3a(8, 8, 0)]),
            ),
            ("min_bend_radius", JsonValue::Integer(10)),
        ]);
        let out = GeometryTubeRoute.execute(&args).unwrap();
        assert_eq!(
            out.get("all_clean").and_then(JsonValue::as_bool),
            Some(false)
        );
        let v = out.get("violations").and_then(JsonValue::as_array).unwrap();
        assert!(v.iter().any(|x| x.get("kind").and_then(JsonValue::as_str)
            == Some("bend_exceeds_leg")));
    }

    #[test]
    fn tube_route_uturn_unroutable() {
        let args = JsonValue::object_from_pairs(&[
            (
                "waypoints",
                JsonValue::Array(vec![pt3a(0, 0, 0), pt3a(8, 0, 0), pt3a(0, 0, 0)]),
            ),
            ("min_bend_radius", JsonValue::Integer(1)),
        ]);
        let out = GeometryTubeRoute.execute(&args).unwrap();
        let v = out.get("violations").and_then(JsonValue::as_array).unwrap();
        assert!(v.iter().any(|x| x.get("kind").and_then(JsonValue::as_str)
            == Some("bend_unroutable")));
    }

    #[test]
    fn tube_route_no_radius_no_violations() {
        let args = JsonValue::object_from_pairs(&[(
            "waypoints",
            JsonValue::Array(vec![pt3a(0, 0, 0), pt3a(8, 0, 0), pt3a(8, 8, 0)]),
        )]);
        let out = GeometryTubeRoute.execute(&args).unwrap();
        assert_eq!(
            out.get("all_clean").and_then(JsonValue::as_bool),
            Some(true)
        );
        assert_eq!(
            out.get("total_length").and_then(JsonValue::as_str),
            Some("16.0"),
            "总长黄金值锁定"
        );
    }

    fn prism(id: &str, base: i64, top: i64) -> JsonValue {
        JsonValue::object_from_pairs(&[
            ("id", JsonValue::string(id)),
            ("kind", JsonValue::string("prism")),
            (
                "points",
                JsonValue::Array(vec![
                    JsonValue::Array(vec![JsonValue::Integer(0), JsonValue::Integer(0)]),
                    JsonValue::Array(vec![JsonValue::Integer(10), JsonValue::Integer(0)]),
                    JsonValue::Array(vec![JsonValue::Integer(10), JsonValue::Integer(10)]),
                    JsonValue::Array(vec![JsonValue::Integer(0), JsonValue::Integer(10)]),
                ]),
            ),
            ("base", JsonValue::Integer(base)),
            ("top", JsonValue::Integer(top)),
        ])
    }

    fn tube(id: &str, x1: i64, z1: i64, x2: i64, z2: i64, r: i64) -> JsonValue {
        JsonValue::object_from_pairs(&[
            ("id", JsonValue::string(id)),
            ("kind", JsonValue::string("tube")),
            (
                "waypoints",
                JsonValue::Array(vec![pt3a(x1, 5, z1), pt3a(x2, 5, z2)]),
            ),
            ("radius", JsonValue::Integer(r)),
        ])
    }

    #[test]
    fn figure_audit_intersecting_tube_reports_and_clean_scene_passes() {
        // 管段水平穿棱柱 → 违规
        let args = JsonValue::object_from_pairs(&[(
            "solids",
            JsonValue::Array(vec![prism("base", 0, 5), tube("pipe", -5, 2, 15, 2, 1)]),
        )]);
        let out = GeometryFigureAudit.execute(&args).unwrap();
        assert_eq!(
            out.get("all_clean").and_then(JsonValue::as_bool),
            Some(false)
        );
        assert_eq!(
            out.get("gap_semantics").and_then(JsonValue::as_str),
            Some("lower_bound"),
            "下界语义声明锁定"
        );
        // 管段在棱柱上方 3、半径 1 → 间隙 2 ≥ 0，干净
        let args = JsonValue::object_from_pairs(&[(
            "solids",
            JsonValue::Array(vec![prism("base", 0, 5), tube("pipe", -5, 8, 15, 8, 1)]),
        )]);
        let out = GeometryFigureAudit.execute(&args).unwrap();
        assert_eq!(
            out.get("all_clean").and_then(JsonValue::as_bool),
            Some(true)
        );
        assert_eq!(
            out.get("min_gap_lb").and_then(JsonValue::as_str),
            Some("2.0"),
            "间隙下界黄金值锁定"
        );
    }

    #[test]
    fn figure_audit_support_check() {
        // 管端 (−5,·) 不在棱柱内 → unsupported_end；改起点入棱柱后干净
        let args = JsonValue::object_from_pairs(&[
            (
                "solids",
                JsonValue::Array(vec![prism("mount", 0, 5), tube("pipe", -5, 2, 15, 2, 1)]),
            ),
            ("require_support", JsonValue::Bool(true)),
        ]);
        let out = GeometryFigureAudit.execute(&args).unwrap();
        let v = out.get("violations").and_then(JsonValue::as_array).unwrap();
        assert!(v.iter().any(|x| x.get("kind").and_then(JsonValue::as_str)
            == Some("unsupported_end")));
    }

    #[test]
    fn figure_audit_double_run_bit_identical() {
        let args = JsonValue::object_from_pairs(&[(
            "solids",
            JsonValue::Array(vec![prism("a", 0, 5), tube("p", -5, 8, 15, 8, 1)]),
        )]);
        let r1 = GeometryFigureAudit.execute(&args).unwrap().to_string();
        let r2 = GeometryFigureAudit.execute(&args).unwrap().to_string();
        assert_eq!(r1, r2, "双跑输出不一致（位元级）");
    }
}

// ============================================================================
// 测试（S3-g1：信封保留键 + 双跑位元一致 + 逐现状固化）
// ============================================================================

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    /// bbox 零件构造（Integer 坐标，规避字符串解析面）
    fn bbox(id: &str, x: i64, y: i64, w: i64, h: i64, z: Option<i64>) -> JsonValue {
        let mut fields = vec![
            ("id", JsonValue::string(id)),
            (
                "shape",
                JsonValue::object_from_pairs(&[
                    ("type", JsonValue::string("bbox")),
                    ("x", JsonValue::Integer(x)),
                    ("y", JsonValue::Integer(y)),
                    ("w", JsonValue::Integer(w)),
                    ("h", JsonValue::Integer(h)),
                ]),
            ),
        ];
        if let Some(zv) = z {
            fields.push(("z", JsonValue::Integer(zv)));
        }
        JsonValue::object_from_pairs(&fields)
    }

    fn parts_arg(parts: Vec<JsonValue>) -> JsonValue {
        JsonValue::object_from_pairs(&[("parts", JsonValue::Array(parts))])
    }

    /// 触碰对：a=[0,0,10,5]，b=[10,0,10,5]（x=10 边贴合，gap=0）
    fn touching_args() -> JsonValue {
        parts_arg(vec![
            bbox("a", 0, 0, 10, 5, None),
            bbox("b", 10, 0, 10, 5, None),
        ])
    }

    /// 分离对：gap=10
    fn apart_args() -> JsonValue {
        parts_arg(vec![
            bbox("a", 0, 0, 10, 5, None),
            bbox("b", 20, 0, 10, 5, None),
        ])
    }

    // ---- 信封保留键（统一信封规范 v1）----

    #[test]
    fn envelope_reserved_keys_present_all_modes() {
        for (name, svc, args) in [
            (
                "geometry_collision_check",
                &GeometryCollisionCheck as &dyn NativeService,
                touching_args(),
            ),
            (
                "geometry_clearance_check",
                &GeometryClearanceCheck as &dyn NativeService,
                parts_arg(vec![
                    bbox("a", 0, 0, 10, 5, None),
                    bbox("b", 20, 0, 10, 5, None),
                ]),
            ),
            (
                "geometry_occlusion_check",
                &GeometryOcclusionCheck as &dyn NativeService,
                parts_arg(vec![
                    bbox("a", 0, 0, 10, 5, Some(1)),
                    bbox("b", 0, 0, 10, 5, Some(1)),
                ]),
            ),
        ] {
            let mut args = args;
            if name == "geometry_clearance_check" {
                args = JsonValue::object_from_pairs(&[
                    ("parts", args.get("parts").unwrap().clone()),
                    ("min_clearance", JsonValue::Integer(5)),
                ]);
            }
            let out = svc.execute(&args).unwrap();
            assert_eq!(
                out.get("status").and_then(JsonValue::as_str),
                Some("determined"),
                "{name}: status 保留键缺失或值错"
            );
            assert_eq!(
                out.get("determinism").and_then(JsonValue::as_str),
                Some("corpus_locked"),
                "{name}: determinism 保留键缺失或值错"
            );
        }
    }

    /// 加性义务：既有字段一个不少、一个不改名；保留键恰好 2 个新增
    #[test]
    fn envelope_is_strictly_additive() {
        let out = GeometryCollisionCheck.execute(&touching_args()).unwrap();
        let obj = out.as_object().unwrap();
        for legacy in [
            "parts",
            "checked_pairs",
            "violations",
            "all_clean",
            "min_gap",
        ] {
            assert!(obj.get(legacy).is_some(), "既有字段 {legacy} 丢失");
        }
        let mut keys: Vec<&String> = obj.keys().collect();
        keys.sort();
        let expected: Vec<&str> = vec![
            "all_clean",
            "checked_pairs",
            "determinism",
            "min_gap",
            "parts",
            "status",
            "violations",
        ];
        let actual: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
        assert_eq!(
            actual, expected,
            "信封键集必须 = 既有字段 ∪ status/determinism"
        );
    }

    /// 双跑位元一致（corpus_locked 验收义务的最小可执行形态）
    #[test]
    fn double_run_bit_identical_all_modes() {
        let cases: Vec<(&dyn NativeService, JsonValue)> = vec![
            (&GeometryCollisionCheck, touching_args()),
            (&GeometryCollisionCheck, apart_args()),
            (
                &GeometryClearanceCheck,
                JsonValue::object_from_pairs(&[
                    ("parts", apart_args().get("parts").unwrap().clone()),
                    ("min_clearance", JsonValue::Integer(5)),
                ]),
            ),
            (
                &GeometryOcclusionCheck,
                parts_arg(vec![
                    bbox("a", 0, 0, 10, 5, Some(2)),
                    bbox("b", 0, 0, 10, 5, Some(1)),
                ]),
            ),
        ];
        for (svc, args) in cases {
            // Display = TCB 规范 JSON 渲染（确定性键序），逐字节比对
            let r1 = svc.execute(&args).unwrap().to_string();
            let r2 = svc.execute(&args).unwrap().to_string();
            assert_eq!(r1, r2, "双跑输出不一致（位元级）");
        }
    }

    /// undetermined 契约声明（S2 登记实测修正）：本族无 undetermined 路径，
    /// status 恒 determined——对触碰边界 case 也成立（容差方向声明即契约）
    #[test]
    fn status_always_determined_including_boundary_cases() {
        for args in [touching_args(), apart_args()] {
            let out = GeometryCollisionCheck.execute(&args).unwrap();
            assert_eq!(
                out.get("status").and_then(JsonValue::as_str),
                Some("determined")
            );
        }
    }

    // ---- 逐现状固化（行为锁定，禁止后续批顺手改语义）----

    #[test]
    fn collision_touch_counts_and_apart_clean() {
        let out = GeometryCollisionCheck.execute(&touching_args()).unwrap();
        assert_eq!(
            out.get("all_clean").and_then(JsonValue::as_bool),
            Some(false)
        );
        let v = out.get("violations").and_then(JsonValue::as_array).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].get("a").and_then(JsonValue::as_str), Some("a"));
        assert_eq!(v[0].get("b").and_then(JsonValue::as_str), Some("b"));
        assert_eq!(
            v[0].get("gap").and_then(JsonValue::as_str),
            Some("0.0"),
            "触碰 gap 序列化形态锁定（float_str 最短往返）"
        );

        let out = GeometryCollisionCheck.execute(&apart_args()).unwrap();
        assert_eq!(
            out.get("all_clean").and_then(JsonValue::as_bool),
            Some(true)
        );
        assert_eq!(
            out.get("min_gap").and_then(JsonValue::as_str),
            Some("10.0"),
            "分离 min_gap 形态锁定"
        );
    }

    #[test]
    fn clearance_requires_min_clearance_and_rejects_negative() {
        let args = parts_arg(vec![
            bbox("a", 0, 0, 10, 5, None),
            bbox("b", 20, 0, 10, 5, None),
        ]);
        let err = GeometryClearanceCheck.execute(&args).unwrap_err();
        assert!(err.contains("min_clearance"), "缺参错误信息锁定: {err}");

        let args = JsonValue::object_from_pairs(&[
            ("parts", args.get("parts").unwrap().clone()),
            ("min_clearance", JsonValue::Integer(-1)),
        ]);
        let err = GeometryClearanceCheck.execute(&args).unwrap_err();
        assert!(err.contains("非负"), "负值拒绝信息锁定: {err}");
    }

    #[test]
    fn occlusion_requires_all_z_and_detects_wrong_order() {
        // 缺 z
        let err = GeometryOcclusionCheck
            .execute(&touching_args())
            .unwrap_err();
        assert!(err.contains("z"), "缺 z 错误信息锁定: {err}");

        // 后绘者 z 更低 → 违规，含绘制序下标
        let out = GeometryOcclusionCheck
            .execute(&parts_arg(vec![
                bbox("base", 0, 0, 10, 5, Some(2)),
                bbox("top", 0, 0, 10, 5, Some(1)),
            ]))
            .unwrap();
        let v = out.get("violations").and_then(JsonValue::as_array).unwrap();
        assert_eq!(v.len(), 1, "后绘 z 低必须判违规");
        assert_eq!(
            v[0].get("order_earlier").and_then(JsonValue::as_array),
            None
        );
        assert!(v[0].get("order_earlier").is_some() && v[0].get("order_later").is_some());

        // 同层不算违规
        let out = GeometryOcclusionCheck
            .execute(&parts_arg(vec![
                bbox("a", 0, 0, 10, 5, Some(1)),
                bbox("b", 0, 0, 10, 5, Some(1)),
            ]))
            .unwrap();
        assert_eq!(
            out.get("all_clean").and_then(JsonValue::as_bool),
            Some(true)
        );
    }
}

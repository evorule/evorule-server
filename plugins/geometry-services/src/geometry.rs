//! 2D 几何核心谓词（f64 精度边界见 lib.rs 诚实声明）
//!
//! 形态：`bbox`（轴对齐矩形，解析期展开为多边形）/ `circle`（圆）/
//! `polygon`（任意简单多边形，顶点按边界序）。
//!
//! 判定语义（容差 EPS = 1e-9，宽容方向，声明即契约）：
//! - 相交 = 几何距离 ≤ EPS（边界相触算相交，多报不漏报）；
//! - 间隙 = 几何距离（相交/相触 = 0）；通过 = gap ≥ 限值 − EPS；
//! - 遮挡 = 重叠对中后绘制方 z 层低于先绘制方即违规（画家算法）。

/// 容差（绝对值；坐标量级为像素级 SVG 场景，绝对容差语义简单可声明）
pub const EPS: f64 = 1e-9;

/// 平面点
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pt {
    pub x: f64,
    pub y: f64,
}

/// 几何形状（bbox 已在解析期展开为多边形）
#[derive(Debug, Clone)]
pub enum Shape {
    Poly(Vec<Pt>),
    Circle { c: Pt, r: f64 },
}

// ============================================================================
// 解析
// ============================================================================

/// 数值解析：Integer 直转 / String 解析（拒绝 NaN/Inf 与杂质），对齐
/// indicator-services 的 parse_f64 边界纪律。
pub fn num_f64(v: &JsonValue) -> Option<f64> {
    match v {
        JsonValue::Integer(i) => Some(*i as f64),
        JsonValue::String(s) => s.trim().parse::<f64>().ok().filter(|f| f.is_finite()),
        _ => None,
    }
}

use evorule_tcb::JsonValue;

/// 解析形状对象：`{"type":"bbox","x","y","w","h"}` /
/// `{"type":"circle","cx","cy","r"}` / `{"type":"polygon","points":[[x,y],...]}`
pub fn parse_shape(v: &JsonValue) -> Result<Shape, String> {
    let obj = v
        .as_object()
        .ok_or_else(|| "shape 须为对象 — 自诊断指引: 形如 {\"type\":\"bbox\",\"x\":0,\"y\":0,\"w\":10,\"h\":5} / {\"type\":\"circle\",\"cx\":0,\"cy\":0,\"r\":3} / {\"type\":\"polygon\",\"points\":[[0,0],[4,0],[2,3]]}".to_string())?;
    let ty = obj.get("type").and_then(JsonValue::as_str).ok_or_else(|| {
        "shape 缺 type 字段 — 自诊断指引: type ∈ bbox | circle | polygon".to_string()
    })?;
    let num = |key: &str| -> Result<f64, String> {
        obj.get(key).and_then(num_f64).ok_or_else(|| {
            format!(
                "shape 缺合法数值字段 {key} — 自诊断指引: 数值为整数或十进制字符串，拒绝 NaN/Inf"
            )
        })
    };
    match ty {
        "bbox" => {
            let (x, y, w, h) = (num("x")?, num("y")?, num("w")?, num("h")?);
            if w <= 0.0 || h <= 0.0 {
                return Err(format!("bbox 宽高须为正 — 自诊断指引: w={w}, h={h}"));
            }
            Ok(Shape::Poly(vec![
                Pt { x, y },
                Pt { x: x + w, y },
                Pt { x: x + w, y: y + h },
                Pt { x, y: y + h },
            ]))
        }
        "circle" => {
            let r = num("r")?;
            if r < 0.0 {
                return Err(format!("circle 半径须非负 — 自诊断指引: r={r}"));
            }
            Ok(Shape::Circle {
                c: Pt {
                    x: num("cx")?,
                    y: num("cy")?,
                },
                r,
            })
        }
        "polygon" => {
            let pts_v = obj
                .get("points")
                .and_then(JsonValue::as_array)
                .ok_or_else(|| {
                    "polygon 缺 points 数组 — 自诊断指引: 形如 [[0,0],[4,0],[2,3]]（≥3 点）"
                        .to_string()
                })?;
            if pts_v.len() < 3 {
                return Err(format!(
                    "polygon 顶点数须 ≥3 — 自诊断指引: 实得 {} 点",
                    pts_v.len()
                ));
            }
            let mut pts = Vec::with_capacity(pts_v.len());
            for (i, p) in pts_v.iter().enumerate() {
                let pair = p.as_array().ok_or_else(|| {
                    format!("polygon 第 {i} 顶点须为 [x,y] 数对 — 自诊断指引: 拒绝嵌套对象")
                })?;
                if pair.len() != 2 {
                    return Err(format!(
                        "polygon 第 {i} 顶点须恰 2 元素 — 自诊断指引: 实得 {} 元素",
                        pair.len()
                    ));
                }
                let (x, y) = (
                    num_f64(&pair[0]).ok_or_else(|| format!("polygon 第 {i} 顶点 x 非合法数值"))?,
                    num_f64(&pair[1]).ok_or_else(|| format!("polygon 第 {i} 顶点 y 非合法数值"))?,
                );
                pts.push(Pt { x, y });
            }
            Ok(Shape::Poly(pts))
        }
        other => Err(format!(
            "shape type 未知: {other} — 自诊断指引: type ∈ bbox | circle | polygon"
        )),
    }
}

// ============================================================================
// 基础谓词（f64，固定运算序）
// ============================================================================

fn cross(o: Pt, a: Pt, b: Pt) -> f64 {
    (a.x - o.x) * (b.y - o.y) - (a.y - o.y) * (b.x - o.x)
}

fn on_segment_collinear(a: Pt, p: Pt, b: Pt) -> bool {
    p.x >= a.x.min(b.x) - EPS
        && p.x <= a.x.max(b.x) + EPS
        && p.y >= a.y.min(b.y) - EPS
        && p.y <= a.y.max(b.y) + EPS
}

/// 线段相交（含端点相触 / 共线重叠）
pub fn segs_intersect(p1: Pt, p2: Pt, q1: Pt, q2: Pt) -> bool {
    let d1 = cross(q1, q2, p1);
    let d2 = cross(q1, q2, p2);
    let d3 = cross(p1, p2, q1);
    let d4 = cross(p1, p2, q2);
    if ((d1 > EPS && d2 < -EPS) || (d1 < -EPS && d2 > EPS))
        && ((d3 > EPS && d4 < -EPS) || (d3 < -EPS && d4 > EPS))
    {
        return true;
    }
    // 共线 / 端点落在对方线段上（触即算）
    (d1.abs() <= EPS && on_segment_collinear(q1, p1, q2))
        || (d2.abs() <= EPS && on_segment_collinear(q1, p2, q2))
        || (d3.abs() <= EPS && on_segment_collinear(p1, q1, p2))
        || (d4.abs() <= EPS && on_segment_collinear(p1, q2, p2))
}

/// 点在多边形内（边界计入：先边界检测，再射线奇偶）
pub fn point_in_poly(p: Pt, poly: &[Pt]) -> bool {
    let n = poly.len();
    for i in 0..n {
        let (a, b) = (poly[i], poly[(i + 1) % n]);
        if segs_intersect(a, b, p, p) {
            return true;
        }
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (pi, pj) = (poly[i], poly[j]);
        if (pi.y > p.y) != (pj.y > p.y) {
            let x_int = (pj.x - pi.x) * (p.y - pi.y) / (pj.y - pi.y) + pi.x;
            if p.x < x_int {
                inside = !inside;
            }
        }
        j = i;
    }
    inside
}

/// 多边形-多边形相交（任一边对相交，或任一顶点落入对方内部）
pub fn polys_intersect(a: &[Pt], b: &[Pt]) -> bool {
    for i in 0..a.len() {
        let (a1, a2) = (a[i], a[(i + 1) % a.len()]);
        for j in 0..b.len() {
            if segs_intersect(a1, a2, b[j], b[(j + 1) % b.len()]) {
                return true;
            }
        }
    }
    a.iter().any(|p| point_in_poly(*p, b)) || b.iter().any(|p| point_in_poly(*p, a))
}

/// 点到线段距离
pub fn pt_seg_dist(p: Pt, a: Pt, b: Pt) -> f64 {
    let (vx, vy) = (b.x - a.x, b.y - a.y);
    let (wx, wy) = (p.x - a.x, p.y - a.y);
    let len2 = vx * vx + vy * vy;
    if len2 <= EPS * EPS {
        return ((p.x - a.x).powi(2) + (p.y - a.y).powi(2)).sqrt();
    }
    let t = ((wx * vx + wy * vy) / len2).clamp(0.0, 1.0);
    let (dx, dy) = (wx - t * vx, wy - t * vy);
    (dx * dx + dy * dy).sqrt()
}

/// 线段-线段距离（相交 = 0；不相交取 4 端点-对边距离最小值，标准充分）
pub fn seg_seg_dist(p1: Pt, p2: Pt, q1: Pt, q2: Pt) -> f64 {
    if segs_intersect(p1, p2, q1, q2) {
        return 0.0;
    }
    pt_seg_dist(p1, q1, q2)
        .min(pt_seg_dist(p2, q1, q2))
        .min(pt_seg_dist(q1, p1, p2))
        .min(pt_seg_dist(q2, p1, p2))
}

/// 圆-多边形距离（圆心在内或触及 = 0；否则圆心到边距离减半径，负值截 0）
pub fn circle_poly_dist(c: Pt, r: f64, poly: &[Pt]) -> f64 {
    if point_in_poly(c, poly) {
        return 0.0;
    }
    let n = poly.len();
    let mut best = f64::INFINITY;
    for i in 0..n {
        best = best.min(pt_seg_dist(c, poly[i], poly[(i + 1) % n]));
    }
    (best - r).max(0.0)
}

/// 两形状几何距离（相交/相触 = 0；判定语义见模块头）
pub fn shapes_dist(a: &Shape, b: &Shape) -> f64 {
    match (a, b) {
        (Shape::Poly(pa), Shape::Poly(pb)) => {
            if polys_intersect(pa, pb) {
                0.0
            } else {
                let mut best = f64::INFINITY;
                for i in 0..pa.len() {
                    for j in 0..pb.len() {
                        best = best.min(seg_seg_dist(
                            pa[i],
                            pa[(i + 1) % pa.len()],
                            pb[j],
                            pb[(j + 1) % pb.len()],
                        ));
                    }
                }
                best
            }
        }
        (Shape::Circle { c, r }, Shape::Poly(pb)) | (Shape::Poly(pb), Shape::Circle { c, r }) => {
            circle_poly_dist(*c, *r, pb)
        }
        (Shape::Circle { c: c1, r: r1 }, Shape::Circle { c: c2, r: r2 }) => {
            ((c1.x - c2.x).hypot(c1.y - c2.y) - r1 - r2).max(0.0)
        }
    }
}

/// 相交判定（距离 ≤ EPS，触即算）
pub fn shapes_intersect(a: &Shape, b: &Shape) -> bool {
    shapes_dist(a, b) <= EPS
}

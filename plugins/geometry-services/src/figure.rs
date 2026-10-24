//! 等距图样内核（3D 场景的确定性几何计算）
//!
//! 为"LLM 生成图样规格、确定性层编译与审计"的分工提供纯计算件：
//! 等距投影 / 管路转弯与弯径校核 / 棱柱-管路场景间隙。
//! 输入均为 JSON 数值（整数或十进制字符串），拒绝 NaN/Inf。
//!
//! # 等距投影契约（声明即契约）
//!
//! 相机由方位角 `azimuth_deg`（缺省 45，∈[0,360)）与仰角 `elevation_deg`
//! （缺省 30，∈(0,90)）定义，z 轴向上，屏幕 y 轴向下：
//!
//! ```text
//! u  = x·cos(az) − y·sin(az)
//! w  = x·sin(az) + y·cos(az)
//! sx = u
//! sy = w·sin(el) − z·cos(el)
//! ```
//!
//! 可选视口适配 `fit{width,height,pad?}`：对投影包围盒等比缩放并居中，
//! `scale = min(avail_x/dx, avail_y/dy)`（某轴跨度 ≤ EPS 时该轴不约束），
//! `offset = pad + (avail − span·scale)/2 − min·scale`；两轴均退化时
//! `scale=1` 且以视口中心定位。
//!
//! # 场景间隙契约（保守方向声明）
//!
//! 棱柱 = 多边形 × [base,top] 层叠体；管段 = 中心线段 ⊕ 球体(r)。
//! 全部成对间隙报告为**下界估计**（真实距离 ≥ 报告值）：
//! 碰撞语义下不漏报（触即算，可多报）；间隙语义下违规判定偏严。
//! 每对间隙 = max(水平分量, 垂直分量, 0) − 组合半径，逐对声明于输出。

use crate::geometry::{num_f64, point_in_poly, polys_intersect, seg_seg_dist, Pt, Shape, EPS};
use evorule_tcb::JsonValue;

/// 三维点
#[derive(Debug, Clone, Copy)]
pub struct Pt3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

/// 点数硬上限（投影服务）
pub const MAX_POINTS: usize = 65536;
/// 单管路转折点硬上限
pub const MAX_WAYPOINTS: usize = 1024;

/// 解析 `[x,y,z]` 三元组
pub fn parse_pt3(v: &JsonValue, ctx: &str) -> Result<Pt3, String> {
    let arr = v
        .as_array()
        .ok_or_else(|| format!("{ctx} 须为 [x,y,z] 数组 — 自诊断指引: 拒绝对象与嵌套结构"))?;
    if arr.len() != 3 {
        return Err(format!(
            "{ctx} 须恰 3 元素 — 自诊断指引: 实得 {} 元素",
            arr.len()
        ));
    }
    let n = |i: usize| -> Result<f64, String> {
        num_f64(&arr[i]).ok_or_else(|| format!("{ctx} 第 {i} 分量非合法数值（拒绝 NaN/Inf）"))
    };
    Ok(Pt3 {
        x: n(0)?,
        y: n(1)?,
        z: n(2)?,
    })
}

// ============================================================================
// 等距投影
// ============================================================================

/// 角度 → 弧度（f64，固定运算序）
fn deg2rad(d: f64) -> f64 {
    d * std::f64::consts::PI / 180.0
}

/// 基础投影（无视口适配）：契约见模块头
pub fn iso_project_pt(p: Pt3, az_deg: f64, el_deg: f64) -> Pt {
    let (az, el) = (deg2rad(az_deg), deg2rad(el_deg));
    let (ca, sa) = (az.cos(), az.sin());
    let (ce, se) = (el.cos(), el.sin());
    let u = p.x * ca - p.y * sa;
    let w = p.x * sa + p.y * ca;
    Pt {
        x: u,
        y: w * se - p.z * ce,
    }
}

/// 视口适配参数：等比缩放 + 平移，使投影包围盒带 pad 居中
pub struct Fit {
    pub width: f64,
    pub height: f64,
    pub pad: f64,
}

/// 求适配变换 `(scale, offset_x, offset_y)`；契约见模块头。
/// 退化（某轴跨度 ≤ EPS）：该轴不约束缩放；两轴均退化：scale=1、视口中心定位。
pub fn fit_transform(pts: &[Pt], fit: &Fit) -> (f64, f64, f64) {
    let (mut minx, mut maxx) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut miny, mut maxy) = (f64::INFINITY, f64::NEG_INFINITY);
    for p in pts {
        minx = minx.min(p.x);
        maxx = maxx.max(p.x);
        miny = miny.min(p.y);
        maxy = maxy.max(p.y);
    }
    let (dx, dy) = (maxx - minx, maxy - miny);
    let avail_x = (fit.width - 2.0 * fit.pad).max(0.0);
    let avail_y = (fit.height - 2.0 * fit.pad).max(0.0);
    let sx = if dx > EPS {
        avail_x / dx
    } else {
        f64::INFINITY
    };
    let sy = if dy > EPS {
        avail_y / dy
    } else {
        f64::INFINITY
    };
    let scale = if sx.is_finite() || sy.is_finite() {
        sx.min(sy)
    } else {
        1.0
    };
    let off = |min_v: f64, span: f64, avail: f64, center: f64| -> f64 {
        if span > EPS {
            fit.pad + (avail - span * scale) / 2.0 - min_v * scale
        } else {
            center - min_v * scale
        }
    };
    let offset_x = off(minx, dx, avail_x, fit.pad + avail_x / 2.0);
    let offset_y = off(miny, dy, avail_y, fit.pad + avail_y / 2.0);
    (scale, offset_x, offset_y)
}

// ============================================================================
// 三维线段与场景谓词
// ============================================================================

pub fn sub3(a: Pt3, b: Pt3) -> Pt3 {
    Pt3 {
        x: a.x - b.x,
        y: a.y - b.y,
        z: a.z - b.z,
    }
}

fn dot3(a: Pt3, b: Pt3) -> f64 {
    a.x * b.x + a.y * b.y + a.z * b.z
}

pub fn len3(a: Pt3) -> f64 {
    dot3(a, a).sqrt()
}

/// 三维线段-线段距离（标准钳位法；退化零长段由解析期拒绝，
/// 案例判定顺序固定保证位元一致）
pub fn seg_seg_dist_3d(p1: Pt3, q1: Pt3, p2: Pt3, q2: Pt3) -> f64 {
    let d1 = sub3(q1, p1);
    let d2 = sub3(q2, p2);
    let r = sub3(p1, p2);
    let a = dot3(d1, d1);
    let e = dot3(d2, d2);
    let f = dot3(d2, r);
    if a <= EPS * EPS && e <= EPS * EPS {
        return len3(r);
    }
    let s: f64;
    let t: f64;
    if a <= EPS * EPS {
        s = 0.0;
        t = (f / e).clamp(0.0, 1.0);
    } else {
        let c = dot3(d1, r);
        if e <= EPS * EPS {
            t = 0.0;
            s = (-c / a).clamp(0.0, 1.0);
        } else {
            let b = dot3(d1, d2);
            let denom = a * e - b * b;
            let s0 = if denom > EPS * EPS {
                ((b * f - c * e) / denom).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let mut t0 = (b * s0 + f) / e;
            if t0 < 0.0 {
                t0 = 0.0;
                s = (-c / a).clamp(0.0, 1.0);
            } else if t0 > 1.0 {
                t0 = 1.0;
                s = ((b - c) / a).clamp(0.0, 1.0);
            } else {
                s = s0;
            }
            t = t0;
        }
    }
    let c1 = Pt3 {
        x: p1.x + d1.x * s,
        y: p1.y + d1.y * s,
        z: p1.z + d1.z * s,
    };
    let c2 = Pt3 {
        x: p2.x + d2.x * t,
        y: p2.y + d2.y * t,
        z: p2.z + d2.z * t,
    };
    len3(sub3(c1, c2))
}

/// 二维线段到多边形的距离（相交/触及/端点在内 = 0；
/// 逐边取线段-线段距离，平行内侧逼近不漏）
pub fn seg_poly_dist_2d(a: Pt, b: Pt, poly: &[Pt]) -> f64 {
    let n = poly.len();
    let mut best = f64::INFINITY;
    for i in 0..n {
        best = best.min(seg_seg_dist(a, b, poly[i], poly[(i + 1) % n]));
    }
    if best <= EPS {
        return 0.0;
    }
    if point_in_poly(a, poly) || point_in_poly(b, poly) {
        return 0.0;
    }
    best
}

/// 棱柱：多边形 × [base, top]
#[derive(Debug, Clone)]
pub struct Prism {
    pub poly: Vec<Pt>,
    pub base: f64,
    pub top: f64,
}

/// 层叠区间间隙（相交/相触 = 0；否则 Gap = 间隔高度）
pub fn slab_gap(b1: f64, t1: f64, b2: f64, t2: f64) -> f64 {
    (b2 - t1).max(b1 - t2).max(0.0)
}

/// 棱柱-棱柱间隙下界 = max(水平多边形距离, 垂直层间隙)，保守方向（≥真实值以下界报告）
pub fn prism_prism_gap_lb(a: &Prism, b: &Prism) -> f64 {
    let shape_a = Shape::Poly(a.poly.clone());
    let shape_b = Shape::Poly(b.poly.clone());
    let d_xy = if polys_intersect(&a.poly, &b.poly) {
        0.0
    } else {
        crate::geometry::shapes_dist(&shape_a, &shape_b)
    };
    d_xy.max(slab_gap(a.base, a.top, b.base, b.top))
}

/// 管段（中心线段 + 半径）
#[derive(Debug, Clone, Copy)]
pub struct TubeSeg {
    pub a: Pt3,
    pub b: Pt3,
    pub r: f64,
}

impl TubeSeg {
    /// 投影到 xy 平面的二维线段
    pub fn xy(&self) -> (Pt, Pt) {
        (
            Pt {
                x: self.a.x,
                y: self.a.y,
            },
            Pt {
                x: self.b.x,
                y: self.b.y,
            },
        )
    }

    /// 线段 z 范围（a.z ≤ b.z 无保证，返回有序对）
    pub fn z_range(&self) -> (f64, f64) {
        (self.a.z.min(self.b.z), self.a.z.max(self.b.z))
    }
}

/// 棱柱-管段间隙下界（保守）：max(水平多边形距离 − 半径, 垂直层间隙 − 半径, 0)
pub fn prism_tube_gap_lb(prism: &Prism, seg: &TubeSeg) -> f64 {
    let (pa, pb) = seg.xy();
    let d_xy = seg_poly_dist_2d(pa, pb, &prism.poly);
    let (zb, zt) = seg.z_range();
    let z_gap = slab_gap(zb, zt, prism.base, prism.top);
    (d_xy - seg.r).max(z_gap - seg.r).max(0.0)
}

/// 管段-管段间隙 = max(0, 中心线距离 − 半径和)
pub fn tube_tube_gap(a: &TubeSeg, b: &TubeSeg) -> f64 {
    (seg_seg_dist_3d(a.a, a.b, b.a, b.b) - a.r - b.r).max(0.0)
}

/// 端点落位判定：点是否被某棱柱承托（xy 在多边形内且 z 在层内含容差）
pub fn supported_by(p: Pt3, prism: &Prism) -> bool {
    let xy = Pt { x: p.x, y: p.y };
    point_in_poly(xy, &prism.poly) && p.z >= prism.base - EPS && p.z <= prism.top + EPS
}

// ============================================================================
// 管路转弯与弯径
// ============================================================================

/// 转角信息：内点转角 θ（0=直行，π=掉头，度）与给定弯径下的切线长度
/// `t = R / tan((π−θ)/2)`（θ=0 时无弯不需切线，t=0）
pub struct BendInfo {
    pub turn_deg: f64,
    pub tangent_len: f64,
}

/// 单转角计算：入段方向 d_in、出段方向 d_out（均已被调用方归一化）
pub fn bend_info(d_in: Pt3, d_out: Pt3, min_bend_radius: Option<f64>) -> BendInfo {
    let cos_turn = dot3(d_in, d_out).clamp(-1.0, 1.0);
    let theta = cos_turn.acos();
    let tangent = match min_bend_radius {
        None => 0.0,
        Some(r) => {
            if theta <= EPS {
                0.0
            } else {
                r / ((std::f64::consts::PI - theta) / 2.0).tan()
            }
        }
    };
    BendInfo {
        turn_deg: theta * 180.0 / std::f64::consts::PI,
        tangent_len: tangent,
    }
}

/// 数值输出辅助（与 services.rs 的 float_str 同形，避免环依赖放此处）
pub fn fnum(f: f64) -> JsonValue {
    JsonValue::string(format!("{f:?}"))
}

// ============================================================================
// 测试（黄金向量 + 边界）
// ============================================================================

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn p3(x: f64, y: f64, z: f64) -> Pt3 {
        Pt3 { x, y, z }
    }

    #[test]
    fn iso_project_axis_golden() {
        // 原点投影 = (0,0)
        let o = iso_project_pt(p3(0.0, 0.0, 0.0), 45.0, 30.0);
        assert!(o.x.abs() < EPS && o.y.abs() < EPS);
        // z 抬升 → 屏幕上移（y 减小）
        let up = iso_project_pt(p3(0.0, 0.0, 10.0), 45.0, 30.0);
        assert!(up.y < o.y - 1.0, "z 抬升必须减小 sy");
        // x 正向 → 屏幕右下（az=45: u>0, w>0 ⇒ sy 增大）
        let px = iso_project_pt(p3(10.0, 0.0, 0.0), 45.0, 30.0);
        assert!(px.x > 0.0 && px.y > o.y);
        // 方位 0°：u=x, w=y
        let a0 = iso_project_pt(p3(3.0, 7.0, 0.0), 0.0, 90.0);
        assert!((a0.x - 3.0).abs() < EPS);
        assert!(
            (a0.y - 7.0).abs() < EPS,
            "el=90 俯视时 sy = w（+y 落向屏幕下方）: 实得 {}",
            a0.y
        );
    }

    #[test]
    fn fit_transform_golden() {
        // 两点水平相距 100，视口 200×100 pad 10 → scale = 180/100 = 1.8
        let pts = vec![Pt { x: 0.0, y: 0.0 }, Pt { x: 100.0, y: 0.0 }];
        let (s, ox, oy) = fit_transform(
            &pts,
            &Fit {
                width: 200.0,
                height: 100.0,
                pad: 10.0,
            },
        );
        assert!((s - 1.8).abs() < 1e-12);
        assert!((ox - 10.0).abs() < 1e-9, "水平居中: offset={ox}");
        assert!(
            (oy - 50.0).abs() < 1e-9,
            "竖直退化轴居视口中心: offset={oy}"
        );
    }

    #[test]
    fn seg_seg_3d_known_distances() {
        // 平行线距离 5
        let d = seg_seg_dist_3d(
            p3(0.0, 0.0, 0.0),
            p3(10.0, 0.0, 0.0),
            p3(0.0, 5.0, 0.0),
            p3(10.0, 5.0, 0.0),
        );
        assert!((d - 5.0).abs() < EPS);
        // 交叉但错层：xy 十字交叉，z 相距 4
        let d = seg_seg_dist_3d(
            p3(-5.0, 0.0, 0.0),
            p3(5.0, 0.0, 0.0),
            p3(0.0, -5.0, 4.0),
            p3(0.0, 5.0, 4.0),
        );
        assert!((d - 4.0).abs() < EPS);
        // 共点 = 0
        let d = seg_seg_dist_3d(
            p3(0.0, 0.0, 0.0),
            p3(1.0, 1.0, 1.0),
            p3(1.0, 1.0, 1.0),
            p3(2.0, 2.0, 2.0),
        );
        assert!(d < EPS);
    }

    #[test]
    fn prism_tube_gap_bound() {
        let prism = Prism {
            poly: vec![
                Pt { x: 0.0, y: 0.0 },
                Pt { x: 10.0, y: 0.0 },
                Pt { x: 10.0, y: 10.0 },
                Pt { x: 0.0, y: 10.0 },
            ],
            base: 0.0,
            top: 5.0,
        };
        // 管段水平穿过棱柱内部 → 间隙下界 0
        let seg = TubeSeg {
            a: p3(-5.0, 5.0, 2.5),
            b: p3(15.0, 5.0, 2.5),
            r: 0.5,
        };
        assert!(prism_tube_gap_lb(&prism, &seg) <= EPS);
        // 管段在棱柱上方 3、半径 0.5 → 垂直分量 3−0.5=2.5
        let seg = TubeSeg {
            a: p3(-5.0, 5.0, 8.0),
            b: p3(15.0, 5.0, 8.0),
            r: 0.5,
        };
        let g = prism_tube_gap_lb(&prism, &seg);
        assert!((g - 2.5).abs() < 1e-9, "实得 {g}");
        // 管段水平外移 2、半径 0.5 → 水平分量 2−0.5=1.5
        let seg = TubeSeg {
            a: p3(-5.0, 12.0, 2.5),
            b: p3(15.0, 12.0, 2.5),
            r: 0.5,
        };
        let g = prism_tube_gap_lb(&prism, &seg);
        assert!((g - 1.5).abs() < 1e-9, "实得 {g}");
    }

    #[test]
    fn bend_geometry_classic_cases() {
        // 直行：无弯
        let straight = bend_info(p3(1.0, 0.0, 0.0), p3(1.0, 0.0, 0.0), Some(2.0));
        assert!(straight.turn_deg.abs() < 1e-9 && straight.tangent_len.abs() < 1e-12);
        // 直角弯 R=2：切线长 = R/tan(45°) = 2
        let right = bend_info(p3(1.0, 0.0, 0.0), p3(0.0, 1.0, 0.0), Some(2.0));
        assert!((right.turn_deg - 90.0).abs() < 1e-9);
        assert!(
            (right.tangent_len - 2.0).abs() < 1e-9,
            "实得 {}",
            right.tangent_len
        );
        // 掉头（180°）：切线需求 → 巨大
        let uturn = bend_info(p3(1.0, 0.0, 0.0), p3(-1.0, 0.0, 0.0), Some(1.0));
        assert!((uturn.turn_deg - 180.0).abs() < 1e-9);
        assert!(
            uturn.tangent_len > 1e6,
            "掉头切线需求应趋无穷: {}",
            uturn.tangent_len
        );
    }

    #[test]
    fn double_run_bit_identical_kernel() {
        let p = iso_project_pt(p3(1.234, 2.345, 3.456), 45.0, 30.0);
        let q = iso_project_pt(p3(1.234, 2.345, 3.456), 45.0, 30.0);
        assert_eq!(format!("{:?}", (p.x, p.y)), format!("{:?}", (q.x, q.y)));
        let d1 = seg_seg_dist_3d(
            p3(0.1, 0.2, 0.3),
            p3(1.0, 2.0, 3.0),
            p3(0.0, 5.0, 0.0),
            p3(10.0, 5.0, 0.0),
        );
        let d2 = seg_seg_dist_3d(
            p3(0.1, 0.2, 0.3),
            p3(1.0, 2.0, 3.0),
            p3(0.0, 5.0, 0.0),
            p3(10.0, 5.0, 0.0),
        );
        assert_eq!(format!("{d1:?}"), format!("{d2:?}"));
    }
}

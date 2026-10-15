//! 几何三服务的参数解析与执行（collision / clearance / occlusion）
//!
//! 共用输入形态：`parts` 数组，元素 = `{"id","shape","z"?}`；
//! 绘制序 = 数组下标（先绘在下、后绘在上，画家算法）。
//! `z` 为可选声明层（同层不判遮挡序，由绘制序决定覆盖）。

use std::sync::Arc;

use evorule_reactor::IoResult;
use evorule_tcb::JsonValue;

use evorule_plugin_kit::NativeService;

use crate::geometry::{num_f64, parse_shape, shapes_dist, Shape, EPS};

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

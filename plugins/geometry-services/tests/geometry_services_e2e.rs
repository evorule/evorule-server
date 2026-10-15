//! geometry-services 端到端测试：经路由器全链路（params → 分发 → 执行）。
//!
//! 覆盖：三服务的判定命中/全净、fail-fast 面（缺参/非法形状/重复 id/空表）、
//! 数值语义（容差方向、浮点输出形态）。

// 测试代码豁免：unwrap/expect/panic 在测试中断言失败即测试失败，语义等价
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use evorule_geometry_services::GeometryServiceRouter;
use evorule_reactor::IoHandler;
use evorule_tcb::JsonValue;
use std::sync::Arc;

fn noop_fallback() -> Arc<dyn IoHandler> {
    struct Noop;
    #[async_trait::async_trait]
    impl IoHandler for Noop {
        async fn execute(&self, _params: &JsonValue) -> evorule_reactor::IoResult {
            Err("fallback".to_string())
        }
    }
    Arc::new(Noop)
}

fn svc_params(name: &str, args: JsonValue) -> JsonValue {
    JsonValue::object_from_pairs(&[("service_name", JsonValue::string(name)), ("args", args)])
}

/// 零件构造：bbox 快捷形态
fn bbox(id: &str, x: f64, y: f64, w: f64, h: f64) -> JsonValue {
    let shape = JsonValue::object_from_pairs(&[
        ("type", JsonValue::string("bbox")),
        ("x", JsonValue::Integer(x as i64)),
        ("y", JsonValue::Integer(y as i64)),
        ("w", JsonValue::Integer(w as i64)),
        ("h", JsonValue::Integer(h as i64)),
    ]);
    JsonValue::object_from_pairs(&[("id", JsonValue::string(id)), ("shape", shape)])
}

fn circle(id: &str, cx: f64, cy: f64, r: f64) -> JsonValue {
    let shape = JsonValue::object_from_pairs(&[
        ("type", JsonValue::string("circle")),
        ("cx", JsonValue::Integer(cx as i64)),
        ("cy", JsonValue::Integer(cy as i64)),
        ("r", JsonValue::Integer(r as i64)),
    ]);
    JsonValue::object_from_pairs(&[("id", JsonValue::string(id)), ("shape", shape)])
}

async fn run(name: &str, args: JsonValue) -> Result<JsonValue, String> {
    let router = GeometryServiceRouter::new(noop_fallback());
    router.execute(&svc_params(name, args)).await
}

// ===== collision =====

#[tokio::test]
async fn collision_detects_overlap() {
    let args = JsonValue::object_from_pairs(&[(
        "parts",
        JsonValue::Array(vec![
            bbox("a", 0.0, 0.0, 10.0, 10.0),
            bbox("b", 5.0, 5.0, 10.0, 10.0),
        ]),
    )]);
    let out = run("geometry_collision_check", args).await.unwrap();
    assert!(!out.get("all_clean").and_then(JsonValue::as_bool).unwrap());
    let v = out.get("violations").and_then(JsonValue::as_array).unwrap();
    assert_eq!(v.len(), 1, "恰一对重叠");
    assert_eq!(v[0].get("a").and_then(JsonValue::as_str).unwrap(), "a");
}

#[tokio::test]
async fn collision_clean_and_pair_count() {
    let args = JsonValue::object_from_pairs(&[(
        "parts",
        JsonValue::Array(vec![
            bbox("a", 0.0, 0.0, 4.0, 4.0),
            bbox("b", 10.0, 10.0, 4.0, 4.0),
            circle("c", 20.0, 20.0, 2.0),
        ]),
    )]);
    let out = run("geometry_collision_check", args).await.unwrap();
    assert!(out.get("all_clean").and_then(JsonValue::as_bool).unwrap());
    assert_eq!(
        out.get("checked_pairs")
            .and_then(JsonValue::as_i64)
            .unwrap(),
        3
    );
    // 全局最小 = b-c 对（b 右上角(14,14) 到圆心(20,20) 距离 hypot(6,6) 减半径 2）
    let min_gap: f64 = out
        .get("min_gap")
        .and_then(JsonValue::as_str)
        .unwrap()
        .parse()
        .unwrap();
    let expected = 6.0f64.hypot(6.0) - 2.0;
    assert!(
        (min_gap - expected).abs() < 1e-12,
        "min_gap 应为 {expected}，实得 {min_gap}"
    );
}

#[tokio::test]
async fn collision_touching_counts() {
    // 边界相触 = 相交（触即算，多报方向）
    let args = JsonValue::object_from_pairs(&[(
        "parts",
        JsonValue::Array(vec![
            bbox("a", 0.0, 0.0, 4.0, 4.0),
            bbox("b", 4.0, 0.0, 4.0, 4.0),
        ]),
    )]);
    let out = run("geometry_collision_check", args).await.unwrap();
    assert!(!out.get("all_clean").and_then(JsonValue::as_bool).unwrap());
}

// ===== clearance =====

#[tokio::test]
async fn clearance_violation_and_tolerance() {
    // a-b 间隙 2；限值 3 → 违规；a-c 间隙 6 → 达标
    let args = JsonValue::object_from_pairs(&[
        (
            "parts",
            JsonValue::Array(vec![
                bbox("a", 0.0, 0.0, 4.0, 4.0),
                bbox("b", 6.0, 0.0, 4.0, 4.0),
                bbox("c", 10.0, 10.0, 4.0, 4.0),
            ]),
        ),
        ("min_clearance", JsonValue::Integer(3)),
    ]);
    let out = run("geometry_clearance_check", args).await.unwrap();
    let v = out.get("violations").and_then(JsonValue::as_array).unwrap();
    assert_eq!(v.len(), 1, "恰一对违规");
    assert_eq!(v[0].get("a").and_then(JsonValue::as_str).unwrap(), "a");
    assert_eq!(v[0].get("b").and_then(JsonValue::as_str).unwrap(), "b");
    // 宽容方向：gap == limit 恰好达标（gap ≥ limit − EPS）
    let args_ok = JsonValue::object_from_pairs(&[
        (
            "parts",
            JsonValue::Array(vec![
                bbox("a", 0.0, 0.0, 4.0, 4.0),
                bbox("b", 7.0, 0.0, 4.0, 4.0),
            ]),
        ),
        ("min_clearance", JsonValue::Integer(3)),
    ]);
    let out = run("geometry_clearance_check", args_ok).await.unwrap();
    assert!(
        out.get("all_clean").and_then(JsonValue::as_bool).unwrap(),
        "恰达限值的对不判违规（宽容方向）"
    );
}

#[tokio::test]
async fn clearance_float_coords_via_string() {
    // 坐标允许十进制字符串承载小数（Integer 之外的输入通道）
    let shape = JsonValue::object_from_pairs(&[
        ("type", JsonValue::string("circle")),
        ("cx", JsonValue::string("0.0")),
        ("cy", JsonValue::string("0.0")),
        ("r", JsonValue::string("2.5")),
    ]);
    let args = JsonValue::object_from_pairs(&[
        (
            "parts",
            JsonValue::Array(vec![
                JsonValue::object_from_pairs(&[
                    ("id", JsonValue::string("disc")),
                    ("shape", shape),
                ]),
                circle("far", 100.0, 100.0, 1.0),
            ]),
        ),
        ("min_clearance", JsonValue::Integer(5)),
    ]);
    let out = run("geometry_clearance_check", args).await.unwrap();
    assert!(out.get("all_clean").and_then(JsonValue::as_bool).unwrap());
}

// ===== occlusion =====

#[tokio::test]
async fn occlusion_violation_when_later_lower_z() {
    // 绘制序: base 先, badge 后; badge z 更低却后绘 = 违规
    let mk = |id: &str, z: i64| {
        let shape = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("bbox")),
            ("x", JsonValue::Integer(0)),
            ("y", JsonValue::Integer(0)),
            ("w", JsonValue::Integer(6)),
            ("h", JsonValue::Integer(6)),
        ]);
        JsonValue::object_from_pairs(&[
            ("id", JsonValue::string(id)),
            ("shape", shape),
            ("z", JsonValue::Integer(z)),
        ])
    };
    let args = JsonValue::object_from_pairs(&[(
        "parts",
        JsonValue::Array(vec![mk("base", 2), mk("badge", 1)]),
    )]);
    let out = run("geometry_occlusion_check", args).await.unwrap();
    assert!(!out.get("all_clean").and_then(JsonValue::as_bool).unwrap());
    assert_eq!(
        out.get("overlapping_pairs")
            .and_then(JsonValue::as_i64)
            .unwrap(),
        1
    );
    // 同层重叠不判违规（绘制序决定覆盖，合法）
    let mk2 = |id: &str| {
        let shape = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("bbox")),
            ("x", JsonValue::Integer(0)),
            ("y", JsonValue::Integer(0)),
            ("w", JsonValue::Integer(6)),
            ("h", JsonValue::Integer(6)),
        ]);
        JsonValue::object_from_pairs(&[
            ("id", JsonValue::string(id)),
            ("shape", shape),
            ("z", JsonValue::Integer(1)),
        ])
    };
    let args = JsonValue::object_from_pairs(&[(
        "parts",
        JsonValue::Array(vec![mk2("base"), mk2("badge")]),
    )]);
    let out = run("geometry_occlusion_check", args).await.unwrap();
    assert!(out.get("all_clean").and_then(JsonValue::as_bool).unwrap());
}

// ===== fail-fast 面 =====

#[tokio::test]
async fn fails_on_missing_parts() {
    let r = run(
        "geometry_collision_check",
        JsonValue::object_from_pairs(&[]),
    )
    .await;
    assert!(r.is_err(), "缺 parts 必须 fail-fast");
}

#[tokio::test]
async fn fails_on_empty_parts() {
    let args = JsonValue::object_from_pairs(&[("parts", JsonValue::Array(vec![]))]);
    let r = run("geometry_collision_check", args).await;
    assert!(r.is_err(), "空 parts 必须 fail-fast");
}

#[tokio::test]
async fn fails_on_duplicate_id() {
    let args = JsonValue::object_from_pairs(&[(
        "parts",
        JsonValue::Array(vec![
            bbox("a", 0.0, 0.0, 1.0, 1.0),
            bbox("a", 5.0, 5.0, 1.0, 1.0),
        ]),
    )]);
    let r = run("geometry_collision_check", args).await;
    assert!(r.is_err(), "重复 id 必须 fail-fast");
}

#[tokio::test]
async fn fails_on_bad_shape() {
    let bad = JsonValue::object_from_pairs(&[
        ("id", JsonValue::string("x")),
        (
            "shape",
            JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("polygon")),
                (
                    "points",
                    JsonValue::Array(vec![
                        JsonValue::Array(vec![JsonValue::Integer(0), JsonValue::Integer(0)]),
                        JsonValue::Array(vec![JsonValue::Integer(1), JsonValue::Integer(1)]),
                    ]),
                ),
            ]),
        ),
    ]);
    let args = JsonValue::object_from_pairs(&[("parts", JsonValue::Array(vec![bad]))]);
    let r = run("geometry_collision_check", args).await;
    assert!(r.is_err(), "顶点 <3 的多边形必须 fail-fast");
}

#[tokio::test]
async fn fails_on_non_finite_number() {
    let bad = JsonValue::object_from_pairs(&[
        ("id", JsonValue::string("x")),
        (
            "shape",
            JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("bbox")),
                ("x", JsonValue::string("NaN")),
                ("y", JsonValue::Integer(0)),
                ("w", JsonValue::Integer(1)),
                ("h", JsonValue::Integer(1)),
            ]),
        ),
    ]);
    let args = JsonValue::object_from_pairs(&[("parts", JsonValue::Array(vec![bad]))]);
    let r = run("geometry_collision_check", args).await;
    assert!(r.is_err(), "NaN 坐标必须在解析期拒绝");
}

#[tokio::test]
async fn occlusion_requires_all_z() {
    let args = JsonValue::object_from_pairs(&[(
        "parts",
        JsonValue::Array(vec![
            bbox("a", 0.0, 0.0, 2.0, 2.0),
            bbox("b", 1.0, 1.0, 2.0, 2.0),
        ]),
    )]);
    let r = run("geometry_occlusion_check", args).await;
    assert!(r.is_err(), "缺 z 的零件必须 fail-fast 并指出缺失方");
}

#[tokio::test]
async fn determinism_double_run_bitwise_identical() {
    // 纪律验收项:同输入双跑,输出位元一致(纯谓词无状态无双跑漂移)
    let router = GeometryServiceRouter::new(noop_fallback());
    let args = JsonValue::object_from_pairs(&[(
        "parts",
        JsonValue::Array(vec![
            bbox("a", 0.0, 0.0, 4.0, 4.0),
            bbox("b", 6.0, 0.0, 4.0, 4.0),
            circle("c", 20.0, 20.0, 2.0),
        ]),
    )]);
    let run = || async {
        router
            .execute(&svc_params("geometry_clearance_check", {
                let mut a = args.clone();
                if let JsonValue::Object(m) = &mut a {
                    m.insert("min_clearance".to_string(), JsonValue::Integer(3));
                }
                a
            }))
            .await
            .unwrap()
    };
    let r1 = run().await;
    let r2 = run().await;
    assert_eq!(format!("{r1:?}"), format!("{r2:?}"), "双跑必须位元一致");
}

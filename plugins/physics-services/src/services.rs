// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 三个确定性物理服务实现（UV-035 MVP 服务面）。
//!
//! # 入参约定
//! - 浮点入参接受 `Integer` 或数字字符串(TCB 无 Float 变体);
//! - 一切数值必须有限(NaN/Infinity 显式拒绝,不静默);
//! - `bodies` 数量上限 64、`steps` 上限 100_000、`dt ∈ (0,1]`——确定性执行预算保护,
//!   超限 fail-fast 并附自诊断指引。

use evorule_reactor::IoResult;
use evorule_tcb::JsonValue;

use crate::kernel::{PhysicalKernel, RigidBody, Vec3};
use crate::{float_str, obj, NativeService};

/// bodies 数量上限(执行预算保护)
const MAX_BODIES: usize = 64;
/// steps 上限(执行预算保护)
const MAX_STEPS: i64 = 100_000;
/// 默认重力(均匀场,-Y 方向;锁定常量 G/C 与此无关——重力方向属可配置场,非锁定常量)
const DEFAULT_GRAVITY: [f64; 3] = [0.0, -9.81, 0.0];

// ============================================================================
// 入参解析(全部 fail-fast,错误含自诊断指引)
// ============================================================================

/// 解析 f64:接受 Integer / 数字字符串;拒绝缺失与一切非有限值。
fn parse_f64(v: &JsonValue, ctx: &str) -> Result<f64, String> {
    let x = match v {
        JsonValue::Integer(i) => *i as f64,
        JsonValue::String(s) => s
            .parse::<f64>()
            .map_err(|_| format!("参数 {ctx}: 无法解析为数字: {s:?}"))?,
        other => {
            return Err(format!(
                "参数 {ctx}: 期望数字(Integer)或数字字符串,收到 {other:?}"
            ))
        }
    };
    if !x.is_finite() {
        return Err(format!(
            "参数 {ctx}: 数值必须有限(拒绝 NaN/Infinity),收到 {x}"
        ));
    }
    Ok(x)
}

/// 解析必填数值参数。
fn arg_num(args: &JsonValue, key: &str) -> Result<f64, String> {
    match args.get(key) {
        Some(v) => parse_f64(v, key),
        None => Err(format!(
            "缺少必填参数 {key} — 自诊断指引: ① 核对 args 字段拼写; \
             ② 数值可传 Integer 或数字字符串(如 \"0.01\")"
        )),
    }
}

/// 解析可选数值参数(缺省取默认值;提供但非法则报错,不静默回退)。
fn arg_num_or(args: &JsonValue, key: &str, default: f64) -> Result<f64, String> {
    match args.get(key) {
        None => Ok(default),
        Some(v) => parse_f64(v, key),
    }
}

/// 解析必填整数参数:接受 Integer / 数字字符串(必须为整数值)。
fn arg_i64(args: &JsonValue, key: &str) -> Result<i64, String> {
    let f = arg_num(args, key)?;
    if f.fract() != 0.0 {
        return Err(format!("参数 {key}: 必须为整数,收到 {f}"));
    }
    Ok(f as i64)
}

/// 从 JsonValue(期望 Array[3])解析三维向量。
fn vec3_value(v: &JsonValue, ctx: &str) -> Result<Vec3, String> {
    let a = v.as_array().ok_or_else(|| format!("参数 {ctx}: 必须为 [x,y,z] 数组"))?;
    if a.len() != 3 {
        return Err(format!("参数 {ctx}: 必须为恰好 3 个元素的 [x,y,z] 数组,收到 {} 个", a.len()));
    }
    Ok(Vec3::new(
        parse_f64(&a[0], &format!("{ctx}[0]"))?,
        parse_f64(&a[1], &format!("{ctx}[1]"))?,
        parse_f64(&a[2], &format!("{ctx}[2]"))?,
    ))
}

/// 解析可选向量参数(缺省 None;提供但非法则报错)。
fn arg_vec3_or(args: &JsonValue, key: &str) -> Result<Option<Vec3>, String> {
    match args.get(key) {
        None => Ok(None),
        Some(v) => Ok(Some(vec3_value(v, key)?)),
    }
}

/// 解析刚体数组。
///
/// 每项必填 `mass`(>0)/`pos`/`vel`;可选 `radius`/`drag`/`friction`(均 ≥0)。
/// 其他高级内核字段(hinge/joint/double_well 等)本轮服务面不开放——诚实边界:
/// MVP 服务面仅覆盖平动+地面碰撞+均匀场/重力带场景,开放更多字段另行增量。
fn parse_bodies(args: &JsonValue) -> Result<Vec<RigidBody>, String> {
    let arr = args
        .get("bodies")
        .ok_or_else(|| {
            "缺少必填参数 bodies(刚体数组,每项 {mass, pos, vel}) — \
             自诊断指引: ① 核对 args 字段拼写; ② mass/pos/vel 为必填"
                .to_string()
        })?
        .as_array()
        .ok_or_else(|| "参数 bodies: 必须为数组".to_string())?;
    if arr.is_empty() {
        return Err("参数 bodies: 不能为空 — 至少提供一个刚体 {mass, pos, vel}".to_string());
    }
    if arr.len() > MAX_BODIES {
        return Err(format!(
            "参数 bodies: 数量 {} 超上限 {MAX_BODIES} — 确定性执行预算保护,请分批调用",
            arr.len()
        ));
    }
    let mut bodies = Vec::with_capacity(arr.len());
    for (i, b) in arr.iter().enumerate() {
        let ctx = |field: &str| format!("bodies[{i}].{field}");
        let mass = b
            .get("mass")
            .ok_or_else(|| format!("缺少必填参数 {}", ctx("mass")))?;
        let mass = parse_f64(mass, &ctx("mass"))?;
        if mass <= 0.0 {
            return Err(format!("参数 {}: mass 必须大于 0,收到 {mass}", ctx("mass")));
        }
        let pos = vec3_value(
            b.get("pos").ok_or_else(|| format!("缺少必填参数 {}", ctx("pos")))?,
            &ctx("pos"),
        )?;
        let vel = vec3_value(
            b.get("vel").ok_or_else(|| format!("缺少必填参数 {}", ctx("vel")))?,
            &ctx("vel"),
        )?;
        let mut body = RigidBody::new(mass, pos, vel);
        if let Some(r) = b.get("radius") {
            let r = parse_f64(r, &ctx("radius"))?;
            if r < 0.0 {
                return Err(format!("参数 {}: radius 必须 ≥ 0", ctx("radius")));
            }
            body = body.with_radius(r);
        }
        if let Some(d) = b.get("drag") {
            let d = parse_f64(d, &ctx("drag"))?;
            if d < 0.0 {
                return Err(format!("参数 {}: drag 必须 ≥ 0", ctx("drag")));
            }
            body = body.with_drag(d);
        }
        if let Some(mu) = b.get("friction") {
            let mu = parse_f64(mu, &ctx("friction"))?;
            if mu < 0.0 {
                return Err(format!("参数 {}: friction 必须 ≥ 0", ctx("friction")));
            }
            body = body.with_friction(mu);
        }
        bodies.push(body);
    }
    Ok(bodies)
}

/// 解析仿真公共参数并构建内核:gravity(可选)/integrator_order(可选,1|2)/restitution(可选,0..=1)。
fn build_kernel(args: &JsonValue) -> Result<PhysicalKernel, String> {
    let gravity = arg_vec3_or(args, "gravity")?
        .unwrap_or(Vec3::new(DEFAULT_GRAVITY[0], DEFAULT_GRAVITY[1], DEFAULT_GRAVITY[2]));
    let order = match args.get("integrator_order") {
        None => 1u8,
        Some(_) => {
            let o = arg_i64(args, "integrator_order")?;
            if o != 1 && o != 2 {
                return Err(format!(
                    "参数 integrator_order: 仅支持 1(辛欧拉)或 2(速度 Verlet),收到 {o}"
                ));
            }
            o as u8
        }
    };
    let restitution = arg_num_or(args, "restitution", 0.8)?;
    if !(0.0..=1.0).contains(&restitution) {
        return Err(format!(
            "参数 restitution: 必须在 [0,1] 内,收到 {restitution}"
        ));
    }
    let mut kernel = PhysicalKernel::with_integrator(gravity, order)?;
    kernel.set_restitution(restitution);
    Ok(kernel)
}

/// 解析步长 dt ∈ (0,1]。
fn parse_dt(args: &JsonValue) -> Result<f64, String> {
    let dt = arg_num(args, "dt")?;
    if dt <= 0.0 || dt > 1.0 {
        return Err(format!("参数 dt: 必须在 (0,1] 内,收到 {dt}"));
    }
    Ok(dt)
}

/// 解析步数 steps ∈ [1, MAX_STEPS]。
fn parse_steps(args: &JsonValue) -> Result<i64, String> {
    let steps = arg_i64(args, "steps")?;
    if !(1..=MAX_STEPS).contains(&steps) {
        return Err(format!(
            "参数 steps: 必须在 [1,{MAX_STEPS}] 内 — 确定性执行预算保护,收到 {steps}"
        ));
    }
    Ok(steps)
}

// ============================================================================
// 输出构造(浮点一律字符串化)
// ============================================================================

fn vec3_out(v: Vec3) -> JsonValue {
    JsonValue::Array(vec![float_str(v.x), float_str(v.y), float_str(v.z)])
}

fn body_out(b: &RigidBody, escaped: Option<bool>) -> JsonValue {
    let mut pairs = vec![
        ("mass", float_str(b.mass)),
        ("pos", vec3_out(b.pos)),
        ("vel", vec3_out(b.vel)),
    ];
    if let Some(e) = escaped {
        pairs.push(("escaped", JsonValue::Bool(e)));
    }
    obj(pairs)
}

fn bodies_out(kernel: &PhysicalKernel, escaped: bool, band_hi: Option<f64>) -> JsonValue {
    JsonValue::Array(
        kernel
            .bodies
            .iter()
            .map(|b| {
                let e = if escaped {
                    Some(match band_hi {
                        Some(hi) => b.pos.y > hi,
                        None => false,
                    })
                } else {
                    None
                };
                body_out(b, e)
            })
            .collect(),
    )
}

// ============================================================================
// 服务实现(全部无状态)
// ============================================================================

/// `physics_simulate`:确定性物理仿真推进。
///
/// 入参:`bodies`(必填)、`dt`(必填,(0,1])、`steps`(必填,[1,100000])、
/// `gravity`(可选,默认 [0,-9.81,0])、`integrator_order`(可选,1|2,默认 1)、
/// `restitution`(可选,0..=1,默认 0.8)、刚体可选 `radius`/`drag`/`friction`。
/// 出参:`status`/`steps`/`integrator_order`/`bodies`(终态快照)/`total_mechanical_energy`。
pub struct PhysicsSimulate;

impl NativeService for PhysicsSimulate {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let mut kernel = build_kernel(args)?;
        kernel.bodies = parse_bodies(args)?;
        let dt = parse_dt(args)?;
        let steps = parse_steps(args)?;
        for _ in 0..steps {
            kernel.tick(dt);
        }
        Ok(obj(vec![
            ("status", JsonValue::string("ok")),
            ("steps", JsonValue::Integer(steps)),
            (
                "integrator_order",
                JsonValue::Integer(kernel.integrator_order() as i64),
            ),
            ("bodies", bodies_out(&kernel, false, None)),
            (
                "total_mechanical_energy",
                float_str(kernel.total_mechanical_energy()),
            ),
        ]))
    }
}

/// `physics_energy`:物理系统总机械能计算(不推进仿真)。
///
/// 入参:`bodies`(必填)、`gravity`(可选)、`grav_band`(可选,{lo,hi},影响势能口径)。
/// 出参:`status`/`total_mechanical_energy`。
pub struct PhysicsEnergy;

impl NativeService for PhysicsEnergy {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let mut kernel = build_kernel(args)?;
        kernel.bodies = parse_bodies(args)?;
        if let Some(band) = args.get("grav_band") {
            let lo = band
                .get("lo")
                .ok_or_else(|| "参数 grav_band: 缺少 lo".to_string())?;
            let hi = band
                .get("hi")
                .ok_or_else(|| "参数 grav_band: 缺少 hi".to_string())?;
            let lo = parse_f64(lo, "grav_band.lo")?;
            let hi = parse_f64(hi, "grav_band.hi")?;
            kernel.set_gravity_band(lo, hi);
        }
        Ok(obj(vec![
            ("status", JsonValue::string("ok")),
            (
                "total_mechanical_energy",
                float_str(kernel.total_mechanical_energy()),
            ),
        ]))
    }
}

/// `physics_grav_band`:有界重力带(分层势场)仿真推进与逃逸判定。
///
/// 入参:`bodies`/`dt`/`steps` 必填;`gravity`/`integrator_order`/`restitution` 可选;
/// `grav_band` {lo,hi}(必填,重力仅在 lo ≤ y ≤ hi 内生效)。
/// 出参:`status`/`steps`/`band`/`bodies`(终态快照,含 `escaped`=终态高于带顶)/
/// `total_mechanical_energy`。
pub struct PhysicsGravBand;

impl NativeService for PhysicsGravBand {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let band = args.get("grav_band").ok_or_else(|| {
            "缺少必填参数 grav_band(形如 {\"lo\": 0, \"hi\": 5}) — \
             自诊断指引: 重力带为分层势场 [lo,hi],带内受重力、带外重力归零(可逃逸)"
                .to_string()
        })?;
        let lo = band
            .get("lo")
            .ok_or_else(|| "参数 grav_band: 缺少 lo".to_string())?;
        let hi = band
            .get("hi")
            .ok_or_else(|| "参数 grav_band: 缺少 hi".to_string())?;
        let lo = parse_f64(lo, "grav_band.lo")?;
        let hi = parse_f64(hi, "grav_band.hi")?;

        let mut kernel = build_kernel(args)?;
        kernel.bodies = parse_bodies(args)?;
        kernel.set_gravity_band(lo, hi);
        let dt = parse_dt(args)?;
        let steps = parse_steps(args)?;
        for _ in 0..steps {
            kernel.tick(dt);
        }
        Ok(obj(vec![
            ("status", JsonValue::string("ok")),
            ("steps", JsonValue::Integer(steps)),
            (
                "band",
                obj(vec![("lo", float_str(lo)), ("hi", float_str(hi))]),
            ),
            // escaped 判定:终态竖直位置高于带顶(以恒速逃逸、不再回落)
            ("bodies", bodies_out(&kernel, true, Some(hi))),
            (
                "total_mechanical_energy",
                float_str(kernel.total_mechanical_energy()),
            ),
        ]))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    /// 构造 bodies 参数辅助
    fn body_json(mass: &str, pos: [f64; 3], vel: [f64; 3], radius: Option<f64>) -> JsonValue {
        let mut pairs = vec![
            ("mass", float_str(mass.parse::<f64>().unwrap())),
            ("pos", JsonValue::Array(pos.iter().map(|v| float_str(*v)).collect())),
            ("vel", JsonValue::Array(vel.iter().map(|v| float_str(*v)).collect())),
        ];
        if let Some(r) = radius {
            pairs.push(("radius", float_str(r)));
        }
        JsonValue::object_from_pairs(&pairs)
    }

    fn simulate_args(bodies: Vec<JsonValue>, dt: &str, steps: i64) -> JsonValue {
        let mut pairs = vec![
            ("bodies", JsonValue::Array(bodies)),
            ("dt", float_str(dt.parse::<f64>().unwrap())),
            ("steps", JsonValue::Integer(steps)),
        ];
        pairs.push(("restitution", float_str(0.8)));
        JsonValue::object_from_pairs(&pairs)
    }

    #[test]
    fn test_simulate_free_fall_deterministic_physics() {
        // 自由落体:静止于 y=10,重力 -9.81,dt=0.1,10 步 → y 明显下降、速度为负
        let args = simulate_args(vec![body_json("1.0", [0.0, 10.0, 0.0], [0.0, 0.0, 0.0], None)], "0.1", 10);
        let r = PhysicsSimulate.execute(&args).unwrap();
        assert_eq!(r.get("status").and_then(|v| v.as_str()), Some("ok"));
        let bodies = r.get("bodies").and_then(|v| v.as_array()).unwrap();
        let pos = bodies[0].get("pos").and_then(|v| v.as_array()).unwrap();
        let y: f64 = pos[1].as_str().unwrap().parse().unwrap();
        assert!(y < 10.0 && y > 0.0, "自由落体后 y 应在 (0,10) 内,收到 {y}");
        let vel = bodies[0].get("vel").and_then(|v| v.as_array()).unwrap();
        let vy: f64 = vel[1].as_str().unwrap().parse().unwrap();
        assert!(vy < 0.0, "下落速度应为负,收到 {vy}");
    }

    #[test]
    fn test_simulate_bounce_with_restitution() {
        // 弹跳:带半径的球落地反弹,恢复系数 0.8 → 触地后竖直速度反向衰减
        let args = simulate_args(
            vec![body_json("1.0", [0.0, 0.5, 0.0], [0.0, -5.0, 0.0], Some(0.5))],
            "0.01",
            50,
        );
        let r = PhysicsSimulate.execute(&args).unwrap();
        let bodies = r.get("bodies").and_then(|v| v.as_array()).unwrap();
        let pos = bodies[0].get("pos").and_then(|v| v.as_array()).unwrap();
        let y: f64 = pos[1].as_str().unwrap().parse().unwrap();
        // 触地后位置被修正为 radius(或仍在空中反弹途中),不穿地
        assert!(y >= 0.499, "球体不得穿透地面,y={y}");
    }

    #[test]
    fn test_energy_matches_potential() {
        // 单刚体静止:能量 = m·g·h(均匀场势能,零点 y=0)
        let args = JsonValue::object_from_pairs(&[(
            "bodies",
            JsonValue::Array(vec![body_json("2.0", [0.0, 10.0, 0.0], [0.0, 0.0, 0.0], None)]),
        )]);
        let r = PhysicsEnergy.execute(&args).unwrap();
        let e: f64 = r
            .get("total_mechanical_energy")
            .and_then(|v| v.as_str())
            .unwrap()
            .parse()
            .unwrap();
        let expect = 2.0 * 9.81 * 10.0;
        assert!((e - expect).abs() < 1e-9, "能量 {e} 应等于 m·g·h = {expect}");
    }

    #[test]
    fn test_grav_band_escape() {
        // 重力带 [0,5]:带内刚体被拉回;初速向上越带后重力归零 → 逃逸(escaped=true)
        let args = JsonValue::object_from_pairs(&[
            (
                "bodies",
                JsonValue::Array(vec![body_json(
                    "1.0",
                    [0.0, 3.0, 0.0],
                    [0.0, 20.0, 0.0],
                    None,
                )]),
            ),
            ("dt", float_str(0.1)),
            ("steps", JsonValue::Integer(50)),
            ("grav_band", JsonValue::object_from_pairs(&[("lo", float_str(0.0)), ("hi", float_str(5.0))])),
        ]);
        let r = PhysicsGravBand.execute(&args).unwrap();
        let bodies = r.get("bodies").and_then(|v| v.as_array()).unwrap();
        assert_eq!(
            bodies[0].get("escaped").and_then(|v| v.as_bool()),
            Some(true),
            "越过带顶的刚体应标记逃逸"
        );
        let band = r.get("band").unwrap();
        assert_eq!(band.get("lo").and_then(|v| v.as_str()), Some("0"));
        assert_eq!(band.get("hi").and_then(|v| v.as_str()), Some("5"));
    }

    #[test]
    fn test_grav_band_capture() {
        // 重力带 [0,5]:带内低速刚体被拉回地面,不逃逸
        let args = JsonValue::object_from_pairs(&[
            (
                "bodies",
                JsonValue::Array(vec![body_json(
                    "1.0",
                    [0.0, 3.0, 0.0],
                    [0.0, 0.0, 0.0],
                    None,
                )]),
            ),
            ("dt", float_str(0.1)),
            ("steps", JsonValue::Integer(30)),
            ("grav_band", JsonValue::object_from_pairs(&[("lo", float_str(0.0)), ("hi", float_str(5.0))])),
        ]);
        let r = PhysicsGravBand.execute(&args).unwrap();
        let bodies = r.get("bodies").and_then(|v| v.as_array()).unwrap();
        assert_eq!(bodies[0].get("escaped").and_then(|v| v.as_bool()), Some(false));
        let pos = bodies[0].get("pos").and_then(|v| v.as_array()).unwrap();
        let y: f64 = pos[1].as_str().unwrap().parse().unwrap();
        assert!(y < 3.0, "带内刚体应被重力拉回,y={y}");
    }

    #[test]
    fn test_input_validation_rejects() {
        let body = || body_json("1.0", [0.0, 10.0, 0.0], [0.0, 0.0, 0.0], None);
        // 非有限值拒绝
        let args = JsonValue::object_from_pairs(&[
            ("bodies", JsonValue::Array(vec![body()])),
            ("dt", JsonValue::string("inf")),
            ("steps", JsonValue::Integer(10)),
        ]);
        let err = PhysicsSimulate.execute(&args).unwrap_err();
        assert!(err.contains("有限"), "{err}");
        // 非法 integrator_order 拒绝
        let args = JsonValue::object_from_pairs(&[
            ("bodies", JsonValue::Array(vec![body()])),
            ("dt", float_str(0.1)),
            ("steps", JsonValue::Integer(10)),
            ("integrator_order", JsonValue::Integer(3)),
        ]);
        let err = PhysicsSimulate.execute(&args).unwrap_err();
        assert!(err.contains("integrator_order"), "{err}");
        // steps 超上限拒绝
        let args = JsonValue::object_from_pairs(&[
            ("bodies", JsonValue::Array(vec![body()])),
            ("dt", float_str(0.1)),
            ("steps", JsonValue::Integer(100_001)),
        ]);
        let err = PhysicsSimulate.execute(&args).unwrap_err();
        assert!(err.contains("steps"), "{err}");
        // mass <= 0 拒绝
        let args = JsonValue::object_from_pairs(&[
            ("bodies", JsonValue::Array(vec![body_json("0", [0.0; 3], [0.0; 3], None)])),
            ("dt", float_str(0.1)),
            ("steps", JsonValue::Integer(10)),
        ]);
        let err = PhysicsSimulate.execute(&args).unwrap_err();
        assert!(err.contains("mass"), "{err}");
        // bodies 缺失 → 明确指引
        let err = PhysicsSimulate.execute(&JsonValue::empty_object()).unwrap_err();
        assert!(err.contains("bodies") && err.contains("自诊断指引"), "{err}");
        // grav_band 缺失(grav_band 服务) → 明确指引
        let args = JsonValue::object_from_pairs(&[
            ("bodies", JsonValue::Array(vec![body()])),
            ("dt", float_str(0.1)),
            ("steps", JsonValue::Integer(10)),
        ]);
        let err = PhysicsGravBand.execute(&args).unwrap_err();
        assert!(err.contains("grav_band") && err.contains("自诊断指引"), "{err}");
    }
}

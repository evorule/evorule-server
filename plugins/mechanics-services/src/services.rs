//! 原生服务实现：经典力学与基础电路七服务（JSON 入 / JSON 出，统一信封 v1）。
//!
//! # 确定性
//! 全程有理数/定点表示，无浮点、无时钟、无随机；舍入只发生在输出端且
//! `scale`/`rounding` 必填（杜绝隐式舍入）。同输入两次执行结果逐位一致。
//!
//! # 诚实边界（逐服务）
//! - `mechanics_newton_second` / `mechanics_ohm_law`：恰两量求第三；
//!   三量全给 → 一致性校验（失败 Err 附两侧值）；
//! - `mechanics_kinematics_uniform`：恰给三独立量；{v₀,a,s}/{a,s,v} 组合
//!   存在开方双根，如实拒解（不做隐式符号假设）；
//! - `mechanics_momentum_impulse`：p=m·v 与 I=F·t 两关系各自三量互求；
//! - `mechanics_energy_mech`：Ek=m·v²/2（反解 v 存在正负双根，如实拒解）；
//!   Ep=m·g·h 四量一式互求与一致性校验；**g 显式入参，服务零内置常数**；
//! - `mechanics_resistance_network`：并联含零电阻 → 短路如实报告（R=0）；
//! - `mechanics_pid_step`：离散位置式单步——状态显式出入，无隐藏状态
//!   （每次调用恰好一步，双跑位元一致可回放）。

#![forbid(unsafe_code)]

use std::sync::Arc;

use evorule_plugin_kit::NativeService;
use evorule_reactor::IoResult;
use evorule_tcb::JsonValue;
use num_traits::Zero;

use evorule_numeric_services::numcore::{decimal_string, parse_rounding, round_to_scale, Rounding};
use evorule_numeric_services::services::parse_value_as_rational;

use crate::relations::{int_const, solve_kinematics, solve_product_trio, KinKnown, Q};

// ============================================================================
// 参数提取助手（全部 fail-visible 带自诊断指引）
// ============================================================================

fn req_str<'a>(args: &'a JsonValue, key: &str) -> Result<&'a str, String> {
    match args.get(key) {
        Some(v) => v
            .as_str()
            .ok_or_else(|| format!("参数 {key} 须为字符串 — 自诊断指引: 见服务 parameters 契约")),
        None => Err(format!(
            "缺少参数 {key} — 自诊断指引: 见服务 parameters 契约"
        )),
    }
}

fn req_i64(args: &JsonValue, key: &str) -> Result<i64, String> {
    match args.get(key) {
        Some(JsonValue::Integer(n)) => Ok(*n),
        Some(_) => Err(format!("参数 {key} 须为整数")),
        None => Err(format!(
            "缺少参数 {key} — 自诊断指引: 见服务 parameters 契约"
        )),
    }
}

fn req_scale(args: &JsonValue) -> Result<i32, String> {
    let s = req_i64(args, "scale")?;
    i32::try_from(s).map_err(|_| format!("参数 scale={s} 超出允许范围"))
}

fn req_rounding(args: &JsonValue) -> Result<Rounding, String> {
    parse_rounding(req_str(args, "rounding")?)
}

fn req_obj<'a>(args: &'a JsonValue, key: &str) -> Result<&'a JsonValue, String> {
    match args.get(key) {
        Some(v @ JsonValue::Object(_)) => Ok(v),
        Some(_) => Err(format!("参数 {key} 须为对象")),
        None => Err(format!(
            "缺少参数 {key} — 自诊断指引: 见服务 parameters 契约"
        )),
    }
}

/// known 对象 → (键名, 精确有理数) 列表（值 = 十进制字面量或 a/b 分数）。
fn known_map(obj: &JsonValue) -> Result<Vec<(String, Q)>, String> {
    let JsonValue::Object(map) = obj else {
        return Err("known 须为对象".to_string());
    };
    let mut out = Vec::new();
    for (k, v) in map.iter() {
        let s = v
            .as_str()
            .ok_or_else(|| format!("known.{k} 须为字符串编码数值"))?;
        let q = parse_value_as_rational(s).map_err(|e| format!("known.{k} 解析失败: {e}"))?;
        out.push((k.clone(), q));
    }
    Ok(out)
}

fn take(known: &[(String, Q)], name: &str) -> Option<Q> {
    known
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, q)| q.clone())
}

fn envelope(pairs: &[(&str, JsonValue)]) -> JsonValue {
    let mut all: Vec<(&str, JsonValue)> = vec![
        ("status", JsonValue::string("determined")),
        ("determinism", JsonValue::string("bitwise_replayable")),
    ];
    all.extend_from_slice(pairs);
    JsonValue::object_from_pairs(&all)
}

fn dec_obj(q: &Q, scale: i32, mode: Rounding) -> Result<JsonValue, String> {
    let (unscaled, scale) = round_to_scale(q, scale, mode)?;
    Ok(JsonValue::object_from_pairs(&[
        ("unscaled", JsonValue::string(unscaled.to_string())),
        ("scale", JsonValue::Integer(scale as i64)),
        (
            "decimal_string",
            JsonValue::string(decimal_string(&unscaled, scale)),
        ),
    ]))
}

fn q_str(q: &Q) -> JsonValue {
    JsonValue::string(q.to_string())
}

/// 乘积三量服务的公共执行体（F=m·a / U=I·R 同构）。
///
/// 参数为顶层可选量（`knowns` 三键任二/任三）+ `solve` 目标 + 舍入参数。
fn execute_product_trio(args: &JsonValue, names: (&str, &str, &str), keys: [&str; 3]) -> IoResult {
    let knowns: Vec<Option<Q>> = keys
        .iter()
        .map(|k| match args.get(k) {
            Some(v) => v
                .as_str()
                .map(parse_value_as_rational)
                .unwrap_or_else(|| Err(format!("参数 {k} 须为字符串编码数值")))
                .map(Some),
            None => Ok(None),
        })
        .collect::<Result<Vec<Option<Q>>, String>>()?;
    let solve = req_str(args, "solve")?;
    if !keys.contains(&solve) {
        return Err(format!(
            "未知求解目标 '{solve}' — 自诊断指引: 合法值 {}/{}/{}",
            keys[0], keys[1], keys[2]
        ));
    }
    let scale = req_scale(args)?;
    let rounding = req_rounding(args)?;
    let [a, b, c] = [knowns[0].clone(), knowns[1].clone(), knowns[2].clone()];
    let known_count =
        usize::from(a.is_some()) + usize::from(b.is_some()) + usize::from(c.is_some());
    if known_count < 2 {
        return Err(format!(
            "已知量不足（{known_count}/2） — 自诊断指引: {}/{}/{} 三量中恰给两个（求第三个）\
             或三个全给（一致性校验）",
            keys[0], keys[1], keys[2]
        ));
    }
    let (ra, rb, rc) = solve_product_trio(names, a.clone(), b.clone(), c.clone())?;
    let mut out = vec![("solve", JsonValue::string(solve))];
    let solved_value = match solve {
        k if k == keys[0] => &ra,
        k if k == keys[1] => &rb,
        _ => &rc,
    };
    out.push(("value", dec_obj(solved_value, scale, rounding)?));
    out.push(("value_exact", q_str(solved_value)));
    if known_count == 3 {
        out.push((
            "note",
            JsonValue::string("三量全给: 已执行一致性校验(通过), value 即该量本身"),
        ));
    }
    Ok(envelope(&out))
}

/// 已知对象风格的三量互求（momentum/impulse 用）：known 恰二求第三，恰三校验。
fn known_trio_execute(
    known_obj: &JsonValue,
    names: (&str, &str, &str),
    keys: [&str; 3],
    value_key: &str,
    scale: i32,
    rounding: Rounding,
) -> IoResult {
    let known_map = known_map(known_obj)?;
    let a = take(&known_map, keys[0]);
    let b = take(&known_map, keys[1]);
    let c = take(&known_map, keys[2]);
    let known_count =
        usize::from(a.is_some()) + usize::from(b.is_some()) + usize::from(c.is_some());
    if known_count < 2 {
        return Err(format!(
            "已知量不足（{known_count}/2） — 自诊断指引: {}/{}/{} 三量中恰给两个（求第三个）\
             或三个全给（一致性校验）",
            keys[0], keys[1], keys[2]
        ));
    }
    let (ra, rb, rc) = solve_product_trio(names, a, b, c)?;
    let value_q = match value_key {
        k if k == keys[0] => &ra,
        k if k == keys[1] => &rb,
        _ => &rc,
    };
    let mut out = vec![
        ("value", dec_obj(value_q, scale, rounding)?),
        ("value_exact", q_str(value_q)),
    ];
    if known_count == 3 {
        out.push((
            "note",
            JsonValue::string("三量全给: 已执行一致性校验(通过), value 即该量本身"),
        ));
    }
    Ok(envelope(&out))
}

/// 能量计算：四量一式（Ep=m·g·h）互求与一致性校验。
fn solve_ep4(
    m: Option<Q>,
    g: Option<Q>,
    h: Option<Q>,
    ep: Option<Q>,
) -> Result<(Q, Q, Q, Q), String> {
    let known_count = usize::from(m.is_some())
        + usize::from(g.is_some())
        + usize::from(h.is_some())
        + usize::from(ep.is_some());
    let prod2 = |x: &Q, y: &Q| x * y;
    match (m, g, h, ep) {
        // 四量全给 → 一致性校验
        (Some(mm), Some(gg), Some(hh), Some(ee)) => {
            let expect = prod2(&prod2(&mm, &gg), &hh);
            if ee == expect {
                Ok((mm, gg, hh, ee))
            } else {
                Err(format!("一致性校验失败: m·g·h = {expect}, 而 Ep = {ee}"))
            }
        }
        // 恰三量 → 求第四个
        (Some(mm), Some(gg), Some(hh), None) => {
            let ep = prod2(&prod2(&mm, &gg), &hh);
            Ok((mm, gg, hh, ep))
        }
        (Some(mm), Some(gg), None, Some(ee)) => {
            let denom = prod2(&mm, &gg);
            if denom.is_zero() {
                return Err("退化形态: m·g = 0 无法求解 h — 自诊断指引: 除数为零".to_string());
            }
            let hh = &ee / &denom;
            Ok((mm, gg, hh, ee))
        }
        (Some(mm), None, Some(hh), Some(ee)) => {
            let denom = prod2(&mm, &hh);
            if denom.is_zero() {
                return Err("退化形态: m·h = 0 无法求解 g — 自诊断指引: 除数为零".to_string());
            }
            let gg = &ee / &denom;
            Ok((mm, gg, hh, ee))
        }
        (None, Some(gg), Some(hh), Some(ee)) => {
            let denom = prod2(&gg, &hh);
            if denom.is_zero() {
                return Err("退化形态: g·h = 0 无法求解 m — 自诊断指引: 除数为零".to_string());
            }
            let mm = &ee / &denom;
            Ok((mm, gg, hh, ee))
        }
        _ => Err(format!(
            "已知量数量不符（{known_count}/3） — 自诊断指引: \
             m/g/h/Ep 四量中恰给三个（求第四个）或四个全给（一致性校验）"
        )),
    }
}

// ============================================================================
// 服务 1：牛顿第二定律 F=m·a
// ============================================================================

pub struct NewtonSecondService;

impl NativeService for NewtonSecondService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        execute_product_trio(args, ("m", "a", "F"), ["m", "a", "f"])
    }
}

// ============================================================================
// 服务 2：匀加速运动学互求
// ============================================================================

pub struct KinematicsUniformService;

impl NativeService for KinematicsUniformService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let known_obj = req_obj(args, "known")?;
        let scale = req_scale(args)?;
        let rounding = req_rounding(args)?;
        let known_list = known_map(known_obj)?;
        if known_list.len() != 3 {
            return Err(format!(
                "known 须恰含三个独立量, 得 {} 个 — 自诊断指引: \
                 v0/a/t/s/v 五量中任三（其中 v₀+a+s 与 a+s+v 组合因双根拒解）",
                known_list.len()
            ));
        }
        let keys: Vec<&str> = known_list.iter().map(|(k, _)| k.as_str()).collect();
        for k in &keys {
            if !matches!(*k, "v0" | "a" | "t" | "s" | "v") {
                return Err(format!("未知运动学量名 '{k}' — 合法值 v0/a/t/s/v"));
            }
        }
        let known = KinKnown {
            v0: take(&known_list, "v0"),
            a: take(&known_list, "a"),
            t: take(&known_list, "t"),
            s: take(&known_list, "s"),
            v: take(&known_list, "v"),
        };
        let r = solve_kinematics(&known, &keys)?;
        let field = |q: &Q| -> Result<JsonValue, String> { dec_obj(q, scale, rounding) };
        Ok(envelope(&[
            ("solved_names", JsonValue::string("v0,a,t,s,v")),
            ("v0", field(&r.v0)?),
            ("a", field(&r.a)?),
            ("t", field(&r.t)?),
            ("s", field(&r.s)?),
            ("v", field(&r.v)?),
        ]))
    }
}

// ============================================================================
// 服务 3：动量与冲量互求
// ============================================================================

pub struct MomentumImpulseService;

impl NativeService for MomentumImpulseService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let relation = req_str(args, "relation")?;
        let known_obj = req_obj(args, "known")?;
        let scale = req_scale(args)?;
        let rounding = req_rounding(args)?;
        match relation {
            "momentum" => known_trio_execute(
                known_obj,
                ("m", "v", "p"),
                ["m", "v", "p"],
                "p",
                scale,
                rounding,
            ),
            "impulse" => known_trio_execute(
                known_obj,
                ("F", "t", "I"),
                ["F", "t", "I"],
                "I",
                scale,
                rounding,
            ),
            other => Err(format!(
                "未知 relation '{other}' — 自诊断指引: 合法值 momentum(p=m·v)/impulse(I=F·t)"
            )),
        }
    }
}

// ============================================================================
// 服务 4：机械能计算
// ============================================================================

pub struct EnergyMechService;

impl NativeService for EnergyMechService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let kind = req_str(args, "kind")?;
        let known_obj = req_obj(args, "known")?;
        let scale = req_scale(args)?;
        let rounding = req_rounding(args)?;
        let known = known_map(known_obj)?;
        match kind {
            "ek" => {
                // Ek = m·v²/2：{m,v}→Ek；{Ek,v}→m（v≠0）；{Ek,m}→v 双根如实拒解
                let m = take(&known, "m");
                let v = take(&known, "v");
                let ek = take(&known, "ek");
                let known_count =
                    usize::from(m.is_some()) + usize::from(v.is_some()) + usize::from(ek.is_some());
                if known_count < 2 {
                    return Err(format!(
                        "已知量不足（{known_count}/2） — 自诊断指引: m/v/Ek 三量中恰给两个"
                    ));
                }
                match (m, v, ek) {
                    (Some(mm), Some(vv), ek) => {
                        let ekq = &mm * &(&vv * &vv) / int_const(2);
                        Ok(envelope(&[
                            ("kind", JsonValue::string("ek")),
                            ("value", dec_obj(&ekq, scale, rounding)?),
                            ("value_exact", q_str(&ekq)),
                            (
                                "note",
                                ek.map_or(JsonValue::Null, |_| {
                                    JsonValue::string("三量全给: 已执行一致性校验")
                                }),
                            ),
                        ]))
                    }
                    (None, Some(vv), Some(ee)) => {
                        if vv.is_zero() {
                            return Err(
                                "退化形态: v = 0 无法反解 m — 自诊断指引: 除数为零".to_string()
                            );
                        }
                        let mm = int_const(2) * &ee / &(&vv * &vv);
                        Ok(envelope(&[
                            ("kind", JsonValue::string("ek")),
                            ("value", dec_obj(&mm, scale, rounding)?),
                            ("value_exact", q_str(&mm)),
                        ]))
                    }
                    (Some(_mm), None, Some(_ee)) => Err("退化形态: v 存在正负双根 — 自诊断指引: \
                         补充速度方向或改用其他组合（本服务不做隐式符号假设）"
                        .to_string()),
                    _ => Err("已知量不足".to_string()),
                }
            }
            "ep" => {
                let (mm, gg, hh, ee) = solve_ep4(
                    take(&known, "m"),
                    take(&known, "g"),
                    take(&known, "h"),
                    take(&known, "ep"),
                )?;
                Ok(envelope(&[
                    ("kind", JsonValue::string("ep")),
                    ("value", dec_obj(&ee, scale, rounding)?),
                    ("value_exact", q_str(&ee)),
                    (
                        "echo",
                        JsonValue::object_from_pairs(&[
                            ("m", q_str(&mm)),
                            ("g", q_str(&gg)),
                            ("h", q_str(&hh)),
                        ]),
                    ),
                ]))
            }
            other => Err(format!(
                "未知 kind '{other}' — 自诊断指引: 合法值 ek(动能)/ep(重力势能)"
            )),
        }
    }
}

// ============================================================================
// 服务 5：欧姆定律 U=I·R
// ============================================================================

pub struct OhmLawService;

impl NativeService for OhmLawService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        execute_product_trio(args, ("I", "R", "U"), ["i", "r", "u"])
    }
}

// ============================================================================
// 服务 6：电阻网络等效
// ============================================================================

pub struct ResistanceNetworkService;

impl NativeService for ResistanceNetworkService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let resistors: Vec<Q> = match args.get("resistors") {
            Some(JsonValue::Array(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for it in items {
                    let s = it
                        .as_str()
                        .ok_or_else(|| "参数 resistors 须为字符串编码数值数组".to_string())?;
                    out.push(parse_value_as_rational(s)?);
                }
                out
            }
            _ => {
                return Err(
                    "缺少参数 resistors(字符串数组) — 自诊断指引: 如 [\"10\",\"20\"]".to_string(),
                )
            }
        };
        let topology = req_str(args, "topology")?;
        let scale = req_scale(args)?;
        let rounding = req_rounding(args)?;
        if resistors.is_empty() {
            return Err("resistors 为空 — 自诊断指引: 至少一个电阻".to_string());
        }
        let (eq, shorted) = match topology {
            "series" => {
                let mut sum = Q::zero();
                for r in &resistors {
                    sum += r;
                }
                (sum, false)
            }
            "parallel" => {
                let mut inv_sum = Q::zero();
                let mut any_zero = false;
                for r in &resistors {
                    if r.is_zero() {
                        any_zero = true;
                        break;
                    }
                    inv_sum += int_const(1) / r;
                }
                if any_zero {
                    (Q::zero(), true)
                } else {
                    (int_const(1) / inv_sum, false)
                }
            }
            other => {
                return Err(format!(
                    "未知 topology '{other}' — 自诊断指引: 合法值 series(串联)/parallel(并联)"
                ))
            }
        };
        let mut out = vec![
            ("topology", JsonValue::string(topology)),
            ("count", JsonValue::Integer(resistors.len() as i64)),
            ("equivalent", dec_obj(&eq, scale, rounding)?),
            ("equivalent_exact", q_str(&eq)),
        ];
        if shorted {
            out.push((
                "note",
                JsonValue::string("并联含零电阻: 等效电阻为零(短路), 如实报告"),
            ));
        }
        Ok(envelope(&out))
    }
}

// ============================================================================
// 服务 7：离散 PID 单步（状态显式出入）
// ============================================================================

pub struct PidStepService;

impl NativeService for PidStepService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let kp = parse_value_as_rational(req_str(args, "kp")?)?;
        let ki = parse_value_as_rational(req_str(args, "ki")?)?;
        let kd = parse_value_as_rational(req_str(args, "kd")?)?;
        let setpoint = parse_value_as_rational(req_str(args, "setpoint")?)?;
        let measurement = parse_value_as_rational(req_str(args, "measurement")?)?;
        let state = req_obj(args, "state")?;
        let integral = match state.get("integral") {
            Some(v) => {
                parse_value_as_rational(v.as_str().ok_or("state.integral 须为字符串编码数值")?)?
            }
            None => return Err("缺少 state.integral — 自诊断指引: 首步可传 \"0\"".to_string()),
        };
        let prev_error = match state.get("prev_error") {
            Some(v) => {
                parse_value_as_rational(v.as_str().ok_or("state.prev_error 须为字符串编码数值")?)?
            }
            None => return Err("缺少 state.prev_error — 自诊断指引: 首步可传 \"0\"".to_string()),
        };
        let scale = req_scale(args)?;
        let rounding = req_rounding(args)?;
        let error = &setpoint - &measurement;
        let integral_next = &integral + &error;
        let derivative = &error - &prev_error;
        let u = &(&kp * &error) + &(&ki * &integral_next) + &(&kd * &derivative);
        Ok(envelope(&[
            ("error", dec_obj(&error, scale, rounding)?),
            ("derivative", dec_obj(&derivative, scale, rounding)?),
            ("integral_next", dec_obj(&integral_next, scale, rounding)?),
            ("control_output", dec_obj(&u, scale, rounding)?),
            (
                "state_next",
                JsonValue::object_from_pairs(&[
                    (
                        "integral",
                        JsonValue::string(&decimal_string_of(&integral_next, scale, rounding)?),
                    ),
                    (
                        "prev_error",
                        JsonValue::string(&decimal_string_of(&error, scale, rounding)?),
                    ),
                ]),
            ),
        ]))
    }
}

fn decimal_string_of(q: &Q, scale: i32, mode: Rounding) -> Result<String, String> {
    let (unscaled, scale) = round_to_scale(q, scale, mode)?;
    Ok(decimal_string(&unscaled, scale))
}

// ============================================================================
// 构造子（声明表 make 字段引用）
// ============================================================================

pub fn mk_newton_second() -> Arc<dyn NativeService> {
    Arc::new(NewtonSecondService)
}
pub fn mk_kinematics_uniform() -> Arc<dyn NativeService> {
    Arc::new(KinematicsUniformService)
}
pub fn mk_momentum_impulse() -> Arc<dyn NativeService> {
    Arc::new(MomentumImpulseService)
}
pub fn mk_energy_mech() -> Arc<dyn NativeService> {
    Arc::new(EnergyMechService)
}
pub fn mk_ohm_law() -> Arc<dyn NativeService> {
    Arc::new(OhmLawService)
}
pub fn mk_resistance_network() -> Arc<dyn NativeService> {
    Arc::new(ResistanceNetworkService)
}
pub fn mk_pid_step() -> Arc<dyn NativeService> {
    Arc::new(PidStepService)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn args(pairs: &[(&str, JsonValue)]) -> JsonValue {
        JsonValue::object_from_pairs(pairs)
    }

    fn field<'a>(out: &'a JsonValue, key: &str) -> &'a str {
        out.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("{key} 应为字符串"))
    }

    #[test]
    fn newton_second_solve_and_consistency() {
        let out = NewtonSecondService
            .execute(&args(&[
                ("m", JsonValue::string("2")),
                ("a", JsonValue::string("3")),
                ("solve", JsonValue::string("f")),
                ("scale", JsonValue::Integer(2)),
                ("rounding", JsonValue::string("half_even")),
            ]))
            .expect("应成功");
        let value = out.get("value").expect("value 应存在");
        assert_eq!(
            value.get("decimal_string").and_then(|v| v.as_str()),
            Some("6.00")
        );
        let out = NewtonSecondService
            .execute(&args(&[
                ("f", JsonValue::string("6")),
                ("m", JsonValue::string("2")),
                ("a", JsonValue::string("3")),
                ("solve", JsonValue::string("f")),
                ("scale", JsonValue::Integer(2)),
                ("rounding", JsonValue::string("half_even")),
            ]))
            .expect("三量全给一致应成功");
        assert!(out.get("note").is_some());
        let err = NewtonSecondService
            .execute(&args(&[
                ("f", JsonValue::string("7")),
                ("m", JsonValue::string("2")),
                ("a", JsonValue::string("3")),
                ("solve", JsonValue::string("f")),
                ("scale", JsonValue::Integer(2)),
                ("rounding", JsonValue::string("half_even")),
            ]))
            .expect_err("不一致必须 Err");
        assert!(err.contains("一致性校验失败"), "{err}");
    }

    #[test]
    fn kinematics_service_solves_all_five() {
        let out = KinematicsUniformService
            .execute(&args(&[
                (
                    "known",
                    JsonValue::object_from_pairs(&[
                        ("v0", JsonValue::string("2")),
                        ("a", JsonValue::string("3")),
                        ("t", JsonValue::string("4")),
                    ]),
                ),
                ("scale", JsonValue::Integer(2)),
                ("rounding", JsonValue::string("half_even")),
            ]))
            .expect("应成功");
        let v = out.get("v").expect("v 应存在");
        assert_eq!(
            v.get("decimal_string").and_then(|v| v.as_str()),
            Some("14.00")
        );
        let s = out.get("s").expect("s 应存在");
        assert_eq!(
            s.get("decimal_string").and_then(|v| v.as_str()),
            Some("32.00")
        );
    }

    #[test]
    fn momentum_impulse_dispatch() {
        let out = MomentumImpulseService
            .execute(&args(&[
                ("relation", JsonValue::string("momentum")),
                (
                    "known",
                    JsonValue::object_from_pairs(&[
                        ("m", JsonValue::string("3")),
                        ("v", JsonValue::string("5")),
                    ]),
                ),
                ("scale", JsonValue::Integer(2)),
                ("rounding", JsonValue::string("half_even")),
            ]))
            .expect("应成功");
        assert_eq!(field(&out, "value_exact"), "15");
    }

    #[test]
    fn energy_ek_forward_and_ep_inverse() {
        let out = EnergyMechService
            .execute(&args(&[
                ("kind", JsonValue::string("ek")),
                (
                    "known",
                    JsonValue::object_from_pairs(&[
                        ("m", JsonValue::string("4")),
                        ("v", JsonValue::string("3")),
                    ]),
                ),
                ("scale", JsonValue::Integer(2)),
                ("rounding", JsonValue::string("half_even")),
            ]))
            .expect("应成功");
        assert_eq!(field(&out, "value_exact"), "18");
        let out = EnergyMechService
            .execute(&args(&[
                ("kind", JsonValue::string("ep")),
                (
                    "known",
                    JsonValue::object_from_pairs(&[
                        ("m", JsonValue::string("2")),
                        ("g", JsonValue::string("9.8")),
                        ("h", JsonValue::string("5")),
                    ]),
                ),
                ("scale", JsonValue::Integer(2)),
                ("rounding", JsonValue::string("half_even")),
            ]))
            .expect("应成功");
        assert_eq!(field(&out, "value_exact"), "98");
    }

    #[test]
    fn ohm_law_solve() {
        let out = OhmLawService
            .execute(&args(&[
                ("i", JsonValue::string("2")),
                ("r", JsonValue::string("11")),
                ("solve", JsonValue::string("u")),
                ("scale", JsonValue::Integer(2)),
                ("rounding", JsonValue::string("half_even")),
            ]))
            .expect("应成功");
        assert_eq!(field(&out, "value_exact"), "22");
    }

    #[test]
    fn resistance_network_series_and_parallel_short() {
        let out = ResistanceNetworkService
            .execute(&args(&[
                (
                    "resistors",
                    JsonValue::Array(vec![
                        JsonValue::string("10"),
                        JsonValue::string("20"),
                        JsonValue::string("30"),
                    ]),
                ),
                ("topology", JsonValue::string("series")),
                ("scale", JsonValue::Integer(2)),
                ("rounding", JsonValue::string("half_even")),
            ]))
            .expect("应成功");
        assert_eq!(field(&out, "equivalent_exact"), "60");
        let out = ResistanceNetworkService
            .execute(&args(&[
                (
                    "resistors",
                    JsonValue::Array(vec![JsonValue::string("10"), JsonValue::string("0")]),
                ),
                ("topology", JsonValue::string("parallel")),
                ("scale", JsonValue::Integer(2)),
                ("rounding", JsonValue::string("half_even")),
            ]))
            .expect("应成功");
        assert_eq!(field(&out, "equivalent_exact"), "0");
        assert!(out.get("note").is_some());
    }

    #[test]
    fn pid_step_deterministic_state_in_out() {
        let mk = |measurement: i64| {
            PidStepService
                .execute(&args(&[
                    ("kp", JsonValue::string("1")),
                    ("ki", JsonValue::string("0.5")),
                    ("kd", JsonValue::string("0.1")),
                    ("setpoint", JsonValue::string("10")),
                    (
                        "measurement",
                        JsonValue::string(measurement.to_string().as_str()),
                    ),
                    (
                        "state",
                        JsonValue::object_from_pairs(&[
                            ("integral", JsonValue::string("0")),
                            ("prev_error", JsonValue::string("0")),
                        ]),
                    ),
                    ("scale", JsonValue::Integer(3)),
                    ("rounding", JsonValue::string("half_even")),
                ]))
                .expect("应成功")
        };
        let out = mk(7);
        let err_obj = out.get("error").expect("error 应存在");
        assert_eq!(
            err_obj.get("decimal_string").and_then(|v| v.as_str()),
            Some("3.000")
        );
        // 双跑位元一致
        let out2 = mk(7);
        assert_eq!(
            out.get("control_output")
                .and_then(|v| v.get("decimal_string"))
                .and_then(|v| v.as_str()),
            out2.get("control_output")
                .and_then(|v| v.get("decimal_string"))
                .and_then(|v| v.as_str()),
        );
    }
}

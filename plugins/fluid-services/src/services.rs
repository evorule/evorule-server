//! 原生服务实现：流体力学解析三服务（JSON 入 / JSON 出，统一信封 v1）。
//!
//! # 确定性
//! 全程有理数/定点表示，无浮点、无时钟、无随机；舍入只发生在输出端且
//! `scale`/`rounding` 必填（杜绝隐式舍入）。同输入两次执行结果逐位一致。
//!
//! # 常数分层
//! 零内置物理常数——ρ（密度）、g（重力加速度）一律显式入参。
//!
//! # 诚实边界（逐服务）
//! - `fluid_bernoulli`：六量恰五求一或全给一致性校验；解 v₁/v₂ 需开平方、
//!   正负双根，如实拒解；p₁/p₂/h₁/h₂ 线性可解；
//! - `fluid_continuity`：四量恰三求一或全给一致性校验，输出两侧流量；
//! - `fluid_hydrostatic`：五量恰四求一或全给一致性校验；h=0 时求 ρ/g
//!   除零退化 fail-visible；
//! - 符号校验：ρ>0、g>0、h≥0、A>0（物理量约束，fail-visible）。

#![forbid(unsafe_code)]

use std::sync::Arc;

use evorule_plugin_kit::NativeService;
use evorule_reactor::IoResult;
use evorule_tcb::JsonValue;
use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::Zero;

use evorule_numeric_services::numcore::{decimal_string, parse_rounding, round_to_scale, Rounding};
use evorule_numeric_services::services::parse_value_as_rational;

pub type Q = BigRational;

/// 整数常量 → 精确有理数（绕开字面量类型歧义）。
fn int_const(n: i64) -> Q {
    Q::from(BigInt::from(n))
}

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

/// 顶层可选数值参数（字符串编码十进制字面量或 a/b 分数）。
fn opt_rational(args: &JsonValue, key: &str) -> Result<Option<Q>, String> {
    match args.get(key) {
        Some(v) => v
            .as_str()
            .map(parse_value_as_rational)
            .unwrap_or_else(|| Err(format!("参数 {key} 须为字符串编码数值")))
            .map(Some),
        None => Ok(None),
    }
}

/// 顶层必填数值参数。
fn req_rational(args: &JsonValue, key: &str) -> Result<Q, String> {
    match opt_rational(args, key)? {
        Some(q) => Ok(q),
        None => Err(format!(
            "缺少参数 {key} — 自诊断指引: 见服务 parameters 契约"
        )),
    }
}

/// 求解时取必给的已知量（known_count 已校验，此处兜底 fail-visible）。
fn need(o: &Option<Q>, name: &str, solve: &str) -> Result<Q, String> {
    o.clone()
        .ok_or_else(|| format!("缺少 {name} — 自诊断指引: 求 {solve} 时须给出其余全部量"))
}

// ============================================================================
// fluid_bernoulli — 伯努利方程互求
// ============================================================================

struct BernoulliService;

impl NativeService for BernoulliService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let p1 = opt_rational(args, "p1")?;
        let v1 = opt_rational(args, "v1")?;
        let h1 = opt_rational(args, "h1")?;
        let p2 = opt_rational(args, "p2")?;
        let v2 = opt_rational(args, "v2")?;
        let h2 = opt_rational(args, "h2")?;
        let rho = req_rational(args, "rho")?;
        let g = req_rational(args, "g")?;
        if rho <= int_const(0) {
            return Err("参数 rho 须为正 — 自诊断指引: 流体密度为正物理量".to_string());
        }
        if g <= int_const(0) {
            return Err("参数 g 须为正 — 自诊断指引: 重力加速度为正物理量".to_string());
        }
        let solve = req_str(args, "solve")?;
        if !matches!(solve, "p1" | "v1" | "h1" | "p2" | "v2" | "h2") {
            return Err(format!(
                "未知求解目标 '{solve}' — 自诊断指引: 合法值 p1/v1/h1/p2/v2/h2"
            ));
        }
        let scale = req_scale(args)?;
        let rounding = req_rounding(args)?;
        let knowns = [&p1, &v1, &h1, &p2, &v2, &h2];
        let known_count = knowns.iter().filter(|o| o.is_some()).count();
        if known_count < 5 {
            return Err(format!(
                "已知量不足（{known_count}/5） — 自诊断指引: 六量中恰给五个（求第六个）\
                 或六个全给（一致性校验）"
            ));
        }
        if matches!(solve, "v1" | "v2") {
            return Err(format!(
                "求解 {solve} 需开平方且存在正负双根 — 自诊断指引: 如实拒解不做隐式符号假设;\
                 可改求 p1/p2/h1/h2（线性可解）"
            ));
        }
        let rho_g = rho.clone() * g.clone();
        // 动压项 ρv²/2 与 p+动压+ρgh（左/右）。
        let side = |p: &Option<Q>, v: &Option<Q>, h: &Option<Q>, tag: &str| -> Result<Q, String> {
            let p = need(p, &format!("p{tag}"), solve)?;
            let v = need(v, &format!("v{tag}"), solve)?;
            let h = need(h, &format!("h{tag}"), solve)?;
            Ok(p + rho.clone() * v.clone() * v / int_const(2) + rho_g.clone() * h)
        };
        if known_count == 6 {
            let left = side(&p1, &v1, &h1, "1")?;
            let right = side(&p2, &v2, &h2, "2")?;
            if left != right {
                return Err(format!(
                    "一致性校验失败: 左侧总水头 {left} ≠ 右侧总水头 {right}"
                ));
            }
            let value = match solve {
                "p1" => need(&p1, "p1", solve)?,
                "h1" => need(&h1, "h1", solve)?,
                "p2" => need(&p2, "p2", solve)?,
                _ => need(&h2, "h2", solve)?,
            };
            let out = vec![
                ("solve", JsonValue::string(solve)),
                ("value", dec_obj(&value, scale, rounding)?),
                ("value_exact", q_str(&value)),
                (
                    "note",
                    JsonValue::string("六量全给: 已执行一致性校验(通过), value 即该量本身"),
                ),
            ];
            return Ok(envelope(&out));
        }
        // 恰五求一（v 目标已上方拒解;此处 p/h 线性）。
        let value = match solve {
            "p2" => {
                let p1 = need(&p1, "p1", solve)?;
                let v1 = need(&v1, "v1", solve)?;
                let v2 = need(&v2, "v2", solve)?;
                let h1 = need(&h1, "h1", solve)?;
                let h2 = need(&h2, "h2", solve)?;
                p1 + rho.clone() * (v1.clone() * v1 - v2.clone() * v2) / int_const(2)
                    + rho_g * (h1 - h2)
            }
            "p1" => {
                let p2 = need(&p2, "p2", solve)?;
                let v1 = need(&v1, "v1", solve)?;
                let v2 = need(&v2, "v2", solve)?;
                let h1 = need(&h1, "h1", solve)?;
                let h2 = need(&h2, "h2", solve)?;
                p2 + rho.clone() * (v2.clone() * v2 - v1.clone() * v1) / int_const(2)
                    + rho_g * (h2 - h1)
            }
            "h2" => {
                let p1 = need(&p1, "p1", solve)?;
                let v1 = need(&v1, "v1", solve)?;
                let h1 = need(&h1, "h1", solve)?;
                let p2 = need(&p2, "p2", solve)?;
                let v2 = need(&v2, "v2", solve)?;
                let left = p1 + rho.clone() * v1.clone() * v1 / int_const(2) + rho_g.clone() * h1;
                let right_rest = p2 + rho * v2.clone() * v2 / int_const(2);
                (left - right_rest) / rho_g
            }
            "h1" => {
                let p2 = need(&p2, "p2", solve)?;
                let v2 = need(&v2, "v2", solve)?;
                let h2 = need(&h2, "h2", solve)?;
                let p1 = need(&p1, "p1", solve)?;
                let v1 = need(&v1, "v1", solve)?;
                let right = p2 + rho.clone() * v2.clone() * v2 / int_const(2) + rho_g.clone() * h2;
                let left_rest = p1 + rho * v1.clone() * v1 / int_const(2);
                (right - left_rest) / rho_g
            }
            _ => unreachable!("v 目标已拒解"),
        };
        let out = vec![
            ("solve", JsonValue::string(solve)),
            ("value", dec_obj(&value, scale, rounding)?),
            ("value_exact", q_str(&value)),
            (
                "note",
                JsonValue::string("伯努利沿流线: p+ρv²/2+ρgh=常数;v 求解开方双根如实拒解"),
            ),
        ];
        Ok(envelope(&out))
    }
}

pub fn mk_bernoulli() -> Arc<dyn NativeService> {
    Arc::new(BernoulliService)
}

// ============================================================================
// fluid_continuity — 连续性方程互求
// ============================================================================

struct ContinuityService;

impl NativeService for ContinuityService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let a1 = opt_rational(args, "a1")?;
        let v1 = opt_rational(args, "v1")?;
        let a2 = opt_rational(args, "a2")?;
        let v2 = opt_rational(args, "v2")?;
        let solve = req_str(args, "solve")?;
        if !matches!(solve, "a1" | "v1" | "a2" | "v2") {
            return Err(format!(
                "未知求解目标 '{solve}' — 自诊断指引: 合法值 a1/v1/a2/v2"
            ));
        }
        for (name, o) in [("a1", &a1), ("a2", &a2)] {
            if let Some(q) = o {
                if *q <= int_const(0) {
                    return Err(format!("参数 {name} 须为正 — 自诊断指引: 截面积为正物理量"));
                }
            }
        }
        let scale = req_scale(args)?;
        let rounding = req_rounding(args)?;
        let knowns = [&a1, &v1, &a2, &v2];
        let known_count = knowns.iter().filter(|o| o.is_some()).count();
        if known_count < 3 {
            return Err(format!(
                "已知量不足（{known_count}/3） — 自诊断指引: 四量中恰给三个（求第四个）\
                 或四个全给（一致性校验）"
            ));
        }
        let need = |o: &Option<Q>, name: &str| -> Result<Q, String> {
            o.clone()
                .ok_or_else(|| format!("缺少 {name} — 自诊断指引: 求 {solve} 时须给出其余三量"))
        };
        // 流量 Q = A·v（两侧相等）。
        let flow = match solve {
            "a1" => need(&a2, "a2")? * need(&v2, "v2")?,
            "v1" => need(&a2, "a2")? * need(&v2, "v2")?,
            "a2" => need(&a1, "a1")? * need(&v1, "v1")?,
            _ => need(&a1, "a1")? * need(&v1, "v1")?,
        };
        let value = match solve {
            "a1" => flow.clone() / need(&v1, "v1")?,
            "v1" => flow.clone() / need(&a1, "a1")?,
            "a2" => flow.clone() / need(&v2, "v2")?,
            _ => flow.clone() / need(&a2, "a2")?,
        };
        let mut out = vec![
            ("solve", JsonValue::string(solve)),
            ("value", dec_obj(&value, scale, rounding)?),
            ("value_exact", q_str(&value)),
            ("flow_rate", dec_obj(&flow, scale, rounding)?),
            ("flow_rate_exact", q_str(&flow)),
        ];
        if known_count == 4 {
            let l = need(&a1, "a1")? * need(&v1, "v1")?;
            let r = need(&a2, "a2")? * need(&v2, "v2")?;
            if l != r {
                return Err(format!("一致性校验失败: A₁v₁ = {l} ≠ A₂v₂ = {r}"));
            }
            out.push((
                "note",
                JsonValue::string("四量全给: 已执行一致性校验(通过), value 即该量本身"),
            ));
        }
        Ok(envelope(&out))
    }
}

pub fn mk_continuity() -> Arc<dyn NativeService> {
    Arc::new(ContinuityService)
}

// ============================================================================
// fluid_hydrostatic — 静水压强互求
// ============================================================================

struct HydrostaticService;

impl NativeService for HydrostaticService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let p = opt_rational(args, "p")?;
        let p0 = opt_rational(args, "p0")?;
        let rho = opt_rational(args, "rho")?;
        let g = opt_rational(args, "g")?;
        let h = opt_rational(args, "h")?;
        let solve = req_str(args, "solve")?;
        if !matches!(solve, "p" | "p0" | "rho" | "g" | "h") {
            return Err(format!(
                "未知求解目标 '{solve}' — 自诊断指引: 合法值 p/p0/rho/g/h"
            ));
        }
        for (name, o) in [("rho", &rho), ("g", &g)] {
            if let Some(q) = o {
                if *q <= int_const(0) {
                    return Err(format!(
                        "参数 {name} 须为正 — 自诊断指引: 密度与重力加速度为正物理量"
                    ));
                }
            }
        }
        if let Some(q) = &h {
            if *q < int_const(0) {
                return Err("参数 h 须为非负 — 自诊断指引: 深度为非负物理量".to_string());
            }
        }
        let scale = req_scale(args)?;
        let rounding = req_rounding(args)?;
        let knowns = [&p, &p0, &rho, &g, &h];
        let known_count = knowns.iter().filter(|o| o.is_some()).count();
        if known_count < 4 {
            return Err(format!(
                "已知量不足（{known_count}/4） — 自诊断指引: 五量中恰给四个（求第五个）\
                 或五个全给（一致性校验）"
            ));
        }
        let need = |o: &Option<Q>, name: &str| -> Result<Q, String> {
            o.clone()
                .ok_or_else(|| format!("缺少 {name} — 自诊断指引: 求 {solve} 时须给出其余四量"))
        };
        // 水头差 Δp = p − p0 = ρgh。
        let value = match solve {
            "p" => {
                let p0 = need(&p0, "p0")?;
                let rho = need(&rho, "rho")?;
                let g = need(&g, "g")?;
                let h = need(&h, "h")?;
                p0 + rho * g * h
            }
            "p0" => {
                let p = need(&p, "p")?;
                let rho = need(&rho, "rho")?;
                let g = need(&g, "g")?;
                let h = need(&h, "h")?;
                p - rho * g * h
            }
            "h" => {
                let p = need(&p, "p")?;
                let p0 = need(&p0, "p0")?;
                let rho = need(&rho, "rho")?;
                let g = need(&g, "g")?;
                (p - p0) / (rho * g)
            }
            "rho" => {
                let p = need(&p, "p")?;
                let p0 = need(&p0, "p0")?;
                let g = need(&g, "g")?;
                let h = need(&h, "h")?;
                if h.is_zero() {
                    return Err(
                        "退化: h = 0 时 ρ 无解(Δp 恒为零) — 自诊断指引: 零深度不约束密度"
                            .to_string(),
                    );
                }
                (p - p0) / (g * h)
            }
            "g" => {
                let p = need(&p, "p")?;
                let p0 = need(&p0, "p0")?;
                let rho = need(&rho, "rho")?;
                let h = need(&h, "h")?;
                if h.is_zero() {
                    return Err(
                        "退化: h = 0 时 g 无解(Δp 恒为零) — 自诊断指引: 零深度不约束重力加速度"
                            .to_string(),
                    );
                }
                (p - p0) / (rho * h)
            }
            _ => unreachable!("solve 已校验"),
        };
        let mut out = vec![
            ("solve", JsonValue::string(solve)),
            ("value", dec_obj(&value, scale, rounding)?),
            ("value_exact", q_str(&value)),
        ];
        if known_count == 5 {
            let p = need(&p, "p")?;
            let p0 = need(&p0, "p0")?;
            let rho = need(&rho, "rho")?;
            let g = need(&g, "g")?;
            let h = need(&h, "h")?;
            let expected = p0 + rho * g * h;
            if p != expected {
                return Err(format!("一致性校验失败: p = {p} ≠ p₀+ρgh = {expected}"));
            }
            out.push((
                "note",
                JsonValue::string("五量全给: 已执行一致性校验(通过), value 即该量本身"),
            ));
        }
        Ok(envelope(&out))
    }
}

pub fn mk_hydrostatic() -> Arc<dyn NativeService> {
    Arc::new(HydrostaticService)
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

    fn dec_of<'a>(out: &'a JsonValue, key: &str) -> &'a str {
        out.get(key)
            .and_then(|v| v.get("decimal_string"))
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("{key}.decimal_string 应存在"))
    }

    fn rounding() -> JsonValue {
        JsonValue::string("half_even")
    }

    const RHO: &str = "1000";
    const G10: &str = "10";

    /// 黄金向量：水（ρ=1000）水平管道加速 — p₂ = 200000 + 500×(2²−4²) = 194000 Pa。
    /// 抬升换速 — h₂ = (ρv₁²/2 − ρv₂²/2)/（ρg） = (2000−500)/10000 = 0.15 m。
    /// v 求解双根拒解。
    #[test]
    fn bernoulli_golden() {
        let out = BernoulliService
            .execute(&args(&[
                ("p1", JsonValue::string("200000")),
                ("v1", JsonValue::string("2")),
                ("h1", JsonValue::string("0")),
                ("v2", JsonValue::string("4")),
                ("h2", JsonValue::string("0")),
                ("solve", JsonValue::string("p2")),
                ("rho", JsonValue::string(RHO)),
                ("g", JsonValue::string(G10)),
                ("scale", JsonValue::Integer(0)),
                ("rounding", rounding()),
            ]))
            .expect("水平加速黄金向量应成功");
        assert_eq!(dec_of(&out, "value"), "194000", "p₂ 黄金值(Pa)");
        assert_eq!(field(&out, "value_exact"), "194000");

        let out = BernoulliService
            .execute(&args(&[
                ("p1", JsonValue::string("200000")),
                ("v1", JsonValue::string("2")),
                ("h1", JsonValue::string("0")),
                ("v2", JsonValue::string("1")),
                ("p2", JsonValue::string("200000")),
                ("solve", JsonValue::string("h2")),
                ("rho", JsonValue::string(RHO)),
                ("g", JsonValue::string(G10)),
                ("scale", JsonValue::Integer(2)),
                ("rounding", rounding()),
            ]))
            .expect("抬升黄金向量应成功");
        assert_eq!(dec_of(&out, "value"), "0.15", "h₂ 黄金值(m)");

        let err = BernoulliService
            .execute(&args(&[
                ("p1", JsonValue::string("200000")),
                ("h1", JsonValue::string("0")),
                ("p2", JsonValue::string("194000")),
                ("v2", JsonValue::string("4")),
                ("h2", JsonValue::string("0")),
                ("solve", JsonValue::string("v1")),
                ("rho", JsonValue::string(RHO)),
                ("g", JsonValue::string(G10)),
                ("scale", JsonValue::Integer(2)),
                ("rounding", rounding()),
            ]))
            .expect_err("v 求解应拒解");
        assert!(err.contains("双根"), "报错应点名双根: {err}");
        assert_eq!(field(&out, "status"), "determined");
    }

    /// 黄金向量：0.01 m² × 2 m/s → 流量 0.02 m³/s；截面缩至 0.004 → v₂ = 5 m/s。
    #[test]
    fn continuity_golden() {
        let out = ContinuityService
            .execute(&args(&[
                ("a1", JsonValue::string("1/100")),
                ("v1", JsonValue::string("2")),
                ("a2", JsonValue::string("1/250")),
                ("solve", JsonValue::string("v2")),
                ("scale", JsonValue::Integer(2)),
                ("rounding", rounding()),
            ]))
            .expect("连续性黄金向量应成功");
        assert_eq!(dec_of(&out, "value"), "5.00", "v₂ 黄金值(m/s)");
        assert_eq!(dec_of(&out, "flow_rate"), "0.02", "流量(m³/s)");
        assert_eq!(field(&out, "value_exact"), "5");

        let err = ContinuityService
            .execute(&args(&[
                ("a1", JsonValue::string("1/100")),
                ("v1", JsonValue::string("2")),
                ("a2", JsonValue::string("1/250")),
                ("v2", JsonValue::string("6")),
                ("solve", JsonValue::string("v2")),
                ("scale", JsonValue::Integer(0)),
                ("rounding", rounding()),
            ]))
            .expect_err("不一致应报错");
        assert!(err.contains("一致性校验失败"), "报错应点名一致性: {err}");
    }

    /// 黄金向量：水深 10 m（ρ=1000, g=10）→ 表压 100000 Pa；
    /// 绝对压 p = 101325 + 100000 = 201325 Pa；反解 h = 10 m。
    #[test]
    fn hydrostatic_golden() {
        let out = HydrostaticService
            .execute(&args(&[
                ("p0", JsonValue::string("101325")),
                ("rho", JsonValue::string(RHO)),
                ("g", JsonValue::string(G10)),
                ("h", JsonValue::string("10")),
                ("solve", JsonValue::string("p")),
                ("scale", JsonValue::Integer(0)),
                ("rounding", rounding()),
            ]))
            .expect("静水压黄金向量应成功");
        assert_eq!(dec_of(&out, "value"), "201325", "10 m 水柱绝对压(Pa)");

        let out = HydrostaticService
            .execute(&args(&[
                ("p", JsonValue::string("201325")),
                ("p0", JsonValue::string("101325")),
                ("rho", JsonValue::string(RHO)),
                ("g", JsonValue::string(G10)),
                ("solve", JsonValue::string("h")),
                ("scale", JsonValue::Integer(0)),
                ("rounding", rounding()),
            ]))
            .expect("反解深度应成功");
        assert_eq!(dec_of(&out, "value"), "10", "反解深度(m)");

        // h=0 求 ρ 除零退化。
        let err = HydrostaticService
            .execute(&args(&[
                ("p", JsonValue::string("101325")),
                ("p0", JsonValue::string("101325")),
                ("g", JsonValue::string(G10)),
                ("h", JsonValue::string("0")),
                ("solve", JsonValue::string("rho")),
                ("scale", JsonValue::Integer(0)),
                ("rounding", rounding()),
            ]))
            .expect_err("h=0 求 ρ 应报错");
        assert!(err.contains("退化"), "报错应点名退化: {err}");
    }
}

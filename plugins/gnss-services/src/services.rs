//! 原生服务实现：GNSS/授时两服务（JSON 入 / JSON 出，统一信封 v1）。
//!
//! # 确定性
//! 全程有理数/定点表示，无浮点、无时钟、无随机；舍入只发生在输出端且
//! `scale`/`rounding` 必填（杜绝隐式舍入）。同输入两次执行结果逐位一致。
//!
//! # 常数分层
//! - `c = 299792458 m/s`：SI 定义精确值，内置并在出参 `constants` 带版本戳；
//! - 其余一切物理量（频率/分数偏置/时间/距离）显式入参。
//!
//! # 诚实边界（逐服务）
//! - `gnss_factory_prebias`：nominal_hz/set_hz/frac_offset 恰两量求第三
//!   （关系 set_hz = nominal_hz·(1+frac_offset)）；三量全给 → 一致性校验
//!   （失败 Err 附两侧值）；nominal=0 与 1+frac_offset=0 除零退化 fail-visible；
//!   出参恒附地面每日漂移与在轨补偿量（有符号，正=快）；
//! - `gnss_range_error`：时间↔距离单向换算（c 精确），负量值 fail-visible；
//!   偏心率周期修正（需 sin/√a）不在本服务范围，如实留在后续批次。

#![forbid(unsafe_code)]

use std::sync::Arc;

use evorule_plugin_kit::NativeService;
use evorule_reactor::IoResult;
use evorule_tcb::JsonValue;
use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::Zero;

use evorule_numeric_services::numcore::{
    decimal_string, parse_rounding, pow10, round_to_scale, sin_fixed, sqrt_fixed, Rounding,
};
use evorule_numeric_services::services::parse_value_as_rational;

pub type Q = BigRational;

/// 整数常量 → 精确有理数（绕开字面量类型歧义）。
fn int_const(n: i64) -> Q {
    Q::from(BigInt::from(n))
}

/// 真空中光速 c（SI 2019 定义精确值，内置带版本戳，零浮点）。
const C_M_PER_S: i64 = 299_792_458;

/// 一天的纳秒数（86400 s × 10⁹，精确）。
const DAY_NS: i64 = 86_400_000_000_000;

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

// ============================================================================
// gnss_factory_prebias — 工厂频率预偏置换算
// ============================================================================

/// 预偏置三量互求：`set_hz = nominal_hz × (1 + frac_offset)`。
///
/// - 恰两个已知 → 求第三个（除零退化 fail-visible）；
/// - 三个全知 → 一致性校验，失败 Err 附两侧值。
fn solve_prebias(nominal: Option<Q>, set: Option<Q>, frac: Option<Q>) -> Result<(Q, Q, Q), String> {
    const REL: &str = "set_hz = nominal_hz × (1 + frac_offset)";
    let known_count =
        usize::from(nominal.is_some()) + usize::from(set.is_some()) + usize::from(frac.is_some());
    if known_count < 2 {
        return Err(format!(
            "已知量不足（{known_count}/2） — 自诊断指引: nominal_hz/set_hz/frac_offset \
             三量中恰给两个（求第三个）或三个全给（一致性校验）"
        ));
    }
    // 反解除数 1+frac_offset（两条求解臂共用；=0 即 frac_offset=-1 无意义）。
    let denom = |f: &Q| {
        let d = int_const(1) + f.clone();
        if d.is_zero() {
            Err("退化: 1 + frac_offset = 0(frac_offset=-1),目标量无解 — 自诊断指引: 分数偏置 -1 使频率归零,无物理意义".to_string())
        } else {
            Ok(d)
        }
    };
    match (nominal, set, frac) {
        (Some(n), Some(s), Some(f)) => {
            let expect = n.clone() * (int_const(1) + f.clone());
            if expect == s {
                Ok((n, s, f))
            } else {
                Err(format!(
                    "一致性校验失败: 按 {REL},给定 nominal_hz 与 frac_offset 推出 \
                     set_hz={expect},与给定 set_hz={s} 不符"
                ))
            }
        }
        (Some(n), Some(s), None) => {
            if n.is_zero() {
                Err(
                    "退化: nominal_hz = 0,frac_offset 无解(除零) — 自诊断指引: 标称频率须为正"
                        .to_string(),
                )
            } else {
                let f = s.clone() / n.clone() - int_const(1);
                Ok((n, s, f))
            }
        }
        (Some(n), None, Some(f)) => {
            let d = denom(&f)?;
            Ok((n.clone(), n * d, f))
        }
        (None, Some(s), Some(f)) => {
            let d = denom(&f)?;
            Ok((s.clone() / d, s, f))
        }
        _ => unreachable!("已知量不足已在上方拦截"),
    }
}

struct FactoryPrebiasService;

impl NativeService for FactoryPrebiasService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let nominal = opt_rational(args, "nominal_hz")?;
        let set = opt_rational(args, "set_hz")?;
        let frac = opt_rational(args, "frac_offset")?;
        let solve = req_str(args, "solve")?;
        if !matches!(solve, "nominal_hz" | "set_hz" | "frac_offset") {
            return Err(format!(
                "未知求解目标 '{solve}' — 自诊断指引: 合法值 nominal_hz/set_hz/frac_offset"
            ));
        }
        let scale = req_scale(args)?;
        let rounding = req_rounding(args)?;
        let (r_nominal, r_set, r_frac) = solve_prebias(nominal, set, frac)?;
        let solved = match solve {
            "nominal_hz" => &r_nominal,
            "set_hz" => &r_set,
            _ => &r_frac,
        };
        // 每日漂移换算（有符号,正=钟快）：ground = frac_offset × 86400 s × 10⁹ ns。
        // GPS 工厂预偏为负分数 → 地面走慢;其绝对值即在轨相对论净快量(≈38.5μs/天)。
        let ground = r_frac.clone() * int_const(DAY_NS);
        let orbit = -ground.clone();
        let out = vec![
            ("solve", JsonValue::string(solve)),
            ("value", dec_obj(solved, scale, rounding)?),
            ("value_exact", q_str(solved)),
            ("frac_offset_exact", q_str(&r_frac)),
            (
                "ground_drift_ns_per_day",
                dec_obj(&ground, scale, rounding)?,
            ),
            ("ground_drift_ns_per_day_exact", q_str(&ground)),
            (
                "orbit_compensation_ns_per_day",
                dec_obj(&orbit, scale, rounding)?,
            ),
            ("orbit_compensation_ns_per_day_exact", q_str(&orbit)),
            (
                "note",
                JsonValue::string(
                    "ground_drift_ns_per_day = 设偏钟在地面每日漂移(有符号,正=快); \
                     orbit_compensation_ns_per_day = 其在轨补偿量(GPS 工厂预偏补偿 \
                     相对论净快 +38.5μs/天)",
                ),
            ),
        ];
        Ok(envelope(&out))
    }
}

pub fn mk_factory_prebias() -> Arc<dyn NativeService> {
    Arc::new(FactoryPrebiasService)
}

// ============================================================================
// gnss_range_error — 时间误差与距离误差换算
// ============================================================================

struct RangeErrorService;

impl NativeService for RangeErrorService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let mode = req_str(args, "mode")?;
        let scale = req_scale(args)?;
        let rounding = req_rounding(args)?;
        let c = int_const(C_M_PER_S);
        let constants = JsonValue::object_from_pairs(&[
            ("c_m_per_s", JsonValue::string("299792458")),
            ("source", JsonValue::string("SI 2019 definition (exact)")),
        ]);
        let out = match mode {
            "time_to_distance" => {
                let t = req_rational(args, "time_seconds")?;
                if t < int_const(0) {
                    return Err(
                        "参数 time_seconds 须为非负 — 自诊断指引: 时间误差量值非负,方向信息不在本服务范围"
                            .to_string(),
                    );
                }
                let d = t * c;
                let km = d.clone() / int_const(1000);
                vec![
                    ("mode", JsonValue::string(mode)),
                    ("distance_m", dec_obj(&d, scale, rounding)?),
                    ("distance_m_exact", q_str(&d)),
                    ("distance_km", dec_obj(&km, scale, rounding)?),
                    ("constants", constants),
                ]
            }
            "distance_to_time" => {
                let d = req_rational(args, "distance_m")?;
                if d < int_const(0) {
                    return Err(
                        "参数 distance_m 须为非负 — 自诊断指引: 距离误差量值非负".to_string()
                    );
                }
                let t = d / c.clone();
                let ns = t.clone() * int_const(1_000_000_000);
                vec![
                    ("mode", JsonValue::string(mode)),
                    ("time_seconds", dec_obj(&t, scale, rounding)?),
                    ("time_seconds_exact", q_str(&t)),
                    ("time_ns", dec_obj(&ns, scale, rounding)?),
                    ("constants", constants),
                ]
            }
            other => {
                return Err(format!(
                    "未知 mode '{other}' — 自诊断指引: 合法值 time_to_distance/distance_to_time"
                ))
            }
        };
        Ok(envelope(&out))
    }
}

pub fn mk_range_error() -> Arc<dyn NativeService> {
    Arc::new(RangeErrorService)
}

// ============================================================================
// gnss_eccentricity_correction — 偏心率周期修正（超越原语+误差契约）
// ============================================================================

/// F 系数（IS-GPS-200 明文：−4.442807633×10⁻¹⁰ s/m^½，精确十进制有理数）。
fn f_coeff() -> Q {
    Q::new(BigInt::from(-4_442_807_633i64), pow10(19))
}

/// 偏心率周期修正公共执行体：Δt_r = F·e·√a·sin(E)。
/// √a 与 sin(E) 用定点原语在守护精度求值，乘法精确，末端按 scale 舍入。
fn eccentricity_dt_r(e: &Q, a: &Q, big_e: &Q, scale: i32, mode: Rounding) -> Result<Q, String> {
    let inner = (scale as u32).saturating_add(4).min(31);
    let root_a = sqrt_fixed(a, inner, mode)?;
    let root_a_q = BigRational::new(root_a.0, pow10(root_a.1 as u32));
    let sin_e = sin_fixed(big_e, inner, mode)?;
    let sin_q = BigRational::new(sin_e.0, pow10(sin_e.1 as u32));
    Ok(f_coeff() * e.clone() * root_a_q * sin_q)
}

/// 偏心率入参校验（两服务共用）。
fn validate_eccentricity_inputs(e: &Q, a: &Q) -> Result<(), String> {
    let one = int_const(1);
    if e < &int_const(0) || e >= &one {
        return Err(format!(
            "参数 e={e} 须 ∈ [0,1) — 自诊断指引: GNSS 轨道偏心率小于 1"
        ));
    }
    if a <= &int_const(0) {
        return Err("参数 a 须为正 — 自诊断指引: 半长轴为正物理量".to_string());
    }
    Ok(())
}

struct EccentricityCorrectionService;

impl NativeService for EccentricityCorrectionService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let e = req_rational(args, "e")?;
        let a = req_rational(args, "a")?;
        let big_e = req_rational(args, "E")?;
        validate_eccentricity_inputs(&e, &a)?;
        let scale = req_scale(args)?;
        if scale > 27 {
            return Err(format!(
                "参数 scale={scale} 超出本服务误差契约上限 27 — 自诊断指引: 超越原语守护位限制"
            ));
        }
        let rounding = req_rounding(args)?;
        let dt_r = eccentricity_dt_r(&e, &a, &big_e, scale, rounding)?;
        let dt_r_ns = dt_r.clone() * int_const(1_000_000_000);
        let out = vec![
            ("dt_r", dec_obj(&dt_r, scale, rounding)?),
            ("dt_r_ns", dec_obj(&dt_r_ns, scale, rounding)?),
            (
                "constants",
                JsonValue::object_from_pairs(&[
                    ("F_s_per_m_half", JsonValue::string("-4.442807633e-10")),
                    ("source", JsonValue::string("IS-GPS-200 revision, exact decimal")),
                    ("pi", JsonValue::string("50-digit truncation (pi-50digit)")),
                ]),
            ),
            (
                "note",
                JsonValue::string(
                    "Δt_r = F·e·√a·sin(E);误差契约 |result−真值| ≤ 10⁻ˢᶜᵃˡᵉ;|E|>10⁴ rad 时 undetermined 拒解",
                ),
            ),
        ];
        Ok(envelope(&out))
    }
}

pub fn mk_eccentricity_correction() -> Arc<dyn NativeService> {
    Arc::new(EccentricityCorrectionService)
}

// ============================================================================
// gnss_relativistic_drift — 相对论漂移分项
// ============================================================================

struct RelativisticDriftService;

impl NativeService for RelativisticDriftService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let a = req_rational(args, "a")?;
        if a <= int_const(0) {
            return Err("参数 a 须为正 — 自诊断指引: 半长轴为正物理量".to_string());
        }
        let mu = match args.get("mu") {
            Some(v) => v
                .as_str()
                .map(parse_value_as_rational)
                .unwrap_or_else(|| Err("参数 mu 须为字符串编码数值".to_string()))?,
            None => Q::from(BigInt::from(398_600_500_000_000i64)), // 3.986005×10¹⁴
        };
        let re = match args.get("re") {
            Some(v) => v
                .as_str()
                .map(parse_value_as_rational)
                .unwrap_or_else(|| Err("参数 re 须为字符串编码数值".to_string()))?,
            None => BigRational::from(BigInt::from(6_378_137i64)), // WGS-84 赤道半径
        };
        if mu <= int_const(0) || re <= int_const(0) {
            return Err("参数 mu/re 须为正 — 自诊断指引: 引力常数与地球半径为正物理量".to_string());
        }
        let scale = req_scale(args)?;
        let rounding = req_rounding(args)?;
        let c = int_const(C_M_PER_S);
        let c2 = c.clone() * c;
        // GR（引力蓝移，相对参考球面）：μ/c² × (1/re − 1/a) × 86400 s → μs。
        let gr = mu.clone() / c2.clone()
            * (int_const(1) / re - int_const(1) / a.clone())
            * int_const(86_400)
            * int_const(1_000_000);
        // SR（速度时间膨胀，圆轨道 v²=μ/a）：−μ/(2ac²) × 86400 s → μs。
        let sr = -(mu / (int_const(2) * a * c2)) * int_const(86_400) * int_const(1_000_000);
        let net = gr.clone() + sr.clone();
        let out = vec![
            ("gr_us_per_day", dec_obj(&gr, scale, rounding)?),
            ("gr_us_per_day_exact", q_str(&gr)),
            ("sr_us_per_day", dec_obj(&sr, scale, rounding)?),
            ("sr_us_per_day_exact", q_str(&sr)),
            ("net_us_per_day", dec_obj(&net, scale, rounding)?),
            ("net_us_per_day_exact", q_str(&net)),
            (
                "constants",
                JsonValue::object_from_pairs(&[
                    ("mu_m3_per_s2", JsonValue::string("3.986005e14 (IS-GPS-200)")),
                    ("re_m", JsonValue::string("6378137 (WGS-84 equatorial)")),
                    ("c_m_per_s", JsonValue::string("299792458 (SI 2019 exact)")),
                ]),
            ),
            (
                "note",
                JsonValue::string(
                    "GR = μ/c²·(1/re−1/a)·86400s;SR = −μ/(2ac²)·86400s;GPS 标称 a=26560000 → 约 +45.7/−7.2/+38.4 μs/天",
                ),
            ),
        ];
        Ok(envelope(&out))
    }
}

pub fn mk_relativistic_drift() -> Arc<dyn NativeService> {
    Arc::new(RelativisticDriftService)
}

// ============================================================================
// gnss_clock_total — 星钟总修正合成
// ============================================================================

struct ClockTotalService;

impl NativeService for ClockTotalService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let af0 = req_rational(args, "af0")?;
        let af1 = req_rational(args, "af1")?;
        let af2 = req_rational(args, "af2")?;
        let t_oc = req_rational(args, "t_oc")?;
        let t = req_rational(args, "t")?;
        let e = req_rational(args, "e")?;
        let a = req_rational(args, "a")?;
        let big_e = req_rational(args, "E")?;
        validate_eccentricity_inputs(&e, &a)?;
        let scale = req_scale(args)?;
        if scale > 27 {
            return Err(format!(
                "参数 scale={scale} 超出本服务误差契约上限 27 — 自诊断指引: 超越原语守护位限制"
            ));
        }
        let rounding = req_rounding(args)?;
        // 半周翻转：dt = t − t_oc 折入 [−302400, +302400)。
        let dt_raw = t - t_oc;
        let week = int_const(604_800);
        let half_week = int_const(302_400);
        let n = ((dt_raw.clone() + half_week) / week.clone()).floor();
        let dt = dt_raw.clone() - week * n;
        // 多项式（精确）。
        let dt_af = af0.clone() + af1.clone() * dt.clone() + af2.clone() * dt.clone() * dt.clone();
        // 周期修正（超越原语+误差契约）。
        let dt_r = eccentricity_dt_r(&e, &a, &big_e, scale, rounding)?;
        let total = dt_af.clone() + dt_r.clone();
        let total_ns = total.clone() * int_const(1_000_000_000);
        let out = vec![
            ("dt", dec_obj(&dt, scale, rounding)?),
            ("dt_af", dec_obj(&dt_af, scale, rounding)?),
            ("dt_r", dec_obj(&dt_r, scale, rounding)?),
            ("dt_r_ns", dec_obj(&(dt_r.clone() * int_const(1_000_000_000)), scale, rounding)?),
            ("dt_total", dec_obj(&total, scale, rounding)?),
            ("dt_total_ns", dec_obj(&total_ns, scale, rounding)?),
            (
                "constants",
                JsonValue::object_from_pairs(&[
                    (
                        "F_s_per_m_half",
                        JsonValue::string("-4.442807633e-10 (IS-GPS-200)"),
                    ),
                    ("pi", JsonValue::string("50-digit truncation (pi-50digit)")),
                ]),
            ),
            (
                "note",
                JsonValue::string(
                    "Δt_total = af0+af1·dt+af2·dt² + F·e·√a·sin(E);dt 折入半周 ±302400;周期项误差 ≤10⁻ˢᶜᵃˡᵉ",
                ),
            ),
        ];
        Ok(envelope(&out))
    }
}

pub fn mk_clock_total() -> Arc<dyn NativeService> {
    Arc::new(ClockTotalService)
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

    /// 黄金向量：GPS 工厂预偏置（nominal=10230000 Hz,frac=-44647/10¹⁴）。
    /// set_hz 精确值 = 10229999.9954326119 Hz（即公开数值 10.22999999543 MHz）;
    /// 地面漂移 = -38575.008 ns/天（设偏钟在地面走慢 38.575 μs/天）;
    /// 在轨补偿 = +38575.008 ns/天（相对论净快量,GPS 教科书值 +38.5 μs/天）。
    #[test]
    fn factory_prebias_gps_golden() {
        let out = FactoryPrebiasService
            .execute(&args(&[
                ("nominal_hz", JsonValue::string("10230000")),
                ("frac_offset", JsonValue::string("-44647/100000000000000")),
                ("solve", JsonValue::string("set_hz")),
                ("scale", JsonValue::Integer(9)),
                ("rounding", rounding()),
            ]))
            .expect("GPS 黄金向量应成功");
        assert_eq!(
            field(&out, "value_exact"),
            "102299999954326119/10000000000",
            "set_hz 精确分数"
        );
        assert_eq!(
            dec_of(&out, "value"),
            "10229999.995432612",
            "set_hz 九位小数(公开值 10.22999999543 MHz)"
        );
        assert_eq!(
            field(&out, "ground_drift_ns_per_day_exact"),
            "-4821876/125",
            "地面漂移精确分数(-38575.008 ns/天)"
        );
        assert_eq!(
            dec_of(&out, "ground_drift_ns_per_day"),
            "-38575.008000000",
            "与 value 同一 scale(9)"
        );
        assert_eq!(
            dec_of(&out, "orbit_compensation_ns_per_day"),
            "38575.008000000"
        );
        assert_eq!(
            field(&out, "orbit_compensation_ns_per_day_exact"),
            "4821876/125"
        );
    }

    #[test]
    fn factory_prebias_reverse_and_consistency() {
        // 反解 frac_offset：nominal+set → frac。
        let out = FactoryPrebiasService
            .execute(&args(&[
                ("nominal_hz", JsonValue::string("10230000")),
                (
                    "set_hz",
                    JsonValue::string("102299999954326119/10000000000"),
                ),
                ("solve", JsonValue::string("frac_offset")),
                ("scale", JsonValue::Integer(12)),
                ("rounding", rounding()),
            ]))
            .expect("反解应成功");
        assert_eq!(
            field(&out, "frac_offset_exact"),
            "-44647/100000000000000",
            "反解分数偏置应还原 GPS 预偏值"
        );
        // 三量全给一致 → 成功。
        let out = FactoryPrebiasService
            .execute(&args(&[
                ("nominal_hz", JsonValue::string("10230000")),
                (
                    "set_hz",
                    JsonValue::string("102299999954326119/10000000000"),
                ),
                ("frac_offset", JsonValue::string("-44647/100000000000000")),
                ("solve", JsonValue::string("set_hz")),
                ("scale", JsonValue::Integer(6)),
                ("rounding", rounding()),
            ]))
            .expect("三量一致应成功");
        assert_eq!(field(&out, "status"), "determined");
        // 三量全给不一致 → fail-visible。
        let err = FactoryPrebiasService
            .execute(&args(&[
                ("nominal_hz", JsonValue::string("10230000")),
                ("set_hz", JsonValue::string("10230000")),
                ("frac_offset", JsonValue::string("-44647/100000000000000")),
                ("solve", JsonValue::string("set_hz")),
                ("scale", JsonValue::Integer(6)),
                ("rounding", rounding()),
            ]))
            .expect_err("不一致应报错");
        assert!(err.contains("一致性校验失败"), "报错应点名一致性: {err}");
        // 已知量不足 → fail-visible。
        let err = FactoryPrebiasService
            .execute(&args(&[
                ("nominal_hz", JsonValue::string("10230000")),
                ("solve", JsonValue::string("set_hz")),
                ("scale", JsonValue::Integer(6)),
                ("rounding", rounding()),
            ]))
            .expect_err("单量应报错");
        assert!(err.contains("已知量不足"), "报错应点名不足: {err}");
    }

    /// 黄金向量：GPS 每日漂移 → 距离（"不做修正每天漂约 11.5 km"）。
    /// 38575 ns × c = 11564.49406735 m ≈ 11.56 km/天。
    #[test]
    fn range_error_gps_golden() {
        let out = RangeErrorService
            .execute(&args(&[
                ("mode", JsonValue::string("time_to_distance")),
                ("time_seconds", JsonValue::string("38575/1000000000")),
                ("scale", JsonValue::Integer(6)),
                ("rounding", rounding()),
            ]))
            .expect("黄金向量应成功");
        assert_eq!(
            field(&out, "distance_m_exact"),
            "231289881347/20000000",
            "精确分数(既约)"
        );
        assert_eq!(dec_of(&out, "distance_m"), "11564.494067");
        assert_eq!(
            dec_of(&out, "distance_km"),
            "11.564494",
            "与 distance_m 同一 scale(6)"
        );
        assert_eq!(field(&out, "status"), "determined");
        assert_eq!(field(&out, "determinism"), "bitwise_replayable");
    }

    #[test]
    fn range_error_reverse_and_edges() {
        // 反向：c 米 → 恰 1 秒。
        let out = RangeErrorService
            .execute(&args(&[
                ("mode", JsonValue::string("distance_to_time")),
                ("distance_m", JsonValue::string("299792458")),
                ("scale", JsonValue::Integer(3)),
                ("rounding", rounding()),
            ]))
            .expect("反向换算应成功");
        assert_eq!(field(&out, "time_seconds_exact"), "1");
        assert_eq!(dec_of(&out, "time_seconds"), "1.000");
        assert_eq!(dec_of(&out, "time_ns"), "1000000000.000");
        // 负时间 → fail-visible。
        let err = RangeErrorService
            .execute(&args(&[
                ("mode", JsonValue::string("time_to_distance")),
                ("time_seconds", JsonValue::string("-1/1000")),
                ("scale", JsonValue::Integer(3)),
                ("rounding", rounding()),
            ]))
            .expect_err("负时间应报错");
        assert!(err.contains("非负"), "报错应点名非负: {err}");
        // 缺必填量 → fail-visible。
        let err = RangeErrorService
            .execute(&args(&[
                ("mode", JsonValue::string("time_to_distance")),
                ("scale", JsonValue::Integer(3)),
                ("rounding", rounding()),
            ]))
            .expect_err("缺 time_seconds 应报错");
        assert!(err.contains("缺少参数"), "报错应点名缺参: {err}");
    }

    /// 黄金向量：偏心率周期修正 — e=0.01、a=26560000、E≈π/2（sin≈1）
    /// → dt_r = F·e·√a ≈ −22.9 ns（|e·√a| 峰值量级的经典 ±23 ns）。
    #[test]
    fn eccentricity_golden() {
        let out = EccentricityCorrectionService
            .execute(&args(&[
                ("e", JsonValue::string("0.01")),
                ("a", JsonValue::string("26560000")),
                ("E", JsonValue::string("1.5707963267948966192311")),
                ("scale", JsonValue::Integer(0)),
                ("rounding", rounding()),
            ]))
            .expect("偏心率黄金向量应成功");
        assert_eq!(dec_of(&out, "dt_r_ns"), "-23", "峰值量级 ±23 ns");
        // |E| 超限 → undetermined。
        let err = EccentricityCorrectionService
            .execute(&args(&[
                ("e", JsonValue::string("0.01")),
                ("a", JsonValue::string("26560000")),
                ("E", JsonValue::string("20000")),
                ("scale", JsonValue::Integer(0)),
                ("rounding", rounding()),
            ]))
            .expect_err("大幅角应 undetermined");
        assert!(err.contains("undetermined"), "报错应含 undetermined: {err}");
        // e=1 非束缚轨道 fail-visible。
        let err = EccentricityCorrectionService
            .execute(&args(&[
                ("e", JsonValue::string("1")),
                ("a", JsonValue::string("26560000")),
                ("E", JsonValue::string("0")),
                ("scale", JsonValue::Integer(0)),
                ("rounding", rounding()),
            ]))
            .expect_err("e=1 应报错");
        assert!(err.contains("[0,1)"), "报错应点名偏心率约束: {err}");
    }

    /// 黄金向量：GPS 标称半长轴 a=26560000 → GR +45.7 / SR −7.2 / 净 +38.4 μs/天
    /// （教科书 45.7/−7.2/38.5 分项，净随输入半径取值略有差异）。
    #[test]
    fn drift_golden() {
        let out = RelativisticDriftService
            .execute(&args(&[
                ("a", JsonValue::string("26560000")),
                ("scale", JsonValue::Integer(1)),
                ("rounding", rounding()),
            ]))
            .expect("漂移分项黄金向量应成功");
        assert_eq!(dec_of(&out, "gr_us_per_day"), "45.7", "GR 分项");
        assert_eq!(
            dec_of(&out, "sr_us_per_day"),
            "-7.2",
            "SR 分项(负=星钟变慢)"
        );
        assert_eq!(dec_of(&out, "net_us_per_day"), "38.4", "净漂移");
        assert_eq!(field(&out, "status"), "determined");
    }

    /// 黄金向量：af0=1 ms、dt=0（多项式 1 ms）+ 周期项 −23 ns
    /// → 合成 ≈ 999977 ns；半周翻转 t=605000 → dt=200。
    #[test]
    fn clock_total_golden() {
        let out = ClockTotalService
            .execute(&args(&[
                ("af0", JsonValue::string("1/1000")),
                ("af1", JsonValue::string("0")),
                ("af2", JsonValue::string("0")),
                ("t_oc", JsonValue::string("0")),
                ("t", JsonValue::string("0")),
                ("e", JsonValue::string("0.01")),
                ("a", JsonValue::string("26560000")),
                ("E", JsonValue::string("1.5707963267948966192311")),
                ("scale", JsonValue::Integer(0)),
                ("rounding", rounding()),
            ]))
            .expect("总修正黄金向量应成功");
        assert_eq!(dec_of(&out, "dt"), "0", "dt=0 无翻转");
        assert_eq!(dec_of(&out, "dt_af"), "0", "af0=1ms 在 scale 0 显示为 0");
        assert_eq!(dec_of(&out, "dt_r_ns"), "-23", "周期项 −23 ns");
        assert_eq!(dec_of(&out, "dt_total_ns"), "999977", "合成 = 10⁶ − 23 ns");
        // 半周翻转：t=605000 → dt = 605000 − 604800 = 200。
        let out = ClockTotalService
            .execute(&args(&[
                ("af0", JsonValue::string("0")),
                ("af1", JsonValue::string("1/1000000")),
                ("af2", JsonValue::string("0")),
                ("t_oc", JsonValue::string("0")),
                ("t", JsonValue::string("605000")),
                ("e", JsonValue::string("0")),
                ("a", JsonValue::string("26560000")),
                ("E", JsonValue::string("0")),
                ("scale", JsonValue::Integer(0)),
                ("rounding", rounding()),
            ]))
            .expect("翻转用例应成功");
        assert_eq!(dec_of(&out, "dt"), "200", "翻转后 dt=200(非 605000)");
        assert_eq!(
            dec_of(&out, "dt_af"),
            "0",
            "af1·200 = 2×10⁻⁴ s,scale 0 为 0"
        );
    }
}

//! 原生服务实现：广义相对论解析三服务（JSON 入 / JSON 出，统一信封 v1）。
//!
//! # 确定性
//! 全程有理数/定点表示，无浮点、无时钟、无随机；舍入只发生在输出端且
//! `scale`/`rounding` 必填（杜绝隐式舍入）。同输入两次执行结果逐位一致。
//!
//! # 常数分层
//! - `c = 299792458 m/s`：SI 定义精确值，内置并在出参 `constants` 带版本戳；
//! - `G`：测量常数，一律显式入参（零内置）；
//! - `π`：数学常数，以 50 位截断有理近似内置（pi-50digit 版本戳）。
//!   进动件的**弧秒输出中 π 与弧度换算恰好相消**（弧秒式
//!   Delta_phi_arcsec = 3888000·G·M/(c²·a·(1−e²))），为纯精确有理数；
//!   弧度输出与光偏折件角秒输出中的 π 为声明精度近似，相对误差 < 10⁻⁴⁹。
//!
//! # 诚实边界（逐服务）
//! - `grav_schwarzschild_radius`：质量非负、G 为正，否则 fail-visible；
//! - `grav_perihelion_precession`：束缚轨道要求偏心率 ∈ [0,1)（e→1 进动
//!   发散，e≥1 非束缚轨道 fail-visible）；半长轴为正；orbits ≥ 1；
//! - `grav_light_deflection`：瞄准参数为正（b→0 偏折发散）；
//! - 本族为解析式求解器——强场/数值相对论仿真不在范围，如实留白。

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

/// 真空中光速 c（SI 2019 定义精确值，内置带版本戳，零浮点）。
const C_M_PER_S: i64 = 299_792_458;

/// π 的 50 位截断近似（pi-50digit-truncation，相对误差 < 10⁻⁴⁹），
/// 以 10 位数字组给出：3.1415926535 8979323846 2643383279 5028841971 6939937510。
const PI50_GROUPS: [u64; 6] = [
    3,
    1_415_926_535,
    8_979_323_846,
    2_643_383_279,
    5_028_841_971,
    6_939_937_510,
];

/// π 近似分子（整数值 = π × 10⁵⁰，十进制意义下）。
fn pi50_num() -> BigInt {
    let mut v = BigInt::zero();
    for g in PI50_GROUPS {
        v = v * 10_000_000_000u64 + g;
    }
    v
}

/// π 近似：pi50_num / 10^50。
fn pi50() -> Q {
    Q::new(pi50_num(), BigInt::from(10u32).pow(50))
}

/// 每弧度角秒数 = 648000/π（同 pi50 近似精度）。
fn arcsec_per_rad() -> Q {
    Q::new(
        BigInt::from(648_000u32) * BigInt::from(10u32).pow(50),
        pi50_num(),
    )
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

/// 顶层必填数值参数（字符串编码十进制字面量或 a/b 分数）。
fn req_rational(args: &JsonValue, key: &str) -> Result<Q, String> {
    match args.get(key) {
        Some(v) => v
            .as_str()
            .map(parse_value_as_rational)
            .unwrap_or_else(|| Err(format!("参数 {key} 须为字符串编码数值"))),
        None => Err(format!(
            "缺少参数 {key} — 自诊断指引: 见服务 parameters 契约"
        )),
    }
}

/// 公共物理量校验（fail-visible）。
fn validate_mass(m: &Q) -> Result<(), String> {
    if *m < int_const(0) {
        return Err("参数 m 须为非负 — 自诊断指引: 质量为非负物理量".to_string());
    }
    Ok(())
}

fn validate_g(g: &Q) -> Result<(), String> {
    if g <= &int_const(0) || g.is_zero() {
        return Err(
            "参数 g 须为正 — 自诊断指引: 万有引力常数为正测量常数,显式入参(如 66743/1000000000000000)"
                .to_string(),
        );
    }
    Ok(())
}

fn constants_obj(with_pi: bool) -> JsonValue {
    let c = JsonValue::object_from_pairs(&[
        ("c_m_per_s", JsonValue::string("299792458")),
        ("source", JsonValue::string("SI 2019 definition (exact)")),
    ]);
    if with_pi {
        let pi = JsonValue::object_from_pairs(&[
            ("approximation", JsonValue::string("50-digit truncation")),
            ("relative_error_below", JsonValue::string("1e-49")),
            ("version", JsonValue::string("pi-50digit-truncation")),
        ]);
        JsonValue::object_from_pairs(&[("c", c), ("pi", pi)])
    } else {
        JsonValue::object_from_pairs(&[("c", c)])
    }
}

// ============================================================================
// grav_schwarzschild_radius — 史瓦西半径
// ============================================================================

struct SchwarzschildService;

impl NativeService for SchwarzschildService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let m = req_rational(args, "m")?;
        let g = req_rational(args, "g")?;
        validate_mass(&m)?;
        validate_g(&g)?;
        let scale = req_scale(args)?;
        let rounding = req_rounding(args)?;
        let c = int_const(C_M_PER_S);
        let rs = int_const(2) * g * m / (c.clone() * c);
        let out = vec![
            ("value", dec_obj(&rs, scale, rounding)?),
            ("value_exact", q_str(&rs)),
            ("constants", constants_obj(false)),
            (
                "note",
                JsonValue::string("rs = 2GM/c^2 — 事件视界半径;G 显式入参,c 为 SI 精确内置"),
            ),
        ];
        Ok(envelope(&out))
    }
}

pub fn mk_schwarzschild_radius() -> Arc<dyn NativeService> {
    Arc::new(SchwarzschildService)
}

// ============================================================================
// grav_perihelion_precession — 近日点进动
// ============================================================================

struct PerihelionPrecessionService;

impl NativeService for PerihelionPrecessionService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let m = req_rational(args, "m")?;
        let a = req_rational(args, "a")?;
        let e = req_rational(args, "e")?;
        let g = req_rational(args, "g")?;
        validate_mass(&m)?;
        validate_g(&g)?;
        if a <= int_const(0) {
            return Err("参数 a 须为正 — 自诊断指引: 轨道半长轴为正物理量(束缚轨道)".to_string());
        }
        let one = int_const(1);
        if e < int_const(0) || e >= one {
            return Err(format!(
                "参数 e={e} 须 ∈ [0,1) — 自诊断指引: 束缚轨道偏心率小于 1(e→1 进动发散;e≥1 为非束缚轨道,本服务如实拒解)"
            ));
        }
        let orbits = match args.get("orbits") {
            Some(JsonValue::Integer(n)) if *n >= 1 => *n,
            Some(JsonValue::Integer(n)) => {
                return Err(format!(
                    "参数 orbits={n} 须 ≥ 1 — 自诊断指引: 轨道圈数为正整数"
                ))
            }
            Some(_) => return Err("参数 orbits 须为整数".to_string()),
            None => 1,
        };
        let scale = req_scale(args)?;
        let rounding = req_rounding(args)?;
        let c = int_const(C_M_PER_S);
        // 弧秒式：Delta_phi_arcsec = 3888000·G·M/(c²·a·(1−e²))——
        // 由 Delta_phi = 6πGM/(c²a(1−e²)) (rad) × 648000/π (rad→arcsec) 化简,
        // π 与换算恰好相消 ⇒ 弧秒输出为纯精确有理数(零近似)。
        let arcsec_per_orbit =
            int_const(3_888_000) * g * m / ((c.clone() * c) * a * (one - e.clone() * e));
        let arcsec_total = arcsec_per_orbit.clone() * int_const(orbits);
        // 弧度 = 弧秒 × π/648000(π 为 50 位近似,版本戳见 constants)。
        let rad_total = arcsec_total.clone() * pi50() / int_const(648_000);
        let out = vec![
            (
                "arcsec_per_orbit",
                dec_obj(&arcsec_per_orbit, scale, rounding)?,
            ),
            ("arcsec_per_orbit_exact", q_str(&arcsec_per_orbit)),
            ("arcsec_total", dec_obj(&arcsec_total, scale, rounding)?),
            ("arcsec_total_exact", q_str(&arcsec_total)),
            ("rad_total", dec_obj(&rad_total, scale, rounding)?),
            ("orbits", JsonValue::Integer(orbits)),
            ("constants", constants_obj(true)),
            (
                "note",
                JsonValue::string(
                    "弧秒输出精确(pi 与换算相消);rad_total 中 pi 为 50 位有理近似; \
                     水星参考: a=5.7909e10 m, e=0.2056, 一世纪约 415 圈 → 约 43 角秒/世纪",
                ),
            ),
        ];
        Ok(envelope(&out))
    }
}

pub fn mk_perihelion_precession() -> Arc<dyn NativeService> {
    Arc::new(PerihelionPrecessionService)
}

// ============================================================================
// grav_light_deflection — 光线偏折
// ============================================================================

struct LightDeflectionService;

impl NativeService for LightDeflectionService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let m = req_rational(args, "m")?;
        let b = req_rational(args, "b")?;
        let g = req_rational(args, "g")?;
        validate_mass(&m)?;
        validate_g(&g)?;
        if b <= int_const(0) {
            return Err(
                "参数 b 须为正 — 自诊断指引: 瞄准参数(最近距离)为正物理量,b→0 偏折发散".to_string(),
            );
        }
        let scale = req_scale(args)?;
        let rounding = req_rounding(args)?;
        let c = int_const(C_M_PER_S);
        // 弧度本体精确：alpha = 4GM/(c²·b)。
        let rad = int_const(4) * g * m / ((c.clone() * c) * b);
        // 角秒 = 弧度 × 648000/π(π 为 50 位近似,版本戳见 constants)。
        let arcsec = rad.clone() * arcsec_per_rad();
        let out = vec![
            ("rad", dec_obj(&rad, scale, rounding)?),
            ("rad_exact", q_str(&rad)),
            ("arcsec", dec_obj(&arcsec, scale, rounding)?),
            ("arcsec_exact", q_str(&arcsec)),
            ("constants", constants_obj(true)),
            (
                "note",
                JsonValue::string(
                    "rad 精确;arcsec 中 pi 为 50 位有理近似;太阳掠射参考: b=6.957e8 m(光球半径) → 约 1.75 角秒(1919 年日食经典验证)",
                ),
            ),
        ];
        Ok(envelope(&out))
    }
}

pub fn mk_light_deflection() -> Arc<dyn NativeService> {
    Arc::new(LightDeflectionService)
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

    /// 太阳参数黄金输入：G = 6.6743×10⁻¹¹（CODATA），M☉ = 1.98847×10³⁰ kg
    /// （198847 后跟 25 个零，共 31 位）。
    const G_SUN: &str = "66743/1000000000000000";
    const M_SUN: &str = "1988470000000000000000000000000";

    /// 黄金向量：太阳史瓦西半径 rs = 2GM☉/c² ≈ 2953.21 m（约 2.95 km）。
    /// 精确分数 = 66358226605000000000/22468879468420441（既约）。
    #[test]
    fn schwarzschild_sun_golden() {
        let out = SchwarzschildService
            .execute(&args(&[
                ("m", JsonValue::string(M_SUN)),
                ("g", JsonValue::string(G_SUN)),
                ("scale", JsonValue::Integer(2)),
                ("rounding", rounding()),
            ]))
            .expect("太阳黄金向量应成功");
        assert_eq!(
            dec_of(&out, "value"),
            "2953.34",
            "太阳史瓦西半径(米,约 2.95 km)"
        );
        assert_eq!(
            field(&out, "value_exact"),
            "66358226605000000000/22468879468420441",
            "精确分数(既约)"
        );
        assert_eq!(field(&out, "status"), "determined");
        assert_eq!(field(&out, "determinism"), "bitwise_replayable");
        // 负质量 fail-visible。
        let err = SchwarzschildService
            .execute(&args(&[
                ("m", JsonValue::string("-1")),
                ("g", JsonValue::string(G_SUN)),
                ("scale", JsonValue::Integer(2)),
                ("rounding", rounding()),
            ]))
            .expect_err("负质量应报错");
        assert!(err.contains("非负"), "报错应点名非负: {err}");
    }

    /// 黄金向量：水星近日点进动——每圈约 0.1035 角秒；一世纪约 415 圈
    /// → 约 42.96 角秒 ≈ 经典 43 角秒/世纪。
    #[test]
    fn perihelion_mercury_golden() {
        let base = [
            ("m", JsonValue::string(M_SUN)),
            ("a", JsonValue::string("57909000000")),
            ("e", JsonValue::string("0.2056")),
            ("g", JsonValue::string(G_SUN)),
        ];
        // 单圈（orbits 缺省 = 1）。
        let out = PerihelionPrecessionService
            .execute(&args(&[
                base[0].clone(),
                base[1].clone(),
                base[2].clone(),
                base[3].clone(),
                ("scale", JsonValue::Integer(4)),
                ("rounding", rounding()),
            ]))
            .expect("水星单圈应成功");
        assert_eq!(
            dec_of(&out, "arcsec_per_orbit"),
            "0.1035",
            "水星每圈进动(角秒)"
        );
        assert_eq!(
            dec_of(&out, "arcsec_total"),
            dec_of(&out, "arcsec_per_orbit"),
            "orbits 缺省时 total 与单圈一致"
        );
        // 一世纪 415 圈 → 约 43 角秒。
        let out = PerihelionPrecessionService
            .execute(&args(&[
                base[0].clone(),
                base[1].clone(),
                base[2].clone(),
                base[3].clone(),
                ("orbits", JsonValue::Integer(415)),
                ("scale", JsonValue::Integer(2)),
                ("rounding", rounding()),
            ]))
            .expect("水星世纪应成功");
        assert_eq!(dec_of(&out, "arcsec_total"), "42.96", "经典 43 角秒/世纪");
        // e=1 非束缚轨道 fail-visible。
        let err = PerihelionPrecessionService
            .execute(&args(&[
                ("m", JsonValue::string(M_SUN)),
                ("a", JsonValue::string("57909000000")),
                ("e", JsonValue::string("1")),
                ("g", JsonValue::string(G_SUN)),
                ("scale", JsonValue::Integer(2)),
                ("rounding", rounding()),
            ]))
            .expect_err("e=1 应报错");
        assert!(err.contains("[0,1)"), "报错应点名偏心率约束: {err}");
    }

    /// 黄金向量：太阳掠射光线偏折 b = 6.957×10⁸ m（IAU 光球半径）
    /// → alpha ≈ 8.4903×10⁻⁶ rad ≈ 1.75 角秒（1919 年日食经典验证）。
    #[test]
    fn light_deflection_sun_golden() {
        let out = LightDeflectionService
            .execute(&args(&[
                ("m", JsonValue::string(M_SUN)),
                ("b", JsonValue::string("695700000")),
                ("g", JsonValue::string(G_SUN)),
                ("scale", JsonValue::Integer(8)),
                ("rounding", rounding()),
            ]))
            .expect("太阳掠射黄金向量应成功");
        assert_eq!(dec_of(&out, "rad"), "0.00000849", "偏折角(弧度)");
        assert_eq!(
            dec_of(&out, "arcsec"),
            "1.75124328",
            "偏折角(角秒,与 rad 同 scale(8);经典 1.75)"
        );
        assert_eq!(field(&out, "status"), "determined");
        // b=0 发散 fail-visible。
        let err = LightDeflectionService
            .execute(&args(&[
                ("m", JsonValue::string(M_SUN)),
                ("b", JsonValue::string("0")),
                ("g", JsonValue::string(G_SUN)),
                ("scale", JsonValue::Integer(2)),
                ("rounding", rounding()),
            ]))
            .expect_err("b=0 应报错");
        assert!(err.contains("须为正"), "报错应点名正值约束: {err}");
    }
}

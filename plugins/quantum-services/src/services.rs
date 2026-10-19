//! 原生服务实现：量子力学解析两服务（JSON 入 / JSON 出，统一信封 v1）。
//!
//! # 确定性
//! 全程有理数运算，无浮点、无时钟、无随机；舍入只发生在输出端且
//! `scale`/`rounding` 必填。同输入两次执行结果逐位一致。
//!
//! # 常数分层
//! - `h`：SI 2019 定义精确值，内置带版本戳；
//! - `ℏ = h/(2π)`：numeric 派生原语（π 用 50 位截断，版本戳在出参）；
//! - `eV`：SI 2019 定义精确值（1.602176634×10⁻¹⁹ J），内置带版本戳；
//! - `m`/`L`/`ω`：显式入参，零内置。
//!
//! # 公式与 π 相消
//! 势阱能级 Eₙ = n²π²ℏ²/(2mL²) 以 ℏ=h/2π 代入后 π 恰好相消，
//! 等价形 Eₙ = n²h²/(8mL²) 为**纯精确有理数**（零近似）；
//! 谐振子 Eₙ=(n+½)ℏω 中的 ℏ 携带 π 的声明精度（相对误差 <10⁻⁴⁹）。

#![forbid(unsafe_code)]

use std::sync::Arc;

use evorule_numeric_services::numcore::{
    decimal_string, parse_rounding, planck_h, planck_reduced, round_to_scale, Rounding,
};
use evorule_numeric_services::services::parse_value_as_rational;
use evorule_plugin_kit::NativeService;
use evorule_reactor::IoResult;
use evorule_tcb::JsonValue;
use num_bigint::BigInt;
use num_rational::BigRational;

pub type Q = BigRational;

/// 电子伏特（SI 2019 定义精确值：1.602176634×10⁻¹⁹ J = 1602176634/10²⁸）。
fn ev_j() -> Q {
    Q::new(BigInt::from(1_602_176_634i64), {
        // 10²⁸ —— BigInt 平方根以下直接构造,避免引入 pow10 之外的依赖。
        let mut d = BigInt::from(1i64);
        for _ in 0..28 {
            d *= 10;
        }
        d
    })
}

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

// ============================================================================
// quantum_infinite_well — 无限深势阱能级
// ============================================================================

struct InfiniteWellService;

impl NativeService for InfiniteWellService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let n = req_i64(args, "n")?;
        if n < 1 {
            return Err(format!(
                "参数 n={n} 须为正整数 — 自诊断指引: 势阱量子数从 1 起"
            ));
        }
        let m = req_rational(args, "m")?;
        let l = req_rational(args, "l")?;
        if m <= int_const(0) {
            return Err("参数 m 须为正 — 自诊断指引: 粒子质量为正物理量".to_string());
        }
        if l <= int_const(0) {
            return Err("参数 l 须为正 — 自诊断指引: 阱宽为正物理量".to_string());
        }
        let scale = req_scale(args)?;
        let rounding = req_rounding(args)?;
        // Eₙ = n²π²ℏ²/(2mL²) 以 ℏ=h/2π 代入 → Eₙ = n²h²/(8mL²),π 相消=纯精确。
        let h = planck_h();
        let energy =
            int_const(n) * int_const(n) * h.clone() * h / (int_const(8) * m * l.clone() * l);
        let energy_ev = energy.clone() / ev_j();
        let out = vec![
            ("n", JsonValue::Integer(n)),
            ("E_j", dec_obj(&energy, scale, rounding)?),
            ("E_j_exact", q_str(&energy)),
            ("E_ev", dec_obj(&energy_ev, scale, rounding)?),
            ("E_ev_exact", q_str(&energy_ev)),
            (
                "constants",
                JsonValue::object_from_pairs(&[
                    (
                        "h_J_s",
                        JsonValue::string("6.62607015e-34 (SI 2019 exact)"),
                    ),
                    (
                        "eV_J",
                        JsonValue::string("1.602176634e-19 (SI 2019 exact)"),
                    ),
                ]),
            ),
            (
                "note",
                JsonValue::string(
                    "Eₙ = n²h²/(8mL²)(ℏ=h/2π 代入后 π 相消=纯精确);参考: 电子 L=1nm → E₁ ≈ 0.376 eV",
                ),
            ),
        ];
        Ok(envelope(&out))
    }
}

pub fn mk_infinite_well() -> Arc<dyn NativeService> {
    Arc::new(InfiniteWellService)
}

// ============================================================================
// quantum_harmonic_oscillator — 量子谐振子能级
// ============================================================================

struct HarmonicOscillatorService;

impl NativeService for HarmonicOscillatorService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let n = req_i64(args, "n")?;
        if n < 0 {
            return Err(format!(
                "参数 n={n} 须为非负整数 — 自诊断指引: 谐振子量子数从 0 起(零点能 n=0)"
            ));
        }
        let omega = req_rational(args, "omega")?;
        if omega <= int_const(0) {
            return Err("参数 omega 须为正 — 自诊断指引: 角频率为正物理量".to_string());
        }
        let scale = req_scale(args)?;
        let rounding = req_rounding(args)?;
        // Eₙ = (n+½)ℏω;ℏ = h/(2π) 携带 π 的 50 位声明精度(版本戳在出参)。
        let energy = (int_const(2 * n) + int_const(1)) / int_const(2) * planck_reduced() * omega;
        let energy_ev = energy.clone() / ev_j();
        let out = vec![
            ("n", JsonValue::Integer(n)),
            ("E_j", dec_obj(&energy, scale, rounding)?),
            ("E_j_exact", q_str(&energy)),
            ("E_ev", dec_obj(&energy_ev, scale, rounding)?),
            ("E_ev_exact", q_str(&energy_ev)),
            (
                "constants",
                JsonValue::object_from_pairs(&[
                    ("h_J_s", JsonValue::string("6.62607015e-34 (SI 2019 exact)")),
                    (
                        "hbar",
                        JsonValue::string("h/(2*pi), pi 50-digit truncation"),
                    ),
                    ("eV_J", JsonValue::string("1.602176634e-19 (SI 2019 exact)")),
                ]),
            ),
            (
                "note",
                JsonValue::string("Eₙ = (n+½)ℏω;ℏ 相对误差 <10⁻⁴⁹;零点能 n=0 = ½ℏω"),
            ),
        ];
        Ok(envelope(&out))
    }
}

pub fn mk_harmonic_oscillator() -> Arc<dyn NativeService> {
    Arc::new(HarmonicOscillatorService)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn args(pairs: &[(&str, JsonValue)]) -> JsonValue {
        JsonValue::object_from_pairs(pairs)
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

    /// 电子质量（CODATA 2018：9.1093837015×10⁻³¹ kg，测量常数显式入参）。
    const M_E: &str = "91093837015/100000000000000000000000000000000000000000";

    /// 黄金向量：1 nm 阱中电子 E₁ = h²/(8mL²) ≈ 0.376 eV；
    /// E₂ = 4×E₁ ≈ 1.504 eV（能级平方律）；n=0 fail-visible。
    #[test]
    fn infinite_well_golden() {
        let out = InfiniteWellService
            .execute(&args(&[
                ("n", JsonValue::Integer(1)),
                ("m", JsonValue::string(M_E)),
                ("l", JsonValue::string("1/1000000000")),
                ("scale", JsonValue::Integer(3)),
                ("rounding", rounding()),
            ]))
            .expect("势阱黄金向量应成功");
        assert_eq!(dec_of(&out, "E_ev"), "0.376", "E₁(eV,经典 0.376)");
        let out = InfiniteWellService
            .execute(&args(&[
                ("n", JsonValue::Integer(2)),
                ("m", JsonValue::string(M_E)),
                ("l", JsonValue::string("1/1000000000")),
                ("scale", JsonValue::Integer(3)),
                ("rounding", rounding()),
            ]))
            .expect("n=2 应成功");
        assert_eq!(dec_of(&out, "E_ev"), "1.504", "E₂=4×E₁(平方律)");
        let err = InfiniteWellService
            .execute(&args(&[
                ("n", JsonValue::Integer(0)),
                ("m", JsonValue::string(M_E)),
                ("l", JsonValue::string("1/1000000000")),
                ("scale", JsonValue::Integer(3)),
                ("rounding", rounding()),
            ]))
            .expect_err("n=0 应报错");
        assert!(err.contains("正整数"), "报错应点名量子数: {err}");
    }

    /// 黄金向量：ω=10¹⁵ rad/s — 零点能 E₀=½ℏω ≈ 0.329 eV；
    /// E₅=5.5ℏω ≈ 3.620 eV；负 n fail-visible。
    #[test]
    fn harmonic_golden() {
        let out = HarmonicOscillatorService
            .execute(&args(&[
                ("n", JsonValue::Integer(0)),
                ("omega", JsonValue::string("1000000000000000")),
                ("scale", JsonValue::Integer(3)),
                ("rounding", rounding()),
            ]))
            .expect("零点能黄金向量应成功");
        assert_eq!(dec_of(&out, "E_ev"), "0.329", "零点能 ½ℏω(eV)");
        let out = HarmonicOscillatorService
            .execute(&args(&[
                ("n", JsonValue::Integer(5)),
                ("omega", JsonValue::string("1000000000000000")),
                ("scale", JsonValue::Integer(3)),
                ("rounding", rounding()),
            ]))
            .expect("n=5 应成功");
        assert_eq!(dec_of(&out, "E_ev"), "3.620", "E₅=5.5ℏω(eV)");
        let err = HarmonicOscillatorService
            .execute(&args(&[
                ("n", JsonValue::Integer(-1)),
                ("omega", JsonValue::string("1000000000000000")),
                ("scale", JsonValue::Integer(3)),
                ("rounding", rounding()),
            ]))
            .expect_err("n=-1 应报错");
        assert!(err.contains("非负整数"), "报错应点名量子数: {err}");
    }
}

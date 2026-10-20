//! 原生服务实现：精确算术八服务（JSON 入 / JSON 出，统一信封 v1）。
//!
//! # 确定性
//! 全程整数/有理数/定点表示，无浮点、无时钟、无随机；舍入只发生在
//! 输出端且 `scale`/`rounding` 必填（缺省即 Err——杜绝隐式舍入）。
//! 同输入两次执行结果逐位一致。
//!
//! # 诚实边界（逐服务）
//! - `numeric_rational_eval`：分数指数不支持（Err，非近似）；
//! - `numeric_decimal_eval`：表达式全程精确有理运算,仅在输出端单次舍入
//!   （无中间舍入——与传统逐步舍入计算器的语义差异如实声明）；
//! - `numeric_decimal_convert`：无限小数如实标注 `terminating=false`；
//! - `numeric_fin_interest`：total 与 interest 各自独立舍入，二者之和
//!   与本金可能相差最小位一单位（显式语义，非缺陷）；
//! - `numeric_fin_amortize`：账面本金先按 scale 舍入；尾差并入末期
//!   （末期 principal = 剩余余额，末期 payment = interest + 该本金）；
//! - `numeric_unit_convert`：换算率恒由调用方提供，服务不内置任何率；
//! - `numeric_dim_eval`：量纲原语复用 inference 家族（单源不漂移）。

#![forbid(unsafe_code)]

use std::sync::Arc;

use evorule_plugin_kit::NativeService;
use evorule_reactor::IoResult;
use evorule_tcb::JsonValue;
use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::{One, Zero};

use crate::numcore::{
    decimal_string, eval_expr, parse_decimal, parse_rounding, round_to_scale, ExprDialect,
    Rounding, MAX_SCALE,
};
use crate::numtheory;

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
    i32::try_from(s).map_err(|_| format!("参数 scale={s} 超出 ±{MAX_SCALE} 允许范围"))
}

fn req_rounding(args: &JsonValue) -> Result<Rounding, String> {
    parse_rounding(req_str(args, "rounding")?)
}

pub fn parse_value_as_rational(value: &str) -> Result<BigRational, String> {
    if let Some((a, b)) = value.split_once('/') {
        let num = BigInt::parse_bytes(a.trim().as_bytes(), 10)
            .ok_or_else(|| format!("分子非法: '{}'", a.trim()))?;
        let den_str = b.trim();
        let den = BigInt::parse_bytes(den_str.as_bytes(), 10)
            .ok_or_else(|| format!("分母非法: '{den_str}'"))?;
        if den.is_zero() {
            return Err("分母为零 — 自诊断指引: 分数分母不得为零".to_string());
        }
        Ok(BigRational::new(num, den))
    } else {
        let (unscaled, scale) = parse_decimal(value)?;
        Ok(BigRational::new(
            unscaled,
            crate::numcore::pow10(scale as u32),
        ))
    }
}

fn dec_obj(q: &BigRational, scale: i32, mode: Rounding) -> Result<JsonValue, String> {
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

/// 从 args 提取可选 `vars` 对象（变量名 → 十进制字面量或 a/b 分数），
/// 返回变量查找闭包；缺省 = 空集（表达式内出现变量即报未知变量）。
fn vars_lookup_from_args(
    args: &JsonValue,
) -> Result<impl Fn(&str) -> Option<BigRational> + '_, String> {
    let named: Vec<(String, BigRational)> = match args.get("vars") {
        Some(JsonValue::Object(m)) => {
            let mut out = Vec::new();
            for (k, val) in m.iter() {
                let s = val
                    .as_str()
                    .ok_or_else(|| format!("vars.{k} 须为字符串编码数值"))?;
                let q =
                    parse_value_as_rational(s).map_err(|e| format!("vars.{k} 解析失败: {e}"))?;
                out.push((k.clone(), q));
            }
            out
        }
        None => Vec::new(),
        Some(_) => {
            return Err(
                "参数 vars 须为对象 — 自诊断指引: 形如 {\"a\":\"2\",\"b\":\"3/4\"}".to_string(),
            )
        }
    };
    Ok(move |name: &str| -> Option<BigRational> {
        named
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, q)| q.clone())
    })
}

fn envelope(pairs: &[(&str, JsonValue)]) -> JsonValue {
    let mut all: Vec<(&str, JsonValue)> = vec![
        ("status", JsonValue::string("determined")),
        ("determinism", JsonValue::string("bitwise_replayable")),
    ];
    all.extend_from_slice(pairs);
    JsonValue::object_from_pairs(&all)
}

// ============================================================================
// 服务 1：有理数表达式精确求值
// ============================================================================

pub struct RationalEvalService;

impl NativeService for RationalEvalService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let expr = req_str(args, "expr")?;
        let vars = vars_lookup_from_args(args)?;
        let val = eval_expr(
            expr,
            ExprDialect {
                decimal_literals: false,
                units: false,
                identifiers: true,
            },
            &vars,
            &|_| None,
        )
        .map_err(|e| format!("{e} (expr={expr})"))?;
        let q = val.q;
        let approx = round_to_scale(&q, 12, Rounding::HalfEven)?;
        Ok(envelope(&[
            ("expr", JsonValue::string(expr)),
            ("numerator", JsonValue::string(q.numer().to_string())),
            ("denominator", JsonValue::string(q.denom().to_string())),
            (
                "decimal_approx",
                JsonValue::string(decimal_string(&approx.0, approx.1)),
            ),
            (
                "approx_note",
                JsonValue::string("decimal_approx 为 12 位小数近似,精确值以分数为准"),
            ),
        ]))
    }
}

// ============================================================================
// 服务 2：定点十进制表达式求值
// ============================================================================

pub struct DecimalEvalService;

impl NativeService for DecimalEvalService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let expr = req_str(args, "expr")?;
        let scale = req_scale(args)?;
        let rounding = req_rounding(args)?;
        let vars = vars_lookup_from_args(args)?;
        let val = eval_expr(
            expr,
            ExprDialect {
                decimal_literals: true,
                units: false,
                identifiers: true,
            },
            &vars,
            &|_| None,
        )
        .map_err(|e| format!("{e} (expr={expr})"))?;
        let q = val.q;
        let (unscaled, out_scale) = round_to_scale(&q, scale, rounding)?;
        Ok(envelope(&[
            ("expr", JsonValue::string(expr)),
            ("numerator", JsonValue::string(q.numer().to_string())),
            ("denominator", JsonValue::string(q.denom().to_string())),
            ("unscaled", JsonValue::string(unscaled.to_string())),
            ("scale", JsonValue::Integer(out_scale as i64)),
            (
                "decimal_string",
                JsonValue::string(decimal_string(&unscaled, out_scale)),
            ),
        ]))
    }
}

// ============================================================================
// 服务 3：分数与十进制互转
// ============================================================================

pub struct DecimalConvertService;

impl NativeService for DecimalConvertService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let value = req_str(args, "value")?;
        let target = req_str(args, "target")?;
        let q = parse_value_as_rational(value)?;
        match target {
            "fraction" => {
                let mut pairs = vec![
                    ("value", JsonValue::string(value)),
                    ("terminating", JsonValue::Bool(true)),
                    ("numerator", JsonValue::string(q.numer().to_string())),
                    ("denominator", JsonValue::string(q.denom().to_string())),
                ];
                pairs.truncate(4);
                Ok(envelope(&pairs))
            }
            "decimal" => {
                let scale = req_scale(args)?;
                let rounding = req_rounding(args)?;
                // 终止性判定：既约分母的素因子只含 2 与 5
                let mut d = q.denom().clone();
                let (mut e2, mut e5) = (0i64, 0i64);
                while (&d % 2u8).is_zero() {
                    d /= 2;
                    e2 += 1;
                }
                while (&d % 5u8).is_zero() {
                    d /= 5;
                    e5 += 1;
                }
                if d.is_one() {
                    let exact = e2.max(e5);
                    if exact > MAX_SCALE as i64 {
                        return Err(format!(
                            "精确十进制表示需 {exact} 位小数, 超上限 ±{MAX_SCALE} — \
                             自诊断指引: 请用 target=fraction 取精确形态"
                        ));
                    }
                    let unscaled = q.numer() * crate::numcore::pow10(exact as u32) / q.denom();
                    return Ok(envelope(&[
                        ("value", JsonValue::string(value)),
                        ("terminating", JsonValue::Bool(true)),
                        ("unscaled", JsonValue::string(unscaled.to_string())),
                        ("scale", JsonValue::Integer(exact)),
                        (
                            "decimal_string",
                            JsonValue::string(decimal_string(&unscaled, exact as i32)),
                        ),
                    ]));
                }
                let (unscaled, out_scale) = round_to_scale(&q, scale, rounding)?;
                Ok(envelope(&[
                    ("value", JsonValue::string(value)),
                    ("terminating", JsonValue::Bool(false)),
                    (
                        "note",
                        JsonValue::string("无限小数,输出为按 scale+rounding 的舍入值"),
                    ),
                    ("unscaled", JsonValue::string(unscaled.to_string())),
                    ("scale", JsonValue::Integer(out_scale as i64)),
                    (
                        "decimal_string",
                        JsonValue::string(decimal_string(&unscaled, out_scale)),
                    ),
                ]))
            }
            other => Err(format!(
                "未知 target '{other}' — 自诊断指引: 合法值 fraction/decimal"
            )),
        }
    }
}

// ============================================================================
// 服务 4：数论原子
// ============================================================================

pub struct NumberTheoryService;

impl NativeService for NumberTheoryService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let op = req_str(args, "op")?;
        let argv: Vec<i64> = match args.get("args") {
            Some(JsonValue::Array(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for it in items {
                    match it {
                        JsonValue::Integer(n) => out.push(*n),
                        _ => return Err("参数 args 须为整数数组".to_string()),
                    }
                }
                out
            }
            _ => {
                return Err(
                    "缺少参数 args(整数数组) — 自诊断指引: 如 {\"args\":[50,25]}".to_string(),
                )
            }
        };
        let result = numtheory::evaluate(op, &argv)?;
        Ok(envelope(&[
            ("op", JsonValue::string(op)),
            ("result", JsonValue::string(result.to_string())),
        ]))
    }
}

// ============================================================================
// 服务 5：单利/复利精确计算
// ============================================================================

pub struct FinInterestService;

impl NativeService for FinInterestService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let principal = parse_value_as_rational(req_str(args, "principal")?)?;
        let rate = parse_value_as_rational(req_str(args, "rate")?)?;
        let periods = req_i64(args, "periods")?;
        let mode = req_str(args, "mode")?;
        let scale = req_scale(args)?;
        let rounding = req_rounding(args)?;
        if periods < 0 {
            return Err("参数 periods 须为非负整数".to_string());
        }
        if periods > 1200 {
            return Err(format!(
                "periods={periods} 超上限 1200 — 自诊断指引: 复利幂体积不可控"
            ));
        }
        let one = BigRational::from_integer(1.into());
        let total_exact = match mode {
            "simple" => {
                &principal
                    * (one.clone() + &rate * BigRational::from_integer(BigInt::from(periods)))
            }
            "compound" => {
                let growth = num_traits::pow(one + rate, periods as usize);
                &principal * growth
            }
            other => {
                return Err(format!(
                    "未知 mode '{other}' — 自诊断指引: 合法值 simple/compound"
                ))
            }
        };
        let interest_exact = &total_exact - &principal;
        Ok(envelope(&[
            ("mode", JsonValue::string(mode)),
            ("periods", JsonValue::Integer(periods)),
            ("total", dec_obj(&total_exact, scale, rounding)?),
            ("interest", dec_obj(&interest_exact, scale, rounding)?),
            (
                "rounding_note",
                JsonValue::string("total 与 interest 各自独立舍入,和与本金可能相差最小位一单位"),
            ),
        ]))
    }
}

// ============================================================================
// 服务 6：等额本息/等额本金摊销表
// ============================================================================

pub struct FinAmortizeService;

impl NativeService for FinAmortizeService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let principal = parse_value_as_rational(req_str(args, "principal")?)?;
        let rate = parse_value_as_rational(req_str(args, "rate")?)?;
        let terms = req_i64(args, "terms")?;
        let method = req_str(args, "method")?;
        let scale = req_scale(args)?;
        let rounding = req_rounding(args)?;
        if terms <= 0 {
            return Err("参数 terms 须为正整数".to_string());
        }
        if terms > 3600 {
            return Err(format!("terms={terms} 超上限 3600"));
        }
        // 每期还款额（等额本金场景 = 每期固定本金份额）
        let payment_exact: BigRational = match method {
            "annuity" => {
                if rate.is_zero() {
                    &principal / BigRational::from_integer(BigInt::from(terms))
                } else {
                    let base = BigRational::from_integer(1.into()) + &rate;
                    let growth = num_traits::pow(base, terms as usize);
                    let inv = if growth.is_zero() {
                        return Err(
                            "复利因子为零 — 自诊断指引: 检查 rate 与 terms 组合".to_string()
                        );
                    } else {
                        growth.recip()
                    };
                    let denom = BigRational::from_integer(1.into()) - inv;
                    if denom.is_zero() {
                        return Err(
                            "年金分母为零 — 自诊断指引: 检查 rate 与 terms 组合".to_string()
                        );
                    }
                    &principal * &rate / denom
                }
            }
            "straight" => &principal / BigRational::from_integer(BigInt::from(terms)),
            other => {
                return Err(format!(
                "未知 method '{other}' — 自诊断指引: 合法值 annuity(等额本息)/straight(等额本金)"
            ))
            }
        };
        // 账面本金先按 scale 舍入（与输出标度一致），全程整数记账
        let ten = crate::numcore::pow10(scale as u32);
        let (mut balance_u, _) = round_to_scale(&principal, scale, rounding)?;
        let (pay_u, _) = round_to_scale(&payment_exact, scale, rounding)?;
        let mut rows: Vec<JsonValue> = Vec::new();
        let mut total_interest = BigInt::zero();
        let mut total_payment = BigInt::zero();
        for i in 1..=terms {
            let balance_q = BigRational::new(balance_u.clone(), ten.clone());
            let interest_exact = &balance_q * &rate;
            let (int_u, _) = round_to_scale(&interest_exact, scale, rounding)?;
            let (prin_u, pay_i_u) = if i == terms {
                // 尾差并入末期：末期本金 = 剩余余额
                let p = balance_u.clone();
                let pay = &int_u + &p;
                (p, pay)
            } else if method == "annuity" {
                let p = &pay_u - &int_u;
                let pay = pay_u.clone();
                (p, pay)
            } else {
                let p = pay_u.clone();
                let pay = &p + &int_u;
                (p, pay)
            };
            balance_u -= &prin_u;
            total_interest += int_u.clone();
            total_payment += pay_i_u.clone();
            rows.push(JsonValue::object_from_pairs(&[
                ("period", JsonValue::Integer(i)),
                (
                    "payment",
                    JsonValue::string(decimal_string(&pay_i_u, scale)),
                ),
                ("interest", JsonValue::string(decimal_string(&int_u, scale))),
                (
                    "principal",
                    JsonValue::string(decimal_string(&prin_u, scale)),
                ),
                (
                    "balance",
                    JsonValue::string(decimal_string(&balance_u, scale)),
                ),
            ]));
        }
        Ok(envelope(&[
            ("method", JsonValue::string(method)),
            ("terms", JsonValue::Integer(terms)),
            ("scale", JsonValue::Integer(scale as i64)),
            ("rows", JsonValue::Array(rows)),
            (
                "total_interest",
                JsonValue::string(decimal_string(&total_interest, scale)),
            ),
            (
                "total_payment",
                JsonValue::string(decimal_string(&total_payment, scale)),
            ),
        ]))
    }
}

// ============================================================================
// 服务 7：单位换算（换算率由调用方显式提供）
// ============================================================================

pub struct UnitConvertService;

impl NativeService for UnitConvertService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let value = parse_value_as_rational(req_str(args, "value")?)?;
        let from = req_str(args, "from")?;
        let to = req_str(args, "to")?;
        let scale = req_scale(args)?;
        let rounding = req_rounding(args)?;
        let table = match args.get("rate_table") {
            Some(t @ JsonValue::Object(_)) => t,
            _ => {
                return Err(
                    "缺少参数 rate_table(对象) — 自诊断指引: 形如 {\"lb/kg\":\"0.45359237\"}"
                        .to_string(),
                )
            }
        };
        let key = format!("{from}/{to}");
        let rate_str = table.get(&key).and_then(|v| v.as_str()).ok_or_else(|| {
            format!(
                "换算率表缺少键 '{key}' — 自诊断指引: rate_table 键形如 \
                 \"来源单位/目标单位\", 服务不内置任何换算率"
            )
        })?;
        let (ru, rs) = parse_decimal(rate_str)?;
        let rate = BigRational::new(ru, crate::numcore::pow10(rs as u32));
        let result_exact = &value * &rate;
        Ok(envelope(&[
            ("from", JsonValue::string(from)),
            ("to", JsonValue::string(to)),
            ("rate_used", JsonValue::string(rate_str)),
            ("result", dec_obj(&result_exact, scale, rounding)?),
        ]))
    }
}

// ============================================================================
// 服务 8：量纲感知表达式求值（量纲原语复用 inference 家族）
// ============================================================================

pub struct DimEvalService;

impl NativeService for DimEvalService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let expr = req_str(args, "expr")?;
        let scale = req_scale(args)?;
        let rounding = req_rounding(args)?;
        let val = crate::numcore::eval_expr(
            expr,
            ExprDialect {
                decimal_literals: true,
                units: true,
                identifiers: false,
            },
            &|_| None,
            &|u: &str| {
                evorule_inference_services::dimension::SIDimension::from_si_unit(u)
                    .map(|d| d.to_vector())
            },
        )
        .map_err(|e| format!("{e} (expr={expr})"))?;
        let dim = val.dim.ok_or_else(|| {
            "表达式无量纲 — 自诊断指引: 至少一个操作数须带单位 \
             (如 3 m/s), 纯数值请用 numeric_decimal_eval"
                .to_string()
        })?;
        let sid = evorule_inference_services::dimension::SIDimension::from_vector(dim);
        Ok(envelope(&[
            ("expr", JsonValue::string(expr)),
            (
                "dimension",
                JsonValue::Array(dim.iter().map(|x| JsonValue::Integer(*x as i64)).collect()),
            ),
            ("si_string", JsonValue::string(sid.to_si_string())),
            ("numerator", JsonValue::string(val.q.numer().to_string())),
            ("denominator", JsonValue::string(val.q.denom().to_string())),
            ("value_decimal", dec_obj(&val.q, scale, rounding)?),
        ]))
    }
}

// ============================================================================
// 构造子（声明表 make 字段引用）
// ============================================================================

pub fn mk_rational_eval() -> Arc<dyn NativeService> {
    Arc::new(RationalEvalService)
}
pub fn mk_decimal_eval() -> Arc<dyn NativeService> {
    Arc::new(DecimalEvalService)
}
pub fn mk_decimal_convert() -> Arc<dyn NativeService> {
    Arc::new(DecimalConvertService)
}
pub fn mk_number_theory() -> Arc<dyn NativeService> {
    Arc::new(NumberTheoryService)
}
pub fn mk_fin_interest() -> Arc<dyn NativeService> {
    Arc::new(FinInterestService)
}
pub fn mk_fin_amortize() -> Arc<dyn NativeService> {
    Arc::new(FinAmortizeService)
}
pub fn mk_unit_convert() -> Arc<dyn NativeService> {
    Arc::new(UnitConvertService)
}
pub fn mk_dim_eval() -> Arc<dyn NativeService> {
    Arc::new(DimEvalService)
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
    fn rational_eval_envelope_and_value() {
        let out = RationalEvalService
            .execute(&args(&[("expr", JsonValue::string("1/3 + 1/6"))]))
            .expect("应成功");
        assert_eq!(field(&out, "numerator"), "1");
        assert_eq!(field(&out, "denominator"), "2");
        assert_eq!(field(&out, "status"), "determined");
        assert_eq!(field(&out, "determinism"), "bitwise_replayable");
    }

    #[test]
    fn decimal_eval_requires_rounding_fail_visible() {
        let err = DecimalEvalService
            .execute(&args(&[
                ("expr", JsonValue::string("1/3")),
                ("scale", JsonValue::Integer(2)),
            ]))
            .expect_err("缺 rounding 必须 Err");
        assert!(err.contains("rounding"), "{err}");
        let out = DecimalEvalService
            .execute(&args(&[
                ("expr", JsonValue::string("1/3")),
                ("scale", JsonValue::Integer(2)),
                ("rounding", JsonValue::string("half_up")),
            ]))
            .expect("应成功");
        assert_eq!(field(&out, "decimal_string"), "0.33");
        // 精确语义：0.1 + 0.2 精确等于 3/10，非 0.30000000000000004
        let out = DecimalEvalService
            .execute(&args(&[
                ("expr", JsonValue::string("0.1 + 0.2")),
                ("scale", JsonValue::Integer(2)),
                ("rounding", JsonValue::string("half_even")),
            ]))
            .expect("应成功");
        assert_eq!(field(&out, "decimal_string"), "0.30");
        assert_eq!(field(&out, "numerator"), "3");
        assert_eq!(field(&out, "denominator"), "10");
    }

    #[test]
    fn decimal_convert_periodic_honest() {
        let out = DecimalConvertService
            .execute(&args(&[
                ("value", JsonValue::string("1/3")),
                ("target", JsonValue::string("decimal")),
                ("scale", JsonValue::Integer(4)),
                ("rounding", JsonValue::string("half_even")),
            ]))
            .expect("应成功");
        assert_eq!(out.get("terminating"), Some(&JsonValue::Bool(false)));
        assert_eq!(field(&out, "decimal_string"), "0.3333");
        let out = DecimalConvertService
            .execute(&args(&[
                ("value", JsonValue::string("1/8")),
                ("target", JsonValue::string("decimal")),
                ("scale", JsonValue::Integer(2)),
                ("rounding", JsonValue::string("half_even")),
            ]))
            .expect("应成功");
        assert_eq!(out.get("terminating"), Some(&JsonValue::Bool(true)));
        assert_eq!(field(&out, "decimal_string"), "0.125");
    }

    #[test]
    fn number_theory_dispatch() {
        let out = NumberTheoryService
            .execute(&args(&[
                ("op", JsonValue::string("nCr")),
                (
                    "args",
                    JsonValue::Array(vec![JsonValue::Integer(52), JsonValue::Integer(5)]),
                ),
            ]))
            .expect("应成功");
        assert_eq!(field(&out, "result"), "2598960");
    }

    #[test]
    fn fin_interest_compound_golden() {
        let out = FinInterestService
            .execute(&args(&[
                ("principal", JsonValue::string("1000")),
                ("rate", JsonValue::string("0.05")),
                ("periods", JsonValue::Integer(2)),
                ("mode", JsonValue::string("compound")),
                ("scale", JsonValue::Integer(2)),
                ("rounding", JsonValue::string("half_even")),
            ]))
            .expect("应成功");
        let total = out.get("total").expect("total 应存在");
        assert_eq!(
            total.get("decimal_string").and_then(|v| v.as_str()),
            Some("1102.50")
        );
    }

    #[test]
    fn fin_amortize_annuity_tail_converges_to_zero() {
        let out = FinAmortizeService
            .execute(&args(&[
                ("principal", JsonValue::string("300")),
                ("rate", JsonValue::string("0.10")),
                ("terms", JsonValue::Integer(3)),
                ("method", JsonValue::string("annuity")),
                ("scale", JsonValue::Integer(2)),
                ("rounding", JsonValue::string("half_up")),
            ]))
            .expect("应成功");
        let rows = match out.get("rows") {
            Some(JsonValue::Array(r)) => r,
            _ => panic!("rows 应为数组"),
        };
        assert_eq!(rows.len(), 3);
        let last = &rows[2];
        assert_eq!(last.get("balance").and_then(|v| v.as_str()), Some("0.00"));
    }

    #[test]
    fn unit_convert_missing_rate_fail_visible() {
        let err = UnitConvertService
            .execute(&args(&[
                ("value", JsonValue::string("10")),
                ("from", JsonValue::string("lb")),
                ("to", JsonValue::string("kg")),
                ("rate_table", JsonValue::object_from_pairs(&[])),
                ("scale", JsonValue::Integer(3)),
                ("rounding", JsonValue::string("half_even")),
            ]))
            .expect_err("缺换算率必须 Err");
        assert!(err.contains("lb/kg"), "{err}");
    }

    #[test]
    fn unit_convert_golden() {
        let out = UnitConvertService
            .execute(&args(&[
                ("value", JsonValue::string("10")),
                ("from", JsonValue::string("lb")),
                ("to", JsonValue::string("kg")),
                (
                    "rate_table",
                    JsonValue::object_from_pairs(&[("lb/kg", JsonValue::string("0.45359237"))]),
                ),
                ("scale", JsonValue::Integer(3)),
                ("rounding", JsonValue::string("half_even")),
            ]))
            .expect("应成功");
        let result = out.get("result").expect("result 应存在");
        assert_eq!(
            result.get("decimal_string").and_then(|v| v.as_str()),
            Some("4.536")
        );
    }

    #[test]
    fn rational_eval_with_vars_bindings() {
        // vars 绑定：规则立法公式(静态)，数据(payload)动态填入
        let out = RationalEvalService
            .execute(&args(&[
                ("expr", JsonValue::string("a*b + c")),
                (
                    "vars",
                    JsonValue::object_from_pairs(&[
                        ("a", JsonValue::string("2")),
                        ("b", JsonValue::string("3")),
                        ("c", JsonValue::string("1/4")),
                    ]),
                ),
            ]))
            .expect("应成功");
        // 2*3 + 1/4 = 25/4
        assert_eq!(field(&out, "numerator"), "25");
        assert_eq!(field(&out, "denominator"), "4");
        let err = RationalEvalService
            .execute(&args(&[("expr", JsonValue::string("x + 1"))]))
            .expect_err("未知变量必须 Err");
        assert!(err.contains("未知变量"), "{err}");
    }

    #[test]
    fn decimal_eval_with_vars_and_rounding() {
        let out = DecimalEvalService
            .execute(&args(&[
                ("expr", JsonValue::string("price * qty")),
                (
                    "vars",
                    JsonValue::object_from_pairs(&[
                        ("price", JsonValue::string("19.99")),
                        ("qty", JsonValue::string("3")),
                    ]),
                ),
                ("scale", JsonValue::Integer(2)),
                ("rounding", JsonValue::string("half_even")),
            ]))
            .expect("应成功");
        assert_eq!(field(&out, "decimal_string"), "59.97");
    }

    #[test]
    fn dim_eval_velocity_times_time() {
        let out = DimEvalService
            .execute(&args(&[
                ("expr", JsonValue::string("(3 m/s) * (2 s)")),
                ("scale", JsonValue::Integer(2)),
                ("rounding", JsonValue::string("half_even")),
            ]))
            .expect("应成功");
        assert_eq!(field(&out, "si_string"), "m");
        assert_eq!(field(&out, "numerator"), "6");
        let dim = match out.get("dimension") {
            Some(JsonValue::Array(a)) => a.clone(),
            _ => panic!("dimension 应为数组"),
        };
        assert_eq!(dim[0], JsonValue::Integer(1));
    }

    #[test]
    fn dim_eval_bare_fractional_power_unit_not_squared() {
        // 裸分数幂单位:9.8 m/s^2 应为 9.8 与加速度量纲——此前词法器把
        // m/s^2 切成 Unit("m/s")+^+2,幂作用到「值×单位」整体得 96.04 m²/s²
        let out = DimEvalService
            .execute(&args(&[
                ("expr", JsonValue::string("9.8 m/s^2")),
                ("scale", JsonValue::Integer(4)),
                ("rounding", JsonValue::string("half_even")),
            ]))
            .expect("应成功");
        assert_eq!(field(&out, "numerator"), "49");
        assert_eq!(field(&out, "denominator"), "5");
        let dim = match out.get("dimension") {
            Some(JsonValue::Array(a)) => a.clone(),
            _ => panic!("dimension 应为数组"),
        };
        assert_eq!(dim[0], JsonValue::Integer(1), "长度幂次应为 1(非 2)");
        assert_eq!(dim[2], JsonValue::Integer(-2), "时间幂次应为 -2(非 -4)");
    }

    #[test]
    fn dim_eval_gravity_times_time() {
        // 演示黄金例:9.8 m/s^2 × 2 s = 19.6 m/s
        let out = DimEvalService
            .execute(&args(&[
                ("expr", JsonValue::string("9.8 m/s^2 * 2 s")),
                ("scale", JsonValue::Integer(2)),
                ("rounding", JsonValue::string("half_even")),
            ]))
            .expect("应成功");
        assert_eq!(field(&out, "si_string"), "m / s");
        assert_eq!(field(&out, "numerator"), "98");
        assert_eq!(field(&out, "denominator"), "5");
    }

    #[test]
    fn dim_eval_negative_caret_exponent() {
        // 负幂记法:9.8 m/s^2 × 2 s^-1 = 19.6 m/s³(负幂正确并入单位;
        // 服务要求结果带量纲,无量纲算式按设计走 numeric_decimal_eval)
        let out = DimEvalService
            .execute(&args(&[
                ("expr", JsonValue::string("9.8 m/s^2 * 2 s^-1")),
                ("scale", JsonValue::Integer(2)),
                ("rounding", JsonValue::string("half_even")),
            ]))
            .expect("应成功");
        assert_eq!(field(&out, "si_string"), "m / s³");
        assert_eq!(field(&out, "numerator"), "98");
        assert_eq!(field(&out, "denominator"), "5");
    }

    #[test]
    fn dim_eval_incompatible_addition_fails() {
        let err = DimEvalService
            .execute(&args(&[
                ("expr", JsonValue::string("(3 m) + (2 s)")),
                ("scale", JsonValue::Integer(2)),
                ("rounding", JsonValue::string("half_even")),
            ]))
            .expect_err("量纲不一致必须 Err");
        assert!(err.contains("量纲不一致"), "{err}");
    }
}

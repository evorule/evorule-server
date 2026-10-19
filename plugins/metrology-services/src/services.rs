//! 原生服务实现：GUM 乘法幂模型不确定度传播（JSON 入 / JSON 出，统一信封 v1）。
//!
//! # 确定性
//! 全程有理数运算 + 定点平方根原语，无浮点、无时钟、无随机。
//! 同输入两次执行结果逐位一致。
//!
//! # 模型
//! y = Π xᵢ^aᵢ（aᵢ 整数敏感指数）；相对合成 u_y/y = √( Σ (aᵢ·uᵢ/xᵢ)² )。
//! - y 为纯精确有理数（整数幂）；相对不确定度经 `sqrt_fixed` 原语，
//!   误差契约 |err| ≤ 10⁻ˢᶜᵃˡᵉ（出参注明）；
//! - **独立输入假设**：相关项（rᵢⱼ≠0）与加法模型不支持，如实拒解。
//!
//! # 诚实边界
//! - 非整数指数 → fail-visible（½ 类指数留待根式原语扩展,不静默近似）；
//! - xᵢ = 0（相对合成无定义）/ uᵢ < 0 → fail-visible；
//! - 项数上限 64（资源守卫）。

#![forbid(unsafe_code)]

use std::sync::Arc;

use evorule_numeric_services::numcore::{
    decimal_string, parse_rounding, round_to_scale, sqrt_fixed, Rounding,
};
use evorule_numeric_services::services::parse_value_as_rational;
use evorule_plugin_kit::NativeService;
use evorule_reactor::IoResult;
use evorule_tcb::JsonValue;
use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::ToPrimitive;
use num_traits::Zero;

pub type Q = BigRational;

fn int_const(n: i64) -> Q {
    Q::from(BigInt::from(n))
}

// ============================================================================
// 参数提取助手（全部 fail-visible 带自诊断指引）
// ============================================================================

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
    match args.get("rounding") {
        Some(v) => v
            .as_str()
            .ok_or_else(|| "参数 rounding 须为字符串".to_string())
            .and_then(parse_rounding),
        None => Err("缺少参数 rounding — 自诊断指引: 见服务 parameters 契约".to_string()),
    }
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

// ============================================================================
// metrology_uncertainty_propagate — GUM 乘法幂模型传播
// ============================================================================

/// 单测量项（x 测得值,u 标准不确定度,a 整数敏感指数）。
struct Term {
    x: Q,
    u: Q,
    a: i64,
}

fn parse_terms(args: &JsonValue) -> Result<Vec<Term>, String> {
    let raw = args
        .get("terms")
        .ok_or_else(|| "缺少参数 terms — 自诊断指引: 见服务 parameters 契约".to_string())?;
    let JsonValue::Array(items) = raw else {
        return Err(
            "参数 terms 须为数组 — 自诊断指引: 每项 {\"x\":..,\"u\":..,\"a\":..}".to_string(),
        );
    };
    if items.is_empty() {
        return Err("terms 为空 — 自诊断指引: 至少 1 项".to_string());
    }
    if items.len() > 64 {
        return Err(format!(
            "terms {} 项超过上限 64 — 自诊断指引: 资源守卫,如确需更多请拆分模型",
            items.len()
        ));
    }
    let mut terms = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let get = |k: &str| -> Result<&str, String> {
            item.get(k)
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("terms[{i}].{k} 须为字符串编码数值"))
        };
        let x = parse_value_as_rational(get("x")?)
            .map_err(|e| format!("terms[{i}].x 解析失败: {e}"))?;
        let u = parse_value_as_rational(get("u")?)
            .map_err(|e| format!("terms[{i}].u 解析失败: {e}"))?;
        let a = match item.get("a") {
            None => 1i64,
            Some(JsonValue::Integer(n)) => *n,
            Some(v) => match v.as_str() {
                Some(sv) => {
                    let q = parse_value_as_rational(sv)
                        .map_err(|e| format!("terms[{i}].a 解析失败: {e}"))?;
                    let int_part = q.floor();
                    if q != int_part {
                        return Err(format!(
                            "terms[{i}].a = {q} 非整数 — 自诊断指引: 本服务只支持整数敏感指数\
                             (y 须为精确有理数;½ 类指数留待根式原语扩展)"
                        ));
                    }
                    let numer = int_part.numer().clone();
                    numer.to_i64().ok_or_else(|| {
                        format!("terms[{i}].a 超出 i64 范围 — 自诊断指引: 指数绝对值过大")
                    })?
                }
                None => {
                    return Err(format!("terms[{i}].a 须为整数或字符串编码数值"));
                }
            },
        };
        if x.is_zero() {
            return Err(format!(
                "terms[{i}].x = 0 — 自诊断指引: 相对不确定度合成要求测得值非零"
            ));
        }
        if u < Q::zero() {
            return Err(format!(
                "terms[{i}].u < 0 — 自诊断指引: 标准不确定度为非负量"
            ));
        }
        terms.push(Term { x, u, a });
    }
    Ok(terms)
}

struct UncertaintyPropagateService;

impl NativeService for UncertaintyPropagateService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let terms = parse_terms(args)?;
        let scale = req_scale(args)?;
        if scale > 27 {
            return Err(format!(
                "参数 scale={scale} 超出本服务误差契约上限 27 — 自诊断指引: 平方根原语守护位限制"
            ));
        }
        let rounding = req_rounding(args)?;
        // y = Π xᵢ^aᵢ（整数幂,纯精确）。
        let mut y = Q::from(BigInt::from(1i64));
        // 相对方差 Σ (a·u/x)²（精确）。
        let mut rel2 = Q::zero();
        for t in &terms {
            let p = if t.a >= 0 {
                num_traits::pow::pow(t.x.clone(), t.a as usize)
            } else {
                Q::from(BigInt::from(1i64)) / num_traits::pow::pow(t.x.clone(), (-t.a) as usize)
            };
            y *= p;
            let rel_term = int_const(t.a) * t.u.clone() / t.x.clone();
            rel2 += rel_term.clone() * rel_term;
        }
        // 相对不确定度 = √rel²（定点平方根,误差契约 |err| ≤ 10⁻ˢᶜᵃˡᵉ）。
        let (rel_unscaled, rel_scale) = sqrt_fixed(&rel2, scale as u32, rounding)?;
        let rel = BigRational::new(rel_unscaled, {
            let mut d = BigInt::from(1i64);
            for _ in 0..rel_scale.max(0) {
                d *= 10;
            }
            d
        });
        let u_y = y.clone() * rel.clone();
        let out = vec![
            ("y", dec_obj(&y, scale, rounding)?),
            ("y_exact", q_str(&y)),
            ("rel_u", dec_obj(&rel, scale, rounding)?),
            ("u_y", dec_obj(&u_y, scale, rounding)?),
            ("terms_count", JsonValue::Integer(terms.len() as i64)),
            (
                "note",
                JsonValue::string(
                    "GUM 乘法幂模型: u_y/y = √(Σ(aᵢ·uᵢ/xᵢ)²);独立输入假设;rel_u 误差 ≤10⁻ˢᶜᵃˡᵉ;\
                     相关项与加法模型如实拒解",
                ),
            ),
        ];
        Ok(envelope(&out))
    }
}

pub fn mk_uncertainty_propagate() -> Arc<dyn NativeService> {
    Arc::new(UncertaintyPropagateService)
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

    fn term(x: &str, u: &str, a: JsonValue) -> JsonValue {
        JsonValue::object_from_pairs(&[
            ("x", JsonValue::string(x)),
            ("u", JsonValue::string(u)),
            ("a", a),
        ])
    }

    /// 黄金向量：y = x·z,x=2±0.01(相对 0.005),z=100±1(相对 0.01)
    /// → y=200,rel=√(0.005²+0.01²)=0.011180,u_y≈2.236。
    #[test]
    fn multiply_model_golden() {
        let out = UncertaintyPropagateService
            .execute(&args(&[
                (
                    "terms",
                    JsonValue::Array(vec![
                        term("2", "0.01", JsonValue::Integer(1)),
                        term("100", "1", JsonValue::Integer(1)),
                    ]),
                ),
                ("scale", JsonValue::Integer(4)),
                ("rounding", rounding()),
            ]))
            .expect("乘法模型黄金向量应成功");
        assert_eq!(dec_of(&out, "y"), "200.0000");
        assert_eq!(dec_of(&out, "rel_u"), "0.0112", "相对合成(误差契约)");
        assert_eq!(
            dec_of(&out, "u_y"),
            "2.2400",
            "u_y 与已发布 rel_u 自洽(200×0.0112)"
        );
    }

    /// 黄金向量：幂模型 y=x²/z（a=2,−1）,x=3±0.03(相对 0.01),z=12±0.12(相对 0.01)
    /// → y=0.75,rel=√(0.02²+0.01²)=0.022361,u_y≈0.016771。
    #[test]
    fn power_model_golden() {
        let out = UncertaintyPropagateService
            .execute(&args(&[
                (
                    "terms",
                    JsonValue::Array(vec![
                        term("3", "0.03", JsonValue::Integer(2)),
                        term("12", "0.12", JsonValue::Integer(-1)),
                    ]),
                ),
                ("scale", JsonValue::Integer(4)),
                ("rounding", rounding()),
            ]))
            .expect("幂模型黄金向量应成功");
        assert_eq!(dec_of(&out, "y"), "0.7500");
        assert_eq!(dec_of(&out, "rel_u"), "0.0224", "指数参与合成(a=2 加倍)");
        assert_eq!(dec_of(&out, "u_y"), "0.0168");
        // 非整数指数 fail-visible。
        let err = UncertaintyPropagateService
            .execute(&args(&[
                (
                    "terms",
                    JsonValue::Array(vec![term("4", "0.01", JsonValue::string("1/2"))]),
                ),
                ("scale", JsonValue::Integer(4)),
                ("rounding", rounding()),
            ]))
            .expect_err("½ 指数应报错");
        assert!(err.contains("非整数"), "报错应点名整数指数约束: {err}");
        // x=0 fail-visible。
        let err = UncertaintyPropagateService
            .execute(&args(&[
                (
                    "terms",
                    JsonValue::Array(vec![term("0", "0.01", JsonValue::Integer(1))]),
                ),
                ("scale", JsonValue::Integer(4)),
                ("rounding", rounding()),
            ]))
            .expect_err("x=0 应报错");
        assert!(err.contains("非零"), "报错应点名非零约束: {err}");
    }
}

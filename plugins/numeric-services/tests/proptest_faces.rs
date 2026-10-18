//! 随机性质面：表达式代数恒等式与分数↔十进制往返不变量（proptest）。
//!
//! 这些性质不依赖具体黄金值——它们是代数结构本身的断言，
//! 与黄金向量面互补。全部经由 crate 公开 API（服务 execute / pub numcore）。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use evorule_numeric_services::numcore::{
    eval_expr, parse_decimal, round_to_scale, ExprDialect, Rounding,
};
use evorule_numeric_services::services::DecimalConvertService;
use evorule_plugin_kit::NativeService;
use num_rational::BigRational;
use proptest::prelude::*;

fn no_units(_: &str) -> Option<[i32; 7]> {
    None
}

fn no_vars(_: &str) -> Option<BigRational> {
    None
}

fn dialect_dec() -> ExprDialect {
    ExprDialect {
        decimal_literals: true,
        units: false,
        identifiers: false,
    }
}

/// 0-9999 的整数策略。
fn small_int() -> impl Strategy<Value = i64> {
    0..=9999i64
}

fn eval_int(src: &str) -> BigRational {
    eval_expr(src, dialect_dec(), &no_vars, &no_units)
        .expect("求值应成功")
        .q
}

fn convert_to_decimal(value: &str, scale: i32, rounding: &str) -> Option<String> {
    let out = DecimalConvertService
        .execute(&evorule_tcb::JsonValue::object_from_pairs(&[
            ("value", evorule_tcb::JsonValue::string(value)),
            ("target", evorule_tcb::JsonValue::string("decimal")),
            ("scale", evorule_tcb::JsonValue::Integer(scale as i64)),
            ("rounding", evorule_tcb::JsonValue::string(rounding)),
        ]))
        .expect("服务调用应成功");
    out.get("decimal_string")
        .and_then(|v| v.as_str())
        .map(String::from)
}

proptest! {
    /// 分配律：(a + b) * c == a*c + b*c（表达式引擎代数正确性）。
    #[test]
    fn distributivity_holds(a in small_int(), b in small_int(), c in small_int().prop_filter("非零", |v| *v != 0)) {
        let lhs = eval_int(&format!("({a} + {b}) * {c}"));
        let rhs_a = eval_int(&format!("{a} * {c}"));
        let rhs_b = eval_int(&format!("{b} * {c}"));
        prop_assert_eq!(lhs, rhs_a + rhs_b);
    }

    /// 幂还原：a^m * a^(-m) == 1（负指数语义）。
    #[test]
    fn power_inverse_holds(a in small_int().prop_filter("非零", |v| *v != 0), m in 0i64..=64) {
        let lhs = eval_int(&format!("{a}^{m} * {a}^-{m}"));
        prop_assert_eq!(lhs, BigRational::from_integer(1.into()));
    }

    /// 终止分数往返：分母只含 2/5 因子的分数 → 十进制 → 同一分数。
    #[test]
    fn terminating_fraction_decimal_roundtrip(
        num in -10000i64..=10000,
        pow2 in 0u32..=10,
        pow5 in 0u32..=6,
    ) {
        let den = 2u64.pow(pow2) * 5u64.pow(pow5);
        let frac = format!("{num}/{den}");
        let dec_str = convert_to_decimal(&frac, 20, "half_even")
            .expect("终止分数转十进制应成功");
        // 往返：十进制字符串 → 有理数 == 原分数
        let (u, s) = parse_decimal(&dec_str).expect("往返字面量须合法");
        let back = BigRational::new(u, evorule_numeric_services::numcore::pow10(s as u32));
        prop_assert_eq!(back, BigRational::new(num.into(), den.into()));
    }

    /// 舍入的确定性：同一输入两次舍入结果逐位一致（信封承诺的自证）。
    #[test]
    fn rounding_is_deterministic(num in -100000i64..=100000, den in 1i64..=997, scale in 0i32..=8) {
        let r = BigRational::new(num.into(), den.into());
        let a = round_to_scale(&r, scale, Rounding::HalfEven).expect("合法 scale");
        let b = round_to_scale(&r, scale, Rounding::HalfEven).expect("合法 scale");
        prop_assert_eq!(a, b);
    }

    /// 十进制字面量解析无损：parse → string → parse 稳定。
    #[test]
    fn decimal_parse_stable(int_part in 0i64..=999, frac_len in 0usize..=6) {
        let digits = "12345678901234567890";
        let lit = if frac_len == 0 {
            format!("{int_part}")
        } else {
            format!("{int_part}.{}", &digits[..frac_len])
        };
        let (u1, s1) = parse_decimal(&lit).expect("合法字面量");
        let s = evorule_numeric_services::numcore::decimal_string(&u1, s1);
        let (u2, s2) = parse_decimal(&s).expect("往返字面量");
        prop_assert_eq!((u1, s1), (u2, s2));
    }
}

//! 数论原子：gcd/lcm/factorial/nCr/nPr——大整数精确计算。
//!
//! # 确定性
//! 纯整数域运算，无浮点无时钟；容量上限显式（超限 fail-visible）。

use num_bigint::BigInt;
use num_traits::{One, Signed, ToPrimitive, Zero};

/// 阶乘参数上限（10000! 约 35k 位十进制——分析工具不是执行器）。
pub const MAX_FACTORIAL_N: u64 = 10000;
/// 组合/排列参数 n 上限。
pub const MAX_NCR_N: u64 = 100000;

fn check_nonneg(name: &str, v: i64) -> Result<u64, String> {
    if v < 0 {
        return Err(format!(
            "参数 {name}={v} 须为非负整数 — 自诊断指引: 本服务不支持负参数"
        ));
    }
    Ok(v as u64)
}

/// 欧几里得 gcd（避免引入额外 trait 依赖）。
pub fn gcd(a: &BigInt, b: &BigInt) -> BigInt {
    let mut a = a.abs();
    let mut b = b.abs();
    while !b.is_zero() {
        let r = &a % &b;
        a = b;
        b = r;
    }
    a
}

pub fn lcm(a: &BigInt, b: &BigInt) -> Result<BigInt, String> {
    if a.is_zero() || b.is_zero() {
        return Ok(BigInt::zero());
    }
    Ok(a * b / gcd(a, b))
}

pub fn factorial(n: u64) -> BigInt {
    let mut acc = BigInt::one();
    let mut i = BigInt::one();
    let limit = BigInt::from(n);
    while i <= limit {
        acc *= &i;
        i += 1;
    }
    acc
}

/// C(n, k)——乘法公式逐步除，每步整除（组合数前缀恒为整数）。
pub fn n_cr(n: u64, k: u64) -> BigInt {
    let k = k.min(n - k);
    let mut acc = BigInt::one();
    let mut i: u64 = 1;
    while i <= k {
        acc *= BigInt::from(n - k + i);
        acc /= BigInt::from(i);
        i += 1;
    }
    acc
}

/// P(n, k) = n·(n-1)···(n-k+1)。
pub fn n_pr(n: u64, k: u64) -> BigInt {
    let mut acc = BigInt::one();
    let mut i: u64 = 0;
    while i < k {
        acc *= BigInt::from(n - i);
        i += 1;
    }
    acc
}

/// 分派入口（op + i64 参数，超限/非法 fail-visible）。
pub fn evaluate(op: &str, args: &[i64]) -> Result<BigInt, String> {
    match op {
        "gcd" | "lcm" => {
            if args.len() != 2 {
                return Err(format!("op={op} 恰需 2 个参数, 得 {}", args.len()));
            }
            let a = BigInt::from(args[0]);
            let b = BigInt::from(args[1]);
            if op == "gcd" {
                Ok(gcd(&a, &b))
            } else {
                lcm(&a, &b)
            }
        }
        "factorial" => {
            let n = *args.first().ok_or("op=factorial 缺参数 n")?;
            let n = check_nonneg("n", n)?;
            if n > MAX_FACTORIAL_N {
                return Err(format!(
                    "n={n} 超阶乘上限 {MAX_FACTORIAL_N} — 自诊断指引: \
                     本服务是分析工具不是执行器, 结果体积不可控"
                ));
            }
            Ok(factorial(n))
        }
        "nCr" | "nPr" => {
            if args.len() != 2 {
                return Err(format!("op={op} 恰需 2 个参数(n, k), 得 {}", args.len()));
            }
            let n = check_nonneg("n", args[0])?;
            let k = check_nonneg("k", args[1])?;
            if k > n {
                return Err(format!("k={k} 不得大于 n={n}"));
            }
            if n > MAX_NCR_N {
                return Err(format!(
                    "n={n} 超上限 {MAX_NCR_N} — 自诊断指引: 结果体积不可控"
                ));
            }
            if op == "nCr" {
                Ok(n_cr(n, k))
            } else {
                Ok(n_pr(n, k))
            }
        }
        other => Err(format!(
            "未知 op '{other}' — 自诊断指引: 合法值 gcd/lcm/factorial/nCr/nPr"
        )),
    }
}

pub fn to_decimal_string(v: &BigInt) -> String {
    v.to_string()
}

pub fn to_i64_hint(v: &BigInt) -> Option<i64> {
    v.to_i64()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn golden_values() {
        assert_eq!(factorial(0), BigInt::one());
        assert_eq!(factorial(10).to_string(), "3628800");
        assert_eq!(n_cr(50, 25).to_string(), "126410606437752");
        assert_eq!(n_cr(52, 5).to_string(), "2598960");
        assert_eq!(n_pr(10, 3).to_string(), "720");
        assert_eq!(gcd(&(48).into(), &(36).into()).to_string(), "12");
        assert_eq!(
            evaluate("lcm", &[4, 6]).expect("lcm 应成功").to_string(),
            "12"
        );
    }

    #[test]
    fn caps_and_validation_fail_visible() {
        assert!(evaluate("factorial", &[10001]).is_err());
        assert!(evaluate("nCr", &[5, 9]).is_err());
        assert!(evaluate("gcd", &[1]).is_err());
        assert!(evaluate("sqrt", &[4]).is_err());
    }
}

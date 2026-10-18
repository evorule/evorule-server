//! 力学关系求解器：三量乘积互求 + 匀加速运动学组合解（纯有理数，确定性）。
//!
//! # 设计
//! - 全程 [`BigRational`] 精确运算；退化形态（除零/双根/欠定）一律
//!   fail-visible Err 并点名退化原因，不做任何隐式取舍；
//! - 匀加速运动学十条组合中，两条存在**符号歧义**（开平方双根），
//!   如实拒解（不给隐式符号假设），其余八条组合确定可解。

use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::Zero;

pub type Q = BigRational;

/// 整数常量 → 精确有理数（绕开字面量类型歧义）。
pub fn int_const(n: i64) -> Q {
    Q::from(BigInt::from(n))
}

/// 三量乘积关系 `c = a·b` 的互求与一致性校验（F=m·a / U=I·R 同构）。
///
/// - 恰两个已知 → 求第三个（除零退化 fail-visible）；
/// - 三个全知 → 一致性校验（`a·b == c`），失败 Err 附两侧值。
pub fn solve_product_trio(
    names: (&str, &str, &str),
    a: Option<Q>,
    b: Option<Q>,
    c: Option<Q>,
) -> Result<(Q, Q, Q), String> {
    match (a, b, c) {
        (Some(a), Some(b), c) => {
            let expect = &a * &b;
            match c {
                Some(cv) => {
                    if cv == expect {
                        Ok((a, b, cv))
                    } else {
                        Err(format!(
                            "一致性校验失败: {0}·{1} = {expect}, 而 {2} = {cv}",
                            names.0, names.1, names.2
                        ))
                    }
                }
                None => Ok((a, b, expect)),
            }
        }
        (Some(a), None, Some(c)) => {
            if a.is_zero() {
                return Err(format!(
                    "退化形态: {0} = 0 无法求解 {1} — 自诊断指引: 除数为零",
                    names.0, names.1
                ));
            }
            let b = &c / &a;
            Ok((a, b, c))
        }
        (None, Some(b), Some(c)) => {
            if b.is_zero() {
                return Err(format!(
                    "退化形态: {1} = 0 无法求解 {0} — 自诊断指引: 除数为零",
                    names.0, names.1
                ));
            }
            let a = &c / &b;
            Ok((a, b, c))
        }
        _ => Err(format!(
            "已知量不足 — 自诊断指引: {0}/{1}/{2} 三量中须恰给两个（求第三个）\
             或三个全给（一致性校验）",
            names.0, names.1, names.2
        )),
    }
}

/// 匀加速运动学五量：v₀（初速度）/a（加速度）/t（时间）/s（位移）/v（末速度）。
#[derive(Debug)]
pub struct KinKnown {
    pub v0: Option<Q>,
    pub a: Option<Q>,
    pub t: Option<Q>,
    pub s: Option<Q>,
    pub v: Option<Q>,
}

/// 组合解结果（五量全集）。
#[derive(Debug)]
pub struct KinSolved {
    pub v0: Q,
    pub a: Q,
    pub t: Q,
    pub s: Q,
    pub v: Q,
}

fn require(k: &Option<Q>, name: &str) -> Result<Q, String> {
    k.clone()
        .ok_or_else(|| format!("缺少独立量 {name} — 自诊断指引: known 须含 {name}"))
}

fn non_zero(k: &Q, name: &str) -> Result<(), String> {
    if k.is_zero() {
        return Err(format!(
            "退化形态: {name} = 0 使本组合不可解 — 自诊断指引: \
             该组合下 {name} 作除数, 请改用其他已知量组合"
        ));
    }
    Ok(())
}

/// 组合解入口。`keys` 为已知量名（恰三个，调用方校验；大小写敏感小写）。
///
/// 八条组合确定可解；{v₀,a,s} 与 {a,s,v} 需开平方且符号双根，如实拒解。
pub fn solve_kinematics(known: &KinKnown, keys: &[&str]) -> Result<KinSolved, String> {
    let mut keys = keys.to_vec();
    keys.sort_unstable();
    let get = |name: &str| -> Result<Q, String> {
        require(
            match name {
                "v0" => &known.v0,
                "a" => &known.a,
                "t" => &known.t,
                "s" => &known.s,
                "v" => &known.v,
                _ => return Err(format!("未知运动学量名 {name}")),
            },
            name,
        )
    };
    match keys.as_slice() {
        ["a", "t", "v0"] => {
            let (v0, a, t) = (get("v0")?, get("a")?, get("t")?);
            let v = &v0 + &(&a * &t);
            let half_at2 = &a * &(&t * &t) / int_const(2);
            let s = &v0 * &t + half_at2;
            Ok(KinSolved { v0, a, t, s, v })
        }
        ["a", "s", "t"] => {
            let (a, t, s) = (get("a")?, get("t")?, get("s")?);
            non_zero(&t, "t")?;
            let at2_half = &a * &(&t * &t) / int_const(2);
            let v0 = (&s - at2_half) / &t;
            let v = &v0 + &a * &t;
            Ok(KinSolved { v0, a, t, s, v })
        }
        ["a", "t", "v"] => {
            let (a, t, v) = (get("a")?, get("t")?, get("v")?);
            let v0 = &v - &a * &t;
            let s = &v * &t - &a * &(&t * &t) / int_const(2);
            Ok(KinSolved { v0, a, t, s, v })
        }
        ["a", "v", "v0"] => {
            let (v0, a, v) = (get("v0")?, get("a")?, get("v")?);
            non_zero(&a, "a")?;
            let t = (&v - &v0) / &a;
            let s = &(&v * &v - &v0 * &v0) / int_const(2);
            Ok(KinSolved { v0, a, t, s, v })
        }
        ["s", "t", "v0"] => {
            let (v0, t, s) = (get("v0")?, get("t")?, get("s")?);
            non_zero(&t, "t")?;
            let two = int_const(2);
            let v = two.clone() * &s / &t - &v0;
            let a = (two.clone() * (&s - &v0 * &t)) / (&t * &t);
            Ok(KinSolved { v0, a, t, s, v })
        }
        ["s", "t", "v"] => {
            let (t, s, v) = (get("t")?, get("s")?, get("v")?);
            non_zero(&t, "t")?;
            let two = int_const(2);
            let v0 = two.clone() * &s / &t - &v;
            let a = (two * (&v * &t - &s)) / (&t * &t);
            Ok(KinSolved { v0, a, t, s, v })
        }
        ["s", "v", "v0"] => {
            let (v0, s, v) = (get("v0")?, get("s")?, get("v")?);
            let sum = &v0 + &v;
            if sum.is_zero() {
                return Err("退化形态: v₀ + v = 0 且该组合下时间与加速度不可唯一确定 — \
                     自诊断指引: 补充 t 或 a 之一"
                    .to_string());
            }
            non_zero(&s, "s")?;
            let two = int_const(2);
            let t = &two * &s / &sum;
            let a = (&v * &v - &v0 * &v0) / (&two * &s);
            Ok(KinSolved { v0, a, t, s, v })
        }
        ["t", "v", "v0"] => {
            let (v0, t, v) = (get("v0")?, get("t")?, get("v")?);
            non_zero(&t, "t")?;
            let a = (&v - &v0) / &t;
            let s = (&v0 + &v) * &t / int_const(2);
            Ok(KinSolved { v0, a, t, s, v })
        }
        ["a", "s", "v0"] | ["a", "s", "v"] => {
            Err("该组合需开平方且时间/初速度存在正负双根 — 自诊断指引: \
             补充 t 或 v/v₀ 之一消除符号歧义（本服务不做隐式符号假设）"
                .to_string())
        }
        other => Err(format!(
            "不支持的已知量组合 {other:?} — 自诊断指引: 恰给 \
             v0/a/t/s/v 中三个独立量"
        )),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn q(n: i64, d: i64) -> Q {
        BigRational::new(n.into(), d.into())
    }

    fn known(v0: Option<Q>, a: Option<Q>, t: Option<Q>, s: Option<Q>, v: Option<Q>) -> KinKnown {
        KinKnown { v0, a, t, s, v }
    }

    #[test]
    fn product_trio_solve_and_consistency() {
        let (_, _, c) = solve_product_trio(("m", "a", "F"), Some(q(2, 1)), Some(q(3, 1)), None)
            .expect("应成功");
        assert_eq!(c, q(6, 1));
        let (a, _, _) = solve_product_trio(("m", "a", "F"), None, Some(q(3, 1)), Some(q(6, 1)))
            .expect("应成功");
        assert_eq!(a, q(2, 1));
        let err = solve_product_trio(("m", "a", "F"), Some(q(7, 1)), Some(q(2, 1)), Some(q(6, 1)))
            .expect_err("不一致必须 Err");
        assert!(err.contains("一致性校验失败"), "{err}");
    }

    #[test]
    fn kinematics_cross_formula_consistency() {
        // v0=2, a=3, t=4 → v=14, s=36；换组合解出的量必须与直解一致
        let direct = solve_kinematics(
            &known(Some(q(2, 1)), Some(q(3, 1)), Some(q(4, 1)), None, None),
            &["v0", "a", "t"],
        )
        .expect("应成功");
        assert_eq!(direct.v, q(14, 1));
        assert_eq!(direct.s, q(32, 1));
        let alt = solve_kinematics(
            &known(Some(q(2, 1)), None, Some(q(4, 1)), None, Some(q(14, 1))),
            &["v0", "t", "v"],
        )
        .expect("应成功");
        assert_eq!(alt.a, q(3, 1));
        assert_eq!(alt.s, q(32, 1));
        let alt2 = solve_kinematics(
            &known(None, Some(q(3, 1)), Some(q(4, 1)), Some(q(32, 1)), None),
            &["a", "t", "s"],
        )
        .expect("应成功");
        assert_eq!(alt2.v0, q(2, 1));
        assert_eq!(alt2.v, q(14, 1));
        let alt3 = solve_kinematics(
            &known(Some(q(2, 1)), None, None, Some(q(32, 1)), Some(q(14, 1))),
            &["v0", "s", "v"],
        )
        .expect("应成功");
        assert_eq!(alt3.t, q(4, 1));
        assert_eq!(alt3.a, q(3, 1));
    }

    #[test]
    fn kinematics_ambiguous_combos_fail_visible() {
        let err = solve_kinematics(
            &known(Some(q(2, 1)), Some(q(3, 1)), None, Some(q(32, 1)), None),
            &["v0", "a", "s"],
        )
        .expect_err("双根组合必须如实拒解");
        assert!(err.contains("双根"), "{err}");
    }

    #[test]
    fn kinematics_degenerate_fail_visible() {
        // a=3, t=0, s=0：组合需除 t → 如实拒
        let err = solve_kinematics(
            &known(None, Some(q(3, 1)), Some(q(0, 1)), Some(q(0, 1)), None),
            &["a", "t", "s"],
        )
        .expect_err("t=0 且需除 t 必须 Err");
        assert!(err.contains("t = 0"), "{err}");
    }
}

//! 精确数值核心：表达式引擎 + 定点十进制表示 + 舍入模式。
//!
//! # 设计要点
//! - 全程 [`BigRational`] 精确运算——表达式求值不做任何中间舍入，
//!   舍入只发生在输出端且必须显式指定 `scale` + `rounding`（fail-visible）；
//! - [`Rounding`] 四模式语义：`half_even`（银行家，缺省推荐）/`half_up`
//!   （远离零）/`floor`（趋向负无穷）/`trunc`（趋向零）；
//! - 十进制字面量 = 字符串编码（符号+整数段+小数段），内部表示为
//!   `(unscaled: BigInt, scale: i32)`，值 = unscaled × 10⁻ˢᶜᵃˡᵉ；
//! - 表达式引擎支持三种方言：纯整数（有理数求值）/ 十进制字面量 /
//!   带单位字面量（单位 → 七分量量纲向量由调用方注入查找函数）。
//! - 确定性：无浮点、无时钟、无随机；同输入两次求值结果逐位一致。

use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::{One, Signed, ToPrimitive, Zero};

/// 结果标度硬上限（|scale| 超界即 Err，防资源滥用）。
pub const MAX_SCALE: i32 = 36;
/// 十进制字面量小数段长度上限。
pub const MAX_LITERAL_SCALE: i32 = 40;
/// 表达式字符数上限。
pub const MAX_EXPR_CHARS: usize = 4096;
/// 括号嵌套深度上限。
pub const MAX_EXPR_DEPTH: usize = 64;
/// 整数指数绝对值上限（防有理数幂的位元体积爆炸）。
pub const MAX_POW_EXP: i64 = 1024;

/// 舍入模式（输出端显式指定，缺省即 Err——杜绝隐式舍入）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rounding {
    /// 银家舍入：正好半程时取偶数方向。
    HalfEven,
    /// 四舍五入：正好半程时远离零。
    HalfUp,
    /// 向负无穷取整。
    Floor,
    /// 向零取整。
    Trunc,
}

/// 解析舍入模式字符串（合法值之外的任何输入 fail-visible）。
pub fn parse_rounding(s: &str) -> Result<Rounding, String> {
    match s {
        "half_even" => Ok(Rounding::HalfEven),
        "half_up" => Ok(Rounding::HalfUp),
        "floor" => Ok(Rounding::Floor),
        "trunc" => Ok(Rounding::Trunc),
        other => Err(format!(
            "未知舍入模式 '{other}' — 自诊断指引: 合法值 \
             half_even(银行家)/half_up(四舍五入远离零)/floor(向负无穷)/trunc(向零)"
        )),
    }
}

/// 10ⁿ（n ≥ 0）。
pub fn pow10(n: u32) -> BigInt {
    let mut v = BigInt::one();
    let mut i = 0;
    while i < n {
        v *= 10;
        i += 1;
    }
    v
}

/// 解析十进制字面量字符串 → `(unscaled, scale)`（值 = unscaled × 10⁻ˢᶜᵃˡᵉ）。
///
/// 合法形态：`[+-]?数字[.数字]`（至少一位数字；小数段长度 ≤
/// [`MAX_LITERAL_SCALE`]）。其余任何形态 fail-visible。
pub fn parse_decimal(s: &str) -> Result<(BigInt, i32), String> {
    let t = s.trim();
    let (neg, body) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (int_part, frac_part) = match body.split_once('.') {
        Some((a, b)) => (a, b),
        None => (body, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return Err(format!(
            "非法十进制字面量 '{s}' — 自诊断指引: 形如 -12.3400"
        ));
    }
    if !int_part.chars().all(|c| c.is_ascii_digit())
        || !frac_part.chars().all(|c| c.is_ascii_digit())
    {
        return Err(format!("非法十进制字面量 '{s}' — 数字段含非数字字符"));
    }
    if frac_part.len() as i64 > MAX_LITERAL_SCALE as i64 {
        return Err(format!(
            "字面量小数段长度 {} 超上限 {MAX_LITERAL_SCALE}: '{s}'",
            frac_part.len()
        ));
    }
    let mut digits = String::with_capacity(int_part.len() + frac_part.len());
    digits.push_str(int_part);
    digits.push_str(frac_part);
    if digits.is_empty() {
        return Err(format!("非法十进制字面量 '{s}' — 缺少数位"));
    }
    let unscaled = BigInt::parse_bytes(digits.as_bytes(), 10)
        .ok_or_else(|| format!("十进制字面量解析失败: '{s}'"))?;
    Ok((
        if neg { -unscaled } else { unscaled },
        frac_part.len() as i32,
    ))
}

/// `(unscaled, scale)` → 十进制字符串（保留给定标度，不做规范化——
/// 字面量 `-12.3400` 往返还原为 `-12.3400`，确定性优先于美观）。
pub fn decimal_string(unscaled: &BigInt, scale: i32) -> String {
    let neg = unscaled.is_negative();
    let digits = unscaled.abs().to_string();
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if scale <= 0 {
        out.push_str(&digits);
        for _ in 0..(-scale) {
            out.push('0');
        }
        return out;
    }
    let scale_u = scale as usize;
    if digits.len() <= scale_u {
        out.push_str("0.");
        for _ in 0..(scale_u - digits.len()) {
            out.push('0');
        }
        out.push_str(&digits);
    } else {
        let split = digits.len() - scale_u;
        out.push_str(&digits[..split]);
        out.push('.');
        out.push_str(&digits[split..]);
    }
    out
}

/// 有理数按 `scale` + 模式舍入 → `(unscaled, scale)`。
///
/// 语义：先精确放大 10^scale，再对有理数做一次整数化——
/// `half_even`/`half_up` 在正好半程时的行为见 [`Rounding`]；
/// `floor` 向负无穷；`trunc` 向零。
pub fn round_to_scale(
    r: &BigRational,
    scale: i32,
    mode: Rounding,
) -> Result<(BigInt, i32), String> {
    if scale.abs() > MAX_SCALE {
        return Err(format!(
            "scale={scale} 超过硬上限 ±{MAX_SCALE} — 自诊断指引: 标度即结果小数位,\
             更高精度请改用分数输出形态"
        ));
    }
    let scaled = r * BigRational::from_integer(pow10(scale as u32));
    let p = scaled.numer();
    let q = scaled.denom();
    let neg = p.is_negative();
    let ap = p.abs();
    let mut qt = &ap / q;
    let rem = ap % q;
    let twice = &rem * 2;
    let up_abs = match mode {
        Rounding::Trunc => false,
        Rounding::Floor => false,
        Rounding::HalfUp => twice >= *q,
        Rounding::HalfEven => twice > *q || (twice == *q && (&qt % 2u8) == BigInt::one()),
    };
    if up_abs {
        qt += 1;
    }
    if mode == Rounding::Floor && neg && !rem.is_zero() {
        qt += 1;
    }
    Ok((if neg { -qt } else { qt }, scale))
}

/// 表达式方言。
#[derive(Clone, Copy, Debug)]
pub struct ExprDialect {
    /// 允许十进制字面量（false 时仅整数字面量）。
    pub decimal_literals: bool,
    /// 允许单位字面量（true 时数字后可跟单位 token，经 `unit_lookup` 查量纲）。
    pub units: bool,
}

#[derive(Debug, Clone)]
enum Tok {
    Int(String),
    Dec(String),
    Unit(String),
    Plus,
    Minu,
    Star,
    Slash,
    Caret,
    LParen,
    RParen,
}

#[derive(Debug, Clone)]
struct Toked {
    kind: Tok,
    pos: usize,
}

fn is_unit_char(c: char) -> bool {
    c.is_alphanumeric()
        || matches!(c, '·' | '°' | '⁄')
        || matches!(
            c,
            '¹' | '²' | '³' | '⁴' | '⁵' | '⁶' | '⁷' | '⁸' | '⁹' | '⁰' | '⁻'
        )
}

fn tokenize(src: &str, dialect: ExprDialect) -> Result<Vec<Toked>, String> {
    if src.chars().count() > MAX_EXPR_CHARS {
        return Err(format!(
            "表达式长度超上限 {MAX_EXPR_CHARS} 字符 — 自诊断指引: 拆分为多次求值"
        ));
    }
    let chars: Vec<(usize, char)> = src.char_indices().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < chars.len() {
        let (pos, c) = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            '+' => out.push(Toked {
                kind: Tok::Plus,
                pos,
            }),
            '-' => out.push(Toked {
                kind: Tok::Minu,
                pos,
            }),
            '*' => out.push(Toked {
                kind: Tok::Star,
                pos,
            }),
            '^' => out.push(Toked {
                kind: Tok::Caret,
                pos,
            }),
            '(' => out.push(Toked {
                kind: Tok::LParen,
                pos,
            }),
            ')' => out.push(Toked {
                kind: Tok::RParen,
                pos,
            }),
            '/' => {
                // 单位段中的 "/"（后随字母/数字）并入单位 token；否则为除号。
                let prev_unit = matches!(out.last().map(|t| &t.kind), Some(Tok::Unit(_)));
                let next_is_unit = chars
                    .get(i + 1)
                    .map(|(_, nc)| is_unit_char(*nc) && !nc.is_ascii_digit())
                    .unwrap_or(false);
                if dialect.units && prev_unit && next_is_unit {
                    if let Some(Toked {
                        kind: Tok::Unit(u), ..
                    }) = out.last_mut()
                    {
                        u.push('/');
                    }
                } else {
                    out.push(Toked {
                        kind: Tok::Slash,
                        pos,
                    });
                }
                i += 1;
                continue;
            }
            c if c.is_ascii_digit() => {
                let start = pos;
                let mut int_part = String::new();
                let mut frac_part = String::new();
                while i < chars.len() && chars[i].1.is_ascii_digit() {
                    int_part.push(chars[i].1);
                    i += 1;
                }
                if dialect.decimal_literals
                    && i < chars.len()
                    && chars[i].1 == '.'
                    && chars.get(i + 1).map(|(_, nc)| nc.is_ascii_digit()) == Some(true)
                {
                    i += 1; // 跳过 '.'
                    while i < chars.len() && chars[i].1.is_ascii_digit() {
                        frac_part.push(chars[i].1);
                        i += 1;
                    }
                }
                let kind = if frac_part.is_empty() {
                    Tok::Int(int_part)
                } else {
                    Tok::Dec(format!("{int_part}.{frac_part}"))
                };
                out.push(Toked { kind, pos: start });
                continue;
            }
            c if dialect.units && (c.is_alphabetic() || c == '·') => {
                let start = pos;
                let mut u = String::new();
                u.push(c);
                i += 1;
                while i < chars.len() {
                    let (_, nc) = chars[i];
                    let unit_continue = (is_unit_char(nc) && !nc.is_ascii_digit())
                        || (nc == '/'
                            && chars
                                .get(i + 1)
                                .map(|(_, nnc)| is_unit_char(*nnc) && !nnc.is_ascii_digit())
                                .unwrap_or(false));
                    if unit_continue {
                        u.push(nc);
                        i += 1;
                    } else {
                        break;
                    }
                }
                out.push(Toked {
                    kind: Tok::Unit(u),
                    pos: start,
                });
                continue;
            }
            other => {
                return Err(format!(
                    "位置 {pos}: 非法字符 '{other}' — 自诊断指引: 表达式仅支持 \
                     数字/+-*/^(){}",
                    if dialect.decimal_literals {
                        " 及小数点"
                    } else {
                        ""
                    }
                ));
            }
        }
        i += 1;
    }
    Ok(out)
}

/// 表达式值 = 精确有理数 + 可选量纲向量（七分量 [L,M,T,I,Θ,N,J]）。
#[derive(Clone, Debug)]
pub struct Val {
    pub q: BigRational,
    pub dim: Option<[i32; 7]>,
}

struct Parser<'a> {
    toks: Vec<Toked>,
    i: usize,
    dialect: ExprDialect,
    unit_lookup: &'a dyn Fn(&str) -> Option<[i32; 7]>,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.i).map(|t| &t.kind)
    }

    fn next(&mut self) -> Option<Toked> {
        let t = self.toks.get(self.i).cloned();
        if t.is_some() {
            self.i += 1;
        }
        t
    }

    fn require_dimless_int(&self, v: &Val, what: &str) -> Result<i64, String> {
        if v.dim.is_some() {
            return Err(format!("{what} 须为无量纲整数 — 量纲值只能作乘除因子"));
        }
        if !v.q.is_integer() {
            return Err(format!("{what} 须为整数 — 分数指数不在本服务能力面"));
        }
        let n =
            v.q.to_integer()
                .to_i64()
                .ok_or_else(|| format!("{what} 超出整数范围"))?;
        if n.abs() > MAX_POW_EXP {
            return Err(format!(
                "{what} 绝对值 {n} 超上限 {MAX_POW_EXP} — 自诊断指引: \
                 幂次过大会产生不可控体积的结果"
            ));
        }
        Ok(n)
    }

    fn expr(&mut self) -> Result<Val, String> {
        let mut left = self.term()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Plus) => 1,
                Some(Tok::Minu) => -1,
                _ => break,
            };
            self.i += 1;
            let right = self.term()?;
            left = match (&left.dim, &right.dim) {
                (None, d) => Val {
                    q: apply(&left.q, &right.q, op),
                    dim: *d,
                },
                (d, None) => Val {
                    q: apply(&left.q, &right.q, op),
                    dim: *d,
                },
                (Some(a), Some(b)) => {
                    if a != b {
                        return Err(format!(
                            "量纲不一致的加减法: {a:?} vs {b:?} — 自诊断指引: \
                             加减仅作用于同量纲操作数"
                        ));
                    }
                    Val {
                        q: apply(&left.q, &right.q, op),
                        dim: Some(*a),
                    }
                }
            };
        }
        Ok(left)
    }

    fn term(&mut self) -> Result<Val, String> {
        let mut left = self.unary()?;
        loop {
            let div = match self.peek() {
                Some(Tok::Star) => false,
                Some(Tok::Slash) => true,
                _ => break,
            };
            self.i += 1;
            let right = self.unary()?;
            if div && right.q.is_zero() {
                return Err(format!(
                    "位置 {}: 除以零 — 自诊断指引: 除数表达式求值为零",
                    self.toks
                        .get(self.i.saturating_sub(1))
                        .map(|t| t.pos)
                        .unwrap_or(0)
                ));
            }
            let dim = match (&left.dim, &right.dim) {
                (None, d) => *d,
                (d, None) => *d,
                (Some(a), Some(b)) => {
                    let mut v = *a;
                    for (i, x) in b.iter().enumerate() {
                        v[i] += if div { -x } else { *x };
                    }
                    if v == [0i32; 7] {
                        None
                    } else {
                        Some(v)
                    }
                }
            };
            let q = if div {
                &left.q / &right.q
            } else {
                &left.q * &right.q
            };
            left = Val { q, dim };
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Val, String> {
        if matches!(self.peek(), Some(Tok::Minu)) {
            self.i += 1;
            let v = self.unary()?;
            return Ok(Val {
                q: -v.q,
                dim: v.dim,
            });
        }
        self.power()
    }

    fn power(&mut self) -> Result<Val, String> {
        let base = self.atom()?;
        if matches!(self.peek(), Some(Tok::Caret)) {
            self.i += 1;
            let exp = self.unary()?;
            let n = self.require_dimless_int(&exp, "指数")?;
            if n < 0 && base.q.is_zero() {
                return Err("负指数作用于零 — 自诊断指引: 底数为零".to_string());
            }
            let dim = match base.dim {
                None => None,
                Some(d) => {
                    let mut v = d;
                    for x in v.iter_mut() {
                        *x = x.saturating_mul(n as i32);
                    }
                    if v == [0i32; 7] {
                        None
                    } else {
                        Some(v)
                    }
                }
            };
            let q = if n < 0 {
                num_traits::pow(base.q, n.unsigned_abs() as usize).recip()
            } else {
                num_traits::pow(base.q, n as usize)
            };
            return Ok(Val { q, dim });
        }
        Ok(base)
    }

    fn atom(&mut self) -> Result<Val, String> {
        match self.next() {
            Some(Toked {
                kind: Tok::Int(d),
                pos,
            }) => {
                let n = BigInt::parse_bytes(d.as_bytes(), 10)
                    .ok_or_else(|| format!("位置 {pos}: 整数字面量解析失败 '{d}'"))?;
                let unit = if self.dialect.units {
                    self.take_unit_suffix()?
                } else {
                    None
                };
                self.attach_unit(BigRational::from_integer(n), unit, pos)
            }
            Some(Toked {
                kind: Tok::Dec(d),
                pos,
            }) => {
                let (unscaled, scale) = parse_decimal(&d)?;
                let q = BigRational::new(unscaled, pow10(scale as u32));
                let unit = if self.dialect.units {
                    self.take_unit_suffix()?
                } else {
                    None
                };
                self.attach_unit(q, unit, pos)
            }
            Some(Toked {
                kind: Tok::LParen, ..
            }) => {
                self.depth += 1;
                if self.depth > MAX_EXPR_DEPTH {
                    return Err(format!("括号嵌套超上限 {MAX_EXPR_DEPTH}"));
                }
                let v = self.expr()?;
                match self.next() {
                    Some(Toked {
                        kind: Tok::RParen, ..
                    }) => {
                        self.depth -= 1;
                        Ok(v)
                    }
                    _ => Err("括号不闭合 — 自诊断指引: 检查 ')' 配对".to_string()),
                }
            }
            Some(Toked {
                kind: Tok::Unit(u),
                pos,
            }) => Err(format!(
                "位置 {pos}: 单位 '{u}' 前缺少数字 — 自诊断指引: 单位字面量形如 3 m/s"
            )),
            other => Err(format!(
                "位置 0: 意外的 token 结尾 — 自诊断指引: 表达式不完整 ({other:?})"
            )),
        }
    }

    /// 数字字面量后紧跟的单位 token（单位方言下）。
    fn take_unit_suffix(&mut self) -> Result<Option<String>, String> {
        if !self.dialect.units {
            return Ok(None);
        }
        if let Some(Toked {
            kind: Tok::Unit(u),
            pos,
        }) = self.toks.get(self.i).cloned()
        {
            let lookup = self.unit_lookup;
            if lookup(&u).is_none() {
                return Err(format!(
                    "位置 {pos}: 未知单位 '{u}' — 自诊断指引: 仅支持 SI 单位及其复合形 \
                     (如 m/s、kg·m/s²)"
                ));
            }
            self.i += 1;
            return Ok(Some(u));
        }
        Ok(None)
    }

    fn attach_unit(&self, q: BigRational, unit: Option<String>, pos: usize) -> Result<Val, String> {
        match unit {
            None => Ok(Val { q, dim: None }),
            Some(u) => {
                let lookup = self.unit_lookup;
                let dim = lookup(&u).ok_or_else(|| format!("位置 {pos}: 未知单位 '{u}'"))?;
                Ok(Val { q, dim: Some(dim) })
            }
        }
    }
}

fn apply(l: &BigRational, r: &BigRational, op: i32) -> BigRational {
    if op > 0 {
        l + r
    } else {
        l - r
    }
}

/// 表达式求值入口。
///
/// `unit_lookup`：单位 token → 七分量量纲向量；未知单位返回 `None`
/// （引擎据此报"未知单位"）。非单位方言传入永不命中的闭包即可。
pub fn eval_expr(
    src: &str,
    dialect: ExprDialect,
    unit_lookup: &dyn Fn(&str) -> Option<[i32; 7]>,
) -> Result<Val, String> {
    let toks = tokenize(src, dialect)?;
    if toks.is_empty() {
        return Err("空表达式 — 自诊断指引: 提供非空算式".to_string());
    }
    let mut p = Parser {
        toks,
        i: 0,
        dialect,
        unit_lookup,
        depth: 0,
    };
    let v = p.expr()?;
    if p.i != p.toks.len() {
        let pos = p.toks.get(p.i).map(|t| t.pos).unwrap_or(0);
        return Err(format!(
            "位置 {pos}: 多余的 token — 自诊断指引: 检查运算符/括号配对"
        ));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn no_units(_: &str) -> Option<[i32; 7]> {
        None
    }

    fn eval_int(src: &str) -> BigRational {
        eval_expr(
            src,
            ExprDialect {
                decimal_literals: false,
                units: false,
            },
            &no_units,
        )
        .expect("求值应成功")
        .q
    }

    #[test]
    fn rational_precedence_and_associativity() {
        assert_eq!(eval_int("1/3 + 1/6"), BigRational::new(1.into(), 2.into()));
        assert_eq!(eval_int("2+3*4"), BigRational::from_integer(14.into()));
        assert_eq!(eval_int("(2+3)*4"), BigRational::from_integer(20.into()));
        assert_eq!(eval_int("2^3^2"), BigRational::from_integer(512.into()));
        assert_eq!(eval_int("-3^2"), BigRational::from_integer((-9).into()));
        assert_eq!(eval_int("2^-2"), BigRational::new(1.into(), 4.into()));
    }

    #[test]
    fn division_by_zero_fail_visible() {
        let err = eval_expr(
            "1/0",
            ExprDialect {
                decimal_literals: false,
                units: false,
            },
            &no_units,
        )
        .expect_err("除零必须报错");
        assert!(err.contains("除以零"), "{err}");
    }

    #[test]
    fn decimal_parsing_and_string_roundtrip() {
        let (u, s) = parse_decimal("-12.3400").expect("解析应成功");
        assert_eq!((u.to_string(), s), ("-123400".to_string(), 4));
        assert_eq!(decimal_string(&u, s), "-12.3400");
        assert_eq!(decimal_string(&(1.into()), -3), "1000");
        assert_eq!(decimal_string(&(5.into()), 2), "0.05");
    }

    #[test]
    fn rounding_modes() {
        let third = BigRational::new(1.into(), 3.into());
        assert_eq!(
            round_to_scale(&third, 2, Rounding::HalfEven).expect("ok"),
            (33.into(), 2)
        );
        // 0.125 → half_even 到 2 位 = 0.12；half_up = 0.13
        let eighth = BigRational::new(1.into(), 8.into());
        assert_eq!(
            round_to_scale(&eighth, 2, Rounding::HalfEven)
                .expect("ok")
                .0
                .to_string(),
            "12"
        );
        assert_eq!(
            round_to_scale(&eighth, 2, Rounding::HalfUp)
                .expect("ok")
                .0
                .to_string(),
            "13"
        );
        // 负数 floor/trunc 分野
        let minus_half = BigRational::new((-1).into(), 2.into());
        assert_eq!(
            round_to_scale(&minus_half, 0, Rounding::Floor)
                .expect("ok")
                .0
                .to_string(),
            "-1"
        );
        assert_eq!(
            round_to_scale(&minus_half, 0, Rounding::Trunc)
                .expect("ok")
                .0
                .to_string(),
            "0"
        );
    }

    #[test]
    fn decimal_literal_expr_exact_then_round() {
        let v = eval_expr(
            "0.1 + 0.2",
            ExprDialect {
                decimal_literals: true,
                units: false,
            },
            &no_units,
        )
        .expect("求值应成功")
        .q;
        assert_eq!(
            v,
            BigRational::new(3.into(), 10.into()),
            "0.1+0.2 精确等于 3/10"
        );
    }
}

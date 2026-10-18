//! 原生服务实现：校验位三服务（JSON 入 / JSON 出，统一信封 v1）。
//!
//! # 确定性
//! 全程整数运算（IBAN mod-97 走大整数），无浮点、无时钟、无随机。
//! 同输入两次执行结果逐位一致。本族输出全部为精确整数/布尔/字符串，
//! 不产生舍入——按舍入纪律的适用条件，契约不含 scale/rounding。
//!
//! # 判定语义（重要）
//! 「校验位不匹配」是 `valid=false` 的**正常判定结果**，不是 error；
//! 「输入含非法字符/长度结构非法」才是 fail-visible error——判定与
//! 故障分离，调用方据 status 与 valid 两层分别处置。
//!
//! # 诚实边界
//! - 校验位算法只保证格式合法；号码是否真实签发/有效属外部事实；
//! - IBAN 国家长度表不内置（如实声明，长度合规由调用方按 ISO 13616
//!   各国注册表核对）；本服务自动剥离空格并大写。

#![forbid(unsafe_code)]

use std::sync::Arc;

use evorule_plugin_kit::NativeService;
use evorule_reactor::IoResult;
use evorule_tcb::JsonValue;
use num_bigint::BigInt;

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

fn envelope(pairs: &[(&str, JsonValue)]) -> JsonValue {
    let mut all: Vec<(&str, JsonValue)> = vec![
        ("status", JsonValue::string("determined")),
        ("determinism", JsonValue::string("bitwise_replayable")),
    ];
    all.extend_from_slice(pairs);
    JsonValue::object_from_pairs(&all)
}

/// 数字串校验（Luhn/ISBN 数字段共用），非法字符 fail-visible。
fn digits_only(s: &str, what: &str) -> Result<(), String> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "参数须为纯数字串({what}) — 自诊断指引: 剥离连字符/空格后重试"
        ));
    }
    Ok(())
}

// ============================================================================
// checksum_luhn — Luhn 模 10
// ============================================================================

/// Luhn 加权和：从最右位起（0 基），奇数位加倍（>9 减 9）后求和。
fn luhn_sum(digits_rev: &[u32]) -> u32 {
    digits_rev
        .iter()
        .enumerate()
        .map(|(i, &d)| {
            if !i.is_multiple_of(2) {
                let x = d * 2;
                if x > 9 {
                    x - 9
                } else {
                    x
                }
            } else {
                d
            }
        })
        .sum()
}

struct LuhnService;

impl NativeService for LuhnService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let mode = req_str(args, "mode")?;
        let raw = req_str(args, "value")?;
        let value = raw.trim();
        match mode {
            "validate" => {
                digits_only(value, "完整号码")?;
                if value.len() < 2 {
                    return Err(format!(
                        "号码长度 {} < 2 — 自诊断指引: Luhn 号码至少含载荷与校验位",
                        value.len()
                    ));
                }
                let rev: Vec<u32> = value.bytes().rev().map(|b| u32::from(b - b'0')).collect();
                let valid = luhn_sum(&rev).is_multiple_of(10);
                Ok(envelope(&[
                    ("mode", JsonValue::string(mode)),
                    ("valid", JsonValue::Bool(valid)),
                    ("length", JsonValue::Integer(value.len() as i64)),
                    ("algorithm", JsonValue::string("luhn-mod-10")),
                ]))
            }
            "append" => {
                digits_only(value, "载荷")?;
                let mut rev: Vec<u32> = value.bytes().rev().map(|b| u32::from(b - b'0')).collect();
                rev.insert(0, 0); // 校验位占位（0 基第 0 位），载荷位序右移一位
                let check = (10 - luhn_sum(&rev) % 10) % 10;
                Ok(envelope(&[
                    ("mode", JsonValue::string(mode)),
                    ("check_digit", JsonValue::Integer(check as i64)),
                    ("full", JsonValue::string(format!("{value}{check}"))),
                    ("algorithm", JsonValue::string("luhn-mod-10")),
                ]))
            }
            other => Err(format!(
                "未知 mode '{other}' — 自诊断指引: 合法值 validate/append"
            )),
        }
    }
}

pub fn mk_checksum_luhn() -> Arc<dyn NativeService> {
    Arc::new(LuhnService)
}

// ============================================================================
// checksum_isbn — ISBN-10 / ISBN-13
// ============================================================================

/// ISBN-10 校验和：Σ (10−k)·dₖ（k 为左起 0 基），合法 ⇔ mod 11 == 0。
fn isbn10_sum(digits: &[u32], check: u32) -> u32 {
    let mut sum = 0u32;
    for (k, &d) in digits.iter().enumerate() {
        sum += (10 - k as u32) * d;
    }
    sum + check
}

/// ISBN-13 校验和：交替权 1,3（左起），合法 ⇔ mod 10 == 0。
fn isbn13_sum(digits: &[u32], check: u32) -> u32 {
    let mut sum = 0u32;
    for (k, &d) in digits.iter().enumerate() {
        sum += if k.is_multiple_of(2) { d } else { 3 * d };
    }
    sum + check
}

struct IsbnService;

impl NativeService for IsbnService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let kind = req_str(args, "kind")?;
        let mode = req_str(args, "mode")?;
        let raw = req_str(args, "value")?;
        let value: String = raw
            .chars()
            .filter(|c| !matches!(c, '-' | ' '))
            .collect::<String>()
            .to_ascii_uppercase();
        match kind {
            "isbn10" => {
                if !value.chars().all(|c| c.is_ascii_digit() || c == 'X') {
                    return Err(format!(
                        "ISBN-10 '{raw}' 含非法字符 — 自诊断指引: 仅数字与校验位 X,连字符/空格自动剥离"
                    ));
                }
                match mode {
                    "validate" => {
                        if value.len() != 10 {
                            return Err(format!(
                                "ISBN-10 长度 {} ≠ 10 — 自诊断指引: 含校验位共 10 位",
                                value.len()
                            ));
                        }
                        let (body, check_ch) = value.split_at(9);
                        digits_only(body, "ISBN-10 前 9 位")?;
                        let digits: Vec<u32> = body.bytes().map(|b| u32::from(b - b'0')).collect();
                        let check = match check_ch.as_bytes()[0] {
                            b'X' => 10u32,
                            b => u32::from(b - b'0'),
                        };
                        let valid = isbn10_sum(&digits, check).is_multiple_of(11);
                        Ok(envelope(&[
                            ("kind", JsonValue::string(kind)),
                            ("mode", JsonValue::string(mode)),
                            ("valid", JsonValue::Bool(valid)),
                            ("algorithm", JsonValue::string("isbn10-mod-11")),
                        ]))
                    }
                    "check_digit" => {
                        digits_only(&value, "ISBN-10 前 9 位")?;
                        if value.len() != 9 {
                            return Err(format!(
                                "载荷长度 {} ≠ 9 — 自诊断指引: check_digit 模式传前 9 位",
                                value.len()
                            ));
                        }
                        let digits: Vec<u32> = value.bytes().map(|b| u32::from(b - b'0')).collect();
                        let check = (11 - isbn10_sum(&digits, 0) % 11) % 11;
                        let check_ch = if check == 10 { "X" } else { &check.to_string() };
                        Ok(envelope(&[
                            ("kind", JsonValue::string(kind)),
                            ("mode", JsonValue::string(mode)),
                            ("check_char", JsonValue::string(check_ch)),
                            ("full", JsonValue::string(format!("{value}{check_ch}"))),
                            ("algorithm", JsonValue::string("isbn10-mod-11")),
                        ]))
                    }
                    other => Err(format!(
                        "未知 mode '{other}' — 自诊断指引: 合法值 validate/check_digit"
                    )),
                }
            }
            "isbn13" => match mode {
                "validate" => {
                    digits_only(&value, "ISBN-13")?;
                    if value.len() != 13 {
                        return Err(format!(
                            "ISBN-13 长度 {} ≠ 13 — 自诊断指引: 含校验位共 13 位",
                            value.len()
                        ));
                    }
                    let digits: Vec<u32> = value.bytes().map(|b| u32::from(b - b'0')).collect();
                    let (body, check) = digits.split_at(12);
                    let valid = isbn13_sum(body, check[0]).is_multiple_of(10);
                    Ok(envelope(&[
                        ("kind", JsonValue::string(kind)),
                        ("mode", JsonValue::string(mode)),
                        ("valid", JsonValue::Bool(valid)),
                        ("algorithm", JsonValue::string("isbn13-mod10-1-3")),
                    ]))
                }
                "check_digit" => {
                    digits_only(&value, "ISBN-13 前 12 位")?;
                    if value.len() != 12 {
                        return Err(format!(
                            "载荷长度 {} ≠ 12 — 自诊断指引: check_digit 模式传前 12 位",
                            value.len()
                        ));
                    }
                    let digits: Vec<u32> = value.bytes().map(|b| u32::from(b - b'0')).collect();
                    let check = (10 - isbn13_sum(&digits, 0) % 10) % 10;
                    Ok(envelope(&[
                        ("kind", JsonValue::string(kind)),
                        ("mode", JsonValue::string(mode)),
                        ("check_digit", JsonValue::Integer(check as i64)),
                        ("full", JsonValue::string(format!("{value}{check}"))),
                        ("algorithm", JsonValue::string("isbn13-mod10-1-3")),
                    ]))
                }
                other => Err(format!(
                    "未知 mode '{other}' — 自诊断指引: 合法值 validate/check_digit"
                )),
            },
            other => Err(format!(
                "未知 kind '{other}' — 自诊断指引: 合法值 isbn10/isbn13"
            )),
        }
    }
}

pub fn mk_checksum_isbn() -> Arc<dyn NativeService> {
    Arc::new(IsbnService)
}

// ============================================================================
// checksum_iban — IBAN mod-97（ISO 13616）
// ============================================================================

/// IBAN 规整：剥空格、大写。返回 (完整串, 已规整) 。
fn iban_normalize(raw: &str) -> String {
    raw.chars()
        .filter(|c| !matches!(c, ' ' | '-'))
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// IBAN 字符 → mod-97 数字串（数字照抄,字母 A→10..Z→35）。
fn iban_to_number(s: &str) -> Result<String, String> {
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        if c.is_ascii_digit() {
            out.push(c);
        } else if c.is_ascii_uppercase() {
            let v = c as u32 - 'A' as u32 + 10;
            out.push_str(&v.to_string());
        } else {
            return Err(format!(
                "IBAN 含非法字符 '{c}' — 自诊断指引: 仅字母与数字,空格/连字符自动剥离"
            ));
        }
    }
    Ok(out)
}

struct IbanService;

impl NativeService for IbanService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let mode = req_str(args, "mode")?;
        let raw = req_str(args, "value")?;
        let value = iban_normalize(raw);
        match mode {
            "validate" => {
                if value.len() < 15 || value.len() > 34 {
                    return Err(format!(
                        "IBAN 长度 {} 超出 [15,34] — 自诊断指引: ISO 13616 规定总长区间",
                        value.len()
                    ));
                }
                let cc = &value[..2];
                if !cc.bytes().all(|b| b.is_ascii_uppercase()) {
                    return Err(format!(
                        "IBAN 前两位须为国家码字母,得到 '{cc}' — 自诊断指引: ISO 13616 结构 CC##BBAN"
                    ));
                }
                if !value[2..4].bytes().all(|b| b.is_ascii_digit()) {
                    return Err(format!(
                        "IBAN 第 3-4 位须为两位校验码数字,得到 '{}' — 自诊断指引: ISO 13616 结构 CC##BBAN",
                        &value[2..4]
                    ));
                }
                // 重排: BBAN + CC## → 视作大整数 → mod 97 == 1。
                let rearranged = format!("{}{}", &value[4..], &value[..4]);
                let n = BigInt::parse_bytes(iban_to_number(&rearranged)?.as_bytes(), 10)
                    .ok_or_else(|| "IBAN 数字化失败 — 自诊断指引: 检查字符集".to_string())?;
                let valid = (n % BigInt::from(97u32)) == BigInt::from(1u32);
                Ok(envelope(&[
                    ("mode", JsonValue::string(mode)),
                    ("valid", JsonValue::Bool(valid)),
                    ("country_code", JsonValue::string(cc)),
                    ("length", JsonValue::Integer(value.len() as i64)),
                    ("algorithm", JsonValue::string("iso13616-mod-97-10")),
                    ("note", JsonValue::string("结构+mod97 已校验;国家长度表不内置,长度合规请按 ISO 13616 各国注册表核对")),
                ]))
            }
            "check_digits" => {
                if value.len() < 5 {
                    return Err(format!(
                        "输入长度 {} 过短 — 自诊断指引: check_digits 模式传 国家码+基本账号(校验位位置传 00),如 DE00370400440532013000",
                        value.len()
                    ));
                }
                let cc = &value[..2];
                if !cc.bytes().all(|b| b.is_ascii_uppercase()) {
                    return Err(format!(
                        "前两位须为国家码字母,得到 '{cc}' — 自诊断指引: 如 DE/FR/GB"
                    ));
                }
                if !value[2..4].bytes().all(|b| b == b'0') {
                    return Err(
                        "第 3-4 位须为 00(补算位) — 自诊断指引: check_digits 模式校验位位置传 00"
                            .to_string(),
                    );
                }
                let rearranged = format!("{}{}00", &value[4..], cc);
                let n = BigInt::parse_bytes(iban_to_number(&rearranged)?.as_bytes(), 10)
                    .ok_or_else(|| "IBAN 数字化失败 — 自诊断指引: 检查字符集".to_string())?;
                let r = (n % BigInt::from(97u32))
                    .to_string()
                    .parse::<u32>()
                    .map_err(|e| format!("mod-97 残差解析失败: {e}"))?;
                let check = 98 - r;
                let full = format!("{}{:02}{}", cc, check, &value[4..]);
                Ok(envelope(&[
                    ("mode", JsonValue::string(mode)),
                    ("check_digits", JsonValue::string(format!("{check:02}"))),
                    ("full", JsonValue::string(full)),
                    ("algorithm", JsonValue::string("iso13616-mod-97-10")),
                    (
                        "note",
                        JsonValue::string(
                            "两位校验码 = 98 − (BBAN+国家码+00 的 mod 97);国家长度表不内置",
                        ),
                    ),
                ]))
            }
            other => Err(format!(
                "未知 mode '{other}' — 自诊断指引: 合法值 validate/check_digits"
            )),
        }
    }
}

pub fn mk_checksum_iban() -> Arc<dyn NativeService> {
    Arc::new(IbanService)
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

    fn bool_of(out: &JsonValue, key: &str) -> bool {
        out.get(key)
            .and_then(|v| match v {
                JsonValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{key} 应为布尔"))
    }

    fn int_of(out: &JsonValue, key: &str) -> i64 {
        out.get(key)
            .and_then(|v| match v {
                JsonValue::Integer(n) => Some(*n),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{key} 应为整数"))
    }

    /// 黄金向量：Luhn 规范示例 79927398713（有效）/79927398714（无效）；
    /// append 7992739871 → 校验位 3。
    #[test]
    fn luhn_golden() {
        let ok = LuhnService
            .execute(&args(&[
                ("mode", JsonValue::string("validate")),
                ("value", JsonValue::string("79927398713")),
            ]))
            .expect("Luhn 有效示例应成功");
        assert!(bool_of(&ok, "valid"));
        let bad = LuhnService
            .execute(&args(&[
                ("mode", JsonValue::string("validate")),
                ("value", JsonValue::string("79927398714")),
            ]))
            .expect("Luhn 无效示例应成功");
        assert!(!bool_of(&bad, "valid"), "篡改校验位应判无效");
        let app = LuhnService
            .execute(&args(&[
                ("mode", JsonValue::string("append")),
                ("value", JsonValue::string("7992739871")),
            ]))
            .expect("Luhn 补位应成功");
        assert_eq!(int_of(&app, "check_digit"), 3);
        assert_eq!(field(&app, "full"), "79927398713");
    }

    /// 黄金向量：ISBN-10 "0306406152" 有效 / X 校验位 "080442957X" 有效；
    /// ISBN-13 "9780306406157" 有效；补位 978030640615 → 7。
    #[test]
    fn isbn_golden() {
        let ok10 = IsbnService
            .execute(&args(&[
                ("kind", JsonValue::string("isbn10")),
                ("mode", JsonValue::string("validate")),
                ("value", JsonValue::string("0-306-40615-2")),
            ]))
            .expect("ISBN-10 有效示例应成功");
        assert!(bool_of(&ok10, "valid"), "连字符自动剥离");
        let x10 = IsbnService
            .execute(&args(&[
                ("kind", JsonValue::string("isbn10")),
                ("mode", JsonValue::string("validate")),
                ("value", JsonValue::string("080442957X")),
            ]))
            .expect("ISBN-10 X 位示例应成功");
        assert!(bool_of(&x10, "valid"), "X 校验位=10 应接受");
        let bad13 = IsbnService
            .execute(&args(&[
                ("kind", JsonValue::string("isbn13")),
                ("mode", JsonValue::string("validate")),
                ("value", JsonValue::string("9780306406158")),
            ]))
            .expect("ISBN-13 无效示例应成功");
        assert!(!bool_of(&bad13, "valid"));
        let app = IsbnService
            .execute(&args(&[
                ("kind", JsonValue::string("isbn13")),
                ("mode", JsonValue::string("check_digit")),
                ("value", JsonValue::string("978030640615")),
            ]))
            .expect("ISBN-13 补位应成功");
        assert_eq!(int_of(&app, "check_digit"), 7);
        assert_eq!(field(&app, "full"), "9780306406157");
    }

    /// 黄金向量：ISO 13616 规范示例 GB82 WEST 1234 5698 7654 32 有效；
    /// DE 补位 DE00 3704 0044 0532 0130 00 → 89。
    #[test]
    fn iban_golden() {
        let gb = IbanService
            .execute(&args(&[
                ("mode", JsonValue::string("validate")),
                ("value", JsonValue::string("GB82 WEST 1234 5698 7654 32")),
            ]))
            .expect("GB 示例应成功");
        assert!(bool_of(&gb, "valid"), "ISO 13616 规范示例应有效");
        assert_eq!(field(&gb, "country_code"), "GB");
        let de = IbanService
            .execute(&args(&[
                ("mode", JsonValue::string("check_digits")),
                ("value", JsonValue::string("DE00370400440532013000")),
            ]))
            .expect("DE 补位应成功");
        assert_eq!(field(&de, "check_digits"), "89");
        assert_eq!(field(&de, "full"), "DE89370400440532013000");
        // 非法字符 fail-visible。
        let err = IbanService
            .execute(&args(&[
                ("mode", JsonValue::string("validate")),
                ("value", JsonValue::string("GB82WEST1234569876543!")),
            ]))
            .expect_err("非法字符应报错");
        assert!(err.contains("非法字符"), "报错应点名非法字符: {err}");
    }
}

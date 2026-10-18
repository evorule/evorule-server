//! 原生服务实现：人民币大写金额转换（JSON 入 / JSON 出，统一信封 v1）。
//!
//! # 确定性
//! 纯字符串/整数运算，无浮点、无时钟、无随机。同输入两次执行逐位一致。
//! 输出为精确汉字串，不产生舍入——契约不含 scale/rounding。
//!
//! # 规则与确定性选择（见 lib.rs 头注）
//! 规范两可处取省略：元位/万位为零且角位非零时「零」省略（人行示例
//! 107000.53 的写法属可写可不写，本服务不写）；到角为止「整」省略。

#![forbid(unsafe_code)]

use std::sync::Arc;

use evorule_plugin_kit::NativeService;
use evorule_reactor::IoResult;
use evorule_tcb::JsonValue;

const DIGITS: [&str; 10] = ["零", "壹", "贰", "叁", "肆", "伍", "陆", "柒", "捌", "玖"];
const IN_GROUP: [&str; 4] = ["仟", "佰", "拾", ""];
const SECTIONS: [&str; 4] = ["元", "万", "亿", "万亿"];
/// 整数部分如实上限（16 位 = 万亿级）。
const MAX_INT_DIGITS: usize = 16;

// ============================================================================
// 参数提取助手（fail-visible 带自诊断指引）
// ============================================================================

fn envelope(pairs: &[(&str, JsonValue)]) -> JsonValue {
    let mut all: Vec<(&str, JsonValue)> = vec![
        ("status", JsonValue::string("determined")),
        ("determinism", JsonValue::string("bitwise_replayable")),
    ];
    all.extend_from_slice(pairs);
    JsonValue::object_from_pairs(&all)
}

// ============================================================================
// 金额 → 中文大写核心
// ============================================================================

/// 4 位组（0..=9999）转段内中文：段内零合并、尾零不写；0 → 空串。
fn group_to_cn(mut n: u32) -> String {
    let mut out = String::new();
    let mut need_zero = false;
    let powers = [1000u32, 100, 10, 1];
    for (pos, &p) in powers.iter().enumerate() {
        let d = n / p;
        n %= p;
        if d == 0 {
            if !out.is_empty() {
                need_zero = true;
            }
        } else {
            if need_zero {
                out.push_str(DIGITS[0]);
                need_zero = false;
            }
            out.push_str(DIGITS[d as usize]);
            out.push_str(IN_GROUP[pos]);
        }
    }
    out
}

/// 整数部分（数字串，无前导零要求）转中文：按 4 位分组跨段拼接。
fn integer_to_cn(digits: &str) -> String {
    let padded_len = digits.len().div_ceil(4) * 4;
    let padded = format!("{digits:0>width$}", width = padded_len);
    let bytes = padded.as_bytes();
    let group_count = padded_len / 4;
    let mut out = String::new();
    for gi in 0..group_count {
        let start = gi * 4;
        let val = (bytes[start] - b'0') as u32 * 1000
            + (bytes[start + 1] - b'0') as u32 * 100
            + (bytes[start + 2] - b'0') as u32 * 10
            + (bytes[start + 3] - b'0') as u32;
        let text = group_to_cn(val);
        if text.is_empty() {
            continue;
        }
        // 跨段零：本组非零但不足千位（高位为零）且前面已有更高段 → 补一个零
        // （覆盖 1 0000 0001 → 壹亿零壹元 一类；连续空段也只产生这一个零）。
        if !out.is_empty() && val < 1000 {
            out.push_str(DIGITS[0]);
        }
        out.push_str(&text);
        if gi + 1 < group_count {
            // 末组为元组,段名「元」由调用方统一追加,此处只补万/亿/万亿段名。
            out.push_str(SECTIONS[group_count - 1 - gi]);
        }
    }
    out
}

/// 金额字符串 → 票据中文大写。输入须为 `^\d+(\.\d{1,2})?$`。
fn amount_to_upper(amount: &str) -> Result<String, String> {
    let (int_part, frac_part) = match amount.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (amount, None),
    };
    if int_part.is_empty() || !int_part.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "金额 '{amount}' 整数部分非法 — 自诊断指引: 须为非负十进制字面量(如 1409.50)"
        ));
    }
    if let Some(f) = frac_part {
        if f.is_empty() || f.len() > 2 || !f.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!(
                "金额 '{amount}' 小数部分非法 — 自诊断指引: 最多两位小数(角/分)"
            ));
        }
    }
    let int_digits = int_part.trim_start_matches('0');
    let int_digits = if int_digits.is_empty() {
        "0"
    } else {
        int_digits
    };
    if int_digits.len() > MAX_INT_DIGITS {
        return Err(format!(
            "金额 '{amount}' 整数部分 {} 位超过如实上限 {MAX_INT_DIGITS} 位 — 自诊断指引: 本服务支持至万亿级(10^16),超限不静默截断",
            int_digits.len()
        ));
    }
    let (jiao, fen) = match frac_part {
        None => (0u8, 0u8),
        Some(f) => {
            let j = f.as_bytes()[0] - b'0';
            let fe = if f.len() == 2 {
                f.as_bytes()[1] - b'0'
            } else {
                0
            };
            (j, fe)
        }
    };

    let mut out = String::new();
    if int_digits == "0" {
        if jiao == 0 && fen == 0 {
            return Ok("零元整".to_string());
        }
        if jiao != 0 {
            out.push_str(DIGITS[jiao as usize]);
            out.push('角');
        }
        if fen != 0 {
            out.push_str(DIGITS[fen as usize]);
            out.push('分');
        }
        return Ok(out);
    }
    out.push_str(&integer_to_cn(int_digits));
    out.push('元');
    if jiao == 0 && fen == 0 {
        out.push('整');
    } else {
        if jiao != 0 {
            out.push_str(DIGITS[jiao as usize]);
            out.push('角');
        }
        if fen != 0 {
            if jiao == 0 {
                // 角位为零而分位非零:元后必写零(人行强制规则)。
                out.push_str(DIGITS[0]);
            }
            out.push_str(DIGITS[fen as usize]);
            out.push('分');
        }
    }
    Ok(out)
}

// ============================================================================
// 服务实现
// ============================================================================

struct AmountUpperService;

impl NativeService for AmountUpperService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let raw = match args.get("amount") {
            Some(v) => v.as_str().ok_or_else(|| {
                "参数 amount 须为字符串 — 自诊断指引: 见服务 parameters 契约".to_string()
            })?,
            None => return Err("缺少参数 amount — 自诊断指引: 见服务 parameters 契约".to_string()),
        };
        let amount = raw.trim();
        if amount.starts_with('-') || amount.starts_with('+') {
            return Err(format!(
                "金额 '{amount}' 带符号 — 自诊断指引: 票据金额为非负,负数如实拒解"
            ));
        }
        let upper = amount_to_upper(amount)?;
        let with_prefix = match args.get("currency_prefix") {
            Some(JsonValue::Bool(b)) => *b,
            Some(_) => {
                return Err("参数 currency_prefix 须为布尔".to_string());
            }
            None => false,
        };
        let final_text = if with_prefix {
            format!("人民币{upper}")
        } else {
            upper
        };
        let out = vec![
            ("upper", JsonValue::string(&final_text)),
            ("amount_normalized", JsonValue::string(amount)),
            (
                "note",
                JsonValue::string(
                    "按票据规范转换;规范两可处(元位/万位为零且角位非零时的「零」、到角为止的「整」)取省略为确定性选择",
                ),
            ),
        ];
        Ok(envelope(&out))
    }
}

pub fn mk_amount_upper() -> Arc<dyn NativeService> {
    Arc::new(AmountUpperService)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn args(pairs: &[(&str, JsonValue)]) -> JsonValue {
        JsonValue::object_from_pairs(pairs)
    }

    fn upper_of(amount: &str) -> String {
        let out = AmountUpperService
            .execute(&args(&[("amount", JsonValue::string(amount))]))
            .unwrap_or_else(|e| panic!("{amount} 应成功: {e}"));
        out.get("upper")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("upper 应存在"))
            .to_string()
    }

    /// 黄金向量：人行示例集 + 边界（零/壹拾/跨段零/大数/纯小数）。
    #[test]
    fn pboc_golden_examples() {
        // 中间一个零。
        assert_eq!(upper_of("1409.50"), "壹仟肆佰零玖元伍角");
        // 连续多个零只写一个零。
        assert_eq!(upper_of("6007.14"), "陆仟零柒元壹角肆分");
        // 元位为零、角位非零（规范两可处:本服务省略零）。
        assert_eq!(upper_of("1680.32"), "壹仟陆佰捌拾元叁角贰分");
        // 角位为零、分位非零:元后必写零。
        assert_eq!(upper_of("16409.02"), "壹万陆仟肆佰零玖元零贰分");
        assert_eq!(upper_of("325.04"), "叁佰贰拾伍元零肆分");
        // 到元为止写整;壹拾开头。
        assert_eq!(upper_of("10"), "壹拾元整");
        assert_eq!(upper_of("0"), "零元整");
        // 大数跨段:亿/万/元 三组。
        assert_eq!(
            upper_of("1234567890.12"),
            "壹拾贰亿叁仟肆佰伍拾陆万柒仟捌佰玖拾元壹角贰分"
        );
        // 中间整段为零 → 跨段一个零。
        assert_eq!(upper_of("100000001"), "壹亿零壹元整");
        // 纯小数。
        assert_eq!(upper_of("0.05"), "伍分");
        assert_eq!(upper_of("0.53"), "伍角叁分");
    }

    #[test]
    fn prefix_and_fail_visible() {
        let out = AmountUpperService
            .execute(&args(&[
                ("amount", JsonValue::string("1.20")),
                ("currency_prefix", JsonValue::Bool(true)),
            ]))
            .expect("带前缀应成功");
        assert_eq!(
            out.get("upper").and_then(|v| v.as_str()),
            Some("人民币壹元贰角")
        );
        // 负数 fail-visible。
        let err = AmountUpperService
            .execute(&args(&[("amount", JsonValue::string("-5"))]))
            .expect_err("负数应报错");
        assert!(err.contains("非负"), "报错应点名非负: {err}");
        // 三位小数 fail-visible。
        let err = AmountUpperService
            .execute(&args(&[("amount", JsonValue::string("1.234"))]))
            .expect_err("三位小数应报错");
        assert!(err.contains("两位小数"), "报错应点名小数位: {err}");
        // 超 16 位整数 fail-visible。
        let err = AmountUpperService
            .execute(&args(&[("amount", JsonValue::string("12345678901234567"))]))
            .expect_err("17 位整数应报错");
        assert!(err.contains("上限"), "报错应点名上限: {err}");
        // 非法字符 fail-visible。
        let err = AmountUpperService
            .execute(&args(&[("amount", JsonValue::string("12a3"))]))
            .expect_err("非法字符应报错");
        assert!(err.contains("非法"), "报错应点名非法: {err}");
    }
}

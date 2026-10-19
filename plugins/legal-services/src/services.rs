//! 原生服务实现：法律期限届日计算（JSON 入 / JSON 出，统一信封 v1）。
//!
//! # 确定性
//! 纯整数历法算术（civil 序数换算），无浮点、无时钟、无随机。
//! 同输入两次执行结果逐位一致。输出为日期串/整数，契约不含 scale/rounding
//! 以外的算术舍入——按舍入纪律适用条件诚实省略（保留参数位仅为家族契约
//! 统一，不参与计算）。
//!
//! # 语义（民事期间规则）
//! - **日起算不含当日**：effective_start = start_date + 1 天；
//! - **day/week**：effective_start + (period_days−1)（day）/ 7·weeks（week）；
//! - **month/year**：按历法对应日推进（effective_start 的日/月号对应到
//!   届期月）；对应日不存在（如 1/31 + 1 月、闏 2/29 + 非闰年）取该月
//!   最后一日；
//! - **顺延**：届日 ∈ holidays（显式入参）→ 顺延至首个非休假日；
//!   defer_weekends = true 时周末同顺延（部分程序期间仅顺延法定休假日）。

#![forbid(unsafe_code)]

use std::sync::Arc;

use evorule_plugin_kit::NativeService;
use evorule_reactor::IoResult;
use evorule_tcb::JsonValue;

// ============================================================================
// civil 历法算术（Howard Hinnant days_from_civil/civil_from_days,纯整数）
// ============================================================================

/// (y,m,d) → 自 1970-01-01 起的天数序数（支持负年份）。
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = y - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 天数序数 → (y,m,d)。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (y + if m <= 2 { 1 } else { 0 }, m, d)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

/// 星期几（0=周一..6=周日；1970-01-01 为周四=3）。
fn weekday_from_days(z: i64) -> u32 {
    ((z + 3).rem_euclid(7)) as u32
}

fn fmt_date(y: i64, m: u32, d: u32) -> String {
    format!("{y:04}-{m:02}-{d:02}")
}

fn parse_date(s: &str) -> Result<(i64, u32, u32), String> {
    let bytes = s.as_bytes();
    let valid_shape = s.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[..4].iter().all(|b| b.is_ascii_digit())
        && bytes[5..7].iter().all(|b| b.is_ascii_digit())
        && bytes[8..10].iter().all(|b| b.is_ascii_digit());
    if !valid_shape {
        return Err(format!("日期 '{s}' 格式非法 — 自诊断指引: 须为 YYYY-MM-DD"));
    }
    let y: i64 = s[..4].parse().unwrap_or(0);
    let m: u32 = s[5..7].parse().unwrap_or(0);
    let d: u32 = s[8..10].parse().unwrap_or(0);
    if !(1..=12).contains(&m) {
        return Err(format!("日期 '{s}' 月份 {m} 非法 — 自诊断指引: 01-12"));
    }
    if d == 0 || d > days_in_month(y, m) {
        return Err(format!(
            "日期 '{s}' 日 {d} 非法 — 自诊断指引: 该月天数为 {}",
            days_in_month(y, m)
        ));
    }
    Ok((y, m, d))
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
// legal_deadline — 期间届日计算
// ============================================================================

struct DeadlineService;

impl NativeService for DeadlineService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let start_str = match args.get("start_date") {
            Some(v) => v
                .as_str()
                .ok_or_else(|| "参数 start_date 须为字符串".to_string())?,
            None => {
                return Err("缺少参数 start_date — 自诊断指引: 见服务 parameters 契约".to_string())
            }
        };
        let period = match args.get("period_days") {
            Some(JsonValue::Integer(n)) if *n > 0 => *n,
            Some(JsonValue::Integer(n)) => {
                return Err(format!(
                    "参数 period_days={n} 须为正整数 — 自诊断指引: 期间为零或负无计算语义"
                ))
            }
            Some(_) => return Err("参数 period_days 须为整数".to_string()),
            None => {
                return Err("缺少参数 period_days — 自诊断指引: 见服务 parameters 契约".to_string())
            }
        };
        let unit = match args.get("unit") {
            Some(v) => v
                .as_str()
                .ok_or_else(|| "参数 unit 须为字符串".to_string())?,
            None => return Err("缺少参数 unit — 自诊断指引: day/week/month/year".to_string()),
        };
        if !matches!(unit, "day" | "week" | "month" | "year") {
            return Err(format!(
                "未知 unit '{unit}' — 自诊断指引: 合法值 day/week/month/year"
            ));
        }
        let defer_weekends = match args.get("defer_weekends") {
            Some(JsonValue::Bool(b)) => *b,
            Some(_) => return Err("参数 defer_weekends 须为布尔".to_string()),
            None => false,
        };
        let mut holidays: Vec<String> = Vec::new();
        if let Some(JsonValue::Array(items)) = args.get("holidays") {
            for (i, v) in items.iter().enumerate() {
                let s = v
                    .as_str()
                    .ok_or_else(|| format!("holidays[{i}] 须为字符串日期"))?;
                parse_date(s).map_err(|e| format!("holidays[{i}] {e}"))?;
                holidays.push(s.to_string());
            }
        }
        let (sy, sm, sd) = parse_date(start_str)?;
        let start_ord = days_from_civil(sy, sm, sd);
        // 日起算不含当日:自下一日起算。
        let eff = start_ord + 1;
        let (deadline_ord, unit_days) = match unit {
            "day" => (eff + period - 1, period),
            "week" => (eff + 7 * period - 1, 7 * period),
            "month" | "year" => {
                // 历法对应日推进:对应日锚起算日日号(start_date 的 d),
                // 月/年推进 period 个单位;对应日不存在取届期月最后一日。
                let total = if unit == "month" {
                    (sy * 12 + sm as i64 - 1) + period
                } else {
                    sy + period
                };
                let (ty, tm) = if unit == "month" {
                    (total / 12, (total % 12 + 1) as u32)
                } else {
                    (total, sm)
                };
                let last = days_in_month(ty, tm);
                let td = sd.min(last);
                (days_from_civil(ty, tm, td), 0)
            }
            _ => unreachable!("unit 已校验"),
        };
        // 顺延:届日 ∈ holidays → 顺延;defer_weekends → 周末同顺延。
        let mut final_ord = deadline_ord;
        let mut deferred = false;
        loop {
            let (fy, fm, fd) = civil_from_days(final_ord);
            let date_str = fmt_date(fy, fm, fd);
            let is_holiday = holidays.iter().any(|h| h == &date_str);
            let is_weekend = weekday_from_days(final_ord) >= 5;
            if is_holiday || (defer_weekends && is_weekend) {
                final_ord += 1;
                deferred = true;
            } else {
                break;
            }
        }
        let (fy, fm, fd) = civil_from_days(final_ord);
        let effective_start = {
            let (y, m, d) = civil_from_days(eff);
            fmt_date(y, m, d)
        };
        let raw = {
            let (y, m, d) = civil_from_days(deadline_ord);
            fmt_date(y, m, d)
        };
        let weekday_names = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
        let out = vec![
            ("effective_start", JsonValue::string(&effective_start)),
            ("deadline_raw", JsonValue::string(&raw)),
            ("deadline", JsonValue::string(fmt_date(fy, fm, fd))),
            ("deadline_weekday", JsonValue::string(weekday_names[weekday_from_days(final_ord) as usize])),
            ("deferred", JsonValue::Bool(deferred)),
            ("unit_days_equivalent", JsonValue::Integer(unit_days)),
            ("period_days", JsonValue::Integer(period)),
            (
                "note",
                JsonValue::string(
                    "起算日不计入自下一日起算;月/年期间到对应日,对应日不存在取当月末;\
                     届日为法定休假日顺延至休假日结束的次日;节假日表显式入参不内置;本结果为历法事实不构成法律意见",
                ),
            ),
        ];
        Ok(envelope(&out))
    }
}

pub fn mk_deadline() -> Arc<dyn NativeService> {
    Arc::new(DeadlineService)
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

    fn run(start: &str, days: i64, unit: &str) -> JsonValue {
        DeadlineService
            .execute(&args(&[
                ("start_date", JsonValue::string(start)),
                ("period_days", JsonValue::Integer(days)),
                ("unit", JsonValue::string(unit)),
                ("scale", JsonValue::Integer(0)),
                ("rounding", JsonValue::string("half_even")),
            ]))
            .unwrap_or_else(|e| panic!("{start}+{days}{unit} 应成功: {e}"))
    }

    /// 黄金向量：日起算不含当日（1/1 起 30 日 → 1/31）；
    /// 月对应日（1/15 + 1 月 → 2/15）；月末收敛（1/31 + 1 月 → 2/28）；
    /// 闰年对应日（2/29 + 2 年 → 2/28）；周末顺延。
    #[test]
    fn deadline_golden() {
        // 30 日:1/1 起算不含,自 1/2 计 30 日 → 1/31。
        assert_eq!(
            field(&run("2026-01-01", 30, "day"), "deadline"),
            "2026-01-31"
        );
        assert_eq!(
            field(&run("2026-01-01", 30, "day"), "effective_start"),
            "2026-01-02"
        );
        // 月对应日。
        assert_eq!(
            field(&run("2026-01-15", 1, "month"), "deadline"),
            "2026-02-15"
        );
        // 对应日不存在 → 当月末(2026 非闰年)。
        assert_eq!(
            field(&run("2026-01-31", 1, "month"), "deadline"),
            "2026-02-28"
        );
        // 闰日 + 2 年 → 非闰年收敛。
        assert_eq!(
            field(&run("2024-02-29", 2, "year"), "deadline"),
            "2026-02-28"
        );
        // 周:1 周 = 7 日。
        assert_eq!(
            field(&run("2026-01-01", 1, "week"), "deadline"),
            "2026-01-08"
        );
        // period 非正 fail-visible。
        let err = DeadlineService
            .execute(&args(&[
                ("start_date", JsonValue::string("2026-01-01")),
                ("period_days", JsonValue::Integer(0)),
                ("unit", JsonValue::string("day")),
                ("scale", JsonValue::Integer(0)),
                ("rounding", JsonValue::string("half_even")),
            ]))
            .expect_err("period=0 应报错");
        assert!(err.contains("正整数"), "报错应点名正整数: {err}");
        // 非法日期 fail-visible。
        let err = DeadlineService
            .execute(&args(&[
                ("start_date", JsonValue::string("2026-02-30")),
                ("period_days", JsonValue::Integer(1)),
                ("unit", JsonValue::string("day")),
                ("scale", JsonValue::Integer(0)),
                ("rounding", JsonValue::string("half_even")),
            ]))
            .expect_err("2/30 应报错");
        assert!(err.contains("非法"), "报错应点名日期非法: {err}");
    }

    /// 顺延黄金向量：2026-01-04 为周日 → 周末顺延至 01-05（周一）；
    /// 节假日表 [01-01,01-02] + 周末顺延:届日 01-01 → 01-05。
    #[test]
    fn deferral_golden() {
        // 2026-01-01 起算 3 日 → 届日 01-04(周日);defer_weekends → 01-05。
        let out = DeadlineService
            .execute(&args(&[
                ("start_date", JsonValue::string("2026-01-01")),
                ("period_days", JsonValue::Integer(3)),
                ("unit", JsonValue::string("day")),
                ("defer_weekends", JsonValue::Bool(true)),
                ("scale", JsonValue::Integer(0)),
                ("rounding", JsonValue::string("half_even")),
            ]))
            .expect("周末顺延应成功");
        assert_eq!(field(&out, "deadline_raw"), "2026-01-04");
        assert_eq!(field(&out, "deadline"), "2026-01-05");
        assert_eq!(field(&out, "deadline_weekday"), "Mon");
        assert_eq!(
            out.get("deferred").and_then(|v| v.as_bool()),
            Some(true),
            "应标记已顺延"
        );
        // 节假日表顺延:届日 01-01 ∈ 表 → 01-02 亦 ∈ 表 → 周末 → 01-05。
        let out = DeadlineService
            .execute(&args(&[
                ("start_date", JsonValue::string("2025-12-30")),
                ("period_days", JsonValue::Integer(2)),
                ("unit", JsonValue::string("day")),
                ("defer_weekends", JsonValue::Bool(true)),
                (
                    "holidays",
                    JsonValue::Array(vec![
                        JsonValue::string("2026-01-01"),
                        JsonValue::string("2026-01-02"),
                    ]),
                ),
                ("scale", JsonValue::Integer(0)),
                ("rounding", JsonValue::string("half_even")),
            ]))
            .expect("节假日顺延应成功");
        assert_eq!(field(&out, "deadline_raw"), "2026-01-01");
        assert_eq!(field(&out, "deadline"), "2026-01-05");
        // 不开周末顺延:01-03(周六)/01-04(周日)不休 → 01-05 因表内 01-01/02 已过,
        // 01-03 周六不在表且 defer_weekends=false → 停在 01-03? 否:01-01 在表,
        // 顺延一日到 01-02(在表)→01-03(周六,不在表,不顺)→ 01-03。
        let out = DeadlineService
            .execute(&args(&[
                ("start_date", JsonValue::string("2025-12-30")),
                ("period_days", JsonValue::Integer(2)),
                ("unit", JsonValue::string("day")),
                (
                    "holidays",
                    JsonValue::Array(vec![
                        JsonValue::string("2026-01-01"),
                        JsonValue::string("2026-01-02"),
                    ]),
                ),
                ("scale", JsonValue::Integer(0)),
                ("rounding", JsonValue::string("half_even")),
            ]))
            .expect("仅节假日顺延应成功");
        assert_eq!(
            field(&out, "deadline"),
            "2026-01-03",
            "周六不在表且不顺周末"
        );
    }
}

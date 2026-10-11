// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Rust UDF 示例：`udf_date_diff_days`（两日期相差天数）
//!
//! # 契约
//! - 入参 JSON：`{"from": "2026-01-01", "to": "2026-03-01"}`
//! - 出参 JSON：`{"from":"2026-01-01","to":"2026-03-01","days":59}`
//! - 日期形态：`YYYY-MM-DD`（公历，严格校验含闰年）；days = to - from（可为负）
//! - **确定性**：纯计算；"今天"类相对时间一律由调用方显式传入日期，本 UDF 不读时钟
//!
//! ⚠️ 示例简化：分配的结果内存不释放（同税额示例口径）。

use std::alloc::Layout;

#[no_mangle]
pub extern "C" fn alloc(len: i32) -> i32 {
    if len <= 0 {
        return 0;
    }
    let layout = match Layout::from_size_align(len as usize, 1) {
        Ok(l) => l,
        Err(_) => return 0,
    };
    let ptr = unsafe { std::alloc::alloc(layout) };
    ptr as i32
}

#[no_mangle]
pub extern "C" fn udf(ptr: i32, len: i32) -> i64 {
    if ptr <= 0 || len <= 0 {
        return 0;
    }
    let input = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let out = match compute(input) {
        Ok(json) => json,
        Err(e) => format!("{{\"error\":\"{e}\"}}").into_bytes(),
    };
    write_out(&out)
}

/// 公历日期 → 天数序数（Howard Hinnant civil_from_days 逆运算，日期域 [0000-03-01, ∞)）
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400; // [0, 399]
    let mp = ((m + 9) % 12) as i64; // 三月起算 [0, 11]
    let doy = (153 * mp + 2) / 5 + (d as i64) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 严格解析 `YYYY-MM-DD`（含闰年与月日域校验，fail-visible）
fn parse_date(s: &str) -> Result<(i64, u32, u32), String> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 {
        return Err(format!("日期形态非法 `{s}`（要求 YYYY-MM-DD）"));
    }
    let y: i64 = parts[0]
        .parse()
        .map_err(|_| format!("年份非法 `{}`", parts[0]))?;
    let m: u32 = parts[1]
        .parse()
        .map_err(|_| format!("月份非法 `{}`", parts[1]))?;
    let d: u32 = parts[2]
        .parse()
        .map_err(|_| format!("日非法 `{}`", parts[2]))?;
    if !(1..=12).contains(&m) {
        return Err(format!("月份越界 `{s}`（[1,12]）"));
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let dim = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if d < 1 || d > dim[(m - 1) as usize] {
        return Err(format!("日越界 `{s}`（该月最多 {} 天）", dim[(m - 1) as usize]));
    }
    Ok((y, m, d))
}

fn compute(input: &[u8]) -> Result<Vec<u8>, String> {
    let v: serde_json::Value =
        serde_json::from_slice(input).map_err(|e| format!("入参不是合法 JSON: {e}"))?;
    let from = v
        .get("from")
        .and_then(|x| x.as_str())
        .ok_or_else(|| "缺少 from（YYYY-MM-DD 字符串）".to_string())?;
    let to = v
        .get("to")
        .and_then(|x| x.as_str())
        .ok_or_else(|| "缺少 to（YYYY-MM-DD 字符串）".to_string())?;

    let (fy, fm, fd) = parse_date(from)?;
    let (ty, tm, td) = parse_date(to)?;
    let days = days_from_civil(ty, tm, td) - days_from_civil(fy, fm, fd);

    let result = serde_json::json!({
        "from": from,
        "to": to,
        "days": days,
    });
    serde_json::to_vec(&result).map_err(|e| format!("序列化失败: {e}"))
}

fn write_out(bytes: &[u8]) -> i64 {
    let p = alloc(bytes.len() as i32);
    if p <= 0 {
        return 0;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len());
    }
    ((p as i64) << 32) | (bytes.len() as i64 & 0xFFFF_FFFF)
}

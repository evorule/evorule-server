// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Rust UDF 示例：`udf_text_normalize`（字符串规范化）
//!
//! # 契约
//! - 入参 JSON：`{"text": "  Hello   World  "}`
//! - 出参 JSON：`{"normalized":"hello world","length":12}`
//! - 规范化语义：去首尾空白 → 转小写 → 连续空白折叠为单个空格
//! - **确定性**：纯计算，无时间/随机/环境依赖
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

fn compute(input: &[u8]) -> Result<Vec<u8>, String> {
    let v: serde_json::Value =
        serde_json::from_slice(input).map_err(|e| format!("入参不是合法 JSON: {e}"))?;
    let text = v
        .get("text")
        .and_then(|x| x.as_str())
        .ok_or_else(|| "缺少 text（字符串）".to_string())?;

    // 去首尾 → 小写 → 空白序列折叠为单空格
    let trimmed = text.trim();
    let mut out = String::with_capacity(trimmed.len());
    let mut in_space = false;
    for ch in trimmed.chars() {
        if ch.is_whitespace() {
            if !in_space && !out.is_empty() {
                out.push(' ');
            }
            in_space = true;
        } else {
            in_space = false;
            for lc in ch.to_lowercase() {
                out.push(lc);
            }
        }
    }
    let normalized = out.trim_end().to_string();

    let result = serde_json::json!({
        "normalized": normalized,
        "length": normalized.chars().count(),
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

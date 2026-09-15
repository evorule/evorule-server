// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Rust UDF 示例：`udf_finance_tax_calc`（77 号阶段 2 · V4）
//!
//! # ABI（与 `evorule-wasm-host` 的约定）
//! - `memory`：线性内存（cdylib 到 `wasm32-unknown-unknown` 时由工具链导出）
//! - `alloc(len: i32) -> i32`：为入参分配空间
//! - `udf(ptr: i32, len: i32) -> i64`：`(结果指针 << 32) | 结果长度`
//!
//! # 契约
//! - 入参 JSON：`{"amount": 1000, "rate": 0.06}`
//! - 出参 JSON：`{"tax": 60, "total": 1060}`
//! - **确定性**：纯计算，不读时间/随机/环境（host 也不提供这类 import）。
//!
//! ⚠️ 示例简化：分配的结果内存**不释放**（进程短生命周期场景可接受）；
//! 生产模块应配套 `dealloc` 并由 host 侧在读取后回收。

use std::alloc::Layout;

/// 分配 `len` 字节，返回指针（0 = 失败）
///
/// 注：本函数与 `std::alloc::alloc` 同名，故下方用**全路径**调用标准库版本，避免 E0255。
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

/// UDF 入口：读入参 → 计算 → 写出参
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

/// 业务逻辑：税额计算
fn compute(input: &[u8]) -> Result<Vec<u8>, String> {
    let v: serde_json::Value =
        serde_json::from_slice(input).map_err(|e| format!("入参不是合法 JSON: {e}"))?;

    let amount = v
        .get("amount")
        .and_then(|x| x.as_f64())
        .ok_or_else(|| "缺少 amount（数字）".to_string())?;
    let rate = v
        .get("rate")
        .and_then(|x| x.as_f64())
        .ok_or_else(|| "缺少 rate（数字）".to_string())?;

    if rate < 0.0 || rate > 1.0 {
        return Err("rate 必须位于 [0,1]".to_string());
    }

    // 四舍五入到分，避免浮点尾差导致输出不确定
    let tax = (amount * rate * 100.0).round() / 100.0;
    let total = ((amount + tax) * 100.0).round() / 100.0;

    let result = serde_json::json!({ "tax": tax, "total": total, "rate": rate });
    serde_json::to_vec(&result).map_err(|e| format!("序列化失败: {e}"))
}

/// 把结果写入新分配的内存并打包返回
fn write_out(bytes: &[u8]) -> i64 {
    let p = alloc(bytes.len() as i32);
    if p <= 0 {
        return 0;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len());
    }
    (((p as u64) << 32) | (bytes.len() as u64)) as i64
}

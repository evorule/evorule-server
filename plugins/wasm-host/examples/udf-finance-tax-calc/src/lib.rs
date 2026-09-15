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
//! - 入参 JSON：`{"amount_cents": 100000, "rate_bp": 600}`（1000.00 元 × 6%）
//! - 出参 JSON：`{"amount_cents":100000,"rate_bp":600,"tax_cents":6000,"total_cents":106000}`
//! - **确定性**：纯计算，不读时间/随机/环境（host 也不提供这类 import）。
//!
//! # ⚠️ 为何是整数定点（金额分 + 税率基点），不是浮点
//! evorule TCB 的 `JsonValue` **没有 Float 变体**（确定性红线：浮点尾差会让
//! 相同输入在不同平台产出不同审计哈希）。REST invoke 与规则 `call_service`
//! 路径都要经过 `JsonValue`，故**浮点入参无法抵达 UDF**：非整数 JSON number
//! 会被降级为字符串，`0.06` 到 guest 手里是 `"0.06"`。
//! 实测证据：`{"amount":1000,"rate":0.06}` 经 server invoke → guest 报
//! `缺少 rate（数字）`；改整数 `{"amount_cents":100000,"rate_bp":600}` 即通。
//! 故金额一律用**最小货币单位（分）**、税率用**基点（1bp = 0.01%）**——这本来
//! 也是金融治理的正确姿势，浮点乘法不该出现在计税口径里。
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

/// 业务逻辑：税额计算（整数定点：金额=分，税率=基点）
///
/// `tax_cents = round_half_up(amount_cents * rate_bp / 10_000)`
fn compute(input: &[u8]) -> Result<Vec<u8>, String> {
    let v: serde_json::Value =
        serde_json::from_slice(input).map_err(|e| format!("入参不是合法 JSON: {e}"))?;

    let amount_cents = v
        .get("amount_cents")
        .and_then(|x| x.as_i64())
        .ok_or_else(|| "缺少 amount_cents（整数，单位：分）".to_string())?;
    let rate_bp = v
        .get("rate_bp")
        .and_then(|x| x.as_i64())
        .ok_or_else(|| "缺少 rate_bp（整数，单位：基点，600 = 6%）".to_string())?;

    if amount_cents < 0 {
        return Err("amount_cents 不得为负".to_string());
    }
    if !(0..=10_000).contains(&rate_bp) {
        return Err("rate_bp 必须位于 [0,10000]".to_string());
    }

    // i128 中间运算防溢出：万亿级金额 × 万级基点仍在 i64 之外，i128 才安全
    let tax_cents = round_half_up(amount_cents as i128 * rate_bp as i128, 10_000);
    let total_cents = amount_cents as i128 + tax_cents;
    if total_cents > i64::MAX as i128 {
        return Err("金额溢出 i64".to_string());
    }

    let result = serde_json::json!({
        "amount_cents": amount_cents,
        "rate_bp": rate_bp,
        "tax_cents": tax_cents,
        "total_cents": total_cents,
    });
    serde_json::to_vec(&result).map_err(|e| format!("序列化失败: {e}"))
}

/// `(numer / denom)` 四舍五入（非负输入，round-half-up）
fn round_half_up(numer: i128, denom: i128) -> i128 {
    (numer + denom / 2) / denom
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

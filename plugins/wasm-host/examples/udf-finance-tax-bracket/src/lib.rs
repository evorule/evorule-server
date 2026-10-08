// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Rust UDF 示例：`udf_finance_tax_bracket`（税档级联计算）
//!
//! 与同目录 `udf-finance-tax-calc`（单税率平坦计算）互补：本例演示
//! **分段级联 + 结构化数组出参**——超出简单四则运算的复杂逻辑走 UDF 形态，
//! 且档位表可作入参（数据驱动），同一段代码算任意税表，口径更新 = 重发
//! `.wasm` 模块（部署面动作），无需改动调用方工作流结构。
//!
//! # ABI（与 `evorule-wasm-host` 的约定，同范本 `udf-finance-tax-calc`）
//! - `memory`：线性内存（cdylib 到 `wasm32-unknown-unknown` 时由工具链导出）
//! - `alloc(len: i32) -> i32`：为入参分配空间
//! - `udf(ptr: i32, len: i32) -> i64`：`(结果指针 << 32) | 结果长度`
//!
//! # 契约
//! - 入参 JSON：
//!   `{"taxable_cents": 50000000}`，或带自定义档位表：
//!   `{"taxable_cents": 1500, "brackets": [{"from_cents":0,"to_cents":1000,"rate_bp":1000},{"from_cents":1000,"to_cents":null,"rate_bp":2000}]}`
//!   - `taxable_cents`：必填整数，年度应税所得，单位：分，>= 0；
//!   - `brackets`：可选档位表数组，元素 `{from_cents, to_cents, rate_bp}`，
//!     `to_cents = null` 表示顶档；缺省 = 内置 7 档年化综合所得税率表
//!     （3%/10%/20%/25%/30%/35%/45%，**示例口径，非生产费率表**）。
//! - 出参 JSON（键序由序列化器排序，语义如下）：
//!   - `taxable_cents`：回显入参；
//!   - `brackets_used`：触及档数（= breakdown 长度，税基 > 0 的档位才列入）；
//!   - `tax_cents`：总税额（分）；
//!   - `effective_rate_bp`：`round_half_up(tax_cents * 10000 / taxable_cents)`，
//!     `taxable_cents = 0` 时定义为 0；
//!   - `breakdown`：逐档明细
//!     `{from_cents, to_cents, rate_bp, taxable_in_bracket_cents, tax_cents}`。
//! - 校验 fail-fast（无静默回退），任何违约 → 出参 `{"error":"..."}` → 宿主 422：
//!   taxable_cents 缺失/非整数/负数；brackets 非数组/空数组/元素缺字段/
//!   from_cents 负数/from >= to/rate_bp 越界/首档 from_cents != 0/
//!   顶档缺失或非唯一/顶档不在末位/档位断裂或交叉（后档 from 必须 = 前档 to）。
//!
//! # ⚠️ 为何是整数定点（金额分 + 税率基点），不是浮点
//! 与范本同理：evorule TCB 的 `JsonValue` **没有 Float 变体**（确定性红线），
//! 浮点入参会被降级成字符串根本到不了 UDF。金额一律用**最小货币单位（分）**、
//! 税率用**基点（1bp = 0.01%）**，中间运算一律 **i128** 防溢出。
//! 段内税 = `round_half_up(段内税基 * rate_bp / 10000)`，逐段四舍五入到分后求和。
//!
//! ⚠️ 示例简化：分配的结果内存**不释放**（每次调用新建实例，内存随实例回收，
//! 见 WASM_UDF_GUIDE §二）。

use std::alloc::Layout;

/// 一档位：`(from_cents, to_cents（None = 顶档）, rate_bp)`
type Bracket = (i64, Option<i64>, i64);

/// 内置缺省档位表：7 档年化综合所得税率（单位：分 / 基点）
///
/// ⚠️ 示例口径，非生产费率表；生产口径经 `brackets` 入参替换（数据驱动，不改代码）。
/// 语义：from 含、to 含，段内税基 = `min(taxable, to) - from`。
const DEFAULT_BRACKETS: [Bracket; 7] = [
    (0, Some(3_600_000), 300),             // 0 ~ 3.6 万元 @ 3%
    (3_600_000, Some(14_400_000), 1_000),  // 3.6 ~ 14.4 万元 @ 10%
    (14_400_000, Some(30_000_000), 2_000), // 14.4 ~ 30 万元 @ 20%
    (30_000_000, Some(42_000_000), 2_500), // 30 ~ 42 万元 @ 25%
    (42_000_000, Some(66_000_000), 3_000), // 42 ~ 66 万元 @ 30%
    (66_000_000, Some(96_000_000), 3_500), // 66 ~ 96 万元 @ 35%
    (96_000_000, None, 4_500),             // 96 万元以上 @ 45%（顶档）
];

/// 分配 `len` 字节，返回指针（0 = 失败）
///
/// 注：本函数与 `std::alloc::alloc` 同名，故函数体内用**全路径**调用标准库版本，避免 E0255。
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

/// 业务逻辑：税档级联（整数定点：金额=分，税率=基点，中间运算 i128）
fn compute(input: &[u8]) -> Result<Vec<u8>, String> {
    let v: serde_json::Value =
        serde_json::from_slice(input).map_err(|e| format!("入参不是合法 JSON: {e}"))?;

    let taxable_cents = v
        .get("taxable_cents")
        .and_then(|x| x.as_i64())
        .ok_or_else(|| "缺少 taxable_cents（整数，单位：分）".to_string())?;
    if taxable_cents < 0 {
        return Err("taxable_cents 不得为负".to_string());
    }

    let brackets = parse_brackets(v.get("brackets"))?;

    // 分段级联：逐段 min(taxable, to) - from 为段内税基（档位表已校验升序无断裂）
    let mut breakdown: Vec<serde_json::Value> = Vec::new();
    let mut tax_total: i128 = 0;
    for (from, to, rate_bp) in &brackets {
        let (from, to, rate_bp) = (*from, *to, *rate_bp);
        if taxable_cents <= from {
            break; // 本档及后续档位全部未触及
        }
        let seg_top = match to {
            Some(t) => t.min(taxable_cents),
            None => taxable_cents, // 顶档：税基顶到 taxable 本身
        };
        let seg = (seg_top - from) as i128; // > 0：taxable > from 且 to > from
        let seg_tax = round_half_up(seg * rate_bp as i128, 10_000);
        tax_total += seg_tax;
        let to_val = match to {
            Some(t) => serde_json::Value::from(t),
            None => serde_json::Value::Null,
        };
        breakdown.push(serde_json::json!({
            "from_cents": from,
            "to_cents": to_val,
            "rate_bp": rate_bp,
            "taxable_in_bracket_cents": seg,
            "tax_cents": seg_tax,
        }));
    }

    if tax_total > i64::MAX as i128 {
        return Err("税额溢出 i64".to_string());
    }
    let taxable = taxable_cents as i128;
    let effective_rate_bp = if taxable == 0 {
        0
    } else {
        round_half_up(tax_total * 10_000, taxable)
    };

    let result = serde_json::json!({
        "taxable_cents": taxable_cents,
        "brackets_used": breakdown.len() as u64,
        "tax_cents": tax_total as i64,
        "effective_rate_bp": effective_rate_bp as i64,
        "breakdown": breakdown,
    });
    serde_json::to_vec(&result).map_err(|e| format!("序列化失败: {e}"))
}

/// 解析并校验档位表（缺省 / null = 内置 7 档）
///
/// 校验语义见模块文档「契约」节——全部显式报错，无静默回退。
fn parse_brackets(v: Option<&serde_json::Value>) -> Result<Vec<Bracket>, String> {
    let val = match v {
        None => return Ok(DEFAULT_BRACKETS.to_vec()),
        Some(x) if x.is_null() => return Ok(DEFAULT_BRACKETS.to_vec()),
        Some(x) => x,
    };
    let arr = val
        .as_array()
        .ok_or_else(|| "brackets 必须是档位数组".to_string())?;
    if arr.is_empty() {
        return Err("brackets 不得为空数组".to_string());
    }

    let mut brackets: Vec<Bracket> = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let obj = item
            .as_object()
            .ok_or_else(|| format!("brackets[{i}] 必须是对象"))?;
        let from = obj
            .get("from_cents")
            .and_then(|x| x.as_i64())
            .ok_or_else(|| format!("brackets[{i}].from_cents 缺失或非整数"))?;
        let to = match obj.get("to_cents") {
            None => return Err(format!("brackets[{i}].to_cents 缺失（整数或 null=顶档）")),
            Some(x) if x.is_null() => None,
            Some(x) => Some(
                x.as_i64()
                    .ok_or_else(|| format!("brackets[{i}].to_cents 必须是整数或 null"))?,
            ),
        };
        let rate = obj
            .get("rate_bp")
            .and_then(|x| x.as_i64())
            .ok_or_else(|| format!("brackets[{i}].rate_bp 缺失或非整数"))?;

        if from < 0 {
            return Err(format!("brackets[{i}].from_cents 不得为负"));
        }
        if let Some(t) = to {
            if from >= t {
                return Err(format!("brackets[{i}] from_cents 必须 < to_cents"));
            }
        }
        if !(0..=10_000).contains(&rate) {
            return Err(format!("brackets[{i}].rate_bp 必须位于 [0,10000]"));
        }
        brackets.push((from, to, rate));
    }

    // 首档必须从 0 起：档位表须覆盖 [0, +inf)，否则低区间漏税属静默少计
    if brackets[0].0 != 0 {
        return Err("首档 from_cents 必须为 0（档位表须覆盖 [0, +inf)）".to_string());
    }
    // 顶档：恰好一个 to_cents=null，且必须是最后一档
    let top_count = brackets.iter().filter(|b| b.1.is_none()).count();
    if top_count != 1 {
        return Err(format!("顶档（to_cents=null）必须恰好一个，实际 {top_count} 个"));
    }
    if brackets.last().map(|b| b.1.is_some()).unwrap_or(true) {
        return Err("顶档必须是最后一档".to_string());
    }
    // 升序 + 无断裂交叉：后档 from 必须等于前档 to（前档 to 为 None 的情况已被顶档校验排除）
    for w in brackets.windows(2) {
        match w[0].1 {
            Some(t) if t == w[1].0 => {}
            _ => {
                return Err(
                    "档位表必须升序且无断裂/交叉（后档 from_cents 必须等于前档 to_cents）"
                        .to_string(),
                )
            }
        }
    }
    Ok(brackets)
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

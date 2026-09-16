// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 负面 fixture：**内存超限**的 UDF（77 号阶段 2 · V3 实证）
//!
//! # 这条 fixture 要证什么
//! 内存上限（`ResourceLimiter`，默认 16 MiB）的承诺是「超限 → 分配失败，
//! 而非宿主进程 OOM」。断言三条：
//! ① 调用返回 422 结构化错误（trap 被归类、可诊断），而非 500 / 连接断开 / 挂起；
//! ② 宿主进程存活（`GET /health` 仍 2xx）—— 16 MiB 的请求没有被真实吃进宿主内存；
//! ③ 失败即时返回（远小于请求超时），证明是沙箱拦截而非等待超时。
//!
//! # 失败路径机理
//! `Vec::with_capacity(64 MiB)` → wasm 分配器（dlmalloc）调 `memory.grow`
//! → 宿主 `memory_growing` 判 `desired > 上限` 拒绝 → guest 侧 grow 返回 -1
//! → 分配器拿不到内存返回 null → `handle_alloc_error`（panic=abort）→
//! `unreachable` trap → 宿主把 trap 归类为结构化错误（422）。
//! 全程**无真实内存被吃掉**：拒绝发生在增长请求处，不是先分配再回收。
//!
//! 与 `infinite-loop` 一样，本模块**是合法模块**（无 import），host 会正常加载它。
//! **不得放入 `plugins/wasm/`**：放进去会让任何对它的规则调用都失败。

/// 分配（ABI 同正常 UDF）：走同一套分配器，让输入缓冲与超限请求共用一条路径
#[no_mangle]
pub extern "C" fn alloc(len: i32) -> i32 {
    if len <= 0 {
        return 0;
    }
    let layout = match std::alloc::Layout::from_size_align(len as usize, 1) {
        Ok(l) => l,
        Err(_) => return 0,
    };
    unsafe { std::alloc::alloc(layout) as i32 }
}

/// 入口：企图一次性分配 64 MiB —— 4 倍于宿主 16 MiB 上限
#[no_mangle]
pub extern "C" fn udf(_ptr: i32, _len: i32) -> i64 {
    // black_box 阻止 LLVM 把"分配后从未使用"优化掉
    let v = Vec::<u8>::with_capacity(64 * 1024 * 1024);
    std::hint::black_box(&v);
    0
}

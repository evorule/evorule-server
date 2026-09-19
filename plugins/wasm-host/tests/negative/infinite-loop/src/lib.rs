// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 负面 fixture：**永不返回**的 UDF（阶段 2 · V3 实证）
//!
//! # 这条 fixture 要证什么
//! fuel 计量的承诺是「病态 UDF 会被中断，且宿主进程存活」。
//! 断言两条：
//! ① 调用返回 422 且错误含 fuel 耗尽，而不是请求挂起；
//! ② 调用之后 `GET /health` 仍 2xx —— 死循环没有拖垮宿主，也没有泄漏实例状态。
//!
//! 与 `wasi-import` 不同，本模块**是合法模块**（无 import），故 host 会正常加载它。

use std::alloc::Layout;

/// 分配（ABI 同正常 UDF）
#[no_mangle]
pub extern "C" fn alloc(len: i32) -> i32 {
    if len <= 0 {
        return 0;
    }
    let layout = match Layout::from_size_align(len as usize, 1) {
        Ok(l) => l,
        Err(_) => return 0,
    };
    unsafe { std::alloc::alloc(layout) as i32 }
}

/// 入口：无限自增循环，永不返回
///
/// `black_box` 阻止 LLVM 把空循环优化掉 —— 否则模块会"秒返回"，
/// fuel 断言就成了假通过。
#[no_mangle]
pub extern "C" fn udf(_ptr: i32, _len: i32) -> i64 {
    let mut i: u64 = 0;
    loop {
        i = std::hint::black_box(i).wrapping_add(1);
    }
}

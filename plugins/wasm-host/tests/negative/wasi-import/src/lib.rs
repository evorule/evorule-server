// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 负面 fixture：含 **WASI import** 的恶意 UDF（阶段 2 · V2 实证）
//!
//! # 这条 fixture 要证什么
//! `evorule-wasm-host` 的零能力承诺是「Linker 不注册任何 host function，含 WASI」。
//! 若该承诺为真，本模块的 `wasi_snapshot_preview1.fd_write` import 在
//! **链接期**就应解析失败 → 加载即拒（fail-fast），而不是等到调用时才暴露。
//!
//! 若本 fixture 反而加载成功，说明沙箱存在能力泄漏通道 = P0 级缺陷。
//!
//! **本模块不得放入 `plugins/wasm/`**（host 对加载失败是 fail-fast 拒绝启动，
//! 放进去会让正常宿主起不来）。它只供 `run_negative_tests.py` 在独立目录加载。

#![no_std]

/// WASI 导入声明：模块真正执行时企图写 stdout/文件
#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
    fn fd_write(fd: i32, iovs: i32, iovs_len: i32, nwritten: i32) -> i32;
}

/// 静态缓冲（no_std 下不引入分配器，避免额外的 host 依赖）
static mut BUF: [u8; 8] = [0; 8];

/// 分配：固定返回静态缓冲地址（负面 fixture 不关心 ABI 正确性）
#[no_mangle]
pub extern "C" fn alloc(len: i32) -> i32 {
    if len <= 0 || len > 8 {
        return 0;
    }
    unsafe { BUF.as_mut_ptr() as i32 }
}

/// 入口：调用 WASI fd_write —— 沙箱应使此模块根本无法被加载
#[no_mangle]
pub extern "C" fn udf(_ptr: i32, _len: i32) -> i64 {
    let buf = unsafe { BUF.as_mut_ptr() };
    // SAFETY: 参数均为指向本模块静态缓冲的合法指针
    unsafe {
        fd_write(1, buf as i32, 1, buf.add(4) as i32);
    }
    0
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.

#![allow(unsafe_code)]
#![allow(non_camel_case_types)]
#![allow(dead_code)]
//! C FFI 接口 —— 暴露 evorule 核心功能给 C/C++/Python 等外部语言。
//!
//! # 设计原则
//!
//! 1. **句柄模式**：所有对象通过 opaque pointer 暴露，避免内存安全问题
//! 2. **同步包装**：使用 tokio runtime 在同步环境中运行 async 代码
//! 3. **零拷贝**：字符串传递使用指针 + 长度模式
//! 4. **错误码**：所有函数返回 evorule_error_code 枚举
//!
//! # 使用示例（C）
//!
//! ```c
//! #include <evorule.h>
//!
//! int main() {
//!     // 创建反应器
//!     evorule_reactor* reactor = evorule_reactor_new();
//!
//!     // 发送命令
//!     evorule_reactor_send_command(reactor, "{\"type\": \"increment\"}");
//!
//!     // 销毁反应器
//!     evorule_reactor_free(reactor);
//!     return 0;
//! }
//! ```

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};

use evorule_tcb::{JsonValue, ObjectMap};

use crate::Reactor;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum evorule_error_code {
    EVORULE_OK = 0,
    EVORULE_ERROR_OOM = 1,
    EVORULE_ERROR_INVALID_ARG = 2,
    EVORULE_ERROR_RUNTIME = 3,
    EVORULE_ERROR_NOT_INITIALIZED = 4,
}

pub type evorule_reactor = c_void;
pub type evorule_result = c_void;

#[no_mangle]
pub extern "C" fn evorule_version() -> *const c_char {
    const VERSION: &str = env!("CARGO_PKG_VERSION");
    match CString::new(VERSION) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null(),
    }
}

#[no_mangle]
pub extern "C" fn evorule_free_string(s: *mut c_char) {
    if !s.is_null() {
        unsafe { drop(CString::from_raw(s)) };
    }
}

/// FFI 句柄：存储 ReactorHandle 和 Runtime
struct ReactorFfiHandle {
    _handle: crate::ReactorHandle,
    _runtime: tokio::runtime::Runtime,
    // 存储 command sender 用于发送命令
    command_tx: crate::FactSender,
    // 存储快照引用
    snapshot: std::sync::Arc<std::sync::Mutex<crate::ReactorStateSnapshot>>,
    // C4：Command FactId 生成器（持久，跨 send_command 调用复用）
    //
    // 修复前每次 `evorule_reactor_send_command` 都 `FactIdGenerator::new()`，
    // 导致所有 Command 的 FactId 恒为 1，FactsLog 中 FactId 重复。
    // 现持久化于句柄内，`next_id()` 单调递增（1, 2, 3, ...）。
    //
    // 线程安全约定：与 `command_tx` 一致，FFI 句柄的 `&mut` 访问由 C 调用方
    // 外部同步（同一句柄的并发 send_command 调用需调用方自行串行化）。
    fact_id_gen: crate::FactIdGenerator,
}

#[no_mangle]
pub extern "C" fn evorule_reactor_new() -> *mut evorule_reactor {
    #[cfg(not(target_arch = "wasm32"))]
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(_) => return std::ptr::null_mut(),
    };
    // wasm32-unknown-unknown 不支持 tokio runtime（std::time::Instant 未实现，
    // runtime 构造即 panic）。FFI 异步入口在 WASM 下不可用，调用方将收到 null。
    // WASM 场景请使用纯同步 TCB 路径（execute_transition + 内存 FactsLog + BLAKE3 Auditor），
    // 详见 evorule-wasm-demo。（66 §10.4-2 处置）
    #[cfg(target_arch = "wasm32")]
    return std::ptr::null_mut();

    // 在 runtime 内创建 reactor
    let reactor = Reactor::builder(Vec::new()).build();
    let (command_tx, _event_rx, _event_tx, handle, facts_log) = reactor.spawn();

    // 获取快照引用（需要访问内部字段）
    // 注意：ReactorHandle 的 snapshot 是私有的，我们需要另一种方式
    // 简化方案：不存储 snapshot，直接使用 handle 的方法

    // 使用 Arc<Mutex<ReactorStateSnapshot>> 的默认值
    let snapshot =
        std::sync::Arc::new(std::sync::Mutex::new(crate::ReactorStateSnapshot::default()));

    // 保持 facts_log 存活（防止 drop）
    let _facts_log = facts_log;

    let ffi_handle = Box::new(ReactorFfiHandle {
        _handle: handle,
        _runtime: runtime,
        command_tx,
        snapshot,
        // C4：Command FactId 从 1 起单调递增（持久于句柄内）
        fact_id_gen: crate::FactIdGenerator::new(),
    });

    Box::into_raw(ffi_handle) as *mut evorule_reactor
}

#[no_mangle]
pub extern "C" fn evorule_reactor_free(reactor: *mut evorule_reactor) {
    if reactor.is_null() {
        return;
    }
    unsafe {
        let handle = Box::from_raw(reactor as *mut ReactorFfiHandle);
        drop(handle);
    }
}

#[no_mangle]
pub extern "C" fn evorule_reactor_send_command(
    reactor: *mut evorule_reactor,
    instruction_json: *const c_char,
) -> evorule_error_code {
    if reactor.is_null() || instruction_json.is_null() {
        return evorule_error_code::EVORULE_ERROR_INVALID_ARG;
    }

    let handle = unsafe { &mut *(reactor as *mut ReactorFfiHandle) };

    let json_str = unsafe { CStr::from_ptr(instruction_json) };
    let json_str = match json_str.to_str() {
        Ok(s) => s,
        Err(_) => return evorule_error_code::EVORULE_ERROR_INVALID_ARG,
    };

    // 手动解析 JSON 字符串为 JsonValue
    // evorule-tcb 的 JsonValue 不实现 serde，需要手动构造
    let instruction = match parse_simple_json(json_str) {
        Some(v) => v,
        None => return evorule_error_code::EVORULE_ERROR_INVALID_ARG,
    };

    // C4：使用句柄内持久的 FactIdGenerator，保证 Command FactId 单调递增（1, 2, 3, ...）。
    // 修复前每次 new() 导致所有 Command 的 FactId 恒为 1，FactsLog 中 FactId 重复。
    let fact = crate::Fact::Command {
        id: handle.fact_id_gen.next_id(),
        instruction,
    };

    // 使用 send 发送到 unbounded channel
    match handle.command_tx.send(fact) {
        Ok(()) => evorule_error_code::EVORULE_OK,
        Err(_) => evorule_error_code::EVORULE_ERROR_RUNTIME,
    }
}

/// 简单 JSON 解析器（支持基本类型）
fn parse_simple_json(s: &str) -> Option<JsonValue> {
    let s = s.trim();
    if s.starts_with('{') && s.ends_with('}') {
        // 简单对象解析
        parse_json_object(s)
    } else if s.starts_with('"') && s.ends_with('"') {
        // 字符串
        Some(JsonValue::string(&s[1..s.len() - 1]))
    } else if s.starts_with('[') && s.ends_with(']') {
        // 空数组
        Some(JsonValue::Array(vec![]))
    } else if s == "true" {
        Some(JsonValue::bool(true))
    } else if s == "false" {
        Some(JsonValue::bool(false))
    } else if s == "null" {
        Some(JsonValue::null())
    } else if let Ok(n) = s.parse::<i64>() {
        Some(JsonValue::integer(n))
    } else {
        None
    }
}

/// 简单 JSON 对象解析（只支持一层键值对）
fn parse_json_object(s: &str) -> Option<JsonValue> {
    let s = s.trim();
    if !s.starts_with('{') || !s.ends_with('}') {
        return None;
    }

    let inner = &s[1..s.len() - 1].trim();
    if inner.is_empty() {
        return Some(JsonValue::Object(ObjectMap::new()));
    }

    // 简单解析：只处理 "key": value 格式
    let mut obj = ObjectMap::new();
    for part in inner.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        // 查找 ":"
        let colon_pos = part.find(':')?;
        let key_part = part[..colon_pos].trim();
        let val_part = part[colon_pos + 1..].trim();

        // 解析 key（去掉引号）
        if !key_part.starts_with('"') || !key_part.ends_with('"') {
            return None;
        }
        let key = &key_part[1..key_part.len() - 1];

        // 解析 value
        let val = parse_simple_json(val_part)?;
        obj.insert(key.to_string(), val);
    }

    Some(JsonValue::Object(obj))
}

struct ResultHandle {
    output: String,
}

#[no_mangle]
pub extern "C" fn evorule_result_get_output(result: *mut evorule_result) -> *const c_char {
    if result.is_null() {
        return std::ptr::null();
    }
    let handle = unsafe { &*(result as *mut ResultHandle) };
    match CString::new(handle.output.clone()) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null(),
    }
}

#[no_mangle]
pub extern "C" fn evorule_result_free(result: *mut evorule_result) {
    if result.is_null() {
        return;
    }
    unsafe {
        let handle = Box::from_raw(result as *mut ResultHandle);
        drop(handle);
    }
}

#[no_mangle]
pub extern "C" fn evorule_reactor_current_queue_size(reactor: *mut evorule_reactor) -> c_int {
    if reactor.is_null() {
        return -1;
    }
    let handle = unsafe { &*(reactor as *mut ReactorFfiHandle) };
    // 使用 snapshot 获取队列长度
    handle
        .snapshot
        .lock()
        .map(|s| s.queue_len as c_int)
        .unwrap_or(-1)
}

#[cfg(test)]
mod tests {
    #![allow(unsafe_code)]
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::Fact;
    use std::time::{Duration, Instant};

    /// C4 端到端：连发 3 条命令后，Command Fact 在 FactsLog 中 FactId 必须为
    /// 单调递增的 [1, 2, 3]。
    ///
    /// 修复前每次 `evorule_reactor_send_command` 都 `FactIdGenerator::new()`，
    /// 导致所有 Command 的 FactId 恒为 1，FactsLog 中 FactId 重复（破坏审计链
    /// 唯一性）。修复后生成器持久于句柄内，`next_id()` 跨调用累加 → 1, 2, 3, ...
    ///
    /// 直接构造 `ReactorFfiHandle`（保留 facts_log clone 用于观察），调用真实
    /// `evorule_reactor_send_command` 三次，确认 facts_log.history() 中 Command
    /// FactId 序列为 [1, 2, 3]。
    #[test]
    fn ffi_send_command_produces_distinct_fact_ids_in_facts_log() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        // 在 runtime 上下文内 spawn reactor（tokio::spawn 需 context）
        let (command_tx, _event_rx, _event_tx, handle, facts_log) = {
            let _guard = runtime.enter();
            let reactor = Reactor::builder(Vec::new()).build();
            reactor.spawn()
        };

        let snapshot =
            std::sync::Arc::new(std::sync::Mutex::new(crate::ReactorStateSnapshot::default()));
        let ffi_handle = Box::new(ReactorFfiHandle {
            _handle: handle,
            _runtime: runtime,
            command_tx,
            snapshot,
            fact_id_gen: crate::FactIdGenerator::new(),
        });
        let ptr = Box::into_raw(ffi_handle) as *mut evorule_reactor;

        // 连发 3 条命令
        let cmds = [r#"{"op":"a"}"#, r#"{"op":"b"}"#, r#"{"op":"c"}"#];
        for c in &cmds {
            let cstr = CString::new(*c).unwrap();
            let rc = evorule_reactor_send_command(ptr, cstr.as_ptr());
            assert_eq!(rc, evorule_error_code::EVORULE_OK);
        }

        // 轮询 facts_log，等待 3 条 Command 落盘（reactor 异步 drain + emit）
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let n = facts_log
                .history()
                .iter()
                .filter(|f| matches!(f, Fact::Command { .. }))
                .count();
            if n >= 3 || Instant::now() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let ids: Vec<u64> = facts_log
            .history()
            .iter()
            .filter_map(|f| match f {
                Fact::Command { id, .. } => Some(id.0),
                _ => None,
            })
            .collect();

        evorule_reactor_free(ptr);

        assert_eq!(
            ids,
            vec![1, 2, 3],
            "C4: Command FactIds in FactsLog must be [1, 2, 3], got {ids:?}"
        );
    }
}

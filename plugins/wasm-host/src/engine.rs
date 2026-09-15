// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! WASM 执行引擎：零能力 + fuel 计量 + 内存上限 + **每次调用独立 Store**。
//!
//! # 安全模型（ADR-0001 / 77 号 V2–V3）
//! - **零能力**：`Linker` 不定义任何 host function。模块若含 WASI / env import，
//!   链接期即失败（fail-fast），不是运行时才暴露。
//! - **fuel**：`Config::consume_fuel(true)` + 每次调用设预算；耗尽 → trap。
//! - **内存上限**：`ResourceLimiter` 在增长时拦截，超限 → alloc 失败而非 host OOM。
//!
//! # 确定性（77 号 V5）
//! **每次调用新建 `Store`** —— 不复用实例、不跨调用保留状态，
//! 且**不暴露**时间/随机/环境等 host function。相同输入 → 逐字节相同输出。

use wasmtime::{Config, Engine, Linker, Memory, Module, ResourceLimiter, Store, TypedFunc};

/// 资源上限（挂到 `Store` 的 `ResourceLimiter`）
///
/// 签名对齐 wasmtime 48（`current`/`desired`/`maximum` **均为 `usize`**，
/// 早期版本的 `u32` 签名已变更 —— 见 `wasmtime-48.0.2/src/runtime/limits.rs:32`）。
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    max_memory_bytes: usize,
    max_table_elements: usize,
}

impl ResourceLimiter for Limits {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(desired <= self.max_memory_bytes)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(desired <= self.max_table_elements)
    }
}

/// `Store` 数据：仅承载资源限制器（**不承载任何跨调用状态**）
struct StoreData {
    limits: Limits,
}

/// UDF 执行引擎（可共享：`Engine` 内部已 Arc 化）
pub struct UdfRuntime {
    engine: Engine,
    fuel_budget: u64,
    limits: Limits,
}

impl UdfRuntime {
    /// 构造。失败返回人类可读错误（配置非法时 fail-fast）。
    pub fn new(fuel_budget: u64, max_memory_bytes: usize) -> Result<Self, String> {
        let mut config = Config::new();
        // fuel：执行步数预算（防死循环 / 恶意计算）
        config.consume_fuel(true);
        // 多内存：UDF 不需要，显式关掉以收紧面
        config.wasm_multi_memory(false);
        // 其余特性（component-model / threads / gc / simd…）保持 wasmtime 默认。
        // 注意：本包以 default-features=false + [runtime, cranelift] 构建，
        // 显式调用 wasm_component_model()/wasm_threads() 等方法会因对应 feature 未启用
        // 而**编译失败**——故此处不写，依赖默认值（默认即关闭）。

        let engine = Engine::new(&config).map_err(|e| format!("wasmtime Engine 创建失败: {e}"))?;
        Ok(Self {
            engine,
            fuel_budget,
            limits: Limits {
                max_memory_bytes,
                max_table_elements: 10_000,
            },
        })
    }

    /// 从文件编译模块（编译一次，多次调用复用）
    pub fn load(&self, path: &std::path::Path) -> Result<Module, String> {
        Module::from_file(&self.engine, path)
            .map_err(|e| format!("WASM 模块编译失败 {}: {e}", path.display()))
    }

    /// 执行一次 UDF：`input` 为入参 JSON 字节，返回出参 JSON 字节。
    ///
    /// **每次调用新建 `Store`**（确定性保证，见模块文档）。
    pub fn call(&self, module: &Module, input: &[u8]) -> Result<Vec<u8>, String> {
        let mut store = Store::new(
            &self.engine,
            StoreData {
                limits: self.limits,
            },
        );
        store
            .set_fuel(self.fuel_budget)
            .map_err(|e| format!("fuel 预算设置失败（是否未启用 consume_fuel？）: {e}"))?;
        store.limiter(|d| &mut d.limits);

        // 零能力：Linker 不注册任何 host function
        let linker = Linker::new(&self.engine);
        let instance: wasmtime::Instance = linker
            .instantiate(&mut store, module)
            .map_err(|e| format!("模块实例化失败（含未授权 import 将被此拦截）: {e}"))?;

        let memory = memory_of(&mut store, &instance)?;
        let alloc = alloc_fn(&mut store, &instance)?;
        let udf = udf_fn(&mut store, &instance)?;

        // 1) 在 guest 内存中分配输入空间
        let in_len = i32::try_from(input.len()).map_err(|_| "输入过长（> i32::MAX）".to_string())?;
        let in_ptr = alloc
            .call(&mut store, in_len)
            .map_err(|e| format!("guest alloc 失败: {e}"))?;
        if in_ptr <= 0 {
            return Err("guest alloc 返回非法指针".to_string());
        }

        // 2) 写入输入
        memory
            .write(&mut store, in_ptr as usize, input)
            .map_err(|e| format!("写入 guest 内存失败: {e}"))?;

        // 3) 调用 UDF
        let ret = match udf.call(&mut store, (in_ptr, in_len)) {
            Ok(v) => v,
            Err(e) => return Err(classify_trap(&e, &store)),
        };

        // 4) 解包返回值：高 32 位 = 结果指针，低 32 位 = 结果长度
        let out_ptr = (ret >> 32) as u32;
        let out_len = (ret & 0xFFFF_FFFF) as u32;
        if out_ptr == 0 {
            return Err("UDF 返回空指针".to_string());
        }
        let out_len_usize =
            usize::try_from(out_len).map_err(|_| "UDF 返回长度非法".to_string())?;

        let mut buf = vec![0u8; out_len_usize];
        memory
            .read(&mut store, out_ptr as usize, &mut buf)
            .map_err(|e| format!("读取 guest 内存失败（返回指针越界？）: {e}"))?;
        Ok(buf)
    }
}

/// 取 guest 导出的 `memory`
fn memory_of(store: &mut Store<StoreData>, instance: &wasmtime::Instance) -> Result<Memory, String> {
    instance
        .get_memory(store, "memory")
        .ok_or_else(|| "模块未导出 memory（UDF ABI 要求）".to_string())
}

/// 取 guest 导出的 `alloc(len) -> ptr`
fn alloc_fn(
    store: &mut Store<StoreData>,
    instance: &wasmtime::Instance,
) -> Result<TypedFunc<i32, i32>, String> {
    instance
        .get_typed_func::<i32, i32>(store, "alloc")
        .map_err(|_| "模块未导出 alloc(i32)->i32（UDF ABI 要求）".to_string())
}

/// 取 guest 导出的 `udf(ptr, len) -> i64`
fn udf_fn(
    store: &mut Store<StoreData>,
    instance: &wasmtime::Instance,
) -> Result<TypedFunc<(i32, i32), i64>, String> {
    instance
        .get_typed_func::<(i32, i32), i64>(store, "udf")
        .map_err(|_| "模块未导出 udf(i32,i32)->i64（UDF ABI 要求）".to_string())
}

/// 把 trap 归类为人类可读的失败原因（V3：审计要能看出是超时还是别的原因）
fn classify_trap(e: &wasmtime::Error, store: &Store<StoreData>) -> String {
    let msg = e.to_string();
    // wasmtime 48：剩余 fuel 用 `Store::get_fuel()`（旧版 `fuel_consumed()` 已移除）
    let remaining = store.get_fuel().unwrap_or(0);
    if msg.contains("all fuel consumed") || msg.contains("fuel") {
        format!("UDF 执行超出 fuel 预算（剩余 {remaining}）——疑似死循环或计算量过大")
    } else if msg.contains("out of bounds") {
        "UDF 内存访问越界（trap）".to_string()
    } else if msg.contains("unreachable") {
        "UDF 主动 trap（unreachable）".to_string()
    } else {
        format!("UDF 执行失败（trap，剩余 fuel {remaining}）: {msg}")
    }
}

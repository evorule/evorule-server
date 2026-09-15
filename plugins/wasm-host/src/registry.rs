// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! UDF 模块注册表：扫描目录 → 编译 → 按服务名索引。
//!
//! **服务名 = `.wasm` 文件名（去扩展名）**，对齐 67 号 `udf_finance_tax_calc` 口径。
//!
//! 用 `BTreeMap` 而非 `HashMap`：加载顺序与 `/health` 的 UDF 列表顺序**确定性**
//! （本项目全局确定性纪律：键序不应随运行时哈希种子变化）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use wasmtime::Module;

use crate::engine::UdfRuntime;

/// 已加载的 UDF 模块集合
pub struct UdfRegistry {
    runtime: Arc<UdfRuntime>,
    /// 服务名 → 已编译模块
    modules: BTreeMap<String, Arc<Module>>,
}

impl UdfRegistry {
    /// 扫描目录并编译其中所有 `*.wasm`。
    ///
    /// 目录不存在 → 返回空注册表（不报错，host 仍可启动并对外探活）。
    pub fn load_dir(runtime: Arc<UdfRuntime>, dir: &Path) -> Result<Self, String> {
        let mut modules = BTreeMap::new();
        if !dir.exists() {
            tracing::warn!(
                "WASM 目录不存在（将加载 0 个 UDF）: {}",
                dir.display()
            );
            return Ok(Self { runtime, modules });
        }

        let entries = std::fs::read_dir(dir)
            .map_err(|e| format!("读取 WASM 目录失败 {}: {e}", dir.display()))?;

        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => return Err(format!("遍历 WASM 目录失败: {e}")),
            };
            let path: PathBuf = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("wasm") {
                continue;
            }
            let name = match path.file_stem().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None => return Err(format!("无法从文件名推导服务名: {}", path.display())),
            };
            let module = match runtime.load(&path) {
                Ok(m) => m,
                Err(e) => {
                    // fail-fast：单个坏模块拒绝启动，避免"运行时才发现某个 UDF 不可用"
                    return Err(format!("加载 UDF `{name}` 失败: {e}"));
                }
            };
            tracing::info!("UDF 已加载: {name} <- {}", path.display());
            modules.insert(name, Arc::new(module));
        }

        Ok(Self { runtime, modules })
    }

    /// 已加载的 UDF 服务名（确定性顺序）
    pub fn names(&self) -> Vec<String> {
        self.modules.keys().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.modules.len()
    }

    /// 取模块的共享引用（供 `spawn_blocking` 移入阻塞线程）
    pub fn module(&self, name: &str) -> Option<Arc<Module>> {
        self.modules.get(name).cloned()
    }

    /// 取运行时（供 `spawn_blocking` 移入阻塞线程）
    pub fn runtime(&self) -> Arc<UdfRuntime> {
        Arc::clone(&self.runtime)
    }
}

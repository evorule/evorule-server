// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! UDF 模块注册表：扫描目录 → 编译 → 按服务名索引 → **声明对账**。
//!
//! **服务名 = `.wasm` 文件名（去扩展名）**，对齐历史批次 `udf_finance_tax_calc` 口径。
//! 该口径必须与 `plugin.json` 的 `services[].name` 一致——由加载期对账强制
//! （见 `declaration` 模块），不一致不再静默。
//!
//! 用 `BTreeMap` 而非 `HashMap`：加载顺序与 `/health` 的 UDF 列表顺序**确定性**
//! （本项目全局确定性纪律：键序不应随运行时哈希种子变化）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use wasmtime::Module;

use crate::declaration::Reconciliation;
use crate::engine::UdfRuntime;

/// 已加载的 UDF 模块集合 (+ 一次性算定的声明对账结论)
pub struct UdfRegistry {
    runtime: Arc<UdfRuntime>,
    /// 服务名 → 已编译模块
    modules: BTreeMap<String, Arc<Module>>,
    /// 声明 × 实载对账结论。模块集在进程生命周期内不变，故只算一次；
    /// 结论经 `/health` 状态码外化（见 `declaration` 模块头）。
    reconciliation: Reconciliation,
}

impl UdfRegistry {
    /// 扫描目录并编译其中所有 `*.wasm`，随后做一次声明对账。
    ///
    /// **目录不存在 → Err（fail-fast）**：目录给错（如误传 MSYS 风格路径）时
    /// 旧实现只打一条 warn 就带着 0 个 UDF 启动，进程活着、能力为零，
    /// 且 `/health` 照报 ok —— 这正是本模块要消灭的静默故障。
    /// 目录存在但为空 → 允许启动（`/health` 会判 degraded 并给出诊断，便于现场排查）。
    pub fn load_dir(
        runtime: Arc<UdfRuntime>,
        dir: &Path,
        declaration_path: Option<&str>,
    ) -> Result<Self, String> {
        let mut modules = BTreeMap::new();
        if !dir.is_dir() {
            return Err(format!(
                "WASM 目录不存在或不是目录: {}（自诊断: ① 检查 WASM_HOST_DIR 是否指向 \
                 真实的 .wasm 目录; ② Windows 下避免传 MSYS 风格路径, 用 `D:/a/b` 形态）",
                dir.display()
            ));
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

        let loaded: Vec<String> = modules.keys().cloned().collect();
        let reconciliation = Reconciliation::run(dir, declaration_path, &loaded);
        Ok(Self {
            runtime,
            modules,
            reconciliation,
        })
    }

    /// 声明对账结论（`/health` 的 degraded 判定与诊断正文来源）
    pub fn reconciliation(&self) -> &Reconciliation {
        &self.reconciliation
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

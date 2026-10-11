// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! UDF 模块注册表：扫描目录 → **完整性校验（前置）** → 编译 → 按服务名索引 → **声明对账**。
//!
//! **服务名 = `.wasm` 文件名（去扩展名）**，对齐历史批次 `udf_font_tax_calc` 之前即定的
//! `udf_finance_tax_calc` 口径。该口径必须与 `plugin.json` 的 `services[].name` 一致——
//! 由加载期对账强制（见 `declaration` 模块），不一致不再静默。
//!
//! **加载序（分发完整性一期）**：完整性校验（`integrity` 模块：清单哈希/验签/纯度
//! 声明/ABI 兼容，策略 warn|enforce|off）**先于编译**——被拒模块不进 wasmtime。
//! 校验放行后才进入引擎零能力防线（import 枚举非空即拒）；清单声明 `zero-import`
//! 而实测 import 非空时，拒载消息融合「纯度声明与实测不符」定性。
//!
//! **热加载（安装自动化三期）**：`modules`/`integrity`/`reconciliation` 装入
//! `RwLock<Arc<...>>` 快照——读路径取快照 Arc（clone 廉价、零阻塞），写路径
//! （install/uninstall/disable）在锁内原子换引用。**正在执行的调用持旧
//! `Arc<Module>` 自然完成**（快照语义，无中断）；每次调用新建 Store 的确定性
//! 语义不受影响。对账结论随每次变更重算（声明×实载交叉即刻反映新模块集）。
//!
//! 用 `BTreeMap` 而非 `HashMap`：加载顺序与 `/health` 的 UDF 列表顺序**确定性**
//! （本项目全局确定性纪律：键序不应随运行时哈希种子变化）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use wasmtime::Module;

use crate::declaration::Reconciliation;
use crate::engine::UdfRuntime;
use crate::integrity::{Integrity, Verifier};

/// 运行时可热更的模块集快照（读路径整体 Arc 克隆，写路径锁内换引用）
pub struct RegistrySnapshot {
    pub modules: Arc<BTreeMap<String, Arc<Module>>>,
    pub integrity: Arc<BTreeMap<String, Integrity>>,
    pub reconciliation: Reconciliation,
}

/// 安装/卸载各阶段耗时明细（透明性：console 回显，失败可定位到阶段）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageTimings {
    pub verify_ms: u128,
    pub persist_ms: u128,
    pub compile_ms: u128,
    pub swap_ms: u128,
}

impl StageTimings {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "verify_ms": self.verify_ms,
            "persist_ms": self.persist_ms,
            "compile_ms": self.compile_ms,
            "swap_ms": self.swap_ms,
        })
    }
}

pub struct UdfRegistry {
    runtime: Arc<UdfRuntime>,
    /// 模块目录（install 落盘目标；uninstall 删除目标）
    dir: PathBuf,
    declaration_path: Option<String>,
    /// 信任锚（install 验签用；与加载期同源同权）
    trusted: Option<Arc<crate::integrity::TrustedKeys>>,
    policy: crate::integrity::SignPolicy,
    state: std::sync::RwLock<RegistrySnapshot>,
}

impl UdfRegistry {
    /// 扫描目录并编译其中所有 `*.wasm`（完整性校验前置），随后做一次声明对账。
    ///
    /// **目录不存在 → Err（fail-fast）**：目录给错（如误传 MSYS 风格路径）时
    /// 旧实现只打一条 warn 就带着 0 个 UDF 启动，进程活着、能力为零，
    /// 且 `/health` 照报 ok —— 这正是本模块要消灭的静默故障。
    /// 目录存在但为空 → 允许启动（`/health` 会判 degraded 并给出诊断，便于现场排查）。
    pub fn load_dir(
        runtime: Arc<UdfRuntime>,
        dir: &Path,
        declaration_path: Option<&str>,
        verifier: &Verifier<'_>,
        trusted: Option<Arc<crate::integrity::TrustedKeys>>,
        policy: crate::integrity::SignPolicy,
    ) -> Result<Self, String> {
        let mut modules: BTreeMap<String, Arc<Module>> = BTreeMap::new();
        let mut integrity: BTreeMap<String, Integrity> = BTreeMap::new();
        if !dir.is_dir() {
            return Err(format!(
                "WASM 目录不存在或不是目录: {}（自诊断: ① 检查 WASM_HOST_DIR 是否指向 \
                 真实的 .wasm 目录; ② Windows 下避免传 MSYS 风格路径, 用 `D:/a/b` 形态）",
                dir.display()
            ));
        }

        let entries = std::fs::read_dir(dir)
            .map_err(|e| format!("读取 WASM 目录失败 {}: {e}", dir.display()))?;

        // 先收集全部 .wasm 路径再逐个处理（BTreeMap 键序保证确定性加载顺序）
        let mut wasm_paths: Vec<PathBuf> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| format!("遍历 WASM 目录失败: {e}"))?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("wasm") {
                wasm_paths.push(path);
            }
        }
        wasm_paths.sort();
        for path in wasm_paths {
            let name = match path.file_stem().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None => return Err(format!("无法从文件名推导服务名: {}", path.display())),
            };
            let wasm_bytes = std::fs::read(&path)
                .map_err(|e| format!("读取模块 {} 失败: {e}", path.display()))?;

            // ① 完整性校验（前置，见模块头加载序）
            let verdict = verifier.verify_module(&name, &wasm_bytes, dir)?;
            if let Integrity::AcceptedUnverified { reason } = &verdict {
                tracing::warn!(
                    "模块 `{name}` 未验证放行（warn 策略）: {reason} —— 生产环境建议 enforce + 官方签名"
                );
            }

            // ② 引擎加载（零能力防线①：import 枚举非空即拒）
            let module = match runtime.load(&path) {
                Ok(m) => m,
                Err(e) => {
                    let fused = if verdict != Integrity::Unchecked {
                        format!(
                            "加载 UDF `{name}` 失败: {e}\
                             \n  —— 纯度对账：清单声明 zero-import 与实测 import 不符（声明×实载不一致，拒载）"
                        )
                    } else {
                        format!("加载 UDF `{name}` 失败: {e}")
                    };
                    return Err(fused);
                }
            };
            tracing::info!(
                "UDF 已加载: {name} <- {} ({})",
                path.display(),
                integrity_tag(&verdict)
            );
            integrity.insert(name.clone(), verdict);
            modules.insert(name, Arc::new(module));
        }

        let loaded: Vec<String> = modules.keys().cloned().collect();
        let reconciliation = Reconciliation::run(dir, declaration_path, &loaded);
        Ok(Self {
            runtime,
            dir: dir.to_path_buf(),
            declaration_path: declaration_path.map(str::to_string),
            trusted,
            policy,
            state: std::sync::RwLock::new(RegistrySnapshot {
                modules: Arc::new(modules),
                integrity: Arc::new(integrity),
                reconciliation,
            }),
        })
    }

    /// 当前快照（读路径：整体 Arc 克隆，零阻塞）
    pub fn snapshot(&self) -> RegistrySnapshot {
        // poison 恢复：持毒多半是写路径 panic 后的数据仍完整（快照换引用前失败），
        // into_inner 取数据继续（fail-visible 靠 /health 对账，不靠锁守卫）
        self.state
            .read()
            .map(|s| RegistrySnapshot {
                modules: Arc::clone(&s.modules),
                integrity: Arc::clone(&s.integrity),
                reconciliation: s.reconciliation.clone(),
            })
            .unwrap_or_else(|p| {
                let s = p.into_inner();
                RegistrySnapshot {
                    modules: Arc::clone(&s.modules),
                    integrity: Arc::clone(&s.integrity),
                    reconciliation: s.reconciliation.clone(),
                }
            })
    }

    /// 声明对账结论（`/health` 的 degraded 判定与诊断正文来源）
    pub fn reconciliation(&self) -> Reconciliation {
        self.snapshot().reconciliation
    }

    /// 完整性结论（/health 外化：verified / accepted_unverified / unchecked）
    pub fn integrity_of(&self, name: &str) -> Integrity {
        self.snapshot()
            .integrity
            .get(name)
            .cloned()
            .unwrap_or(Integrity::Unchecked)
    }

    /// 已加载的 UDF 服务名（确定性顺序）
    pub fn names(&self) -> Vec<String> {
        self.snapshot().modules.keys().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.snapshot().modules.len()
    }

    /// 取模块的共享引用（供 `spawn_blocking` 移入阻塞线程）
    pub fn module(&self, name: &str) -> Option<Arc<Module>> {
        self.snapshot().modules.get(name).cloned()
    }

    /// 取运行时（供 `spawn_blocking` 移入阻塞线程）
    pub fn runtime(&self) -> Arc<UdfRuntime> {
        Arc::clone(&self.runtime)
    }

    // ========================================================================
    // 变更路径（安装自动化三期）：install / uninstall / disable
    // ========================================================================

    /// 安装：验签 → 原子落盘（staging 目录）→ 编译 → 快照换引用。
    ///
    /// 验签与加载期**同一代码路径**（`integrity::Verifier`，当前策略+当前信任锚
    /// 同权同责）；任何一步失败则 staging 目录整体清理、线上快照零变化。
    /// 同名已存在 = 升级语义；新版本号 < 旧版本号需 `allow_downgrade` 显式放行。
    pub fn install(
        &self,
        name: &str,
        wasm_bytes: Vec<u8>,
        manifest_bytes: Vec<u8>,
        sig_bytes: Option<Vec<u8>>,
        allow_downgrade: bool,
    ) -> Result<StageTimings, String> {
        if !valid_name(name) {
            return Err(format!("非法模块名: {name:?}（只允许字母数字与 - _）"));
        }
        let t0 = std::time::Instant::now();

        // ① staging 目录落三件套 → 同一验签路径（加载期=安装期同权同责）
        let staging = self.dir.join(format!(".staging-{name}"));
        cleanup_dir(&staging);
        std::fs::create_dir_all(&staging).map_err(|e| format!("建 staging 目录失败: {e}"))?;
        std::fs::write(staging.join(format!("{name}.wasm")), &wasm_bytes)
            .map_err(|e| format!("staging 写 .wasm 失败: {e}"))?;
        std::fs::write(
            staging.join(format!("{name}.plugin-module.json")),
            &manifest_bytes,
        )
        .map_err(|e| format!("staging 写清单失败: {e}"))?;
        if let Some(sig) = &sig_bytes {
            std::fs::write(staging.join(format!("{name}.wasm.sig")), sig)
                .map_err(|e| format!("staging 写签名失败: {e}"))?;
        }

        let verifier = Verifier::new(self.policy, self.trusted.as_deref());
        let verify_result = verifier.verify_module(name, &wasm_bytes, &staging);
        let verdict = match verify_result {
            Ok(v) => v,
            Err(e) => {
                cleanup_dir(&staging);
                return Err(format!("安装被拒（验签/清单校验）: {e}"));
            }
        };
        let verify_ms = t0.elapsed().as_millis();

        // ② 升级/降级语义（旧清单 version 比对）
        let old_version = self
            .snapshot()
            .modules
            .contains_key(name)
            .then(|| {
                std::fs::read_to_string(self.dir.join(format!("{name}.plugin-module.json")))
                    .ok()
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                    .and_then(|m| {
                        m.get("version")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                    })
            })
            .flatten();
        if let (Some(old), Some(new)) = (&old_version, &version_of(&manifest_bytes)) {
            if !allow_downgrade && crate::integrity::version_at_least(old, new) == Some(false) {
                cleanup_dir(&staging);
                return Err(format!(
                    "版本降级需显式放行: 当前 {old} < 目标 {new}（X-Allow-Downgrade: true）"
                ));
            }
        }

        // ③ 编译（staging 路径；import 枚举+纯度融合与加载期一致）
        let staged_wasm = staging.join(format!("{name}.wasm"));
        let persist_ms = t0.elapsed().as_millis() - verify_ms;
        let compile_started = std::time::Instant::now();
        let module = match self.runtime.load(&staged_wasm) {
            Ok(m) => m,
            Err(e) => {
                cleanup_dir(&staging);
                return Err(format!(
                    "安装被拒（引擎加载）: {e}\
                     \n  —— 纯度对账：清单声明 zero-import 与实测 import 不符（拒载）"
                ));
            }
        };
        let compile_ms = compile_started.elapsed().as_millis();

        // ④ 原子生效：三件套 rename 到运行目录（同盘 rename 原子）+ 快照换引用
        let swap_started = std::time::Instant::now();
        for file in [
            format!("{name}.wasm"),
            format!("{name}.plugin-module.json"),
            format!("{name}.wasm.sig"),
        ] {
            let from = staging.join(&file);
            let to = self.dir.join(&file);
            if from.exists() {
                std::fs::rename(&from, &to).map_err(|e| {
                    format!("生效失败（{} → {}）: {e}", from.display(), to.display())
                })?;
            } else if file.ends_with(".wasm.sig") && sig_bytes.is_none() {
                // 本批无签名但旧版本有 → 清理旧签名（材料状态如实反映当前批）
                let _ = std::fs::remove_file(&to);
            }
        }
        cleanup_dir(&staging);

        {
            // poison 恢复：同 snapshot 读路径口径
            let mut state = self.state.write().unwrap_or_else(|p| p.into_inner());
            let mut modules = (*state.modules).clone();
            let mut integrity = (*state.integrity).clone();
            modules.insert(name.to_string(), Arc::new(module));
            integrity.insert(name.to_string(), verdict);
            let loaded: Vec<String> = modules.keys().cloned().collect();
            state.modules = Arc::new(modules);
            state.integrity = Arc::new(integrity);
            state.reconciliation =
                Reconciliation::run(&self.dir, self.declaration_path.as_deref(), &loaded);
        }
        let swap_ms = swap_started.elapsed().as_millis();

        tracing::info!("UDF 已安装: {name}（verify {verify_ms}ms / persist {persist_ms}ms / compile {compile_ms}ms / swap {swap_ms}ms）");
        Ok(StageTimings {
            verify_ms,
            persist_ms,
            compile_ms,
            swap_ms,
        })
    }

    /// 卸载：快照移除 + 三件套物理删除（不可逆；disable=仅运行时下线，文件保留）
    pub fn uninstall(&self, name: &str) -> Result<StageTimings, String> {
        let t0 = std::time::Instant::now();
        let mut removed = 0usize;
        for file in [
            format!("{name}.wasm"),
            format!("{name}.plugin-module.json"),
            format!("{name}.wasm.sig"),
        ] {
            // 兼容卸载请求未带扩展名的宽容形态（{name}.wasm 直接匹配文件名）
            let p = self.dir.join(&file);
            if p.is_file() {
                std::fs::remove_file(&p).map_err(|e| format!("删除 {} 失败: {e}", p.display()))?;
                removed += 1;
            }
        }
        let persist_ms = t0.elapsed().as_millis();
        self.detach_snapshot(name);
        tracing::info!("UDF 已卸载: {name}（移除 {removed} 个文件）");
        Ok(StageTimings {
            verify_ms: 0,
            persist_ms,
            compile_ms: 0,
            swap_ms: t0.elapsed().as_millis() - persist_ms,
        })
    }

    /// 禁用：仅运行时下线（快照移除，文件保留——重启后恢复在役）
    pub fn disable(&self, name: &str) -> Result<StageTimings, String> {
        let t0 = std::time::Instant::now();
        self.detach_snapshot(name);
        Ok(StageTimings {
            verify_ms: 0,
            persist_ms: 0,
            compile_ms: 0,
            swap_ms: t0.elapsed().as_millis(),
        })
    }

    /// 快照移除（uninstall/disable 共用；文件处置由调用方负责）
    fn detach_snapshot(&self, name: &str) {
        // poison 恢复：同 snapshot 读路径口径
        let mut state = self.state.write().unwrap_or_else(|p| p.into_inner());
        let mut modules = (*state.modules).clone();
        let mut integrity = (*state.integrity).clone();
        modules.remove(name);
        integrity.remove(name);
        let loaded: Vec<String> = modules.keys().cloned().collect();
        state.modules = Arc::new(modules);
        state.integrity = Arc::new(integrity);
        state.reconciliation =
            Reconciliation::run(&self.dir, self.declaration_path.as_deref(), &loaded);
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn cleanup_dir(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

fn version_of(manifest_bytes: &[u8]) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(std::str::from_utf8(manifest_bytes).ok()?)
        .ok()?
        .get("version")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

fn integrity_tag(v: &Integrity) -> &'static str {
    match v {
        Integrity::Verified { .. } => "签名已验证",
        Integrity::AcceptedUnverified { .. } => "未验证放行(warn)",
        Integrity::Unchecked => "完整性检查关闭(off)",
    }
}

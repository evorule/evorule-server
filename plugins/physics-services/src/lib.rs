// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 确定性物理仿真原生服务（UV-035:第二个原生插件 crate 泛化验证）。
//!
//! # 定位
//! - 把 rpsm-demo 的确定性物理内核(vendored 快照,见 [`kernel`] 模块头注)封装为
//!   evorule-server 的原生 `IoHandler` 服务,使 `io_request(call_service/call_external,
//!   service_name=physics_*)` 在进程内确定性执行。
//! - 独立 crate(`plugins/physics-services`),不修改核心 crate,仅经 `IoDispatcher` 挂载。
//! - 本 crate 是插件机制泛化验证载体:与 `demo-services` 平行、结构同构,
//!   证明「清单(UV-030)+ 声明 SSOT(UV-029)+ 健康节」对第二个插件成立。
//!
//! # 路由设计(与 demo-services 同构)
//! - `PhysicsServiceRouter` 实现 [IoHandler],按 `params.service_name` 分发:
//!   - 命中原生服务名 → 调用对应原生实现(进程内,确定性)
//!   - 未命中 → 回落 `ServiceRegistryHandler`(HTTP,兼容其他外部服务)
//! - 原生实现接收的入参 = `params.args`。
//!
//! # 确定性边界(诚实声明)
//! - 内核为辛积分器 + 锁定常量 + 固定 f64 精度:同平台同输入**逐位一致**;
//! - 服务无状态、无墙钟、无随机源;
//! - 浮点(TCB 无 Float 变体)入参接受 Integer/数字字符串,出参一律字符串化
//!   (与 demo-services `float_str` 约定一致);
//! - 跨平台浮点差异不在承诺范围;内核确定性模型详见 `kernel::mod` 头注。

#![forbid(unsafe_code)]

pub mod kernel;
pub mod services;

use std::sync::Arc;

use async_trait::async_trait;
use evorule_reactor::{IoHandler, IoResult};
use evorule_tcb::JsonValue;

use crate::services::{PhysicsEnergy, PhysicsGravBand, PhysicsSimulate};

/// 原生服务的统一入参入口：接收 `args`（已由引擎解析 __ 路径为实际值），返回业务结果。
///
/// 注:与 `demo-services` 的同名 trait 结构相同、各自持有——第二个插件先行落地,
/// trait 上提公共 crate 属插件注册表化(W2)的设计决策,本轮不做跨插件抽象先行。
pub trait NativeService: Send + Sync {
    fn execute(&self, args: &JsonValue) -> IoResult;
}

// ============================================================================
// 复合路由：原生优先，HTTP 回落
// ============================================================================

/// 复合 IoHandler：`service_name` 命中原生服务名 → 原生执行；否则回落 HTTP。
///
/// 挂载到 `IoType::call_service()` / `IoType::call_external()`。
pub struct PhysicsServiceRouter {
    /// 原生服务实例(name+实例成对存放,支持部署期启用子集)
    instances: Vec<(&'static str, Arc<dyn NativeService>)>,
    /// 原生未命中时的 HTTP 回落（ServiceRegistryHandler，读 service_registry.json）
    fallback: Arc<dyn IoHandler>,
}

/// 原生服务声明项(与 demo-services 同构:新增原生能力 = 向 [`NATIVE_SERVICES`] 追加一项)。
///
/// `name` 与治理侧服务目录种子对齐——共同事实源为本插件声明文件
/// `official_native_services.json`(UV-029 泛化,SSOT),双侧守卫锁定漂移(见文件底部测试)。
pub struct NativeServiceDef {
    /// 全局唯一服务名(`io_request` 的 `service_name`;跨插件唯一,治理侧聚合时锁定)
    pub name: &'static str,
    /// 是否涉及凭据/敏感数据(与治理侧目录元数据对齐)
    pub sensitive: bool,
    /// 一句话描述(供能力对账/文档派生)
    pub description: &'static str,
    /// 实例构造子(带状态服务在此注入默认状态)
    pub make: fn() -> Arc<dyn NativeService>,
}

/// 原生服务声明表(SSOT:本插件原生叶子能力全量清单,顺序即路由查找序)。
pub const NATIVE_SERVICES: &[NativeServiceDef] = &[
    NativeServiceDef {
        name: "physics_simulate",
        sensitive: false,
        description: "确定性物理仿真推进(刚体+辛积分器,vendored rpsm-core 内核)",
        make: mk_simulate,
    },
    NativeServiceDef {
        name: "physics_energy",
        sensitive: false,
        description: "物理系统总机械能计算(确定性)",
        make: mk_energy,
    },
    NativeServiceDef {
        name: "physics_grav_band",
        sensitive: false,
        description: "有界重力带(分层势场)仿真推进与逃逸判定(确定性)",
        make: mk_grav_band,
    },
];

fn mk_simulate() -> Arc<dyn NativeService> {
    Arc::new(PhysicsSimulate)
}
fn mk_energy() -> Arc<dyn NativeService> {
    Arc::new(PhysicsEnergy)
}
fn mk_grav_band() -> Arc<dyn NativeService> {
    Arc::new(PhysicsGravBand)
}

impl PhysicsServiceRouter {
    /// 构造复合路由。`fallback` 为未命中原生服务名时的 HTTP 处理器
    /// （通常传 `Arc<ServiceRegistryHandler>`，由调用方按 --allow-loopback 构造）。
    pub fn new(fallback: Arc<dyn IoHandler>) -> Self {
        Self {
            instances: NATIVE_SERVICES.iter().map(|d| (d.name, (d.make)())).collect(),
            fallback,
        }
    }

    /// 部署期启用子集构造(UV-030 插件清单化,与 demo-services 同语义)。
    ///
    /// - `enabled` 为启用服务名集合（顺序无关,路由查找仍按 `NATIVE_SERVICES` 声明序）;
    /// - 未知名 / 重复名 / 空启用集 → fail-fast Err(含指引,不静默忽略);
    /// - 宿主零具体名特判:新增原生能力 = `NATIVE_SERVICES` 追加一项 + 清单启用。
    pub fn with_enabled(fallback: Arc<dyn IoHandler>, enabled: &[&str]) -> Result<Self, String> {
        if enabled.is_empty() {
            return Err(
                "插件启用集为空 — 若要停用全部原生服务请直接 enabled=false(不挂载本路由),\
                 若要启用请在 plugin_manifest.services 中至少列出一个服务"
                    .to_string(),
            );
        }
        let mut seen: Vec<&str> = Vec::new();
        for name in enabled {
            let known = NATIVE_SERVICES.iter().any(|d| d.name == *name);
            if !known {
                return Err(format!(
                    "plugin_manifest 引用了未注册的原生服务 '{name}' — 合法服务名: [{}]。\
                     自诊断指引: ① 核对 service_name 拼写(以本清单为准,非治理侧目录); \
                     ② 新增原生服务请向 NATIVE_SERVICES 声明表追加一项后在清单中启用",
                    NATIVE_SERVICES
                        .iter()
                        .map(|d| d.name)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if seen.contains(name) {
                return Err(format!(
                    "plugin_manifest 服务 '{name}' 重复声明 — 请去重后重试(不静默去重)"
                ));
            }
            seen.push(name);
        }
        Ok(Self {
            instances: NATIVE_SERVICES
                .iter()
                .filter(|d| seen.contains(&d.name))
                .map(|d| (d.name, (d.make)()))
                .collect(),
            fallback,
        })
    }

    /// 当前实例已启用的原生服务名列表(健康可见性/能力对账用)
    pub fn enabled_service_names(&self) -> Vec<&'static str> {
        self.instances.iter().map(|(n, _)| *n).collect()
    }

    /// 原生服务名列表（server 侧服务绑定核对/能力对账用；自声明表派生）
    pub fn native_service_names() -> Vec<&'static str> {
        NATIVE_SERVICES.iter().map(|d| d.name).collect()
    }

    /// 解析 `service_name`（params 顶层），并取出 `args`（默认空对象）。
    fn split_params(params: &JsonValue) -> (Option<String>, JsonValue) {
        let service_name = params
            .get("service_name")
            .and_then(|v| v.as_str())
            .or_else(|| params.get("name").and_then(|v| v.as_str()))
            .map(|s| s.to_string());
        let args = params
            .get("args")
            .cloned()
            .unwrap_or_else(JsonValue::empty_object);
        (service_name, args)
    }
}

#[async_trait]
impl IoHandler for PhysicsServiceRouter {
    async fn execute(&self, params: &JsonValue) -> IoResult {
        let (service_name, args) = Self::split_params(params);
        let name = service_name.as_deref().unwrap_or("");
        // 声明表查找分发:新增原生服务 = 表加一项,此处零改动;
        // 仅在本路由实例已启用的子集内查找(未启用 → 回落/如实报错)
        match self.instances.iter().find(|(n, _)| *n == name) {
            Some((_, svc)) => svc.execute(&args),
            _ => self.fallback.execute(params).await,
        }
    }
}

// ============================================================================
// JsonValue 构造工具（确定性、浮点字符串化约定）
// ============================================================================

/// 构造 JsonValue 对象（与 `object_from_pairs` 同义，命名更贴近业务）
pub(crate) fn obj(pairs: Vec<(&str, JsonValue)>) -> JsonValue {
    JsonValue::object_from_pairs(&pairs)
}

/// f64 → JsonValue 字符串（TCB 无 Float 变体，浮点一律字符串化，与 demo-services 约定一致）
pub(crate) fn float_str(f: f64) -> JsonValue {
    JsonValue::string(format!("{f}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[tokio::test]
    async fn test_router_fallback_on_unknown_service() {
        // 未知 service_name → 回落 fallback（这里用一个总是报错的自定义 handler 验证分发）
        struct ErrHandler;
        #[async_trait]
        impl IoHandler for ErrHandler {
            async fn execute(&self, _params: &JsonValue) -> IoResult {
                Err("fallback-called".to_string())
            }
        }
        let router = PhysicsServiceRouter::new(Arc::new(ErrHandler));
        let params = JsonValue::object_from_pairs(&[
            ("io_type", JsonValue::string("call_service")),
            ("service_name", JsonValue::string("unknown_svc")),
        ]);
        let err = router.execute(&params).await.unwrap_err();
        assert_eq!(err, "fallback-called");
    }

    #[test]
    fn test_native_service_table_invariants() {
        // 声明表不变量:服务名唯一;每项构造子可用
        let mut names: Vec<&str> = NATIVE_SERVICES.iter().map(|d| d.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "声明表服务名必须唯一");
        for d in NATIVE_SERVICES {
            let _ = (d.make)();
        }
    }

    #[tokio::test]
    async fn test_all_native_services_route_natively() {
        // 声明表每一项都必须命中原生执行(而非 HTTP 回落)——
        // fallback 用哨兵报错,任何一项落到 fallback 即失败
        struct SentinelHandler;
        #[async_trait]
        impl IoHandler for SentinelHandler {
            async fn execute(&self, _params: &JsonValue) -> IoResult {
                Err("fallback-called".to_string())
            }
        }
        let router = PhysicsServiceRouter::new(Arc::new(SentinelHandler));
        for d in NATIVE_SERVICES {
            let params = JsonValue::object_from_pairs(&[
                ("service_name", JsonValue::string(d.name)),
                ("args", JsonValue::object_from_pairs(&[])),
            ]);
            // 空参数下业务结果可成功可失败,但绝不能是 fallback 哨兵
            if let Err(e) = router.execute(&params).await {
                assert_ne!(e, "fallback-called", "服务 {} 落到了 HTTP 回落", d.name);
            }
        }
    }

    #[test]
    fn test_native_service_table_matches_declaration_file() -> Result<(), String> {
        // 同步守卫(UV-029 声明文件化泛化):本插件声明文件 official_native_services.json 为 SSOT。
        // 本表(name/sensitive/description)与文件三字段+顺序全量比对——
        // 新增服务 = 改文件 + 本表追加 make 项,漂移即失败(不静默)。
        let raw = include_str!("../official_native_services.json");
        let file: serde_json::Value =
            serde_json::from_str(raw).map_err(|e| format!("声明文件非法 JSON: {e}"))?;
        let services = file
            .get("services")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "声明文件缺 services 数组".to_string())?;
        if services.len() != NATIVE_SERVICES.len() {
            return Err(format!(
                "声明文件服务数({})与 NATIVE_SERVICES({})不一致 — 两处必须同步(文件为 SSOT,表持有 make 构造子)",
                services.len(),
                NATIVE_SERVICES.len()
            ));
        }
        for (i, def) in NATIVE_SERVICES.iter().enumerate() {
            let s = &services[i];
            let name = s
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("services[{i}] 缺 name"))?;
            let sensitive = s
                .get("sensitive")
                .and_then(|v| v.as_bool())
                .ok_or_else(|| format!("services[{i}]({name}) 缺 sensitive"))?;
            let description = s
                .get("description")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("services[{i}]({name}) 缺 description"))?;
            if name != def.name || sensitive != def.sensitive || description != def.description {
                return Err(format!(
                    "声明文件与 NATIVE_SERVICES 在第 {i} 项漂移:\n  文件: {name} sensitive={sensitive} {description}\n  表:   {} sensitive={} {}\n声明文件为 SSOT — 请以文件为准修正本表(或有意变更时先改文件)",
                    def.name, def.sensitive, def.description
                ));
            }
        }
        Ok(())
    }

    // ===== UV-030 插件清单化:部署期启用子集 =====

    struct ErrHandler;
    #[async_trait]
    impl IoHandler for ErrHandler {
        async fn execute(&self, _params: &JsonValue) -> IoResult {
            Err("fallback-called".to_string())
        }
    }

    #[test]
    fn test_with_enabled_subset_filter_deterministic() {
        // 子集过滤:仅启用的服务命中原生执行,未启用的回落;
        // 路由查找仍按声明序(与 enabled 传入顺序无关)
        let router = PhysicsServiceRouter::with_enabled(
            Arc::new(ErrHandler),
            &["physics_energy", "physics_simulate"], // 乱序传入
        )
        .unwrap();
        let mut names = router.enabled_service_names();
        names.sort_unstable();
        assert_eq!(names, vec!["physics_energy", "physics_simulate"]);
        // 启用的服务 → 原生(不走 fallback 哨兵)
        let rt = tokio::runtime::Runtime::new().unwrap();
        let params = JsonValue::object_from_pairs(&[
            ("service_name", JsonValue::string("physics_energy")),
            (
                "args",
                JsonValue::object_from_pairs(&[(
                    "bodies",
                    JsonValue::Array(vec![JsonValue::object_from_pairs(&[
                        ("mass", JsonValue::Integer(1)),
                        (
                            "pos",
                            JsonValue::Array(vec![
                                JsonValue::Integer(0),
                                JsonValue::Integer(10),
                                JsonValue::Integer(0),
                            ]),
                        ),
                        (
                            "vel",
                            JsonValue::Array(vec![
                                JsonValue::Integer(0),
                                JsonValue::Integer(0),
                                JsonValue::Integer(0),
                            ]),
                        ),
                    ])]),
                )]),
            ),
        ]);
        let r = rt.block_on(router.execute(&params)).unwrap();
        assert!(r.get("total_mechanical_energy").is_some());
        // 未启用的服务 → 回落
        let params = JsonValue::object_from_pairs(&[
            ("service_name", JsonValue::string("physics_grav_band")),
            ("args", JsonValue::object_from_pairs(&[])),
        ]);
        assert_eq!(
            rt.block_on(router.execute(&params)).unwrap_err(),
            "fallback-called"
        );
    }

    #[test]
    fn test_with_enabled_rejects_unknown_duplicate_and_empty() {
        fn expect_err(r: Result<PhysicsServiceRouter, String>) -> String {
            match r {
                Ok(_) => panic!("应当构造失败"),
                Err(e) => e,
            }
        }
        // 未知名 → Err 含合法名清单与指引
        let err = expect_err(PhysicsServiceRouter::with_enabled(
            Arc::new(ErrHandler),
            &["physics_energy", "no_such_svc"],
        ));
        assert!(err.contains("no_such_svc"), "{err}");
        assert!(err.contains("physics_energy") && err.contains("NATIVE_SERVICES"), "{err}");
        // 重复名 → Err(不静默去重)
        let err = expect_err(PhysicsServiceRouter::with_enabled(
            Arc::new(ErrHandler),
            &["physics_energy", "physics_energy"],
        ));
        assert!(err.contains("重复"), "{err}");
        // 空启用集 → Err(指引改用 enabled=false)
        let err = expect_err(PhysicsServiceRouter::with_enabled(Arc::new(ErrHandler), &[]));
        assert!(err.contains("enabled=false"), "{err}");
    }

    #[tokio::test]
    async fn test_simulate_deterministic_bitwise() {
        // 泛化验证核心证据:同输入两次执行,输出逐位一致(含浮点字符串化形态)
        struct ErrHandler;
        #[async_trait]
        impl IoHandler for ErrHandler {
            async fn execute(&self, _params: &JsonValue) -> IoResult {
                Err("fallback-called".to_string())
            }
        }
        let router = PhysicsServiceRouter::new(Arc::new(ErrHandler));
        let params = JsonValue::object_from_pairs(&[
            ("service_name", JsonValue::string("physics_simulate")),
            (
                "args",
                JsonValue::object_from_pairs(&[
                    (
                        "bodies",
                        JsonValue::Array(vec![
                            JsonValue::object_from_pairs(&[
                                ("mass", JsonValue::Integer(2)),
                                (
                                    "pos",
                                    JsonValue::Array(vec![
                                        JsonValue::Integer(0),
                                        JsonValue::Integer(10),
                                        JsonValue::Integer(0),
                                    ]),
                                ),
                                (
                                    "vel",
                                    JsonValue::Array(vec![
                                        JsonValue::Integer(1),
                                        JsonValue::Integer(0),
                                        JsonValue::Integer(0),
                                    ]),
                                ),
                            ]),
                            JsonValue::object_from_pairs(&[
                                ("mass", JsonValue::string("3.5")),
                                (
                                    "pos",
                                    JsonValue::Array(vec![
                                        JsonValue::string("0.5"),
                                        JsonValue::Integer(12),
                                        JsonValue::Integer(0),
                                    ]),
                                ),
                                (
                                    "vel",
                                    JsonValue::Array(vec![
                                        JsonValue::Integer(0),
                                        JsonValue::Integer(0),
                                        JsonValue::Integer(0),
                                    ]),
                                ),
                            ]),
                        ]),
                    ),
                    ("dt", JsonValue::string("0.01")),
                    ("steps", JsonValue::Integer(100)),
                    ("integrator_order", JsonValue::Integer(2)),
                ]),
            ),
        ]);
        let r1 = router.execute(&params).await.unwrap();
        let r2 = router.execute(&params).await.unwrap();
        assert_eq!(r1.to_string(), r2.to_string(), "同输入两次执行必须逐位一致");
    }
}

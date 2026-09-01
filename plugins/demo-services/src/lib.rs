// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! yuanze-demos 业务服务 Rust 原生实现（Phase 1：服务 Rust 化）
//!
//! # 定位
//! - 把 yuanze-demos 的 7 个 Python FastAPI 服务内嵌为 evorule-server 的原生 `IoHandler`，
//!   使 `io_request(call_service, service_name=...)` 在进程内确定性执行，无需起 Python 服务。
//! - 独立 crate（`plugins/demo-services`），不修改核心 crate，仅经 `IoDispatcher` 挂载。
//!
//! # 路由设计
//! - `DemoServiceRouter` 实现 [IoHandler]，按 `params.service_name` 分发：
//!   - 命中原生服务名 → 调用对应原生实现（进程内，确定性）
//!   - 未命中 → 回落 `ServiceRegistryHandler`（HTTP，兼容其他外部服务）
//! - 原生实现接收的入参 = `params.args`（与 HTTP 版发送的 body 语义一致）。
//!
//! # 与 Python 基线的一致性
//! - 业务返回结构（converged_ok / status / passed 等）与 Python 服务完全一致，
//!   保证 facts/audit 业务语义一致（确定性对比）。
//! - 浮点（TCB 无 Float 变体）一律以字符串返回，与 serde_to_json_value 行为一致。
//! - 墙钟隔离：robot_move 不再用 time/uuid，改用确定性逻辑计数器（确定性 ID）。

#![forbid(unsafe_code)]

pub mod config_persist;
pub mod ik_solver;
pub mod llm_advisor;
pub mod robot_move;
pub mod rule_sandbox;
pub mod sampling;
pub mod shadow_validate;

use std::sync::Arc;

use async_trait::async_trait;
use evorule_reactor::{IoHandler, IoResult};
use evorule_tcb::JsonValue;

use crate::config_persist::ConfigPersist;
use crate::ik_solver::IkSolver;
use crate::llm_advisor::LlmAdvisor;
use crate::robot_move::RobotMove;
use crate::rule_sandbox::RuleSandbox;
use crate::sampling::Sampling;
use crate::shadow_validate::ShadowValidate;

/// 原生服务的统一入参入口：接收 `args`（已由引擎解析 __ 路径为实际值），返回业务结果。
///
/// 注:机制件(trait/声明项/过滤路由器)已上提 `evorule-plugin-kit`(插件 NativeService
/// 抽象上提,自本插件逐行同构机制归一,行为逐字节等价),此处 re-export 保持原 API 路径。
pub use evorule_plugin_kit::{NativeService, NativeServiceDef};

// ============================================================================
// 复合路由：原生优先，HTTP 回落（机制件上提 plugin-kit，本 crate 薄壳委托）
// ============================================================================

/// 复合 IoHandler：`service_name` 命中原生服务名 → 原生执行；否则回落 HTTP。
///
/// 薄壳具名路由器:机制(new / with_enabled 三拒绝 / 声明序查找 / 回落 / split_params)
/// 已上提 `evorule-plugin-kit`(行为逐字节等价),本插件仅自持声明表并保持具名 API。
/// 挂载到 `IoType::call_service()` / `IoType::call_external()`。
pub struct DemoServiceRouter(evorule_plugin_kit::NativeServiceRouter);

impl DemoServiceRouter {
    /// 构造复合路由。`fallback` 为未命中原生服务名时的 HTTP 处理器
    /// （通常传 `Arc<ServiceRegistryHandler>`，由调用方按 --allow-loopback 构造）。
    pub fn new(fallback: Arc<dyn IoHandler>) -> Self {
        Self(evorule_plugin_kit::NativeServiceRouter::new(
            NATIVE_SERVICES,
            fallback,
        ))
    }

    /// 部署期启用子集构造（UV-030 插件清单化;三拒绝语义与错误文案见 plugin-kit,逐字节不变）。
    pub fn with_enabled(fallback: Arc<dyn IoHandler>, enabled: &[&str]) -> Result<Self, String> {
        evorule_plugin_kit::NativeServiceRouter::with_enabled(NATIVE_SERVICES, fallback, enabled)
            .map(Self)
    }

    /// 当前实例已启用的原生服务名列表(健康可见性/能力对账用)
    pub fn enabled_service_names(&self) -> Vec<&'static str> {
        self.0.enabled_service_names()
    }

    /// 原生服务名列表（server 侧服务绑定核对/能力对账用；自声明表派生）
    pub fn native_service_names() -> Vec<&'static str> {
        evorule_plugin_kit::NativeServiceRouter::native_service_names(NATIVE_SERVICES)
    }
}

#[async_trait]
impl IoHandler for DemoServiceRouter {
    async fn execute(&self, params: &JsonValue) -> IoResult {
        self.0.execute(params).await
    }
}

/// 原生服务声明表(SSOT:执行侧原生叶子能力全量清单,顺序即路由查找序)。
pub const NATIVE_SERVICES: &[NativeServiceDef] = &[
    NativeServiceDef {
        name: "inverse_kinematics_solver",
        sensitive: false,
        description: "机器人逆运动学求解(Phase 1 原生)",
        make: mk_ik,
    },
    NativeServiceDef {
        name: "robot_move_joints",
        sensitive: false,
        description: "机器人关节移动(确定性,Phase 1 原生)",
        make: mk_robot,
    },
    NativeServiceDef {
        name: "llm_advisor",
        sensitive: true,
        description: "LLM 建议服务(sensitive:涉及外部 LLM API)",
        make: mk_llm,
    },
    NativeServiceDef {
        name: "shadow_ik_solver",
        sensitive: false,
        description: "影子 IK 求解(对照验证)",
        make: mk_shadow,
    },
    NativeServiceDef {
        name: "sampling_service",
        sensitive: false,
        description: "采样服务",
        make: mk_sampling,
    },
    NativeServiceDef {
        name: "rule_sandbox",
        sensitive: false,
        description: "规则沙箱验证服务",
        make: mk_sandbox,
    },
    NativeServiceDef {
        name: "config_persist",
        sensitive: false,
        description: "规则热加载持久化服务",
        make: mk_config,
    },
];

fn mk_ik() -> Arc<dyn NativeService> {
    Arc::new(IkSolver)
}
fn mk_robot() -> Arc<dyn NativeService> {
    Arc::new(RobotMove::default())
}
fn mk_llm() -> Arc<dyn NativeService> {
    Arc::new(LlmAdvisor)
}
fn mk_shadow() -> Arc<dyn NativeService> {
    Arc::new(ShadowValidate)
}
fn mk_sampling() -> Arc<dyn NativeService> {
    Arc::new(Sampling::default())
}
fn mk_sandbox() -> Arc<dyn NativeService> {
    Arc::new(RuleSandbox)
}
fn mk_config() -> Arc<dyn NativeService> {
    Arc::new(ConfigPersist)
}

// ============================================================================
// JsonValue 构造工具（确定性、BTreeMap 排序）
// ============================================================================

/// 构造 JsonValue 对象（与 `object_from_pairs` 同义，命名更贴近业务）
pub(crate) fn obj(pairs: Vec<(&str, JsonValue)>) -> JsonValue {
    JsonValue::object_from_pairs(&pairs)
}

/// 读取 args 中的字符串字段，无则返回默认值
pub(crate) fn arg_str(v: &JsonValue, key: &str, default: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| default.to_string())
}

/// 读取 args 中的整数字段，无则返回默认值
pub(crate) fn arg_i64(v: &JsonValue, key: &str, default: i64) -> i64 {
    v.get(key).and_then(|x| x.as_i64()).unwrap_or(default)
}

/// 读取 args 中的 f64（兼容 Integer / String 两种形态），无则返回默认值
pub(crate) fn arg_f64(v: &JsonValue, key: &str, default: f64) -> f64 {
    match v.get(key) {
        Some(JsonValue::Integer(i)) => *i as f64,
        Some(JsonValue::String(s)) => s.parse::<f64>().unwrap_or(default),
        _ => default,
    }
}

/// f64 → JsonValue 字符串（TCB 无 Float 变体，浮点一律字符串化，与 serde_to_json_value 一致）
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
        let router = DemoServiceRouter::new(Arc::new(ErrHandler));
        let params = JsonValue::object_from_pairs(&[
            ("io_type", JsonValue::string("call_service")),
            ("service_name", JsonValue::string("unknown_svc")),
        ]);
        let err = router.execute(&params).await.unwrap_err();
        assert_eq!(err, "fallback-called");
    }

    #[tokio::test]
    async fn test_router_routes_config_persist_natively() {
        // config_persist 是原生服务，不应走 fallback（fallback 报错即可证明未命中）
        struct ErrHandler;
        #[async_trait]
        impl IoHandler for ErrHandler {
            async fn execute(&self, _params: &JsonValue) -> IoResult {
                Err("should-not-be-called".to_string())
            }
        }
        let router = DemoServiceRouter::new(Arc::new(ErrHandler));
        let params = JsonValue::object_from_pairs(&[
            ("service_name", JsonValue::string("config_persist")),
            (
                "args",
                JsonValue::object_from_pairs(&[
                    ("operation", JsonValue::string("append_transform")),
                    (
                        "rule",
                        JsonValue::object_from_pairs(&[("type", JsonValue::string("branch"))]),
                    ),
                ]),
            ),
        ]);
        let r = router.execute(&params).await.unwrap();
        assert_eq!(r.get("success").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(r.get("rule_type").and_then(|v| v.as_str()), Some("branch"));
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
        let router = DemoServiceRouter::new(Arc::new(SentinelHandler));
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
        // 同步守卫(UV-029 声明文件化):声明文件 official_native_services.json 为 SSOT。
        // 本表(name/sensitive/description)与文件三字段+顺序全量比对——
        // 新增服务 = 改文件 + 本表追加 make 项,漂移即失败(不静默)。
        // 治理侧经同步脚本消费同一文件(嵌入副本),不再硬编码种子。
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
        let router = DemoServiceRouter::with_enabled(
            Arc::new(ErrHandler),
            &["config_persist", "llm_advisor"], // 乱序传入
        )
        .unwrap();
        let mut names = router.enabled_service_names();
        names.sort_unstable();
        assert_eq!(names, vec!["config_persist", "llm_advisor"]);
        // 启用的服务 → 原生(不走 fallback 哨兵)
        let params = JsonValue::object_from_pairs(&[
            ("service_name", JsonValue::string("config_persist")),
            ("args", JsonValue::object_from_pairs(&[])),
        ]);
        let rt = tokio::runtime::Runtime::new().unwrap();
        if let Err(e) = rt.block_on(router.execute(&params)) {
            assert_ne!(e, "fallback-called", "已启用服务不应落到回落");
        }
        // 未启用的服务 → 回落
        let params = JsonValue::object_from_pairs(&[
            (
                "service_name",
                JsonValue::string("inverse_kinematics_solver"),
            ),
            ("args", JsonValue::object_from_pairs(&[])),
        ]);
        assert_eq!(
            rt.block_on(router.execute(&params)).unwrap_err(),
            "fallback-called"
        );
    }

    #[test]
    fn test_with_enabled_rejects_unknown_duplicate_and_empty() {
        fn expect_err(r: Result<DemoServiceRouter, String>) -> String {
            match r {
                Ok(_) => panic!("应当构造失败"),
                Err(e) => e,
            }
        }
        // 未知名 → Err 含合法名清单与指引
        let err = expect_err(DemoServiceRouter::with_enabled(
            Arc::new(ErrHandler),
            &["config_persist", "no_such_svc"],
        ));
        assert!(err.contains("no_such_svc"), "{err}");
        assert!(
            err.contains("config_persist") && err.contains("NATIVE_SERVICES"),
            "{err}"
        );
        // 重复名 → Err(不静默去重)
        let err = expect_err(DemoServiceRouter::with_enabled(
            Arc::new(ErrHandler),
            &["config_persist", "config_persist"],
        ));
        assert!(err.contains("重复"), "{err}");
        // 空启用集 → Err(指引改用 enabled=false)
        let err = expect_err(DemoServiceRouter::with_enabled(Arc::new(ErrHandler), &[]));
        assert!(err.contains("enabled=false"), "{err}");
    }
}

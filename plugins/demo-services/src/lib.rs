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
pub trait NativeService: Send + Sync {
    fn execute(&self, args: &JsonValue) -> IoResult;
}

// ============================================================================
// 复合路由：原生优先，HTTP 回落
// ============================================================================

/// 复合 IoHandler：`service_name` 命中原生服务名 → 原生执行；否则回落 HTTP。
///
/// 挂载到 `IoType::call_service()` / `IoType::call_external()`。
pub struct DemoServiceRouter {
    ik_solver: IkSolver,
    robot_move: RobotMove,
    llm_advisor: LlmAdvisor,
    shadow_validate: ShadowValidate,
    sampling: Sampling,
    rule_sandbox: RuleSandbox,
    config_persist: ConfigPersist,
    /// 原生未命中时的 HTTP 回落（ServiceRegistryHandler，读 service_registry.json）
    fallback: Arc<dyn IoHandler>,
}

/// 原生服务名白名单（SSOT：与 `execute` 分发的索引一一对应）。
///
/// server 侧服务绑定核对（`SessionApi::import_bundle`）以此集合判定执行侧已绑定的
/// 叶子能力（Phase 1 原生服务）。新增原生服务必须同时更新本常量与下方 match 分支。
pub const NATIVE_SERVICE_NAMES: [&str; 7] = [
    "inverse_kinematics_solver",
    "robot_move_joints",
    "llm_advisor",
    "shadow_ik_solver",
    "sampling_service",
    "rule_sandbox",
    "config_persist",
];

impl DemoServiceRouter {
    /// 构造复合路由。`fallback` 为未命中原生服务名时的 HTTP 处理器
    /// （通常传 `Arc<ServiceRegistryHandler>`，由调用方按 --allow-loopback 构造）。
    pub fn new(fallback: Arc<dyn IoHandler>) -> Self {
        Self {
            ik_solver: IkSolver,
            robot_move: RobotMove::default(),
            llm_advisor: LlmAdvisor,
            shadow_validate: ShadowValidate,
            sampling: Sampling::default(),
            rule_sandbox: RuleSandbox,
            config_persist: ConfigPersist,
            fallback,
        }
    }

    /// 原生服务名列表（SSOT：server 侧服务绑定核对用）
    pub fn native_service_names() -> &'static [&'static str] {
        &NATIVE_SERVICE_NAMES
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
impl IoHandler for DemoServiceRouter {
    async fn execute(&self, params: &JsonValue) -> IoResult {
        let (service_name, args) = Self::split_params(params);
        let name = service_name.as_deref().unwrap_or("");
        // 索引由 NATIVE_SERVICE_NAMES 顺序决定（SSOT）：新增原生服务须同步 const 与分支
        match NATIVE_SERVICE_NAMES.iter().position(|&n| n == name) {
            Some(0) => self.ik_solver.execute(&args),
            Some(1) => self.robot_move.execute(&args),
            Some(2) => self.llm_advisor.execute(&args),
            Some(3) => self.shadow_validate.execute(&args),
            Some(4) => self.sampling.execute(&args),
            Some(5) => self.rule_sandbox.execute(&args),
            Some(6) => self.config_persist.execute(&args),
            _ => self.fallback.execute(params).await,
        }
    }
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
}

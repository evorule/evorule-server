// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 图分派原生服务（常数修订冲击传播分析插件）。
//!
//! # 定位
//! - 对规则集内**常数修订提案**做确定性冲击半径传播分析——纯计算、零 I/O、
//!   零状态、无墙钟，进程内原生执行（`io_request(call_service)`）。
//! - 独立 crate（`plugins/graph-dispatch-services`），不修改核心 crate，
//!   仅经 `IoDispatcher` 挂载（机制同 physics-services / inference-services 先例）。
//! - 语义底座 = `evorule-rule-loader` 装载产物层的常数依赖图
//!   （`dep_graph::build_dependency_graph`：指令-常数引用闭包 + 量纲耦合边
//!   + 显式跨域引用边），服务对调用方传入的规则集文档独立物化同构图后做传播推演。
//!
//! # 路由设计
//! - `GraphDispatchServiceRouter` 实现 [`IoHandler`]，按 `params.service_name` 分发：
//!   - 命中原生服务名 → 调用对应原生实现（进程内，确定性）
//!   - 未命中 → 回落 `ServiceRegistryHandler`（HTTP，兼容其他外部服务）
//! - 原生实现接收的入参 = `params.args`。
//!
//! # 服务清单（SSOT = official_native_services.json，双侧守卫锁定）
//! 1. `graph_shock_propagate` — 常数修订冲击半径传播分析：
//!    引用闭包（depth=1）→ regime 复查（域界闭区间外=失效）→
//!    量纲耦合层间传播 + 跨域单向传播（混合 BFS 至不动点）→ 已裁命题反推；
//!    修订提案强制出处元数据（source 非空 + verified 恒真），
//!    确定性（同输入双跑逐位一致）。

#![forbid(unsafe_code)]

pub mod services;

use std::sync::Arc;

use async_trait::async_trait;
use evorule_reactor::{IoHandler, IoResult};
use evorule_tcb::JsonValue;

// 机制件(trait/声明项/过滤路由器)在 plugin-kit,本 crate 自持声明表+薄壳具名路由器
pub use evorule_plugin_kit::{NativeService, NativeServiceDef};

use crate::services::mk_graph_shock_propagate;

// ============================================================================
// 声明表（SSOT 配对文件: official_native_services.json; 顺序即声明序,
// name/sensitive/description 三字段与顺序均被双侧守卫锁定）
// ============================================================================

/// 原生服务声明表（新增原生能力 = 本表追加一项 + SSOT 文件追加一项 +
/// 运行 sync 脚本同步治理侧嵌入副本）
pub const NATIVE_SERVICES: &[NativeServiceDef] = &[
    NativeServiceDef {
        name: "graph_shock_propagate",
        sensitive: false,
        description: "常数修订冲击半径传播分析(依赖图引用闭包+regime复查+量纲耦合层间传播+已裁命题反推,修订提案强制出处元数据,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"rule_set":{"type":"object","description":"规则集对象,含 metadata.formula_registry(登记条目:instruction_type/formula/dimensions/可选 constants 引用清单)与可选 metadata.constants(常数声明清单:constant_id/version/value_hash/可选 domain_lo/domain_hi)"},"revision":{"type":"object","properties":{"constant_id":{"type":"string","description":"被修订常数 id(须已在 metadata.constants 声明)"},"new_value":{"type":"number","description":"修订后取值"},"new_uncertainty":{"type":"number","description":"可选,修订后不确定度"},"source":{"type":"string","description":"修订出处(非空;修订提案强制出处元数据)"},"verified":{"type":"boolean","description":"必须恰为 true(修订提案验证位)"}},"required":["constant_id","new_value","source","verified"]},"adjudications":{"type":"array","items":{"type":"object","description":"历史裁决条目 {instruction_type, state},调用方从审计链 lifecycle 取数后传入,服务不做生命周期过滤","properties":{"instruction_type":{"type":"string"},"state":{"type":"string"}},"required":["instruction_type","state"]},"description":"历史裁决清单(已裁命题反推数据源)"}},"required":["rule_set","revision","adjudications"]}"#,
        ),
        make: mk_graph_shock_propagate,
    },
];

// ============================================================================
// 复合路由：原生优先，HTTP 回落（机制件在 plugin-kit，本 crate 薄壳委托）
// ============================================================================

/// 复合 IoHandler：`service_name` 命中原生服务名 → 原生执行；否则回落 HTTP。
///
/// 薄壳具名路由器：机制(new / with_enabled 三拒绝 / 声明序查找 / 回落 /
/// split_params)已上提 plugin-kit(行为逐字节等价)，本插件仅自持声明表并保持具名 API。
pub struct GraphDispatchServiceRouter(evorule_plugin_kit::NativeServiceRouter);

impl GraphDispatchServiceRouter {
    /// 构造复合路由。`fallback` 为未命中原生服务名时的 HTTP 处理器
    /// （通常传 `Arc<ServiceRegistryHandler>`，由调用方按 --allow-loopback 构造）。
    pub fn new(fallback: Arc<dyn IoHandler>) -> Self {
        Self(evorule_plugin_kit::NativeServiceRouter::new(
            NATIVE_SERVICES,
            fallback,
        ))
    }

    /// 部署期启用子集构造。
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
impl IoHandler for GraphDispatchServiceRouter {
    async fn execute(&self, params: &JsonValue) -> IoResult {
        self.0.execute(params).await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

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
        let router = GraphDispatchServiceRouter::new(Arc::new(SentinelHandler));
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
        // 同步守卫:本插件声明文件 official_native_services.json 为 SSOT。
        // 本表(name/sensitive/description)与文件三字段+顺序全量比对——
        // 新增服务 = 改文件 + 本表追加 make 项,漂移即失败(不静默)。
        let raw = include_str!("../official_native_services.json");
        let file: serde_json::Value =
            serde_json::from_str(raw).map_err(|e| format!("声明文件非法 JSON: {e}"))?;
        let services = file
            .get("services")
            .and_then(|v| v.as_array())
            .ok_or("声明文件缺 services 数组")?;
        if services.len() != NATIVE_SERVICES.len() {
            return Err(format!(
                "声明文件 {} 项 vs 声明表 {} 项 — 数量不一致",
                services.len(),
                NATIVE_SERVICES.len()
            ));
        }
        for (i, (f, d)) in services.iter().zip(NATIVE_SERVICES).enumerate() {
            let name = f.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let sensitive = f
                .get("sensitive")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let description = f.get("description").and_then(|v| v.as_str()).unwrap_or("");
            if name != d.name {
                return Err(format!("第 {i} 项 name 漂移: 文件={name} 表={}", d.name));
            }
            if sensitive != d.sensitive {
                return Err(format!("服务 {name} sensitive 漂移"));
            }
            if description != d.description {
                return Err(format!("服务 {name} description 漂移"));
            }
        }
        Ok(())
    }
}

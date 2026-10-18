// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 流体力学解析原生服务（伯努利互求/连续性方程/静水压强）。
//!
//! # 定位
//! - 流体族第一批解析子集（全代数精确、教科书公式、黄金值手算可溯），
//!   与 physics-services 的刚体**仿真**内核互补不重叠；建筑供水/化工
//!   配管校验等治理场景的公式底座；
//! - 精确数值原语复用 numeric-services 家族；独立 crate，仅经
//!   `IoDispatcher` 挂载。
//!
//! # 常数分层纪律
//! - **零内置物理常数**——ρ（密度）与 g（重力加速度）一律显式入参。
//!
//! # 路由设计
//! - `FluidServiceRouter` 实现 [`IoHandler`]，按 `params.service_name` 分发：
//!   - 命中原生服务名 → 调用对应原生实现（进程内，确定性）
//!   - 未命中 → 回落 `ServiceRegistryHandler`（HTTP，兼容其他外部服务）
//!
//! # 统一信封 v1
//! - 全部服务成功出参含保留键 `status`（恒 `determined`）+
//!   `determinism`（恒 `bitwise_replayable`）；error 走框架 `Err(String)`
//!   通道并带自诊断指引。
//!
//! # 舍入纪律（同 numeric 家族第一设计约束）
//! 一切可能产生舍入的服务，`scale` 与 `rounding` 均为**必填**参数。
//!
//! # 诚实边界
//! - 伯努利解 v₁/v₂ 需开平方、正负双根 → 如实拒解（不做隐式符号假设）；
//!   p/h 四量线性可解；
//! - 除零退化（流量求解零速度/零面积、静水压 h=0 求 ρ/g）fail-visible；
//! - 物理量符号校验：ρ>0、g>0、h≥0、A>0；
//! - 解析公式互求，非数值仿真——CFD 类需求如实留白（外包宿主通路）。
//!
//! # 服务清单（SSOT = official_native_services.json，双侧守卫锁定）
//! 1. `fluid_bernoulli` — 伯努利方程互求（p/h 线性可解）
//! 2. `fluid_continuity` — 连续性方程互求（含流量输出）
//! 3. `fluid_hydrostatic` — 静水压强互求

#![forbid(unsafe_code)]

use std::sync::Arc;

use async_trait::async_trait;
use evorule_plugin_kit::NativeServiceDef;
use evorule_reactor::{IoHandler, IoResult};
use evorule_tcb::JsonValue;

pub mod services;

use services::{mk_bernoulli, mk_continuity, mk_hydrostatic};

// ============================================================================
// 声明表（SSOT 配对文件: official_native_services.json; 顺序即声明序,
// name/sensitive/description 三字段与顺序均被双侧守卫锁定）
// ============================================================================

/// 原生服务声明表（新增原生能力 = 本表追加一项 + SSOT 文件追加一项 +
/// 运行 sync 脚本同步治理侧嵌入副本）
pub const NATIVE_SERVICES: &[NativeServiceDef] = &[
    NativeServiceDef {
        name: "fluid_bernoulli",
        sensitive: false,
        description: "伯努利方程互求(p+ρv²/2+ρgh 沿流线守恒;p/h 四量线性可解,v 求解开方双根如实拒解;ρ/g 显式入参,精确有理数,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"p1":{"type":"string","description":"上游压强(Pa)"},"v1":{"type":"string","description":"上游流速(m/s)"},"h1":{"type":"string","description":"上游高度(m)"},"p2":{"type":"string","description":"下游压强(Pa)"},"v2":{"type":"string","description":"下游流速(m/s)"},"h2":{"type":"string","description":"下游高度(m)"},"solve":{"type":"string","enum":["p1","v1","h1","p2","v2","h2"]},"rho":{"type":"string","description":"流体密度(kg/m³,正值显式入参)"},"g":{"type":"string","description":"重力加速度(m/s²,正值显式入参)"},"scale":{"type":"integer"},"rounding":{"type":"string","enum":["half_even","half_up","floor","trunc"]}},"required":["solve","rho","g","scale","rounding"]}"#,
        ),
        make: mk_bernoulli,
    },
    NativeServiceDef {
        name: "fluid_continuity",
        sensitive: false,
        description: "连续性方程互求(A₁v₁=A₂v₂,不可压;四量任缺一线性可解,全给一致性校验,输出流量,精确有理数,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"a1":{"type":"string","description":"上游截面积(m²,正值)"},"v1":{"type":"string","description":"上游流速(m/s)"},"a2":{"type":"string","description":"下游截面积(m²,正值)"},"v2":{"type":"string","description":"下游流速(m/s)"},"solve":{"type":"string","enum":["a1","v1","a2","v2"]},"scale":{"type":"integer"},"rounding":{"type":"string","enum":["half_even","half_up","floor","trunc"]}},"required":["solve","scale","rounding"]}"#,
        ),
        make: mk_continuity,
    },
    NativeServiceDef {
        name: "fluid_hydrostatic",
        sensitive: false,
        description: "静水压强互求(p=p₀+ρgh;五量任缺一线性可解,全给一致性校验;ρ/g 显式入参,精确有理数,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"p":{"type":"string","description":"目标深度压强(Pa)"},"p0":{"type":"string","description":"液面压强(Pa)"},"rho":{"type":"string","description":"液体密度(kg/m³,正值显式入参)"},"g":{"type":"string","description":"重力加速度(m/s²,正值显式入参)"},"h":{"type":"string","description":"深度(m,非负)"},"solve":{"type":"string","enum":["p","p0","rho","g","h"]},"scale":{"type":"integer"},"rounding":{"type":"string","enum":["half_even","half_up","floor","trunc"]}},"required":["solve","scale","rounding"]}"#,
        ),
        make: mk_hydrostatic,
    },
];

/// 复合 IoHandler：`service_name` 命中原生服务名 → 原生执行；否则回落 HTTP。
///
/// 薄壳具名路由器：机制已上提 plugin-kit，本插件仅自持声明表并保持具名 API。
pub struct FluidServiceRouter(evorule_plugin_kit::NativeServiceRouter);

impl FluidServiceRouter {
    /// 构造复合路由。`fallback` 为未命中原生服务名时的 HTTP 处理器。
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
impl IoHandler for FluidServiceRouter {
    async fn execute(&self, params: &JsonValue) -> IoResult {
        self.0.execute(params).await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use evorule_tcb::JsonValue;

    #[test]
    fn test_native_service_table_invariants() {
        let mut names: Vec<&str> = NATIVE_SERVICES.iter().map(|d| d.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "声明表服务名必须唯一");
        assert_eq!(total, 3, "本插件声明表应为 3 项");
        for d in NATIVE_SERVICES {
            let _ = (d.make)();
        }
    }

    #[test]
    fn test_declaration_ssot_matches_native_services() {
        let raw = include_str!("../official_native_services.json");
        let doc: serde_json::Value = serde_json::from_str(raw).expect("SSOT 文件须为合法 JSON");
        let items = doc
            .get("services")
            .and_then(|v| v.as_array())
            .expect("SSOT 须含 services 数组");
        assert_eq!(
            items.len(),
            NATIVE_SERVICES.len(),
            "SSOT 条目数与声明表不一致"
        );
        for (i, (item, def)) in items.iter().zip(NATIVE_SERVICES).enumerate() {
            let name = item.get("name").and_then(|v| v.as_str()).expect("name");
            let sensitive = item
                .get("sensitive")
                .and_then(|v| v.as_bool())
                .expect("sensitive");
            let description = item
                .get("description")
                .and_then(|v| v.as_str())
                .expect("description");
            assert_eq!(name, def.name, "第 {i} 条服务名漂移");
            assert_eq!(sensitive, def.sensitive, "服务 {name} sensitive 漂移");
            assert_eq!(description, def.description, "服务 {name} description 漂移");
        }
    }

    #[test]
    fn test_parameters_contract_parses_and_covers_all() {
        for d in NATIVE_SERVICES {
            let raw = d.parameters.expect("每项声明须带 parameters 契约");
            let parsed: serde_json::Value =
                serde_json::from_str(raw).expect("parameters 须为合法 JSON");
            assert_eq!(
                parsed.get("type").and_then(|v| v.as_str()),
                Some("object"),
                "服务 {} 契约 type 须为 object",
                d.name
            );
        }
    }

    #[tokio::test]
    async fn test_all_native_services_route_natively() {
        struct SentinelHandler;
        #[async_trait]
        impl IoHandler for SentinelHandler {
            async fn execute(&self, _params: &JsonValue) -> IoResult {
                Err("fallback-called".to_string())
            }
        }
        let router = FluidServiceRouter::new(Arc::new(SentinelHandler));
        for d in NATIVE_SERVICES {
            let params = JsonValue::object_from_pairs(&[
                ("service_name", JsonValue::string(d.name)),
                ("args", JsonValue::object_from_pairs(&[])),
            ]);
            match router.execute(&params).await {
                Err(e) if e.contains("fallback-called") => {
                    panic!("服务 {} 落到了 HTTP 回落", d.name)
                }
                _ => {}
            }
        }
    }
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 广义相对论解析原生服务（史瓦西半径/近日点进动/光线偏折）。
//!
//! # 定位
//! - 相对论族第一批解析子集（全代数精确）：三个经典验证公式——史瓦西半径、
//!   水星近日点进动（43 角秒/世纪）、太阳掠射光线偏折（1.75 角秒）——
//!   与 gnss-services 的 GPS 钟差修正构成「相对论工程双叙事」；
//! - 精确数值原语复用 numeric-services 家族（定点/有理数/舍入/值解析，
//!   crate 依赖单源不漂移）；独立 crate，仅经 `IoDispatcher` 挂载。
//!
//! # 常数分层纪律
//! - `c = 299792458 m/s`（SI 定义精确值）内置，出参带版本戳；
//! - `G`（万有引力常数）为测量常数，**一律显式入参，零内置**；
//! - `π` 为数学常数：以 50 位截断有理近似内置（`pi-50digit` 版本戳），
//!   近似相对误差 < 10⁻⁴⁹，仅出现在弧度换算面；进动件的弧秒输出中 π
//!   与换算恰好相消，为纯精确有理数。
//!
//! # 路由设计
//! - `GravServiceRouter` 实现 [`IoHandler`]，按 `params.service_name` 分发：
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
//! - 质量非负、几何量正值、偏心率 ∈ [0,1) 否则 fail-visible；
//! - 引力时间膨胀（需开方误差契约）与引力红移等解析件属后续批次；
//! - 本族为解析式求解器，非数值相对论仿真。
//!
//! # 服务清单（SSOT = official_native_services.json，双侧守卫锁定）
//! 1. `grav_schwarzschild_radius` — 史瓦西半径 rs=2GM/c²
//! 2. `grav_perihelion_precession` — 近日点进动（弧秒式精确）
//! 3. `grav_light_deflection` — 光线偏折 4GM/(c²b)

#![forbid(unsafe_code)]

use std::sync::Arc;

use async_trait::async_trait;
use evorule_plugin_kit::NativeServiceDef;
use evorule_reactor::{IoHandler, IoResult};
use evorule_tcb::JsonValue;

pub mod services;

use services::{mk_light_deflection, mk_perihelion_precession, mk_schwarzschild_radius};

// ============================================================================
// 声明表（SSOT 配对文件: official_native_services.json; 顺序即声明序,
// name/sensitive/description 三字段与顺序均被双侧守卫锁定）
// ============================================================================

/// 原生服务声明表（新增原生能力 = 本表追加一项 + SSOT 文件追加一项 +
/// 运行 sync 脚本同步治理侧嵌入副本）
pub const NATIVE_SERVICES: &[NativeServiceDef] = &[
    NativeServiceDef {
        name: "grav_schwarzschild_radius",
        sensitive: false,
        description: "史瓦西半径 rs=2GM/c(G 显式入参,c 为 SI 精确定义内置,精确有理数,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"m":{"type":"string","description":"中心质量(kg,十进制字面量或分数,非负)"},"g":{"type":"string","description":"万有引力常数(m^3/(kg·s^2),测量常数显式入参)"},"scale":{"type":"integer"},"rounding":{"type":"string","enum":["half_even","half_up","floor","trunc"]}},"required":["m","g","scale","rounding"]}"#,
        ),
        make: mk_schwarzschild_radius,
    },
    NativeServiceDef {
        name: "grav_perihelion_precession",
        sensitive: false,
        description: "近日点进动 Delta_phi=3888000GM/(c^2*a*(1-e^2)) 弧秒式精确(水星 43 角秒/世纪经典验证;弧度输出用 pi 的 50 位有理近似带版本戳,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"m":{"type":"string","description":"中心质量(kg,非负)"},"a":{"type":"string","description":"轨道半长轴(m,正值)"},"e":{"type":"string","description":"轨道偏心率([0,1))"},"g":{"type":"string","description":"万有引力常数(测量常数显式入参)"},"orbits":{"type":"integer","description":"轨道圈数(默认 1;水星一世纪约 415 圈)"},"scale":{"type":"integer"},"rounding":{"type":"string","enum":["half_even","half_up","floor","trunc"]}},"required":["m","a","e","g","scale","rounding"]}"#,
        ),
        make: mk_perihelion_precession,
    },
    NativeServiceDef {
        name: "grav_light_deflection",
        sensitive: false,
        description: "光线偏折 alpha=4GM/(c^2*b)(太阳掠射 1.75 角秒经典验证;角秒/弧度中 pi 用 50 位有理近似带版本戳,弧度本体精确,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"m":{"type":"string","description":"中心质量(kg,非负)"},"b":{"type":"string","description":"瞄准参数/最近距离(m,正值,太阳掠射取光球半径)"},"g":{"type":"string","description":"万有引力常数(测量常数显式入参)"},"scale":{"type":"integer"},"rounding":{"type":"string","enum":["half_even","half_up","floor","trunc"]}},"required":["m","b","g","scale","rounding"]}"#,
        ),
        make: mk_light_deflection,
    },
];

/// 复合 IoHandler：`service_name` 命中原生服务名 → 原生执行；否则回落 HTTP。
///
/// 薄壳具名路由器：机制已上提 plugin-kit，本插件仅自持声明表并保持具名 API。
pub struct GravServiceRouter(evorule_plugin_kit::NativeServiceRouter);

impl GravServiceRouter {
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
impl IoHandler for GravServiceRouter {
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
        let router = GravServiceRouter::new(Arc::new(SentinelHandler));
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

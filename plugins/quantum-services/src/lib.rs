// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 量子力学解析原生服务（无限深势阱能级/量子谐振子能级）。
//!
//! # 定位
//! - 量子族第一批解析子集（教科书公式、黄金值手算可溯）：两个最经典的
//!   能级量子化公式，科普与教学场景的直接底座；
//! - 精确数值原语复用 numeric-services 家族（含 ℏ=h/2π 派生原语）；
//!   独立 crate，仅经 `IoDispatcher` 挂载。
//!
//! # 常数分层纪律
//! - `h = 6.62607015×10⁻³⁴ J·s`（SI 2019 定义精确值）内置带版本戳；
//! - `ℏ = h/(2π)`：π 用 50 位截断近似（pi-50digit 版本戳），ℏ 相对误差
//!   < 10⁻⁴⁹；
//! - `eV = 1.602176634×10⁻¹⁹ J`（SI 2019 定义精确值）内置带版本戳；
//! - `m`（粒子质量）、`L`（阱宽）、`ω`（角频率）一律显式入参。
//!
//! # 路由设计
//! - `QuantumServiceRouter` 实现 [`IoHandler`]，按 `params.service_name` 分发：
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
//! - 量子数 n 须为正整数（势阱）/非负整数（谐振子），fail-visible；
//! - 解析能级公式——波函数/叠加态/测量动力学不在本族范围，如实留白。
//!
//! # 服务清单（SSOT = official_native_services.json，双侧守卫锁定）
//! 1. `quantum_infinite_well` — 无限深势阱能级 Eₙ=n²π²ℏ²/(2mL²)
//! 2. `quantum_harmonic_oscillator` — 量子谐振子能级 Eₙ=(n+½)ℏω

#![forbid(unsafe_code)]

use std::sync::Arc;

use async_trait::async_trait;
use evorule_plugin_kit::NativeServiceDef;
use evorule_reactor::{IoHandler, IoResult};
use evorule_tcb::JsonValue;

pub mod services;

use services::{mk_harmonic_oscillator, mk_infinite_well};

// ============================================================================
// 声明表（SSOT 配对文件: official_native_services.json; 顺序即声明序,
// name/sensitive/description 三字段与顺序均被双侧守卫锁定）
// ============================================================================

/// 原生服务声明表（新增原生能力 = 本表追加一项 + SSOT 文件追加一项 +
/// 运行 sync 脚本同步治理侧嵌入副本）
pub const NATIVE_SERVICES: &[NativeServiceDef] = &[
    NativeServiceDef {
        name: "quantum_infinite_well",
        sensitive: false,
        description: "无限深势阱能级 Eₙ=n²π²ℏ²/(2mL²)(ℏ=h/2π 派生:h 为 SI 精确内置带版本戳,π 用 50 位近似;m/L 显式入参;J 与 eV 双单位输出,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"n":{"type":"integer","description":"量子数(正整数)"},"m":{"type":"string","description":"粒子质量(kg,正值,如电子 9.1093837015e-31 需写为分数或十进制)"},"l":{"type":"string","description":"阱宽 L(m,正值)"},"scale":{"type":"integer"},"rounding":{"type":"string","enum":["half_even","half_up","floor","trunc"]}},"required":["n","m","l","scale","rounding"]}"#,
        ),
        make: mk_infinite_well,
    },
    NativeServiceDef {
        name: "quantum_harmonic_oscillator",
        sensitive: false,
        description: "量子谐振子能级 Eₙ=(n+½)ℏω(ℏ 派生同上;ω 显式入参;J 与 eV 双单位输出,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"n":{"type":"integer","description":"量子数(非负整数)"},"omega":{"type":"string","description":"角频率(rad/s,正值)"},"scale":{"type":"integer"},"rounding":{"type":"string","enum":["half_even","half_up","floor","trunc"]}},"required":["n","omega","scale","rounding"]}"#,
        ),
        make: mk_harmonic_oscillator,
    },
];

/// 复合 IoHandler：`service_name` 命中原生服务名 → 原生执行；否则回落 HTTP。
///
/// 薄壳具名路由器：机制已上提 plugin-kit，本插件仅自持声明表并保持具名 API。
pub struct QuantumServiceRouter(evorule_plugin_kit::NativeServiceRouter);

impl QuantumServiceRouter {
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
impl IoHandler for QuantumServiceRouter {
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
        assert_eq!(total, 2, "本插件声明表应为 2 项");
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
        let router = QuantumServiceRouter::new(Arc::new(SentinelHandler));
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

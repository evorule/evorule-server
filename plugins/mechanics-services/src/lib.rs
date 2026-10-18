// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 经典力学与基础电路公式原生服务（牛顿第二定律/匀加速运动学/动量冲量/
//! 机械能/欧姆定律/电阻网络/离散 PID 单步）。
//!
//! # 定位
//! - 公式级代数互求（无时间步进），与 physics-services 的刚体**仿真**
//!   内核互补不重叠；与 inference 量纲族构成「形值双验」组合；
//! - 精确数值原语复用 numeric-services 家族（定点/有理数/舍入/值解析，
//!   crate 依赖单源不漂移）；独立 crate，仅经 `IoDispatcher` 挂载。
//!
//! # 路由设计
//! - `MechanicsServiceRouter` 实现 [`IoHandler`]，按 `params.service_name` 分发：
//!   - 命中原生服务名 → 调用对应原生实现（进程内，确定性）
//!   - 未命中 → 回落 `ServiceRegistryHandler`（HTTP，兼容其他外部服务）
//!
//! # 统一信封 v1
//! - 全部 7 服务成功出参含保留键 `status`（恒 `determined`）+
//!   `determinism`（恒 `bitwise_replayable`）；error 走框架 `Err(String)`
//!   通道并带自诊断指引。
//!
//! # 舍入纪律（同 numeric 家族第一设计约束）
//! 一切可能产生舍入的服务，`scale` 与 `rounding` 均为**必填**参数。
//!
//! # 诚实边界
//! - 运动学 {v₀,a,s}/{a,s,v} 组合开平方双根 → 如实拒解；
//! - 三量全给 → 一致性校验（失败 Err 附两侧值）；
//! - `mechanics_energy_mech` 反解 v 存在正负双根 → 如实拒解；
//! - **零内置物理常数**——g 等一律显式入参。
//!
//! # 服务清单（SSOT = official_native_services.json，双侧守卫锁定）
//! 1. `mechanics_newton_second` — F=m·a 三量互求与一致性校验
//! 2. `mechanics_kinematics_uniform` — 匀加速运动学互求（八组合确定可解）
//! 3. `mechanics_momentum_impulse` — 动量/冲量互求（p=m·v, I=F·t）
//! 4. `mechanics_energy_mech` — 机械能计算（Ek/Ep）
//! 5. `mechanics_ohm_law` — 欧姆定律 U=I·R 三量互求
//! 6. `mechanics_resistance_network` — 串/并联等效电阻
//! 7. `mechanics_pid_step` — 离散 PID 单步（状态显式出入）

#![forbid(unsafe_code)]

use std::sync::Arc;

use async_trait::async_trait;
use evorule_plugin_kit::NativeServiceDef;
use evorule_reactor::{IoHandler, IoResult};
use evorule_tcb::JsonValue;

pub mod relations;
pub mod services;

use services::{
    mk_energy_mech, mk_kinematics_uniform, mk_momentum_impulse, mk_newton_second, mk_ohm_law,
    mk_pid_step, mk_resistance_network,
};

// ============================================================================
// 声明表（SSOT 配对文件: official_native_services.json; 顺序即声明序,
// name/sensitive/description 三字段与顺序均被双侧守卫锁定）
// ============================================================================

/// 原生服务声明表（新增原生能力 = 本表追加一项 + SSOT 文件追加一项 +
/// 运行 sync 脚本同步治理侧嵌入副本）
pub const NATIVE_SERVICES: &[NativeServiceDef] = &[
    NativeServiceDef {
        name: "mechanics_newton_second",
        sensitive: false,
        description: "牛顿第二定律 F=m·a 三量互求与一致性校验(精确有理数,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"f":{"type":"string","description":"力(十进制字面量或分数,N)"},"m":{"type":"string","description":"质量(kg)"},"a":{"type":"string","description":"加速度(m/s²)"},"solve":{"type":"string","enum":["f","m","a"]},"scale":{"type":"integer"},"rounding":{"type":"string","enum":["half_even","half_up","floor","trunc"]}},"required":["solve","scale","rounding"]}"#,
        ),
        make: mk_newton_second,
    },
    NativeServiceDef {
        name: "mechanics_kinematics_uniform",
        sensitive: false,
        description: "匀加速运动学互求(v0/a/t/s/v 任三独立量解其余,双根组合如实拒解,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"known":{"type":"object","description":"恰三个独立量,如 {\"v0\":\"2\",\"a\":\"3\",\"t\":\"4\"}"},"scale":{"type":"integer"},"rounding":{"type":"string","enum":["half_even","half_up","floor","trunc"]}},"required":["known","scale","rounding"]}"#,
        ),
        make: mk_kinematics_uniform,
    },
    NativeServiceDef {
        name: "mechanics_momentum_impulse",
        sensitive: false,
        description: "动量与冲量互求(p=m·v 与 I=F·t,精确有理数,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"relation":{"type":"string","enum":["momentum","impulse"]},"known":{"type":"object","description":"恰两或三个量"},"scale":{"type":"integer"},"rounding":{"type":"string","enum":["half_even","half_up","floor","trunc"]}},"required":["relation","known","scale","rounding"]}"#,
        ),
        make: mk_momentum_impulse,
    },
    NativeServiceDef {
        name: "mechanics_energy_mech",
        sensitive: false,
        description: "机械能计算(Ek=m·v²/2 与 Ep=m·g·h,g 显式入参零内置常数,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"kind":{"type":"string","enum":["ek","ep"]},"known":{"type":"object","description":"按 kind 给定: ek→m/v/Ek 任二, ep→m/g/h/Ep 任三"},"scale":{"type":"integer"},"rounding":{"type":"string","enum":["half_even","half_up","floor","trunc"]}},"required":["kind","known","scale","rounding"]}"#,
        ),
        make: mk_energy_mech,
    },
    NativeServiceDef {
        name: "mechanics_ohm_law",
        sensitive: false,
        description: "欧姆定律 U=I·R 三量互求与一致性校验(精确有理数,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"u":{"type":"string","description":"电压(V)"},"i":{"type":"string","description":"电流(A)"},"r":{"type":"string","description":"电阻(Ω)"},"solve":{"type":"string","enum":["u","i","r"]},"scale":{"type":"integer"},"rounding":{"type":"string","enum":["half_even","half_up","floor","trunc"]}},"required":["solve","scale","rounding"]}"#,
        ),
        make: mk_ohm_law,
    },
    NativeServiceDef {
        name: "mechanics_resistance_network",
        sensitive: false,
        description: "电阻网络等效(串联Σ/并联倒数和,有理数精确,短路如实报告,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"resistors":{"type":"array","items":{"type":"string"},"description":"电阻数组(十进制字面量或分数)"},"topology":{"type":"string","enum":["series","parallel"]},"scale":{"type":"integer"},"rounding":{"type":"string","enum":["half_even","half_up","floor","trunc"]}},"required":["resistors","topology","scale","rounding"]}"#,
        ),
        make: mk_resistance_network,
    },
    NativeServiceDef {
        name: "mechanics_pid_step",
        sensitive: false,
        description: "离散 PID 单步(位置式,状态显式出入无隐藏状态,双跑位元一致,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"kp":{"type":"string"},"ki":{"type":"string"},"kd":{"type":"string"},"setpoint":{"type":"string","description":"目标值"},"measurement":{"type":"string","description":"本次测量值"},"state":{"type":"object","description":"{\"integral\":\"0\",\"prev_error\":\"0\"},首步两值传 0"},"scale":{"type":"integer"},"rounding":{"type":"string","enum":["half_even","half_up","floor","trunc"]}},"required":["kp","ki","kd","setpoint","measurement","state","scale","rounding"]}"#,
        ),
        make: mk_pid_step,
    },
];

/// 复合 IoHandler：`service_name` 命中原生服务名 → 原生执行；否则回落 HTTP。
///
/// 薄壳具名路由器：机制已上提 plugin-kit，本插件仅自持声明表并保持具名 API。
pub struct MechanicsServiceRouter(evorule_plugin_kit::NativeServiceRouter);

impl MechanicsServiceRouter {
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
impl IoHandler for MechanicsServiceRouter {
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
        assert_eq!(total, 7, "本插件声明表应为 7 项");
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
        let router = MechanicsServiceRouter::new(Arc::new(SentinelHandler));
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

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! GNSS/授时原生服务（工厂频率预偏置换算/时间误差与距离误差换算）。
//!
//! # 定位
//! - GPS 卫星钟相对论修正链的确定性计算入口（完整三层修正架构分析见
//!   尽调档）：本插件覆盖纯有理数可闭合的两件——工厂预偏置四量互算与
//!   时间-距离误差换算；含超越函数的偏心率周期修正件属后续批次；
//! - 精确数值原语复用 numeric-services 家族（定点/有理数/舍入/值解析，
//!   crate 依赖单源不漂移）；独立 crate，仅经 `IoDispatcher` 挂载。
//!
//! # 常数分层纪律
//! - `c = 299792458 m/s`（SI 定义精确值）内置，出参带版本戳；
//! - 标称频率/设偏频率/分数偏置/时间/距离一律显式入参，零隐式数据。
//!
//! # 路由设计
//! - `GnssServiceRouter` 实现 [`IoHandler`]，按 `params.service_name` 分发：
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
//! - 三量全给 → 一致性校验（失败 Err 附两侧值）；
//! - 除零退化（nominal=0 / 1+frac_offset=0）→ fail-visible 拒解；
//! - 时间/距离量值负数 → fail-visible 拒解（物理量值非负）；
//! - 偏心率周期修正（需 sin/√a）不在本插件——如实留在后续批次。
//!
//! # 服务清单（SSOT = official_native_services.json，双侧守卫锁定）
//! 1. `gnss_factory_prebias` — 工厂频率预偏置换算（三量互求+每日漂移）
//! 2. `gnss_range_error` — 时间误差与距离误差换算
//! 3. `gnss_eccentricity_correction` — 偏心率周期修正（超越原语+误差契约）
//! 4. `gnss_relativistic_drift` — 相对论漂移分项（GR/SR/净）
//! 5. `gnss_clock_total` — 星钟总修正合成（多项式+周期修正）

#![forbid(unsafe_code)]

use std::sync::Arc;

use async_trait::async_trait;
use evorule_plugin_kit::NativeServiceDef;
use evorule_reactor::{IoHandler, IoResult};
use evorule_tcb::JsonValue;

pub mod services;

use services::{
    mk_clock_total, mk_eccentricity_correction, mk_factory_prebias, mk_range_error,
    mk_relativistic_drift,
};

// ============================================================================
// 声明表（SSOT 配对文件: official_native_services.json; 顺序即声明序,
// name/sensitive/description 三字段与顺序均被双侧守卫锁定）
// ============================================================================

/// 原生服务声明表（新增原生能力 = 本表追加一项 + SSOT 文件追加一项 +
/// 运行 sync 脚本同步治理侧嵌入副本）
pub const NATIVE_SERVICES: &[NativeServiceDef] = &[
    NativeServiceDef {
        name: "gnss_factory_prebias",
        sensitive: false,
        description:
            "GNSS 工厂频率预偏置换算(标称/设偏/分数偏置三量互求+每日漂移纳秒换算,精确有理数,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"nominal_hz":{"type":"string","description":"标称频率(Hz,十进制字面量或分数,如 GPS 星钟 10230000)"},"set_hz":{"type":"string","description":"设偏频率(Hz,工厂预偏后实际设定值)"},"frac_offset":{"type":"string","description":"分数频率偏置(有符号,如 GPS 工厂预偏 -44647/100000000000000)"},"solve":{"type":"string","enum":["nominal_hz","set_hz","frac_offset"]},"scale":{"type":"integer"},"rounding":{"type":"string","enum":["half_even","half_up","floor","trunc"]}},"required":["solve","scale","rounding"]}"#,
        ),
        make: mk_factory_prebias,
    },
    NativeServiceDef {
        name: "gnss_range_error",
        sensitive: false,
        description: "GNSS 时间误差与距离误差换算(c=SI 精确定义内置带版本戳,精确有理数,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"mode":{"type":"string","enum":["time_to_distance","distance_to_time"]},"time_seconds":{"type":"string","description":"时间误差(s,非负,十进制字面量或分数;mode=time_to_distance 必填)"},"distance_m":{"type":"string","description":"距离误差(m,非负;mode=distance_to_time 必填)"},"scale":{"type":"integer"},"rounding":{"type":"string","enum":["half_even","half_up","floor","trunc"]}},"required":["mode","scale","rounding"]}"#,
        ),
        make: mk_range_error,
    },
    NativeServiceDef {
        name: "gnss_eccentricity_correction",
        sensitive: false,
        description: "偏心率周期修正 Δt_r=F·e·√a·sin(E)(F 为 IS-GPS-200 明文内置带版本戳;√a/sin 用定点原语,误差 ≤10⁻ˢᶜᵃˡᵉ 契约,大幅角 undetermined,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"e":{"type":"string","description":"轨道偏心率([0,1))"},"a":{"type":"string","description":"轨道半长轴(m,正值)"},"E":{"type":"string","description":"偏近点角(rad,|E|≤10⁴)"},"scale":{"type":"integer"},"rounding":{"type":"string","enum":["half_even","half_up","floor","trunc"]}},"required":["e","a","E","scale","rounding"]}"#,
        ),
        make: mk_eccentricity_correction,
    },
    NativeServiceDef {
        name: "gnss_relativistic_drift",
        sensitive: false,
        description: "相对论漂移分项(GR/SR/净,μs/天;μ 与参考地球半径内置带版本戳,a 显式入参,精确有理数,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"a":{"type":"string","description":"轨道半长轴(m,正值,GPS 标称 26560000)"},"re":{"type":"string","description":"参考地球半径(m,缺省 6378137 WGS-84 赤道)"},"mu":{"type":"string","description":"地球引力常数(m³/s²,缺省 3.986005×10¹⁴ IS-GPS-200)"},"scale":{"type":"integer"},"rounding":{"type":"string","enum":["half_even","half_up","floor","trunc"]}},"required":["a","scale","rounding"]}"#,
        ),
        make: mk_relativistic_drift,
    },
    NativeServiceDef {
        name: "gnss_clock_total",
        sensitive: false,
        description: "星钟总修正合成(广播多项式含半周翻转 + 偏心率周期修正,双分量+合成值纳秒呈现,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"af0":{"type":"string","description":"钟偏置(s)"},"af1":{"type":"string","description":"漂移(s/s)"},"af2":{"type":"string","description":"漂移率(s/s²)"},"t_oc":{"type":"string","description":"钟差参考时刻(s)"},"t":{"type":"string","description":"当前时刻(s,dt 折入 ±302400)"},"e":{"type":"string","description":"轨道偏心率([0,1))"},"a":{"type":"string","description":"轨道半长轴(m,正值)"},"E":{"type":"string","description":"偏近点角(rad,|E|≤10⁴)"},"scale":{"type":"integer"},"rounding":{"type":"string","enum":["half_even","half_up","floor","trunc"]}},"required":["af0","af1","af2","t_oc","t","e","a","E","scale","rounding"]}"#,
        ),
        make: mk_clock_total,
    },
];

/// 复合 IoHandler：`service_name` 命中原生服务名 → 原生执行；否则回落 HTTP。
///
/// 薄壳具名路由器：机制已上提 plugin-kit，本插件仅自持声明表并保持具名 API。
pub struct GnssServiceRouter(evorule_plugin_kit::NativeServiceRouter);

impl GnssServiceRouter {
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
impl IoHandler for GnssServiceRouter {
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
        assert_eq!(total, 5, "本插件声明表应为 5 项");
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
        let router = GnssServiceRouter::new(Arc::new(SentinelHandler));
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

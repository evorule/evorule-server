// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 治理数学原生服务（比例分配/数组聚合/时长差/区间对）。
//!
//! # 定位
//! - 治理数学 Tier 2（05 号归属裁定）：乘除/聚合/选秩类计算面能力，
//!   与 band_check 等 Tier 1 立法指令构成规则层"判定+计算"分工；
//! - 精确数值原语复用 numeric-services 家族；独立 crate，仅经
//!   `IoDispatcher` 挂载。
//!
//! # 路由设计
//! - `GovServiceRouter` 实现 [`IoHandler`]，按 `params.service_name` 分发：
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
//! - 全零权重（份额无定义）/空数组（聚合无定义）→ fail-visible；
//! - 最大余数法仅对非负整数 total+非负权重定义（负份额无治理语义）；
//! - duration 负值如实输出（end<start 属输入事实,不截断不报错）。
//!
//! # 服务清单（SSOT = official_native_services.json，双侧守卫锁定）
//! 1. `gov_ratio_split` — 比例分配（最大余数法整数分配可选）
//! 2. `gov_aggregate` — 数组聚合（sum/avg/median/min/max）
//! 3. `gov_window_duration` — 时长差 end−start
//! 4. `gov_window_overlap` — 区间对判定

#![forbid(unsafe_code)]

use std::sync::Arc;

use async_trait::async_trait;
use evorule_plugin_kit::NativeServiceDef;
use evorule_reactor::{IoHandler, IoResult};
use evorule_tcb::JsonValue;

pub mod services;

use services::{mk_aggregate, mk_ratio_split, mk_window_duration, mk_window_overlap};

// ============================================================================
// 声明表（SSOT 配对文件: official_native_services.json; 顺序即声明序,
// name/sensitive/description 三字段与顺序均被双侧守卫锁定）
// ============================================================================

/// 原生服务声明表（新增原生能力 = 本表追加一项 + SSOT 文件追加一项 +
/// 运行 sync 脚本同步治理侧嵌入副本）
pub const NATIVE_SERVICES: &[NativeServiceDef] = &[
    NativeServiceDef {
        name: "gov_ratio_split",
        sensitive: false,
        description: "比例分配(total 按 weights 精确有理份额;mode=integer 时最大余数法整数分配保证和恰为 total;零浮点,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"total":{"type":"string","description":"待分配总量(非负整数字面量;integer 模式下为可分整数,如金额分)"},"weights":{"type":"array","items":{"type":"string"},"description":"权重数组(非负,十进制字面量或分数;全零权重 fail-visible)"},"mode":{"type":"string","enum":["exact","integer"],"description":"exact=精确有理份额;integer=最大余数法整数分配(和恰为 total)"},"scale":{"type":"integer"},"rounding":{"type":"string","enum":["half_even","half_up","floor","trunc"]}},"required":["total","weights","mode","scale","rounding"]}"#,
        ),
        make: mk_ratio_split,
    },
    NativeServiceDef {
        name: "gov_aggregate",
        sensitive: false,
        description: "数组聚合(sum/avg/median/min/max,精确有理数;median 偶数个取中间两数平均,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"values":{"type":"array","items":{"type":"string"},"description":"数值数组(十进制字面量或分数,至少 1 项)"},"op":{"type":"string","enum":["sum","avg","median","min","max"]},"scale":{"type":"integer"},"rounding":{"type":"string","enum":["half_even","half_up","floor","trunc"]}},"required":["values","op","scale","rounding"]}"#,
        ),
        make: mk_aggregate,
    },
    NativeServiceDef {
        name: "gov_window_duration",
        sensitive: false,
        description: "时长差 end−start(epoch 秒 i64,精确;负值如实输出不截断,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"end":{"type":"integer","description":"结束时刻(epoch 秒)"},"start":{"type":"integer","description":"起始时刻(epoch 秒)"},"scale":{"type":"integer"},"rounding":{"type":"string","enum":["half_even","half_up","floor","trunc"]}},"required":["end","start","scale","rounding"]}"#,
        ),
        make: mk_window_duration,
    },
    NativeServiceDef {
        name: "gov_window_overlap",
        sensitive: false,
        description: "区间对判定(overlap 长度 max(0,min(e)−max(s)) 与关系标签 no_overlap/adjacent/overlap/contained_1_in_2/contained_2_in_1/identical,精确有理数,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"s1":{"type":"string","description":"区间 1 起(十进制字面量或分数)"},"e1":{"type":"string","description":"区间 1 止(须 ≥ s1)"},"s2":{"type":"string","description":"区间 2 起"},"e2":{"type":"string","description":"区间 2 止(须 ≥ s2)"},"scale":{"type":"integer"},"rounding":{"type":"string","enum":["half_even","half_up","floor","trunc"]}},"required":["s1","e1","s2","e2","scale","rounding"]}"#,
        ),
        make: mk_window_overlap,
    },
];

/// 复合 IoHandler：`service_name` 命中原生服务名 → 原生执行；否则回落 HTTP。
///
/// 薄壳具名路由器：机制已上提 plugin-kit，本插件仅自持声明表并保持具名 API。
pub struct GovServiceRouter(evorule_plugin_kit::NativeServiceRouter);

impl GovServiceRouter {
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
impl IoHandler for GovServiceRouter {
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
        assert_eq!(total, 4, "本插件声明表应为 4 项");
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
        let router = GovServiceRouter::new(Arc::new(SentinelHandler));
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

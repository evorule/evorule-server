// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 精确算术原生服务（有理数/定点十进制/数论原子/利息摊销/单位换算/量纲感知求值）。
//!
//! # 定位
//! - 补齐 TCB 无 Float 数据模型下的**精确数值计算通路**：整数 / 有理数 /
//!   定点十进制（字符串编码 + 显式标度 + 显式舍入模式）三条表示，
//!   全程 `bitwise_replayable`；
//! - 独立 crate（`plugins/numeric-services`），不修改核心 crate，
//!   仅经 `IoDispatcher` 挂载（机制同 inference-services 先例）；
//! - 量纲感知求值复用 inference 家族的 SI 量纲原语（单源不漂移）。
//!
//! # 路由设计
//! - `NumericServiceRouter` 实现 [`IoHandler`]，按 `params.service_name` 分发：
//!   - 命中原生服务名 → 调用对应原生实现（进程内，确定性）
//!   - 未命中 → 回落 `ServiceRegistryHandler`（HTTP，兼容其他外部服务）
//! - 原生实现接收的入参 = `params.args`。
//!
//! # 统一信封 v1
//! - 全部 8 服务成功出参含保留键 `status`（恒 `determined`）+
//!   `determinism`（恒 `bitwise_replayable`：纯整数/有理数域，同二进制
//!   同输入双跑位元一致）；error 走框架 `Err(String)` 通道并带自诊断指引。
//!
//! # 舍入纪律（本族第一设计约束）
//! 一切可能产生舍入的服务，`scale` 与 `rounding` 均为**必填**参数——
//! 杜绝隐式舍入；表达式求值全程精确有理运算，仅在输出端单次舍入。
//!
//! # 服务清单（SSOT = official_native_services.json，双侧守卫锁定）
//! 1. `numeric_rational_eval` — 有理数表达式精确求值（既约分数输出）
//! 2. `numeric_decimal_eval` — 定点十进制表达式求值（精确运算+输出端单次舍入）
//! 3. `numeric_decimal_convert` — 分数与十进制互转（循环小数如实标注）
//! 4. `numeric_number_theory` — 数论原子（gcd/lcm/factorial/nCr/nPr）
//! 5. `numeric_fin_interest` — 单利/复利精确计算
//! 6. `numeric_fin_amortize` — 等额本息/等额本金摊销表（尾差并入末期）
//! 7. `numeric_unit_convert` — 单位换算（换算率由调用方显式提供）
//! 8. `numeric_dim_eval` — 量纲感知表达式求值（复用量纲原语）

#![forbid(unsafe_code)]

use std::sync::Arc;

use async_trait::async_trait;
use evorule_plugin_kit::NativeServiceDef;
use evorule_reactor::{IoHandler, IoResult};
use evorule_tcb::JsonValue;

pub mod numcore;
pub mod numtheory;
pub mod services;

use services::{
    mk_decimal_convert, mk_decimal_eval, mk_dim_eval, mk_fin_amortize, mk_fin_interest,
    mk_number_theory, mk_rational_eval, mk_unit_convert,
};

// ============================================================================
// 声明表（SSOT 配对文件: official_native_services.json; 顺序即声明序,
// name/sensitive/description 三字段与顺序均被双侧守卫锁定）
// ============================================================================

/// 原生服务声明表（新增原生能力 = 本表追加一项 + SSOT 文件追加一项 +
/// 运行 sync 脚本同步治理侧嵌入副本）
pub const NATIVE_SERVICES: &[NativeServiceDef] = &[
    NativeServiceDef {
        name: "numeric_rational_eval",
        sensitive: false,
        description: "有理数表达式精确求值(整数与四则/整数幂,既约分数输出,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"expr":{"type":"string","description":"整数有理表达式,如 1/3 + 1/6、(2+3)*4、2^-2"}},"required":["expr"]}"#,
        ),
        make: mk_rational_eval,
    },
    NativeServiceDef {
        name: "numeric_decimal_eval",
        sensitive: false,
        description:
            "定点十进制表达式求值(支持 vars 变量绑定,全程精确有理运算,输出端按 scale+rounding 单次舍入,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"expr":{"type":"string","description":"十进制表达式,可引用 vars 中的变量名"},"vars":{"type":"object","description":"可选变量绑定: 变量名 → 十进制字面量或 a/b 分数","additionalProperties":{"type":"string"}},"scale":{"type":"integer","description":"结果小数位(0-36)"},"rounding":{"type":"string","enum":["half_even","half_up","floor","trunc"],"description":"舍入模式(必填,无隐式舍入)"}},"required":["expr","scale","rounding"]}"#,
        ),
        make: mk_decimal_eval,
    },
    NativeServiceDef {
        name: "numeric_decimal_convert",
        sensitive: false,
        description: "分数与十进制互转(终止性如实标注,无限小数须显式舍入,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"value":{"type":"string","description":"分数(a/b)或十进制字面量"},"target":{"type":"string","enum":["fraction","decimal"]},"scale":{"type":"integer","description":"target=decimal 且无限小数时必填"},"rounding":{"type":"string","enum":["half_even","half_up","floor","trunc"]}},"required":["value","target"]}"#,
        ),
        make: mk_decimal_convert,
    },
    NativeServiceDef {
        name: "numeric_number_theory",
        sensitive: false,
        description: "数论原子(gcd/lcm/factorial/nCr/nPr,大整数精确,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"op":{"type":"string","enum":["gcd","lcm","factorial","nCr","nPr"]},"args":{"type":"array","items":{"type":"integer"},"description":"op 所需整数参数(gcd/lcm/nCr/nPr 两项,factorial 一项)"}},"required":["op","args"]}"#,
        ),
        make: mk_number_theory,
    },
    NativeServiceDef {
        name: "numeric_fin_interest",
        sensitive: false,
        description: "单利/复利精确计算(中间量全程有理数精确,末端显式舍入,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"principal":{"type":"string","description":"本金(十进制字面量或分数)"},"rate":{"type":"string","description":"每期利率(十进制字面量或分数)"},"periods":{"type":"integer","description":"期数(0-1200)"},"mode":{"type":"string","enum":["simple","compound"]},"scale":{"type":"integer"},"rounding":{"type":"string","enum":["half_even","half_up","floor","trunc"]}},"required":["principal","rate","periods","mode","scale","rounding"]}"#,
        ),
        make: mk_fin_interest,
    },
    NativeServiceDef {
        name: "numeric_fin_amortize",
        sensitive: false,
        description: "等额本息/等额本金摊销表(逐期显式舍入,尾差并入末期,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"principal":{"type":"string"},"rate":{"type":"string","description":"每期利率(十进制字面量或分数)"},"terms":{"type":"integer","description":"期数(1-3600)"},"method":{"type":"string","enum":["annuity","straight"],"description":"annuity=等额本息, straight=等额本金"},"scale":{"type":"integer"},"rounding":{"type":"string","enum":["half_even","half_up","floor","trunc"]}},"required":["principal","rate","terms","method","scale","rounding"]}"#,
        ),
        make: mk_fin_amortize,
    },
    NativeServiceDef {
        name: "numeric_unit_convert",
        sensitive: false,
        description: "单位换算(换算率由调用方显式提供,服务不内置任何率,有理数中转末端舍入,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"value":{"type":"string","description":"待换算值(十进制字面量或分数)"},"from":{"type":"string"},"to":{"type":"string"},"rate_table":{"type":"object","description":"换算率表,键形如 来源/目标,值为定点率字符串","additionalProperties":{"type":"string"}},"scale":{"type":"integer"},"rounding":{"type":"string","enum":["half_even","half_up","floor","trunc"]}},"required":["value","from","to","rate_table","scale","rounding"]}"#,
        ),
        make: mk_unit_convert,
    },
    NativeServiceDef {
        name: "numeric_dim_eval",
        sensitive: false,
        description: "量纲感知表达式求值(单位字面量解析+量纲检查+精确求值,复用量纲原语,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"expr":{"type":"string","description":"带单位表达式,如 (3 m/s) * (2 s)"},"scale":{"type":"integer"},"rounding":{"type":"string","enum":["half_even","half_up","floor","trunc"]}},"required":["expr","scale","rounding"]}"#,
        ),
        make: mk_dim_eval,
    },
];

/// 复合 IoHandler：`service_name` 命中原生服务名 → 原生执行；否则回落 HTTP。
///
/// 薄壳具名路由器：机制(new / with_enabled 三拒绝 / 声明序查找 / 回落 /
/// split_params)已上提 plugin-kit，本插件仅自持声明表并保持具名 API。
pub struct NumericServiceRouter(evorule_plugin_kit::NativeServiceRouter);

impl NumericServiceRouter {
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
impl IoHandler for NumericServiceRouter {
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
        // 声明表不变量:服务名唯一;每项构造子可用
        let mut names: Vec<&str> = NATIVE_SERVICES.iter().map(|d| d.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "声明表服务名必须唯一");
        assert_eq!(total, 8, "本插件声明表应为 8 项");
        for d in NATIVE_SERVICES {
            let _ = (d.make)();
        }
    }

    #[test]
    fn test_declaration_ssot_matches_native_services() {
        // 同步守卫:本插件声明文件 official_native_services.json 为 SSOT。
        // 本测试锁 name/sensitive/description 三字段 + 顺序（漂移即红,
        // 强制显式重跑 sync 脚本同步治理侧嵌入副本）。
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
        // 契约门禁:8 项声明全部带 parameters,且逐项可解析为 JSON Schema 对象
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
            assert!(
                parsed
                    .get("properties")
                    .and_then(|v| v.as_object())
                    .is_some(),
                "服务 {} 契约须含 properties",
                d.name
            );
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
        let router = NumericServiceRouter::new(Arc::new(SentinelHandler));
        for d in NATIVE_SERVICES {
            let params = JsonValue::object_from_pairs(&[
                ("service_name", JsonValue::string(d.name)),
                ("args", JsonValue::object_from_pairs(&[])),
            ]);
            // 不要求业务成功(缺参 Err 亦证明命中原生执行),唯独不允许 fallback-called
            match router.execute(&params).await {
                Err(e) if e.contains("fallback-called") => {
                    panic!("服务 {} 落到了 HTTP 回落", d.name)
                }
                _ => {}
            }
        }
    }
}

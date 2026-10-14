// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 推理分析原生服务（第一个确定性静态分析插件：量纲检查）。
//!
//! # 定位
//! - 对 SI 单位与物理公式做**确定性静态量纲分析**——纯计算、零 I/O、
//!   零状态、无墙钟，进程内原生执行（`io_request(call_service)`）。
//! - 独立 crate（`plugins/inference-services`），不修改核心 crate，
//!   仅经 `IoDispatcher` 挂载（机制同 physics-services 先例）。
//! - 算法对照上游参考实现逐函数移植，测试预言（期望值）同源。
//!
//! # 路由设计
//! - `InferenceServiceRouter` 实现 [`IoHandler`]，按 `params.service_name` 分发：
//!   - 命中原生服务名 → 调用对应原生实现（进程内，确定性）
//!   - 未命中 → 回落 `ServiceRegistryHandler`（HTTP，兼容其他外部服务）
//! - 原生实现接收的入参 = `params.args`。
//!
//! # 服务清单（SSOT = official_native_services.json，双侧守卫锁定）
//! 1. `inference_dimension_from_unit` — SI 单位字符串 → 七分量量纲向量
//! 2. `inference_dimension_compare_units` — 两单位量纲等价性比较
//! 3. `inference_dimension_verify_formula` — 物理公式量纲一致性验证

#![forbid(unsafe_code)]

pub mod dimension;
pub mod services;

use std::sync::Arc;

use async_trait::async_trait;
use evorule_reactor::{IoHandler, IoResult};
use evorule_tcb::JsonValue;

// 机制件(trait/声明项/过滤路由器)在 plugin-kit,本 crate 自持声明表+薄壳具名路由器
pub use evorule_plugin_kit::{NativeService, NativeServiceDef};

use crate::services::{mk_compare, mk_from_unit, mk_verify_formula};

// ============================================================================
// 声明表（SSOT 配对文件: official_native_services.json; 顺序即声明序,
// name/sensitive/description 三字段与顺序均被双侧守卫锁定）
// ============================================================================

/// 原生服务声明表（新增原生能力 = 本表追加一项 + SSOT 文件追加一项 +
/// 运行 sync 脚本同步治理侧嵌入副本）
pub const NATIVE_SERVICES: &[NativeServiceDef] = &[
    NativeServiceDef {
        name: "inference_dimension_from_unit",
        sensitive: false,
        description: "SI 单位字符串解析为七分量量纲向量([L,M,T,I,Θ,N,J],确定性)",
        make: mk_from_unit,
    },
    NativeServiceDef {
        name: "inference_dimension_compare_units",
        sensitive: false,
        description: "两 SI 单位量纲等价性比较(七分量向量逐位相等判定,确定性)",
        make: mk_compare,
    },
    NativeServiceDef {
        name: "inference_dimension_verify_formula",
        sensitive: false,
        description: "物理公式量纲一致性验证(已知变量量纲推导四则与幂表达式,确定性)",
        make: mk_verify_formula,
    },
];

// ============================================================================
// 复合路由：原生优先，HTTP 回落（机制件在 plugin-kit，本 crate 薄壳委托）
// ============================================================================

/// 复合 IoHandler：`service_name` 命中原生服务名 → 原生执行；否则回落 HTTP。
///
/// 薄壳具名路由器：机制(new / with_enabled 三拒绝 / 声明序查找 / 回落 /
/// split_params)已上提 plugin-kit(行为逐字节等价)，本插件仅自持声明表并保持具名 API。
pub struct InferenceServiceRouter(evorule_plugin_kit::NativeServiceRouter);

impl InferenceServiceRouter {
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
impl IoHandler for InferenceServiceRouter {
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
        let router = InferenceServiceRouter::new(Arc::new(SentinelHandler));
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

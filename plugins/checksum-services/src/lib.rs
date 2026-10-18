// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 校验位原生服务（Luhn/ISBN-10/ISBN-13/IBAN mod-97）。
//!
//! # 定位
//! - 法律/金融/票证场景的标识符校验位族：银行卡（Luhn）、书号（ISBN）、
//!   国际银行账号（IBAN mod-97）——全部纯整数运算，validate/补位双向；
//! - 精确数值原语复用 numeric-services 家族；独立 crate，仅经
//!   `IoDispatcher` 挂载。
//!
//! # 常数与数据分层
//! - 算法零常数零数据表（权重序列内生于公式）；
//! - IBAN 国家长度表**不内置**（如实声明：结构+mod-97 已定，长度合规
//!   由调用方按 ISO 13616 各国注册表核对）。
//!
//! # 路由设计
//! - `ChecksumServiceRouter` 实现 [`IoHandler`]，按 `params.service_name` 分发：
//!   - 命中原生服务名 → 调用对应原生实现（进程内，确定性）
//!   - 未命中 → 回落 `ServiceRegistryHandler`（HTTP，兼容其他外部服务）
//!
//! # 统一信封 v1
//! - 全部服务成功出参含保留键 `status`（恒 `determined`）+
//!   `determinism`（恒 `bitwise_replayable`）；error 走框架 `Err(String)`
//!   通道并带自诊断指引。校验失败（校验位不匹配）不是 error——
//!   是 `valid=false` 的正常判定结果（fail-visible 与业务判定分离）。
//!
//! # 诚实边界
//! - 非法字符（非数字/非规定字母）fail-visible；
//! - 校验位算法只保证「格式合法」——号码是否真实存在/有效属外部事实，
//!   不在本服务范围；
//! - ISBN-10 的 X 校验位按标准接受并如实回显。
//!
//! # 服务清单（SSOT = official_native_services.json，双侧守卫锁定）
//! 1. `checksum_luhn` — Luhn 模 10（validate/append）
//! 2. `checksum_isbn` — ISBN-10/13（validate/check_digit）
//! 3. `checksum_iban` — IBAN mod-97（validate/check_digits）

#![forbid(unsafe_code)]

use std::sync::Arc;

use async_trait::async_trait;
use evorule_plugin_kit::NativeServiceDef;
use evorule_reactor::{IoHandler, IoResult};
use evorule_tcb::JsonValue;

pub mod services;

use services::{mk_checksum_iban, mk_checksum_isbn, mk_checksum_luhn};

// ============================================================================
// 声明表（SSOT 配对文件: official_native_services.json; 顺序即声明序,
// name/sensitive/description 三字段与顺序均被双侧守卫锁定）
// ============================================================================

/// 原生服务声明表（新增原生能力 = 本表追加一项 + SSOT 文件追加一项 +
/// 运行 sync 脚本同步治理侧嵌入副本）
pub const NATIVE_SERVICES: &[NativeServiceDef] = &[
    NativeServiceDef {
        name: "checksum_luhn",
        sensitive: false,
        description: "Luhn 模 10 校验(银行卡号/IMEI 等;validate 校验/append 补位,纯整数,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"mode":{"type":"string","enum":["validate","append"],"description":"validate=校验含校验位的完整号码;append=为载荷补算校验位"},"value":{"type":"string","description":"数字串(validate=完整号码;append=不含校验位的载荷)"}},"required":["mode","value"]}"#,
        ),
        make: mk_checksum_luhn,
    },
    NativeServiceDef {
        name: "checksum_isbn",
        sensitive: false,
        description: "ISBN 校验(isbn10 模 11 含 X 位/isbn13 交替权 1-3;validate 校验/check_digit 补位,纯整数,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"kind":{"type":"string","enum":["isbn10","isbn13"]},"mode":{"type":"string","enum":["validate","check_digit"],"description":"validate=校验完整 ISBN;check_digit=为前 9/12 位补算校验位"},"value":{"type":"string","description":"ISBN(validate=完整;check_digit=前 9/12 位;isbn10 校验位可为 X,连字符自动剥离)"}},"required":["kind","mode","value"]}"#,
        ),
        make: mk_checksum_isbn,
    },
    NativeServiceDef {
        name: "checksum_iban",
        sensitive: false,
        description: "IBAN mod-97 校验(ISO 13616;validate 校验/check_digits 补算两位校验码;结构+mod97,国家长度表不内置如实声明,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"mode":{"type":"string","enum":["validate","check_digits"],"description":"validate=校验完整 IBAN;check_digits=按国家码+基本账号补算两位校验码"},"value":{"type":"string","description":"validate=完整 IBAN(空格自动剥离);check_digits=国家码+基本账号 BBAN(两位校验码位置传 00)"}},"required":["mode","value"]}"#,
        ),
        make: mk_checksum_iban,
    },
];

/// 复合 IoHandler：`service_name` 命中原生服务名 → 原生执行；否则回落 HTTP。
///
/// 薄壳具名路由器：机制已上提 plugin-kit，本插件仅自持声明表并保持具名 API。
pub struct ChecksumServiceRouter(evorule_plugin_kit::NativeServiceRouter);

impl ChecksumServiceRouter {
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
impl IoHandler for ChecksumServiceRouter {
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
        let router = ChecksumServiceRouter::new(Arc::new(SentinelHandler));
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

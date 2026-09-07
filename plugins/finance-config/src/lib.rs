// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! finance-config 原生插件 —— 财务域配置键读写。
//!
//! 机制件: trait/声明项/过滤路由器已上提 evorule-plugin-kit；本 crate 自持声明表 + 薄壳委托。

#![forbid(unsafe_code)]

pub mod config_service;
pub mod store;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;

use async_trait::async_trait;
use evorule_reactor::{IoHandler, IoResult};
use evorule_tcb::JsonValue;

pub use crate::config_service::{make_get, make_set};
pub use crate::store::ConfigStore;
pub use evorule_plugin_kit::{NativeService, NativeServiceDef};

// ============================================================================
// 全局 store 单例（OnceLock 延迟初始化）
// ============================================================================

static STORE: OnceLock<Arc<ConfigStore>> = OnceLock::new();

fn store() -> Arc<ConfigStore> {
    STORE
        .get_or_init(|| {
            let path = std::env::var("EVORULE_FINANCE_CONFIG_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("data/config/finance-config.json"));
            Arc::new(
                ConfigStore::open(&path).unwrap_or_else(|e| {
                    eprintln!(
                        "[finance-config] ❌ Store 初始化失败: {e}。自诊断指引: \
                         ① 确认 data/config/ 目录可写; \
                         ② 检查 finance-config.json 格式（JSON 文件损坏时删除后由系统重建）; \
                         ③ 若自定义路径，设置环境变量 EVORULE_FINANCE_CONFIG_PATH 指向合法文件。"
                    );
                    panic!("finance-config store 初始化失败: {e}")
                }),
            )
        })
        .clone()
}

// ============================================================================
// 声明表（SSOT: official_native_services.json 为唯一事实源）
// ============================================================================

pub const NATIVE_SERVICES: &[NativeServiceDef] = &[
    NativeServiceDef {
        name: "finance_config_get",
        sensitive: false,
        description: "财务配置键读取（fail-fast，key 不存在返回显式错误）",
        make: mk_get,
    },
    NativeServiceDef {
        name: "finance_config_set",
        sensitive: true,
        description: "财务配置键写入（需人审门确认后落库，走审计链）",
        make: mk_set,
    },
];

fn mk_get() -> Arc<dyn NativeService> {
    make_get(store())
}

fn mk_set() -> Arc<dyn NativeService> {
    make_set(store())
}

// ============================================================================
// 薄壳具名路由器
// ============================================================================

pub struct FinanceConfigRouter(evorule_plugin_kit::NativeServiceRouter);

impl FinanceConfigRouter {
    pub fn new(fallback: Arc<dyn IoHandler>) -> Self {
        Self(evorule_plugin_kit::NativeServiceRouter::new(
            NATIVE_SERVICES,
            fallback,
        ))
    }

    pub fn with_enabled(
        fallback: Arc<dyn IoHandler>,
        enabled: &[&str],
    ) -> Result<Self, String> {
        evorule_plugin_kit::NativeServiceRouter::with_enabled(
            NATIVE_SERVICES,
            fallback,
            enabled,
        )
        .map(Self)
    }

    pub fn enabled_service_names(&self) -> Vec<&'static str> {
        self.0.enabled_service_names()
    }

    pub fn native_service_names() -> Vec<&'static str> {
        evorule_plugin_kit::NativeServiceRouter::native_service_names(NATIVE_SERVICES)
    }
}

#[async_trait]
impl IoHandler for FinanceConfigRouter {
    async fn execute(&self, params: &JsonValue) -> IoResult {
        self.0.execute(params).await
    }
}

// ============================================================================
// 守卫测试（与 demo-services lib.rs 同构）
// ============================================================================

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    // ---- 辅助 ----

    struct ErrHandler;
    #[async_trait]
    impl IoHandler for ErrHandler {
        async fn execute(&self, _params: &JsonValue) -> IoResult {
            Err("fallback-called".to_string())
        }
    }

    fn svc_params(service_name: &str, args: JsonValue) -> JsonValue {
        JsonValue::object_from_pairs(&[
            ("service_name", JsonValue::string(service_name)),
            ("args", args),
        ])
    }

    // ---- 路由守卫 ----

    #[tokio::test]
    async fn test_router_fallback_on_unknown_service() {
        let router = FinanceConfigRouter::new(Arc::new(ErrHandler));
        let p = svc_params(
            "no_such_svc",
            JsonValue::object_from_pairs(&[("key", JsonValue::string("x"))]),
        );
        let err = router.execute(&p).await.unwrap_err();
        assert_eq!(err, "fallback-called");
    }

    #[tokio::test]
    async fn test_router_routes_finance_config_get_natively() {
        let router = FinanceConfigRouter::new(Arc::new(ErrHandler));
        let p = svc_params(
            "finance_config_get",
            JsonValue::object_from_pairs(&[("key", JsonValue::string("config:limits.travel.max_amount"))]),
        );
        let r = router.execute(&p).await.unwrap();
        assert_eq!(r.get("exists").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(
            r.get("error").and_then(|v| v.as_str()),
            Some("CONFIG_KEY_NOT_FOUND")
        );
    }

    #[tokio::test]
    async fn test_router_routes_finance_config_set_natively() {
        let router = FinanceConfigRouter::new(Arc::new(ErrHandler));
        let p = svc_params(
            "finance_config_set",
            JsonValue::object_from_pairs(&[
                ("key", JsonValue::string("config:limits.travel.max_amount")),
                ("new_value", JsonValue::Integer(5000)),
                ("reason", JsonValue::string("测试")),
                ("proposed_by", JsonValue::string("test")),
            ]),
        );
        let r = router.execute(&p).await.unwrap();
        assert_eq!(r.get("success").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            r.get("awaiting_approval").and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    // ---- 声明表守卫 ----

    #[test]
    fn test_native_service_table_invariants() {
        let mut names: Vec<&str> = NATIVE_SERVICES.iter().map(|d| d.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "声明表服务名必须唯一");
        for d in NATIVE_SERVICES {
            let _ = (d.make)();
        }
    }

    #[test]
    fn test_native_service_table_matches_declaration_file() {
        let raw = include_str!("../official_native_services.json");
        let file: serde_json::Value =
            serde_json::from_str(raw).expect("声明文件非法 JSON");
        let services = file
            .get("services")
            .and_then(|v| v.as_array())
            .expect("声明文件缺 services 数组");
        assert_eq!(
            services.len(),
            NATIVE_SERVICES.len(),
            "声明文件服务数({})与 NATIVE_SERVICES({})不一致",
            services.len(),
            NATIVE_SERVICES.len()
        );
        for (i, def) in NATIVE_SERVICES.iter().enumerate() {
            let s = &services[i];
            let name = s.get("name").and_then(|v| v.as_str()).unwrap_or_else(|| {
                panic!("services[{i}] 缺 name")
            });
            let sensitive = s
                .get("sensitive")
                .and_then(|v| v.as_bool())
                .unwrap_or_else(|| panic!("services[{i}]({name}) 缺 sensitive"));
            let description = s
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!("services[{i}]({name}) 缺 description"));
            assert_eq!(name, def.name, "声明文件与 NATIVE_SERVICES 在第 {i} 项漂移 (name)");
            assert_eq!(sensitive, def.sensitive, "声明文件与 NATIVE_SERVICES 在第 {i} 项漂移 (sensitive)");
            assert_eq!(description, def.description, "声明文件与 NATIVE_SERVICES 在第 {i} 项漂移 (description)");
        }
    }

    // ---- 子集启用守卫 ----

    #[test]
    fn test_with_enabled_subset_filter_deterministic() {
        let router =
            FinanceConfigRouter::with_enabled(Arc::new(ErrHandler), &["finance_config_get"])
                .unwrap();
        assert_eq!(
            router.enabled_service_names(),
            vec!["finance_config_get"]
        );

        let p = svc_params(
            "finance_config_get",
            JsonValue::object_from_pairs(&[("key", JsonValue::string("x"))]),
        );
        let rt = tokio::runtime::Runtime::new().unwrap();
        let r = rt.block_on(router.execute(&p)).unwrap();
        assert!(r.get("exists").is_some());

        let p2 = svc_params(
            "finance_config_set",
            JsonValue::object_from_pairs(&[
                ("key", JsonValue::string("x")),
                ("new_value", JsonValue::Integer(1)),
            ]),
        );
        assert_eq!(
            rt.block_on(router.execute(&p2)).unwrap_err(),
            "fallback-called"
        );
    }

    #[test]
    fn test_with_enabled_rejects_unknown_duplicate_and_empty() {
        fn expect_err(r: Result<FinanceConfigRouter, String>) -> String {
            match r {
                Ok(_) => panic!("应当构造失败"),
                Err(e) => e,
            }
        }

        let err = expect_err(FinanceConfigRouter::with_enabled(
            Arc::new(ErrHandler),
            &["finance_config_get", "no_such_svc"],
        ));
        assert!(err.contains("no_such_svc"), "{err}");
        assert!(err.contains("finance_config_get"));

        let err = expect_err(FinanceConfigRouter::with_enabled(
            Arc::new(ErrHandler),
            &["finance_config_get", "finance_config_get"],
        ));
        assert!(err.contains("重复"), "{err}");

        let err = expect_err(FinanceConfigRouter::with_enabled(
            Arc::new(ErrHandler),
            &[],
        ));
        assert!(err.contains("enabled=false"), "{err}");
    }

    // ---- 端到端: set → approve → get（用独立 store，避免全局 OnceLock 污染）----

    #[test]
    fn test_full_set_approve_get_flow() {
        let dir = std::env::temp_dir().join(format!(
            "evorule-finance-config-e2e-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("finance-config-test.json");

        let store = Arc::new(ConfigStore::open(&path).unwrap());

        use crate::config_service::{ConfigGetService, ConfigSetService};

        let set_svc = ConfigSetService { store: store.clone() };
        let get_svc = ConfigGetService { store: store.clone() };

        let set_args = JsonValue::object_from_pairs(&[
            ("key", JsonValue::string("config:limits.travel.max_amount")),
            ("new_value", JsonValue::Integer(2000)),
            ("reason", JsonValue::string("测试提案")),
            ("proposed_by", JsonValue::string("tester")),
        ]);
        let r1 = set_svc.execute(&set_args).unwrap();
        assert_eq!(r1.get("success").and_then(|v| v.as_bool()), Some(true));
        let proposal_id = r1
            .get("proposal_id")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_string();

        let get_args = JsonValue::object_from_pairs(&[(
            "key",
            JsonValue::string("config:limits.travel.max_amount"),
        )]);
        let r2 = get_svc.execute(&get_args).unwrap();
        assert_eq!(r2.get("exists").and_then(|v| v.as_bool()), Some(false));

        store.approve_proposal(&proposal_id, "finance_dir").unwrap();

        let r3 = get_svc.execute(&get_args).unwrap();
        assert_eq!(r3.get("exists").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(r3.get("value").and_then(|v| v.as_i64()), Some(2000));

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: crate::store::StoreFile = serde_json::from_str(&content).unwrap();
        assert!(!parsed.audit.is_empty());
    }
}
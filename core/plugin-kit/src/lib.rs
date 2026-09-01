// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 进程内原生插件机制公共件(插件 NativeService 抽象上提,UV-037 闭环后兑现
//! `plugins/indicator-services/src/lib.rs` 的跨插件抽象评估注记)。
//!
//! # 来源与等价性承诺(诚实声明)
//! - 本 crate 机制代码自 `plugins/demo-services` / `plugins/physics-services` /
//!   `plugins/indicator-services` 三插件 lib.rs 中**逐行同构**的复合路由段归一而来
//!   (trait / 声明项 / 过滤路由器:构造、启用子集三拒绝、声明序查找、HTTP 回落);
//! - 归一为纯等价重构:三拒绝校验语义与错误文案**逐字节不变**,
//!   回落/启用序/健康可见性行为不变(既有测试零改动语义通过为验收门禁);
//! - 差异仅在:声明表由各插件自持(本 crate 只定义项结构与泛型路由器),
//!   `service_name` 解析、浮点字符串化等业务约定亦留各插件。
//!
//! # 使用方式(插件侧)
//! - 插件导出 `&'static [NativeServiceDef]` 声明表(SSOT 模式同 UV-025/029/030):
//!   新增原生能力 = 表追加一项 + 清单启用,宿主与插件机制零改动;
//! - 插件以薄壳具名结构体委托本 crate 路由器(保持既有具名 API 与测试不变),
//!   或直接使用 [`NativeServiceRouter`] / [`mount_router`]。

#![forbid(unsafe_code)]

use std::sync::Arc;

use async_trait::async_trait;
use evorule_reactor::{IoHandler, IoResult};
use evorule_tcb::JsonValue;

/// 原生服务的统一入参入口：接收 `args`（已由引擎解析 __ 路径为实际值），返回业务结果。
pub trait NativeService: Send + Sync {
    fn execute(&self, args: &JsonValue) -> IoResult;
}

/// 原生服务声明项(UV-025 声明式注册:新增原生能力 = 向插件声明表追加一项,
/// 路由器/能力对账/绑定核对零改动)。
///
/// `name` 与治理侧服务目录种子对齐——共同事实源为各插件声明文件
/// `official_native_services.json`(UV-029,SSOT),双侧守卫锁定漂移。
pub struct NativeServiceDef {
    /// 全局唯一服务名(`io_request` 的 `service_name`;跨插件唯一,治理侧聚合时锁定)
    pub name: &'static str,
    /// 是否涉及凭据/敏感数据(与治理侧目录元数据对齐)
    pub sensitive: bool,
    /// 一句话描述(供能力对账/文档派生)
    pub description: &'static str,
    /// 实例构造子(带状态服务在此注入默认状态)
    pub make: fn() -> Arc<dyn NativeService>,
}

/// 复合 IoHandler：`service_name` 命中原生服务名 → 原生执行；否则回落 HTTP。
///
/// 挂载到 `IoType::call_service()` / `IoType::call_external()`。
/// 声明表由调用方传入(`&'static [NativeServiceDef]`,各插件自持),
/// 路由查找恒按声明序(与启用集传入顺序无关)。
pub struct NativeServiceRouter {
    /// 原生服务实例(UV-030:name+实例成对存放,支持部署期启用子集)
    instances: Vec<(&'static str, Arc<dyn NativeService>)>,
    /// 原生未命中时的 HTTP 回落（ServiceRegistryHandler，读 service_registry.json）
    fallback: Arc<dyn IoHandler>,
}

impl NativeServiceRouter {
    /// 构造复合路由(全量启用)。`fallback` 为未命中原生服务名时的 HTTP 处理器
    /// （通常传 `Arc<ServiceRegistryHandler>`，由调用方按 --allow-loopback 构造）。
    pub fn new(defs: &'static [NativeServiceDef], fallback: Arc<dyn IoHandler>) -> Self {
        Self {
            instances: defs.iter().map(|d| (d.name, (d.make)())).collect(),
            fallback,
        }
    }

    /// 部署期启用子集构造(UV-030 插件清单化)。
    ///
    /// - `enabled` 为启用服务名集合（顺序无关,路由查找仍按声明表声明序）;
    /// - 未知名 / 重复名 / 空启用集 → fail-fast Err(含指引,不静默忽略);
    /// - 宿主零具体名特判:新增原生能力 = 插件声明表追加一项 + 清单启用。
    pub fn with_enabled(
        defs: &'static [NativeServiceDef],
        fallback: Arc<dyn IoHandler>,
        enabled: &[&str],
    ) -> Result<Self, String> {
        if enabled.is_empty() {
            return Err(
                "插件启用集为空 — 若要停用全部原生服务请直接 enabled=false(不挂载本路由),\
                 若要启用请在 plugin_manifest.services 中至少列出一个服务"
                    .to_string(),
            );
        }
        let mut seen: Vec<&str> = Vec::new();
        for name in enabled {
            let known = defs.iter().any(|d| d.name == *name);
            if !known {
                return Err(format!(
                    "plugin_manifest 引用了未注册的原生服务 '{name}' — 合法服务名: [{}]。\
                     自诊断指引: ① 核对 service_name 拼写(以本清单为准,非治理侧目录); \
                     ② 新增原生服务请向 NATIVE_SERVICES 声明表追加一项后在清单中启用",
                    defs.iter().map(|d| d.name).collect::<Vec<_>>().join(", ")
                ));
            }
            if seen.contains(name) {
                return Err(format!(
                    "plugin_manifest 服务 '{name}' 重复声明 — 请去重后重试(不静默去重)"
                ));
            }
            seen.push(name);
        }
        Ok(Self {
            instances: defs
                .iter()
                .filter(|d| seen.contains(&d.name))
                .map(|d| (d.name, (d.make)()))
                .collect(),
            fallback,
        })
    }

    /// 当前实例已启用的原生服务名列表(健康可见性/能力对账用)
    pub fn enabled_service_names(&self) -> Vec<&'static str> {
        self.instances.iter().map(|(n, _)| *n).collect()
    }

    /// 原生服务名列表（server 侧服务绑定核对/能力对账用；自声明表派生）
    pub fn native_service_names(defs: &'static [NativeServiceDef]) -> Vec<&'static str> {
        defs.iter().map(|d| d.name).collect()
    }

    /// 解析 `service_name`（params 顶层），并取出 `args`（默认空对象）。
    fn split_params(params: &JsonValue) -> (Option<String>, JsonValue) {
        let service_name = params
            .get("service_name")
            .and_then(|v| v.as_str())
            .or_else(|| params.get("name").and_then(|v| v.as_str()))
            .map(|s| s.to_string());
        let args = params
            .get("args")
            .cloned()
            .unwrap_or_else(JsonValue::empty_object);
        (service_name, args)
    }
}

#[async_trait]
impl IoHandler for NativeServiceRouter {
    async fn execute(&self, params: &JsonValue) -> IoResult {
        let (service_name, args) = Self::split_params(params);
        let name = service_name.as_deref().unwrap_or("");
        // 声明表查找分发:新增原生服务 = 表加一项,此处零改动;
        // UV-030:仅在本路由实例已启用的子集内查找(未启用 → 回落/如实报错)
        match self.instances.iter().find(|(n, _)| *n == name) {
            Some((_, svc)) => svc.execute(&args),
            _ => self.fallback.execute(params).await,
        }
    }
}

/// 挂载辅助：按启用语义构造 `Arc<dyn IoHandler>`(server 侧 `PluginDef` 直引,
/// 免逐插件包装构造子)。
///
/// - `None` = 全量启用(缺省,存量零迁移);
/// - `Some(&[])` 之外的切片 = 子集启用(三拒绝校验同 [`NativeServiceRouter::with_enabled`])。
///
/// `Some(空集)` 在清单语义中对应 `enabled=false`(不挂载,见 UV-030 清单解析),
/// 不会走到本函数;此处仍以三拒绝口径如实报错,不静默转全启。
pub fn mount_router(
    defs: &'static [NativeServiceDef],
    fallback: Arc<dyn IoHandler>,
    enabled: Option<&[&str]>,
) -> Result<Arc<dyn IoHandler>, String> {
    match enabled {
        None => Ok(Arc::new(NativeServiceRouter::new(defs, fallback))),
        Some(names) => Ok(Arc::new(NativeServiceRouter::with_enabled(
            defs, fallback, names,
        )?)),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    // ===== 测试桩:两服务声明表(机制用例自三插件同构用例归一) =====

    struct Echo;
    impl NativeService for Echo {
        fn execute(&self, args: &JsonValue) -> IoResult {
            Ok(JsonValue::object_from_pairs(&[
                ("svc", JsonValue::string("echo")),
                ("got", args.get("k").cloned().unwrap_or(JsonValue::Null)),
            ]))
        }
    }

    struct Other;
    impl NativeService for Other {
        fn execute(&self, _args: &JsonValue) -> IoResult {
            Ok(JsonValue::object_from_pairs(&[(
                "svc",
                JsonValue::string("other"),
            )]))
        }
    }

    static DEFS: &[NativeServiceDef] = &[
        NativeServiceDef {
            name: "kit_echo",
            sensitive: false,
            description: "echo 桩",
            make: || Arc::new(Echo),
        },
        NativeServiceDef {
            name: "kit_other",
            sensitive: false,
            description: "other 桩",
            make: || Arc::new(Other),
        },
    ];

    struct ErrHandler;
    #[async_trait]
    impl IoHandler for ErrHandler {
        async fn execute(&self, _params: &JsonValue) -> IoResult {
            Err("fallback-called".to_string())
        }
    }

    fn params_of(service_name: &str, args: JsonValue) -> JsonValue {
        JsonValue::object_from_pairs(&[
            ("service_name", JsonValue::string(service_name)),
            ("args", args),
        ])
    }

    #[tokio::test]
    async fn test_router_fallback_on_unknown_service() {
        // 未知 service_name → 回落 fallback（哨兵报错验证分发）
        let router = NativeServiceRouter::new(DEFS, Arc::new(ErrHandler));
        let p = params_of("unknown_svc", JsonValue::object_from_pairs(&[]));
        assert_eq!(router.execute(&p).await.unwrap_err(), "fallback-called");
    }

    #[tokio::test]
    async fn test_args_passed_through_and_name_alias() {
        // args 原样到达 + `name` 别名兼容解析(与三插件同语义)
        let router = NativeServiceRouter::new(DEFS, Arc::new(ErrHandler));
        let p = JsonValue::object_from_pairs(&[
            ("name", JsonValue::string("kit_echo")),
            (
                "args",
                JsonValue::object_from_pairs(&[("k", JsonValue::Integer(7))]),
            ),
        ]);
        let r = router.execute(&p).await.unwrap();
        assert_eq!(r.get("svc").and_then(|v| v.as_str()), Some("echo"));
        assert_eq!(r.get("got").and_then(|v| v.as_i64()), Some(7));
    }

    #[test]
    fn test_def_table_invariants() {
        // 声明表不变量:服务名唯一;每项构造子可用
        let mut names: Vec<&str> = DEFS.iter().map(|d| d.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "声明表服务名必须唯一");
        for d in DEFS {
            let _ = (d.make)();
        }
    }

    #[tokio::test]
    async fn test_all_defs_route_natively() {
        // 声明表每一项都必须命中原生执行(而非 HTTP 回落)——
        // fallback 用哨兵报错,任何一项落到 fallback 即失败
        let router = NativeServiceRouter::new(DEFS, Arc::new(ErrHandler));
        for d in DEFS {
            let p = params_of(d.name, JsonValue::object_from_pairs(&[]));
            // 空参数下业务结果可成功可失败,但绝不能是 fallback 哨兵
            if let Err(e) = router.execute(&p).await {
                assert_ne!(e, "fallback-called", "服务 {} 落到了 HTTP 回落", d.name);
            }
        }
    }

    #[tokio::test]
    async fn test_with_enabled_subset_filter_deterministic() {
        // 子集过滤:仅启用的服务命中原生执行,未启用的回落
        let router =
            NativeServiceRouter::with_enabled(DEFS, Arc::new(ErrHandler), &["kit_echo"]).unwrap();
        assert_eq!(router.enabled_service_names(), vec!["kit_echo"]);
        // 启用的服务 → 原生(不走 fallback 哨兵)
        let p = params_of("kit_echo", JsonValue::object_from_pairs(&[]));
        let r = router.execute(&p).await.unwrap();
        assert_eq!(r.get("svc").and_then(|v| v.as_str()), Some("echo"));
        // 未启用的服务 → 回落
        let p = params_of("kit_other", JsonValue::object_from_pairs(&[]));
        assert_eq!(router.execute(&p).await.unwrap_err(), "fallback-called");
    }

    #[test]
    fn test_with_enabled_rejects_unknown_duplicate_and_empty() {
        // router 未实现 Debug,以 match 显式取 Err(测试惯例与三插件一致)
        fn expect_err(r: Result<NativeServiceRouter, String>) -> String {
            match r {
                Ok(_) => panic!("应当构造失败"),
                Err(e) => e,
            }
        }
        // 未知名 → Err 含合法名清单与指引
        let err = expect_err(NativeServiceRouter::with_enabled(
            DEFS,
            Arc::new(ErrHandler),
            &["kit_echo", "no_such_svc"],
        ));
        assert!(err.contains("no_such_svc"), "{err}");
        assert!(
            err.contains("kit_echo") && err.contains("NATIVE_SERVICES"),
            "{err}"
        );
        // 重复名 → Err(不静默去重)
        let err = expect_err(NativeServiceRouter::with_enabled(
            DEFS,
            Arc::new(ErrHandler),
            &["kit_echo", "kit_echo"],
        ));
        assert!(err.contains("重复"), "{err}");
        // 空启用集 → Err(指引改用 enabled=false)
        let err = expect_err(NativeServiceRouter::with_enabled(
            DEFS,
            Arc::new(ErrHandler),
            &[],
        ));
        assert!(err.contains("enabled=false"), "{err}");
    }

    #[tokio::test]
    async fn test_mount_router_all_and_subset() {
        // mount_router:None 全启(原生命中);Some 子集(未启用回落,口径与 with_enabled 一致)
        let all = mount_router(DEFS, Arc::new(ErrHandler), None).unwrap();
        let p = params_of("kit_other", JsonValue::object_from_pairs(&[]));
        assert_eq!(
            all.execute(&p)
                .await
                .unwrap()
                .get("svc")
                .and_then(|v| v.as_str()),
            Some("other")
        );
        let subset = mount_router(DEFS, Arc::new(ErrHandler), Some(&["kit_echo"])).unwrap();
        assert_eq!(
            subset.execute(&p).await.unwrap_err(),
            "fallback-called",
            "Some 子集外服务应回落,不静默转全启"
        );
    }
}

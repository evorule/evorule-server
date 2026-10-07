// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 确定性模板渲染原生服务。
//!
//! # 定位与选型边界
//! - 面向「规则输出→文档/消息」的数据转换域：上下文+模板 → JSON/Markdown/纯文本；
//! - 语法 v1 与 pack 模板占位符家族对齐（`{{path}}` 变量替换延续 pack 惯例），
//!   并提供 `{% if %}/{% else %}/{% endif %}` 与 `{% for x in path %}/{% endfor %}`
//!   控制流最小集——语法规范（含字符串化规则/确定性边界/执行预算）见本插件 README
//!   与 [`render`] 模块文档；
//! - evo-agent 侧 task_template（`{node_id}` 单花括号，agent 编排域）不属本服务域：
//!   两套语法不同域，不强行统一；
//! - 渲染 = 确定性纯函数：无时钟、无随机、无 IO，同输入逐字节同输出；
//!   时间/随机等非确定量由调用方作为显式参数放入 context（禁内置时钟函数）。
//!
//! # 双入口（同一引擎 SSOT）
//! - 规则内：`io_request(call_service, service_name=template_render, args=…)`，
//!   经既有 IO 审计通路落账；
//! - REST：`POST /api/templates/render`（server 侧直接调用本 crate [`render`] 纯函数，
//!   纯函数无落库，与 pack 模板 generate 同口径）。
//!
//! # 路由设计（与 demo/physics/indicator 同构）
//! - `TemplateServiceRouter` 实现 [IoHandler]，按 `params.service_name` 分发：
//!   命中原生服务名 → 原生执行（进程内，确定性）；未命中 → 回落
//!   `ServiceRegistryHandler`（HTTP，兼容其他外部服务）；
//! - 原生实现接收的入参 = `params.args`。

#![forbid(unsafe_code)]

pub mod render;

use std::sync::Arc;

use async_trait::async_trait;
use evorule_reactor::{IoHandler, IoResult};
use evorule_tcb::JsonValue;

pub use evorule_plugin_kit::{NativeService, NativeServiceDef};

use crate::render::{render_checked, Format};

// ============================================================================
// 原生服务声明表（SSOT：official_native_services.json 为声明基准，
// 本表持有 make 构造子；同步守卫锁定三字段+顺序，漂移即测试红）
// ============================================================================

/// 原生服务声明表(本插件原生叶子能力全量清单,顺序即路由查找序)。
pub const NATIVE_SERVICES: &[NativeServiceDef] = &[NativeServiceDef {
    name: "template_render",
    sensitive: false,
    description:
        "模板渲染(上下文+模板→JSON/Markdown/纯文本,{{}} 家族语法,if/for 最小集,确定性纯函数)",
    make: mk_template_render,
}];

fn mk_template_render() -> Arc<dyn NativeService> {
    Arc::new(TemplateRender)
}

// ============================================================================
// template_render 服务实现：args = { template, context?, format? }
// ============================================================================

/// `template_render`：模板 + 上下文 → 渲染文本（确定性纯函数封装）。
///
/// 契约（fail-fast，不静默回退）：
/// - `template`：必填字符串，非空，长度 ≤ [`render::MAX_TEMPLATE_BYTES`]；
/// - `context`：可选对象（缺省空对象），模板变量的取值来源；
/// - `format`：可选字符串 `text | markdown | json`（缺省 `text`）；`json` 时
///   渲染产物额外做合法性校验（不是合法 JSON 即报错）；
/// - 返回：`{"status":"ok","format":<声明值>,"content":<渲染文本>}`。
struct TemplateRender;

impl NativeService for TemplateRender {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let template = args
            .get("template")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                "缺 template 参数 — 需为字符串。自诊断指引: args 形如 \
             { \"template\": \"...\", \"context\": {...}, \"format\": \"text|markdown|json\" }"
                    .to_string()
            })?;
        if template.is_empty() {
            return Err(
                "template 为空字符串 — 空模板无渲染语义，fail-fast（不返回空产物）".to_string(),
            );
        }
        if template.len() > render::MAX_TEMPLATE_BYTES {
            return Err(format!(
                "template {} 字节超出上限 {} — 执行预算保护，请拆分模板",
                template.len(),
                render::MAX_TEMPLATE_BYTES
            ));
        }
        let context = match args.get("context") {
            None | Some(JsonValue::Null) => JsonValue::empty_object(),
            Some(v) if matches!(v, JsonValue::Object(_)) => v.clone(),
            Some(other) => {
                return Err(format!(
                    "context 需为对象（缺省空对象），得到 {other} — 形如 {{\"k\": v}}"
                ));
            }
        };
        let format = match args.get("format") {
            None | Some(JsonValue::Null) => Format::Text,
            Some(JsonValue::String(s)) => Format::parse(s)?,
            Some(other) => {
                return Err(format!(
                    "format 需为字符串（text|markdown|json），得到 {other}"
                ));
            }
        };
        let rendered = render_checked(template, &context, format)?;
        Ok(JsonValue::object_from_pairs(&[
            ("status", JsonValue::string("ok")),
            ("format", JsonValue::string(rendered.format.as_str())),
            ("content", JsonValue::string(rendered.content)),
        ]))
    }
}

// ============================================================================
// 复合路由：原生优先，HTTP 回落（机制件上提 plugin-kit，本 crate 薄壳委托）
// ============================================================================

/// 复合 IoHandler：`service_name` 命中原生服务名 → 原生执行；否则回落 HTTP。
///
/// 薄壳具名路由器：机制（new / with_enabled 三拒绝 / 声明序查找 / 回落 / split_params）
/// 已上提 `evorule-plugin-kit`（行为与既有插件等价），本插件仅自持声明表并保持具名 API。
/// 挂载到 `IoType::call_service` / `IoType::call_external`。
pub struct TemplateServiceRouter(evorule_plugin_kit::NativeServiceRouter);

impl TemplateServiceRouter {
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

    /// 当前实例已启用的原生服务名列表（健康可见性/能力对账用）
    pub fn enabled_service_names(&self) -> Vec<&'static str> {
        self.0.enabled_service_names()
    }

    /// 原生服务名列表（server 侧服务绑定核对/能力对账用；自声明表派生）
    pub fn native_service_names() -> Vec<&'static str> {
        evorule_plugin_kit::NativeServiceRouter::native_service_names(NATIVE_SERVICES)
    }
}

#[async_trait]
impl IoHandler for TemplateServiceRouter {
    async fn execute(&self, params: &JsonValue) -> IoResult {
        self.0.execute(params).await
    }
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn args_of(template: &str, context: Option<JsonValue>, format: Option<&str>) -> JsonValue {
        let mut pairs: Vec<(&str, JsonValue)> = vec![("template", JsonValue::string(template))];
        if let Some(c) = context {
            pairs.push(("context", c));
        }
        if let Some(f) = format {
            pairs.push(("format", JsonValue::string(f)));
        }
        JsonValue::object_from_pairs(&pairs)
    }

    // ===== 声明表机制（与既有插件同构）=====

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
        let router = TemplateServiceRouter::new(Arc::new(SentinelHandler));
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

    #[tokio::test]
    async fn test_router_fallback_on_unknown_service() {
        // 未知 service_name → 回落 fallback（哨兵报错验证分发）
        struct ErrHandler;
        #[async_trait]
        impl IoHandler for ErrHandler {
            async fn execute(&self, _params: &JsonValue) -> IoResult {
                Err("fallback-called".to_string())
            }
        }
        let router = TemplateServiceRouter::new(Arc::new(ErrHandler));
        let params = JsonValue::object_from_pairs(&[
            ("io_type", JsonValue::string("call_service")),
            ("service_name", JsonValue::string("unknown_svc")),
        ]);
        let err = router.execute(&params).await.unwrap_err();
        assert_eq!(err, "fallback-called");
    }

    #[test]
    fn test_with_enabled_subset_filter_deterministic() {
        // 子集过滤:仅启用的服务命中原生执行,未启用的回落
        struct ErrHandler;
        #[async_trait]
        impl IoHandler for ErrHandler {
            async fn execute(&self, _params: &JsonValue) -> IoResult {
                Err("fallback-called".to_string())
            }
        }
        let router =
            TemplateServiceRouter::with_enabled(Arc::new(ErrHandler), &["template_render"])
                .unwrap();
        assert_eq!(router.enabled_service_names(), vec!["template_render"]);
        // 未启用列表中的其他名字 → 回落
        let rt = tokio::runtime::Runtime::new().unwrap();
        let params = JsonValue::object_from_pairs(&[
            ("service_name", JsonValue::string("other_svc")),
            ("args", JsonValue::empty_object()),
        ]);
        assert_eq!(
            rt.block_on(router.execute(&params)).unwrap_err(),
            "fallback-called"
        );
    }

    #[test]
    fn test_with_enabled_rejects_unknown_duplicate_and_empty() {
        struct ErrHandler;
        #[async_trait]
        impl IoHandler for ErrHandler {
            async fn execute(&self, _params: &JsonValue) -> IoResult {
                Err("fallback-called".to_string())
            }
        }
        fn expect_err(r: Result<TemplateServiceRouter, String>) -> String {
            match r {
                Ok(_) => panic!("应当构造失败"),
                Err(e) => e,
            }
        }
        // 未知名 → Err 含合法名清单与指引
        let err = expect_err(TemplateServiceRouter::with_enabled(
            Arc::new(ErrHandler),
            &["template_render", "no_such_svc"],
        ));
        assert!(err.contains("no_such_svc"), "{err}");
        assert!(
            err.contains("template_render") && err.contains("NATIVE_SERVICES"),
            "{err}"
        );
        // 重复名 → Err(不静默去重)
        let err = expect_err(TemplateServiceRouter::with_enabled(
            Arc::new(ErrHandler),
            &["template_render", "template_render"],
        ));
        assert!(err.contains("重复"), "{err}");
        // 空启用集 → Err(指引改用 enabled=false)
        let err = expect_err(TemplateServiceRouter::with_enabled(
            Arc::new(ErrHandler),
            &[],
        ));
        assert!(err.contains("enabled=false"), "{err}");
    }

    // ===== 同步守卫：声明文件 official_native_services.json 为 SSOT =====

    #[test]
    fn test_native_service_table_matches_declaration_file() -> Result<(), String> {
        // 同步守卫:本插件声明文件 official_native_services.json 为 SSOT。
        // 本表(name/sensitive/description)与文件三字段+顺序全量比对——
        // 变更服务 = 改文件 + 本表同步,漂移即失败(不静默)。
        let raw = include_str!("../official_native_services.json");
        let file: serde_json::Value =
            serde_json::from_str(raw).map_err(|e| format!("声明文件非法 JSON: {e}"))?;
        let services = file
            .get("services")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "声明文件缺 services 数组".to_string())?;
        if services.len() != NATIVE_SERVICES.len() {
            return Err(format!(
                "声明文件服务数({})与 NATIVE_SERVICES({})不一致 — 两处必须同步(文件为 SSOT,表持有 make 构造子)",
                services.len(),
                NATIVE_SERVICES.len()
            ));
        }
        for (i, def) in NATIVE_SERVICES.iter().enumerate() {
            let s = &services[i];
            let name = s
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("services[{i}] 缺 name"))?;
            let sensitive = s
                .get("sensitive")
                .and_then(|v| v.as_bool())
                .ok_or_else(|| format!("services[{i}]({name}) 缺 sensitive"))?;
            let description = s
                .get("description")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("services[{i}]({name}) 缺 description"))?;
            if name != def.name || sensitive != def.sensitive || description != def.description {
                return Err(format!(
                    "声明文件与 NATIVE_SERVICES 在第 {i} 项漂移:\n  文件: {name} sensitive={sensitive} {description}\n  表:   {} sensitive={} {}\n声明文件为 SSOT — 请以文件为准修正本表(或有意变更时先改文件)",
                    def.name, def.sensitive, def.description
                ));
            }
        }
        Ok(())
    }

    // ===== template_render 服务契约 =====

    #[tokio::test]
    async fn test_service_render_ok_text() {
        let router = TemplateServiceRouter::new(Arc::new(UnreachableHandler));
        let params = JsonValue::object_from_pairs(&[
            ("service_name", JsonValue::string("template_render")),
            (
                "args",
                args_of(
                    "你好, {{name}}!{% if vip %} VIP{% endif %}",
                    Some(ctx_user()),
                    None,
                ),
            ),
        ]);
        let r = router.execute(&params).await.unwrap();
        assert_eq!(r.get("status").and_then(|v| v.as_str()), Some("ok"));
        assert_eq!(r.get("format").and_then(|v| v.as_str()), Some("text"));
        assert_eq!(
            r.get("content").and_then(|v| v.as_str()),
            Some("你好, 张三! VIP")
        );
    }

    #[tokio::test]
    async fn test_service_render_ok_json_format() {
        let router = TemplateServiceRouter::new(Arc::new(UnreachableHandler));
        let params = JsonValue::object_from_pairs(&[
            ("service_name", JsonValue::string("template_render")),
            (
                "args",
                args_of(
                    "{\"n\": {{count}}}",
                    Some(ctx(&[("count", JsonValue::Integer(7))])),
                    Some("json"),
                ),
            ),
        ]);
        let r = router.execute(&params).await.unwrap();
        assert_eq!(r.get("format").and_then(|v| v.as_str()), Some("json"));
        assert_eq!(
            r.get("content").and_then(|v| v.as_str()),
            Some("{\"n\": 7}")
        );
    }

    #[tokio::test]
    async fn test_service_fail_fast_contracts() {
        let router = Arc::new(TemplateServiceRouter::new(Arc::new(UnreachableHandler)));
        let exec = move |args: JsonValue| {
            let params = JsonValue::object_from_pairs(&[
                ("service_name", JsonValue::string("template_render")),
                ("args", args),
            ]);
            let router = Arc::clone(&router);
            async move { router.execute(&params).await }
        };
        // 缺 template
        let err = exec(JsonValue::empty_object()).await.unwrap_err();
        assert!(err.contains("缺 template"), "{err}");
        // 空 template
        let err = exec(args_of("", None, None)).await.unwrap_err();
        assert!(err.contains("为空"), "{err}");
        // 非对象 context
        let err = exec(args_of("t", Some(JsonValue::Integer(1)), None))
            .await
            .unwrap_err();
        assert!(err.contains("context 需为对象"), "{err}");
        // 非法 format
        let err = exec(args_of("t", None, Some("yaml"))).await.unwrap_err();
        assert!(err.contains("format"), "{err}");
        // 渲染错误透传（变量缺失 fail-fast）
        let err = exec(args_of("{{nope}}", None, None)).await.unwrap_err();
        assert!(err.contains("变量未找到"), "{err}");
    }

    // ===== 测试桩 =====

    struct UnreachableHandler;
    #[async_trait]
    impl IoHandler for UnreachableHandler {
        async fn execute(&self, _params: &JsonValue) -> IoResult {
            Err("unreachable-fallback".to_string())
        }
    }

    fn ctx(pairs: &[(&str, JsonValue)]) -> JsonValue {
        JsonValue::object_from_pairs(pairs)
    }

    fn ctx_user() -> JsonValue {
        ctx(&[
            ("name", JsonValue::string("张三")),
            ("vip", JsonValue::Bool(true)),
        ])
    }
}

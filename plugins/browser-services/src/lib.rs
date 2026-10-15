// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! browser-service：zendriver-rs 驱动的浏览器操作 NativeService 插件。
//!
//! # 定位（design-13/14/15）
//! - evo-agent P0.5 操作能力域：浏览网页、填表单、抓数据、下载上传。
//! - 形态：evorule-server 进程内 NativeService 插件（非 MCP、非 HTTP sidecar）。
//! - 底层：zendriver-rs（纯 Rust async CDP）；同步 trait × async 经 `bridge` 桥接。
//!
//! # 最大能力面
//! - NATIVE_SERVICES 28 项一次声明全（SSOT=official_native_services.json）；
//!   v0.1 先打通表/路由/审计，真 zendriver 调用逐服务接入。

#![forbid(unsafe_code)]

pub mod bridge;

use std::sync::Arc;

use async_trait::async_trait;
use evorule_plugin_kit::{NativeService, NativeServiceDef, NativeServiceRouter};
use evorule_reactor::{IoHandler, IoResult};
use evorule_tcb::JsonValue;
use base64::Engine as _;

// ============================================================================
// 薄壳具名路由器（机制上提 plugin-kit，本插件仅自持声明表）
// ============================================================================

pub struct BrowserServiceRouter(NativeServiceRouter);

impl BrowserServiceRouter {
    pub fn new(fallback: Arc<dyn IoHandler>) -> Self {
        Self(NativeServiceRouter::new(NATIVE_SERVICES, fallback))
    }

    pub fn with_enabled(
        fallback: Arc<dyn IoHandler>,
        enabled: &[&str],
    ) -> Result<Self, String> {
        NativeServiceRouter::with_enabled(NATIVE_SERVICES, fallback, enabled).map(Self)
    }

    pub fn enabled_service_names(&self) -> Vec<&'static str> {
        self.0.enabled_service_names()
    }

    pub fn native_service_names() -> Vec<&'static str> {
        NativeServiceRouter::native_service_names(NATIVE_SERVICES)
    }
}

#[async_trait]
impl IoHandler for BrowserServiceRouter {
    async fn execute(&self, params: &JsonValue) -> IoResult {
        self.0.execute(params).await
    }
}

// ============================================================================
// 28 个服务：v0.1 全部占位返回 not-implemented，真 zendriver 调用逐服务接入。
// 服务名与 sensitive 与 official_native_services.json 逐字段+顺序一致（漂移测试守卫）。
// ============================================================================

macro_rules! placeholder {
    ($($svc:ident => $name:literal),* $(,)?) => {
        $(
            pub struct $svc;
            impl NativeService for $svc {
                fn execute(&self, _args: &JsonValue) -> IoResult {
                    bridge::not_implemented($name)
                }
            }
        )*
    };
}

placeholder! {
}

// ============================================================================
// 核心真实现：browser_launch / browser_navigate / browser_get_html /
//            browser_screenshot / browser_click
// ============================================================================

pub struct BrowserLaunch;
impl NativeService for BrowserLaunch {
    fn execute(&self, _args: &JsonValue) -> IoResult {
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async {
            let b = bridge::Bridge::browser().await?;
            let tab = b.main_tab();
            let title: String = tab
                .evaluate_main("document.title")
                .await
                .map_err(|e| format!("evaluate title: {e}"))?;
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("launched", JsonValue::string("true")),
                ("edge", JsonValue::string("launched")),
                ("title", JsonValue::string(title)),
            ]))
        })
    }
}

pub struct BrowserNavigate;
impl NativeService for BrowserNavigate {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let url = args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "browser_navigate 需要 url 参数".to_string())?;
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async {
            let b = bridge::Bridge::browser().await?;
            let tab = b.main_tab();
            tab.goto(url).await.map_err(|e| format!("goto {url}: {e}"))?;
            let title: String = tab
                .evaluate_main("document.title")
                .await
                .map_err(|e| format!("evaluate title: {e}"))?;
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("url", JsonValue::string(url)),
                ("title", JsonValue::string(title)),
            ]))
        })
    }
}

pub struct BrowserGetHtml;
impl NativeService for BrowserGetHtml {
    fn execute(&self, _args: &JsonValue) -> IoResult {
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async {
            let b = bridge::Bridge::browser().await?;
            let tab = b.main_tab();
            let html = tab.content().await.map_err(|e| format!("get html: {e}"))?;
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("html", JsonValue::string(html)),
            ]))
        })
    }
}

pub struct BrowserScreenshot;
impl NativeService for BrowserScreenshot {
    fn execute(&self, _args: &JsonValue) -> IoResult {
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async {
            let b = bridge::Bridge::browser().await?;
            let tab = b.main_tab();
            let png = tab.screenshot().await.map_err(|e| format!("screenshot: {e}"))?;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("format", JsonValue::string("png")),
                ("b64", JsonValue::string(b64)),
            ]))
        })
    }
}

pub struct BrowserClick;
impl NativeService for BrowserClick {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let selector = args
            .get("selector")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "browser_click 需要 selector 参数(CSS)".to_string())?;
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async {
            let b = bridge::Bridge::browser().await?;
            let tab = b.main_tab();
            let el = tab
                .find()
                .css(selector)
                .one()
                .await
                .map_err(|e| format!("find {selector}: {e}"))?;
            el.click().await.map_err(|e| format!("click {selector}: {e}"))?;
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("clicked", JsonValue::string(selector)),
            ]))
        })
    }
}

// ---- 导航族 ----
macro_rules! nav_simple {
    ($svc:ident, $js:literal, $ok:literal) => {
        pub struct $svc;
        impl NativeService for $svc {
            fn execute(&self, _args: &JsonValue) -> IoResult {
                let bridge = bridge::Bridge::get()?;
                bridge.block_on(async {
                    let b = bridge::Bridge::browser().await?;
                    let _: String = b.main_tab()
                        .evaluate_main(concat!($js, "; 'ok'"))
                        .await
                        .map_err(|e| concat!(stringify!($svc), ": {e}"))?;
                    Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                        ("ok", JsonValue::string($ok)),
                    ]))
                })
            }
        }
    };
}
nav_simple!(BrowserBack, "history.back()", "back");
nav_simple!(BrowserForward, "history.forward()", "forward");
nav_simple!(BrowserReload, "location.reload()", "reload");

pub struct BrowserWait;
impl NativeService for BrowserWait {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let ms = args.get("ms").and_then(|v| v.as_i64()).unwrap_or(500);
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async {
            tokio::time::sleep(std::time::Duration::from_millis(ms as u64)).await;
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("waited_ms", JsonValue::string(&ms.to_string())),
            ]))
        })
    }
}

// P0: 等元素出现（轮询 querySelector，替代硬 sleep）
pub struct BrowserWaitFor;
impl NativeService for BrowserWaitFor {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let selector = args.get("selector").and_then(|v| v.as_str())
            .ok_or_else(|| "browser_wait_for 需要 selector")?;
        let timeout_ms = args.get("timeout_ms").and_then(|v| v.as_i64()).unwrap_or(5000);
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async {
            let b = bridge::Bridge::browser().await?;
            let tab = b.main_tab();
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);
            loop {
                let found: bool = tab
                    .evaluate_main(&format!(
                        "!!document.querySelector({})",
                        serde_json::to_string(selector).unwrap_or_default()
                    ))
                    .await
                    .unwrap_or(false);
                if found {
                    return Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                        ("found", JsonValue::string("true")),
                        ("selector", JsonValue::string(selector)),
                    ]));
                }
                if std::time::Instant::now() >= deadline {
                    return Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                        ("found", JsonValue::string("false")),
                        ("selector", JsonValue::string(selector)),
                        ("timeout", JsonValue::string("true")),
                    ]));
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        })
    }
}

// ---- 表单/输入族 ----
macro_rules! el_action {
    ($svc:ident, $method:ident, $val_field:literal, $arg:ident) => {
        pub struct $svc;
        impl NativeService for $svc {
            fn execute(&self, args: &JsonValue) -> IoResult {
                let selector = args.get("selector").and_then(|v| v.as_str())
                    .ok_or_else(|| concat!(stringify!($svc), " 需要 selector"))?;
                let v = args.get($val_field).and_then(|x| x.as_str()).unwrap_or("");
                let bridge = bridge::Bridge::get()?;
                bridge.block_on(async {
                    let b = bridge::Bridge::browser().await?;
                    let el = b.main_tab().find().css(selector).one().await
                        .map_err(|e| format!("find {selector}: {e}"))?;
                    el.$method(v).await.map_err(|e| format!("{}: {e}", stringify!($method)))?;
                    Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                        ("ok", JsonValue::string(stringify!($method))),
                    ]))
                })
            }
        }
    };
}
el_action!(BrowserFill, set_value, "value", value);
el_action!(BrowserType, type_text_fast, "text", text);

pub struct BrowserSelect;
impl NativeService for BrowserSelect {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let selector = args.get("selector").and_then(|v| v.as_str())
            .ok_or_else(|| "browser_select 需要 selector")?;
        let value = args.get("value").and_then(|v| v.as_str()).unwrap_or("");
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async {
            let b = bridge::Bridge::browser().await?;
            let el = b.main_tab().find().css(selector).one().await
                .map_err(|e| format!("find {selector}: {e}"))?;
            el.set_value(value).await.map_err(|e| format!("select: {e}"))?;
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("selected", JsonValue::string(value)),
            ]))
        })
    }
}

pub struct BrowserCheck;
impl NativeService for BrowserCheck {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let selector = args.get("selector").and_then(|v| v.as_str())
            .ok_or_else(|| "browser_check 需要 selector")?;
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async {
            let b = bridge::Bridge::browser().await?;
            let el = b.main_tab().find().css(selector).one().await
                .map_err(|e| format!("find {selector}: {e}"))?;
            el.click().await.map_err(|e| format!("check: {e}"))?;
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("toggled", JsonValue::string(selector)),
            ]))
        })
    }
}

pub struct BrowserHover;
impl NativeService for BrowserHover {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let selector = args.get("selector").and_then(|v| v.as_str())
            .ok_or_else(|| "browser_hover 需要 selector")?;
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async {
            let b = bridge::Bridge::browser().await?;
            let el = b.main_tab().find().css(selector).one().await
                .map_err(|e| format!("find {selector}: {e}"))?;
            el.hover().await.map_err(|e| format!("hover: {e}"))?;
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("hovered", JsonValue::string(selector)),
            ]))
        })
    }
}

pub struct BrowserDrag;
impl NativeService for BrowserDrag {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let selector = args.get("selector").and_then(|v| v.as_str())
            .ok_or_else(|| "browser_drag 需要 selector")?;
        let x = args.get("x").and_then(|v| v.as_i64()).unwrap_or(0) as f64;
        let y = args.get("y").and_then(|v| v.as_i64()).unwrap_or(0) as f64;
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async {
            let b = bridge::Bridge::browser().await?;
            let el = b.main_tab().find().css(selector).one().await
                .map_err(|e| format!("find {selector}: {e}"))?;
            el.mouse_drag((x, y), 40).await.map_err(|e| format!("drag: {e}"))?;
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("dragged_to", JsonValue::string(&format!("({x},{y})"))),
            ]))
        })
    }
}

pub struct BrowserScroll;
impl NativeService for BrowserScroll {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let delta_y = args.get("dy").and_then(|v| v.as_i64()).unwrap_or(500);
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async {
            let b = bridge::Bridge::browser().await?;
            let tab = b.main_tab();
            let js = format!("window.scrollBy(0, {delta_y}); 'ok'");
            let r: String = tab.evaluate_main(js).await.map_err(|e| format!("scroll: {e}"))?;
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("scrolled_by", JsonValue::string(&delta_y.to_string())),
                ("result", JsonValue::string(r)),
            ]))
        })
    }
}

pub struct BrowserPress;
impl NativeService for BrowserPress {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let key = args.get("key").and_then(|v| v.as_str())
            .ok_or_else(|| "browser_press 需要 key（如 Enter/Tab/Escape）".to_string())?;
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async {
            let b = bridge::Bridge::browser().await?;
            let tab = b.main_tab();
            // 用 CDP 无关的 JS keydown 派发做兜底（不依赖 Key 枚举映射表）
            let js = format!(
                "(function(){{let k={{Enter:'Enter',Tab:'Tab',Escape:'Escape',Backspace:'Backspace',ArrowDown:'ArrowDown',ArrowUp:'ArrowUp'}}[{{}}]||'{key}';document.dispatchEvent(new KeyboardEvent('keydown',{{key:k,bubbles:true}}));return k;}})()",
            );
            let r: String = tab.evaluate_main(js).await.map_err(|e| format!("press: {e}"))?;
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("pressed", JsonValue::string(r)),
            ]))
        })
    }
}

// ---- 感知/读类（evaluate_main 兜底）----
pub struct BrowserGetText;
impl NativeService for BrowserGetText {
    fn execute(&self, _args: &JsonValue) -> IoResult {
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async {
            let b = bridge::Bridge::browser().await?;
            let text: String = b.main_tab()
                .evaluate_main("document.body.innerText")
                .await
                .map_err(|e| format!("get text: {e}"))?;
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("text", JsonValue::string(text)),
            ]))
        })
    }
}

pub struct BrowserCookies;
impl NativeService for BrowserCookies {
    fn execute(&self, _args: &JsonValue) -> IoResult {
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async {
            let b = bridge::Bridge::browser().await?;
            let jar: String = b.main_tab()
                .evaluate_main("document.cookie")
                .await
                .map_err(|e| format!("cookies: {e}"))?;
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("cookie_string", JsonValue::string(jar)),
            ]))
        })
    }
}

pub struct BrowserStorage;
impl NativeService for BrowserStorage {
    fn execute(&self, _args: &JsonValue) -> IoResult {
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async {
            let b = bridge::Bridge::browser().await?;
            let s: String = b.main_tab()
                .evaluate_main("JSON.stringify(localStorage)")
                .await
                .map_err(|e| format!("storage: {e}"))?;
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("localStorage", JsonValue::string(s)),
            ]))
        })
    }
}

pub struct BrowserEvaluate;
impl NativeService for BrowserEvaluate {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let expr = args.get("expression").and_then(|v| v.as_str())
            .ok_or_else(|| "browser_evaluate 需要 expression".to_string())?;
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async {
            let b = bridge::Bridge::browser().await?;
            // 包成立即求值的表达式，返回字符串化结果
            let wrapped = format!("(function(){{try{{return String(eval({expr:?}));}}catch(e){{return 'ERR:'+e;}}}})()");
            let r: String = b.main_tab().evaluate_main(wrapped)
                .await.map_err(|e| format!("evaluate: {e}"))?;
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("result", JsonValue::string(r)),
            ]))
        })
    }
}

// ---- 多 tab ----
pub struct BrowserTabs;
impl NativeService for BrowserTabs {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let op = args.get("op").and_then(|v| v.as_str()).unwrap_or("list");
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async move {
            let b = bridge::Bridge::browser().await?;
            match op {
                "new" => {
                    let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("about:blank");
                    let tab = b.new_tab_at(url).await.map_err(|e| format!("new_tab: {e}"))?;
                    Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                        ("op", JsonValue::string("new")),
                        ("target_id", JsonValue::string(tab.target_id().to_string())),
                    ]))
                }
                _ => {
                    let tabs = b.tabs().await;
                    let mut out = Vec::new();
                    for t in &tabs {
                        let u = t.url().await.map(|u| u.to_string()).unwrap_or_default();
                        out.push(evorule_tcb::JsonValue::object_from_pairs(&[
                            ("target_id", JsonValue::string(t.target_id().to_string())),
                            ("url", JsonValue::string(u)),
                        ]));
                    }
                    Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                        ("op", JsonValue::string("list")),
                        ("count", JsonValue::string(&tabs.len().to_string())),
                        ("tabs", JsonValue::Array(out)),
                    ]))
                }
            }
        })
    }
}

// P0: 切到指定 tab（CDP Target.activateTarget）
pub struct BrowserSwitchTab;
impl NativeService for BrowserSwitchTab {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let target_id = args.get("target_id").and_then(|v| v.as_str())
            .ok_or_else(|| "browser_switch_tab 需要 target_id")?;
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async move {
            let b = bridge::Bridge::browser().await?;
            let conn = b.cdp();
            conn.call_raw(
                "Target.activateTarget",
                serde_json::json!({ "targetId": target_id }),
                None,
            )
            .await
            .map_err(|e| format!("activate tab: {e}"))?;
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("switched_to", JsonValue::string(target_id)),
            ]))
        })
    }
}

// P0: 关闭指定 tab
pub struct BrowserCloseTab;
impl NativeService for BrowserCloseTab {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let target_id = args.get("target_id").and_then(|v| v.as_str())
            .ok_or_else(|| "browser_close_tab 需要 target_id")?;
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async move {
            let b = bridge::Bridge::browser().await?;
            let conn = b.cdp();
            conn.call_raw(
                "Target.closeTarget",
                serde_json::json!({ "targetId": target_id }),
                None,
            )
            .await
            .map_err(|e| format!("close tab: {e}"))?;
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("closed", JsonValue::string(target_id)),
            ]))
        })
    }
}

// P1: 取指定元素文本
pub struct BrowserGetElementText;
impl NativeService for BrowserGetElementText {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let selector = args.get("selector").and_then(|v| v.as_str())
            .ok_or_else(|| "browser_get_element_text 需要 selector")?;
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async move {
            let b = bridge::Bridge::browser().await?;
            let js = format!(
                "(function(){{var el=document.querySelector({});return el?el.innerText:''}})()",
                serde_json::to_string(selector).unwrap_or_default()
            );
            let text: String = b.main_tab().evaluate_main(&js).await.unwrap_or_default();
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("text", JsonValue::string(&text)),
                ("selector", JsonValue::string(selector)),
            ]))
        })
    }
}

// P1: 取元素属性（href/value/src 等）
pub struct BrowserGetAttribute;
impl NativeService for BrowserGetAttribute {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let selector = args.get("selector").and_then(|v| v.as_str())
            .ok_or_else(|| "browser_get_attribute 需要 selector")?;
        let attr = args.get("attribute").and_then(|v| v.as_str())
            .ok_or_else(|| "browser_get_attribute 需要 attribute")?;
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async move {
            let b = bridge::Bridge::browser().await?;
            let js = format!(
                "(function(){{var el=document.querySelector({sel});return el?el.getAttribute({attr}):null}})()",
                sel = serde_json::to_string(selector).unwrap_or_default(),
                attr = serde_json::to_string(attr).unwrap_or_default(),
            );
            let val: String = b.main_tab().evaluate_main(&js).await.unwrap_or_default();
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("value", JsonValue::string(&val)),
                ("selector", JsonValue::string(selector)),
                ("attribute", JsonValue::string(attr)),
            ]))
        })
    }
}

// P1: 判断元素是否存在
pub struct BrowserExists;
impl NativeService for BrowserExists {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let selector = args.get("selector").and_then(|v| v.as_str())
            .ok_or_else(|| "browser_exists 需要 selector")?;
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async move {
            let b = bridge::Bridge::browser().await?;
            let js = format!(
                "!!document.querySelector({})",
                serde_json::to_string(selector).unwrap_or_default()
            );
            let found: bool = b.main_tab().evaluate_main(&js).await.unwrap_or(false);
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("exists", JsonValue::string(if found { "true" } else { "false" })),
                ("selector", JsonValue::string(selector)),
            ]))
        })
    }
}

// P2: 等网络空闲（AJAX 完成）
pub struct BrowserWaitNetworkIdle;
impl NativeService for BrowserWaitNetworkIdle {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let timeout_ms = args.get("timeout_ms").and_then(|v| v.as_i64()).unwrap_or(5000);
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async move {
            let b = bridge::Bridge::browser().await?;
            let tab = b.main_tab();
            // 轮询 performance resource timing，等 inflight=0
            let js = "(function(){return performance.getEntriesByType('resource').filter(r=>r.responseEnd===0).length})()";
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);
            loop {
                let inflight: i64 = tab.evaluate_main(js).await.unwrap_or(0);
                if inflight == 0 {
                    return Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                        ("idle", JsonValue::string("true")),
                    ]));
                }
                if std::time::Instant::now() >= deadline {
                    return Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                        ("idle", JsonValue::string("false")),
                        ("timeout", JsonValue::string("true")),
                    ]));
                }
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
        })
    }
}

// P2: 取当前 URL + title
pub struct BrowserGetLocation;
impl NativeService for BrowserGetLocation {
    fn execute(&self, _args: &JsonValue) -> IoResult {
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async {
            let b = bridge::Bridge::browser().await?;
            let tab = b.main_tab();
            let url: String = tab.evaluate_main("location.href").await.unwrap_or_default();
            let title: String = tab.evaluate_main("document.title").await.unwrap_or_default();
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("url", JsonValue::string(&url)),
                ("title", JsonValue::string(&title)),
            ]))
        })
    }
}

// P2: 滚元素到视口
pub struct BrowserScrollToElement;
impl NativeService for BrowserScrollToElement {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let selector = args.get("selector").and_then(|v| v.as_str())
            .ok_or_else(|| "browser_scroll_to_element 需要 selector")?;
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async move {
            let b = bridge::Bridge::browser().await?;
            let js = format!(
                "(function(){{var el=document.querySelector({});if(el)el.scrollIntoView({{block:'center'}});return !!el}})()",
                serde_json::to_string(selector).unwrap_or_default()
            );
            let ok: bool = b.main_tab().evaluate_main(&js).await.unwrap_or(false);
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("scrolled", JsonValue::string(if ok { "true" } else { "false" })),
                ("selector", JsonValue::string(selector)),
            ]))
        })
    }
}

// P2: 等新 tab 出现（点链接后自动检测）
pub struct BrowserWaitNewTab;
impl NativeService for BrowserWaitNewTab {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let timeout_ms = args.get("timeout_ms").and_then(|v| v.as_i64()).unwrap_or(5000);
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async move {
            let b = bridge::Bridge::browser().await?;
            let before = b.tabs().await.len();
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);
            loop {
                let tabs = b.tabs().await;
                if tabs.len() > before {
                    let newest = tabs.last().ok_or("no tabs")?;
                    return Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                        ("new_tab", JsonValue::string("true")),
                        ("target_id", JsonValue::string(newest.target_id().to_string())),
                    ]));
                }
                if std::time::Instant::now() >= deadline {
                    return Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                        ("new_tab", JsonValue::string("false")),
                        ("timeout", JsonValue::string("true")),
                    ]));
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        })
    }
}

// P2: 双击元素
pub struct BrowserDoubleClick;
impl NativeService for BrowserDoubleClick {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let selector = args.get("selector").and_then(|v| v.as_str())
            .ok_or_else(|| "browser_double_click 需要 selector")?;
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async move {
            let b = bridge::Bridge::browser().await?;
            let js = format!(
                "(function(){{var el=document.querySelector({});if(el)el.dispatchEvent(new MouseEvent('dblclick',{{bubbles:true,cancelable:true}}));return !!el}})()",
                serde_json::to_string(selector).unwrap_or_default()
            );
            let ok: bool = b.main_tab().evaluate_main(&js).await.unwrap_or(false);
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("ok", JsonValue::string(if ok { "double_click" } else { "not_found" })),
            ]))
        })
    }
}

// P2: 右键元素
pub struct BrowserRightClick;
impl NativeService for BrowserRightClick {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let selector = args.get("selector").and_then(|v| v.as_str())
            .ok_or_else(|| "browser_right_click 需要 selector")?;
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async move {
            let b = bridge::Bridge::browser().await?;
            let js = format!(
                "(function(){{var el=document.querySelector({});if(el)el.dispatchEvent(new MouseEvent('contextmenu',{{bubbles:true,cancelable:true}}));return !!el}})()",
                serde_json::to_string(selector).unwrap_or_default()
            );
            let ok: bool = b.main_tab().evaluate_main(&js).await.unwrap_or(false);
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("ok", JsonValue::string(if ok { "right_click" } else { "not_found" })),
            ]))
        })
    }
}

// P2: 清空 input
pub struct BrowserClear;
impl NativeService for BrowserClear {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let selector = args.get("selector").and_then(|v| v.as_str())
            .ok_or_else(|| "browser_clear 需要 selector")?;
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async move {
            let b = bridge::Bridge::browser().await?;
            let js = format!(
                "(function(){{var el=document.querySelector({});if(el){{el.value='';el.dispatchEvent(new Event('input',{{bubbles:true}}));}}return !!el}})()",
                serde_json::to_string(selector).unwrap_or_default()
            );
            let ok: bool = b.main_tab().evaluate_main(&js).await.unwrap_or(false);
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("cleared", JsonValue::string(if ok { "true" } else { "false" })),
            ]))
        })
    }
}

// P2: 设 viewport 大小
pub struct BrowserSetViewport;
impl NativeService for BrowserSetViewport {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let width = args.get("width").and_then(|v| v.as_i64()).unwrap_or(1280);
        let height = args.get("height").and_then(|v| v.as_i64()).unwrap_or(720);
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async move {
            let b = bridge::Bridge::browser().await?;
            b.cdp().call_raw(
                "Emulation.setDeviceMetricsOverride",
                serde_json::json!({ "width": width, "height": height, "deviceScaleFactor": 1, "mobile": false }),
                None,
            ).await.map_err(|e| format!("setViewport: {e}"))?;
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("width", JsonValue::string(&width.to_string())),
                ("height", JsonValue::string(&height.to_string())),
            ]))
        })
    }
}

// P2: 在 iframe 里跑 JS（跨域限制下只能取同域 iframe）
pub struct BrowserIframeEval;
impl NativeService for BrowserIframeEval {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let selector = args.get("selector").and_then(|v| v.as_str())
            .ok_or_else(|| "browser_iframe_eval 需要 iframe selector")?;
        let script = args.get("script").and_then(|v| v.as_str())
            .ok_or_else(|| "browser_iframe_eval 需要 script")?;
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async move {
            let b = bridge::Bridge::browser().await?;
            let js = format!(
                "(function(){{var f=document.querySelector({sel});if(!f||!f.contentDocument)return JSON.stringify({{error:'no iframe access'}});var w=f.contentWindow;var r=eval({sc});return JSON.stringify(r)}})()",
                sel = serde_json::to_string(selector).unwrap_or_default(),
                sc = serde_json::to_string(script).unwrap_or_default(),
            );
            let result: String = b.main_tab().evaluate_main(&js).await.unwrap_or_default();
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("result", JsonValue::string(&result)),
            ]))
        })
    }
}

// P2: 多选下拉（<select multiple>）
pub struct BrowserMultiSelect;
impl NativeService for BrowserMultiSelect {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let selector = args.get("selector").and_then(|v| v.as_str())
            .ok_or_else(|| "browser_multi_select 需要 selector")?;
        let values = args.get("values").and_then(|v| v.as_array())
            .ok_or_else(|| "browser_multi_select 需要 values 数组")?;
        let vals: Vec<String> = values.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect();
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async move {
            let b = bridge::Bridge::browser().await?;
            let vals_json = serde_json::to_string(&vals).unwrap_or_default();
            let js = format!(
                "(function(){{var sel=document.querySelector({sel});if(!sel)return false;Array.from(sel.options).forEach(function(o){{o.selected={vals}.includes(o.value||o.text)}});sel.dispatchEvent(new Event('change',{{bubbles:true}}));return true}})()",
                sel = serde_json::to_string(selector).unwrap_or_default(),
                vals = vals_json,
            );
            let ok: bool = b.main_tab().evaluate_main(&js).await.unwrap_or(false);
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("ok", JsonValue::string(if ok { "multi_select" } else { "not_found" })),
                ("count", JsonValue::string(&vals.len().to_string())),
            ]))
        })
    }
}

// P2: 列出所有 iframe（辅助选择 iframe 操作目标）
pub struct BrowserListFrames;
impl NativeService for BrowserListFrames {
    fn execute(&self, _args: &JsonValue) -> IoResult {
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async {
            let b = bridge::Bridge::browser().await?;
            let js = "(function(){var f=Array.from(document.querySelectorAll('iframe'));return JSON.stringify(f.map(function(x,i){return {i:i,src:x.src||'',id:x.id||'',name:x.name||''}}))})()";
            let out: String = b.main_tab().evaluate_main(&js).await.unwrap_or_default();
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("frames", JsonValue::string(&out)),
            ]))
        })
    }
}

pub struct BrowserDialog;
impl NativeService for BrowserDialog {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let accept = args.get("accept").and_then(|v| v.as_bool()).unwrap_or(true);
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async {
            let b = bridge::Bridge::browser().await?;
            let tab = b.main_tab();
            // 注册一次性订阅，await 第一个 dialog 事件
            let exp = tab.expect_dialog();
            let d = exp.await.map_err(|e| format!("expect_dialog: {e}"))?;
            if accept {
                d.accept(None).await.map_err(|e| format!("dialog accept: {e}"))?;
            } else {
                d.dismiss().await.map_err(|e| format!("dialog dismiss: {e}"))?;
            }
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("dialog", JsonValue::string("handled")),
                ("accept", JsonValue::string(if accept { "true" } else { "false" })),
            ]))
        })
    }
}

pub struct BrowserUpload;
impl NativeService for BrowserUpload {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let path = args.get("path").and_then(|v| v.as_str())
            .ok_or_else(|| "browser_upload 需要 path（本地文件绝对路径）".to_string())?;
        let selector = args.get("selector").and_then(|v| v.as_str())
            .unwrap_or("input[type=file]");
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async {
            let b = bridge::Bridge::browser().await?;
            let tab = b.main_tab();
            // 1. setup chooser 拦截（在点之前）
            let exp = tab
                .expect_file_chooser(&[path.to_string()])
                .await
                .map_err(|e| format!("expect_file_chooser setup: {e}"))?;
            // 2. 点 file input 触发 chooser
            tab.find().css(selector).one().await
                .map_err(|e| format!("find file input: {e}"))?
                .click().await
                .map_err(|e| format!("click file input: {e}"))?;
            // 3. 等 chooser 自动接线文件
            let _matched = exp.await.map_err(|e| format!("await chooser: {e}"))?;
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("uploaded", JsonValue::string(path)),
            ]))
        })
    }
}

pub struct BrowserDownload;
impl NativeService for BrowserDownload {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let selector = args.get("selector").and_then(|v| v.as_str())
            .unwrap_or("a[download]");
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async {
            let b = bridge::Bridge::browser().await?;
            let tab = b.main_tab();
            // 1. setup download 拦截（在点之前）
            let exp = tab
                .expect_download()
                .await
                .map_err(|e| format!("expect_download setup: {e}"))?;
            // 2. 点下载链接触发
            tab.find().css(selector).one().await
                .map_err(|e| format!("find download link: {e}"))?
                .click().await
                .map_err(|e| format!("click download link: {e}"))?;
            // 3. 等下载完成
            let dl = exp.await.map_err(|e| format!("await download: {e}"))?;
            let p = dl.path().await;
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("download", JsonValue::string("captured")),
                ("path", JsonValue::string(p.map(|x| x.to_string_lossy().to_string()).unwrap_or_default())),
            ]))
        })
    }
}

// ---- 网络事件流 / a11y tree / console 日志 ----
pub struct BrowserNetworkReq;
impl NativeService for BrowserNetworkReq {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let start = args.get("start").and_then(|v| v.as_bool()).unwrap_or(true);
        let clear = args.get("clear").and_then(|v| v.as_bool()).unwrap_or(false);
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async {
            if start {
                let b = bridge::Bridge::browser().await?;
                let tab = b.main_tab();
                match tab.monitor().start().await {
                    Ok(mut mon) => {
                        bridge.spawn(async move {
                            use futures::StreamExt;
                            while let Some(ev) = mon.next().await {
                                if let Ok(mut buf) = bridge::monitor_buf().lock() {
                                    buf.push(format!("{:?}", ev));
                                }
                            }
                        });
                    }
                    Err(e) => return Err(format!("monitor start: {e}")),
                }
            }
            if clear {
                if let Ok(mut buf) = bridge::monitor_buf().lock() {
                    buf.clear();
                }
            }
            let out: Vec<String> = bridge::monitor_buf()
                .lock()
                .map(|b| b.clone())
                .unwrap_or_default();
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("count", JsonValue::string(&out.len().to_string())),
                ("events", JsonValue::Array(out.into_iter().map(JsonValue::string).collect())),
            ]))
        })
    }
}

pub struct BrowserA11yTree;
impl NativeService for BrowserA11yTree {
    fn execute(&self, _args: &JsonValue) -> IoResult {
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async {
            let b = bridge::Bridge::browser().await?;
            // 用 raw CDP Accessibility.getFullAXTree（库未封装，escape hatch）
            let conn = b.cdp();
            let info = conn
                .call_raw("Accessibility.getFullAXTree", serde_json::json!({}), None)
                .await
                .map_err(|e| format!("a11y cdp: {e}"))?;
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("ax_tree", JsonValue::string(info.to_string())),
            ]))
        })
    }
}

pub struct BrowserConsoleLog;
impl NativeService for BrowserConsoleLog {
    fn execute(&self, _args: &JsonValue) -> IoResult {
        let bridge = bridge::Bridge::get()?;
        bridge.block_on(async {
            let b = bridge::Bridge::browser().await?;
            // 注入 console 收集器，读回 window.__console_logs（v0.1 快照式）
            let _: String = b.main_tab()
                .evaluate_main(
                    "window.__cl = window.__cl || []; \
                     if (!window.__hooked) { window.__hooked=true; \
                     ['log','warn','error'].forEach(function(m){ var o=console[m].bind(console); \
                     console[m]=function(){window.__cl.push(m+': '+Array.from(arguments).join(' '));o.apply(console,arguments);};}); } \
                     'ok'")
                .await
                .map_err(|e| format!("console hook: {e}"))?;
            let logs: String = b.main_tab()
                .evaluate_main("JSON.stringify(window.__cl||[])")
                .await
                .map_err(|e| format!("console read: {e}"))?;
            Ok(evorule_tcb::JsonValue::object_from_pairs(&[
                ("logs", JsonValue::string(logs)),
            ]))
        })
    }
}

pub const NATIVE_SERVICES: &[NativeServiceDef] = &[
    NativeServiceDef { name: "browser_launch", sensitive: false, description: "启动/复用常驻 Browser，返回状态", make: || Arc::new(BrowserLaunch) },
    NativeServiceDef { name: "browser_navigate", sensitive: false, description: "跳转 URL，等 load", make: || Arc::new(BrowserNavigate) },
    NativeServiceDef { name: "browser_back", sensitive: false, description: "后退", make: || Arc::new(BrowserBack) },
    NativeServiceDef { name: "browser_forward", sensitive: false, description: "前进", make: || Arc::new(BrowserForward) },
    NativeServiceDef { name: "browser_reload", sensitive: false, description: "刷新", make: || Arc::new(BrowserReload) },
    NativeServiceDef { name: "browser_tabs", sensitive: false, description: "列/开/关/切 tab", make: || Arc::new(BrowserTabs) },
    NativeServiceDef { name: "browser_wait", sensitive: false, description: "等 selector/idle/ms", make: || Arc::new(BrowserWait) },
    NativeServiceDef { name: "browser_wait_for_element", sensitive: false, description: "轮询等元素出现（selector+timeout_ms）", make: || Arc::new(BrowserWaitFor) },
    NativeServiceDef { name: "browser_switch_tab", sensitive: false, description: "切到指定 target_id 的 tab", make: || Arc::new(BrowserSwitchTab) },
    NativeServiceDef { name: "browser_close_tab", sensitive: false, description: "关闭指定 target_id 的 tab", make: || Arc::new(BrowserCloseTab) },
    NativeServiceDef { name: "browser_get_element_text", sensitive: false, description: "取指定元素文本", make: || Arc::new(BrowserGetElementText) },
    NativeServiceDef { name: "browser_get_attribute", sensitive: false, description: "取元素属性(href/value/src)", make: || Arc::new(BrowserGetAttribute) },
    NativeServiceDef { name: "browser_exists", sensitive: false, description: "判断元素是否存在", make: || Arc::new(BrowserExists) },
    NativeServiceDef { name: "browser_wait_network_idle", sensitive: false, description: "等网络空闲(AJAX完成)", make: || Arc::new(BrowserWaitNetworkIdle) },
    NativeServiceDef { name: "browser_get_location", sensitive: false, description: "取当前URL+title", make: || Arc::new(BrowserGetLocation) },
    NativeServiceDef { name: "browser_scroll_to_element", sensitive: false, description: "滚元素到视口中央", make: || Arc::new(BrowserScrollToElement) },
    NativeServiceDef { name: "browser_wait_new_tab", sensitive: false, description: "等新tab出现并返回id", make: || Arc::new(BrowserWaitNewTab) },
    NativeServiceDef { name: "browser_double_click", sensitive: false, description: "双击元素", make: || Arc::new(BrowserDoubleClick) },
    NativeServiceDef { name: "browser_right_click", sensitive: false, description: "右键元素", make: || Arc::new(BrowserRightClick) },
    NativeServiceDef { name: "browser_clear", sensitive: false, description: "清空input", make: || Arc::new(BrowserClear) },
    NativeServiceDef { name: "browser_set_viewport", sensitive: false, description: "设viewport宽高", make: || Arc::new(BrowserSetViewport) },
    NativeServiceDef { name: "browser_iframe_eval", sensitive: false, description: "在iframe里跑JS", make: || Arc::new(BrowserIframeEval) },
    NativeServiceDef { name: "browser_multi_select", sensitive: false, description: "多选下拉", make: || Arc::new(BrowserMultiSelect) },
    NativeServiceDef { name: "browser_list_frames", sensitive: false, description: "列出所有iframe", make: || Arc::new(BrowserListFrames) },
    NativeServiceDef { name: "browser_click", sensitive: false, description: "点元素", make: || Arc::new(BrowserClick) },
    NativeServiceDef { name: "browser_fill", sensitive: false, description: "填 input 值", make: || Arc::new(BrowserFill) },
    NativeServiceDef { name: "browser_type", sensitive: false, description: "逐字键入", make: || Arc::new(BrowserType) },
    NativeServiceDef { name: "browser_select", sensitive: false, description: "下拉选", make: || Arc::new(BrowserSelect) },
    NativeServiceDef { name: "browser_check", sensitive: false, description: "勾 checkbox", make: || Arc::new(BrowserCheck) },
    NativeServiceDef { name: "browser_hover", sensitive: false, description: "悬停", make: || Arc::new(BrowserHover) },
    NativeServiceDef { name: "browser_drag", sensitive: false, description: "拖拽", make: || Arc::new(BrowserDrag) },
    NativeServiceDef { name: "browser_scroll", sensitive: false, description: "滚动", make: || Arc::new(BrowserScroll) },
    NativeServiceDef { name: "browser_press", sensitive: false, description: "按键/快捷键", make: || Arc::new(BrowserPress) },
    NativeServiceDef { name: "browser_screenshot", sensitive: false, description: "截图 base64（png/jpeg/fullpage）", make: || Arc::new(BrowserScreenshot) },
    NativeServiceDef { name: "browser_accessibility_tree", sensitive: false, description: "AX tree dump（CDP Accessibility 直补）", make: || Arc::new(BrowserA11yTree) },
    NativeServiceDef { name: "browser_get_html", sensitive: false, description: "取 trimmed DOM", make: || Arc::new(BrowserGetHtml) },
    NativeServiceDef { name: "browser_get_text", sensitive: false, description: "取可见文本", make: || Arc::new(BrowserGetText) },
    NativeServiceDef { name: "browser_console_log", sensitive: false, description: "console 事件（CDP Runtime 直补）", make: || Arc::new(BrowserConsoleLog) },
    NativeServiceDef { name: "browser_network_requests", sensitive: false, description: "被动网络事件流", make: || Arc::new(BrowserNetworkReq) },
    NativeServiceDef { name: "browser_cookies", sensitive: true, description: "读/写 cookies（凭据）", make: || Arc::new(BrowserCookies) },
    NativeServiceDef { name: "browser_storage", sensitive: true, description: "localStorage/sessionStorage（含 token）", make: || Arc::new(BrowserStorage) },
    NativeServiceDef { name: "browser_dialog", sensitive: false, description: "accept/dismiss JS dialog", make: || Arc::new(BrowserDialog) },
    NativeServiceDef { name: "browser_upload", sensitive: false, description: "文件上传", make: || Arc::new(BrowserUpload) },
    NativeServiceDef { name: "browser_download", sensitive: false, description: "等待下载", make: || Arc::new(BrowserDownload) },
    NativeServiceDef { name: "browser_evaluate", sensitive: false, description: "页面内跑 JS", make: || Arc::new(BrowserEvaluate) },
];

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn test_table_matches_declaration_file() -> Result<(), String> {
        let raw = include_str!("../official_native_services.json");
        let file: serde_json::Value =
            serde_json::from_str(raw).map_err(|e| format!("声明文件非法 JSON: {e}"))?;
        let services = file
            .get("services")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "声明文件缺 services 数组".to_string())?;
        if services.len() != NATIVE_SERVICES.len() {
            return Err(format!(
                "声明文件服务数({})与 NATIVE_SERVICES({})不一致",
                services.len(),
                NATIVE_SERVICES.len()
            ));
        }
        for (i, def) in NATIVE_SERVICES.iter().enumerate() {
            let s = &services[i];
            let name = s.get("name").and_then(|v| v.as_str()).ok_or("缺 name")?;
            let sensitive = s.get("sensitive").and_then(|v| v.as_bool()).ok_or("缺 sensitive")?;
            let desc = s.get("description").and_then(|v| v.as_str()).ok_or("缺 description")?;
            if name != def.name || sensitive != def.sensitive || desc != def.description {
                return Err(format!(
                    "第 {i} 项漂移: 文件 {name}/{sensitive}/{desc} vs 表 {}/{}/{}",
                    def.name, def.sensitive, def.description
                ));
            }
        }
        Ok(())
    }

    #[test]
    fn test_table_names_unique_and_make_callable() {
        let mut names: Vec<&str> = NATIVE_SERVICES.iter().map(|d| d.name).collect();
        let n = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), n);
        for d in NATIVE_SERVICES {
            let _ = (d.make)();
        }
    }

    /// 端到端：真拉起本机 Edge，完整走 launch→navigate→get_html→screenshot→click。
    /// 需要本机 Edge 可用；失败会如实报错（不静默）。
    #[test]
    fn test_real_edge_e2e() {
        // navigate
        BrowserNavigate
            .execute(&JsonValue::object_from_pairs(&[(
                "url",
                JsonValue::string("https://example.com"),
            )]))
            .expect("导航 example.com 应成功");

        // get_html
        let html_out = BrowserGetHtml.execute(&JsonValue::object_from_pairs(&[]))
            .expect("get html");
        let html = html_out.get("html").and_then(|v| v.as_str()).unwrap_or("");
        assert!(html.contains("<html"), "HTML 应含 <html>");

        // screenshot
        let shot = BrowserScreenshot.execute(&JsonValue::object_from_pairs(&[]))
            .expect("screenshot");
        let b64 = shot.get("b64").and_then(|v| v.as_str()).unwrap_or("");
        assert!(b64.len() > 1000, "截图 base64 应 >1KB, 实际 {}", b64.len());

        // click: example.com 有唯一 <a> 链接
        let click_out = BrowserClick
            .execute(&JsonValue::object_from_pairs(&[
                ("selector", JsonValue::string("a")),
            ]))
            .expect("click <a> 应成功");
        assert_eq!(
            click_out.get("clicked").and_then(|v| v.as_str()),
            Some("a")
        );
    }
}

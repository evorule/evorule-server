// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! ai-plugin 外部插件包 —— 服务端驻留 LLM 执行者（契约 v1.2；回归验证 L1 地基）。
//!
//! 形态：独立 HTTP 进程（flow-studio 同模式）。服务端点收到调用后自建
//! 一次性 sidecar 审计会话，走完整 `call_external → LLM → io_response → Stable`
//! 回路再返回——prompt/结果全文照旧入审计链，与浏览器审计桥同协议位。
//!
//! 路由：
//!   POST /services/ai_plugin_chat
//!        body: { messages: [{role,content}...], model?, temperature?, audit_purpose? }
//!        → 200 { "reply": "...", "session_id": N }   （session_id 供审计对账）
//!        → 400 { "error": "..." }  请求体非法
//!        → 500 { "error": "..." }  插件配置问题（fail-fast 自诊断）
//!        → 502 { "error": "..." }  server 不可达/协议/引擎/LLM 失败（错误已回写 io_response）
//!   POST /llm/execute（SG-1 件1 扩展型执行体，单规格双消费）
//!        body: OpenAI 兼容请求体原样（7 键全量透传，不裁剪）
//!        → 200 agent LlmResponse 同构四键 {content, tool_calls, finish_reason, token_usage}
//!        → 500 { "error": "..." }  扩展执行体未配置/白名单物料未就绪（fail-closed）
//!        → 502 { "error": "..." }  provider 失败 / 模型不在准入白名单（网络入口执法）
//!   POST /tools/web_search（批K 段2 web_search 中继；evo-agent 进程零密钥）
//!        body: { "query": "...", "count"?: n }   （count 缺省 5、代码级帽 10）
//!        → 200 { status:"ok", provider, query, result_count, results:[{title,url,description}] }
//!        → 400 { "error": "..." }  请求体非法
//!        → 500 { "error": "..." }  未启用/Brave key 未配置（fail-visible 自诊断）
//!        → 502 { "error": "..." }  Brave 上游失败（归因透传）
//!   GET  /health      存活探针
//!
//! 红线：本进程内 LLM 出站仅两条受管辖路径——①call_external 审计回路内的
//! `call_llm`（sidecar IoRequest 分支专用，见 lib.rs）；②扩展执行体
//! `call_llm_ext`（POST /llm/execute，模型白名单网络入口执法，prompt 已由
//! agent 侧命令事实先行入链）。非 LLM 出站仅 web_search 中继一条
//! （POST /tools/web_search → Brave Search API，key 仅本插件侧持有）；其余
//! 任何直连禁止；凭据（llm_api_key/brave_api_key）不进日志/错误消息。
//!
//! 配置：`ai-plugin.json`（--config 指定；缺省依次找 exe 同目录/工作目录）。
//! 缺文件 fail-fast 自诊断退出——不静默、不带缺省凭据启动（清单缺省禁用，
//! 部署方显式配置后才装载）。

#![forbid(unsafe_code)]

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use evorule_ai_plugin::{
    build_http_client, build_llm_execute_client, build_model_routes, call_llm_ext,
    call_llm_ext_stream, load_model_allowlist_file, run_audited_chat, web_search_relay,
    ChatRequest, ModelRoutes, PluginConfig, PluginError, WebSearchError, WEB_SEARCH_DEFAULT_COUNT,
    WEB_SEARCH_MAX_COUNT,
};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone)]
struct PluginState {
    cfg: Arc<PluginConfig>,
    http: std::sync::Arc<reqwest::Client>,
    /// 扩展执行体出站客户端（重定向禁令 + connect 超时；独立于 sidecar 回路客户端）
    execute_http: std::sync::Arc<reqwest::Client>,
    /// 模型准入白名单（None = 扩展执行体未就绪，/llm/execute 显式拒绝；
    /// 内层 Arc 槽支持物料热重载——读侧零锁拷贝，写侧整槽替换）
    allowlist: AllowlistHot,
    /// 多 provider 路由表（None = 未启用 llm_providers，/llm/execute 走单一
    /// llm_execute_url；Some = 路由模式，按模型解析端点/凭据，未命中显式拒）
    routes: Option<Arc<ModelRoutes>>,
    /// 调用方令牌（Some = /llm/execute 校验 Authorization;None = 不鉴权）
    caller_token: Option<String>,
}

/// 可热换的白名单槽：Arc<RwLock<Arc<BTreeSet>>>——读侧 clone Arc（无锁宽临界
/// 区），热重载写侧整槽替换；None = 未就绪（fail-closed）。物料文件 mtime
/// 轮询触发重载，重载失败保留旧表并 warn 留痕（不降级、不断服）。
#[derive(Clone)]
struct AllowlistHot {
    path: Option<String>,
    state: Arc<std::sync::RwLock<Option<Arc<BTreeSet<String>>>>>,
}

impl AllowlistHot {
    fn unready() -> Self {
        Self {
            path: None,
            state: Arc::new(std::sync::RwLock::new(None)),
        }
    }

    fn ready(path: String, set: BTreeSet<String>) -> Self {
        Self {
            path: Some(path),
            state: Arc::new(std::sync::RwLock::new(Some(Arc::new(set)))),
        }
    }

    fn get(&self) -> Option<Arc<BTreeSet<String>>> {
        // 锁中毒（写侧 panic 残留）按未就绪处理：fail-closed 拒绝，不扩散 panic
        self.state.read().ok().and_then(|g| g.clone())
    }

    fn swap(&self, set: BTreeSet<String>) {
        if let Ok(mut g) = self.state.write() {
            *g = Some(Arc::new(set));
        } else {
            tracing::error!("allowlist 槽锁中毒，热重载写入被拒（fail-closed 保留执法现状）");
        }
    }
}

/// 物料热重载任务：mtime 轮询（10s）驱动；文件 mtime 变化才重载。
async fn allowlist_hot_reload_task(path: String, hot: AllowlistHot) {
    let mut last = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok());
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        let current = std::fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok());
        if current.is_some() && current != last {
            last = current;
            match load_model_allowlist_file(&path) {
                Ok(set) => {
                    tracing::info!("白名单物料热重载成功: {path}（{} 个模型）", set.len());
                    hot.swap(set);
                }
                Err(e) => {
                    tracing::warn!("白名单物料热重载失败，保留旧表（fail-visible 不断服）: {e}");
                }
            }
        }
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // 参数解析（轻量手写，插件包不引入 clap 依赖）
    let mut config_path: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => match args.next() {
                Some(p) => config_path = Some(PathBuf::from(p)),
                None => {
                    eprintln!("[ai-plugin] --config 需要一个配置文件路径");
                    std::process::exit(2);
                }
            },
            "--help" | "-h" => {
                println!("ai-plugin [--config <ai-plugin.json>]");
                println!(
                    "服务端驻留 LLM 执行者（契约 v1.2）：POST /services/ai_plugin_chat；\
                     plugin.json service.base_url 指向本服务"
                );
                println!(
                    "配置缺省查找顺序: --config 指定路径 → exe 同目录/ai-plugin.json → 工作目录/ai-plugin.json"
                );
                return;
            }
            other => {
                eprintln!("[ai-plugin] 未知参数 {other}（--help 查看用法）");
                std::process::exit(2);
            }
        }
    }

    // 配置装载（fail-fast 自诊断：缺文件即退出，不静默启动）
    let (path_used, content) = match resolve_config(config_path) {
        Ok(pair) => pair,
        Err(msg) => {
            eprintln!("[ai-plugin] {msg}");
            std::process::exit(1);
        }
    };
    let cfg = match PluginConfig::from_json_str(&content) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[ai-plugin] 配置文件非法 {}: {e}", path_used.display());
            std::process::exit(1);
        }
    };
    tracing::info!(
        "ai-plugin 配置装载成功: {}（server={} llm_model={} 监听={} 凭据来源={:?}）",
        path_used.display(),
        cfg.server_base_url,
        cfg.llm_model,
        cfg.listen_addr,
        cfg.llm_api_key_source
    );

    let listen_addr = cfg.listen_addr.clone();
    // 扩展执行体就绪判定（SG-1 件1）：llm_execute_url 或 llm_providers 任一
    // 配置即要求白名单物料可加载——加载失败仅降级为「/llm/execute 全拒」
    // （fail-closed，自诊断留痕），不阻断既有 /services/ai_plugin_chat 通道启动。
    let routing_mode = cfg.llm_providers.as_ref().is_some_and(|p| !p.is_empty());
    let allowlist = if cfg.llm_execute_url.is_some() || routing_mode {
        match cfg.model_allowlist_path.as_deref() {
            None => {
                tracing::warn!(
                    "扩展执行体已配置但 model_allowlist_path 缺失: /llm/execute 将以\
                     自诊断消息拒绝所有请求（fail-closed）"
                );
                None
            }
            Some(p) => match load_model_allowlist_file(p) {
                Ok(set) => {
                    tracing::info!("模型白名单物料装载成功: {p}（{} 个模型）", set.len());
                    Some(set)
                }
                Err(e) => {
                    tracing::warn!(
                        "模型白名单物料装载失败: {e}（/llm/execute 将以自诊断消息\
                         拒绝所有请求，fail-closed）"
                    );
                    None
                }
            },
        }
    } else {
        None
    };
    // 多 provider 路由装配（可选 llm_providers 配置节）：模型 ⊆ 白名单物料 +
    // 凭据环境变量在位才装载；部分/全部未就绪只降级自诊断留痕，不阻断启动，
    // 且绝不回落单端点路径（fail-closed）。
    let (routes, route_diags) =
        build_model_routes(&cfg, allowlist.as_ref(), &|name| std::env::var(name).ok());
    for d in &route_diags {
        tracing::warn!("{d}");
    }
    match routes.as_ref() {
        Some(r) if !r.is_empty() => tracing::info!(
            "多 provider 路由装载完成: {} 个模型可达（providers: {}）",
            r.routed_models().count(),
            cfg.llm_providers
                .as_ref()
                .map(|ps| ps
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "))
                .unwrap_or_default()
        ),
        Some(_) => tracing::warn!(
            "多 provider 路由已配置但无一 provider 就绪: /llm/execute 将拒绝所有请求\
             （fail-closed，见上方诊断）"
        ),
        None => {}
    }
    let caller_token = cfg.caller_token.clone();
    let allowlist_hot = match (allowlist, cfg.model_allowlist_path.clone()) {
        (Some(set), Some(path)) => AllowlistHot::ready(path, set),
        _ => AllowlistHot::unready(),
    };
    let state = PluginState {
        cfg: Arc::new(cfg),
        http: std::sync::Arc::new(match build_http_client() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[ai-plugin] {e}");
                std::process::exit(1);
            }
        }),
        execute_http: std::sync::Arc::new(match build_llm_execute_client() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[ai-plugin] {e}");
                std::process::exit(1);
            }
        }),
        allowlist: allowlist_hot.clone(),
        routes: routes.map(Arc::new),
        caller_token,
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("[ai-plugin] tokio runtime 构建失败: {e}");
            std::process::exit(1);
        }
    };
    // 物料热重载哨兵：mtime 轮询驱动准入清单热生效（翻译器/配置面改文件
    // 即可，无需重启插件；重载失败保留旧表，fail-visible 不断服）
    if let Some(path) = state.allowlist.path.clone() {
        let hot = state.allowlist.clone();
        runtime.spawn(allowlist_hot_reload_task(path, hot));
    }
    runtime.block_on(async move {
        let app = Router::new()
            .route("/services/ai_plugin_chat", post(ai_plugin_chat))
            .route("/llm/execute", post(llm_execute))
            .route("/llm/execute-stream", post(llm_execute_stream))
            .route("/llm/models", get(llm_models))
            .route("/tools/web_search", post(web_search))
            .route("/health", get(health))
            .with_state(state);
        let listener = match tokio::net::TcpListener::bind(&listen_addr).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!(
                    "[ai-plugin] 监听 {listen_addr} 失败: {e}（自诊断指引: ① 端口被占用请用 \
                     配置 listen_addr 换端口; ② 同步修改 plugin.json base_url 并重启 server）"
                );
                std::process::exit(1);
            }
        };
        tracing::info!("ai-plugin 就绪: http://{listen_addr}（POST /services/ai_plugin_chat）");
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("[ai-plugin] 服务异常退出: {e}");
            std::process::exit(1);
        }
    });
}

/// 配置查找：--config 显式路径 → exe 同目录 → 工作目录。
/// 返回 (实际使用的路径, 文件内容)；找不到返回自诊断消息。
fn resolve_config(explicit: Option<PathBuf>) -> Result<(PathBuf, String), String> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(p) = explicit {
        candidates.push(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("ai-plugin.json"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("ai-plugin.json"));
    }
    for c in &candidates {
        if let Ok(content) = std::fs::read_to_string(c) {
            return Ok((c.clone(), content));
        }
    }
    Err(format!(
        "找不到配置文件 ai-plugin.json（已查找: {}）。\
         自诊断指引: ① 复制 plugins/ai-plugin/config.example.json 为 ai-plugin.json; \
         ② 填写 server_base_url/llm_endpoint/llm_model，LLM 凭据推荐设环境变量 \
         EVORULE_AI_PLUGIN_LLM_API_KEY（不落盘），或填入文件 llm_api_key; \
         ③ 用 --config 指定路径或放到 exe 同目录; \
         ④ plugin.json 中 ai-plugin 条目 enabled=false 时不会装载（缺省禁用，配置完成后显式开启）",
        candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(" → ")
    ))
}

async fn health() -> Json<Value> {
    Json(json!({ "ok": true, "service": "ai-plugin" }))
}

/// 扩展执行体门禁（/llm/execute 与 /llm/execute-stream 共用）：调用方令牌
/// 校验（配置后启用）+ 白名单就绪校验（fail-closed）。通过返回白名单引用，
/// 否则返回已构造的错误响应。
fn require_ext_gate(
    st: &PluginState,
    headers: &axum::http::HeaderMap,
) -> Result<std::sync::Arc<BTreeSet<String>>, Response> {
    // 调用方令牌校验(配置后启用)——本机其他进程无令牌即 401,
    // 防烧用户配额;浏览器侧被 JSON preflight 天然阻断。
    if let Some(expected) = st.caller_token.as_deref() {
        let present = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        if present != Some(&format!("Bearer {expected}")) {
            return Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(json!({ "error": "caller token missing or mismatched" })),
            )
                .into_response());
        }
    }
    match st.allowlist.get() {
        Some(allowlist) => Ok(allowlist),
        None => {
            let ext_configured = st.cfg.llm_execute_url.is_some()
                || st.cfg.llm_providers.as_ref().is_some_and(|p| !p.is_empty());
            let diag = if ext_configured {
                "扩展执行体未就绪: model_allowlist_path 未配置或白名单物料装载失败\
                 （见启动日志；物料 = 00_constraint_model_allowlist.json server 侧副本）"
                    .to_string()
            } else {
                "扩展执行体未就绪: 配置键 llm_execute_url / model_allowlist_path 未填写\
                 （provider 全 URL 形态，如 https://api.minimaxi.com/v1/text/chatcompletion_v2）"
                    .to_string()
            };
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({ "error": diag })),
            )
                .into_response())
        }
    }
}

/// 扩展型执行体端点（SG-1 件1）：body 原样 → 四键。
/// 未就绪（未配置/白名单物料未装载）→ 500 自诊断（fail-closed，绝不静默放行）；
/// 白名单违规/provider 失败 → 502 显式上抛（fail-visible，无静默兜底）。
async fn llm_execute(
    State(st): State<PluginState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let allowlist = match require_ext_gate(&st, &headers) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    match call_llm_ext(
        &st.cfg,
        &st.execute_http,
        &allowlist,
        st.routes.as_deref(),
        &body,
    )
    .await
    {
        Ok(v) => Json(v).into_response(),
        Err(e) => {
            tracing::warn!("llm/execute 失败: {e}");
            error_response(&e)
        }
    }
}

/// 扩展型执行体真流式端点：body 原样 → 归一化 SSE 帧逐帧下行（真流式，
/// 思考/正文/tool_call 分片全程不经整包缓冲）。门禁与 /llm/execute 同面
/// （fail-closed）；provider 失败在 SSE 头发出前 → 错误响应，流内失败 →
/// error 帧下行后即止（帧协议见 lib.rs call_llm_ext_stream 文档）。
async fn llm_execute_stream(
    State(st): State<PluginState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let allowlist = match require_ext_gate(&st, &headers) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    match call_llm_ext_stream(
        &st.cfg,
        &st.execute_http,
        &allowlist,
        st.routes.as_deref(),
        &body,
    )
    .await
    {
        Ok(frames) => match axum::http::Response::builder()
            .status(StatusCode::OK)
            .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
            .body(axum::body::Body::from_stream(frames))
        {
            Ok(resp) => resp.into_response(),
            Err(e) => {
                tracing::warn!("llm/execute-stream: SSE 响应构建失败: {e}");
                error_response(&PluginError::Config(format!("SSE 响应构建失败: {e}")))
            }
        },
        Err(e) => {
            tracing::warn!("llm/execute-stream 失败: {e}");
            error_response(&e)
        }
    }
}

/// 模型清单端点（S5 模型选择器配套）：白名单只读下发——正源即执法面同一
/// BTreeSet，杜绝 serve 侧复制产生双真相。`{ready, models}`：未就绪
/// ready=false+空表+自诊断语（fail-closed 姿态对消费方可见）；就绪出排序
/// 数组。本端点不携带任何凭据物料。
async fn llm_models(State(st): State<PluginState>, headers: axum::http::HeaderMap) -> Response {
    // 调用方令牌校验与 /llm/execute 同面一致（配置后启用）
    if let Some(expected) = st.caller_token.as_deref() {
        let present = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        if present != Some(&format!("Bearer {expected}")) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "caller token missing or mismatched" })),
            )
                .into_response();
        }
    }
    let hot = st.allowlist.get();
    match hot.as_ref() {
        None => Json(json!({
            "ready": false,
            "models": [],
            "note": "扩展执行体未就绪: 白名单物料未配置或装载失败（见启动日志）"
        }))
        .into_response(),
        Some(set) => match st.routes.as_ref() {
            // 未启用多 provider 路由：清单=白名单全量（既有行为零变化）
            None => {
                let models: Vec<&String> = set.iter().collect();
                Json(json!({ "ready": true, "models": models })).into_response()
            }
            // 路由模式：清单=白名单 ∩ 已装载路由（视图过滤，正源仍是物料；
            // 未路由模型随 note 显式可见，不在可选清单里出现坏选项）
            Some(routes) => {
                let models: Vec<&String> =
                    set.iter().filter(|m| routes.resolve(m).is_some()).collect();
                let unrouted: Vec<&str> = set
                    .iter()
                    .filter(|m| routes.resolve(m).is_none())
                    .map(String::as_str)
                    .collect();
                if models.is_empty() {
                    Json(json!({
                        "ready": false,
                        "models": [],
                        "note": "已配置多 provider 路由但无一 provider 就绪\
                                 （凭据环境变量缺失或模型超出白名单，见启动日志）"
                    }))
                    .into_response()
                } else if unrouted.is_empty() {
                    Json(json!({ "ready": true, "models": models })).into_response()
                } else {
                    Json(json!({
                        "ready": true,
                        "models": models,
                        "note": format!(
                            "{} 个白名单模型未配置已就绪 provider 路由，暂不可选: {}",
                            unrouted.len(),
                            unrouted.join(", ")
                        ),
                    }))
                    .into_response()
                }
            }
        },
    }
}

/// web_search 中继端点（批K 段2）：evo-agent WebSearchTool → Brave Search。
/// 调用方令牌校验与 /llm/execute 同面一致（配置后启用；本机其他进程无令牌
/// 即 401）。请求体非法 → 400；未启用/未配 key → 500 自诊断（fail-visible，
/// 绝不静默）；Brave 上游失败 → 502 归因透传。key 不进日志/错误消息。
async fn web_search(
    State(st): State<PluginState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    // 调用方令牌校验（配置后启用，与流式端点同款）
    if let Some(expected) = st.caller_token.as_deref() {
        let present = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        if present != Some(&format!("Bearer {expected}")) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "caller token missing or mismatched" })),
            )
                .into_response();
        }
    }
    // 请求体校验：query 非空字符串；count 缺省 5、正整数、代码级帽 10
    let Some(query) = body
        .get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|q| !q.is_empty())
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "字段 'query' 须为非空字符串" })),
        )
            .into_response();
    };
    let count = match body.get("count") {
        None | Some(Value::Null) => WEB_SEARCH_DEFAULT_COUNT,
        Some(Value::Number(n)) => match n.as_u64() {
            Some(c) if c >= 1 => c.min(WEB_SEARCH_MAX_COUNT),
            _ => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "字段 'count' 须为正整数" })),
                )
                    .into_response()
            }
        },
        Some(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "字段 'count' 须为正整数" })),
            )
                .into_response()
        }
    };
    match web_search_relay(&st.cfg, query, count).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => {
            tracing::warn!("tools/web_search 失败: {e}");
            let status = match &e {
                WebSearchError::NotReady(_) => StatusCode::INTERNAL_SERVER_ERROR,
                WebSearchError::Upstream(_) => StatusCode::BAD_GATEWAY,
            };
            (status, Json(json!({ "error": e.to_string() }))).into_response()
        }
    }
}

async fn ai_plugin_chat(State(st): State<PluginState>, Json(body): Json<Value>) -> Response {
    // 请求体校验：消费方错误 → 400
    let req = match ChatRequest::from_value(&body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    };
    let purpose = req
        .audit_purpose
        .clone()
        .unwrap_or_else(|| "chat".to_string());
    match run_audited_chat(&st.cfg, &st.http, &req).await {
        Ok(outcome) => {
            tracing::info!(
                "ai_plugin_chat 完成: session={} purpose={purpose} reply_chars={}",
                outcome.session_id,
                outcome.reply.chars().count()
            );
            Json(json!({ "reply": outcome.reply, "session_id": outcome.session_id }))
                .into_response()
        }
        Err(e) => {
            // 错误已回写 io_response（lib 层保证），此处显式上抛，无静默兜底
            tracing::warn!("ai_plugin_chat 失败: purpose={purpose} error={e}");
            error_response(&e)
        }
    }
}

/// 错误 → HTTP 映射：插件自身配置问题 500；上游(server/协议/引擎/LLM) 502。
fn error_response(e: &PluginError) -> Response {
    let status = match e {
        PluginError::Config(_) => StatusCode::INTERNAL_SERVER_ERROR,
        PluginError::ServerUnreachable(_)
        | PluginError::Protocol(_)
        | PluginError::Engine(_)
        | PluginError::Llm(_) => StatusCode::BAD_GATEWAY,
    };
    (status, Json(json!({ "error": e.to_string() }))).into_response()
}

#[cfg(test)]
mod tests {
    // 测试代码允许 unwrap/expect/panic（lib.rs tests 同先例：生产路径仍全量 deny）
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use axum::body::to_bytes;

    fn test_state(
        allowlist: Option<BTreeSet<String>>,
        caller_token: Option<String>,
    ) -> PluginState {
        let cfg = PluginConfig::from_json_str(
            r#"{
                "listen_addr": "127.0.0.1:0",
                "server_base_url": "http://127.0.0.1:18080",
                "llm_endpoint": "https://example.invalid/v1",
                "llm_api_key": "unused-in-test",
                "llm_model": "test-model",
                "llm_temperature": 0.2,
                "llm_timeout_ms": 1000,
                "tools_enabled": []
            }"#,
        )
        .expect("test config must parse");
        PluginState {
            cfg: Arc::new(cfg),
            http: Arc::new(reqwest::Client::new()),
            execute_http: Arc::new(reqwest::Client::new()),
            allowlist: match allowlist {
                Some(set) => AllowlistHot::ready("test-material.json".to_string(), set),
                None => AllowlistHot::unready(),
            },
            routes: None,
            caller_token,
        }
    }

    async fn body_json(resp: Response) -> Value {
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.expect("body");
        serde_json::from_slice(&bytes).expect("json body")
    }

    #[tokio::test]
    async fn llm_models_ready_lists_sorted_allowlist() {
        let st = test_state(
            Some(BTreeSet::from([
                "MiniMax-M3".to_string(),
                "MiniMax-M2.5".to_string(),
            ])),
            None,
        );
        let resp = llm_models(State(st), axum::http::HeaderMap::new()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["ready"], serde_json::json!(true));
        assert_eq!(
            v["models"],
            serde_json::json!(["MiniMax-M2.5", "MiniMax-M3"])
        );
    }

    #[tokio::test]
    async fn llm_models_unready_reports_diag_not_error() {
        let st = test_state(None, None);
        let resp = llm_models(State(st), axum::http::HeaderMap::new()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["ready"], serde_json::json!(false));
        assert_eq!(v["models"], serde_json::json!([]));
        assert!(v["note"].as_str().is_some_and(|s| !s.is_empty()));
    }

    #[tokio::test]
    async fn llm_models_requires_caller_token_when_configured() {
        let st = test_state(
            Some(BTreeSet::from(["m1".to_string()])),
            Some("secret-token".into()),
        );
        let resp = llm_models(State(st), axum::http::HeaderMap::new()).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let st = test_state(
            Some(BTreeSet::from(["m1".to_string()])),
            Some("secret-token".into()),
        );
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer secret-token"),
        );
        let resp = llm_models(State(st), headers).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ---------- 多 provider 路由（llm_providers 可选配置节） ----------

    /// 路由模式 PluginState：providers 配置与装配产物同源（build_model_routes
    /// 真装配路径），env 闭包注入测试凭据（隔离宿主真实环境变量）。
    fn routed_state(
        providers: Value,
        allowlist: BTreeSet<String>,
        env_key: &'static str,
        caller_token: Option<String>,
    ) -> PluginState {
        let cfg = PluginConfig::from_value_with_env(
            &json!({
                "listen_addr": "127.0.0.1:0",
                "server_base_url": "http://127.0.0.1:18080",
                "llm_endpoint": "https://example.invalid/v1",
                "llm_api_key": "legacy-unused",
                "llm_model": "test-model",
                "tools_enabled": [],
                "llm_providers": providers
            }),
            &|k: &str| {
                if k == env_key {
                    Some("routed-key".to_string())
                } else {
                    None
                }
            },
        )
        .expect("test config");
        let (routes, diags) = build_model_routes(&cfg, Some(&allowlist), &|k: &str| {
            if k == env_key {
                Some("routed-key".to_string())
            } else {
                None
            }
        });
        assert!(diags.is_empty(), "{diags:?}");
        PluginState {
            cfg: Arc::new(cfg),
            http: Arc::new(reqwest::Client::new()),
            execute_http: Arc::new(build_llm_execute_client().expect("execute client")),
            allowlist: AllowlistHot::ready("test-material.json".to_string(), allowlist),
            routes: routes.map(Arc::new),
            caller_token,
        }
    }

    #[tokio::test]
    async fn allowlist_hot_slot_swaps_atomically() {
        let hot = AllowlistHot::ready("m.json".to_string(), BTreeSet::from(["a".to_string()]));
        assert!(hot.get().expect("ready slot").contains("a"));
        hot.swap(BTreeSet::from(["b".to_string()]));
        let cur = hot.get().expect("ready slot");
        assert!(
            cur.contains("b") && !cur.contains("a"),
            "swap replaces whole set"
        );
        assert!(AllowlistHot::unready().get().is_none());
    }

    #[tokio::test]
    async fn llm_models_routing_mode_filters_unrouted_models_with_note() {
        let providers = json!([{
            "name": "p1",
            "execute_url": "http://127.0.0.1:1/v1",
            "api_key_env": "K_ROUTE",
            "models": ["m-b", "m-a"]
        }]);
        let st = routed_state(
            providers,
            BTreeSet::from(["m-a".to_string(), "m-b".to_string(), "m-c".to_string()]),
            "K_ROUTE",
            None,
        );
        let resp = llm_models(State(st), axum::http::HeaderMap::new()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["ready"], serde_json::json!(true));
        assert_eq!(v["models"], serde_json::json!(["m-a", "m-b"]));
        assert!(
            v["note"].as_str().unwrap().contains("m-c"),
            "未路由模型随 note 显式可见"
        );
    }

    #[tokio::test]
    async fn llm_models_routing_mode_all_unarmed_reports_not_ready() {
        // 凭据 env 缺失 → provider 全部未装载 → ready=false + 自诊断（不静默出空清单）
        let cfg = PluginConfig::from_value_with_env(
            &json!({
                "listen_addr": "127.0.0.1:0",
                "server_base_url": "http://127.0.0.1:18080",
                "llm_endpoint": "https://example.invalid/v1",
                "llm_api_key": "legacy-unused",
                "llm_model": "test-model",
                "tools_enabled": [],
                "llm_providers": [{
                    "name": "p1",
                    "execute_url": "http://127.0.0.1:1/v1",
                    "api_key_env": "K_MISSING",
                    "models": ["m-a"]
                }]
            }),
            &|_| None::<String>,
        )
        .expect("test config");
        let set = BTreeSet::from(["m-a".to_string()]);
        let (routes, _) = build_model_routes(&cfg, Some(&set), &|_| None::<String>);
        let st = PluginState {
            cfg: Arc::new(cfg),
            http: Arc::new(reqwest::Client::new()),
            execute_http: Arc::new(reqwest::Client::new()),
            allowlist: AllowlistHot::ready("test-material.json".to_string(), set),
            routes: routes.map(Arc::new),
            caller_token: None,
        };
        let resp = llm_models(State(st), axum::http::HeaderMap::new()).await;
        let v = body_json(resp).await;
        assert_eq!(v["ready"], serde_json::json!(false));
        assert!(v["note"].as_str().unwrap().contains("无一 provider 就绪"));
    }

    #[tokio::test]
    async fn llm_execute_routes_to_provider_upstream_with_default_params() {
        let captured: Arc<std::sync::Mutex<Option<(String, Value)>>> =
            Arc::new(std::sync::Mutex::new(None));
        let cap = captured.clone();
        let app = Router::new().route(
            "/v1/chat",
            post(
                move |headers: axum::http::HeaderMap, Json(body): Json<Value>| {
                    let cap = cap.clone();
                    async move {
                        let auth = headers
                            .get(axum::http::header::AUTHORIZATION)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string();
                        *cap.lock().unwrap() = Some((auth, body));
                        Json(json!({ "choices": [{ "message": { "content": "route-ok" } }] }))
                    }
                },
            ),
        );
        let addr = spawn_provider(app).await;
        let providers = json!([{
            "name": "p1",
            "execute_url": format!("http://{addr}/v1/chat"),
            "api_key_env": "K_ROUTE",
            "models": ["m-a"],
            "default_params": { "temperature": 0.9, "max_tokens": 111 }
        }]);
        let st = routed_state(
            providers,
            BTreeSet::from(["m-a".to_string()]),
            "K_ROUTE",
            None,
        );
        let resp = llm_execute(
            State(st),
            axum::http::HeaderMap::new(),
            Json(json!({ "model": "m-a", "messages": [{ "role": "user", "content": "hi" }] })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["content"], "route-ok");
        let (auth, body) = captured.lock().unwrap().clone().unwrap();
        assert_eq!(auth, "Bearer routed-key", "凭据来自 provider 专属 env");
        assert_eq!(
            body["temperature"],
            serde_json::json!(0.9),
            "provider 缺省参数打底"
        );
        assert_eq!(body["max_tokens"], serde_json::json!(111));
        assert_eq!(body["model"], "m-a");
    }

    #[tokio::test]
    async fn llm_execute_body_fields_override_provider_default_params() {
        let captured: Arc<std::sync::Mutex<Option<Value>>> = Arc::new(std::sync::Mutex::new(None));
        let cap = captured.clone();
        let app = Router::new().route(
            "/v1/chat",
            post(move |Json(body): Json<Value>| {
                let cap = cap.clone();
                async move {
                    *cap.lock().unwrap() = Some(body);
                    Json(json!({ "choices": [{ "message": { "content": "ok" } }] }))
                }
            }),
        );
        let addr = spawn_provider(app).await;
        let providers = json!([{
            "name": "p1",
            "execute_url": format!("http://{addr}/v1/chat"),
            "api_key_env": "K_ROUTE",
            "models": ["m-a"],
            "default_params": { "temperature": 0.9 }
        }]);
        let st = routed_state(
            providers,
            BTreeSet::from(["m-a".to_string()]),
            "K_ROUTE",
            None,
        );
        let resp = llm_execute(
            State(st),
            axum::http::HeaderMap::new(),
            Json(json!({ "model": "m-a", "messages": [], "temperature": 0.1 })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = captured.lock().unwrap().clone().unwrap();
        assert_eq!(
            body["temperature"],
            serde_json::json!(0.1),
            "请求体同键字段优先"
        );
    }

    #[tokio::test]
    async fn llm_execute_rejects_model_without_route_in_routing_mode() {
        let providers = json!([{
            "name": "p1",
            "execute_url": "http://127.0.0.1:1/v1",
            "api_key_env": "K_ROUTE",
            "models": ["m-a"]
        }]);
        let st = routed_state(
            providers,
            BTreeSet::from(["m-a".to_string(), "m-b".to_string()]),
            "K_ROUTE",
            None,
        );
        let resp = llm_execute(
            State(st),
            axum::http::HeaderMap::new(),
            Json(json!({ "model": "m-b", "messages": [] })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let v = body_json(resp).await;
        let err = v["error"].as_str().unwrap();
        assert!(err.contains("provider 路由"), "{err}");
        assert!(
            !err.contains("legacy-unused"),
            "错误消息不泄露任何凭据形态字段"
        );
    }

    // ---------- /llm/execute-stream 真流式端点（与 /llm/execute 同门禁面） ----------

    /// 构造带/不带 llm_execute_url 的 PluginState（env 闭包恒 None——隔离宿主
    /// 真机 key；execute_http 用带重定向禁令的真实出站客户端）
    fn stream_state(
        llm_execute_url: Option<&str>,
        allowlist: Option<BTreeSet<String>>,
        caller_token: Option<String>,
    ) -> PluginState {
        let mut cfg = json!({
            "listen_addr": "127.0.0.1:0",
            "server_base_url": "http://127.0.0.1:18080",
            "llm_endpoint": "https://example.invalid/v1",
            "llm_api_key": "test-key",
            "llm_model": "test-model",
            "tools_enabled": []
        });
        if let Some(u) = llm_execute_url {
            cfg["llm_execute_url"] = json!(u);
        }
        let cfg =
            PluginConfig::from_value_with_env(&cfg, &|_| None::<String>).expect("test config");
        PluginState {
            cfg: Arc::new(cfg),
            http: Arc::new(reqwest::Client::new()),
            execute_http: Arc::new(build_llm_execute_client().expect("execute client")),
            allowlist: match allowlist {
                Some(set) => AllowlistHot::ready("test-material.json".to_string(), set),
                None => AllowlistHot::unready(),
            },
            routes: None,
            caller_token,
        }
    }

    async fn spawn_provider(app: Router) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        addr
    }

    #[tokio::test]
    async fn llm_execute_stream_gate_401_and_unready_500() {
        // 令牌配置后缺凭据 → 401（与 /llm/execute 同门禁面）
        let st = stream_state(
            Some("http://127.0.0.1:1/v1/text/chatcompletion_v2"),
            Some(BTreeSet::from(["m1".to_string()])),
            Some("secret-token".into()),
        );
        let resp = llm_execute_stream(
            State(st),
            axum::http::HeaderMap::new(),
            Json(json!({"model": "m1", "messages": []})),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // 白名单未就绪 → 500 fail-closed 自诊断（不出站）
        let st = stream_state(None, None, None);
        let resp = llm_execute_stream(
            State(st),
            axum::http::HeaderMap::new(),
            Json(json!({"model": "m1", "messages": []})),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let v = body_json(resp).await;
        assert!(v["error"].as_str().is_some_and(|s| !s.is_empty()));
    }

    #[tokio::test]
    async fn llm_execute_stream_route_relays_sse_frames() {
        use tower::ServiceExt;
        // provider mock：reasoning/content/finish+usage 全型 SSE
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"思\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"答\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],",
            "\"usage\":{\"total_tokens\":3}}\n\n",
        );
        let app_provider = Router::new().route(
            "/v1/text/chatcompletion_v2",
            post(move || {
                let sse = sse.to_string();
                async move {
                    (
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        sse,
                    )
                }
            }),
        );
        let addr = spawn_provider(app_provider).await;
        let st = stream_state(
            Some(&format!("http://{addr}/v1/text/chatcompletion_v2")),
            Some(BTreeSet::from(["m1".to_string()])),
            None,
        );
        let router = Router::new()
            .route("/llm/execute", post(llm_execute))
            .route("/llm/execute-stream", post(llm_execute_stream))
            .with_state(st);
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/llm/execute-stream")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                json!({"model": "m1", "messages": [{"role": "user", "content": "hi"}]}).to_string(),
            ))
            .expect("request");
        let resp = router.oneshot(req).await.expect("oneshot");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/event-stream"),
            "SSE 头必须显式 text/event-stream"
        );
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.expect("body");
        let text = String::from_utf8(bytes.to_vec()).expect("utf8");
        // 逐帧下行：reasoning → content → done 五键 → [DONE] 哨兵
        let lines: Vec<&str> = text
            .split("\n\n")
            .filter(|f| f.starts_with("data: "))
            .collect();
        assert_eq!(
            lines.len(),
            4,
            "三归一帧 + done 帧 + [DONE]（共 4 data 行）"
        );
        let f0: Value = serde_json::from_str(lines[0].trim_start_matches("data: ")).unwrap();
        assert_eq!(f0["type"], "reasoning");
        assert_eq!(f0["text"], "思");
        let f1: Value = serde_json::from_str(lines[1].trim_start_matches("data: ")).unwrap();
        assert_eq!(f1["type"], "content");
        assert_eq!(f1["text"], "答");
        let f2: Value = serde_json::from_str(lines[2].trim_start_matches("data: ")).unwrap();
        assert_eq!(f2["type"], "done");
        assert_eq!(f2["content"], "答");
        assert_eq!(f2["reasoning"], "思");
        assert_eq!(f2["finish_reason"], "stop");
        assert_eq!(f2["token_usage"], json!({"total_tokens": 3}));
        assert_eq!(lines[3].trim(), "data: [DONE]");
    }

    // ---------- web_search 中继端点（批K 段2） ----------

    /// 构造带/不带 web_search 配置的 PluginState（env 闭包恒 None——测试
    /// 不受宿主环境变量影响；allowlist 恒 None，web_search 不消费它）
    fn ws_state(
        provider: Option<&str>,
        key: Option<&str>,
        caller_token: Option<String>,
    ) -> PluginState {
        let mut cfg = json!({
            "listen_addr": "127.0.0.1:0",
            "server_base_url": "http://127.0.0.1:18080",
            "llm_endpoint": "https://example.invalid/v1",
            "llm_api_key": "unused-in-test",
            "llm_model": "test-model",
            "tools_enabled": []
        });
        if let Some(p) = provider {
            cfg["web_search_provider"] = json!(p);
        }
        if let Some(k) = key {
            cfg["brave_api_key"] = json!(k);
        }
        let cfg =
            PluginConfig::from_value_with_env(&cfg, &|_| None).expect("test config must parse");
        PluginState {
            cfg: Arc::new(cfg),
            http: Arc::new(reqwest::Client::new()),
            execute_http: Arc::new(reqwest::Client::new()),
            allowlist: AllowlistHot::unready(),
            routes: None,
            caller_token,
        }
    }

    #[tokio::test]
    async fn web_search_requires_caller_token_when_configured() {
        // 无令牌 → 401（同 /llm/execute 面）
        let st = ws_state(Some("brave"), Some("k"), Some("secret-token".into()));
        let resp = web_search(
            State(st),
            axum::http::HeaderMap::new(),
            Json(json!({"query": "q"})),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // 令牌正确 → 通过校验进入业务面（provider 未配置 → 500 自诊断，
        // 不触网；同时验证令牌形态）
        let st = ws_state(None, None, Some("secret-token".into()));
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer secret-token"),
        );
        let resp = web_search(State(st), headers, Json(json!({"query": "q"}))).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn web_search_not_ready_self_diagnosis() {
        // 未启用（provider 未配置）→ 500 + 显式指引
        let st = ws_state(None, None, None);
        let resp = web_search(
            State(st),
            axum::http::HeaderMap::new(),
            Json(json!({"query": "q"})),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let v = body_json(resp).await;
        assert!(
            v["error"]
                .as_str()
                .is_some_and(|s| s.contains("web_search 未启用")),
            "{v}"
        );
        // 启用但未配 key → 500 + 提及环境变量名
        let st = ws_state(Some("brave"), None, None);
        let resp = web_search(
            State(st),
            axum::http::HeaderMap::new(),
            Json(json!({"query": "q"})),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let v = body_json(resp).await;
        assert!(
            v["error"]
                .as_str()
                .is_some_and(|s| s.contains("EVORULE_AI_PLUGIN_BRAVE_API_KEY")),
            "{v}"
        );
    }

    #[tokio::test]
    async fn web_search_rejects_bad_body() {
        // query 缺失 / 空白 → 400
        for bad in [json!({}), json!({"query": "   "})] {
            let st = ws_state(Some("brave"), Some("k"), None);
            let resp = web_search(State(st), axum::http::HeaderMap::new(), Json(bad)).await;
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            let v = body_json(resp).await;
            assert!(
                v["error"].as_str().is_some_and(|s| s.contains("query")),
                "{v}"
            );
        }
        // count 非法（0 / 非数字）→ 400
        for bad in [
            json!({"query": "q", "count": 0}),
            json!({"query": "q", "count": "x"}),
        ] {
            let st = ws_state(Some("brave"), Some("k"), None);
            let resp = web_search(State(st), axum::http::HeaderMap::new(), Json(bad)).await;
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            let v = body_json(resp).await;
            assert!(
                v["error"].as_str().is_some_and(|s| s.contains("count")),
                "{v}"
            );
        }
        // count 超帽 → 静默钳制进业务面（未启用配置下表现为 500 自诊断，非 400）
        let st = ws_state(None, None, None);
        let resp = web_search(
            State(st),
            axum::http::HeaderMap::new(),
            Json(json!({"query": "q", "count": 99})),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}

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
    build_http_client, build_llm_execute_client, call_llm_ext, load_model_allowlist_file,
    run_audited_chat, web_search_relay, ChatRequest, PluginConfig, PluginError, WebSearchError,
    WEB_SEARCH_DEFAULT_COUNT, WEB_SEARCH_MAX_COUNT,
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
    /// 模型准入白名单（None = 扩展执行体未就绪，/llm/execute 显式拒绝）
    allowlist: Arc<Option<BTreeSet<String>>>,
    /// 调用方令牌（Some = /llm/execute 校验 Authorization;None = 不鉴权）
    caller_token: Option<String>,
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
    // 扩展执行体就绪判定（SG-1 件1）：llm_execute_url 配置即要求白名单物料
    // 可加载——加载失败仅降级为「/llm/execute 全拒」（fail-closed，自诊断
    // 留痕），不阻断既有 /services/ai_plugin_chat 通道启动。
    let allowlist = if cfg.llm_execute_url.is_some() {
        match cfg.model_allowlist_path.as_deref() {
            None => {
                tracing::warn!(
                    "llm_execute_url 已配置但 model_allowlist_path 缺失: /llm/execute 将以\
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
    let caller_token = cfg.caller_token.clone();
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
        allowlist: Arc::new(allowlist),
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
    runtime.block_on(async move {
        let app = Router::new()
            .route("/services/ai_plugin_chat", post(ai_plugin_chat))
            .route("/llm/execute", post(llm_execute))
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

/// 扩展型执行体端点（SG-1 件1）：body 原样 → 四键。
/// 未就绪（未配置/白名单物料未装载）→ 500 自诊断（fail-closed，绝不静默放行）；
/// 白名单违规/provider 失败 → 502 显式上抛（fail-visible，无静默兜底）。
async fn llm_execute(
    State(st): State<PluginState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    // O 系安全登记:调用方令牌校验(配置后启用)——本机其他进程无令牌即 401,
    // 防烧用户配额;浏览器侧被 JSON preflight 天然阻断。
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
    let Some(allowlist) = st.allowlist.as_ref() else {
        let diag = if st.cfg.llm_execute_url.is_some() {
            "扩展执行体未就绪: model_allowlist_path 未配置或白名单物料装载失败\
             （见启动日志；物料 = 00_constraint_model_allowlist.json server 侧副本）"
                .to_string()
        } else {
            "扩展执行体未就绪: 配置键 llm_execute_url / model_allowlist_path 未填写\
             （provider 全 URL 形态，如 https://api.minimaxi.com/v1/text/chatcompletion_v2）"
                .to_string()
        };
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": diag })),
        )
            .into_response();
    };
    match call_llm_ext(&st.cfg, &st.execute_http, allowlist, &body).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => {
            tracing::warn!("llm/execute 失败: {e}");
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
    match st.allowlist.as_ref() {
        None => Json(json!({
            "ready": false,
            "models": [],
            "note": "扩展执行体未就绪: 白名单物料未配置或装载失败（见启动日志）"
        }))
        .into_response(),
        Some(set) => {
            let models: Vec<&String> = set.iter().collect();
            Json(json!({ "ready": true, "models": models })).into_response()
        }
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
    // 调用方令牌校验（O 系安全登记同款：配置后启用）
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
            allowlist: Arc::new(allowlist),
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
            allowlist: Arc::new(None),
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

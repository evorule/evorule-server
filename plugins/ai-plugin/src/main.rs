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
//!   GET  /health      存活探针
//!
//! 红线：本进程内 LLM 出站仅两条受管辖路径——①call_external 审计回路内的
//! `call_llm`（sidecar IoRequest 分支专用，见 lib.rs）；②扩展执行体
//! `call_llm_ext`（POST /llm/execute，模型白名单网络入口执法，prompt 已由
//! agent 侧命令事实先行入链）。其余任何直连禁止；凭据不进日志/错误消息。
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
    run_audited_chat, ChatRequest, PluginConfig, PluginError,
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
async fn llm_execute(State(st): State<PluginState>, Json(body): Json<Value>) -> Response {
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

async fn ai_plugin_chat(State(st): State<PluginState>, Json(body): Json<Value>) -> Response {
    // 请求体校验：消费方错误 → 400
    let req = match ChatRequest::from_value(&body) {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": e.to_string() }))).into_response(),
    };
    let purpose = req.audit_purpose.clone().unwrap_or_else(|| "chat".to_string());
    match run_audited_chat(&st.cfg, &st.http, &req).await {
        Ok(outcome) => {
            tracing::info!(
                "ai_plugin_chat 完成: session={} purpose={purpose} reply_chars={}",
                outcome.session_id,
                outcome.reply.chars().count()
            );
            Json(json!({ "reply": outcome.reply, "session_id": outcome.session_id })).into_response()
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

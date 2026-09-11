// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! ai-plugin 编译核 —— 服务端驻留 LLM 执行者（契约 v1.2；UV-172 L1 地基）。
//!
//! 自编排审计回路：服务端点收到调用后自建一次性 sidecar 审计会话，走
//! `call_external → LLM(服务端托管凭据) → io_response` 完整回路再返回。
//! prompt/结果全文照旧入审计链——与浏览器审计桥（console-cloud
//! `audited-llm.ts`，对齐 evo-agent `audited_llm.rs` 协议契约）同协议位。
//!
//! 红线（插件红线清单条款，立项 05 文档 §2.1）：
//!   - 本 crate 内 `call_llm` 是唯一 LLM 路径，且只允许在 sidecar 循环的
//!     IoRequest 处理分支内被调用（防 UV-057 同型影子调用）；
//!   - LLM 执行失败也要回写错误 io_response（引擎状态机收尾，不留悬空
//!     IoRequest），再向消费方显式报错，无静默兜底；
//!   - 凭据（llm_api_key）不进日志/不进错误消息/不进 URL。
//!
//! 协议要点（与 audited-llm.ts 对齐）：
//!   - 必须先订阅 SSE 再提交命令（broadcast 通道不重放历史）；
//!   - 事件形态：`IoRequest`（带 `id`）/`Stable`/`Error`，data 行 JSON；
//!   - `executor:"server"` 提示随命令 params 入审计链（通道协调位，
//!     可审计、防双应答竞争；缺省浏览器桥认领）。

use futures_util::StreamExt;
use reqwest::header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::{json, Value};
use std::fmt;
use std::time::Duration;

/// 每个等待点的超时（毫秒）：作用于单个 HTTP 请求 / 下一个 SSE 事件，
/// 非全周期硬上限（对齐浏览器桥 SIDECAR_WAIT_TIMEOUT_MS 语义）。
pub const SIDECAR_WAIT_TIMEOUT_MS: u64 = 90_000;

/// LLM 请求超时缺省（毫秒）
pub const LLM_TIMEOUT_DEFAULT_MS: u64 = 60_000;

#[derive(Debug)]
pub enum PluginError {
    /// 配置非法/缺失（fail-fast；不含凭据内容）
    Config(String),
    /// evorule-server 不可达
    ServerUnreachable(String),
    /// sidecar 协议失败（含 SSE 流异常/解析失败）
    Protocol(String),
    /// 引擎报 Error 事件
    Engine(String),
    /// LLM 执行失败（错误已先回写 io_response）
    Llm(String),
}

impl fmt::Display for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PluginError::Config(m) => write!(f, "配置错误: {m}"),
            PluginError::ServerUnreachable(m) => write!(f, "evorule-server 不可达: {m}"),
            PluginError::Protocol(m) => write!(f, "sidecar 协议失败: {m}"),
            PluginError::Engine(m) => write!(f, "evorule 引擎错误: {m}"),
            PluginError::Llm(m) => write!(f, "LLM 执行失败: {m}"),
        }
    }
}

impl std::error::Error for PluginError {}

/// 插件配置（ai-plugin.json）。
///
/// 凭据安全：`llm_api_key` 只存在于本结构，序列化/日志/错误消息均不得携带。
#[derive(Debug, Clone)]
pub struct PluginConfig {
    /// 服务监听地址
    pub listen_addr: String,
    /// evorule-server 根地址（sidecar 回路的对端）
    pub server_base_url: String,
    /// server Bearer 凭据（server 开启认证时必填；否则 None）
    pub server_auth_token: Option<String>,
    /// LLM API 根地址（OpenAI 兼容，/chat/completions 自动拼接）
    pub llm_endpoint: String,
    pub llm_api_key: String,
    pub llm_model: String,
    /// 缺省采样温度（调用方可按次覆盖）
    pub llm_temperature: f64,
    /// LLM 请求超时（毫秒）
    pub llm_timeout_ms: u64,
}

fn require_str(v: &Value, key: &str) -> Result<String, PluginError> {
    let s = v
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if s.is_empty() {
        return Err(PluginError::Config(format!(
            "缺少必填字段 '{key}'（自诊断指引: 复制 config.example.json 为 ai-plugin.json 并填写; \
             字段缺省语义见插件 README）"
        )));
    }
    Ok(s.to_string())
}

fn require_http(s: &str, key: &str) -> Result<(), PluginError> {
    if s.starts_with("http://") || s.starts_with("https://") {
        Ok(())
    } else {
        Err(PluginError::Config(format!(
            "字段 '{key}' 仅支持 http/https 地址: 已提供的前缀非法"
        )))
    }
}

impl PluginConfig {
    /// 从 JSON 解析；缺必填字段/非法值 fail-fast（附自诊断指引，不静默补省）。
    pub fn from_value(v: &Value) -> Result<Self, PluginError> {
        let server_base_url = require_str(v, "server_base_url")?;
        require_http(&server_base_url, "server_base_url")?;
        let llm_endpoint = require_str(v, "llm_endpoint")?;
        require_http(&llm_endpoint, "llm_endpoint")?;
        let llm_api_key = require_str(v, "llm_api_key")?;
        let llm_model = require_str(v, "llm_model")?;
        let listen_addr = v
            .get("listen_addr")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("127.0.0.1:9130")
            .to_string();
        let server_auth_token = v
            .get("server_auth_token")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let llm_temperature = match v.get("llm_temperature") {
            None => 0.2,
            Some(Value::Number(n)) => {
                let t = n.as_f64().unwrap_or(0.2);
                if !(-2.0..=2.0).contains(&t) {
                    return Err(PluginError::Config(
                        "字段 'llm_temperature' 须在 [-2.0, 2.0] 区间".to_string(),
                    ));
                }
                t
            }
            Some(_) => {
                return Err(PluginError::Config(
                    "字段 'llm_temperature' 须为数字".to_string(),
                ))
            }
        };
        let llm_timeout_ms = match v.get("llm_timeout_ms") {
            None => LLM_TIMEOUT_DEFAULT_MS,
            Some(Value::Number(n)) => {
                let t = n.as_u64().ok_or_else(|| {
                    PluginError::Config("字段 'llm_timeout_ms' 须为正整数毫秒".to_string())
                })?;
                if !(1000..=300_000).contains(&t) {
                    return Err(PluginError::Config(
                        "字段 'llm_timeout_ms' 须在 [1000, 300000] 区间".to_string(),
                    ));
                }
                t
            }
            Some(_) => {
                return Err(PluginError::Config(
                    "字段 'llm_timeout_ms' 须为正整数毫秒".to_string(),
                ))
            }
        };
        Ok(Self {
            listen_addr: listen_addr.to_string(),
            server_base_url,
            server_auth_token,
            llm_endpoint,
            llm_api_key,
            llm_model,
            llm_temperature,
            llm_timeout_ms,
        })
    }

    pub fn from_json_str(s: &str) -> Result<Self, PluginError> {
        let v: Value = serde_json::from_str(s)
            .map_err(|e| PluginError::Config(format!("配置 JSON 非法: {e}")))?;
        Self::from_value(&v)
    }
}

/// 服务端点请求体：`{messages, model?, temperature?, audit_purpose?}`。
#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// OpenAI 兼容消息数组（[{role, content}, …]）
    pub messages: Vec<Value>,
    /// 覆盖配置缺省模型
    pub model: Option<String>,
    /// 覆盖配置缺省温度
    pub temperature: Option<f64>,
    /// 审计用途标签（随命令事实入链；缺省 "chat"）
    pub audit_purpose: Option<String>,
}

impl ChatRequest {
    pub fn from_value(v: &Value) -> Result<Self, PluginError> {
        let Some(msgs) = v.get("messages").and_then(Value::as_array) else {
            return Err(PluginError::Config(
                "缺少必填字段 'messages'（OpenAI 兼容消息数组）".to_string(),
            ));
        };
        if msgs.is_empty() {
            return Err(PluginError::Config("字段 'messages' 不能为空".to_string()));
        }
        for (i, m) in msgs.iter().enumerate() {
            let ok = m.get("role").and_then(Value::as_str).is_some()
                && m.get("content").and_then(Value::as_str).is_some();
            if !ok {
                return Err(PluginError::Config(format!(
                    "messages[{i}] 非法（须为 {{role, content}} 对象）"
                )));
            }
        }
        let model = v
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let temperature = match v.get("temperature") {
            None => None,
            Some(Value::Number(n)) => Some(n.as_f64().unwrap_or(0.2)),
            Some(_) => {
                return Err(PluginError::Config(
                    "字段 'temperature' 须为数字".to_string(),
                ))
            }
        };
        let audit_purpose = v
            .get("audit_purpose")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        Ok(Self {
            messages: msgs.clone(),
            model,
            temperature,
            audit_purpose,
        })
    }
}

/// SSE 事件（server 侧事实 JSON；与 audited-llm.ts SidecarEvent 对齐）
#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    pub event_type: String,
    pub id: Option<i64>,
    pub message: Option<String>,
}

/// 从缓冲提取下一个完整 SSE 事件（`data:` 行 JSON）；注释/心跳块内部跳过；
/// 事件不完整返回 None 留待续读；data JSON 非法 = 协议错误。
pub fn next_sse_event(buffer: &mut String) -> Result<Option<SseEvent>, PluginError> {
    loop {
        let Some(idx) = buffer.find("\n\n") else {
            return Ok(None); // 无完整块，留待续读
        };
        let raw: String = buffer.drain(..idx + 2).collect();
        for line in raw.lines() {
            let Some(rest) = line.strip_prefix("data:") else {
                continue; // 注释/心跳行
            };
            let json_part = rest.trim();
            if json_part.is_empty() {
                continue;
            }
            let v: Value = serde_json::from_str(json_part).map_err(|e| {
                PluginError::Protocol(format!("SSE 事件 JSON 解析失败: {e}"))
            })?;
            let event_type = v
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let id = v.get("id").and_then(Value::as_i64);
            let message = v
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string);
            return Ok(Some(SseEvent {
                event_type,
                id,
                message,
            }));
        }
        // 本块只有注释/心跳 → 继续扫描下一块（缓冲中可能已有完整 data 事件，
        // 提前返回 None 会导致在等流上超时）
    }
}

/// HTTP 客户端（无全局超时：各等待点独立限时，SSE 长流不受限）
pub fn build_http_client() -> Result<reqwest::Client, PluginError> {
    reqwest::Client::builder()
        .build()
        .map_err(|e| PluginError::Config(format!("HTTP 客户端构建失败: {e}")))
}

fn auth_headers(cfg: &PluginConfig) -> Result<reqwest::header::HeaderMap, PluginError> {
    let mut h = reqwest::header::HeaderMap::new();
    h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Some(tok) = cfg.server_auth_token.as_deref() {
        let val = HeaderValue::from_str(&format!("Bearer {tok}"))
            .map_err(|_| PluginError::Config("server_auth_token 含非法头字符".to_string()))?;
        h.insert(AUTHORIZATION, val);
    }
    Ok(h)
}

async fn fetch_step(
    http: &reqwest::Client,
    method: reqwest::Method,
    url: String,
    headers: reqwest::header::HeaderMap,
    body: Option<Value>,
    step: &'static str,
) -> Result<reqwest::Response, PluginError> {
    let mut req = http.request(method, &url).headers(headers);
    if let Some(b) = body {
        req = req.json(&b);
    }
    let fut = req.timeout(Duration::from_millis(SIDECAR_WAIT_TIMEOUT_MS)).send();
    match tokio::time::timeout(Duration::from_millis(SIDECAR_WAIT_TIMEOUT_MS), fut).await {
        Err(_) => Err(PluginError::Protocol(format!(
            "审计桥超时({step},{SIDECAR_WAIT_TIMEOUT_MS}ms)"
        ))),
        Ok(Err(e)) => Err(PluginError::ServerUnreachable(format!(
            "无法连接 evorule-server(审计桥 {step}): {url} ({e})"
        ))),
        Ok(Ok(resp)) => Ok(resp),
    }
}

async fn assert_ok(
    resp: reqwest::Response,
    step: &'static str,
    session_id: &str,
) -> Result<reqwest::Response, PluginError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    let snippet: String = body.chars().take(200).collect();
    Err(PluginError::Protocol(format!(
        "evorule-server {step} 失败(HTTP {status})[session {session_id}]: {snippet}"
    )))
}

/// LLM 调用 —— 本 crate 唯一 LLM 路径。
///
/// 红线：只允许在 [`run_audited_chat`] 的 IoRequest 分支内被调用（影子调用禁令）。
/// 失败分类：网络/超时/HTTP 状态/JSON/结构；错误消息不含 key。
pub async fn call_llm(
    cfg: &PluginConfig,
    http: &reqwest::Client,
    messages: &[Value],
    model: &str,
    temperature: f64,
) -> Result<String, PluginError> {
    let url = format!("{}/chat/completions", cfg.llm_endpoint.trim_end_matches('/'));
    let body = json!({ "model": model, "messages": messages, "temperature": temperature });
    let resp = http
        .post(&url)
        .bearer_auth(&cfg.llm_api_key)
        .json(&body)
        .timeout(Duration::from_millis(cfg.llm_timeout_ms))
        .send()
        .await
        .map_err(|e| PluginError::Llm(format!("LLM 请求失败({url}): {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        let snippet: String = text.chars().take(200).collect();
        let kind = match status.as_u16() {
            401 => "鉴权失败(apiKey 无效)",
            429 => "限流(429)",
            _ => "上游错误",
        };
        return Err(PluginError::Llm(format!(
            "LLM HTTP {status}({kind}): {snippet}"
        )));
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| PluginError::Llm(format!("LLM 响应 JSON 解析失败: {e}")))?;
    let content = v
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            PluginError::Llm("LLM 响应结构异常（无 choices[0].message.content）".to_string())
        })?;
    Ok(content.to_string())
}

/// 自编排审计回路结果：回复文本 + sidecar 会话 id（供审计对账：
/// `GET /api/sessions/{id}/audit` 可回放该次执行的 prompt/io_response 事实链）。
#[derive(Debug, Clone)]
pub struct AuditedChatOutcome {
    pub reply: String,
    pub session_id: i64,
}

/// 自编排审计回路：一次性 sidecar 会话内完成
/// `call_external(+executor:"server") → LLM → io_response → Stable`。
///
/// 失败时保证：LLM 错误已先回写错误 io_response（不留悬空 IoRequest）再上抛；
/// 会话尽力关闭（不掩盖主流程结果）。
pub async fn run_audited_chat(
    cfg: &PluginConfig,
    http: &reqwest::Client,
    req: &ChatRequest,
) -> Result<AuditedChatOutcome, PluginError> {
    let base = cfg.server_base_url.trim_end_matches('/').to_string();
    let headers = auth_headers(cfg)?;

    // 1. 一次性 sidecar 会话
    let created = fetch_step(
        http,
        reqwest::Method::POST,
        format!("{base}/api/sessions"),
        headers.clone(),
        Some(json!({})),
        "create_session",
    )
    .await?;
    let created = assert_ok(created, "create_session", "-").await?;
    let created: Value = created
        .json()
        .await
        .map_err(|e| PluginError::Protocol(format!("create_session 响应解析失败: {e}")))?;
    let session_id = created
        .get("session_id")
        .and_then(Value::as_i64)
        .ok_or_else(|| PluginError::Protocol("create_session 缺少 session_id".to_string()))?;
    let sid = session_id.to_string();

    // 2. 先订阅再提交命令（broadcast 通道不重放历史）
    let outcome = run_loop(cfg, http, req, &base, &sid, headers.clone()).await;

    // 3. 尽力关闭 sidecar 会话（防会话数耗尽；失败不掩盖主流程）
    let _ = fetch_step(
        http,
        reqwest::Method::DELETE,
        format!("{base}/api/sessions/{sid}"),
        headers,
        None,
        "close_session",
    )
    .await;

    outcome.map(|reply| AuditedChatOutcome { reply, session_id })
}

async fn run_loop(
    cfg: &PluginConfig,
    http: &reqwest::Client,
    req: &ChatRequest,
    base: &str,
    sid: &str,
    headers: reqwest::header::HeaderMap,
) -> Result<String, PluginError> {
    let stream_resp = fetch_step(
        http,
        reqwest::Method::GET,
        format!("{base}/api/sessions/{sid}/events"),
        headers.clone(),
        None,
        "subscribe_events",
    )
    .await?;
    let stream_resp = assert_ok(stream_resp, "subscribe_events", sid).await?;
    let mut stream = stream_resp.bytes_stream();

    // 3. 提交 call_external 命令 —— prompt 全文(messages)入审计链；
    //    executor:"server" 通道协调位随命令事实入链（可审计、防双应答）
    let params = json!({
        "model": req.model.clone().unwrap_or_else(|| cfg.llm_model.clone()),
        "temperature": req.temperature.unwrap_or(cfg.llm_temperature),
        "messages": req.messages,
        "audit_purpose": req.audit_purpose.clone().unwrap_or_else(|| "chat".to_string()),
        "executor": "server"
    });
    let submitted = fetch_step(
        http,
        reqwest::Method::POST,
        format!("{base}/api/sessions/{sid}/command"),
        headers.clone(),
        Some(json!({ "instruction": { "type": "call_external", "params": params } })),
        "submit_command",
    )
    .await?;
    assert_ok(submitted, "submit_command", sid).await?;

    // 4. 事件回路：IoRequest → LLM 执行 → io_response；Stable → 完成
    let mut buffer = String::new();
    let mut reply: Option<String> = None;
    loop {
        let ev = loop {
            if let Some(ev) = next_sse_event(&mut buffer)? {
                break ev;
            }
            let next = tokio::time::timeout(
                Duration::from_millis(SIDECAR_WAIT_TIMEOUT_MS),
                stream.next(),
            )
            .await;
            match next {
                Err(_) => {
                    return Err(PluginError::Protocol(format!(
                        "等待 SSE 事件超时({SIDECAR_WAIT_TIMEOUT_MS}ms)[session {sid}]"
                    )))
                }
                Ok(None) => {
                    return Err(PluginError::Protocol(format!(
                        "SSE 流在 Stable 前关闭(审计回路未完成)[session {sid}]"
                    )))
                }
                Ok(Some(Err(e))) => {
                    return Err(PluginError::Protocol(format!(
                        "SSE 流读取失败[session {sid}]: {e}"
                    )))
                }
                Ok(Some(Ok(bytes))) => {
                    buffer.push_str(&String::from_utf8_lossy(&bytes));
                }
            }
        };
        match ev.event_type.as_str() {
            "IoRequest" => {
                let Some(request_id) = ev.id else {
                    return Err(PluginError::Protocol(format!(
                        "IoRequest 事件缺少 id[session {sid}]"
                    )));
                };
                // LLM 执行；失败也要把错误写进 io_response 再抛（不留悬空 IoRequest）
                let call = call_llm(
                    cfg,
                    http,
                    &req.messages,
                    &req.model.clone().unwrap_or_else(|| cfg.llm_model.clone()),
                    req.temperature.unwrap_or(cfg.llm_temperature),
                )
                .await;
                match call {
                    Ok(text) => {
                        post_io_response(
                            http,
                            base,
                            sid,
                            headers.clone(),
                            request_id,
                            json!({ "content": text.clone() }),
                            None,
                        )
                        .await?;
                        reply = Some(text);
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        let wrote = post_io_response(
                            http,
                            base,
                            sid,
                            headers.clone(),
                            request_id,
                            json!({ "error": msg.clone() }),
                            Some(msg.clone()),
                        )
                        .await;
                        if let Err(w) = wrote {
                            // 回写失败不掩盖原始 LLM 错误，但须如实留痕
                            tracing::warn!("错误 io_response 回写失败(原始 LLM 错误原样上抛): {w}");
                        }
                        return Err(e);
                    }
                }
            }
            "Stable" => {
                return reply.ok_or_else(|| {
                    PluginError::Protocol(format!(
                        "Stable 到达但未执行 LLM(审计回路异常)[session {sid}]"
                    ))
                });
            }
            "Error" => {
                let msg = ev
                    .message
                    .unwrap_or_else(|| "evorule 引擎报 Error 事件".to_string());
                return Err(PluginError::Engine(msg));
            }
            _ => {} // StateTransition 等其他事件忽略
        }
    }
}

async fn post_io_response(
    http: &reqwest::Client,
    base: &str,
    sid: &str,
    headers: reqwest::header::HeaderMap,
    request_id: i64,
    result: Value,
    error: Option<String>,
) -> Result<(), PluginError> {
    let resp = fetch_step(
        http,
        reqwest::Method::POST,
        format!("{base}/api/sessions/{sid}/io_response"),
        headers,
        Some(json!({ "request_id": request_id, "result": result, "error": error })),
        "submit_io_response",
    )
    .await?;
    assert_ok(resp, "submit_io_response", sid).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    // 测试代码允许 unwrap/expect/panic（flow-studio 同先例：生产路径仍全量 deny）
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use axum::extract::State;
    use axum::response::{IntoResponse, Response};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    // ---------- 配置解析 ----------

    #[test]
    fn config_parse_ok_with_defaults() {
        let cfg = PluginConfig::from_json_str(
            r#"{"server_base_url":"http://127.0.0.1:18080",
                "llm_endpoint":"https://api.example.com/v1",
                "llm_api_key":"sk-test","llm_model":"m1"}"#,
        )
        .unwrap();
        assert_eq!(cfg.listen_addr, "127.0.0.1:9130");
        assert_eq!(cfg.llm_temperature, 0.2);
        assert_eq!(cfg.llm_timeout_ms, LLM_TIMEOUT_DEFAULT_MS);
        assert!(cfg.server_auth_token.is_none());
    }

    #[test]
    fn config_missing_required_fails_fast() {
        let err = PluginConfig::from_json_str(r#"{"llm_endpoint":"http://x"}"#).unwrap_err();
        assert!(err.to_string().contains("缺少必填字段 'server_base_url'"));
        let err = PluginConfig::from_json_str(
            r#"{"server_base_url":"http://x","llm_endpoint":"http://x","llm_model":"m"}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("缺少必填字段 'llm_api_key'"));
    }

    #[test]
    fn config_bad_scheme_rejected() {
        let err = PluginConfig::from_json_str(
            r#"{"server_base_url":"ftp://x","llm_endpoint":"http://x",
                "llm_api_key":"k","llm_model":"m"}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("仅支持 http/https"));
    }

    #[test]
    fn config_auth_token_and_ranges() {
        let cfg = PluginConfig::from_json_str(
            r#"{"server_base_url":"http://x","server_auth_token":"tok",
                "llm_endpoint":"http://x","llm_api_key":"k","llm_model":"m",
                "llm_temperature":1.5,"llm_timeout_ms":120000}"#,
        )
        .unwrap();
        assert_eq!(cfg.server_auth_token.as_deref(), Some("tok"));
        assert_eq!(cfg.llm_temperature, 1.5);
        assert_eq!(cfg.llm_timeout_ms, 120000);
        // 空白 token 视为未配置
        let cfg2 = PluginConfig::from_json_str(
            r#"{"server_base_url":"http://x","server_auth_token":"  ",
                "llm_endpoint":"http://x","llm_api_key":"k","llm_model":"m"}"#,
        )
        .unwrap();
        assert!(cfg2.server_auth_token.is_none());
    }

    // ---------- 请求体解析 ----------

    #[test]
    fn chat_request_parse() {
        let ok = ChatRequest::from_value(&json!({
            "messages": [{"role":"user","content":"hi"}],
            "model":"m2","temperature":0.5,"audit_purpose":"draft_rule"
        }))
        .unwrap();
        assert_eq!(ok.messages.len(), 1);
        assert_eq!(ok.model.as_deref(), Some("m2"));
        assert_eq!(ok.temperature, Some(0.5));
        assert_eq!(ok.audit_purpose.as_deref(), Some("draft_rule"));
        assert!(ChatRequest::from_value(&json!({})).is_err());
        assert!(ChatRequest::from_value(&json!({"messages":[]})).is_err());
        assert!(
            ChatRequest::from_value(&json!({"messages":[{"role":"user"}]})).is_err(),
            "缺 content 的消息应拒绝"
        );
    }

    // ---------- SSE 解析 ----------

    #[test]
    fn sse_parser_extracts_complete_events() {
        let mut buf = String::from(": ping\n\ndata: {\"type\":\"IoRequest\",\"id\":7}\n\ndata: {\"type\":\"Stable\"}\n\n");
        let e1 = next_sse_event(&mut buf).unwrap().unwrap();
        assert_eq!(e1.event_type, "IoRequest");
        assert_eq!(e1.id, Some(7));
        let e2 = next_sse_event(&mut buf).unwrap().unwrap();
        assert_eq!(e2.event_type, "Stable");
        assert!(next_sse_event(&mut buf).unwrap().is_none(), "耗尽后 None");
    }

    #[test]
    fn sse_parser_holds_incomplete_and_rejects_bad_json() {
        let mut buf = String::from("data: {\"type\":\"Io");
        assert!(next_sse_event(&mut buf).unwrap().is_none());
        buf.push_str("Req\",\"id\":3}\n\n");
        let e = next_sse_event(&mut buf).unwrap().unwrap();
        assert_eq!(e.id, Some(3));
        let mut bad = String::from("data: not-json\n\n");
        assert!(next_sse_event(&mut bad).is_err());
    }

    // ---------- mock server（sidecar 全回路 e2e） ----------

    #[derive(Clone)]
    struct MockState {
        command: Arc<Mutex<Option<Value>>>,
        io_responses: Arc<Mutex<Vec<Value>>>,
        closed: Arc<Mutex<bool>>,
        llm_ok: Arc<AtomicBool>,
        command_submitted: Arc<tokio::sync::Notify>,
        io_done: Arc<tokio::sync::Notify>,
    }

    impl MockState {
        fn new(llm_ok: bool) -> Self {
            Self {
                command: Arc::new(Mutex::new(None)),
                io_responses: Arc::new(Mutex::new(Vec::new())),
                closed: Arc::new(Mutex::new(false)),
                llm_ok: Arc::new(AtomicBool::new(llm_ok)),
                command_submitted: Arc::new(tokio::sync::Notify::new()),
                io_done: Arc::new(tokio::sync::Notify::new()),
            }
        }
    }

    async fn mock_create() -> Json<Value> {
        Json(json!({ "session_id": 1 }))
    }

    async fn mock_command(
        State(st): State<MockState>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        *st.command.lock().unwrap_or_else(|e| e.into_inner()) = Some(body);
        st.command_submitted.notify_one();
        Json(json!({ "ok": true }))
    }

    async fn mock_io_response(
        State(st): State<MockState>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        st.io_responses
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(body);
        st.io_done.notify_one();
        Json(json!({ "ok": true }))
    }

    async fn mock_close(State(st): State<MockState>) -> Json<Value> {
        *st.closed.lock().unwrap_or_else(|e| e.into_inner()) = true;
        Json(json!({ "ok": true }))
    }

    async fn mock_llm(State(st): State<MockState>) -> Response {
        if st.llm_ok.load(Ordering::SeqCst) {
            Json(json!({"choices":[{"message":{"content":"mock reply"}}]})).into_response()
        } else {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":"boom"})),
            )
                .into_response()
        }
    }

    async fn mock_events(State(st): State<MockState>) -> Response {
        // 先等命令提交再发 IoRequest（先订阅后命令的协议顺序由消费端保证，
        // mock 用通知模拟 server 事实广播时序）；io_response 后发 Stable。
        let st2 = st.clone();
        let stream = futures_util::stream::unfold(0u8, move |phase| {
            let st = st2.clone();
            async move {
                match phase {
                    0 => {
                        st.command_submitted.notified().await;
                        Some((
                            Ok::<_, std::convert::Infallible>(format!(
                                "data: {}\n\n",
                                json!({"type":"IoRequest","id":7})
                            )),
                            1u8,
                        ))
                    }
                    1 => {
                        st.io_done.notified().await;
                        Some((
                            Ok::<_, std::convert::Infallible>(format!(
                                "data: {}\n\n",
                                json!({"type":"Stable"})
                            )),
                            2u8,
                        ))
                    }
                    _ => None,
                }
            }
        });
        (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            axum::body::Body::from_stream(stream),
        )
            .into_response()
    }

    async fn spawn_mock(st: MockState) -> String {
        let app = Router::new()
            .route("/api/sessions", post(mock_create))
            .route("/api/sessions/{id}/events", get(mock_events))
            .route("/api/sessions/{id}/command", post(mock_command))
            .route("/api/sessions/{id}/io_response", post(mock_io_response))
            .route("/api/sessions/{id}", axum::routing::delete(mock_close))
            .route("/llm/chat/completions", post(mock_llm))
            .with_state(st);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    fn test_request() -> ChatRequest {
        ChatRequest::from_value(&json!({
            "messages": [{"role":"user","content":"写一条阈值规则"}]
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn audited_loop_happy_path_records_command_and_io_response() {
        let st = MockState::new(true);
        let base = spawn_mock(st.clone()).await;
        let cfg = PluginConfig::from_json_str(&format!(
            r#"{{"server_base_url":"{base}","llm_endpoint":"{base}/llm",
                "llm_api_key":"sk-mock","llm_model":"m1"}}"#
        ))
        .unwrap();
        let http = build_http_client().unwrap();
        let outcome = run_audited_chat(&cfg, &http, &test_request()).await.unwrap();
        assert_eq!(outcome.reply, "mock reply");
        assert_eq!(outcome.session_id, 1);

        // 命令事实：call_external + messages(prompt 全文) + executor 提示位
        let cmd = st.command.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let cmd = cmd.expect("命令应被记录");
        assert_eq!(cmd["instruction"]["type"], "call_external");
        assert_eq!(cmd["instruction"]["params"]["executor"], "server");
        assert_eq!(
            cmd["instruction"]["params"]["messages"][0]["content"],
            "写一条阈值规则"
        );
        assert_eq!(cmd["instruction"]["params"]["audit_purpose"], "chat");

        // io_response 事实：结果全文入链
        let ios = st.io_responses.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(ios.len(), 1);
        assert_eq!(ios[0]["request_id"], 7);
        assert_eq!(ios[0]["result"]["content"], "mock reply");
        assert!(ios[0]["error"].is_null());

        // 会话收尾
        assert!(*st.closed.lock().unwrap_or_else(|e| e.into_inner()));
    }

    #[tokio::test]
    async fn llm_failure_writes_error_io_response_then_errors() {
        let st = MockState::new(false);
        let base = spawn_mock(st.clone()).await;
        let cfg = PluginConfig::from_json_str(&format!(
            r#"{{"server_base_url":"{base}","llm_endpoint":"{base}/llm",
                "llm_api_key":"sk-mock","llm_model":"m1"}}"#
        ))
        .unwrap();
        let http = build_http_client().unwrap();
        let err = run_audited_chat(&cfg, &http, &test_request())
            .await
            .unwrap_err();
        assert!(matches!(err, PluginError::Llm(_)), "{err}");

        // 错误也要回写 io_response（不留悬空 IoRequest）
        let ios = st.io_responses.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(ios.len(), 1);
        assert_eq!(ios[0]["request_id"], 7);
        assert!(ios[0]["result"]["error"].as_str().is_some());
        assert_eq!(ios[0]["error"], ios[0]["result"]["error"]);
    }
}

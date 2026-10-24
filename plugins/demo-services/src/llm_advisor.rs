// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `llm_advisor` 原生实现 —— 真实 LLM 出网 + 离线确定性 mock 双路径。
//!
//! # 两条路径（返回体恒带溯源位，替身绝不冒充真身）
//!
//! | 形态 | 触发条件 | `provider` | `real_llm` |
//! |---|---|---|---|
//! | 出网 | 配置 `EVORULE_LLM_ADVISOR_ENDPOINT` | `http` | `true` |
//! | 离线 | 未配置端点 | `offline_mock` | `false` |
//!
//! # fail-visible 纪律
//!
//! - 端点已配置但调用失败 → **报错**，绝不静默降级为 mock
//!   （否则「自进化闭环用了真 LLM」这句话会被离线假数据顶替）；
//! - 同步入口 `execute` 无外部出口能力，若端点已配置 → **报错**，
//!   不返回 mock（防同步调用方在不知情下拿到假建议）。
//!
//! # 凭据纪律
//!
//! 凭据只从环境变量注入，**不进日志 / 不进错误消息 / 不进 URL**；
//! 不落配置文件、不进规则集（规则集是审计数据，密钥进规则集即审计链泄密）。
//!
//! # 与 Python 基线的一致性
//!
//! - `_jinja_substitute` 复刻（`{{ key }}` 替换，值用 Python 兼容 JSON 序列化）
//! - 离线 mock 建议文本与 Python `_call_llm` 离线分支逐字一致
//! - 返回 `{suggestion, model}`（另加 `provider` / `real_llm` 溯源位）

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;

use evorule_tcb::JsonValue;

use crate::{arg_f64, arg_i64, arg_str, obj, NativeService};

/// LLM 端点环境变量（缺省 = 离线 mock，不出网）。
///
/// 端点形态（OpenAI 兼容 chat completions）：
/// - `https://host` 或 `https://host/` → 拼 `/v1/chat/completions`
/// - `https://host/v1` → 拼 `/chat/completions`
/// - 其余（路径非空且非 `/v1`）→ **按全 URL 端点原样使用**
pub const LLM_ENDPOINT_ENV: &str = "EVORULE_LLM_ADVISOR_ENDPOINT";

/// LLM 凭据环境变量（Bearer；不进日志 / 错误消息 / URL）
pub const LLM_API_KEY_ENV: &str = "EVORULE_LLM_ADVISOR_API_KEY";

/// LLM 请求超时（毫秒；缺省 30000）
pub const LLM_TIMEOUT_MS_ENV: &str = "EVORULE_LLM_ADVISOR_TIMEOUT_MS";

/// 缺省超时（毫秒）
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Python `json.dumps(v, ensure_ascii=False)` 兼容序列化（默认分隔符 `, ` 与 `: `）
fn py_json_dumps(v: &JsonValue) -> String {
    match v {
        JsonValue::Null => "null".to_string(),
        JsonValue::Bool(b) => {
            if *b {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        JsonValue::Integer(i) => format!("{i}"),
        JsonValue::String(s) => serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string()),
        JsonValue::Array(a) => {
            let items: Vec<String> = a.iter().map(py_json_dumps).collect();
            format!("[{}]", items.join(", "))
        }
        JsonValue::Object(m) => {
            let items: Vec<String> = m
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{}: {}",
                        serde_json::to_string(k).unwrap_or_else(|_| "\"\"".to_string()),
                        py_json_dumps(v)
                    )
                })
                .collect();
            format!("{{{}}}", items.join(", "))
        }
    }
}

/// Python `_jinja_substitute` 复刻：替换 `{{ key }}`（键两侧允许空白），缺失键保留原样。
fn jinja_substitute(template: &str, variables: &BTreeMap<String, JsonValue>) -> String {
    let chars: Vec<char> = template.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '{' && i + 1 < chars.len() && chars[i + 1] == '{' {
            // 找闭合 }}
            let mut end = None;
            let mut j = i + 2;
            while j + 1 < chars.len() {
                if chars[j] == '}' && chars[j + 1] == '}' {
                    end = Some(j);
                    break;
                }
                j += 1;
            }
            if let Some(end) = end {
                let inner: String = chars[i + 2..end].iter().collect();
                let key = inner.trim();
                match variables.get(key) {
                    Some(val) => out.push_str(&py_json_dumps(val)),
                    None => {
                        out.push_str("{{");
                        out.push_str(&inner);
                        out.push_str("}}");
                    }
                }
                i = end + 2;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// 离线确定性 mock 建议（与 Python `_call_llm` 离线分支一致）
fn offline_mock(prompt: &str, system: &str, model: &str) -> String {
    let truncated: String = prompt.chars().take(200).collect();
    format!(
        "[Offline-Mock LLM, model={model}]\n\
         【系统提示】{system}\n\
         【建议步骤】\n\
         1. 复核输入参数合法性（检查 pending_alert 字段）；\n\
         2. 检查求解器参数 tolerance 是否过于严格；\n\
         3. 切换 solver_type=TRAC-IK 或增大 max_iterations 重试。\n\
         【用户实际 prompt】\n\
         {truncated}..."
    )
}

/// 渲染 prompt（模板 + 变量），两条路径共用
fn render_prompt(args: &JsonValue) -> String {
    let template = arg_str(
        args,
        "prompt_template",
        "告警: {{ alert }}, 输入: {{ snapshot }}，请生成 3 条中文排查建议。",
    );
    let alert = args.get("tpl_alert").cloned().unwrap_or(JsonValue::Null);
    let snapshot = args.get("tpl_snapshot").cloned().unwrap_or(JsonValue::Null);
    let mut variables = BTreeMap::new();
    variables.insert("alert".to_string(), alert);
    variables.insert("snapshot".to_string(), snapshot);
    jinja_substitute(&template, &variables)
}

/// 读取已配置的 LLM 端点（None = 未配置 → 走离线）
fn configured_endpoint() -> Option<String> {
    std::env::var(LLM_ENDPOINT_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 端点归一：空路径 → `/v1/chat/completions`；`/v1` → 拼 `/chat/completions`；其余原样
fn resolve_endpoint(raw: &str) -> Result<String, String> {
    let s = raw.trim().trim_end_matches('/');
    let (scheme, rest) = s.split_once("://").ok_or_else(|| {
        "端点须为 http(s) 绝对 URL（如 https://host/v1/chat/completions）".to_string()
    })?;
    if scheme != "http" && scheme != "https" {
        return Err("端点协议仅支持 http / https".to_string());
    }
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if host.is_empty() {
        return Err("端点缺少主机名".to_string());
    }
    Ok(match path {
        "" => format!("{scheme}://{host}/v1/chat/completions"),
        "/v1" => format!("{scheme}://{host}/v1/chat/completions"),
        _ => s.to_string(),
    })
}

/// 读取超时（毫秒）：env > 缺省；非法值回退缺省
fn timeout_ms() -> u64 {
    std::env::var(LLM_TIMEOUT_MS_ENV)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_TIMEOUT_MS)
}

/// 全局共享 HTTP 客户端（连接复用；进程内单例）
fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(DEFAULT_TIMEOUT_MS))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// 脱敏：错误/回显文本里的端点 URL 与凭据一律替换为占位符
///
/// 理由：reqwest 的错误 Display **默认带 URL**（`error sending request for url (...)`），
/// 直传即等于把端点写进错误消息；上游错误页也可能回显请求 URL 或凭据。
fn redact(text: &str, url: &str, api_key: &str) -> String {
    let mut out = text.to_string();
    if !api_key.is_empty() {
        out = out.replace(api_key, "<redacted-credential>");
    }
    if !url.is_empty() {
        out = out.replace(url, "<redacted-endpoint>");
    }
    out
}

/// 截断（错误消息用；防超长）
fn truncate(s: &str, n: usize) -> String {
    let t: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        format!("{t}…")
    } else {
        t
    }
}

/// 构造 OpenAI 兼容 chat completions 请求体
fn build_request_body(
    args: &JsonValue,
    prompt: &str,
    system: &str,
    model: &str,
) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": prompt},
        ],
        "temperature": arg_f64(args, "temperature", 0.2),
        "max_tokens": arg_i64(args, "max_tokens", 600),
    })
}

/// 出网调用 OpenAI 兼容端点 → `(建议正文, 服务端回报的模型名)`
///
/// 失败一律 `Err`（fail-visible）：**不降级为 mock**。
/// 错误消息**不含端点 URL 与凭据**。
async fn call_remote(
    args: &JsonValue,
    prompt: &str,
    system: &str,
    model: &str,
) -> Result<(String, String), String> {
    let raw = configured_endpoint().ok_or_else(|| "未配置 LLM 端点".to_string())?;
    let url = resolve_endpoint(&raw).map_err(|e| format!("LLM 端点配置无效: {e}"))?;
    let api_key = std::env::var(LLM_API_KEY_ENV).unwrap_or_default();
    let body = build_request_body(args, prompt, system, model);

    let mut req = http_client()
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .timeout(std::time::Duration::from_millis(timeout_ms()))
        .json(&body);
    if !api_key.is_empty() {
        req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {api_key}"));
    }

    let resp = req
        .send()
        .await
        // `without_url()` 剥掉 reqwest 错误里带的 URL（reqwest 默认会把它写进 Display）
        .map_err(|e| {
            format!(
                "LLM 端点请求失败: {}（端点 URL 与凭据不外泄）",
                e.without_url()
            )
        })?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("LLM 端点响应读取失败: {}", e.without_url()))?;
    if !status.is_success() {
        return Err(format!(
            "LLM 端点返回非 2xx: HTTP {status} — {}",
            truncate(&redact(&text, &url, &api_key), 200)
        ));
    }
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("LLM 端点响应非合法 JSON: {e}"))?;
    let content = v
        .pointer("/choices/0/message/content")
        .and_then(|c| c.as_str())
        .ok_or_else(|| {
            "LLM 响应缺少 choices[0].message.content — 自诊断指引: 端点须为 OpenAI 兼容 chat completions"
                .to_string()
        })?
        .to_string();
    let reported = v
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or(model)
        .to_string();
    Ok((content, reported))
}

/// `llm_advisor` 原生服务
pub struct LlmAdvisor;

impl LlmAdvisor {
    /// 离线结果（带溯源位）
    fn offline_result(args: &JsonValue) -> JsonValue {
        let prompt = render_prompt(args);
        let system = arg_str(args, "system", "你是一名资深的机器人系统可靠性工程师。");
        let model = arg_str(args, "model", "gpt-4o-mini");
        let suggestion = offline_mock(&prompt, &system, &model);
        obj(vec![
            ("suggestion", JsonValue::string(suggestion)),
            ("model", JsonValue::string(model)),
            ("provider", JsonValue::string("offline_mock")),
            ("real_llm", JsonValue::Bool(false)),
        ])
    }
}

impl NativeService for LlmAdvisor {
    /// 同步入口 = **无外部出口能力**的路径。
    ///
    /// 端点已配置时**报错**而非返回 mock：同步调用方拿到的必须是真身或明确的错误，
    /// 不得在不知情下拿到假建议。
    fn execute(&self, args: &JsonValue) -> evorule_reactor::IoResult {
        if configured_endpoint().is_some() {
            return Err("llm_advisor 同步入口无外部出口能力，但已配置 LLM 端点 — \
                 自诊断指引: 经 IoDispatcher 的 call_service 路由调用（走异步出网路径）"
                .to_string());
        }
        Ok(LlmAdvisor::offline_result(args))
    }

    /// 异步入口 = 正规出网路径（插件外部 I/O 的唯一正规口子）。
    fn execute_async<'a>(
        &'a self,
        args: &'a JsonValue,
    ) -> Pin<Box<dyn Future<Output = evorule_reactor::IoResult> + Send + 'a>> {
        Box::pin(async move {
            if configured_endpoint().is_none() {
                // 未配置端点 → 离线替身（如实标注 provider / real_llm）
                return Ok(LlmAdvisor::offline_result(args));
            }
            let prompt = render_prompt(args);
            let system = arg_str(args, "system", "你是一名资深的机器人系统可靠性工程师。");
            let model = arg_str(args, "model", "gpt-4o-mini");
            let (suggestion, reported_model) = call_remote(args, &prompt, &system, &model).await?;
            Ok(obj(vec![
                ("suggestion", JsonValue::string(suggestion)),
                ("model", JsonValue::string(reported_model)),
                ("provider", JsonValue::string("http")),
                ("real_llm", JsonValue::Bool(true)),
            ]))
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    #![allow(clippy::await_holding_lock)]
    use super::*;
    use crate::NativeService;

    /// env 变量在测试进程内共享 —— **所有会读/写 `LLM_ENDPOINT_ENV` 的用例必须串行**。
    ///
    /// cargo test 默认多线程并行：不串行就会出现「A 用例刚 set 完端点、B 用例以为
    /// 自己没配端点」的竞态（典型 flaky 源，且表现为随机失败，极难定位）。
    /// 锁中毒时取 inner 继续跑——一个用例失败不应级联污染其余用例。
    /// 守卫全程持有（横跨 await 点）是 env 全局态串行的必然形态，测试面豁免 await_holding_lock；
    /// 该豁免仅约束测试面，生产代码的锁跨 await 仍被 -D warnings 阻断。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn test_py_json_dumps_object_matches_python() {
        // Python: json.dumps({"a": 1, "b": [1, 2], "c": null}, ensure_ascii=False)
        //       = '{"a": 1, "b": [1, 2], "c": null}'
        let v = JsonValue::object_from_pairs(&[
            ("a", JsonValue::Integer(1)),
            (
                "b",
                JsonValue::Array(vec![JsonValue::Integer(1), JsonValue::Integer(2)]),
            ),
            ("c", JsonValue::Null),
        ]);
        assert_eq!(py_json_dumps(&v), r#"{"a": 1, "b": [1, 2], "c": null}"#);
    }

    #[test]
    fn test_jinja_substitute_basic() {
        let template = "告警: {{ alert }}, 输入: {{ snapshot }}。";
        let mut vars = BTreeMap::new();
        vars.insert(
            "alert".to_string(),
            JsonValue::object_from_pairs(&[("error", JsonValue::string("出界"))]),
        );
        vars.insert("snapshot".to_string(), JsonValue::Integer(42));
        let out = jinja_substitute(template, &vars);
        assert_eq!(out, "告警: {\"error\": \"出界\"}, 输入: 42。");
    }

    #[test]
    fn test_offline_advise_shape() {
        let _guard = env_guard();
        let svc = LlmAdvisor;
        let args = JsonValue::object_from_pairs(&[
            ("model", JsonValue::string("gpt-4o-mini")),
            (
                "tpl_alert",
                JsonValue::object_from_pairs(&[("error", JsonValue::string("x"))]),
            ),
        ]);
        let r = svc.execute(&args).unwrap();
        let suggestion = r.get("suggestion").and_then(|v| v.as_str()).unwrap();
        assert!(suggestion.starts_with("[Offline-Mock LLM, model=gpt-4o-mini]"));
        assert!(suggestion.contains("【建议步骤】"));
        assert!(r.get("model").and_then(|v| v.as_str()) == Some("gpt-4o-mini"));
    }

    #[test]
    fn test_offline_result_carries_provenance() {
        // 替身必须自带溯源位：provider=offline_mock / real_llm=false
        let _guard = env_guard();
        let svc = LlmAdvisor;
        let r = svc.execute(&JsonValue::empty_object()).unwrap();
        assert_eq!(
            r.get("provider").and_then(|v| v.as_str()),
            Some("offline_mock")
        );
        assert_eq!(r.get("real_llm").and_then(|v| v.as_bool()), Some(false));
    }

    #[test]
    fn test_sync_path_refuses_to_serve_mock_when_endpoint_configured() {
        // 端点已配置 → 同步入口报错（不返回 mock 冒充真身）
        let _guard = env_guard();
        std::env::set_var(LLM_ENDPOINT_ENV, "http://127.0.0.1:9/v1/chat/completions");
        let svc = LlmAdvisor;
        let r = svc.execute(&JsonValue::empty_object());
        std::env::remove_var(LLM_ENDPOINT_ENV);
        let err = r.unwrap_err();
        assert!(
            err.contains("无外部出口能力"),
            "同步入口在已配端点时须明确拒绝: {err}"
        );
    }

    #[tokio::test]
    async fn test_async_offline_when_endpoint_absent() {
        // 未配置端点 → 离线替身 + 溯源位
        let _guard = env_guard();
        std::env::remove_var(LLM_ENDPOINT_ENV);
        let svc = LlmAdvisor;
        let r = svc.execute_async(&JsonValue::empty_object()).await.unwrap();
        assert_eq!(
            r.get("provider").and_then(|v| v.as_str()),
            Some("offline_mock")
        );
        assert_eq!(r.get("real_llm").and_then(|v| v.as_bool()), Some(false));
    }

    #[tokio::test]
    async fn test_async_endpoint_unreachable_fails_visible_not_mock() {
        // 端点已配置但不可达 → 报错，**不得**降级为 mock
        // 端口 9 (discard) 在本机通常无监听 → 连接失败；短超时避免用例挂住
        let _guard = env_guard();
        std::env::set_var(LLM_ENDPOINT_ENV, "http://127.0.0.1:9/v1/chat/completions");
        std::env::set_var(LLM_TIMEOUT_MS_ENV, "2000");
        let svc = LlmAdvisor;
        let r = svc.execute_async(&JsonValue::empty_object()).await;
        std::env::remove_var(LLM_TIMEOUT_MS_ENV);
        std::env::remove_var(LLM_ENDPOINT_ENV);
        let err = r.unwrap_err();
        assert!(err.contains("LLM 端点"), "端点不可达须显式报错: {err}");
        assert!(
            !err.contains("offline_mock"),
            "失败消息不得出现替身来源: {err}"
        );
    }

    #[test]
    fn test_resolve_endpoint_shapes() {
        assert_eq!(
            resolve_endpoint("https://api.example.com").unwrap(),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(
            resolve_endpoint("https://api.example.com/v1").unwrap(),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(
            resolve_endpoint("https://api.example.com/v1/chat/completions").unwrap(),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(
            resolve_endpoint("https://api.example.com/custom/path").unwrap(),
            "https://api.example.com/custom/path"
        );
        assert!(resolve_endpoint("not-a-url").is_err());
        assert!(resolve_endpoint("ftp://host/x").is_err());
    }

    #[test]
    fn test_request_body_carries_model_system_and_prompt() {
        let args = JsonValue::object_from_pairs(&[
            ("model", JsonValue::string("m-x")),
            ("system", JsonValue::string("s-x")),
            ("temperature", JsonValue::string("0.3")),
            ("max_tokens", JsonValue::Integer(123)),
        ]);
        let body = build_request_body(&args, "p-x", "s-x", "m-x");
        assert_eq!(body["model"], serde_json::json!("m-x"));
        assert_eq!(body["messages"][0]["role"], serde_json::json!("system"));
        assert_eq!(body["messages"][0]["content"], serde_json::json!("s-x"));
        assert_eq!(body["messages"][1]["content"], serde_json::json!("p-x"));
        assert_eq!(body["temperature"], serde_json::json!(0.3));
        assert_eq!(body["max_tokens"], serde_json::json!(123));
    }
}

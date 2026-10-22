// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `llm_advisor` 真 LLM 出网路径 e2e（进程内 OpenAI 兼容桩）
//!
//! 覆盖「替身 vs 真身」的判别纪律（2026-10-22）：
//! - 出网成功 → `provider=http` / `real_llm=true`，建议正文**来自端点**（非本地替身）
//! - 出网失败（不可达 / 非 2xx / 响应不合形状）→ **报错**，绝不降级为替身
//! - 端点未配置 → 离线替身，且如实标注 `provider=offline_mock` / `real_llm=false`
//! - 凭据只进 Authorization 头：不进请求体、不进返回体、不进错误消息、错误消息不含端点 URL
//!
//! 桩是真 TCP 服务（127.0.0.1 随机端口），走真实 HTTP 栈，不 mock 客户端。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Mutex;
use std::time::Duration;

use evorule_demo_services::llm_advisor::{LlmAdvisor, LLM_API_KEY_ENV, LLM_ENDPOINT_ENV, LLM_TIMEOUT_MS_ENV};
use evorule_demo_services::NativeService;
use evorule_tcb::JsonValue;

/// env 变量在测试进程内共享 —— 所有会读/写 LLM_* env 的用例必须串行，
/// 否则 cargo test 默认多线程并行会出现「A 刚 set 完、B 以为没配」的竞态。
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// 桩捕获到的请求
struct Captured {
    head: String,
    body: String,
}

/// 进程内 OpenAI 兼容桩：回放固定响应体，并记录收到的请求
struct Stub {
    port: u16,
    rx: Receiver<Captured>,
}

fn spawn_stub(response: &'static str) -> Stub {
    let listener = TcpListener::bind("127.0.0.1:0").expect("桩监听端口失败");
    let port = listener.local_addr().expect("桩本地地址失败").port();
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Some(captured) = read_request(&mut stream) else {
                continue;
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.len(),
                response
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
            let _ = tx.send(captured);
        }
    });
    Stub { port, rx }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// 读完整 HTTP 请求（头 + Content-Length 指定的体）
fn read_request(stream: &mut TcpStream) -> Option<Captured> {
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok()?;
    let mut raw: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        raw.extend_from_slice(&chunk[..n]);
        if let Some(p) = find_subslice(&raw, b"\r\n\r\n") {
            break p + 4;
        }
    };
    let head = String::from_utf8_lossy(&raw[..header_end]).to_string();
    let content_len = head
        .lines()
        .find_map(|line| {
            let (k, v) = line.split_once(':')?;
            if k.trim().eq_ignore_ascii_case("content-length") {
                v.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    let mut body = raw[header_end..].to_vec();
    while body.len() < content_len {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some(Captured {
        head,
        body: String::from_utf8_lossy(&body).to_string(),
    })
}

fn probe_args() -> JsonValue {
    JsonValue::object_from_pairs(&[
        ("model", JsonValue::string("gpt-4o-mini")),
        ("system", JsonValue::string("你是 EvoRule 架构师。")),
        (
            "prompt_template",
            JsonValue::string("当前故障：{{ alert }}。请生成修复规则。"),
        ),
        ("tpl_alert", JsonValue::string("量纲不一致")),
    ])
}

#[tokio::test]
async fn e2e_real_endpoint_round_trip_is_marked_as_real_llm() {
    let _guard = env_guard();
    let stub = spawn_stub(
        r#"{"id":"chatcmpl-1","model":"stub-reported-model","choices":[{"index":0,"message":{"role":"assistant","content":"STUB_REPLY 建议：检查量纲一致性"},"finish_reason":"stop"}]}"#,
    );
    std::env::set_var(LLM_ENDPOINT_ENV, format!("http://127.0.0.1:{}/v1", stub.port));
    std::env::set_var(LLM_API_KEY_ENV, "probe-secret-9f3a");
    std::env::set_var(LLM_TIMEOUT_MS_ENV, "5000");

    let svc = LlmAdvisor;
    let r = svc
        .execute_async(&probe_args())
        .await
        .expect("端点可达时应成功出网");

    // 溯源位：真身不得冒充替身
    assert_eq!(r.get("provider").and_then(|v| v.as_str()), Some("http"));
    assert_eq!(r.get("real_llm").and_then(|v| v.as_bool()), Some(true));
    // 正文与模型名都来自端点回报
    assert_eq!(
        r.get("suggestion").and_then(|v| v.as_str()),
        Some("STUB_REPLY 建议：检查量纲一致性")
    );
    assert_eq!(
        r.get("model").and_then(|v| v.as_str()),
        Some("stub-reported-model")
    );

    // 桩确实收到了请求：路径按 OpenAI 兼容形态拼装
    let captured = stub
        .rx
        .recv_timeout(Duration::from_secs(5))
        .expect("桩应收到请求");
    let request_line = captured.head.lines().next().unwrap_or("");
    assert!(
        request_line.starts_with("POST /v1/chat/completions "),
        "实际请求行: {request_line}"
    );
    // 凭据只在 Authorization 头
    let head_lower = captured.head.to_lowercase();
    assert!(
        head_lower.contains("authorization: bearer probe-secret-9f3a"),
        "请求头应带 Bearer 凭据，实际: {}",
        captured.head
    );
    let body: serde_json::Value = serde_json::from_str(&captured.body).expect("请求体应为 JSON");
    assert!(
        !captured.body.contains("probe-secret-9f3a"),
        "凭据不得进请求体: {}",
        captured.body
    );
    // 请求体带模型 / 系统提示 / 渲染后的 prompt
    assert_eq!(body.get("model").and_then(|v| v.as_str()), Some("gpt-4o-mini"));
    let msgs = body
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages 应为数组");
    assert_eq!(msgs.len(), 2);
    assert!(msgs[0]
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .contains("EvoRule 架构师"));
    assert!(msgs[1]
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .contains("量纲不一致"));
    // 返回体不得含凭据
    for key in ["suggestion", "model", "provider"] {
        let v = r.get(key).and_then(|v| v.as_str()).unwrap_or("");
        assert!(!v.contains("probe-secret-9f3a"), "返回体 {key} 不得含凭据");
    }
}

#[tokio::test]
async fn e2e_unreachable_endpoint_fails_visible_without_leaking() {
    let _guard = env_guard();
    // 取一个随即释放的端口 → 连接必失败
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").port()
    };
    std::env::set_var(LLM_ENDPOINT_ENV, format!("http://127.0.0.1:{port}/v1"));
    std::env::set_var(LLM_API_KEY_ENV, "probe-secret-9f3a");
    std::env::set_var(LLM_TIMEOUT_MS_ENV, "3000");

    let svc = LlmAdvisor;
    let err = svc
        .execute_async(&probe_args())
        .await
        .expect_err("端点不可达必须报错，不得降级为替身");

    assert!(
        !err.contains("probe-secret-9f3a"),
        "错误消息不得含凭据: {err}"
    );
    assert!(
        !err.contains(&format!("127.0.0.1:{port}")),
        "错误消息不得含端点 URL: {err}"
    );
}

#[tokio::test]
async fn e2e_non_2xx_fails_visible_not_mock() {
    let _guard = env_guard();
    // 桩返回 500：服务实现须报错，不得把错误页当建议
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let _ = read_request(&mut stream);
            // 上游把请求 URL 与凭据回显进错误页 —— 服务实现须脱敏
            let body = format!(
                "{{\"error\":\"upstream boom\",\"echo_url\":\"http://127.0.0.1:{port}/v1/chat/completions\",\"echo_key\":\"probe-secret-9f3a\"}}"
            );
            let resp = format!(
                "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });
    std::env::set_var(LLM_ENDPOINT_ENV, format!("http://127.0.0.1:{port}/v1"));
    std::env::set_var(LLM_API_KEY_ENV, "probe-secret-9f3a");
    std::env::set_var(LLM_TIMEOUT_MS_ENV, "5000");

    let svc = LlmAdvisor;
    let err = svc
        .execute_async(&probe_args())
        .await
        .expect_err("非 2xx 必须报错");
    assert!(err.contains("500"), "错误消息应含 HTTP 状态码: {err}");
    assert!(!err.contains("probe-secret-9f3a"), "错误消息不得含凭据: {err}");
    assert!(
        !err.contains(&format!("127.0.0.1:{port}")),
        "上游回显的端点 URL 必须被脱敏: {err}"
    );
    assert!(
        err.contains("<redacted-endpoint>") || err.contains("<redacted-credential>"),
        "脱敏后应有占位符标记: {err}"
    );
}

#[tokio::test]
async fn e2e_response_without_choices_content_fails_visible() {
    let _guard = env_guard();
    let stub = spawn_stub(r#"{"id":"chatcmpl-2","model":"m","choices":[]}"#);
    std::env::set_var(LLM_ENDPOINT_ENV, format!("http://127.0.0.1:{}/v1", stub.port));
    std::env::set_var(LLM_API_KEY_ENV, "probe-secret-9f3a");
    std::env::set_var(LLM_TIMEOUT_MS_ENV, "5000");

    let svc = LlmAdvisor;
    let err = svc
        .execute_async(&probe_args())
        .await
        .expect_err("响应缺 choices[0].message.content 必须报错");
    assert!(
        err.contains("choices"),
        "错误消息应指明响应形状要求: {err}"
    );
    assert!(!err.contains("probe-secret-9f3a"), "错误消息不得含凭据: {err}");
}

#[tokio::test]
async fn e2e_offline_mode_is_explicitly_marked_as_mock() {
    let _guard = env_guard();
    std::env::set_var(LLM_ENDPOINT_ENV, "");
    std::env::set_var(LLM_API_KEY_ENV, "");

    let svc = LlmAdvisor;
    let r = svc
        .execute_async(&probe_args())
        .await
        .expect("未配置端点应走离线替身");
    assert_eq!(r.get("provider").and_then(|v| v.as_str()), Some("offline_mock"));
    assert_eq!(r.get("real_llm").and_then(|v| v.as_bool()), Some(false));
    let suggestion = r.get("suggestion").and_then(|v| v.as_str()).unwrap_or("");
    assert!(suggestion.contains("Offline-Mock"), "离线建议须自报家门: {suggestion}");
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! HTTP I/O Handler —— 基于 `reqwest` 执行 HTTP 请求。
//!
//! 从参数中提取 `url`、可选 `method`（默认 `GET`）、`headers`（对象）、
//! `body`（字符串或对象/数组）与 `timeout_ms`，发送请求并将响应体以
//! `JsonValue::String` 形式返回。
//!
//! 非 2xx 响应返回 `Err`，错误消息包含截断后的响应体，便于上层诊断远端错误。
//!
//! # 参数说明
//! - `url`（必需，字符串）：请求地址
//! - `method`（可选，字符串，默认 `GET`）：白名单 `GET`/`POST`/`PUT`/`PATCH`/`DELETE`/`HEAD`
//! - `headers`（可选，对象）：键值对，值必须为字符串
//! - `body`（可选）：字符串作为原始 text body；对象/数组序列化为 JSON 并自动
//!   设置 `Content-Type: application/json`
//! - `timeout_ms`（可选，整数，默认 10000）：非正数回退默认

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use async_trait::async_trait;
use evorule_reactor::{IoHandler, IoResult};
use evorule_tcb::JsonValue;
use reqwest::header::{HeaderName, HeaderValue};
use reqwest::Client;
use url::Url;

/// 默认请求超时（毫秒）（HTTP 10s）
const DEFAULT_TIMEOUT_MS: i64 = 10_000;

/// 允许的 HTTP 方法（白名单，防止任意 method）
const ALLOWED_HTTP_METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"];

/// 非 2xx 错误响应体截断长度（避免超大错误页撑爆错误消息）
const MAX_ERROR_BODY_LEN: usize = 4096;

/// HTTP 处理器
///
/// 持有 `reqwest::Client`，支持 `GET`/`POST`/`PUT`/`PATCH`/`DELETE`/`HEAD` 方法。
/// 客户端内部管理连接池，可被多任务共享。
pub struct HttpHandler {
    client: Client,
    /// 是否允许 loopback（127.0.0.0/8, ::1）目标。
    ///
    /// 生产环境永远为 `false`（SSRF 防护拒绝 loopback）。
    /// 仅在 `new_for_tests()` 构造时为 `true`，供 mockito 等本地 mock server 使用。
    #[allow(dead_code)]
    allow_loopback: bool,
}

/// 默认最大空闲连接数
const DEFAULT_POOL_MAX_IDLE_PER_HOST: usize = 100;
/// 默认连接超时（秒）
const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 30;
/// 默认 TCP Keep-Alive（秒）
const DEFAULT_TCP_KEEPALIVE_SECS: u64 = 60;

impl HttpHandler {
    /// 创建新的 HTTP 处理器（生产环境，SSRF 防护拒绝 loopback）。
    ///
    /// 使用优化的连接池配置：
    /// - `pool_max_idle_per_host`: 100（默认 2）
    /// - `connect_timeout`: 30s
    /// - `tcp_keepalive`: 60s
    pub fn new() -> Self {
        let client = Self::build_client(
            DEFAULT_POOL_MAX_IDLE_PER_HOST,
            Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS),
            Duration::from_secs(DEFAULT_TCP_KEEPALIVE_SECS),
        );
        Self {
            client,
            allow_loopback: false,
        }
    }

    /// 创建允许 loopback 目标的 HTTP 处理器（仅测试用）。
    ///
    /// 生产代码不应调用此构造函数。mockito 等 mock server 绑定到 127.0.0.1，
    /// 需要 loopback 放行才能通过 SSRF 防护。
    #[cfg(test)]
    pub fn new_for_tests() -> Self {
        let client = Self::build_client(
            DEFAULT_POOL_MAX_IDLE_PER_HOST,
            Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS),
            Duration::from_secs(DEFAULT_TCP_KEEPALIVE_SECS),
        );
        Self {
            client,
            allow_loopback: true,
        }
    }

    /// 创建允许 loopback 目标的 HTTP 处理器（开发模式）。
    ///
    /// 与 `new_for_tests()` 相同但不受 `#[cfg(test)]` 限制，供 evorule-server
    /// 的 `--allow-loopback` CLI 标志使用。仅在本地开发环境（如调用同机
    /// 127.0.0.1 上的 FastAPI/mock 服务）时启用，生产环境永远不要使用。
    pub fn new_dev_allow_loopback() -> Self {
        let client = Self::build_client(
            DEFAULT_POOL_MAX_IDLE_PER_HOST,
            Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS),
            Duration::from_secs(DEFAULT_TCP_KEEPALIVE_SECS),
        );
        Self {
            client,
            allow_loopback: true,
        }
    }

    /// 内部：构建 reqwest Client
    fn build_client(
        pool_max_idle_per_host: usize,
        connect_timeout: Duration,
        tcp_keepalive: Duration,
    ) -> Client {
        match Client::builder()
            .pool_max_idle_per_host(pool_max_idle_per_host)
            .connect_timeout(connect_timeout)
            .tcp_keepalive(tcp_keepalive)
            // B1 修复（SSRF 绕过防护）：禁用 HTTP 重定向跟随。
            //
            // reqwest 默认跟随最多 10 次重定向。SSRF 防护只在原始 URL 的 DNS 解析
            // 结果上做 IP 黑名单检查，重定向后的目标 IP 不再校验。攻击者可配置
            // 公网 URL → 302 → 169.254.169.254（云元数据）绕过 SSRF 防护。
            //
            // 禁用后，3xx 响应会作为结果返回给上层（非 2xx → Err），由调用方决定
            // 如何处理。这是 SSRF 防护的行业最佳实践。
            .redirect(reqwest::redirect::Policy::none())
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Failed to build reqwest Client: {}", e);
                Client::new()
            }
        }
    }

    /// 使用自定义连接池配置创建 HTTP 处理器
    pub fn with_pool_config(
        pool_max_idle_per_host: usize,
        connect_timeout: Duration,
        tcp_keepalive: Duration,
    ) -> Self {
        let client = Self::build_client(pool_max_idle_per_host, connect_timeout, tcp_keepalive);
        Self {
            client,
            allow_loopback: false,
        }
    }
}

impl Default for HttpHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl IoHandler for HttpHandler {
    async fn execute(&self, params: &JsonValue) -> IoResult {
        // 提取 url（必需）
        let url = params
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required param: url".to_string())?;

        // SSRF 防护（第一步）—— 仅做 URL scheme 校验（无网络 I/O）
        // DNS 解析 + IP 黑名单检查推迟到所有参数校验之后，避免参数错误时
        // 仍发起 DNS 查询（既慢又导致测试依赖网络）。
        let parsed = Url::parse(url).map_err(|e| format!("invalid url: {e}"))?;
        let scheme = parsed.scheme().to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            return Err(format!(
                "SSRF blocked: scheme '{scheme}' is not allowed (only http/https)"
            ));
        }

        // 提取 method（可选，默认 GET；白名单防止任意 method）
        let method_str = params
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("GET")
            .to_ascii_uppercase();
        if !ALLOWED_HTTP_METHODS.contains(&method_str.as_str()) {
            return Err(format!(
                "unsupported http method: '{method_str}', allowed: {}",
                ALLOWED_HTTP_METHODS.join(", ")
            ));
        }
        let method = reqwest::Method::from_bytes(method_str.as_bytes())
            .map_err(|e| format!("invalid http method '{method_str}': {e}"))?;

        // 提取 timeout_ms（可选，默认 10s；非正数回退默认）
        let timeout_ms: i64 = params
            .get("timeout_ms")
            .and_then(|v| v.as_i64())
            .unwrap_or(DEFAULT_TIMEOUT_MS);
        let timeout = if timeout_ms > 0 {
            Duration::from_millis(timeout_ms as u64)
        } else {
            Duration::from_millis(DEFAULT_TIMEOUT_MS as u64)
        };

        // 构建请求
        let mut req = self.client.request(method, url).timeout(timeout);

        // 提取 headers（可选，对象形式，值必须为字符串）
        if let Some(headers) = params.get("headers").and_then(|v| v.as_object()) {
            for (k, v) in headers.iter() {
                let name = HeaderName::try_from(k.as_str())
                    .map_err(|e| format!("invalid header name '{k}': {e}"))?;
                let val_str = v
                    .as_str()
                    .ok_or_else(|| format!("header value for '{k}' must be a string"))?;
                let value = HeaderValue::try_from(val_str)
                    .map_err(|e| format!("invalid header value for '{k}': {e}"))?;
                req = req.header(name, value);
            }
        }

        // 提取 body（可选；POST/PUT/PATCH 常用）
        // - 字符串：作为原始 text body（Content-Type 由调用方在 headers 设置）
        // - 对象/数组：序列化为 JSON 文本，并自动设置 Content-Type: application/json
        // （若调用方在 headers 显式设置了 Content-Type，会覆盖此默认——以调用方为准）
        // - Null/Bool/Integer：拒绝（避免隐式把标量当 body 发出）
        if let Some(body) = params.get("body") {
            match body {
                JsonValue::String(s) => {
                    req = req.body(s.clone());
                }
                JsonValue::Object(_) | JsonValue::Array(_) => {
                    let json = body.to_string();
                    req = req.header(reqwest::header::CONTENT_TYPE, "application/json");
                    req = req.body(json);
                }
                JsonValue::Null | JsonValue::Bool(_) | JsonValue::Integer(_) => {
                    return Err("param 'body' must be a string, object, or array".to_string());
                }
            }
        }

        // SSRF 防护（第二步）—— DNS 解析 + IP 黑名单检查
        // 在所有参数校验通过后、实际发请求前执行，避免参数错误时仍发起 DNS 查询。
        let host_str = parsed
            .host_str()
            .ok_or_else(|| "SSRF blocked: url has no host".to_string())?
            .to_string();
        let port = parsed
            .port()
            .unwrap_or(if scheme == "https" { 443 } else { 80 });
        let hosts: Vec<SocketAddr> = tokio::net::lookup_host(format!("{host_str}:{port}"))
            .await
            .map_err(|e| format!("dns lookup failed for '{host_str}': {e}"))?
            .collect();
        if hosts.is_empty() {
            return Err(format!("SSRF blocked: no dns record for host '{host_str}'"));
        }
        for addr in &hosts {
            let blocked = is_ip_ssrf_blocked(&addr.ip());
            // 测试模式 + loopback IP → 放行（供 mockito 等本地 mock server 使用）
            if blocked && !(self.allow_loopback && is_loopback_ip(&addr.ip())) {
                return Err(format!(
                    "SSRF blocked: host '{host_str}' resolves to blocked IP {}",
                    addr.ip()
                ));
            }
        }

        // 发送请求
        let response = req
            .send()
            .await
            .map_err(|e| format!("http request failed: {e}"))?;

        // 先读响应体（无论成功失败都读，便于错误诊断）
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| format!("read response body failed: {e}"))?;

        if !status.is_success() {
            // 非 2xx：把响应体（截断）包进错误消息，便于上层诊断远端错误。
            // 旧实现只返回状态码，丢失远端错误详情。
            let snippet = truncate_for_error(&body, MAX_ERROR_BODY_LEN);
            return Err(format!(
                "http request failed with status: {status}, body: {snippet}"
            ));
        }

        Ok(JsonValue::String(body))
    }
}

/// 截断字符串用于错误消息，超长加省略标记。
///
/// 按字符边界截断（`chars().take(n)`），避免切断 UTF-8 多字节字符。
fn truncate_for_error(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_len).collect();
        format!("{truncated}...[truncated]")
    }
}

// ============================================================================
// SSRF IP 黑名单（修复：H6 SSRF）
// ============================================================================

/// 判断 IP 是否为 loopback（127.0.0.0/8 或 ::1）。
///
/// 用于 `allow_loopback` 测试模式下的放行判断：
/// 即使该 IP 在 SSRF 黑名单内，测试模式允许 loopback 通过。
fn is_loopback_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

/// 判断 IP 是否属于 SSRF 拦截列表。
///
/// 覆盖的地址段：
/// - IPv4: 127.0.0.0/8（环回）、169.254.0.0/16（链路本地/云元数据）、10.0.0.0/8（私有 A）、
///   172.16.0.0/12（私有 B）、192.168.0.0/16（私有 C）、0.0.0.0/8（本网络）、
///   224.0.0.0/4（多播 D 类）、240.0.0.0/4（保留 E 类）、255.255.255.255
/// - IPv6: ::1（环回）、::ffff:0:0/96（IPv4 映射，间接覆盖 IPv4 私有段）、
///   fe80::/10（链路本地）、fc00::/7（唯一本地 ULA）、ff00::/8（多播）
#[allow(clippy::match_overlapping_arm)]
fn is_ip_ssrf_blocked(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            matches!(
                o,
                [127, _, _, _]        // 127.0.0.0/8
                    | [169, 254, _, _] // 169.254.0.0/16
                    | [10, _, _, _]    // 10.0.0.0/8
                    | [192, 168, _, _] // 192.168.0.0/16
                    | [0, _, _, _]     // 0.0.0.0/8
                    | [255, 255, 255, 255]
            ) || (o[0] == 172 && (16..=31).contains(&o[1])) // 172.16.0.0/12
                || o[0] >= 224 // D 类 224-239 + E 类 240-255
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            // ::1
            if s == [0, 0, 0, 0, 0, 0, 0, 1] {
                return true;
            }
            // ::ffff:0:0/96 (IPv4 mapped) — 解出内部 IPv4 再递归判断
            if s[0] == 0 && s[1] == 0 && s[2] == 0 && s[3] == 0 && s[4] == 0 {
                if s[5] == 0xffff {
                    let v4_inner = std::net::Ipv4Addr::new(
                        (s[6] >> 8) as u8,
                        (s[6] & 0xff) as u8,
                        (s[7] >> 8) as u8,
                        (s[7] & 0xff) as u8,
                    );
                    return is_ip_ssrf_blocked(&IpAddr::V4(v4_inner));
                }
                // 兼容兼容（::w.x.y.z 形式）
                if s[5] == 0 {
                    let v4_inner = std::net::Ipv4Addr::new(
                        (s[6] >> 8) as u8,
                        (s[6] & 0xff) as u8,
                        (s[7] >> 8) as u8,
                        (s[7] & 0xff) as u8,
                    );
                    return is_ip_ssrf_blocked(&IpAddr::V4(v4_inner));
                }
            }
            // fe80::/10 链路本地
            if (s[0] & 0xffc0) == 0xfe80 {
                return true;
            }
            // fc00::/7 ULA
            if (s[0] & 0xfe00) == 0xfc00 {
                return true;
            }
            // ff00::/8 多播
            if s[0] & 0xff00 == 0xff00 {
                return true;
            }
            // ::（unspecified）在某些场景也当 SSRF
            s == [0, 0, 0, 0, 0, 0, 0, 0]
        }
    }
}

#[cfg(test)]
mod ssrf_tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn test_blocked_v4() {
        for s in [
            "127.0.0.1",
            "127.255.255.1",
            "169.254.169.254", // AWS/GCP 元数据
            "169.254.0.1",
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.0.1",
            "192.168.255.255",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "240.0.0.1",
        ] {
            let ip: IpAddr = s.parse().unwrap();
            assert!(is_ip_ssrf_blocked(&ip), "should block {s}");
        }
    }

    #[test]
    fn test_allowed_v4() {
        for s in [
            "8.8.8.8",
            "1.1.1.1",
            "223.5.5.5",
            "172.15.255.255", // 172.15 是公网
            "172.32.0.0",     // 172.32 是公网
        ] {
            let ip: IpAddr = s.parse().unwrap();
            assert!(!is_ip_ssrf_blocked(&ip), "should allow {s}");
        }
    }

    #[test]
    fn test_v6_loopback() {
        let ip = IpAddr::V6(Ipv6Addr::LOCALHOST);
        assert!(is_ip_ssrf_blocked(&ip));
    }

    #[test]
    fn test_v6_link_local() {
        let ip: IpAddr = "fe80::1".parse().unwrap();
        assert!(is_ip_ssrf_blocked(&ip));
    }

    #[test]
    fn test_v6_ula() {
        let ip: IpAddr = "fc00::1".parse().unwrap();
        assert!(is_ip_ssrf_blocked(&ip));
        let ip: IpAddr = "fd12:3456:789a::1".parse().unwrap();
        assert!(is_ip_ssrf_blocked(&ip));
    }

    #[test]
    fn test_v6_unspecified() {
        let ip = IpAddr::V6(Ipv6Addr::UNSPECIFIED);
        assert!(is_ip_ssrf_blocked(&ip));
    }

    #[test]
    fn test_v6_multicast() {
        let ip: IpAddr = "ff02::1".parse().unwrap();
        assert!(is_ip_ssrf_blocked(&ip));
    }

    #[test]
    fn test_v6_mapped_v4_private() {
        // ::ffff:127.0.0.1 → ::ffff:7f00:0001
        let ip: IpAddr = "::ffff:127.0.0.1".parse().unwrap();
        assert!(is_ip_ssrf_blocked(&ip));
        let ip: IpAddr = "::ffff:169.254.169.254".parse().unwrap();
        assert!(is_ip_ssrf_blocked(&ip));
        let ip: IpAddr = "::ffff:10.0.0.1".parse().unwrap();
        assert!(is_ip_ssrf_blocked(&ip));
        let ip: IpAddr = "::ffff:192.168.1.1".parse().unwrap();
        assert!(is_ip_ssrf_blocked(&ip));
        // 映射的公网 IP 应该放行
        let ip: IpAddr = "::ffff:8.8.8.8".parse().unwrap();
        assert!(!is_ip_ssrf_blocked(&ip));
    }

    #[test]
    fn test_v6_compatible_v4() {
        // ::127.0.0.1 (deprecated 兼容形式)
        let v = Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0x7f00, 1);
        let ip = IpAddr::V6(v);
        assert!(is_ip_ssrf_blocked(&ip));
    }

    #[test]
    fn test_v6_global_public() {
        let ip: IpAddr = "2001:4860:4860::8888".parse().unwrap();
        assert!(!is_ip_ssrf_blocked(&ip));
    }

    // 使 warnings 安静（类型在同文件另一个 test mod 也用）
    #[allow(dead_code)]
    fn _unused(_: Ipv4Addr) {}
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used, clippy::panic)]
    use super::*;
    use evorule_tcb::JsonValue;

    /// 构造 params 对象
    fn params(pairs: &[(&str, JsonValue)]) -> JsonValue {
        JsonValue::object_from_pairs(pairs)
    }

    #[test]
    fn test_new_returns_handler() {
        let _handler = HttpHandler::new_for_tests();
        let _handler2 = HttpHandler::default();
    }

    #[test]
    fn test_with_pool_config() {
        let _handler =
            HttpHandler::with_pool_config(50, Duration::from_secs(15), Duration::from_secs(30));
    }

    #[test]
    fn test_pool_config_constants() {
        assert_eq!(DEFAULT_POOL_MAX_IDLE_PER_HOST, 100);
        assert_eq!(DEFAULT_CONNECT_TIMEOUT_SECS, 30);
        assert_eq!(DEFAULT_TCP_KEEPALIVE_SECS, 60);
        assert_eq!(DEFAULT_TIMEOUT_MS, 10_000);
    }

    #[test]
    fn test_allowed_http_methods_contains_common() {
        assert!(ALLOWED_HTTP_METHODS.contains(&"GET"));
        assert!(ALLOWED_HTTP_METHODS.contains(&"POST"));
        assert!(ALLOWED_HTTP_METHODS.contains(&"PUT"));
        assert!(ALLOWED_HTTP_METHODS.contains(&"DELETE"));
        assert!(!ALLOWED_HTTP_METHODS.contains(&"TRACE"));
    }

    #[test]
    fn test_truncate_for_error_short() {
        assert_eq!(truncate_for_error("abc", 10), "abc");
    }

    #[test]
    fn test_truncate_for_error_long() {
        let s = "a".repeat(20);
        let t = truncate_for_error(&s, 5);
        assert!(t.starts_with("aaaaa"));
        assert!(t.ends_with("...[truncated]"));
    }

    #[test]
    fn test_truncate_for_error_utf8_boundary() {
        // 中文字符（3 字节），按字符截断不应切断 UTF-8
        let s = "中文测试字符串";
        let t = truncate_for_error(s, 2);
        assert!(t.starts_with("中文"));
        assert!(t.ends_with("...[truncated]"));
    }

    // ===== HttpHandler::execute 集成测试（mockito mock server）=====

    #[tokio::test]
    async fn test_http_get_returns_body() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("GET", "/get")
            .with_status(200)
            .with_body("hello world")
            .create_async()
            .await;
        let handler = HttpHandler::new_for_tests();
        let url = format!("{}/get", server.url());
        let r = handler
            .execute(&params(&[("url", JsonValue::string(url.as_str()))]))
            .await
            .unwrap();
        assert_eq!(r.as_str(), Some("hello world"));
        m.assert_async().await;
    }

    #[tokio::test]
    async fn test_http_method_defaults_to_get() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("GET", "/d")
            .with_status(200)
            .with_body("ok")
            .create_async()
            .await;
        let handler = HttpHandler::new_for_tests();
        let url = format!("{}/d", server.url());
        // 不传 method，默认 GET
        let r = handler
            .execute(&params(&[("url", JsonValue::string(url.as_str()))]))
            .await
            .unwrap();
        assert_eq!(r.as_str(), Some("ok"));
        m.assert_async().await;
    }

    #[tokio::test]
    async fn test_http_post_with_json_body() {
        let mut server = mockito::Server::new_async().await;
        // body 为对象 → 序列化为 JSON；expected 用 JsonValue 自身生成以保证格式一致
        let body = JsonValue::object_from_pairs(&[("a", JsonValue::Integer(1))]);
        let expected = body.to_string();
        let m = server
            .mock("POST", "/post")
            .match_body(mockito::Matcher::Exact(expected))
            .with_status(200)
            .with_body("posted")
            .create_async()
            .await;
        let handler = HttpHandler::new_for_tests();
        let url = format!("{}/post", server.url());
        let r = handler
            .execute(&params(&[
                ("url", JsonValue::string(url.as_str())),
                ("method", JsonValue::string("POST")),
                ("body", body),
            ]))
            .await
            .unwrap();
        assert_eq!(r.as_str(), Some("posted"));
        m.assert_async().await;
    }

    #[tokio::test]
    async fn test_http_post_with_string_body() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("POST", "/raw")
            .match_body(mockito::Matcher::Exact("raw-text-body".to_string()))
            .with_status(200)
            .with_body("ok")
            .create_async()
            .await;
        let handler = HttpHandler::new_for_tests();
        let url = format!("{}/raw", server.url());
        let r = handler
            .execute(&params(&[
                ("url", JsonValue::string(url.as_str())),
                ("method", JsonValue::string("POST")),
                ("body", JsonValue::string("raw-text-body")),
            ]))
            .await
            .unwrap();
        assert_eq!(r.as_str(), Some("ok"));
        m.assert_async().await;
    }

    #[tokio::test]
    async fn test_http_body_rejects_scalar() {
        let handler = HttpHandler::new_for_tests();
        let r = handler
            .execute(&params(&[
                ("url", JsonValue::string("http://example.com")),
                ("body", JsonValue::Integer(42)),
            ]))
            .await;
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("body"));
    }

    #[tokio::test]
    async fn test_http_custom_headers() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("GET", "/h")
            .match_header("x-custom", "value123")
            .with_status(200)
            .with_body("ok")
            .create_async()
            .await;
        let handler = HttpHandler::new_for_tests();
        let url = format!("{}/h", server.url());
        let r = handler
            .execute(&params(&[
                ("url", JsonValue::string(url.as_str())),
                (
                    "headers",
                    JsonValue::object_from_pairs(&[("x-custom", JsonValue::string("value123"))]),
                ),
            ]))
            .await
            .unwrap();
        assert_eq!(r.as_str(), Some("ok"));
        m.assert_async().await;
    }

    #[tokio::test]
    async fn test_http_header_value_must_be_string() {
        let handler = HttpHandler::new_for_tests();
        let r = handler
            .execute(&params(&[
                ("url", JsonValue::string("http://example.com")),
                (
                    "headers",
                    JsonValue::object_from_pairs(&[("x-n", JsonValue::Integer(1))]),
                ),
            ]))
            .await;
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("header value"));
    }

    #[tokio::test]
    async fn test_http_4xx_returns_body_in_error() {
        // regression for D-2: 非 2xx 旧实现只返回状态码，丢失 body
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("GET", "/err")
            .with_status(404)
            .with_body("not found detail")
            .create_async()
            .await;
        let handler = HttpHandler::new_for_tests();
        let url = format!("{}/err", server.url());
        let r = handler
            .execute(&params(&[("url", JsonValue::string(url.as_str()))]))
            .await;
        assert!(r.is_err());
        let err = r.unwrap_err();
        assert!(err.contains("404"), "error should contain status: {err}");
        assert!(
            err.contains("not found detail"),
            "error should contain body: {err}"
        );
        m.assert_async().await;
    }

    #[tokio::test]
    async fn test_http_missing_url() {
        let handler = HttpHandler::new_for_tests();
        let r = handler
            .execute(&params(&[("not_url", JsonValue::string("x"))]))
            .await;
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("missing required param: url"));
    }

    #[tokio::test]
    async fn test_http_unsupported_method() {
        let handler = HttpHandler::new_for_tests();
        let r = handler
            .execute(&params(&[
                ("url", JsonValue::string("http://example.com")),
                ("method", JsonValue::string("TRACE")),
            ]))
            .await;
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("unsupported http method"));
    }

    #[tokio::test]
    async fn test_http_timeout_returns_error() {
        let mut server = mockito::Server::new_async().await;
        // 服务器延迟 1s 才返回 body，客户端 200ms 超时
        let m = server
            .mock("GET", "/slow")
            .with_status(200)
            .with_chunked_body(|w| {
                std::thread::sleep(std::time::Duration::from_secs(1));
                w.write_all(b"late")
            })
            .create_async()
            .await;
        let handler = HttpHandler::new_for_tests();
        let url = format!("{}/slow", server.url());
        let r = handler
            .execute(&params(&[
                ("url", JsonValue::string(url.as_str())),
                ("timeout_ms", JsonValue::Integer(200)),
            ]))
            .await;
        assert!(r.is_err());
        // 请求已发出（可能未完成），mock 不强 assert 以避免 flaky
        let _ = m;
    }
}

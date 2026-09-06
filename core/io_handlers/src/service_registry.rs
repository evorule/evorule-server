// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 服务注册中心 —— `call_service` / `call_external` 的 `service_name` → HTTP 端点映射。
//!
//! # 设计原则（机制-策略分离）
//! - 机制层 `HttpHandler` 保持通用：只认 `url` / `method` / `headers` / `body` / `timeout_ms`
//! - 策略层 `ServiceRegistry` 放在应用层（本文件）：读 JSON 配置，把 `service_name` 翻译为
//!   HTTP 调用所需参数，然后**委托**给 `HttpHandler` 实际执行
//! - 这样 `HttpHandler` 仍可独立给 `http_get` 等 IoType 使用，业务侧只需写
//!   `{"io_type": "call_service", "service_name": "inverse_kinematics_solver", "args": {...}}`
//!
//! # 配置格式（service_registry.json）
//! ```json
//! {
//!   "inverse_kinematics_solver": {
//!     "url": "http://127.0.0.1:5001/api/ik/solve",
//!     "method": "POST",
//!     "headers": { "X-Source": "evorule" },
//!     "timeout_ms": 5000
//!   },
//!   "notify_vip": {
//!     "url": "http://srv.internal/notify",
//!     "method": "POST"
//!   }
//! }
//! ```
//!
//! - `url`：**必需**，字符串
//! - `method`：可选，默认 `POST`（白名单同 `HttpHandler`）
//! - `headers`：可选，对象；值会覆盖同名 params 里的 headers（以服务注册表为准，避免规则私自加鉴权头）
//! - `timeout_ms`：可选，整数；未设置时由 `HttpHandler` 回退默认 10s
//! - `body_template`：**可选**，特殊字段；默认策略：如果 service params 里有 `args` 对象，
//!   则把 `args` 序列化作为 HTTP body 发出；调用方也可在 params 中显式传 `body` 覆盖

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use evorule_reactor::{IoHandler, IoResult};
use evorule_tcb::JsonValue;

use crate::http_handler::HttpHandler;

/// 单个服务的注册条目（对应 service_registry.json 中的一个对象）
#[derive(Debug, Clone)]
pub struct ServiceEntry {
    pub url: String,
    pub method: String,
    pub headers: BTreeMap<String, String>,
    pub timeout_ms: Option<i64>,
    /// 服务业务版本（C4，可选；server 侧 /api/services 能力对账用，缺省 None 向前兼容）
    pub version: Option<String>,
    /// 服务描述（可选；能力对账展示用）
    pub description: Option<String>,
}

/// 服务元数据（C5：/api/services 能力对账只读视图；name 保序确定性）
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ServiceMeta {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// 服务注册中心
#[derive(Debug, Clone, Default)]
pub struct ServiceRegistry {
    entries: BTreeMap<String, ServiceEntry>,
}

impl ServiceRegistry {
    /// 空注册中心（call_service 所有调用都会返回 "unknown service_name"）
    pub fn empty() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// 从 JSON 文件加载服务注册中心
    ///
    /// 文件不存在时返回空注册中心（不报错，只打 warning），方便开发阶段。
    /// JSON 解析失败会返回错误，防止配置文件损坏静默失效。
    pub fn load_from_file(path: impl AsRef<Path>) -> Result<Self, String> {
        let p = path.as_ref();
        if !p.exists() {
            tracing::warn!(
                "service_registry file not found: {} — call_service 会在运行时报 unknown service",
                p.display()
            );
            return Ok(Self::empty());
        }
        let raw = std::fs::read_to_string(p)
            .map_err(|e| format!("read service_registry failed {}: {}", p.display(), e))?;
        Self::load_from_str(&raw)
    }

    /// 从 JSON 字符串解析（便于测试和内存注入）
    ///
    /// C9：解析后先跑 schema 门禁（evorule-rule-schema::validate_service_registry，
    /// $defs 权威源 = 本文件 ServiceEntry），失败显式报错——防止结构非法的注册表
    /// 静默失效到运行时才在 call_service 处暴露。之后逐条目 parse 校验语义
    /// （url scheme 白名单等 schema 无法表达的部分）。
    pub fn load_from_str(json: &str) -> Result<Self, String> {
        let obj: serde_json::Value = serde_json::from_str(json)
            .map_err(|e| format!("parse service_registry json failed: {}", e))?;
        let map = obj
            .as_object()
            .ok_or_else(|| "service_registry 顶层必须是 JSON object".to_string())?;

        // C9: schema 门禁（加载期 fail-fast，与"JSON 解析失败报错防静默失效"同语义）
        let report = evorule_rule_schema::validate_service_registry(&obj);
        if !report.valid {
            return Err(format!(
                "service_registry 未通过 schema 门禁: {}",
                report.errors.join("; ")
            ));
        }

        let mut entries = BTreeMap::new();
        for (name, val) in map {
            let entry = parse_service_entry(name, val)?;
            entries.insert(name.clone(), entry);
        }
        tracing::info!(count = entries.len(), "loaded service_registry entries");
        Ok(Self { entries })
    }

    /// 注册单个条目（程序式动态注册）
    #[allow(dead_code)]
    pub fn insert(&mut self, name: String, entry: ServiceEntry) {
        self.entries.insert(name, entry);
    }

    /// 按名称查找条目
    pub fn get(&self, name: &str) -> Option<&ServiceEntry> {
        self.entries.get(name)
    }

    /// 已注册服务名列表（server 侧服务绑定核对用；BTreeMap 保序 → 确定性）
    pub fn service_names(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    /// 服务元数据列表（C5：server 侧 /api/services 能力对账用）
    pub fn service_metadata(&self) -> Vec<ServiceMeta> {
        self.entries
            .iter()
            .map(|(name, e)| ServiceMeta {
                name: name.clone(),
                version: e.version.clone(),
                description: e.description.clone(),
            })
            .collect()
    }

    /// 已注册条目数量
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn parse_service_entry(name: &str, val: &serde_json::Value) -> Result<ServiceEntry, String> {
    let obj = val
        .as_object()
        .ok_or_else(|| format!("service '{}' value must be JSON object", name))?;

    let url_str = obj
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("service '{}' missing required field 'url'", name))?;
    // N2 修复：校验 URL scheme 为 http/https，防止 file://、data:// 等危险 scheme。
    // HttpHandler 最终也会校验，但早期校验给出更清晰的配置错误诊断。
    let parsed_url = url::Url::parse(url_str)
        .map_err(|e| format!("service '{}' has invalid url '{}': {}", name, url_str, e))?;
    let scheme = parsed_url.scheme().to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(format!(
            "service '{}' url scheme '{}' is not allowed (only http/https): {}",
            name, scheme, url_str
        ));
    }
    let url = url_str.to_string();

    let method = obj
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("POST")
        .to_ascii_uppercase();

    let headers = match obj.get("headers") {
        Some(serde_json::Value::Object(h)) => {
            let mut map = BTreeMap::new();
            for (k, v) in h {
                let s = v
                    .as_str()
                    .ok_or_else(|| {
                        format!("service '{}' header '{}' value must be string", name, k)
                    })?
                    .to_string();
                map.insert(k.clone(), s);
            }
            map
        }
        Some(_) => {
            return Err(format!(
                "service '{}' field 'headers' must be JSON object",
                name
            ));
        }
        None => BTreeMap::new(),
    };

    let timeout_ms = obj.get("timeout_ms").and_then(|v| v.as_i64());

    Ok(ServiceEntry {
        url,
        method,
        headers,
        timeout_ms,
        version: obj
            .get("version")
            .and_then(|v| v.as_str())
            .map(String::from),
        description: obj
            .get("description")
            .and_then(|v| v.as_str())
            .map(String::from),
    })
}

// ==========================================================================
// ServiceRegistryHandler：把 call_service(call_external) 的 service_name 形式
// params 翻译成 HttpHandler 能执行的 params，然后委托执行
// ==========================================================================

/// 基于 `ServiceRegistry` 的 `IoHandler` 包装器
///
/// IoType: `CALL_SERVICE` / `CALL_EXTERNAL`
///
/// 入参优先级（后者覆盖前者）：
///   service_registry entry → params（让 rules 侧可以覆盖如 body/单请求超时）
///
/// 安全：`url` 一旦由 service_registry 给出，params 侧不得再覆盖（防止规则写
/// `"service_name":"notify_vip","url":"http://evil.com"` 绕过注册表打外部）。
pub struct ServiceRegistryHandler {
    registry: ServiceRegistry,
    http: Arc<HttpHandler>,
}

impl ServiceRegistryHandler {
    pub fn new(registry: ServiceRegistry, http: Arc<HttpHandler>) -> Self {
        Self { registry, http }
    }

    /// 转换 params：service_name → url/method/headers/timeout_ms
    ///
    /// 返回：合并后的 JsonValue（作为传给 HttpHandler 的 params）
    fn resolve(&self, params: &JsonValue) -> Result<JsonValue, String> {
        let service_name = params
            .get("service_name")
            .and_then(|v| v.as_str())
            .or_else(|| params.get("name").and_then(|v| v.as_str()))
            .ok_or_else(|| {
                "call_service/call_external missing required param: service_name".to_string()
            })?;

        let entry = self.registry.get(service_name).ok_or_else(|| {
            format!(
                "unknown service_name '{service_name}' — 服务未在执行侧 service_registry 绑定。\
                 自诊断指引: ① 确认 service_registry.json 已配置该 service_name 的 url/method;\
                 ② 确认 server 以 --service-registry <path> 启动（缺省不加载注册表，\
                 call_service/call_external 必然 unknown）;\
                 ③ 核对治理侧数据集 data_dependencies 声明的服务名与注册表键完全一致\
                 （三层绑定: 声明 → 无凭据模板 → 执行侧绑定）"
            )
        })?;

        // 用 BTreeMap 构建合并后的 params
        let mut merged: BTreeMap<String, JsonValue> = BTreeMap::new();

        // 1. 注册表条目写入（url/method/headers/timeout_ms）
        merged.insert("url".into(), JsonValue::string(entry.url.as_str()));
        merged.insert("method".into(), JsonValue::string(entry.method.as_str()));
        if !entry.headers.is_empty() {
            let h: BTreeMap<String, JsonValue> = entry
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), JsonValue::string(v.as_str())))
                .collect();
            merged.insert("headers".into(), JsonValue::Object(h));
        }
        if let Some(t) = entry.timeout_ms {
            merged.insert("timeout_ms".into(), JsonValue::Integer(t));
        }

        // 2. 合并 params 里的字段（url 禁止覆盖；body/args/timeout_ms 允许覆盖或补充）
        if let Some(obj) = params.as_object() {
            for (k, v) in obj {
                match k.as_str() {
                    // 禁止覆盖：service_registry 是唯一真相
                    "url" => continue,
                    "service_name" | "name" => continue,
                    // headers：合并，若两边都有同名字段，**注册表优先**（防止规则私自加鉴权头）
                    "headers" => {
                        if let (Some(JsonValue::Object(reg_h)), Some(param_h)) =
                            (merged.get("headers"), v.as_object())
                        {
                            let mut combined = reg_h.clone();
                            for (pk, pv) in param_h {
                                combined.entry(pk.clone()).or_insert_with(|| pv.clone());
                            }
                            merged.insert("headers".into(), JsonValue::Object(combined));
                        } else if !merged.contains_key("headers") {
                            merged.insert("headers".into(), v.clone());
                        }
                    }
                    // args：默认把 args 对象序列化成 JSON body；若 params 已显式传 body 则 body 优先
                    "args" => {
                        if merged.contains_key("body") {
                            // 显式 body 已存在，忽略 args
                        } else if matches!(v, JsonValue::Object(_) | JsonValue::Array(_)) {
                            // JSON body：序列化后以 String 存
                            merged.insert("body".into(), JsonValue::string(v.to_string().as_str()));
                            // 自动设置 Content-Type: application/json（在 headers 里注入，如果注册表没设）
                            match merged.entry("headers".into()) {
                                std::collections::btree_map::Entry::Vacant(e) => {
                                    let mut h = BTreeMap::new();
                                    h.insert(
                                        "Content-Type".into(),
                                        JsonValue::string("application/json"),
                                    );
                                    e.insert(JsonValue::Object(h));
                                }
                                std::collections::btree_map::Entry::Occupied(mut e) => {
                                    if let JsonValue::Object(h) = e.get_mut() {
                                        h.entry("Content-Type".into()).or_insert_with(|| {
                                            JsonValue::string("application/json")
                                        });
                                    }
                                }
                            }
                        }
                    }
                    // 其他字段：param 覆盖注册表（如 timeout_ms）
                    _ => {
                        merged.insert(k.clone(), v.clone());
                    }
                }
            }
        }

        Ok(JsonValue::Object(merged))
    }
}

#[async_trait]
impl IoHandler for ServiceRegistryHandler {
    async fn execute(&self, params: &JsonValue) -> IoResult {
        let resolved = self.resolve(params)?;
        let result = self.http.execute(&resolved).await?;
        // call_service 语义约定服务返回 JSON：尝试把字符串响应解析为结构化 JsonValue。
        // - 解析成功且顶层为对象/数组：返回解析后的值（让规则能直接 .field 访问）
        // - 解析失败或顶层为标量：保持原字符串（兼容纯文本/错误响应，避免破坏语义）
        //
        // 注意：`JsonValue` 无 Float 变体（TCB 确定性设计），浮点数转字符串保留原样。
        // 业务侧应像 ik_core.py 那样在服务内部完成浮点比较并返回 bool 标志（如
        // converged_ok），让 TCB 只做 eq 判断；residual 等浮点字段仅作透传。
        if let JsonValue::String(s) = &result {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s) {
                match &parsed {
                    serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                        return Ok(serde_to_json_value(parsed));
                    }
                    _ => {}
                }
            }
        }
        Ok(result)
    }
}

/// 把 `serde_json::Value` 递归转换为 evorule TCB 的 `JsonValue`。
///
/// `JsonValue` 无 Float 变体（TCB 确定性设计）：
/// - 整数（i64 范围内）→ `JsonValue::Integer`
/// - 浮点 / 超出 i64 的整数 → `JsonValue::String`（保留原样，调用方决定如何处理）
fn serde_to_json_value(v: serde_json::Value) -> JsonValue {
    match v {
        serde_json::Value::Null => JsonValue::Null,
        serde_json::Value::Bool(b) => JsonValue::Bool(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                JsonValue::Integer(i)
            } else {
                JsonValue::String(n.to_string().into())
            }
        }
        serde_json::Value::String(s) => JsonValue::String(s.into()),
        serde_json::Value::Array(arr) => {
            JsonValue::Array(arr.into_iter().map(serde_to_json_value).collect())
        }
        serde_json::Value::Object(obj) => {
            let mut map: BTreeMap<String, JsonValue> = BTreeMap::new();
            for (k, v) in obj {
                map.insert(k, serde_to_json_value(v));
            }
            JsonValue::Object(map)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn test_empty_registry() {
        let r = ServiceRegistry::empty();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn test_load_from_str_ok() {
        let json = r#"{
            "notify_vip": {"url": "http://x/notify", "method": "POST", "timeout_ms": 3000},
            "ik": {"url": "http://x/ik"}
        }"#;
        let r = ServiceRegistry::load_from_str(json).unwrap();
        assert_eq!(r.len(), 2);
        let ik = r.get("ik").unwrap();
        assert_eq!(ik.url, "http://x/ik");
        assert_eq!(ik.method, "POST"); // 默认 POST
        assert!(ik.timeout_ms.is_none());
        let nv = r.get("notify_vip").unwrap();
        assert_eq!(nv.method, "POST");
        assert_eq!(nv.timeout_ms, Some(3000));
    }

    #[test]
    fn test_load_bad_top_level() {
        let err = ServiceRegistry::load_from_str("[]").unwrap_err();
        assert!(err.contains("顶层必须是 JSON object"));
    }

    #[test]
    fn test_load_entry_missing_url() {
        // C9: 缺 url 现在由 schema 门禁先行拦截（报错消息为 schema 门禁前缀）
        let err = ServiceRegistry::load_from_str(r#"{"a": {"method":"POST"}}"#).unwrap_err();
        assert!(
            err.contains("schema 门禁"),
            "缺 url 应由 schema 门禁拦截, got: {err}"
        );
    }

    /// C9: schema 门禁拦截结构非法条目（headers 值非字符串 / timeout_ms 负数）——
    /// 这类错误 parse 层各报一条，schema 层加载期一次性拦截
    #[test]
    fn test_load_rejected_by_schema_gate() {
        let json = r#"{
            "bad": {"url": "http://x/y", "headers": {"X-Auth": 12345}, "timeout_ms": -1}
        }"#;
        let err = ServiceRegistry::load_from_str(json).unwrap_err();
        assert!(
            err.contains("schema 门禁"),
            "结构非法应被门禁拦截, got: {err}"
        );
    }

    /// C9: 未知字段向前兼容（schema additionalProperties 开放），不得拒绝
    #[test]
    fn test_load_unknown_fields_forward_compatible() {
        let json = r#"{"ok":{"url":"http://good/endpoint","body_template":{"a":1}}}"#;
        let r = ServiceRegistry::load_from_str(json).unwrap();
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn test_resolve_basic_args() {
        let reg = ServiceRegistry::load_from_str(
            r#"{"ik":{"url":"http://srv/ik","method":"POST","headers":{"X-From":"evorule"}}}"#,
        )
        .unwrap();
        let handler = ServiceRegistryHandler::new(reg, Arc::new(HttpHandler::new()));
        let params = JsonValue::object_from_pairs(&[
            ("service_name", JsonValue::string("ik")),
            (
                "args",
                JsonValue::object_from_pairs(&[
                    ("target_pose", JsonValue::string("0.1,0.2,0.3")),
                    ("solver_type", JsonValue::string("TRAC-IK")),
                ]),
            ),
        ]);
        let resolved = handler.resolve(&params).unwrap();
        assert_eq!(
            resolved.get("url").and_then(|v| v.as_str()),
            Some("http://srv/ik")
        );
        assert_eq!(
            resolved.get("method").and_then(|v| v.as_str()),
            Some("POST")
        );
        // args 被序列化成 JSON body
        assert!(resolved.get("body").and_then(|v| v.as_str()).is_some());
        let body_str = resolved.get("body").and_then(|v| v.as_str()).unwrap();
        let body: serde_json::Value = serde_json::from_str(body_str).unwrap();
        assert_eq!(
            body.get("target_pose").and_then(|v| v.as_str()),
            Some("0.1,0.2,0.3")
        );
        // Content-Type 自动注入
        let headers = resolved.get("headers").and_then(|v| v.as_object()).unwrap();
        assert_eq!(
            headers.get("Content-Type").and_then(|v| v.as_str()),
            Some("application/json")
        );
        assert_eq!(
            headers.get("X-From").and_then(|v| v.as_str()),
            Some("evorule")
        );
    }

    #[test]
    fn test_resolve_unknown_service() {
        let handler =
            ServiceRegistryHandler::new(ServiceRegistry::empty(), Arc::new(HttpHandler::new()));
        let params = JsonValue::object_from_pairs(&[("service_name", JsonValue::string("nope"))]);
        let err = handler.resolve(&params).unwrap_err();
        assert!(err.contains("unknown service_name 'nope'"));
        // 自愈原则：绑定缺失错误必须携带可自助排查的指引（测试门口径）
        assert!(err.contains("自诊断指引"), "应含自诊断指引, got: {err}");
        assert!(
            err.contains("--service-registry"),
            "应指向启动参数, got: {err}"
        );
    }

    #[test]
    fn test_resolve_url_cannot_be_overridden() {
        let reg =
            ServiceRegistry::load_from_str(r#"{"ok":{"url":"http://good/endpoint"}}"#).unwrap();
        let handler = ServiceRegistryHandler::new(reg, Arc::new(HttpHandler::new()));
        let params = JsonValue::object_from_pairs(&[
            ("service_name", JsonValue::string("ok")),
            ("url", JsonValue::string("http://evil.com")),
        ]);
        let resolved = handler.resolve(&params).unwrap();
        assert_eq!(
            resolved.get("url").and_then(|v| v.as_str()),
            Some("http://good/endpoint")
        );
    }

    #[test]
    fn test_resolve_missing_service_name() {
        let handler =
            ServiceRegistryHandler::new(ServiceRegistry::empty(), Arc::new(HttpHandler::new()));
        let params = JsonValue::object_from_pairs(&[("foo", JsonValue::Integer(1))]);
        let err = handler.resolve(&params).unwrap_err();
        assert!(err.contains("missing required param: service_name"));
    }
}

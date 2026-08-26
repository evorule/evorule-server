// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 规则文件热重载服务
//!
//! 监听规则目录变化，自动将新规则发送到 evorule-server
//!
//! # 设计
//! - 使用 notify crate 监听文件系统变化
//! - 通过 HTTP API 与 evorule-server 通信
//! - 支持手动触发重载和自动监听两种模式
//!
//! # 端点
//!
//! - `GET  /status` — 服务状态 + 规则计数
//! - `POST /reload` — 手动触发规则重载
//! - `GET  /rules`  — 列出规则文件路径
//!
//! # 设计说明
//!
//! - **handler 在 lib 中**：HTTP handler 定义在 lib.rs 而非 bin，便于 oneshot 测试。
//! - **mutex 毒化降级**：`config` 用 `std::sync::Mutex` 保护，毒化时不 panic，
//!   降级访问受污染数据并记录 error 日志。

#![forbid(unsafe_code)]
// C5 (unwrap/expect/panic = deny) 仅约束生产代码;测试代码保留 unwrap 惯例
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use axum::{
    extract::State,
    http::StatusCode,
    response::Json,
    routing::{get, post},
    Router,
};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::task;
use tracing::{info, warn};

pub mod config;
pub mod loader;
pub mod watcher;

use config::HotReloadConfig;
use loader::load_rules;
use watcher::{create_watcher, ChangeType};

/// 热重载服务
#[derive(Clone, Debug)]
pub struct HotReloadService {
    config: Arc<Mutex<HotReloadConfig>>,
}

impl HotReloadService {
    /// 创建新的热重载服务
    ///
    /// 若 `config.session_id` 为 `None`，会向 evorule-server 请求创建新会话。
    pub async fn new(mut config: HotReloadConfig) -> Result<Self, String> {
        if config.session_id.is_none() {
            let sid =
                Self::create_session(&config.evorule_server_url, config.auth_token.as_deref())
                    .await?;
            info!(session_id = sid, "创建新会话");
            config.session_id = Some(sid);
        }

        Ok(Self {
            config: Arc::new(Mutex::new(config)),
        })
    }

    /// 创建 evorule 会话
    ///
    /// N4：`auth_token` 设置后会携带 `Authorization: Bearer <token>` 头
    async fn create_session(server_url: &str, auth_token: Option<&str>) -> Result<u64, String> {
        let client = reqwest::Client::new();
        let url = format!("{}/api/sessions", server_url);

        let mut req = client.post(&url).json(&serde_json::json!({}));
        if let Some(token) = auth_token {
            req = req.header("Authorization", format!("Bearer {}", token));
        }

        let resp = req
            .send()
            .await
            .map_err(|e| format!("创建会话失败: {}", e))?;

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("解析响应失败: {}", e))?;

        body["session_id"]
            .as_u64()
            .ok_or_else(|| "会话 ID 无效".to_string())
    }

    /// 发送规则到 evorule-server
    ///
    /// N4：`auth_token` 设置后会携带 `Authorization: Bearer <token>` 头
    pub async fn send_rules(
        server_url: &str,
        session_id: u64,
        rules: &[serde_json::Value],
        auth_token: Option<&str>,
    ) -> Result<(), String> {
        let client = reqwest::Client::new();
        let url = format!("{}/api/sessions/{}/command", server_url, session_id);

        for rule in rules {
            let command = serde_json::json!({
                "instruction": {
                    "type": "transform",
                    "payload": rule.clone(),
                }
            });

            let mut req = client.post(&url).json(&command);
            if let Some(token) = auth_token {
                req = req.header("Authorization", format!("Bearer {}", token));
            }

            let resp = req
                .send()
                .await
                .map_err(|e| format!("发送规则失败: {}", e))?;

            if !resp.status().is_success() {
                return Err(format!("服务器返回错误: {}", resp.status()));
            }
        }

        Ok(())
    }

    /// 重载规则
    pub async fn reload_rules(&self) -> Result<usize, String> {
        // 在 await 之前获取配置并释放锁
        let (server_url, session_id, rules_dir, auth_token) = {
            let config = self.lock_config();
            (
                config.evorule_server_url.clone(),
                config.session_id,
                config.rules_dir.clone(),
                config.auth_token.clone(),
            )
        };

        let rules = load_rules(Path::new(&rules_dir))?;

        if rules.is_empty() {
            return Err("未找到任何规则文件".to_string());
        }

        let session_id = session_id.ok_or_else(|| "未设置会话 ID".to_string())?;

        Self::send_rules(&server_url, session_id, &rules, auth_token.as_deref()).await?;

        Ok(rules.len())
    }

    /// 启动文件监听
    pub async fn start(&self) -> Result<(), String> {
        let rules_dir = {
            let config = self.lock_config();
            config.rules_dir.clone()
        };
        let path = Path::new(&rules_dir);

        if !path.exists() {
            return Err(format!("规则目录不存在: {}", path.display()));
        }

        let rx = create_watcher(path).map_err(|e| format!("创建文件监听器失败: {}", e))?;

        let svc_clone = self.clone();

        task::spawn(async move {
            while let Ok(change) = rx.recv() {
                info!(path = %change.path, event = ?change.event_type, "检测到文件变化");

                // S1：删除规则文件不会从 server 移除已有规则
                if matches!(change.event_type, ChangeType::Remove) {
                    warn!(
                        path = %change.path,
                        "规则文件被删除。注意：hot_reload 仅支持增量添加规则，\
                         删除文件不会从 server 移除已有规则。如需清除旧规则，请重启 session。"
                    );
                }

                // 先获取配置（在 await 之前释放锁）
                let (server_url, session_id, rules_dir, auth_token) = {
                    let config = svc_clone.lock_config();
                    (
                        config.evorule_server_url.clone(),
                        config.session_id,
                        config.rules_dir.clone(),
                        config.auth_token.clone(),
                    )
                };

                let rules = match load_rules(Path::new(&rules_dir)) {
                    Ok(r) => r,
                    Err(e) => {
                        warn!(error = %e, "加载规则失败");
                        continue;
                    }
                };

                if rules.is_empty() {
                    continue;
                }

                if let Some(session_id) = session_id {
                    if let Err(e) =
                        Self::send_rules(&server_url, session_id, &rules, auth_token.as_deref())
                            .await
                    {
                        warn!(error = %e, "发送规则失败");
                    } else {
                        info!(count = rules.len(), "规则自动重载成功");
                    }
                }
            }
        });

        info!("热重载服务已启动");
        Ok(())
    }

    /// 获取配置（返回 Arc 副本）
    pub fn config(&self) -> Arc<Mutex<HotReloadConfig>> {
        self.config.clone()
    }

    /// 锁定 config，毒化时取回内部数据继续访问（不 panic）
    fn lock_config(&self) -> std::sync::MutexGuard<'_, HotReloadConfig> {
        match self.config.lock() {
            Ok(g) => g,
            Err(p) => {
                tracing::error!("config mutex 毒化,降级访问受污染数据");
                p.into_inner()
            }
        }
    }
}

// ============ HTTP API ============

/// handler 错误类型：状态码 + 错误消息
type HandlerError = (StatusCode, String);

/// `GET /status` — 服务状态 + 规则计数
async fn status_handler(State(svc): State<HotReloadService>) -> Json<serde_json::Value> {
    let config = svc.lock_config();
    let rules_dir = config.rules_dir.clone();
    let session_id = config.session_id;
    let rule_count = load_rules(Path::new(&rules_dir))
        .map(|r| r.len())
        .unwrap_or(0);
    Json(serde_json::json!({
        "status": "running",
        "rules_dir": rules_dir,
        "session_id": session_id,
        "rule_count": rule_count,
    }))
}

/// `POST /reload` — 手动触发规则重载
async fn reload_handler(
    State(svc): State<HotReloadService>,
) -> Result<Json<serde_json::Value>, HandlerError> {
    match svc.reload_rules().await {
        Ok(count) => Ok(Json(serde_json::json!({
            "success": true,
            "message": "规则重载成功",
            "rule_count": count,
        }))),
        Err(e) => Ok(Json(serde_json::json!({
            "success": false,
            "message": format!("规则重载失败: {e}"),
            "rule_count": 0,
        }))),
    }
}

/// `GET /rules` — 列出规则文件路径
async fn rules_handler(
    State(svc): State<HotReloadService>,
) -> Result<Json<serde_json::Value>, HandlerError> {
    let config = svc.lock_config();
    let files = loader::list_rule_files(Path::new(&config.rules_dir))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(serde_json::json!(files)))
}

/// 构建路由（公开以便测试）
pub fn build_router(service: HotReloadService) -> Router {
    Router::new()
        .route("/status", get(status_handler))
        .route("/reload", post(reload_handler))
        .route("/rules", get(rules_handler))
        .with_state(service)
}

/// 启动 HTTP API 服务器
pub async fn run_server(service: HotReloadService, addr: &str) -> Result<(), String> {
    let app = build_router(service);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("绑定地址 {addr} 失败: {e}"))?;
    info!("热重载 HTTP API 已启动 addr={addr}");
    const ENDPOINTS: &[&str] = &[
        "GET  /status  (服务状态 + 规则计数)",
        "POST /reload  (手动触发规则重载)",
        "GET  /rules   (列出规则文件路径)",
    ];
    for ep in ENDPOINTS {
        info!("  {ep}");
    }
    axum::serve(listener, app)
        .await
        .map_err(|e| format!("服务器错误: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use config::HotReloadConfig;
    use http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use loader::{list_rule_files, load_rule_file};
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;
    use tower::ServiceExt;
    use watcher::{ChangeType, FileChange};

    // ============ 辅助构造 ============

    /// 在临时目录中写入 JSON 文件
    fn write_json_file(dir: &TempDir, name: &str, content: &str) {
        let path = dir.path().join(name);
        fs::write(&path, content).expect("写入测试文件失败");
    }

    /// 一个通过 Schema 门禁的引擎原生规则（transform[]）
    fn engine_native_rule() -> String {
        r#"{"transform":[{"type":"set","params":{"attr":"x","operation":"set","value":1}}]}"#
            .to_string()
    }

    /// 构造带预设 session_id 的 service（不走网络创建会话）
    async fn new_service_with_session(rules_dir: String, server_url: String) -> HotReloadService {
        let config = HotReloadConfig {
            rules_dir,
            evorule_server_url: server_url,
            session_id: Some(42),
            auth_token: None,
            poll_interval_ms: 1000,
            auto_start: false,
        };
        HotReloadService::new(config)
            .await
            .expect("service 创建失败")
    }

    /// 辅助: 发送请求并返回 (status, body_text)
    async fn send_request(router: Router, req: Request<Body>) -> (StatusCode, String) {
        let response = router.oneshot(req).await.expect("oneshot failed");
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body collect failed")
            .to_bytes();
        (status, String::from_utf8_lossy(&body).to_string())
    }

    // ============ config 测试 ============

    #[test]
    fn test_default_config() {
        let config = HotReloadConfig::default();
        assert_eq!(config.rules_dir, "./rules");
        assert_eq!(config.evorule_server_url, "http://127.0.0.1:18080");
        assert_eq!(config.session_id, None);
        assert_eq!(config.poll_interval_ms, 1000);
        assert!(config.auto_start);
    }

    // ============ loader 测试 ============

    #[test]
    fn test_load_rules_multiple_files() {
        let dir = TempDir::new().expect("创建临时目录失败");
        write_json_file(&dir, "rule1.json", &engine_native_rule());
        write_json_file(&dir, "rule2.json", &engine_native_rule());

        let rules = load_rules(dir.path()).expect("加载应成功");
        assert_eq!(rules.len(), 2, "应加载 2 个规则文件");
    }

    #[test]
    fn test_load_rules_empty_dir() {
        let dir = TempDir::new().expect("创建临时目录失败");
        let rules = load_rules(dir.path()).expect("空目录不应报错");
        assert!(rules.is_empty(), "空目录返回空数组");
    }

    #[test]
    fn test_load_rules_nonexistent_dir() {
        let result = load_rules(std::path::Path::new("/nonexistent/path/xyz"));
        assert!(result.is_err(), "不存在的目录应报错");
        assert!(result.unwrap_err().contains("规则目录不存在"));
    }

    #[test]
    fn test_load_rules_skips_invalid_json() {
        let dir = TempDir::new().expect("创建临时目录失败");
        write_json_file(&dir, "valid.json", &engine_native_rule());
        write_json_file(&dir, "invalid.json", r#"{ bad json }"#);

        let rules = load_rules(dir.path()).expect("整体不应报错");
        assert_eq!(rules.len(), 1, "应只加载有效文件,跳过无效 JSON");
    }

    #[test]
    fn test_load_rules_ignores_non_json_files() {
        let dir = TempDir::new().expect("创建临时目录失败");
        write_json_file(&dir, "rule.json", &engine_native_rule());
        // 非 JSON 文件应被忽略
        fs::write(dir.path().join("readme.txt"), "not a rule").expect("写入失败");
        fs::write(dir.path().join("config.yaml"), "key: value").expect("写入失败");

        let rules = load_rules(dir.path()).expect("加载应成功");
        assert_eq!(rules.len(), 1, "应只加载 .json 文件");
    }

    #[test]
    fn test_load_rules_recursive_includes_bundle_subdir() {
        let dir = TempDir::new().expect("创建临时目录失败");
        write_json_file(&dir, "top.json", &engine_native_rule());
        let bundle_dir = dir.path().join("bundles/bundle-1");
        fs::create_dir_all(&bundle_dir).expect("创建 bundle 目录失败");
        fs::write(bundle_dir.join("entry.json"), engine_native_rule()).expect("写入失败");
        // bundle_manifest.json 应被排除（不当作规则文件解析）
        fs::write(
            bundle_dir.join(evorule_bundle::BUNDLE_MANIFEST_FILE),
            r#"{"bundle_id":"bundle-1","entry_files":[]}"#,
        )
        .expect("写入失败");

        let rules = load_rules(dir.path()).expect("加载应成功");
        assert_eq!(
            rules.len(),
            2,
            "递归应加载顶层 + bundle 子目录条目，排除 manifest"
        );
    }

    #[test]
    fn test_list_rule_files_recursive() {
        let dir = TempDir::new().expect("创建临时目录失败");
        write_json_file(&dir, "top.json", &engine_native_rule());
        let bundle_dir = dir.path().join("bundles/bundle-1");
        fs::create_dir_all(&bundle_dir).expect("创建 bundle 目录失败");
        fs::write(bundle_dir.join("entry.json"), engine_native_rule()).expect("写入失败");
        fs::write(
            bundle_dir.join(evorule_bundle::BUNDLE_MANIFEST_FILE),
            r#"{}"#,
        )
        .expect("写入失败");

        let files = list_rule_files(dir.path()).expect("读取规则目录失败");
        assert_eq!(files.len(), 2, "递归列出应排除 manifest");
        assert!(
            files
                .iter()
                .any(|f| f.ends_with("bundles\\bundle-1\\entry.json")
                    || f.ends_with("bundles/bundle-1/entry.json")),
            "应包含 bundle 子目录条目: {files:?}"
        );
    }

    #[test]
    fn test_load_rule_file_valid() {
        let dir = TempDir::new().expect("创建临时目录失败");
        let path = dir.path().join("rule.json");
        fs::write(&path, r#"{"name": "test"}"#).expect("写入失败");

        let rule = load_rule_file(&path).expect("应成功解析");
        assert_eq!(rule["name"], "test");
    }

    #[test]
    fn test_load_rule_file_invalid_json() {
        let dir = TempDir::new().expect("创建临时目录失败");
        let path = dir.path().join("bad.json");
        fs::write(&path, r#"{ broken }"#).expect("写入失败");

        let err = load_rule_file(&path).expect_err("应报错");
        assert!(err.contains("JSON 解析失败"));
    }

    #[test]
    fn test_load_rule_file_nonexistent() {
        let err =
            load_rule_file(std::path::Path::new("/nonexistent/file.json")).expect_err("应报错");
        assert!(err.contains("读取文件失败"));
    }

    #[test]
    fn test_list_rule_files() {
        let dir = TempDir::new().expect("创建临时目录失败");
        write_json_file(&dir, "a.json", "{}");
        write_json_file(&dir, "b.json", "{}");
        fs::write(dir.path().join("c.txt"), "ignored").expect("写入失败");

        let files = list_rule_files(dir.path()).expect("读取规则目录失败");
        assert_eq!(files.len(), 2, "应只列出 .json 文件");
    }

    // ============ watcher 测试 ============

    #[test]
    fn test_file_change_from_create_event() {
        let event = notify::Event::new(notify::EventKind::Create(notify::event::CreateKind::Any))
            .add_path(PathBuf::from("/tmp/rule.json"));
        let change = FileChange::from(event);
        assert!(matches!(change.event_type, ChangeType::Create));
        assert_eq!(change.path, "/tmp/rule.json");
    }

    #[test]
    fn test_file_change_from_modify_event() {
        let event = notify::Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Any))
            .add_path(PathBuf::from("/tmp/rule.json"));
        let change = FileChange::from(event);
        assert!(matches!(change.event_type, ChangeType::Modify));
    }

    #[test]
    fn test_file_change_from_remove_event() {
        let event = notify::Event::new(notify::EventKind::Remove(notify::event::RemoveKind::Any))
            .add_path(PathBuf::from("/tmp/rule.json"));
        let change = FileChange::from(event);
        assert!(matches!(change.event_type, ChangeType::Remove));
    }

    #[test]
    fn test_file_change_from_other_event_defaults_to_modify() {
        // EventKind::Any 不是 create/modify/remove → 默认 Modify
        let event =
            notify::Event::new(notify::EventKind::Any).add_path(PathBuf::from("/tmp/rule.json"));
        let change = FileChange::from(event);
        assert!(matches!(change.event_type, ChangeType::Modify));
    }

    #[test]
    fn test_file_change_with_no_path() {
        let event = notify::Event::new(notify::EventKind::Create(notify::event::CreateKind::Any));
        let change = FileChange::from(event);
        assert_eq!(change.path, "", "无路径时 path 为空字符串");
    }

    // ============ 服务层 HTTP 测试（mockito） ============

    #[tokio::test]
    async fn test_new_with_existing_session_id() {
        // 已有 session_id → 不调 create_session,不需要 mock
        let config = HotReloadConfig {
            rules_dir: "./rules".to_string(),
            evorule_server_url: "http://127.0.0.1:1".to_string(),
            session_id: Some(99),
            auth_token: None,
            poll_interval_ms: 1000,
            auto_start: false,
        };
        let svc = HotReloadService::new(config)
            .await
            .expect("已有 session_id 时应直接成功");
        let config = svc.lock_config();
        assert_eq!(config.session_id, Some(99));
    }

    #[tokio::test]
    async fn test_new_creates_session() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/api/sessions")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(serde_json::json!({"session_id": 42}).to_string())
            .create_async()
            .await;

        let config = HotReloadConfig {
            rules_dir: "./rules".to_string(),
            evorule_server_url: server.url(),
            session_id: None,
            auth_token: None,
            poll_interval_ms: 1000,
            auto_start: false,
        };
        let svc = HotReloadService::new(config).await.expect("应成功创建会话");
        let config = svc.lock_config();
        assert_eq!(config.session_id, Some(42));
    }

    #[tokio::test]
    async fn test_new_create_session_error() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/api/sessions")
            .with_status(500)
            .create_async()
            .await;

        let config = HotReloadConfig {
            rules_dir: "./rules".to_string(),
            evorule_server_url: server.url(),
            session_id: None,
            auth_token: None,
            poll_interval_ms: 1000,
            auto_start: false,
        };
        let err = HotReloadService::new(config)
            .await
            .expect_err("上游 500 应报错");
        // body 解析失败或 session_id 无效
        assert!(
            err.contains("解析响应失败") || err.contains("会话 ID 无效"),
            "实际错误: {err}"
        );
    }

    #[tokio::test]
    async fn test_send_rules_success() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/api/sessions/42/command")
            .with_status(200)
            .create_async()
            .await;

        let rules = vec![serde_json::json!({"name": "rule1"})];
        HotReloadService::send_rules(&server.url(), 42, &rules, None)
            .await
            .expect("发送应成功");
    }

    #[tokio::test]
    async fn test_send_rules_upstream_error() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/api/sessions/42/command")
            .with_status(500)
            .create_async()
            .await;

        let rules = vec![serde_json::json!({"name": "rule1"})];
        let err = HotReloadService::send_rules(&server.url(), 42, &rules, None)
            .await
            .expect_err("上游 500 应报错");
        assert!(err.contains("服务器返回错误"), "实际错误: {err}");
    }

    #[tokio::test]
    async fn test_reload_rules_success() {
        let dir = TempDir::new().expect("创建临时目录失败");
        write_json_file(&dir, "rule1.json", &engine_native_rule());
        write_json_file(&dir, "rule2.json", &engine_native_rule());

        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/api/sessions/42/command")
            .with_status(200)
            .expect(2) // 2 条规则 = 2 次请求
            .create_async()
            .await;

        let svc =
            new_service_with_session(dir.path().to_string_lossy().to_string(), server.url()).await;

        let count = svc.reload_rules().await.expect("重载应成功");
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn test_reload_rules_no_rules_found() {
        let dir = TempDir::new().expect("创建临时目录失败");
        // 空目录,无规则文件

        let svc = new_service_with_session(
            dir.path().to_string_lossy().to_string(),
            "http://127.0.0.1:1".to_string(),
        )
        .await;

        let err = svc.reload_rules().await.expect_err("无规则应报错");
        assert!(err.contains("未找到任何规则文件"));
    }

    // ============ handler oneshot 测试 ============

    #[tokio::test]
    async fn test_handler_status() {
        let dir = TempDir::new().expect("创建临时目录失败");
        write_json_file(&dir, "rule1.json", &engine_native_rule());
        write_json_file(&dir, "rule2.json", &engine_native_rule());

        let svc = new_service_with_session(
            dir.path().to_string_lossy().to_string(),
            "http://127.0.0.1:1".to_string(),
        )
        .await;
        let router = build_router(svc);

        let (status, body) = send_request(
            router,
            Request::builder()
                .method("GET")
                .uri("/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"status\":\"running\""));
        assert!(body.contains("\"session_id\":42"));
        assert!(body.contains("\"rule_count\":2"));
    }

    #[tokio::test]
    async fn test_handler_reload_success() {
        let dir = TempDir::new().expect("创建临时目录失败");
        write_json_file(&dir, "rule1.json", &engine_native_rule());

        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/api/sessions/42/command")
            .with_status(200)
            .create_async()
            .await;

        let svc =
            new_service_with_session(dir.path().to_string_lossy().to_string(), server.url()).await;
        let router = build_router(svc);

        let (status, body) = send_request(
            router,
            Request::builder()
                .method("POST")
                .uri("/reload")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"success\":true"));
        assert!(body.contains("\"rule_count\":1"));
    }

    #[tokio::test]
    async fn test_handler_reload_no_rules() {
        let dir = TempDir::new().expect("创建临时目录失败");
        // 空目录

        let svc = new_service_with_session(
            dir.path().to_string_lossy().to_string(),
            "http://127.0.0.1:1".to_string(),
        )
        .await;
        let router = build_router(svc);

        let (status, body) = send_request(
            router,
            Request::builder()
                .method("POST")
                .uri("/reload")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"success\":false"));
        assert!(body.contains("未找到任何规则文件"));
    }

    #[tokio::test]
    async fn test_handler_rules() {
        let dir = TempDir::new().expect("创建临时目录失败");
        write_json_file(&dir, "a.json", "{}");
        write_json_file(&dir, "b.json", "{}");
        fs::write(dir.path().join("c.txt"), "ignored").expect("写入失败");

        let svc = new_service_with_session(
            dir.path().to_string_lossy().to_string(),
            "http://127.0.0.1:1".to_string(),
        )
        .await;
        let router = build_router(svc);

        let (status, body) = send_request(
            router,
            Request::builder()
                .method("GET")
                .uri("/rules")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("a.json"));
        assert!(body.contains("b.json"));
        assert!(!body.contains("c.txt"), "不应包含非 JSON 文件");
    }
}

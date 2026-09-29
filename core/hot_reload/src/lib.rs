// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 规则文件热重载服务
//!
//! 监听规则目录变化，触发 evorule-server 规则重载，使规则对新会话生效
//!
//! # 设计
//! - 使用 notify crate 监听文件系统变化
//! - 通过 HTTP API（`POST /api/rules/reload`）触发 evorule-server 重载规则
//! - 支持手动触发重载和自动监听两种模式
//! - 语义为「目录为准全量重载」：规则对新会话生效，已存在会话保持不变
//!   （TCB 不可变语义）
//!
//! # 端点
//!
//! - `GET  /status` — 服务状态 + 规则计数
//! - `POST /reload` — 手动触发规则重载
//! - `GET  /rules`  — 列出规则文件路径
//!
//! # 使用约束
//!
//! `rules_dir` 必须指向 evorule-server 的 `--rules-dir`（服务端 reload 从
//! 该目录合并加载规则），否则监听到的变化不会反映到服务端规则集。
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
    pub async fn new(config: HotReloadConfig) -> Result<Self, String> {
        Ok(Self {
            config: Arc::new(Mutex::new(config)),
        })
    }

    /// 触发 evorule-server 规则重载（`POST /api/rules/reload`）
    ///
    /// N4：`auth_token` 设置后会携带 `Authorization: Bearer <token>` 头
    ///
    /// 返回 `(previous_rules, current_rules)`——服务端重载前后的规则总数
    /// （TCB 宪法 + rules_dir 业务规则合并后的数量）。
    pub async fn trigger_reload(
        server_url: &str,
        auth_token: Option<&str>,
    ) -> Result<(usize, usize), String> {
        let client = reqwest::Client::new();
        let url = format!("{}/api/rules/reload", server_url);

        let mut req = client.post(&url);
        if let Some(token) = auth_token {
            req = req.header("Authorization", format!("Bearer {}", token));
        }

        let resp = req
            .send()
            .await
            .map_err(|e| format!("触发规则重载失败: {}", e))?;

        let status = resp.status();
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("解析重载响应失败: {}", e))?;

        if !status.is_success() {
            let detail = body["error"].as_str().unwrap_or("unknown");
            return Err(format!("服务器返回错误: {} ({detail})", status));
        }

        let previous = body["previous_rules"].as_u64().unwrap_or(0) as usize;
        let current = body["current_rules"].as_u64().unwrap_or(0) as usize;
        Ok((previous, current))
    }

    /// 重载规则（手动触发入口）
    ///
    /// 校验规则目录存在后触发服务端 reload；规则为「目录为准全量重载」，
    /// 空目录同样允许触发（用于清空业务规则，仅保留 TCB 宪法）。
    pub async fn reload_rules(&self) -> Result<usize, String> {
        // 在 await 之前获取配置并释放锁
        let (server_url, rules_dir, auth_token) = {
            let config = self.lock_config();
            (
                config.evorule_server_url.clone(),
                config.rules_dir.clone(),
                config.auth_token.clone(),
            )
        };

        let path = Path::new(&rules_dir);
        if !path.exists() {
            return Err(format!("规则目录不存在: {}", path.display()));
        }

        let (_, current) = Self::trigger_reload(&server_url, auth_token.as_deref()).await?;

        Ok(current)
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

        // watcher 必须移入 task 保活——drop 后停止监听（notify 语义）
        let (watcher, rx) =
            create_watcher(path).map_err(|e| format!("创建文件监听器失败: {}", e))?;

        let svc_clone = self.clone();

        task::spawn(async move {
            // 保活绑定：_watcher 持有至 task 结束
            let _watcher = watcher;
            while let Ok(change) = rx.recv() {
                info!(path = %change.path, event = ?change.event_type, "检测到文件变化");

                if matches!(change.event_type, ChangeType::Remove) {
                    info!(
                        path = %change.path,
                        "规则文件被删除。重载后该规则对新会话移除；\
                         已存在会话保持不变（TCB 不可变语义）。"
                    );
                }

                // 先获取配置（在 await 之前释放锁）
                let (server_url, auth_token) = {
                    let config = svc_clone.lock_config();
                    (config.evorule_server_url.clone(), config.auth_token.clone())
                };

                match Self::trigger_reload(&server_url, auth_token.as_deref()).await {
                    Ok((previous, current)) => {
                        info!(
                            previous_rules = previous,
                            current_rules = current,
                            "规则自动重载成功"
                        );
                    }
                    Err(e) => {
                        warn!(error = %e, "触发规则重载失败");
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
    let server_url = config.evorule_server_url.clone();
    let rule_count = load_rules(Path::new(&rules_dir))
        .map(|r| r.len())
        .unwrap_or(0);
    Json(serde_json::json!({
        "status": "running",
        "rules_dir": rules_dir,
        "evorule_server_url": server_url,
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

    /// 构造预设配置的 service（不走网络）
    async fn new_service(rules_dir: String, server_url: String) -> HotReloadService {
        let config = HotReloadConfig {
            rules_dir,
            evorule_server_url: server_url,
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

    /// 构造 server /api/rules/reload 成功响应体
    fn reload_ok_body(previous: u64, current: u64) -> String {
        serde_json::json!({
            "reload_ok": true,
            "previous_rules": previous,
            "current_rules": current,
            "error": null,
            "rejected_overlaps": [],
        })
        .to_string()
    }

    #[tokio::test]
    async fn test_new_without_session() {
        // reload 通路不需要会话——config 直接生效
        let config = HotReloadConfig {
            rules_dir: "./rules".to_string(),
            evorule_server_url: "http://127.0.0.1:1".to_string(),
            auth_token: None,
            poll_interval_ms: 1000,
            auto_start: false,
        };
        let svc = HotReloadService::new(config)
            .await
            .expect("service 创建应直接成功");
        let config = svc.lock_config();
        assert_eq!(config.evorule_server_url, "http://127.0.0.1:1");
    }

    #[tokio::test]
    async fn test_trigger_reload_success() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/api/rules/reload")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(reload_ok_body(10, 12))
            .create_async()
            .await;

        let (previous, current) = HotReloadService::trigger_reload(&server.url(), None)
            .await
            .expect("触发重载应成功");
        assert_eq!(previous, 10);
        assert_eq!(current, 12);
    }

    #[tokio::test]
    async fn test_trigger_reload_upstream_error() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/api/rules/reload")
            .with_status(500)
            .with_header("content-type", "application/json")
            .with_body(
                serde_json::json!({
                    "reload_ok": false,
                    "previous_rules": 10,
                    "current_rules": 10,
                    "error": "解析失败",
                    "rejected_overlaps": [],
                })
                .to_string(),
            )
            .create_async()
            .await;

        let err = HotReloadService::trigger_reload(&server.url(), None)
            .await
            .expect_err("上游 500 应报错");
        assert!(err.contains("服务器返回错误"), "实际错误: {err}");
        assert!(err.contains("解析失败"), "错误应携带服务端 detail: {err}");
    }

    #[tokio::test]
    async fn test_reload_rules_success() {
        let dir = TempDir::new().expect("创建临时目录失败");
        write_json_file(&dir, "rule1.json", &engine_native_rule());
        write_json_file(&dir, "rule2.json", &engine_native_rule());

        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/api/rules/reload")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(reload_ok_body(10, 12))
            .create_async()
            .await;

        let svc = new_service(dir.path().to_string_lossy().to_string(), server.url()).await;

        // 返回服务端重载后的规则总数（current_rules）
        let count = svc.reload_rules().await.expect("重载应成功");
        assert_eq!(count, 12);
    }

    #[tokio::test]
    async fn test_reload_rules_empty_dir_allowed() {
        // 目录为准全量重载：空目录允许触发（用于清空业务规则）
        let dir = TempDir::new().expect("创建临时目录失败");

        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/api/rules/reload")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(reload_ok_body(10, 10))
            .create_async()
            .await;

        let svc = new_service(dir.path().to_string_lossy().to_string(), server.url()).await;

        let count = svc.reload_rules().await.expect("空目录重载应成功");
        assert_eq!(count, 10);
    }

    #[tokio::test]
    async fn test_reload_rules_nonexistent_dir() {
        let svc = new_service(
            "Z:\\nonexistent\\path\\xyz".to_string(),
            "http://127.0.0.1:1".to_string(),
        )
        .await;

        let err = svc.reload_rules().await.expect_err("目录不存在应报错");
        assert!(err.contains("规则目录不存在"), "实际错误: {err}");
    }

    // ============ handler oneshot 测试 ============

    #[tokio::test]
    async fn test_handler_status() {
        let dir = TempDir::new().expect("创建临时目录失败");
        write_json_file(&dir, "rule1.json", &engine_native_rule());
        write_json_file(&dir, "rule2.json", &engine_native_rule());

        let svc = new_service(
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
        assert!(body.contains("\"rule_count\":2"));
    }

    #[tokio::test]
    async fn test_handler_reload_success() {
        let dir = TempDir::new().expect("创建临时目录失败");
        write_json_file(&dir, "rule1.json", &engine_native_rule());

        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/api/rules/reload")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(reload_ok_body(10, 11))
            .create_async()
            .await;

        let svc = new_service(dir.path().to_string_lossy().to_string(), server.url()).await;
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
        assert!(body.contains("\"rule_count\":11"));
    }

    #[tokio::test]
    async fn test_handler_reload_server_error() {
        let dir = TempDir::new().expect("创建临时目录失败");
        write_json_file(&dir, "rule1.json", &engine_native_rule());

        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/api/rules/reload")
            .with_status(500)
            .with_header("content-type", "application/json")
            .with_body(
                serde_json::json!({
                    "reload_ok": false,
                    "previous_rules": 10,
                    "current_rules": 10,
                    "error": "宪法解析失败",
                    "rejected_overlaps": [],
                })
                .to_string(),
            )
            .create_async()
            .await;

        let svc = new_service(dir.path().to_string_lossy().to_string(), server.url()).await;
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

        // handler 层不向上传播错误，统一 200 + success:false（原有契约）
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"success\":false"));
        assert!(body.contains("规则重载失败"));
    }

    #[tokio::test]
    async fn test_handler_rules() {
        let dir = TempDir::new().expect("创建临时目录失败");
        write_json_file(&dir, "a.json", "{}");
        write_json_file(&dir, "b.json", "{}");
        fs::write(dir.path().join("c.txt"), "ignored").expect("写入失败");

        let svc = new_service(
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

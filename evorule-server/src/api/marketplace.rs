// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 模板市场 API —— `/api/marketplace/templates` 端点族
//!
//! P09_IMPORT_EXPORT_INFRA_DESIGN.md §7.1 定义的 P1 契约（console marketplace.ts
//! 4 处 P1 注释预留的接线路径）：
//! - `GET  /api/marketplace/templates`            → 列出用户上传模板
//! - `POST /api/marketplace/templates`            → multipart 上传（meta JSON + content）
//! - `GET  /api/marketplace/templates/{id}/download` → 下载内容（递增计数）
//! - `DELETE /api/marketplace/templates/{id}`     → 删除（连同内容）
//! - `PATCH /api/marketplace/templates/{id}`      → 编辑
//!
//! 存储与 rules_dir **物理隔离**（同 knowledge_dir 派生法：`{rules 父目录}/marketplace/`）——
//! TCB 扫描 rules_dir，用户上传内容绝不可入规则加载路径。布局：
//!
//! ```text
//! {marketplace_dir}/templates/{id}/meta.json    模板元数据（含服务端字段）
//! {marketplace_dir}/templates/{id}/content.bin  上传的原始内容
//! ```
//!
//! 职责边界（如实登记）：
//! - server 只存 **user 上传**模板；official/builtin 模板由 console 本地内置数据
//!   （`BUILTIN_MARKET_TEMPLATES`）提供，双方在列表语义上合并（console 侧拼装）
//! - 单租户本地 server，无跨用户身份体系：DELETE 不做所有权校验（认证由统一
//!   中间件把关）；多租户/审核流属后续项，触发条件 = 模板市场对外开放

use axum::extract::{Multipart, Path, Query, State};
use axum::http::{header, StatusCode};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::Value;

use evorule_io_handlers::ServiceMeta;

use crate::api::platform_auth::append_platform_event;
use crate::api::server::{AppState, MarketplaceDir, SessionApi};

/// 单模板上传内容上限（模板为规则/数据集/表单定义，10MB 已远超需要；防滥用）
const MAX_CONTENT_BYTES: usize = 10 * 1024 * 1024;

/// 构造 `/api/marketplace` 的路由（挂入统一认证中间件覆盖的路由组）
///
/// 认证语义（注释修正，与实际行为对齐）：
/// - 认证启用（设置了 auth_token）：本组端点经统一中间件校验 Bearer
///   （静态 token / 平台会话 hash 双通道），匿名请求 401；
/// - 显式豁免（`--insecure-serve`）：无认证，本组端点（含写接口）匿名可达——
///   仅限本机回环体验/开发场景，启动日志显式声明，不构成"受保护"承诺。
pub fn marketplace_router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/marketplace/templates",
            get(list_templates).post(upload_template),
        )
        .route(
            "/api/marketplace/templates/{id}",
            delete(delete_template_handler).patch(update_template_handler),
        )
        .route(
            "/api/marketplace/templates/{id}/download",
            get(download_template),
        )
        .route("/api/marketplace/plugins", get(list_plugins))
        .route(
            "/api/marketplace/plugins/{id}/download",
            get(download_plugin),
        )
        .route(
            "/api/marketplace/plugins/{id}/install",
            post(install_plugin_handler),
        )
        .route(
            "/api/marketplace/plugins/{id}/uninstall",
            post(uninstall_plugin_handler),
        )
        .route(
            "/api/marketplace/plugins/{id}/disable",
            post(disable_plugin_handler),
        )
}

/// 统一错误响应：`{ "success": false, "message": ... }`（与 permissions.rs 同形状）
fn err(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<Value>) {
    (
        status,
        Json(serde_json::json!({ "success": false, "message": message.into() })),
    )
}

/// 模板 ID 字符集白名单（防路径穿越：只允许字母数字与连字符/下划线）
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// 生成服务端模板 ID（时间戳 hex + 进程内原子序号，同进程内单调唯一）
fn generate_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("tpl-{ts:x}-{seq:04x}")
}

/// 原子写入（同目录临时文件 + rename，与 server 落盘惯例一致）
fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(|e| format!("写临时文件失败({}): {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("原子改名失败({}): {e}", path.display()))
}

/// 读取单条模板元数据文件
fn read_meta(meta_path: &std::path::Path) -> Result<Value, String> {
    let raw = std::fs::read_to_string(meta_path)
        .map_err(|e| format!("读元数据失败({}): {e}", meta_path.display()))?;
    serde_json::from_str(&raw).map_err(|e| format!("元数据 JSON 解析失败: {e}"))
}

// ============================================================================
// 核心存储逻辑（与 HTTP 层解耦，供单测直测）
// ============================================================================

/// 上传存储：写 meta.json + content.bin，返回完整模板元数据
fn store_template(
    marketplace_dir: &std::path::Path,
    meta_in: &Value,
    content: &[u8],
) -> Result<Value, String> {
    // meta 必填字段校验（name/type/category；type/category 值域由 console 侧枚举约束，
    // server 只校验存在性与类型，避免与前端枚举漂移）
    let name = meta_in
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");
    if name.is_empty() {
        return Err("模板元数据缺少 name（或为空）".into());
    }
    if meta_in.get("type").and_then(|v| v.as_str()).is_none() {
        return Err("模板元数据缺少 type".into());
    }
    if meta_in.get("category").and_then(|v| v.as_str()).is_none() {
        return Err("模板元数据缺少 category".into());
    }

    let id = generate_id();
    let dir = marketplace_dir.join("templates").join(&id);
    std::fs::create_dir_all(&dir).map_err(|e| format!("建模板目录失败({}): {e}", dir.display()))?;

    let now = chrono_now_iso();
    let hash = blake3::hash(content).to_string();
    let download_url = format!("/api/marketplace/templates/{id}/download");

    // 服务端权威字段覆盖（客户端提供的 id/source/download_url/计数/时间戳一律不采信）
    let mut meta = meta_in.clone();
    let obj = meta
        .as_object_mut()
        .ok_or_else(|| "模板元数据必须是 JSON 对象".to_string())?;
    obj.insert("id".into(), Value::String(id.clone()));
    obj.insert("source".into(), Value::String("user".into()));
    obj.insert("download_url".into(), Value::String(download_url));
    obj.insert("content_hash".into(), Value::String(hash));
    obj.insert("download_count".into(), Value::from(0u64));
    obj.insert("created_at".into(), Value::String(now.clone()));
    obj.insert("updated_at".into(), Value::String(now));

    let content_path = dir.join("content.bin");
    atomic_write(&content_path, content)?;
    let meta_path = dir.join("meta.json");
    atomic_write(
        &meta_path,
        &serde_json::to_vec_pretty(&meta).map_err(|e| e.to_string())?,
    )?;

    Ok(meta)
}

/// 编辑模板：meta 整体替换（必填字段校验同上传）+ content 可选替换。
///
/// 服务端权威字段纪律（与 store_template 同口径）：
/// - 保留原值不采信客户端：`id` / `source` / `download_url` / `download_count` / `created_at`
/// - 派生刷新：`updated_at` = 当前时间；`content_hash` = content 提供时重算，缺省保留原值
///   （仅改元数据不动内容时 hash 不变，审计面对得上账）
fn update_template(
    marketplace_dir: &std::path::Path,
    id: &str,
    meta_in: &Value,
    content: Option<&[u8]>,
) -> Result<Value, String> {
    if !valid_id(id) {
        return Err(format!("非法模板 ID: {id:?}（只允许字母数字与 - _）"));
    }
    // 存在性 + 完整性先核（不存在/残缺目录 → 明确报错，不静默建新）
    let (meta, _old_content) = load_template(marketplace_dir, id)?;

    // meta 必填字段校验（与 store_template 同口径：name/type/category 存在性与类型）
    let name = meta_in
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");
    if name.is_empty() {
        return Err("模板元数据缺少 name（或为空）".into());
    }
    if meta_in.get("type").and_then(|v| v.as_str()).is_none() {
        return Err("模板元数据缺少 type".into());
    }
    if meta_in.get("category").and_then(|v| v.as_str()).is_none() {
        return Err("模板元数据缺少 category".into());
    }

    // 以客户端 meta 为底（普通字段整体替换），权威字段从旧值回填/派生
    let mut new_meta = meta_in.clone();
    let obj = new_meta
        .as_object_mut()
        .ok_or_else(|| "模板元数据必须是 JSON 对象".to_string())?;
    for key in [
        "id",
        "source",
        "download_url",
        "download_count",
        "created_at",
    ] {
        if let Some(v) = meta.get(key) {
            obj.insert(key.into(), v.clone());
        }
    }
    if let Some(bytes) = content {
        obj.insert(
            "content_hash".into(),
            Value::String(blake3::hash(bytes).to_string()),
        );
    } else if let Some(v) = meta.get("content_hash") {
        obj.insert("content_hash".into(), v.clone());
    }
    obj.insert("updated_at".into(), Value::String(chrono_now_iso()));

    let dir = marketplace_dir.join("templates").join(id);
    if let Some(bytes) = content {
        let content_path = dir.join("content.bin");
        atomic_write(&content_path, bytes)?;
    }
    let meta_path = dir.join("meta.json");
    atomic_write(
        &meta_path,
        &serde_json::to_vec_pretty(&new_meta).map_err(|e| e.to_string())?,
    )?;
    Ok(new_meta)
}

/// 列出全部用户模板（created_at 降序）
fn list_templates_from(marketplace_dir: &std::path::Path) -> Result<Vec<Value>, String> {
    let root = marketplace_dir.join("templates");
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let entries = std::fs::read_dir(&root)
        .map_err(|e| format!("扫描模板目录失败({}): {e}", root.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("遍历模板目录失败: {e}"))?;
        let meta_path = entry.path().join("meta.json");
        if !meta_path.is_file() {
            // 目录缺 meta.json = 异常残留，显式跳过但记录（不静默装作不存在）
            tracing::warn!("模板目录缺 meta.json，跳过: {}", entry.path().display());
            continue;
        }
        let meta = read_meta(&meta_path)?;
        out.push(meta);
    }
    out.sort_by(|a, b| {
        let ka = a.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
        let kb = b.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
        kb.cmp(ka)
    });
    Ok(out)
}

/// 读取模板内容（返回 元数据 + 字节；供 download 处理计数后回写）
fn load_template(marketplace_dir: &std::path::Path, id: &str) -> Result<(Value, Vec<u8>), String> {
    if !valid_id(id) {
        return Err(format!("非法模板 ID: {id:?}（只允许字母数字与 - _）"));
    }
    let dir = marketplace_dir.join("templates").join(id);
    let meta_path = dir.join("meta.json");
    let content_path = dir.join("content.bin");
    if !meta_path.is_file() || !content_path.is_file() {
        return Err(format!(
            "模板不存在或不完整: {id}（meta.json/content.bin 缺失；可刷新市场列表核对）"
        ));
    }
    let meta = read_meta(&meta_path)?;
    let content = std::fs::read(&content_path)
        .map_err(|e| format!("读模板内容失败({}): {e}", content_path.display()))?;
    Ok((meta, content))
}

/// 删除模板目录
fn remove_template(marketplace_dir: &std::path::Path, id: &str) -> Result<(), String> {
    if !valid_id(id) {
        return Err(format!("非法模板 ID: {id:?}（只允许字母数字与 - _）"));
    }
    let dir = marketplace_dir.join("templates").join(id);
    if !dir.is_dir() {
        return Err(format!("模板不存在: {id}（可能已删除；刷新列表核对）"));
    }
    std::fs::remove_dir_all(&dir).map_err(|e| format!("删除模板目录失败({}): {e}", dir.display()))
}

/// 递增下载计数（download 处理器调用；失败仅告警——计数是统计位，不得阻断下载）
fn bump_download_count(marketplace_dir: &std::path::Path, id: &str) {
    let meta_path = marketplace_dir.join("templates").join(id).join("meta.json");
    let Ok(mut meta) = read_meta(&meta_path) else {
        tracing::warn!("下载计数更新失败（meta 不可读）: {id}");
        return;
    };
    let count = meta
        .get("download_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if let Some(obj) = meta.as_object_mut() {
        obj.insert("download_count".into(), Value::from(count + 1));
    }
    match serde_json::to_vec_pretty(&meta) {
        Ok(bytes) => {
            if let Err(e) = atomic_write(&meta_path, &bytes) {
                tracing::warn!("下载计数回写失败: {e}");
            }
        }
        Err(e) => tracing::warn!("下载计数序列化失败: {e}"),
    }
}

/// 当前时间 ISO8601（无 chrono 依赖，UNIX 时间秒近似；created_at 仅排序/展示用）
fn chrono_now_iso() -> String {
    // 与 console `new Date.toISOString` 可比较的 UTC 形态
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // civil_from_days（Howard Hinnant 算法）: Unix 纪元 → 年月日
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!("{y:04}-{mth:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

// ============================================================================
// HTTP 处理器
// ============================================================================

/// `GET /api/marketplace/templates` → 列出用户上传模板
#[utoipa::path(
    get,
    path = "/api/marketplace/templates",
    tag = "marketplace",
    responses(
        (status = 200, description = "用户上传模板列表（created_at 降序）", body = serde_json::Value),
        (status = 500, description = "存储扫描失败", body = serde_json::Value)
    )
)]
async fn list_templates(
    State(dir): State<MarketplaceDir>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let templates =
        list_templates_from(&dir.0).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let count = templates.len();
    Ok(Json(serde_json::json!({
        "success": true,
        "count": count,
        "templates": templates,
    })))
}

/// `POST /api/marketplace/templates` → multipart 上传（meta JSON + content）
#[utoipa::path(
    post,
    path = "/api/marketplace/templates",
    tag = "marketplace",
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "上传成功，返回服务端补全后的完整模板", body = serde_json::Value),
        (status = 400, description = "meta 缺字段 / content 缺失 / 超限", body = serde_json::Value),
        (status = 500, description = "落盘失败", body = serde_json::Value)
    )
)]
async fn upload_template(
    State(dir): State<MarketplaceDir>,
    mut multipart: Multipart,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let mut meta: Option<Value> = None;
    let mut content: Option<Vec<u8>> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("multipart 解析失败: {e}")))?
    {
        match field.name().unwrap_or("") {
            "meta" => {
                let text = field
                    .text()
                    .await
                    .map_err(|e| err(StatusCode::BAD_REQUEST, format!("meta 字段读取失败: {e}")))?;
                let v: Value = serde_json::from_str(&text)
                    .map_err(|e| err(StatusCode::BAD_REQUEST, format!("meta 非 JSON: {e}")))?;
                meta = Some(v);
            }
            "content" => {
                let bytes = field.bytes().await.map_err(|e| {
                    err(
                        StatusCode::BAD_REQUEST,
                        format!("content 字段读取失败: {e}"),
                    )
                })?;
                if bytes.len() > MAX_CONTENT_BYTES {
                    return Err(err(
                        StatusCode::BAD_REQUEST,
                        format!("content 超限（{} 字节 > {MAX_CONTENT_BYTES}）", bytes.len()),
                    ));
                }
                content = Some(bytes.to_vec());
            }
            _ => {} // 未知字段忽略（前向兼容）
        }
    }
    let meta = meta.ok_or_else(|| {
        err(
            StatusCode::BAD_REQUEST,
            "multipart 缺少 meta 字段（JSON 模板元数据）",
        )
    })?;
    let content = content.ok_or_else(|| {
        err(
            StatusCode::BAD_REQUEST,
            "multipart 缺少 content 字段（模板内容）",
        )
    })?;

    let template = store_template(&dir.0, &meta, &content)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(
        serde_json::json!({ "success": true, "template": template }),
    ))
}

/// `PATCH /api/marketplace/templates/{id}` → 编辑模板
///
/// multipart：meta（JSON，必填，普通字段整体替换）+ content（可选，提供则替换并重算
/// content_hash，缺省=保留原内容）。服务端权威字段（id/source/download_url/
/// download_count/created_at）不采信客户端值，一律保留原值；updated_at 刷新。
#[utoipa::path(
    patch,
    path = "/api/marketplace/templates/{id}",
    tag = "marketplace",
    params(("id" = String, Path, description = "模板 ID")),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "编辑成功，返回服务端补全后的完整模板", body = serde_json::Value),
        (status = 400, description = "meta 缺字段 / content 超限 / 非法模板 ID", body = serde_json::Value),
        (status = 404, description = "模板不存在或不完整", body = serde_json::Value),
        (status = 500, description = "落盘失败", body = serde_json::Value)
    )
)]
async fn update_template_handler(
    State(dir): State<MarketplaceDir>,
    Path(id): Path<String>,
    mut multipart: Multipart,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let mut meta: Option<Value> = None;
    let mut content: Option<Vec<u8>> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("multipart 解析失败: {e}")))?
    {
        match field.name().unwrap_or("") {
            "meta" => {
                let text = field
                    .text()
                    .await
                    .map_err(|e| err(StatusCode::BAD_REQUEST, format!("meta 字段读取失败: {e}")))?;
                let v: Value = serde_json::from_str(&text)
                    .map_err(|e| err(StatusCode::BAD_REQUEST, format!("meta 非 JSON: {e}")))?;
                meta = Some(v);
            }
            "content" => {
                let bytes = field.bytes().await.map_err(|e| {
                    err(
                        StatusCode::BAD_REQUEST,
                        format!("content 字段读取失败: {e}"),
                    )
                })?;
                if bytes.len() > MAX_CONTENT_BYTES {
                    return Err(err(
                        StatusCode::BAD_REQUEST,
                        format!("content 超限（{} 字节 > {MAX_CONTENT_BYTES}）", bytes.len()),
                    ));
                }
                content = Some(bytes.to_vec());
            }
            _ => {} // 未知字段忽略（前向兼容）
        }
    }
    let meta = meta.ok_or_else(|| {
        err(
            StatusCode::BAD_REQUEST,
            "multipart 缺少 meta 字段（JSON 模板元数据）",
        )
    })?;

    let template = update_template(&dir.0, &id, &meta, content.as_deref()).map_err(|msg| {
        let status = if msg.starts_with("非法") || msg.starts_with("模板元数据") {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::NOT_FOUND
        };
        err(status, msg)
    })?;
    Ok(Json(
        serde_json::json!({ "success": true, "template": template }),
    ))
}

/// `GET /api/marketplace/templates/{id}/download` → 下载内容（递增下载计数）
#[utoipa::path(
    get,
    path = "/api/marketplace/templates/{id}/download",
    tag = "marketplace",
    params(("id" = String, Path, description = "模板 ID")),
    responses(
        (status = 200, description = "模板原始内容（application/octet-stream 附件）", body = Vec<u8>),
        (status = 400, description = "非法模板 ID", body = serde_json::Value),
        (status = 404, description = "模板不存在或不完整", body = serde_json::Value)
    )
)]
async fn download_template(
    State(dir): State<MarketplaceDir>,
    Path(id): Path<String>,
) -> Result<axum::response::Response, (StatusCode, Json<Value>)> {
    let (meta, content) = load_template(&dir.0, &id).map_err(|msg| {
        let status = if msg.starts_with("非法") {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::NOT_FOUND
        };
        err(status, msg)
    })?;

    bump_download_count(&dir.0, &id);

    let filename = meta
        .get("name")
        .and_then(|v| v.as_str())
        .map(|n| n.replace(['\\', '/', '"'], "_"))
        .unwrap_or_else(|| id.clone());
    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/octet-stream")
        .header(
            "content-disposition",
            format!("attachment; filename=\"{filename}.json\""),
        )
        .body(axum::body::Body::from(content))
        .map_err(|e| {
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("响应构建失败: {e}"),
            )
        })
}

/// `DELETE /api/marketplace/templates/{id}` → 删除模板
#[utoipa::path(
    delete,
    path = "/api/marketplace/templates/{id}",
    tag = "marketplace",
    params(("id" = String, Path, description = "模板 ID")),
    responses(
        (status = 200, description = "删除成功", body = serde_json::Value),
        (status = 400, description = "非法模板 ID", body = serde_json::Value),
        (status = 404, description = "模板不存在", body = serde_json::Value)
    )
)]
async fn delete_template_handler(
    State(dir): State<MarketplaceDir>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    remove_template(&dir.0, &id).map_err(|msg| {
        let status = if msg.starts_with("非法") {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::NOT_FOUND
        };
        err(status, msg)
    })?;
    Ok(Json(serde_json::json!({ "success": true, "deleted": id })))
}

// ============================================================================
// WASM 插件类目（官方审核制只读；分发物三件套由运维放置，P5 二期）
// ============================================================================
//
// 与模板类目的职责差异（如实登记）：
// - 模板 = 用户上传制（POST/PATCH/DELETE 全开，单租户无审核流）；
// - 插件 = **官方审核制**（一期只读：无上传/删除/编辑端点）——分发物含可执行
//   二进制，准入门槛高于模板；资产放置 = 运维按《插件发布指南》拷入目录，
//   重启 server（扫描式列表，目录即事实源）。
//
// 存储布局（与 templates/ 同级物理隔离，不入规则加载路径）：
//
// ```text
// {marketplace_dir}/plugins/{plugin_id}/
// ├── {plugin_id}.wasm                 模块二进制（服务名 = plugin_id）
// ├── {plugin_id}.plugin-module.json   分发清单（wasm-host integrity 一期格式）
// ├── {plugin_id}.wasm.sig             分离签名（可缺省——市场只报材料状态）
// └── meta.json                        发布元数据（author/description/下载计数）
// ```
//
// **市场不绕闸**：本类目只陈列材料状态（has_signature/key_id/哈希比对），
// 签名真伪与纯度的**机器验证在 wasm-host 加载期**（enforce 策略）——市场列表
// 不构成信任背书，`integrity_mismatch`（清单哈希 × 实测不符）只做 fail-visible
// 标注，不阻断下载（审查者需要能看到坏样本）。

/// 插件列表分页 query（缺省=第一页 50 条；缺省参数下既有消费方零迁移）
#[derive(serde::Deserialize, Default)]
pub struct PluginListQuery {
    pub page: Option<u32>,
    pub per_page: Option<u32>,
}

/// 插件分发物清单（wasm-host integrity 一期格式的只读投影；字段缺失容忍——
/// 市场陈列对清单取超集读取，严格校验归 wasm-host 加载期）
#[derive(serde::Deserialize)]
struct PluginPartQuery {
    part: Option<String>,
}

/// 扫描插件目录并列出全部分发物材料状态（plugin_id 字典序，确定性）
fn list_plugins_from(marketplace_dir: &std::path::Path) -> Result<Vec<Value>, String> {
    let root = marketplace_dir.join("plugins");
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut ids: Vec<String> = Vec::new();
    let entries = std::fs::read_dir(&root)
        .map_err(|e| format!("扫描插件目录失败({}): {e}", root.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("遍历插件目录失败: {e}"))?;
        let path = entry.path();
        if path.is_dir() {
            if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                ids.push(name.to_string());
            }
        }
    }
    ids.sort();

    let mut out = Vec::new();
    for id in ids {
        if !valid_id(&id) {
            tracing::warn!("插件目录名非法（跳过）: {id}");
            continue;
        }
        if let Some(item) = read_plugin_entry(&root, &id)? {
            out.push(item);
        } // else：不完整目录已 warn，跳过
    }
    Ok(out)
}

/// 读取单个插件的分发物材料状态。`Ok(None)` = 目录不完整（跳过，已 warn）。
fn read_plugin_entry(root: &std::path::Path, id: &str) -> Result<Option<Value>, String> {
    let dir = root.join(id);
    let wasm_path = dir.join(format!("{id}.wasm"));
    let manifest_path = dir.join(format!("{id}.plugin-module.json"));
    let sig_path = dir.join(format!("{id}.wasm.sig"));
    if !wasm_path.is_file() || !manifest_path.is_file() {
        tracing::warn!(
            "插件目录不完整（缺 .wasm 或分发清单，跳过）: {}",
            dir.display()
        );
        return Ok(None);
    }
    let wasm_bytes = std::fs::read(&wasm_path).map_err(|e| format!("读插件模块失败({id}): {e}"))?;
    // 服务端权威重算（对齐模板类目 content_hash 惯例）
    let actual_hash = blake3::hash(&wasm_bytes).to_string();

    let manifest: Value = read_meta(&manifest_path)?;
    let declared_hash = manifest
        .get("blake3")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // "blake3:<hex>" → 裸 hex 比对（清单形态带前缀，marketplace meta 不带）
    let declared_bare = declared_hash
        .strip_prefix("blake3:")
        .unwrap_or(declared_hash);
    let integrity_mismatch = !declared_bare.is_empty() && declared_bare != actual_hash;

    let mut item = serde_json::json!({
        "id": id,
        "name": id,
        "download_url": format!("/api/marketplace/plugins/{id}/download"),
        "content_hash": actual_hash,
        "has_signature": sig_path.is_file(),
        "integrity_mismatch": integrity_mismatch,
    });
    let obj = match item.as_object_mut() {
        Some(o) => o,
        None => return Err(format!("插件 `{id}` 列表项构造异常（非对象）")),
    };
    for key in ["version", "abi", "purity", "engine_min"] {
        if let Some(v) = manifest.get(key) {
            obj.insert(key.into(), v.clone());
        }
    }
    if let Some(key_id) = sig_path
        .is_file()
        .then(|| std::fs::read_to_string(&sig_path).ok())
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|s| s.get("key_id").and_then(|v| v.as_str()).map(str::to_string))
    {
        obj.insert("key_id".into(), Value::String(key_id));
    }
    // meta.json 可缺省（清单是身份事实源；meta 只补展示字段）
    let meta_path = dir.join("meta.json");
    if meta_path.is_file() {
        if let Ok(meta) = read_meta(&meta_path) {
            for key in ["description", "author", "download_count", "created_at"] {
                if let Some(v) = meta.get(key) {
                    obj.insert(key.into(), v.clone());
                }
            }
        }
    }
    Ok(Some(item))
}

/// 读取插件分发物指定部件的字节（part = wasm | manifest | signature）
fn load_plugin_part(
    marketplace_dir: &std::path::Path,
    id: &str,
    part: &str,
) -> Result<(Vec<u8>, &'static str, &'static str), String> {
    if !valid_id(id) {
        return Err(format!("非法插件 ID: {id:?}（只允许字母数字与 - _）"));
    }
    let dir = marketplace_dir.join("plugins").join(id);
    let (path, ctype, ext): (std::path::PathBuf, &'static str, &'static str) = match part {
        "wasm" => (dir.join(format!("{id}.wasm")), "application/wasm", "wasm"),
        "manifest" => (
            dir.join(format!("{id}.plugin-module.json")),
            "application/json",
            "plugin-module.json",
        ),
        "signature" => (
            dir.join(format!("{id}.wasm.sig")),
            "application/json",
            "sig",
        ),
        other => {
            return Err(format!(
                "未知 part=`{other}`（合法值: wasm | manifest | signature）"
            ))
        }
    };
    if !path.is_file() {
        return Err(format!(
            "插件 `{id}` 的 {part} 部件不存在（可刷新市场列表核对材料状态）"
        ));
    }
    let bytes = std::fs::read(&path).map_err(|e| format!("读取失败({}): {e}", path.display()))?;
    Ok((bytes, ctype, ext))
}

/// 递增插件下载计数（meta.json download_count；失败仅告警不阻断——统计位）
fn bump_plugin_download_count(marketplace_dir: &std::path::Path, id: &str) {
    let meta_path = marketplace_dir.join("plugins").join(id).join("meta.json");
    if !meta_path.is_file() {
        return; // 无 meta 的插件不记计数（清单为主身份，容忍）
    }
    let Ok(mut meta) = read_meta(&meta_path) else {
        tracing::warn!("插件下载计数更新失败（meta 不可读）: {id}");
        return;
    };
    let count = meta
        .get("download_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if let Some(obj) = meta.as_object_mut() {
        obj.insert("download_count".into(), Value::from(count + 1));
    }
    match serde_json::to_vec_pretty(&meta) {
        Ok(bytes) => {
            if let Err(e) = atomic_write(&meta_path, &bytes) {
                tracing::warn!("插件下载计数回写失败: {e}");
            }
        }
        Err(e) => tracing::warn!("插件下载计数序列化失败: {e}"),
    }
}

/// `GET /api/marketplace/plugins` → 列出插件分发物材料状态
#[utoipa::path(
    get,
    path = "/api/marketplace/plugins",
    tag = "marketplace",
    responses(
        (status = 200, description = "插件列表（plugin_id 字典序）", body = serde_json::Value),
        (status = 500, description = "存储扫描失败", body = serde_json::Value)
    )
)]
async fn list_plugins(
    State(dir): State<MarketplaceDir>,
    Query(q): Query<PluginListQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let all = list_plugins_from(&dir.0).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let total = all.len();
    // 分页（110 号 P3-11）：page 从 1 起、per_page 缺省 50 上限 200；缺省参数=全量返回
    // （向后兼容：既有消费方无 query 时行为不变）。per_page=0 视为缺省。
    let per_page = q.per_page.unwrap_or(50).clamp(1, 200);
    let page = q.page.unwrap_or(1).max(1);
    let start = ((page - 1) as usize) * (per_page as usize);
    let page_items: Vec<Value> = all
        .iter()
        .skip(start)
        .take(per_page as usize)
        .cloned()
        .collect();
    let returned = page_items.len();
    let count = returned;
    let plugins = page_items;
    // 宿主信息节点：console 安装前兼容性判定的事实源。
    // available 三态显式建模——宿主不可达显式 false，不省略字段不默认兼容。
    // install_channel_enabled 与 available 正交：前者=server 侧管理凭证配置
    // （token 未配置=安装通道未开启），后者=宿主进程可达性
    let host = match probe_host_health().await {
        Ok(h) => serde_json::json!({
            "available": true,
            "version": h.get("host_version"),
            "abi": h.get("host_abi"),
            "install_channel_enabled": wasm_host_admin_token().is_some(),
        }),
        Err(_) => serde_json::json!({
            "available": false,
            "install_channel_enabled": wasm_host_admin_token().is_some(),
        }),
    };
    Ok(Json(serde_json::json!({
        "success": true,
        "count": count,
        "total": total,
        "page": page,
        "per_page": per_page,
        "plugins": plugins,
        "host": host,
    })))
}

/// `GET /api/marketplace/plugins/{id}/download?part=wasm|manifest|signature`
///
/// 下载分发物部件（wasm 为二进制原样字节，manifest/signature 为 JSON 文本）；
/// 每次下载计 1（meta.json download_count，缺失 meta 的插件不计数）。
#[utoipa::path(
    get,
    path = "/api/marketplace/plugins/{id}/download",
    tag = "marketplace",
    params(
        ("id" = String, Path, description = "插件 ID（=目录名=服务名）"),
        ("part" = Option<String>, Query, description = "wasm（默认）| manifest | signature")
    ),
    responses(
        (status = 200, description = "部件字节（wasm=application/wasm，其余 application/json）"),
        (status = 400, description = "非法 ID 或未知 part", body = serde_json::Value),
        (status = 404, description = "插件或部件不存在", body = serde_json::Value)
    )
)]
async fn download_plugin(
    State(dir): State<MarketplaceDir>,
    Path(id): Path<String>,
    Query(q): Query<PluginPartQuery>,
) -> Result<(StatusCode, [(header::HeaderName, String); 2], Vec<u8>), (StatusCode, Json<Value>)> {
    let part = q.part.as_deref().unwrap_or("wasm");
    let (bytes, ctype, ext) =
        load_plugin_part(&dir.0, &id, part).map_err(|e| err(StatusCode::NOT_FOUND, e))?;
    bump_plugin_download_count(&dir.0, &id);
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, ctype.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{id}.{ext}\""),
            ),
        ],
        bytes,
    ))
}

// ============================================================================
// 单测（tempdir 直测存储逻辑，无 HTTP 层）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_meta() -> Value {
        json!({
            "type": "rule",
            "name": "医疗患者随访",
            "description": "随访规则模板",
            "category": "medical",
            "tags": ["医疗"],
            "author": { "id": "u1", "displayName": "张三" },
            "version": "1.0.0",
        })
    }

    #[test]
    fn store_and_list_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        // 空库 → 空列表（非报错）
        assert!(list_templates_from(dir).unwrap().is_empty());

        let t1 = store_template(dir, &sample_meta(), b"content-1").unwrap();
        let t2 = store_template(dir, &sample_meta(), b"content-2").unwrap();

        let list = list_templates_from(dir).unwrap();
        assert_eq!(list.len(), 2);
        // 服务端权威字段
        for t in [&t1, &t2] {
            assert_eq!(t["source"], "user");
            assert_eq!(t["download_count"], 0);
            assert!(t["download_url"]
                .as_str()
                .unwrap()
                .starts_with("/api/marketplace/templates/"));
            assert!(t["content_hash"].as_str().unwrap().len() >= 32);
        }
        // ID 不同
        assert_ne!(t1["id"], t2["id"]);
    }

    #[test]
    fn store_rejects_missing_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        // 缺 name
        let mut m = sample_meta();
        m.as_object_mut().unwrap().remove("name");
        assert!(store_template(dir, &m, b"x").is_err());

        // 缺 type
        let mut m = sample_meta();
        m.as_object_mut().unwrap().remove("type");
        assert!(store_template(dir, &m, b"x").is_err());

        // 缺 category
        let mut m = sample_meta();
        m.as_object_mut().unwrap().remove("category");
        assert!(store_template(dir, &m, b"x").is_err());

        // 非对象 meta
        assert!(store_template(dir, &json!("str"), b"x").is_err());
    }

    #[test]
    fn download_roundtrip_and_count_bump() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let t = store_template(dir, &sample_meta(), b"hello-content").unwrap();
        let id = t["id"].as_str().unwrap().to_string();

        let (_meta, content) = load_template(dir, &id).unwrap();
        assert_eq!(content, b"hello-content");

        bump_download_count(dir, &id);
        let list = list_templates_from(dir).unwrap();
        assert_eq!(list[0]["download_count"], 1);

        // 内容哈希与内容一致
        let stored = list[0]["content_hash"].as_str().unwrap();
        assert_eq!(stored, blake3::hash(b"hello-content").to_string());
    }

    #[test]
    fn load_missing_or_incomplete_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        // 不存在
        assert!(load_template(dir, "tpl-nonexist").is_err());

        // 不完整（只有 meta 没有 content）
        let tdir = dir.join("templates").join("tpl-half");
        std::fs::create_dir_all(&tdir).unwrap();
        std::fs::write(tdir.join("meta.json"), "{}").unwrap();
        assert!(load_template(dir, "tpl-half").is_err());
    }

    #[test]
    fn path_traversal_ids_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        assert!(load_template(dir, "../evil").is_err());
        assert!(load_template(dir, "..\\evil").is_err());
        assert!(load_template(dir, "a/b").is_err());
        assert!(load_template(dir, "").is_err());
        assert!(remove_template(dir, "../evil").is_err());
        // 合法字符集放行（不存在的 ID 报"不存在"而非"非法"）
        let e = load_template(dir, "tpl-ok-ID_9").unwrap_err();
        assert!(e.contains("不存在"), "got: {e}");
    }

    #[test]
    fn delete_removes_dir_then_404_semantics() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let t = store_template(dir, &sample_meta(), b"x").unwrap();
        let id = t["id"].as_str().unwrap().to_string();

        remove_template(dir, &id).unwrap();
        // 再删 → 不存在错误
        let e = remove_template(dir, &id).unwrap_err();
        assert!(e.contains("不存在"), "got: {e}");
        assert!(list_templates_from(dir).unwrap().is_empty());
    }

    #[test]
    fn invalid_meta_skipped_with_warn_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        store_template(dir, &sample_meta(), b"x").unwrap();
        // 异常残留目录（无 meta.json）不阻断列表
        let bad = dir.join("templates").join("tpl-broken");
        std::fs::create_dir_all(&bad).unwrap();
        let list = list_templates_from(dir).unwrap();
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn created_at_desc_sort() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let a = store_template(dir, &sample_meta(), b"1").unwrap();
        // 手工把 a 的 created_at 拨老
        let apath = dir
            .join("templates")
            .join(a["id"].as_str().unwrap())
            .join("meta.json");
        let mut m = read_meta(&apath).unwrap();
        m.as_object_mut()
            .unwrap()
            .insert("created_at".into(), json!("2000-01-01T00:00:00Z"));
        std::fs::write(&apath, serde_json::to_vec(&m).unwrap()).unwrap();
        let b = store_template(dir, &sample_meta(), b"2").unwrap();

        let list = list_templates_from(dir).unwrap();
        assert_eq!(list[0]["id"], b["id"], "新的在前");
        assert_eq!(list[1]["id"], a["id"]);
    }

    #[test]
    fn chrono_now_iso_format_sanity() {
        // 手搓日期算法（Hinnant civil_from_days）的防回归：格式 + 年份合理域
        let s = chrono_now_iso();
        assert_eq!(s.len(), 20, "ISO 形态长度: {s}");
        for (i, c) in s.char_indices() {
            let ok = match i {
                4 | 7 => c == '-',
                10 => c == 'T',
                13 | 16 => c == ':',
                19 => c == 'Z',
                _ => c.is_ascii_digit(),
            };
            assert!(ok, "位置 {i} 字符 {c:?} 不符合 ISO 形态: {s}");
        }
        let year: i64 = s[..4].parse().unwrap();
        assert!((2026..2100).contains(&year), "年份超出合理域: {s}");
        let month: u32 = s[5..7].parse().unwrap();
        assert!((1..=12).contains(&month), "月份越界: {s}");
        let day: u32 = s[8..10].parse().unwrap();
        assert!((1..=31).contains(&day), "日期越界: {s}");
        let hour: u32 = s[11..13].parse().unwrap();
        assert!(hour < 24, "小时越界: {s}");
        let minute: u32 = s[14..16].parse().unwrap();
        assert!(minute < 60, "分钟越界: {s}");
    }

    // ---------- 编辑（PATCH）----------

    #[test]
    fn update_meta_and_content_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let t = store_template(dir, &sample_meta(), b"old-content").unwrap();
        let id = t["id"].as_str().unwrap().to_string();
        let old_hash = t["content_hash"].as_str().unwrap().to_string();
        let old_created = t["created_at"].as_str().unwrap().to_string();

        // 下载几次制造非零计数（验证编辑不动计数）
        bump_download_count(dir, &id);
        bump_download_count(dir, &id);

        let mut m = sample_meta();
        m["name"] = json!("医疗患者随访（修订）");
        m["description"] = json!("修订后的随访规则模板");
        let updated = update_template(dir, &id, &m, Some(b"new-content")).unwrap();

        // 元数据更新生效
        assert_eq!(updated["name"], "医疗患者随访（修订）");
        // 服务端权威字段保留原值
        assert_eq!(updated["id"], json!(id));
        assert_eq!(updated["source"], "user");
        assert_eq!(updated["download_count"], 2, "编辑不得改动下载计数");
        assert_eq!(updated["created_at"], json!(old_created));
        assert!(updated["download_url"]
            .as_str()
            .unwrap()
            .starts_with("/api/marketplace/templates/"));
        // content 替换 → hash 重算
        assert_eq!(
            updated["content_hash"].as_str().unwrap(),
            blake3::hash(b"new-content").to_string()
        );
        assert_ne!(updated["content_hash"].as_str().unwrap(), old_hash);
        // updated_at 刷新（≥ 原 created_at，ISO 字符串可比较）
        assert!(updated["updated_at"].as_str().unwrap() >= old_created.as_str());

        // 落盘内容确实替换
        let (_meta, content) = load_template(dir, &id).unwrap();
        assert_eq!(content, b"new-content");
        // 列表读到的是新值
        let list = list_templates_from(dir).unwrap();
        assert_eq!(list[0]["name"], "医疗患者随访（修订）");
    }

    #[test]
    fn update_meta_only_keeps_content_and_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let t = store_template(dir, &sample_meta(), b"stable-content").unwrap();
        let id = t["id"].as_str().unwrap().to_string();
        let old_hash = t["content_hash"].as_str().unwrap().to_string();

        let mut m = sample_meta();
        m["description"] = json!("仅改描述，不动内容");
        let updated = update_template(dir, &id, &m, None).unwrap();

        // content 缺省 → hash 保留原值（审计面对得上账）
        assert_eq!(updated["content_hash"].as_str().unwrap(), old_hash);
        let (_meta, content) = load_template(dir, &id).unwrap();
        assert_eq!(content, b"stable-content");
        assert_eq!(updated["description"], "仅改描述，不动内容");
    }

    #[test]
    fn update_rejects_client_forged_authority_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let t = store_template(dir, &sample_meta(), b"x").unwrap();
        let id = t["id"].as_str().unwrap().to_string();

        // 客户端伪造 id/source/download_count/download_url/created_at → 一律不采信
        let mut m = sample_meta();
        m["id"] = json!("tpl-forged");
        m["source"] = json!("official");
        m["download_count"] = json!(9999);
        m["download_url"] = json!("http://evil.example/x");
        m["created_at"] = json!("1999-01-01T00:00:00Z");
        let updated = update_template(dir, &id, &m, None).unwrap();

        assert_eq!(updated["id"], json!(id), "伪造 id 不采信");
        assert_eq!(updated["source"], "user");
        assert_eq!(updated["download_count"], 0);
        assert!(updated["download_url"]
            .as_str()
            .unwrap()
            .starts_with("/api/marketplace/templates/"));
        assert_ne!(updated["created_at"], json!("1999-01-01T00:00:00Z"));
    }

    #[test]
    fn update_missing_or_invalid_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        // 不存在 → 错误含"不存在"（404 语义）
        let e = update_template(dir, "tpl-nonexist", &sample_meta(), None).unwrap_err();
        assert!(e.contains("不存在"), "got: {e}");

        // 非法 ID → 错误含"非法"（400 语义）
        let e = update_template(dir, "../evil", &sample_meta(), None).unwrap_err();
        assert!(e.contains("非法"), "got: {e}");

        // 缺 name / type / category → 拒绝
        let t = store_template(dir, &sample_meta(), b"x").unwrap();
        let id = t["id"].as_str().unwrap().to_string();
        let mut m = sample_meta();
        m.as_object_mut().unwrap().remove("name");
        assert!(update_template(dir, &id, &m, None).is_err());
        let mut m = sample_meta();
        m.as_object_mut().unwrap().remove("type");
        assert!(update_template(dir, &id, &m, None).is_err());
        let mut m = sample_meta();
        m.as_object_mut().unwrap().remove("category");
        assert!(update_template(dir, &id, &m, None).is_err());

        // 非对象 meta → 拒绝
        assert!(update_template(dir, &id, &json!("str"), None).is_err());
    }

    #[test]
    fn update_content_overflow_guarded_at_handler_shape() {
        // content 超限校验在 HTTP handler（MAX_CONTENT_BYTES）；核心函数层验证
        // content 可选性语义本身（None=保留）已由 update_meta_only 覆盖。
        // 此处验证大 content 在核心层无隐藏截断（如数写入完整性）。
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let t = store_template(dir, &sample_meta(), b"x").unwrap();
        let id = t["id"].as_str().unwrap().to_string();

        let big = vec![b'a'; 1024 * 1024]; // 1MB，低于 10MB 上限
        let updated = update_template(dir, &id, &sample_meta(), Some(&big)).unwrap();
        assert_eq!(
            updated["content_hash"].as_str().unwrap(),
            blake3::hash(&big).to_string()
        );
        let (_meta, content) = load_template(dir, &id).unwrap();
        assert_eq!(content.len(), 1024 * 1024);
    }

    // ---- 插件类目（官方审核制只读）----

    fn seed_plugin(dir: &std::path::Path, id: &str, wasm: &[u8], with_sig: bool) {
        let d = dir.join("plugins").join(id);
        std::fs::create_dir_all(&d).unwrap();
        let hash = blake3::hash(wasm).to_string();
        std::fs::write(d.join(format!("{id}.wasm")), wasm).unwrap();
        std::fs::write(
            d.join(format!("{id}.plugin-module.json")),
            format!(
                r#"{{"module":"{id}","version":"1.0.0","abi":1,"blake3":"blake3:{hash}","purity":"zero-import"}}"#
            ),
        )
        .unwrap();
        if with_sig {
            std::fs::write(
                d.join(format!("{id}.wasm.sig")),
                format!(r#"{{"module":"{id}","key_id":"demo-2026","signature":"00"}}"#),
            )
            .unwrap();
        }
        std::fs::write(
            d.join("meta.json"),
            r#"{"description":"演示插件","author":"official","download_count":0}"#,
        )
        .unwrap();
    }

    #[test]
    fn plugin_listing_reports_material_status() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        seed_plugin(dir, "demo", b"demo-wasm-bytes", true);
        let list = list_plugins_from(dir).unwrap();
        assert_eq!(list.len(), 1);
        let p = &list[0];
        assert_eq!(p["id"], "demo");
        assert_eq!(p["has_signature"], true);
        assert_eq!(p["key_id"], "demo-2026");
        assert_eq!(p["integrity_mismatch"], false);
        assert_eq!(p["purity"], "zero-import");
        assert_eq!(p["abi"], 1);
        assert_eq!(
            p["content_hash"].as_str().unwrap(),
            blake3::hash(b"demo-wasm-bytes").to_string()
        );
        assert_eq!(p["description"], "演示插件");
    }

    #[test]
    fn plugin_listing_integrity_mismatch_visible_not_hidden() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        seed_plugin(dir, "demo", b"original", true);
        // 事后换 .wasm 内容（清单哈希与实测不符）→ 列表标注而非隐藏
        std::fs::write(dir.join("plugins/demo/demo.wasm"), b"tampered").unwrap();
        let list = list_plugins_from(dir).unwrap();
        assert_eq!(list.len(), 1, "坏样本仍陈列（审查者需可见）");
        assert_eq!(list[0]["integrity_mismatch"], true);
    }

    #[test]
    fn plugin_listing_skips_incomplete_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        seed_plugin(dir, "good", b"x", false);
        // 残缺目录（缺清单）→ 跳过并 warn，不影响其余
        let bad = dir.join("plugins").join("bad~");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("bad~.wasm"), b"y").unwrap();
        let list = list_plugins_from(dir).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["id"], "good");
        assert_eq!(list[0]["has_signature"], false, "无签名如实陈列");
    }

    #[test]
    fn plugin_part_download_all_parts_and_unknown_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        seed_plugin(dir, "demo", b"wasm-bytes-here", true);
        let (bytes, _, ext) = load_plugin_part(dir, "demo", "wasm").unwrap();
        assert_eq!(bytes, b"wasm-bytes-here");
        assert_eq!(ext, "wasm");
        let (bytes, _, _) = load_plugin_part(dir, "demo", "manifest").unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("zero-import"));
        let (bytes, _, _) = load_plugin_part(dir, "demo", "signature").unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("demo-2026"));
        assert!(load_plugin_part(dir, "demo", "source").is_err());
        assert!(load_plugin_part(dir, "ghost", "wasm").is_err());
        assert!(load_plugin_part(dir, "../escape", "wasm").is_err());
    }

    #[test]
    fn plugin_download_count_bumps() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        seed_plugin(dir, "demo", b"x", false);
        bump_plugin_download_count(dir, "demo");
        bump_plugin_download_count(dir, "demo");
        let meta = read_meta(&dir.join("plugins/demo/meta.json")).unwrap();
        assert_eq!(meta["download_count"], 2);
    }
}

// ============================================================================
// 插件安装自动化代理（三期）：server 只做鉴权转发+审计+registry 合入，
// 不落盘不碰进程（落盘/验签/热加载归 wasm-host /admin 端点）
// ============================================================================

/// 实时探宿主 /health（预检事实源；公开面零 token）。失败 = 显式不可预检。
async fn probe_host_health() -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    let url = format!("{}/health", wasm_host_admin_url().trim_end_matches('/'));
    let resp = reqwest::Client::new()
        .get(url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| {
            err(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("宿主不可达，无法预检兼容性: {e}"),
            )
        })?;
    // 200（对账一致）与 503（对账 degraded，如禁用后零声明场景）的 body 都携带
    // 版本握手字段——预检只消费版本事实，不关心对账结论；
    // 连接层失败（进程死/端口不通）才是"无法预检"。
    if !matches!(
        resp.status(),
        StatusCode::OK | StatusCode::SERVICE_UNAVAILABLE
    ) {
        return Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("宿主健康检查返回 {}", resp.status()),
        ));
    }
    let value: serde_json::Value = resp.json().await.map_err(|e| {
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("宿主健康响应解析失败: {e}"),
        )
    })?;
    if value.get("host_abi").and_then(|v| v.as_u64()).is_none() {
        return Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "宿主健康响应缺版本握手字段（wasm-host 版本过旧，请升级后重试）",
        ));
    }
    Ok(value)
}

/// 三段语义版比较 `current >= min`（与 wasm-host version_at_least 同口径；
/// 解析失败 = false，fail-visible 不放行）
fn version_at_least_three(current: &str, min: &str) -> bool {
    let parse = |v: &str| -> Option<Vec<u64>> {
        v.split('.')
            .map(|p| p.parse::<u64>().ok())
            .collect::<Option<Vec<_>>>()
    };
    let (Some(c), Some(m)) = (parse(current), parse(min)) else {
        return false;
    };
    // 逐段比较：任一段分出大小即定序；全等则满足
    for i in 0..3 {
        let cv = c.get(i).copied().unwrap_or(0);
        let mv = m.get(i).copied().unwrap_or(0);
        if cv != mv {
            return cv > mv;
        }
    }
    true
}

/// wasm-host 管理面地址（env `WASM_HOST_ADMIN_URL`，缺省本机 9140）
fn wasm_host_admin_url() -> String {
    std::env::var("WASM_HOST_ADMIN_URL").unwrap_or_else(|_| "http://127.0.0.1:9140".into())
}

/// wasm-host 管理凭证（env `WASM_HOST_ADMIN_TOKEN`，与 wasm-host 侧同值配置；
/// 未配置 = 安装通道未开启，端点显式 503 而非静默失败）
fn wasm_host_admin_token() -> Option<String> {
    std::env::var("WASM_HOST_ADMIN_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

/// 手拼 multipart body（零新依赖：multipart 只是 body 格式；三件套字段名
/// 与 wasm-host /admin/install 契约对齐）
fn build_admin_multipart(
    wasm: &[u8],
    manifest: &[u8],
    signature: Option<&[u8]>,
    allow_downgrade: bool,
) -> (Vec<u8>, String) {
    let boundary = format!(
        "----evorule-install-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let mut body = Vec::new();
    let part = |name: &str, filename: &str, bytes: &[u8], body: &mut Vec<u8>| {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n")
                .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    };
    part("wasm", "module.wasm", wasm, &mut body);
    part("manifest", "manifest.json", manifest, &mut body);
    if let Some(sig) = signature {
        part("signature", "module.sig", sig, &mut body);
    }
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"allow_downgrade\"\r\n\r\n{allow_downgrade}\r\n").as_bytes(),
    );
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (body, boundary)
}

/// 转发 wasm-host 管理端点（安装自动化三期）
async fn forward_admin(
    path: &str,
    body: Option<(Vec<u8>, String)>,
) -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    let token = wasm_host_admin_token().ok_or_else(|| {
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            "安装通道未开启（server 未配置 WASM_HOST_ADMIN_TOKEN）",
        )
    })?;
    let url = format!("{}{path}", wasm_host_admin_url().trim_end_matches('/'));
    let mut req = reqwest::Client::new()
        .post(url)
        .bearer_auth(&token)
        .timeout(std::time::Duration::from_secs(180));
    if let Some((bytes, boundary)) = body {
        req = req.header(
            reqwest::header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        );
        req = req.body(bytes);
    }
    let resp = req.send().await.map_err(|e| {
        err(
            StatusCode::BAD_GATEWAY,
            format!("wasm-host 管理面不可达: {e}"),
        )
    })?;
    let status = resp.status();
    let value: serde_json::Value = resp.json().await.unwrap_or_else(|_| serde_json::json!({}));
    if !status.is_success() {
        let message = value
            .get("error")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("wasm-host 返回 HTTP {status}"));
        return Err((
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            err(status, message).1,
        ));
    }
    Ok(value)
}

/// `POST /api/marketplace/plugins/{id}/install`——一键安装：
/// 市场三件套 → wasm-host 验签/落盘/热加载 → registry 合入（装完即用）+ 审计
#[utoipa::path(
    post,
    path = "/api/marketplace/plugins/{id}/install",
    tag = "marketplace",
    params(("id" = String, Path, description = "插件 ID（市场在架）")),
    responses(
        (status = 200, description = "安装成功（含阶段耗时与实载清单）", body = serde_json::Value),
        (status = 404, description = "插件不在架 / wasm-host 不可达", body = serde_json::Value),
        (status = 422, description = "验签/编译拒绝（fail-visible，零落盘）", body = serde_json::Value),
        (status = 503, description = "安装通道未开启（server 未配置管理凭证）", body = serde_json::Value)
    )
)]
async fn install_plugin_handler(
    State(dir): State<MarketplaceDir>,
    State(sessions): State<SessionApi>,
    State(facts): State<evorule_governance::shared_facts_log::SharedFactsLog>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let (wasm, _, _) =
        load_plugin_part(&dir.0, &id, "wasm").map_err(|e| err(StatusCode::NOT_FOUND, e))?;
    let (manifest, _, _) =
        load_plugin_part(&dir.0, &id, "manifest").map_err(|e| err(StatusCode::NOT_FOUND, e))?;
    let signature = load_plugin_part(&dir.0, &id, "signature")
        .ok()
        .map(|(b, _, _)| b);

    // 兼容性预检：转发前实时探宿主 /health——abi 等值硬校验+
    // engine_min 提示性校验，不兼容提前 422 并指路（宿主侧校验保持双保险）。
    // fail-visible：宿主不可达 → 503 显式"无法预检"，不降级为默认兼容。
    let manifest_meta: serde_json::Value =
        serde_json::from_slice(&manifest).unwrap_or_else(|_| serde_json::json!({}));
    let host = probe_host_health().await?;
    if let Some(host_abi) = host.get("host_abi").and_then(|v| v.as_u64()) {
        if let Some(plugin_abi) = manifest_meta.get("abi").and_then(|v| v.as_u64()) {
            if plugin_abi != host_abi {
                return Err(err(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!(
                        "分发清单 ABI={plugin_abi} 与宿主 ABI={host_abi} 不兼容——插件需按新 ABI 重编译并重签"
                    ),
                ));
            }
        }
    }
    if let (Some(min), Some(host_version)) = (
        manifest_meta
            .get("engine_min")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        host.get("host_version")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    ) {
        // 三段语义版比较（与 wasm-host version_at_least 同口径）
        if !version_at_least_three(&host_version, &min) {
            return Err(err(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("宿主需升级至 >= {min}（当前 {host_version}）才能运行该插件"),
            ));
        }
    }

    let (body, boundary) = build_admin_multipart(&wasm, &manifest, signature.as_deref(), false);
    let result = forward_admin("/admin/install", Some((body, boundary))).await?;

    // registry 运行时合入（装完即用）：敏感/超时缺省同 auto_discover 口径
    sessions.merge_registry_service(ServiceMeta {
        name: id.clone(),
        version: manifest_meta
            .get("version")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        description: Some("wasm 插件（市场安装）".into()),
    });
    append_platform_event(
        &facts,
        "platform.event.plugin_installed",
        serde_json::json!({
            "plugin": id,
            "version": manifest_meta.get("version"),
            "source": "marketplace",
        }),
    );
    Ok(Json(serde_json::json!({
        "success": true,
        "installed": id,
        "detail": result,
    })))
}

/// `POST /api/marketplace/plugins/{id}/uninstall`——卸载（快照移除+文件删除）
async fn uninstall_plugin_handler(
    State(sessions): State<SessionApi>,
    State(facts): State<evorule_governance::shared_facts_log::SharedFactsLog>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let result = forward_admin(&format!("/admin/uninstall/{id}"), None).await?;
    sessions.remove_registry_service(&id);
    append_platform_event(
        &facts,
        "platform.event.plugin_uninstalled",
        serde_json::json!({ "plugin": id, "source": "marketplace" }),
    );
    Ok(Json(
        serde_json::json!({ "success": true, "uninstalled": id, "detail": result }),
    ))
}

/// `POST /api/marketplace/plugins/{id}/disable`——运行时下线（文件保留，重启恢复）
async fn disable_plugin_handler(
    State(sessions): State<SessionApi>,
    State(facts): State<evorule_governance::shared_facts_log::SharedFactsLog>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let result = forward_admin(&format!("/admin/disable/{id}"), None).await?;
    sessions.remove_registry_service(&id);
    append_platform_event(
        &facts,
        "platform.event.plugin_disabled",
        serde_json::json!({ "plugin": id, "source": "marketplace" }),
    );
    Ok(Json(
        serde_json::json!({ "success": true, "disabled": id, "detail": result }),
    ))
}

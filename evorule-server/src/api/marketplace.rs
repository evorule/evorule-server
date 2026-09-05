// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 模板市场 API —— `/api/marketplace/templates` 端点族（UV-084 W4 / UV-064 实化；UV-087 补编辑）
//!
//! P09_IMPORT_EXPORT_INFRA_DESIGN.md §7.1 定义的 P1 契约（console marketplace.ts
//! 4 处 P1 注释预留的接线路径）：
//! - `GET  /api/marketplace/templates`            → 列出用户上传模板
//! - `POST /api/marketplace/templates`            → multipart 上传（meta JSON + content）
//! - `GET  /api/marketplace/templates/{id}/download` → 下载内容（递增计数）
//! - `DELETE /api/marketplace/templates/{id}`     → 删除（连同内容）
//! - `PATCH /api/marketplace/templates/{id}`      → 编辑（UV-087：meta 必填 + content 可选替换）
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

use axum::extract::{Multipart, Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde_json::Value;

use crate::api::server::{AppState, MarketplaceDir};

/// 单模板上传内容上限（模板为规则/数据集/表单定义，10MB 已远超需要；防滥用）
const MAX_CONTENT_BYTES: usize = 10 * 1024 * 1024;

/// 构造 `/api/marketplace` 的路由（挂入受认证保护路由组）
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
    std::fs::write(&tmp, bytes)
        .map_err(|e| format!("写临时文件失败({}): {e}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| format!("原子改名失败({}): {e}", path.display()))
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
    if meta_in
        .get("category")
        .and_then(|v| v.as_str())
        .is_none()
    {
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
    atomic_write(&meta_path, &serde_json::to_vec_pretty(&meta).map_err(|e| e.to_string())?)?;

    Ok(meta)
}

/// 编辑模板（UV-087）：meta 整体替换（必填字段校验同上传）+ content 可选替换。
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
    if meta_in
        .get("category")
        .and_then(|v| v.as_str())
        .is_none()
    {
        return Err("模板元数据缺少 category".into());
    }

    // 以客户端 meta 为底（普通字段整体替换），权威字段从旧值回填/派生
    let mut new_meta = meta_in.clone();
    let obj = new_meta
        .as_object_mut()
        .ok_or_else(|| "模板元数据必须是 JSON 对象".to_string())?;
    for key in ["id", "source", "download_url", "download_count", "created_at"] {
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
fn load_template(
    marketplace_dir: &std::path::Path,
    id: &str,
) -> Result<(Value, Vec<u8>), String> {
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
    // 与 console `new Date().toISOString()` 可比较的 UTC 形态
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
    let templates = list_templates_from(&dir.0)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
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
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| err(StatusCode::BAD_REQUEST, format!("content 字段读取失败: {e}")))?;
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
        err(StatusCode::BAD_REQUEST, "multipart 缺少 meta 字段（JSON 模板元数据）")
    })?;
    let content = content.ok_or_else(|| {
        err(StatusCode::BAD_REQUEST, "multipart 缺少 content 字段（模板内容）")
    })?;

    let template = store_template(&dir.0, &meta, &content)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(serde_json::json!({ "success": true, "template": template })))
}

/// `PATCH /api/marketplace/templates/{id}` → 编辑模板（UV-087）
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
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| err(StatusCode::BAD_REQUEST, format!("content 字段读取失败: {e}")))?;
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
        err(StatusCode::BAD_REQUEST, "multipart 缺少 meta 字段（JSON 模板元数据）")
    })?;

    let template = update_template(&dir.0, &id, &meta, content.as_deref()).map_err(|msg| {
        let status = if msg.starts_with("非法") || msg.starts_with("模板元数据") {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::NOT_FOUND
        };
        err(status, msg)
    })?;
    Ok(Json(serde_json::json!({ "success": true, "template": template })))
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
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("响应构建失败: {e}")))
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
    Ok(Json(
        serde_json::json!({ "success": true, "deleted": id }),
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
            assert!(t["download_url"].as_str().unwrap().starts_with("/api/marketplace/templates/"));
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
        m.as_object_mut().unwrap().insert(
            "created_at".into(),
            json!("2000-01-01T00:00:00Z"),
        );
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

    // ---------- UV-087 编辑（PATCH）----------

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
}

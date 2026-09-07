// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 平台用户体系与授权
//!
//! **术语**:本模块是"平台授权"(谁能登录、能用 console 哪些功能)。
//! 与既有 `/api/permissions`(规则运行时执行权限,治理域)是两套体系,前缀刻意分开:
//! 本模块全部挂 `/api/platform/*`。
//!
//! **状态存储(对计划 D2 的偏差修正)**:evorule-server 自身无数据库依赖
//! (data/evorule.db 属规则 I/O handler),故用户/角色/会话不建 SQLite 表,
//! 改存 [`SharedFactsLog`] 事实(path 前缀 `platform.`,last-write-wins 回放):
//!
//! - `platform.user.{username}`  → 用户档案(Argon2id 哈希/状态/角色)
//! - `platform.role.{rolename}`  → 角色定义(builtin/权限集/状态)
//! - `platform.session.{token_hash}` → 会话(token 哈希/过期时间/吊销标记)
//! - `platform.event.{unique}`   → 认证事件(登录失败等,append-only)
//!
//! 收益:每次授权变更天然进入治理审计链(BLAKE3),D7 审计闭环零额外实现;
//! 持久化复用 shared_facts.wal(--wal-dir 启用时)。
//!
//! **会话 token**:不透明随机 256-bit hex,库存 blake3 哈希(不存明文)。
//! 默认有效期 7 天,登出/停用用户即时吊销(追加 revoked 事实)。
//!
//! **认证检查边界（)**:bootstrap/login/status 公开;me/logout/change-password
//! 与平台管理端点在 handler 内自校验平台 token/权限点;业务 API 经
//! [`unified_auth_middleware`](挂 protected_routes)统一认证(双凭据:
//! 静态 token 或平台会话),401 统一 JSON 错误体。

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use blake3::Hasher;
use evorule_governance::shared_facts_log::SharedFactsLog;
use evorule_tcb::JsonValue;
use utoipa::ToSchema;

use argon2::password_hash::{rand_core::OsRng, SaltString};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};

use std::collections::BTreeMap;

/// 事实路径前缀(平台授权命名空间)
const FACT_PREFIX: &str = "platform.";

/// 平台授权事实写入的来源会话 ID(0 = 系统/全局)
const GLOBAL_SESSION: u64 = 0;

/// 会话 token 有效期(毫秒)= 7 天
const SESSION_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;

// ---------------------------------------------------------------------------
// 权限点注册表
// ---------------------------------------------------------------------------

/// 全部权限点(15 = 业务 12 点 + 平台管理 3 点)。
///
/// 业务 12 点与 console-cloud `permission-matrix.ts` 保持同名(种子迁移);
/// 平台管理 3 点是本专项新增。权限点代码内置注册,不允许运行时增删
/// (避免语义漂移;角色→权限点的关联存事实)。
pub const PLATFORM_ACTIONS: &[&str] = &[
    // 业务 12 点(P08 §5.1 种子)
    "view_monitor",
    "view_audit_chain",
    "intervene_runtime",
    "rollback_ruleset",
    "create_workspace",
    "edit_draft",
    "review_in_workspace",
    "submit_to_publish",
    "start_sandbox",
    "approve_publish",
    "view_publish_queue",
    "view_test_report",
    // 平台管理 3 点
    "manage_users",
    "manage_roles",
    "view_users",
];

/// 内置角色(对齐多租户设计 4 角色):不可删除;administrator 不可改权限集
pub const BUILTIN_ROLES: &[(&str, &[&str])] = &[
    (
        "administrator",
        &[
            "view_monitor",
            "view_audit_chain",
            "intervene_runtime",
            "rollback_ruleset",
            "create_workspace",
            "edit_draft",
            "review_in_workspace",
            "submit_to_publish",
            "start_sandbox",
            "approve_publish",
            "view_publish_queue",
            "view_test_report",
            "manage_users",
            "manage_roles",
            "view_users",
        ],
    ),
    (
        "approver",
        &[
            "view_monitor",
            "view_audit_chain",
            "view_publish_queue",
            "approve_publish",
            "view_test_report",
        ],
    ),
    (
        "rule_engineer",
        &[
            "view_monitor",
            "create_workspace",
            "edit_draft",
            "review_in_workspace",
            "submit_to_publish",
            "start_sandbox",
            "view_test_report",
        ],
    ),
    ("viewer", &["view_monitor", "view_test_report"]),
];

// ---------------------------------------------------------------------------
// 快照模型(facts → last-write-wins 回放)
// ---------------------------------------------------------------------------

/// 平台用户(事实回放后的当前状态)
#[derive(Debug, Clone)]
pub struct PlatformUser {
    pub username: String,
    pub display_name: String,
    pub email: String,
    pub department: String,
    pub password_hash: String,
    pub status: String, // ACTIVE | DISABLED
    pub role: String,
}

/// 平台角色(事实回放后的当前状态)
#[derive(Debug, Clone)]
pub struct PlatformRole {
    pub name: String,
    pub builtin: bool,
    pub status: String, // ACTIVE | DISABLED
    pub description: String,
    pub permissions: Vec<String>,
}

/// 平台会话(事实回放后的当前状态)
#[derive(Debug, Clone)]
pub struct StoredSession {
    pub token_hash: String,
    pub username: String,
    pub expires_at_ms: u64,
    pub revoked: bool,
}

/// 平台状态快照(version 单调递增,前端据此感知授权变更)
#[derive(Debug, Default)]
pub struct PlatformSnapshot {
    pub version: u64,
    pub users: BTreeMap<String, PlatformUser>,
    pub roles: BTreeMap<String, PlatformRole>,
    pub sessions: BTreeMap<String, StoredSession>,
}

impl PlatformSnapshot {
    /// 从 SharedFactsLog 回放平台授权状态。
    ///
    /// 同一 path 多次 append 时按事实顺序取最后一条(last-write-wins),
    /// 与治理审计链的追加语义一致。
    pub fn replay(shared: &SharedFactsLog) -> Result<Self, AuthError> {
        let facts = shared.facts_by_path_prefix(FACT_PREFIX);
        let mut snap = PlatformSnapshot {
            version: shared.version(),
            ..Default::default()
        };
        for f in facts {
            let key = match f.path.strip_prefix(FACT_PREFIX) {
                Some(k) => k.to_string(),
                None => continue,
            };
            let (kind, name) = match key.split_once('.') {
                Some(kv) => kv,
                None => continue, // platform.event.* 走不到这里(event 自带二级 key,统一在下方过滤)
            };
            if kind == "event" {
                continue; // 认证事件不参与状态回放,仅入链审计
            }
            let v = &f.value;
            // 墓碑语义:最后一条事实带 deleted=true 时该实体从快照移除
            // (事实日志 append-only,删除 = 追加墓碑;之后可重新创建同名实体)
            let deleted = v
                .get("deleted")
                .is_some_and(|b| matches!(b, JsonValue::Bool(true)));
            if deleted {
                match kind {
                    "user" => {
                        snap.users.remove(name);
                    }
                    "role" => {
                        snap.roles.remove(name);
                    }
                    _ => {}
                }
                continue;
            }
            match kind {
                "user" => {
                    let u = PlatformUser {
                        username: name.to_string(),
                        display_name: jstr(v, "display_name"),
                        email: jstr(v, "email"),
                        department: jstr(v, "department"),
                        password_hash: jstr(v, "password_hash"),
                        status: jstr(v, "status"),
                        role: jstr(v, "role"),
                    };
                    snap.users.insert(name.to_string(), u);
                }
                "role" => {
                    let permissions = v
                        .get("permissions")
                        .and_then(|p| p.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    let r = PlatformRole {
                        name: name.to_string(),
                        builtin: v
                            .get("builtin")
                            .is_some_and(|b| matches!(b, JsonValue::Bool(true))),
                        status: jstr(v, "status"),
                        description: jstr(v, "description"),
                        permissions,
                    };
                    snap.roles.insert(name.to_string(), r);
                }
                "session" => {
                    let s = StoredSession {
                        token_hash: name.to_string(),
                        username: jstr(v, "username"),
                        expires_at_ms: v.get("expires_at_ms").and_then(|n| n.as_i64()).unwrap_or(0)
                            as u64,
                        revoked: v
                            .get("revoked")
                            .is_some_and(|b| matches!(b, JsonValue::Bool(true))),
                    };
                    snap.sessions.insert(name.to_string(), s);
                }
                _ => {}
            }
        }
        Ok(snap)
    }

    /// 校验会话 token 哈希:存在、未吊销、未过期、用户 ACTIVE。
    /// 命中返回用户名与角色权限集。
    pub fn validate_session(
        &self,
        token_hash: &str,
        now_ms: u64,
    ) -> Result<(String, Vec<String>), AuthError> {
        let s = self
            .sessions
            .get(token_hash)
            .ok_or(AuthError::InvalidToken)?;
        if s.revoked {
            return Err(AuthError::InvalidToken);
        }
        if s.expires_at_ms <= now_ms {
            return Err(AuthError::SessionExpired);
        }
        let u = self.users.get(&s.username).ok_or(AuthError::InvalidToken)?;
        if u.status != "ACTIVE" {
            return Err(AuthError::UserDisabled);
        }
        let perms = self
            .roles
            .get(&u.role)
            .map(|r| r.permissions.clone())
            .unwrap_or_default();
        Ok((u.username.clone(), perms))
    }
}

/// 事实 JSON 取字符串字段(缺省空串)
fn jstr(v: &JsonValue, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}

// ---------------------------------------------------------------------------
// 认证错误(如实上报语义:区分原因,不静默)
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum AuthError {
    InvalidToken,
    SessionExpired,
    UserDisabled,
    BadCredentials,
    Forbidden(String),
    Conflict(String),
    BadRequest(String),
    Storage(String),
}

impl AuthError {
    fn status(&self) -> StatusCode {
        match self {
            AuthError::InvalidToken | AuthError::SessionExpired => StatusCode::UNAUTHORIZED,
            AuthError::UserDisabled => StatusCode::UNAUTHORIZED,
            AuthError::BadCredentials => StatusCode::UNAUTHORIZED,
            AuthError::Forbidden(_) => StatusCode::FORBIDDEN,
            AuthError::Conflict(_) => StatusCode::CONFLICT,
            AuthError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AuthError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
    fn message(&self) -> String {
        match self {
            AuthError::InvalidToken => "无效或已吊销的会话".into(),
            AuthError::SessionExpired => "会话已过期,请重新登录".into(),
            AuthError::UserDisabled => "用户已被停用".into(),
            AuthError::BadCredentials => "用户名或密码错误".into(),
            AuthError::Forbidden(m) => m.clone(),
            AuthError::Conflict(m) => m.clone(),
            AuthError::BadRequest(m) => m.clone(),
            AuthError::Storage(m) => m.clone(),
        }
    }
}

type ApiResult =
    Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)>;

impl From<AuthError> for (StatusCode, Json<serde_json::Value>) {
    fn from(e: AuthError) -> Self {
        err_json(e)
    }
}

fn ok_json(status: StatusCode, v: serde_json::Value) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(v))
}

fn err_json(e: AuthError) -> (StatusCode, Json<serde_json::Value>) {
    (
        e.status(),
        Json(serde_json::json!({ "success": false, "message": e.message() })),
    )
}

// ---------------------------------------------------------------------------
// 密码(Argon2id)与会话 token
// ---------------------------------------------------------------------------

/// Argon2id 哈希口令(PHC 字符串格式,含盐)
pub fn hash_password(password: &str) -> Result<String, AuthError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AuthError::Storage(format!("口令哈希失败: {e}")))
}

/// 校验口令(Argon2id 恒定时间验证)
pub fn verify_password(password: &str, phc_hash: &str) -> bool {
    PasswordHash::new(phc_hash)
        .map(|parsed| {
            Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok()
        })
        .unwrap_or(false)
}

/// 生成 256-bit 随机会话 token(hex);返回 (明文, blake3 哈希)。
/// 库中只存哈希,明文仅在登录响应返回一次。
pub fn generate_token() -> Result<(String, String), AuthError> {
    use argon2::password_hash::rand_core::RngCore;
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let mut hex = String::with_capacity(64);
    for b in bytes {
        hex.push(char::from(HEX[(b >> 4) as usize]));
        hex.push(char::from(HEX[(b & 0x0f) as usize]));
    }
    let hash = blake3::hash(hex.as_bytes()).to_hex().to_string();
    Ok((hex, hash))
}

const HEX: &[u8; 16] = b"0123456789abcdef";

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 从 Authorization: Bearer <token> 提取并哈希
fn bearer_token_hash(headers: &HeaderMap) -> Result<String, AuthError> {
    let raw = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .ok_or(AuthError::InvalidToken)?;
    if raw.is_empty() {
        return Err(AuthError::InvalidToken);
    }
    Ok(Hasher::new()
        .update(raw.as_bytes())
        .finalize()
        .to_hex()
        .to_string())
}

// ---------------------------------------------------------------------------
// 事实写入(全部经 SharedFactsLog.append,自动入治理审计链)
// ---------------------------------------------------------------------------

fn append_fact(
    shared: &SharedFactsLog,
    path: &str,
    value: serde_json::Value,
) -> Result<(), AuthError> {
    shared
        .append(path, serde_to_tcb(value), GLOBAL_SESSION)
        .map(|_| ())
        .map_err(|e| AuthError::Storage(format!("平台授权事实写入失败: {e}")))
}

fn user_fact_path(username: &str) -> String {
    format!("{FACT_PREFIX}user.{username}")
}

fn role_fact_path(name: &str) -> String {
    format!("{FACT_PREFIX}role.{name}")
}

fn session_fact_path(token_hash: &str) -> String {
    format!("{FACT_PREFIX}session.{token_hash}")
}

/// 认证事件入链(append-only,唯一 path 由 时间戳+随机后缀 保证)
fn append_auth_event(shared: &SharedFactsLog, kind: &str, detail: serde_json::Value) {
    let mut suffix = String::new();
    let _ = generate_token().map(|(_, h)| suffix = h.chars().take(16).collect());
    let path = format!("{FACT_PREFIX}event.{}.{}", now_ms(), suffix);
    let value = serde_json::json!({ "kind": kind, "detail": detail });
    if let Err(e) = append_fact(shared, &path, value) {
        tracing::warn!("认证事件入链失败(kind={kind}): {}", e.message());
    }
}

/// 平台事件通用写入口(插件探活报警等非认证子系统复用)。
/// 与认证事件同管道:platform.event.{kind}.{unix_ms}{随机后缀} → SharedFactsLog
/// prev_hash 链 → /api/audit/platform-events 报表自动可见。kind 不含点。
pub fn append_platform_event(shared: &SharedFactsLog, kind: &str, detail: serde_json::Value) {
    append_auth_event(shared, kind, detail)
}

/// serde_json::Value → evorule_tcb::JsonValue(与 server.rs/main.rs 一致)
fn serde_to_tcb(v: serde_json::Value) -> JsonValue {
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
            JsonValue::Array(arr.into_iter().map(serde_to_tcb).collect())
        }
        serde_json::Value::Object(obj) => {
            let pairs: Vec<(String, JsonValue)> = obj
                .into_iter()
                .map(|(k, val)| (k, serde_to_tcb(val)))
                .collect();
            JsonValue::object_from_pairs_owned(pairs)
        }
    }
}

/// 首启 seed:内置 4 角色无任何 role 事实时写入(幂等:按事实回放判定)。
/// 在每个平台端点入口调用;并发首次调用可能重复追加同值事实,
/// last-write-wins 回放下结果一致,无害。
pub fn ensure_seed(shared: &SharedFactsLog) -> Result<(), AuthError> {
    let snap = PlatformSnapshot::replay(shared)?;
    if !snap.roles.is_empty() {
        return Ok(());
    }
    for (name, perms) in BUILTIN_ROLES {
        append_fact(
            shared,
            &role_fact_path(name),
            serde_json::json!({
                "builtin": true,
                "status": "ACTIVE",
                "description": builtin_role_description(name),
                "permissions": perms,
            }),
        )?;
    }
    tracing::info!("平台授权:已 seed {} 个内置角色", BUILTIN_ROLES.len());
    Ok(())
}

fn builtin_role_description(name: &str) -> &'static str {
    match name {
        "administrator" => "内置管理员(权限集不可修改)",
        "approver" => "内置审批人",
        "rule_engineer" => "内置规则工程师",
        "viewer" => "内置查看者",
        _ => "内置角色",
    }
}

// ---------------------------------------------------------------------------
// 请求体
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize, ToSchema)]
pub struct CredentialsReq {
    pub username: String,
    pub password: String,
}

#[derive(serde::Deserialize, ToSchema)]
pub struct BootstrapReq {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub display_name: String,
}

#[derive(serde::Deserialize, ToSchema)]
pub struct ChangePasswordReq {
    pub old_password: String,
    pub new_password: String,
}

// ---------------------------------------------------------------------------
// Handlers（bootstrap / login / logout / me / change-password)
// ---------------------------------------------------------------------------

/// 平台授权路由(bootstrap/login/status 公开;其余在 handler 内自校验平台 token)。
/// W2b 将把全部 API 统一纳入全局认证中间件语义(范围待定)。
pub fn platform_auth_router() -> Router<AppState> {
    Router::new()
        .route("/api/platform/auth/bootstrap", post(bootstrap))
        .route("/api/platform/auth/login", post(login))
        .route("/api/platform/auth/logout", post(logout))
        .route("/api/platform/auth/me", get(me))
        .route("/api/platform/auth/status", get(auth_status))
        .route("/api/platform/auth/change-password", post(change_password))
        .route("/api/platform/permissions", get(list_permissions))
        .route("/api/platform/users", get(list_users).post(create_user))
        .route(
            "/api/platform/users/{username}",
            patch(update_user).delete(delete_user),
        )
        .route("/api/platform/roles", get(list_roles).post(create_role))
        .route(
            "/api/platform/roles/{name}",
            patch(update_role).delete(delete_role),
        )
}

use crate::api::server::AppState;

/// `POST /api/platform/auth/bootstrap` — 首启创建管理员。
/// 仅当平台无任何用户时可用(幂等保护);成功即登录态建立的前置。
#[utoipa::path(
    post,
    path = "/api/platform/auth/bootstrap",
    tag = "platform-auth",
    request_body = BootstrapReq,
    responses(
        (status = 201, description = "管理员已创建(role=administrator)", body = serde_json::Value),
        (status = 400, description = "用户名非法或密码长度不足 8 位", body = serde_json::Value),
        (status = 409, description = "平台已存在用户,bootstrap 不可用", body = serde_json::Value),
        (status = 500, description = "口令哈希/事实写入失败", body = serde_json::Value)
    )
)]
async fn bootstrap(
    State(shared): State<SharedFactsLog>,
    Json(req): Json<BootstrapReq>,
) -> ApiResult {
    ensure_seed(&shared)?;
    validate_username(&req.username)?;
    if req.password.len() < 8 {
        return Err(err_json(AuthError::BadRequest("密码长度至少 8 位".into())));
    }
    let snap = PlatformSnapshot::replay(&shared)?;
    if !snap.users.is_empty() {
        return Err(err_json(AuthError::Conflict(
            "平台已存在用户,bootstrap 不可用;请直接登录".into(),
        )));
    }
    let hash = hash_password(&req.password)?;
    append_fact(
        &shared,
        &user_fact_path(&req.username),
        serde_json::json!({
            "display_name": if req.display_name.is_empty() { req.username.clone() } else { req.display_name },
            "email": "",
            "department": "",
            "password_hash": hash,
            "status": "ACTIVE",
            "role": "administrator",
        }),
    )?;
    append_auth_event(
        &shared,
        "bootstrap_admin",
        serde_json::json!({ "username": req.username }),
    );
    tracing::info!("平台授权:管理员 {}/ 已创建(bootstrap)", req.username);
    Ok(ok_json(
        StatusCode::CREATED,
        serde_json::json!({ "success": true, "username": req.username }),
    ))
}

/// `POST /api/platform/auth/login` — 登录。
/// 返回 { token, user(不含哈希), permissions, permissions_version }。
/// 失败如实区分:凭据错误 / 用户停用(均 401,审计链记录 login_failed)。
#[utoipa::path(
    post,
    path = "/api/platform/auth/login",
    tag = "platform-auth",
    request_body = CredentialsReq,
    responses(
        (status = 200, description = "登录成功,返回会话 token(明文仅此一次)与权限集", body = serde_json::Value),
        (status = 401, description = "用户名/密码错误或用户已停用", body = serde_json::Value),
        (status = 500, description = "事实写入失败", body = serde_json::Value)
    )
)]
async fn login(State(shared): State<SharedFactsLog>, Json(req): Json<CredentialsReq>) -> ApiResult {
    ensure_seed(&shared)?;
    let snap = PlatformSnapshot::replay(&shared)?;
    let user = snap.users.get(&req.username);
    let verified = match user {
        Some(u) => verify_password(&req.password, &u.password_hash),
        None => {
            // 防用户枚举:对不存在用户也做一次哈希校验耗时对齐
            let _ = verify_password(&req.password, DUMMY_HASH);
            false
        }
    };
    if !verified {
        append_auth_event(
            &shared,
            "login_failed",
            serde_json::json!({ "username": req.username }),
        );
        return Err(err_json(AuthError::BadCredentials));
    }
    let Some(user) = user else {
        // 不可达(verified=true 蕴含 user 存在),但按门禁要求不使用 expect
        return Err(err_json(AuthError::BadCredentials));
    };
    if user.status != "ACTIVE" {
        append_auth_event(
            &shared,
            "login_rejected_disabled",
            serde_json::json!({ "username": req.username }),
        );
        return Err(err_json(AuthError::UserDisabled));
    }
    let (token, token_hash) = generate_token()?;
    let expires_at_ms = now_ms() + SESSION_TTL_MS;
    append_fact(
        &shared,
        &session_fact_path(&token_hash),
        serde_json::json!({ "username": req.username, "expires_at_ms": expires_at_ms, "revoked": false }),
    )?;
    let permissions = snap
        .roles
        .get(&user.role)
        .map(|r| r.permissions.clone())
        .unwrap_or_default();
    append_auth_event(
        &shared,
        "login_success",
        serde_json::json!({ "username": req.username }),
    );
    Ok(ok_json(
        StatusCode::OK,
        serde_json::json!({
            "success": true,
            "token": token,
            "expires_at_ms": expires_at_ms,
            "user": {
                "username": user.username,
                "displayName": user.display_name,
                "email": user.email,
                "department": user.department,
                "role": user.role,
            },
            "permissions": permissions,
            "permissions_version": snap.version,
        }),
    ))
}

/// 防用户枚举用的 dummy PHC(真实 Argon2id 格式,校验必失败但耗时对齐)。
/// 生成于编译示例口令,非任何真实凭据。
const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$OXJLd1pEUHQ2VzR2UWc$dVTJx8LwBvnrQOnAWhlgLb9uMqfnDGmVUXeRQHhUXEM";

/// 平台会话校验失败响应（HTTP 状态 + JSON 错误体）
type SessionAuthFailure = (StatusCode, Json<serde_json::Value>);

/// 平台会话校验通过上下文（快照, 用户名, 权限集）
type SessionContext = (PlatformSnapshot, String, Vec<String>);

/// 从请求头提取平台会话并校验,返回 (快照, 用户名, 权限集)
fn require_session(
    shared: &SharedFactsLog,
    headers: &HeaderMap,
) -> Result<SessionContext, SessionAuthFailure> {
    let token_hash = bearer_token_hash(headers).map_err(err_json)?;
    let snap = PlatformSnapshot::replay(shared).map_err(err_json)?;
    let (username, perms) = snap
        .validate_session(&token_hash, now_ms())
        .map_err(err_json)?;
    Ok((snap, username, perms))
}

// ---------------------------------------------------------------------------
// W2b:业务 API 统一认证中间件(双凭据 + 统一 401 语义)
// ---------------------------------------------------------------------------

/// 统一 401 响应体(与平台端点错误形状一致:success/message)
fn unauthorized_response() -> axum::response::Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({
            "success": false,
            "message": "未认证或凭据已失效;请先登录获取平台会话 token",
        })),
    )
        .into_response()
}

/// 业务 API 统一认证中间件(挂 protected_routes)。
///
/// **双凭据语义**:
///
/// 1. AuthConfig 未启用(开发模式)→ 放行,语义不变;
/// 2. Bearer token 命中静态 user/service token → 放行并注入
///    [`crate::auth::CallerIdentity`](evo-agent 侧车审计桥走此通道,即"白名单");
/// 3. 否则按平台会话校验(库存 blake3 哈希)→ 命中注入 `CallerIdentity::User`
///    (平台用户等同普通 user 凭据,不可写受保护域 `shared.*.stable.*`);
/// 4. 全部未命中 → 401 + 统一 JSON 错误体(此前为空 body 的裸状态码)。
///
/// 403 语义由端点层自理:平台管理端点在 handler 内校验权限点。
///
/// (: 直返 `Response`——原 `Result<Response, Response>` 两分支都产出
/// Response,Err 包装无语义且触发 clippy result_large_err(Response ≥128 字节))
pub async fn unified_auth_middleware(
    State((auth_config, shared)): State<(crate::auth::AuthConfig, SharedFactsLog)>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if !auth_config.is_enabled() {
        return next.run(req).await;
    }
    let raw = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_string();
    if !raw.is_empty() && auth_config.validate(&raw) {
        let identity = auth_config.identity(&raw);
        req.extensions_mut().insert(identity);
        return next.run(req).await;
    }
    // 平台会话凭据(非空才尝试;空 token 直接 401,与静态路径 N1 规则一致)
    if raw.is_empty() {
        return unauthorized_response();
    }
    let token_hash = Hasher::new()
        .update(raw.as_bytes())
        .finalize()
        .to_hex()
        .to_string();
    let snap = match PlatformSnapshot::replay(&shared) {
        Ok(snap) => snap,
        Err(_) => return unauthorized_response(),
    };
    match snap.validate_session(&token_hash, now_ms()) {
        Ok((username, _perms)) => {
            req.extensions_mut()
                .insert(crate::auth::CallerIdentity::User);
            tracing::debug!(username = %username, "平台会话认证通过");
            next.run(req).await
        }
        Err(_) => unauthorized_response(),
    }
}

/// `POST /api/platform/auth/logout` — 吊销当前会话(幂等)。
#[utoipa::path(
    post,
    path = "/api/platform/auth/logout",
    tag = "platform-auth",
    responses(
        (status = 200, description = "会话已吊销(重复登出幂等成功)", body = serde_json::Value),
        (status = 401, description = "无效或已吊销的会话", body = serde_json::Value),
        (status = 500, description = "事实写入失败", body = serde_json::Value)
    )
)]
async fn logout(State(shared): State<SharedFactsLog>, headers: HeaderMap) -> ApiResult {
    let token_hash = bearer_token_hash(&headers).map_err(err_json)?;
    let snap = PlatformSnapshot::replay(&shared).map_err(err_json)?;
    let s = snap
        .sessions
        .get(&token_hash)
        .ok_or_else(|| err_json(AuthError::InvalidToken))?;
    if !s.revoked {
        append_fact(
            &shared,
            &session_fact_path(&token_hash),
            serde_json::json!({ "username": s.username, "expires_at_ms": s.expires_at_ms, "revoked": true }),
        )?;
    }
    Ok(ok_json(
        StatusCode::OK,
        serde_json::json!({ "success": true }),
    ))
}

/// `GET /api/platform/auth/me` — 当前用户 + 最新权限矩阵。
/// 前端以此刷新 can 缓存(permissions_version 变化即授权有变更)。
#[utoipa::path(
    get,
    path = "/api/platform/auth/me",
    tag = "platform-auth",
    responses(
        (status = 200, description = "当前用户档案(不含口令哈希)与最新权限矩阵", body = serde_json::Value),
        (status = 401, description = "未认证/会话失效", body = serde_json::Value),
        (status = 500, description = "事实回放失败", body = serde_json::Value)
    )
)]
async fn me(State(shared): State<SharedFactsLog>, headers: HeaderMap) -> ApiResult {
    let (snap, username, perms) = require_session(&shared, &headers)?;
    let user = snap
        .users
        .get(&username)
        .ok_or_else(|| err_json(AuthError::InvalidToken))?;
    Ok(ok_json(
        StatusCode::OK,
        serde_json::json!({
            "success": true,
            "user": {
                "username": user.username,
                "displayName": user.display_name,
                "email": user.email,
                "department": user.department,
                "role": user.role,
            },
            "permissions": perms,
            "permissions_version": snap.version,
        }),
    ))
}

/// `POST /api/platform/auth/change-password` — 本人改密(需旧密码)。
#[utoipa::path(
    post,
    path = "/api/platform/auth/change-password",
    tag = "platform-auth",
    request_body = ChangePasswordReq,
    responses(
        (status = 200, description = "密码已修改", body = serde_json::Value),
        (status = 400, description = "新密码长度不足 8 位", body = serde_json::Value),
        (status = 401, description = "未认证或旧密码错误", body = serde_json::Value),
        (status = 500, description = "口令哈希/事实写入失败", body = serde_json::Value)
    )
)]
async fn change_password(
    State(shared): State<SharedFactsLog>,
    headers: HeaderMap,
    Json(req): Json<ChangePasswordReq>,
) -> ApiResult {
    let (snap, username, _perms) = require_session(&shared, &headers)?;
    if req.new_password.len() < 8 {
        return Err(err_json(AuthError::BadRequest(
            "新密码长度至少 8 位".into(),
        )));
    }
    let user = snap
        .users
        .get(&username)
        .ok_or_else(|| err_json(AuthError::InvalidToken))?;
    if !verify_password(&req.old_password, &user.password_hash) {
        append_auth_event(
            &shared,
            "change_password_failed",
            serde_json::json!({ "username": username }),
        );
        return Err(err_json(AuthError::BadCredentials));
    }
    let hash = hash_password(&req.new_password)?;
    append_fact(
        &shared,
        &user_fact_path(&username),
        serde_json::json!({
            "display_name": user.display_name,
            "email": user.email,
            "department": user.department,
            "password_hash": hash,
            "status": user.status,
            "role": user.role,
        }),
    )?;
    append_auth_event(
        &shared,
        "change_password",
        serde_json::json!({ "username": username }),
    );
    Ok(ok_json(
        StatusCode::OK,
        serde_json::json!({ "success": true, "permissions_version": snap.version }),
    ))
}

// ---------------------------------------------------------------------------
// 管理端点（用户管理 / 角色管理 / 权限点注册表)
// ---------------------------------------------------------------------------

/// 认证 + 单权限点校验
fn require_permission(
    shared: &SharedFactsLog,
    headers: &HeaderMap,
    action: &str,
) -> Result<(PlatformSnapshot, String), (StatusCode, Json<serde_json::Value>)> {
    let (snap, username, perms) = require_session(shared, headers)?;
    if !perms.iter().any(|p| p == action) {
        return Err(err_json(AuthError::Forbidden(format!(
            "缺少权限: {action}"
        ))));
    }
    Ok((snap, username))
}

/// 认证 + 任一权限点校验(查看类端点两种角色都可见)
fn require_any_permission(
    shared: &SharedFactsLog,
    headers: &HeaderMap,
    actions: &[&str],
) -> Result<PlatformSnapshot, (StatusCode, Json<serde_json::Value>)> {
    let (snap, _, perms) = require_session(shared, headers)?;
    if !actions.iter().any(|a| perms.iter().any(|p| p == a)) {
        return Err(err_json(AuthError::Forbidden(format!(
            "缺少权限: {}(其一)",
            actions.join(" / ")
        ))));
    }
    Ok(snap)
}

fn user_json(u: &PlatformUser) -> serde_json::Value {
    serde_json::json!({
        "username": u.username,
        "displayName": u.display_name,
        "email": u.email,
        "department": u.department,
        "status": u.status,
        "role": u.role,
    })
}

fn role_json(r: &PlatformRole) -> serde_json::Value {
    serde_json::json!({
        "name": r.name,
        "builtin": r.builtin,
        "status": r.status,
        "description": r.description,
        "permissions": r.permissions,
    })
}

fn validate_name(name: &str, label: &str) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let ok = !name.trim().is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.');
    if ok {
        Ok(())
    } else {
        Err(err_json(AuthError::BadRequest(format!(
            "{label}仅允许字母/数字/_-.,长度 1-64"
        ))))
    }
}

fn validate_username(username: &str) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    validate_name(username, "用户名")
}

/// 权限点子集校验(注册表内置,不允许未知权限点)
fn validate_permissions(perms: &[String]) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    for p in perms {
        if !PLATFORM_ACTIONS.contains(&p.as_str()) {
            return Err(err_json(AuthError::BadRequest(format!(
                "未知权限点: {p}(权限点注册表内置,不可自创)"
            ))));
        }
    }
    Ok(())
}

/// 目标角色必须存在且 ACTIVE
fn validate_role_assignment(
    snap: &PlatformSnapshot,
    role: &str,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    match snap.roles.get(role) {
        None => Err(err_json(AuthError::BadRequest(format!(
            "角色不存在: {role}"
        )))),
        Some(r) if r.status != "ACTIVE" => Err(err_json(AuthError::BadRequest(format!(
            "角色已停用,不可分配: {role}"
        )))),
        Some(_) => Ok(()),
    }
}

/// 平台必须始终保留至少一名 ACTIVE 管理员(排除 target 后计数)。
/// 用于:停用/降级最后一名管理员、删除管理员账号的拦截。
fn ensure_other_active_admin(
    snap: &PlatformSnapshot,
    target: &str,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let other_admins = snap
        .users
        .values()
        .filter(|u| u.role == "administrator" && u.status == "ACTIVE" && u.username != target)
        .count();
    if other_admins == 0 {
        return Err(err_json(AuthError::Conflict(
            "平台必须保留至少一名 ACTIVE 管理员;请先提升其他用户为管理员".into(),
        )));
    }
    Ok(())
}

/// `GET /api/platform/auth/status` — 公开:登录页判断是否需要 bootstrap 引导。
/// :同时下发演示登录入口开关(demo_auth),登录页据此隐藏演示模式入口。
#[utoipa::path(
    get,
    path = "/api/platform/auth/status",
    tag = "platform-auth",
    responses(
        (status = 200, description = "引导状态(needs_bootstrap)与演示登录开关(demo_auth)", body = serde_json::Value),
        (status = 500, description = "事实回放失败", body = serde_json::Value)
    )
)]
async fn auth_status(
    State(shared): State<SharedFactsLog>,
    State(demo): State<crate::api::server::DemoAuthFlag>,
) -> ApiResult {
    let snap = PlatformSnapshot::replay(&shared).map_err(err_json)?;
    Ok(ok_json(
        StatusCode::OK,
        serde_json::json!({
            "success": true,
            "needs_bootstrap": snap.users.is_empty(),
            "demo_auth": demo.0,
        }),
    ))
}

/// `GET /api/platform/permissions` — 权限点注册表(登录用户可读,角色编辑器渲染用)。
#[utoipa::path(
    get,
    path = "/api/platform/permissions",
    tag = "platform-auth",
    responses(
        (status = 200, description = "权限点注册表(actions)与内置角色(builtin_roles)", body = serde_json::Value),
        (status = 401, description = "未认证", body = serde_json::Value),
        (status = 500, description = "事实回放失败", body = serde_json::Value)
    )
)]
async fn list_permissions(State(shared): State<SharedFactsLog>, headers: HeaderMap) -> ApiResult {
    require_session(&shared, &headers)?;
    let builtin_roles: Vec<serde_json::Value> = BUILTIN_ROLES
        .iter()
        .map(|(name, perms)| {
            serde_json::json!({ "name": name, "builtin": true, "permissions": perms })
        })
        .collect();
    Ok(ok_json(
        StatusCode::OK,
        serde_json::json!({
            "success": true,
            "actions": PLATFORM_ACTIONS,
            "builtin_roles": builtin_roles,
        }),
    ))
}

/// `GET /api/platform/users` — 用户列表(view_users 或 manage_users)。
#[utoipa::path(
    get,
    path = "/api/platform/users",
    tag = "platform-auth",
    responses(
        (status = 200, description = "用户列表(不含口令哈希)", body = serde_json::Value),
        (status = 401, description = "未认证", body = serde_json::Value),
        (status = 403, description = "缺少 view_users / manage_users 权限", body = serde_json::Value),
        (status = 500, description = "事实回放失败", body = serde_json::Value)
    )
)]
async fn list_users(State(shared): State<SharedFactsLog>, headers: HeaderMap) -> ApiResult {
    let snap = require_any_permission(&shared, &headers, &["view_users", "manage_users"])?;
    let users: Vec<serde_json::Value> = snap.users.values().map(user_json).collect();
    Ok(ok_json(
        StatusCode::OK,
        serde_json::json!({ "success": true, "users": users, "version": snap.version }),
    ))
}

#[derive(serde::Deserialize, ToSchema)]
pub struct CreateUserReq {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub department: String,
    pub role: String,
}

/// `POST /api/platform/users` — 创建用户(manage_users)。
#[utoipa::path(
    post,
    path = "/api/platform/users",
    tag = "platform-auth",
    request_body = CreateUserReq,
    responses(
        (status = 201, description = "用户已创建", body = serde_json::Value),
        (status = 400, description = "用户名/密码非法或角色不存在/已停用", body = serde_json::Value),
        (status = 401, description = "未认证", body = serde_json::Value),
        (status = 403, description = "缺少 manage_users 权限", body = serde_json::Value),
        (status = 409, description = "用户已存在", body = serde_json::Value),
        (status = 500, description = "口令哈希/事实写入失败", body = serde_json::Value)
    )
)]
async fn create_user(
    State(shared): State<SharedFactsLog>,
    headers: HeaderMap,
    Json(req): Json<CreateUserReq>,
) -> ApiResult {
    let (snap, _caller) = require_permission(&shared, &headers, "manage_users")?;
    validate_username(&req.username)?;
    if req.password.len() < 8 {
        return Err(err_json(AuthError::BadRequest("密码长度至少 8 位".into())));
    }
    if snap.users.contains_key(&req.username) {
        return Err(err_json(AuthError::Conflict(format!(
            "用户已存在: {}",
            req.username
        ))));
    }
    validate_role_assignment(&snap, &req.role)?;
    let hash = hash_password(&req.password)?;
    let display_name = if req.display_name.is_empty() {
        req.username.clone()
    } else {
        req.display_name
    };
    append_fact(
        &shared,
        &user_fact_path(&req.username),
        serde_json::json!({
            "display_name": display_name,
            "email": req.email,
            "department": req.department,
            "password_hash": hash,
            "status": "ACTIVE",
            "role": req.role,
        }),
    )?;
    append_auth_event(
        &shared,
        "user_created",
        serde_json::json!({ "username": req.username, "role": req.role, "by": _caller }),
    );
    Ok(ok_json(
        StatusCode::CREATED,
        serde_json::json!({ "success": true, "username": req.username }),
    ))
}

#[derive(serde::Deserialize, ToSchema)]
pub struct UpdateUserReq {
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub department: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
}

/// `PATCH /api/platform/users/{username}` — 部分更新用户档案/角色/状态(manage_users)。
///
/// 保护规则:不能停用自己的账号;不能停用/降级最后一名 ACTIVE 管理员。
#[utoipa::path(
    patch,
    path = "/api/platform/users/{username}",
    tag = "platform-auth",
    params(("username" = String, Path, description = "用户名")),
    request_body = UpdateUserReq,
    responses(
        (status = 200, description = "用户档案已更新", body = serde_json::Value),
        (status = 400, description = "status 取值非法或角色不存在/已停用", body = serde_json::Value),
        (status = 401, description = "未认证", body = serde_json::Value),
        (status = 403, description = "缺少 manage_users 权限/停用自己/最后一名管理员保护", body = serde_json::Value),
        (status = 409, description = "用户不存在", body = serde_json::Value),
        (status = 500, description = "事实写入失败", body = serde_json::Value)
    )
)]
async fn update_user(
    State(shared): State<SharedFactsLog>,
    headers: HeaderMap,
    axum::extract::Path(username): axum::extract::Path<String>,
    Json(req): Json<UpdateUserReq>,
) -> ApiResult {
    let (snap, _caller) = require_permission(&shared, &headers, "manage_users")?;
    let Some(user) = snap.users.get(&username) else {
        return Err(err_json(AuthError::Conflict(format!(
            "用户不存在: {username}"
        ))));
    };
    let display_name = req
        .display_name
        .unwrap_or_else(|| user.display_name.clone());
    let email = req.email.unwrap_or_else(|| user.email.clone());
    let department = req.department.unwrap_or_else(|| user.department.clone());
    let role = req.role.unwrap_or_else(|| user.role.clone());
    let status = req.status.unwrap_or_else(|| user.status.clone());
    if status != "ACTIVE" && status != "DISABLED" {
        return Err(err_json(AuthError::BadRequest(
            "status 仅允许 ACTIVE | DISABLED".into(),
        )));
    }
    validate_role_assignment(&snap, &role)?;
    // 自我保护:不能停用自己的账号(防误操作锁死自己)
    if username == _caller && status == "DISABLED" {
        return Err(err_json(AuthError::Forbidden("不能停用自己的账号".into())));
    }
    // 最后管理员保护:目标为 ACTIVE 管理员且操作会使其失去管理员/停用
    if user.role == "administrator"
        && user.status == "ACTIVE"
        && (role != "administrator" || status == "DISABLED")
    {
        ensure_other_active_admin(&snap, &username)?;
    }
    append_fact(
        &shared,
        &user_fact_path(&username),
        serde_json::json!({
            "display_name": display_name,
            "email": email,
            "department": department,
            "password_hash": user.password_hash,
            "status": status,
            "role": role,
        }),
    )?;
    append_auth_event(
        &shared,
        "user_updated",
        serde_json::json!({ "username": username, "by": _caller }),
    );
    Ok(ok_json(
        StatusCode::OK,
        serde_json::json!({ "success": true }),
    ))
}

/// `DELETE /api/platform/users/{username}` — 删除用户(墓碑事实,manage_users)。
///
/// 保护规则:不能删除自己;不能删除最后一名 ACTIVE 管理员。
/// 用户被删后其全部会话立即失效(回放后无此用户,validate_session 报 InvalidToken)。
#[utoipa::path(
    delete,
    path = "/api/platform/users/{username}",
    tag = "platform-auth",
    params(("username" = String, Path, description = "用户名")),
    responses(
        (status = 200, description = "用户已删除(墓碑事实,历史保留)", body = serde_json::Value),
        (status = 401, description = "未认证", body = serde_json::Value),
        (status = 403, description = "缺少 manage_users 权限/删除自己/最后一名管理员保护", body = serde_json::Value),
        (status = 409, description = "用户不存在", body = serde_json::Value),
        (status = 500, description = "事实写入失败", body = serde_json::Value)
    )
)]
async fn delete_user(
    State(shared): State<SharedFactsLog>,
    headers: HeaderMap,
    axum::extract::Path(username): axum::extract::Path<String>,
) -> ApiResult {
    let (snap, _caller) = require_permission(&shared, &headers, "manage_users")?;
    let Some(user) = snap.users.get(&username) else {
        return Err(err_json(AuthError::Conflict(format!(
            "用户不存在: {username}"
        ))));
    };
    if username == _caller {
        return Err(err_json(AuthError::Forbidden("不能删除自己的账号".into())));
    }
    if user.role == "administrator" && user.status == "ACTIVE" {
        ensure_other_active_admin(&snap, &username)?;
    }
    append_fact(
        &shared,
        &user_fact_path(&username),
        serde_json::json!({ "deleted": true }),
    )?;
    append_auth_event(
        &shared,
        "user_deleted",
        serde_json::json!({ "username": username, "by": _caller }),
    );
    Ok(ok_json(
        StatusCode::OK,
        serde_json::json!({ "success": true }),
    ))
}

/// `GET /api/platform/roles` — 角色列表(登录用户可读,工作流中的角色引用需要)。
#[utoipa::path(
    get,
    path = "/api/platform/roles",
    tag = "platform-auth",
    responses(
        (status = 200, description = "角色列表(含内置角色与权限集)", body = serde_json::Value),
        (status = 401, description = "未认证", body = serde_json::Value),
        (status = 500, description = "事实回放失败", body = serde_json::Value)
    )
)]
async fn list_roles(State(shared): State<SharedFactsLog>, headers: HeaderMap) -> ApiResult {
    require_session(&shared, &headers)?;
    let snap = PlatformSnapshot::replay(&shared).map_err(err_json)?;
    let roles: Vec<serde_json::Value> = snap.roles.values().map(role_json).collect();
    Ok(ok_json(
        StatusCode::OK,
        serde_json::json!({ "success": true, "roles": roles, "version": snap.version }),
    ))
}

#[derive(serde::Deserialize, ToSchema)]
pub struct CreateRoleReq {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub permissions: Vec<String>,
}

/// `POST /api/platform/roles` — 创建自定义角色(manage_roles)。
#[utoipa::path(
    post,
    path = "/api/platform/roles",
    tag = "platform-auth",
    request_body = CreateRoleReq,
    responses(
        (status = 201, description = "自定义角色已创建", body = serde_json::Value),
        (status = 400, description = "角色名非法或包含未知权限点", body = serde_json::Value),
        (status = 401, description = "未认证", body = serde_json::Value),
        (status = 403, description = "缺少 manage_roles 权限", body = serde_json::Value),
        (status = 409, description = "角色已存在", body = serde_json::Value),
        (status = 500, description = "事实写入失败", body = serde_json::Value)
    )
)]
async fn create_role(
    State(shared): State<SharedFactsLog>,
    headers: HeaderMap,
    Json(req): Json<CreateRoleReq>,
) -> ApiResult {
    let (snap, _caller) = require_permission(&shared, &headers, "manage_roles")?;
    validate_name(&req.name, "角色名")?;
    if snap.roles.contains_key(&req.name) {
        return Err(err_json(AuthError::Conflict(format!(
            "角色已存在: {}",
            req.name
        ))));
    }
    validate_permissions(&req.permissions)?;
    append_fact(
        &shared,
        &role_fact_path(&req.name),
        serde_json::json!({
            "builtin": false,
            "status": "ACTIVE",
            "description": req.description,
            "permissions": req.permissions,
        }),
    )?;
    append_auth_event(
        &shared,
        "role_created",
        serde_json::json!({ "name": req.name, "by": _caller }),
    );
    Ok(ok_json(
        StatusCode::CREATED,
        serde_json::json!({ "success": true, "name": req.name }),
    ))
}

#[derive(serde::Deserialize, ToSchema)]
pub struct UpdateRoleReq {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub permissions: Option<Vec<String>>,
}

/// `PATCH /api/platform/roles/{name}` — 更新角色(manage_roles)。
///
/// 保护规则:内置角色不可停用;administrator 权限集不可修改(计划 D4);
/// 其余内置角色权限集可调整(计划 §5 D4:内置不可删,administrator 单独锁权限集)。
#[utoipa::path(
    patch,
    path = "/api/platform/roles/{name}",
    tag = "platform-auth",
    params(("name" = String, Path, description = "角色名")),
    request_body = UpdateRoleReq,
    responses(
        (status = 200, description = "角色已更新", body = serde_json::Value),
        (status = 400, description = "status 取值非法或包含未知权限点", body = serde_json::Value),
        (status = 401, description = "未认证", body = serde_json::Value),
        (status = 403, description = "缺少 manage_roles 权限/内置角色不可停用/内置管理员权限集不可修改", body = serde_json::Value),
        (status = 409, description = "角色不存在", body = serde_json::Value),
        (status = 500, description = "事实写入失败", body = serde_json::Value)
    )
)]
async fn update_role(
    State(shared): State<SharedFactsLog>,
    headers: HeaderMap,
    axum::extract::Path(name): axum::extract::Path<String>,
    Json(req): Json<UpdateRoleReq>,
) -> ApiResult {
    let (snap, _caller) = require_permission(&shared, &headers, "manage_roles")?;
    let Some(role) = snap.roles.get(&name) else {
        return Err(err_json(AuthError::Conflict(format!("角色不存在: {name}"))));
    };
    let description = req.description.unwrap_or_else(|| role.description.clone());
    let status = req.status.unwrap_or_else(|| role.status.clone());
    if status != "ACTIVE" && status != "DISABLED" {
        return Err(err_json(AuthError::BadRequest(
            "status 仅允许 ACTIVE | DISABLED".into(),
        )));
    }
    if role.builtin && status != "ACTIVE" {
        return Err(err_json(AuthError::Forbidden("内置角色不可停用".into())));
    }
    let permissions = match req.permissions {
        None => role.permissions.clone(),
        Some(p) => {
            if role.builtin && name == "administrator" {
                return Err(err_json(AuthError::Forbidden(
                    "内置管理员权限集不可修改".into(),
                )));
            }
            validate_permissions(&p)?;
            p
        }
    };
    append_fact(
        &shared,
        &role_fact_path(&name),
        serde_json::json!({
            "builtin": role.builtin,
            "status": status,
            "description": description,
            "permissions": permissions,
        }),
    )?;
    append_auth_event(
        &shared,
        "role_updated",
        serde_json::json!({ "name": name, "by": _caller }),
    );
    Ok(ok_json(
        StatusCode::OK,
        serde_json::json!({ "success": true }),
    ))
}

/// `DELETE /api/platform/roles/{name}` — 删除自定义角色(墓碑事实,manage_roles)。
///
/// 保护规则:内置角色不可删除;仍有用户挂靠时拒绝(计划 D4:删除前检查引用)。
#[utoipa::path(
    delete,
    path = "/api/platform/roles/{name}",
    tag = "platform-auth",
    params(("name" = String, Path, description = "角色名")),
    responses(
        (status = 200, description = "自定义角色已删除(墓碑事实)", body = serde_json::Value),
        (status = 401, description = "未认证", body = serde_json::Value),
        (status = 403, description = "缺少 manage_roles 权限/内置角色不可删除", body = serde_json::Value),
        (status = 409, description = "角色不存在或仍有用户挂靠", body = serde_json::Value),
        (status = 500, description = "事实写入失败", body = serde_json::Value)
    )
)]
async fn delete_role(
    State(shared): State<SharedFactsLog>,
    headers: HeaderMap,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> ApiResult {
    let (snap, _caller) = require_permission(&shared, &headers, "manage_roles")?;
    let Some(role) = snap.roles.get(&name) else {
        return Err(err_json(AuthError::Conflict(format!("角色不存在: {name}"))));
    };
    if role.builtin {
        return Err(err_json(AuthError::Forbidden("内置角色不可删除".into())));
    }
    let referenced = snap.users.values().filter(|u| u.role == name).count();
    if referenced > 0 {
        return Err(err_json(AuthError::Conflict(format!(
            "角色 {name} 仍有 {referenced} 个用户挂靠,请先迁移用户后再删除"
        ))));
    }
    append_fact(
        &shared,
        &role_fact_path(&name),
        serde_json::json!({ "deleted": true }),
    )?;
    append_auth_event(
        &shared,
        "role_deleted",
        serde_json::json!({ "name": name, "by": _caller }),
    );
    Ok(ok_json(
        StatusCode::OK,
        serde_json::json!({ "success": true }),
    ))
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]

    use super::*;

    fn shared_log() -> SharedFactsLog {
        SharedFactsLog::new()
    }

    #[test]
    fn test_password_hash_roundtrip() {
        let h = hash_password("s3cret-pass!").unwrap();
        assert!(h.starts_with("$argon2id$"));
        assert!(verify_password("s3cret-pass!", &h));
        assert!(!verify_password("wrong-pass", &h));
    }

    // ------------------------- W2:管理端点 -------------------------

    /// 断言式取错误状态码(避免 unwrap_err 触发 Json must_use 警告)
    trait ApiResultExt {
        fn err_status(self) -> StatusCode;
        fn expect_ok(self);
    }

    impl ApiResultExt
        for Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)>
    {
        fn err_status(self) -> StatusCode {
            match self {
                Ok(_) => panic!("预期失败,实际成功"),
                Err((s, _)) => s,
            }
        }
        fn expect_ok(self) {
            match self {
                Ok(_) => {}
                Err(e) => panic!("预期成功,实际失败: {e:?}"),
            }
        }
    }

    fn auth_headers(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        h
    }

    async fn login_token(shared: &SharedFactsLog, username: &str, password: &str) -> String {
        let (_, Json(v)) = login(
            State(shared.clone()),
            Json(CredentialsReq {
                username: username.into(),
                password: password.into(),
            }),
        )
        .await
        .unwrap();
        v["token"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn test_user_management_flow() {
        let shared = shared_log();
        ensure_seed(&shared).unwrap();
        let resp = bootstrap(
            State(shared.clone()),
            Json(BootstrapReq {
                username: "root".into(),
                password: "admin-pass-123".into(),
                display_name: String::new(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(resp.0, StatusCode::CREATED);
        let admin_token = login_token(&shared, "root", "admin-pass-123").await;
        let admin_h = auth_headers(&admin_token);

        // 建用户(角色未知 → 400)
        let bad_role = create_user(
            State(shared.clone()),
            admin_h.clone(),
            Json(CreateUserReq {
                username: "carol".into(),
                password: "carol-pass-123".into(),
                display_name: String::new(),
                email: String::new(),
                department: String::new(),
                role: "nonexistent".into(),
            }),
        )
        .await;
        assert_eq!(bad_role.err_status(), StatusCode::BAD_REQUEST);

        // 建用户成功
        let resp = create_user(
            State(shared.clone()),
            admin_h.clone(),
            Json(CreateUserReq {
                username: "carol".into(),
                password: "carol-pass-123".into(),
                display_name: String::new(),
                email: String::new(),
                department: String::new(),
                role: "viewer".into(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(resp.0, StatusCode::CREATED);
        // 重复用户名 → 409
        assert_eq!(
            create_user(
                State(shared.clone()),
                admin_h.clone(),
                Json(CreateUserReq {
                    username: "carol".into(),
                    password: "carol-pass-123".into(),
                    display_name: String::new(),
                    email: String::new(),
                    department: String::new(),
                    role: "viewer".into(),
                }),
            )
            .await
            .err_status(),
            StatusCode::CONFLICT
        );

        // viewer 无 manage_users → 403
        let viewer_token = login_token(&shared, "carol", "carol-pass-123").await;
        assert_eq!(
            create_user(
                State(shared.clone()),
                auth_headers(&viewer_token),
                Json(CreateUserReq {
                    username: "dave".into(),
                    password: "dave-pass-123".into(),
                    display_name: String::new(),
                    email: String::new(),
                    department: String::new(),
                    role: "viewer".into(),
                }),
            )
            .await
            .err_status(),
            StatusCode::FORBIDDEN
        );

        // 保护:root 停用自己 → 403;降级唯一管理员 → 409
        assert_eq!(
            update_user(
                State(shared.clone()),
                admin_h.clone(),
                axum::extract::Path("root".into()),
                Json(UpdateUserReq {
                    display_name: None,
                    email: None,
                    department: None,
                    role: None,
                    status: Some("DISABLED".into()),
                }),
            )
            .await
            .err_status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            update_user(
                State(shared.clone()),
                admin_h.clone(),
                axum::extract::Path("root".into()),
                Json(UpdateUserReq {
                    display_name: None,
                    email: None,
                    department: None,
                    role: Some("viewer".into()),
                    status: None,
                }),
            )
            .await
            .err_status(),
            StatusCode::CONFLICT
        );

        // 停用 carol → 其会话立即失效
        update_user(
            State(shared.clone()),
            admin_h.clone(),
            axum::extract::Path("carol".into()),
            Json(UpdateUserReq {
                display_name: None,
                email: None,
                department: None,
                role: None,
                status: Some("DISABLED".into()),
            }),
        )
        .await
        .expect_ok();
        // 停用后 carol 的会话校验报 UserDisabled
        let snap = PlatformSnapshot::replay(&shared).unwrap();
        assert!(snap.users.get("carol").unwrap().status == "DISABLED");
        assert!(matches!(
            require_session(&shared, &auth_headers(&viewer_token))
                .unwrap_err()
                .0,
            StatusCode::UNAUTHORIZED
        ));

        // 删除 carol(有 ACTIVE 管理员 root 在,允许)→ 回放后无此用户
        delete_user(
            State(shared.clone()),
            admin_h.clone(),
            axum::extract::Path("carol".into()),
        )
        .await
        .expect_ok();
        let snap = PlatformSnapshot::replay(&shared).unwrap();
        assert!(!snap.users.contains_key("carol"), "墓碑后用户应消失");
        // 被删用户会话 token 失效
        assert!(require_session(&shared, &auth_headers(&viewer_token)).is_err());
    }

    #[tokio::test]
    // 角色管理全流程用例(CRUD+内置保护断言),场景化测试不拆分
    #[allow(clippy::too_many_lines)]
    async fn test_role_management_flow() {
        let shared = shared_log();
        ensure_seed(&shared).unwrap();
        bootstrap(
            State(shared.clone()),
            Json(BootstrapReq {
                username: "root".into(),
                password: "admin-pass-123".into(),
                display_name: String::new(),
            }),
        )
        .await
        .expect_ok();
        let admin_token = login_token(&shared, "root", "admin-pass-123").await;
        let admin_h = auth_headers(&admin_token);

        // 未知权限点 → 400
        assert_eq!(
            create_role(
                State(shared.clone()),
                admin_h.clone(),
                Json(CreateRoleReq {
                    name: "analyst".into(),
                    description: String::new(),
                    permissions: vec!["view_monitor".into(), "made_up_perm".into()],
                }),
            )
            .await
            .err_status(),
            StatusCode::BAD_REQUEST
        );

        // 建自定义角色
        let resp = create_role(
            State(shared.clone()),
            admin_h.clone(),
            Json(CreateRoleReq {
                name: "analyst".into(),
                description: "数据分析".into(),
                permissions: vec!["view_monitor".into(), "view_test_report".into()],
            }),
        )
        .await
        .unwrap();
        assert_eq!(resp.0, StatusCode::CREATED);

        // 挂靠用户后删除 → 409;迁移后删除 → ok 且回放消失
        create_user(
            State(shared.clone()),
            admin_h.clone(),
            Json(CreateUserReq {
                username: "erin".into(),
                password: "erin-pass-123".into(),
                display_name: String::new(),
                email: String::new(),
                department: String::new(),
                role: "analyst".into(),
            }),
        )
        .await
        .expect_ok();
        assert_eq!(
            delete_role(
                State(shared.clone()),
                admin_h.clone(),
                axum::extract::Path("analyst".into()),
            )
            .await
            .err_status(),
            StatusCode::CONFLICT
        );
        // 迁移用户到 viewer
        update_user(
            State(shared.clone()),
            admin_h.clone(),
            axum::extract::Path("erin".into()),
            Json(UpdateUserReq {
                display_name: None,
                email: None,
                department: None,
                role: Some("viewer".into()),
                status: None,
            }),
        )
        .await
        .expect_ok();
        delete_role(
            State(shared.clone()),
            admin_h.clone(),
            axum::extract::Path("analyst".into()),
        )
        .await
        .expect_ok();
        let snap = PlatformSnapshot::replay(&shared).unwrap();
        assert!(!snap.roles.contains_key("analyst"), "墓碑后角色应消失");

        // 内置角色:不可删;administrator 权限集不可改;内置不可停用
        assert_eq!(
            delete_role(
                State(shared.clone()),
                admin_h.clone(),
                axum::extract::Path("viewer".into()),
            )
            .await
            .err_status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            update_role(
                State(shared.clone()),
                admin_h.clone(),
                axum::extract::Path("administrator".into()),
                Json(UpdateRoleReq {
                    description: None,
                    status: None,
                    permissions: Some(vec!["view_monitor".into()]),
                }),
            )
            .await
            .err_status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            update_role(
                State(shared.clone()),
                admin_h.clone(),
                axum::extract::Path("viewer".into()),
                Json(UpdateRoleReq {
                    description: None,
                    status: Some("DISABLED".into()),
                    permissions: None,
                }),
            )
            .await
            .err_status(),
            StatusCode::FORBIDDEN
        );

        // 自定义角色权限集可改
        create_role(
            State(shared.clone()),
            admin_h.clone(),
            Json(CreateRoleReq {
                name: "ops".into(),
                description: String::new(),
                permissions: vec!["view_monitor".into()],
            }),
        )
        .await
        .expect_ok();
        update_role(
            State(shared.clone()),
            admin_h.clone(),
            axum::extract::Path("ops".into()),
            Json(UpdateRoleReq {
                description: None,
                status: None,
                permissions: Some(vec!["view_monitor".into(), "view_audit_chain".into()]),
            }),
        )
        .await
        .expect_ok();
        let snap = PlatformSnapshot::replay(&shared).unwrap();
        assert_eq!(
            snap.roles.get("ops").unwrap().permissions,
            vec!["view_monitor".to_string(), "view_audit_chain".to_string()]
        );
    }

    #[tokio::test]
    async fn test_unified_auth_middleware_dual_credentials() {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        use axum::middleware;
        use tower::ServiceExt;

        let shared = shared_log();
        ensure_seed(&shared).unwrap();
        bootstrap(
            State(shared.clone()),
            Json(BootstrapReq {
                username: "root".into(),
                password: "admin-pass-123".into(),
                display_name: String::new(),
            }),
        )
        .await
        .expect_ok();
        let platform_token = login_token(&shared, "root", "admin-pass-123").await;

        let auth_config = crate::auth::AuthConfig::new(vec!["static-svc-token".into()], true);
        let app: Router = Router::new()
            .route("/api/ping", get(|| async { "ok" }))
            .layer(middleware::from_fn_with_state(
                (auth_config, shared.clone()),
                unified_auth_middleware,
            ));

        let send = |app: Router, token: Option<String>| {
            let mut builder = HttpRequest::builder().uri("/api/ping");
            if let Some(t) = token {
                builder = builder.header(axum::http::header::AUTHORIZATION, format!("Bearer {t}"));
            }
            app.oneshot(builder.body(Body::empty()).unwrap())
        };

        // 1. 无凭据 → 401 + 统一 JSON 错误体
        let resp = send(app.clone(), None).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["success"], serde_json::Value::Bool(false));

        // 2. 静态 token(侧车通道)→ 200
        let resp = send(app.clone(), Some("static-svc-token".into()))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // 3. 平台会话 token(console 通道)→ 200
        let resp = send(app.clone(), Some(platform_token)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // 4. 未知 token → 401
        let resp = send(app, Some("bogus-token".into())).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn test_token_generate_and_hash() {
        let (t1, h1) = generate_token().unwrap();
        let (t2, h2) = generate_token().unwrap();
        assert_eq!(t1.len(), 64);
        assert_ne!(t1, t2, "token 必须随机");
        assert_ne!(h1, h2);
        assert_eq!(
            blake3::hash(t1.as_bytes()).to_hex().to_string(),
            h1,
            "库存哈希 = blake3(明文)"
        );
    }

    #[test]
    fn test_seed_and_replay() {
        let shared = shared_log();
        ensure_seed(&shared).unwrap();
        // 幂等:再次 seed 不追加(roles 已非空)
        ensure_seed(&shared).unwrap();
        let snap = PlatformSnapshot::replay(&shared).unwrap();
        assert_eq!(snap.roles.len(), 4);
        let admin = snap.roles.get("administrator").unwrap();
        assert!(admin.builtin);
        assert_eq!(admin.permissions.len(), PLATFORM_ACTIONS.len());
        assert!(snap.users.is_empty());
    }

    #[test]
    fn test_session_validate_lifecycle() {
        let shared = shared_log();
        ensure_seed(&shared).unwrap();
        // 建用户 + 开会话
        let hash = hash_password("passw0rd-long").unwrap();
        append_fact(
            &shared,
            &user_fact_path("alice"),
            serde_json::json!({
                "display_name": "Alice", "email": "", "department": "",
                "password_hash": hash, "status": "ACTIVE", "role": "viewer",
            }),
        )
        .unwrap();
        let (_token, token_hash) = generate_token().unwrap();
        let expires = now_ms() + 1000;
        append_fact(
            &shared,
            &session_fact_path(&token_hash),
            serde_json::json!({ "username": "alice", "expires_at_ms": expires, "revoked": false }),
        )
        .unwrap();

        let snap = PlatformSnapshot::replay(&shared).unwrap();
        let (username, perms) = snap.validate_session(&token_hash, now_ms()).unwrap();
        assert_eq!(username, "alice");
        assert_eq!(
            perms,
            vec!["view_monitor".to_string(), "view_test_report".to_string()]
        );

        // 过期 → SessionExpired
        let snap2 = PlatformSnapshot::replay(&shared).unwrap();
        assert!(matches!(
            snap2.validate_session(&token_hash, expires + 1),
            Err(AuthError::SessionExpired)
        ));

        // 吊销(last-write-wins)→ InvalidToken
        append_fact(
            &shared,
            &session_fact_path(&token_hash),
            serde_json::json!({ "username": "alice", "expires_at_ms": expires, "revoked": true }),
        )
        .unwrap();
        let snap3 = PlatformSnapshot::replay(&shared).unwrap();
        assert!(matches!(
            snap3.validate_session(&token_hash, now_ms()),
            Err(AuthError::InvalidToken)
        ));

        // 用户停用 → UserDisabled(即使会话有效)
        let (t2, h2) = generate_token().unwrap();
        append_fact(
            &shared,
            &session_fact_path(&h2),
            serde_json::json!({ "username": "alice", "expires_at_ms": now_ms() + 1000, "revoked": false }),
        )
        .unwrap();
        append_fact(
            &shared,
            &user_fact_path("alice"),
            serde_json::json!({
                "display_name": "Alice", "email": "", "department": "",
                "password_hash": hash, "status": "DISABLED", "role": "viewer",
            }),
        )
        .unwrap();
        let snap4 = PlatformSnapshot::replay(&shared).unwrap();
        let _ = t2;
        assert!(matches!(
            snap4.validate_session(&h2, now_ms()),
            Err(AuthError::UserDisabled)
        ));
    }

    #[test]
    fn test_last_write_wins_on_user_update() {
        let shared = shared_log();
        ensure_seed(&shared).unwrap();
        let h1 = hash_password("password-one").unwrap();
        let h2 = hash_password("password-two").unwrap();
        for h in [&h1, &h2] {
            append_fact(
                &shared,
                &user_fact_path("bob"),
                serde_json::json!({
                    "display_name": "Bob", "email": "", "department": "",
                    "password_hash": h, "status": "ACTIVE", "role": "viewer",
                }),
            )
            .unwrap();
        }
        let snap = PlatformSnapshot::replay(&shared).unwrap();
        let bob = snap.users.get("bob").unwrap();
        assert!(verify_password("password-two", &bob.password_hash));
        assert!(!verify_password("password-one", &bob.password_hash));
    }

    #[test]
    fn test_dummy_hash_is_valid_phc_but_never_verifies() {
        // 防枚举 dummy:必须是合法 PHC 格式(解析不炸),且校验恒失败
        let parsed = PasswordHash::new(DUMMY_HASH).expect("dummy PHC 合法");
        assert!(Argon2::default()
            .verify_password(b"anything", &parsed)
            .is_err());
    }
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 平台用户体系与授权(UV-017 W1)
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
//! **认证检查边界(W1)**:bootstrap/login 公开;me/logout/change-password
//! 在 handler 内自校验平台 token。与其他 API 的统一 401/403 语义在 W2 接入
//! 全局 auth 中间件时完成。

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use blake3::Hasher;

use evorule_governance::shared_facts_log::SharedFactsLog;
use evorule_tcb::JsonValue;

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
    // 平台管理 3 点(UV-017 新增)
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
        let facts = shared
            .facts_by_path_prefix(FACT_PREFIX);
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
                        builtin: v.get("builtin").map_or(false, |b| {
                            matches!(b, JsonValue::Bool(true))
                        }),
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
                            .map_or(false, |b| matches!(b, JsonValue::Bool(true))),
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
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
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
    Forbidden(&'static str),
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
            AuthError::Forbidden(m) => (*m).into(),
            AuthError::Conflict(m) => m.clone(),
            AuthError::BadRequest(m) => m.clone(),
            AuthError::Storage(m) => m.clone(),
        }
    }
}

type ApiResult = Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)>;

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
    Ok(Hasher::new().update(raw.as_bytes()).finalize().to_hex().to_string())
}

// ---------------------------------------------------------------------------
// 事实写入(全部经 SharedFactsLog.append,自动入治理审计链)
// ---------------------------------------------------------------------------

fn append_fact(shared: &SharedFactsLog, path: &str, value: serde_json::Value) -> Result<(), AuthError> {
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
            let pairs: Vec<(String, JsonValue)> =
                obj.into_iter().map(|(k, val)| (k, serde_to_tcb(val))).collect();
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

#[derive(serde::Deserialize)]
pub struct CredentialsReq {
    pub username: String,
    pub password: String,
}

#[derive(serde::Deserialize)]
pub struct BootstrapReq {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub display_name: String,
}

#[derive(serde::Deserialize)]
pub struct ChangePasswordReq {
    pub old_password: String,
    pub new_password: String,
}

// ---------------------------------------------------------------------------
// Handlers(W1:bootstrap / login / logout / me / change-password)
// ---------------------------------------------------------------------------

/// 平台授权路由(bootstrap/login 公开;me 等在 handler 内自校验平台 token)。
/// W2 将把 me 类端点统一纳入全局认证中间件语义。
pub fn platform_auth_router() -> Router<AppState> {
    Router::new()
        .route("/api/platform/auth/bootstrap", post(bootstrap))
        .route("/api/platform/auth/login", post(login))
        .route("/api/platform/auth/logout", post(logout))
        .route("/api/platform/auth/me", get(me))
        .route("/api/platform/auth/change-password", post(change_password))
}

use crate::api::server::AppState;

/// `POST /api/platform/auth/bootstrap` — 首启创建管理员。
/// 仅当平台无任何用户时可用(幂等保护);成功即登录态建立的前置。
async fn bootstrap(
    State(shared): State<SharedFactsLog>,
    Json(req): Json<BootstrapReq>,
) -> ApiResult {
    ensure_seed(&shared)?;
    validate_username(&req.username)?;
    if req.password.len() < 8 {
        return Err(err_json(AuthError::BadRequest(
            "密码长度至少 8 位".into(),
        )));
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
    append_auth_event(&shared, "bootstrap_admin", serde_json::json!({ "username": req.username }));
    tracing::info!("平台授权:管理员 {}/ 已创建(bootstrap)", req.username);
    Ok(ok_json(
        StatusCode::CREATED,
        serde_json::json!({ "success": true, "username": req.username }),
    ))
}

fn validate_username(username: &str) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let ok = !username.trim().is_empty()
        && username.len() <= 64
        && username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.');
    if ok {
        Ok(())
    } else {
        Err(err_json(AuthError::BadRequest(
            "用户名仅允许字母/数字/_-.,长度 1-64".into(),
        )))
    }
}

/// `POST /api/platform/auth/login` — 登录。
/// 返回 { token, user(不含哈希), permissions, permissions_version }。
/// 失败如实区分:凭据错误 / 用户停用(均 401,审计链记录 login_failed)。
async fn login(
    State(shared): State<SharedFactsLog>,
    Json(req): Json<CredentialsReq>,
) -> ApiResult {
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
    append_auth_event(&shared, "login_success", serde_json::json!({ "username": req.username }));
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

/// 从请求头提取平台会话并校验,返回 (快照, 用户名, 权限集)
fn require_session(
    shared: &SharedFactsLog,
    headers: &HeaderMap,
) -> Result<(PlatformSnapshot, String, Vec<String>), (StatusCode, Json<serde_json::Value>)> {
    let token_hash = bearer_token_hash(headers).map_err(err_json)?;
    let snap = PlatformSnapshot::replay(shared).map_err(err_json)?;
    let (username, perms) = snap
        .validate_session(&token_hash, now_ms())
        .map_err(err_json)?;
    Ok((snap, username, perms))
}

/// `POST /api/platform/auth/logout` — 吊销当前会话(幂等)。
async fn logout(
    State(shared): State<SharedFactsLog>,
    headers: HeaderMap,
) -> ApiResult {
    let token_hash = bearer_token_hash(&headers).map_err(err_json)?;
    let snap = PlatformSnapshot::replay(&shared).map_err(err_json)?;
    let s = snap.sessions.get(&token_hash).ok_or_else(|| err_json(AuthError::InvalidToken))?;
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
/// 前端以此刷新 can() 缓存(permissions_version 变化即授权有变更)。
async fn me(
    State(shared): State<SharedFactsLog>,
    headers: HeaderMap,
) -> ApiResult {
    let (snap, username, perms) = require_session(&shared, &headers)?;
    let user = snap.users.get(&username).ok_or_else(|| err_json(AuthError::InvalidToken))?;
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
async fn change_password(
    State(shared): State<SharedFactsLog>,
    headers: HeaderMap,
    Json(req): Json<ChangePasswordReq>,
) -> ApiResult {
    let (snap, username, perms) = require_session(&shared, &headers)?;
    if req.new_password.len() < 8 {
        return Err(err_json(AuthError::BadRequest(
            "新密码长度至少 8 位".into(),
        )));
    }
    let user = snap.users.get(&username).ok_or_else(|| err_json(AuthError::InvalidToken))?;
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
    append_auth_event(&shared, "change_password", serde_json::json!({ "username": username }));
    Ok(ok_json(
        StatusCode::OK,
        serde_json::json!({ "success": true, "permissions_version": perms.len() }),
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
        assert_eq!(perms, vec!["view_monitor".to_string(), "view_test_report".to_string()]);

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

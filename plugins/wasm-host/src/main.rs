// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `evorule-wasm-host` —— WASM UDF 宿主进程。
//!
//! # 定位（阶段 2 / ADR-0001）
//! 以 **external 插件**身份经 HTTP 接入 `evorule-server`：
//! server 侧 `call_service` → `service_registry` 查 URL → HTTP POST → 本进程执行 `.wasm`。
//!
//! **刻意进程外**（ADR-0001 决策 1）：`evorule-server` 二进制零膨胀、
//! 源码零改动，wasmtime 依赖树不进入 server 的 `Cargo.lock`。
//!
//! # 子命令（分发完整性 CLI；无参数 = 服务模式）
//! | 子命令 | 说明 |
//! |---|---|
//! | `keygen` | 生成 ed25519 密钥对（种子私钥私密保存，公钥入信任锚） |
//! | `sign` | 为 `.wasm` 生成 per-module 清单 + 分离签名（分发三件套） |
//! | `verify` | 离线验证目录内全部模块（发布者自检 / 边缘前置 / CI 门禁） |
//!
//! # 端点
//! | 方法 | 路径 | 说明 |
//! |---|---|---|
//! | GET | `/health` | 探活（跨仓契约：2xx + JSON，否则判 Offline 并告警） |
//! | GET | `/services` | 实载清单自报（历史批次自动发现：server 侧 `auto_discover` 装载时拉取） |
//! | POST | `/services/{name}` | 执行 UDF，body = 入参 JSON；200 成功 / 404 未知 UDF / 422 执行失败 |
//!
//! **`/health` 的两档语义（探活可信度补强，见 `declaration` 模块）**：
//! - 声明与实载一致 → **200** → server 判 `online`；
//! - 可判定的不一致（0 个模块 / 声明了但未加载 / 加载了但未声明）→ **503**
//!   → server 判 `offline` 并记 `platform.event.plugin_offline` 告警。
//!
//! 完整性结论随 `/health` body 的 `integrity` 字段外化（verified /
//! accepted_unverified / unchecked）——server 探活判定只看状态码，
//! 完整性不改变 200/503 语义（拒载在加载期已发生，表现为对账不一致）。
//!
//! **路径为何是 `/services/{name}`（契约 v1.2 硬性）**：server 装载外部插件包时按
//! `base_url + /services/{name}` 派生路由（`main.rs::load_external_plugins`），
//! 是本进程唯一会被调用的 URI 形态。此处不提供第二形态（如 `/udf/{name}`）——
//! 双入口必然漂移，且未被调用的那个永远是死代码。
//!
//! **入参形态**：server 的 `service_registry` 把规则侧 `params.args` 序列化为
//! HTTP body（`{"amount":1000,...}`）并置 `Content-Type: application/json`，
//! 故 guest 收到的即原始入参 JSON，host 不包装、不解析。
//!
//! **调用出口语义**：非 2xx 会被 server 侧 `http_handler` 转为 `Err`（消息含
//! status + 响应体截断），因此本进程的 404/422 在规则侧表现为显式失败而非静默。
//!
//! # UDF ABI（guest 必须导出）
//! - `memory`：线性内存
//! - `alloc(len: i32) -> i32`：分配输入空间
//! - `udf(ptr: i32, len: i32) -> i64`：执行；返回 `(结果指针 << 32) | 结果长度`
//!
//! **失败约定**：出参 JSON 顶层若含 `error` 字段，host 视为 UDF 业务失败（→ 422）；
//! 否则 → 200。理由见 `call_udf` 内注释。
//!
//! # 配置（环境变量）
//! - `WASM_HOST_ADDR` 监听地址，默认 `127.0.0.1:9140`；**非回环地址必须配置
//!   `WASM_HOST_AUTH_TOKEN`，否则拒绝启动**（服务面/管理面不对外裸奔）
//! - `WASM_HOST_DIR`  `.wasm` 目录，默认 `../wasm`（相对本包目录即 `plugins/wasm`）；
//!   **目录不存在即拒绝启动**（fail-fast，不再带 0 个 UDF 静默运行）
//! - `WASM_HOST_PLUGIN_JSON` 声明文件位置，缺省按
//!   `<WASM_HOST_DIR>/../wasm-host/plugin.json` 探测（不认 cwd，见 `declaration` 模块）
//! - `WASM_HOST_FUEL` fuel 预算，默认 `100_000_000`
//! - `WASM_HOST_MAX_MEM` 单模块内存上限字节，默认 `16777216`（16 MiB）
//! - `WASM_HOST_AUTH_TOKEN` 服务面（`/services/*`）Bearer token；配置后一律
//!   强制校验。未配置时仅允许回环监听（见 `WASM_HOST_ADDR` 行的拒启规则）
//! - `WASM_HOST_SIGN_POLICY` 验签策略 `warn|enforce|off`，默认 `enforce`
//!   （enforce=三件套强制+信任锚缺失拒启动；warn=材料缺失放行/存在则全量
//!   校验，仅作显式迁移降档；off=等价既有行为）
//! - `WASM_HOST_TRUSTED_KEYS` 信任锚文件，缺省按
//!   `<WASM_HOST_DIR>/../wasm-host/trusted-keys.json` 探测；enforce 策略下缺失即拒启动

mod declaration;
mod engine;
mod integrity;
mod registry;

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Multipart, Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use engine::UdfRuntime;
use integrity::{Integrity, SignPolicy, TrustedKeys, Verifier};
use registry::UdfRegistry;
use serde_json::{json, Value};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("keygen") => cli_keygen(),
        Some("sign") => cli_sign(&args[2..]),
        Some("verify") => cli_verify(&args[2..]),
        Some(other) => {
            eprintln!(
                "未知子命令 `{other}`。用法:\n  \
                 evorule-wasm-host               # 服务模式\n  \
                 evorule-wasm-host keygen        # 生成密钥对\n  \
                 evorule-wasm-host sign --wasm <path> --version <v> --key <seed_hex> --key-id <id> [--notes <t>] [--engine-min <v>]\n  \
                 evorule-wasm-host verify --dir <path> --keys <trusted.json> [--policy warn|enforce|off]"
            );
            std::process::exit(2);
        }
        None => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("tokio runtime 构建失败: {e}"))
            .and_then(|rt| rt.block_on(async { serve().await }))
            .unwrap_or_else(|e| {
                eprintln!("evorule-wasm-host: {e}");
                std::process::exit(1);
            }),
    }
}

// ---------------- 服务模式 ----------------

async fn serve() -> Result<(), String> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let addr = std::env::var("WASM_HOST_ADDR").unwrap_or_else(|_| "127.0.0.1:9140".to_string());
    // 服务面鉴权（fail-closed）：token 配置后 /services/* 一律 Bearer 校验；
    // 未配置 token 时仅允许回环监听——非回环地址 + 无 token = UDF 执行端点
    // 对外裸奔，拒绝启动（显式配置 token 或改回回环地址后才可对外服务）。
    let services_token = std::env::var("WASM_HOST_AUTH_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());
    {
        let bound: std::net::SocketAddr = addr
            .parse()
            .map_err(|e| format!("WASM_HOST_ADDR `{addr}` 不是合法的 addr:port: {e}"))?;
        if !bound.ip().is_loopback() && services_token.is_none() {
            return Err(format!(
                "拒绝启动: WASM_HOST_ADDR={addr} 为非回环地址但未配置 WASM_HOST_AUTH_TOKEN，\
                 /services/* 执行端点将对外裸奔；配置 token 后重启，或改用回环地址。"
            ));
        }
    }
    let dir = std::env::var("WASM_HOST_DIR").unwrap_or_else(|_| "../wasm".to_string());
    let fuel: u64 = std::env::var("WASM_HOST_FUEL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100_000_000);
    let max_mem: usize = std::env::var("WASM_HOST_MAX_MEM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(16 * 1024 * 1024);

    // 验签策略（非法值 fail-visible：拼错策略名不能等价于关掉防线）
    // 缺省 = enforce（fail-closed）：无信任锚/未签名模块拒载或拒启动；
    // 存量未签名部署须显式设 WASM_HOST_SIGN_POLICY=warn 降档（留日志可审计）。
    let policy = match std::env::var("WASM_HOST_SIGN_POLICY") {
        Ok(raw) => SignPolicy::parse(&raw)?,
        Err(_) => SignPolicy::Enforce,
    };
    let dir_path = std::path::Path::new(&dir).to_path_buf();
    let trusted_path = std::env::var("WASM_HOST_TRUSTED_KEYS").ok();
    let trusted =
        load_trusted_keys(&dir_path, trusted_path.as_deref(), policy)?.map(std::sync::Arc::new);

    tracing::info!(
        "分发完整性策略: {}（信任锚: {}）",
        policy.as_str(),
        if trusted.is_some() {
            "已装载"
        } else {
            "未配置"
        }
    );

    let runtime = match UdfRuntime::new(fuel, max_mem) {
        Ok(r) => Arc::new(r),
        Err(e) => return Err(format!("运行时初始化失败: {e}")),
    };

    let verifier = Verifier::new(policy, trusted.as_deref());
    let registry = match UdfRegistry::load_dir(
        Arc::clone(&runtime),
        &dir_path,
        std::env::var("WASM_HOST_PLUGIN_JSON").ok().as_deref(),
        &verifier,
        trusted.clone(),
        policy,
    ) {
        Ok(r) => Arc::new(r),
        Err(e) => return Err(format!("UDF 加载失败（fail-fast）: {e}")),
    };

    tracing::info!(
        "evorule-wasm-host: 已加载 {} 个 UDF: {:?}",
        registry.len(),
        registry.names()
    );

    // 声明对账：结论决定 /health 是 200 还是 503，故必须在启动期就喊清楚，
    // 不能等运维去 curl 才发现。
    {
        let rec = registry.reconciliation();
        match rec.state {
            crate::declaration::ReconciliationState::Ok => {
                if rec.auto_discover {
                    tracing::info!(
                        "声明对账一致（auto_discover: 目录=身份事实源, 策略表超集校验）: \
                         plugin.json({}) 策略覆盖 {} / 实载 {} 个服务\
                        （未覆盖 {} 个按默认策略接管）",
                        rec.source.as_deref().unwrap_or("(未知来源)"),
                        rec.declared.len(),
                        rec.loaded.len(),
                        rec.undeclared.len()
                    );
                } else {
                    tracing::info!(
                        "声明对账一致: plugin.json({}) × 实载 {} 个服务",
                        rec.source.as_deref().unwrap_or("(未知来源)"),
                        rec.loaded.len()
                    );
                }
            }
            crate::declaration::ReconciliationState::Degraded => tracing::error!(
                "声明对账不一致 → /health 将返回 503（server 侧会判 offline 并告警）: {}",
                rec.reason.as_deref().unwrap_or("(未给原因)")
            ),
            crate::declaration::ReconciliationState::Unavailable => tracing::warn!(
                "声明对账跳过（无法判定，不判 degraded）: {}",
                rec.reason.as_deref().unwrap_or("(未给原因)")
            ),
        }
    }

    // 管理面（安装自动化三期）：缺省关闭——WASM_HOST_ADMIN_TOKEN 未配置时
    // 三端点**整体不注册**（404 形态=端点不存在，不暴露能力面）
    let admin_token = std::env::var("WASM_HOST_ADMIN_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());
    let mut app = Router::new()
        .route("/health", get(health))
        .route("/services", get(list_services))
        .route("/services/{name}", post(call_udf));
    if admin_token.is_some() {
        tracing::info!("管理面已开启: /admin/install|uninstall|disable（Bearer token 鉴权）");
        app = app
            .route("/admin/install", post(admin_install))
            .route("/admin/uninstall/{name}", post(admin_uninstall))
            .route("/admin/disable/{name}", post(admin_disable));
    } else {
        tracing::info!("管理面关闭（WASM_HOST_ADMIN_TOKEN 未配置）——安装自动化通道不可用");
    }
    let app = app
        .layer(axum::middleware::from_fn_with_state(
            admin_token.map(|t| AdminToken(t.into())),
            admin_auth_middleware,
        ))
        .layer(axum::middleware::from_fn_with_state(
            services_token.map(|t| AdminToken(t.into())),
            services_auth_middleware,
        ))
        .with_state(registry);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("bind {addr} failed: {e}"))?;

    tracing::info!("evorule-wasm-host listening on {addr}");
    axum::serve(listener, app)
        .await
        .map_err(|e| format!("serve failed: {e}"))
}

/// 信任锚装载：显式路径 or `<WASM_HOST_DIR>/../wasm-host/trusted-keys.json` 缺省探测。
///
/// enforce 策略下信任锚必需（没有公钥的 enforce 无意义）→ 缺失/解析失败拒启动；
/// warn/off 下缺失允许（warn 对"带签名材料的模块"会因不可验而拒载，见 integrity 模块头）。
fn load_trusted_keys(
    wasm_dir: &std::path::Path,
    explicit: Option<&str>,
    policy: SignPolicy,
) -> Result<Option<TrustedKeys>, String> {
    let path = match explicit {
        Some(p) => std::path::PathBuf::from(p),
        None => wasm_dir
            .parent()
            .unwrap_or(wasm_dir)
            .join("wasm-host")
            .join("trusted-keys.json"),
    };
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if policy == SignPolicy::Enforce {
                return Err(format!(
                    "enforce 策略要求信任锚文件: {}（缺失即拒启动）",
                    path.display()
                ));
            }
            return Ok(None);
        }
        Err(e) => return Err(format!("读取信任锚 {} 失败: {e}", path.display())),
    };
    let keys: TrustedKeys = serde_json::from_slice(&bytes)
        .map_err(|e| format!("信任锚 {} 解析失败: {e}", path.display()))?;
    Ok(Some(keys))
}

/// 探活：`{base_url}/health` 必须 2xx + JSON（历史批次 plugin_probe 契约）。
///
/// **状态码即结论**：`degraded` → 503（server 判 offline 并告警）；
/// `ok` / `unavailable` → 200（后者在 body 里显式标注"未对账"）。
/// `unavailable` 不判 503 的理由见 `declaration` 模块头（拒绝假警）。
async fn health(State(reg): State<Arc<UdfRegistry>>) -> (StatusCode, Json<Value>) {
    let rec = reg.reconciliation();
    let code = if rec.is_degraded() {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };
    let status = if rec.is_degraded() { "degraded" } else { "ok" };
    let mut integrity_map: BTreeMap<String, Value> = BTreeMap::new();
    for name in reg.names() {
        integrity_map.insert(name.clone(), reg.integrity_of(&name).to_json());
    }
    (
        code,
        Json(json!({
            "status": status,
            "service": "evorule-wasm-host",
            "modules": reg.len(),
            "udfs": reg.names(),
            // 探活可信度：可服务性对账（声明 × 实载）。字段含义见 declaration 模块。
            "reconciliation": rec.to_json(),
            // 分发完整性（一期）：verified / accepted_unverified / unchecked
            "integrity": integrity_map,
            // 版本握手：安装前兼容性预检的宿主侧事实源。
            // host_abi 引用 integrity::HOST_ABI_VERSION 常量（单一事实源，禁字面量副本）
            "host_version": crate::integrity::host_version(),
            "host_abi": crate::integrity::HOST_ABI_VERSION,
        })),
    )
}

/// 实载清单自报（历史批次自动发现 · 路线 A：host 自报 + server 拉取合入）。
///
/// **身份与可服务性分端点回答**：本端点只答「实载了哪些服务」（身份，永远如实
/// 200），可服务性对账（声明 × 实载 → 503 语义）仍由 `/health` 承担——
/// 两问一答会互相污染语义：若这里随对账降级，server 侧「拉取失败」与
/// 「host 不健康」两种故障就会混为同一个信号，违反单一职责。
///
/// 服务名口径与 `/health` 的 `udfs`、`POST /services/{name}` 路由一致
/// （BTreeMap 确定性键序），server 侧以本清单为**身份单一事实源**合入注册表。
async fn list_services(State(reg): State<Arc<UdfRegistry>>) -> Json<Value> {
    Json(json!({
        "service": "evorule-wasm-host",
        "count": reg.len(),
        "services": reg.names(),
    }))
}

/// 执行 UDF。body 原样透传给 guest（host 不解析入参，保持 ABI 中立）。
async fn call_udf(
    State(reg): State<Arc<UdfRegistry>>,
    Path(name): Path<String>,
    body: Bytes,
) -> (StatusCode, Json<Value>) {
    let module = match reg.module(&name) {
        Some(m) => m,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "ok": false, "error": format!("unknown udf: {name}") })),
            )
        }
    };

    let runtime = reg.runtime();
    let input = body.to_vec();

    // CPU 密集的 wasmtime 执行移出 async worker（T5）
    let outcome = tokio::task::spawn_blocking(move || runtime.call(&module, &input)).await;

    let out = match outcome {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(e)) => {
            // UDF 自身失败（trap / fuel 耗尽 / 内存越界）→ 422，让规则侧显式感知
            tracing::warn!("UDF `{name}` 执行失败: {e}");
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "ok": false, "udf": name, "error": e })),
            );
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "ok": false, "udf": name, "error": format!("阻塞任务失败: {e}") })),
            )
        }
    };

    match serde_json::from_slice::<Value>(&out) {
        Ok(value) => {
            // ABI 约定：出参 JSON **顶层含 `error` 字段** = UDF 业务失败。
            // 理由：host 无法区分"UDF 成功返回了一个 JSON"与"UDF 自己报错并写成 JSON"，
            // 若不约定，业务错误会被当成 200 成功透传给规则侧，治理语义失真。
            if let Some(e) = value.get("error") {
                tracing::warn!("UDF `{name}` 业务失败: {e}");
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({ "ok": false, "udf": name, "error": e })),
                );
            }
            (StatusCode::OK, Json(json!({ "ok": true, "value": value })))
        }
        Err(e) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "ok": false,
                "udf": name,
                "error": format!("UDF 返回值不是合法 JSON: {e}"),
            })),
        ),
    }
}

// ---------------- CLI 子命令（分发完整性） ----------------

fn cli_keygen() {
    match integrity::generate_keys() {
        Ok((seed_hex, pk_hex)) => {
            // 机器友好形态（`标签 值`）：发布指南/脚本按 token 解析；
            // 种子=私密保存永不落仓，公钥=入 trusted-keys.json
            println!("seed_hex {seed_hex}");
            println!("public_key_hex {pk_hex}");
        }
        Err(e) => {
            eprintln!("keygen 失败: {e}");
            std::process::exit(1);
        }
    }
}

/// 极简 flag 解析（`--flag value`；本 CLI 参数面固定，不引依赖）
struct Flags {
    map: BTreeMap<String, String>,
}

impl Flags {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut map = BTreeMap::new();
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            if let Some(flag) = a.strip_prefix("--") {
                let value = args.get(i + 1).ok_or_else(|| format!("`{a}` 缺值"))?;
                map.insert(flag.to_string(), value.clone());
                i += 2;
            } else {
                return Err(format!(
                    "非预期参数 `{a}`（本 CLI 只接受 --flag value 形态）"
                ));
            }
        }
        Ok(Self { map })
    }

    fn get(&self, key: &str) -> Result<&str, String> {
        self.map
            .get(key)
            .map(String::as_str)
            .ok_or_else(|| format!("缺少 --{key}"))
    }

    fn opt(&self, key: &str) -> Option<&str> {
        self.map.get(key).map(String::as_str)
    }
}

fn cli_sign(args: &[String]) {
    let flags = match Flags::parse(args) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("参数错误: {e}");
            std::process::exit(2);
        }
    };
    let outcome = (|| -> Result<(), String> {
        let wasm_path = flags.get("wasm")?;
        let version = flags.get("version")?;
        let key_hex = flags.get("key")?;
        let key_id = flags.get("key-id")?;
        let seed_vec = integrity::hex_decode(key_hex)?;
        let seed: [u8; 32] = seed_vec
            .try_into()
            .map_err(|_| "--key 必须是 64 位 hex（32 字节种子）".to_string())?;

        let path = std::path::Path::new(wasm_path);
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| format!("无法从文件名推导模块名: {wasm_path}"))?;
        let wasm_bytes = std::fs::read(path).map_err(|e| format!("读取 {wasm_path} 失败: {e}"))?;

        // ① 清单（含 .wasm 内容哈希）
        let mut manifest = integrity::build_manifest(name, version, &wasm_bytes);
        manifest.release_notes = flags.opt("notes").map(str::to_string);
        manifest.engine_min = flags.opt("engine-min").map(str::to_string);
        let manifest_path = path.with_file_name(format!("{name}.plugin-module.json"));
        let manifest_json =
            serde_json::to_vec_pretty(&manifest).map_err(|e| format!("清单序列化失败: {e}"))?;
        std::fs::write(&manifest_path, &manifest_json)
            .map_err(|e| format!("写清单 {} 失败: {e}", manifest_path.display()))?;

        // ② 分离签名：ed25519 over 清单文件精确字节（sig → 清单 → blake3 → .wasm 链）
        let sig = integrity::sign(seed, &manifest_json);
        let sig_doc = integrity::DetachedSignature {
            module: name.to_string(),
            key_id: key_id.to_string(),
            signature: integrity::hex_encode(&sig),
        };
        let sig_path = path.with_file_name(format!("{name}.wasm.sig"));
        let sig_json =
            serde_json::to_vec_pretty(&sig_doc).map_err(|e| format!("签名序列化失败: {e}"))?;
        std::fs::write(&sig_path, &sig_json)
            .map_err(|e| format!("写签名 {} 失败: {e}", sig_path.display()))?;

        println!(
            "已签发 `{name}` v{version}:\n  清单 {} (blake3 {})\n  签名 {} (key_id={key_id})",
            manifest_path.display(),
            manifest.blake3,
            sig_path.display()
        );
        Ok(())
    })();
    if let Err(e) = outcome {
        eprintln!("sign 失败: {e}");
        std::process::exit(1);
    }
}

fn cli_verify(args: &[String]) {
    let flags = match Flags::parse(args) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("参数错误: {e}");
            std::process::exit(2);
        }
    };
    let outcome = (|| -> Result<bool, String> {
        let dir = flags.get("dir")?;
        let keys_path = flags.get("keys")?;
        let policy = match flags.opt("policy") {
            Some(raw) => SignPolicy::parse(raw)?,
            None => SignPolicy::Enforce, // verify 默认按最严口径离线自检
        };
        let keys_bytes =
            std::fs::read(keys_path).map_err(|e| format!("读取信任锚 {keys_path} 失败: {e}"))?;
        let trusted: TrustedKeys =
            serde_json::from_slice(&keys_bytes).map_err(|e| format!("信任锚解析失败: {e}"))?;

        let verifier = Verifier::new(policy, Some(&trusted));
        let mut all_pass = true;
        let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
            .map_err(|e| format!("读取目录 {dir} 失败: {e}"))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("wasm"))
            .collect();
        entries.sort();
        if entries.is_empty() {
            println!("目录无 .wasm 模块: {dir}");
            return Ok(true);
        }
        for path in entries {
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("(unknown)")
                .to_string();
            let wasm_bytes =
                std::fs::read(&path).map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;
            match verifier.verify_module(&name, &wasm_bytes, std::path::Path::new(dir)) {
                Ok(v) => {
                    println!("PASS {name} ({})", integrity_desc(&v));
                }
                Err(e) => {
                    println!("FAIL {name}: {e}");
                    all_pass = false;
                }
            }
        }
        Ok(all_pass)
    })();
    match outcome {
        Ok(true) => println!("verify: 全部通过"),
        Ok(false) => {
            println!("verify: 存在未通过模块");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("verify 失败: {e}");
            std::process::exit(1);
        }
    }
}

fn integrity_desc(v: &Integrity) -> &'static str {
    match v {
        Integrity::Verified { .. } => "签名已验证",
        Integrity::AcceptedUnverified { .. } => "未验证放行(warn)",
        Integrity::Unchecked => "未检查(off)",
    }
}

// ---------------- 管理面（安装自动化三期） ----------------

/// 管理 token 的 newtype（axum 中间件 state 载体）
#[derive(Clone)]
struct AdminToken(Arc<str>);

/// 管理 token 校验中间件：只拦 `/admin/*`；Bearer 不匹配 → 401（不区分
/// 「未配置」与「错误」的文案细节——token 未配置时端点本就不注册）。
async fn admin_auth_middleware(
    State(expected): State<Option<AdminToken>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if req.uri().path().starts_with("/admin") {
        let ok = expected.as_ref().is_some_and(|t| {
            req.headers()
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .is_some_and(|given| given == t.0.as_ref())
        });
        if !ok {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "ok": false, "error": "admin token 缺失或不匹配" })),
            )
                .into_response();
        }
    }
    next.run(req).await
}

/// 服务面 token 校验中间件：`WASM_HOST_AUTH_TOKEN` 已配置时拦 `/services/*`
/// （含 `/services` 清单与 `/services/{name}` 执行端点）；未配置时启动期已
/// 保证仅回环监听（见 `serve` 的拒绝启动检查）——两层合起来执行端点不裸奔。
async fn services_auth_middleware(
    State(expected): State<Option<AdminToken>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if let Some(expected) = expected.as_ref() {
        if req.uri().path().starts_with("/services") {
            let ok = req
                .headers()
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .is_some_and(|given| given == expected.0.as_ref());
            if !ok {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({ "ok": false, "error": "service token 缺失或不匹配" })),
                )
                    .into_response();
            }
        }
    }
    next.run(req).await
}

/// `POST /admin/install`：multipart 三件套（wasm / manifest / signature 可缺省）
/// + 可选字段 allow_downgrade。全程阶段计时回显（透明性）。
async fn admin_install(
    State(reg): State<Arc<UdfRegistry>>,
    mut multipart: Multipart,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let mut wasm: Option<Vec<u8>> = None;
    let mut manifest: Option<Vec<u8>> = None;
    let mut signature: Option<Vec<u8>> = None;
    let mut allow_downgrade = false;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| err_json(StatusCode::BAD_REQUEST, format!("multipart 解析失败: {e}")))?
    {
        match field.name().unwrap_or("") {
            "wasm" => wasm = Some(field.bytes().await.map_err(bad_req)?.to_vec()),
            "manifest" => manifest = Some(field.bytes().await.map_err(bad_req)?.to_vec()),
            "signature" => signature = Some(field.bytes().await.map_err(bad_req)?.to_vec()),
            "allow_downgrade" => {
                allow_downgrade = field.text().await.unwrap_or_default().trim() == "true";
            }
            _ => {}
        }
    }
    let manifest_bytes = manifest.ok_or_else(|| {
        err_json(
            StatusCode::BAD_REQUEST,
            "缺少 manifest 字段（分发清单 JSON）",
        )
    })?;
    let module_name = serde_json::from_slice::<Value>(&manifest_bytes)
        .ok()
        .and_then(|m| m.get("module").and_then(|v| v.as_str()).map(str::to_string))
        .ok_or_else(|| {
            err_json(
                StatusCode::BAD_REQUEST,
                "清单缺少 module 字段（无法推导模块名）",
            )
        })?;
    let wasm_bytes =
        wasm.ok_or_else(|| err_json(StatusCode::BAD_REQUEST, "缺少 wasm 字段（模块二进制）"))?;

    // 编译 CPU 密集 → spawn_blocking（与 call_udf 同款纪律）
    let name_for_resp = module_name.clone();
    let reg2 = std::sync::Arc::clone(&reg);
    let outcome = tokio::task::spawn_blocking(move || {
        reg2.install(
            &module_name,
            wasm_bytes,
            manifest_bytes,
            signature,
            allow_downgrade,
        )
    })
    .await
    .map_err(|e| {
        err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("阻塞任务失败: {e}"),
        )
    })?;

    match outcome {
        Ok(stages) => {
            let snap = reg.snapshot();
            Ok(Json(json!({
                "ok": true,
                "installed": name_for_resp,
                "stages": stages.to_json(),
                "modules": snap.modules.len(),
                "integrity": snap.integrity.get(&name_for_resp).map(|i| i.to_json()),
                // 静态制部署提示：新服务需 plugin.json 补声明或开启 auto_discover 才可被 server 路由
                "reconciliation": snap.reconciliation.to_json(),
            })))
        }
        Err(e) => Err(err_json(StatusCode::UNPROCESSABLE_ENTITY, e)),
    }
}

/// `POST /admin/uninstall/{name}`：快照移除+三件套物理删除（不可逆）
async fn admin_uninstall(
    State(reg): State<Arc<UdfRegistry>>,
    Path(name): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let name_for_resp = name.clone();
    let reg2 = std::sync::Arc::clone(&reg);
    let outcome = tokio::task::spawn_blocking(move || reg2.uninstall(&name))
        .await
        .map_err(|e| {
            err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("阻塞任务失败: {e}"),
            )
        })?;
    match outcome {
        Ok(stages) => Ok(Json(json!({
            "ok": true, "uninstalled": name_for_resp, "stages": stages.to_json(),
            "modules": reg.len(),
        }))),
        Err(e) => Err(err_json(StatusCode::UNPROCESSABLE_ENTITY, e)),
    }
}

/// `POST /admin/disable/{name}`：仅运行时下线（文件保留，重启恢复在役）
async fn admin_disable(
    State(reg): State<Arc<UdfRegistry>>,
    Path(name): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let name_for_resp = name.clone();
    let reg2 = std::sync::Arc::clone(&reg);
    let outcome = tokio::task::spawn_blocking(move || reg2.disable(&name))
        .await
        .map_err(|e| {
            err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("阻塞任务失败: {e}"),
            )
        })?;
    match outcome {
        Ok(stages) => Ok(Json(json!({
            "ok": true, "disabled": name_for_resp, "stages": stages.to_json(),
            "modules": reg.len(),
            "note": "运行时下线（文件保留，重启后恢复在役）",
        }))),
        Err(e) => Err(err_json(StatusCode::UNPROCESSABLE_ENTITY, e)),
    }
}

fn bad_req(e: axum::extract::multipart::MultipartError) -> (StatusCode, Json<Value>) {
    err_json(StatusCode::BAD_REQUEST, format!("字段读取失败: {e}"))
}

fn err_json(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<Value>) {
    (
        status,
        Json(json!({ "ok": false, "error": message.into() })),
    )
}

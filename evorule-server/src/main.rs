// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! evorule-server —— 独立二进制服务入口(应用层)
//!
//! H5: 从核心层迁出到独立仓,
//! 因为核心层不应依赖具体 I/O handler 实现(策略)。
//!
//! 启动 GovernanceServer（HTTP API + SSE 事件流 + 多会话管理），
//! 内置 IoSubscriber（DB / HTTP / Memory 三种 I/O handler,来自 evorule-io-handlers crate）。
//!
//! # 用法
//! ```bash
//! evorule-server --addr 0.0.0.0:18080 --auth-token secret123
//! evorule-server --config evorule.json --log-format json
//! ```
//!
//! # 配置加载优先级
//! CLI 参数 > 环境变量（前缀 `EVORULE_`）> JSON 配置文件 > 内置默认值
//!
//! # 优雅退出
//! - 监听 SIGTERM（Docker 停止信号）和 SIGINT（Ctrl+C）
//! - 收到信号后：readiness 设为 false（负载均衡器切走流量）→ 等待进行中请求 → 30s 超时强制退出
//! - `GET /api/health/liveness` 始终 200；`GET /api/health/readiness` 在退出期间返回 503

#![forbid(unsafe_code)]
// C5 (unwrap/expect/panic = deny) 仅约束生产代码;测试代码保留 unwrap 惯例
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use evorule_reactor::{FactsLog, IoType, Reactor};
#[cfg(test)]
use evorule_tcb::JsonValue;
use std::time::Instant;
// H6: AuthConfig 和 API server 从 lib 导入（应用层模块已迁至 src/lib.rs）
use evorule_governance::auditor::Auditor;
use evorule_server::api::server::{AppState, GovernanceApi, GovernanceServer, SessionApi};
use evorule_server::auth::AuthConfig;
use evorule_server::input_sanitizer::InputSanitizer;
// H5: IoDispatcher/IoSubscriber 来自 evorule-governance(机制层)
use evorule_governance::{IoDispatcher, IoSubscriber};
// H5: 具体 handler 实现来自 evorule-io-handlers(应用层,从 evorule-governance 迁出)
use evorule_io_handlers::{
    DbHandler, HttpHandler, MemoryHandler, ServiceRegistry, ServiceRegistryHandler,
    StatementWhitelist, WhitelistedDbHandler,
};
// Phase 1: yuanze-demos 业务服务 Rust 原生实现（复合路由：原生优先，HTTP 回落）
use evorule_demo_services::NATIVE_SERVICES as DEMO_NATIVE_SERVICES;
use evorule_finance_config::NATIVE_SERVICES as FINANCE_NATIVE_SERVICES;
use evorule_indicator_services::NATIVE_SERVICES as INDICATOR_NATIVE_SERVICES;
use evorule_physics_services::NATIVE_SERVICES as PHYSICS_NATIVE_SERVICES;
// H6: SharedMetrics trait object 类型来自核心层，PrometheusMetrics 实现来自本地 metrics_impl
use evorule_governance::metrics::SharedMetrics;
use evorule_governance::shared_facts_log::SharedFactsLog;
use tracing::{error, info, warn};
use tracing_appender::rolling::{RollingFileAppender, Rotation};

/// 优雅退出超时（等待进行中请求的最长时间）
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

// ===== JSON 配置文件结构 =====

/// JSON 配置文件顶层结构
///
/// 示例文件 (`evorule.json`)：
/// ```json
/// {
/// "server": {
/// "addr": "0.0.0.0:18080",
/// "max_rounds": 1000
/// },
/// "auth": {
/// "token": "secret123"
/// },
/// "paths": {
/// "core_eval": "./evorule-tcb/core_eval.json",
/// "rules_dir": "./rules",
/// "db_path": "./data/evorule.db",
/// "memory_dir": "./data/memory"
/// },
/// "log": {
/// "level": "info",
/// "format": "json",
/// "file": "./logs/evorule.log"
/// }
/// }
/// ```
///
/// **为什么用 JSON?**
/// EvoRule 的核心理念是"只接受和运行 JSON 数据集"。
/// 配置文件虽然不是业务规则,但也应该是 JSON,以保持原则一致性。
#[derive(Debug, Default, serde::Deserialize)]
struct FileConfig {
    #[serde(default)]
    server: FileServerConfig,
    #[serde(default)]
    auth: FileAuthConfig,
    #[serde(default)]
    paths: FilePathsConfig,
    #[serde(default)]
    log: FileLogConfig,
}

#[derive(Debug, Default, serde::Deserialize)]
struct FileServerConfig {
    addr: Option<String>,
    max_rounds: Option<usize>,
    /// :演示登录入口开关（缺省 true；生产部署建议 false）
    demo_auth: Option<bool>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct FileAuthConfig {
    token: Option<String>,
    /// B5-server：受信服务管道 token（service 身份，可写受保护域）
    #[serde(default)]
    service_token: Option<String>,
    /// CORS 允许的 Origin 列表（未配置/空 = 放行本机 loopback Origin 任意端口）
    /// 例：["https://app.example.com", "http://localhost:5173"]
    allowed_origins: Option<Vec<String>>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct FilePathsConfig {
    core_eval: Option<PathBuf>,
    rules_dir: Option<PathBuf>,
    db_path: Option<PathBuf>,
    memory_dir: Option<PathBuf>,
    /// WAL 文件存储目录（可选，指定后启用 WAL 持久化）
    wal_dir: Option<PathBuf>,
    /// call_service/call_external 的 service_name → URL 映射文件（可选）
    service_registry: Option<PathBuf>,
    /// SQL 语句模板白名单文件（可选，未设置则禁用 QUERY_DB）
    statement_whitelist: Option<PathBuf>,
    /// 插件清单文件
    plugins: Option<PathBuf>,
    /// Workspace 元数据库路径 (P10, 可选, 默认 ./data/workspace.db)
    workspace_db: Option<PathBuf>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct FileLogConfig {
    level: Option<String>,
    /// `plain` 或 `json`
    format: Option<String>,
    /// 日志文件路径（生产环境持久化，可选）
    file: Option<PathBuf>,
    /// 日志文件保留天数（默认 7 天）
    max_days: Option<u32>,
    /// 日志目录最大占用空间（MB，默认 1024MB）
    max_size_mb: Option<u64>,
}

/// 加载 JSON 配置文件
///
/// 文件不存在时返回空配置（不报错，允许纯 CLI 启动）。
// 配置加载多分支 (YAML/JSON/env), 拆函数需共享路径状态。详见 GATE_REFERENCE.md §六(豁免索引)
#[allow(clippy::cognitive_complexity)]
fn load_config_file(path: &Option<PathBuf>) -> FileConfig {
    match path {
        Some(p) if p.exists() => {
            let content = match std::fs::read_to_string(p) {
                Ok(s) => s,
                Err(e) => {
                    warn!(
                        "读取配置文件失败 {}: {}，将仅使用 CLI/环境变量",
                        p.display(),
                        e
                    );
                    return FileConfig::default();
                }
            };
            match serde_json::from_str::<FileConfig>(&content) {
                Ok(cfg) => {
                    info!("已加载配置文件: {}", p.display());
                    cfg
                }
                Err(e) => {
                    warn!(
                        "解析配置文件失败 {}: {}，将仅使用 CLI/环境变量",
                        p.display(),
                        e
                    );
                    FileConfig::default()
                }
            }
        }
        Some(p) => {
            warn!("配置文件不存在: {}，将仅使用 CLI/环境变量", p.display());
            FileConfig::default()
        }
        None => FileConfig::default(),
    }
}

/// evorule-server 启动配置
///
/// 所有字段均可通过 CLI 参数、环境变量（前缀 `EVORULE_`）或 JSON 配置文件提供。
/// 优先级：CLI > 环境变量 > 配置文件 > 内置默认值。
#[derive(Parser, Debug)]
#[command(name = "evorule-server", version, about = "EvoRule 治理层 HTTP 服务")]
struct Cli {
    /// JSON 配置文件路径（可选，例: ./evorule.json）
    #[arg(long, env = "EVORULE_CONFIG")]
    config: Option<PathBuf>,

    /// 监听地址
    #[arg(long, env = "EVORULE_ADDR")]
    addr: Option<String>,

    /// Bearer 认证 token（未提供时禁用认证，仅用于开发）
    ///
    /// L2 安全提示: CLI 参数在进程列表(`ps aux`)中可见,其他用户可读到。
    /// 生产环境优先使用 `EVORULE_AUTH_TOKEN` 环境变量或 JSON 配置文件。
    #[arg(long, env = "EVORULE_AUTH_TOKEN")]
    auth_token: Option<String>,

    /// B5-server：受信服务管道 token（service 身份，可写受保护域）
    ///
    /// 与 `EVORULE_AUTH_TOKEN` 独立的 env/CLI（凭据分层）：user token 禁止写
    /// `shared.*.stable.llm.*` / `stable.system.*` 受保护域，service token 可写。
    /// 仅在认证启用（设置了 auth_token）时生效；认证禁用（显式豁免模式）
    /// 时服务 token 被忽略且不注入身份。
    #[arg(long, env = "EVORULE_SERVICE_TOKEN")]
    service_token: Option<String>,

    /// 显式豁免认证（仅限 loopback 绑定）：绑定回环地址且未设置 auth_token 时，
    /// 必须显式声明本参数才允许无认证启动（显式豁免安全策略，UV-116）。
    ///
    /// 旧实现（0.4.x）：loopback + 无 token 隐式进入无认证模式（仅 info 日志），
    /// 漏配时全部受保护端点静默匿名可达（市场写接口实测匿名可写坐实风险）。
    /// 修复后：未声明即拒绝启动（fail-fast）+ 三选一自诊断指引。
    /// 非 loopback 绑定不受本参数影响（fail-closed 硬拒不放松）。
    #[arg(long, env = "EVORULE_INSECURE_SERVE")]
    insecure_serve: bool,

    /// 宪法文件路径（server_eval.json，不可热重载；更名）
    #[arg(long, env = "EVORULE_CORE_EVAL")]
    core_eval: Option<PathBuf>,

    /// 业务规则目录（热重载监听目录）
    #[arg(long, env = "EVORULE_RULES_DIR")]
    rules_dir: Option<PathBuf>,

    /// SQLite 数据库文件路径
    #[arg(long, env = "EVORULE_DB_PATH")]
    db_path: Option<PathBuf>,

    /// Workspace 元数据库路径 (P10: 工作空间 + 规则元数据, 独立于业务 db_path)
    #[arg(long, env = "EVORULE_WORKSPACE_DB")]
    workspace_db: Option<PathBuf>,

    /// Memory handler 存储根目录
    #[arg(long, env = "EVORULE_MEMORY_DIR")]
    memory_dir: Option<PathBuf>,

    /// 反应器最大指令执行步数
    #[arg(long, env = "EVORULE_MAX_ROUNDS")]
    max_rounds: Option<usize>,

    /// 日志级别（error/warn/info/debug/trace）
    #[arg(long, env = "EVORULE_LOG_LEVEL")]
    log_level: Option<String>,

    /// 日志格式（`plain` 或 `json`，默认 `plain`）
    #[arg(long, env = "EVORULE_LOG_FORMAT")]
    log_format: Option<String>,

    /// 日志文件路径（生产环境持久化，可选）
    #[arg(long, env = "EVORULE_LOG_FILE")]
    log_file: Option<PathBuf>,

    /// 日志文件保留天数（默认 7 天）
    #[arg(long, env = "EVORULE_LOG_MAX_DAYS")]
    log_max_days: Option<u32>,

    /// 日志目录最大占用空间（MB，默认 1024MB）
    #[arg(long, env = "EVORULE_LOG_MAX_SIZE_MB")]
    log_max_size_mb: Option<u64>,

    /// WAL 文件存储目录（可选，指定后启用 WAL 持久化）
    #[arg(long, env = "EVORULE_WAL_DIR")]
    wal_dir: Option<PathBuf>,

    /// WAL fsync 开关（P02：启用后在每次 WAL 写入后执行 fsync，确保断电时数据不丢失）
    #[arg(long, env = "EVORULE_WAL_FSYNC")]
    wal_fsync: bool,

    /// WAL 文件最大大小（MB，P03：达到此大小后自动轮换文件，默认 100MB，0 表示不轮换）
    #[arg(long, env = "EVORULE_WAL_MAX_SIZE_MB")]
    wal_max_size_mb: Option<u64>,

    /// 启用审计链实时验证（P06：每次 audit_new 后自动验证审计链完整性）
    #[arg(long, env = "EVORULE_AUTO_VERIFY")]
    auto_verify: bool,

    /// 自动验证阈值（P06：审计条目数超过此值时跳过验证，0 表示不限制，默认 1000）
    #[arg(long, env = "EVORULE_AUTO_VERIFY_THRESHOLD")]
    auto_verify_threshold: Option<usize>,

    /// 自动验证间隔（P06：每 N 次 audit_new 验证一次，默认 1）
    #[arg(long, env = "EVORULE_AUTO_VERIFY_INTERVAL")]
    auto_verify_interval: Option<usize>,

    /// 禁用速率限制(仅用于 benchmark/性能测试)
    #[arg(long, env = "EVORULE_NO_RATE_LIMIT")]
    no_rate_limit: bool,

    /// call_service/call_external 的 service_name→URL 映射文件（可选，例 ./service_registry.json）
    #[arg(long, env = "EVORULE_SERVICE_REGISTRY")]
    service_registry: Option<PathBuf>,

    /// SQL 语句模板白名单文件（可选，未设置则 QUERY_DB 调用全部返回错误，防止任意 SQL）
    #[arg(long, env = "EVORULE_STATEMENT_WHITELIST")]
    statement_whitelist: Option<PathBuf>,

    /// 插件清单文件
    ///
    /// 例 ./plugin_manifest.json：
    /// { "plugins": { "demo-services": { "enabled": true, "services": ["config_persist"] } } }
    /// services 省略 = 该插件全部服务；enabled=false = 不挂载该插件（call_service 走 HTTP 注册表）。
    /// 清单中未知名/重复名/空启用集 → 启动 fail-fast（错误含指引）。
    #[arg(long, env = "EVORULE_PLUGINS")]
    plugins: Option<PathBuf>,

    /// CORS 允许的 Origin 列表（逗号分隔；空 = 放行本机 loopback Origin
    /// (localhost/127.0.0.1/[::1] 任意端口,开发友好);* 代表放行全部）
    ///
    /// 生产部署(监听 0.0.0.0)必须显式配置精确白名单。
    /// 例：--allowed-origins "https://app.example.com,http://localhost:5173"
    #[arg(long, env = "EVORULE_ALLOWED_ORIGINS", value_delimiter = ',')]
    allowed_origins: Vec<String>,

    /// 允许 HTTP handler 访问 loopback 地址（仅本地开发，调用同机 127.0.0.1 服务时使用）
    ///
    /// 生产环境永远不要启用——SSRF 防护会因此放行 127.0.0.0/8 和私有 IP 段。
    #[arg(long, env = "EVORULE_ALLOW_LOOPBACK")]
    allow_loopback: bool,

    /// 启用 /metrics 端点认证（S2：默认关闭，Prometheus scraper 通常不带 token）
    ///
    /// 启用后 /metrics 端点也需要 Authorization: Bearer <token> 头。
    #[arg(long, env = "EVORULE_METRICS_AUTH")]
    metrics_auth: bool,

    /// 挂载 OpenAPI Swagger UI（默认关闭，避免生产暴露接口面）
    ///
    /// 启用后 `GET /api/docs` 提供交互式 API 文档。
    /// `/api/openapi.json`（单一真相源）始终可用，与此开关无关。
    #[arg(long, env = "EVORULE_OPENAPI_UI")]
    openapi_ui: bool,

    /// 启用强制中止会话端点（POST /api/sessions/{id}/abort，默认关闭）
    ///
    /// abort 是破坏性操作。双保险：即使认证通过，未显式开启时也不注册该
    /// 路由（返回 404），防止误触发/滥用。
    #[arg(long, env = "EVORULE_ALLOW_ABORT")]
    allow_abort: bool,

    /// 静态前端目录（可选，例 ./web）
    ///
    /// 设置后由本服务同源托管 Web UI（如 console-cloud 的 adapter-static 产物）：
    /// 未命中 /api 路由的 GET 请求走静态文件，未知路径回退 index.html（SPA）。
    /// 目录下必须存在 index.html，否则拒绝启动（fail-fast）。
    #[arg(long, env = "EVORULE_WEB_DIR")]
    web_dir: Option<PathBuf>,

    /// 演示登录入口开关：经 /api/platform/auth/status 公开下发，
    /// 登录页据此隐藏「演示模式（预置角色一键登录）」入口。
    /// 体验包默认开；生产部署建议 `--demo-auth false`（或 env EVORULE_DEMO_AUTH=false / 配置文件 server.demo_auth）。
    /// 支持 `--demo-auth`（=true）与 `--demo-auth false` 两种写法。
    #[arg(
        long,
        env = "EVORULE_DEMO_AUTH",
        default_missing_value = "true",
        num_args = 0..=1,
        value_parser = clap::value_parser!(bool),
    )]
    demo_auth: Option<bool>,

    /// 服务端 PDF 导出的中文字体显式指定：TTF/OTF/TTC 路径。
    /// 缺省时自动探测系统字体（Windows: msyh/simhei/simsun；Linux: Noto Sans CJK/
    /// 文泉驿）；探测不到时 PDF 导出显式报错（fail-fast，不生成缺字 PDF）。
    #[arg(long, env = "EVORULE_PDF_FONT")]
    pdf_font: Option<PathBuf>,
}

/// 合并后的最终配置（CLI > env > file > default）
struct ResolvedConfig {
    addr: String,
    auth_token: Option<String>,
    /// B5-server：受信服务管道 token（仅认证启用时生效）
    service_token: Option<String>,
    core_eval: PathBuf,
    rules_dir: PathBuf,
    db_path: PathBuf,
    memory_dir: PathBuf,
    max_rounds: usize,
    log_level: String,
    log_format: String,
    log_file: Option<PathBuf>,
    log_max_days: u32,
    log_max_size_mb: u64,
    /// WAL 文件存储目录（可选，指定后启用 WAL 持久化）
    wal_dir: Option<PathBuf>,
    /// WAL fsync 开关（P02：启用后在每次 WAL 写入后执行 fsync）
    wal_fsync: bool,
    /// WAL 文件最大大小（字节，P03：达到此大小后自动轮换文件）
    max_wal_size_bytes: u64,
    /// 是否启用审计链实时验证（P06）
    auto_verify: bool,
    /// 自动验证阈值（P06，0 表示不限制）
    auto_verify_threshold: usize,
    /// 自动验证间隔（P06，1 表示每次都验证）
    auto_verify_interval: usize,
    /// 是否禁用速率限制（仅 benchmark 使用）
    rate_limit_per_sec: u64,
    /// service_name→URL 映射文件（ServiceRegistry）
    service_registry: Option<PathBuf>,
    /// SQL 模板白名单文件（未设置则 QUERY_DB 全部拒绝）
    statement_whitelist: Option<PathBuf>,
    /// 插件清单文件
    plugins: Option<PathBuf>,
    /// CORS 白名单；若 CLI 指定了 "*" 则为全放行模式（仅限开发）
    allowed_origins: Vec<String>,
    /// 是否允许 HTTP handler 访问 loopback（仅本地开发）
    allow_loopback: bool,
    /// S2：/metrics 端点是否需要认证
    metrics_auth: bool,
    /// 是否挂载 OpenAPI Swagger UI（--openapi-ui）
    openapi_ui: bool,
    /// 是否启用强制中止端点（--allow-abort，默认 false）
    allow_abort: bool,
    /// 静态前端目录（--web-dir）；None = 不托管静态文件
    web_dir: Option<PathBuf>,
    /// :演示登录入口开关（默认 true；CLI > env > file > default）
    demo_auth: bool,
    /// 显式豁免认证（UV-116）：loopback+无 token 时须显式声明才允许无认证启动
    insecure_serve: bool,
    /// Workspace 元数据库路径 (P10, 默认 ./data/workspace.db)
    workspace_db: PathBuf,
}

impl ResolvedConfig {
    /// 按 CLI > env > file > default 优先级解析配置
    #[allow(clippy::let_and_return)]
    fn resolve(cli: Cli, file: FileConfig) -> Self {
        let max_wal_size_mb = cli.wal_max_size_mb.unwrap_or(100);
        let allowed_origins = if cli.allowed_origins.is_empty() {
            file.auth.allowed_origins.unwrap_or_default()
        } else {
            cli.allowed_origins
        };
        let cfg = Self {
            addr: cli
                .addr
                .or(file.server.addr)
                .unwrap_or_else(|| "0.0.0.0:18080".to_string()),
            auth_token: cli.auth_token.or(file.auth.token),
            insecure_serve: cli.insecure_serve,
            service_token: cli.service_token.or(file.auth.service_token),
            core_eval: cli
                .core_eval
                .or(file.paths.core_eval)
                // 默认指向本仓 resources/(:v0.4.1 起 server 份宪法业务规则集
                // 更名为 server_eval.json,与 evorule 仓宪法原则 core_eval.json 区分)
                .unwrap_or_else(|| PathBuf::from("./resources/server_eval.json")),
            rules_dir: cli
                .rules_dir
                .or(file.paths.rules_dir)
                .unwrap_or_else(|| PathBuf::from("./rules")),
            db_path: cli
                .db_path
                .or(file.paths.db_path)
                .unwrap_or_else(|| PathBuf::from("./data/evorule.db")),
            memory_dir: cli
                .memory_dir
                .or(file.paths.memory_dir)
                .unwrap_or_else(|| PathBuf::from("./data/memory")),
            max_rounds: cli.max_rounds.or(file.server.max_rounds).unwrap_or(1000),
            log_level: cli
                .log_level
                .or(file.log.level)
                .unwrap_or_else(|| "info".to_string()),
            log_format: cli
                .log_format
                .or(file.log.format)
                .unwrap_or_else(|| "plain".to_string()),
            log_file: cli.log_file.or(file.log.file),
            log_max_days: cli.log_max_days.or(file.log.max_days).unwrap_or(7),
            log_max_size_mb: cli.log_max_size_mb.or(file.log.max_size_mb).unwrap_or(1024),
            wal_dir: cli.wal_dir.or(file.paths.wal_dir),
            wal_fsync: cli.wal_fsync,
            max_wal_size_bytes: max_wal_size_mb * 1024 * 1024,
            auto_verify: cli.auto_verify,
            auto_verify_threshold: cli.auto_verify_threshold.unwrap_or(1000),
            auto_verify_interval: cli.auto_verify_interval.unwrap_or(1),
            // 速率限制：默认持续速率 200 req/s（burst=200;period 换算见
            // resolve_governor_config 注释）。
            // 实测修正(2026-09-01):此前误传 per_sec=1,经 resolve_governor_config
            // 换算实为每秒回补 1 个令牌,合法多用户流量被持续 429。
            // --no-rate-limit 设为 0 → build_router 完全跳过 GovernorLayer（真正禁用限速）
            rate_limit_per_sec: if cli.no_rate_limit { 0 } else { 200 },
            service_registry: cli.service_registry.or(file.paths.service_registry),
            statement_whitelist: cli.statement_whitelist.or(file.paths.statement_whitelist),
            plugins: cli.plugins.or(file.paths.plugins),
            allowed_origins,
            allow_loopback: cli.allow_loopback,
            // S2：从 CLI/环境变量读取 metrics_auth 配置
            metrics_auth: cli.metrics_auth,
            // OpenAPI Swagger UI 开关（默认 false）
            openapi_ui: cli.openapi_ui,
            // abort 破坏性端点开关（默认 false，双保险）
            allow_abort: cli.allow_abort,
            // 静态前端目录（默认 None，不托管静态文件）
            web_dir: cli.web_dir,
            // :演示登录入口开关（CLI > env > file > 默认 true）
            demo_auth: cli.demo_auth.or(file.server.demo_auth).unwrap_or(true),
            // P10: workspace 元数据库路径 (独立于业务 db_path)
            workspace_db: cli
                .workspace_db
                .or(file.paths.workspace_db)
                .unwrap_or_else(|| PathBuf::from("./data/workspace.db")),
        };
        cfg
    }
}

// ===== 插件清单 =====

/// 单个插件的挂载决定(部署期事实,启动后不可变)
#[derive(Debug, Clone, PartialEq)]
enum PluginMount {
    /// 未提及/未配置清单 → 全部启用(存量零迁移)
    All,
    /// 启用子集(清单已校验:未知名/重复名在 make_router_enabled 内 fail-fast)
    Subset(Vec<String>),
    /// 明确停用 → 不挂载本路由,call_service/call_external 直连 HTTP 注册表
    Off,
}

#[derive(serde::Deserialize)]
struct PluginManifestEntry {
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    services: Option<Vec<String>>,
}

#[derive(serde::Deserialize)]
struct PluginManifestFile {
    #[serde(default)]
    plugins: std::collections::BTreeMap<String, PluginManifestEntry>,
}

fn default_true() -> bool {
    true
}

/// 进程内插件登记项(泛化;插件抽象上提后路由机制件归一至 plugin-kit,
/// 登记表直引各插件声明表):新增进程内插件 = 在 [`PLUGIN_DEFS`] 追加一项
/// (id + 声明表指针),清单解析/挂载链/健康节机制代码零改动。
struct PluginDef {
    /// 清单与 /api/health 中的插件 id
    id: &'static str,
    /// 该插件声明表(声明序;服务名清单/全启构造/子集三拒绝构造均自表派生)
    defs: &'static [evorule_plugin_kit::NativeServiceDef],
}

/// 该插件全部合法服务名(声明序;清单空集报错与健康节呈现用)。
fn plugin_service_names(def: &PluginDef) -> Vec<&'static str> {
    evorule_plugin_kit::NativeServiceRouter::native_service_names(def.defs)
}

/// 该插件进程内服务能力清单(声明序;/api/services native 对账与 invoke 敏感判定用)。
fn plugin_service_infos(
    def: &PluginDef,
) -> Vec<evorule_server::api::server::BoundServiceInfo> {
    def.defs
        .iter()
        .map(|s| evorule_server::api::server::BoundServiceInfo {
            name: s.name.to_string(),
            source: "native".to_string(),
            version: Some("1.0.0".to_string()),
            description: Some(s.description.to_string()),
            plugin: Some(def.id.to_string()),
            sensitive: s.sensitive,
        })
        .collect()
}

/// 进程内插件登记表(声明序即挂载序与回落链序)。
const PLUGIN_DEFS: &[PluginDef] = &[
    PluginDef {
        id: "demo-services",
        defs: DEMO_NATIVE_SERVICES,
    },
    PluginDef {
        id: "physics-services",
        defs: PHYSICS_NATIVE_SERVICES,
    },
    PluginDef {
        id: "indicator-services",
        defs: INDICATOR_NATIVE_SERVICES,
    },
    PluginDef {
        id: "finance-config",
        defs: FINANCE_NATIVE_SERVICES,
    },
];

/// 加载并解析插件清单;未配置 → 全部插件全启(缺省)。
///
/// fail-fast 原则(自愈):文件不可读 / JSON 非法 / 未知插件 id / 空 services
/// 均显式报错并附自诊断指引,不静默忽略任何清单条目。
/// 清单未提及的插件 = All(缺省全启,存量零迁移)。
fn load_plugin_mounts(path: Option<&PathBuf>) -> Result<Vec<(&'static str, PluginMount)>, String> {
    let Some(p) = path else {
        return Ok(PLUGIN_DEFS
            .iter()
            .map(|d| (d.id, PluginMount::All))
            .collect());
    };
    let content = std::fs::read_to_string(p).map_err(|e| {
        format!(
            "读取插件清单失败 {}: {}（自诊断指引: ① 确认 --plugins / paths.plugins 路径正确; \
             ② 确认进程对该文件有读权限; ③ 修复后重启服务）",
            p.display(),
            e
        )
    })?;
    let manifest: PluginManifestFile = serde_json::from_str(&content).map_err(|e| {
        format!(
            "插件清单 JSON 非法 {}: {}（自诊断指引: ① 校验 JSON 语法; \
             ② 合法形态见 README「插件清单」章节: {{\"plugins\":{{\"demo-services\":{{\"enabled\":true}},\"physics-services\":{{\"enabled\":true}}}}}}）",
            p.display(),
            e
        )
    })?;
    let mut mounts: Vec<(&'static str, PluginMount)> = Vec::new();
    for (id, entry) in &manifest.plugins {
        let def = PLUGIN_DEFS.iter().find(|d| d.id == *id).ok_or_else(|| {
            let ids = PLUGIN_DEFS
                .iter()
                .map(|d| d.id)
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "插件清单含未知的进程内插件 id '{id}' — 当前可用: [{ids}]。\
                 自诊断指引: ① 进程外服务不走 plugin_manifest,请配置 service_registry.json; \
                 ② 新增进程内插件需在 main.rs PLUGIN_DEFS 登记后才能进清单"
            )
        })?;
        if !entry.enabled {
            mounts.push((def.id, PluginMount::Off));
            continue;
        }
        match &entry.services {
            None => mounts.push((def.id, PluginMount::All)),
            Some(names) => {
                if names.is_empty() {
                    return Err(format!(
                        "插件清单 {}.services 为空 — 若要停用全部原生服务请直接 \
                         \"enabled\": false; 若要启用请至少列出一个服务名。合法服务名: [{}]",
                        def.id,
                        plugin_service_names(def).join(", ")
                    ));
                }
                mounts.push((def.id, PluginMount::Subset(names.clone())));
            }
        }
    }
    Ok(mounts)
}

/// 将 `serde_json::Value` 转换为 `evorule_tcb::JsonValue`
///
/// 注：runtime 改用 `SessionApi::load_merged_transforms_from_fs` 后，此函数仅在
/// `load_core_eval`（单元测试专用）中使用，故整体标 `#[cfg(test)]`。
#[cfg(test)]
fn serde_to_tcb(v: serde_json::Value) -> JsonValue {
    use std::collections::BTreeMap;
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
            let mut map = BTreeMap::new();
            for (k, val) in obj {
                map.insert(k, serde_to_tcb(val));
            }
            JsonValue::Object(map)
        }
    }
}

/// 加载 SQL 语句白名单（statement_whitelist.json）
fn load_statement_whitelist(path: Option<&std::path::Path>) -> Result<StatementWhitelist, String> {
    StatementWhitelist::load_from_file(path)
}

/// 确保目录存在
fn ensure_dir(path: &PathBuf) -> Result<(), String> {
    // M4 修复: 旧代码直接用 path.exists 和 create_dir_all(path),
    // 但当 path 为空路径(如 db_path 是 "evorule.db" 时 parent 返回 Some(""))
    // 时,create_dir_all("") 在某些平台会返回错误。
    // 现在检查 path 是否为空,空路径视为当前目录,无需创建。
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    if !path.exists() {
        std::fs::create_dir_all(path)
            .map_err(|e| format!("创建目录失败 {}: {}", path.display(), e))?;
    }
    Ok(())
}

/// 加载宪法文件(server_eval.json)并转换为 transform 列表
///
/// 注：当前服务器 runtime 不再使用此函数（改用 SessionApi::load_merged_transforms_from_fs 统一合并），
/// 保留仅用于单元测试。
#[cfg(test)]
fn load_core_eval(path: &PathBuf) -> Result<Vec<JsonValue>, String> {
    let json_str = std::fs::read_to_string(path)
        .map_err(|e| format!("读取宪法文件失败 {}: {}", path.display(), e))?;
    let json: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| format!("解析宪法文件失败: {}", e))?;
    let transform: Vec<JsonValue> = json
        .get("transform")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().cloned().map(serde_to_tcb).collect())
        .unwrap_or_default();
    if transform.is_empty() {
        return Err(format!(
            "宪法文件中没有 transform 规则(文件: {}),\
             请检查文件内容是否包含 \"transform\" 数组字段",
            path.display()
        ));
    }
    Ok(transform)
}

/// 初始化日志订阅器（支持 plain 和 json 两种格式，支持文件持久化）
///
/// # 参数
/// - `level`: 日志级别（error/warn/info/debug/trace）
/// - `format`: 日志格式（plain/json）
/// - `log_file`: 日志文件路径（可选，指定后启用文件持久化）
///
/// # 返回
/// `Some(WorkerGuard)` 当启用文件持久化时返回,调用方必须持有 guard 直到进程退出。
/// `None` 当仅输出到控制台时返回。
///
/// # C2 修复
/// 旧代码 `let (non_blocking, _guard) = ...` 把 guard 作为函数局部变量,
/// 函数返回时 guard 被 drop,tracing_appender 后台线程立即退出,
/// 导致后续所有日志写入被丢弃(服务器看似正常运行但无日志输出)。
/// 现在把 guard 返回给 main,保证后台线程存活到进程退出。
fn init_logging(
    level: &str,
    format: &str,
    log_file: Option<&PathBuf>,
) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let filter = tracing_subscriber::EnvFilter::try_new(level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let is_json = format.to_lowercase() == "json";

    if let Some(file_path) = log_file {
        let dir = file_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        let file_name = file_path
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("evorule.log"));

        let appender = RollingFileAppender::new(Rotation::DAILY, dir, file_name);
        let (non_blocking, guard) = tracing_appender::non_blocking(appender);

        let builder = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(non_blocking);

        if is_json {
            let _ = tracing::subscriber::set_global_default(builder.json().finish());
        } else {
            let _ = tracing::subscriber::set_global_default(builder.finish());
        }
        return Some(guard);
    }

    let builder = tracing_subscriber::fmt().with_env_filter(filter);

    if is_json {
        let _ = tracing::subscriber::set_global_default(builder.json().finish());
    } else {
        let _ = tracing::subscriber::set_global_default(builder.finish());
    }
    None
}

/// 读取日志目录并按修改时间升序返回日志文件列表（最新在最后）
///
/// C1 修复: RollingFileAppender::new(Rotation::DAILY, dir, "evorule.log")
/// 生成的文件名是 "evorule.log.YYYY-MM-DD",不是 ".log" 结尾。
/// 用 starts_with("evorule.log") 同时匹配 "evorule.log"(无轮转)和
/// "evorule.log.2026-07-28"(轮转文件)。
fn list_log_files(log_dir: &PathBuf) -> Vec<std::fs::DirEntry> {
    let entries = match std::fs::read_dir(log_dir) {
        Ok(e) => e,
        Err(e) => {
            warn!("读取日志目录失败 {}: {}", log_dir.display(), e);
            return Vec::new();
        }
    };
    let mut files: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("evorule.log"))
        .collect();
    files.sort_by(|a, b| {
        a.metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
            .cmp(
                &b.metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH),
            )
    });
    files
}

// 日志清理任务多分支, 拆函数需共享任务状态。详见 GATE_REFERENCE.md §六(豁免索引)
#[allow(clippy::cognitive_complexity)]
async fn log_cleanup_task(log_dir: PathBuf, max_days: u32, max_size_mb: u64) {
    let interval = Duration::from_secs(3600);
    let max_size_bytes = max_size_mb * 1024 * 1024;

    loop {
        tokio::time::sleep(interval).await;

        if !log_dir.exists() {
            continue;
        }

        let log_files = list_log_files(&log_dir);
        if log_files.is_empty() {
            continue;
        }

        // H1 修复: 用 saturating_sub 防止 u64 减法 underflow
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_else(|_| std::time::Duration::from_secs(0))
            .as_secs();
        let cutoff = now_secs.saturating_sub((max_days as u64) * 24 * 3600);

        // === 阶段一: 按过期时间清理 ===
        let mut deleted_count = 0;
        let mut surviving_files = Vec::with_capacity(log_files.len());
        for entry in log_files {
            let mtime = match entry.metadata().and_then(|m| m.modified()) {
                Ok(t) => t
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                Err(_) => {
                    surviving_files.push(entry);
                    continue;
                }
            };

            if mtime < cutoff {
                if let Err(e) = std::fs::remove_file(entry.path()) {
                    warn!("删除过期日志文件失败 {}: {}", entry.path().display(), e);
                    surviving_files.push(entry);
                } else {
                    deleted_count += 1;
                }
            } else {
                surviving_files.push(entry);
            }
        }

        if deleted_count > 0 {
            info!("已清理 {} 个过期日志文件", deleted_count);
        }

        // M1 修复: 用 surviving_files 重新计算总大小
        let total_size: u64 = surviving_files
            .iter()
            .filter_map(|e| e.metadata().ok())
            .map(|m| m.len())
            .sum();

        // === 阶段二: 按总大小清理(超出 max_size_bytes 时删除最旧文件) ===
        if total_size > max_size_bytes {
            let mut to_delete = Vec::new();
            let mut current_size = total_size;
            let newest_idx = surviving_files.len().saturating_sub(1);

            for (i, entry) in surviving_files.iter().enumerate() {
                if current_size <= max_size_bytes {
                    break;
                }
                if i == newest_idx {
                    continue;
                }
                if let Ok(len) = entry.metadata().map(|m| m.len()) {
                    to_delete.push(entry.path().clone());
                    current_size = current_size.saturating_sub(len);
                }
            }

            for path in &to_delete {
                if let Err(e) = std::fs::remove_file(path) {
                    warn!("删除日志文件失败 {}: {}", path.display(), e);
                }
            }

            if !to_delete.is_empty() {
                info!(
                    "已清理 {} 个日志文件以释放空间，原大小: {}MB，目标: {}MB",
                    to_delete.len(),
                    total_size / 1024 / 1024,
                    max_size_mb
                );
            }
        }
    }
}

/// 启动期认证策略校验（UV-116 显式豁免安全策略）。
///
/// 无 auth_token 时按"绑定地址 × 显式声明"二维判定：
/// - 非 loopback（含地址解析失败，安全侧失败）：一律 fail-closed 拒绝
///   （既有 B3 策略，本参数不提供豁免口子）；
/// - loopback：须显式声明 `--insecure-serve` 才允许无认证启动，否则 fail-fast
///   并给三选一自诊断指引（旧 0.4.x 在此隐式放行，漏配即静默裸奔——市场写
///   接口匿名可写实测坐实）。
///
/// 返回 Err(拒绝原因) = 拒绝启动；Ok(()) = 按当前配置放行。
fn validate_auth_policy(
    auth_token: Option<&str>,
    insecure_serve: bool,
    addr: &str,
) -> Result<(), String> {
    if auth_token.is_some() {
        return Ok(());
    }
    // H3 修复（保留）：SocketAddr 解析判断 loopback，覆盖 IPv4/IPv6；
    // 解析失败视为非 loopback（安全侧失败）。
    let is_non_loopback = addr
        .parse::<std::net::SocketAddr>()
        .map(|socket| !socket.ip().is_loopback())
        .unwrap_or(true);
    if is_non_loopback {
        return Err(format!(
            "🛑 拒绝启动：服务器绑定到非 loopback 地址 {addr} 但未设置认证 token。\n\
             这是 fail-closed 安全策略，不受 --insecure-serve 影响。\n\
             生产环境必须设置 --auth-token 或 EVORULE_AUTH_TOKEN 环境变量；\n\
             本地开发请绑定到 loopback 地址（如 --addr 127.0.0.1:18080）。"
        ));
    }
    if !insecure_serve {
        return Err(
            "🛑 拒绝启动：绑定 loopback 且未设置认证 token，且未显式声明 --insecure-serve。\n\
             无认证模式必须显式声明（显式豁免安全策略，防漏配静默裸奔）。三选一：\n  \
             1) 设置 --auth-token <token> 或 EVORULE_AUTH_TOKEN（正式部署，推荐）；\n  \
             2) 显式加 --insecure-serve 声明接受无认证（仅限本机回环开发/体验包演示场景）；\n  \
             3) 配置文件 auth.token 提供凭据。"
                .to_string(),
        );
    }
    Ok(())
}

#[tokio::main]
// 主函数集成所有子命令 + 启动流程, 268 行是当前架构必要。详见 GATE_REFERENCE.md §六(豁免索引)
#[allow(clippy::too_many_lines)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // 加载 JSON 配置文件（若指定）
    // M3 修复: 注释原本写"TOML",但实际加载的是 JSON(serde_json::from_str)。
    let file_config = load_config_file(&cli.config);

    // 按 CLI > env > file > default 优先级解析
    // （pdf_font 提前取出：它不经 ResolvedConfig 链，直接注入 pdf_export 的
    //  进程级静态配置——该模块唯一消费者是 /api/export/pdf handler）
    let pdf_font = cli.pdf_font.clone();
    let cfg = ResolvedConfig::resolve(cli, file_config);

    // W6：服务端 PDF 字体覆盖（--pdf-font；缺省=自动探测系统字体）
    evorule_server::api::pdf_export::set_font_override(pdf_font);

    // 1. 初始化日志（支持 JSON 结构化日志，支持文件持久化）
    // C2 修复: 必须持有 _log_guard 直到进程退出,否则 tracing_appender 后台线程
    // 会被 drop,导致文件日志写入全部丢失。
    let _log_guard = init_logging(&cfg.log_level, &cfg.log_format, cfg.log_file.as_ref());

    let start_time = Instant::now();
    info!("=== evorule-server 启动中 ===");
    info!("监听地址: {}", cfg.addr);
    info!("宪法路径: {}", cfg.core_eval.display());
    info!("规则目录: {}", cfg.rules_dir.display());
    info!("数据库: {}", cfg.db_path.display());
    info!("Memory 目录: {}", cfg.memory_dir.display());
    // 1.5 认证策略早期预检（UV-116）：在 WAL/DB/会话等资源初始化之前 fail-fast，
    // 拒绝发生在毫秒级、零资源占用；step 8 构建处保留同一校验（防御纵深，届时必过）。
    if let Err(reason) =
        validate_auth_policy(cfg.auth_token.as_deref(), cfg.insecure_serve, &cfg.addr)
    {
        eprintln!("{reason}");
        error!("{reason}");
        std::process::exit(1);
    }
    info!("日志格式: {}", cfg.log_format);
    info!(
        "日志输出: {}",
        if let Some(file) = &cfg.log_file {
            format!("文件: {}", file.display())
        } else {
            "控制台".to_string()
        }
    );
    info!(
        "日志清理: 保留 {} 天, 最大 {}MB",
        cfg.log_max_days, cfg.log_max_size_mb
    );
    info!(
        "认证: {}",
        if cfg.auth_token.is_some() {
            "已启用（Bearer 静态 token + 平台会话双通道）"
        } else {
            "已禁用（显式豁免 --insecure-serve，仅限本机回环）"
        }
    );

    // P-06: 打印 feature flag 状态（帮助用户了解当前构建配置）
    info!("=== Feature 状态 ===");
    info!(
        "  WAL persistence: {}",
        if cfg!(feature = "persistence") {
            "enabled"
        } else {
            "disabled (use --features persistence)"
        }
    );
    info!("  Prometheus metrics: {}", "enabled (H6: 应用层实现)");
    info!(
        "WAL: {}",
        if let Some(dir) = &cfg.wal_dir {
            format!(
                "目录: {}, fsync: {}, 最大大小: {}MB",
                dir.display(),
                cfg.wal_fsync,
                cfg.max_wal_size_bytes / (1024 * 1024)
            )
        } else {
            "已禁用（纯内存模式）".to_string()
        }
    );
    // 持久化防呆：未配 WAL 时共享事实（平台用户/角色/认证事件/治理事实）与会话
    // 审计链均纯内存，重启即全部丢失。必须显著警示（丢数据风险 + 配置方法），
    // 不允许静默降级——与共享事实恢复失败拒绝启动（AUDIT-A1）同一防呆口径。
    if cfg.wal_dir.is_none() {
        warn!(
            "未配置 --wal-dir：平台用户/角色/认证事件等共享事实与会话审计链均为纯内存模式，\
             服务重启即全部丢失（开发模式语义，正式部署不可接受）。\
             启用持久化：--wal-dir <目录>（或环境变量 EVORULE_WAL_DIR / 配置文件 paths.wal_dir）"
        );
    }
    info!(
        "实时审计验证: {}",
        if cfg.auto_verify {
            format!(
                "已启用（阈值: {}, 间隔: {}）",
                cfg.auto_verify_threshold, cfg.auto_verify_interval
            )
        } else {
            "已禁用".to_string()
        }
    );
    // :演示登录入口状态回显（经 auth/status 下发给登录页）
    info!(
        "演示登录: {}",
        if cfg.demo_auth {
            "已启用（默认；生产部署建议 --demo-auth false）".to_string()
        } else {
            "已禁用（登录页不显示演示模式入口）".to_string()
        }
    );

    // 2. 加载规则（TCB 宪法 core_eval.json + rules_dir 业务规则合并）
    // cfg.rules_dir 之前被解析但从未消费，现在真正合并。
    // 复用 SessionApi::load_merged_with_layout（统一一份合并逻辑，避免双份代码漂移）；
    // 同时产出规则集 layout（下标→来源/指令类型解析 + 版本哈希），供命中统计聚合器使用。
    let step_start = Instant::now();
    let (core_eval, ruleset_layout) =
        SessionApi::load_merged_with_layout(&cfg.core_eval, &cfg.rules_dir)?;
    info!(
        "已加载 {} 条 transform 规则（ruleset_version={}，耗时: {}ms）",
        core_eval.len(),
        ruleset_layout.ruleset_version,
        step_start.elapsed().as_millis()
    );

    // 3. 确保数据目录存在
    let step_start = Instant::now();
    if let Some(parent) = cfg.db_path.parent() {
        ensure_dir(&parent.to_path_buf())?;
    }
    ensure_dir(&cfg.memory_dir)?;
    if let Some(wal_dir) = &cfg.wal_dir {
        ensure_dir(wal_dir)?;
    }
    // P10: workspace 元数据库父目录
    if let Some(parent) = cfg.workspace_db.parent() {
        ensure_dir(&parent.to_path_buf())?;
    }
    info!(
        "数据目录检查完成（耗时: {}ms）",
        step_start.elapsed().as_millis()
    );

    // 4. 初始化 I/O handler（H5: handler 实现来自 evorule-io-handlers crate）
    let step_start = Instant::now();
    let db = DbHandler::connect_file(&cfg.db_path)
        .await
        .map_err(|e| format!("数据库连接失败: {}", e))?;
    info!(
        "数据库连接完成（耗时: {}ms）",
        step_start.elapsed().as_millis()
    );

    // 加载 service_registry.json（call_service/call_external 的 service_name→URL 映射）
    let step_start = Instant::now();
    let registry = match &cfg.service_registry {
        Some(path) => ServiceRegistry::load_from_file(path)
            .map_err(|e| format!("加载 service_registry 失败: {}", e))?,
        None => ServiceRegistry::empty(),
    };
    let reg_count = registry.len();
    // 服务绑定核对集：注册表服务名注入 SessionApi，与原生叶子能力并集
    let registry_names = registry.service_names();
    if reg_count == 0 {
        warn!(
            "ServiceRegistry 为空 — call_service/call_external 会返回 'unknown service_name'，\
             请通过 --service-registry 设置 service_registry.json"
        );
    }

    // 加载 SQL 语句白名单（statement_whitelist.json）
    let statement_whitelist = load_statement_whitelist(cfg.statement_whitelist.as_deref())?;
    info!("已加载 {} 条 SQL 白名单模板", statement_whitelist.len());

    // H5: 通过 builder 模式注册 handler(trait object 动态分发)
    // HTTP_GET 走原生 HttpHandler（直接传 URL，无 service_name 翻译）
    // CALL_EXTERNAL / CALL_SERVICE 走 ServiceRegistryHandler（service_name→URL 翻译）
    // --allow-loopback 时用开发模式构造器，放行 127.0.0.1 / 私有 IP（仅本地开发）
    let http = Arc::new(if cfg.allow_loopback {
        warn!("🔓 --allow-loopback 已启用：SSRF 防护放行 loopback 和私有 IP（仅限本地开发！）");
        HttpHandler::new_dev_allow_loopback()
    } else {
        HttpHandler::new()
    });
    let svc_handler = Arc::new(ServiceRegistryHandler::new(registry.clone(), http.clone()));
    // /: 插件清单决定各插件挂载形态（缺省全启,存量零迁移）。
    // 校验失败 → 启动 fail-fast（错误含自诊断指引）。
    let plugin_mounts = load_plugin_mounts(cfg.plugins.as_ref())?;
    // 按登记表声明序构建回落链:各插件路由原生优先,未命中回落链尾(HTTP 注册表)。
    // 逆序包裹——链条头 = 第一个已挂载插件;全停用时链条头 = 直连 svc_handler。
    let mut chain_tail: Arc<dyn evorule_reactor::IoHandler> = svc_handler.clone();
    let mut plugin_health = serde_json::Map::new();
    // /api/services native 对账清单:按 manifest 实际挂载状态收集(Off 不入清单,
    // Subset 仅启用子集)——对账清单 = 真实可路由服务,与回落链同一事实来源。
    let mut native_service_infos = Vec::new();
    for (id, mount) in plugin_mounts.iter().rev() {
        let Some(def) = PLUGIN_DEFS.iter().find(|d| d.id == *id) else {
            continue; // 清单解析已锁定 id ∈ PLUGIN_DEFS,此分支不可达,防御性跳过
        };
        match mount {
            PluginMount::Off => {
                info!(
                    "插件清单: {id} enabled=false — 该插件服务 call_service/call_external \
                     直连 HTTP 服务注册表（{} entries）",
                    reg_count
                );
                plugin_health.insert(id.to_string(), serde_json::json!({ "enabled": false }));
            }
            PluginMount::All => {
                if cfg.plugins.is_some() {
                    info!(
                        "插件清单: {id} 全部启用（{} 个原生服务）",
                        plugin_service_names(def).len()
                    );
                }
                plugin_health.insert(
                    id.to_string(),
                    serde_json::json!({ "enabled": true, "services": plugin_service_names(def) }),
                );
                native_service_infos.extend(plugin_service_infos(def));
                chain_tail = evorule_plugin_kit::mount_router(def.defs, chain_tail, None)
                    .map_err(|e| format!("插件清单校验失败: {}", e))?;
            }
            PluginMount::Subset(names) => {
                let refs: Vec<&str> = names.iter().map(String::as_str).collect();
                let router =
                    evorule_plugin_kit::mount_router(def.defs, chain_tail.clone(), Some(&refs))
                        .map_err(|e| format!("插件清单校验失败: {}", e))?;
                // 健康呈现按声明序过滤(与路由器 enabled_service_names 同口径)
                let all_names = plugin_service_names(def);
                let enabled_ordered: Vec<&str> = all_names
                    .iter()
                    .copied()
                    .filter(|n| refs.contains(n))
                    .collect();
                info!(
                    "插件清单: {id} 启用子集 [{}]（声明表全量 {},已裁剪 {}）",
                    enabled_ordered.join(", "),
                    all_names.len(),
                    all_names.len() - enabled_ordered.len()
                );
                plugin_health.insert(
                    id.to_string(),
                    serde_json::json!({ "enabled": true, "services": enabled_ordered }),
                );
                native_service_infos.extend(
                    plugin_service_infos(def)
                        .into_iter()
                        .filter(|i| refs.contains(&i.name.as_str())),
                );
                chain_tail = router;
            }
        }
    }
    // 逆序循环收集 = 登记声明序的倒序,reverse 恢复声明序(对账清单与登记表同序)。
    native_service_infos.reverse();
    let call_handler: Arc<dyn evorule_reactor::IoHandler> = chain_tail;
    let memory = Arc::new(MemoryHandler::new(cfg.memory_dir.clone()));
    let db_wrapped = WhitelistedDbHandler::new(db, statement_whitelist);
    // /: 注入插件健康快照 → /api/health 的 plugins 节(启动后不可变)。
    // 按登记表逐插件如实呈现运行时挂载事实,键序 = 插件登记声明序。
    evorule_server::api::server::set_plugin_health(serde_json::Value::Object(plugin_health));
    let dispatcher = IoDispatcher::builder()
        .register(IoType::call_external(), call_handler.clone())
        .register(IoType::http_get(), http.clone())
        .register(IoType::call_service(), call_handler.clone())
        .register(IoType::query_db(), Arc::new(db_wrapped))
        .register(IoType::save_memory(), memory)
        .build();
    // 为 SessionApi 保留一份 dispatcher 副本：每个新 session 会 clone 此对象
    // 再 spawn 一个 IoSubscriber，将 session reactor 的 IoRequest 分发到 handler。
    // 不注入时，session 的 IoRequest 无人处理，60s 后超时（错误：
    // "I/O 请求超时错误：pending I/O 超过 60s 未响应"）。
    // 主 dispatcher 仍由下方单反应器 IoSubscriber 消费。
    let session_dispatcher = dispatcher.clone();
    info!(
        "[1/4] I/O handler 已初始化（DB+白名单/HTTP+服务注册/Memory），\
         service_registry: {} entries（耗时: {}ms）",
        reg_count,
        step_start.elapsed().as_millis()
    );

    // 4.5 创建 Prometheus 指标共享引用 + IoSubscriber（带 metrics）
    // Prometheus 指标通过 IoSubscriber 注入到 I/O 调度路径
    // hit-stats 聚合器注册自身指标（evorule_rule_hits_total /
    // evorule_rules_zero_hits）到同一 registry；注册失败 fail-fast 拒绝启动。
    let prometheus_metrics = evorule_server::metrics_impl::shared_prometheus_metrics()
        .map_err(|e| format!("Prometheus 指标初始化失败: {}", e))?;
    let metrics: SharedMetrics = prometheus_metrics.clone();
    let hit_stats = Arc::new(
        evorule_server::api::hit_stats::HitStatsAggregator::with_registry(
            ruleset_layout,
            prometheus_metrics.registry(),
        )
        .map_err(|e| format!("hit-stats 指标注册失败: {}", e))?,
    );
    let subscriber = IoSubscriber::new(dispatcher)
        .with_metrics(metrics.clone())
        .with_skip(Arc::new(
            evorule_server::api::server::is_external_executor_request,
        ));

    // 5. 创建单反应器（GovernanceApi 向后兼容路由用）
    // 单反应器模式也启用 WAL 持久化（与多会话一样，保证重启后可回放审计链）
    let mut reactor_builder = Reactor::builder(core_eval.clone()).max_rounds(cfg.max_rounds);
    if let Some(wal_dir) = &cfg.wal_dir {
        let single_wal = wal_dir.join("governance_single_reactor.wal");
        match FactsLog::with_wal_and_fsync(&single_wal, cfg.wal_fsync) {
            Ok(fl) => {
                reactor_builder = reactor_builder.facts_log(fl);
                info!(
                    "单反应器 WAL 已启用：{}（fsync={}, max_wal_size_bytes={}）",
                    single_wal.display(),
                    cfg.wal_fsync,
                    cfg.max_wal_size_bytes,
                );
            }
            Err(e) => {
                warn!("单反应器 WAL 创建失败，退化为纯内存模式：{}", e);
            }
        }
    }
    let reactor = reactor_builder.build();
    // H4 修复: 保留 reactor_handle,旧代码用 _handle 丢弃了句柄,
    // 导致反应器任务 panic 或异常退出时无法被检测或取消,成为孤儿任务。
    // 现在保留 handle,在服务器退出时 abort 反应器,防止进程卡死。
    let (tx, _rx, event_tx, reactor_handle, facts_log) = reactor.spawn();

    // 6. spawn IoSubscriber 任务（订阅 event，执行 I/O，回写 IoResponse）
    // H2 修复: 旧代码 `let _ = subscriber.run(...).await` 吞掉了错误,
    // I/O 处理崩溃时无任何日志,服务器看似正常但所有 I/O 请求超时。
    // 现在记录错误日志,便于运维定位。
    let sub_rx = event_tx.subscribe();
    let sub_tx = tx.clone();
    tokio::spawn(async move {
        if let Err(e) = subscriber.run(sub_rx, sub_tx).await {
            error!("IoSubscriber 异常退出: {}", e);
        }
    });

    // 单反应器路径的命中归因记录（与多会话共享同一聚合器）
    let hit_rx = event_tx.subscribe();
    let hit_agg = hit_stats.clone();
    tokio::spawn(async move {
        evorule_server::api::hit_stats::run_recorder(hit_rx, (*hit_agg).clone()).await;
    });

    // 7. spawn 日志清理任务（定期清理过期和超大日志文件）
    if let Some(log_file) = &cfg.log_file {
        if let Some(log_dir) = log_file.parent() {
            let log_dir = log_dir.to_path_buf();
            let max_days = cfg.log_max_days;
            let max_size_mb = cfg.log_max_size_mb;
            tokio::spawn(async move {
                log_cleanup_task(log_dir, max_days, max_size_mb).await;
            });
        }
    }

    info!(
        "[2/4] 反应器 + I/O 订阅者已启动（耗时: {}ms）",
        step_start.elapsed().as_millis()
    );

    // 8. 创建审计器 + GovernanceApi + SessionApi + AppState
    let step_start = Instant::now();
    let auditor = Auditor::new(facts_log.clone());
    let api = GovernanceApi::new(tx.clone(), facts_log, auditor);
    let session_api = SessionApi::new_with_full_config(
        core_eval,
        cfg.max_rounds,
        cfg.wal_dir.clone(),
        cfg.wal_fsync,
        cfg.max_wal_size_bytes,
        cfg.auto_verify,
        cfg.auto_verify_threshold,
        cfg.auto_verify_interval,
        cfg.core_eval.clone(),
        cfg.rules_dir.clone(),
    )
    .with_dispatcher(session_dispatcher)
    .with_bound_services(registry_names)
    // 注入命中统计聚合器（注册了 Prometheus 指标、含权威初始 layout）
    .with_hit_stats(hit_stats)
    // C5/C6：注册表显式绑定元数据（version/description）注入，供 /api/services 能力对账
    // 与 sensitive 服务绑定核对使用（02 方案服务契约三层闭环 层3）。
    .with_registry_services(registry.service_metadata())
    // 插件对账泛化:/api/services native 清单 = manifest 实际挂载的全插件服务
    // (含 plugin 归属/描述/敏感标记);invoke 直调复用同一回落链,无第二执行路径。
    .with_native_services(native_service_infos)
    .with_service_chain(call_handler.clone());

    // 创建 readiness flag（优雅退出时设为 false）
    let readiness: Arc<AtomicBool> = Arc::new(AtomicBool::new(true));

    // 创建跨会话共享事实存储
    // 当 wal_dir 配置时，从 WAL + metadata 恢复历史共享事实；否则纯内存模式
    // AUDIT-A1 同款修复（2026-08-27）：恢复失败时拒绝启动而非静默降级——
    // 共享事实是跨会话审计链的一部分，残缺状态下继续服务会破坏可回放性承诺。
    let shared_facts = if let Some(wal_dir) = &cfg.wal_dir {
        let shared_wal = wal_dir.join("shared_facts.wal");
        let shared_meta = wal_dir.join("shared_facts_meta.json");
        match SharedFactsLog::recover(&shared_wal, &shared_meta) {
            Ok(log) => {
                info!(
                    "共享事实 WAL 已恢复：{}（metadata: {}）",
                    shared_wal.display(),
                    shared_meta.display()
                );
                log
            }
            Err(e) => {
                error!(
                    "共享事实 WAL 恢复失败，拒绝启动（请检查磁盘/权限或备份后清理 wal_dir）：{}",
                    e
                );
                return Err(format!("shared facts WAL recovery failed: {e}").into());
            }
        }
    } else {
        SharedFactsLog::new()
    };

    // P10: 初始化 Workspace 元数据库 + 服务 (多租户工作空间 + 规则元数据管理)
    // workspace_db 独立于业务 db_path,存储 workspace/member/rule/session 元数据。
    // SessionApi 实现 SessionOps trait,通过 Arc 桥接到底层 SessionManager,
    // 使 WorkspaceService 能创建/fork/close 会话而无需直接依赖 evorule-governance。
    //
    // 三层架构完整接入 (SANDBOX_ORCHESTRATION_DESIGN.md + PUBLISH_QUEUE_DESIGN.md):
    // - WorkspaceService: 多租户工作空间 + 会话管理
    // - RuleMetaService: 规则元数据 + 状态机 + BLAKE3 哈希
    // - SandboxService: 沙盒编排 (fork + 合成数据 + 测试报告)
    // - PublishService: 发布队列 + 三级权限 + 滚动 session 热重载
    // - SessionSwitchedBroadcaster: U7 SSE session_switched 推送
    let workspace_db = evorule_workspace::WorkspaceDb::open(&cfg.workspace_db)
        .map_err(|e| format!("workspace db 初始化失败: {}", e))?;
    let workspace_db = Arc::new(workspace_db);
    // T5: 把 workspace 元数据库注入 SessionApi，使 bundle 导入时写入审计溯源（bundle_imports 表，
    // 管理元数据墙钟旁路，不参与 fact/哈希/审计验证链）。须在 workspace_db 创建后、AppState 组装前注入。
    let session_api = session_api.with_workspace_db(workspace_db.clone());
    // ①: reaper 启动移到 workspace_db 注入之后——生产会话保活 + 失忆自愈
    // 重建依赖该接线(原时序在注入前启动,reaper 拿不到 production_state)。
    session_api.start_reaper();
    let session_ops: Arc<dyn evorule_workspace::SessionOps> = Arc::new(session_api.clone());
    let workspace_service = Arc::new(evorule_workspace::WorkspaceService::new(
        workspace_db.clone(),
        session_ops.clone(),
    ));
    let rule_meta_service = Arc::new(evorule_workspace::RuleMetaService::new(
        workspace_db.clone(),
    ));
    // SessionSwitchedBroadcaster (U7): 共享底层 channel 映射, Clone 廉价
    let switcher = evorule_workspace::SessionSwitchedBroadcaster::new();
    // SandboxService: 沙盒编排 (持有 session_ops, 通过 db 直接查询规则版本)
    let sandbox_service = Arc::new(evorule_workspace::SandboxService::new(
        workspace_db.clone(),
        session_ops.clone(),
    ));
    // RollingSessionService: 滚动 session 热重载 (reload → fork → switch → audit → broadcast → drain)
    let rolling_session = evorule_workspace::RollingSessionService::new(
        workspace_db.clone(),
        session_ops.clone(),
        switcher.clone(),
    );
    // PublishService: 发布队列 + 三级权限 (持有 RollingSessionService)
    // 审计⑥ 批 B C5: 注入 rules_dir, 发布审批通过时规范 DatasetBundle 原子落盘
    let publish_service = Arc::new(evorule_workspace::PublishService::new(
        workspace_db.clone(),
        rolling_session,
        cfg.rules_dir.clone(),
    ));
    // VerdictService: 判定契约 + wall-clock 旁路 (界面升级 v1.0 阶段 A.3/A.4)
    let verdict_service = Arc::new(evorule_workspace::VerdictService::new(workspace_db.clone()));
    let workspace_state = evorule_workspace::WorkspaceState::new(
        workspace_service,
        rule_meta_service,
        sandbox_service,
        publish_service,
        Arc::new(switcher),
        verdict_service,
    );
    info!(
        "Workspace 元数据库已就绪: {} (P10: 多租户 + 规则元数据 + 沙盒编排 + 发布队列)",
        cfg.workspace_db.display()
    );

    // : 全新实例引导初始化(启动期,幂等)。
    // 死锁链(修复前):沙盒 fork 需 production_state.current_session_id →
    // 生产会话仅由发布流(rolling_session)初始化 → 发布闸门一又要求已完成
    // 的沙盒报告 → 全新实例三环互锁,"建规则→沙盒验证→发布"主链不可达
    // (分发包首启同样命中;单测因预置 update_production_state 绕过而掩盖)。
    // 修复:current_session_id=NULL(从未初始化)时自动创建初始生产会话
    // (空规则集,仅宪法 core_eval),沙盒可 fork、闸门一保持刚性（不动）。
    // 初始化不构成发布:ruleset_version 保持 0,hash 置空串,operator 标记
    // system:bootstrap 可追溯。已有生产会话的实例不受影响(幂等跳过)。
    //
    // 补强(重启失忆替换):SessionManager 为内存态,重启后既有
    // current_session_id 指向已失忆会话 → 沙盒 fork 404。启动期以
    // session_exists 校验,失忆视同未初始化:重建会话并替换引用,
    // 但保留既有 ruleset_version/ruleset_hash(会话是进程内对象,
    // 规则集状态经 rules_dir/production_state 持久,重建不改版本语义)。
    let prod_state = workspace_db
        .get_production_state()
        .map_err(|e| format!("启动期读取 production_state 失败: {e}"))?;
    let need_bootstrap = match prod_state.current_session_id {
        None => true,
        Some(sid) => {
            let alive = session_ops.session_exists(sid as u64).await;
            if !alive {
                warn!(
                    stale_session_id = sid,
                    ": 生产会话已失忆(server 重启后 SessionManager 为内存态),将重建"
                );
            }
            !alive
        }
    };
    if need_bootstrap {
        let init_session_id = session_ops
            .create_session()
            .await
            .map_err(|e| format!("初始生产会话创建失败: {e}"))?;
        workspace_db
            .update_production_state(
                init_session_id as i64,
                // 保留重启前的版本与哈希(仅替换会话引用);
                // 全新实例为 0/空串(初始化不构成发布)。
                prod_state.ruleset_version,
                prod_state.ruleset_hash.as_deref().unwrap_or(""),
                "system:bootstrap",
            )
            .map_err(|e| format!("production_state 写入失败: {e}"))?;
        info!(
            init_session_id,
            ": 引导初始化 — 已创建初始生产会话(空规则集),沙盒/发布链解锁"
        );
    } else {
        info!(
            current_session_id = prod_state.current_session_id,
            ": 已有生产会话,跳过引导初始化(幂等)"
        );
    }

    // AppState 注入 metrics 和 readiness
    // H6: metrics 总是注入（PrometheusMetrics 实现 IoMetrics trait）
    let state = AppState::new(
        api,
        session_api,
        metrics.clone(),
        readiness.clone(),
        shared_facts,
        workspace_state,
        Arc::new(InputSanitizer::with_default_rules()),
    )
    // :演示登录入口开关注入（经 auth/status 公开下发）
    .with_demo_auth(cfg.demo_auth);

    info!(
        "[3/4] 审计器 + GovernanceApi + SessionApi 已创建（耗时: {}ms）",
        step_start.elapsed().as_millis()
    );

    // 8. 构建服务器（带认证）
    let step_start = Instant::now();
    let auth = match &cfg.auth_token {
        Some(token) => {
            // B5-server：service token 仅在认证启用时生效（disabled 模式无身份区分）
            AuthConfig::new(vec![token.clone()], true)
                .with_service_tokens(cfg.service_token.iter().cloned().collect())
        }
        None => {
            if cfg.service_token.is_some() {
                warn!(
                    "EVORULE_SERVICE_TOKEN 已设置但认证未启用（无 auth_token），服务 token 被忽略"
                );
            }
            // UV-116 修复（显式豁免安全策略）：无 token 时按"绑定地址 × 显式声明"
            // 二维校验——非 loopback 一律 fail-closed（既有 B3 不放松）；loopback
            // 须显式 --insecure-serve 声明豁免，否则拒绝启动（旧实现隐式放行）。
            // 逻辑提取为 validate_auth_policy 以便四象限单测覆盖。
            if let Err(reason) =
                validate_auth_policy(cfg.auth_token.as_deref(), cfg.insecure_serve, &cfg.addr)
            {
                // 双通道输出:eprintln! 直写 stderr 不依赖 tracing subscriber
                // 状态(实测 error! 在本路径可能静默丢失→exit 1 无任何解释,
                // 违反"fail-fast+可自诊断"标准);error! 走日志文件留痕。
                eprintln!("{reason}");
                error!("{reason}");
                std::process::exit(1);
            }
            if cfg.auth_token.is_none() {
                info!("🔓 无认证模式（显式豁免 --insecure-serve，仅限本机回环：所有受保护端点匿名可达，勿绑定非回环地址）");
            }
            AuthConfig::disabled()
        }
    };
    let server = GovernanceServer::new(
        state,
        auth,
        cfg.addr.clone(),
        cfg.rate_limit_per_sec,
        200,
        cfg.allowed_origins.clone(),
        // S2：/metrics 端点是否需要认证（--metrics-auth 控制）
        cfg.metrics_auth,
        // OpenAPI Swagger UI 开关（--openapi-ui 控制，默认关闭）
        cfg.openapi_ui,
        // abort 强制中止端点开关（--allow-abort 控制，默认关闭，双保险）
        cfg.allow_abort,
        // 静态前端托管目录（--web-dir 控制，默认 None）
        cfg.web_dir.clone(),
    );

    // fail-fast：--web-dir 指定的目录必须存在 index.html，否则拒绝启动
    if let Some(dir) = &cfg.web_dir {
        if !dir.join("index.html").is_file() {
            error!(
                "🛑 拒绝启动：--web-dir {} 下未找到 index.html。\n\
                 请先构建前端产物（如 console-cloud 仓 `npm run build`，产物在 build/），\n\
                 再将该目录传入 --web-dir。",
                dir.display()
            );
            std::process::exit(1);
        }
        info!(
            "静态前端已启用：{}（同源托管，未命中路由回退 index.html）",
            dir.display()
        );
    }

    info!(
        "[4/4] HTTP 服务器已就绪，监听 {}（耗时: {}ms）",
        cfg.addr,
        step_start.elapsed().as_millis()
    );
    info!(
        "=== evorule-server 启动完成（总耗时: {}ms）===",
        start_time.elapsed().as_millis()
    );
    info!("端点：");
    info!("  健康检查: GET  http://{}/api/health", cfg.addr);
    info!("  Liveness: GET  http://{}/api/health/liveness", cfg.addr);
    info!("  Readiness: GET http://{}/api/health/readiness", cfg.addr);
    info!("  Metrics:  GET  http://{}/metrics", cfg.addr);
    info!("  创建会话: POST http://{}/api/sessions", cfg.addr);
    info!(
        "  提交命令: POST http://{}/api/sessions/{{id}}/command",
        cfg.addr
    );
    info!(
        "  查询状态: GET  http://{}/api/sessions/{{id}}/state",
        cfg.addr
    );
    info!(
        "  SSE 事件: GET  http://{}/api/sessions/{{id}}/events",
        cfg.addr
    );
    info!("  审计报告: GET  http://{}/api/audit", cfg.addr);
    info!("  OpenAPI 规范: GET http://{}/api/openapi.json", cfg.addr);
    info!(
        "  Swagger UI: {}",
        if cfg.openapi_ui {
            format!("GET http://{}/api/docs", cfg.addr)
        } else {
            "未启用（--openapi-ui 开启）".to_string()
        }
    );
    info!(
        "  强制中止: {}",
        if cfg.allow_abort {
            format!(
                "POST http://{}/api/sessions/{{id}}/abort（已启用 --allow-abort）",
                cfg.addr
            )
        } else {
            "未启用（--allow-abort 开启后注册，默认关闭双保险）".to_string()
        }
    );
    info!(
        "优雅退出：SIGTERM/SIGINT → readiness=false → 等待 {}s",
        GRACEFUL_SHUTDOWN_TIMEOUT.as_secs()
    );

    // 9. 启动服务器（带优雅退出）
    // 使用 into_make_service_with_connect_info 注入客户端 IP，
    // 以支持 GovernorLayer（速率限制）按 IP 限流
    // :bind 失败必须双通道可见 —— ①error! 级日志落 --log-file 文件
    // （此前 `?` 直接传播,日志文件止于启动 info 流无 ERROR 行）;②格式化错误
    // 消息返回 main（stderr 打印,分发包 bat 以 2>> 收集 stderr 后用户可查）。
    let bind_addr = &cfg.addr;
    let listener = match tokio::net::TcpListener::bind(bind_addr).await {
        Ok(l) => l,
        Err(e) => {
            // 端口号从 addr 字符串尾部截取(供 netstat 定位指引;解析失败给兜底提示)
            let port = bind_addr.rsplit(':').next().unwrap_or("<端口解析失败>");
            error!(
                "监听地址绑定失败 {}: {} — 端口大概率已被占用。自诊断指引: \
                 ①Windows 下 `netstat -ano | findstr :{port}` 找到占用进程 PID,\
                 任务管理器确认后结束该进程(常见为上次未退出的 evorule-server 残留实例);\
                 ②或用 --addr 换一个空闲端口(同时更新前端指向);\
                 ③若为治理端口 18081,同法处置 evorule-rule-serve 残留实例",
                bind_addr,
                e,
                port = port
            );
            return Err(format!(
                "监听地址绑定失败 {}: {} — 端口大概率已被占用\
                 （详见日志文件 ERROR 记录: netstat -ano | findstr :{port} 定位占用进程）",
                bind_addr,
                e,
                port = port
            )
            .into());
        }
    };
    let router = server.build_router();
    let serve = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    );

    // 优雅退出信号处理
    // C3 修复(含 regression 修复):
    // - 第一版修复用 `tokio::time::timeout(30s, graceful)` 包裹整个 serve,
    // 但 timeout 从服务器启动就开始计时,30s 内没收到信号就超时退出 → regression。
    // - 正确做法: 用 oneshot channel 协调。shutdown future 收到信号后:
    // 1. 立即返回(让 axum 停止接收新连接)
    // 2. 通过 oneshot 通知外部开始 30s 超时计时
    // 然后用 tokio::select! 在 `graceful.await` 和"信号后 30s sleep"之间选择。
    // 这样: 无信号时服务器永久运行; 有信号后最多等 30s in-flight 请求。
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let readiness_flag = readiness.clone();
    let shutdown = async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            match signal(SignalKind::terminate()) {
                Ok(mut sigterm) => {
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {
                            info!("收到 SIGINT (Ctrl+C) 信号，开始优雅退出...");
                        }
                        _ = sigterm.recv() => {
                            info!("收到 SIGTERM 信号，开始优雅退出...");
                        }
                    }
                }
                // M5 修复: 中文 field "错误" 改为英文 "error"
                Err(e) => {
                    tracing::warn!(error = %e, "SIGTERM handler 安装失败，仅监听 SIGINT");
                    match tokio::signal::ctrl_c().await {
                        Ok(()) => info!("收到 SIGINT (Ctrl+C) 信号，开始优雅退出..."),
                        Err(e) => {
                            // Windows 后台环境可能无法安装 ctrl_c handler
                            // 用 pending 让 shutdown 永不完成,服务器持续运行
                            warn!(error = %e, "信号处理器安装失败,服务器将持续运行直到进程被杀死");
                            std::future::pending::<()>().await;
                        }
                    }
                }
            }
        }
        #[cfg(not(unix))]
        {
            match tokio::signal::ctrl_c().await {
                Ok(()) => info!("收到 Ctrl+C 信号，开始优雅退出..."),
                Err(e) => {
                    // Windows 后台环境可能无法安装 ctrl_c handler
                    // 用 pending 让 shutdown 永不完成,服务器持续运行
                    warn!(error = %e, "ctrl_c handler 安装失败,服务器将持续运行直到进程被杀死");
                    std::future::pending::<()>().await;
                }
            }
        }

        // 立即标记不就绪,负载均衡器切走流量
        readiness_flag.store(false, Ordering::SeqCst);
        info!("已标记为不就绪（readiness=false），/api/health/readiness 将返回 503");
        // 通知外部开始 30s 超时计时
        let _ = shutdown_tx.send(());
        // shutdown future 在此立即返回,axum 将停止接收新连接,
        // 然后等待 in-flight 请求完成(由 with_graceful_shutdown 内部处理)。
    };

    // 优雅退出 + 30s 超时(仅在收到信号后才开始计时)
    let graceful = serve.with_graceful_shutdown(shutdown);
    tokio::select! {
        // 分支 A: axum 正常完成 graceful shutdown(in-flight 请求在 30s 内完成)
        result = graceful => {
            match result {
                Ok(()) => info!("服务器已优雅退出"),
                Err(e) => {
                    error!("服务器退出错误: {}", e);
                    return Err(e.into());
                }
            }
        }
        // 分支 B: 信号已收到但 30s 内 in-flight 请求未完成,强制结束
        _ = async {
            // 等待 shutdown future 发出的信号通知
            let _ = shutdown_rx.await;
            // 信号已收到,开始 30s 超时(此时 axum 已停止接收新请求)
            info!(
                "等待 in-flight 请求完成,最多 {}s",
                GRACEFUL_SHUTDOWN_TIMEOUT.as_secs()
            );
            tokio::time::sleep(GRACEFUL_SHUTDOWN_TIMEOUT).await;
        } => {
            error!(
                "优雅退出超时（{}s），仍有 in-flight 请求未完成，强制结束",
                GRACEFUL_SHUTDOWN_TIMEOUT.as_secs()
            );
        }
    }

    // H4 修复: 服务器退出后 abort 单反应器任务,防止孤儿任务阻止进程退出。
    // 单反应器是向后兼容模式,axum 退出后不再有新命令,反应器应被清理。
    if !reactor_handle.is_finished() {
        reactor_handle.abort();
        info!("已中止单反应器任务");
    }

    // 缺口6 修复: 服务器退出前导出当前生产 session 的 BLAKE3 审计链
    // (rolling_swap 关闭旧 session 时由缺口5处理; 此处处理 server 直接退出的场景)
    // 未导出的审计链会随 SessionManager 内存释放而永久丢失 — 对合规审计不可接受。
    match workspace_db.get_production_state() {
        Ok(prod_state) => {
            if let Some(session_id) = prod_state.current_session_id {
                let session_id = session_id as u64;
                info!(
                    session_id,
                    "正在导出生产 session 审计链 (缺口6: server 退出前持久化)..."
                );
                match evorule_workspace::rolling_session::export_production_audit_chain(
                    &session_ops,
                    session_id,
                )
                .await
                {
                    Some(path) => {
                        // 记录 session_closed (server_shutdown) 到 production_audit
                        let source_ws_ids = serde_json::json!([]).to_string();
                        let ruleset_hash = prod_state.ruleset_hash.unwrap_or_default();
                        if let Err(e) = workspace_db.insert_production_audit(
                            "session_closed",
                            prod_state.ruleset_version,
                            None,
                            &ruleset_hash,
                            session_id as i64,
                            &source_ws_ids,
                            "system",
                            Some("server_shutdown"),
                            Some(&path),
                            None,
                        ) {
                            warn!(
                                session_id,
                                error = %e,
                                "Failed to record session_closed audit event on shutdown (缺口6)"
                            );
                        }
                        info!(
                            session_id,
                            path = %path,
                            "生产 session 审计链已导出 (缺口6)"
                        );
                    }
                    None => {
                        warn!(
                            session_id,
                            "Failed to export production session audit chain on shutdown (缺口6)"
                        );
                    }
                }
            } else {
                info!("无活跃生产 session, 跳过审计链导出");
            }
        }
        Err(e) => {
            warn!(
                error = %e,
                "Failed to query production_state for audit chain export on shutdown (缺口6)"
            );
        }
    }

    info!("evorule-server 已停止");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use tempfile::TempDir;

    // ============ 启动期认证策略校验（UV-116 四象限 + 边界） ============

    #[test]
    fn auth_policy_token_provided_always_ok() {
        // 有 token：loopback / 非 loopback 均放行（token 优先，地址无关）
        assert!(validate_auth_policy(Some("secret"), false, "127.0.0.1:18080").is_ok());
        assert!(validate_auth_policy(Some("secret"), false, "0.0.0.0:18080").is_ok());
    }

    #[test]
    fn auth_policy_non_loopback_no_token_fails_closed() {
        // 既有 B3 策略：非 loopback + 无 token 一律拒绝，--insecure-serve 不提供豁免口子
        let err = validate_auth_policy(None, false, "0.0.0.0:18080").unwrap_err();
        assert!(err.contains("非 loopback"));
        let err = validate_auth_policy(None, true, "192.168.1.10:18080").unwrap_err();
        assert!(err.contains("fail-closed"));
    }

    #[test]
    fn auth_policy_loopback_without_declaration_fails_fast() {
        // UV-116 核心行为：loopback + 无 token + 未显式声明 → 拒绝启动（旧实现隐式放行）
        let err = validate_auth_policy(None, false, "127.0.0.1:18080").unwrap_err();
        assert!(err.contains("--insecure-serve"));
        assert!(err.contains("三选一")); // 自诊断指引完整性
                                         // IPv6 回环同样适用
        let err = validate_auth_policy(None, false, "[::1]:18080").unwrap_err();
        assert!(err.contains("--insecure-serve"));
    }

    #[test]
    fn auth_policy_loopback_explicit_declaration_ok() {
        // loopback + 无 token + 显式声明 → 放行（显式降级，诚实语义）
        assert!(validate_auth_policy(None, true, "127.0.0.1:18080").is_ok());
        assert!(validate_auth_policy(None, true, "[::1]:18080").is_ok());
    }

    #[test]
    fn auth_policy_unparseable_addr_treated_as_non_loopback() {
        // 地址解析失败 → 安全侧失败（视为非 loopback）→ 拒绝
        let err = validate_auth_policy(None, true, "not-an-addr").unwrap_err();
        assert!(err.contains("fail-closed") || err.contains("非 loopback"));
    }

    // ============ /插件清单加载测试 ============

    fn write_manifest(dir: &TempDir, content: &str) -> PathBuf {
        let p = dir.path().join("plugin_manifest.json");
        std::fs::write(&p, content).unwrap();
        p
    }

    /// 从挂载结果中取指定插件的挂载决定
    fn mount_of<'a>(mounts: &'a [(&'static str, PluginMount)], id: &str) -> &'a PluginMount {
        &mounts
            .iter()
            .find(|(i, _)| *i == id)
            .unwrap_or_else(|| panic!("插件 {id} 应在挂载结果中"))
            .1
    }

    #[test]
    fn test_plugin_mount_none_defaults_all() {
        // 未配置清单 → 登记表全量插件全部启用(存量零迁移)
        let mounts = load_plugin_mounts(None).unwrap();
        assert_eq!(mounts.len(), PLUGIN_DEFS.len());
        assert!(mounts.iter().all(|(_, m)| m == &PluginMount::All));
    }

    #[test]
    fn test_plugin_mount_parse_variants() {
        let dir = TempDir::new().unwrap();
        // services 省略 = 全部启用
        let p = write_manifest(
            &dir,
            r#"{ "plugins": { "demo-services": { "enabled": true } } }"#,
        );
        let mounts = load_plugin_mounts(Some(&p)).unwrap();
        assert_eq!(mount_of(&mounts, "demo-services"), &PluginMount::All);
        // enabled=false → Off
        let p = write_manifest(
            &dir,
            r#"{ "plugins": { "demo-services": { "enabled": false } } }"#,
        );
        let mounts = load_plugin_mounts(Some(&p)).unwrap();
        assert_eq!(mount_of(&mounts, "demo-services"), &PluginMount::Off);
        // 子集
        let p = write_manifest(
            &dir,
            r#"{ "plugins": { "demo-services": { "enabled": true, "services": ["config_persist", "llm_advisor"] } } }"#,
        );
        let mounts = load_plugin_mounts(Some(&p)).unwrap();
        assert_eq!(
            mount_of(&mounts, "demo-services"),
            &PluginMount::Subset(vec![
                "config_persist".to_string(),
                "llm_advisor".to_string()
            ])
        );
    }

    #[test]
    fn test_plugin_mount_multi_plugin() {
        // 双插件:子集与停用并存,互不影响;未提及插件缺省全启(存量零迁移)
        let dir = TempDir::new().unwrap();
        let p = write_manifest(
            &dir,
            r#"{ "plugins": {
                "demo-services": { "enabled": true, "services": ["config_persist"] },
                "physics-services": { "enabled": false }
            } }"#,
        );
        let mounts = load_plugin_mounts(Some(&p)).unwrap();
        assert_eq!(
            mount_of(&mounts, "demo-services"),
            &PluginMount::Subset(vec!["config_persist".to_string()])
        );
        assert_eq!(mount_of(&mounts, "physics-services"), &PluginMount::Off);
        // 只提及 physics-services:demo-services 未提及 = 缺省全启
        let p = write_manifest(
            &dir,
            r#"{ "plugins": { "physics-services": { "enabled": true, "services": ["physics_simulate"] } } }"#,
        );
        let mounts = load_plugin_mounts(Some(&p)).unwrap();
        assert_eq!(
            mount_of(&mounts, "physics-services"),
            &PluginMount::Subset(vec!["physics_simulate".to_string()])
        );
    }

    #[test]
    fn test_plugin_mount_fail_fast() {
        let dir = TempDir::new().unwrap();
        // 未知插件 id → Err 含指引与全部可用 id
        let p = write_manifest(
            &dir,
            r#"{ "plugins": { "no-such-plugin": { "enabled": true } } }"#,
        );
        let err = load_plugin_mounts(Some(&p)).unwrap_err();
        assert!(
            err.contains("no-such-plugin")
                && err.contains("demo-services")
                && err.contains("physics-services"),
            "{err}"
        );
        // 空 services → Err 指引改用 enabled=false,并列出该插件合法服务名
        let p = write_manifest(
            &dir,
            r#"{ "plugins": { "physics-services": { "enabled": true, "services": [] } } }"#,
        );
        let err = load_plugin_mounts(Some(&p)).unwrap_err();
        assert!(
            err.contains("physics-services.services")
                && err.contains("physics_simulate")
                && err.contains("enabled"),
            "{err}"
        );
        // JSON 非法 → Err
        let p = write_manifest(&dir, "{ not-json");
        let err = load_plugin_mounts(Some(&p)).unwrap_err();
        assert!(err.contains("JSON 非法"), "{err}");
        // 文件不存在 → Err(显式报错,不静默降级)
        let err = load_plugin_mounts(Some(&dir.path().join("missing.json"))).unwrap_err();
        assert!(err.contains("读取插件清单失败"), "{err}");
    }

    // ============ FileConfig 反序列化测试 ============

    #[test]
    fn test_file_config_full() {
        let json = r#"{
            "server": {"addr": "0.0.0.0:9999", "max_rounds": 500},
            "auth": {"token": "mysecret"},
            "paths": {
                "core_eval": "/eval.json",
                "rules_dir": "/rules",
                "db_path": "/data.db",
                "memory_dir": "/mem",
                "wal_dir": "/wal"
            },
            "log": {
                "level": "debug",
                "format": "json",
                "file": "/var/log/evo.log",
                "max_days": 30,
                "max_size_mb": 512
            }
        }"#;
        let cfg: FileConfig = serde_json::from_str(json).expect("完整配置应解析成功");
        assert_eq!(cfg.server.addr.as_deref(), Some("0.0.0.0:9999"));
        assert_eq!(cfg.server.max_rounds, Some(500));
        assert_eq!(cfg.auth.token.as_deref(), Some("mysecret"));
        assert_eq!(
            cfg.paths.core_eval.as_deref(),
            Some(std::path::Path::new("/eval.json"))
        );
        assert_eq!(
            cfg.paths.rules_dir.as_deref(),
            Some(std::path::Path::new("/rules"))
        );
        assert_eq!(
            cfg.paths.db_path.as_deref(),
            Some(std::path::Path::new("/data.db"))
        );
        assert_eq!(
            cfg.paths.memory_dir.as_deref(),
            Some(std::path::Path::new("/mem"))
        );
        assert_eq!(
            cfg.paths.wal_dir.as_deref(),
            Some(std::path::Path::new("/wal"))
        );
        assert_eq!(cfg.log.level.as_deref(), Some("debug"));
        assert_eq!(cfg.log.format.as_deref(), Some("json"));
        assert_eq!(cfg.log.max_days, Some(30));
        assert_eq!(cfg.log.max_size_mb, Some(512));
    }

    #[test]
    fn test_file_config_empty_object() {
        let cfg: FileConfig = serde_json::from_str("{}").expect("空对象应解析为全默认");
        assert!(cfg.server.addr.is_none());
        assert!(cfg.server.max_rounds.is_none());
        assert!(cfg.auth.token.is_none());
        assert!(cfg.paths.core_eval.is_none());
        assert!(cfg.log.level.is_none());
    }

    #[test]
    fn test_file_config_partial() {
        let json = r#"{"server": {"addr": "127.0.0.1:8080"}}"#;
        let cfg: FileConfig = serde_json::from_str(json).expect("部分配置应解析成功");
        assert_eq!(cfg.server.addr.as_deref(), Some("127.0.0.1:8080"));
        assert!(cfg.server.max_rounds.is_none());
        assert!(cfg.auth.token.is_none());
    }

    #[test]
    fn test_file_config_unknown_fields_ignored() {
        let json = r#"{"server": {"addr": "x:1"}, "unknown_field": 42}"#;
        let cfg: FileConfig = serde_json::from_str(json).expect("未知字段应被忽略");
        assert_eq!(cfg.server.addr.as_deref(), Some("x:1"));
    }

    #[test]
    fn test_file_config_invalid_json() {
        let result: Result<FileConfig, _> = serde_json::from_str("not json");
        assert!(result.is_err(), "无效 JSON 应返回错误");
    }

    // ============ load_config_file 测试 ============

    #[test]
    fn test_load_config_file_none() {
        let cfg = load_config_file(&None);
        assert!(cfg.server.addr.is_none());
    }

    #[test]
    fn test_load_config_file_nonexistent() {
        let cfg = load_config_file(&Some(PathBuf::from("/nonexistent/path/config.json")));
        assert!(cfg.server.addr.is_none(), "不存在的文件应返回默认配置");
    }

    #[test]
    fn test_load_config_file_valid() {
        let dir = TempDir::new().expect("创建临时目录失败");
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"server": {"addr": "0.0.0.0:3000", "max_rounds": 200}}"#,
        )
        .expect("写入失败");
        let cfg = load_config_file(&Some(path));
        assert_eq!(cfg.server.addr.as_deref(), Some("0.0.0.0:3000"));
        assert_eq!(cfg.server.max_rounds, Some(200));
    }

    #[test]
    fn test_load_config_file_invalid_json() {
        let dir = TempDir::new().expect("创建临时目录失败");
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "not valid json").expect("写入失败");
        let cfg = load_config_file(&Some(path));
        assert!(cfg.server.addr.is_none(), "无效 JSON 应降级为默认配置");
    }

    // ============ ResolvedConfig::resolve 测试 ============

    /// 构造最小 CLI（仅二进制名，所有字段为默认）
    fn minimal_cli() -> Cli {
        Cli::parse_from(["evorule-server"])
    }

    #[test]
    fn test_resolve_defaults() {
        let cfg = ResolvedConfig::resolve(minimal_cli(), FileConfig::default());
        assert_eq!(cfg.addr, "0.0.0.0:18080");
        assert_eq!(cfg.core_eval, PathBuf::from("./resources/server_eval.json"));
        assert_eq!(cfg.rules_dir, PathBuf::from("./rules"));
        assert_eq!(cfg.db_path, PathBuf::from("./data/evorule.db"));
        assert_eq!(cfg.memory_dir, PathBuf::from("./data/memory"));
        assert_eq!(cfg.max_rounds, 1000);
        assert_eq!(cfg.log_level, "info");
        assert_eq!(cfg.log_format, "plain");
        assert_eq!(cfg.log_max_days, 7);
        assert_eq!(cfg.log_max_size_mb, 1024);
        assert!(!cfg.wal_fsync);
        assert_eq!(cfg.max_wal_size_bytes, 100 * 1024 * 1024);
        assert!(!cfg.auto_verify);
        assert_eq!(cfg.auto_verify_threshold, 1000);
        assert_eq!(cfg.auto_verify_interval, 1);
        assert_eq!(cfg.rate_limit_per_sec, 200, "默认限速应为 200 req/s(修正)");
    }

    #[test]
    fn test_resolve_cli_overrides_file() {
        let cli = Cli::parse_from([
            "evorule-server",
            "--addr",
            "0.0.0.0:9999",
            "--max-rounds",
            "42",
        ]);
        let file = FileConfig {
            server: FileServerConfig {
                addr: Some("0.0.0.0:1111".to_string()),
                max_rounds: Some(999),
                demo_auth: None,
            },
            ..Default::default()
        };
        let cfg = ResolvedConfig::resolve(cli, file);
        assert_eq!(cfg.addr, "0.0.0.0:9999", "CLI addr 应覆盖 file");
        assert_eq!(cfg.max_rounds, 42, "CLI max_rounds 应覆盖 file");
    }

    #[test]
    fn test_resolve_file_fills_when_cli_none() {
        let file = FileConfig {
            server: FileServerConfig {
                addr: Some("0.0.0.0:7777".to_string()),
                max_rounds: Some(300),
                demo_auth: None,
            },
            auth: FileAuthConfig {
                token: Some("filetoken".to_string()),
                service_token: None,
                allowed_origins: None,
            },
            ..Default::default()
        };
        let cfg = ResolvedConfig::resolve(minimal_cli(), file);
        assert_eq!(cfg.addr, "0.0.0.0:7777", "file 应填充 CLI 缺失的 addr");
        assert_eq!(cfg.max_rounds, 300);
        assert_eq!(cfg.auth_token.as_deref(), Some("filetoken"));
    }

    #[test]
    fn test_resolve_service_token() {
        // CLI > env > file；此处验证 CLI 与 file 两条来源
        let cli = Cli::parse_from([
            "evorule-server",
            "--auth-token",
            "usertoken",
            "--service-token",
            "svctoken",
        ]);
        let cfg = ResolvedConfig::resolve(cli, FileConfig::default());
        assert_eq!(cfg.auth_token.as_deref(), Some("usertoken"));
        assert_eq!(cfg.service_token.as_deref(), Some("svctoken"));

        let file = FileConfig {
            auth: FileAuthConfig {
                token: Some("fileuser".to_string()),
                service_token: Some("filesvc".to_string()),
                allowed_origins: None,
            },
            ..Default::default()
        };
        let cfg = ResolvedConfig::resolve(minimal_cli(), file);
        assert_eq!(cfg.service_token.as_deref(), Some("filesvc"));
    }

    #[test]
    fn test_resolve_no_rate_limit() {
        let cli = Cli::parse_from(["evorule-server", "--no-rate-limit"]);
        let cfg = ResolvedConfig::resolve(cli, FileConfig::default());
        assert_eq!(cfg.rate_limit_per_sec, 0, "--no-rate-limit 应设 per_sec=0");
    }

    #[test]
    fn test_resolve_wal_size_conversion() {
        let cli = Cli::parse_from(["evorule-server", "--wal-max-size-mb", "256"]);
        let cfg = ResolvedConfig::resolve(cli, FileConfig::default());
        assert_eq!(cfg.max_wal_size_bytes, 256 * 1024 * 1024);
    }

    #[test]
    fn test_resolve_wal_dir_from_file() {
        let file = FileConfig {
            paths: FilePathsConfig {
                wal_dir: Some(PathBuf::from("/data/wal")),
                ..Default::default()
            },
            ..Default::default()
        };
        let cfg = ResolvedConfig::resolve(minimal_cli(), file);
        assert_eq!(
            cfg.wal_dir.as_deref(),
            Some(std::path::Path::new("/data/wal"))
        );
    }

    #[test]
    fn test_resolve_log_file_from_file() {
        let file = FileConfig {
            log: FileLogConfig {
                file: Some(PathBuf::from("/var/log/evo.log")),
                max_days: Some(14),
                max_size_mb: Some(2048),
                ..Default::default()
            },
            ..Default::default()
        };
        let cfg = ResolvedConfig::resolve(minimal_cli(), file);
        assert_eq!(
            cfg.log_file.as_deref(),
            Some(std::path::Path::new("/var/log/evo.log"))
        );
        assert_eq!(cfg.log_max_days, 14);
        assert_eq!(cfg.log_max_size_mb, 2048);
    }

    // ============ serde_to_tcb 测试 ============

    #[test]
    fn test_serde_to_tcb_null() {
        assert_eq!(serde_to_tcb(serde_json::Value::Null), JsonValue::Null);
    }

    #[test]
    fn test_serde_to_tcb_bool() {
        assert_eq!(
            serde_to_tcb(serde_json::Value::Bool(true)),
            JsonValue::Bool(true)
        );
        assert_eq!(
            serde_to_tcb(serde_json::Value::Bool(false)),
            JsonValue::Bool(false)
        );
    }

    #[test]
    fn test_serde_to_tcb_integer() {
        assert_eq!(serde_to_tcb(serde_json::json!(42)), JsonValue::Integer(42));
        assert_eq!(serde_to_tcb(serde_json::json!(-7)), JsonValue::Integer(-7));
    }

    #[test]
    fn test_serde_to_tcb_string() {
        assert_eq!(
            serde_to_tcb(serde_json::json!("hello")),
            JsonValue::String("hello".into())
        );
    }

    #[test]
    fn test_serde_to_tcb_array() {
        let v = serde_json::json!([1, "two", true]);
        let result = serde_to_tcb(v);
        match result {
            JsonValue::Array(arr) => {
                assert_eq!(arr.len(), 3);
                assert_eq!(arr[0], JsonValue::Integer(1));
            }
            _ => panic!("应是 Array"),
        }
    }

    #[test]
    fn test_serde_to_tcb_object() {
        let v = serde_json::json!({"key": "value", "num": 10});
        let result = serde_to_tcb(v);
        match result {
            JsonValue::Object(map) => {
                assert_eq!(map.len(), 2);
                assert_eq!(map.get("key"), Some(&JsonValue::String("value".into())));
            }
            _ => panic!("应是 Object"),
        }
    }

    // ============ load_core_eval 测试 ============

    #[test]
    fn test_load_core_eval_valid() {
        let dir = TempDir::new().expect("创建临时目录失败");
        let path = dir.path().join("core_eval.json");
        std::fs::write(&path, r#"{"transform": [{"type": "noop"}]}"#).expect("写入失败");
        let result = load_core_eval(&path).expect("应加载成功");
        assert_eq!(result.len(), 1, "应有 1 个 transform");
    }

    #[test]
    fn test_load_core_eval_empty_transform() {
        let dir = TempDir::new().expect("创建临时目录失败");
        let path = dir.path().join("empty.json");
        std::fs::write(&path, r#"{"transform": []}"#).expect("写入失败");
        let err = load_core_eval(&path).expect_err("空 transform 应报错");
        assert!(err.contains("没有 transform 规则"));
    }

    #[test]
    fn test_load_core_eval_missing_transform_field() {
        let dir = TempDir::new().expect("创建临时目录失败");
        let path = dir.path().join("no_transform.json");
        std::fs::write(&path, r#"{"other": "field"}"#).expect("写入失败");
        let err = load_core_eval(&path).expect_err("缺少 transform 字段应报错");
        assert!(err.contains("没有 transform 规则"));
    }

    #[test]
    fn test_load_core_eval_nonexistent() {
        let err = load_core_eval(&PathBuf::from("/nonexistent/server_eval.json"))
            .expect_err("不存在的文件应报错");
        assert!(err.contains("读取宪法文件失败"));
    }

    // ============ ensure_dir 测试 ============

    #[test]
    fn test_ensure_dir_creates() {
        let dir = TempDir::new().expect("创建临时目录失败");
        let new_dir = dir.path().join("subdir/nested");
        ensure_dir(&new_dir).expect("应创建嵌套目录");
        assert!(new_dir.exists());
    }

    #[test]
    fn test_ensure_dir_existing() {
        let dir = TempDir::new().expect("创建临时目录失败");
        ensure_dir(&PathBuf::from(dir.path())).expect("已存在目录不应报错");
    }
}

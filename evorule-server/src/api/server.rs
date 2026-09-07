// SPDX-License-Identifier: AGPL-3.0-or-later

// Copyright (C) 2026 EvoRule Project

// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.

//! HTTP API 服务（axum）

//!

//! 提供外部访问接口，支持通过 HTTP 提交命令、查询状态、获取审计报告。

//!

//! # API 路由

//! - `POST /api/command` — 提交命令到反应器

//! - `POST /api/payload` — 更新 payload 字段

//! - `GET /api/state` — 获取当前状态快照

//! - `GET /api/audit` — 获取审计报告

//! - `POST /api/reload` — 手动触发配置热重载

//! - `GET /api/health` — 健康检查

use crate::api::audit_archive;
use crate::auth::{requires_service_identity, AuthConfig, CallerIdentity};
use crate::input_sanitizer::InputSanitizer;
use axum::http::Method;
use axum::Extension;

use evorule_demo_services::DemoServiceRouter;
use evorule_io_handlers::ServiceMeta;

use evorule_governance::auditor::Auditor;

use evorule_governance::metrics::SharedMetrics;

use evorule_governance::session;

use evorule_governance::shared_facts_log::SharedFactsLog;

use evorule_governance::{IoDispatcher, IoSubscriber};

use evorule_reactor::{Fact, FactId, FactSender, FactsLog, IoType};

use evorule_tcb::JsonValue;

use evorule_workspace::api::WorkspaceState;

use serde::{Deserialize, Serialize};

use utoipa::ToSchema;

use std::sync::atomic::AtomicBool;

use std::collections::HashSet;

use std::sync::Arc;

use tokio::sync::Mutex;

/// LLM 审计形态判定（K 约束族，2026-08-30）
///
/// `call_external` 且带 `messages`（LLM 消息历史）且无 `service_name`/`name`
/// —— 此类 IoRequest **不由 server 内置 IoSubscriber 自动应答**，
/// 留给外部执行者（evo-agent AuditedLlm / console-cloud 浏览器审计桥）：
/// 它们经 sidecar 会话把 prompt 全文入审计链、本地执行 LLM 后回写 io_response。
/// 若内置订阅者抢先错误应答，外部执行者的 io_response 会被反应器按
/// Unknown IoResponse 忽略，审计回路永远失败。
pub fn is_llm_audit_request(io_type: &IoType, params: &JsonValue) -> bool {
    io_type.as_str() == "call_external"
        && params.get("messages").is_some()
        && params.get("service_name").is_none()
        && params.get("name").is_none()
}

/// 消费方本地工具形态判定（2026-09-07）
///
/// `call_service` 且带 `tool_name` 且无 `service_name`/`name` —— 此类 IoRequest
/// **不由 server 内置 IoSubscriber 自动应答**，留给外部订阅者（evo-agent 等）
/// 本地执行工具后回写 io_response。
///
/// 背景：call_service 有两种互斥的参数形状——
/// - `service_name`/`name`：平台 HTTP 路由形态（ServiceRegistryHandler 执行）；
/// - `tool_name`：消费方本地工具形态（evo-agent 宪法 collect 生成，agent 本地
///   执行工具）。后者对内置订阅者而言是"未知服务"，若抢先应答
///   `missing required param: service_name` 错误 IoResponse，反应器即消费该
///   request，外部执行者随后回写的真实工具结果会被按 stale 拒绝
///   （"IoResponse for unknown/stale request_id"），工具循环断链。
///   与 `is_llm_audit_request` 防御的 call_external 形态完全同构。
pub fn is_agent_tool_request(io_type: &IoType, params: &JsonValue) -> bool {
    io_type.as_str() == "call_service"
        && params.get("tool_name").is_some()
        && params.get("service_name").is_none()
        && params.get("name").is_none()
}

/// 内置 IoSubscriber 的合并 skip 谓词：任一外部执行者形态命中即跳过自动应答
pub fn is_external_executor_request(io_type: &IoType, params: &JsonValue) -> bool {
    is_llm_audit_request(io_type, params) || is_agent_tool_request(io_type, params)
}

/// 就绪标志（优雅退出时设为 false，readiness 端点返回 503）
pub type ReadinessFlag = Arc<AtomicBool>;

/// Governance API 共享状态
///
///
/// 持有反应器的 command 通道发送端和 FactsLog 引用，
///
/// 供 axum handler 共享访问。
///
#[derive(Clone)]

pub struct GovernanceApi {
    /// command 通道发送端（提交 Fact 到反应器）
    command_tx: FactSender,

    /// FactsLog 克隆（读取状态和历史）
    facts_log: FactsLog,

    /// 审计器（`Arc<Mutex>` 保护，因为需要可变操作）
    auditor: Arc<Mutex<Auditor>>,

    /// ID 生成器偏移
    next_id: Arc<std::sync::atomic::AtomicU64>,
}

impl GovernanceApi {
    /// 创建新 API 状态
    pub fn new(command_tx: FactSender, facts_log: FactsLog, auditor: Auditor) -> Self {
        Self {
            command_tx,

            facts_log,

            auditor: Arc::new(Mutex::new(auditor)),

            next_id: Arc::new(std::sync::atomic::AtomicU64::new(20000)),
        }
    }

    /// 生成下一个 FactId
    fn next_id(&self) -> FactId {
        FactId(
            self.next_id
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        )
    }

    /// 提交命令到反应器
    pub fn send_command(&self, instruction: JsonValue) -> Result<FactId, String> {
        let id = self.next_id();

        self.command_tx
            .send(Fact::Command { id, instruction })
            .map_err(|_| "Command channel closed".to_string())?;

        Ok(id)
    }

    /// 提交 PayloadUpdate 到反应器
    pub fn send_payload_update(&self, path: String, value: JsonValue) -> Result<FactId, String> {
        let id = self.next_id();

        self.command_tx
            .send(Fact::PayloadUpdate { id, path, value })
            .map_err(|_| "Command channel closed".to_string())?;

        Ok(id)
    }

    /// 获取当前状态快照
    pub fn snapshot(&self) -> (JsonValue, Vec<JsonValue>, u64) {
        self.facts_log.snapshot()
    }

    /// 获取 FactsLog 引用
    pub fn facts_log(&self) -> &FactsLog {
        &self.facts_log
    }

    /// 审计新增事实
    pub async fn audit_new(&self) -> usize {
        let mut auditor = self.auditor.lock().await;

        auditor.audit_new()
    }

    /// 获取审计报告
    pub async fn audit_report(&self) -> Result<String, serde_json::Error> {
        let auditor = self.auditor.lock().await;

        auditor.report()
    }

    /// 获取审计条目数
    pub async fn audit_entry_count(&self) -> usize {
        let auditor = self.auditor.lock().await;

        auditor.entries().len()
    }

    /// 验证审计链完整性
    pub async fn audit_verify(&self) -> bool {
        let auditor = self.auditor.lock().await;

        auditor.verify()
    }

    /// 获取审计器引用（用于高级操作）
    pub fn auditor(&self) -> Arc<Mutex<Auditor>> {
        self.auditor.clone()
    }
}

/// 全局 SSE 连接数上限（防止连接耗尽）
const MAX_SSE_CONNECTIONS: u64 = 100;

/// SSE 心跳间隔（每 15s 发送 `: ping` 保持连接活跃）
const SSE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// SSE 连接最大空闲时长（10 分钟无事件自动关闭）
const SSE_MAX_IDLE: Duration = Duration::from_secs(600);

/// HTTP 请求体大小上限（1MB，防止超大请求体攻击）
const MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024;

/// HTTP 并发请求数上限（1000 并发，防止连接耗尽）
const MAX_CONCURRENCY: usize = 1000;

/// 会话管理 API 共享状态
///
///
/// 持有 `SessionManager`（`Arc<Mutex>` 保护），管理多个独立反应器实例。
///
/// 每个会话拥有独立的 state、FactsLog、command/event 通道，配合长驻模式持续服务。
///
#[derive(Clone)]

pub struct SessionApi {
    /// 会话管理器
    sessions: Arc<Mutex<session::SessionManager>>,

    /// API 层 FactId 计数器（从 30000 起，避免与反应器自身 ID 冲突）
    next_id: Arc<std::sync::atomic::AtomicU64>,

    /// 当前活跃 SSE 连接数（全局计数器，限制 MAX_SSE_CONNECTIONS）
    sse_connections: Arc<AtomicU64>,

    /// 已加载的核心规则（core_eval）

    /// reload 时会更新此处 + SessionManager 内部 core_eval
    core_eval: Arc<std::sync::RwLock<Arc<Vec<JsonValue>>>>,

    /// core_eval.json 路径（TCB 宪法路径，reload 时重新读取；起默认名 server_eval.json）
    core_eval_path: std::path::PathBuf,

    /// rules_dir 路径（业务规则目录，reload 时重扫描）
    rules_dir: std::path::PathBuf,

    /// knowledge 数据资产目录（与 rules_dir 物理隔离，`{knowledge_dir}/bundles/`）
    knowledge_dir: std::path::PathBuf,

    /// 模板市场目录
    marketplace_dir: std::path::PathBuf,

    /// 执行侧数据资产库（启动/导入时从 knowledge_dir 加载；W3 经
    /// `knowledge_store` 直读。数据条目不进 TCB，本库是执行侧唯一消费通道）
    knowledge_store: Arc<std::sync::RwLock<Arc<crate::knowledge_store::KnowledgeStore>>>,

    /// 启动加载数据资产库失败记录（fail-fast 口径：不静默掩盖——服务器仍可跑
    /// 规则，但数据面异常必须可见；经 `knowledge_load_error` 暴露，导入刷新成功即清除）
    knowledge_load_error: Arc<std::sync::RwLock<Option<String>>>,

    /// I/O 分发器（clone 给每个新 session 的 IoSubscriber，共享底层 handler）

    /// None 时 session 的 IoRequest 无人处理（纯计算场景）
    dispatcher: Option<IoDispatcher>,

    /// workspace 元数据库（bundle 导入审计溯源 bundle_imports 表）
    ///
    /// 仅用于写入/查询**管理元数据**（墙钟旁路），绝不参与 fact / 内容哈希 / 审计验证链。
    /// 未接线（None）时不记录 bundle 导入溯源（如单元测试环境）。
    workspace_db: Option<Arc<evorule_workspace::WorkspaceDb>>,

    /// 执行侧已绑定服务名集合（阻断项 ①：import_bundle 服务绑定核对）
    ///
    /// = 原生叶子能力（evorule-demo-services `NATIVE_SERVICES` 声明表）+ 注册表
    /// （service_registry.json）的并集。`import_bundle` 校验 bundle 声明的服务必须
    /// ⊆ 本集合，缺失则**显式失败**（不静默，防"治理侧声明、执行侧未绑定 →
    /// 运行时 unknown service_name"）。`with_bound_services` 按并集语义追加。
    bound_services: Arc<HashSet<String>>,

    /// 插件进程内服务能力清单（C5 能力对账的 native 来源）
    ///
    /// main.rs 从 PLUGIN_DEFS 声明表派生注入（含 plugin 归属/描述/敏感标记）。
    /// 默认 = demo-services 派生（二进制硬依赖的最小兜底，保持既有测试兼容），
    /// 生产路径由 `with_native_services` 覆盖为全插件清单。
    native_services: Arc<Vec<BoundServiceInfo>>,

    /// 插件服务回落链（插件路由 → registry HTTP 回落），`POST /api/services/{name}/invoke`
    /// 直调复用此链（与 io_request 同一实现，无第二执行路径）。
    /// None（未装配）时 invoke 返回 503。
    service_chain: Option<Arc<dyn evorule_reactor::IoHandler>>,

    /// 注册表（service_registry.json）显式绑定的服务元数据（C5/C6）
    ///
    /// - C5：`GET /api/services` 能力对账的来源 `registry` 条目（带 version/description）；
    /// - C6：声明 `sensitive=true` 的服务必须 ∈ 本集合（注册表显式绑定，含端点/凭据配置位），
    ///   仅原生内嵌不满足敏感服务要求 → import 显式失败（不静默）。
    ///
    /// 原生服务（`NATIVE_SERVICES` 声明表）由 `DemoServiceRouter` 恒在，不在此列表。
    registry_services: Arc<Vec<ServiceMeta>>,

    /// 审计档案：wal_dir 下历史会话 WAL 的只读重建缓存。
    /// 与活跃会话 API 物理隔离（独立 /api/audit-archive 前缀），全程无 WAL 写路径。
    archive_cache: Arc<std::sync::Mutex<audit_archive::ArchiveCache>>,

    /// 规则命中统计聚合器：消费各会话/单反应器的 TransitionTrace，
    /// 按 规则集版本×来源×下标 聚合；查询面 /api/rules/hit-stats 与 Prometheus 指标。
    hit_stats: Arc<crate::api::hit_stats::HitStatsAggregator>,
}

impl SessionApi {
    /// 创建会话管理 API
    ///
    ///
    /// # 参数
    ///
    /// - `core_eval`：transform 规则列表（用于创建每个会话的反应器）
    ///
    /// - `max_rounds`：每个反应器的最大指令执行步数
    ///
    pub fn new(core_eval: Vec<JsonValue>, max_rounds: usize) -> Self {
        Self::new_with_fsync(core_eval, max_rounds, false)
    }

    /// 创建会话管理 API（支持 fsync 配置，P02）
    ///
    ///
    /// # 参数
    ///
    /// - `core_eval`：transform 规则列表（用于创建每个会话的反应器）
    ///
    /// - `max_rounds`：每个反应器的最大指令执行步数
    ///
    /// - `wal_fsync`：是否在每次 WAL 写入后执行 fsync
    ///
    pub fn new_with_fsync(core_eval: Vec<JsonValue>, max_rounds: usize, wal_fsync: bool) -> Self {
        Self::new_with_wal_options(core_eval, max_rounds, None, wal_fsync, 100 * 1024 * 1024)
    }

    /// 创建会话管理 API（支持完整 WAL 配置，P03）
    ///
    ///
    /// # 参数
    ///
    /// - `core_eval`：transform 规则列表（用于创建每个会话的反应器）
    ///
    /// - `max_rounds`：每个反应器的最大指令执行步数
    ///
    /// - `wal_dir`：WAL 文件存储目录（为 None 时使用纯内存模式）
    ///
    /// - `wal_fsync`：是否在每次 WAL 写入后执行 fsync
    ///
    /// - `max_wal_size_bytes`：单个 WAL 文件最大大小（0 表示不轮换）
    ///
    pub fn new_with_wal_options(
        core_eval: Vec<JsonValue>,

        max_rounds: usize,

        wal_dir: Option<std::path::PathBuf>,

        wal_fsync: bool,

        max_wal_size_bytes: u64,
    ) -> Self {
        Self::new_with_full_config(
            core_eval,
            max_rounds,
            wal_dir,
            wal_fsync,
            max_wal_size_bytes,
            false,
            1000,
            1,
            std::path::PathBuf::from("./resources/server_eval.json"),
            std::path::PathBuf::from("./rules"),
        )
    }

    /// 创建会话管理 API（支持完整配置，P06）
    ///
    ///
    /// # 参数
    ///
    /// - `core_eval`：transform 规则列表（用于创建每个会话的反应器）
    ///
    /// - `max_rounds`：每个反应器的最大指令执行步数
    ///
    /// - `wal_dir`：WAL 文件存储目录（为 None 时使用纯内存模式）
    ///
    /// - `wal_fsync`：是否在每次 WAL 写入后执行 fsync
    ///
    /// - `max_wal_size_bytes`：单个 WAL 文件最大大小（0 表示不轮换）
    ///
    /// - `auto_verify`：是否启用审计链实时验证
    ///
    /// - `auto_verify_threshold`：自动验证阈值（0 表示不限制）
    ///
    /// - `auto_verify_interval`：自动验证间隔（1 表示每次都验证）
    ///
    /// - `core_eval_path`：TCB 宪法 core_eval.json 路径（reload 时重读取）
    ///
    /// - `rules_dir`：业务规则目录（reload 时重扫描）
    ///
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_full_config(
        core_eval: Vec<JsonValue>,

        max_rounds: usize,

        wal_dir: Option<std::path::PathBuf>,

        wal_fsync: bool,

        max_wal_size_bytes: u64,

        auto_verify: bool,

        auto_verify_threshold: usize,

        auto_verify_interval: usize,

        core_eval_path: std::path::PathBuf,

        rules_dir: std::path::PathBuf,
    ) -> Self {
        let ce_cloned = core_eval.clone();

        // 初始 layout（来源标签暂全记 "core_eval"，生产路径由 main.rs
        // 经 `with_hit_stats` 注入含 rules_dir 文件级溯源的权威 layout；reload 后自动换版）
        let initial_layout = crate::api::hit_stats::RulesetLayout::from_rules(
            &ce_cloned,
            vec!["core_eval".to_string(); ce_cloned.len()],
        );

        let sessions = Arc::new(Mutex::new(
            session::SessionManager::with_limits_and_wal_and_auto_verify(
                core_eval,
                max_rounds,
                session::DEFAULT_MAX_SESSIONS,
                session::DEFAULT_SESSION_TTL,
                wal_dir.clone(),
                session::DEFAULT_SHARD_COUNT,
                wal_fsync,
                max_wal_size_bytes,
                auto_verify,
                auto_verify_threshold,
                auto_verify_interval,
            ),
        ));

        // Q12 W2：knowledge 数据资产目录与 rules_dir 物理隔离（`{rules 父目录}/knowledge`），
        // 启动即加载数据资产库。目录不存在 → 空库（执行侧可只跑规则不承载数据资产）；
        // 加载失败不静默：记录错误并经 `knowledge_load_error` 显式暴露（服务器仍可跑
        // 规则——数据资产完整性问题不应阻断规则执行，但绝不允许不可见）。
        let knowledge_dir = rules_dir
            .parent()
            .map(|p| p.join("knowledge"))
            .unwrap_or_else(|| std::path::PathBuf::from("knowledge"));

        // W4：模板市场目录与 rules_dir 物理隔离（同 knowledge 派生法：
        // `{rules 父目录}/marketplace`）。TCB 扫描 rules_dir，用户上传内容
        // 绝不可入规则加载路径。目录懒创建（首次上传时建）。
        let marketplace_dir = rules_dir
            .parent()
            .map(|p| p.join("marketplace"))
            .unwrap_or_else(|| std::path::PathBuf::from("marketplace"));
        let (knowledge_store, knowledge_load_error) =
            match crate::knowledge_store::KnowledgeStore::load_from_disk(&knowledge_dir) {
                Ok(ks) => (ks, None),
                Err(e) => {
                    tracing::error!("knowledge 数据资产库加载失败（不静默，数据面不可用）: {e}");
                    (crate::knowledge_store::KnowledgeStore::default(), Some(e))
                }
            };

        Self {
            sessions: sessions.clone(),

            next_id: Arc::new(std::sync::atomic::AtomicU64::new(30000)),

            sse_connections: Arc::new(AtomicU64::new(0)),

            core_eval: Arc::new(std::sync::RwLock::new(Arc::new(ce_cloned))),

            core_eval_path,

            rules_dir,

            knowledge_dir,

            marketplace_dir,

            knowledge_store: Arc::new(std::sync::RwLock::new(Arc::new(knowledge_store))),

            knowledge_load_error: Arc::new(std::sync::RwLock::new(knowledge_load_error)),

            dispatcher: None,

            workspace_db: None,

            // 默认绑定 = 原生叶子能力（Phase 1 demo-services 是二进制硬依赖，始终可路由）
            // 默认 native 清单 = demo 派生兜底（生产路径 main.rs 经 with_native_services 覆盖为全插件）
            native_services: Arc::new(
                DemoServiceRouter::native_service_names()
                    .iter()
                    .map(|name| BoundServiceInfo {
                        name: (*name).to_string(),
                        source: "native".to_string(),
                        version: Some("1.0.0".to_string()),
                        description: None,
                        plugin: Some("demo-services".to_string()),
                        sensitive: false,
                        parameters: None,
                    })
                    .collect(),
            ),

            service_chain: None,

            bound_services: Arc::new(
                DemoServiceRouter::native_service_names()
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            ),
            // 注册表显式绑定元数据：默认空（C5/C6；由 with_registry_services 注入）
            registry_services: Arc::new(Vec::new()),

            // ：审计档案只读缓存（wal_dir 透传；None=纯内存模式无档案）
            archive_cache: Arc::new(std::sync::Mutex::new(audit_archive::ArchiveCache::new(
                wal_dir,
            ))),

            // 命中统计聚合器（初始 layout 见构造器开头）
            hit_stats: Arc::new(crate::api::hit_stats::HitStatsAggregator::new(
                initial_layout,
            )),
        }
    }

    /// 注入规则命中统计聚合器（builder 模式）
    ///
    /// 生产路径注入注册了 Prometheus registry 的聚合器（main.rs 构造），
    /// 并注入含 rules_dir 文件级溯源的权威初始 layout。
    pub fn with_hit_stats(
        mut self,
        hit_stats: Arc<crate::api::hit_stats::HitStatsAggregator>,
    ) -> Self {
        self.hit_stats = hit_stats;
        self
    }

    /// 为指定会话 spawn hit-stats 事件记录任务
    ///
    /// 订阅会话 reactor 的 event 通道，消费 TransitionTrace 归因事实进聚合器。
    /// 会话结束（通道关闭）任务自动退出。
    fn spawn_hit_stats_recorder(&self, session_id: u64) {
        let agg = self.hit_stats.clone();
        let sessions = self.sessions.clone();
        tokio::spawn(async move {
            let rx = {
                let sessions = sessions.lock().await;
                sessions
                    .get_session(session_id)
                    .map(|s| s.event_tx.subscribe())
            };
            if let Some(rx) = rx {
                crate::api::hit_stats::run_recorder(rx, (*agg).clone()).await;
            }
        });
    }

    /// 注入 I/O 分发器（builder 模式）
    ///
    ///
    /// 注入后，每个新创建的 session 会自动 spawn 一个 IoSubscriber，
    ///
    /// 将 session reactor 的 IoRequest 分发到注册的 handler。
    ///
    pub fn with_dispatcher(mut self, dispatcher: IoDispatcher) -> Self {
        self.dispatcher = Some(dispatcher);

        self
    }

    /// 追加执行侧已绑定服务（builder 模式，T6 服务绑定核对）
    ///
    /// **并集语义**：原生叶子能力（`NATIVE_SERVICE_NAMES`）始终在集合内，
    /// 此处追加注册表（service_registry.json）/额外绑定，避免调用方重复枚举原生服务。
    pub fn with_bound_services<I: IntoIterator<Item = String>>(mut self, names: I) -> Self {
        let mut set = (*self.bound_services).clone();
        set.extend(names);
        self.bound_services = Arc::new(set);

        self
    }

    /// 注入注册表显式绑定的服务元数据（builder 模式，C5/C6）
    ///
    /// 来自 service_registry.json 的条目（含 version/description）。`bound_services`
    /// 的并集追加调用方自行处理；此处仅记录注册表条目（C6 敏感核对 + C5 能力对账）。
    pub fn with_registry_services<I: IntoIterator<Item = ServiceMeta>>(mut self, metas: I) -> Self {
        let mut v = (*self.registry_services).clone();
        v.extend(metas);
        v.sort_by(|a, b| a.name.cmp(&b.name));
        self.registry_services = Arc::new(v);
        self
    }

    /// 注入插件进程内服务能力清单（builder 模式，插件对账泛化）
    ///
    /// main.rs 从 PLUGIN_DEFS 声明表派生（含 plugin 归属/描述/敏感标记）。
    /// **替换语义**：覆盖默认 demo 兜底清单；同时 `bound_services` 并集追加
    /// 全部 native 名（import_bundle 敏感服务核对随生产清单泛化）。
    pub fn with_native_services(mut self, infos: Vec<BoundServiceInfo>) -> Self {
        let mut set = (*self.bound_services).clone();
        set.extend(infos.iter().map(|i| i.name.clone()));
        self.bound_services = Arc::new(set);
        self.native_services = Arc::new(infos);
        self
    }

    /// 注入插件服务回落链（builder 模式，invoke 直调用）
    ///
    /// 与 io_request 同一处理器链（插件路由 → registry HTTP 回落），
    /// 保证直调与会话调用无第二执行路径。
    pub fn with_service_chain(mut self, chain: Arc<dyn evorule_reactor::IoHandler>) -> Self {
        self.service_chain = Some(chain);
        self
    }

    /// 注入 workspace 元数据库（builder 模式, T5 bundle 导入溯源）
    ///
    /// 仅用于管理元数据旁路（bundle_imports 表），不参与 fact / 哈希 / 审计链。
    pub fn with_workspace_db(mut self, workspace_db: Arc<evorule_workspace::WorkspaceDb>) -> Self {
        self.workspace_db = Some(workspace_db);

        self
    }

    /// 获取已加载的核心规则（core_eval）只读引用
    ///
    ///
    /// 供 Portal API 查询当前加载的 transform 规则列表。
    ///
    /// reload 后此方法返回新的规则。
    ///
    pub fn core_eval(&self) -> Arc<Vec<JsonValue>> {
        let guard = match self.core_eval.read() {
            Ok(g) => g,

            Err(poisoned) => poisoned.into_inner(),
        };

        guard.clone()
    }

    /// 返回当前已加载 transform 规则数量
    pub fn core_eval_len(&self) -> usize {
        self.core_eval().len()
    }

    // ========================================================================
    // Q12 W2/W3：执行侧数据资产通道（物理隔离，不进 TCB）
    // ========================================================================

    /// Q12 W3：执行侧数据资产库只读快照（原生服务消费接口）
    ///
    /// IoHandler 侧原生服务（如 RPSM）按 `dataset_id`/`entry_id` 直读 payload，
    /// 进程内零开销；MVP 不做网络数据面（HTTP 查询端点属段 2）。
    pub fn knowledge_store(&self) -> Arc<crate::knowledge_store::KnowledgeStore> {
        match self.knowledge_store.read() {
            Ok(g) => g.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// knowledge 数据资产目录路径（与 rules_dir 物理隔离）
    pub fn knowledge_dir(&self) -> &std::path::Path {
        &self.knowledge_dir
    }

    /// 启动加载数据资产库失败记录（None = 正常；Some = 数据面不可用，需运维自愈）
    pub fn knowledge_load_error(&self) -> Option<String> {
        match self.knowledge_load_error.read() {
            Ok(g) => g.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// 重新从磁盘加载数据资产库（导入/运维刷新通道）。
    ///
    /// - 加载失败 → Err（fail-fast，不静默：落盘已发生但内存索引未更新，
    ///   调用方必须知悉数据面与磁盘不一致）；
    /// - 加载成功 → 替换内存索引并清除历史启动错误记录。
    fn refresh_knowledge_store(&self) -> Result<(), String> {
        let ks = crate::knowledge_store::KnowledgeStore::load_from_disk(&self.knowledge_dir)?;
        match self.knowledge_store.write() {
            Ok(mut guard) => *guard = Arc::new(ks),
            Err(poisoned) => *poisoned.into_inner() = Arc::new(ks),
        }
        match self.knowledge_load_error.write() {
            Ok(mut guard) => *guard = None,
            Err(poisoned) => *poisoned.into_inner() = None,
        }
        Ok(())
    }

    /// 生成下一个 FactId
    fn next_id(&self) -> FactId {
        FactId(
            self.next_id
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        )
    }

    /// 尝试获取一个 SSE 连接配额
    ///
    ///
    /// 成功返回 `SseConnectionGuard`，连接关闭时自动释放配额。
    ///
    /// 超过 `MAX_SSE_CONNECTIONS` 上限返回 `None`。
    ///
    fn try_acquire_sse(&self) -> Option<SseConnectionGuard> {
        let current = self.sse_connections.load(Ordering::SeqCst);

        if current >= MAX_SSE_CONNECTIONS {
            tracing::warn!(
                current,
                max = MAX_SSE_CONNECTIONS,
                "SSE 连接数已达上限，拒绝新连接"
            );

            return None;
        }

        let new_val = self.sse_connections.fetch_add(1, Ordering::SeqCst);

        if new_val >= MAX_SSE_CONNECTIONS {
            // 并发竞争回退

            self.sse_connections.fetch_sub(1, Ordering::SeqCst);

            tracing::warn!(
                current = new_val,
                max = MAX_SSE_CONNECTIONS,
                "SSE 连接数已达上限（并发竞争回退）"
            );

            return None;
        }

        tracing::debug!(
            active = new_val + 1,
            max = MAX_SSE_CONNECTIONS,
            "SSE 连接已建立"
        );

        Some(SseConnectionGuard {
            counter: self.sse_connections.clone(),
        })
    }

    /// 返回当前活跃 SSE 连接数（用于监控/测试）
    pub fn sse_connection_count(&self) -> u64 {
        self.sse_connections.load(Ordering::SeqCst)
    }

    /// 返回当前活跃会话数（语义修正）
    ///
    ///
    /// 与 `sse_connection_count` 的区别：
    ///
    /// - `sse_connection_count`：SSE 连接数（一个会话可能无 SSE 或多 SSE）
    ///
    /// - `active_session_count`：真实活跃会话数（SessionManager 内部 atomic 计数）
    ///
    ///
    ///
    /// Portal summary 应使用此方法而非 SSE 连接数。
    ///
    pub async fn active_session_count(&self) -> u64 {
        let mgr = self.sessions.lock().await;

        mgr.len() as u64
    }

    /// 启动后台 reaper 任务，定期清理过期和已结束的会话
    ///
    ///
    /// 应在服务器启动时调用一次。清理间隔为 5 分钟。
    ///
    /// ①: 必须在 `with_workspace_db` **之后**调用——生产会话保护与
    /// 自愈重建依赖 workspace 元数据接线(main.rs 已调整启动时序)。
    ///
    pub fn start_reaper(&self) {
        let sessions = self.sessions.clone();

        let workspace_db = self.workspace_db.clone();

        // ①: 自愈重建句柄(仅 workspace_db 接线时启用;单测/无元数据
        // 环境退化为原始 reap 行为,无保护无自愈)
        let recovery_api = if workspace_db.is_some() {
            Some(self.clone())
        } else {
            None
        };

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(evorule_governance::session::REAPER_INTERVAL);

            interval.tick().await; // 跳过第一次立即触发

            loop {
                interval.tick().await;

                let (finished, expired) =
                    reap_once(&sessions, workspace_db.as_ref(), recovery_api.as_ref()).await;

                let reaped = finished + expired;
                if reaped > 0 {
                    tracing::info!(
                        reaped_count = reaped,
                        "Background reaper cleaned up expired/finished sessions"
                    );
                }
            }
        });
    }

    /// 重新从磁盘加载 TCB 宪法（core_eval.json）+ 业务规则（rules_dir）。
    ///
    ///
    /// 原子替换内部 `core_eval` 缓存和 SessionManager 内部用于新会话的 core_eval。
    ///
    /// 已存在会话的反应器**不会**中途换规则（保证 TCB 不可变语义）。
    ///
    ///
    ///
    /// # 返回
    ///
    /// - `Ok((old_len, new_len))`：旧规则数和新规则数
    ///
    /// - `Err(String)`：读取/解析失败（失败时旧规则保持不变）
    ///
    pub async fn reload_from_disk(&self) -> Result<(usize, usize), String> {
        let (new_transforms, new_layout) =
            Self::load_merged_with_layout(&self.core_eval_path, &self.rules_dir)?;

        let new_len = new_transforms.len();

        // 1. 替换 SessionManager 内部 core_eval（新会话使用新规则）

        let old_len_mgr;

        {
            let mut mgr = self.sessions.lock().await;

            let old = mgr.replace_core_eval(new_transforms.clone());

            old_len_mgr = old.len();
        }

        // 2. 替换 SessionApi 自己的缓存

        let old_len_cache;

        {
            let mut w = match self.core_eval.write() {
                Ok(g) => g,

                Err(poisoned) => poisoned.into_inner(),
            };

            old_len_cache = w.len();

            *w = Arc::new(new_transforms);
        }

        // 3. hit-stats 聚合器换版（新计数进新版本桶，旧版本切片保留）
        self.hit_stats.adopt_layout(new_layout);

        tracing::info!(
            old_len = old_len_cache,
            new_len,
            "session rules reloaded from disk"
        );

        Ok((old_len_mgr.max(old_len_cache), new_len))
    }

    /// 从文件系统合并加载：TCB 宪法 core_eval.json（在前）+ rules_dir/*.json（在后，按文件名字典序）
    pub fn load_merged_transforms_from_fs(
        core_eval_path: &std::path::Path,

        rules_dir: &std::path::Path,
    ) -> Result<Vec<JsonValue>, String> {
        Self::load_merged_with_layout(core_eval_path, rules_dir).map(|(rules, _)| rules)
    }

    /// 合并加载并产出规则集 layout（单一权威装载点）
    ///
    /// 与 [`Self::load_merged_transforms_from_fs`] 同一次读取产出两件事：
    /// 合并规则列表（引擎输入）+ [`RulesetLayout`]（下标→来源/指令类型解析 +
    /// 规则集版本哈希）。避免双份装载逻辑漂移——hit-stats 的下标解析正确性
    /// 依赖"layout 与引擎合并顺序一致"这一不变式。
    pub fn load_merged_with_layout(
        core_eval_path: &std::path::Path,

        rules_dir: &std::path::Path,
    ) -> Result<(Vec<JsonValue>, crate::api::hit_stats::RulesetLayout), String> {
        let mut rules = Self::load_core_eval_transforms(core_eval_path)?;

        let mut sources = vec!["core_eval".to_string(); rules.len()];

        let (extra, extra_sources) = Self::load_rules_dir_with_sources(rules_dir);

        rules.extend(extra);

        sources.extend(extra_sources);

        let layout = crate::api::hit_stats::RulesetLayout::from_rules(&rules, sources);

        Ok((rules, layout))
    }

    /// 加载 TCB 宪法 core_eval.json 的 transform 数组。
    ///
    ///
    /// 要求文件存在、可解析、`transform` 字段非空，任一不满足返回 Err。
    ///
    fn load_core_eval_transforms(
        core_eval_path: &std::path::Path,
    ) -> Result<Vec<JsonValue>, String> {
        let tcb_raw = match std::fs::read_to_string(core_eval_path) {
            Ok(s) => s,
            Err(e) => {
                // 兼容检测（拒绝静默回退）:v0.4.1 起 server 份宪法业务规则集由
                // core_eval.json 更名为 server_eval.json。检测到"新名缺失但旧名存在"时,
                // 显式给出迁移指引而非自动回退读旧名——遵循"系统自愈 + 用户可见"原则。
                if e.kind() == std::io::ErrorKind::NotFound {
                    let legacy = core_eval_path.with_file_name("core_eval.json");
                    if core_eval_path.file_name().and_then(|n| n.to_str())
                        == Some("server_eval.json")
                        && legacy.exists()
                    {
                        return Err(format!(
                            "宪法文件 {} 不存在,但同目录检测到旧名 {} — v0.4.1 起 server 份宪法\
                             业务规则集已更名为 server_eval.json(,与 evorule 仓宪法原则区分)。\
                             迁移指引: ①将旧文件重命名为 server_eval.json;或 ②以 --core-eval / \
                             EVORULE_CORE_EVAL / 配置文件 paths.core_eval 显式指定旧路径",
                            core_eval_path.display(),
                            legacy.display()
                        ));
                    }
                }
                return Err(format!(
                    "读取宪法文件失败 {}: {}",
                    core_eval_path.display(),
                    e
                ));
            }
        };

        let tcb_json: serde_json::Value =
            serde_json::from_str(&tcb_raw).map_err(|e| format!("解析宪法文件失败: {}", e))?;

        let tcb: Vec<JsonValue> = tcb_json
            .get("transform")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().cloned().map(serde_to_tcb).collect())
            .ok_or_else(|| {
                format!(
                    "宪法文件 {} 没有 transform 数组字段",
                    core_eval_path.display()
                )
            })?;

        if tcb.is_empty() {
            return Err(format!(
                "宪法文件 {} 的 transform 数组为空",
                core_eval_path.display()
            ));
        }

        // 修复(2026-09-01): call_external 指令规则是 LLM 审计桥的平台契约。
        // 会话反应器的 IoRequest 完全由宪法规则驱动;若宪法缺该规则,LLM 审计桥命令会被
        // all([]) 兜底规则静默 no-op(无 IoRequest、无 Error 事实),违反"拒绝静默通过"原则。
        // 因此启动期 fail-fast 校验,并给出可自诊断的修复指引。
        let has_call_external = tcb.iter().any(|r| {
            r.get("params")
                .and_then(|p| p.get("domain"))
                .and_then(|d| {
                    d.get("type").and_then(|t| t.as_str()).map(|t| {
                        t == "instruction"
                            && d.get("instruction_type")
                                .and_then(|it| it.as_str())
                                .is_some_and(|it| it == "call_external")
                    })
                })
                .unwrap_or(false)
        });
        if !has_call_external {
            return Err(format!(
                "宪法文件 {} 缺少 call_external 指令规则 — LLM 审计桥将静默失效,拒绝启动。\
                 自诊断指引: ①检查该文件 transform 数组中是否存在 params.domain.instruction_type == \"call_external\" 的规则;\
                 ②v0.4.0 最小评估集不含该规则(ReAct 剧本迁出决策),需升级至 v0.4.1+ 或从源仓权威宪法同步;\
                 ③若为自定义宪法,请补入该规则或改用 --core-eval 指向完整宪法",
                core_eval_path.display()
            ));
        }

        Ok(tcb)
    }

    /// 扫描业务规则目录（递归，含 `rules/bundles/{bundle_id}/` 子目录，T3），按完整路径
    /// 字典序加载所有 *.json 的 transform 数组（确定性加载顺序；排除 `bundle_manifest.json`）。
    ///
    ///
    /// 目录不存在或读取失败时返回空 Vec（不报错）；单个文件解析失败时
    ///
    /// warn 日志并跳过该文件（fail-soft，保证热重载可用性）。
    ///
    /// 带 per-rule 来源标签装载 rules_dir
    ///
    /// 来源标签 = 相对 rules_dir 的文件路径（`/` 归一化），与合并列表等长。
    /// 目录不存在返回空（纯宪法规则集）。
    fn load_rules_dir_with_sources(
        rules_dir: &std::path::Path,
    ) -> (Vec<JsonValue>, Vec<String>) {
        if !rules_dir.exists() {
            return (Vec::new(), Vec::new());
        }

        let mut paths: Vec<std::path::PathBuf> = Vec::new();

        Self::collect_json_files_recursive(rules_dir, &mut paths);

        // 完整路径字典序 = 确定性（bundles/ 子目录条目按路径自然归位）
        paths.sort();

        let mut out: Vec<JsonValue> = Vec::new();

        let mut sources: Vec<String> = Vec::new();

        for p in paths {
            if let Some(extra) = Self::parse_rule_file(&p) {
                // 来源标签 = 相对 rules_dir 的路径（/ 归一化，URL/标签安全）
                let rel = p
                    .strip_prefix(rules_dir)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .replace('\\', "/");
                sources.extend(std::iter::repeat(rel).take(extra.len()));
                out.extend(extra);
            }
        }

        (out, sources)
    }

    /// 递归收集规则 .json 文件路径：跳过子目录的 manifest，其余按目录展开
    fn collect_json_files_recursive(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(read_dir) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in read_dir.flatten() {
            let p = entry.path();
            if p.is_dir() {
                // 跳过隐藏目录（`.tmp/.bak/.stale` 等临时/备份目录不参与加载）
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.starts_with('.') {
                    continue;
                }
                Self::collect_json_files_recursive(&p, out);
            } else if p.extension().map(|e| e == "json").unwrap_or(false)
                && p.file_name()
                    .map(|n| n != crate::api::bundles::BUNDLE_MANIFEST_FILE)
                    .unwrap_or(true)
            {
                out.push(p);
            }
        }
    }

    /// 读取并解析单个业务规则文件，返回其 transform 数组。
    ///
    ///
    /// 文件读取/解析失败或格式不符时 warn 并返回 None（fail-soft，保证热重载可用性）。
    ///
    fn parse_rule_file(p: &std::path::Path) -> Option<Vec<JsonValue>> {
        let json = Self::load_rule_doc(p)?;
        let arr = Self::extract_transform_array(p, &json)?;
        if !Self::passes_schema_gate(p, &arr) {
            return None;
        }
        Some(arr.into_iter().map(serde_to_tcb).collect())
    }

    /// 读取规则文件并解析为 JSON；失败时 warn 并返回 None
    fn load_rule_doc(p: &std::path::Path) -> Option<serde_json::Value> {
        let raw = match std::fs::read_to_string(p) {
            Ok(s) => s,

            Err(e) => {
                tracing::warn!("读取业务规则文件 {} 失败: {}，已跳过", p.display(), e);

                return None;
            }
        };

        match serde_json::from_str(&raw) {
            Ok(v) => Some(v),

            Err(e) => {
                tracing::warn!("解析业务规则文件 {} 失败: {}，已跳过", p.display(), e);

                None
            }
        }
    }

    /// 从规则文档中提取 transform 数组（兼容 {transform:[]} 与裸 transform 数组两种形态）
    fn extract_transform_array(
        p: &std::path::Path,
        json: &serde_json::Value,
    ) -> Option<Vec<serde_json::Value>> {
        if let Some(arr) = json.get("transform").and_then(|x| x.as_array()) {
            return Some(arr.to_vec());
        }
        if let Some(arr) = json.as_array() {
            return Some(arr.to_vec());
        }
        tracing::warn!(
            "业务规则文件 {} 既不是 {{transform:[]}} 也不是 transform 数组，已跳过",
            p.display()
        );
        None
    }

    /// Schema 门禁（线1 防御层, records/77）：校验引擎原生结构，非法时 warn 并拒绝加载
    fn passes_schema_gate(p: &std::path::Path, arr: &[serde_json::Value]) -> bool {
        // P2-04/P2-05：启动/热重载加载业务规则时即校验引擎原生结构（元指令白名单、必填参数、
        // 域结构等），拦截含不支持元指令或结构非法的规则并 warn 跳过——避免"启动照常、运行时才
        // 崩溃"（TCB 只保证确定性执行，不保证用户规则正确性；防御在 server 层）。
        let schema_report =
            evorule_rule_schema::validate_transform_list(&serde_json::Value::Array(arr.to_vec()));
        if !schema_report.valid {
            let detail = schema_report.errors.join("; ");
            tracing::warn!(
                "业务规则文件 {} 未通过 Schema 门禁（引擎原生结构非法，参见固化 rule_set v1.0 Schema），已跳过: {}",
                p.display(),
                detail
            );
            return false;
        }
        true
    }

    /// ①.5 测试证据引用校验。
    /// 与治理域 export 侧形状校验(evorule-rule export_with_tests)双闸同口径:
    ///   a. verdict=pass 必须携带可追溯标记(subset 非空且每项 sandbox:<id> 或
    ///      human:<actor>)——封死"零证据 pass"直 POST import 的伪造路径;
    ///   b. sandbox:<id> 引用必须在本机 workspace 元数据可追溯:
    ///      不存在(伪造/跨环境) → 拒收;非 closed(测试未完成) → 拒收;
    ///      报告 summary.failed≠0(fail 报告不得作 pass 证据) → 拒收;
    ///      报告文件缺失/损坏 → 拒收(fail-closed:校验层缺位即不通过,不静默)。
    /// human:<actor> 无需存在性校验(显式降级声明,人无表可查)。
    /// 跨环境信任(报告哈希/随包携带)登记为后续项——当前拒收符合
    /// "不让未经验证的信息通过"(40 号 §6.1 阶段一)。
    ///
    fn validate_test_evidence(&self, bundle: &evorule_bundle::DatasetBundle) -> Result<(), String> {
        if bundle.tests.verdict != evorule_bundle::TestVerdict::Pass {
            return Ok(()); // 非 pass 无要求
        }
        let traceable = !bundle.tests.subset.is_empty()
            && bundle
                .tests
                .subset
                .iter()
                .all(|s| s.starts_with("sandbox:") || s.starts_with("human:"));
        if !traceable {
            return Err(
                "测试证据校验失败（不静默）: verdict=pass 的导入必须携带可追溯标记\
                 (tests.subset 每项须为 sandbox:<沙盒ID> 或 human:<操作者>)。\
                 请从治理域测试工作台导出(机器背书)或显式人工背书"
                    .to_string(),
            );
        }
        for ref_item in &bundle.tests.subset {
            let Some(sid_str) = ref_item.strip_prefix("sandbox:") else {
                continue; // human: 标记无需存在性校验
            };
            let sid: i64 = sid_str.parse().map_err(|_| {
                format!("测试证据校验失败: sandbox 引用格式非法({ref_item}),须为 sandbox:<数字ID>")
            })?;
            let ws_db = self.workspace_db.as_ref().ok_or_else(|| {
                format!(
                    "测试证据校验失败: 引用了沙盒报告({ref_item})但 workspace 元数据未接线,\
                     无法验证引用(fail-closed 不放行)"
                )
            })?;
            let sb = ws_db
                .get_sandbox_session(sid)
                .map_err(|e| format!("测试证据校验失败: 查询沙盒会话 {sid} 出错: {e}"))?
                .ok_or_else(|| {
                    format!(
                        "测试证据校验失败（不静默）: 沙盒引用 sandbox:{sid} 在本机不存在\
                         (引用伪造或跨环境导入)。本机执行的规则集请从本机测试工作台导出;\
                         跨环境信任需报告随包携带(后续项)"
                    )
                })?;
            if sb.status != evorule_workspace::SandboxStatus::Closed {
                return Err(format!(
                    "测试证据校验失败: 沙盒 #{sid} 状态为 {:?}(非 closed,测试未完成),\
                     不得作为 pass 证据",
                    sb.status
                ));
            }
            // 报告一致性: 读关闭时落盘的 TestReport(与 generate_test_report
            // 关闭态同口径推导 report_path)
            let export_path = sb.export_path.as_deref().ok_or_else(|| {
                format!(
                    "测试证据校验失败: 沙盒 #{sid} 关闭但无报告导出路径(数据异常),\
                     请重跑沙盒测试"
                )
            })?;
            let file_name = export_path.rsplit('/').next().unwrap_or_default();
            let report_path = format!(
                "{}/report_{}",
                evorule_workspace::SANDBOX_REPORT_DIR,
                file_name
            );
            let content = std::fs::read_to_string(&report_path).map_err(|_| {
                format!(
                    "测试证据校验失败: 沙盒 #{sid} 报告文件缺失({report_path};\
                     可能被清理),请重跑沙盒测试"
                )
            })?;
            let report: serde_json::Value = serde_json::from_str(&content)
                .map_err(|e| format!("测试证据校验失败: 沙盒 #{sid} 报告文件损坏: {e}"))?;
            let failed = report
                .pointer("/summary/failed")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| {
                    format!("测试证据校验失败: 沙盒 #{sid} 报告缺少 summary.failed 字段(结构异常)")
                })?;
            if failed != 0 {
                return Err(format!(
                    "测试证据校验失败（不静默）: 沙盒 #{sid} 报告有 {failed} 个失败用例,\
                     不得作为 pass 证据"
                ));
            }
        }
        Ok(())
    }

    /// ③ 第 8 项执行侧服务绑定核对（阻断项 ①）：bundle 声明的服务必须已绑定，
    /// 缺失 → **显式失败**（不静默）。防"治理侧声明 / 执行侧未绑定 → 运行时
    /// unknown service_name"（35 号 三层绑定：执行侧 service_registry 绑定）。
    /// 核对集 = 原生叶子能力 + service_registry.json（`with_bound_services` 注入）。
    /// C6（02 方案层 3）：sensitive=true 的服务必须**注册表显式绑定**。
    ///
    fn validate_service_bindings(
        &self,
        bundle: &evorule_bundle::DatasetBundle,
    ) -> Result<(), String> {
        let Some(dd) = &bundle.data_dependencies else {
            return Ok(());
        };
        let missing: Vec<&str> = dd
            .services
            .iter()
            .map(|s| s.service_name.as_str())
            .filter(|name| !self.bound_services.contains(*name))
            .collect();
        if !missing.is_empty() {
            let mut bound: Vec<&str> = self.bound_services.iter().map(String::as_str).collect();
            bound.sort_unstable();
            return Err(format!(
                "快照包声明了执行侧未绑定的服务（不静默）: {}；当前已绑定: {}",
                missing.join(", "),
                bound.join(", ")
            ));
        }

        // C6（02 方案层 3）：声明 sensitive=true 的服务必须**注册表显式绑定**
        // （service_registry.json，端点/凭据配置位），仅原生内嵌不满足敏感服务要求
        // （涉及凭据的服务必须可由运维显式配置/核对）。未经注册表绑定 → 显式失败（不静默）。
        let registry_names: HashSet<&str> = self
            .registry_services
            .iter()
            .map(|m| m.name.as_str())
            .collect();
        let sensitive_unbound: Vec<&str> = dd
            .services
            .iter()
            .filter(|s| s.sensitive)
            .map(|s| s.service_name.as_str())
            .filter(|name| !registry_names.contains(name))
            .collect();
        if !sensitive_unbound.is_empty() {
            let mut reg: Vec<&str> = registry_names.iter().copied().collect();
            reg.sort_unstable();
            return Err(format!(
                "快照包声明了 sensitive 服务但执行侧未在 service_registry 显式绑定（不静默）: {}；当前注册表: {}",
                sensitive_unbound.join(", "),
                reg.join(", ")
            ));
        }
        Ok(())
    }

    /// T2: 导入快照包（36 号 集成契约）—— 6 项硬校验 → 逐条 Schema 门禁 → 服务绑定核对
    /// → 原子落盘 → 触发 reload。
    ///
    /// - 任一硬校验失败 → `Err`（显式报错，不静默跳过，T0/35 号 §9）；
    /// - `dry_run=true` 只跑校验链（6 项 + Schema 门禁 + 服务绑定核对），不落盘不 reload；
    /// - 返回 [evorule_bundle::ImportResult]（校验通过后的运行配置）。
    pub async fn import_bundle(
        &self,
        bundle: &evorule_bundle::DatasetBundle,
        dry_run: bool,
    ) -> Result<evorule_bundle::ImportResult, String> {
        // ⓪ Q12 条目类型同质性：Rule 与 Knowledge 不得混装同一 bundle
        // （载荷语义互斥——transform 指令集与领域 payload 的门禁/消费通道完全不同）
        let has_rule = bundle
            .entries
            .iter()
            .any(|e| e.entry_kind == evorule_bundle::EntryKind::Rule);
        let has_knowledge = bundle
            .entries
            .iter()
            .any(|e| e.entry_kind == evorule_bundle::EntryKind::Knowledge);
        if has_rule && has_knowledge {
            return Err(
                "快照包条目类型混装（Rule 与 Knowledge 不得共存于同一 bundle，不静默）".to_string(),
            );
        }
        let is_knowledge = has_knowledge;

        // ① 6 项硬校验（schema → 防篡改 → 版本链 → 符号三方一致/领域 schema 强校验
        //    → 版本解析 → 闸门一证据）。Q12 D3：Knowledge 条目经 resolver 做领域
        //    schema 强校验（执行侧注册表 = `{knowledge_dir}/domain_schemas/*.json`，
        //    运维注入；未命中即拒绝——与治理侧同口径）。
        let result = evorule_bundle::BundleImporter::validate(bundle, &|uri: &str| {
            crate::knowledge_store::lookup_domain_schema_in(&self.knowledge_dir, uri)
        })
        .map_err(|e| format!("快照包校验失败（不静默）: {e}"))?;

        // ①.5 B2: 测试证据引用校验 —— 详见 validate_test_evidence 文档
        self.validate_test_evidence(bundle)?;

        // ② 第 7 项逐条 Schema 门禁（硬失败，防 loader fail-soft 静默跳过非法规则）。
        // Q12：Knowledge 条目不进 TCB，跳过 transform 门禁（D3 领域 schema 强校验
        // 已在 BundleImporter::validate 内完成——数据条目走自己的门禁，不是没有门禁）。
        for entry in &bundle.entries {
            if entry.entry_kind == evorule_bundle::EntryKind::Knowledge {
                continue;
            }
            let report = evorule_rule_schema::validate_rule_input(&entry.rule_body);
            if !report.valid {
                return Err(format!(
                    "条目 `{}` 未通过 Schema 门禁（引擎原生结构非法）: {}",
                    entry.entry_id,
                    report.errors.join("; ")
                ));
            }
        }

        // ③ 第 8 项执行侧服务绑定核对 —— 详见 validate_service_bindings 文档
        self.validate_service_bindings(bundle)?;

        if dry_run {
            return Ok(result);
        }

        // ④ 原子落盘（临时目录 → rename，失败清理无半成品；含 bundle_manifest.json）。
        // Q12 W1 分流：知识包落 `{knowledge_dir}/bundles/`（与 rules_dir 物理隔离，
        // TCB 加载路径天然不触碰数据文件）；规则包落 `rules_dir/bundles/`（原语义）。
        if is_knowledge {
            evorule_workspace::bundle_land::land_knowledge_bundle_atomically(
                &self.knowledge_dir,
                bundle,
                &result,
            )?;

            // ⑤a 数据资产不进 TCB：不触发规则 reload；刷新 KnowledgeStore 即刻可读。
            // 刷新失败 → Err（落盘已发生但内存索引未更新，数据面与磁盘不一致必须显式）。
            self.refresh_knowledge_store()?;
        } else {
            self.land_bundle_atomically(bundle, &result)?;

            // ⑤b 触发既有 reload 链（新会话使用新规则；已存在会话不改 TCB 语义）
            self.reload_from_disk().await?;
        }

        // ⑥ T5 审计溯源：bundle 导入历史写入 workspace 元数据库（bundle_imports 表）。
        // 管理元数据（imported_at 墙钟旁路），绝不渗入 fact / 内容哈希 / 审计验证链。
        // 写入失败 → 显式 Err（不静默掩盖审计缺失，35 号 §9）；bundle 落盘已完成但溯源
        // 未记录，调用方需知悉。溯源主体沿用治理侧导出者 exported_by（发布链发布者），
        // 缺省 fallback "system"。
        if let Some(ws_db) = &self.workspace_db {
            let imported_by = if bundle.audit.exported_by.is_empty() {
                "system".to_string()
            } else {
                bundle.audit.exported_by.clone()
            };
            ws_db
                .insert_bundle_import(
                    &bundle.bundle_id,
                    &result.dataset_id,
                    &result.source_version,
                    result.selection_mode.as_str(),
                    result.resolved_version.as_deref(),
                    &bundle.audit.content_hash,
                    result.entry_count as i64,
                    &imported_by,
                )
                .map_err(|e| format!("bundle 导入溯源写入失败（不静默）: {e}"))?;
        }

        Ok(result)
    }

    /// T4: 列出当前激活的 bundle（读 `rules/bundles/*/bundle_manifest.json`）。
    ///
    /// - 目录不存在 → 空列表（非错误）；
    /// - manifest 读取/解析失败 → **显式 Err**（不静默跳过，防激活状态被掩盖，
    ///   符合"透明可审计不静默"原则）；
    /// - 按 dataset_id 字典序稳定排序。
    pub fn active_bundles(&self) -> Result<Vec<crate::api::bundles::BundleManifest>, String> {
        let base = self.rules_dir.join("bundles");
        if !base.exists() {
            return Ok(Vec::new());
        }
        let read_dir =
            std::fs::read_dir(&base).map_err(|e| format!("读取 bundles 目录失败: {e}"))?;
        let mut out = Vec::new();
        for entry in read_dir.flatten() {
            let p = entry.path();
            if !p.is_dir() {
                continue;
            }
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') {
                continue; // 临时/备份目录
            }
            let manifest_path = p.join(crate::api::bundles::BUNDLE_MANIFEST_FILE);
            if !manifest_path.is_file() {
                continue;
            }
            let raw = std::fs::read_to_string(&manifest_path)
                .map_err(|e| format!("读取 `{}` 失败: {e}", manifest_path.display()))?;
            let m: crate::api::bundles::BundleManifest = serde_json::from_str(&raw)
                .map_err(|e| format!("解析 `{}` 失败: {e}", manifest_path.display()))?;
            out.push(m);
        }
        out.sort_by(|a, b| a.dataset_id.cmp(&b.dataset_id));
        Ok(out)
    }

    /// T5: 列出 bundle 导入溯源记录（bundle_imports 表, 按导入时间倒序, 限制条数）。
    ///
    /// - workspace 元数据库未接线（None）→ 空列表（非错误，未启用溯源）；
    /// - 查询失败 → **显式 Err**（不静默，35 号 §9）。
    pub fn list_bundle_imports(
        &self,
        limit: i64,
    ) -> Result<Vec<evorule_workspace::BundleImportRecord>, String> {
        match &self.workspace_db {
            Some(ws_db) => ws_db
                .list_bundle_imports(limit)
                .map_err(|e| format!("读取 bundle 导入溯源失败（不静默）: {e}")),
            None => Ok(Vec::new()),
        }
    }

    /// 原子落盘：`rules/bundles/{bundle_id}/{entry_id}.json`（rule_body 原样零转译）
    /// + `bundle_manifest.json`（版本语义/法规基准/哈希/条目→文件映射）。
    ///
    /// 审计⑥ 批 B（C5）: 实现下沉至 evorule-workspace（落盘 SSOT，发布链共用一份），
    /// 此处为薄委托。
    fn land_bundle_atomically(
        &self,
        bundle: &evorule_bundle::DatasetBundle,
        result: &evorule_bundle::ImportResult,
    ) -> Result<(), String> {
        evorule_workspace::bundle_land::land_bundle_atomically(&self.rules_dir, bundle, result)
    }
}

/// 读取生产会话 ID。
/// 读取失败 error 报警后按 None
/// 处理——本 tick 跳过保活/自愈,不阻断回收(报警不静默)。
fn production_session_id(
    workspace_db: Option<&Arc<evorule_workspace::WorkspaceDb>>,
) -> Option<u64> {
    workspace_db.and_then(|db| {
        db.get_production_state()
            .inspect_err(|e| {
                tracing::error!(
                    error = %e,
                    ": reaper 读取 production_state 失败,本 tick 跳过生产会话保活(报警不静默)"
                )
            })
            .ok()
            .and_then(|ps| ps.current_session_id.map(|i| i as u64))
    })
}

/// 生产会话保活:仍存活则刷新 last_activity(TTL 检查随后不会命中)。
///
async fn keepalive_production_session(
    sessions: &Arc<Mutex<session::SessionManager>>,
    prod_id: Option<u64>,
) {
    if let Some(pid) = prod_id {
        let mgr = sessions.lock().await;
        if mgr.get_session(pid).is_some() {
            mgr.touch_session(pid);
        }
    }
}

/// 自愈重建第二步:切换 production_state 会话引用(保留 ruleset_version/hash)。
///
fn switch_production_reference(
    workspace_db: &Arc<evorule_workspace::WorkspaceDb>,
    pid: u64,
    new_id: u64,
) {
    let (version, hash) = workspace_db
        .get_production_state()
        .map(|ps| {
            (
                ps.ruleset_version,
                ps.ruleset_hash.as_deref().unwrap_or("").to_string(),
            )
        })
        .unwrap_or((0, String::new()));
    match workspace_db.update_production_state(
        new_id as i64,
        version,
        &hash,
        "system:reaper-recovery",
    ) {
        Ok(()) => {
            tracing::info!(
                stale_session_id = pid,
                new_session_id = new_id,
                ": 生产会话已自愈重建(保留 ruleset_version/hash,语义为替换会话引用)"
            );
        }
        Err(e) => {
            tracing::error!(
                error = %e,
                new_session_id = new_id,
                ": 自愈重建 production_state 写入失败,新会话已建但引用未切换(下次 tick 重试)"
            );
        }
    }
}

/// 生产会话自愈重建:失忆(被 reap_finished 回收/reactor 异常退出)时报警 + 重建。
/// 与 启动期重建同构:保留 ruleset_version/hash,operator=system:reaper-recovery,
/// 语义为"替换会话引用"而非发布。
///
async fn recover_production_session(
    recovery_api: &SessionApi,
    workspace_db: &Arc<evorule_workspace::WorkspaceDb>,
    pid: u64,
) {
    let alive = recovery_api
        .sessions
        .lock()
        .await
        .get_session(pid)
        .is_some();
    if alive {
        return;
    }
    tracing::error!(
        session_id = pid,
        ": 生产会话失忆(reaper 回收/reactor 异常退出),触发运行期自愈重建"
    );
    // SessionOps::create_session 会为新会话 spawn IoSubscriber(与
    // 启动期重建同一条链)
    let new_id = match evorule_workspace::SessionOps::create_session(recovery_api).await {
        Ok(new_id) => new_id,
        Err(e) => {
            tracing::error!(
                error = ?e,
                ": 自愈重建会话创建失败,生产链路受阻(沙盒 fork/监控将 404)直至重建成功"
            );
            return;
        }
    };
    switch_production_reference(workspace_db, pid, new_id);
}

/// ①: reaper 单次回收(生产会话保活 + 失忆自愈重建)。
///
/// 从 `start_reaper` 抽出为独立异步函数以便单测(后台 spawn 任务不可直测)。
/// 三段语义:
///
/// 1. **保活**: 回收前先 touch 生产会话。查询端点(state/invariants/finished)
///    均不 touch,监控大屏在线也不保活——生产会话 30min 无命令即被 TTL 回收,
///    `production_state.current_session_id` 成幻影引用(监控轮询/沙盒 fork 全
///    404),直到重启才被 重建。生产会话是当前生效规则集的执行载体,
///    生命周期归治理链管辖(rolling_session 切换/server 退出),不适用空闲
///    回收语义。
/// 2. **回收**: `reap_all`(此时生产会话 last_activity 刚刷新,TTL 检查不会
///    命中;`reap_finished` 仍可回收 reactor 已退出的生产会话——那正是需要
///    自愈的场景)。
/// 3. **自愈**: 回收后检测生产会话存活,失忆则 error 报警 + 重建(与
///    启动期重建同构:保留 ruleset_version/hash,operator=system:reaper-recovery,
///    语义为"替换会话引用"而非发布)。旧会话 WAL 留痕仍在磁盘(audit_archive
///    可重建),内存 auditor 已随回收丢失——error 级报警供追溯(报警面纪律:
///    静默处置允许,静默通过禁止)。
///
/// `workspace_db` 为 None(单测/无元数据接线)时退化为纯回收,无保护无自愈。
/// 返回 (finished, expired) 细分(后台 reaper 记总数,手动 reap 端点报细分)。
///
async fn reap_once(
    sessions: &Arc<Mutex<session::SessionManager>>,
    workspace_db: Option<&Arc<evorule_workspace::WorkspaceDb>>,
    recovery_api: Option<&SessionApi>,
) -> (usize, usize) {
    let prod_id = production_session_id(workspace_db);

    // 1. 保活: 生产会话仍存活则刷新 last_activity(TTL 检查随后不会命中)
    keepalive_production_session(sessions, prod_id).await;

    // 2. 回收(生产会话刚被 touch,TTL 不命中;finished 的生产会话会被回收,
    //    由下一段自愈兜底)
    let finished = {
        let mgr = sessions.lock().await;

        mgr.reap_finished()
    };

    let expired = {
        let mgr = sessions.lock().await;

        mgr.reap_expired()
    };

    // 3. 自愈: 生产会话失忆(被 reap_finished 回收/reactor 异常退出)时报警 + 重建
    if let (Some(pid), Some(api), Some(db)) = (prod_id, recovery_api, workspace_db) {
        recover_production_session(api, db, pid).await;
    }

    (finished, expired)
}

// =============================================================================

// WC-10: SessionOps for SessionApi — 桥接 evorule-workspace 与 SessionManager

// =============================================================================

// 设计依据: WORKSPACE_CRATE_DESIGN.md §4 (SessionOps trait)

// workspace crate 通过 SessionOps trait 抽象会话操作,此处为 SessionApi 实现该 trait,

// 使 WorkspaceService 能够调用底层 SessionManager 的方法。

#[async_trait::async_trait]

impl evorule_workspace::SessionOps for SessionApi {
    async fn create_session(&self) -> evorule_workspace::WorkspaceResult<u64> {
        let result = {
            let sessions = self.sessions.lock().await;

            sessions.create_session()
        };

        match result {
            Ok(id) => {
                // 为新 session 的 reactor spawn IoSubscriber (复用 create_session handler 逻辑)

                // spawn hit-stats 归因记录任务
                self.spawn_hit_stats_recorder(id);

                if let Some(ref dispatcher) = self.dispatcher {
                    let sessions = self.sessions.lock().await;

                    if let Some(session) = sessions.get_session(id) {
                        let event_rx = session.event_tx.subscribe();

                        let command_tx = session.command_tx.clone();

                        let subscriber = IoSubscriber::new(dispatcher.clone())
                            .with_skip(Arc::new(is_external_executor_request));

                        tokio::spawn(async move {
                            if let Err(e) = subscriber.run(event_rx, command_tx).await {
                                tracing::error!(

                                    session_id = id,

                                    error = %e,

                                    "Workspace SessionOps: IoSubscriber 异常退出"

                                );
                            }
                        });
                    }
                }

                Ok(id)
            }

            Err(e) => Err(evorule_workspace::WorkspaceError::internal(format!(
                "create_session failed: {e:?}"
            ))),
        }
    }

    async fn fork_session(
        &self,

        parent_session_id: u64,
    ) -> evorule_workspace::WorkspaceResult<u64> {
        let result = {
            let sessions = self.sessions.lock().await;

            sessions.create_session_from_parent_at_version(parent_session_id, None)
        };

        match result {
            Ok(id) => {
                // spawn hit-stats 归因记录任务
                self.spawn_hit_stats_recorder(id);

                Ok(id)
            }

            Err(evorule_governance::session::SessionError::NotFound { id }) => Err(
                evorule_workspace::WorkspaceError::not_found("session", id.to_string()),
            ),

            Err(evorule_governance::session::SessionError::LimitExceeded { current, max }) => {
                Err(evorule_workspace::WorkspaceError::internal(format!(
                    "session limit exceeded: {current}/{max}"
                )))
            }

            Err(e) => Err(evorule_workspace::WorkspaceError::internal(format!(
                "fork_session failed: {e:?}"
            ))),
        }
    }

    async fn close_session(&self, session_id: u64) -> evorule_workspace::WorkspaceResult<()> {
        let result = {
            let sessions = self.sessions.lock().await;

            sessions.close_session(session_id)
        };

        match result {
            Ok(_) => Ok(()),

            Err(_) => Err(evorule_workspace::WorkspaceError::not_found(
                "session",
                session_id.to_string(),
            )),
        }
    }

    async fn session_exists(&self, session_id: u64) -> bool {
        self.sessions.lock().await.get_session(session_id).is_some()
    }

    async fn list_sessions(&self) -> evorule_workspace::WorkspaceResult<Vec<u64>> {
        let sessions = self.sessions.lock().await;

        Ok(sessions.list_sessions())
    }

    async fn send_command(
        &self,

        session_id: u64,

        command: serde_json::Value,
    ) -> evorule_workspace::WorkspaceResult<u64> {
        let id = self.next_id();

        let instruction = serde_to_tcb(command);

        let sessions = self.sessions.lock().await;

        sessions.touch_session(session_id);

        let session = sessions.get_session(session_id).ok_or_else(|| {
            evorule_workspace::WorkspaceError::not_found("session", session_id.to_string())
        })?;

        session
            .command_tx
            .send(Fact::Command { id, instruction })
            .map_err(|_| {
                evorule_workspace::WorkspaceError::internal(
                    "command channel closed (reactor exited)",
                )
            })?;

        // 缺口2 修复: send_command 后实时刷新审计链

        // 将 FactsLog 中已处理但尚未审计的 Fact 刷入 BLAKE3 哈希链。

        // reactor 异步处理, 此处刷新已处理的 fact; 未处理的由读方法兜底刷新。

        session.audit_new();

        Ok(id.0)
    }

    async fn get_session_state(
        &self,

        session_id: u64,
    ) -> evorule_workspace::WorkspaceResult<serde_json::Value> {
        let sessions = self.sessions.lock().await;

        sessions.touch_session(session_id);

        let session = sessions.get_session(session_id).ok_or_else(|| {
            evorule_workspace::WorkspaceError::not_found("session", session_id.to_string())
        })?;

        let (payload, queue, version) = session.facts_log.snapshot();

        let mut obj = serde_json::Map::new();

        obj.insert("payload".to_string(), tcb_to_serde(&payload));

        obj.insert(
            "queue".to_string(),
            serde_json::Value::Array(queue.iter().map(tcb_to_serde).collect()),
        );

        obj.insert(
            "version".to_string(),
            serde_json::Value::Number(version.into()),
        );

        Ok(serde_json::Value::Object(obj))
    }

    // ===== 沙盒编排扩展 (SANDBOX_ORCHESTRATION_DESIGN.md §3.2) =====

    /// 获取审计报告 (含 BLAKE3 链验证结果)
    ///
    /// 返回 JSON,包含审计链长度、验证状态、Fact 统计等。
    async fn get_audit_report(
        &self,

        session_id: u64,
    ) -> evorule_workspace::WorkspaceResult<serde_json::Value> {
        let sessions = self.sessions.lock().await;

        sessions.touch_session(session_id);

        let session = sessions.get_session(session_id).ok_or_else(|| {
            evorule_workspace::WorkspaceError::not_found("session", session_id.to_string())
        })?;

        // 兜底刷新: send_command 后已实时刷新, 此处确保 reactor 异步处理的 fact 也进链

        let _new_count = session.audit_new();

        let report_str = session.audit_report().map_err(|e| {
            evorule_workspace::WorkspaceError::internal(format!("audit report failed: {e}"))
        })?;

        let report: serde_json::Value = serde_json::from_str(&report_str).map_err(|e| {
            evorule_workspace::WorkspaceError::internal(format!("audit parse failed: {e}"))
        })?;

        // 附加验证状态 (report 字段已含 last_hash/entry_count,补充 verified)

        let mut enriched = report;

        if let serde_json::Value::Object(ref mut map) = enriched {
            map.insert("verified".into(), serde_json::json!(session.audit_verify()));

            map.insert("session_id".into(), serde_json::json!(session_id));
        }

        Ok(enriched)
    }

    /// 获取审计链导出 (JSON 字符串)
    ///
    /// 返回完整的审计链 JSON,用于沙盒关闭时导出 test Fact。
    async fn get_audit_export(
        &self,

        session_id: u64,
    ) -> evorule_workspace::WorkspaceResult<String> {
        let sessions = self.sessions.lock().await;

        sessions.touch_session(session_id);

        let session = sessions.get_session(session_id).ok_or_else(|| {
            evorule_workspace::WorkspaceError::not_found("session", session_id.to_string())
        })?;

        // 兜底刷新: 确保 reactor 异步处理的 fact 已进 BLAKE3 链后再导出

        let _new_count = session.audit_new();

        Ok(session.audit_export().map_err(|e| {
            evorule_workspace::WorkspaceError::internal(format!("audit export failed: {e}"))
        })?)
    }

    /// 获取 Fact 列表 (用于测试报告统计)
    ///
    /// 从审计链导出中解析出 Fact 列表。
    async fn get_facts(
        &self,

        session_id: u64,
    ) -> evorule_workspace::WorkspaceResult<Vec<serde_json::Value>> {
        let export_str = self.get_audit_export(session_id).await?;

        let export: serde_json::Value = serde_json::from_str(&export_str).map_err(|e| {
            evorule_workspace::WorkspaceError::internal(format!("audit export parse: {e}"))
        })?;

        // 审计导出包含 entries 数组,每条 entry 有 fact_id/fact_type/content_hash 等

        // 提取 entries 作为 Fact 列表

        let entries = export
            .get("entries")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        Ok(entries)
    }

    /// 获取因果链 (某条 Fact 的因果追溯)
    ///
    /// 返回从根 Fact 到指定 Fact 的因果链 JSON。
    async fn get_causal_chain(
        &self,

        session_id: u64,

        fact_id: u64,
    ) -> evorule_workspace::WorkspaceResult<serde_json::Value> {
        use evorule_reactor::FactId;

        let sessions = self.sessions.lock().await;

        sessions.touch_session(session_id);

        let session = sessions.get_session(session_id).ok_or_else(|| {
            evorule_workspace::WorkspaceError::not_found("session", session_id.to_string())
        })?;

        // 兜底刷新: 确保 fact 已进审计链后再追溯因果

        let _new_count = session.audit_new();

        let chain = session.causal_chain(FactId(fact_id));

        let entries: Vec<serde_json::Value> = chain
            .iter()
            .map(|e| {
                serde_json::json!({

                    "fact_id": e.fact_id.0,

                    "fact_type": e.fact_type,

                    "logical_time": e.logical_time,

                    "content_hash": e.content_hash,

                    "prev_hash": e.prev_hash,

                    "cause": e.cause.map(|c| c.0),

                })
            })
            .collect();

        Ok(serde_json::json!({

            "session_id": session_id,

            "fact_id": fact_id,

            "chain_length": entries.len(),

            "chain": entries,

        }))
    }

    // ===== 发布队列扩展 (PUBLISH_QUEUE_DESIGN.md §4) =====

    /// 触发规则热重载
    ///
    /// 调用 reload_from_disk,使 SessionManager 内部 core_eval 更新
    /// (影响后续新创建的 session,已存在 session 不受影响 — TCB 不可变语义)。
    async fn reload_rules(&self) -> evorule_workspace::WorkspaceResult<()> {
        self.reload_from_disk().await.map(|_| ()).map_err(|e| {
            evorule_workspace::WorkspaceError::internal(format!("reload_rules failed: {e}"))
        })
    }

    /// 显式刷新审计链 (缺口5 修复)
    ///
    /// 将 FactsLog 中尚未审计的 Fact 刷入 BLAKE3 哈希链, 返回本次新增条目数。
    /// send_command 后应调用此方法确保审计链实时性。
    async fn flush_audit(&self, session_id: u64) -> evorule_workspace::WorkspaceResult<usize> {
        let sessions = self.sessions.lock().await;

        sessions.touch_session(session_id);

        let session = sessions.get_session(session_id).ok_or_else(|| {
            evorule_workspace::WorkspaceError::not_found("session", session_id.to_string())
        })?;

        Ok(session.audit_new())
    }
}

/// SSE 连接配额守卫
///
///
/// RAII 模式：Drop 时自动减少全局 SSE 连接计数器，
///
/// 确保连接断开后配额被正确释放。
///
pub struct SseConnectionGuard {
    counter: Arc<AtomicU64>,
}

impl Drop for SseConnectionGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);

        tracing::debug!("SSE 连接配额已释放");
    }
}

/// SSE 指标守卫
///
///
/// RAII 模式：Drop 时自动减少 SSE 连接指标。
///
/// 在 `session_events` 的 `stream!` 内部持有，stream 结束时自动释放。
///
struct SseMetricsGuard(SharedMetrics);

impl Drop for SseMetricsGuard {
    fn drop(&mut self) {
        self.0.dec_sse_connections();
    }
}

/// 应用全局状态（合并 GovernanceApi + SessionApi + AgentManager + Metrics + Readiness + Workspace）
///
///
/// 通过 axum `FromRef` 模式，handler 可按需提取子状态：
///
/// - `State<GovernanceApi>` — 单反应器模式路由
///
/// - `State<SessionApi>` — 多会话模式路由
///
/// - `State<SharedMetrics>` — Prometheus 指标
///
/// - `State<ReadinessFlag>` — 就绪标志
///
/// - `State<WorkspaceState>` — 工作空间 + 规则元数据 (P10)
/// - `State<Arc<InputSanitizer>>` — Phase 1 第一层输入净化（Prompt 注入防御）
#[derive(Clone)]

pub struct AppState {
    /// 单反应器 API（向后兼容）
    governance: GovernanceApi,

    /// 多会话 API
    sessions: SessionApi,

    /// Prometheus 指标
    metrics: SharedMetrics,

    /// 就绪标志（优雅退出时设为 false）
    readiness: ReadinessFlag,

    /// 跨会话共享事实存储
    shared_facts: SharedFactsLog,

    /// 工作空间状态 (P10: 多租户工作空间 + 规则元数据)
    workspace: WorkspaceState,
    /// Phase 1: 输入净化器（HTTP 入口 Prompt 注入防御，静默改写）
    sanitizer: Arc<InputSanitizer>,

    /// :演示登录入口开关（--demo-auth，默认开）。
    /// 经 /api/platform/auth/status 公开下发，登录页据此隐藏演示入口。
    demo_auth: bool,

    /// 模板市场目录句柄
    marketplace_dir: MarketplaceDir,
}

impl AppState {
    /// 创建应用全局状态
    pub fn new(
        governance: GovernanceApi,

        sessions: SessionApi,

        metrics: SharedMetrics,

        readiness: ReadinessFlag,

        shared_facts: SharedFactsLog,

        workspace: WorkspaceState,
        sanitizer: Arc<InputSanitizer>,
    ) -> Self {
        // W4：模板市场目录自 SessionApi 派生（rules_dir 父目录拼接）——
        // 同模块直读私有字段；先取路径再移动 sessions，避免 use-after-move
        let marketplace_dir = MarketplaceDir(sessions.marketplace_dir.clone());
        Self {
            governance,

            sessions,

            metrics,

            readiness,

            shared_facts,

            workspace,
            sanitizer,
            // :演示登录入口默认开（体验包语义；生产建议 --demo-auth false）
            demo_auth: true,
            marketplace_dir,
        }
    }

    /// :设置演示登录入口开关（builder 风格，默认 true）
    pub fn with_demo_auth(mut self, enabled: bool) -> Self {
        self.demo_auth = enabled;
        self
    }

    /// :演示登录入口是否可用
    pub fn demo_auth(&self) -> bool {
        self.demo_auth
    }
}

/// :演示登录入口开关的 axum 状态提取器（经 FromRef 从 AppState 派生）。
#[derive(Clone, Copy, Debug)]
pub struct DemoAuthFlag(pub bool);

impl FromRef<AppState> for DemoAuthFlag {
    fn from_ref(state: &AppState) -> Self {
        DemoAuthFlag(state.demo_auth)
    }
}

impl FromRef<AppState> for GovernanceApi {
    fn from_ref(state: &AppState) -> Self {
        state.governance.clone()
    }
}

impl FromRef<AppState> for SessionApi {
    fn from_ref(state: &AppState) -> Self {
        state.sessions.clone()
    }
}

impl FromRef<AppState> for SharedMetrics {
    fn from_ref(state: &AppState) -> Self {
        state.metrics.clone()
    }
}

impl FromRef<AppState> for ReadinessFlag {
    fn from_ref(state: &AppState) -> Self {
        state.readiness.clone()
    }
}

impl FromRef<AppState> for SharedFactsLog {
    fn from_ref(state: &AppState) -> Self {
        state.shared_facts.clone()
    }
}

impl FromRef<AppState> for WorkspaceState {
    fn from_ref(state: &AppState) -> Self {
        state.workspace.clone()
    }
}

impl FromRef<AppState> for Arc<InputSanitizer> {
    fn from_ref(state: &AppState) -> Self {
        state.sanitizer.clone()
    }
}

/// 模板市场目录句柄
#[derive(Clone)]
pub struct MarketplaceDir(pub std::path::PathBuf);

impl FromRef<AppState> for MarketplaceDir {
    fn from_ref(state: &AppState) -> Self {
        state.marketplace_dir.clone()
    }
}

/// HTTP API 请求体

#[derive(Debug, Deserialize, ToSchema)]

pub struct CommandRequest {
    /// 指令 JSON（任意 JSON 值，作为 TCB 指令提交给反应器）
    pub instruction: serde_json::Value,
}

/// HTTP API 通用响应

#[derive(Debug, Serialize, ToSchema)]

pub struct ApiResponse {
    /// 是否成功
    pub success: bool,

    /// 消息
    pub message: String,

    /// Fact ID（如适用）
    pub fact_id: Option<u64>,
}

/// 插件健康快照（启动时由 main 注入；未注入 = 未配置清单,原生插件缺省全启用）
static PLUGIN_HEALTH: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();

/// 启动期注入插件健康快照（main.rs 在插件清单校验通过后调用一次）
pub fn set_plugin_health(v: serde_json::Value) {
    let _ = PLUGIN_HEALTH.set(v);
}

/// 插件运行时存活状态快照（探活任务每轮更新；键=external 插件 id）。
/// 与 PLUGIN_HEALTH（启动期挂载事实,OnceLock 不可变）分离——挂载事实与
/// 运行时存活是两类语义,health handler 合并呈现于 external 插件节。
static PLUGIN_LIVENESS: std::sync::RwLock<std::collections::BTreeMap<String, LivenessEntry>> =
    std::sync::RwLock::new(std::collections::BTreeMap::new());

/// 单插件运行时存活状态（探活快照,/api/health external 插件节呈现）
#[derive(Debug, Clone, serde::Serialize)]
pub struct LivenessEntry {
    /// 存活状态:online(2xx 且 JSON 可解析)/offline(超时/连接失败/非 2xx)/
    /// no_probe(/health 404/405,插件未实现探活端点——不报警)
    pub status: String,
    /// 最近一次探测时间(unix ms)
    pub last_probe_ts: u64,
    /// 最近一次在线时间(unix ms;从未在线则省略)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_ok_ts: Option<u64>,
    /// 最近一次探测失败摘要(online 时省略)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// 探活任务每轮写入/更新插件存活状态(main 侧 plugin_probe 调用)
pub fn update_plugin_liveness(id: &str, entry: LivenessEntry) {
    if let Ok(mut map) = PLUGIN_LIVENESS.write() {
        map.insert(id.to_string(), entry);
    }
}

/// external 插件节合并运行时存活状态(纯函数,单测锁定):仅当 liveness 表
/// 有该插件条目且插件节标记 external=true 时插入 status/last_probe/last_ok/
/// last_error——探活任务未运行(空表)时响应与启动期快照逐字节一致(向后兼容)。
pub fn merge_liveness_into_plugins(
    plugins: Option<serde_json::Value>,
    liveness: &std::collections::BTreeMap<String, LivenessEntry>,
) -> Option<serde_json::Value> {
    let mut plugins = plugins?;
    let obj = plugins.as_object_mut()?;
    for (id, entry) in liveness {
        let Some(node) = obj.get_mut(id) else {
            continue;
        };
        if node.get("external") != Some(&serde_json::Value::Bool(true)) {
            continue; // 仅 external 插件有探活语义(native 随宿主生死)
        }
        let Some(m) = node.as_object_mut() else {
            continue;
        };
        m.insert("status".into(), serde_json::Value::String(entry.status.clone()));
        m.insert("last_probe".into(), serde_json::json!(entry.last_probe_ts));
        if let Some(ok) = entry.last_ok_ts {
            m.insert("last_ok".into(), serde_json::json!(ok));
        }
        if let Some(e) = &entry.last_error {
            m.insert("last_error".into(), serde_json::Value::String(e.clone()));
        }
    }
    Some(plugins)
}

/// `/api/health` 专用响应

#[derive(Debug, Serialize, ToSchema)]

pub struct HealthResponse {
    /// 是否成功
    pub success: bool,

    /// 消息
    pub message: String,

    /// 插件健康快照
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugins: Option<serde_json::Value>,
}

/// PayloadUpdate 请求体

#[derive(Debug, Deserialize, ToSchema)]

pub struct PayloadUpdateRequest {
    /// 字段路径
    pub path: String,

    /// 字段值
    pub value: serde_json::Value,
}

// ===== OpenAPI 响应 schema（单一真相源，P2）=====

// 以下类型仅用于 `#[utoipa::path]` 标注中精确描述各端点的响应结构。

// 动态载荷（payload / queue / fact 等）保留为 `serde_json::Value`，

// 由 description 说明其结构；固定结构字段一律强类型化。

/// 会话 ID 通用响应（create/close 等）

#[derive(Debug, Serialize, ToSchema)]

pub struct SessionIdResponse {
    /// 会话 ID
    pub session_id: u64,

    /// 消息
    pub message: String,
}

/// 会话列表响应

#[derive(Debug, Serialize, ToSchema)]

pub struct SessionListResponse {
    /// 活跃会话 ID 列表
    pub sessions: Vec<u64>,
}

/// 从父会话派生响应（from/fork）

#[derive(Debug, Serialize, ToSchema)]

pub struct SessionForkResponse {
    /// 新会话 ID
    pub session_id: u64,

    /// 父会话 ID
    pub parent_session_id: u64,

    /// 消息
    pub message: String,

    /// fork 时的父会话版本（可选）
    pub forked_from_version: Option<u64>,
}

/// 单反应器状态快照（GET /api/state）

#[derive(Debug, Serialize, ToSchema)]

pub struct StateResponse {
    /// 当前 payload（任意 JSON 状态载荷）
    pub payload: serde_json::Value,

    /// 待处理指令队列（Fact JSON 数组）
    pub queue: Vec<serde_json::Value>,

    /// FactsLog 版本号
    pub version: u64,
}

/// 会话状态快照（GET /api/sessions/{id}/state）

#[derive(Debug, Serialize, ToSchema)]

pub struct SessionStateResponse {
    /// 当前 payload
    pub payload: serde_json::Value,

    /// 待处理指令队列
    pub queue: Vec<serde_json::Value>,

    /// FactsLog 版本号
    pub version: u64,

    /// 反应器运行时状态
    pub reactor: ReactorStatus,
}

/// 反应器运行时状态（session_state.reactor）

#[derive(Debug, Serialize, ToSchema)]

pub struct ReactorStatus {
    /// 当前执行阶段（如 Idle/Running/Finished）
    pub phase: Option<String>,

    /// 因果链深度
    pub causal_depth: Option<u64>,

    /// 结构不变式违规计数
    pub structural_invariant_violations: u64,

    /// 待处理 I/O 数量
    pub pending_io_count: Option<u64>,

    /// 当前执行步数
    pub current_step: Option<u64>,
}

/// 审计条目（与 core AuditEntry 字段一致，用于 OpenAPI 强类型标注）
///
///
/// 对应 evorule_governance::auditor::AuditEntry，
///
/// 运行时通过 Auditor::report 序列化产出，字段语义完全一致。
///
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]

pub struct AuditEntryResponse {
    /// Fact ID
    pub fact_id: u64,

    /// Fact 类型名
    pub fact_type: String,

    /// 逻辑时钟值
    pub logical_time: u64,

    /// 内容哈希
    pub content_hash: String,

    /// 前一条目的哈希（形成哈希链）
    pub prev_hash: String,

    /// 因果父 Fact ID（如有）

    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<u64>,

    /// F5：完整 Fact 内容 JSON（仅查询参数 include_content=true 时存在；
    /// Auditor 条目本身不含内容，server 从 FactsLog 按 fact_id 关联注入）

    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_json: Option<serde_json::Value>,
}

/// 审计报告响应（GET /api/sessions/{id}/audit）

#[derive(Debug, Serialize, ToSchema)]

pub struct AuditResponse {
    /// 会话 ID
    pub session_id: u64,

    /// 事实条目数
    pub fact_count: u64,

    /// 最新条目哈希
    pub last_hash: Option<serde_json::Value>,

    /// 审计链完整性验证结果
    pub verified: bool,

    /// 审计条目数组（强类型，与 core AuditEntry 字段一致）
    pub entries: Vec<AuditEntryResponse>,
}

/// 审计链验证响应（GET /api/sessions/{id}/audit/verify）

#[derive(Debug, Serialize, ToSchema)]

pub struct AuditVerifyResponse {
    /// 会话 ID
    pub session_id: u64,

    /// 事实条目数
    pub fact_count: Option<serde_json::Value>,

    /// 最新条目哈希
    pub last_hash: Option<serde_json::Value>,

    /// 审计链完整性验证结果
    pub verified: bool,
}

/// 因果链条目

#[derive(Debug, Serialize, ToSchema)]

pub struct CausalChainEntry {
    /// 事实 ID
    pub fact_id: u64,

    /// 事实类型
    pub fact_type: String,

    /// 逻辑时间
    pub logical_time: u64,

    /// 内容哈希
    pub content_hash: String,

    /// 前一哈希
    pub prev_hash: String,

    /// 原因事实 ID（可选）
    pub cause: Option<u64>,
}

/// 因果链响应（GET /api/sessions/{id}/audit/causal/{fact_id}）

#[derive(Debug, Serialize, ToSchema)]

pub struct CausalChainResponse {
    /// 会话 ID
    pub session_id: u64,

    /// 目标事实 ID
    pub fact_id: u64,

    /// 链长度
    pub chain_length: usize,

    /// 因果链条目
    pub chain: Vec<CausalChainEntry>,
}

/// 审计链导入响应（import / import/compressed）

#[derive(Debug, Serialize, ToSchema)]

pub struct AuditImportResponse {
    /// 会话 ID
    pub session_id: u64,

    /// 是否导入成功
    pub imported: bool,

    /// 导入后验证是否通过
    pub verify_ok: bool,

    /// 状态（ok / verify_failed）
    pub status: String,

    /// 格式（仅 compressed 端点，如 "gzip"）
    pub format: Option<String>,
}

/// 时间旅行回退响应（GET /api/sessions/{id}/rewind）

#[derive(Debug, Serialize, ToSchema)]

pub struct RewindResponse {
    /// 会话 ID
    pub session_id: u64,

    /// 目标版本
    pub target_version: u64,

    /// 回退后的 payload
    pub payload: serde_json::Value,

    /// 回退后的队列
    pub queue: serde_json::Value,

    /// 实际回退到的版本
    pub actual_version: u64,
}

/// 会话 diff 响应（GET /api/sessions/{id}/diff）

#[derive(Debug, Serialize, ToSchema)]

pub struct DiffResponse {
    /// 会话 ID
    pub session_id: u64,

    /// 起始版本
    pub from_version: u64,

    /// 结束版本
    pub to_version: u64,

    /// diff 条目（[key,value] / [key,old,new] 元组数组）
    pub items: Vec<serde_json::Value>,

    /// 被移除的键值对（[key,value] 数组）
    pub removed: Vec<serde_json::Value>,

    /// diff 摘要
    pub summary: String,
}

/// 会话 payload 事实条目

#[derive(Debug, Serialize, ToSchema)]

pub struct SessionFact {
    /// 事实 ID
    pub fact_id: u64,

    /// FactsLog 版本号
    pub version: u64,

    /// payload 路径
    pub path: String,

    /// 路径对应的值
    pub value: serde_json::Value,
}

/// 共享事实条目

#[derive(Debug, Serialize, ToSchema)]

pub struct SharedFact {
    /// 事实 ID
    pub fact_id: u64,

    /// 共享路径
    pub path: String,

    /// 值
    pub value: serde_json::Value,

    /// 来源会话 ID
    pub source_session_id: u64,

    /// 版本号
    pub version: u64,
}

/// used_at_startup 查询响应

#[derive(Debug, Serialize, ToSchema)]

pub struct UsedAtStartupResponse {
    /// 会话 ID
    pub session_id: u64,

    /// 该会话启动时使用的共享事实 ID 列表
    pub fact_ids: Vec<u64>,
}

/// 共享事实使用方响应

#[derive(Debug, Serialize, ToSchema)]

pub struct SessionsUsingFactResponse {
    /// 共享事实 ID
    pub fact_id: u64,

    /// 使用该事实的会话 ID 列表
    pub sessions: Vec<u64>,
}

/// Debug phase 响应

#[derive(Debug, Serialize, ToSchema)]

pub struct DebugPhaseResponse {
    /// 会话 ID
    pub session_id: u64,

    /// 当前执行阶段
    pub phase: Option<String>,
}

/// Debug queue 响应

#[derive(Debug, Serialize, ToSchema)]

pub struct DebugQueueResponse {
    /// 会话 ID
    pub session_id: u64,

    /// 队列（当前恒为空数组，供调试占位）
    pub queue: Vec<serde_json::Value>,
}

/// Debug pending_io 响应

#[derive(Debug, Serialize, ToSchema)]

pub struct DebugPendingIoResponse {
    /// 会话 ID
    pub session_id: u64,

    /// 待处理 I/O 数量
    pub pending_io_count: u64,

    /// 待处理 I/O 列表（当前恒为空数组，供调试占位）
    pub pending_io: Vec<serde_json::Value>,
}

/// 会话中断响应

#[derive(Debug, Serialize, ToSchema)]

pub struct InterruptResponse {
    /// 会话 ID
    pub session_id: u64,

    /// 是否成功
    pub success: bool,

    /// 消息
    pub message: String,
}

/// 单会话元数据响应（GET /api/sessions/{id}，014 合法 API #1）
///
///
/// 补齐跨会话因果追溯所需的父链信息：parent_session_id / initial_content_hash。
///
#[derive(Debug, Serialize, ToSchema)]

pub struct SessionMetadataResponse {
    /// 会话 ID
    pub session_id: u64,

    /// 父会话 ID（派生会话才有）
    pub parent_session_id: Option<u64>,

    /// 初始内容哈希（基于父会话最终状态或初始 payload 计算）
    pub initial_content_hash: Option<String>,

    /// 初始内容哈希口径（审计⑥ C2）: "audit_chain_compact"（当前,与审计链同源）或 "tcb_display"（历史兼容）
    pub content_hash_scheme: Option<&'static str>,

    /// 距最近活动的空闲秒数（单调时钟，便于观察活跃度）
    pub idle_secs: f64,

    /// 反应器是否已结束
    pub is_finished: bool,

    /// 当前执行阶段（如 Idle/Running/Finished，未启动时为 None）
    pub phase: Option<String>,

    /// 审计链自动验证是否启用
    pub auto_verify: bool,
}

/// 会话回收响应（POST /api/sessions/reap，014 合法 API #5）

#[derive(Debug, Serialize, ToSchema)]

pub struct ReapResponse {
    /// 回收的已结束会话数
    pub finished: usize,

    /// 回收的已过期会话数
    pub expired: usize,

    /// 合计
    pub total: usize,
}

/// 会话完成状态响应

#[derive(Debug, Serialize, ToSchema)]

pub struct FinishedResponse {
    /// 会话 ID
    pub session_id: u64,

    /// 是否已结束
    pub finished: bool,
}

/// 因果深度响应

#[derive(Debug, Serialize, ToSchema)]

pub struct CausalDepthResponse {
    /// 会话 ID
    pub session_id: u64,

    /// 因果链深度
    pub causal_depth: u64,
}

/// 结构不变式违规响应

#[derive(Debug, Serialize, ToSchema)]

pub struct InvariantsResponse {
    /// 会话 ID
    pub session_id: u64,

    /// 结构不变式违规计数
    pub structural_invariant_violations: u64,
}

/// 待处理 I/O 数量响应

#[derive(Debug, Serialize, ToSchema)]

pub struct PendingIoCountResponse {
    /// 会话 ID
    pub session_id: u64,

    /// 待处理 I/O 数量
    pub pending_io_count: u64,
}

/// 执行步数响应

#[derive(Debug, Serialize, ToSchema)]

pub struct StepResponse {
    /// 会话 ID
    pub session_id: u64,

    /// 当前执行步数
    pub current_step: u64,
}

/// 会话快照响应（GET /api/sessions/{id}/snapshot）

#[derive(Debug, Serialize, ToSchema)]

pub struct SnapshotResponse {
    /// 会话 ID
    pub session_id: u64,

    /// 是否已结束
    pub finished: bool,

    /// 执行阶段
    pub phase: String,

    /// FactsLog 版本号
    pub version: u64,

    /// 已执行步数
    pub steps: u64,

    /// 待处理 I/O 数量
    pub pending_io_count: u64,

    /// 结构不变式违规计数
    pub structural_invariant_violations: u64,
}

/// 审计链自动验证状态响应

#[derive(Debug, Serialize, ToSchema)]

pub struct AutoVerifyResponse {
    /// 会话 ID
    pub session_id: u64,

    /// 自动验证是否启用
    pub auto_verify: bool,
}

/// 审计链自动验证配置响应（POST）

#[derive(Debug, Serialize, ToSchema)]

pub struct AutoVerifyConfigureResponse {
    /// 会话 ID
    pub session_id: u64,

    /// 是否成功
    pub success: bool,

    /// 自动验证是否启用
    pub auto_verify: bool,

    /// 验证阈值
    pub threshold: u64,

    /// 验证间隔
    pub interval: u64,

    /// 消息
    pub message: String,
}

/// IoResponse 请求体（外部提交）

#[derive(Debug, Deserialize, ToSchema)]

pub struct IoResponseRequest {
    /// 对应的 IoRequest ID
    pub request_id: u64,

    /// I/O 执行结果
    pub result: serde_json::Value,

    /// I/O 错误信息（可选）
    pub error: Option<String>,
}

/// 审计链自动验证配置请求体

#[derive(Debug, Deserialize, ToSchema)]

pub struct AutoVerifyRequest {
    /// 是否启用自动验证
    pub enabled: bool,

    /// 验证阈值（0 = 不限制）

    #[serde(default)]
    pub threshold: u64,

    /// 验证间隔（1 = 每次都验证）

    #[serde(default)]
    pub interval: u64,
}

/// 共享事实 ID 批量请求体（rollup / used_at_startup）

#[derive(Debug, Deserialize, ToSchema)]

pub struct FactIdsRequest {
    /// 事实 ID 列表
    pub fact_ids: Vec<u64>,
}

/// 将 serde_json::Value 转换为 evorule_tcb::JsonValue
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
            let mut map = std::collections::BTreeMap::new();

            for (k, val) in obj {
                map.insert(k, serde_to_tcb(val));
            }

            JsonValue::Object(map)
        }
    }
}

/// 将 evorule_tcb::JsonValue 转换为 serde_json::Value
fn tcb_to_serde(v: &JsonValue) -> serde_json::Value {
    match v {
        JsonValue::Null => serde_json::Value::Null,

        JsonValue::Bool(b) => serde_json::Value::Bool(*b),

        JsonValue::Integer(i) => serde_json::Value::Number((*i).into()),

        JsonValue::String(s) => serde_json::Value::String(s.to_string()),

        JsonValue::Array(arr) => serde_json::Value::Array(arr.iter().map(tcb_to_serde).collect()),

        JsonValue::Object(map) => {
            let mut obj = serde_json::Map::new();

            for (k, val) in map {
                obj.insert(k.clone(), tcb_to_serde(val));
            }

            serde_json::Value::Object(obj)
        }
    }
}

// portal 已移除（应用层功能）

use async_stream::stream;

use axum::body::Bytes;

use axum::extract::{FromRef, Path, Query, State};

use axum::http::{header, StatusCode};

use axum::response::sse::{Event, Sse};

use axum::response::{IntoResponse, Json, Response};

use axum::routing::{get, post};

use axum::Router;

use futures_core::Stream;

use std::sync::atomic::{AtomicU64, Ordering};

use std::time::Duration;

use tokio::sync::broadcast;

use tower_http::cors::{AllowOrigin, CorsLayer};

use tower_http::limit::RequestBodyLimitLayer;

/// 将 Fact 序列化为 SSE 事件 data 字段（JSON 字符串）
///
///
/// 格式：`{"type":"Command","id":1,"instruction":{...}}`
///
pub fn fact_to_sse_data(fact: &Fact) -> String {
    let mut obj = serde_json::Map::new();

    match fact {
        Fact::Command { id, instruction } => {
            obj.insert("type".into(), serde_json::Value::String("Command".into()));

            obj.insert("id".into(), serde_json::Value::Number(id.0.into()));

            obj.insert("instruction".into(), tcb_to_serde(instruction));
        }

        Fact::StateTransition {
            id,

            cause,

            new_payload,

            new_queue,
        } => {
            obj.insert(
                "type".into(),
                serde_json::Value::String("StateTransition".into()),
            );

            obj.insert("id".into(), serde_json::Value::Number(id.0.into()));

            obj.insert("cause".into(), serde_json::Value::Number(cause.0.into()));

            obj.insert("new_payload".into(), tcb_to_serde(new_payload));

            obj.insert(
                "new_queue".into(),
                serde_json::Value::Array(new_queue.iter().map(tcb_to_serde).collect()),
            );
        }

        Fact::IoRequest {
            id,

            cause,

            io_type,

            params,
        } => {
            obj.insert("type".into(), serde_json::Value::String("IoRequest".into()));

            obj.insert("id".into(), serde_json::Value::Number(id.0.into()));

            obj.insert("cause".into(), serde_json::Value::Number(cause.0.into()));

            obj.insert(
                "io_type".into(),
                serde_json::Value::String(io_type.to_string()),
            );

            obj.insert("params".into(), tcb_to_serde(params));
        }

        Fact::IoResponse {
            id,

            request_id,

            result,

            error,
        } => {
            obj.insert(
                "type".into(),
                serde_json::Value::String("IoResponse".into()),
            );

            obj.insert("id".into(), serde_json::Value::Number(id.0.into()));

            obj.insert(
                "request_id".into(),
                serde_json::Value::Number(request_id.0.into()),
            );

            obj.insert("result".into(), tcb_to_serde(result));

            if let Some(err) = error {
                obj.insert("error".into(), serde_json::Value::String(err.clone()));
            }
        }

        //：Stable 瘦身为版本号（原 final_snapshot 全量快照为
        // O(n²) 根因之一）；状态本体由最近一条 StateTransition.new_payload 承担，
        // 消费方经会话 snapshot API 获取最终 payload
        Fact::Stable { id, version } => {
            obj.insert("type".into(), serde_json::Value::String("Stable".into()));

            obj.insert("id".into(), serde_json::Value::Number(id.0.into()));

            obj.insert(
                "version".into(),
                serde_json::Value::Number((*version).into()),
            );
        }

        Fact::PayloadUpdate { id, path, value } => {
            obj.insert(
                "type".into(),
                serde_json::Value::String("PayloadUpdate".into()),
            );

            obj.insert("id".into(), serde_json::Value::Number(id.0.into()));

            obj.insert("path".into(), serde_json::Value::String(path.clone()));

            obj.insert("value".into(), tcb_to_serde(value));
        }

        Fact::Error { id, message } => {
            obj.insert("type".into(), serde_json::Value::String("Error".into()));

            obj.insert("id".into(), serde_json::Value::Number(id.0.into()));

            obj.insert("message".into(), serde_json::Value::String(message.clone()));
        }

        // 规则命中归因轨迹（记录性事实，不推进版本；
        // rule_hits 与该次转换的合并规则列表等长，按执行顺序）
        Fact::TransitionTrace {
            id,
            cause,
            rule_hits,
        } => {
            obj.insert(
                "type".into(),
                serde_json::Value::String("TransitionTrace".into()),
            );

            obj.insert("id".into(), serde_json::Value::Number(id.0.into()));

            obj.insert("cause".into(), serde_json::Value::Number(cause.0.into()));

            obj.insert(
                "rule_hits".into(),
                serde_json::Value::Array(
                    rule_hits
                        .iter()
                        .map(|h| {
                            serde_json::json!({
                                "index": h.index,
                                "instr_type": h.instr_type,
                                "hit": h.hit,
                            })
                        })
                        .collect(),
                ),
            );
        }
    }

    serde_json::Value::Object(obj).to_string()
}

/// 健康检查 handler（向后兼容，等价于 liveness）

#[utoipa::path(

    get,

    path = "/api/health",

    tag = "health",

    responses(

        (status = 200, description = "服务健康", body = HealthResponse)

    )

)]

async fn health() -> Json<HealthResponse> {
    // external 插件节合并运行时存活状态(插件探活):探活任务在跑才呈现——
    // liveness 空表时响应与启动期快照逐字节一致(向后兼容)。读锁短持即放。
    let liveness = PLUGIN_LIVENESS.read().ok().map(|m| m.clone());
    let plugins = PLUGIN_HEALTH.get().cloned();
    let plugins = match liveness {
        Some(map) if !map.is_empty() => merge_liveness_into_plugins(plugins, &map),
        _ => plugins,
    };
    Json(HealthResponse {
        success: true,

        message: "ok".to_string(),

        // :插件健康快照(未配置清单 → 省略该节)
        plugins,
    })
}

/// Liveness 探针（进程存活检查）
///
/// `GET /api/health/liveness` → 始终返回 200，只要进程在运行就算存活。
/// Kubernetes livenessProbe 用此端点判断是否需要重启容器。
#[utoipa::path(

    get,

    path = "/api/health/liveness",

    tag = "health",

    responses(

        (status = 200, description = "进程存活", body = ApiResponse)

    )

)]

async fn liveness() -> Json<ApiResponse> {
    Json(ApiResponse {
        success: true,

        message: "alive".to_string(),

        fact_id: None,
    })
}

/// Readiness 探针（就绪检查）
///
/// `GET /api/health/readiness` → readiness flag 为 true 时返回 200，否则 503。
/// 优雅退出时 flag 设为 false，负载均衡器将流量切走。
#[utoipa::path(

    get,

    path = "/api/health/readiness",

    tag = "health",

    responses(

        (status = 200, description = "服务就绪", body = ApiResponse),

        (status = 503, description = "服务不就绪（优雅退出中）")

    )

)]

async fn readiness(State(flag): State<ReadinessFlag>) -> Result<Json<ApiResponse>, StatusCode> {
    if flag.load(std::sync::atomic::Ordering::SeqCst) {
        Ok(Json(ApiResponse {
            success: true,

            message: "ready".to_string(),

            fact_id: None,
        }))
    } else {
        Err(StatusCode::SERVICE_UNAVAILABLE)
    }
}

/// Prometheus 指标端点
///
///
/// `GET /metrics` → 返回 Prometheus 文本格式指标数据。
///
/// 此端点免认证（Prometheus scraper 通常不携带 token），但仍受速率限制和并发限制保护。
///
#[utoipa::path(
    get,
    path = "/metrics",
    tag = "metrics",
    responses(
        (status = 200, description = "Prometheus 文本格式指标", content_type = "text/plain", body = String)
    )
)]
async fn metrics_handler(State(metrics): State<SharedMetrics>) -> String {
    metrics.render_as_text()
}

/// HTTP 请求计数中间件（接入 http_requests_total 指标）
///
///
/// 用 method + 归一化 path + status 作为 label。
///
/// path 归一化：把纯数字段替换为 `{id}`，避免 /api/sessions/42/command 与
///
/// /api/sessions/43/command 产生不同 label 导致 Prometheus 基数爆炸。
///
async fn http_metrics_middleware(
    State(metrics): State<SharedMetrics>,

    req: axum::extract::Request,

    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = req.method().clone();

    let path = req.uri().path().to_string();

    let response = next.run(req).await;

    let status_code = response.status();

    let normalized = normalize_path_for_metrics(&path);

    metrics.inc_http_requests(method.as_str(), &normalized, status_code.as_str());

    response
}

/// 归一化 path 用于 metrics label，防止基数爆炸
///
///
/// 把纯数字段替换为 `{id}`：
///
/// - `/api/sessions/42/command` → `/api/sessions/{id}/command`
///
/// - `/api/sessions/42/audit/100` → `/api/sessions/{id}/audit/{id}`
///
fn normalize_path_for_metrics(path: &str) -> String {
    path.split('/')
        .map(|seg| {
            if !seg.is_empty() && seg.chars().all(|c| c.is_ascii_digit()) {
                "{id}"
            } else {
                seg
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// 提交命令 handler

#[utoipa::path(

    post,

    path = "/api/command",

    tag = "governance",

    request_body = CommandRequest,

    responses(

        (status = 200, description = "命令已提交，返回 fact_id", body = ApiResponse),

        (status = 401, description = "未认证")

    )

)]

async fn submit_command(
    State(api): State<GovernanceApi>,

    State(metrics): State<SharedMetrics>,
    State(sanitizer): State<Arc<InputSanitizer>>,
    Json(req): Json<CommandRequest>,
) -> Result<Json<ApiResponse>, StatusCode> {
    // Phase 1: 第一层输入净化（静默改写 Prompt 注入内容）
    let (instruction_value, sanitize_report) = sanitizer.sanitize_value(&req.instruction);
    if sanitize_report.has_hits() {
        // P5-A1：命中指标化（按 rule），攻击态势可监控告警
        for rule in sanitize_report.unique_hits() {
            metrics.inc_sanitize_hits(rule);
        }
        tracing::warn!(
            hits = ?sanitize_report.unique_hits(),
            hit_count = sanitize_report.hit_count(),
            "submit_command 输入净化命中（已静默改写）"
        );
    }

    // 按指令类型计数

    {
        let cmd_type = instruction_value
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        metrics.inc_commands(cmd_type);
    }

    // Opt3：指令层结构门禁（线1 防御层, records/77）。与 session_command 同口径，
    // 提交即校验并拦截结构非法指令（单数 __io_result__ / 坏路径 / 非法控制流结构）。
    let schema_report = evorule_rule_schema::validate_command_instruction(&instruction_value);
    if !schema_report.valid {
        let detail = schema_report.errors.join("; ");
        tracing::warn!("submit_command 未通过 Schema 门禁，已拒绝提交: {detail}");
        return Ok(Json(ApiResponse {
            success: false,
            message: format!(
                "指令未通过 Schema 门禁（引擎原生结构非法，参见固化 rule_set v1.0 Schema，records/77）: {detail}"
            ),
            fact_id: None,
        }));
    }

    let instruction = serde_to_tcb(instruction_value);
    match api.send_command(instruction) {
        Ok(id) => Ok(Json(ApiResponse {
            success: true,

            message: "Command submitted".to_string(),

            fact_id: Some(id.0),
        })),

        Err(msg) => Ok(Json(ApiResponse {
            success: false,

            message: msg,

            fact_id: None,
        })),
    }
}

/// PayloadUpdate handler

#[utoipa::path(

    post,

    path = "/api/payload",

    tag = "governance",

    request_body = PayloadUpdateRequest,

    responses(

        (status = 200, description = "PayloadUpdate 已提交，返回 fact_id", body = ApiResponse),

        (status = 403, description = "受保护域写入被拒绝（需 service 身份，B5-server）"),

        (status = 401, description = "未认证")

    )

)]

async fn update_payload(
    State(api): State<GovernanceApi>,

    State(metrics): State<SharedMetrics>,
    State(sanitizer): State<Arc<InputSanitizer>>,

    identity: Option<Extension<CallerIdentity>>,

    Json(req): Json<PayloadUpdateRequest>,
) -> Result<(StatusCode, Json<ApiResponse>), StatusCode> {
    // B5-server：受保护域准入——`shared.*.stable.llm.*` / `stable.system.*` 仅 service 身份可写。
    // 身份由认证中间件注入：认证启用时必注入（User/Service）；identity 为 None
    // 即认证禁用（loopback 开发模式），按放行处理（开发模式语义不变）。
    if requires_service_identity(&req.path)
        && matches!(identity, Some(Extension(CallerIdentity::User)))
    {
        tracing::warn!(path = %req.path, "update_payload 受保护域写入被拒绝（需 service 身份）");
        return Ok((
            StatusCode::FORBIDDEN,
            Json(ApiResponse {
                success: false,
                message: format!(
                    "写入受保护域 {} 被拒绝：stable.llm / stable.system 仅受信服务管道可写。\
                     服务端需配置 EVORULE_SERVICE_TOKEN，调用方（如 evo-agent）需携带该 service token。",
                    req.path
                ),
                fact_id: None,
            }),
        ));
    }

    // Phase 1: 第一层输入净化（静默改写 Prompt 注入内容）
    let (sanitized_value, sanitize_report) = sanitizer.sanitize_value(&req.value);
    if sanitize_report.has_hits() {
        // P5-A1：命中指标化（按 rule）
        for rule in sanitize_report.unique_hits() {
            metrics.inc_sanitize_hits(rule);
        }
        tracing::warn!(
            hits = ?sanitize_report.unique_hits(),
            hit_count = sanitize_report.hit_count(),
            path = %req.path,
            "update_payload 输入净化命中（已静默改写）"
        );
    }
    let value = serde_to_tcb(sanitized_value);

    match api.send_payload_update(req.path, value) {
        Ok(id) => Ok((
            StatusCode::OK,
            Json(ApiResponse {
                success: true,

                message: "PayloadUpdate submitted".to_string(),

                fact_id: Some(id.0),
            }),
        )),

        Err(msg) => Ok((
            StatusCode::OK,
            Json(ApiResponse {
                success: false,

                message: msg,

                fact_id: None,
            }),
        )),
    }
}

/// 获取状态快照 handler

#[utoipa::path(

    get,

    path = "/api/state",

    tag = "governance",

    responses(

        (status = 200, description = "状态快照（payload + 指令队列 + 版本号）", body = StateResponse),

        (status = 401, description = "未认证")

    )

)]

async fn get_state(State(api): State<GovernanceApi>) -> Json<serde_json::Value> {
    let (payload, queue, version) = api.snapshot();

    let mut obj = serde_json::Map::new();

    obj.insert("payload".to_string(), tcb_to_serde(&payload));

    obj.insert(
        "queue".to_string(),
        serde_json::Value::Array(queue.iter().map(tcb_to_serde).collect()),
    );

    obj.insert(
        "version".to_string(),
        serde_json::Value::Number(version.into()),
    );

    // 同步审计

    api.audit_new().await;

    Json(serde_json::Value::Object(obj))
}

/// 获取审计报告 handler

#[utoipa::path(

    get,

    path = "/api/audit",

    tag = "governance",

    responses(

        (status = 200, description = "审计报告 JSON（entry_count / last_hash / entries）"),

        (status = 401, description = "未认证")

    )

)]

async fn get_audit(
    State(api): State<GovernanceApi>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    api.audit_new().await;

    let report = api.audit_report().await.map_err(|e| {
        tracing::error!(error = %e, "audit report failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    match serde_json::from_str::<serde_json::Value>(&report) {
        Ok(json) => Ok(Json(json)),

        Err(_) => Ok(Json(serde_json::Value::String(report))),
    }
}

// ===== 平台认证事件报表=====

/// 平台认证事件条目
#[derive(Debug, Serialize, ToSchema)]
pub struct PlatformEventEntry {
    /// 共享事实 ID（链序 = 写入时间序）
    pub fact_id: u64,
    /// 事实路径（完整,取证定位用）
    pub path: String,
    /// 事件类型（login_success / user_created / role_updated / ...）
    pub kind: String,
    /// 事件时间（Unix 毫秒,自路径内嵌时间戳解析;畸形路径为 null）
    pub ts_ms: Option<u64>,
    /// 事件详情（kind 特定:username / role / by / ...）
    pub detail: serde_json::Value,
}

/// 平台认证事件查询参数
#[derive(Debug, Deserialize, ToSchema, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PlatformEventsQuery {
    /// 可选:按事件类型过滤（如 login_failed）
    pub kind: Option<String>,
    /// 可选:最多返回条数（链序取前 N;缺省全量）
    pub limit: Option<usize>,
}

/// `GET /api/audit/platform-events` — 平台认证事件报表
///
/// 自 SharedFactsLog 读取 `platform.event.*` 事实（append-only,随共享 WAL
/// 入 prev_hash 链,`platform_auth::append_auth_event` 写入）,按链序返回。
/// 路径形态 `platform.event.{kind}.{unix_ms}{随机后缀}`（kind 不含点）。
#[utoipa::path(

    get,

    path = "/api/audit/platform-events",

    tag = "governance",

    params(PlatformEventsQuery),

    responses(

        (status = 200, description = "平台认证事件列表（fact_id 链序,total 为过滤后总数）"),

        (status = 401, description = "未认证")

    )

)]

async fn platform_events_handler(
    State(shared): State<SharedFactsLog>,
    Query(q): Query<PlatformEventsQuery>,
) -> Json<serde_json::Value> {
    const EVENT_PREFIX: &str = "platform.event.";
    let mut facts = shared.facts_by_path_prefix(EVENT_PREFIX);
    // 链序口径:fact_id 升序 = 写入时间序(底层 facts_by_path_prefix 返回序非链序,显式排序)
    facts.sort_by_key(|f| f.fact_id.0);
    let mut events: Vec<PlatformEventEntry> = Vec::with_capacity(facts.len());
    for f in facts {
        let path = f.path;
        // rest = "{kind}.{unix_ms}{随机后缀}";kind 不含点,畸形路径如实降级(ts_ms=null)
        let rest = match path.strip_prefix(EVENT_PREFIX) {
            Some(r) => r,
            None => continue,
        };
        let (kind, ms_suffix) = match rest.split_once('.') {
            Some((k, m)) => (k.to_string(), m),
            None => (rest.to_string(), ""),
        };
        if let Some(want) = q.kind.as_deref() {
            if kind != want {
                continue;
            }
        }
        let ts_ms = ms_suffix
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse::<u64>()
            .ok();
        let value = tcb_to_serde(&f.value);
        events.push(PlatformEventEntry {
            fact_id: f.fact_id.0,
            path,
            kind,
            ts_ms,
            detail: value
                .get("detail")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        });
    }
    let total = events.len();
    if let Some(n) = q.limit {
        events.truncate(n);
    }
    Json(serde_json::json!({ "events": events, "total": total }))
}

// ===== 会话管理路由（多反应器实例模式）=====

/// 创建会话 handler
///
/// `POST /api/sessions` → 创建新的长驻反应器实例，返回 session_id
/// 超过最大会话数时返回 429 Too Many Requests
#[utoipa::path(

    post,

    path = "/api/sessions",

    tag = "sessions",

    responses(

        (status = 200, description = "会话创建成功，返回 session_id", body = SessionIdResponse),

        (status = 429, description = "超过最大会话数"),

        (status = 500, description = "创建失败")

    )

)]
// 会话创建主路径:参数校验/配额/规则装载/WAL 初始化串联,拆函数需传递 6+ 状态,
// 详见 GATE_REFERENCE.md §六(豁免索引)
#[allow(clippy::cognitive_complexity)]
async fn create_session(
    State(api): State<SessionApi>,

    State(metrics): State<SharedMetrics>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let result = {
        let sessions = api.sessions.lock().await;

        sessions.create_session()
    };

    match result {
        Ok(id) => {
            metrics.inc_sessions(); // 会话数 +1

            // spawn hit-stats 归因记录任务
            api.spawn_hit_stats_recorder(id);

            // 为新 session 的 reactor spawn IoSubscriber

            // 没有 IoSubscriber 时，session 的 IoRequest 会 60s 超时

            if let Some(ref dispatcher) = api.dispatcher {
                let sessions = api.sessions.lock().await;

                if let Some(session) = sessions.get_session(id) {
                    let event_rx = session.event_tx.subscribe();

                    let command_tx = session.command_tx.clone();

                    let subscriber = IoSubscriber::new(dispatcher.clone())
                        .with_metrics(metrics.clone())
                        .with_skip(Arc::new(is_external_executor_request));

                    tokio::spawn(async move {
                        if let Err(e) = subscriber.run(event_rx, command_tx).await {
                            tracing::error!(

                                session_id = id,

                                error = %e,

                                "Session IoSubscriber 异常退出"

                            );
                        }
                    });

                    tracing::info!(session_id = id, "IoSubscriber 已为 session 启动");
                }
            }

            Ok(Json(serde_json::json!({

                "session_id": id,

                "message": "Session created"

            })))
        }

        Err(evorule_governance::session::SessionError::LimitExceeded { current, max }) => {
            tracing::warn!(current, max, "Session creation rejected: limit exceeded");

            Err(StatusCode::TOO_MANY_REQUESTS)
        }

        Err(evorule_governance::session::SessionError::WalUnavailable { session_id, source }) => {
            tracing::error!(
                session_id,
                error = %source,
                "Session creation rejected: WAL unavailable (audit chain cannot be established)"
            );

            Err(StatusCode::SERVICE_UNAVAILABLE)
        }

        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// 列出所有会话 handler
///
/// `GET /api/sessions` → 返回所有活跃会话 ID
#[utoipa::path(

    get,

    path = "/api/sessions",

    tag = "sessions",

    responses(

        (status = 200, description = "活跃会话 ID 列表", body = SessionListResponse)

    )

)]

async fn list_sessions(
    State(api): State<SessionApi>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let list = {
        let sessions = api.sessions.lock().await;

        sessions.list_sessions()
    };

    Ok(Json(serde_json::json!({

        "sessions": list

    })))
}

/// 获取单会话元数据（GET /api/sessions/{id}，014 合法 API #1）
///
/// 补齐跨会话因果追溯所需的父链信息：parent_session_id / initial_content_hash，
/// 以及活跃度（idle_secs）、阶段、自动验证等运行时状态。
#[utoipa::path(

    get,

    path = "/api/sessions/{id}",

    tag = "sessions",

    params(("id" = u64, Path, description = "会话 ID")),

    responses(

        (status = 200, description = "会话元数据", body = SessionMetadataResponse),

        (status = 404, description = "会话不存在")

    )

)]

async fn session_metadata(
    State(api): State<SessionApi>,

    Path(session_id): Path<u64>,
) -> Result<Json<SessionMetadataResponse>, StatusCode> {
    let sessions = api.sessions.lock().await;

    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(SessionMetadataResponse {
        session_id,

        parent_session_id: session.parent_session_id(),

        initial_content_hash: session.initial_content_hash().map(String::from),

        content_hash_scheme: match session.content_hash_scheme() {
            evorule_governance::session::ContentHashScheme::AuditChainCompact => {
                Some("audit_chain_compact")
            }
            evorule_governance::session::ContentHashScheme::TcbDisplay => Some("tcb_display"),
        },

        idle_secs: session.last_activity().elapsed().as_secs_f64(),

        is_finished: session.is_finished(),

        phase: session.current_phase().map(|p| p.as_str().to_string()),

        auto_verify: session.is_auto_verify_enabled(),
    }))
}

/// 回收已结束 / 已过期会话（POST /api/sessions/reap，014 合法 API #5）

#[utoipa::path(

    post,

    path = "/api/sessions/reap",

    tag = "sessions",

    responses(

        (status = 200, description = "回收结果", body = ReapResponse)

    )

)]

async fn session_reap(State(api): State<SessionApi>) -> Result<Json<ReapResponse>, StatusCode> {
    // ①: 手动回收与后台 reaper 走同一 reap_once——生产会话保活 +
    // 失忆自愈(否则手动触发 reap 可绕过保护,把 TTL 到期的生产会话回收成幻影)
    let (finished, expired) = reap_once(&api.sessions, api.workspace_db.as_ref(), Some(&api)).await;

    Ok(Json(ReapResponse {
        finished,

        expired,

        total: finished + expired,
    }))
}

/// 关闭会话 handler
///
/// `DELETE /api/sessions/:id` → 关闭指定会话，反应器优雅退出
#[utoipa::path(

    delete,

    path = "/api/sessions/{id}",

    tag = "sessions",

    params(("id" = u64, Path, description = "会话 ID")),

    responses(

        (status = 200, description = "会话已关闭", body = SessionIdResponse),

        (status = 409, description = "拒绝关闭：该会话是生产会话（production_state.current_session_id 引用中），须走治理发布流切换或重启 server 重建"),

        (status = 404, description = "会话不存在")

    )

)]

async fn close_session(
    State(api): State<SessionApi>,

    State(metrics): State<SharedMetrics>,

    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // ①b: 生产会话删除保护(fail-fast + 自诊断指引)。
    // DELETE 恰指向 production_state.current_session_id 时,删除后引用成
    // 幻影(监控大屏轮询 404、沙盒 fork 404),直到重启才被 重建。
    // 生产会话生命周期归治理链管辖(rolling_session 切换/server 退出导出),
    // 不开放裸删除;确需重置请走治理发布流切换,或重启 server(触发 重建)。
    // 注: rolling_session 对旧生产会话的回收走内部 drain+close(SessionOps
    // trait,switch 之后才关闭),不经本 HTTP 端点,治理链不受此保护影响。
    if let Some(ws_db) = &api.workspace_db {
        match ws_db.get_production_state() {
            Ok(ps) if ps.current_session_id == Some(session_id as i64) => {
                tracing::warn!(
                    session_id,
                    ": 拒绝删除生产会话(production_state.current_session_id 引用中)"
                );
                return Err(StatusCode::CONFLICT);
            }
            Ok(_) => {}
            Err(e) => {
                // fail-closed: 读不到状态 = 无法排除是生产会话,保守拒绝
                tracing::error!(
                    error = %e,
                    session_id,
                    ": close_session 读取 production_state 失败,保守拒绝删除"
                );
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    }

    let result = {
        let sessions = api.sessions.lock().await;

        sessions.close_session(session_id)
    };

    match result {
        Ok(_) => {
            metrics.dec_sessions(); // 会话数 -1

            Ok(Json(serde_json::json!({

                "session_id": session_id,

                "message": "Session closed"

            })))
        }

        Err(_) => Err(StatusCode::NOT_FOUND),
    }
}

/// 从父会话创建子会话 handler
///
///
/// `POST /api/sessions/from/:parent_id` → 基于父会话创建新会话，
///
/// 记录跨会话因果关系（父会话 ID + 初始内容哈希）
///
#[derive(serde::Deserialize, utoipa::ToSchema)]

pub struct CreateSessionFromParentParams {
    /// 从父会话的指定版本派生（默认最新版本）
    pub version: Option<u64>,
}

#[utoipa::path(

    post,

    path = "/api/sessions/from/{parent_id}",

    tag = "sessions",

    params(

        ("parent_id" = u64, Path, description = "父会话 ID"),

        ("version" = Option<u64>, Query, description = "从父会话的指定版本派生（默认最新）")

    ),

    responses(

        (status = 200, description = "子会话创建成功", body = SessionForkResponse),

        (status = 404, description = "父会话不存在"),

        (status = 429, description = "超过最大会话数"),

        (status = 400, description = "版本无效")

    )

)]
// 派生会话创建:继承校验+版本语义,同 create_session 拆分受限
#[allow(clippy::cognitive_complexity)]
async fn create_session_from_parent(
    State(api): State<SessionApi>,

    State(metrics): State<SharedMetrics>,

    Path(parent_id): Path<u64>,

    Query(params): Query<CreateSessionFromParentParams>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let result = {
        let sessions = api.sessions.lock().await;

        sessions.create_session_from_parent_at_version(parent_id, params.version)
    };

    match result {
        Ok(id) => {
            metrics.inc_sessions();

            // spawn hit-stats 归因记录任务
            api.spawn_hit_stats_recorder(id);

            Ok(Json(serde_json::json!({

                "session_id": id,

                "parent_session_id": parent_id,

                "message": "Session created from parent",

                "forked_from_version": params.version

            })))
        }

        Err(evorule_governance::session::SessionError::NotFound { id }) => {
            tracing::warn!(parent_id = id, "Parent session not found");

            Err(StatusCode::NOT_FOUND)
        }

        Err(evorule_governance::session::SessionError::LimitExceeded { current, max }) => {
            tracing::warn!(current, max, "Session creation rejected: limit exceeded");

            Err(StatusCode::TOO_MANY_REQUESTS)
        }

        Err(evorule_governance::session::SessionError::InvalidVersion { version }) => {
            tracing::warn!(version, "Invalid version for session fork");

            Err(StatusCode::BAD_REQUEST)
        }

        Err(evorule_governance::session::SessionError::WalUnavailable { session_id, source }) => {
            tracing::error!(
                session_id,
                error = %source,
                "Session creation rejected: WAL unavailable (audit chain cannot be established)"
            );

            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

#[derive(serde::Deserialize, utoipa::ToSchema)]

pub struct CreateSessionForkParams {
    /// fork 时的父会话版本（必填）
    pub version: Option<u64>,
}

#[utoipa::path(

    post,

    path = "/api/sessions/fork/{parent_id}",

    tag = "sessions",

    params(

        ("parent_id" = u64, Path, description = "父会话 ID"),

        ("version" = u64, Query, description = "fork 时的父会话版本（必填）")

    ),

    responses(

        (status = 200, description = "fork 成功", body = SessionForkResponse),

        (status = 404, description = "父会话不存在"),

        (status = 429, description = "超过最大会话数"),

        (status = 400, description = "缺少/无效版本")

    )

)]
// fork 会话创建:继承校验+版本语义,同 create_session 拆分受限
#[allow(clippy::cognitive_complexity)]
async fn create_session_fork(
    State(api): State<SessionApi>,

    State(metrics): State<SharedMetrics>,

    Path(parent_id): Path<u64>,

    Query(params): Query<CreateSessionForkParams>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let version = params.version.ok_or(StatusCode::BAD_REQUEST)?;

    let result = {
        let sessions = api.sessions.lock().await;

        sessions.create_session_from_parent_at_version(parent_id, Some(version))
    };

    match result {
        Ok(id) => {
            metrics.inc_sessions();

            // spawn hit-stats 归因记录任务
            api.spawn_hit_stats_recorder(id);

            Ok(Json(serde_json::json!({

                "session_id": id,

                "parent_session_id": parent_id,

                "forked_from_version": version,

                "message": "Session forked from parent at specified version"

            })))
        }

        Err(evorule_governance::session::SessionError::NotFound { id }) => {
            tracing::warn!(parent_id = id, "Parent session not found for fork");

            Err(StatusCode::NOT_FOUND)
        }

        Err(evorule_governance::session::SessionError::LimitExceeded { current, max }) => {
            tracing::warn!(current, max, "Session fork rejected: limit exceeded");

            Err(StatusCode::TOO_MANY_REQUESTS)
        }

        Err(evorule_governance::session::SessionError::InvalidVersion { version }) => {
            tracing::warn!(version, "Invalid version for session fork");

            Err(StatusCode::BAD_REQUEST)
        }

        Err(evorule_governance::session::SessionError::WalUnavailable { session_id, source }) => {
            tracing::error!(
                session_id,
                error = %source,
                "Session fork rejected: WAL unavailable (audit chain cannot be established)"
            );

            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

/// 会话命令提交 handler
///
/// `POST /api/sessions/:id/command` → 提交命令到指定会话的反应器
#[utoipa::path(

    post,

    path = "/api/sessions/{id}/command",

    tag = "sessions",

    params(("id" = u64, Path, description = "会话 ID")),

    request_body = CommandRequest,

    responses(

        (status = 200, description = "命令已提交，返回 fact_id", body = ApiResponse),

        (status = 404, description = "会话不存在")

    )

)]

async fn session_command(
    State(api): State<SessionApi>,

    State(metrics): State<SharedMetrics>,
    State(sanitizer): State<Arc<InputSanitizer>>,
    Path(session_id): Path<u64>,

    Json(req): Json<CommandRequest>,
) -> Result<Json<ApiResponse>, StatusCode> {
    // Phase 1: 第一层输入净化（静默改写 Prompt 注入内容）
    let (instruction_value, sanitize_report) = sanitizer.sanitize_value(&req.instruction);
    if sanitize_report.has_hits() {
        // P5-A1：命中指标化（按 rule）
        for rule in sanitize_report.unique_hits() {
            metrics.inc_sanitize_hits(rule);
        }
        tracing::warn!(
            hits = ?sanitize_report.unique_hits(),
            hit_count = sanitize_report.hit_count(),
            session_id = session_id,
            "session_command 输入净化命中（已静默改写）"
        );
    }

    // 按指令类型计数

    {
        let cmd_type = instruction_value
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        metrics.inc_commands(cmd_type);
    }

    // Opt3：指令层结构门禁（线1 防御层, records/77）。
    // 提交即校验（双层语言分派：元指令层 transform_rule / 指令层 instruction），
    // 拦截单数 __io_result__、坏路径、非法控制流结构——避免"提交照常、运行时才崩溃"。
    // 拒绝返回 success:false + 明确原因，指令不进引擎（TCB 只保证确定性执行，不保证正确性）。
    let schema_report = evorule_rule_schema::validate_command_instruction(&instruction_value);
    if !schema_report.valid {
        let detail = schema_report.errors.join("; ");
        tracing::warn!(
            session_id = session_id,
            "session_command 未通过 Schema 门禁，已拒绝提交: {detail}"
        );
        return Ok(Json(ApiResponse {
            success: false,
            message: format!(
                "指令未通过 Schema 门禁（引擎原生结构非法，参见固化 rule_set v1.0 Schema，records/77）: {detail}"
            ),
            fact_id: None,
        }));
    }

    let id = api.next_id();
    let instruction = serde_to_tcb(instruction_value);

    let sessions = api.sessions.lock().await;

    sessions.touch_session(session_id);

    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    match session.command_tx.send(Fact::Command { id, instruction }) {
        Ok(()) => {
            // 缺口2 修复: 命令提交后实时刷新审计链 (BLAKE3 哈希链)

            session.audit_new();

            Ok(Json(ApiResponse {
                success: true,

                message: "Command submitted".to_string(),

                fact_id: Some(id.0),
            }))
        }

        Err(_) => Ok(Json(ApiResponse {
            success: false,

            message: "Command channel closed (reactor exited)".to_string(),

            fact_id: None,
        })),
    }
}

/// 会话状态查询 handler
///
/// `GET /api/sessions/:id/state` → 返回指定会话的状态快照
#[utoipa::path(

    get,

    path = "/api/sessions/{id}/state",

    tag = "sessions",

    params(("id" = u64, Path, description = "会话 ID")),

    responses(

        (status = 200, description = "会话状态快照", body = SessionStateResponse),

        (status = 404, description = "会话不存在")

    )

)]

async fn session_state(
    State(api): State<SessionApi>,

    State(metrics): State<SharedMetrics>,

    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;

    sessions.touch_session(session_id);

    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    let (payload, queue, version) = session.facts_log.snapshot();

    metrics.set_facts_log_version(version);

    let mut reactor_obj = serde_json::Map::new();

    reactor_obj.insert(
        "phase".into(),
        session
            .handle
            .current_phase()
            .map(|p| serde_json::Value::String(p.as_str().to_string()))
            .unwrap_or(serde_json::Value::Null),
    );

    reactor_obj.insert(
        "causal_depth".into(),
        session
            .handle
            .causal_depth()
            .map(|d| serde_json::Value::Number(d.into()))
            .unwrap_or(serde_json::Value::Null),
    );

    reactor_obj.insert(
        "structural_invariant_violations".into(),
        serde_json::Value::Number(session.handle.structural_invariant_violations().into()),
    );

    reactor_obj.insert(
        "pending_io_count".into(),
        session
            .handle
            .pending_io_count()
            .map(|c| serde_json::Value::Number(c.into()))
            .unwrap_or(serde_json::Value::Null),
    );

    reactor_obj.insert(
        "current_step".into(),
        session
            .handle
            .current_step()
            .map(|s| serde_json::Value::Number(s.into()))
            .unwrap_or(serde_json::Value::Null),
    );

    let mut obj = serde_json::Map::new();

    obj.insert("payload".into(), tcb_to_serde(&payload));

    obj.insert(
        "queue".into(),
        serde_json::Value::Array(queue.iter().map(tcb_to_serde).collect()),
    );

    obj.insert("version".into(), serde_json::Value::Number(version.into()));

    obj.insert("reactor".into(), serde_json::Value::Object(reactor_obj));

    Ok(Json(serde_json::Value::Object(obj)))
}

/// 会话审计报告 handler
///
/// `GET /api/sessions/:id/audit` → 返回指定会话的审计报告
#[utoipa::path(

    get,

    path = "/api/sessions/{id}/audit",

    tag = "sessions",

    params(
        ("id" = u64, Path, description = "会话 ID"),
        ("include_content" = Option<bool>, Query, description = "F5: 为 true 时每条审计条目附加 content_json（完整 Fact 内容 JSON，含 IoRequest params / IoResponse result 等）；默认 false 保持轻量响应")
    ),

    responses(

        (status = 200, description = "审计报告（含 fact_count / last_hash / verified / entries）", body = AuditResponse),

        (status = 404, description = "会话不存在")

    )

)]

async fn session_audit(
    State(api): State<SessionApi>,

    Path(session_id): Path<u64>,

    axum::extract::Query(query): axum::extract::Query<AuditReportQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;

    sessions.touch_session(session_id);

    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    // 先审计新事实

    let _new_count = session.audit_new();

    let report_str = session.audit_report().map_err(|e| {
        tracing::error!(session_id, "audit report failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let report: serde_json::Value = serde_json::from_str(&report_str).map_err(|e| {
        tracing::error!(session_id, "audit report parse failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // F5（audit-chain 专项 2026-08-28）：按需注入完整 Fact 内容。
    // Auditor 自持条目只含哈希/因果元数据（P1-F4 设计），不存内容——
    // 外部审计者此前无法从 API 重建"LLM 看到什么/回了什么"（P3-N1）。
    // include_content=true 时从 FactsLog 内存投影按 fact_id 关联，
    // 注入 content_json（Fact::to_json 完整内容）。默认关闭，保持轻量。

    let content_index: std::collections::BTreeMap<u64, serde_json::Value> =
        if query.include_content.unwrap_or(false) {
            session
                .facts_log
                .history()
                .iter()
                .map(|f| (f.id().0, tcb_to_serde(&f.to_json())))
                .collect()
        } else {
            std::collections::BTreeMap::new()
        };

    // 重新映射字段名以符合 API 文档规范

    let mut normalized = serde_json::Map::new();

    normalized.insert("session_id".into(), serde_json::json!(session_id));

    normalized.insert(
        "fact_count".into(),
        report
            .get("entry_count")
            .cloned()
            .unwrap_or(serde_json::json!(0)),
    );

    normalized.insert(
        "last_hash".into(),
        report
            .get("last_hash")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    );

    normalized.insert("verified".into(), serde_json::json!(session.audit_verify()));

    let mut entries = report
        .get("entries")
        .cloned()
        .unwrap_or(serde_json::json!([]));

    if !content_index.is_empty() {
        if let Some(arr) = entries.as_array_mut() {
            for entry in arr.iter_mut() {
                if let Some(fid) = entry.get("fact_id").and_then(|v| v.as_u64()) {
                    if let Some(content) = content_index.get(&fid) {
                        if let Some(obj) = entry.as_object_mut() {
                            obj.insert("content_json".into(), content.clone());
                        }
                    }
                }
            }
        }
    }

    normalized.insert("entries".into(), entries);

    Ok(Json(serde_json::Value::Object(normalized)))
}

/// F5：审计报告查询参数
#[derive(Debug, Clone, Default, serde::Deserialize)]

pub struct AuditReportQuery {
    /// 为 true 时每条审计条目附加 content_json（完整 Fact 内容）
    #[serde(default)]
    pub include_content: Option<bool>,
}

/// 审计档案会话列表 handler
///
/// `GET /api/audit-archive/sessions` → wal_dir 下全部历史会话档案（只读）。
/// 与活跃会话 API 物理隔离：本端点纯只读，无 touch/写路径；
/// 活跃会话仍在列表中标注（前端活跃列表优先走 /api/sessions 实时端点）。
#[utoipa::path(
    get,
    path = "/api/audit-archive/sessions",
    tag = "sessions",
    responses(
        (status = 200, description = "历史会话档案元数据列表（含 LLM 侧车标记与 audit_purpose）"),
        (status = 500, description = "档案目录扫描失败")
    )
)]
async fn archive_sessions(
    State(api): State<SessionApi>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // 先取活跃会话 ID（不持 std 锁跨 await）
    let active_ids: HashSet<u64> = {
        let sessions = api.sessions.lock().await;
        sessions.list_sessions().into_iter().collect()
    };

    let mut cache = api
        .archive_cache
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let metas = cache.list();

    Ok(Json(serde_json::json!({
        "sessions": metas,
        "active_session_ids": active_ids,
    })))
}

/// 审计档案单会话审计 handler
///
/// `GET /api/audit-archive/sessions/{id}/audit?include_content=true`
/// → 从 WAL 重建该历史会话的审计链（活跃会话审计同形响应）。
/// `include_content=true` 时注入 `content_json`（完整 Fact 内容，
/// 含 LLM prompt 全文 / io_response 结果全文）。只读：不 touch、不写 WAL。
#[utoipa::path(
    get,
    path = "/api/audit-archive/sessions/{id}/audit",
    tag = "sessions",
    params(("id" = u64, Path, description = "会话 ID")),
    responses(
        (status = 200, description = "重建的审计链（verified=false 表示检测到篡改/损坏，如实上报）"),
        (status = 404, description = "无该会话档案或 wal_dir 未启用"),
        (status = 500, description = "WAL 读取失败")
    )
)]
async fn archive_session_audit(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
    axum::extract::Query(query): axum::extract::Query<AuditReportQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let cache = api
        .archive_cache
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    match cache.read_audit(session_id, query.include_content.unwrap_or(false)) {
        Ok(v) => Ok(Json(v)),
        Err(audit_archive::ArchiveError::NotFound(_))
        | Err(audit_archive::ArchiveError::WalDisabled) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!(session_id, "audit archive read failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// 会话审计链验证 handler
///
/// `GET /api/sessions/:id/audit/verify` → 验证指定会话的审计链完整性
#[utoipa::path(

    get,

    path = "/api/sessions/{id}/audit/verify",

    tag = "sessions",

    params(("id" = u64, Path, description = "会话 ID")),

    responses(

        (status = 200, description = "验证结果，返回 verified 标志", body = AuditVerifyResponse),

        (status = 404, description = "会话不存在")

    )

)]

async fn session_audit_verify(
    State(api): State<SessionApi>,

    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;

    sessions.touch_session(session_id);

    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    let _new_count = session.audit_new();

    let valid = session.audit_verify();

    // 获取审计报告以提取更多信息

    let report_str = session.audit_report().map_err(|e| {
        tracing::error!(session_id, "audit report failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let report: serde_json::Value = serde_json::from_str(&report_str).map_err(|e| {
        tracing::error!(session_id, "audit report parse failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(Json(serde_json::json!({

        "verified": valid,

        "session_id": session_id,

        "fact_count": report.get("entry_count").cloned().unwrap_or(serde_json::Value::Null),

        "last_hash": report.get("last_hash").cloned().unwrap_or(serde_json::Value::Null),

    })))
}

/// 会话因果链查询 handler
///
/// `GET /api/sessions/:id/audit/causal/:fact_id` → 追溯指定 Fact 的因果链
#[utoipa::path(

    get,

    path = "/api/sessions/{id}/audit/causal/{fact_id}",

    tag = "sessions",

    params(

        ("id" = u64, Path, description = "会话 ID"),

        ("fact_id" = u64, Path, description = "事实 ID")

    ),

    responses(

        (status = 200, description = "因果链，含 chain 数组", body = CausalChainResponse),

        (status = 404, description = "会话不存在")

    )

)]

async fn session_causal_chain(
    State(api): State<SessionApi>,

    Path((session_id, fact_id)): Path<(u64, u64)>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    use evorule_reactor::FactId;

    let sessions = api.sessions.lock().await;

    sessions.touch_session(session_id);

    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    let _new_count = session.audit_new();

    let chain = session.causal_chain(FactId(fact_id));

    let entries: Vec<serde_json::Value> = chain
        .iter()
        .map(|e| {
            serde_json::json!({

                "fact_id": e.fact_id.0,

                "fact_type": e.fact_type,

                "logical_time": e.logical_time,

                "content_hash": e.content_hash,

                "prev_hash": e.prev_hash,

                "cause": e.cause.map(|c| c.0),

            })
        })
        .collect();

    Ok(Json(serde_json::json!({

        "session_id": session_id,

        "fact_id": fact_id,

        "chain_length": entries.len(),

        "chain": entries,

    })))
}

/// 会话审计链导出 handler（P04）
///
/// `GET /api/sessions/:id/audit/export` → 导出指定会话的审计链为 JSON
///
/// 返回包含完整哈希链的审计数据，可用于跨实例迁移、离线分析或备份。
/// 导出是只读操作，使用 GET 方法语义更合适。
#[utoipa::path(

    get,

    path = "/api/sessions/{id}/audit/export",

    tag = "sessions",

    params(("id" = u64, Path, description = "会话 ID")),

    responses(

        (status = 200, description = "审计链 JSON（完整哈希链数据）"),

        (status = 404, description = "会话不存在")

    )

)]

async fn session_audit_export(
    State(api): State<SessionApi>,

    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;

    sessions.touch_session(session_id);

    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    // 先审计新事实，确保导出包含最新条目

    let _new_count = session.audit_new();

    let export_str = session.audit_export().map_err(|e| {
        tracing::error!(session_id, "audit export failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let export: serde_json::Value = serde_json::from_str(&export_str).map_err(|e| {
        tracing::error!(session_id, "audit export parse failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(Json(export))
}

/// 会话审计链导入 handler（P04）
///
/// `POST /api/sessions/:id/audit/import` → 导入外部审计链数据
///
/// **安全注意事项**：
/// 1. 导入操作会**完全覆盖**当前会话的审计链，具有破坏性
/// 2. 应仅允许管理员或授权用户调用此接口
/// 3. 建议在调用前验证导入数据的来源和完整性
/// 4. 导入后会自动调用 `verify` 验证审计链完整性
///
/// # 返回
/// - `200 OK`：导入成功且审计链验证通过
/// - `202 Accepted`：导入成功但审计链验证失败（数据可能已损坏）
/// - `400 Bad Request`：JSON 解析失败或字段缺失
/// - `404 Not Found`：会话不存在
#[utoipa::path(

    post,

    path = "/api/sessions/{id}/audit/import",

    tag = "sessions",

    params(("id" = u64, Path, description = "会话 ID")),

    request_body = serde_json::Value,

    responses(

        (status = 200, description = "导入成功且验证通过", body = AuditImportResponse),

        (status = 202, description = "导入成功但验证失败", body = AuditImportResponse),

        (status = 400, description = "JSON 解析失败或字段缺失"),

        (status = 404, description = "会话不存在")

    )

)]

async fn session_audit_import(
    State(api): State<SessionApi>,

    Path(session_id): Path<u64>,

    Json(data): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;

    sessions.touch_session(session_id);

    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    let json_str = serde_json::to_string(&data).map_err(|_| StatusCode::BAD_REQUEST)?;

    let (import_ok, verify_ok) = session.audit_import(&json_str);

    if !import_ok {
        return Err(StatusCode::BAD_REQUEST);
    }

    let status = if verify_ok { "ok" } else { "verify_failed" };

    if !verify_ok {
        tracing::warn!(
            session_id = session_id,
            "session_audit_import: 导入成功但审计链验证失败"
        );
    }

    Ok(Json(serde_json::json!({

        "session_id": session_id,

        "imported": import_ok,

        "verify_ok": verify_ok,

        "status": status,

    })))
}

/// 会话审计链压缩导出 handler（P05）
///
/// `GET /api/sessions/:id/audit/export/compressed` → 返回 gzip 压缩的审计链
///
/// 返回 `application/gzip` 二进制数据，体积通常为 JSON 格式的 5-10%。
/// 适用于网络传输受限或大批量迁移场景。
#[utoipa::path(

    get,

    path = "/api/sessions/{id}/audit/export/compressed",

    tag = "sessions",

    params(("id" = u64, Path, description = "会话 ID")),

    responses(

        (status = 200, description = "gzip 压缩的审计链（application/gzip 二进制）"),

        (status = 404, description = "会话不存在"),

        (status = 500, description = "压缩失败")

    )

)]

async fn session_audit_export_compressed(
    State(api): State<SessionApi>,

    Path(session_id): Path<u64>,
) -> Result<Response, StatusCode> {
    let sessions = api.sessions.lock().await;

    sessions.touch_session(session_id);

    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    // 先审计新事实，确保导出包含最新条目

    let _new_count = session.audit_new();

    let compressed = session.audit_export_compressed();

    if compressed.is_empty() {
        tracing::warn!(
            session_id = session_id,
            "session_audit_export_compressed: 压缩失败"
        );

        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/gzip"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"audit_chain.json.gz\"",
            ),
        ],
        compressed,
    )
        .into_response())
}

/// 会话审计链压缩导入 handler（P05）
///
/// `POST /api/sessions/:id/audit/import/compressed` → 导入 gzip 压缩的审计链
///
/// 请求体为 gzip 二进制数据（`Content-Type: application/gzip`）。
/// 解压后等价于 [`session_audit_import`]，导入成功后自动调用 `verify`。
///
/// **安全注意事项** 与 [`session_audit_import`] 相同。
#[utoipa::path(

    post,

    path = "/api/sessions/{id}/audit/import/compressed",

    tag = "sessions",

    params(("id" = u64, Path, description = "会话 ID")),

    request_body(content = inline(String), description = "gzip 压缩的审计链二进制（application/gzip）"),

    responses(

        (status = 200, description = "导入成功且验证通过", body = AuditImportResponse),

        (status = 202, description = "导入成功但验证失败", body = AuditImportResponse),

        (status = 400, description = "请求体为空或解析失败"),

        (status = 404, description = "会话不存在")

    )

)]

async fn session_audit_import_compressed(
    State(api): State<SessionApi>,

    Path(session_id): Path<u64>,

    body: Bytes,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;

    sessions.touch_session(session_id);

    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    if body.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let (import_ok, verify_ok) = session.audit_import_compressed(&body);

    if !import_ok {
        return Err(StatusCode::BAD_REQUEST);
    }

    let status = if verify_ok { "ok" } else { "verify_failed" };

    if !verify_ok {
        tracing::warn!(
            session_id = session_id,
            "session_audit_import_compressed: 导入成功但审计链验证失败"
        );
    }

    Ok(Json(serde_json::json!({

        "session_id": session_id,

        "imported": import_ok,

        "verify_ok": verify_ok,

        "status": status,

        "format": "gzip",

    })))
}

/// 会话 PayloadUpdate handler
///
/// `POST /api/sessions/:id/payload` → 更新指定会话的 payload 字段
#[utoipa::path(

    post,

    path = "/api/sessions/{id}/payload",

    tag = "sessions",

    params(("id" = u64, Path, description = "会话 ID")),

    request_body = PayloadUpdateRequest,

    responses(

        (status = 200, description = "PayloadUpdate 已提交，返回 fact_id", body = ApiResponse),

        (status = 403, description = "受保护域写入被拒绝（需 service 身份，B5-server）"),

        (status = 404, description = "会话不存在")

    )

)]
// payload 读写路径:权限/保护域/版本分支多,详见 GATE_REFERENCE.md §六(豁免索引)
#[allow(clippy::cognitive_complexity)]
async fn session_payload(
    State(api): State<SessionApi>,

    State(shared_facts): State<SharedFactsLog>,

    State(metrics): State<SharedMetrics>,

    State(sanitizer): State<Arc<InputSanitizer>>,

    Path(session_id): Path<u64>,

    identity: Option<Extension<CallerIdentity>>,

    Json(req): Json<PayloadUpdateRequest>,
) -> Result<(StatusCode, Json<ApiResponse>), StatusCode> {
    let id = api.next_id();

    // B5-server：受保护域准入——`shared.*.stable.llm.*` / `stable.system.*` 仅 service 身份可写。
    // 身份由认证中间件注入：认证启用时必注入（User/Service）；identity 为 None
    // 即认证禁用（loopback 开发模式），按放行处理（开发模式语义不变）。
    if requires_service_identity(&req.path)
        && matches!(identity, Some(Extension(CallerIdentity::User)))
    {
        tracing::warn!(session_id, path = %req.path, "session_payload 受保护域写入被拒绝（需 service 身份）");
        return Ok((
            StatusCode::FORBIDDEN,
            Json(ApiResponse {
                success: false,
                message: format!(
                    "写入受保护域 {} 被拒绝：stable.llm / stable.system 仅受信服务管道可写。\
                     服务端需配置 EVORULE_SERVICE_TOKEN，调用方（如 evo-agent）需携带该 service token。",
                    req.path
                ),
                fact_id: None,
            }),
        ));
    }

    // Phase 1: 第一层输入净化（静默改写 Prompt 注入内容）
    let (sanitized_value, sanitize_report) = sanitizer.sanitize_value(&req.value);
    if sanitize_report.has_hits() {
        // P5-A1：命中指标化（按 rule）
        for rule in sanitize_report.unique_hits() {
            metrics.inc_sanitize_hits(rule);
        }
        tracing::warn!(
            hits = ?sanitize_report.unique_hits(),
            hit_count = sanitize_report.hit_count(),
            session_id = session_id,
            path = %req.path,
            "session_payload 输入净化命中（已静默改写）"
        );
    }
    let value = serde_to_tcb(sanitized_value);

    let sessions = api.sessions.lock().await;

    sessions.touch_session(session_id);

    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    // L-1: 当路径以 "shared." 开头时，同步写入 SharedFactsLog（跨会话广播）

    // best-effort：失败仅 warn，不影响主流程

    if req.path.starts_with("shared.") {
        if let Err(e) = shared_facts.append(&req.path, value.clone(), session_id) {
            tracing::warn!(session_id, path = %req.path, "SharedFactsLog append failed: {e}");
        }
    }

    match session.command_tx.send(Fact::PayloadUpdate {
        id,

        path: req.path,

        value,
    }) {
        Ok(()) => Ok((
            StatusCode::OK,
            Json(ApiResponse {
                success: true,

                message: "PayloadUpdate submitted".to_string(),

                fact_id: Some(id.0),
            }),
        )),

        Err(_) => Ok((
            StatusCode::OK,
            Json(ApiResponse {
                success: false,

                message: "Command channel closed (reactor exited)".to_string(),

                fact_id: None,
            }),
        )),
    }
}

/// SSE 事件流 handler
///
/// `GET /api/sessions/:id/events` → 订阅指定会话的 event broadcast 通道，
/// 将 Fact 事件流式推送给客户端（text/event-stream）。
///
/// 事件格式：`data: {"type":"Command","id":1,"instruction":{...}}`
///
/// 连接保持直到：
/// - 客户端断开连接
/// - 会话被关闭（反应器退出，broadcast 通道关闭）
/// - 空闲超时（10 分钟无事件）
///
/// # 安全措施
/// - 全局 SSE 连接数限制（`MAX_SSE_CONNECTIONS=100`），超限返回 503
/// - 心跳（每 15s 发 `: ping`，防止代理/防火墙超时断开）
/// - 空闲超时（10 分钟无实际事件自动关闭，心跳不计入）
#[utoipa::path(

    get,

    path = "/api/sessions/{id}/events",

    tag = "sessions",

    params(("id" = u64, Path, description = "会话 ID")),

    responses(

        (status = 200, description = "SSE 事件流（text/event-stream），data 行格式：{\"type\":\"Command\",\"id\":1,\"instruction\":{...}}"),

        (status = 404, description = "会话不存在"),

        (status = 503, description = "SSE 连接数已满")

    )

)]

async fn session_events(
    State(api): State<SessionApi>,

    State(metrics): State<SharedMetrics>,

    Path(session_id): Path<u64>,
) -> Result<Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>>, StatusCode> {
    // 获取 SSE 连接配额，超限返回 503

    let sse_guard = api
        .try_acquire_sse()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    // SSE 连接指标 +1（stream 结束时通过 SseMetricsGuard 自动 -1）

    metrics.inc_sse_connections();

    // 从 SessionManager 获取 event 通道接收端

    let mut event_rx = {
        let sessions = api.sessions.lock().await;

        sessions.touch_session(session_id);

        let session = sessions
            .get_session(session_id)
            .ok_or(StatusCode::NOT_FOUND)?;

        session.event_tx.subscribe()
    };

    // 创建异步流：从 broadcast 接收 Fact，转换为 SSE Event

    // 心跳 + 空闲超时

    let stream = stream! {

        // 持有 SSE 连接配额守卫，stream 结束时自动释放

        let _guard = sse_guard;

        // 持有 metrics 守卫，stream 结束时自动 dec_sse_connections

        let _metrics_guard = SseMetricsGuard(metrics);



        let mut heartbeat = tokio::time::interval(SSE_HEARTBEAT_INTERVAL);

        heartbeat.tick().await; // 跳过第一次立即触发

        let mut last_event_time = tokio::time::Instant::now();



        loop {

            let idle_deadline = last_event_time + SSE_MAX_IDLE;

            tokio::select! {

                // 心跳：定期发送 : ping 保持连接活跃

                _ = heartbeat.tick() => {

                    yield Ok::<_, std::convert::Infallible>(

                        Event::default().comment("ping")

                    );

                }

                // 事件接收

                recv_result = event_rx.recv() => {

                    match recv_result {

                        Ok(fact) => {

                            last_event_time = tokio::time::Instant::now();

                            let data = fact_to_sse_data(&fact);

                            yield Ok::<_, std::convert::Infallible>(

                                Event::default().data(data)

                            );

                        }

                        Err(broadcast::error::RecvError::Closed) => {

                            tracing::debug!(

                                session_id,

                                "SSE stream closed: broadcast channel closed"

                            );

                            break;

                        }

                        Err(broadcast::error::RecvError::Lagged(n)) => {

                            tracing::warn!(

                                session_id,

                                dropped = n,

                                "SSE stream lagged, events dropped"

                            );

                            continue;

                        }

                    }

                }

                // 空闲超时：10 分钟无实际事件自动关闭（心跳不重置计时）

                _ = tokio::time::sleep_until(idle_deadline) => {

                    tracing::info!(

                        session_id,

                        idle_secs = SSE_MAX_IDLE.as_secs(),

                        "SSE 连接空闲超时，自动关闭"

                    );

                    break;

                }

            }

        }

    };

    Ok(Sse::new(stream))
}

#[derive(serde::Deserialize, utoipa::ToSchema)]

pub struct ReplayParams {
    /// 起始版本（默认 0）
    pub from: Option<u64>,

    /// 结束版本（默认最新版本）
    pub to: Option<u64>,
}

#[derive(serde::Deserialize, utoipa::ToSchema)]

pub struct FactsByPrefixParams {
    /// 路径前缀（默认空 = 全部）
    pub prefix: Option<String>,
}

// JoinRequest/BroadcastRequest/default_exclude_source 已删除（cluster 模块已移除）

// IoResponseRequest 已上移至数据模型区（带 ToSchema 标注）

/// 会话回放 handler
///
/// `GET /api/sessions/:id/replay?from=&to=` → 按版本区间回放 Fact 历史
#[utoipa::path(

    get,

    path = "/api/sessions/{id}/replay",

    tag = "sessions",

    params(

        ("id" = u64, Path, description = "会话 ID"),

        ("from" = Option<u64>, Query, description = "起始版本（默认 0）"),

        ("to" = Option<u64>, Query, description = "结束版本（默认最新）")

    ),

    responses(

        (status = 200, description = "Fact JSON 数组（每项含 version 字段）", body = Vec<FactEnvelope>),

        (status = 404, description = "会话不存在")

    )

)]

async fn session_replay(
    State(api): State<SessionApi>,

    Path(session_id): Path<u64>,

    Query(params): Query<ReplayParams>,
) -> Result<Json<Vec<FactEnvelope>>, StatusCode> {
    let sessions = api.sessions.lock().await;

    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    let from = params.from.unwrap_or(0);

    let to = params.to.unwrap_or_else(|| session.facts_log.version());

    let all_facts = session.facts_log.history_with_versions();

    let result: Vec<FactEnvelope> = all_facts
        .into_iter()
        .filter(|(v, _)| *v >= from && *v <= to)
        .map(|(version, fact)| fact_to_envelope(&fact, version))
        .collect();

    Ok(Json(result))
}

/// Fact 信封（带版本号的 Fact JSON 表示）
///
/// 用于 history/replay 端点的响应元素 schema。每项是 `Fact::to_json` 输出
/// 附加 `version` 字段，由 `type` 判别 7 种变体（与 core `Fact` 逐一对应）。
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(tag = "type")]
pub enum FactEnvelope {
    /// 用户提交新指令（触发执行）
    Command {
        /// Fact ID
        id: u64,
        /// 版本号（FactsLog 中的版本）
        version: u64,
        /// 待执行的指令对象
        instruction: serde_json::Value,
    },
    /// 外部更新 payload 字段
    PayloadUpdate {
        /// Fact ID
        id: u64,
        /// 版本号（FactsLog 中的版本）
        version: u64,
        /// 要更新的 payload 路径
        path: String,
        /// 新值
        value: serde_json::Value,
    },
    /// 状态转换（由反应器自动产生）
    StateTransition {
        /// Fact ID
        id: u64,
        /// 版本号（FactsLog 中的版本）
        version: u64,
        /// 触发此转换的源事实 ID
        cause: u64,
        /// 转换后的新 payload
        new_payload: serde_json::Value,
        /// 转换后的新指令队列
        new_queue: Vec<serde_json::Value>,
    },
    /// I/O 请求（由 TCB 产生，由治理层消费）
    IoRequest {
        /// Fact ID（用于 IoRequest ↔ IoResponse 配对）
        id: u64,
        /// 版本号（FactsLog 中的版本）
        version: u64,
        /// 触发此 I/O 请求的源事实 ID
        cause: u64,
        /// I/O 类型
        io_type: String,
        /// 请求参数
        params: serde_json::Value,
    },
    /// I/O 响应（由治理层产生，由反应器消费）
    IoResponse {
        /// Fact ID
        id: u64,
        /// 版本号（FactsLog 中的版本）
        version: u64,
        /// 对应的 IoRequest ID
        request_id: u64,
        /// I/O 执行结果
        result: serde_json::Value,
        /// I/O 错误信息（null=成功，字符串=失败描述）
        error: Option<String>,
    },
    /// 系统稳定（无更多指令可执行）
    Stable {
        /// Fact ID
        id: u64,
        /// 版本号（FactsLog 中的版本）
        version: u64,
    },
    /// 系统错误（超时或 TCB 内部错误）
    Error {
        /// Fact ID
        id: u64,
        /// 版本号（FactsLog 中的版本）
        version: u64,
        /// 错误描述
        message: String,
    },
    /// 规则命中归因轨迹（记录性事实；不推进版本号）
    TransitionTrace {
        /// Fact ID
        id: u64,
        /// 版本号（FactsLog 中的版本；trace 不推进版本，与同次收敛事实相同）
        version: u64,
        /// 同次转换的 StateTransition / Error(ignored) 事实 ID
        cause: u64,
        /// 各规则命中归因（与合并规则列表等长，按执行顺序）
        rule_hits: Vec<TraceHitDto>,
    },
}

/// 单条规则命中归因
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TraceHitDto {
    /// 规则在合并规则列表中的下标
    pub index: u64,
    /// 规则顶层指令类型（如 "branch"、"set"）
    pub instr_type: String,
    /// 是否结构命中
    pub hit: bool,
}

/// 将 core `Fact` 转换为强类型信封（附加版本号，字段与 `Fact::to_json` 对齐）
fn fact_to_envelope(fact: &Fact, version: u64) -> FactEnvelope {
    match fact {
        Fact::Command { id, instruction } => FactEnvelope::Command {
            id: id.0,
            version,
            instruction: tcb_to_serde(instruction),
        },
        Fact::PayloadUpdate { id, path, value } => FactEnvelope::PayloadUpdate {
            id: id.0,
            version,
            path: path.clone(),
            value: tcb_to_serde(value),
        },
        Fact::StateTransition {
            id,
            cause,
            new_payload,
            new_queue,
        } => FactEnvelope::StateTransition {
            id: id.0,
            version,
            cause: cause.0,
            new_payload: tcb_to_serde(new_payload),
            new_queue: new_queue.iter().map(tcb_to_serde).collect(),
        },
        Fact::IoRequest {
            id,
            cause,
            io_type,
            params,
        } => FactEnvelope::IoRequest {
            id: id.0,
            version,
            cause: cause.0,
            io_type: io_type.as_str().to_string(),
            params: tcb_to_serde(params),
        },
        Fact::IoResponse {
            id,
            request_id,
            result,
            error,
        } => FactEnvelope::IoResponse {
            id: id.0,
            version,
            request_id: request_id.0,
            result: tcb_to_serde(result),
            error: error.clone(),
        },
        //：不再内嵌 final_snapshot 全量快照（O(n²) 根因）
        Fact::Stable { id, .. } => FactEnvelope::Stable { id: id.0, version },
        Fact::Error { id, message } => FactEnvelope::Error {
            id: id.0,
            version,
            message: message.clone(),
        },
        // 规则命中归因轨迹
        Fact::TransitionTrace {
            id,
            cause,
            rule_hits,
        } => FactEnvelope::TransitionTrace {
            id: id.0,
            version,
            cause: cause.0,
            rule_hits: rule_hits
                .iter()
                .map(|h| TraceHitDto {
                    index: h.index,
                    instr_type: h.instr_type.clone(),
                    hit: h.hit,
                })
                .collect(),
        },
    }
}

/// 会话历史 handler
///
/// `GET /api/sessions/:id/history` → 返回全部 Fact 历史（含版本号）
#[utoipa::path(

    get,

    path = "/api/sessions/{id}/history",

    tag = "sessions",

    params(("id" = u64, Path, description = "会话 ID")),

    responses(

        (status = 200, description = "Fact JSON 数组（每项含 version 字段）", body = Vec<FactEnvelope>),

        (status = 404, description = "会话不存在")

    )

)]

async fn session_history(
    State(api): State<SessionApi>,

    Path(session_id): Path<u64>,
) -> Result<Json<Vec<FactEnvelope>>, StatusCode> {
    let sessions = api.sessions.lock().await;

    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    let all_facts = session.facts_log.history_with_versions();

    let result: Vec<FactEnvelope> = all_facts
        .into_iter()
        .map(|(version, fact)| fact_to_envelope(&fact, version))
        .collect();

    Ok(Json(result))
}

// rewind/diff 端点（时间旅行调试）

#[derive(Deserialize, utoipa::ToSchema)]

pub struct RewindParams {
    /// 回退目标版本
    version: u64,
}

#[derive(Deserialize, utoipa::ToSchema)]

pub struct DiffParams {
    /// diff 起始版本
    a: u64,

    /// diff 结束版本
    b: u64,
}

/// 会话时间旅行回退 handler
///
/// `GET /api/sessions/:id/rewind?version=` → 回退到指定版本的状态快照
#[utoipa::path(

    get,

    path = "/api/sessions/{id}/rewind",

    tag = "sessions",

    params(

        ("id" = u64, Path, description = "会话 ID"),

        ("version" = u64, Query, description = "回退目标版本")

    ),

    responses(

        (status = 200, description = "回退后的状态快照", body = RewindResponse),

        (status = 400, description = "目标版本不存在"),

        (status = 404, description = "会话不存在")

    )

)]

async fn session_rewind(
    State(api): State<SessionApi>,

    Path(session_id): Path<u64>,

    Query(params): Query<RewindParams>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;

    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    let facts = session.facts_log.history();

    let snapshot = evorule_governance::time_machine::rewind(&facts, params.version);

    match snapshot {
        Some(snap) => Ok(Json(serde_json::json!({

            "session_id": session_id,

            "target_version": params.version,

            "payload": snap.payload,

            "queue": snap.queue,

            "actual_version": snap.version,

        }))),

        None => Err(StatusCode::BAD_REQUEST),
    }
}

/// 会话 diff handler
///
/// `GET /api/sessions/:id/diff?a=&b=` → 对比两个版本之间的 payload 差异
#[utoipa::path(

    get,

    path = "/api/sessions/{id}/diff",

    tag = "sessions",

    params(

        ("id" = u64, Path, description = "会话 ID"),

        ("a" = u64, Query, description = "diff 起始版本"),

        ("b" = u64, Query, description = "diff 结束版本")

    ),

    responses(

        (status = 200, description = "diff 结果（items / removed / summary）", body = DiffResponse),

        (status = 404, description = "会话不存在")

    )

)]

async fn session_diff(
    State(api): State<SessionApi>,

    Path(session_id): Path<u64>,

    Query(params): Query<DiffParams>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;

    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    let facts = session.facts_log.history();

    // B8b 配套（evorule-governance 0.4.1）：diff 对 rewind 不可达版本
    // 由静默回退空 payload 改为返回 Err(TimeMachineError)——与 rewind 端点
    // 同语义映射为 400 BAD_REQUEST。
    let diff = match evorule_governance::time_machine::diff(&facts, params.a, params.b) {
        Ok(d) => d,
        Err(_) => return Err(StatusCode::BAD_REQUEST),
    };

    // 契约对齐(S1 修复,2026-08-03):

    //   ttd 修复 2 + SPEC §1.1 规定 diff 返回 { items: [...] },元素为元组:

    //     - added   → [key, value]      (2 元组)

    //     - changed → [key, old, new]   (3 元组)

    //   D1-B 扩展契约:removed 不并入 items(items 契约只支持 added/changed 语义),

    //   单独作为 removed 字段返回 [[key, value], ...]。

    //   之前返回 { added, removed, changed, unchanged, summary } 违背既定契约,已修正。

    let mut items: Vec<serde_json::Value> =
        Vec::with_capacity(diff.added.len() + diff.changed.len());

    for (k, v) in &diff.added {
        items.push(serde_json::json!([k, v]));
    }

    for (k, old, new) in &diff.changed {
        items.push(serde_json::json!([k, old, new]));
    }

    let removed: Vec<serde_json::Value> = diff
        .removed
        .iter()
        .map(|(k, v)| serde_json::json!([k, v]))
        .collect();

    Ok(Json(serde_json::json!({

        "session_id": session_id,

        "from_version": params.a,

        "to_version": params.b,

        "items": items,

        "removed": removed,

        "summary": diff.summary(),

    })))
}

/// 按路径前缀查询会话 payload 事实
///
/// `GET /api/sessions/:id/facts?prefix=` → 返回匹配前缀的 PayloadUpdate 事实
#[utoipa::path(

    get,

    path = "/api/sessions/{id}/facts",

    tag = "shared-facts",

    params(

        ("id" = u64, Path, description = "会话 ID"),

        ("prefix" = Option<String>, Query, description = "路径前缀（默认空 = 全部）")

    ),

    responses(

        (status = 200, description = "PayloadUpdate 事实数组", body = [SessionFact]),

        (status = 404, description = "会话不存在")

    )

)]

async fn session_facts_by_prefix(
    State(api): State<SessionApi>,

    Path(session_id): Path<u64>,

    Query(params): Query<FactsByPrefixParams>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;

    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    let prefix = params.prefix.unwrap_or_default();

    let facts = session.facts_log.facts_by_path_prefix(&prefix);

    // D-S3 修复(2026-08-03):非 PayloadUpdate 的 fact 跳过(filter),不再返回空对象 {}。

    //   facts 端点语义是"按 path prefix 的 payload 更新索引",只返回 PayloadUpdate。

    let result: Vec<_> = facts
        .into_iter()
        .filter_map(|(version, fact)| {
            if let Fact::PayloadUpdate { id, path, value } = fact {
                Some(serde_json::json!({

                    "fact_id": id.0,

                    "version": version,

                    "path": path,

                    "value": tcb_to_serde(&value),

                }))
            } else {
                None
            }
        })
        .collect();

    Ok(Json(serde_json::Value::Array(result)))
}

/// 按路径前缀查询共享事实
///
/// `GET /api/shared/facts?prefix=` → 返回匹配前缀的共享事实
#[utoipa::path(

    get,

    path = "/api/shared/facts",

    tag = "shared-facts",

    params(("prefix" = Option<String>, Query, description = "路径前缀（默认空 = 全部）")),

    responses(

        (status = 200, description = "共享事实数组", body = [SharedFact])

    )

)]

async fn shared_facts_by_prefix(
    State(shared_facts): State<SharedFactsLog>,

    Query(params): Query<FactsByPrefixParams>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let prefix = params.prefix.unwrap_or_default();

    let facts = shared_facts.facts_by_path_prefix(&prefix);

    let result: Vec<_> = facts
        .into_iter()
        .map(|sf| {
            serde_json::json!({

                "fact_id": sf.fact_id.0,

                "path": sf.path,

                "value": tcb_to_serde(&sf.value),

                "source_session_id": sf.source_session_id,

                "version": sf.version,

            })
        })
        .collect();

    Ok(Json(serde_json::Value::Array(result)))
}

/// 共享事实来源查询
///
/// `GET /api/shared/facts/{fact_id}/source` → 返回共享事实的完整信息
#[utoipa::path(

    get,

    path = "/api/shared/facts/{fact_id}/source",

    tag = "shared-facts",

    params(("fact_id" = u64, Path, description = "共享事实 ID")),

    responses(

        (status = 200, description = "共享事实详情", body = SharedFact),

        (status = 404, description = "共享事实不存在")

    )

)]

async fn shared_fact_source(
    State(shared_facts): State<SharedFactsLog>,

    Path(fact_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let fact = shared_facts
        .fact_by_id(FactId(fact_id))
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(serde_json::json!({

        "fact_id": fact.fact_id.0,

        "path": fact.path,

        "value": tcb_to_serde(&fact.value),

        "source_session_id": fact.source_session_id,

        "version": fact.version,

    })))
}

/// 记录会话启动时使用的共享事实
///
/// `POST /api/sessions/{id}/used_at_startup` → 记录该会话启动时引用的共享事实 ID
#[utoipa::path(

    post,

    path = "/api/sessions/{id}/used_at_startup",

    tag = "shared-facts",

    params(("id" = u64, Path, description = "会话 ID")),

    request_body = FactIdsRequest,

    responses(

        (status = 200, description = "记录成功", body = ApiResponse),

        (status = 400, description = "请求体缺少 fact_ids 或格式错误")

    )

)]

async fn record_used_at_startup(
    State(shared_facts): State<SharedFactsLog>,

    Path(session_id): Path<u64>,

    Json(req): Json<FactIdsRequest>,
) -> Result<Json<ApiResponse>, StatusCode> {
    let fact_ids: Vec<FactId> = req.fact_ids.into_iter().map(FactId).collect();

    shared_facts.record_used_at_startup(session_id, &fact_ids);

    tracing::info!(
        session_id,
        fact_count = fact_ids.len(),
        "Recorded used_at_startup"
    );

    Ok(Json(ApiResponse {
        success: true,

        message: "used_at_startup recorded".to_string(),

        fact_id: None,
    }))
}

/// 查询会话启动时使用的共享事实
///
/// `GET /api/sessions/{id}/used_at_startup` → 返回该会话启动时引用的共享事实 ID
#[utoipa::path(

    get,

    path = "/api/sessions/{id}/used_at_startup",

    tag = "shared-facts",

    params(("id" = u64, Path, description = "会话 ID")),

    responses(

        (status = 200, description = "共享事实 ID 列表", body = UsedAtStartupResponse)

    )

)]

async fn get_used_at_startup(
    State(shared_facts): State<SharedFactsLog>,

    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let fact_ids = shared_facts
        .get_used_at_startup(session_id)
        .unwrap_or_default();

    let result: Vec<_> = fact_ids.into_iter().map(|f| f.0).collect();

    Ok(Json(serde_json::json!({

        "session_id": session_id,

        "fact_ids": result,

    })))
}

/// 查询共享事实的使用方
///
/// `GET /api/shared/facts/{fact_id}/used_by` → 返回使用该共享事实的会话列表
#[utoipa::path(

    get,

    path = "/api/shared/facts/{fact_id}/used_by",

    tag = "shared-facts",

    params(("fact_id" = u64, Path, description = "共享事实 ID")),

    responses(

        (status = 200, description = "使用该事实的会话 ID 列表", body = SessionsUsingFactResponse)

    )

)]

async fn get_sessions_using_fact(
    State(shared_facts): State<SharedFactsLog>,

    Path(fact_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = shared_facts.get_sessions_using_fact(FactId(fact_id));

    Ok(Json(serde_json::json!({

        "fact_id": fact_id,

        "sessions": sessions,

    })))
}

/// `POST /api/shared/facts/rollup` — 标记一批共享事实为已 rollup
///
/// 被标记的 fact_id 在 `facts_by_path_prefix` 查询中被过滤，
/// 但仍可通过 `fact_by_id` 访问以保留审计可追溯性。
#[utoipa::path(

    post,

    path = "/api/shared/facts/rollup",

    tag = "shared-facts",

    request_body = FactIdsRequest,

    responses(

        (status = 200, description = "标记成功，返回标记数量", body = ApiResponse),

        (status = 400, description = "请求体缺少 fact_ids 或格式错误")

    )

)]

async fn shared_facts_rollup(
    State(shared_facts): State<SharedFactsLog>,

    Json(req): Json<FactIdsRequest>,
) -> Result<Json<ApiResponse>, StatusCode> {
    let fact_ids: Vec<FactId> = req.fact_ids.into_iter().map(FactId).collect();

    let count = fact_ids.len();

    shared_facts.mark_as_rollup(&fact_ids);

    tracing::info!(fact_count = count, "Marked shared facts as rolled up");

    Ok(Json(ApiResponse {
        success: true,

        message: format!("{} facts marked as rolled up", count),

        fact_id: None,
    }))
}

/// 共享事实日志版本响应

#[derive(Debug, Serialize, ToSchema)]

pub struct SharedFactsVersionResponse {
    /// 当前版本号（FactsLog 版本）
    pub version: u64,

    /// 历史记录数量
    pub history_len: usize,
}

/// 获取共享事实日志版本（GET /api/shared/facts/version，014 合法 API #3）

#[utoipa::path(

    get,

    path = "/api/shared/facts/version",

    tag = "shared-facts",

    responses(

        (status = 200, description = "共享事实日志版本信息", body = SharedFactsVersionResponse)

    )

)]

async fn shared_facts_version(
    State(shared_facts): State<SharedFactsLog>,
) -> Result<Json<SharedFactsVersionResponse>, StatusCode> {
    Ok(Json(SharedFactsVersionResponse {
        version: shared_facts.version(),

        history_len: shared_facts.history_len(),
    }))
}

/// 调试：查询会话当前阶段

#[utoipa::path(

    get,

    path = "/api/sessions/{id}/debug/phase",

    tag = "sessions",

    params(("id" = u64, Path, description = "会话 ID")),

    responses(

        (status = 200, description = "当前执行阶段", body = DebugPhaseResponse),

        (status = 404, description = "会话不存在")

    )

)]

async fn debug_phase(
    State(api): State<SessionApi>,

    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;

    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    let phase = session
        .handle
        .current_phase()
        .map(|p| serde_json::Value::String(p.as_str().to_string()))
        .unwrap_or(serde_json::Value::Null);

    Ok(Json(serde_json::json!({

        "session_id": session_id,

        "phase": phase,

    })))
}

/// 调试：查询会话队列

#[utoipa::path(

    get,

    path = "/api/sessions/{id}/debug/queue",

    tag = "sessions",

    params(("id" = u64, Path, description = "会话 ID")),

    responses(

        (status = 200, description = "队列（当前恒为空）", body = DebugQueueResponse),

        (status = 404, description = "会话不存在")

    )

)]

async fn debug_queue(
    State(api): State<SessionApi>,

    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;

    let _session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(serde_json::json!({

        "session_id": session_id,

        "queue": serde_json::Value::Array(vec![]),

    })))
}

/// 调试：查询会话待处理 I/O

#[utoipa::path(

    get,

    path = "/api/sessions/{id}/debug/pending_io",

    tag = "sessions",

    params(("id" = u64, Path, description = "会话 ID")),

    responses(

        (status = 200, description = "待处理 I/O 数量与列表", body = DebugPendingIoResponse),

        (status = 404, description = "会话不存在")

    )

)]

async fn debug_pending_io(
    State(api): State<SessionApi>,

    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;

    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    let pending_count = session.handle.pending_io_count().unwrap_or(0);

    Ok(Json(serde_json::json!({

        "session_id": session_id,

        "pending_io_count": pending_count,

        "pending_io": serde_json::Value::Array(vec![]),

    })))
}

/// 中断会话（下一检查点生效）

#[utoipa::path(

    post,

    path = "/api/sessions/{id}/interrupt",

    tag = "sessions",

    params(("id" = u64, Path, description = "会话 ID")),

    responses(

        (status = 200, description = "中断请求已受理", body = InterruptResponse),

        (status = 404, description = "会话不存在")

    )

)]

async fn session_interrupt(
    State(api): State<SessionApi>,

    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;

    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    session.handle.interrupt();

    Ok(Json(serde_json::json!({

        "session_id": session_id,

        "success": true,

        "message": "Interrupt requested, reactor will respond at next checkpoint",

    })))
}

/// 强制中止会话（POST /api/sessions/{id}/abort，014 合法 API #4）
///
/// 破坏性操作：直接中止反应器任务，不等待 checkpoint。
///
/// 双保险：该端点默认不挂载（未启用 `--allow-abort` 时返回 404），
/// 即使认证通过也需显式开启才能访问。OpenAPI 文档始终可见以保持契约一致。
///
/// 需要认证 + `--allow-abort`（或 `EVORULE_ALLOW_ABORT=1`）均满足才可调用。

#[utoipa::path(

    post,

    path = "/api/sessions/{id}/abort",

    tag = "sessions",

    params(("id" = u64, Path, description = "会话 ID")),

    responses(

        (status = 200, description = "会话已中止", body = InterruptResponse),

        (status = 404, description = "会话不存在")

    )

)]

async fn session_abort(
    State(api): State<SessionApi>,

    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;

    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    session.abort();

    Ok(Json(serde_json::json!({

        "session_id": session_id,

        "success": true,

        "message": "Session aborted",

    })))
}

/// 检查会话是否已结束

#[utoipa::path(

    get,

    path = "/api/sessions/{id}/finished",

    tag = "sessions",

    params(("id" = u64, Path, description = "会话 ID")),

    responses(

        (status = 200, description = "会话是否已结束", body = FinishedResponse),

        (status = 404, description = "会话不存在")

    )

)]

async fn session_finished(
    State(api): State<SessionApi>,

    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;

    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(serde_json::json!({

        "session_id": session_id,

        "finished": session.is_finished(),

    })))
}

/// 获取因果链深度

#[utoipa::path(

    get,

    path = "/api/sessions/{id}/causal_depth",

    tag = "sessions",

    params(("id" = u64, Path, description = "会话 ID")),

    responses(

        (status = 200, description = "因果链深度", body = CausalDepthResponse),

        (status = 404, description = "会话不存在")

    )

)]

async fn session_causal_depth(
    State(api): State<SessionApi>,

    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;

    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(serde_json::json!({

        "session_id": session_id,

        "causal_depth": session.causal_depth(),

    })))
}

/// 获取不变式违规计数

#[utoipa::path(

    get,

    path = "/api/sessions/{id}/invariants",

    tag = "sessions",

    params(("id" = u64, Path, description = "会话 ID")),

    responses(

        (status = 200, description = "结构不变式违规计数", body = InvariantsResponse),

        (status = 404, description = "会话不存在")

    )

)]

async fn session_invariants(
    State(api): State<SessionApi>,

    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;

    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(serde_json::json!({

        "session_id": session_id,

        "structural_invariant_violations": session.structural_invariant_violations(),

    })))
}

/// 获取待处理 I/O 数量

#[utoipa::path(

    get,

    path = "/api/sessions/{id}/pending_io_count",

    tag = "sessions",

    params(("id" = u64, Path, description = "会话 ID")),

    responses(

        (status = 200, description = "待处理 I/O 数量", body = PendingIoCountResponse),

        (status = 404, description = "会话不存在")

    )

)]

async fn session_pending_io_count(
    State(api): State<SessionApi>,

    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;

    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(serde_json::json!({

        "session_id": session_id,

        "pending_io_count": session.pending_io_count(),

    })))
}

/// 获取当前执行步数

#[utoipa::path(

    get,

    path = "/api/sessions/{id}/step",

    tag = "sessions",

    params(("id" = u64, Path, description = "会话 ID")),

    responses(

        (status = 200, description = "当前执行步数", body = StepResponse),

        (status = 404, description = "会话不存在")

    )

)]

async fn session_step(
    State(api): State<SessionApi>,

    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;

    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(serde_json::json!({

        "session_id": session_id,

        "current_step": session.current_step(),

    })))
}

/// 获取完整状态快照

#[utoipa::path(

    get,

    path = "/api/sessions/{id}/snapshot",

    tag = "sessions",

    params(("id" = u64, Path, description = "会话 ID")),

    responses(

        (status = 200, description = "完整状态快照", body = SnapshotResponse),

        (status = 404, description = "会话不存在")

    )

)]

async fn session_snapshot(
    State(api): State<SessionApi>,

    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;

    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    match session.snapshot() {
        Some(snap) => Ok(Json(serde_json::json!({

            "session_id": session_id,

            "finished": snap.finished,

            "phase": snap.phase.as_str(),

            "version": snap.version,

            "steps": snap.steps,

            "pending_io_count": snap.pending_io_count,

            "structural_invariant_violations": snap.structural_invariant_violations,

        }))),

        None => Ok(Json(serde_json::json!({

            "session_id": session_id,

            "error": "Failed to get snapshot (reactor finished or lock poisoned)",

        }))),
    }
}

/// 查询审计链自动验证状态

#[utoipa::path(

    get,

    path = "/api/sessions/{id}/audit/auto_verify",

    tag = "sessions",

    params(("id" = u64, Path, description = "会话 ID")),

    responses(

        (status = 200, description = "自动验证状态", body = AutoVerifyResponse),

        (status = 404, description = "会话不存在")

    )

)]

async fn session_auto_verify_get(
    State(api): State<SessionApi>,

    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;

    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(serde_json::json!({

        "session_id": session_id,

        "auto_verify": session.is_auto_verify_enabled(),

    })))
}

/// 设置审计链自动验证配置

#[utoipa::path(

    post,

    path = "/api/sessions/{id}/audit/auto_verify",

    tag = "sessions",

    params(("id" = u64, Path, description = "会话 ID")),

    request_body = AutoVerifyRequest,

    responses(

        (status = 200, description = "配置已更新", body = AutoVerifyConfigureResponse),

        (status = 404, description = "会话不存在")

    )

)]

async fn session_auto_verify_post(
    State(api): State<SessionApi>,

    Path(session_id): Path<u64>,

    Json(req): Json<AutoVerifyRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;

    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    let enabled = req.enabled;

    let threshold = req.threshold as usize;

    let interval = req.interval as usize;

    session.set_auto_verify(enabled, threshold, interval);

    Ok(Json(serde_json::json!({

        "session_id": session_id,

        "success": true,

        "auto_verify": enabled,

        "threshold": threshold,

        "interval": interval,

        "message": format!("Auto-verify {}", if enabled { "enabled" } else { "disabled" }),

    })))
}

/// IoResponse 外部提交 handler
///
/// `POST /api/sessions/:id/io_response` → 外部应用提交 IoResponse，
/// 允许外部系统通过 HTTP API 异步返回 I/O 执行结果。
///
/// 请求体格式：
/// ```json
/// {
/// "request_id": 123,
/// "result": {"content": "response data"},
/// "error": null
/// }
/// ```
#[utoipa::path(

    post,

    path = "/api/sessions/{id}/io_response",

    tag = "sessions",

    params(("id" = u64, Path, description = "会话 ID")),

    request_body = IoResponseRequest,

    responses(

        (status = 200, description = "IoResponse 已提交，返回 fact_id", body = ApiResponse),

        (status = 404, description = "会话不存在")

    )

)]

async fn session_io_response(
    State(api): State<SessionApi>,

    Path(session_id): Path<u64>,

    Json(req): Json<IoResponseRequest>,
) -> Result<Json<ApiResponse>, StatusCode> {
    let id = api.next_id();

    let request_id = evorule_reactor::FactId(req.request_id);

    let result = serde_to_tcb(req.result);

    let sessions = api.sessions.lock().await;

    sessions.touch_session(session_id);

    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    match session.command_tx.send(Fact::IoResponse {
        id,

        request_id,

        result,

        error: req.error,
    }) {
        Ok(()) => {
            tracing::info!(
                session_id,
                request_id = req.request_id,
                "IoResponse submitted externally"
            );

            Ok(Json(ApiResponse {
                success: true,

                message: "IoResponse submitted".to_string(),

                fact_id: Some(id.0),
            }))
        }

        Err(_) => Ok(Json(ApiResponse {
            success: false,

            message: "Command channel closed (reactor exited)".to_string(),

            fact_id: None,
        })),
    }
}

// cluster 相关 handler 已移除（多 reactor 协作原语，应用层功能）

/// 治理层 HTTP 服务器
///
///
/// 支持两套路由：
///
/// - 单反应器模式（`/api/command`、`/api/state` 等，向后兼容）
///
/// - 多会话模式（`/api/sessions/*`，配合长驻反应器和 SSE 事件流）
///
pub struct GovernanceServer {
    state: AppState,

    auth: AuthConfig,

    /// 监听地址（保留用于诊断/显示，实际绑定在 main.rs 中完成）

    #[allow(dead_code)]
    addr: String,

    /// 速率限制：每 IP 持续速率（req/s）。0 表示禁用限速。
    rate_limit_per_sec: u64,

    /// 速率限制：令牌桶容量（突发上限）
    rate_limit_burst: u32,

    /// CORS 允许的 Origin 白名单

    ///

    /// - 空列表:默认放行本机 loopback Origin(localhost/127.0.0.1/[::1]

    ///   任意端口,开发友好);外部 Origin 仍被拒绝

    /// - 非空列表：只允许列表中的 Origin 通过。列表元素示例：`"http://localhost:3000"`

    /// - 生产部署(监听 0.0.0.0)必须显式配置精确白名单
    allowed_origins: Arc<Vec<String>>,

    /// S2：/metrics 端点是否需要认证（默认 false，Prometheus scraper 通常不带 token）
    metrics_requires_auth: bool,

    /// 是否挂载 Swagger UI（--openapi-ui）。默认关闭，避免生产暴露接口面。

    /// `/api/openapi.json` 始终可用（单一真相源），UI 仅是可视化外壳。
    openapi_ui: bool,

    /// 是否启用强制中止端点（POST /api/sessions/{id}/abort，--allow-abort）。
    /// 默认关闭：即使认证通过也不注册该路由（双保险，返回 404）。
    allow_abort: bool,

    /// 静态前端目录（--web-dir）。Some 时由本服务同源托管 Web UI：
    /// 未命中 /api 路由的 GET 请求走 ServeDir，未知路径回退 index.html（SPA）。
    web_dir: Option<std::path::PathBuf>,
}

impl GovernanceServer {
    /// 创建新服务器
    ///
    ///
    /// # 参数
    ///
    /// - `state`：应用全局状态（合并 GovernanceApi + SessionApi）
    ///
    /// - `auth`：认证配置
    ///
    /// - `addr`：监听地址（如 "0.0.0.0:8080"）
    ///
    /// - `rate_limit_per_sec`：每 IP 持续速率（req/s），`0` = 禁用
    ///
    /// - `rate_limit_burst`：突发上限（令牌桶容量）
    ///
    /// - `allowed_origins`：CORS 允许的 Origin 白名单（空=放行本机 loopback 任意端口，
    ///   非空=精确白名单）
    ///
    /// - `metrics_requires_auth`：/metrics 是否需要认证
    ///
    /// - `openapi_ui`：是否挂载 Swagger UI（默认 false）
    ///
    /// - `allow_abort`：是否启用强制中止端点（默认 false，双保险）
    ///
    /// - `web_dir`：静态前端目录（--web-dir）；None = 不托管静态文件
    ///
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        state: AppState,

        auth: AuthConfig,

        addr: String,

        rate_limit_per_sec: u64,

        rate_limit_burst: u32,

        allowed_origins: Vec<String>,

        metrics_requires_auth: bool,

        openapi_ui: bool,

        allow_abort: bool,

        web_dir: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            state,

            auth,

            addr,

            rate_limit_per_sec,

            rate_limit_burst,

            allowed_origins: Arc::new(allowed_origins),

            metrics_requires_auth,

            openapi_ui,

            allow_abort,

            web_dir,
        }
    }

    /// 创建禁用认证的开发服务器（保留默认限速 200 req/s）
    #[allow(dead_code)]
    pub fn dev(state: AppState, addr: String) -> Self {
        Self::new(
            state,
            AuthConfig::disabled(),
            addr,
            1,
            200,
            vec![
                "http://localhost:3000".to_string(),
                "https://localhost:3000".to_string(),
                "http://127.0.0.1:3000".to_string(),
            ],
            false,
            false,
            // 开发服务器默认也不启用 abort（双保险，需显式 --allow-abort）
            false,
            None,
        )
    }

    /// 创建禁用认证 + 禁用限速的基准测试服务器（仅用于 benchmarks）
    #[allow(dead_code)]
    pub fn bench(state: AppState, addr: String) -> Self {
        // per_sec=0 触发 build_router 完全跳过 GovernorLayer（真正禁用限速）

        Self::new(
            state,
            AuthConfig::disabled(),
            addr,
            0,
            0,
            vec![],
            false,
            false,
            false,
            None,
        )
    }

    /// 构建路由（公开，供 bin 自定义启动流程使用）
    ///
    ///
    /// # 安全层（从内到外）
    ///
    /// 1. `auth_middleware` — Bearer token 认证
    ///
    /// 2. `RequestBodyLimitLayer` — 请求体大小限制（1MB）
    ///
    /// 3. `ConcurrencyLimitLayer` — 并发连接数限制（1000）
    ///
    /// 4. `CorsLayer` — CORS 预检处理
    ///
    /// 5. `GovernorLayer` — 速率限制（每 IP `rate_limit_burst / rate_limit_per_sec` req/s）
    ///
    ///
    ///
    /// # 注意
    ///
    /// `GovernorLayer` 依赖 `ConnectInfo<SocketAddr>` 提取客户端 IP，
    ///
    /// 因此 bin 启动时必须使用 `into_make_service_with_connect_info::<SocketAddr>`。
    ///
    ///
    ///
    /// # tower-governor 参数语义
    ///
    /// `per_second` 是令牌桶补充周期（秒），每周期补充 `burst_size` 个令牌。
    ///
    /// 持续速率 = burst_size / per_second（req/s）。
    ///
    /// burst_size 同时是桶的最大容量（突发上限）。
    ///
    // axum Router 多 route 集中配置, 拆函数需共享 AppState。详见 GATE_REFERENCE.md §六(豁免索引)
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
    pub fn build_router(&self) -> Router {
        let auth = self.auth.clone();

        // 速率限制配置（令牌桶：每 period 秒补充 burst 个令牌）

        // rate_limit_per_sec == 0 表示完全禁用限速（不添加 GovernorLayer）

        // 修复：之前用 (1, 1_000_000) 模拟"无限速"，但 GovernorConfigBuilder::finish

        // 可能 fallback 到 GovernorConfig::default（默认低限速），导致 --no-rate-limit

        // 实际仍触发 429。现在通过 resolve_governor_config 条件性返回 None 来跳过 GovernorLayer。

        //

        // 注意：GovernorLayer 不能存入 Option<GovernorLayer> 变量（其 M/RespBody 泛型

        // 只能在 .layer 调用时通过 Layer trait 约束推断），因此采用 match 分支。

        // 公开路由（免认证）— health/liveness/readiness/metrics

        // /api/rules/validate 从 protected_routes 移到 public_routes（仅读 JSON，不改状态）

        // /api/rules/reload 是运营操作，但仍放在 public_routes 里（由部署环境的

        // 网络层防火墙限制访问来源，evorule-server 作为框架层不做 RBAC 强制）

        // H6: 显式指定 Router 状态类型为 AppState，消除 SharedMetrics 的 FromRef 歧义

        //（Arc<dyn IoMetrics> 既满足 FromRef<AppState>，又满足 blanket impl FromRef<T> for T: Clone）

        let public_routes = Router::<AppState>::new()
            .route("/api/health", get(health))
            .route("/api/health/liveness", get(liveness))
            .route("/api/health/readiness", get(readiness))
            .route("/api/rules/validate", post(validate_rules_handler))
            // OpenAPI 单一真相源：仅暴露规范元数据（无业务数据），故免认证，
            // 便于前端 codegen 与运维查阅。Swagger UI 交互界面由 --openapi-ui 单独控制。
            .route("/api/openapi.json", get(crate::api::openapi::openapi_json))
            // C5：执行侧已绑定服务能力对账（仅只读能力元数据，不改状态）——
            // 供场景包导入前服务需求预检与治理侧服务目录（GET /v1/services）核对。
            .route("/api/services", get(list_services_handler))
            // 平台授权:bootstrap/login/status 免认证;其余平台端点
            // (me/管理端点等)handler 内自校验平台 token/权限点。
            .merge(crate::api::platform_auth::platform_auth_router());

        // abort 破坏性端点双保险：即使认证通过也默认拒绝，仅 --allow-abort 显式
        // 开启后才挂载该路由（默认不注册 → 404）。空 Router merge 无副作用。
        let abort_router = if self.allow_abort {
            Router::new().route("/api/sessions/{id}/abort", post(session_abort))
        } else {
            Router::new()
        };

        // 受保护路由（需认证）

        let protected_routes = Router::new()
            // 单反应器模式路由（向后兼容）
            .route("/api/command", post(submit_command))
            // 服务直调：与 io_request 同一处理器链，属状态变更执行面——
            // 必须受认证保护；敏感服务另有 403 守卫（须走会话审计链）。
            .route(
                "/api/services/{name}/invoke",
                post(invoke_service_handler),
            )
            .route("/api/payload", post(update_payload))
            .route("/api/state", get(get_state))
            .route("/api/audit", get(get_audit))
            // :平台认证事件报表(只读,自 SharedFactsLog platform.event.* 派生)
            .route("/api/audit/platform-events", get(platform_events_handler))
            // 多会话模式路由
            .route("/api/sessions", post(create_session).get(list_sessions))
            // ：审计档案（只读，历史会话 WAL 重建；与活跃会话 API 物理隔离）
            .route("/api/audit-archive/sessions", get(archive_sessions))
            .route(
                "/api/audit-archive/sessions/{id}/audit",
                get(archive_session_audit),
            )
            .route(
                "/api/sessions/from/{parent_id}",
                post(create_session_from_parent),
            )
            .route("/api/sessions/fork/{parent_id}", post(create_session_fork))
            .route(
                "/api/sessions/{id}",
                get(session_metadata).delete(close_session),
            )
            .route("/api/sessions/reap", post(session_reap))
            .route("/api/sessions/{id}/command", post(session_command))
            .route("/api/sessions/{id}/state", get(session_state))
            .route("/api/sessions/{id}/audit", get(session_audit))
            .route("/api/sessions/{id}/audit/verify", get(session_audit_verify))
            .route("/api/sessions/{id}/audit/export", get(session_audit_export))
            .route(
                "/api/sessions/{id}/audit/import",
                post(session_audit_import),
            )
            .route(
                "/api/sessions/{id}/audit/export/compressed",
                get(session_audit_export_compressed),
            )
            .route(
                "/api/sessions/{id}/audit/import/compressed",
                post(session_audit_import_compressed),
            )
            .route(
                "/api/sessions/{id}/audit/causal/{fact_id}",
                get(session_causal_chain),
            )
            .route("/api/sessions/{id}/payload", post(session_payload))
            .route("/api/sessions/{id}/events", get(session_events))
            .route("/api/sessions/{id}/io_response", post(session_io_response))
            // 治理层演进 API（回放、时间旅行、集群协作）
            .route("/api/sessions/{id}/replay", get(session_replay))
            .route("/api/sessions/{id}/history", get(session_history))
            .route("/api/sessions/{id}/rewind", get(session_rewind))
            .route("/api/sessions/{id}/diff", get(session_diff))
            .route("/api/sessions/{id}/facts", get(session_facts_by_prefix))
            .route("/api/shared/facts", get(shared_facts_by_prefix))
            .route(
                "/api/shared/facts/{fact_id}/source",
                get(shared_fact_source),
            )
            .route(
                "/api/shared/facts/{fact_id}/used_by",
                get(get_sessions_using_fact),
            )
            .route("/api/shared/facts/version", get(shared_facts_version))
            .route("/api/shared/facts/rollup", post(shared_facts_rollup))
            .route(
                "/api/sessions/{id}/used_at_startup",
                post(record_used_at_startup),
            )
            .route(
                "/api/sessions/{id}/used_at_startup",
                get(get_used_at_startup),
            )
            .route("/api/sessions/{id}/debug/phase", get(debug_phase))
            .route("/api/sessions/{id}/debug/queue", get(debug_queue))
            .route("/api/sessions/{id}/debug/pending_io", get(debug_pending_io))
            .route("/api/sessions/{id}/interrupt", post(session_interrupt))
            .route("/api/sessions/{id}/finished", get(session_finished))
            .route("/api/sessions/{id}/causal_depth", get(session_causal_depth))
            .route("/api/sessions/{id}/invariants", get(session_invariants))
            .route(
                "/api/sessions/{id}/pending_io_count",
                get(session_pending_io_count),
            )
            .route("/api/sessions/{id}/step", get(session_step))
            .route("/api/sessions/{id}/snapshot", get(session_snapshot))
            .route(
                "/api/sessions/{id}/audit/auto_verify",
                get(session_auto_verify_get),
            )
            .route(
                "/api/sessions/{id}/audit/auto_verify",
                post(session_auto_verify_post),
            )
            // B2 修复：reload 从 public_routes 移到 protected_routes。
            // 该端点会重新加载 core_eval.json + rules_dir，是运营操作，
            // 未认证用户不应触发（DoS 风险 + rules_dir 可写时注入恶意规则）。
            .route("/api/rules/reload", post(reload_rules_handler))
            .route("/api/rules", get(get_rules))
            // 规则命中统计查询面（聚合器数据，需认证）
            .route("/api/rules/hit-stats", get(hit_stats_handler))
            .route(
                "/api/rules/hit-stats/{rule_key}",
                get(hit_stats_rule_handler),
            )
            // T2: 快照包导入端点（36 号 集成契约）——写 rules_dir 的运营操作，走受保护路由
            .route(
                "/api/bundles/import",
                post(crate::api::bundles::import_bundle_handler),
            )
            .route(
                "/api/bundles/import/dry-run",
                post(crate::api::bundles::import_bundle_dry_run_handler),
            )
            // T4: 报告当前激活 bundle（版本语义运行配置只读）
            .route(
                "/api/bundles/active",
                get(crate::api::bundles::active_bundles_handler),
            )
            // T5: bundle 导入溯源记录（bundle_imports 表, 只读审计查询）
            .route(
                "/api/bundles/imports",
                get(crate::api::bundles::list_bundle_imports_handler),
            )
            // Q12 段2 P1: 执行侧数据面（SDK/原生服务消费通道, 只读；受保护路由 S5）
            .route(
                "/api/knowledge",
                get(crate::api::knowledge::knowledge_datasets_handler),
            )
            .route(
                "/api/knowledge/{ds}/entries",
                get(crate::api::knowledge::knowledge_entries_handler),
            )
            .route(
                "/api/knowledge/{ds}/entries/{entry_id}",
                get(crate::api::knowledge::knowledge_entry_handler),
            )
            // 权限管理端点族（A-流 权限系统，受认证保护）
            .merge(crate::api::permissions::permissions_router())
            // 模板市场端点族
            .merge(crate::api::marketplace::marketplace_router())
            // 服务端 PDF 导出（/ 实化，纯 Rust 文本型；受认证
            // 保护 + 独立 body 上限 32MB——console 可携带全量审计事实）
            .merge(crate::api::pdf_export::pdf_export_router())
            // P10: 工作空间 + 规则元数据路由 (18 个端点, 受认证保护)
            .merge(evorule_workspace::build_workspace_router())
            // abort 双保险：条件挂载（--allow-abort 关闭时为空 Router）
            .merge(abort_router)
            // rewind/diff 已移至 application/core/time_machine（本地实现）
            // W2b:统一认证中间件(双凭据:静态 user/service token 或
            // 平台会话 token;401 统一 JSON 错误体)。evo-agent 侧车审计桥等
            // 内部调用方沿用静态 service token,无需改造。
            .layer(axum::middleware::from_fn_with_state(
                (auth, self.state.shared_facts.clone()),
                crate::api::platform_auth::unified_auth_middleware,
            ));

        // CORS 白名单（从 self.allowed_origins 构建）

        //

        // 策略：

        // - 列表非空 → 只允许列表中的 Origin（精确匹配）

        // - 列表为空 → 严格拒绝跨域（不允许任何外部 Origin）

        let origins_cloned: Vec<String> = (*self.allowed_origins).clone();

        // S3：检测通配符 origin，warn 提示浏览器兼容性问题

        if origins_cloned.iter().any(|o| o == "*") {
            tracing::warn!(
                "CORS 配置包含通配符 '*'。结合 allow_credentials(true)，\
                 浏览器会拒绝此响应（CORS 规范禁止通配符 + credentials）。\
                 请使用精确 Origin 列表（如 https://app.example.com）替代。"
            );
        }

        let cors = if origins_cloned.is_empty() {
            // 默认模式(未配置 --allowed-origins):放行本机 loopback Origin
            // (localhost / 127.0.0.1 / [::1],任意端口,http/https)。
            //
            // 为什么放行 loopback:开发/演示场景前端端口不固定(vite 5173/5174/
            // 4173、preview 随机端口),固定白名单会造成"首次启动连不上"的摩擦。
            //
            // 安全边界仍然保留:
            // - 外部网站(drive-by)的 Origin 是非 loopback 的(如 http://evil.com),
            //   fetch http://localhost:18080 仍会被本策略拒绝;
            // - 生产部署(监听 0.0.0.0)必须显式配置 --allowed-origins 白名单,
            //   精确到协议+域名+端口,不应依赖本默认值。
            tracing::info!(
                "CORS: 未配置 --allowed-origins,默认放行本机 Origin \
                 (localhost/127.0.0.1/[::1] 任意端口);生产部署请显式配置白名单"
            );

            CorsLayer::new()
                .allow_methods([
                    Method::GET,
                    Method::POST,
                    Method::PUT,
                    Method::DELETE,
                    Method::OPTIONS,
                ])
                .allow_headers([
                    axum::http::header::CONTENT_TYPE,
                    axum::http::header::AUTHORIZATION,
                    axum::http::header::ACCEPT,
                ])
                .allow_origin(AllowOrigin::predicate(|origin, _parts| {
                    // 非 ASCII Origin(不合法)to_str 会 Err,一律拒绝
                    let s = origin.to_str().unwrap_or("");
                    let host = s
                        .strip_prefix("http://")
                        .or_else(|| s.strip_prefix("https://"));
                    match host {
                        Some(h) => {
                            h.starts_with("localhost")
                                || h.starts_with("127.0.0.1")
                                || h.starts_with("[::1]")
                        }
                        None => false,
                    }
                }))
                .allow_credentials(true)
        } else {
            let mut header_vals: Vec<axum::http::HeaderValue> = Vec::new();

            for o in &origins_cloned {
                match axum::http::HeaderValue::from_str(o) {
                    Ok(v) => header_vals.push(v),

                    Err(e) => {
                        tracing::warn!("跳过无效 CORS Origin: {} → {}", o, e);
                    }
                }
            }

            CorsLayer::new()
                .allow_methods([
                    Method::GET,
                    Method::POST,
                    Method::PUT,
                    Method::PATCH,
                    Method::DELETE,
                    Method::OPTIONS,
                ])
                .allow_headers([
                    axum::http::header::CONTENT_TYPE,
                    axum::http::header::AUTHORIZATION,
                    axum::http::header::ACCEPT,
                ])
                .allow_origin(header_vals)
                .allow_credentials(true)
        };

        // 合并路由 + 全局安全层（从内到外：body limit → concurrency → cors → rate limit）

        // 修复：当 resolve_governor_config 返回 None 时，完全跳过 GovernorLayer（真正禁用限速）

        // S2：/metrics 根据 metrics_requires_auth 决定是否需要认证

        // 独立构建 metrics_router，避免改动 public/protected 路由分组的结构

        let metrics_router = Router::<AppState>::new().route("/metrics", get(metrics_handler));

        let metrics_router = if self.metrics_requires_auth {
            metrics_router.layer(axum::middleware::from_fn_with_state(
                (self.auth.clone(), self.state.shared_facts.clone()),
                crate::api::platform_auth::unified_auth_middleware,
            ))
        } else {
            metrics_router
        };

        let router = Router::new()
            .merge(public_routes)
            .merge(protected_routes)
            .merge(metrics_router)
            .layer(axum::middleware::from_fn_with_state(
                self.state.metrics.clone(),
                http_metrics_middleware,
            ))
            .layer(RequestBodyLimitLayer::new(MAX_REQUEST_BODY_BYTES))
            .layer(tower::limit::ConcurrencyLimitLayer::new(MAX_CONCURRENCY))
            .layer(cors);

        // OpenAPI Swagger UI（--openapi-ui 显式开启，默认关闭避免生产暴露接口面）

        // utoipa-swagger-ui 9 对 axum 0.8 实现了 `From<SwaggerUi> for Router<S>`，可直接 merge。

        // 注意：不能再用 `.url("/api/openapi.json", spec)` 自带 spec 路由——public_routes

        // 已注册同路径的 openapi_json handler，会触发 "Overlapping method route" panic。

        // 改为 Config 指向已有端点，让 UI 从 /api/openapi.json 拉取规范。

        let router = if self.openapi_ui {
            tracing::info!("OpenAPI Swagger UI 已挂载：GET /api/docs");

            router.merge(
                utoipa_swagger_ui::SwaggerUi::new("/api/docs")
                    .config(utoipa_swagger_ui::Config::new(["/api/openapi.json"])),
            )
        } else {
            router
        };

        // 静态前端托管（--web-dir 显式开启，默认关闭）
        //
        // 挂为 fallback_service：只接管未命中 /api 与 /metrics 路由的请求，
        // 未知路径回退 index.html（SvelteKit adapter-static 的 SPA fallback）。

        let router = if let Some(dir) = self.web_dir.clone() {
            tracing::info!(
                "静态前端已挂载：--web-dir {}（未命中路由回退 index.html）",
                dir.display()
            );

            router.fallback_service(
                tower_http::services::ServeDir::new(&dir)
                    .append_index_html_on_directories(true)
                    .fallback(tower_http::services::ServeFile::new(dir.join("index.html"))),
            )
        } else {
            router
        };

        match resolve_governor_config(self.rate_limit_per_sec, self.rate_limit_burst) {
            None => {
                tracing::info!("速率限制已禁用（--no-rate-limit / per_sec=0）");

                router.with_state(self.state.clone())
            }

            Some(cfg) => {
                // 修正(2026-09-01,W3 演练排障发现):原公式 burst/per_sec
                // 会把默认配置误报为"1 req/s(burst=200)",误导排障(实际持续速率
                // = per_sec req/s:令牌桶每 1000/per_sec 毫秒回补 1 个令牌,
                // burst 只是桶容量/突发上限,实测 135+ req/s 持续零 429)。
                tracing::info!(
                    "速率限制已启用：{} req/s（burst={}）",
                    self.rate_limit_per_sec,
                    self.rate_limit_burst
                );

                router
                    .layer(tower_governor::GovernorLayer::new(cfg))
                    .with_state(self.state.clone())
            }
        }
    }

    /// 启动 HTTP 服务器
    ///
    ///
    /// 使用 `into_make_service_with_connect_info::<SocketAddr>` 注入客户端 IP，
    ///
    /// 以支持 `GovernorLayer`（速率限制）的按 IP 限流。
    ///
    #[allow(dead_code)]
    pub async fn serve(self) -> Result<(), std::io::Error> {
        // H6: 此方法为预留 API（main.rs 使用 build_router + axum::serve 自行启动以支持优雅退出）

        let router = self.build_router();

        let listener = tokio::net::TcpListener::bind(&self.addr).await?;

        tracing::info!("Governance HTTP server listening on {}", self.addr);

        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
    }
}

// ====================================================================

// 规则查询 + 校验 API

// ====================================================================

/// 当前生效规则响应

#[derive(Debug, Serialize, ToSchema)]

pub struct RulesResponse {
    /// 规则数量
    pub count: usize,

    /// 当前生效的 transform 规则列表（core_eval）
    pub core_eval: Vec<serde_json::Value>,
}

/// 获取当前生效的 core_eval 规则（GET /api/rules，014 合法 API #2）

#[utoipa::path(

    get,

    path = "/api/rules",

    tag = "rules",

    responses(

        (status = 200, description = "当前生效规则", body = RulesResponse)

    )

)]

async fn get_rules(State(api): State<SessionApi>) -> Result<Json<RulesResponse>, StatusCode> {
    let core_eval = api.core_eval();

    let core_eval_serde: Vec<serde_json::Value> = core_eval.iter().map(tcb_to_serde).collect();

    Ok(Json(RulesResponse {
        count: core_eval.len(),

        core_eval: core_eval_serde,
    }))
}

// =============================================================================
// 规则命中统计查询面（/api/rules/hit-stats）
// =============================================================================

/// hit-stats 查询参数
#[derive(Debug, serde::Deserialize, ToSchema)]
pub struct HitStatsQuery {
    /// 规则集版本（缺省 = 当前版本）
    pub version: Option<String>,
    /// 清单筛选：`all`（默认，命中清单+零命中清单）| `hit`（仅命中）| `zero`（仅零命中/死规则候选）
    pub filter: Option<String>,
}

/// GET /api/rules/hit-stats —— 规则命中统计清单
///
/// 返回指定（或当前）规则集版本下各规则的结构命中计数与零命中清单。
/// 数据源为 server 侧聚合器（消费引擎 TransitionTrace 归因事实），进程内存
/// 存储：重启后计数归零，全量权威在审计链 WAL（可后续派生重建）。
#[utoipa::path(
    get,
    path = "/api/rules/hit-stats",
    tag = "rules",
    params(
        ("version" = String, Query, description = "规则集版本（缺省=当前版本）"),
        ("filter" = String, Query, description = "筛选：all（默认）| hit | zero")
    ),
    responses(
        (status = 200, description = "规则命中统计清单", body = crate::api::hit_stats::HitStatsResponse),
        (status = 400, description = "filter 参数非法"),
        (status = 404, description = "指定版本不存在（超出保留窗口）")
    )
)]
pub async fn hit_stats_handler(
    State(api): State<SessionApi>,
    Query(params): Query<HitStatsQuery>,
) -> Result<Json<crate::api::hit_stats::HitStatsResponse>, StatusCode> {
    let filter = crate::api::hit_stats::HitFilter::parse(params.filter.as_deref())
        .ok_or(StatusCode::BAD_REQUEST)?;
    api.hit_stats
        .snapshot(params.version.as_deref(), filter)
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// GET /api/rules/hit-stats/{rule_key} —— 单规则跨版本命中切片
///
/// `rule_key` 形如 `{index}@{source}`：`index` 为合并规则列表下标，`source`
/// 为 URL 编码的来源标签（宪法规则集为 `core_eval`，业务规则为 rules_dir
/// 相对文件路径）。例：`0@core_eval`、
/// `2@rules%2Fbundles%2Fexpenses.json`。
#[utoipa::path(
    get,
    path = "/api/rules/hit-stats/{rule_key}",
    tag = "rules",
    params(("rule_key" = String, Path, description = "规则键：{index}@{source}")),
    responses(
        (status = 200, description = "单规则跨版本命中切片（无统计的已知版本计 0）", body = crate::api::hit_stats::RuleSeriesResponse),
        (status = 400, description = "rule_key 格式非法（需 {index}@{source}）")
    )
)]
pub async fn hit_stats_rule_handler(
    State(api): State<SessionApi>,
    Path(rule_key): Path<String>,
) -> Result<Json<crate::api::hit_stats::RuleSeriesResponse>, StatusCode> {
    let (index_raw, source) = rule_key.split_once('@').ok_or(StatusCode::BAD_REQUEST)?;
    let index: u64 = index_raw.trim().parse().map_err(|_| StatusCode::BAD_REQUEST)?;
    if source.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    Ok(Json(api.hit_stats.rule_series(source, index)))
}

/// 执行侧已绑定服务信息（C5：`GET /api/services` 能力对账）
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BoundServiceInfo {
    pub name: String,
    /// `native`（宿主自带进程内模块）| `plugin`（外部插件包，plugin.json 声明）|
    /// `registry`（service_registry.json 显式绑定）
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 归属插件 id（如 "demo-services"/"physics-services"/"finance-config"）；registry 条目无归属，为 None
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    /// 敏感服务标记（声明表/plugin.json 派生）：true 时禁止 REST 直调（invoke 403），必须走会话审计链
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sensitive: bool,
    /// 参数契约（OpenAI function parameters 子集；外部插件包 plugin.json 声明）：
    /// LLM 消费方据此生成动态工具 schema，可带参调用。
    /// native/registry 来源缺省 None（无参数契约声明，消费方降级空 schema）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
}

/// GET /api/services —— 执行侧已绑定服务能力对账（C5）
///
/// 返回执行侧可路由服务全集：插件进程内服务（`native`，main.rs 从 PLUGIN_DEFS
/// 派生注入，含 per-plugin 归属与声明表 description）+ service_registry.json
/// （`registry`，带配置的 version/description）。
/// 供场景包导入前的服务需求预检（02 方案 §3.5）与治理侧服务目录
/// （`GET /v1/services`）做服务需求核对。
#[utoipa::path(
    get,
    path = "/api/services",
    tag = "services",
    responses(
        (status = 200, description = "执行侧已绑定服务全集（native + registry）", body = Vec<BoundServiceInfo>)
    )
)]
pub async fn list_services_handler(State(api): State<SessionApi>) -> Json<Vec<BoundServiceInfo>> {
    let mut out: Vec<BoundServiceInfo> = api.native_services.as_ref().clone();
    // 同名去重：registry 仅是 HTTP 回落绑定，native/plugin 条目携带完整元数据
    // （sensitive/parameters/plugin 归属）。同名时以 native/plugin 源为准，避免
    // 对账清单出现双源重复条目——消费方（LLM 消费桥/服务目录）按名索引一旦命中
    // 缺元数据的 registry 条目，会发生敏感守卫降级与参数契约丢失。
    // 去重语义与 invoke 路由优先级一致（插件路由原生优先 → registry HTTP 回落）。
    let known: std::collections::HashSet<String> =
        out.iter().map(|s| s.name.clone()).collect();
    for meta in api.registry_services.iter() {
        if known.contains(&meta.name) {
            continue;
        }
        out.push(BoundServiceInfo {
            name: meta.name.clone(),
            source: "registry".to_string(),
            version: meta.version.clone(),
            description: meta.description.clone(),
            plugin: None,
            sensitive: false,
            parameters: None,
        });
    }
    Json(out)
}

/// POST /api/services/{name}/invoke —— 插件服务直调（服务消费契约）
///
/// 与 io_request **同一处理器链**（插件路由原生优先 → registry HTTP 回落），
/// 无第二执行路径。请求 body = 服务 `args`（JSON 对象）。
///
/// 治理语义（fail-fast）：
/// - 未知服务 → 404（附合法名指引）；
/// - native 且 sensitive=true → 403 —— 直调敏感服务=静默绕过会话审批链，
///   禁止（静默通过禁止）；敏感服务必须经会话 call_service 指令走审计与审批；
/// - 未装配服务链 → 503。
#[utoipa::path(
    post,
    path = "/api/services/{name}/invoke",
    tag = "services",
    params(("name" = String, Path, description = "服务名（GET /api/services 对账清单内）")),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "服务执行结果", body = serde_json::Value),
        (status = 403, description = "敏感服务禁止直调（走会话审计链）"),
        (status = 404, description = "服务不在对账清单"),
        (status = 502, description = "服务执行失败（错误透传）"),
        (status = 503, description = "服务链未装配")
    )
)]
pub async fn invoke_service_handler(
    State(api): State<SessionApi>,
    Path(name): Path<String>,
    Json(args): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let sensitive_native = api
        .native_services
        .iter()
        .find(|i| i.name == name)
        .map(|i| i.sensitive);
    let known = sensitive_native.is_some()
        || api.registry_services.iter().any(|m| m.name == name);
    if !known {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!("unknown service: {name}（合法名见 GET /api/services 对账清单）")
            })),
        ));
    }
    if sensitive_native == Some(true) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": format!(
                    "sensitive service {name} 禁止 REST 直调：必须经会话 call_service 指令走审计与审批链"
                )
            })),
        ));
    }
    let Some(chain) = api.service_chain.as_ref() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "service chain 未装配（需 main.rs with_service_chain 注入）" })),
        ));
    };
    let params = serde_to_tcb(serde_json::json!({ "service_name": name, "args": args }));
    match chain.execute(&params).await {
        Ok(result) => Ok(Json(tcb_to_serde(&result))),
        Err(e) => Err((
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": e })),
        )),
    }
}

/// 校验结果响应（与 core ValidationResult 字段一致）
///
///
/// 对应 evorule_governance::rule_validation::ValidationResult，
///
/// 用于 /api/rules/validate 的 200/422 响应 body 强类型标注。
///
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]

pub struct ValidationResultResponse {
    /// 是否通过（所有校验项均无 error）
    pub passed: bool,

    /// 静态校验结果
    pub static_validation: StaticValidationResponse,

    /// 安全分析结果
    pub security_analysis: SecurityAnalysisResponse,

    /// 汇总
    pub summary: ValidationSummaryResponse,
}

/// 静态校验结果

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]

pub struct StaticValidationResponse {
    /// 校验项列表
    pub checks: Vec<ValidationCheckResponse>,

    /// 错误计数
    pub error_count: usize,

    /// 警告计数
    pub warn_count: usize,
}

/// 安全分析结果

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]

pub struct SecurityAnalysisResponse {
    /// 分析项列表
    pub checks: Vec<ValidationCheckResponse>,

    /// 风险计数
    pub risk_count: usize,

    /// 整体风险等级：low / medium / high
    pub risk_level: String,
}

/// 校验汇总

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]

pub struct ValidationSummaryResponse {
    /// 总规则数
    pub total_transforms: usize,

    /// 总错误数
    pub total_errors: usize,

    /// 总警告数
    pub total_warnings: usize,

    /// 总风险数
    pub total_risks: usize,
}

/// 单条校验项

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]

pub struct ValidationCheckResponse {
    /// 校验项名称
    pub name: String,

    /// 是否通过
    pub passed: bool,

    /// 严重级别：error / warn / info
    pub level: String,

    /// 详细描述
    pub message: String,

    /// 关联的 transform 索引（-1 表示全局）
    pub transform_index: i32,
}

/// 请求体：待校验的规则 JSON

#[derive(serde::Deserialize, utoipa::ToSchema)]

pub struct ValidateRulesRequest {
    /// 规则 JSON 字符串（支持三种格式：{transform:[...]}/[...]/{...}）
    pub rules: String,
}

/// 规则校验端点
///
/// 接收 JSON 规则字符串，返回静态验证 + 安全分析结果。
///
/// # 请求格式
/// ```json
/// {"rules": "{\"transform\":[{\"type\":\"noop\"}]}"}
/// ```
///
/// # 响应格式
/// ```json
/// {
/// "passed": true,
/// "static_validation": { "checks": [...], "error_count": 0, "warn_count": 0 },
/// "security_analysis": { "checks": [...], "risk_count": 0, "risk_level": "low" },
/// "summary": { "total_transforms": 1, "total_errors": 0, "total_warnings": 0, "total_risks": 0 }
/// }
/// ```
#[utoipa::path(

    post,

    path = "/api/rules/validate",

    tag = "rules",

    request_body = ValidateRulesRequest,

    responses(

        (status = 200, description = "规则通过校验，返回静态验证 + 安全分析结果", body = ValidationResultResponse),

        (status = 422, description = "规则存在错误，返回校验详情", body = ValidationResultResponse),

        (status = 400, description = "规则 JSON 无法解析")

    )

)]

async fn validate_rules_handler(
    Json(req): Json<ValidateRulesRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)> {
    // === Schema 门禁（线1 防御层, records/77）===
    // 以固化 rule_set v1.0 Schema 为权威基准，先拦截引擎原生结构非法的规则，
    // 再走 governance 详细校验。防止结构非法规则被误判/静默放行。
    let parsed: serde_json::Value = match serde_json::from_str(&req.rules) {
        Ok(v) => v,
        Err(e) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!("JSON 解析失败: {e}"),
                    "passed": false
                })),
            ));
        }
    };

    // 输入形态归一化（与 governance extract_transforms 口径一致）：
    // - { "transform": [...] } → 校验 transform 数组
    // - [...] → 校验数组
    // - {type:...} 单对象 → 视为单条 transform，包装为数组
    if !parsed.is_object() && !parsed.is_array() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "JSON 必须是对象或数组",
                "passed": false
            })),
        ));
    }

    let schema_report = if let Some(arr) = parsed.get("transform").and_then(|v| v.as_array()) {
        evorule_rule_schema::validate_transform_list(&serde_json::Value::Array(arr.clone()))
    } else if parsed.is_array() {
        evorule_rule_schema::validate_transform_list(&parsed)
    } else {
        // 单条 transform 对象 → 包装为数组再校验
        evorule_rule_schema::validate_transform_list(&serde_json::json!([parsed.clone()]))
    };

    if !schema_report.valid {
        return Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "passed": false,
                "schema_gate": "failed",
                "schema_errors": schema_report.errors,
                "message": "规则未通过 Schema 门禁（引擎原生结构非法，参见固化 rule_set v1.0 Schema，records/77）"
            })),
        ));
    }

    match evorule_governance::rule_validation::validate_rules_from_json(&req.rules) {
        Ok(result) => {
            let json = serde_json::to_value(&result).unwrap_or_default();

            let status = if result.passed {
                StatusCode::OK
            } else {
                StatusCode::UNPROCESSABLE_ENTITY
            };

            Ok((status, Json(json)))
        }

        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({

                "error": e,

                "passed": false

            })),
        )),
    }
}

// ============================================================================

// POST /api/rules/reload — 重新从磁盘加载 TCB 宪法 + 业务规则

// ============================================================================

/// 规则热重载响应

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]

pub struct RulesReloadedResponse {
    pub reload_ok: bool,

    pub previous_rules: usize,

    pub current_rules: usize,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 规则热重载 handler
///
/// - 请求：空（`POST /api/rules/reload`，可传空 body `{}`）
/// - 成功：`200 {"reload_ok":true,"previous_rules":N,"current_rules":M}`
/// - 失败：`500 {"reload_ok":false,"previous_rules":N,"current_rules":N,"error":"..."}`（旧规则保留）
#[utoipa::path(

    post,

    path = "/api/rules/reload",

    tag = "rules",

    responses(

        (status = 200, description = "重载成功", body = RulesReloadedResponse),

        (status = 500, description = "重载失败（旧规则保留）", body = RulesReloadedResponse)

    )

)]

async fn reload_rules_handler(
    State(sessions): State<SessionApi>,
) -> Result<(StatusCode, Json<RulesReloadedResponse>), (StatusCode, Json<RulesReloadedResponse>)> {
    let old_len = sessions.core_eval_len();

    match sessions.reload_from_disk().await {
        Ok((old, new_len)) => Ok((
            StatusCode::OK,
            Json(RulesReloadedResponse {
                reload_ok: true,

                previous_rules: old,

                current_rules: new_len,

                error: None,
            }),
        )),

        Err(e) => {
            tracing::error!("rules reload failed: {}", e);

            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(RulesReloadedResponse {
                    reload_ok: false,

                    previous_rules: old_len,

                    current_rules: old_len,

                    error: Some(e),
                }),
            ))
        }
    }
}

/// 速率限制配置决策（纯函数，可单元测试）
///
///
/// 根据 `per_sec` 和 `burst` 参数构造 `GovernorConfig`，决定是否启用限速。
///
///
///
/// # 参数
///
/// - `per_sec`：令牌桶补充周期（秒）。`0` 表示禁用限速。
///
/// - `burst`：令牌桶容量（突发上限）。
///
///
///
/// # 返回
///
/// - `None`：禁用限速（调用方不应添加 `GovernorLayer`）
///
/// - `Some(cfg)`：启用限速，使用返回的配置构造 `GovernorLayer`
///
///
///
/// # 设计理由
///
/// `GovernorConfigBuilder::finish` 可能返回 `None`，旧代码用
///
/// `unwrap_or_else(GovernorConfig::default)` fallback，但 `default` 的限速值
///
/// 很低，会导致 `--no-rate-limit` 名义禁用、实际仍强限速的 bug。
///
/// 抽取为独立函数后，`per_sec == 0` 路径直接返回 `None`，彻底绕过 fallback 陷阱。
///
pub fn resolve_governor_config(
    per_sec: u64,

    burst: u32,
) -> Option<
    tower_governor::governor::GovernorConfig<
        tower_governor::key_extractor::PeerIpKeyExtractor,
        governor::middleware::NoOpMiddleware,
    >,
> {
    if per_sec == 0 {
        return None;
    }

    // tower_governor 0.8 的 `per_second(n)` 语义是"每 n 秒回补 1 个令牌"
    // (period = Duration::from_secs(n)),并非"每秒 n 个请求"。
    // 实测定论(2026-09-01):此前传 per_sec=1 实为 1 req/s,合法流量被 429。
    // 本参数语义 = 持续速率 req/s,故换算 period = 1000/per_sec 毫秒(≥1ms 下限防零)。
    let period_ms = (1000 / per_sec).max(1);

    tower_governor::governor::GovernorConfigBuilder::default()
        .per_millisecond(period_ms)
        .burst_size(burst)
        .finish()
        .or_else(|| {
            tracing::warn!(
                "GovernorConfigBuilder::finish() 返回 None，fallback 到 GovernorConfig::default()"
            );

            Some(tower_governor::governor::GovernorConfig::default())
        })
}

#[cfg(test)]
mod tests {

    #![allow(clippy::unwrap_used)]
    #![allow(clippy::panic, clippy::expect_used)]

    use super::*;

    // ===== resolve_governor_config 单元测试 =====

    // 覆盖 030 文档记录的 --no-rate-limit 修复场景

    #[test]

    fn test_resolve_governor_config_disabled_when_per_sec_zero() {
        // 本次 bug 的核心场景：--no-rate-limit → per_sec=0 → 必须返回 None

        let result = resolve_governor_config(0, 200);

        assert!(
            result.is_none(),
            "per_sec=0 必须返回 None（禁用限速），实际返回: {result:?}"
        );
    }

    #[test]

    fn test_resolve_governor_config_disabled_when_both_zero() {
        // bench 路径：per_sec=0, burst=0 → 必须返回 None

        let result = resolve_governor_config(0, 0);

        assert!(
            result.is_none(),
            "per_sec=0 且 burst=0 必须返回 None，实际返回: {result:?}"
        );
    }

    #[test]

    fn test_resolve_governor_config_enabled_normal() {
        // 默认配置：per_sec=200(=200 req/s,period 5ms), burst=200 → 必须返回 Some

        let result = resolve_governor_config(1, 200);

        assert!(
            result.is_some(),
            "per_sec=1, burst=200 必须返回 Some（启用限速）"
        );
    }

    #[test]

    fn test_resolve_governor_config_enabled_small_burst() {
        // 边界值：per_sec=1, burst=1 → 仍应返回 Some（最小有效限速配置）

        let result = resolve_governor_config(1, 1);

        assert!(
            result.is_some(),
            "per_sec=1, burst=1 必须返回 Some（最小有效限速配置）"
        );
    }

    // ===== 既有单元测试 =====

    #[test]

    fn test_serde_to_tcb_roundtrip() {
        let original = serde_json::json!({

            "name": "test",

            "count": 42,

            "active": true,

            "items": [1, 2, 3]

        });

        let tcb = serde_to_tcb(original.clone());

        let back = tcb_to_serde(&tcb);

        assert_eq!(back, original);
    }

    #[test]

    fn test_tcb_to_serde_null() {
        let tcb = JsonValue::Null;

        let serde = tcb_to_serde(&tcb);

        assert!(serde.is_null());
    }

    #[test]

    fn test_tcb_to_serde_nested() {
        let mut inner = std::collections::BTreeMap::new();

        inner.insert("key".to_string(), JsonValue::String("value".into()));

        let tcb = JsonValue::Object(inner);

        let mut expected = serde_json::Map::new();

        expected.insert(
            "key".to_string(),
            serde_json::Value::String("value".to_string()),
        );

        let serde = tcb_to_serde(&tcb);

        assert_eq!(serde, serde_json::Value::Object(expected));
    }

    #[test]

    fn test_fact_to_sse_data_command() {
        let mut params = std::collections::BTreeMap::new();

        params.insert("attr".to_string(), JsonValue::string("x"));

        params.insert("delta".to_string(), JsonValue::Integer(5));

        let mut instr = std::collections::BTreeMap::new();

        instr.insert("type".to_string(), JsonValue::string("increment"));

        instr.insert("params".to_string(), JsonValue::Object(params));

        let fact = Fact::Command {
            id: FactId(1),

            instruction: JsonValue::Object(instr),
        };

        let json: serde_json::Value = serde_json::from_str(&fact_to_sse_data(&fact)).unwrap();

        assert_eq!(json["type"], "Command");

        assert_eq!(json["id"], 1);

        assert_eq!(json["instruction"]["type"], "increment");
    }

    #[test]

    fn test_fact_to_sse_data_stable() {
        let fact = Fact::Stable {
            id: FactId(10),

            version: 42,
        };

        let json: serde_json::Value = serde_json::from_str(&fact_to_sse_data(&fact)).unwrap();

        assert_eq!(json["type"], "Stable");

        assert_eq!(json["id"], 10);

        assert_eq!(json["version"], 42);
    }

    #[test]

    fn test_fact_to_sse_data_error() {
        let fact = Fact::Error {
            id: FactId(99),

            message: "max rounds exceeded".to_string(),
        };

        let json: serde_json::Value = serde_json::from_str(&fact_to_sse_data(&fact)).unwrap();

        assert_eq!(json["type"], "Error");

        assert_eq!(json["id"], 99);

        assert_eq!(json["message"], "max rounds exceeded");
    }

    #[test]

    fn test_fact_to_sse_data_io_request() {
        use evorule_reactor::IoType;

        let fact = Fact::IoRequest {
            id: FactId(3),

            cause: FactId(1),

            io_type: IoType::call_external(),

            params: JsonValue::Null,
        };

        let json: serde_json::Value = serde_json::from_str(&fact_to_sse_data(&fact)).unwrap();

        assert_eq!(json["type"], "IoRequest");

        assert_eq!(json["id"], 3);

        assert_eq!(json["cause"], 1);

        assert_eq!(json["io_type"], "call_external");
    }

    #[test]

    fn test_fact_to_sse_data_payload_update() {
        let fact = Fact::PayloadUpdate {
            id: FactId(5),

            path: "x".to_string(),

            value: JsonValue::Integer(42),
        };

        let json: serde_json::Value = serde_json::from_str(&fact_to_sse_data(&fact)).unwrap();

        assert_eq!(json["type"], "PayloadUpdate");

        assert_eq!(json["id"], 5);

        assert_eq!(json["path"], "x");

        assert_eq!(json["value"], 42);
    }

    // ====================================================================

    // /api/rules/validate 端点测试

    // ====================================================================

    // 直接调用 validate_rules_handler,覆盖所有异常分支:

    // - 400 BAD_REQUEST: JSON 解析失败

    // - 422 UNPROCESSABLE_ENTITY: 静态验证失败

    // - 200 OK: 验证通过(含 warn 级别不阻断的情况)

    /// 辅助函数: 调用 handler 并返回 (状态码, 响应体)
    async fn call_validate(
        rules: &str,
    ) -> Result<(StatusCode, serde_json::Value), (StatusCode, serde_json::Value)> {
        let req = ValidateRulesRequest {
            rules: rules.to_string(),
        };

        match validate_rules_handler(Json(req)).await {
            Ok((status, json)) => Ok((status, json.0)),

            Err((status, json)) => Err((status, json.0)),
        }
    }

    // --- 400 BAD_REQUEST: JSON 解析失败 ---

    #[tokio::test]

    async fn test_validate_invalid_json_syntax() {
        // 无效 JSON 语法(花括号不匹配)

        let result = call_validate(r#"{"transform":[invalid}"#).await;

        assert!(result.is_err(), "无效 JSON 语法应返回 Err(400)");

        let (status, body) = result.unwrap_err();

        assert_eq!(status, StatusCode::BAD_REQUEST);

        assert_eq!(body["passed"], false);

        assert!(body["error"].as_str().unwrap().contains("JSON 解析失败"));
    }

    #[tokio::test]

    async fn test_validate_empty_string() {
        // 空字符串不是合法 JSON

        let result = call_validate("").await;

        assert!(result.is_err());

        let (status, _) = result.unwrap_err();

        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]

    async fn test_validate_json_null() {
        // JSON null 既不是对象也不是数组

        let result = call_validate("null").await;

        assert!(result.is_err());

        let (status, body) = result.unwrap_err();

        assert_eq!(status, StatusCode::BAD_REQUEST);

        assert!(body["error"].as_str().unwrap().contains("对象或数组"));
    }

    #[tokio::test]

    async fn test_validate_json_primitive() {
        // JSON 原始值(数字)不是对象或数组

        let result = call_validate("12345").await;

        assert!(result.is_err());

        let (status, _) = result.unwrap_err();

        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]

    async fn test_validate_json_string() {
        // JSON 字符串不是对象或数组

        let result = call_validate(r#""hello""#).await;

        assert!(result.is_err());

        let (status, _) = result.unwrap_err();

        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]

    async fn test_validate_json_boolean() {
        // JSON 布尔值不是对象或数组

        let result = call_validate("true").await;

        assert!(result.is_err());

        let (status, _) = result.unwrap_err();

        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    // --- 422 UNPROCESSABLE_ENTITY: 静态验证失败 ---

    #[tokio::test]

    async fn test_validate_empty_transform_array() {
        // transform 数组为空

        let (status, body) = call_validate(r#"{"transform":[]}"#).await.unwrap();

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        assert_eq!(body["passed"], false);

        // Schema 门禁:空 transform 数组被拒（minItems=1）

        assert_eq!(body["schema_gate"], "failed");

        assert!(!body["schema_errors"].as_array().unwrap().is_empty());
    }

    #[tokio::test]

    async fn test_validate_missing_type_field() {
        // transform 缺少 type 字段

        let (status, body) = call_validate(r#"{"transform":[{"params":{"attr":"x"}}]}"#)
            .await
            .unwrap();

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        assert_eq!(body["passed"], false);

        // Schema 门禁:缺 type 字段被拒（transform_rule 必填 type）

        assert_eq!(body["schema_gate"], "failed");

        assert!(body["schema_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e.as_str().unwrap().contains("type")));
    }

    #[tokio::test]

    async fn test_validate_unknown_type() {
        // type 不在白名单中

        let (status, body) = call_validate(r#"{"transform":[{"type":"unknown_type"}]}"#)
            .await
            .unwrap();

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        assert_eq!(body["passed"], false);

        // Schema 门禁:未知 type 被拒（type 枚举仅 6 元指令）

        assert_eq!(body["schema_gate"], "failed");

        assert!(body["schema_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e.as_str().unwrap().contains("unknown_type")));
    }

    #[tokio::test]

    async fn test_validate_set_missing_required_params() {
        // set 指令缺少必填参数 (需要 attr/operation/value,只提供 attr)

        let (status, body) =
            call_validate(r#"{"transform":[{"type":"set","params":{"attr":"x"}}]}"#)
                .await
                .unwrap();

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        assert_eq!(body["passed"], false);

        // Schema 门禁:set 缺 operation/value 被拒（必填）

        assert_eq!(body["schema_gate"], "failed");

        assert!(body["schema_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e.as_str().unwrap().contains("required")));
    }

    #[tokio::test]

    async fn test_validate_branch_missing_domain() {
        // branch 指令缺少必填参数 domain

        let (status, body) = call_validate(r#"{"transform":[{"type":"branch"}]}"#)
            .await
            .unwrap();

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        assert_eq!(body["passed"], false);
    }

    #[tokio::test]

    async fn test_validate_io_request_missing_io_type() {
        // io_request 指令缺少必填参数 io_type

        let (status, body) = call_validate(r#"{"transform":[{"type":"io_request"}]}"#)
            .await
            .unwrap();

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        assert_eq!(body["passed"], false);
    }

    #[tokio::test]

    async fn test_validate_push_missing_instructions() {
        // push 指令缺少必填参数 instructions

        let (status, body) = call_validate(r#"{"transform":[{"type":"push"}]}"#)
            .await
            .unwrap();

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        assert_eq!(body["passed"], false);
    }

    #[tokio::test]

    async fn test_validate_increment_transform_type_rejected() {
        // increment 是指令层类型，不是元指令层 transform 类型（P0-01），Schema 门禁应拒绝

        let (status, body) =
            call_validate(r#"{"transform":[{"type":"increment","params":{"attr":"x"}}]}"#)
                .await
                .unwrap();

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        assert_eq!(body["passed"], false);

        assert_eq!(body["schema_gate"], "failed");
    }

    #[tokio::test]

    async fn test_validate_io_request_params_too_large() {
        // io_request 的 params 超过 10KB 上限

        // 构造一个约 11KB 的大字符串作为 io_type 的值

        let large_value = "x".repeat(11000);

        let rules = serde_json::json!({

            "transform": [{

                "type": "io_request",

                "params": {

                    "io_type": "call_service",

                    "data": large_value

                }

            }]

        })
        .to_string();

        let (status, body) = call_validate(&rules).await.unwrap();

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        assert_eq!(body["passed"], false);

        // params_complete 应报 params 过大

        let checks = &body["static_validation"]["checks"];

        assert!(checks
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "params_complete" && c["level"] == "error"));
    }

    #[tokio::test]

    async fn test_validate_transform_count_exceeds_limit() {
        // transform 数量超过 64 条上限

        // 用合法 set 填充 65 条（超过 TCB 上限 64）

        let transforms: Vec<serde_json::Value> = (0..65)
            .map(|_| {
                serde_json::json!({"type": "set", "params": {"attr": "x", "operation": "set", "value": 1}})
            })
            .collect();

        let rules = serde_json::json!({"transform": transforms}).to_string();

        let (status, body) = call_validate(&rules).await.unwrap();

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        assert_eq!(body["passed"], false);

        // Schema 门禁:transform 数量超限（maxItems=64 / 引擎上限）

        assert_eq!(body["schema_gate"], "failed");

        assert!(body["schema_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e.as_str().unwrap().contains("上限")));
    }

    // --- 200 OK: 验证通过 ---

    #[tokio::test]

    async fn test_validate_noop_transform_rejected() {
        // noop 是指令层类型，不是元指令层 transform 类型（P0-01），Schema 门禁应拒绝

        let (status, body) = call_validate(r#"{"transform":[{"type":"noop"}]}"#)
            .await
            .unwrap();

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        assert_eq!(body["passed"], false);

        assert_eq!(body["schema_gate"], "failed");
    }

    #[tokio::test]

    async fn test_validate_valid_set() {
        // 合法 set 指令

        let (status, body) = call_validate(
            r#"{"transform":[{"type":"set","params":{"attr":"x","operation":"set","value":1}}]}"#,
        )
        .await
        .unwrap();

        assert_eq!(status, StatusCode::OK);

        assert_eq!(body["passed"], true);

        assert_eq!(body["summary"]["total_errors"], 0);
    }

    #[tokio::test]

    async fn test_validate_valid_branch() {
        // 合法 branch 指令(含 on_true/on_false)

        let (status, body) = call_validate(

            r#"{"transform":[{"type":"branch","params":{"domain":{"type":"all","inner":[]},"on_true":[{"type":"set","params":{"attr":"x","operation":"set","value":1}}],"on_false":[{"type":"set","params":{"attr":"x","operation":"set","value":0}}]}}]}"#,

        )

        .await

        .unwrap();

        assert_eq!(status, StatusCode::OK);

        assert_eq!(body["passed"], true);
    }

    #[tokio::test]

    async fn test_validate_valid_io_request() {
        // 合法 io_request 指令

        let (status, body) = call_validate(
            r#"{"transform":[{"type":"io_request","params":{"io_type":"call_service"}}]}"#,
        )
        .await
        .unwrap();

        assert_eq!(status, StatusCode::OK);

        assert_eq!(body["passed"], true);
    }

    #[tokio::test]

    async fn test_validate_single_transform_object() {
        // 单条 transform 对象(非数组,非标准 transform 包装)

        let (status, body) =
            call_validate(r#"{"type":"set","params":{"attr":"x","operation":"set","value":1}}"#)
                .await
                .unwrap();

        assert_eq!(status, StatusCode::OK);

        assert_eq!(body["passed"], true);

        assert_eq!(body["summary"]["total_transforms"], 1);
    }

    #[tokio::test]

    async fn test_validate_top_level_array() {
        // 顶层数组格式

        let (status, body) = call_validate(r#"[{"type":"set","params":{"attr":"x","operation":"set","value":1}},{"type":"set","params":{"attr":"y","operation":"set","value":2}}]"#)
            .await
            .unwrap();

        assert_eq!(status, StatusCode::OK);

        assert_eq!(body["passed"], true);

        assert_eq!(body["summary"]["total_transforms"], 2);
    }

    #[tokio::test]

    async fn test_validate_invalid_operation_rejected() {
        // set 的 operation 不在合法枚举（set/add/sub）→ Schema 门禁拦截（P1-02）

        let (status, body) = call_validate(

            r#"{"transform":[{"type":"set","params":{"attr":"x","operation":"invalid_op","value":1}}]}"#,

        )

        .await

        .unwrap();

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        assert_eq!(body["passed"], false);

        assert_eq!(body["schema_gate"], "failed");
    }

    #[tokio::test]

    async fn test_validate_multiple_errors() {
        // 多条规则各有不同错误,验证 error 计数正确

        let (status, body) = call_validate(
            r#"{"transform":[

                {"type":"unknown_type"},

                {"params":{}},

                {"type":"set","params":{"attr":"x"}}

            ]}"#,
        )
        .await
        .unwrap();

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        assert_eq!(body["passed"], false);

        // Schema 门禁:三条规则均结构非法,应至少报 3 条 schema 错误

        assert_eq!(body["schema_gate"], "failed");

        assert!(
            body["schema_errors"].as_array().unwrap().len() >= 3,
            "应有至少 3 条 schema 错误"
        );
    }

    #[tokio::test]

    async fn test_validate_response_structure() {
        // 验证响应体的完整结构(所有必需字段都存在)

        let (status, body) = call_validate(
            r#"{"transform":[{"type":"set","params":{"attr":"x","operation":"set","value":1}}]}"#,
        )
        .await
        .unwrap();

        assert_eq!(status, StatusCode::OK);

        // 顶层字段

        assert!(body["passed"].is_boolean());

        assert!(body["static_validation"].is_object());

        assert!(body["security_analysis"].is_object());

        assert!(body["summary"].is_object());

        // static_validation 子字段

        assert!(body["static_validation"]["checks"].is_array());

        assert!(body["static_validation"]["error_count"].is_u64());

        assert!(body["static_validation"]["warn_count"].is_u64());

        // security_analysis 子字段

        assert!(body["security_analysis"]["checks"].is_array());

        assert!(body["security_analysis"]["risk_count"].is_u64());

        assert!(body["security_analysis"]["risk_level"].is_string());

        // summary 子字段

        assert!(body["summary"]["total_transforms"].is_u64());

        assert!(body["summary"]["total_errors"].is_u64());

        assert!(body["summary"]["total_warnings"].is_u64());

        assert!(body["summary"]["total_risks"].is_u64());
    }

    #[tokio::test]

    async fn test_validate_error_response_structure() {
        // 验证错误响应体结构(400 分支)

        let result = call_validate("not_json").await;

        assert!(result.is_err());

        let (_, body) = result.unwrap_err();

        assert_eq!(body["passed"], false);

        assert!(body["error"].is_string());
    }

    // ====================================================================

    // Handler oneshot 测试（通过真实 build_router 验证端到端）

    // ====================================================================

    // 使用 GovernanceServer::bench 构建无认证、无限速的路由，

    // 通过 tower::ServiceExt::oneshot 发送请求并检查响应。

    // 覆盖路由注册、FromRef 状态提取、中间件链、handler 响应格式。

    use crate::metrics_impl::shared_prometheus_metrics;

    use evorule_reactor::Reactor;

    use tower::ServiceExt;

    /// 构造测试用 AppState（最小依赖，无需加载 core_eval.json）
    ///
    ///
    /// 返回 `(AppState, ReadinessFlag)`，flag 可用于测试 readiness 端点的两种状态。
    ///
    ///
    ///
    /// 内部 spawn 的反应器在 rx/event_tx/handle drop 后仍会运行（孤儿任务）：
    ///
    /// - ReactorHandle 无 Drop impl，丢弃不会 abort 任务
    ///
    /// - emit_fact 对无接收者的 broadcast send 已优雅处理（debug 日志，不 panic）
    ///
    /// - 孤儿任务在 `#[tokio::test]` 运行时 drop 时被自动取消
    ///
    /// 服务对账同名去重：registry 回落绑定与 native/plugin 源同名时，
    /// 对账清单必须只保留 native/plugin 条目（带 sensitive/parameters/plugin
    /// 全元数据），消费方按名索引不再有命中缺元数据条目的形态；仅 registry
    /// 绑定的服务不受去重影响，照常列出。
    #[tokio::test]
    async fn test_list_services_dedups_same_name_registry_entry() {
        let mut instr = std::collections::BTreeMap::new();

        instr.insert("type".to_string(), JsonValue::string("noop"));

        let core_eval = vec![JsonValue::Object(instr)];

        let sessions = SessionApi::new(core_eval, 100)
            .with_native_services(vec![BoundServiceInfo {
                name: "finance_config_get".to_string(),
                source: "plugin".to_string(),
                version: Some("0.5.0".to_string()),
                description: Some("插件包声明条目（携带完整元数据）".to_string()),
                plugin: Some("finance-config".to_string()),
                sensitive: false,
                parameters: Some(serde_json::json!({
                    "type": "object",
                    "properties": { "key": { "type": "string" } },
                    "required": ["key"]
                })),
            }])
            .with_registry_services([
                ServiceMeta {
                    name: "finance_config_get".to_string(),
                    version: None,
                    description: Some("registry 同名回落绑定".to_string()),
                },
                ServiceMeta {
                    name: "registry_only_service".to_string(),
                    version: Some("1.0.0".to_string()),
                    description: Some("仅 registry 绑定的服务".to_string()),
                },
            ]);

        let list = list_services_handler(axum::extract::State(sessions)).await.0;

        let dupes = list
            .iter()
            .filter(|s| s.name == "finance_config_get")
            .count();
        assert_eq!(dupes, 1, "同名服务在对账清单必须唯一");

        let kept = list
            .iter()
            .find(|s| s.name == "finance_config_get")
            .unwrap();
        assert_eq!(kept.source, "plugin", "同名时必须保留 native/plugin 源");
        assert_eq!(kept.plugin.as_deref(), Some("finance-config"));
        assert!(kept.parameters.is_some(), "保留条目必须携带参数契约");

        let only = list
            .iter()
            .find(|s| s.name == "registry_only_service")
            .unwrap();
        assert_eq!(only.source, "registry", "仅 registry 绑定的服务不被去重误伤");
    }
    fn make_test_state() -> (AppState, ReadinessFlag) {
        let mut instr = std::collections::BTreeMap::new();

        instr.insert("type".to_string(), JsonValue::string("noop"));

        let core_eval = vec![JsonValue::Object(instr)];

        let reactor = Reactor::builder(core_eval.clone()).max_rounds(100).build();

        let (tx, _rx, _event_tx, _handle, facts_log) = reactor.spawn();

        let auditor = Auditor::new(facts_log.clone());

        let governance = GovernanceApi::new(tx, facts_log, auditor);

        // P10: 构造测试用 WorkspaceState (内存 SQLite + 桥接到 sessions)

        let ws_db = Arc::new(evorule_workspace::WorkspaceDb::in_memory().unwrap());

        // ①: SessionApi 接线 workspace_db(close_session 生产会话保护 +
        // reaper 保活/自愈依赖);与 main.rs 生产装配时序一致
        let sessions = SessionApi::new(core_eval, 100).with_workspace_db(ws_db.clone());

        let metrics: SharedMetrics = shared_prometheus_metrics().unwrap();

        let readiness: ReadinessFlag = Arc::new(AtomicBool::new(true));

        let shared_facts = SharedFactsLog::new();

        let session_ops: Arc<dyn evorule_workspace::SessionOps> = Arc::new(sessions.clone());

        let ws_service = Arc::new(evorule_workspace::WorkspaceService::new(
            ws_db.clone(),
            session_ops.clone(),
        ));

        let rule_meta_service = Arc::new(evorule_workspace::RuleMetaService::new(ws_db.clone()));

        // 构造沙盒/发布/滚动 session 服务 (SANDBOX_ORCHESTRATION_DESIGN.md §3, PUBLISH_QUEUE_DESIGN.md §3)

        let switcher = evorule_workspace::SessionSwitchedBroadcaster::new();

        let sandbox_service = Arc::new(evorule_workspace::SandboxService::new(
            ws_db.clone(),
            session_ops.clone(),
        ));

        let rolling_session = evorule_workspace::RollingSessionService::new(
            ws_db.clone(),
            session_ops,
            switcher.clone(),
        );

        let publish_service = Arc::new(evorule_workspace::PublishService::new(
            ws_db.clone(),
            rolling_session,
            // 审计⑥ 批 B C5: 发布链落盘目标; 此测试不触发发布, 临时目录占位
            std::env::temp_dir().join("evorule-apitest-rules"),
        ));

        let verdict_service = Arc::new(evorule_workspace::VerdictService::new(ws_db.clone()));

        let workspace_state = evorule_workspace::WorkspaceState::new(
            ws_service,
            rule_meta_service,
            sandbox_service,
            publish_service,
            Arc::new(switcher),
            verdict_service,
        );

        (
            AppState::new(
                governance,
                sessions,
                metrics,
                readiness.clone(),
                shared_facts,
                workspace_state,
                Arc::new(InputSanitizer::with_default_rules()),
            ),
            readiness,
        )
    }

    /// 构造测试用 Router（bench 模式：无认证、无限速，适合 oneshot）
    fn make_test_router(state: &AppState) -> Router {
        GovernanceServer::bench(state.clone(), "0.0.0.0:0".to_string()).build_router()
    }

    /// 构造测试用 Router（`allow_abort: true`，用于强制中止端点测试）
    ///
    /// `bench` 默认 `allow_abort: false`（双保险），abort 路由不会挂载，
    /// 因此单独构造启用 abort 的 Router。
    fn make_abort_router(state: &AppState) -> Router {
        GovernanceServer::new(
            state.clone(),
            AuthConfig::disabled(),
            "0.0.0.0:0".to_string(),
            0,
            0,
            vec![],
            false,
            false,
            true,
            None,
        )
        .build_router()
    }

    /// 发送 oneshot JSON 请求并返回 (状态码, 响应体 JSON)
    ///
    ///
    /// 响应体非 JSON 时返回 `Value::Null`（如 `/metrics` 返回纯文本）。
    ///
    async fn oneshot_json(
        router: Router,

        method: &str,

        uri: &str,

        body: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let mut builder = axum::http::Request::builder().method(method).uri(uri);

        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }

        let request = if let Some(b) = body {
            builder.body(axum::body::Body::from(b.to_string())).unwrap()
        } else {
            builder.body(axum::body::Body::empty()).unwrap()
        };

        let response = router.oneshot(request).await.unwrap();

        let status = response.status();

        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();

        let json: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);

        (status, json)
    }

    // --- 静态前端托管（--web-dir） ---

    #[tokio::test]

    async fn test_web_dir_serves_spa_fallback() {
        let (state, _) = make_test_state();

        let tmp = tempfile::tempdir().unwrap();

        std::fs::write(tmp.path().join("index.html"), "<html>evorule-web</html>").unwrap();

        let assets = tmp.path().join("assets");

        std::fs::create_dir_all(&assets).unwrap();

        std::fs::write(assets.join("app.js"), "console.log(1);").unwrap();

        let router = GovernanceServer::new(
            state,
            AuthConfig::disabled(),
            "0.0.0.0:0".to_string(),
            0,
            0,
            vec![],
            false,
            false,
            false,
            Some(tmp.path().to_path_buf()),
        )
        .build_router();

        // GET / → index.html（目录默认页）

        let (status, _) = oneshot_json(router.clone(), "GET", "/", None).await;

        assert_eq!(status, StatusCode::OK);

        // 未命中路径 → SPA fallback 回 index.html

        let (status, _) = oneshot_json(router.clone(), "GET", "/some/spa/route", None).await;

        assert_eq!(status, StatusCode::OK);

        // 静态资源文件按路径命中

        let (status, _) = oneshot_json(router.clone(), "GET", "/assets/app.js", None).await;

        assert_eq!(status, StatusCode::OK);

        // /api 路由不被静态托管遮蔽

        let (status, json) = oneshot_json(router, "GET", "/api/health", None).await;

        assert_eq!(status, StatusCode::OK);

        assert_eq!(json["message"], "ok");
    }

    // --- LLM 审计形态判定（K 约束族：IoSubscriber 跳过谓词） ---

    #[test]
    fn test_is_llm_audit_request_shape() {
        // 审计形态：call_external + messages + 无 service_name/name
        let audit = serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "audit_purpose": "draft_rule"
        });
        assert!(is_llm_audit_request(
            &IoType::call_external(),
            &serde_to_tcb(audit.clone())
        ));

        // service 调用形态（有 service_name）→ 不跳过，照常分发
        let service = serde_json::json!({
            "service_name": "inverse_kinematics_solver",
            "args": {}
        });
        assert!(!is_llm_audit_request(
            &IoType::call_external(),
            &serde_to_tcb(service)
        ));

        // name 别名形态 → 不跳过
        let named = serde_json::json!({ "name": "svc", "args": {} });
        assert!(!is_llm_audit_request(
            &IoType::call_external(),
            &serde_to_tcb(named)
        ));

        // 无 messages 的 call_external → 不跳过
        let bare = serde_json::json!({ "model": "m" });
        assert!(!is_llm_audit_request(
            &IoType::call_external(),
            &serde_to_tcb(bare)
        ));

        // 其他 io_type → 不跳过
        assert!(!is_llm_audit_request(
            &IoType::call_service(),
            &serde_to_tcb(audit.clone())
        ));
    }

    // --- 消费方本地工具形态判定（IoSubscriber 跳过谓词） ---

    #[test]
    fn test_is_agent_tool_request_shape() {
        // 工具形态：call_service + tool_name + 无 service_name/name → 跳过自动应答
        let tool = serde_json::json!({
            "tool_name": "file_write",
            "args": { "path": "workspace/expenses_2026.json", "content": "45.50" }
        });
        assert!(is_agent_tool_request(
            &IoType::call_service(),
            &serde_to_tcb(tool.clone())
        ));
        // 合并谓词同样命中
        assert!(is_external_executor_request(
            &IoType::call_service(),
            &serde_to_tcb(tool)
        ));

        // 平台 HTTP 路由形态（有 service_name）→ 不跳过，内置订阅者照常分发
        let service = serde_json::json!({
            "service_name": "payroll_svc",
            "args": {}
        });
        assert!(!is_agent_tool_request(
            &IoType::call_service(),
            &serde_to_tcb(service.clone())
        ));
        assert!(!is_external_executor_request(
            &IoType::call_service(),
            &serde_to_tcb(service)
        ));

        // name 别名形态 → 不跳过（平台路由别名归 ServiceRegistryHandler）
        let named = serde_json::json!({ "name": "svc", "args": {} });
        assert!(!is_agent_tool_request(
            &IoType::call_service(),
            &serde_to_tcb(named)
        ));

        // 无 tool_name 的 call_service → 不跳过
        let bare = serde_json::json!({ "args": {} });
        assert!(!is_agent_tool_request(
            &IoType::call_service(),
            &serde_to_tcb(bare)
        ));

        // 其他 io_type 携带 tool_name → 两谓词均不命中（tool_name 仅在 call_service 上表意）
        let wrong_type = serde_json::json!({ "tool_name": "file_write", "args": {} });
        assert!(!is_agent_tool_request(
            &IoType::call_external(),
            &serde_to_tcb(wrong_type.clone())
        ));
        assert!(!is_external_executor_request(
            &IoType::call_external(),
            &serde_to_tcb(wrong_type)
        ));
        // call_external + messages 仍由 LLM 审计谓词接管（合并谓词回归）
        let with_messages =
            serde_json::json!({ "model": "m", "messages": [{"role": "user", "content": "hi"}] });
        assert!(is_external_executor_request(
            &IoType::call_external(),
            &serde_to_tcb(with_messages)
        ));
    }

    // --- 公开端点（免认证） ---

    #[tokio::test]

    async fn test_health_oneshot() {
        let (state, _) = make_test_state();

        let (status, json) =
            oneshot_json(make_test_router(&state), "GET", "/api/health", None).await;

        assert_eq!(status, StatusCode::OK);

        assert_eq!(json["success"], true);

        assert_eq!(json["message"], "ok");
    }

    #[tokio::test]

    async fn test_liveness_oneshot() {
        let (state, _) = make_test_state();

        let (status, json) = oneshot_json(
            make_test_router(&state),
            "GET",
            "/api/health/liveness",
            None,
        )
        .await;

        assert_eq!(status, StatusCode::OK);

        assert_eq!(json["message"], "alive");
    }

    #[tokio::test]

    async fn test_readiness_ready_oneshot() {
        let (state, _) = make_test_state();

        let (status, json) = oneshot_json(
            make_test_router(&state),
            "GET",
            "/api/health/readiness",
            None,
        )
        .await;

        assert_eq!(status, StatusCode::OK);

        assert_eq!(json["message"], "ready");
    }

    #[tokio::test]

    async fn test_readiness_not_ready_oneshot() {
        // 模拟优雅退出：readiness flag 设为 false → 503

        let (state, flag) = make_test_state();

        flag.store(false, std::sync::atomic::Ordering::SeqCst);

        let request = axum::http::Request::builder()
            .method("GET")
            .uri("/api/health/readiness")
            .body(axum::body::Body::empty())
            .unwrap();

        let response = make_test_router(&state).oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]

    async fn test_metrics_endpoint_oneshot() {
        let (state, _) = make_test_state();

        // 先提交一条命令触发 commands_total 计数器

        // （IntCounterVec 是惰性创建子指标，未调用 inc_commands 时 TextEncoder 不输出该指标）

        let body = r#"{"instruction":{"type":"noop"}}"#;

        let _ = oneshot_json(make_test_router(&state), "POST", "/api/command", Some(body)).await;

        // SharedMetrics 通过 Arc 共享，第二次 oneshot 看到同一份指标

        let request = axum::http::Request::builder()
            .method("GET")
            .uri("/metrics")
            .body(axum::body::Body::empty())
            .unwrap();

        let response = make_test_router(&state).oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();

        let text = String::from_utf8(bytes.to_vec()).unwrap();

        assert!(text.contains("evorule_sessions_active"));

        assert!(text.contains("evorule_commands_total"));

        assert!(text.contains("noop"), "应包含命令类型标签 type=\"noop\"");
    }

    // --- 会话管理端点 ---

    #[tokio::test]

    async fn test_list_sessions_empty_oneshot() {
        let (state, _) = make_test_state();

        let (status, json) =
            oneshot_json(make_test_router(&state), "GET", "/api/sessions", None).await;

        assert_eq!(status, StatusCode::OK);

        assert_eq!(json["sessions"], serde_json::json!([]));
    }

    #[tokio::test]

    async fn test_create_session_oneshot() {
        let (state, _) = make_test_state();

        let (status, json) =
            oneshot_json(make_test_router(&state), "POST", "/api/sessions", None).await;

        assert_eq!(status, StatusCode::OK);

        assert!(json["session_id"].is_number());

        assert_eq!(json["message"], "Session created");
    }

    #[tokio::test]

    async fn test_close_nonexistent_session_404() {
        let (state, _) = make_test_state();

        let (status, _) = oneshot_json(
            make_test_router(&state),
            "DELETE",
            "/api/sessions/9999",
            None,
        )
        .await;

        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]

    async fn test_session_state_nonexistent_404() {
        let (state, _) = make_test_state();

        let (status, _) = oneshot_json(
            make_test_router(&state),
            "GET",
            "/api/sessions/9999/state",
            None,
        )
        .await;

        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]

    async fn test_session_command_nonexistent_404() {
        let (state, _) = make_test_state();

        let body = r#"{"instruction":{"type":"noop"}}"#;

        let (status, _) = oneshot_json(
            make_test_router(&state),
            "POST",
            "/api/sessions/9999/command",
            Some(body),
        )
        .await;

        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]

    async fn test_session_full_flow_oneshot() {
        let (state, _) = make_test_state();

        // 1. 创建会话

        let (status, json) =
            oneshot_json(make_test_router(&state), "POST", "/api/sessions", None).await;

        assert_eq!(status, StatusCode::OK);

        let session_id = json["session_id"].as_u64().unwrap();

        // 2. 查询会话状态（含 reactor 子对象）

        let uri = format!("/api/sessions/{session_id}/state");

        let (status, json) = oneshot_json(make_test_router(&state), "GET", &uri, None).await;

        assert_eq!(status, StatusCode::OK);

        assert!(json["payload"].is_object());

        assert!(json["version"].is_number());

        assert!(json["reactor"].is_object());

        // 3. 提交命令到会话

        let uri = format!("/api/sessions/{session_id}/command");

        let body = r#"{"instruction":{"type":"noop"}}"#;

        let (status, json) = oneshot_json(make_test_router(&state), "POST", &uri, Some(body)).await;

        assert_eq!(status, StatusCode::OK);

        assert_eq!(json["success"], true);

        // 4. 关闭会话

        let uri = format!("/api/sessions/{session_id}");

        let (status, json) = oneshot_json(make_test_router(&state), "DELETE", &uri, None).await;

        assert_eq!(status, StatusCode::OK);

        assert_eq!(json["message"], "Session closed");
    }

    /// F5 回归（audit-chain 2026-08-28）：审计报告按需注入完整 Fact 内容。
    /// - 默认（include_content 缺省）：entries 无 content_json（向后兼容，轻量）；
    /// - include_content=true：每条 entry 附 content_json（Fact::to_json 全量内容），
    ///   外部审计者可从 API 重建"命令/IO 参数与结果"（P3-N1 兑现）。
    #[tokio::test]

    async fn test_session_audit_include_content_oneshot() {
        let (state, _) = make_test_state();

        // 1. 创建会话

        let (status, json) =
            oneshot_json(make_test_router(&state), "POST", "/api/sessions", None).await;

        assert_eq!(status, StatusCode::OK);

        let session_id = json["session_id"].as_u64().unwrap();

        // 2. 提交命令产生 Command fact（noop 指令，与全流程测试一致）

        let uri = format!("/api/sessions/{session_id}/command");

        let body = r#"{"instruction":{"type":"noop","note":"f5-audit-content"}}"#;

        let (status, _) = oneshot_json(make_test_router(&state), "POST", &uri, Some(body)).await;

        assert_eq!(status, StatusCode::OK);

        // 3. 默认请求：entries 不含 content_json
        // （command 经 channel 异步进 reactor，轮询等待审计条目出现）

        let uri = format!("/api/sessions/{session_id}/audit");

        let mut entries: Vec<serde_json::Value> = Vec::new();

        for _ in 0..40 {
            let (status, json) = oneshot_json(make_test_router(&state), "GET", &uri, None).await;

            assert_eq!(status, StatusCode::OK);

            assert_eq!(json["verified"], true);

            entries = json["entries"].as_array().cloned().unwrap_or_default();

            if !entries.is_empty() {
                break;
            }

            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        assert!(!entries.is_empty(), "提交命令后应有审计条目");

        assert!(
            entries.iter().all(|e| e.get("content_json").is_none()),
            "默认响应不得包含 content_json"
        );

        // 4. include_content=true：entries 附 content_json，且能找到提交的命令内容

        let uri = format!("/api/sessions/{session_id}/audit?include_content=true");

        let (status, json) = oneshot_json(make_test_router(&state), "GET", &uri, None).await;

        assert_eq!(status, StatusCode::OK);

        let entries = json["entries"].as_array().unwrap();

        assert!(!entries.is_empty());
        assert!(
            entries.iter().any(|e| e.get("content_json").is_some()),
            "include_content=true 时 entries 应含 content_json"
        );

        let serialized = serde_json::to_string(&json).unwrap();

        assert!(
            serialized.contains("f5-audit-content"),
            "content_json 应包含提交命令的完整内容（LLM 看到什么可从 API 重建）"
        );
    }

    // --- 014 新增端点单元测试 ---

    #[tokio::test]

    async fn test_session_metadata_oneshot() {
        let (state, _) = make_test_state();

        // 创建会话
        let (status, json) =
            oneshot_json(make_test_router(&state), "POST", "/api/sessions", None).await;

        assert_eq!(status, StatusCode::OK);

        let session_id = json["session_id"].as_u64().unwrap();

        // 查询会话元数据
        let uri = format!("/api/sessions/{session_id}");

        let (status, json) = oneshot_json(make_test_router(&state), "GET", &uri, None).await;

        assert_eq!(status, StatusCode::OK);

        assert_eq!(json["session_id"].as_u64(), Some(session_id));

        assert!(json["parent_session_id"].is_null());

        assert!(json["initial_content_hash"].is_null() || json["initial_content_hash"].is_string());

        assert!(json["idle_secs"].is_number());

        assert_eq!(json["is_finished"], false);

        assert!(json["phase"].is_null() || json["phase"].is_string());

        assert_eq!(json["auto_verify"], false);
    }

    #[tokio::test]

    async fn test_session_metadata_nonexistent_404() {
        let (state, _) = make_test_state();

        let (status, _) =
            oneshot_json(make_test_router(&state), "GET", "/api/sessions/9999", None).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]

    async fn test_rules_get_oneshot() {
        let (state, _) = make_test_state();

        let (status, json) =
            oneshot_json(make_test_router(&state), "GET", "/api/rules", None).await;

        assert_eq!(status, StatusCode::OK);

        // make_test_state 的 core_eval 包含 1 条 noop 规则
        assert_eq!(json["count"], 1);

        assert!(json["core_eval"].is_array());

        assert_eq!(json["core_eval"][0]["type"], "noop");
    }

    #[tokio::test]

    async fn test_shared_facts_version_oneshot() {
        let (state, _) = make_test_state();

        let (status, json) = oneshot_json(
            make_test_router(&state),
            "GET",
            "/api/shared/facts/version",
            None,
        )
        .await;

        assert_eq!(status, StatusCode::OK);

        assert_eq!(json["version"], 0);

        assert_eq!(json["history_len"], 0);
    }

    #[tokio::test]

    async fn test_session_reap_empty_oneshot() {
        let (state, _) = make_test_state();

        let (status, json) =
            oneshot_json(make_test_router(&state), "POST", "/api/sessions/reap", None).await;

        assert_eq!(status, StatusCode::OK);

        assert_eq!(json["finished"], 0);

        assert_eq!(json["expired"], 0);

        assert_eq!(json["total"], 0);
    }

    #[tokio::test]

    async fn test_session_reap_preserves_active_oneshot() {
        let (state, _) = make_test_state();

        // 创建会话（活跃中）
        let (status, json) =
            oneshot_json(make_test_router(&state), "POST", "/api/sessions", None).await;

        assert_eq!(status, StatusCode::OK);

        let session_id = json["session_id"].as_u64().unwrap();

        // 回收不应销毁活跃会话
        let (status, json) =
            oneshot_json(make_test_router(&state), "POST", "/api/sessions/reap", None).await;

        assert_eq!(status, StatusCode::OK);

        assert_eq!(json["total"], 0);

        // 活跃会话仍在列表中
        let (status, json) =
            oneshot_json(make_test_router(&state), "GET", "/api/sessions", None).await;

        assert_eq!(status, StatusCode::OK);

        assert_eq!(json["sessions"][0].as_u64(), Some(session_id));
    }

    // --- ①: reaper 生产会话保活 + 失忆自愈 + 删除保护 ---

    /// ①a: reap_once 保活——生产会话被 touch,非生产会话 TTL 到期被回收。
    /// 内含对照组: 两会话同时创建同时到期,仅生产会话存活 ⇒ 存活来自保活而非 TTL 未到。
    #[tokio::test]
    async fn test_uv079_reap_once_keeps_production_session_alive() {
        let mut instr = std::collections::BTreeMap::new();
        instr.insert("type".to_string(), JsonValue::string("noop"));
        let core_eval = vec![JsonValue::Object(instr)];

        // 短 TTL(100ms) 直接组装 SessionManager,绕过 SessionApi 默认 30min TTL
        let sessions: Arc<Mutex<session::SessionManager>> =
            Arc::new(Mutex::new(session::SessionManager::with_limits(
                core_eval,
                100,
                100,
                std::time::Duration::from_millis(100),
            )));

        let prod_id = sessions.lock().await.create_session().unwrap();
        let other_id = sessions.lock().await.create_session().unwrap();

        let db = Arc::new(evorule_workspace::WorkspaceDb::in_memory().unwrap());
        db.update_production_state(prod_id as i64, 0, "", "test:uv079")
            .unwrap();

        // 等待两会话空闲到期(150ms > 100ms TTL;reap_once 内 touch 会重置
        // 生产会话的 last_activity,故其存活只能来自保活)
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let (finished, expired) = reap_once(&sessions, Some(&db), None).await;

        // 非生产会话被回收;生产会话被 touch 保活存活(幻影引用未发生)
        assert_eq!(finished + expired, 1);
        assert!(sessions.lock().await.get_session(prod_id).is_some());
        assert!(sessions.lock().await.get_session(other_id).is_none());
    }

    /// ①a: reap_once 自愈——幻影引用(current_session_id 指向不存在的
    /// 会话,原始形态)被检测并重建,版本/哈希保留。
    #[tokio::test]
    async fn test_uv079_reap_once_recovers_phantom_production_reference() {
        let (state, _) = make_test_state();
        let api = state.sessions.clone();
        let db = api.workspace_db.clone().unwrap();
        let sessions = api.sessions.clone();

        // 构造幻影: 引用不存在的会话 999,版本 5/哈希 hash-abc
        db.update_production_state(999, 5, "hash-abc", "test:uv079")
            .unwrap();

        reap_once(&sessions, Some(&db), Some(&api)).await;

        let ps = db.get_production_state().unwrap();
        let new_id = ps.current_session_id.expect("自愈后必有生产会话引用");
        assert_ne!(new_id, 999);
        // 语义为"替换会话引用"而非发布: 版本/哈希保留
        assert_eq!(ps.ruleset_version, 5);
        assert_eq!(ps.ruleset_hash.as_deref(), Some("hash-abc"));
        assert_eq!(
            ps.last_operated_by.as_deref(),
            Some("system:reaper-recovery")
        );
        // 新会话真实存活(不再是幻影)
        assert!(sessions.lock().await.get_session(new_id as u64).is_some());
    }

    /// ①b: DELETE 生产会话被 409 拒绝且会话存活;普通会话删除不受影响。
    #[tokio::test]
    async fn test_uv079_close_production_session_rejected_409() {
        let (state, _) = make_test_state();
        let router = make_test_router(&state);
        let api = state.sessions.clone();
        let db = api.workspace_db.clone().unwrap();

        // 创建两个会话,其一标记为生产
        let (prod_id, other_id) = {
            let mgr = api.sessions.lock().await;
            let p = mgr.create_session().unwrap();
            let o = mgr.create_session().unwrap();
            (p, o)
        };
        db.update_production_state(prod_id as i64, 0, "", "test:uv079")
            .unwrap();

        // 删除生产会话 → 409 拒绝(fail-fast,指引走治理流/重启)
        let (status, _) = oneshot_json(
            router.clone(),
            "DELETE",
            &format!("/api/sessions/{prod_id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);

        // 会话未被关闭(保护生效)
        assert!(api.sessions.lock().await.get_session(prod_id).is_some());

        // 删除普通会话 → 200(既有行为不受影响)
        let (status, _) = oneshot_json(
            router.clone(),
            "DELETE",
            &format!("/api/sessions/{other_id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(api.sessions.lock().await.get_session(other_id).is_none());
    }

    // --- 014 强制中止端点（--allow-abort） ---

    #[tokio::test]

    async fn test_session_abort_oneshot() {
        let (state, _) = make_test_state();

        // 创建会话
        let (status, json) =
            oneshot_json(make_test_router(&state), "POST", "/api/sessions", None).await;

        assert_eq!(status, StatusCode::OK);

        let session_id = json["session_id"].as_u64().unwrap();

        // 强制中止（需 --allow-abort 挂载路由）
        let uri = format!("/api/sessions/{session_id}/abort");

        let (status, json) = oneshot_json(make_abort_router(&state), "POST", &uri, None).await;

        assert_eq!(status, StatusCode::OK);

        assert_eq!(json["session_id"].as_u64(), Some(session_id));

        assert_eq!(json["success"], true);

        assert_eq!(json["message"], "Session aborted");
    }

    #[tokio::test]

    async fn test_session_abort_disabled_404() {
        let (state, _) = make_test_state();

        // 未启用 --allow-abort 时路由未挂载 → 404
        let (status, _) = oneshot_json(
            make_test_router(&state),
            "POST",
            "/api/sessions/1/abort",
            None,
        )
        .await;

        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]

    async fn test_session_abort_nonexistent_404() {
        let (state, _) = make_test_state();

        let (status, _) = oneshot_json(
            make_abort_router(&state),
            "POST",
            "/api/sessions/9999/abort",
            None,
        )
        .await;

        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    // --- 单反应器端点（向后兼容） ---

    #[tokio::test]

    async fn test_submit_command_oneshot() {
        let (state, _) = make_test_state();

        let body = r#"{"instruction":{"type":"noop"}}"#;

        let (status, json) =
            oneshot_json(make_test_router(&state), "POST", "/api/command", Some(body)).await;

        assert_eq!(status, StatusCode::OK);

        assert_eq!(json["success"], true);

        assert!(json["fact_id"].is_number());
    }

    #[tokio::test]

    async fn test_submit_command_io_result_singular_rejected() {
        // Opt3 实证：demos 08.json 风格单数 __io_result__ 在提交期即被 Schema 门禁拦截
        let (state, _) = make_test_state();
        let body = r#"{"instruction":{"type":"branch","params":{"domain":{"type":"instruction","instruction_type":"sampling_decider"},"on_true":[{"type":"branch","params":{"domain":{"type":"exists","path":"__exec__.payload.__io_result__"},"on_true":[],"on_false":[]}}]}}}"#;
        let (status, json) =
            oneshot_json(make_test_router(&state), "POST", "/api/command", Some(body)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["success"], false);
        assert!(json["message"]
            .as_str()
            .unwrap_or("")
            .contains("__io_result__"));
    }

    #[tokio::test]

    async fn test_submit_command_sequence_bad_structure_rejected() {
        // Opt3 实证：指令层 sequence 缺 instructions → 提交期被拒
        let (state, _) = make_test_state();
        let body = r#"{"instruction":{"type":"sequence","params":{}}}"#;
        let (status, json) =
            oneshot_json(make_test_router(&state), "POST", "/api/command", Some(body)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["success"], false);
    }

    #[tokio::test]

    async fn test_submit_command_valid_sequence_accepted() {
        // Opt3 实证：合法 sequence 工作流（demos 010 风格）仍通过（无假阳性）
        let (state, _) = make_test_state();
        let body = r#"{"instruction":{"type":"sequence","params":{"instructions":[{"type":"sampling_decider","params":{}}]}}}"#;
        let (status, json) =
            oneshot_json(make_test_router(&state), "POST", "/api/command", Some(body)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["success"], true);
    }

    #[tokio::test]

    async fn test_get_state_oneshot() {
        let (state, _) = make_test_state();

        let (status, json) =
            oneshot_json(make_test_router(&state), "GET", "/api/state", None).await;

        assert_eq!(status, StatusCode::OK);

        assert!(json["payload"].is_object());

        assert!(json["version"].is_number());
    }

    #[tokio::test]

    async fn test_get_audit_oneshot() {
        let (state, _) = make_test_state();

        let (status, json) =
            oneshot_json(make_test_router(&state), "GET", "/api/audit", None).await;

        assert_eq!(status, StatusCode::OK);

        assert!(json.is_object());
    }

    // --- :平台认证事件报表 ---

    #[tokio::test]
    async fn test_platform_events_empty_oneshot() {
        let (state, _) = make_test_state();

        let (status, json) = oneshot_json(
            make_test_router(&state),
            "GET",
            "/api/audit/platform-events",
            None,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["total"], 0);
        assert!(json["events"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_platform_events_chain_order_and_fields() {
        let (state, _) = make_test_state();
        let shared = SharedFactsLog::from_ref(&state);

        let detail = |username: &str, by: Option<&str>| {
            let mut pairs = vec![("username", JsonValue::string(username))];
            if let Some(by) = by {
                pairs.push(("by", JsonValue::string(by)));
            }
            JsonValue::object_from_pairs(&pairs)
        };
        let event = |kind: &str, detail: JsonValue| {
            JsonValue::object_from_pairs(&[("kind", JsonValue::string(kind)), ("detail", detail)])
        };
        // 三类事件 + 一条非事件平台事实(不进报表)
        shared
            .append(
                "platform.event.login_success.1725000000001aaaa1111bbbb2222",
                event("login_success", detail("alice", None)),
                0,
            )
            .unwrap();
        shared
            .append(
                "platform.event.login_failed.1725000000002cccc3333dddd4444",
                event("login_failed", detail("bob", None)),
                0,
            )
            .unwrap();
        shared
            .append(
                "platform.user.alice",
                JsonValue::object_from_pairs(&[("roles", JsonValue::string("admin"))]),
                0,
            )
            .unwrap();
        shared
            .append(
                "platform.event.user_created.1725000000003eeee5555ffff6666",
                event("user_created", detail("carol", Some("admin"))),
                0,
            )
            .unwrap();

        let (status, json) = oneshot_json(
            make_test_router(&state),
            "GET",
            "/api/audit/platform-events",
            None,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["total"], 3);
        let events = json["events"].as_array().unwrap();
        // 链序:fact_id 升序 = 写入时间序
        let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["login_success", "login_failed", "user_created"]);
        assert_eq!(events[0]["detail"]["username"], "alice");
        assert_eq!(events[1]["detail"]["username"], "bob");
        assert_eq!(events[2]["detail"]["by"], "admin");
        assert_eq!(events[0]["ts_ms"], 1725000000001i64);
        assert!(events[0]["path"]
            .as_str()
            .unwrap()
            .starts_with("platform.event."));
    }

    #[tokio::test]
    async fn test_platform_events_kind_filter_and_limit() {
        let (state, _) = make_test_state();
        let shared = SharedFactsLog::from_ref(&state);

        for (i, kind) in ["login_success", "login_failed", "login_success"]
            .iter()
            .enumerate()
        {
            let path = format!("platform.event.{kind}.172500000001{i}aaaa1111bbbb2222");
            shared
                .append(
                    &path,
                    JsonValue::object_from_pairs(&[
                        ("kind", JsonValue::string(*kind)),
                        (
                            "detail",
                            JsonValue::object_from_pairs(&[("username", JsonValue::string("u"))]),
                        ),
                    ]),
                    0,
                )
                .unwrap();
        }

        // kind 过滤
        let (status, json) = oneshot_json(
            make_test_router(&state),
            "GET",
            "/api/audit/platform-events?kind=login_success",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["total"], 2);

        // limit 截断(链序取前 N;total 仍为过滤后总数)
        let (status, json) = oneshot_json(
            make_test_router(&state),
            "GET",
            "/api/audit/platform-events?limit=2",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["total"], 3);
        assert_eq!(json["events"].as_array().unwrap().len(), 2);
    }

    // --- 规则校验端点（通过路由，含中间件链） ---

    #[tokio::test]

    async fn test_validate_rules_via_router_oneshot() {
        let (state, _) = make_test_state();

        let body = r#"{"rules":"{\"transform\":[{\"type\":\"set\",\"params\":{\"attr\":\"x\",\"operation\":\"set\",\"value\":1}}]}"}"#;

        let (status, json) = oneshot_json(
            make_test_router(&state),
            "POST",
            "/api/rules/validate",
            Some(body),
        )
        .await;

        assert_eq!(status, StatusCode::OK);

        assert_eq!(json["passed"], true);
    }

    // --- 认证中间件 ---

    #[tokio::test]

    async fn test_auth_blocks_unauthorized_oneshot() {
        let (state, _) = make_test_state();

        let auth = AuthConfig::new(vec!["secret_token".to_string()], true);

        let router = GovernanceServer::new(
            state,
            auth,
            "0.0.0.0:0".to_string(),
            0,
            0,
            vec![],
            // S2：测试中 /metrics 无需认证
            false,
            // 测试不挂载 Swagger UI
            false,
            // 测试不启用 abort
            false,
            None,
        )
        .build_router();

        // 受保护路由未带 token → 401

        let (status, _) = oneshot_json(router, "GET", "/api/sessions", None).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]

    async fn test_auth_passes_with_valid_token_oneshot() {
        let (state, _) = make_test_state();

        let auth = AuthConfig::new(vec!["secret_token".to_string()], true);

        let router = GovernanceServer::new(
            state,
            auth,
            "0.0.0.0:0".to_string(),
            0,
            0,
            vec![],
            // S2：测试中 /metrics 无需认证
            false,
            // 测试不挂载 Swagger UI
            false,
            // 测试不启用 abort
            false,
            None,
        )
        .build_router();

        let request = axum::http::Request::builder()
            .method("GET")
            .uri("/api/sessions")
            .header("authorization", "Bearer secret_token")
            .body(axum::body::Body::empty())
            .unwrap();

        let response = router.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]

    async fn test_public_routes_skip_auth_oneshot() {
        let (state, _) = make_test_state();

        let auth = AuthConfig::new(vec!["secret_token".to_string()], true);

        let router = GovernanceServer::new(
            state,
            auth,
            "0.0.0.0:0".to_string(),
            0,
            0,
            vec![],
            // S2：测试中 /metrics 无需认证
            false,
            // 测试不挂载 Swagger UI
            false,
            // 测试不启用 abort
            false,
            None,
        )
        .build_router();

        // /api/health 是公开路由，即使启用认证也无需 token

        let (status, _) = oneshot_json(router, "GET", "/api/health", None).await;

        assert_eq!(status, StatusCode::OK);
    }

    // --- B5-server：受保护域准入（stable.llm / stable.system 仅 service 身份可写） ---

    /// 构造启用认证的测试 Router（user token + service token 双凭据）
    fn make_authed_router(state: &AppState, user_token: &str, service_token: &str) -> Router {
        GovernanceServer::new(
            state.clone(),
            AuthConfig::new(vec![user_token.to_string()], true)
                .with_service_tokens(vec![service_token.to_string()]),
            "0.0.0.0:0".to_string(),
            0,
            0,
            vec![],
            // S2：测试中 /metrics 无需认证
            false,
            // 测试不挂载 Swagger UI
            false,
            // 测试不启用 abort
            false,
            None,
        )
        .build_router()
    }

    /// 发送带 Bearer token 的 oneshot JSON 请求并返回 (状态码, 响应体 JSON)
    async fn oneshot_json_with_token(
        router: Router,
        method: &str,
        uri: &str,
        token: &str,
        body: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let mut builder = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {token}"));

        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }

        let request = if let Some(b) = body {
            builder.body(axum::body::Body::from(b.to_string())).unwrap()
        } else {
            builder.body(axum::body::Body::empty()).unwrap()
        };

        let response = router.oneshot(request).await.unwrap();

        let status = response.status();

        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();

        let json: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);

        (status, json)
    }

    /// user token 写受保护域 `stable.llm` → 403 + 指引文案（fail-visible）
    #[tokio::test]
    async fn test_b5_protected_domain_rejects_user_token_oneshot() {
        let (state, _) = make_test_state();

        let router = make_authed_router(&state, "user_token", "service_token");

        let body = r#"{"path":"shared.default.stable.llm.gpt-4o.summary","value":"forged"}"#;

        let (status, json) =
            oneshot_json_with_token(router, "POST", "/api/payload", "user_token", Some(body)).await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(json["success"], false);
        let msg = json["message"].as_str().unwrap_or("");
        assert!(
            msg.contains("EVORULE_SERVICE_TOKEN"),
            "403 错误信息应含配置指引，实际: {msg}"
        );
    }

    /// service token 写受保护域 → 放行
    #[tokio::test]
    async fn test_b5_protected_domain_allows_service_token_oneshot() {
        let (state, _) = make_test_state();

        let router = make_authed_router(&state, "user_token", "service_token");

        let body = r#"{"path":"shared.default.stable.llm.gpt-4o.summary","value":"extracted"}"#;

        let (status, json) =
            oneshot_json_with_token(router, "POST", "/api/payload", "service_token", Some(body))
                .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["success"], true);
    }

    /// user token 写非受保护 shared 路径 → 放行（权限差异仅受保护域）
    #[tokio::test]
    async fn test_b5_non_protected_shared_path_allows_user_token_oneshot() {
        let (state, _) = make_test_state();

        let router = make_authed_router(&state, "user_token", "service_token");

        let body = r#"{"path":"shared.default.user.notes","value":"ok"}"#;

        let (status, json) =
            oneshot_json_with_token(router, "POST", "/api/payload", "user_token", Some(body)).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["success"], true);
    }

    /// 会话 payload 端点：user token 写受保护域 → 403，service token → 200
    #[tokio::test]
    async fn test_b5_session_payload_protected_domain_oneshot() {
        let (state, _) = make_test_state();

        let router = make_authed_router(&state, "user_token", "service_token");

        // 创建会话（user token 即可）
        let (status, json) =
            oneshot_json_with_token(router.clone(), "POST", "/api/sessions", "user_token", None)
                .await;
        assert_eq!(status, StatusCode::OK);
        let session_id = json["session_id"].as_u64().unwrap();

        // user token 写受保护域 → 403
        let uri = format!("/api/sessions/{session_id}/payload");
        let body = r#"{"path":"shared.default.stable.system.pipeline","value":"forged"}"#;
        let (status, json) =
            oneshot_json_with_token(router.clone(), "POST", &uri, "user_token", Some(body)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(json["success"], false);

        // service token 写受保护域 → 放行
        let (status, json) =
            oneshot_json_with_token(router, "POST", &uri, "service_token", Some(body)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["success"], true);
    }

    /// 认证禁用（loopback 开发模式）时不注入身份 → 受保护域按放行处理（语义不变）
    #[tokio::test]
    async fn test_b5_dev_mode_allows_protected_domain_oneshot() {
        let (state, _) = make_test_state();

        let body = r#"{"path":"shared.default.stable.llm.gpt-4o.summary","value":"dev"}"#;

        let (status, json) =
            oneshot_json(make_test_router(&state), "POST", "/api/payload", Some(body)).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["success"], true);
    }

    // ====================================================================
    // 审计⑥ C14：HTTP 层端到端 —— bundles/import 校验失败即 400 + 发布队列全流程
    // ====================================================================

    /// 同 make_test_state，但 SessionApi 与 PublishService 共享同一 rules_dir。
    ///
    /// 发布队列 e2e 用：审批通过 → bundle 落盘 rules_dir → GET /api/bundles/active
    /// （扫描 {rules_dir}/bundles manifest）即可观测发布链闭环，无需窥探文件系统。
    /// 返回 ws_db 供测试引导 production session（见 test_publish_queue_full_flow_oneshot
    /// 头注：沙盒 fork 依赖已存在的生产会话）。
    fn make_publish_e2e_state(
        rules_dir: std::path::PathBuf,
    ) -> (AppState, Arc<evorule_workspace::WorkspaceDb>) {
        let mut instr = std::collections::BTreeMap::new();
        instr.insert("type".to_string(), JsonValue::string("noop"));
        let core_eval = vec![JsonValue::Object(instr)];
        let reactor = Reactor::builder(core_eval.clone()).max_rounds(100).build();
        let (tx, _rx, _event_tx, _handle, facts_log) = reactor.spawn();
        let auditor = Auditor::new(facts_log.clone());
        let governance = GovernanceApi::new(tx, facts_log, auditor);
        let sessions = SessionApi::new_with_full_config(
            core_eval,
            100,
            None,
            false,
            100 * 1024 * 1024,
            false,
            1000,
            1,
            // TCB 宪法路径：相对仓库根定位（测试 CWD 为 crate 目录，
            // "./resources/server_eval.json" 解析不到，滚动热重载 reload_from_disk 必读该文件）
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../resources/server_eval.json"),
            rules_dir.clone(),
        );
        let metrics: SharedMetrics = shared_prometheus_metrics().unwrap();
        let readiness: ReadinessFlag = Arc::new(AtomicBool::new(true));
        let shared_facts = SharedFactsLog::new();
        let ws_db = Arc::new(evorule_workspace::WorkspaceDb::in_memory().unwrap());
        let session_ops: Arc<dyn evorule_workspace::SessionOps> = Arc::new(sessions.clone());
        let ws_service = Arc::new(evorule_workspace::WorkspaceService::new(
            ws_db.clone(),
            session_ops.clone(),
        ));
        let rule_meta_service = Arc::new(evorule_workspace::RuleMetaService::new(ws_db.clone()));
        let switcher = evorule_workspace::SessionSwitchedBroadcaster::new();
        let sandbox_service = Arc::new(evorule_workspace::SandboxService::new(
            ws_db.clone(),
            session_ops.clone(),
        ));
        let rolling_session = evorule_workspace::RollingSessionService::new(
            ws_db.clone(),
            session_ops,
            switcher.clone(),
        );
        let publish_service = Arc::new(evorule_workspace::PublishService::new(
            ws_db.clone(),
            rolling_session,
            rules_dir,
        ));
        let verdict_service = Arc::new(evorule_workspace::VerdictService::new(ws_db.clone()));
        let workspace_state = evorule_workspace::WorkspaceState::new(
            ws_service,
            rule_meta_service,
            sandbox_service,
            publish_service,
            Arc::new(switcher),
            verdict_service,
        );
        let state = AppState::new(
            governance,
            sessions,
            metrics,
            readiness,
            shared_facts,
            workspace_state,
            Arc::new(InputSanitizer::with_default_rules()),
        );
        (state, ws_db)
    }

    /// :演示登录开关 — 默认开(体验包语义),with_demo_auth(false) 可关闭,
    /// 且 DemoAuthFlag 经 FromRef 提取与 AppState 字段一致(status 端点下发语义)。
    #[tokio::test]
    async fn test_demo_auth_flag_default_on_and_builder_off() {
        let (state, _ws_db) = make_test_state();
        assert!(state.demo_auth(), "默认应开启演示登录入口(体验包默认开)");
        assert!(DemoAuthFlag::from_ref(&state).0);

        let state = state.with_demo_auth(false);
        assert!(!state.demo_auth());
        assert!(!DemoAuthFlag::from_ref(&state).0);
    }

    /// C14：/api/bundles/import 校验失败必须 HTTP 400（显式错误体，不静默）。
    ///
    /// 构造合法 DatasetBundle（content_hash 正确），再篡改条目 rule_body →
    /// 防篡改哈希校验失败 → 400 + {"error":…, "imported":false}。
    #[tokio::test]
    async fn test_bundles_import_tampered_bundle_400_oneshot() {
        use evorule_bundle::{
            BundleAudit, BundleDatasetMeta, BundleEntry, BundleTests, DatasetBundle, Provenance,
            TestVerdict, BUNDLE_SCHEMA_VERSION,
        };

        let (state, _) = make_test_state();

        let mut bundle = DatasetBundle {
            bundle_schema_version: BUNDLE_SCHEMA_VERSION.to_string(),
            bundle_id: "bundle-itest-400".into(),
            dataset: BundleDatasetMeta {
                dataset_id: "ds-itest-400".into(),
                name: "itest-400".into(),
                tenant_id: "org-evorule".into(),
                instance_id: "org-evorule".into(),
                versioning: evorule_bundle::Versioning::default(),
                version_selection: None,
                law_ref: None,
                view_of: None,
                event_schemas: vec![],
            },
            entries: vec![BundleEntry {
                entry_id: "e-400".into(),
                entry_kind: evorule_bundle::EntryKind::Rule,
                rule_body: serde_json::json!({
                    "transform": [
                        {"type": "set", "params": {"attr": "x", "value": 1}}
                    ]
                }),
                schema_ref: None,
                provenance: Provenance {
                    source: "itest".into(),
                    clause: None,
                    document_id: None,
                    effective_from: None,
                    effective_to: None,
                    last_verified: None,
                    verified_by: None,
                },
                domain: "d".into(),
                tags: vec![],
                dependencies: vec![],
            }],
            data_dependencies: None,
            tests: BundleTests {
                // B2: pass 必带可追溯标记(篡改用例在哈希层先拒,此处形状合规)
                subset: vec!["human:itest".into()],
                fixtures: vec![],
                verdict: TestVerdict::Pass,
            },
            audit: BundleAudit {
                exported_at: "2026-08-29T00:00:00Z".into(),
                exported_by: "itest".into(),
                source_version: "v1".into(),
                content_hash: String::new(),
                hash_algo: "blake3".into(),
            },
        };
        bundle.audit.content_hash = bundle.compute_content_hash();
        // 篡改条目内容但不重算哈希 → 防篡改校验失败
        bundle.entries[0].rule_body = serde_json::json!({
            "transform": [
                {"type": "io_request", "params": {"io_type": "call_service", "service_name": "hacked"}}
            ]
        });
        let body = format!(
            r#"{{"bundle":{}}}"#,
            serde_json::to_string(&bundle).unwrap()
        );

        let (status, json) = oneshot_json(
            make_test_router(&state),
            "POST",
            "/api/bundles/import",
            Some(&body),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST, "校验失败必须 400: {json}");
        assert_eq!(json["imported"], false);
        assert!(
            json["error"].as_str().unwrap_or("").contains("校验失败"),
            "错误体应显式: {json}"
        );
    }

    // ====================================================================
    // Q12 W6：数据资产通道端到端 —— knowledge bundle 落盘/加载/直读
    //         + TCB 合并集负向断言（数据文件不进规则执行路径）
    // ====================================================================

    /// rpsm 形态场景领域 schema（测试替身：领域 schema 归领域仓所有，
    /// 此处仅模拟"运维把领域 schema 注入 {knowledge_dir}/domain_schemas/"）。
    /// 注意：用 const 而非 `-> &'static str` 函数——build.rs 门禁的花括号状态机
    /// 不感知生命周期撇号，`'static` 会令其字符状态误吞后续花括号（详见 GATE_REFERENCE）。
    const Q12_SCENARIO_SCHEMA_JSON: &str = r#"{
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://rpsm.evorule.org/schemas/scenario/v1.0.json",
            "type": "object",
            "required": ["scenario_id", "gravity", "bodies"],
            "properties": {
                "scenario_id": {"type": "string"},
                "gravity": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3},
                "restitution": {"type": "number", "minimum": 0, "maximum": 1},
                "bodies": {"type": "array", "items": {"type": "object"}}
            }
        }"#;

    /// 构造 knowledge bundle（rpsm 场景形态；哈希签名完整，可直接过 import_bundle 校验链）
    fn q12_knowledge_bundle(
        bundle_id: &str,
        dataset_id: &str,
        entry_id: &str,
        schema_ref: &str,
    ) -> evorule_bundle::DatasetBundle {
        use evorule_bundle::{
            BundleAudit, BundleDatasetMeta, BundleEntry, BundleTests, DatasetBundle, LawRef,
            Provenance, TestVerdict, VersionSelection, VersionSelectionMode, Versioning,
            BUNDLE_SCHEMA_VERSION,
        };
        let mut bundle = DatasetBundle {
            bundle_schema_version: BUNDLE_SCHEMA_VERSION.to_string(),
            bundle_id: bundle_id.into(),
            dataset: BundleDatasetMeta {
                dataset_id: dataset_id.into(),
                name: "Q12 数据资产演示集".into(),
                tenant_id: "org-evorule".into(),
                instance_id: "org-evorule".into(),
                versioning: Versioning::default(),
                version_selection: Some(VersionSelection {
                    mode: VersionSelectionMode::AutoByEffectiveDate,
                    pinned_version: None,
                    pinned_include_patch: None,
                }),
                law_ref: Some(LawRef {
                    document_id: "rpsm-scenarios".into(),
                    law_version: None,
                    effective_from: Some("2026-08-30".into()),
                    effective_to: None,
                }),
                view_of: None,
                event_schemas: vec![],
            },
            entries: vec![BundleEntry {
                entry_id: entry_id.into(),
                entry_kind: evorule_bundle::EntryKind::Knowledge,
                rule_body: serde_json::json!({
                    "scenario_id": "spring-single-particle",
                    "gravity": [0.0, -9.81, 0.0],
                    "restitution": 1.0,
                    "bodies": [{"id": "particle-1"}]
                }),
                schema_ref: Some(schema_ref.into()),
                provenance: Provenance {
                    source: "rpsm 内置场景".into(),
                    clause: None,
                    document_id: None,
                    effective_from: None,
                    effective_to: None,
                    last_verified: None,
                    verified_by: None,
                },
                domain: "physics".into(),
                tags: vec![],
                dependencies: vec![],
            }],
            data_dependencies: None,
            tests: BundleTests {
                // B2: pass 必带可追溯标记(执行域 import 侧校验);
                // 测试意图=合法可导入知识包,人工背书形态
                subset: vec!["human:q12-itest".into()],
                fixtures: vec![],
                verdict: TestVerdict::Pass,
            },
            audit: BundleAudit {
                exported_at: "2026-08-30T00:00:00Z".into(),
                exported_by: "q12-itest".into(),
                source_version: "v1".into(),
                content_hash: String::new(),
                hash_algo: "blake3".into(),
            },
        };
        bundle.audit.content_hash = bundle.compute_content_hash();
        bundle
    }

    /// Q12 W6-1：knowledge bundle 导入端到端——落盘 knowledge_dir（物理隔离）
    /// → KnowledgeStore 导入后即刻可读（导入刷新 + W3 直读）
    /// → TCB 合并集负向断言（数据文件不出现在规则合并集）。
    #[tokio::test]
    async fn test_knowledge_bundle_import_land_load_direct_read_oneshot() {
        let tmp = tempfile::tempdir().unwrap();
        let rules_dir = tmp.path().join("rules");
        let core_eval_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../resources/server_eval.json");

        // 执行侧领域 schema 注册（运维注入通道：{knowledge_dir}/domain_schemas/）
        let ddir = tmp.path().join("knowledge").join("domain_schemas");
        std::fs::create_dir_all(&ddir).unwrap();
        std::fs::write(ddir.join("scenario.json"), Q12_SCENARIO_SCHEMA_JSON).unwrap();

        let core_eval: Vec<JsonValue> = vec![];
        let sessions = SessionApi::new_with_full_config(
            core_eval.clone(),
            100,
            None,
            false,
            100 * 1024 * 1024,
            false,
            1000,
            1,
            core_eval_path.clone(),
            rules_dir.clone(),
        );
        assert!(
            sessions.knowledge_load_error().is_none(),
            "空 knowledge 目录应得空库而非错误"
        );
        // TCB 合并集基线（导入前）：resources/core_eval.json 自带若干宪法规则
        let baseline = SessionApi::load_merged_transforms_from_fs(&core_eval_path, &rules_dir)
            .expect("TCB 合并加载不应失败");

        let bundle = q12_knowledge_bundle(
            "bundle-q12-demo-v1",
            "ds-q12-demo",
            "scn-001",
            "https://rpsm.evorule.org/schemas/scenario/v1.0.json",
        );
        let result = sessions
            .import_bundle(&bundle, false)
            .await
            .expect("knowledge bundle 导入应通过全部校验门");
        assert_eq!(result.dataset_id, "ds-q12-demo");
        assert_eq!(result.entry_count, 1);

        // W1 落盘物理隔离：数据在 {knowledge_dir}/bundles，rules_dir 不产生任何文件
        let landed = tmp
            .path()
            .join("knowledge")
            .join("bundles")
            .join("bundle-q12-demo-v1");
        assert!(
            landed.join("bundle_manifest.json").is_file(),
            "manifest 应落盘"
        );
        assert!(landed.join("scn-001.json").is_file(), "数据条目应落盘");
        assert!(
            !rules_dir.join("bundles").exists(),
            "数据包不得落入 rules_dir（物理隔离）"
        );

        // TCB 合并集负向断言：数据文件不出现在规则合并集（导入前后一致）
        let merged = SessionApi::load_merged_transforms_from_fs(&core_eval_path, &rules_dir)
            .expect("TCB 合并加载不应被数据资产影响");
        assert_eq!(merged.len(), baseline.len(), "数据条目不得进入 TCB 合并集");

        // W2 导入刷新 + W3 直读：导入后 KnowledgeStore 即刻可读（无需重启）
        let store = sessions.knowledge_store();
        let rec = store
            .get("ds-q12-demo", "scn-001")
            .expect("数据条目应可直读");
        assert_eq!(rec.payload["scenario_id"], "spring-single-particle");
        assert_eq!(
            rec.schema_ref.as_deref(),
            Some("https://rpsm.evorule.org/schemas/scenario/v1.0.json")
        );
        assert_eq!(rec.bundle_id, "bundle-q12-demo-v1");
        assert_eq!(rec.source_version, "v1");
    }

    /// Q12 W6-2：Rule 与 Knowledge 混装同一 bundle → 显式拒绝（载荷语义互斥，不静默）
    #[tokio::test]
    async fn test_knowledge_mixed_bundle_rejected_oneshot() {
        let tmp = tempfile::tempdir().unwrap();
        let rules_dir = tmp.path().join("rules");
        let core_eval_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../resources/server_eval.json");
        let sessions = SessionApi::new_with_full_config(
            vec![],
            100,
            None,
            false,
            100 * 1024 * 1024,
            false,
            1000,
            1,
            core_eval_path,
            rules_dir.clone(),
        );

        let mut bundle = q12_knowledge_bundle(
            "bundle-q12-mixed",
            "ds-q12-mixed",
            "scn-mixed",
            "https://rpsm.evorule.org/schemas/scenario/v1.0.json",
        );
        bundle.entries.push(evorule_bundle::BundleEntry {
            entry_id: "rule-mixed".into(),
            entry_kind: evorule_bundle::EntryKind::Rule,
            rule_body: serde_json::json!({
                "transform": [
                    {"type": "set", "params": {"attr": "x", "value": 1}}
                ]
            }),
            schema_ref: None,
            provenance: bundle.entries[0].provenance.clone(),
            domain: "physics".into(),
            tags: vec![],
            dependencies: vec![],
        });
        bundle.audit.content_hash = bundle.compute_content_hash();

        let err = sessions
            .import_bundle(&bundle, false)
            .await
            .expect_err("混装 bundle 必须显式拒绝");
        assert!(err.contains("混装"), "错误应显式指出混装: {err}");
        assert!(
            !tmp.path().join("knowledge").join("bundles").exists(),
            "拒绝的 bundle 不得落盘"
        );
    }

    // ====================================================================
    // B2: 测试证据引用校验（执行域 import 侧——入执行域的口）
    // ====================================================================

    /// B2-形状: verdict=pass 但 subset 为空(零证据 pass)→ 显式拒绝,
    /// 封死绕过治理域手写伪造直 POST import 的路径。
    #[tokio::test]
    async fn test_uv080_import_rejects_pass_without_traceable_subset() {
        let tmp = tempfile::tempdir().unwrap();
        let rules_dir = tmp.path().join("rules");
        let core_eval_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../resources/server_eval.json");
        // 执行侧领域 schema 注册（与 test_knowledge_bundle_import 同构）
        let ddir = tmp.path().join("knowledge").join("domain_schemas");
        std::fs::create_dir_all(&ddir).unwrap();
        std::fs::write(ddir.join("scenario.json"), Q12_SCENARIO_SCHEMA_JSON).unwrap();
        let sessions = SessionApi::new_with_full_config(
            vec![],
            100,
            None,
            false,
            100 * 1024 * 1024,
            false,
            1000,
            1,
            core_eval_path,
            rules_dir.clone(),
        );

        let mut bundle = q12_knowledge_bundle(
            "bundle-uv080-shape",
            "ds-uv080-shape",
            "scn-uv080",
            "https://rpsm.evorule.org/schemas/scenario/v1.0.json",
        );
        // 零证据 pass: 空 subset
        bundle.tests.subset = vec![];
        bundle.audit.content_hash = bundle.compute_content_hash();

        let err = sessions
            .import_bundle(&bundle, false)
            .await
            .expect_err("零证据 pass 必须显式拒绝");
        assert!(
            err.contains("必须携带可追溯标记"),
            "错误应指向可追溯标记缺失: {err}"
        );
        assert!(
            !tmp.path().join("knowledge").join("bundles").exists(),
            "拒绝的 bundle 不得落盘"
        );
    }

    /// B2-引用: sandbox:<id> 引用本机不存在的沙盒(伪造/跨环境)→ 显式拒绝。
    /// resolver 环境与 test_knowledge_import_refresh 同构(schema URI 命中),
    /// 另接线 in-memory workspace_db(沙盒表为空)。
    #[tokio::test]
    async fn test_uv080_import_rejects_phantom_sandbox_reference() {
        let tmp = tempfile::tempdir().unwrap();
        let rules_dir = tmp.path().join("rules");
        let core_eval_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../resources/server_eval.json");
        // 执行侧领域 schema 注册（与 test_knowledge_bundle_import 同构）
        let ddir = tmp.path().join("knowledge").join("domain_schemas");
        std::fs::create_dir_all(&ddir).unwrap();
        std::fs::write(ddir.join("scenario.json"), Q12_SCENARIO_SCHEMA_JSON).unwrap();
        let sessions = SessionApi::new_with_full_config(
            vec![],
            100,
            None,
            false,
            100 * 1024 * 1024,
            false,
            1000,
            1,
            core_eval_path,
            rules_dir.clone(),
        )
        .with_workspace_db(Arc::new(
            evorule_workspace::WorkspaceDb::in_memory().unwrap(),
        ));

        let mut bundle = q12_knowledge_bundle(
            "bundle-uv080-ref",
            "ds-uv080-ref",
            "scn-uv080-ref",
            "https://rpsm.evorule.org/schemas/scenario/v1.0.json",
        );
        // 引用不存在的沙盒 999
        bundle.tests.subset = vec!["sandbox:999".into()];
        bundle.audit.content_hash = bundle.compute_content_hash();

        let err = sessions
            .import_bundle(&bundle, false)
            .await
            .expect_err("幻影沙盒引用必须显式拒绝");
        assert!(
            err.contains("在本机不存在"),
            "错误应指向沙盒引用不存在: {err}"
        );
        assert!(
            !tmp.path().join("knowledge").join("bundles").exists(),
            "拒绝的 bundle 不得落盘"
        );
    }

    /// B2-正路径: human:<actor> 显式人工背书 → 放行(无需存在性校验,
    /// 标记即显式降级声明)。
    #[tokio::test]
    async fn test_uv080_import_allows_human_endorsement() {
        let tmp = tempfile::tempdir().unwrap();
        let rules_dir = tmp.path().join("rules");
        let core_eval_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../resources/server_eval.json");
        // 执行侧领域 schema 注册（与 test_knowledge_bundle_import 同构）
        let ddir = tmp.path().join("knowledge").join("domain_schemas");
        std::fs::create_dir_all(&ddir).unwrap();
        std::fs::write(ddir.join("scenario.json"), Q12_SCENARIO_SCHEMA_JSON).unwrap();
        let sessions = SessionApi::new_with_full_config(
            vec![],
            100,
            None,
            false,
            100 * 1024 * 1024,
            false,
            1000,
            1,
            core_eval_path,
            rules_dir.clone(),
        );

        // q12_knowledge_bundle 的 subset 已是 human 背书形态
        let bundle = q12_knowledge_bundle(
            "bundle-uv080-human",
            "ds-uv080-human",
            "scn-uv080-human",
            "https://rpsm.evorule.org/schemas/scenario/v1.0.json",
        );
        let result = sessions
            .import_bundle(&bundle, false)
            .await
            .expect("human 背书应放行(显式降级,无需存在性校验)");
        assert_eq!(result.dataset_id, "ds-uv080-human");
        assert_eq!(result.entry_count, 1);
    }

    /// B2-真实沙盒引用: closed 沙盒 + PASS 报告(failed=0)→ 放行;
    /// FAIL 报告(failed>0)→ 拒收(fail 报告不得作 pass 证据)。
    /// 在 in-memory db 造真实沙盒记录 + 磁盘报告文件(与 close_sandbox 落盘
    /// 同构:report_<facts basename>.json 于 SANDBOX_REPORT_DIR)。
    #[tokio::test]
    async fn test_uv080_import_sandbox_reference_with_report_consistency() {
        let tmp = tempfile::tempdir().unwrap();
        let rules_dir = tmp.path().join("rules");
        let core_eval_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../resources/server_eval.json");
        // 执行侧领域 schema 注册（与 test_knowledge_bundle_import 同构）
        let ddir = tmp.path().join("knowledge").join("domain_schemas");
        std::fs::create_dir_all(&ddir).unwrap();
        std::fs::write(ddir.join("scenario.json"), Q12_SCENARIO_SCHEMA_JSON).unwrap();
        let ws_db = Arc::new(evorule_workspace::WorkspaceDb::in_memory().unwrap());
        let sessions = SessionApi::new_with_full_config(
            vec![],
            100,
            None,
            false,
            100 * 1024 * 1024,
            false,
            1000,
            1,
            core_eval_path,
            rules_dir.clone(),
        )
        .with_workspace_db(ws_db.clone());

        // 先建 workspace 行(沙盒记录外键依赖) + 两个 closed 沙盒记录: PASS / FAIL
        // DateTime<Utc> 从 production_state 行取(chrono 非 evorule-server 直接依赖,
        // 不为此新增依赖)
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let ws_id = format!("ws-uv080-{ts}");
        {
            let now_dt = ws_db.get_production_state().unwrap().updated_at;
            let ws = evorule_workspace::models::WorkspaceRecord {
                id: ws_id.clone(),
                name: "ws-uv080".into(),
                owner_id: "test:uv080".into(),
                created_at: now_dt,
                updated_at: now_dt,
                archived_at: None,
                state: evorule_workspace::models::WorkspaceState::Active,
                description: None,
            };
            ws_db.insert_workspace(&ws).unwrap();
        }
        let report_dir = evorule_workspace::SANDBOX_REPORT_DIR;
        std::fs::create_dir_all(report_dir).unwrap();
        let facts_1 = format!("audit_sandbox_1_{ts}.json");
        let facts_2 = format!("audit_sandbox_2_{ts}.json");
        let sb_pass = ws_db
            .insert_sandbox_session(None, &ws_id, 1, Some("hash-uv080"), 1, "test:uv080")
            .unwrap();
        let sb_fail = ws_db
            .insert_sandbox_session(None, &ws_id, 1, Some("hash-uv080"), 1, "test:uv080")
            .unwrap();
        // insert 自增从 1 起;以实际返回 id 为准写报告与引用
        let export_1 = format!("{report_dir}/{facts_1}");
        let export_2 = format!("{report_dir}/{facts_2}");
        ws_db.close_sandbox_session(sb_pass, &export_1).unwrap();
        ws_db.close_sandbox_session(sb_fail, &export_2).unwrap();
        // 报告文件(与 generate_test_report 落盘同构:report_<facts basename>)
        std::fs::write(
            format!("{report_dir}/report_{facts_1}"),
            r#"{"summary": {"total_cases": 9, "passed": 9, "failed": 0, "skipped": 0}}"#,
        )
        .unwrap();
        std::fs::write(
            format!("{report_dir}/report_{facts_2}"),
            r#"{"summary": {"total_cases": 9, "passed": 7, "failed": 2, "skipped": 0}}"#,
        )
        .unwrap();

        // PASS 沙盒引用 → 放行
        let mut bundle = q12_knowledge_bundle(
            "bundle-uv080-sb-pass",
            "ds-uv080-sb-pass",
            "scn-uv080-sb-pass",
            "https://rpsm.evorule.org/schemas/scenario/v1.0.json",
        );
        bundle.tests.subset = vec![format!("sandbox:{sb_pass}")];
        bundle.audit.content_hash = bundle.compute_content_hash();
        let result = sessions
            .import_bundle(&bundle, false)
            .await
            .expect("closed 沙盒 + PASS 报告引用应放行");
        assert_eq!(result.dataset_id, "ds-uv080-sb-pass");
        assert_eq!(result.entry_count, 1);

        // FAIL 沙盒引用 → 拒收(报告一致性)
        let mut bundle = q12_knowledge_bundle(
            "bundle-uv080-sb-fail",
            "ds-uv080-sb-fail",
            "scn-uv080-sb-fail",
            "https://rpsm.evorule.org/schemas/scenario/v1.0.json",
        );
        bundle.tests.subset = vec![format!("sandbox:{sb_fail}")];
        bundle.audit.content_hash = bundle.compute_content_hash();
        let err = sessions
            .import_bundle(&bundle, false)
            .await
            .expect_err("FAIL 报告不得作为 pass 证据");
        assert!(err.contains("失败用例"), "错误应指向报告失败用例: {err}");

        // 清理测试报告文件(写于 crate 相对路径 ./data/sandbox_reports,仓库忽略区)
        let _ = std::fs::remove_file(format!("{report_dir}/report_{facts_1}"));
        let _ = std::fs::remove_file(format!("{report_dir}/report_{facts_2}"));
    }

    /// Q12 W6-3：schema_ref 领域 schema 未注册（resolver 未命中）→ 拒绝入库
    /// （D3 fail-fast：无领域 schema 的 payload 不得入库，不静默放行）
    #[tokio::test]
    async fn test_knowledge_import_resolver_miss_rejected_oneshot() {
        let tmp = tempfile::tempdir().unwrap();
        let rules_dir = tmp.path().join("rules");
        let core_eval_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../resources/server_eval.json");
        let sessions = SessionApi::new_with_full_config(
            vec![],
            100,
            None,
            false,
            100 * 1024 * 1024,
            false,
            1000,
            1,
            core_eval_path,
            rules_dir.clone(),
        );

        // 不注入领域 schema → resolver 未命中
        let bundle = q12_knowledge_bundle(
            "bundle-q12-unknown-schema",
            "ds-q12-unknown",
            "scn-unknown",
            "https://rpsm.evorule.org/schemas/scenario/v1.0.json",
        );
        let err = sessions
            .import_bundle(&bundle, false)
            .await
            .expect_err("resolver 未命中必须拒绝");
        assert!(
            err.contains("未在解析器注册"),
            "错误应指向领域 schema 未注册: {err}"
        );
        assert!(
            !tmp.path().join("knowledge").join("bundles").exists(),
            "拒绝的 bundle 不得落盘"
        );
    }

    /// C14：发布队列 HTTP 层全流程端到端。
    ///
    /// 前置引导：创建一个生产会话并写入 production_state（模拟"已发布过 v1"
    /// 的部署——沙盒 fork 依赖已存在的生产会话；全新部署的首发布引导路径
    /// 属产品缺口，另立台账观察，不在本测试范围内伪造）。
    ///
    /// 流程：建工作区 → 建规则 → Draft→Candidate → 建测试集 → 沙盒测试 →
    /// 关闭（闸门一证据）→ 科室主任提交发布 → Admin 审批通过 → bundle 落盘
    /// rules_dir → GET /api/bundles/active 可观测（发布链闭环）。
    #[tokio::test]
    async fn test_publish_queue_full_flow_oneshot() {
        let _ = tracing_subscriber::fmt::try_init();
        let tmp = tempfile::tempdir().unwrap();
        let rules_dir = tmp.path().join("rules");
        let (state, ws_db) = make_publish_e2e_state(rules_dir);

        // 0. 引导生产会话
        let (status, json) =
            oneshot_json(make_test_router(&state), "POST", "/api/sessions", None).await;
        assert_eq!(status, StatusCode::OK);
        let prod_sid = json["session_id"].as_u64().unwrap();
        ws_db
            .update_production_state(prod_sid as i64, 0, "init_hash", "system")
            .unwrap();

        // 1. 建工作区
        let (status, json) = oneshot_json(
            make_test_router(&state),
            "POST",
            "/api/workspaces",
            Some(r#"{"name":"e2e-ws","owner_id":"head-1","description":null}"#),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "建工作区失败: {json}");
        let ws_id = json["id"].as_str().unwrap().to_string();

        // 2. 建规则（合法 set 元指令，过发布链 Schema 门禁）
        let (status, json) = oneshot_json(
            make_test_router(&state),
            "POST",
            &format!("/api/workspaces/{ws_id}/rules"),
            Some(
                r#"{"name":"rule-e2e","content":"{\"transform\":[{\"type\":\"set\",\"params\":{\"attr\":\"payload.result\",\"operation\":\"set\",\"value\":\"ok\"}}]}","created_by":"head-1","description":null}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "建规则失败: {json}");
        let rule_id = json["id"].as_str().unwrap().to_string();

        // 3. Draft → Candidate
        let (status, _) = oneshot_json(
            make_test_router(&state),
            "POST",
            &format!("/api/workspaces/{ws_id}/rules/{rule_id}/submit"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // 4. 取当前版本 ID
        let (status, json) = oneshot_json(
            make_test_router(&state),
            "GET",
            &format!("/api/workspaces/{ws_id}/rules/{rule_id}/versions"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let versions = if json.is_array() {
            json.clone()
        } else {
            json["versions"].clone()
        };
        let rv_id = versions
            .as_array()
            .and_then(|a| a.first())
            .and_then(|v| v["id"].as_str())
            .unwrap_or_else(|| panic!("版本列表异常: {json}"))
            .to_string();

        // 5. 建测试数据集
        let (status, json) = oneshot_json(
            make_test_router(&state),
            "POST",
            &format!("/api/workspaces/{ws_id}/test-datasets"),
            Some(r#"{"name":"ds-e2e","cases_json":"[]","created_by":"head-1"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "建测试集失败: {json}");
        let ds_id = json["id"].as_i64().expect("测试集 id 缺失");

        // 6. 沙盒测试 → 关闭（闸门一证据）
        let body = format!(
            r#"{{"rule_version_ids":["{rv_id}"],"test_dataset_id":{ds_id},"started_by":"head-1"}}"#
        );
        let (status, json) = oneshot_json(
            make_test_router(&state),
            "POST",
            &format!("/api/workspaces/{ws_id}/sandboxes"),
            Some(&body),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "启动沙盒失败: {json}");
        let sandbox_id = json["sandbox_id"].as_i64().expect("沙盒 id 缺失");

        let (status, _) = oneshot_json(
            make_test_router(&state),
            "POST",
            &format!("/api/workspaces/{ws_id}/sandboxes/{sandbox_id}/close"),
            Some(r#"{"closed_by":"head-1"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "关闭沙盒失败");

        // 7. 科室主任提交发布
        let body = format!(
            r#"{{"workspace_id":"{ws_id}","rule_version_ids":["{rv_id}"],"test_report_sandbox_id":{sandbox_id},"submitted_by":"head-1","role":"department_head"}}"#
        );
        let (status, json) = oneshot_json(
            make_test_router(&state),
            "POST",
            "/api/publish/queue",
            Some(&body),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "提交发布失败: {json}");
        let queue_id = json["id"].as_i64().expect("队列项 id 缺失");

        // 8. Admin 审批通过 → 触发校验/落盘/热重载
        let (status, json) = oneshot_json(
            make_test_router(&state),
            "POST",
            &format!("/api/publish/queue/{queue_id}/review"),
            Some(
                r#"{"decision":"approved","comment":"e2e","reviewed_by":"admin-1","role":"admin"}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "审批失败: {json}");
        assert_eq!(
            json["status"], "published",
            "审批通过应进入 published: {json}"
        );

        // 9. 发布链闭环可观测：bundle 已落盘 rules_dir 并被 active 列表扫描到
        let (status, json) =
            oneshot_json(make_test_router(&state), "GET", "/api/bundles/active", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            json["count"].as_u64(),
            Some(1),
            "发布落盘的 bundle 应出现在 active 列表: {json}"
        );
        let ds = json["bundles"][0].clone();
        assert!(
            ds["dataset_id"]
                .as_str()
                .map(|s| !s.is_empty())
                .unwrap_or(false),
            "active bundle 的 dataset_id 应非空: {json}"
        );
        assert!(
            ds["content_hash"]
                .as_str()
                .map(|s| s.starts_with("blake3:"))
                .unwrap_or(false),
            "active bundle 的 content_hash 应为 blake3: 前缀: {json}"
        );
    }
}

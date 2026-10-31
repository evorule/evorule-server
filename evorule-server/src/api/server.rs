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

//! - `POST /api/sessions/{id}/payloads` — 批量更新会话 payload（一次 HTTP 多条写入）

//! - `GET /api/state` — 获取当前状态快照

//! - `GET /api/audit` — 获取审计报告

//! - `POST /api/reload` — 手动触发配置热重载

//! - `GET /api/health` — 健康检查

use crate::api::audit_archive;
use crate::auth::{requires_service_identity, AuthConfig, CallerIdentity, ServiceGateArmed};
use crate::input_sanitizer::InputSanitizer;
use axum::http::Method;
use axum::Extension;

use evorule_demo_services::DemoServiceRouter;
use evorule_io_handlers::ServiceMeta;

use evorule_governance_host::Auditor;

use evorule_governance::metrics::SharedMetrics;

use evorule_governance_host::session;

use evorule_governance::shared_facts_log::SharedFactsLog;

use evorule_governance::{permission::PermissionGate, IoDispatcher, IoSubscriber};

use evorule_reactor::{Fact, FactId, FactSender, FactsLog, IoType, TraceHit};

use evorule_tcb::JsonValue;

// 唯一装载通道（宪法收口）：生产/沙盒规则集装载一律经 core/rule_loader
use evorule_rule_loader::{serde_to_tcb, LoadContext, LoadSource, MergedPart};

use evorule_workspace::api::WorkspaceState;

use serde::{Deserialize, Serialize};

use utoipa::ToSchema;

use std::sync::atomic::{AtomicBool, AtomicUsize};

use std::collections::{BTreeMap, BTreeSet, HashSet};

use std::sync::Arc;

use tokio::sync::Mutex;

/// 影子强制违规文件计数（纪律门禁影子强制整改，2026-09-24）。
///
/// L2 纪律门禁 warn（缺省）期命中违规的**文件数**（按文件去重前先简单累加
/// 各接线点，宪法与业务文件各计 1）。语义 =「若 enforce 将拒载的文件数」：
/// 计数 > 0 即影子期不干净；装载完成后 main 以 ERROR 级汇总曝光，作为
/// 分面灰度 flip（影子期违规归零）的观测依据。enforce 期不累计（违规
/// 本身已拒载/拒启，行动面生效）。
static SHADOW_VIOLATION_FILES: AtomicUsize = AtomicUsize::new(0);

/// LLM 审计形态判定（K 约束族，2026-08-30）
///
/// `call_external` 且带 `messages`（LLM 消息历史）且无 `service_name`/`name`
/// —— 此类 IoRequest **不由 server 内置 IoSubscriber 自动应答**，
/// 留给外部执行者（evo-agent AuditedLlm / console-cloud 浏览器审计桥）：
/// 它们经 sidecar 会话把 prompt 全文入审计链、本地执行 LLM 后回写 io_response。
/// 若内置订阅者抢先错误应答，外部执行者的 io_response 会被反应器按
/// Unknown IoResponse 忽略，审计回路永远失败。
///
/// **契约固化**：本函数已改为表驱动委托——形状真相源=
/// [`crate::api::io_contract::IO_CONTRACT_V1_SHAPES`]（`llm_audit` 条目），
/// 契约导出与谓词判定同表同源；行为与历史实现逐一等价（等价锁定测试见
/// io_contract.rs tests：test_llm_audit_shape_equivalence）。
pub fn is_llm_audit_request(io_type: &IoType, params: &JsonValue) -> bool {
    crate::api::io_contract::shape_matches("llm_audit", io_type, params)
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
    crate::api::io_contract::shape_matches("agent_tool", io_type, params)
}

/// bundle 自有探针形态判定（探针形态改造版）。
///
/// `flow_probe` 为 bundle 层自有 io_type（平台 io_type 发射权部署面独占，bundle
/// 规则的 io 往返正道），无内置 handler——此类 IoRequest **不由内置 IoSubscriber
/// 自动应答**，留给外部执行者（流程固化探针等）经 `POST /api/sessions/{id}/io_response`
/// 应答。若不跳过，IoRequest 会进入分发路径：先被 PermissionGate 以 unknown 调用者
/// 拒绝（fail-closed），即使无 gate 也会被 dispatcher 以 no-handler 错误应答——
/// 请求即被消费，外部执行者随后的真实应答被按 stale 拒绝，探针往返断链。
/// 与 `is_llm_audit_request` / `is_agent_tool_request` 完全同构。
///
/// 跳过语义：请求保持 pending（审计链留 IoRequest 事实，透明可查），直至外部
/// 应答到达或 io 超时预算兜底——不存在静默吞没。
pub fn is_flow_probe_request(io_type: &IoType, _params: &JsonValue) -> bool {
    crate::api::io_contract::shape_matches("flow_probe", io_type, _params)
}

/// 内置 IoSubscriber 的合并 skip 谓词：任一外部执行者形态命中即跳过自动应答
pub fn is_external_executor_request(io_type: &IoType, params: &JsonValue) -> bool {
    is_llm_audit_request(io_type, params)
        || is_agent_tool_request(io_type, params)
        || is_flow_probe_request(io_type, params)
}

/// L2 约束层文件名判定（单一权威，tier_inventory / l2_inventory / rule_file_gate 三处共用）。
///
/// 新权威前缀 `00_constraint_`（命名收敛 v3.0）；旧前缀 `00_meta_` 保持
/// 兼容读取（存量零迁移）。仅 rules_dir **根目录直置**文件可据此判定为 L2
/// （目录归属由调用方校验）。
fn is_l2_meta_file_name(name: &str) -> bool {
    name.starts_with("00_constraint_") || name.starts_with("00_meta_")
}

/// rules_dir 装载排序键（注入序治理，全序确定）
///
/// 字段序 = 排序优先级：
/// ① `promoted_at_desc` —— 晋升约束（`00_constraint_promoted_` 前缀 **且**
/// `metadata.promoted_at` 可解析为 RFC3339）按晋升时间**降序**排前：最新晋升的
/// 约束最先求值，enforce 命中即终止执行链 → 归因变更为新约束、种子类既有
/// enforce 不再重复拦截。非晋升文件该字段为 `None`（Reverse 序下恒排有值项之后）。
/// ② `path` —— 平级兜底键：种子/业务/bundles 文件之间、以及晋升文件 promoted_at
/// 平级时，按完整路径字典序收敛（全序保证；「文件名排序废弃」指不再作为主排序键，
/// 兜底确定性仍依赖字典序）。存量 promoted 产物缺失该字段时自然落入兜底类。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RulesDirLoadOrderKey {
    promoted_at_desc: std::cmp::Reverse<Option<chrono::DateTime<chrono::FixedOffset>>>,
    path: std::path::PathBuf,
}

/// 读取单个规则文件的装载排序键（见 [`RulesDirLoadOrderKey`]）
///
/// 读取/解析失败不报错（fail-soft，与目录扫描同策略；排序退化不影响该文件
/// 参与装载，后续 parse_rule_file 仍会独立校验）。peek 与 parse_rule_file
/// 各自独立读取同一文件（reload 低频 + 规则文件小，双读成本可忽略）。
fn rules_dir_load_order_key(p: &std::path::Path) -> RulesDirLoadOrderKey {
    let is_promoted_name = p
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("00_constraint_promoted_"));
    let promoted_at = if is_promoted_name {
        let parsed = std::fs::read_to_string(p)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| {
                v.pointer("/metadata/promoted_at")
                    .and_then(|x| x.as_str())
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            });
        if parsed.is_none() {
            tracing::warn!(
                "晋升约束文件 {} 缺失/非法 metadata.promoted_at，装载序退化为路径字典序兜底",
                p.display()
            );
        }
        parsed
    } else {
        None
    };
    RulesDirLoadOrderKey {
        promoted_at_desc: std::cmp::Reverse(promoted_at),
        path: p.to_path_buf(),
    }
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

    /// Command id 计数器抬升（单反应器 WAL 续链用）
    ///
    /// 计数器为进程内存态（缺省自 20000 起），重启续链后归档内已有历史
    /// Command id——不抬升则新 Command 与历史 Command 在 fact_id_index 内
    /// 撞号（or_insert 静默吞并，因果链查询污染）。`exclusive_max` 传归档
    /// 最大 Command id（无则 0）；fetch_max 语义保证低于现值不回退。
    pub fn ensure_command_ids_above(&self, exclusive_max: u64) {
        self.next_id.fetch_max(
            exclusive_max.saturating_add(1),
            std::sync::atomic::Ordering::SeqCst,
        );
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

    /// 已封审计锚点快照（G-A2）
    pub async fn audit_anchors(&self) -> Vec<evorule_governance::anchor::AuditAnchor> {
        let auditor = self.auditor.lock().await;
        auditor.anchors().to_vec()
    }

    /// 立即封一个审计锚点（G-A2；未启用锚点链时 Err）
    pub async fn seal_anchor_now(
        &self,
    ) -> Result<evorule_governance::anchor::AuditAnchor, evorule_governance::anchor::AnchorError>
    {
        let mut auditor = self.auditor.lock().await;
        auditor.seal_anchor_now()
    }

    /// 验证锚点链+事实链绑定（G-A2；keys: key_id → 验签公钥 hex）
    pub async fn verify_anchors(&self, keys: BTreeMap<String, String>) -> Result<(), String> {
        let auditor = self.auditor.lock().await;
        auditor.verify_anchors(&keys)
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
    /// 会话级 pause 准入闸（干预端点置位，resume 解除；内存态，重启清零）
    paused_sessions: Arc<std::sync::Mutex<HashSet<u64>>>,

    /// 心跳限频账（session_id → 上次心跳时刻；内存态，重启清零）
    heartbeat_last: Arc<std::sync::Mutex<std::collections::HashMap<u64, std::time::Instant>>>,

    /// 会话管理器
    sessions: Arc<Mutex<session::SessionManager>>,

    /// WAL 目录（fork-from-archive 读归档链的来源；None = 纯内存模式，archive fork 404）
    /// 由 `fork_from_archive` 消费：两个 fork 端点在父会话不在内存时，
    /// 经此读 `session_{parent_id}.wal` 归档链重建初始状态建新会话。
    wal_dir: Option<std::path::PathBuf>,

    /// API 层 FactId 计数器（从 30000 起，避免与反应器自身 ID 冲突）
    ///
    /// ID 空间语义：本计数器是 **API 层全局
    /// 命令面**——Command 的 ID 从 30000 高位区起分配，跨会话单调递增；与会话内
    /// 反应器自身 FactId（低位区，从 1 起）、共享账本 fact_id（独立空间）分属
    /// 三个不同 ID 空间，响应中的 `fact_id` 字段按来源分别取自其一（payload
    /// 观测值如 30331 即本高位区）。跨重启续起语义：单反应器路径经
    /// `ensure_command_ids_above(归档最大 Command id)` 抬升；会话复活路径经
    /// `session_or_rehydrate` 的 `fetch_max(max_api_id + 1)` 按归档续起——
    /// 高位区与低位区各自按类续起、两区不相交，防新事实与恢复历史撞号。
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
    /// 仅用于管理元数据旁路（bundle_imports 表），不参与 fact / 哈希 / 审计验证链。
    /// 未接线（None）时不记录 bundle 导入溯源（如单元测试环境）。
    workspace_db: Option<Arc<evorule_workspace::WorkspaceDb>>,

    /// 跨会话共享事实存储（C6 入口守卫判定源，D8 版本域）
    ///
    /// 默认内存新建（无历史条目 → 判定走默认策略）；main.rs 启动期经
    /// `with_shared_facts` 注入 WAL 恢复实例，供全部 per-session
    /// `IoSubscriber` 的 `PermissionGate` 判定使用（与全局分发路径同一实例）。
    shared_facts: SharedFactsLog,

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
    /// 运行时可热更（安装自动化三期）：install/uninstall 运行时合入/移除，
    /// 同 core_eval/knowledge_store 的 RwLock<Arc<>> 热更骨架
    registry_services: Arc<std::sync::RwLock<Arc<Vec<ServiceMeta>>>>,

    /// 审计档案：wal_dir 下历史会话 WAL 的只读重建缓存。
    /// 与活跃会话 API 物理隔离（独立 /api/audit-archive 前缀），全程无 WAL 写路径。
    archive_cache: Arc<std::sync::Mutex<audit_archive::ArchiveCache>>,

    /// external 插件管理面端点表（插件审批代理转发目标,键=插件 id）
    ///
    /// main.rs 启动期从 external 挂载结果派生（base_url）+ env 读取 admin token
    /// （EVORULE_PLUGIN_ADMIN_TOKEN__<ID 大写下划线>;未配置 → admin_token=None,
    /// 代理端点 503 fail-fast,与插件管理面自身语义一致）。默认空表=无 external
    /// 插件,代理端点一律 404。
    plugin_admins: Arc<std::collections::BTreeMap<String, PluginAdminEndpoint>>,

    /// 插件契约 v1：声明式 pack 资产注册表（02-Plugin-Contract-v1.md §4）
    ///
    /// main.rs 启动期经 `load_plugin_packs` 读盘装载（fail-fast），运行期只读
    /// （契约 §4.1.3 无热加载）。资产面三 API 的数据来源；默认空注册表。
    plugin_packs: crate::api::plugin_packs::PluginPackRegistry,

    /// 规则命中统计聚合器：消费各会话/单反应器的 TransitionTrace，
    /// 按 规则集版本×来源×下标 聚合；查询面 /api/rules/hit-stats 与 Prometheus 指标。
    hit_stats: Arc<crate::api::hit_stats::HitStatsAggregator>,

    /// 会话级 caller_role 声明表
    ///
    /// 会话创建时若声明 caller_role（"human"/"llm"），在此登记（内存辅助态）：
    /// 命令入口据此对声明会话提交的每条 Command 注入 instruction 根部
    /// `__meta__.caller_role`（随 WAL 落账 = 判定权威源；本表仅承载「注入决策」，
    /// 丢失不破坏判定与回放确定性——resolver 以 WAL 事实为权威，见专项设计档 §四）。
    /// 注入发生在 Schema 门禁之后：transform_rule 条目级键白名单针对用户/LLM
    /// 产物，服务端系统注入不属其对象。
    caller_roles: Arc<std::sync::Mutex<std::collections::HashMap<u64, String>>>,

    /// 惰性同 sid 复活开关（缺省开）
    ///
    /// 开启时，续聊/查态入口对不在内存表的会话先尝试从 WAL 归档按原 sid 复活
    /// （`session_or_rehydrate`）；关闭则逐位回到「不在表即 404」的既有语义。
    rehydrate_enabled: bool,

    /// Prometheus 指标句柄（复活路径 spawn IoSubscriber/inc_sessions 用；
    /// None 时以 noop 兜底——桥接路径与测试构造无 State 注入）
    metrics: Option<SharedMetrics>,
}

/// external 插件管理面端点(审批代理转发目标 + server 侧持有的 admin token)
///
/// token 由部署侧 env 同时配给插件进程与 server 两侧(密钥零落盘);
/// server 不落盘不打印(启动期仅 info! 呈现配置与否,不呈现值)。
#[derive(Debug, Clone)]
pub struct PluginAdminEndpoint {
    /// 插件进程根地址(管理面 = {base_url}/admin/...)
    pub base_url: String,
    /// server 侧持有的该插件 admin token(None=未配置,代理 503 fail-fast)
    pub admin_token: Option<String>,
}

/// 插件 id → server 侧 admin token 环境变量名(纯函数,单测锁定映射)
///
/// 约定 `EVORULE_PLUGIN_ADMIN_TOKEN__<ID 大写,非字母数字折叠为下划线>`:
/// "finance-config" → "EVORULE_PLUGIN_ADMIN_TOKEN__FINANCE_CONFIG"。
pub fn plugin_admin_token_env(id: &str) -> String {
    let folded: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("EVORULE_PLUGIN_ADMIN_TOKEN__{folded}")
}

/// 批次A（P1-10 折叠撞名）: 两个不同插件 id 是否折叠映射到同一
/// admin token env 名。折叠把非 ASCII 字母数字统一映射为 '_',不同 id 可撞
/// 同一 env 名（如 "finance-config" 与 "finance_config"）——部署侧将无法
/// 分别配置 admin token（一个值同时授权两个插件的审批代理,归因不可分）。
/// 装载期 fail-fast 拒绝装载（调用方 load_external_plugins）。
pub fn plugin_admin_token_env_collides(a: &str, b: &str) -> bool {
    a != b && plugin_admin_token_env(a) == plugin_admin_token_env(b)
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
            // 便捷构造保持引擎缺省（30s/60s），缺省常量随动防漂移
            session::DEFAULT_IO_WARN_TIMEOUT.as_secs(),
            session::DEFAULT_IO_ERROR_TIMEOUT.as_secs(),
            // 0=引擎缺省 TTL（便捷构造不改行为）
            0,
            // 锚点链缺省关（便捷构造不改行为；部署面经 main 注入）
            None,
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
    /// - `io_warn_timeout_secs`：pending I/O 超时警告阈值（秒，贯通至每个会话反应器）
    ///
    /// - `io_error_timeout_secs`：pending I/O 超时错误阈值（秒，贯通至每个会话反应器）
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

        io_warn_timeout_secs: u64,

        io_error_timeout_secs: u64,

        session_ttl_secs: u64,

        anchor_config: Option<session::SessionAnchorConfig>,
    ) -> Self {
        let ce_cloned = core_eval.clone();

        // 初始 layout（来源标签暂全记 "core_eval"，生产路径由 main.rs
        // 经 `with_hit_stats` 注入含 rules_dir 文件级溯源的权威 layout；reload 后自动换版）
        let initial_layout = crate::api::hit_stats::RulesetLayout::from_rules(
            &ce_cloned,
            vec!["core_eval".to_string(); ce_cloned.len()],
        );

        let mut session_mgr = session::SessionManager::with_limits_and_wal_and_auto_verify(
            core_eval,
            max_rounds,
            session::DEFAULT_MAX_SESSIONS,
            // TTL 配置面——main 三层解析注入；缺省
            // DEFAULT_SESSION_TTL(1800s) 行为不变。启动断言在 main.rs
            // （TTL ≥ io_error_timeout+余量,warn/enforce 语义同 step budget 断言）。
            if session_ttl_secs == 0 {
                session::DEFAULT_SESSION_TTL
            } else {
                std::time::Duration::from_secs(session_ttl_secs)
            },
            wal_dir.clone(),
            session::DEFAULT_SHARD_COUNT,
            wal_fsync,
            max_wal_size_bytes,
            auto_verify,
            auto_verify_threshold,
            auto_verify_interval,
        )
        .with_io_timeouts(
            Some(std::time::Duration::from_secs(io_warn_timeout_secs)),
            Some(std::time::Duration::from_secs(io_error_timeout_secs)),
            None,
        );
        // G-A2 锚点链装配：配置注入即启用（None=锚点关，行为与旧版逐位一致）
        if let Some(cfg) = anchor_config {
            session_mgr = session_mgr.with_anchor_config(cfg);
        }

        let sessions = Arc::new(Mutex::new(session_mgr));

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
            paused_sessions: Arc::new(std::sync::Mutex::new(HashSet::new())),

            heartbeat_last: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),

            sessions: sessions.clone(),

            // fork-from-archive：归档链源目录（与 SessionManager/archive_cache 同源）
            // 顺带修复：wal_dir 同时供本字段与 archive_cache 消费，
            // 需 clone 一次否则双重 move 编译失败
            wal_dir: wal_dir.clone(),

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

            shared_facts: SharedFactsLog::new(),

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
            registry_services: Arc::new(std::sync::RwLock::new(Arc::new(Vec::new()))),

            // ：审计档案只读缓存（wal_dir 透传；None=纯内存模式无档案）
            archive_cache: Arc::new(std::sync::Mutex::new(audit_archive::ArchiveCache::new(
                wal_dir,
            ))),

            // external 插件管理面端点表(默认空=无 external 插件,main.rs 生产注入)
            plugin_admins: Arc::new(std::collections::BTreeMap::new()),

            // 插件契约 v1：声明式 pack 资产注册表（默认空=无 pack,main.rs 生产注入）
            plugin_packs: crate::api::plugin_packs::PluginPackRegistry::empty(),

            // 命中统计聚合器（初始 layout 见构造器开头）
            hit_stats: Arc::new(crate::api::hit_stats::HitStatsAggregator::new(
                initial_layout,
            )),

            // 会话级 caller_role 声明表（默认空=全部会话不声明，零回归）
            caller_roles: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),

            // 惰性同 sid 复活开关（缺省开；builder/配置面可关，关闭即回到既有 404 语义）
            rehydrate_enabled: true,

            // Prometheus 指标（生产路径 main.rs 经 with_shared_metrics 注入；缺省 None=noop 兜底）
            metrics: None,
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

    /// 覆盖惰性同 sid 复活开关（builder 模式；缺省开）
    ///
    /// 关闭后续聊/查态入口对不在内存表的会话维持 404（既有语义逐位回归）。
    pub fn with_session_rehydrate(mut self, enabled: bool) -> Self {
        self.rehydrate_enabled = enabled;
        self
    }

    /// 注入规则集版本哈希（G6 最小修透传）：会话创建落 `__ruleset__.version`
    /// 锚定事实，TraceHit/Violation 规则下标可回溯解释
    ///
    /// # 失败语义
    ///
    /// 返回 `Err` 而非 panic：server 主代码路径禁用 panic-prone 构造
    /// （编译期 S1 门禁：`panic!` / `.unwrap(` / `.expect(` / `debug_assert!`）。
    /// 锁竞争在启动装配期不应发生（会话管理器此时无人持有）；真发生即启动装配序有 bug——
    /// **如实报错拒绝启动**，不静默跳过（否则版本锚定缺失，而其缺失会让
    /// TraceHit/Violation 的规则下标失去可回溯解释）。
    ///
    /// # 上下文注记（2026-10-22 修正）
    ///
    /// builder 在 **tokio runtime 内** 被调用（`main.rs` 的 `#[tokio::main] async fn main`
    /// 体内），故**不可**用 `blocking_lock()`——tokio 明令禁止在 runtime 线程上阻塞，
    /// 调用即 panic（"Cannot block the current thread from within a runtime"）。
    /// 历史注记：2026-10-22 19:07 的 903b13b 引入本注入时，调用点已在 async main 内，
    /// 而此处原注释写的是"builder 非 async 上下文…先于 runtime 进入服务"——与实际调用点
    /// 不符，导致**服务启动即崩**（该缺陷由 2026-10-22 的实机验证发现）。
    pub fn with_ruleset_version(self, version: String) -> Result<Self, String> {
        {
            // 借用作用域收敛在本块内：块结束即释放 MutexGuard 与 match 临时值，
            // 随后才可把 self 整体移出（否则 E0505：cannot move out of `self` because it is borrowed）
            match self.sessions.try_lock() {
                Ok(mgr) => mgr.set_ruleset_version(version),
                Err(_) => {
                    return Err("with_ruleset_version: 会话管理器锁在启动装配期已被占用 — \
                         会话管理器不应在启动装配段被持有（否则 G6 规则集版本锚定无法注入）"
                        .to_string())
                }
            }
        }
        Ok(self)
    }

    /// 注入 Prometheus 指标句柄（builder 模式）
    ///
    /// 复活路径的 inc_sessions 与 IoSubscriber 装配消费；未注入（桥接路径/
    /// 测试构造）时以 noop 兜底，行为一致仅无指标。
    pub fn with_shared_metrics(mut self, metrics: SharedMetrics) -> Self {
        self.metrics = Some(metrics);
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

    /// 为指定会话 spawn 回写转发任务（执行侧失败上报接线）
    ///
    /// 与 hit-stats 记录器同通道订阅、同生命周期；旗标缺省关
    /// （`EVORULE_WRITEBACK_URL` 未配置时任务立即退出，零旁路开销）。
    fn spawn_writeback_forwarder(&self, session_id: u64) {
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
                crate::api::writeback_forward::run_forwarder(rx, (*agg).clone(), session_id).await;
            }
        });
    }

    /// 为新会话 spawn IoSubscriber（create_session / from / fork
    /// 三处复用的公共构造段，防漂移）
    ///
    /// - 没有 IoSubscriber 时，session 的 IoRequest 会 60s 超时；
    /// - dispatcher 为 None（纯计算场景）时静默跳过；
    /// - `inherit_role` 为 Some 时登记会话声明（fork 语义 = 同主体派生，
    ///   声明继承父会话）；None 不登记（fail-closed：未声明 → Unknown → Deny）。
    async fn spawn_io_subscriber_for_session(
        &self,
        session_id: u64,
        metrics: SharedMetrics,
        inherit_role: Option<String>,
    ) {
        // 声明登记（fork 继承 / 显式声明两种来源；创建成功才登记）
        if let Some(role) = inherit_role {
            if let Ok(mut table) = self.caller_roles.lock() {
                table.insert(session_id, role);
            }
            tracing::info!(
                session_id,
                "会话已继承 caller_role 声明（命令入口将注入 __meta__）"
            );
        }

        if let Some(ref dispatcher) = self.dispatcher {
            let sessions = self.sessions.lock().await;

            if let Some(session) = sessions.get_session(session_id) {
                let event_rx = session.event_tx.subscribe();

                let command_tx = session.command_tx.clone();

                // 注入会话级 caller_role resolver（应用层接缝；
                // 声明随 WAL 事实，回放确定性成立）
                let mut gate = PermissionGate::new(Arc::new(self.shared_facts.clone()))
                    .with_caller_role_resolver(
                        crate::api::caller_role_resolver::session_caller_role_resolver(
                            session.facts_log.clone(),
                        ),
                    );
                // SG-2：静态路由策略资产装配（可选收紧层；缺失/解析失败不装配 + 显式留痕）
                if let Some(policy) =
                    crate::api::route_policy_loader::shared_route_policy(&self.rules_dir)
                {
                    gate = gate.with_route_policy((*policy).clone());
                }

                let mut subscriber = IoSubscriber::new(dispatcher.clone())
                    .with_metrics(metrics)
                    .with_skip(Arc::new(is_external_executor_request))
                    .with_permission_gate(gate);
                // SG-3：会话级熔断装配（可选收紧层；阈值 0 = 熔断面关闭 =
                // 误伤回退开关。每会话独立 CircuitState，计数天然会话作用域）
                if let Some(circuit_cfg) =
                    crate::api::route_policy_loader::circuit_config_from_env()
                {
                    subscriber = subscriber.with_circuit_breaker(
                        evorule_governance::circuit_breaker::CircuitState::new(circuit_cfg),
                    );
                }

                tokio::spawn(async move {
                    if let Err(e) = subscriber.run(event_rx, command_tx).await {
                        tracing::error!(
                            session_id,
                            error = %e,
                            "Session IoSubscriber 异常退出"
                        );
                    }
                });

                tracing::info!(session_id, "IoSubscriber 已为 session 启动");
            }
        }
    }

    /// 会话获取或惰性复活（续聊/查态四入口共用）
    ///
    /// 快路径（行为零变化）：会话在内存表 → touch 后返回克隆。
    /// 未命中且复活开关开：锁外读 WAL 归档并全链哈希校验（fail-closed，
    /// 损坏归档绝不复活），随后锁内按原 sid 复活（`rehydrate_session_from_wal`，
    /// 内部含并发复活双查），锁外补装配随行任务（hit-stats/回写转发/IoSubscriber，
    /// 与 create_session 成功路径同款三件套防漂移）。
    ///
    /// API 层命令 ID 计数器按归档最大 Command ID 续起——命令 ID（高位区）
    /// 与反应器 FactId（低位区）分属两个独立计数器，复活时各自按类续起
    /// 保持两区不相交，防新事实与恢复历史在 fact_id_index 内碰撞。
    // 复杂度豁免：快路径/复活/装配三段本就是文档化的一条链，拆分会割裂
    // 哈希校验 fail-closed 语义的局部性；重构属独立批次，暂以 allow 保 CI 纪律。
    #[allow(clippy::cognitive_complexity)]
    async fn session_or_rehydrate(
        &self,
        session_id: u64,
    ) -> Result<session::Session, RehydrateRejection> {
        // 快路径：内存表命中 → touch → 返回
        {
            let sessions = self.sessions.lock().await;
            if let Some(session) = sessions.get_session(session_id) {
                sessions.touch_session(session_id);
                return Ok(session);
            }
        }

        if !self.rehydrate_enabled {
            return Err(RehydrateRejection::NoArchive);
        }
        let Some(wal_dir) = self.wal_dir.as_ref() else {
            return Err(RehydrateRejection::NoArchive);
        };

        // 锁外：读归档 + 全链哈希校验（fail-closed，先于任何复活动作）
        let records = match read_archive_records(wal_dir, session_id) {
            Ok(records) => records,
            Err(ArchiveForkRejection::NoArchive) => return Err(RehydrateRejection::NoArchive),
            Err(ArchiveForkRejection::Io(e)) => {
                tracing::error!(session_id, error = %e, "复活前置：归档 WAL 读取失败");
                return Err(RehydrateRejection::Io(e));
            }
            Err(other) => {
                tracing::error!(session_id, rejection = %other, "复活前置：非预期拒绝形态");
                return Err(RehydrateRejection::Corrupted(other.to_string()));
            }
        };
        // 零记录归档（真·空 WAL，如 0 字节引导壳）＝无可复活状态：按无归档
        // 静默回落重建（与 NoArchive 同道，不打 ERROR；G6 哨兵 id 解析修复后
        // 启动期「WAL invalid fact」噪音的剩余来源即此形态）。
        if records.is_empty() {
            return Err(RehydrateRejection::NoArchive);
        }
        if let Err(rej) = replay_archive_state(session_id, &records, None) {
            match rej {
                ArchiveForkRejection::Corrupted(reason) => {
                    tracing::error!(
                        session_id,
                        %reason,
                        "复活拒绝：归档链完整性校验失败（fail-closed，不造会话）"
                    );
                    return Err(RehydrateRejection::Corrupted(reason));
                }
                ArchiveForkRejection::Io(e) => {
                    tracing::error!(session_id, error = %e, "复活拒绝：归档重放 I/O 失败");
                    return Err(RehydrateRejection::Io(e));
                }
                other => {
                    tracing::error!(session_id, rejection = %other, "复活拒绝：归档重放非预期失败");
                    return Err(RehydrateRejection::Corrupted(other.to_string()));
                }
            }
        }

        // API 层命令 ID 计数器按归档最大 Command ID 续起（高位区续起）
        let mut max_api_id: u64 = 0;
        for record in &records {
            if let evorule_reactor::Fact::Command { id, .. } = &record.fact {
                max_api_id = max_api_id.max(id.0);
            }
        }
        if max_api_id > 0 {
            self.next_id
                .fetch_max(max_api_id + 1, std::sync::atomic::Ordering::Relaxed);
        }

        // 锁内复活（SessionManager 内部含并发复活双查与名额预留）
        let session = {
            let sessions = self.sessions.lock().await;
            sessions
                .rehydrate_session_from_wal(session_id)
                .map_err(|e| match e {
                    session::SessionError::LimitExceeded { current, max } => {
                        RehydrateRejection::LimitExceeded { current, max }
                    }
                    session::SessionError::WalUnavailable { session_id, source } => {
                        RehydrateRejection::WalUnavailable {
                            session_id,
                            source: source.to_string(),
                        }
                    }
                    other => {
                        tracing::error!(session_id, error = %other, "复活拒绝：会话重建失败");
                        RehydrateRejection::Corrupted(other.to_string())
                    }
                })?
        };

        tracing::info!(session_id, "会话已从 WAL 归档按原 ID 复活");

        // 锁外补装配随行任务（同 create_session 成功路径三件套，防漂移）
        let metrics = self
            .metrics
            .clone()
            .unwrap_or_else(evorule_governance::metrics::noop_metrics);
        metrics.inc_sessions();
        self.spawn_hit_stats_recorder(session_id);
        self.spawn_writeback_forwarder(session_id);
        self.spawn_io_subscriber_for_session(session_id, metrics, None)
            .await;

        Ok(session)
    }

    /// 引导面复活入口：启动期优先复活原生产会话。
    ///
    /// 复用 [`Self::session_or_rehydrate`] 全链语义（归档哈希校验 fail-closed /
    /// API 命令计数器续起 / 随行任务三件套装配），拒绝原因转字符串供启动期
    /// ERROR 留痕。复活开关关闭（--session-rehydrate false）时原语自然回落
    /// NoArchive 文案——引导面按「复活失败」处理并回落重建分支，与既有
    /// 开发语义逐位一致；生产档位该开关已被启动门强制为 true。
    pub async fn revive_for_bootstrap(&self, session_id: u64) -> Result<(), String> {
        self.session_or_rehydrate(session_id)
            .await
            .map(|_| ())
            .map_err(|rej| rej.to_string())
    }

    /// 读取会话的 caller_role 声明（未声明 = None）
    fn declared_caller_role(&self, session_id: u64) -> Option<String> {
        self.caller_roles.lock().ok()?.get(&session_id).cloned()
    }

    /// 移除会话声明（会话终结路径调用，防声明表无界增长）
    fn remove_caller_role(&self, session_id: u64) {
        if let Ok(mut table) = self.caller_roles.lock() {
            table.remove(&session_id);
        }
    }

    /// 对声明过的会话向指令根部注入 `__meta__.caller_role`
    ///
    /// - 仅声明会话注入；未声明指令原样（零回归）；
    /// - 注入发生在 Schema 门禁之后（transform_rule 条目级键白名单针对用户/LLM
    ///   产物，服务端系统注入不属其对象）；TCB 引擎对指令根部额外键零消费
    ///   （transition 只读 type、executor 只读 type/params，差分尽调已实证）；
    /// - instruction 非 object（理论上门禁已拦）不注入。
    fn inject_caller_role(&self, session_id: u64, instruction: &mut serde_json::Value) {
        if let Some(role) = self.declared_caller_role(session_id) {
            if let Some(obj) = instruction.as_object_mut() {
                let meta = obj
                    .entry("__meta__".to_string())
                    .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
                if let Some(m) = meta.as_object_mut() {
                    m.insert("caller_role".to_string(), serde_json::Value::String(role));
                }
            }
        }
    }

    /// 注入 I/O 分发器（builder 模式）
    ///
    ///
    /// 注入后，每个新创建的 session 会自动 spawn 一个 IoSubscriber，
    ///
    /// 将 session reactor 的 IoRequest 分发到注册的 handler。
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
    /// registry_services 锁写（poison 恢复同上）
    fn registry_lock_write_mut(&self) -> std::sync::RwLockWriteGuard<'_, Arc<Vec<ServiceMeta>>> {
        match self.registry_services.write() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// registry_services 快照（值语义 clone；poison 恢复：into_inner 取数据）
    fn registry_services_vec(&self) -> Vec<ServiceMeta> {
        let g = match self.registry_services.read() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        g.as_ref().clone()
    }

    pub fn with_registry_services<I: IntoIterator<Item = ServiceMeta>>(self, metas: I) -> Self {
        let mut v = self.registry_services_vec();
        v.extend(metas);
        v.sort_by(|a, b| a.name.cmp(&b.name));
        *self.registry_lock_write_mut() = Arc::new(v);
        self
    }

    /// registry_services 快照（运行时热更读路径；确定性 name 序由写入侧维护）
    pub(crate) fn registry_services_snapshot(&self) -> Arc<Vec<ServiceMeta>> {
        Arc::new(self.registry_services_vec())
    }

    /// 运行时合入/覆盖单个服务元数据（安装自动化三期：install 成功后调用）
    pub fn merge_registry_service(&self, meta: ServiceMeta) {
        let mut v = self.registry_services_vec();
        v.retain(|m| m.name != meta.name);
        v.push(meta);
        v.sort_by(|a, b| a.name.cmp(&b.name));
        *self.registry_lock_write_mut() = Arc::new(v);
    }

    /// 运行时移除服务元数据（uninstall/disable 后调用）；返回是否确有移除
    pub fn remove_registry_service(&self, name: &str) -> bool {
        let mut v = self.registry_services_vec();
        let before = v.len();
        v.retain(|m| m.name != name);
        let removed = v.len() != before;
        *self.registry_lock_write_mut() = Arc::new(v);
        removed
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

    /// 注入跨会话共享事实存储（builder 模式，C6 入口守卫判定源）
    ///
    /// main.rs 启动期把 WAL 恢复（或内存新建）的 shared_facts 注入 SessionApi，
    /// 供全部 per-session `IoSubscriber` 的 `PermissionGate` 判定使用；与全局
    /// 分发路径共用同一实例（权限条目/版本域一致，D8）。
    pub fn with_shared_facts(mut self, shared_facts: SharedFactsLog) -> Self {
        self.shared_facts = shared_facts;

        self
    }

    /// 注入 external 插件管理面端点表（builder 模式，插件审批代理）
    ///
    /// main.rs 启动期从 external 挂载结果 + env（EVORULE_PLUGIN_ADMIN_TOKEN__<ID>）
    /// 派生注入；代理端点据此转发（base_url）并附 Bearer token。
    pub fn with_plugin_admins(
        mut self,
        admins: std::collections::BTreeMap<String, PluginAdminEndpoint>,
    ) -> Self {
        self.plugin_admins = Arc::new(admins);
        self
    }

    /// 注入声明式 pack 资产注册表（builder 模式，插件契约 v1 资产面）
    ///
    /// main.rs 启动期 `load_plugin_packs` 装载后注入；运行期只读（无热加载）。
    pub fn with_plugin_packs(
        mut self,
        packs: crate::api::plugin_packs::PluginPackRegistry,
    ) -> Self {
        self.plugin_packs = packs;
        self
    }

    /// 插件契约 v1 pack 注册表只读访问（plugin_packs 模块 handler 使用）
    pub(crate) fn plugin_packs(&self) -> &crate::api::plugin_packs::PluginPackRegistry {
        &self.plugin_packs
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
            let mut interval =
                tokio::time::interval(evorule_governance_host::session::REAPER_INTERVAL);

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
    /// - `Ok((old_len, new_len, rejected_overlaps))`：旧规则数、新规则数（剔除后）、
    ///   装载防线被拒重叠条目清单（审计面；同时已由 [`Self::log_rejected_overlaps`] 大声上报）
    ///
    /// - `Err(String)`：读取/解析失败（失败时旧规则保持不变）
    ///
    pub async fn reload_from_disk(
        &self,
    ) -> Result<
        (
            usize,
            usize,
            Vec<RejectedOverlapEntry>,
            Vec<RejectedDomainEntry>,
            Vec<RejectedGateFileEntry>,
        ),
        String,
    > {
        let (new_transforms, new_layout, rejected_overlaps, rejected_domains, rejected_gate_files) =
            Self::load_merged_with_layout(&self.core_eval_path, &self.rules_dir)?;

        // 装载防线横幅：reload 与启动共用（0 = 干净 info；>0 = 逐条 ERROR + 汇总）
        Self::log_rejected_overlaps(&rejected_overlaps, "reload");

        // domain 求值装载门禁横幅（同款双态口径）
        Self::log_rejected_domains(&rejected_domains, "reload");

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

        Ok((
            old_len_mgr.max(old_len_cache),
            new_len,
            rejected_overlaps,
            rejected_domains,
            rejected_gate_files,
        ))
    }

    /// 从文件系统合并加载：TCB 宪法 core_eval.json（在前）+ rules_dir/*.json（在后，按文件名字典序）
    pub fn load_merged_transforms_from_fs(
        core_eval_path: &std::path::Path,

        rules_dir: &std::path::Path,
    ) -> Result<Vec<JsonValue>, String> {
        Self::load_merged_with_layout(core_eval_path, rules_dir).map(|(rules, _, _, _, _)| rules)
    }

    /// 合并加载并产出规则集 layout（单一权威装载点）
    ///
    /// 与 [`Self::load_merged_transforms_from_fs`] 同一次读取产出三件事：
    /// 合并规则列表（引擎输入）+ [`RulesetLayout`]（下标→来源/指令类型解析 +
    /// 规则集版本哈希）+ 装载防线被拒重叠条目清单（审计面）。
    /// 避免双份装载逻辑漂移——hit-stats 的下标解析正确性
    /// 依赖"layout 与引擎合并顺序一致"这一不变式（防线剔除同步作用于二者）。
    pub fn load_merged_with_layout(
        core_eval_path: &std::path::Path,

        rules_dir: &std::path::Path,
    ) -> Result<MergedLoadOutcome, String> {
        // ── 宪法件读取+装载收口（single constitution load path）──
        // [CON-S-006] 旧壳名已退役（仓内零命中）：读文件→解析→
        // schema 校验→剥壳→三门→provenance 四步直调 evorule_rule_loader 唯一通道，
        // 与其他所有装载面同源。
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
        let ctx = LoadContext::constitution(&core_eval_path.display().to_string());

        // ── 装载门合并（R5/[CON-S-005] 定稿：两层分工之「装载门合并」层）──
        // 宪法部件在前 + rules_dir 部件按注入序在后，装载与拼装统一在
        // load_rule_set_merged 函数内完成（禁调用方拼装）；各部件以自身
        // ctx 重过四步门（与单件门确定性等判），任一部件失败=整体拒载。
        // io 重叠/domain 防线是生产防线（非三门），仍在本函数后续段执行。
        let (dir_parts, dir_labels, rejected_gate_files) =
            Self::load_rules_dir_with_sources(rules_dir);
        let mut parts = Vec::with_capacity(1 + dir_parts.len());
        parts.push(MergedPart { doc: tcb_json, ctx });
        parts.extend(dir_parts);
        let merged = evorule_rule_loader::load_rule_set_merged(&parts).map_err(|e| {
            format!(
                "合并装载门禁未过（宪法 {} + rules_dir 部件）: {}",
                core_eval_path.display(),
                e
            )
        })?;
        let constitution_len = merged.parts[0].rule_count;
        let constitution = &merged.engine_rules[..constitution_len];

        // 修复(2026-09-01): call_external 指令规则是 LLM 审计桥的平台契约。
        // 会话反应器的 IoRequest 完全由宪法规则驱动;若宪法缺该规则,LLM 审计桥命令会被
        // all([]) 兜底规则静默 no-op(无 IoRequest、无 Error 事实),违反"拒绝静默通过"原则。
        // 因此启动期 fail-fast 校验,并给出可自诊断的修复指引。
        // （transform 数组空态由 rule_loader minItems=1 门拒绝，此处不再重复检查。）
        let has_call_external = constitution.iter().any(|r| {
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

        // 来源标签展开（[CON-S-005] 拼装在 loader 内完成，标签按部件 rule_count
        // 对齐引擎合并序——layout 下标一致性不变式保持）
        let mut sources = Vec::with_capacity(merged.engine_rules.len());
        sources.extend(std::iter::repeat_n(
            "core_eval".to_string(),
            constitution_len,
        ));
        for (i, label) in dir_labels.iter().enumerate() {
            sources.extend(std::iter::repeat_n(
                label.clone(),
                merged.parts[i + 1].rule_count,
            ));
        }

        let rules = merged.engine_rules;

        // 装载期 I/O 权利面独占防线：先剔除重叠条目，再以剔除后列表构建
        // layout（下标一致性不变式保持：layout 与引擎输入同源同长度）
        let (rules, sources, rejected_overlaps) =
            Self::reject_overlapped_io_rules(rules, sources, constitution_len);

        // domain 求值装载门禁（三态声明化方案 v4）：再剔除 eq/lt 域
        // 「缺 on_missing 声明 / 声明非法值 / ValueLiteralAmbiguous 写作错误」条目
        // （fail-closed 无警告通道；宪法前缀段豁免同装载防线；动态域=字符串形态
        // `__` 引用天然豁免——域对象运行时才定形，归运行时防御层）
        let (rules, sources, rejected_domains) =
            Self::reject_undeclared_domain_rules(rules, sources, constitution_len);

        let layout = crate::api::hit_stats::RulesetLayout::from_rules(&rules, sources);

        Ok((
            rules,
            layout,
            rejected_overlaps,
            rejected_domains,
            rejected_gate_files,
        ))
    }

    // =========================================================================
    // 装载期 I/O 权利面独占防线（2026-09-27，立项档 §四）
    //
    // 缺陷：同一 io_type 的 I/O 生命周期规则（发射 io_request / 消费
    // `__io_results__.{T}`）在合并规则集中多于一处时，TCB all-match 顺序执行
    // 语义下消费轮「先消费清除 → 后位规则重发 io_request → IoRequired 早退
    // 丢半成品」→ 会话无限循环（实证：bundle e3-call-external 与宪法 L1
    // call_external 规则语义重叠致 evo-agent minimax 全部会话死循环）。
    //
    // 防线：合并列表按序扫描，同一 io_type 的「发射权」「消费权」全集合仅首条
    // 持有；后续重叠条目装载期拒载（fail-closed，无影子期/逃生阀——立项档
    // §4.2 案 a）+ 逐条 ERROR + 汇总横幅（[`Self::log_rejected_overlaps`]）。
    //
    // 宪法豁免：core_eval 前缀段条目只声明权利、永不拒载（宪法优先；宪法内部
    // 组合由宪法维护者负责，如 L1 call_service 双规则靠 domain 互斥共存），
    // 拒载面仅限 rules_dir 并入条目。发射权独占同时覆盖「双路径规则重叠」
    // （发射+消费同条，e2/e3 型）与「无守卫纯发射重叠」（t0-r2/flow-e 型）；
    // 消费权独占闭合「跨规则 消费者在前+发射者在后」组合。项目方扩面裁定
    // 见立项档 §4.0-5。
    //
    // 诚实边界：参数化 io_type（`__` 前缀引用路径）静态不可判定，不入检测面；
    // 检测为静态特征匹配（path/attr/value 引用 `__io_results__.{T}`），不执行
    // 规则语义，性能零负担。
    // =========================================================================

    /// 从 transform 指令子树提取 I/O 能力面：`(发射 io_type 集, 消费 io_type 集)`。
    ///
    /// - **发射面**：子树中 `io_request` 指令的 `params.io_type` 为**字面量**
    ///   （`__` 前缀引用路径属参数化形态，静态不可判定 → 诚实边界外）；
    /// - **消费面**：子树 `path`/`attr`/`value` 字符串字段引用 `__io_results__.{T}`，
    ///   覆盖 exists 判定、set null 清除、set value 读取三类消费特征
    ///   （描述性 prose 不在上述键内，不会误报）；
    /// - 递归覆盖 branch on_true/on_false、push instructions、domain inner/not 等全部嵌套。
    fn io_capability(rule: &JsonValue) -> (BTreeSet<String>, BTreeSet<String>) {
        const IO_RESULTS_MARKER: &str = "__io_results__.";

        fn walk(v: &JsonValue, emit: &mut BTreeSet<String>, consume: &mut BTreeSet<String>) {
            match v {
                JsonValue::Array(items) => {
                    for it in items {
                        walk(it, emit, consume);
                    }
                }
                JsonValue::Object(map) => {
                    // 发射面：io_request 的字面量 io_type
                    if map.get("type").and_then(|t| t.as_str()) == Some("io_request") {
                        if let Some(io_type) = map
                            .get("params")
                            .and_then(|p| p.get("io_type"))
                            .and_then(|t| t.as_str())
                        {
                            if !io_type.is_empty() && !io_type.starts_with("__") {
                                emit.insert(io_type.to_string());
                            }
                        }
                    }
                    // 消费面：path/attr/value 引用 __io_results__.{T}
                    for key in ["path", "attr", "value"] {
                        if let Some(s) = map.get(key).and_then(|x| x.as_str()) {
                            if let Some(pos) = s.find(IO_RESULTS_MARKER) {
                                let rest = &s[pos + IO_RESULTS_MARKER.len()..];
                                let seg = rest.split('.').next().unwrap_or("");
                                if !seg.is_empty() && !seg.starts_with("__") {
                                    consume.insert(seg.to_string());
                                }
                            }
                        }
                    }
                    for child in map.values() {
                        walk(child, emit, consume);
                    }
                }
                _ => {}
            }
        }

        let mut emit = BTreeSet::new();
        let mut consume = BTreeSet::new();
        walk(rule, &mut emit, &mut consume);
        (emit, consume)
    }

    /// 被拒条目特征摘要（顶层指令类型 + domain.instruction_type，如 `branch[call_external]`）
    fn rule_summary(rule: &JsonValue) -> String {
        let ty = rule.get("type").and_then(|t| t.as_str()).unwrap_or("?");
        let it = rule
            .get("params")
            .and_then(|p| p.get("domain"))
            .and_then(|d| d.get("instruction_type"))
            .and_then(|t| t.as_str());
        match it {
            Some(it) => format!("{}[{}]", ty, it),
            None => ty.to_string(),
        }
    }

    /// 权利面独占判定（纯函数）：按序扫描合并列表，返回
    /// `(kept_rules, kept_sources, rejected)`。
    ///
    /// - `constitution_len`：合并列表前缀中宪法条目数——宪法条目只声明权利、永不拒载；
    /// - rules_dir 条目：其发射 io_type 已被更早条目声明，或其消费 io_type 已被
    ///   更早条目声明 → 拒载（fail-closed）；
    /// - 被拒条目携带原始下标/来源/特征摘要/重叠明细，全程可审计。
    fn reject_overlapped_io_rules(
        rules: Vec<JsonValue>,
        sources: Vec<String>,
        constitution_len: usize,
    ) -> (Vec<JsonValue>, Vec<String>, Vec<RejectedOverlapEntry>) {
        use std::collections::BTreeMap;

        // 权利声明账本：io_type → (首声明下标, 来源标签)
        let mut emit_claims: BTreeMap<String, (usize, String)> = BTreeMap::new();
        let mut consume_claims: BTreeMap<String, (usize, String)> = BTreeMap::new();

        let mut kept_rules = Vec::with_capacity(rules.len());
        let mut kept_sources = Vec::with_capacity(sources.len());
        let mut rejected = Vec::new();

        for (idx, (rule, source)) in rules.into_iter().zip(sources).enumerate() {
            let (emit, consume) = Self::io_capability(&rule);
            let is_constitution = idx < constitution_len;

            let mut overlaps: Vec<RejectedOverlapDetail> = Vec::new();
            for t in &emit {
                if let Some((owner_idx, owner_src)) = emit_claims.get(t) {
                    overlaps.push(RejectedOverlapDetail {
                        io_type: t.clone(),
                        kind: "emit".to_string(),
                        claimed_by: format!("{}#{}", owner_src, owner_idx),
                    });
                }
            }
            for t in &consume {
                if let Some((owner_idx, owner_src)) = consume_claims.get(t) {
                    overlaps.push(RejectedOverlapDetail {
                        io_type: t.clone(),
                        kind: "consume".to_string(),
                        claimed_by: format!("{}#{}", owner_src, owner_idx),
                    });
                }
            }

            if !overlaps.is_empty() && !is_constitution {
                rejected.push(RejectedOverlapEntry {
                    index: idx,
                    source,
                    rule_summary: Self::rule_summary(&rule),
                    overlaps,
                });
                continue; // 拒载：不进保留列表，权利面不更新
            }

            // 保留条目（含宪法条目）声明其权利面
            for t in emit {
                emit_claims.entry(t).or_insert((idx, source.clone()));
            }
            for t in consume {
                consume_claims.entry(t).or_insert((idx, source.clone()));
            }
            kept_rules.push(rule);
            kept_sources.push(source);
        }

        (kept_rules, kept_sources, rejected)
    }

    // =========================================================================
    // domain 求值装载门禁（2026-10-01，静默 false 根修方案 2' v4）
    //
    // 缺陷：eq/lt 域「状态侧静默 false」——路径缺失/类型不可比/value 被字面量化
    // 三种情形与「真实比对为假」在输出上不可区分（审计不可归因），已致真实事故
    // 历史事故：pack v1 eq 比对从未生效，部署后无人知。
    //
    // 防线：装载期对字面 eq/lt 域对象三道拒收门（fail-closed，无警告通道——
    // TCB/server 治理世界观没有警告，只有拒收与放行）：
    //   ① missing_declaration：缺 on_missing 显式声明（作者必须显式决策——
    //      error=Missing 拒绝执行 / unsat=Missing 走 on_false+归因落账）；
    //   ② invalid_declaration：on_missing 非 error/unsat（声明值非法=显式拒收，
    //      不静默回退缺省）；
    //   ③ value_literal_ambiguous：value 字符串命中 exec 根段点路径形态（像路径
    //      引用但缺 __ 前缀的历史写作错误形态）——一律拒绝、无宽容选项；
    //      判定谓词复用 TCB 导出 is_root_segment_dot_path（零镜像复算）。
    //
    // 豁免面：
    //   - 宪法前缀段条目永不拒载（同装载防线豁免；宪法内部组合由宪法维护者负责）；
    //   - 动态域豁免：domain 值为 `__` 字符串引用（域对象运行时才定形）静态不可
    //     判定——本门禁只对字面域对象生效，字符串形态 walk 天然不命中；动态到达
    //     缺声明由 TCB 运行时防御层兜底（兼容缺省 unsat + 归因落账，T1/T2 范围）。
    //
    // 存量不追溯：已装载规则不受影响；重新装载/更新/导入时触发校验（渐进收敛）。
    // =========================================================================

    /// 单规则 domain 门禁违规提取（纯函数）：walk 整棵指令树，收集全部 eq/lt/in
    /// 域对象的门违规。eq/lt/in 类型键仅域对象持有（transform_rule 5 元指令与
    /// 指令层类型均不含 eq/lt/in），对象形态判定无误伤面。
    /// 三道门（missing_declaration/invalid_declaration/value_literal_ambiguous）
    /// 为 eq/lt/in 共用；in 域另有第 4 道 values_declaration 门（design-19 R-3）。
    fn domain_gate_violations(rule: &JsonValue) -> Vec<DomainViolation> {
        const ON_MISSING_VALUES: [&str; 2] = ["error", "unsat"];

        fn walk(v: &JsonValue, out: &mut Vec<DomainViolation>) {
            match v {
                JsonValue::Array(items) => {
                    for it in items {
                        walk(it, out);
                    }
                }
                JsonValue::Object(map) => {
                    let ty = map.get("type").and_then(|t| t.as_str());
                    if matches!(ty, Some("eq") | Some("lt") | Some("in")) {
                        let ty = ty.unwrap_or("");
                        let path_desc = map
                            .get("path")
                            .and_then(|p| p.as_str())
                            .map(|p| format!("path=\"{p}\""))
                            .unwrap_or_else(|| "path 缺失".to_string());
                        match map.get("on_missing") {
                            None => out.push(DomainViolation {
                                door: "missing_declaration".to_string(),
                                detail: format!(
                                    "{ty} 域 {path_desc} 缺 on_missing 显式声明（error=Missing 拒执行 / unsat=Missing 走 on_false+归因）"
                                ),
                            }),
                            Some(d) => {
                                if !matches!(d.as_str(), Some(s) if ON_MISSING_VALUES.contains(&s))
                                {
                                    out.push(DomainViolation {
                                        door: "invalid_declaration".to_string(),
                                        detail: format!(
                                            "{ty} 域 {path_desc} on_missing 声明值非法（仅允许 error/unsat）"
                                        ),
                                    });
                                }
                            }
                        }
                        if let Some(val) = map.get("value").and_then(|v| v.as_str()) {
                            if evorule_tcb::domain::is_root_segment_dot_path(val) {
                                out.push(DomainViolation {
                                    door: "value_literal_ambiguous".to_string(),
                                    detail: format!(
                                        "{ty} 域 {path_desc} value=\"{val}\" 呈 exec 根段点路径形态但缺 __ 前缀（ValueLiteralAmbiguous 写作错误，运行时将被字面量化静默比对）"
                                    ),
                                });
                            }
                        }
                        if ty == "in" {
                            // 第 4 道 values_declaration 门（design-19 R-3）：
                            // values 缺失/非数组/空数组 → 拒载（fail-fast，空集大概率写作错误）
                            let values_arr = map.get("values").and_then(|v| v.as_array());
                            if map.get("values").is_none() {
                                out.push(DomainViolation {
                                    door: "values_declaration".to_string(),
                                    detail: format!(
                                        "in 域 {path_desc} 缺 values 显式声明（集合成员判定必备）"
                                    ),
                                });
                            } else if values_arr.is_none() {
                                out.push(DomainViolation {
                                    door: "values_declaration".to_string(),
                                    detail: format!(
                                        "in 域 {path_desc} values 非数组（集合成员判定要求字面量数组）"
                                    ),
                                });
                            } else if values_arr.is_some_and(|a| a.is_empty()) {
                                out.push(DomainViolation {
                                    door: "values_declaration".to_string(),
                                    detail: format!(
                                        "in 域 {path_desc} values 为空数组（空集恒假，大概率写作错误——fail-fast 拒载）"
                                    ),
                                });
                            }
                            // value_literal_ambiguous 门扩 values 字符串元素逐个检查
                            if let Some(arr) = values_arr {
                                for el in arr.iter() {
                                    if let Some(s) = el.as_str() {
                                        if evorule_tcb::domain::is_root_segment_dot_path(s) {
                                            out.push(DomainViolation {
                                                door: "value_literal_ambiguous".to_string(),
                                                detail: format!(
                                                    "in 域 {path_desc} values 元素 \"{s}\" 呈 exec 根段点路径形态但缺 __ 前缀（ValueLiteralAmbiguous 写作错误，运行时将被字面量化静默比对）"
                                                ),
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                    for child in map.values() {
                        walk(child, out);
                    }
                }
                _ => {}
            }
        }

        let mut out = Vec::new();
        walk(rule, &mut out);
        out
    }

    /// domain 门禁装载判定（纯函数）：按序扫描合并列表，返回
    /// `(kept_rules, kept_sources, rejected)`。宪法前缀段豁免同装载防线；
    /// 剔除同步作用于合并列表与 sources（下标一致性不变式保持）。
    fn reject_undeclared_domain_rules(
        rules: Vec<JsonValue>,
        sources: Vec<String>,
        constitution_len: usize,
    ) -> (Vec<JsonValue>, Vec<String>, Vec<RejectedDomainEntry>) {
        let mut kept_rules = Vec::with_capacity(rules.len());
        let mut kept_sources = Vec::with_capacity(sources.len());
        let mut rejected = Vec::new();

        for (idx, (rule, source)) in rules.into_iter().zip(sources).enumerate() {
            let violations = Self::domain_gate_violations(&rule);
            if !violations.is_empty() && idx >= constitution_len {
                rejected.push(RejectedDomainEntry {
                    index: idx,
                    source,
                    rule_summary: Self::rule_summary(&rule),
                    violations,
                });
                continue; // 拒载：不进保留列表
            }
            kept_rules.push(rule);
            kept_sources.push(source);
        }

        (kept_rules, kept_sources, rejected)
    }

    /// bundle 导入前置 domain 门禁预判（纯函数）：缺声明/非法声明/value 写作错误
    /// 均为单条目绝对判定、不依赖装载基线（与 I/O 权利面重叠预判的「账本比对」
    /// 不同），逐新条目独立 walk 即可；落盘前显式 Err（调用方 400）。
    fn predict_import_undeclared_domains(
        bundle_id: &str,
        entries: &[(String, JsonValue)],
    ) -> Vec<RejectedDomainEntry> {
        let mut rejected = Vec::new();
        for (pos, (entry_id, rule)) in entries.iter().enumerate() {
            let violations = Self::domain_gate_violations(rule);
            if !violations.is_empty() {
                rejected.push(RejectedDomainEntry {
                    index: pos,
                    source: format!("bundles/{bundle_id}/{entry_id}"),
                    rule_summary: Self::rule_summary(rule),
                    violations,
                });
            }
        }
        rejected
    }

    /// ③.6 导入预判拒载明细组装（纯函数）：逐条目「来源(特征) 门[明细]」拼接。
    fn format_import_domain_rejection(rejected: &[RejectedDomainEntry]) -> String {
        rejected
            .iter()
            .map(|r| {
                let vs = r
                    .violations
                    .iter()
                    .map(|v| format!("{}[{}]", v.door, v.detail))
                    .collect::<Vec<_>>()
                    .join("；");
                format!("`{}`({}) {}", r.source, r.rule_summary, vs)
            })
            .collect::<Vec<_>>()
            .join("；")
    }

    /// ③.5 I/O 权利面重叠预判拒载明细组装（纯函数）：
    /// 逐条目「来源(特征) 与已装载 权利人 重叠 [明细]」拼接。
    fn format_import_io_rejection(predicted: &[RejectedOverlapEntry]) -> String {
        predicted
            .iter()
            .map(|r| {
                let ovs = r
                    .overlaps
                    .iter()
                    .map(|o| format!("{} {}", o.kind, o.io_type))
                    .collect::<Vec<_>>()
                    .join("、");
                format!(
                    "`{}`({}) 与已装载 {} 重叠 [{}]",
                    r.source, r.rule_summary, r.overlaps[0].claimed_by, ovs
                )
            })
            .collect::<Vec<_>>()
            .join("；")
    }

    /// domain 门禁汇总横幅（启动 main.rs 与 reload 共用；对齐装载防线双态口径）。
    ///
    /// - 0 拒载 → info（门禁干净）；
    /// - >0 → 逐条 ERROR（下标/来源/特征/门类/明细）+ 汇总 ERROR。
    pub fn log_rejected_domains(rejected: &[RejectedDomainEntry], context: &str) {
        if rejected.is_empty() {
            tracing::info!(
                target: "domain_gate",
                "domain 求值装载门禁汇总（{}）：0 拒载——eq/lt 域 on_missing 声明与 value 形态全部合规",
                context
            );
            return;
        }
        for e in rejected {
            let details = e
                .violations
                .iter()
                .map(|v| format!("{}({})", v.door, v.detail))
                .collect::<Vec<_>>()
                .join("; ");
            tracing::error!(
                target: "domain_gate",
                index = e.index,
                source = %e.source,
                "domain 求值装载门禁：拒载违规规则 [{}] {} — {}",
                e.rule_summary, e.source, details
            );
        }
        tracing::error!(
            target: "domain_gate",
            count = rejected.len(),
            "domain 求值装载门禁汇总（{}）：{} 条规则被拒载（eq/lt 域必须显式声明 on_missing；\
             value 不得呈缺 __ 前缀的 exec 根段点路径形态——状态侧静默 false 通道已从装载面关闭，\
             补齐声明或修正 value 后可重新装载）",
            context,
            rejected.len()
        );
    }

    /// bundle 导入前置 I/O 权利面重叠预判（纯函数）：落盘前以同一判定语义给出
    /// 「导入即拒」的显式 400 明细，替代「落盘成功 + reload 拒载半激活」的静默形态。
    ///
    /// 与装载期 [`Self::reject_overlapped_io_rules`] 同源同语义（同一
    /// [`Self::io_capability`] 能力提取 + 同款权利账本比对，零判定分叉）：
    ///
    /// - 基线：当前装载集（最近一次防线剔除后的 kept 列表）全部条目播种权利账本；
    ///   来源位于 `exclude_prefixes` 之下的条目除外——它们将被本次导入原子替换
    ///   （同 bundle_id 重导入 / 同 dataset 单激活换版），不构成真实基线冲突；
    /// - 新条目按 `entries` 顺序逐条判定：发射/消费权利与基线或更早新条目重叠 → 拒；
    /// - 取基线方向比装载期「首声明序」更保守：新条目与既有条目冲突一律拒导入——
    ///   若放行，装载序可能反转让既有部署面规则的权利被 bundle 条目静默挤占
    ///   （部署面权利被剥夺比显式 400 更不可接受）；
    /// - 返回条目 `index` = 新条目在 `entries` 中的序位，`source` =
    ///   `bundles/{bundle_id}/{entry_id}`（与装载期来源标签同口径）；最终裁决
    ///   权威仍是装载期防线（双保险，覆盖非导入通道的 rules_dir 变更）。
    fn predict_import_io_overlaps(
        loaded: &[JsonValue],
        loaded_sources: &[String],
        bundle_id: &str,
        exclude_prefixes: &[String],
        entries: &[(String, JsonValue)],
    ) -> Vec<RejectedOverlapEntry> {
        use std::collections::BTreeMap;

        let excluded = |src: &str| exclude_prefixes.iter().any(|p| src.starts_with(p.as_str()));

        // 1. 基线播种：当前装载集 kept 条目（已过防线，相互零冲突）
        let mut emit_claims: BTreeMap<String, (usize, String)> = BTreeMap::new();
        let mut consume_claims: BTreeMap<String, (usize, String)> = BTreeMap::new();
        for (idx, (rule, source)) in loaded.iter().zip(loaded_sources.iter()).enumerate() {
            if excluded(source) {
                continue;
            }
            let (emit, consume) = Self::io_capability(rule);
            for t in emit {
                emit_claims.entry(t).or_insert((idx, source.clone()));
            }
            for t in consume {
                consume_claims.entry(t).or_insert((idx, source.clone()));
            }
        }

        // 2. 新条目逐条判定（重叠明细结构与装载期防线同构）
        let mut rejected = Vec::new();
        for (pos, (entry_id, rule)) in entries.iter().enumerate() {
            let (emit, consume) = Self::io_capability(rule);
            let self_source = format!("bundles/{}/{}", bundle_id, entry_id);
            let mut overlaps: Vec<RejectedOverlapDetail> = Vec::new();
            for t in &emit {
                if let Some((owner_idx, owner_src)) = emit_claims.get(t) {
                    overlaps.push(RejectedOverlapDetail {
                        io_type: t.clone(),
                        kind: "emit".to_string(),
                        claimed_by: format!("{}#{}", owner_src, owner_idx),
                    });
                }
            }
            for t in &consume {
                if let Some((owner_idx, owner_src)) = consume_claims.get(t) {
                    overlaps.push(RejectedOverlapDetail {
                        io_type: t.clone(),
                        kind: "consume".to_string(),
                        claimed_by: format!("{}#{}", owner_src, owner_idx),
                    });
                }
            }
            if !overlaps.is_empty() {
                rejected.push(RejectedOverlapEntry {
                    index: pos,
                    source: self_source,
                    rule_summary: Self::rule_summary(rule),
                    overlaps,
                });
                continue;
            }
            // 无重叠新条目声明其权利面（同包后位条目与之前条目冲突同样可判定）
            for t in emit {
                emit_claims.entry(t).or_insert((pos, self_source.clone()));
            }
            for t in consume {
                consume_claims
                    .entry(t)
                    .or_insert((pos, self_source.clone()));
            }
        }
        rejected
    }

    /// 装载防线汇总横幅（启动 main.rs 与 reload 共用；对齐影子汇总双态口径）。
    ///
    /// - 0 拒载 → info（权利面干净）；
    /// - >0 → 逐条 ERROR（下标/来源/特征/io_type/重叠面/首声明者）+ 汇总 ERROR。
    pub fn log_rejected_overlaps(rejected: &[RejectedOverlapEntry], context: &str) {
        if rejected.is_empty() {
            tracing::info!(
                target: "io_overlap_gate",
                "I/O 权利面独占防线汇总（{}）：0 重叠拒载——规则集发射/消费权利面无冲突",
                context
            );
            return;
        }
        for e in rejected {
            let details = e
                .overlaps
                .iter()
                .map(|d| {
                    format!(
                        "{}({}重叠 ← 首声明 {})",
                        d.io_type,
                        if d.kind == "emit" { "发射" } else { "消费" },
                        d.claimed_by
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            tracing::error!(
                target: "io_overlap_gate",
                index = e.index,
                source = %e.source,
                "I/O 权利面独占防线：拒载重叠规则 [{}] {} — {}",
                e.rule_summary, e.source, details
            );
        }
        tracing::error!(
            target: "io_overlap_gate",
            count = rejected.len(),
            "I/O 权利面独占防线汇总（{}）：{} 条重叠规则被拒载（同 io_type 发射/消费权利仅首条持有；\
             被拒条目为冗余重复，业务语义由首条承担；否则 all-match 语义下消费轮重发将致会话无限循环）",
            context,
            rejected.len()
        );
    }

    /// 扫描业务规则目录（递归，含 `rules/bundles/{bundle_id}/` 子目录，T3），按注入序
    /// 治理排序加载所有 *.json 的 transform 数组（排除 `bundle_manifest.json`）：
    /// 晋升约束（`00_constraint_promoted_` 前缀 + `metadata.promoted_at`）按晋升时间
    /// 降序排前，其余按完整路径字典序兜底（全序确定；见 [`rules_dir_load_order_key`]）。
    ///
    ///
    /// 目录不存在或读取失败时返回空 Vec（不报错）；单个文件解析失败时
    ///
    /// warn 日志并跳过该文件（fail-soft，保证热重载可用性）。
    ///
    /// 装载 rules_dir 为合并部件清单（per-part 来源标签）
    ///
    /// 来源标签 = 相对 rules_dir 的文件路径（`/` 归一化），与部件清单等长；
    /// 规则条数展开由调用方按合并结果的 parts[i].rule_count 完成。
    /// 目录不存在返回空（纯宪法规则集）。
    fn load_rules_dir_with_sources(
        rules_dir: &std::path::Path,
    ) -> (Vec<MergedPart>, Vec<String>, Vec<RejectedGateFileEntry>) {
        if !rules_dir.exists() {
            return (Vec::new(), Vec::new(), Vec::new());
        }

        let mut paths: Vec<std::path::PathBuf> = Vec::new();

        Self::collect_json_files_recursive(rules_dir, &mut paths);

        // 注入序治理（确定性显式排序键，替代单一路径字典序）：
        // 晋升约束（promoted_at 降序）排前 → 新约束先于种子类既有 enforce 求值，
        // 命中即终止 → 归因变更为新约束、种子不再重复拦截；其余文件（种子/业务/
        // bundles）平级按完整路径字典序兜底（确定性保持）。
        paths.sort_by_key(|p| rules_dir_load_order_key(p));

        // 批次F（P1-14 reload 防篡改）: bundle 条目 blake3 复验，
        // 失配条目 fail-fast 拒载（ERROR 不静默；拒载原因在收集阶段明示）
        let tampered = Self::collect_tampered_bundle_files(rules_dir);

        let mut parts: Vec<MergedPart> = Vec::new();

        let mut labels: Vec<String> = Vec::new();

        let mut rejected_gate_files: Vec<RejectedGateFileEntry> = Vec::new();

        for p in paths {
            if tampered.contains(&p) {
                continue; // 复验失配/manifest 非法——收集阶段已 ERROR 留痕
            }
            // R5/[CON-S-005]：返回部件（文档+部件 ctx）而非装载结果——装载与
            // 拼装统一在 load_rule_set_merged 函数内完成（禁调用方拼装）。
            // 标签与部件一一对应，规则条数展开由调用方按 parts[i].rule_count。
            match Self::parse_rule_file(&p, rules_dir) {
                Ok(Some((doc, ctx))) => {
                    // 来源标签 = 相对 rules_dir 的路径（/ 归一化，URL/标签安全）
                    let rel = p
                        .strip_prefix(rules_dir)
                        .unwrap_or(&p)
                        .to_string_lossy()
                        .replace('\\', "/");
                    labels.push(rel);
                    parts.push(MergedPart { doc, ctx });
                }
                Ok(None) => {}
                Err(rejected) => rejected_gate_files.push(rejected),
            }
        }

        (parts, labels, rejected_gate_files)
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

    /// 批次F（P1-14 reload 防篡改）: bundle 条目 blake3 复验收集。
    ///
    /// 扫描 `{rules_dir}/bundles/` 各 bundle 目录（带 `bundle_manifest.json`），
    /// 对 manifest 记录了 content_hash 的条目重算落盘文件哈希比对：
    /// - 哈希失配/文件缺失 → 拒载该条目（ERROR 不静默）；
    /// - manifest 在但不可读/非法 → 无法证明条目未篡改 → fail-closed 拒载该
    ///   目录全部条目（ERROR 不静默）；
    /// - 旧 manifest 未记录哈希的条目 → 跳过（防护不追溯存量，零迁移）；
    /// - 无 manifest 的目录（手工放置规则文件）→ 维持既有语义照常加载。
    ///
    /// 存量豁免清零 L2（2026-09-15）: 在条目复验前先做 manifest 落盘完整性比对
    /// （`landed_content_hash`，防"文件与 manifest 记录被同步篡改"绕过条目哈希）：
    /// - 记录值 ≠ 重算值 → manifest 不可信 → fail-closed 拒载该目录全部条目；
    /// - 旧 manifest 未记录该字段 → 跳过（零迁移，与条目哈希同策略）。
    ///
    /// 返回拒载文件路径集（装载清单与 tier_inventory 共用，保证视图一致）。
    fn collect_tampered_bundle_files(
        rules_dir: &std::path::Path,
    ) -> std::collections::BTreeSet<std::path::PathBuf> {
        use evorule_workspace::bundle_land::{
            verify_bundle_entry_hashes, verify_landed_hash_recorded, BundleManifest,
        };
        let mut out = std::collections::BTreeSet::new();
        let Ok(rd) = std::fs::read_dir(rules_dir.join("bundles")) else {
            return out;
        };
        for entry in rd.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') {
                continue; // 临时/备份目录本就不参与加载
            }
            let manifest_path = dir.join(crate::api::bundles::BUNDLE_MANIFEST_FILE);
            let refuse_whole_dir = |reason: String, out: &mut std::collections::BTreeSet<_>| {
                let mut all = Vec::new();
                Self::collect_json_files_recursive(&dir, &mut all);
                for f in all {
                    tracing::error!(
                        "bundle 条目拒载（批次F）: {} — manifest 复验不可用（{reason}）,\
                         fail-closed 拒载;恢复方式: 重新导入该 bundle 或回滚文件",
                        f.display()
                    );
                    out.insert(f);
                }
            };
            let raw = match std::fs::read_to_string(&manifest_path) {
                Ok(raw) => raw,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    continue; // 非 bundle 管理目录，保持既有语义
                }
                Err(e) => {
                    refuse_whole_dir(format!("manifest 读取失败: {e}"), &mut out);
                    continue;
                }
            };
            let manifest: BundleManifest = match serde_json::from_str(&raw) {
                Ok(m) => m,
                Err(e) => {
                    refuse_whole_dir(format!("manifest 解析失败: {e}"), &mut out);
                    continue;
                }
            };
            // L2 落盘完整性比对先于条目复验：manifest 自身不可信则条目哈希无意义
            if let Err(reason) = verify_landed_hash_recorded(&manifest) {
                refuse_whole_dir(reason, &mut out);
                continue;
            }
            for f in verify_bundle_entry_hashes(&dir, &manifest) {
                tracing::error!(
                    "bundle 条目拒载（批次F 复验失配）: {} — 落盘内容与导入时 \
                     blake3 哈希不一致（疑似篡改/半成品）,fail-fast 拒载该条目;\
                     恢复方式: 重新导入该 bundle 或回滚文件",
                    dir.join(&f).display()
                );
                out.insert(dir.join(f));
            }
        }
        out
    }

    /// 三层规则清单（层级可观测，设计方案 §2.1）
    ///
    /// 分层是**纯约定**（执行顺序由"core_eval 在前 + 完整路径字典序"保证，本函数不参与
    /// 加载路径，只做只读扫描）：
    /// - L1 宪法层：core_eval 文件（server_eval.json），仅用户特批可改
    /// - L2 约束层：rules_dir **根目录直置**的 `00_constraint_*.json`（新权威前缀）
    ///   或 `00_meta_*.json`（旧前缀兼容），仅治理链晋升可写
    ///   （子目录内约束前缀不算 L2——L3 补丁落点在 bundles/{id}/ 子目录，
    ///   防伪造层级；且字典序保证根目录 00_constraint_ 恒先于 bundles/ 执行）
    /// - L3 业务层：其余全部（根目录存量文件 + bundles/ 子目录条目）
    pub fn tier_inventory(
        core_eval_path: &std::path::Path,
        rules_dir: &std::path::Path,
    ) -> Vec<RuleTierEntry> {
        let mut tiers = Vec::new();

        // L1 宪法层（单文件）
        tiers.push(RuleTierEntry {
            tier: "L1_core_eval".to_string(),
            files: if core_eval_path.exists() { 1 } else { 0 },
            paths: vec![core_eval_path.to_string_lossy().to_string()],
        });

        // L2/L3 分类：根目录直置约束前缀（00_constraint_ 新 / 00_meta_ 旧兼容）= L2，其余（含子目录）= L3
        let (mut l2, mut l3): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
        let mut all: Vec<std::path::PathBuf> = Vec::new();
        Self::collect_json_files_recursive(rules_dir, &mut all);
        // 批次F: bundle 复验拒载的条目不计入清单（清单与实载一致的既有口径）
        let tampered = Self::collect_tampered_bundle_files(rules_dir);
        for p in all {
            if tampered.contains(&p) {
                continue; // 拒载证据见加载日志 ERROR（复验失配/manifest 非法）
            }
            let rel = p
                .strip_prefix(rules_dir)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/");
            // 清单只列「加载生效」的文件：未过装载门禁（四步门，与加载路径同一
            // 通道）的不计入（拒载证据见加载日志 warn / reload 出证），
            // 保证清单视图与 count（实际加载规则数）一致，运维不会被拒载文件误导
            let passes_gate = std::fs::read_to_string(&p)
                .ok()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                .map(|j| Self::rule_file_gate(&j, &p, rules_dir).is_ok())
                .unwrap_or(false);
            if !passes_gate {
                continue;
            }
            let is_root_meta = p.parent().map(|d| d == rules_dir).unwrap_or(false)
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .map(is_l2_meta_file_name)
                    .unwrap_or(false);
            if is_root_meta {
                l2.push(rel);
            } else {
                l3.push(rel);
            }
        }
        l2.sort();
        l3.sort();
        tiers.push(RuleTierEntry {
            tier: "L2_meta".to_string(),
            files: l2.len(),
            paths: l2,
        });
        tiers.push(RuleTierEntry {
            tier: "L3_business".to_string(),
            files: l3.len(),
            paths: l3,
        });
        tiers
    }

    /// L2 约束（元规则）只读清单投影（结构化，供 LLM 代理读取约束边界摘要）。
    ///
    /// 扫描口径与 [`Self::tier_inventory`] 的 L2 层单一权威同源：rules_dir **根目录直置**
    /// 的 `00_constraint_*.json`（新权威前缀）或 `00_meta_*.json`（旧前缀兼容）且过装载
    /// 门禁（[`Self::rule_file_gate`] 四步门，拒载文件不投影）。
    /// 输出为结构化投影——`metadata.title` + `metadata.guard_for` +
    /// 晋升账三字段 `metadata.promoted_from/promoted_at/promoted_by`
    /// （非晋升条目为 null），不包含 transform / enforce 等执行语义内容。
    ///
    /// fail-soft：rules_dir 不存在 / 单文件读取或解析失败 → 跳过该文件；
    /// 全部失败 → 空清单（count=0）。排序确定：按文件名排序。
    pub fn l2_inventory(rules_dir: &std::path::Path) -> L2InventoryResponse {
        let mut files: Vec<L2InventoryEntry> = Vec::new();
        let Ok(read_dir) = std::fs::read_dir(rules_dir) else {
            return L2InventoryResponse { count: 0, files };
        };
        for entry in read_dir.flatten() {
            let p = entry.path();
            if !p.is_file() {
                continue;
            }
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !(is_l2_meta_file_name(name) && name.ends_with(".json")) {
                continue;
            }
            // fail-soft：读取或解析失败 → 跳过该文件
            let json = std::fs::read_to_string(&p)
                .ok()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
            let Some(json) = json else {
                continue;
            };
            // 未过装载门禁（四步门）的不投影（清单只列加载生效文件，与 tier_inventory
            // 口径一致，同一通道判定）
            if Self::rule_file_gate(&json, &p, rules_dir).is_err() {
                continue;
            }
            let meta = json.get("metadata");
            let title = meta
                .and_then(|m| m.get("title"))
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            let guard_for = meta
                .and_then(|m| m.get("guard_for"))
                .and_then(|g| g.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let path = p
                .strip_prefix(rules_dir)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/");
            let meta_str = |key: &str| -> Option<String> {
                meta.and_then(|m| m.get(key))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            };
            files.push(L2InventoryEntry {
                path,
                title,
                guard_for,
                promoted_from: meta_str("promoted_from"),
                promoted_at: meta_str("promoted_at"),
                promoted_by: meta_str("promoted_by"),
            });
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        L2InventoryResponse {
            count: files.len(),
            files,
        }
    }

    /// 读取并解析单个业务规则文件，返回其 transform 数组。
    ///
    ///
    /// 文件读取/解析失败或格式不符时 warn 并返回 None（fail-soft，保证热重载可用性）。
    ///
    fn parse_rule_file(
        p: &std::path::Path,
        rules_dir: &std::path::Path,
    ) -> Result<Option<(serde_json::Value, LoadContext)>, RejectedGateFileEntry> {
        let json = Self::load_rule_doc(p).map_err(|reason| RejectedGateFileEntry {
            path: p
                .strip_prefix(rules_dir)
                .unwrap_or(p)
                .to_string_lossy()
                .replace('\\', "/"),
            gate: "read".into(),
            reason,
        })?;
        let ctx = Self::rule_file_gate(&json, p, rules_dir)?;
        Ok(Some((json, ctx)))
    }

    /// 单文件装载门（清单面与装载面共用的唯一通道）：L2 身份由治理事实判定
    /// （rules_dir 根目录 00_constraint_*/00_meta_* 前缀 = 约束文件；其余 = 业务
    /// 文件），tier 门（正向声明+反向伪造+enforce 限定）/schema 门/discipline 门/
    /// eq-lt domain 门四步同源走 rule_loader。清单视图的「加载生效」判定与
    /// 装载路径、reload 拒载出证（rejected_gate_files）同由此函数裁决——同一
    /// 文件三面同判，杜绝「清单里在、加载时不载」。
    ///
    /// 通过返回该文件的装载上下文；拒绝返回带门类标签的拒载明细。
    fn rule_file_gate(
        json: &serde_json::Value,
        p: &std::path::Path,
        rules_dir: &std::path::Path,
    ) -> Result<LoadContext, RejectedGateFileEntry> {
        let rel = p
            .strip_prefix(rules_dir)
            .unwrap_or(p)
            .to_string_lossy()
            .replace('\\', "/");
        let is_root_meta = p.parent().map(|d| d == rules_dir).unwrap_or(false)
            && p.file_name()
                .and_then(|n| n.to_str())
                .map(is_l2_meta_file_name)
                .unwrap_or(false);
        let path_str = p.display().to_string();
        let ctx = if is_root_meta {
            LoadContext::constraint_file(&path_str)
        } else {
            LoadContext::business_file(&path_str)
        };
        // V2 fail-soft 单文件预筛门（豁免保留）：同 ctx 走 load_rule_set 四步，
        // 与 merged 入口的部件门确定性等判——此处拒载的文件不进合并清单
        // （warn+跳过保目录可用性，fail-visible 明细上浮）；幸存部件的装载与
        // 拼装统一由 load_rule_set_merged 函数内完成（[CON-S-005] 禁调用方拼装）
        if let Err(e) = evorule_rule_loader::load_rule_set(json, &ctx) {
            // 门禁拒绝=治理纪律执行（B2 类旁路在此消解）；保留生产 loader 的
            // fail-soft 语义（warn+跳过该文件，其余文件照常装载）+ fail-visible
            //（拒载明细上浮至合并层与 reload 响应，与旧 schema gate 同可查面）
            tracing::warn!("规则文件 {} 装载门禁未过，已拒载: {}", p.display(), e);
            let gate = if matches!(&e, evorule_rule_loader::ConstitutionError::TierGate(_)) {
                "tier"
            } else if matches!(
                &e,
                evorule_rule_loader::ConstitutionError::DomainOnMissingGate(_)
            ) {
                "domain_on_missing"
            } else if matches!(
                &e,
                evorule_rule_loader::ConstitutionError::DisciplineGate(_)
            ) {
                "discipline"
            } else {
                "schema"
            };
            return Err(RejectedGateFileEntry {
                path: rel,
                gate: gate.into(),
                reason: e.to_string(),
            });
        }
        Ok(ctx)
    }

    /// 文档是否携带 `enforce` 元指令（transform 数组两种形态：`{transform:[]}` / 裸数组）
    fn contains_enforce_rule(json: &serde_json::Value) -> bool {
        json.get("transform")
            .and_then(|x| x.as_array())
            .or_else(|| json.as_array())
            .map(|a| {
                a.iter()
                    .any(|r| r.get("type").and_then(|t| t.as_str()) == Some("enforce"))
            })
            .unwrap_or(false)
    }

    /// 读取规则文件并解析为 JSON；失败时 warn 并返回 None
    fn load_rule_doc(p: &std::path::Path) -> Result<serde_json::Value, String> {
        let raw = match std::fs::read_to_string(p) {
            Ok(s) => s,

            Err(e) => {
                tracing::warn!("读取业务规则文件 {} 失败: {}，已跳过", p.display(), e);

                return Err(format!("读取失败: {e}"));
            }
        };

        match serde_json::from_str(&raw) {
            Ok(v) => Ok(v),

            Err(e) => {
                tracing::warn!("解析业务规则文件 {} 失败: {}，已跳过", p.display(), e);

                Err(format!("JSON 解析失败: {e}"))
            }
        }
    }

    // 旧手写提取桥（transform 数组直取/schema 判型/discipline 判型，无 _with 桥）/
    // check_discipline_for_core_eval（无 _with 桥）已随装载收口删除：
    // 四步装载（schema 校验→剥壳→三门→provenance）统一走 evorule_rule_loader::
    // load_rule_set。剩余 discipline/tier 门禁面仅测试用 → #[cfg(test)]，
    // 生产二进制零残留（模式单一事实源已随 LoadContext::discipline_enforce 迁入
    // rule_loader：env EVORULE_DISCIPLINE_GATE）。

    /// L2 纪律门禁模式开关：env `EVORULE_DISCIPLINE_GATE`（判定 SSOT =
    /// evorule_rule_loader::discipline_enforce_from，R7 fail-closed 对齐）。
    /// - 缺省/空串/`enforce`（大小写不敏感）= 违规拒载（业务文件 fail-soft /
    ///   宪法 fail-fast；W1 观测期豁免已于 2026-10-14 收口，SR-16）；
    /// - 显式 `warn` = warn 纯观测（一键降级回退口保留）；
    /// - 非法值 = panic 拒载（fail-closed，[CON-L2-004]/SR-13-R7：门禁配置错误
    ///   不得静默降级为观测态）。
    #[cfg(test)]
    fn discipline_gate_enforce() -> bool {
        evorule_rule_loader::discipline_enforce_from(
            std::env::var("EVORULE_DISCIPLINE_GATE").ok().as_deref(),
        )
    }

    /// 门禁模式横幅（每进程一次）：让运维在启动日志直接看到当前模式与切换方式
    #[cfg(test)]
    fn discipline_gate_banner() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let enforce = Self::discipline_gate_enforce();
            tracing::info!(
                target: "discipline_gate",
                enforce,
                env = "EVORULE_DISCIPLINE_GATE",
                "L2 纪律门禁已接线：DC-01..DC-09；判定 SSOT = \
                 evorule-discipline crate，纪律数据 SSOT = evorule-tcb/discipline/core_eval.json"
            );
            if !enforce {
                tracing::warn!(
                    target: "discipline_gate",
                    "当前为 warn 观测模式（显式 env，一键降级回退口）：违规文件照常装载 \
                     但以 ERROR 级大声曝光（若 enforce 将拒载）；缺省已收口为 enforce \
                     （W1 豁免 2026-10-14 收口）"
                );
            }
        });
    }

    /// 影子期违规文件数（门禁影子强制整改，2026-09-24）：装载完成后由 main 汇总曝光。
    /// 0 = 影子期干净（flip 安全前提）；> 0 = 存在 enforce 下将被拒载的文件。
    pub fn shadow_violation_files() -> usize {
        SHADOW_VIOLATION_FILES.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 违规明细结构化日志（两个接线点共用；字段口径 = 清零判据的观测源）。
    ///
    /// 影子强制：日志级别 warn→error——违规不再淹没在日常 warn 噪音里，
    /// 「若 enforce 将拒载」必须大到不可忽视；文件照常装载（行动面不变）。
    #[cfg(test)]
    fn log_discipline_violations(
        p: &std::path::Path,
        violations: &[evorule_discipline::Violation],
        source: &str,
    ) {
        for v in violations {
            tracing::error!(
                target: "discipline_gate",
                source,
                file = %p.display(),
                dc_code = %v.code,
                node_type = %v.node_type,
                node_path = %v.path,
                reason = %v.reason,
                "影子强制：未过 L2 纪律门禁（若 EVORULE_DISCIPLINE_GATE=enforce 该文件将被拒载）"
            );
        }
    }

    /// L2 纪律门禁（业务 loader 接线；影子强制整改 2026-09-24）。
    ///
    /// - **warn 模式（缺省，W1 影子强制）**：违规文件照常装载（行动面不变，
    ///   吸收门禁残余误报，防未登记合法规则集被误拒致系统不可用），但以
    ///   **ERROR 级**大声曝光（file/dc_code/node_type/node_path/reason +
    ///   「若 enforce 将拒载」文案），并累计影子违规文件数供装载后汇总——
    ///   暴露音量 = enforce，行动面 = warn；flip 条件 = 影子期违规归零；
    /// - **enforce 模式**：违规拒载（fail-soft，与 schema/tier 门禁同款），
    ///   一键降级 = env 切回 warn，无需发版；
    /// - 门禁自身求值失败：warn 期 fail-open（不拦装载），enforce 期 fail-closed
    ///   （「校验层缺位即不通过」）。
    #[cfg(test)]
    fn passes_discipline_gate_with(
        p: &std::path::Path,
        arr: &[serde_json::Value],
        enforce: bool,
    ) -> bool {
        Self::discipline_gate_banner();
        let tcb_rules: Vec<JsonValue> = arr.iter().cloned().map(serde_to_tcb).collect();
        match evorule_discipline::check(&tcb_rules) {
            Ok(report) if report.violations.is_empty() => true,
            Ok(report) => {
                Self::log_discipline_violations(p, &report.violations, "rule_file");
                if enforce {
                    tracing::warn!(
                        target: "discipline_gate",
                        file = %p.display(),
                        violations = report.violations.len(),
                        "enforce 模式：拒绝加载该规则文件"
                    );
                    false
                } else {
                    // 影子强制：文件照常装载，但计数供装载后汇总曝光
                    SHADOW_VIOLATION_FILES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    true
                }
            }
            Err(e) => {
                tracing::error!(
                    target: "discipline_gate",
                    file = %p.display(),
                    error = %e,
                    "纪律门禁求值失败"
                );
                !enforce
            }
        }
    }

    /// ①.5 测试证据引用校验。
    /// 与治理域 export 侧形状校验(evorule-rule export_with_tests)双闸同口径:
    ///   a. verdict=pass 必须携带可追溯标记(subset 非空且每项均为
    ///      sandbox:<id> 机器背书引用)——封死"零证据 pass"直 POST import
    ///      的伪造路径;人工背书 human:<actor> 形态已下线(自由文本 actor
    ///      可被任意伪造,构成无沙盒验证的 pass 证据面),不再接受。
    ///   b. sandbox:<id> 引用必须在本机 workspace 元数据可追溯:
    ///      不存在(伪造/跨环境) → 拒收;非 closed(测试未完成) → 拒收;
    ///      报告 summary.failed≠0(fail 报告不得作 pass 证据) → 拒收;
    ///      报告文件缺失/损坏 → 拒收(fail-closed:校验层缺位即不通过,不静默)。
    /// 跨环境信任(报告哈希/随包携带)登记为后续项——当前拒收符合
    /// "不让未经验证的信息通过"(设计文档 §6.1 阶段一)。
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
                .all(|s| s.starts_with("sandbox:"));
        if !traceable {
            return Err(
                "测试证据校验失败（不静默）: verdict=pass 的导入必须携带可追溯标记\
                 (tests.subset 每项须为 sandbox:<沙盒ID> 机器背书沙盒引用)。\
                 请从治理域测试工作台导出"
                    .to_string(),
            );
        }
        for ref_item in &bundle.tests.subset {
            let Some(sid_str) = ref_item.strip_prefix("sandbox:") else {
                continue; // 防御性分支:前缀闸已保证 sandbox: 前缀,不应到达
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
            let file_name = export_path.rsplit(['/', '\\']).next().unwrap_or_default();
            let report_path = evorule_workspace::sandbox_report_dir()
                .join(format!("report_{file_name}"))
                .to_string_lossy()
                .to_string();
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
            // 报告↔沙盒绑定校验:报告内嵌的草稿规则集指纹必须与沙盒会话记录
            // 一致——防「报告文件被另一个沙盒的报告顶替」(export_path 指向的
            // 文件名可被外部改写,此前对报告内容与该沙盒的从属关系零校验)。
            // 两侧都在场即必须相等;报告侧缺失视为结构异常拒收;沙盒记录侧
            // 缺失(极老数据)放行并留痕,不做追溯性拒收。
            let report_ruleset_hash = report
                .pointer("/draft_ruleset_hash")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let sandbox_ruleset_hash = sb.draft_ruleset_hash.clone();
            match (report_ruleset_hash, sandbox_ruleset_hash) {
                (Some(rp), Some(sbh)) if rp != sbh => {
                    return Err(format!(
                        "测试证据校验失败（不静默）: 沙盒 #{sid} 报告的规则集指纹与沙盒记录不符\
                         (报告={rp} 记录={sbh})——报告与沙盒错配,不得作为 pass 证据"
                    ));
                }
                (Some(_), None) => {
                    tracing::warn!(
                        sandbox_id = sid,
                        "sandbox record lacks draft_ruleset_hash (legacy data); \
                         report-to-sandbox binding check skipped"
                    );
                }
                (None, Some(_)) => {
                    return Err(format!(
                        "测试证据校验失败: 沙盒 #{sid} 报告缺少 draft_ruleset_hash(结构异常),\
                         报告无法锚定到沙盒记录,不得作为 pass 证据"
                    ));
                }
                _ => {}
            }

            // 内容对照（BND-D-03 演进收口）:沙盒记录携带 draft 规则快照时，
            // 逐条 bundle 条目的 rule_body 必须命中快照中的某条规则
            // （canonical JSON 同构比对，evorule_hash::json_digest 键序稳定）——
            // 把「沙盒测过这些规则」从 hash 绑定升级为内容对照。
            // 快照缺失（v9 前创建的存量沙盒）放行并留痕，零迁移。
            if let Some(draft_json) = sb.draft_rules_json.as_deref() {
                let draft: Vec<serde_json::Value> =
                    serde_json::from_str(draft_json).map_err(|e| {
                        format!("测试证据校验失败: 沙盒 #{sid} draft 规则快照损坏: {e}")
                    })?;
                let tested: std::collections::BTreeSet<String> =
                    draft.iter().map(evorule_hash::json_digest).collect();
                for entry in &bundle.entries {
                    let d = evorule_hash::json_digest(&entry.rule_body);
                    if !tested.contains(&d) {
                        return Err(format!(
                            "测试证据校验失败（不静默）: 条目 {} 的规则内容未被沙盒 #{sid} 的\
                             draft 规则集覆盖（内容对照失配）——沙盒所测规则集与包内条目不一致,\
                             不得作为 pass 证据",
                            entry.entry_id
                        ));
                    }
                }
            } else {
                tracing::warn!(
                    sandbox_id = sid,
                    "sandbox record lacks draft rules snapshot (legacy data); \
                     content-level coverage check skipped"
                );
            }
        }
        Ok(())
    }

    /// ③ 第 8 项执行侧服务绑定核对（阻断项 ①）：bundle 声明的服务必须已绑定，
    /// 缺失 → **显式失败**（不静默）。防"治理侧声明 / 执行侧未绑定 → 运行时
    /// unknown service_name"（历史批次 三层绑定：执行侧 service_registry 绑定）。
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
        let registry_services = self.registry_services_vec();
        let registry_names: HashSet<&str> =
            registry_services.iter().map(|m| m.name.as_str()).collect();
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

    /// 门禁拒载文件的日志留痕（import 响应契约未含该字段；运维可见与
    /// import 响应可见分开——reload 响应面已程序化携带，此处仅日志）
    fn log_gate_rejections(rejected: &[RejectedGateFileEntry]) {
        for g in rejected {
            tracing::warn!(
                path = %g.path,
                gate = %g.gate,
                "bundle 导入触发 reload: 规则文件被门禁拒载"
            );
        }
    }

    /// T2: 导入快照包（历史批次 集成契约）—— 6 项硬校验 → 逐条 Schema 门禁 → 服务绑定核对
    /// → I/O 权利面重叠前置预判 → 原子落盘 → 触发 reload。
    ///
    /// - 任一硬校验失败 → `Err`（显式报错，不静默跳过，T0/设计文档 §9）；
    /// - I/O 权利面重叠预判：bundle 规则条目与当前装载集（或同包更早条目）存在
    ///   发射/消费权利重叠 → 落盘前显式 `Err`（400），明细可归因到规则对——
    ///   与 reload 期防线同源同语义，reload 期防线保留作双保险；
    /// - `dry_run=true` 只跑校验链（6 项 + Schema 门禁 + 服务绑定核对 + 重叠预判），
    ///   不落盘不 reload；
    /// - 返回 [ImportOutcome]：运行配置 + reload 期被拒重叠条目（正常为空；
    ///   非空=预判与 reload 之间 rules_dir 并发变更的半激活事实，显式到达调用方）。
    pub async fn import_bundle(
        &self,
        bundle: &evorule_bundle::DatasetBundle,
        dry_run: bool,
    ) -> Result<ImportOutcome, String> {
        // ⓪-a 条目数上限（与编译链 compile-evopack 的 ≤64 口径对齐）：导入会把
        // 每个条目展开为规则文件并触发全量重载,无上限时一个 1MiB 包即可制造
        // 数千文件写入+重发放大。
        const MAX_IMPORT_ENTRIES: usize = 64;
        if bundle.entries.len() > MAX_IMPORT_ENTRIES {
            return Err(format!(
                "快照包条目数 {} 超过导入上限 {MAX_IMPORT_ENTRIES}（条目展开为规则文件                 并触发全量重载,超大批次请拆包导入）",
                bundle.entries.len()
            ));
        }

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

        // ②.5 enforce 层级门禁（回归验证）：bundle 恒为 L3（落 rules_dir/bundles/），
        // 携带 enforce 的规则导入期即硬拒收——不给"loader fail-soft 跳过"留口，
        // 治理语义：引擎级阻止权不随 bundle 分发，仅治理链晋升（meta_promotion）可达 L2。
        for entry in &bundle.entries {
            if entry.entry_kind == evorule_bundle::EntryKind::Knowledge {
                continue;
            }
            if Self::contains_enforce_rule(&entry.rule_body) {
                return Err(format!(
                    "条目 `{}` 携带 enforce 强制原语（enforce 仅允许 L2 元规则文件；\
                     bundle 恒为 L3，禁止随包分发引擎级阻止权，如需强制约束请走元规则晋升治理链）",
                    entry.entry_id
                ));
            }
        }

        // ③ 第 8 项执行侧服务绑定核对 —— 详见 validate_service_bindings 文档
        self.validate_service_bindings(bundle)?;

        // ③.5 I/O 权利面重叠前置预判（仅规则包；数据资产不进 TCB 无权利面）：
        // 命中 → 落盘前显式 Err（调用方 400），明细归因到规则对。
        // 基线排除：同 bundle_id 与同 dataset 已激活 bundle 的旧条目将被本次
        // 原子落盘替换（单激活换版），其权利声明不构成冲突。
        if has_rule {
            let mut exclude_prefixes = vec![format!("bundles/{}/", bundle.bundle_id)];
            for m in self.active_bundles()? {
                if m.dataset_id == result.dataset_id && m.bundle_id != bundle.bundle_id {
                    exclude_prefixes.push(format!("bundles/{}/", m.bundle_id));
                }
            }
            let loaded = self
                .core_eval
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let loaded_sources = self.hit_stats.current_sources();
            let entries: Vec<(String, JsonValue)> = bundle
                .entries
                .iter()
                .map(|e| (e.entry_id.clone(), serde_to_tcb(e.rule_body.clone())))
                .collect();
            let predicted = Self::predict_import_io_overlaps(
                &loaded,
                &loaded_sources,
                &bundle.bundle_id,
                &exclude_prefixes,
                &entries,
            );
            if !predicted.is_empty() {
                let detail = Self::format_import_io_rejection(&predicted);
                return Err(format!(
                    "快照包条目与当前装载规则集存在 I/O 权利面重叠（发射/消费权利首声明独占，\
                     导入前置预判；bundle 规则正道=使用自有 io_type，不得发射/消费平台保留 \
                     io_type）: {detail}",
                ));
            }

            // ③.6 domain 求值装载门禁前置预判（仅规则包）：缺 on_missing 声明 /
            // 声明非法值 / ValueLiteralAmbiguous 写作错误 → 落盘前显式 Err（调用方
            // 400）。缺声明为单条目绝对判定（不依赖装载基线），与 ③.5 账本比对预判
            // 不同源但同族（落盘前显式拒、替代半激活静默）。
            let predicted_domains =
                Self::predict_import_undeclared_domains(&bundle.bundle_id, &entries);
            if !predicted_domains.is_empty() {
                return Err(format!(
                    "快照包条目存在 domain 求值缺失策略未合规声明（eq/lt 域必须显式 \
                     on_missing=error/unsat；value 不得呈缺 __ 前缀的 exec 根段点路径形态；\
                     导入前置预判）: {}",
                    Self::format_import_domain_rejection(&predicted_domains),
                ));
            }
        }

        if dry_run {
            return Ok(ImportOutcome {
                result,
                rejected_overlaps: Vec::new(),
                rejected_domains: Vec::new(),
            });
        }

        // ④ 原子落盘（临时目录 → rename，失败清理无半成品；含 bundle_manifest.json）。
        // Q12 W1 分流：知识包落 `{knowledge_dir}/bundles/`（与 rules_dir 物理隔离，
        // TCB 加载路径天然不触碰数据文件）；规则包落 `rules_dir/bundles/`（原语义）。
        let mut reload_rejected: Vec<RejectedOverlapEntry> = Vec::new();
        let mut reload_rejected_domains: Vec<RejectedDomainEntry> = Vec::new();
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

            // ⑤b 触发既有 reload 链（新会话使用新规则；已存在会话不改 TCB 语义）。
            // 被拒重叠条目软结果透传到响应体（正常为空——③.5 前置预判已拦；
            // 非空=预判与 reload 之间 rules_dir 并发变更，半激活事实显式不静默）。
            // domain 门禁拒载同款透传（③.6 前置预判已拦，正常为空）。
            let (_, _, rejected, rejected_domains, rejected_gate_files) =
                self.reload_from_disk().await?;
            reload_rejected = rejected;
            reload_rejected_domains = rejected_domains;
            // 门禁拒载文件走日志面(ImportOutcome 契约未含该字段,加字段属
            // 契约演进;此处不静默——运维可见与 import 响应可见分开)
            Self::log_gate_rejections(&rejected_gate_files);
        }

        // ⑥ T5 审计溯源：bundle 导入历史写入 workspace 元数据库（bundle_imports 表）。
        // 管理元数据（imported_at 墙钟旁路），绝不渗入 fact / 内容哈希 / 审计验证链。
        // 写入失败 → 显式 Err（不静默掩盖审计缺失，设计文档 §9）；bundle 落盘已完成但溯源
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

        Ok(ImportOutcome {
            result,
            rejected_overlaps: reload_rejected,
            rejected_domains: reload_rejected_domains,
        })
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
    /// - 查询失败 → **显式 Err**（不静默，设计文档 §9）。
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
    /// 会话主体声明登记（workspace 面）：登记后命令入口注入 `__meta__.caller_role`。
    /// 声明值为内存态（与主会话声明同口径），serve 重启后随会话一同消失（fail-closed）。
    async fn declare_caller_role(
        &self,
        session_id: u64,
        role: &str,
    ) -> evorule_workspace::WorkspaceResult<()> {
        if let Ok(mut table) = self.caller_roles.lock() {
            table.insert(session_id, role.to_string());
            tracing::info!(
                session_id,
                "workspace 会话已声明 caller_role（命令入口将注入 __meta__）"
            );
            Ok(())
        } else {
            Err(evorule_workspace::WorkspaceError::invalid_input(
                "caller_role 登记失败（声明表不可用）",
            ))
        }
    }

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

                // spawn 回写转发任务（旗标缺省关）
                self.spawn_writeback_forwarder(id);

                if let Some(ref dispatcher) = self.dispatcher {
                    let sessions = self.sessions.lock().await;

                    if let Some(session) = sessions.get_session(id) {
                        let event_rx = session.event_tx.subscribe();

                        let command_tx = session.command_tx.clone();

                        // 注入会话级 caller_role resolver（应用层接缝；
                        // 声明随 WAL 事实，回放确定性成立）
                        let mut gate = PermissionGate::new(Arc::new(self.shared_facts.clone()))
                            .with_caller_role_resolver(
                                crate::api::caller_role_resolver::session_caller_role_resolver(
                                    session.facts_log.clone(),
                                ),
                            );
                        // SG-2：静态路由策略资产装配（可选收紧层；缺失/解析失败不装配 + 显式留痕）
                        if let Some(policy) =
                            crate::api::route_policy_loader::shared_route_policy(&self.rules_dir)
                        {
                            gate = gate.with_route_policy((*policy).clone());
                        }

                        let mut subscriber = IoSubscriber::new(dispatcher.clone())
                            .with_skip(Arc::new(is_external_executor_request))
                            .with_permission_gate(gate);
                        // SG-3：会话级熔断装配（可选收紧层；阈值 0 = 熔断面关闭 =
                        // 误伤回退开关。每会话独立 CircuitState，计数天然会话作用域）
                        if let Some(circuit_cfg) =
                            crate::api::route_policy_loader::circuit_config_from_env()
                        {
                            subscriber = subscriber.with_circuit_breaker(
                                evorule_governance::circuit_breaker::CircuitState::new(circuit_cfg),
                            );
                        }

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

                // spawn 回写转发任务（旗标缺省关）
                self.spawn_writeback_forwarder(id);

                Ok(id)
            }

            Err(evorule_governance_host::session::SessionError::NotFound { id }) => Err(
                evorule_workspace::WorkspaceError::not_found("session", id.to_string()),
            ),

            Err(evorule_governance_host::session::SessionError::LimitExceeded { current, max }) => {
                Err(evorule_workspace::WorkspaceError::internal(format!(
                    "session limit exceeded: {current}/{max}"
                )))
            }

            Err(e) => Err(evorule_workspace::WorkspaceError::internal(format!(
                "fork_session failed: {e:?}"
            ))),
        }
    }

    /// 从父会话 fork 沙盒会话并注入草稿规则集（SessionOps 桥接）
    ///
    /// 序列（同一把 sessions 锁内，无并发窗口）：
    /// 1. 权利面预检（fail-fast）：草稿规则 io_type 发射/消费与生产规则集及
    ///    草稿集内部独占冲突 → 拒启沙盒并明示冲突明细——避免冲突规则被静默
    ///    剥除后测试建立在不完整规则集上（静默通过禁令）；
    /// 2. 正统 API 挂载（governance 0.8.5+）：
    ///    `create_session_from_parent_at_version_with_core_eval` 直接为新会话
    ///    挂载专属规则集（core_eval_override）——全局规则集全程不变，生产
    ///    与其他会话不受影响。
    async fn fork_session_with_rules(
        &self,
        parent_session_id: u64,
        extra_rules: &[serde_json::Value],
        provenance: &evorule_workspace::DraftRulesProvenance,
    ) -> evorule_workspace::WorkspaceResult<u64> {
        use std::collections::{BTreeMap, BTreeSet};

        // R4/[CON-S-003]：治理指纹与规则数必须一一对应（fail-fast——错位即
        // provenance 失真，静默截断=静默通过禁令不容）
        if provenance.rule_version_ids.len() != extra_rules.len() {
            return Err(evorule_workspace::WorkspaceError::invalid_input(format!(
                "草稿治理指纹与规则数不一致（{} 条规则 vs {} 个版本 ID），拒绝启动沙盒",
                extra_rules.len(),
                provenance.rule_version_ids.len()
            )));
        }

        let sessions = self.sessions.lock().await;

        // 草稿规则转 TCB 形态（io_capability 与 Reactor 规则集均为 TCB JsonValue）
        //
        // ── 宪法不容替换（SR-03，P-1 合并语义）──
        // governance 的 core_eval_override 是**替换语义**（Some=挂载给定规则集
        // 而非全局规则集）。沙盒草稿不得顶掉宪法：override 必须=全局规则集
        // （含宪法前缀段）+ 过门草稿 的全集。草稿先经 rule_loader 三门
        // （tier 门拒沙盒草稿携 enforce=治理旁路；schema 门剥壳=A 缺陷消解），
        // 门禁不过 fail-fast 拒启（此时尚未落 db，无孤儿记录）。
        // 装载 ctx 携带真实治理指纹（R4）：逐条 ctx=自身版本 ID+草稿集哈希，
        // provenance 与 DB 治理链对齐（"draft#i"+空 hash 伪造已退役）。
        let global_ruleset = sessions.core_eval().to_vec();
        let global_len = global_ruleset.len();
        let mut converted: Vec<JsonValue> = Vec::with_capacity(global_len + extra_rules.len());
        converted.extend(global_ruleset);
        for (i, r) in extra_rules.iter().enumerate() {
            let ctx = LoadContext::sandbox(LoadSource::DbDraft {
                rule_version_ids: vec![provenance.rule_version_ids[i].clone()],
                ruleset_hash: provenance.ruleset_hash.clone(),
            });
            match evorule_rule_loader::load_rule_set(r, &ctx) {
                Ok(loaded) => converted.extend(loaded.engine_rules),
                Err(e) => {
                    return Err(evorule_workspace::WorkspaceError::invalid_input(format!(
                        "沙盒草稿规则 #{i} 装载门禁未过，拒绝启动沙盒: {e}"
                    )));
                }
            }
        }

        // 1. 权利面预检：生产规则集权利声明账本（io_type → 首声明方标签）
        let mut emit_claims: BTreeMap<String, String> = BTreeMap::new();
        let mut consume_claims: BTreeMap<String, String> = BTreeMap::new();
        for (idx, rule) in sessions.core_eval().iter().enumerate() {
            let (emit, consume): (BTreeSet<String>, BTreeSet<String>) = Self::io_capability(rule);
            for t in emit {
                emit_claims
                    .entry(t)
                    .or_insert_with(|| format!("core_eval#{idx}"));
            }
            for t in consume {
                consume_claims
                    .entry(t)
                    .or_insert_with(|| format!("core_eval#{idx}"));
            }
        }
        // 草稿逐条：先查冲突（对生产+草稿内部），再入账本。
        // converted 前缀=全局规则集（与上面账本同源），跳过只扫草稿段——
        // 否则宪法条目被当草稿重复记账=假阳性冲突。
        let mut conflicts: Vec<String> = Vec::new();
        for (idx, rule) in converted[global_len..].iter().enumerate() {
            let (emit, consume): (BTreeSet<String>, BTreeSet<String>) = Self::io_capability(rule);
            for t in &emit {
                if let Some(owner) = emit_claims.get(t) {
                    conflicts.push(format!("draft#{idx} emit io_type \"{t}\" 与 {owner} 冲突"));
                }
            }
            for t in &consume {
                if let Some(owner) = consume_claims.get(t) {
                    conflicts.push(format!(
                        "draft#{idx} consume io_type \"{t}\" 与 {owner} 冲突"
                    ));
                }
            }
            for t in emit {
                emit_claims
                    .entry(t)
                    .or_insert_with(|| format!("draft#{idx}"));
            }
            for t in consume {
                consume_claims
                    .entry(t)
                    .or_insert_with(|| format!("draft#{idx}"));
            }
        }
        if !conflicts.is_empty() {
            return Err(evorule_workspace::WorkspaceError::invalid_input(format!(
                "草稿规则权利面冲突，拒绝启动沙盒: {}",
                conflicts.join("; ")
            )));
        }

        // 2. 正统 API：fork 时挂载会话专属规则集（governance 0.8.5+），
        //    全局规则集不再经历 replace/restore，全程不变
        let result = sessions.create_session_from_parent_at_version_with_core_eval(
            parent_session_id,
            None,
            Some(converted),
        );

        match result {
            Ok(id) => {
                self.spawn_hit_stats_recorder(id);
                self.spawn_writeback_forwarder(id);
                Ok(id)
            }
            Err(evorule_governance_host::session::SessionError::NotFound { id }) => Err(
                evorule_workspace::WorkspaceError::not_found("session", id.to_string()),
            ),
            Err(evorule_governance_host::session::SessionError::LimitExceeded { current, max }) => {
                Err(evorule_workspace::WorkspaceError::internal(format!(
                    "session limit exceeded: {current}/{max}"
                )))
            }
            Err(e) => Err(evorule_workspace::WorkspaceError::internal(format!(
                "fork_session_with_rules failed: {e:?}"
            ))),
        }
    }

    async fn close_session(&self, session_id: u64) -> evorule_workspace::WorkspaceResult<()> {
        let result = {
            let sessions = self.sessions.lock().await;

            sessions.close_session(session_id)
        };

        match result {
            Ok(_) => {
                // 会话关闭，同步清理声明表
                self.remove_caller_role(session_id);

                Ok(())
            }

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

        // 声明会话注入 __meta__.caller_role（workspace 桥接路径，落账前注入；
        // TCB 引擎对根部额外键零消费）
        let mut command = command;
        self.inject_caller_role(session_id, &mut command);

        let instruction = serde_to_tcb(command);

        // 获取或惰性复活（内存表未命中且开关开时从 WAL 归档按原 sid 复活）
        let session = self.session_or_rehydrate(session_id).await.map_err(|rej| {
            tracing::warn!(session_id, rejection = %rej, "会话获取失败（含复活尝试）");
            if matches!(
                rej,
                RehydrateRejection::LimitExceeded { .. }
                    | RehydrateRejection::WalUnavailable { .. }
            ) {
                evorule_workspace::WorkspaceError::internal(rej.to_string())
            } else {
                evorule_workspace::WorkspaceError::not_found("session", session_id.to_string())
            }
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
        // 获取或惰性复活（内存表未命中且开关开时从 WAL 归档按原 sid 复活）
        let session = self.session_or_rehydrate(session_id).await.map_err(|rej| {
            tracing::warn!(session_id, rejection = %rej, "会话获取失败（含复活尝试）");
            if matches!(
                rej,
                RehydrateRejection::LimitExceeded { .. }
                    | RehydrateRejection::WalUnavailable { .. }
            ) {
                evorule_workspace::WorkspaceError::internal(rej.to_string())
            } else {
                evorule_workspace::WorkspaceError::not_found("session", session_id.to_string())
            }
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
        // 读侧惰性复活（o31 同族）：桥接消费面同样不直取内存表 404
        let session = self
            .session_or_rehydrate(session_id)
            .await
            .map_err(|rej| match rej {
                RehydrateRejection::NoArchive => {
                    evorule_workspace::WorkspaceError::not_found("session", session_id.to_string())
                }
                other => evorule_workspace::WorkspaceError::internal(format!(
                    "session rehydrate rejected: {other}"
                )),
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
        // 读侧惰性复活（o31 同族）：桥接消费面同样不直取内存表 404
        let session = self
            .session_or_rehydrate(session_id)
            .await
            .map_err(|rej| match rej {
                RehydrateRejection::NoArchive => {
                    evorule_workspace::WorkspaceError::not_found("session", session_id.to_string())
                }
                other => evorule_workspace::WorkspaceError::internal(format!(
                    "session rehydrate rejected: {other}"
                )),
            })?;

        // 兜底刷新: 确保 reactor 异步处理的 fact 已进 BLAKE3 链后再导出

        let _new_count = session.audit_new();

        Ok(session.audit_export().map_err(|e| {
            evorule_workspace::WorkspaceError::internal(format!("audit export failed: {e}"))
        })?)
    }

    /// 获取 Fact 列表 (用于测试报告统计)
    ///
    /// 返回会话事实日志的**完整 Fact JSON**（`{"type","id",...}` 变体字段，
    /// 与 `Fact::to_json` 序列化形态一致：Command 含 instruction、IoResponse 含
    /// result/error、Error 含 message、Violation 含 reason 等）。
    ///
    /// 判定语义归真（沙盒报告 P0 修复）：此前本方法返回审计链 entries
    /// （fact_type/fact_id 元数据键名），而消费方 TestReportBuilder 按
    /// "type"/"id" 读取 → fact_type 恒 unknown → 报告判定恒 Passed（假阳性
    /// 总根因）。改为直读 FactsLog 完整事实：键名与消费方契约对齐，且携带
    /// 判定所需的语义字段（指令类型/错误消息/IoResponse error）；审计链导出
    /// 面（get_audit_export，BLAKE3 元数据形态）不受影响。
    async fn get_facts(
        &self,

        session_id: u64,
    ) -> evorule_workspace::WorkspaceResult<Vec<serde_json::Value>> {
        // 读侧惰性复活（o31 同族）：桥接消费面同样不直取内存表 404
        let session = self
            .session_or_rehydrate(session_id)
            .await
            .map_err(|rej| match rej {
                RehydrateRejection::NoArchive => {
                    evorule_workspace::WorkspaceError::not_found("session", session_id.to_string())
                }
                other => evorule_workspace::WorkspaceError::internal(format!(
                    "session rehydrate rejected: {other}"
                )),
            })?;

        Ok(session
            .facts_log
            .history()
            .iter()
            .map(|f| tcb_to_serde(&f.to_json()))
            .collect())
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

        // 读侧惰性复活（o31 同族）：桥接消费面同样不直取内存表 404
        let session = self
            .session_or_rehydrate(session_id)
            .await
            .map_err(|rej| match rej {
                RehydrateRejection::NoArchive => {
                    evorule_workspace::WorkspaceError::not_found("session", session_id.to_string())
                }
                other => evorule_workspace::WorkspaceError::internal(format!(
                    "session rehydrate rejected: {other}"
                )),
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
        // 读侧惰性复活（o31 同族）：桥接消费面同样不直取内存表 404
        let session = self
            .session_or_rehydrate(session_id)
            .await
            .map_err(|rej| match rej {
                RehydrateRejection::NoArchive => {
                    evorule_workspace::WorkspaceError::not_found("session", session_id.to_string())
                }
                other => evorule_workspace::WorkspaceError::internal(format!(
                    "session rehydrate rejected: {other}"
                )),
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

    /// 应用级配额限流管理器(per-app 速率+日配额;
    /// 创建时从最近快照恢复日计数,快照后台任务由 main 装配后 spawn)
    app_quota: std::sync::Arc<crate::api::app_quota::AppQuotaManager>,
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
        // 配额管理器随 AppState 创建(从最近快照恢复日计数);
        // 快照后台任务在 main 装配完成后 spawn(需要 Arc,测试路径不 spawn)
        let app_quota = std::sync::Arc::new(crate::api::app_quota::AppQuotaManager::start_recover(
            &shared_facts,
        ));
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
            app_quota,
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

    /// 应用配额管理器(快照任务 spawn 用)
    pub fn app_quota(&self) -> std::sync::Arc<crate::api::app_quota::AppQuotaManager> {
        self.app_quota.clone()
    }
}

/// :演示登录入口开关的 axum 状态提取器（经 FromRef 从 AppState 派生）。
#[derive(Clone, Copy, Debug)]
pub struct DemoAuthFlag(pub bool);

impl FromRef<AppState> for std::sync::Arc<crate::api::app_quota::AppQuotaManager> {
    fn from_ref(state: &AppState) -> Self {
        state.app_quota.clone()
    }
}

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

    /// 稳定错误码（i18n 阶段2）：仅错误响应（success=false）填充，
    /// 前端据此本地化错误文案。成功响应为 None。
    pub code: Option<String>,
}

/// HTTP 状态码 → 稳定错误码映射（i18n 阶段2 续：裸 StatusCode 错误点 code 透传）。
///
/// 门禁脚本 `check_error_code_i18n.py` 扫描本函数体内的 `Some("...")` 字面量，
/// 与前端 err.* 字典对照；删除本函数或挪动字面量会使门禁报错（刻意设计）。
fn status_error_code(status: axum::http::StatusCode) -> Option<&'static str> {
    use axum::http::StatusCode;
    match status {
        StatusCode::BAD_REQUEST => Some("BAD_REQUEST"),
        StatusCode::UNAUTHORIZED => Some("UNAUTHORIZED"),
        StatusCode::FORBIDDEN => Some("FORBIDDEN"),
        StatusCode::NOT_FOUND => Some("NOT_FOUND"),
        StatusCode::METHOD_NOT_ALLOWED => Some("METHOD_NOT_ALLOWED"),
        StatusCode::CONFLICT => Some("CONFLICT"),
        StatusCode::PAYLOAD_TOO_LARGE => Some("PAYLOAD_TOO_LARGE"),
        StatusCode::TOO_MANY_REQUESTS => Some("RATE_LIMITED"),
        StatusCode::INTERNAL_SERVER_ERROR => Some("INTERNAL_ERROR"),
        StatusCode::BAD_GATEWAY => Some("BAD_GATEWAY"),
        StatusCode::SERVICE_UNAVAILABLE => Some("SERVICE_UNAVAILABLE"),
        _ => None,
    }
}

/// 裸 StatusCode 错误点 code 透传中间件（i18n 阶段2 续）。
///
/// 对「4xx/5xx 且无 Content-Type（即 handler 返回裸 StatusCode 的空体响应）」
/// 注入与 ApiResponse 同构的 JSON 体（success=false + code + message），
/// 使前端可按 code 本地化。已有 JSON 体的错误响应（ApiResponse/{error} 形态）
/// 与一切带 Content-Type 的响应（SSE/文件/静态资源）原样放行，不重复包装。
async fn inject_error_code_middleware(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let res = next.run(req).await;

    let status = res.status();
    if !(status.is_client_error() || status.is_server_error()) {
        return res;
    }
    if res.headers().contains_key(axum::http::header::CONTENT_TYPE) {
        return res;
    }
    let Some(code) = status_error_code(status) else {
        return res;
    };

    let (mut parts, body) = res.into_parts();
    let bytes = match axum::body::to_bytes(body, MAX_REQUEST_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => return axum::response::Response::from_parts(parts, axum::body::Body::empty()),
    };
    if !bytes.is_empty() {
        // 无 Content-Type 但非空体（罕见）：不覆写，原样回填
        return axum::response::Response::from_parts(parts, axum::body::Body::from(bytes));
    }

    // 保留原响应状态码与头（WWW-Authenticate 等），仅替换体为 ApiResponse 同构 JSON；
    // 注意不能走 Json::into_response()（默认 200，会吞掉原状态码）
    let payload = match serde_json::to_vec(&ApiResponse {
        success: false,
        message: status.canonical_reason().unwrap_or("Error").to_string(),
        fact_id: None,
        code: Some(code.to_string()),
    }) {
        Ok(v) => v,
        Err(_) => return axum::response::Response::from_parts(parts, axum::body::Body::empty()),
    };
    parts.headers.remove(axum::http::header::CONTENT_TYPE);
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    parts.headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::header::HeaderValue::from_static("application/json"),
    );
    axum::response::Response::from_parts(parts, axum::body::Body::from(payload))
}

/// 插件健康快照（启动时由 main 注入；未注入 = 未配置清单,原生插件缺省全启用）
static PLUGIN_HEALTH: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();

/// 启动期注入插件健康快照（main.rs 在插件清单校验通过后调用一次）
pub fn set_plugin_health(v: serde_json::Value) {
    let _ = PLUGIN_HEALTH.set(v);
}

/// 入口守卫（PermissionGate）装配状态信号（DEV-5：对应 [ASSURANCE.md](ASSURANCE.md) §3.6 条款 5 的可观测义务）。
///
/// server 启动并完成**全部** I/O 分发路径的 `with_permission_gate` 注入后，由 main 调用
/// [`mark_guard_assembled`] 置 `true`；外部可通过 `GET /api/health` 的 `guard_assembled`
/// 字段区分"守卫已装配 / 未装配"——避免 fail-open 静默失效不可被外部发现。
static GUARD_ASSEMBLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 标记入口守卫已装配（main 在完成全部分发路径注入后调用一次）
pub fn mark_guard_assembled() {
    GUARD_ASSEMBLED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// 读取入口守卫装配状态（供 `GET /api/health` 呈现）
pub fn is_guard_assembled() -> bool {
    GUARD_ASSEMBLED.load(std::sync::atomic::Ordering::SeqCst)
}

/// 部署档位健康快照（P0-1 生产就绪批：main 启动期写入一次，供 `GET /api/health` 呈现）
///
/// 扩 guard_assembled 进程级静态先例：部署巡检与告警据此核验「profile / 持久化 /
/// 认证 / 限速」布尔位与实际配置一致——带病运行（如开发档放行的裸奔配置）可被
/// 外部发现，而非只在启动日志里可见一次。OnceLock 写一次不可变（启动后配置面
/// 不变的语义与插件挂载快照同构）。
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
pub struct DeploymentHealth {
    /// 启动档位：`production`（缺省语义）/ `development`（显式豁免）
    pub profile: String,
    /// 档位是否来自缺省（未显式声明 --profile/EVORULE_PROFILE）——
    /// `true` = 当前 production 语义来自缺省反转（漏配即拒启已武装）；
    /// `false` = 运维显式声明了档位（含显式 production，配置自证）
    pub profile_defaulted: bool,
    /// WAL 持久化是否配置（wal_dir 三通道任一命中）
    pub wal_dir_configured: bool,
    /// 认证是否启用（auth_token 或 service_token 任一在场）
    pub auth_enabled: bool,
    /// 速率限制是否启用（--no-rate-limit 时为 false）
    pub rate_limit_enabled: bool,
}

static DEPLOYMENT_HEALTH: std::sync::OnceLock<DeploymentHealth> = std::sync::OnceLock::new();

/// 写入部署档位健康快照（main 启动期调用一次；重复调用忽略——OnceLock 语义）
pub fn set_deployment_health(snapshot: DeploymentHealth) {
    let _ = DEPLOYMENT_HEALTH.set(snapshot);
}

/// 读取部署档位健康快照（未注入时 None——`/api/health` 该节整体省略，向后兼容）
pub fn deployment_health() -> Option<DeploymentHealth> {
    DEPLOYMENT_HEALTH.get().cloned()
}

// ===== 启动期 WAL 全链校验健康位（P1 W-5）=====
/// 校验状态：跳过（flag 未开，或 wal_dir 未配置无可校验对象）
pub const WAL_VERIFY_SKIPPED: u8 = 0;
/// 校验状态：通过
pub const WAL_VERIFY_OK: u8 = 1;
/// 校验状态：失败（现场保留不拒启，仅经 /api/health 告警——取证保护语义）
pub const WAL_VERIFY_FAILED: u8 = 2;

static WAL_VERIFY_REACTOR_STATUS: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(WAL_VERIFY_SKIPPED);
static WAL_VERIFY_SHARED_FACTS_STATUS: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(WAL_VERIFY_SKIPPED);

/// 写入反应器审计链启动校验状态（main 启动期调用）
pub fn set_wal_verify_reactor(status: u8) {
    WAL_VERIFY_REACTOR_STATUS.store(status, std::sync::atomic::Ordering::Relaxed);
}

/// 写入共享事实账本启动校验状态（main 启动期调用）
pub fn set_wal_verify_shared_facts(status: u8) {
    WAL_VERIFY_SHARED_FACTS_STATUS.store(status, std::sync::atomic::Ordering::Relaxed);
}

/// 校验状态码 → 展示字符串（未知值按 skipped 处理——不猜不造）
fn wal_verify_status_str(status: u8) -> &'static str {
    match status {
        WAL_VERIFY_OK => "ok",
        WAL_VERIFY_FAILED => "failed",
        _ => "skipped",
    }
}

/// 启动期 WAL 全链校验状态（P1 W-5；两通路各一位）
#[derive(Debug, Serialize, ToSchema, PartialEq)]
pub struct WalVerifyHealth {
    /// 反应器审计链校验：`skipped` / `ok` / `failed`
    pub reactor: String,

    /// 共享事实账本校验：`skipped` / `ok` / `failed`
    pub shared_facts: String,
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
        m.insert(
            "status".into(),
            serde_json::Value::String(entry.status.clone()),
        );
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

    /// 入口守卫（PermissionGate）装配状态（C6 装配可观测信号，见 ASSURANCE.md §3.6 条款 5）
    ///
    /// `true` = 全部 I/O 分发路径已注入守卫，入口仲裁生效（fail-closed）；
    /// `false` = 服务启动未启用守卫，I/O 走 fail-open（应由运维纠正）。
    pub guard_assembled: bool,

    /// 部署档位快照（P0-1 生产就绪批：profile/持久化/认证/限速布尔位）
    ///
    /// main 启动期注入；未注入（理论上仅测试构造路径）时整节省略——响应向后兼容。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployment: Option<DeploymentHealth>,

    /// 回写转发漏发计数（观察面，先记后治）
    ///
    /// `EVORULE_WRITEBACK_URL` 未配置（转发未启用）时整节省略——响应与
    /// 未部署回写时逐字节兼容。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub writeback: Option<WritebackHealth>,

    /// WAL 观察面（P1 W-3；wal_dir 未配置 → 整节省略，响应向后兼容）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wal: Option<WalHealth>,
}

/// 回写转发漏发计数快照（进程级累计；多会话转发器共享）
#[derive(Debug, Serialize, ToSchema)]
pub struct WritebackHealth {
    /// 转发发送失败累计（收件端点非 2xx / 传输错误；不重试不补发，仅可见性登记）
    pub misfires: u64,

    /// 事件流落后丢帧累计（broadcast Lagged）
    pub dropped: u64,
}

/// WAL 观察面 health 节（P1 W-3；wal_dir 未配置/观察面未注册 → 整节省略，向后兼容）
#[derive(Debug, Serialize, ToSchema)]
pub struct WalHealth {
    /// checkpoint 累计统计
    pub checkpoint: WalCheckpointHealth,

    /// 后台轻采样存储快照（尚无采样时省略）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage: Option<WalStorageHealth>,

    /// 启动期全链校验状态（P1 W-5）
    pub verify: WalVerifyHealth,
}

/// checkpoint 累计统计（全通道 checkpoint，含 partial failure 计数）
#[derive(Debug, Serialize, ToSchema)]
pub struct WalCheckpointHealth {
    /// 执行总次数
    pub total: u64,

    /// 存在通路轮换失败的次数
    pub failed: u64,

    /// 最近一次全成功 checkpoint 墙钟（Unix 毫秒；从未成功省略）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_success_at_ms: Option<u64>,
}

/// WAL 存储轻采样快照（文件大小有界标签聚合 + 分区剩余空间）
#[derive(Debug, Serialize, ToSchema)]
pub struct WalStorageHealth {
    /// wal_dir 下 WAL 文件总数（*.wal 及分片）
    pub files: u64,

    /// WAL 文件总字节数
    pub total_bytes: u64,

    /// 分区剩余字节数（获取失败省略——不猜，不造数）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub free_bytes: Option<u64>,

    /// 本次采样墙钟（Unix 毫秒）
    pub sampled_at_ms: u64,
}

impl From<crate::wal_observation::WalCheckpointStats> for WalCheckpointHealth {
    fn from(s: crate::wal_observation::WalCheckpointStats) -> Self {
        Self {
            total: s.total,
            failed: s.failed,
            last_success_at_ms: s.last_success_at_ms,
        }
    }
}

impl From<crate::wal_observation::WalStorageSnapshot> for WalStorageHealth {
    fn from(s: crate::wal_observation::WalStorageSnapshot) -> Self {
        Self {
            files: s.files,
            total_bytes: s.total_bytes,
            free_bytes: s.free_bytes,
            sampled_at_ms: s.sampled_at_ms,
        }
    }
}

/// PayloadUpdate 请求体

#[derive(Debug, Deserialize, ToSchema)]

pub struct PayloadUpdateRequest {
    /// 字段路径
    pub path: String,

    /// 字段值
    pub value: serde_json::Value,
}

/// 批量 PayloadUpdate 请求（跨仓挂账二：一次 HTTP 写入多条 payload）
#[derive(Debug, Deserialize, ToSchema)]

pub struct BatchPayloadUpdateRequest {
    /// 逐条更新（path + value）
    pub updates: Vec<PayloadUpdateRequest>,
}

/// 批量 PayloadUpdate 逐条结果
#[derive(Debug, Serialize, ToSchema)]

pub struct BatchPayloadItemResult {
    /// 字段路径
    pub path: String,

    /// 本条是否成功
    pub success: bool,

    /// 会话侧事实 ID（成功时返回）
    pub fact_id: Option<u64>,

    /// 失败原因（成功时为 null）
    pub error: Option<String>,
}

/// 批量 PayloadUpdate 响应
#[derive(Debug, Serialize, ToSchema)]

pub struct BatchPayloadResponse {
    /// 全部成功才为 true
    pub success: bool,

    /// 汇总说明
    pub message: String,

    /// 总条数
    pub total: usize,

    /// 成功条数
    pub succeeded: usize,

    /// 失败条数
    pub failed: usize,

    /// 逐条结果
    pub results: Vec<BatchPayloadItemResult>,
}

// ===== OpenAPI 响应 schema（单一真相源，P2）=====

// 以下类型仅用于 `#[utoipa::path]` 标注中精确描述各端点的响应结构。

// 动态载荷（payload / queue / fact 等）保留为 `serde_json::Value`，

// 由 description 说明其结构；固定结构字段一律强类型化。

/// 会话创建请求体
///
/// 可选 body：缺省（无 body / 空 object）= 不声明，行为与既有会话完全一致
/// （io 判定 Unknown → 默认策略 Deny，fail-closed 零回归）。
#[derive(Debug, Default, Deserialize, ToSchema)]

pub struct CreateSessionRequest {
    /// 调用者角色声明（可选）
    ///
    /// 声明后：该会话内每条命令的 instruction 根部注入 `__meta__.caller_role`
    /// 随 WAL 落账，I/O 判定权限门按此角色解析主体匹配与默认策略。
    /// 仅接受 "human" | "llm"；其余值 400 拒绝（fail-closed，不静默降级）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caller_role: Option<String>,
}

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
/// 对应 evorule_governance_host::auditor::AuditEntry，
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

    /// 会话侧源头 fact_id（N6 链路统一，R10；旧数据/旧签名写入为 null）
    pub origin_fact_id: Option<u64>,
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

// serde_to_tcb 已收编至 evorule_rule_loader::serde_to_tcb（唯一实现，
// 顶部 use 引入；本文件旧手写版本已删除——禁止再手写第二条转换实现）

/// 将 evorule_tcb::JsonValue 转换为 serde_json::Value
pub(crate) fn tcb_to_serde(v: &JsonValue) -> serde_json::Value {
    match v {
        JsonValue::Null => serde_json::Value::Null,

        JsonValue::Bool(b) => serde_json::Value::Bool(*b),

        JsonValue::Integer(i) => serde_json::Value::Number((*i).into()),

        // u64 全域通道（v0.9.0）：serde Number 原生无损承载，不经 i64 cast
        JsonValue::U64(v) => serde_json::Value::Number((*v).into()),

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

/// H12 双通道衔接：把已落盘 bundle 条目同步注册为 workspace 规则（沙盒可测）
/// - bundle 经治理链导入后落在 `{rules_dir}/bundles/{bundle_id}/`（条目 JSON + manifest）；
/// - 本端点把这些条目注册为 workspace 规则（state=Draft，rule_version v1），
///   使沙盒（rule_version_ids 引用）可覆盖治理链导入的规则——双通道正式衔接。
/// - 幂等：同名规则（`{bundle_id}:{entry_id}`）已存在则跳过。
#[derive(Debug, serde::Deserialize)]
pub struct BundleSyncRequest {
    pub bundle_id: String,
    pub created_by: String,
}

#[derive(Debug, serde::Serialize)]
pub struct SyncedRuleInfo {
    pub entry_id: String,
    pub rule_id: String,
    pub rule_version_id: String,
}

#[derive(Debug, serde::Serialize)]
pub struct BundleSyncResponse {
    pub bundle_id: String,
    pub synced: Vec<SyncedRuleInfo>,
    pub skipped: Vec<String>,
}

pub async fn bundle_sync_handler(
    State(state): State<AppState>,
    Path(workspace_id): Path<String>,
    Json(req): Json<BundleSyncRequest>,
) -> Result<Json<BundleSyncResponse>, (StatusCode, Json<serde_json::Value>)> {
    use std::collections::HashSet;

    let bundle_dir = state
        .sessions
        .rules_dir
        .join("bundles")
        .join(&req.bundle_id);
    let manifest_path = bundle_dir.join(evorule_bundle::BUNDLE_MANIFEST_FILE);
    let manifest: evorule_workspace::BundleManifest = serde_json::from_slice(
        &std::fs::read(&manifest_path).map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": format!("bundle manifest 读取失败（bundle_id={}）: {e}", req.bundle_id),
                    "synced": false
                })),
            )
        })?,
    )
    .map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!("bundle manifest 解析失败: {e}"), "synced": false})),
        )
    })?;

    // 幂等查重：同名规则已存在 → 跳过
    let existing = state
        .workspace
        .rule_meta_service
        .list_rules(&workspace_id)
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(
                    serde_json::json!({"error": format!("list_rules 失败: {e}"), "synced": false}),
                ),
            )
        })?;
    let existing_names: HashSet<String> = existing.iter().map(|r| r.name.clone()).collect();

    let mut synced = Vec::new();
    let mut skipped = Vec::new();
    for ef in &manifest.entry_files {
        let name = format!("{}:{}", req.bundle_id, ef.entry_id);
        if existing_names.contains(&name) {
            skipped.push(ef.entry_id.clone());
            continue;
        }
        let content = std::fs::read_to_string(bundle_dir.join(&ef.file)).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("条目 {} 读取失败: {e}", ef.entry_id), "synced": false})),
            )
        })?;
        let rule = state
            .workspace
            .rule_meta_service
            .create_rule(
                &workspace_id,
                evorule_workspace::CreateRuleRequest {
                    name: name.clone(),
                    content,
                    created_by: req.created_by.clone(),
                    description: Some(format!("H12 bundle-sync: {}", req.bundle_id)),
                },
            )
            .await
            .map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": format!("规则 {} 创建失败: {e}", ef.entry_id), "synced": false})),
                )
            })?;
        synced.push(SyncedRuleInfo {
            entry_id: ef.entry_id.clone(),
            rule_id: rule.id.clone(),
            rule_version_id: rule.current_version_id.clone().unwrap_or_default(),
        });
    }
    Ok(Json(BundleSyncResponse {
        bundle_id: req.bundle_id,
        synced,
        skipped,
    }))
}

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
                serde_json::Value::Array(rule_hits.iter().map(trace_hit_to_sse_value).collect()),
            );
        }

        // enforce 强制拦截事实（回归验证，记录性事实，不推进版本）
        Fact::Violation {
            id,
            cause,
            rule_index,
            reason,
            instruction,
        } => {
            obj.insert("type".into(), serde_json::Value::String("Violation".into()));

            obj.insert("id".into(), serde_json::Value::Number(id.0.into()));

            obj.insert("cause".into(), serde_json::Value::Number(cause.0.into()));

            obj.insert(
                "rule_index".into(),
                serde_json::Value::Number((*rule_index).into()),
            );

            obj.insert("reason".into(), serde_json::Value::String(reason.clone()));

            obj.insert("instruction".into(), tcb_to_serde(instruction));
        }
    }

    serde_json::Value::Object(obj).to_string()
}

/// TraceHit → SSE JSON 值（TransitionTrace rule_hits 数组元素）。
/// domain_attr 仅在 Some 时透出（None 省略）：既有消费方无感；branch/enforce
/// 的三态域归因（sat/unsat/missing + missing_reason）与 on_missing 声明值
/// 随事实面透传（专项-20261001）。
fn trace_hit_to_sse_value(h: &TraceHit) -> serde_json::Value {
    let mut hit_obj = serde_json::Map::new();
    hit_obj.insert("index".into(), serde_json::json!(h.index));
    hit_obj.insert(
        "instr_type".into(),
        serde_json::Value::String(h.instr_type.clone()),
    );
    hit_obj.insert("hit".into(), serde_json::Value::Bool(h.hit));
    if let Some(attr) = &h.domain_attr {
        let mut attr_obj = serde_json::Map::new();
        attr_obj.insert(
            "outcome".into(),
            serde_json::Value::String(attr.outcome.clone()),
        );
        if let Some(reason) = &attr.missing_reason {
            attr_obj.insert(
                "missing_reason".into(),
                serde_json::Value::String(reason.clone()),
            );
        }
        if let Some(policy) = &attr.on_missing {
            attr_obj.insert(
                "on_missing".into(),
                serde_json::Value::String(policy.clone()),
            );
        }
        hit_obj.insert("domain_attr".into(), serde_json::Value::Object(attr_obj));
    }
    serde_json::Value::Object(hit_obj)
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
        // C6 装配可观测信号（DEV-5）：外部据此区分守卫已装配 / 未装配
        guard_assembled: is_guard_assembled(),
        // P0-1 部署档位快照（生产就绪批）：profile/持久化/认证/限速布尔位
        deployment: deployment_health(),
        // 回写转发漏发计数（未配置回写 → 省略该节，响应逐字节兼容）
        writeback: crate::api::writeback_forward::health_snapshot()
            .map(|(misfires, dropped)| WritebackHealth { misfires, dropped }),
        // WAL 观察面（P1 W-3）：wal_dir 配置且观察面已注册才呈现（响应向后兼容）
        wal: wal_health_snapshot(),
    })
}

/// WAL 观察面 health 节组装（纯函数；未配置 wal_dir / 观察面未注册 → None）
fn wal_health_snapshot() -> Option<WalHealth> {
    let deployment = deployment_health()?;
    if !deployment.wal_dir_configured {
        return None;
    }
    let obs = crate::wal_observation::WalObservation::global()?;
    Some(WalHealth {
        checkpoint: obs.checkpoint_stats().into(),
        storage: obs.storage_snapshot().map(Into::into),
        verify: WalVerifyHealth {
            reactor: wal_verify_status_str(
                WAL_VERIFY_REACTOR_STATUS.load(std::sync::atomic::Ordering::Relaxed),
            )
            .to_string(),
            shared_facts: wal_verify_status_str(
                WAL_VERIFY_SHARED_FACTS_STATUS.load(std::sync::atomic::Ordering::Relaxed),
            )
            .to_string(),
        },
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

        code: None,
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

            code: None,
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

            code: Some("SCHEMA_GATE_FAILED".to_string()),
        }));
    }

    let instruction = serde_to_tcb(instruction_value);
    match api.send_command(instruction) {
        Ok(id) => Ok(Json(ApiResponse {
            success: true,

            message: "Command submitted".to_string(),

            fact_id: Some(id.0),

            code: None,
        })),

        Err(msg) => Ok(Json(ApiResponse {
            success: false,

            message: msg,

            fact_id: None,

            code: Some("SESSION_ERROR".to_string()),
        })),
    }
}

/// 受保护域写入准入判定（三条 payload 写入路径共用）。
///
/// - 守卫武装（认证禁用但服务端配置了 service token，中间件已注入
///   武装标记）：仅 Service 身份放行，无凭证/无效凭证一律拒绝；
/// - 守卫未武装：User/App 拒绝；None（认证禁用且未配置 service token）
///   放行——零配置开发模式语义不变。
fn protected_write_denied(
    identity: &Option<Extension<CallerIdentity>>,
    gate: &Option<Extension<ServiceGateArmed>>,
) -> bool {
    if gate.is_some() {
        !matches!(identity, Some(Extension(CallerIdentity::Service)))
    } else {
        matches!(
            identity,
            Some(Extension(CallerIdentity::User)) | Some(Extension(CallerIdentity::App))
        )
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

    gate: Option<Extension<ServiceGateArmed>>,

    Json(req): Json<PayloadUpdateRequest>,
) -> Result<(StatusCode, Json<ApiResponse>), StatusCode> {
    // 受保护域准入——`shared.*.stable.llm.*` / `stable.system.*` 仅 service 身份可写。
    // 认证启用：身份由中间件注入（User/Service/App），User/App 拒绝（外部
    // 应用与普通用户均非受信服务管道）。
    // 认证禁用但配置了 service token：守卫独立武装（中间件注入武装标记），
    // 须携带有效 service token，无凭证/无效凭证一律拒绝——守卫与全局认证
    // 开关解耦，认证禁用的显式豁免只覆盖非受保护域。
    // 认证禁用且未配置 service token：放行（零配置开发模式语义不变）。
    if requires_service_identity(&req.path) && protected_write_denied(&identity, &gate) {
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

                code: Some("UNAUTHORIZED".to_string()),
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

                code: None,
            }),
        )),

        Err(msg) => Ok((
            StatusCode::OK,
            Json(ApiResponse {
                success: false,

                message: msg,

                fact_id: None,

                code: Some("SESSION_ERROR".to_string()),
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

    request_body = Option<CreateSessionRequest>,

    responses(

        (status = 200, description = "会话创建成功，返回 session_id", body = SessionIdResponse),

        (status = 400, description = "caller_role 声明非法（仅接受 human | llm）"),

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

    body: Option<Json<CreateSessionRequest>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // 可选 caller_role 声明（fail-closed——非法值 400 显式拒绝，不静默降级 Unknown）
    let declared_role: Option<String> = match body {
        Some(Json(req)) => match req.caller_role.as_deref() {
            None | Some("") => None,
            Some(r @ ("human" | "llm")) => Some(r.to_string()),
            Some(other) => {
                tracing::warn!(caller_role = %other, "会话创建被拒：caller_role 非法");
                return Err(StatusCode::BAD_REQUEST);
            }
        },
        None => None,
    };

    let result = {
        let sessions = api.sessions.lock().await;

        sessions.create_session()
    };

    match result {
        Ok(id) => {
            metrics.inc_sessions(); // 会话数 +1

            // 声明登记（创建成功才登记；失败路径无会话可声明）
            if let Some(role) = declared_role {
                if let Ok(mut table) = api.caller_roles.lock() {
                    table.insert(id, role);
                }
                tracing::info!(
                    session_id = id,
                    "会话已声明 caller_role（命令入口将注入 __meta__）"
                );
            }

            // spawn hit-stats 归因记录任务
            api.spawn_hit_stats_recorder(id);

            // spawn 回写转发任务（旗标缺省关）
            api.spawn_writeback_forwarder(id);

            // 为新 session 的 reactor spawn IoSubscriber（公共构造段）
            // 没有 IoSubscriber 时，session 的 IoRequest 会 60s 超时
            // （声明登记已在上方完成，此处不重复继承）
            api.spawn_io_subscriber_for_session(id, metrics.clone(), None)
                .await;

            Ok(Json(serde_json::json!({

                "session_id": id,

                "message": "Session created"

            })))
        }

        Err(evorule_governance_host::session::SessionError::LimitExceeded { current, max }) => {
            tracing::warn!(current, max, "Session creation rejected: limit exceeded");

            Err(StatusCode::TOO_MANY_REQUESTS)
        }

        Err(evorule_governance_host::session::SessionError::WalUnavailable {
            session_id,
            source,
        }) => {
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
    // 读侧惰性复活（o31 同族）：内存未命中且归档在即按原 sid 复活，不直取 404
    let session = api.session_or_rehydrate(session_id).await.map_err(|rej| {
        tracing::warn!(session_id, rejection = %rej, "会话读侧获取失败（含复活尝试）");
        rej.to_status()
    })?;

    Ok(Json(SessionMetadataResponse {
        session_id,

        parent_session_id: session.parent_session_id(),

        initial_content_hash: session.initial_content_hash().map(String::from),

        content_hash_scheme: match session.content_hash_scheme() {
            evorule_governance_host::session::ContentHashScheme::AuditChainCompact => {
                Some("audit_chain_compact")
            }
            evorule_governance_host::session::ContentHashScheme::TcbDisplay => Some("tcb_display"),
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

        (status = 404, description = "父会话不存在（内存与归档均无）"),

        (status = 429, description = "超过最大会话数"),

        (status = 400, description = "版本无效"),

        (status = 500, description = "归档 fork 时审计链完整性校验失败（fail-closed）"),

        (status = 503, description = "新会话 WAL 不可用（审计链无法建立）")

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

    // fork-from-archive：内存父会话不存在时，从 wal_dir 归档链兜底重建
    //（读 WAL → 全链校验重放 → create_session_from_initial_state）。
    // 归档也不存在（NoArchive）则回落到既有 NotFound → 404 分支。
    let (result, archive_version) = match result {
        Err(evorule_governance_host::session::SessionError::NotFound { .. }) => {
            match fork_from_archive(&api, parent_id, params.version).await {
                Ok((id, forked_version)) => (Ok(id), Some(forked_version)),
                Err(ArchiveForkRejection::NoArchive) => (
                    Err(evorule_governance_host::session::SessionError::NotFound { id: parent_id }),
                    None,
                ),
                Err(rej) => {
                    tracing::warn!(parent_id, "fork-from-archive rejected: {:?}", rej);
                    return Err(rej.to_status());
                }
            }
        }
        other => (other, None),
    };

    match result {
        Ok(id) => {
            metrics.inc_sessions();

            // spawn hit-stats 归因记录任务
            api.spawn_hit_stats_recorder(id);

            // spawn 回写转发任务（旗标缺省关）
            api.spawn_writeback_forwarder(id);

            // fork 子会话补 IoSubscriber（原缺口：IoRequest 60s
            // 超时无人应答）＋声明继承父会话（fork 语义 = 同主体派生）。
            // 父未声明 → 不登记（Unknown → Deny，fail-closed 与今日一致）；
            // fork-from-archive（archive_version.is_some()）父会话已不在内存、
            // 无声明可继承 → 不登记（fail-closed，响应体 source="archive" 可区分）。
            let inherited_role = api
                .declared_caller_role(parent_id)
                .filter(|_| archive_version.is_none());
            api.spawn_io_subscriber_for_session(id, metrics.clone(), inherited_role)
                .await;

            let mut body = serde_json::json!({

                "session_id": id,

                "parent_session_id": parent_id,

                "message": "Session created from parent",

                "forked_from_version": archive_version.or(params.version)

            });
            // 归档 fork 标记：响应体可区分内存 fork 与归档链 fork
            if archive_version.is_some() {
                body["source"] = serde_json::json!("archive");
            }
            Ok(Json(body))
        }

        Err(evorule_governance_host::session::SessionError::NotFound { id }) => {
            tracing::warn!(parent_id = id, "Parent session not found");

            Err(StatusCode::NOT_FOUND)
        }

        Err(evorule_governance_host::session::SessionError::LimitExceeded { current, max }) => {
            tracing::warn!(current, max, "Session creation rejected: limit exceeded");

            Err(StatusCode::TOO_MANY_REQUESTS)
        }

        Err(evorule_governance_host::session::SessionError::InvalidVersion { version }) => {
            tracing::warn!(version, "Invalid version for session fork");

            Err(StatusCode::BAD_REQUEST)
        }

        Err(evorule_governance_host::session::SessionError::WalUnavailable {
            session_id,
            source,
        }) => {
            tracing::error!(
                session_id,
                error = %source,
                "Session creation rejected: WAL unavailable (audit chain cannot be established)"
            );

            Err(StatusCode::SERVICE_UNAVAILABLE)
        }

        // 归档链完整性失败 → 500（fail-closed，原因入 error 日志）。
        // ArchiveCorrupted 变体随 evorule 0.4.3 发布（fork-from-archive 原语），
        // 2026-09-09 联动升级由 catch-all 桥接改为显式命名分支。
        Err(evorule_governance_host::session::SessionError::ArchiveCorrupted { reason }) => {
            tracing::error!(
                reason,
                "Session creation rejected: archive corrupted (fail-closed)"
            );
            Err(StatusCode::INTERNAL_SERVER_ERROR)
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

        (status = 404, description = "父会话不存在（内存与归档均无）"),

        (status = 429, description = "超过最大会话数"),

        (status = 400, description = "缺少/无效版本"),

        (status = 500, description = "归档 fork 时审计链完整性校验失败（fail-closed）"),

        (status = 503, description = "新会话 WAL 不可用（审计链无法建立）")

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

    // fork-from-archive：内存父会话不存在时，从 wal_dir 归档链兜底重建
    //（读 WAL → 全链校验重放 → create_session_from_initial_state）。
    // 归档也不存在（NoArchive）则回落到既有 NotFound → 404 分支。
    let (result, archive_version) = match result {
        Err(evorule_governance_host::session::SessionError::NotFound { .. }) => {
            match fork_from_archive(&api, parent_id, Some(version)).await {
                Ok((id, forked_version)) => (Ok(id), Some(forked_version)),
                Err(ArchiveForkRejection::NoArchive) => (
                    Err(evorule_governance_host::session::SessionError::NotFound { id: parent_id }),
                    None,
                ),
                Err(rej) => {
                    tracing::warn!(parent_id, "fork-from-archive rejected: {:?}", rej);
                    return Err(rej.to_status());
                }
            }
        }
        other => (other, None),
    };

    match result {
        Ok(id) => {
            metrics.inc_sessions();

            // spawn hit-stats 归因记录任务
            api.spawn_hit_stats_recorder(id);

            // spawn 回写转发任务（旗标缺省关）
            api.spawn_writeback_forwarder(id);

            // fork 子会话补 IoSubscriber＋声明继承（同 from 端点口径）
            let inherited_role = api
                .declared_caller_role(parent_id)
                .filter(|_| archive_version.is_none());
            api.spawn_io_subscriber_for_session(id, metrics.clone(), inherited_role)
                .await;

            let mut body = serde_json::json!({

                "session_id": id,

                "parent_session_id": parent_id,

                "forked_from_version": archive_version.unwrap_or(version),

                "message": "Session forked from parent at specified version"

            });
            // 归档 fork 标记：响应体可区分内存 fork 与归档链 fork
            if archive_version.is_some() {
                body["source"] = serde_json::json!("archive");
            }
            Ok(Json(body))
        }

        Err(evorule_governance_host::session::SessionError::NotFound { id }) => {
            tracing::warn!(parent_id = id, "Parent session not found for fork");

            Err(StatusCode::NOT_FOUND)
        }

        Err(evorule_governance_host::session::SessionError::LimitExceeded { current, max }) => {
            tracing::warn!(current, max, "Session fork rejected: limit exceeded");

            Err(StatusCode::TOO_MANY_REQUESTS)
        }

        Err(evorule_governance_host::session::SessionError::InvalidVersion { version }) => {
            tracing::warn!(version, "Invalid version for session fork");

            Err(StatusCode::BAD_REQUEST)
        }

        Err(evorule_governance_host::session::SessionError::WalUnavailable {
            session_id,
            source,
        }) => {
            tracing::error!(
                session_id,
                error = %source,
                "Session fork rejected: WAL unavailable (audit chain cannot be established)"
            );

            Err(StatusCode::SERVICE_UNAVAILABLE)
        }

        // 同 create_session：ArchiveCorrupted（归档校验失败拒绝 fork）→ 500 fail-closed。
        // 2026-09-09 联动升级由 catch-all 桥接改为显式命名分支。
        Err(evorule_governance_host::session::SessionError::ArchiveCorrupted { reason }) => {
            tracing::error!(
                reason,
                "Session fork rejected: archive corrupted (fail-closed)"
            );
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// 惰性同 sid 复活失败原因（由调用方映射为 HTTP 状态码/工作区错误）
#[derive(Debug)]
enum RehydrateRejection {
    /// 无归档可用（wal_dir 未启用 / 无该会话档案 / 复活开关关闭）→ 404（既有语义）
    NoArchive,
    /// 归档链完整性校验失败（fail-closed，疑似篡改/损坏）→ 404
    ///
    /// 按裁定与「无归档」同码：调用方契约（死会话=404）不变，error! 留痕保证可见。
    Corrupted(String),
    /// 归档 WAL 读取 I/O 失败 → 404（同上裁定口径）
    Io(String),
    /// 会话数超限 → 429
    LimitExceeded { current: usize, max: usize },
    /// WAL 不可用（复活会话审计链无法续链，确定性引擎拒启）→ 503
    WalUnavailable { session_id: u64, source: String },
}

impl std::fmt::Display for RehydrateRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoArchive => write!(f, "无归档可用（wal_dir 未启用或无该会话档案）"),
            Self::Corrupted(reason) => write!(f, "归档链完整性校验失败: {reason}"),
            Self::Io(e) => write!(f, "归档 WAL 读取失败: {e}"),
            Self::LimitExceeded { current, max } => write!(f, "会话数超限: {current}/{max}"),
            Self::WalUnavailable { session_id, source } => {
                write!(f, "会话 {session_id} 复活 WAL 不可用: {source}")
            }
        }
    }
}

impl RehydrateRejection {
    fn to_status(&self) -> StatusCode {
        match self {
            Self::NoArchive | Self::Corrupted(_) | Self::Io(_) => StatusCode::NOT_FOUND,
            Self::LimitExceeded { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::WalUnavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

/// fork-from-archive 失败原因（由调用方映射为 HTTP 状态码）
#[derive(Debug)]
enum ArchiveForkRejection {
    /// 无归档可用（wal_dir 未启用 / 无该父会话档案）→ 维持 404 语义
    NoArchive,
    /// 归档链完整性校验失败（fail-closed，疑似篡改/损坏）→ 500
    Corrupted(String),
    /// 归档 WAL 读取 I/O 失败 → 500
    Io(String),
    /// 目标版本不在归档链内 → 400
    InvalidVersion(u64),
    /// 会话数超限 → 429
    LimitExceeded { current: usize, max: usize },
    /// WAL 不可用（新会话审计链无法建立，确定性引擎拒启）→ 503
    WalUnavailable { session_id: u64, source: String },
}

impl std::fmt::Display for ArchiveForkRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoArchive => write!(f, "无归档可用（wal_dir 未启用或无该会话档案）"),
            Self::Corrupted(reason) => write!(f, "归档链完整性校验失败: {reason}"),
            Self::Io(e) => write!(f, "归档 WAL 读取失败: {e}"),
            Self::InvalidVersion(v) => write!(f, "目标版本 {v} 不在归档链内"),
            Self::LimitExceeded { current, max } => write!(f, "会话数超限: {current}/{max}"),
            Self::WalUnavailable { session_id, source } => {
                write!(f, "新会话 {session_id} WAL 不可用: {source}")
            }
        }
    }
}

impl ArchiveForkRejection {
    fn to_status(&self) -> StatusCode {
        match self {
            Self::NoArchive => StatusCode::NOT_FOUND,
            Self::InvalidVersion(_) => StatusCode::BAD_REQUEST,
            Self::LimitExceeded { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::WalUnavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Self::Corrupted(_) | Self::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// 读父会话归档 WAL 全部记录（含轮换分片，只读；与审计档案查询同一通道）
fn read_archive_records(
    wal_dir: &std::path::Path,
    parent_id: u64,
) -> Result<Vec<evorule_reactor::WalRecord>, ArchiveForkRejection> {
    match audit_archive::read_records(wal_dir, parent_id) {
        Ok(records) => Ok(records),
        Err(
            audit_archive::ArchiveError::NotFound(_) | audit_archive::ArchiveError::WalDisabled,
        ) => Err(ArchiveForkRejection::NoArchive),
        Err(audit_archive::ArchiveError::Io(e)) => {
            tracing::error!(parent_id, error = %e, "fork-from-archive: 归档 WAL 读取失败");
            Err(ArchiveForkRejection::Io(e))
        }
    }
}

/// 归档记录全链哈希校验 + 重放到目标版本（fail-closed，先于任何建会话动作）
fn replay_archive_state(
    parent_id: u64,
    records: &[evorule_reactor::WalRecord],
    version: Option<u64>,
) -> Result<(JsonValue, u64), ArchiveForkRejection> {
    match session::payload_from_wal_records(records, version) {
        Ok(state) => Ok(state),
        Err(session::SessionError::ArchiveCorrupted { reason }) => {
            tracing::error!(
                parent_id,
                %reason,
                "fork-from-archive: 归档链完整性校验失败，拒绝 fork（fail-closed）"
            );
            Err(ArchiveForkRejection::Corrupted(reason))
        }
        Err(session::SessionError::InvalidVersion { version }) => {
            tracing::warn!(
                parent_id,
                version,
                "fork-from-archive: 目标版本不在归档链内"
            );
            Err(ArchiveForkRejection::InvalidVersion(version))
        }
        Err(e) => {
            tracing::error!(parent_id, error = %e, "fork-from-archive: 归档重放失败");
            Err(ArchiveForkRejection::Corrupted(e.to_string()))
        }
    }
}

/// 读归档 WAL 并重放出初始状态（fork-from-archive 的同步前置步骤，会话锁外执行）
fn load_archive_initial_state(
    wal_dir: &std::path::Path,
    parent_id: u64,
    version: Option<u64>,
) -> Result<(JsonValue, u64), ArchiveForkRejection> {
    let records = read_archive_records(wal_dir, parent_id)?;
    replay_archive_state(parent_id, &records, version)
}

/// fork-from-archive：父会话不在内存时，从 `wal_dir` 归档链重建初始状态建新会话。
///
/// 引擎侧原语：[`session::payload_from_wal_records`]（全链校验重放）+
/// [`session::SessionManager::create_session_from_initial_state`]（统一 fork
/// 落点，子会话审计链从初始状态独立续起，因果父链接保留）。
///
/// 返回 `(新会话 ID, 实际 fork 的父版本号)`。文件读取/校验/重放在会话锁外完成，
/// 仅最后建会话一步持有会话锁。
async fn fork_from_archive(
    api: &SessionApi,
    parent_id: u64,
    version: Option<u64>,
) -> Result<(u64, u64), ArchiveForkRejection> {
    let Some(wal_dir) = api.wal_dir.as_ref() else {
        return Err(ArchiveForkRejection::NoArchive);
    };

    // 读归档 + 全链校验重放（fail-closed，先于任何建会话动作）
    let (initial_payload, initial_version) =
        match load_archive_initial_state(wal_dir, parent_id, version) {
            Ok(state) => state,
            Err(rej) => return Err(rej),
        };

    // 建新会话（统一 fork 落点：WAL 版 FactsLog fail-closed → spawn reactor → 注册）
    let sessions = api.sessions.lock().await;
    match sessions.create_session_from_initial_state(
        Some(parent_id),
        initial_payload,
        initial_version,
    ) {
        Ok(id) => Ok((id, initial_version)),
        Err(session::SessionError::LimitExceeded { current, max }) => {
            Err(ArchiveForkRejection::LimitExceeded { current, max })
        }
        Err(session::SessionError::WalUnavailable { session_id, source }) => {
            Err(ArchiveForkRejection::WalUnavailable {
                session_id,
                source: source.to_string(),
            })
        }
        Err(e) => Err(ArchiveForkRejection::Corrupted(e.to_string())),
    }
}

/// 命令提交 query 参数
#[derive(Debug, Deserialize)]
pub struct CommandWaitParams {
    /// `true` = 同请求等待该指令处理完成（StateTransition/Violation 落链）后应答；
    /// 缺省/false = 既有 send 即返语义
    pub wait: Option<bool>,
}

/// 命令提交响应（既有形态与 wait=true 形态的双形态包裹）
#[derive(Debug, Serialize, ToSchema)]
#[serde(untagged)]
pub enum CommandSubmitResponse {
    /// 既有 send 即返形态（wait 缺省/false，或 Schema 门禁/通道错误）
    Ack(ApiResponse),
    /// wait=true 同步等待形态
    Waited(CommandWaitResponse),
}

/// wait=true 同步等待响应
#[derive(Debug, Serialize, ToSchema)]
pub struct CommandWaitResponse {
    /// 提交是否成功（结论事实落链即 true）
    pub success: bool,

    /// 消息
    pub message: String,

    /// 提交事实 ID（与既有 ApiResponse.fact_id 同义）
    pub fact_id: u64,

    /// 受理结论：`true`=指令受理（StateTransition 落链）；`false`=被 enforce
    /// 拦截（Violation 落链）；`null`=等待窗口内结论未落定（消费方可退回
    /// 既有的 version 轮询判据）
    pub accepted: Option<bool>,

    /// enforce 拦截详情（accepted=false 时填充）
    pub violation: Option<CommandViolation>,

    /// 稳定码：RULE_VIOLATION（拦截）/ WAIT_TIMEOUT（窗口超时）
    pub code: Option<String>,
}

/// enforce 拦截详情（与链上 Violation 事实的 rule_index/reason 字段同源）
#[derive(Debug, Serialize, ToSchema)]
pub struct CommandViolation {
    /// 命中的 enforce 规则在合并规则列表中的下标
    pub rule_index: u64,

    /// 违规说明（enforce params.reason）
    pub reason: String,
}

/// wait=true 单条指令的结论
#[derive(Debug)]
enum CommandVerdict {
    /// 指令受理（StateTransition 落链；IoRequest 亦视为受理——指令已进入执行）
    Accepted,
    /// 被 enforce 拦截（Violation 落链）
    Rejected { rule_index: u64, reason: String },
    /// 等待窗口内结论未落定
    Timeout,
}

/// wait=true 等待窗口（引擎毫秒级收敛，2s 上限覆盖深度队列；超时降级为
/// 既有 send 即返语义，消费方按 accepted=null 退回轮询判据）
const COMMAND_WAIT_TIMEOUT: Duration = Duration::from_secs(2);

/// wait=true 轮询间隔（结论事实以 cause 精确匹配，高频低开销尾窗扫描）
const COMMAND_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// 每次轮询的尾窗扫描深度（结论事实紧跟提交事实落链，128 覆盖同窗并发提交）
const COMMAND_WAIT_SCAN_WINDOW: usize = 128;

/// 会话锁外轮询 facts_log，等待指定提交事实的结论事实落链
///
/// 判据（与链上事实一一对应，零新增判据源）：
/// - `StateTransition{cause=提交事实}` / `IoRequest{cause=提交事实}` → 受理；
/// - `Violation{cause=提交事实}` → enforce 拦截（记录性事实，不推进版本，
///   故仅靠 version 轮询无法感知——本等待恰好补齐该缺口）；
/// - 窗口耗尽 → [`CommandVerdict::Timeout`]（降级语义，不误判）。
async fn wait_for_command_verdict(
    facts_log: &FactsLog,
    fact_id: u64,
    timeout: Duration,
) -> CommandVerdict {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        for (_, fact) in facts_log.history_last_with_versions(COMMAND_WAIT_SCAN_WINDOW) {
            match fact {
                Fact::StateTransition { cause, .. } | Fact::IoRequest { cause, .. }
                    if cause.0 == fact_id =>
                {
                    return CommandVerdict::Accepted;
                }
                Fact::Violation {
                    cause,
                    rule_index,
                    reason,
                    ..
                } if cause.0 == fact_id => {
                    return CommandVerdict::Rejected { rule_index, reason };
                }
                _ => {}
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return CommandVerdict::Timeout;
        }
        tokio::time::sleep(COMMAND_WAIT_POLL_INTERVAL).await;
    }
}

/// 会话命令提交 handler
///
/// `POST /api/sessions/:id/command` → 提交命令到指定会话的反应器。
/// `?wait=true` 时在同一请求内等待该指令的结论事实落链后应答
/// （StateTransition=受理 / Violation=enforce 拦截；纯应答时序变化，
/// 指令与 Violation 照常落链，账面语义不变）。
#[utoipa::path(

    post,

    path = "/api/sessions/{id}/command",

    tag = "sessions",

    params(
        ("id" = u64, Path, description = "会话 ID"),
        ("wait" = Option<bool>, Query, description = "true=同请求等待指令处理完成（含 enforce 拦截结论）后应答")
    ),

    request_body = CommandRequest,

    responses(

        (status = 200, description = "命令已提交/已处理，返回 fact_id（wait=true 时含受理结论）", body = CommandSubmitResponse),

        (status = 404, description = "会话不存在（存在有效归档时按原 sid 复活；归档缺失或链校验失败仍 404）")

    )

)]

async fn session_command(
    State(api): State<SessionApi>,

    State(metrics): State<SharedMetrics>,
    State(sanitizer): State<Arc<InputSanitizer>>,
    Path(session_id): Path<u64>,

    Query(params): Query<CommandWaitParams>,

    Json(req): Json<CommandRequest>,
) -> Result<Json<CommandSubmitResponse>, StatusCode> {
    // Phase 1: 第一层输入净化（静默改写 Prompt 注入内容）
    let (mut instruction_value, sanitize_report) = sanitizer.sanitize_value(&req.instruction);
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

    // pause 准入闸（会话级干预，管理端点置位后生效）——paused 会话拒绝新
    // Command（423 语义），在途队列与 pending io 超时路径不受影响，io_response
    // 放行（否则超时 Error 污染链）；pause 状态经 GET/POST pause、resume 管理。
    {
        let paused = api
            .paused_sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if paused.contains(&session_id) {
            return Ok(Json(CommandSubmitResponse::Ack(ApiResponse {
                success: false,
                message: "会话处于暂停态（intervention pause），指令未进引擎；resume 后可重新提交"
                    .to_string(),
                fact_id: None,
                code: Some("SESSION_PAUSED".to_string()),
            })));
        }
    }

    let schema_report = evorule_rule_schema::validate_command_instruction(&instruction_value);
    if !schema_report.valid {
        let detail = schema_report.errors.join("; ");
        tracing::warn!(
            session_id = session_id,
            "session_command 未通过 Schema 门禁，已拒绝提交: {detail}"
        );
        return Ok(Json(CommandSubmitResponse::Ack(ApiResponse {
            success: false,
            message: format!(
                "指令未通过 Schema 门禁（引擎原生结构非法，参见固化 rule_set v1.0 Schema，records/77）: {detail}"
            ),
            fact_id: None,

            code: Some("SCHEMA_GATE_FAILED".to_string()),
        })));
    }

    // 声明会话注入 __meta__.caller_role（Schema 门禁已过，落账前注入；
    // 随 WAL 落账 = resolver 判定权威源）
    api.inject_caller_role(session_id, &mut instruction_value);

    let instruction = serde_to_tcb(instruction_value);

    // 获取或惰性复活（内存表未命中且开关开时从 WAL 归档按原 sid 复活）。
    // 会话锁不跨 send 持有：session 为句柄克隆，command_tx 生命周期独立于
    // 活跃表（TTL 回收只移表不杀反应器），通道语义与既有失败分支一致。
    // 命令 ID 分配必须在本步之后——复活路径经
    // fetch_max(max_api_id + 1) 按归档抬升 API 层计数器；若先分配后复活，
    // 重启后首命令将复用缺省起点与归档最大 Command id 撞号（集成测试
    // test_rehydrate_lifts_api_command_id_space 锁定此时序）。
    let session = api.session_or_rehydrate(session_id).await.map_err(|rej| {
        tracing::warn!(session_id, rejection = %rej, "会话获取失败（含复活尝试）");
        rej.to_status()
    })?;

    let id = api.next_id();

    match session.command_tx.send(Fact::Command { id, instruction }) {
        Ok(()) => {
            // 缺口2 修复: 命令提交后实时刷新审计链 (BLAKE3 哈希链)

            session.audit_new();

            if !params.wait.unwrap_or(false) {
                return Ok(Json(CommandSubmitResponse::Ack(ApiResponse {
                    success: true,

                    message: "Command submitted".to_string(),

                    fact_id: Some(id.0),

                    code: None,
                })));
            }

            // 同步裁决等待（?wait=true）: 纯应用层等待语义——会话锁外轮询
            // facts_log，直到该指令的结论事实落链（StateTransition/IoRequest=
            // 受理；Violation=enforce 拦截）。指令与 Violation 照常落链，
            // 账面/可回放语义不变，仅应答时序从「轮询感知」变「同步感知」。
            let facts_log = session.facts_log.clone();
            let auditor = session.auditor.clone();

            let verdict = wait_for_command_verdict(&facts_log, id.0, COMMAND_WAIT_TIMEOUT).await;

            // 结论事实刚落链，再刷一次审计链保账面及时
            if let Ok(mut auditor) = auditor.lock() {
                auditor.audit_new();
            }

            Ok(Json(match verdict {
                CommandVerdict::Accepted => CommandSubmitResponse::Waited(CommandWaitResponse {
                    success: true,
                    message: "Command processed".to_string(),
                    fact_id: id.0,
                    accepted: Some(true),
                    violation: None,
                    code: None,
                }),
                CommandVerdict::Rejected { rule_index, reason } => {
                    CommandSubmitResponse::Waited(CommandWaitResponse {
                        success: true,
                        message: format!("Command rejected by enforced rule: {reason}"),
                        fact_id: id.0,
                        accepted: Some(false),
                        violation: Some(CommandViolation { rule_index, reason }),
                        code: Some("RULE_VIOLATION".to_string()),
                    })
                }
                CommandVerdict::Timeout => CommandSubmitResponse::Waited(CommandWaitResponse {
                    success: true,
                    message: "Command submitted (result not settled within wait window)"
                        .to_string(),
                    fact_id: id.0,
                    accepted: None,
                    violation: None,
                    code: Some("WAIT_TIMEOUT".to_string()),
                }),
            }))
        }

        Err(_) => Ok(Json(CommandSubmitResponse::Ack(ApiResponse {
            success: false,

            message: "Command channel closed (reactor exited)".to_string(),

            fact_id: None,

            code: Some("SESSION_ERROR".to_string()),
        }))),
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

        (status = 404, description = "会话不存在（存在有效归档时按原 sid 复活；归档缺失或链校验失败仍 404）")

    )

)]

async fn session_state(
    State(api): State<SessionApi>,

    State(metrics): State<SharedMetrics>,

    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // 获取或惰性复活（内存表未命中且开关开时从 WAL 归档按原 sid 复活）
    let session = api.session_or_rehydrate(session_id).await.map_err(|rej| {
        tracing::warn!(session_id, rejection = %rej, "会话获取失败（含复活尝试）");
        rej.to_status()
    })?;

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
    let session = api.session_or_rehydrate(session_id).await.map_err(|rej| {
        tracing::warn!(session_id, rejection = %rej, "会话读侧获取失败（含复活尝试）");
        rej.to_status()
    })?;

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
    let session = api.session_or_rehydrate(session_id).await.map_err(|rej| {
        tracing::warn!(session_id, rejection = %rej, "会话读侧获取失败（含复活尝试）");
        rej.to_status()
    })?;

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

/// 会话锚点链查询 handler（G-A2）
///
/// `GET /api/sessions/:id/anchors` → 返回已封审计锚点列表
///
/// 空列表 = 锚点链未启用（SessionManager 未配置 anchor）或尚未封条。
#[utoipa::path(
    get,
    path = "/api/sessions/{id}/anchors",
    tag = "sessions",
    params(("id" = u64, Path, description = "会话 ID")),
    responses(
        (status = 200, description = "锚点列表（enabled 标志锚点链是否已封出至少一个锚点）"),
        (status = 404, description = "会话不存在")
    )
)]
async fn session_anchors(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let session = api.session_or_rehydrate(session_id).await.map_err(|rej| {
        tracing::warn!(session_id, rejection = %rej, "会话读侧获取失败（含复活尝试）");
        rej.to_status()
    })?;
    // 兜底刷新：确保自动封条间隔已到的事实先入链再取快照
    let _new_count = session.audit_new();
    let anchors: Vec<serde_json::Value> =
        session.anchors().into_iter().map(anchor_to_json).collect();
    let count = anchors.len();
    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "count": count,
        "anchors": anchors,
    })))
}

/// AuditAnchor → JSON（AuditAnchor 未实现 Serialize，部署面手动映射；
/// 字段与 evorule-governance anchor 模块一一对应）
fn anchor_to_json(a: evorule_governance::anchor::AuditAnchor) -> serde_json::Value {
    // anchor_format：签名载荷格式版本（独立验证器按此分派验签目标）
    serde_json::json!({
        "anchor_format": evorule_governance::anchor::ANCHOR_FORMAT,
        "seq": a.seq,
        "session_id": a.session_id,
        "fact_range": {"lo": a.fact_range.lo, "hi": a.fact_range.hi},
        "chain_head": a.chain_head,
        "key_id": a.key_id,
        "engine_id": a.engine_id,
        "logical_time": a.logical_time,
        "anchor_hash": a.anchor_hash,
        "signature": a.signature,
    })
}

/// 会话锚点封条 handler（G-A2）
///
/// `POST /api/sessions/:id/anchors/seal` → 立即封一个锚点（会话结束封条/手动强制封）
///
/// 未启用锚点链时返回 409（与启用与否可区分，不与 404 混淆）。
#[utoipa::path(
    post,
    path = "/api/sessions/{id}/anchors/seal",
    tag = "sessions",
    params(("id" = u64, Path, description = "会话 ID")),
    responses(
        (status = 200, description = "封出的锚点"),
        (status = 404, description = "会话不存在"),
        (status = 409, description = "锚点链未启用（SessionManager 未配置 anchor）")
    )
)]
async fn session_anchor_seal(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, (StatusCode, axum::Json<serde_json::Value>)> {
    let sessions = api.sessions.lock().await;
    sessions.touch_session(session_id);
    let session = sessions.get_session(session_id).ok_or((
        StatusCode::NOT_FOUND,
        axum::Json(serde_json::json!({"error": "session not found"})),
    ))?;
    let _new_count = session.audit_new();
    let anchor = session.seal_anchor_now().map_err(|e| {
        tracing::warn!(session_id, "anchor seal unavailable: {e}");
        // 409 响应体携带具体 AnchorError 与启用指引，
        // 不再丢弃错误信息（此前排障只能看到裸 409）
        (
            StatusCode::CONFLICT,
            axum::Json(serde_json::json!({
                "error": e.to_string(),
                "hint": "anchor chain not enabled: set EVORULE_ANCHOR_SEED (64-hex ed25519 seed) and restart",
            })),
        )
    })?;
    let anchor_json = anchor_to_json(anchor);
    drop(sessions);
    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "sealed": true,
        "anchor": anchor_json,
    })))
}

/// 心跳最小间隔（秒）。心跳以 PayloadUpdate 进会话链 bump version，有链膨胀
/// 成本（参照 Stable 瘦身事故），限频 60s/次；超频 429。
const HEARTBEAT_MIN_INTERVAL_SECS: u64 = 60;

/// `POST /api/sessions/{id}/heartbeat` → 注入心跳（监控干预 io.inject_heartbeat）
///
/// 通路：`command_tx.send(Fact::PayloadUpdate)`（与 io 注入同一通路），心跳事实
/// 进会话链自证（操作落账原则）；等效 touch 防 reap。pause 不拦截心跳——pause
/// 准入闸语义是"拒新 Command"，监控信号与 pending io 超时同属放行面。
/// 限频 [`HEARTBEAT_MIN_INTERVAL_SECS`]，超频 429。
#[utoipa::path(
    post,
    path = "/api/sessions/{id}/heartbeat",
    tag = "sessions",
    params(("id" = u64, Path, description = "会话 ID")),
    responses(
        (status = 200, description = "心跳已注入（返回事实 id 与路径）", body = serde_json::Value),
        (status = 404, description = "会话不存在"),
        (status = 429, description = "距上次心跳不足限频窗口"),
        (status = 409, description = "反应器已退出（指令通道关闭）")
    )
)]
async fn session_heartbeat(
    State(api): State<SessionApi>,
    State(metrics): State<SharedMetrics>,
    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // 限频先行（不持有 sessions 锁判定，降低竞争面）
    {
        let mut last = api.heartbeat_last.lock().unwrap_or_else(|p| p.into_inner());
        let now = std::time::Instant::now();
        if let Some(t) = last.get(&session_id) {
            if now.duration_since(*t).as_secs() < HEARTBEAT_MIN_INTERVAL_SECS {
                return Err(StatusCode::TOO_MANY_REQUESTS);
            }
        }
        last.insert(session_id, now);
    }
    let sessions = api.sessions.lock().await;
    sessions.touch_session(session_id);
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;
    let ts = chrono::Utc::now().to_rfc3339();
    let path = format!("system.heartbeat.{}", chrono::Utc::now().timestamp_millis());
    let id = api.next_id();
    session
        .command_tx
        .send(evorule_reactor::Fact::PayloadUpdate {
            id,
            path: path.clone(),
            value: evorule_tcb::JsonValue::String(ts.into()),
        })
        .map_err(|_| StatusCode::CONFLICT)?;
    metrics.inc_commands("heartbeat");
    drop(sessions);
    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "injected": true,
        "fact_id": id.0,
        "path": path,
    })))
}

/// `POST /api/sessions/{id}/gc` → 手动触发事实链压缩（compact）
///
/// 契约：compact 丢弃 last_stable_version 前历史 → history/rewind/causal 对已
/// 压缩区间失效；`compacted_info` 进响应。生产无先例——属高危干预，挂
/// `--allow-interventions` 双保险（默认不注册路由，404），先 dev 验证回放兼容。
#[utoipa::path(
    post,
    path = "/api/sessions/{id}/gc",
    tag = "sessions",
    params(("id" = u64, Path, description = "会话 ID")),
    responses(
        (status = 200, description = "压缩执行（ratio + compacted_info）", body = serde_json::Value),
        (status = 404, description = "会话不存在")
    )
)]
async fn session_gc(
    State(api): State<SessionApi>,
    State(metrics): State<SharedMetrics>,
    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;
    sessions.touch_session(session_id);
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;
    let ratio = session.facts_log.compact();
    let info = session.facts_log.compacted_info();
    drop(sessions);
    // 操作落账：压缩改变链可见历史，平台事件报表须可追溯
    crate::api::platform_auth::append_platform_event(
        &api.shared_facts,
        "intervention_gc",
        serde_json::json!({
            "session_id": session_id,
            "ratio": ratio,
            "compacted_info": info,
        }),
    );
    metrics.inc_commands("gc");
    let info_json = info.map(|(v, n)| serde_json::json!({ "version": v, "entries": n }));
    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "compacted": true,
        "ratio": ratio,
        "compacted_info": info_json,
    })))
}

/// `GET /api/sessions/{id}/pause` → 查询 pause 准入闸状态
///
/// 纯内存闸位查询：不校验会话存在性（会话被 reap 后闸位残留也能如实报告；
/// server 重启后闸位清零——pause 是内存态，不持久化）。
#[utoipa::path(
    get,
    path = "/api/sessions/{id}/pause",
    tag = "sessions",
    params(("id" = u64, Path, description = "会话 ID")),
    responses(
        (status = 200, description = "闸位状态", body = serde_json::Value)
    )
)]
async fn session_pause_status(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
) -> Json<serde_json::Value> {
    let paused = api
        .paused_sessions
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let paused = paused.contains(&session_id);
    Json(serde_json::json!({ "session_id": session_id, "paused": paused }))
}

/// `POST /api/sessions/{id}/pause` → 置位 pause 准入闸（拒新 Command，423/SESSION_PAUSED）
///
/// 不真停反应器：在途队列与 pending io 超时路径继续，io_response 放行（否则
/// 超时 Error 污染链）。reap 风险：pause 后会话不再被指令触碰，空闲 TTL 到期
/// 仍会被回收——长驻暂停须调用方周期 heartbeat（等效 touch）维持。
#[utoipa::path(
    post,
    path = "/api/sessions/{id}/pause",
    tag = "sessions",
    params(("id" = u64, Path, description = "会话 ID")),
    responses(
        (status = 200, description = "已置位", body = serde_json::Value),
        (status = 404, description = "会话不存在")
    )
)]
async fn session_pause(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
    actor: Option<Extension<crate::api::platform_auth::AuthedActor>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    {
        let sessions = api.sessions.lock().await;
        sessions.touch_session(session_id);
        sessions
            .get_session(session_id)
            .ok_or(StatusCode::NOT_FOUND)?;
    }
    {
        let mut paused = api
            .paused_sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        paused.insert(session_id);
    }
    crate::api::platform_auth::append_platform_event(
        &api.shared_facts,
        "intervention_pause",
        serde_json::json!({ "session_id": session_id, "operator": resolve_approver(actor) }),
    );
    Ok(Json(
        serde_json::json!({ "session_id": session_id, "paused": true }),
    ))
}

/// `POST /api/sessions/{id}/resume` → 解除 pause 准入闸（重新接受 Command）
///
/// 幂等：未置位时解除同样返回成功；会话已 reap 时仍清闸位（防同 id 复用残留）。
#[utoipa::path(
    post,
    path = "/api/sessions/{id}/resume",
    tag = "sessions",
    params(("id" = u64, Path, description = "会话 ID")),
    responses(
        (status = 200, description = "已解除（幂等）", body = serde_json::Value)
    )
)]
async fn session_resume(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
    actor: Option<Extension<crate::api::platform_auth::AuthedActor>>,
) -> Json<serde_json::Value> {
    {
        let mut paused = api
            .paused_sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        paused.remove(&session_id);
    }
    {
        let sessions = api.sessions.lock().await;
        sessions.touch_session(session_id);
    }
    crate::api::platform_auth::append_platform_event(
        &api.shared_facts,
        "intervention_resume",
        serde_json::json!({ "session_id": session_id, "operator": resolve_approver(actor) }),
    );
    Json(serde_json::json!({ "session_id": session_id, "paused": false }))
}

/// `POST /api/sessions/{id}/invariants/scan` → 读取引擎最近一轮不变式自检报告
///
/// 反应器每迭代头运行内置自检并把违规明细发布到共享报告（新鲜度 ≤ 一个
/// 迭代间隔）。检出违规时逐条落 `Fact::Violation`（记录性事实，不 bump
/// version；`rule_index=u64::MAX` 保留值标记机制层记录，`cause=0` 表示无
/// 来源事实——引擎自检发现，非指令触发）。幂等注意：重复扫描会重复落账。
#[utoipa::path(
    post,
    path = "/api/sessions/{id}/invariants/scan",
    tag = "sessions",
    params(("id" = u64, Path, description = "会话 ID")),
    responses(
        (status = 200, description = "扫描报告（steps + violations + 落账数）", body = serde_json::Value),
        (status = 404, description = "会话不存在"),
        (status = 409, description = "反应器已退出（无扫描报告）")
    )
)]
async fn session_invariants_scan(
    State(api): State<SessionApi>,
    State(metrics): State<SharedMetrics>,
    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;
    sessions.touch_session(session_id);
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;
    let report = session
        .handle
        .latest_invariant_scan()
        .ok_or(StatusCode::CONFLICT)?;
    let mut recorded = 0usize;
    for v in &report.violations {
        let id = api.next_id();
        let appended = session.facts_log.append(evorule_reactor::Fact::Violation {
            id,
            cause: evorule_reactor::FactId(0),
            rule_index: u64::MAX,
            reason: v.as_str().to_string(),
            instruction: evorule_tcb::JsonValue::object_from_pairs_owned(vec![(
                "scan_steps".to_string(),
                evorule_tcb::JsonValue::Integer(report.steps as i64),
            )]),
        });
        if appended.is_ok() {
            recorded += 1;
        }
    }
    drop(sessions);
    metrics.inc_commands("invariants_scan");
    let violations: Vec<&str> = report.violations.iter().map(|v| v.as_str()).collect();
    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "steps": report.steps,
        "violations": violations,
        "violation_count": violations.len(),
        "recorded": recorded,
    })))
}

/// `POST /api/sessions/{id}/io/cancel_all` → 取消全部 pending I/O（干预原语）
///
/// 取消对象取自快照 pending 清单（对账面）；实际取消由反应器逐请求镜像
/// 超时错误路径（emit `Fact::Error` + 强移除），Error 事实进审计链可查。
/// 丢弃在途 I/O 属丢弃性动作——挂 `--allow-interventions` 双保险（默认
/// 路由不注册，404）。
#[utoipa::path(
    post,
    path = "/api/sessions/{id}/io/cancel_all",
    tag = "sessions",
    params(("id" = u64, Path, description = "会话 ID")),
    responses(
        (status = 200, description = "已请求取消（requested=取消对象清单）", body = serde_json::Value),
        (status = 404, description = "会话不存在")
    )
)]
async fn session_io_cancel_all(
    State(api): State<SessionApi>,
    State(metrics): State<SharedMetrics>,
    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;
    sessions.touch_session(session_id);
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;
    let requested: Vec<u64> = session
        .handle
        .snapshot()
        .map(|s| s.pending_io.iter().map(|e| e.id.0).collect())
        .unwrap_or_default();
    session.handle.cancel_all_pending();
    drop(sessions);
    // 动作落账：取消动作本身进平台事件；被取消请求的 Error 事实由反应器入链
    crate::api::platform_auth::append_platform_event(
        &api.shared_facts,
        "intervention_cancel_all",
        serde_json::json!({
            "session_id": session_id,
            "requested_count": requested.len(),
        }),
    );
    metrics.inc_commands("io_cancel_all");
    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "requested": requested,
        "count": requested.len(),
    })))
}

/// `POST /api/sessions/{id}/wal/rotate` → 手动轮换会话 WAL（干预原语）
///
/// 纯追加式分片切换（序号 +1 开新文件，旧分片保持完整）：崩溃安全，不影响
/// 链内容与可回放性。纯内存模式（无 WAL）返回 `rotated:false`。
#[utoipa::path(
    post,
    path = "/api/sessions/{id}/wal/rotate",
    tag = "sessions",
    params(("id" = u64, Path, description = "会话 ID")),
    responses(
        (status = 200, description = "轮换结果（rotated=false = 无 WAL 可轮换）", body = serde_json::Value),
        (status = 404, description = "会话不存在"),
        (status = 500, description = "WAL 轮换失败（IO 错误）")
    )
)]
async fn session_wal_rotate(
    State(api): State<SessionApi>,
    State(metrics): State<SharedMetrics>,
    Path(session_id): Path<u64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let sessions = api.sessions.lock().await;
    sessions.touch_session(session_id);
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;
    let rotate_result = session.facts_log.rotate_wal();
    // WAL 观察面（P1 W-3）：轮换调用计数（未注册观察面 → 零操作）
    if let Some(obs) = crate::wal_observation::WalObservation::global() {
        obs.record_rotate("session", rotate_result.is_ok());
    }
    let rotated = rotate_result.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    drop(sessions);
    crate::api::platform_auth::append_platform_event(
        &api.shared_facts,
        "intervention_wal_rotate",
        serde_json::json!({ "session_id": session_id, "rotated": rotated }),
    );
    metrics.inc_commands("wal_rotate");
    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "rotated": rotated,
    })))
}

/// `POST /api/admin/wal/checkpoint` → 全通道 WAL checkpoint（治理面运维端点）
///
/// 对三通路各执行一次纯追加式 WAL 分片切换（崩溃安全，不影响链内容与可回放性）：
/// - 会话通路：逐在线会话 `facts_log.rotate_wal()`（逐项上报，单会话失败不整体 fail）
/// - 单反应器通路：`governance.facts_log().rotate_wal()`
/// - 共享事实通路：`shared_facts.rotate_wal()`
///
/// `rotated:false` = 该通路纯内存/无可轮换后端（非失败）。partial failure
/// 逐项上报，HTTP 恒 200（轮换是可逆运维动作，单项 IO 错误不否定其余通路）。
/// 人工治理动作：自动化凭据拒绝 + 平台管理员硬校验（与审批模式切换同款前置）；
/// 落账 platform.event（admin_wal_checkpoint）。
#[utoipa::path(
    post,
    path = "/api/admin/wal/checkpoint",
    tag = "admin",
    responses(
        (status = 200, description = "三通道轮换结果（partial failure 逐项上报）", body = serde_json::Value),
        (status = 401, description = "未认证"),
        (status = 403, description = "非管理员或自动化凭据")
    )
)]
async fn admin_wal_checkpoint(
    State(api): State<SessionApi>,
    State(gov): State<GovernanceApi>,
    State(metrics): State<SharedMetrics>,
    headers: axum::http::HeaderMap,
    identity: Option<Extension<CallerIdentity>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    // 人工治理动作，自动化凭据拒绝（与审批模式切换同款前置）
    if matches!(
        identity,
        Some(Extension(CallerIdentity::Service)) | Some(Extension(CallerIdentity::App))
    ) {
        return Err(proxy_err(
            StatusCode::FORBIDDEN,
            "自动化凭据禁止执行 WAL checkpoint（人工治理动作）".to_string(),
        ));
    }
    let username = crate::api::platform_auth::require_administrator(&api.shared_facts, &headers)?;

    // 会话通路：逐在线会话轮换（持锁遍历，与单会话 wal/rotate 同款锁纪律）
    let mut items: Vec<serde_json::Value> = Vec::new();
    let mut rotated_count = 0usize;
    let mut in_memory_count = 0usize;
    let mut failed_count = 0usize;
    {
        let sessions = api.sessions.lock().await;
        for sid in sessions.list_sessions() {
            match sessions.get_session(sid) {
                Some(s) => {
                    let r = s.facts_log.rotate_wal();
                    // WAL 观察面（P1 W-3）：逐会话轮换计数（未注册 → 零操作）
                    if let Some(obs) = crate::wal_observation::WalObservation::global() {
                        obs.record_rotate("session", r.is_ok());
                    }
                    match r {
                        Ok(true) => {
                            rotated_count += 1;
                            items.push(serde_json::json!({ "session_id": sid, "rotated": true }));
                        }
                        Ok(false) => {
                            in_memory_count += 1;
                            items.push(serde_json::json!({ "session_id": sid, "rotated": false }));
                        }
                        Err(e) => {
                            failed_count += 1;
                            items.push(
                                serde_json::json!({ "session_id": sid, "error": e.to_string() }),
                            );
                        }
                    }
                }
                None => {
                    // 列举后被回收的会话：跳过并上报，不算失败
                    items.push(serde_json::json!({ "session_id": sid, "skipped": true }));
                }
            }
        }
    }
    let sessions_summary = serde_json::json!({
        "total": items.len(),
        "rotated": rotated_count,
        "in_memory": in_memory_count,
        "failed": failed_count,
        "items": items,
    });

    // 单反应器通路 / 共享事实通路：单点轮换，错误逐项上报
    let reactor_rotate = gov.facts_log().rotate_wal();
    let shared_rotate = api.shared_facts.rotate_wal();
    // WAL 观察面（P1 W-3）：通路轮换计数 + checkpoint 计数（未注册 → 零操作）
    if let Some(obs) = crate::wal_observation::WalObservation::global() {
        obs.record_rotate("reactor", reactor_rotate.is_ok());
        obs.record_rotate("shared_facts", shared_rotate.is_ok());
    }
    let reactor_result = match reactor_rotate {
        Ok(rotated) => serde_json::json!({ "rotated": rotated }),
        Err(e) => serde_json::json!({ "error": e.to_string() }),
    };
    let shared_result = match shared_rotate {
        Ok(rotated) => serde_json::json!({ "rotated": rotated }),
        Err(e) => serde_json::json!({ "error": e.to_string() }),
    };
    if let Some(obs) = crate::wal_observation::WalObservation::global() {
        let checkpoint_ok = failed_count == 0
            && reactor_result.get("error").is_none()
            && shared_result.get("error").is_none();
        obs.record_checkpoint(checkpoint_ok);
    }

    // 治理落账：platform.event（操作者 + 三通路摘要）
    crate::api::platform_auth::append_platform_event(
        &api.shared_facts,
        "admin_wal_checkpoint",
        serde_json::json!({
            "operator": username,
            "sessions": {
                "total": sessions_summary["total"],
                "rotated": rotated_count,
                "in_memory": in_memory_count,
                "failed": failed_count,
            },
            "reactor": reactor_result,
            "shared_facts": shared_result,
        }),
    );
    metrics.inc_commands("admin_wal_checkpoint");

    Ok(Json(serde_json::json!({
        "sessions": sessions_summary,
        "reactor": reactor_result,
        "shared_facts": shared_result,
    })))
}

/// 发布域身份绑定中间件（挂 workspace 路由；认证中间件之后运行）
///
/// 平台会话（含静态 user token 的占位身份）→ 注入
/// `evorule_workspace::api::PublishActorBinding`：平台 administrator →
/// `max_role = Admin`；其余（快照无此用户的静态 token 在内）→
/// `max_role = DepartmentHead`——Admin 角色声明自此必须平台管理员登录背书。
/// 自动化凭据（service/app key）→ 注入 `PublishAutomatedCred`。
/// 二者皆无（开发模式零配置）→ 不注入，workspace handler 回落请求体自声明。
async fn publish_binding_middleware(
    State(shared): State<SharedFactsLog>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use evorule_workspace::api::{PublishActorBinding, PublishAutomatedCred};
    use evorule_workspace::models::PublishRole;

    let automated = matches!(
        req.extensions().get::<crate::auth::CallerIdentity>(),
        Some(crate::auth::CallerIdentity::Service) | Some(crate::auth::CallerIdentity::App)
    );
    if automated {
        req.extensions_mut().insert(PublishAutomatedCred);
    } else if let Some(actor) = req
        .extensions()
        .get::<crate::api::platform_auth::AuthedActor>()
        .cloned()
    {
        // 管理员判定以平台事实快照为准; 快照无此用户（静态 token 占位身份）
        // 按 DepartmentHead 上限处理——静态凭据不得自封 Admin
        let is_admin = crate::api::platform_auth::PlatformSnapshot::replay(&shared)
            .ok()
            .and_then(|snap| snap.users.get(&actor.0).cloned())
            .map(|u| u.role == "administrator" && u.status == "ACTIVE")
            .unwrap_or(false);
        req.extensions_mut().insert(PublishActorBinding {
            user: actor.0,
            max_role: if is_admin {
                PublishRole::Admin
            } else {
                PublishRole::DepartmentHead
            },
        });
    }
    next.run(req).await
}

/// `GET /api/publish/approve_mode` → 查询审批模式开关
///
/// 开 = submit 直发（跳过人工审批驻留）；关（缺省）= submit 驻留 pending。
#[utoipa::path(
    get,
    path = "/api/publish/approve_mode",
    tag = "workspace",
    responses(
        (status = 200, description = "开关状态", body = serde_json::Value),
        (status = 503, description = "workspace 元数据库未接线")
    )
)]
async fn get_approve_mode(
    State(api): State<SessionApi>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let db = api.workspace_db.as_ref().ok_or_else(|| {
        proxy_err(
            StatusCode::SERVICE_UNAVAILABLE,
            "workspace 元数据库未接线".to_string(),
        )
    })?;
    let enabled = db
        .get_approve_mode()
        .map_err(|e| proxy_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "approve_mode": enabled })))
}

/// `POST /api/publish/approve_mode` → 切换审批模式开关（管理员在场+双重落账）
///
/// 前置硬校验：平台管理员本人（require_administrator——开关决定生产 rollout
/// 是否跳过人工审批，操作者必须是管理员而非持代理权限的角色）+ 人类凭据
/// （service/app key 拒绝）。落账双通道：production_audit（发布域生命周期）
/// + platform.event（平台事件报表）。
#[utoipa::path(
    post,
    path = "/api/publish/approve_mode",
    tag = "workspace",
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "已切换", body = serde_json::Value),
        (status = 400, description = "缺少 enabled(bool)"),
        (status = 401, description = "未认证"),
        (status = 403, description = "非管理员或自动化凭据"),
        (status = 503, description = "workspace 元数据库未接线")
    )
)]
async fn set_approve_mode(
    State(api): State<SessionApi>,
    headers: axum::http::HeaderMap,
    identity: Option<Extension<CallerIdentity>>,
    Json(req): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    // 开关切换 = 人工治理动作，自动化凭据拒绝（与插件审批代理同款前置）
    if matches!(
        identity,
        Some(Extension(CallerIdentity::Service)) | Some(Extension(CallerIdentity::App))
    ) {
        return Err(proxy_err(
            StatusCode::FORBIDDEN,
            "自动化凭据禁止切换审批模式（人工治理动作）".to_string(),
        ));
    }
    let username = crate::api::platform_auth::require_administrator(&api.shared_facts, &headers)?;
    let enabled = req
        .get("enabled")
        .and_then(|v| v.as_bool())
        .ok_or_else(|| proxy_err(StatusCode::BAD_REQUEST, "缺少 enabled(bool)".to_string()))?;
    let db = api.workspace_db.as_ref().ok_or_else(|| {
        proxy_err(
            StatusCode::SERVICE_UNAVAILABLE,
            "workspace 元数据库未接线".to_string(),
        )
    })?;
    db.set_approve_mode(enabled, &username)
        .map_err(|e| proxy_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    // 治理落账：production_audit（生命周期事件, 版本上下文为当前生产版本）
    let prod = db
        .get_production_state()
        .map_err(|e| proxy_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let _ = db.insert_production_audit(
        "approve_mode_changed",
        prod.ruleset_version,
        None,
        prod.ruleset_hash.as_deref().unwrap_or(""),
        prod.current_session_id.unwrap_or(0),
        "[]",
        &username,
        Some(if enabled {
            "approve_mode enabled: submit 直发（跳过人工审批驻留）"
        } else {
            "approve_mode disabled: submit 驻留 pending（人工审批恢复）"
        }),
        None,
        None,
    );
    crate::api::platform_auth::append_platform_event(
        &api.shared_facts,
        "approve_mode_changed",
        serde_json::json!({ "enabled": enabled, "operator": username }),
    );
    Ok(Json(
        serde_json::json!({ "success": true, "approve_mode": enabled }),
    ))
}

/// `POST /api/publish/sessions` → 发布试验会话（fork 自生产会话，不接管生产）
///
/// 纯编排：fork 自当前生产会话当前版本 + 全局发布锁互斥。与 rolling_swap
/// 全量编排区分：不接管 production_state（无原子切换/SSE/drain）——产出
/// 隔离试验会话，风险边界为会话资源本身。
#[utoipa::path(
    post,
    path = "/api/publish/sessions",
    tag = "workspace",
    responses(
        (status = 201, description = "试验会话已创建（fork 自生产会话）", body = serde_json::Value),
        (status = 404, description = "生产会话不存在（内存中）"),
        (status = 409, description = "当前无生产会话（尚未部署规则集）"),
        (status = 503, description = "workspace 元数据库未接线")
    )
)]
async fn publish_start_session(
    State(api): State<SessionApi>,
    State(workspace): State<WorkspaceState>,
    State(metrics): State<SharedMetrics>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)> {
    let db = api.workspace_db.as_ref().ok_or_else(|| {
        proxy_err(
            StatusCode::SERVICE_UNAVAILABLE,
            "workspace 元数据库未接线".to_string(),
        )
    })?;
    let prod = db
        .get_production_state()
        .map_err(|e| proxy_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let parent_id = prod.current_session_id.ok_or_else(|| {
        proxy_err(
            StatusCode::CONFLICT,
            "当前无生产会话（尚未部署规则集），无可 fork 的生产会话".to_string(),
        )
    })? as u64;

    // 全局发布锁互斥: 与 submit/review/rollback 发布动作串行化
    let _guard = workspace.publish_service.publish_lock_guard().await;

    let forked = {
        let sessions = api.sessions.lock().await;
        sessions.create_session_from_parent_at_version(parent_id, None)
    };
    let new_id = match forked {
        Ok(id) => id,
        Err(e) => {
            return Err(proxy_err(
                StatusCode::NOT_FOUND,
                format!("生产会话 fork 失败: {e}"),
            ))
        }
    };
    metrics.inc_sessions();
    // 与 fork 端点同款接线（补 IoSubscriber/归因/回写）
    api.spawn_hit_stats_recorder(new_id);
    api.spawn_writeback_forwarder(new_id);
    let inherited_role = api.declared_caller_role(parent_id);
    api.spawn_io_subscriber_for_session(new_id, metrics.clone(), inherited_role)
        .await;
    // 动作落账: 平台事件（试验会话不接管 production_state, 无 production_audit 版本事件）
    crate::api::platform_auth::append_platform_event(
        &api.shared_facts,
        "publish_session_started",
        serde_json::json!({
            "session_id": new_id,
            "forked_from": parent_id,
            "ruleset_version": prod.ruleset_version,
        }),
    );
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "session_id": new_id,
            "forked_from": parent_id,
            "kind": "publish_trial",
            "ruleset_version": prod.ruleset_version,
            "note": "试验会话不接管生产状态;滚动切换走 rolling_swap 全量编排",
        })),
    ))
}

/// `POST /api/publish/sessions/{id}/promote` → 试验会话晋级生产（窄场景）
///
/// 把 fork 自当前生产会话的试验会话扶正为生产引用。语义对齐自愈重建先例：
/// **替换会话引用而非发布**——ruleset_version/ruleset_hash 原样保留（规则集
/// 内容未变，版本单调性不受扰）；U7 `session_switched` SSE 通知旧生产会话
/// 订阅者；production_audit + platform.event 双落账。
///
/// 窄场景硬约束：目标会话必须是当前生产会话的 fork（`parent_session_id`
/// 对账）——杜绝把内容无关的任意会话扶正造成版本/内容脱钩。
/// 门控：平台管理员本人 + 人类凭据（同 approve_mode）。
#[utoipa::path(
    post,
    path = "/api/publish/sessions/{id}/promote",
    tag = "workspace",
    params(("id" = u64, Path, description = "试验会话 ID")),
    responses(
        (status = 200, description = "已晋级（生产引用换防，版本/哈希不变）", body = serde_json::Value),
        (status = 400, description = "目标不是当前生产会话的 fork（窄场景约束）"),
        (status = 401, description = "未认证"),
        (status = 403, description = "非管理员或自动化凭据"),
        (status = 404, description = "目标会话不存在（内存中）"),
        (status = 409, description = "当前无生产会话，或目标已是生产会话"),
        (status = 503, description = "workspace 元数据库未接线")
    )
)]
async fn publish_promote_session(
    State(api): State<SessionApi>,
    State(workspace): State<WorkspaceState>,
    Path(session_id): Path<u64>,
    headers: axum::http::HeaderMap,
    identity: Option<Extension<CallerIdentity>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    // 晋级 = 人工治理动作，自动化凭据拒绝（与 approve_mode 同款前置）
    if matches!(
        identity,
        Some(Extension(CallerIdentity::Service)) | Some(Extension(CallerIdentity::App))
    ) {
        return Err(proxy_err(
            StatusCode::FORBIDDEN,
            "自动化凭据禁止晋级生产会话（人工治理动作）".to_string(),
        ));
    }
    let username = crate::api::platform_auth::require_administrator(&api.shared_facts, &headers)?;
    let db = api.workspace_db.as_ref().ok_or_else(|| {
        proxy_err(
            StatusCode::SERVICE_UNAVAILABLE,
            "workspace 元数据库未接线".to_string(),
        )
    })?;
    let prod = db
        .get_production_state()
        .map_err(|e| proxy_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let old_pid = prod.current_session_id.ok_or_else(|| {
        proxy_err(
            StatusCode::CONFLICT,
            "当前无生产会话（尚未部署规则集），无可晋级基准".to_string(),
        )
    })? as u64;
    if old_pid == session_id {
        return Err(proxy_err(
            StatusCode::CONFLICT,
            format!("会话 {session_id} 已是生产会话"),
        ));
    }
    {
        let sessions = api.sessions.lock().await;
        let target = sessions.get_session(session_id).ok_or_else(|| {
            proxy_err(
                StatusCode::NOT_FOUND,
                format!("目标会话 {session_id} 不存在（内存中）"),
            )
        })?;
        // 窄场景硬约束: 目标必须是当前生产会话的 fork（防内容无关会话脱钩版本/哈希）
        let parent_ok = target.parent_session_id() == Some(old_pid);
        if !parent_ok {
            return Err(proxy_err(
                StatusCode::BAD_REQUEST,
                format!(
                    "仅支持晋级 fork 自当前生产会话（{old_pid}）的试验会话;目标 {session_id} 的父会话为 {:?}",
                    target.parent_session_id()
                ),
            ));
        }
    }
    // 引用换防: 保留 ruleset_version/ruleset_hash（替换会话引用而非发布）
    db.update_production_state(
        session_id as i64,
        prod.ruleset_version,
        prod.ruleset_hash.as_deref().unwrap_or(""),
        &username,
    )
    .map_err(|e| proxy_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    // U7 SSE: 通知旧生产会话订阅者跟随切换
    let _ = workspace
        .switcher
        .broadcast_switched(
            old_pid,
            session_id,
            prod.ruleset_version,
            prod.ruleset_hash.as_deref().unwrap_or(""),
        )
        .await;
    // 双落账: production_audit（生命周期事件）+ platform.event
    let _ = db.insert_production_audit(
        "production_session_promoted",
        prod.ruleset_version,
        None,
        prod.ruleset_hash.as_deref().unwrap_or(""),
        session_id as i64,
        "[]",
        &username,
        Some(&format!(
            "promoted trial session {session_id} (fork of {old_pid}); ruleset unchanged"
        )),
        None,
        None,
    );
    crate::api::platform_auth::append_platform_event(
        &api.shared_facts,
        "production_session_promoted",
        serde_json::json!({
            "session_id": session_id,
            "previous_production_session": old_pid,
            "ruleset_version": prod.ruleset_version,
            "operator": username,
        }),
    );
    Ok(Json(serde_json::json!({
        "promoted": true,
        "session_id": session_id,
        "previous_production_session": old_pid,
        "ruleset_version": prod.ruleset_version,
        "note": "生产引用换防完成;旧会话保留(不再受生产保护,空闲回收照常)",
    })))
}

/// 会话锚点验证 handler（G-A2）
///
/// `POST /api/sessions/:id/anchors/verify` → 验证锚点链结构+签名+与事实链绑定
///
/// 请求体（可选）：`{"public_keys": {"key_id": "<hex 公钥>"}}`；
/// 未提供 public_keys 时使用 server 本部署面配置的公钥（同一 seed 派生）。
#[utoipa::path(
    post,
    path = "/api/sessions/{id}/anchors/verify",
    tag = "sessions",
    params(("id" = u64, Path, description = "会话 ID")),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "验证结果（valid/anchor_count/错误明细）"),
        (status = 404, description = "会话不存在")
    )
)]
async fn session_anchor_verify(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
    body: Option<Json<serde_json::Value>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let external_keys: BTreeMap<String, String> = match body {
        Some(Json(v)) => v
            .get("public_keys")
            .and_then(|k| k.as_object())
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default(),
        None => BTreeMap::new(),
    };
    let keys = if external_keys.is_empty() {
        // 未提供时用部署面公钥（anchor_setup 配置的同 seed 派生）
        crate::anchor_setup::deployment_public_keys().unwrap_or_default()
    } else {
        external_keys
    };
    let sessions = api.sessions.lock().await;
    sessions.touch_session(session_id);
    let session = sessions
        .get_session(session_id)
        .ok_or(StatusCode::NOT_FOUND)?;
    let _new_count = session.audit_new();
    let anchors = session.anchors();
    let result = session.verify_anchors(&keys);
    drop(sessions);
    // P0-3（G4 修复）：空锚点链语义显式化——0 锚点时 valid 置 null（客户端
    // 不再把"什么都没验"读成"验过了"）；anchor_count 保留供旧客户端判断
    let anchor_status = if anchors.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::Bool(result.is_ok())
    };
    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "valid": anchor_status,
        "anchor_count": anchors.len(),
        "error": result.err(),
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

    let session = api.session_or_rehydrate(session_id).await.map_err(|rej| {
        tracing::warn!(session_id, rejection = %rej, "会话读侧获取失败（含复活尝试）");
        rej.to_status()
    })?;

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
    let session = api.session_or_rehydrate(session_id).await.map_err(|rej| {
        tracing::warn!(session_id, rejection = %rej, "会话读侧获取失败（含复活尝试）");
        rej.to_status()
    })?;

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
/// - `200 OK`：导入成功（验证失败不换状态码，以响应体 `verify_ok=false` /
///   `status="verify_failed"` 表达，数据可能已损坏）
/// - `400 Bad Request`：JSON 解析失败或字段缺失
/// - `404 Not Found`：会话不存在
#[utoipa::path(

    post,

    path = "/api/sessions/{id}/audit/import",

    tag = "sessions",

    params(("id" = u64, Path, description = "会话 ID")),

    request_body = serde_json::Value,

    responses(

        (status = 200, description = "导入成功（验证失败以响应体 verify_ok/status 字段表达）", body = AuditImportResponse),

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
    let session = api.session_or_rehydrate(session_id).await.map_err(|rej| {
        tracing::warn!(session_id, rejection = %rej, "会话读侧获取失败（含复活尝试）");
        rej.to_status()
    })?;

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

        (status = 200, description = "导入成功（验证失败以响应体 verify_ok/status 字段表达）", body = AuditImportResponse),

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
#[allow(clippy::too_many_arguments)]
async fn session_payload(
    State(api): State<SessionApi>,

    State(shared_facts): State<SharedFactsLog>,

    State(metrics): State<SharedMetrics>,

    State(sanitizer): State<Arc<InputSanitizer>>,

    Path(session_id): Path<u64>,

    identity: Option<Extension<CallerIdentity>>,

    gate: Option<Extension<ServiceGateArmed>>,

    Json(req): Json<PayloadUpdateRequest>,
) -> Result<(StatusCode, Json<ApiResponse>), StatusCode> {
    let id = api.next_id();

    // 受保护域准入——与 update_payload 同源三态语义（守卫武装=凭有效
    // service token；未武装=User/App 拒绝、认证禁用且未配置 service
    // token 放行），判定逻辑见 protected_write_denied。
    if requires_service_identity(&req.path) && protected_write_denied(&identity, &gate) {
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

                code: Some("UNAUTHORIZED".to_string()),
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
    //
    // R10 时序交换：先 send（会话侧事实确定入链）成功后，才广播并携带
    // origin_fact_id=Some(id)——origin 永不悬空（共享侧条目必然指向已入链
    // 的会话侧事实）。广播仍 best-effort：失败仅 warn，不影响主流程。
    // 行为变化（相对旧时序）：reactor 已退出（send 失败）时不再向共享账本
    // 写入指向未入链事实的幽灵条目；HTTP 响应语义不变。

    let send_result = session.command_tx.send(Fact::PayloadUpdate {
        id,

        path: req.path.clone(),

        value: value.clone(),
    });

    if send_result.is_ok() && req.path.starts_with("shared.") {
        if let Err(e) = shared_facts.append_with_origin(&req.path, value, session_id, Some(id.0)) {
            tracing::warn!(session_id, path = %req.path, "SharedFactsLog append failed: {e}");
        }
    }

    match send_result {
        Ok(()) => Ok((
            StatusCode::OK,
            Json(ApiResponse {
                success: true,

                message: "PayloadUpdate submitted".to_string(),

                fact_id: Some(id.0),

                code: None,
            }),
        )),

        Err(_) => Ok((
            StatusCode::OK,
            Json(ApiResponse {
                success: false,

                message: "Command channel closed (reactor exited)".to_string(),

                fact_id: None,

                code: Some("SESSION_ERROR".to_string()),
            }),
        )),
    }
}

/// 批量端点受保护域预校验（纯函数）：受限身份写入 stable.llm / stable.system
/// 受保护域时返回 Some((命中条数, 首个命中路径))，供整批拒绝响应构造；None = 放行。
/// 受限身份判定走 [`protected_write_denied`]（守卫武装态下无有效 service
/// 凭据即受限，未武装态下 User/App 受限）。
fn batch_protected_offense(
    updates: &[PayloadUpdateRequest],
    identity_limited: bool,
) -> Option<(usize, String)> {
    if !identity_limited {
        return None;
    }
    let offending: Vec<String> = updates
        .iter()
        .filter(|u| requires_service_identity(&u.path))
        .map(|u| u.path.clone())
        .collect();
    let first = offending.first()?.clone();
    Some((offending.len(), first))
}

/// 批量单条更新处理（与单条端点同语义）：净化 → 指标 → 发送 → shared 旁路 → 逐条结果
fn process_batch_update(
    api: &SessionApi,
    session: &session::Session,
    shared_facts: &SharedFactsLog,
    metrics: &SharedMetrics,
    sanitizer: &Arc<InputSanitizer>,
    session_id: u64,
    update: &PayloadUpdateRequest,
) -> BatchPayloadItemResult {
    // Phase 1: 输入净化（与单条端点同语义，静默改写 + 命中指标化）
    let (sanitized_value, sanitize_report) = sanitizer.sanitize_value(&update.value);
    sanitize_report
        .unique_hits()
        .into_iter()
        .for_each(|rule| metrics.inc_sanitize_hits(rule));
    let value = serde_to_tcb(sanitized_value);
    let id = api.next_id();

    // R10 时序交换（与单条端点同语义）：先 send 后广播，origin 永不悬空
    let send_result = session.command_tx.send(Fact::PayloadUpdate {
        id,
        path: update.path.clone(),
        value: value.clone(),
    });
    if send_result.is_ok() && update.path.starts_with("shared.") {
        if let Err(e) = shared_facts.append_with_origin(&update.path, value, session_id, Some(id.0))
        {
            tracing::warn!(
                session_id,
                path = %update.path,
                "batch: SharedFactsLog append failed: {e}"
            );
        }
    }

    let ok = send_result.is_ok();
    BatchPayloadItemResult {
        path: update.path.clone(),
        success: ok,
        fact_id: ok.then_some(id.0),
        error: (!ok).then(|| "SESSION_ERROR: command channel closed (reactor exited)".to_string()),
    }
}

/// 会话批量 PayloadUpdate handler（跨仓挂账二）
///
/// `POST /api/sessions/:id/payloads` → 一次 HTTP 写入多条 payload。
///
/// 原子性语义（诚实契约）：
/// - **预校验整批拒绝**：空批次 400；任一条目命中受保护域且身份不足 403
///   （列出违规路径）；会话不存在 404——拒绝发生在任何写入之前；
/// - **执行期逐条上报**：command_tx 关闭（reactor 退出）等执行期失败
///   不回滚已写条目，逐条 `results[i]` 携带 success/fact_id/error；
/// - `success=true` 当且仅当全部条目成功。
#[utoipa::path(

    post,

    path = "/api/sessions/{id}/payloads",

    tag = "sessions",

    params(("id" = u64, Path, description = "会话 ID")),

    request_body = BatchPayloadUpdateRequest,

    responses(

        (status = 200, description = "批量已处理（逐条结果见 results）", body = BatchPayloadResponse),

        (status = 400, description = "空批次"),

        (status = 403, description = "任一条目命中受保护域且身份不足（整批拒绝，未写入任何条目）"),

        (status = 404, description = "会话不存在")

    )

)]
#[allow(clippy::too_many_arguments)]
async fn batch_session_payload(
    State(api): State<SessionApi>,

    State(shared_facts): State<SharedFactsLog>,

    State(metrics): State<SharedMetrics>,

    State(sanitizer): State<Arc<InputSanitizer>>,

    Path(session_id): Path<u64>,

    identity: Option<Extension<CallerIdentity>>,

    gate: Option<Extension<ServiceGateArmed>>,

    Json(req): Json<BatchPayloadUpdateRequest>,
) -> Result<(StatusCode, Json<BatchPayloadResponse>), (StatusCode, Json<BatchPayloadResponse>)> {
    if req.updates.is_empty() {
        tracing::warn!(session_id, "batch payload rejected: empty updates");
        return Err((
            StatusCode::BAD_REQUEST,
            Json(BatchPayloadResponse {
                success: false,
                message: "updates 不能为空".to_string(),
                total: 0,
                succeeded: 0,
                failed: 0,
                results: Vec::new(),
            }),
        ));
    }

    // 预校验：受保护域整批拒绝（任何写入发生之前）——与 session_payload
    // 的单条语义同源（stable.llm / stable.system 仅受信服务管道；守卫
    // 武装态下无有效 service 凭据即受限）
    let identity_limited = protected_write_denied(&identity, &gate);
    if let Some((offending_count, first_path)) =
        batch_protected_offense(&req.updates, identity_limited)
    {
        tracing::warn!(
            session_id,
            count = offending_count,
            "batch_session_payload 受保护域写入被整批拒绝（需 service 身份）"
        );
        let failed = req.updates.len();
        return Err((
            StatusCode::FORBIDDEN,
            Json(BatchPayloadResponse {
                success: false,
                message: format!(
                    "批量含 {} 条受保护域写入（如 {}）：stable.llm / stable.system 仅受信服务管道可写。整批拒绝，未写入任何条目。",
                    offending_count, first_path
                ),
                total: failed,
                succeeded: 0,
                failed,
                results: req
                    .updates
                    .iter()
                    .map(|u| BatchPayloadItemResult {
                        path: u.path.clone(),
                        success: false,
                        fact_id: None,
                        error: Some(
                            "UNAUTHORIZED: protected domain requires service identity".to_string(),
                        ),
                    })
                    .collect(),
            }),
        ));
    }

    let sessions = api.sessions.lock().await;

    sessions.touch_session(session_id);

    let session = sessions.get_session(session_id).ok_or((
        StatusCode::NOT_FOUND,
        Json(BatchPayloadResponse {
            success: false,
            message: format!("session {session_id} not found"),
            total: req.updates.len(),
            succeeded: 0,
            failed: req.updates.len(),
            results: Vec::new(),
        }),
    ))?;

    let mut results = Vec::with_capacity(req.updates.len());
    for update in &req.updates {
        results.push(process_batch_update(
            &api,
            &session,
            &shared_facts,
            &metrics,
            &sanitizer,
            session_id,
            update,
        ));
    }

    let total = results.len();
    let succeeded = results.iter().filter(|r| r.success).count();
    let failed = total - succeeded;
    tracing::info!(
        session_id,
        total,
        succeeded,
        failed,
        "Batch payload processed"
    );

    Ok((
        StatusCode::OK,
        Json(BatchPayloadResponse {
            success: failed == 0,
            message: format!("batch processed: {succeeded}/{total} succeeded"),
            total,
            succeeded,
            failed,
            results,
        }),
    ))
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
        let session = api.session_or_rehydrate(session_id).await.map_err(|rej| {
            tracing::warn!(session_id, rejection = %rej, "会话读侧获取失败（含复活尝试）");
            rej.to_status()
        })?;

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
    let session = api.session_or_rehydrate(session_id).await.map_err(|rej| {
        tracing::warn!(session_id, rejection = %rej, "会话读侧获取失败（含复活尝试）");
        rej.to_status()
    })?;

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
    /// enforce 强制拦截（回归验证，记录性事实；不推进版本号）
    Violation {
        /// Fact ID
        id: u64,
        /// 版本号（FactsLog 中的版本；violation 不推进版本）
        version: u64,
        /// 触发此拦截的源事实 ID
        cause: u64,
        /// 命中的 enforce 规则在合并规则列表中的下标
        rule_index: u64,
        /// 违规说明（enforce params.reason）
        reason: String,
        /// 被拒绝执行的违规指令
        instruction: serde_json::Value,
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
    /// 域判定归因（仅 branch/enforce 求值成功时出现，其余省略；
    /// 专项-20261001 三态域判定：sat/unsat/missing 归因 + on_missing 声明值）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain_attr: Option<DomainAttrDto>,
}

/// 域判定归因（branch/enforce 专属；专项-20261001 方案 2' v4）
///
/// 口径（与 `hit` 正交）：`hit` = 结构命中（有无生效路径）；
/// `domain_attr.outcome` = 域条件三态求值结果；`on_missing` = 规则文本
/// 静态声明值。R1 纪律：归因仅作审计用途，不回灌执行。
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DomainAttrDto {
    /// 域判定结果："sat" / "unsat" / "missing"
    pub outcome: String,
    /// Missing 分类（仅 outcome="missing" 时出现）："path_not_found" /
    /// "incomparable" / "value_literal_ambiguous"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing_reason: Option<String>,
    /// 规则文本 `on_missing` 声明值："error" / "unsat"；未声明时省略
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_missing: Option<String>,
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
                    domain_attr: h.domain_attr.as_ref().map(|a| DomainAttrDto {
                        outcome: a.outcome.clone(),
                        missing_reason: a.missing_reason.clone(),
                        on_missing: a.on_missing.clone(),
                    }),
                })
                .collect(),
        },
        // enforce 强制拦截事实（回归验证，记录性事实，不推进版本）
        Fact::Violation {
            id,
            cause,
            rule_index,
            reason,
            instruction,
        } => FactEnvelope::Violation {
            id: id.0,
            version,
            cause: cause.0,
            rule_index: *rule_index,
            reason: reason.clone(),
            instruction: tcb_to_serde(instruction),
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
    let session = api.session_or_rehydrate(session_id).await.map_err(|rej| {
        tracing::warn!(session_id, rejection = %rej, "会话读侧获取失败（含复活尝试）");
        rej.to_status()
    })?;

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
    let session = api.session_or_rehydrate(session_id).await.map_err(|rej| {
        tracing::warn!(session_id, rejection = %rej, "会话读侧获取失败（含复活尝试）");
        rej.to_status()
    })?;

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
    // o31 原发点：MemoryTab 共享事实查询——内存未命中时必须走惰性复活，
    // 引擎重启后历史会话的 facts 索引才不至于直取 404
    let session = api.session_or_rehydrate(session_id).await.map_err(|rej| {
        tracing::warn!(session_id, rejection = %rej, "会话读侧获取失败（含复活尝试）");
        rej.to_status()
    })?;

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

                "origin_fact_id": sf.origin_fact_id,

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

        "origin_fact_id": fact.origin_fact_id,

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

        code: None,
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
///
/// R11（静默失败修复）：空批次 → 400；含不存在的 fact_id → 404
/// 并逐个列出（典型原因：误用写入响应的 payload 侧 fact_id——与共享表
/// ID 空间不通用）。修复前任何输入都无条件返回 200，错 ID 空间静默无效。
#[utoipa::path(

    post,

    path = "/api/shared/facts/rollup",

    tag = "shared-facts",

    request_body = FactIdsRequest,

    responses(

        (status = 200, description = "标记成功，返回标记数量", body = ApiResponse),

        (status = 400, description = "fact_ids 为空"),

        (status = 404, description = "存在共享账本中不存在的 fact_id（逐个列出）")

    )

)]

async fn shared_facts_rollup(
    State(shared_facts): State<SharedFactsLog>,

    Json(req): Json<FactIdsRequest>,
) -> Result<(StatusCode, Json<ApiResponse>), (StatusCode, Json<ApiResponse>)> {
    if req.fact_ids.is_empty() {
        tracing::warn!("rollup rejected: empty fact_ids");
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiResponse {
                success: false,
                message: "fact_ids 不能为空".to_string(),
                fact_id: None,
                code: status_error_code(StatusCode::BAD_REQUEST).map(str::to_string),
            }),
        ));
    }

    let fact_ids: Vec<FactId> = req.fact_ids.into_iter().map(FactId).collect();

    let outcome = shared_facts.mark_as_rollup(&fact_ids);

    if !outcome.unknown.is_empty() {
        // 不存在的 id 必须显式失败：静默 200 会让调用方误以为标记成功，
        // 旧摘要永不失效 → 反复 rollup 膨胀（实测确认的事故形态）
        tracing::warn!(
            unknown = ?outcome.unknown,
            marked = outcome.marked,
            "rollup: request contains fact_ids absent from the shared ledger \
             (likely payload-side fact_id misuse — the two ID spaces are not interchangeable)"
        );
        let show: Vec<String> = outcome
            .unknown
            .iter()
            .take(20)
            .map(|id| id.0.to_string())
            .collect();
        let ellipsis = if outcome.unknown.len() > 20 {
            ", ..."
        } else {
            ""
        };
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiResponse {
                success: false,
                message: format!(
                    "{} 个 fact_id 在共享账本中不存在（常见原因：误用 payload 侧 fact_id，两个 ID 空间不通用）；已标记 {} 个；未知 id: [{}{}]",
                    outcome.unknown.len(),
                    outcome.marked,
                    show.join(", "),
                    ellipsis
                ),
                fact_id: None,
                code: status_error_code(StatusCode::NOT_FOUND).map(str::to_string),
            }),
        ));
    }

    tracing::info!(
        fact_count = outcome.marked,
        "Marked shared facts as rolled up"
    );

    Ok((
        StatusCode::OK,
        Json(ApiResponse {
            success: true,
            message: format!("{} facts marked as rolled up", outcome.marked),
            fact_id: None,
            code: None,
        }),
    ))
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
    let session = api.session_or_rehydrate(session_id).await.map_err(|rej| {
        tracing::warn!(session_id, rejection = %rej, "会话读侧获取失败（含复活尝试）");
        rej.to_status()
    })?;

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
    let _session = api.session_or_rehydrate(session_id).await.map_err(|rej| {
        tracing::warn!(session_id, rejection = %rej, "会话读侧获取失败（含复活尝试）");
        rej.to_status()
    })?;

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
    let session = api.session_or_rehydrate(session_id).await.map_err(|rej| {
        tracing::warn!(session_id, rejection = %rej, "会话读侧获取失败（含复活尝试）");
        rej.to_status()
    })?;

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
    let session = api.session_or_rehydrate(session_id).await.map_err(|rej| {
        tracing::warn!(session_id, rejection = %rej, "会话读侧获取失败（含复活尝试）");
        rej.to_status()
    })?;

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
    let session = api.session_or_rehydrate(session_id).await.map_err(|rej| {
        tracing::warn!(session_id, rejection = %rej, "会话读侧获取失败（含复活尝试）");
        rej.to_status()
    })?;

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
    let session = api.session_or_rehydrate(session_id).await.map_err(|rej| {
        tracing::warn!(session_id, rejection = %rej, "会话读侧获取失败（含复活尝试）");
        rej.to_status()
    })?;

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
    let session = api.session_or_rehydrate(session_id).await.map_err(|rej| {
        tracing::warn!(session_id, rejection = %rej, "会话读侧获取失败（含复活尝试）");
        rej.to_status()
    })?;

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
    let session = api.session_or_rehydrate(session_id).await.map_err(|rej| {
        tracing::warn!(session_id, rejection = %rej, "会话读侧获取失败（含复活尝试）");
        rej.to_status()
    })?;

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
    let session = api.session_or_rehydrate(session_id).await.map_err(|rej| {
        tracing::warn!(session_id, rejection = %rej, "会话读侧获取失败（含复活尝试）");
        rej.to_status()
    })?;

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
    let session = api.session_or_rehydrate(session_id).await.map_err(|rej| {
        tracing::warn!(session_id, rejection = %rej, "会话读侧获取失败（含复活尝试）");
        rej.to_status()
    })?;

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

        (status = 404, description = "会话不存在"),

        (status = 422, description = "输出门禁拒绝（enforce 模式命中绕工具特征，error=IO_GUARD_REJECTED）")

    )

)]

async fn session_io_response(
    State(api): State<SessionApi>,

    Path(session_id): Path<u64>,

    Json(req): Json<IoResponseRequest>,
) -> Result<Json<ApiResponse>, (StatusCode, Json<serde_json::Value>)> {
    let id = api.next_id();

    let request_id = evorule_reactor::FactId(req.request_id);

    let result_tcb = serde_to_tcb(req.result.clone());

    let sessions = api.sessions.lock().await;

    sessions.touch_session(session_id);

    let session = sessions.get_session(session_id).ok_or((
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error": "session not found"})),
    ))?;

    // 输出门禁（io_guard）：最终应答绕工具行为封堵。仅 is_finished=true 分支
    // 触发；工具结果回喂路径与错误应答不触。observe=只落 Violation 账不阻断
    // （缺省）；enforce=落账并以 422 拒绝（evo-agent 感知 422 后纠偏回喂重试）。
    let io_guard_mode = super::io_guard::guard_mode();

    if io_guard_mode != super::io_guard::IoGuardMode::Off {
        if let Some(hit) = super::io_guard::detect_final_response_violation(
            &api.rules_dir,
            &session.facts_log,
            &req.result,
            req.error.as_deref(),
        ) {
            let reason = format!(
                "输出门禁命中：收尾文本含「{}」动作特征（{} 域），但本会话未调用该域任何工具",
                hit.phrase, hit.domain
            );

            let violation_id = api.next_id();

            let instruction = serde_to_tcb(serde_json::json!({
                "type": "io_guard",
                "params": {
                    "domain": hit.domain,
                    "phrase": hit.phrase,
                    "request_id": req.request_id,
                    "mode": format!("{io_guard_mode:?}").to_lowercase(),
                }
            }));

            // Violation 为记录性事实（不 bump version）；rule_index=u64::MAX
            // 为保留值，标记特征表命中（区别于规则层 enforce 的规则下标）；
            // cause 指向被拒的 IoResponse 事实
            let _ = session.facts_log.append(evorule_reactor::Fact::Violation {
                id: violation_id,
                cause: request_id,
                rule_index: u64::MAX,
                reason: reason.clone(),
                instruction,
            });

            tracing::info!(
                session_id,
                request_id = req.request_id,
                domain = %hit.domain,
                mode = ?io_guard_mode,
                "io_guard hit on final response"
            );

            // 外判立法（部署面 v0.8.0）：确定性外判域命中时，经
            // externalization_violation 记录通道把违规事实写进 payload 审计链
            // （评审面可检测；与 tool_trace 同通道；运行时 Violation 账不受影响）
            if hit.domain == super::io_guard::EXTERNALIZATION_DOMAIN {
                let breach_id = api.next_id();
                let breach_instruction = serde_to_tcb(serde_json::json!({
                    "type": "externalization_violation",
                    "params": {
                        "attr": "meta_compliance.externalization_breach",
                        "value": {
                            "domain": hit.domain,
                            "phrase": hit.phrase,
                            "request_id": req.request_id,
                            "mode": format!("{io_guard_mode:?}").to_lowercase(),
                        }
                    }
                }));
                let _ = session.command_tx.send(evorule_reactor::Fact::Command {
                    id: breach_id,
                    instruction: breach_instruction,
                });
            }

            if io_guard_mode == super::io_guard::IoGuardMode::Enforce {
                return Err((
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(serde_json::json!({
                        "error": "IO_GUARD_REJECTED",
                        "code": "IO_GUARD_REJECTED",
                        "message": reason,
                        "domain": hit.domain,
                    })),
                ));
            }
        }
    }

    match session.command_tx.send(Fact::IoResponse {
        id,

        request_id,

        result: result_tcb,

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

                code: None,
            }))
        }

        Err(_) => Ok(Json(ApiResponse {
            success: false,

            message: "Command channel closed (reactor exited)".to_string(),

            fact_id: None,

            code: Some("SESSION_ERROR".to_string()),
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

    /// 是否启用高危干预端点族（POST /api/sessions/{id}/gc 等，--allow-interventions）。
    /// 默认关闭：即使认证通过也不注册该路由（双保险，返回 404）。
    allow_interventions: bool,

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

        allow_interventions: bool,

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

            allow_interventions,

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
            // 高危干预端点族同样默认关闭（双保险，需显式 --allow-interventions）
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
            // 契约固化：IO 形状契约导出——skip 谓词真相源的
            // 机读形态，供 evo-agent 启动协商与双侧交叉锁测。免认证定位同
            // openapi.json（仅契约元数据，无业务数据）。
            .route(
                "/api/io-contract",
                get(crate::api::io_contract::io_contract_endpoint),
            )
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

        // 高危干预端点双保险（--allow-interventions）：gc 压缩改变链可见历史
        // 且生产无先例；cancel_all 丢弃在途 I/O——两者默认不注册路由（404）；
        // heartbeat/pause/resume/scan/wal_rotate 可逆或只读，走
        // protected_routes 常规认证即可。
        let interventions_router = if self.allow_interventions {
            Router::new()
                .route("/api/sessions/{id}/gc", post(session_gc))
                .route(
                    "/api/sessions/{id}/io/cancel_all",
                    post(session_io_cancel_all),
                )
        } else {
            Router::new()
        };

        // 受保护路由（需认证）

        let protected_routes = Router::new()
            // 单反应器模式路由（向后兼容）
            .route("/api/command", post(submit_command))
            // 服务直调：与 io_request 同一处理器链，属状态变更执行面——
            // 必须受认证保护；敏感服务另有 403 守卫（须走会话审计链）。
            .route("/api/services/{name}/invoke", post(invoke_service_handler))
            // 插件审批代理:external 插件管理面的 server 侧统一入口(平台认证保护,
            // approver 强制注入登录身份)——console 插件审批面的后端通道
            .route(
                "/api/plugins/{id}/admin/proposals",
                get(plugin_admin_list_proposals),
            )
            .route(
                "/api/plugins/{id}/admin/proposals/{pid}/approve",
                post(plugin_admin_approve),
            )
            .route(
                "/api/plugins/{id}/admin/proposals/{pid}/reject",
                post(plugin_admin_reject),
            )
            // 插件契约 v1 资产面（只读 + 纯函数生成,R3:不落库不进治理状态;
            // 草稿生效仍走既有 Draft→Publish 链）——console 通用表单的后端通道
            .route(
                "/api/plugins",
                get(crate::api::plugin_packs::list_plugins_handler),
            )
            .route(
                "/api/plugins/{pack_id}/assets/{kind}",
                get(crate::api::plugin_packs::plugin_assets_handler),
            )
            .route(
                "/api/plugins/templates/{pack_id}/{template_id}/generate",
                post(crate::api::plugin_packs::generate_template_handler),
            )
            // 契约 v1.1 流程编译代理（draft-only,R3;R2 等价性门禁为强制位）
            .route(
                "/api/plugins/flows/{pack_id}/{flow_id}/compile",
                post(crate::api::plugin_packs::compile_flow_handler),
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
            .route("/api/sessions/{id}/anchors", get(session_anchors))
            .route("/api/sessions/{id}/anchors/seal", post(session_anchor_seal))
            .route(
                "/api/sessions/{id}/anchors/verify",
                post(session_anchor_verify),
            )
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
            .route("/api/sessions/{id}/payloads", post(batch_session_payload))
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
            // L2 约束（元规则）只读清单投影（LLM 代理消费面，需认证）
            .route("/api/rules/l2-inventory", get(l2_inventory_handler))
            .route(
                "/api/sessions/{id}/evolution-signals",
                get(session_evolution_signals),
            )
            // 规则命中统计查询面（聚合器数据，需认证）
            .route("/api/rules/hit-stats", get(hit_stats_handler))
            .route(
                "/api/rules/hit-stats/{rule_key}",
                get(hit_stats_rule_handler),
            )
            // T2: 快照包导入端点（历史批次 集成契约）——写 rules_dir 的运营操作，走受保护路由
            .route(
                "/api/bundles/import",
                post(crate::api::bundles::import_bundle_handler),
            )
            .route(
                "/api/bundles/import/dry-run",
                post(crate::api::bundles::import_bundle_dry_run_handler),
            )
            // 快照包导出端点（workspace 治理域带证据导出，部署链上游）——
            // 与 import/dry-run 构成部署闭环；导出不落盘不激活；受保护路由
            .route(
                "/api/bundles/export",
                post(crate::api::bundles::export_bundle_handler),
            )
            .route(
                "/api/workspaces/{id}/bundle-sync",
                post(crate::api::server::bundle_sync_handler),
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
            // 模板渲染（{{}} 家族语法 if/for 最小集;与 NativeService template_render
            // 共用引擎 SSOT;纯函数无落库;受认证保护）
            .merge(crate::api::template_render::template_render_router())
            // P10: 工作空间 + 规则元数据路由 (18 个端点, 受认证保护)
            .merge(evorule_workspace::build_workspace_router().layer(
                axum::middleware::from_fn_with_state(
                    self.state.shared_facts.clone(),
                    publish_binding_middleware,
                ),
            ))
            // 工作空间自助加入(身份服务端注入,viewer 最小权限,幂等;
            // WorkspaceState 经 FromRef 从 AppState 派生)
            .route("/api/workspaces/{id}/members/join", post(workspace_join))
            // abort 双保险：条件挂载（--allow-abort 关闭时为空 Router）
            .merge(abort_router)
            // 高危干预双保险：条件挂载（--allow-interventions 关闭时为空 Router）
            .merge(interventions_router)
            // 会话级干预端点（可逆/只读，常规认证）：
            // heartbeat 限频注入（监控信号，pause 放行面）、
            // pause/resume 准入闸（拒新 Command，在途队列与 io_response 不受影响）
            .route("/api/sessions/{id}/heartbeat", post(session_heartbeat))
            .route(
                "/api/sessions/{id}/pause",
                get(session_pause_status).post(session_pause),
            )
            .route("/api/sessions/{id}/resume", post(session_resume))
            // 不变式扫描（读引擎自检报告；检出违规落 Fact::Violation）与
            // WAL 手动轮换（纯存储操作，崩溃安全）——可逆/只读，常规认证
            .route(
                "/api/sessions/{id}/invariants/scan",
                post(session_invariants_scan),
            )
            .route("/api/sessions/{id}/wal/rotate", post(session_wal_rotate))
            // 全通道 WAL checkpoint（治理面运维端点，管理员硬校验在 handler 内）
            .route("/api/admin/wal/checkpoint", post(admin_wal_checkpoint))
            // 发布域治理端点（approve_mode 切换须管理员在场+双落账;
            // start_session 纯编排 fork 试验会话, 不接管生产状态）
            .route(
                "/api/publish/approve_mode",
                get(get_approve_mode).post(set_approve_mode),
            )
            .route("/api/publish/sessions", post(publish_start_session))
            .route(
                "/api/publish/sessions/{id}/promote",
                post(publish_promote_session),
            )
            // rewind/diff 已移至 application/core/time_machine（本地实现）
            // W2b:统一认证中间件(双凭据:静态 user/service token 或
            // 平台会话 token;401 统一 JSON 错误体)。evo-agent 侧车审计桥等
            // 内部调用方沿用静态 service token,无需改造。
            // State 增补 AppQuotaManager(per-app 配额检查)
            .layer(axum::middleware::from_fn_with_state(
                (
                    auth,
                    self.state.shared_facts.clone(),
                    self.state.app_quota.clone(),
                ),
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
            // State 增补 AppQuotaManager(与 protected_routes 同构)
            metrics_router.layer(axum::middleware::from_fn_with_state(
                (
                    self.auth.clone(),
                    self.state.shared_facts.clone(),
                    self.state.app_quota.clone(),
                ),
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

                // i18n 阶段2 续：code 透传中间件置于最外层（含 fallback/静态资源 404）
                router
                    .layer(axum::middleware::from_fn(inject_error_code_middleware))
                    .with_state(self.state.clone())
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

                // i18n 阶段2 续：code 透传中间件置于 GovernorLayer 之外（governor 429 也能带 code）
                router
                    .layer(tower_governor::GovernorLayer::new(cfg))
                    .layer(axum::middleware::from_fn(inject_error_code_middleware))
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

/// 单层规则清单项（层级可观测，设计方案 §2.1）
#[derive(Debug, Serialize, ToSchema)]

pub struct RuleTierEntry {
    /// 层级标签：L1_core_eval=宪法层（仅用户特批可改）| L2_meta=元规则层（rules_dir 根目录 00_meta_*.json，仅治理链晋升可写）| L3_business=业务层（其余全部，LLM 补丁热加载落点）
    pub tier: String,

    /// 该层文件数
    pub files: usize,

    /// 文件路径列表（L1 为 core_eval 文件绝对路径；L2/L3 为相对 rules_dir 的 / 归一化路径）
    pub paths: Vec<String>,
}

/// 当前生效规则响应

#[derive(Debug, Serialize, ToSchema)]

pub struct RulesResponse {
    /// 规则数量
    pub count: usize,

    /// 当前生效的 transform 规则列表（core_eval）
    pub core_eval: Vec<serde_json::Value>,

    /// 三层规则清单（L1 宪法 / L2 元规则 / L3 业务；分层为纯约定，执行顺序由"core_eval 在前 + 完整路径字典序"保证）
    pub tiers: Vec<RuleTierEntry>,
}

/// L2 约束（元规则）清单项（只读投影：metadata.title + metadata.guard_for +
/// 晋升账三字段，不含任何执行语义）
#[derive(Debug, Serialize, ToSchema)]

pub struct L2InventoryEntry {
    /// 相对 rules_dir 的文件路径（'/' 归一）
    pub path: String,

    /// metadata.title（缺省空串）
    pub title: String,

    /// 守卫的指令类型清单（metadata.guard_for，缺省空数组）
    pub guard_for: Vec<String>,

    /// 晋升源锚（metadata.promoted_from，形如 "rule_version:<版本id>"；非晋升条目为 null）
    pub promoted_from: Option<String>,

    /// 晋升时间（metadata.promoted_at；非晋升条目为 null）
    pub promoted_at: Option<String>,

    /// 晋升操作方（metadata.promoted_by；非晋升条目为 null）
    pub promoted_by: Option<String>,
}

/// L2 约束规则清单响应
#[derive(Debug, Serialize, ToSchema)]

pub struct L2InventoryResponse {
    /// 投影文件数
    pub count: usize,

    /// 按文件名排序的 L2 清单
    pub files: Vec<L2InventoryEntry>,
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

    // 三层清单随响应返回（只读扫描，运维一眼核对层级是否被篡改）
    let tiers = SessionApi::tier_inventory(&api.core_eval_path, &api.rules_dir);

    Ok(Json(RulesResponse {
        count: core_eval.len(),

        core_eval: core_eval_serde,

        tiers,
    }))
}

/// 获取 L2 约束（元规则）只读清单投影（GET /api/rules/l2-inventory，014 合法 API 家族）
///
/// 扫描口径与 `/api/rules` 的三层清单 L2 层单一权威同源（见 [`SessionApi::l2_inventory`]）；
/// 供 LLM 代理等消费方生成规则草稿前读取约束边界摘要。
#[utoipa::path(

    get,

    path = "/api/rules/l2-inventory",

    tag = "rules",

    responses(

        (status = 200, description = "L2 约束规则清单", body = L2InventoryResponse)

    )

)]

async fn l2_inventory_handler(State(api): State<SessionApi>) -> Json<L2InventoryResponse> {
    Json(SessionApi::l2_inventory(&api.rules_dir))
}

/// 进化信号查询参数（`GET /api/sessions/{id}/evolution-signals`）
#[derive(Debug, Deserialize)]
struct EvolutionSignalsParams {
    /// 信号条数上限（count 降序截断；0 = 不限；缺省 20）
    limit: Option<usize>,
}

/// 获取会话进化信号（GET /api/sessions/{id}/evolution-signals，014 合法 API 家族）
///
/// 自进化信号聚合（产品化）：对会话审计链（FactsLog）中的 `Fact::Violation` 事实做
/// **只读聚合**（纯函数核心见 [`evorule_workspace::evolution_scanner`]），
/// 同时投影发布队列 pending 计数。无定时器、无任何写操作；空会话 →
/// 200 + 空信号列表（fail-soft）。
#[utoipa::path(
    get,
    path = "/api/sessions/{id}/evolution-signals",
    tag = "sessions",
    params(
        ("id" = u64, Path, description = "会话标识"),
        ("limit" = Option<usize>, Query, description = "信号条数上限（0=不限，缺省 20）")
    ),
    responses(
        (status = 200, description = "进化信号聚合投影", body = evorule_workspace::evolution_scanner::EvolutionSignalsResponse),
        (status = 404, description = "会话不存在")
    )
)]
async fn session_evolution_signals(
    State(api): State<SessionApi>,
    Path(session_id): Path<u64>,
    Query(params): Query<EvolutionSignalsParams>,
) -> Result<Json<evorule_workspace::evolution_scanner::EvolutionSignalsResponse>, StatusCode> {
    use evorule_workspace::evolution_scanner::{
        extract_instr_type, scan_violations, QueueCounts, ViolationSnapshot,
    };

    let session = api.session_or_rehydrate(session_id).await.map_err(|rej| {
        tracing::warn!(session_id, rejection = %rej, "会话读侧获取失败（含复活尝试）");
        rej.to_status()
    })?;

    // ① 违规事实只读采集（FactsLog 单调版本号 = 确定性时钟）
    let facts = session.facts_log.history_with_versions();
    let mut snapshots: Vec<ViolationSnapshot> = Vec::new();
    for (version, fact) in &facts {
        if let Fact::Violation {
            rule_index,
            reason,
            instruction,
            ..
        } = fact
        {
            snapshots.push(ViolationSnapshot {
                version: *version,
                // 归因升级：规则身份引用优先（经当前 layout 解析为 source#同源序号），
                // 越界异常态回退 rule_index 字面量（见 hit_stats::resolve_rule_ref）
                rule_ref: api.hit_stats.resolve_rule_ref(*rule_index),
                reason: reason.clone(),
                instr_type: extract_instr_type(&tcb_to_serde(instruction)),
            });
        }
    }

    // ② 发布队列 pending 计数（fail-soft：库未接线/查询失败 → 全零）
    let queue = api
        .workspace_db
        .as_ref()
        .map(|db| {
            let mut counts = QueueCounts::default();
            if let Ok(items) = db.list_publish_queue(
                Some(evorule_workspace::models::PublishStatus::Pending),
                None,
            ) {
                for item in items {
                    match item.kind {
                        evorule_workspace::models::PublishKind::MetaPromotion => {
                            counts.pending_meta_promotion += 1;
                        }
                        evorule_workspace::models::PublishKind::Normal => {
                            counts.pending_normal += 1;
                        }
                    }
                }
            }
            counts
        })
        .unwrap_or_default();

    Ok(Json(scan_violations(
        session_id,
        &snapshots,
        queue,
        params.limit.unwrap_or(20),
    )))
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
    let index: u64 = index_raw
        .trim()
        .parse()
        .map_err(|_| StatusCode::BAD_REQUEST)?;
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
    let known: std::collections::HashSet<String> = out.iter().map(|s| s.name.clone()).collect();
    for meta in api.registry_services_snapshot().iter() {
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

/// invoke 成功响应的 provenance 元数据（结果可信度锚：消费方——LLM 工具
/// 面/REST 直调方——据此区分「宿主进程内确定性服务」与「外部执行体透传
/// 结果」）。deterministic 判定：native = 宿主进程内确定性模块（true）；
/// plugin/registry = 外部执行体（插件进程/wasm-host/HTTP 回落），宿主不
/// 背书其确定性（false）。
fn invoke_provenance(api: &SessionApi, name: &str) -> serde_json::Value {
    if let Some(info) = api.native_services.iter().find(|i| i.name == name) {
        serde_json::json!({
            "service": name,
            "source": info.source,
            "plugin": info.plugin,
            "version": info.version,
            "deterministic": info.source == "native",
        })
    } else {
        // registry 回落绑定（native/plugin 未命中）：非宿主确定性执行体
        let registry = api.registry_services_snapshot();
        let meta = registry.iter().find(|m| m.name == name);
        serde_json::json!({
            "service": name,
            "source": "registry",
            "plugin": null,
            "version": meta.and_then(|m| m.version.clone()),
            "deterministic": false,
        })
    }
}

/// POST /api/services/{name}/invoke —— 插件服务直调（服务消费契约）
///
/// 与 io_request **同一处理器链**（插件路由原生优先 → registry HTTP 回落），
/// 无第二执行路径。请求 body = 服务 `args`（JSON 对象）。
///
/// 成功响应（200）为 provenance 信封：`{"result": <服务结果原样>,
/// "provenance": {service, source, plugin, version, deterministic}}`——
/// 结果可信度锚随行（deterministic：native=宿主进程内确定性服务 true；
/// plugin/registry=外部执行体，宿主不背书其确定性）。错误响应保持
/// `{"error": ..., "error_kind"?: ...}` 形态（成功/失败对称的双元数据面）。
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
        (status = 200, description = "服务执行结果 provenance 信封（result = 服务结果原样；provenance = 结果可信度锚）", body = serde_json::Value),
        (status = 403, description = "敏感服务禁止直调（走会话审计链）"),
        (status = 404, description = "服务不在对账清单"),
        (status = 502, description = "服务执行失败（错误透传）"),
        (status = 503, description = "服务链未装配")
    )
)]
pub async fn invoke_service_handler(
    State(api): State<SessionApi>,
    Path(name): Path<String>,
    actor: Option<Extension<crate::api::platform_auth::AuthedActor>>,
    Json(args): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let sensitive_native = api
        .native_services
        .iter()
        .find(|i| i.name == name)
        .map(|i| i.sensitive);
    let known = sensitive_native.is_some()
        || api
            .registry_services_snapshot()
            .iter()
            .any(|m| m.name == name);
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
            Json(
                serde_json::json!({ "error": "service chain 未装配（需 main.rs with_service_chain 注入）" }),
            ),
        ));
    };
    let params = serde_to_tcb(serde_json::json!({ "service_name": name, "args": args }));
    // 插件契约 v1 §3.3：操作者身份经 task_local 作用域随链路透传——
    // ServiceRegistryHandler 的动态头解析器（actor_headers）在本作用域内
    // 读取并注入 X-Evorule-Actor-*；会话 io_request 路径不在作用域内（其
    // 审计归因走 Fact 链,不靠头）。
    let actor_ctx = crate::api::plugin_packs::ActorContext {
        actor_type: if actor.is_some() { "user" } else { "anonymous" }.to_string(),
        actor_id: actor
            .map(|Extension(a)| a.0)
            .unwrap_or_else(|| "anonymous".to_string()),
    };
    let result = crate::api::plugin_packs::CURRENT_ACTOR
        .scope(actor_ctx, chain.execute(&params))
        .await;
    match result {
        // 成功响应信封：result=服务结果原样 + provenance=结果可信度锚
        // （deterministic 档位等）；错误响应保持 error/error_kind 形态
        // （成功/失败对称的双元数据面）
        Ok(result) => Ok(Json(serde_json::json!({
            "result": tcb_to_serde(&result),
            "provenance": invoke_provenance(&api, &name),
        }))),
        Err(e) => {
            // 批次B（P1-11 错误脱敏）: 错误响应不再内嵌链路错误原文
            // （reqwest Display/上游响应片段可携带内网 URL/端口/报文细节）。
            // 分类 → 通用文案回传;原始细节只进服务端日志。会话内 call_service
            // 路径（io_request Fact 入链）不经此处,错误串零改动。
            // 安装自动化三期回落：运行时安装的服务（known 已过=registry_services 登记、
            // 静态路由表未绑定 → execute 报 unknown service_name）→ 直转 wasm-host
            // 执行端点（/services/{name} 公开面，非管理面）。静态绑定服务不受影响
            // （其失败非 unknown 类）。
            if e.to_string().contains("unknown service_name") {
                let host_url = std::env::var("WASM_HOST_ADMIN_URL")
                    .unwrap_or_else(|_| "http://127.0.0.1:9140".into());
                let url = format!("{}/services/{}", host_url.trim_end_matches('/'), name);
                let client = reqwest::Client::builder()
                    .no_proxy()
                    .build()
                    .unwrap_or_else(|e| {
                        tracing::warn!("invoke 回落客户端构建失败（降级默认客户端）: {e}");
                        reqwest::Client::new()
                    });
                return match client
                    .post(url)
                    .json(&args)
                    .timeout(std::time::Duration::from_secs(180))
                    .send()
                    .await
                {
                    Ok(resp) => {
                        let status = resp.status();
                        match resp.json::<serde_json::Value>().await {
                            // wasm-host 回落成功同样走 provenance 信封
                            // （registry 回落绑定=非宿主确定性执行体）
                            Ok(value) if status.is_success() => Ok(Json(serde_json::json!({
                                "result": value,
                                "provenance": invoke_provenance(&api, &name),
                            }))),
                            Ok(value) => Err((
                                StatusCode::UNPROCESSABLE_ENTITY,
                                Json(serde_json::json!({ "error": value })),
                            )),
                            Err(_) => Err((
                                StatusCode::BAD_GATEWAY,
                                Json(serde_json::json!({
                                    "error": "服务调用失败: 上游响应解析（详情见服务端日志）"
                                })),
                            )),
                        }
                    }
                    Err(_) => Err((
                        StatusCode::BAD_GATEWAY,
                        Json(serde_json::json!({
                            "error": "服务调用失败: 上游请求失败（详情见服务端日志）"
                        })),
                    )),
                };
            }
            // 网络类签名（超时/连接）→ 502 脱敏（原文可携带上游 URL/端口）;
            // 其余为服务自身校验错误——原生服务的错误串按设计约定即"自诊断
            // 指引"（作者态、确定性、无内网细节），400 原文回传给调用方，
            // 不再把可自修复的入参错误伪装成网关故障。
            if upstream_network_error(&e) {
                let class = classify_upstream_error(&e);
                tracing::warn!("REST invoke 服务直调失败(网络类): service={name} detail={e}");
                Err((
                    StatusCode::BAD_GATEWAY,
                    Json(serde_json::json!({
                        "error": format!("服务调用失败: {class}（详情见服务端日志）")
                    })),
                ))
            } else {
                tracing::warn!("REST invoke 服务直调失败(服务校验): service={name} detail={e}");
                Err((
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": e.to_string(),
                        "error_kind": "service_validation"
                    })),
                ))
            }
        }
    }
}

// ===== 插件审批代理（external 插件管理面的 server 侧统一入口）=====

/// 审批代理转发超时（审批为写路径须插件落库,略宽于探活的 3s）
const PLUGIN_PROXY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// 代理错误 JSON 快捷构造
fn proxy_err(status: StatusCode, msg: String) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({ "error": msg })))
}

/// 批次B（P1-11 错误脱敏）: 上游/链路错误字符串分类 → 通用文案。
///
/// invoke 直调与审批代理的错误响应不再内嵌 reqwest Display 或上游响应片段
/// （可能携带内网 URL/端口/上游报文等内部拓扑细节）。本函数对最终错误串做
/// 关键词分类（timeout/connect/其他）,回传统一口径文案;原始细节只进服务端
/// 日志（调用方 warn!）。纯函数,单测锁定。
fn classify_upstream_error(e: &str) -> &'static str {
    let lower = e.to_ascii_lowercase();
    if lower.contains("timed out") || lower.contains("timeout") || lower.contains("deadline") {
        "上游超时（服务未在时限内响应）"
    } else if lower.contains("dns lookup failed")
        || lower.contains("connection refused")
        || lower.contains("connection reset")
        || lower.contains("connect")
        || lower.contains("error sending request")
    {
        "上游连接失败（服务不可达）"
    } else {
        "上游请求失败"
    }
}

/// 网络/传输类错误签名判定（与 classify_upstream_error 前两支同源）:
/// 命中 → 上游故障（502 脱敏）;未命中 → 服务自身校验错误（400 原文）。
fn upstream_network_error(e: &str) -> bool {
    let lower = e.to_ascii_lowercase();
    lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("deadline")
        || lower.contains("dns lookup failed")
        || lower.contains("connection refused")
        || lower.contains("connection reset")
        || lower.contains("connect")
        || lower.contains("error sending request")
}

/// approver 强制注入判定（纯逻辑,单测锁定）:
/// - 平台会话/静态 user token → AuthedActor 注入值（username / "static-user"）;
/// - 认证关闭（开发模式,无任何 Extension）→ "anonymous";
/// - **不信任请求体自报操作者**——body 中 approver 一律被覆盖（防伪造审批人）。
fn resolve_approver(actor: Option<Extension<crate::api::platform_auth::AuthedActor>>) -> String {
    match actor {
        Some(Extension(a)) => a.0,
        None => "anonymous".to_string(),
    }
}

/// 插件审批代理转发核心:目标 = {base_url}/admin/{path_tail},附 server 侧持有的
/// Bearer admin token,响应状态码与 JSON body 原样透传。
///
/// fail-fast 语义（与插件管理面自身一致）:
/// - 插件 id 不在端点表（未知/非 external）→ 404;
/// - server 侧未配置该插件 admin token → 503（不静默裸奔）;
/// - 上游不可达/非 JSON 响应 → 502（错误摘要透传）。
async fn proxy_plugin_admin(
    api: &SessionApi,
    id: &str,
    path_tail: &str,
    method: reqwest::Method,
    body: Option<serde_json::Value>,
) -> Result<(StatusCode, serde_json::Value), (StatusCode, Json<serde_json::Value>)> {
    let Some(ep) = api.plugin_admins.get(id) else {
        return Err(proxy_err(
            StatusCode::NOT_FOUND,
            format!("unknown plugin: {id}（仅 external 插件有管理面代理;合法 id 见 GET /api/health plugins 节 external=true 条目）"),
        ));
    };
    let Some(token) = ep.admin_token.as_deref() else {
        return Err(proxy_err(
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "插件 {id} 的 admin token 未配置（env: {}）— 代理拒绝转发（fail-fast,不静默）",
                plugin_admin_token_env(id)
            ),
        ));
    };
    let url = format!("{}/admin/{path_tail}", ep.base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(PLUGIN_PROXY_TIMEOUT)
        .build()
        .map_err(|e| {
            proxy_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("HTTP client 构建失败: {e}"),
            )
        })?;
    let mut req = client
        .request(method, &url)
        .header("Authorization", format!("Bearer {token}"))
        .header("X-Source", "evorule-server");
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req.send().await.map_err(|e| {
        let summary = if e.is_timeout() {
            "上游超时(5s)".to_string()
        } else if e.is_connect() {
            "上游连接失败(插件进程未监听/端口不可达)".to_string()
        } else {
            // 批次B: fallback 分支同口径——raw reqwest Display 不回传
            // 客户端,分类通用文案 + 细节只进服务端日志
            tracing::warn!("插件审批代理转发失败: plugin={id} url={url} detail={e}");
            "上游请求失败（详情见服务端日志）".to_string()
        };
        proxy_err(
            StatusCode::BAD_GATEWAY,
            format!("插件管理面转发失败: {summary}"),
        )
    })?;
    let status = StatusCode::from_u16(resp.status().as_u16())
        .map_err(|e| proxy_err(StatusCode::BAD_GATEWAY, format!("上游状态码非法: {e}")))?;
    let text = resp
        .text()
        .await
        .map_err(|e| proxy_err(StatusCode::BAD_GATEWAY, format!("上游响应读取失败: {e}")))?;
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => {
            // 批次B: 非 JSON 上游响应不再原样透传（raw 面收缩）——
            // 客户端只收通用文案,原文（截断）进服务端日志
            let snippet: String = text.chars().take(512).collect();
            tracing::warn!(
                "插件审批代理: 上游响应非 JSON: plugin={id} status={status} len={} 原文(截断): {snippet}",
                text.len()
            );
            serde_json::json!({ "error": "上游响应非 JSON（原始响应已省略,详情见服务端日志）" })
        }
    };
    Ok((status, value))
}

/// GET /api/plugins/{id}/admin/proposals —— 代理插件待批提案列表（只读）
#[utoipa::path(
    get,
    path = "/api/plugins/{id}/admin/proposals",
    tag = "plugins",
    params(("id" = String, Path, description = "external 插件 id")),
    responses(
        (status = 200, description = "插件待批提案列表（插件自持管理面响应原样透传）", body = serde_json::Value),
        (status = 404, description = "未知/非 external 插件"),
        (status = 502, description = "插件管理面不可达"),
        (status = 503, description = "server 侧未配置该插件 admin token")
    )
)]
pub async fn plugin_admin_list_proposals(
    State(api): State<SessionApi>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)> {
    let (status, value) =
        proxy_plugin_admin(&api, &id, "proposals", reqwest::Method::GET, None).await?;
    Ok((status, Json(value)))
}

// ============================================================================
// 工作空间自由加入(人工显式动作,身份服务端注入)
// ============================================================================

/// POST /api/workspaces/{id}/members/join —— 当前登录用户自助加入工作空间
///
/// 人工治理动作对齐先例:service/app 自动化凭据拒绝;身份取认证中间件注入的
/// AuthedActor,不信任前端自报。角色固定 viewer 最小权限(沙盒族端点只校验
/// 「是成员」),幂等(已在名单直接成功)。认证关闭(演示模式)无身份可注入 → 403。
#[utoipa::path(
    post,
    path = "/api/workspaces/{id}/members/join",
    tag = "workspace",
    params(
        ("id" = String, Path, description = "工作空间 ID")
    ),
    responses(
        (status = 200, description = "已加入或已在名单(幂等),joined=本次是否实际加入", body = serde_json::Value),
        (status = 403, description = "无平台登录身份,或自动化凭据禁止自助加入"),
        (status = 404, description = "工作空间不存在"),
        (status = 409, description = "工作空间非 Active")
    )
)]
pub async fn workspace_join(
    State(ws_state): State<WorkspaceState>,
    Path(workspace_id): Path<String>,
    actor: Option<Extension<crate::api::platform_auth::AuthedActor>>,
    identity: Option<Extension<CallerIdentity>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if matches!(
        identity,
        Some(Extension(CallerIdentity::Service)) | Some(Extension(CallerIdentity::App))
    ) {
        return Err(proxy_err(
            StatusCode::FORBIDDEN,
            "自动化凭据禁止自助加入动作(人工治理语义)".to_string(),
        ));
    }
    let username = match actor {
        Some(Extension(a)) if !a.0.is_empty() => a.0,
        _ => {
            return Err(proxy_err(
                StatusCode::FORBIDDEN,
                "加入工作空间需要平台登录身份(请先登录主系统)".to_string(),
            ));
        }
    };
    let joined = ws_state
        .workspace_service
        .join_workspace(&workspace_id, &username)
        .await
        .map_err(|e| -> (StatusCode, Json<serde_json::Value>) {
            let status = match &e {
                evorule_workspace::WorkspaceError::NotFound { .. } => StatusCode::NOT_FOUND,
                evorule_workspace::WorkspaceError::InvalidStateTransition { .. }
                | evorule_workspace::WorkspaceError::AlreadyExists { .. } => StatusCode::CONFLICT,
                _ => StatusCode::BAD_REQUEST,
            };
            proxy_err(status, e.to_string())
        })?;
    Ok(Json(serde_json::json!({
        "success": true,
        "joined": joined,
        "message": if joined { "已加入工作空间(角色 viewer)".to_string() } else { "已是工作空间成员".to_string() }
    })))
}

/// POST /api/plugins/{id}/admin/proposals/{pid}/approve —— 代理审批通过
///
/// approver 强制注入:取平台登录身份（AuthedActor）覆盖 body 自报值;
/// 认证动作经插件自持审计（AuditEntry）留痕,审计归属不变。
#[utoipa::path(
    post,
    path = "/api/plugins/{id}/admin/proposals/{pid}/approve",
    tag = "plugins",
    params(
        ("id" = String, Path, description = "external 插件 id"),
        ("pid" = String, Path, description = "提案 id")
    ),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "提案已批准并落库（插件自持审计留痕）", body = serde_json::Value),
        (status = 403, description = "service 凭据禁止人工审批动作"),
        (status = 404, description = "未知/非 external 插件或提案不存在"),
        (status = 502, description = "插件管理面不可达"),
        (status = 503, description = "server 侧未配置该插件 admin token")
    )
)]
pub async fn plugin_admin_approve(
    State(api): State<SessionApi>,
    Path((id, pid)): Path<(String, String)>,
    actor: Option<Extension<crate::api::platform_auth::AuthedActor>>,
    identity: Option<Extension<CallerIdentity>>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)> {
    // 审批 = 人工治理动作,自动化凭据(service/app key)拒绝(人类在场语义;
    // app key 通道同理 403)
    if matches!(
        identity,
        Some(Extension(CallerIdentity::Service)) | Some(Extension(CallerIdentity::App))
    ) {
        return Err(proxy_err(
            StatusCode::FORBIDDEN,
            "service 凭据禁止执行人工审批动作".to_string(),
        ));
    }
    let mut body = serde_json::json!({});
    if let serde_json::Value::Object(m) = &mut body {
        m.insert(
            "approver".into(),
            serde_json::json!(resolve_approver(actor)),
        );
    }
    let (status, value) = proxy_plugin_admin(
        &api,
        &id,
        &format!("proposals/{pid}/approve"),
        reqwest::Method::POST,
        Some(body),
    )
    .await?;
    Ok((status, Json(value)))
}

/// POST /api/plugins/{id}/admin/proposals/{pid}/reject —— 代理审批拒绝
///
/// approver 强制注入同 approve;reason 保留前端自报值（拒绝理由属操作内容,
/// 非操作者身份,不存在伪造身份面）。
#[utoipa::path(
    post,
    path = "/api/plugins/{id}/admin/proposals/{pid}/reject",
    tag = "plugins",
    params(
        ("id" = String, Path, description = "external 插件 id"),
        ("pid" = String, Path, description = "提案 id")
    ),
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "提案已拒绝（插件自持审计留痕）", body = serde_json::Value),
        (status = 403, description = "service 凭据禁止人工审批动作"),
        (status = 404, description = "未知/非 external 插件或提案不存在"),
        (status = 502, description = "插件管理面不可达"),
        (status = 503, description = "server 侧未配置该插件 admin token")
    )
)]
pub async fn plugin_admin_reject(
    State(api): State<SessionApi>,
    Path((id, pid)): Path<(String, String)>,
    actor: Option<Extension<crate::api::platform_auth::AuthedActor>>,
    identity: Option<Extension<CallerIdentity>>,
    body: Option<Json<serde_json::Value>>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)> {
    // 审批 = 人工治理动作,自动化凭据(service/app key)拒绝(人类在场语义;
    // app key 通道同理 403)
    if matches!(
        identity,
        Some(Extension(CallerIdentity::Service)) | Some(Extension(CallerIdentity::App))
    ) {
        return Err(proxy_err(
            StatusCode::FORBIDDEN,
            "service 凭据禁止执行人工审批动作".to_string(),
        ));
    }
    let mut body = body
        .map(|Json(v)| v)
        .unwrap_or_else(|| serde_json::json!({}));
    if let serde_json::Value::Object(m) = &mut body {
        m.insert(
            "approver".into(),
            serde_json::json!(resolve_approver(actor)),
        );
    }
    let (status, value) = proxy_plugin_admin(
        &api,
        &id,
        &format!("proposals/{pid}/reject"),
        reqwest::Method::POST,
        Some(body),
    )
    .await?;
    Ok((status, Json(value)))
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

/// 装载期 I/O 权利面独占防线：被拒条目的单条重叠明细
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RejectedOverlapDetail {
    /// 重叠的 io_type
    pub io_type: String,
    /// 重叠面：`emit`（发射重叠）/ `consume`（消费重叠）
    pub kind: String,
    /// 权利首声明者（`来源#下标`，如 `core_eval#7`）
    pub claimed_by: String,
}

/// 装载期 I/O 权利面独占防线：被拒载的重叠规则条目（审计面）
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RejectedOverlapEntry {
    /// 被拒条目在剔除前合并列表中的原始下标（与启动日志/hit-stats 下标口径一致）
    pub index: usize,
    /// 来源标签（rules_dir 相对路径）
    pub source: String,
    /// 被拒规则特征摘要（顶层指令类型 + domain.instruction_type）
    pub rule_summary: String,
    /// 重叠明细（发射/消费 × io_type）
    pub overlaps: Vec<RejectedOverlapDetail>,
}

/// domain 求值装载门禁：被拒条目的单条违规明细
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DomainViolation {
    /// 违规门类：`missing_declaration`（缺 on_missing 声明）/
    /// `invalid_declaration`（on_missing 非 error/unsat）/
    /// `value_literal_ambiguous`（value 呈 exec 根段点路径形态缺 __ 前缀）
    pub door: String,
    /// 违规位置与说明（含 domain.path 与修复指引）
    pub detail: String,
}

/// domain 求值装载门禁：被拒载的违规规则条目（审计面）
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RejectedDomainEntry {
    /// 被拒条目在剔除前合并列表中的原始下标（与启动日志/hit-stats 下标口径一致）
    pub index: usize,
    /// 来源标签（rules_dir 相对路径；导入预判为 `bundles/{bundle_id}/{entry_id}`）
    pub source: String,
    /// 被拒规则特征摘要（顶层指令类型 + domain.instruction_type）
    pub rule_summary: String,
    /// 违规明细（三道门：缺声明/声明非法/value 写作错误）
    pub violations: Vec<DomainViolation>,
}

/// 合并装载产出五元组：合并规则列表（引擎输入）+ layout（下标→来源解析）
/// + 装载防线被拒重叠条目 + domain 门禁被拒条目 + 四步门被拒文件。
pub type MergedLoadOutcome = (
    Vec<JsonValue>,
    crate::api::hit_stats::RulesetLayout,
    Vec<RejectedOverlapEntry>,
    Vec<RejectedDomainEntry>,
    Vec<RejectedGateFileEntry>,
);

/// 四步门（tier/schema/discipline/domain-on-missing）被拒文件的程序化可查面
/// （与 rejected_overlaps/rejected_domains 同构：reload 返回 200 时调用方
/// 可区分「正常重载」与「某文件被门禁剔除」）。
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RejectedGateFileEntry {
    /// 来源标签（rules_dir 相对路径，/ 归一化）
    pub path: String,
    /// 拒载门（tier / schema / discipline / domain_on_missing）
    pub gate: String,
    /// 拒载原因（loader 四部分错误的原文摘要）
    pub reason: String,
}

/// bundle 导入结果（运行配置 + reload 期被拒重叠条目）
///
/// `Deref` 到 [evorule_bundle::ImportResult]：既有运行配置字段访问面零迁移。
#[derive(Debug)]
pub struct ImportOutcome {
    /// 校验通过后的运行配置（与导入前一致）
    pub result: evorule_bundle::ImportResult,
    /// reload 期 I/O 权利面防线拒载的重叠条目（导入前置预判已拦，正常为空；
    /// 非空=预判与 reload 之间 rules_dir 并发变更的半激活事实，显式不静默）
    pub rejected_overlaps: Vec<RejectedOverlapEntry>,
    /// reload 期 domain 门禁拒载的违规条目（导入前置预判已拦，正常为空；
    /// 非空=预判与 reload 之间 rules_dir 并发变更的半激活事实，显式不静默）
    pub rejected_domains: Vec<RejectedDomainEntry>,
}

impl std::ops::Deref for ImportOutcome {
    type Target = evorule_bundle::ImportResult;

    fn deref(&self) -> &Self::Target {
        &self.result
    }
}

/// 规则热重载响应

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]

pub struct RulesReloadedResponse {
    pub reload_ok: bool,

    pub previous_rules: usize,

    pub current_rules: usize,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,

    /// 装载防线：本次装载被拒载的重叠条目（空 = 无重叠）
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rejected_overlaps: Vec<RejectedOverlapEntry>,

    /// domain 门禁：本次装载被拒载的违规条目（空 = 无违规）
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rejected_domains: Vec<RejectedDomainEntry>,

    /// 四步门：本次装载被拒载的文件（空 = 无文件被门禁剔除；
    /// 与 rejected_overlaps/rejected_domains 同构，reload 返回 200 时
    /// 调用方可区分「正常重载」与「某文件被门禁剔除」）
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rejected_gate_files: Vec<RejectedGateFileEntry>,
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
// 响应体本就是本 handler 的既有契约形态(成功/失败同构),新增门禁拒载字段后
// 体积越过 result_large_err 阈值——契约不变,显式豁免而非缩小返回类型
#[allow(clippy::result_large_err)]
async fn reload_rules_handler(
    State(sessions): State<SessionApi>,
) -> Result<(StatusCode, Json<RulesReloadedResponse>), (StatusCode, Json<RulesReloadedResponse>)> {
    let old_len = sessions.core_eval_len();

    match sessions.reload_from_disk().await {
        Ok((old, new_len, rejected_overlaps, rejected_domains, rejected_gate_files)) => Ok((
            StatusCode::OK,
            Json(RulesReloadedResponse {
                reload_ok: true,

                previous_rules: old,

                current_rules: new_len,

                error: None,

                rejected_overlaps,

                rejected_domains,

                rejected_gate_files,
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

                    rejected_overlaps: Vec::new(),

                    rejected_domains: Vec::new(),

                    rejected_gate_files: Vec::new(),
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

    // ===== caller_role 注入单元测试 =====

    mod o179 {
        use super::*;

        /// 空白 SessionApi（纯内存，无 dispatcher）
        fn blank_api() -> SessionApi {
            SessionApi::new(vec![], 64)
        }

        #[test]
        fn o179_inject_declared_session_adds_meta_and_keeps_body() {
            let api = blank_api();
            api.caller_roles
                .lock()
                .unwrap()
                .insert(1, "human".to_string());

            let mut instr = serde_json::json!({
                "type": "io_request",
                "params": {"io_type": "call_service", "service_name": "s"}
            });
            api.inject_caller_role(1, &mut instr);

            assert_eq!(instr["__meta__"]["caller_role"], "human");
            // 原有键不受影响
            assert_eq!(instr["params"]["io_type"], "call_service");
            assert_eq!(instr["type"], "io_request");
        }

        #[test]
        fn o179_inject_undeclared_or_closed_session_is_noop() {
            let api = blank_api();
            let mut instr = serde_json::json!({"type": "set", "params": {"attr": "x", "operation": "set", "value": 1}});
            api.inject_caller_role(42, &mut instr); // 未登记的会话
            assert!(instr.get("__meta__").is_none());

            // 声明后移除（close 语义）→ 同样不注入
            api.caller_roles
                .lock()
                .unwrap()
                .insert(7, "llm".to_string());
            api.remove_caller_role(7);
            assert!(api.declared_caller_role(7).is_none());
        }

        #[test]
        fn o179_inject_into_existing_meta_object_merges() {
            let api = blank_api();
            api.caller_roles
                .lock()
                .unwrap()
                .insert(2, "llm".to_string());
            let mut instr = serde_json::json!({"type": "noop", "__meta__": {"note": "keep"}});
            api.inject_caller_role(2, &mut instr);
            assert_eq!(instr["__meta__"]["caller_role"], "llm");
            assert_eq!(instr["__meta__"]["note"], "keep");
        }

        /// 差分验证（设计档 §八风险 1 退路判定）：
        /// 同一指令带/不带根部 `__meta__` 进 TCB execute_transition，
        /// 规则命中与转换结果完全一致 —— 根部额外键与 TCB 兼容，退路不触发。
        #[test]
        fn o179_tcb_differential_meta_key_has_no_rule_effect() {
            // 宪法同构最小片段（与装载防线宪法测试同源结构）：
            // L1 直接规则（set counter）+ 桥接 branch（call_service → io_request）
            let core_eval = o135_tcb(O135_CONSTITUTION);
            let plain = serde_json::json!({
                "type": "call_service",
                "params": {"service_name": "demo", "args": {"k": "v"}}
            });

            // 注入前的原文必须过提交期 Schema 门禁（回归锚）
            assert!(
                evorule_rule_schema::validate_command_instruction(&plain).valid,
                "原文应过 Schema 门禁"
            );

            let mut injected = plain.clone();
            injected["__meta__"] = serde_json::json!({"caller_role": "human"});

            let payload = serde_json::json!({});
            let queue: Vec<JsonValue> = Vec::new();
            let r_plain = evorule_tcb::execute_transition(
                &core_eval,
                &serde_to_tcb(plain),
                &serde_to_tcb(payload.clone()),
                &queue,
            )
            .unwrap();
            let r_injected = evorule_tcb::execute_transition(
                &core_eval,
                &serde_to_tcb(injected.clone()),
                &serde_to_tcb(payload),
                &queue,
            )
            .unwrap();

            // 同为 IoRequired（桥接分支命中），io_type 一致 —— 规则命中行为不变
            match (&r_plain, &r_injected) {
                (
                    evorule_tcb::TransitionResult::IoRequired {
                        io_type: t1,
                        params: _,
                    },
                    evorule_tcb::TransitionResult::IoRequired {
                        io_type: t2,
                        params: _,
                    },
                ) => assert_eq!(t1, t2, "带 __meta__ 与不带的规则命中 io_type 必须一致"),
                (a, b) => panic!("两侧应同为 IoRequired，实际 plain={a:?} injected={b:?}"),
            }
        }

        /// 注入发生在 Schema 门禁之后的守护测试：
        /// 元指令（transform_rule 白名单 additionalProperties=false）原文不含
        /// __meta__ 可过门禁 —— 若未来有人把注入挪到门禁前，此测试立即红。
        #[test]
        fn o179_meta_key_would_break_meta_instruction_gate_guard() {
            let meta_instr = serde_json::json!({
                "type": "io_request",
                "params": {"io_type": "call_service"},
                "__meta__": {"caller_role": "human"}
            });
            assert!(
                !evorule_rule_schema::validate_command_instruction(&meta_instr).valid,
                "根部 __meta__ 的元指令原文应被门禁拒绝（守护注入时机在门禁之后）"
            );
        }
    }

    // ===== fork/from IoSubscriber + 声明继承（io 应答缺口合并修复）=====

    mod o185 {
        use super::*;

        /// EchoHandler：call_service 应答回显参数（结果注入 __io_results__）
        struct EchoHandler;

        #[async_trait::async_trait]
        impl evorule_reactor::IoHandler for EchoHandler {
            async fn execute(&self, params: &JsonValue) -> evorule_reactor::IoResult {
                Ok(params.clone())
            }
        }

        /// core_eval：任意命令触发一次 call_service pending I/O
        ///（与 session.rs 超时预算测试同构的最小 io core_eval）
        fn make_io_core_eval() -> Vec<JsonValue> {
            let mut params = std::collections::BTreeMap::new();
            params.insert("io_type".to_string(), JsonValue::string("call_service"));
            params.insert("service_name".to_string(), JsonValue::string("demo"));
            let mut instr = std::collections::BTreeMap::new();
            instr.insert("type".to_string(), JsonValue::string("io_request"));
            instr.insert("params".to_string(), JsonValue::Object(params));
            vec![JsonValue::Object(instr)]
        }

        /// 时限内轮询会话事实链，等待首个 IoResponse，返回 (error, result)
        async fn wait_io_response(
            session: &evorule_governance_host::session::Session,
            deadline: std::time::Duration,
        ) -> (Option<String>, JsonValue) {
            let start = tokio::time::Instant::now();
            loop {
                assert!(
                    start.elapsed() < deadline,
                    "时限内未收到 IoResponse（IoSubscriber 未应答？）"
                );
                for fact in session.facts_log.history() {
                    if let Fact::IoResponse { result, error, .. } = fact {
                        return (error, result);
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }

        /// 核心验收：fork 子会话（经公共辅助 spawn IoSubscriber + 声明继承）
        /// 发射 call_service io 时可被应答分发——io 应答缺口（60s 超时无人应答）
        /// 修复后的正向探针。
        #[tokio::test]
        async fn o185_fork_child_session_io_dispatches_with_inherited_role() {
            let metrics: SharedMetrics = shared_prometheus_metrics().unwrap();

            let dispatcher = IoDispatcher::builder()
                .register(IoType::call_service(), Arc::new(EchoHandler))
                .build();
            // 权限缺省 fail-closed 后（human 缺省 Deny，宽松=显式经账本声明），
            // 生产组装在启动期幂等播种 human×io:* 放行条目；本测试按生产同口径
            // 补种——本探针验证的是 fork 角色继承下的分发机制，不是缺省策略。
            let shared_facts = evorule_governance::shared_facts_log::SharedFactsLog::new();
            {
                use evorule_governance::permission::{
                    Effect, PermissionEntry, PermissionState, Resource, ResourceType, Subject,
                    SubjectType,
                };
                let mut entry = PermissionEntry::new(
                    "default-human-allow-io",
                    Subject {
                        subject_type: SubjectType::User,
                        id: "human".to_string(),
                    },
                    Resource {
                        resource_type: ResourceType::IoAction,
                        path: "io:*".to_string(),
                    },
                    Effect::Allow,
                );
                entry.state = PermissionState::Active;
                entry.updated_by = "bootstrap".to_string();
                evorule_governance::permission::PermissionTable::store_entry(
                    &shared_facts,
                    &entry,
                    0,
                )
                .expect("seed default-human-allow-io (same as production bootstrap)");
            }
            let api = SessionApi::new(make_io_core_eval(), 100)
                .with_dispatcher(dispatcher)
                .with_shared_facts(shared_facts);

            // 父会话声明 human（等价于经 /api/sessions 带 caller_role 创建）
            let parent = api.sessions.lock().await.create_session().unwrap();
            api.caller_roles
                .lock()
                .unwrap()
                .insert(parent, "human".to_string());

            // fork：与 from/fork handler 相同的接线（取父声明 → 公共辅助）
            let child = api.sessions.lock().await.create_session().unwrap();
            let inherited = api.declared_caller_role(parent);
            assert_eq!(inherited.as_deref(), Some("human"), "父声明应可读出");
            api.spawn_io_subscriber_for_session(child, metrics, inherited)
                .await;

            // 子会话发命令；__meta__ 为命令入口 inject_caller_role 的等价形态
            let session = api
                .sessions
                .lock()
                .await
                .get_session(child)
                .expect("子会话应存在");
            let mut meta = std::collections::BTreeMap::new();
            meta.insert("caller_role".to_string(), JsonValue::string("human"));
            let mut instr = std::collections::BTreeMap::new();
            instr.insert("type".to_string(), JsonValue::string("tick"));
            instr.insert("__meta__".to_string(), JsonValue::Object(meta));
            session
                .command_tx
                .send(Fact::Command {
                    // 直连命令手工指定 id：避开反应器 id_gen 从 1 起的引导事实
                    // （撞 id 会导致 Command append 失败 → resolver 断链 fail-closed）
                    id: FactId(10_000),
                    instruction: JsonValue::Object(instr),
                })
                .unwrap();

            // 子会话 io 被应答分发（非 60s 超时）：human 声明 + 启动期同款放行种子
            let (error, result) =
                wait_io_response(&session, std::time::Duration::from_secs(5)).await;
            assert!(
                error.is_none(),
                "继承 human 声明的子会话 io 应成功分发，实际错误: {error:?}"
            );
            assert_eq!(
                result.get("service_name"),
                Some(&JsonValue::string("demo")),
                "EchoHandler 应回显参数"
            );
        }

        /// fork 子会话声明继承：llm 父 → from / fork 两端点子会话均继承 llm
        #[tokio::test]
        async fn o185_fork_inherits_parent_declaration_from_and_fork() {
            let (state, _) = make_test_state();
            let router = make_test_router(&state);

            // 声明 llm 的父会话
            let (status, body) = oneshot_json(
                router.clone(),
                "POST",
                "/api/sessions",
                Some(r#"{"caller_role":"llm"}"#),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            let parent = body["session_id"].as_u64().unwrap();

            // from 端点（默认最新版本）→ 子会话继承 llm
            let (status, body) = oneshot_json(
                router.clone(),
                "POST",
                &format!("/api/sessions/from/{parent}"),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "from 应成功: {body}");
            let child_from = body["session_id"].as_u64().unwrap();
            assert_eq!(
                state.sessions.declared_caller_role(child_from).as_deref(),
                Some("llm"),
                "from 子会话必须继承父声明"
            );

            // fork 端点（显式版本；version=0 = 初始空状态，恒有效）
            let (status, body) = oneshot_json(
                router,
                "POST",
                &format!("/api/sessions/fork/{parent}?version=0"),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "fork 应成功: {body}");
            let child_fork = body["session_id"].as_u64().unwrap();
            assert_eq!(
                state.sessions.declared_caller_role(child_fork).as_deref(),
                Some("llm"),
                "fork 子会话必须继承父声明"
            );
        }

        /// 父未声明 → 子不登记（fail-closed：Unknown → Deny 与今日一致）
        #[tokio::test]
        async fn o185_fork_without_parent_declaration_child_stays_undeclared() {
            let (state, _) = make_test_state();
            let router = make_test_router(&state);

            let (status, body) = oneshot_json(router.clone(), "POST", "/api/sessions", None).await;
            assert_eq!(status, StatusCode::OK);
            let parent = body["session_id"].as_u64().unwrap();
            assert!(
                state.sessions.declared_caller_role(parent).is_none(),
                "未带声明的创建不应登记"
            );

            let (status, body) = oneshot_json(
                router,
                "POST",
                &format!("/api/sessions/from/{parent}"),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "from 应成功: {body}");
            let child = body["session_id"].as_u64().unwrap();

            assert!(
                state.sessions.declared_caller_role(child).is_none(),
                "父未声明 → 子会话不得登记（fail-closed）"
            );
        }
    }

    /// 临时 rules_dir 工厂（进程内唯一目录名，测试结束自清理）
    fn make_sort_test_rules_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("evorule-load-order-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_rule_file(dir: &std::path::Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    fn constraint_body(title: &str, promoted_at: Option<&str>) -> String {
        let pa = promoted_at
            .map(|t| format!(r#","promoted_at":"{t}""#))
            .unwrap_or_default();
        format!(
            r#"{{"metadata":{{"tier":"constraint","title":"{title}"{pa}}},"transform":[{{"type":"set","params":{{"attr":"mark","operation":"set","value":"{title}"}}}}]}}"#
        )
    }

    // ===== L2 纪律门禁接线 =====

    /// DC-09 违规样本：io_request 前有同规则 set（IoRequired 纯信号会丢弃 set → 重放失真）。
    /// 结构合法（过 schema 门禁），专门用于验证纪律门禁的独立拦截。
    const DC09_VIOLATING_RULE: &str = r#"{"transform":[
        {"type":"set","params":{"attr":"x","operation":"set","value":1}},
        {"type":"io_request","params":{"io_type":"llm"}}
    ]}"#;

    fn parse_transforms(body: &str) -> Vec<serde_json::Value> {
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        v.get("transform")
            .and_then(|t| t.as_array())
            .map(|a| a.to_vec())
            .unwrap_or_default()
    }

    /// W1 纯观测（warn 模式）：违规**不拦装载**——纪律门禁 warn 期的目的是
    /// 吸收残余误报，误报的最大代价 = 一行日志，而非可用性事故。
    #[test]
    fn discipline_gate_warn_mode_still_loads_violating_file() {
        let p = std::path::Path::new("rules/dc09-violating.json");
        let arr = parse_transforms(DC09_VIOLATING_RULE);
        assert!(!arr.is_empty());
        assert!(
            SessionApi::passes_discipline_gate_with(p, &arr, false),
            "warn 模式（W1 纯观测）违规文件必须照常装载"
        );
        assert!(
            !SessionApi::passes_discipline_gate_with(p, &arr, true),
            "enforce 模式同一违规文件必须拒载（一键降级旋钮的反向验证）"
        );
    }

    /// enforce 模式拒载语义（fail-soft 单文件）：违规文件返回 false → parse_rule_file
    /// 返回 None → 与 schema/tier 门禁同款「warn + 跳过」，服务不中断。
    #[test]
    fn discipline_gate_enforce_mode_rejects_violating_file() {
        let p = std::path::Path::new("rules/dc09-violating.json");
        let arr = parse_transforms(DC09_VIOLATING_RULE);
        assert!(
            !SessionApi::passes_discipline_gate_with(p, &arr, true),
            "enforce 模式必须拒载 DC-09 违规文件"
        );
    }

    /// 端到端：经 load_merged_transforms_from_fs 全管线验证接线点 ①（parse_rule_file）。
    /// 宪法 2 条 + 违规文件 1 条；按当前 env 模式断言是否计入（缺省 enforce → 拒载；
    /// 显式 warn → 计入）。
    #[test]
    fn discipline_gate_wired_into_parse_rule_file_pipeline() {
        let tmp = tempfile::tempdir().unwrap();
        let rules_dir = tmp.path().join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(rules_dir.join("dc09.json"), DC09_VIOLATING_RULE).unwrap();
        let core_eval_path = tmp.path().join("server_eval.json");
        std::fs::write(
            &core_eval_path,
            r#"{"transform":[
                {"type":"set","params":{"attr":"result","operation":"set","value":"ok"}},
                {"type":"branch","params":{"domain":{"type":"instruction","instruction_type":"call_external"},"on_true":[],"on_false":[]}}
            ]}"#,
        )
        .unwrap();
        let merged = SessionApi::load_merged_transforms_from_fs(&core_eval_path, &rules_dir)
            .expect("warn 模式下装载不应失败");
        // merged 是扁平 transform 列表：宪法 2 条 + 违规文件 2 条（set+io_request）
        let expect_violating = !SessionApi::discipline_gate_enforce();
        assert_eq!(
            merged.len(),
            if expect_violating { 4 } else { 2 },
            "warn 模式 = 宪法 2 + 违规 2（照常装载）；enforce 模式 = 仅宪法 2"
        );
    }

    // ===== 装载期 I/O 权利面独占防线 =====

    /// serde_json transform 文档 → TCB JsonValue 列表
    fn o135_tcb(body: &str) -> Vec<JsonValue> {
        parse_transforms(body)
            .into_iter()
            .map(serde_to_tcb)
            .collect()
    }

    /// 宪法同构最小片段：call_external 双路径 + call_service 双路径（复刻 server_eval L1）
    const O135_CONSTITUTION: &str = r#"{"transform":[
        {"type":"branch","params":{"domain":{"type":"instruction","instruction_type":"call_external"},"on_true":[
            {"type":"branch","params":{"domain":{"type":"exists","path":"__exec__.payload.__io_results__.call_external"},
             "on_true":[
                {"type":"set","params":{"attr":"llm_response","operation":"set","value":"__exec__.payload.__io_results__.call_external"}},
                {"type":"set","params":{"attr":"__exec__.payload.__io_results__.call_external","operation":"set","value":null}}],
             "on_false":[{"type":"io_request","params":{"io_type":"call_external","messages":"m"}}]}}],
         "on_false":[]}},
        {"type":"branch","params":{"domain":{"type":"all","inner":[{"type":"instruction","instruction_type":"call_service"},{"type":"exists","path":"__exec__.instruction.params.service_name"}]},
         "on_true":[{"type":"branch","params":{"domain":{"type":"exists","path":"__exec__.payload.__io_results__.call_service"},
           "on_true":[
             {"type":"set","params":{"attr":"service_result","operation":"set","value":"__exec__.payload.__io_results__.call_service"}},
             {"type":"set","params":{"attr":"__exec__.payload.__io_results__.call_service","operation":"set","value":null}}],
           "on_false":[{"type":"io_request","params":{"io_type":"call_service","service_name":"s"}}]}}],
         "on_false":[]}}
    ]}"#;

    /// e3-call-external 同构文件：双路径 branch + 恒空 branch（第二条无害放行）
    const O135_E3_LIKE: &str = r#"{"transform":[
        {"type":"branch","params":{"domain":{"type":"instruction","instruction_type":"call_external"},"on_true":[
            {"type":"branch","params":{"domain":{"type":"exists","path":"__exec__.payload.__io_results__.call_external"},
             "on_true":[
                {"type":"set","params":{"attr":"llm_response","operation":"set","value":"__exec__.payload.__io_results__.call_external"}},
                {"type":"set","params":{"attr":"__exec__.payload.__io_results__.call_external","operation":"set","value":null}}],
             "on_false":[{"type":"io_request","params":{"io_type":"call_external","messages":"m"}}]}}],
         "on_false":[]}},
        {"type":"branch","params":{"domain":{"type":"all","inner":[]},"on_true":[],"on_false":[]}}
    ]}"#;

    /// t0-r2/flow-e 同构：无守卫纯发射（消费轮重发缺陷型）
    const O135_T0_LIKE: &str = r#"{"transform":[
        {"type":"branch","params":{"domain":{"type":"instruction","instruction_type":"call_service"},"on_true":[
            {"type":"io_request","params":{"io_type":"call_service","service_name":"echo","args?":"a"}}],"on_false":[]}}
    ]}"#;

    /// 无 I/O 特征的普通业务规则（防线零误伤面）
    const O135_BENIGN: &str = r#"{"transform":[
        {"type":"set","params":{"attr":"mark","operation":"set","value":"ok"}},
        {"type":"branch","params":{"domain":{"type":"exists","path":"flag"},"on_true":[],"on_false":[]}}
    ]}"#;

    /// io_capability 特征面：双路径/纯发射/参数化诚实边界/enforce 不参与/prose 不误报
    #[test]
    fn o135_io_capability_feature_face() {
        let (e3_emit, e3_consume) = SessionApi::io_capability(&o135_tcb(O135_E3_LIKE)[0]);
        assert!(e3_emit.contains("call_external") && e3_consume.contains("call_external"));
        assert_eq!(e3_emit.len(), 1);
        assert_eq!(e3_consume.len(), 1);

        // e3 第二条恒空 branch：无任何能力面
        let (empty_emit, empty_consume) = SessionApi::io_capability(&o135_tcb(O135_E3_LIKE)[1]);
        assert!(empty_emit.is_empty() && empty_consume.is_empty());

        // 纯发射：只有发射面
        let (t0_emit, t0_consume) = SessionApi::io_capability(&o135_tcb(O135_T0_LIKE)[0]);
        assert!(t0_emit.contains("call_service"));
        assert!(t0_consume.is_empty());

        // 诚实边界：参数化 io_type（__ 引用路径）不入发射面
        let parameterized = r#"{"transform":[
            {"type":"branch","params":{"domain":{"type":"instruction","instruction_type":"custom"},"on_true":[
                {"type":"io_request","params":{"io_type":"__exec__.instruction.params.target"}}],"on_false":[]}}
        ]}"#;
        let (p_emit, p_consume) = SessionApi::io_capability(&o135_tcb(parameterized)[0]);
        assert!(p_emit.is_empty() && p_consume.is_empty());

        // enforce 指令不参与检测面（L2 约束非 I/O 生命周期规则）
        let enforce = r#"{"transform":[
            {"type":"enforce","params":{"domain":{"type":"exists","path":"quota_exceeded"},"reason":"r"}}
        ]}"#;
        let (en_emit, en_consume) = SessionApi::io_capability(&o135_tcb(enforce)[0]);
        assert!(en_emit.is_empty() && en_consume.is_empty());

        // prose 引用（无 `__io_results__.` 点缀形态）不误报
        let prose = r#"{"transform":[
            {"type":"set","params":{"attr":"note","operation":"set","value":"__io_results__ 按类型隔离"}}
        ]}"#;
        let (_, pr_consume) = SessionApi::io_capability(&o135_tcb(prose)[0]);
        assert!(pr_consume.is_empty());
    }

    /// 正例（端到端）：宪法 L1 双路径 + e3 同构文件 → 恰拒 e3 双路径条目，
    /// 恒空第二条与宪法保留（保留列表含完整消费语义）。
    #[test]
    fn o135_end_to_end_rejects_e3_like_overlap() {
        let tmp = tempfile::tempdir().unwrap();
        let rules_dir = tmp.path().join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(rules_dir.join("e3-call-external.json"), O135_E3_LIKE).unwrap();
        let core_eval_path = tmp.path().join("server_eval.json");
        std::fs::write(&core_eval_path, O135_CONSTITUTION).unwrap();

        let (rules, _layout, rejected, _rejected_domains, _gate_files) =
            SessionApi::load_merged_with_layout(&core_eval_path, &rules_dir).expect("装载不应失败");

        // 宪法 2 条 + e3 文件 2 条 − 拒 1 = 3
        assert_eq!(rules.len(), 3);
        assert_eq!(rejected.len(), 1, "恰拒 e3 双路径 transform[0] 一条");
        let e = &rejected[0];
        assert_eq!(e.index, 2, "被拒下标 = 宪法之后首条（原始合并下标口径）");
        assert_eq!(e.source, "e3-call-external.json");
        let kinds: Vec<&str> = e.overlaps.iter().map(|d| d.kind.as_str()).collect();
        assert!(kinds.contains(&"emit") && kinds.contains(&"consume"));
        assert!(
            e.overlaps.iter().all(|d| d.io_type == "call_external"),
            "重叠 io_type 均为 call_external"
        );
        assert!(
            e.overlaps
                .iter()
                .all(|d| d.claimed_by.starts_with("core_eval#")),
            "首声明者为宪法条目"
        );
        // 保留列表首条仍为宪法 call_external 双路径（消费语义由首条承担）
        let (k_emit, k_consume) = SessionApi::io_capability(&rules[0]);
        assert!(k_emit.contains("call_external") && k_consume.contains("call_external"));
    }

    /// 扩面（端到端）：无守卫纯发射（t0/flow-e 型）与宪法发射权重叠 → 拒载。
    #[test]
    fn o135_emission_only_overlap_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let rules_dir = tmp.path().join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(rules_dir.join("t0-r2.json"), O135_T0_LIKE).unwrap();
        let core_eval_path = tmp.path().join("server_eval.json");
        std::fs::write(&core_eval_path, O135_CONSTITUTION).unwrap();

        let (rules, _layout, rejected, _rejected_domains, _gate_files) =
            SessionApi::load_merged_with_layout(&core_eval_path, &rules_dir).unwrap();

        // 宪法 2 条 + t0 1 条 − 拒 1 = 2
        assert_eq!(rules.len(), 2);
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0].source, "t0-r2.json");
        assert_eq!(rejected[0].overlaps.len(), 1);
        assert_eq!(rejected[0].overlaps[0].kind, "emit");
        assert_eq!(rejected[0].overlaps[0].io_type, "call_service");
    }

    /// 负例（端到端）：无重叠规则集 → 零拒载（防线零误伤）。
    #[test]
    fn o135_benign_ruleset_zero_rejection() {
        let tmp = tempfile::tempdir().unwrap();
        let rules_dir = tmp.path().join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(rules_dir.join("benign.json"), O135_BENIGN).unwrap();
        let core_eval_path = tmp.path().join("server_eval.json");
        std::fs::write(&core_eval_path, O135_CONSTITUTION).unwrap();

        let (rules, _layout, rejected, _rejected_domains, _gate_files) =
            SessionApi::load_merged_with_layout(&core_eval_path, &rules_dir).unwrap();
        // 宪法 2 条 + benign 文件 2 条（set + branch，均无 I/O 能力面）
        assert_eq!(rules.len(), 4);
        assert!(rejected.is_empty(), "无重叠必须零拒载");
    }

    /// 宪法豁免（纯函数）：宪法内部同 io_type 双路径双条目（L1-8/9 domain 互斥型）
    /// 互不拒载；rules_dir 条目不得触发对宪法条目的拒载。
    #[test]
    fn o135_constitution_internal_overlap_exempt() {
        let mut rules = o135_tcb(O135_CONSTITUTION); // 2 条宪法（call_external + call_service 各一）
                                                     // 宪法第 3 条：call_service 第二双路径（复刻 L1 tool_name 变体）
        rules.push(
            o135_tcb(r#"{"transform":[
                {"type":"branch","params":{"domain":{"type":"all","inner":[{"type":"instruction","instruction_type":"call_service"},{"type":"exists","path":"__exec__.instruction.params.tool_name"}]},
                 "on_true":[{"type":"branch","params":{"domain":{"type":"exists","path":"__exec__.payload.__io_results__.call_service"},
                   "on_true":[{"type":"set","params":{"attr":"__exec__.payload.__io_results__.call_service","operation":"set","value":null}}],
                   "on_false":[{"type":"io_request","params":{"io_type":"call_service","tool_name":"t"}}]}}],
                 "on_false":[]}}
            ]}"#)[0]
                .clone(),
        );
        let sources = vec!["core_eval".to_string(); 3];
        let (kept, _ks, rejected) = SessionApi::reject_overlapped_io_rules(rules, sources, 3);
        assert_eq!(kept.len(), 3, "宪法内部重叠互不拒载（豁免）");
        assert!(rejected.is_empty());
    }

    /// 多条重叠仅保首条（纯函数）：权利首声明者 = 宪法条目，后续全部拒载。
    #[test]
    fn o135_multiple_overlaps_keep_first_only() {
        let mut rules = o135_tcb(O135_CONSTITUTION);
        rules.push(o135_tcb(O135_E3_LIKE)[0].clone());
        rules.push(o135_tcb(O135_E3_LIKE)[0].clone());
        let sources = vec![
            "core_eval".to_string(),
            "core_eval".to_string(),
            "bundles/a/e3.json".to_string(),
            "bundles/b/e3-copy.json".to_string(),
        ];
        let (kept, _ks, rejected) = SessionApi::reject_overlapped_io_rules(rules, sources, 2);
        assert_eq!(kept.len(), 2, "仅宪法 2 条保留");
        assert_eq!(rejected.len(), 2, "两条重叠全部拒载");
        assert_eq!(rejected[0].index, 2);
        assert_eq!(rejected[1].index, 3);
        assert!(
            rejected
                .iter()
                .flat_map(|r| r.overlaps.iter())
                .all(|d| d.claimed_by == "core_eval#0"),
            "首声明者统一指向宪法首条"
        );
    }

    /// 消费权独占（纯函数）：跨规则「消费者在后」组合同样拒载
    /// （消费-only 条目清除 __io_results__ 会使后位发射者误判首次调用）。
    #[test]
    fn o135_cross_rule_consume_overlap_rejected() {
        let mut rules = o135_tcb(O135_CONSTITUTION);
        // 消费-only 条目（set null 清除 call_external，无发射）
        rules.push(
            o135_tcb(r#"{"transform":[
                {"type":"branch","params":{"domain":{"type":"instruction","instruction_type":"call_external"},"on_true":[
                    {"type":"set","params":{"attr":"__exec__.payload.__io_results__.call_external","operation":"set","value":null}}],"on_false":[]}}
            ]}"#)[0]
                .clone(),
        );
        let sources = vec![
            "core_eval".to_string(),
            "core_eval".to_string(),
            "c.json".to_string(),
        ];
        let (kept, _ks, rejected) = SessionApi::reject_overlapped_io_rules(rules, sources, 2);
        assert_eq!(kept.len(), 2);
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0].overlaps[0].kind, "consume");
    }

    /// 诚实边界（纯函数）：参数化 io_type 发射不参与独占判定（静态不可判定，漏检留痕）。
    #[test]
    fn o135_parameterized_io_type_out_of_face() {
        let mut rules = o135_tcb(O135_CONSTITUTION);
        rules.push(
            o135_tcb(r#"{"transform":[
                {"type":"branch","params":{"domain":{"type":"instruction","instruction_type":"custom"},"on_true":[
                    {"type":"io_request","params":{"io_type":"__exec__.instruction.params.target"}}],"on_false":[]}}
            ]}"#)[0]
                .clone(),
        );
        let sources = vec![
            "core_eval".to_string(),
            "core_eval".to_string(),
            "p.json".to_string(),
        ];
        let (kept, _ks, rejected) = SessionApi::reject_overlapped_io_rules(rules, sources, 2);
        assert_eq!(kept.len(), 3, "参数化形态放行（诚实边界）");
        assert!(rejected.is_empty());
    }

    // =========================================================================
    // domain 求值装载门禁（三态声明化方案 v4 · 端到端/装载面用例素材）
    // =========================================================================

    /// 测试辅助：构造携带指定域对象的最小 branch 规则 JSON
    fn t4b_branch_rule(domain_obj: &str) -> String {
        format!(
            r#"{{"transform":[{{"type":"branch","params":{{"domain":{domain_obj},"on_true":[{{"type":"set","params":{{"attr":"meta_guard.ok","operation":"set","value":1}}}}],"on_false":[]}}}}]}}"#
        )
    }

    /// 端到端（导入面 + 装载面）：装载面为**双层门禁**——schema gate
    /// （[`Self::passes_schema_gate`]，文件级 fail-closed）先行拒收缺 on_missing /
    /// 非法声明 / 历史写作错误旧形态 value 的规则文件；walk 门禁（条目级 + 审计
    /// 透传）为纵深面。本测试锁死：违规文件不进合并列表；声明齐全规则、动态域
    /// （字符串形态豁免）与合法带点字面量正常放行。三道门的条目级判定语义由
    /// [`t4b_domain_gate_pure_function_nesting_and_exempt`] 纯函数直调锁死；
    /// 导入面（无 schema gate）由 ③.6 预判担主道（见导入预判测试）。
    #[test]
    fn t4b_domain_gate_end_to_end_reject_and_pass() {
        let tmp = tempfile::tempdir().unwrap();
        let rules_dir = tmp.path().join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();

        // door ①：缺声明
        std::fs::write(
            rules_dir.join("a-missing.json"),
            t4b_branch_rule(r#"{"type":"eq","path":"payload.status","value":"pending"}"#),
        )
        .unwrap();
        // door ②：声明值非法（不在 error/unsat 值域）
        std::fs::write(
            rules_dir.join("b-invalid.json"),
            t4b_branch_rule(
                r#"{"type":"lt","path":"payload.count","value":5,"on_missing":"warn"}"#,
            ),
        )
        .unwrap();
        // door ③：历史写作错误形态（value 像 exec 路径但缺 __ 前缀）
        std::fs::write(
            rules_dir.join("c-ambiguous.json"),
            t4b_branch_rule(
                r#"{"type":"eq","path":"payload.milestone","value":"instruction.params.milestone_target","on_missing":"unsat"}"#,
            ),
        )
        .unwrap();
        // 合规放行：显式 error / 显式 unsat / 动态域字符串（豁免）/ 合法带点字面量（零误伤）
        std::fs::write(
            rules_dir.join("d-ok.json"),
            r#"{"transform":[
                    {"type":"branch","params":{"domain":{"type":"eq","path":"payload.role","value":"admin","on_missing":"error"},"on_true":[{"type":"set","params":{"attr":"meta_guard.ok","operation":"set","value":1}}],"on_false":[]}},
                    {"type":"branch","params":{"domain":{"type":"lt","path":"payload.drift_count","value":3,"on_missing":"unsat"},"on_true":[{"type":"set","params":{"attr":"meta_guard.ok","operation":"set","value":1}}],"on_false":[]}},
                    {"type":"branch","params":{"domain":"__exec__.instruction.params.domain","on_true":[{"type":"set","params":{"attr":"meta_guard.ok","operation":"set","value":1}}],"on_false":[]}},
                    {"type":"branch","params":{"domain":{"type":"eq","path":"payload.scope","value":"meta_tool.pending_target_scope","on_missing":"unsat"},"on_true":[{"type":"set","params":{"attr":"meta_guard.ok","operation":"set","value":1}}],"on_false":[]}}
                ]}"#,
        )
        .unwrap();
        let core_eval_path = tmp.path().join("server_eval.json");
        std::fs::write(&core_eval_path, O135_CONSTITUTION).unwrap();

        let (rules, _layout, rejected_overlaps, rejected, _gate_files) =
            SessionApi::load_merged_with_layout(&core_eval_path, &rules_dir).expect("装载不应失败");

        assert!(rejected_overlaps.is_empty(), "本样本不涉 I/O 权利面");
        // schema gate 已把 a/b/c 三文件整体剔除（fail-closed），d-ok 4 条全放行：
        // kept = 宪法 2 + d-ok 4 = 6；walk 门禁（纵深面）无条目到达 → 拒载明细为空
        assert_eq!(
            rules.len(),
            6,
            "违规 3 文件被 schema gate 剔除，不进合并列表"
        );
        // 拒载明细逐文件各留 1 条汇总记录（fail-visible：剔除原因可查）；
        // 未被剔除的文件不再有条目级重复计数（walk 门禁纵深面零新增）
        assert_eq!(rejected.len(), 3, "被剔 3 文件各留 1 条拒载明细");
        for r in &rejected {
            assert!(
                r.source.starts_with("a-missing")
                    || r.source.starts_with("b-invalid")
                    || r.source.starts_with("c-ambiguous"),
                "拒载明细只含被剔文件: {}",
                r.source
            );
            assert_eq!(
                r.violations.len(),
                1,
                "每文件 1 条汇总明细（不逐条目重复计数）"
            );
        }
    }

    /// 纯函数：嵌套面（all.inner 内 eq 同检）+ value `__` 路径引用形态不误伤 +
    /// 宪法前缀段豁免（有违规也不拒载，同装载防线宪法优先语义）。
    #[test]
    fn t4b_domain_gate_pure_function_nesting_and_exempt() {
        // 嵌套：all.inner 内 eq 缺声明同检（walk 整棵树）
        let nested = o135_tcb(
            r#"{"transform":[
            {"type":"branch","params":{"domain":{"type":"all","inner":[
                {"type":"exists","path":"payload.a"},
                {"type":"eq","path":"payload.status","value":"x"}]},
             "on_true":[{"type":"set","params":{"attr":"meta_guard.ok","operation":"set","value":1}}],"on_false":[]}}
        ]}"#,
        );
        let violations = SessionApi::domain_gate_violations(&nested[0]);
        assert_eq!(violations.len(), 1, "嵌套 inner 内 eq 同检");
        assert_eq!(violations[0].door, "missing_declaration");

        // 门 ②：on_missing 非法值（不在 error/unsat 值域）逐条目判定
        let invalid = o135_tcb(
            r#"{"transform":[
            {"type":"branch","params":{"domain":{"type":"lt","path":"payload.count",
             "value":5,"on_missing":"warn"},
             "on_true":[{"type":"set","params":{"attr":"meta_guard.ok","operation":"set","value":1}}],"on_false":[]}}
        ]}"#,
        );
        let v2 = SessionApi::domain_gate_violations(&invalid[0]);
        assert_eq!(v2.len(), 1, "声明值非法=显式拒收，不静默回退缺省");
        assert_eq!(v2[0].door, "invalid_declaration");

        // 门 ③：历史写作错误形态 value（像 exec 路径但缺 __ 前缀）一律拒、无宽容选项
        let ambiguous = o135_tcb(
            r#"{"transform":[
            {"type":"branch","params":{"domain":{"type":"eq","path":"payload.milestone",
             "value":"instruction.params.milestone_target","on_missing":"unsat"},
             "on_true":[{"type":"set","params":{"attr":"meta_guard.ok","operation":"set","value":1}}],"on_false":[]}}
        ]}"#,
        );
        let v3 = SessionApi::domain_gate_violations(&ambiguous[0]);
        assert_eq!(v3.len(), 1, "历史写作错误形态必须可判");
        assert_eq!(v3[0].door, "value_literal_ambiguous");
        assert!(v3[0].detail.contains("instruction.params.milestone_target"));
        // 声明与写作错误同条并存时逐条开列（三道门独立判定）
        let both = o135_tcb(
            r#"{"transform":[
            {"type":"branch","params":{"domain":{"type":"eq","path":"payload.milestone",
             "value":"instruction.params.milestone_target","on_missing":"halt"},
             "on_true":[{"type":"set","params":{"attr":"meta_guard.ok","operation":"set","value":1}}],"on_false":[]}}
        ]}"#,
        );
        let vb = SessionApi::domain_gate_violations(&both[0]);
        assert_eq!(
            vb.len(),
            2,
            "invalid_declaration + value_literal_ambiguous 并存"
        );

        // value `__` 路径引用形态（合法 path_ref，盘点 northstar r3/r4 形态）不触发门 ③
        let path_ref = o135_tcb(
            r#"{"transform":[
            {"type":"branch","params":{"domain":{"type":"eq","path":"payload.a",
             "value":"__exec__.payload.milestone_target","on_missing":"unsat"},
             "on_true":[{"type":"set","params":{"attr":"meta_guard.ok","operation":"set","value":1}}],"on_false":[]}}
        ]}"#,
        );
        assert!(
            SessionApi::domain_gate_violations(&path_ref[0]).is_empty(),
            "__ 前缀路径引用是合法比较形态"
        );

        // 合法带点字面量（盘点实证 10 处形态：meta_*/MiniMax-*）零误伤
        let dotted = o135_tcb(
            r#"{"transform":[
            {"type":"branch","params":{"domain":{"type":"eq","path":"payload.model",
             "value":"MiniMax-M2.5","on_missing":"unsat"},
             "on_true":[{"type":"set","params":{"attr":"meta_guard.ok","operation":"set","value":1}}],"on_false":[]}}
        ]}"#,
        );
        assert!(SessionApi::domain_gate_violations(&dotted[0]).is_empty());

        // 非宪法条目违规 → 拒载；宪法条目（idx < constitution_len）违规 → 豁免放行
        let mut rules = o135_tcb(O135_CONSTITUTION);
        rules.push(nested[0].clone());
        let sources = vec![
            "core_eval".to_string(),
            "core_eval".to_string(),
            "x.json".to_string(),
        ];
        let (kept, _ks, rejected) = SessionApi::reject_undeclared_domain_rules(rules, sources, 2);
        assert_eq!(kept.len(), 2, "宪法 2 条保留，违规条目被拒");
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0].index, 2, "被拒下标 = 原始合并下标口径");
        assert_eq!(rejected[0].source, "x.json");

        let constitution_violation =
            o135_tcb(&t4b_branch_rule(r#"{"type":"eq","path":"p","value":"v"}"#))[0].clone();
        let (kept2, _ks2, rejected2) = SessionApi::reject_undeclared_domain_rules(
            vec![constitution_violation],
            vec!["core_eval".to_string()],
            1,
        );
        assert!(rejected2.is_empty(), "宪法前缀段豁免");
        assert_eq!(kept2.len(), 1);
    }

    /// in 域门（design-19）：三道共用门覆盖 in + 第 4 道 values_declaration 门 +
    /// values 元素字面量歧义 + 合法形态（含带点字面量元素）零误伤。
    #[test]
    fn t4b_domain_gate_in_domain_values_door_and_ambiguity() {
        // 合法 in（声明齐全 + 非空集合 + 无根段点元素）→ 零违规
        let in_ok = o135_tcb(
            r#"{"transform":[
            {"type":"branch","params":{"domain":{"type":"in","path":"payload.model",
             "values":["MiniMax-M2.5","MiniMax-M3"],"on_missing":"unsat"},
             "on_true":[{"type":"set","params":{"attr":"meta_guard.ok","operation":"set","value":1}}],"on_false":[]}}
        ]}"#,
        );
        assert!(
            SessionApi::domain_gate_violations(&in_ok[0]).is_empty(),
            "合法 in 形态零误伤"
        );

        // 共用门 ①②：in 缺 on_missing / 声明值非法同检
        let in_no_decl = o135_tcb(
            r#"{"transform":[
            {"type":"branch","params":{"domain":{"type":"in","path":"payload.model",
             "values":["a"]},
             "on_true":[{"type":"set","params":{"attr":"meta_guard.ok","operation":"set","value":1}}],"on_false":[]}}
        ]}"#,
        );
        let vi = SessionApi::domain_gate_violations(&in_no_decl[0]);
        assert_eq!(vi.len(), 1, "in 缺 on_missing 同检");
        assert_eq!(vi[0].door, "missing_declaration");

        let in_bad_decl = o135_tcb(
            r#"{"transform":[
            {"type":"branch","params":{"domain":{"type":"in","path":"payload.model",
             "values":["a"],"on_missing":"warn"},
             "on_true":[{"type":"set","params":{"attr":"meta_guard.ok","operation":"set","value":1}}],"on_false":[]}}
        ]}"#,
        );
        let vi = SessionApi::domain_gate_violations(&in_bad_decl[0]);
        assert_eq!(vi.len(), 1, "in 声明值非法同检");
        assert_eq!(vi[0].door, "invalid_declaration");

        // 第 4 道 values_declaration：缺失 / 非数组 / 空数组逐形态拒
        for (shape, desc) in [
            (
                r#"{"type":"in","path":"payload.model","on_missing":"unsat"}"#,
                "values 缺失",
            ),
            (
                r#"{"type":"in","path":"payload.model","values":"a","on_missing":"unsat"}"#,
                "values 非数组",
            ),
            (
                r#"{"type":"in","path":"payload.model","values":[],"on_missing":"unsat"}"#,
                "values 空数组",
            ),
        ] {
            let rule = o135_tcb(&format!(
                r#"{{"transform":[
                {{"type":"branch","params":{{"domain":{shape},
                 "on_true":[{{"type":"set","params":{{"attr":"meta_guard.ok","operation":"set","value":1}}}}],"on_false":[]}}}}
            ]}}"#
            ));
            let vi = SessionApi::domain_gate_violations(&rule[0]);
            assert_eq!(vi.len(), 1, "{desc} 必须可判");
            assert_eq!(vi[0].door, "values_declaration", "{desc}");
        }

        // 共用门 ③ 扩 values 字符串元素：根段点形态元素逐个拒
        let in_ambiguous = o135_tcb(
            r#"{"transform":[
            {"type":"branch","params":{"domain":{"type":"in","path":"payload.model",
             "values":["MiniMax-M3","instruction.params.model"],"on_missing":"unsat"},
             "on_true":[{"type":"set","params":{"attr":"meta_guard.ok","operation":"set","value":1}}],"on_false":[]}}
        ]}"#,
        );
        let vi = SessionApi::domain_gate_violations(&in_ambiguous[0]);
        assert_eq!(vi.len(), 1, "values 根段点元素必须可判");
        assert_eq!(vi[0].door, "value_literal_ambiguous");
        assert!(vi[0].detail.contains("instruction.params.model"));

        // 合法带点字面量元素（MiniMax-* 形态）零误伤
        let in_dotted = o135_tcb(
            r#"{"transform":[
            {"type":"branch","params":{"domain":{"type":"in","path":"payload.model",
             "values":["MiniMax-M2.5","MiniMax-M3"],"on_missing":"unsat"},
             "on_true":[{"type":"set","params":{"attr":"meta_guard.ok","operation":"set","value":1}}],"on_false":[]}}
        ]}"#,
        );
        assert!(SessionApi::domain_gate_violations(&in_dotted[0]).is_empty());
    }

    /// 导入前置预判（导入面）：缺声明 bundle 条目在落盘前被预判拒收，
    /// 明细归因到 `bundles/{bundle_id}/{entry_id}`。
    #[test]
    fn t4b_import_prediction_rejects_undeclared_entries() {
        let good = o135_tcb(&t4b_branch_rule(
            r#"{"type":"eq","path":"payload.role","value":"admin","on_missing":"error"}"#,
        ))[0]
            .clone();
        let bad = o135_tcb(&t4b_branch_rule(
            r#"{"type":"lt","path":"payload.count","value":5}"#,
        ))[0]
            .clone();
        let entries = vec![
            ("good-entry".to_string(), good),
            ("bad-entry".to_string(), bad),
        ];
        let predicted = SessionApi::predict_import_undeclared_domains("bdl-x", &entries);
        assert_eq!(predicted.len(), 1, "仅缺声明条目被预判拒收");
        assert_eq!(predicted[0].index, 1);
        assert_eq!(predicted[0].source, "bundles/bdl-x/bad-entry");
        assert_eq!(predicted[0].violations[0].door, "missing_declaration");
    }

    /// 接线点 ②（宪法 loader）装载语义已收口至 evorule_rule_loader（四步固定顺序），
    /// enforce/warn 两态语义由其纪律门测试覆盖；本测试直调纪律求值本体，
    /// 锁死 DC-02 违规样本（enforce 嵌入 branch 内）必被检出。
    #[test]
    fn discipline_gate_wired_into_core_eval_loader() {
        // DC-02 违规：enforce 嵌入 branch 内（{"transform":[...]} 包裹形态）
        let violating = r#"{"transform":[
            {"type":"branch","params":{"domain":{"type":"exists","path":"a"},
             "on_true":[{"type":"enforce","params":{"domain":{"type":"exists","path":"b"},"reason":"r"}}],
             "on_false":[]}}
        ]}"#;
        let tcb: Vec<JsonValue> = parse_transforms(violating)
            .into_iter()
            .map(serde_to_tcb)
            .collect();
        assert!(!tcb.is_empty(), "测试样本必须实际解析出规则");
        let report = evorule_discipline::check(&tcb).expect("纪律求值本身不得失败");
        assert!(
            !report.violations.is_empty(),
            "DC-02 嵌套 enforce 样本必须被纪律门禁检出"
        );
        assert!(
            report.violations.iter().any(|v| v.code.contains("DC-02")),
            "违规码应含 DC-02: {:?}",
            report.violations
        );
    }

    /// 回归锁：随包发布的宪法 server_eval.json 必须通过纪律门禁（0 违规）。
    /// R-A4/R-B1 的误报修复以 server 装载侧视角再次锁死——宪法违规会直接
    /// 拒载服务器自身的核心规则集。
    #[test]
    fn shipped_server_eval_passes_discipline_gate() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("resources")
            .join("server_eval.json");
        if !path.exists() {
            // 非随包检出（如裁剪仓）跳过；随包环境必须存在
            return;
        }
        let raw = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let arr = v
            .get("transform")
            .and_then(|t| t.as_array())
            .map(|a| a.to_vec())
            .unwrap_or_default();
        assert!(!arr.is_empty(), "随包宪法应有 transform 数组");
        let tcb: Vec<JsonValue> = arr.into_iter().map(serde_to_tcb).collect();
        let report = evorule_discipline::check(&tcb).unwrap();
        assert!(
            report.violations.is_empty(),
            "随包宪法不得含纪律违规: {:?}",
            report.violations
        );
    }

    #[test]
    fn test_rules_dir_load_order_promoted_before_seed() {
        // 注入序治理核心语义：晋升约束按 promoted_at 降序排前，种子（无 promoted_at）
        // 兜底其后 → 新约束 rule_index < 种子 rule_index，enforce 命中即终止时归因=新约束。
        // 文件名字典序种子在前（'e'<'p'），治理后必须反转。
        let dir = make_sort_test_rules_dir("promoted-first");
        write_rule_file(
            &dir,
            "00_constraint_evo_guard.json",
            &constraint_body("种子", None),
        );
        write_rule_file(
            &dir,
            "00_constraint_promoted_new.json",
            &constraint_body("新约束", Some("2026-09-22T08:00:00Z")),
        );
        write_rule_file(
            &dir,
            "00_constraint_promoted_old.json",
            &constraint_body("旧约束", Some("2026-09-21T08:00:00Z")),
        );

        let (parts, sources, _gate_files) = SessionApi::load_rules_dir_with_sources(&dir);

        let idx = |name: &str| {
            sources
                .iter()
                .position(|s| s == name)
                .unwrap_or_else(|| panic!("来源标签 {name} 缺失"))
        };
        assert_eq!(
            parts.len(),
            sources.len(),
            "部件与来源标签等长不变式（R5 per-part）"
        );
        assert!(
            idx("00_constraint_promoted_new.json") < idx("00_constraint_promoted_old.json"),
            "新晋升先于旧晋升（promoted_at 降序）"
        );
        assert!(
            idx("00_constraint_promoted_old.json") < idx("00_constraint_evo_guard.json"),
            "晋升约束排前，种子兜底其后"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_rules_dir_load_order_fallback_lexicographic() {
        // 兜底确定性：无 promoted_at 的文件（存量形态）按路径字典序；
        // promoted 前缀但字段缺失 → 退化兜底且不影响其余文件装载。
        let dir = make_sort_test_rules_dir("fallback");
        write_rule_file(
            &dir,
            "00_constraint_promoted_legacy.json",
            &constraint_body("存量", None),
        );
        write_rule_file(
            &dir,
            "10_b.json",
            r#"{"transform":[{"type":"set","params":{"attr":"b","operation":"set","value":"v"}}]}"#,
        );
        write_rule_file(
            &dir,
            "20_a.json",
            r#"{"transform":[{"type":"set","params":{"attr":"a","operation":"set","value":"v"}}]}"#,
        );

        let (_, sources, _gate_files) = SessionApi::load_rules_dir_with_sources(&dir);

        let names: Vec<&str> = sources.iter().map(String::as_str).collect();
        assert_eq!(
            names,
            vec![
                "00_constraint_promoted_legacy.json",
                "10_b.json",
                "20_a.json"
            ],
            "无 promoted_at 时按完整路径字典序兜底（确定性保持）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_rules_dir_load_order_promoted_tie_path_asc() {
        // 全序保证：promoted_at 平级时按路径字典序收敛（非晋升文件仍在其后）
        let dir = make_sort_test_rules_dir("tie");
        write_rule_file(
            &dir,
            "00_constraint_promoted_z.json",
            &constraint_body("平级乙", Some("2026-09-22T08:00:00Z")),
        );
        write_rule_file(
            &dir,
            "00_constraint_promoted_a.json",
            &constraint_body("平级甲", Some("2026-09-22T08:00:00Z")),
        );
        write_rule_file(
            &dir,
            "30_x.json",
            r#"{"transform":[{"type":"set","params":{"attr":"x","operation":"set","value":"v"}}]}"#,
        );

        let (_, sources, _gate_files) = SessionApi::load_rules_dir_with_sources(&dir);

        let names: Vec<&str> = sources.iter().map(String::as_str).collect();
        assert_eq!(
            names,
            vec![
                "00_constraint_promoted_a.json",
                "00_constraint_promoted_z.json",
                "30_x.json"
            ],
            "promoted_at 平级按路径字典序；非晋升文件兜底其后"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

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
    /// 插件审批代理 env 名映射（id → EVORULE_PLUGIN_ADMIN_TOKEN__<ID 大写,非字母数字→_>）
    #[test]
    fn test_plugin_admin_token_env_mapping() {
        assert_eq!(
            plugin_admin_token_env("finance-config"),
            "EVORULE_PLUGIN_ADMIN_TOKEN__FINANCE_CONFIG"
        );
        assert_eq!(
            plugin_admin_token_env("my_plugin.2"),
            "EVORULE_PLUGIN_ADMIN_TOKEN__MY_PLUGIN_2"
        );
        assert_eq!(plugin_admin_token_env("a"), "EVORULE_PLUGIN_ADMIN_TOKEN__A");
    }

    /// 批次A（P1-10 折叠撞名）: 不同 id 折叠撞同一 env 名 → true;
    /// 不同 env 名 / 同一 id → false（装载期 fail-fast 的纯函数判定基座）
    #[test]
    fn test_plugin_admin_token_env_collision() {
        assert!(plugin_admin_token_env_collides(
            "finance-config",
            "finance_config"
        ));
        assert!(plugin_admin_token_env_collides("a.b", "a-b"));
        assert!(!plugin_admin_token_env_collides(
            "finance-config",
            "finance-config2"
        ));
        assert!(!plugin_admin_token_env_collides(
            "finance-config",
            "finance-config"
        ));
        assert!(!plugin_admin_token_env_collides("a", "b"));
    }

    /// 批次B（P1-11 错误脱敏）: 链路错误串分类——timeout/connect 类
    /// 命中对应通用文案;含 URL/端口/响应片段的 raw Display 落"其他"通用文案
    /// （客户端永远只见分类文案,原文只进日志）
    #[test]
    fn test_classify_upstream_error() {
        // timeout 类
        assert_eq!(
            classify_upstream_error("http request failed: operation timed out"),
            "上游超时（服务未在时限内响应）"
        );
        assert_eq!(
            classify_upstream_error("Timeout while waiting"),
            "上游超时（服务未在时限内响应）"
        );
        // connect 类
        assert_eq!(
            classify_upstream_error(
                "error sending request for url (http://127.0.0.1:8848/services/x)"
            ),
            "上游连接失败（服务不可达）"
        );
        assert_eq!(
            classify_upstream_error("dns lookup failed for 'internal-host'"),
            "上游连接失败（服务不可达）"
        );
        assert_eq!(
            classify_upstream_error("os error 10061 connection refused"),
            "上游连接失败（服务不可达）"
        );
        // 其他类（含上游响应片段的 raw 面——一律通用文案）
        assert_eq!(
            classify_upstream_error(
                "http request failed with status: 500 Internal Server Error, body: {\"secret\":\"...\"}"
            ),
            "上游请求失败"
        );
        assert_eq!(
            classify_upstream_error("service chain 未装配"),
            "上游请求失败"
        );
    }

    /// approver 强制注入:AuthedActor 存在取注入值,缺失（认证关闭）记 anonymous;
    /// 请求体自报 approver 由代理端点无条件覆盖（防伪造操作者,handler 内 m.insert 语义）。
    #[test]
    fn test_resolve_approver() {
        assert_eq!(
            resolve_approver(Some(Extension(crate::api::platform_auth::AuthedActor(
                "admin".to_string()
            )))),
            "admin"
        );
        assert_eq!(
            resolve_approver(Some(Extension(crate::api::platform_auth::AuthedActor(
                "static-user".to_string()
            )))),
            "static-user"
        );
        assert_eq!(resolve_approver(None), "anonymous");
    }

    /// 代理 fail-fast:未知/非 external 插件 → 404（不转发）
    #[tokio::test]
    async fn test_plugin_admin_proxy_unknown_plugin_404() {
        let core_eval = vec![JsonValue::Object(std::collections::BTreeMap::new())];
        let sessions = SessionApi::new(core_eval, 10);
        let r =
            proxy_plugin_admin(&sessions, "ghost", "proposals", reqwest::Method::GET, None).await;
        let (status, body) = r.unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.0.to_string().contains("unknown plugin"));
    }

    /// 代理 fail-fast:server 侧 token 未配置 → 503（附 env 名指引,不静默转发）
    #[tokio::test]
    async fn test_plugin_admin_proxy_token_missing_503() {
        let core_eval = vec![JsonValue::Object(std::collections::BTreeMap::new())];
        let mut admins = std::collections::BTreeMap::new();
        admins.insert(
            "finance-config".to_string(),
            PluginAdminEndpoint {
                base_url: "http://127.0.0.1:9".to_string(),
                admin_token: None,
            },
        );
        let sessions = SessionApi::new(core_eval, 10).with_plugin_admins(admins);
        let r = proxy_plugin_admin(
            &sessions,
            "finance-config",
            "proposals",
            reqwest::Method::GET,
            None,
        )
        .await;
        let (status, body) = r.unwrap_err();
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let msg = body.0.to_string();
        assert!(msg.contains("EVORULE_PLUGIN_ADMIN_TOKEN__FINANCE_CONFIG"));
    }

    /// 服务对账同名去重：registry 回落绑定与 native/plugin 源同名时，
    /// 对账清单必须只保留 native/plugin 条目（带 sensitive/parameters/plugin
    /// 全元数据），消费方按名索引不再有命中缺元数据条目的形态；仅 registry
    /// 绑定的服务不受去重影响，照常列出。
    /// 回归（2026-10-22）：`SessionApi::with_ruleset_version` 必须在 **tokio runtime 内**
    /// 安全调用——生产调用点在 `#[tokio::main] async fn main` 体内。
    /// 旧实现用 `blocking_lock()`，在 runtime 线程上调用即 panic
    /// （"Cannot block the current thread from within a runtime"）⇒ **服务启动即崩**。
    /// 该缺陷由 903b13b（2026-10-22 19:07）引入，2026-10-22 实机验证发现。
    ///
    /// 覆盖缺口说明：`patches/evorule-governance/src/session.rs` 的
    /// `ruleset_version_anchored_into_chain` 走的是 **SessionManager 自身的 by-value
    /// builder**，**未覆盖 SessionApi 这层包装** ⇒ 该 panic 漏过全部测试。
    #[tokio::test]
    async fn test_with_ruleset_version_safe_inside_tokio_runtime() {
        let mut instr = std::collections::BTreeMap::new();
        instr.insert("type".to_string(), JsonValue::string("noop"));
        let core_eval = vec![JsonValue::Object(instr)];
        // 旧实现在此 panic —— 本用例即崩溃点
        let api = SessionApi::new(core_eval, 100)
            .with_ruleset_version("blake3:test-ruleset".to_string())
            .expect("启动装配期锁应无竞争");
        // 注入确实落到会话管理器上（创建会话后链上应有 __ruleset__.version 锚定事实）
        let _ = api;
    }

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

        let list = list_services_handler(axum::extract::State(sessions))
            .await
            .0;

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
        assert_eq!(
            only.source, "registry",
            "仅 registry 绑定的服务不被去重误伤"
        );
    }

    // ===== G-A2 锚点三端点（未启用态：与旧版行为一致） =====

    #[tokio::test]
    async fn g_a2_anchor_endpoints_disabled_by_default() {
        let (state, _) = make_test_state();
        let router = make_test_router(&state);

        let (status, body) = oneshot_json(router.clone(), "POST", "/api/sessions", None).await;
        assert_eq!(status, StatusCode::OK);
        let sid = body["session_id"].as_u64().unwrap();

        // GET /anchors：空表（未启用）
        let (status, body) = oneshot_json(
            router.clone(),
            "GET",
            &format!("/api/sessions/{sid}/anchors"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "anchors 应成功: {body}");
        assert_eq!(body["count"], 0, "未启用锚点链 count 必须为 0");

        // POST /anchors/seal：409（与未启用可区分，不与 404 混淆）
        let (status, _) = oneshot_json(
            router.clone(),
            "POST",
            &format!("/api/sessions/{sid}/anchors/seal"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "未启用时 seal 必须 409");

        // POST /anchors/verify：0 锚点结构验证通过
        let (status, body) = oneshot_json(
            router,
            "POST",
            &format!("/api/sessions/{sid}/anchors/verify"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "verify 应成功: {body}");
        // P0-3（G4）：空锚点链 valid=null（"没封过锚点"≠"验过了"）
        assert!(
            body["valid"].is_null(),
            "空锚点链 valid 必须为 null: {body}"
        );
        assert_eq!(body["anchor_count"], 0);

        // 404 面：不存在会话
        let (state2, _) = make_test_state();
        let router2 = make_test_router(&state2);
        let (status, _) = oneshot_json(router2, "GET", "/api/sessions/999999/anchors", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// fork_session_with_rules 三件机制契约（沙盒编排断链修复回归锚）：
    /// ①权利面冲突 fail-fast 拒启（草稿 io_type 与宪法冲突）；②无冲突草稿
    /// fork 成功且全局 core_eval 全程不变（草稿经 override 仅进沙盒
    /// Reactor）；③fork 后沙盒事实链只含 M-4 基态血统锚（0.10.0 正本语义：
    /// fork 基态 digest 锚定入链；测试管理器未注入 ruleset_version 故无 G6
    /// 锚）——状态快照仍非事实历史，报告判定面前提=前导锚落入噪声组。
    #[tokio::test]
    async fn fork_with_rules_conflict_reject_and_restore() {
        use evorule_workspace::{DraftRulesProvenance, SessionOps as _};

        // 宪法声明 io_type "shared_io"（发射面）
        let constitution = serde_json::json!({
            "transform": [{"type": "io_request", "params": {"io_type": "shared_io"}}]
        });
        let api = SessionApi::new(vec![serde_to_tcb(constitution)], 100);
        let parent = api.create_session().await.unwrap();
        // R4：测试治理指纹（标签 ID；真实链路由 start_sandbox 传 DB 版本 ID）
        let prov = |vid: &str| DraftRulesProvenance {
            rule_version_ids: vec![vid.to_string()],
            ruleset_hash: "blake3:test-draft-ruleset".to_string(),
        };

        // ① 草稿声明同一 io_type → 权利面冲突拒启
        let draft_conflict = serde_json::json!({
            "transform": [{"type": "io_request", "params": {"io_type": "shared_io"}}]
        });
        let err = api
            .fork_session_with_rules(parent, &[draft_conflict], &prov("test-draft-0"))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("权利面冲突"),
            "io 冲突应拒启沙盒, got: {err}"
        );

        // ② 无冲突草稿（独立 io_type + 纯状态变换）→ fork 成功, 全局规则集不变
        let draft_ok = serde_json::json!({
            "transform": [
                {"type": "io_request", "params": {"io_type": "draft_only_io"}},
                {"type": "set", "params": {"attr": "__exec__.payload.x", "operation": "set", "value": 1}},
            ]
        });
        let before = api.sessions.lock().await.core_eval().to_vec();
        let sandbox = api
            .fork_session_with_rules(parent, &[draft_ok], &prov("test-draft-0"))
            .await
            .unwrap();
        let after = api.sessions.lock().await.core_eval().to_vec();
        assert_eq!(before, after, "正统 API 挂载，全局 core_eval 全程不变");

        // ③ fork 后事实链只含 M-4 基态血统锚（探针契约；0.10.0 正本语义）
        let facts = api.get_facts(sandbox).await.unwrap();
        assert_eq!(facts.len(), 1, "fork 链应只含 M-4 基态锚: {facts:?}");
        assert_eq!(facts[0]["path"], "__fork__.initial_state");
    }

    /// 宪法不容替换（SR-03 契约锚）：fork 注入 override 必须=全局规则集（含宪法）
    /// + 过门草稿的全集，而非草稿顶掉宪法。验证面：沙盒会话实际挂载的规则集
    /// 含宪法前缀段；草稿携 enforce（治理旁路）被 tier 门 fail-fast 拒启。
    #[tokio::test]
    async fn fork_with_rules_constitution_not_replaced() {
        use evorule_workspace::{DraftRulesProvenance, SessionOps as _};

        // 宪法：一条 set 规则（可识别标记）
        let constitution = serde_json::json!({
            "transform": [{"type": "set", "params": {"attr": "constitution_mark", "operation": "set", "value": "present"}}]
        });
        // 剥壳（夹具对齐 production 装载面形态；壳对象会被 TCB 拒）
        let rules: Vec<serde_json::Value> = constitution["transform"].as_array().unwrap().clone();
        let api = SessionApi::new(rules.iter().map(|r| serde_to_tcb(r.clone())).collect(), 100);
        let parent = api.create_session().await.unwrap();
        // R4：测试治理指纹（标签 ID；真实链路由 start_sandbox 传 DB 版本 ID）
        let prov = |vid: &str| DraftRulesProvenance {
            rule_version_ids: vec![vid.to_string()],
            ruleset_hash: "blake3:test-draft-ruleset".to_string(),
        };

        // ① 沙盒草稿携 enforce → tier 门拒启（B2 消解：沙盒非 L2，禁止治理旁路原语）
        let draft_enforce = serde_json::json!({
            "transform": [{
                "type": "enforce",
                "params": {"domain": {"type": "all", "inner": []}, "reason": "sandbox hijack attempt"}
            }]
        });
        let err = api
            .fork_session_with_rules(parent, &[draft_enforce], &prov("test-draft-0"))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("装载门禁未过"),
            "沙盒草稿携 enforce 应被 tier 门拒启, got: {err}"
        );

        // ② 合法草稿 fork 成功 → 沙盒 Reactor 挂载全集（宪法前缀+草稿后缀）。
        // 验证法=执行面：Session 无会话级规则集 getter（Reactor 内部持有），
        // 提交一条指令，宪法 set 规则若在位必写 constitution_mark（探针）。
        let draft_ok = serde_json::json!({
            "transform": [
                {"type": "set", "params": {"attr": "draft_mark", "operation": "set", "value": 1}}
            ]
        });
        let sandbox = api
            .fork_session_with_rules(parent, &[draft_ok], &prov("test-draft-0"))
            .await
            .unwrap();
        api.send_command(sandbox, serde_json::json!({"type": "noop", "params": {}}))
            .await
            .unwrap();
        // 反应器异步处理，轮询 state 至多 ~2s（state 结构={payload,queue,version}）
        let mut payload = serde_json::Value::Null;
        for _ in 0..40 {
            let state = api.get_session_state(sandbox).await.unwrap();
            payload = state.get("payload").cloned().unwrap_or_default();
            if payload.get("constitution_mark").is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(
            payload.get("constitution_mark").and_then(|v| v.as_str()),
            Some("present"),
            "宪法 set 规则必须在沙盒生效（不容替换）: {payload}"
        );
    }

    fn make_test_state() -> (AppState, ReadinessFlag) {
        let mut instr = std::collections::BTreeMap::new();

        instr.insert("type".to_string(), JsonValue::string("noop"));

        let core_eval = vec![JsonValue::Object(instr)];

        make_test_state_with_core_eval(core_eval)
    }

    /// 可自定义宪法的测试 state 构造（wait=true 集成测用 set 分派宪法——
    /// 缺省 noop 宪法非合法 transform，指令一律落无 cause 的 Error，
    /// StateTransition 永不落链，wait 等待无从谈起）
    fn make_test_state_with_core_eval(core_eval: Vec<JsonValue>) -> (AppState, ReadinessFlag) {
        let (state, readiness, _ws_db) = make_test_state_full(core_eval, SharedFactsLog::new());
        (state, readiness)
    }

    /// 同 [`Self::make_test_state_with_core_eval`]，另带 workspace 元数据库
    /// 句柄（发布域治理端点测试用：直接初始化 production_state 等）。
    fn make_test_state_full(
        core_eval: Vec<JsonValue>,
        shared_facts: SharedFactsLog,
    ) -> (AppState, ReadinessFlag, Arc<evorule_workspace::WorkspaceDb>) {
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

        // 生产同构接线（main.rs with_shared_facts）: SessionApi 与 AppState
        // 共享同一实例——平台会话/平台事件与 SessionApi 侧读写同源。
        // 实例由调用方注入（W-4 磁盘满演练需注入 WAL-backed 实例）。
        let sessions = sessions.with_shared_facts(shared_facts.clone());

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
            ws_db,
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
            false,
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

    // --- 会话级干预端点（pause/heartbeat/gc） ---

    /// pause 准入闸：paused 会话拒新 Command（SESSION_PAUSED），resume 恢复；
    /// pause 对不存在会话 404；pause 后心跳仍放行（监控信号不受闸）。
    #[tokio::test]
    async fn intervention_pause_gates_commands_resume_restores() {
        let (state, _) = make_test_state();
        let router = make_test_router(&state);

        let (status, body) = oneshot_json(router.clone(), "POST", "/api/sessions", None).await;
        assert_eq!(status, StatusCode::OK);
        let sid = body["session_id"].as_u64().unwrap();

        // 基线：noop 指令成功
        let cmd_uri = format!("/api/sessions/{sid}/command");
        let (status, body) = oneshot_json(
            router.clone(),
            "POST",
            &cmd_uri,
            Some(r#"{"instruction":{"type":"noop"}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], true, "基线命令应成功: {body}");

        // 初始闸位：未暂停
        let (status, body) = oneshot_json(
            router.clone(),
            "GET",
            &format!("/api/sessions/{sid}/pause"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["paused"], false);

        // 不存在会话 pause → 404
        let (status, _) =
            oneshot_json(router.clone(), "POST", "/api/sessions/999999/pause", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // 置位 → 命令被闸（success:false + SESSION_PAUSED，指令不进引擎）
        let (status, body) = oneshot_json(
            router.clone(),
            "POST",
            &format!("/api/sessions/{sid}/pause"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["paused"], true);

        let (status, body) = oneshot_json(
            router.clone(),
            "POST",
            &cmd_uri,
            Some(r#"{"instruction":{"type":"noop"}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], false, "paused 会话命令必须被拒: {body}");
        assert_eq!(body["code"], "SESSION_PAUSED");

        // 心跳不受闸（监控信号放行面）
        let (status, body) = oneshot_json(
            router.clone(),
            "POST",
            &format!("/api/sessions/{sid}/heartbeat"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "pause 不拦截心跳: {body}");
        assert_eq!(body["injected"], true);

        // 解除 → 命令恢复
        let (status, body) = oneshot_json(
            router.clone(),
            "POST",
            &format!("/api/sessions/{sid}/resume"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["paused"], false);

        let (status, body) = oneshot_json(
            router.clone(),
            "POST",
            &cmd_uri,
            Some(r#"{"instruction":{"type":"noop"}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], true, "resume 后命令应恢复: {body}");
    }

    /// 心跳限频：60s 窗口内第二次 429；窗口过期后放行；不存在会话 404 兜底。
    #[tokio::test]
    async fn intervention_heartbeat_rate_limited() {
        let (state, _) = make_test_state();
        let router = make_test_router(&state);

        let (status, body) = oneshot_json(router.clone(), "POST", "/api/sessions", None).await;
        assert_eq!(status, StatusCode::OK);
        let sid = body["session_id"].as_u64().unwrap();
        let uri = format!("/api/sessions/{sid}/heartbeat");

        let (status, body) = oneshot_json(router.clone(), "POST", &uri, None).await;
        assert_eq!(status, StatusCode::OK, "首次心跳应成功: {body}");
        assert_eq!(body["injected"], true);

        let (status, _) = oneshot_json(router.clone(), "POST", &uri, None).await;
        assert_eq!(
            status,
            StatusCode::TOO_MANY_REQUESTS,
            "窗口内第二次必须 429"
        );

        // 窗口过期（测试直接回拨限频账）→ 放行
        {
            let mut last = state
                .sessions
                .heartbeat_last
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            last.insert(
                sid,
                std::time::Instant::now() - std::time::Duration::from_secs(61),
            );
        }
        let (status, body) = oneshot_json(router.clone(), "POST", &uri, None).await;
        assert_eq!(status, StatusCode::OK, "窗口过期后应放行: {body}");

        // 不存在会话：回拨绕过限频后 404 兜底
        {
            let mut last = state
                .sessions
                .heartbeat_last
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            last.insert(
                888888,
                std::time::Instant::now() - std::time::Duration::from_secs(61),
            );
        }
        let (status, _) = oneshot_json(
            router.clone(),
            "POST",
            "/api/sessions/888888/heartbeat",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// gc 双保险：默认不注册路由（404）；干预开关启用后可压缩，响应带
    /// ratio/compacted_info；不存在会话 404。
    #[tokio::test]
    async fn intervention_gc_gated_behind_flag() {
        let (state, _) = make_test_state();

        // 默认 Router：gc 路由不存在
        let (status, _) =
            oneshot_json(make_test_router(&state), "POST", "/api/sessions/1/gc", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "默认必须 404（双保险）");

        // 启用干预端点的 Router（镜像 make_abort_router 构造，末位 bool=干预开关）
        let router = GovernanceServer::new(
            state.clone(),
            AuthConfig::disabled(),
            "0.0.0.0:0".to_string(),
            0,
            0,
            vec![],
            false,
            false,
            false,
            true,
            None,
        )
        .build_router();

        let (status, body) = oneshot_json(router.clone(), "POST", "/api/sessions", None).await;
        assert_eq!(status, StatusCode::OK);
        let sid = body["session_id"].as_u64().unwrap();

        let (status, body) = oneshot_json(
            router.clone(),
            "POST",
            &format!("/api/sessions/{sid}/gc"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "gc 应成功: {body}");
        assert_eq!(body["compacted"], true);
        assert!(body["ratio"].is_number());

        let (status, _) =
            oneshot_json(router.clone(), "POST", "/api/sessions/999999/gc", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// cancel_all 双保险 + 扫描/轮换 smoke：cancel_all 默认 404（高危门控），
    /// 启用后空取消返回空清单；scan 返回干净报告；wal_rotate 内存模式返回
    /// rotated:false；不存在会话一律 404。
    #[tokio::test]
    async fn intervention_cancel_all_scan_wal_rotate_smoke() {
        let (state, _) = make_test_state();

        // 默认 Router：cancel_all 路由不存在（高危双保险）
        let (status, _) = oneshot_json(
            make_test_router(&state),
            "POST",
            "/api/sessions/1/io/cancel_all",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "默认必须 404（双保险）");

        // 启用干预端点的 Router
        let router = GovernanceServer::new(
            state.clone(),
            AuthConfig::disabled(),
            "0.0.0.0:0".to_string(),
            0,
            0,
            vec![],
            false,
            false,
            false,
            true,
            None,
        )
        .build_router();

        let (status, body) = oneshot_json(router.clone(), "POST", "/api/sessions", None).await;
        assert_eq!(status, StatusCode::OK);
        let sid = body["session_id"].as_u64().unwrap();

        // cancel_all：无 pending → 空清单
        let (status, body) = oneshot_json(
            router.clone(),
            "POST",
            &format!("/api/sessions/{sid}/io/cancel_all"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "cancel_all 应成功: {body}");
        assert_eq!(body["count"], 0);
        assert!(body["requested"].as_array().unwrap().is_empty());

        // invariants/scan：干净会话 → 空违规
        let (status, body) = oneshot_json(
            router.clone(),
            "POST",
            &format!("/api/sessions/{sid}/invariants/scan"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "scan 应成功: {body}");
        assert_eq!(body["violation_count"], 0);
        assert!(body["violations"].as_array().unwrap().is_empty());
        assert_eq!(body["recorded"], 0);

        // wal/rotate：测试态纯内存（无 WAL）→ rotated:false
        let (status, body) = oneshot_json(
            router.clone(),
            "POST",
            &format!("/api/sessions/{sid}/wal/rotate"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "wal_rotate 应成功: {body}");
        assert_eq!(body["rotated"], false);

        // 不存在会话：三个端点一律 404
        for uri in [
            "/api/sessions/999999/io/cancel_all",
            "/api/sessions/999999/invariants/scan",
            "/api/sessions/999999/wal/rotate",
        ] {
            let (status, _) = oneshot_json(router.clone(), "POST", uri, None).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{uri} 应 404");
        }
    }

    /// 审批模式开关：缺省关；未认证 401；管理员 bootstrap 登录后可切换
    /// （即切即查）；非管理员 403。
    #[tokio::test]
    async fn publish_approve_mode_admin_gated() {
        let (state, _) = make_test_state();
        let router = make_test_router(&state);

        // 缺省关（publish_settings 缺行）
        let (status, body) =
            oneshot_json(router.clone(), "GET", "/api/publish/approve_mode", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["approve_mode"], false);

        // 未认证切换 → 401
        let (status, _) = oneshot_json(
            router.clone(),
            "POST",
            "/api/publish/approve_mode",
            Some(r#"{"enabled":true}"#),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // bootstrap 管理员 + 登录
        let (status, body) = oneshot_json(
            router.clone(),
            "POST",
            "/api/platform/auth/bootstrap",
            Some(r#"{"username":"root","password":"admin-pass-123","display_name":""}"#),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let (status, body) = oneshot_json(
            router.clone(),
            "POST",
            "/api/platform/auth/login",
            Some(r#"{"username":"root","password":"admin-pass-123"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let admin_token = body["token"].as_str().unwrap().to_string();

        // 管理员开启 → 即查即得
        let (status, body) = oneshot_json_with_token(
            router.clone(),
            "POST",
            "/api/publish/approve_mode",
            &admin_token,
            Some(r#"{"enabled":true}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["approve_mode"], true);
        let (status, body) = oneshot_json_with_token(
            router.clone(),
            "GET",
            "/api/publish/approve_mode",
            &admin_token,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["approve_mode"], true);

        // 非管理员（viewer）→ 403
        let (status, body) = oneshot_json_with_token(
            router.clone(),
            "POST",
            "/api/platform/users",
            &admin_token,
            Some(
                r#"{"username":"carol","password":"carol-pass-123","display_name":"","email":"","department":"","role":"viewer"}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let (status, body) = oneshot_json(
            router.clone(),
            "POST",
            "/api/platform/auth/login",
            Some(r#"{"username":"carol","password":"carol-pass-123"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let carol_token = body["token"].as_str().unwrap().to_string();
        let (status, _) = oneshot_json_with_token(
            router.clone(),
            "POST",
            "/api/publish/approve_mode",
            &carol_token,
            Some(r#"{"enabled":false}"#),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "非管理员必须 403");

        // 关闭回落（管理员）
        let (status, body) = oneshot_json_with_token(
            router.clone(),
            "POST",
            "/api/publish/approve_mode",
            &admin_token,
            Some(r#"{"enabled":false}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["approve_mode"], false);
    }

    // ===== 启动期 WAL 全链校验健康位（P1 W-5）=====

    #[test]
    fn wal_verify_status_str_mapping() {
        assert_eq!(wal_verify_status_str(WAL_VERIFY_SKIPPED), "skipped");
        assert_eq!(wal_verify_status_str(WAL_VERIFY_OK), "ok");
        assert_eq!(wal_verify_status_str(WAL_VERIFY_FAILED), "failed");
        // 未知值按 skipped 处理（不猜不造）
        assert_eq!(wal_verify_status_str(99), "skipped");
    }

    #[test]
    fn wal_verify_health_defaults_to_skipped() {
        // 静态位缺省 skipped： WalVerifyHealth 组装路径逐字段锁定
        let v = WalVerifyHealth {
            reactor: wal_verify_status_str(
                WAL_VERIFY_REACTOR_STATUS.load(std::sync::atomic::Ordering::Relaxed),
            )
            .to_string(),
            shared_facts: wal_verify_status_str(
                WAL_VERIFY_SHARED_FACTS_STATUS.load(std::sync::atomic::Ordering::Relaxed),
            )
            .to_string(),
        };
        assert_eq!(
            v,
            WalVerifyHealth {
                reactor: "skipped".to_string(),
                shared_facts: "skipped".to_string(),
            }
        );
        // setter 写入后按位呈现
        set_wal_verify_reactor(WAL_VERIFY_FAILED);
        set_wal_verify_shared_facts(WAL_VERIFY_OK);
        assert_eq!(
            wal_verify_status_str(
                WAL_VERIFY_REACTOR_STATUS.load(std::sync::atomic::Ordering::Relaxed)
            ),
            "failed"
        );
        assert_eq!(
            wal_verify_status_str(
                WAL_VERIFY_SHARED_FACTS_STATUS.load(std::sync::atomic::Ordering::Relaxed)
            ),
            "ok"
        );
        // 还原，避免污染同进程其他用例对缺省值的观察
        set_wal_verify_reactor(WAL_VERIFY_SKIPPED);
        set_wal_verify_shared_facts(WAL_VERIFY_SKIPPED);
    }

    // ===== 磁盘满演练（P1 W-4；方案 A：OS 级目录注入，2026-10-18 报批通过）=====

    /// 磁盘满演练·机制链：磁盘满在写路径的表现=持续写失败（同一错误分支：
    /// open/write Err → WalError → 连续计数）。目录冒充轮换目标制造持续失败，
    /// 验证经本工作区依赖解析（vendored patch）后的阈值=3 升级行为链：
    /// 连续计数递增 → 达阈值错误升级含自助诊断 → 升级回调恰好一次。
    /// 与 patch 内 W1 回归测试同构，此处锁定 server 侧依赖解析下的行为。
    #[test]
    fn wal_disk_full_drill_write_failure_chain_escalates_at_threshold() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        // SSOT: evorule_reactor facts_log::WAL_FAIL_TERMINATE_THRESHOLD=3
        // （模块私有不可达；值经 patch 单测与 W1 回归锁定，漂移时下方
        // 阈值后断言会失准并在此暴露）
        const WAL_FAIL_TERMINATE_THRESHOLD: u64 = 3;

        let base = tempfile::tempdir().unwrap();
        let wal_path = base.path().join("drill.wal");

        // max=1 字节：首条 append 后即触发轮换条件，后续 append 在 rotate 处失败
        let log = FactsLog::with_wal_options(&wal_path, 1, false).unwrap();
        log.append(Fact::Command {
            id: FactId(1),
            instruction: JsonValue::empty_object(),
        })
        .unwrap(); // 基线写入成功（写后触发轮换条件）

        // 预置足够多轮换目标目录：rotate 单次失败后 seq 自增"意外自愈"，
        // 须覆盖阈值次数的连续失败（与 patch 内 W1 回归同款手法）
        for seq in 1..=(WAL_FAIL_TERMINATE_THRESHOLD + 3) {
            std::fs::create_dir(base.path().join(format!("drill.{seq}.wal"))).unwrap();
        }

        let escalated = std::sync::Arc::new(AtomicUsize::new(0));
        let cb = escalated.clone();
        log.set_on_wal_failure_exhausted(move |guidance| {
            cb.fetch_add(1, Ordering::SeqCst);
            assert!(
                guidance.contains("磁盘"),
                "升级指导应含磁盘自诊 hints: {guidance}"
            );
        });

        for i in 1..=WAL_FAIL_TERMINATE_THRESHOLD {
            let result = log.append(Fact::Command {
                id: FactId(i + 10),
                instruction: JsonValue::empty_object(),
            });
            let err_msg = format!("{}", result.unwrap_err());
            if i < WAL_FAIL_TERMINATE_THRESHOLD {
                assert!(err_msg.contains('#'), "阈值前错误应携带连续计数: {err_msg}");
            } else {
                assert!(
                    err_msg.contains("自助诊断") || err_msg.contains("排查"),
                    "达阈值错误应升级含自助指导: {err_msg}"
                );
            }
        }
        assert_eq!(
            escalated.load(Ordering::SeqCst),
            1,
            "升级回调应恰在达阈值时触发一次"
        );
    }

    /// 磁盘满演练·治理面：共享事实通路轮换失败（磁盘满同构：轮换目标预置
    /// 为目录）时，checkpoint 端点 partial failure 逐项上报——shared_facts
    /// 项携带 error、HTTP 恒 200，其余通路（纯内存 rotated:false，非失败）
    /// 不受牵连；故障后 append 仍可用（治理落账不丢）。
    #[tokio::test]
    async fn wal_disk_full_drill_checkpoint_reports_partial_failure() {
        let dir = tempfile::tempdir().unwrap();
        // WAL-backed 共享事实：append 正常（bootstrap/登录/落账事件可写），
        // 仅轮换目标被注入为目录 → rotate_wal 必败
        let shared = SharedFactsLog::with_wal(dir.path().join("shared_facts.wal")).unwrap();
        std::fs::create_dir(dir.path().join("shared_facts.1.wal")).unwrap();

        let core_eval = {
            let mut instr = std::collections::BTreeMap::new();
            instr.insert("type".to_string(), JsonValue::string("noop"));
            vec![JsonValue::Object(instr)]
        };
        let (state, _, _) = make_test_state_full(core_eval, shared);
        let router = make_test_router(&state);

        // bootstrap 管理员 + 登录（事件经 WAL-backed 共享事实落账，正常写路径）
        let (status, body) = oneshot_json(
            router.clone(),
            "POST",
            "/api/platform/auth/bootstrap",
            Some(r#"{"username":"root","password":"admin-pass-123","display_name":""}"#),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let (status, body) = oneshot_json(
            router.clone(),
            "POST",
            "/api/platform/auth/login",
            Some(r#"{"username":"root","password":"admin-pass-123"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let admin_token = body["token"].as_str().unwrap().to_string();

        let (status, body) = oneshot_json_with_token(
            router,
            "POST",
            "/api/admin/wal/checkpoint",
            &admin_token,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        // 演练核心断言：故障通路逐项上报 error，整体不 fail
        let shared_err = body["shared_facts"]["error"].as_str().unwrap_or("");
        assert!(
            !shared_err.is_empty(),
            "shared_facts 通路应上报 error: {body}"
        );
        // 其余通路不受牵连：反应器纯内存 rotated:false（非失败）；会话 0 失败
        assert_eq!(body["reactor"]["rotated"], false, "{body}");
        assert_eq!(body["sessions"]["failed"], 0, "{body}");
    }

    /// 全通道 WAL checkpoint：未认证 401；管理员可执行（测试态纯内存 →
    /// 三通路 rotated:false 非失败）；非管理员 403。
    #[tokio::test]
    async fn admin_wal_checkpoint_admin_gated() {
        let (state, _) = make_test_state();
        let router = make_test_router(&state);

        // 未认证 → 401
        let (status, _) =
            oneshot_json(router.clone(), "POST", "/api/admin/wal/checkpoint", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // bootstrap 管理员 + 登录
        let (status, body) = oneshot_json(
            router.clone(),
            "POST",
            "/api/platform/auth/bootstrap",
            Some(r#"{"username":"root","password":"admin-pass-123","display_name":""}"#),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let (status, body) = oneshot_json(
            router.clone(),
            "POST",
            "/api/platform/auth/login",
            Some(r#"{"username":"root","password":"admin-pass-123"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let admin_token = body["token"].as_str().unwrap().to_string();

        // 建一个会话（测试态纯内存，无 WAL）让会话通路有对象可轮换
        let (status, body) = oneshot_json(router.clone(), "POST", "/api/sessions", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");

        // 管理员执行 → 200，三通路逐项上报（纯内存 rotated:false 非失败）
        let (status, body) = oneshot_json_with_token(
            router.clone(),
            "POST",
            "/api/admin/wal/checkpoint",
            &admin_token,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["sessions"]["total"], 1);
        assert_eq!(body["sessions"]["failed"], 0);
        assert_eq!(body["sessions"]["items"][0]["rotated"], false);
        assert_eq!(body["reactor"]["rotated"], false);
        assert_eq!(body["shared_facts"]["rotated"], false);

        // 非管理员（viewer）→ 403
        let (status, body) = oneshot_json_with_token(
            router.clone(),
            "POST",
            "/api/platform/users",
            &admin_token,
            Some(
                r#"{"username":"carol","password":"carol-pass-123","display_name":"","email":"","department":"","role":"viewer"}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let (status, body) = oneshot_json(
            router.clone(),
            "POST",
            "/api/platform/auth/login",
            Some(r#"{"username":"carol","password":"carol-pass-123"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let carol_token = body["token"].as_str().unwrap().to_string();
        let (status, _) = oneshot_json_with_token(
            router.clone(),
            "POST",
            "/api/admin/wal/checkpoint",
            &carol_token,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "非管理员必须 403");
    }

    /// 发布试验会话：未初始化生产会话（current_session_id 缺省 NULL）→ 409。
    /// fork 成功通路由 create_session_from_parent 端点集成测试覆盖（同一原语）。
    #[tokio::test]
    async fn publish_start_session_conflicts_without_production_session() {
        let (state, _) = make_test_state();
        let router = make_test_router(&state);

        let (status, body) = oneshot_json(router, "POST", "/api/publish/sessions", None).await;
        assert_eq!(status, StatusCode::CONFLICT, "无生产会话必须 409: {body}");
    }

    /// 试验会话晋级生产（窄场景）：fork 自生产 → 晋级 → 生产引用换防且
    /// 版本/哈希原样保留；非 fork 目标 400、自晋级 409、缺失 404、非管理员 403。
    #[tokio::test]
    async fn publish_promote_session_narrow_scenario() {
        let noop_core_eval = {
            let mut instr = std::collections::BTreeMap::new();
            instr.insert("type".to_string(), JsonValue::string("noop"));
            vec![JsonValue::Object(instr)]
        };
        let (state, _flag, ws_db) = make_test_state_full(noop_core_eval, SharedFactsLog::new());
        let router = make_test_router(&state);

        // bootstrap 管理员 + 登录
        let (status, body) = oneshot_json(
            router.clone(),
            "POST",
            "/api/platform/auth/bootstrap",
            Some(r#"{"username":"root","password":"admin-pass-123","display_name":""}"#),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let (status, body) = oneshot_json(
            router.clone(),
            "POST",
            "/api/platform/auth/login",
            Some(r#"{"username":"root","password":"admin-pass-123"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let admin_token = body["token"].as_str().unwrap().to_string();

        // 生产会话（初始引用换防前先建一个会话占位）
        let (status, body) = oneshot_json(router.clone(), "POST", "/api/sessions", None).await;
        assert_eq!(status, StatusCode::OK);
        let prod_sid = body["session_id"].as_u64().unwrap();
        ws_db
            .update_production_state(prod_sid as i64, 7, "hash-v7", "test")
            .unwrap();

        // 试验会话: fork 自生产
        let (status, body) = oneshot_json(
            router.clone(),
            "POST",
            &format!("/api/sessions/from/{prod_sid}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let trial_sid = body["session_id"].as_u64().unwrap();

        // 非 fork 会话（无父）→ 400 窄场景约束
        let (status, body) = oneshot_json(router.clone(), "POST", "/api/sessions", None).await;
        assert_eq!(status, StatusCode::OK);
        let stray_sid = body["session_id"].as_u64().unwrap();
        let (status, _) = oneshot_json_with_token(
            router.clone(),
            "POST",
            &format!("/api/publish/sessions/{stray_sid}/promote"),
            &admin_token,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "非 fork 目标必须 400");

        // 自晋级 → 409
        let (status, _) = oneshot_json_with_token(
            router.clone(),
            "POST",
            &format!("/api/publish/sessions/{prod_sid}/promote"),
            &admin_token,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);

        // 缺失会话 → 404
        let (status, _) = oneshot_json_with_token(
            router.clone(),
            "POST",
            "/api/publish/sessions/999999/promote",
            &admin_token,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // 非管理员 → 403
        let (status, body) = oneshot_json_with_token(
            router.clone(),
            "POST",
            "/api/platform/users",
            &admin_token,
            Some(
                r#"{"username":"carol","password":"carol-pass-123","display_name":"","email":"","department":"","role":"viewer"}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let (status, body) = oneshot_json(
            router.clone(),
            "POST",
            "/api/platform/auth/login",
            Some(r#"{"username":"carol","password":"carol-pass-123"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let carol_token = body["token"].as_str().unwrap().to_string();
        let (status, _) = oneshot_json_with_token(
            router.clone(),
            "POST",
            &format!("/api/publish/sessions/{trial_sid}/promote"),
            &carol_token,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // 管理员晋级 → 200, 生产引用换防且版本/哈希保留
        let (status, body) = oneshot_json_with_token(
            router.clone(),
            "POST",
            &format!("/api/publish/sessions/{trial_sid}/promote"),
            &admin_token,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["promoted"], true);
        assert_eq!(body["previous_production_session"], prod_sid);

        let prod = ws_db.get_production_state().unwrap();
        assert_eq!(prod.current_session_id, Some(trial_sid as i64));
        assert_eq!(prod.ruleset_version, 7, "版本必须原样保留");
        assert_eq!(prod.ruleset_hash.as_deref(), Some("hash-v7"));

        // 晋级后原生产会话再晋级已换防目标 → 409（已是生产会话）
        let (status, _) = oneshot_json_with_token(
            router.clone(),
            "POST",
            &format!("/api/publish/sessions/{trial_sid}/promote"),
            &admin_token,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
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

    // --- bundle 自有探针形态判定（探针形态改造版：IoSubscriber 跳过谓词） ---

    #[test]
    fn test_is_flow_probe_request_shape() {
        let probe_type = IoType::new("flow_probe");

        // 探针形态：io_type=flow_probe（bundle 自有类型，无内置 handler）→ 跳过自动应答
        let probe = serde_json::json!({ "args": { "msg": "flow-probe-no-svc" } });
        assert!(is_flow_probe_request(
            &probe_type,
            &serde_to_tcb(probe.clone())
        ));
        assert!(is_external_executor_request(
            &probe_type,
            &serde_to_tcb(probe)
        ));

        // 参数形态不限：外部应答契约只约定 request_id/result/error，params 由发射方自定
        let empty = serde_json::json!({});
        assert!(is_flow_probe_request(&probe_type, &serde_to_tcb(empty)));

        // 平台 io_type 不受该谓词影响（flow_probe 仅按 io_type 精确匹配）
        assert!(!is_flow_probe_request(
            &IoType::call_service(),
            &serde_to_tcb(serde_json::json!({ "args": {} }))
        ));
        assert!(!is_flow_probe_request(
            &IoType::call_external(),
            &serde_to_tcb(serde_json::json!({ "args": {} }))
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

    /// i18n 阶段2 续：裸 StatusCode 错误点经中间件注入 ApiResponse 同构 JSON 体
    /// （状态码不变 + code 透传），前端据此本地化。
    async fn test_error_code_passthrough_injects_json_body() {
        let (state, _) = make_test_state();

        let (status, json) = oneshot_json(
            make_test_router(&state),
            "DELETE",
            "/api/sessions/9999",
            None,
        )
        .await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(json["success"], serde_json::Value::Bool(false));
        assert_eq!(json["code"], "NOT_FOUND");
        assert!(json["fact_id"].is_null());
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

    /// wait=true 同步裁决: set 指令受理 → 同请求内 StateTransition 落链后应答
    /// accepted=true（与既有 send 即返形态并存, 无 wait 参数走 Ack 形态）。
    #[tokio::test]

    async fn test_session_command_wait_true_accepted() {
        // set 分派宪法（复刻生产 server_eval set 分派）: 指令受理落 StateTransition
        // （缺省 noop 宪法非合法 transform, 指令一律落无 cause 的 Error, wait 无从判定）
        let core_eval: Vec<JsonValue> = parse_transforms(
            r#"{"transform":[
                {"type":"branch","params":{"domain":{"type":"instruction","instruction_type":"set"},"on_true":[
                    {"type":"set","params":{"attr":"__exec__.instruction.params.attr","operation":"set","value":"__exec__.instruction.params.value"}}],"on_false":[]}}
            ]}"#,
        )
        .into_iter()
        .map(serde_to_tcb)
        .collect();
        let (state, _) = make_test_state_with_core_eval(core_eval);

        // 创建会话
        let (status, json) =
            oneshot_json(make_test_router(&state), "POST", "/api/sessions", None).await;
        assert_eq!(status, StatusCode::OK);
        let session_id = json["session_id"].as_u64().unwrap();

        // wait=true 提交 set 指令: 同请求等待 StateTransition 落链后应答
        let uri = format!("/api/sessions/{session_id}/command?wait=true");
        let body = r#"{"instruction":{"type":"set","params":{"attr":"result","operation":"set","value":"ok"}}}"#;
        let (status, json) = oneshot_json(make_test_router(&state), "POST", &uri, Some(body)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["success"], true);
        assert_eq!(json["accepted"], true);
        assert!(json["fact_id"].is_number());
        assert!(json["violation"].is_null());

        // 无 wait 参数仍走既有 Ack 形态（send 即返, 无 accepted 字段）
        let uri = format!("/api/sessions/{session_id}/command");
        let (status, json) = oneshot_json(make_test_router(&state), "POST", &uri, Some(body)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["success"], true);
        assert!(json.get("accepted").is_none() || json["accepted"].is_null());
    }

    /// wait=true 结论判据三路径（函数级）: StateTransition/IoRequest=受理,
    /// Violation=enforce 拦截（记录性事实不推进 version, 唯 cause 可判）,
    /// 空窗=超时降级（短超时实测走完等待窗口, 不依赖 test-util 时钟）。
    #[tokio::test]

    async fn test_wait_for_command_verdict_paths() {
        let empty_payload = serde_to_tcb(serde_json::json!({}));
        let empty_instruction = serde_to_tcb(serde_json::json!({"type":"noop"}));
        let no_wait = Duration::from_millis(10);

        // StateTransition{cause=42} → Accepted
        let log = FactsLog::new();
        log.append(Fact::StateTransition {
            id: FactId(2),
            cause: FactId(42),
            new_payload: empty_payload.clone(),
            new_queue: vec![],
        })
        .unwrap();
        assert!(matches!(
            wait_for_command_verdict(&log, 42, no_wait).await,
            CommandVerdict::Accepted
        ));

        // Violation{cause=42} → Rejected{rule_index, reason}
        let log = FactsLog::new();
        log.append(Fact::Violation {
            id: FactId(3),
            cause: FactId(42),
            rule_index: 7,
            reason: "out of sandbox".to_string(),
            instruction: empty_instruction.clone(),
        })
        .unwrap();
        match wait_for_command_verdict(&log, 42, no_wait).await {
            CommandVerdict::Rejected { rule_index, reason } => {
                assert_eq!(rule_index, 7);
                assert_eq!(reason, "out of sandbox");
            }
            other => panic!("expected Rejected, got {other:?}"),
        }

        // IoRequest{cause=42} → 受理（指令已进入执行）
        let log = FactsLog::new();
        log.append(Fact::IoRequest {
            id: FactId(4),
            cause: FactId(42),
            io_type: IoType::call_external(),
            params: empty_payload.clone(),
        })
        .unwrap();
        assert!(matches!(
            wait_for_command_verdict(&log, 42, no_wait).await,
            CommandVerdict::Accepted
        ));

        // 无结论事实 → Timeout（短超时实测走完等待窗口）
        let log = FactsLog::new();
        assert!(matches!(
            wait_for_command_verdict(&log, 42, no_wait).await,
            CommandVerdict::Timeout
        ));
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

    async fn test_shared_facts_rollup_empty_batch_400_oneshot() {
        let (state, _) = make_test_state();

        let (status, json) = oneshot_json(
            make_test_router(&state),
            "POST",
            "/api/shared/facts/rollup",
            Some(r#"{"fact_ids": []}"#),
        )
        .await;

        // R11（静默失败修复）：空批次显式 400，绝不静默 200
        assert_eq!(status, StatusCode::BAD_REQUEST);

        assert_eq!(json["success"], false);

        assert!(json["message"].as_str().unwrap().contains("不能为空"));
    }

    #[tokio::test]

    async fn test_shared_facts_rollup_unknown_id_404_partial_marking_oneshot() {
        let (state, _) = make_test_state();

        // 账本侧先落一条已知 fact（首条 fact_id=1）；请求混入未知 id 999
        // ——典型事故形态：误用 payload 侧 fact_id（两个 ID 空间不通用）
        state
            .shared_facts
            .append(
                "shared.test.sessions.s1.summary",
                JsonValue::string("s1"),
                1,
            )
            .unwrap();

        let (status, json) = oneshot_json(
            make_test_router(&state),
            "POST",
            "/api/shared/facts/rollup",
            Some(r#"{"fact_ids": [1, 999]}"#),
        )
        .await;

        // R11：未知 id 必须显式 404 并逐个列出（部分标记语义：已知 id 正常入账）
        assert_eq!(status, StatusCode::NOT_FOUND);

        assert_eq!(json["success"], false);

        let message = json["message"].as_str().unwrap();

        assert!(message.contains("999"), "应列出未知 id：{message}");

        assert!(message.contains("已标记 1"), "应报告已标记数：{message}");

        // 账本侧：已知 id 已入 rollup 标记，未知 id 不入（不给账本塞垃圾标记）
        assert!(state.shared_facts.is_rolled_up(&FactId(1)));

        assert!(!state.shared_facts.is_rolled_up(&FactId(999)));
    }

    #[tokio::test]

    async fn test_shared_facts_rollup_all_known_ids_200_oneshot() {
        let (state, _) = make_test_state();

        state
            .shared_facts
            .append(
                "shared.test.sessions.s1.summary",
                JsonValue::string("s1"),
                1,
            )
            .unwrap();

        state
            .shared_facts
            .append(
                "shared.test.sessions.s2.summary",
                JsonValue::string("s2"),
                1,
            )
            .unwrap();

        let (status, json) = oneshot_json(
            make_test_router(&state),
            "POST",
            "/api/shared/facts/rollup",
            Some(r#"{"fact_ids": [1, 2]}"#),
        )
        .await;

        assert_eq!(status, StatusCode::OK);

        assert_eq!(json["success"], true);

        assert_eq!(json["message"], "2 facts marked as rolled up");

        assert!(state.shared_facts.is_rolled_up(&FactId(1)));

        assert!(state.shared_facts.is_rolled_up(&FactId(2)));
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
    async fn test_prodreg_reap_once_keeps_production_session_alive() {
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
        db.update_production_state(prod_id as i64, 0, "", "test:prodreg")
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
    async fn test_prodreg_reap_once_recovers_phantom_production_reference() {
        let (state, _) = make_test_state();
        let api = state.sessions.clone();
        let db = api.workspace_db.clone().unwrap();
        let sessions = api.sessions.clone();

        // 构造幻影: 引用不存在的会话 999,版本 5/哈希 hash-abc
        db.update_production_state(999, 5, "hash-abc", "test:prodreg")
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
    async fn test_prodreg_close_production_session_rejected_409() {
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
        db.update_production_state(prod_id as i64, 0, "", "test:prodreg")
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

    #[test]
    fn test_ensure_command_ids_above_lifts_counter() {
        // 单反应器 WAL 续链计数器抬升语义：缺省起点锁定（改动须评审连带面）
        // → 抬升生效 → fetch_max 低于现值不回退
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let facts_log = FactsLog::new();
        let api = GovernanceApi::new(tx, facts_log.clone(), Auditor::new(facts_log));
        assert_eq!(api.next_id().0, 20000, "缺省起点锁定：改动须评审连带面");
        api.ensure_command_ids_above(25000);
        assert_eq!(api.next_id().0, 25001);
        api.ensure_command_ids_above(100);
        assert!(api.next_id().0 > 25001, "fetch_max 低于现值不回退");
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

    // append_platform_event 真实写入路径的 kind 契约:path 第二段必须携带 kind
    // (探活报警 plugin_offline/plugin_online 经此通道;kind 缺段时过滤恒空,报警失聪)
    #[tokio::test]
    async fn test_platform_events_kind_via_append_platform_event() {
        let (state, _) = make_test_state();
        let shared = SharedFactsLog::from_ref(&state);

        crate::api::platform_auth::append_platform_event(
            &shared,
            "plugin_offline",
            serde_json::json!({ "plugin_id": "finance-config", "base_url": "http://127.0.0.1:9110", "error": "connect refused" }),
        );
        crate::api::platform_auth::append_platform_event(
            &shared,
            "login_failed",
            serde_json::json!({ "username": "alice" }),
        );

        let (status, json) = oneshot_json(
            make_test_router(&state),
            "GET",
            "/api/audit/platform-events?kind=plugin_offline",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["total"], 1);
        let ev = &json["events"][0];
        assert_eq!(ev["kind"], "plugin_offline");
        assert_eq!(ev["detail"]["plugin_id"], "finance-config");
        assert!(ev["ts_ms"].is_u64());

        // 全量可见且 kind 正确解析(非数字串畸形降级)
        let (status, json) = oneshot_json(
            make_test_router(&state),
            "GET",
            "/api/audit/platform-events",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["total"], 2);
        let kinds: Vec<&str> = json["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["kind"].as_str().unwrap())
            .collect();
        assert!(kinds.contains(&"plugin_offline"));
        assert!(kinds.contains(&"login_failed"));
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
            // 测试不启用高危干预端点族
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
            // 测试不启用高危干预端点族
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
            // 测试不启用高危干预端点族
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
            // 测试不启用高危干预端点族
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

    /// 会话批量 payload 端点（跨仓挂账二）：一次 HTTP 多条写入；
    /// 受保护域整批 403（预校验，未写入任何条目）；空批次 400
    #[tokio::test]
    async fn test_batch_session_payload_oneshot() {
        let (state, _) = make_test_state();

        let router = make_authed_router(&state, "user_token", "service_token");

        // 创建会话（user token 即可）
        let (status, json) =
            oneshot_json_with_token(router.clone(), "POST", "/api/sessions", "user_token", None)
                .await;
        assert_eq!(status, StatusCode::OK);
        let session_id = json["session_id"].as_u64().unwrap();

        let uri = format!("/api/sessions/{session_id}/payloads");

        // 正常批量：两条非受保护域 → 200 全成功，逐条 fact_id
        let body = r#"{"updates":[
            {"path":"shared.default.stable.user.topic1","value":"a"},
            {"path":"shared.default.events.e1","value":"b"}]}"#;
        let (status, json) =
            oneshot_json_with_token(router.clone(), "POST", &uri, "user_token", Some(body)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["success"], true);
        assert_eq!(json["total"], 2);
        assert_eq!(json["succeeded"], 2);
        assert_eq!(json["failed"], 0);
        assert!(json["results"][0]["fact_id"].is_u64());
        assert!(json["results"][1]["fact_id"].is_u64());

        // 混入一条受保护域 + user token → 整批 403（含已通过校验的条目也未写入）
        let body403 = r#"{"updates":[
            {"path":"shared.default.stable.user.ok2","value":"a"},
            {"path":"shared.default.stable.llm.gpt-4o.summary","value":"forged"}]}"#;
        let (status, json) =
            oneshot_json_with_token(router.clone(), "POST", &uri, "user_token", Some(body403))
                .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(json["success"], false);
        assert_eq!(json["failed"], 2);
        assert_eq!(json["succeeded"], 0);

        // service token 同批 → 放行全成功
        let (status, json) =
            oneshot_json_with_token(router.clone(), "POST", &uri, "service_token", Some(body403))
                .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["success"], true);
        assert_eq!(json["succeeded"], 2);

        // 空批次 → 400
        let (status, json) = oneshot_json_with_token(
            router,
            "POST",
            &uri,
            "user_token",
            Some(r#"{"updates":[]}"#),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["success"], false);
    }

    // --- 应用级凭据（app key 通道端到端） ---

    /// 在已启用认证的 router 上完成 bootstrap → login → 签发 app 凭据，
    /// 返回 (router, app_key, platform_token)。
    async fn issue_app_key_via_router(router: Router, app_id: &str) -> (Router, String, String) {
        // bootstrap（公开路由）
        let (status, _) = oneshot_json_with_token(
            router.clone(),
            "POST",
            "/api/platform/auth/bootstrap",
            "",
            Some(r#"{"username":"root","password":"admin-pass-123"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        // login（公开路由）→ 平台会话 token
        let (status, json) = oneshot_json_with_token(
            router.clone(),
            "POST",
            "/api/platform/auth/login",
            "",
            Some(r#"{"username":"root","password":"admin-pass-123"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let platform_token = json["token"].as_str().unwrap().to_string();
        // 签发 app 凭据（handler 自校验 manage_apps）
        let body = format!(r#"{{"app_id":"{app_id}","description":"e2e"}}"#);
        let (status, json) = oneshot_json_with_token(
            router.clone(),
            "POST",
            "/api/platform/apps",
            &platform_token,
            Some(&body),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let key = json["key"].as_str().unwrap().to_string();
        (router, key, platform_token)
    }

    /// app key 通道端到端：认证通过 → 受保护域 403（App 非 service）→
    /// 审批代理 403（人类在场）→ 归因事件经 /api/shared/facts 可查 →
    /// 吊销后 401 即时生效。
    #[tokio::test]
    async fn test_app_key_channel_end_to_end_oneshot() {
        let (state, _) = make_test_state();

        let router = make_authed_router(&state, "user_token", "service_token");
        let (router, app_key, platform_token) = issue_app_key_via_router(router, "evo-agent").await;

        // 1. app key 认证通过（读受保护路由 /api/state）
        let (status, _) =
            oneshot_json_with_token(router.clone(), "GET", "/api/state", &app_key, None).await;
        assert_eq!(status, StatusCode::OK);

        // 2. app key 写受保护域 → 403（App 身份非受信服务管道）
        let body = r#"{"path":"shared.default.stable.llm.gpt-4o.summary","value":"forged"}"#;
        let (status, json) =
            oneshot_json_with_token(router.clone(), "POST", "/api/payload", &app_key, Some(body))
                .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(json["success"], false);

        // 3. app key 审批代理 → 403（审批 = 人类在场）
        let (status, _) = oneshot_json_with_token(
            router.clone(),
            "POST",
            "/api/plugins/finance-config/admin/proposals/p1/approve",
            &app_key,
            Some(r#"{}"#),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // 4. 归因事件入链（经静态 token 查 /api/shared/facts）
        let (status, json) = oneshot_json_with_token(
            router.clone(),
            "GET",
            "/api/shared/facts?prefix=platform.event.app_invoke",
            "user_token",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let raw = serde_json::to_string(&json).unwrap();
        assert!(raw.contains("app_invoke"), "归因事件应可查: {raw}");
        assert!(raw.contains("evo-agent"), "归因事件应含 app_id: {raw}");

        // 5. 管理面：列表（含哈希）→ 吊销 → 即时 401
        let (status, json) = oneshot_json_with_token(
            router.clone(),
            "GET",
            "/api/platform/apps",
            &platform_token,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let apps = json["apps"].as_array().unwrap();
        assert_eq!(apps.len(), 1);
        assert!(apps[0]["key_hash"].as_str().unwrap().starts_with("blake3:"));
        assert!(
            !serde_json::to_string(&json).unwrap().contains(&app_key),
            "列表不得泄露明文 key"
        );

        let (status, _) = oneshot_json_with_token(
            router.clone(),
            "POST",
            "/api/platform/apps/evo-agent/revoke",
            &platform_token,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) =
            oneshot_json_with_token(router, "GET", "/api/state", &app_key, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "吊销须即时生效");
    }

    /// 认证禁用且未配置 service token（守卫未武装）时不注入身份 →
    /// 受保护域按放行处理（零配置开发模式语义不变）
    #[tokio::test]
    async fn test_b5_dev_mode_allows_protected_domain_oneshot() {
        let (state, _) = make_test_state();

        let body = r#"{"path":"shared.default.stable.llm.gpt-4o.summary","value":"dev"}"#;

        let (status, json) =
            oneshot_json(make_test_router(&state), "POST", "/api/payload", Some(body)).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["success"], true);
    }

    /// 构造"认证禁用 + service token 已配置"的测试 Router（守卫武装的
    /// 显式豁免形态：受保护域写入须凭据，其余端点匿名可达）
    fn make_armed_insecure_router(state: &AppState, service_token: &str) -> Router {
        GovernanceServer::new(
            state.clone(),
            AuthConfig::disabled().with_service_tokens(vec![service_token.to_string()]),
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
            // 测试不启用高危干预端点族
            false,
            None,
        )
        .build_router()
    }

    /// 守卫武装 + 无凭据 → 受保护域写入 403（守卫与认证开关解耦：
    /// 显式豁免不再敞开受保护域）
    #[tokio::test]
    async fn test_b5_armed_insecure_rejects_unauthenticated_protected_write_oneshot() {
        let (state, _) = make_test_state();

        let router = make_armed_insecure_router(&state, "svc-secret");

        let body = r#"{"path":"shared.default.stable.llm.gpt-4o.summary","value":"forged"}"#;

        let (status, json) = oneshot_json(router, "POST", "/api/payload", Some(body)).await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(json["success"], false);
        assert_eq!(json["code"], "UNAUTHORIZED");
    }

    /// 守卫武装 + 错误凭据 → 受保护域写入 403（无效 token 不得借道）
    #[tokio::test]
    async fn test_b5_armed_insecure_rejects_wrong_token_protected_write_oneshot() {
        let (state, _) = make_test_state();

        let router = make_armed_insecure_router(&state, "svc-secret");

        let body = r#"{"path":"shared.default.stable.llm.gpt-4o.summary","value":"forged"}"#;

        let (status, _) = oneshot_json_with_token(
            router,
            "POST",
            "/api/payload",
            "not-the-service-token",
            Some(body),
        )
        .await;

        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    /// 守卫武装 + 有效 service token → 受保护域写入放行
    #[tokio::test]
    async fn test_b5_armed_insecure_allows_service_token_protected_write_oneshot() {
        let (state, _) = make_test_state();

        let router = make_armed_insecure_router(&state, "svc-secret");

        let body = r#"{"path":"shared.default.stable.llm.gpt-4o.summary","value":"extracted"}"#;

        let (status, json) =
            oneshot_json_with_token(router, "POST", "/api/payload", "svc-secret", Some(body)).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["success"], true);
    }

    /// 守卫武装 + 无凭据 → 非受保护 shared 路径仍放行（显式豁免语义
    /// 只收窄受保护域，不波及普通写入面）
    #[tokio::test]
    async fn test_b5_armed_insecure_allows_non_protected_write_without_token_oneshot() {
        let (state, _) = make_test_state();

        let router = make_armed_insecure_router(&state, "svc-secret");

        let body = r#"{"path":"shared.default.summary.latest","value":"open"}"#;

        let (status, json) = oneshot_json(router, "POST", "/api/payload", Some(body)).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["success"], true);
    }

    /// 守卫武装 + 无凭据 → 批量端点含受保护域条目时整批 403（与单条
    /// 端点同源判定）
    #[tokio::test]
    async fn test_b5_armed_insecure_batch_rejects_protected_write_oneshot() {
        let (state, _) = make_test_state();

        let router = make_armed_insecure_router(&state, "svc-secret");

        let body = r#"{"updates":[
            {"path":"shared.default.summary.latest","value":"open"},
            {"path":"shared.default.stable.llm.gpt-4o.summary","value":"forged"}
        ]}"#;

        let (status, json) =
            oneshot_json(router, "POST", "/api/sessions/1/payloads", Some(body)).await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(json["success"], false);
        assert_eq!(json["succeeded"], 0, "整批拒绝：未写入任何条目");
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
            evorule_governance_host::session::DEFAULT_IO_WARN_TIMEOUT.as_secs(),
            evorule_governance_host::session::DEFAULT_IO_ERROR_TIMEOUT.as_secs(),
            0,    // 0=引擎缺省 TTL
            None, // 锚点链：测试构造不启用
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
                // 知识资产化 A 批：规则条目四治理字段不携带（None）
                knowledge_kind: None,
                trust_level: None,
                license_ref: None,
                execution_contract: None,
            }],
            data_dependencies: None,
            recipe_snapshot: None,
            tests: BundleTests {
                // B2: pass 必带 sandbox: 机器背书引用(篡改用例在哈希层先拒,
                // 证据闸在防篡改之后,此引用不需真实沙盒)
                subset: vec!["sandbox:itest".into()],
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
                // 知识资产化 A 批：知识条目治理字段（本 fixture 不携带，None=旧格式字节兼容）
                knowledge_kind: None,
                trust_level: None,
                license_ref: None,
                execution_contract: None,
            }],
            data_dependencies: None,
            recipe_snapshot: None,
            tests: BundleTests {
                // B2: pass 必带 sandbox: 机器背书引用(执行域深度校验:存在性+
                // closed+报告 failed==0)——正路径调用方测试需 seed 真实沙盒;
                // 拒点在证据闸之前的负路径测试复用本夹具不受影响
                subset: vec!["sandbox:1".into()],
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
        // 夹具包引用 sandbox:1 → in-memory 元数据库 seed 真实沙盒(深度校验放行面)
        let ws_db = Arc::new(evorule_workspace::WorkspaceDb::in_memory().unwrap());
        let seed_sid = crate::api::evidence_fixture::seed_pass_sandbox(&ws_db);
        assert_eq!(seed_sid, 1, "空库首个 seed 应得 sandbox:1(夹具引用约定)");
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
            evorule_governance_host::session::DEFAULT_IO_WARN_TIMEOUT.as_secs(),
            evorule_governance_host::session::DEFAULT_IO_ERROR_TIMEOUT.as_secs(),
            0,    // 0=引擎缺省 TTL
            None, // 锚点链：测试构造不启用
        )
        .with_workspace_db(ws_db.clone());
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
            evorule_governance_host::session::DEFAULT_IO_WARN_TIMEOUT.as_secs(),
            evorule_governance_host::session::DEFAULT_IO_ERROR_TIMEOUT.as_secs(),
            0,    // 0=引擎缺省 TTL
            None, // 锚点链：测试构造不启用
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
            // 知识资产化 A 批：规则条目四治理字段不携带（None）
            knowledge_kind: None,
            trust_level: None,
            license_ref: None,
            execution_contract: None,
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
    async fn test_importreg_import_rejects_pass_without_traceable_subset() {
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
            evorule_governance_host::session::DEFAULT_IO_WARN_TIMEOUT.as_secs(),
            evorule_governance_host::session::DEFAULT_IO_ERROR_TIMEOUT.as_secs(),
            0,    // 0=引擎缺省 TTL
            None, // 锚点链：测试构造不启用
        );

        let mut bundle = q12_knowledge_bundle(
            "bundle-importreg-shape",
            "ds-importreg-shape",
            "scn-importreg",
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
    async fn test_importreg_import_rejects_phantom_sandbox_reference() {
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
            evorule_governance_host::session::DEFAULT_IO_WARN_TIMEOUT.as_secs(),
            evorule_governance_host::session::DEFAULT_IO_ERROR_TIMEOUT.as_secs(),
            0,    // 0=引擎缺省 TTL
            None, // 锚点链：测试构造不启用
        )
        .with_workspace_db(Arc::new(
            evorule_workspace::WorkspaceDb::in_memory().unwrap(),
        ));

        let mut bundle = q12_knowledge_bundle(
            "bundle-importreg-ref",
            "ds-importreg-ref",
            "scn-importreg-ref",
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

    /// B2-负路径: human:<actor> 人工背书形态 → 拒收(人工降级通路已下线——
    /// 自由文本 actor 可被任意伪造,构成无沙盒验证的 pass 证据面)。
    #[tokio::test]
    async fn test_importreg_import_rejects_human_endorsement() {
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
            evorule_governance_host::session::DEFAULT_IO_WARN_TIMEOUT.as_secs(),
            evorule_governance_host::session::DEFAULT_IO_ERROR_TIMEOUT.as_secs(),
            0,    // 0=引擎缺省 TTL
            None, // 锚点链：测试构造不启用
        );

        // 夹具包 subset 改写为 human 背书形态(拒点在证据闸前缀检查,无需 seed)
        let mut bundle = q12_knowledge_bundle(
            "bundle-importreg-human",
            "ds-importreg-human",
            "scn-importreg-human",
            "https://rpsm.evorule.org/schemas/scenario/v1.0.json",
        );
        bundle.tests.subset = vec!["human:q12-itest".into()];
        bundle.audit.content_hash = bundle.compute_content_hash();
        let err = sessions
            .import_bundle(&bundle, false)
            .await
            .expect_err("human 人工背书形态已下线,必须拒收");
        assert!(
            err.contains("可追溯标记"),
            "错误应指向证据闸(sandbox:-only): {err}"
        );
        assert!(
            !tmp.path().join("knowledge").join("bundles").exists(),
            "拒绝的 bundle 不得落盘"
        );
    }

    /// B2-真实沙盒引用: closed 沙盒 + PASS 报告(failed=0)→ 放行;
    /// FAIL 报告(failed>0)→ 拒收(fail 报告不得作 pass 证据)。
    /// 在 in-memory db 造真实沙盒记录 + 磁盘报告文件(与 close_sandbox 落盘
    /// 同构:report_<facts basename>.json 于 SANDBOX_REPORT_DIR)。
    #[tokio::test]
    async fn test_importreg_import_sandbox_reference_with_report_consistency() {
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
            evorule_governance_host::session::DEFAULT_IO_WARN_TIMEOUT.as_secs(),
            evorule_governance_host::session::DEFAULT_IO_ERROR_TIMEOUT.as_secs(),
            0,    // 0=引擎缺省 TTL
            None, // 锚点链：测试构造不启用
        )
        .with_workspace_db(ws_db.clone());

        // 先建 workspace 行(沙盒记录外键依赖) + 两个 closed 沙盒记录: PASS / FAIL
        // DateTime<Utc> 从 production_state 行取(chrono 非 evorule-server 直接依赖,
        // 不为此新增依赖)
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let ws_id = format!("ws-importreg-{ts}");
        {
            let now_dt = ws_db.get_production_state().unwrap().updated_at;
            let ws = evorule_workspace::models::WorkspaceRecord {
                id: ws_id.clone(),
                name: "ws-importreg".into(),
                owner_id: "test:importreg".into(),
                created_at: now_dt,
                updated_at: now_dt,
                archived_at: None,
                state: evorule_workspace::models::WorkspaceState::Active,
                description: None,
            };
            ws_db.insert_workspace(&ws).unwrap();
        }
        let report_dir = evorule_workspace::sandbox_report_dir()
            .to_string_lossy()
            .to_string();
        std::fs::create_dir_all(&report_dir).unwrap();
        let facts_1 = format!("audit_sandbox_1_{ts}.json");
        let facts_2 = format!("audit_sandbox_2_{ts}.json");
        // PASS 沙盒的 draft 快照 = bundle 条目的 rule_body 原文(内容对照的
        // 数据面:快照含条目内容才放行)
        let sb_pass_draft = serde_json::json!([{
            "scenario_id": "spring-single-particle",
            "gravity": [0.0, -9.81, 0.0],
            "restitution": 1.0,
            "bodies": [{"id": "particle-1"}]
        }])
        .to_string();
        let sb_pass = ws_db
            .insert_sandbox_session(
                None,
                &ws_id,
                1,
                Some("hash-importreg"),
                Some(&sb_pass_draft),
                1,
                "test:importreg",
            )
            .unwrap();
        let sb_fail = ws_db
            .insert_sandbox_session(
                None,
                &ws_id,
                1,
                Some("hash-importreg"),
                Some(r#"["patch-1"]"#),
                1,
                "test:importreg",
            )
            .unwrap();
        // insert 自增从 1 起;以实际返回 id 为准写报告与引用
        let export_1 = format!("{report_dir}/{facts_1}");
        let export_2 = format!("{report_dir}/{facts_2}");
        ws_db.close_sandbox_session(sb_pass, &export_1).unwrap();
        ws_db.close_sandbox_session(sb_fail, &export_2).unwrap();
        // 报告文件(与 generate_test_report 落盘同构:report_<facts basename>)
        std::fs::write(
            format!("{report_dir}/report_{facts_1}"),
            r#"{"draft_ruleset_hash": "hash-importreg", "summary": {"total_cases": 9, "passed": 9, "failed": 0, "skipped": 0}}"#,
        )
        .unwrap();
        std::fs::write(
            format!("{report_dir}/report_{facts_2}"),
            r#"{"summary": {"total_cases": 9, "passed": 7, "failed": 2, "skipped": 0}}"#,
        )
        .unwrap();

        // PASS 沙盒引用 → 放行
        let mut bundle = q12_knowledge_bundle(
            "bundle-importreg-sb-pass",
            "ds-importreg-sb-pass",
            "scn-importreg-sb-pass",
            "https://rpsm.evorule.org/schemas/scenario/v1.0.json",
        );
        bundle.tests.subset = vec![format!("sandbox:{sb_pass}")];
        bundle.audit.content_hash = bundle.compute_content_hash();
        let result = sessions
            .import_bundle(&bundle, false)
            .await
            .expect("closed 沙盒 + PASS 报告引用应放行");
        assert_eq!(result.dataset_id, "ds-importreg-sb-pass");
        assert_eq!(result.entry_count, 1);

        // FAIL 沙盒引用 → 拒收(报告一致性)
        let mut bundle = q12_knowledge_bundle(
            "bundle-importreg-sb-fail",
            "ds-importreg-sb-fail",
            "scn-importreg-sb-fail",
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
            evorule_governance_host::session::DEFAULT_IO_WARN_TIMEOUT.as_secs(),
            evorule_governance_host::session::DEFAULT_IO_ERROR_TIMEOUT.as_secs(),
            0,    // 0=引擎缺省 TTL
            None, // 锚点链：测试构造不启用
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
                r#"{"name":"rule-e2e","content":"{\"transform\":[{\"type\":\"set\",\"params\":{\"attr\":\"result\",\"operation\":\"set\",\"value\":\"ok\"}}]}","created_by":"head-1","description":null}"#,
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

    // ====================================================================
    // tier 层级门禁单测（正向 / 反向 / 缺声明 / 子目录不算 L2）
    // ====================================================================

    /// 构造临时规则文件并返回 (文件路径, 解析后 JSON)
    fn tier_gate_fixture(
        dir: &std::path::Path,
        rel: &str,
        body: &str,
    ) -> (std::path::PathBuf, serde_json::Value) {
        let f = dir.join(rel);
        if let Some(parent) = f.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&f, body).unwrap();
        let json = serde_json::from_str::<serde_json::Value>(body).unwrap();
        (f, json)
    }

    #[test]
    fn test_tier_gate_root_meta_with_decl_passes() {
        // 正向：根目录 00_meta_ 文件带 tier=meta → 放行（旧形态兼容回归）
        let dir = std::env::temp_dir().join("reg145_tier_gate_ok");
        std::fs::create_dir_all(&dir).unwrap();
        let (f, json) = tier_gate_fixture(
            &dir,
            "00_meta_x.json",
            r#"{"kind":"rule_set","metadata":{"tier":"meta"},"transform":[{"type":"set","params":{"attr":"x","operation":"set","value":1}}]}"#,
        );
        assert!(SessionApi::rule_file_gate(&json, &f, &dir).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_tier_gate_constraint_new_authority_passes() {
        // 正向（新权威形态，命名收敛 v3.0）：根目录 00_constraint_ 文件带 tier=constraint → 放行
        let dir = std::env::temp_dir().join("reg145_tier_gate_constraint_ok");
        std::fs::create_dir_all(&dir).unwrap();
        let (f, json) = tier_gate_fixture(
            &dir,
            "00_constraint_x.json",
            r#"{"kind":"rule_set","metadata":{"tier":"constraint"},"transform":[{"type":"set","params":{"attr":"x","operation":"set","value":1}}]}"#,
        );
        assert!(SessionApi::rule_file_gate(&json, &f, &dir).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_tier_gate_prefix_tier_cross_combo_passes() {
        // 交叉组合（前缀与 tier 声明正交双值）：新前缀+旧 tier、旧前缀+新 tier 均放行
        let dir = std::env::temp_dir().join("reg145_tier_gate_cross");
        std::fs::create_dir_all(&dir).unwrap();
        for (name, tier) in [
            ("00_constraint_cross1.json", "meta"),
            ("00_meta_cross2.json", "constraint"),
        ] {
            let (f, json) = tier_gate_fixture(
                &dir,
                name,
                &format!(
                    r#"{{"kind":"rule_set","metadata":{{"tier":"{tier}"}},"transform":[{{"type":"set","params":{{"attr":"x","operation":"set","value":1}}}}]}}"#
                ),
            );
            assert!(
                SessionApi::rule_file_gate(&json, &f, &dir).is_ok(),
                "{name} + tier={tier} 应放行"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_tier_gate_constraint_prefix_business_rejected() {
        // 反向（新前缀）：非 L2 文件携带 tier=constraint 同样按层级伪造拒载
        let dir = std::env::temp_dir().join("reg145_tier_gate_constraint_subdir");
        std::fs::create_dir_all(&dir).unwrap();
        let (f, json) = tier_gate_fixture(
            &dir,
            "sub/00_constraint_y.json",
            r#"{"kind":"rule_set","metadata":{"tier":"constraint"},"transform":[{"type":"set","params":{"attr":"x","operation":"set","value":1}}]}"#,
        );
        assert!(!SessionApi::rule_file_gate(&json, &f, &dir).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_tier_gate_root_meta_without_decl_rejected() {
        // 缺声明：根目录 00_meta_ 文件无 tier → 拒载（防裸文件冒充元规则）
        let dir = std::env::temp_dir().join("reg145_tier_gate_no_decl");
        std::fs::create_dir_all(&dir).unwrap();
        let (f, json) = tier_gate_fixture(
            &dir,
            "00_meta_x.json",
            r#"{"kind":"rule_set","metadata":{"title":"x"},"transform":[{"type":"set","params":{"attr":"x","operation":"set","value":1}}]}"#,
        );
        assert!(!SessionApi::rule_file_gate(&json, &f, &dir).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_tier_gate_l3_with_meta_decl_rejected() {
        // 反向：bundles/ 子目录条目带 tier=meta → 拒载（防 LLM 补丁伪造层级）
        let dir = std::env::temp_dir().join("reg145_tier_gate_forged");
        std::fs::create_dir_all(&dir).unwrap();
        let (f, json) = tier_gate_fixture(
            &dir,
            "bundles/b-001/entry_a.json",
            r#"{"kind":"rule_set","metadata":{"tier":"meta"},"transform":[{"type":"set","params":{"attr":"x","operation":"set","value":1}}]}"#,
        );
        assert!(!SessionApi::rule_file_gate(&json, &f, &dir).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_tier_gate_subdir_meta_prefix_not_l2() {
        // 子目录内 00_meta_ 前缀不算 L2：带 tier=meta 同样按伪造拒载
        let dir = std::env::temp_dir().join("reg145_tier_gate_subdir");
        std::fs::create_dir_all(&dir).unwrap();
        let (f, json) = tier_gate_fixture(
            &dir,
            "sub/00_meta_y.json",
            r#"{"kind":"rule_set","metadata":{"tier":"meta"},"transform":[{"type":"set","params":{"attr":"x","operation":"set","value":1}}]}"#,
        );
        assert!(!SessionApi::rule_file_gate(&json, &f, &dir).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_tier_gate_root_business_file_untouched() {
        // 存量业务文件（根目录无前缀、无 tier 声明）→ 放行（兼容零影响）
        let dir = std::env::temp_dir().join("reg145_tier_gate_biz");
        std::fs::create_dir_all(&dir).unwrap();
        let (f, json) = tier_gate_fixture(
            &dir,
            "biz_rules.json",
            r#"{"kind":"rule_set","metadata":{"title":"biz"},"transform":[{"type":"set","params":{"attr":"x","operation":"set","value":1}}]}"#,
        );
        assert!(SessionApi::rule_file_gate(&json, &f, &dir).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_tier_gate_enforce_in_l2_passes() {
        // 回归验证 正向：L2 元规则文件携带 enforce → 放行（强制原语唯一合法落点）
        let dir = std::env::temp_dir().join("reg147_enforce_l2_ok");
        std::fs::create_dir_all(&dir).unwrap();
        let (f, json) = tier_gate_fixture(
            &dir,
            "00_meta_guard.json",
            r#"{"kind":"rule_set","metadata":{"tier":"meta"},"transform":[{"type":"enforce","params":{"domain":{"type":"exists","path":"payload.x"},"reason":"违规拦截"}}]}"#,
        );
        assert!(SessionApi::rule_file_gate(&json, &f, &dir).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_tier_gate_enforce_in_business_rejected() {
        // 回归验证 反向：业务文件携带 enforce → 拒载（防 LLM 补丁私自获得引擎级阻止权）
        let dir = std::env::temp_dir().join("reg147_enforce_biz_rej");
        std::fs::create_dir_all(&dir).unwrap();
        let (f, json) = tier_gate_fixture(
            &dir,
            "biz_rules.json",
            r#"{"kind":"rule_set","metadata":{"title":"biz"},"transform":[{"type":"enforce","params":{"domain":{"type":"exists","path":"payload.x"},"reason":"越权"}}]}"#,
        );
        assert!(!SessionApi::rule_file_gate(&json, &f, &dir).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_tier_gate_enforce_in_subdir_meta_rejected() {
        // 回归验证 反向：子目录 00_meta_ 前缀不算 L2，携带 enforce 同样拒载
        let dir = std::env::temp_dir().join("reg147_enforce_subdir_rej");
        std::fs::create_dir_all(&dir).unwrap();
        let (f, json) = tier_gate_fixture(
            &dir,
            "sub/00_meta_y.json",
            r#"{"kind":"rule_set","metadata":{"tier":"meta"},"transform":[{"type":"enforce","params":{"domain":{"type":"exists","path":"payload.x"},"reason":"越权"}}]}"#,
        );
        assert!(!SessionApi::rule_file_gate(&json, &f, &dir).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    // ===== L2 约束清单投影（l2_inventory）单元测试 =====

    #[test]
    fn test_l2_inventory_projects_root_meta_sorted() {
        // 根目录 00_meta_ 且过层级门禁 → 投影 title/guard_for；按文件名排序确定
        let dir = std::env::temp_dir().join("l2inv_projects_sorted");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        tier_gate_fixture(
            &dir,
            "00_meta_b_second.json",
            r#"{"kind":"rule_set","metadata":{"tier":"meta","title":"守卫乙","guard_for":["robot_move"]},"transform":[{"type":"set","params":{"attr":"x","operation":"set","value":1}}]}"#,
        );
        tier_gate_fixture(
            &dir,
            "00_meta_a_first.json",
            r#"{"kind":"rule_set","metadata":{"tier":"meta","title":"守卫甲","guard_for":["validate_precision","robot_move"]},"transform":[{"type":"set","params":{"attr":"x","operation":"set","value":1}}]}"#,
        );
        let inv = SessionApi::l2_inventory(&dir);
        assert_eq!(inv.count, 2, "两个过门禁的 L2 文件都应投影");
        // 排序确定：文件名字典序
        assert_eq!(inv.files[0].path, "00_meta_a_first.json");
        assert_eq!(inv.files[0].title, "守卫甲");
        assert_eq!(
            inv.files[0].guard_for,
            vec!["validate_precision".to_string(), "robot_move".to_string()]
        );
        assert_eq!(inv.files[1].path, "00_meta_b_second.json");
        assert_eq!(inv.files[1].title, "守卫乙");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_l2_inventory_projects_constraint_prefix() {
        // 新权威前缀（命名收敛 v3.0）：根目录 00_constraint_ 过门禁 → 同口径投影；
        // 晋升条目（metadata 带 promoted_*）→ 晋升账三字段随投影透出
        let dir = std::env::temp_dir().join("l2inv_projects_constraint");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        tier_gate_fixture(
            &dir,
            "00_constraint_new.json",
            r#"{"kind":"rule_set","metadata":{"tier":"constraint","title":"约束守卫","guard_for":["robot_move"]},"transform":[{"type":"set","params":{"attr":"x","operation":"set","value":1}}]}"#,
        );
        tier_gate_fixture(
            &dir,
            "00_constraint_promoted_abc.json",
            r#"{"kind":"rule_set","metadata":{"tier":"constraint","title":"晋升约束",
                "promoted_from":"rule_version:01M3BDCC39Y5FQVGYHSE034R36",
                "promoted_at":"2026-09-25T04:33:05Z","promoted_by":"console"},"transform":[{"type":"set","params":{"attr":"x","operation":"set","value":1}}]}"#,
        );
        let inv = SessionApi::l2_inventory(&dir);
        assert_eq!(inv.count, 2, "新前缀 L2 文件应投影");
        assert_eq!(inv.files[0].path, "00_constraint_new.json");
        assert_eq!(inv.files[0].title, "约束守卫");
        // 非晋升条目：晋升账三字段为 null
        assert_eq!(inv.files[0].promoted_from, None);
        let promoted = &inv.files[1];
        assert_eq!(promoted.path, "00_constraint_promoted_abc.json");
        assert_eq!(
            promoted.promoted_from.as_deref(),
            Some("rule_version:01M3BDCC39Y5FQVGYHSE034R36")
        );
        assert_eq!(
            promoted.promoted_at.as_deref(),
            Some("2026-09-25T04:33:05Z")
        );
        assert_eq!(promoted.promoted_by.as_deref(), Some("console"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_l2_inventory_excludes_gate_failures_and_subdir() {
        // 拒载文件不投影（缺 tier 声明）；子目录 00_meta_ 不算 L2；普通业务文件不投影
        let dir = std::env::temp_dir().join("l2inv_excludes");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 缺 tier=meta 声明 → 层级门禁拒载 → 不投影
        tier_gate_fixture(
            &dir,
            "00_meta_no_decl.json",
            r#"{"kind":"rule_set","metadata":{"title":"裸文件"},"transform":[{"type":"set","params":{"attr":"x","operation":"set","value":1}}]}"#,
        );
        // 子目录内 00_meta_ 前缀（带 tier=meta）→ 层级伪造拒载 → 不投影
        tier_gate_fixture(
            &dir,
            "sub/00_meta_forged.json",
            r#"{"kind":"rule_set","metadata":{"tier":"meta","title":"伪造"},"transform":[{"type":"set","params":{"attr":"x","operation":"set","value":1}}]}"#,
        );
        // 根目录普通业务文件 → 非 00_meta_ 前缀 → 不投影
        tier_gate_fixture(
            &dir,
            "biz_rules.json",
            r#"{"kind":"rule_set","metadata":{"tier":"meta","title":"业务文件不应出现在 L2 清单"},"transform":[{"type":"set","params":{"attr":"x","operation":"set","value":1}}]}"#,
        );
        // 唯一合法 L2
        tier_gate_fixture(
            &dir,
            "00_meta_ok.json",
            r#"{"kind":"rule_set","metadata":{"tier":"meta","title":"合法守卫"},"transform":[{"type":"set","params":{"attr":"x","operation":"set","value":1}}]}"#,
        );
        let inv = SessionApi::l2_inventory(&dir);
        assert_eq!(inv.count, 1, "仅根目录过门禁的 00_meta_ 文件投影");
        assert_eq!(inv.files[0].path, "00_meta_ok.json");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_inventory_excludes_files_rejected_by_full_load_gate() {
        // 清单视图与实载一致性：装载路径按四步门（层级/schema/纪律/domain）拒载的
        // 文件，两张清单同样不得投影——防「清单里在、加载时不载」的判定分叉
        // （清单门与装载门必须同一通道）。
        let dir = std::env::temp_dir().join("inventory_full_gate_consistency");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 合法 L2：全门通过 → 双清单投影
        tier_gate_fixture(
            &dir,
            "00_meta_ok.json",
            r#"{"kind":"rule_set","metadata":{"tier":"meta","title":"合法守卫"},"transform":[{"type":"set","params":{"attr":"x","operation":"set","value":1}}]}"#,
        );
        // 层级过但 domain 门挂：eq 域缺 on_missing 显式声明 → 装载路径拒载
        tier_gate_fixture(
            &dir,
            "00_meta_domain_bad.json",
            r#"{"kind":"rule_set","metadata":{"tier":"meta","title":"domain 挂门"},"transform":[{"type":"branch","params":{"domain":{"type":"eq","path":"payload.s","value":"x"},"on_true":[],"on_false":[]}}]}"#,
        );
        let inv = SessionApi::l2_inventory(&dir);
        assert_eq!(
            inv.count,
            1,
            "挂 domain 门的文件装载期必拒，L2 清单不得投影: {:?}",
            inv.files.iter().map(|f| &f.path).collect::<Vec<_>>()
        );
        assert_eq!(inv.files[0].path, "00_meta_ok.json");
        let tiers = SessionApi::tier_inventory(&dir.join("server_eval_absent.json"), &dir);
        let l2 = tiers
            .iter()
            .find(|t| t.tier == "L2_meta")
            .expect("L2_meta 层条目应存在");
        assert_eq!(
            l2.paths,
            vec!["00_meta_ok.json".to_string()],
            "三层清单 L2 路径集须与实载一致（挂 domain 门的文件排除）: {:?}",
            l2.paths
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_l2_inventory_field_defaults_and_fail_soft() {
        // 字段缺省容错：title 缺省空串、guard_for 缺省空数组；坏 JSON → fail-soft 跳过
        let dir = std::env::temp_dir().join("l2inv_defaults_failsoft");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        tier_gate_fixture(
            &dir,
            "00_meta_minimal.json",
            r#"{"kind":"rule_set","metadata":{"tier":"meta"},"transform":[{"type":"set","params":{"attr":"x","operation":"set","value":1}}]}"#,
        );
        // 坏 JSON → 读取/解析失败 → 跳过（不影响其他文件）；直写绕过 fixture（其内部解析会 panic）
        std::fs::write(
            dir.join("00_meta_broken.json"),
            r#"{"kind":"rule_set", NOT VALID"#,
        )
        .unwrap();
        let inv = SessionApi::l2_inventory(&dir);
        assert_eq!(inv.count, 1, "坏 JSON 被跳过，仅合法文件投影");
        assert_eq!(inv.files[0].title, "", "title 缺省应为空串");
        assert!(
            inv.files[0].guard_for.is_empty(),
            "guard_for 缺省应为空数组"
        );
        assert_eq!(inv.files[0].promoted_from, None, "晋升账字段缺省应为 null");
        assert_eq!(inv.files[0].promoted_at, None);
        assert_eq!(inv.files[0].promoted_by, None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_l2_inventory_missing_dir_empty() {
        // fail-soft：rules_dir 不存在 → 空清单（count=0），不 panic
        let dir = std::env::temp_dir().join("l2inv_missing_dir_no_such");
        let _ = std::fs::remove_dir_all(&dir);
        let inv = SessionApi::l2_inventory(&dir);
        assert_eq!(inv.count, 0);
        assert!(inv.files.is_empty());
    }

    #[tokio::test]
    async fn test_l2_inventory_endpoint_200_empty_and_populated() {
        // 端点级：空目录 → 200 + 空清单；有 L2 文件 → 200 + 投影
        let (state, _) = make_test_state();

        // 空目录
        let empty = tempfile::tempdir().unwrap();
        let mut state_empty = state.clone();
        state_empty.sessions.rules_dir = empty.path().to_path_buf();

        let router = GovernanceServer::new(
            state_empty,
            AuthConfig::disabled(),
            "0.0.0.0:0".to_string(),
            0,
            0,
            vec![],
            false,
            false,
            false,
            false,
            None,
        )
        .build_router();
        let (status, json) = oneshot_json(router, "GET", "/api/rules/l2-inventory", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["count"], 0);
        assert_eq!(json["files"].as_array().map(|a| a.len()), Some(0));

        // 有 L2 文件
        let populated = tempfile::tempdir().unwrap();
        std::fs::write(
            populated.path().join("00_meta_seed.json"),
            r#"{"kind":"rule_set","metadata":{"tier":"meta","title":"种子元规则","guard_for":["robot_move"]},"transform":[{"type":"set","params":{"attr":"x","operation":"set","value":1}}]}"#,
        )
        .unwrap();
        let mut state_pop = state.clone();
        state_pop.sessions.rules_dir = populated.path().to_path_buf();
        let router = GovernanceServer::new(
            state_pop,
            AuthConfig::disabled(),
            "0.0.0.0:0".to_string(),
            0,
            0,
            vec![],
            false,
            false,
            false,
            false,
            None,
        )
        .build_router();
        let (status, json) = oneshot_json(router, "GET", "/api/rules/l2-inventory", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["count"], 1);
        assert_eq!(json["files"][0]["path"], "00_meta_seed.json");
        assert_eq!(json["files"][0]["title"], "种子元规则");
        assert_eq!(json["files"][0]["guard_for"][0], "robot_move");
    }

    #[test]
    fn test_contains_enforce_rule_bare_array() {
        // 裸数组形态（transform 顶层即数组）的 enforce 检测
        let with = serde_json::from_str::<serde_json::Value>(
            r#"[{"type":"set","params":{}},{"type":"enforce","params":{}}]"#,
        )
        .unwrap();
        let without = serde_json::from_str::<serde_json::Value>(
            r#"[{"type":"set","params":{}},{"type":"push","params":{}}]"#,
        )
        .unwrap();
        let embedded = serde_json::from_str::<serde_json::Value>(
            r#"{"metadata":{"title":"enforce 字样出现在标题不误报"},"transform":[{"type":"set","params":{}}]}"#,
        )
        .unwrap();
        assert!(SessionApi::contains_enforce_rule(&with));
        assert!(!SessionApi::contains_enforce_rule(&without));
        assert!(!SessionApi::contains_enforce_rule(&embedded));
    }

    // ===== V3/V4 parity 对齐测试（把关报告动作序列④尾；[CON-L4-002] V3/V4 风险面）=====
    // 锁「清单面（server.rs 既有语义）与装载面（rule_loader 唯一通道）对同一文档
    // 判定一致」——两实现长期并存（管辖分工见 SR-02 决策 3），漂移即红灯。
    mod parity {
        use super::*;

        /// V4：eq/lt/in domain 门 parity——同一规则树，清单面 domain_gate_violations
        /// （TCB 域）与装载面 walk_domain_gate_violations（serde 域）的违规必须
        /// **逐条全等**（门类+明细，按多重集比对），并锚定各样本期望门类——
        /// 含根段点路径判定谓词的边界样本（双侧共用 TCB 唯一权威实现）。
        #[test]
        fn v4_domain_gate_parity_same_violations() {
            const MISSING: &str = "missing_declaration";
            const INVALID: &str = "invalid_declaration";
            const AMBIGUOUS: &str = "value_literal_ambiguous";
            const VALUES: &str = "values_declaration";
            // (描述, 样本, 期望门类多重集)
            let samples: Vec<(&str, &str, Vec<&str>)> = vec![
                // 缺 on_missing
                (
                    "eq 缺 on_missing",
                    r#"{"type":"branch","params":{"domain":{"type":"eq","path":"payload.s","value":"x"},"on_true":[],"on_false":[]}}"#,
                    vec![MISSING],
                ),
                // on_missing 非法值
                (
                    "lt on_missing 非法",
                    r#"{"type":"branch","params":{"domain":{"type":"lt","path":"payload.c","value":5,"on_missing":"warn"},"on_true":[],"on_false":[]}}"#,
                    vec![INVALID],
                ),
                // value 字面量歧义（历史写作错误形态）
                (
                    "eq value 歧义",
                    r#"{"type":"branch","params":{"domain":{"type":"eq","path":"payload.m","value":"instruction.params.milestone_target","on_missing":"unsat"},"on_true":[],"on_false":[]}}"#,
                    vec![AMBIGUOUS],
                ),
                // 干净声明（零违例）
                (
                    "eq 干净",
                    r#"{"type":"branch","params":{"domain":{"type":"eq","path":"payload.s","value":"done","on_missing":"error"},"on_true":[],"on_false":[]}}"#,
                    vec![],
                ),
                // 嵌套 inner 内缺声明（walk 整树语义）
                (
                    "嵌套缺声明",
                    r#"{"type":"branch","params":{"domain":{"type":"all","inner":[{"type":"exists","path":"payload.a"},{"type":"eq","path":"payload.b","value":"y"}]},"on_true":[],"on_false":[]}}"#,
                    vec![MISSING],
                ),
                // __ 前缀显式路径引用=合法（不误伤）
                (
                    "__ 引用合法",
                    r#"{"type":"branch","params":{"domain":{"type":"eq","path":"payload.s","value":"__exec__.payload.__io_results__.x","on_missing":"unsat"},"on_true":[],"on_false":[]}}"#,
                    vec![],
                ),
                // in 域第 4 道 values 门三形态
                (
                    "in 缺 values",
                    r#"{"type":"branch","params":{"domain":{"type":"in","path":"payload.model","on_missing":"unsat"},"on_true":[],"on_false":[]}}"#,
                    vec![VALUES],
                ),
                (
                    "in values 非数组",
                    r#"{"type":"branch","params":{"domain":{"type":"in","path":"payload.model","values":"M3","on_missing":"unsat"},"on_true":[],"on_false":[]}}"#,
                    vec![VALUES],
                ),
                (
                    "in values 空数组",
                    r#"{"type":"branch","params":{"domain":{"type":"in","path":"payload.model","values":[],"on_missing":"unsat"},"on_true":[],"on_false":[]}}"#,
                    vec![VALUES],
                ),
                (
                    "in 元素歧义",
                    r#"{"type":"branch","params":{"domain":{"type":"in","path":"payload.model","values":["M3","payload.x"],"on_missing":"unsat"},"on_true":[],"on_false":[]}}"#,
                    vec![AMBIGUOUS],
                ),
                (
                    "in 合法",
                    r#"{"type":"branch","params":{"domain":{"type":"in","path":"payload.model","values":["M3","M2.5"],"on_missing":"unsat"},"on_true":[],"on_false":[]}}"#,
                    vec![],
                ),
                // 根段点路径判定谓词边界（双侧共用 TCB is_root_segment_dot_path）
                (
                    "谓词边界 含 __ 段",
                    r#"{"type":"branch","params":{"domain":{"type":"eq","path":"payload.t","value":"instruction.x__y","on_missing":"unsat"},"on_true":[],"on_false":[]}}"#,
                    vec![AMBIGUOUS],
                ),
                (
                    "谓词边界 空段",
                    r#"{"type":"branch","params":{"domain":{"type":"eq","path":"payload.t","value":"payload..x","on_missing":"unsat"},"on_true":[],"on_false":[]}}"#,
                    vec![],
                ),
                (
                    "谓词边界 queue 根段",
                    r#"{"type":"branch","params":{"domain":{"type":"eq","path":"payload.t","value":"queue.a.b","on_missing":"unsat"},"on_true":[],"on_false":[]}}"#,
                    vec![AMBIGUOUS],
                ),
                (
                    "谓词边界 非根段首词",
                    r#"{"type":"branch","params":{"domain":{"type":"eq","path":"payload.t","value":"instructionx.a","on_missing":"unsat"},"on_true":[],"on_false":[]}}"#,
                    vec![],
                ),
                (
                    "数值 value 不查歧义",
                    r#"{"type":"branch","params":{"domain":{"type":"lt","path":"payload.n","value":5,"on_missing":"unsat"},"on_true":[],"on_false":[]}}"#,
                    vec![],
                ),
            ];
            for (desc, body, expected_doors) in samples {
                let v: serde_json::Value = serde_json::from_str(body).unwrap();
                let tcb_rule = serde_to_tcb(v.clone());
                // 清单面：生产纯函数（TCB 域）
                let prod = SessionApi::domain_gate_violations(&tcb_rule);
                // 装载面：walk_domain_gate_violations（serde 域）
                let load = evorule_rule_loader::walk_domain_gate_violations(&v);
                let mut prod_items: Vec<(String, String)> =
                    prod.into_iter().map(|d| (d.door, d.detail)).collect();
                let mut load_items: Vec<(String, String)> = load
                    .into_iter()
                    .map(|h| (h.door.to_string(), h.detail))
                    .collect();
                // 逐条全等（门类+明细多重集；遍历序依赖各域 map 实现，不比顺序）
                prod_items.sort();
                load_items.sort();
                assert_eq!(
                    prod_items, load_items,
                    "V4 parity 破裂（逐条全等）[{desc}]：清单面={prod_items:?} 装载面={load_items:?}"
                );
                // 期望锚定：双侧都错成一致时仍能被基准抓住
                let mut prod_doors: Vec<String> =
                    prod_items.iter().map(|(d, _)| d.clone()).collect();
                prod_doors.sort();
                let mut expected: Vec<String> =
                    expected_doors.iter().map(|s| s.to_string()).collect();
                expected.sort();
                assert_eq!(
                    prod_doors, expected,
                    "V4 基准漂移 [{desc}]：实际={prod_doors:?} 预期={expected:?}"
                );
            }
        }
    }
}

#[cfg(test)]
mod gate_file_rejection_tests {
    use super::*;

    /// 门禁拒载的文件在 reload 链有程序化可查面——
    /// parse_rule_file 返回 Err(rejected) 而非静默 None,
    /// 且 gate 标签按 ConstitutionError 变体归类,全管线五元组携带
    #[tokio::test]
    async fn gate_rejected_file_surfaces_in_reload_chain() {
        let tmp = tempfile::tempdir().unwrap();
        let rules_dir = tmp.path().join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        let core_eval_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../resources/server_eval.json");

        // 种子:一个合法业务文件(保底,目录非空)
        std::fs::write(
            rules_dir.join("ok_rule.json"),
            r#"{"metadata":{"title":"ok-rule"},"transform":[{"type":"set","params":{"attr":"x","operation":"set","value":1}}]}"#,
        )
        .unwrap();
        // 违规:非 L2 文件携带 tier=constraint(层级伪造,tier 门拒载)
        std::fs::write(
            rules_dir.join("fake_l2.json"),
            r#"{"metadata":{"title":"fake","tier":"constraint"},"transform":[{"type":"set","params":{"attr":"a","operation":"set","value":"v"}}]}"#,
        )
        .unwrap();

        let (_rules, _layout, overlaps, domains, gate_files) =
            SessionApi::load_merged_with_layout(&core_eval_path, &rules_dir).unwrap();
        assert!(overlaps.is_empty() && domains.is_empty());
        let fake = gate_files
            .iter()
            .find(|g| g.path.ends_with("fake_l2.json"))
            .expect("层级伪造文件应出现在 rejected_gate_files");
        assert_eq!(fake.gate, "tier");
        assert!(
            fake.reason.contains("tier") || fake.reason.contains("L2"),
            "reason 应点名层级问题: {}",
            fake.reason
        );
    }
}

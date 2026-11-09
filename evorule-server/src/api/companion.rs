// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 伴跑状态机 A→B 最小版（获批设计稿）
//!
//! 审计链驱动的确定性旁路消费者：规则集版本变更（载入/编辑检出=同一装载漏斗）
//! → 去抖 → lint 三件套进程内直调（与 HTTP invoke 同一确定性内核）→ 报告落
//! 伴随域自有哈希链事实（记录性，不进业务会话链）→ 认证集更新 → 覆盖率可查询。
//!
//! 触发源（同一漏斗入口，报告与状态携带 trigger 标注）：
//! - `startup`：main.rs 启动期注入
//! - `reload`：管理端点 reload 漏斗尾
//! - `watch`：编辑检出 watcher（非递归 watch rules_dir 与 core_eval，事件批
//!   排空后单次只读求版；无第二去抖状态，唯一静默窗=runner 的 config.debounce）
//!
//! 执行隔离：分析 future 可注入专用 runtime（单 worker 线程）承载——CPU 密集
//! 段零抢占主多线程 runtime；未注入时内联执行（缺省不变式兜底）。
//! 链自检：启动期全量重放（坏链隔离重建，绝不静默）+ 空闲期内存链尾验 +
//! 落盘连续失败降级事实（status 可见）。
//!
//! 硬边界（四条照单全收）：
//! 1. 只报告不拦截——本模块无任何阻断权，产出=报告/证书+查询面
//! 2. 干预必留痕——enable/disable 双向落链事实 + `tracing::warn!`
//! 3. 收尾覆盖率打标——`GET /api/companion/coverage` 可查询
//! 4. 零侵入主循环——独立 tokio task（`run`），不改 evo-agent（零 commit）
//!    与 server 既有 handler 语义（新增独立 `/api/companion/*` 命名空间）
//!
//! 落链形态（获批设计稿 D2 项的施工实现）：`evorule-reactor` 为
//! crates.io 依赖（vendored patch 已退役收编为原生 crate），本批不可加枚举
//! 变体——故为**伴随域自有哈希链事实**：JSONL 追加 + blake3 前链（`prev_hash`
//! 链接、`evorule_hash::json_digest` 键序稳定摘要），记录性事实语义与
//! reactor `TransitionTrace` 同构（不推进会话版本）；链文件与 wal-dir 同级
//! 配置面（`companion_chain.jsonl`），无 wal-dir（纯内存/测试）时内存链兜底。

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::json;
use tokio_util::sync::CancellationToken;

use evorule_inference_services::conflict::ConflictReport;
use evorule_inference_services::cycle::CycleReport;
use evorule_inference_services::rw_extract::RuleRw;

use super::companion_uplink::{
    companion_uplink_entry, companion_uplink_entry_id, companion_uplink_payload, post_propose,
    CompanionUplinkConfig,
};
use super::server::SessionApi;

/// lint 三件套（设计稿：全部非 sensitive，REST 直调合法，进程内直调同权）
pub(crate) const LINT_SERVICE_NAMES: &[&str] = &[
    "inference_rw_extract",
    "inference_cycle_detect",
    "inference_conflict_detect",
];

/// 缺省去抖静默窗（获批设计稿 D3 参数）
pub(crate) const DEFAULT_DEBOUNCE_MS: u64 = 3000;

/// 缺省两次已完成分析最小间隔（获批设计稿 D3 参数）
pub(crate) const DEFAULT_MIN_INTERVAL_MS: u64 = 60_000;

/// 分析抢占哨兵：被更新版本取消的分析以此为 suite 错误返回，
/// `analyze` 识别后**不落链、不计 failed、不进错误历史**（仅 stale_cancelled++）
pub(crate) const CANCEL_SENTINEL: &str = "companion analysis cancelled (superseded)";

/// 认证集容量策略：锚定版本常驻 + 最近 N 条（硬上限 9）
pub(crate) const CERTIFIED_MAX_EXTRA: usize = 8;

/// 错误历史环形缓冲容量（status 按时序呈现）
pub(crate) const ERROR_HISTORY_CAP: usize = 5;

/// 触发源标注（报告 payload 与 status 呈现；同一漏斗入口的可观测区分面）
pub(crate) const TRIGGER_STARTUP: &str = "startup";
pub(crate) const TRIGGER_RELOAD: &str = "reload";
pub(crate) const TRIGGER_WATCH: &str = "watch";

/// 落盘连续失败降级阈值（达到即落 ChainDegraded 降级事实 + status 呈现）
const CHAIN_DEGRADE_THRESHOLD: u64 = 3;

// ── lint 增量分析状态面（rw 读写集缓存 + per-file 摘要表）──

/// 上次分析成功的 lint 状态快照（单表有界：只留最近一次成功分析的
/// per-file 摘要 + per-file 读写集；下次分析 diff 之即得变更文件集）
///
/// 键 = 文件来源标注（rules_dir 内相对路径或 `core_eval`）；值：
/// - `digests`：per-file 内容摘要（后过滤口径——跨文件重叠防线会因其他
///   文件变更改变本文件的后过滤规则集，故缓存有效性键必须锚定后过滤
///   分组而非装载器文档哈希；摘要原语=evorule_hash json_digest 族，
///   与 ruleset_version 同源）
/// - `rws`：per-file 规则读写集（合并序下标已回填；宪法段含 excluded 条目）
#[derive(Debug, Clone, Default)]
pub(crate) struct LintRwCache {
    pub digests: BTreeMap<String, String>,
    pub rws: BTreeMap<String, Vec<RuleRw>>,
}

impl LintRwCache {
    /// diff 新旧摘要表 → 变更/新增/删除文件集合（升序，确定性）
    pub fn diff_changed_files(&self, new_digests: &BTreeMap<String, String>) -> Vec<String> {
        let mut changed: Vec<String> = new_digests
            .iter()
            .filter(|(k, v)| self.digests.get(*k) != Some(*v))
            .map(|(k, _)| k.clone())
            .collect();
        for k in self.digests.keys() {
            if !new_digests.contains_key(k) {
                changed.push(k.clone());
            }
        }
        changed.sort();
        changed.dedup();
        changed
    }
}

/// RuleRw → JSON（增量路径序列化器：与 inference-services 服务输出同形——
/// 服务实现冻结零改，对拍测试锚定逐字段一致）
pub(crate) fn rw_entry_to_json(rw: &RuleRw) -> serde_json::Value {
    let set_json = |s: &std::collections::BTreeSet<String>| {
        serde_json::Value::Array(
            s.iter()
                .map(|v| serde_json::Value::String(v.clone()))
                .collect(),
        )
    };
    let const_pairs: Vec<serde_json::Value> = rw
        .const_writes
        .iter()
        .map(|(k, v)| {
            json!({
                "path": k,
                "literal": v,
            })
        })
        .collect();
    let io_pairs: Vec<serde_json::Value> = rw
        .ios
        .iter()
        .map(|e| {
            json!({
                "io_type": &e.io_type,
                "service_name": &e.service_name,
            })
        })
        .collect();
    json!({
        "id": &rw.id,
        "source": &rw.source,
        "index": i64::try_from(rw.index).unwrap_or(-1),
        "reads": set_json(&rw.reads),
        "writes": set_json(&rw.writes),
        "const_writes": const_pairs,
        "ios": io_pairs,
        "instr_types": set_json(&rw.instr_types),
        "domain_dyn": rw.domain_dyn,
        "write_dyn": rw.write_dyn,
        "domain_universal": rw.domain_universal,
        "excluded": rw.excluded,
    })
}

/// 合并读写集 → rw_extract 服务输出形态（status/determinism 信封 + results）
pub(crate) fn rw_results_to_json(rws: &[RuleRw]) -> serde_json::Value {
    let results: Vec<serde_json::Value> = rws.iter().map(rw_entry_to_json).collect();
    json!({
        "status": "determined",
        "determinism": "bitwise_replayable",
        "results": results,
    })
}

/// CycleReport → cycle_detect 服务输出形态（统一信封 v1）
pub(crate) fn cycle_report_to_json(report: &CycleReport) -> serde_json::Value {
    let cycle_list = |cycles: &[Vec<String>]| {
        serde_json::Value::Array(
            cycles
                .iter()
                .map(|c| {
                    serde_json::Value::Array(
                        c.iter()
                            .map(|id| serde_json::Value::String(id.clone()))
                            .collect(),
                    )
                })
                .collect(),
        )
    };
    let id_list = |ids: &[String]| {
        serde_json::Value::Array(
            ids.iter()
                .map(|id| serde_json::Value::String(id.clone()))
                .collect(),
        )
    };
    json!({
        "status": "determined",
        "determinism": "bitwise_replayable",
        "has_cycles": report.has_cycles,
        "cycle_count": i64::try_from(report.cycle_count).unwrap_or(-1),
        "static_cycles": cycle_list(&report.static_cycles),
        "conservative_cycles": cycle_list(&report.conservative_cycles),
        "topological_order": id_list(&report.topological_order),
        "safe_rules": id_list(&report.safe_rules),
        "unsafe_rules": id_list(&report.unsafe_rules),
        "excluded": id_list(&report.excluded),
    })
}

/// ConflictReport → conflict_detect 服务输出形态（统一信封 v1）
pub(crate) fn conflict_report_to_json(report: &ConflictReport) -> serde_json::Value {
    let items: Vec<serde_json::Value> = report
        .conflicts
        .iter()
        .map(|c| {
            json!({
                "rule_id_1": &c.rule_id_1,
                "rule_id_2": &c.rule_id_2,
                "conflict_type": &c.conflict_type,
                "description": &c.description,
                "conservative": c.conservative,
            })
        })
        .collect();
    json!({
        "status": "determined",
        "determinism": "bitwise_replayable",
        "has_conflicts": report.has_conflicts,
        "conflict_count": i64::try_from(report.conflict_count).unwrap_or(-1),
        "conflicts": items,
        "excluded": &report.excluded,
    })
}

/// 伴跑状态（验收骨架「状态链可见」五态链）
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Phase {
    /// 静默·当前版本已认证（或尚无版本信号）
    Idle,
    /// 脏·检出版本变更，去抖计时中
    Dirty,
    /// 分析中·lint 三件套执行
    Analyzing,
    /// 证书出具·报告落链完成
    Certified,
    /// 停着·待命（disabled；A 态，零伴跑活动）
    Disabled,
}

impl Phase {
    fn as_str(&self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Dirty => "dirty",
            Phase::Analyzing => "analyzing",
            Phase::Certified => "certified",
            Phase::Disabled => "disabled",
        }
    }
}

/// 伴随域哈希链状态（合成治理会话链的物理载体）
struct ChainState {
    /// 链文件路径（None=纯内存模式：测试/无 wal-dir 部署；链仍在内存可验证）
    path: Option<std::path::PathBuf>,
    /// 报告全文落盘目录（path 的兄弟目录 reports/；None 同步为 None）
    reports_dir: Option<std::path::PathBuf>,
    seq: u64,
    prev_hash: String,
    /// 内存链（有界保留尾部 CHAIN_MEM_CAP 条，供 status/verify 快照）
    mem: Vec<serde_json::Value>,
}

/// 内存链尾部保留上限（status 呈现面，防无界增长；全量以链文件为准）
const CHAIN_MEM_CAP: usize = 64;

/// 认证条目：认证时刻 + clean 标记（两级证书：clean ⊆ certified 由
/// 单窗口单条目结构保证——clean 是条目上的属性，不是平行集合）
#[derive(Debug, Clone, Copy)]
struct CertifiedEntry {
    at: u64,
    clean: bool,
}

/// 有界认证集：锚定版本常驻 + 最近 [`CERTIFIED_MAX_EXTRA`] 条（淘汰者由
/// 调用方落链 `CompanionCertExpired` 记录性事实——认证集是窗口不是全集）
struct CertifiedSet {
    map: HashMap<String, CertifiedEntry>,
    order: VecDeque<String>,
    anchor: Option<String>,
}

impl CertifiedSet {
    fn new(anchor: Option<String>) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            anchor,
        }
    }

    fn contains(&self, v: &str) -> bool {
        self.map.contains_key(v)
    }

    /// clean 标记读取（仅对在窗口内的版本有意义；窗口外恒 false）
    fn is_clean(&self, v: &str) -> bool {
        self.map.get(v).is_some_and(|e| e.clean)
    }

    fn len(&self) -> usize {
        self.map.len()
    }

    fn versions(&self) -> Vec<String> {
        self.map.keys().cloned().collect()
    }

    /// clean 版本清单（升序，呈现面确定性）
    fn clean_versions(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .map
            .iter()
            .filter(|(_, e)| e.clean)
            .map(|(k, _)| k.clone())
            .collect();
        v.sort();
        v
    }

    /// 插入认证（含 clean 标记）；超限淘汰最旧的非锚版本并返回之
    /// （`(版本, 认证时刻)`，供落链记录性事实）
    fn insert(&mut self, version: &str, at: u64, clean: bool) -> Option<(String, u64)> {
        if self
            .map
            .insert(version.to_string(), CertifiedEntry { at, clean })
            .is_none()
        {
            self.order.push_back(version.to_string());
        }
        let cap = CERTIFIED_MAX_EXTRA + usize::from(self.anchor.is_some());
        let mut evicted = None;
        while self.map.len() > cap {
            // 弹出最旧；跳过锚（锚永不淘汰）
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if Some(old.as_str()) == self.anchor.as_deref() {
                self.order.push_back(old); // 锚挪回队尾保活，继续找非锚
                let mut next = None;
                for (i, v) in self.order.iter().enumerate() {
                    if Some(v.as_str()) != self.anchor.as_deref() {
                        next = Some(i);
                        break;
                    }
                }
                match next {
                    Some(i) => {
                        if let Some(v) = self.order.remove(i) {
                            if let Some(entry) = self.map.remove(&v) {
                                evicted = Some((v, entry.at));
                            }
                        }
                    }
                    None => break, // 全是锚（理论不可达）
                }
            } else {
                if let Some(entry) = self.map.remove(&old) {
                    evicted = Some((old, entry.at));
                }
            }
        }
        evicted
    }
}

/// 提案内存呈现窗容量上限（提案记录是链事实的有界呈现窗；链事实
/// 永久留痕可审计，超限淘汰最旧——与链条目内存窗同纪律）
const PROPOSALS_CAP: usize = 64;

/// C 态提案记录（内存呈现面）：机器观测（alert 级发现）自动产生，
/// 人工处置（approve/reject）只完成提案生命周期——零拦截红线：
/// 处置不改变伴跑分析/认证/报告行为
struct ProposalRecord {
    proposal_id: String,
    ruleset_version: String,
    findings_digest: String,
    findings: serde_json::Value,
    proposed_at: u64,
    status: &'static str,
    adjudicated_at: Option<u64>,
    actor: Option<String>,
}

impl ProposalRecord {
    fn render(&self) -> serde_json::Value {
        json!({
            "proposal_id": self.proposal_id,
            "ruleset_version": self.ruleset_version,
            "findings_digest": self.findings_digest,
            "findings": self.findings,
            "proposed_at": self.proposed_at,
            "status": self.status,
            "adjudicated_at": self.adjudicated_at,
            "actor": self.actor,
        })
    }
}

/// 伴跑配置（获批设计稿 D3 参数，配置面可调）
#[derive(Debug, Clone)]
pub struct CompanionConfig {
    pub debounce: Duration,
    pub min_interval: Duration,
}

/// C 态提案治理链上行状态（键=proposal_id；D2 状态呈现面）。
/// `pending`=出站任务在途；终态 `ok`/`failed` 由出站任务写回。
/// 有界性：键域=提案呈现窗（同源 PROPOSALS_CAP 淘汰节奏，提案淘汰后
/// 残留状态条目随同提案窗一致有界——状态仅经 uplink_proposal 写入，
/// 写入前提是提案在窗内）
struct UplinkState {
    state: &'static str,
    attempted_at: u64,
    last_error: Option<String>,
    http_status: Option<u16>,
}

impl UplinkState {
    fn render(&self) -> serde_json::Value {
        json!({
            "state": self.state,
            "attempted_at": self.attempted_at,
            "last_error": self.last_error,
            "http_status": self.http_status,
        })
    }
}

/// 上行请求守卫错误（管理面映射 HTTP 语义：未配置=400 / 未找到=404 /
/// 非 approved 或 in-flight=409）。零拦截红线：错误只影响上行动作本身，
/// 绝不影响裁决结果（裁决在先已完成）。
#[derive(Debug)]
pub(crate) enum UplinkError {
    NotConfigured,
    NotFound,
    NotApproved(String),
    InFlight,
}

impl std::fmt::Display for UplinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UplinkError::NotConfigured => write!(
                f,
                "companion uplink not configured (governance uplink disabled)"
            ),
            UplinkError::NotFound => write!(f, "proposal not found"),
            UplinkError::NotApproved(status) => {
                write!(f, "proposal not approved (status: {status}); only approved proposals can be uplinked")
            }
            UplinkError::InFlight => write!(f, "uplink already in flight for this proposal"),
        }
    }
}

impl Default for CompanionConfig {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(DEFAULT_DEBOUNCE_MS),
            min_interval: Duration::from_millis(DEFAULT_MIN_INTERVAL_MS),
        }
    }
}

impl CompanionConfig {
    /// 测试态（去抖 10ms/节流 0）：契约与集成测试快进
    pub fn for_tests() -> Self {
        Self {
            debounce: Duration::from_millis(10),
            min_interval: Duration::from_millis(0),
        }
    }
}

/// 伴跑中枢（进程内单例；`Arc<CompanionHub>` 经 `with_companion` 注入
/// [`SessionApi`]，runner 经 `tokio::spawn` 独立 task 运行）
pub struct CompanionHub {
    config: CompanionConfig,
    /// B 态开关（原子读面；写面经 `set_enabled` 留痕）
    enabled: AtomicBool,
    /// enabled 的 watch 面（runner 阻塞等待 B 态用）
    enabled_tx: tokio::sync::watch::Sender<bool>,
    /// enabled master 接收端（保活面：防 send 无接收端丢值——run 里后订阅的
    /// receiver 依赖 sender 存储的最新值；与 version_rx 构造期预建同构）
    _enabled_rx_master: tokio::sync::watch::Receiver<bool>,
    /// 版本信号（`None` 尚无信号；`Some(v)` 最新已知规则集版本）
    version_tx: tokio::sync::watch::Sender<Option<String>>,
    /// 构造期预建接收端（run 克隆消费——防 subscribe 晚于首个信号漏事件）
    version_rx: tokio::sync::watch::Receiver<Option<String>>,
    phase: Mutex<Phase>,
    /// 最新已知规则集版本（信号面，与认证面 `certified` 分离）
    latest_version: Mutex<Option<String>>,
    dirty_since: Mutex<Option<u64>>,
    last_report: Mutex<Option<serde_json::Value>>,
    /// 错误历史环形缓冲（容量 [`ERROR_HISTORY_CAP`]，时序；status 的
    /// `last_error` 字段=最新一条，兼容既有消费者）
    errors: Mutex<VecDeque<String>>,
    last_analysis_done: Mutex<Option<std::time::Instant>>,
    /// 认证集（有界：锚+最近 N；淘汰落链 CompanionCertExpired）
    certified: Mutex<CertifiedSet>,
    /// 分析抢占令牌（per-run 换新：notify 更新版本时取消 in-flight 分析）
    cancel_token: Mutex<Option<CancellationToken>>,
    /// 锚定版本（启动期注入=会话创建 `__ruleset__.version` 同源；覆盖率会话归属判据）
    anchor_version: Mutex<Option<String>>,
    /// 最近一次信号的触发源标注（startup/reload/watch；报告 payload 呈现）
    last_trigger: Mutex<&'static str>,
    /// 分析执行 runtime 句柄（None=内联执行缺省兜底；Some=专用 runtime 承载）
    exec: Mutex<Option<tokio::runtime::Handle>>,
    /// 计数器（status 呈现面）
    analyses_started: AtomicU64,
    analyses_ok: AtomicU64,
    analyses_failed: AtomicU64,
    stale_skipped: AtomicU64,
    /// 被更新版本抢占取消的分析数（与真失败严格区分：不落链不计 failed）
    stale_cancelled: AtomicU64,
    chain: Mutex<ChainState>,
    /// 链完整性最近一次校验结论（启动重放/空闲尾验共同更新）
    chain_verified: AtomicBool,
    /// 落盘连续失败降级标志（达阈值置位；一次成功追加即恢复）
    chain_write_degraded: AtomicBool,
    /// 落盘连续失败计数（达 [`CHAIN_DEGRADE_THRESHOLD`] 触发降级事实）
    append_fail_streak: AtomicU64,
    /// Prometheus 指标面（None=完全关闭；只读计数零控制面——硬边界）
    metrics: Mutex<Option<CompanionMetrics>>,
    /// lint 增量状态（上次成功分析的 per-file 摘要+读写集；None=尚无基线，
    /// 下次分析走全量；成功分析后提交——单表有界，不随版本增长）
    lint_cache: Mutex<Option<LintRwCache>>,
    /// C 态提案记录（有界呈现窗；链事实永久留痕）
    proposals: Mutex<Vec<ProposalRecord>>,
    /// C 态提案治理链上行状态（键=proposal_id；缺省空=上行面零行为）
    uplink: Mutex<HashMap<String, UplinkState>>,
    /// 治理链上行配置（None=未配置=上行面完全关闭；main.rs 启动期 env
    /// 读取注入，测试可注入环回 stub。缺省关：未配置时提案/裁决照常）
    uplink_config: Mutex<Option<CompanionUplinkConfig>>,
    /// law_set 装配投影（启动期 law_host 装配结论逐集列明；纯呈现面，
    /// 零触 lint 漏斗——law_set 走独立部署件通道不进本中枢分析面）。
    /// 缺省空数组=未配置 law_set 装配面（现行为不变）。
    law_sets: Mutex<Vec<serde_json::Value>>,
}

impl CompanionHub {
    /// 构造中枢。`chain_path`：链文件路径（None=纯内存）。
    /// `anchor_version`：启动期规则集版本（与 G6 会话锚定同源）。
    ///
    /// 既有链文件在构造期全量重放校验（启动期自检；坏链隔离重建——
    /// 坏文件改名留档，首条落 `ChainIntegrityAlert`，绝不静默重建）。
    pub fn new(
        config: CompanionConfig,
        chain_path: Option<std::path::PathBuf>,
        anchor_version: Option<String>,
    ) -> Arc<Self> {
        let (enabled_tx, enabled_rx_master) = tokio::sync::watch::channel(false);
        let (version_tx, version_rx) = tokio::sync::watch::channel(None::<String>);
        let reports_dir = chain_path.as_ref().map(|p| {
            p.parent()
                .map(|d| d.join("reports"))
                .unwrap_or_else(|| std::path::PathBuf::from("reports"))
        });
        let replay = chain_path.as_deref().map(replay_chain_from_disk);
        let (replay_seq, replay_prev, replay_mem, replay_verified, replay_alert) = match &replay {
            Some(r) => (
                r.seq,
                r.prev_hash.clone(),
                r.mem.clone(),
                r.verified,
                r.alert.clone(),
            ),
            None => (0, String::new(), Vec::new(), true, None),
        };
        let hub = Arc::new(Self {
            config,
            enabled: AtomicBool::new(false),
            enabled_tx,
            _enabled_rx_master: enabled_rx_master,
            version_tx,
            version_rx,
            phase: Mutex::new(Phase::Disabled),
            latest_version: Mutex::new(None),
            dirty_since: Mutex::new(None),
            last_report: Mutex::new(None),
            errors: Mutex::new(VecDeque::new()),
            last_analysis_done: Mutex::new(None),
            certified: Mutex::new(CertifiedSet::new(anchor_version.clone())),
            cancel_token: Mutex::new(None),
            anchor_version: Mutex::new(anchor_version),
            last_trigger: Mutex::new(TRIGGER_STARTUP),
            exec: Mutex::new(None),
            analyses_started: AtomicU64::new(0),
            analyses_ok: AtomicU64::new(0),
            analyses_failed: AtomicU64::new(0),
            stale_skipped: AtomicU64::new(0),
            stale_cancelled: AtomicU64::new(0),
            chain: Mutex::new(ChainState {
                path: chain_path,
                reports_dir,
                seq: replay_seq,
                prev_hash: replay_prev,
                mem: replay_mem,
            }),
            chain_verified: AtomicBool::new(replay_verified),
            chain_write_degraded: AtomicBool::new(false),
            append_fail_streak: AtomicU64::new(0),
            metrics: Mutex::new(None),
            lint_cache: Mutex::new(None),
            proposals: Mutex::new(Vec::new()),
            uplink: Mutex::new(HashMap::new()),
            uplink_config: Mutex::new(None),
            law_sets: Mutex::new(Vec::new()),
        });
        // 坏链隔离重建：告警事实为重启后新链首条（error 日志已在重放时落）
        if let Some(alert) = replay_alert {
            hub.append_chain("ChainIntegrityAlert", alert);
        }
        hub
    }

    /// 挂载 Prometheus 指标面（与 server 既有 registry 同注册形态；只读计数；
    /// 构造后运行前调用，重复调用以最后一次为准）
    pub fn with_metrics(self: &Arc<Self>, registry: &prometheus::Registry) {
        *lock_poem(&self.metrics) = CompanionMetrics::register(registry);
    }

    /// 注入治理链上行配置（main.rs 启动期 env 读取产物；None=保持关闭）。
    /// 缺省关：未配置时上行面零行为（提案/裁决/分析/认证照常）；测试可
    /// 注入环回 stub 地址（同 `with_metrics` 构造后运行前调用形态）。
    pub fn with_uplink_config(self: &Arc<Self>, cfg: Option<CompanionUplinkConfig>) {
        *lock_poem(&self.uplink_config) = cfg;
    }

    /// 注入分析执行 runtime（单 worker 线程承载三件套 CPU 密集段；构造后
    /// 运行前调用；不注入=内联执行缺省兜底）
    pub fn with_exec_runtime(self: &Arc<Self>, handle: tokio::runtime::Handle) {
        *lock_poem(&self.exec) = Some(handle);
    }

    /// 注入 law_set 装配投影（main.rs 启动期 law_host 装配结论；构造后
    /// 运行前调用，启动后不变）。逐集列明（loaded+rejected），空切片=
    /// 未配置 law_set 装配面（status 呈现空数组，缺省不变式）。
    pub fn with_law_sets_projection(self: &Arc<Self>, sets: Vec<serde_json::Value>) {
        *lock_poem(&self.law_sets) = sets;
    }

    /// 装载漏斗信号入口（启动=reload=watch 同一漏斗；事件驱动零轮询）。
    /// `source`：触发源标注（`TRIGGER_*`），随报告 payload 与 status 呈现；
    /// 同版本信号幂等（与 source 无关——版本相同即零活动）。
    pub fn notify_version_source(&self, version: String, source: &'static str) {
        {
            let mut lv = lock_poem(&self.latest_version);
            if lv.as_deref() == Some(version.as_str()) {
                return; // 同版本信号幂等（零活动语义：无变更即无事件）
            }
            *lv = Some(version.clone());
        }
        *lock_poem(&self.last_trigger) = source;
        // 抢占：新版本到达即取消 in-flight 旧版分析（防证书序列倒挂）
        if let Some(token) = lock_poem(&self.cancel_token).as_ref() {
            token.cancel();
            self.stale_cancelled.fetch_add(1, Ordering::Relaxed);
            self.record_metric_stale();
        }
        {
            let mut ds = lock_poem(&self.dirty_since);
            if ds.is_none() {
                *ds = Some(now_secs());
            }
        }
        self.set_phase_if_enabled(Phase::Dirty);
        let _ = self.version_tx.send(Some(version));
    }

    /// [`notify_version_source`] 的缺省源便捷封装（启动期信号=缺省源；
    /// reload/watch 漏斗点用显式源标注）
    pub fn notify_version(&self, version: String) {
        self.notify_version_source(version, TRIGGER_STARTUP);
    }

    fn set_phase_if_enabled(&self, phase: Phase) {
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        *lock_poem(&self.phase) = phase;
    }

    /// B/A 切换唯一入口（硬边界②：双向落链事实 + warn 级日志）
    pub fn set_enabled(&self, on: bool, actor: &str) {
        self.enabled.store(on, Ordering::Relaxed);
        let _ = self.enabled_tx.send(on);
        {
            let mut ph = lock_poem(&self.phase);
            *ph = if on {
                // enable 后已知版本未认证 → 直接进脏态（runner 会话驱动分析）
                let uncertified = {
                    let lv = lock_poem(&self.latest_version).clone();
                    match lv {
                        Some(v) => !lock_poem(&self.certified).contains(&v),
                        None => false,
                    }
                };
                if uncertified {
                    Phase::Dirty
                } else {
                    Phase::Idle
                }
            } else {
                Phase::Disabled
            };
        }
        tracing::warn!(enabled = on, actor, "companion state changed");
        self.append_chain(
            "CompanionStateChange",
            json!({ "enabled": on, "actor": actor }),
        );
        if on {
            // enable 触发补分析：把当前已知版本再发一次信号（幂等面由认证集挡）
            if let Some(v) = lock_poem(&self.latest_version).clone() {
                let _ = self.version_tx.send(Some(v));
            }
        }
    }

    /// runner 主循环（独立 tokio task；单任务=天然 single-flight，最新版胜出）
    pub async fn run(self: Arc<Self>, api: SessionApi) {
        // 构造期预建接收端的克隆：首个信号（含 run 启动前已达的 startup 信号）必达
        let mut rx = self.version_rx.clone();
        let mut enabled_rx = self.enabled_tx.subscribe();
        loop {
            // 等待版本信号（事件驱动，零轮询）
            if rx.changed().await.is_err() {
                return;
            }
            let Some(_version) = rx.borrow_and_update().clone() else {
                continue;
            };
            // A→B 闸：未启用则不分析（待命零活动语义；enable 时 set_enabled 会补发信号）
            loop {
                if *enabled_rx.borrow() {
                    break;
                }
                if enabled_rx.changed().await.is_err() {
                    return;
                }
            }
            // 去抖：静默窗后取最新版（窗内新信号到来则以最新版为准）
            tokio::time::sleep(self.config.debounce).await;
            let latest = match rx.borrow().clone() {
                Some(v) => v,
                None => continue,
            };
            // 已认证且未变 → 零活动
            if lock_poem(&self.certified).contains(&latest) {
                *lock_poem(&self.phase) = Phase::Certified;
                continue;
            }
            // 节流：距上次完成不足最小间隔 → 等满余量再分析（保证最终一致）
            let remaining = {
                let done = lock_poem(&self.last_analysis_done);
                match *done {
                    Some(t) => self
                        .config
                        .min_interval
                        .checked_sub(t.elapsed())
                        .unwrap_or(Duration::ZERO),
                    None => Duration::ZERO,
                }
            };
            if !remaining.is_zero() {
                tokio::time::sleep(remaining).await;
            }
            self.analyze(&api, &latest).await;
            // 空闲期链尾自检：认证成功后延时一个节流周期做内存链重算
            // （detached 任务，不占 runner 单飞；失败落 ChainIntegrityAlert）
            if *lock_poem(&self.phase) == Phase::Certified {
                self.spawn_idle_verification();
            }
        }
    }

    /// 空闲期链尾增量验证（最近 [`CHAIN_MEM_CAP`] 条内存链重算）：
    /// 通过 → `chain_verified=true`；失败 → error 日志 + `ChainIntegrityAlert`
    /// 落链 + `chain_verified=false`（status 呈现，绝不静默）。
    fn spawn_idle_verification(self: &Arc<Self>) {
        let hub = self.clone();
        let wait = self.config.min_interval;
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            match hub.verify_chain() {
                Ok(_) => hub.chain_verified.store(true, Ordering::Relaxed),
                Err(e) => hub.chain_integrity_alert(format!("idle tail verification failed: {e}")),
            }
        });
    }

    /// 链完整性告警（启动重放判坏 / 空闲尾验失败共用的唯一收口）：
    /// error 日志 + `chain_verified=false` + `ChainIntegrityAlert` 记录性事实。
    fn chain_integrity_alert(&self, detail: String) {
        self.chain_verified.store(false, Ordering::Relaxed);
        tracing::error!(detail = %detail, "companion chain integrity alert");
        self.append_chain("ChainIntegrityAlert", json!({ "detail": detail }));
    }

    /// 执行一次分析（快照即喂→三件套→落链→认证；per-run 取消令牌防倒挂）。
    /// 注入执行 runtime 时分析 future 派发至专用单 worker runtime——CPU 密集
    /// 段零抢占主多线程 runtime；未注入时内联执行（缺省兜底）。
    async fn analyze(&self, api: &SessionApi, version: &str) {
        self.analyses_started.fetch_add(1, Ordering::Relaxed);
        *lock_poem(&self.phase) = Phase::Analyzing;
        let token = CancellationToken::new();
        *lock_poem(&self.cancel_token) = Some(token.clone());
        let started = std::time::Instant::now();
        // 局部绑定：锁 guard 随语句结束释放（不得跨 await 持有——Send 约束）
        let exec_handle = lock_poem(&self.exec).clone();
        let result = match exec_handle {
            Some(handle) => {
                // 专用执行 runtime：owned 克隆进 future（'static 约束）
                let api = api.clone();
                let version = version.to_string();
                match handle
                    .spawn(
                        async move { api.companion_run_lint_suite(&version, Some(&token)).await },
                    )
                    .await
                {
                    Ok(r) => r,
                    Err(e) => Err(format!("companion exec runtime join failed: {e}")),
                }
            }
            None => api.companion_run_lint_suite(version, Some(&token)).await,
        };
        *lock_poem(&self.cancel_token) = None;
        match result {
            Ok(report) => self.certify_success(version, &report, started),
            Err(e) if e == CANCEL_SENTINEL => {
                // 被更新版本抢占：不落链、不计 failed、不进错误历史
                *lock_poem(&self.dirty_since) = None;
                tracing::info!(ruleset_version = version, "companion analysis superseded");
            }
            Err(e) => {
                self.analyses_failed.fetch_add(1, Ordering::Relaxed);
                {
                    let mut errs = lock_poem(&self.errors);
                    if errs.len() >= ERROR_HISTORY_CAP {
                        errs.pop_front();
                    }
                    errs.push_back(e.clone());
                }
                // 失败回到脏态（fail-visible：错误历史呈现；下一次版本信号自然重试）
                *lock_poem(&self.phase) = Phase::Dirty;
                self.record_metric_failed(started.elapsed());
                tracing::warn!(ruleset_version = version, error = %e, "companion analysis failed");
            }
        }
    }

    /// 成功分支收口：报告落链（含全文落盘）→ 认证集有界插入（淘汰者落链）→
    /// 状态/计数/指标更新（错误历史清零）
    fn certify_success(
        &self,
        version: &str,
        report: &serde_json::Value,
        started: std::time::Instant,
    ) {
        let report_digest = evorule_hash::json_digest(report);
        let analyses: Vec<serde_json::Value> = report
            .get("analyses")
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|a| {
                        json!({
                            "service": a.get("service"),
                            "ok": a.get("ok"),
                            "error": a.get("error"),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        // severity 两级判定（只读 analyses 输出，服务实现零改）：
        // conflict 有发现→alert > cycle 有发现→warn > 全净→clean；
        // rw excluded/异常形态为 info 级，不阻断 clean（弱化语义）；
        // 服务失败（ok=false）无发现数据可读→warn（不确定性下限，阻断 clean）
        let mut severity = "clean";
        let mut findings_conflicts: i64 = 0;
        let mut findings_cycles: i64 = 0;
        let mut findings_rw_excluded: i64 = 0;
        if let Some(items) = report.get("analyses").and_then(|a| a.as_array()) {
            for a in items {
                let service = a.get("service").and_then(|v| v.as_str()).unwrap_or("");
                let result = a.get("result");
                match (service, a.get("ok").and_then(|v| v.as_bool())) {
                    (_, Some(false)) | (_, None) => severity = "warn",
                    ("inference_conflict_detect", Some(true)) => {
                        if let Some(r) = result {
                            if r.get("has_conflicts").and_then(|v| v.as_bool()) == Some(true) {
                                severity = "alert";
                            }
                            findings_conflicts += r
                                .get("conflict_count")
                                .and_then(|v| v.as_i64())
                                .unwrap_or(0);
                        }
                    }
                    ("inference_cycle_detect", Some(true)) => {
                        if let Some(r) = result {
                            if r.get("has_cycles").and_then(|v| v.as_bool()) == Some(true)
                                && severity != "alert"
                            {
                                severity = "warn";
                            }
                            findings_cycles +=
                                r.get("cycle_count").and_then(|v| v.as_i64()).unwrap_or(0);
                        }
                    }
                    ("inference_rw_extract", Some(true)) => {
                        if let Some(items) = result
                            .and_then(|r| r.get("results"))
                            .and_then(|v| v.as_array())
                        {
                            findings_rw_excluded += items
                                .iter()
                                .filter(|x| {
                                    x.get("excluded").and_then(|v| v.as_bool()) == Some(true)
                                })
                                .count() as i64;
                        }
                    }
                    _ => {}
                }
            }
        }
        let clean = severity == "clean";
        let findings = json!({
            "conflicts": findings_conflicts,
            "cycles": findings_cycles,
            "rw_excluded": findings_rw_excluded,
        });
        let seq_report = self.append_chain_with_artifact(
            "CompanionReport",
            json!({
                "ruleset_version": version,
                "rule_count": report.get("rule_count"),
                "constitution_excluded_count": report.get("constitution_excluded_count"),
                "severity": severity,
                "findings": &findings,
                "analyses": analyses,
                "trigger": lock_poem(&self.last_trigger).clone(),
            }),
            Some((report, &report_digest)),
        );
        // C 态提案化（alert-only 口径）：alert 级发现自动产提案——
        // 记录性动作，不改变本报告的认证/呈现语义（零拦截红线）
        if severity == "alert" {
            self.maybe_propose(version, &findings);
        }
        // 认证集有界插入（含 clean 标记）；淘汰者落链记录性事实（窗口语义可见）
        let evicted = lock_poem(&self.certified).insert(version, now_secs(), clean);
        if let Some((old, old_at)) = evicted {
            self.append_chain(
                "CompanionCertExpired",
                json!({ "version": old, "certified_at": old_at, "superseded_by": version }),
            );
        }
        *lock_poem(&self.last_report) = Some(json!({
            "report": report,
            "report_json_digest": report_digest,
            "chain_seq": seq_report,
            "severity": severity,
        }));
        lock_poem(&self.errors).clear();
        *lock_poem(&self.dirty_since) = None;
        *lock_poem(&self.last_analysis_done) = Some(std::time::Instant::now());
        *lock_poem(&self.phase) = Phase::Certified;
        self.analyses_ok.fetch_add(1, Ordering::Relaxed);
        self.record_metric_ok(started.elapsed());
        tracing::info!(
            ruleset_version = version,
            seq = seq_report,
            "companion report certified"
        );
    }

    /// 伴随域哈希链追加（blake3 前链；落盘 fail-soft——内存链恒为准）
    fn append_chain(&self, kind: &str, payload: serde_json::Value) -> u64 {
        self.append_chain_with_artifact(kind, payload, None)
    }

    /// 仅内存链追加（磁盘正坏时的降级事实载体：`ChainDegraded` 不再触盘，
    /// 事实在内存链与 status 呈现；内存链 hash 序自洽，重启后以磁盘重放为准）
    fn append_chain_mem_only(&self, kind: &str, payload: serde_json::Value) -> u64 {
        self.push_entry(kind, payload, None).seq
    }

    fn append_chain_with_artifact(
        &self,
        kind: &str,
        payload: serde_json::Value,
        artifact: Option<(&serde_json::Value, &String)>,
    ) -> u64 {
        let pushed = self.push_entry(kind, payload, artifact);
        // 报告全文落盘（设计稿：全文落 companion 本地报告目录，事实内持 digest 互证）
        if let (Some(dir), Some((full, _))) = (pushed.reports_dir.as_ref(), artifact) {
            let file = dir.join(format!("report_{:06}.json", pushed.seq));
            if let Some(parent) = file.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&file, serde_json::to_vec_pretty(full).unwrap_or_default());
        }
        // fail-soft 追加：磁盘故障不阻断伴跑（内存链+日志可见）；
        // 连续失败达阈值 → 降级可见（ChainDegraded 事实 + status 标志）
        if let (Some(path), Some(line)) = (pushed.path.as_ref(), pushed.line.as_ref()) {
            match append_line(path, line) {
                Err(e) => self.on_append_failure(pushed.seq, &e),
                Ok(()) => self.on_append_success(),
            }
        }
        pushed.seq
    }

    /// 落盘连续失败处置：达 [`CHAIN_DEGRADE_THRESHOLD`] → error 日志 +
    /// `chain_write_degraded=true` + `ChainDegraded` 降级事实（仅内存链——
    /// 磁盘正坏，事实在 status 呈现；一次成功追加即恢复）。
    fn on_append_failure(&self, seq: u64, e: &std::io::Error) {
        let streak = self.append_fail_streak.fetch_add(1, Ordering::Relaxed) + 1;
        if streak == CHAIN_DEGRADE_THRESHOLD {
            self.chain_write_degraded.store(true, Ordering::Relaxed);
            tracing::error!(
                seq, streak, error = %e,
                "companion chain append failed repeatedly (degraded; memory chain authoritative)"
            );
            self.append_chain_mem_only(
                "ChainDegraded",
                json!({
                    "threshold": CHAIN_DEGRADE_THRESHOLD,
                    "last_error": e.to_string(),
                }),
            );
        } else if streak < CHAIN_DEGRADE_THRESHOLD {
            tracing::warn!(seq, streak, error = %e, "companion chain append failed (memory chain authoritative)");
        } else {
            tracing::debug!(seq, streak, error = %e, "companion chain append failed (degraded)");
        }
    }

    /// 落盘成功：失败计数清零；此前处于降级态则恢复并留 info 日志。
    fn on_append_success(&self) {
        self.append_fail_streak.store(0, Ordering::Relaxed);
        if self.chain_write_degraded.swap(false, Ordering::Relaxed) {
            tracing::info!("companion chain append recovered (degradation cleared)");
        }
    }

    /// 内存链条目构造+入链（唯一写点：seq/prev_hash/mem 顺序推进）。
    /// 调用方负责磁盘写与降级处置（`append_chain_mem_only` 不触盘）。
    fn push_entry(
        &self,
        kind: &str,
        payload: serde_json::Value,
        artifact: Option<(&serde_json::Value, &String)>,
    ) -> PushedEntry {
        let mut chain = lock_poem(&self.chain);
        chain.seq += 1;
        let seq = chain.seq;
        let mut entry = json!({
            "seq": seq,
            "ts_secs": now_secs(),
            "kind": kind,
            "payload": payload,
            "prev_hash": chain.prev_hash,
        });
        if let Some((_, digest)) = artifact {
            entry["report_json_digest"] = json!(digest);
        }
        let hash = evorule_hash::json_digest(&entry);
        entry["hash"] = json!(hash);
        chain.prev_hash = hash;
        chain.mem.push(entry.clone());
        let cap = chain.mem.len().saturating_sub(CHAIN_MEM_CAP);
        if cap > 0 {
            chain.mem.drain(0..cap);
        }
        let line = match serde_json::to_string(&entry) {
            Ok(l) => Some(l),
            Err(e) => {
                tracing::warn!(seq, error = %e, "companion chain serialize failed (memory chain authoritative)");
                None
            }
        };
        PushedEntry {
            seq,
            line,
            path: chain.path.clone(),
            reports_dir: chain.reports_dir.clone(),
        }
    }

    /// 链完整性校验（逐条重算哈希+前链比对）。内存链是磁盘链的**尾窗**
    /// （超容量条目被修剪），首条 prev_hash 可能指向已修剪前驱——只校验
    /// 窗内连续性与逐条哈希；首条 prev 由启动重放全量校验兜底。
    pub fn verify_chain(&self) -> Result<u64, String> {
        let chain = lock_poem(&self.chain);
        let mut prev: Option<String> = None;
        for entry in &chain.mem {
            let hash = verify_entry(entry, prev.as_deref())?;
            prev = Some(hash);
        }
        Ok(chain.seq)
    }

    /// status 呈现（验收骨架「状态链可见」查询面）
    pub fn status_json(&self) -> serde_json::Value {
        // 锁序纪律：proposals 先于 chain（与 maybe_propose/adjudicate 的
        // proposals→chain 获取顺序一致，防交叉死锁）
        let proposals = lock_poem(&self.proposals)
            .iter()
            .map(|p| {
                let mut v = p.render();
                v["uplink"] = self.uplink_json_for(&p.proposal_id);
                v
            })
            .collect::<Vec<_>>();
        let uplinks: BTreeMap<String, serde_json::Value> = lock_poem(&self.uplink)
            .iter()
            .map(|(k, s)| (k.clone(), s.render()))
            .collect();
        let chain = lock_poem(&self.chain);
        let errors = lock_poem(&self.errors).clone();
        let last_report = lock_poem(&self.last_report).clone();
        // 容量透出：现役条数取最近报告实测值（None=尚无报告）；上限与双档
        // 阈值随治理常数派生（与 TCB MAX_TRANSFORM_RULES 同源，见 rule_schema）
        let rule_count = last_report
            .as_ref()
            .and_then(|r| r.get("report"))
            .and_then(|r| r.get("rule_count"))
            .and_then(|v| v.as_u64());
        let cap = evorule_rule_schema::MAX_TRANSFORM_RULES;
        // 均衡态呈现块：五元窗快照+最近窗间差分 rates+均衡布尔（评估任务
        // 窗结算后经规则层判定缓存；None=反应器不可达/读回超时，fail-visible
        // 不猜稳态）+最近归因+窗长。切换批（立法 v0.12.0）后 gate 接管主
        // 契约，影子双跑块随 equilibrium_check 规则退役同批退场。
        // 相位标签（crisis/relaxing/absorbing）由评估任务在窗结算原子点
        // 计算入视图（结算记录同源可回放，呈现层零二次派生）；窗警量与
        // 窗认证尾部环形缓存全量透出（旧→新序，τ 序列真机拉取源）。
        let eq_windows = crate::equilibrium_obs::shared().snapshot();
        let eq_view = crate::equilibrium_obs::view();
        let equilibrium = json!({
            "proposals_in_total": eq_windows.proposals_in_total,
            "digested_total": eq_windows.digested_total,
            "alert_free_windows": eq_windows.alert_free_windows,
            "certified_streak": eq_windows.certified_streak,
            "last_coverage": eq_windows.last_coverage,
            "in_rate": eq_view.in_rate,
            "digest_rate": eq_view.digest_rate,
            "stable": eq_view.stable,
            "last_cause": eq_view.last_cause,
            "window_secs": eq_view.window_secs,
            "phase": eq_view.phase,
            "window_alert_tail": crate::equilibrium_obs::shared()
                .window_alert_tail(crate::equilibrium_obs::WINDOW_ALERT_TAIL_CAP),
            "window_cert_tail": crate::equilibrium_obs::shared()
                .window_cert_tail(crate::equilibrium_obs::WINDOW_ALERT_TAIL_CAP),
        });
        json!({
            "enabled": self.enabled.load(Ordering::Relaxed),
            "state": lock_poem(&self.phase).as_str(),
            "latest_version": lock_poem(&self.latest_version).clone(),
            "dirty_since": lock_poem(&self.dirty_since).clone(),
            "last_error": errors.back().cloned(),
            "errors": errors.iter().cloned().collect::<Vec<_>>(),
            "last_report": last_report,
            "ruleset_capacity": {
                "rule_count": rule_count,
                "cap": cap,
                "warn_80": (cap * 80).div_ceil(100),
                "freeze_92": (cap * 92).div_ceil(100),
            },
            "certified_versions": lock_poem(&self.certified).versions(),
            "clean_versions": lock_poem(&self.certified).clean_versions(),
            "counters": {
                "analyses_started": self.analyses_started.load(Ordering::Relaxed),
                "analyses_ok": self.analyses_ok.load(Ordering::Relaxed),
                "analyses_failed": self.analyses_failed.load(Ordering::Relaxed),
                "stale_skipped": self.stale_skipped.load(Ordering::Relaxed),
                "stale_cancelled": self.stale_cancelled.load(Ordering::Relaxed),
            },
            "chain": {
                "seq": chain.seq,
                "kind": "companion-hash-chain-v1",
                "verified": self.chain_verified.load(Ordering::Relaxed),
                "write_degraded": self.chain_write_degraded.load(Ordering::Relaxed),
                "tail": chain.mem.iter().rev().take(8).cloned().collect::<Vec<_>>(),
            },
            "last_trigger": lock_poem(&self.last_trigger).to_string(),
            "proposals": proposals,
            "uplinks": uplinks,
            "law_sets": lock_poem(&self.law_sets).clone(),
            "equilibrium": equilibrium,
        })
    }

    /// C 态提案产出（幂等去重：`version + findings digest` 幂等键，同一
    /// 规则集版本的同族 findings 合并一条提案——重复 alert 不重复产提案，
    /// 同键已处置亦不重复产）。确定性 proposal_id 由幂等键派生；链落
    /// `CompanionProposal` 记录性事实。返回提案 ID（None=幂等跳过或元定律
    /// 拒收）。
    ///
    /// 元定律登记门（幂等去重之后、建提案之前）：findings 全表面命中
    /// `tiered_adjudicator::is_meta_rule_proposal`（均衡判据定律面）即拒收
    /// 出可提案集——不建提案、不推 proposals、不记均衡观测账，仅落链
    /// `CompanionProposalRejected` 留痕（cause=`meta_rule_denied`）后返回
    /// None。登记门有意**不去重**：同 findings 重复注入每次都落一条拒绝
    /// 记录（幂等键只约束正常提案产出，不约束拒绝留痕——恶意重放审计
    /// 可见）。
    pub(crate) fn maybe_propose(
        &self,
        version: &str,
        findings: &serde_json::Value,
    ) -> Option<String> {
        let findings_digest = evorule_hash::json_digest(findings);
        let proposal_id = format!("prop-{version}-{}", &findings_digest[..12]);
        let mut proposals = lock_poem(&self.proposals);
        if proposals.iter().any(|p| p.proposal_id == proposal_id) {
            return None;
        }
        // 元定律登记门：命中均衡判据定律面即拒收（只加不改：未命中路径
        // 与既有产出流程零差）
        if crate::tiered_adjudicator::is_meta_rule_proposal(findings) {
            self.append_chain(
                "CompanionProposalRejected",
                json!({
                    "proposal_id": &proposal_id,
                    "findings_digest": &findings_digest,
                    "cause": crate::tiered_adjudicator::REJECT_CAUSE_META_RULE,
                    "rejected_at": now_secs(),
                }),
            );
            tracing::info!(
                proposal_id = %proposal_id,
                cause = crate::tiered_adjudicator::REJECT_CAUSE_META_RULE,
                "companion proposal rejected at registry (meta-rule)"
            );
            return None;
        }
        let record = ProposalRecord {
            proposal_id: proposal_id.clone(),
            ruleset_version: version.to_string(),
            findings_digest: findings_digest.clone(),
            findings: findings.clone(),
            proposed_at: now_secs(),
            status: "proposed",
            adjudicated_at: None,
            actor: None,
        };
        let id = record.proposal_id.clone();
        self.append_chain(
            "CompanionProposal",
            json!({
                "proposal_id": &record.proposal_id,
                "ruleset_version": record.ruleset_version,
                "findings_digest": &record.findings_digest,
                "findings": &record.findings,
                "proposed_at": record.proposed_at,
                "status": record.status,
            }),
        );
        proposals.push(record);
        let cap = proposals.len().saturating_sub(PROPOSALS_CAP);
        if cap > 0 {
            proposals.drain(0..cap);
        }
        tracing::info!(
            proposal_id = %id,
            ruleset_version = version,
            "companion proposal created (alert findings)"
        );
        // 均衡观测记账（纯观测零拦截）：一件提案入管
        crate::equilibrium_obs::shared().record_proposal();
        Some(id)
    }

    /// 提案人工处置（approve/reject）：完成提案生命周期并落链
    /// `CompanionAdjudication` 裁决事实。零拦截红线：处置是记录性动作，
    /// 不改变伴跑分析/认证/报告行为。outcome 取 "approved"/"rejected"
    /// （由管理面端点映射，二值封闭）。错误：未找到 / 已处置（不重复裁决）。
    pub(crate) fn adjudicate(
        &self,
        proposal_id: &str,
        outcome: &'static str,
        actor: &str,
    ) -> Result<serde_json::Value, String> {
        let mut proposals = lock_poem(&self.proposals);
        let rec = proposals
            .iter_mut()
            .find(|p| p.proposal_id == proposal_id)
            .ok_or_else(|| format!("proposal not found: {proposal_id}"))?;
        if rec.status != "proposed" {
            return Err(format!(
                "proposal already adjudicated: {proposal_id} ({})",
                rec.status
            ));
        }
        rec.status = outcome;
        rec.adjudicated_at = Some(now_secs());
        rec.actor = Some(actor.to_string());
        let payload = json!({
            "proposal_id": proposal_id,
            "outcome": outcome,
            "adjudicated_at": rec.adjudicated_at,
            "actor": actor,
        });
        self.append_chain("CompanionAdjudication", payload);
        tracing::info!(
            proposal_id = %proposal_id,
            outcome,
            actor,
            "companion proposal adjudicated"
        );
        // 均衡观测记账（纯观测零拦截）：approved/rejected 均计消化
        // （消化=处置完成出管道，两态同权防 reject 风暴假性供过于求）
        crate::equilibrium_obs::shared().record_digested();
        Ok(rec.render())
    }

    /// 提案清单呈现（只读管理面；与 status 的 proposals 键同源；各条附
    /// uplink 字段——无上行记录=null）
    pub(crate) fn proposals_list(&self) -> Vec<serde_json::Value> {
        lock_poem(&self.proposals)
            .iter()
            .map(|p| {
                let mut v = p.render();
                v["uplink"] = self.uplink_json_for(&p.proposal_id);
                v
            })
            .collect()
    }

    /// 提案的 uplink 状态呈现值（无记录=null；锁序：proposals→uplink）
    fn uplink_json_for(&self, proposal_id: &str) -> serde_json::Value {
        lock_poem(&self.uplink)
            .get(proposal_id)
            .map(UplinkState::render)
            .unwrap_or(serde_json::Value::Null)
    }

    /// 提案治理链上行（D1=approve 后自动触发；D2=手动重试同入口）。
    /// 守卫序：已配置 → 提案在窗且 status=approved（仅 approved 可上行/
    /// 重试）→ 无 in-flight。置 pending 后 spawn 出站任务（fire-and-forget：
    /// 终态 ok/failed 写回状态面+落 `CompanionUplink` 链事实）。
    ///
    /// 零拦截红线：裁决在先已完成（CompanionAdjudication 已落链），本方法
    /// 任何失败路径只影响上行动作本身——不回滚裁决、不改分析/认证/呈现。
    /// 返回 pending 态信息（含幂等 entry_id）供管理面呈现。
    pub(crate) fn uplink_proposal(
        self: &Arc<Self>,
        proposal_id: &str,
    ) -> Result<serde_json::Value, UplinkError> {
        let cfg = lock_poem(&self.uplink_config)
            .clone()
            .ok_or(UplinkError::NotConfigured)?;
        let (entry, actor) = {
            let proposals = lock_poem(&self.proposals);
            let rec = proposals
                .iter()
                .find(|p| p.proposal_id == proposal_id)
                .ok_or(UplinkError::NotFound)?;
            if rec.status != "approved" {
                return Err(UplinkError::NotApproved(rec.status.to_string()));
            }
            let actor = rec.actor.clone().unwrap_or_else(|| "unknown".to_string());
            let entry = companion_uplink_entry(
                proposal_id,
                &rec.ruleset_version,
                &rec.findings_digest,
                &rec.findings,
            );
            (entry, actor)
        };
        {
            let mut uplinks = lock_poem(&self.uplink);
            if uplinks
                .get(proposal_id)
                .is_some_and(|s| s.state == "pending")
            {
                return Err(UplinkError::InFlight);
            }
            uplinks.insert(
                proposal_id.to_string(),
                UplinkState {
                    state: "pending",
                    attempted_at: now_secs(),
                    last_error: None,
                    http_status: None,
                },
            );
        }
        let entry_id = companion_uplink_entry_id(proposal_id);
        let body = companion_uplink_payload(&cfg, entry, proposal_id, &actor);
        let hub = Arc::clone(self);
        let pid = proposal_id.to_string();
        let chain_entry_id = entry_id.clone();
        tokio::spawn(async move {
            let outcome = post_propose(&cfg, &body).await;
            let (state, last_error, http_status): (&'static str, Option<String>, Option<u16>) =
                match &outcome {
                    Ok(code) if (200..300).contains(code) => ("ok", None, Some(*code)),
                    Ok(code) => (
                        "failed",
                        Some(format!("governance propose rejected with status {code}")),
                        Some(*code),
                    ),
                    Err(e) => ("failed", Some(e.clone()), None),
                };
            {
                let mut uplinks = lock_poem(&hub.uplink);
                if let Some(s) = uplinks.get_mut(&pid) {
                    s.state = state;
                    s.last_error = last_error.clone();
                    s.http_status = http_status;
                }
            }
            hub.append_chain(
                "CompanionUplink",
                json!({
                    "proposal_id": pid,
                    "entry_id": chain_entry_id,
                    "outcome": state,
                    "http_status": http_status,
                    "last_error": last_error,
                }),
            );
            if state == "ok" {
                tracing::info!(
                    proposal_id = %pid,
                    "companion proposal uplinked to governance (adjudication recorded)"
                );
            } else {
                tracing::warn!(
                    proposal_id = %pid,
                    error = ?last_error,
                    "companion proposal uplink failed (adjudication unaffected; retry via management endpoint)"
                );
            }
        });
        Ok(json!({
            "state": "pending",
            "entry_id": entry_id,
        }))
    }

    /// 上次成功分析的 lint 增量基线快照（None=尚无基线）
    pub(crate) fn lint_cache_snapshot(&self) -> Option<LintRwCache> {
        lock_poem(&self.lint_cache).clone()
    }

    /// 提交 lint 增量基线（成功分析收尾时调用——单表覆盖写，不随版本增长）
    pub(crate) fn commit_lint_cache(&self, cache: LintRwCache) {
        *lock_poem(&self.lint_cache) = Some(cache);
    }

    /// 覆盖率呈现（版本认证制；会话归属=创建时锚定版本∈认证集）
    ///
    /// `session_versions`：每会话的版本归属快照（session_id, 锚定版本），
    /// 由调用方从 SessionManager 分片快照枚举。逐会话统计：
    /// - `sessions_total` = 快照行数（含无版本归属会话）
    /// - `per_version` = 版本 → {sessions, certified}（无归属会话不入桶）
    /// - `sessions_covered` = 锚定版本∈认证集的会话数
    ///
    /// `anchor_certified` 二值口径保留（兼容旧消费方）。
    pub fn coverage_json(&self, session_versions: &[(u64, Option<String>)]) -> serde_json::Value {
        let certified = lock_poem(&self.certified);
        let anchor = lock_poem(&self.anchor_version).clone();
        let anchor_certified = anchor
            .as_ref()
            .map(|v| certified.contains(v))
            .unwrap_or(false);
        let sessions_total = session_versions.len();
        let mut per_version: BTreeMap<String, usize> = BTreeMap::new();
        let mut sessions_covered = 0usize;
        for (_, version) in session_versions {
            if let Some(v) = version {
                *per_version.entry(v.clone()).or_insert(0) += 1;
                if certified.contains(v) {
                    sessions_covered += 1;
                }
            }
        }
        let ratio = if sessions_total == 0 {
            None
        } else {
            Some(sessions_covered as f64 / sessions_total as f64)
        };
        if let Some(m) = lock_poem(&self.metrics).as_ref() {
            m.chain_seq.set(lock_poem(&self.chain).seq as i64);
            if let Some(r) = ratio {
                m.coverage_ratio_permille.set((r * 1000.0) as i64);
            }
        }
        // 均衡观测记账（纯观测零拦截）：最近覆盖率快照入窗（千分比）
        if let Some(r) = ratio {
            crate::equilibrium_obs::shared().record_coverage((r * 1000.0) as i64);
        }
        let per_version_json: BTreeMap<String, serde_json::Value> = per_version
            .into_iter()
            .map(|(v, n)| {
                let certified_flag = certified.contains(v.as_str());
                let clean_flag = certified_flag && certified.is_clean(v.as_str());
                (
                    v,
                    json!({
                        "sessions": n,
                        "certified": certified_flag,
                        "clean": clean_flag,
                    }),
                )
            })
            .collect();
        json!({
            "certified_versions": certified.versions(),
            "certified_count": certified.len(),
            "anchor_version": anchor,
            "anchor_certified": anchor_certified,
            "sessions_total": sessions_total,
            "sessions_covered": sessions_covered,
            "companion_coverage_ratio": ratio,
            "per_version": per_version_json,
        })
    }

    fn record_metric_ok(&self, d: Duration) {
        if let Some(m) = lock_poem(&self.metrics).as_ref() {
            m.analyses_total.with_label_values(&["ok"]).inc();
            m.analysis_duration.observe(d.as_secs_f64());
        }
    }

    fn record_metric_failed(&self, d: Duration) {
        if let Some(m) = lock_poem(&self.metrics).as_ref() {
            m.analyses_total.with_label_values(&["failed"]).inc();
            m.analysis_duration.observe(d.as_secs_f64());
        }
    }

    fn record_metric_stale(&self) {
        if let Some(m) = lock_poem(&self.metrics).as_ref() {
            m.analyses_total.with_label_values(&["stale"]).inc();
        }
    }
}

/// Prometheus 指标组（挂 server 既有 registry；硬边界：只读计数零控制面）
struct CompanionMetrics {
    analyses_total: prometheus::IntCounterVec,
    chain_seq: prometheus::IntGauge,
    coverage_ratio_permille: prometheus::IntGauge,
    analysis_duration: prometheus::Histogram,
}

impl CompanionMetrics {
    fn register(registry: &prometheus::Registry) -> Option<Self> {
        // 命名空间统一 evorule_companion（与 server 指标族对齐）
        let analyses_total = prometheus::IntCounterVec::new(
            prometheus::Opts::new(
                "analyses_total",
                "companion analysis outcomes (ok/failed/stale)",
            )
            .namespace("evorule_companion"),
            &["result"],
        )
        .ok()?;
        let chain_seq = prometheus::IntGauge::with_opts(
            prometheus::Opts::new("chain_seq", "current companion hash chain sequence")
                .namespace("evorule_companion"),
        )
        .ok()?;
        let coverage_ratio_permille = prometheus::IntGauge::with_opts(
            prometheus::Opts::new(
                "coverage_ratio_permille",
                "session coverage ratio in permille (integer, 1‰ resolution)",
            )
            .namespace("evorule_companion"),
        )
        .ok()?;
        let analysis_duration = prometheus::Histogram::with_opts(
            prometheus::HistogramOpts::new(
                "analysis_duration_seconds",
                "companion analysis wall time (baseline for the incremental batch)",
            )
            .namespace("evorule_companion"),
        )
        .ok()?;
        let metrics = Self {
            analyses_total,
            chain_seq,
            coverage_ratio_permille,
            analysis_duration,
        };
        let mut ok = true;
        if registry
            .register(Box::new(metrics.analyses_total.clone()))
            .is_err()
        {
            ok = false;
        }
        if registry
            .register(Box::new(metrics.chain_seq.clone()))
            .is_err()
        {
            ok = false;
        }
        if registry
            .register(Box::new(metrics.coverage_ratio_permille.clone()))
            .is_err()
        {
            ok = false;
        }
        if registry
            .register(Box::new(metrics.analysis_duration.clone()))
            .is_err()
        {
            ok = false;
        }
        if ok {
            Some(metrics)
        } else {
            None
        }
    }
}

/// 锁毒化恢复（server 侧既有口径：poisoned.into_inner()——数据本体可恢复）
fn lock_poem<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// JSONL 追加（新建父目录；追加失败上抛由调用方 fail-soft 处置）
fn append_line(path: &std::path::Path, line: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(line.as_bytes())?;
    f.write_all(b"\n")
}

/// 一次入链的产出（调用方据此做磁盘写/降级处置；`line=None`=序列化失败）
struct PushedEntry {
    seq: u64,
    line: Option<String>,
    path: Option<std::path::PathBuf>,
    reports_dir: Option<std::path::PathBuf>,
}

/// 单条链条目校验（hash 重算+前链比对），返回条目自身 hash。
/// `expected_prev=None`：跳过前链比对（内存尾窗首条——前驱可能已修剪；
/// 全链首条的空 prev 校验由启动重放从严兜底）。
fn verify_entry(entry: &serde_json::Value, expected_prev: Option<&str>) -> Result<String, String> {
    let seq = entry
        .get("seq")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| "chain entry missing seq".to_string())?;
    let mut bare = entry.clone();
    let hash = bare
        .get("hash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("chain entry {seq} missing hash"))?
        .to_string();
    bare.as_object_mut()
        .ok_or_else(|| "chain entry not object".to_string())?
        .remove("hash");
    let recomputed = evorule_hash::json_digest(&bare);
    if recomputed != hash {
        return Err(format!("chain entry {seq} hash mismatch"));
    }
    if let Some(expected) = expected_prev {
        let entry_prev = entry
            .get("prev_hash")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if entry_prev != expected {
            return Err(format!("chain entry {seq} prev_hash mismatch"));
        }
    }
    Ok(hash)
}

/// 启动重放结果：合法链 → seq/prev_hash/内存尾窗延续；坏链 → 隔离重建。
struct ChainReplay {
    seq: u64,
    prev_hash: String,
    mem: Vec<serde_json::Value>,
    verified: bool,
    alert: Option<serde_json::Value>,
}

/// 启动期链全量重放校验（D3 隔离重建案）：
/// 逐行解析+哈希重算+前链比对（从严：首条 prev 必须为空）。
/// 全部通过 → 延续 seq/prev_hash/尾窗；自首个坏行（解析失败/哈希不符/前链
/// 断裂/写一半截断）起判坏 → 坏文件改名 `.corrupt-<unix_ts>` 原样留档
/// （证据保全），新链自空起步（首条由调用方落 `ChainIntegrityAlert`），
/// `verified=false`——绝不静默重建。文件不存在 = 空链新起点（现行为）。
fn replay_chain_from_disk(path: &std::path::Path) -> ChainReplay {
    let mut ok = ChainReplay {
        seq: 0,
        prev_hash: String::new(),
        mem: Vec::new(),
        verified: true,
        alert: None,
    };
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        // 文件不存在 = 空链新起点（现行为不变；非坏链不隔离不告警）
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ok,
        Err(e) => {
            // 不可读视同坏链：同样隔离留档（重命名尽力而为，失败留 error）
            quarantine_chain_file(path);
            return ChainReplay {
                seq: 0,
                prev_hash: String::new(),
                mem: Vec::new(),
                verified: false,
                alert: Some(json!({ "detail": format!("chain file unreadable: {e}") })),
            };
        }
    };
    let mut prev = String::new();
    let mut mem: Vec<serde_json::Value> = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let checked = (|| -> Result<serde_json::Value, String> {
            let entry: serde_json::Value =
                serde_json::from_str(line).map_err(|e| format!("parse failed: {e}"))?;
            verify_entry(&entry, Some(&prev))?;
            Ok(entry)
        })();
        match checked {
            Ok(entry) => {
                prev = entry
                    .get("hash")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                ok.seq = entry.get("seq").and_then(|v| v.as_u64()).unwrap_or(0);
                mem.push(entry);
                let cap = mem.len().saturating_sub(CHAIN_MEM_CAP);
                if cap > 0 {
                    mem.drain(0..cap);
                }
            }
            Err(reason) => {
                // 判坏即隔离：seq 断行号即证据，全量入 alert 明细
                let detail = format!(
                    "chain corrupted at line {} (last valid seq {}): {reason}",
                    idx + 1,
                    mem.last()
                        .and_then(|e| e.get("seq"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0)
                );
                quarantine_chain_file(path);
                return ChainReplay {
                    seq: 0,
                    prev_hash: String::new(),
                    mem: Vec::new(),
                    verified: false,
                    alert: Some(json!({ "detail": detail })),
                };
            }
        }
    }
    ok.prev_hash = prev;
    ok.mem = mem;
    ok
}

/// 坏链文件隔离留档：改名 `<原名>.corrupt-<unix_ts>`（同名冲突尽力追加时戳
/// 二次区分）；改名失败留 error 日志（文件保留原位，重放结论仍为判坏）。
fn quarantine_chain_file(path: &std::path::Path) {
    let ts = now_secs();
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "companion_chain.jsonl".to_string());
    let target = path.with_file_name(format!("{file_name}.corrupt-{ts}"));
    match std::fs::rename(path, &target) {
        Ok(()) => tracing::error!(
            quarantined_to = %target.display(),
            "companion chain file corrupted; quarantined (rebuild with integrity alert)"
        ),
        Err(e) => tracing::error!(
            path = %path.display(), error = %e,
            "companion chain file corrupted; quarantine rename failed (rebuild still applies)"
        ),
    }
}

/// 单路径 watch 建立（创建失败/watch 失败均 warn 并返回 None——watcher 面
/// fail-soft：编辑检出不可用不阻断伴跑，漏斗其余触发源照常）
fn watch_path(
    tx: std::sync::mpsc::Sender<notify::Result<notify::Event>>,
    path: &std::path::Path,
    label: &str,
) -> Option<notify::RecommendedWatcher> {
    use notify::{RecursiveMode, Watcher};
    match notify::recommended_watcher(tx) {
        Ok(mut w) => match w.watch(path, RecursiveMode::NonRecursive) {
            Ok(()) => Some(w),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "companion watcher: {label} watch failed"
                );
                None
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "companion watcher: create {label} watcher failed");
            None
        }
    }
}

/// 编辑检出 watcher（D1 推荐案）：非递归 watch `rules_dir` 与 `core_eval`
/// 所在目录（按路径过滤 core_eval 变更——原子替换语义下单文件 watch 不可靠）。
/// 事件批排空后单次只读求版（`load_merged_with_layout`），求版成功经
/// `notify_version_source(source="watch")` 进既有漏斗——**无第二去抖状态**，
/// 唯一静默窗=runner 的 `config.debounce`；同版本信号幂等天然去重。
/// 求版失败（写入中间态/非法 JSON）fail-soft 跳过，最终一致的合法写入会再触发。
/// watcher 句柄随事件线程闭包保活至进程退出（master 接收端纪律：无接收端
/// 即丢事件）。常开（信号幂等廉价；A 态待命零活动语义不受影响）。
pub fn spawn_fs_watcher(
    hub: Arc<CompanionHub>,
    core_eval_path: std::path::PathBuf,
    rules_dir: std::path::PathBuf,
) {
    let (tx, rx) = std::sync::mpsc::channel::<notify::Result<notify::Event>>();
    let mut watchers: Vec<notify::RecommendedWatcher> = Vec::new();
    if let Some(w) = watch_path(tx.clone(), &rules_dir, "rules_dir") {
        watchers.push(w);
    }
    if let Some(parent) = core_eval_path.parent().map(|p| p.to_path_buf()) {
        if let Some(w) = watch_path(tx, &parent, "core_eval parent") {
            watchers.push(w);
        }
    }
    if watchers.is_empty() {
        tracing::warn!("companion watcher: no watch established (edit detection inactive)");
        return;
    }
    let spawn = std::thread::Builder::new()
        .name("companion-watch".to_string())
        .spawn(move || {
            // watcher 句柄随闭包保活：drop 即停 watch（master 接收端纪律）
            let _keepalive = watchers;
            loop {
                // 阻塞等首事件；全部 send 端释放（watcher 句柄 drop）→ 线程退出
                match rx.recv() {
                    Ok(_) => {}
                    Err(_) => return,
                }
                // 批排空（无计时器无第二静默窗；同版本信号幂等兜底）
                while rx.try_recv().is_ok() {}
                match SessionApi::load_merged_with_layout(&core_eval_path, &rules_dir) {
                    Ok((_, layout, _, _, _)) => {
                        hub.notify_version_source(layout.ruleset_version, TRIGGER_WATCH);
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "companion watcher: skipped unparsable intermediate state");
                    }
                }
            }
        });
    if let Err(e) = spawn {
        tracing::warn!(error = %e, "companion watcher: event thread spawn failed (edit detection inactive)");
    }
}

/// 链文件独占锁句柄（持句柄至进程退出；Drop 无操作——OS 自动释放）
pub struct ChainLock {
    _file: std::fs::File,
}

impl ChainLock {
    /// 链文件独占锁（多实例 fail-fast）：`companion_chain.lock` 同目录创建。
    /// create_new 成功=本实例持锁；失败=读既有文件核对 PID：
    /// 持有进程仍存活→拒启（双实例并发追加会互踩，fail-safe 优先）；
    /// 死 PID（崩溃残留）→覆盖接管。
    pub fn acquire(chain_path: &std::path::Path) -> Result<Self, String> {
        let lock_path = chain_path.with_extension("lock");
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(f) => {
                let mut f = f;
                use std::io::Write;
                let _ = f.write_all(std::process::id().to_string().as_bytes());
                Ok(Self { _file: f })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing = std::fs::read_to_string(&lock_path)
                    .map_err(|e| format!("companion chain lock unreadable: {e}"))?;
                let pid: u32 = existing
                    .trim()
                    .parse()
                    .map_err(|_| format!("companion chain lock corrupt: {existing:?}"))?;
                if pid_alive(pid) {
                    Err(format!(
                        "companion chain is held by live process {pid}; refuse to start \
                         (delete the stale lock file only if you are sure no other instance runs)"
                    ))
                } else {
                    // 崩溃残留：覆盖接管
                    std::fs::write(&lock_path, std::process::id().to_string())
                        .map_err(|e| format!("companion chain lock takeover failed: {e}"))?;
                    let f = std::fs::OpenOptions::new()
                        .write(true)
                        .open(&lock_path)
                        .map_err(|e| format!("companion chain lock reopen failed: {e}"))?;
                    tracing::warn!(pid, "companion chain lock takeover from dead holder");
                    Ok(Self { _file: f })
                }
            }
            Err(e) => Err(format!("companion chain lock create failed: {e}")),
        }
    }
}

/// PID 探活（跨平台：unix 读 /proc；windows 走 tasklist）
fn pid_alive(pid: u32) -> bool {
    if pid == std::process::id() {
        return true;
    }
    #[cfg(unix)]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(windows)]
    {
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
            .map(|o| {
                let s = String::from_utf8_lossy(&o.stdout);
                // 命中行形如 "name","pid",...；无匹配时 tasklist 输出 INFO 行
                s.contains(&format!("\"{pid}\""))
            })
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_equilibrium_block_phase_and_window_tails() {
        let hub = CompanionHub::new(CompanionConfig::for_tests(), None, None);
        let eq = hub.status_json()["equilibrium"].clone();
        // 相位标签在词表内（进程级共享视图的当前值随演练面记账演进，不预设）
        let phase = eq["phase"].as_str().expect("phase 应为字符串");
        assert!(
            matches!(phase, "crisis" | "relaxing" | "absorbing"),
            "phase 应在词表内，got {phase}"
        );
        // 窗警量/窗认证尾部：环形容量上限内、元素全为非负整数
        for key in ["window_alert_tail", "window_cert_tail"] {
            let arr = eq[key]
                .as_array()
                .unwrap_or_else(|| panic!("{key} 应为数组"));
            assert!(
                arr.len() <= crate::equilibrium_obs::WINDOW_ALERT_TAIL_CAP,
                "{key} 不超环形容量，got {}",
                arr.len()
            );
            assert!(
                arr.iter().all(serde_json::Value::is_u64),
                "{key} 元素全为非负整数"
            );
        }
        // 既有十字段仍在（契约回归：只加不改）
        for key in [
            "proposals_in_total",
            "digested_total",
            "alert_free_windows",
            "certified_streak",
            "last_coverage",
            "in_rate",
            "digest_rate",
            "stable",
            "last_cause",
            "window_secs",
        ] {
            assert!(eq.get(key).is_some(), "既有字段 {key} 仍在");
        }
    }

    #[test]
    fn certified_set_bounded_anchor_never_evicted() {
        let mut set = CertifiedSet::new(Some("anchor".to_string()));
        set.insert("anchor", 1, false);
        let mut evicted_any = false;
        for i in 0..14 {
            let v = format!("v{i}");
            if let Some(old) = set.insert(&v, 2, true) {
                assert_ne!(old.0, "anchor", "锚永不淘汰");
                evicted_any = true;
            }
        }
        assert!(evicted_any, "超限后发生淘汰");
        assert!(
            set.len() <= CERTIFIED_MAX_EXTRA + 1,
            "认证集有界（锚+{}），实际 {}",
            CERTIFIED_MAX_EXTRA,
            set.len()
        );
        assert!(set.contains("anchor"), "锚仍在认证集");
    }

    #[test]
    fn certified_set_bounded_without_anchor() {
        let mut set = CertifiedSet::new(None);
        for i in 0..12 {
            set.insert(&format!("v{i}"), 1, true);
        }
        assert_eq!(set.len(), CERTIFIED_MAX_EXTRA, "无锚认证集硬上限=最近 N 条");
    }

    /// 两级证书契约①：clean 是认证条目属性，clean 集 ⊆ certified 集（单窗口
    /// 结构保证，无平行集合）；未认证/窗口外/淘汰版本 clean 恒 false
    #[test]
    fn clean_is_subset_of_certified_single_window() {
        let mut set = CertifiedSet::new(None);
        set.insert("clean-v", 1, true);
        set.insert("dirty-v", 2, false);
        assert!(set.contains("clean-v") && set.is_clean("clean-v"));
        assert!(set.contains("dirty-v") && !set.is_clean("dirty-v"));
        assert!(!set.is_clean("never-certified"));
        // 淘汰后 clean/certified 同生共死（单条目删除，无残留 clean 标记）
        for i in 0..(CERTIFIED_MAX_EXTRA + 2) {
            set.insert(&format!("w{i}"), 3, true);
        }
        assert!(
            !set.contains("clean-v") && !set.is_clean("clean-v"),
            "淘汰版本 certified 与 clean 一并消失"
        );
        let clean = set.clean_versions();
        assert!(
            clean.iter().all(|v| set.contains(v)),
            "clean_versions ⊆ certified_versions"
        );
    }

    /// 覆盖呈现 per_version：逐会话版本归属统计（认证/clean 双维度），
    /// 无归属会话计入 total 但不入桶
    #[test]
    fn coverage_per_version_counts_sessions_and_flags() {
        let hub = CompanionHub::new(CompanionConfig::for_tests(), None, Some("vA".to_string()));
        let started = std::time::Instant::now();
        let lint_report = |analyses: serde_json::Value| {
            serde_json::json!({
                "kind": "CompanionReport",
                "ruleset_version": "v",
                "rule_count": 3,
                "constitution_excluded_count": 0,
                "analyses": analyses,
            })
        };
        let clean = lint_report(serde_json::json!([
            {"service": "inference_conflict_detect", "ok": true,
             "result": {"has_conflicts": false, "conflict_count": 0}},
            {"service": "inference_cycle_detect", "ok": true,
             "result": {"has_cycles": false, "cycle_count": 0}},
            {"service": "inference_rw_extract", "ok": true, "result": {"results": []}},
        ]));
        let dirty = lint_report(serde_json::json!([
            {"service": "inference_conflict_detect", "ok": true,
             "result": {"has_conflicts": true, "conflict_count": 1, "conflicts": []}},
            {"service": "inference_cycle_detect", "ok": true,
             "result": {"has_cycles": false, "cycle_count": 0}},
            {"service": "inference_rw_extract", "ok": true, "result": {"results": []}},
        ]));
        hub.certify_success("vA", &clean, started);
        hub.certify_success("vB", &dirty, started);

        let snap = vec![
            (1u64, Some("vA".to_string())),
            (2u64, Some("vA".to_string())),
            (3u64, Some("vB".to_string())),
            (4u64, None),
        ];
        let cov = hub.coverage_json(&snap);
        assert_eq!(cov["sessions_total"], json!(4), "总数含无归属会话");
        assert_eq!(cov["sessions_covered"], json!(3), "覆盖=锚定版本∈认证集");
        let pv = &cov["per_version"];
        assert_eq!(pv["vA"]["sessions"], json!(2));
        assert_eq!(pv["vA"]["certified"], json!(true));
        assert_eq!(pv["vA"]["clean"], json!(true), "vA 全净→clean");
        assert_eq!(pv["vB"]["sessions"], json!(1));
        assert_eq!(pv["vB"]["certified"], json!(true));
        assert_eq!(pv["vB"]["clean"], json!(false), "vB 有发现→不 clean");
        assert!(pv.get("unknown-version").is_none(), "无归属会话不入桶");
        // 认证窗口外版本：certified=false、clean=false（窗口语义）
        let cov2 = hub.coverage_json(&[(9u64, Some("expired".to_string()))]);
        assert_eq!(cov2["per_version"]["expired"]["certified"], json!(false));
        assert_eq!(cov2["per_version"]["expired"]["clean"], json!(false));
        assert_eq!(cov2["sessions_covered"], json!(0));
    }
    /// 两级证书契约②：severity 判定（certify_success 输入形态）——
    /// 有 conflict 发现→alert 不误报 clean；有 cycle 发现→warn；
    /// rw excluded 发现不阻断 clean；全净→clean
    #[test]
    fn severity_mapping_conflict_alert_cycle_warn_clean() {
        let hub = CompanionHub::new(CompanionConfig::for_tests(), None, None);
        let report = |analyses: serde_json::Value| {
            serde_json::json!({
                "kind": "CompanionReport",
                "ruleset_version": "v",
                "rule_count": 3,
                "constitution_excluded_count": 0,
                "analyses": analyses,
            })
        };
        let chain_payload = |seq: u64| -> serde_json::Value {
            let chain = lock_poem(&hub.chain);
            chain
                .mem
                .iter()
                .find(|e| e["seq"] == json!(seq))
                .cloned()
                .unwrap_or(serde_json::Value::Null)
        };
        // 带矛盾：conflict has_conflicts=true → alert 且不误报 clean
        let started = std::time::Instant::now();
        let with_conflict = report(serde_json::json!([
            {"service": "inference_conflict_detect", "ok": true,
             "result": {"has_conflicts": true, "conflict_count": 2, "conflicts": []}},
            {"service": "inference_cycle_detect", "ok": true,
             "result": {"has_cycles": false, "cycle_count": 0}},
            {"service": "inference_rw_extract", "ok": true,
             "result": {"results": []}},
        ]));
        hub.certify_success("v-conflict", &with_conflict, started);
        // alert 级收尾额外落 CompanionProposal（C 态提案化）——链尾不再是
        // 报告条目，报告按 kind 定位；提案事实紧跟其后
        let tail_kind = lock_poem(&hub.chain).mem.last().map(|e| e["kind"].clone());
        assert_eq!(
            tail_kind,
            Some(json!("CompanionProposal")),
            "alert 级发现自动产提案（链尾）"
        );
        let entry = lock_poem(&hub.chain)
            .mem
            .iter()
            .rev()
            .find(|e| e["kind"] == "CompanionReport")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        assert_eq!(entry["payload"]["severity"], json!("alert"));
        assert_eq!(entry["payload"]["findings"]["conflicts"], json!(2));
        assert!(
            !lock_poem(&hub.certified).is_clean("v-conflict"),
            "有发现不误报 clean"
        );
        // 带环路：cycle has_cycles=true → warn
        let with_cycle = report(serde_json::json!([
            {"service": "inference_conflict_detect", "ok": true,
             "result": {"has_conflicts": false, "conflict_count": 0}},
            {"service": "inference_cycle_detect", "ok": true,
             "result": {"has_cycles": true, "cycle_count": 1}},
            {"service": "inference_rw_extract", "ok": true,
             "result": {"results": []}},
        ]));
        hub.certify_success("v-cycle", &with_cycle, started);
        let seq_after = lock_poem(&hub.chain).seq;
        let entry = chain_payload(seq_after);
        assert_eq!(entry["payload"]["severity"], json!("warn"));
        assert!(!lock_poem(&hub.certified).is_clean("v-cycle"));
        // 全净（含 rw excluded 发现）→ clean（rw 弱化语义不阻断）
        let clean_with_rw = report(serde_json::json!([
            {"service": "inference_conflict_detect", "ok": true,
             "result": {"has_conflicts": false, "conflict_count": 0}},
            {"service": "inference_cycle_detect", "ok": true,
             "result": {"has_cycles": false, "cycle_count": 0}},
            {"service": "inference_rw_extract", "ok": true,
             "result": {"results": [{"id": "r1", "excluded": true}, {"id": "r2", "excluded": false}]}},
        ]));
        hub.certify_success("v-clean", &clean_with_rw, started);
        let seq_after = lock_poem(&hub.chain).seq;
        let entry = chain_payload(seq_after);
        assert_eq!(entry["payload"]["severity"], json!("clean"));
        assert_eq!(entry["payload"]["findings"]["rw_excluded"], json!(1));
        // 服务失败 → 不确定性下限 warn（阻断 clean）
        let with_failure = report(serde_json::json!([
            {"service": "inference_conflict_detect", "ok": false, "error": "boom"},
        ]));
        hub.certify_success("v-failed", &with_failure, started);
        let seq_after = lock_poem(&hub.chain).seq;
        let entry = chain_payload(seq_after);
        assert_eq!(entry["payload"]["severity"], json!("warn"));
        // clean_versions ⊆ certified_versions 且逐项一致
        let status = hub.status_json();
        let certified: Vec<String> = status["certified_versions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        for v in status["clean_versions"].as_array().unwrap() {
            assert!(certified.contains(&v.as_str().unwrap().to_string()));
        }
        assert_eq!(
            status["clean_versions"],
            json!(["v-clean"]),
            "仅全净版本入 clean 集"
        );
    }

    #[test]
    fn notify_version_cancels_inflight_and_counts() {
        let hub = CompanionHub::new(CompanionConfig::for_tests(), None, None);
        // 无 in-flight：cancel 计数不动
        hub.notify_version("v1".to_string());
        assert_eq!(hub.stale_cancelled.load(Ordering::Relaxed), 0);
        // 塞入 in-flight token（模拟分析进行中）→ 新版本到达即取消+计数
        let token = CancellationToken::new();
        *lock_poem(&hub.cancel_token) = Some(token.clone());
        hub.notify_version("v2".to_string());
        assert!(token.is_cancelled(), "新版本到达取消 in-flight 分析");
        assert_eq!(hub.stale_cancelled.load(Ordering::Relaxed), 1);
        // 同版本幂等信号不再重复取消
        hub.notify_version("v2".to_string());
        assert_eq!(hub.stale_cancelled.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn chain_lock_acquire_rejects_live_holder() {
        let dir = std::env::temp_dir().join(format!(
            "companion-lock-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let chain = dir.join("companion_chain.jsonl");
        std::fs::create_dir_all(&dir).unwrap();
        // 首次获取成功
        let _guard = ChainLock::acquire(&chain).unwrap_or_else(|e| panic!("首次持锁失败: {e}"));
        // 第二实例：持锁者=本进程（活）→ 拒启
        let err = match ChainLock::acquire(&chain) {
            Ok(_) => panic!("活持锁者应拒启"),
            Err(e) => e,
        };
        assert!(err.contains("held by live process"), "实际: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chain_lock_takeover_from_dead_pid() {
        let dir = std::env::temp_dir().join(format!(
            "companion-lock-dead-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let chain = dir.join("companion_chain.jsonl");
        std::fs::create_dir_all(&dir).unwrap();
        // 预置死 PID 残留锁（PID 取远超常规范围；探活失败即死）
        let dead_pid = 4_000_000_000u32;
        let lock_path = chain.with_extension("lock");
        std::fs::write(&lock_path, dead_pid.to_string()).unwrap();
        assert!(!pid_alive(dead_pid), "测试前置：{dead_pid} 应判死");
        let guard = ChainLock::acquire(&chain).unwrap_or_else(|e| panic!("死 PID 应接管: {e}"));
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 测试用唯一临时目录（纳秒时戳防并发撞名）
    fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "companion-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn replay_continues_seq_and_prev_hash_across_restart() {
        let dir = unique_temp_dir("replay-ok");
        let chain = dir.join("companion_chain.jsonl");
        // 第一生命周期：两笔事实落盘
        {
            let hub = CompanionHub::new(CompanionConfig::for_tests(), Some(chain.clone()), None);
            hub.append_chain("CompanionStateChange", json!({ "enabled": true }));
            hub.append_chain("CompanionStateChange", json!({ "enabled": false }));
        }
        // 第二生命周期：重放校验全过，seq/prev_hash 延续（重启断链缺陷修复面）
        let hub2 = CompanionHub::new(CompanionConfig::for_tests(), Some(chain.clone()), None);
        assert!(hub2.chain_verified.load(Ordering::Relaxed), "重放校验通过");
        let st = hub2.status_json();
        assert_eq!(st["chain"]["seq"], serde_json::json!(2), "seq 延续");
        assert_eq!(st["chain"]["verified"], serde_json::json!(true));
        // 重放后继续追加：前链无缝（第三笔 prev=第二笔 hash）
        hub2.append_chain("CompanionStateChange", json!({ "enabled": true }));
        assert!(hub2.verify_chain().is_ok(), "重放后追加链仍完整");
        let tail = hub2.status_json()["chain"]["tail"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(
            tail.first().map(|e| e["seq"].clone()),
            Some(serde_json::json!(3)),
            "status 尾窗倒序（最新在前）：重放后追加 seq 延续"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replay_quarantines_corrupted_chain_with_alert() {
        let dir = unique_temp_dir("replay-corrupt");
        let chain = dir.join("companion_chain.jsonl");
        {
            let hub = CompanionHub::new(CompanionConfig::for_tests(), Some(chain.clone()), None);
            hub.append_chain("CompanionStateChange", json!({ "enabled": true }));
        }
        // 注入坏行（写一半崩溃等价形态：截断 JSON）
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&chain)
            .unwrap();
        f.write_all(b"{\"seq\":2,\"kind\":\"broken").unwrap();
        drop(f);
        // 重启：判坏→隔离重建（非静默：error 日志+ChainIntegrityAlert 首条+verified=false）
        let hub2 = CompanionHub::new(CompanionConfig::for_tests(), Some(chain.clone()), None);
        assert!(
            !hub2.chain_verified.load(Ordering::Relaxed),
            "坏链重放 verified=false"
        );
        let st = hub2.status_json();
        assert_eq!(st["chain"]["verified"], serde_json::json!(false));
        assert_eq!(
            st["chain"]["seq"],
            serde_json::json!(1),
            "新链自告警首条起步"
        );
        let tail = st["chain"]["tail"].as_array().unwrap();
        assert!(
            tail.iter().any(|e| e["kind"] == "ChainIntegrityAlert"
                && e["payload"]["detail"]
                    .as_str()
                    .unwrap_or("")
                    .contains("line 2")),
            "告警携坏行定位: {tail:?}"
        );
        // 坏文件原样留档（证据保全）：.corrupt-* 恰一份且含判坏前合法条目
        // （原路径由告警首条追加重建——非静默重建的落链面）
        let quarantined: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".corrupt-"))
            .collect();
        assert_eq!(quarantined.len(), 1, "隔离留档恰一份: {quarantined:?}");
        let preserved = std::fs::read_to_string(dir.join(&quarantined[0])).unwrap_or_default();
        assert!(
            preserved.contains("CompanionStateChange"),
            "留档保留判坏前全部证据"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_degrade_visible_after_repeated_disk_failures() {
        let dir = unique_temp_dir("append-degrade");
        // 链文件置只读：append 打开即 ACCESS_DENIED（磁盘故障可靠替身；
        // 目录替身在 Windows 上 FILE_APPEND_DATA 语义不可靠）
        let chain = dir.join("chain.jsonl");
        std::fs::write(&chain, "").unwrap();
        let mut perms = std::fs::metadata(&chain).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&chain, perms).unwrap();
        let hub = CompanionHub::new(CompanionConfig::for_tests(), Some(chain), None);
        let st0 = hub.status_json();
        assert_eq!(st0["chain"]["write_degraded"], serde_json::json!(false));
        for _ in 0..CHAIN_DEGRADE_THRESHOLD {
            hub.append_chain("CompanionStateChange", json!({ "n": 1 }));
        }
        let st = hub.status_json();
        assert_eq!(
            st["chain"]["write_degraded"],
            serde_json::json!(true),
            "连续 {} 次落盘失败即降级可见",
            CHAIN_DEGRADE_THRESHOLD
        );
        let tail = st["chain"]["tail"].as_array().unwrap();
        assert!(
            tail.iter().any(|e| e["kind"] == "ChainDegraded"),
            "降级事实入内存链: {tail:?}"
        );
        assert!(hub.verify_chain().is_ok(), "降级事实内存链 hash 序自洽");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mem_window_verify_ok_after_pruning() {
        // 尾窗首条 prev 指向已修剪前驱——verify_chain 跳过首条前链比对
        // （全链首条空 prev 校验由启动重放从严兜底）
        let hub = CompanionHub::new(CompanionConfig::for_tests(), None, None);
        for i in 0..(CHAIN_MEM_CAP + 10) {
            hub.append_chain("CompanionStateChange", json!({ "i": i }));
        }
        let st = hub.status_json();
        assert_eq!(st["chain"]["seq"], serde_json::json!(CHAIN_MEM_CAP + 10));
        assert_eq!(
            st["chain"]["tail"].as_array().unwrap().len(),
            8,
            "status 尾窗呈现帽=8"
        );
        assert!(hub.verify_chain().is_ok(), "修剪后尾窗校验仍通过");
    }

    #[test]
    fn trigger_source_visible_in_status() {
        let hub = CompanionHub::new(CompanionConfig::for_tests(), None, None);
        assert_eq!(
            hub.status_json()["last_trigger"],
            serde_json::json!(TRIGGER_STARTUP),
            "缺省源=启动期"
        );
        hub.notify_version_source("v1".to_string(), TRIGGER_WATCH);
        assert_eq!(
            hub.status_json()["last_trigger"],
            serde_json::json!(TRIGGER_WATCH)
        );
        // 同版本幂等：重复信号不改源标注
        hub.notify_version_source("v1".to_string(), TRIGGER_RELOAD);
        assert_eq!(
            hub.status_json()["last_trigger"],
            serde_json::json!(TRIGGER_WATCH),
            "同版本早退不改 last_trigger"
        );
    }

    /// alert 报告夹具（conflict 有发现 → severity=alert）
    fn alert_report() -> serde_json::Value {
        serde_json::json!({
            "kind": "CompanionReport",
            "ruleset_version": "v",
            "rule_count": 3,
            "constitution_excluded_count": 0,
            "analyses": [
                {"service": "inference_conflict_detect", "ok": true,
                 "result": {"has_conflicts": true, "conflict_count": 2, "conflicts": []}},
                {"service": "inference_cycle_detect", "ok": true,
                 "result": {"has_cycles": false, "cycle_count": 0}},
                {"service": "inference_rw_extract", "ok": true, "result": {"results": []}},
            ],
        })
    }

    /// C 态提案契约：alert 级发现自动产提案——链落 CompanionProposal
    /// 记录性事实（payload 齐）+ 内存呈现窗一条；同 version 同族 findings
    /// 重复 alert 幂等不重复产
    #[test]
    fn proposal_alert_creates_chain_fact_and_dedups() {
        let hub = CompanionHub::new(CompanionConfig::for_tests(), None, None);
        let started = std::time::Instant::now();
        hub.certify_success("vAlert", &alert_report(), started);

        let chain_entries = |hub: &CompanionHub| -> Vec<serde_json::Value> {
            lock_poem(&hub.chain)
                .mem
                .iter()
                .filter(|e| e["kind"] == "CompanionProposal")
                .cloned()
                .collect()
        };
        let entries = chain_entries(&hub);
        assert_eq!(entries.len(), 1, "alert 产恰一条提案事实");
        let payload = &entries[0]["payload"];
        assert!(
            payload["proposal_id"]
                .as_str()
                .unwrap_or("")
                .starts_with("prop-vAlert-"),
            "确定性 ID 由幂等键派生"
        );
        assert_eq!(payload["ruleset_version"], serde_json::json!("vAlert"));
        assert!(
            !payload["findings_digest"].as_str().unwrap_or("").is_empty(),
            "findings digest 在场"
        );
        assert_eq!(payload["findings"]["conflicts"], serde_json::json!(2));
        assert_eq!(payload["status"], serde_json::json!("proposed"));
        assert!(payload["proposed_at"].as_u64().unwrap_or(0) > 0);
        // status 呈现（与链同源）
        let st = hub.status_json();
        assert_eq!(st["proposals"].as_array().unwrap().len(), 1);
        assert_eq!(
            st["proposals"][0]["proposal_id"], payload["proposal_id"],
            "status 与链事实同源"
        );
        // 幂等：同 version 同族 findings 重复 alert 不重复产
        hub.certify_success("vAlert", &alert_report(), started);
        assert_eq!(
            chain_entries(&hub).len(),
            1,
            "重复 alert 幂等去重（version+findings digest 同键）"
        );
        assert_eq!(hub.status_json()["proposals"].as_array().unwrap().len(), 1);
        // 同 version 不同 findings（不同族）→ 新提案
        let mut other = alert_report();
        other["analyses"][0]["result"]["conflict_count"] = serde_json::json!(5);
        hub.certify_success("vAlert", &other, started);
        assert_eq!(
            chain_entries(&hub).len(),
            2,
            "同版本不同族 findings 产新提案"
        );
    }

    /// 元定律登记门：findings 携带均衡判据阈值放宽物料 → 拒收返回 None，
    /// 不建提案（proposals 呈现窗长度不变）+ 链落 CompanionProposalRejected
    /// 留痕（cause=meta_rule_denied）；同 findings 重复注入不去重——每次
    /// 注入都落一条拒绝记录（恶意重放审计可见）
    #[test]
    fn meta_rule_findings_rejected_at_registry() {
        let hub = CompanionHub::new(CompanionConfig::for_tests(), None, None);
        let rejected_entries = |hub: &CompanionHub| -> Vec<serde_json::Value> {
            lock_poem(&hub.chain)
                .mem
                .iter()
                .filter(|e| e["kind"] == "CompanionProposalRejected")
                .cloned()
                .collect()
        };
        let meta_findings = serde_json::json!({
            "conflicts": 2,
            "cycles": 0,
            "rw_excluded": 0,
            "target_param": "alert_free_windows_required",
            "new_value": 1,
        });
        assert_eq!(
            hub.status_json()["proposals"].as_array().unwrap().len(),
            0,
            "测试前置：呈现窗为空"
        );
        assert!(
            hub.maybe_propose("vMeta", &meta_findings).is_none(),
            "元定律面 findings 拒收返回 None"
        );
        let entries = rejected_entries(&hub);
        assert_eq!(entries.len(), 1, "拒收落链留痕恰一条");
        let payload = &entries[0]["payload"];
        assert!(
            payload["proposal_id"]
                .as_str()
                .unwrap_or("")
                .starts_with("prop-vMeta-"),
            "确定性 ID 由幂等键派生（与正常提案同形态）"
        );
        assert!(
            !payload["findings_digest"].as_str().unwrap_or("").is_empty(),
            "findings digest 在场"
        );
        assert_eq!(
            payload["cause"],
            serde_json::json!(crate::tiered_adjudicator::REJECT_CAUSE_META_RULE)
        );
        assert!(payload["rejected_at"].as_u64().unwrap_or(0) > 0);
        // 不建提案：呈现窗长度不变（无 CompanionProposal 记录）
        assert_eq!(
            hub.status_json()["proposals"].as_array().unwrap().len(),
            0,
            "拒收提案不进呈现窗"
        );
        assert!(hub.verify_chain().is_ok(), "拒收落链后链完整可验");
        // 重复注入不去重：每次注入都落一条拒绝记录（幂等键只约束正常
        // 提案产出，不约束拒绝留痕）
        assert!(hub.maybe_propose("vMeta", &meta_findings).is_none());
        assert_eq!(
            rejected_entries(&hub).len(),
            2,
            "同 findings 重复注入每次留痕（恶意重放审计可见）"
        );
    }

    /// 防误伤回归：普通 findings 走正常提案路径（Some(id) + 链落
    /// CompanionProposal）——登记门未命中即零差
    #[test]
    fn normal_findings_still_propose() {
        let hub = CompanionHub::new(CompanionConfig::for_tests(), None, None);
        let normal = serde_json::json!({"conflicts": 3, "cycles": 0, "rw_excluded": 0});
        let id = hub
            .maybe_propose("vNorm", &normal)
            .expect("普通 findings 正常产提案");
        assert!(id.starts_with("prop-vNorm-"));
        let proposals: Vec<_> = lock_poem(&hub.chain)
            .mem
            .iter()
            .filter(|e| e["kind"] == "CompanionProposal")
            .cloned()
            .collect();
        assert_eq!(proposals.len(), 1, "正常提案事实落链");
        assert_eq!(
            proposals[0]["payload"]["proposal_id"],
            serde_json::json!(id)
        );
        let rejected = lock_poem(&hub.chain)
            .mem
            .iter()
            .filter(|e| e["kind"] == "CompanionProposalRejected")
            .count();
        assert_eq!(rejected, 0, "普通 findings 不误命中元定律登记门");
    }

    /// 口径钉死：warn/clean 级不产提案；clean 认证路径零变化
    /// （零拦截回归：提案机制的存在不影响既有认证语义）
    #[test]
    fn proposal_only_alert_warn_and_clean_never_propose() {
        let hub = CompanionHub::new(CompanionConfig::for_tests(), None, None);
        let started = std::time::Instant::now();
        let report_with = |conflicts: bool, cycles: bool| {
            serde_json::json!({
                "kind": "CompanionReport",
                "ruleset_version": "v",
                "rule_count": 3,
                "constitution_excluded_count": 0,
                "analyses": [
                    {"service": "inference_conflict_detect", "ok": true,
                     "result": {"has_conflicts": conflicts, "conflict_count": 0}},
                    {"service": "inference_cycle_detect", "ok": true,
                     "result": {"has_cycles": cycles, "cycle_count": 1}},
                    {"service": "inference_rw_extract", "ok": true, "result": {"results": []}},
                ],
            })
        };
        hub.certify_success("vClean", &report_with(false, false), started);
        hub.certify_success("vWarn", &report_with(false, true), started);
        let chain_count = lock_poem(&hub.chain)
            .mem
            .iter()
            .filter(|e| e["kind"] == "CompanionProposal")
            .count();
        assert_eq!(chain_count, 0, "warn/clean 级不产提案");
        let st = hub.status_json();
        assert_eq!(st["proposals"].as_array().unwrap().len(), 0);
        // 零拦截回归：clean/warn 认证照常（certified 双版本在册）
        assert_eq!(st["certified_versions"].as_array().unwrap().len(), 2);
        assert_eq!(st["clean_versions"], serde_json::json!(["vClean"]));
    }

    /// 处置生命周期：approve/reject 落链 CompanionAdjudication（payload 齐）、
    /// 生命周期守卫（未找到=Err，已处置不重复裁决）、零拦截回归
    /// （处置前后认证/报告语义零变化）
    #[test]
    fn adjudication_lifecycle_and_guards() {
        let hub = CompanionHub::new(CompanionConfig::for_tests(), None, None);
        let started = std::time::Instant::now();
        hub.certify_success("vAlert", &alert_report(), started);
        let status_before = hub.status_json();
        let id = hub.status_json()["proposals"][0]["proposal_id"]
            .as_str()
            .unwrap()
            .to_string();

        // approve：状态推进+裁决事实落链
        let rendered = hub.adjudicate(&id, "approved", "op-1").expect("approve ok");
        assert_eq!(rendered["status"], serde_json::json!("approved"));
        assert_eq!(rendered["actor"], serde_json::json!("op-1"));
        assert!(rendered["adjudicated_at"].as_u64().is_some());
        let chain = lock_poem(&hub.chain);
        let adj = chain
            .mem
            .iter()
            .find(|e| e["kind"] == "CompanionAdjudication")
            .expect("裁决事实在链");
        assert_eq!(adj["payload"]["proposal_id"], serde_json::json!(id));
        assert_eq!(adj["payload"]["outcome"], serde_json::json!("approved"));
        assert_eq!(adj["payload"]["actor"], serde_json::json!("op-1"));
        assert!(adj["payload"]["adjudicated_at"].as_u64().is_some());
        drop(chain);
        assert!(hub.verify_chain().is_ok(), "裁决落链后链完整可验");

        // 守卫：已处置不重复裁决（approve/reject 皆拒）+ 未找到
        assert!(hub.adjudicate(&id, "approved", "op-1").is_err());
        assert!(hub.adjudicate(&id, "rejected", "op-1").is_err());
        assert!(hub
            .adjudicate("prop-nonexistent", "approved", "op-1")
            .unwrap_err()
            .contains("not found"));

        // reject 路径：不同族提案独立处置
        let id2 = hub
            .maybe_propose("vOther", &serde_json::json!({"conflicts": 9}))
            .expect("新族提案");
        let rendered2 = hub.adjudicate(&id2, "rejected", "op-2").expect("reject ok");
        assert_eq!(rendered2["status"], serde_json::json!("rejected"));
        let outcomes: Vec<_> = lock_poem(&hub.chain)
            .mem
            .iter()
            .filter(|e| e["kind"] == "CompanionAdjudication")
            .map(|e| e["payload"]["outcome"].clone())
            .collect();
        assert_eq!(
            outcomes,
            vec![serde_json::json!("approved"), serde_json::json!("rejected")],
            "两裁决事实独立在链"
        );

        // 零拦截回归：处置前后认证集/报告呈现逐字节不变
        let status_after = hub.status_json();
        assert_eq!(
            status_before["certified_versions"],
            status_after["certified_versions"]
        );
        assert_eq!(
            status_before["clean_versions"],
            status_after["clean_versions"]
        );
        assert_eq!(status_before["last_report"], status_after["last_report"]);
        assert_eq!(status_before["state"], status_after["state"]);
    }

    // ===== 治理链上行（出站适配器；缺省关；环回 stub 实测）=====

    /// 治理 propose 环回 stub：按 Content-Length 收 POST 体、回固定状态码
    /// （可配回包延迟，制造 pending 窗口）、请求体经 channel 交测试断言。
    async fn spawn_propose_stub(
        status: u16,
        delay_ms: u64,
    ) -> (
        std::net::SocketAddr,
        tokio::sync::mpsc::UnboundedReceiver<(String, String)>, // (headers, body)
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut served = 0usize;
            while served < 8 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                served += 1;
                let mut buf: Vec<u8> = Vec::new();
                let mut tmp = [0u8; 4096];
                let header_end = loop {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        break buf.len();
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&buf[..header_end]).to_lowercase();
                let len: usize = headers
                    .lines()
                    .find(|l| l.starts_with("content-length:"))
                    .and_then(|l| l.split(':').nth(1))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                while buf.len() < header_end + len {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let body = String::from_utf8_lossy(&buf[header_end..]).to_string();
                // 头面一并回传（D3① 凭据双发断言用；小写归一如上行）
                let _ = tx.send((headers, body));
                if delay_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                }
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        (addr, rx)
    }

    fn uplink_cfg(addr: std::net::SocketAddr, dataset: Option<&str>) -> CompanionUplinkConfig {
        CompanionUplinkConfig {
            base_url: format!("http://{addr}"),
            api_key: "test-scoped-key".to_string(),
            dataset: dataset.map(|d| d.to_string()),
        }
    }

    /// 轮询等待上行出站任务到达终态（stub 环回即时回包，上限 3s 兜底）
    async fn wait_uplink_terminal(
        hub: &std::sync::Arc<CompanionHub>,
        pid: &str,
    ) -> serde_json::Value {
        for _ in 0..300 {
            {
                let up = lock_poem(&hub.uplink);
                if let Some(s) = up.get(pid) {
                    if s.state != "pending" {
                        return s.render();
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("uplink 未在时限内到达终态");
    }

    /// 上行成功链路：approve 后出站 2xx → 状态 ok + CompanionUplink 落链
    /// （outcome/entry_id/http 终态齐）；请求体载荷面照 A 案重塑（K3 同构
    /// fact 壳/findings 全文随 payload/凭据双发 X-Api-Key+Bearer/dataset 携带）
    #[tokio::test]
    async fn uplink_success_chain_fact_and_payload() {
        let (addr, mut rx) = spawn_propose_stub(200, 0).await;
        let hub = CompanionHub::new(CompanionConfig::for_tests(), None, None);
        hub.with_uplink_config(Some(uplink_cfg(addr, Some("gov-ds"))));
        let started = std::time::Instant::now();
        hub.certify_success("vUp", &alert_report(), started);
        let id = hub.status_json()["proposals"][0]["proposal_id"]
            .as_str()
            .unwrap()
            .to_string();
        hub.adjudicate(&id, "approved", "op-up")
            .expect("approve ok");

        let info = hub.uplink_proposal(&id).expect("上行受理");
        assert_eq!(info["state"], serde_json::json!("pending"));
        let entry_id = info["entry_id"].as_str().unwrap().to_string();
        assert_eq!(
            entry_id,
            companion_uplink_entry_id(&id),
            "entry_id 由 proposal_id 确定性派生"
        );

        let term = wait_uplink_terminal(&hub, &id).await;
        assert_eq!(term["state"], serde_json::json!("ok"));
        assert_eq!(term["http_status"], serde_json::json!(200));

        let facts: Vec<_> = lock_poem(&hub.chain)
            .mem
            .iter()
            .filter(|e| e["kind"] == "CompanionUplink")
            .cloned()
            .collect();
        assert_eq!(facts.len(), 1, "恰一条上行链事实");
        let pl = &facts[0]["payload"];
        assert_eq!(pl["proposal_id"], serde_json::json!(id));
        assert_eq!(pl["entry_id"], serde_json::json!(entry_id));
        assert_eq!(pl["outcome"], serde_json::json!("ok"));
        assert_eq!(pl["http_status"], serde_json::json!(200));
        assert!(hub.verify_chain().is_ok(), "上行落链后链完整可验");

        let (headers, raw): (String, String) = rx.recv().await.expect("stub 收到出站请求");
        // D3① 凭据双发：X-Api-Key（rule serve 原生通道）+ Bearer（engine
        // 统一鉴权中间件通道），同值
        assert!(
            headers.contains("x-api-key: test-scoped-key"),
            "X-Api-Key 头在场: {headers}"
        );
        assert!(
            headers.contains("authorization: bearer test-scoped-key"),
            "Authorization Bearer 头在场: {headers}"
        );
        let body: serde_json::Value = serde_json::from_str(&raw).expect("请求体为合法 JSON");
        assert_eq!(body["dataset_id"], serde_json::json!("gov-ds"));
        let entry = &body["entry"];
        assert_eq!(entry["entry_id"], serde_json::json!(entry_id));
        // A 案重塑：K3 同构 fact 壳（statement/title/confidence/findings 全文）
        assert_eq!(entry["version"], serde_json::json!(1));
        assert_eq!(
            entry["schema_ref"],
            serde_json::json!("builtin:knowledge/fact")
        );
        assert_eq!(entry["payload"]["confidence"], serde_json::json!(0.6));
        assert!(
            !entry["payload"]["statement"]
                .as_str()
                .unwrap_or("")
                .is_empty(),
            "statement 摘要定式在场"
        );
        assert!(
            entry["payload"]["findings"].is_object(),
            "findings 全文随 payload 入账"
        );

        let st = hub.status_json();
        assert_eq!(st["uplinks"][id.as_str()]["state"], serde_json::json!("ok"));
        assert_eq!(
            st["proposals"][0]["uplink"]["state"],
            serde_json::json!("ok"),
            "提案条目附 uplink 字段"
        );
    }

    /// 零拦截降级链路：出站指向死端口 → 状态 failed+last_error、落链
    /// failed 事实、裁决/认证照常；重试改投活 stub → 同 entry_id 幂等
    /// 复用、状态转 ok、第二落链事实齐
    #[tokio::test]
    async fn uplink_degraded_adjudication_intact_and_retry_recovers() {
        // 占一个空闲端口再释放：随后连接该端口即遭拒（快败，不挂 5s 超时）
        let dead = std::net::TcpListener::bind("127.0.0.1:0").expect("占位取空闲端口");
        let dead_addr = dead.local_addr().expect("addr");
        drop(dead);
        let hub = CompanionHub::new(CompanionConfig::for_tests(), None, None);
        hub.with_uplink_config(Some(uplink_cfg(dead_addr, None)));
        let started = std::time::Instant::now();
        hub.certify_success("vDeg", &alert_report(), started);
        let id = hub.status_json()["proposals"][0]["proposal_id"]
            .as_str()
            .unwrap()
            .to_string();
        hub.adjudicate(&id, "approved", "op-deg")
            .expect("approve ok");

        let info = hub
            .uplink_proposal(&id)
            .expect("受理（守卫只看配置与提案态，不预检网络）");
        let term = wait_uplink_terminal(&hub, &id).await;
        assert_eq!(term["state"], serde_json::json!("failed"));
        assert!(
            term["last_error"]
                .as_str()
                .unwrap_or("")
                .contains("uplink transport error"),
            "last_error 记录传输层失败"
        );

        // 零拦截：裁决事实照常在链，失败不回滚裁决
        assert!(
            lock_poem(&hub.chain)
                .mem
                .iter()
                .any(|e| e["kind"] == "CompanionAdjudication"),
            "裁决事实在场"
        );
        let facts: Vec<_> = lock_poem(&hub.chain)
            .mem
            .iter()
            .filter(|e| e["kind"] == "CompanionUplink")
            .cloned()
            .collect();
        assert_eq!(facts.len(), 1, "failed 上行事实落链");
        assert_eq!(facts[0]["payload"]["outcome"], serde_json::json!("failed"));

        // 重试恢复：改投活 stub，同 entry_id 幂等复用
        let (addr, mut rx) = spawn_propose_stub(200, 0).await;
        hub.with_uplink_config(Some(uplink_cfg(addr, None)));
        let info2 = hub.uplink_proposal(&id).expect("重试受理");
        assert_eq!(
            info2["entry_id"], info["entry_id"],
            "重试幂等：同提案恒同 entry_id"
        );
        let term2 = wait_uplink_terminal(&hub, &id).await;
        assert_eq!(term2["state"], serde_json::json!("ok"));
        let body: serde_json::Value =
            serde_json::from_str(&rx.recv().await.expect("stub 收到重试出站请求").1)
                .expect("请求体为合法 JSON");
        assert!(
            body.get("dataset_id").is_none(),
            "dataset 未配置时省略 dataset_id 键"
        );
        let facts2 = lock_poem(&hub.chain)
            .mem
            .iter()
            .filter(|e| e["kind"] == "CompanionUplink")
            .count();
        assert_eq!(facts2, 2, "failed+ok 两条上行事实独立在链");
        assert!(hub.verify_chain().is_ok());
    }

    /// 未配置零行为回归：上行面无状态、无链事实、裁决照常、status
    /// uplinks 空、proposals 条目 uplink=null
    #[tokio::test]
    async fn uplink_unconfigured_zero_behavior() {
        let hub = CompanionHub::new(CompanionConfig::for_tests(), None, None);
        let started = std::time::Instant::now();
        hub.certify_success("vZ", &alert_report(), started);
        let id = hub.status_json()["proposals"][0]["proposal_id"]
            .as_str()
            .unwrap()
            .to_string();
        hub.adjudicate(&id, "approved", "op-z").expect("approve ok");

        match hub.uplink_proposal(&id) {
            Err(UplinkError::NotConfigured) => {}
            other => panic!("未配置应 NotConfigured，实际 {other:?}"),
        }
        // 守卫序前置：配置核先于提案核（未找到同报 NotConfigured）
        assert!(matches!(
            hub.uplink_proposal("prop-x"),
            Err(UplinkError::NotConfigured)
        ));

        let st = hub.status_json();
        assert_eq!(
            st["uplinks"].as_object().expect("uplinks 键在场").len(),
            0,
            "上行状态面为空"
        );
        assert_eq!(
            st["proposals"][0]["uplink"],
            serde_json::Value::Null,
            "提案条目 uplink=null"
        );
        assert!(
            !lock_poem(&hub.chain)
                .mem
                .iter()
                .any(|e| e["kind"] == "CompanionUplink"),
            "无上行链事实"
        );
    }

    /// 守卫面：proposed/rejected 皆拒上行、未找到拒、in-flight（pending
    /// 窗口内二次上行）拒
    #[tokio::test]
    async fn uplink_guards_not_approved_notfound_inflight() {
        let (addr, _rx) = spawn_propose_stub(200, 0).await;
        let hub = CompanionHub::new(CompanionConfig::for_tests(), None, None);
        hub.with_uplink_config(Some(uplink_cfg(addr, None)));
        let started = std::time::Instant::now();
        hub.certify_success("vG", &alert_report(), started);
        let id = hub.status_json()["proposals"][0]["proposal_id"]
            .as_str()
            .unwrap()
            .to_string();

        assert!(
            matches!(
                hub.uplink_proposal(&id),
                Err(UplinkError::NotApproved(s)) if s == "proposed"
            ),
            "proposed 态拒上行"
        );
        assert!(
            matches!(hub.uplink_proposal("prop-nope"), Err(UplinkError::NotFound)),
            "未找到拒"
        );
        hub.adjudicate(&id, "rejected", "op-g").expect("reject ok");
        assert!(
            matches!(
                hub.uplink_proposal(&id),
                Err(UplinkError::NotApproved(s)) if s == "rejected"
            ),
            "rejected 态拒上行"
        );

        // in-flight：慢 stub 拉住 pending 窗口，窗口内二次上行拒
        hub.certify_success("vG2", &alert_report(), started);
        let id2 = hub.status_json()["proposals"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| {
                p["proposal_id"]
                    .as_str()
                    .unwrap_or("")
                    .starts_with("prop-vG2-")
            })
            .map(|p| p["proposal_id"].as_str().unwrap().to_string())
            .expect("vG2 提案在窗");
        hub.adjudicate(&id2, "approved", "op-g")
            .expect("approve ok");
        let (slow_addr, _slow_rx) = spawn_propose_stub(200, 300).await;
        hub.with_uplink_config(Some(uplink_cfg(slow_addr, None)));
        hub.uplink_proposal(&id2).expect("首次上行受理");
        assert!(
            matches!(hub.uplink_proposal(&id2), Err(UplinkError::InFlight)),
            "pending 窗口内二次上行拒"
        );
        wait_uplink_terminal(&hub, &id2).await;
    }
}

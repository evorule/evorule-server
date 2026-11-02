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

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::json;
use tokio_util::sync::CancellationToken;

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

/// 有界认证集：锚定版本常驻 + 最近 [`CERTIFIED_MAX_EXTRA`] 条（淘汰者由
/// 调用方落链 `CompanionCertExpired` 记录性事实——认证集是窗口不是全集）
struct CertifiedSet {
    map: HashMap<String, u64>,
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

    fn len(&self) -> usize {
        self.map.len()
    }

    fn versions(&self) -> Vec<String> {
        self.map.keys().cloned().collect()
    }

    /// 插入认证；超限淘汰最旧的非锚版本并返回之（供落链）
    fn insert(&mut self, version: &str, at: u64) -> Option<String> {
        if self.map.insert(version.to_string(), at).is_none() {
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
                            self.map.remove(&v);
                            evicted = Some(v);
                        }
                    }
                    None => break, // 全是锚（理论不可达）
                }
            } else {
                self.map.remove(&old);
                evicted = Some(old);
            }
        }
        evicted
    }
}

/// 伴跑配置（获批设计稿 D3 参数，配置面可调）
#[derive(Debug, Clone)]
pub struct CompanionConfig {
    pub debounce: Duration,
    pub min_interval: Duration,
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

    /// 注入分析执行 runtime（单 worker 线程承载三件套 CPU 密集段；构造后
    /// 运行前调用；不注入=内联执行缺省兜底）
    pub fn with_exec_runtime(self: &Arc<Self>, handle: tokio::runtime::Handle) {
        *lock_poem(&self.exec) = Some(handle);
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
        let seq_report = self.append_chain_with_artifact(
            "CompanionReport",
            json!({
                "ruleset_version": version,
                "rule_count": report.get("rule_count"),
                "constitution_excluded_count": report.get("constitution_excluded_count"),
                "analyses": analyses,
                "trigger": lock_poem(&self.last_trigger).clone(),
            }),
            Some((report, &report_digest)),
        );
        // 认证集有界插入；淘汰者落链记录性事实（窗口语义可见）
        let evicted = lock_poem(&self.certified).insert(version, now_secs());
        if let Some(old) = evicted {
            self.append_chain(
                "CompanionCertExpired",
                json!({ "version": old, "superseded_by": version }),
            );
        }
        *lock_poem(&self.last_report) = Some(json!({
            "report": report,
            "report_json_digest": report_digest,
            "chain_seq": seq_report,
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
        let chain = lock_poem(&self.chain);
        let errors = lock_poem(&self.errors).clone();
        json!({
            "enabled": self.enabled.load(Ordering::Relaxed),
            "state": lock_poem(&self.phase).as_str(),
            "latest_version": lock_poem(&self.latest_version).clone(),
            "dirty_since": lock_poem(&self.dirty_since).clone(),
            "last_error": errors.back().cloned(),
            "errors": errors.iter().cloned().collect::<Vec<_>>(),
            "last_report": lock_poem(&self.last_report).clone(),
            "certified_versions": lock_poem(&self.certified).versions(),
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
        })
    }

    /// 覆盖率呈现（版本认证制；会话归属=锚定版本∈认证集）
    pub fn coverage_json(&self, sessions_total: usize) -> serde_json::Value {
        let certified = lock_poem(&self.certified);
        let anchor = lock_poem(&self.anchor_version).clone();
        let anchor_certified = anchor
            .as_ref()
            .map(|v| certified.contains(v))
            .unwrap_or(false);
        let sessions_covered = if anchor_certified { sessions_total } else { 0 };
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
        json!({
            "certified_versions": certified.versions(),
            "certified_count": certified.len(),
            "anchor_version": anchor,
            "anchor_certified": anchor_certified,
            "sessions_total": sessions_total,
            "sessions_covered": sessions_covered,
            "companion_coverage_ratio": ratio,
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
    fn certified_set_bounded_anchor_never_evicted() {
        let mut set = CertifiedSet::new(Some("anchor".to_string()));
        set.insert("anchor", 1);
        let mut evicted_any = false;
        for i in 0..14 {
            let v = format!("v{i}");
            if let Some(old) = set.insert(&v, 2) {
                assert_ne!(old, "anchor", "锚永不淘汰");
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
            set.insert(&format!("v{i}"), 1);
        }
        assert_eq!(set.len(), CERTIFIED_MAX_EXTRA, "无锚认证集硬上限=最近 N 条");
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
}

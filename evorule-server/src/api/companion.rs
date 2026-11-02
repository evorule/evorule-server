// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 伴跑状态机 A→B 最小版（获批设计稿）
//!
//! 审计链驱动的确定性旁路消费者：规则集版本变更（载入/编辑检出=同一装载漏斗）
//! → 去抖 → lint 三件套进程内直调（与 HTTP invoke 同一确定性内核）→ 报告落
//! 伴随域自有哈希链事实（记录性，不进业务会话链）→ 认证集更新 → 覆盖率可查询。
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
    /// 计数器（status 呈现面）
    analyses_started: AtomicU64,
    analyses_ok: AtomicU64,
    analyses_failed: AtomicU64,
    stale_skipped: AtomicU64,
    /// 被更新版本抢占取消的分析数（与真失败严格区分：不落链不计 failed）
    stale_cancelled: AtomicU64,
    chain: Mutex<ChainState>,
    /// Prometheus 指标面（None=完全关闭；只读计数零控制面——硬边界）
    metrics: Mutex<Option<CompanionMetrics>>,
}

impl CompanionHub {
    /// 构造中枢。`chain_path`：链文件路径（None=纯内存）。
    /// `anchor_version`：启动期规则集版本（与 G6 会话锚定同源）。
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
        Arc::new(Self {
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
            analyses_started: AtomicU64::new(0),
            analyses_ok: AtomicU64::new(0),
            analyses_failed: AtomicU64::new(0),
            stale_skipped: AtomicU64::new(0),
            stale_cancelled: AtomicU64::new(0),
            chain: Mutex::new(ChainState {
                path: chain_path,
                reports_dir,
                seq: 0,
                prev_hash: String::new(),
                mem: Vec::new(),
            }),
            metrics: Mutex::new(None),
        })
    }

    /// 挂载 Prometheus 指标面（与 server 既有 registry 同注册形态；只读计数；
    /// 构造后运行前调用，重复调用以最后一次为准）
    pub fn with_metrics(self: &Arc<Self>, registry: &prometheus::Registry) {
        *lock_poem(&self.metrics) = CompanionMetrics::register(registry);
    }

    /// 装载漏斗信号入口（启动=reload_from_disk=同一漏斗；事件驱动零轮询）
    pub fn notify_version(&self, version: String) {
        {
            let mut lv = lock_poem(&self.latest_version);
            if lv.as_deref() == Some(version.as_str()) {
                return; // 同版本信号幂等（零活动语义：无变更即无事件）
            }
            *lv = Some(version.clone());
        }
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
        }
    }

    /// 执行一次分析（快照即喂→三件套→落链→认证；per-run 取消令牌防倒挂）
    async fn analyze(&self, api: &SessionApi, version: &str) {
        self.analyses_started.fetch_add(1, Ordering::Relaxed);
        *lock_poem(&self.phase) = Phase::Analyzing;
        let token = CancellationToken::new();
        *lock_poem(&self.cancel_token) = Some(token.clone());
        let started = std::time::Instant::now();
        let result = api.companion_run_lint_suite(version, Some(&token)).await;
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

    fn append_chain_with_artifact(
        &self,
        kind: &str,
        payload: serde_json::Value,
        artifact: Option<(&serde_json::Value, &String)>,
    ) -> u64 {
        let mut chain = lock_poem(&self.chain);
        chain.seq += 1;
        let seq = chain.seq;
        // 报告全文落盘（设计稿：全文落 companion 本地报告目录，事实内持 digest 互证）
        if let (Some(dir), Some((full, _digest))) = (chain.reports_dir.clone(), artifact) {
            let file = dir.join(format!("report_{seq:06}.json"));
            if let Some(parent) = file.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&file, serde_json::to_vec_pretty(full).unwrap_or_default());
        }
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
        if let Some(path) = &chain.path {
            let line = match serde_json::to_string(&entry) {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(seq, error = %e, "companion chain serialize failed (memory chain authoritative)");
                    return seq;
                }
            };
            // fail-soft 追加：磁盘故障不阻断伴跑（内存链+日志可见）
            if let Err(e) = append_line(path, &line) {
                tracing::warn!(seq, error = %e, "companion chain append failed (memory chain authoritative)");
            }
        }
        seq
    }

    /// 链完整性校验（逐条重算哈希+前链比对）
    pub fn verify_chain(&self) -> Result<u64, String> {
        let chain = lock_poem(&self.chain);
        let mut prev = String::new();
        for entry in &chain.mem {
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
            let entry_prev = entry
                .get("prev_hash")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if entry_prev != prev {
                return Err(format!("chain entry {seq} prev_hash mismatch"));
            }
            prev = hash;
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
                "tail": chain.mem.iter().rev().take(8).cloned().collect::<Vec<_>>(),
            },
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
}

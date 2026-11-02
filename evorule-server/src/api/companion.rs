//! 伴跑状态机 A→B 最小版（design-22 批 R3 / design-63 获批设计稿，CLAIM-BOARD B-098）
//!
//! 审计链驱动的确定性旁路消费者：规则集版本变更（载入/编辑检出=同一装载漏斗）
//! → 去抖 → lint 三件套进程内直调（与 HTTP invoke 同一确定性内核）→ 报告落
//! 伴随域自有哈希链事实（记录性，不进业务会话链）→ 认证集更新 → 覆盖率可查询。
//!
//! 硬边界（design-22 §三 R3 四条照单全收）：
//! 1. 只报告不拦截——本模块无任何阻断权，产出=报告/证书+查询面
//! 2. 干预必留痕——enable/disable 双向落链事实 + `tracing::warn!`
//! 3. 收尾覆盖率打标——`GET /api/companion/coverage` 可查询
//! 4. 零侵入主循环——独立 tokio task（`run`），不改 evo-agent（零 commit）
//!    与 server 既有 handler 语义（新增独立 `/api/companion/*` 命名空间）
//!
//! 落链形态（design-63 §10 D2 获批项的施工实现）：`evorule-reactor` 为
//! crates.io 依赖（vendored patch 已退役收编为原生 crate），本批不可加枚举
//! 变体——故为**伴随域自有哈希链事实**：JSONL 追加 + blake3 前链（`prev_hash`
//! 链接、`evorule_hash::json_digest` 键序稳定摘要），记录性事实语义与
//! reactor `TransitionTrace` 同构（不推进会话版本）；链文件与 wal-dir 同级
//! 配置面（`companion_chain.jsonl`），无 wal-dir（纯内存/测试）时内存链兜底。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::json;

use super::server::SessionApi;

/// lint 三件套（design-63 §1.3：全部非 sensitive，REST 直调合法，进程内直调同权）
pub(crate) const LINT_SERVICE_NAMES: &[&str] = &[
    "inference_rw_extract",
    "inference_cycle_detect",
    "inference_conflict_detect",
];

/// 缺省去抖静默窗（design-63 §10 D3 获批参数）
pub(crate) const DEFAULT_DEBOUNCE_MS: u64 = 3000;

/// 缺省两次已完成分析最小间隔（design-63 §10 D3 获批参数）
pub(crate) const DEFAULT_MIN_INTERVAL_MS: u64 = 60_000;

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

/// 伴跑配置（design-63 §10 D3 获批参数，配置面可调）
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
    last_error: Mutex<Option<String>>,
    last_analysis_done: Mutex<Option<std::time::Instant>>,
    /// 认证集：规则集版本 → 认证时刻（UNIX 秒）
    certified: Mutex<HashMap<String, u64>>,
    /// 锚定版本（启动期注入=会话创建 `__ruleset__.version` 同源；覆盖率会话归属判据）
    anchor_version: Mutex<Option<String>>,
    /// 计数器（status 呈现面）
    analyses_started: AtomicU64,
    analyses_ok: AtomicU64,
    analyses_failed: AtomicU64,
    stale_skipped: AtomicU64,
    chain: Mutex<ChainState>,
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
            last_error: Mutex::new(None),
            last_analysis_done: Mutex::new(None),
            certified: Mutex::new(HashMap::new()),
            anchor_version: Mutex::new(anchor_version),
            analyses_started: AtomicU64::new(0),
            analyses_ok: AtomicU64::new(0),
            analyses_failed: AtomicU64::new(0),
            stale_skipped: AtomicU64::new(0),
            chain: Mutex::new(ChainState {
                path: chain_path,
                reports_dir,
                seq: 0,
                prev_hash: String::new(),
                mem: Vec::new(),
            }),
        })
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
                let uncertified = match (
                    lock_poem(&self.latest_version).clone(),
                    lock_poem(&self.certified).clone(),
                ) {
                    (Some(v), c) => !c.contains_key(&v),
                    (None, _) => false,
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
            if lock_poem(&self.certified).contains_key(&latest) {
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

    /// 执行一次分析（快照即喂→三件套→落链→认证）
    async fn analyze(&self, api: &SessionApi, version: &str) {
        self.analyses_started.fetch_add(1, Ordering::Relaxed);
        *lock_poem(&self.phase) = Phase::Analyzing;
        match api.companion_run_lint_suite(version).await {
            Ok(report) => {
                let report_digest = evorule_hash::json_digest(&report);
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
                    Some((&report, &report_digest)),
                );
                {
                    let mut c = lock_poem(&self.certified);
                    c.insert(version.to_string(), now_secs());
                }
                *lock_poem(&self.last_report) = Some(json!({
                    "report": report,
                    "report_json_digest": report_digest,
                    "chain_seq": seq_report,
                }));
                *lock_poem(&self.last_error) = None;
                *lock_poem(&self.dirty_since) = None;
                *lock_poem(&self.last_analysis_done) = Some(std::time::Instant::now());
                *lock_poem(&self.phase) = Phase::Certified;
                self.analyses_ok.fetch_add(1, Ordering::Relaxed);
                tracing::info!(
                    ruleset_version = version,
                    seq = seq_report,
                    "companion report certified"
                );
            }
            Err(e) => {
                self.analyses_failed.fetch_add(1, Ordering::Relaxed);
                *lock_poem(&self.last_error) = Some(e.clone());
                // 失败回到脏态（fail-visible：last_error 呈现；下一次版本信号自然重试）
                *lock_poem(&self.phase) = Phase::Dirty;
                tracing::warn!(ruleset_version = version, error = %e, "companion analysis failed");
            }
        }
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
        // 报告全文落盘（design-63 §5：全文落 companion 本地报告目录，事实内持 digest 互证）
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
        json!({
            "enabled": self.enabled.load(Ordering::Relaxed),
            "state": lock_poem(&self.phase).as_str(),
            "latest_version": lock_poem(&self.latest_version).clone(),
            "dirty_since": lock_poem(&self.dirty_since).clone(),
            "last_error": lock_poem(&self.last_error).clone(),
            "last_report": lock_poem(&self.last_report).clone(),
            "certified_versions": lock_poem(&self.certified).keys().cloned().collect::<Vec<_>>(),
            "counters": {
                "analyses_started": self.analyses_started.load(Ordering::Relaxed),
                "analyses_ok": self.analyses_ok.load(Ordering::Relaxed),
                "analyses_failed": self.analyses_failed.load(Ordering::Relaxed),
                "stale_skipped": self.stale_skipped.load(Ordering::Relaxed),
            },
            "chain": {
                "seq": chain.seq,
                "kind": "companion-hash-chain-v1",
                "tail": chain.mem.iter().rev().take(8).cloned().collect::<Vec<_>>(),
            },
        })
    }

    /// 覆盖率呈现（design-63 §6 版本认证制；会话归属=锚定版本∈认证集）
    pub fn coverage_json(&self, sessions_total: usize) -> serde_json::Value {
        let certified = lock_poem(&self.certified);
        let anchor = lock_poem(&self.anchor_version).clone();
        let anchor_certified = anchor
            .as_ref()
            .map(|v| certified.contains_key(v))
            .unwrap_or(false);
        let sessions_covered = if anchor_certified { sessions_total } else { 0 };
        let ratio = if sessions_total == 0 {
            None
        } else {
            Some(sessions_covered as f64 / sessions_total as f64)
        };
        json!({
            "certified_versions": certified.keys().cloned().collect::<Vec<_>>(),
            "certified_count": certified.len(),
            "anchor_version": anchor,
            "anchor_certified": anchor_certified,
            "sessions_total": sessions_total,
            "sessions_covered": sessions_covered,
            "companion_coverage_ratio": ratio,
        })
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

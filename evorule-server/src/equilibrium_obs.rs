// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Companion-side equilibrium observation aggregator (rule-layer v0.12.0 intake).
//!
//! Maintains the windowed counters that `equilibrium_gate` (server_eval.json
//! v0.12.0, having taken over the primary contract from the retired
//! `equilibrium_check`) adjudicates over: proposal intake vs digestion rates,
//! consecutive alert-free windows, latest verification coverage and the
//! certified-streak. The rule layer only ever sees a caller-supplied snapshot
//! — window aggregation and streak accounting stay here (zero data reshaping
//! inside the rule layer).
//!
//! Event sources are wired as pure observation (zero interception): the verify
//! face records proposals / certificates / alerts, the companion face records
//! proposal digestion and coverage, and a dedicated window-settlement task
//! ([`spawn_equilibrium_evaluator`]) closes windows, emits the legislated
//! `equilibrium_gate` instruction into the single reactor and caches the
//! verdict for the status surface. The gauges below expose the live state on
//! the metrics surface.
//!
//! Feature extraction is registry-driven: every feature quantity consumed by
//! the window evaluator is declared in [`EQUILIBRIUM_FEATURES`] (analytic
//! formula + dimensional fingerprint + window semantics + regime / error
//! contract / reset semantics / evidence anchor). The evaluator extracts
//! values strictly through the registry's scope ([`extract_feature_values`]);
//! the emitted `equilibrium_gate` instruction and the status surface remain
//! contractually unchanged (same parameter names and values as before the
//! registry migration).
//!
//! Every settlement also advances the evidence anchor ([`EquilibriumWindows::
//! settle_window_evidence`]): an incremental hash chain over the full
//! settlement observation records (window inputs, feature values and formula
//! version stamps), so any recorded observation stream replays to the exact
//! same anchor sequence — the registry's `evidence_anchor` declarations
//! describe this bound anchor form. The anchor is metadata only: it never
//! enters the emitted instruction parameters or the status contract.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use prometheus::IntGauge;

/// Shared gauge handles registered by `PrometheusMetrics::new()`; update helpers are
/// no-ops before registration (same pattern as `RULESET_RULE_COUNT_SHARED`).
static EQUILIBRIUM_ALERT_FREE_WINDOWS: std::sync::OnceLock<IntGauge> = std::sync::OnceLock::new();
static EQUILIBRIUM_CERTIFIED_STREAK: std::sync::OnceLock<IntGauge> = std::sync::OnceLock::new();
static EQUILIBRIUM_PROPOSALS_IN_TOTAL: std::sync::OnceLock<prometheus::IntCounter> =
    std::sync::OnceLock::new();
static EQUILIBRIUM_DIGESTED_TOTAL: std::sync::OnceLock<prometheus::IntCounter> =
    std::sync::OnceLock::new();

/// Register the shared equilibrium gauges/counters (called from metrics registration).
pub fn register_shared(
    alert_free_windows: IntGauge,
    certified_streak: IntGauge,
    proposals_in_total: prometheus::IntCounter,
    digested_total: prometheus::IntCounter,
) {
    let _ = EQUILIBRIUM_ALERT_FREE_WINDOWS.set(alert_free_windows);
    let _ = EQUILIBRIUM_CERTIFIED_STREAK.set(certified_streak);
    let _ = EQUILIBRIUM_PROPOSALS_IN_TOTAL.set(proposals_in_total);
    let _ = EQUILIBRIUM_DIGESTED_TOTAL.set(digested_total);
}

// ---------------------------------------------------------------------------
// 进程级共享实例（与上方 gauge 的静态挂载先例同型）
// ---------------------------------------------------------------------------

static SHARED_WINDOWS: std::sync::OnceLock<std::sync::Arc<EquilibriumWindows>> =
    std::sync::OnceLock::new();

/// 进程级共享窗计数实例：verify 面与伴跑面零耦合共同记账。
pub fn shared() -> std::sync::Arc<EquilibriumWindows> {
    SHARED_WINDOWS
        .get_or_init(|| std::sync::Arc::new(EquilibriumWindows::new()))
        .clone()
}

/// 当前窗内 alert/contradiction 计数（与窗结构解耦：窗结算时由评估任务
/// 一次性取走清空，聚合件零改动）。
static PENDING_ALERTS: AtomicU64 = AtomicU64::new(0);

/// 一次 alert/contradiction 判定记入当前窗（纯观测，零拦截）。
pub fn record_alert() {
    PENDING_ALERTS.fetch_add(1, Ordering::Relaxed);
}

/// 窗结算：取走并清空当前窗 alert 计数（>0 = 本窗有警）。
pub fn drain_pending_alerts() -> u64 {
    PENDING_ALERTS.swap(0, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Φ 位势与待裁观测（分层分域：law_id → (domain, h_p, lvl_p)；纯观测零拦截）
// ---------------------------------------------------------------------------

/// 一次待重裁入账条目（graph_shock_propagate 结果回包 `pending_readjudication[]`
/// 元素经 law 注册面映射后的 server 侧记账产物）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingReadjudication {
    /// law/指令 id（与 law_set 注册面同 id 空间）。
    pub law_id: String,
    /// 归属域（law 的 domains[0]；None = 不可单域归因）。
    pub domain: Option<String>,
    /// 版本链高度 h_p（重裁水位 lvl_p 推进上限；来自 law 注册面
    /// max_version_depth）。
    pub chain_height: u32,
}

/// 单 law 待重裁账目：(归属域, 链高 h_p, 重裁水位 lvl_p)。
#[derive(Debug, Clone)]
struct PendingEntry {
    domain: Option<String>,
    height: u32,
    level: u32,
}

/// 待重裁账（Φ 位势增量维护面）：Φ_dom = Σ_p (h_p − lvl_p)，入账/推进点
/// 成对增量增减（O(1) 摊还，不重算）；全局待裁总数 = Σ(h_p − lvl_p)，
/// 与按域分账同一账本两视图。
#[derive(Debug, Default)]
struct ReadjudicationLedger {
    entries: BTreeMap<String, PendingEntry>,
    phi_by_domain: BTreeMap<String, u64>,
    pending_total: u64,
}

static READJUDICATION_LEDGER: std::sync::OnceLock<Mutex<ReadjudicationLedger>> =
    std::sync::OnceLock::new();

fn readjudication_ledger() -> &'static Mutex<ReadjudicationLedger> {
    READJUDICATION_LEDGER.get_or_init(|| Mutex::new(ReadjudicationLedger::default()))
}

/// 域账键（None = 不可单域归因，落全局归因桶 `""`，与域警账同口径不丢账）。
fn domain_key(domain: Option<&str>) -> String {
    domain.unwrap_or("").to_string()
}

/// 待重裁入账（纯观测零拦截）。
///
/// - 同 law_id 同 (domain, h_p) 重复入账 = no-op 守卫（不重复计）；
/// - 同 law_id 不同 (domain, h_p) 以新值为准刷新（旧 Φ 贡献成对减除，
///   新值自 lvl=0 重计）；
/// - Φ 与全局待裁总数同一锁内成对增量维护。
pub fn record_pending_readjudication(entries: &[PendingReadjudication]) {
    let mut ledger = readjudication_ledger()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for e in entries {
        if let Some(old) = ledger.entries.get(&e.law_id) {
            if old.domain == e.domain && old.height == e.chain_height {
                continue;
            }
        }
        if let Some(old) = ledger.entries.remove(&e.law_id) {
            let outstanding = u64::from(old.height.saturating_sub(old.level));
            let key = domain_key(old.domain.as_deref());
            if let Some(phi) = ledger.phi_by_domain.get_mut(&key) {
                *phi = phi.saturating_sub(outstanding);
                if *phi == 0 {
                    ledger.phi_by_domain.remove(&key);
                }
            }
            ledger.pending_total = ledger.pending_total.saturating_sub(outstanding);
        }
        ledger.entries.insert(
            e.law_id.clone(),
            PendingEntry {
                domain: e.domain.clone(),
                height: e.chain_height,
                level: 0,
            },
        );
        let key = domain_key(e.domain.as_deref());
        let phi = ledger.phi_by_domain.entry(key).or_insert(0);
        *phi = phi.saturating_add(u64::from(e.chain_height));
        ledger.pending_total = ledger
            .pending_total
            .saturating_add(u64::from(e.chain_height));
    }
}

/// 重裁推进（纯观测零拦截）：该 law 重裁水位 lvl_p +1，封顶 h_p（单调只升
/// 不降，封顶后再推进 no-op）；Φ_dom 与全局待裁总数成对 −1。未知 law_id
/// no-op（纯观测，无需 fail-visible）。
pub fn record_readjudicated(law_id: &str) {
    let mut ledger = readjudication_ledger()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(entry) = ledger.entries.get_mut(law_id) else {
        return;
    };
    if entry.level >= entry.height {
        return;
    }
    entry.level += 1;
    let key = domain_key(entry.domain.as_deref());
    if let Some(phi) = ledger.phi_by_domain.get_mut(&key) {
        *phi = phi.saturating_sub(1);
        if *phi == 0 {
            ledger.phi_by_domain.remove(&key);
        }
    }
    ledger.pending_total = ledger.pending_total.saturating_sub(1);
}

/// 当前按域待裁位势（Φ_dom = Σ_p (h_p − lvl_p)；零值域不出现在 map）。
pub fn phi_by_domain() -> BTreeMap<String, u64> {
    readjudication_ledger()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .phi_by_domain
        .clone()
}

/// 全局待裁总数（Σ(h_p − lvl_p)，与 [`phi_by_domain`] 按域分账同一账本
/// 两视图）。
pub fn pending_readjudication_total() -> u64 {
    readjudication_ledger()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .pending_total
}

// ---------------------------------------------------------------------------
// 振荡报警（件④：停滞域判定与一次报警；纯观测零拦截）
// ---------------------------------------------------------------------------

/// 停滞窗数阈值：某域 pending>0 且 Φ 连续 N 窗全等 = 停滞（报警一次）。
pub const STALL_WINDOWS: u32 = 2;

/// 停滞判定态（进程级共享，结算循环每窗推进一次）。
#[derive(Debug, Default)]
struct StallState {
    /// domain → 上一窗 Φ（缺席 = 零/无基线）。
    last_phi: BTreeMap<String, u64>,
    /// domain → 当前正值 Φ 持续窗数（首见窗立基线计 1，全等递增）。
    stagnation_windows: BTreeMap<String, u32>,
    /// 当前停滞域集合（解除即移除可复位；在集即一次报警守卫——持续停滞
    /// 不重复累报）。
    stalled: std::collections::BTreeSet<String>,
}

static STALL_STATE: std::sync::OnceLock<Mutex<StallState>> = std::sync::OnceLock::new();

fn stall_state() -> &'static Mutex<StallState> {
    STALL_STATE.get_or_init(|| Mutex::new(StallState::default()))
}

/// 结算点停滞判定（评估循环每窗调用一次；纯观测零拦截）。返回本窗末
/// 停滞域集合（view.stall_domains 契约面）。
///
/// 逐域判定：pending>0 即 Φ_dom>0（按域分账与全域总数同一账本两视图，
/// 域 pending>0 ⟺ Φ_dom>0）且 Φ 跨窗保持全等。正值首见窗立基线计 1，
/// 其后每窗全等递增，计数达 [`STALL_WINDOWS`] 即停滞并经 [`record_alert`]
/// 报警一次（计入窗警量账，既有 certified-streak/相位机自然响应）；Φ
/// 变化或归零（pending 归零）= 停滞解除，计数复位并出集（解除后再停滞
/// 可再报）。
pub(crate) fn settle_stall_domains(phi: &BTreeMap<String, u64>) -> Vec<String> {
    let mut st = stall_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // 基线域 ∪ 本窗域并集，BTreeMap 确定序（重放可复算）。
    let mut domains: Vec<String> = st.last_phi.keys().cloned().collect();
    for k in phi.keys() {
        if !st.last_phi.contains_key(k) {
            domains.push(k.clone());
        }
    }
    for d in domains {
        let cur = phi.get(&d).copied().unwrap_or(0);
        let last = st.last_phi.get(&d).copied().unwrap_or(0);
        let count = if cur > 0 && cur == last {
            st.stagnation_windows.get(&d).copied().unwrap_or(0) + 1
        } else if cur > 0 {
            // 正值但与上一窗不等（或首见）：以新值立基线计 1。
            1
        } else {
            0
        };
        if count == 0 {
            // 停滞解除（Φ 归零 = pending 归零）：全量复位。
            st.stagnation_windows.remove(&d);
            st.stalled.remove(&d);
            st.last_phi.remove(&d);
        } else {
            if cur != last {
                // Φ 变化：旧停滞出集复位，新值重立基线。
                st.stalled.remove(&d);
            }
            st.stagnation_windows.insert(d.clone(), count);
            st.last_phi.insert(d.clone(), cur);
            if count >= STALL_WINDOWS && st.stalled.insert(d.clone()) {
                // 新停滞域报警一次（持续停滞在集不重复累报）。
                record_alert();
            }
        }
    }
    st.stalled.iter().cloned().collect()
}

/// 停滞域吸收相否决门（结算点相位标签处加门；相位机只加不改）：stalled
/// 域的 absorbing 判定被否决回落 relaxing，crisis/relaxing 照常推算
/// （否决门只压"深稳态"误判，不改其余两态推算；停滞域不在相位 map
/// 不虚造条目）。
pub(crate) fn veto_stalled_absorbing(
    domain_phases: &mut BTreeMap<String, String>,
    stalled: &[String],
) {
    for d in stalled {
        if domain_phases.get(d).map(String::as_str) == Some("absorbing") {
            domain_phases.insert(d.clone(), "relaxing".to_string());
        }
    }
}

/// 三层相位复合（结算原子点确定性纯函数；status 呈现面与系统级裁决
/// 发射共用同一派生，呈现层零二次派生）。三层各自独立三态标签：
/// - `capability` 层 = 集级相位（窗结算 [`phase_label`] 结果原样透传）；
/// - `rules` 层 = 域级相位复合：任一域 crisis ⇒ crisis；全域 absorbing
///   （含空域集）⇒ absorbing；否则 relaxing；
/// - `data` 层 = 位势进度面：位势总和归零 ⇒ absorbing；位势存在且相对
///   上一结算窗不降（新扰动或排空停滞）⇒ crisis；位势存在且逐窗下降
///   （排空中）⇒ relaxing。（待裁账与位势账同源同排空：单立"待裁>0"
///   危机带会与吸收态粘连出跳步——真机实测 phi 与 pending 同窗归零，
///   故 crisis/relaxing 以进度参照上一窗位势切分。）
///
/// 返回 map 键序 BTreeMap 确定性（`capability`/`data`/`rules`）。
pub(crate) fn compose_layer_phases(
    capability: &str,
    domain_phases: &BTreeMap<String, String>,
    pending_total: u64,
    phi_total: u64,
    prev_phi_total: u64,
) -> BTreeMap<String, String> {
    let rules = if domain_phases.values().any(|p| p == "crisis") {
        "crisis"
    } else if domain_phases.values().all(|p| p == "absorbing") {
        "absorbing"
    } else {
        "relaxing"
    };
    let data = if phi_total == 0 {
        "absorbing"
    } else if phi_total >= prev_phi_total {
        "crisis"
    } else {
        "relaxing"
    };
    let mut layers = BTreeMap::new();
    layers.insert("capability".to_string(), capability.to_string());
    layers.insert("rules".to_string(), rules.to_string());
    layers.insert("data".to_string(), data.to_string());
    layers
}

/// 测试隔离辅助（进程级共享态先到先得坑的复位面，仅测试构建内编译；
/// 调用方须先取本文件测试串行锁防互串账）。
#[cfg(test)]
pub(crate) fn reset_readjudication_state() {
    *readjudication_ledger()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = ReadjudicationLedger::default();
    *stall_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = StallState::default();
}

// ---------------------------------------------------------------------------
// 均衡态视图（评估任务发布，status 呈现面取用）
// ---------------------------------------------------------------------------

/// 最近一次窗结算后的均衡态呈现快照。
///
/// `stable = None` = 反应器不可达或读回超时（fail-visible：不猜稳态，
/// 呈现层原样透出 null）。
///
/// `phase` = 均衡相位标签（`crisis`/`relaxing`/`absorbing`），在窗结算
/// 原子点以本窗实值计算（尾窗警量/判据门槛/认证连击三输入同源快照，
/// 与结算记录同源可回放；status 呈现层读缓存不做二次派生）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EquilibriumView {
    pub stable: Option<bool>,
    pub last_cause: Option<String>,
    pub window_secs: u64,
    pub in_rate: i64,
    pub digest_rate: i64,
    pub phase: &'static str,
    /// 域级相位（只加不改：每域独立三态标签，域 id → `crisis`/`relaxing`/
    /// `absorbing`）。每域以自身窗警量尾部独立 [`phase_label`]；认证
    /// streak 属集级——全局认证连击全域共用同一输入（域账只分警量不分
    /// 认证，该裁定为准）。空域账（尾部无窗）不出现在 map。stalled 域的
    /// absorbing 判定在结算点被否决门回落 relaxing（见
    /// [`veto_stalled_absorbing`]）。
    pub domain_phases: BTreeMap<String, String>,
    /// 按域待裁位势（Φ_dom = Σ_p (h_p − lvl_p)；零值域不出现）。结算
    /// 原子点快照（publish_view 前），与相位同源可回放。
    pub phi_by_domain: BTreeMap<String, u64>,
    /// 全局待裁总数（Σ(h_p − lvl_p)，与按域分账同一账本两视图）。
    pub pending_readjudication: u64,
    /// 本窗末停滞域集合（pending>0 且 Φ 连续 [`STALL_WINDOWS`] 窗全等；
    /// 件④振荡报警面）。
    pub stall_domains: Vec<String>,
    /// 三层相位复合（只加不改新键：键 `capability`/`rules`/`data` 各自
    /// 独立三态标签，键序 BTreeMap 确定性）。capability 层 = 集级相位，
    /// rules 层 = 域级相位复合，data 层 = 待裁位势面。
    pub layer_phases: BTreeMap<String, String>,
    /// 系统级相位标签（三层复合经规则层裁决读回，`crisis`/`relaxing`/
    /// `absorbing`；读回失败回落 `relaxing`，不猜）。
    pub system_phase: String,
    /// 系统级均衡布尔（= 读回 `system_phase` 是否 `absorbing`）。
    pub system_balanced: bool,
    /// 三层位势总和（Σ域 Φ，与按域分账同一账本两视图；结算原子点快照）。
    pub phi_total: u64,
}

static VIEW: Mutex<EquilibriumView> = Mutex::new(EquilibriumView {
    stable: None,
    last_cause: None,
    window_secs: 0,
    in_rate: 0,
    digest_rate: 0,
    phase: "relaxing",
    domain_phases: BTreeMap::new(),
    phi_by_domain: BTreeMap::new(),
    pending_readjudication: 0,
    stall_domains: Vec::new(),
    layer_phases: BTreeMap::new(),
    system_phase: String::new(),
    system_balanced: false,
    phi_total: 0,
});

/// 发布最近窗结算视图（评估任务内部调用）。
fn publish_view(v: EquilibriumView) {
    *VIEW
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = v;
}

/// 最近窗结算视图（status 呈现面取用）。
pub fn view() -> EquilibriumView {
    VIEW.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

// ---------------------------------------------------------------------------
// 窗结算评估任务（发射方装配：独立 tokio task，纯观测零拦截）
// ---------------------------------------------------------------------------

/// 评估任务配置（env 可调=装配面缺省常量；设了但非法 fail-fast 拒绝启动）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EquilibriumEvaluatorConfig {
    /// 观测窗长（秒）。
    pub window_secs: u64,
    /// 判据②阈值：连续无警窗数下限。
    pub alert_free_windows_required: i64,
    /// 判据③阈值：验证覆盖率下限（千分比）。
    pub coverage_min: i64,
    /// 判据④阈值：连续 stable 认证次数下限。
    pub certified_streak_min: i64,
}

impl Default for EquilibriumEvaluatorConfig {
    /// 判据阈值缺省值：复合冲击实验定标维持（2026-11-09 实验纪实：恢复段
    /// 实测无警窗 2-3、三轮吸收相于 streak=3 一致收敛、深稳态零假警；
    /// 覆盖率记账面在真机形态下产出缺位——该判据门限数据待记账链勘通后
    /// 复核，登记在案）。每条取值均可溯实验数据锚，非拍脑袋预设。
    fn default() -> Self {
        Self {
            window_secs: 60,
            alert_free_windows_required: 3,
            coverage_min: 900,
            certified_streak_min: 3,
        }
    }
}

impl EquilibriumEvaluatorConfig {
    /// env 装配（未设=缺省；设了但非法=显式报错，不猜不静默回落）。
    pub fn from_env() -> Result<Self, String> {
        fn parse_u64(key: &str) -> Result<Option<u64>, String> {
            match std::env::var(key) {
                Ok(v) => v
                    .trim()
                    .parse::<u64>()
                    .map(Some)
                    .map_err(|_| format!("{key} invalid (expected non-negative integer): {v}")),
                Err(std::env::VarError::NotPresent) => Ok(None),
                Err(e) => Err(format!("{key} unreadable: {e}")),
            }
        }
        fn parse_i64(key: &str) -> Result<Option<i64>, String> {
            match std::env::var(key) {
                Ok(v) => v
                    .trim()
                    .parse::<i64>()
                    .map(Some)
                    .map_err(|_| format!("{key} invalid (expected integer): {v}")),
                Err(std::env::VarError::NotPresent) => Ok(None),
                Err(e) => Err(format!("{key} unreadable: {e}")),
            }
        }
        Ok(Self {
            window_secs: parse_u64("EVORULE_EQUILIBRIUM_WINDOW_SECS")?.unwrap_or(60),
            alert_free_windows_required: parse_i64(
                "EVORULE_EQUILIBRIUM_ALERT_FREE_WINDOWS_REQUIRED",
            )?
            .unwrap_or(3),
            coverage_min: parse_i64("EVORULE_EQUILIBRIUM_COVERAGE_MIN")?.unwrap_or(900),
            certified_streak_min: parse_i64("EVORULE_EQUILIBRIUM_CERTIFIED_STREAK_MIN")?
                .unwrap_or(3),
        })
    }
}

// ---------------------------------------------------------------------------
// 特征定义表（registry 形态：特征 = 确定性解析公式，禁黑箱函数）
// ---------------------------------------------------------------------------

/// 窗口语义：特征量相对观测窗的取数方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowSemantics {
    /// 窗内瞬时值（窗结算时点取样或由观测面随取随存）。
    Instant,
    /// 跨窗累计值（进程生命周期单调计数）。
    Cumulative,
    /// 窗间差分（相邻窗结算点之间的增量）。
    Diff,
    /// 连计（逐窗递增，触发重置事件归零）。
    Streak,
}

/// 适用域声明（域外显式拒域 → undetermined 出口，fail-visible 不猜）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeatureRegime {
    /// 适用域描述（域外取值按 `undetermined_outside` 弃权）。
    pub domain: &'static str,
    /// 量化步长（登记面整数单位；1 = 原生整数步长）。
    pub quant_step: i64,
    /// 域外是否显式弃权（undetermined 出口）。
    pub undetermined_outside: bool,
}

/// 均衡观测特征定义条目（形态对齐 formula_registry：解析公式入册）。
///
/// 八字段 schema：特征标识 / 解析公式 / 七元量纲指纹 / 窗口语义 /
/// 适用域 / 误差契约 / 清零语义 / 复算凭证锚。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EquilibriumFeatureSpec {
    /// 稳定特征标识（立法面与呈现面引用此 id）。
    pub feature_id: &'static str,
    /// 解析公式（`lhs = rhs` 形态；rhs 仅用已知量纲变量与四则/幂运算，
    /// 可被量纲服务确定性解析验证）。
    pub formula_ref: &'static str,
    /// 七元量纲指纹 [L,M,T,I,Θ,N,J]（无量纲 = 全零）。
    pub dim_vector: [i64; 7],
    /// 窗口语义。
    pub window_semantics: WindowSemantics,
    /// 适用域声明。
    pub regime: FeatureRegime,
    /// 误差契约（精确语义 / 饱和行为 / fail-visible 出口）。
    pub error_contract: &'static str,
    /// 清零语义（非连计类条目 = 显式「不适用」声明）。
    pub reset_semantics: &'static str,
    /// 复算凭证锚声明（输入观测流 + 公式版本戳 + 输出可复算）。
    pub evidence_anchor: &'static str,
}

/// 复算凭证锚通用声明（首批特征同源：输入=窗口观测流增量哈希链头——
/// 每窗结算时以本窗观测记录推进的 evidence anchor，genesis 起链；
/// 公式=formula_ref 版本戳；输出=观测流重放下确定性可复算）。
const EVIDENCE_ANCHOR_WINDOW_STREAM: &str =
    "input: incremental hash-chain head over the windowed observation stream \
     (advanced at every settlement with the full observation record, \
     genesis-anchored); formula: formula_ref version stamp; value: \
     deterministic under observation stream replay";

/// 观测流证据链 genesis 哨兵（首窗 prev_anchor；上下文快照链 genesis
/// 先例同型——首窗之前无观测，链从固定哨兵起算）。
const EVIDENCE_ANCHOR_GENESIS: &str = "genesis";

/// 均衡观测特征定义表（首批 = 窗评估任务现提取的全部特征量，自硬编码
/// 口径原样平移；值语义逐项不变，对外发射与呈现面契约冻结）。
///
/// 发射名对照：立法参数 `coverage` 对应注册特征 `coverage_ratio`
/// （发射面参数名冻结，注册 id 为分析名）。
pub const EQUILIBRIUM_FEATURES: &[EquilibriumFeatureSpec] = &[
    EquilibriumFeatureSpec {
        feature_id: "proposals_in_total",
        formula_ref: "proposals_in_total = event_count",
        dim_vector: [0, 0, 0, 0, 0, 0, 0],
        window_semantics: WindowSemantics::Cumulative,
        regime: FeatureRegime {
            domain: "non-negative integers within the u64 intake counter range",
            quant_step: 1,
            undetermined_outside: true,
        },
        error_contract: "exact integer accounting; the u64 counter is monotonic and never \
             reset within a process lifetime",
        reset_semantics: "not applicable (cumulative counter, no streak state)",
        evidence_anchor: EVIDENCE_ANCHOR_WINDOW_STREAM,
    },
    EquilibriumFeatureSpec {
        feature_id: "digested_total",
        formula_ref: "digested_total = event_count",
        dim_vector: [0, 0, 0, 0, 0, 0, 0],
        window_semantics: WindowSemantics::Cumulative,
        regime: FeatureRegime {
            domain: "non-negative integers within the u64 digestion counter range",
            quant_step: 1,
            undetermined_outside: true,
        },
        error_contract: "exact integer accounting; the u64 counter is monotonic and never \
             reset within a process lifetime",
        reset_semantics: "not applicable (cumulative counter, no streak state)",
        evidence_anchor: EVIDENCE_ANCHOR_WINDOW_STREAM,
    },
    EquilibriumFeatureSpec {
        feature_id: "in_rate",
        formula_ref: "in_rate = event_count / s",
        dim_vector: [0, 0, -1, 0, 0, 0, 0],
        window_semantics: WindowSemantics::Diff,
        regime: FeatureRegime {
            domain: "non-negative integer differentials over one settlement window",
            quant_step: 1,
            undetermined_outside: true,
        },
        error_contract: "exact saturating differential of the u64 intake counter between \
             consecutive settlements; counter saturation degrades magnitude, \
             never sign",
        reset_semantics: "not applicable (per-window differential, no streak state)",
        evidence_anchor: EVIDENCE_ANCHOR_WINDOW_STREAM,
    },
    EquilibriumFeatureSpec {
        feature_id: "digest_rate",
        formula_ref: "digest_rate = event_count / s",
        dim_vector: [0, 0, -1, 0, 0, 0, 0],
        window_semantics: WindowSemantics::Diff,
        regime: FeatureRegime {
            domain: "non-negative integer differentials over one settlement window",
            quant_step: 1,
            undetermined_outside: true,
        },
        error_contract: "exact saturating differential of the u64 digestion counter between \
             consecutive settlements; counter saturation degrades magnitude, \
             never sign",
        reset_semantics: "not applicable (per-window differential, no streak state)",
        evidence_anchor: EVIDENCE_ANCHOR_WINDOW_STREAM,
    },
    EquilibriumFeatureSpec {
        feature_id: "net_intake_rate",
        formula_ref: "net_intake_rate = in_rate - digest_rate",
        dim_vector: [0, 0, -1, 0, 0, 0, 0],
        window_semantics: WindowSemantics::Diff,
        regime: FeatureRegime {
            domain: "integers within i64 (each operand is a non-negative \
                     window differential)",
            quant_step: 1,
            undetermined_outside: true,
        },
        error_contract: "exact integer difference of in_rate and digest_rate (saturating \
             at the i64 bound); both operands carry the differential contract \
             above",
        reset_semantics: "not applicable (derived quantity, no streak state)",
        evidence_anchor: EVIDENCE_ANCHOR_WINDOW_STREAM,
    },
    EquilibriumFeatureSpec {
        feature_id: "window_alert_count",
        formula_ref: "window_alert_count = event_count",
        dim_vector: [0, 0, 0, 0, 0, 0, 0],
        window_semantics: WindowSemantics::Instant,
        regime: FeatureRegime {
            domain: "non-negative integers within a single settlement window",
            quant_step: 1,
            undetermined_outside: true,
        },
        error_contract: "exact take-and-clear drain of the pending-alert counter at window \
             settlement; concurrent records are never lost (atomic add / swap)",
        reset_semantics: "drained to zero at every window settlement (take-and-clear)",
        evidence_anchor: EVIDENCE_ANCHOR_WINDOW_STREAM,
    },
    EquilibriumFeatureSpec {
        feature_id: "alert_free_windows",
        formula_ref: "alert_free_windows = window_count",
        dim_vector: [0, 0, 0, 0, 0, 0, 0],
        window_semantics: WindowSemantics::Streak,
        regime: FeatureRegime {
            domain: "non-negative integers (process-lifetime streak)",
            quant_step: 1,
            undetermined_outside: true,
        },
        error_contract: "exact integer streak; a window settling with window_alert_count > 0 \
             resets the streak before the next increment",
        reset_semantics: "reset to 0 when a window settles with window_alert_count > 0; \
             otherwise incremented by 1 per window",
        evidence_anchor: EVIDENCE_ANCHOR_WINDOW_STREAM,
    },
    EquilibriumFeatureSpec {
        feature_id: "coverage_ratio",
        formula_ref: "coverage_ratio = coverage_per_mille",
        dim_vector: [0, 0, 0, 0, 0, 0, 0],
        window_semantics: WindowSemantics::Instant,
        regime: FeatureRegime {
            domain: "non-negative per-mille integers as handed over by the \
                     verification face (target range 0..=1000)",
            quant_step: 1,
            undetermined_outside: true,
        },
        error_contract: "carried verbatim from the verification face (no arithmetic here); \
             absent coverage is fail-visible (fail-closed verdict, never \
             guessed stable)",
        reset_semantics: "not applicable (instant snapshot, no streak state)",
        evidence_anchor: EVIDENCE_ANCHOR_WINDOW_STREAM,
    },
    EquilibriumFeatureSpec {
        feature_id: "certified_streak",
        formula_ref: "certified_streak = certificate_count",
        dim_vector: [0, 0, 0, 0, 0, 0, 0],
        window_semantics: WindowSemantics::Streak,
        regime: FeatureRegime {
            domain: "non-negative integers (process-lifetime streak)",
            quant_step: 1,
            undetermined_outside: true,
        },
        error_contract: "exact integer streak; any certificate verdict other than stable \
             resets the streak",
        reset_semantics: "reset to 0 on a non-stable certificate outcome; incremented by 1 \
             on each stable certificate",
        evidence_anchor: EVIDENCE_ANCHOR_WINDOW_STREAM,
    },
    // ── 相变刻画语言（弛豫时间 τ 序列统计：临界慢化度量族）──────────
    // 三条统一声明：均为描述性度量，判定阈值不预设（诚实边界：只做
    // 度量与描述性标签，不做判定立法——危机/临界判定阈值候选实验数据
    // 定标）。计算入口 = 实验/分析面对 τ 序列拉取
    // （[`extract_relaxation_series`]），非窗结算内联发射——发射面契约
    // 冻结不动，故不在 [`FeatureValues`] / [`extract_feature_values`]
    // 提取映射内（登记面 = 窗结算提取面 ∪ 本族分析面派生量，双向守卫
    // 见 tests）。τ 为回合窗计数：全部无量纲，regime 同型（整数步长 +
    // 域外显式弃权）。窗口语义按最贴切既有变体登记：一阶差分 = Diff
    // （相邻回合差分，与「窗间差分」同构）；滑窗方差 / lag-1 自相关 =
    // Instant（分析时点对末 k=3 滑窗的瞬时快照统计）。
    EquilibriumFeatureSpec {
        feature_id: "tau_first_difference",
        formula_ref: "tau_first_difference = tau_i - tau_i_prev",
        dim_vector: [0, 0, 0, 0, 0, 0, 0],
        window_semantics: WindowSemantics::Diff,
        regime: FeatureRegime {
            domain: "integers within i64 (each tau is a non-negative episode window count)",
            quant_step: 1,
            undetermined_outside: true,
        },
        error_contract: "exact i64 difference of the last two relaxation times (positive \
             trend = critical slowing down signal); a series shorter than two \
             episodes is undetermined (fail-visible exit)",
        reset_semantics: "not applicable (per-episode derived quantity, no streak state)",
        evidence_anchor: EVIDENCE_ANCHOR_WINDOW_STREAM,
    },
    EquilibriumFeatureSpec {
        feature_id: "tau_sliding_variance",
        formula_ref: "tau_sliding_variance = (tau_i - tau_j)**2 + (tau_i - tau_k)**2 + (tau_j - tau_k)**2",
        dim_vector: [0, 0, 0, 0, 0, 0, 0],
        window_semantics: WindowSemantics::Instant,
        regime: FeatureRegime {
            domain: "non-negative integers within i128 product range then saturating to i64",
            quant_step: 1,
            undetermined_outside: true,
        },
        error_contract: "exact integer sum of pairwise squared differences over the fixed \
             k=3 sliding window (equals k^2 * variance, integer-exact and \
             monotonically equivalent to variance at fixed k); a series shorter \
             than the window is undetermined; saturation degrades magnitude, \
             never sign",
        reset_semantics: "not applicable (per-analysis derived quantity, no streak state)",
        evidence_anchor: EVIDENCE_ANCHOR_WINDOW_STREAM,
    },
    EquilibriumFeatureSpec {
        feature_id: "tau_autocorrelation",
        formula_ref: "tau_autocorrelation = (tau_1 - mean_prev) * (tau_2 - mean_next) + (tau_2 - mean_prev) * (tau_3 - mean_next)",
        dim_vector: [0, 0, 0, 0, 0, 0, 0],
        window_semantics: WindowSemantics::Instant,
        regime: FeatureRegime {
            domain: "non-negative integers within i128 product range then saturating to i64",
            quant_step: 1,
            undetermined_outside: true,
        },
        error_contract: "registered value = lag-1 autocorrelation numerator over the fixed \
             k=3 window (exact integer); the ratio is recovered via the paired \
             denominator exposed by the same accessor returning (numerator, \
             denominator); the ratio is undetermined when the denominator is \
             zero (constant series) and the exact integer form is undetermined \
             when the exact rational numerator/denominator is half-integer",
        reset_semantics: "not applicable (per-analysis derived quantity, no streak state)",
        evidence_anchor: EVIDENCE_ANCHOR_WINDOW_STREAM,
    },
    // ── 相变刻画语言（恢复时间 τ_recover 序列统计：第二弛豫度量双轨）──
    // 现行 τ 定义结构性恒等于判据门槛（回合闭合点=streak 达标，τ 恒为
    // 常数序列，慢化统计特征真机数据恒平凡）——本族按恢复回合独立定义：
    // 回合=非零警窗开启、闭合=首个 stable 认证窗，τ_recover=实际窗数，
    // 携带弛豫快慢信息（复合冲击真机实验实测恢复序列非常数，见实验纪
    // 实 2026-11-09）。计算入口 = 实验/分析面对双流拉取
    // （[`extract_recover_series`]），统计特征复用同型纯函数
    // （[`tau_first_difference`] / [`tau_sliding_variance`] /
    // [`tau_autocorrelation`]），非窗结算内联发射——发射面契约冻结
    // 不动，不在 [`FeatureValues`] / [`extract_feature_values`] 提取
    // 映射内（登记面 = 窗结算提取面 ∪ 本族分析面派生量，双向守卫见
    // tests）。三条 regime/error_contract/anchor 声明与 τ 族逐条同型。
    EquilibriumFeatureSpec {
        feature_id: "tau_recover_first_difference",
        formula_ref: "tau_recover_first_difference = tau_recover_i - tau_recover_i_prev",
        dim_vector: [0, 0, 0, 0, 0, 0, 0],
        window_semantics: WindowSemantics::Diff,
        regime: FeatureRegime {
            domain: "integers within i64 (each tau_recover is a non-negative episode \
                     window count)",
            quant_step: 1,
            undetermined_outside: true,
        },
        error_contract: "exact i64 difference of the last two recovery times (positive \
             trend = critical slowing down signal); a series shorter than two \
             episodes is undetermined (fail-visible exit)",
        reset_semantics: "not applicable (per-episode derived quantity, no streak state)",
        evidence_anchor: EVIDENCE_ANCHOR_WINDOW_STREAM,
    },
    EquilibriumFeatureSpec {
        feature_id: "tau_recover_sliding_variance",
        formula_ref: "tau_recover_sliding_variance = (tau_recover_i - tau_recover_j)**2 + (tau_recover_i - tau_recover_k)**2 + (tau_recover_j - tau_recover_k)**2",
        dim_vector: [0, 0, 0, 0, 0, 0, 0],
        window_semantics: WindowSemantics::Instant,
        regime: FeatureRegime {
            domain: "non-negative integers within i128 product range then saturating to i64",
            quant_step: 1,
            undetermined_outside: true,
        },
        error_contract: "exact integer sum of pairwise squared differences over the fixed \
             k=3 sliding window of the recovery-time series (equals k^2 * variance, \
             integer-exact and monotonically equivalent to variance at fixed k); a \
             series shorter than the window is undetermined; saturation degrades \
             magnitude, never sign",
        reset_semantics: "not applicable (per-analysis derived quantity, no streak state)",
        evidence_anchor: EVIDENCE_ANCHOR_WINDOW_STREAM,
    },
    EquilibriumFeatureSpec {
        feature_id: "tau_recover_autocorrelation",
        formula_ref: "tau_recover_autocorrelation = (tau_recover_1 - mean_prev) * (tau_recover_2 - mean_next) + (tau_recover_2 - mean_prev) * (tau_recover_3 - mean_next)",
        dim_vector: [0, 0, 0, 0, 0, 0, 0],
        window_semantics: WindowSemantics::Instant,
        regime: FeatureRegime {
            domain: "non-negative integers within i128 product range then saturating to i64",
            quant_step: 1,
            undetermined_outside: true,
        },
        error_contract: "registered value = lag-1 autocorrelation numerator over the fixed \
             k=3 window of the recovery-time series (exact integer); the ratio is \
             recovered via the paired denominator exposed by the same accessor \
             returning (numerator, denominator); the ratio is undetermined when \
             the denominator is zero (constant series) and the exact integer form \
             is undetermined when the exact rational numerator/denominator is \
             half-integer",
        reset_semantics: "not applicable (per-analysis derived quantity, no streak state)",
        evidence_anchor: EVIDENCE_ANCHOR_WINDOW_STREAM,
    },
];

/// 一次窗结算提取的全部注册特征值（命名字段与 [`EQUILIBRIUM_FEATURES`]
/// 条目一一对应；发射面消费其中五项，`to_map` 供定义表对账与立法面按
/// id 取用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeatureValues {
    pub proposals_in_total: i64,
    pub digested_total: i64,
    pub in_rate: i64,
    pub digest_rate: i64,
    pub net_intake_rate: i64,
    pub window_alert_count: i64,
    pub alert_free_windows: i64,
    pub coverage_ratio: i64,
    pub certified_streak: i64,
}

impl FeatureValues {
    /// 注册特征值映射（id → 值；键集合与定义表一致，双向完备性由测试锁定）。
    pub fn to_map(&self) -> std::collections::BTreeMap<&'static str, i64> {
        use std::collections::BTreeMap;

        let mut values = BTreeMap::new();
        values.insert("proposals_in_total", self.proposals_in_total);
        values.insert("digested_total", self.digested_total);
        values.insert("in_rate", self.in_rate);
        values.insert("digest_rate", self.digest_rate);
        values.insert("net_intake_rate", self.net_intake_rate);
        values.insert("window_alert_count", self.window_alert_count);
        values.insert("alert_free_windows", self.alert_free_windows);
        values.insert("coverage_ratio", self.coverage_ratio);
        values.insert("certified_streak", self.certified_streak);
        values
    }
}

/// 特征量提取：按定义表口径从窗快照与窗警排水量计算全部注册特征值。
/// 各项算术与平移前硬编码口径逐点一致。
pub fn extract_feature_values(
    prev: &WindowsSnapshot,
    after: &WindowsSnapshot,
    window_alert_count: u64,
) -> FeatureValues {
    let in_rate = after
        .proposals_in_total
        .saturating_sub(prev.proposals_in_total) as i64;
    let digest_rate = after.digested_total.saturating_sub(prev.digested_total) as i64;
    FeatureValues {
        proposals_in_total: after.proposals_in_total as i64,
        digested_total: after.digested_total as i64,
        in_rate,
        digest_rate,
        net_intake_rate: in_rate.saturating_sub(digest_rate),
        window_alert_count: window_alert_count as i64,
        alert_free_windows: after.alert_free_windows as i64,
        coverage_ratio: after.last_coverage,
        certified_streak: after.certified_streak as i64,
    }
}

// ---------------------------------------------------------------------------
// 相变刻画语言（τ 序列分析面：弛豫时间统计特征与相标签）
// ---------------------------------------------------------------------------

/// 滑动方差 / lag-1 自相关共用的固定滑窗长度（k = 3；k 固定保证整数
/// 等价形与重放确定性）。
const TAU_SLIDING_WINDOW: usize = 3;

/// 弛豫回合序列提取：从逐窗告警计数流提取每回合弛豫时间 τ——
/// 一段连续零告警窗在 streak 达到 required（认证事件）时闭合一个回合，
/// τ = 该回合 streak 达标所需窗数（闭合时点 c == required，故 τ 恒等于
/// required）；回合以非零告警窗（或流首）开启，回合闭合后 streak 立即
/// 归零重新起算（同段持续零告警可连续闭合多个回合），窗告警 > 0 将
/// streak 清零。
///
/// 确定性：纯函数（无时钟、无随机源、无外部状态），同一观测流重放必得
/// 同序列——与证据锚的观测流重放口径同源。
///
/// 诚实边界：流结束时 streak 未达 required 的尾段回合不产出 τ（未认证
/// 回合不进序列）；required = 0 = 无判据，弛豫不定义 → 空序列。
pub fn extract_relaxation_series(alert_counts: &[u64], required: u64) -> Vec<u64> {
    let mut series = Vec::new();
    if required == 0 {
        return series;
    }
    let mut streak: u64 = 0;
    for &count in alert_counts {
        if count == 0 {
            streak += 1;
            if streak == required {
                series.push(streak);
                streak = 0;
            }
        } else {
            streak = 0;
        }
    }
    series
}

/// recover 弛豫回合序列提取（第二弛豫度量双轨，只加不改——现行
/// [`extract_relaxation_series`] 语义与黄金钉零动）：从逐窗告警计数流 +
/// 逐窗 stable 认证数流提取每回合恢复时间 τ_recover——回合以非零告警窗
/// 开启（回合已开启时持续告警不重开），以**首个 stable 认证窗**闭合，
/// τ_recover = 回合开启窗到认证窗的实际窗数（携带弛豫快慢信息：现行 τ
/// 定义下 τ ≡ required 为结构性常数序列，临界慢化统计特征恒平凡——
/// 真机复合冲击实验实测恢复序列非常数，为慢化度量的有效输入）。
///
/// 回合闭合后立即重新起算：闭合窗自身告警计数 > 0 时同窗开启新回合
/// （告警与认证同窗：先闭合后开启）。无警窗段的认证不产出回合（回合
/// 未开启）；流结束时未闭合的尾段回合不产出（未认证回合不进序列，
/// 与现行 τ 的诚实边界同型）。
///
/// 确定性：纯函数（无时钟、无随机源、无外部状态），同一观测流对重放
/// 必得同序列——与证据锚的观测流重放口径同源。两流长度不齐时按较短
/// 流截断（fail-visible 不猜缺失侧）。
pub fn extract_recover_series(alert_counts: &[u64], cert_counts: &[u64]) -> Vec<u64> {
    let mut series = Vec::new();
    let mut open: Option<usize> = None;
    let n = alert_counts.len().min(cert_counts.len());
    for i in 0..n {
        if alert_counts[i] > 0 && open.is_none() {
            open = Some(i);
        }
        if let Some(start) = open {
            if cert_counts[i] > 0 {
                series.push((i - start + 1) as u64);
                open = None;
                if alert_counts[i] > 0 {
                    open = Some(i);
                }
            }
        }
    }
    series
}

/// 相变刻画语言 τ 一阶差分（相邻回合差分）：弛豫时间序列趋势为正 =
/// 临界慢化的初级信号（回归均衡越来越慢）。
///
/// 返回末两元素差 `τ_n − τ_{n−1}`（精确 i64 差分；两元素均在声明的
/// i64 域内时差分必不溢出）。诚实边界：描述性度量，判定阈值不预设；
/// 取值入口 = 实验/分析面对 τ 序列拉取（[`extract_relaxation_series`]），
/// 非窗结算内联发射（发射面契约冻结不动）。
///
/// undetermined 出口（fail-visible 不猜）：序列不足两回合；τ 值超出
/// 声明的 i64 域（域外弃权，见登记面 regime）。
pub fn tau_first_difference(series: &[u64]) -> Option<i64> {
    let n = series.len();
    if n < 2 {
        return None;
    }
    let prev = i64::try_from(series[n - 2]).ok()?;
    let last = i64::try_from(series[n - 1]).ok()?;
    Some(last - prev)
}

/// 相变刻画语言 τ 滑动方差（末 k=3 窗）：临界慢化核心度量——弛豫时间
/// 波动随相变临近而放大。
///
/// 登记值取整数恒等形：`Σ_{i<j}(τ_i − τ_j)²`（k 点恒等式
/// `Σ_{i<j}(x_i − x_j)² = k²·variance`；k 固定 = [`TAU_SLIDING_WINDOW`]
/// 时与方差单调等价）——避免均值非整时平方偏差和出现有理数、破坏注册
/// 表整数量化契约（quant_step = 1）。中间量走 i128 乘积域，饱和退化
/// 幅值不改号（平方和恒非负），最终饱和到 i64。
///
/// 诚实边界：描述性度量，判定阈值不预设；取值入口 = 分析面 τ 序列
/// 拉取，非窗结算内联发射。序列不足 k=3 → None（fail-visible）。
pub fn tau_sliding_variance(series: &[u64]) -> Option<i64> {
    let n = series.len();
    if n < TAU_SLIDING_WINDOW {
        return None;
    }
    let window = &series[n - TAU_SLIDING_WINDOW..];
    let mut acc: i128 = 0;
    for i in 0..TAU_SLIDING_WINDOW {
        for j in (i + 1)..TAU_SLIDING_WINDOW {
            let d = i128::from(window[i]) - i128::from(window[j]);
            acc = acc.saturating_add(d.saturating_mul(d));
        }
    }
    Some(acc.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
}

/// 相变刻画语言 τ lag-1 自相关（末 k=3 窗）：相邻回合弛豫时间相关性
/// 随相变临近增强的信号。
///
/// 返回 `(分子, 分母)` 整数对：分子 = `Σ(τ_i − μ_prev)(τ_{i+1} − μ_next)`
/// （μ_prev = 前两点精确均值、μ_next = 后两点精确均值），分母 =
/// `Σ(τ_i − μ_prev)²`；比值 = 分子/分母即 lag-1 自相关的有理估计——
/// 登记面只登记分子，分母经本访问器成对暴露。k=3 小窗下比值范围可能
/// 超出 [−1, 1]（诚实边界：小窗估计不做钳制，仅描述性）。
///
/// 整数形（exact 口径）：精确均值下分子 = `(τ_1−τ_2)(τ_2−τ_3)/2`、
/// 分母 = `(τ_1−τ_2)²/2`——两者同为整数时按精确值返回；奇偶性使精确形
/// 为半整数时超出整数量化格点（quant_step = 1）→ None（域外弃权）。
/// undetermined 出口（fail-visible 不猜）：序列不足 k=3；前两点相等
/// 使分母为零（含常数序列，比值未定义）；乘积超出 i128 域。
pub fn tau_autocorrelation(series: &[u64]) -> Option<(i64, i64)> {
    let n = series.len();
    if n < TAU_SLIDING_WINDOW {
        return None;
    }
    let a = i128::from(series[n - 3]);
    let b = i128::from(series[n - 2]);
    let c = i128::from(series[n - 1]);
    let d_ab = a - b;
    let d_bc = b - c;
    let num2 = d_ab.checked_mul(d_bc)?;
    let den2 = d_ab.checked_mul(d_ab)?;
    if den2 == 0 {
        return None;
    }
    if num2 % 2 != 0 || den2 % 2 != 0 {
        return None;
    }
    let num = num2 / 2;
    let den = den2 / 2;
    Some((
        num.clamp(i64::MIN as i128, i64::MAX as i128) as i64,
        den.clamp(i64::MIN as i128, i64::MAX as i128) as i64,
    ))
}

/// 相标签（描述性）：按观察窗尾直接观测量给出三态定义性标签——
/// 尾窗有非零告警 → `"crisis"`（危机期）；certified_streak 达标
/// （≥ required）→ `"absorbing"`（吸收期/深稳态）；否则 → `"relaxing"`
/// （弛豫中：告警已清但判据 streak 未满）。
///
/// 标签次序即判定次序：危机优先（有警即危机，压倒 streak 达标）。
/// 诚实边界：三标签为直接观测量的定义性描述，非判定立法；危机/临界
/// 判定阈值候选实验数据定标，不预设。
pub fn phase_label(
    alert_counts_tail: &[u64],
    required: u64,
    certified_streak: u64,
) -> &'static str {
    if alert_counts_tail.iter().any(|&count| count > 0) {
        return "crisis";
    }
    if certified_streak >= required {
        return "absorbing";
    }
    "relaxing"
}

/// 读回超时（反应器不可达/积压时本窗视图以 None fail-visible 呈现）。
const READBACK_TIMEOUT: Duration = Duration::from_secs(10);
/// 读回轮询间隔。
const READBACK_POLL: Duration = Duration::from_millis(100);

/// 装配均衡评估任务：每窗结算一次（取走窗内 alert 计数 → `close_window`）
/// → 结算观测记录推进证据锚链（复算凭证实锚，审计日志留痕）
/// → 组装九参发射 `equilibrium_gate` 进单反应器 → 读回
/// `payload.equilibrium`（立法 v0.12.0 起 gate 接管主契约，
/// `equilibrium_check` 规则与影子双跑机制已退役）→ 追加发射
/// `system_phase`（三层相位复合的系统级裁决，立法 v0.14.0）读回
/// `payload.system_phase`（读回失败回落 relaxing）缓存视图供 status 呈现。
///
/// 纯观测零拦截：任何失败路径只影响本任务的视图发布（stable=None），
/// 不改写任何既有响应路径。
pub fn spawn_equilibrium_evaluator(
    api: crate::api::server::GovernanceApi,
    cfg: EquilibriumEvaluatorConfig,
) -> tokio::task::JoinHandle<()> {
    // 差分基线在 spawn 时刻捕获（非任务首跑时）——spawn 之后发生的记账
    // 保证落在下一窗差分内，不因任务调度延迟被吞进基线。
    let baseline = shared().snapshot();
    tokio::spawn(async move { equilibrium_evaluator_loop(api, cfg, baseline).await })
}

async fn equilibrium_evaluator_loop(
    api: crate::api::server::GovernanceApi,
    cfg: EquilibriumEvaluatorConfig,
    mut prev_totals: WindowsSnapshot,
) {
    let windows = shared();
    let mut ticker = tokio::time::interval(Duration::from_secs(cfg.window_secs.max(1)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // 上一结算窗位势总和（data 层进度感知派生的参照基线；首窗取 0）。
    let mut prev_phi_total: u64 = 0;
    loop {
        ticker.tick().await;
        // 特征量按定义表口径统一提取（提取顺序与平移前逐点一致：
        // 快照 → 窗警排水 → 窗结算 → 结算后快照）。
        let snap = windows.snapshot();
        // 窗结算成对排水（只加不改：全局侧同一 PENDING_ALERTS 计数器、同一
        // 窗界，口径零动；按域侧同锁成对取走——全局口径 = 分账之和逐窗严格
        // 成立；既有 drain_pending_alerts 自由函数保留为零改动兼容面）。
        let (window_alert_count, domain_alert_counts) = windows.drain_window_alerts();
        let had_alert = window_alert_count > 0;
        windows.close_window(had_alert);
        windows.record_window_alert_count(window_alert_count);
        windows.record_domain_window_alert_counts(&domain_alert_counts);
        let window_cert_count = windows.take_window_cert_count();
        windows.record_window_cert_count(window_cert_count);
        let after = windows.snapshot();
        let features = extract_feature_values(&prev_totals, &after, window_alert_count);
        // 相位在窗结算原子点以本窗实值计算（尾窗警量/判据门槛/认证连击
        // 三输入同源），与结算记录同源可回放；status 呈现层读缓存二次
        // 派生不做（防读算与窗结算异相竞态）。
        let phase = phase_label(
            &[window_alert_count],
            cfg.certified_streak_min.max(0) as u64,
            after.certified_streak,
        );
        // 域级相位（只加不改）：每域以自身窗警量尾部独立标签；认证 streak
        // 属集级（全局认证连击全域共用同一输入，域账只分警量不分认证）。
        // Φ 位势与待裁观测快照（结算原子点，publish_view 前）+ 停滞判定
        // （件④）与停滞域吸收相否决门。
        let phi_snapshot = phi_by_domain();
        let pending_snapshot = pending_readjudication_total();
        let stalled = settle_stall_domains(&phi_snapshot);
        let mut domain_phases = windows.domain_phase_views(
            cfg.certified_streak_min.max(0) as u64,
            after.certified_streak,
        );
        veto_stalled_absorbing(&mut domain_phases, &stalled);
        // 三层相位复合（结算原子点纯派生）：capability=集级相位、
        // rules=域级相位复合、data=待裁位势面；位势总和与按域分账同源
        // （同一快照可回放）。
        let phi_total: u64 = phi_snapshot.values().sum();
        let layer_phases = compose_layer_phases(
            phase,
            &domain_phases,
            pending_snapshot,
            phi_total,
            prev_phi_total,
        );
        prev_phi_total = phi_total;
        // 复算凭证实锚（元数据面）：本窗结算观测记录推进观测流增量哈希
        // 链，审计日志留痕；不进发射参数与 status 契约（契约冻结不动）。
        let anchor = windows.settle_window_evidence(
            &prev_totals,
            &after,
            window_alert_count,
            window_cert_count,
            &features,
        );
        tracing::info!(anchor = %anchor, "equilibrium window settlement evidence anchor advanced");
        prev_totals = snap;
        let instr = build_equilibrium_gate_instruction(&features, &cfg);
        let in_rate = features.in_rate;
        let digest_rate = features.digest_rate;
        // 读回基线：仅认发射之后且指令队列清空的快照（快照不可得=本轮必超时）
        let baseline_version = match api.snapshot().await {
            Ok((_, _, v)) => v,
            Err(_) => u64::MAX,
        };
        // 切换批后单发 gate（立法 v0.12.0）：gate 已接管主契约
        // payload.equilibrium/_cause，影子双跑与比对机制随 equilibrium_check
        // 规则退役同批退场；undetermined 经 cause=missing_observation 可辨。
        let view = match api.send_command(instr) {
            Ok(_) => {
                let gate =
                    read_verdict(&api, baseline_version, "equilibrium", "equilibrium_cause").await;
                // 系统级相位（三层复合经规则层裁决）：gate 读回后追加发送
                // system_phase 指令并读回 payload.system_phase；读回失败
                // 回落 relaxing 且均衡布尔置假（fail-visible 不猜，不 panic）。
                let readback_baseline = match api.snapshot().await {
                    Ok((_, _, v)) => v,
                    Err(_) => u64::MAX,
                };
                let system_readback = match api
                    .send_command(build_system_phase_instruction(&layer_phases, phi_total))
                {
                    Ok(_) => {
                        read_verdict(
                            &api,
                            readback_baseline,
                            "system_phase",
                            "system_phase_cause",
                        )
                        .await
                    }
                    Err(_) => None,
                };
                let (system_phase, system_balanced) = match system_readback {
                    Some((Some(p), _)) => {
                        let balanced = p == "absorbing";
                        (p, balanced)
                    }
                    _ => ("relaxing".to_string(), false),
                };
                match gate {
                    Some((verdict, cause)) => EquilibriumView {
                        stable: verdict.as_deref().map(|s| s == "stable"),
                        last_cause: cause,
                        window_secs: cfg.window_secs,
                        in_rate,
                        digest_rate,
                        phase,
                        domain_phases: domain_phases.clone(),
                        phi_by_domain: phi_snapshot.clone(),
                        pending_readjudication: pending_snapshot,
                        stall_domains: stalled.clone(),
                        layer_phases: layer_phases.clone(),
                        system_phase: system_phase.clone(),
                        system_balanced,
                        phi_total,
                    },
                    None => EquilibriumView {
                        stable: None,
                        last_cause: Some("readback_timeout".to_string()),
                        window_secs: cfg.window_secs,
                        in_rate,
                        digest_rate,
                        phase,
                        domain_phases: domain_phases.clone(),
                        phi_by_domain: phi_snapshot.clone(),
                        pending_readjudication: pending_snapshot,
                        stall_domains: stalled.clone(),
                        layer_phases: layer_phases.clone(),
                        system_phase,
                        system_balanced,
                        phi_total,
                    },
                }
            }
            Err(e) => EquilibriumView {
                stable: None,
                last_cause: Some(format!("reactor_unavailable: {e}")),
                window_secs: cfg.window_secs,
                in_rate,
                digest_rate,
                phase,
                domain_phases,
                phi_by_domain: phi_snapshot,
                pending_readjudication: pending_snapshot,
                stall_domains: stalled,
                layer_phases,
                system_phase: "relaxing".to_string(),
                system_balanced: false,
                phi_total,
            },
        };
        publish_view(view);
    }
}

/// 读回本窗判定：轮询直至快照版本越过发射基线且指令队列清空，取
/// `payload.equilibrium / equilibrium_cause`（立法 v0.12.0 起 gate 接管
/// 主契约，单读回面）。超时 = None（fail-visible 不猜）。注释性竞态余量：
/// 基线捕获与指令处理之间若有无关 fact 抬版本，读回可能取到上一窗判定
/// ——均衡判定本身幂等收敛，演练面流量受控。
async fn read_verdict(
    api: &crate::api::server::GovernanceApi,
    baseline_version: u64,
    verdict_attr: &str,
    cause_attr: &str,
) -> Option<(Option<String>, Option<String>)> {
    let deadline = tokio::time::Instant::now() + READBACK_TIMEOUT;
    loop {
        tokio::time::sleep(READBACK_POLL).await;
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        let (payload, queue, version) = api.snapshot().await.ok()?;
        if version <= baseline_version || !queue.is_empty() {
            continue;
        }
        let business = payload
            .get("__exec__")
            .and_then(|e| e.get("payload"))
            .unwrap_or(&payload);
        let verdict = business.get(verdict_attr).and_then(|v| v.as_str());
        let cause = business.get(cause_attr).and_then(|v| v.as_str());
        return Some((verdict.map(|s| s.to_string()), cause.map(|s| s.to_string())));
    }
}

/// 组装 `equilibrium_gate` 指令（立法 v0.12.0 参数面：立法面引用
/// 特征 id——coverage_ratio/net_intake_rate 用注册名而非发射旧名
/// coverage；net_intake_rate 为判据①证据参数随行不参与比较）。
fn build_equilibrium_gate_instruction(
    values: &FeatureValues,
    cfg: &EquilibriumEvaluatorConfig,
) -> evorule_tcb::JsonValue {
    use std::collections::BTreeMap;

    use evorule_tcb::JsonValue;

    let mut params = BTreeMap::new();
    params.insert("in_rate".to_string(), JsonValue::Integer(values.in_rate));
    params.insert(
        "digest_rate".to_string(),
        JsonValue::Integer(values.digest_rate),
    );
    params.insert(
        "net_intake_rate".to_string(),
        JsonValue::Integer(values.net_intake_rate),
    );
    params.insert(
        "alert_free_windows".to_string(),
        JsonValue::Integer(values.alert_free_windows),
    );
    params.insert(
        "coverage_ratio".to_string(),
        JsonValue::Integer(values.coverage_ratio),
    );
    params.insert(
        "certified_streak".to_string(),
        JsonValue::Integer(values.certified_streak),
    );
    params.insert(
        "alert_free_windows_required".to_string(),
        JsonValue::Integer(cfg.alert_free_windows_required),
    );
    params.insert(
        "coverage_min".to_string(),
        JsonValue::Integer(cfg.coverage_min),
    );
    params.insert(
        "certified_streak_min".to_string(),
        JsonValue::Integer(cfg.certified_streak_min),
    );
    let mut instr = BTreeMap::new();
    instr.insert("type".to_string(), JsonValue::string("equilibrium_gate"));
    instr.insert("params".to_string(), JsonValue::Object(params));
    JsonValue::Object(instr)
}

/// 组装 `system_phase` 指令（系统级相位裁决立法 v0.14.0 参数面：调用方
/// 在结算原子点把三层复合折算成四整数参数直传，规则层只比大小）。
fn build_system_phase_instruction(
    layer_phases: &BTreeMap<String, String>,
    phi_total: u64,
) -> evorule_tcb::JsonValue {
    use std::collections::BTreeMap;

    use evorule_tcb::JsonValue;

    let layer_total = layer_phases.len() as i64;
    let layers_in_crisis = layer_phases
        .values()
        .filter(|p| p.as_str() == "crisis")
        .count() as i64;
    let layers_absorbing = layer_phases
        .values()
        .filter(|p| p.as_str() == "absorbing")
        .count() as i64;

    let mut params = BTreeMap::new();
    params.insert(
        "layers_in_crisis".to_string(),
        JsonValue::Integer(layers_in_crisis),
    );
    params.insert(
        "layers_absorbing".to_string(),
        JsonValue::Integer(layers_absorbing),
    );
    params.insert("layer_total".to_string(), JsonValue::Integer(layer_total));
    params.insert(
        "phi_total".to_string(),
        JsonValue::Integer(phi_total as i64),
    );
    let mut instr = BTreeMap::new();
    instr.insert("type".to_string(), JsonValue::string("system_phase"));
    instr.insert("params".to_string(), JsonValue::Object(params));
    JsonValue::Object(instr)
}

/// Windowed observation state behind the `equilibrium_gate` parameters.
#[derive(Debug, Default)]
pub struct EquilibriumWindows {
    inner: Mutex<WindowsState>,
}

/// 窗警量尾部环形缓存容量（τ 相变分析面的观察窗深；只加不改——
/// 发射面契约零动，缓存本体旧→新序语义不变；status 呈现面全量透出
/// 供真机拉取）。
pub(crate) const WINDOW_ALERT_TAIL_CAP: usize = 16;

#[derive(Debug, Default)]
struct WindowsState {
    proposals_in_total: u64,
    digested_total: u64,
    alert_free_windows: u64,
    certified_streak: u64,
    last_coverage: i64,
    /// 已结算窗数（证据记录窗序，单调递增）。
    settlements: u64,
    /// 观测流证据链当前锚（空串=尚未结算，链未起）。
    evidence_anchor: String,
    /// 最近窗警量环形缓存（旧→新序，容量 [`WINDOW_ALERT_TAIL_CAP`]，
    /// 每窗结算登记一条——τ 序列提取 [`extract_relaxation_series`] 与
    /// 相标签 [`phase_label`] 的会话级拉取源）。
    window_alert_tail: VecDeque<u64>,
    /// 最近窗 stable 认证数环形缓存（旧→新序，同容量——recover 弛豫
    /// 回合提取 [`extract_recover_series`] 的认证侧拉取源；纯观测面
    /// 只加不改，发射契约零动，status 呈现面全量透出）。
    window_cert_tail: VecDeque<u64>,
    /// 本窗内已记账的 stable 认证数（take-and-clear：认证记账即时累计、
    /// 窗结算排水——与 pending alert 同型对称）。
    cert_pending: u64,
    /// 本窗内按域 pending 警量（take-and-clear：归因记账即时累计、窗结算
    /// 与全局面同锁成对排水——与 cert_pending 同型对称；`""` = 全局归因
    /// 桶，承接空串/不可单域归因警情，不丢账）。
    domain_alert_pending: BTreeMap<String, u64>,
    /// 按域窗警量尾部环形缓存（只加不改：域 → 最近窗警量序列，旧→新序，
    /// 容量 [`WINDOW_ALERT_TAIL_CAP`]，与 [`Self::window_alert_tail`] 同型
    /// 语义；`""` = 全局归因桶；在册域每窗必推进一条——无警窗记 0，保持
    /// 域尾窗序与全局面同型；域自首次归因警情起入册）。
    domain_alert_tail: BTreeMap<String, VecDeque<u64>>,
}

impl EquilibriumWindows {
    pub fn new() -> Self {
        Self::default()
    }

    /// One proposal entered the intake side of the pipeline.
    pub fn record_proposal(&self) {
        let mut s = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        s.proposals_in_total += 1;
        if let Some(c) = EQUILIBRIUM_PROPOSALS_IN_TOTAL.get() {
            c.inc();
        }
    }

    /// One proposal was digested (law set materialized / superseded).
    pub fn record_digested(&self) {
        let mut s = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        s.digested_total += 1;
        if let Some(c) = EQUILIBRIUM_DIGESTED_TOTAL.get() {
            c.inc();
        }
    }

    /// 一次 alert/contradiction 判定按域归因记账（只加不改；纯观测零拦截）。
    ///
    /// 全局口径 = 分账之和：全局 pending 计数与本域 pending 在同一锁内成对
    /// +1（空串/不可单域归因 → 落全局归因桶 `""`，不丢账——fail-visible
    /// 优于静默丢弃）。域账自首次归因警情起自作物料化，无预注册域清单
    /// （非空域串原样入账；既有 [`record_alert`] 自由函数语义零动 = 纯全局
    /// 计数，兼容面保留）。
    pub fn record_alert_in(&self, domain: &str) {
        let mut s = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        PENDING_ALERTS.fetch_add(1, Ordering::Relaxed);
        let key = if domain.is_empty() { "" } else { domain };
        *s.domain_alert_pending.entry(key.to_string()).or_insert(0) += 1;
    }

    /// Close one observation window: `had_alert == false` extends the alert-free streak,
    /// otherwise the streak resets to zero.
    pub fn close_window(&self, had_alert: bool) {
        let mut s = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if had_alert {
            s.alert_free_windows = 0;
        } else {
            s.alert_free_windows += 1;
        }
        if let Some(g) = EQUILIBRIUM_ALERT_FREE_WINDOWS.get() {
            g.set(s.alert_free_windows as i64);
        }
    }

    /// 窗结算成对排水：全局与按域 pending 警量在同一锁内取走清空（全局侧
    /// 与 [`drain_pending_alerts`] 为同一计数器、同一口径——评估任务改走本
    /// 入口后，全局口径 = 分账之和逐窗严格成立；自由函数保留为零改动兼容
    /// 面，返回 (全局窗警量, 按域窗警量)）。
    pub fn drain_window_alerts(&self) -> (u64, BTreeMap<String, u64>) {
        let mut s = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let global = PENDING_ALERTS.swap(0, Ordering::Relaxed);
        let domains = std::mem::take(&mut s.domain_alert_pending);
        (global, domains)
    }

    /// One certificate outcome: stable extends the certified streak, anything else resets it.
    pub fn record_certificate(&self, stable: bool) {
        let mut s = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if stable {
            s.certified_streak += 1;
            // 本窗 stable 认证记账（take-and-clear 的累计侧）：recover 弛豫
            // 回合提取的窗级认证流源，纯观测面只加不改。
            s.cert_pending += 1;
        } else {
            s.certified_streak = 0;
        }
        if let Some(g) = EQUILIBRIUM_CERTIFIED_STREAK.get() {
            g.set(s.certified_streak as i64);
        }
    }

    /// Latest verification coverage snapshot (computed by the verification face).
    pub fn record_coverage(&self, coverage: i64) {
        let mut s = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        s.last_coverage = coverage;
    }

    /// Snapshot handed to the rule layer as the `equilibrium_gate` params
    /// (rates are caller-side differentials over these totals).
    pub fn snapshot(&self) -> WindowsSnapshot {
        let s = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        WindowsSnapshot {
            proposals_in_total: s.proposals_in_total,
            digested_total: s.digested_total,
            alert_free_windows: s.alert_free_windows,
            certified_streak: s.certified_streak,
            last_coverage: s.last_coverage,
        }
    }

    /// 推进窗结算证据锚（evidence_anchor 实锚绑定）：以本窗结算观测记录
    /// （结算前后快照+窗警量+全部注册特征值+formula_ref 版本戳）构造链步
    /// `{prev_anchor, window}`，经 `evorule_hash::json_digest`（紧凑
    /// serde_json、无前缀 64-hex）推进观测流增量哈希链并缓存链头。
    ///
    /// 哈希域声明：均衡观测流自有哈希链事实（companion 伴随链同型先例——
    /// server 本地链自声明、原始原语计算），与其他哈希域不可互证；
    /// genesis 首窗起链；单结算写者（评估任务每窗调用一次）。
    pub fn settle_window_evidence(
        &self,
        prev: &WindowsSnapshot,
        after: &WindowsSnapshot,
        window_alert_count: u64,
        window_cert_count: u64,
        features: &FeatureValues,
    ) -> String {
        let mut s = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        s.settlements += 1;
        let window_seq = s.settlements;
        let prev_anchor = if s.evidence_anchor.is_empty() {
            EVIDENCE_ANCHOR_GENESIS.to_string()
        } else {
            s.evidence_anchor.clone()
        };
        let record = build_settlement_record(
            window_seq,
            prev,
            after,
            window_alert_count,
            window_cert_count,
            features,
        );
        let entry = serde_json::json!({ "prev_anchor": prev_anchor, "window": record });
        let anchor = evorule_hash::json_digest(&entry);
        s.evidence_anchor = anchor.clone();
        anchor
    }

    /// 当前观测流证据锚（空串=尚未结算，链未起）。元数据面：不进
    /// 发射参数与 status 契约，仅供审计日志与对账面取用。
    pub fn evidence_anchor(&self) -> String {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .evidence_anchor
            .clone()
    }

    /// 登记本窗警量进尾部环形缓存（评估循环每窗结算调用一次；纯观测
    /// 面只加不改——发射与 status 契约零动）。
    pub fn record_window_alert_count(&self, count: u64) {
        let mut s = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if s.window_alert_tail.len() >= WINDOW_ALERT_TAIL_CAP {
            s.window_alert_tail.pop_front();
        }
        s.window_alert_tail.push_back(count);
    }

    /// 拉取最近 k 窗警量（旧→新序；k 超缓存深度则返回现存全部）——
    /// τ 序列提取与相标签的会话级观察入口（设计档 §8「取值入口 =
    /// 实验/分析面对 τ 序列拉取」的实现面）。
    pub fn window_alert_tail(&self, k: usize) -> Vec<u64> {
        let s = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let start = s.window_alert_tail.len().saturating_sub(k);
        s.window_alert_tail.iter().skip(start).copied().collect()
    }

    /// 登记本窗按域警量进尾部环形缓存（评估循环每窗结算调用一次；在册域
    /// 每窗必推进一条——本窗无警记 0，保持域尾窗序与全局面同型；新域自
    /// 本窗入册；容量 [`WINDOW_ALERT_TAIL_CAP`] 逐域独立执法。只加不改：
    /// 全局面 [`Self::record_window_alert_count`] 零动）。
    pub fn record_domain_window_alert_counts(&self, counts: &BTreeMap<String, u64>) {
        let mut s = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // 在册域 = 尾部既有键 ∪ 本窗分账键（BTreeMap 确定序，重放可复算）。
        let mut domains: Vec<String> = s.domain_alert_tail.keys().cloned().collect();
        for key in counts.keys() {
            if !s.domain_alert_tail.contains_key(key) {
                domains.push(key.clone());
            }
        }
        for domain in domains {
            let count = counts.get(&domain).copied().unwrap_or(0);
            let tail = s.domain_alert_tail.entry(domain).or_default();
            if tail.len() >= WINDOW_ALERT_TAIL_CAP {
                tail.pop_front();
            }
            tail.push_back(count);
        }
    }

    /// 拉取指定域最近 k 窗警量（旧→新序；未入账域 = 空序列；k 超缓存深度
    /// 返回现存全部）——域级 τ/相位分析面的会话级拉取入口（与
    /// [`Self::window_alert_tail`] 同型成对）。
    pub fn domain_alert_tail(&self, domain: &str, k: usize) -> Vec<u64> {
        let s = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match s.domain_alert_tail.get(domain) {
            Some(tail) => {
                let start = tail.len().saturating_sub(k);
                tail.iter().skip(start).copied().collect()
            }
            None => Vec::new(),
        }
    }

    /// 域级相位视图（窗结算原子点发布面取用）：每域以自身窗警量尾部独立
    /// [`phase_label`] 标签；认证 streak 属集级——全局认证连击全域共用同一
    /// 输入（域账只分警量不分认证，该裁定为准）；空域账（尾部无窗）不出
    /// 现在 map。
    pub fn domain_phase_views(
        &self,
        required: u64,
        certified_streak: u64,
    ) -> BTreeMap<String, String> {
        let s = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        s.domain_alert_tail
            .iter()
            .filter(|(_, tail)| !tail.is_empty())
            .map(|(domain, tail)| {
                let counts: Vec<u64> = tail.iter().copied().collect();
                (
                    domain.clone(),
                    phase_label(&counts, required, certified_streak).to_string(),
                )
            })
            .collect()
    }

    /// 排走本窗累计的 stable 认证数（take-and-clear：认证记账即时累计、
    /// 窗结算排水——与 pending alert 的 take-and-clear 同型对称）。
    /// 纯观测面只加不改。
    pub fn take_window_cert_count(&self) -> u64 {
        let mut s = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut s.cert_pending)
    }

    /// 登记本窗 stable 认证数进尾部环形缓存（评估循环每窗结算调用一次；
    /// 纯观测面只加不改——发射与 status 契约零动）。
    pub fn record_window_cert_count(&self, count: u64) {
        let mut s = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if s.window_cert_tail.len() >= WINDOW_ALERT_TAIL_CAP {
            s.window_cert_tail.pop_front();
        }
        s.window_cert_tail.push_back(count);
    }

    /// 拉取最近 k 窗 stable 认证数（旧→新序；k 超缓存深度则返回现存
    /// 全部）——recover 弛豫回合提取 [`extract_recover_series`] 的认证
    /// 侧会话级拉取入口（与 [`Self::window_alert_tail`] 同型成对）。
    pub fn window_cert_tail(&self, k: usize) -> Vec<u64> {
        let s = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let start = s.window_cert_tail.len().saturating_sub(k);
        s.window_cert_tail.iter().skip(start).copied().collect()
    }
}

/// 本窗结算观测记录（复算凭证输入面）：窗序 + 结算前后快照全量 +
/// 窗警量 + 窗 stable 认证数 + 全部注册特征值 + formula_ref 版本戳——
/// 观测流重放的最小完备证据集（输入+公式版本+输出三要素齐备；认证
/// 侧窗流入册使 recover 回合提取可重放）。
fn build_settlement_record(
    window_seq: u64,
    prev: &WindowsSnapshot,
    after: &WindowsSnapshot,
    window_alert_count: u64,
    window_cert_count: u64,
    features: &FeatureValues,
) -> serde_json::Value {
    fn totals(v: &WindowsSnapshot) -> serde_json::Value {
        serde_json::json!({
            "proposals_in_total": v.proposals_in_total,
            "digested_total": v.digested_total,
            "alert_free_windows": v.alert_free_windows,
            "certified_streak": v.certified_streak,
            "last_coverage": v.last_coverage,
        })
    }
    let feature_map: serde_json::Map<String, serde_json::Value> = features
        .to_map()
        .into_iter()
        .map(|(k, v)| (k.to_string(), serde_json::json!(v)))
        .collect();
    let formula_map: serde_json::Map<String, serde_json::Value> = EQUILIBRIUM_FEATURES
        .iter()
        .map(|s| (s.feature_id.to_string(), serde_json::json!(s.formula_ref)))
        .collect();
    serde_json::json!({
        "window_seq": window_seq,
        "prev_totals": totals(prev),
        "after_totals": totals(after),
        "window_alert_count": window_alert_count,
        "window_cert_count": window_cert_count,
        "features": serde_json::Value::Object(feature_map),
        "formula_refs": serde_json::Value::Object(formula_map),
    })
}

/// Point-in-time view of the equilibrium observation state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowsSnapshot {
    pub proposals_in_total: u64,
    pub digested_total: u64,
    pub alert_free_windows: u64,
    pub certified_streak: u64,
    pub last_coverage: i64,
}

/// 全局 pending 警量计数面测试串行锁：record_alert/record_alert_in 及
/// 各排水入口落同一进程级计数器，涉及全局口径断言的测试须互斥执行，
/// 防并行线程互串账（域侧分账为实例级，不受此锁约束）。文件级
/// pub(crate)（仅测试构建）：跨模块账面测试（如服务回包映射面）共用
/// 同一把锁与 reset 面。
#[cfg(test)]
pub(crate) static PENDING_ALERTS_TEST_SERIAL: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_window_accounting_and_resets() {
        let eq = EquilibriumWindows::new();
        eq.record_proposal();
        eq.record_proposal();
        eq.record_digested();
        eq.close_window(false);
        eq.close_window(false);
        eq.record_certificate(true);
        eq.record_certificate(true);
        eq.record_coverage(90);
        let snap = eq.snapshot();
        assert_eq!(snap.proposals_in_total, 2);
        assert_eq!(snap.digested_total, 1);
        assert_eq!(snap.alert_free_windows, 2);
        assert_eq!(snap.certified_streak, 2);
        assert_eq!(snap.last_coverage, 90);

        // An alerting window resets the alert-free streak; an unstable
        // certificate resets the certified streak — both fail closed.
        eq.close_window(true);
        eq.record_certificate(false);
        let snap = eq.snapshot();
        assert_eq!(snap.alert_free_windows, 0);
        assert_eq!(snap.certified_streak, 0);
        assert_eq!(
            snap.proposals_in_total, 2,
            "alerts do not touch intake totals"
        );
    }

    #[test]
    fn test_window_alert_tail_ring_cap_and_order() {
        let eq = EquilibriumWindows::new();
        for count in 1..=(WINDOW_ALERT_TAIL_CAP as u64 + 3) {
            eq.record_window_alert_count(count);
        }
        let tail = eq.window_alert_tail(WINDOW_ALERT_TAIL_CAP);
        let expected: Vec<u64> = (4..=(WINDOW_ALERT_TAIL_CAP as u64 + 3)).collect();
        assert_eq!(tail, expected, "容量滚动丢最旧且保持旧→新序");
        // k 超缓存深度 → 返回现存全部
        assert_eq!(
            eq.window_alert_tail(WINDOW_ALERT_TAIL_CAP + 10).len(),
            WINDOW_ALERT_TAIL_CAP
        );
        // k 取尾部子段（旧→新）
        let cap = WINDOW_ALERT_TAIL_CAP as u64;
        assert_eq!(eq.window_alert_tail(3), vec![cap + 1, cap + 2, cap + 3]);
        assert!(eq.window_alert_tail(0).is_empty());
    }

    #[test]
    fn test_snapshot_independent_of_shared_gauges() {
        // Unregistered path: snapshot must work without any shared gauge handles.
        let eq = EquilibriumWindows::new();
        eq.close_window(false);
        assert_eq!(eq.snapshot().alert_free_windows, 1);
    }

    #[test]
    fn test_shared_instance_and_pending_alerts() {
        let _serial = PENDING_ALERTS_TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Process-wide shared instance: same Arc handed out on every call.
        let a = shared();
        let b = shared();
        assert!(std::sync::Arc::ptr_eq(&a, &b));

        // Pending alerts: concurrent verify-face traffic may record into the
        // process-wide counter in parallel, so assert only our own accounting
        // guarantee — records made on this thread are always included in this
        // thread's next drain (nothing else drains), concurrent adds only grow.
        drain_pending_alerts();
        record_alert();
        record_alert();
        let after = drain_pending_alerts();
        assert!(
            after >= 2,
            "own records must be accounted in next drain: after={after}"
        );

        // View starts fail-visible (no verdict guessed before first settlement;
        // no evaluator runs inside the unit-test binary).
        let v = view();
        assert_eq!(v.stable, None);
    }

    #[test]
    fn test_record_alert_in_global_equals_domain_sum() {
        let _serial = PENDING_ALERTS_TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // 互斥前置清残留（与既有全局计数测试同款口径前提）。
        drain_pending_alerts();
        let eq = EquilibriumWindows::new();
        eq.record_alert_in("alpha");
        eq.record_alert_in("alpha");
        eq.record_alert_in("beta");
        // 空串归因 → 全局归因桶 ""（不丢账，fail-visible）。
        eq.record_alert_in("");
        let (global, domains) = eq.drain_window_alerts();
        assert_eq!(domains.get("alpha"), Some(&2));
        assert_eq!(domains.get("beta"), Some(&1));
        assert_eq!(domains.get(""), Some(&1), "空串归因落全局归因桶");
        let sum: u64 = domains.values().sum();
        assert_eq!(global, sum, "全局口径 = 分账之和（含 \"\" 桶）");
        assert_eq!(global, 4);
    }

    #[test]
    fn test_domain_alert_ledger_settlement_and_independent_phases() {
        let _serial = PENDING_ALERTS_TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drain_pending_alerts();
        let eq = EquilibriumWindows::new();
        // 窗 1：两域各 1 警入账（域账自首次归因警情物料化）。
        eq.record_alert_in("alpha");
        eq.record_alert_in("beta");
        let (global, domains) = eq.drain_window_alerts();
        assert_eq!(global, 2);
        assert_eq!(domains.get("alpha"), Some(&1));
        assert_eq!(domains.get("beta"), Some(&1));
        eq.close_window(global > 0);
        eq.record_window_alert_count(global);
        eq.record_domain_window_alert_counts(&domains);
        // 窗 2-17：全域静默 → 在册域每窗推进一条 0（窗序对齐），首警滚出
        // 16 容量环（两域各自 tail 推进）。
        for _ in 0..16 {
            let (g, d) = eq.drain_window_alerts();
            assert_eq!(g, 0);
            assert!(d.is_empty(), "静默窗零分账");
            eq.close_window(false);
            eq.record_window_alert_count(g);
            eq.record_domain_window_alert_counts(&d);
        }
        assert_eq!(
            eq.domain_alert_tail("alpha", WINDOW_ALERT_TAIL_CAP),
            vec![0; WINDOW_ALERT_TAIL_CAP],
            "首警已滚出环形容量"
        );
        // 认证 streak 属集级：全局认证连击达标（全域共用同一 streak 输入）。
        for _ in 0..3 {
            eq.record_certificate(true);
        }
        // 窗 18：仅 alpha 域复警 → 两域相位独立：alpha 尾窗有警 = crisis；
        // beta 尾全零 + streak 达标 = absorbing。
        eq.record_alert_in("alpha");
        let (global, domains) = eq.drain_window_alerts();
        assert_eq!(global, 1);
        assert_eq!(domains.get("alpha"), Some(&1));
        assert!(!domains.contains_key("beta"), "beta 静默窗零分账");
        eq.close_window(global > 0);
        eq.record_window_alert_count(global);
        eq.record_domain_window_alert_counts(&domains);
        let phases = eq.domain_phase_views(3, eq.snapshot().certified_streak);
        assert_eq!(phases.len(), 2, "在册两域全量出现（空域账才缺席）");
        assert_eq!(phases.get("alpha").map(String::as_str), Some("crisis"));
        assert_eq!(phases.get("beta").map(String::as_str), Some("absorbing"));
        // 未入账域无尾部、无相位。
        assert!(eq.domain_alert_tail("never-seen", 4).is_empty());
    }

    #[test]
    fn test_view_carries_domain_phases() {
        // 缺省视图无域账（空域账不出现在 map）。
        assert!(EquilibriumView::default().domain_phases.is_empty());
        // 视图发布通道透传：domain_phases 经 publish_view/view() 往返。
        let mut domain_phases = BTreeMap::new();
        domain_phases.insert("alpha".to_string(), "crisis".to_string());
        domain_phases.insert("beta".to_string(), "absorbing".to_string());
        // phase 保持词表内、stable 保持 None：进程级视图为共享态，companion
        // 呈现测试与既有 fail-visible 断言并行读取不受本测试扰动。
        publish_view(EquilibriumView {
            stable: None,
            last_cause: None,
            window_secs: 60,
            in_rate: 0,
            digest_rate: 0,
            phase: "relaxing",
            domain_phases,
            phi_by_domain: BTreeMap::new(),
            pending_readjudication: 0,
            stall_domains: Vec::new(),
            layer_phases: BTreeMap::new(),
            system_phase: String::new(),
            system_balanced: false,
            phi_total: 0,
        });
        let read = view();
        assert_eq!(
            read.domain_phases.get("alpha").map(String::as_str),
            Some("crisis")
        );
        assert_eq!(
            read.domain_phases.get("beta").map(String::as_str),
            Some("absorbing")
        );
        // 既有字段零动（回归口径）。
        assert_eq!(read.window_secs, 60);
        assert_eq!(read.phase, "relaxing");
        assert_eq!(read.stable, None);
    }

    #[test]
    fn test_registry_and_extraction_are_in_sync() {
        // 定义表特征 id 集合 = 提取映射键集合（双向，漏登记/漏提取即红）。
        // 相变刻画语言三条 τ 统计特征为分析面派生量：计算入口 = 分析面对
        // τ 序列拉取，非窗结算内联提取（发射面契约冻结），故不在提取
        // 映射内；登记面 = 窗结算提取面 ∪ 分析面 τ 特征（恰此三条），
        // τ 特征自身完备性由 phase-language 专项测试锁定。
        let prev = WindowsSnapshot {
            proposals_in_total: 0,
            digested_total: 0,
            alert_free_windows: 0,
            certified_streak: 0,
            last_coverage: 0,
        };
        let after = WindowsSnapshot {
            proposals_in_total: 7,
            digested_total: 3,
            alert_free_windows: 2,
            certified_streak: 4,
            last_coverage: 940,
        };
        let values = extract_feature_values(&prev, &after, 5).to_map();
        let registry_ids: std::collections::BTreeSet<_> =
            EQUILIBRIUM_FEATURES.iter().map(|s| s.feature_id).collect();
        let extracted_ids: std::collections::BTreeSet<_> = values.keys().copied().collect();
        for id in &extracted_ids {
            assert!(
                registry_ids.contains(id),
                "extracted id must be registered: {id}"
            );
        }
        let expected_analysis: std::collections::BTreeSet<_> = [
            "tau_first_difference",
            "tau_sliding_variance",
            "tau_autocorrelation",
            "tau_recover_first_difference",
            "tau_recover_sliding_variance",
            "tau_recover_autocorrelation",
        ]
        .into_iter()
        .collect();
        let analysis_only: std::collections::BTreeSet<_> =
            registry_ids.difference(&extracted_ids).copied().collect();
        assert_eq!(
            analysis_only, expected_analysis,
            "registry entries beyond window extraction must be exactly the \
             analysis-face tau + tau_recover features"
        );
    }

    #[test]
    fn test_feature_values_equal_legacy_window_quantities() {
        // 平移等价：每个特征值与旧硬编码口径逐项相等（行为零变更锚）。
        let prev = WindowsSnapshot {
            proposals_in_total: 10,
            digested_total: 4,
            alert_free_windows: 1,
            certified_streak: 2,
            last_coverage: 900,
        };
        let after = WindowsSnapshot {
            proposals_in_total: 13,
            digested_total: 9,
            alert_free_windows: 5,
            certified_streak: 7,
            last_coverage: 1000,
        };
        let v = extract_feature_values(&prev, &after, 3);
        assert_eq!(v.in_rate, 3);
        assert_eq!(v.digest_rate, 5);
        assert_eq!(v.net_intake_rate, -2);
        assert_eq!(v.window_alert_count, 3);
        assert_eq!(v.alert_free_windows, 5);
        assert_eq!(v.coverage_ratio, 1000);
        assert_eq!(v.certified_streak, 7);
        assert_eq!(v.proposals_in_total, 13);
        assert_eq!(v.digested_total, 9);
        // 发射面参数 coverage 冻结取 coverage_ratio 注册值。
        let m = v.to_map();
        assert_eq!(m["coverage_ratio"], v.coverage_ratio);
    }

    #[test]
    fn test_gate_instruction_params_use_feature_ids() {
        use evorule_tcb::JsonValue;
        // 影子指令（立法 v0.11.0）：参数面引用特征 id——coverage_ratio 替代
        // 发射旧名 coverage，net_intake_rate 作判据①证据参数随行；九参齐全。
        let cfg = EquilibriumEvaluatorConfig {
            window_secs: 1,
            alert_free_windows_required: 2,
            coverage_min: 900,
            certified_streak_min: 3,
        };
        let values = FeatureValues {
            proposals_in_total: 10,
            digested_total: 9,
            in_rate: 3,
            digest_rate: 5,
            net_intake_rate: -2,
            window_alert_count: 0,
            alert_free_windows: 5,
            coverage_ratio: 1000,
            certified_streak: 7,
        };
        let instr = build_equilibrium_gate_instruction(&values, &cfg);
        let JsonValue::Object(m) = &instr else {
            panic!("instruction must be object");
        };
        assert_eq!(m["type"], JsonValue::string("equilibrium_gate"));
        let JsonValue::Object(p) = &m["params"] else {
            panic!("params must be object");
        };
        let expected: std::collections::BTreeMap<&str, i64> = [
            ("in_rate", 3),
            ("digest_rate", 5),
            ("net_intake_rate", -2),
            ("alert_free_windows", 5),
            ("coverage_ratio", 1000),
            ("certified_streak", 7),
            ("alert_free_windows_required", 2),
            ("coverage_min", 900),
            ("certified_streak_min", 3),
        ]
        .into_iter()
        .collect();
        assert_eq!(p.len(), expected.len(), "九参齐全");
        for (k, v) in &expected {
            assert_eq!(
                p.get(*k),
                Some(&JsonValue::Integer(*v)),
                "参数 {k} 应按特征 id 直传 {v}"
            );
        }
    }

    #[test]
    fn test_registry_dimension_fingerprints_via_dimension_service() {
        // 量纲指纹：经推理族 dimension_verify_formula 服务面校验——每条
        // formula_ref 解析成立，且 rhs 推导量纲 = 登记七元指纹。
        use evorule_inference_services::services::DimensionVerifyFormula;
        use evorule_inference_services::NativeService;
        use evorule_tcb::JsonValue;

        // 特征 id 按目标量纲登记（lhs 可推导）；公式右侧仅用基础量纲变量。
        let si_units: Vec<(&str, JsonValue)> = [
            ("proposals_in_total", "1"),
            ("digested_total", "1"),
            ("in_rate", "1/s"),
            ("digest_rate", "1/s"),
            ("net_intake_rate", "1/s"),
            ("window_alert_count", "1"),
            ("alert_free_windows", "1"),
            ("coverage_ratio", "1"),
            ("certified_streak", "1"),
            ("event_count", "1"),
            ("window_count", "1"),
            ("certificate_count", "1"),
            ("coverage_per_mille", "1"),
            ("s", "s"),
            // 相变刻画语言 τ 特征（分析面）：τ 为回合窗计数，全无量纲；
            // mean_prev/mean_next 为 k=3 窗前两点/后两点的精确均值符号。
            ("tau_first_difference", "1"),
            ("tau_sliding_variance", "1"),
            ("tau_autocorrelation", "1"),
            ("tau_i", "1"),
            ("tau_i_prev", "1"),
            ("tau_j", "1"),
            ("tau_k", "1"),
            ("tau_1", "1"),
            ("tau_2", "1"),
            ("tau_3", "1"),
            // 相变刻画语言 τ_recover 特征（分析面第二弛豫度量双轨）：同全
            // 无量纲——τ_recover 为恢复回合实际窗计数。
            ("tau_recover_first_difference", "1"),
            ("tau_recover_sliding_variance", "1"),
            ("tau_recover_autocorrelation", "1"),
            ("tau_recover_i", "1"),
            ("tau_recover_i_prev", "1"),
            ("tau_recover_j", "1"),
            ("tau_recover_k", "1"),
            ("tau_recover_1", "1"),
            ("tau_recover_2", "1"),
            ("tau_recover_3", "1"),
            ("mean_prev", "1"),
            ("mean_next", "1"),
        ]
        .iter()
        .map(|(k, v)| (*k, JsonValue::string(*v)))
        .collect();
        let formulas: Vec<JsonValue> = EQUILIBRIUM_FEATURES
            .iter()
            .map(|s| JsonValue::string(s.formula_ref))
            .collect();
        let args = JsonValue::object_from_pairs(&[
            (
                "si_units",
                JsonValue::Object(
                    si_units
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v))
                        .collect(),
                ),
            ),
            ("formulas", JsonValue::Array(formulas)),
        ]);
        let out = DimensionVerifyFormula
            .execute(&args)
            .expect("dimension service execution");
        let results = out
            .get("results")
            .and_then(|v| v.as_array())
            .expect("dimension service results array");
        assert_eq!(results.len(), EQUILIBRIUM_FEATURES.len());
        for (r, spec) in results.iter().zip(EQUILIBRIUM_FEATURES) {
            assert_eq!(
                r.get("passed").and_then(|v| v.as_bool()),
                Some(true),
                "formula_ref must be dimensionally well-formed: {} -> {r:?}",
                spec.feature_id
            );
            let rhs: Vec<i64> = r
                .get("right_dimension")
                .and_then(|v| v.as_array())
                .expect("right-hand dimension vector")
                .iter()
                .map(|p| p.as_i64().expect("integer exponent"))
                .collect();
            assert_eq!(
                rhs.as_slice(),
                spec.dim_vector.as_slice(),
                "dimension fingerprint mismatch for {}",
                spec.feature_id
            );
        }
    }

    #[test]
    fn test_settlement_record_carries_full_evidence_set() {
        // 结算观测记录 = 复算凭证最小完备集：窗序 + 前后快照全量 + 窗警量
        // + 全部注册特征值 + formula_ref 版本戳逐条绑定；registry 逐特征
        // evidence_anchor 声明非空且绑定实锚形态（同源声明逐条一致）。
        let prev = WindowsSnapshot {
            proposals_in_total: 4,
            digested_total: 2,
            alert_free_windows: 1,
            certified_streak: 1,
            last_coverage: 950,
        };
        let after = WindowsSnapshot {
            proposals_in_total: 9,
            digested_total: 5,
            alert_free_windows: 2,
            certified_streak: 2,
            last_coverage: 970,
        };
        let features = extract_feature_values(&prev, &after, 3);
        let record = build_settlement_record(7, &prev, &after, 3, 0, &features);
        assert_eq!(record["window_seq"], serde_json::json!(7));
        assert_eq!(record["window_alert_count"], serde_json::json!(3));
        for (key, expected) in [
            ("proposals_in_total", 4),
            ("digested_total", 2),
            ("alert_free_windows", 1),
            ("certified_streak", 1),
            ("last_coverage", 950),
        ] {
            assert_eq!(
                record["prev_totals"][key],
                serde_json::json!(expected),
                "prev totals must be recorded verbatim: {key}"
            );
        }
        assert_eq!(
            record["after_totals"]["proposals_in_total"],
            serde_json::json!(9)
        );
        let values = features.to_map();
        // 窗结算特征值逐条记录且键集合恰 = 提取映射（相变刻画语言 τ
        // 分析面派生量不入结算特征值——发射面契约冻结）。
        let recorded = record
            .get("features")
            .and_then(|v| v.as_object())
            .expect("features object in settlement record");
        assert_eq!(
            recorded.len(),
            values.len(),
            "settlement feature map must match the window-extraction map exactly"
        );
        for (id, expected) in &values {
            assert_eq!(
                record["features"][*id],
                serde_json::json!(expected),
                "feature value must be recorded: {id}"
            );
        }
        // 全部登记条目（含分析面 τ 特征）：formula 版本戳逐条入册；
        // evidence_anchor 声明非空且绑定实锚形态（同源声明逐条一致）。
        for spec in EQUILIBRIUM_FEATURES.iter() {
            assert_eq!(
                record["formula_refs"][spec.feature_id],
                serde_json::json!(spec.formula_ref),
                "formula version stamp must be recorded: {}",
                spec.feature_id
            );
            assert!(
                !spec.evidence_anchor.is_empty(),
                "registry anchor declaration non-empty: {}",
                spec.feature_id
            );
            assert_eq!(
                spec.evidence_anchor, EVIDENCE_ANCHOR_WINDOW_STREAM,
                "anchor declaration must describe the bound anchor form: {}",
                spec.feature_id
            );
        }
    }

    #[test]
    fn test_evidence_anchor_chain_deterministic_and_tamper_propagating() {
        // 同观测序列双跑 → 锚链逐窗一致（确定性，genesis 起链）；逐窗推进
        // 相邻锚必不等；任一窗观测被改 → 该窗及其后所有锚分叉（增量链
        // 篡改传播红）。锚形态 = 无前缀 64-hex。
        let settle_two = |alert_count_second: u64| {
            let eq = EquilibriumWindows::new();
            let genesis = WindowsSnapshot {
                proposals_in_total: 0,
                digested_total: 0,
                alert_free_windows: 0,
                certified_streak: 0,
                last_coverage: 0,
            };
            eq.record_proposal();
            eq.record_digested();
            eq.record_coverage(900);
            eq.close_window(false);
            let after_first = eq.snapshot();
            let first_features = extract_feature_values(&genesis, &after_first, 0);
            let a1 = eq.settle_window_evidence(&genesis, &after_first, 0, 0, &first_features);
            let second_features =
                extract_feature_values(&after_first, &after_first, alert_count_second);
            let a2 = eq.settle_window_evidence(
                &after_first,
                &after_first,
                alert_count_second,
                0,
                &second_features,
            );
            let a3 = eq.settle_window_evidence(
                &after_first,
                &after_first,
                alert_count_second,
                0,
                &second_features,
            );
            (eq, a1, a2, a3)
        };

        let (eq_a, a1, a2, a3) = settle_two(0);
        assert_eq!(
            eq_a.evidence_anchor(),
            a3,
            "cached head must track last settlement"
        );
        for a in [&a1, &a2, &a3] {
            assert_eq!(a.len(), 64, "anchor body must be 64-hex: {a}");
            assert!(
                a.chars().all(|c| c.is_ascii_hexdigit()),
                "anchor must be hex: {a}"
            );
        }
        assert_ne!(
            a1, a2,
            "per-window progression: adjacent anchors must differ"
        );
        assert_ne!(
            a2, a3,
            "per-window progression: adjacent anchors must differ"
        );

        let (_, b1, b2, b3) = settle_two(0);
        assert_eq!(
            a1, b1,
            "same observation sequence replays to the same anchor"
        );
        assert_eq!(a2, b2);
        assert_eq!(a3, b3);

        // 篡改传播：第二窗窗警量不同 → 第二窗起锚链全分叉（第一窗不变）。
        let (_, c1, c2, c3) = settle_two(2);
        assert_eq!(c1, a1, "untampered first window keeps its anchor");
        assert_ne!(c2, a2, "tampered window observation must change its anchor");
        assert_ne!(c3, a3, "tamper must propagate to all subsequent anchors");
    }

    #[test]
    fn test_extract_relaxation_series_golden_vectors() {
        // 黄金向量（手算）：窗 0-2 三零闭合 τ=3；窗 3 有警清零；窗 4-6
        // 闭合 τ=3；窗 7-9 闭合 τ=3 → [3, 3, 3]（闭合后 streak 归零立即
        // 起算，同段零告警可连续闭合多个回合）。
        assert_eq!(
            extract_relaxation_series(&[0, 0, 0, 1, 0, 0, 0, 0, 0, 0], 3),
            vec![3, 3, 3]
        );
        // 不等长例（手算）：窗 0 有警；窗 1-3 闭合 τ=3；窗 4 零告警
        // （streak=1）但窗 5 有警清零；窗 6-8 闭合 τ=3；窗 9-10
        // streak=2 < 3 未闭合不产出 → [3, 3]（尾段未认证回合剔除——
        // 诚实边界）。
        assert_eq!(
            extract_relaxation_series(&[1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0], 3),
            vec![3, 3]
        );
        // required = 0 = 无判据 → 空序列；空流 → 空序列；尾段未达 → 空序列。
        assert_eq!(extract_relaxation_series(&[0, 0, 0], 0), Vec::<u64>::new());
        assert_eq!(extract_relaxation_series(&[], 3), Vec::<u64>::new());
        assert_eq!(extract_relaxation_series(&[1, 0, 0], 3), Vec::<u64>::new());
        // 窗告警计数 > 1 同样视为有警窗（非零即破零告警 streak）：
        // [2, 0, 0, 0] required=2 → 窗 2 闭合 τ=2，窗 3 streak=1 未闭合。
        assert_eq!(extract_relaxation_series(&[2, 0, 0, 0], 2), vec![2]);
    }

    #[test]
    fn test_tau_features_golden_vectors() {
        // 黄金向量（手算逐值）：[2, 3, 5, 9] 递增 = 临界慢化形态。
        let series = [2_u64, 3, 5, 9];
        // 一阶差分 = 末两元素差 9 - 5。
        assert_eq!(tau_first_difference(&series), Some(4));
        // 滑动方差（整数恒等形 Σ_{i<j}(τ_i−τ_j)² = k²·variance）：
        // 末 k=3 窗 [3, 5, 9] → (3−5)² + (3−9)² + (5−9)² = 4 + 36 + 16 = 56。
        assert_eq!(tau_sliding_variance(&series), Some(56));
        // lag-1 自相关整数形：μ_prev = (3+5)/2 = 4，μ_next = (5+9)/2 = 7；
        // 分子 = (3−4)(5−7) + (5−4)(9−7) = 2 + 2 = 4；分母 = 1 + 1 = 2。
        assert_eq!(tau_autocorrelation(&series), Some((4, 2)));

        // 常数序列：滑动方差 = 0；自相关分母 = 0 → 比值未定义 → None
        // （诚实出口）；一阶差分 = 0。
        let constant = [4_u64, 4, 4];
        assert_eq!(tau_first_difference(&constant), Some(0));
        assert_eq!(tau_sliding_variance(&constant), Some(0));
        assert_eq!(tau_autocorrelation(&constant), None);

        // 短序列：不足两回合差分 None；不足 k=3 滑窗统计 None。
        let short = [2_u64, 3];
        assert_eq!(tau_first_difference(&short), Some(1));
        assert_eq!(tau_sliding_variance(&short), None);
        assert_eq!(tau_autocorrelation(&short), None);
        assert_eq!(tau_first_difference(&[]), None);
        assert_eq!(tau_sliding_variance(&[]), None);
        assert_eq!(tau_autocorrelation(&[]), None);

        // 负相关例（手算）：[5, 1, 9]：μ_prev=3，μ_next=5；分子 =
        // (5−3)(1−5) + (1−3)(9−5) = −8 − 8 = −16；分母 = 4 + 4 = 8。
        // k=3 小窗比值 −2 超出 [−1, 1] —— 诚实边界：不钳制，仅描述性。
        assert_eq!(tau_autocorrelation(&[5_u64, 1, 9]), Some((-16, 8)));

        // 奇偶边界：精确均值下分子 = (τ1−τ2)(τ2−τ3)/2、分母 = (τ1−τ2)²/2，
        // 精确形为半整数时超出整数量化格点 → None（fail-visible；
        // [1, 2, 4]: 分母 = (1−2)²/2 = 1/2 非整数）。
        assert_eq!(tau_autocorrelation(&[1_u64, 2, 4]), None);

        // 域外弃权：τ 超出声明的 i64 域 → 一阶差分 None（fail-visible）。
        assert_eq!(
            tau_first_difference(&[0, u64::MAX]),
            None,
            "tau beyond the declared i64 domain is undetermined"
        );
        // i128 乘积域边界：滑窗方差饱和退化幅值不改号（i64::MAX）；
        // 自相关乘积超出 i128 域 → None。
        let huge = [u64::MAX - 1, 0, u64::MAX - 1];
        assert_eq!(tau_sliding_variance(&huge), Some(i64::MAX));
        assert_eq!(tau_autocorrelation(&huge), None);
    }

    #[test]
    fn test_phase_label_descriptive_states() {
        // 三态定义性标签（直接观测量，无调参阈值）：危机 > 吸收 > 弛豫。
        assert_eq!(phase_label(&[0, 5, 0], 3, 9), "crisis", "尾窗有警 = 危机期");
        assert_eq!(
            phase_label(&[0, 0, 0], 3, 3),
            "absorbing",
            "无警且 streak 达标 = 吸收期/深稳态"
        );
        assert_eq!(
            phase_label(&[0, 0, 0], 3, 2),
            "relaxing",
            "无警但 streak 未满 = 弛豫中"
        );
        // 标签次序：危机压倒 streak 达标（有警即危机）。
        assert_eq!(phase_label(&[1], 1, 5), "crisis");
        // 空尾窗 = 无警观测；streak 达标与否定吸收/弛豫。
        assert_eq!(phase_label(&[], 3, 3), "absorbing");
        assert_eq!(phase_label(&[], 3, 0), "relaxing");
    }

    #[test]
    fn test_registry_includes_phase_language_features() {
        // 相变刻画语言三条 τ 特征已登记且八字段完备（id/公式/量纲/窗口
        // 语义/适用域/误差契约/清零语义/复算锚），锚声明同源绑定，
        // regime 全部整数步长 + 域外弃权，量纲全零（τ = 回合窗计数）。
        assert_eq!(TAU_SLIDING_WINDOW, 3, "滑窗长度固定 k=3（整数等价形前提）");
        for id in [
            "tau_first_difference",
            "tau_sliding_variance",
            "tau_autocorrelation",
        ] {
            let spec = match EQUILIBRIUM_FEATURES.iter().find(|s| s.feature_id == id) {
                Some(spec) => spec,
                None => panic!("feature must be registered: {id}"),
            };
            assert_eq!(
                spec.dim_vector,
                [0, 0, 0, 0, 0, 0, 0],
                "tau features are dimensionless: {id}"
            );
            assert!(!spec.formula_ref.is_empty(), "{id}");
            assert!(!spec.regime.domain.is_empty(), "{id}");
            assert_eq!(spec.regime.quant_step, 1, "{id}");
            assert!(spec.regime.undetermined_outside, "{id}");
            assert!(!spec.error_contract.is_empty(), "{id}");
            assert!(!spec.reset_semantics.is_empty(), "{id}");
            assert_eq!(
                spec.evidence_anchor, EVIDENCE_ANCHOR_WINDOW_STREAM,
                "anchor declaration must describe the bound anchor form: {id}"
            );
        }
        // 只加不改：既有九条窗结算特征逐条仍在册（登记面零删改）。
        for id in [
            "proposals_in_total",
            "digested_total",
            "in_rate",
            "digest_rate",
            "net_intake_rate",
            "window_alert_count",
            "alert_free_windows",
            "coverage_ratio",
            "certified_streak",
        ] {
            assert!(
                EQUILIBRIUM_FEATURES.iter().any(|s| s.feature_id == id),
                "existing feature must stay registered: {id}"
            );
        }
    }

    #[test]
    fn test_phase_language_functions_deterministic() {
        // 确定性双跑：同输入两跑全等输出（纯函数契约：无时钟、无随机源、
        // 无外部状态——观测流重放口径与证据锚同源）。
        let stream = [0_u64, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        assert_eq!(
            extract_relaxation_series(&stream, 3),
            extract_relaxation_series(&stream, 3)
        );
        let series = [2_u64, 3, 5, 9];
        assert_eq!(tau_first_difference(&series), tau_first_difference(&series));
        assert_eq!(tau_sliding_variance(&series), tau_sliding_variance(&series));
        assert_eq!(tau_autocorrelation(&series), tau_autocorrelation(&series));
        assert_eq!(phase_label(&[0, 0, 1], 3, 5), phase_label(&[0, 0, 1], 3, 5));
    }

    #[test]
    fn test_extract_recover_series_real_experiment_replay() {
        // 黄金钉（真机复合冲击实验观测流重放，2026-11-09，84 窗实测）：
        // E1 连续注入 8 警窗开启回合，E1/E2 余震全程无 stable 认证悬置至
        // E3 轮 1 首认证闭合 τ=25（长弛豫真形态）；轮 2/3 τ=6/5（静默
        // 间隔+认证节律）。三态判定 40/40 中，episode 形态与逐窗记账全
        // 对账；E4 深稳态认证（无警回合未开启）正确不产出。
        let mut alerts = vec![0_u64; 84];
        for idx in [
            3, 4, 5, 6, 7, 8, 9, 10, // E1: 窗 4-11 连续 8 警窗
            13, 14, // E2 前段: 窗 14-15
            17, 18, 19, 20, // E2 后段: 窗 18-21（窗归属竞态断段形态）
            24, 25, // E3 r1 注入: 窗 25-26
            38, 39, // E3 r2 注入: 窗 39-40
            54, 55, // E3 r3 注入: 窗 55-56
        ] {
            alerts[idx] = 1;
        }
        let mut certs = vec![0_u64; 84];
        for idx in [
            27, 29, 31, 33, // E3 r1 认证: 窗 28/30/32/34
            43, 45, 47, 49, // E3 r2 认证: 窗 44/46/48/50
            58, 60, 62, 63, // E3 r3 认证: 窗 59/61/63/64
            70, 74, 79, // E4 稀疏认证: 窗 71/75/80（无警不产出）
        ] {
            certs[idx] = 1;
        }
        let series = extract_recover_series(&alerts, &certs);
        assert_eq!(
            series,
            vec![25, 6, 5],
            "replay must reproduce the measured recovery-time series"
        );
        // 慢化统计三特征非平凡（真机数据 [25,6,5]：非常数序列差分/方差/
        // 自相关均有效值域——现行 τ 定义下结构性恒平凡）。
        assert_eq!(tau_first_difference(&series), Some(-1));
        assert_eq!(tau_sliding_variance(&series), Some(762));
        assert_eq!(
            tau_autocorrelation(&series),
            None,
            "odd numerator = half-integer, undetermined"
        );
    }

    #[test]
    fn test_extract_recover_series_morphology() {
        // 形态钉：同窗认证+告警（先闭合后重开；重开回合流尾未闭合不产出）。
        let series = extract_recover_series(&[0, 1, 1, 0, 1], &[0, 0, 1, 0, 0]);
        assert_eq!(series, vec![2], "close-then-reopen on the same window");
        // 无警窗段认证不产出回合。
        let series = extract_recover_series(&[0, 0, 0], &[0, 1, 1]);
        assert!(series.is_empty(), "cert before any alert opens no episode");
        // 流尾未闭合回合不产出（未认证回合不进序列，诚实边界同型）。
        let series = extract_recover_series(&[1, 0, 0], &[0, 0, 0]);
        assert!(series.is_empty(), "unclosed tail episode stays out");
        // 两流长度不齐：按较短流截断（fail-visible 不猜缺失侧）。
        let series = extract_recover_series(&[1, 0, 0, 0], &[0, 1]);
        assert_eq!(series, vec![2]);
        // 空流：空序列。
        assert!(extract_recover_series(&[], &[]).is_empty());
        // 确定性双跑。
        let a = [0_u64, 1, 0, 1, 0];
        let c = [0_u64, 0, 1, 0, 1];
        assert_eq!(
            extract_recover_series(&a, &c),
            extract_recover_series(&a, &c)
        );
    }

    #[test]
    fn test_window_cert_tail_and_pending_drain() {
        // 认证侧 take-and-clear 与环形缓存（与警侧同型对称）：认证记账
        // 即时累计、结算排水、缓存登记、cap 裁剪、访问器窗口序。
        let eq = EquilibriumWindows::new();
        eq.record_certificate(true);
        eq.record_certificate(true);
        eq.record_certificate(false);
        assert_eq!(
            eq.take_window_cert_count(),
            2,
            "resets do not count as stable certs"
        );
        assert_eq!(eq.take_window_cert_count(), 0, "take-and-clear");
        eq.record_window_cert_count(2);
        eq.record_window_cert_count(0);
        eq.record_window_cert_count(1);
        assert_eq!(eq.window_cert_tail(2), vec![0, 1], "last-k window order");
        for i in 0..(WINDOW_ALERT_TAIL_CAP as u64 + 4) {
            eq.record_window_cert_count(i);
        }
        let tail = eq.window_cert_tail(usize::MAX);
        assert_eq!(tail.len(), WINDOW_ALERT_TAIL_CAP, "cap enforced");
        assert_eq!(
            *tail.last().unwrap(),
            WINDOW_ALERT_TAIL_CAP as u64 + 3,
            "newest kept"
        );
    }

    #[test]
    fn test_tau_recover_features_registered_and_computed() {
        // τ_recover 三特征注册钉：同型三件（id/formula_ref/regime/锚）在
        // 册 + 值域计算钉（复用同型纯函数，B-218 实测序列 [25,6,5] 形态）。
        for id in [
            "tau_recover_first_difference",
            "tau_recover_sliding_variance",
            "tau_recover_autocorrelation",
        ] {
            let spec = EQUILIBRIUM_FEATURES
                .iter()
                .find(|s| s.feature_id == id)
                .unwrap_or_else(|| panic!("tau_recover feature must be registered: {id}"));
            assert!(
                spec.formula_ref.starts_with(id),
                "formula_ref must be self-describing: {id}"
            );
            assert_eq!(spec.dim_vector, [0, 0, 0, 0, 0, 0, 0]);
            assert!(spec.error_contract.contains("undetermined"));
        }
        // 发射面契约冻结：τ_recover 族不在窗结算内联提取映射内（分析面
        // 派生量，只加不改纪律的登记面形态）。
        let snap = WindowsSnapshot {
            proposals_in_total: 0,
            digested_total: 0,
            alert_free_windows: 0,
            certified_streak: 0,
            last_coverage: 0,
        };
        let fv = extract_feature_values(&snap, &snap, 0);
        assert!(
            !fv.to_map().contains_key("tau_recover_first_difference"),
            "tau_recover family stays out of the inline extraction map"
        );
    }

    #[test]
    fn test_phi_ledger_increment_cap_and_noop() {
        let _serial = PENDING_ALERTS_TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_readjudication_state();
        // 3 条入账（h=2）：alpha×2、beta×1 → Φ = 6。
        record_pending_readjudication(&[
            PendingReadjudication {
                law_id: "law_a".to_string(),
                domain: Some("alpha".to_string()),
                chain_height: 2,
            },
            PendingReadjudication {
                law_id: "law_b".to_string(),
                domain: Some("alpha".to_string()),
                chain_height: 2,
            },
            PendingReadjudication {
                law_id: "law_c".to_string(),
                domain: Some("beta".to_string()),
                chain_height: 2,
            },
        ]);
        assert_eq!(pending_readjudication_total(), 6, "Φ = Σ h_p");
        assert_eq!(phi_by_domain().get("alpha"), Some(&4), "Φ_dom 按域分账");
        assert_eq!(phi_by_domain().get("beta"), Some(&2));
        // 逐级推进：law_a 推满（2 次）→ alpha −2。
        record_readjudicated("law_a");
        record_readjudicated("law_a");
        assert_eq!(phi_by_domain().get("alpha"), Some(&2));
        assert_eq!(pending_readjudication_total(), 4, "每推进一次 Φ −1");
        // 封顶断言：lvl+1 不越过 h——推满后再推进 no-op（不产生负位势）。
        record_readjudicated("law_a");
        assert_eq!(phi_by_domain().get("alpha"), Some(&2));
        assert_eq!(pending_readjudication_total(), 4);
        // 未知 law_id no-op。
        record_readjudicated("never_accounted");
        assert_eq!(pending_readjudication_total(), 4);
        // law_b/law_c 推满 → Φ 清零、pending 清零（零值域不出 map）。
        record_readjudicated("law_b");
        record_readjudicated("law_b");
        record_readjudicated("law_c");
        record_readjudicated("law_c");
        assert_eq!(pending_readjudication_total(), 0);
        assert!(phi_by_domain().is_empty(), "零值域不出现在 map");
    }

    #[test]
    fn test_pending_noop_guard_and_refresh() {
        let _serial = PENDING_ALERTS_TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_readjudication_state();
        record_pending_readjudication(&[PendingReadjudication {
            law_id: "law_x".to_string(),
            domain: Some("alpha".to_string()),
            chain_height: 3,
        }]);
        assert_eq!(pending_readjudication_total(), 3);
        // 同 (domain, h_p) 重复入账 no-op（不重复计）。
        record_pending_readjudication(&[PendingReadjudication {
            law_id: "law_x".to_string(),
            domain: Some("alpha".to_string()),
            chain_height: 3,
        }]);
        assert_eq!(pending_readjudication_total(), 3);
        // 推进一次后同域不同 h 刷新 → 以新值为准（旧贡献减除，lvl 归零重计）。
        record_readjudicated("law_x");
        assert_eq!(pending_readjudication_total(), 2);
        record_pending_readjudication(&[PendingReadjudication {
            law_id: "law_x".to_string(),
            domain: Some("alpha".to_string()),
            chain_height: 5,
        }]);
        assert_eq!(
            pending_readjudication_total(),
            5,
            "刷新：旧贡献成对减除，新值自 lvl=0 重计"
        );
        // 跨域刷新 → 旧域清账、新域重计。
        record_pending_readjudication(&[PendingReadjudication {
            law_id: "law_x".to_string(),
            domain: Some("beta".to_string()),
            chain_height: 2,
        }]);
        assert_eq!(pending_readjudication_total(), 2);
        let phi = phi_by_domain();
        assert!(!phi.contains_key("alpha"), "旧域零值不出 map");
        assert_eq!(phi.get("beta"), Some(&2));
        // 不可归因（None）落全局归因桶 ""（不丢账，fail-visible 同口径）。
        record_pending_readjudication(&[PendingReadjudication {
            law_id: "law_none".to_string(),
            domain: None,
            chain_height: 1,
        }]);
        assert_eq!(phi_by_domain().get(""), Some(&1));
        assert_eq!(pending_readjudication_total(), 3);
    }

    #[test]
    fn test_stall_alarm_once_and_reset() {
        let _serial = PENDING_ALERTS_TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drain_pending_alerts();
        reset_readjudication_state();
        let mut phi = BTreeMap::new();
        phi.insert("alpha".to_string(), 1_u64);
        // 窗 1：正值首见 → 立基线计 1，未达阈值不停滞。
        assert!(settle_stall_domains(&phi).is_empty());
        assert_eq!(drain_pending_alerts(), 0, "未停滞不报警");
        // 窗 2：Φ 全等且计数达 STALL_WINDOWS → 停滞入集 + 报警一次。
        assert_eq!(settle_stall_domains(&phi), vec!["alpha".to_string()]);
        assert!(
            drain_pending_alerts() >= 1,
            "停滞报警经 record_alert 计入窗警量账"
        );
        // 窗 3：持续停滞 → 在集不重复累报。
        assert_eq!(settle_stall_domains(&phi), vec!["alpha".to_string()]);
        assert_eq!(drain_pending_alerts(), 0, "持续停滞不重复累报");
        // 窗 4：Φ 变化（1→2）→ 旧停滞出集复位，新值重立基线。
        let mut phi2 = BTreeMap::new();
        phi2.insert("alpha".to_string(), 2_u64);
        assert!(settle_stall_domains(&phi2).is_empty());
        assert_eq!(drain_pending_alerts(), 0);
        // 窗 5：新值全等 → 重新停滞 + 再报警（解除复位后再停滞可再报）。
        assert_eq!(settle_stall_domains(&phi2), vec!["alpha".to_string()]);
        assert!(drain_pending_alerts() >= 1, "解除复位后重新停滞可再报警");
        // 窗 6：Φ 归零（pending 归零，域键缺席）→ 解除复位出集。
        assert!(settle_stall_domains(&BTreeMap::new()).is_empty());
        assert!(drain_pending_alerts() == 0, "解除后不报警");
        // STALL_WINDOWS 阈值钉：N=2。
        assert_eq!(STALL_WINDOWS, 2);
    }

    #[test]
    fn test_stalled_absorbing_veto_gate() {
        // 停滞域 absorbing 否决回落 relaxing；crisis/relaxing 照常推算；
        // 停滞域不在相位 map 不虚造条目。
        let mut phases = BTreeMap::new();
        phases.insert("alpha".to_string(), "absorbing".to_string());
        phases.insert("beta".to_string(), "crisis".to_string());
        phases.insert("gamma".to_string(), "relaxing".to_string());
        veto_stalled_absorbing(&mut phases, &["alpha".to_string(), "beta".to_string()]);
        assert_eq!(
            phases.get("alpha").map(String::as_str),
            Some("relaxing"),
            "stalled 域 absorbing 判定被否决"
        );
        assert_eq!(
            phases.get("beta").map(String::as_str),
            Some("crisis"),
            "crisis 照常推算"
        );
        assert_eq!(phases.get("gamma").map(String::as_str), Some("relaxing"));
        veto_stalled_absorbing(&mut phases, &["never".to_string()]);
        assert!(!phases.contains_key("never"), "不虚造相位条目");
    }

    #[test]
    fn test_view_carries_phi_pending_stall() {
        // 缺省视图：空 Φ / 零待裁 / 无停滞域。
        let d = EquilibriumView::default();
        assert!(d.phi_by_domain.is_empty());
        assert_eq!(d.pending_readjudication, 0);
        assert!(d.stall_domains.is_empty());
        // 视图发布通道透传：新三字段经 publish_view/view() 往返。phase 保持
        // 词表内、stable 保持 None：进程级共享态不扰动并行读取（同款口径）。
        let mut phi = BTreeMap::new();
        phi.insert("alpha".to_string(), 4_u64);
        publish_view(EquilibriumView {
            stable: None,
            last_cause: None,
            window_secs: 60,
            in_rate: 0,
            digest_rate: 0,
            phase: "relaxing",
            domain_phases: BTreeMap::new(),
            phi_by_domain: phi,
            pending_readjudication: 6,
            stall_domains: vec!["alpha".to_string()],
            layer_phases: BTreeMap::new(),
            system_phase: String::new(),
            system_balanced: false,
            phi_total: 0,
        });
        let read = view();
        assert_eq!(read.phi_by_domain.get("alpha"), Some(&4));
        assert_eq!(read.pending_readjudication, 6);
        assert_eq!(read.stall_domains, vec!["alpha".to_string()]);
        // 既有字段零动（回归口径）。
        assert_eq!(read.window_secs, 60);
        assert_eq!(read.phase, "relaxing");
        assert_eq!(read.stable, None);
    }

    #[test]
    fn test_compose_layer_phases() {
        // 场景 1：rules 层任一域 crisis ⇒ crisis；data 层位势存在且相对
        // 上一窗不降（含待裁新扰动）⇒ crisis；capability 层集级相位原样
        // 透传。BTreeMap 键序确定性断言。
        let mut rules = BTreeMap::new();
        rules.insert("alpha".to_string(), "absorbing".to_string());
        rules.insert("beta".to_string(), "crisis".to_string());
        let layers = compose_layer_phases("relaxing", &rules, 2, 7, 7);
        let keys: Vec<&str> = layers.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec!["capability", "data", "rules"],
            "键序 BTreeMap 确定性"
        );
        assert_eq!(
            layers.get("capability").map(String::as_str),
            Some("relaxing")
        );
        assert_eq!(layers.get("rules").map(String::as_str), Some("crisis"));
        assert_eq!(layers.get("data").map(String::as_str), Some("crisis"));

        // 场景 2：三层全 absorbing（rules 层含空域集同判 absorbing）且
        // data 层位势总和为零 ⇒ absorbing。
        let empty = BTreeMap::new();
        let layers = compose_layer_phases("absorbing", &empty, 0, 0, 0);
        assert_eq!(
            layers.get("capability").map(String::as_str),
            Some("absorbing")
        );
        assert_eq!(
            layers.get("rules").map(String::as_str),
            Some("absorbing"),
            "空域集全域 absorbing 同判"
        );
        assert_eq!(layers.get("data").map(String::as_str), Some("absorbing"));

        // 场景 3：混合（无 crisis、吸收未满员）⇒ rules 层 relaxing；data
        // 层位势存在且逐窗下降（排空中）⇒ relaxing。
        let mut mixed = BTreeMap::new();
        mixed.insert("alpha".to_string(), "absorbing".to_string());
        mixed.insert("beta".to_string(), "relaxing".to_string());
        let layers = compose_layer_phases("relaxing", &mixed, 0, 5, 8);
        assert_eq!(layers.get("rules").map(String::as_str), Some("relaxing"));
        assert_eq!(
            layers.get("data").map(String::as_str),
            Some("relaxing"),
            "位势逐窗下降为排空中 relaxing"
        );

        // 场景 4：data 层新扰动（首窗位势出现，参照基线 0）⇒ crisis；
        // 位势等值停滞亦 crisis（不降即危机）。
        let layers = compose_layer_phases("relaxing", &empty, 2, 7, 0);
        assert_eq!(layers.get("data").map(String::as_str), Some("crisis"));
        let layers = compose_layer_phases("relaxing", &empty, 0, 3, 3);
        assert_eq!(layers.get("data").map(String::as_str), Some("crisis"));
    }

    #[test]
    fn test_view_carries_system_phase() {
        let _serial = PENDING_ALERTS_TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // 缺省视图：空三层账 / 未裁决系统相位 / 非均衡 / 零位势总和。
        let d = EquilibriumView::default();
        assert!(d.layer_phases.is_empty());
        assert_eq!(d.system_phase, "");
        assert!(!d.system_balanced);
        assert_eq!(d.phi_total, 0);
        // 视图发布通道透传：四新字段经 publish_view/view() 往返。phase 保持
        // 词表内、stable 保持 None：进程级共享态不扰动并行读取（同款口径）。
        let mut layer_phases = BTreeMap::new();
        layer_phases.insert("capability".to_string(), "absorbing".to_string());
        layer_phases.insert("rules".to_string(), "absorbing".to_string());
        layer_phases.insert("data".to_string(), "absorbing".to_string());
        publish_view(EquilibriumView {
            stable: None,
            last_cause: None,
            window_secs: 60,
            in_rate: 0,
            digest_rate: 0,
            phase: "relaxing",
            domain_phases: BTreeMap::new(),
            phi_by_domain: BTreeMap::new(),
            pending_readjudication: 0,
            stall_domains: Vec::new(),
            layer_phases,
            system_phase: "absorbing".to_string(),
            system_balanced: true,
            phi_total: 9,
        });
        let read = view();
        assert_eq!(
            read.layer_phases.get("capability").map(String::as_str),
            Some("absorbing")
        );
        assert_eq!(read.system_phase, "absorbing");
        assert!(read.system_balanced);
        assert_eq!(read.phi_total, 9);
        // 既有字段零动（回归口径）。
        assert_eq!(read.window_secs, 60);
        assert_eq!(read.phase, "relaxing");
        assert_eq!(read.stable, None);
    }
}

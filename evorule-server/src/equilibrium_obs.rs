// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Companion-side equilibrium observation aggregator (rule-layer v0.10.0 intake).
//!
//! Maintains the windowed counters that `equilibrium_check` (server_eval.json v0.10.0)
//! adjudicates over: proposal intake vs digestion rates, consecutive alert-free windows,
//! latest verification coverage and the certified-streak. The rule layer only ever sees
//! a caller-supplied snapshot — window aggregation and streak accounting stay here
//! (zero data reshaping inside the rule layer).
//!
//! Event sources are wired as pure observation (zero interception): the verify
//! face records proposals / certificates / alerts, the companion face records
//! proposal digestion and coverage, and a dedicated window-settlement task
//! ([`spawn_equilibrium_evaluator`]) closes windows, emits the legislated
//! `equilibrium_check` instruction into the single reactor and caches the
//! verdict for the status surface. The gauges below expose the live state on
//! the metrics surface.

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
// 均衡态视图（评估任务发布，status 呈现面取用）
// ---------------------------------------------------------------------------

/// 最近一次窗结算后的均衡态呈现快照。
///
/// `stable = None` = 反应器不可达或读回超时（fail-visible：不猜稳态，
/// 呈现层原样透出 null）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EquilibriumView {
    pub stable: Option<bool>,
    pub last_cause: Option<String>,
    pub window_secs: u64,
    pub in_rate: i64,
    pub digest_rate: i64,
}

static VIEW: Mutex<EquilibriumView> = Mutex::new(EquilibriumView {
    stable: None,
    last_cause: None,
    window_secs: 0,
    in_rate: 0,
    digest_rate: 0,
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

/// 读回超时（反应器不可达/积压时本窗视图以 None fail-visible 呈现）。
const READBACK_TIMEOUT: Duration = Duration::from_secs(10);
/// 读回轮询间隔。
const READBACK_POLL: Duration = Duration::from_millis(100);

/// 装配均衡评估任务：每窗结算一次（取走窗内 alert 计数 → `close_window`）
/// → 组装八参发射 `equilibrium_check` 进单反应器 → 读回
/// `payload.equilibrium` 缓存视图供 status 呈现。
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
    loop {
        ticker.tick().await;
        // 窗间差分：本窗入管/消化增量（判据① 输入）
        let snap = windows.snapshot();
        let in_rate = snap
            .proposals_in_total
            .saturating_sub(prev_totals.proposals_in_total) as i64;
        let digest_rate = snap
            .digested_total
            .saturating_sub(prev_totals.digested_total) as i64;
        prev_totals = snap;
        // 窗结算：本窗有警则无警连计清零（判据② 输入）
        let had_alert = drain_pending_alerts() > 0;
        windows.close_window(had_alert);
        let after = windows.snapshot();
        let instr = build_equilibrium_instruction(
            in_rate,
            digest_rate,
            after.alert_free_windows as i64,
            cfg.alert_free_windows_required,
            after.last_coverage,
            cfg.coverage_min,
            after.certified_streak as i64,
            cfg.certified_streak_min,
        );
        // 读回基线：仅认发射之后且指令队列清空的快照（快照不可得=本轮必超时）
        let baseline_version = match api.snapshot().await {
            Ok((_, _, v)) => v,
            Err(_) => u64::MAX,
        };
        let view = match api.send_command(instr) {
            Ok(_) => match read_verdict(&api, baseline_version).await {
                Some((stable, cause)) => EquilibriumView {
                    stable,
                    last_cause: cause,
                    window_secs: cfg.window_secs,
                    in_rate,
                    digest_rate,
                },
                None => EquilibriumView {
                    stable: None,
                    last_cause: Some("readback_timeout".to_string()),
                    window_secs: cfg.window_secs,
                    in_rate,
                    digest_rate,
                },
            },
            Err(e) => EquilibriumView {
                stable: None,
                last_cause: Some(format!("reactor_unavailable: {e}")),
                window_secs: cfg.window_secs,
                in_rate,
                digest_rate,
            },
        };
        publish_view(view);
    }
}

/// 读回本窗判定：轮询直至快照版本越过发射基线且指令队列清空，取
/// `payload.equilibrium / equilibrium_cause`。超时 = None（fail-visible
/// 不猜）。注释性竞态余量：基线捕获与指令处理之间若有无关 fact 抬版本，
/// 读回可能取到上一窗判定——均衡判定本身幂等收敛，演练面流量受控。
async fn read_verdict(
    api: &crate::api::server::GovernanceApi,
    baseline_version: u64,
) -> Option<(Option<bool>, Option<String>)> {
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
        let verdict = business.get("equilibrium").and_then(|v| v.as_str());
        let cause = business
            .get("equilibrium_cause")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        return Some((verdict.map(|s| s == "stable"), cause));
    }
}

/// 组装 `equilibrium_check` 八参指令（立法 v0.10.0 参数面：调用方直传
/// 快照差分与阈值，规则层零数据重塑）。
#[allow(clippy::too_many_arguments)]
fn build_equilibrium_instruction(
    in_rate: i64,
    digest_rate: i64,
    alert_free_windows: i64,
    alert_free_windows_required: i64,
    coverage: i64,
    coverage_min: i64,
    certified_streak: i64,
    certified_streak_min: i64,
) -> evorule_tcb::JsonValue {
    use std::collections::BTreeMap;

    use evorule_tcb::JsonValue;

    let mut params = BTreeMap::new();
    params.insert("in_rate".to_string(), JsonValue::Integer(in_rate));
    params.insert("digest_rate".to_string(), JsonValue::Integer(digest_rate));
    params.insert(
        "alert_free_windows".to_string(),
        JsonValue::Integer(alert_free_windows),
    );
    params.insert(
        "alert_free_windows_required".to_string(),
        JsonValue::Integer(alert_free_windows_required),
    );
    params.insert("coverage".to_string(), JsonValue::Integer(coverage));
    params.insert("coverage_min".to_string(), JsonValue::Integer(coverage_min));
    params.insert(
        "certified_streak".to_string(),
        JsonValue::Integer(certified_streak),
    );
    params.insert(
        "certified_streak_min".to_string(),
        JsonValue::Integer(certified_streak_min),
    );
    let mut instr = BTreeMap::new();
    instr.insert("type".to_string(), JsonValue::string("equilibrium_check"));
    instr.insert("params".to_string(), JsonValue::Object(params));
    JsonValue::Object(instr)
}

/// Windowed observation state behind the eight `equilibrium_check` parameters.
#[derive(Debug, Default)]
pub struct EquilibriumWindows {
    inner: Mutex<WindowsState>,
}

#[derive(Debug, Default)]
struct WindowsState {
    proposals_in_total: u64,
    digested_total: u64,
    alert_free_windows: u64,
    certified_streak: u64,
    last_coverage: i64,
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

    /// One certificate outcome: stable extends the certified streak, anything else resets it.
    pub fn record_certificate(&self, stable: bool) {
        let mut s = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if stable {
            s.certified_streak += 1;
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

    /// Snapshot handed to the rule layer as the eight `equilibrium_check` params
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
    fn test_snapshot_independent_of_shared_gauges() {
        // Unregistered path: snapshot must work without any shared gauge handles.
        let eq = EquilibriumWindows::new();
        eq.close_window(false);
        assert_eq!(eq.snapshot().alert_free_windows, 1);
    }

    #[test]
    fn test_shared_instance_and_pending_alerts() {
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
}

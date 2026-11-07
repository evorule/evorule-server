//! Companion-side equilibrium observation aggregator (B-182 / rule-layer v0.10.0 intake).
//!
//! Maintains the windowed counters that `equilibrium_check` (server_eval.json v0.10.0)
//! adjudicates over: proposal intake vs digestion rates, consecutive alert-free windows,
//! latest verification coverage and the certified-streak. The rule layer only ever sees
//! a caller-supplied snapshot — window aggregation and streak accounting stay here
//! (zero data reshaping inside the rule layer).
//!
//! Event sources (propose / digest / alert / window close / certificate / coverage) are
//! wired by the companion scheduling face in a later batch (P6 criticality drill); the
//! gauges below already expose the live state on the metrics surface.

use std::sync::Mutex;

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
}

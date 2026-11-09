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

use std::collections::VecDeque;
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
/// `equilibrium_check` 规则与影子双跑机制已退役）缓存视图供 status 呈现。
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
        // 特征量按定义表口径统一提取（提取顺序与平移前逐点一致：
        // 快照 → 窗警排水 → 窗结算 → 结算后快照）。
        let snap = windows.snapshot();
        let window_alert_count = drain_pending_alerts();
        let had_alert = window_alert_count > 0;
        windows.close_window(had_alert);
        windows.record_window_alert_count(window_alert_count);
        let after = windows.snapshot();
        let features = extract_feature_values(&prev_totals, &after, window_alert_count);
        // 复算凭证实锚（元数据面）：本窗结算观测记录推进观测流增量哈希
        // 链，审计日志留痕；不进发射参数与 status 契约（契约冻结不动）。
        let anchor =
            windows.settle_window_evidence(&prev_totals, &after, window_alert_count, &features);
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
            Ok(_) => match read_verdict(&api, baseline_version, "equilibrium", "equilibrium_cause")
                .await
            {
                Some((verdict, cause)) => EquilibriumView {
                    stable: verdict.as_deref().map(|s| s == "stable"),
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

/// Windowed observation state behind the `equilibrium_gate` parameters.
#[derive(Debug, Default)]
pub struct EquilibriumWindows {
    inner: Mutex<WindowsState>,
}

/// 窗警量尾部环形缓存容量（τ 相变分析面的观察窗深；只加不改——
/// 发射面契约与 status 契约零动，本缓存仅为实验/分析面拉取服务）。
const WINDOW_ALERT_TAIL_CAP: usize = 16;

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
        let record = build_settlement_record(window_seq, prev, after, window_alert_count, features);
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
}

/// 本窗结算观测记录（复算凭证输入面）：窗序 + 结算前后快照全量 +
/// 窗警量 + 全部注册特征值 + formula_ref 版本戳——观测流重放的最小
/// 完备证据集（输入+公式版本+输出三要素齐备）。
fn build_settlement_record(
    window_seq: u64,
    prev: &WindowsSnapshot,
    after: &WindowsSnapshot,
    window_alert_count: u64,
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
        ]
        .into_iter()
        .collect();
        let analysis_only: std::collections::BTreeSet<_> =
            registry_ids.difference(&extracted_ids).copied().collect();
        assert_eq!(
            analysis_only, expected_analysis,
            "registry entries beyond window extraction must be exactly the \
             analysis-face tau features"
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
        let record = build_settlement_record(7, &prev, &after, 3, &features);
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
            let a1 = eq.settle_window_evidence(&genesis, &after_first, 0, &first_features);
            let second_features =
                extract_feature_values(&after_first, &after_first, alert_count_second);
            let a2 = eq.settle_window_evidence(
                &after_first,
                &after_first,
                alert_count_second,
                &second_features,
            );
            let a3 = eq.settle_window_evidence(
                &after_first,
                &after_first,
                alert_count_second,
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
}

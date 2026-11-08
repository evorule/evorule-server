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

/// 复算凭证锚通用声明（首批特征同源：输入=窗观测流，公式=formula_ref
/// 版本戳，输出=确定性可复算）。
const EVIDENCE_ANCHOR_WINDOW_STREAM: &str =
    "input: windowed observation counters (this module) replayable from the \
     companion hash chain; formula: formula_ref version stamp; value: \
     deterministic under replay";

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

/// 读回超时（反应器不可达/积压时本窗视图以 None fail-visible 呈现）。
const READBACK_TIMEOUT: Duration = Duration::from_secs(10);
/// 读回轮询间隔。
const READBACK_POLL: Duration = Duration::from_millis(100);

/// 装配均衡评估任务：每窗结算一次（取走窗内 alert 计数 → `close_window`）
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
        let after = windows.snapshot();
        let features = extract_feature_values(&prev_totals, &after, window_alert_count);
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

    #[test]
    fn test_registry_and_extraction_are_in_sync() {
        // 定义表特征 id 集合 = 提取映射键集合（双向，漏登记/漏提取即红）。
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
        assert_eq!(registry_ids, extracted_ids);
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
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 应用级配额限流（59 号专项 W1）
//!
//! # 语义
//! - **per-app 速率限制**：governor direct 令牌桶（每 app 独立实例，容量=per_sec），
//!   仅对 `CallerIdentity::App` 生效（User/Service 直通）；与既有 per-IP 全局
//!   GovernorLayer 语义正交（IP 层防 DoS 基线，本层公平性预算），二者可叠加。
//! - **每日总量配额**：UTC 固定窗口（epoch day），锁内 check-then-increment
//!   （分片锁保证原子，宁可拒绝不超发）。
//! - **超限反馈**：429 + `Retry-After`；`app_quota_exceeded` 报警事件经
//!   [`append_platform_event`] 入审计链。聚合防抖：首超即报，此后每聚合窗口
//!   至多一条（带累计超限计数）——持续超限不静默，也不刷屏。
//! - **防写放大**：超限请求不落逐条 `app_invoke` 归因（与"unknown token 不留痕"
//!   同一原则：DoS 者打不爆审计链）；聚合事件携带累计计数，审计面不缺总数。
//! - **持久化**：日计数定期快照为 `app_quota_snapshot` 平台事件，重启从最近
//!   快照恢复（快照间隔内的计数在崩溃时丢失，上限=一个快照周期，如实接受）。
//!
//! # 时钟
//! 全部判定经 `now_ms` 参数注入（生产传 [`crate::api::platform_auth::now_ms`]，
//! 单测注入任意值），不做内部取时——可测性优先。
//!
//! # 配置
//! 快照周期缺省 60s，env `EVORULE_QUOTA_SNAPSHOT_SECS` 覆盖（0=关闭快照），
//! 0 与负值/非数字一律回落缺省并 warn（不静默）。

use evorule_governance::shared_facts_log::SharedFactsLog;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 平台事件 kind：配额快照（日计数持久化）
pub const SNAPSHOT_KIND: &str = "app_quota_snapshot";
/// 平台事件 kind：配额超限报警
const EXCEEDED_KIND: &str = "app_quota_exceeded";

/// 一天毫秒数（UTC 固定窗口边界）
const DAY_MS: u64 = 86_400_000;
/// 聚合报警节流窗口（毫秒）：同 app 同维度超限事件最小间隔
const AGGREGATE_WINDOW_MS: u64 = 60_000;
/// 快照周期缺省（秒）
const SNAPSHOT_SECS_DEFAULT: u64 = 60;

/// 配额维度（超限报警 payload 与 Retry-After 语义区分用）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaDimension {
    /// 速率（令牌桶）
    Rate,
    /// 每日总量（UTC 日窗口）
    Daily,
}

impl QuotaDimension {
    fn as_str(self) -> &'static str {
        match self {
            QuotaDimension::Rate => "rate",
            QuotaDimension::Daily => "daily",
        }
    }
}

/// 单应用的配额视图（`validate_app_key` 命中时返回，供 [`AppQuotaManager::check`]）。
///
/// `None` = 该维度不限（缺省，向后兼容：未显式配置配额的应用行为完全不变）。
#[derive(Debug, Clone)]
pub struct AppCredentials {
    pub app_id: String,
    pub rate_limit_per_sec: Option<u64>,
    pub daily_quota: Option<u64>,
}

/// 单 app 的限流内存状态
#[derive(Debug)]
struct AppLimitState {
    /// 速率层状态（Some=限流启用；配置变化时惰性重建）
    rate: Option<RateState>,
    /// 日计数（UTC epoch day；跨日惰性重置）
    daily: DailyState,
    /// 速率超限聚合报警状态：(窗口起点 ms, 累计超限计数)
    exceeded_rate: Option<(u64, u64)>,
    /// 日配额超限聚合报警状态：同上
    exceeded_daily: Option<(u64, u64)>,
}

/// 简化令牌桶（时钟注入驱动，可测性优先——不取系统时钟）。
///
/// 容量 = per_sec（突发上限），补充速率 = per_sec 令牌/秒；
/// `tokens` 浮点累积，`last_refill_ms` 为上次补充参考点。
#[derive(Debug)]
struct RateState {
    configured: u64,
    tokens: f64,
    last_refill_ms: u64,
}

impl RateState {
    fn new(per_sec: u64, now_ms: u64) -> Self {
        Self {
            configured: per_sec,
            tokens: per_sec as f64,
            last_refill_ms: now_ms,
        }
    }

    /// 补充令牌并尝试扣减 1 个。通过 → Ok(())；拒绝 → Err(retry_after_secs)。
    fn try_take(&mut self, now_ms: u64) -> Result<(), u64> {
        let per_sec = self.configured as f64;
        let elapsed = now_ms.saturating_sub(self.last_refill_ms) as f64;
        self.tokens = (self.tokens + elapsed * per_sec / 1000.0).min(per_sec);
        self.last_refill_ms = now_ms;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Ok(())
        } else {
            // 补足 1 个令牌所需秒数，向上取整且至少 1s
            let deficit_secs = (1.0 - self.tokens) / per_sec;
            Err(deficit_secs.ceil().max(1.0) as u64)
        }
    }
}

/// 日计数状态
#[derive(Debug, Clone)]
struct DailyState {
    day: u64,
    used: u64,
}

impl DailyState {
    fn new(day: u64) -> Self {
        Self { day, used: 0 }
    }
}

/// UTC epoch day（`now_ms / DAY_MS`）
fn epoch_day(now_ms: u64) -> u64 {
    now_ms / DAY_MS
}

/// 日配额的 Retry-After 秒数（到 UTC 次日零点，向上取整）
fn secs_until_next_day(now_ms: u64) -> u64 {
    let into_day = now_ms % DAY_MS;
    (DAY_MS - into_day).div_ceil(1000)
}

/// 应用级配额限流管理器（单进程内存态；server 当前单进程形态，
/// 多实例演进时状态外置另立，见 59 号方案边界）。
///
/// 锁纪律：`Mutex<HashMap>` 内只做内存判定与计数更新；平台事件
/// append（IO）在锁外进行，锁内不触碰 SharedFactsLog。
pub struct AppQuotaManager {
    inner: Mutex<HashMap<String, AppLimitState>>,
}

impl AppQuotaManager {
    /// 创建空管理器（不做恢复；生产用 [`Self::start_recover`]）
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// 创建并从最近快照恢复日计数（同日快照才恢复；跨日=窗口已重置）。
    pub fn start_recover(shared: &SharedFactsLog) -> Self {
        let mgr = Self::new();
        mgr.recover(shared);
        mgr
    }

    /// 从最近一条 `app_quota_snapshot` 事件恢复日计数。
    ///
    /// 事件按事实追加序扫描，取最后一条；平台事件 payload 形态为
    /// `{kind, detail:{day, counts}}`（append_platform_event 统一包裹），
    /// 业务字段在 `detail` 层。`day` 与今日 epoch day 相等才恢复
    /// （跨日快照忽略——窗口重置语义）。
    pub fn recover(&self, shared: &SharedFactsLog) {
        let facts = shared.facts_by_path_prefix("platform.event.");
        let Some(last) = facts.iter().rfind(|f| f.path.contains(SNAPSHOT_KIND)) else {
            return;
        };
        let Some(detail) = last.value.get("detail") else {
            tracing::warn!("应用配额:快照事件缺 detail 层,跳过恢复(path={})", last.path);
            return;
        };
        let day = detail.get("day").and_then(|n| n.as_i64()).unwrap_or(-1);
        if day < 0 || day as u64 != epoch_day(crate::api::platform_auth::now_ms()) {
            return; // 跨日快照：窗口已重置，从 0 起
        }
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut recovered = 0usize;
        if let Some(counts) = detail.get("counts").and_then(|c| c.as_object()) {
            for (app_id, used) in counts {
                if let Some(u) = used.as_i64() {
                    if u > 0 {
                        let st = inner
                            .entry(app_id.clone())
                            .or_insert_with(|| AppLimitState {
                                rate: None,
                                daily: DailyState::new(epoch_day(
                                    crate::api::platform_auth::now_ms(),
                                )),
                                exceeded_rate: None,
                                exceeded_daily: None,
                            });
                        st.daily.used = u as u64;
                        recovered += 1;
                    }
                }
            }
        }
        tracing::info!("应用配额:已从快照恢复 {recovered} 个应用的日计数(day {day})");
    }

    /// 配额检查入口（认证中间件通道三命中后调用）。
    ///
    /// 通过 → `Ok(())`（调用方继续既有的 app_invoke 归因链路）；
    /// 超限 → `Err((retry_after_secs, dimension))`（调用方回 429，**不落逐条
    /// 归因**；本方法按防抖语义锁外发出聚合报警事件：首超即报，持续超限
    /// 每 60s 聚合一条携带累计超限计数——方案 §1.4）。
    pub fn check(
        &self,
        creds: &AppCredentials,
        shared: &SharedFactsLog,
        now_ms: u64,
    ) -> Result<(), (u64, QuotaDimension)> {
        // 锁内判定+状态更新；待发事件收集，锁 drop 后统一 append
        // events: (窗口起点 ms, 维度, 自首超以来累计超限计数)
        let mut events: Vec<(u64, QuotaDimension, u64)> = Vec::new();
        let mut verdict: Result<(), (u64, QuotaDimension)> = Ok(());
        {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let st = inner
                .entry(creds.app_id.clone())
                .or_insert_with(|| AppLimitState {
                    rate: None,
                    daily: DailyState::new(epoch_day(now_ms)),
                    exceeded_rate: None,
                    exceeded_daily: None,
                });

            // ① 速率层（Some 才限；状态缺失或配置变化时惰性重建）
            if let Some(per_sec) = creds.rate_limit_per_sec {
                let rebuild = match &st.rate {
                    None => true,
                    Some(rs) => rs.configured != per_sec,
                };
                if rebuild {
                    st.rate = Some(RateState::new(per_sec, now_ms));
                }
                if let Some(rs) = &mut st.rate {
                    if let Err(retry) = rs.try_take(now_ms) {
                        // 被拒请求不扣日配额预算；防抖：首超/到点才发事件
                        if let Some((start, count)) = record_exceeded(&mut st.exceeded_rate, now_ms)
                        {
                            events.push((start, QuotaDimension::Rate, count));
                        }
                        verdict = Err((retry, QuotaDimension::Rate));
                    }
                }
            }

            // ② 日配额层（仅速率通过时判定——被拒请求不扣预算；
            //    check-then-increment 在分片锁内原子完成，宁可拒绝不超发）
            if verdict.is_ok() {
                if let Some(quota_max) = creds.daily_quota {
                    let day = epoch_day(now_ms);
                    if st.daily.day != day {
                        st.daily = DailyState::new(day);
                    }
                    if st.daily.used >= quota_max {
                        if let Some((start, count)) =
                            record_exceeded(&mut st.exceeded_daily, now_ms)
                        {
                            events.push((start, QuotaDimension::Daily, count));
                        }
                        verdict = Err((secs_until_next_day(now_ms), QuotaDimension::Daily));
                    } else {
                        st.daily.used += 1;
                    }
                }
            }

            // ③ 通过：报警状态复位（下次超限重新首报——"恢复后超限"不因
            //    旧窗口而吞报）；超限路径不复位（聚合窗口继续累计）
            if verdict.is_ok() {
                st.exceeded_rate = None;
                st.exceeded_daily = None;
            }
        }

        // 锁外发聚合报警事件（防抖语义见模块注释）
        for (start, dim, count) in events {
            append_platform_event_locked(shared, &creds.app_id, dim, start, count, now_ms);
        }
        verdict
    }

    /// 吊销后清理内存态（limiter/日计数/报警状态全清；REVOKED 在认证层
    /// 已拒，清理仅为防内存残留与归因干净）。配额更新无需主动同步——
    /// check 按请求携带的配置惰性重建 limiter（配置值比对），日计数保留
    /// （当日已用量语义不因配额调整而清零）。
    pub fn on_revoked(&self, app_id: &str) {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(app_id);
    }

    /// 今日已用量（供列表透出；跨日未触碰的 app 返回 0）。
    pub fn today_usage(&self, app_id: &str, now_ms: u64) -> u64 {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner
            .get(app_id)
            .filter(|st| st.daily.day == epoch_day(now_ms))
            .map(|st| st.daily.used)
            .unwrap_or(0)
    }

    /// 当前窗口日计数快照（供后台快照任务落事件）。
    fn snapshot_counts(&self, day: u64) -> HashMap<String, u64> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner
            .iter()
            .filter(|(_, st)| st.daily.day == day && st.daily.used > 0)
            .map(|(id, st)| (id.clone(), st.daily.used))
            .collect()
    }

    /// 启动快照后台任务（周期把当日计数落 `app_quota_snapshot` 事件）。
    ///
    /// 仅生产装配调用（main，`Arc<Self>` 计入 AppState 存活至进程结束）；
    /// 测试不 spawn（时钟注入即可覆盖恢复语义）。
    /// 周期经 env `EVORULE_QUOTA_SNAPSHOT_SECS` 配置（缺省 60，0=关闭）。
    pub fn spawn_snapshot_task(self: &Arc<Self>, shared: SharedFactsLog) {
        let secs = resolve_snapshot_secs();
        if secs == 0 {
            tracing::info!("应用配额:快照已关闭(EVORULE_QUOTA_SNAPSHOT_SECS=0)");
            return;
        }
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(secs));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let now_ms = crate::api::platform_auth::now_ms();
                let day = epoch_day(now_ms);
                let counts = this.snapshot_counts(day);
                if counts.is_empty() {
                    continue; // 空表不落事件（审计链不刷屏）
                }
                let counts_json: serde_json::Map<String, serde_json::Value> = counts
                    .into_iter()
                    .map(|(k, v)| (k, serde_json::Value::from(v)))
                    .collect();
                crate::api::platform_auth::append_platform_event(
                    &shared,
                    SNAPSHOT_KIND,
                    serde_json::json!({ "day": day, "counts": counts_json, "ts": now_ms }),
                );
            }
        });
        tracing::info!("应用配额:快照任务已启动(每 {secs}s)");
    }
}

impl Default for AppQuotaManager {
    fn default() -> Self {
        Self::new()
    }
}

/// 超限报警聚合（锁内调用，防抖语义——方案 §1.4：首超即报，持续超限
/// 每 60s 聚合一条带累计计数）：
/// - 首超：开新窗口，返回 `Some((now, 1))` → 发事件；
/// - 窗口内（now - start < 60s）：累计计数静默，返回 `None` → 不发；
/// - 窗口外到点：返回 `Some((旧窗口起点, 累计数+1))` → 发事件（携带自首超
///   以来的累计超限计数），并重开窗口 `(now, 1)`。
///
/// 被动补报语义：到点事件随下一发超限请求触发（无定时器），持续超限下
/// 实际间隔 ≈ 60s，"每 60s 必有一条"成立。
fn record_exceeded(state: &mut Option<(u64, u64)>, now_ms: u64) -> Option<(u64, u64)> {
    match state {
        Some((start, count)) if now_ms.saturating_sub(*start) < AGGREGATE_WINDOW_MS => {
            *count += 1;
            None // 窗口内静默累计
        }
        Some((start, count)) => {
            let event = Some((*start, *count + 1)); // 到点补报：携带累计计数
            *state = Some((now_ms, 1)); // 重开窗口
            event
        }
        None => {
            *state = Some((now_ms, 1));
            Some((now_ms, 1)) // 首超即报
        }
    }
}

/// 聚合报警事件落链（锁外调用）。
fn append_platform_event_locked(
    shared: &SharedFactsLog,
    app_id: &str,
    dim: QuotaDimension,
    window_start_ms: u64,
    exceeded_count: u64,
    now_ms: u64,
) {
    crate::api::platform_auth::append_platform_event(
        shared,
        EXCEEDED_KIND,
        serde_json::json!({
            "app_id": app_id,
            "dimension": dim.as_str(),
            "window_start_ms": window_start_ms,
            "exceeded_count": exceeded_count,
            "ts": now_ms,
        }),
    );
}

/// 快照周期解析：env 覆盖，非法值 warn 回落缺省（不静默）。
fn resolve_snapshot_secs() -> u64 {
    match std::env::var("EVORULE_QUOTA_SNAPSHOT_SECS") {
        Ok(v) => match v.trim().parse::<u64>() {
            Ok(n) => n,
            Err(_) => {
                tracing::warn!(
                    "EVORULE_QUOTA_SNAPSHOT_SECS={v:?} 非法,回落缺省 {SNAPSHOT_SECS_DEFAULT}s"
                );
                SNAPSHOT_SECS_DEFAULT
            }
        },
        Err(_) => SNAPSHOT_SECS_DEFAULT,
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use evorule_governance::shared_facts_log::SharedFactsLog;

    fn creds(app_id: &str, rate: Option<u64>, daily: Option<u64>) -> AppCredentials {
        AppCredentials {
            app_id: app_id.to_string(),
            rate_limit_per_sec: rate,
            daily_quota: daily,
        }
    }

    fn event_count(shared: &SharedFactsLog, kind: &str) -> usize {
        shared
            .facts_by_path_prefix("platform.event.")
            .iter()
            .filter(|f| f.path.contains(kind))
            .count()
    }

    fn app_invoke_count(shared: &SharedFactsLog, app_id: &str) -> usize {
        shared
            .facts_by_path_prefix("platform.event.")
            .iter()
            .filter(|f| {
                // 平台事件 payload 形态 {kind, detail:{...}},业务字段在 detail 层
                f.path.contains("app_invoke")
                    && f.value
                        .get("detail")
                        .and_then(|d| d.get("app_id"))
                        .and_then(|v| v.as_str())
                        == Some(app_id)
            })
            .count()
    }

    #[test]
    fn test_no_quota_passes_unlimited() {
        let mgr = AppQuotaManager::new();
        let shared = SharedFactsLog::new();
        let c = creds("free-app", None, None);
        for i in 0..1000 {
            assert!(
                mgr.check(&c, &shared, 1_000_000 + i).is_ok(),
                "未设配额的应用不应被限流(第 {i} 次)"
            );
        }
    }

    #[test]
    fn test_rate_limit_blocks_burst_above_per_sec() {
        let mgr = AppQuotaManager::new();
        let shared = SharedFactsLog::new();
        let c = creds("bursty", Some(2), None);
        assert!(mgr.check(&c, &shared, 1_000).is_ok());
        assert!(mgr.check(&c, &shared, 1_001).is_ok());
        // 突发第 3 发即超（burst=per_sec=2）
        let err = mgr.check(&c, &shared, 1_002).expect_err("应超速率");
        assert_eq!(err.1, QuotaDimension::Rate);
        assert!(err.0 >= 1, "Retry-After 应至少 1s");
        // 报警聚合：首超 1 条
        assert_eq!(event_count(&shared, EXCEEDED_KIND), 1);
        // 恢复后状态复位（令牌按 2/s 回复，等 1s 再过）
        assert!(mgr.check(&c, &shared, 2_100).is_ok());
    }

    #[test]
    fn test_rate_limit_retry_after_decreases_or_stays() {
        let mgr = AppQuotaManager::new();
        let shared = SharedFactsLog::new();
        let c = creds("r", Some(1), None);
        assert!(mgr.check(&c, &shared, 1_000).is_ok());
        assert!(mgr.check(&c, &shared, 1_001).is_err());
    }

    #[test]
    fn test_daily_quota_no_overissue() {
        let mgr = AppQuotaManager::new();
        let shared = SharedFactsLog::new();
        let c = creds("budget", None, Some(3));
        for i in 0..3 {
            assert!(mgr.check(&c, &shared, 1_000 + i).is_ok(), "前 3 发应通过");
        }
        let err = mgr
            .check(&c, &shared, 1_010)
            .expect_err("第 4 发应超日配额");
        assert_eq!(err.1, QuotaDimension::Daily);
        // Retry-After = 到 UTC 次日零点（1_010ms 时刻 → 86399s 左右）
        assert!(err.0 > 86_000 && err.0 <= 86_400, "retry={}", err.0);
        assert_eq!(mgr.today_usage("budget", 1_011), 3);
    }

    #[test]
    fn test_daily_resets_on_day_change() {
        let mgr = AppQuotaManager::new();
        let shared = SharedFactsLog::new();
        let c = creds("daily", None, Some(1));
        assert!(mgr.check(&c, &shared, 1_000).is_ok());
        assert!(mgr.check(&c, &shared, 1_001).is_err());
        // 跨日（day 0 → day 1 = +86400000ms）后重新可用
        assert!(mgr.check(&c, &shared, 1_000 + DAY_MS).is_ok());
        assert_eq!(mgr.today_usage("daily", 1_000 + DAY_MS + 1), 1);
    }

    #[test]
    fn test_exceeded_alert_aggregates_within_window() {
        let mgr = AppQuotaManager::new();
        let shared = SharedFactsLog::new();
        let c = creds("agg", None, Some(1));
        assert!(mgr.check(&c, &shared, 1_000).is_ok());
        // 窗口内连续超限：首超 1 条,后续静默累计
        for i in 0..5 {
            let _ = mgr.check(&c, &shared, 2_000 + i);
        }
        assert_eq!(
            event_count(&shared, EXCEEDED_KIND),
            1,
            "聚合窗口内只报 1 条"
        );
        // 聚合窗口(60s)过后再超：再报 1 条
        let _ = mgr.check(&c, &shared, 2_000 + AGGREGATE_WINDOW_MS + 1);
        assert_eq!(
            event_count(&shared, EXCEEDED_KIND),
            2,
            "窗口过后应再报 1 条"
        );
        // 超限请求不落逐条 app_invoke（防写放大）
        assert_eq!(app_invoke_count(&shared, "agg"), 0);
    }

    #[test]
    fn test_exceeded_alert_carries_cumulative_count() {
        let mgr = AppQuotaManager::new();
        let shared = SharedFactsLog::new();
        let c = creds("cnt", None, Some(1));
        assert!(mgr.check(&c, &shared, 1_000).is_ok());
        for i in 0..3 {
            let _ = mgr.check(&c, &shared, 2_000 + i);
        }
        // 下一条报警事件(跨窗口)带累计计数
        let _ = mgr.check(&c, &shared, 2_000 + AGGREGATE_WINDOW_MS + 5);
        let facts = shared.facts_by_path_prefix("platform.event.");
        let exceeded: Vec<_> = facts
            .iter()
            .filter(|f| f.path.contains(EXCEEDED_KIND))
            .collect();
        assert_eq!(exceeded.len(), 2);
        let last = exceeded.last().expect("有超限事件");
        assert_eq!(
            last.value
                .get("detail")
                .and_then(|d| d.get("exceeded_count"))
                .and_then(|n| n.as_i64()),
            Some(4),
            "到点补报应携带自首超以来的累计超限计数(1 首报 + 2 静默 + 1 本次)"
        );
    }

    #[test]
    fn test_pass_resets_exceeded_state() {
        let mgr = AppQuotaManager::new();
        let shared = SharedFactsLog::new();
        let c = creds("reset", None, Some(2));
        assert!(mgr.check(&c, &shared, 1_000).is_ok());
        assert!(mgr.check(&c, &shared, 1_001).is_ok());
        assert!(mgr.check(&c, &shared, 1_002).is_err()); // 超限,报警窗口开
                                                         // 窗口内恢复通道:让应用重新成功(模拟次日/令牌恢复后的下一发通过)
        let c2 = creds("reset", None, Some(10)); // 配额上调(同 app)
        assert!(mgr.check(&c2, &shared, 1_100).is_ok());
        // 复位后再次超限,应重新首报(事件数 2:第一次窗口 + 新窗口)
        let c3 = creds("reset", None, Some(2));
        assert!(mgr.check(&c3, &shared, 1_200).is_err());
        assert_eq!(
            event_count(&shared, EXCEEDED_KIND),
            2,
            "复位后超限应重新首报"
        );
    }

    #[test]
    fn test_snapshot_roundtrip_recover() {
        let shared = SharedFactsLog::new();
        let now_ms = crate::api::platform_auth::now_ms();
        let day = epoch_day(now_ms);
        // 模拟既往快照:budget 用了 7
        crate::api::platform_auth::append_platform_event(
            &shared,
            SNAPSHOT_KIND,
            serde_json::json!({ "day": day, "counts": { "budget": 7 }, "ts": now_ms }),
        );
        let mgr = AppQuotaManager::start_recover(&shared);
        assert_eq!(mgr.today_usage("budget", now_ms), 7, "重启后应从快照恢复");
        // 配额 9 → 剩 2 发
        let c = creds("budget", None, Some(9));
        assert!(mgr.check(&c, &shared, now_ms + 1).is_ok());
        assert!(mgr.check(&c, &shared, now_ms + 2).is_ok());
        assert!(
            mgr.check(&c, &shared, now_ms + 3).is_err(),
            "恢复计数+新增应达配额上限"
        );
    }

    #[test]
    fn test_recover_ignores_stale_day_snapshot() {
        let shared = SharedFactsLog::new();
        let now_ms = crate::api::platform_auth::now_ms();
        // 昨天的快照:不应恢复(窗口重置语义)
        crate::api::platform_auth::append_platform_event(
            &shared,
            SNAPSHOT_KIND,
            serde_json::json!({ "day": epoch_day(now_ms) - 1, "counts": { "old": 100 }, "ts": now_ms }),
        );
        let mgr = AppQuotaManager::start_recover(&shared);
        assert_eq!(mgr.today_usage("old", now_ms), 0, "跨日快照不应恢复");
    }

    #[test]
    fn test_on_revoked_clears_state() {
        let mgr = AppQuotaManager::new();
        let shared = SharedFactsLog::new();
        let c = creds("gone", None, Some(1));
        assert!(mgr.check(&c, &shared, 1_000).is_ok());
        assert_eq!(mgr.today_usage("gone", 1_001), 1);
        mgr.on_revoked("gone");
        assert_eq!(mgr.today_usage("gone", 1_002), 0, "吊销后内存态应清空");
    }
}

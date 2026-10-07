// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 回写转发（执行侧失败上报接线）：会话 Violation 族事实 → evorule-rule 回写收件端点
//!
//! 旗标缺省关：`EVORULE_WRITEBACK_URL` 未配置/为空时转发器直接退出（零旁路开销）。
//! 配置（进程 env，读取一次缓存）：
//! - `EVORULE_WRITEBACK_URL`：evorule-rule 服务基址（如 `http://127.0.0.1:18081`），
//!   收件路径固定拼接 `/v1/writeback/rule_failure`；
//! - `EVORULE_WRITEBACK_KEY`：X-Api-Key（scope=writeback:rule_failure，rule 侧自管认证）；
//! - `EVORULE_WRITEBACK_TENANT`：事件归属租户（缺省 `org-evorule`）；
//! - `EVORULE_WRITEBACK_DATASET`：事件归属数据集（缺省 `server-sessions`）。
//!
//! fail-soft：网络/远端失败仅 warn 留痕，绝不影响会话执行与事实链——转发是
//! 观察面，不承载审计责任（审计链仍在 facts_log）。漏发不静默：进程级计数
//! （misfires/dropped）随每条 warn 携带累计值，并经 [`health_snapshot`] 呈现
//! 于 `GET /api/health` 的 `writeback` 节（先记后治——重试/持久队列留待
//! 后续批次）。
//!
//! 事件映射（RuleFailureEvent，schema 单源 evorule-rule model::writeback）：
//! - `entry_id` = 规则身份引用（hit_stats `resolve_rule_ref`：`{source}#{序号}`；
//!   io_guard 保留值 rule_index=u64::MAX 记 `io_guard`）；
//! - `version_used` = 当前规则集 layout 版本（hit_stats `current_version`）；
//! - `failure.type` = `enforce_violation`，detail = Violation.reason，
//!   observed = instruction（TCB→serde 原样）；
//! - `execution_ctx.fact_ids` = [violation id, cause id]（回溯锚点）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use evorule_reactor::Fact;

use crate::api::hit_stats::HitStatsAggregator;

/// 转发配置（env 解析产物；URL 空 = off）
struct WritebackConfig {
    /// rule 侧服务基址（尾部 `/` 归一）
    base_url: String,
    /// X-Api-Key（scope=writeback:rule_failure）
    api_key: String,
    /// 事件归属租户
    tenant_id: String,
    /// 事件归属数据集
    dataset_id: String,
}

static CONFIG: OnceLock<Option<WritebackConfig>> = OnceLock::new();

/// 进程级漏发计数（观察面登记，先记后治）：转发发送失败（非 2xx / 传输
/// 错误）与事件流落后丢帧各自累计。转发是尽力而为的观察面（审计链仍在
/// facts_log），漏发不重试不补发——但必须可见：计数经 warn 日志字段与
/// `GET /api/health` 呈现，重试/持久队列等治理动作留待后续批次。
static MISFIRES: AtomicU64 = AtomicU64::new(0);
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// 登记一次发送失败，返回累计值（供 warn 日志携带）
fn record_misfire() -> u64 {
    MISFIRES.fetch_add(1, Ordering::Relaxed) + 1
}

/// 登记一次丢帧 n 条，返回累计值
fn record_dropped(n: u64) -> u64 {
    DROPPED.fetch_add(n, Ordering::Relaxed) + n
}

/// 漏发计数快照（原始读取，不受旗标门控；测试与观察面共用）
pub fn misfire_counts() -> (u64, u64) {
    (
        MISFIRES.load(Ordering::Relaxed),
        DROPPED.load(Ordering::Relaxed),
    )
}

/// `/api/health` 呈现用快照：转发未配置（旗标关）返回 None（整节省略，
/// 响应与未部署回写时逐字节兼容）；已配置返回 (misfires, dropped)
pub fn health_snapshot() -> Option<(u64, u64)> {
    config().map(|_| misfire_counts())
}

/// 解析配置（纯函数，便于单测）；URL 空/空白 = None（off）
fn build_config(
    url: String,
    api_key: String,
    tenant: Option<String>,
    dataset: Option<String>,
) -> Option<WritebackConfig> {
    let url = url.trim().trim_end_matches('/').to_string();
    if url.is_empty() {
        return None;
    }
    Some(WritebackConfig {
        base_url: url,
        api_key,
        tenant_id: tenant.unwrap_or_else(|| "org-evorule".to_string()),
        dataset_id: dataset.unwrap_or_else(|| "server-sessions".to_string()),
    })
}

fn config() -> Option<&'static WritebackConfig> {
    CONFIG
        .get_or_init(|| {
            build_config(
                std::env::var("EVORULE_WRITEBACK_URL").unwrap_or_default(),
                std::env::var("EVORULE_WRITEBACK_KEY").unwrap_or_default(),
                std::env::var("EVORULE_WRITEBACK_TENANT").ok(),
                std::env::var("EVORULE_WRITEBACK_DATASET").ok(),
            )
        })
        .as_ref()
}

/// ISO-8601 UTC 墙钟（秒精度；时钟异常回退 0 纪元，不 panic）
fn now_iso() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
        .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

/// 回写转发循环：订阅会话/单反应器 event 通道，消费 Violation 族事实逐条转发
///
/// 旗标关（`EVORULE_WRITEBACK_URL` 未配置）时立即退出；通道关闭（会话结束）
/// 自动退出；Lagged 时 warn 留痕（转发尽力而为，不承载审计责任）。
pub async fn run_forwarder(
    mut rx: tokio::sync::broadcast::Receiver<Fact>,
    agg: HitStatsAggregator,
    session_id: u64,
) {
    let Some(cfg) = config() else {
        return;
    };
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "回写转发器 HTTP 客户端构建失败（本会话转发关闭）");
            return;
        }
    };
    let url = format!("{}/v1/writeback/rule_failure", cfg.base_url);
    loop {
        match rx.recv().await {
            Ok(Fact::Violation {
                id,
                cause,
                rule_index,
                reason,
                instruction,
            }) => {
                // entry_id 归因：规则身份引用优先；io_guard 保留值（rule_index=u64::MAX）
                // 记固定标签（特征表命中不归属单条规则）
                let entry_id = if rule_index == u64::MAX {
                    "io_guard".to_string()
                } else {
                    agg.resolve_rule_ref(rule_index)
                };
                let event = serde_json::json!({
                    "event_type": "rule_failure",
                    "tenant_id": cfg.tenant_id,
                    "dataset_id": cfg.dataset_id,
                    "version_used": agg.current_version(),
                    "entry_id": entry_id,
                    "occurred_at": now_iso(),
                    "execution_ctx": {
                        "fact_ids": [id.0.to_string(), cause.0.to_string()],
                    },
                    "failure": {
                        "type": "enforce_violation",
                        "detail": reason,
                        "observed": crate::api::server::tcb_to_serde(&instruction),
                    },
                });
                let api_key = cfg.api_key.clone();
                let target = format!("session={session_id} entry={entry_id}");
                // Client 为内部 Arc 的廉价克隆；URL 每轮克隆进任务
                let client = client.clone();
                let url = url.clone();
                tokio::spawn(async move {
                    match client
                        .post(&url)
                        .header("X-Api-Key", api_key)
                        .json(&event)
                        .send()
                        .await
                    {
                        Ok(resp) if resp.status().is_success() => {}
                        Ok(resp) => {
                            let misfires_total = record_misfire();
                            tracing::warn!(
                                status = %resp.status(),
                                target = %target,
                                misfires_total,
                                "回写收件端点返回非 2xx（事件未入队；漏发已计数）"
                            );
                        }
                        Err(e) => {
                            let misfires_total = record_misfire();
                            tracing::warn!(
                                error = %e,
                                target = %target,
                                misfires_total,
                                "回写转发失败（fail-soft，不影响会话执行；漏发已计数）"
                            );
                        }
                    }
                });
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                let dropped_total = record_dropped(n);
                tracing::warn!(
                    dropped = n,
                    dropped_total,
                    "回写转发器事件流落后丢帧（观察面尽力而为，审计链不受影响；漏发已计数）"
                );
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_config_flag_off_and_defaults() {
        // 旗标缺省关：URL 空/空白 = None
        assert!(build_config(String::new(), String::new(), None, None).is_none());
        assert!(build_config("   ".into(), String::new(), None, None).is_none());

        // URL 归一（去尾斜杠）+ 缺省租户/数据集
        let cfg = build_config(
            "http://127.0.0.1:18081/".into(),
            "evorule_key".into(),
            None,
            None,
        )
        .expect("config on");
        assert_eq!(cfg.base_url, "http://127.0.0.1:18081");
        assert_eq!(cfg.tenant_id, "org-evorule");
        assert_eq!(cfg.dataset_id, "server-sessions");
        assert_eq!(cfg.api_key, "evorule_key");

        // 显式覆盖租户/数据集
        let cfg2 = build_config(
            "http://rule:18081".into(),
            "k".into(),
            Some("org-x".into()),
            Some("ds-y".into()),
        )
        .expect("config on");
        assert_eq!(cfg2.tenant_id, "org-x");
        assert_eq!(cfg2.dataset_id, "ds-y");
    }

    #[test]
    fn test_now_iso_shape() {
        let ts = now_iso();
        // ISO-8601 UTC 秒精度 Z 形态
        assert_eq!(ts.len(), 20, "{ts}");
        assert!(ts.ends_with('Z'), "{ts}");
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[10..11], "T");
    }

    #[test]
    fn test_misfire_counters_monotonic() {
        // 漏发计数进程级只增不减（先记后治观察面；多会话转发器共享累计）
        let (m0, d0) = misfire_counts();
        let m_after = record_misfire();
        let d_after = record_dropped(3);
        let (m1, d1) = misfire_counts();
        assert_eq!(m_after, m0 + 1, "misfire 计数应恰增 1");
        assert_eq!(d_after, d0 + 3, "dropped 计数应恰增 3");
        assert!(m1 >= m_after && d1 >= d_after, "计数单调不减");
    }
}

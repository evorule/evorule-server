// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 插件探活:external 插件进程运行时存活探测 + 状态翻转报警。
//!
//! 职责边界:
//! - 探测 = GET `{base_url}/health`(超时 3s):2xx 且 JSON 可解析 = online,
//!   404/405 = no_probe(未实现探活端点,不报警),其余(超时/连接拒绝/5xx/非 JSON)= offline
//! - 状态翻转即报(报警权系统独占,无条件行使):进入 offline 记
//!   `platform.event.plugin_offline`(error! 自诊断日志);退出 offline 记
//!   `plugin_online`(系统关警附全链留痕);首轮探测即 offline 同样报警——
//!   "offline 是唯一报警态,退出即关警",无报警悬挂
//! - 每轮探测后更新 PLUGIN_LIVENESS 快照(/api/health external 插件节合并呈现)
//! - 不做自动重启/拉起(watchdog 后置另立);native 插件不探活(随宿主生死);
//!   registry 绑定服务不探活(运维自有监控范畴)

use std::collections::BTreeMap;
use std::time::Duration;

use evorule_governance::shared_facts_log::SharedFactsLog;
use tracing::{error, info};

use super::platform_auth::append_platform_event;
#[cfg(test)]
use super::server::merge_liveness_into_plugins;
use super::server::{update_plugin_liveness, LivenessEntry};

/// 单插件探测超时
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// 探活目标(启动期从 external 插件挂载结果派生:id + base_url 零新增配置)
#[derive(Debug, Clone)]
pub struct ProbeTarget {
    pub id: String,
    pub base_url: String,
}

/// 探测结果三态
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeStatus {
    /// 2xx 且 JSON 可解析
    Online,
    /// 超时/连接拒绝/非 2xx(除 404/405)/2xx 但响应非 JSON
    Offline,
    /// /health 返回 404/405——插件进程可达但未实现探活端点(不报警)
    NoProbe,
}

impl ProbeStatus {
    fn as_str(&self) -> &'static str {
        match self {
            ProbeStatus::Online => "online",
            ProbeStatus::Offline => "offline",
            ProbeStatus::NoProbe => "no_probe",
        }
    }
}

/// HTTP 响应分类(纯函数,单测锁定):code + 响应体是否可解析为 JSON → 三态
pub fn classify_response(code: u16, body_is_json: bool) -> ProbeStatus {
    if code == 404 || code == 405 {
        return ProbeStatus::NoProbe;
    }
    if (200..300).contains(&code) && body_is_json {
        return ProbeStatus::Online;
    }
    ProbeStatus::Offline
}

/// 状态翻转报警决策(纯函数,单测锁定):
/// - 进入 offline(含首轮探测即 offline)→ plugin_offline 报警
/// - 退出 offline(恢复 online 或进程可达但 no_probe)→ plugin_online 关警留痕
/// - 其余迁移(online↔no_probe、状态持续)→ 无事件
pub fn transition_alert(prev: Option<&ProbeStatus>, now: &ProbeStatus) -> Option<&'static str> {
    let was_offline = matches!(prev, Some(ProbeStatus::Offline));
    let is_offline = *now == ProbeStatus::Offline;
    if is_offline && !was_offline {
        return Some("plugin_offline");
    }
    if was_offline && !is_offline {
        return Some("plugin_online");
    }
    None
}

/// 探测一轮:GET {base_url}/health → (三态, 失败摘要)
async fn probe_once(client: &reqwest::Client, base_url: &str) -> (ProbeStatus, Option<String>) {
    let url = format!("{}/health", base_url.trim_end_matches('/'));
    match client.get(&url).send().await {
        Ok(resp) => {
            let code = resp.status().as_u16();
            if code == 404 || code == 405 {
                // 进程可达但未实现探活端点——先取 body 前不做 JSON 解析,
                // 直接归 no_probe(存量插件可先跑起来,文档引导新插件实现)
                return (ProbeStatus::NoProbe, None);
            }
            let body_json = resp.json::<serde_json::Value>().await.ok().is_some();
            let status = classify_response(code, body_json);
            let err = if status == ProbeStatus::Online {
                None
            } else {
                Some(format!(
                    "HTTP {code}{}",
                    if body_json { "" } else { "(响应非 JSON)" }
                ))
            };
            (status, err)
        }
        Err(e) => {
            let summary = if e.is_timeout() {
                "探测超时(3s)".to_string()
            } else if e.is_connect() {
                "连接失败(进程未监听/端口不可达)".to_string()
            } else {
                format!("请求失败: {e}")
            };
            (ProbeStatus::Offline, Some(summary))
        }
    }
}

/// 报警事件落链 + error!/info! 自诊断日志(报警 fact 与处置留痕全量,
/// 人类面安静不等于无报警)
fn emit_alert(shared: &SharedFactsLog, kind: &str, id: &str, base_url: &str, err: Option<&str>) {
    let detail = serde_json::json!({
        "plugin_id": id,
        "base_url": base_url,
        "error": err,
    });
    append_platform_event(shared, kind, detail);
    match kind {
        "plugin_offline" => error!(
            "插件离线: {id}（{base_url}）{}（自诊断指引: ① 检查插件进程是否存活; \
             ② 检查端口/启动脚本; ③ 恢复后下轮探活自动记 plugin_online 关警）",
            err.unwrap_or("未知错误")
        ),
        "plugin_online" => {
            info!("插件恢复在线: {id}（{base_url}）— 系统关警（plugin_online 事件已入链留痕）")
        }
        _ => {}
    }
}

/// 构造本轮存活快照(纯逻辑,便于单测锁定字段语义)
fn liveness_entry(
    status: &ProbeStatus,
    now_ms: u64,
    last_ok_ts: Option<u64>,
    last_error: Option<String>,
) -> LivenessEntry {
    LivenessEntry {
        status: status.as_str().to_string(),
        last_probe_ts: now_ms,
        last_ok_ts,
        last_error,
    }
}

/// 启动常驻探活任务(tokio::spawn;与 log_cleanup_task 同模式)。
/// targets 为空 = 无 external 插件,不 spawn(纯内存零开销)。
/// 任务随进程生存——server shutdown 即进程退出,无需独立取消句柄
/// (与 log_cleanup_task 既有形态一致)。
pub fn spawn_probe_task(targets: Vec<ProbeTarget>, interval: Duration, shared: SharedFactsLog) {
    if targets.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let client = match reqwest::Client::builder().timeout(PROBE_TIMEOUT).build() {
            Ok(c) => c,
            Err(e) => {
                error!("插件探活任务启动失败(HTTP client 构建失败): {e}");
                return;
            }
        };
        // 上轮状态表 + 最近在线时间表(探活任务私有内存态)
        let mut prev: BTreeMap<String, ProbeStatus> = BTreeMap::new();
        let mut last_ok: BTreeMap<String, u64> = BTreeMap::new();
        loop {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            for t in &targets {
                let (status, err) = probe_once(&client, &t.base_url).await;
                if status == ProbeStatus::Online {
                    last_ok.insert(t.id.clone(), now_ms);
                }
                // 状态翻转即报(首轮 offline 也报;offline 退出即关警留痕)
                if let Some(kind) = transition_alert(prev.get(&t.id), &status) {
                    emit_alert(&shared, kind, &t.id, &t.base_url, err.as_deref());
                }
                update_plugin_liveness(
                    &t.id,
                    liveness_entry(&status, now_ms, last_ok.get(&t.id).copied(), err),
                );
                prev.insert(t.id.clone(), status);
            }
            tokio::time::sleep(interval).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== classify_response 三态分类 =====

    #[test]
    fn test_classify_200_json_is_online() {
        assert_eq!(classify_response(200, true), ProbeStatus::Online);
        assert_eq!(classify_response(204, true), ProbeStatus::Online);
    }

    #[test]
    fn test_classify_200_non_json_is_offline() {
        assert_eq!(classify_response(200, false), ProbeStatus::Offline);
    }

    #[test]
    fn test_classify_404_405_is_no_probe() {
        assert_eq!(classify_response(404, false), ProbeStatus::NoProbe);
        assert_eq!(classify_response(405, true), ProbeStatus::NoProbe);
    }

    #[test]
    fn test_classify_5xx_and_others_offline() {
        assert_eq!(classify_response(500, true), ProbeStatus::Offline);
        assert_eq!(classify_response(503, false), ProbeStatus::Offline);
        assert_eq!(classify_response(401, true), ProbeStatus::Offline);
        assert_eq!(classify_response(301, true), ProbeStatus::Offline);
    }

    // ===== transition_alert 翻转矩阵 =====

    #[test]
    fn test_first_probe_offline_alerts() {
        // 首轮探测即 offline:启动即故障不是"无翻转"豁免
        assert_eq!(
            transition_alert(None, &ProbeStatus::Offline),
            Some("plugin_offline")
        );
    }

    #[test]
    fn test_first_probe_online_or_no_probe_silent() {
        assert_eq!(transition_alert(None, &ProbeStatus::Online), None);
        assert_eq!(transition_alert(None, &ProbeStatus::NoProbe), None);
    }

    #[test]
    fn test_online_to_offline_alerts_and_recovery_closes() {
        assert_eq!(
            transition_alert(Some(&ProbeStatus::Online), &ProbeStatus::Offline),
            Some("plugin_offline")
        );
        // 恢复 → 系统关警(全链留痕)
        assert_eq!(
            transition_alert(Some(&ProbeStatus::Offline), &ProbeStatus::Online),
            Some("plugin_online")
        );
        // 进程恢复但 /health 仍未实现(404 = TCP+HTTP 可达)→ 同样关警,
        // 报警面语义 = offline 是唯一报警态,退出即关警,无报警悬挂
        assert_eq!(
            transition_alert(Some(&ProbeStatus::Offline), &ProbeStatus::NoProbe),
            Some("plugin_online")
        );
    }

    #[test]
    fn test_no_repeated_alerts_and_no_probe_transitions_silent() {
        // 持续 offline 不重复报警
        assert_eq!(
            transition_alert(Some(&ProbeStatus::Offline), &ProbeStatus::Offline),
            None
        );
        // online↔no_probe 迁移不产生事件(no_probe 不报警语义)
        assert_eq!(
            transition_alert(Some(&ProbeStatus::Online), &ProbeStatus::NoProbe),
            None
        );
        assert_eq!(
            transition_alert(Some(&ProbeStatus::NoProbe), &ProbeStatus::Online),
            None
        );
        // 持续 online/no_probe 静默
        assert_eq!(
            transition_alert(Some(&ProbeStatus::Online), &ProbeStatus::Online),
            None
        );
    }

    // ===== liveness_entry 快照字段语义 =====

    #[test]
    fn test_liveness_entry_fields() {
        let e = liveness_entry(&ProbeStatus::Online, 1000, Some(900), None);
        assert_eq!(e.status, "online");
        assert_eq!(e.last_probe_ts, 1000);
        assert_eq!(e.last_ok_ts, Some(900));
        assert!(e.last_error.is_none());

        let e = liveness_entry(
            &ProbeStatus::Offline,
            2000,
            Some(900),
            Some("连接失败".to_string()),
        );
        assert_eq!(e.status, "offline");
        assert_eq!(e.last_error.as_deref(), Some("连接失败"));

        let e = liveness_entry(&ProbeStatus::NoProbe, 3000, None, None);
        assert_eq!(e.status, "no_probe");
        assert!(e.last_ok_ts.is_none());

        // 序列化省略语义:last_error 缺席时字段不出现(向后兼容呈现)
        let v = serde_json::to_value(liveness_entry(&ProbeStatus::Online, 1, None, None)).unwrap();
        assert!(v.get("last_error").is_none());
        assert!(v.get("last_ok_ts").is_none());
        assert_eq!(v.get("status").and_then(|s| s.as_str()), Some("online"));
    }

    // ===== merge_liveness_into_plugins 合并呈现 =====

    #[test]
    fn test_merge_into_external_plugin_node() {
        let plugins = serde_json::json!({
            "finance-config": {
                "enabled": true, "external": true, "services": ["finance_config_get"]
            },
            "demo-services": { "enabled": true, "services": ["demo_a"] }
        });
        let mut map = std::collections::BTreeMap::new();
        map.insert(
            "finance-config".to_string(),
            LivenessEntry {
                status: "offline".to_string(),
                last_probe_ts: 111,
                last_ok_ts: Some(100),
                last_error: Some("HTTP 500".to_string()),
            },
        );
        let merged = merge_liveness_into_plugins(Some(plugins), &map).unwrap();
        let fin = &merged["finance-config"];
        assert_eq!(fin["status"], "offline");
        assert_eq!(fin["last_probe"], 111);
        assert_eq!(fin["last_ok"], 100);
        assert_eq!(fin["last_error"], "HTTP 500");
        // external 标记与原字段保留
        assert_eq!(fin["external"], true);
        // native 插件节不受影响
        let demo = &merged["demo-services"];
        assert!(demo.get("status").is_none());
        assert_eq!(demo["enabled"], true);
    }

    #[test]
    fn test_merge_skips_native_and_unknown_ids() {
        let plugins = serde_json::json!({
            "demo-services": { "enabled": true }
        });
        let mut map = std::collections::BTreeMap::new();
        map.insert(
            "demo-services".to_string(), // native,非 external → 不合并
            LivenessEntry {
                status: "online".to_string(),
                last_probe_ts: 1,
                last_ok_ts: None,
                last_error: None,
            },
        );
        map.insert(
            "ghost-plugin".to_string(), // 不在健康快照中 → 忽略
            LivenessEntry {
                status: "online".to_string(),
                last_probe_ts: 2,
                last_ok_ts: None,
                last_error: None,
            },
        );
        let merged = merge_liveness_into_plugins(Some(plugins), &map).unwrap();
        assert!(merged["demo-services"].get("status").is_none());
        assert!(merged.get("ghost-plugin").is_none());
    }

    #[test]
    fn test_merge_empty_liveness_is_identity() {
        // 探活未运行(空表)→ 响应与启动期快照逐字节一致(向后兼容锁定)
        let plugins = serde_json::json!({
            "finance-config": { "enabled": true, "external": true }
        });
        let original = plugins.clone();
        let merged = merge_liveness_into_plugins(Some(plugins), &std::collections::BTreeMap::new());
        assert_eq!(merged.unwrap(), original);
    }

    #[test]
    fn test_merge_none_plugins_is_none() {
        let merged = merge_liveness_into_plugins(None, &std::collections::BTreeMap::new());
        assert!(merged.is_none());
    }

    // ===== ProbeStatus 字符串映射（/api/health status 字段值 SSOT）=====

    #[test]
    fn test_probe_status_str_mapping() {
        assert_eq!(ProbeStatus::Online.as_str(), "online");
        assert_eq!(ProbeStatus::Offline.as_str(), "offline");
        assert_eq!(ProbeStatus::NoProbe.as_str(), "no_probe");
    }
}

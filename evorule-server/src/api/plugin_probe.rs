// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 插件探活:external 插件进程运行时存活探测 + 状态翻转报警。
//!
//! 职责边界:
//! - 探测 = GET `{base_url}/health`(超时 3s):2xx 且 JSON 可解析 = online,
//!   404/405 = no_probe(未实现探活端点,不报警),
//!   401/403 = unauthorized(批次A:进程活着但鉴权被拒,告警语义=凭据/配置
//!   问题,重启无效——与 offline「进程死了」区分,看门狗侧对 unauthorized 不动作),
//!   其余(超时/连接拒绝/5xx/非 JSON)= offline
//! - 状态翻转即报(报警权系统独占,无条件行使):进入 offline 记
//!   `platform.event.plugin_offline`(error! 自诊断日志);进入 unauthorized 记
//!   `platform.event.plugin_unauthorized`(凭据/配置问题自诊断);退出报警态
//!   (offline/unauthorized → online 或 no_probe)记 `plugin_online`
//!   (系统关警附全链留痕);首轮探测即 offline/unauthorized 同样报警——
//!   "offline/unauthorized 是报警态,退出即关警",无报警悬挂
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

/// 探测结果四态
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeStatus {
    /// 2xx 且 JSON 可解析
    Online,
    /// 超时/连接拒绝/非 2xx(除 404/405/401/403)/2xx 但响应非 JSON
    Offline,
    /// /health 返回 404/405——插件进程可达但未实现探活端点(不报警)
    NoProbe,
    /// /health 返回 401/403——进程活着但鉴权被拒(批次A):
    /// 告警语义=凭据/配置问题,重启无效;与 offline(进程死了)区分,
    /// 看门狗侧不动作(5xx 维持 offline 不细分,Q7 裁定)
    Unauthorized,
}

impl ProbeStatus {
    fn as_str(&self) -> &'static str {
        match self {
            ProbeStatus::Online => "online",
            ProbeStatus::Offline => "offline",
            ProbeStatus::NoProbe => "no_probe",
            ProbeStatus::Unauthorized => "unauthorized",
        }
    }
}

/// HTTP 响应分类(纯函数,单测锁定):code + 响应体是否可解析为 JSON → 四态
pub fn classify_response(code: u16, body_is_json: bool) -> ProbeStatus {
    if code == 404 || code == 405 {
        return ProbeStatus::NoProbe;
    }
    if code == 401 || code == 403 {
        return ProbeStatus::Unauthorized;
    }
    if (200..300).contains(&code) && body_is_json {
        return ProbeStatus::Online;
    }
    ProbeStatus::Offline
}

/// 状态翻转报警决策(纯函数,单测锁定)。返回本轮要落链的事件序列(至多 2 个):
/// - 进入 offline(含首轮)→ plugin_offline 报警
/// - 进入 unauthorized(含首轮)→ plugin_unauthorized 报警(凭据/配置问题)
/// - 离开报警态(offline/unauthorized 恢复 online/no_probe)→ plugin_online 关警留痕
/// - 跨报警态迁移(offline↔unauthorized)→ 先关旧警再开新警(事件序 =
///   [plugin_online, 新警],链上如实呈现「进程活着但凭据错」的进展)
/// - 其余迁移(online↔no_probe、状态持续)→ 无事件
pub fn transition_alert(prev: Option<&ProbeStatus>, now: &ProbeStatus) -> Vec<&'static str> {
    let mut events: Vec<&'static str> = Vec::new();
    let was_offline = matches!(prev, Some(ProbeStatus::Offline));
    let is_offline = *now == ProbeStatus::Offline;
    let was_unauth = matches!(prev, Some(ProbeStatus::Unauthorized));
    let is_unauth = *now == ProbeStatus::Unauthorized;
    // 关警优先:离开原报警态(去向任何其他状态,含另一报警态)先关警
    if was_offline && !is_offline {
        events.push("plugin_online");
    }
    if was_unauth && !is_unauth {
        events.push("plugin_online");
    }
    // 开警:进入新报警态
    if is_offline && !was_offline {
        events.push("plugin_offline");
    }
    if is_unauth && !was_unauth {
        events.push("plugin_unauthorized");
    }
    events
}

/// 探测一轮:GET {base_url}/health → (四态, 失败摘要)
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
            if code == 401 || code == 403 {
                // 批次A:进程活着但鉴权被拒——凭据/配置问题,
                // 重启无效;与 offline(进程死了)区分,看门狗不动作
                return (
                    ProbeStatus::Unauthorized,
                    Some(format!(
                        "HTTP {code}(凭据/配置问题:进程活着但鉴权被拒,重启无效;\
                         检查插件鉴权配置/plugin_admin_token_env 注入)"
                    )),
                );
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
        "plugin_unauthorized" => error!(
            "插件鉴权被拒: {id}（{base_url}）{}（凭据/配置问题——进程活着,重启无效; \
             自诊断指引: ① 核对插件侧鉴权配置与 server 侧 plugin_admin_token_env 注入; \
             ② 凭据轮换后下轮探活自动记 plugin_online 关警）",
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
    // 批次B:并发探测句柄(目标 + join 句柄,按原序聚合结果);
    // 函数级类型别名化解 clippy::type_complexity
    type ProbeHandle = (
        ProbeTarget,
        tokio::task::JoinHandle<(ProbeStatus, Option<String>)>,
    );
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
            // 批次B:并发探测(tokio::spawn 逐目标并发,reqwest::Client
            // 内部 Arc 廉价克隆)——多插件慢/超时不再串行拖长整轮;结果按
            // 原序聚合,落链/快照次序与旧实现一致(不涉确定性面)
            let handles: Vec<ProbeHandle> = targets
                .iter()
                .map(|t| {
                    let client = client.clone();
                    let base_url = t.base_url.clone();
                    (
                        t.clone(),
                        tokio::spawn(async move { probe_once(&client, &base_url).await }),
                    )
                })
                .collect();
            for (t, handle) in handles {
                let (status, err) = match handle.await {
                    Ok(pair) => pair,
                    Err(e) => (
                        ProbeStatus::Offline,
                        Some(format!("探活任务异常终止(join 失败): {e}")),
                    ),
                };
                if status == ProbeStatus::Online {
                    last_ok.insert(t.id.clone(), now_ms);
                }
                // 状态翻转即报(首轮 offline/unauthorized 也报;报警态退出即关警留痕;
                // 跨报警态迁移先关旧警再开新警)
                for kind in transition_alert(prev.get(&t.id), &status) {
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
        assert_eq!(classify_response(301, true), ProbeStatus::Offline);
    }

    #[test]
    fn test_classify_401_403_is_unauthorized() {
        // 批次A:401/403 与 offline 分道——凭据/配置问题,重启无效
        assert_eq!(classify_response(401, true), ProbeStatus::Unauthorized);
        assert_eq!(classify_response(403, false), ProbeStatus::Unauthorized);
    }

    // ===== transition_alert 翻转矩阵 =====

    #[test]
    fn test_first_probe_offline_alerts() {
        // 首轮探测即 offline:启动即故障不是"无翻转"豁免
        assert_eq!(
            transition_alert(None, &ProbeStatus::Offline),
            vec!["plugin_offline"]
        );
    }

    #[test]
    fn test_first_probe_unauthorized_alerts() {
        // 批次A:首轮即 unauthorized 同样报警(凭据问题不悬挂)
        assert_eq!(
            transition_alert(None, &ProbeStatus::Unauthorized),
            vec!["plugin_unauthorized"]
        );
    }

    #[test]
    fn test_first_probe_online_or_no_probe_silent() {
        assert!(transition_alert(None, &ProbeStatus::Online).is_empty());
        assert!(transition_alert(None, &ProbeStatus::NoProbe).is_empty());
    }

    #[test]
    fn test_online_to_offline_alerts_and_recovery_closes() {
        assert_eq!(
            transition_alert(Some(&ProbeStatus::Online), &ProbeStatus::Offline),
            vec!["plugin_offline"]
        );
        // 恢复 → 系统关警(全链留痕)
        assert_eq!(
            transition_alert(Some(&ProbeStatus::Offline), &ProbeStatus::Online),
            vec!["plugin_online"]
        );
        // 进程恢复但 /health 仍未实现(404 = TCP+HTTP 可达)→ 同样关警,
        // 报警面语义 = 报警态退出即关警,无报警悬挂
        assert_eq!(
            transition_alert(Some(&ProbeStatus::Offline), &ProbeStatus::NoProbe),
            vec!["plugin_online"]
        );
    }

    #[test]
    fn test_unauthorized_transitions_open_and_close() {
        // online → unauthorized:开凭据警
        assert_eq!(
            transition_alert(Some(&ProbeStatus::Online), &ProbeStatus::Unauthorized),
            vec!["plugin_unauthorized"]
        );
        // unauthorized → online:关警留痕
        assert_eq!(
            transition_alert(Some(&ProbeStatus::Unauthorized), &ProbeStatus::Online),
            vec!["plugin_online"]
        );
        // unauthorized → no_probe(进程可达):同样关警
        assert_eq!(
            transition_alert(Some(&ProbeStatus::Unauthorized), &ProbeStatus::NoProbe),
            vec!["plugin_online"]
        );
        // 持续 unauthorized 不重复报警
        assert!(
            transition_alert(Some(&ProbeStatus::Unauthorized), &ProbeStatus::Unauthorized)
                .is_empty()
        );
    }

    #[test]
    fn test_cross_alarming_transition_closes_then_opens() {
        // offline → unauthorized:进程活着但凭据错——先关 offline 警再开凭据警
        assert_eq!(
            transition_alert(Some(&ProbeStatus::Offline), &ProbeStatus::Unauthorized),
            vec!["plugin_online", "plugin_unauthorized"]
        );
        // unauthorized → offline:凭据问题恶化成进程失联——先关凭据警再开离线警
        assert_eq!(
            transition_alert(Some(&ProbeStatus::Unauthorized), &ProbeStatus::Offline),
            vec!["plugin_online", "plugin_offline"]
        );
    }

    #[test]
    fn test_no_repeated_alerts_and_no_probe_transitions_silent() {
        // 持续 offline 不重复报警
        assert!(transition_alert(Some(&ProbeStatus::Offline), &ProbeStatus::Offline).is_empty());
        // online↔no_probe 迁移不产生事件(no_probe 不报警语义)
        assert!(transition_alert(Some(&ProbeStatus::Online), &ProbeStatus::NoProbe).is_empty());
        assert!(transition_alert(Some(&ProbeStatus::NoProbe), &ProbeStatus::Online).is_empty());
        // 持续 online/no_probe 静默
        assert!(transition_alert(Some(&ProbeStatus::Online), &ProbeStatus::Online).is_empty());
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
        // 批次A:unauthorized 为 /api/health status 新增值
        assert_eq!(ProbeStatus::Unauthorized.as_str(), "unauthorized");
    }

    #[test]
    fn test_liveness_entry_unauthorized_status() {
        // unauthorized 进快照:status 字段呈现新值,last_ok 不更新语义与 offline 一致
        let e = liveness_entry(
            &ProbeStatus::Unauthorized,
            4000,
            Some(3900),
            Some("HTTP 401(凭据/配置问题)".to_string()),
        );
        assert_eq!(e.status, "unauthorized");
        assert_eq!(e.last_error.as_deref(), Some("HTTP 401(凭据/配置问题)"));
    }
}

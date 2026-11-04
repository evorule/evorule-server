// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 伴跑提案治理链上行出站适配器（记录性转发；缺省关）
//!
//! 人工 approve 裁决完成后，将提案内容确定性转发至治理面提案入库端点
//! （`X-Api-Key: entries:propose` 族 scoped key）。与回写消费端 K3 出站
//! （evorule-rule `K3ProposeConfig`）完全同构：
//! - 配置仅 env 读取，URL 空/未配置 = 完全关闭（提案/裁决/分析/认证面
//!   零行为变化；上行失败不回滚裁决、不改任何既有语义——零拦截延伸）
//! - `entry_id` = 确定性幂等 slug（fnv1a64，K3 `k3_propose_entry_id` /
//!   evo-agent sediment 同款实现；非密码哈希、不入 evorule-hash 域注册表
//!   ——幂等键用途，同提案重复上行/重试不重复入库）
//! - 失败降级：传输失败/非 2xx 仅写回上行状态面并落 `CompanionUplink`
//!   链事实（由调用方处置），本模块只报 HTTP 终态不重试（重试走管理端点）
//!
//! 上行请求体形态照治理 propose 服务信封（`dataset_id` + `entry` + `cause`，
//! K3 出站同款调用形态：`POST {base}/api/services/knowledge-propose/invoke`）。

use serde_json::{json, Value};

/// 上行配置（env 外置，缺省关；凭据仅 env 不落盘）
#[derive(Debug, Clone)]
pub struct CompanionUplinkConfig {
    /// 治理服务基址（尾 `/` 归一）；上行路径固定拼
    /// `/api/services/knowledge-propose/invoke`
    pub base_url: String,
    /// X-Api-Key（治理侧 scoped key，entries:propose 族）
    pub api_key: String,
    /// 候选入账目标数据集（可选；未配置时请求体省略 `dataset_id` 键）
    pub dataset: Option<String>,
}

impl CompanionUplinkConfig {
    /// env 解析（main.rs 启动期一次读取）；URL 空/空白 = None（缺省关）
    pub fn from_env() -> Option<Self> {
        Self::build(
            std::env::var("EVORULE_COMPANION_UPLINK_URL").unwrap_or_default(),
            std::env::var("EVORULE_COMPANION_UPLINK_KEY").unwrap_or_default(),
            std::env::var("EVORULE_COMPANION_UPLINK_DATASET").unwrap_or_default(),
        )
    }

    /// 纯函数解析（单测锚点）；URL 空/空白 = None（off）
    fn build(url: String, api_key: String, dataset: String) -> Option<Self> {
        let url = url.trim().trim_end_matches('/').to_string();
        if url.is_empty() {
            return None;
        }
        let dataset = dataset.trim();
        Some(Self {
            base_url: url,
            api_key,
            dataset: if dataset.is_empty() {
                None
            } else {
                Some(dataset.to_string())
            },
        })
    }
}

/// 确定性 entry_id slug（fnv1a64 over `companion|{proposal_id}`；K3
/// `k3_propose_entry_id` 同款实现——同提案重提议得同 ID = 幂等锚，治理侧
/// 同 entry_id 重提议不产生重复条目。非密码哈希：幂等键用途，不入
/// evorule-hash 域注册表（域归属判定 2026-11-04 留痕施工实录）。）
pub fn companion_uplink_entry_id(proposal_id: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in format!("companion|{proposal_id}").as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("cc-{:016x}", h)
}

/// 上行 entry 载荷（字段面照获批设计：entry_id 派生自 proposal_id、
/// 规则集版本、findings 摘要与全文、提案/裁决时点、裁决人、来源标注）
pub fn companion_uplink_entry(
    proposal_id: &str,
    ruleset_version: &str,
    findings_digest: &str,
    findings: &Value,
    proposed_at: u64,
    adjudicated_at: Option<u64>,
    actor: &str,
) -> Value {
    json!({
        "entry_id": companion_uplink_entry_id(proposal_id),
        "ruleset_version": ruleset_version,
        "findings_digest": findings_digest,
        "findings": findings,
        "proposed_at": proposed_at,
        "adjudicated_at": adjudicated_at,
        "actor": actor,
        "source": "companion",
    })
}

/// 上行请求体（治理 propose 服务信封：dataset_id + entry + cause；
/// dataset 未配置时省略 `dataset_id` 键）
pub fn companion_uplink_payload(
    cfg: &CompanionUplinkConfig,
    entry: Value,
    proposal_id: &str,
    actor: &str,
) -> Value {
    let mut body = json!({
        "entry": entry,
        "cause": format!("companion uplink: proposal {proposal_id} approved by {actor}"),
        "source_session_id": Option::<String>::None,
    });
    if let Some(ds) = &cfg.dataset {
        body["dataset_id"] = json!(ds);
    }
    body
}

/// 出站 POST（reqwest 直连治理面；5s 超时照 K3 出站口径）。返回 HTTP
/// 终态码，Err=传输层失败（含超时；非 2xx 是 Ok 终态由调用方判级）。
pub async fn post_propose(cfg: &CompanionUplinkConfig, body: &Value) -> Result<u16, String> {
    let url = format!("{}/api/services/knowledge-propose/invoke", cfg.base_url);
    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .timeout(std::time::Duration::from_secs(5))
        .header("X-Api-Key", &cfg.api_key)
        .json(body)
        .send()
        .await
        .map_err(|e| format!("uplink transport error: {e}"))?;
    Ok(resp.status().as_u16())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// entry_id 幂等与可区分性：同提案恒同 ID（重试幂等锚）、异提案异 ID
    #[test]
    fn entry_id_deterministic_and_distinguishing() {
        let a1 = companion_uplink_entry_id("prop-v1-abc123");
        let a2 = companion_uplink_entry_id("prop-v1-abc123");
        assert_eq!(a1, a2, "同提案重派生得同 ID（幂等锚）");
        let b = companion_uplink_entry_id("prop-v1-def456");
        assert_ne!(a1, b, "异提案异 ID");
        assert!(
            a1.starts_with("cc-") && a1.len() == 19,
            "slug 形态 cc- + 16 hex：{a1}"
        );
        assert!(a1[3..].chars().all(|c| c.is_ascii_hexdigit()), "尾段全 hex");
    }

    /// 配置解析：URL 空/空白=off（缺省关）；dataset 空白归一为 None
    #[test]
    fn config_build_off_when_url_empty() {
        assert!(CompanionUplinkConfig::build(String::new(), "k".into(), "d".into()).is_none());
        assert!(CompanionUplinkConfig::build("   ".into(), "k".into(), "d".into()).is_none());
        let cfg = CompanionUplinkConfig::build(
            "http://127.0.0.1:18081/".into(),
            "k".into(),
            String::new(),
        )
        .expect("url 非空即开");
        assert_eq!(cfg.base_url, "http://127.0.0.1:18081", "尾 / 归一");
        assert!(cfg.dataset.is_none(), "dataset 空白=可选未配置");
    }

    /// payload：dataset 已配置携带 dataset_id；未配置省略该键
    #[test]
    fn payload_dataset_key_conditional() {
        let cfg_with = CompanionUplinkConfig {
            base_url: "http://x".into(),
            api_key: "k".into(),
            dataset: Some("gov-ds".into()),
        };
        let cfg_without = CompanionUplinkConfig {
            base_url: "http://x".into(),
            api_key: "k".into(),
            dataset: None,
        };
        let entry =
            companion_uplink_entry("prop-1", "v1", "dg", &json!({"a": 1}), 1, Some(2), "op");
        let body_with = companion_uplink_payload(&cfg_with, entry.clone(), "prop-1", "op");
        assert_eq!(body_with["dataset_id"], json!("gov-ds"));
        assert_eq!(
            body_with["entry"]["entry_id"],
            json!(companion_uplink_entry_id("prop-1"))
        );
        assert_eq!(body_with["entry"]["source"], json!("companion"));
        let body_without = companion_uplink_payload(&cfg_without, entry, "prop-1", "op");
        assert!(
            body_without.get("dataset_id").is_none(),
            "dataset 未配置省略键"
        );
    }
}

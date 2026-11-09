// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 分级裁决器 —— 消费 P3' verify 提案物料包，按治理矩阵决定
//! 「低危自动集成」或「降级人工闸链」。
//!
//! # 治理矩阵（元定律守卫×类别层×事实层三条件与）
//! 自动集成判据 = `类别∈低危 ∧ 配额内 ∧ 水位未冻结`；类别层对非低危
//! 一票否决（即使水位/配额全绿也不自动）。类别层之前另有元定律守卫
//! 前置查：提案全表面命中均衡判据定律类词表即拒收（decision=`rejected`，
//! cause=`meta_rule_denied`，不出站不入闸）。三条裁决闸全部纯函数可复算：
//! - ①类别判定：proposal kind→proposal_class 结构映射（`constant_update`
//!   之外的 kind 一律非低危——只收不放宽：未知结构永远人工）；
//! - ②水位预检：ruleset 水位达冻结档（92%）即拒自动（companion 容量
//!   双档 80%/92% 的硬档；warn 档只记不入闸）；
//! - ③配额检查：per-window 行权频次滑窗，超限降级人工提案入闸
//!   （fail-visible 不丢弃——提案物料包照常返回调用方，人工路径零改动）。
//!
//! # 缺省不变式
//! 显式配置激活（`EVORULE_TIERED_ADJUDICATOR_URL`+`_KEY`，缺省双空=off）；
//! off 时 verify 响应与裁决器上线前逐字节一致，不产生任何出站请求。
//!
//! # 铁律自查
//! - 铁律 3（判定只提案不改写执行行为）：自动集成=自动采纳提案进规则集
//!   生命周期（机器闸受端六检最终执法），零拦截执行行为；
//! - 铁律 2（数据必带出处）：低危档仅收带出处物料（`measured.source`
//!   非空），缺出处=降级人工；
//! - 受端拒（422 tier 报告/429 配额/5xx/传输失败）：如实记录裁决决策，
//!   降级人工提案路径——不重试、不吞错、不回滚裁决呈现。

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// 出站超时（照 companion uplink 5s 口径）
const POST_TIMEOUT: Duration = Duration::from_secs(5);

/// 滚动窗口宽度（1 小时，与 rule 受端配额同档）
pub const QUOTA_WINDOW: Duration = Duration::from_secs(3600);
/// 缺省配额：每窗自动集成次数上限（与 rule 受端缺省同档）
pub const DEFAULT_MAX_AUTO_PER_HOUR: usize = 100;

/// 裁决器出站配置（缺省关：URL/KEY 任一空白=off）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdjudicatorConfig {
    pub base_url: String,
    pub api_key: String,
}

impl AdjudicatorConfig {
    /// 双值齐备才成配置（照 CompanionUplinkConfig::build 语义）
    pub fn build(base_url: String, api_key: String) -> Option<Self> {
        if base_url.trim().is_empty() || api_key.trim().is_empty() {
            return None;
        }
        Some(Self {
            base_url: base_url.trim().trim_end_matches('/').to_string(),
            api_key: api_key.trim().to_string(),
        })
    }

    /// env 外置读取（每次调用现读：测试可先行注入；缺省双空=off）
    pub fn from_env() -> Option<Self> {
        Self::build(
            std::env::var("EVORULE_TIERED_ADJUDICATOR_URL").unwrap_or_default(),
            std::env::var("EVORULE_TIERED_ADJUDICATOR_KEY").unwrap_or_default(),
        )
    }
}

/// 自动集成出站 POST（照 companion uplink 出站模具：5s 超时、凭据头双发——
/// `X-Api-Key` 兼容 rule serve 原生鉴权，`Authorization: Bearer` 兼容统一鉴权）。
/// 返回 HTTP 终态码；Err=传输层失败（非 2xx 是 Ok 终态由调用方判级降人工）。
pub async fn post_transition_entry(
    cfg: &AdjudicatorConfig,
    dataset_id: &str,
    entry_id: &str,
    proposal_class: &str,
    cause: &str,
) -> Result<u16, String> {
    let url = format!("{}/v1/invoke/transition_entry", cfg.base_url);
    let body = json!({
        "dataset_id": dataset_id,
        "entry_id": entry_id,
        "to": "active",
        "cause": cause,
        "proposal_class": proposal_class,
    });
    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .timeout(POST_TIMEOUT)
        .header("X-Api-Key", &cfg.api_key)
        .header("Authorization", format!("Bearer {}", cfg.api_key))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("adjudicator transport error: {e}"))?;
    Ok(resp.status().as_u16())
}

// ============================================================================
// ①类别判定（结构映射，纯函数；只收不放宽）
// ============================================================================

/// proposal kind → proposal_class 结构映射：
/// - `constant_update`（常量值修正，带出处+不确定度）→ 低危
///   `constant_update`；
/// - 其余 kind（`law_review`=定律语义复核等）→ `None`=非低危，
///   一票否决走人工闸链（未知结构永远人工，只收不放宽）。
pub fn classify_proposal(kind: &str) -> Option<&'static str> {
    match kind {
        "constant_update" => Some("constant_update"),
        _ => None,
    }
}

/// 铁律 2 出处闸：低危档仅收带出处物料（measured.source 非空）。
pub fn provenance_present(proposal: &Value) -> bool {
    proposal
        .get("payload")
        .and_then(|p| p.get("measured"))
        .and_then(|m| m.get("source"))
        .and_then(|s| s.as_str())
        .is_some_and(|s| !s.trim().is_empty())
}

// ============================================================================
// ⓪元定律守卫（前置查，纯函数；只加不改）
// ============================================================================

/// 元定律守卫：均衡判据定律类不在伴跑提案管线可提案集；扩词=改代码
/// 过 PR+裁定留痕。
pub const META_RULE_INSTRUCTION_IDS: [&str; 1] = ["equilibrium_gate"];
/// 均衡判据阈值参数名（提案正文携带即视为判据修改物料）
pub const META_RULE_THRESHOLD_PARAMS: [&str; 3] = [
    "alert_free_windows_required",
    "coverage_min",
    "certified_streak_min",
];
/// 元定律面拒收 cause 值
pub const REJECT_CAUSE_META_RULE: &str = "meta_rule_denied";

/// 元定律面检测：提案全表面（kind/entry_id/subject/payload）序列化后保守
/// 子串匹配词表——命中即元定律类，拒收出可提案集。保守偏置有意为之
/// （宁可错拒不放过自我扩权；constant_update 正文提及阈值参数名即拦截，
/// 真实低危物料不携带元定律词汇）。只加不改：未命中词表的提案路径与
/// 既有三闸零差。
pub fn is_meta_rule_proposal(proposal: &Value) -> bool {
    let s = proposal.to_string();
    META_RULE_INSTRUCTION_IDS
        .iter()
        .chain(META_RULE_THRESHOLD_PARAMS.iter())
        .any(|w| s.contains(w))
}

// ============================================================================
// ②水位预检（纯函数）
// ============================================================================

/// 水位预检：ruleset 条目数达冻结档（cap 的 92%）即拒自动。
/// `rule_count=None`（companion 伴跑未装配/尚无报告）= 水位闸不在位，
/// 无冻结证据→放行（该闸仅在伴跑在位时生效；水量实据缺失不虚构拒绝）。
pub fn water_level_allows(rule_count: Option<u64>, cap: u64) -> bool {
    let Some(n) = rule_count else {
        return true;
    };
    let freeze = (cap * 92).div_ceil(100);
    n < freeze
}

// ============================================================================
// ③配额检查（per-window 滑窗，与 rule 受端同语义镜像）
// ============================================================================

/// 自动集成频次配额（进程级单例挂 AppState）：超限=裁决降人工，
/// fail-visible 不丢弃（提案物料包照常返回调用方）。
#[derive(Debug)]
pub struct AutoIntegrationQuota {
    max_per_hour: usize,
    windows: Mutex<VecDeque<Instant>>,
}

impl AutoIntegrationQuota {
    /// `max_per_hour=0`=禁用配额（显式声明激活；缺省=启用）
    pub fn new(max_per_hour: usize) -> Self {
        Self {
            max_per_hour,
            windows: Mutex::new(VecDeque::new()),
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(DEFAULT_MAX_AUTO_PER_HOUR)
    }

    /// 配额检查+占位（原子）：余量充足记一笔返回 true；超限 false。
    /// 锁毒化容忍（C5 无 unwrap 口径）：毒化锁内容仍可安全取用。
    pub fn acquire(&self) -> bool {
        if self.max_per_hour == 0 {
            return true;
        }
        let mut w = self
            .windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        while let Some(front) = w.front() {
            if now.duration_since(*front) > QUOTA_WINDOW {
                w.pop_front();
            } else {
                break;
            }
        }
        if w.len() >= self.max_per_hour {
            return false;
        }
        w.push_back(now);
        true
    }

    /// 当前窗口已用额度（观测面）
    pub fn used(&self) -> usize {
        self.windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

// ============================================================================
// 裁决决策（纯函数聚合，可回放）
// ============================================================================

/// 单笔提案的裁决决策（verify 响应 `adjudications[]` 逐条附出，可回放）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdjudicationDecision {
    pub entry_id: String,
    pub proposal_class: &'static str,
    /// `auto_integrate`=低危且三闸全绿（自动出站）；
    /// `human_path`=非低危/缺出处/水位冻结/超配额（降级人工闸链）；
    /// `rejected`=元定律面拒收（不出站不入闸）
    pub decision: &'static str,
    pub reason: String,
}

impl AdjudicationDecision {
    pub fn render(&self) -> Value {
        json!({
            "entry_id": self.entry_id,
            "proposal_class": self.proposal_class,
            "decision": self.decision,
            "reason": self.reason,
        })
    }
}

/// 对一笔提案物料包做三闸裁决（纯函数；配额以 `quota_ok` 入参注入——
/// 占位由调用方对 auto 决策原子执行，保证决策与占位一一对应）
pub fn decide(
    proposal: &Value,
    rule_count: Option<u64>,
    ruleset_cap: u64,
    quota_ok: bool,
) -> Option<AdjudicationDecision> {
    let entry_id = proposal
        .get("entry_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    // 元定律守卫前置查（类别层之前第一查）：命中即拒收出可提案集
    if is_meta_rule_proposal(proposal) {
        return Some(AdjudicationDecision {
            entry_id,
            proposal_class: "entry_create",
            decision: "rejected",
            reason: "meta_rule_denied: 元定律面（均衡判据定律类不在伴跑提案管线可提案集）——判据修改走项目管理面立法流程".to_string(),
        });
    }
    let kind = proposal.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    let Some(class) = classify_proposal(kind) else {
        return Some(AdjudicationDecision {
            entry_id,
            proposal_class: "entry_create",
            decision: "human_path",
            reason: format!("类别层一票否决：proposal kind `{kind}` 非低危（人工闸链）"),
        });
    };
    if !provenance_present(proposal) {
        return Some(AdjudicationDecision {
            entry_id,
            proposal_class: class,
            decision: "human_path",
            reason: "出处闸：measured.source 缺失（铁律 2：低危档仅收带出处物料）".to_string(),
        });
    }
    if !water_level_allows(rule_count, ruleset_cap) {
        return Some(AdjudicationDecision {
            entry_id,
            proposal_class: class,
            decision: "human_path",
            reason: "水位预检：ruleset 水位达冻结档（降级人工闸链）".to_string(),
        });
    }
    if !quota_ok {
        return Some(AdjudicationDecision {
            entry_id,
            proposal_class: class,
            decision: "human_path",
            reason: "配额检查：本窗自动集成频次超限（降级人工提案入闸，fail-visible 不丢弃）"
                .to_string(),
        });
    }
    Some(AdjudicationDecision {
        entry_id,
        proposal_class: class,
        decision: "auto_integrate",
        reason: "低危×水位未冻结×配额内：自动集成（机器闸受端六检最终执法）".to_string(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn config_build_off_when_either_blank() {
        assert!(AdjudicatorConfig::build(String::new(), "k".into()).is_none());
        assert!(AdjudicatorConfig::build("http://x".into(), "  ".into()).is_none());
        let cfg = AdjudicatorConfig::build("http://127.0.0.1:1/".into(), " k ".into()).unwrap();
        assert_eq!(cfg.base_url, "http://127.0.0.1:1");
        assert_eq!(cfg.api_key, "k");
    }

    #[test]
    fn classify_maps_only_low_risk_kind() {
        assert_eq!(
            classify_proposal("constant_update"),
            Some("constant_update")
        );
        assert_eq!(classify_proposal("law_review"), None);
        assert_eq!(classify_proposal(""), None);
        assert_eq!(classify_proposal("unknown_kind"), None);
    }

    #[test]
    fn provenance_gate_requires_non_empty_source() {
        let with_src = json!({"payload": {"measured": {"source": "lab-notebook-7"}}});
        assert!(provenance_present(&with_src));
        let blank = json!({"payload": {"measured": {"source": "  "}}});
        assert!(!provenance_present(&blank));
        let missing = json!({"payload": {"measured": {}}});
        assert!(!provenance_present(&missing));
        let no_payload = json!({"kind": "constant_update"});
        assert!(!provenance_present(&no_payload));
    }

    #[test]
    fn water_level_freeze_blocks_and_absent_gate_passes() {
        let cap = 1000u64;
        assert!(water_level_allows(Some(0), cap));
        assert!(water_level_allows(Some(919), cap));
        assert!(!water_level_allows(Some(920), cap), "冻结档（92%）拒自动");
        assert!(
            water_level_allows(None, cap),
            "伴跑不在位=水位闸无输入，不虚构冻结"
        );
    }

    #[test]
    fn quota_blocks_over_limit_and_zero_disables() {
        let q = AutoIntegrationQuota::new(2);
        assert!(q.acquire());
        assert!(q.acquire());
        assert!(!q.acquire(), "超配额拒绝");
        let off = AutoIntegrationQuota::new(0);
        for _ in 0..5 {
            assert!(off.acquire());
        }
    }

    fn constant_update_proposal(source: &str) -> Value {
        json!({
            "kind": "constant_update",
            "dataset_id": "ds-law",
            "entry_id": "c-g",
            "payload": {"measured": {"source": source, "value": 9.81}}
        })
    }

    #[test]
    fn decide_paths_full_matrix() {
        // 低危+出处+水位+配额 → auto
        let d = decide(&constant_update_proposal("lab"), Some(10), 1000, true).unwrap();
        assert_eq!(d.decision, "auto_integrate");
        assert_eq!(d.proposal_class, "constant_update");
        // 类别一票否决：law_review → human（即使其余全绿）
        let d = decide(
            &json!({"kind": "law_review", "entry_id": "l-1", "payload": {"measured": {"source": "s"}}}),
            Some(10),
            1000,
            true,
        )
        .unwrap();
        assert_eq!(d.decision, "human_path");
        assert!(d.reason.contains("一票否决"));
        // 缺出处 → human
        let d = decide(&constant_update_proposal(" "), Some(10), 1000, true).unwrap();
        assert_eq!(d.decision, "human_path");
        assert!(d.reason.contains("出处"));
        // 水位冻结 → human
        let d = decide(&constant_update_proposal("lab"), Some(950), 1000, true).unwrap();
        assert_eq!(d.decision, "human_path");
        assert!(d.reason.contains("水位"));
        // 超配额 → human（fail-visible）
        let d = decide(&constant_update_proposal("lab"), Some(10), 1000, false).unwrap();
        assert_eq!(d.decision, "human_path");
        assert!(d.reason.contains("配额"));
    }

    /// 元定律守卫前置查：均衡判据定律面物料三形态（伪装常量修正注入/
    /// 显式 meta_rule kind/正文携带指令 id）一律 rejected——其余闸全绿
    /// （出处齐、水位松、配额给 true）仍拒收出可提案集
    #[test]
    fn meta_rule_proposals_rejected() {
        // ① 伪装注入：kind=constant_update 且出处齐全，payload 携带阈值
        //    放宽物料（模拟「放宽 alert_free_windows_required 至 1」）
        let d = decide(
            &json!({
                "kind": "constant_update",
                "entry_id": "c-1",
                "payload": {
                    "measured": {"source": "lab", "value": 1.0},
                    "target_param": "alert_free_windows_required",
                    "new_value": 1,
                }
            }),
            Some(10),
            1000,
            true,
        )
        .unwrap();
        assert_eq!(d.decision, "rejected");
        assert!(
            d.reason.contains("meta_rule_denied"),
            "reason: {}",
            d.reason
        );
        // ② 显式 meta_rule kind（携带指令 id）
        let d = decide(
            &json!({
                "kind": "meta_rule",
                "entry_id": "m-2",
                "payload": {
                    "measured": {"source": "lab"},
                    "instruction_id": "equilibrium_gate",
                }
            }),
            Some(10),
            1000,
            true,
        )
        .unwrap();
        assert_eq!(d.decision, "rejected");
        assert!(
            d.reason.contains("meta_rule_denied"),
            "reason: {}",
            d.reason
        );
        // ③ 正文携带指令 id（伪装常量修正形态）
        let d = decide(
            &json!({
                "kind": "constant_update",
                "entry_id": "c-3",
                "payload": {
                    "measured": {"source": "lab", "value": 9.81},
                    "note": "re-certify equilibrium_gate before relax",
                }
            }),
            Some(10),
            1000,
            true,
        )
        .unwrap();
        assert_eq!(d.decision, "rejected");
        assert!(
            d.reason.contains("meta_rule_denied"),
            "reason: {}",
            d.reason
        );
    }

    /// 防误伤回归：未命中元定律词表的提案与既有三闸零差——既有
    /// constant_update 全绿仍 auto_integrate；payload 仅含无关词
    /// 不被词表子串误命中
    #[test]
    fn non_meta_proposals_unaffected() {
        let d = decide(&constant_update_proposal("lab"), Some(10), 1000, true).unwrap();
        assert_eq!(d.decision, "auto_integrate");
        assert_eq!(d.proposal_class, "constant_update");
        // 无关词（"coverage" 不含词表词 "coverage_min"）：不误命中
        let d = decide(
            &json!({
                "kind": "constant_update",
                "entry_id": "c-ok",
                "payload": {
                    "measured": {"source": "lab", "value": 2.5},
                    "notes": "wavelength coverage",
                }
            }),
            Some(10),
            1000,
            true,
        )
        .unwrap();
        assert_eq!(d.decision, "auto_integrate");
    }

    /// 词表常量钉死：指令 id + 三阈值参数名 + 拒收 cause 值
    /// （扩词=改代码过 PR，词表漂移由此测试当场自捕）
    #[test]
    fn meta_rule_wordlist_constants() {
        assert_eq!(META_RULE_INSTRUCTION_IDS, ["equilibrium_gate"]);
        assert_eq!(
            META_RULE_THRESHOLD_PARAMS,
            [
                "alert_free_windows_required",
                "coverage_min",
                "certified_streak_min",
            ]
        );
        assert_eq!(REJECT_CAUSE_META_RULE, "meta_rule_denied");
    }
}

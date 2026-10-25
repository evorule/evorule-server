// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! I/O 路由策略资产（SG-2）—— 静态收紧层
//!
//! 资产化三元决策表 `caller_role × io_type × target → allow/deny/ask`，
//! 装配于 [`super::PermissionGate`]（`with_route_policy`），先于动态权限表判定。
//!
//! # 定位（裁决序 v2，统帅批准）
//!
//! **放行权唯一来源 = 动态 [`super::PermissionTable`]（审批流 Active 条目 /
//! 审批过的 default policy）；本层只握收紧权（deny）与升级权（ask），无放行权。**
//!
//! 裁决序：
//! 1. 静态命中 deny → 终局 Deny（收紧闸门；动态表 Active Allow 也拒——
//!    deny-overrides 单向收紧，不属绕过审批流）；
//! 2. 静态命中 ask + caller_role=llm/unknown（无人值守）→ 回退 Deny，
//!    由 [`super::gate::PermissionGate::check_routed`] 给出 `ask-fallback` 语义留痕；
//! 3. 静态命中 ask + caller_role=human → 落动态表（保留审批通路）；
//! 4. 静态 allow / 未命中 → 落动态表 + default policy（现状不变）。
//!
//! 资产中 allow 条目仍可表达，语义 = 显式声明「此路由归审批流管辖」（可审计
//! 对照），**不产生额外放行**。
//!
//! # 资产 schema（v1）
//!
//! ```json
//! {
//!   "version": 1,
//!   "default": { "human": "allow", "llm": "allow", "unknown": "allow" },
//!   "ask_fallback": { "unattended": "deny" },
//!   "rules": [
//!     { "caller_role": "llm", "io_type": "*", "target": "*", "decision": "deny" }
//!   ]
//! }
//! ```
//!
//! - `default`：未命中 rules 时按角色的静态处置。**缺省全 allow（不收紧）**——
//!   静态层缺省 deny 会让审批过的 Active Allow 也被拒，违反审批流权威；
//!   收紧必须显式声明。
//! - `ask_fallback.unattended`：ask 无人值守回退规则（资产显式声明），
//!   唯一合法值 `deny`（allow = 静默放行 LLM 绝不允许；ask = 循环无意义）。
//!
//! # 匹配语义（确定性）
//!
//! - rules 数组顺序遍历，**先命中先用**；
//! - caller_role：精确（`human`/`llm`/`unknown`）或 `*` 通配；
//! - io_type / target：`*` 全通配、`prefix*` 前缀通配、否则精确相等；
//! - target 描述串：取 IoRequest params 的 `target` 字段（缺省空串；
//!   各 io_type 的 params 结构异构，P0 约定字段而非逐类型提取）；
//! - 解析失败整体拒绝（fail-closed），绝不静默降级——装配端对解析失败
//!   显式告警且不装配本层（动态表权威不变），禁止静默通过。

use serde::{Deserialize, Serialize};

/// 路由决策三元
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RouteDecision {
    /// 不收紧，落动态权限表（审批流权威；本层不产生放行权）
    Allow,
    /// 终局拒绝（收紧闸门）
    Deny,
    /// 升级人工裁决；无人值守（llm/unknown）按 ask_fallback 回退
    Ask,
}

/// ask 无人值守回退规则（资产显式声明段）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", default)]
pub struct AskFallback {
    /// caller_role ∈ {llm, unknown} 命中 ask 时的回退决策；唯一合法值 deny
    pub unattended: RouteDecision,
}

impl Default for AskFallback {
    /// 缺省 deny（fail-closed 回退）
    fn default() -> Self {
        Self {
            unattended: RouteDecision::Deny,
        }
    }
}

/// 逐角色缺省静态处置（未命中 rules 时）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", default)]
pub struct PerRoleDefault {
    /// human 角色缺省处置
    pub human: RouteDecision,
    /// llm 角色缺省处置
    pub llm: RouteDecision,
    /// unknown 角色缺省处置
    pub unknown: RouteDecision,
}

impl Default for PerRoleDefault {
    /// 缺省全 allow（不收紧）——见模块文档「default 段」语义
    fn default() -> Self {
        Self {
            human: RouteDecision::Allow,
            llm: RouteDecision::Allow,
            unknown: RouteDecision::Allow,
        }
    }
}

/// 单条路由规则
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteRule {
    /// 调用者角色：`human` / `llm` / `unknown` / `*`
    pub caller_role: String,
    /// I/O 类型：精确、`prefix*` 前缀通配或 `*`
    pub io_type: String,
    /// 目标描述串模式：精确、`prefix*` 前缀通配或 `*`
    pub target: String,
    /// 决策
    pub decision: RouteDecision,
}

/// 路由策略资产 —— `caller_role × io_type × target → allow/deny/ask`
///
/// 纯数据 + 纯函数判定（无时钟/随机/可变状态），同输入重放字节一致。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RoutePolicy {
    /// 资产 schema 版本（当前 1）
    #[serde(default = "default_version")]
    pub version: u32,
    /// 未命中 rules 时的逐角色缺省处置
    #[serde(default)]
    pub default: PerRoleDefault,
    /// ask 无人值守回退规则
    #[serde(default)]
    pub ask_fallback: AskFallback,
    /// 路由规则（数组顺序先命中先用）
    #[serde(default)]
    pub rules: Vec<RouteRule>,
}

fn default_version() -> u32 {
    1
}

impl RoutePolicy {
    /// 从 JSON 文本解析资产（fail-closed：任何非法值整体拒绝）
    ///
    /// 校验：
    /// - version 必须为 1；
    /// - `ask_fallback.unattended` 只允许 deny（allow = 静默放行 LLM，
    ///   ask = 升级循环无意义）；
    /// - 规则字段非空。
    pub fn from_json_str(text: &str) -> Result<Self, RoutePolicyError> {
        let policy: RoutePolicy = serde_json::from_str(text)?;
        policy.validate()?;
        Ok(policy)
    }

    /// 结构校验（反序列化后仍需满足的语义约束）
    pub fn validate(&self) -> Result<(), RoutePolicyError> {
        if self.version != 1 {
            return Err(RoutePolicyError::Invalid(format!(
                "unsupported schema version: {}",
                self.version
            )));
        }
        if self.ask_fallback.unattended != RouteDecision::Deny {
            return Err(RoutePolicyError::Invalid(format!(
                "ask_fallback.unattended only allows 'deny', got {:?}",
                self.ask_fallback.unattended
            )));
        }
        for (i, r) in self.rules.iter().enumerate() {
            if r.caller_role.is_empty() || r.io_type.is_empty() || r.target.is_empty() {
                return Err(RoutePolicyError::Invalid(format!(
                    "rule[{i}] has empty field (caller_role/io_type/target must be non-empty)"
                )));
            }
        }
        Ok(())
    }

    /// 纯函数判定：返回原始路由决策（无人值守回退逻辑在 gate 层处理）
    ///
    /// 未知名角色（非 human/llm/unknown）走 `unknown` 槽（fail-closed 对齐）。
    pub fn decide(&self, caller_role: &str, io_type: &str, target: &str) -> RouteDecision {
        for r in &self.rules {
            if role_matches(&r.caller_role, caller_role)
                && pattern_matches(&r.io_type, io_type)
                && pattern_matches(&r.target, target)
            {
                return r.decision;
            }
        }
        match caller_role {
            "human" => self.default.human,
            "llm" => self.default.llm,
            _ => self.default.unknown,
        }
    }
}

/// 角色匹配：`*` 通配或精确相等
fn role_matches(pattern: &str, role: &str) -> bool {
    pattern == "*" || pattern == role
}

/// 模式匹配：`*` 全通配、`prefix*` 前缀通配、否则精确相等
/// （与 [`super::table`] 的 resource_matches 同规则）
fn pattern_matches(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    match pattern.strip_suffix('*') {
        Some(prefix) => value.starts_with(prefix),
        None => pattern == value,
    }
}

/// 路由策略错误
#[derive(Debug, thiserror::Error)]
pub enum RoutePolicyError {
    /// JSON 反序列化失败
    #[error("route policy deserialization failed: {0}")]
    Json(#[from] serde_json::Error),
    /// 语义校验失败
    #[error("route policy validation failed: {0}")]
    Invalid(String),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// J-SG2 schema 权威样例（资产文件 rules/00_constraint_route_policy.json
    /// 由 gitignore 管辖不进 commit；本测试即 schema 权威锁）
    const SAMPLE: &str = r#"{
        "version": 1,
        "default": { "human": "allow", "llm": "allow", "unknown": "allow" },
        "ask_fallback": { "unattended": "deny" },
        "rules": [
            { "caller_role": "llm", "io_type": "http_get", "target": "api.internal:*", "decision": "deny" },
            { "caller_role": "*", "io_type": "call_external", "target": "*", "decision": "ask" }
        ]
    }"#;

    #[test]
    fn parse_sample_and_validate() {
        let p = RoutePolicy::from_json_str(SAMPLE).expect("sample must parse");
        assert_eq!(p.version, 1);
        assert_eq!(p.rules.len(), 2);
        assert_eq!(p.ask_fallback.unattended, RouteDecision::Deny);
    }

    #[test]
    fn parse_defaults_when_sections_missing() {
        // 缺段：default 全 allow（不收紧）、ask_fallback 缺省 deny、rules 空
        let p = RoutePolicy::from_json_str(r#"{"version":1}"#).expect("minimal must parse");
        assert_eq!(p.default, PerRoleDefault::default());
        assert!(p.rules.is_empty());
        assert_eq!(p.decide("llm", "http_get", ""), RouteDecision::Allow);
    }

    #[test]
    fn reject_bad_ask_fallback_and_version_and_empty_fields() {
        // unattended=allow = 静默放行 LLM，必须整体拒绝
        let bad = r#"{"version":1,"ask_fallback":{"unattended":"allow"}}"#;
        assert!(RoutePolicy::from_json_str(bad).is_err());
        let bad = r#"{"version":1,"ask_fallback":{"unattended":"ask"}}"#;
        assert!(RoutePolicy::from_json_str(bad).is_err());

        let bad = r#"{"version":2}"#;
        assert!(RoutePolicy::from_json_str(bad).is_err());

        let bad = r#"{"version":1,"rules":[{"caller_role":"","io_type":"*","target":"*","decision":"deny"}]}"#;
        assert!(RoutePolicy::from_json_str(bad).is_err());

        let bad = "not json";
        assert!(RoutePolicy::from_json_str(bad).is_err());
    }

    #[test]
    fn rules_first_match_wins_with_patterns() {
        let p = RoutePolicy::from_json_str(SAMPLE).unwrap();

        // rule[0]：llm + http_get + target 前缀 api.internal: → deny
        assert_eq!(
            p.decide("llm", "http_get", "api.internal:8080/x"),
            RouteDecision::Deny
        );
        // rule[0] target 不匹配 → 落 default.llm = allow
        assert_eq!(
            p.decide("llm", "http_get", "other.example"),
            RouteDecision::Allow
        );
        // rule[1]：任意角色 + call_external → ask（rule[0] io_type 不匹配跳过）
        assert_eq!(
            p.decide("human", "call_external", "anything"),
            RouteDecision::Ask
        );
        assert_eq!(
            p.decide("llm", "call_external", "anything"),
            RouteDecision::Ask
        );
        // 未命中任何 rule → default 按角色
        assert_eq!(p.decide("human", "query_db", "x"), RouteDecision::Allow);
        assert_eq!(p.decide("unknown", "query_db", "x"), RouteDecision::Allow);
    }

    #[test]
    fn unknown_role_uses_unknown_slot() {
        let p = RoutePolicy::from_json_str(SAMPLE).unwrap();
        // 非 human/llm 角色串 → unknown 槽（fail-closed 对齐）
        assert_eq!(p.decide("weird", "query_db", "x"), RouteDecision::Allow);
    }

    /// J-SG2 重放确定性：同输入重复判定结果一致（serde 序列化字节一致）
    #[test]
    fn decide_is_deterministic_replay() {
        let p = RoutePolicy::from_json_str(SAMPLE).unwrap();
        let cases = [
            ("llm", "http_get", "api.internal:8080/x"),
            ("llm", "http_get", "other"),
            ("human", "call_external", "anything"),
            ("unknown", "query_db", "x"),
        ];
        for &(role, io, tgt) in &cases {
            let first = p.decide(role, io, tgt);
            let first_serde = serde_json::to_string(&first).unwrap();
            for _ in 0..3 {
                let again = p.decide(role, io, tgt);
                assert_eq!(again, first, "decide 必须确定性：{role}/{io}/{tgt}");
                assert_eq!(
                    serde_json::to_string(&again).unwrap(),
                    first_serde,
                    "决策序列化字节必须一致：{role}/{io}/{tgt}"
                );
            }
        }
    }
}

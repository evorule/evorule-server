// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! I/O 权限前置判定门（B-2）
//!
//! 在每条 I/O 请求的真正分发（`IoDispatcher::dispatch`）之前，先按调用上下文对
//! 权限快照做判定，实现"入口仲裁"而非事后审计。本门是**可选装配**：由应用层
//! 显式 `IoSubscriber::with_permission_gate` 注入后才会启用，默认不做权限判定。
//!
//! # 版本语义（D8）
//! 判定时把 `ctx.v_trigger` 冻结为 `SharedFactsLog::version()`，再基于该时点的权限
//! 快照（`PermissionTable::snapshot_at`）重建一致视图。权限变更只影响更大版本的
//! 新调用，绝不影响已判定的旧调用，保证可审计回放确定性。
//!
//! # 资源/动作映射约定
//! 本门以 `resource = {resource_prefix}{io_type}`（默认 `io:http_get` 形）、
//! `action = "io"` 作为判定键，与 `PermissionTable::evaluate` 的匹配规则对齐。
//! 管理员在 `/api/permissions` 中按此约定配置条目（`Resource.path` 支持 `*` 前缀通配）。

use std::sync::Arc;

use evorule_reactor::{CallerRole, CallerRoleResolver, IoCallContext};
use evorule_tcb::JsonValue;

use super::route_policy::{RouteDecision, RoutePolicy};
use super::table::{PermissionTable, Verdict};
use crate::shared_facts_log::SharedFactsLog;

/// I/O 权限前置判定门
///
/// 持有只读的 [`SharedFactsLog`]（用于读取权限条目与定位当前版本），在每次
/// I/O 调用点冻结版本快照并产出 [`Verdict`]。`Clone` 仅复制 `Arc`，代价极低。
#[derive(Clone)]
pub struct PermissionGate {
    /// 共享事实日志（只读，用于读取权限条目与版本号）
    shared_log: Arc<SharedFactsLog>,
    /// I/O 资源前缀：判定资源串 = `{resource_prefix}{io_type}`（默认 `io:`）
    resource_prefix: Arc<str>,
    /// caller_role 解析器接缝（B-3，可选）。`None` = 沿用 `ctx.caller_role`（默认 Unknown）
    caller_role_resolver: Option<CallerRoleResolver>,
    /// 静态路由策略（SG-2，可选收紧层）。`None` = 未装配，直接落动态表（现行为）
    route_policy: Option<RoutePolicy>,
}

impl PermissionGate {
    /// 创建权限判定门，使用默认资源前缀 `io:`
    pub fn new(shared_log: Arc<SharedFactsLog>) -> Self {
        Self {
            shared_log,
            resource_prefix: Arc::from("io:"),
            caller_role_resolver: None,
            route_policy: None,
        }
    }

    /// 注入 caller_role 解析器（B-3 接缝，可选）
    ///
    /// 应用层可借此在持有会话私有 FactsLog 处完成逐 cause 链的角色解析；缺省不注入
    /// 则沿用 `ctx.caller_role`（默认 `Unknown` → 默认策略 fail-closed）。
    pub fn with_caller_role_resolver(mut self, resolver: CallerRoleResolver) -> Self {
        self.caller_role_resolver = Some(resolver);
        self
    }

    /// 注入静态路由策略（SG-2，可选收紧层）
    ///
    /// 装配后判定先经静态路由表（`caller_role × io_type × target` 三元）：
    /// - deny → 终局 Deny（收紧闸门）；
    /// - ask + llm/unknown → 无人值守回退 Deny（`route=ask-fallback`）；
    /// - ask + human / allow / 未命中 → 落动态表（审批流权威，静态层无放行权）。
    ///
    /// 未装配 = 现行为不变（直接落动态表）。装配端对资产解析失败应显式告警
    /// 且不装配本层（禁止静默通过）。
    pub fn with_route_policy(mut self, policy: RoutePolicy) -> Self {
        self.route_policy = Some(policy);
        self
    }

    /// 指定资源前缀（builder 式）
    pub fn with_resource_prefix(mut self, prefix: impl Into<Arc<str>>) -> Self {
        self.resource_prefix = prefix.into();
        self
    }

    /// 判定一次 I/O 调用
    ///
    /// # 行为
    /// - 若注入 `with_caller_role_resolver`，以其结果为权威覆盖 `ctx.caller_role`；
    ///   否则沿用 ctx 自带值（默认 `Unknown` → 默认策略 fail-closed）；
    /// - 将 `ctx.v_trigger` 冻结为 `shared_log.version()`（D8，版本域对齐）；
    /// - 权限快照重建失败按 `Deny` 处理（fail-closed，宁可拒不可放）；
    /// - 判定键：`resource = {resource_prefix}{io_type}`、`action = "io"`。
    pub fn check(
        &self,
        ctx: &mut IoCallContext,
        io_type: &str,
        payload: Option<&JsonValue>,
    ) -> Verdict {
        self.check_routed(ctx, io_type, payload).0
    }

    /// 判定一次 I/O 调用并给出静态路由语义（SG-2）
    ///
    /// 返回 `(verdict, route)`，`route` 语义：
    /// - `None`：未装配路由层 / 静态层未收紧（allow 或 ask+human）→ 落动态表；
    /// - `Some("deny")`：静态路由表命中 deny，终局拒绝（未经动态表）；
    /// - `Some("ask-fallback")`：静态命中 ask 且 caller_role 为 llm/unknown
    ///   （无人值守），按资产 `ask_fallback.unattended`（唯一合法值 deny）回退拒绝。
    ///
    /// `route` 语义由回写端（IoSubscriber）拼入错误 IoResponse 的 error 串
    /// （确定性构造），实现 J-SG2 的 deny/ask 负向实测与 default deny 可区分。
    ///
    /// # 裁决序（v2，审批流权威不变）
    /// 静态层只握收紧权（deny）与升级权（ask），无放行权——allow 不产生
    /// 额外放行，放行权唯一来源 = 动态 [`PermissionTable`]。
    pub fn check_routed(
        &self,
        ctx: &mut IoCallContext,
        io_type: &str,
        payload: Option<&JsonValue>,
    ) -> (Verdict, Option<&'static str>) {
        // B-3 接缝：注入 resolver 则以结果为权威覆盖 caller_role；否则沿用 ctx 自带值
        if let Some(resolver) = &self.caller_role_resolver {
            ctx.caller_role = resolver(ctx);
        }

        // D8：冻结触发版本（只影响本调用，不改动 ctx 其余字段所有权语义）
        ctx.v_trigger = self.shared_log.version();

        // SG-2：静态路由预门（可选装配）——deny/ask(无人值守) 在此收紧，其余落动态表
        if let Some(policy) = &self.route_policy {
            let target = payload
                .and_then(|p| p.get("target"))
                .and_then(|t| t.as_str())
                .unwrap_or("");
            let decision = policy.decide(ctx.caller_role.as_str(), io_type, target);
            match decision {
                RouteDecision::Deny => return (Verdict::Deny, Some("deny")),
                // ask 无人值守（llm/unknown）→ 回退 deny；human 保留审批通路落动态表
                RouteDecision::Ask if !matches!(ctx.caller_role, CallerRole::Human) => {
                    return (Verdict::Deny, Some("ask-fallback"));
                }
                RouteDecision::Allow | RouteDecision::Ask => {}
            }
        }

        // 快照不可重建 → fail-closed
        let Ok(table) = PermissionTable::snapshot_at(&self.shared_log, ctx.v_trigger) else {
            return (Verdict::Deny, None);
        };
        let resource = format!("{}{}", self.resource_prefix, io_type);
        (table.evaluate(ctx, &resource, "io", payload), None)
    }

    /// 当前共享日志版本号（调试 / 审计用）
    pub fn version(&self) -> u64 {
        self.shared_log.version()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Arc;

    use evorule_reactor::{CallerRole, FactId};

    use super::*;

    /// 空共享日志：无任何权限条目 → 判定走 default_policy（human=Allow, llm/unknown=Deny）
    fn empty_gate() -> PermissionGate {
        PermissionGate::new(Arc::new(SharedFactsLog::new()))
    }

    #[test]
    fn resolver_overrides_caller_role_and_changes_verdict() {
        let mut ctx = IoCallContext::new(FactId(1), 0, None);
        let gate = empty_gate().with_caller_role_resolver(Arc::new(|_| CallerRole::Human));
        // resolver 返回 Human → 空表走 default_policy.human = Deny（fail-closed：
        // 未播种人类权限条目时不做隐式放行，部署面须显式 seed）
        assert_eq!(gate.check(&mut ctx, "fake_call", None), Verdict::Deny);
    }

    #[test]
    fn human_default_deny_is_fail_closed() {
        // 缺省策略三腿全 Deny：任何角色在空表上都不可隐式放行
        use crate::permission::{DefaultPolicy, Effect};
        let dp = DefaultPolicy::default();
        assert_eq!(dp.human, Effect::Deny);
        assert_eq!(dp.llm, Effect::Deny);
        assert_eq!(dp.unknown, Effect::Deny);
    }

    #[test]
    fn explicit_human_seed_still_allows() {
        // 正向对照：显式播种 human Allow 条目后，human 判定放行（种子语义不受
        // 缺省策略收紧影响）
        use crate::permission::{
            DefaultPolicy, Effect, PermissionEntry, PermissionState, Resource, ResourceType,
            Subject,
        };
        let log = Arc::new(SharedFactsLog::new());
        let mut entry = PermissionEntry::new(
            "seed-human-fake-call",
            Subject::human("human"),
            Resource {
                resource_type: ResourceType::IoAction,
                path: "io:fake_call".to_string(),
            },
            Effect::Allow,
        );
        entry.state = PermissionState::Active;
        PermissionTable::store_entry(&log, &entry, 0).expect("store entry");

        let gate = PermissionGate::new(log)
            .with_caller_role_resolver(Arc::new(|_| CallerRole::Human));
        let mut ctx = IoCallContext::new(FactId(1), 0, None);
        assert_eq!(gate.check(&mut ctx, "fake_call", None), Verdict::Allow);
    }

    #[test]
    fn resolver_llm_is_fail_closed_on_default() {
        let mut ctx = IoCallContext::new(FactId(1), 0, None);
        let gate = empty_gate().with_caller_role_resolver(Arc::new(|_| CallerRole::Llm));
        // resolver 返回 Llm → 空表走 default_policy.llm = Deny
        assert_eq!(gate.check(&mut ctx, "fake_call", None), Verdict::Deny);
    }

    #[test]
    fn no_resolver_keeps_ctx_unknown_deny() {
        let mut ctx = IoCallContext::new(FactId(1), 0, None);
        let gate = empty_gate();
        // 未注入 resolver → caller_role 保持 Unknown → default_policy.unknown = Deny（fail-closed）
        assert_eq!(gate.check(&mut ctx, "fake_call", None), Verdict::Deny);
    }

    /// 静默通过修复实证（#2）：Deny 条目的条件求值失败必须 fail-closed 返回 Deny，
    /// 绝不静默跳过。
    ///
    /// 旧行为：`unwrap_or_default()` 把求值错误降级为 `false` → 跳过 Deny 条目 →
    /// 落到 human 默认 Allow（fail-open）。新行为：显式告警 + 立即返回 Deny。
    #[test]
    fn broken_deny_condition_is_fail_closed() {
        use crate::permission::{
            Effect, PermissionEntry, PermissionState, Resource, ResourceType, Subject,
        };

        let log = Arc::new(SharedFactsLog::new());
        let mut entry = PermissionEntry::new(
            "broken-deny",
            Subject::any(),
            Resource {
                resource_type: ResourceType::IoAction,
                path: "io:fake_call".to_string(),
            },
            Effect::Deny,
        );
        entry.state = PermissionState::Active;
        // 非法条件 domain：TCB evaluate_domain 对未知域类型显式报错（M5），而非静默求值
        entry.conditions = Some(serde_json::json!({ "type": "not_a_real_domain_op" }));
        PermissionTable::store_entry(&log, &entry, 0).expect("store entry");

        let gate = PermissionGate::new(log);
        let mut ctx = IoCallContext::new(FactId(1), 0, None);
        // human 默认 Allow：若坏条件被静默跳过 Deny 条目 → 会 fail-open 返回 Allow
        let verdict = gate.check(&mut ctx, "fake_call", None);
        assert_eq!(
            verdict,
            Verdict::Deny,
            "条件求值失败必须 fail-closed Deny，不得静默跳过 Deny 条目造成 fail-open"
        );
    }

    /// C6.1（判定正确性）+ C6.3（默认拒绝方向）穷尽决策表验证（AL2 证据，对齐 ASSURANCE.md §3.6）。
    ///
    /// 枚举有限判定域（CallerRole × io_type）的全部组合，断言：
    /// 1. 判定是**全函数**：任意组合必有确定 Verdict（Allow/Deny），不 panic、不返回 Candidate；
    /// 2. 判定**确定性**：同输入重复调用结果一致（无隐藏随机/时钟依赖）；
    /// 3. 默认拒绝方向（C6.3）：除显式 seeded Human→io:* Allow 外，Llm/Unknown 一律 Deny（fail-closed）；
    /// 4. 种子精确性：seeded Human 仅放大 Human，不泄漏到 Llm/Unknown（主体匹配精确，无 Any 通配）。
    #[test]
    fn exhaustive_decision_table_default_deny() {
        use crate::permission::{
            Effect, PermissionEntry, PermissionState, Resource, ResourceType, Subject, SubjectType,
        };

        let log = Arc::new(SharedFactsLog::new());
        // 镜像 server 启动种子：仅 Human 对 io:* 放行（与 main.rs 种子逻辑一致）
        let mut seed = PermissionEntry::new(
            "default-human-allow-io",
            Subject {
                subject_type: SubjectType::User,
                id: "human".to_string(),
            },
            Resource {
                resource_type: ResourceType::IoAction,
                path: "io:*".to_string(),
            },
            Effect::Allow,
        );
        seed.state = PermissionState::Active;
        PermissionTable::store_entry(&log, &seed, 0).expect("seed store");

        let gate = PermissionGate::new(log);

        let roles = [CallerRole::Human, CallerRole::Llm, CallerRole::Unknown];
        let io_types = [
            "http_get",
            "call_external",
            "call_service",
            "query_db",
            "save_memory",
            "fake_io",
        ];

        for &role in &roles {
            for &io in &io_types {
                let mut ctx = IoCallContext::new(FactId(1), 0, None);
                ctx.caller_role = role;
                let first = gate.check(&mut ctx, io, None);

                // 1) 全函数：必有确定 verdict，且只可能是 Allow/Deny（不是 Candidate）
                assert!(
                    matches!(first, Verdict::Allow | Verdict::Deny),
                    "判定必须全函数：{role:?}/{io} 得到 {first:?}"
                );

                // 2) 确定性：重复调用结果一致
                let mut ctx2 = IoCallContext::new(FactId(1), 0, None);
                ctx2.caller_role = role;
                let second = gate.check(&mut ctx2, io, None);
                assert_eq!(first, second, "判定必须确定性：{role:?}/{io}");

                // 3)+4) 默认拒绝方向 + 种子精确性
                match role {
                    CallerRole::Human => assert_eq!(
                        first,
                        Verdict::Allow,
                        "seeded Human 应放行 io:*，但 {io} 得到 {first:?}"
                    ),
                    CallerRole::Llm | CallerRole::Unknown => assert_eq!(
                        first,
                        Verdict::Deny,
                        "Llm/Unknown 无匹配条目必须 fail-closed Deny，但 {role:?}/{io} 得到 {first:?}"
                    ),
                }
            }
        }
    }

    // —— SG-2：静态路由预门（裁决序 v2，审批流权威不变）——
    //
    // 策略：llm+http_get+api.internal:* → deny；human+query_db:* → deny（跨层收紧
    // 验证）；*+call_external:* → ask。default 段缺省（全 allow = 不收紧）。
    const SG2_POLICY: &str = r#"{
        "version": 1,
        "rules": [
            { "caller_role": "llm", "io_type": "http_get", "target": "api.internal:*", "decision": "deny" },
            { "caller_role": "human", "io_type": "query_db", "target": "*", "decision": "deny" },
            { "caller_role": "*", "io_type": "call_external", "target": "*", "decision": "ask" }
        ]
    }"#;

    /// 构造装配路由策略的门（动态表种子 = human 对 io:* Allow，镜像 server 启动种子）
    fn sg2_gate() -> super::PermissionGate {
        use super::super::route_policy::RoutePolicy;
        use crate::permission::{
            Effect, PermissionEntry, PermissionState, Resource, ResourceType, Subject, SubjectType,
        };

        let log = Arc::new(SharedFactsLog::new());
        let mut seed = PermissionEntry::new(
            "default-human-allow-io",
            Subject {
                subject_type: SubjectType::User,
                id: "human".to_string(),
            },
            Resource {
                resource_type: ResourceType::IoAction,
                path: "io:*".to_string(),
            },
            Effect::Allow,
        );
        seed.state = PermissionState::Active;
        PermissionTable::store_entry(&log, &seed, 0).expect("seed store");
        let policy = RoutePolicy::from_json_str(SG2_POLICY).expect("policy json");
        PermissionGate::new(log).with_route_policy(policy)
    }

    /// 构造带 target 的 params（TCB JsonValue）
    fn params_with_target(target: &str) -> JsonValue {
        JsonValue::object_from_pairs(&[("target", JsonValue::string(target))])
    }

    /// 裁决序 1：静态 deny 终局拒绝——动态表 Active Allow（human io:* 种子）也收紧
    #[test]
    fn sg2_static_deny_overrides_dynamic_active_allow() {
        let gate = sg2_gate();
        let mut ctx = IoCallContext::new(FactId(1), 0, None);
        ctx.caller_role = CallerRole::Human;
        // human + query_db：动态表种子 Allow，静态 deny 收紧 → 终局 Deny
        let (verdict, route) = gate.check_routed(&mut ctx, "query_db", None);
        assert_eq!(verdict, Verdict::Deny);
        assert_eq!(route, Some("deny"), "静态 deny 必须给出 route 语义");
    }

    /// 裁决序 2：ask + llm（无人值守）→ 回退 Deny + ask-fallback 语义
    #[test]
    fn sg2_ask_llm_unattended_falls_back_to_deny() {
        let gate = sg2_gate();
        let mut ctx = IoCallContext::new(FactId(1), 0, None);
        ctx.caller_role = CallerRole::Llm;
        let (verdict, route) = gate.check_routed(&mut ctx, "call_external", None);
        assert_eq!(verdict, Verdict::Deny);
        assert_eq!(route, Some("ask-fallback"), "ask 无人值守回退必须给出 ask-fallback 语义");

        // unknown 同属无人值守
        let mut ctx = IoCallContext::new(FactId(1), 0, None);
        ctx.caller_role = CallerRole::Unknown;
        let (verdict, route) = gate.check_routed(&mut ctx, "call_external", None);
        assert_eq!((verdict, route), (Verdict::Deny, Some("ask-fallback")));
    }

    /// 裁决序 3：ask + human → 落动态表保留审批通路（种子 Allow 放行，route=None）
    #[test]
    fn sg2_ask_human_goes_to_dynamic_table() {
        let gate = sg2_gate();
        let mut ctx = IoCallContext::new(FactId(1), 0, None);
        ctx.caller_role = CallerRole::Human;
        let (verdict, route) = gate.check_routed(&mut ctx, "call_external", None);
        assert_eq!(verdict, Verdict::Allow, "ask+human 必须落动态表（种子 Allow 放行）");
        assert_eq!(route, None, "落动态表的决策不携带静态 route 语义");
    }

    /// 裁决序 4：静态 allow 不产生放行权——llm 未命中收紧规则落动态表，仍按动态表 Deny
    #[test]
    fn sg2_static_allow_grants_no_permission() {
        let gate = sg2_gate();
        let mut ctx = IoCallContext::new(FactId(1), 0, None);
        ctx.caller_role = CallerRole::Llm;
        // llm + http_get + target 不匹配 deny 规则 → 静态 allow → 落动态表 → llm 无条目 Deny
        let (verdict, route) =
            gate.check_routed(&mut ctx, "http_get", Some(&params_with_target("other.example")));
        assert_eq!(verdict, Verdict::Deny, "静态 allow 绝不产生放行权（审批流权威）");
        assert_eq!(route, None, "落动态表的 Deny 不携带静态 route 语义（区别于静态 deny）");

        // llm + http_get + target 命中 deny 规则 → 静态终局 deny
        let mut ctx = IoCallContext::new(FactId(1), 0, None);
        ctx.caller_role = CallerRole::Llm;
        let (verdict, route) = gate
            .check_routed(&mut ctx, "http_get", Some(&params_with_target("api.internal:8080/x")));
        assert_eq!((verdict, route), (Verdict::Deny, Some("deny")));
    }

    /// 未装配路由层 = 现行为不变（无 route 语义）
    #[test]
    fn sg2_no_route_policy_keeps_existing_behavior() {
        let log = Arc::new(SharedFactsLog::new());
        let gate = PermissionGate::new(log);
        let mut ctx = IoCallContext::new(FactId(1), 0, None);
        ctx.caller_role = CallerRole::Llm;
        let (verdict, route) = gate.check_routed(&mut ctx, "call_external", None);
        assert_eq!((verdict, route), (Verdict::Deny, None));
    }

    /// J-SG2 重放确定性：check_routed 同输入重复调用输出一致
    #[test]
    fn sg2_check_routed_is_deterministic() {
        let gate = sg2_gate();
        let cases: [(CallerRole, &str, Option<&str>); 5] = [
            (CallerRole::Human, "query_db", None),
            (CallerRole::Llm, "call_external", None),
            (CallerRole::Human, "call_external", None),
            (CallerRole::Llm, "http_get", Some("api.internal:8080/x")),
            (CallerRole::Unknown, "call_external", None),
        ];
        for &(role, io, target) in &cases {
            let run = || {
                let mut ctx = IoCallContext::new(FactId(1), 0, None);
                ctx.caller_role = role;
                let payload = target.map(params_with_target);
                gate.check_routed(&mut ctx, io, payload.as_ref())
            };
            let first = run();
            for _ in 0..3 {
                assert_eq!(run(), first, "check_routed 必须确定性：{role:?}/{io}/{target:?}");
            }
        }
    }
}

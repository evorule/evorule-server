// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 版本化权限快照与判定
//!
//! `PermissionTable` 将存储在 `SharedFactsLog` 下的权限条目重建为 [时点版本] 的一致视图，
//! 并按 deny-overrides 规则产出 [`Verdict`]。

use std::collections::BTreeMap;

use evorule_reactor::{CallerRole, IoCallContext};
use evorule_tcb::JsonValue;
use serde::{Deserialize, Serialize};

use super::condition::ConditionEvaluator;
use super::entry::{Effect, PermissionEntry, PermissionError, Subject};
use super::tcb_to_serde;
use crate::shared_facts_log::SharedFactsLog;

/// 权限条目存储前缀（`SharedFactsLog.facts_by_path_prefix` 查询用）
pub const ENTRY_PREFIX: &str = "shared.__permission__.entry.";
/// 默认策略存储路径（精确匹配）
pub const DEFAULT_POLICY_PATH: &str = "shared.__permission__.default_policy";
/// 删除墓碑标记：值对象中的 `__deleted` 字段为 `true` 表示该条目已被删除
const TOMBSTONE_KEY: &str = "__deleted";

/// 逐角色默认策略（无任何匹配时的兜底）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", default)]
pub struct DefaultPolicy {
    /// 人类调用者默认行为
    pub human: Effect,
    /// LLM 调用者默认行为
    pub llm: Effect,
    /// 未知调用者默认行为（fail-closed）
    pub unknown: Effect,
}

impl Default for DefaultPolicy {
    fn default() -> Self {
        Self {
            human: Effect::Allow,
            llm: Effect::Deny,
            unknown: Effect::Deny,
        }
    }
}

/// 权限判定结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// 允许执行
    Allow,
    /// 拒绝执行
    Deny,
    /// 待审批（命中候选条目，需审批人裁决后才会 Allow/Deny）
    Candidate,
}

/// 版本化权限快照 —— 只读判定表
#[derive(Debug, Clone)]
pub struct PermissionTable {
    /// 最新版（非墓碑）条目集合
    entries: Vec<PermissionEntry>,
    /// 逐角色默认策略
    default_policy: DefaultPolicy,
    /// 该快照对应的共享事实版本（[v_shared]）
    version: u64,
}

impl PermissionTable {
    /// 重建 [v_shared] 时点的权限快照
    ///
    /// # 约定
    /// - 只考虑版本号不超过 [v_shared] 的事实；
    /// - 同一条目取最高版本（后写覆盖先写）；
    /// - 墓碑（`__deleted: true`）条目被跳过。
    pub fn snapshot_at(
        log: &SharedFactsLog,
        v_shared: u64,
    ) -> Result<PermissionTable, PermissionError> {
        let mut latest: BTreeMap<String, (u64, JsonValue)> = BTreeMap::new();
        for fact in log.facts_by_path_prefix(ENTRY_PREFIX) {
            // 围栏用严格小于：账本 history 记录的是 version_before（写入前版本），
            // 而 append 返回的「本次写入后版本」= version_before + 1。
            // v_shared 时点须包含 version_before < v_shared 的全部事实；
            // 若放宽为 <=，version_before == v_shared 的事实（实际属于下一时点）
            // 会被错误纳入历史重建（off-by-one，链路 2/3 回归已实证）。
            if fact.version >= v_shared {
                continue;
            }
            let id = fact.path.trim_start_matches(ENTRY_PREFIX).to_string();
            match latest.get(&id) {
                Some((prev_v, _)) if *prev_v >= fact.version => {}
                _ => {
                    latest.insert(id, (fact.version, fact.value.clone()));
                }
            }
        }

        let mut entries = Vec::with_capacity(latest.len());
        for (id, (fact_version, value)) in latest {
            let serde_value = tcb_to_serde(&value);
            if serde_value
                .as_object()
                .and_then(|o| o.get(TOMBSTONE_KEY))
                .and_then(|v| v.as_bool())
                == Some(true)
            {
                continue;
            }
            let mut entry: PermissionEntry = serde_json::from_value(serde_value)?;
            entry.id = id;
            entry.version = fact_version;
            entries.push(entry);
        }

        let default_policy = load_default_policy(log, v_shared);
        Ok(PermissionTable {
            entries,
            default_policy,
            version: v_shared,
        })
    }

    /// 创建空快照（默认策略）
    pub fn empty() -> Self {
        Self {
            entries: Vec::new(),
            default_policy: DefaultPolicy::default(),
            version: 0,
        }
    }

    /// 快照版本号
    pub const fn version(&self) -> u64 {
        self.version
    }

    /// 只读访问全部条目
    pub fn entries(&self) -> &[PermissionEntry] {
        &self.entries
    }

    /// 按 ID 取条目
    pub fn get(&self, id: &str) -> Option<&PermissionEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// 判定一次 I/O 调用是否被允许
    ///
    /// # 字段
    /// - `ctx`: 调用上下文（caller_role 用于主体匹配与默认策略）
    /// - `resource`: 资源匹配串（如 `shared.users` / `db.users.query`）
    /// - `action`: 动作标识（不匹配时按"未命中"处理）
    /// - `payload`: 载荷，用于条件求值；`None` 时空载荷参与条件评估
    ///
    /// # 规则
    /// - deny-overrides：任一匹配的 *Active + effect=Deny* 条目 → `Deny`
    /// - 否则任一匹配且生效的 *Active + effect=Allow* → `Allow`
    /// - 命中 `Candidate` 条目（subject/resource/action 匹配）→ `Candidate`
    /// - 全部未命中 → 按调用者角色走 [`DefaultPolicy`]
    pub fn evaluate(
        &self,
        ctx: &IoCallContext,
        resource: &str,
        action: &str,
        payload: Option<&JsonValue>,
    ) -> Verdict {
        let role_key = ctx.caller_role.as_str();
        let empty_payload = JsonValue::Null;
        let payload = payload.unwrap_or(&empty_payload);

        let mut candidate = false;
        let mut allowed = false;

        for e in &self.entries {
            if e.state.is_candidate() {
                if subject_matches(&e.subject, role_key)
                    && action_matches(&e.action, action)
                    && resource_matches(&e.resource, resource)
                {
                    candidate = true;
                }
                continue;
            }
            if !e.state.is_active() {
                continue;
            }
            if !subject_matches(&e.subject, role_key)
                || !action_matches(&e.action, action)
                || !resource_matches(&e.resource, resource)
            {
                continue;
            }
            let cond_ok = match &e.conditions {
                None => true,
                Some(c) => {
                    let evaluator = ConditionEvaluator;
                    match evaluator.evaluate(c, ctx, payload) {
                        Ok(v) => v,
                        Err(err) => {
                            // 透明性（P-透明）：条件求值失败必须显式告警，绝不静默降级为
                            // "条件不满足"——否则坏条件会悄悄跳过 Deny 条目造成 fail-open。
                            // fail-closed：宁可拒绝，也不放行。
                            tracing::warn!(
                                "权限条件求值失败，按 Deny fail-closed 处理（entry={}, 条件={}）: {}",
                                e.id, c, err
                            );
                            return Verdict::Deny;
                        }
                    }
                }
            };
            if !cond_ok {
                continue;
            }
            match e.effect {
                Effect::Deny => return Verdict::Deny,
                Effect::Allow => allowed = true,
            }
        }

        if allowed {
            return Verdict::Allow;
        }
        if candidate {
            return Verdict::Candidate;
        }
        let effect = match ctx.caller_role {
            CallerRole::Human => self.default_policy.human,
            CallerRole::Llm => self.default_policy.llm,
            CallerRole::Unknown => self.default_policy.unknown,
        };
        match effect {
            Effect::Allow => Verdict::Allow,
            Effect::Deny => Verdict::Deny,
        }
    }

    /// 写入/更新一条目（追加新版本，历史保留）
    ///
    /// 返回本次写入对应的共享事实版本号。
    pub fn store_entry(
        log: &SharedFactsLog,
        entry: &PermissionEntry,
        session_id: u64,
    ) -> Result<u64, PermissionError> {
        let value = super::serde_to_tcb(&serde_json::to_value(entry)?);
        log.append(&path_for(ENTRY_PREFIX, &entry.id), value, session_id)
            .map_err(|e| PermissionError::Store(e.to_string()))
    }

    /// 删除一条目（写入墓碑，历史保留）
    pub fn remove(log: &SharedFactsLog, id: &str, session_id: u64) -> Result<(), PermissionError> {
        let mut tomb = serde_json::Map::new();
        tomb.insert(TOMBSTONE_KEY.to_string(), serde_json::Value::Bool(true));
        let value = super::serde_to_tcb(&serde_json::Value::Object(tomb));
        log.append(&path_for(ENTRY_PREFIX, id), value, session_id)
            .map_err(|e| PermissionError::Store(e.to_string()))?;
        Ok(())
    }

    /// 覆盖默认策略
    pub fn set_default_policy(
        log: &SharedFactsLog,
        policy: &DefaultPolicy,
        session_id: u64,
    ) -> Result<u64, PermissionError> {
        let value = super::serde_to_tcb(&serde_json::to_value(policy)?);
        log.append(DEFAULT_POLICY_PATH, value, session_id)
            .map_err(|e| PermissionError::Store(e.to_string()))
    }
}

/// 拼接条目存储完整路径
fn path_for(prefix: &str, id: &str) -> String {
    format!("{prefix}{id}")
}

/// 从共享事实读取最新默认策略（解析失败则回退默认）
fn load_default_policy(log: &SharedFactsLog, v_shared: u64) -> DefaultPolicy {
    let latest = log
        .facts_by_path_prefix(DEFAULT_POLICY_PATH)
        .into_iter()
        .filter(|f| f.version <= v_shared && f.path == DEFAULT_POLICY_PATH)
        .max_by_key(|f| f.version);
    match latest {
        Some(f) => match serde_json::from_value::<DefaultPolicy>(tcb_to_serde(&f.value)) {
            Ok(p) => p,
            Err(e) => {
                // 透明性：默认策略反序列化失败必须显式告警，绝不静默回退默认策略
                // （否则策略文件损坏时人类调用者会被静默 fail-open 放行）。
                tracing::warn!(
                    "默认策略反序列化失败（路径 {}），回退 DefaultPolicy::default(): {e}",
                    DEFAULT_POLICY_PATH
                );
                DefaultPolicy::default()
            }
        },
        None => DefaultPolicy::default(),
    }
}

/// 主体匹配：`Any` 恒真；其余按 id 与调用者角色串（`human`/`llm`/`unknown`）比较
fn subject_matches(subject: &Subject, role_key: &str) -> bool {
    subject.subject_type == super::entry::SubjectType::Any || subject.id == role_key
}

/// 动作匹配：`*` 通配；否则精确相等
fn action_matches(rule_action: &str, action: &str) -> bool {
    rule_action == "*" || rule_action == action
}

/// 资源匹配：`*` 结尾为前缀通配；否则精确相等
fn resource_matches(resource: &super::entry::Resource, target: &str) -> bool {
    let pattern = resource.path.as_str();
    if pattern.is_empty() {
        return false;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        target.starts_with(prefix)
    } else {
        target == pattern
    }
}

/// C6.5 链路测试（AL1 实现级证据，对齐 ASSURANCE.md §3.6 条款 4 + 失效条件 4）。
///
/// **条款 4（篡改可检测）**：守卫判定所依据的权限表，其变更本身经过账本（因而受 C4 保护）。
/// **失效条件 4**：权限表存在不经账本的修改路径。
///
/// 本模块通过机制层实证证明以下链路闭环：
/// 1. 每条权限变更（`store_entry` / `remove` / `set_default_policy`）都经由 `SharedFactsLog::append`
///    落到账本，且可从账本原始事实回读 —— 写路径唯一、账本是唯一真相源；
/// 2. `PermissionTable` 是账本的**时点投影**（只读快照），不存在 `&mut self` 旁路写入 API；
/// 3. 删除是写墓碑而非抹除，账本 append-only，历史在旧版本快照中仍可见 —— 决策可追溯（§3.6 条款 3）；
/// 4. 快照严格按 `v_shared` 版本围栏，证明权限表是账本某时间点的精确投影（支撑 C5 重放忠实）；
/// 5. 账本落定的权限变更确实改变守卫判定结果（`evaluate` 输出随之变化）—— 闭环到「决策」；
/// 6. 连默认策略（`human`/`llm`/`unknown`）这类 fail-open/fail-closed 姿态也走账本，无旁路。
///
/// 注：本测试仅覆盖**机制层**（C6 范围）。应用层 `/api/permissions` 是否调用这些机制层入口，
/// 属 C6 §3.6 条款 5（装配可观测）与 H6.2 的判定，不在本链路测试范围内。
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Arc;

    use evorule_reactor::{CallerRole, FactId, IoCallContext};

    use super::{PermissionTable, ENTRY_PREFIX};
    use crate::permission::{
        Effect, PermissionEntry, PermissionState, Resource, ResourceType, Subject,
    };
    use crate::shared_facts_log::SharedFactsLog;

    /// 构造一条 `Active + Allow` 的具名主体权限条目（subject = human「human」，资源 `io:*`）
    fn active_human_allow(id: &str) -> PermissionEntry {
        let mut e = PermissionEntry::new(
            id,
            Subject::human("human"),
            Resource {
                resource_type: ResourceType::IoAction,
                path: "io:*".to_string(),
            },
            Effect::Allow,
        );
        e.state = PermissionState::Active;
        e
    }

    // —— 链路 1：权限写入经账本 append，且可从账本原始事实与派生快照双向回读 ——
    #[test]
    fn permission_write_goes_through_ledger_and_reflects_in_snapshot() {
        let log = Arc::new(SharedFactsLog::new());

        let entry = active_human_allow("io-allow-human");
        let stored_version = PermissionTable::store_entry(&log, &entry, 0).expect("store_entry");

        // (a) 变更已落到账本原始事实层：路径前缀可查、且内容完整
        let raw: Vec<_> = log.facts_by_path_prefix(ENTRY_PREFIX);
        assert_eq!(raw.len(), 1, "store_entry 必须向账本追加恰好一条事实");
        assert_eq!(
            raw[0].path,
            format!("{ENTRY_PREFIX}io-allow-human"),
            "账本事实路径必须与 ENTRY_PREFIX + id 精确对应"
        );

        // (b) 派生快照与账本一致：权限表是账本的投影，而非独立内存副本
        let table = PermissionTable::snapshot_at(&log, stored_version).expect("snapshot_at");
        let back = table.get("io-allow-human").expect("条目必须出现在快照中");
        assert_eq!(back.effect, Effect::Allow);
        assert_eq!(back.state, PermissionState::Active);

        // (c) 账本落定的变更确实改变守卫判定结果 —— 闭环到「决策」
        let mut ctx = IoCallContext::new(FactId(0), stored_version, None);
        ctx.caller_role = CallerRole::Human;
        assert_eq!(
            table.evaluate(&ctx, "io:call_external", "*", None),
            crate::permission::Verdict::Allow,
            "账本中 Active Allow 条目必须使匹配的人类调用者被放行"
        );
    }

    // —— 链路 2：删除写墓碑、账本 append-only，历史在旧版本快照中仍可见（决策可追溯）——
    #[test]
    fn remove_is_tombstone_and_ledger_history_is_append_only() {
        let log = Arc::new(SharedFactsLog::new());

        let v1 = PermissionTable::store_entry(&log, &active_human_allow("p-del"), 0).unwrap();
        assert!(
            PermissionTable::snapshot_at(&log, v1)
                .unwrap()
                .get("p-del")
                .is_some(),
            "v1 快照必须含刚写入的条目"
        );

        PermissionTable::remove(&log, "p-del", 0).expect("remove should succeed");
        let v2 = log.version();

        // 删除后当前快照不再含该条目（墓碑跳过）
        assert!(
            PermissionTable::snapshot_at(&log, v2)
                .unwrap()
                .get("p-del")
                .is_none(),
            "v2 快照必须因墓碑排除已删除条目"
        );

        // 但旧版本 v1 快照仍可见 —— 账本是 append-only，历史不可抹除（C4 保护前提 + 决策可追溯）
        assert!(
            PermissionTable::snapshot_at(&log, v1)
                .unwrap()
                .get("p-del")
                .is_some(),
            "旧版本快照必须仍含已删除条目：账本 append-only，删除仅追加墓碑"
        );

        // 账本原始事实层保留两条（写入 + 墓碑），而非一条被改写 —— 证明无就地改写旁路
        let raw = log.facts_by_path_prefix(ENTRY_PREFIX);
        assert_eq!(
            raw.len(),
            2,
            "账本必须保留「写入」与「墓碑」两条事实，而非就地改写单条"
        );
    }

    // —— 链路 3：快照严格按 v_shared 版本围栏（权限表是账本的时点投影）——
    #[test]
    fn snapshot_is_point_in_time_projection_fenced_by_version() {
        let log = Arc::new(SharedFactsLog::new());

        let v1 = PermissionTable::store_entry(&log, &active_human_allow("p-a"), 0).unwrap();
        let v2 = PermissionTable::store_entry(&log, &active_human_allow("p-b"), 0).unwrap();

        // v1 时点：仅含 p-a（p-b 的版本 > v1，必须被排除）
        let snap1 = PermissionTable::snapshot_at(&log, v1).unwrap();
        assert!(snap1.get("p-a").is_some(), "v1 必须含 p-a");
        assert!(snap1.get("p-b").is_none(), "v1 必须排除版本 > v1 的 p-b");

        // v2 时点：p-a 与 p-b 均在
        let snap2 = PermissionTable::snapshot_at(&log, v2).unwrap();
        assert!(snap2.get("p-a").is_some(), "v2 必须含 p-a");
        assert!(snap2.get("p-b").is_some(), "v2 必须含 p-b");
    }

    // —— 链路 4：快照是不可变拷贝（非可变视图），账本是唯一权威，无 in-place 旁路 ——
    #[test]
    fn snapshot_is_immutable_copy_not_mutable_view() {
        let log = Arc::new(SharedFactsLog::new());

        let v1 = PermissionTable::store_entry(&log, &active_human_allow("p-imm"), 0).unwrap();
        let snap1 = PermissionTable::snapshot_at(&log, v1).unwrap();
        assert!(snap1.get("p-imm").is_some());

        // 后续向账本追加新条目 —— 旧快照不得被突变（证明快照是拷贝而非可变视图，无 in-place 旁路）
        let _v2 = PermissionTable::store_entry(&log, &active_human_allow("p-imm2"), 0).unwrap();
        assert!(
            snap1.get("p-imm2").is_none(),
            "旧快照不应被后续账本变更突变：快照是不可变拷贝，不存在绕过账本的就地写入"
        );

        // 新快照从账本取最新 —— 账本是唯一权威
        let log_version = log.version();
        let snap2 = PermissionTable::snapshot_at(&log, log_version).unwrap();
        assert!(snap2.get("p-imm").is_some() && snap2.get("p-imm2").is_some());
    }

    // —— 链路 5：默认策略（fail-open/fail-closed 姿态）同样经账本，无旁路 ——
    #[test]
    fn default_policy_change_is_ledger_backed() {
        let log = Arc::new(SharedFactsLog::new());

        // 空表 + 默认策略（human=Allow）下，人类调用者默认放行
        let v0 = log.version();
        let table0 = PermissionTable::snapshot_at(&log, v0).unwrap();
        let mut ctx = IoCallContext::new(FactId(0), v0, None);
        ctx.caller_role = CallerRole::Human;
        assert_eq!(
            table0.evaluate(&ctx, "io:anything", "*", None),
            crate::permission::Verdict::Allow,
            "默认策略 human=Allow 应使空表下人类调用者默认放行"
        );

        // 经账本改写默认策略：human → Deny（fail-closed 姿态），证明该姿态变更也走账本
        let deny_human = crate::permission::DefaultPolicy {
            human: Effect::Deny,
            llm: Effect::Deny,
            unknown: Effect::Deny,
        };
        let v1 = PermissionTable::set_default_policy(&log, &deny_human, 0).unwrap();
        let table1 = PermissionTable::snapshot_at(&log, v1).unwrap();
        let mut ctx1 = IoCallContext::new(FactId(0), v1, None);
        ctx1.caller_role = CallerRole::Human;
        assert_eq!(
            table1.evaluate(&ctx1, "io:anything", "*", None),
            crate::permission::Verdict::Deny,
            "经账本改写后的默认策略必须生效：人类调用者转为 Deny（fail-closed）"
        );
    }
}

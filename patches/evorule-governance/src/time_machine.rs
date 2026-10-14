// SPDX-License-Identifier: AGPL-3.0-or-later
//! 时间机器模块 —— 提供 rewind/diff 核心能力
//!
//! # 设计原则
//! - 基于 FactsLog 的完整历史，支持回溯到任意版本
//! - 计算两个版本间的 payload 差异
//! - 纯函数实现，无副作用，可测试

use evorule_reactor::Fact;
use evorule_tcb::JsonValue;
use serde::{Deserialize, Serialize};

/// 将 JsonValue 转换为 serde_json::Value
fn json_value_to_serde(v: &JsonValue) -> serde_json::Value {
    match v {
        JsonValue::Null => serde_json::Value::Null,
        JsonValue::Bool(b) => serde_json::Value::Bool(*b),
        JsonValue::Integer(i) => serde_json::Value::Number((*i).into()),
        JsonValue::String(s) => serde_json::Value::String(s.to_string()),
        JsonValue::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(json_value_to_serde).collect())
        }
        JsonValue::Object(map) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in map.iter() {
                obj.insert(k.clone(), json_value_to_serde(v));
            }
            serde_json::Value::Object(obj)
        }
    }
}

/// rewind 结果：指定版本的物化快照
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewindSnapshot {
    /// 该版本的 payload 快照
    pub payload: serde_json::Value,
    /// 该版本的指令队列
    pub queue: Vec<serde_json::Value>,
    /// 版本号
    pub version: u64,
}

/// 两个版本间的 payload diff
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PayloadDiff {
    /// v_b 新增的字段（v_a 没有，v_b 有）
    pub added: Vec<(String, serde_json::Value)>,
    /// v_b 删除的字段（v_a 有，v_b 没有）
    pub removed: Vec<(String, serde_json::Value)>,
    /// 值变化的字段 (key, v_a 的值, v_b 的值)
    pub changed: Vec<(String, serde_json::Value, serde_json::Value)>,
    /// 值未变化的字段名
    pub unchanged: Vec<String>,
}

impl PayloadDiff {
    /// diff 是否为空（两个 payload 完全相同）
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }

    /// 变更字段总数（added + removed + changed）
    pub fn change_count(&self) -> usize {
        self.added.len() + self.removed.len() + self.changed.len()
    }

    /// 生成可读的 diff 摘要
    pub fn summary(&self) -> String {
        format!(
            "diff: +{} -{} ~{} (={} unchanged)",
            self.added.len(),
            self.removed.len(),
            self.changed.len(),
            self.unchanged.len()
        )
    }
}

/// 时间机器错误（ / B8b）
///
/// `diff` 在 rewind 不可达时不再静默回退空 payload（旧行为会产生无告警的
/// 错误 diff 结果），而是显式返回本错误。
#[derive(Debug, thiserror::Error)]
#[error("time machine: rewind to version {version} unavailable (facts history insufficient or version out of range)")]
pub struct TimeMachineError {
    /// 不可达的版本号
    pub version: u64,
}

/// 应用 PayloadUpdate 的路径更新（与 reactor::update_payload 逻辑一致）
///
/// 断点 9 修复：rewind/rewind_payload 需要正确应用 PayloadUpdate 对 payload
/// 的修改，否则版本计数和状态重建都会错误。
///
/// # 算法（与 reactor.rs:832-890 保持一致）
///
/// 1. 先尝试 `resolve_path_mut`（路径已存在 → 直接更新）
/// 2. 失败则尝试嵌套路径创建（`split('.')` 导航，中间节点自动创建）
/// 3. 都失败则静默跳过（rewind 是 best-effort 重建，不返回 Err）
///
/// # 为什么不复用 reactor::update_payload
///
/// - reactor::update_payload 是 Reactor 的私有方法，返回 `Result<(), ReactorError>`
/// - rewind 不能失败（返回 Option），需要 best-effort 语义
/// - 提取为 pub(crate) 辅助函数避免跨 crate 暴露内部实现
pub(crate) fn apply_payload_update(payload: &mut JsonValue, path: &str, value: JsonValue) {
    // 1. 路径已存在：直接更新
    if let Some(target) = evorule_tcb::path::resolve_path_mut(payload, path) {
        *target = value;
        return;
    }

    // 2. 路径不存在：嵌套创建（与 reactor.rs:832-890 一致）
    let parts: Vec<&str> = path.split('.').collect();
    if parts.is_empty() {
        return;
    }

    let field = match parts.last() {
        Some(f) => *f,
        None => return,
    };

    let parent_obj = if parts.len() == 1 {
        // 单层路径：在 payload 顶层创建字段
        if let JsonValue::Object(map) = payload {
            map
        } else {
            return;
        }
    } else {
        // 多层路径：导航到父对象，中间节点不存在则自动创建
        let mut current = payload;
        for &part in &parts[0..parts.len() - 1] {
            if let JsonValue::Object(map) = current {
                if !map.contains_key(part) {
                    map.insert(part.to_string(), JsonValue::empty_object());
                }
                current = match map.get_mut(part) {
                    Some(v) => v,
                    None => return,
                };
            } else {
                return;
            }
        }
        if let JsonValue::Object(map) = current {
            map
        } else {
            return;
        }
    };

    parent_obj.insert(field.to_string(), value);
}

/// 从 facts 列表回溯到指定 version，返回当时的快照
///
/// # 语义说明
///
/// - rewind 返回指定 version 的状态快照（payload + queue + version）
/// - 包含 target_version 及之前的所有状态变更：
///   - `StateTransition`：更新 payload 和 queue，version += 1
///   - `IoResponse`：version += 1（状态变更是瞬时的，由后续 StateTransition 捕获）
///   - `PayloadUpdate`：应用路径更新到 payload，version += 1
/// - 其他 Fact（Command/Stable/Error）：不改变状态，不递增 version
///
/// # 断点 9 修复
///
/// 原实现忽略 PayloadUpdate（`_ => {}`），导致 version 计数不一致和状态重建
/// 错误。修复后正确应用 PayloadUpdate 的路径更新。
///
/// # 算法
///
/// 遍历 facts，对每个 Fact 应用状态转换规则：
/// - `StateTransition`：更新 payload 和 queue，version += 1
/// - `IoResponse`：version += 1（不应用状态变更，由后续 StateTransition 捕获）
/// - `PayloadUpdate`：应用路径更新到 payload，version += 1
/// - 其他 Fact：忽略
///
/// 当 version == target_version 时停止，返回当前快照。
pub fn rewind(facts: &[Fact], target_version: u64) -> Option<RewindSnapshot> {
    if target_version == 0 {
        return Some(RewindSnapshot {
            payload: serde_json::Value::Object(serde_json::Map::new()),
            queue: Vec::new(),
            version: 0,
        });
    }

    // 断点 9 修复：内部使用 JsonValue 以支持 resolve_path_mut
    let mut payload = JsonValue::empty_object();
    let mut queue: Vec<JsonValue> = Vec::new();
    let mut version: u64 = 0;

    for fact in facts {
        match fact {
            Fact::StateTransition {
                new_payload,
                new_queue,
                ..
            } => {
                payload = new_payload.clone();
                queue = new_queue.clone();
                version += 1;
            }
            Fact::IoResponse { .. } => {
                // IoResponse 的状态变更是瞬时的（inject __io_result__ + push_front
                // 原指令），后续 StateTransition 会捕获最终状态。
                // rewind 到 IoResponse 版本时返回最近 StateTransition 的状态。
                version += 1;
            }
            // 断点 9 修复：PayloadUpdate 必须应用路径更新并递增 version
            Fact::PayloadUpdate { path, value, .. } => {
                apply_payload_update(&mut payload, path, value.clone());
                version += 1;
            }
            _ => {}
        }
        if version == target_version {
            break;
        }
    }

    if version < target_version {
        return None;
    }

    Some(RewindSnapshot {
        payload: json_value_to_serde(&payload),
        queue: queue.iter().map(json_value_to_serde).collect(),
        version,
    })
}

/// 计算两个版本间的 payload diff
///
/// # 参数
///
/// - `facts`：完整的 Fact 历史
/// - `v_a`：起始版本
/// - `v_b`：目标版本
///
/// # 返回
///
/// 返回 `Ok(PayloadDiff)`，包含 added/removed/changed/unchanged 四类字段。
///
/// # 错误（ / B8b）
///
/// 任一版本 rewind 不可达（事实历史不足/版本越界）时返回
/// [`TimeMachineError`]——**不再**静默回退空 payload（旧行为会产生无告警的
/// 错误 diff 结果，直接动摇回放工具的可信性）。
pub fn diff(facts: &[Fact], v_a: u64, v_b: u64) -> Result<PayloadDiff, TimeMachineError> {
    let payload_a = rewind(facts, v_a)
        .map(|s| s.payload)
        .ok_or(TimeMachineError { version: v_a })?;
    let payload_b = rewind(facts, v_b)
        .map(|s| s.payload)
        .ok_or(TimeMachineError { version: v_b })?;

    Ok(compute_diff(&payload_a, &payload_b))
}

/// 计算两个 JSON 对象的 diff
fn compute_diff(payload_a: &serde_json::Value, payload_b: &serde_json::Value) -> PayloadDiff {
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    let mut unchanged = Vec::new();

    if let Some(map_a) = payload_a.as_object() {
        for (key, val_a) in map_a {
            match payload_b.as_object().and_then(|m| m.get(key)) {
                Some(val_b) => {
                    if val_a == val_b {
                        unchanged.push(key.clone());
                    } else {
                        changed.push((key.clone(), val_a.clone(), val_b.clone()));
                    }
                }
                None => {
                    removed.push((key.clone(), val_a.clone()));
                }
            }
        }
    }

    if let Some(map_b) = payload_b.as_object() {
        for (key, val_b) in map_b {
            if payload_a.as_object().and_then(|m| m.get(key)).is_none() {
                added.push((key.clone(), val_b.clone()));
            }
        }
    }

    PayloadDiff {
        added,
        removed,
        changed,
        unchanged,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use evorule_reactor::FactId;
    use evorule_tcb::JsonValue;

    /// 将 serde_json::Value 转换为 JsonValue
    fn serde_to_json(v: serde_json::Value) -> JsonValue {
        match v {
            serde_json::Value::Null => JsonValue::Null,
            serde_json::Value::Bool(b) => JsonValue::Bool(b),
            serde_json::Value::Number(n) => {
                // JsonValue 不支持 Float，尝试转换为 Integer
                if let Some(i) = n.as_i64() {
                    JsonValue::Integer(i)
                } else {
                    // 浮点数或超出范围，转为 Null
                    JsonValue::Null
                }
            }
            serde_json::Value::String(s) => JsonValue::string(s),
            serde_json::Value::Array(arr) => {
                JsonValue::Array(arr.into_iter().map(serde_to_json).collect())
            }
            serde_json::Value::Object(map) => {
                let mut obj = std::collections::BTreeMap::new();
                for (k, v) in map {
                    obj.insert(k, serde_to_json(v));
                }
                JsonValue::Object(obj)
            }
        }
    }

    fn make_state_transition(version: u64, payload: serde_json::Value) -> Fact {
        Fact::StateTransition {
            id: FactId(version),
            cause: FactId(version.saturating_sub(1)),
            new_payload: serde_to_json(payload),
            new_queue: vec![],
        }
    }

    fn make_io_response(version: u64) -> Fact {
        Fact::IoResponse {
            id: FactId(version),
            request_id: FactId(version.saturating_sub(1)),
            result: JsonValue::Null,
            error: None,
        }
    }

    #[test]
    fn test_rewind_zero_returns_empty() {
        let facts: Vec<Fact> = vec![];
        let result = rewind(&facts, 0);
        assert!(result.is_some());
        let snap = result.unwrap();
        assert_eq!(snap.version, 0);
        assert!(snap.payload.as_object().unwrap().is_empty());
        assert!(snap.queue.is_empty());
    }

    #[test]
    fn test_rewind_basic_state_transition() {
        let facts = vec![make_state_transition(1, serde_json::json!({"amount": 100}))];

        let result = rewind(&facts, 1);
        assert!(result.is_some());
        let snap = result.unwrap();
        assert_eq!(snap.version, 1);
        assert_eq!(snap.payload, serde_json::json!({"amount": 100}));
    }

    #[test]
    fn test_rewind_multiple_state_transitions() {
        let facts = vec![
            make_state_transition(1, serde_json::json!({"step": 1})),
            make_state_transition(2, serde_json::json!({"step": 2})),
            make_state_transition(3, serde_json::json!({"step": 3})),
        ];

        let result = rewind(&facts, 2);
        assert!(result.is_some());
        let snap = result.unwrap();
        assert_eq!(snap.version, 2);
        assert_eq!(snap.payload, serde_json::json!({"step": 2}));
    }

    #[test]
    fn test_rewind_io_response_increments_version() {
        let facts = vec![
            make_state_transition(1, serde_json::json!({"amount": 50})),
            make_io_response(2),
        ];

        let result = rewind(&facts, 2);
        assert!(result.is_some());
        let snap = result.unwrap();
        assert_eq!(snap.version, 2);
        assert_eq!(snap.payload, serde_json::json!({"amount": 50}));
    }

    #[test]
    fn test_rewind_out_of_range_returns_none() {
        let facts = vec![make_state_transition(1, serde_json::json!({"amount": 100}))];

        let result = rewind(&facts, 5);
        assert!(result.is_none());
    }

    #[test]
    fn test_diff_added_field() {
        let facts = vec![
            make_state_transition(1, serde_json::json!({"a": 1})),
            make_state_transition(2, serde_json::json!({"a": 1, "b": 2})),
        ];

        let diff_result = diff(&facts, 1, 2).unwrap();
        assert_eq!(diff_result.added.len(), 1);
        assert_eq!(diff_result.added[0].0, "b");
        assert_eq!(diff_result.removed.len(), 0);
        assert_eq!(diff_result.changed.len(), 0);
        assert_eq!(diff_result.unchanged.len(), 1);
        assert_eq!(diff_result.unchanged[0], "a");
    }

    #[test]
    fn test_diff_removed_field() {
        let facts = vec![
            make_state_transition(1, serde_json::json!({"a": 1, "b": 2})),
            make_state_transition(2, serde_json::json!({"a": 1})),
        ];

        let diff_result = diff(&facts, 1, 2).unwrap();
        assert_eq!(diff_result.removed.len(), 1);
        assert_eq!(diff_result.removed[0].0, "b");
        assert_eq!(diff_result.added.len(), 0);
        assert_eq!(diff_result.changed.len(), 0);
    }

    #[test]
    fn test_diff_changed_field() {
        let facts = vec![
            make_state_transition(1, serde_json::json!({"a": 1})),
            make_state_transition(2, serde_json::json!({"a": 99})),
        ];

        let diff_result = diff(&facts, 1, 2).unwrap();
        assert_eq!(diff_result.changed.len(), 1);
        assert_eq!(diff_result.changed[0].0, "a");
        assert_eq!(diff_result.changed[0].1, serde_json::json!(1));
        assert_eq!(diff_result.changed[0].2, serde_json::json!(99));
        assert_eq!(diff_result.added.len(), 0);
        assert_eq!(diff_result.removed.len(), 0);
    }

    #[test]
    fn test_diff_identical_payloads() {
        let facts = vec![
            make_state_transition(1, serde_json::json!({"a": 1, "b": 2})),
            make_state_transition(2, serde_json::json!({"a": 1, "b": 2})),
        ];

        let diff_result = diff(&facts, 1, 2).unwrap();
        assert!(diff_result.is_empty());
        assert_eq!(diff_result.unchanged.len(), 2);
    }

    #[test]
    fn test_payload_diff_summary() {
        let diff_result = PayloadDiff {
            added: vec![("x".to_string(), serde_json::json!(1))],
            removed: vec![("y".to_string(), serde_json::json!(2))],
            changed: vec![("z".to_string(), serde_json::json!(3), serde_json::json!(4))],
            unchanged: vec!["w".to_string()],
        };
        let summary = diff_result.summary();
        assert!(summary.contains("+1"));
        assert!(summary.contains("-1"));
        assert!(summary.contains("~1"));
        assert!(summary.contains("=1"));
    }

    /// B8b 回归（）：rewind 不可达时 diff 必须显式报错，
    /// 绝不静默回退空 payload（旧 unwrap_or_else 行为会产生无告警的错误 diff）。
    #[test]
    fn test_diff_unreachable_version_is_error_not_empty() {
        let facts = vec![make_state_transition(1, serde_json::json!({"a": 1}))];

        let err = diff(&facts, 1, 99).unwrap_err();
        assert_eq!(
            err.version, 99,
            "越界版本必须返回 TimeMachineError 并携带版本号"
        );

        let err = diff(&facts, 50, 1).unwrap_err();
        assert_eq!(err.version, 50);
    }

    // ========================================================================
    // 断点 9 修复测试：PayloadUpdate 在 rewind/diff 中的正确处理
    // ========================================================================

    fn make_payload_update(version: u64, path: &str, value: serde_json::Value) -> Fact {
        Fact::PayloadUpdate {
            id: FactId(version),
            path: path.to_string(),
            value: serde_to_json(value),
        }
    }

    /// 验证 rewind 正确应用 PayloadUpdate 的路径更新
    #[test]
    fn test_rewind_payload_update_applied() {
        let facts = vec![
            make_state_transition(1, serde_json::json!({"amount": 100})),
            make_payload_update(2, "amount", serde_json::json!(200)),
        ];

        let result = rewind(&facts, 2);
        assert!(result.is_some());
        let snap = result.unwrap();
        assert_eq!(snap.version, 2);
        assert_eq!(snap.payload, serde_json::json!({"amount": 200}));
    }

    /// 验证 PayloadUpdate 正确递增 version，且不干扰其他版本的 rewind
    #[test]
    fn test_rewind_payload_update_version_increment() {
        let facts = vec![
            make_state_transition(1, serde_json::json!({"x": 1})),
            make_payload_update(2, "y", serde_json::json!(2)),
        ];

        // rewind 到 version 1（StateTransition）
        let result = rewind(&facts, 1);
        assert!(result.is_some());
        let snap = result.unwrap();
        assert_eq!(snap.version, 1);
        assert_eq!(snap.payload, serde_json::json!({"x": 1}));

        // rewind 到 version 2（PayloadUpdate）
        let result = rewind(&facts, 2);
        assert!(result.is_some());
        let snap = result.unwrap();
        assert_eq!(snap.version, 2);
        assert_eq!(snap.payload, serde_json::json!({"x": 1, "y": 2}));
    }

    /// 验证 StateTransition + IoResponse + PayloadUpdate 混合场景
    #[test]
    fn test_rewind_mixed_facts() {
        let facts = vec![
            make_state_transition(1, serde_json::json!({"a": 1})),
            make_io_response(2),
            make_payload_update(3, "b", serde_json::json!(2)),
            make_state_transition(4, serde_json::json!({"a": 1, "b": 2, "c": 3})),
        ];

        // rewind 到 version 3（PayloadUpdate 后，StateTransition 前）
        let result = rewind(&facts, 3);
        assert!(result.is_some());
        let snap = result.unwrap();
        assert_eq!(snap.version, 3);
        assert_eq!(snap.payload, serde_json::json!({"a": 1, "b": 2}));

        // rewind 到 version 4（StateTransition 覆盖整个 payload）
        let result = rewind(&facts, 4);
        assert!(result.is_some());
        let snap = result.unwrap();
        assert_eq!(snap.version, 4);
        assert_eq!(snap.payload, serde_json::json!({"a": 1, "b": 2, "c": 3}));
    }

    /// 验证 PayloadUpdate 的嵌套路径创建（与 reactor::update_payload 一致）
    #[test]
    fn test_rewind_payload_update_nested_path() {
        let facts = vec![
            make_state_transition(1, serde_json::json!({})),
            make_payload_update(2, "user.profile.name", serde_json::json!("Alice")),
        ];

        let result = rewind(&facts, 2);
        assert!(result.is_some());
        let snap = result.unwrap();
        assert_eq!(snap.version, 2);
        assert_eq!(
            snap.payload,
            serde_json::json!({"user": {"profile": {"name": "Alice"}}})
        );
    }

    /// 验证 diff 在 PayloadUpdate 场景下正确计算差异
    #[test]
    fn test_diff_with_payload_update() {
        let facts = vec![
            make_state_transition(1, serde_json::json!({"a": 1})),
            make_payload_update(2, "b", serde_json::json!(2)),
        ];

        let diff_result = diff(&facts, 1, 2).unwrap();
        assert_eq!(diff_result.added.len(), 1);
        assert_eq!(diff_result.added[0].0, "b");
        assert_eq!(diff_result.removed.len(), 0);
        assert_eq!(diff_result.changed.len(), 0);
        assert_eq!(diff_result.unchanged.len(), 1);
        assert_eq!(diff_result.unchanged[0], "a");
    }

    /// 验证 PayloadUpdate 修改已存在字段时 diff 计算为 changed
    #[test]
    fn test_diff_payload_update_changed_field() {
        let facts = vec![
            make_state_transition(1, serde_json::json!({"a": 1})),
            make_payload_update(2, "a", serde_json::json!(99)),
        ];

        let diff_result = diff(&facts, 1, 2).unwrap();
        assert_eq!(diff_result.changed.len(), 1);
        assert_eq!(diff_result.changed[0].0, "a");
        assert_eq!(diff_result.changed[0].1, serde_json::json!(1));
        assert_eq!(diff_result.changed[0].2, serde_json::json!(99));
        assert_eq!(diff_result.added.len(), 0);
        assert_eq!(diff_result.removed.len(), 0);
    }
}

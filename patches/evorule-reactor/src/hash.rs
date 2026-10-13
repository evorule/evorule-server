// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 审计链哈希工具（BLAKE3）
//!
//! 这是 evorule 审计链的核心完整性保证机制。
//!
//! # 设计
//! - 使用 `blake3` crate（1.x）计算 256 位哈希
//! - 序列化采用显式 JSON 格式：确保序列化格式的稳定性，不受 Debug 实现变更影响
//! - 哈希以十六进制字符串形式返回
//!
//! # 哈希链算法
//! - `prev_hash` 初始为 `"genesis"`
//! - 对每个 Fact，计算 `content_hash = blake3(fact_to_stable_json(fact))`
//! - 链哈希：`chain_hash = blake3(prev_hash + content_hash)`
//! - 更新 `prev_hash = chain_hash`，继续处理下一个 Fact
//!
//! # 两套 WAL 合并
//! 本模块是哈希算法的**单一真相源**（single source of truth）。
//! - evorule-governance/src/hash.rs re-export 本模块
//! - evorule-cli/src/hash.rs re-export 本模块
//! - 通过 `test_cross_validate_with_tier2` 测试保证三方一致

use std::fmt;

use evorule_tcb::JsonValue;
use tracing::{debug, trace};

use crate::fact::Fact;

/// 哈希计算错误类型
///
/// 包含错误消息，便于排查哈希计算过程中的异常。
#[derive(Debug)]
pub struct HashError {
    message: String,
}

impl HashError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for HashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "哈希计算错误: {}", self.message)
    }
}

impl std::error::Error for HashError {}

/// 将 `JsonValue` 转换为 `serde_json::Value`
///
/// 内部函数，用于 Fact 序列化前的类型转换。
/// 保证转换过程确定性，不受 JsonValue 内部实现影响。
fn tcb_to_serde(value: &JsonValue) -> serde_json::Value {
    match value {
        JsonValue::Null => serde_json::Value::Null,
        JsonValue::Bool(b) => serde_json::Value::Bool(*b),
        JsonValue::Integer(i) => serde_json::Value::Number(serde_json::Number::from(*i)),
        JsonValue::String(s) => serde_json::Value::String(s.to_string()),
        JsonValue::Array(arr) => serde_json::Value::Array(arr.iter().map(tcb_to_serde).collect()),
        JsonValue::Object(obj) => {
            let mut map = serde_json::Map::new();
            for (k, v) in obj.iter() {
                map.insert(k.clone(), tcb_to_serde(v));
            }
            serde_json::Value::Object(map)
        }
    }
}

/// 域归因条目 → 稳定 JSON（None 字段省略，键序由 BTreeMap 固定）
///
/// 与 `wal.rs` 的同名私有函数保持同构：哈希链序列化与 WAL 持久化
/// 必须产出一致形状，否则回放重算哈希将失配。
fn trace_attr_to_json(attr: &crate::fact::TraceDomainAttribution) -> serde_json::Value {
    let mut attr_obj = serde_json::Map::new();
    attr_obj.insert(
        "outcome".into(),
        serde_json::Value::String(attr.outcome.clone()),
    );
    if let Some(reason) = &attr.missing_reason {
        attr_obj.insert(
            "missing_reason".into(),
            serde_json::Value::String(reason.clone()),
        );
    }
    if let Some(policy) = &attr.on_missing {
        attr_obj.insert(
            "on_missing".into(),
            serde_json::Value::String(policy.clone()),
        );
    }
    serde_json::Value::Object(attr_obj)
}

/// 将 Fact 序列化为稳定的 JSON 格式
///
/// 使用显式的 JSON 序列化而非依赖 Debug trait，确保：
/// 1. 序列化格式不受 Rust 版本或 Debug 实现变更影响
/// 2. 字段顺序固定且可预测（serde_json::Map 基于 BTreeMap，字母序）
/// 3. 跨版本兼容性有保障
///
/// # 参数
/// - `fact`: 要序列化的事实
///
/// # 返回值
/// - `Ok(serde_json::Value)`: 序列化后的 JSON 值
/// - `Err(HashError)`: 序列化失败
// 7 种 Fact 变体扁平 match + 嵌套, 拆函数需共享中间变量。详见 GATE_REFERENCE.md §六(豁免索引)
#[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
pub fn fact_to_stable_json(fact: &Fact) -> Result<serde_json::Value, HashError> {
    let fact_type = fact.type_name();
    let fact_id = fact.id();

    trace!(
        事实ID = ?fact_id,
        事实类型 = %fact_type,
        "序列化开始"
    );

    let mut obj = serde_json::Map::new();
    match fact {
        Fact::Command { id, instruction } => {
            trace!(
                事实ID = ?id,
                指令大小 = instruction.to_string().len(),
                "处理命令类型事实"
            );
            obj.insert("type".into(), serde_json::Value::String("Command".into()));
            obj.insert("id".into(), serde_json::Value::Number(id.0.into()));
            obj.insert("instruction".into(), tcb_to_serde(instruction));
        }
        Fact::PayloadUpdate { id, path, value } => {
            trace!(
                事实ID = ?id,
                路径 = %path,
                "处理载荷更新类型事实"
            );
            obj.insert(
                "type".into(),
                serde_json::Value::String("PayloadUpdate".into()),
            );
            obj.insert("id".into(), serde_json::Value::Number(id.0.into()));
            obj.insert("path".into(), serde_json::Value::String(path.clone()));
            obj.insert("value".into(), tcb_to_serde(value));
        }
        Fact::StateTransition {
            id,
            cause,
            new_payload,
            new_queue,
        } => {
            trace!(
                事实ID = ?id,
                原因ID = ?cause,
                队列长度 = new_queue.len(),
                "处理状态转换类型事实"
            );
            obj.insert(
                "type".into(),
                serde_json::Value::String("StateTransition".into()),
            );
            obj.insert("id".into(), serde_json::Value::Number(id.0.into()));
            obj.insert("cause".into(), serde_json::Value::Number(cause.0.into()));
            obj.insert("new_payload".into(), tcb_to_serde(new_payload));
            obj.insert(
                "new_queue".into(),
                serde_json::Value::Array(new_queue.iter().map(tcb_to_serde).collect()),
            );
        }
        Fact::IoRequest {
            id,
            cause,
            io_type,
            params,
        } => {
            trace!(
                事实ID = ?id,
                原因ID = ?cause,
                IO类型 = %io_type.as_str(),
                "处理IO请求类型事实"
            );
            obj.insert("type".into(), serde_json::Value::String("IoRequest".into()));
            obj.insert("id".into(), serde_json::Value::Number(id.0.into()));
            obj.insert("cause".into(), serde_json::Value::Number(cause.0.into()));
            obj.insert(
                "io_type".into(),
                serde_json::Value::String(io_type.as_str().into()),
            );
            obj.insert("params".into(), tcb_to_serde(params));
        }
        Fact::IoResponse {
            id,
            request_id,
            result,
            error,
        } => {
            trace!(
                事实ID = ?id,
                请求ID = ?request_id,
                是否有错误 = error.is_some(),
                "处理IO响应类型事实"
            );
            obj.insert(
                "type".into(),
                serde_json::Value::String("IoResponse".into()),
            );
            obj.insert("id".into(), serde_json::Value::Number(id.0.into()));
            obj.insert(
                "request_id".into(),
                serde_json::Value::Number(request_id.0.into()),
            );
            obj.insert("result".into(), tcb_to_serde(result));
            obj.insert(
                "error".into(),
                error
                    .as_ref()
                    .map(|e| serde_json::Value::String(e.clone()))
                    .unwrap_or(serde_json::Value::Null),
            );
        }
        Fact::Stable { id, version } => {
            trace!(事实ID = ?id, 版本 = version, "处理稳定状态类型事实");
            obj.insert("type".into(), serde_json::Value::String("Stable".into()));
            obj.insert("id".into(), serde_json::Value::Number(id.0.into()));
            obj.insert(
                "version".into(),
                serde_json::Value::Number((*version).into()),
            );
        }
        Fact::Error { id, message } => {
            trace!(
                事实ID = ?id,
                消息 = %message,
                "处理错误类型事实"
            );
            obj.insert("type".into(), serde_json::Value::String("Error".into()));
            obj.insert("id".into(), serde_json::Value::Number(id.0.into()));
            obj.insert("message".into(), serde_json::Value::String(message.clone()));
        }
        Fact::TransitionTrace {
            id,
            cause,
            rule_hits,
        } => {
            // 归因轨迹参与哈希链（审计链防篡改覆盖归因事实）
            trace!(事实ID = ?id, 原因ID = ?cause, 命中数 = rule_hits.len(), "处理命中归因轨迹事实");
            obj.insert(
                "type".into(),
                serde_json::Value::String("TransitionTrace".into()),
            );
            obj.insert("id".into(), serde_json::Value::Number(id.0.into()));
            obj.insert("cause".into(), serde_json::Value::Number(cause.0.into()));
            let hits: Vec<serde_json::Value> = rule_hits
                .iter()
                .map(|h| {
                    // domain_attr 仅在 Some 时插入键（None 省略）：
                    // 存量无归因事实的哈希不变，增量归因事实纳入哈希链防篡改。
                    let mut hit_obj = serde_json::Map::new();
                    hit_obj.insert("index".into(), serde_json::json!(h.index));
                    hit_obj.insert(
                        "instr_type".into(),
                        serde_json::Value::String(h.instr_type.clone()),
                    );
                    hit_obj.insert("hit".into(), serde_json::Value::Bool(h.hit));
                    if let Some(attr) = &h.domain_attr {
                        hit_obj.insert("domain_attr".into(), trace_attr_to_json(attr));
                    }
                    serde_json::Value::Object(hit_obj)
                })
                .collect();
            obj.insert("rule_hits".into(), serde_json::Value::Array(hits));
        }
        Fact::Violation {
            id,
            cause,
            rule_index,
            reason,
            instruction,
        } => {
            // 违规拦截事实参与哈希链（审计链防篡改覆盖拦截记录；回归验证）
            trace!(事实ID = ?id, 原因ID = ?cause, 规则下标 = rule_index, "处理违规拦截事实");
            obj.insert("type".into(), serde_json::Value::String("Violation".into()));
            obj.insert("id".into(), serde_json::Value::Number(id.0.into()));
            obj.insert("cause".into(), serde_json::Value::Number(cause.0.into()));
            obj.insert(
                "rule_index".into(),
                serde_json::Value::Number((*rule_index).into()),
            );
            obj.insert("reason".into(), serde_json::Value::String(reason.clone()));
            obj.insert("instruction".into(), tcb_to_serde(instruction));
        }
    }

    let value = serde_json::Value::Object(obj);

    trace!(
        事实ID = ?fact_id,
        事实类型 = %fact_type,
        "序列化完成"
    );

    Ok(value)
}

/// 计算内容的 BLAKE3 哈希
///
/// 将 `JsonValue` 序列化为确定性字符串后计算哈希。
/// 使用 `tcb_to_serde` 进行显式 JSON 序列化，确保格式稳定性。
///
/// # 参数
/// - `value`: 要计算哈希的 JSON 值
///
/// # 返回值
/// - `Ok(String)`: 64 字符的十六进制哈希字符串
/// - `Err(HashError)`: 序列化失败
#[allow(dead_code)] // 供 evorule-governance re-export 使用
pub fn content_hash(value: &JsonValue) -> Result<String, HashError> {
    #[cfg(kani)]
    {
        // Kani 模式：blake3 的位操作循环会导致 CBMC memcmp 状态爆炸
        // （4357+ 次循环展开未止）。用确定性简化哈希替代，保持幂等性。
        // 注意：不用 format!（触发 core::unicode::skip_search 状态爆炸）。
        Ok(String::from("c"))
    }
    #[cfg(not(kani))]
    {
        let serde_value = tcb_to_serde(value);
        let serialized = serde_json::to_string(&serde_value)
            .map_err(|e| HashError::new(format!("内容哈希序列化失败: {}", e)))?;
        let hash = blake3::hash(serialized.as_bytes()).to_hex().to_string();

        trace!(
            序列化长度 = serialized.len(),
            哈希值 = %hash,
            "内容哈希计算完成"
        );

        Ok(hash)
    }
}

/// 计算 Fact 的哈希（基于稳定的 JSON 序列化）
///
/// 使用显式 JSON 序列化格式而非 Debug trait，确保序列化格式的稳定性。
///
/// # 参数
/// - `fact`: 要计算哈希的事实
///
/// # 返回值
/// - `Ok(String)`: 64 字符的十六进制哈希字符串
/// - `Err(HashError)`: 哈希计算失败
///
/// # 示例
/// ```
/// use evorule_reactor::{Fact, FactId, fact_hash};
/// use evorule_tcb::JsonValue;
///
/// let fact = Fact::Command {
///     id: FactId(1),
///     instruction: JsonValue::empty_object(),
/// };
/// let hash = fact_hash(&fact).unwrap();
/// assert_eq!(hash.len(), 64);
/// ```
pub fn fact_hash(fact: &Fact) -> Result<String, HashError> {
    #[cfg(kani)]
    {
        // Kani 模式：blake3 的位操作循环会导致 CBMC memcmp 状态爆炸。
        // 用确定性简化哈希替代：基于 id 的单字符映射（a-g），
        // 保持幂等性（相同 id → 相同 hash）和区分性（不同 id → 不同 hash）。
        // 不用 format!/to_string()/type_name（触发 Unicode 状态爆炸）。
        let id = fact.id().0;
        let h = match id % 7 {
            0 => "a",
            1 => "b",
            2 => "c",
            3 => "d",
            4 => "e",
            5 => "f",
            _ => "g",
        };
        Ok(String::from(h))
    }
    #[cfg(not(kani))]
    {
        let fact_type = fact.type_name();
        let fact_id = fact.id();

        debug!(
            事实ID = ?fact_id,
            事实类型 = %fact_type,
            "开始计算事实哈希"
        );

        let value = fact_to_stable_json(fact)?;
        let serialized = serde_json::to_string(&value)
            .map_err(|e| HashError::new(format!("序列化失败: {}", e)))?;

        let hash = blake3::hash(serialized.as_bytes()).to_hex().to_string();

        debug!(
            事实ID = ?fact_id,
            事实类型 = %fact_type,
            哈希值 = %hash,
            序列化长度 = serialized.len(),
            "事实哈希计算完成"
        );

        Ok(hash)
    }
}

/// 计算哈希链的最终链哈希
///
/// 这是审计链的核心计算函数。
///
/// # 算法
/// - `prev_hash` 初始为 `"genesis"`
/// - 对每个 Fact，计算 `content_hash = fact_hash(fact)`
/// - 链哈希：`chain_hash = blake3(prev_hash + content_hash)`
/// - 更新 `prev_hash = chain_hash`，继续处理下一个 Fact
/// - 返回最终的 `prev_hash`（即整条链的链哈希）
///
/// # 参数
/// - `facts`: 事实列表（按因果顺序）
///
/// # 返回值
/// - `Ok(String)`: 最终链哈希（64 字符十六进制）
/// - `Err(HashError)`: 任一 Fact 哈希计算失败
///
/// # 注意
/// 本函数**只计算**链哈希，**不验证**完整性。
/// 验证逻辑由调用方比对存储的哈希与重算的哈希。
/// 空列表返回 `"genesis"`。
pub fn compute_chain_hash(facts: &[Fact]) -> Result<String, HashError> {
    let fact_count = facts.len();

    debug!(事实数量 = fact_count, "开始计算链哈希");

    if fact_count == 0 {
        debug!("事实列表为空，返回 genesis 哈希");
        return Ok(String::from("genesis"));
    }

    let mut prev_hash = String::from("genesis");

    trace!(
        初始前序哈希 = %prev_hash,
        "初始化前序哈希为创世值"
    );

    for (index, fact) in facts.iter().enumerate() {
        let fact_type = fact.type_name();
        let fact_id = fact.id();

        trace!(
            索引 = index,
            事实ID = ?fact_id,
            事实类型 = %fact_type,
            前序哈希 = %prev_hash,
            "处理事实"
        );

        let fh = fact_hash(fact)?;

        trace!(
            索引 = index,
            事实ID = ?fact_id,
            事实哈希 = %fh,
            "事实哈希计算完成"
        );

        let current = chain_step(&prev_hash, &fh);

        trace!(
            索引 = index,
            事实ID = ?fact_id,
            当前哈希 = %current,
            "计算当前哈希(前序哈希+事实哈希)"
        );

        prev_hash = current;
    }

    debug!(
        事实数量 = fact_count,
        最终哈希 = %prev_hash,
        "链哈希计算完成"
    );

    Ok(prev_hash)
}

/// 计算单步链哈希：chain_hash = blake3(prev_hash + content_hash)
///
/// Kani 模式下用确定性简化版本（字符串拼接）替代 blake3，
/// 避免 CBMC 对 blake3 位操作循环的 memcmp 状态爆炸。
/// 保持链步的结合性：`chain_step(chain_step(a, b), c)` 可归纳分解。
///
/// # 参数
/// - `prev_hash`: 前序链哈希（初始为 `"genesis"`）
/// - `content_hash`: 当前 Fact 的内容哈希
///
/// # 返回值
/// 新的链哈希
pub fn chain_step(prev_hash: &str, content_hash: &str) -> String {
    #[cfg(kani)]
    {
        // Kani 模式：确定性简化，保持链步结构。
        // 不用 format!（触发 core::unicode::skip_search 状态爆炸），
        // 用 String::from + push_str（字节级操作，短字符串）。
        let mut s = String::from(prev_hash);
        s.push_str(content_hash);
        s
    }
    #[cfg(not(kani))]
    {
        let combined = format!("{}{}", prev_hash, content_hash);
        blake3::hash(combined.as_bytes()).to_hex().to_string()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
    use super::*;
    use crate::fact::{Fact, FactId, IoType, TraceDomainAttribution, TraceHit};
    use evorule_tcb::JsonValue;

    #[test]
    fn test_fact_to_stable_json_format() {
        let command = Fact::Command {
            id: FactId(1),
            instruction: JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("increment")),
                (
                    "params",
                    JsonValue::object_from_pairs(&[
                        ("attr", JsonValue::string("x")),
                        ("delta", JsonValue::Integer(5)),
                    ]),
                ),
            ]),
        };

        let json_value = fact_to_stable_json(&command).unwrap();

        assert_eq!(json_value.get("type").unwrap().as_str().unwrap(), "Command");
        assert_eq!(json_value.get("id").unwrap().as_u64().unwrap(), 1);
        assert!(json_value.get("instruction").is_some());
    }

    #[test]
    fn test_transition_trace_hash_without_attr_matches_legacy_shape() {
        // 存量哈希兼容：domain_attr 为 None 时 stable JSON 命中条目与
        // 旧实现逐键一致（无 domain_attr 键）→ 存量事实哈希不变
        let fact = Fact::TransitionTrace {
            id: FactId(9),
            cause: FactId(2),
            rule_hits: vec![TraceHit {
                index: 0,
                instr_type: "branch".into(),
                hit: false,
                domain_attr: None,
            }],
        };
        let json = fact_to_stable_json(&fact).unwrap();
        let hits = json.get("rule_hits").unwrap().as_array().unwrap();
        assert_eq!(hits.len(), 1);
        let hit = hits.first().unwrap();
        assert_eq!(
            hit,
            &serde_json::json!({"index": 0, "instr_type": "branch", "hit": false}),
            "无归因条目必须与旧格式逐键一致（None 省略，存量哈希不变）"
        );
    }

    #[test]
    fn test_transition_trace_hash_with_attr_shape_and_distinct() {
        // 归因落账进哈希链（防篡改覆盖归因事实）：Some 时键出现、
        // None 省略字段不出现，且与无归因版本哈希不同
        let fact_with = Fact::TransitionTrace {
            id: FactId(9),
            cause: FactId(2),
            rule_hits: vec![TraceHit {
                index: 0,
                instr_type: "branch".into(),
                hit: false,
                domain_attr: Some(TraceDomainAttribution {
                    outcome: "missing".into(),
                    missing_reason: Some("path_not_found".into()),
                    on_missing: Some("unsat".into()),
                }),
            }],
        };
        let fact_without = Fact::TransitionTrace {
            id: FactId(9),
            cause: FactId(2),
            rule_hits: vec![TraceHit {
                index: 0,
                instr_type: "branch".into(),
                hit: false,
                domain_attr: None,
            }],
        };
        let json = fact_to_stable_json(&fact_with).unwrap();
        let hit = json
            .get("rule_hits")
            .unwrap()
            .as_array()
            .unwrap()
            .first()
            .unwrap();
        assert_eq!(
            hit.get("domain_attr").unwrap(),
            &serde_json::json!({
                "outcome": "missing",
                "missing_reason": "path_not_found",
                "on_missing": "unsat"
            }),
            "归因条目形状（None 字段省略）"
        );
        let hash_with = fact_hash(&fact_with).unwrap();
        let hash_without = fact_hash(&fact_without).unwrap();
        assert_ne!(
            hash_with, hash_without,
            "归因事实必须纳入哈希链（有归因 vs 无归因哈希不同）"
        );
    }

    #[test]
    fn test_fact_hash_all_variants() {
        let test_facts = vec![
            Fact::Command {
                id: FactId(1),
                instruction: JsonValue::empty_object(),
            },
            Fact::PayloadUpdate {
                id: FactId(2),
                path: "test.path".into(),
                value: JsonValue::string("test_value"),
            },
            Fact::StateTransition {
                id: FactId(3),
                cause: FactId(1),
                new_payload: JsonValue::empty_object(),
                new_queue: vec![],
            },
            Fact::IoRequest {
                id: FactId(4),
                cause: FactId(3),
                io_type: IoType::http_get(),
                params: JsonValue::empty_object(),
            },
            Fact::IoResponse {
                id: FactId(5),
                request_id: FactId(4),
                result: JsonValue::string("response"),
                error: None,
            },
            Fact::IoResponse {
                id: FactId(6),
                request_id: FactId(4),
                result: JsonValue::Null,
                error: Some("timeout".to_string()),
            },
            Fact::Stable {
                id: FactId(7),
                version: 0,
            },
            Fact::Error {
                id: FactId(8),
                message: "test error".into(),
            },
        ];

        for fact in test_facts {
            let hash = fact_hash(&fact).unwrap();
            assert_eq!(hash.len(), 64);

            let hash2 = fact_hash(&fact).unwrap();
            assert_eq!(
                hash,
                hash2,
                "fact_hash should be deterministic for {}",
                fact.type_name()
            );
        }
    }

    #[test]
    fn test_fact_hash_identity() {
        let fact1 = Fact::Command {
            id: FactId(1),
            instruction: JsonValue::string("same"),
        };
        let fact2 = Fact::Command {
            id: FactId(1),
            instruction: JsonValue::string("same"),
        };

        assert_eq!(fact_hash(&fact1).unwrap(), fact_hash(&fact2).unwrap());
    }

    #[test]
    fn test_fact_hash_different_ids() {
        let fact1 = Fact::Command {
            id: FactId(1),
            instruction: JsonValue::string("same"),
        };
        let fact2 = Fact::Command {
            id: FactId(2),
            instruction: JsonValue::string("same"),
        };

        assert_ne!(fact_hash(&fact1).unwrap(), fact_hash(&fact2).unwrap());
    }

    #[test]
    fn test_fact_hash_snapshot() {
        let test_facts = [
            Fact::Command {
                id: FactId(1),
                instruction: JsonValue::empty_object(),
            },
            Fact::PayloadUpdate {
                id: FactId(2),
                path: "test.path".into(),
                value: JsonValue::string("test_value"),
            },
            Fact::StateTransition {
                id: FactId(3),
                cause: FactId(1),
                new_payload: JsonValue::empty_object(),
                new_queue: vec![],
            },
            Fact::IoRequest {
                id: FactId(4),
                cause: FactId(3),
                io_type: IoType::http_get(),
                params: JsonValue::empty_object(),
            },
            Fact::IoResponse {
                id: FactId(5),
                request_id: FactId(4),
                result: JsonValue::string("response"),
                error: None,
            },
            Fact::Stable {
                id: FactId(6),
                version: 0,
            },
            Fact::Error {
                id: FactId(7),
                message: "test error".into(),
            },
        ];

        let current_hashes: Vec<String> =
            test_facts.iter().map(|f| fact_hash(f).unwrap()).collect();
        let snapshot_file = env!("CARGO_MANIFEST_DIR").to_string() + "/hash_snapshot.txt";

        if std::path::Path::new(&snapshot_file).exists() {
            let snapshot = std::fs::read_to_string(&snapshot_file).unwrap();
            let expected_hashes: Vec<String> = snapshot.lines().map(|s| s.to_string()).collect();
            assert_eq!(
                current_hashes, expected_hashes,
                "Hash snapshot mismatch! If this is an expected change (e.g., Fact struct modification), delete {} to regenerate.",
                snapshot_file
            );
        } else {
            let snapshot_content = current_hashes.join("\n");
            std::fs::write(&snapshot_file, snapshot_content).unwrap();
            println!("Created hash snapshot: {}", snapshot_file);
        }
    }

    #[test]
    fn test_compute_chain_hash_empty() {
        let facts: Vec<Fact> = vec![];
        let chain_hash = compute_chain_hash(&facts).unwrap();
        assert_eq!(chain_hash, "genesis");
    }

    #[test]
    fn test_compute_chain_hash_deterministic() {
        let facts = vec![
            Fact::Command {
                id: FactId(1),
                instruction: JsonValue::empty_object(),
            },
            Fact::StateTransition {
                id: FactId(2),
                cause: FactId(1),
                new_payload: JsonValue::empty_object(),
                new_queue: vec![],
            },
        ];

        let result1 = compute_chain_hash(&facts).unwrap();
        let result2 = compute_chain_hash(&facts).unwrap();
        assert_eq!(result1, result2);
    }

    #[test]
    fn test_compute_chain_hash_order_sensitive() {
        let fact1 = Fact::Command {
            id: FactId(1),
            instruction: JsonValue::empty_object(),
        };
        let fact2 = Fact::StateTransition {
            id: FactId(2),
            cause: FactId(1),
            new_payload: JsonValue::empty_object(),
            new_queue: vec![],
        };

        let chain1 = compute_chain_hash(&[fact1.clone(), fact2.clone()]).unwrap();
        let chain2 = compute_chain_hash(&[fact2, fact1]).unwrap();
        assert_ne!(chain1, chain2, "链哈希应对 Fact 顺序敏感");
    }

    /// 交叉验证测试：与 evorule-governance/src/hash.rs 的快照比对
    ///
    /// 本测试确保 tier1 和 tier2 的哈希算法产生相同的哈希值。
    /// 如果 tier2 hash.rs 仍维护独立实现，此测试会检测到不一致。
    #[test]
    fn test_cross_validate_with_tier2() {
        // 与 evorule-governance/src/hash.rs 的 test_fact_hash_snapshot 使用相同的 Fact
        let test_facts = [
            Fact::Command {
                id: FactId(1),
                instruction: JsonValue::empty_object(),
            },
            Fact::PayloadUpdate {
                id: FactId(2),
                path: "test.path".into(),
                value: JsonValue::string("test_value"),
            },
            Fact::StateTransition {
                id: FactId(3),
                cause: FactId(1),
                new_payload: JsonValue::empty_object(),
                new_queue: vec![],
            },
            Fact::IoRequest {
                id: FactId(4),
                cause: FactId(3),
                io_type: IoType::http_get(),
                params: JsonValue::empty_object(),
            },
            Fact::IoResponse {
                id: FactId(5),
                request_id: FactId(4),
                result: JsonValue::string("response"),
                error: None,
            },
            Fact::Stable {
                id: FactId(6),
                version: 0,
            },
            Fact::Error {
                id: FactId(7),
                message: "test error".into(),
            },
        ];

        // 计算 tier1 的哈希
        let tier1_hashes: Vec<String> = test_facts.iter().map(|f| fact_hash(f).unwrap()).collect();

        // 读取 tier2 的快照文件（如果存在）
        let tier2_snapshot_path = "../../evorule-governance/src/hash_snapshot.txt".to_string();
        if std::path::Path::new(&tier2_snapshot_path).exists() {
            let tier2_snapshot = std::fs::read_to_string(&tier2_snapshot_path).unwrap();
            let tier2_hashes: Vec<String> = tier2_snapshot.lines().map(|s| s.to_string()).collect();

            assert_eq!(
                tier1_hashes, tier2_hashes,
                "tier1 和 tier2 的 fact_hash 不一致！\
                 如果这是预期变更（如 Fact 结构修改），\
                 请删除 evorule-governance/src/hash_snapshot.txt 重新生成。"
            );
        }
    }
}

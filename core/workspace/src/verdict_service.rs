// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 判定契约服务 + wall-clock 旁路服务 — 界面升级 v1.0 阶段 A.3/A.4
//!
//! 设计依据: 实施文档_界面升级_v1.0.md §四 A.3/A.4 + 00_架构边界原则.md §七
//!
//! # 职责边界 (公共层旁路, 非确定性)
//! - 判定契约是**应用层业务判定**, 不需要确定性, 不进 evorule 仓/审计链
//! - evaluate 返回值必须含确定性标注 note
//! - version_clock_map 是**旁路索引**, 绝不进审计链哈希 (00 §六 Fact 无 wall-clock)
//!
//! # 转译 vs 判定
//! - 转译 (rule_translate.rs): 结构转换纯函数, condition/action ↔ transform
//! - 判定 (本服务): 业务语义判定, payload → pass/block/none

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::db::WorkspaceDb;
use crate::error::{WorkspaceError, WorkspaceResult};
use crate::models::{VerdictContractRecord, VersionClockMapRecord};

/// 判定契约 + wall-clock 旁路服务
pub struct VerdictService {
    db: Arc<WorkspaceDb>,
}

impl VerdictService {
    pub fn new(db: Arc<WorkspaceDb>) -> Self {
        Self { db }
    }

    // ========================================================================
    // 判定契约 CRUD
    // ========================================================================

    /// 创建判定契约
    pub async fn create_contract(
        &self,
        workspace_id: &str,
        req: CreateVerdictContractRequest,
    ) -> WorkspaceResult<VerdictContractRecord> {
        if req.name.trim().is_empty() {
            return Err(WorkspaceError::invalid_input("contract name must not be empty"));
        }
        if req.created_by.trim().is_empty() {
            return Err(WorkspaceError::invalid_input("created_by must not be empty"));
        }
        self.db.insert_verdict_contract(
            workspace_id,
            &req.name,
            &req.rules_json,
            req.is_default,
            &req.created_by,
        )
    }

    /// 列出 workspace 的判定契约
    pub async fn list_contracts(
        &self,
        workspace_id: &str,
    ) -> WorkspaceResult<Vec<VerdictContractRecord>> {
        self.db.list_verdict_contracts(workspace_id)
    }

    /// 获取单条判定契约
    pub async fn get_contract(&self, id: i64) -> WorkspaceResult<VerdictContractRecord> {
        self.db.get_verdict_contract(id)
    }

    /// 更新判定契约 (patch)
    pub async fn update_contract(
        &self,
        id: i64,
        patch: UpdateVerdictContractRequest,
    ) -> WorkspaceResult<VerdictContractRecord> {
        self.db.update_verdict_contract(
            id,
            patch.rules_json.as_deref(),
            patch.is_default,
            patch.name.as_deref(),
        )
    }

    /// 删除判定契约
    pub async fn delete_contract(&self, id: i64) -> WorkspaceResult<()> {
        self.db.delete_verdict_contract(id)
    }

    // ========================================================================
    // 判定 evaluate (应用层业务判定, 非确定性)
    // ========================================================================

    /// 对 payload 做判定
    ///
    /// - contract_id 指定 → 用该契约; 否则用 workspace 默认契约 (is_default=1)
    /// - 逐条匹配 rules_json: {field, op, value, verdict}, 首条匹配胜出
    /// - 无匹配 → verdict="none"
    /// - 返回值含确定性标注 note (00 §七: 应用层判定, 非 evorule 确定性)
    pub async fn evaluate(
        &self,
        workspace_id: &str,
        req: EvaluateVerdictRequest,
    ) -> WorkspaceResult<EvaluateVerdictResult> {
        let contract = if let Some(cid) = req.contract_id {
            self.db.get_verdict_contract(cid)?
        } else {
            self.db.get_default_verdict_contract(workspace_id)?.ok_or_else(|| {
                WorkspaceError::not_found(
                    "verdict_contract (default)",
                    workspace_id.to_string(),
                )
            })?
        };

        let rules: Vec<Value> = serde_json::from_str(&contract.rules_json).map_err(|e| {
            WorkspaceError::internal(format!("invalid rules_json in contract: {e}"))
        })?;

        let mut matched_rule_id: Option<String> = None;
        let mut verdict = "none".to_string();

        for (i, rule) in rules.iter().enumerate() {
            if match_rule(rule, &req.payload) {
                // 取该条声明的 verdict (默认 "block")
                verdict = rule
                    .get("verdict")
                    .and_then(|v| v.as_str())
                    .unwrap_or("block")
                    .to_string();
                matched_rule_id = rule
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| Some(format!("rule[{i}]")));
                break;
            }
        }

        Ok(EvaluateVerdictResult {
            verdict,
            matched_rule_id,
            source_workspace_ids: vec![workspace_id.to_string()],
            note: "应用层判定（非 evorule 确定性）".to_string(),
        })
    }

    // ========================================================================
    // wall-clock 旁路 (version_clock_map)
    // ========================================================================

    /// 旁路记录 version → wall-clock (供 server reactor 产生 Fact 时事务外调用)
    ///
    /// 设计约束: 绝不进审计链哈希 (00 §六/§七)。
    pub async fn record_clock(
        &self,
        session_id: i64,
        version: i64,
        wall_clock: &str,
        source: Option<&str>,
    ) -> WorkspaceResult<VersionClockMapRecord> {
        self.db
            .record_version_clock(session_id, version, wall_clock, source)
    }

    /// 范围查询 [from_version, to_version] 的 wall-clock (供 console 批量查询)
    pub async fn lookup_clock(
        &self,
        session_id: i64,
        from_version: Option<i64>,
        to_version: Option<i64>,
    ) -> WorkspaceResult<Vec<VersionClockMapRecord>> {
        self.db
            .lookup_version_clock_range(session_id, from_version, to_version)
    }
}

// =============================================================================
// DTO
// =============================================================================

/// 创建判定契约请求 (对齐 A.3 POST body: {name, rules_json, is_default})
#[derive(Debug, Deserialize)]
pub struct CreateVerdictContractRequest {
    pub name: String,
    /// 条件集合 JSON 文本: [{field, op, value, verdict}]
    pub rules_json: String,
    #[serde(default)]
    pub is_default: bool,
    pub created_by: String,
}

/// 更新判定契约请求 (PATCH, 全字段可选)
#[derive(Debug, Deserialize, Default)]
pub struct UpdateVerdictContractRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub rules_json: Option<String>,
    #[serde(default)]
    pub is_default: Option<bool>,
}

/// evaluate 请求 (对齐 A.3: {payload, contract_id?})
#[derive(Debug, Deserialize)]
pub struct EvaluateVerdictRequest {
    pub payload: Value,
    #[serde(default)]
    pub contract_id: Option<i64>,
}

/// evaluate 响应 (对齐 A.3: {verdict, matched_rule_id?, source_workspace_ids, note})
#[derive(Debug, Serialize)]
pub struct EvaluateVerdictResult {
    pub verdict: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_rule_id: Option<String>,
    pub source_workspace_ids: Vec<String>,
    pub note: String,
}

/// clock/record 请求 (对齐 A.4: {version, wall_clock})
#[derive(Debug, Deserialize)]
pub struct RecordClockRequest {
    pub version: i64,
    pub wall_clock: String,
    #[serde(default)]
    pub source: Option<String>,
}

/// clock/lookup 查询参数 (对齐 A.4: from_version, to_version)
#[derive(Debug, Deserialize, Default)]
pub struct LookupClockQuery {
    pub from_version: Option<i64>,
    pub to_version: Option<i64>,
}

// =============================================================================
// 匹配引擎 (纯函数, 应用层判定)
// =============================================================================

/// 单条规则匹配
///
/// rule 形如: {field, op, value, verdict}
/// - op="eq":   payload[field] == value
/// - op="lt":   payload[field] < value (数值比较)
/// - op="gt":   payload[field] > value
/// - op="exists": payload 含 field 键
fn match_rule(rule: &Value, payload: &Value) -> bool {
    let Some(rule_obj) = rule.as_object() else {
        return false;
    };
    let field = rule_obj.get("field").and_then(|v| v.as_str()).unwrap_or("");
    let op = rule_obj.get("op").and_then(|v| v.as_str()).unwrap_or("eq");
    let value = rule_obj.get("value");

    let payload_val = payload.get(field);

    match op {
        "eq" => payload_val == value,
        "lt" => {
            let p = payload_val.and_then(|v| v.as_f64());
            let v = value.and_then(|v| v.as_f64());
            match (p, v) {
                (Some(a), Some(b)) => a < b,
                _ => false,
            }
        }
        "gt" => {
            let p = payload_val.and_then(|v| v.as_f64());
            let v = value.and_then(|v| v.as_f64());
            match (p, v) {
                (Some(a), Some(b)) => a > b,
                _ => false,
            }
        }
        "exists" => payload_val.is_some(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use serde_json::json;

    #[test]
    fn match_eq() {
        let rule = json!({"field": "age", "op": "eq", "value": 18});
        let payload = json!({"age": 18});
        assert!(match_rule(&rule, &payload));
        let payload2 = json!({"age": 20});
        assert!(!match_rule(&rule, &payload2));
    }

    #[test]
    fn match_lt() {
        let rule = json!({"field": "amount", "op": "lt", "value": 100});
        assert!(match_rule(&rule, &json!({"amount": 50})));
        assert!(!match_rule(&rule, &json!({"amount": 150})));
    }

    #[test]
    fn match_exists() {
        let rule = json!({"field": "vip", "op": "exists"});
        assert!(match_rule(&rule, &json!({"vip": true})));
        assert!(!match_rule(&rule, &json!({"age": 1})));
    }
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 权限子系统 —— 机制层数据模型、版本化快照与条件评估
//!
//! # 定位
//! 本模块是治理层能力面新增之一，服务于 evorule-server 的 `/api/permissions` 端点族
//! （应用层策略）。核心职责：
//! - [`entry`]：`PermissionEntry` 数据模型（与实施文档 18 的 schema 对齐）
//! - [`table`]：`PermissionTable` —— 基于 `SharedFactsLog` 的版本化权限快照与判定
//! - [`condition`]：`ConditionEvaluator` —— 复用 TCB `evaluate_domain` 评估条件表达式
//!
//! # 持久化约定
//! 权限条目存储在 `SharedFactsLog` 的 `shared.__permission__.entry.<id>` 路径下，
//! 每次新建/更新以 `append` 追写一个新版本，历史版本保留以保证可审计回放。
//! 版本化快照 `PermissionTable::snapshot_at(log, v_shared)` 据此重建 [v_shared] 时点的
//! 一致视图，保证权限判定确定性（与 `IoCallContext.v_trigger` 语义对齐，见 reactor `io_context`）。
//!
//! # 设计原则（additive）
//! 本模块不复用/不修改任何既有 trait 或签名，仅新增纯数据模型 + 纯函数，
//! 由应用层（evorule-server）负责 HTTP 路由与审批流程编排。

pub mod condition;
pub mod entry;
pub mod gate;
pub mod route_policy;
pub mod table;

pub use condition::ConditionEvaluator;
pub use entry::{
    Effect, PermissionEntry, PermissionError, PermissionState, Resource, ResourceType, Scope,
    Subject, SubjectType,
};
pub use gate::PermissionGate;
pub use route_policy::{AskFallback, PerRoleDefault, RouteDecision, RoutePolicy, RouteRule};
pub use table::{DefaultPolicy, PermissionTable, Verdict};

use evorule_tcb::JsonValue;

/// 将 TCB 的 `JsonValue` 转换为 `serde_json::Value`（用于 serde 反序列化）
///
/// 权限条目从 `SharedFactsLog` 读回时是 TCB 类型，本桥接函数将其无损转为
/// `serde_json::Value`，从而可继续用 serde 反序列化为 `PermissionEntry`。
pub(crate) fn tcb_to_serde(v: &JsonValue) -> serde_json::Value {
    match v {
        JsonValue::Null => serde_json::Value::Null,
        JsonValue::Bool(b) => serde_json::Value::Bool(*b),
        JsonValue::Integer(i) => serde_json::Value::Number((*i).into()),
        // u64 全域通道（v0.9.0）：serde Number 原生无损承载，不经 i64 cast
        JsonValue::U64(v) => serde_json::Value::Number((*v).into()),
        JsonValue::String(s) => serde_json::Value::String(s.to_string()),
        JsonValue::Array(a) => serde_json::Value::Array(a.iter().map(tcb_to_serde).collect()),
        JsonValue::Object(m) => {
            let obj = m
                .iter()
                .map(|(k, value)| (k.clone(), tcb_to_serde(value)))
                .collect::<serde_json::Map<_, _>>();
            serde_json::Value::Object(obj)
        }
    }
}

/// 将 `serde_json::Value` 转换为 TCB 的 `JsonValue`（用于序列化写入共享事实）
///
/// 与 [`tcb_to_serde`] 互为逆操作，供新增/更新权限条目的写入路径使用。
pub(crate) fn serde_to_tcb(v: &serde_json::Value) -> JsonValue {
    match v {
        serde_json::Value::Null => JsonValue::Null,
        serde_json::Value::Bool(b) => JsonValue::Bool(*b),
        serde_json::Value::Number(n) => n
            .as_i64()
            .map(JsonValue::Integer)
            .unwrap_or(JsonValue::Null),
        serde_json::Value::String(s) => JsonValue::String(s.clone().into()),
        serde_json::Value::Array(a) => JsonValue::Array(a.iter().map(serde_to_tcb).collect()),
        serde_json::Value::Object(o) => {
            let mut map = JsonValue::empty_object();
            if let Some(obj) = map.as_object_mut() {
                for (k, value) in o {
                    obj.insert(k.clone(), serde_to_tcb(value));
                }
            }
            map
        }
    }
}

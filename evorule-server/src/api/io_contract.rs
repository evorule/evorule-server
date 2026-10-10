// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! IO 形状契约 v1（E-8 集成契约固化，34 号档）
//!
//! **单一真相源 = 本模块的形状表**（[`IO_CONTRACT_V1_SHAPES`]）：
//! 内置 IoSubscriber 的 skip 谓词（server.rs `is_external_executor_request`）
//! 与对外契约导出（`GET /api/io-contract`）由**同一张表**驱动——改形状必先
//! 改表，改表必过快照测试（`io_contract_v1.snapshot.json`），快照比对失败
//! 即提示 bump 契约版本并更新快照（显式人为动作）。这沿 openapi.rs「从
//! 代码生成契约，杜绝手写文档漂移」的既定哲学，将两仓集成契约从「三个
//! if 语句的参数形状约定」升格为版本化、机读、双侧可锁的显式协议。
//!
//! 契约消费方（evo-agent）侧持有本契约的 pinned 副本（带生成锚 sha256），
//! 启动期协商：端点 404=旧 server（未固化期）warn 通过；版本不在支持集
//! =hard fail fail-closed（34 号档 §2.2）。
//!
//! **版本演进规则**（改动任何形状字段前必读）：
//! - 加/删 required/forbidden 键、改互斥形态 = 破坏性 → bump
//!   [`IO_CONTRACT_VERSION`]，server 需并行支持窗口期时在
//!   [`IO_CONTRACT_SUPPORTED_VERSIONS`] 声明；
//! - 仅新增可选键（旧消费者不违约）→ 可不 bump，但**快照必更新**；
//! - 弃用形态先标 `deprecated` 一个版本再删。

use evorule_reactor::IoType;
use evorule_tcb::JsonValue;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// 当前契约主版本（破坏性变更时 bump；与 [`IO_CONTRACT_SUPPORTED_VERSIONS`]
/// 的最大值保持一致）。
pub const IO_CONTRACT_VERSION: u32 = 1;

/// 本 server 进程支持的契约版本集（协商用）。v1 起步，单元素。
pub const IO_CONTRACT_SUPPORTED_VERSIONS: &[u32] = &[1];

/// 契约生成锚：skip 谓词的源定义（导出体呈现，供消费方核对溯源）。
pub const IO_CONTRACT_SOURCE_MODULE: &str = "evorule-server/src/api/io_contract.rs";

/// 单个外部执行者形态的形状规格。
///
/// 仅 Serialize（导出方向）：真相源是本仓静态表，消费方（evo-agent）
/// 用自家类型解析导出 JSON，无需在本仓反序列化。
#[derive(Debug, Clone, Serialize, ToSchema, PartialEq)]
pub struct IoShapeSpec {
    /// 形态名（与 server.rs 历史谓词函数名对应，稳定标识）
    pub shape: &'static str,
    /// 匹配的 io_type
    pub io_type: &'static str,
    /// 参数必须**在场**的键（存在性判定，不校验值类型——值域属业务层）
    pub required_keys: &'static [&'static str],
    /// 参数必须**缺席**的键（互斥防御：命中即由平台内置 handler 应答，
    /// 外部执行者不得回写——防 stale 拒绝断链）
    pub forbidden_keys: &'static [&'static str],
    /// 形态的执行者/消费方
    pub consumer: IoShapeConsumer,
    /// 一句话语义
    pub semantics: &'static str,
    /// 弃用标记（None=现行；Some(版本号)=自该版本起弃用，下一 major 删除）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deprecated_since: Option<u32>,
}

/// 形态消费方。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum IoShapeConsumer {
    /// evo-agent AuditedLlm / console-cloud 浏览器审计桥
    LlmAuditBridge,
    /// evo-agent 本地工具执行器（宪法 io_request → call_service）
    AgentToolExecutor,
    /// bundle 层流程固化探针（evo-agent 不消费）
    FlowProbe,
}

/// v1 形状表——**skip 谓词与契约导出的共同真相源**。
///
/// 表条目与历史谓词语义逐一等价（server.rs 重构前 :85/:106/:125）：
/// - `llm_audit`      ⇔ is_llm_audit_request（2026-08-30 K 约束族）
/// - `agent_tool`     ⇔ is_agent_tool_request（2026-09-07）
/// - `flow_probe`     ⇔ is_flow_probe_request（探针形态改造版）
pub static IO_CONTRACT_V1_SHAPES: &[IoShapeSpec] = &[
    IoShapeSpec {
        shape: "llm_audit",
        io_type: "call_external",
        required_keys: &["messages"],
        forbidden_keys: &["service_name", "name"],
        consumer: IoShapeConsumer::LlmAuditBridge,
        semantics: "LLM 审计形态：留给外部执行者本地跑 LLM 并经审计链回写 io_response；内置订阅者抢先应答会使外部回写被 stale 拒绝，审计回路永久断链",
        deprecated_since: None,
    },
    IoShapeSpec {
        shape: "agent_tool",
        io_type: "call_service",
        required_keys: &["tool_name"],
        forbidden_keys: &["service_name", "name"],
        consumer: IoShapeConsumer::AgentToolExecutor,
        semantics: "消费方本地工具形态：留给 evo-agent 本地执行工具后回写 io_response；抢先应答 missing service_name 会消费 request 致工具循环断链",
        deprecated_since: None,
    },
    IoShapeSpec {
        shape: "flow_probe",
        io_type: "flow_probe",
        required_keys: &[],
        forbidden_keys: &[],
        consumer: IoShapeConsumer::FlowProbe,
        semantics: "bundle 自有探针形态：平台 io_type 发射权部署面独占，bundle 规则 io 往返正道；无内置 handler，外部经 POST /api/sessions/{id}/io_response 应答",
        deprecated_since: None,
    },
];

/// 表驱动形状匹配（skip 谓词的统一实现，替代三个手写 if 函数）。
///
/// 语义与历史谓词逐一等价：io_type 相等 ∧ required 全在场 ∧ forbidden 全缺席。
/// `flow_probe` 形态 required/forbidden 均空——纯 io_type 判定，与历史
/// `is_flow_probe_request` 一致。
pub fn matches_shape(spec: &IoShapeSpec, io_type: &IoType, params: &JsonValue) -> bool {
    if io_type.as_str() != spec.io_type {
        return false;
    }
    // JsonValue::Object 才有键可查；非对象参数在键存在性判定上等价于空表
    let obj = match params {
        JsonValue::Object(m) => m,
        _ => return spec.required_keys.is_empty(),
    };
    spec.required_keys.iter().all(|k| obj.contains_key(*k))
        && spec.forbidden_keys.iter().all(|k| !obj.contains_key(*k))
}

/// 按形态名做表驱动匹配（server.rs 历史谓词的统一委托入口，E-8 契约固化）
pub fn shape_matches(shape: &str, io_type: &IoType, params: &JsonValue) -> bool {
    IO_CONTRACT_V1_SHAPES
        .iter()
        .find(|s| s.shape == shape)
        .map(|spec| matches_shape(spec, io_type, params))
        .unwrap_or(false)
}

/// `GET /api/io-contract` 端点（免认证，与 /api/openapi.json 同款定位：
/// 仅暴露契约元数据）——evo-agent 启动协商的数据源（34 号档 §2.2）。
#[utoipa::path(
    get,
    path = "/api/io-contract",
    tag = "health",
    responses(
        (status = 200, description = "IO 形状契约 v1（skip 谓词真相源导出）", body = IoContractResponse)
    )
)]
pub async fn io_contract_endpoint() -> axum::Json<IoContractResponse> {
    axum::Json(io_contract_response())
}

/// 内置 IoSubscriber 的合并 skip 谓词（表驱动重实现，行为与历史
/// `is_external_executor_request` 逐一等价）。
pub fn is_external_executor_request(io_type: &IoType, params: &JsonValue) -> bool {
    IO_CONTRACT_V1_SHAPES
        .iter()
        .any(|spec| matches_shape(spec, io_type, params))
}

/// `GET /api/io-contract` 响应体。
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct IoContractResponse {
    /// 本端点导出的契约主版本
    pub contract_version: u32,
    /// 本进程支持的版本集（协商窗口）
    pub supported_versions: Vec<u32>,
    /// 形状规格全集（当前版本）
    pub shapes: Vec<IoShapeSpec>,
    /// 生成锚（真相源模块路径）
    pub generated_from: String,
}

/// 契约导出（单一入口：端点与快照测试共用）。
pub fn io_contract_response() -> IoContractResponse {
    IoContractResponse {
        contract_version: IO_CONTRACT_VERSION,
        supported_versions: IO_CONTRACT_SUPPORTED_VERSIONS.to_vec(),
        shapes: IO_CONTRACT_V1_SHAPES.to_vec(),
        generated_from: IO_CONTRACT_SOURCE_MODULE.to_string(),
    }
}

/// 稳定序列化（快照比对用：键序确定，不依赖 serde_json 特性）。
/// 序列化失败时回退空串——快照测试将立即失配暴露（非 panic 路径，
/// 符合 S1 门禁）。
pub fn io_contract_snapshot_json() -> String {
    serde_json::to_string_pretty(&io_contract_response()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// serde_json → tcb JsonValue 转换（测试自持，与 server.rs serde_to_tcb 同构）
    fn tcb(v: serde_json::Value) -> JsonValue {
        match v {
            serde_json::Value::Null => JsonValue::Null,
            serde_json::Value::Bool(b) => JsonValue::Bool(b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    JsonValue::Integer(i)
                } else {
                    JsonValue::String(n.to_string().into())
                }
            }
            serde_json::Value::String(s) => JsonValue::String(s.into()),
            serde_json::Value::Array(arr) => JsonValue::Array(arr.into_iter().map(tcb).collect()),
            serde_json::Value::Object(obj) => JsonValue::Object(
                obj.into_iter().map(|(k, val)| (k.into(), tcb(val))).collect(),
            ),
        }
    }

    fn ext() -> IoType {
        IoType::new("call_external")
    }
    fn svc() -> IoType {
        IoType::new("call_service")
    }
    fn probe() -> IoType {
        IoType::new("flow_probe")
    }

    // --- 与历史谓词的行为等价锁定（重构前后同输入同输出） ---

    #[test]
    fn test_llm_audit_shape_equivalence() {
        let audit = serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
        });
        assert!(matches_shape(&IO_CONTRACT_V1_SHAPES[0], &ext(), &tcb(audit.clone())));
        assert!(is_external_executor_request(&ext(), &tcb(audit)));

        // 有 service_name → 平台路由形态，不跳过
        let service = serde_json::json!({"service_name": "svc", "args": {}});
        assert!(!is_external_executor_request(&ext(), &tcb(service)));
        // 有 name → 别名形态，不跳过
        let named = serde_json::json!({"name": "svc", "args": {}});
        assert!(!is_external_executor_request(&ext(), &tcb(named)));
        // 无 messages → 不跳过
        let bare = serde_json::json!({"model": "m"});
        assert!(!is_external_executor_request(&ext(), &tcb(bare)));
    }

    #[test]
    fn test_agent_tool_shape_equivalence() {
        let tool = serde_json::json!({"tool_name": "file_write", "args": {}});
        assert!(matches_shape(&IO_CONTRACT_V1_SHAPES[1], &svc(), &tcb(tool.clone())));
        assert!(is_external_executor_request(&svc(), &tcb(tool)));

        // service_name/name 形态 → ServiceRegistryHandler 应答，不跳过
        let platform = serde_json::json!({"service_name": "solver", "args": {}});
        assert!(!is_external_executor_request(&svc(), &tcb(platform)));
        let named = serde_json::json!({"name": "solver", "args": {}});
        assert!(!is_external_executor_request(&svc(), &tcb(named)));
        // 无 tool_name → 不跳过
        let bare = serde_json::json!({"args": {}});
        assert!(!is_external_executor_request(&svc(), &tcb(bare)));
    }

    #[test]
    fn test_flow_probe_shape_equivalence() {
        assert!(is_external_executor_request(&probe(), &tcb(serde_json::json!({}))));
        // 纯 io_type 判定：参数形状无关（历史 is_flow_probe_request 同款）
        assert!(is_external_executor_request(
            &probe(),
            &tcb(serde_json::json!({"anything": 1}))
        ));
        // 其他 io_type 不受此形态影响
        assert!(!is_external_executor_request(
            &ext(),
            &tcb(serde_json::json!({"tool_name": "x"}))
        ));
    }

    // --- 契约导出完整性 ---

    #[test]
    fn test_contract_export_structure() {
        let resp = io_contract_response();
        assert_eq!(resp.contract_version, 1);
        assert_eq!(resp.supported_versions, vec![1]);
        assert_eq!(resp.shapes.len(), 3);
        // 表内 io_type+键集唯一性（防两形态意外重叠致 skip 歧义）
        let mut seen = std::collections::HashSet::new();
        for s in &resp.shapes {
            assert!(seen.insert((s.io_type, s.required_keys, s.forbidden_keys)),
                "duplicate shape signature: {}", s.shape);
        }
        // 消费方标注完备
        assert!(resp.shapes.iter().any(|s| s.shape == "llm_audit"
            && s.consumer == IoShapeConsumer::LlmAuditBridge));
        assert!(resp.shapes.iter().any(|s| s.shape == "agent_tool"
            && s.consumer == IoShapeConsumer::AgentToolExecutor));
    }

    // --- 快照锁定（改形状必红：bump 版本+更新快照=显式人为动作） ---

    #[test]
    fn test_io_contract_snapshot_pinned() {
        let pinned = include_str!("../../tests/io_contract_v1.snapshot.json");
        let live = io_contract_snapshot_json();
        assert_eq!(
            live.trim(), pinned.trim(),
            "io 契约与 pinned 快照漂移：若为有意形状变更，请 bump IO_CONTRACT_VERSION \
             并重新生成 tests/io_contract_v1.snapshot.json（34 号档版本演进规则）"
        );
    }
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! flow-studio 编译器 —— 声明式流程 JSON v0 → 内核规则草稿（契约 v1.1 §6）
//!
//! 编译核 [`compile_flow`] 是纯函数：同 flow 输入 → 字节级同草稿输出
//! （R1 确定性：零随机/零时钟/零 IO）。产物 transform type 集合 ⊆
//! {branch, io_request}（v0 语义）；上游 server 侧 R2 等价性门禁以
//! 6 元指令白名单强制兜底（不依赖本编译器自觉，契约 §6）。
//!
//! # 编译语义 v0（契约钉死，扩充 = 契约演进事件）
//!
//! - 线性链 start→…→end 按序展开为 transform 数组（数组序 = 执行序）；
//! - 审批节点（带 threshold）→ `branch(lt(form_ref_resolved.path, threshold),
//!   on_false=[io_request])`——低于阈值自动通过，达到阈值人工审批；
//! - 审批节点（无 threshold）→ `io_request(call_external)` 等待节点；
//! - guard="approved" 是审批出边的声明式契约标记（装载期校验），不单独编译
//!   为分支——审批结果的继续/终止语义由外部审批服务经既有 io 回写链处理。
//!
//! # 输入约定（R2 边界）
//!
//! 输入 flow 由 server 编译代理预解析：审批节点带 `form_ref_resolved`
//! {scene, field, path}——场景取值域锁定（R2）的唯一事实来源是 server 侧
//! pack.scene_fields 索引，本编译器只消费解析结果，**缺 form_ref_resolved
//! 即拒绝编译**（防御绕过场景锁定的直连调用）。
//!
//! # 审计与来源
//!
//! 产物 provenance 由 server 代理包裹（pack/flow/compiler 版本），本编译器
//! 不注入规则 JSON `_meta`（约束族 D：不触碰 _meta schema 权威面）。

use serde_json::{json, Value};

/// 单链最大步数防御（输入环由 visited 检测；此上限为第二道防线，
/// 与 evorule-tcb MAX_DOMAIN_DEPTH / MAX_TRANSFORM_RULES 同构的终止性防线）
const MAX_CHAIN_STEPS: usize = 1024;

/// 编译声明式流程 JSON → 内核规则草稿（契约 v1.1 §6；纯函数）。
///
/// # 输入（结构由 server 装载期 fail-fast 校验；此处做最小防御性校验）
///
/// ```json
/// {
///   "flow_id": "expense_approval_flow",
///   "description": "金额阈值审批流程",
///   "nodes": [
///     { "node_id": "n1", "node_type": "start" },
///     { "node_id": "n2", "node_type": "approval",
///       "params": { "role": "CFO", "prompt": "超阈值审批" },
///       "form_ref_resolved": { "scene": "expense", "field": "amount",
///                              "path": "__exec__.payload.amount" },
///       "threshold": 5000 },
///     { "node_id": "n3", "node_type": "end" }
///   ],
///   "edges": [ { "from": "n1", "to": "n2" },
///              { "from": "n2", "to": "n3", "guard": "approved" } ]
/// }
/// ```
///
/// # 输出
///
/// `{ "id", "version": 1, "description", "transform": [...] }`
/// （provenance 由 server 代理包裹）
///
/// # Errors
///
/// 结构缺失/类型不符/审批节点缺 `form_ref_resolved`（R2 防御）/
/// 环或断链/无审批节点——全部显式 Err，绝不静默跳过。
pub fn compile_flow(flow: &Value) -> Result<Value, String> {
    let obj = flow
        .as_object()
        .ok_or_else(|| "flow 顶层必须是 JSON object".to_string())?;
    let flow_id = obj
        .get("flow_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "flow 缺 flow_id（必须是非空字符串）".to_string())?;
    let description = obj
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or(flow_id)
        .to_string();
    let nodes = obj
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("flow {flow_id} 缺 nodes 数组"))?;
    if nodes.is_empty() {
        return Err(format!("flow {flow_id} nodes 不能为空"));
    }
    let edges = obj
        .get("edges")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("flow {flow_id} 缺 edges 数组"))?;

    // 节点索引与出边表（防御性重建；BTreeMap 保证遍历序确定）
    let mut node_by_id: std::collections::BTreeMap<&str, &Value> =
        std::collections::BTreeMap::new();
    let mut out: std::collections::BTreeMap<&str, &str> = std::collections::BTreeMap::new();
    let mut start_id: Option<&str> = None;
    for n in nodes {
        let id = n
            .get("node_id")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("flow {flow_id} 节点缺 node_id"))?;
        if node_by_id.insert(id, n).is_some() {
            return Err(format!("flow {flow_id} 节点 id 重复: '{id}'"));
        }
        if n.get("node_type").and_then(Value::as_str) == Some("start") {
            if start_id.is_some() {
                return Err(format!("flow {flow_id} 存在多个 start 节点"));
            }
            start_id = Some(id);
        }
    }
    for e in edges {
        let from = e
            .get("from")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("flow {flow_id} 边缺 from"))?;
        let to = e
            .get("to")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("flow {flow_id} 边缺 to"))?;
        if out.insert(from, to).is_some() {
            return Err(format!("flow {flow_id} 节点 {from} 有多条出边（v0 线性链）"));
        }
    }
    let start = start_id.ok_or_else(|| format!("flow {flow_id} 缺 start 节点"))?;

    // 走链收集审批节点（visited 防环 + 步数上限双防线）
    let mut approvals: Vec<&Value> = Vec::new();
    let mut visited: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    let mut cur = start;
    for _ in 0..MAX_CHAIN_STEPS {
        if !visited.insert(cur) {
            return Err(format!("flow {flow_id} 在节点 {cur} 检测到环（编译拒绝）"));
        }
        if let Some(n) = node_by_id.get(cur) {
            if n.get("node_type").and_then(Value::as_str) == Some("approval") {
                approvals.push(n);
            }
        }
        let Some(&next) = out.get(cur) else {
            break; // 到达终点（无出边）；断链由审批校验外的 server 装载兜底
        };
        cur = next;
    }
    if visited.len() >= MAX_CHAIN_STEPS {
        return Err(format!("flow {flow_id} 超过单链步数上限 {MAX_CHAIN_STEPS}"));
    }
    if approvals.is_empty() {
        return Err(format!(
            "flow {flow_id} 无审批节点 — v0 流程必须至少包含一个审批节点"
        ));
    }

    // 逐审批节点编译为 transform 步
    let mut transform: Vec<Value> = Vec::with_capacity(approvals.len());
    for a in &approvals {
        transform.push(compile_approval(flow_id, a)?);
    }
    Ok(json!({
        "id": flow_id,
        "version": 1,
        "description": description,
        "transform": transform,
    }))
}

/// 编译单个审批节点（纯函数）。
fn compile_approval(flow_id: &str, node: &Value) -> Result<Value, String> {
    let node_id = node
        .get("node_id")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let fail = |msg: &str| -> String { format!("flow {flow_id} 审批节点 {node_id}: {msg}") };
    let params = node
        .get("params")
        .and_then(Value::as_object)
        .ok_or_else(|| fail("缺 params object"))?;
    let role = params
        .get("role")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| fail("params.role 必须是非空字符串"))?;
    let prompt = params
        .get("prompt")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| fail("params.prompt 必须是非空字符串"))?;
    // R2 防御：缺 form_ref_resolved = 绕过 server 场景锁定 → 拒绝编译
    let resolved = node
        .get("form_ref_resolved")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            fail("缺 form_ref_resolved — form_ref 必须经 server 场景索引解析 \
                  （R2 取值域锁定;直连编译器不接受未解析引用）")
        })?;
    let path = resolved
        .get("path")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| fail("form_ref_resolved.path 必须是非空字符串（R2）"))?;
    let io_step = json!({
        "type": "io_request",
        "params": {
            "io_type": "call_external",
            "role": role,
            "prompt": prompt,
        }
    });
    match node.get("threshold") {
        Some(t) => {
            let threshold = t.as_i64().ok_or_else(|| {
                fail("threshold 必须是整数（domain lt 仅 i64,确定性）")
            })?;
            Ok(json!({
                "type": "branch",
                "params": {
                    "domain": { "type": "lt", "path": path, "value": threshold },
                    "on_false": [io_step],
                }
            }))
        }
        None => Ok(io_step),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use serde_json::json;

    /// 标准输入：1 审批节点（threshold）线性链（server 解析后的形态）
    fn resolved_flow() -> Value {
        json!({
            "flow_id": "expense_approval_flow",
            "description": "金额阈值审批流程",
            "version": 1,
            "nodes": [
                { "node_id": "n1", "node_type": "start" },
                { "node_id": "n2", "node_type": "approval",
                  "params": { "role": "CFO", "prompt": "超阈值审批" },
                  "form_ref": { "scene": "expense", "field": "amount" },
                  "form_ref_resolved": { "scene": "expense", "field": "amount",
                                         "path": "__exec__.payload.amount" },
                  "threshold": 5000 },
                { "node_id": "n3", "node_type": "end" }
            ],
            "edges": [
                { "from": "n1", "to": "n2" },
                { "from": "n2", "to": "n3", "guard": "approved" }
            ]
        })
    }

    #[test]
    fn threshold_approval_compiles_to_branch_lt_io_request() {
        let d = compile_flow(&resolved_flow()).unwrap();
        assert_eq!(d["id"], json!("expense_approval_flow"));
        assert_eq!(d["version"], json!(1));
        assert_eq!(d["description"], json!("金额阈值审批流程"));
        let t = &d["transform"];
        assert_eq!(t.as_array().map(Vec::len), Some(1));
        assert_eq!(t[0]["type"], json!("branch"));
        assert_eq!(
            t[0]["params"]["domain"],
            json!({ "type": "lt", "path": "__exec__.payload.amount", "value": 5000 })
        );
        assert_eq!(t[0]["params"]["on_false"][0]["type"], json!("io_request"));
        assert_eq!(
            t[0]["params"]["on_false"][0]["params"],
            json!({ "io_type": "call_external", "role": "CFO", "prompt": "超阈值审批" })
        );
    }

    #[test]
    fn plain_approval_compiles_to_io_request() {
        let mut f = resolved_flow();
        f["nodes"][1].as_object_mut().unwrap().remove("threshold");
        let d = compile_flow(&f).unwrap();
        let t = d["transform"].as_array().unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0]["type"], json!("io_request"));
        assert_eq!(t[0]["params"]["io_type"], json!("call_external"));
    }

    #[test]
    fn multi_approval_order_follows_chain_order() {
        let mut f = resolved_flow();
        let nodes = f["nodes"].as_array_mut().unwrap();
        nodes.insert(
            2,
            json!({ "node_id": "n2b", "node_type": "approval",
                    "params": { "role": "finance_manager", "prompt": "复核" },
                    "form_ref_resolved": { "scene": "expense", "field": "amount",
                                           "path": "__exec__.payload.amount" } }),
        );
        let edges = f["edges"].as_array_mut().unwrap();
        edges.insert(1, json!({ "from": "n2", "to": "n2b", "guard": "approved" }));
        edges[2] = json!({ "from": "n2b", "to": "n3", "guard": "approved" });
        let d = compile_flow(&f).unwrap();
        let t = d["transform"].as_array().unwrap();
        assert_eq!(t.len(), 2, "链序 n2(带阈值)→n2b(无阈值)");
        assert_eq!(t[0]["type"], json!("branch"));
        assert_eq!(t[0]["params"]["domain"]["value"], json!(5000));
        assert_eq!(t[1]["type"], json!("io_request"));
        assert_eq!(t[1]["params"]["role"], json!("finance_manager"));
    }

    #[test]
    fn compile_is_deterministic_byte_level() {
        let f = resolved_flow();
        let d1 = compile_flow(&f).unwrap();
        let d2 = compile_flow(&f).unwrap();
        assert_eq!(
            serde_json::to_string(&d1).unwrap(),
            serde_json::to_string(&d2).unwrap(),
            "R1: 同输入必须字节级同输出"
        );
    }

    #[test]
    fn rejects_flow_without_approval_node() {
        let mut f = resolved_flow();
        f["nodes"]
            .as_array_mut()
            .unwrap()
            .retain(|n| n["node_type"] != json!("approval"));
        let err = compile_flow(&f).unwrap_err();
        assert!(err.contains("无审批节点"), "got: {err}");
    }

    #[test]
    fn rejects_unresolved_form_ref_r2_defense() {
        let mut f = resolved_flow();
        f["nodes"][1]
            .as_object_mut()
            .unwrap()
            .remove("form_ref_resolved");
        let err = compile_flow(&f).unwrap_err();
        assert!(err.contains("form_ref_resolved") && err.contains("R2"), "got: {err}");
    }

    #[test]
    fn rejects_cycle_and_missing_params() {
        // 环（防御第二道防线）
        let mut f = resolved_flow();
        f["edges"][1] = json!({ "from": "n2", "to": "n1", "guard": "approved" });
        assert!(compile_flow(&f).is_err());
        // 缺 params
        let mut f2 = resolved_flow();
        f2["nodes"][1]
            .as_object_mut()
            .unwrap()
            .remove("params");
        let err = compile_flow(&f2).unwrap_err();
        assert!(err.contains("params"), "got: {err}");
        // threshold 浮点拒绝
        let mut f3 = resolved_flow();
        f3["nodes"][1]["threshold"] = json!(3.5);
        assert!(compile_flow(&f3).is_err());
    }
}

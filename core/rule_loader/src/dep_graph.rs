// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 常数依赖图物化（装载产物）：指令-常数引用边 + 量纲耦合边，随集发布（附哈希）。
//!
//! # 定位
//!
//! 从规则集 `metadata` 物化一张**确定性常数依赖图**：
//! - **指令节点**：`metadata.formula_registry` 登记条目（公式 → 量纲 + 可选常数引用清单）；
//! - **常数节点**：`metadata.constants` 声明清单（版本/值哈希/可选域界）；
//! - **引用边①**：登记条目 `constants` 数组逐 id → 该指令公式引用该常数；
//! - **耦合边②**：任意两指令存在轴 i 使 dims_a[i]==dims_b[i] 且 !=0 → 一条耦合边
//!   （同轴同非零幂次 = 该轴物理量同类，同维可比——量纲耦合层间传播的定义依据）。
//!
//! 图是**装载产物**：随装载结果发布（附 blake3 内容哈希），供常数修订冲击传播等
//! 下游确定性分析消费；本模块独立校验自身输入面（登记了引用则常数必须已声明，
//! 「谁用谁登记」同律），与量纲登记门（被引用立法指令必须有登记）分工不耦合。
//!
//! # 输入面（全部可选）
//!
//! - `metadata.constants`（数组，可缺）：每条 {constant_id/version/value_hash 非空串,
//!   domain_lo/domain_hi 数字可选}；constant_id 重复拒；
//! - `metadata.formula_registry`（数组，可缺）：既有校验字段照旧
//!   （instruction_type/formula/dimensions），新可选字段 `constants`（字符串数组）。
//!
//! 规则集不携带任何登记 = 空图（Ok）——登记为增量物化，非强制全员登记。
//!
//! # 确定性
//!
//! 全部节点/边按字典序排序；digest 对不含 digest 的四字段规范化序列化计算
//! （serde_json 紧凑字节，BTreeMap 键序），经 evorule-hash 单一事实源——
//! 同输入两次构建字节与 digest 全等。

use serde::Serialize;
use serde_json::Value;

/// 七分量量纲向量长度（[L,M,T,I,Θ,N,J]，与 lib.rs 量纲登记门同一口径）
const DIMENSION_VECTOR_LEN: usize = 7;

// ============================================================================
// 数据结构（字段确定性排序；全部可序列化——图随集发布供下游消费）
// ============================================================================

/// 指令节点（formula_registry 登记条目的图内形态）
#[derive(Serialize, Debug, Clone)]
pub struct InstructionNode {
    /// 指令类型（图内排序键）
    pub instruction_type: String,
    /// 被裁决量的公式（登记原文）
    pub formula: String,
    /// 七分量量纲幂次 [L,M,T,I,Θ,N,J]
    pub dimensions: [i64; 7],
    /// 该指令公式引用的常数 id 清单（登记声明序；未登记 = 空）
    pub constants: Vec<String>,
}

/// 常数节点（metadata.constants 声明条目的图内形态）
#[derive(Serialize, Debug, Clone)]
pub struct ConstantNode {
    /// 常数 id（图内排序键，全局唯一）
    pub constant_id: String,
    /// 版本戳（如常数表年份）
    pub version: String,
    /// 值哈希（常数取值的指纹，修订对账用）
    pub value_hash: String,
    /// 域下界（可选，闭区间）
    pub domain_lo: Option<f64>,
    /// 域上界（可选，闭区间）
    pub domain_hi: Option<f64>,
}

/// 引用边：指令公式 → 常数（「谁用谁登记」的边形态）
#[derive(Serialize, Debug, Clone)]
pub struct ReferenceEdge {
    /// 指令类型
    pub instruction: String,
    /// 被引用常数 id
    pub constant: String,
}

/// 量纲耦合边：两指令存在同轴同非零幂次（a < b 字典序）
#[derive(Serialize, Debug, Clone)]
pub struct CouplingEdge {
    /// 端点 a（字典序较小）
    pub a: String,
    /// 端点 b（字典序较大）
    pub b: String,
    /// 共享轴位（升序，0..7 对应 [L,M,T,I,Θ,N,J]）
    pub shared_axes: Vec<usize>,
}

/// 常数依赖图（装载产物，随集发布）。
///
/// 字段序 = digest 载荷序：digest 对前四字段（不含 digest 本身）的规范化
/// 序列化字节计算，`blake3:` 前缀自描述形态。
#[derive(Serialize, Debug, Clone)]
pub struct DependencyGraph {
    /// 指令节点（按 instruction_type 字典序）
    pub instructions: Vec<InstructionNode>,
    /// 常数节点（按 constant_id 字典序）
    pub constants: Vec<ConstantNode>,
    /// 引用边（按 (instruction, constant) 字典序，去重）
    pub reference_edges: Vec<ReferenceEdge>,
    /// 量纲耦合边（按 (a, b) 字典序）
    pub coupling_edges: Vec<CouplingEdge>,
    /// 内容哈希（"blake3:hex"，对前四字段规范化序列化计算）
    pub digest: String,
}

/// digest 载荷（不含 digest 字段本身——对图内容计算，发布时随图携带）
#[derive(Serialize)]
struct GraphDigestPayload<'a> {
    instructions: &'a [InstructionNode],
    constants: &'a [ConstantNode],
    reference_edges: &'a [ReferenceEdge],
    coupling_edges: &'a [CouplingEdge],
}

// ============================================================================
// 错误（中文诊断：违什么 + 在哪 JSON 路径；零 panic——取值全走显式分支）
// ============================================================================

/// 依赖图构建错误（违什么 + 在哪；Display 供服务层透传）
#[derive(Debug, Clone, Serialize)]
pub struct DependencyGraphError {
    /// 违什么
    pub violation: String,
    /// 在哪（JSON 路径）
    pub location: String,
}

impl std::fmt::Display for DependencyGraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[依赖图] {}（位置：{}）", self.violation, self.location)
    }
}

impl std::error::Error for DependencyGraphError {}

fn graph_err(violation: impl Into<String>, location: impl Into<String>) -> DependencyGraphError {
    DependencyGraphError {
        violation: violation.into(),
        location: location.into(),
    }
}

// ============================================================================
// 构建函数（同输入两次构建字节与 digest 全等）
// ============================================================================

/// 从规则集文档物化常数依赖图（输入面全部可选，零登记 = 空图 Ok）。
///
/// 本函数独立校验自身输入面（常数声明合法性 / 引用已声明性 / 登记条目结构），
/// 不与量纲登记门耦合——门管「被引用立法指令必须有登记」，本模块管
/// 「登记了引用则常数必须已声明」。
pub fn build_dependency_graph(doc: &Value) -> Result<DependencyGraph, DependencyGraphError> {
    // 常数声明清单（可缺）：id 唯一性 + 条目结构校验
    let (constants, declared_ids) = parse_constants(doc)?;

    // 登记清单（可缺）：条目结构校验 + 引用已声明性（谁用谁登记同律）
    let (instructions, reference_pairs) = parse_registry(doc, &declared_ids)?;

    // 量纲耦合边：任意两指令同轴同非零幂次（instructions 已按字典序，a<b 天然成立）
    let coupling_edges = build_coupling_edges(&instructions);

    // 引用边：集合形态天然去重 + 字典序
    let reference_edges: Vec<ReferenceEdge> = reference_pairs
        .into_iter()
        .map(|(instruction, constant)| ReferenceEdge {
            instruction,
            constant,
        })
        .collect();

    // digest：对前四字段（不含 digest 本身）的规范化序列化字节计算，
    // "blake3:" 前缀自描述形态（与仓内 provenance content_hash 先例一致）
    let payload = GraphDigestPayload {
        instructions: &instructions,
        constants: &constants,
        reference_edges: &reference_edges,
        coupling_edges: &coupling_edges,
    };
    let hex = evorule_hash::try_json_digest(&payload)
        .map_err(|e| graph_err(format!("依赖图 digest 序列化失败: {e}"), "$.metadata"))?;
    Ok(DependencyGraph {
        instructions,
        constants,
        reference_edges,
        coupling_edges,
        digest: evorule_hash::prefixed(&hex),
    })
}

/// 解析 metadata.constants（可缺）：常数节点（按 constant_id 字典序）+ 已声明 id 集。
fn parse_constants(
    doc: &Value,
) -> Result<(Vec<ConstantNode>, std::collections::BTreeSet<String>), DependencyGraphError> {
    let mut out = Vec::new();
    let mut declared = std::collections::BTreeSet::new();
    let entries = match doc.pointer("/metadata/constants") {
        None => return Ok((out, declared)),
        Some(Value::Array(entries)) => entries,
        Some(_) => {
            return Err(graph_err(
                "metadata.constants 须为数组",
                "$.metadata.constants",
            ))
        }
    };
    for (i, entry) in entries.iter().enumerate() {
        let loc = format!("$.metadata.constants[{i}]");
        let obj = entry
            .as_object()
            .ok_or_else(|| graph_err("常数声明条目须为对象", &loc))?;
        let constant_id = non_empty_str(obj, "constant_id", &loc)?;
        let version = non_empty_str(obj, "version", &loc)?;
        let value_hash = non_empty_str(obj, "value_hash", &loc)?;
        let domain_lo = optional_domain(obj, "domain_lo", &loc)?;
        let domain_hi = optional_domain(obj, "domain_hi", &loc)?;
        if !declared.insert(constant_id.clone()) {
            return Err(graph_err(
                format!("常数 id \"{constant_id}\" 重复声明（声明须唯一）"),
                &loc,
            ));
        }
        out.push(ConstantNode {
            constant_id,
            version,
            value_hash,
            domain_lo,
            domain_hi,
        });
    }
    out.sort_by(|a, b| a.constant_id.cmp(&b.constant_id));
    Ok((out, declared))
}

/// 登记解析产物：指令节点（instruction_type 字典序）+ 引用边对
/// （(instruction, constant) 字典序，集合形态天然去重）
type RegistryParse = (
    Vec<InstructionNode>,
    std::collections::BTreeSet<(String, String)>,
);

/// 解析 metadata.formula_registry（可缺）：指令节点（按 instruction_type 字典序）
/// + 引用边对（(instruction, constant) 字典序，集合形态去重）。
fn parse_registry(
    doc: &Value,
    declared_ids: &std::collections::BTreeSet<String>,
) -> Result<RegistryParse, DependencyGraphError> {
    let mut out = Vec::new();
    let mut pairs = std::collections::BTreeSet::new();
    let entries = match doc.pointer("/metadata/formula_registry") {
        None => {
            // 登记缺失但常数声明存在 → 常数节点照常入图（无指令/边）
            return Ok((out, pairs));
        }
        Some(Value::Array(entries)) => entries,
        Some(_) => {
            return Err(graph_err(
                "metadata.formula_registry 须为数组",
                "$.metadata.formula_registry",
            ))
        }
    };
    let mut seen_types = std::collections::BTreeSet::new();
    for (i, entry) in entries.iter().enumerate() {
        let loc = format!("$.metadata.formula_registry[{i}]");
        let obj = entry
            .as_object()
            .ok_or_else(|| graph_err("登记条目须为对象", &loc))?;
        let instruction_type = non_empty_str(obj, "instruction_type", &loc)?;
        let formula = non_empty_str(obj, "formula", &loc)?;
        let dims = parse_dimensions(obj, &loc)?;
        if !seen_types.insert(instruction_type.clone()) {
            return Err(graph_err(
                format!("instruction_type \"{instruction_type}\" 重复登记（登记须唯一）"),
                &loc,
            ));
        }
        // 可选常数引用清单：逐 id 校验已声明性（谁用谁登记同律）
        let mut referenced = Vec::new();
        match obj.get("constants") {
            None | Some(Value::Null) => {}
            Some(Value::Array(ids)) => {
                for (k, id) in ids.iter().enumerate() {
                    let id = id.as_str().ok_or_else(|| {
                        graph_err(
                            format!("constants[{k}] 须为字符串（常数 id）"),
                            format!("{loc}.constants[{k}]"),
                        )
                    })?;
                    if !declared_ids.contains(id) {
                        return Err(graph_err(
                            format!(
                                "登记引用常数 \"{id}\"，但 metadata.constants 未声明该 id \
                                （谁用谁登记：引用的常数必须先声明）"
                            ),
                            format!("{loc}.constants[{k}]"),
                        ));
                    }
                    referenced.push(id.to_string());
                }
            }
            Some(_) => {
                return Err(graph_err(
                    "constants 须为字符串数组（该指令公式引用的常数 id 清单）",
                    format!("{loc}.constants"),
                ));
            }
        }
        for id in &referenced {
            pairs.insert((instruction_type.clone(), id.clone()));
        }
        out.push(InstructionNode {
            instruction_type,
            formula,
            dimensions: dims,
            constants: referenced,
        });
    }
    out.sort_by(|a, b| a.instruction_type.cmp(&b.instruction_type));
    Ok((out, pairs))
}

/// 解析七分量量纲幂次向量（恰好 7 元素、逐位整数）。
fn parse_dimensions(
    obj: &serde_json::Map<String, Value>,
    loc: &str,
) -> Result<[i64; 7], DependencyGraphError> {
    let dims = obj
        .get("dimensions")
        .and_then(Value::as_array)
        .ok_or_else(|| graph_err("dimensions 缺失或非数组", loc))?;
    if dims.len() != DIMENSION_VECTOR_LEN {
        return Err(graph_err(
            format!(
                "dimensions 须为恰好 {DIMENSION_VECTOR_LEN} 元素的 [L,M,T,I,Θ,N,J] 幂次向量（实际 {} 元素）",
                dims.len()
            ),
            loc,
        ));
    }
    let mut out = [0i64; DIMENSION_VECTOR_LEN];
    for (k, d) in dims.iter().enumerate() {
        if !d.is_i64() {
            return Err(graph_err(
                format!("dimensions[{k}] 非整数（七分量幂次须为整数）"),
                loc,
            ));
        }
        out[k] = d.as_i64().unwrap_or_default();
    }
    Ok(out)
}

/// 量纲耦合边：两两配对，同轴同非零幂次即耦合（shared_axes 升序）。
fn build_coupling_edges(instructions: &[InstructionNode]) -> Vec<CouplingEdge> {
    let mut edges = Vec::new();
    for (i, a) in instructions.iter().enumerate() {
        for b in &instructions[i + 1..] {
            let shared: Vec<usize> = (0..DIMENSION_VECTOR_LEN)
                .filter(|&axis| a.dimensions[axis] == b.dimensions[axis] && a.dimensions[axis] != 0)
                .collect();
            if !shared.is_empty() {
                edges.push(CouplingEdge {
                    a: a.instruction_type.clone(),
                    b: b.instruction_type.clone(),
                    shared_axes: shared,
                });
            }
        }
    }
    edges.sort_by(|x, y| (&x.a, &x.b).cmp(&(&y.a, &y.b)));
    edges
}

/// 取非空字符串字段（缺失 / 非字符串 / 空串 → 错误，位置含字段名）。
fn non_empty_str(
    obj: &serde_json::Map<String, Value>,
    key: &str,
    loc: &str,
) -> Result<String, DependencyGraphError> {
    let field_loc = format!("{loc}.{key}");
    let s = obj
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| graph_err(format!("{key} 缺失或非字符串"), &field_loc))?;
    if s.is_empty() {
        return Err(graph_err(format!("{key} 为空串（须非空）"), &field_loc));
    }
    Ok(s.to_string())
}

/// 取可选数字域界字段（缺省 None；存在但非数字 → 错误）。
fn optional_domain(
    obj: &serde_json::Map<String, Value>,
    key: &str,
    loc: &str,
) -> Result<Option<f64>, DependencyGraphError> {
    let field_loc = format!("{loc}.{key}");
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => Ok(n.as_f64()),
        Some(_) => Err(graph_err(
            format!("{key} 须为数字（闭区间域界）"),
            &field_loc,
        )),
    }
}

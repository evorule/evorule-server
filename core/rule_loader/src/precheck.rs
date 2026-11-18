// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 装载预检（观测面）：装载即出账，规则集包络静态可核。
//!
//! # 定位（观测面，非执法面）
//!
//! 每次装载产出机器可读预检报告：规则数/上限比、io_request 计数、全树 `type`
//! 分布全部静态可复算。职责分层：
//!
//! - **预检（本模块）只外显不执法**：`precheck_warning` 在规则数逼近上限时提示
//!   立法面规划扩容；对预检结果不做任何装载决策（拒绝/放行不变）。
//! - **执法权在内核既有闸**：规则数超限时由内核 `TooManyTransformRules`
//!   fail-closed 拒执行，预检不重复报错、不拦截、不吞错。
//! - **运行期量不入预检**：branch 深度 / META 指令预算是运行期量，归内核既有闸
//!   （`MAX_BRANCH_DEPTH` / `MAX_TOTAL_META_INSTRUCTIONS`），预检不估算不越权。
//!
//! # 真值单源
//!
//! 上限真值 = `evorule_tcb::MAX_TRANSFORM_RULES`（pub 常量），本模块禁止复制
//! 字面 64；内核包络参数化（TCB 包络方向）施工后本报告自动跟随，零返工。
//!
//! # `domain_histogram` 词表
//!
//! 全树 `type` 字段**通用计数**（`BTreeMap` 键序确定性）：顶层规则型（`branch`）、
//! 域谓词型（`eq`/`lt`/`in`/`exists`/`instruction`/`all`/`not`/`has_fields`）、
//! 指令元型（`set`/`push`/`noop`/`io_request` 等）同词表统一入账——新增类型
//! 自动入账，零代码变更；`io_request_count` 即直方图中 `io_request` 键的计数
//! （叶子约束审计面：每条 transform 规则最多一个 io_request 元指令）。

use std::collections::BTreeMap;

use evorule_tcb::JsonValue;
use serde::Serialize;

/// WARN 阈值（千分比）。首版常量；如需参数化，归内核包络参数面统一治理。
pub const WARN_THRESHOLD_PERMILLE: u32 = 800;

/// 装载预检报告（additive 观测字段；未消费时行为逐字节不变）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrecheckReport {
    /// 本次装载的 transform 规则数
    pub transform_rules_used: u32,
    /// transform 规则上限（读自 `evorule_tcb` 常量，非本 crate 字面值）
    pub transform_rules_limit: u32,
    /// 规则数/上限比，整数千分比（`used*1000/limit` 向下取整；禁浮点，
    /// 对齐确定性口径）
    pub utilization_permille: u32,
    /// 规则集中 `io_request` 元指令出现数（叶子约束审计面）
    pub io_request_count: u32,
    /// 全树 `type` 字段分布（词表见模块文档；`BTreeMap` 键序确定性）
    pub domain_histogram: BTreeMap<String, u32>,
    /// ≥800‰ 时提示立法面规划扩容；仅外显，不影响装载结果
    pub precheck_warning: Option<String>,
}

/// 从剥壳后的引擎规则列表（TCB 域）构建装载预检报告。
///
/// 纯函数、零副作用、确定性（相同输入 → 相同报告）；只读规则树，
/// 不修改任何输入，不影响装载判定。
pub fn build_precheck(rules: &[JsonValue]) -> PrecheckReport {
    let used = rules.len();
    let limit = evorule_tcb::MAX_TRANSFORM_RULES;

    let mut histogram: BTreeMap<String, u32> = BTreeMap::new();
    let mut io_request_count: u32 = 0;
    for rule in rules {
        walk_type_nodes(rule, &mut histogram, &mut io_request_count);
    }

    // u64 中间量防溢出（usize 平台上 used*1000 理论可达溢出界）；
    // 整数除法向下取整，禁浮点。
    let permille = ((used as u64).saturating_mul(1000) / limit as u64) as u32;

    let precheck_warning = if permille >= WARN_THRESHOLD_PERMILLE {
        Some(format!(
            "rules nearing transform limit ({used}/{limit}, {permille}\u{2030})"
        ))
    } else {
        None
    };

    PrecheckReport {
        transform_rules_used: used as u32,
        transform_rules_limit: limit as u32,
        utilization_permille: permille,
        io_request_count,
        domain_histogram: histogram,
        precheck_warning,
    }
}

/// 递归收集整棵规则树中所有对象的 `type` 字段（字符串形态）计数。
fn walk_type_nodes(
    v: &JsonValue,
    histogram: &mut BTreeMap<String, u32>,
    io_request_count: &mut u32,
) {
    match v {
        JsonValue::Array(items) => {
            for it in items {
                walk_type_nodes(it, histogram, io_request_count);
            }
        }
        JsonValue::Object(map) => {
            if let Some(t) = map.get("type").and_then(JsonValue::as_str) {
                *histogram.entry(t.to_string()).or_insert(0) += 1;
                if t == "io_request" {
                    *io_request_count += 1;
                }
            }
            for child in map.values() {
                walk_type_nodes(child, histogram, io_request_count);
            }
        }
        _ => {}
    }
}

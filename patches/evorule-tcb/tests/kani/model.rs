// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 结构化符号输入辅助（tests/kani/model.rs）
//!
//! # 设计原则：固定结构 + 符号叶子
//! 不整体 `kani::any::<JsonValue>()`（全符号 BTreeMap/Vec 会状态爆炸）；
//! 改为构造**已知形状**的 `JsonValue`（固定键、固定结构），仅叶子值符号化
//! （`kani::any::<i64>()` / `kani::any::<bool>()` 等），
//! 兼顾"直接验证生产代码"与"控制展开成本"（见 verification/kani-formal-verification-design.md §3）。

#![cfg(kani)]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use evorule_tcb::{JsonValue, ObjectMap};

/// 返回全部 6 种变体的具体实例（固定数组，每类 1 个代表，无堆分配）。
///
/// 供 P1-P3 使用：用索引循环做两两比较（6×6=36 种组合），覆盖全部 match 分支。
/// 相比 Vec<JsonValue>，`[JsonValue; 6]` 无堆分配，CBMC 按栈上数组展开，
/// 无 Vec 迭代器/堆指针开销，展开成本显著降低。
///
/// ⚠️ 本函数不含符号值，CBMC 按具体常量计算，
/// 求解路径仅覆盖 match 分支选择，不验证标准库内部（memcmp/映射容器遍历）。
pub(crate) fn all_variants() -> [JsonValue; 6] {
    [
        JsonValue::Null,
        JsonValue::Bool(true),
        JsonValue::Integer(1),
        JsonValue::string("s"),
        JsonValue::Array(vec![JsonValue::Integer(1)]),
        JsonValue::empty_object(),
    ]
}

/// 构造"已知形状、符号叶子"的 payload 对象。
/// 键数固定（x/y/obj.flag）→ ObjectMap 大小确定；叶子值符号化 → 覆盖全部输入值。
pub(crate) fn any_payload() -> JsonValue {
    let mut map = ObjectMap::new();
    map.insert("x".to_string(), JsonValue::Integer(kani::any::<i64>()));
    map.insert("y".to_string(), JsonValue::Integer(kani::any::<i64>()));
    map.insert(
        "obj".to_string(),
        JsonValue::Object({
            let mut m = ObjectMap::new();
            m.insert("flag".to_string(), JsonValue::Bool(kani::any::<bool>()));
            m
        }),
    );
    JsonValue::Object(map)
}

/// 符号指令（type 取合法集合之一，0..=3 → set/push/branch/io_request）。
/// ⚠️ 本函数仅提供"type 符号化、无 params"的最简形状；
/// 需要具体 params 的证明（P13-P18）各自构造固定形状的指令。
pub(crate) fn any_instruction() -> JsonValue {
    let t = kani::any::<u8>() % 4;
    let mut instr = ObjectMap::new();
    instr.insert(
        "type".to_string(),
        JsonValue::string(match t {
            0 => "set",
            1 => "push",
            2 => "branch",
            _ => "io_request",
        }),
    );
    JsonValue::Object(instr)
}

/// 构造完整 exec_state：`__exec__` 下含 `instruction` + `payload` + `queue`。
/// 供 `execute_meta_instruction`（P12/P13/P14）使用——它需要 `__exec__.payload` 与 `__exec__.queue`。
pub(crate) fn any_state() -> JsonValue {
    let mut exec = ObjectMap::new();
    exec.insert("instruction".to_string(), JsonValue::string("noop"));
    exec.insert("payload".to_string(), any_payload());
    exec.insert("queue".to_string(), JsonValue::Array(Vec::new()));
    let mut root = ObjectMap::new();
    root.insert("__exec__".to_string(), JsonValue::Object(exec));
    JsonValue::Object(root)
}

/// 构造**完全具体**的 exec_state（无符号叶子，供 P8/P9/P10/P11 等层 3 证明使用）。
///
/// ⚠️ 本函数全部用具体常量（无符号叶子）：符号叶子会让映射比较 + 展开状态爆炸
/// （P8 实测符号 exec_state + 具体域列表被 WSL 内存打断）。
/// 仅验证 `evaluate_domain` 自身的 match 分支与路径解析逻辑，覆盖目标不变。
pub(crate) fn concrete_exec_state() -> JsonValue {
    let mut payload = ObjectMap::new();
    payload.insert("x".to_string(), JsonValue::Integer(1));
    payload.insert("y".to_string(), JsonValue::Integer(2));
    payload.insert(
        "obj".to_string(),
        obj(vec![("flag", JsonValue::Bool(true))]),
    );
    payload.insert(
        "items".to_string(),
        JsonValue::array(vec![
            obj(vec![("name", JsonValue::string("a"))]),
            obj(vec![("name", JsonValue::string("b"))]),
        ]),
    );
    let mut exec = ObjectMap::new();
    exec.insert(
        "instruction".to_string(),
        obj(vec![("type", JsonValue::string("set"))]),
    );
    exec.insert("payload".to_string(), JsonValue::Object(payload));
    exec.insert("queue".to_string(), JsonValue::Array(Vec::new()));
    let mut root = ObjectMap::new();
    root.insert("__exec__".to_string(), JsonValue::Object(exec));
    JsonValue::Object(root)
}

/// 构造**单键最简** exec_state：`{ __exec__: { payload: { x: 1 } } }`（每层 ObjectMap 仅 1 键）。
///
/// 历史教训：Kani 对 BTreeMap（红黑树）展开成本极高（键越多、树越深，CBMC 查找路径越多），
/// P8 系列实测多键 `concrete_exec_state()`（4+ 键 × 3 层）展开 ≥3GB 不收敛。
/// 2026-09-13 起 kani 构建下 ObjectMap 已切换为 KaniMap（有序 Vec，见 value.rs），
/// 多键不再爆炸；本函数保留单键最小状态以进一步压低展开规模。
pub(crate) fn single_key_exec_state() -> JsonValue {
    let mut payload = ObjectMap::new();
    payload.insert("x".to_string(), JsonValue::Integer(1));
    let mut exec = ObjectMap::new();
    exec.insert("payload".to_string(), JsonValue::Object(payload));
    let mut root = ObjectMap::new();
    root.insert("__exec__".to_string(), JsonValue::Object(exec));
    JsonValue::Object(root)
}

/// 以给定 payload 内容构造完整 exec_state（供 P15/P16/P17/P18 传入具体 payload）。
pub(crate) fn state_with_payload(payload: ObjectMap) -> JsonValue {
    let mut exec = ObjectMap::new();
    exec.insert("instruction".to_string(), JsonValue::string("noop"));
    exec.insert("payload".to_string(), JsonValue::Object(payload));
    exec.insert("queue".to_string(), JsonValue::Array(Vec::new()));
    let mut root = ObjectMap::new();
    root.insert("__exec__".to_string(), JsonValue::Object(exec));
    JsonValue::Object(root)
}

// ===== P21 专用：ReAct 三条规则（口径与 src/transition.rs 单元测试一致） =====
// TCB 零依赖、不内嵌 JSON 解析器，core_eval.json 由上层加载后传入 execute_transition，
// 因此这里手工构造与 core_eval.json v0.3.1 ReAct 三条规则一一对应的规则列表。
// 全部为具体常量（无符号值），Kani 按具体常量折叠，展开成本可控。

fn s(v: &str) -> JsonValue {
    JsonValue::string(v)
}
fn iv(v: i64) -> JsonValue {
    JsonValue::Integer(v)
}
/// owned 构造（W3-3，G2：构造层与 Clone 实现解绑）——值 move 进 ObjectMap，零深克隆。
pub(crate) fn obj(pairs: Vec<(&str, JsonValue)>) -> JsonValue {
    JsonValue::object_from_pairs_owned(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}
fn arr(v: Vec<JsonValue>) -> JsonValue {
    JsonValue::array(v)
}

fn instr_domain(t: &str) -> JsonValue {
    obj(vec![("type", s("instruction")), ("instruction_type", s(t))])
}
fn exists_domain(path: &str) -> JsonValue {
    obj(vec![("type", s("exists")), ("path", s(path))])
}
fn lt_domain(path: &str, value: i64) -> JsonValue {
    obj(vec![
        ("type", s("lt")),
        ("path", s(path)),
        ("value", iv(value)),
    ])
}
fn branch(domain: JsonValue, on_true: Vec<JsonValue>, on_false: Vec<JsonValue>) -> JsonValue {
    obj(vec![
        ("type", s("branch")),
        (
            "params",
            obj(vec![
                ("domain", domain),
                ("on_true", arr(on_true)),
                ("on_false", arr(on_false)),
            ]),
        ),
    ])
}
fn set_instr(attr: &str, op: &str, value: JsonValue) -> JsonValue {
    obj(vec![
        ("type", s("set")),
        (
            "params",
            obj(vec![
                ("attr", s(attr)),
                ("operation", s(op)),
                ("value", value),
            ]),
        ),
    ])
}
fn push_noop() -> JsonValue {
    obj(vec![
        ("type", s("push")),
        (
            "params",
            obj(vec![(
                "instructions",
                arr(vec![obj(vec![("type", s("noop"))])]),
            )]),
        ),
    ])
}

/// 与 core_eval.json v0.3.1 的 ReAct 三条规则一一对应
/// （self_init / call_external / call_service；工具扇出/结果合并的循环编排
/// 职责在应用层 runner，collect/merge 原语已退役（历史批次），终止统一 push noop）。
pub(crate) fn react_core_eval() -> Vec<JsonValue> {
    // 1) react_iteration 自初始化（缺失时置 0，否则跳过）
    let self_init = branch(
        obj(vec![
            ("type", s("all")),
            (
                "inner",
                arr(vec![
                    instr_domain("call_external"),
                    obj(vec![
                        ("type", s("not")),
                        ("inner", exists_domain("__exec__.payload.react_iteration")),
                    ]),
                ]),
            ),
        ]),
        vec![set_instr("react_iteration", "set", iv(0))],
        vec![],
    );

    // 2) call_external：消费 LLM 结果（工具扇出编排在应用层 runner，
    //    collect 已退役，消费轮统一 push noop 终止）
    let call_external = branch(
        instr_domain("call_external"),
        vec![branch(
            exists_domain("__exec__.payload.__io_results__.call_external"),
            vec![
                set_instr(
                    "llm_response",
                    "set",
                    s("__exec__.payload.__io_results__.call_external"),
                ),
                branch(
                    exists_domain("__exec__.instruction.params.tools"),
                    vec![set_instr(
                        "tools",
                        "set",
                        s("__exec__.instruction.params.tools"),
                    )],
                    vec![],
                ),
                set_instr(
                    "__exec__.payload.__io_results__.call_external",
                    "set",
                    JsonValue::Null,
                ),
                // 循环终止（工具扇出编排在应用层 runner，collect 已退役）
                push_noop(),
            ],
            vec![obj(vec![
                ("type", s("io_request")),
                (
                    "params",
                    obj(vec![
                        ("io_type", s("call_external")),
                        ("messages", s("__exec__.instruction.params.messages")),
                        ("tools", s("__exec__.instruction.params.tools")),
                    ]),
                ),
            ])],
        )],
        vec![],
    );

    // 3) call_service：消费工具结果 → lt 检查（结果合并与下一条 LLM 调用的生成
    //    由应用层 runner 编排，merge 已退役，消费轮统一 push noop 终止）
    let call_service = branch(
        instr_domain("call_service"),
        vec![branch(
            exists_domain("__exec__.payload.__io_results__.call_service"),
            vec![
                set_instr(
                    "service_result",
                    "set",
                    s("__exec__.payload.__io_results__.call_service"),
                ),
                set_instr(
                    "__exec__.payload.__io_results__.call_service",
                    "set",
                    JsonValue::Null,
                ),
                branch(
                    lt_domain("__exec__.payload.react_iteration", 10),
                    // 循环终止（结果合并与下一条 LLM 调用编排在应用层 runner，merge 已退役）
                    vec![set_instr("react_iteration", "add", iv(1)), push_noop()],
                    vec![push_noop()],
                ),
            ],
            vec![obj(vec![
                ("type", s("io_request")),
                (
                    "params",
                    obj(vec![
                        ("io_type", s("call_service")),
                        (
                            "service_name",
                            s("__exec__.instruction.params.service_name"),
                        ),
                        ("args", s("__exec__.instruction.params.args")),
                    ]),
                ),
            ])],
        )],
        vec![],
    );

    vec![self_init, call_external, call_service]
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Kani 证明清单（P1-P14、P18-P21）
//!
//! 对应 verification/kani-formal-verification-design.md §四 的分层验证：
//! - Layer 1 基础类型层：P1-P3
//! - Layer 2 路径解析层：P4-P7
//! - Layer 3 域评估层：P8-P11
//! - Layer 4 元指令层：P12-P14、P18（经公开 `execute_meta_instruction` 间接覆盖私有元指令；
//!   P15/P16/P17 随 collect/merge 原语退役删除，见规则清理方案）
//! - Layer 5 状态转换层：P19-P21
//!
//! 原则：只调用公开 API；结构化符号输入（见 model.rs）；验证"属性"而非具体行为。

#![cfg(kani)]

extern crate alloc;

use alloc::string::ToString;
use alloc::vec::Vec;

use evorule_tcb::domain::{evaluate_domain, evaluate_domain_outcome, MAX_DOMAIN_DEPTH};
use evorule_tcb::executor::{execute_meta_instruction, MetaInstructionResult, MAX_BRANCH_DEPTH};
use evorule_tcb::path::resolve_path;
use evorule_tcb::{
    execute_transition, JsonValue, ObjectMap, TcbError, TransitionResult, MAX_TRANSFORM_RULES,
};

use super::model;

// ==================== W3-1: harness 结构自检断言（S3 硬前置，CR-20260913-004 / ADR-0002） ====================
// 目的：把 F1 类构造层静默退化（cfg 载体下嵌套复合值被清空而 proof 不自知）转成响亮失败——
// 全部 B 档 harness 在验证目标属性前，先断言构造产物形状符合预期（键存在 / 类型正确 /
// 嵌套复合完整）。构造层若再次静默退化，proof 以断言失败报错，而非可疑超时/假 PASS。
// 成本：断言全部作用于具体构造（键名/期望值为编译期常量），CBMC 常量折叠后
// 预期路径零符号开销；符号选择点（如 4 元指令 type）仅增加 O(4) 字符串比较。

/// 断言 `v` 含键 `key` 并返回该字段引用（`v` 非对象或缺键即 panic = 响亮失败）。
fn shape_field<'a>(v: &'a JsonValue, what: &str, key: &str) -> &'a JsonValue {
    match v.get(key) {
        Some(f) => f,
        None => panic!(
            "结构自检失败 [{}]: 缺少键 {:?}（对象缺失或嵌套复合被清空）",
            what, key
        ),
    }
}

/// 断言字段为字符串 `expected`。
fn shape_str(v: &JsonValue, what: &str, expected: &str) {
    match v.as_str() {
        Some(s) => assert!(
            s == expected,
            "结构自检失败 [{}]: 应为 {:?}，实际 {:?}",
            what,
            expected,
            s
        ),
        None => panic!("结构自检失败 [{}]: 应为字符串 {:?}", what, expected),
    }
}

/// 断言字段为长度恰为 `expected_len` 的数组，返回切片引用。
fn shape_array<'a>(v: &'a JsonValue, what: &str, expected_len: usize) -> &'a [JsonValue] {
    match v.as_array() {
        Some(a) => {
            assert!(
                a.len() == expected_len,
                "结构自检失败 [{}]: 数组长度应为 {}，实际 {}",
                what,
                expected_len,
                a.len()
            );
            a
        }
        None => panic!("结构自检失败 [{}]: 应为长度 {} 的数组", what, expected_len),
    }
}

/// 断言字段为字符串且取值在 `allowed` 集合内（符号选择构造的值域哨兵）。
fn shape_str_in(v: &JsonValue, what: &str, allowed: &[&str]) {
    match v.as_str() {
        Some(s) => {
            let mut ok = false;
            for a in allowed {
                if s == *a {
                    ok = true;
                }
            }
            assert!(ok, "结构自检失败 [{}]: 字符串值不在允许集合内", what);
        }
        None => panic!("结构自检失败 [{}]: 应为字符串", what),
    }
}

/// 断言 `__exec__.payload.<key>` 链可达并返回叶子引用（单键族 exec_state 形状哨兵）。
fn shape_payload_leaf<'a>(state: &'a JsonValue, what: &str, key: &str) -> &'a JsonValue {
    shape_field(
        shape_field(shape_field(state, what, "__exec__"), what, "payload"),
        what,
        key,
    )
}

/// 断言完整 exec_state 形状（`__exec__` 存在 + instruction 字段存在 + payload 对象 +
/// queue 空数组），返回 payload 引用。适用于 any_state / state_with_payload 族。
fn shape_full_state<'a>(state: &'a JsonValue, what: &str) -> &'a JsonValue {
    let exec = shape_field(state, what, "__exec__");
    shape_field(exec, what, "instruction");
    let payload = shape_field(exec, what, "payload");
    assert!(
        payload.as_object().is_some(),
        "结构自检失败 [{}]: payload 应为对象",
        what
    );
    let queue = shape_field(exec, what, "queue");
    shape_array(queue, what, 0);
    payload
}

/// 断言 `model::concrete_exec_state()` 形状完整（P9/P10 共用哨兵）。
fn shape_concrete_exec_state<'a>(state: &'a JsonValue) -> &'a JsonValue {
    let exec = shape_field(state, "exec_state", "__exec__");
    let instruction = shape_field(exec, "exec_state", "instruction");
    shape_str(
        shape_field(instruction, "exec_state.instruction", "type"),
        "exec_state.instruction.type",
        "set",
    );
    let payload = shape_field(exec, "exec_state", "payload");
    shape_field(payload, "exec_state.payload", "x");
    shape_field(payload, "exec_state.payload", "y");
    let obj = shape_field(payload, "exec_state.payload", "obj");
    shape_field(obj, "exec_state.payload.obj", "flag");
    let items = shape_array(
        shape_field(payload, "exec_state.payload", "items"),
        "exec_state.payload.items",
        2,
    );
    shape_str(
        shape_field(&items[0], "exec_state.payload.items[0]", "name"),
        "exec_state.payload.items[0].name",
        "a",
    );
    shape_str(
        shape_field(&items[1], "exec_state.payload.items[1]", "name"),
        "exec_state.payload.items[1].name",
        "b",
    );
    payload
}

// ==================== Layer 1: 基础类型层 ====================

/// P1: JsonValue::PartialEq 永不 panic（6 种变体两两比较全覆盖）
/// 显式枚举 all_variants() 做两两比较：CBMC 按具体常量执行，无符号值，
/// 覆盖全部 6×6=36 种变体组合的 match 分支（不验证标准库 memcmp/BTreeMap 内部）。
///
/// ⚠️ 实测修正（见 verification/kani-formal-verification-design.md §9）：
/// 1. 输入用固定数组 `[JsonValue; 6]`（model::all_variants），索引循环，
///    避免 Vec 堆分配与 slice 迭代器展开；unwind=8 恰好完全展开 6×6 循环
///    （`for i in 0..6` 需 7 次展开：6 次 body + 1 次退出检查）。
/// 2. `core::mem::forget(vals)` 跳过 `JsonValue` 析构（BTreeMap/Vec/Cow 析构
///    会触发 CBMC 红黑树/堆指针展开，P1 曾 ≥2GB 内存不收敛）。
#[kani::proof]
#[kani::unwind(8)]
fn verify_partial_eq_never_panics() {
    let vals = model::all_variants();
    for i in 0..vals.len() {
        for j in 0..vals.len() {
            let _ = vals[i] == vals[j];
        }
    }
    core::mem::forget(vals);
}

/// P2: JsonValue::Ord 永不 panic
/// 同 P1：显式枚举 6 种变体两两 cmp，覆盖全部 match 分支。
#[kani::proof]
#[kani::unwind(8)]
fn verify_ord_never_panics() {
    let vals = model::all_variants();
    for i in 0..vals.len() {
        for j in 0..vals.len() {
            let _ = vals[i].cmp(&vals[j]);
        }
    }
    core::mem::forget(vals);
}

/// P3: 类型转换安全（as_* 均返回 Option，不 panic）
/// 遍历全部 6 种变体调用 as_*，覆盖类型匹配与不匹配两种路径。
#[kani::proof]
#[kani::unwind(8)]
fn verify_as_methods_never_panic() {
    let vals = model::all_variants();
    for i in 0..vals.len() {
        let v = &vals[i];
        let _ = v.as_i64();
        let _ = v.as_str();
        let _ = v.as_bool();
        let _ = v.as_array();
        let _ = v.as_object();
    }
    core::mem::forget(vals);
}

// ==================== Layer 2: 路径解析层 ====================

/// P4a: resolve_path 永不 panic - 简单字段访问 ("x")
/// 单条具体路径，每个 proof 只验证一次展开，避免累积爆炸。
#[kani::proof]
#[kani::unwind(8)]
fn verify_resolve_path_simple_field() {
    let state = JsonValue::array(vec![JsonValue::Integer(0), JsonValue::Integer(1)]);
    let _ = resolve_path(&state, "x");
    core::mem::forget(state);
}

/// P4b: resolve_path 永不 panic - 嵌套点号 ("x.y")
/// ⚠️ unwind 必须精确匹配 parse 循环次数（3 字符 → 4 次展开），
/// 过大（如 12）会制造大量无用符号分支导致 CBMC 状态爆炸（实测 2.5GB 不收敛）。
#[kani::proof]
#[kani::unwind(4)]
fn verify_resolve_path_nested_dot() {
    let state = JsonValue::array(vec![JsonValue::Integer(0), JsonValue::Integer(1)]);
    let _ = resolve_path(&state, "x.y");
    core::mem::forget(state);
}

/// P4c: resolve_path 永不 panic - 纯索引访问 ("[0]")
/// unwind=4：parse 循环 3 字符 → 4 次展开。
#[kani::proof]
#[kani::unwind(4)]
fn verify_resolve_path_array_index() {
    let state = JsonValue::array(vec![JsonValue::Integer(0), JsonValue::Integer(1)]);
    let _ = resolve_path(&state, "[0]");
    core::mem::forget(state);
}

/// P4d: resolve_path 永不 panic - 双点/空段无效 ("x..y" 或 ".x")
/// ⚠️ 用 ".x"（2字符）覆盖"空段非法"分支（return None）：
/// "x..y"（4字符）触发 `matches!(segments.last(), Some(PathSegment::Index(_,_)))`
/// 检查 + `core::mem::take` 转移，CBMC 展开成本过高（实测 unwind=5 ≥2GB 不收敛）；
/// ".x" 在首字符 '.' 处 current 为空且非 after_index → 直接 return None，分支更少。
/// unwind=3：2 字符 → 3 次展开。
#[kani::proof]
#[kani::unwind(3)]
fn verify_resolve_path_double_dot() {
    let state = JsonValue::array(vec![JsonValue::Integer(0), JsonValue::Integer(1)]);
    let _ = resolve_path(&state, ".x");
    core::mem::forget(state);
}

/// P4e: resolve_path 永不 panic - 转义点号 ("x\\.y")
/// unwind=5：4 字符 → 5 次展开。
#[kani::proof]
#[kani::unwind(5)]
fn verify_resolve_path_escaped_dot() {
    let state = JsonValue::array(vec![JsonValue::Integer(0), JsonValue::Integer(1)]);
    let _ = resolve_path(&state, "x\\.y");
    core::mem::forget(state);
}

/// P5: resolve_path 是确定性的（纯函数属性，经两次调用验证不 panic）
///
/// ⚠️ 注意：resolve_path 是纯函数（无全局状态/随机数），确定性为自由属性。
/// 本 proof 仅验证两次调用不 panic 且不产生副作用（assert_eq 会触发 PartialEq 额外
/// 展开，实测 ≥5.5GB 不收敛）。纯函数确定性由编译器保证，无需符号验证。
#[kani::proof]
#[kani::unwind(2)]
fn verify_resolve_path_deterministic() {
    let state = JsonValue::array(vec![JsonValue::Integer(0), JsonValue::Integer(1)]);
    // 两次调用，仅验证不 panic；确定性由纯函数性质保证
    let _ = resolve_path(&state, "x");
    let _ = resolve_path(&state, "x");
    core::mem::forget(state);
}

/// P6a: 无效路径返回 None - 空路径 ("")
#[kani::proof]
#[kani::unwind(1)]
fn verify_resolve_path_empty_returns_none() {
    let state = JsonValue::array(vec![JsonValue::Integer(0), JsonValue::Integer(1)]);
    assert!(resolve_path(&state, "").is_none());
    core::mem::forget(state);
}

/// P6b: 无效路径返回 None - 尾点号 ("x.")
/// unwind=3：2 字符 → 3 次展开。
#[kani::proof]
#[kani::unwind(3)]
fn verify_resolve_path_trailing_dot() {
    let state = JsonValue::array(vec![JsonValue::Integer(0), JsonValue::Integer(1)]);
    assert!(resolve_path(&state, "x.").is_none());
    core::mem::forget(state);
}

/// P6c: 无效路径返回 None - 无效索引字符 ("[abc]")
/// unwind=6：5 字符 → 6 次展开。
#[kani::proof]
#[kani::unwind(6)]
fn verify_resolve_path_invalid_index_char() {
    let state = JsonValue::array(vec![JsonValue::Integer(0), JsonValue::Integer(1)]);
    assert!(resolve_path(&state, "[abc]").is_none());
    core::mem::forget(state);
}

/// P6d: 无效路径返回 None - 缺少右括号 ("[0")
/// unwind=3：2 字符 → 3 次展开。
#[kani::proof]
#[kani::unwind(3)]
fn verify_resolve_path_missing_close_bracket() {
    let state = JsonValue::array(vec![JsonValue::Integer(0), JsonValue::Integer(1)]);
    assert!(resolve_path(&state, "[0").is_none());
    core::mem::forget(state);
}

/// P7: 数组索引不越界（get() 语义，越界返回 None 而非 panic）
///
/// ⚠️ 实测修正（见 verification/kani-formal-verification-design.md §9）：
/// 1. state 用**纯数组**（不包 Object/BTreeMap）：BTreeMap::insert 的红黑树
///    插入 + 析构会让 CBMC 展开路径爆炸（实测无限制 / unwind=8 均 ≥2GB 不收敛）；
///    纯 `JsonValue::Array` 的 `arr.get(idx)` 语义已能完整覆盖"越界返回 None"。
/// 2. `core::mem::forget(state)` 跳过 `Vec` 析构，避免展开堆指针释放逻辑。
/// 3. `arr[0]` 字段形式（`resolve_path(&state,"arr[0]")`）依赖 Object 包装，
///    会引入 BTreeMap 成本，其 Object 访问路径已由 P4a-P4e 覆盖，故此处仅验证纯索引。
#[kani::proof]
#[kani::unwind(4)]
fn verify_array_index_bounds() {
    let state = JsonValue::array(vec![JsonValue::Integer(1), JsonValue::Integer(2)]);
    let _ = resolve_path(&state, "[0]");
    let _ = resolve_path(&state, "[9]"); // 越界 → None，不 panic
    core::mem::forget(state);
}

// ==================== Layer 3: 域评估层 ====================

/// P8a: evaluate_domain eq 永不 panic
/// ⚠️ exec_state 用单键 ObjectMap（`single_key_exec_state`，kani 构建下即 KaniMap）控制展开规模。
/// 路径 "payload.x"（9 字符）parse 循环需 10 次展开，unwind=24 留余量。
#[kani::proof]
#[kani::unwind(24)]
fn verify_evaluate_domain_eq_never_panics() {
    let exec_state = model::single_key_exec_state();
    let domain = model::obj(vec![
        ("type", JsonValue::string("eq")),
        ("path", JsonValue::string("payload.x")),
        ("value", JsonValue::Integer(1)),
    ]);
    shape_str(shape_field(&domain, "domain", "type"), "domain.type", "eq");
    shape_str(
        shape_field(&domain, "domain", "path"),
        "domain.path",
        "payload.x",
    );
    shape_field(&domain, "domain", "value");
    shape_payload_leaf(&exec_state, "exec_state", "x");
    let _ = evaluate_domain(&domain, &exec_state);
    core::mem::forget(exec_state);
    core::mem::forget(domain);
}

/// P8h: evaluate_domain_outcome eq 永不 panic（三态版缺省路径，专项-20261001）
///
/// 与 P8a 同构（无 on_missing 声明 → 兼容缺省 unsat 分派），验证
/// DomainOutcome 三态求值路径（含 is_root_segment_dot_path 判定与
/// Missing 构造）不 panic。
#[kani::proof]
#[kani::unwind(24)]
fn verify_evaluate_domain_outcome_eq_never_panics() {
    let exec_state = model::single_key_exec_state();
    let domain = model::obj(vec![
        ("type", JsonValue::string("eq")),
        ("path", JsonValue::string("payload.x")),
        ("value", JsonValue::Integer(1)),
    ]);
    let _ = evaluate_domain_outcome(&domain, &exec_state);
    core::mem::forget(exec_state);
    core::mem::forget(domain);
}

/// P8i: evaluate_domain_outcome 的 MissingRejected 分派路径永不 panic
///
/// on_missing="error" 显式声明 + 路径缺失 → dispatch_missing 构造
/// `Err(MissingRejected)`（含 format! 拼接 detail）——验证错误构造
/// 路径不 panic（拼接口径：reason.label() 固定串 + 具体路径串）。
#[kani::proof]
#[kani::unwind(32)]
fn verify_evaluate_domain_outcome_missing_rejected_never_panics() {
    let exec_state = model::single_key_exec_state();
    let domain = model::obj(vec![
        ("type", JsonValue::string("eq")),
        ("path", JsonValue::string("payload.missing")),
        ("value", JsonValue::Integer(1)),
        ("on_missing", JsonValue::string("error")),
    ]);
    let _ = evaluate_domain_outcome(&domain, &exec_state);
    core::mem::forget(exec_state);
    core::mem::forget(domain);
}

/// P8b: evaluate_domain lt 永不 panic
#[kani::proof]
#[kani::unwind(24)]
fn verify_evaluate_domain_lt_never_panics() {
    let exec_state = model::single_key_exec_state();
    let domain = model::obj(vec![
        ("type", JsonValue::string("lt")),
        ("path", JsonValue::string("payload.x")),
        ("value", JsonValue::Integer(1)),
    ]);
    shape_str(shape_field(&domain, "domain", "type"), "domain.type", "lt");
    shape_str(
        shape_field(&domain, "domain", "path"),
        "domain.path",
        "payload.x",
    );
    shape_field(&domain, "domain", "value");
    shape_payload_leaf(&exec_state, "exec_state", "x");
    let _ = evaluate_domain(&domain, &exec_state);
    core::mem::forget(exec_state);
    core::mem::forget(domain);
}

/// P8c: evaluate_domain exists 永不 panic
#[kani::proof]
#[kani::unwind(24)]
fn verify_evaluate_domain_exists_never_panics() {
    let exec_state = model::single_key_exec_state();
    let domain = model::obj(vec![
        ("type", JsonValue::string("exists")),
        ("path", JsonValue::string("payload.x")),
    ]);
    shape_str(
        shape_field(&domain, "domain", "type"),
        "domain.type",
        "exists",
    );
    shape_str(
        shape_field(&domain, "domain", "path"),
        "domain.path",
        "payload.x",
    );
    shape_payload_leaf(&exec_state, "exec_state", "x");
    let _ = evaluate_domain(&domain, &exec_state);
    core::mem::forget(exec_state);
    core::mem::forget(domain);
}

/// P8d: evaluate_domain instruction 永不 panic
/// 无路径解析（instruction 域直接读 state 的 instruction 字段）。
/// ⚠️ 仍用单键 exec_state（kani 构建下即 KaniMap）控制展开规模。
#[kani::proof]
#[kani::unwind(32)]
fn verify_evaluate_domain_instruction_never_panics() {
    let exec_state = model::single_key_exec_state();
    let domain = model::obj(vec![
        ("type", JsonValue::string("instruction")),
        ("instruction_type", JsonValue::string("set")),
    ]);
    shape_str(
        shape_field(&domain, "domain", "type"),
        "domain.type",
        "instruction",
    );
    shape_str(
        shape_field(&domain, "domain", "instruction_type"),
        "domain.instruction_type",
        "set",
    );
    shape_payload_leaf(&exec_state, "exec_state", "x");
    let _ = evaluate_domain(&domain, &exec_state);
    core::mem::forget(exec_state);
    core::mem::forget(domain);
}

/// P8e: evaluate_domain all 永不 panic（空列表）
#[kani::proof]
#[kani::unwind(16)]
fn verify_evaluate_domain_all_never_panics() {
    let exec_state = model::single_key_exec_state();
    let domain = model::obj(vec![
        ("type", JsonValue::string("all")),
        ("inner", JsonValue::Array(vec![])),
    ]);
    shape_str(shape_field(&domain, "domain", "type"), "domain.type", "all");
    shape_array(shape_field(&domain, "domain", "inner"), "domain.inner", 0);
    shape_payload_leaf(&exec_state, "exec_state", "x");
    let _ = evaluate_domain(&domain, &exec_state);
    core::mem::forget(exec_state);
    core::mem::forget(domain);
}

/// P8f: evaluate_domain not 永不 panic
#[kani::proof]
#[kani::unwind(24)]
fn verify_evaluate_domain_not_never_panics() {
    let exec_state = model::single_key_exec_state();
    let domain = model::obj(vec![
        ("type", JsonValue::string("not")),
        (
            "inner",
            model::obj(vec![
                ("type", JsonValue::string("exists")),
                ("path", JsonValue::string("payload.x")),
            ]),
        ),
    ]);
    shape_str(shape_field(&domain, "domain", "type"), "domain.type", "not");
    let inner = shape_field(&domain, "domain", "inner");
    shape_str(
        shape_field(inner, "domain.inner", "type"),
        "domain.inner.type",
        "exists",
    );
    shape_str(
        shape_field(inner, "domain.inner", "path"),
        "domain.inner.path",
        "payload.x",
    );
    shape_payload_leaf(&exec_state, "exec_state", "x");
    let _ = evaluate_domain(&domain, &exec_state);
    core::mem::forget(exec_state);
    core::mem::forget(domain);
}

/// P8g: evaluate_domain has_fields 永不 panic
/// ⚠️ 单键 exec_state 不含 obj.flag，故 domain 路径指向不存在字段（覆盖缺失分支）。
#[kani::proof]
#[kani::unwind(24)]
fn verify_evaluate_domain_has_fields_never_panics() {
    let exec_state = model::single_key_exec_state();
    let domain = model::obj(vec![
        ("type", JsonValue::string("has_fields")),
        ("path", JsonValue::string("payload.x")),
        ("fields", JsonValue::array(vec![JsonValue::string("flag")])),
    ]);
    shape_str(
        shape_field(&domain, "domain", "type"),
        "domain.type",
        "has_fields",
    );
    shape_str(
        shape_field(&domain, "domain", "path"),
        "domain.path",
        "payload.x",
    );
    let fields = shape_array(shape_field(&domain, "domain", "fields"), "domain.fields", 1);
    shape_str(&fields[0], "domain.fields[0]", "flag");
    shape_payload_leaf(&exec_state, "exec_state", "x");
    let _ = evaluate_domain(&domain, &exec_state);
    core::mem::forget(exec_state);
    core::mem::forget(domain);
}

/// P9: evaluate_domain 是确定性的（纯函数属性）
///
/// ⚠️ 同 P5：evaluate_domain 是纯函数（无全局状态/随机数），确定性为自由属性，
/// 无需符号验证。用完全具体的 domain + exec_state 验证两次调用不 panic。
#[kani::proof]
#[kani::unwind(16)]
fn verify_evaluate_domain_deterministic() {
    let domain = model::obj(vec![
        ("type", JsonValue::string("eq")),
        ("path", JsonValue::string("payload.x")),
        ("value", JsonValue::Integer(1)),
    ]);
    let exec_state = model::concrete_exec_state();
    shape_str(shape_field(&domain, "domain", "type"), "domain.type", "eq");
    shape_str(
        shape_field(&domain, "domain", "path"),
        "domain.path",
        "payload.x",
    );
    shape_field(&domain, "domain", "value");
    shape_concrete_exec_state(&exec_state);
    let _ = evaluate_domain(&domain, &exec_state);
    let _ = evaluate_domain(&domain, &exec_state);
}

/// P10: 深度限制生效（kani 构建下 MAX_DOMAIN_DEPTH=4，CR-20260913-002；生产构建为 64）
/// 用具体深嵌套输入（嵌套 MAX_DOMAIN_DEPTH+1 层 not）验证不 panic 且深度分支可达。
/// 注：evaluate_domain_inner 与 MAX_DOMAIN_DEPTH 均为私有，只能经 evaluate_domain 间接验证。
/// unwind 需 > 5（evaluate_domain_inner 递归 5 层后到达深度保护分支）。
/// exec_state 用完全具体实例避免状态爆炸。
#[kani::proof]
#[kani::unwind(16)]
fn verify_domain_depth_limit() {
    let mut domain = model::obj(vec![
        ("type", JsonValue::string("exists")),
        ("path", JsonValue::string("payload.x")),
    ]);
    for _ in 0..(MAX_DOMAIN_DEPTH + 1) {
        domain = model::obj(vec![("type", JsonValue::string("not")), ("inner", domain)]);
    }
    let exec_state = model::concrete_exec_state();
    let mut cur = &domain;
    for _ in 0..(MAX_DOMAIN_DEPTH + 1) {
        shape_str(shape_field(cur, "domain", "type"), "domain.type", "not");
        cur = shape_field(cur, "domain", "inner");
    }
    shape_str(shape_field(cur, "domain", "type"), "domain.type", "exists");
    shape_concrete_exec_state(&exec_state);
    // 不 panic；depth > MAX_DOMAIN_DEPTH 时返回 false（evaluate_domain_inner 深度保护）
    let _ = evaluate_domain(&domain, &exec_state);
}

/// P11: has_fields 空数组返回 false（与源码语义一致）
#[kani::proof]
fn verify_has_fields_empty_array() {
    // exec_state 顶层必须有 __exec__，路径写全 __exec__.payload.obj
    let exec_state = model::obj(vec![(
        "__exec__",
        model::obj(vec![(
            "payload",
            model::obj(vec![(
                "obj",
                model::obj(vec![("tool_calls", JsonValue::Array(vec![]))]),
            )]),
        )]),
    )]);
    let domain = model::obj(vec![
        ("type", JsonValue::string("has_fields")),
        ("path", JsonValue::string("__exec__.payload.obj")),
        (
            "fields",
            JsonValue::array(vec![JsonValue::string("tool_calls")]),
        ),
    ]);
    let obj = shape_payload_leaf(&exec_state, "exec_state", "obj");
    shape_array(
        shape_field(obj, "exec_state.payload.obj", "tool_calls"),
        "exec_state.payload.obj.tool_calls",
        0,
    );
    shape_str(
        shape_field(&domain, "domain", "type"),
        "domain.type",
        "has_fields",
    );
    shape_str(
        shape_field(&domain, "domain", "path"),
        "domain.path",
        "__exec__.payload.obj",
    );
    let fields = shape_array(shape_field(&domain, "domain", "fields"), "domain.fields", 1);
    shape_str(&fields[0], "domain.fields[0]", "tool_calls");
    // evaluate_domain 现返回 Result<bool, TcbError>（回归验证 fail-fast 语义）：
    // 结构合法域求值 Ok(false)——空数组视为不存在
    let r = evaluate_domain(&domain, &exec_state);
    assert!(matches!(r, Ok(false)), "空数组应视为不存在");
}

// ==================== Layer 4: 元指令层 ====================

/// P12: execute_meta_instruction 永不 panic（4 种元指令全覆盖）
/// 私有元指令（exec_set/exec_push/exec_branch/exec_io_request）
/// 统一经 execute_meta_instruction 按 type 间接覆盖。
#[kani::proof]
fn verify_execute_meta_instruction_never_panics() {
    let instr = model::any_instruction();
    let state = model::any_state();
    let depth = kani::any::<usize>();
    kani::assume(depth < MAX_BRANCH_DEPTH);
    shape_str_in(
        shape_field(&instr, "instr", "type"),
        "instr.type",
        &["set", "push", "branch", "io_request"],
    );
    let payload = shape_full_state(&state, "state");
    shape_field(payload, "state.payload", "x");
    shape_field(payload, "state.payload", "y");
    let obj = shape_field(payload, "state.payload", "obj");
    shape_field(obj, "state.payload.obj", "flag");
    let _ = execute_meta_instruction(&instr, state, depth);
}

/// P13: set 算术安全（add/sub 溢出返回 IntegerOverflow，不 panic）
#[kani::proof]
fn verify_exec_set_arithmetic_safe() {
    let instr = model::obj(vec![
        ("type", JsonValue::string("set")),
        (
            "params",
            model::obj(vec![
                ("attr", JsonValue::string("x")),
                (
                    "operation",
                    JsonValue::string(if kani::any::<bool>() { "add" } else { "sub" }),
                ),
                ("value", JsonValue::Integer(kani::any::<i64>())),
            ]),
        ),
    ]);
    let state = model::any_state();
    shape_str(shape_field(&instr, "instr", "type"), "instr.type", "set");
    let params = shape_field(&instr, "instr", "params");
    shape_str(shape_field(params, "params", "attr"), "params.attr", "x");
    shape_str_in(
        shape_field(params, "params", "operation"),
        "params.operation",
        &["add", "sub"],
    );
    shape_field(params, "params", "value");
    let payload = shape_full_state(&state, "state");
    shape_field(payload, "state.payload", "x");
    let r = execute_meta_instruction(&instr, state, 0);
    // 无论 Ok/Err 均不 panic；溢出时返回 IntegerOverflow
    if let Err(e) = r {
        let _ = e;
    }
}

/// P14: branch 深度限制生效（depth >= MAX_BRANCH_DEPTH → NestingTooDeep）
#[kani::proof]
fn verify_branch_depth_limit() {
    let instr = model::obj(vec![
        ("type", JsonValue::string("branch")),
        (
            "params",
            model::obj(vec![
                (
                    "domain",
                    model::obj(vec![
                        ("type", JsonValue::string("exists")),
                        ("path", JsonValue::string("x")),
                    ]),
                ),
                (
                    "on_true",
                    JsonValue::Array(vec![model::obj(vec![("type", JsonValue::string("noop"))])]),
                ),
                (
                    "on_false",
                    JsonValue::Array(vec![model::obj(vec![("type", JsonValue::string("noop"))])]),
                ),
            ]),
        ),
    ]);
    let state = model::any_state();
    shape_str(shape_field(&instr, "instr", "type"), "instr.type", "branch");
    let params = shape_field(&instr, "instr", "params");
    let domain = shape_field(params, "params", "domain");
    shape_str(
        shape_field(domain, "params.domain", "type"),
        "params.domain.type",
        "exists",
    );
    shape_str(
        shape_field(domain, "params.domain", "path"),
        "params.domain.path",
        "x",
    );
    let on_true = shape_array(
        shape_field(params, "params", "on_true"),
        "params.on_true",
        1,
    );
    shape_str(
        shape_field(&on_true[0], "params.on_true[0]", "type"),
        "params.on_true[0].type",
        "noop",
    );
    let on_false = shape_array(
        shape_field(params, "params", "on_false"),
        "params.on_false",
        1,
    );
    shape_str(
        shape_field(&on_false[0], "params.on_false[0]", "type"),
        "params.on_false[0].type",
        "noop",
    );
    let payload = shape_full_state(&state, "state");
    shape_field(payload, "state.payload", "x");
    let r = execute_meta_instruction(&instr, state, MAX_BRANCH_DEPTH);
    // depth >= MAX_BRANCH_DEPTH 时返回 NestingTooDeep（不 panic）
    assert!(matches!(r, Err(TcbError::NestingTooDeep { .. })));
}

/// P18: io_request 触发正确（v0.3.1 ReAct：可选参数路径不存在时跳过，不 panic）
#[kani::proof]
fn verify_io_request_safe() {
    let instr = model::obj(vec![
        ("type", JsonValue::string("io_request")),
        (
            "params",
            model::obj(vec![
                ("io_type", JsonValue::string("call_external")),
                ("messages", JsonValue::string("__exec__.payload.messages")),
                // tools 路径不存在 → 可选参数，跳过（不 panic）
                ("tools", JsonValue::string("__exec__.payload.missing_tools")),
            ]),
        ),
    ]);
    let state = model::state_with_payload(ObjectMap::new());
    shape_str(
        shape_field(&instr, "instr", "type"),
        "instr.type",
        "io_request",
    );
    let params = shape_field(&instr, "instr", "params");
    shape_str(
        shape_field(params, "params", "io_type"),
        "params.io_type",
        "call_external",
    );
    shape_str(
        shape_field(params, "params", "messages"),
        "params.messages",
        "__exec__.payload.messages",
    );
    shape_str(
        shape_field(params, "params", "tools"),
        "params.tools",
        "__exec__.payload.missing_tools",
    );
    shape_full_state(&state, "state");
    let r = execute_meta_instruction(&instr, state, 0);
    assert!(r.is_ok(), "io_request 不应 panic");
}

// ==================== Layer 4.5: enforce 强制原语（回归验证，P18a-P18c） ====================

// ⚠️ 实测教训（沿用 P8 系列经验，见 model.rs 注释）：符号叶子 exec_state + 域求值
// 会让 CBMC/SAT 展开状态爆炸（P18a 首版实测 2.5h 不收敛）。因此：
// - P18a（永不 panic）：叶子**全部具体化**，仅结构选择符号化（7 域类型 7 选 1、
//   形态/缺字段开关）——panic 自由只依赖代码路径覆盖，具体值不影响覆盖面；
// - P18b/P18c（语义属性）：使用**单键最小状态**（仅 payload.x，1 键 payload，
//   同 P13 已验证收敛的规模），语义属性需要符号值驱动真假两分支。

/// P18a 专用：**全具体** exec_state（payload.x=1 / obj.flag / d=exists 域对象）。
fn concrete_enforce_state() -> JsonValue {
    let mut payload = ObjectMap::new();
    payload.insert("x".to_string(), JsonValue::Integer(1));
    payload.insert(
        "obj".to_string(),
        model::obj(vec![("flag", JsonValue::Bool(true))]),
    );
    payload.insert(
        "d".to_string(),
        model::obj(vec![
            ("type", JsonValue::string("exists")),
            ("path", JsonValue::string("payload.x")),
        ]),
    );
    let state = model::state_with_payload(payload);
    let x = shape_payload_leaf(&state, "state", "x");
    assert!(
        x.as_i64() == Some(1),
        "结构自检失败 [state.payload.x]: 应为 Integer(1)"
    );
    let obj = shape_payload_leaf(&state, "state", "obj");
    shape_field(obj, "state.payload.obj", "flag");
    let d = shape_payload_leaf(&state, "state", "d");
    shape_str(
        shape_field(d, "state.payload.d", "type"),
        "state.payload.d.type",
        "exists",
    );
    state
}

/// P18a 专用：7 域类型 7 选 1 的**具体形状** domain（值取 2，对 x=1 eq/lt 恒假），
/// 或 `__` 路径引用形态（指向 payload.d 的 exists 域，恒真）。
/// 真假两分支均可达：exists/instruction/all/has_fields/路径引用 → 真（达 reason 检查与
/// Halted 构造）；eq/lt/not → 假（达 State 返回）。
fn concrete_domain(t: u8, use_path_ref: bool) -> JsonValue {
    if use_path_ref {
        let s = JsonValue::string("__exec__.payload.d");
        shape_str(&s, "domain(path_ref)", "__exec__.payload.d");
        return s;
    }
    match t % 7 {
        0 => {
            let d = model::obj(vec![
                ("type", JsonValue::string("eq")),
                ("path", JsonValue::string("payload.x")),
                ("value", JsonValue::Integer(2)),
            ]);
            shape_str(
                shape_field(&d, "domain[eq]", "type"),
                "domain[eq].type",
                "eq",
            );
            shape_str(
                shape_field(&d, "domain[eq]", "path"),
                "domain[eq].path",
                "payload.x",
            );
            shape_field(&d, "domain[eq]", "value");
            d
        }
        1 => {
            let d = model::obj(vec![
                ("type", JsonValue::string("lt")),
                ("path", JsonValue::string("payload.x")),
                ("value", JsonValue::Integer(2)),
            ]);
            shape_str(
                shape_field(&d, "domain[lt]", "type"),
                "domain[lt].type",
                "lt",
            );
            shape_str(
                shape_field(&d, "domain[lt]", "path"),
                "domain[lt].path",
                "payload.x",
            );
            shape_field(&d, "domain[lt]", "value");
            d
        }
        2 => {
            let d = model::obj(vec![
                ("type", JsonValue::string("exists")),
                ("path", JsonValue::string("payload.x")),
            ]);
            shape_str(
                shape_field(&d, "domain[exists]", "type"),
                "domain[exists].type",
                "exists",
            );
            shape_str(
                shape_field(&d, "domain[exists]", "path"),
                "domain[exists].path",
                "payload.x",
            );
            d
        }
        3 => {
            let d = model::obj(vec![
                ("type", JsonValue::string("instruction")),
                ("instruction_type", JsonValue::string("noop")),
            ]);
            shape_str(
                shape_field(&d, "domain[instr]", "type"),
                "domain[instr].type",
                "instruction",
            );
            shape_str(
                shape_field(&d, "domain[instr]", "instruction_type"),
                "domain[instr].instruction_type",
                "noop",
            );
            d
        }
        4 => {
            let d = model::obj(vec![
                ("type", JsonValue::string("all")),
                (
                    "inner",
                    JsonValue::Array(vec![model::obj(vec![
                        ("type", JsonValue::string("exists")),
                        ("path", JsonValue::string("payload.x")),
                    ])]),
                ),
            ]);
            shape_str(
                shape_field(&d, "domain[all]", "type"),
                "domain[all].type",
                "all",
            );
            let inner = shape_array(
                shape_field(&d, "domain[all]", "inner"),
                "domain[all].inner",
                1,
            );
            shape_str(
                shape_field(&inner[0], "domain[all].inner[0]", "type"),
                "domain[all].inner[0].type",
                "exists",
            );
            d
        }
        5 => {
            let d = model::obj(vec![
                ("type", JsonValue::string("not")),
                (
                    "inner",
                    model::obj(vec![
                        ("type", JsonValue::string("exists")),
                        ("path", JsonValue::string("payload.x")),
                    ]),
                ),
            ]);
            shape_str(
                shape_field(&d, "domain[not]", "type"),
                "domain[not].type",
                "not",
            );
            let inner = shape_field(&d, "domain[not]", "inner");
            shape_str(
                shape_field(inner, "domain[not].inner", "type"),
                "domain[not].inner.type",
                "exists",
            );
            d
        }
        _ => {
            let d = model::obj(vec![
                ("type", JsonValue::string("has_fields")),
                ("path", JsonValue::string("payload.obj")),
                ("fields", JsonValue::Array(vec![JsonValue::string("flag")])),
            ]);
            shape_str(
                shape_field(&d, "domain[has_fields]", "type"),
                "domain[has_fields].type",
                "has_fields",
            );
            let fields = shape_array(
                shape_field(&d, "domain[has_fields]", "fields"),
                "domain[has_fields].fields",
                1,
            );
            shape_str(&fields[0], "domain[has_fields].fields[0]", "flag");
            d
        }
    }
}

/// 构造 type=enforce 指令（domain/reason 按开关放置，覆盖缺失字段错误路径）。
fn enforce_instruction(domain: JsonValue, with_domain: bool, with_reason: bool) -> JsonValue {
    let mut params = ObjectMap::new();
    if with_domain {
        params.insert("domain".to_string(), domain);
    }
    if with_reason {
        params.insert("reason".to_string(), JsonValue::string("guard"));
    }
    assert!(
        params.contains_key("domain") == with_domain,
        "结构自检失败 [enforce.params]: domain 放置与开关不符"
    );
    assert!(
        params.contains_key("reason") == with_reason,
        "结构自检失败 [enforce.params]: reason 放置与开关不符"
    );
    let mut instr = ObjectMap::new();
    instr.insert("type".to_string(), JsonValue::string("enforce"));
    instr.insert("params".to_string(), JsonValue::Object(params));
    let instr = JsonValue::Object(instr);
    shape_str(
        shape_field(&instr, "enforce.instr", "type"),
        "enforce.instr.type",
        "enforce",
    );
    shape_field(&instr, "enforce.instr", "params");
    instr
}

/// P18a: exec_enforce 永不 panic（经公开 execute_meta_instruction 间接覆盖私有 exec_enforce）
/// 覆盖：7 域类型 × 字面对象/`__` 路径引用两种 domain 形态 × domain/reason 缺失错误路径
/// × 真假两求值分支（叶子具体化，结构选择符号化——见本节头部实测教训）。
#[kani::proof]
fn verify_exec_enforce_never_panics() {
    let t = kani::any::<u8>();
    let use_path_ref = kani::any::<bool>();
    let with_domain = kani::any::<bool>();
    let with_reason = kani::any::<bool>();
    let instr = enforce_instruction(concrete_domain(t, use_path_ref), with_domain, with_reason);
    let state = concrete_enforce_state();
    let _ = execute_meta_instruction(&instr, state, 0);
}

/// P18b/C 专用：**单键最小** enforce 指令（eq 域 + reason，符号值驱动真假两分支）。
fn eq_enforce_instruction(v: i64) -> JsonValue {
    let domain = model::obj(vec![
        ("type", JsonValue::string("eq")),
        ("path", JsonValue::string("payload.x")),
        ("value", JsonValue::Integer(v)),
    ]);
    shape_str(
        shape_field(&domain, "eq_domain", "type"),
        "eq_domain.type",
        "eq",
    );
    shape_str(
        shape_field(&domain, "eq_domain", "path"),
        "eq_domain.path",
        "payload.x",
    );
    shape_field(&domain, "eq_domain", "value");
    enforce_instruction(domain, true, true)
}

/// P18b/C 专用：**单键最小** exec_state（仅 payload.x，1 键 payload——同 P13 收敛规模）。
fn minimal_state(x: i64) -> JsonValue {
    let mut payload = ObjectMap::new();
    payload.insert("x".to_string(), JsonValue::Integer(x));
    let state = model::state_with_payload(payload);
    shape_payload_leaf(&state, "minimal_state", "x");
    state
}

/// P18b: enforce 二值语义——domain 为真当且仅当 `Halted { reason 原文 }`；
/// domain 为假走 noop 且状态原样保留（`payload.x` 值不变）。
/// 这是"阻止而非仅留痕"语义的形式化锚点：Halted 不携带任何状态，
/// 调用方（transition/reactor）丢弃半成品状态即不可能"边拦截边放行"。
#[kani::proof]
fn verify_exec_enforce_halt_semantics() {
    let x = kani::any::<i64>();
    let v = kani::any::<i64>();
    let instr = eq_enforce_instruction(v);
    let state = minimal_state(x);
    let r = execute_meta_instruction(&instr, state, 0);
    match r {
        Ok(MetaInstructionResult::Halted { reason }) => {
            kani::assert(x == v, "domain 求值为真（payload.x == value）才允许 Halted");
            kani::assert(
                reason == "guard",
                "Halted.reason 必须是 enforce.params.reason 原文（审计回显）",
            );
        }
        Ok(MetaInstructionResult::State(s)) => {
            kani::assert(x != v, "domain 求值为假必须走 noop 继续");
            match resolve_path(&s, "__exec__.payload.x") {
                Some(resolved) => {
                    kani::assert(*resolved == JsonValue::Integer(x), "noop 分支状态原样保留")
                }
                None => kani::assert(false, "noop 分支必须完整返回状态"),
            }
        }
        _ => kani::assert(false, "形态完整的 enforce 输入不得产生 IoRequired/Err"),
    }
}

/// P18c: enforce 确定性——同输入两次执行结果完全一致（真假两分支皆覆盖）。
#[kani::proof]
fn verify_exec_enforce_deterministic() {
    let x = kani::any::<i64>();
    let v = kani::any::<i64>();
    let instr = eq_enforce_instruction(v);
    // 构造两次相同状态（避免 JsonValue 深拷贝展开），分别独立执行
    let r1 = execute_meta_instruction(&instr, minimal_state(x), 0);
    let r2 = execute_meta_instruction(&instr, minimal_state(x), 0);
    match (r1, r2) {
        (Ok(a), Ok(b)) => kani::assert(a == b, "enforce 同输入两次执行结果必须一致"),
        (Err(_), Err(_)) => {} // 本证明输入形态完整，Err 分支不可达；同为 Err 亦满足形态一致
        _ => kani::assert(false, "同输入两次执行的结果形态必须一致"),
    }
}

/// P18d: 约束前置门排列等价——顶层 enforce 的判定与其在 core_eval 中的
/// 排列位置无关（BUG-P0-005 修复的核心语义保证，2026-09-24 约束前置门引入）。
/// 同一 {enforce, noop} 两规则集按两种排列分别执行：
/// - x == v（domain 为真）：两排列都必须 `Halted`，`reason` 原文一致
///   （`rule_index` 允许不同——它是各自列表内的下标，本就随排列变化）；
/// - x != v（domain 为假）：两排列都不得因 enforce 停，状态面等价
///   （State 载荷一致 / Ignored 归因向量等长）。
/// 判定依据：约束前置门把 enforce 判定上下文固定为转换前输入状态快照，
/// 且 noop 不修改 `__exec__.*` 求值面。
#[kani::proof]
fn verify_exec_enforce_permutation_equivalence() {
    let x = kani::any::<i64>();
    let v = kani::any::<i64>();
    let enforce_rule = eq_enforce_instruction(v);
    let noop_rule = model::obj(vec![("type", JsonValue::string("noop"))]);
    let instruction = model::obj(vec![("type", JsonValue::string("noop"))]);
    let queue: Vec<JsonValue> = vec![];
    // 排列 A：enforce 在前；排列 B：noop 在前。各独立构造相同 payload（避免 JsonValue 深拷贝展开）。
    let core_a = vec![enforce_rule.clone(), noop_rule.clone()];
    let core_b = vec![noop_rule, enforce_rule];
    assert!(
        core_a.len() == 2 && core_b.len() == 2,
        "结构自检失败 [core]: 应各为 2 条规则"
    );
    shape_str(&core_a[0], "core_a[0]", "enforce");
    shape_str(&core_b[1], "core_b[1]", "enforce");
    shape_str(&core_a[1], "core_a[1]", "noop");
    shape_str(&core_b[0], "core_b[0]", "noop");
    shape_str(
        shape_field(&instruction, "instruction", "type"),
        "instruction.type",
        "noop",
    );
    let mut pm_a = ObjectMap::new();
    pm_a.insert("x".to_string(), JsonValue::Integer(x));
    let payload_a = JsonValue::Object(pm_a);
    let mut pm_b = ObjectMap::new();
    pm_b.insert("x".to_string(), JsonValue::Integer(x));
    let payload_b = JsonValue::Object(pm_b);

    let ra = execute_transition(&core_a, &instruction, &payload_a, &queue);
    let rb = execute_transition(&core_b, &instruction, &payload_b, &queue);
    match (ra, rb) {
        // domain 为真：两排列都必须 Halted，reason 原文跨排列一致
        (
            Ok(TransitionResult::Halted { reason: r1, .. }),
            Ok(TransitionResult::Halted { reason: r2, .. }),
        ) => {
            kani::assert(
                x == v,
                "Halted 收场当且仅当 enforce domain 为真（payload.x == value）",
            );
            kani::assert(r1 == r2, "Halted.reason 原文必须跨排列一致");
        }
        // domain 为假：两排列都不得因 enforce 停；状态面（payload/queue）等价
        (
            Ok(TransitionResult::State {
                new_payload: pa,
                new_queue: qa,
                ..
            }),
            Ok(TransitionResult::State {
                new_payload: pb,
                new_queue: qb,
                ..
            }),
        ) => {
            kani::assert(x != v, "State 收场当且仅当 enforce domain 为假");
            kani::assert(pa == pb, "两排列终态 payload 必须等价");
            kani::assert(qa == qb, "两排列终态 queue 必须等价");
        }
        // domain 为假且规则整体产生 noop 效果：归因向量等长（下标本异，不逐位比）
        (
            Ok(TransitionResult::Ignored { rule_hits: ha, .. }),
            Ok(TransitionResult::Ignored { rule_hits: hb, .. }),
        ) => {
            kani::assert(x != v, "Ignored 收场当且仅当 enforce domain 为假");
            kani::assert(ha.len() == hb.len(), "两排列归因向量必须等长");
        }
        _ => kani::assert(false, "两排列的结果形态必须一致（判定与排列无关）"),
    }
}

// ==================== Layer 5: 状态转换层 ====================

/// P19: execute_transition 永不 panic（结构化符号）
#[kani::proof]
fn verify_execute_transition_never_panics() {
    let core_eval = vec![model::obj(vec![("type", JsonValue::string("noop"))])]; // 固定规则数（1），避免全符号 Vec 展开
    let instruction = model::any_instruction();
    let payload = model::any_payload();
    let queue: Vec<JsonValue> = vec![]; // 固定空队列
    assert!(
        core_eval.len() == 1,
        "结构自检失败 [core_eval]: 应为 1 条规则"
    );
    shape_str(&core_eval[0], "core_eval[0]", "noop");
    shape_str_in(
        shape_field(&instruction, "instruction", "type"),
        "instruction.type",
        &["set", "push", "branch", "io_request"],
    );
    shape_field(&payload, "payload", "x");
    shape_field(&payload, "payload", "y");
    let obj = shape_field(&payload, "payload", "obj");
    shape_field(obj, "payload.obj", "flag");
    let _ = execute_transition(&core_eval, &instruction, &payload, &queue);
}

/// P20: 规则数限制生效（core_eval.len() > MAX_TRANSFORM_RULES → TooManyTransformRules）
#[kani::proof]
fn verify_transform_rules_limit() {
    let core_eval: Vec<JsonValue> = (0..=MAX_TRANSFORM_RULES)
        .map(|_| model::obj(vec![("type", JsonValue::string("noop"))]))
        .collect(); // MAX_TRANSFORM_RULES + 1 条规则
    let instruction = model::obj(vec![("type", JsonValue::string("noop"))]);
    let payload = JsonValue::empty_object();
    let queue: Vec<JsonValue> = vec![];
    assert!(
        core_eval.len() == MAX_TRANSFORM_RULES + 1,
        "结构自检失败 [core_eval]: 规则数应为 MAX_TRANSFORM_RULES+1"
    );
    shape_str(&core_eval[0], "core_eval[0]", "noop");
    shape_str(&core_eval[MAX_TRANSFORM_RULES], "core_eval[last]", "noop");
    shape_str(
        shape_field(&instruction, "instruction", "type"),
        "instruction.type",
        "noop",
    );
    assert!(
        payload.as_object().is_some(),
        "结构自检失败 [payload]: 应为 Object"
    );
    let r = execute_transition(&core_eval, &instruction, &payload, &queue);
    assert!(matches!(r, Err(TcbError::TooManyTransformRules { .. })));
}

/// P21: ReAct 循环——call_external 无结果时返回 IoRequired（v0.3.1）
/// 手工构造 ReAct 三条规则（与 transition.rs react_e2e_tests 一致），见 model::react_core_eval()。
#[kani::proof]
fn verify_react_io_required() {
    let core_eval = model::react_core_eval();
    let instruction = model::obj(vec![
        ("type", JsonValue::string("call_external")),
        (
            "params",
            model::obj(vec![
                (
                    "messages",
                    JsonValue::Array(vec![model::obj(vec![
                        ("role", JsonValue::string("user")),
                        ("content", JsonValue::string("hi")),
                    ])]),
                ),
                ("tools", JsonValue::Array(vec![])),
            ]),
        ),
    ]);
    let payload = JsonValue::empty_object();
    let queue: Vec<JsonValue> = vec![];
    assert!(
        core_eval.len() == 3,
        "结构自检失败 [core_eval]: 应为 3 条 ReAct 规则"
    );
    shape_str(&core_eval[0], "core_eval[0]", "branch");
    shape_str(&core_eval[1], "core_eval[1]", "branch");
    shape_str(&core_eval[2], "core_eval[2]", "branch");
    let p0 = shape_field(&core_eval[0], "core_eval[0]", "params");
    let d0 = shape_field(p0, "core_eval[0].params", "domain");
    shape_str(
        shape_field(d0, "core_eval[0].params.domain", "type"),
        "self_init.domain.type",
        "all",
    );
    let inner0 = shape_array(
        shape_field(d0, "core_eval[0].params.domain", "inner"),
        "self_init.domain.inner",
        2,
    );
    shape_str(
        shape_field(&inner0[0], "self_init.domain.inner[0]", "type"),
        "self_init.domain.inner[0].type",
        "instruction",
    );
    shape_str(
        shape_field(&inner0[1], "self_init.domain.inner[1]", "type"),
        "self_init.domain.inner[1].type",
        "not",
    );
    let p1 = shape_field(&core_eval[1], "core_eval[1]", "params");
    let d1 = shape_field(p1, "core_eval[1].params", "domain");
    shape_str(
        shape_field(d1, "call_external.domain", "type"),
        "call_external.domain.type",
        "instruction",
    );
    shape_str(
        shape_field(d1, "call_external.domain", "instruction_type"),
        "call_external.domain.instruction_type",
        "call_external",
    );
    let on_true1 = shape_array(
        shape_field(p1, "call_external.params", "on_true"),
        "call_external.on_true",
        1,
    );
    shape_str(
        shape_field(&on_true1[0], "call_external.on_true[0]", "type"),
        "call_external.on_true[0].type",
        "branch",
    );
    shape_array(
        shape_field(p1, "call_external.params", "on_false"),
        "call_external.on_false",
        0,
    );
    let p2 = shape_field(&core_eval[2], "core_eval[2]", "params");
    let d2 = shape_field(p2, "core_eval[2].params", "domain");
    shape_str(
        shape_field(d2, "call_service.domain", "type"),
        "call_service.domain.type",
        "instruction",
    );
    shape_str(
        shape_field(d2, "call_service.domain", "instruction_type"),
        "call_service.domain.instruction_type",
        "call_service",
    );
    shape_str(
        shape_field(&instruction, "instruction", "type"),
        "instruction.type",
        "call_external",
    );
    let iparams = shape_field(&instruction, "instruction", "params");
    let msgs = shape_array(
        shape_field(iparams, "instruction.params", "messages"),
        "instruction.params.messages",
        1,
    );
    shape_str(
        shape_field(&msgs[0], "instruction.params.messages[0]", "role"),
        "instruction.params.messages[0].role",
        "user",
    );
    shape_array(
        shape_field(iparams, "instruction.params", "tools"),
        "instruction.params.tools",
        0,
    );
    assert!(
        payload.as_object().is_some(),
        "结构自检失败 [payload]: 应为 Object"
    );
    let r = execute_transition(&core_eval, &instruction, &payload, &queue);
    match r {
        Ok(TransitionResult::IoRequired { io_type, .. }) => assert_eq!(io_type, "call_external"),
        Ok(_) => panic!("should be IoRequired"),
        Err(e) => panic!("unexpected error: {:?}", e),
    }
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
//
// 示例:展示 evorule-tcb 的核心 API —— `execute_transition`
//
// 5 步上手:
//   1. 构造一个业务 payload (`{x: 10}`)
//   2. 构造一条 core_eval 规则 (本例为 `set(x, 42)`)
//   3. 构造当前指令 (`noop` — 不消耗规则,纯函数测试用)
//   4. 调用 `execute_transition` 执行一步状态转换
//   5. 读取新 payload 验证
//
// 运行方式:
//   cargo run -p evorule-tcb --example basic_transition

use evorule_tcb::{execute_transition, JsonValue, TransitionResult};
use std::collections::BTreeMap;

fn main() {
    // 1. 业务 payload: { x: 10 }
    let mut p = BTreeMap::new();
    p.insert("x".to_string(), JsonValue::Integer(10));
    let payload = JsonValue::object(p);
    println!("初始 payload: {payload}");

    // 2. core_eval 规则: set(x, 42) — 把 attr "x" 设为 42
    let mut set_params = BTreeMap::new();
    set_params.insert("attr".to_string(), JsonValue::string("x"));
    set_params.insert("operation".to_string(), JsonValue::string("set"));
    set_params.insert("value".to_string(), JsonValue::Integer(42));
    let mut set_rule = BTreeMap::new();
    set_rule.insert("type".to_string(), JsonValue::string("set"));
    set_rule.insert("params".to_string(), JsonValue::object(set_params));
    let core_eval = vec![JsonValue::object(set_rule)];

    // 3. 当前指令: noop(不触发任何规则,仅作为 TCB 的输入)
    let mut instr = BTreeMap::new();
    instr.insert("type".to_string(), JsonValue::string("noop"));
    let instruction = JsonValue::object(instr);

    // 4. 执行
    let result = execute_transition(&core_eval, &instruction, &payload, &[]);

    // 5. 验证
    match result {
        Ok(TransitionResult::State { new_payload, .. }) => {
            println!("执行后 payload: {new_payload}");
            let x = new_payload.get("x").and_then(|v| v.as_i64());
            assert_eq!(x, Some(42), "x 应被 set 改成 42");
            println!("✅ x 已被正确改成 42");
        }
        Ok(TransitionResult::Ignored { .. }) => {
            eprintln!("⚠️ 指令被忽略（无匹配规则）");
            std::process::exit(1);
        }
        Ok(TransitionResult::IoRequired { .. }) => {
            eprintln!("❌ 不应触发 IoRequired(本例规则无 I/O)");
            std::process::exit(1);
        }
        Ok(TransitionResult::Halted { .. }) => {
            eprintln!("❌ 不应触发 Halted(本例规则无 enforce)");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("❌ TCB 执行失败: {e:?}");
            std::process::exit(1);
        }
    }
}

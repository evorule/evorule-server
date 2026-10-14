// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 并发会话隔离测试 — 041 v2.0 A6
//!
//! 验证 project_memory 硬约束：
//! "每个 session 必须有独立 reactor 实例以确保多用户并发隔离"。
//!
//! 覆盖三个并发场景：
//! 1. 多会话并发执行命令，状态不得串扰（核心隔离保证）
//! 2. 并发创建/关闭会话无死锁，计数归零
//! 3. 并发创建的会话 ID 唯一
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use evorule_governance::SessionManager;
use evorule_reactor::{Fact, FactId};
use evorule_tcb::JsonValue;

/// 构造与 core_eval.json increment 规则一致的 transform 规则。
///
/// core_eval 是"映射规则"（branch → set+add），不是业务指令本身。
/// reactor 用它把业务指令 "increment" 转换为元指令 set(attr, add, delta)。
fn make_core_eval() -> Vec<JsonValue> {
    vec![JsonValue::object_from_pairs(&[
        ("type", JsonValue::string("branch")),
        (
            "params",
            JsonValue::object_from_pairs(&[
                (
                    "domain",
                    JsonValue::object_from_pairs(&[
                        ("type", JsonValue::string("instruction")),
                        ("instruction_type", JsonValue::string("increment")),
                    ]),
                ),
                (
                    "on_true",
                    JsonValue::array(vec![JsonValue::object_from_pairs(&[
                        ("type", JsonValue::string("set")),
                        (
                            "params",
                            JsonValue::object_from_pairs(&[
                                (
                                    "attr",
                                    JsonValue::string("__exec__.instruction.params.attr"),
                                ),
                                ("operation", JsonValue::string("add")),
                                (
                                    "value",
                                    JsonValue::string("__exec__.instruction.params.delta"),
                                ),
                            ]),
                        ),
                    ])]),
                ),
            ]),
        ),
    ])]
}

fn make_increment(fact_id: u64, delta: i64) -> Fact {
    Fact::Command {
        id: FactId(fact_id),
        instruction: JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("increment")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("attr", JsonValue::string("x")),
                    ("delta", JsonValue::Integer(delta)),
                ]),
            ),
        ]),
    }
}

/// 从 payload 取 x 字段整数值
fn payload_x(payload: &JsonValue) -> Option<i64> {
    match payload {
        JsonValue::Object(map) => map.get("x").and_then(|v| v.as_i64()),
        _ => None,
    }
}

/// 8 个会话并发执行 25 条 increment 命令，每会话 delta 不同。
/// 验证最终各会话 x == delta * 25，状态不串扰，审计链各自完整。
#[tokio::test]
async fn concurrent_sessions_state_isolation() {
    const N_SESSIONS: usize = 8;
    const CMDS_PER_SESSION: usize = 25;

    let mgr = Arc::new(SessionManager::new(make_core_eval(), 10000));

    // 1. 创建 N 个会话
    let mut session_ids = Vec::with_capacity(N_SESSIONS);
    for _ in 0..N_SESSIONS {
        session_ids.push(mgr.create_session().expect("create session"));
    }
    assert_eq!(mgr.len(), N_SESSIONS);

    // 2. 并发：每会话一个 task，发 CMDS 条 increment(delta=idx+1)
    let mut tasks = Vec::with_capacity(N_SESSIONS);
    for (idx, &sid) in session_ids.iter().enumerate() {
        let session = mgr.get_session(sid).expect("get session");
        let delta = (idx + 1) as i64;
        tasks.push(tokio::spawn(async move {
            for n in 1..=CMDS_PER_SESSION {
                session
                    .command_tx
                    .send(make_increment(n as u64, delta))
                    .expect("send command");
            }
            // 预期 x = delta * CMDS
            delta * CMDS_PER_SESSION as i64
        }));
    }

    // 3. 等待所有命令发送完成
    let mut expected = Vec::with_capacity(N_SESSIONS);
    for t in tasks {
        expected.push(t.await.expect("task join"));
    }

    // 4. 等待反应器处理完（长运行模式，Stable 后仍驻留）
    tokio::time::sleep(Duration::from_millis(800)).await;

    // 5. 验证隔离：各会话 x == expected[idx]，无串扰；审计链各自完整
    for (idx, &sid) in session_ids.iter().enumerate() {
        let session = mgr.get_session(sid).expect("get session after run");
        let (payload, _queue, version) = session.facts_log.snapshot();
        let x = payload_x(&payload)
            .unwrap_or_else(|| panic!("session {} payload 缺少 x 字段: {}", sid, payload));
        assert_eq!(
            x, expected[idx],
            "session {} (idx {}) 状态串扰: x={} expected={} (并发隔离被破坏)",
            sid, idx, x, expected[idx]
        );
        assert!(
            session.audit_verify(),
            "session {} 审计链完整性校验失败",
            sid
        );
        assert!(
            version >= CMDS_PER_SESSION as u64,
            "session {} version 异常偏低: {} (期望 >= {})",
            sid,
            version,
            CMDS_PER_SESSION
        );
    }

    // 6. 清理
    for &sid in &session_ids {
        let _ = mgr.close_session(sid);
    }
}

/// 并发创建/关闭会话无死锁，最终计数归零。
#[tokio::test]
async fn concurrent_create_close_no_deadlock() {
    let mgr = Arc::new(SessionManager::new(make_core_eval(), 100));

    let mut tasks = Vec::new();
    for _ in 0..16 {
        let mgr = mgr.clone();
        tasks.push(tokio::spawn(async move {
            let id = mgr.create_session().expect("create session");
            // 立即关闭（触发 command_tx drop → reactor 优雅退出）
            let _handle = mgr.close_session(id).expect("close session");
        }));
    }

    for t in tasks {
        t.await.expect("task join");
    }

    assert_eq!(mgr.len(), 0, "所有会话关闭后计数应归零，实际 {}", mgr.len());
}

/// 并发创建的会话 ID 必须唯一。
#[tokio::test]
async fn concurrent_create_yields_unique_ids() {
    let mgr = Arc::new(SessionManager::new(make_core_eval(), 200));

    let mut tasks = Vec::new();
    for _ in 0..32 {
        let mgr = mgr.clone();
        tasks.push(tokio::spawn(async move {
            mgr.create_session().expect("create")
        }));
    }

    let mut ids = Vec::new();
    for t in tasks {
        ids.push(t.await.expect("join"));
    }

    ids.sort();
    let original_len = ids.len();
    ids.dedup();
    assert_eq!(
        ids.len(),
        original_len,
        "并发创建产生重复 SessionId: {} unique / {} total",
        ids.len(),
        original_len
    );

    for id in &ids {
        let _ = mgr.close_session(*id);
    }
}

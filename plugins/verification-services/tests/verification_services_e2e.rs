// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 逻辑验证原生服务集成测试：
//! ① SAT 黄金向量（手算可溯:DPLL 首现序+正枝先行的确定性期望 / 鸽笼 UNSAT 完备判定）
//! ② FSM 黄金向量（教科书式:witness 路径 / bounded 受限语义 / 等价与最短区分反例）
//! ③ 服务契约（缺参 fail-fast 点名 / 未知服务诚实错误 / 输出信封保留键）
//! ④ 确定性（同输入两次执行逐位一致）

#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试面:断言可读性优先,机制同 plugin-kit 测试桩

use evorule_reactor::IoHandler;
use evorule_tcb::JsonValue;
use evorule_verification_services::{VerificationServiceRouter, NATIVE_SERVICES};

fn svc_params(service_name: &str, args: JsonValue) -> JsonValue {
    JsonValue::object_from_pairs(&[
        ("service_name", JsonValue::string(service_name)),
        ("args", args),
    ])
}

fn noop_fallback() -> std::sync::Arc<dyn IoHandler> {
    struct Noop;
    #[async_trait::async_trait]
    impl IoHandler for Noop {
        async fn execute(&self, _params: &JsonValue) -> evorule_reactor::IoResult {
            Err("fallback-not-expected".to_string())
        }
    }
    std::sync::Arc::new(Noop)
}

fn int(n: i64) -> JsonValue {
    JsonValue::Integer(n)
}

fn ints(v: &[i64]) -> JsonValue {
    JsonValue::array(v.iter().map(|&n| int(n)).collect())
}

fn int_matrix(rows: &[&[i64]]) -> JsonValue {
    JsonValue::array(rows.iter().map(|r| ints(r)).collect())
}

// ── ① SAT 黄金向量 ──

/// 手算可溯:F = (x1∨x2)∧(¬x1∨x3)∧(¬x2∨¬x3)。
/// DPLL(首现序+正枝先行):x1=true → 子句2 变单元 x3=true → 子句3 变单元 x2=false
/// → 三子句全满足,SAT。期望指派 [true,false,true](变量 1..n 升序)。
#[tokio::test]
async fn sat_golden_unit_propagation_trace() {
    let router = VerificationServiceRouter::new(noop_fallback());
    let args = JsonValue::object_from_pairs(&[
        ("vars", int(3)),
        (
            "clauses",
            JsonValue::array(vec![ints(&[1, 2]), ints(&[-1, 3]), ints(&[-2, -3])]),
        ),
    ]);
    let out = router
        .execute(&svc_params("verification_sat_solve", args))
        .await
        .unwrap();
    assert_eq!(out.get("status").unwrap(), &JsonValue::string("determined"));
    assert_eq!(
        out.get("satisfiable").unwrap(),
        &JsonValue::Bool(true),
        "手算 SAT: x1=T,x3=T(单元),x2=F(单元)"
    );
    assert_eq!(
        out.get("assignment").unwrap(),
        &JsonValue::array(vec![
            JsonValue::Bool(true),
            JsonValue::Bool(false),
            JsonValue::Bool(true)
        ]),
        "指派按变量 1..n 升序"
    );
}

/// 鸽笼原理 P(3,2) 的完全图化身:三变量两两「至少一真∧至多一真」→ 恰一真
/// → 2·Σ = 3 无整数解 → UNSAT。DPLL 穷尽搜索空间后如实报完备判定。
#[tokio::test]
async fn sat_golden_pigeonhole_unsat() {
    let router = VerificationServiceRouter::new(noop_fallback());
    let args = JsonValue::object_from_pairs(&[
        ("vars", int(3)),
        (
            "clauses",
            JsonValue::array(vec![
                ints(&[1, 2]),
                ints(&[1, 3]),
                ints(&[2, 3]), // 两两至少一真
                ints(&[-1, -2]),
                ints(&[-1, -3]),
                ints(&[-2, -3]), // 两两至多一真
            ]),
        ),
    ]);
    let out = router
        .execute(&svc_params("verification_sat_solve", args))
        .await
        .unwrap();
    assert_eq!(
        out.get("satisfiable").unwrap(),
        &JsonValue::Bool(false),
        "P(3,2) 鸽笼:UNSAT 为完备判定(DPLL 穷尽)"
    );
}

/// 空子句=恒假:含空子句的公式必 UNSAT(语法保留语义,不报错)。
#[tokio::test]
async fn sat_empty_clause_is_unsat() {
    let router = VerificationServiceRouter::new(noop_fallback());
    let args = JsonValue::object_from_pairs(&[
        ("vars", int(1)),
        (
            "clauses",
            JsonValue::array(vec![ints(&[1]), JsonValue::array(vec![])]),
        ),
    ]);
    let out = router
        .execute(&svc_params("verification_sat_solve", args))
        .await
        .unwrap();
    assert_eq!(out.get("satisfiable").unwrap(), &JsonValue::Bool(false));
}

#[tokio::test]
async fn sat_missing_var_fails_fast_with_guidance() {
    let router = VerificationServiceRouter::new(noop_fallback());
    let args = JsonValue::object_from_pairs(&[("clauses", JsonValue::array(vec![]))]);
    let err = router
        .execute(&svc_params("verification_sat_solve", args))
        .await
        .unwrap_err();
    assert!(err.contains("vars"), "错误应点名缺失参数, got: {err}");
}

/// 资源守卫:超变量上限 fail-visible 拒解(不静默降级)。
#[tokio::test]
async fn sat_over_limit_fails_visible() {
    let router = VerificationServiceRouter::new(noop_fallback());
    let args =
        JsonValue::object_from_pairs(&[("vars", int(201)), ("clauses", JsonValue::array(vec![]))]);
    let err = router
        .execute(&svc_params("verification_sat_solve", args))
        .await
        .unwrap_err();
    assert!(err.contains("上限"), "超限应如实拒解, got: {err}");
}

// ── ② FSM 黄金向量 ──

/// 教科书翻转机:2 状态 1 符号,0→1→0…,接受 {1}。
/// targets=[1]:1 步可达,witness word=[0] path=[0,1];语言非空,最短接受 1 步。
#[tokio::test]
async fn fsm_reachability_golden_witness() {
    let router = VerificationServiceRouter::new(noop_fallback());
    let args = JsonValue::object_from_pairs(&[
        ("states", int(2)),
        ("symbols", int(1)),
        ("transitions", int_matrix(&[&[1], &[0]])),
        ("accepting", ints(&[1])),
        ("initial", ints(&[0])),
        ("targets", ints(&[1])),
    ]);
    let out = router
        .execute(&svc_params("verification_fsm_reachability", args))
        .await
        .unwrap();
    assert_eq!(
        out.get("targets_reachable").unwrap(),
        &JsonValue::Bool(true)
    );
    assert_eq!(out.get("bounded").unwrap(), &JsonValue::Bool(false));
    assert_eq!(out.get("language_empty").unwrap(), &JsonValue::Bool(false));
    assert_eq!(out.get("min_accept_steps").unwrap(), &int(1));
    let witness = out.get("witness").unwrap();
    assert_eq!(witness.get("steps").unwrap(), &int(1));
    assert_eq!(witness.get("start").unwrap(), &int(0));
    assert_eq!(witness.get("word").unwrap(), &ints(&[0]));
    assert_eq!(witness.get("path").unwrap(), &ints(&[0, 1]));
}

/// bounded 受限语义:max_steps=0 时 1 步目标未达 → reachable=false 且
/// bounded=true 如实标注(未达≠不可达);language_empty 恒全空间=非空。
#[tokio::test]
async fn fsm_reachability_bounded_honest_semantics() {
    let router = VerificationServiceRouter::new(noop_fallback());
    let args = JsonValue::object_from_pairs(&[
        ("states", int(2)),
        ("symbols", int(1)),
        ("transitions", int_matrix(&[&[1], &[0]])),
        ("accepting", ints(&[1])),
        ("initial", ints(&[0])),
        ("targets", ints(&[1])),
        ("max_steps", int(0)),
    ]);
    let out = router
        .execute(&svc_params("verification_fsm_reachability", args))
        .await
        .unwrap();
    assert_eq!(
        out.get("targets_reachable").unwrap(),
        &JsonValue::Bool(false),
        "0 步探查看不到 1 步目标"
    );
    assert_eq!(out.get("bounded").unwrap(), &JsonValue::Bool(true));
    assert_eq!(
        out.get("language_empty").unwrap(),
        &JsonValue::Bool(false),
        "语言空性不受上界约束,恒完备"
    );
}

/// 不完全转移表 fail-visible 拒解(不做隐式补全)。
#[tokio::test]
async fn fsm_incomplete_table_fails_visible() {
    let router = VerificationServiceRouter::new(noop_fallback());
    let args = JsonValue::object_from_pairs(&[
        ("states", int(2)),
        ("symbols", int(2)),
        ("transitions", int_matrix(&[&[1], &[0]])), // 每行 1 列≠2 符号
        ("accepting", ints(&[1])),
        ("initial", ints(&[0])),
        ("targets", ints(&[1])),
    ]);
    let err = router
        .execute(&svc_params("verification_fsm_reachability", args))
        .await
        .unwrap_err();
    assert!(
        err.contains("fail-visible") || err.contains("列数"),
        "不完全表须拒解, got: {err}"
    );
}

/// 等价·教科书:翻转机 M1 与「M1+不可达接受态」M3 语言相同 → 等价
/// (不可达状态不影响语言等价;乘积机全遍历无区分态)。
#[tokio::test]
async fn fsm_equivalence_golden_equivalent_with_unreachable() {
    let router = VerificationServiceRouter::new(noop_fallback());
    // M1: 2 状态,sym0 翻转/sym1 自环,接受 {1} → 奇数个 sym0 的词
    // M3: 3 状态,前两态同 M1,状态 2 不可达且接受 → 语言同 M1
    let args = JsonValue::object_from_pairs(&[
        ("a_states", int(2)),
        ("a_symbols", int(2)),
        ("a_transitions", int_matrix(&[&[1, 0], &[0, 1]])),
        ("a_accepting", ints(&[1])),
        ("b_states", int(3)),
        ("b_symbols", int(2)),
        ("b_transitions", int_matrix(&[&[1, 0], &[0, 1], &[1, 1]])),
        ("b_accepting", ints(&[1, 2])),
    ]);
    let out = router
        .execute(&svc_params("verification_fsm_equivalence", args))
        .await
        .unwrap();
    assert_eq!(
        out.get("equivalent").unwrap(),
        &JsonValue::Bool(true),
        "不可达接受态不改变语言,乘积全遍历应无区分"
    );
}

/// 等价·反例:同构机但初始态接受性不同 → 空词即最短区分词;
/// 两机轨迹各含起点与终态,终态接受性互补(反例可机械复核)。
#[tokio::test]
async fn fsm_equivalence_golden_counterexample() {
    let router = VerificationServiceRouter::new(noop_fallback());
    // A: 翻转机接受 {1}(奇 sym0);B: 恒 0 机接受 {0}(仅空串)
    let args = JsonValue::object_from_pairs(&[
        ("a_states", int(2)),
        ("a_symbols", int(1)),
        ("a_transitions", int_matrix(&[&[1], &[0]])),
        ("a_accepting", ints(&[1])),
        ("b_states", int(1)),
        ("b_symbols", int(1)),
        ("b_transitions", int_matrix(&[&[0]])),
        ("b_accepting", ints(&[0])),
    ]);
    let out = router
        .execute(&svc_params("verification_fsm_equivalence", args))
        .await
        .unwrap();
    assert_eq!(out.get("equivalent").unwrap(), &JsonValue::Bool(false));
    assert_eq!(
        out.get("counterexample_word").unwrap(),
        &ints(&[]),
        "空词即区分:初始态接受性 A=F B=T"
    );
    assert_eq!(out.get("end_a_accepting").unwrap(), &JsonValue::Bool(false));
    assert_eq!(out.get("end_b_accepting").unwrap(), &JsonValue::Bool(true));
    // 两机轨迹含反例词走后的完整路径(起点+每步终态)
    assert_eq!(out.get("path_a").unwrap(), &ints(&[0]));
    assert_eq!(out.get("path_b").unwrap(), &ints(&[0]));
}

/// 非空区分词反例(单符号可达不一致):
/// A: 0→1(接受),B: 0→0(不接受) → 词 [0] 区分。
#[tokio::test]
async fn fsm_equivalence_shortest_word_one_step() {
    let router = VerificationServiceRouter::new(noop_fallback());
    let args = JsonValue::object_from_pairs(&[
        ("a_states", int(2)),
        ("a_symbols", int(1)),
        ("a_transitions", int_matrix(&[&[1], &[1]])),
        ("a_accepting", ints(&[1])),
        ("b_states", int(2)),
        ("b_symbols", int(1)),
        ("b_transitions", int_matrix(&[&[0], &[0]])),
        ("b_accepting", ints(&[1])),
    ]);
    let out = router
        .execute(&svc_params("verification_fsm_equivalence", args))
        .await
        .unwrap();
    assert_eq!(out.get("equivalent").unwrap(), &JsonValue::Bool(false));
    assert_eq!(
        out.get("counterexample_word").unwrap(),
        &ints(&[0]),
        "最短区分词 [0]:A 终态 1 接受,B 终态 0 不接受"
    );
    assert_eq!(out.get("path_a").unwrap(), &ints(&[0, 1]));
    assert_eq!(out.get("path_b").unwrap(), &ints(&[0, 0]));
}

// ── ③④ 契约与确定性 ──

#[tokio::test]
async fn unknown_service_falls_back_to_http_per_framework_contract() {
    // 框架契约(plugin-kit router 自带测试同源):未知 service_name → 回落 fallback
    // (HTTP ServiceRegistryHandler 兜底),路由器不截胡。哨兵报错即证实回落。
    let router = VerificationServiceRouter::new(noop_fallback());
    let params = svc_params("verification_nonexistent", JsonValue::empty_object());
    let err = router.execute(&params).await.unwrap_err();
    assert_eq!(
        err, "fallback-not-expected",
        "未知服务应原样透传 fallback 错误(回落 HTTP 契约)"
    );
}

#[tokio::test]
async fn envelope_has_reserved_keys_on_all_services() {
    let router = VerificationServiceRouter::new(noop_fallback());
    // 逐服务喂一份最小合法输入,断言保留键打头且 determinism 恒位元可重放
    let cases: Vec<(&str, JsonValue)> = vec![
        (
            "verification_sat_solve",
            JsonValue::object_from_pairs(&[
                ("vars", int(1)),
                ("clauses", JsonValue::array(vec![ints(&[1])])),
            ]),
        ),
        (
            "verification_fsm_reachability",
            JsonValue::object_from_pairs(&[
                ("states", int(1)),
                ("symbols", int(1)),
                ("transitions", int_matrix(&[&[0]])),
                ("accepting", ints(&[0])),
                ("initial", ints(&[0])),
                ("targets", ints(&[0])),
            ]),
        ),
        (
            "verification_fsm_equivalence",
            JsonValue::object_from_pairs(&[
                ("a_states", int(1)),
                ("a_symbols", int(1)),
                ("a_transitions", int_matrix(&[&[0]])),
                ("a_accepting", ints(&[0])),
                ("b_states", int(1)),
                ("b_symbols", int(1)),
                ("b_transitions", int_matrix(&[&[0]])),
                ("b_accepting", ints(&[0])),
            ]),
        ),
    ];
    for (name, args) in cases {
        let out = router
            .execute(&svc_params(name, args))
            .await
            .map_err(|e| format!("服务 {name} 最小合法输入不应失败: {e}"))
            .unwrap();
        assert_eq!(
            out.get("status").unwrap(),
            &JsonValue::string("determined"),
            "{name}"
        );
        assert_eq!(
            out.get("determinism").unwrap(),
            &JsonValue::string("bitwise_replayable"),
            "{name}"
        );
    }
}

#[tokio::test]
async fn deterministic_bitwise_replay() {
    let router = VerificationServiceRouter::new(noop_fallback());
    let args = JsonValue::object_from_pairs(&[
        ("vars", int(4)),
        (
            "clauses",
            JsonValue::array(vec![
                ints(&[1, 2, 3]),
                ints(&[-1, 4]),
                ints(&[-2, -4]),
                ints(&[2, 3, -4]),
            ]),
        ),
    ]);
    let params = svc_params("verification_sat_solve", args);
    let a = router.execute(&params).await.unwrap();
    let b = router.execute(&params).await.unwrap();
    assert_eq!(a, b, "同输入两次执行须逐位一致(含 note 文本与键序)");
}

#[test]
fn declaration_table_has_three_services() {
    let names: Vec<&str> = NATIVE_SERVICES.iter().map(|d| d.name).collect();
    assert_eq!(
        names,
        vec![
            "verification_sat_solve",
            "verification_fsm_reachability",
            "verification_fsm_equivalence"
        ]
    );
}

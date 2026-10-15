// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 推理分析原生服务集成测试：
//! ① 服务契约（入参解析 fail-fast / 输出结构）
//! ② 算法预言（与上游参考实现测试同源的期望值,经服务层验证）
//! ③ 路由行为（原生命中 / 未知服务诚实错误 / 启用子集三拒绝）
//! ④ 确定性（同输入两次执行逐位一致）

#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试面:断言可读性优先,机制同 plugin-kit 测试桩

use evorule_inference_services::{InferenceServiceRouter, NATIVE_SERVICES};
use evorule_reactor::IoHandler;
use evorule_tcb::JsonValue;

fn svc_params(service_name: &str, args: JsonValue) -> JsonValue {
    JsonValue::object_from_pairs(&[
        ("service_name", JsonValue::string(service_name)),
        ("args", args),
    ])
}

fn str_val(s: &str) -> JsonValue {
    JsonValue::string(s)
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

// ── ① 单位解析服务契约 ──

#[tokio::test]
async fn from_unit_parses_base_units() {
    let router = InferenceServiceRouter::new(noop_fallback());
    let args = JsonValue::object_from_pairs(&[(
        "units",
        JsonValue::Array(vec![str_val("m"), str_val("kg"), str_val("s")]),
    )]);
    let out = router
        .execute(&svc_params("inference_dimension_from_unit", args))
        .await
        .unwrap();

    let results = out.get("results").unwrap();
    let items = results.as_array().expect("results 须为数组");
    assert_eq!(items.len(), 3);
    // m → [1,0,0,0,0,0,0]
    assert_eq!(items[0].get("ok").unwrap(), &JsonValue::Bool(true));
    assert_eq!(
        items[0].get("dimension").unwrap(),
        &JsonValue::Array(vec![
            JsonValue::Integer(1),
            JsonValue::Integer(0),
            JsonValue::Integer(0),
            JsonValue::Integer(0),
            JsonValue::Integer(0),
            JsonValue::Integer(0),
            JsonValue::Integer(0),
        ])
    );
}

#[tokio::test]
async fn from_unit_reports_unknown_honestly() {
    let router = InferenceServiceRouter::new(noop_fallback());
    let args = JsonValue::object_from_pairs(&[(
        "units",
        JsonValue::Array(vec![str_val("xyz"), str_val("")]),
    )]);
    let out = router
        .execute(&svc_params("inference_dimension_from_unit", args))
        .await
        .unwrap();
    let items = out
        .get("results")
        .unwrap()
        .as_array()
        .expect("results 须为数组");
    assert_eq!(items[0].get("ok").unwrap(), &JsonValue::Bool(false)); // xyz 未知
    assert_eq!(items[1].get("ok").unwrap(), &JsonValue::Bool(true)); // 空 = 无量纲
}

#[tokio::test]
async fn from_unit_missing_param_fails_fast_with_guidance() {
    let router = InferenceServiceRouter::new(noop_fallback());
    let args = JsonValue::object_from_pairs(&[("other", JsonValue::Array(vec![]))]);
    let err = router
        .execute(&svc_params("inference_dimension_from_unit", args))
        .await
        .unwrap_err();
    assert!(err.contains("units"), "错误应点名缺失参数, got: {err}");
    assert!(err.contains("自诊断指引"), "错误应含自诊断指引, got: {err}");
}

// ── ② 比较服务 ──

#[tokio::test]
async fn compare_units_equivalence() {
    let router = InferenceServiceRouter::new(noop_fallback());
    let args = JsonValue::object_from_pairs(&[(
        "pairs",
        JsonValue::Array(vec![
            JsonValue::Array(vec![str_val("N"), str_val("kg·m/s²")]), // 等价
            JsonValue::Array(vec![str_val("m"), str_val("kg")]),      // 不等价
            JsonValue::Array(vec![str_val("m"), str_val("xyz")]),     // 右侧未知
        ]),
    )]);
    let out = router
        .execute(&svc_params("inference_dimension_compare_units", args))
        .await
        .unwrap();
    let items = out
        .get("results")
        .unwrap()
        .as_array()
        .expect("results 须为数组");
    assert_eq!(items[0].get("compatible").unwrap(), &JsonValue::Bool(true));
    assert_eq!(items[1].get("compatible").unwrap(), &JsonValue::Bool(false));
    assert_eq!(items[2].get("compatible").unwrap(), &JsonValue::Bool(false));
    assert_eq!(items[2].get("right_dimension").unwrap(), &JsonValue::Null);
}

// ── ③ 公式验证服务（预言与上游参考实现测试同源） ──

async fn verify(formulas: &[&str]) -> Vec<JsonValue> {
    let router = InferenceServiceRouter::new(noop_fallback());
    let mut si = Vec::new();
    for (k, v) in [
        ("F", "N"),
        ("m", "kg"),
        ("a", "m/s²"),
        ("v", "m/s"),
        ("d", "m"),
        ("t", "s"),
        ("P", "W"),
        ("A", "m²"),
        ("T", "K"),
        ("V", "m³"),
    ] {
        si.push((k, str_val(v)));
    }
    let args = JsonValue::object_from_pairs(&[
        ("si_units", JsonValue::object_from_pairs(&si)),
        (
            "formulas",
            JsonValue::Array(formulas.iter().map(|f| str_val(f)).collect()),
        ),
    ]);
    let out = router
        .execute(&svc_params("inference_dimension_verify_formula", args))
        .await
        .unwrap();
    match out.get("results").unwrap() {
        JsonValue::Array(items) => items.clone(),
        _ => std::unreachable!("results 须为数组"),
    }
}

#[tokio::test]
async fn formula_newton2_passes() {
    let r = verify(&["F == m * a"]).await;
    assert_eq!(r[0].get("passed").unwrap(), &JsonValue::Bool(true));
}

#[tokio::test]
async fn formula_mismatch_fails() {
    // F = m*v 量纲不一致（力 vs 动量）
    let r = verify(&["F == m * v"]).await;
    assert_eq!(r[0].get("passed").unwrap(), &JsonValue::Bool(false));
    let msg = r[0].get("message").unwrap().as_str().unwrap_or_default();
    assert!(msg.contains("不一致"), "got: {msg}");
}

#[tokio::test]
async fn formula_velocity_division_passes() {
    let r = verify(&["v == d / t"]).await;
    assert_eq!(r[0].get("passed").unwrap(), &JsonValue::Bool(true));
}

#[tokio::test]
async fn formula_subtraction_dimension_mismatch_fails() {
    // x - y 同量纲相减应通过（长度-长度）;这里混入质量→失败面用独立调用
    let router = InferenceServiceRouter::new(noop_fallback());
    let args = JsonValue::object_from_pairs(&[
        (
            "si_units",
            JsonValue::object_from_pairs(&[
                ("x", str_val("m")),
                ("y", str_val("kg")),
                ("z", str_val("m")),
            ]),
        ),
        ("formulas", JsonValue::Array(vec![str_val("z == x - y")])),
    ]);
    let out = router
        .execute(&svc_params("inference_dimension_verify_formula", args))
        .await
        .unwrap();
    let items = out
        .get("results")
        .unwrap()
        .as_array()
        .expect("results 须为数组");
    assert_eq!(items[0].get("passed").unwrap(), &JsonValue::Bool(false));
}

#[tokio::test]
async fn formula_unknown_variable_reports_honestly() {
    let r = verify(&["F == q * a"]).await; // q 未登记
    assert_eq!(r[0].get("passed").unwrap(), &JsonValue::Bool(false));
    let msg = r[0].get("message").unwrap().as_str().unwrap_or_default();
    assert!(
        msg.contains("无法推导"),
        "未知变量应如实报无法推导, got: {msg}"
    );
}

#[tokio::test]
async fn formula_bad_format_reports_honestly() {
    let r = verify(&["F m a"]).await; // 无分隔符
    assert_eq!(r[0].get("passed").unwrap(), &JsonValue::Bool(false));
    let msg = r[0].get("message").unwrap().as_str().unwrap_or_default();
    assert!(msg.contains("格式错误"), "got: {msg}");
}

#[tokio::test]
async fn formula_missing_si_units_all_unknown() {
    let router = InferenceServiceRouter::new(noop_fallback());
    let args = JsonValue::object_from_pairs(&[(
        "formulas",
        JsonValue::Array(vec![str_val("F == m * a")]),
    )]);
    let out = router
        .execute(&svc_params("inference_dimension_verify_formula", args))
        .await
        .unwrap();
    let items = out
        .get("results")
        .unwrap()
        .as_array()
        .expect("results 须为数组");
    // si_units 缺省 = 空映射 → 两侧均无法推导（宽松缺省,结果如实）
    assert_eq!(items[0].get("passed").unwrap(), &JsonValue::Bool(false));
}

// ── ④ 路由行为 ──

#[tokio::test]
async fn unknown_service_falls_back_and_fallback_errors_honestly() {
    // 机制:未命中原生服务名 → 整个 params 透传回落(真实部署中回落为
    // ServiceRegistryHandler,由它报 unknown service_name);测试桩回落恒错,
    // 错误出现 = 透传确实发生了(未被原生拦截)
    let router = InferenceServiceRouter::new(noop_fallback());
    let err = router
        .execute(&svc_params("no_such_service", JsonValue::Null))
        .await
        .unwrap_err();
    assert_eq!(
        err, "fallback-not-expected",
        "未知服务应透传回落, got: {err}"
    );
}

#[tokio::test]
async fn enabled_subset_accepts_listed_rejects_unlisted() {
    let router =
        InferenceServiceRouter::with_enabled(noop_fallback(), &["inference_dimension_from_unit"])
            .unwrap();
    let args = JsonValue::object_from_pairs(&[("units", JsonValue::Array(vec![str_val("m")]))]);
    let ok = router
        .execute(&svc_params("inference_dimension_from_unit", args))
        .await;
    assert!(ok.is_ok());

    // 未启用服务不进原生路由查找 → 落到回落(真实部署由回落诚实报 unknown service_name)
    let err = router
        .execute(&svc_params(
            "inference_dimension_verify_formula",
            JsonValue::Null,
        ))
        .await
        .unwrap_err();
    assert_eq!(
        err, "fallback-not-expected",
        "未启用服务应落回落而非原生执行, got: {err}"
    );
}

#[test]
fn with_enabled_rejects_unknown_and_duplicate_and_empty() {
    let take_err = |r: Result<InferenceServiceRouter, String>| match r {
        Err(e) => e,
        Ok(_) => std::unreachable!("预期失败但成功了"),
    };
    // 未知名
    let e1 = take_err(InferenceServiceRouter::with_enabled(
        noop_fallback(),
        &["nope"],
    ));
    assert!(e1.contains("未注册"), "got: {e1}");
    // 重复名
    let e2 = take_err(InferenceServiceRouter::with_enabled(
        noop_fallback(),
        &[
            "inference_dimension_from_unit",
            "inference_dimension_from_unit",
        ],
    ));
    assert!(e2.contains("重复声明"), "got: {e2}");
    // 空启用集
    let e3 = take_err(InferenceServiceRouter::with_enabled(noop_fallback(), &[]));
    assert!(e3.contains("启用集为空"), "got: {e3}");
}

#[test]
fn declaration_table_matches_ssot_file() {
    // 执行侧守卫：声明表与 SSOT 文件三字段+顺序一致（构建期嵌入对比）
    let ssot = include_str!("../official_native_services.json");
    let parsed: serde_json::Value = serde_json::from_str(ssot).unwrap();
    let services = parsed["services"].as_array().unwrap();
    assert_eq!(
        services.len(),
        NATIVE_SERVICES.len(),
        "SSOT 与声明表项数不一致"
    );
    for (i, svc) in services.iter().enumerate() {
        let def = &NATIVE_SERVICES[i];
        assert_eq!(
            svc["name"].as_str().unwrap(),
            def.name,
            "第 {i} 项 name 漂移"
        );
        assert_eq!(
            svc["sensitive"].as_bool().unwrap(),
            def.sensitive,
            "第 {i} 项 sensitive 漂移"
        );
        assert_eq!(
            svc["description"].as_str().unwrap(),
            def.description,
            "第 {i} 项 description 漂移"
        );
    }
}

// ── ⑤ 确定性：同输入两次执行逐位一致 ──

#[tokio::test]
async fn same_input_bit_identical_results() {
    let router = InferenceServiceRouter::new(noop_fallback());
    let args = JsonValue::object_from_pairs(&[
        (
            "si_units",
            JsonValue::object_from_pairs(&[
                ("F", str_val("N")),
                ("m", str_val("kg")),
                ("a", str_val("m/s²")),
                ("sigma", str_val("kg·s⁻³·K⁻⁴")),
            ]),
        ),
        (
            "formulas",
            JsonValue::Array(vec![
                str_val("F == m * a"),
                str_val("F == m * v"),
                str_val("P == sigma * A * T**4"),
            ]),
        ),
    ]);
    let params = svc_params("inference_dimension_verify_formula", args);
    let r1 = router.execute(&params).await.unwrap();
    let r2 = router.execute(&params).await.unwrap();
    assert_eq!(r1.to_string(), r2.to_string(), "同输入两次执行必须逐位一致");
}

#[test]
fn native_service_names_derived_from_table() {
    let names = InferenceServiceRouter::native_service_names();
    assert_eq!(
        names,
        vec![
            "inference_dimension_from_unit",
            "inference_dimension_compare_units",
            "inference_dimension_verify_formula",
            "inference_rw_extract",
            "inference_cycle_detect",
            "inference_conflict_detect",
            "inference_forward_simulate",
            "inference_backward_plan",
            "inference_effect_predict",
            "inference_convergence_check",
        ]
    );
}

// ============================================================================
// 阶段 2：规则静态分析三服务（112 号，设计文档 §五/§六）
// ============================================================================

use std::sync::Arc;

/// 构造 core_eval 形态的 branch+eq+set 规则
fn branch_eq_write(path: &str, eq_val: &str, attr: &str, value: i64) -> JsonValue {
    JsonValue::object_from_pairs(&[
        ("type", JsonValue::string("branch")),
        (
            "params",
            JsonValue::object_from_pairs(&[
                (
                    "domain",
                    JsonValue::object_from_pairs(&[
                        ("type", JsonValue::string("eq")),
                        ("path", JsonValue::string(path)),
                        ("value", JsonValue::string(eq_val)),
                    ]),
                ),
                (
                    "on_true",
                    JsonValue::Array(vec![JsonValue::object_from_pairs(&[
                        ("type", JsonValue::string("set")),
                        (
                            "params",
                            JsonValue::object_from_pairs(&[
                                ("attr", JsonValue::string(attr)),
                                ("value", JsonValue::Integer(value)),
                            ]),
                        ),
                    ])]),
                ),
            ]),
        ),
    ])
}

/// 规则集入参（来源标签自动生成 rules/rN.json）
fn rs_args(rules: Vec<JsonValue>) -> JsonValue {
    let sources: Vec<JsonValue> = (0..rules.len())
        .map(|i| JsonValue::string(format!("rules/r{i}.json")))
        .collect();
    JsonValue::object_from_pairs(&[
        ("rules", JsonValue::Array(rules)),
        ("sources", JsonValue::Array(sources)),
    ])
}

#[tokio::test]
async fn rw_extract_reports_reads_writes_and_flags() {
    let router = InferenceServiceRouter::new(noop_fallback());
    // r0: 静态写 payload.x 常量；r1: 宪法形态动态写（attr 路径引用）
    let rules = vec![
        branch_eq_write("__exec__.payload.flag", "on", "x", 42),
        JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "attr",
                        JsonValue::string("__exec__.instruction.params.attr"),
                    ),
                    ("value", JsonValue::Integer(1)),
                ]),
            ),
        ]),
    ];
    let out = router
        .execute(&svc_params("inference_rw_extract", rs_args(rules)))
        .await
        .unwrap();
    let items = out
        .get("results")
        .and_then(|v| v.as_array())
        .expect("results 数组");
    assert_eq!(items.len(), 2);
    let r0 = &items[0];
    assert_eq!(
        r0.get("id").and_then(|v| v.as_str()),
        Some("rules/r0.json#0")
    );
    let reads0 = r0
        .get("reads")
        .and_then(|v| v.as_array())
        .expect("reads 数组");
    assert!(reads0.iter().any(|v| v.as_str() == Some("payload.flag")));
    let writes0 = r0
        .get("writes")
        .and_then(|v| v.as_array())
        .expect("writes 数组");
    assert!(writes0.iter().any(|v| v.as_str() == Some("payload.x")));
    assert_eq!(r0.get("write_dyn"), Some(&JsonValue::Bool(false)));
    let r1 = &items[1];
    assert_eq!(r1.get("write_dyn"), Some(&JsonValue::Bool(true)));
}

#[tokio::test]
async fn cycle_detect_finds_two_node_static_cycle() {
    let router = InferenceServiceRouter::new(noop_fallback());
    // r0 存在 b 写 a；r1 存在 a 写 b → 静态环
    let mk = |guard: &str, attr: &str| {
        JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("exists")),
                            ("path", JsonValue::string(guard)),
                        ]),
                    ),
                    (
                        "on_true",
                        JsonValue::Array(vec![JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("set")),
                            (
                                "params",
                                JsonValue::object_from_pairs(&[
                                    ("attr", JsonValue::string(attr)),
                                    ("value", JsonValue::Integer(1)),
                                ]),
                            ),
                        ])]),
                    ),
                ]),
            ),
        ])
    };
    let rules = vec![mk("__exec__.payload.b", "a"), mk("__exec__.payload.a", "b")];
    let out = router
        .execute(&svc_params("inference_cycle_detect", rs_args(rules)))
        .await
        .unwrap();
    assert_eq!(out.get("has_cycles"), Some(&JsonValue::Bool(true)));
    assert_eq!(out.get("cycle_count"), Some(&JsonValue::Integer(1)));
    let cycles = out
        .get("static_cycles")
        .and_then(|v| v.as_array())
        .expect("static_cycles 数组");
    assert_eq!(cycles.len(), 1);
    let cycle = cycles[0].as_array().expect("环为数组");
    assert_eq!(cycle.len(), 2);
    assert!(out
        .get("excluded")
        .and_then(|v| v.as_array())
        .expect("excluded")
        .is_empty());
}

#[tokio::test]
async fn cycle_detect_acyclic_reports_topological_order() {
    let router = InferenceServiceRouter::new(noop_fallback());
    let rules = vec![
        branch_eq_write("__exec__.payload.mode", "a", "x", 1),
        branch_eq_write("__exec__.payload.flag", "on", "y", 2),
    ];
    let out = router
        .execute(&svc_params("inference_cycle_detect", rs_args(rules)))
        .await
        .unwrap();
    assert_eq!(out.get("has_cycles"), Some(&JsonValue::Bool(false)));
    let topo = out
        .get("topological_order")
        .and_then(|v| v.as_array())
        .expect("topological_order 数组");
    assert_eq!(topo.len(), 2);
}

#[tokio::test]
async fn conflict_detect_contradictory_and_shadow() {
    let router = InferenceServiceRouter::new(noop_fallback());
    // 常量矛盾对
    let rules1 = vec![
        branch_eq_write("__exec__.payload.mode", "a", "x", 1),
        branch_eq_write("__exec__.payload.mode", "a", "x", 2),
    ];
    let out1 = router
        .execute(&svc_params(
            "inference_conflict_detect",
            rs_args(rules1.clone()),
        ))
        .await
        .unwrap();
    assert_eq!(out1.get("has_conflicts"), Some(&JsonValue::Bool(true)));
    let conflicts1 = out1
        .get("conflicts")
        .and_then(|v| v.as_array())
        .expect("conflicts 数组");
    assert!(conflicts1.iter().any(|c| {
        c.get("conflict_type").and_then(|v| v.as_str()) == Some("contradictory_constants")
    }));
    // 同常量 → 良性无冲突
    let rules2 = vec![
        branch_eq_write("__exec__.payload.mode", "a", "x", 1),
        branch_eq_write("__exec__.payload.mode", "a", "x", 1),
    ];
    let out2 = router
        .execute(&svc_params("inference_conflict_detect", rs_args(rules2)))
        .await
        .unwrap();
    assert_eq!(out2.get("has_conflicts"), Some(&JsonValue::Bool(false)));
    // 指令类型互斥 → 可证不重叠
    let mk_instr = |t: &str, value: i64| {
        JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("instruction")),
                            ("instruction_type", JsonValue::string(t)),
                        ]),
                    ),
                    (
                        "on_true",
                        JsonValue::Array(vec![JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("set")),
                            (
                                "params",
                                JsonValue::object_from_pairs(&[
                                    ("attr", JsonValue::string("x")),
                                    ("value", JsonValue::Integer(value)),
                                ]),
                            ),
                        ])]),
                    ),
                ]),
            ),
        ])
    };
    let rules3 = vec![mk_instr("increment", 1), mk_instr("decrement", 2)];
    let out3 = router
        .execute(&svc_params("inference_conflict_detect", rs_args(rules3)))
        .await
        .unwrap();
    assert_eq!(out3.get("has_conflicts"), Some(&JsonValue::Bool(false)));
}

#[tokio::test]
async fn conflict_detect_io_result_contention() {
    let router = InferenceServiceRouter::new(noop_fallback());
    let mk_io = |n: i64| {
        JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("instruction")),
                            ("instruction_type", JsonValue::string("call_service")),
                        ]),
                    ),
                    (
                        "on_false",
                        JsonValue::Array(vec![JsonValue::object_from_pairs(&[
                            ("type", JsonValue::string("io_request")),
                            (
                                "params",
                                JsonValue::object_from_pairs(&[
                                    ("io_type", JsonValue::string("call_service")),
                                    ("marker", JsonValue::Integer(n)),
                                ]),
                            ),
                        ])]),
                    ),
                ]),
            ),
        ])
    };
    let rules = vec![mk_io(1), mk_io(2)];
    let out = router
        .execute(&svc_params("inference_conflict_detect", rs_args(rules)))
        .await
        .unwrap();
    let conflicts = out
        .get("conflicts")
        .and_then(|v| v.as_array())
        .expect("conflicts 数组");
    assert!(conflicts.iter().any(|c| {
        c.get("conflict_type").and_then(|v| v.as_str()) == Some("io_result_contention")
    }));
}

#[tokio::test]
async fn constitution_excluded_by_default_and_listed() {
    let router = InferenceServiceRouter::new(noop_fallback());
    let dyn_rule = JsonValue::object_from_pairs(&[
        ("type", JsonValue::string("set")),
        (
            "params",
            JsonValue::object_from_pairs(&[
                (
                    "attr",
                    JsonValue::string("__exec__.instruction.params.attr"),
                ),
                ("value", JsonValue::Integer(1)),
            ]),
        ),
    ]);
    let args = JsonValue::object_from_pairs(&[
        ("rules", JsonValue::Array(vec![dyn_rule.clone()])),
        (
            "sources",
            JsonValue::Array(vec![JsonValue::string("core_eval")]),
        ),
    ]);
    let out = router
        .execute(&svc_params("inference_cycle_detect", args))
        .await
        .unwrap();
    let excluded = out
        .get("excluded")
        .and_then(|v| v.as_array())
        .expect("excluded 数组");
    assert_eq!(excluded.len(), 1);
    assert_eq!(excluded[0].as_str(), Some("core_eval#0"));
    // 显式包含 → 不豁免
    let args2 = JsonValue::object_from_pairs(&[
        ("rules", JsonValue::Array(vec![dyn_rule])),
        (
            "sources",
            JsonValue::Array(vec![JsonValue::string("core_eval")]),
        ),
        ("include_constitution", JsonValue::Bool(true)),
    ]);
    let out2 = router
        .execute(&svc_params("inference_cycle_detect", args2))
        .await
        .unwrap();
    assert!(out2
        .get("excluded")
        .and_then(|v| v.as_array())
        .expect("excluded")
        .is_empty());
}

#[tokio::test]
async fn ruleset_services_fail_fast_with_guidance() {
    let router = InferenceServiceRouter::new(noop_fallback());
    // 缺 sources
    let bad = JsonValue::object_from_pairs(&[("rules", JsonValue::Array(vec![]))]);
    let err = router
        .execute(&svc_params("inference_cycle_detect", bad))
        .await
        .unwrap_err();
    assert!(err.contains("sources"), "错误须含缺失参数指引: {err}");
    // 长度不一致
    let bad2 = JsonValue::object_from_pairs(&[
        (
            "rules",
            JsonValue::Array(vec![branch_eq_write("payload.m", "a", "x", 1)]),
        ),
        ("sources", JsonValue::Array(vec![])),
    ]);
    let err2 = router
        .execute(&svc_params("inference_rw_extract", bad2))
        .await
        .unwrap_err();
    assert!(err2.contains("长度不一致"));
    // 未知服务仍走 HTTP 回落（诚实错误）
    struct SentinelHandler;
    #[async_trait::async_trait]
    impl IoHandler for SentinelHandler {
        async fn execute(&self, _params: &JsonValue) -> evorule_reactor::IoResult {
            Err("unknown-service-fallback".to_string())
        }
    }
    let router2 = InferenceServiceRouter::new(Arc::new(SentinelHandler));
    let err3 = router2
        .execute(&svc_params(
            "inference_unknown",
            JsonValue::object_from_pairs(&[]),
        ))
        .await
        .unwrap_err();
    assert_eq!(err3, "unknown-service-fallback");
}

#[tokio::test]
async fn ruleset_services_deterministic_double_run() {
    let router = InferenceServiceRouter::new(noop_fallback());
    let rules = vec![
        branch_eq_write("__exec__.payload.mode", "a", "x", 1),
        branch_eq_write("__exec__.payload.mode", "a", "x", 2),
    ];
    for svc in [
        "inference_rw_extract",
        "inference_cycle_detect",
        "inference_conflict_detect",
    ] {
        let params = svc_params(svc, rs_args(rules.clone()));
        let r1 = router.execute(&params).await.unwrap();
        let r2 = router.execute(&params).await.unwrap();
        assert_eq!(
            r1.to_string(),
            r2.to_string(),
            "{svc} 同输入两次执行必须逐位一致"
        );
    }
}

// ============================================================================
// 真实语料回归（env 门控：EVORULE_CORPUS=1 时执行；CI 无本地语料路径自动跳过）
// 设计文档 §六.2：宪法 core_eval.json / 部署面 server_eval.json / rules_dir 全量跑
// ============================================================================

fn corpus_enabled() -> bool {
    std::env::var("EVORULE_CORPUS").ok().as_deref() == Some("1")
}

fn load_json_array(path: &str) -> Vec<JsonValue> {
    let raw = std::fs::read_to_string(path).expect("语料文件可读");
    let doc: serde_json::Value = serde_json::from_str(&raw).expect("语料为合法 JSON");
    let arr = doc
        .get("transform")
        .and_then(|v| v.as_array())
        .expect("语料含 transform 数组");
    // serde_json::Value → evorule_tcb::JsonValue 通用转换（语料加载专用）
    fn conv(v: &serde_json::Value) -> JsonValue {
        match v {
            serde_json::Value::Null => JsonValue::Null,
            serde_json::Value::Bool(b) => JsonValue::Bool(*b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    JsonValue::Integer(i)
                } else {
                    JsonValue::string(n.to_string())
                }
            }
            serde_json::Value::String(s) => JsonValue::string(s.clone()),
            serde_json::Value::Array(items) => JsonValue::Array(items.iter().map(conv).collect()),
            serde_json::Value::Object(map) => {
                let pairs: Vec<(String, JsonValue)> =
                    map.iter().map(|(k, v)| (k.clone(), conv(v))).collect();
                JsonValue::object_from_pairs_owned(pairs)
            }
        }
    }
    arr.iter().map(conv).collect()
}

/// 三份真实规则集（宪法 + 部署面 + 部署规则）全量通过三服务：
/// 断言 = 不 panic / 不 Err / 输出结构完备 / 双跑位元一致。
/// 宪法段豁免桶下预期无环无冲突（部署面是否有真实发现人工复核，不做硬断言）。
#[tokio::test]
async fn real_corpus_smoke_three_sources() {
    if !corpus_enabled() {
        return;
    }
    let router = InferenceServiceRouter::new(noop_fallback());
    let mut sources: Vec<JsonValue> = Vec::new();
    let mut rules: Vec<JsonValue> = Vec::new();
    // ① 宪法（豁免桶）
    for r in load_json_array("D:/evorule/evorule-tcb/core_eval.json") {
        rules.push(r);
        sources.push(JsonValue::string("core_eval"));
    }
    // ② 部署面
    for r in load_json_array("D:/evorule-server/resources/server_eval.json") {
        rules.push(r);
        sources.push(JsonValue::string("server_eval"));
    }
    // ③ 部署规则（字典序）
    let mut rule_files: Vec<String> = std::fs::read_dir("D:/evorule-server/rules")
        .expect("rules_dir 可读")
        .filter_map(|e| {
            let p = e.ok()?.path();
            if p.extension()?.to_str()? == "json" {
                p.to_str().map(String::from)
            } else {
                None
            }
        })
        .collect();
    rule_files.sort();
    for f in &rule_files {
        for r in load_json_array(f) {
            rules.push(r);
            sources.push(JsonValue::string(format!(
                "rules/{}",
                std::path::Path::new(f)
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
            )));
        }
    }
    let args = JsonValue::object_from_pairs(&[
        ("rules", JsonValue::Array(rules)),
        ("sources", JsonValue::Array(sources)),
    ]);
    for svc in [
        "inference_rw_extract",
        "inference_cycle_detect",
        "inference_conflict_detect",
        "inference_effect_predict",
        "inference_convergence_check",
    ] {
        let params = svc_params(svc, args.clone());
        let r1 = router.execute(&params).await;
        assert!(r1.is_ok(), "{svc} 真实语料不得 Err: {:?}", r1.err());
        let r2 = router.execute(&params).await.unwrap();
        assert_eq!(
            r1.unwrap().to_string(),
            r2.to_string(),
            "{svc} 真实语料双跑必须位元一致"
        );
    }
    // backward（阶段 3）：带目标路径做部署面可达性查询
    let bw_args = JsonValue::object_from_pairs(&[
        (
            "rules",
            args.get("rules").cloned().unwrap_or(JsonValue::Null),
        ),
        (
            "sources",
            args.get("sources").cloned().unwrap_or(JsonValue::Null),
        ),
        ("target_path", str_val("payload.x")),
    ]);
    let b1 = router
        .execute(&svc_params("inference_backward_plan", bw_args.clone()))
        .await;
    assert!(b1.is_ok(), "backward 真实语料不得 Err: {:?}", b1.err());
    let b2 = router
        .execute(&svc_params("inference_backward_plan", bw_args))
        .await
        .unwrap();
    assert_eq!(
        b1.unwrap().to_string(),
        b2.to_string(),
        "backward 真实语料双跑必须位元一致"
    );
    // forward（阶段 3）：最小合成文档仿真 3 步
    let fwd_args = JsonValue::object_from_pairs(&[
        (
            "rules",
            args.get("rules").cloned().unwrap_or(JsonValue::Null),
        ),
        (
            "sources",
            args.get("sources").cloned().unwrap_or(JsonValue::Null),
        ),
        (
            "initial_document",
            JsonValue::object_from_pairs(&[
                (
                    "instruction",
                    JsonValue::object_from_pairs(&[("type", str_val("probe"))]),
                ),
                (
                    "payload",
                    JsonValue::object_from_pairs(&[("x", JsonValue::Integer(0))]),
                ),
                ("queue", JsonValue::Array(vec![])),
            ]),
        ),
        ("max_steps", JsonValue::Integer(3)),
    ]);
    let f1 = router
        .execute(&svc_params("inference_forward_simulate", fwd_args.clone()))
        .await;
    assert!(f1.is_ok(), "forward 真实语料不得 Err: {:?}", f1.err());
    let f2 = router
        .execute(&svc_params("inference_forward_simulate", fwd_args))
        .await
        .unwrap();
    assert_eq!(
        f1.unwrap().to_string(),
        f2.to_string(),
        "forward 真实语料双跑必须位元一致"
    );
    // 宪法豁免桶显式标注
    let out = router
        .execute(&svc_params("inference_cycle_detect", args))
        .await
        .unwrap();
    let excluded = out
        .get("excluded")
        .and_then(|v| v.as_array())
        .expect("excluded 数组");
    assert!(
        excluded.iter().any(|v| v.as_str() == Some("core_eval#0")),
        "宪法段须显式出现在豁免清单"
    );
}

// ============================================================================
// 阶段 3 四服务 e2e（112 阶段 3，R-1~R-3 裁定口径）
// ============================================================================

fn stage3_rules() -> Vec<JsonValue> {
    let set_x = |v: i64| {
        JsonValue::object_from_pairs(&[
            ("type", str_val("set")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("attr", str_val("x")),
                    ("operation", str_val("set")),
                    ("value", JsonValue::Integer(v)),
                ]),
            ),
        ])
    };
    vec![
        // r0: payload.flag 存在 → set x=1
        JsonValue::object_from_pairs(&[
            ("type", str_val("branch")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    (
                        "domain",
                        JsonValue::object_from_pairs(&[
                            ("type", str_val("exists")),
                            ("path", str_val("__exec__.payload.flag")),
                        ]),
                    ),
                    ("on_true", JsonValue::Array(vec![set_x(1)])),
                ]),
            ),
        ]),
        // r1: io_request（仿真断流材料）
        JsonValue::object_from_pairs(&[
            ("type", str_val("io_request")),
            (
                "params",
                JsonValue::object_from_pairs(&[
                    ("io_type", str_val("call_service")),
                    (
                        "params",
                        JsonValue::object_from_pairs(&[("service_name", str_val("svc"))]),
                    ),
                ]),
            ),
        ]),
    ]
}

fn stage3_sources() -> Vec<JsonValue> {
    vec![str_val("rules/a.json"), str_val("rules/b.json")]
}

fn stage3_doc() -> JsonValue {
    JsonValue::object_from_pairs(&[
        (
            "instruction",
            JsonValue::object_from_pairs(&[("type", str_val("probe"))]),
        ),
        (
            "payload",
            JsonValue::object_from_pairs(&[("flag", JsonValue::Bool(true))]),
        ),
        ("queue", JsonValue::Array(vec![])),
    ])
}

#[tokio::test]
async fn forward_simulate_e2e_fixpoint_then_io_block() {
    let router = InferenceServiceRouter::new(noop_fallback());
    let args = JsonValue::object_from_pairs(&[
        ("rules", JsonValue::Array(stage3_rules())),
        ("sources", JsonValue::Array(stage3_sources())),
        ("initial_document", stage3_doc()),
        ("max_steps", JsonValue::Integer(16)),
    ]);
    let out = router
        .execute(&svc_params("inference_forward_simulate", args))
        .await
        .unwrap();
    // r0 命中（flag 存在）→ x=1；下一轮扫描 r1 io_request → 诚实断流
    assert_eq!(
        out.get("stopped_reason").and_then(|v| v.as_str()),
        Some("blocked_on_io")
    );
    assert_eq!(out.get("steps").and_then(|v| v.as_i64()), Some(1));
    let io = out.get("blocked_on_io").unwrap();
    assert_eq!(
        io.get("rule_id").and_then(|v| v.as_str()),
        Some("rules/b.json#1")
    );
    assert_eq!(
        io.get("io_type").and_then(|v| v.as_str()),
        Some("call_service")
    );
    let payload = out.get("final_document").unwrap().get("payload").unwrap();
    assert_eq!(payload.get("x").and_then(|v| v.as_i64()), Some(1));
    assert!(out
        .get("simulation_scope")
        .and_then(|v| v.as_str())
        .is_some());
}

#[tokio::test]
async fn backward_plan_e2e_producer_chain() {
    let router = InferenceServiceRouter::new(noop_fallback());
    let args = JsonValue::object_from_pairs(&[
        ("rules", JsonValue::Array(stage3_rules())),
        ("sources", JsonValue::Array(stage3_sources())),
        ("target_path", str_val("payload.x")),
    ]);
    let out = router
        .execute(&svc_params("inference_backward_plan", args))
        .await
        .unwrap();
    assert_eq!(out.get("achievable").and_then(|v| v.as_bool()), Some(true));
    let seq = out.get("rule_sequence").and_then(|v| v.as_array()).unwrap();
    assert_eq!(seq.len(), 1, "唯一生产者 r0");
    assert_eq!(seq[0].as_str(), Some("rules/a.json#0"));
}

#[tokio::test]
async fn effect_predict_e2e_impact_grading() {
    let router = InferenceServiceRouter::new(noop_fallback());
    let args = JsonValue::object_from_pairs(&[
        ("rules", JsonValue::Array(stage3_rules())),
        ("sources", JsonValue::Array(stage3_sources())),
    ]);
    let out = router
        .execute(&svc_params("inference_effect_predict", args))
        .await
        .unwrap();
    let results = out.get("results").and_then(|v| v.as_array()).unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(
        results[0].get("impact").and_then(|v| v.as_str()),
        Some("write_few")
    );
    assert_eq!(
        results[1].get("impact").and_then(|v| v.as_str()),
        Some("io_touching")
    );
}

#[tokio::test]
async fn convergence_check_e2e_structure() {
    let router = InferenceServiceRouter::new(noop_fallback());
    let args = JsonValue::object_from_pairs(&[
        ("rules", JsonValue::Array(stage3_rules())),
        ("sources", JsonValue::Array(stage3_sources())),
    ]);
    let out = router
        .execute(&svc_params("inference_convergence_check", args))
        .await
        .unwrap();
    // 结构完备断言（具体判定归 lib 单测；此处验证路由 + 字段面）
    assert!(out.get("converges").and_then(|v| v.as_bool()).is_some());
    assert!(out.get("undetermined").and_then(|v| v.as_bool()).is_some());
    assert!(out.get("issues").and_then(|v| v.as_array()).is_some());
    assert!(out.get("excluded").and_then(|v| v.as_array()).is_some());
}

#[tokio::test]
async fn forward_simulate_e2e_missing_doc_fails_fast() {
    let router = InferenceServiceRouter::new(noop_fallback());
    let args = JsonValue::object_from_pairs(&[
        ("rules", JsonValue::Array(stage3_rules())),
        ("sources", JsonValue::Array(stage3_sources())),
    ]);
    let r = router
        .execute(&svc_params("inference_forward_simulate", args))
        .await;
    assert!(r.is_err(), "缺 initial_document 必须 fail-fast");
}

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
        ]
    );
}

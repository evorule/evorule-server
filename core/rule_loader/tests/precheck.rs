// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 装载预检（观测面）测试：报告形态与确定性 / WARN 边界 / 超限内核兜底 /
//! 部署面 fixture 锚（环境门控）/ 纯观测不变式。

use std::path::PathBuf;

use evorule_rule_loader::{
    build_precheck, load_rule_set, serde_to_tcb, ConstitutionError, LoadContext, LoadedRuleset,
};
use evorule_tcb::{execute_transition, JsonValue, TcbError};
use serde_json::json;

// ============================================================================
// 合成规则集构造（单 branch 规则克隆扩容；attr 逐条相异防纪律门误判）
// ============================================================================

fn branch_rule(idx: usize) -> serde_json::Value {
    json!({
        "type": "branch", "params": {
            "domain": { "type": "all", "inner": [], "on_missing": "error" },
            "on_true": [ { "type": "set", "params": {
                "attr": format!("x{idx}"), "operation": "set", "value": idx as i64 } } ],
            "on_false": []
        }
    })
}

fn rule_set_doc(rules: Vec<serde_json::Value>) -> serde_json::Value {
    json!({
        "$schema": "https://evorule.org/schemas/rule_set/v1.0.json",
        "kind": "rule_set",
        "id": "com.evorule.test.precheck",
        "version": "0.1.0",
        "metadata": { "title": "precheck test" },
        "transform": rules
    })
}

fn load(doc: &serde_json::Value) -> LoadedRuleset {
    let ctx = LoadContext::business_file("precheck-test.json");
    load_rule_set(doc, &ctx).expect("合成规则集装载应通过")
}

// ============================================================================
// ① 报告形态 + 确定性 + 纯观测不变式
// ============================================================================

#[test]
fn precheck_shape_determinism_and_purity() {
    let doc = rule_set_doc(vec![branch_rule(0)]);
    let loaded = load(&doc);

    let p = &loaded.precheck;
    assert_eq!(p.transform_rules_used, 1);
    assert_eq!(
        p.transform_rules_limit,
        evorule_tcb::MAX_TRANSFORM_RULES as u32
    );
    // 1*1000/limit 向下取整（整数千分比，禁浮点）。上限随 tcb 常量演进时
    // 本锚需同步定谳——锚执法面（设计稿 v1.0 误记 64，实况=crates.io
    // evorule-tcb 0.13.0 常量 128，见板行勘误注）。
    assert_eq!(p.utilization_permille, 7); // 1000/128 = 7.8125 → 7
    assert_eq!(p.io_request_count, 0);
    assert_eq!(p.domain_histogram.get("branch"), Some(&1));
    assert_eq!(p.domain_histogram.get("set"), Some(&1));
    assert_eq!(p.domain_histogram.get("all"), Some(&1));
    assert!(p.precheck_warning.is_none());

    // 确定性：同一文档两次装载 → 报告全等
    let reloaded = load(&doc);
    assert_eq!(reloaded.precheck, loaded.precheck);

    // 纯观测：统计不触碰规则树（重复统计 + 规则树逐值不变）
    let snapshot = loaded.engine_rules.clone();
    let again = build_precheck(&loaded.engine_rules);
    assert_eq!(again, loaded.precheck);
    assert_eq!(loaded.engine_rules, snapshot);

    // 机器可读面：serde 字段集=设计六字段，无多余键
    let v = serde_json::to_value(&loaded.precheck).expect("报告序列化应成功");
    let obj = v.as_object().expect("报告应为对象");
    let keys: Vec<&String> = obj.keys().collect();
    assert_eq!(keys.len(), 6);
    for k in [
        "transform_rules_used",
        "transform_rules_limit",
        "utilization_permille",
        "io_request_count",
        "domain_histogram",
        "precheck_warning",
    ] {
        assert!(obj.contains_key(k), "缺字段 {k}");
    }
}

// ============================================================================
// ② WARN 边界（≥800‰ 在场 / <800‰ None；整数千分比向下取整口径）
// ============================================================================

#[test]
fn warn_threshold_boundary() {
    // 102*1000/128 = 796.875 → 796‰ < 800 → 无警告
    let below = load(&rule_set_doc((0..102).map(branch_rule).collect()));
    assert_eq!(below.precheck.utilization_permille, 796);
    assert!(below.precheck.precheck_warning.is_none());

    // 103*1000/128 = 804.6875 → 804‰ ≥ 800 → 警告在场（文本含规则数/上限/千分比）
    let above = load(&rule_set_doc((0..103).map(branch_rule).collect()));
    assert_eq!(above.precheck.utilization_permille, 804);
    assert_eq!(
        above.precheck.precheck_warning.as_deref(),
        Some("rules nearing transform limit (103/128, 804‰)")
    );
}

// ============================================================================
// ③ 超限件：预检不拦截不吞错；装载前门（schema maxItems=上限）与内核
//    TooManyTransformRules 双防线分层定谳
// ============================================================================

#[test]
fn over_limit_kernel_still_fails_closed() {
    let over_limit = evorule_tcb::MAX_TRANSFORM_RULES + 1; // 129@tcb 0.13.0

    // 装载前门：schema transform maxItems=128 先拒（预检不参与执法——拒载
    // 归 schema 门，预检始终纯观测）
    let over_doc = rule_set_doc((0..over_limit).map(branch_rule).collect());
    let ctx = LoadContext::business_file("precheck-test.json");
    let err = load_rule_set(&over_doc, &ctx).expect_err("超上限制规则集应被装载前门拒");
    assert!(
        matches!(err, ConstitutionError::SchemaViolation(_)),
        "拒载应归 schema 门，实得: {err}"
    );

    // 满额合法件（128 条=1000‰）：装载放行，预检如实外显（不吞错不降级）
    let full = load(&rule_set_doc(
        (0..evorule_tcb::MAX_TRANSFORM_RULES)
            .map(branch_rule)
            .collect(),
    ));
    assert_eq!(full.precheck.transform_rules_used, 128);
    assert_eq!(full.precheck.utilization_permille, 1000); // 128*1000/128 整除
    assert_eq!(
        full.precheck.precheck_warning.as_deref(),
        Some("rules nearing transform limit (128/128, 1000‰)")
    );

    // 内核既有闸独立可达：不经装载通道直调 execute_transition，
    // 超限 core_eval → TooManyTransformRules fail-closed 兜底照常命中
    let rules_tcb: Vec<JsonValue> = (0..over_limit)
        .map(|i| serde_to_tcb(branch_rule(i)))
        .collect();
    let mut instr = std::collections::BTreeMap::new();
    instr.insert("type".to_string(), JsonValue::string("noop"));
    let instruction = JsonValue::object(instr);
    let mut payload_map = std::collections::BTreeMap::new();
    payload_map.insert("x".to_string(), JsonValue::Integer(0));
    let payload = JsonValue::object(payload_map);

    let result = execute_transition(&rules_tcb, &instruction, &payload, &[]);
    match result {
        Err(TcbError::TooManyTransformRules { .. }) => {} // 预检未拦截未吞错
        Ok(_) => panic!("内核必须对超限规则集 fail-closed"),
        Err(e) => panic!("内核错误类型非 TooManyTransformRules: {e:?}"),
    }
}

// ============================================================================
// ④ 部署面 fixture 锚：server_eval.json 23 条/上限 128（环境门控：fixture 在场才跑）
// ============================================================================

/// fixture 定位：env `EVORULE_SERVER_EVAL_FIXTURE` 优先，缺省取仓内
/// installer/payload/resources/server_eval.json（部署分发汇集产物，不入库——
/// CI 检出无此件时本测试跳过，门控口径=fixture 在场才实跑）。
fn fixture_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("EVORULE_SERVER_EVAL_FIXTURE") {
        let path = PathBuf::from(p);
        return path.is_file().then_some(path);
    }
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../installer/payload/resources/server_eval.json");
    path.is_file().then_some(path)
}

#[test]
fn fixture_anchor_server_eval_23_of_64() {
    let Some(path) = fixture_path() else {
        eprintln!(
            "SKIP: server_eval.json fixture 不在场（installer/payload 不入库）；\
             本地实跑请设 EVORULE_SERVER_EVAL_FIXTURE"
        );
        return;
    };

    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("fixture 读取失败 {}: {e}", path.display()));
    let doc: serde_json::Value = serde_json::from_str(&raw).expect("fixture JSON 解析应成功");
    let ctx = LoadContext::constitution(&path.display().to_string());
    let loaded = load_rule_set(&doc, &ctx).expect("部署面宪法装载应通过");

    let p = &loaded.precheck;
    assert_eq!(p.transform_rules_used, 23);
    // 上限真值=evorule-tcb 常量（crates.io 0.13.0=128；设计稿 v1.0 误记 64，
    // 实况勘误见板行——锚按常量现值冻结，tcb 演进时同步定谳）
    assert_eq!(p.transform_rules_limit, 128);
    // 23*1000/128 = 179.6875 → 179（判据冻结）
    assert_eq!(p.utilization_permille, 179);
    assert!(p.precheck_warning.is_none());
    assert_eq!(p.io_request_count, 3);

    // 全树 type 直方图锚（transform 子树实测冻结；词表=通用计数）
    let expect: &[(&str, u32)] = &[
        ("all", 13),
        ("branch", 51),
        ("exists", 24),
        ("instruction", 22),
        ("io_request", 3),
        ("lt", 14),
        ("noop", 3),
        ("not", 2),
        ("push", 7),
        ("set", 49),
    ];
    assert_eq!(p.domain_histogram.len(), expect.len(), "直方图键集应全等");
    for (k, v) in expect {
        assert_eq!(p.domain_histogram.get(*k), Some(v), "直方图键 {k} 计数漂移");
    }
    assert_eq!(p.domain_histogram.values().sum::<u32>(), 188);
}

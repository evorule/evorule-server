// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 图分派原生服务集成测试：
//! ① SSOT 守卫（声明表 ↔ 声明文件三字段顺序一致 + 路由器含名）
//! ② 黄金向量（手算可溯：域外修订全链传播 + 域内修订零传播，断言逐字段）
//! ③ 篡改即红（input_digest / graph_digest 锚定）
//! ④ fail-visible（出处元数据缺失 / 验证位非真 / 未知常数 / 未声明引用）
//! ⑤ 确定性（同输入双跑输出序列化字节全等）

#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试面:断言可读性优先,机制同 plugin-kit 测试桩

use evorule_graph_dispatch_services::{GraphDispatchServiceRouter, NATIVE_SERVICES};
use evorule_reactor::IoHandler;
use evorule_tcb::JsonValue;

fn svc_params(args: JsonValue) -> JsonValue {
    JsonValue::object_from_pairs(&[
        ("service_name", JsonValue::string("graph_shock_propagate")),
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

/// 测试面 TCB → serde 转换（输出字节比对用；与服务层同语义的纯镜像）
fn tcb_to_serde_test(v: &JsonValue) -> serde_json::Value {
    match v {
        JsonValue::Null => serde_json::Value::Null,
        JsonValue::Bool(b) => serde_json::Value::Bool(*b),
        JsonValue::Integer(i) => serde_json::Value::Number((*i).into()),
        JsonValue::U64(u) => serde_json::Value::Number((*u).into()),
        JsonValue::String(s) => serde_json::Value::String(s.to_string()),
        JsonValue::Array(items) => {
            serde_json::Value::Array(items.iter().map(tcb_to_serde_test).collect())
        }
        JsonValue::Object(map) => {
            let mut obj = serde_json::Map::new();
            for (k, val) in map.iter() {
                obj.insert(k.clone(), tcb_to_serde_test(val));
            }
            serde_json::Value::Object(obj)
        }
    }
}

// ============================================================================
// 黄金向量语料（手算可溯）
//
// 规则集：常数 c1（版本 2018，域 [0,1]）；
// 指令 A{dims [1,0,0,0,0,0,0], constants:["c1"]}、B{同 dims, constants:["c1"]}、
//       C{dims [1,0,0,0,0,0,0], 无引用}、D{dims [0,1,0,0,0,0,0]}。
//
// 手算依赖图：
//   引用边 = (A,c1), (B,c1)；
//   耦合边 = (A,B,L轴), (A,C,L轴), (B,C,L轴)；D 与任何人无非零同轴 → 零耦合。
//
// 修订 c1 → 5（域 [0,1] 外）：
//   直接受影响 = {A,B}（depth1, reference）；regime 复查 5>1 → A,B 失效；
//   耦合 BFS：层1 波及 C（depth2, coupling）→ 失效；D 全程不受影响。
//   ⇒ affected=[A(1,reference),B(1,reference),C(2,coupling)]，
//     regime_invalidated=[A,B,C]，adjudications_invalidated=[A 条目]，
//     domains_touched=[[1,0,0,0,0,0,0]]，propagation_depth=2。
//
// 修订 c1 → 0.5（域内）：
//   直接受影响 = {A,B}；regime 复查 0.5∈[0,1] → 零失效 → 无传播；
//   ⇒ regime_invalidated=[]，affected=[A(1,reference),B(1,reference)]，
//     domains_touched=[]，propagation_depth=1。
// ============================================================================

/// 黄金向量规则集（serde json! 构造 → serde_to_tcb 入 TCB 域，与装载通道同转换）
fn golden_rule_set() -> JsonValue {
    let doc = serde_json::json!({
        "metadata": {
            "constants": [
                { "constant_id": "c1", "version": "2018", "value_hash": "h1", "domain_lo": 0, "domain_hi": 1 }
            ],
            "formula_registry": [
                { "instruction_type": "A", "formula": "f_a", "dimensions": [1, 0, 0, 0, 0, 0, 0], "constants": ["c1"] },
                { "instruction_type": "B", "formula": "f_b", "dimensions": [1, 0, 0, 0, 0, 0, 0], "constants": ["c1"] },
                { "instruction_type": "C", "formula": "f_c", "dimensions": [1, 0, 0, 0, 0, 0, 0] },
                { "instruction_type": "D", "formula": "f_d", "dimensions": [0, 1, 0, 0, 0, 0, 0] }
            ]
        }
    });
    evorule_rule_loader::serde_to_tcb(doc)
}

/// 黄金向量修订提案（出处元数据齐备）
fn golden_revision(new_value: serde_json::Value) -> JsonValue {
    let doc = serde_json::json!({
        "constant_id": "c1",
        "new_value": new_value,
        "source": "constants-table-2018-review",
        "verified": true
    });
    evorule_rule_loader::serde_to_tcb(doc)
}

/// 黄金向量裁决清单（A 与 D 已裁决）
fn golden_adjudications() -> JsonValue {
    let doc = serde_json::json!([
        { "instruction_type": "A", "state": "adjudicated" },
        { "instruction_type": "D", "state": "adjudicated" }
    ]);
    evorule_rule_loader::serde_to_tcb(doc)
}

fn golden_args(new_value: serde_json::Value) -> JsonValue {
    JsonValue::object_from_pairs(&[
        ("rule_set", golden_rule_set()),
        ("revision", golden_revision(new_value)),
        ("adjudications", golden_adjudications()),
    ])
}

// ── ① SSOT 守卫 ──

#[tokio::test]
async fn ssot_guard_and_router_contains_name() {
    // 声明表 ↔ 声明文件三字段顺序一致（lib.rs 单测同守，此处锚定 e2e 面）
    let raw = include_str!("../official_native_services.json");
    let file: serde_json::Value = serde_json::from_str(raw).unwrap();
    let services = file.get("services").and_then(|v| v.as_array()).unwrap();
    assert_eq!(services.len(), NATIVE_SERVICES.len());
    for (f, d) in services.iter().zip(NATIVE_SERVICES) {
        assert_eq!(f.get("name").and_then(|v| v.as_str()), Some(d.name));
        assert_eq!(
            f.get("sensitive").and_then(|v| v.as_bool()),
            Some(d.sensitive)
        );
        assert_eq!(
            f.get("description").and_then(|v| v.as_str()),
            Some(d.description)
        );
    }
    // 路由器含名
    let names = GraphDispatchServiceRouter::native_service_names();
    assert_eq!(names, vec!["graph_shock_propagate"]);
}

// ── ② 黄金向量（手算值逐字段断言）──

#[tokio::test]
async fn golden_vector_domain_outside_revision_propagates() {
    let router = GraphDispatchServiceRouter::new(noop_fallback());
    let out = router
        .execute(&svc_params(golden_args(serde_json::json!(5))))
        .await
        .unwrap();

    // 受影响集：A,B depth1 reference；C depth2 coupling；D 不出现
    let affected = out
        .get("affected_instructions")
        .unwrap()
        .as_array()
        .unwrap();
    let affected_desc: Vec<(String, i64, String)> = affected
        .iter()
        .map(|e| {
            (
                e.get("instruction_type")
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string(),
                e.get("depth").unwrap().as_i64().unwrap(),
                e.get("via").unwrap().as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        affected_desc,
        vec![
            ("A".to_string(), 1, "reference".to_string()),
            ("B".to_string(), 1, "reference".to_string()),
            ("C".to_string(), 2, "coupling".to_string()),
        ],
        "手算期望: A,B 引用闭包 depth1; C 耦合波及 depth2; D 不受影响"
    );

    // regime 失效集
    let invalidated = out.get("regime_invalidated").unwrap().as_array().unwrap();
    let invalidated_names: Vec<String> = invalidated
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(invalidated_names, vec!["A", "B", "C"]);

    // 已裁命题反推：A 条目原样保留（保序），D 未失效不出现
    let adj_inv = out
        .get("adjudications_invalidated")
        .unwrap()
        .as_array()
        .unwrap();
    assert_eq!(adj_inv.len(), 1);
    assert_eq!(
        adj_inv[0].get("instruction_type").unwrap().as_str(),
        Some("A")
    );
    assert_eq!(
        adj_inv[0].get("state").unwrap().as_str(),
        Some("adjudicated")
    );

    // 失效量纲去重（A/B/C 同 dims [1,0,0,0,0,0,0]）
    let domains = out.get("domains_touched").unwrap().as_array().unwrap();
    assert_eq!(domains.len(), 1);
    let dims: Vec<i64> = domains[0]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect();
    assert_eq!(dims, vec![1, 0, 0, 0, 0, 0, 0]);

    // 传播深度与标量字段
    assert_eq!(out.get("propagation_depth").unwrap().as_i64(), Some(2));
    assert_eq!(out.get("shocked_constant").unwrap().as_str(), Some("c1"));
    assert_eq!(out.get("new_value").unwrap().as_i64(), Some(5));

    // 双 digest 形态
    assert!(
        out.get("graph_digest")
            .unwrap()
            .as_str()
            .unwrap()
            .starts_with("blake3:"),
        "graph_digest 须为 blake3 前缀形态"
    );
    assert!(
        out.get("input_digest")
            .unwrap()
            .as_str()
            .unwrap()
            .starts_with("blake3:"),
        "input_digest 须为 blake3 前缀形态"
    );
}

#[tokio::test]
async fn golden_vector_domain_inside_revision_no_propagation() {
    let router = GraphDispatchServiceRouter::new(noop_fallback());
    // 0.5 为小数：serde json! 数字 → serde_to_tcb 入 TCB 域成字符串形态（引擎惯例）
    let out = router
        .execute(&svc_params(golden_args(serde_json::json!(0.5))))
        .await
        .unwrap();

    let invalidated = out.get("regime_invalidated").unwrap().as_array().unwrap();
    assert!(invalidated.is_empty(), "域内修订零失效: {invalidated:?}");

    let affected = out
        .get("affected_instructions")
        .unwrap()
        .as_array()
        .unwrap();
    assert_eq!(affected.len(), 2, "域内修订仅引用闭包 A,B");
    for e in affected {
        assert_eq!(e.get("via").unwrap().as_str(), Some("reference"));
        assert_eq!(e.get("depth").unwrap().as_i64(), Some(1));
    }
    let names: Vec<&str> = affected
        .iter()
        .map(|e| e.get("instruction_type").unwrap().as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["A", "B"]);

    assert_eq!(out.get("propagation_depth").unwrap().as_i64(), Some(1));
    assert!(out
        .get("domains_touched")
        .unwrap()
        .as_array()
        .unwrap()
        .is_empty());
    assert!(out
        .get("adjudications_invalidated")
        .unwrap()
        .as_array()
        .unwrap()
        .is_empty());
}

// ── ②b 小数域界（TCB 域小数字符串惯例 = 真机 REST 通道形态）──
//
// 真机实验（B-211/D5 首轮 18180 实例）抓得的集成缺口：serde_to_tcb 将小数
// 转为字符串承载（引擎惯例），旧 optional_domain 仅认 serde Number——整数域界
// （黄金向量 0/1）可过、小数域界真机必炸。修复后本节锁死真机形态回归。

#[tokio::test]
async fn fractional_domain_bounds_accepted_via_tcb_decimal_string() {
    let router = GraphDispatchServiceRouter::new(noop_fallback());
    // alpha 域 [2e-5, 2.6e-5]（serde 小数 → TCB 字符串）；thermal 引用 alpha，
    // clearance 同 dims 无引用（耦合波及对象）
    let doc = serde_json::json!({
        "metadata": {
            "constants": [
                { "constant_id": "alpha", "version": "design-baseline", "value_hash": "h1", "domain_lo": 0.00002, "domain_hi": 0.000026 }
            ],
            "formula_registry": [
                { "instruction_type": "thermal", "formula": "f_t", "dimensions": [1, 0, 0, 0, 0, 0, 0], "constants": ["alpha"] },
                { "instruction_type": "clearance", "formula": "f_c", "dimensions": [1, 0, 0, 0, 0, 0, 0] }
            ]
        }
    });
    let rule_set = evorule_rule_loader::serde_to_tcb(doc);
    let revision = |value: serde_json::Value| {
        evorule_rule_loader::serde_to_tcb(serde_json::json!({
            "constant_id": "alpha", "new_value": value,
            "source": "recalibration", "verified": true
        }))
    };

    // 域内小数修订：引用闭包命中、零失效零传播
    let args_in = JsonValue::object_from_pairs(&[
        ("rule_set", rule_set.clone()),
        ("revision", revision(serde_json::json!(0.0000242))),
        ("adjudications", golden_adjudications()),
    ]);
    let out_in = router.execute(&svc_params(args_in)).await.unwrap();
    assert!(
        out_in
            .get("regime_invalidated")
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty(),
        "域内小数修订零失效: {out_in:?}"
    );
    let affected_in = out_in
        .get("affected_instructions")
        .unwrap()
        .as_array()
        .unwrap();
    assert_eq!(affected_in.len(), 1, "域内仅引用闭包 thermal");
    assert_eq!(
        affected_in[0].get("instruction_type").unwrap().as_str(),
        Some("thermal")
    );

    // 域外小数修订：regime 失效 + 耦合 BFS depth2（迷你风暴）
    let args_out = JsonValue::object_from_pairs(&[
        ("rule_set", rule_set),
        ("revision", revision(serde_json::json!(0.0000285))),
        ("adjudications", golden_adjudications()),
    ]);
    let out_out = router.execute(&svc_params(args_out)).await.unwrap();
    let invalidated: Vec<String> = out_out
        .get("regime_invalidated")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        invalidated,
        vec!["clearance", "thermal"],
        "域外失效含耦合波及"
    );
    assert_eq!(
        out_out.get("propagation_depth").unwrap().as_i64(),
        Some(2),
        "耦合层传播 depth2"
    );
}

#[tokio::test]
async fn non_numeric_domain_string_fails_visible() {
    let router = GraphDispatchServiceRouter::new(noop_fallback());
    let doc = serde_json::json!({
        "metadata": {
            "constants": [
                { "constant_id": "c1", "version": "2018", "value_hash": "h1", "domain_lo": "abc" }
            ],
            "formula_registry": [
                { "instruction_type": "A", "formula": "f_a", "dimensions": [1, 0, 0, 0, 0, 0, 0], "constants": ["c1"] }
            ]
        }
    });
    let args = JsonValue::object_from_pairs(&[
        ("rule_set", evorule_rule_loader::serde_to_tcb(doc)),
        ("revision", golden_revision(serde_json::json!(5))),
        ("adjudications", golden_adjudications()),
    ]);
    let err = router.execute(&svc_params(args)).await.unwrap_err();
    assert!(
        err.contains("domain_lo"),
        "不可解析域界字符串应点名 domain_lo: {err}"
    );
}

// ── ②c 声明而未引用常数可见性面 ──
//
// 真机实验（G1 缺口实证）：已声明但零指令登记引用的常数（如 SI 级精确重定义
// 的 k_B）对冲击传播零半径且旧出参静默无痕。出参 `unreferenced_constants`
// 使该静默态有痕：每次 invoke 点名声明集 − 引用集差集（字典序）。

#[tokio::test]
async fn unreferenced_declared_constants_are_named_in_report() {
    let router = GraphDispatchServiceRouter::new(noop_fallback());
    // alpha 被引用；k_B 声明而零引用
    let doc = serde_json::json!({
        "metadata": {
            "constants": [
                { "constant_id": "alpha", "version": "design-baseline", "value_hash": "h1", "domain_lo": 0.00002, "domain_hi": 0.000026 },
                { "constant_id": "k_B", "version": "codata-2014", "value_hash": "h2" }
            ],
            "formula_registry": [
                { "instruction_type": "thermal", "formula": "f_t", "dimensions": [1, 0, 0, 0, 0, 0, 0], "constants": ["alpha"] }
            ]
        }
    });
    let revision_alpha = evorule_rule_loader::serde_to_tcb(serde_json::json!({
        "constant_id": "alpha", "new_value": 5,
        "source": "recalibration", "verified": true
    }));
    let args = JsonValue::object_from_pairs(&[
        ("rule_set", evorule_rule_loader::serde_to_tcb(doc)),
        ("revision", revision_alpha),
        ("adjudications", golden_adjudications()),
    ]);
    let out = router.execute(&svc_params(args)).await.unwrap();
    let unreferenced: Vec<String> = out
        .get("unreferenced_constants")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        unreferenced,
        vec!["k_B"],
        "声明而零引用的常数须逐次点名: {out:?}"
    );

    // 对照：全部声明常数均被引用 → 空数组
    let full = serde_json::json!({
        "metadata": {
            "constants": [
                { "constant_id": "c1", "version": "2018", "value_hash": "h1" }
            ],
            "formula_registry": [
                { "instruction_type": "A", "formula": "f_a", "dimensions": [1, 0, 0, 0, 0, 0, 0], "constants": ["c1"] }
            ]
        }
    });
    let args_full = JsonValue::object_from_pairs(&[
        ("rule_set", evorule_rule_loader::serde_to_tcb(full)),
        ("revision", golden_revision(serde_json::json!(5))),
        ("adjudications", golden_adjudications()),
    ]);
    let out_full = router.execute(&svc_params(args_full)).await.unwrap();
    assert!(out_full
        .get("unreferenced_constants")
        .unwrap()
        .as_array()
        .unwrap()
        .is_empty());
}

// ── ③ 篡改即红 ──

#[tokio::test]
async fn tamper_new_value_changes_input_digest() {
    let router = GraphDispatchServiceRouter::new(noop_fallback());
    let out5 = router
        .execute(&svc_params(golden_args(serde_json::json!(5))))
        .await
        .unwrap();
    let out6 = router
        .execute(&svc_params(golden_args(serde_json::json!(6))))
        .await
        .unwrap();
    let d5 = out5.get("input_digest").unwrap().as_str().unwrap();
    let d6 = out6.get("input_digest").unwrap().as_str().unwrap();
    assert_ne!(d5, d6, "new_value 改动 → input_digest 必变（红锚语义）");
}

#[tokio::test]
async fn tamper_formula_changes_graph_digest() {
    let router = GraphDispatchServiceRouter::new(noop_fallback());
    let registry_json = |formula_b: &str| {
        serde_json::json!({
            "metadata": {
                "constants": [
                    { "constant_id": "c1", "version": "2018", "value_hash": "h1", "domain_lo": 0, "domain_hi": 1 }
                ],
                "formula_registry": [
                    { "instruction_type": "A", "formula": "f_a", "dimensions": [1, 0, 0, 0, 0, 0, 0], "constants": ["c1"] },
                    { "instruction_type": "B", "formula": formula_b, "dimensions": [1, 0, 0, 0, 0, 0, 0], "constants": ["c1"] },
                    { "instruction_type": "C", "formula": "f_c", "dimensions": [1, 0, 0, 0, 0, 0, 0] },
                    { "instruction_type": "D", "formula": "f_d", "dimensions": [0, 1, 0, 0, 0, 0, 0] }
                ]
            }
        })
    };
    // 改 formula（B 的公式文本）→ 依赖图内容变 → graph_digest 必变
    let baseline_rule_set = evorule_rule_loader::serde_to_tcb(registry_json("f_b"));
    let tampered_rule_set = evorule_rule_loader::serde_to_tcb(registry_json("f_b_tampered"));
    let args_baseline = JsonValue::object_from_pairs(&[
        ("rule_set", baseline_rule_set),
        ("revision", golden_revision(serde_json::json!(5))),
        ("adjudications", golden_adjudications()),
    ]);
    let args_tampered = JsonValue::object_from_pairs(&[
        ("rule_set", tampered_rule_set),
        ("revision", golden_revision(serde_json::json!(5))),
        ("adjudications", golden_adjudications()),
    ]);
    let out_baseline = router.execute(&svc_params(args_baseline)).await.unwrap();
    let out_tampered = router.execute(&svc_params(args_tampered)).await.unwrap();
    assert_ne!(
        out_baseline.get("graph_digest").unwrap().as_str().unwrap(),
        out_tampered.get("graph_digest").unwrap().as_str().unwrap(),
        "formula 改动 → graph_digest 必变（红锚语义）"
    );
}

// ── ④ fail-visible ──

#[tokio::test]
async fn missing_source_fails_visible() {
    let router = GraphDispatchServiceRouter::new(noop_fallback());
    let doc = serde_json::json!({
        "constant_id": "c1", "new_value": 5, "verified": true
    });
    let args = JsonValue::object_from_pairs(&[
        ("rule_set", golden_rule_set()),
        ("revision", evorule_rule_loader::serde_to_tcb(doc)),
        ("adjudications", golden_adjudications()),
    ]);
    let err = router.execute(&svc_params(args)).await.unwrap_err();
    assert!(err.contains("source"), "缺出处应点名 source: {err}");
    assert!(err.contains("出处元数据"), "应点明出处元数据强制: {err}");
}

#[tokio::test]
async fn verified_false_fails_visible() {
    let router = GraphDispatchServiceRouter::new(noop_fallback());
    let doc = serde_json::json!({
        "constant_id": "c1", "new_value": 5, "source": "s", "verified": false
    });
    let args = JsonValue::object_from_pairs(&[
        ("rule_set", golden_rule_set()),
        ("revision", evorule_rule_loader::serde_to_tcb(doc)),
        ("adjudications", golden_adjudications()),
    ]);
    let err = router.execute(&svc_params(args)).await.unwrap_err();
    assert!(err.contains("verified"), "验证位非真应点名 verified: {err}");
}

#[tokio::test]
async fn unknown_constant_id_fails_visible() {
    let router = GraphDispatchServiceRouter::new(noop_fallback());
    let doc = serde_json::json!({
        "constant_id": "ghost", "new_value": 5, "source": "s", "verified": true
    });
    let args = JsonValue::object_from_pairs(&[
        ("rule_set", golden_rule_set()),
        ("revision", evorule_rule_loader::serde_to_tcb(doc)),
        ("adjudications", golden_adjudications()),
    ]);
    let err = router.execute(&svc_params(args)).await.unwrap_err();
    assert!(err.contains("ghost"), "未知修订对象应点名常数 id: {err}");
}

#[tokio::test]
async fn undeclared_reference_in_rule_set_fails_visible() {
    let router = GraphDispatchServiceRouter::new(noop_fallback());
    // rule_set 引用未声明常数 → 依赖图构建错误透传（谁用谁登记同律）
    let doc = serde_json::json!({
        "metadata": {
            "formula_registry": [
                { "instruction_type": "A", "formula": "f_a", "dimensions": [1, 0, 0, 0, 0, 0, 0], "constants": ["ghost"] }
            ]
        }
    });
    let args = JsonValue::object_from_pairs(&[
        ("rule_set", evorule_rule_loader::serde_to_tcb(doc)),
        ("revision", golden_revision(serde_json::json!(5))),
        ("adjudications", golden_adjudications()),
    ]);
    let err = router.execute(&svc_params(args)).await.unwrap_err();
    assert!(err.contains("ghost"), "应点名未声明常数: {err}");
}

// ── ⑤ 确定性 ──

#[tokio::test]
async fn double_run_output_bytes_identical() {
    let router = GraphDispatchServiceRouter::new(noop_fallback());
    let out1 = router
        .execute(&svc_params(golden_args(serde_json::json!(5))))
        .await
        .unwrap();
    let out2 = router
        .execute(&svc_params(golden_args(serde_json::json!(5))))
        .await
        .unwrap();
    assert_eq!(out1, out2, "同输入双跑输出结构全等");
    let b1 = serde_json::to_vec(&tcb_to_serde_test(&out1)).unwrap();
    let b2 = serde_json::to_vec(&tcb_to_serde_test(&out2)).unwrap();
    assert_eq!(b1, b2, "同输入双跑输出序列化字节全等");
}

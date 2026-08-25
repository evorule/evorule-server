// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! UX 门禁 ↔ 权威校验 对齐集成测试（方案 B 自动报警）
//!
//! `evorule-server` 是唯一同时依赖 `evorule-governance`（权威校验）与
//! `evorule-workspace`（编辑器 UX 门禁 G1-G7）的 crate，因此在此落地"边界行为
//! 一致性"测试。两者校验的是**同一格式**——translate 输出的 core_eval transform
//! （`params.attr`，见 rule_translate.rs translate_to_transform 的对齐注释），
//! 因此可直接对同一份 JSON 断言两边**放行/拒绝结论**一致。
//!
//! # 报警机制
//! 核心仓（evorule-governance）调整校验语义（指令白名单 / 深度限制 / 参数要求）
//! 而 workspace 门禁未同步时，本测试在 CI 中变红 —— 即"自动感知"。
//! 判定标准 = 结论一致，而非错误细节或常量数值相等（rule_translate.rs 常量区口径声明）。
//!
//! # 已知非对称（设计使然, 非缺陷）
//! 两边范围**刻意不同**，不要求全量一致：
//! - workspace 独有（更严的结构门禁）: G3 IO 双相位 / G4 域类型白名单 /
//!   G5 `__` 路径引用 / G6 末条兜底 —— 校验 translate 输出不应产生的结构问题
//! - governance 独有（更严的安全门禁）: 无限循环 / 递归嵌套 / 无界 IO /
//!   payload 增长 / 自引用 / io_request 参数大小 / increment/decrement 参数
//!
//! 这些非对称由 [`documented_stricter_ux_gates`] 显式钉住当前关系，避免静默漂移。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

/// 对同一份 core_eval 规则 JSON，计算 `(UX 门禁通过?, 权威校验通过?)`
///
/// 权威校验返回 `Err`（JSON 解析 / transform 提取失败）视为"权威拒绝"。
fn verdict(json: &str) -> (bool, bool) {
    let ux = evorule_workspace::rule_translate::validate_rule_json(json).valid;
    let auth = evorule_governance::rule_validation::validate_rules_from_json(json)
        .map(|r| r.passed)
        .unwrap_or(false);
    (ux, auth)
}

/// 断言 UX 门禁与权威校验在该输入上的结论一致，且 UX 门的结论符合语料期望
fn assert_agrees(json: &str, name: &str, ux_expected: bool) {
    let (ux, auth) = verdict(json);
    assert_eq!(
        ux, ux_expected,
        "语料前提失效 [{name}]: 期望 UX={ux_expected}, 实际 UX={ux} —— 请复核本测试语料"
    );
    assert_eq!(
        ux,
        auth,
        "对齐失败 [{name}]: UX 门禁={ux} vs 权威校验={auth} —— 核心仓校验语义可能已变更而 workspace 门禁未同步"
    );
}

/// 构造 `levels` 层嵌套 branch 链（每层 on_true 包下一层），底层是 set 指令
fn build_deep_nesting(levels: usize) -> String {
    let mut inner = serde_json::json!({
        "type": "set",
        "params": { "attr": "x", "operation": "set", "value": 1 }
    });
    for _ in 0..levels {
        inner = serde_json::json!({
            "type": "branch",
            "params": {
                "domain": { "type": "all", "inner": [] },
                "on_true": [inner],
                "on_false": []
            }
        });
    }
    serde_json::json!({ "transform": [inner] }).to_string()
}

#[test]
fn ux_gate_matches_authoritative_validation() {
    // A. 合法浅规则（set + all([]) 兜底 branch）→ 双方均通过
    let valid = r#"{
        "transform": [
            { "type": "set", "params": { "attr": "x", "operation": "set", "value": 42 } },
            { "type": "branch", "params": { "domain": { "type": "all", "inner": [] }, "on_true": [] } }
        ]
    }"#;
    assert_agrees(valid, "valid_shallow", true);

    // B. 未知元指令类型 → 双方均拒绝
    //    若核心仓新增/调整合法指令白名单而 workspace G2 未同步，本断言报警。
    let unknown_instr = r#"{"transform":[{ "type": "bogus", "params": {} }]}"#;
    assert_agrees(unknown_instr, "unknown_instruction", false);

    // E. 深度嵌套（70 层，超出双方上限：UX 递归 64 / 权威 branch 嵌套 8）→ 双方均拒绝
    //    若核心仓上调嵌套上限至 ≥70，权威校验将放行，本断言即报警（workspace 门禁未同步）。
    let deep = build_deep_nesting(70);
    assert_agrees(&deep, "deep_nesting_70", false);
}

#[test]
fn documented_depth_boundary_relationship() {
    // 深度边界钉住（对"核心仓调整嵌套深度限制"最灵敏的探测器）:
    // P2-02 已将 governance MAX_NESTING_DEPTH 从 8 对齐到 64（与 UX 递归上限、
    // TCB MAX_BRANCH_DEPTH 三方一致），故 10 层嵌套 branch（深度 10 < 64）
    // 双方均放行——原先"UX 放行、权威拒绝"的非对称边界已消除，此处改为
    // 钉住"对齐后一致放行"。
    // 若核心仓调整嵌套上限使该关系翻转，请先同步 workspace 门禁口径（G7 / 常量区注释），
    // 再更新本断言。
    let depth10 = build_deep_nesting(10);
    let (ux, auth) = verdict(&depth10);
    assert!(
        ux && auth,
        "深度边界语料已失效 [depth_10]: UX 门禁={ux} vs 权威校验={auth} —— 核心仓可能已调整嵌套深度上限, 请核对 workspace 门禁口径"
    );
}

#[test]
fn documented_stricter_ux_gates() {
    // 已知非对称（设计如此，非缺陷）：workspace 结构门禁比权威校验更严。
    // UX 门禁校验的是 translate 输出不应产生的结构问题；权威校验负责安全拦截。
    // 若任一断言失效，说明核心仓已开始拦截此类结构（非对称消除，属预期演进），
    // 更新本语料并在上方"已知非对称"清单同步即可。

    // G4 域类型白名单: UX 拒绝未知域类型; 权威不校验域类型 → 放行
    let unknown_domain = r#"{"transform":[{ "type": "branch",
        "params": { "domain": { "type": "bogus_domain", "inner": [] }, "on_true": [] } }]}"#;
    let (ux_g4, auth_g4) = verdict(unknown_domain);
    assert!(
        !ux_g4 && auth_g4,
        "非对称语料已失效 [G4 unknown_domain]: UX 门禁={ux_g4} vs 权威校验={auth_g4} —— 核心仓可能已开始校验域类型"
    );

    // G6 末条兜底: UX 强制末条 branch + all([]); 权威不要求兜底 → 放行
    let no_fallback = r#"{"transform":[{ "type": "set",
        "params": { "attr": "x", "operation": "set", "value": 1 } }]}"#;
    let (ux_g6, auth_g6) = verdict(no_fallback);
    assert!(
        !ux_g6 && auth_g6,
        "非对称语料已失效 [G6 no_fallback]: UX 门禁={ux_g6} vs 权威校验={auth_g6} —— 核心仓可能已开始强制兜底规则"
    );
}

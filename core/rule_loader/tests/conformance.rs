// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! [CON-R-005] conformance 环测试（SR-15 三阶段 件4）
//!
//! 消费宪法仓（evorule-system-rules）`acceptance/golden/rule_set/` 的**装载档**
//! 金样例（LOADER_DEFS 6 对，与 tools/check_golden_coverage.py 同批文件），
//! 走真装载路径 `load_rule_set`：
//!
//! - 断言一：装载档合法集 × `load_rule_set()` → 全 Ok（且 engine_rules 非空）
//! - 断言二：装载档非法集 → 全 Err，且拒绝面 = schema 门（非法装载档金样例
//!   按制度必为 schema 级拒绝——与 coverage 脚本 schema 双向断言同构对齐；
//!   门级非法样例属 schema 档，不入装载档目录）
//!
//! 源仓定位（仿 scripts/check_schema_sync.py locate_source）：
//! `EVORULE_RULES_SRC` 环境变量优先 → 兄弟目录 `<检出根>/evorule-system-rules`。
//! 兄弟仓不在时本测试 panic（环测试是闭环件，缺源仓=环境不完整，fail-fast）。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use evorule_rule_loader::{load_rule_set, ConstitutionError, LoadContext};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// 装载档金样例目录（相对宪法仓根）
const GOLDEN_SUBDIR: &str = "acceptance/golden/rule_set";

/// 装载档 def 名清单（SSOT = 宪法仓 tools/check_golden_coverage.py LOADER_DEFS；
/// 双侧清单漂移由「conformance 全绿 + coverage 100%」两判据联合钳制）
const LOADER_DEFS: [&str; 6] = [
    "path",
    "payload_attr_path",
    "path_or_literal",
    "domain",
    "instruction",
    "transform_rule",
];

/// 宪法仓定位：`EVORULE_RULES_SRC` → 兄弟目录
/// （本 crate 位于 <检出根>/evorule-server/core/rule_loader，三级 parent 即检出根）
fn locate_rules_repo() -> Option<PathBuf> {
    let anchor = Path::new("schemas").join("rule_set").join("v1.0.json");
    if let Ok(src) = std::env::var("EVORULE_RULES_SRC") {
        let p = PathBuf::from(src);
        if p.join(&anchor).is_file() {
            return Some(p);
        }
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let checkout_root = manifest
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)?;
    let sibling = checkout_root.join("evorule-system-rules");
    if sibling.join(&anchor).is_file() {
        return Some(sibling);
    }
    None
}

#[test]
fn conformance_golden_loader_docs() {
    // 确定性钳制：discipline 门 enforce 位不受开发者环境干扰
    // （缺省路径 = enforce 语义——[CON-L2-004] W1 观测期豁免已收口，SR-16；
    //   合法金样例自身合规，enforce 下必须全过；显式 warn 环境不得污染判定）
    std::env::remove_var("EVORULE_DISCIPLINE_GATE");

    let repo = locate_rules_repo().unwrap_or_else(|| {
        panic!(
            "找不到 evorule-system-rules 宪法仓（conformance 环测试是闭环件，\
             缺源仓=环境不完整）。设 EVORULE_RULES_SRC 或将仓置于兄弟目录"
        )
    });
    let golden_dir = repo.join(GOLDEN_SUBDIR);

    let mut legal_loaded = 0usize;
    let mut illegal_rejected = 0usize;

    for def in LOADER_DEFS {
        for (side, expect_valid) in [("valid", true), ("invalid", false)] {
            let path = golden_dir.join(format!("{def}.{side}.json"));
            let raw = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("金样例读取失败 {}: {e}", path.display()));
            let doc: Value = serde_json::from_str(&raw)
                .unwrap_or_else(|e| panic!("金样例 JSON 解析失败 {}: {e}", path.display()));

            let ctx = LoadContext::business_file(path.to_string_lossy().as_ref());
            let outcome = load_rule_set(&doc, &ctx);

            match (expect_valid, outcome) {
                (true, Ok(loaded)) => {
                    assert!(
                        !loaded.engine_rules.is_empty(),
                        "合法金样例装载后 engine_rules 为空: {}",
                        path.display()
                    );
                    legal_loaded += 1;
                }
                (true, Err(e)) => panic!("合法金样例被拒 {}:\n{e}", path.display()),
                (false, Ok(_)) => {
                    panic!("非法金样例被放行（拒绝面失守）: {}", path.display())
                }
                (false, Err(ConstitutionError::SchemaViolation(fe))) => {
                    // 拒绝面=schema 门（制度：装载档非法样例必为 schema 级拒绝）；
                    // 顺手断言四段式报错非残缺（[CON-K2-001] 雏形面）
                    assert!(
                        !fe.violation.is_empty() && !fe.minimal_example.is_empty(),
                        "四段式报错残缺: {}",
                        path.display()
                    );
                    illegal_rejected += 1;
                }
                (false, Err(e)) => {
                    panic!("非法金样例被拒但拒绝面非 schema 门 {}: {e}", path.display())
                }
            }
        }
    }

    assert_eq!(legal_loaded, LOADER_DEFS.len(), "合法集未全数通过");
    assert_eq!(illegal_rejected, LOADER_DEFS.len(), "非法集未全数拒绝");
    println!(
        "[CON-R-005] conformance 全绿：装载档金样例 {legal_loaded} 合法全过 / {illegal_rejected} 非法全拒"
    );
}

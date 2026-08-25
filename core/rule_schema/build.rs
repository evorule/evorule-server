// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 构建期门禁：三个内嵌 schema 文件必须是合法 JSON，且跨文件 $ref 的 $id 自洽。
//!
//! 作用：把「schema 损坏」从运行时问题提前到构建期问题（与转译器同一纪律）。
//! 若 schema 改动后不同步（缺失字段/非法 JSON/$id 漂移），本脚本直接令构建失败。
//!
//! C5 纪律：构建脚本同样禁止 unwrap/expect/panic（deny 级 lint），
//! 所有失败路径统一以 `Err(String)` 返回，由 `main` 转非零退出码令构建失败。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const RULE_SET: &str = "schemas/rule_set/v1.0.json";
const META: &str = "schemas/_meta/v1.0.json";
const SHARED: &str = "schemas/_shared/v1.0.json";

const RULE_SET_ID: &str = "https://evorule.org/schemas/rule_set/v1.0.json";
const META_ID: &str = "https://evorule.org/schemas/_meta/v1.0.json";
const SHARED_ID: &str = "https://evorule.org/schemas/_shared/v1.0.json";

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("evorule-rule-schema build gate FAILED: {msg}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// 读取并解析 schema JSON；失败返回带文件路径的构建期错误信息
fn load_json(path: &Path) -> Result<serde_json::Value, String> {
    let raw = fs::read_to_string(path)
        .map_err(|e| format!("读取 schema {} 失败: {}", path.display(), e))?;
    serde_json::from_str(&raw)
        .map_err(|e| format!("schema {} 不是合法 JSON: {}", path.display(), e))
}

/// 取 schema 的 $id 字符串
fn id_of<'a>(doc: &'a serde_json::Value, name: &str) -> Result<&'a str, String> {
    doc.get("$id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("{name} 缺 $id"))
}

fn run() -> Result<(), String> {
    let manifest_dir = PathBuf::from(
        env::var("CARGO_MANIFEST_DIR").map_err(|e| format!("读取 CARGO_MANIFEST_DIR 失败: {e}"))?,
    );
    for rel in [RULE_SET, META, SHARED] {
        let p = manifest_dir.join(rel);
        load_json(&p)?;
        println!("cargo:rerun-if-changed={}", p.display());
    }

    let rs = load_json(&manifest_dir.join(RULE_SET))?;
    let meta = load_json(&manifest_dir.join(META))?;
    let shared = load_json(&manifest_dir.join(SHARED))?;

    let rs_id = id_of(&rs, "rule_set")?;
    let meta_id = id_of(&meta, "_meta")?;
    let shared_id = id_of(&shared, "_shared")?;

    if rs_id != RULE_SET_ID {
        return Err(format!("rule_set $id 漂移: {rs_id}"));
    }
    if meta_id != META_ID {
        return Err(format!("_meta $id 漂移: {meta_id}"));
    }
    if shared_id != SHARED_ID {
        return Err(format!("_shared $id 漂移: {shared_id}"));
    }

    // rule_set 的 allOf[0] 必须指向 meta，transform.items 必须指向 shared#/$defs/transform_rule
    let all_of = rs
        .get("allOf")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "rule_set 缺 allOf".to_string())?;
    let first_ref = all_of
        .first()
        .and_then(|v| v.get("$ref"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "rule_set allOf[0] 缺 $ref".to_string())?;
    if first_ref != "../_meta/v1.0.json" {
        return Err(format!("rule_set allOf[0] 未指向 _meta: {first_ref}"));
    }

    let items_ref = rs
        .get("allOf")
        .and_then(|v| v.as_array())
        .and_then(|v| v.get(1))
        .and_then(|v| v.get("properties"))
        .and_then(|v| v.get("transform"))
        .and_then(|v| v.get("items"))
        .and_then(|v| v.get("$ref"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "transform.items 缺 $ref".to_string())?;
    if items_ref != "../_shared/v1.0.json#/$defs/transform_rule" {
        return Err(format!(
            "transform.items 未指向 shared transform_rule: {items_ref}"
        ));
    }

    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}

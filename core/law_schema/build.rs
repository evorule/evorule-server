// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 构建期门禁：三件内嵌 schema 文件必须是合法 JSON，且 $id 自洽、定稿锚不漂移。
//!
//! 与 core/rule_schema 的构建门同族：把「schema 损坏/漂移」从运行时问题提前到
//! 构建期问题。三件 = law_set（kind 定稿全文）+ _meta（治理骨架）+ _shared（演进位）。
//! $id 归 law_set 家族命名空间（与 rule_schema 顶层 _meta/_shared 全量件隔离）。
//!
//! C5 纪律：构建脚本同样禁止 unwrap/expect/panic（deny 级 lint），
//! 所有失败路径统一以 `Err(String)` 返回，由 `main` 转非零退出码令构建失败。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

const LAW_SET: &str = "schemas/law_set/v1.0.json";
const META: &str = "schemas/_meta/v1.0.json";
const SHARED: &str = "schemas/_shared/v1.0.json";

const LAW_SET_ID: &str = "https://evorule.org/schemas/law_set/v1.0.json";
const META_ID: &str = "https://evorule.org/schemas/law_set/_meta/v1.0.json";
const SHARED_ID: &str = "https://evorule.org/schemas/law_set/_shared/v1.0.json";

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("evorule-law-schema build gate FAILED: {msg}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// 读取并解析 schema JSON；失败返回带文件路径的构建期错误信息
fn load_json(path: &Path) -> Result<Value, String> {
    let raw = fs::read_to_string(path)
        .map_err(|e| format!("读取 schema {} 失败: {}", path.display(), e))?;
    serde_json::from_str(&raw)
        .map_err(|e| format!("schema {} 不是合法 JSON: {}", path.display(), e))
}

/// 取 schema 的 $id 字符串
fn id_of<'a>(doc: &'a Value, name: &str) -> Result<&'a str, String> {
    doc.get("$id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("{name} 缺 $id"))
}

/// 取指定 pointer 的字符串值（定稿锚读取）
fn str_at<'a>(doc: &'a Value, pointer: &str) -> Result<&'a str, String> {
    doc.pointer(pointer)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("law_set schema 缺定稿锚 {pointer}"))
}

/// 取指定 pointer 的 JSON 值（定稿锚读取）
fn val_at<'a>(doc: &'a Value, pointer: &str) -> Result<&'a Value, String> {
    doc.pointer(pointer)
        .ok_or_else(|| format!("law_set schema 缺定稿锚 {pointer}"))
}

/// 数组含元素（定稿锚断言辅助）
fn arr_contains(doc: &Value, pointer: &str, expected: &str) -> Result<(), String> {
    let arr = val_at(doc, pointer)?
        .as_array()
        .ok_or_else(|| format!("law_set schema 定稿锚 {pointer} 非数组"))?;
    if arr.iter().any(|v| v.as_str() == Some(expected)) {
        Ok(())
    } else {
        Err(format!("law_set schema 定稿锚 {pointer} 未含 {expected}"))
    }
}

fn run() -> Result<(), String> {
    let manifest_dir = PathBuf::from(
        env::var("CARGO_MANIFEST_DIR").map_err(|e| format!("读取 CARGO_MANIFEST_DIR 失败: {e}"))?,
    );
    for rel in [LAW_SET, META, SHARED] {
        let p = manifest_dir.join(rel);
        load_json(&p)?;
        println!("cargo:rerun-if-changed={}", p.display());
    }

    let law_set = load_json(&manifest_dir.join(LAW_SET))?;
    let meta = load_json(&manifest_dir.join(META))?;
    let shared = load_json(&manifest_dir.join(SHARED))?;

    if id_of(&law_set, "law_set")? != LAW_SET_ID {
        return Err(format!("law_set $id 漂移: {}", id_of(&law_set, "law_set")?));
    }
    if id_of(&meta, "_meta")? != META_ID {
        return Err(format!("_meta $id 漂移: {}", id_of(&meta, "_meta")?));
    }
    if id_of(&shared, "_shared")? != SHARED_ID {
        return Err(format!("_shared $id 漂移: {}", id_of(&shared, "_shared")?));
    }

    // ── law_set v1.0 定稿锚（防手改漂移：kind/$schema const + law $def 结构）──
    if str_at(&law_set, "/properties/kind/const")? != "law_set" {
        return Err("law_set properties.kind.const 定稿锚漂移".to_string());
    }
    if str_at(&law_set, "/properties/$schema/const")? != LAW_SET_ID {
        return Err("law_set properties.$schema.const 定稿锚漂移".to_string());
    }
    for field in ["id", "formula", "symbols", "si_units"] {
        arr_contains(&law_set, "/$defs/law/required", field)?;
    }
    if val_at(&law_set, "/$defs/law/additionalProperties")? != &serde_json::json!(false) {
        return Err(
            "law_set $defs.law.additionalProperties 必须为 false（未知键 fail-fast）".to_string(),
        );
    }
    if str_at(&law_set, "/properties/laws/items/$ref")? != "#/$defs/law" {
        return Err("law_set properties.laws.items.$ref 未指向内嵌 law $def".to_string());
    }
    if val_at(&law_set, "/properties/laws/maxItems")? != &serde_json::json!(128) {
        return Err("law_set properties.laws.maxItems 定稿锚漂移（须 128）".to_string());
    }

    // ── _meta 治理骨架锚：kind 封闭 law_set ──
    if str_at(&meta, "/properties/kind/const")? != "law_set" {
        return Err("_meta properties.kind.const 未封闭 law_set".to_string());
    }

    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}

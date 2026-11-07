// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 构建期门禁：两件内嵌 schema 文件必须是合法 JSON，且 $id 自洽、定稿锚不漂移。
//!
//! 与 core/law_schema 的构建门同族：把「schema 损坏/漂移」从运行时问题提前到
//! 构建期问题。两件 = verify_request（验证命题输入）+ constant_set（常量集部署件）。
//! $id 归 criticality 命名空间（与 law_set / rule_set 家族隔离）。
//!
//! C5 纪律：构建脚本同样禁止 unwrap/expect/panic（deny 级 lint），
//! 所有失败路径统一以 `Err(String)` 返回，由 `main` 转非零退出码令构建失败。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

const VERIFY_REQUEST: &str = "schemas/verify_request/v1.0.json";
const CONSTANT_SET: &str = "schemas/constant_set/v1.0.json";

const VERIFY_REQUEST_ID: &str = "https://evorule.org/schemas/criticality/verify_request/v1.0.json";
const CONSTANT_SET_ID: &str = "https://evorule.org/schemas/criticality/constant_set/v1.0.json";

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("evorule-criticality build gate FAILED: {msg}");
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
        .ok_or_else(|| format!("criticality schema 缺定稿锚 {pointer}"))
}

/// 取指定 pointer 的 JSON 值（定稿锚读取）
fn val_at<'a>(doc: &'a Value, pointer: &str) -> Result<&'a Value, String> {
    doc.pointer(pointer)
        .ok_or_else(|| format!("criticality schema 缺定稿锚 {pointer}"))
}

fn run() -> Result<(), String> {
    let manifest_dir = PathBuf::from(
        env::var("CARGO_MANIFEST_DIR").map_err(|e| format!("读取 CARGO_MANIFEST_DIR 失败: {e}"))?,
    );
    for rel in [VERIFY_REQUEST, CONSTANT_SET] {
        let p = manifest_dir.join(rel);
        load_json(&p)?;
        println!("cargo:rerun-if-changed={}", p.display());
    }

    let verify_request = load_json(&manifest_dir.join(VERIFY_REQUEST))?;
    let constant_set = load_json(&manifest_dir.join(CONSTANT_SET))?;

    if id_of(&verify_request, "verify_request")? != VERIFY_REQUEST_ID {
        return Err(format!(
            "verify_request $id 漂移: {}",
            id_of(&verify_request, "verify_request")?
        ));
    }
    if id_of(&constant_set, "constant_set")? != CONSTANT_SET_ID {
        return Err(format!(
            "constant_set $id 漂移: {}",
            id_of(&constant_set, "constant_set")?
        ));
    }

    // ── verify_request v1.0 定稿锚（防手改漂移）──
    if str_at(&verify_request, "/properties/$schema/const")? != VERIFY_REQUEST_ID {
        return Err("verify_request properties.$schema.const 定稿锚漂移".to_string());
    }
    for field in ["subject", "claims", "context"] {
        let required = val_at(&verify_request, "/required")?
            .as_array()
            .ok_or_else(|| "verify_request required 非数组".to_string())?;
        if !required.iter().any(|v| v.as_str() == Some(field)) {
            return Err(format!("verify_request required 未含 {field}"));
        }
    }
    if val_at(&verify_request, "/additionalProperties")? != &serde_json::json!(false) {
        return Err(
            "verify_request additionalProperties 必须为 false（未知键 fail-fast）".to_string(),
        );
    }
    if val_at(&verify_request, "/properties/claims/maxItems")? != &serde_json::json!(64) {
        return Err("verify_request claims.maxItems 定稿锚漂移（须 64）".to_string());
    }
    if str_at(&verify_request, "/properties/claims/items/$ref")? != "#/$defs/claim" {
        return Err("verify_request claims.items.$ref 未指向内嵌 claim $def".to_string());
    }
    if val_at(&verify_request, "/$defs/claim/additionalProperties")? != &serde_json::json!(false) {
        return Err("claim $def additionalProperties 必须为 false".to_string());
    }

    // ── constant_set v1.0 定稿锚（kind 封闭 + additionalProperties:false）──
    if str_at(&constant_set, "/properties/kind/const")? != "constant_set" {
        return Err("constant_set properties.kind.const 定稿锚漂移（须 constant_set）".to_string());
    }
    if str_at(&constant_set, "/properties/$schema/const")? != CONSTANT_SET_ID {
        return Err("constant_set properties.$schema.const 定稿锚漂移".to_string());
    }
    if val_at(&constant_set, "/additionalProperties")? != &serde_json::json!(false) {
        return Err("constant_set additionalProperties 必须为 false".to_string());
    }
    if val_at(&constant_set, "/$defs/constant/additionalProperties")? != &serde_json::json!(false) {
        return Err("constant $def additionalProperties 必须为 false".to_string());
    }
    if str_at(
        &constant_set,
        "/$defs/constant/properties/metadata/required/0",
    )? != "source"
    {
        return Err("constant metadata required 未封闭 source（铁律 2：无出处拒收）".to_string());
    }

    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 规则文件加载器
//!
//! # Schema 门禁（线1 防御层, records/77）
//! `load_rules` 对每个解析成功的规则文件先跑 `evorule_rule_schema::validate_rule_input`：
//! - 合规（引擎原生结构，`{transform:[]}` 或裸 transform 数组）→ 放行
//! - 不合规 → 跳过该文件并记录 warn（含具体 Schema 错误），**不向 server 推送**，
//!   防止 Schema 非法规则进入引擎（TCB 只保证确定性执行，不保证正确性）。

use serde_json::Value;
use std::fs;
use std::path::Path;
use tracing::{info, warn};

/// 加载规则文件
///
/// 递归扫描目录（含 `bundles/{bundle_id}/` 子目录，T3）下所有 `.json` 文件
/// （排除 `bundle_manifest.json`），按完整路径字典序逐个解析为 `serde_json::Value`
/// （确定性加载顺序）。解析失败或未通过 Schema 门禁的文件会被跳过（记录 warn 日志），
/// 不会中断整体加载。
pub fn load_rules(dir: &Path) -> Result<Vec<Value>, String> {
    if !dir.exists() {
        return Err(format!("规则目录不存在: {}", dir.display()));
    }

    let mut paths = Vec::new();
    collect_json_files_recursive(dir, &mut paths);
    paths.sort();

    let rules: Vec<Value> = paths
        .into_iter()
        .filter_map(|path| match load_rule_file(&path) {
            Ok(rule) => {
                // Schema 门禁：引擎原生结构校验（线1 拦截，不静默放行）
                let report = evorule_rule_schema::validate_rule_input(&rule);
                if !report.valid {
                    let detail = report.errors.join("; ");
                    warn!(
                        file = %path.display(),
                        schema = %detail,
                        "规则文件未通过 Schema 门禁，跳过（不推送 server）"
                    );
                    return None;
                }
                info!(file = %path.display(), "加载规则文件（Schema 门禁通过）");
                Some(rule)
            }
            Err(e) => {
                warn!(file = %path.display(), error = %e, "加载规则文件失败，跳过");
                None
            }
        })
        .collect();

    if rules.is_empty() {
        warn!(dir = %dir.display(), "未找到任何规则文件");
    }

    Ok(rules)
}

/// 递归收集 `.json` 文件路径（T3）：子目录展开，排除 `bundle_manifest.json`；
/// 跳过隐藏目录（`.` 前缀，T4：`.tmp/.bak/.stale` 等临时/备份目录不参与加载）。
fn collect_json_files_recursive(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') {
                continue; // 隐藏目录：临时/备份目录不参与加载
            }
            collect_json_files_recursive(&p, out);
        } else if is_json_file(&p)
            && p.file_name()
                .map(|n| n != evorule_bundle::BUNDLE_MANIFEST_FILE)
                .unwrap_or(true)
        {
            out.push(p);
        }
    }
}

/// 判断路径是否为 `.json` 文件
fn is_json_file(path: &Path) -> bool {
    path.is_file() && path.extension().map(|e| e == "json").unwrap_or(false)
}

/// 加载单个规则文件
pub fn load_rule_file(path: &Path) -> Result<Value, String> {
    let content = fs::read_to_string(path).map_err(|e| format!("读取文件失败: {}", e))?;

    serde_json::from_str(&content).map_err(|e| format!("JSON 解析失败: {}", e))
}

/// 获取规则文件列表（递归，含子目录，排除 `bundle_manifest.json`）
///
/// 读取失败时返回错误，不静默返回空列表（防止调用方误判为"无规则"）。
pub fn list_rule_files(dir: &Path) -> Result<Vec<String>, String> {
    let mut paths = Vec::new();
    collect_json_files_recursive(dir, &mut paths);
    paths.sort();
    Ok(paths
        .into_iter()
        .map(|p| p.display().to_string())
        .collect())
}

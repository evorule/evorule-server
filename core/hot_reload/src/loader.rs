// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 规则文件加载器

use serde_json::Value;
use std::fs;
use std::path::Path;
use tracing::{info, warn};

/// 加载规则文件
///
/// 扫描目录下所有 `.json` 文件，逐个解析为 `serde_json::Value`。
/// 解析失败的文件会被跳过（记录 warn 日志），不会中断整体加载。
pub fn load_rules(dir: &Path) -> Result<Vec<Value>, String> {
    if !dir.exists() {
        return Err(format!("规则目录不存在: {}", dir.display()));
    }

    let entries = fs::read_dir(dir).map_err(|e| format!("读取规则目录失败: {}", e))?;

    let rules: Vec<Value> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_json_file(p))
        .filter_map(|path| match load_rule_file(&path) {
            Ok(rule) => {
                info!(file = %path.display(), "加载规则文件");
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

/// 判断路径是否为 `.json` 文件
fn is_json_file(path: &Path) -> bool {
    path.is_file() && path.extension().map(|e| e == "json").unwrap_or(false)
}

/// 加载单个规则文件
pub fn load_rule_file(path: &Path) -> Result<Value, String> {
    let content = fs::read_to_string(path).map_err(|e| format!("读取文件失败: {}", e))?;

    serde_json::from_str(&content).map_err(|e| format!("JSON 解析失败: {}", e))
}

/// 获取规则文件列表
pub fn list_rule_files(dir: &Path) -> Vec<String> {
    let mut files = Vec::new();

    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().map(|e| e == "json").unwrap_or(false) {
                files.push(path.display().to_string());
            }
        }
    }

    files
}

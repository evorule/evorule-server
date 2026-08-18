// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! Memory I/O Handler —— 基于 `tokio::fs` 实现持久化键值存储。
//!
//! - 写模式：参数包含 `value` 字段时，将内容写入 `base_dir/<key>` 文件，返回 `JsonValue::Bool(true)`。
//! - 读模式：参数不包含 `value` 字段时，读取 `base_dir/<key>` 文件内容，返回 `JsonValue::String(content)`。
//!
//! 通过文件系统实现简单的持久化记忆，适用于规则上下文、缓存等场景。

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use evorule_reactor::{IoHandler, IoResult};
use evorule_tcb::JsonValue;

/// 单次文件 I/O 超时（Memory 5s，防止 NFS/网络文件系统卡住）
const MEMORY_TIMEOUT: Duration = Duration::from_secs(5);

/// Memory 处理器
///
/// 以文件系统为后端的键值存储。所有键被映射为 `base_dir` 下的文件路径。
pub struct MemoryHandler {
    /// 存储根目录
    base_dir: PathBuf,
}

impl MemoryHandler {
    /// 创建新的 Memory 处理器。
    ///
    /// # 参数
    /// - `base_dir`: 存储根目录，所有键将作为该目录下的文件。
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    /// 解析键对应的文件路径。
    ///
    /// 路径遍历防护：把所有可能用于逃逸 `base_dir` 的字符替换为下划线，
    /// 确保最终路径始终是 `base_dir` 下的单层文件名。
    ///
    /// 替换的字符：
    /// - `/` 和 `\`：路径分隔符（跨平台）
    /// - `..`：父目录引用
    /// - `:`：Windows 盘符（`C:`）与 NTFS alternate data stream（`file:stream`）
    ///
    /// 旧实现未处理 `:`，在 Windows 上 key 含盘符可能触发异常路径语义。
    fn resolve_path(&self, key: &str) -> PathBuf {
        let safe_key = key.replace(['/', '\\', ':'], "_").replace("..", "_");
        self.base_dir.join(safe_key)
    }
}

#[async_trait]
impl IoHandler for MemoryHandler {
    async fn execute(&self, params: &JsonValue) -> IoResult {
        // 提取 key（必需）
        let key = params
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required param: key".to_string())?;

        // N6 修复：限制 key 长度，防止超长 key 触发 OS 文件名错误
        //（大多数文件系统限制单个文件名 ≤ 255 字节）
        const MAX_KEY_LEN: usize = 255;
        if key.len() > MAX_KEY_LEN {
            return Err(format!(
                "key too long: {} bytes (max {})",
                key.len(),
                MAX_KEY_LEN
            ));
        }

        let path = self.resolve_path(key);

        // 根据 value 是否存在区分写/读模式
        if let Some(value) = params.get("value") {
            // 写模式：value 必须为字符串
            let content = value
                .as_str()
                .ok_or_else(|| "param 'value' must be a string".to_string())?;

            // 确保父目录存在（5s 超时）
            if let Some(parent) = path.parent() {
                tokio::time::timeout(MEMORY_TIMEOUT, tokio::fs::create_dir_all(parent))
                    .await
                    .map_err(|_| {
                        format!("create dir timed out after {}s", MEMORY_TIMEOUT.as_secs())
                    })?
                    .map_err(|e| format!("create dir failed: {e}"))?;
            }

            // 写入文件（5s 超时）
            tokio::time::timeout(MEMORY_TIMEOUT, tokio::fs::write(&path, content))
                .await
                .map_err(|_| format!("write file timed out after {}s", MEMORY_TIMEOUT.as_secs()))?
                .map_err(|e| format!("write file failed: {e}"))?;

            Ok(JsonValue::Bool(true))
        } else {
            // 读模式（5s 超时）
            // 区分 NotFound 与其他 I/O 错误，便于上层做"键不存在则用默认值"模式。
            // 旧实现把 NotFound 混入通用 "read file failed: ..."，上层无法判断。
            let read_result =
                tokio::time::timeout(MEMORY_TIMEOUT, tokio::fs::read_to_string(&path))
                    .await
                    .map_err(|_| {
                        format!("read file timed out after {}s", MEMORY_TIMEOUT.as_secs())
                    })?;
            match read_result {
                Ok(content) => Ok(JsonValue::String(content.into())),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    Err(format!("key not found: {key}"))
                }
                Err(e) => Err(format!("read file failed: {e}")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used, clippy::panic)]
    use super::*;
    use evorule_tcb::JsonValue;
    use std::sync::atomic::{AtomicU64, Ordering};

    static MEM_COUNTER: AtomicU64 = AtomicU64::new(0);
    fn temp_mem_dir() -> PathBuf {
        let n = MEM_COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("evorule_io_mem_{}_{}", std::process::id(), n))
    }

    fn params(pairs: &[(&str, JsonValue)]) -> JsonValue {
        JsonValue::object_from_pairs(pairs)
    }

    #[test]
    fn test_resolve_path_sanitizes_traversal() {
        let base = PathBuf::from("/tmp/mem");
        let handler = MemoryHandler::new(base.clone());
        let p = handler.resolve_path("../etc/passwd");
        // 路径穿越字符应被替换，最终路径仍在 base_dir 内
        let s = p.to_string_lossy();
        assert!(!s.contains(".."), "path should not contain ..: {s}");
        assert!(
            p.starts_with(&base),
            "path must stay within base_dir: {p:?} vs base {base:?}"
        );
    }

    #[test]
    fn test_resolve_path_replaces_slashes() {
        let handler = MemoryHandler::new(PathBuf::from("/tmp/mem"));
        let p = handler.resolve_path("a/b/c");
        // key 的 `/` `\` `:` 应被替换为 `_`，文件名组件只含安全字符
        // （base_dir 本身在 Windows 上含 `\`，故只检查 file_name 部分）
        let file_name = p.file_name().unwrap().to_string_lossy();
        assert_eq!(file_name.to_string(), "a_b_c");
    }

    #[test]
    fn test_resolve_path_sanitizes_colon() {
        // regression for R-1: 旧实现未处理 `:`，Windows 盘符/ADS 风险
        let handler = MemoryHandler::new(PathBuf::from("/tmp/mem"));
        let p = handler.resolve_path("C:file");
        let s = p.to_string_lossy();
        assert!(!s.contains("C:file"), "colon should be replaced: {s}");
    }

    #[test]
    fn test_resolve_path_strips_dotdot_variants() {
        let base = PathBuf::from("/tmp/mem");
        let handler = MemoryHandler::new(base.clone());
        for key in ["..", "..\\..", "a..b", "....", "../..", "..%2f"] {
            let p = handler.resolve_path(key);
            assert!(p.starts_with(&base), "key {key:?} escaped base_dir: {p:?}");
        }
    }

    // ===== MemoryHandler::execute 集成测试（临时目录）=====

    #[tokio::test]
    async fn test_memory_write_read_roundtrip() {
        let dir = temp_mem_dir();
        let handler = MemoryHandler::new(dir.clone());
        // 写
        let r = handler
            .execute(&params(&[
                ("key", JsonValue::string("k1")),
                ("value", JsonValue::string("hello-memory")),
            ]))
            .await
            .unwrap();
        assert_eq!(r.as_bool(), Some(true));
        // 读
        let r = handler
            .execute(&params(&[("key", JsonValue::string("k1"))]))
            .await
            .unwrap();
        assert_eq!(r.as_str(), Some("hello-memory"));
    }

    #[tokio::test]
    async fn test_memory_overwrite() {
        let dir = temp_mem_dir();
        let handler = MemoryHandler::new(dir);
        handler
            .execute(&params(&[
                ("key", JsonValue::string("k")),
                ("value", JsonValue::string("v1")),
            ]))
            .await
            .unwrap();
        handler
            .execute(&params(&[
                ("key", JsonValue::string("k")),
                ("value", JsonValue::string("v2")),
            ]))
            .await
            .unwrap();
        let r = handler
            .execute(&params(&[("key", JsonValue::string("k"))]))
            .await
            .unwrap();
        assert_eq!(r.as_str(), Some("v2"));
    }

    #[tokio::test]
    async fn test_memory_read_not_found_distinct() {
        // regression for D-3: 旧实现把 NotFound 混入 "read file failed"，无法区分
        let dir = temp_mem_dir();
        let handler = MemoryHandler::new(dir);
        let r = handler
            .execute(&params(&[("key", JsonValue::string("never-written"))]))
            .await;
        assert!(r.is_err());
        let err = r.unwrap_err();
        assert!(
            err.contains("key not found"),
            "NotFound must be distinguishable: {err}"
        );
    }

    #[tokio::test]
    async fn test_memory_missing_key() {
        let dir = temp_mem_dir();
        let handler = MemoryHandler::new(dir);
        let r = handler
            .execute(&params(&[("not_key", JsonValue::string("x"))]))
            .await;
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("missing required param: key"));
    }

    #[tokio::test]
    async fn test_memory_value_must_be_string() {
        let dir = temp_mem_dir();
        let handler = MemoryHandler::new(dir);
        let r = handler
            .execute(&params(&[
                ("key", JsonValue::string("k")),
                ("value", JsonValue::Integer(42)),
            ]))
            .await;
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("value"));
    }

    #[tokio::test]
    async fn test_memory_path_traversal_execute_layer() {
        // execute 层路径遍历防护：恶意 key 不能写到 base_dir 外。
        // 若防护失败，文件会落在 base_dir 的父目录，base_dir 内不会有该文件。
        let dir = temp_mem_dir();
        let handler = MemoryHandler::new(dir.clone());
        handler
            .execute(&params(&[
                ("key", JsonValue::string("../escape")),
                ("value", JsonValue::string("evil")),
            ]))
            .await
            .unwrap();
        // 清洗后的文件应存在于 base_dir 内
        let entries = std::fs::read_dir(&dir).unwrap();
        let names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            names.iter().any(|n| n.contains("escape")),
            "sanitized file should exist in base_dir: {names:?}"
        );
    }
}

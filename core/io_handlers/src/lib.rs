// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! EvoRule I/O Handler 实现 —— DB/HTTP/Memory
//!
//! # H5 迁移背景
//!
//! 此 crate 从 `evorule-governance/src/io_handlers/` 迁出,属于应用层(策略)。
//!
//! 依赖 `evorule-reactor` 的 `IoHandler` trait,不依赖 `evorule-governance`,
//! 避免循环依赖(evorule-governance 的 IoDispatcher 框架是机制,留核心)。
//!
//! # 模块结构
//! - `db_handler` — SQLite 数据库 I/O(基于 sqlx)
//! - `http_handler` — HTTP 请求 I/O(基于 reqwest，支持 GET/POST/PUT/PATCH/DELETE/HEAD)
//! - `memory_handler` — 文件系统键值存储 I/O(基于 tokio::fs)

#![forbid(unsafe_code)]

pub mod db_handler;
pub mod http_handler;
pub mod memory_handler;
pub mod service_registry;

pub use db_handler::{DbHandler, StatementEntry, StatementWhitelist, WhitelistedDbHandler};
pub use http_handler::HttpHandler;
pub use memory_handler::MemoryHandler;
pub use service_registry::{ServiceEntry, ServiceRegistry, ServiceRegistryHandler};

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used, clippy::panic)]
    use super::*;
    use evorule_reactor::{IoHandler, IoType};
    use evorule_tcb::JsonValue;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    static DYN_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// 验证三个 handler 都满足 `IoHandler` trait 的 object-safety，
    /// 能作为 `Arc<dyn IoHandler>` 注册到 dispatcher 风格的 map 并通过 trait object 调用。
    /// 这是 H5 架构（IoDispatcher 持有 `HashMap<IoType, Arc<dyn IoHandler>>`）的前提。
    #[tokio::test]
    async fn test_handlers_register_as_dyn_io_handler() {
        let n = DYN_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("evorule_io_dyn_{}_{}", std::process::id(), n));

        let mut handlers: HashMap<IoType, Arc<dyn IoHandler>> = HashMap::new();
        handlers.insert(
            IoType::save_memory(),
            Arc::new(MemoryHandler::new(dir.clone())),
        );

        // HttpHandler / DbHandler 也能转为 trait object（验证 object-safety）
        let _http_dyn: Arc<dyn IoHandler> = Arc::new(HttpHandler::new());
        let db = DbHandler::connect("sqlite::memory:").await.unwrap();
        let _db_dyn: Arc<dyn IoHandler> = Arc::new(db);

        // 通过 trait object 调用 MemoryHandler::execute
        let h = handlers.get(&IoType::save_memory()).unwrap();
        let r = h
            .execute(&JsonValue::object_from_pairs(&[
                ("key", JsonValue::string("dyn-key")),
                ("value", JsonValue::string("dyn-val")),
            ]))
            .await
            .unwrap();
        assert_eq!(r.as_bool(), Some(true));

        // 读回验证
        let r = h
            .execute(&JsonValue::object_from_pairs(&[(
                "key",
                JsonValue::string("dyn-key"),
            )]))
            .await
            .unwrap();
        assert_eq!(r.as_str(), Some("dyn-val"));
    }

    /// 验证 IoType 常量与 handler 的语义对应（文档化约定）。
    #[test]
    fn test_iotype_constants_available() {
        // 这五个常量是 main.rs IoDispatcher 注册时用的，确保它们存在且可比较
        assert_eq!(IoType::call_external().as_str(), "call_external");
        assert_eq!(IoType::http_get().as_str(), "http_get");
        assert_eq!(IoType::query_db().as_str(), "query_db");
        assert_eq!(IoType::save_memory().as_str(), "save_memory");
        assert_eq!(IoType::call_service().as_str(), "call_service");
    }
}

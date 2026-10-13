// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! I/O Handler trait —— object-safe,支持 trait object 动态分发
//!
//! # H5 迁移背景
//!
//! 此 trait 原位于 `evorule-governance/src/io_handler.rs`,使用 `impl Future`(RPITIT)
//! 签名,非 object-safe,无法 `dyn IoHandler`。
//!
//! H5 架构债务修复中,trait 下沉到 evorule-reactor(与 `IoType` 同层),并改用
//! `#[async_trait]` 使其 object-safe。这样:
//! - 应用层 crate(`evorule-io-handlers`)可依赖 evorule-reactor(而非 evorule-governance),
//!   避免循环依赖
//! - `IoDispatcher` 可持有 `HashMap<IoType, Arc<dyn IoHandler>>`,动态注册 handler
//!
//! # 使用示例
//!
//! ```
//! use async_trait::async_trait;
//! use evorule_reactor::{IoHandler, IoResult};
//! use evorule_tcb::JsonValue;
//!
//! struct EchoHandler;
//!
//! #[async_trait]
//! impl IoHandler for EchoHandler {
//!     async fn execute(&self, params: &JsonValue) -> IoResult {
//!         Ok(params.clone())
//!     }
//! }
//! ```

use async_trait::async_trait;
use evorule_tcb::JsonValue;

/// I/O 执行结果
///
/// 成功时返回 `JsonValue`(将注入到 `payload.__io_results__.{io_type}`),
/// 失败时返回错误消息(将记录到 `IoResponse.error` 字段)。
pub type IoResult = Result<JsonValue, String>;

/// I/O Handler trait —— 所有 I/O handler 实现此接口
///
/// 使用 `#[async_trait]` 使 trait object-safe,支持 `Arc<dyn IoHandler>`。
///
/// `params` 来自 `Fact::IoRequest.params`,已由 TCB 从路径引用解析为具体值。
///
/// # 实现者职责
/// - 从 `params` 提取所需字段(如 `url`、`query`、`key`)
/// - 执行实际 I/O 操作(HTTP 请求、SQL 查询、文件读写等)
/// - 返回 `Ok(JsonValue)` 或 `Err(String)`
///
/// # 调用方
/// - `evorule-governance::IoDispatcher` 根据 `IoType` 分发到对应 handler
/// - `evorule-governance::IoSubscriber` 订阅 `Fact::IoRequest` 后调用 dispatch
#[async_trait]
pub trait IoHandler: Send + Sync {
    /// 执行 I/O 操作
    ///
    /// # 参数
    /// - `params`:I/O 请求参数(由 `io_request` 指令透传,如
    ///   `call_external` 为 `{"messages": [...], "tools": [...]}`,
    ///   `call_service` 为 `{"service_name": "...", "args": {...}}`)
    ///
    /// # 返回
    /// - `Ok(JsonValue)`:I/O 结果(将注入到 `payload.__io_results__.{io_type}`)
    /// - `Err(String)`:错误消息(将记录到 `IoResponse.error`,结果不会被消费)
    async fn execute(&self, params: &JsonValue) -> IoResult;
}

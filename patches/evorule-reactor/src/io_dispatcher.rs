// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! I/O Dispatcher —— trait object 动态分发（机制层，v0.2.0 从 governance 下沉至 reactor）
//!
//! # v0.2.0 下沉背景
//!
//! v0.1.x 中 `IoDispatcher` 位于 evorule-governance，但 agent（解决方案1）不依赖
//! governance，导致 agent 无法复用 IoDispatcher，只能用 `call_service` + 二级路由
//! 借道。v0.2.0 自定义 IoType 能力要求 agent 能直接按 IoType 注册 handler，故将
//! IoDispatcher 下沉到 evorule-reactor（机制层基座），governance 与 agent 均可复用。
//!
//! # 设计
//!
//! `HashMap<IoType, Arc<dyn IoHandler>>` trait object 模式：
//! - 具体 handler 实现由应用层注入
//! - 通过 `builder()` + `register()` 动态注册
//! - 核心层不感知具体实现类型（解耦 sqlx/reqwest 等）
//!
//! # 性能
//!
//! trait object 动态分发对 I/O 操作无感知：dispatch 调用后立即 await，
//! vtable 查找的纳秒级开销相比 I/O 毫秒级延迟可忽略。
//!
//! `IoType` 作 `HashMap` key：v0.2.0 起 `IoType(pub Arc<str>)` 实现 `Hash + Eq`，
//! 哈希基于 `str` 内容，相同字符串的 `IoType`（无论 `new` 还是工厂函数构造）哈希一致。

use std::collections::HashMap;
use std::sync::Arc;

use crate::{IoHandler, IoResult, IoType};
use evorule_tcb::JsonValue;

/// I/O 分发器（机制层）
///
/// 持有 `HashMap<IoType, Arc<dyn IoHandler>>`，根据 IoType 分发到对应 handler。
/// 具体 handler 由应用层注册，核心层不感知具体实现类型。
///
/// `Clone`：内部 handler 都是 `Arc<dyn IoHandler>`，clone 仅增加引用计数，
/// 不复制 handler 本身。多会话模式下每个 session 可共享同一组 handler。
#[derive(Clone)]
pub struct IoDispatcher {
    /// IoType → handler 映射
    handlers: HashMap<IoType, Arc<dyn IoHandler>>,
}

impl IoDispatcher {
    /// 创建空分发器（无 handler 注册）
    ///
    /// 通常使用 `IoDispatcher::builder()` 链式注册 handler。
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
        }
    }

    /// 注册 handler 到指定 IoType
    ///
    /// 若该 IoType 已有 handler，将覆盖旧 handler。
    pub fn register(&mut self, io_type: IoType, handler: Arc<dyn IoHandler>) {
        self.handlers.insert(io_type, handler);
    }

    /// 获取 builder，链式注册 handler
    ///
    /// # 示例
    /// ```ignore
    /// let dispatcher = IoDispatcher::builder()
    ///     .register(IoType::call_external(), Arc::new(llm_handler))
    ///     .register(IoType::call_service(), Arc::new(service_handler))
    ///     .build();
    /// ```
    pub fn builder() -> IoDispatcherBuilder {
        IoDispatcherBuilder::new()
    }

    /// 根据 IoType 分发执行
    ///
    /// 若该 IoType 未注册 handler，返回错误（让反应器感知，而非静默失败）。
    pub async fn dispatch(&self, io_type: &IoType, params: &JsonValue) -> IoResult {
        match self.handlers.get(io_type) {
            Some(handler) => handler.execute(params).await,
            None => {
                tracing::warn!(
                    "Unknown IoType: {}, no handler registered",
                    io_type.as_str()
                );
                Err(format!(
                    "no handler registered for IoType: {}",
                    io_type.as_str()
                ))
            }
        }
    }

    /// 是否存在该 IoType 的 handler
    ///
    /// 供加载期校验（能力7 `validate_core_eval_io_types`）使用。
    pub fn contains(&self, io_type: &IoType) -> bool {
        self.handlers.contains_key(io_type)
    }

    /// 已注册的所有 IoType
    ///
    /// 供 `ReactorBuilder::known_io_types` 快速失败校验（能力4）：注册了 dispatcher 的
    /// reactor 可拒绝不在集合内的 io_type（拼错立即 `Fact::Error`，而非透传到 subscriber）。
    pub fn known_types(&self) -> impl Iterator<Item = &IoType> {
        self.handlers.keys()
    }
}

impl Default for IoDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

/// I/O 分发器 builder（链式注册）
///
/// 通过 `IoDispatcher::builder()` 创建，链式调用 `register()` 后 `build()` 完成。
pub struct IoDispatcherBuilder {
    handlers: HashMap<IoType, Arc<dyn IoHandler>>,
}

impl IoDispatcherBuilder {
    /// 创建空 builder
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
        }
    }

    /// 注册 handler（链式调用）
    pub fn register(mut self, io_type: IoType, handler: Arc<dyn IoHandler>) -> Self {
        self.handlers.insert(io_type, handler);
        self
    }

    /// 构建 IoDispatcher
    pub fn build(self) -> IoDispatcher {
        IoDispatcher {
            handlers: self.handlers,
        }
    }
}

impl Default for IoDispatcherBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoHandler;
    #[async_trait::async_trait]
    impl IoHandler for EchoHandler {
        async fn execute(&self, params: &JsonValue) -> IoResult {
            Ok(params.clone())
        }
    }

    #[test]
    fn dispatcher_routes_by_io_type() {
        let mut d = IoDispatcher::new();
        d.register(IoType::new("retrieve"), Arc::new(EchoHandler));
        assert!(d.contains(&IoType::new("retrieve")));
        assert!(!d.contains(&IoType::new("file")));
        assert_eq!(d.known_types().count(), 1);
    }

    #[tokio::test]
    async fn dispatch_hit_and_miss() {
        let d = IoDispatcher::builder()
            .register(IoType::new("retrieve"), Arc::new(EchoHandler))
            .build();
        let params = JsonValue::Null;
        assert!(d.dispatch(&IoType::new("retrieve"), &params).await.is_ok());
        assert!(d.dispatch(&IoType::new("file"), &params).await.is_err());
    }

    #[test]
    fn new_equals_factory_key_collision() {
        // IoType::new("call_service") 与 IoType::call_service() 哈希一致，同一 key
        let mut d = IoDispatcher::new();
        d.register(IoType::call_service(), Arc::new(EchoHandler));
        assert!(d.contains(&IoType::new("call_service")));
    }
}

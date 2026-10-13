// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! EvoRule 反应式执行器
//!
//! # 设计原则
//! - 事实驱动：所有交互通过 Fact 通道进行
//! - 单一串行通道：保证调度确定性
//! - 稳定检测：队列空 + 无待处理 I/O = 稳定
//! - 无状态泄漏：所有状态由反应器维护
//! - Append-Only 审计链：所有 Fact 追加到 FactsLog，支持审计重放
//!
//! # 架构
//! ```text
//! 用户/治理层 → FactSender → [command mpsc] → 反应器
//!                                                ↓
//!                                          调用 TCB 核心
//!                                                ↓
//!                                  产生新 Fact → event broadcast → 用户/I/O 订阅者/审计器
//!                                                ↓
//!                                  所有 Fact → FactsLog（审计链）
//! ```
//!
//! # 模块结构
//! - `fact` — Fact 枚举、FactId、IoType
//! - `io_handler` — I/O Handler trait（H5: 从 evorule-governance 下沉,object-safe）
//! - `channel` — 双通道封装（command + event）
//! - `reactor` — 反应器核心引擎
//! - `state` — 反应器内部状态
//! - `facts_log` — Append-Only 审计链（含哈希链）
//! - `stable_detector` — 稳定检测逻辑
//! - `invariants` — 结构不变量检查
//! - `hash` — BLAKE3 哈希链算法（单一真相源）
//! - `wal` — WAL 读写（JSONL 格式，含哈希字段）
//! - `pure` — 纯逻辑模块，Kani 形式化验证准备
//! - `ffi` — C FFI 接口（feature = "ffi"）
//! - `error` — 错误类型定义
//! - `phase` — 反应器阶段定义
//!
//! # 使用示例
//! ```ignore
//! use evorule_reactor::{Reactor, Fact, FactId};
//! use evorule_tcb::JsonValue;
//!
//! let core_eval = vec![];  // 从 core_eval.json 加载
//! let reactor = Reactor::builder(core_eval).max_rounds(1000).build();
//! let (tx, mut rx, _event_tx, _handle, _facts_log) = reactor.spawn();
//!
//! // 提交命令
//! tx.send(Fact::Command {
//!     id: FactId(1),
//!     instruction: JsonValue::object_from_pairs(&[
//!         ("type", JsonValue::string("increment")),
//!         ("params", JsonValue::object_from_pairs(&[
//!             ("attr", JsonValue::string("x")),
//!             ("delta", JsonValue::Integer(5)),
//!         ])),
//!     ]),
//! }).unwrap();
//!
//! // 等待 Stable 事实（broadcast recv 返回 Result）
//! while let Ok(fact) = rx.recv().await {
//!     if let Fact::Stable { version, .. } = fact {
//!         println!("会话于版本 {} 稳定", version);
//!         break;
//!     }
//! }
//! ```

#![deny(unsafe_code)]
#![deny(missing_docs)]
// 永不 panic 保障：禁止以下 panic-prone 模式（与 evorule-tcb 基线对齐）
// 测试/基准模块内可局部 #[allow(...)] 豁免
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::panic)]

mod channel;
mod error;
mod fact;
mod facts_log;
// ffi feature 必须暴露 extern "C" 接口，局部 allow unsafe_code 覆盖 deny
// 默认 feature (无 ffi) 下 ffi 模块不会被编译
#[cfg_attr(feature = "ffi", allow(unsafe_code))]
#[cfg(feature = "ffi")]
mod ffi;
mod hash;
mod invariants;
// H5: IoHandler trait 下沉到 evorule-reactor(与 IoType 同层,object-safe)
mod io_context;
mod io_dispatcher;
mod io_handler;
mod phase;
#[allow(dead_code)]
mod pure;
mod reactor;
mod stable_detector;
mod state;
#[cfg(feature = "persistence")]
mod wal;

pub use channel::{ChannelPair, EventReceiver, EventSender, FactReceiver, FactSender};
pub use error::ReactorError;
pub use fact::{ControlFlowType, Fact, FactId, FactIdGenerator, IoType, TraceHit};
// H5: IoHandler/IoResult 从 evorule-governance 下沉,供应用层 crate 使用
pub use facts_log::{FactsLog, FactsLogError};
pub use hash::{
    chain_step, compute_chain_hash, content_hash, fact_hash, fact_to_stable_json, HashError,
};
pub use invariants::InvariantViolation;
pub use io_context::{CallerRole, CallerRoleResolver, IoCallContext};
pub use io_dispatcher::{IoDispatcher, IoDispatcherBuilder};
pub use io_handler::{IoHandler, IoResult};
pub use phase::{PhaseContext, ReactorPhase};
pub use reactor::{
    InvariantScanReport, PendingIoEntry, Reactor, ReactorBuilder, ReactorHandle,
    ReactorStateSnapshot,
};
pub use stable_detector::StableDetector;
#[cfg(feature = "persistence")]
pub use wal::{
    fact_from_json, fact_to_json, read_wal, read_wal_with_hash, serde_to_tcb, tcb_to_serde,
    FactWalStore, MemoryWalStore, WalError, WalRecord, WalWriter,
};

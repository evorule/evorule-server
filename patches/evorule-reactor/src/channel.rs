// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 双通道封装（command mpsc + event broadcast）
//!
//! # 设计依据
//! 基于《02_反应式数据执行器》§3.3 和《04_树形结构》channel.rs 定义：
//! - **command 通道**：`mpsc::unbounded_channel`，单接收者（反应器），保证 FIFO
//! - **event 通道**：`broadcast::channel`，多接收者（用户/治理层 I/O 订阅者/审计器）
//!
//! # 双通道架构
//! - **command 通道**：用户/治理层 → 反应器（提交 Command/PayloadUpdate/IoResponse）
//! - **event 通道**：反应器 → 用户/治理层（产生 StateTransition/IoRequest/Stable/Error）
//!   - broadcast 支持 evorule-governance 的 I/O 订阅者和审计器同时订阅

use crate::fact::Fact;
use tokio::sync::{broadcast, mpsc};

/// command 通道发送器（可克隆，用于多个组件共享）
pub type FactSender = mpsc::UnboundedSender<Fact>;

/// command 通道接收器（唯一，由反应器持有）
pub type FactReceiver = mpsc::UnboundedReceiver<Fact>;

/// event 通道发送器（可克隆，反应器持有；通过 `subscribe()` 创建新接收者）
pub type EventSender = broadcast::Sender<Fact>;

/// event 通道接收器（可克隆，用户/治理层 I/O 订阅者/审计器各自持有）
pub type EventReceiver = broadcast::Receiver<Fact>;

/// broadcast 通道容量
///
/// 反应器产生的 Fact 不会非常频繁，1024 足以容纳一轮完整的
/// Command → StateTransition → IoRequest → IoResponse → Stable 序列。
/// 若接收者落后超过此容量，会收到 `RecvError::Lagged`。
const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// 双通道对（command mpsc + event broadcast）
///
/// 封装双通道的创建逻辑，确保类型一致性。
pub struct ChannelPair {
    /// command 通道发送端（用户持有）
    pub command_tx: FactSender,
    /// command 通道接收端（反应器持有）
    pub command_rx: FactReceiver,
    /// event 通道发送端（反应器持有）
    pub event_tx: EventSender,
    /// event 通道接收端（第一个订阅者，用户持有）
    pub event_rx: EventReceiver,
}

impl ChannelPair {
    /// 创建新的双通道对
    pub fn new() -> Self {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            command_tx,
            command_rx,
            event_tx,
            event_rx,
        }
    }
}

impl Default for ChannelPair {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::fact::FactId;
    use evorule_tcb::JsonValue;

    #[tokio::test]
    async fn test_command_channel_fifo_order() {
        let mut pair = ChannelPair::new();

        // 发送三条事实到 command 通道
        pair.command_tx
            .send(Fact::Command {
                id: FactId(1),
                instruction: JsonValue::empty_object(),
            })
            .unwrap();
        pair.command_tx
            .send(Fact::Command {
                id: FactId(2),
                instruction: JsonValue::empty_object(),
            })
            .unwrap();
        pair.command_tx
            .send(Fact::Command {
                id: FactId(3),
                instruction: JsonValue::empty_object(),
            })
            .unwrap();

        // 验证 FIFO 顺序
        let f1 = pair.command_rx.recv().await.unwrap();
        let f2 = pair.command_rx.recv().await.unwrap();
        let f3 = pair.command_rx.recv().await.unwrap();

        assert_eq!(f1.id(), FactId(1));
        assert_eq!(f2.id(), FactId(2));
        assert_eq!(f3.id(), FactId(3));
    }

    #[test]
    fn test_command_sender_clone() {
        let pair = ChannelPair::new();
        let tx_clone = pair.command_tx.clone();

        // 克隆的发送端应能发送
        tx_clone
            .send(Fact::Stable {
                id: FactId(1),
                version: 0,
            })
            .unwrap();

        // 原始发送端也能发送
        pair.command_tx
            .send(Fact::Stable {
                id: FactId(2),
                version: 0,
            })
            .unwrap();
    }

    #[tokio::test]
    async fn test_event_broadcast_multiple_subscribers() {
        let mut pair = ChannelPair::new();

        // 创建第二个订阅者
        let mut rx2 = pair.event_tx.subscribe();

        // 发送到 event 通道
        pair.event_tx
            .send(Fact::Stable {
                id: FactId(1),
                version: 0,
            })
            .unwrap();

        // 两个接收者都应收到
        let f1 = pair.event_rx.recv().await.unwrap();
        let f2 = rx2.recv().await.unwrap();
        assert_eq!(f1.id(), FactId(1));
        assert_eq!(f2.id(), FactId(1));
    }
}

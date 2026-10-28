// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 单线程本地队列（通道单线程化落地物，嵌入式同步执行器前置材料）
//!
//! # 定位（诚实声明）
//!
//! [`crate::channel`] 的 tokio `mpsc/broadcast` 依赖 `Send + Sync` 约束与
//! 多线程执行器；单线程宿主（嵌入式同步执行器、浏览器 worker）不需要、
//! 也负担不起这些约束。本模块提供同一**方法面**的单线程实现：
//!
//! - **无 tokio、无锁**：`Rc<RefCell<VecDeque>>` 纯单线程数据结构
//! - **类型级单线程执法**：`Rc` 使全部类型 `!Send + !Sync`——跨线程使用
//!   在编译期不可见，而非运行时约定
//! - **FIFO 逐字一致**：command 语义（unbounded、单接收者）与 event 语义
//!   （多订阅者、新订阅者不收历史）与 tokio 版对齐
//!
//! # 边界（与 tokio 版的诚实差异）
//!
//! - **同步面**：本模块只提供 `send/try_recv`；异步阻塞 `recv().await`
//!   属同步执行器（后续批次）的驱动职责，本模块不预造异步层
//! - **无容量上限 / 无 Lagged**：event 订阅队列无界；tokio broadcast 的
//!   容量-Lagged 语义不复制（嵌入式宿主内存受控，溢出应显式失败而非静默丢 Fact）
//! - **本批不接线生产路径**：native 生产通道仍为 tokio 版（字节等价铁律），
//!   切换发生在同步执行器落地批
//!
//! # 用法
//!
//! ```ignore
//! use evorule_reactor::channel_local::{local_pair, LocalBroadcaster};
//!
//! let (tx, rx) = local_pair::<Fact>();        // command 面（mpsc 语义）
//! let (evt_tx, evt_rx) = local_broadcaster::<Fact>(); // event 面（broadcast 语义）
//! tx.push(fact)?;
//! let fact = rx.try_pop().unwrap();
//! ```

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::{Rc, Weak};

/// command 面错误（对齐 `mpsc::SendError` 的最小语义面）
#[derive(Debug, PartialEq, Eq)]
pub struct LocalSendError;

impl std::fmt::Display for LocalSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "local queue receiver dropped")
    }
}

impl std::error::Error for LocalSendError {}

/// 共享队列核（发送端与接收端各持一个强/弱引用对）
struct SharedQueue<T> {
    deque: RefCell<VecDeque<T>>,
}

/// 单线程 command 发送端（unbounded mpsc 语义，可克隆共享同一队列）
///
/// 持弱引用：接收端 drop 后 `send` 即失败（对齐 `mpsc::SendError` 语义）。
pub struct LocalSender<T> {
    inner: Weak<SharedQueue<T>>,
}

impl<T> Clone for LocalSender<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> LocalSender<T> {
    /// 入队（unbounded：永不因容量失败，仅接收端 drop 后失败）
    pub fn send(&self, value: T) -> Result<(), LocalSendError> {
        self.inner
            .upgrade()
            .ok_or(LocalSendError)?
            .deque
            .borrow_mut()
            .push_back(value);
        Ok(())
    }

    /// 当前积压长度（接收端 drop 后为 `None`）
    pub fn len(&self) -> Option<usize> {
        Some(self.inner.upgrade()?.deque.borrow().len())
    }

    /// 队列是否为空（接收端 drop 后为 `None`）
    pub fn is_empty(&self) -> Option<bool> {
        Some(self.inner.upgrade()?.deque.borrow().is_empty())
    }
}

impl<T> std::fmt::Debug for LocalSender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalSender").finish_non_exhaustive()
    }
}

/// 单线程 command 接收端（单消费者 FIFO）
///
/// 持强引用：队列由接收端所有——这是 `send` 失败判据的锚点。
pub struct LocalReceiver<T> {
    inner: Rc<SharedQueue<T>>,
}

impl<T> LocalReceiver<T> {
    /// 出队：队空返回 `None`
    pub fn try_pop(&self) -> Option<T> {
        self.inner.deque.borrow_mut().pop_front()
    }

    /// 当前积压长度
    pub fn len(&self) -> usize {
        self.inner.deque.borrow().len()
    }

    /// 队列是否为空
    pub fn is_empty(&self) -> bool {
        self.inner.deque.borrow().is_empty()
    }
}

impl<T> std::fmt::Debug for LocalReceiver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalReceiver").finish_non_exhaustive()
    }
}

/// 创建单线程 command 通道对（unbounded mpsc 语义）
pub fn local_pair<T>() -> (LocalSender<T>, LocalReceiver<T>) {
    let inner = Rc::new(SharedQueue {
        deque: RefCell::new(VecDeque::new()),
    });
    (
        LocalSender {
            inner: Rc::downgrade(&inner),
        },
        LocalReceiver { inner },
    )
}

/// event 广播器核（订阅者列表；发送端持强引用，订阅者各持自己的队列）
struct BroadcasterCore<T> {
    subscribers: RefCell<Vec<Weak<RefCell<VecDeque<T>>>>>,
}

/// 单线程 event 发送端（broadcast 语义：多订阅者、各自独立队列、新订阅者不收历史）
pub struct LocalEventSender<T> {
    core: Rc<BroadcasterCore<T>>,
}

impl<T> Clone for LocalEventSender<T> {
    fn clone(&self) -> Self {
        Self {
            core: self.core.clone(),
        }
    }
}

impl<T: Clone> LocalEventSender<T> {
    /// 广播：向全部存活订阅者各投递一份克隆
    ///
    /// 已弃置的订阅者（接收端已 drop）在此回收，与 tokio broadcast 的
    /// 死订阅者清理语义对齐。
    pub fn send(&self, value: &T) {
        let mut subs = self.core.subscribers.borrow_mut();
        let mut dead = Vec::new();
        for (idx, weak) in subs.iter().enumerate() {
            match weak.upgrade() {
                Some(q) => q.borrow_mut().push_back(value.clone()),
                None => dead.push(idx),
            }
        }
        for idx in dead.into_iter().rev() {
            subs.swap_remove(idx);
        }
    }

    /// 当前存活订阅者数
    pub fn subscriber_count(&self) -> usize {
        self.core.subscribers.borrow().len()
    }
}

impl<T> std::fmt::Debug for LocalEventSender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalEventSender").finish_non_exhaustive()
    }
}

/// 单线程 event 接收端（每订阅者独立队列）
pub struct LocalEventReceiver<T> {
    queue: Rc<RefCell<VecDeque<T>>>,
}

impl<T> LocalEventReceiver<T> {
    /// 出队：队空返回 `None`
    pub fn try_pop(&self) -> Option<T> {
        self.queue.borrow_mut().pop_front()
    }

    /// 当前积压长度
    pub fn len(&self) -> usize {
        self.queue.borrow().len()
    }

    /// 队列是否为空
    pub fn is_empty(&self) -> bool {
        self.queue.borrow().is_empty()
    }
}

impl<T> Clone for LocalEventReceiver<T> {
    /// 克隆共享同一订阅队列（同一订阅者的多消费者形态）；
    /// 新订阅者应经 [`LocalEventSender::subscribe`] 创建。
    fn clone(&self) -> Self {
        Self {
            queue: self.queue.clone(),
        }
    }
}

impl<T> std::fmt::Debug for LocalEventReceiver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalEventReceiver").finish_non_exhaustive()
    }
}

/// 创建单线程 event 广播通道（broadcast 语义：新订阅者不收历史）
///
/// 返回（发送端，首个订阅者接收端）——与 [`crate::channel::ChannelPair`]
/// 的 event 面形态对齐；后续订阅者经 [`LocalEventSender::subscribe`] 创建。
pub fn local_broadcaster<T>() -> (LocalEventSender<T>, LocalEventReceiver<T>) {
    let core = Rc::new(BroadcasterCore {
        subscribers: RefCell::new(Vec::new()),
    });
    let first = Rc::new(RefCell::new(VecDeque::new()));
    core.subscribers.borrow_mut().push(Rc::downgrade(&first));
    (
        LocalEventSender { core },
        LocalEventReceiver { queue: first },
    )
}

impl<T> LocalEventSender<T> {
    /// 创建新订阅者（不收历史——与 tokio broadcast 语义对齐）
    pub fn subscribe(&self) -> LocalEventReceiver<T> {
        let queue = Rc::new(RefCell::new(VecDeque::new()));
        self.core
            .subscribers
            .borrow_mut()
            .push(Rc::downgrade(&queue));
        LocalEventReceiver { queue }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::panic)]
    use super::*;

    /// command 面 FIFO：先入先出、unbounded、克隆发送端共享队列
    #[test]
    fn local_pair_fifo_and_shared_sender() {
        let (tx, rx) = local_pair::<u64>();
        tx.send(1).unwrap();
        tx.send(2).unwrap();
        let tx2 = tx.clone();
        tx2.send(3).unwrap();
        assert_eq!(rx.len(), 3);
        assert_eq!(rx.try_pop(), Some(1));
        assert_eq!(rx.try_pop(), Some(2));
        assert_eq!(rx.try_pop(), Some(3));
        assert!(rx.try_pop().is_none());
        assert_eq!(rx.len(), 0);
        assert_eq!(tx.is_empty(), Some(true));
    }

    /// 接收端 drop 后发送端 send 失败（LocalSendError，对齐 mpsc 语义）
    #[test]
    fn local_pair_receiver_drop_yields_send_error() {
        let (tx, rx) = local_pair::<u64>();
        drop(rx);
        assert_eq!(tx.send(1), Err(LocalSendError));
        assert_eq!(tx.len(), None);
    }

    /// 广播面：多订阅者各得一份、新订阅者不收历史、死订阅者被回收
    #[test]
    fn broadcaster_fanout_no_history_and_dead_cleanup() {
        let (tx, rx1) = local_broadcaster::<u64>();
        tx.send(&10);
        let rx2 = tx.subscribe(); // 不收历史
        assert!(rx2.try_pop().is_none());
        tx.send(&20);
        assert_eq!(rx1.try_pop(), Some(10));
        assert_eq!(rx1.try_pop(), Some(20));
        assert_eq!(rx2.try_pop(), Some(20));
        assert_eq!(tx.subscriber_count(), 2);
        drop(rx1);
        tx.send(&30); // 触发死订阅者回收
        assert_eq!(tx.subscriber_count(), 1);
        assert_eq!(rx2.try_pop(), Some(30));
    }

    /// 类型级单线程执法：Rc 系类型必须 !Send（编译期保证的运行时断言镜像）
    #[test]
    fn local_types_are_not_send() {
        fn assert_not_send<T: 'static>() {}
        assert_not_send::<LocalSender<u8>>();
        assert_not_send::<LocalReceiver<u8>>();
        assert_not_send::<LocalEventSender<u8>>();
        assert_not_send::<LocalEventReceiver<u8>>();
    }

    /// 发送端视角 len 语义：接收端存活 → Some；接收端 drop → None
    #[test]
    fn receiver_len_tracks_sender_liveness() {
        let (tx, rx) = local_pair::<u64>();
        tx.send(1).unwrap();
        assert_eq!(rx.len(), 1);
        assert_eq!(tx.len(), Some(1));
        drop(rx);
        assert_eq!(tx.len(), None);
        assert_eq!(tx.send(2), Err(LocalSendError));
    }
}

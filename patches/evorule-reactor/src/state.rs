// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 反应器内部状态

use crate::fact::{FactId, IoType};
use crate::phase::ReactorPhase;
use evorule_tcb::JsonValue;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

// ============================================================================
// Kani 专用集合类型
//
// Kani 对 BTreeSet/BTreeMap 的红黑树内部结构建模能力有限（见
// evorule-tcb/verification/kani-formal-verification-design.md），即使集合只有 1 个元素，insert/remove 操作
// 也会展开大量红黑树节点操作路径，导致 CBMC 状态爆炸。
//
// 解决方案：在 #[cfg(kani)] 模式下用基于 Vec 的线性集合替代 BTreeSet/
// BTreeMap。Vec 的 push/swap_remove/contains 操作在 Kani 中建模高效
// （无指针追踪、无递归树遍历）。API 与 BTreeSet/BTreeMap 兼容，使
// ReactorState 的方法实现无需条件编译分支。
//
// 参考模式：evorule-tcb/verification/fixed_map.rs（FixedMap 替代 BTreeMap）。
// ============================================================================

#[cfg(kani)]
mod kani_collections {
    use crate::fact::FactId;

    /// Kani 专用：基于 Vec 的集合，替代 BTreeSet<FactId>
    ///
    /// API 兼容 BTreeSet<FactId> 的常用方法（insert/remove/len/is_empty/
    /// contains）。不保证迭代顺序（使用 swap_remove），但 Kani proof
    /// 不依赖迭代顺序。
    #[derive(Debug, Clone, Default)]
    pub(crate) struct KIdSet(Vec<FactId>);

    impl KIdSet {
        pub fn new() -> Self {
            Self(Vec::new())
        }
        pub fn insert(&mut self, id: FactId) -> bool {
            if self.0.iter().any(|x| x == &id) {
                return false;
            }
            self.0.push(id);
            true
        }
        pub fn remove(&mut self, id: &FactId) -> bool {
            if let Some(pos) = self.0.iter().position(|x| x == id) {
                self.0.swap_remove(pos);
                true
            } else {
                false
            }
        }
        pub fn len(&self) -> usize {
            self.0.len()
        }
        pub fn is_empty(&self) -> bool {
            self.0.is_empty()
        }
        pub fn contains(&self, id: &FactId) -> bool {
            self.0.iter().any(|x| x == id)
        }
    }

    /// Kani 专用：基于 Vec 的映射，替代 BTreeMap<FactId, V>
    ///
    /// API 兼容 BTreeMap<FactId, V> 的常用方法（insert/remove/len/
    /// is_empty/contains_key/get/get_mut/iter）。
    #[derive(Debug, Clone, Default)]
    pub(crate) struct KIdMap<V>(Vec<(FactId, V)>);

    impl<V> KIdMap<V> {
        pub fn new() -> Self {
            Self(Vec::new())
        }
        pub fn insert(&mut self, id: FactId, val: V) -> Option<V> {
            if let Some(pos) = self.0.iter().position(|(k, _)| k == &id) {
                Some(std::mem::replace(&mut self.0[pos].1, val))
            } else {
                self.0.push((id, val));
                None
            }
        }
        pub fn remove(&mut self, id: &FactId) -> Option<V> {
            if let Some(pos) = self.0.iter().position(|(k, _)| k == id) {
                Some(self.0.swap_remove(pos).1)
            } else {
                None
            }
        }
        pub fn len(&self) -> usize {
            self.0.len()
        }
        pub fn is_empty(&self) -> bool {
            self.0.is_empty()
        }
        pub fn contains_key(&self, id: &FactId) -> bool {
            self.0.iter().any(|(k, _)| k == id)
        }
        pub fn get(&self, id: &FactId) -> Option<&V> {
            self.0.iter().find(|(k, _)| k == id).map(|(_, v)| v)
        }
        pub fn iter(&self) -> std::slice::Iter<'_, (FactId, V)> {
            self.0.iter()
        }
    }

    /// KIdMap 的引用迭代器，兼容 `for (k, v) in &map` 语法
    /// （BTreeMap 的 IntoIterator for &BTreeMap 产出 (&K, &V)）
    pub(crate) struct KIdMapIter<'a, V> {
        inner: std::slice::Iter<'a, (FactId, V)>,
    }

    impl<'a, V> Iterator for KIdMapIter<'a, V> {
        type Item = (&'a FactId, &'a V);
        fn next(&mut self) -> Option<Self::Item> {
            self.inner.next().map(|(k, v)| (k, v))
        }
    }

    impl<'a, V> IntoIterator for &'a KIdMap<V> {
        type Item = (&'a FactId, &'a V);
        type IntoIter = KIdMapIter<'a, V>;
        fn into_iter(self) -> Self::IntoIter {
            KIdMapIter {
                inner: self.0.iter(),
            }
        }
    }

    /// KIdSet 的引用迭代器，兼容 `for x in &set` 语法
    pub(crate) struct KIdSetIter<'a> {
        inner: std::slice::Iter<'a, FactId>,
    }

    impl<'a> Iterator for KIdSetIter<'a> {
        type Item = &'a FactId;
        fn next(&mut self) -> Option<Self::Item> {
            self.inner.next()
        }
    }

    impl<'a> IntoIterator for &'a KIdSet {
        type Item = &'a FactId;
        type IntoIter = KIdSetIter<'a>;
        fn into_iter(self) -> Self::IntoIter {
            KIdSetIter {
                inner: self.0.iter(),
            }
        }
    }
}

/// 反应器内部状态
#[derive(Debug, Clone)]
#[allow(dead_code)] // 部分工具方法供 evorule-governance 或扩展使用
pub(crate) struct ReactorState {
    /// 当前业务状态
    pub payload: JsonValue,

    /// 当前指令队列
    pub queue: VecDeque<JsonValue>,

    /// 单调递增版本号（每次状态变更 +1）
    pub version: u64,

    /// 上一次的版本号（用于不变式 #3：version 单调递增自检）
    ///
    /// 由 `bump_version` 维护：`prev_version = version; version += 1;`
    pub prev_version: u64,

    /// 待响应的 I/O 请求数量
    pub pending_io_count: usize,

    /// 待处理的 I/O 请求集合（用于验证 IoResponse）
    ///
    /// 使用 BTreeSet 保证确定性迭代顺序（与 evorule-tcb 风格一致）。
    #[cfg(not(kani))]
    pub pending_requests: BTreeSet<FactId>,
    /// Kani 模式：用 KIdSet 替代 BTreeSet，避免红黑树建模导致 CBMC 状态爆炸
    #[cfg(kani)]
    pub pending_requests: kani_collections::KIdSet,

    /// 触发 I/O 的原指令缓存（IoRequest id → (原指令, cause)）
    ///
    /// IoResponse 到达后，反应器将原指令重新推送回队列前端，
    /// 使 core_eval.json 中的 `exists(__io_results__.{io_type})` 双路径生效：
    /// - 首次执行：走 on_false 分支，触发 io_request
    /// - 恢复执行：走 on_true 分支，set 消费 __io_results__.{io_type} 到业务字段
    ///
    /// 断点 1 修复：同时缓存 cause（触发 I/O 的原指令的 cause FactId），
    /// 使恢复执行时 StateTransition/IoRequest 的 cause 指向正确的 Fact。
    #[cfg(not(kani))]
    pub pending_io_instructions: BTreeMap<FactId, (JsonValue, FactId)>,
    /// Kani 模式：用 KIdMap 替代 BTreeMap
    #[cfg(kani)]
    pub pending_io_instructions: kani_collections::KIdMap<(JsonValue, FactId)>,

    /// 指令队列的平行 cause 追踪（与 `queue` 一一对应）
    ///
    /// 断点 1 修复：每个队列中的指令都关联一个 cause FactId，
    /// 用于 StateTransition/IoRequest 的 cause 字段。
    /// 解决 drain 多个 Command 时 current_cause 被覆盖的问题。
    ///
    /// 不变式：`instruction_causes.len() == queue.len()` 始终成立。
    pub instruction_causes: VecDeque<FactId>,

    /// I/O 请求发射时间戳（P3-11：用于超时检测）
    ///
    /// 每次 `register_io_request` 时记录当前时间，
    /// 超时检测时扫描此映射判断是否超过 warn/error 阈值。
    #[cfg(not(kani))]
    pub pending_io_timestamps: BTreeMap<FactId, Instant>,
    /// Kani 模式：用 KIdMap 替代 BTreeMap
    #[cfg(kani)]
    pub pending_io_timestamps: kani_collections::KIdMap<Instant>,

    /// I/O 请求类型记录（阶段3-1.4：用于按 io_type 查超时阈值）
    ///
    /// 每次 `register_io_request` 时记录 io_type，
    /// `check_io_timeouts` 时按 io_type 查 `IoTimeoutPolicy` 获取专属阈值。
    #[cfg(not(kani))]
    pub pending_io_types: BTreeMap<FactId, IoType>,
    /// Kani 模式：用 KIdMap 替代 BTreeMap
    #[cfg(kani)]
    pub pending_io_types: kani_collections::KIdMap<IoType>,

    /// I/O 恢复执行标志
    ///
    /// IoResponse 到达后设为 true，重新执行原指令后（execute_transition 返回 State）
    /// 反应器检查此标志，若为 true 则清除 `payload.__io_results__` 并重置为 false。
    /// 这是必要的：v0.3.1 中 `exists` 域对 null 返回 false（core_eval 用 null 清除结果），
    /// 若 io_recovery 未清除，后续不同的 I/O 指令会错误地认为仍在恢复中。
    pub io_recovery: bool,

    /// 控制层阶段（白盒化：让执行状态机可观察）
    ///
    /// 由主循环每次迭代更新，用于 tracing 与调试。
    /// 阶段转移函数是纯算法（见 [crate::phase::ReactorPhase::next]）。
    pub phase: ReactorPhase,

    /// 不变式违规累计计数（白盒化：结构性自检结果）
    ///
    /// 每次 phase 转移时调用 `check_invariants`，违规时累加。
    /// 仅记录，不强制中断反应器（用 `tracing::error!` 而非 `debug_assert!`，符合 F11）。
    pub structural_invariant_violations: u64,

    /// Kani 专用：__io_result__ 存在标志
    ///
    /// Kani 对 `JsonValue::Object` 内部的 `BTreeMap<String, JsonValue>` 操作
    /// （insert/remove/contains_key）建模能力有限，即使只操作 1 个 key 也会
    /// 导致 CBMC 状态爆炸。此标志在 Kani 模式下替代 BTreeMap 操作，
    /// 使 `inject_io_result`/`clear_io_result`/`has_io_result` 成为 O(1) 操作。
    #[cfg(kani)]
    pub kani_has_io_result: bool,
}

#[allow(dead_code)] // 部分队列操作方法供扩展使用
impl ReactorState {
    pub fn new() -> Self {
        Self {
            payload: JsonValue::empty_object(),
            queue: VecDeque::new(),
            instruction_causes: VecDeque::new(),
            version: 0,
            prev_version: 0,
            pending_io_count: 0,
            #[cfg(not(kani))]
            pending_requests: BTreeSet::new(),
            #[cfg(kani)]
            pending_requests: kani_collections::KIdSet::new(),
            #[cfg(not(kani))]
            pending_io_instructions: BTreeMap::new(),
            #[cfg(kani)]
            pending_io_instructions: kani_collections::KIdMap::new(),
            #[cfg(not(kani))]
            pending_io_timestamps: BTreeMap::new(),
            #[cfg(kani)]
            pending_io_timestamps: kani_collections::KIdMap::new(),
            #[cfg(not(kani))]
            pending_io_types: BTreeMap::new(),
            #[cfg(kani)]
            pending_io_types: kani_collections::KIdMap::new(),
            io_recovery: false,
            phase: ReactorPhase::default(),
            structural_invariant_violations: 0,
            #[cfg(kani)]
            kani_has_io_result: false,
        }
    }

    /// 安全递增版本号（同时更新 prev_version，支持不变式 #3 自检）
    ///
    /// 替代 `state.version += 1;`，确保 prev_version 同步更新。
    /// 在 PayloadUpdate / IoResponse / StateTransition 处理时调用。
    pub fn bump_version(&mut self) {
        self.prev_version = self.version;
        self.version += 1;
    }

    /// 稳定条件：队列空 + 无待处理 I/O
    pub fn is_stable(&self) -> bool {
        self.queue.is_empty() && self.pending_io_count == 0
    }

    /// 返回队列长度
    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

    /// 弹出队首指令及其关联的 cause
    ///
    /// 断点 1 修复：返回 (instruction, cause) 对，
    /// cause 用于后续 StateTransition/IoRequest Fact 的 cause 字段。
    pub fn pop_instruction(&mut self) -> Option<(JsonValue, FactId)> {
        let instr = self.queue.pop_front()?;
        let cause = self.instruction_causes.pop_front().unwrap_or(FactId(0));
        Some((instr, cause))
    }

    /// 推指令到队首，关联指定 cause
    pub fn push_front(&mut self, instruction: JsonValue, cause: FactId) {
        self.queue.push_front(instruction);
        self.instruction_causes.push_front(cause);
    }

    /// 推多条指令到队首，共享同一 cause
    ///
    /// 反序后 push_front 保持原序：[a,b,c] → push c,b,a → 队列 [a,b,c,...]
    pub fn push_front_all(&mut self, instructions: Vec<JsonValue>, cause: FactId) {
        for instr in instructions.into_iter().rev() {
            self.queue.push_front(instr);
            self.instruction_causes.push_front(cause);
        }
    }

    /// 推指令到队尾，关联指定 cause
    pub fn push_back(&mut self, instruction: JsonValue, cause: FactId) {
        self.queue.push_back(instruction);
        self.instruction_causes.push_back(cause);
    }

    /// 推多条指令到队尾，共享同一 cause
    pub fn push_back_all(&mut self, instructions: Vec<JsonValue>, cause: FactId) {
        for instr in instructions {
            self.queue.push_back(instr);
            self.instruction_causes.push_back(cause);
        }
    }

    /// 清空队列和 cause 队列（同步清空，保持 len 相等）
    ///
    /// 用于 MaxRoundsExceeded 和队列长度超限的恢复路径。
    ///
    /// # Kani 模式
    ///
    /// CBMC 将 VecDeque 堆缓冲区中的 `JsonValue` 按任意变体建模，
    /// `clear` 的 `drop_in_place` 会展开 `Object(BTreeMap)` 红黑树
    /// 析构循环，无界 unwind 导致状态爆炸（P0-11 超时根因）。
    /// 改用 `mem::take` + `forget`：长度清空语义与真实 clear 一致，
    /// 仅跳过元素析构（与 kani_collections 替代方案同理）。
    pub fn clear_queue(&mut self) {
        #[cfg(kani)]
        {
            let q = std::mem::take(&mut self.queue);
            std::mem::forget(q);
            let c = std::mem::take(&mut self.instruction_causes);
            std::mem::forget(c);
            return;
        }
        #[cfg(not(kani))]
        {
            self.queue.clear();
            self.instruction_causes.clear();
        }
    }

    /// 用 execute_transition 返回的新队列更新状态，同步重建 cause 队列
    ///
    /// 断点 1 修复：push 元指令将新指令前置到队列头部，
    /// 所以 new_queue = [新 push 的指令...] + [原有剩余指令]。
    /// - 前 new_count 条是新 push 的指令 → 继承 current_cause
    /// - 后 old_len 条是原有指令 → 保留原 cause
    ///
    /// 参数:
    /// - `new_queue`: execute_transition 返回的新队列
    /// - `current_cause`: 当前执行的指令的 cause（新 push 的指令继承此 cause）
    pub fn update_queue_with_causes(&mut self, new_queue: Vec<JsonValue>, current_cause: FactId) {
        let old_len = self.queue.len();
        let old_causes: Vec<FactId> = self.instruction_causes.drain(..).collect();
        let new_count = new_queue.len().saturating_sub(old_len);
        self.queue = VecDeque::with_capacity(new_queue.len());
        for (i, instr) in new_queue.into_iter().enumerate() {
            let c = if i < new_count {
                current_cause
            } else {
                old_causes
                    .get(i - new_count)
                    .copied()
                    .unwrap_or(current_cause)
            };
            self.queue.push_back(instr);
            self.instruction_causes.push_back(c);
        }
    }

    /// 注册一个待处理的 I/O 请求
    ///
    /// 幂等：重复注册同一 id 不会增加计数（防止 count 与 set 大小不一致）。
    /// P3-11：同时记录发射时间戳，用于超时检测。
    /// 阶段3-1.4：同时记录 io_type，用于按类型查超时阈值。
    pub fn register_io_request(&mut self, id: FactId, io_type: IoType) {
        if self.pending_requests.insert(id) {
            self.pending_io_count += 1;
            self.pending_io_timestamps.insert(id, Instant::now());
            self.pending_io_types.insert(id, io_type);
        }
    }

    /// 查询指定 I/O 请求的 io_type（IoResponse 处理时用于定位 `__io_results__.{io_type}`）
    ///
    /// 必须在 `complete_io_request` 之前调用，因为完成请求会移除 io_type 记录。
    pub fn get_io_type(&self, id: &FactId) -> Option<&IoType> {
        self.pending_io_types.get(id)
    }

    /// 完成一个 I/O 请求，返回是否成功（即该请求是否在等待中）
    ///
    /// P3-11：同时移除时间戳。
    /// 阶段3-1.4：同时移除 io_type 记录。
    pub fn complete_io_request(&mut self, id: FactId) -> bool {
        if self.pending_requests.remove(&id) {
            self.pending_io_count = self.pending_io_count.saturating_sub(1);
            self.pending_io_timestamps.remove(&id);
            self.pending_io_types.remove(&id);
            true
        } else {
            false
        }
    }

    /// 扫描 pending I/O 超时（P3-11）
    ///
    /// 返回 `(warn_ids, error_ids)`：
    /// - `warn_ids`：超过 `warn_timeout` 但未超过 `error_timeout` 的请求 ID
    /// - `error_ids`：超过 `error_timeout` 的请求 ID（调用方应发射 Error 并恢复）
    pub fn scan_io_timeouts(
        &self,
        warn_timeout: Duration,
        error_timeout: Duration,
    ) -> (Vec<FactId>, Vec<FactId>) {
        let now = Instant::now();
        let mut warn_ids = Vec::new();
        let mut error_ids = Vec::new();
        // 用 .iter() 兼容 BTreeMap 和 KIdMap（Kani 模式）
        // BTreeMap::iter() 产出 (&FactId, &Instant)
        // KIdMap::iter() 产出 &(FactId, Instant)，for 解构后等价
        for (id, timestamp) in self.pending_io_timestamps.iter() {
            let elapsed = now.duration_since(*timestamp);
            if elapsed >= error_timeout {
                error_ids.push(*id);
            } else if elapsed >= warn_timeout {
                warn_ids.push(*id);
            }
        }
        (warn_ids, error_ids)
    }

    /// 强制移除一个超时的 I/O 请求（P3-11：error 超时恢复用）
    ///
    /// 与 `complete_io_request` 不同，此方法不期望 IoResponse 到达，
    /// 而是反应器主动清理超时请求。同时移除缓存的指令、时间戳和 io_type 记录。
    ///
    /// 幂等性：若 id 不在 pending_requests 中，不做任何修改（保持计数一致）。
    /// 这确保 `pending_io_count == pending_requests.len()` 不变量在任意 id 输入下保持。
    pub fn force_remove_io_request(&mut self, id: FactId) {
        if self.pending_requests.remove(&id) {
            self.pending_io_count = self.pending_io_count.saturating_sub(1);
            self.pending_io_instructions.remove(&id);
            self.pending_io_timestamps.remove(&id);
            self.pending_io_types.remove(&id);
        }
    }

    /// 缓存触发 I/O 的原指令及其 cause（IoRequest 产生时调用）
    ///
    /// IoResponse 到达后，通过 `take_io_instruction` 取出并重新推送回队列前端，
    /// 使 core_eval.json 中的双路径机制生效。
    ///
    /// 断点 1 修复：同时缓存 cause，使恢复执行时 cause 指向正确的 Fact。
    pub fn save_io_instruction(&mut self, id: FactId, instruction: JsonValue, cause: FactId) {
        self.pending_io_instructions
            .insert(id, (instruction, cause));
    }

    /// 取出并移除缓存的原指令及其 cause（IoResponse 处理时调用）
    ///
    /// 返回 `Some((instruction, cause))` 表示该 id 有缓存，应重新推送回队列；
    /// 返回 `None` 表示该 id 无缓存（可能是未知 IoResponse 或重复处理）。
    pub fn take_io_instruction(&mut self, id: FactId) -> Option<(JsonValue, FactId)> {
        self.pending_io_instructions.remove(&id)
    }

    /// 清除 payload 中的 `__io_results__` 字段
    ///
    /// v0.3.1：I/O 结果按 io_type 隔离存储在 `__io_results__.{io_type}`，
    /// core_eval 消费后将对应项置为 null（`exists` 将 null 视为不存在）。
    /// I/O 恢复执行后调用，整体移除 `__io_results__` 容器，
    /// 防止残留的 null 项在后续 I/O 指令中被误读、也避免陈旧结果影响后续 I/O 指令。
    /// 因为同一时刻最多只有一个 pending I/O，整体移除是安全的。
    pub fn clear_io_result(&mut self) {
        #[cfg(kani)]
        {
            self.kani_has_io_result = false;
            return;
        }
        #[cfg(not(kani))]
        {
            if let JsonValue::Object(map) = &mut self.payload {
                map.remove("__io_results__");
            }
        }
    }

    /// 原子清除 I/O 恢复状态（同时清除 `__io_results__` 和 `io_recovery` 标志）
    ///
    /// 此方法在一次调用内完成两个操作，避免中间状态违反不变式 #2/#4。
    /// 注意: 不变式检查在 'main 循环开头调用，不会在此方法内部触发，
    /// 所以中间状态不会被误报。
    pub fn clear_io_recovery(&mut self) {
        self.clear_io_result();
        self.io_recovery = false;
    }
}

impl Default for ReactorState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn test_state_initial() {
        let state = ReactorState::new();
        assert!(state.is_stable());
        assert_eq!(state.version, 0);
        assert_eq!(state.pending_io_count, 0);
        assert!(state.pending_requests.is_empty());
    }

    #[test]
    fn test_io_request_tracking() {
        let mut state = ReactorState::new();
        let id1 = FactId(1);
        let id2 = FactId(2);

        state.register_io_request(id1, IoType::call_external());
        assert_eq!(state.pending_io_count, 1);
        assert!(state.pending_requests.contains(&id1));

        state.register_io_request(id2, IoType::call_external());
        assert_eq!(state.pending_io_count, 2);
        assert!(state.pending_requests.contains(&id2));

        assert!(state.complete_io_request(id1));
        assert_eq!(state.pending_io_count, 1);
        assert!(!state.pending_requests.contains(&id1));

        // 完成不存在的请求
        assert!(!state.complete_io_request(FactId(999)));
        assert_eq!(state.pending_io_count, 1);
    }

    #[test]
    fn test_push_front_all_preserves_order() {
        let mut state = ReactorState::new();
        let instrs = vec![
            JsonValue::string("a"),
            JsonValue::string("b"),
            JsonValue::string("c"),
        ];
        state.push_front_all(instrs, FactId(1));

        assert_eq!(
            state.pop_instruction(),
            Some((JsonValue::string("a"), FactId(1)))
        );
        assert_eq!(
            state.pop_instruction(),
            Some((JsonValue::string("b"), FactId(1)))
        );
        assert_eq!(
            state.pop_instruction(),
            Some((JsonValue::string("c"), FactId(1)))
        );
    }

    #[test]
    fn test_push_back_all_preserves_order() {
        let mut state = ReactorState::new();
        let instrs = vec![
            JsonValue::string("x"),
            JsonValue::string("y"),
            JsonValue::string("z"),
        ];
        state.push_back_all(instrs, FactId(2));

        assert_eq!(
            state.pop_instruction(),
            Some((JsonValue::string("x"), FactId(2)))
        );
        assert_eq!(
            state.pop_instruction(),
            Some((JsonValue::string("y"), FactId(2)))
        );
        assert_eq!(
            state.pop_instruction(),
            Some((JsonValue::string("z"), FactId(2)))
        );
    }

    #[test]
    fn test_push_front_single() {
        let mut state = ReactorState::new();
        state.push_back(JsonValue::string("a"), FactId(1));
        state.push_back(JsonValue::string("b"), FactId(2));

        // push_front 将指令插入队首
        state.push_front(JsonValue::string("urgent"), FactId(3));

        assert_eq!(
            state.pop_instruction(),
            Some((JsonValue::string("urgent"), FactId(3)))
        );
        assert_eq!(
            state.pop_instruction(),
            Some((JsonValue::string("a"), FactId(1)))
        );
        assert_eq!(
            state.pop_instruction(),
            Some((JsonValue::string("b"), FactId(2)))
        );
        assert_eq!(state.pop_instruction(), None);
    }

    #[test]
    fn test_push_front_all_to_empty_queue() {
        let mut state = ReactorState::new();
        state.push_front_all(vec![JsonValue::string("only")], FactId(1));
        assert_eq!(
            state.pop_instruction(),
            Some((JsonValue::string("only"), FactId(1)))
        );
        assert_eq!(state.pop_instruction(), None);
    }

    #[test]
    fn test_push_front_all_interleaved_with_existing() {
        let mut state = ReactorState::new();
        state.push_back(JsonValue::string("old"), FactId(1));

        // push_front_all 应将新指令插入到 old 之前
        state.push_front_all(
            vec![JsonValue::string("new1"), JsonValue::string("new2")],
            FactId(2),
        );

        assert_eq!(
            state.pop_instruction(),
            Some((JsonValue::string("new1"), FactId(2)))
        );
        assert_eq!(
            state.pop_instruction(),
            Some((JsonValue::string("new2"), FactId(2)))
        );
        assert_eq!(
            state.pop_instruction(),
            Some((JsonValue::string("old"), FactId(1)))
        );
    }

    #[test]
    fn test_register_duplicate_io_request() {
        // 重复注册同一 id 应幂等：count 与 set 大小保持一致
        let mut state = ReactorState::new();
        let id = FactId(1);

        state.register_io_request(id, IoType::call_external());
        assert_eq!(state.pending_io_count, 1);
        assert_eq!(state.pending_requests.len(), 1);

        // 重复注册同一 id：count 不变（幂等）
        state.register_io_request(id, IoType::call_external());
        assert_eq!(state.pending_io_count, 1);
        assert_eq!(state.pending_requests.len(), 1);

        // 完成后：count 减为 0，set 为空
        assert!(state.complete_io_request(id));
        assert_eq!(state.pending_io_count, 0);
        assert!(state.pending_requests.is_empty());

        // 再次完成：返回 false
        assert!(!state.complete_io_request(id));
        assert_eq!(state.pending_io_count, 0);
    }

    #[test]
    fn test_queue_len() {
        let mut state = ReactorState::new();
        assert_eq!(state.queue_len(), 0);

        state.push_back(JsonValue::string("a"), FactId(1));
        assert_eq!(state.queue_len(), 1);

        state.push_back(JsonValue::string("b"), FactId(2));
        assert_eq!(state.queue_len(), 2);

        state.pop_instruction();
        assert_eq!(state.queue_len(), 1);
    }

    #[test]
    fn test_pop_from_empty_queue() {
        let mut state = ReactorState::new();
        assert_eq!(state.pop_instruction(), None);
    }

    #[test]
    fn test_instruction_causes_synced_with_queue() {
        // 断点 1: 验证 instruction_causes 与 queue 始终同步
        let mut state = ReactorState::new();
        assert_eq!(state.instruction_causes.len(), state.queue.len());

        state.push_back(JsonValue::string("a"), FactId(10));
        state.push_back(JsonValue::string("b"), FactId(20));
        assert_eq!(state.instruction_causes.len(), state.queue.len());

        state.pop_instruction();
        assert_eq!(state.instruction_causes.len(), state.queue.len());

        state.clear_queue();
        assert_eq!(state.instruction_causes.len(), state.queue.len());
        assert_eq!(state.instruction_causes.len(), 0);
    }

    #[test]
    fn test_update_queue_with_causes_new_push() {
        // 断点 1: 验证 update_queue_with_causes 正确分配 cause
        // 模拟: pop 指令后队列有 [B, C]（cause=[cB, cC]），
        // execute_transition push 了 [X, Y]，返回 new_queue = [X, Y, B, C]
        let mut state = ReactorState::new();
        state.push_back(JsonValue::string("B"), FactId(2));
        state.push_back(JsonValue::string("C"), FactId(3));
        // 此时 queue = [B, C], causes = [2, 3]

        // 模拟 pop 后的 cause
        let current_cause = FactId(1); // 被 pop 的指令的 cause

        // new_queue = [X, Y, B, C]（push 前置了 X, Y）
        let new_queue = vec![
            JsonValue::string("X"),
            JsonValue::string("Y"),
            JsonValue::string("B"),
            JsonValue::string("C"),
        ];
        state.update_queue_with_causes(new_queue, current_cause);

        // 验证: X, Y 继承 current_cause(1)，B, C 保留原 cause(2, 3)
        assert_eq!(
            state.pop_instruction(),
            Some((JsonValue::string("X"), FactId(1)))
        );
        assert_eq!(
            state.pop_instruction(),
            Some((JsonValue::string("Y"), FactId(1)))
        );
        assert_eq!(
            state.pop_instruction(),
            Some((JsonValue::string("B"), FactId(2)))
        );
        assert_eq!(
            state.pop_instruction(),
            Some((JsonValue::string("C"), FactId(3)))
        );
    }

    #[test]
    fn test_complete_io_request_empty() {
        let mut state = ReactorState::new();
        // 空状态下完成任意 id 应返回 false
        assert!(!state.complete_io_request(FactId(999)));
        assert_eq!(state.pending_io_count, 0);
    }

    #[test]
    fn test_default_state() {
        let state = ReactorState::default();
        assert!(state.is_stable());
        assert_eq!(state.version, 0);
        assert_eq!(state.queue_len(), 0);
    }

    // ===== P3-11 资源管理测试 =====

    #[test]
    fn test_register_io_request_records_timestamp() {
        // P3-11: register_io_request 应同时记录时间戳
        let mut state = ReactorState::new();
        let id = FactId(1);
        state.register_io_request(id, IoType::call_external());
        assert!(state.pending_io_timestamps.contains_key(&id));
    }

    #[test]
    fn test_complete_io_request_removes_timestamp() {
        // P3-11: complete_io_request 应同时移除时间戳
        let mut state = ReactorState::new();
        let id = FactId(1);
        state.register_io_request(id, IoType::call_external());
        assert!(state.pending_io_timestamps.contains_key(&id));
        assert!(state.complete_io_request(id));
        assert!(!state.pending_io_timestamps.contains_key(&id));
    }

    #[test]
    fn test_scan_io_timeouts_no_pending() {
        // P3-11: 无 pending I/O 时 scan 返回空
        let state = ReactorState::new();
        let (warn_ids, error_ids) =
            state.scan_io_timeouts(Duration::from_secs(30), Duration::from_secs(60));
        assert!(warn_ids.is_empty());
        assert!(error_ids.is_empty());
    }

    #[test]
    fn test_scan_io_timeouts_warn_level() {
        // P3-11: 超过 warn_timeout 但未超过 error_timeout
        let mut state = ReactorState::new();
        let id = FactId(1);
        state.register_io_request(id, IoType::call_external());
        // 模拟 35s 前注册的请求（超过 30s warn，未超过 60s error）
        state
            .pending_io_timestamps
            .insert(id, Instant::now() - Duration::from_secs(35));
        let (warn_ids, error_ids) =
            state.scan_io_timeouts(Duration::from_secs(30), Duration::from_secs(60));
        assert_eq!(warn_ids, vec![FactId(1)]);
        assert!(error_ids.is_empty());
    }

    #[test]
    fn test_scan_io_timeouts_error_level() {
        // P3-11: 超过 error_timeout
        let mut state = ReactorState::new();
        let id = FactId(1);
        state.register_io_request(id, IoType::call_external());
        // 模拟 65s 前注册的请求（超过 60s error）
        state
            .pending_io_timestamps
            .insert(id, Instant::now() - Duration::from_secs(65));
        let (warn_ids, error_ids) =
            state.scan_io_timeouts(Duration::from_secs(30), Duration::from_secs(60));
        // 超过 error 阈值的不会同时出现在 warn 列表中
        assert!(warn_ids.is_empty());
        assert_eq!(error_ids, vec![FactId(1)]);
    }

    #[test]
    fn test_scan_io_timeouts_mixed() {
        // P3-11: 混合场景：一个 warn 级别，一个 error 级别，一个正常
        let mut state = ReactorState::new();
        let warn_id = FactId(1);
        let error_id = FactId(2);
        let normal_id = FactId(3);
        state.register_io_request(warn_id, IoType::call_external());
        state.register_io_request(error_id, IoType::call_external());
        state.register_io_request(normal_id, IoType::call_external());
        // 手动设置时间戳
        state
            .pending_io_timestamps
            .insert(warn_id, Instant::now() - Duration::from_secs(40));
        state
            .pending_io_timestamps
            .insert(error_id, Instant::now() - Duration::from_secs(70));
        // normal_id 保持当前时间
        let (warn_ids, error_ids) =
            state.scan_io_timeouts(Duration::from_secs(30), Duration::from_secs(60));
        assert_eq!(warn_ids, vec![FactId(1)]);
        assert_eq!(error_ids, vec![FactId(2)]);
    }

    #[test]
    fn test_force_remove_io_request() {
        // P3-11 + 阶段3-1.4: force_remove_io_request 应清除所有相关状态
        let mut state = ReactorState::new();
        let id = FactId(1);
        state.register_io_request(id, IoType::call_external());
        state.save_io_instruction(id, JsonValue::string("original_instruction"), FactId(100));

        // 确认状态已设置
        assert_eq!(state.pending_io_count, 1);
        assert!(state.pending_requests.contains(&id));
        assert!(state.pending_io_instructions.contains_key(&id));
        assert!(state.pending_io_timestamps.contains_key(&id));
        assert!(state.pending_io_types.contains_key(&id));

        // 强制移除
        state.force_remove_io_request(id);

        // 确认所有相关状态已清除
        assert_eq!(state.pending_io_count, 0);
        assert!(!state.pending_requests.contains(&id));
        assert!(!state.pending_io_instructions.contains_key(&id));
        assert!(!state.pending_io_timestamps.contains_key(&id));
        assert!(!state.pending_io_types.contains_key(&id));
    }

    #[test]
    fn test_force_remove_nonexistent_io_request() {
        // P3-11: 强制移除不存在的请求不应 panic
        let mut state = ReactorState::new();
        state.force_remove_io_request(FactId(999));
        assert_eq!(state.pending_io_count, 0);
    }

    // ===== 阶段3-1.4 I/O 类型记录测试 =====

    #[test]
    fn test_register_io_request_records_io_type() {
        // 阶段3-1.4: register_io_request 应同时记录 io_type
        let mut state = ReactorState::new();
        let id = FactId(1);
        state.register_io_request(id, IoType::query_db());
        assert_eq!(state.pending_io_types.get(&id), Some(&IoType::query_db()));
    }

    #[test]
    fn test_complete_io_request_removes_io_type() {
        // 阶段3-1.4: complete_io_request 应同时移除 io_type 记录
        let mut state = ReactorState::new();
        let id = FactId(1);
        state.register_io_request(id, IoType::call_external());
        assert!(state.pending_io_types.contains_key(&id));
        assert!(state.complete_io_request(id));
        assert!(!state.pending_io_types.contains_key(&id));
    }

    #[test]
    fn test_register_multiple_io_types() {
        // 阶段3-1.4: 不同请求记录不同 io_type
        let mut state = ReactorState::new();
        state.register_io_request(FactId(1), IoType::call_external());
        state.register_io_request(FactId(2), IoType::query_db());
        state.register_io_request(FactId(3), IoType::http_get());

        assert_eq!(state.pending_io_types.len(), 3);
        assert_eq!(
            state.pending_io_types.get(&FactId(1)),
            Some(&IoType::call_external())
        );
        assert_eq!(
            state.pending_io_types.get(&FactId(2)),
            Some(&IoType::query_db())
        );
        assert_eq!(
            state.pending_io_types.get(&FactId(3)),
            Some(&IoType::http_get())
        );
    }
}

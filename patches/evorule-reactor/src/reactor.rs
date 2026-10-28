// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 反应器核心 - 事实驱动的状态转换引擎
//!
//! # 架构
//! - 双通道：command 通道（用户→反应器），event 通道（反应器→用户）
//! - 每轮：等待 Fact → 处理 Fact → 持续执行队列指令直到空或 IoRequired
//! - max_rounds 限制**指令执行步数**（替代 max_steps），不限制 Fact 数
//! - 稳定条件：队列空 + 无待处理 I/O + 已执行过（steps > 0）
//! - 所有 Fact 追加到 FactsLog 审计链，支持审计重放

use crate::channel::ChannelPair;
use crate::error::ReactorError;
use crate::fact::{Fact, FactId, FactIdGenerator, IoType, TraceHit};
use crate::facts_log::FactsLog;
use crate::invariants::InvariantViolation;
use crate::phase::ReactorPhase;
use crate::runtime::{ClockHandle, RuntimeClock};
use crate::stable_detector::StableDetector;
use crate::state::ReactorState;
use crate::{EventReceiver, EventSender, FactReceiver, FactSender};

use evorule_tcb::path::resolve_path_mut;
use evorule_tcb::{execute_transition, JsonValue, TransitionResult};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use std::collections::HashSet;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 默认队列长度上限（P3-11：超过时 80% warn / 100% Error）
const DEFAULT_MAX_QUEUE_LEN: usize = 1000;

/// 默认 I/O 超时检查间隔（P3-11：阻塞等待时定期扫描超时）
const IO_TIMEOUT_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// 阶段3-1.3：Executing 阶段快照更新间隔（每 N 步更新一次）
///
/// 反应器在 Executing 阶段的 `while` 循环中连续执行指令时，
/// 每执行 N 步更新一次 `ReactorStateSnapshot`，让 tier2 能观察到执行进度。
/// 选用 100 的原因：
/// - 锁开销可忽略（纳秒级，相对 `execute_transition` 的微秒级可不计）
/// - tier2 轮询 Prometheus 指标的典型间隔为 5-15s，
///   100 步粒度足以反映宏观进度而不产生噪声
const SNAPSHOT_UPDATE_INTERVAL: usize = 100;

/// 反应器状态快照（只读访问，供 tier2 查询）
///
/// 反应器主循环定期更新此快照，ReactorHandle 通过它暴露状态机语义。
/// 所有字段都是机制（控制层状态），非业务语义。
///
/// # version 字段说明
///
/// `version` 字段表示**状态变更次数**（state_version），
/// 只在 PayloadUpdate/IoResponse/StateTransition 时递增。
/// Command/Stable/Error 不递增（不改变业务状态）。
///
/// 注意: 此字段不是"Fact 处理总深度"（causal_depth）。
/// 真正的 causal_depth 应通过 FactsLog 的 Fact 总数计算。
/// HTTP API 的 `reactor.causal_depth` 字段名保留了历史兼容性，
/// 但实际语义是 state_version（状态变更次数），不是 Fact 处理总深度。
#[derive(Debug, Clone, Default)]
pub struct ReactorStateSnapshot {
    /// 当前执行阶段
    pub phase: ReactorPhase,
    /// 状态变更次数（state_version，只在 PayloadUpdate/IoResponse/StateTransition 时递增）
    pub version: u64,
    /// 不变式违规累计计数
    pub structural_invariant_violations: u64,
    /// 待响应的 I/O 请求数量
    pub pending_io_count: usize,
    /// 当前已执行指令步数
    pub steps: usize,
    /// 当前队列长度
    pub queue_len: usize,
    /// 反应器是否已结束（true = 已退出）
    pub finished: bool,
    /// 待响应 I/O 清单（干预可视化：cancel-all 前的取消对象对账面）
    ///
    /// 每轮迭代头随快照刷新；空表=无 pending（Vec 空不分配，热路径零成本）。
    pub pending_io: Vec<PendingIoEntry>,
}

/// 阶段6：pending I/O 详情条目（snapshot 中单个 I/O 请求的状态）
///
/// `started_at` 存的是发射时间戳，inspect 时调用 `started_at.elapsed()`
/// 计算 Duration，避免 snapshot 持续更新时 Duration 失真。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingIoEntry {
    /// I/O 请求的 FactId
    pub id: FactId,
    /// I/O 类型（机制：路由用，非业务语义）
    pub io_type: IoType,
    /// I/O 请求发射时间戳（inspect 时算 elapsed）
    pub started_at: Instant,
}

/// 最近一轮不变式扫描报告（每迭代头随内置自检刷新）
///
/// 反应器每轮迭代头本就运行 `check_invariants`（此前只累计计数+日志），
/// 本报告把违规明细同步发布给外部只读消费——手动扫描端点由此获得
/// 与引擎自身所见一致的最新结果（新鲜度 ≤ 一个迭代间隔），无需额外
/// 按需扫描通路。
#[derive(Debug, Clone, Default)]
pub struct InvariantScanReport {
    /// 产生本报告时的已执行步数（消费方可借此判断报告新鲜度）
    pub steps: usize,
    /// 本轮检出的违规明细（空表 = 干净）
    pub violations: Vec<InvariantViolation>,
}

/// 反应器配置构建器
#[derive(Debug, Clone)]
pub struct ReactorBuilder {
    core_eval: Vec<JsonValue>,
    max_rounds: usize,
    /// P3-11：队列长度上限（默认 1000）
    max_queue_len: usize,
    /// P3-11：I/O 超时警告阈值（默认 30s）
    io_warn_timeout: Duration,
    /// P3-11：I/O 超时错误阈值（默认 60s，超过后发射 Error 恢复反应器）
    io_error_timeout: Duration,
    /// P3-11：I/O 超时检查间隔（默认 5s，测试可缩短）
    io_timeout_check_interval: Duration,
    /// FactsLog（默认使用 `FactsLog::new()` 纯内存模式）
    facts_log: FactsLog,
    /// 执行中断标志（外部可通过 ReactorHandle 设置）
    interrupt_flag: Arc<AtomicBool>,
    /// 取消全部 pending I/O 标志（外部干预原语，ReactorHandle 置位）
    cancel_all_flag: Arc<AtomicBool>,
    /// 最近一轮不变式扫描结果（每迭代头刷新，ReactorHandle 只读）
    latest_invariants: Arc<Mutex<InvariantScanReport>>,
    /// FactId 起始值（None=从 1 开始全新启动，Some(n)=从 n 恢复续跑）
    ///
    /// 用于崩溃恢复：从历史 WAL 中的最大 reactor 内部 FactId + 1 续跑，
    /// 避免 StateTransition/IoRequest/Stable/Error 的 FactId 与历史 WAL 重复。
    fact_id_start: Option<u64>,
    /// v0.2.0 能力4：已知 io_type 集合（快速失败校验，None=不校验透传）
    known_io_types: Option<Arc<HashSet<String>>>,
    /// 运行时时钟（时钟缝，默认 [`SystemClock`]；测试可注入 [`FixedClock`]）
    clock: ClockHandle,
}

impl ReactorBuilder {
    /// 创建构建器
    pub fn new(core_eval: Vec<JsonValue>) -> Self {
        Self {
            core_eval,
            max_rounds: 10000,
            max_queue_len: DEFAULT_MAX_QUEUE_LEN,
            io_warn_timeout: Duration::from_secs(30),
            io_error_timeout: Duration::from_secs(60),
            io_timeout_check_interval: IO_TIMEOUT_CHECK_INTERVAL,
            facts_log: FactsLog::new(),
            interrupt_flag: Arc::new(AtomicBool::new(false)),
            cancel_all_flag: Arc::new(AtomicBool::new(false)),
            latest_invariants: Arc::new(Mutex::new(InvariantScanReport::default())),
            fact_id_start: None,
            known_io_types: None,
            clock: ClockHandle::system(),
        }
    }

    /// 设置最大指令执行步数（硬上界，替代 max_steps）
    pub fn max_rounds(mut self, max_rounds: usize) -> Self {
        self.max_rounds = max_rounds;
        self
    }

    /// 设置队列长度上限（P3-11）
    ///
    /// 队列长度达到 80% 时发射 warn 日志，达到 100% 时发射 `Fact::Error` 并清空队列。
    pub fn max_queue_len(mut self, max_queue_len: usize) -> Self {
        self.max_queue_len = max_queue_len;
        self
    }

    /// 设置 I/O 超时警告阈值（P3-11，默认 30s）
    ///
    /// pending I/O 超过此时长未响应时发射 warn 日志。
    pub fn io_warn_timeout(mut self, timeout: Duration) -> Self {
        self.io_warn_timeout = timeout;
        self
    }

    /// 设置 I/O 超时错误阈值（P3-11，默认 60s）
    ///
    /// pending I/O 超过此时长未响应时发射 `Fact::Error`，移除该 I/O 请求，恢复反应器。
    pub fn io_error_timeout(mut self, timeout: Duration) -> Self {
        self.io_error_timeout = timeout;
        self
    }

    /// 设置 I/O 超时检查间隔（P3-11，默认 5s，测试可缩短）
    ///
    /// 反应器在阻塞等待命令时，每隔此间隔扫描一次 pending I/O 超时。
    /// 生产环境建议 5s，测试环境可设为 50ms 以加速测试。
    pub fn io_timeout_check_interval(mut self, interval: Duration) -> Self {
        self.io_timeout_check_interval = interval;
        self
    }

    /// 设置自定义 FactsLog（用于 WAL 持久化场景）
    ///
    /// 默认使用 `FactsLog::new()`（纯内存模式）。如需 WAL 持久化，
    /// 可先调用 `FactsLog::with_wal()` 创建，再通过此方法传入。
    ///
    /// # 规范合规
    ///
    /// - ✅ 持久化是机制（Rust 可写，见 §2.1）
    /// - ✅ 不涉及业务语义判断
    pub fn facts_log(mut self, facts_log: FactsLog) -> Self {
        self.facts_log = facts_log;
        self
    }

    /// 设置 reactor 内部 FactId 起始值（崩溃恢复续跑用）
    ///
    /// 默认 `None` = 从 1 开始（`FactIdGenerator::new()`）。
    /// `Some(n)` = 从 n 开始（`FactIdGenerator::resume(n)`），用于崩溃恢复续跑，
    /// 避免 reactor 内部产生的 StateTransition/IoRequest/Stable/Error 的 FactId
    /// 与历史 WAL 中同类型 Fact 的 FactId 重复。
    ///
    /// # 规范合规
    ///
    /// - ✅ FactId 是机制（控制层标识），非业务语义
    /// - ✅ 不影响 Kani 不变式（无 proof 验证 FactId 全局唯一性）
    pub fn fact_id_start(mut self, start: u64) -> Self {
        self.fact_id_start = Some(start);
        self
    }

    /// v0.2.0 能力4：注册已知 io_type 集合（快速失败校验）
    ///
    /// 注册后，IoRequired 时若 io_type 不在集合内，立即发射 `Fact::Error`（恢复
    /// v0.1.x 拼错 io_type 快速失败的确定性）。未注册（默认）则透传不校验，
    /// 由 subscriber 决定能否处理。
    ///
    /// 通常从 `IoDispatcher::known_types()` 收集：
    /// ```ignore
    /// let dispatcher = IoDispatcher::builder().register(...).build();
    /// reactor.known_io_types(dispatcher.known_types().map(|t| t.as_str().to_string()));
    /// ```
    pub fn known_io_types(mut self, types: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.known_io_types = Some(Arc::new(types.into_iter().map(Into::into).collect()));
        self
    }

    /// 注入运行时时钟（时钟缝）
    ///
    /// 默认 [`SystemClock`]（native 行为与直接 `Instant::now()` 一致）。
    /// 测试与嵌入式形态可注入 [`FixedClock`] 等确定性实现，
    /// 使超时扫描等时间相关行为可确定性复现。
    pub fn clock<C: RuntimeClock + 'static>(mut self, clock: C) -> Self {
        self.clock = ClockHandle::new(Arc::new(clock));
        self
    }

    /// 构建反应器
    pub fn build(self) -> Reactor {
        Reactor {
            core_eval: self.core_eval,
            max_rounds: self.max_rounds,
            max_queue_len: self.max_queue_len,
            io_warn_timeout: self.io_warn_timeout,
            io_error_timeout: self.io_error_timeout,
            io_timeout_check_interval: self.io_timeout_check_interval,
            facts_log: self.facts_log,
            interrupt_flag: self.interrupt_flag,
            cancel_all_flag: self.cancel_all_flag,
            latest_invariants: self.latest_invariants,
            fact_id_start: self.fact_id_start,
            known_io_types: self.known_io_types,
            clock: self.clock,
        }
    }
}

/// 反应器实例
pub struct Reactor {
    core_eval: Vec<JsonValue>,
    max_rounds: usize,
    /// P3-11：队列长度上限
    max_queue_len: usize,
    /// P3-11：I/O 超时警告阈值
    io_warn_timeout: Duration,
    /// P3-11：I/O 超时错误阈值
    io_error_timeout: Duration,
    /// P3-11：I/O 超时检查间隔
    io_timeout_check_interval: Duration,
    facts_log: FactsLog,
    /// 执行中断标志（外部可通过 ReactorHandle 设置）
    interrupt_flag: Arc<AtomicBool>,
    /// 取消全部 pending I/O 标志（外部干预原语，ReactorHandle 置位）
    cancel_all_flag: Arc<AtomicBool>,
    /// 最近一轮不变式扫描结果（每迭代头刷新，ReactorHandle 只读）
    latest_invariants: Arc<Mutex<InvariantScanReport>>,
    /// FactId 起始值（None=从 1 开始全新启动，Some(n)=从 n 恢复续跑）
    fact_id_start: Option<u64>,
    /// v0.2.0 能力4：已知 io_type 集合（快速失败校验，None=不校验透传）
    known_io_types: Option<Arc<HashSet<String>>>,
    /// 运行时时钟（时钟缝，经 Builder 注入，默认 SystemClock）
    clock: ClockHandle,
}

impl Reactor {
    /// 创建构建器
    pub fn builder(core_eval: Vec<JsonValue>) -> ReactorBuilder {
        ReactorBuilder::new(core_eval)
    }

    /// 启动反应器，返回 (command_tx, event_rx, event_tx, handle, facts_log)
    ///
    /// - `command_tx`：用户通过它提交 Fact（Command/PayloadUpdate/IoResponse）
    /// - `event_rx`：用户通过它接收反应器产生的 Fact（StateTransition/IoRequest/Stable/Error）
    /// - `event_tx`：event 通道发送端克隆，tier2 可通过 `event_tx.subscribe()` 创建额外接收者
    /// - `handle`：反应器任务句柄（含状态快照 + 中断标志，支持只读查询与 interrupt）
    /// - `facts_log`：审计链克隆（可用于审计重放）
    ///
    /// # 阶段6（第四组）
    ///
    /// 返回的 `ReactorHandle` 携带共享快照与中断标志，支持：
    /// - `interrupt()`：请求反应器在当前指令后退出
    /// - `abort()`：强制中止反应器任务
    /// - GDB 风格的 pause/resume/step/inspect + interrupt/watch 已由 [evorule-server 仓](https://gitee.com/evorule/evorule-server) `core/debug_control` 模块实现
    pub fn spawn(
        self,
    ) -> (
        FactSender,
        EventReceiver,
        EventSender,
        ReactorHandle,
        FactsLog,
    ) {
        let channels = ChannelPair::new();
        let facts_log = self.facts_log.clone();
        let event_tx_clone = channels.event_tx.clone();
        // 创建共享状态快照，reactor 主循环更新，ReactorHandle 读取
        let snapshot = Arc::new(Mutex::new(ReactorStateSnapshot::default()));
        let snapshot_for_run = Arc::clone(&snapshot);
        // 执行中断标志（共享在 reactor 主循环与 ReactorHandle 之间）
        let interrupt_flag = self.interrupt_flag.clone();
        let interrupt_flag_for_run = self.interrupt_flag.clone();
        // 取消全部 pending I/O 标志（外部干预原语，同 interrupt 共享模式）
        let cancel_all_flag = self.cancel_all_flag.clone();
        let cancel_all_flag_for_run = self.cancel_all_flag.clone();
        // 最近一轮不变式扫描（每迭代头刷新，handle 只读）
        let latest_invariants = self.latest_invariants.clone();
        let latest_invariants_for_run = self.latest_invariants.clone();

        // W1 方案 b（2026-08-27）：WAL 连续写失败达阈值 → 触发中断标志 +
        // 发射含用户自助指导的 Error fact，终止当前执行以保护可回放性。
        // Error fact 无法经 emit_fact 写入（那正是失败的路径），改经 event
        // 通道直接下发，保证客户端必收到指导信息。
        #[cfg(feature = "persistence")]
        {
            let interrupt_for_cb = interrupt_flag.clone();
            let event_tx_for_cb = channels.event_tx.clone();
            facts_log.set_on_wal_failure_exhausted(move |guidance: &str| {
                tracing::error!("WAL failure escalation: terminating session. {guidance}");
                interrupt_for_cb.store(true, std::sync::atomic::Ordering::Release);
                // 尽力发送；broadcast 无接收者时错误可忽略（客户端重连后从状态查询感知）
                let _ = event_tx_for_cb.send(Fact::Error {
                    id: FactId(u64::MAX),
                    message: guidance.to_string(),
                });
            });
        }

        let handle = tokio::spawn(self.run(
            channels.command_rx,
            channels.event_tx,
            snapshot_for_run,
            interrupt_flag_for_run,
            cancel_all_flag_for_run,
            latest_invariants_for_run,
        ));
        (
            channels.command_tx,
            channels.event_rx,
            event_tx_clone,
            ReactorHandle {
                handle,
                snapshot,
                interrupt_flag,
                cancel_all_flag,
                latest_invariants,
            },
            facts_log,
        )
    }

    /// 反应器主循环（长驻模式）
    ///
    /// 流程：
    /// 1. 非阻塞 drain command 通道中所有待处理 Fact（ISSUE-1 修复：避免多命令丢失）
    /// 2. 稳定检测：队列空 + 无 pending I/O + 已执行过（ISSUE-2：使用 StableDetector）
    /// 3. 如果队列空或等待 I/O 且未 drain 到任何 Fact，阻塞等 Fact
    /// 4. 持续执行队列指令，直到队列空或 IoRequired
    ///    - BUG-3 修复：max_rounds 检查在 pop_instruction 之前，避免指令丢失
    ///    - ISSUE-4/5：FactsLog append 和 event_tx send 错误记录警告日志
    ///
    /// # 长驻模式（v6.1）
    ///
    /// 反应器在 Stable 后**不退出**，重置步数计数器，继续等待下一命令。
    /// 这使 HTTP API 能持续服务多个顺序命令。
    ///
    /// 错误处理：
    /// - `MaxRoundsExceeded`：发射 Error → 清空队列 → 重置 steps → 发射 Stable → 继续
    /// - `TcbError`：发射 Error → 继续（队列空时由稳定检测自动发射 Stable）
    /// - `ChannelClosed`（所有 command_tx 被丢弃）：优雅退出 `Ok(())`
    // 反应器主循环: 多 match 分支 + 嵌套 if, 拆函数影响接口稳定性。详见 GATE_REFERENCE.md §六(豁免索引)
    #[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
    async fn run(
        self,
        mut cmd_rx: FactReceiver,
        event_tx: EventSender,
        snapshot: Arc<Mutex<ReactorStateSnapshot>>,
        interrupt_flag: Arc<AtomicBool>,
        cancel_all_flag: Arc<AtomicBool>,
        latest_invariants: Arc<Mutex<InvariantScanReport>>,
    ) -> Result<(), ReactorError> {
        let mut state = ReactorState::new();
        // 种子化（2026-09-08）：facts_log 已带初始状态时（live fork /
        // fork-from-archive 经 set_initial_state，或 WAL recover 重放后挂载），
        // TCB 必须从该状态续跑——否则 TCB 与审计链快照分叉：链上 payload = X，
        // 实际执行却从空状态起步，fork 语义与"可回放"承诺同时被破坏。
        // 条件 version > 0：全新会话的空链（version 0）无需种子，行为不变。
        let (seed_payload, seed_queue, seed_version) = self.facts_log.snapshot();
        if seed_version > 0 {
            state.payload = seed_payload;
            state.queue = seed_queue.into();
            state.version = seed_version;
            state.prev_version = seed_version.saturating_sub(1);
        }
        let mut id_gen = self
            .fact_id_start
            .map_or_else(FactIdGenerator::new, FactIdGenerator::resume);
        let mut steps: usize = 0;
        // 断点 1 修复：删除 current_cause 全局变量，
        // 改为通过 instruction_causes 队列追踪每个指令的独立 cause。

        tracing::debug!(
            "Reactor started (long-running), max_rounds={}",
            self.max_rounds
        );

        'main: loop {
            // 更新状态快照（供 tier2 只读查询）
            Self::update_snapshot(&snapshot, &state, steps, false);
            // 0. 不变式自检：检查上一轮是否引入结构性违规
            //    违规用 tracing::error! 记录，不中断反应器（符合 F11）；
            //    明细同步发布到共享扫描报告（外部干预端点只读消费）
            let scan_violations = Self::run_invariant_check(&mut state, steps);
            if let Ok(mut report) = latest_invariants.lock() {
                report.steps = steps;
                report.violations = scan_violations;
            }

            // 0.5 检查执行中断标志
            if interrupt_flag.swap(false, std::sync::atomic::Ordering::Acquire) {
                state.phase = ReactorPhase::Error;
                let id = id_gen.next_id();
                let err_fact = Fact::Error {
                    id,
                    message: "Execution interrupted by external request".to_string(),
                };
                Self::emit_fact(&self.facts_log, &event_tx, err_fact);
                // 发射 Stable 恢复
                state.phase = ReactorPhase::Stable;
                let stable_id = id_gen.next_id();
                let stable_fact = Fact::Stable {
                    id: stable_id,
                    version: state.version,
                };
                Self::emit_fact(&self.facts_log, &event_tx, stable_fact);
                steps = 0;
                state.phase = ReactorPhase::Idle;
                continue 'main;
            }

            // 0.55 取消全部 pending I/O（外部干预原语，镜像超时错误路径：
            // 逐请求 emit Fact::Error + force_remove，恢复反应器；不发 Stable——
            // 与超时路径一致，由后续迭代头的稳定检测自然收敛）
            if cancel_all_flag.swap(false, std::sync::atomic::Ordering::Acquire) {
                let cancel_ids: Vec<FactId> = state.pending_io_timestamps.keys().copied().collect();
                for io_id in cancel_ids {
                    tracing::info!(
                        io_request_id = %io_id,
                        "I/O 请求被外部取消（cancel-all pending）：发射 Error 恢复反应器"
                    );
                    let err_fact_id = id_gen.next_id();
                    let err_fact = Fact::Error {
                        id: err_fact_id,
                        message: format!("I/O request {io_id} cancelled by external request"),
                    };
                    Self::emit_fact(&self.facts_log, &event_tx, err_fact);
                    state.force_remove_io_request(io_id);
                }
            }

            // 1. 非阻塞 drain command 通道中所有待处理 Fact
            //    ISSUE-1 修复：避免稳定检测前遗漏通道中已排队的 Fact
            state.phase = ReactorPhase::Draining;
            let mut drained_any = false;
            loop {
                match cmd_rx.try_recv() {
                    Ok(fact) => {
                        drained_any = true;
                        tracing::trace!(
                            phase = %state.phase.as_str(),
                            "Drained fact: {} (id={})",
                            fact.type_name(),
                            fact.id()
                        );
                        // 断点 1 修复：cause 在 handle_fact 中通过 push_back(instruction, fact_id) 关联
                        Self::emit_fact(&self.facts_log, &event_tx, fact.clone());
                        Self::handle_fact(
                            &mut state,
                            fact,
                            &self.facts_log,
                            &event_tx,
                            &mut id_gen,
                        )?;
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        tracing::debug!(
                            "Reactor command channel closed during drain, shutting down"
                        );
                        // 标记反应器已结束，让 tier2 只读 API 返回 None
                        Self::update_snapshot(&snapshot, &state, steps, true);
                        return Ok(());
                    }
                }
            }

            // 2. 稳定检测：队列空 + 无 pending I/O + 已执行过
            //    ISSUE-2 修复：使用 StableDetector::is_stable 静态方法
            //
            //    注意: steps > 0 条件意味着纯 PayloadUpdate（不入队、不增加 steps）
            //    在系统刚启动时不会触发 Stable。tier2 应直接监听 Fact::PayloadUpdate
            //    来确认状态变更，而非依赖 Stable 事件。
            if StableDetector::is_stable(state.queue.len(), state.pending_io_count) && steps > 0 {
                state.phase = ReactorPhase::Stable;
                tracing::info!(
                    phase = %state.phase.as_str(),
                    "Reactor stable after {} steps, version {}",
                    steps,
                    state.version
                );
                let id = id_gen.next_id();
                let fact = Fact::Stable {
                    id,
                    version: state.version,
                };
                Self::emit_fact(&self.facts_log, &event_tx, fact);
                // 长驻模式：重置步数，继续等待下一命令（不退出）
                steps = 0;
                state.phase = ReactorPhase::Idle;
                continue 'main;
            }

            // 3. 如果队列空或等待 I/O 且未 drain 到任何 Fact，阻塞等 Fact
            //    P3-11：使用 timeout 定期扫描 pending I/O 超时（30s warn / 60s error）
            if (state.queue.is_empty() || state.pending_io_count > 0) && !drained_any {
                state.phase = ReactorPhase::AwaitingIo;
                let fact = match tokio::time::timeout(self.io_timeout_check_interval, cmd_rx.recv())
                    .await
                {
                    Ok(Some(f)) => f,
                    Ok(None) => {
                        tracing::debug!("Reactor command channel closed, shutting down");
                        // 标记反应器已结束，让 tier2 只读 API 返回 None
                        Self::update_snapshot(&snapshot, &state, steps, true);
                        return Ok(());
                    }
                    Err(_) => {
                        // P3-11: 超时，扫描 pending I/O 超时（时钟缝：now 由注入时钟提供）
                        Self::check_io_timeouts(
                            &mut state,
                            self.clock.now(),
                            &self.facts_log,
                            &event_tx,
                            &mut id_gen,
                            self.io_warn_timeout,
                            self.io_error_timeout,
                        );
                        continue 'main;
                    }
                };
                tracing::trace!("Processing fact: {} (id={})", fact.type_name(), fact.id());
                // 断点 1 修复：cause 在 handle_fact 中通过 push_back(instruction, fact_id) 关联
                Self::emit_fact(&self.facts_log, &event_tx, fact.clone());
                Self::handle_fact(&mut state, fact, &self.facts_log, &event_tx, &mut id_gen)?;
            }

            // 4. 持续执行队列指令（pending_io==0 时）
            state.phase = ReactorPhase::Executing;
            while state.pending_io_count == 0 {
                // P3-11: max_rounds 80% 警告（首次触发时记录，仅一次）
                let warn_threshold = self.max_rounds * 4 / 5;
                if warn_threshold > 0 && steps == warn_threshold {
                    tracing::warn!(
                        phase = %state.phase.as_str(),
                        steps,
                        max_rounds = self.max_rounds,
                        threshold_pct = 80,
                        "指令执行步数达到 max_rounds 的 80%（{} / {}），即将接近上限",
                        steps,
                        self.max_rounds
                    );
                }

                // BUG-3 修复：先检查 max_rounds，再弹出指令，避免指令丢失
                if steps >= self.max_rounds {
                    state.phase = ReactorPhase::Error;
                    let id = id_gen.next_id();
                    let err = ReactorError::MaxRoundsExceeded {
                        rounds: steps,
                        max_rounds: self.max_rounds,
                    };
                    tracing::error!(phase = %state.phase.as_str(), "{}", err);
                    let fact = Fact::Error {
                        id,
                        message: err.to_string(),
                    };
                    Self::emit_fact(&self.facts_log, &event_tx, fact);
                    // 长驻模式：清空队列，重置步数，发射 Stable，继续等待下一命令
                    // 断点 1 修复：同步清空 cause 队列
                    state.clear_queue();
                    steps = 0;
                    state.phase = ReactorPhase::Stable;
                    let stable_id = id_gen.next_id();
                    let stable_fact = Fact::Stable {
                        id: stable_id,
                        version: state.version,
                    };
                    Self::emit_fact(&self.facts_log, &event_tx, stable_fact);
                    state.phase = ReactorPhase::Idle;
                    continue 'main;
                }

                let (instruction, cause) = match state.pop_instruction() {
                    Some(pair) => pair,
                    None => break, // 队列空
                };
                steps += 1;

                // 每 N 步更新快照，让 tier2 能观察到执行进度
                // （'main 循环开头已更新一次，此处补充 Executing 热路径中的定期更新）
                if steps % SNAPSHOT_UPDATE_INTERVAL == 0 {
                    Self::update_snapshot(&snapshot, &state, steps, false);
                }

                tracing::trace!(
                    phase = %state.phase.as_str(),
                    "Executing instruction (step {}): {:?}",
                    steps,
                    instruction
                );

                let result = execute_transition(
                    &self.core_eval,
                    &instruction,
                    &state.payload,
                    &state.queue.iter().cloned().collect::<Vec<_>>(),
                );

                match result {
                    Ok(TransitionResult::State {
                        new_payload,
                        new_queue,
                        rule_hits,
                    }) => {
                        state.payload = new_payload;
                        // 断点 1 修复：同步重建 cause 队列
                        // new_queue = [新 push 的指令] + [原有指令]
                        // 新 push 的指令继承当前 cause，原有指令保留原 cause
                        state.update_queue_with_causes(new_queue, cause);

                        // P3-11: 队列长度分级告警（80% warn / 100% Error+清空）
                        let queue_len = state.queue.len();
                        let queue_warn_threshold = self.max_queue_len * 4 / 5;
                        if queue_len >= self.max_queue_len && self.max_queue_len > 0 {
                            // 100%：硬限制，发射 Error 并清空队列
                            state.phase = ReactorPhase::Error;
                            tracing::error!(
                                phase = %state.phase.as_str(),
                                queue_len,
                                max_queue_len = self.max_queue_len,
                                "队列长度超过上限，发射 Error 并清空队列"
                            );
                            let err_id = id_gen.next_id();
                            let err_fact = Fact::Error {
                                id: err_id,
                                message: format!(
                                    "Queue length {} exceeds max {}",
                                    queue_len, self.max_queue_len
                                ),
                            };
                            Self::emit_fact(&self.facts_log, &event_tx, err_fact);
                            // 断点 1 修复：同步清空 cause 队列
                            state.clear_queue();
                            // 发射 Stable 恢复
                            state.phase = ReactorPhase::Stable;
                            let stable_id = id_gen.next_id();
                            let stable_fact = Fact::Stable {
                                id: stable_id,
                                version: state.version,
                            };
                            Self::emit_fact(&self.facts_log, &event_tx, stable_fact);
                            steps = 0;
                            state.phase = ReactorPhase::Idle;
                            continue 'main;
                        } else if queue_len >= queue_warn_threshold && queue_warn_threshold > 0 {
                            // 80%：软限制警告
                            tracing::warn!(
                                phase = %state.phase.as_str(),
                                queue_len,
                                max_queue_len = self.max_queue_len,
                                threshold_pct = 80,
                                "队列长度接近上限（80%）：{} / {}",
                                queue_len,
                                self.max_queue_len
                            );
                        }

                        // I/O 恢复执行后清除 __io_results__，防止残留影响后续 I/O 指令。
                        // v0.3.1：core_eval 消费结果后以 null 清除（exists 将 null 视为不存在），
                        // 此处在恢复执行完成后整体移除 __io_results__ 容器并复位 io_recovery 标志。
                        if state.io_recovery {
                            state.clear_io_recovery();
                        }
                        state.bump_version();
                        let id = id_gen.next_id();
                        let fact = Fact::StateTransition {
                            id,
                            cause, // 断点 1 修复：使用指令关联的 cause
                            new_payload: state.payload.clone(),
                            new_queue: state.queue.iter().cloned().collect(),
                        };
                        Self::emit_fact(&self.facts_log, &event_tx, fact);
                        // 收敛转换的命中归因轨迹（紧随 StateTransition，
                        // cause 指向它；记录性事实，不推进版本）
                        let trace_id = id_gen.next_id();
                        let trace_fact = Fact::TransitionTrace {
                            id: trace_id,
                            cause: id,
                            rule_hits: rule_hits.iter().map(TraceHit::from_rule_hit).collect(),
                        };
                        Self::emit_fact(&self.facts_log, &event_tx, trace_fact);
                    }
                    Ok(TransitionResult::Ignored {
                        instruction_type,
                        reason,
                        rule_hits,
                    }) => {
                        // 指令被静默忽略：产生 Error 事实，使系统显式感知此问题
                        // 注意：Error 事实不 bump log version（与 FactsLog 行为对齐），
                        // 因此 reactor 也不 bump version，保持 reactor_version == log_version 不变式
                        state.phase = ReactorPhase::Error;
                        let id = id_gen.next_id();
                        let msg = format!(
                            "Instruction ignored by TCB: type={}, reason={}, instruction={:?}",
                            instruction_type, reason, instruction
                        );
                        tracing::warn!(phase = %state.phase.as_str(), "{}", msg);
                        let fact = Fact::Error { id, message: msg };
                        Self::emit_fact(&self.facts_log, &event_tx, fact);
                        // 静默失败的命中归因（全未命中是死规则检测的
                        // 直接数据源；cause 指向本 Error 事实）
                        let trace_id = id_gen.next_id();
                        let trace_fact = Fact::TransitionTrace {
                            id: trace_id,
                            cause: id,
                            rule_hits: rule_hits.iter().map(TraceHit::from_rule_hit).collect(),
                        };
                        Self::emit_fact(&self.facts_log, &event_tx, trace_fact);
                        state.phase = ReactorPhase::Idle;
                        continue 'main;
                    }
                    Ok(TransitionResult::Halted { rule_index, reason }) => {
                        // enforce 强制原语命中（回归验证）：违规指令被**拒绝执行**
                        // —— 指令已出队即丢弃（不推回队列、不产生 StateTransition、
                        // 不 bump version：payload/queue 保持该步前原样，动作被阻止
                        // 而非仅留痕）。系统独占发射 Violation 事实（三权分立：
                        // 阻止权/记录权在系统，LLM/规则零通道伪造拦截记录），
                        // Halted 信号本身即收敛信号（TCB 已中断剩余 transform 并
                        // 丢弃半成品），无需 TransitionTrace（无 rule_hits 可归因，
                        // 归因以 rule_index+reason 承载）。
                        state.phase = ReactorPhase::Error;
                        let id = id_gen.next_id();
                        tracing::warn!(
                            phase = %state.phase.as_str(),
                            rule_index,
                            %reason,
                            "enforce 强制拦截：违规指令被拒绝执行"
                        );
                        let fact = Fact::Violation {
                            id,
                            cause, // 指向触发违规的来源事实（Command/IoResponse）
                            rule_index: rule_index as u64,
                            reason,
                            instruction: instruction.clone(),
                        };
                        Self::emit_fact(&self.facts_log, &event_tx, fact);
                        state.phase = ReactorPhase::Idle;
                        continue 'main;
                    }
                    Ok(TransitionResult::IoRequired {
                        io_type: io_type_str,
                        params,
                    }) => {
                        let id = id_gen.next_id();
                        // v0.2.0：io_type 透传不校验（校验责任移到 subscriber；
                        // known_io_types 快速失败见能力4/阶段2）。未知 io_type 由
                        // 下游 subscriber 决定能否处理，处理不了 → error IoResponse
                        // → reactor 走现有 Error 路径。
                        // v0.2.0 能力4：known_io_types 快速失败校验（可选）
                        if let Some(known) = &self.known_io_types {
                            if !known.contains(&io_type_str) {
                                state.phase = ReactorPhase::Error;
                                let msg = format!(
                                    "unknown io_type: {} (not in known_io_types, instruction: {:?})",
                                    io_type_str, instruction
                                );
                                tracing::error!(phase = %state.phase.as_str(), "{}", msg);
                                let fact = Fact::Error { id, message: msg };
                                Self::emit_fact(&self.facts_log, &event_tx, fact);
                                state.phase = ReactorPhase::Idle;
                                continue 'main;
                            }
                        }
                        let io_type = IoType::new(&io_type_str);
                        // .clone()：io_type 之后还要用于 debug! 与 Fact::IoRequest
                        state.register_io_request(id, io_type.clone());
                        // BUG 修复：缓存触发 I/O 的原指令及其 cause，IoResponse 到达后重新推送回队列，
                        // 使 core_eval.json 中的 exists(__io_results__.{io_type}) 双路径生效：
                        // 首次执行走 on_false（io_request），恢复执行走 on_true（set 消费结果）。
                        // 断点 1 修复：同时缓存 cause，使恢复执行时 cause 指向正确的 Fact。
                        state.save_io_instruction(id, instruction.clone(), cause);
                        state.phase = ReactorPhase::AwaitingIo;
                        tracing::debug!(
                            phase = %state.phase.as_str(),
                            "IoRequest {} (io_type={})",
                            id,
                            io_type
                        );
                        let fact = Fact::IoRequest {
                            id,
                            cause, // 断点 1 修复：使用指令关联的 cause
                            io_type,
                            params,
                        };
                        Self::emit_fact(&self.facts_log, &event_tx, fact);
                        break; // 退出 while，等待 IoResponse
                    }
                    Err(err) => {
                        state.phase = ReactorPhase::Error;
                        let id = id_gen.next_id();
                        // 断点 2 修复：在 Error 消息中包含指令 Debug 信息，
                        // 使审计链可追溯丢失的指令（指令不推回队列，避免无限循环）
                        let msg = format!(
                            "TCB error at step {}: {} (instruction: {:?})",
                            steps, err, instruction
                        );
                        tracing::error!(phase = %state.phase.as_str(), "{}", msg);
                        let fact = Fact::Error { id, message: msg };
                        Self::emit_fact(&self.facts_log, &event_tx, fact);
                        // 长驻模式：不退出，继续执行队列中剩余指令。
                        // 若队列已空，外层循环的稳定检测会自动发射 Stable。
                        state.phase = ReactorPhase::Idle;
                        continue 'main;
                    }
                }
            }
        }
    }

    /// 更新共享状态快照（供 ReactorHandle 只读查询）
    ///
    /// 反应器主循环每次迭代开头调用，将 `ReactorState` 的关键字段
    /// 复制到 `ReactorStateSnapshot`，使 tier2 无需访问反应器内部状态即可查询。
    ///
    /// # 规范合规
    ///
    /// - ✅ 只读访问（snapshot 由 Arc<Mutex> 共享，仅此处写入）
    /// - ✅ 字段都是机制（控制层状态），非业务语义
    /// - ✅ 锁持有时间极短（仅字段拷贝），不跨 await
    /// - ✅ 锁中毒时不中断反应器（仅记录 warn）
    fn update_snapshot(
        snapshot: &Arc<Mutex<ReactorStateSnapshot>>,
        state: &ReactorState,
        steps: usize,
        finished: bool,
    ) {
        if let Ok(mut snap) = snapshot.lock() {
            snap.phase = state.phase;
            snap.version = state.version;
            snap.structural_invariant_violations = state.structural_invariant_violations;
            snap.pending_io_count = state.pending_io_count;
            snap.steps = steps;
            snap.queue_len = state.queue.len();
            snap.finished = finished;
            // pending I/O 清单（干预可视化面）：有 pending 才重建，空表零分配
            snap.pending_io.clear();
            if state.pending_io_count > 0 {
                snap.pending_io.extend(state.pending_io_timestamps.iter().filter_map(
                    |(io_id, started_at)| {
                        state.pending_io_types.get(io_id).map(|io_type| PendingIoEntry {
                            id: *io_id,
                            io_type: io_type.clone(),
                            started_at: *started_at,
                        })
                    },
                ));
            }
        } else {
            tracing::warn!("ReactorStateSnapshot mutex poisoned, tier2 queries will be stale");
        }
    }

    /// 发射事实：追加到 FactsLog 并发送到 event broadcast 通道
    ///
    /// - FactsLog append 失败：记录 warn（不影响主流程，因为 FactsLog 是审计辅助）
    /// - event_tx send 失败：记录 warn（broadcast 通道无接收者时返回错误）
    fn emit_fact(facts_log: &FactsLog, event_tx: &EventSender, fact: Fact) {
        if let Err(e) = facts_log.append(fact.clone()) {
            tracing::warn!("FactsLog append failed: {}", e);
        }
        if event_tx.send(fact).is_err() {
            tracing::debug!("Event broadcast channel has no receivers, fact not delivered");
        }
    }

    /// 不变式自检（结构性 + 语义性）
    ///
    /// 在主循环每次迭代开头调用：
    /// 检查 5 条结构性不变式（[crate::invariants]），违规用 `tracing::error!` 记录
    ///
    /// 违规累计计数到 `state.structural_invariant_violations`，不中断反应器（符合 F11）。
    /// 返回本轮违规明细（调用方发布到共享扫描报告供外部只读消费）。
    fn run_invariant_check(state: &mut ReactorState, steps: usize) -> Vec<InvariantViolation> {
        // 结构性不变式（硬编码，始终启用）
        let structural = crate::invariants::check_invariants(state, steps);
        let structural_count = structural.len() as u64;
        state.structural_invariant_violations = state
            .structural_invariant_violations
            .saturating_add(structural_count);
        for v in &structural {
            tracing::error!(
                phase = %state.phase.as_str(),
                violation = v.as_str(),
                total_violations = state.structural_invariant_violations,
                "不变式违规: {}",
                v
            );
        }
        structural
    }

    /// P3-11 + 阶段3-1.4: 扫描 pending I/O 超时（分级告警，按 io_type 查表）
    ///
    /// - `warn_ids`：超过 warn 阈值但未超过 error 阈值，记录 warn 日志
    /// - `error_ids`：超过 error 阈值，发射 `Fact::Error`，强制移除请求，恢复反应器
    ///
    /// 此方法在主循环的 `tokio::time::timeout` 超时分支中调用，
    /// 确保长时间未响应的 I/O 不会永久阻塞反应器。
    fn check_io_timeouts(
        state: &mut ReactorState,
        now: Instant,
        facts_log: &FactsLog,
        event_tx: &EventSender,
        id_gen: &mut FactIdGenerator,
        warn_timeout: Duration,
        error_timeout: Duration,
    ) {
        if state.pending_io_count == 0 {
            return;
        }

        let mut warn_ids = Vec::new();
        let mut error_ids = Vec::new();

        for (id, timestamp) in &state.pending_io_timestamps {
            let elapsed = now.duration_since(*timestamp);

            if elapsed >= error_timeout {
                error_ids.push((*id, error_timeout.as_secs()));
            } else if elapsed >= warn_timeout {
                warn_ids.push((*id, warn_timeout.as_secs()));
            }
        }

        for (id, warn_secs) in &warn_ids {
            tracing::warn!(
                io_request_id = %id,
                warn_timeout_secs = warn_secs,
                "I/O 请求超时警告：pending I/O 超过 {}s 未响应",
                warn_secs
            );
        }

        for (id, error_secs) in error_ids {
            tracing::error!(
                io_request_id = %id,
                error_timeout_secs = error_secs,
                "I/O 请求超时错误：pending I/O 超过 {}s 未响应，发射 Error 恢复反应器",
                error_secs
            );
            let err_fact_id = id_gen.next_id();
            let err_fact = Fact::Error {
                id: err_fact_id,
                message: format!("I/O request {} timed out after {}s", id, error_secs),
            };
            Self::emit_fact(facts_log, event_tx, err_fact);
            state.force_remove_io_request(id);
        }
    }

    /// 处理 Fact（仅更新状态，不执行 TCB）
    ///
    /// `facts_log`/`event_tx`/`id_gen`：用于异常路径发射 `Fact::Error`
    /// （ / A1：unknown IoResponse 不再静默忽略）。
    // 7 种 Fact 变体 match, 拆函数需暴露内部状态。详见 GATE_REFERENCE.md §六(豁免索引)
    #[allow(clippy::cognitive_complexity)]
    fn handle_fact(
        state: &mut ReactorState,
        fact: Fact,
        facts_log: &FactsLog,
        event_tx: &EventSender,
        id_gen: &mut FactIdGenerator,
    ) -> Result<(), ReactorError> {
        match fact {
            Fact::Command { id, instruction } => {
                tracing::debug!("Received Command");
                // 断点 1 修复：以 Command 的 FactId 作为 cause 关联到指令
                state.push_back(instruction, id);
            }

            Fact::PayloadUpdate { id: _, path, value } => {
                tracing::debug!("Received PayloadUpdate: {}", path);
                Self::update_payload(state, &path, value)?;
                state.bump_version();
            }

            Fact::IoResponse {
                id: _,
                request_id,
                result,
                error,
            } => {
                tracing::debug!("Received IoResponse for {}", request_id);
                if let Some(err_msg) = &error {
                    tracing::warn!("IoResponse carries error: {}", err_msg);
                }
                // v0.3.1：先取 io_type（complete_io_request 会移除 io_type 记录），
                // 用于将结果注入 `__io_results__.{io_type}`（按类型隔离）。
                let io_type = state.get_io_type(&request_id).cloned();
                if !state.complete_io_request(request_id) {
                    //（A1）：unknown IoResponse 不再静默忽略。
                    // IoResponse 事实本身已入链（command 通道先 emit 再 handle），但
                    // 状态层拒绝消费——链上需有显式异常标记才能自解释（审计重放可
                    // 逐条对账"响应事实 + 异常标记"）。与上方超时路径的 Error-fact
                    // 机制同构；Error 为可恢复事实，不影响会话可用性。
                    let err_fact_id = id_gen.next_id();
                    let err_fact = Fact::Error {
                        id: err_fact_id,
                        message: format!(
                            "IoResponse for unknown/stale request_id {} ignored (duplicate, late-after-timeout, or forged)",
                            request_id
                        ),
                    };
                    Self::emit_fact(facts_log, event_tx, err_fact);
                    tracing::warn!("Unknown IoResponse: {}, recorded as Error fact", request_id);
                    return Ok(());
                }
                // v0.3.1 修复：null 结果与错误响应没有可消费的结果。
                // `exists` 域将 null 视为"已清除/不存在"，若把 null 注入 __io_results__ 后
                // 再重新推送原指令，恢复执行时 exists==false → 指令无限重发 io_request（死循环）。
                // 处理：丢弃缓存的原指令，不再重新推送；错误信息由 warn 日志与
                // FactsLog 中的 IoResponse（error 字段）保留。
                if result.is_null() || error.is_some() {
                    state.take_io_instruction(request_id);
                    state.bump_version();
                    return Ok(());
                }
                if let Some(io_type) = io_type {
                    Self::inject_io_result(state, &io_type, result)?;
                } else {
                    // 理论不可达：register_io_request 总是记录 io_type。
                    // 此处静默跳过注入，避免中断反应器主流程。
                    tracing::warn!(
                        "IoResponse for {} has no recorded io_type, result not injected",
                        request_id
                    );
                }
                // BUG 修复：取出缓存的原指令，重新推送回队列前端。
                // 反应器主循环将再次调用 execute_transition 执行同一指令，
                // 此时 payload.__io_results__.{io_type} 已注入，core_eval.json 中
                // exists(__io_results__.{io_type}) 为真 → 走 on_true 分支，set 消费结果到业务字段。
                if let Some((orig_instruction, orig_cause)) = state.take_io_instruction(request_id)
                {
                    // 断点 1 修复：用缓存的 cause 重新关联指令
                    state.push_front(orig_instruction, orig_cause);
                    // 标记 I/O 恢复执行：下一次 execute_transition 返回 State 后
                    // 需清除 __io_results__，防止残留影响后续不同的 I/O 指令。
                    state.io_recovery = true;
                }
                state.bump_version();
            }

            // 反应器自身产生的 Fact 不应通过 command 通道回来，忽略
            Fact::IoRequest { .. }
            | Fact::StateTransition { .. }
            | Fact::Stable { .. }
            | Fact::Error { .. }
            | Fact::TransitionTrace { .. }
            | Fact::Violation { .. } => {
                tracing::trace!("Ignoring self-produced fact");
            }
        }
        Ok(())
    }

    /// 更新 payload 字段
    ///
    /// - 支持已存在的嵌套路径（通过 resolve_path_mut）
    /// - 支持顶层字段创建（path 不含 `.` 或 `[`）
    /// - 支持递归创建嵌套路径（中间节点不存在时自动创建空对象）
    fn update_payload(
        state: &mut ReactorState,
        path: &str,
        value: JsonValue,
    ) -> Result<(), ReactorError> {
        if let Some(target) = resolve_path_mut(&mut state.payload, path) {
            *target = value;
            return Ok(());
        }

        let parts: Vec<&str> = path.split('.').collect();
        if parts.is_empty() {
            return Err(ReactorError::InvalidState {
                field: "payload path is empty",
            });
        }

        let field = parts.last().ok_or(ReactorError::InvalidState {
            field: "payload path is empty",
        })?;

        let parent_obj = if parts.len() == 1 {
            if let JsonValue::Object(map) = &mut state.payload {
                map
            } else {
                return Err(ReactorError::InvalidState {
                    field: "payload is not an object",
                });
            }
        } else {
            let mut current = &mut state.payload;

            for &part in parts.get(..parts.len() - 1).unwrap_or(&[]) {
                if let JsonValue::Object(map) = current {
                    if !map.contains_key(part) {
                        map.insert(part.to_string(), JsonValue::empty_object());
                    }
                    current = map.get_mut(part).ok_or(ReactorError::InvalidState {
                        field: "failed to access nested path",
                    })?;
                } else {
                    return Err(ReactorError::InvalidState {
                        field: "intermediate path is not an object",
                    });
                }
            }

            if let JsonValue::Object(map) = current {
                map
            } else {
                return Err(ReactorError::InvalidState {
                    field: "parent path is not an object",
                });
            }
        };

        parent_obj.insert(field.to_string(), value);
        Ok(())
    }

    /// 注入 I/O 结果到 payload.__io_results__.{io_type}（v0.3.1 按类型隔离）
    ///
    /// 使用 `update_payload` 处理嵌套路径：`__io_results__` 不存在时自动创建空对象。
    /// 不递增 version（version 递增由调用方在 IoResponse 处理时统一执行）。
    fn inject_io_result(
        state: &mut ReactorState,
        io_type: &IoType,
        result: JsonValue,
    ) -> Result<(), ReactorError> {
        let path = format!("__io_results__.{}", io_type.as_str());
        Self::update_payload(state, &path, result)
    }
}

/// 反应器任务句柄
///
/// # 生命周期（长驻模式）
///
/// 反应器在 Stable 后不退出，持续等待下一命令。终止方式：
/// - **优雅退出**：丢弃所有 `command_tx` 发送端 → `join()` 返回 `Ok(())`
/// - **强制中止**：调用 `abort()` → `join()` 返回 `Err(TaskJoinError)`
///
/// # 阶段3-1.3：只读状态机 API
///
/// `ReactorHandle` 内部持有 `Arc<Mutex<ReactorStateSnapshot>>`，
/// 反应器主循环每次迭代时更新快照。tier2 可通过以下 5 个只读 API
/// 查询反应器的控制层状态，无需访问反应器内部：
///
/// - [`current_phase`](Self::current_phase)：当前执行阶段
/// - [`causal_depth`](Self::causal_depth)：因果链深度（= version 号）
/// - [`structural_invariant_violations`](Self::structural_invariant_violations)：不变式违规累计计数
/// - [`pending_io_count`](Self::pending_io_count)：待响应的 I/O 请求数量
/// - [`current_step`](Self::current_step)：当前已执行指令步数
///
/// 反应器结束后，前 4 个 "当前状态" API 返回 `None`（`structural_invariant_violations`
/// 是累计计数，仍可读）。
///
/// # 阶段6：可观测性与中断（第四组）
///
/// `ReactorHandle` 持有共享快照与中断标志，支持：
///
/// **中断 API**：
/// - [`interrupt`](Self::interrupt)：请求反应器在当前指令后退出（设置标志，主循环检测后退出）
/// - [`abort`](Self::abort)：强制中止反应器任务（tokio::task::abort）
///
/// **只读查询 API**（基于 snapshot）：
/// - [`current_phase`](Self::current_phase)：当前执行阶段
/// - [`causal_depth`](Self::causal_depth)：因果链深度
/// - [`is_finished`](Self::is_finished)：是否已结束
///
/// **调试控制**（由 [evorule-server 仓](https://gitee.com/evorule/evorule-server) `core/debug_control` 模块实现）：
/// - pause/resume/step/inspect + interrupt/watch
/// - 基于 `interrupt()` + FactsLog rewind 实现伪单步
pub struct ReactorHandle {
    handle: JoinHandle<Result<(), ReactorError>>,
    /// 共享状态快照（reactor 主循环更新，只读 API 读取）
    snapshot: Arc<Mutex<ReactorStateSnapshot>>,
    /// 执行中断标志（共享在 reactor 主循环与 handle 之间）
    interrupt_flag: Arc<AtomicBool>,
    /// 取消全部 pending I/O 标志（共享在 reactor 主循环与 handle 之间）
    cancel_all_flag: Arc<AtomicBool>,
    /// 最近一轮不变式扫描报告（每迭代头刷新，handle 只读）
    latest_invariants: Arc<Mutex<InvariantScanReport>>,
}

impl ReactorHandle {
    /// 等待反应器结束
    ///
    /// - 优雅退出（command_tx 全部丢弃）返回 `Ok(())`
    /// - 强制中止（abort）返回 `Err(TaskJoinError)`
    pub async fn join(self) -> Result<(), ReactorError> {
        self.handle.await.map_err(|e| ReactorError::TaskJoinError {
            message: e.to_string(),
        })?
    }

    /// 中止反应器任务（强制）
    pub fn abort(&self) {
        self.handle.abort();
    }

    /// 检查反应器是否已结束
    ///
    /// 综合 `tokio::task` 的结束状态与快照的 `finished` 标志：
    /// - 任务句柄已结束 → `true`
    /// - 快照标记 `finished=true`（reactor 主循环在退出前设置）→ `true`
    /// - 否则 → `false`
    pub fn is_finished(&self) -> bool {
        if self.handle.is_finished() {
            return true;
        }
        self.snapshot.lock().map(|s| s.finished).unwrap_or(false)
    }

    /// 当前执行阶段（阶段3-1.3）
    ///
    /// 返回 `None` 表示反应器已结束或快照锁中毒。
    pub fn current_phase(&self) -> Option<ReactorPhase> {
        let snap = self.snapshot.lock().ok()?;
        if snap.finished {
            return None;
        }
        Some(snap.phase)
    }

    /// 因果链深度（= version 号，阶段3-1.3）
    ///
    /// 返回 `None` 表示反应器已结束或快照锁中毒。
    /// version 是 `u64`，转 `usize` 失败时也返回 `None`。
    pub fn causal_depth(&self) -> Option<usize> {
        let snap = self.snapshot.lock().ok()?;
        if snap.finished {
            return None;
        }
        usize::try_from(snap.version).ok()
    }

    /// 不变式违规累计计数（阶段3-1.3）
    ///
    /// 返回 `u64`：即使反应器已结束，累计计数仍可读（用于事后审计）。
    /// 锁中毒时返回 0。
    pub fn structural_invariant_violations(&self) -> u64 {
        self.snapshot
            .lock()
            .map(|s| s.structural_invariant_violations)
            .unwrap_or(0)
    }

    /// 待响应的 I/O 请求数量（阶段3-1.3）
    ///
    /// 返回 `None` 表示反应器已结束或快照锁中毒。
    pub fn pending_io_count(&self) -> Option<usize> {
        let snap = self.snapshot.lock().ok()?;
        if snap.finished {
            return None;
        }
        Some(snap.pending_io_count)
    }

    /// 当前已执行指令步数（阶段3-1.3）
    ///
    /// 返回 `None` 表示反应器已结束或快照锁中毒。
    pub fn current_step(&self) -> Option<usize> {
        let snap = self.snapshot.lock().ok()?;
        if snap.finished {
            return None;
        }
        Some(snap.steps)
    }

    /// 读取完整快照（阶段3-1.3 + 1.5）
    ///
    /// 返回 `ReactorStateSnapshot` 的克隆。tier2 可基于此构建 Prometheus 指标。
    /// 反应器结束后仍可读取最后一次快照（含 `finished=true`）。
    pub fn snapshot(&self) -> Option<ReactorStateSnapshot> {
        self.snapshot.lock().ok().map(|s| s.clone())
    }

    /// 请求中断反应器执行（控制 API）
    ///
    /// 设置中断标志，reactor 在下次主循环检查点响应：
    /// - 发射 Error 事件（消息："Execution interrupted by external request"）
    /// - 发射 Stable 事件（包含当前 payload 快照）
    /// - 重置步数计数器
    /// - 返回到 Idle 状态等待下一命令
    ///
    /// 与 `abort()` 的区别：
    /// - `interrupt()`：优雅中断，保留当前状态，可继续执行
    /// - `abort()`：强制终止任务，状态不可恢复
    pub fn interrupt(&self) {
        self.interrupt_flag
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// 请求取消全部 pending I/O（外部干预原语）
    ///
    /// 设置取消标志，reactor 主循环在下次迭代头消费：对每个 pending I/O
    /// 镜像超时错误路径——逐请求发射 `Fact::Error`（"cancelled by external
    /// request"）+ `force_remove_io_request`，恢复反应器。与超时路径一致，
    /// 不主动发 Stable（由后续迭代的稳定检测自然收敛）。
    ///
    /// 幂等：无 pending 时置位被空转消费，无副作用。取消清单经 FactsLog
    /// 的 Error 事实可查（进审计链）。调用方可先用 [`snapshot`](Self::snapshot)
    /// 的 `pending_io` 清单对账取消对象。
    ///
    /// 与 [`interrupt`](Self::interrupt) 的区别：interrupt 中断当前指令执行并
    /// 发射 Error+Stable；本原语只清理 pending I/O，不影响在途指令。
    pub fn cancel_all_pending(&self) {
        self.cancel_all_flag
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// 最近一轮不变式扫描报告（只读）
    ///
    /// 反应器每轮迭代头运行内置自检并把违规明细发布到本报告——返回值即
    /// 引擎自身最新所见（新鲜度 ≤ 一个迭代间隔）。`None` = 快照锁中毒。
    /// 违规是否落账（Fact::Violation）由调用方决定，本层不重复落账。
    pub fn latest_invariant_scan(&self) -> Option<InvariantScanReport> {
        self.latest_invariants.lock().ok().map(|r| r.clone())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::panic, clippy::expect_used)]
    use super::*;

    #[test]
    fn test_update_payload_existing_path() {
        let mut state = ReactorState::new();
        state.payload = JsonValue::object_from_pairs(&[("x", JsonValue::Integer(1))]);
        Reactor::update_payload(&mut state, "x", JsonValue::Integer(42)).unwrap();
        assert_eq!(state.payload.get("x"), Some(&JsonValue::Integer(42)));
    }

    #[test]
    fn test_update_payload_top_level_create() {
        let mut state = ReactorState::new();
        Reactor::update_payload(&mut state, "new_field", JsonValue::string("hello")).unwrap();
        assert_eq!(
            state.payload.get("new_field").and_then(|v| v.as_str()),
            Some("hello")
        );
    }

    #[test]
    fn test_update_payload_nested_nonexistent_creates() {
        let mut state = ReactorState::new();
        let result = Reactor::update_payload(&mut state, "a.b.c", JsonValue::Integer(1));
        assert!(result.is_ok());
        assert_eq!(
            state
                .payload
                .get("a")
                .and_then(|v| v.as_object())
                .and_then(|m| m.get("b"))
                .and_then(|v| v.as_object())
                .and_then(|m| m.get("c"))
                .and_then(|v| v.as_i64()),
            Some(1)
        );
    }

    #[test]
    fn test_inject_io_result() {
        let mut state = ReactorState::new();
        let io_type = IoType::call_external();
        Reactor::inject_io_result(&mut state, &io_type, JsonValue::string("llm_response")).unwrap();
        assert_eq!(
            state
                .payload
                .get("__io_results__")
                .and_then(|r| r.get("call_external"))
                .and_then(|v| v.as_str()),
            Some("llm_response")
        );
    }

    // ===== P3-11 资源管理 Builder 配置测试 =====

    /// cancel-all 干预原语全链：pending I/O 挂起 → 快照清单可见 → 取消 →
    /// pending 清零 + 逐请求 Error 事实入链（镜像超时路径语义）。
    #[tokio::test]
    async fn cancel_all_pending_clears_requests_and_emits_errors() {
        // 宪法：任何指令 → 发射 io_request（等外部响应，永久挂起）
        let instruction = JsonValue::object_from_pairs(&[
            ("type", JsonValue::string("io_request")),
            (
                "params",
                JsonValue::object_from_pairs(&[("io_type", JsonValue::string("llm"))]),
            ),
        ]);
        let reactor = Reactor::builder(vec![instruction])
            .io_timeout_check_interval(Duration::from_millis(20))
            .build();
        let (command_tx, _event_rx, _event_tx, handle, facts_log) = reactor.spawn();

        command_tx
            .send(Fact::Command {
                id: FactId(1),
                instruction: JsonValue::object_from_pairs(&[(
                    "type",
                    JsonValue::string("io_request"),
                )]),
            })
            .ok()
            .expect("command channel open");

        // 等待 reactor 侧 pending 登记（快照清单接线验证）
        let mut saw_pending = false;
        for _ in 0..200 {
            if let Some(snap) = handle.snapshot() {
                if snap.pending_io_count == 1 && snap.pending_io.len() == 1 {
                    saw_pending = true;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(saw_pending, "pending I/O 必须进快照清单（干预可视化面）");

        // 取消 → pending 清零
        handle.cancel_all_pending();
        let mut cleared = false;
        for _ in 0..200 {
            if handle
                .snapshot()
                .map(|s| s.pending_io_count == 0 && s.pending_io.is_empty())
                .unwrap_or(false)
            {
                cleared = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(cleared, "cancel-all 后 pending 必须清零");

        // 逐请求 Error 事实入链（取消清单可审计）
        let cancelled: Vec<String> = facts_log
            .history()
            .into_iter()
            .filter_map(|f| match f {
                Fact::Error { message, .. }
                    if message.contains("cancelled by external request") =>
                {
                    Some(message)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            cancelled.len(),
            1,
            "恰好一条取消 Error 事实: {cancelled:?}"
        );

        // 不变式扫描报告可用且干净
        let report = handle
            .latest_invariant_scan()
            .expect("扫描报告可用");
        assert!(report.violations.is_empty(), "正常取消不产生违规");
    }

    #[test]
    fn test_builder_defaults() {
        let builder = ReactorBuilder::new(vec![]);
        assert_eq!(builder.max_rounds, 10000);
        assert_eq!(builder.max_queue_len, DEFAULT_MAX_QUEUE_LEN);
        assert_eq!(builder.io_warn_timeout, Duration::from_secs(30));
        assert_eq!(builder.io_error_timeout, Duration::from_secs(60));
        assert_eq!(builder.io_timeout_check_interval, IO_TIMEOUT_CHECK_INTERVAL);
    }

    #[test]
    fn test_builder_max_queue_len() {
        let builder = ReactorBuilder::new(vec![]).max_queue_len(500);
        assert_eq!(builder.max_queue_len, 500);
    }

    #[test]
    fn test_builder_io_timeouts() {
        let builder = ReactorBuilder::new(vec![])
            .io_warn_timeout(Duration::from_secs(10))
            .io_error_timeout(Duration::from_secs(20));
        assert_eq!(builder.io_warn_timeout, Duration::from_secs(10));
        assert_eq!(builder.io_error_timeout, Duration::from_secs(20));
    }

    #[test]
    fn test_builder_all_p3_11_options() {
        // P3-11: 综合配置
        let builder = ReactorBuilder::new(vec![])
            .max_rounds(100)
            .max_queue_len(200)
            .io_warn_timeout(Duration::from_secs(15))
            .io_error_timeout(Duration::from_secs(45));

        assert_eq!(builder.max_rounds, 100);
        assert_eq!(builder.max_queue_len, 200);
        assert_eq!(builder.io_warn_timeout, Duration::from_secs(15));
        assert_eq!(builder.io_error_timeout, Duration::from_secs(45));
    }

    #[test]
    fn test_max_rounds_80_percent_threshold() {
        // P3-11: 验证 80% 阈值计算
        // max_rounds=100 → warn_threshold=80
        assert_eq!(100usize * 4 / 5, 80);
        // max_rounds=1000 → warn_threshold=800
        assert_eq!(1000usize * 4 / 5, 800);
        // max_rounds=5 → warn_threshold=4
        assert_eq!(5usize * 4 / 5, 4);
        // max_rounds=3 → warn_threshold=2
        assert_eq!(3usize * 4 / 5, 2);
    }

    #[test]
    fn test_queue_80_percent_threshold() {
        // P3-11: 验证队列 80% 阈值计算
        // max_queue_len=1000 → warn_threshold=800
        assert_eq!(1000usize * 4 / 5, 800);
        // max_queue_len=100 → warn_threshold=80
        assert_eq!(100usize * 4 / 5, 80);
        // max_queue_len=10 → warn_threshold=8
        assert_eq!(10usize * 4 / 5, 8);
    }

    // ===== ReactorHandle 只读 API 测试 =====

    #[tokio::test]
    async fn test_handle_snapshot_initial_state() {
        // 反应器刚启动时，快照应反映初始状态
        let reactor = Reactor::builder(vec![]).build();
        let (cmd_tx, _event_rx, _event_tx, handle, _facts_log) = reactor.spawn();

        // 等待反应器主循环更新快照（第一次 'main 迭代开头调用 update_snapshot）
        tokio::time::sleep(Duration::from_millis(50)).await;

        // 因果深度初始为 0
        let depth = handle.causal_depth();
        assert!(depth.is_some(), "causal_depth 应返回 Some");
        assert_eq!(depth.unwrap(), 0);

        // 不变式违规为 0
        assert_eq!(handle.structural_invariant_violations(), 0);

        // pending_io_count 为 0
        let pending = handle.pending_io_count();
        assert!(pending.is_some(), "pending_io_count 应返回 Some");
        assert_eq!(pending.unwrap(), 0);

        // current_step 为 0
        let step = handle.current_step();
        assert!(step.is_some(), "current_step 应返回 Some");
        assert_eq!(step.unwrap(), 0);

        // current_phase 应是 Idle 或 Draining（反应器刚启动可能处于任一阶段）
        let phase = handle.current_phase();
        assert!(phase.is_some(), "current_phase 应返回 Some");
        let p = phase.unwrap();
        assert!(
            p == ReactorPhase::Idle || p == ReactorPhase::Draining,
            "初始阶段应为 Idle 或 Draining，实际: {:?}",
            p
        );

        // snapshot() 应返回完整快照
        let snap = handle.snapshot();
        assert!(snap.is_some(), "snapshot 应返回 Some");
        assert_eq!(snap.unwrap().version, 0);

        // 优雅退出
        drop(cmd_tx);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn test_handle_snapshot_returns_none_after_shutdown() {
        // 反应器结束后，"当前状态" API 返回 None，但 structural_invariant_violations 仍可读
        let reactor = Reactor::builder(vec![]).build();
        let (cmd_tx, _event_rx, _event_tx, handle, _facts_log) = reactor.spawn();

        // 等待反应器启动
        tokio::time::sleep(Duration::from_millis(50)).await;

        // 优雅退出：丢弃所有 command_tx
        drop(cmd_tx);
        tokio::time::sleep(Duration::from_millis(100)).await;

        // 反应器应已结束
        assert!(handle.is_finished(), "反应器应在 command_tx 丢弃后结束");

        // "当前状态" API 应返回 None（finished=true）
        assert_eq!(
            handle.current_phase(),
            None,
            "结束后 current_phase 应返回 None"
        );
        assert_eq!(
            handle.causal_depth(),
            None,
            "结束后 causal_depth 应返回 None"
        );
        assert_eq!(
            handle.pending_io_count(),
            None,
            "结束后 pending_io_count 应返回 None"
        );
        assert_eq!(
            handle.current_step(),
            None,
            "结束后 current_step 应返回 None"
        );

        // structural_invariant_violations 是累计计数，仍可读
        assert_eq!(
            handle.structural_invariant_violations(),
            0,
            "结束后 structural_invariant_violations 仍可读（累计计数）"
        );

        // snapshot() 仍可读最后一次快照（含 finished=true）
        let snap = handle.snapshot();
        assert!(snap.is_some(), "结束后 snapshot 仍可读");
        assert!(snap.unwrap().finished, "快照应标记 finished=true");
    }
}

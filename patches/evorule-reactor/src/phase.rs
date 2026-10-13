// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 反应器执行阶段（控制层状态机显式化）
//!
//! # 设计
//!
//! ReactorPhase 是反应器的**控制层状态**，描述"现在在做什么"：
//! - 不是业务语义（"在算 call_llm"），而是控制语义（"在等 I/O"）
//! - 转移函数是纯算法，可单测
//! - 主循环通过 phase 驱动，可观察、可演化
//!
//! # 规范合规
//!
//! - Phase 是控制层概念，非业务语义（符合 G8）
//! - 转移函数是纯算法（符合机制-策略分离）
//! - 不包含业务术语字符串（符合 §5.2）
//! - 嵌套最多 2 层（符合 F8）
//! - 单函数不超过 50 行（符合 F9）

/// 反应器执行阶段
///
/// 描述反应器主循环当前所处的控制层状态。
/// 每次主循环迭代时更新，用于可观测性和调试。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReactorPhase {
    /// 启动或上一轮 Stable 后，等待第一个 Fact
    #[default]
    Idle,
    /// 非阻塞 drain command 通道中所有待处理 Fact
    Draining,
    /// 持续执行队列指令（pending_io == 0）
    Executing,
    /// 等待 I/O 响应（pending_io > 0）
    AwaitingIo,
    /// 本轮已 Stable，发射 Stable Fact 后回到 Idle
    Stable,
    /// 错误状态（max_rounds/TCB/I/O timeout），恢复后回到 Idle
    Error,
}

impl ReactorPhase {
    /// 阶段名称（用于 tracing 和 Prometheus 标签）
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Draining => "draining",
            Self::Executing => "executing",
            Self::AwaitingIo => "awaiting_io",
            Self::Stable => "stable",
            Self::Error => "error",
        }
    }
}

/// 阶段转移上下文（纯数据，用于转移函数判断）
///
/// 从 ReactorState 提取的关键信息，使转移函数成为纯函数。
#[derive(Debug, Clone)]
pub struct PhaseContext {
    /// 队列是否为空
    pub queue_empty: bool,
    /// pending I/O 数量
    pub pending_io: usize,
    /// 已执行步数
    pub steps: usize,
    /// 本轮是否 drain 到任何 Fact
    pub drained_any: bool,
}

impl ReactorPhase {
    /// 阶段转移函数（纯算法，可单测）
    ///
    /// 根据当前阶段和上下文，返回下一个阶段。
    /// 此函数不修改任何状态，仅做逻辑判断。
    pub fn next(self, ctx: &PhaseContext) -> Self {
        match self {
            Self::Idle => Self::Draining,
            Self::Draining => Self::next_after_draining(ctx),
            Self::Executing => Self::next_after_executing(ctx),
            Self::AwaitingIo | Self::Stable | Self::Error => Self::Idle,
        }
    }

    /// Draining 后的转移：根据上下文判断下一个阶段
    fn next_after_draining(ctx: &PhaseContext) -> Self {
        if ctx.pending_io > 0 {
            Self::AwaitingIo
        } else if !ctx.queue_empty {
            Self::Executing
        } else if ctx.steps > 0 {
            Self::Stable
        } else {
            Self::Idle
        }
    }

    /// Executing 后的转移：根据上下文判断下一个阶段
    fn next_after_executing(ctx: &PhaseContext) -> Self {
        if ctx.pending_io > 0 {
            Self::AwaitingIo
        } else if ctx.queue_empty {
            Self::post_execution(ctx)
        } else {
            Self::Executing
        }
    }

    /// 执行完毕后的转移：判断稳定还是回到 Idle
    fn post_execution(ctx: &PhaseContext) -> Self {
        if ctx.steps > 0 {
            Self::Stable
        } else {
            Self::Idle
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(queue_empty: bool, pending_io: usize, steps: usize, drained_any: bool) -> PhaseContext {
        PhaseContext {
            queue_empty,
            pending_io,
            steps,
            drained_any,
        }
    }

    #[test]
    fn test_phase_idle_to_draining() {
        let phase = ReactorPhase::Idle;
        let c = ctx(true, 0, 0, false);
        assert_eq!(phase.next(&c), ReactorPhase::Draining);
    }

    #[test]
    fn test_phase_draining_to_executing_when_queue_nonempty() {
        let phase = ReactorPhase::Draining;
        let c = ctx(false, 0, 0, true);
        assert_eq!(phase.next(&c), ReactorPhase::Executing);
    }

    #[test]
    fn test_phase_draining_to_awaiting_io() {
        let phase = ReactorPhase::Draining;
        let c = ctx(true, 2, 5, true);
        assert_eq!(phase.next(&c), ReactorPhase::AwaitingIo);
    }

    #[test]
    fn test_phase_draining_to_stable() {
        let phase = ReactorPhase::Draining;
        let c = ctx(true, 0, 10, true);
        assert_eq!(phase.next(&c), ReactorPhase::Stable);
    }

    #[test]
    fn test_phase_draining_to_idle_when_no_work() {
        let phase = ReactorPhase::Draining;
        let c = ctx(true, 0, 0, false);
        assert_eq!(phase.next(&c), ReactorPhase::Idle);
    }

    #[test]
    fn test_phase_executing_to_awaiting_io() {
        let phase = ReactorPhase::Executing;
        let c = ctx(false, 1, 3, true);
        assert_eq!(phase.next(&c), ReactorPhase::AwaitingIo);
    }

    #[test]
    fn test_phase_executing_to_stable_when_queue_empty() {
        let phase = ReactorPhase::Executing;
        let c = ctx(true, 0, 5, true);
        assert_eq!(phase.next(&c), ReactorPhase::Stable);
    }

    #[test]
    fn test_phase_executing_continues_when_queue_nonempty() {
        let phase = ReactorPhase::Executing;
        let c = ctx(false, 0, 3, true);
        assert_eq!(phase.next(&c), ReactorPhase::Executing);
    }

    #[test]
    fn test_phase_awaiting_io_to_idle() {
        let phase = ReactorPhase::AwaitingIo;
        let c = ctx(false, 0, 5, true);
        assert_eq!(phase.next(&c), ReactorPhase::Idle);
    }

    #[test]
    fn test_phase_stable_to_idle() {
        let phase = ReactorPhase::Stable;
        let c = ctx(true, 0, 0, false);
        assert_eq!(phase.next(&c), ReactorPhase::Idle);
    }

    #[test]
    fn test_phase_error_to_idle() {
        let phase = ReactorPhase::Error;
        let c = ctx(true, 0, 0, false);
        assert_eq!(phase.next(&c), ReactorPhase::Idle);
    }

    #[test]
    fn test_phase_as_str() {
        assert_eq!(ReactorPhase::Idle.as_str(), "idle");
        assert_eq!(ReactorPhase::Draining.as_str(), "draining");
        assert_eq!(ReactorPhase::Executing.as_str(), "executing");
        assert_eq!(ReactorPhase::AwaitingIo.as_str(), "awaiting_io");
        assert_eq!(ReactorPhase::Stable.as_str(), "stable");
        assert_eq!(ReactorPhase::Error.as_str(), "error");
    }
}

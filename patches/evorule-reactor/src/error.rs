// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 反应器错误类型

/// 反应器错误
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReactorError {
    /// 通道已关闭（所有发送者被丢弃）
    ChannelClosed,

    /// 达到最大轮次限制
    MaxRoundsExceeded {
        /// 实际轮次数
        rounds: usize,
        /// 最大允许轮次数
        max_rounds: usize,
    },

    /// TCB 执行错误（包含原始错误信息）
    TcbError {
        /// 错误描述
        message: String,
    },

    /// 无效状态（payload 或 queue 缺失）
    InvalidState {
        /// 缺失的字段
        field: &'static str,
    },

    /// 取消信号（外部请求停止）
    Cancelled,

    /// 反应器任务异常终止（panic 或被 abort）
    TaskJoinError {
        /// 错误描述
        message: String,
    },
}

impl core::fmt::Display for ReactorError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ReactorError::ChannelClosed => write!(f, "fact channel closed"),
            ReactorError::MaxRoundsExceeded { rounds, max_rounds } => {
                write!(f, "max rounds exceeded: {} > {}", rounds, max_rounds)
            }
            ReactorError::TcbError { message } => write!(f, "TCB error: {}", message),
            ReactorError::InvalidState { field } => write!(f, "invalid state: missing {}", field),
            ReactorError::Cancelled => write!(f, "reactor cancelled"),
            ReactorError::TaskJoinError { message } => write!(f, "task join error: {}", message),
        }
    }
}

impl std::error::Error for ReactorError {}

impl From<evorule_tcb::TcbError> for ReactorError {
    fn from(err: evorule_tcb::TcbError) -> Self {
        ReactorError::TcbError {
            message: err.to_string(),
        }
    }
}

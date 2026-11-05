// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 引擎自 watchdog（运行时冻结兜底面）：进程内存活但 runtime 全冻结的最后一道闸。
//!
//! 背景：事件循环悬死形态=进程存活而全部端点无响应（同 runtime 共享 worker
//! 的同步阻塞 IO 拖冻）。壳 guard 只盯进程存活，救不了
//! 「活而哑」；本模块在进程内自监测：
//!
//! - 心跳任务（tokio）：每 5s 对 [`HEARTBEAT`] 原子计数 +1。tokio runtime
//!   一旦全冻结，该任务停摆，计数即断流——它就是「runtime 是否还能调度」
//!   的直接信号（B-139 修复后正常形态不应再冻结，本模块为未知阻塞源兜底）。
//! - watchdog 线程（std::thread）：**独立于 tokio runtime**——async 侧冻结
//!   会拖死一切 tokio 任务，所以监测者必须是同步线程。检测计数断流超过
//!   [`STALL_LIMIT_SECS`]（120s）→ `error!` 告警 + 退出码 3，交壳 guard
//!   组重启（guard 对退出码 3 的处置=引擎单件重启）。
//!
//! 启动宽限：心跳任务 spawn 前计数恒为 0；watchdog 见到首个非零计数才开始
//! 断流判定，启动慢不会被误杀。宽限上限 [`START_GRACE_SECS`]（600s）内未起
//! 心跳只 `warn!` 不退出（启动期故障交由启动自身的失败路径处理）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// 心跳 bump 周期（秒）。
pub const BUMP_INTERVAL_SECS: u64 = 5;
/// 计数断流判定阈值（秒）。24 个 bump 周期的窗口，容忍负载毛刺；
/// 超过即判 runtime 冻结。
pub const STALL_LIMIT_SECS: u64 = 120;
/// 启动宽限（秒）：首个心跳出现前的等待上限，超时只告警不退出。
pub const START_GRACE_SECS: u64 = 600;
/// 壳 guard 约定的引擎自重启退出码。
pub const WATCHDOG_EXIT_CODE: i32 = 3;

static HEARTBEAT: AtomicU64 = AtomicU64::new(0);

/// 计数断流判定（纯函数，测试锚）。
pub fn is_stalled(last_seen: u64, current_seen: u64) -> bool {
    current_seen == last_seen
}

/// 启动心跳任务 + watchdog 线程。须在 `#[tokio::main]` 上下文内调用
/// （心跳任务依赖 runtime），且在日志初始化之后（告警走文件持久化面）。
pub fn start() {
    tokio::spawn(async {
        let mut tick = tokio::time::interval(Duration::from_secs(BUMP_INTERVAL_SECS));
        loop {
            tick.tick().await;
            HEARTBEAT.fetch_add(1, Ordering::Relaxed);
        }
    });

    let watchdog_body = || {
        // 等首个心跳（启动宽限）
        let started = Instant::now();
        while HEARTBEAT.load(Ordering::Relaxed) == 0 {
            if started.elapsed() >= Duration::from_secs(START_GRACE_SECS) {
                tracing::warn!(
                    "watchdog: 心跳任务 {}s 未启动（启动宽限耗尽），仅告警不退出",
                    START_GRACE_SECS
                );
                return;
            }
            std::thread::sleep(Duration::from_secs(1));
        }

        // 断流监控
        let mut last_seen = HEARTBEAT.load(Ordering::Relaxed);
        let mut stalled_since: Option<Instant> = None;
        loop {
            std::thread::sleep(Duration::from_secs(BUMP_INTERVAL_SECS));
            let current = HEARTBEAT.load(Ordering::Relaxed);
            if is_stalled(last_seen, current) {
                let since = *stalled_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= Duration::from_secs(STALL_LIMIT_SECS) {
                    tracing::error!(
                        "watchdog: 心跳断流超过 {}s（tokio runtime 疑似全冻结，\
                             进程活而调度停）——退出码 {} 交壳 guard 组重启",
                        STALL_LIMIT_SECS,
                        WATCHDOG_EXIT_CODE
                    );
                    // 给 tracing_appender 非阻塞 writer 一点刷盘窗口
                    std::thread::sleep(Duration::from_secs(1));
                    std::process::exit(WATCHDOG_EXIT_CODE);
                }
            } else {
                stalled_since = None;
            }
            last_seen = current;
        }
    };

    // watchdog 缺席=兜底面缺失，但进程本身可正常运行——只告警不杀进程
    if let Err(e) = std::thread::Builder::new()
        .name("engine-watchdog".into())
        .spawn(watchdog_body)
    {
        tracing::error!("watchdog 线程启动失败（兜底面缺席）: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stalled_iff_counter_frozen() {
        assert!(is_stalled(41, 41), "计数不变=断流");
        assert!(!is_stalled(41, 42), "计数推进=存活");
        assert!(!is_stalled(41, 100), "大幅推进=存活");
    }
}

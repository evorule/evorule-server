// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `robot_move_joints` 原生实现 —— 确定性仿真。
//!
//! # 确定性改造（墙钟隔离纪律）
//! Python 基线用 `time.strftime('%Y') + uuid4` 生成 trajectory_id，破坏可重放性。
//! 这里改用**进程内逻辑计数器**（Arc<AtomicU64>，跨会话共享，充当逻辑时钟）：
//! 相同调用序列 → 相同 ID 序列，可重放、可审计。浮点 speed 以字符串返回。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use evorule_tcb::JsonValue;

use crate::{arg_f64, obj, NativeService};

/// `robot_move_joints` 原生服务
pub struct RobotMove {
    /// 逻辑时钟（确定性 trajectory_id 序号）
    seq: Arc<AtomicU64>,
}

impl Default for RobotMove {
    fn default() -> Self {
        Self {
            seq: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl NativeService for RobotMove {
    fn execute(&self, args: &JsonValue) -> evorule_reactor::IoResult {
        let joints = args
            .get("joints")
            .and_then(|v| v.as_array())
            .map(|a| a.to_vec())
            .unwrap_or_default();
        if joints.is_empty() {
            return Ok(obj(vec![
                ("status", JsonValue::string("ERROR")),
                ("trajectory_id", JsonValue::Null),
                ("message", JsonValue::string("joints 不能为空")),
            ]));
        }
        let speed = arg_f64(args, "speed", 0.5);
        let n = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(obj(vec![
            ("status", JsonValue::string("OK")),
            ("trajectory_id", JsonValue::string(format!("TRAJ-{n:06}"))),
            (
                "message",
                JsonValue::string(format!(
                    "simulated move {} joints at speed {speed}",
                    joints.len()
                )),
            ),
        ]))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::NativeService;

    #[test]
    fn test_empty_joints_returns_error() {
        let svc = RobotMove::default();
        let args = JsonValue::object_from_pairs(&[("speed", JsonValue::string("0.5"))]);
        let r = svc.execute(&args).unwrap();
        assert_eq!(r.get("status").and_then(|v| v.as_str()), Some("ERROR"));
        assert!(r.get("trajectory_id").map(|v| v.is_null()).unwrap_or(false));
    }

    #[test]
    fn test_ok_with_deterministic_id() {
        let svc = RobotMove::default();
        let args = JsonValue::object_from_pairs(&[
            (
                "joints",
                JsonValue::Array(vec![JsonValue::string("0.1"), JsonValue::string("0.2")]),
            ),
            ("speed", JsonValue::string("0.5")),
        ]);
        let r = svc.execute(&args).unwrap();
        assert_eq!(r.get("status").and_then(|v| v.as_str()), Some("OK"));
        let id = r.get("trajectory_id").and_then(|v| v.as_str()).unwrap();
        assert!(id.starts_with("TRAJ-"), "确定性 ID: {id}");
        assert_eq!(
            r.get("message").and_then(|v| v.as_str()),
            Some("simulated move 2 joints at speed 0.5")
        );
    }
}

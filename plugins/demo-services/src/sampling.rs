// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `sampling_service` 原生实现 —— 有状态计数，每 N 次调用返回 `trigger=true` 并重置。
//!
//! 与 Python 基线一致：计数是**跨会话全局**（Python 用模块级全局 `_counter` + Lock），
//! 此处用 `AtomicI64`（路由器共享同一实例），保证相同调用序列 → 相同结果。

use std::sync::atomic::{AtomicI64, Ordering};

use evorule_tcb::JsonValue;

use crate::{arg_i64, obj, NativeService};

/// `sampling_service` 原生服务
pub struct Sampling {
    counter: AtomicI64,
}

impl Default for Sampling {
    fn default() -> Self {
        Self {
            counter: AtomicI64::new(0),
        }
    }
}

impl NativeService for Sampling {
    fn execute(&self, args: &JsonValue) -> evorule_reactor::IoResult {
        let interval = arg_i64(args, "sample_interval", 5).max(1);
        let c = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
        if c >= interval {
            self.counter.store(0, Ordering::SeqCst);
            Ok(obj(vec![
                ("trigger", JsonValue::Bool(true)),
                ("counter", JsonValue::Integer(0)),
                ("sample_interval", JsonValue::Integer(interval)),
            ]))
        } else {
            Ok(obj(vec![
                ("trigger", JsonValue::Bool(false)),
                ("counter", JsonValue::Integer(c)),
                ("sample_interval", JsonValue::Integer(interval)),
            ]))
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::NativeService;

    #[test]
    fn test_sampling_trigger_every_interval() {
        let svc = Sampling::default();
        let args = JsonValue::object_from_pairs(&[("sample_interval", JsonValue::Integer(3))]);
        let r1 = svc.execute(&args).unwrap();
        assert_eq!(r1.get("trigger").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(r1.get("counter").and_then(|v| v.as_i64()), Some(1));
        let r2 = svc.execute(&args).unwrap();
        assert_eq!(r2.get("counter").and_then(|v| v.as_i64()), Some(2));
        let r3 = svc.execute(&args).unwrap();
        assert_eq!(r3.get("trigger").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(r3.get("counter").and_then(|v| v.as_i64()), Some(0));
        // 重置后重新计数
        let r4 = svc.execute(&args).unwrap();
        assert_eq!(r4.get("trigger").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(r4.get("counter").and_then(|v| v.as_i64()), Some(1));
    }
}

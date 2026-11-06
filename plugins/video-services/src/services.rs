// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 服务实现层（骨架批：探针服务）。
//!
//! 信封 v1：成功出参保留键 `status`=determined + `determinism`=
//! bitwise_replayable 打头；error 走框架 `Err(String)` 带自诊断指引。

use std::sync::Arc;

use evorule_plugin_kit::NativeService;
use evorule_reactor::IoResult;
use evorule_tcb::JsonValue;

fn envelope(pairs: &[(&str, JsonValue)]) -> JsonValue {
    let mut all: Vec<(&str, JsonValue)> = vec![
        ("status", JsonValue::string("determined")),
        ("determinism", JsonValue::string("bitwise_replayable")),
    ];
    all.extend_from_slice(pairs);
    JsonValue::object_from_pairs(&all)
}

/// 探针服务：回显可选 `echo` 字符串，恒返固定信封结构（同步 execute，
/// 纯计算零出网——与全族确定性纪律一致）。
struct VideoSkeletonProbe;

impl NativeService for VideoSkeletonProbe {
    fn execute(&self, args: &JsonValue) -> IoResult {
        // 参数形态宽松：echo 可缺省（骨架探针无必填业务参数）
        let echo = args.get("echo").and_then(|v| v.as_str()).unwrap_or("");
        Ok(envelope(&[
            ("service", JsonValue::string("video_service_skeleton_probe")),
            ("echo", JsonValue::string(echo)),
        ]))
    }
}

/// 声明表构造子：骨架探针
pub fn mk_skeleton_probe() -> Arc<dyn NativeService> {
    Arc::new(VideoSkeletonProbe)
}

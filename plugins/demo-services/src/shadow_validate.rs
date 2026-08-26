// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `shadow_ik_solver` 原生实现 —— 影子验证（备用算法，返回预设结果）。
//!
//! 与 Python 基线一致：返回结构相同但数值为预设值 + `diff_percent` / `diff_exceeded`
//! （浮点比较外部化，TCB 只做 eq 判断）。max_diff 为字符串 "5.0" 等。

use evorule_tcb::JsonValue;

use crate::{arg_f64, arg_str, obj, NativeService};

/// `shadow_ik_solver` 原生服务
pub struct ShadowValidate;

impl NativeService for ShadowValidate {
    fn execute(&self, args: &JsonValue) -> evorule_reactor::IoResult {
        let solver_type = arg_str(args, "solver_type", "LMA");
        let max_diff = arg_f64(args, "max_diff", 5.0);
        let diff_percent = 1.2;
        let diff_exceeded = diff_percent > max_diff;
        Ok(obj(vec![
            ("converged", JsonValue::Bool(true)),
            ("converged_ok", JsonValue::Bool(true)),
            (
                "joint_positions",
                JsonValue::Array(vec![
                    JsonValue::string("0.4700"),
                    JsonValue::string("-2.2300"),
                    JsonValue::string("4.5200"),
                    JsonValue::string("0.0"),
                    JsonValue::string("0.0"),
                    JsonValue::string("0.0"),
                ]),
            ),
            ("residual", JsonValue::string("0.00051")),
            ("diff_percent", JsonValue::string("1.2")),
            ("diff_exceeded", JsonValue::Bool(diff_exceeded)),
            ("solver", JsonValue::string(format!("shadow-{solver_type}"))),
        ]))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::NativeService;

    #[test]
    fn test_shadow_default_not_exceeded() {
        let svc = ShadowValidate;
        let args = JsonValue::object_from_pairs(&[("max_diff", JsonValue::string("5.0"))]);
        let r = svc.execute(&args).unwrap();
        assert_eq!(r.get("converged_ok").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            r.get("diff_exceeded").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(r.get("solver").and_then(|v| v.as_str()), Some("shadow-LMA"));
    }

    #[test]
    fn test_shadow_exceeded_when_max_diff_small() {
        let svc = ShadowValidate;
        let args = JsonValue::object_from_pairs(&[("max_diff", JsonValue::string("0.5"))]);
        let r = svc.execute(&args).unwrap();
        assert_eq!(r.get("diff_exceeded").and_then(|v| v.as_bool()), Some(true));
    }
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! L2 规则集形态纪律 —— 机器可读版 core_eval
//!
//! # 这一层补的是什么
//!
//! TCB_SPEC.md 规范的是 **TCB 自己的代码怎么写**（T/G/D 系列），`build.rs`
//! 在编译期强制它。但 TCB 的行为还取决于喂给它的 **core_eval 规则集**长什么样
//! ——而这一层历史上完全空白，只能靠 code review。
//!
//! BUG-P0-005 就是这个缺口的产物：TCB 内核行为完全正确，是 L1 `io_request`
//! 规则排在 L2 `enforce` 之前、命中即返回，导致那条 `enforce` **永不求值**
//! （守卫存在但未执行）。
//!
//! 本模块把「规则集的形态纪律」写成 core_eval JSON（`discipline/core_eval.json`），
//! **判定条款在数据里**（可版本化 / 可评审 / 可 diff），本 crate 只承载常量与
//! 一致性校验。
//!
//! # 为什么纪律依赖内核语义（必须同步演进）
//!
//! DC-05（遮蔽检测）的定义直接依赖内核的分层方式：BUG-P0-005 修复后，顶层
//! `enforce` 走「约束前置门」独立求值（**约束不是状态变换**），不再参与序列内
//! 的早退竞争，因此**不是**遮蔽源。内核改了，纪律必须同步改——二者是一份契约。
//!
//! # 使用方
//!
//! `evorule validate`（evorule-cli）消费本常量作为前置门禁：遍历待校验规则集的
//! 每个节点，把内核看不见的「列表级事实」标注进 `instruction._ctx`，再交给
//! `execute_transition` 求值；命中即 `Halted { rule_index, reason }` → 拒载。
//!
//! # 诚实边界
//!
//! 用 TCB 检查喂给 TCB 的规则属于自证，本纪律保证的是**规则层的形态纪律**，
//! 不是 `execute_transition` 自身的语义正确性（后者由 TCB_SPEC 的 T/G/D 系列
//! 与 Kani 承担）。它不能替代形式化验证，也不构成安全边界。

/// 纪律规则集原文（core_eval JSON 数组），编译期嵌入。
///
/// 之所以用 `include_str!` 而非运行时读取：
/// - TCB 是零 I/O 内核，运行时不碰文件系统；
/// - 纪律与内核版本强绑定，随 crate 发布，不存在「数据与代码版本错配」。
///
/// 消费方负责 JSON 解析；本 crate 零依赖，故不在此解析。
pub const DISCIPLINE_CORE_EVAL: &str = include_str!("../discipline/core_eval.json");

#[cfg(test)]
mod tests {
    use super::DISCIPLINE_CORE_EVAL;
    use crate::META_INSTRUCTION_TYPES;
    use alloc::vec::Vec;

    /// 从 JSON 原文提取所有 `"instruction_type": "<v>"` 的值（去重）
    ///
    /// 手工扫描而非解析：本 crate 零依赖（no_std），dev-dependencies 亦无 JSON 库。
    fn declared_instruction_types(json: &str) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for seg in json.split("\"instruction_type\"").skip(1) {
            let tail = seg.trim_start();
            let Some(after_colon) = tail.strip_prefix(':') else {
                continue;
            };
            let Some(inner) = after_colon.trim_start().strip_prefix('"') else {
                continue;
            };
            if let Some((t, _)) = inner.split_once('"') {
                if !out.contains(&t) {
                    out.push(t);
                }
            }
        }
        out
    }

    /// SSOT 漂移防线：纪律里承认的元指令集合必须等于 `META_INSTRUCTION_TYPES`
    ///
    /// 内核新增元指令而纪律未同步 → 合法规则会被误判 DC-01；内核退役元指令而
    /// 纪律未同步 → 非法规则会被放行。**两个方向都必须拦。**
    #[test]
    fn discipline_types_match_tcb_ssot() {
        let mut declared = declared_instruction_types(DISCIPLINE_CORE_EVAL);
        declared.sort_unstable();
        let mut meta: Vec<&str> = META_INSTRUCTION_TYPES.to_vec();
        meta.sort_unstable();
        assert_eq!(
            declared, meta,
            "纪律集承认的元指令必须与 executor.rs META_INSTRUCTION_TYPES 一致（SSOT 漂移）"
        );
    }

    /// 纪律条文数必须等于 reason 数：每条 enforce 都要能给出可审计的违规说明
    #[test]
    fn every_discipline_rule_carries_a_reason() {
        let rules = DISCIPLINE_CORE_EVAL
            .matches("\"type\": \"enforce\"")
            .count();
        let reasons = DISCIPLINE_CORE_EVAL.matches("\"reason\":").count();
        assert!(
            rules > 0,
            "纪律集为空等于门禁失效：至少应有一条 enforce 条款"
        );
        assert_eq!(
            rules, reasons,
            "每条纪律必须是带 reason 的 enforce（DC-03 的自检）"
        );
    }

    /// DC 编号必须连续：编号错乱会让审查者按错误的条款定位问题
    #[test]
    fn discipline_codes_are_contiguous() {
        let n = DISCIPLINE_CORE_EVAL.matches("\"reason\":").count();
        for i in 1..=n {
            let code = alloc::format!("DC-{i:02}");
            assert!(
                DISCIPLINE_CORE_EVAL.contains(code.as_str()),
                "纪律集缺少条款编号 {code}（编号必须 DC-01 起连续）"
            );
        }
    }
}

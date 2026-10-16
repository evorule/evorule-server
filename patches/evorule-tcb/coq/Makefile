# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 EvoRule Project
# This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#
# Coq 形式化验证 Makefile(阶段 2 接口占位)
#
# 阶段 1 仅预留目录与 Makefile,不参与 Rust 编译,不影响 `cargo build`。
# 阶段 2 将在此目录实施 Coq 8 模块(~2700 行)对 evorule-tcb 核心算法的
# 数学形式化证明(见 EVORULE_FORMAL_VERIFICATION_PLAN_v3.md §阶段 2)。
#
# 阶段 2 计划模块:
#   - JsonValue.v       — JsonValue 数据结构的 Coq 建模
#   - PathResolve.v     — resolve_path 路径解析的形式化
#   - DomainEval.v      — evaluate_domain 域条件评估的形式化
#   - Transition.v      — execute_transition 状态转换的形式化
#   - Termination.v     — 终止性证明(MAX_TRANSFORM_RULES 上界)
#   - DepthBound.v      — 递归深度强制(MAX_BRANCH_DEPTH)
#   - Determinism.v     — 确定性证明(相同输入 → 相同输出)
#   - SpecRefinement.v  — Rust 实现与 Coq 规范的细化关系
#
# 使用方式(阶段 2 实施后):
#   make all       — 编译全部 Coq 证明
#   make verify    — 运行证明检查
#   make clean     — 清理编译产物

all:
	@echo "Coq formalization: Phase 2 (not yet implemented)"
	@echo "See EVORULE_FORMAL_VERIFICATION_PLAN_v3.md for the roadmap."

verify:
	@echo "Coq verification: Phase 2 (not yet implemented)"

clean:
	@echo "Nothing to clean (Phase 2 not yet implemented)"

.PHONY: all verify clean

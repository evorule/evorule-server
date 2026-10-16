<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: AGPL-3.0-or-later

  This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
-->

# Kani 形式化验证指南

[evorule-tcb](../) 的 34 个 Kani proof（v0.6.0 随规则清理退役 P15/P16/P17，原 37 个）位于 [`tests/kani/kani_proofs.rs`](../tests/kani/kani_proofs.rs)，
经 [`tests/kani_entry.rs`](../tests/kani_entry.rs) 顶层入口引入，由 `#[cfg(kani)]` 门控
（`cargo kani` 自动注入 `--cfg kani`，普通 `cargo build`/`cargo test` 不编译）。

> **验证状态唯一权威**：[verification/STATUS.md](../../verification/STATUS.md)（五档词汇，见
> [verification/MECHANISM.md](../../verification/MECHANISM.md) M1/M2）。本指南只记录
> proof 清单与运行方式，不承载状态断言。

## 📋 Proof 分档清单（34 个）

### A 档 14 个 —— CI PR 闸门（kani.yml `kani-tcb-a-tier` job）

v0.6.0 重跑实测（2026-09-14，commit `25c0cc0`，Kani 0.67.0 + nightly-2025-11-21，WSL）：14/14 PASS，单 proof 0.3~4.0s。

| 属性 | Proof（函数名） |
| ---- | --------------- |
| P0-3（11 个） | `verify_resolve_path_simple_field` / `_nested_dot` / `_array_index` / `_double_dot` / `_escaped_dot` / `_empty_returns_none` / `_trailing_dot` / `_invalid_index_char` / `_missing_close_bracket` / `_deterministic` / `verify_array_index_bounds` |
| P0-6（3 个） | `verify_partial_eq_never_panics` / `verify_ord_never_panics` / `verify_as_methods_never_panic` |

### B 档 20 个 —— 仅手动触发且允许失败（kani.yml `kani-tcb-b-tier` job）

实测判定「当前不可运行」（2026-09-11，Kani 0.67.0 / WSL Ubuntu 22.04；下列为退役前 23 个时点的历史记录）：600s 全超时
（其中 2 个实际跑 910s+）；3600s 仍超时（`verify_merge_safe` 跑满 3603s）；unwind 4/8 无改善。
覆盖 evaluate_domain 系列（8 个）、execute_transition / enforce 系列、has_fields 等
（原 collect/merge/substitute_template 相关 3 个 proof 已随 v0.6.0 退役删除），
完整名单见 [STATUS.md](../../verification/STATUS.md) 附录 B。

⚠️ 不要为 B 档加大超时——至今无证据表明它们会终止。若将来要让 B 档可验证，
正确做法是缩小符号输入规模，不是加大超时。B 档属性由 proptest 兜底覆盖
（[`tests/proptest_props.rs`](../tests/proptest_props.rs) /
[`tests/determinism_proptest.rs`](../tests/determinism_proptest.rs)）。

### 历史注记

- **FixedMap 时代（已废弃）**：早期方案曾用固定容量 `FixedMap` 抽象 + 12 个 proof
  （旧 `verification/kani_proofs.rs`，2026-07-27 口径 9 PASS + 3 TIMEOUT），因 CBMC 对嵌套
  FixedMap 状态爆炸而弃用，改用「直接验证生产代码 + 结构化符号输入」
  （[plan v3 §1.2](../../verification/plan/EVORULE_FORMAL_VERIFICATION_PLAN_v3.md)）。
- **旧证据隔离**：v0.3.1 时代旧证据已隔离至
  [`verification/evidence/kani/_invalidated/`](../verification/evidence/kani/_invalidated/)，
  详见该目录 README。
- **专项设计稿**：[`verification/kani-formal-verification-design.md`](../verification/kani-formal-verification-design.md)
  （其中旧 P1–P21 编号已作废，与属性号的映射见 [STATUS.md](../../verification/STATUS.md) 附录 A）。

## 🛠️ 安装

### Linux / macOS

```bash
cargo install --locked kani-verifier --version 0.67.0
cargo-kani setup
```

### Windows (WSL Ubuntu 22.04 推荐)

```bash
# 1. 启用 WSL (PowerShell admin):
wsl --install -d Ubuntu-22.04

# 2. 在 WSL 内:
cargo install --locked kani-verifier --version 0.67.0
cargo-kani setup

# 3. 验证:
cargo kani --version
```

### Windows (Docker)

```bash
docker run --rm -v ${PWD}/evorule-tcb:/workspace -w /workspace   model-checking/kani:latest cargo kani
```

## 🚀 运行

⚠️ **TCB proof 位于 tests/ 下，必须加 `--tests`**（经 `tests/kani_entry.rs` 顶层入口；
不加则 Checking harness 计数为 0，静默全绿）：

```bash
# A 档单个 proof（PR 闸门同款命令）
cargo kani -p evorule-tcb --tests --harness verify_resolve_path_simple_field --output-format=terse

# 全部 37 个（含 B 档 23 个，预计大量超时，不建议）
cargo kani -p evorule-tcb --tests --output-format=terse
```

实跑证据的收集方式、命名规范与归档位置见
[verification/evidence/README.md](../verification/evidence/README.md)。

> 根目录 `scripts/run-kani.sh` wrapper 当前仅覆盖 evorule-reactor；TCB 请用上述命令。

## 🔧 故障排查

| 症状                                   | 原因                          | 修复                                                |
| -------------------------------------- | ----------------------------- | --------------------------------------------------- |
| `kani: command not found`              | 未安装                        | `cargo install kani-verifier --version 0.67.0`     |
| `cargo-kani: command not found`        | PATH 缺 `~/.cargo/bin`        | `export PATH="$HOME/.cargo/bin:$PATH"`              |
| Checking harness 计数为 0              | TCB proof 在 tests/ 下        | 必须加 `--tests`                                    |
| B 档 proof 超时                        | 符号执行状态爆炸（实测 600s/3600s 均超时） | 判定不可运行；如需推进请缩小符号输入规模（非加大超时） |
| `CBMC out of memory`                   | 单 proof 状态爆炸             | 加 `--output-format=terse` 或拆分 proof             |
| `error[E0432]: unresolved import kani` | 未启用 kani feature           | 不需要 feature — `cargo kani` 自动注入 `--cfg kani` |
| Windows native 失败                    | Kani 不支持 Windows           | 用 WSL 或 Docker                                    |

## 📊 CI

[`.github/workflows/kani.yml`](../../.github/workflows/kani.yml)：

- `kani-tcb-a-tier`：push/PR 触发，A 档 14 个 proof 串行，单 proof 300s 上限，job 30min
- `kani-tcb-b-tier`：仅 `workflow_dispatch` 手动触发，`continue-on-error` 允许失败（180min）
- 同文件 `kani-reactor` job：reactor 3 个 CI proof（见 [reactor Kani 指南](../../evorule-reactor/docs/KANI.md)）

## 📖 延伸阅读

- [Kani 官方文档](https://model-checking.github.io/kani/)
- [verification/STATUS.md](../../verification/STATUS.md) — 验证状态唯一权威（含 37-proof 分档清单附录 B、reactor 11-proof 清单附录 C）
- [verification/MECHANISM.md](../../verification/MECHANISM.md) — 验证机制（状态/证据/披露规则 M1–M11）
- [evorule-reactor Kani 指南](../../evorule-reactor/docs/KANI.md)
- [TCB_SPEC.md 形式化验证章节](../TCB_SPEC.md)
- [tests/proptest_props.rs](../tests/proptest_props.rs) / [tests/determinism_proptest.rs](../tests/determinism_proptest.rs) — B 档属性的 proptest 兜底覆盖

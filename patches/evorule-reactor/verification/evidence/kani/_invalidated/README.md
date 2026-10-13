<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: AGPL-3.0-or-later

  This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
-->

# 已失效证据隔离区（_invalidated）

> **性质**：本目录存放失效/被替代的验证证据（[MECHANISM.md](../../../../../verification/MECHANISM.md) M3.5）。
> **规则**：证据失效不物理删除，`git mv` 移入本目录，git 历史可溯原始位置与内容（`git log --follow`）；本 README 记录每批隔离的作废原因与隔离日期。

## 批次 1（2026-09-14）：reactor 旧 4 对证据随规则清理失效隔离

**来源**：`../`（`evorule-reactor/verification/evidence/kani/`），8 个文件（4 对 `.log` + `.stdout.txt`，命名锚定 `03643aa` / `bdfb8d4`，产出于 2026-09-12）。

**失效判定**（[MECHANISM.md](../../../../../verification/MECHANISM.md) M3.4）：规则清理（CR-20260914-001，提交 `10c743d` + `25c0cc0`）变更了被验证依赖的 TCB 生产代码（collect/merge 元指令退役，v0.6.0 破坏性变更）。reactor 4 个 CI proof 的源码本身未变更，但证据 SHA 锚定的被验证代码环境已失效，谨慎起见复跑替代并隔离旧证据。

**逐文件清单**（适用共同判定，无逐文件特有问题）：

| 文件对 | 归档时结果 |
| ------ | ---------- |
| P0-11.invariant_cause_queue_sync_PASS_03643aa_20260912_225316 | PASS（1.60s，647 断言 0 失败，CI 同参数） |
| P1-3.invariant_version_monotonic_PASS_bdfb8d4_20260912_145126 | PASS |
| P1-5.command_does_not_decrease_queue_PASS_03643aa_20260912_225328 | PASS（`--default-unwind 4`，0.86s） |
| P1-6.max_rounds_termination_PASS_bdfb8d4_20260912_145136 | PASS |

**替代证据**：于 `25c0cc0` 复跑 4 个 CI proof 全 PASS（2026-09-14，WSL Kani 0.67.0），按 M3.1 命名归档于 `../`（见 [STATUS.md](../../../../../verification/STATUS.md) P0-11/P1-3/P1-5/P1-6 证据列）。

**说明**：P0-11 修复前 3 份超时 FAIL 过程证据（`FAIL_bdfb8d4_20260912_*`）按 M3.2 合规存档，保留于 `../` 主目录，不属于本隔离区范围。

**披露记录**：见 [DISCLOSURE_LOG.md](../../../../../verification/DISCLOSURE_LOG.md) 2026-09-14「规则清理」条目（隔离执行）与 2026-09-15「历史遗留项门禁整改」条目（本 README 补建）。

## 批次 2（2026-09-15）：reactor 4 对证据随 lint 清零批次失效隔离

**来源**：`../`（`evorule-reactor/verification/evidence/kani/`），8 个文件（4 对 `.log` + `.stdout.txt`，命名锚定 `25c0cc0`，产出于 2026-09-14）。

**失效判定**（[MECHANISM.md](../../../../../verification/MECHANISM.md) M3.4）：主仓提交 `35b6182` 变更了被验证依赖（reactor `src/pure.rs` 测试模块 import 排序——零语义，监督核查 R03 diff 级核验；TCB `src/value.rs`/`src/executor.rs` rustfmt 与 lint 豁免标注——零语义）。按 M3.4「证据 SHA 绑定早于被验证代码或依赖最后一次变更即失效」，随主仓同批 A 档证据一并重置。

**逐文件清单**（适用共同判定，无逐文件特有问题）：

| 文件对 | 归档时结果 |
| ------ | ---------- |
| P0-11.invariant_cause_queue_sync_PASS_25c0cc0_20260914_190211 | PASS |
| P1-3.invariant_version_monotonic_PASS_25c0cc0_20260914_190211 | PASS |
| P1-5.command_does_not_decrease_queue_PASS_25c0cc0_20260914_190211 | PASS（`--default-unwind 4`） |
| P1-6.max_rounds_termination_PASS_25c0cc0_20260914_190211 | PASS（`--default-unwind 4`） |

**替代证据**：于 `34c841d` 重跑 4 个 CI proof 全 PASS（2026-09-15，WSL Kani 0.67.0；合计 18/18 = TCB 14 + reactor 4），按 M3.1 命名归档于 `../`（见 [STATUS.md](../../../../../verification/STATUS.md) P0-11/P1-3/P1-5/P1-6 证据列）。

**说明**：P0-11 修复前 3 份超时 FAIL 过程证据（`FAIL_bdfb8d4_20260912_*`）按 M3.2 合规存档，保留于 `../` 主目录，不属于本隔离区范围。

**披露记录**：见 [DISCLOSURE_LOG.md](../../../../../verification/DISCLOSURE_LOG.md) 2026-09-15 lint 清零条目。

## 批次 3（2026-09-18）：reactor 4 对证据随仓库提交历史整理失效隔离

**来源**：`../`（`evorule-reactor/verification/evidence/kani/`），8 个文件（4 对 `.log` + `.stdout.txt`，命名锚定 `34c841d`，产出于 2026-09-15）。

**失效判定**（[MECHANISM.md](../../../../../verification/MECHANISM.md) M3.4）：主仓 2026-09-18 仓库提交历史整理（公开历史 commit message 规范化清洗，全链 commit 重写、版本 tag 重打）致证据 SHA 锚定全部失效（旧 hash 不在新历史中）；树内容与整理前末端 `34c841d` 零变更，proof 源码文件本身未变更。按 M3.4 复跑替代（拒绝修改既有证据内容），旧证据随批隔离。

**逐文件清单**（适用共同判定，无逐文件特有问题）：

| 文件对 | 归档时结果 |
| ------ | ---------- |
| P0-11.invariant_cause_queue_sync_PASS_34c841d_20260915_202409 | PASS |
| P1-3.invariant_version_monotonic_PASS_34c841d_20260915_202409 | PASS |
| P1-5.command_does_not_decrease_queue_PASS_34c841d_20260915_202409 | PASS（`--default-unwind 4`） |
| P1-6.max_rounds_termination_PASS_34c841d_20260915_202409 | PASS（`--default-unwind 4`） |

**替代证据**：于 `a3d728f` 重跑 4 个 CI proof 全 PASS（2026-09-18，WSL Kani 0.67.0；合计 18/18 = TCB 14 + reactor 4），按 M3.1 命名归档于 `../`（见 [STATUS.md](../../../../../verification/STATUS.md) P0-11/P1-3/P1-5/P1-6 证据列）。复跑前置修复：`src/invariants.rs` 测试辅助 `set_io_result` 做了 cfg 兼容适配——原 `BTreeMap::entry()` 在 kani cfg 下 Object 的替身实现（有序 Vec 模型）上无此 API（E0599），改用 `contains_key` + `get_mut`/`insert` 组合使两种 cfg 下 API 统一；该修复属 `#[cfg(test)]` 测试辅助代码，不触及本批 proof 源码（`verification/kani_proofs.rs`），不影响证据锚定。

**同批其他处置**：复跑首轮产物中 4 个文件（文件名 `*_PASS_a3d728f_20260918_105558`，实为上述 E0599 编译失败输出）因证据生成脚本文件名硬编码 `_PASS_` 而误名（文件名 PASS、内容 FAIL），确认零证据价值（编译失败过程，非属性反例）后直接删除，不适用隔离；脚本缺陷同批修复（stdout 配对文件落盘、命名按实测状态生成）。

**披露记录**：见 [DISCLOSURE_LOG.md](../../../../../verification/DISCLOSURE_LOG.md) 2026-09-18 仓库提交历史整理与证据基线重置条目。

## 批次 4（2026-09-24）：4 个 `.FAIL.raw` 环境故障残留隔离

**来源**：`../`（`evorule-reactor/verification/evidence/kani/`），4 个 `.raw` 文件（2026-09-24 本地 Kani 批验尝试的原始输出，未入库）。

**失效判定**（[MECHANISM.md](../../../../../verification/MECHANISM.md) M3）：`.raw` 命名不符合 M3.1 四要素规范，且内容为 WSL rustup 工具链安装故障原始输出（`os error 39: Directory not empty`）——**验证未实际执行**，无验证结论（M3.2 不满足）。涉及 4 个 CI proofs（P0-11/P1-3/P1-5/P1-6）均有现行有效替代证据（批次 3 于 `a3d728f` 复跑 4/4 PASS，见 [STATUS.md](../../../../../verification/STATUS.md)）。

**处置**：移入本隔离区（未入库文件，直接 `Move-Item`，无 git 历史锚定）；故障根因（rustup 工具链目录残留冲突）留待验证环境专项处理。

**披露记录**：本次为提交前置门禁（check_status_sync S4）整改，随 evorule-discipline crate 抽取批次提交。

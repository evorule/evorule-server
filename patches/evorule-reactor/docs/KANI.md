<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: AGPL-3.0-or-later

  This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
-->

# Kani 形式化验证指南

[evorule-reactor](../) 的 11 个 Kani proof 函数位于 [`verification/kani_proofs.rs`](../verification/kani_proofs.rs)，
由 `#[cfg(kani)]` 门控（`cargo kani` 自动注入 `--cfg kani`）。

## 📋 Proof 清单

### CI 子集（4 个，kani.yml `kani-reactor` job，单 proof 300s；version_monotonic / max_rounds_termination / command_does_not_decrease_queue 带 `--default-unwind 4`，P0-11 不带——该配置实测对 P0-11 无效）

| #   | Proof                         | 验证目标                                         | 状态 |
| --- | ----------------------------- | ------------------------------------------------ | ---- |
| 2   | `invariant_version_monotonic` | version 单调递增，bump_version 后 > prev_version | 📊   |
| 4   | `command_does_not_decrease_queue` | apply_command 后队列长度严格 +1                  | 📊   |
| 5   | `max_rounds_termination`      | is_stable 终止条件正确性 + 有界循环终止          | 📊   |
| 6   | `invariant_cause_queue_sync` | instruction_causes.len() == queue.len() 同步    | 📊   |

> 状态一律见 [verification/STATUS.md](../../verification/STATUS.md)（唯一权威）。

### 完整验证（7 个，未入 CI）

| #   | Proof                                               | 验证目标                              | 状态 |
| --- | --------------------------------------------------- | ------------------------------------- | ---- |
| 1a  | `invariant_io_count_register_complete`              | register/complete 保持 4 字段长度相等 | 📊   |
| 1b  | `invariant_io_count_force_remove`                   | force_remove 保持 4 字段长度相等      | 📊   |
| 3   | `invariant_io_recovery_iff_result`                  | io_recovery ⇔ payload 含 io_result    | 📊   |
| 7   | `proof_fact_log_append_monotonic`                   | FactsLog append 版本单调 + 历史增长   | 📊   |
| 8   | `proof_hash_chain_back_link`                        | 哈希链 back-link 正确性               | 📊   |
| 9   | `proof_reactor_invariants_preserved_after_pure_ops` | 多次操作后所有不变量同时成立          | 📊   |
| 10  | `proof_phase_state_machine_cannot_jump`             | Phase 状态机转移正确,不跳跃           | 📊   |

> **状态**：一律见 [verification/STATUS.md](../../verification/STATUS.md)（唯一权威，五档词汇）。
> 原实测口径（2026-07-27 旧版本代码：10/11 PASS + 1/11 TIMEOUT）为历史记录，其后 proof 源码与被验证代码均有变更。
> 逻辑正确性另由单元测试覆盖。

## 🧪 Proof 详细说明

### Proof 1a/1b: I/O 计数一致性

纯函数管理的 4 个 I/O 字段必须保持长度一致：

```text
pending_io_count == pending_requests.len()
pending_io_count == pending_io_types.len()
pending_io_count == pending_io_instructions.len()
```

**设计权衡**：使用固定 `FactId(1)`/`FactId(2)` 而非 `kani::any()`，避免 `BTreeSet<u64>`
任意 key 导致 BTreeMap 内部红黑树状态爆炸。拆分为 1a/1b 两个独立 proof，
每个 BTreeSet 最多 1-2 个元素。任意 id 的保底由 proptest 提供。

### Proof 2: `invariant_version_monotonic`

`version >= prev_version`，且 `bump_version` 后 `version > prev_version`。
对**任意** u64 初始值保持单调性（`kani::any()`）。
不涉及 BTreeSet/BTreeMap，CBMC 状态空间极小。

### Proof 3: `invariant_io_recovery_iff_result`

`io_recovery == true` 当且仅当 `payload.__io_result__` 存在。
使用 `register_io_request_pure`（始终缓存指令），确保 `take_io_instruction` 返回 `Some`。

### Proof 4: `command_does_not_decrease_queue`

`apply_command` 后 `queue.len() == old_len + 1`（严格递增）。
2026-09-12 修复超时（与 P0-11 同根因：proof 末尾 state 正常 Drop 触发 JsonValue 符号化变体的 BTreeMap 析构 unwind 爆炸；修复 = `forget(state)`），本地实测 0.57s PASS，入 CI 闸门（`--default-unwind 4` 实测通过）。

### Proof 5: `max_rounds_termination`

反应器主循环在 `max_rounds` 步内必然终止。
终止条件：`is_stable(queue空, 无pending I/O, steps > 0)`。
对任意输入（`kani::any()`），返回值与终止条件一致。

### Proof 6: `invariant_cause_queue_sync`（P0-11）

`instruction_causes.len() == queue.len()` — cause 队列与 instruction 队列同步。
使用 `JsonValue::Null`（无堆分配）避免 CBMC 状态爆炸。
`kani::any()` 用于 FactId,验证任意 cause 值下不变量保持。

> **当前状态（2026-09-12）**：超时根因已修复并重入 CI 闸门——CBMC 将 VecDeque 堆缓冲区中的 `JsonValue` 按任意变体建模，任何触发 `JsonValue` Drop 的路径（pop 返回值 / clear 的 `drop_in_place` / state 整体 Drop）都会展开 `Object(BTreeMap)` 红黑树析构的无界 unwind。修复 = proof 侧 `forget(popped)`/`forget(state)` + `clear_queue` 的 `#[cfg(kani)]` take+forget 分支（[`src/state.rs`](../src/state.rs)）。本地实测 1.53s PASS（CI 中不带 `--default-unwind`）。修复前 3 份超时 FAIL 证据保留于 [`verification/evidence/kani/`](../verification/evidence/kani/)，状态见 [verification/STATUS.md](../../verification/STATUS.md)。

### Proof 7: `proof_fact_log_append_monotonic`

FactsLog append 操作保持 version 单调递增和 history_len 严格增长。
使用 `--unwind 6`（3 次 append 的 Vec push 最多 6 次展开）。
固定 Fact + `JsonValue::Null` 最小化序列化状态空间。

### Proof 8: `proof_hash_chain_back_link`

哈希链 `chain_step(prev_hash, fact_hash)` 的 back-link 正确性。
Kani 模式下自动切换为简化哈希（不用 `format!`/`blake3`），避免状态爆炸。
N=3 固定 Fact，足以覆盖归纳步骤。

### Proof 9: `proof_reactor_invariants_preserved_after_pure_ops`

验证**多次操作的组合性**——操作序列（cmd → bump → cmd → bump → pop）后
所有不变量（version 单调、queue 长度、io_recovery、pending_io_count）同时成立。

### Proof 10: `proof_phase_state_machine_cannot_jump`

Phase 状态机转移正确性：不能跳过阶段。
使用 `kani::assume(pending_io <= 3)` 和 `kani::assume(steps <= 3)` 限界。
状态空间: 6 × 2 × 4 × 4 × 2 = 384 种组合，Kani 瞬间完成。

## 🛠️ 安装

### Linux / WSL Ubuntu 22.04

```bash
cargo install --locked kani-verifier --version 0.67.0
cargo-kani setup
cargo kani --version
```

### 验证 evorule-reactor

```bash
# 从 workspace 根目录
cd /path/to/evorule

# 跑全部 proof（11 个）
cargo kani -p evorule-reactor --output-format=terse

# 跑单个 proof
cargo kani -p evorule-reactor --harness invariant_version_monotonic --output-format=terse
cargo kani -p evorule-reactor --harness max_rounds_termination --output-format=terse
cargo kani -p evorule-reactor --harness invariant_cause_queue_sync --output-format=terse
cargo kani -p evorule-reactor --harness proof_fact_log_append_monotonic --output-format=terse
cargo kani -p evorule-reactor --harness proof_hash_chain_back_link --output-format=terse
cargo kani -p evorule-reactor --harness proof_phase_state_machine_cannot_jump --output-format=terse
```

> **注意**：evorule-reactor 依赖 tokio，Kani 编译 tokio 依赖可能需要较长时间（首次 5-15 分钟）。
> 后续增量编译会快很多。proof 函数本身是同步的，不调用 tokio。

### 使用项目 wrapper（跨平台,支持双 crate）

```bash
./scripts/run-kani.sh --crate evorule-reactor                          # 跑全部
./scripts/run-kani.sh --crate evorule-reactor --list                   # 列出 proof
./scripts/run-kani.sh --crate evorule-reactor --harness max_rounds_termination
```

## 🔧 故障排查

| 症状                              | 原因                                 | 修复                                                                   |
| --------------------------------- | ------------------------------------ | ---------------------------------------------------------------------- |
| `CBMC out of memory`              | BTreeSet/VecDeque 状态爆炸           | 加 `--default-unwind 200` 或拆分 proof                                 |
| `unresolved import kani`          | 未用 `cargo kani`（缺 `--cfg kani`） | 用 `cargo kani -p evorule-reactor`，不是 `cargo build --features kani` |
| 编译 tokio 超时                   | tokio 依赖大                         | 首次编译慢属正常，缓存后增量快                                         |
| `error[E0432]: unresolved import` | Kani 版本不匹配                      | 用 0.67.0                                                              |

## 🗂️ 超时产物收集与分析

当 proof 因 CBMC 状态爆炸超时时，使用 [`collect_kani_artifacts.sh`](../collect_kani_artifacts.sh)
自动收集所有中间产物，便于定位超时原因。

### 常用命令

```bash
cd /path/to/evorule

# 列出所有可用 proof
./evorule-reactor/collect_kani_artifacts.sh --list

# 收集单个 proof 的产物（300s 超时）
./evorule-reactor/collect_kani_artifacts.sh \
  --harness invariant_io_count_register_complete \
  --timeout 300

# 收集所有 proof 的产物（600s 超时）
./evorule-reactor/collect_kani_artifacts.sh --all --timeout 600

# 分析已收集的产物（生成状态爆炸诊断报告）
./evorule-reactor/collect_kani_artifacts.sh \
  --analyze ./kani-artifacts/invariant_io_count_register_complete
```

### 状态爆炸类型诊断

`--analyze` 模式会自动识别超时类型并给出对策：

| 类型         | 症状                       | 对策                                                             |
| ------------ | -------------------------- | ---------------------------------------------------------------- |
| 内存溢出型   | CBMC 直接 OOM 退出         | 减少 BTreeSet/BTreeMap 元素数、拆分 proof、调 `--default-unwind` |
| 循环展开型   | 卡在 unwinding loop 阶段   | 用 `--default-unwind` 限制展开深度、`kani::assume` 限制循环次数  |
| SAT 求解器型 | 命题公式过大，求解耗时过长 | 换求解器（cadical/minisat）、减少变量数、拆分 proof              |

## 📊 CI

Kani 验证由 [`.github/workflows/kani.yml`](../../.github/workflows/kani.yml) 承载（非 ci.yml），在以下情况触发：

- push 到 main 或 PR，且修改 `evorule-tcb/**`、`evorule-reactor/src/**`、`evorule-reactor/verification/**` 等 Kani 相关路径
- 手动触发 (`workflow_dispatch`，可选 a / b / all 档位)

`kani-reactor` job：30 min 超时，单 proof 300s 上限，**4 个 CI proof**（P0-11 `invariant_cause_queue_sync` 2026-09-12 修复超时根因后重入，单独跑且不带 `--default-unwind`——该配置实测对 P0-11 无效；P1-5 `command_does_not_decrease_queue` 同日同根因修复入闸，与 version_monotonic / max_rounds_termination 同组带 `--default-unwind 4`；不可加 `--tests`，proof 经 `src/pure.rs` `#[path]` 引入 lib）。

> reactor 其余 7 个 proof 未入 CI（涉及堆分配数据结构或实测超时，跨 Kani 版本稳定性未知），状态一律见 [verification/STATUS.md](../../verification/STATUS.md)。同文件另含 TCB A/B 档两个 job（见 [tcb KANI 指南](../../evorule-tcb/docs/KANI.md)）。

## 📖 延伸阅读

- [Kani 官方文档](https://model-checking.github.io/kani/)
- [evorule-tcb Kani 验证设计](../../evorule-tcb/verification/kani-formal-verification-design.md)
- [verification/kani_proofs.rs](../verification/kani_proofs.rs) — proof 源码
- [verification/differential_test.rs](../verification/differential_test.rs) — 差分测试

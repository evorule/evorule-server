<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: AGPL-3.0-or-later

  This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
-->

# evorule-reactor

> EvoRule 三层架构的 Tier 1 反应式执行器 —— 事实驱动的状态转换引擎。

- **版本**:v0.6.0
- **依赖**:evorule-tcb = "0.6.0"(io_request 单轮触发/消费;多轮编排由应用层负责)
- **协议**:AGPL-3.0-or-later
- **测试**:`cargo test` 全量 PASS / 0 failed（2026-09-14 v0.6.0 全量回归，CI 常驻）
- **build.rs 编译时门禁**:15 模式(G8 控制流 `conditional`/`while_loop`/`sequence` + F11 `unwrap`/`expect`/`panic!`/`debug_assert!` + S5.2 业务术语 7 条 + T10 `unsafe`,文件级豁免 `ffi.rs`/`facts_log.rs`),非测试代码强制,PASSED
- **G8 门控遵守**:反应器主循环(`reactor.rs`)的控制流分支是**策略数据**(Fact 变体的 match)而非**硬编码业务逻辑**;任何业务分支均由 `core_eval.json` 数据驱动,编译期通过 `build.rs` 递归扫描确认。
- **`unsafe`**:`#![deny(unsafe_code)]`(`ffi.rs` 在 `ffi` feature 下局部 `#[allow(unsafe_code)]`,FFI 边界,已文档化)
- **P0 修复(2026-07-25)**:`Box::leak` 内存泄漏已修复(`IoType::parse` 返回 `Option`);锁中毒改为 `e.into_inner()` 恢复(非 panic)
- **v0.2.0 重构(2026-08-04)**:`IoType` 内部从 `&'static str` 改为 `Arc<str>`,支持 `IoType::new()` 注册任意 io_type(失去 `Copy`,5 个 `const` 改工厂函数);`IoHandler`/`IoDispatcher` 从 governance 下沉至本 crate(trait 改 `#[async_trait]` object-safe);`IoType::parse` 标记 `#[deprecated]`
- **v0.3.1 TCB 升级**:依赖 evorule-tcb 升至 0.3.1,ReAct 循环由 `call_external`/`call_service`/`collect`/`merge` 驱动(迭代上限 10);I/O 结果按 `__io_results__.{io_type}` 类型隔离,消费后以 null 清除
- **v0.6.0 收窄(2026-09-14,规则清理)**:依赖 evorule-tcb 升至 0.6.0;`collect`/`merge` 元指令退役,`io_loop_rules` 改单轮——ReAct 多轮编排归应用层,`io_request` 保留单轮触发/消费语义
- **Kani 形式化验证**:11 个 `#[kani::proof]`(`verification/kani_proofs.rs`),其中 4 个入 CI PR 闸门(根仓 `.github/workflows/kani.yml`,v0.6.0 重跑 4/4 PASS);状态唯一权威见根 [`verification/STATUS.md`](../verification/STATUS.md)

> 本 crate 属于 [EvoRule](https://gitee.com/evorule) 生态:[主仓](https://gitee.com/evorule/evorule) ｜ [在线控制台 Demo](https://evorule.github.io/evorule-console-cloud/) ｜ [evorule-server（应用层）](https://gitee.com/evorule/evorule-server)

## 设计原则

- **事实驱动**：所有交互通过 Fact 通道进行
- **单一串行通道**：保证调度确定性
- **稳定检测**：队列空 + 无待处理 I/O = 稳定
- **无状态泄漏**：所有状态由反应器维护
- **Append-Only 审计链**：所有 Fact 追加到 FactsLog，支持审计重放
- **哈希链完整性**：每次 `append` 自动计算 BLAKE3 哈希链（`content_hash`/`prev_hash`/`chain_hash`），篡改可检测；纯内存模式同样维护（`last_hash` 始终存在，不依赖 persistence feature）

## 架构

```text
项目方/治理层 → FactSender → [command mpsc] → 反应器
                                              ↓
                                        调用 TCB 核心
                                              ↓
                              产生新 Fact → event broadcast → 项目方/I/O 订阅者/审计器
                                              ↓
                              所有 Fact → FactsLog（审计链 + 哈希链）
```

## 模块结构

```text
src/
├── channel.rs          # 通道封装（command/event）
├── error.rs            # 错误类型（ReactorError / FactsLogError 等）
├── fact.rs             # Fact 枚举（7 种变体）+ FactId + IoType
├── facts_log.rs        # Append-Only FactsLog（内存 + WAL 持久化 + 哈希链）
├── ffi.rs              # C FFI 接口（feature = "ffi"）
├── hash.rs             # BLAKE3 哈希链算法（单一真相源，tier2/CLI re-export）
├── invariants.rs       # 结构不变量检查
├── io_dispatcher.rs    # I/O 分发器（v0.2.0 从 governance 下沉；builder + register(IoType, handler)）
├── io_handler.rs       # IoHandler trait + IoResult（v0.2.0 从 governance 下沉；#[async_trait] object-safe）
├── lib.rs              # 模块声明 + 公共 API 导出
├── phase.rs            # 反应器阶段（Executing/Idle/Stable 等）
├── pure.rs             # 纯函数执行器（TCB 调用封装）
├── reactor.rs          # 反应器主循环 + ReactorBuilder + ReactorHandle
├── stable_detector.rs  # 稳定状态检测器
├── state.rs            # 反应器状态快照
└── wal.rs              # WAL 读写（JSONL 格式，含哈希字段）[feature = "persistence"]

tests/
├── integration_test.rs # 集成测试（28 用例：I/O 双路径 / 类型隔离 / 审计重放）
└── complex_rule_test.rs # 复杂规则集成测试（2 用例：normal/vip 订单）

verification/
└── differential_test.rs # 差分验证（11 用例：reactor 运行时 vs execute_transition 纯函数）
```

> **注意**：`wal.rs` 仅在启用 `persistence` feature 时编译（见 [lib.rs](src/lib.rs) `#[cfg(feature = "persistence")]`）。不启用该 feature 时,`FactsLog` 仍以纯内存模式运行并维护哈希链。

## 快速入门

```rust
use evorule_reactor::{Reactor, Fact, FactId};
use evorule_tcb::JsonValue;

let core_eval = vec![];
let reactor = Reactor::builder(core_eval).max_rounds(1000).build();
let (tx, mut rx, _event_tx, _handle, _facts_log) = reactor.spawn();

tx.send(Fact::Command {
    id: FactId(1),
    instruction: JsonValue::object_from_pairs(&[
        ("type", JsonValue::string("increment")),
        ("params", JsonValue::object_from_pairs(&[
            ("attr", JsonValue::string("x")),
            ("delta", JsonValue::Integer(5)),
        ])),
    ]),
}).unwrap();

while let Ok(fact) = rx.recv().await {
    if let Fact::Stable { version, .. } = fact {
        println!("会话于版本 {} 稳定", version);
        break;
    }
}
```

## API 列表

### Reactor

| 方法                          | 说明                       |
| ----------------------------- | -------------------------- |
| `Reactor::builder(core_eval)` | 创建反应器构建器           |
| `builder.max_rounds(n)`       | 设置最大执行轮数           |
| `builder.build()`             | 构建反应器实例             |
| `reactor.spawn()`             | 启动反应器，返回通道和句柄 |

### ReactorHandle

| 方法                                       | 说明                   |
| ------------------------------------------ | ---------------------- |
| `handle.join()`                            | 等待反应器完成         |
| `handle.abort()`                           | 中止反应器             |
| `handle.is_finished()`                     | 查询反应器是否已结束   |
| `handle.current_phase()`                   | 获取当前阶段           |
| `handle.causal_depth()`                    | 获取因果深度           |
| `handle.structural_invariant_violations()` | 获取结构不变量违规计数 |
| `handle.pending_io_count()`                | 获取待处理 I/O 数量    |
| `handle.current_step()`                    | 获取当前执行步数       |
| `handle.snapshot()`                        | 获取反应器状态快照     |
| `handle.interrupt()`                       | 中断执行               |

### Fact 类型

| 类型                    | 说明                                          |
| ----------------------- | --------------------------------------------- |
| `Fact::Command`         | 项目方命令（触发执行）                        |
| `Fact::StateTransition` | 状态转换（反应器自动产生，携带 cause 因果链） |
| `Fact::Stable`          | 稳定状态（队列空 + 无 pending I/O）           |
| `Fact::Error`           | 错误（超时 / TCB 错误 / 队列溢出）            |
| `Fact::IoRequest`       | I/O 请求（TCB 产生，治理层消费）              |
| `Fact::IoResponse`      | I/O 响应（治理层产生，反应器消费）            |
| `Fact::PayloadUpdate`   | 负载更新（治理层注入）                        |

## 审计链与哈希链

### WAL 格式（含哈希链）

每条 WAL 记录包含以下字段：

```json
{
  "version_before": 0,
  "fact": {"type": "Command", "id": 1, "instruction": {...}},
  "content_hash": "blake3(fact_to_stable_json(fact))",
  "prev_hash": "前一条的 chain_hash（首条为 \"genesis\"）",
  "chain_hash": "blake3(prev_hash + content_hash)"
}
```

### 哈希链算法

- **单一真相源**：`evorule_reactor::hash` 模块
- **算法**：BLAKE3
- **content_hash** = `blake3(fact_to_stable_json(fact))`
- **chain_hash** = `blake3(prev_hash + content_hash)`
- **初始 prev_hash** = `"genesis"`
- evorule-governance 和 evorule-cli 通过 re-export 使用同一算法，三方哈希值字节级一致

### FactsLog

| 方法                       | 说明                                 |
| -------------------------- | ------------------------------------ |
| `FactsLog::new()`          | 创建纯内存 FactsLog                  |
| `FactsLog::with_initial_payload(p)` | 创建带初始 payload 的纯内存 FactsLog |
| `FactsLog::with_wal(path)` | 创建带 WAL 持久化的 FactsLog         |
| `FactsLog::recover(path)`  | 从 WAL 恢复 FactsLog（重放 + 挂载 + 恢复哈希链尾）  |
| `facts_log.append(fact)`   | 追加 Fact（自动计算哈希链 + 写 WAL） |
| `facts_log.last_hash()`    | 获取审计链末尾哈希                   |
| `facts_log.history()`      | 获取全部 Fact 历史                   |
| `facts_log.history_with_versions()` | 获取 Fact 历史（携带版本号）   |
| `facts_log.facts_by_path_prefix(prefix)` | 按路径前缀加速查询（A3 路径索引） |
| `facts_log.version()`      | 获取当前版本号                       |
| `facts_log.last_stable_version()` | 获取最后稳定版本号               |
| `facts_log.read_from(v)`   | 从指定版本起重放（审计重放）         |
| `facts_log.compact()`      | 压缩历史，返回压缩比例               |

## C FFI 接口

启用 `ffi` feature 编译动态链接库：

```bash
cargo build --features ffi --release
```

### C API 示例

```c
#include "include/evorule.h"

evorule_reactor* reactor = evorule_reactor_new();
evorule_reactor_send_command(reactor, "{\"type\": \"increment\"}");
evorule_reactor_free(reactor);
```

## Feature Flags

| Feature       | 说明                                    |
| ------------- | --------------------------------------- |
| `persistence` | WAL 持久化（FactsLog + 哈希链写入磁盘） |
| `ffi`         | C FFI 接口（生成 cdylib）               |
| `kani`        | Kani 形式化验证（保留标记；实际由 `cargo kani` 注入 `cfg(kani)` 门控） |

## 版本策略

本项目遵循 [Semantic Versioning](https://semver.org/)：

- **MAJOR**：API 不兼容变更
- **MINOR**：向后兼容的功能新增
- **PATCH**：向后兼容的 bug 修复

## v0.3.2 更新

- **新增 `io_context` 模块**: CallerRole / IoCallContext / CallerRoleResolver（I/O 调用上下文与角色解析）
- **build.rs 新增 L1b 变更治理门禁**: CHANGE_REQUEST.md 必须存在且审查状态为"已批准"/"紧急通过"；新增策略层反模式检测；三仓（evorule-tcb/reactor/governance）build.rs 保持同一份内联副本实现
- **`EVORULE_SKIP_CR_GATE=1`** 环境变量可跳过 L1b 变更治理门禁（仅限本地开发）
- > **现状注记（TCB-2026-29 整改, 2026-09-15）**：上述 CR 构建校验属工程质量自查纪律（非防伪造审查机制），已从 build.rs 移出公开仓，`EVORULE_SKIP_CR_GATE` 随之删除；自查由维护者本地 git pre-commit hook 承接。策略层检测保留且无阀常开。历史条目按当时事实保留。

## 设计文档参考

- 项目级文档总索引: [`DOCS_INDEX.md`](../DOCS_INDEX.md)（所有 L1 公开文档的唯一入口）
- 项目级架构总览: [`README.md`](../README.md)（三层架构 + 快速开始）
- 本模块规格: [`REACTOR_SPEC.md`](REACTOR_SPEC.md)

---

## 许可证

**代码**:`AGPL-3.0-or-later`(见 [`LICENSE`](LICENSE) 文件)。

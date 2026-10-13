<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: AGPL-3.0-or-later

  This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
-->

# Tier 1 (Reactor) — 形式化规范

> **适用范围**: evorule-reactor
> **协议**: AGPL-3.0-or-later
> **状态**: 权威 (本文档是 `build.rs` 编译时门禁的依据)
> **跨模块设计**: 见 [GATE_REFERENCE.md](../GATE_REFERENCE.md) §四(跨模块门控图)+ §五(SPEC 章节编号映射)

---

## 核心原则

> **反应器是机制, 业务是策略。** 如果业务需求变了, 这行 Rust 代码不变。

这套规范的核心试金石是**"机制-策略分离原则"**:

> **如果业务需求变了, 这行代码需要改吗?**
>
> - **需要改** → 这是**策略** (业务逻辑), **必须**放在 JSON 数据中
> - **不需要改** → 这是**机制** (执行框架), **允许**写在 Rust 中

---

## 一、允许在 Rust (反应器) 中做的事情 ("机制")

以下属于系统的"骨架"或"管道", 不包含业务意图, 允许编写 Rust 代码:

### 1.1 反应器生命周期控制 (`reactor.rs`, `stable_detector.rs`)

- 控制 `max_rounds` 循环、检测版本号是否变动 (循环控制是机制)
- **注意**: 稳定检测的逻辑 (版本号比较) 是纯算法机制, 但**稳定阈值** (如 3 次) 是策略, 必须来自 JSON 配置, 不得硬编码

### 1.2 数据加载与结构转换 (`wal.rs`, `facts_log.rs`, `rule_validator.rs`, `rule_safety.rs`)

- 从文件系统读取 JSON 并反序列化
- 将 JSON 规则树转换为内部指令结构 (纯粹的格式映射, 无校验逻辑)
- 追加式日志的读写、版本号的单调递增 (日志存储是机制)

### 1.3 事实与状态管理 (`fact.rs`, `state.rs`, `channel.rs`, `phase.rs`)

- Fact 的构造、传递、状态机管理
- 通道发送/接收逻辑 (数据管道)
- 阶段 (phase) 切换控制

### 1.4 不变性与验证 (`invariants.rs`, `semantic_invariants.rs`, `pure.rs`)

- 不变量检查 (纯算法, 无业务判断)
- 语义不变量验证
- 纯函数标记

### 1.5 调试与可观测性 (`debug_control.rs`, `metrics.rs`, `time_machine.rs`)

- 调试控制开关
- 指标收集 (机制, 非业务数据)
- 时间旅行调试 (状态快照回放)

### 1.6 I/O 超时策略 (`io_timeout_policy.rs`)

- 超时控制 (机制, 阈值来自 JSON)

### 1.7 错误处理 (`error.rs`)

- 错误类型定义与传播

### 1.8 FFI 接口 (`ffi.rs`)

- C FFI 暴露 (阶段9, 可选 feature)

---

## 二、绝对禁止在 Rust 反应器做的事情 ("策略")

这些是业务逻辑或业务数据, 一旦写在 Rust 中即构成"漂移", `build.rs` 会直接拦截编译:

| 编号           | 禁止项                   | 示例                                                                 | 执行层                           |
| :------------- | :----------------------- | :------------------------------------------------------------------- | :------------------------------- |
| **G7/G8**      | 控制流指令名硬编码       | `"conditional"` / `"while_loop"` / `"sequence"` 出现在 Rust 字符串中 | L1 (build.rs)                    |
| **G1** (= F11) | panic-prone 构造         | `debug_assert!` / `.unwrap(` / `.expect(`                            | L1 (build.rs)                    |
| **§5.2**       | 业务术语字符串字面量     | `"math_rule"` / `"admin"` / `"summarize"` 等                         | L1 (build.rs)                    |
| **F1**         | 硬编码业务指令类型       | `if instruction_type == "math_rule"`                                 | L1 (§5.2 覆盖)                   |
| **F2**         | 硬编码数字阈值           | `if score > 80`                                                      | L3 (review)                      |
| **F3**         | 动态 prompt 拼接         | `format!("请总结：{}", content)`                                     | L3 (review)                      |
| **F4**         | 动态 SQL 拼接            | `format!("SELECT * FROM users WHERE id={}", id)`                     | L3 (review)                      |
| **F5**         | 硬编码权限/角色判断      | `if user.role == "admin"`                                            | L1 (§5.2 覆盖)                   |
| **F6**         | Rust 中过滤/排序规则列表 | `rules.iter().filter(\|r\| r.type == "active")`                      | L3 (review)                      |
| **F7/F8**      | if/else 嵌套 > 2 层      | —                                                                    | L2 (clippy cognitive_complexity) |
| **F9**         | 函数 > 50 行             | —                                                                    | L2 (clippy too_many_lines)       |
| **F10**        | 跨 Handler 互调          | handler A 调 handler B 的方法                                        | L3 (review)                      |

---

## 三、§5.2 业务术语表

以下术语**不得**作为字符串字面量出现在 evorule-reactor 的 Rust 源码中
(豁免: `#[cfg(test)]` 测试模块 + `src/fact.rs` 中的 IoType 内置字符串值定义):

> **v0.2.0 注**:`IoType` 已从封闭枚举重构为 `Arc<str>` + 工厂函数,支持 `IoType::new("任意字符串")` 注册自定义 io_type。
> 上表 `call_external` / `call_service` 仍为 fact.rs 内置工厂函数的字符串值(豁免);应用层自定义 io_type 的字符串字面量不受此表约束(属应用层策略,不在本 crate Rust 源码中)。

| 术语            | 类别     | 替代方案                                                    |
| :-------------- | :------- | :---------------------------------------------------------- |
| `math_rule`     | 业务规则 | 放 `core_eval.json`                                         |
| `physics_rule`  | 业务规则 | 放 `core_eval.json`                                         |
| `summarize`     | prompt   | 用模板变量                                                  |
| `admin`         | 角色     | 放权限配置                                                  |
| `teacher`       | 角色     | 放权限配置                                                  |
| `call_external` | I/O 指令 | IoType 内置值（fact.rs 工厂函数 `IoType::call_external()`） |
| `call_service`  | I/O 指令 | IoType 内置值（fact.rs 工厂函数 `IoType::call_service()`）  |

---

## 四、编译时门禁 (build.rs)

**build.rs 扫描的 15 个模式**:

| 规则                 | 模式                                                                                                        | 数量 |
| :------------------- | :---------------------------------------------------------------------------------------------------------- | :--- |
| G7/G8 (控制流硬编码) | `"conditional"`, `"while_loop"`, `"sequence"`                                                               | 3    |
| G1/F11 (panic-prone) | `debug_assert!`, `.unwrap(`, `.expect(`, `panic!(`                                                          | 4    |
| §5.2 (业务术语)      | `"math_rule"`, `"physics_rule"`, `"summarize"`, `"admin"`, `"teacher"`, `"call_external"`, `"call_service"` | 7    |
| G2/T10 (unsafe)      | `unsafe` (文件级豁免: `ffi.rs` / `facts_log.rs`)                                                            | 1    |

**豁免**:

- 任意命名的 `#[cfg(test)] mod <ident> { ... }` 测试模块 — 测试 fixture 可构造这些字符串（TCB-2026-35：按 `mod <ident>` 定位，非固定 `tests` 字面）
- 注释 (`//`, `///`, `//!`, `/* */`) — 文档可自由提及
- `src/fact.rs` (G8/§5.2 模式) — IoType 内置字符串值 / ControlFlowType 枚举映射的唯一真值来源

**紧急跳过**: `EVORULE_SKIP_GATE=1 cargo build` (阀值仅 `1`/`true` 生效 fail-closed, 须有书面理由 + `EVORULE_SKIP_REASON` 登记, 永不永久禁用)

**L1b 策略层检测 (v0.3.2 新增; TCB-2026-27/-29 整改)**: 除上述 L1a 字面量门禁外, `build.rs` 还执行策略层反模式检测(扫描 src/ **全文件**禁止策略层代码, TCB-2026-28 撤测试豁免; **无阀常开**, 不设旁路环境变量)。旧 CHANGE_REQUEST.md 构建校验属工程质量自查纪律, 从来不是防伪造审查机制, 已移出公开仓, `EVORULE_SKIP_CR_GATE` 随之删除; 自查由维护者本地 git pre-commit hook 承接 (不随仓库/发布公开)。四仓(evorule-tcb / evorule-reactor / evorule-governance / evorule-cli)build.rs 的共享函数保持同一份内联副本实现, 同步纪律由 `evorule-cli/tests/gate_sync_test.rs` 机器化锁定 (TCB-2026-30: 按函数名提取函数体, 规范化后逐仓比对, 漂移即 `cargo test` 红)。

---

## 五、跨模块引用

- **G1-G8** (全局门): 见 [GATE_REFERENCE.md](../GATE_REFERENCE.md) §四
- **F1-F11** (模块门): 见 [GATE_REFERENCE.md](../GATE_REFERENCE.md) §二.2 + §五.2
- **T1** (tier0 指令集有限性): 见 `../evorule-tcb/TCB_SPEC.md` §一
- **D1-D10** (数据流约束): 见 [GATE_REFERENCE.md](../GATE_REFERENCE.md) §四

evorule-governance 的 `GOVERNANCE_SPEC.md` 与本文档**结构相同** (G8 + F11 + §5.2),
这是有意的双层一致 (避免 tier1/tier2 走偏); 模式清单差异仅在 reactor 多一个 T10 `unsafe`。

---

## 总结口诀

> **写 Rust 只写"怎么跑" (循环、路由、存日志), 不写"跑什么" (阈值、模板、权限表)。凡是要根据业务变的值, 统统放进 JSON。**

---

**这份规范是 evorule-reactor 代码的权威标准。如有新增需求, 必须先更新这份规范, 再修改代码。**

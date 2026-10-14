<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: AGPL-3.0-or-later

  This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
-->

# Tier 2 (Governance) — 形式化规范

> **适用范围**: evorule-governance
> **协议**: AGPL-3.0-or-later
> **状态**: 权威 (本文档是 `build.rs` 编译时门禁的依据)
> **跨模块设计**: 见 [GATE_REFERENCE.md](../GATE_REFERENCE.md) §四(跨模块门控图)+ §五(SPEC 章节编号映射)

---

## 核心原则

> **如果业务需求变了, 你必须改 Rust 代码才能满足它, 那这段代码就是策略, 应该放在 JSON 里, 不是 Rust 里。**

治理层 (Governance) = **机制**: 暴露 HTTP/SSE + 审计链 + I/O 路由的脚手架。
它不得包含任何业务逻辑或领域词汇。新增业务规则应放入 `core_eval.json` (宪法),
而非 `evorule-governance/` 源码。

---

## 一、build.rs 强制执行的约束

| 编号           | 约束                                   | 理由                                                                                                                                                             | 禁止模式                                                                                                                                                           |
| :------------- | :------------------------------------- | :--------------------------------------------------------------------------------------------------------------------------------------------------------------- | :----------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **G7/G8**      | 反应器/治理层不得展开控制流原语        | 控制流属于 `core_eval.json`。Rust 中硬编码 `conditional` / `while_loop` / `sequence` 等于新增第 4 个元指令, 违反 evorule-tcb T1 红线 (指令集有限性 = 确定性来源) | `"conditional"`, `"while_loop"`, `"sequence"` 作为字符串字面量出现在 `*.rs` 中 (非 `#[cfg(test)]` 内)                                                              |
| **G1** (= F11) | 非测试代码不得 panic                   | evorule 是可审计系统; 管道中途 panic 会破坏 fact log。evorule-tcb 是唯一允许用 `Result` 处理不变量违规的层                                                       | `debug_assert!`, `.unwrap(`, `.expect(` 在非测试代码中                                                                                                             |
| **§5.2**       | 业务术语字符串字面量不得出现在 Rust 中 | 领域词汇属于 `core_eval.json`。Rust 中硬编码 `math_rule` / `call_external` / `teacher` 等于策略泄漏到机制, 违反"机制-策略分离"原则                               | `"math_rule"`, `"physics_rule"`, `"summarize"`, `"admin"`, `"teacher"`, `"call_external"`, `"call_service"` 作为字符串字面量 (非 `#[cfg(test)]` 且非 `fact.rs` 内) |

### 豁免 (build.rs 内置)

- `#[cfg(test)] mod tests { ... }` 测试模块 — 测试 fixture 可构造这些字符串驱动门控
- 注释 (`//`, `///`, `//!`, `/* */`) — 文档可自由提及
- `src/fact.rs` (G8/§5.2 模式) — IoType 内置字符串值 / ControlFlowType 枚举映射的**唯一**真值来源（注：`fact.rs` 在 `evorule-reactor`；governance 通过 re-export 使用 `IoType`，自身不定义 io_type 字符串值）

> **重要**: evorule-governance/build.rs 必须包含 `fact.rs` 豁免逻辑, 与 evorule-reactor/build.rs 保持一致。
> 这是 tier1/tier2 双层一致性的要求 (见 `00-design.md` §6 统一模板)。

### 紧急跳过

```bash
EVORULE_SKIP_GATE=1 cargo build       # 跳过 L1a 字面量门禁
EVORULE_SKIP_REASON="原因"            # 跳过理由登记 (未登记将出 warning)
```

阀值仅 `1`/`true` 生效 (`0`/空/其他值 = 门禁照常执行, fail-closed)。跳过必须临时且有书面理由。**永不永久禁用。** 当门控触发时, 正确做法几乎总是:
将违规字面量移入 `core_eval.json` 并通过元指令层引用, 或重命名它。

### L1b 策略层检测 (v0.3.2 新增; 门禁语义整改)

除上述 L1a 字面量门禁外, `build.rs` 还执行:

- **策略层反模式检测**: 扫描 `src/` 目录**全文件**(含测试模块, 测试豁免已撤), 禁止策略层代码(conditional / while_loop / sequence 等控制流指令)进入机制层。检测到违规时构建失败。**无阀常开**——机制-策略分离是设计不变量, 不设旁路环境变量。
- **CR 自查已移出公开仓 (既定裁定)**: 旧 L1b 的 CHANGE_REQUEST.md 构建校验属工程质量自查纪律, 从来不是防伪造审查机制, 已从 build.rs 移除, `EVORULE_SKIP_CR_GATE` 随之删除; 自查由维护者本地 git pre-commit hook 承接 (不随仓库/发布公开)。CHANGE_REQUEST.md 登记纪律本身不变。
- **四仓同步**: `evorule-tcb` / `evorule-reactor` / `evorule-governance` / `evorule-cli` 的 build.rs 共享函数保持同一份内联副本实现, 任何修改必须四仓同步; 同步纪律已机器化 (`evorule-cli/tests/gate_sync_test.rs`)。

---

## 二、允许在 Rust (治理层) 中做的事情 ("机制")

### 2.1 I/O 路由与 Handler (`io_subscriber.rs` + `io_dispatcher.rs` / `io_handler.rs` re-export)

- `io_subscriber.rs` — I/O 订阅者：消费 `Fact::IoRequest` → 按 `IoType` 路由到注入的 `IoHandler` 实现 → 回写 `Fact::IoResponse`（路由是机制，路由目标是策略）
- `io_dispatcher.rs` / `io_handler.rs` — **v0.2.0 起为 re-export**，`IoDispatcher` / `IoHandler` trait 定义已下沉至 `evorule-reactor`（机制层基座统一，agent 可直接依赖 reactor）；本 crate 保留 re-export 向后兼容
- 具体 I/O Handler 实现（db / http / memory）属应用层，已迁出本 crate（见 §2.5）

### 2.2 审计与哈希 (`auditor.rs`, `hash.rs`, `clock.rs`, `signing.rs`)

- 记录 TCB 返回的 `before`/`after` 快照 (只记录, 不判断内容)
- 计算 BLAKE3 哈希、维护逻辑时钟 (审计工具是机制)
- `hash.rs` 为 re-export（单一真相源在 `evorule-reactor::hash`）
- `signing.rs` — **v0.3.2 新增** G-A1 审计锚点签名（`AuditSigner` / `verify_signature`），ed25519（RFC 8032）确定性签名，在哈希链之上提供真实性/防抵赖；私钥不落盘审计资产，由调用方注入 32 字节种子（未配置签名器时审计行为与旧版完全一致）

### 2.3 共享状态 (`shared_facts_log.rs`)

- 共享 fact log（跨会话审计）
- **使用约束（B10，report-002 声明）**：`reset()` 仅限**纯内存实例**调用——底层 `FactsLog::reset` 不删除磁盘 WAL，持久化实例调用后旧事实会在重启后复活形成孤儿
- **已知局限**：`detect_orphan_facts` 基于内存投影；发生过压缩（compact）后历史前缀已丢弃，孤儿会**漏检但不误报**；孤儿只检测 + 告警，不自动伪造映射

### 2.4 会话 / 规则验证 / 时间机器 / 指标 / 权限 (`session.rs`, `rule_validation.rs`, `time_machine.rs`, `metrics.rs`, `permission.rs`)

- `session.rs` — SessionManager（多反应器实例生命周期管理）
- `rule_validation.rs` — 基于 tier0 `core_eval.json` 的 JSON Schema 规则验证（RuleValidator）
- `time_machine.rs` — replay / rewind / fork / diff 4 个 API（机制层能力；仅"可视化调试器 UI"在应用层）；**（v0.4.1：B8b，report-002）：`diff` 对 rewind 不可达版本显式返回 `Err(TimeMachineError)`，不再静默回退空 payload
- `metrics.rs` — IoMetrics trait（机制层接口，Prometheus 实现由应用层提供）
- `permission.rs` — **v0.3.2 新增** 权限门控模块（PermissionGate / PermissionTable / PermissionEntry / Verdict / ConditionEvaluator / DefaultPolicy / PermissionState / PermissionError），机制层权限原语，具体权限策略由应用层注入；**v0.4.1**（B6，report-002）：`IoSubscriber::with_permission_gate` 装配点落地——注入后 IoRequest 分派前经 `PermissionGate::check` 入口仲裁，拒绝回写错误 IoResponse 且不分派；未装配时行为与历史版本一致（默认不做权限判定）

### 2.5 已迁出（H5/H6 边界清理 + v0.2.0 下沉）

以下模块原属 governance，已迁至应用层或下沉至 reactor（本 crate 不再包含）：

- `api/` 目录（portal / session / server / auth / hot_reload）— HTTP API / SSE / Bearer 认证 / 热重载 → 应用层
- `io_handlers/` 目录（db / http / memory handler）— 具体 I/O 实现 → 应用层
- `cluster.rs` — 多 reactor 协作原语 → 应用层
- `object_pool.rs` — FactsLog 对象复用 → 应用层
- `io_dispatcher.rs` / `io_handler.rs` 的**定义** — v0.2.0 下沉至 `evorule-reactor`（本 crate 保留 re-export）

---

## 三、绝对禁止在 Rust 治理层做的事情 ("策略")

与 evorule-reactor 完全相同, 见 `../evorule-reactor/REACTOR_SPEC.md` §二。

---

## 四、跨模块引用

- **G1-G8** (全局门): 见 [GATE_REFERENCE.md](../GATE_REFERENCE.md) §四
- **F1-F11** (模块门): 见 [GATE_REFERENCE.md](../GATE_REFERENCE.md) §二.2 + §五.3
- **D1-D10** (数据流约束): 见 [GATE_REFERENCE.md](../GATE_REFERENCE.md) §四
- **T1** (tier0 指令集有限性): 见 `../evorule-tcb/TCB_SPEC.md` §一
- **tier1 双层一致**: 见 `../evorule-reactor/REACTOR_SPEC.md`

---

## 五、build.rs 一致性

evorule-governance/build.rs 跟 evorule-reactor/build.rs **结构相同** (共享函数同一份内联副本),
这是有意的双层一致 (避免 tier1/tier2 走偏)。字面量门禁模式为 reactor 的 14/15 子集——
governance 不设 T10 `unsafe` 模式 (该仓 `#![deny(unsafe_code)]` 由 rustc 直接兜底)。

**关键一致性要求**:

- FORBIDDEN 数组: governance 14 模式为 reactor 15 模式去掉 T10 的子集
- `strip_test_mod` 函数: 实现方式相同
- `fact.rs` 豁免: **两者都必须包含** (G8/§5.2 模式在 fact.rs 中豁免)
- **L1b 策略层检测 (v0.3.2 新增; 门禁语义整改)**: `detect_strategy_patterns` 函数必须四仓(evorule-tcb / evorule-reactor / evorule-governance / evorule-cli)同步, 防止四个模块的检测标准走偏; 同步已机器化 (`evorule-cli/tests/gate_sync_test.rs` 逐函数比对, 四仓同步整改)。旧 `validate_change_request_gate` 已移出公开仓 (自查本地化), 不再要求同步

---

## 如何新增约束

1. 在上方 **build.rs 强制执行的约束** 表中加一行: 编号、约束文本、理由、禁止模式
2. 在 `build.rs` 的 `FORBIDDEN` 数组中加对应的 `(label, needle)` 条目
3. 如果约束需要比字节子串匹配更复杂的扫描 (如 AST 感知), 在此处文档说明
4. 运行 `cargo build -p evorule-governance` 确认门控在干净树上仍 PASS

---

## 总结口诀

> **治理层 = 管道 + 审计 + 路由。业务决策全部放 core_eval.json, Rust 只写"怎么跑"。**

---

**这份规范是 evorule-governance 代码的权威标准。如果构建失败且你认为门控有误,
要问的不是"能否绕过", 而是"规范是否需要更新"。规范需要更新时, 先更新规范, 再更新 build.rs。**

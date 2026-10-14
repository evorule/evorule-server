<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: AGPL-3.0-or-later

  This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
-->

# evorule-governance

> EvoRule 三层架构的 Tier 2 治理层 —— I/O 订阅者、审计链、HTTP API。

- **版本**:v0.6.0
- **依赖**:evorule-tcb = "0.6.0" + evorule-reactor = { version = "0.6.0", features = ["persistence"] }
- **协议**:AGPL-3.0-or-later
- **测试**:`cargo test` 全量 PASS / 0 failed（2026-09-14 v0.6.0 全量回归，CI 常驻）
- **build.rs 编译时门禁**:F11 禁止 `unwrap`/`expect`/`panic!`/`debug_assert!`(非测试代码),**G8 控制流白盒化**,PASSED
- **G8 门控遵守**:治理层的业务语义(审计/会话/调试端点)全部通过 **结构不变式 + Fact 数据驱动**,不在 `src/**/*.rs`(非测试代码)中展开 if/else 业务控制流。
- **`unsafe`**:`#![forbid(unsafe_code)]`
- **P0 修复(2026-07-25)**:锁中毒改为 `e.into_inner()` 恢复(非 panic);`auditor.rs` 哈希失败改为跳过损坏 Fact(非 panic);SIGTERM handler 安装失败降级为仅监听 SIGINT(非 panic)

> 本 crate 属于 [EvoRule](https://gitee.com/evorule) 生态:[主仓](https://gitee.com/evorule/evorule) ｜ [在线控制台 Demo](https://evorule.github.io/evorule-console-cloud/) ｜ [evorule-server（应用层）](https://gitee.com/evorule/evorule-server)

## 定位声明

`evorule-governance` 是 EvoRule **三层架构的 Tier 2 治理层**(纯机制层,不含业务策略):

```text
┌─────────────────────────────────────────────────┐
│  evorule-governance  治理层（本 crate）             │
│  · I/O 分发框架 / 审计链 / 规则验证 / 时间机器    │
│  · 机制层库: IoDispatcher / IoHandler（v0.2.0 起定义下沉至 reactor，本 crate re-export）│
├─────────────────────────────────────────────────┤
│  evorule-reactor     反应式执行器（Fact/MPSC/WAL）  │
├─────────────────────────────────────────────────┤
│  evorule-tcb         纯计算内核                     │
└─────────────────────────────────────────────────┘
```

**职责(机制)**:

- **I/O 订阅者**:消费 `Fact::IoRequest` → 通过 IoDispatcher 分发给上层注入的 `IoHandler` 实现 → 产生 `Fact::IoResponse`（具体 Handler 实现由应用层注入；v0.2.0 起 `IoDispatcher`/`IoHandler` 定义下沉至 evorule-reactor，本 crate re-export 向后兼容）
- **审计链**:基于 tier1 WAL 的 blake3 哈希链,支持 `load_from_tier1_wal()` 加载并验证完整性
- **规则验证器**:基于 tier0 `core_eval.json` 的 JSON Schema 规则验证（RuleValidator）
- **时间机器**:基于 tier1 FactsLog 的 replay / rewind / fork / diff 4 个 API
- **SessionManager**:会话管理器（多反应器实例生命周期管理，机制层）

**不承担(策略/应用)**:

- ❌ 具体 I/O handler 实现(HTTP/SQLite/Memory handler → 由应用层提供)
- ❌ HTTP API / SSE / Prometheus metrics / Bearer 认证（→ 由应用层提供）
- ❌ 业务策略(具体规则/权限配置由上层应用提供)

## 模块结构

```text
src/
├── auditor.rs              # 审计器(基于 tier1 WAL 的哈希链验证)
├── clock.rs                # 逻辑时钟
├── hash.rs                 # blake3 哈希算法(re-export evorule_reactor::hash)
├── io_dispatcher.rs        # I/O 分发器（v0.2.0 起为 re-export，定义已下沉至 evorule-reactor）
├── io_handler.rs           # IoHandler trait + IoResult（v0.2.0 起为 re-export，定义已下沉至 evorule-reactor）
├── io_subscriber.rs        # I/O 订阅者(消费 IoRequest → 分发 → 回写 IoResponse)
├── metrics.rs              # IoMetrics trait(机制层接口, Prometheus 实现由应用层提供)
├── permission/             # 权限门控(机制层原语, 具体策略由应用层注入)
├── rule_validation.rs      # 规则验证器(RuleValidator, 基于 tier0 core_eval.json)
├── session.rs              # 会话管理器(SessionManager, 多反应器实例生命周期管理)
├── shared_facts_log.rs     # 共享 FactsLog(跨 session 审计)
├── signing.rs              # G-A1 审计锚点签名(ed25519 确定性签名, 真实性/防抵赖)
└── time_machine.rs         # 时间机器(replay / rewind / fork / diff)
```

**H5/H6 边界清理后已移除的模块**(均属应用层,已迁出本 crate):

- `api/` 目录 — HTTP API (axum 路由 / Bearer token / CORS / 速率限制 / SSE)
- `io_handlers/` 目录 — 具体 I/O 实现(db_handler / http_handler / memory_handler)
- `bin/` 目录 — 独立二进制
- `cluster.rs` — 多 reactor 协作原语
- `object_pool.rs` — FactsLog 对象复用优化
- `api/portal.rs` — Portal 聚合端点(应用层 UI)
- `api/hot_reload.rs` — 业务规则热重载

> **注意**：`time_machine.rs` **未被移除** — 时间旅行 4 个 API 是机制层能力，保留在本 crate；仅"可视化调试器 UI"在应用层。

## 作为 lib 使用（evorule 核心仓）

evorule-governance 现为**纯机制层库**（无 bin target），应作为 library 被上层应用依赖：

```toml
# 在你自己的应用仓的 Cargo.toml 中
[dependencies]
evorule-governance = { version = "0.6.0" }
```

快速开始示例：

```rust
use evorule_governance::{SessionManager, RuleValidator, Auditor};

// 1. 启动一个 reactor 会话
let sessions = SessionManager::new();
let session_id = sessions.create(Default::default()).await?;

// 2. 验证 JSON 规则
let validator = RuleValidator::from_default_core_eval()?;
let report = validator.validate(&my_rule_json)?;
if !report.is_valid() { /* 拒绝非法规则 */ }

// 3. 提交命令 (通过 SessionManager 路由到对应 reactor)
sessions.submit(&session_id, command_payload).await?;

// 4. 审计: 加载并验证 blake3 哈希链
let auditor = Auditor::load_from_tier1_wal(wal_path)?;
let verified = auditor.verify_chain()?;
```

> **HTTP API 项目方**：本 crate 不提供 HTTP API。如需 HTTP/SSE 服务，由应用层基于本 crate 的机制自行构建。

## 审计链与哈希链

### 两套 WAL 合并(tier1 哈希链)

自 0.1.0 起,哈希链已提升到 tier1 的 FactsLog/WAL 层:

- **tier1 WAL** 写入时自动计算并存储哈希链(`content_hash`/`prev_hash`/`chain_hash`)
- **tier2 Auditor** 不再独立写 WAL(`append_wal` 已废弃)
- **恢复审计状态** 使用 `Auditor::load_from_tier1_wal()`(读取 tier1 WAL 并验证哈希链)
- **单一真相源** 哈希算法的唯一实现在 `evorule_reactor::hash`,tier2 通过 re-export 调用

### WAL 格式(v2)

```json
{
  "version_before": 0,
  "fact": {"type": "Command", "id": 1, "instruction": {...}},
  "content_hash": "blake3(fact_to_stable_json(fact))",
  "prev_hash": "前一条的 chain_hash(首条为 \"genesis\")",
  "chain_hash": "blake3(prev_hash + content_hash)"
}
```

### Auditor API

| 方法                                | 说明                          |
| ----------------------------------- | ----------------------------- |
| `Auditor::new(facts_log)`           | 创建审计器                    |
| `auditor.load_from_tier1_wal(path)` | 从 tier1 WAL 加载并验证哈希链 |
| `auditor.entries()`                 | 获取审计条目列表              |
| `auditor.last_hash()`               | 获取审计链末尾哈希            |
| `auditor.append_wal(...)`           | ⚠️ 已废弃,不再使用            |

## 安全

### 已实现（本 crate 机制层）

- **blake3 哈希链**(`hash.rs` + `auditor.rs`):基于 tier1 WAL 的 append-only 审计链,每个 Fact 携带哈希字段,篡改可检测;`Auditor::load_from_tier1_wal()` 提供完整性校验。
- **WAL 持久化**:通过 `persistence` feature 启用 tier1 WAL 持久化(由 evorule-reactor 提供)。
- **build.rs 编译时门禁**:F11 禁止 `panic!`/`unwrap`/`expect`(非测试代码)。
- **`#![forbid(unsafe_code)]`**:全 crate 禁止 unsafe。

> HTTP API 认证、速率限制、CORS、SQLite/HTTP handler 等应用层安全特性不在本 crate 范围内，由应用层基于本 crate 的机制接口自行实现。

### 历史安全审计基线

> 历史审计报告: 安全审计基线（待发布，1.0 升门时提供）（历史全栈审计快照，含已迁出的应用层代码风险项 H6-H9/M1）

本 crate 为纯机制层库，不包含 HTTP API、I/O handler、认证中间件等应用层代码。历史审计中的 H6（SSRF）、H7（SQL 注入）、H8（CORS）、H9（DB URL 回退）、M1（auth 默认禁用）等风险项均针对已迁出的应用层实现，由应用层各自承担修复责任。

本 crate 自身的安全保证为：永不 panic（build.rs F11 门禁）、`#![forbid(unsafe_code)]`、blake3 哈希链审计完整性。

## Feature Flags

| Feature       | 说明                                   |
| ------------- | -------------------------------------- |
| `persistence` | WAL 持久化(依赖 tier1 evorule-reactor) |

> `metrics` 和 `auth` feature 已随应用层代码迁出移除。IoMetrics trait 总是可用（机制层接口），Prometheus 实现和认证中间件由应用层提供。

---

## v0.3.2 更新

- **新增 `permission` 模块**: PermissionGate / PermissionTable / PermissionEntry / Verdict / ConditionEvaluator / DefaultPolicy / PermissionState / PermissionError（机制层权限原语，具体权限策略由应用层注入）
- **`auditor.report()` / `auditor.export()` 返回值变更**: 从 `String` 改为 `Result<String, serde_json::Error>`，不再静默退化为 `"{}"`
- **`session.audit_report()` / `session.audit_export()` 返回值变更**: 从 `String` 改为 `Result<String, String>`
- **`verify_hash_chain` 已删除**: 原函数始终返回 `true` 是"假验证"陷阱，已彻底删除。替代方案：用 `compute_chain_hash` 重算后与存储的链哈希比对
- **规则校验行为变更**: 元指令白名单修正为 6 种（branch/set/push/io_request/collect/merge），移除误混的 noop/increment/decrement；MAX_NESTING_DEPTH 8→64；set 非法 operation 提升为 error；merge 新增 tool_result/tool_results 校验
- **Fact 类型映射修正**: 移除 ControlSignal（Fact 枚举无此变体），新增 Stable（终止事实）
- **build.rs 新增 L1b 变更治理门禁**: CHANGE_REQUEST.md 必须存在且审查状态为"已批准"/"紧急通过"；新增策略层反模式检测
- > **现状注记（既定整改, 2026-09-15）**：上述 CR 构建校验属工程质量自查纪律（非防伪造审查机制），已从 build.rs 移出公开仓，`EVORULE_SKIP_CR_GATE` 随之删除；自查由维护者本地 git pre-commit hook 承接。策略层检测保留且无阀常开。历史条目按当时事实保留。

## v0.6.0 更新

- **元指令白名单收窄（规则清理，破坏性变更）**: `VALID_TRANSFORM_TYPES` 收窄为 4 种（branch/set/push/io_request，= TCB − enforce）；`collect`/`merge` 校验逻辑删除，规则文件使用将**加载即拒**；LLM 多轮编排归应用层

## 设计文档参考

- 项目级文档总索引: [`DOCS_INDEX.md`](../DOCS_INDEX.md)（所有 L1 公开文档的唯一入口）
- 项目级架构总览: [`README.md`](../README.md)（三层架构 + 快速开始）
- 本模块规格: [`GOVERNANCE_SPEC.md`](GOVERNANCE_SPEC.md)

---

## 协议与分发

**代码**:`AGPL-3.0-or-later`(见 [`LICENSE`](LICENSE))。

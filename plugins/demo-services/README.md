<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: AGPL-3.0-or-later

  This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
-->

# evorule-demo-services

**yuanze-demos 业务服务 Rust 原生实现 —— 以 IoHandler 形式挂载到 evorule-server IoDispatcher**

> **crate 类型**: 内部 lib（`publish = false`，不进 crates.io）
> **引入版本**: v0.3.0
> **阶段**: Phase 1（服务 Rust 化）

---

## 定位

把 yuanze-demos 的 7 个 Python FastAPI 服务内嵌为 evorule-server 的原生 `IoHandler`，使 `io_request(call_service, service_name=...)` 在进程内确定性执行，无需起 Python 服务。

- 独立 crate（`plugins/demo-services`），不修改核心 crate
- 仅经 `IoDispatcher` 挂载，与核心层解耦
- 复合路由：原生优先，HTTP 回落

---

## 7 个原生服务

| 服务名 | 源文件 | 功能 |
|--------|--------|------|
| `ik_solver` | `ik_solver.rs` (8.8KB) | 逆运动学求解器（LMA 算法） |
| `llm_advisor` | `llm_advisor.rs` (6.9KB) | LLM 建议生成（规则驱动，非真实 LLM 调用） |
| `robot_move` | `robot_move.rs` (3.4KB) | 机器人移动指令生成（确定性 ID，无墙钟依赖） |
| `rule_sandbox` | `rule_sandbox.rs` (12.6KB) | 规则沙盒试运行（隔离执行 + 结果对比） |
| `sampling` | `sampling.rs` (2.7KB) | 采样决策器（决定哪些规则需要影子验证） |
| `shadow_validate` | `shadow_validate.rs` (2.7KB) | 影子验证（新旧规则并行执行 + 结果对比） |
| `config_persist` | `config_persist.rs` (2.6KB) | 配置持久化（服务配置读写） |

---

## 复合路由设计

`DemoServiceRouter` 实现 `IoHandler` trait，按 `params.service_name` 分发：

```
io_request(call_service, service_name="ik_solver", args={...})
    │
    ├─ 命中原生服务名 → 调用对应原生实现（进程内，确定性，无网络）
    │
    └─ 未命中 → 回落 ServiceRegistryHandler（HTTP，兼容其他外部服务）
```

原生实现接收的入参 = `params.args`（与 HTTP 版发送的 body 语义一致）。

---

## 与 Python 基线的一致性

| 维度 | 保证措施 |
|------|----------|
| **业务返回结构** | `converged_ok` / `status` / `passed` 等字段与 Python 服务完全一致，保证 facts/audit 业务语义一致（确定性对比） |
| **浮点处理** | TCB 无 Float 变体，浮点一律以字符串返回，与 `serde_to_json_value` 行为一致 |
| **墙钟隔离** | `robot_move` 不再用 `time`/`uuid`，改用确定性逻辑计数器（确定性 ID），保证可复现 |
| **错误处理** | 所有原生服务实现 `NativeService` trait，统一 `execute(&self, args: &JsonValue) -> IoResult` 签名 |

---

## 挂载方式

在 `evorule-server` 的 `main.rs` 中：

```rust
use evorule_demo_services::DemoServiceRouter;
use evorule_io_handlers::ServiceRegistryHandler;

// 复合路由：原生优先，HTTP 回落
let router = DemoServiceRouter::new(
    ServiceRegistryHandler::from_registry_path("service_registry.json")?
);

// 挂载到 IoDispatcher
dispatcher.register_handler("call_service", Arc::new(router));
```

---

## 依赖

- `evorule-tcb` 0.3.1 — 核心类型（`JsonValue` 等）
- `evorule-reactor` 0.3.1 — `IoHandler` / `IoResult` trait
- `evorule-io-handlers` — `ServiceRegistryHandler`（HTTP 回落）
- `evorule-rule-schema` — 规则 Schema 校验（rule_sandbox 使用）
- `serde_json` — JSON 处理
- `tracing` — 日志
- `async-trait` — `IoHandler` trait object-safety

> **注意**: 核心层依赖通过顶层 `[patch.crates-io]` 覆盖为本地源码（开发阶段），发布前需移除 patch 段并使用 crates.io 正式版本。

---

## 相关文件

- [INTEGRATION_GUIDE.md](../../docs/INTEGRATION_GUIDE.md) §1.3 — ServiceRegistry 机制
- [GATE_REFERENCE.md](../../GATE_REFERENCE.md) §3.5 — 本 crate 的门控说明
- `rules/bundles/bundle-ds-yuanze-01-v3/` — 配套的 15 条规则包

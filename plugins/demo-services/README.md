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

> 服务名以声明文件 `official_native_services.json`（本目录，SSOT）为准，下表与声明文件同步维护；清单启用/治理侧目录种子均以此派生（治理侧经嵌入副本同步）。

| 服务名 | 源文件 | 功能 |
|--------|--------|------|
| `inverse_kinematics_solver` | `ik_solver.rs` | 逆运动学求解器（LMA 算法） |
| `robot_move_joints` | `robot_move.rs` | 机器人移动指令生成（确定性 ID，无墙钟依赖） |
| `llm_advisor` | `llm_advisor.rs` | LLM 建议生成（规则驱动，非真实 LLM 调用;sensitive:涉及外部 LLM API） |
| `shadow_ik_solver` | `shadow_validate.rs` | 影子 IK 求解（新旧规则并行执行 + 结果对照） |
| `sampling_service` | `sampling.rs` | 采样决策器（决定哪些规则需要影子验证） |
| `rule_sandbox` | `rule_sandbox.rs` | 规则沙盒试运行（隔离执行 + 结果对比） |
| `config_persist` | `config_persist.rs` | 配置持久化（热加载补丁 mock；`persisted:true` 为 mock 假成功语义，不代表真实落库，真实配置读写走 finance-config 外部插件包） |

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

> 机制件说明：路由器机制（trait / 声明项结构 / 过滤路由器 / 三拒绝校验）已上提至
> `core/plugin-kit`（`evorule-plugin-kit`）公共 crate，三插件归一单份维护；
> 本 crate 为薄壳具名委托（`DemoServiceRouter` → `NativeServiceRouter`），
> 对外 API 与行为逐字节等价，插件自持声明表 `NATIVE_SERVICES`。

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

在 `evorule-server` 的 `main.rs` 中（按插件清单决定挂载形态，见下节）：

```rust
use evorule_demo_services::DemoServiceRouter;
use evorule_io_handlers::ServiceRegistryHandler;

// 全量挂载：复合路由，原生优先，HTTP 回落
let router = DemoServiceRouter::new(svc_handler.clone);

// 子集挂载：仅启用清单声明的服务
let router = DemoServiceRouter::with_enabled(svc_handler.clone, &["config_persist"])?;

// 挂载到 IoDispatcher
dispatcher.register(IoType::call_service, router);
```

---

## 插件清单化（部署期启用/裁剪）

本插件支持部署期按清单启用子集（`evorule-server --plugins plugin_manifest.json`）：

- `services` 省略 = 全部 7 服务启用；显式列出 = 子集启用；`enabled: false` = 不挂载本路由。
- 未知名 / 重复名 / 空启用集 → 启动 fail-fast（错误含合法服务名与自诊断指引，不静默去重）。
- 未启用的服务名回落 `ServiceRegistryHandler`（HTTP），与进程外服务同路径。
- 运行可见性：`GET /api/health` 的 `plugins` 节呈现实际挂载的服务名集（声明表序）。

完整清单语义见 evorule-server README「插件清单」章节。

---

## 新增一个原生服务（C5 指引）

新增原生能力 = 声明文件追加一项 + `NATIVE_SERVICES` 声明表追加构造子，宿主代码零改动：

1. 实现 `NativeService` trait（`execute(&self, args: &JsonValue) -> IoResult`；浮点一律字符串返回，确定性优先，禁用墙钟/随机源）。
2. 在 `official_native_services.json`（SSOT）追加一项（name/sensitive/description）。
3. 在 `lib.rs` 的 `NATIVE_SERVICES` 声明表追加 `NativeServiceDef { name, sensitive, description, make }`（`make` 构造子必须留在代码；其余元数据以声明文件为准）。
4. 运行 `evorule-server/scripts/sync-native-services.ps1`：同步治理侧嵌入副本 + 双侧守卫自动验证（脚本即节奏，双绿才算完成）。
5. 路由分发 / 清单校验 / `/api/health` plugins 节 / 治理侧目录种子自动生效，无需改动其他代码；部署方按需在 `plugin_manifest.json` 的 `services` 中启用（缺省全启用，无需动作）。

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

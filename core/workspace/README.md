<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: AGPL-3.0-or-later

  This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
-->

# evorule-workspace

**多租户工作空间 + 规则元数据管理 + 会话桥接 + 沙盒编排 + 发布队列**

> **crate 类型**: 内部 lib（`publish = false`，不进 crates.io）
> **引入版本**: v0.2.0
> **基础设施层级**: P10（基础设施层）

---

## 定位

Workspace crate 是 evorule-server 的**多租户管理核心**，提供：
- 工作空间（Workspace）的 CRUD 和隔离
- 规则元数据的版本管理、状态机流转、BLAKE3 哈希链
- 会话桥接（SessionBridge）：将 workspace 操作桥接到 evorule-governance 的 SessionApi
- 沙盒编排（SandboxService）：规则试运行、合成 IO 响应、测试报告生成
- 发布队列（PublishService）：三级权限审批、滚动 session 热重载

---

## 模块结构（16 个子模块）

| 模块 | 大小 | 职责 |
|------|------|------|
| `db` | 102KB | SQLite 连接 + schema 迁移 + 全部 CRUD（workspaces/rules/sessions/sandbox/publish_queue/production） |
| `api` | 62KB | HTTP handler + Router 构建（workspace API 端点） |
| `rule_translate` | 43KB | BusinessRule ↔ evorule 核心 6 域类型双向翻译引擎 |
| `publish_service` | 39KB | 发布队列 + 三级权限 + 状态机（P1） |
| `models` | 39KB | 数据模型 + 状态机枚举 |
| `sandbox_service` | 31KB | 沙盒编排主流程（S1） |
| `rolling_session` | 27KB | 滚动 session 热重载编排（P2） |
| `rule_meta_service` | 25KB | 规则元数据 + 状态机 + BLAKE3 哈希 |
| `workspace_service` | 22KB | Workspace 业务服务 |
| `verdict_service` | 11KB | 沙盒判定服务 |
| `test_report` | 9.5KB | 测试报告 schema + 生成 + BLAKE3 签名（S3） |
| `error` | 7.7KB | 错误类型 + axum IntoResponse 实现 |
| `session_switched` | 6.3KB | U7 SSE session_switched 广播（P3） |
| `mock_io_responder` | 6.1KB | 沙盒合成 IO 响应器（S2） |
| `session_bridge` | 5.4KB | SessionOps trait（桥接 SessionApi） |
| `lib` | 3.5KB | crate 入口 + 模块声明 |

---

## 规则翻译引擎（rule_translate）

`rule_translate.rs` 是 workspace 与 evorule 核心之间的**翻译层**，解决以下问题：

- evorule TCB 仅支持 6 种域类型（eq/lt/exists/instruction/all/not/has_fields），不支持 gt/gte/or
- workspace 业务规则使用更丰富的域类型，需要翻译为核心可执行格式
- `gte` → `not(lt)`，`gt` → `not(all([lt, eq]))`，`or` → `not(all([not(a), not(b)]))`
- 反向翻译：核心格式 → 业务规则格式，用于展示和编辑

**关键修复（v0.2.0）**：
- `params.path` vs `params.attr` 不一致：统一为 `params.attr`（evorule core `exec_set` 读取 `attr`）
- `action_set` 角色丢失：修复 value 字段正确包含 role 信息
- G5 校验白名单遗漏：新增 `__exec__.result.*` 路径前缀

---

## 沙盒编排（sandbox_service）

沙盒服务提供规则试运行能力，不影响生产 session：

1. **创建沙盒 session**：从生产规则创建隔离的 session
2. **合成 IO 响应**（`mock_io_responder`）：根据规则配置生成模拟 I/O 响应，无需真实外部服务
3. **执行测试用例**：提交预设指令序列，收集执行结果
4. **生成测试报告**（`test_report`）：包含通过/失败统计、差异对比、BLAKE3 签名
5. **判定**（`verdict_service`）：根据测试报告判定规则是否可发布

---

## 发布队列（publish_service）

发布服务实现三级权限审批流程：

1. **草稿（Draft）** → 开发者创建规则
2. **沙盒验证（Sandbox）** → 规则通过沙盒测试
3. **审批中（Pending）** → 提交审批
4. **已批准（Approved）** → 审批通过
5. **发布中（Publishing）** → 滚动 session 热重载
6. **已发布（Published）** → 规则生效

**滚动 session 热重载**（`rolling_session`）：发布时不中断现有 session，逐步将新规则应用到新创建的 session，旧 session 继续使用旧规则直到结束。

---

## 数据持久化

- **数据库**: SQLite（`rusqlite` with `bundled` feature）
- **表结构**: workspaces / rules / rule_versions / sessions / sandbox_runs / test_reports / publish_queue / production_rules
- **迁移**: 内置 schema 迁移逻辑，版本化管理
- **哈希链**: 规则元数据使用 BLAKE3 哈希链，保证不可篡改

---

## 安全约束

- `#![forbid(unsafe_code)]`（C4）
- 测试代码外禁止 `unwrap`/`expect`/`panic`（C5）
- 所有数据库操作使用参数化查询，防止 SQL 注入
- 工作空间隔离：每个 workspace 的规则和 session 完全隔离，通过 workspace_id 过滤

---

## 依赖

- `rusqlite` (bundled) — SQLite 数据库
- `serde` + `serde_json` — JSON 序列化
- `blake3` — 哈希链
- `chrono` — 时间处理
- `ulid` — 唯一 ID 生成
- `axum` 0.8 + `tokio` — HTTP 框架和异步运行时
- `evorule-tcb` / `evorule-reactor` / `evorule-governance` — 核心引擎

---

## 相关文档

- [INTEGRATION_GUIDE.md](../../docs/INTEGRATION_GUIDE.md) — 集成指南
- [PITFALLS.md](../../docs/PITFALLS.md) — 踩坑记录
- [GATE_REFERENCE.md](../../GATE_REFERENCE.md) — 门控参考
- `evorule-server/src/api/server.rs` — workspace API 路由挂载

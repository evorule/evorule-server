<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: AGPL-3.0-or-later

  This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
-->

# evorule-server

**独立二进制服务入口（应用层）—— HTTP API + SSE 事件流 + 多会话管理**

> **crate 类型**: binary（`publish = false`，不进 crates.io）
> **监听端口**: 默认 `0.0.0.0:18080`
> **设计依据**: H5（从核心层迁出到独立仓）、H6（AuthConfig 和 API server 从 lib 导入）

---

## 定位

evorule-server 是 EvoRule 规则引擎的**应用层服务**，提供：
- RESTful HTTP API（session 管理、命令提交、状态查询、审计链、规则校验等）
- SSE 事件流（实时推送 fact log、状态变更）
- 多会话管理（每个 session 独立的反应器实例和审计链）
- 内置 I/O handler（DB / HTTP / Memory，来自 `evorule-io-handlers` crate）
- Bearer token 认证 + 速率限制
- Prometheus 指标端点
- OpenAPI 3.1 规范导出（`utoipa` 单一真相源）

**核心层不应依赖具体 I/O handler 实现（策略）**，因此应用层独立成仓。

---

## 源码结构

| 文件 | 大小 | 职责 |
|------|------|------|
| `api/server.rs` | 232KB | HTTP 路由 + 中间件 + GovernanceApi（约 60 条路由） |
| `main.rs` | 70KB | 启动入口、CLI 参数、配置加载、优雅退出 |
| `api/bundles.rs` | 31KB | 规则包 API（导入/列出/回滚，0.3.0 新增） |
| `input_sanitizer.rs` | 27KB | 输入清洗（防止恶意输入注入） |
| `metrics_impl.rs` | 14KB | Prometheus 指标实现（7 个核心 metric） |
| `api/permissions.rs` | 10KB | 权限 API（0.3.0 新增） |
| `auth.rs` | 8KB | 认证逻辑（Bearer token + 恒定时间比较） |
| `api/openapi.rs` | 8KB | OpenAPI 单一真相源（`utoipa::OpenApi` derive） |
| `lib.rs` | 885B | crate 入口 + 模块声明 |
| `api/mod.rs` | 809B | API 模块声明 |

---

## 快速开始

```bash
# 编译
cargo build --release

# 启动（无认证，仅本地开发）
evorule-server --addr 127.0.0.1:18080

# 启动（带认证，生产环境）
evorule-server --addr 0.0.0.0:18080 --auth-token secret123

# 使用配置文件
evorule-server --config evorule.json --log-format json
```

**配置加载优先级**: CLI 参数 > 环境变量（前缀 `EVORULE_`）> JSON 配置文件 > 内置默认值

---

## 主要 CLI 参数

| 参数 | 环境变量 | 默认值 | 说明 |
|------|----------|--------|------|
| `--addr` | `EVORULE_ADDR` | `0.0.0.0:18080` | 监听地址 |
| `--auth-token` | `EVORULE_AUTH_TOKEN` | (空) | Bearer token（留空=关闭认证，仅 dev） |
| `--service-token` | `EVORULE_SERVICE_TOKEN` | (空) | 受信服务管道 token（service 身份，可写受保护域 `stable.llm`/`stable.system`；仅认证启用时生效，B5） |
| `--config` | `EVORULE_CONFIG` | (无) | JSON 配置文件路径 |
| `--log-format` | `EVORULE_LOG_FORMAT` | `text` | 日志格式：text / json |
| `--rules-dir` | `EVORULE_RULES_DIR` | `./rules` | 规则目录（hot_reload 监控） |
| `--workspace-db` | `EVORULE_WORKSPACE_DB` | `./data/workspace.db` | Workspace SQLite 数据库路径 |
| `--enable-workspace-api` | `EVORULE_ENABLE_WORKSPACE_API` | `false` | 启用 Workspace API |
| `--metrics-auth` | `EVORULE_METRICS_AUTH` | `false` | /metrics 端点是否需要认证 |
| `--openapi-ui` | `EVORULE_OPENAPI_UI` | `false` | 启用 Swagger UI（/api/docs） |
| `--allow-loopback` | `EVORULE_ALLOW_LOOPBACK` | `false` | 允许 I/O handler 访问 loopback 地址（生产环境永远不要启用） |

> **安全约束**: 无 token 且绑定非 loopback 地址时 **fail-closed 拒绝启动**（B3）。loopback 地址（127.0.0.1 / [::1]）仍允许无认证启动供本地开发。

---

## 优雅退出

- 监听 `SIGTERM`（Docker 停止信号）和 `SIGINT`（Ctrl+C）
- 收到信号后：
  1. `readiness` 设为 `false`（负载均衡器切走流量）
  2. 等待进行中请求完成
  3. 30s 超时强制退出
- `GET /api/health/liveness` 始终 200
- `GET /api/health/readiness` 在退出期间返回 503

---

## API 概览

约 60 条路由，主要分类：

| 分类 | 路径前缀 | 说明 |
|------|----------|------|
| 健康检查 | `/api/health/*` | liveness / readiness |
| Session 管理 | `/api/sessions/*` | 创建/列出/关闭/命令/状态/事件流 |
| 审计链 | `/api/sessions/{id}/audit/*` | 报告/验证/导出/导入/因果链追溯 |
| 时间机器 | `/api/sessions/{id}/rewind`, `/diff` | 回溯/对比/分支 |
| 调试 | `/api/sessions/{id}/debug/*` | phase/queue/pending_io |
| 规则校验 | `/api/rules/validate` | JSON Schema 校验（core/rule_schema） |
| 规则包 | `/api/bundles/*` | 导入/列出/回滚（0.3.0） |
| 权限 | `/api/permissions/*` | 权限管理（0.3.0） |
| Workspace | `/api/workspaces/*` | 多租户工作空间（需启用） |
| 指标 | `/metrics` | Prometheus 格式 |
| OpenAPI | `/api/openapi.json`, `/api/docs` | OpenAPI 3.1 规范 + Swagger UI |

完整路由定义见 `src/api/server.rs`。

---

## 安全约束

- `#![forbid(unsafe_code)]`（C4）
- 测试代码外禁止 `unwrap`/`expect`/`panic`（C5）
- Bearer token 认证使用恒定时间比较（防止时序攻击）
- 空 token 过滤（防止 `ct_eq("", "")` 返回 true 的空 token 通过认证）
- SSRF 防护：HttpHandler 禁用 HTTP 重定向跟随（B1），默认禁止 loopback 地址
- POST /api/rules/reload 移入认证保护（B2）
- 输入清洗：`input_sanitizer.rs` 防止恶意输入注入

---

## 依赖

- `evorule-tcb` / `evorule-reactor` / `evorule-governance` — 核心引擎
- `evorule-io-handlers` — I/O handler 实现（DB/HTTP/Memory）
- `evorule-workspace` — 多租户工作空间
- `evorule-rule-schema` — 规则 Schema 校验
- `evorule-demo-services` — 原生业务服务插件
- `axum` 0.8 + `tokio` — HTTP 框架和异步运行时
- `clap` — CLI 参数解析
- `tracing` — 日志
- `utoipa` + `utoipa-swagger-ui` — OpenAPI 单一真相源
- `prometheus` — 指标收集

---

## 相关文档

- [README.md](../../README.md) — 项目根 README（架构概览、配置参数、API 简表）
- [INTEGRATION_GUIDE.md](../../docs/INTEGRATION_GUIDE.md) — 实战集成指南
- [PITFALLS.md](../../docs/PITFALLS.md) — 踩坑记录与避坑指南
- [RELEASE_PROCESS.md](../../docs/RELEASE_PROCESS.md) — 发布流程
- [GATE_REFERENCE.md](../../GATE_REFERENCE.md) — 门控参考

<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: AGPL-3.0-or-later

  This file is part of EvoRule Server, licensed under GNU Affero General Public License v3 or later.
-->

<div align="center">

# EvoRule Server

**EvoRule 核心的官方 HTTP 服务入口**

> 把 evorule 的确定性反应式执行能力,封装成可远程访问、可监控、可集成的服务

<br>

[![Version](https://img.shields.io/badge/version-0.3.0-green.svg)](Cargo.toml)
[![License](https://img.shields.io/badge/license-AGPL--3.0-blue.svg)](LICENSE)
[![Status](https://img.shields.io/badge/status-stable--release--v0.3.0-brightgreen.svg)](CHANGELOG.md)
[![Built with](https://img.shields.io/badge/built--with-Axum%200.8-blue.svg)](https://github.com/tokio-rs/axum)

[快速开始](#快速开始) ·
[架构](#架构) ·
[API 概览](#api-概览) ·
[实战指南](docs/INTEGRATION_GUIDE.md) ·
[避坑记录](docs/PITFALLS.md) ·
[配置](#配置) ·
[部署](#部署) ·
[路线图](#已知限制--路线图)

</div>

---

> ## ✅ v0.3.0 — 稳定发布 (2026-08-26)
>
> 这是 EvoRule Server 仓的**第三个版本**。
> **本仓库独立 release**,不绑核心仓的发布节奏。
>
> v0.3.0 主要变化:核心库依赖 crates.io 0.3.2(tcb / reactor / governance);
> 新增 `core/rule_schema` 规则 Schema 门禁;/api/bundles 规则包 API(导入/列出/回滚);
> /api/permissions 权限 API;plugins/demo-services 插件示例;
> evorule-bundle 快照包共享校验(6 项校验链 + 原子落盘)。
> ⚠️ Breaking:`audit_report()` 与 `GET /api/audit` 返回值改为显式 Result(不再静默退化)。
>
> 本仓库**不是** EvoRule 的核心引擎 —— 核心引擎以 `evorule-tcb` / `evorule-reactor` / `evorule-governance` 形式发布到 crates.io。本仓的定位是**框架的官方 HTTP server 实现** + server 配套的 lib(auth / io_handlers / metrics / hot_reload / debug_control / semantic_invariants / time_machine / rule_tools / workspace)。
>
> **使用风险自负**。issue / PR 欢迎,但不保证响应时间。

---

## 一句话定位

**EvoRule Server = 把 evorule 核心跑成 HTTP 服务。**

核心引擎提供 `execute_transition` 纯函数 + 反应器运行时;本仓提供 HTTP 入口、Session 管理、审计流、Prometheus 指标、认证、I/O handler 编排、调试控制。

**适合谁用**:

- 想把 evorule 集成进现有系统的**集成商 / SRE**
- 需要远程 session 管理的**规则工程师**
- 需要审计流 SSE 接口的**审计员 / 合规官**
- 想要 devops 友好(Docker / Prometheus / Grafana / OpenAPI)的**运维**

---

## 架构

```
┌─────────────────────────────────────────────────────────────┐
│                  evorule-server 进程 (单端口 18080)            │
├─────────────────────────────────────────────────────────────┤
│  axum HTTP (18080) + /metrics 端点 (Prometheus 抓取)          │
├─────────────────────────────────────────────────────────────┤
│  Session API     Audit SSE       Debug API    Metrics        │
│  /api/sessions/...   /api/sessions/{id}/events   /api/sessions/{id}/debug/...  │
├─────────────────────────────────────────────────────────────┤
│  evorule-server (本仓)  ← 编排 + 路由 + 状态                  │
│  ├── core/io_handlers     DB / HTTP / Memory 适配器           │
│  ├── core/auth            Bearer + 速率限制 + 恒定时间比较    │
│  ├── core/metrics         Prometheus 指标(7 个核心 metric)    │
│  ├── core/hot_reload      rules/ 目录监控 + 零停机重载       │
│  ├── core/debug_control   pause / resume / step / inspect    │
│  ├── core/semantic_invariants  规则一致性自检                  │
│  ├── core/time_machine    rewind / diff / fork                │
│  ├── core/rule_tools      规则脚手架 + 校验                   │
│  ├── core/rule_schema     规则 Schema 门禁 (v0.3.0 新增)     │
│  ├── core/workspace       多租户工作空间 + 规则元数据管理     │
│  └── plugins/              业务服务插件 (v0.3.0 新增)         │
├─────────────────────────────────────────────────────────────┤
│  evorule 核心 (crates.io 依赖)                               │
│  ├── evorule-tcb      纯函数执行 + 类型安全                   │
│  ├── evorule-reactor  反应式 runtime + 哈希链 WAL             │
│  └── evorule-governance  SessionManager + Auditor + time_machine  │
└─────────────────────────────────────────────────────────────┘
```

**关键约束**:

- 本仓**不改核心引擎代码** —— 核心引擎变更走 crates.io release
- 本仓**独立 release**,不绑其他仓的发布节奏
- 本仓的 publish 状态:`evorule-server` `publish = false`(应用层,不进 crates.io);`core/*` lib 同样 `publish = false`(内部 lib,跟随 server 仓发布)

---

## 快速开始

### 1. 编译

```bash
git clone https://gitee.com/evorule/evorule-server.git
cd evorule-server
cargo build --release
```

### 2. 启动

**最小启动（基础 session / 审计 / 时间机器 / 规则热重载）**——默认监听 `0.0.0.0:18080`,数据存 `./data/`:

```bash
./target/release/evorule-server
```

> ⚠️ **启用 `call_external` / `call_service`（角色 1/3 外部服务调用）必须显式挂载服务注册表**,否则 `ServiceRegistry` 为空,任何 `service_name` 调用都会返回 `unknown service_name`:
>
> ```bash
> ./target/release/evorule-server \
>   --service-registry ./service_registry.json \
>   --allow-loopback
> ```
>
> 仓库已内置 `service_registry.json`(含 `echo_svc` / `llm_advisor` 示例)与 `echo_server.py` 演示后端。可用 `dev-start.sh` 一键拉起完整本地演示环境,并跑通 `role13_demo` 端到端验证。详见[实战指南](docs/INTEGRATION_GUIDE.md)。

### 3. 第一个 session

```bash
# 健康检查
curl http://localhost:18080/api/health

# 创建 session(无需请求体)
curl -X POST http://localhost:18080/api/sessions

# 提交命令(set counter=1)
curl -X POST http://localhost:18080/api/sessions/<session_id>/command \
  -H "Content-Type: application/json" \
  -d '{"instruction":{"type":"set","params":{"attr":"counter","operation":"set","value":1}}}'

# 查询状态
curl http://localhost:18080/api/sessions/<session_id>/state
```

完整路由见源码 `src/api/server.rs`（约 50 条）。

---

## API 概览

| 路径                                        | 方法      | 说明                     |
| ------------------------------------------- | --------- | ------------------------ |
| `/api/health`                               | GET       | 健康检查                 |
| `/api/health/liveness`                      | GET       | 存活检查（始终 200）     |
| `/api/health/readiness`                     | GET       | 就绪检查（退出期间 503） |
| `/api/sessions`                             | POST/GET  | 创建/列出 session        |
| `/api/sessions/{id}`                        | DELETE    | 关闭 session             |
| `/api/sessions/{id}/command`                | POST      | 提交命令                 |
| `/api/sessions/{id}/state`                  | GET       | 当前状态快照             |
| `/api/sessions/{id}/events`                 | GET (SSE) | 事件流（含心跳）         |
| `/api/sessions/{id}/audit`                  | GET       | 审计报告                 |
| `/api/sessions/{id}/audit/verify`           | GET       | 验证审计链完整性         |
| `/api/sessions/{id}/audit/export`           | GET       | 导出审计链 JSON          |
| `/api/sessions/{id}/audit/import`           | POST      | 导入审计链（覆盖）       |
| `/api/sessions/{id}/audit/causal/{fact_id}` | GET       | 因果链追溯               |
| `/api/sessions/{id}/rewind`                 | GET       | 时间回溯（`?version=N`） |
| `/api/sessions/{id}/diff`                   | GET       | 时点对比（`?a=N&b=M`）   |
| `/api/sessions/fork/{parent_id}`            | POST      | 时点分支（`?version=N`） |
| `/api/sessions/{id}/debug/phase`            | GET       | 当前 phase               |
| `/api/sessions/{id}/debug/queue`            | GET       | 队列状态                 |
| `/api/sessions/{id}/debug/pending_io`       | GET       | 待处理 I/O               |
| `/api/sessions/{id}/interrupt`              | POST      | 中断反应器               |
| `/api/sessions/{id}/snapshot`               | GET       | 完整快照                 |
| `/api/rules/validate`                       | POST      | 规则校验                 |
| `/api/bundles/import`                       | POST      | 导入规则包（6 项校验+原子落盘） (v0.3.0) |
| `/api/bundles/import/dry-run`               | POST      | 规则包导入预检（只校验不落盘） (v0.3.0)  |
| `/api/bundles/active`                       | GET       | 当前激活规则包列表 (v0.3.0) |
| `/api/bundles/imports`                      | GET       | 规则包导入历史 (v0.3.0)  |
| `/api/permissions`                          | GET/POST  | 权限管理 (v0.3.0)        |
| `/api/services`                             | GET       | 已绑定服务列表 (v0.3.0)  |
| `/api/workspaces`                           | POST/GET  | 创建/列出工作空间         |
| `/api/workspaces/{id}/rules`                | GET/POST  | 工作空间规则 CRUD         |
| `/api/workspaces/{id}/rules/{rule_id}/versions` | GET  | 规则版本管理              |
| `/api/workspaces/{id}/sandbox`              | POST      | 沙盒试运行                |
| `/metrics`                                  | GET       | Prometheus 指标          |

完整路由（约 60 条）见源码 `src/api/server.rs` + `src/api/bundles.rs` + `src/api/permissions.rs`。

### 深入阅读

- [实战集成指南](docs/INTEGRATION_GUIDE.md) — I/O handler 架构、session 生命周期、审计链完整使用、规则编写实战要点、本地开发环境搭建
- [踩坑记录与避坑指南](docs/PITFALLS.md) — 15 个实际集成中遇到的坑（I/O 超时 / SSRF / HTTP 头语义 / 规则 path / TCB 类型限制等），每个含现象、根因、修复方案

---

## 配置

配置加载优先级：**CLI 参数 > 环境变量（前缀 `EVORULE_`）> JSON 配置文件 > 内置默认值**。

### 环境变量 / CLI 参数

| 变量                      | CLI 参数            | 默认                         | 说明                                    |
| ------------------------- | ------------------- | ---------------------------- | --------------------------------------- |
| `EVORULE_CONFIG`          | `--config`          | (无)                         | JSON 配置文件路径                       |
| `EVORULE_ADDR`            | `--addr`            | `0.0.0.0:18080`              | 监听地址                                |
| `EVORULE_AUTH_TOKEN`      | `--auth-token`      | (空)                         | Bearer token（留空 = 关闭认证，仅 dev） |
| `EVORULE_SERVICE_TOKEN`   | `--service-token`   | (空)                         | 受信服务管道 token（service 身份，可写受保护域 `stable.llm`/`stable.system`；仅认证启用时生效，B5） |
| `EVORULE_CORE_EVAL`       | `--core-eval`       | `./resources/core_eval.json` | 宪法文件路径（不可热重载）              |
| `EVORULE_RULES_DIR`       | `--rules-dir`       | `./rules`                    | 业务规则目录（热重载监听）              |
| `EVORULE_DB_PATH`         | `--db-path`         | `./data/evorule.db`          | SQLite 数据库路径                       |
| `EVORULE_MEMORY_DIR`      | `--memory-dir`      | `./data/memory`              | Memory handler 存储目录                 |
| `EVORULE_MAX_ROUNDS`      | `--max-rounds`      | `1000`                       | 反应器最大指令执行步数                  |
| `EVORULE_LOG_LEVEL`       | `--log-level`       | `info`                       | tracing 级别                            |
| `EVORULE_LOG_FORMAT`      | `--log-format`      | `plain`                      | 日志格式（`plain` / `json`）            |
| `EVORULE_LOG_FILE`        | `--log-file`        | (空)                         | 日志文件路径（不设则仅输出 stderr）     |
| `EVORULE_WAL_DIR`         | `--wal-dir`         | (空)                         | WAL 目录（指定后启用持久化）            |
| `EVORULE_WAL_FSYNC`       | `--wal-fsync`       | `false`                      | 每次 WAL 写入后 fsync                   |
| `EVORULE_WAL_MAX_SIZE_MB` | `--wal-max-size-mb` | `100`                        | 单个 WAL 文件最大大小（0 = 不轮换）     |
| `EVORULE_AUTO_VERIFY`     | `--auto-verify`     | `false`                      | 审计链实时验证                          |
| `EVORULE_NO_RATE_LIMIT`   | `--no-rate-limit`   | `false`                      | 禁用速率限制（仅 benchmark）            |
| `EVORULE_ALLOWED_ORIGINS` | `--allowed-origins` | (空) | CORS 允许的 Origin 列表（逗号分隔;空 = 仅同源;`*` = 全放行，仅开发）。需配合启动参数，否则浏览器跨源请求会被拒;vite dev 可用 `proxy` 绕过 |
| `EVORULE_WORKSPACE_DB`    | `--workspace-db`    | `./data/workspace.db`        | Workspace 元数据库路径（P10, 独立于业务 db_path） |
| `EVORULE_LOG_MAX_DAYS`    | `--log-max-days`    | `7`                          | 日志文件保留天数                  |
| `EVORULE_LOG_MAX_SIZE_MB` | `--log-max-size-mb` | `1024`                       | 日志目录最大占用空间（MB）        |
| `EVORULE_AUTO_VERIFY_THRESHOLD` | `--auto-verify-threshold` | `1000`            | 审计条目数超过此值时跳过验证（0 = 不限制） |
| `EVORULE_AUTO_VERIFY_INTERVAL` | `--auto-verify-interval` | `1`               | 每 N 次 audit_new 验证一次        |
| `EVORULE_SERVICE_REGISTRY` | `--service-registry` | (空)                        | service_name→URL 映射文件（call_service/call_external 用）;**不配置则注册表为空,所有外部服务调用报 `unknown service_name`** |
| `EVORULE_STATEMENT_WHITELIST` | `--statement-whitelist` | (空)                  | SQL 语句模板白名单文件（未设置则 QUERY_DB 全部返回错误） |
| `EVORULE_ALLOW_LOOPBACK`  | `--allow-loopback`  | `false`                      | 允许 HTTP handler 访问 loopback 地址（仅本地开发, 生产禁用） |
| `EVORULE_METRICS_AUTH`    | `--metrics-auth`    | `false`                      | 启用 /metrics 端点认证（需 Bearer token） |
| `EVORULE_OPENAPI_UI`      | `--openapi-ui`      | `false`                      | 挂载 Swagger UI（`GET /api/docs`;`/api/openapi.json` 始终可用） |
| `EVORULE_ALLOW_ABORT`     | `--allow-abort`     | `false`                      | 启用强制中止会话端点（`POST /api/sessions/{id}/abort`, 默认 404） |

### JSON 配置文件

```bash
evorule-server --config evorule.json
```

```json
{
  "server": { "addr": "0.0.0.0:18080", "max_rounds": 1000 },
  "auth": { "token": "<your-bearer-token>" },
  "paths": {
    "core_eval": "./resources/core_eval.json",
    "rules_dir": "./rules",
    "db_path": "./data/evorule.db",
    "memory_dir": "./data/memory",
    "wal_dir": "./data/wal"
  },
  "log": { "level": "info", "format": "json", "file": "./logs/evorule.log" }
}
```

文件不存在或解析失败时降级为纯 CLI/环境变量启动（仅 warn 日志，不报错）。

---

## 部署

### Docker(推荐)

```bash
docker build -t evorule-server:0.3.0 .
docker run -d --name evorule-server -p 18080:18080 -v $(pwd)/data:/data evorule-server:0.3.0
```

### 二进制

```bash
cargo build --release
./target/release/evorule-server
```

### 性能基准(参考)

| 场景                | 吞吐              | 备注               |
| ------------------- | ----------------- | ------------------ |
| 单 session 顺序命令 | 5000 cmd/s        | bench_determinism  |
| 多 session 并发     | 800 cmd/s/session | bench_throughput   |
| 100k 命令长 session | 1.2 GB WAL        | bench_long_session |

具体数字见 `examples/bench_*` 跑出来的结果。

---

## 已知限制 / 路线图

| 项                                       | 状态     | 说明                                  |
| ---------------------------------------- | -------- | ------------------------------------- |
| `cargo build --release` 编译时间         | ~3-4 min | cold build                            |
| 启动时间(冷启动)                         | ~2s      | 含 WAL 校验                           |
| OpenAPI 单一真相源                       | ✅       | `GET /api/openapi.json`, Swagger UI 需 `--openapi-ui` 显式开启 |
| 多租户工作空间 (`core/workspace`)        | ✅       | P10 阶段, 规则 CRUD + 版本 + 沙盒     |
| 输入净化 (Prompt 注入防御, Phase 1)      | ✅       | `InputSanitizer` 公共服务, 静默改写   |
| API 版本化 (`/api/v1/` 锁定)             | ❌       | 1.0 之前不承诺                        |
| 多反应器协作原语                         | ❌       | 路线图                                |
| 第三方安全审计                           | ❌       | 1.0 之前不做                          |
| 集群模式 (cluster/)                      | ❌       | 已弃用,见 commit 历史                 |
| 外部服务调用 (call_external/call_service) | ✅       | 需挂载 `--service-registry` + (本地) `--allow-loopback`;内置 `service_registry.json` 与 `echo_server.py` 演示后端,`role13_demo` 已端到端验证 |

当前以本节"已知限制 / 路线图"表格为准。

---

## 依赖关系

本仓依赖以下 crates.io 包（核心引擎）：

- `evorule-tcb` — 纯函数执行 + 类型安全
- `evorule-reactor` — 反应式 runtime + 哈希链 WAL
- `evorule-governance` — SessionManager + Auditor + time_machine

本仓**独立发布**，不绑核心仓的发布节奏。

---

## 贡献

见 [CONTRIBUTING.md](CONTRIBUTING.md)。

---

## 许可证

EvoRule Server 采用 **AGPL + 商业授权双轨许可**(与[核心仓](https://gitee.com/evorule/evorule)一致):

- **代码(本仓所有 Rust 代码)**:AGPL-3.0-or-later(见 [LICENSE](LICENSE)),为闭源商业/白标场景提供**商业许可**,详见 [DUAL_LICENSE.md](DUAL_LICENSE.md)/ [COMMERCIAL_LICENSE.md](COMMERCIAL_LICENSE.md);政府/学术界/非营利可申请免费豁免,见 [FREE_COMMERCIAL_LICENSE.md](FREE_COMMERCIAL_LICENSE.md)
- **文档**:`docs/` 下文档以 CC-BY-4.0 发布,本 README 顶部为 AGPL 头部
- **宪法**:`resources/core_eval.json` 采用 CC0 1.0 公共领域(自核心仓同步)

---

## 联系方式

- 邮箱:<evorulelab@gmail.com>
- Gitee:[@evorulelab](https://gitee.com/evorule)

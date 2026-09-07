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

[![Version](https://img.shields.io/badge/version-0.5.0-green.svg)](Cargo.toml)
[![License](https://img.shields.io/badge/license-AGPL--3.0-blue.svg)](LICENSE)
[![Status](https://img.shields.io/badge/status-stable--release--v0.5.0-brightgreen.svg)](CHANGELOG.md)
[![Built with](https://img.shields.io/badge/built--with-Axum%200.8-blue.svg)](https://github.com/tokio-rs/axum)

[快速开始](#快速开始) ·
[认证](#认证与用户体系) ·
[架构](#架构) ·
[API 概览](#api-概览) ·
[实战指南](docs/INTEGRATION_GUIDE.md) ·
[避坑记录](docs/PITFALLS.md) ·
[配置](#配置) ·
[部署](#部署) ·
[路线图](#已知限制--路线图)

</div>

---

> ## ✅ v0.5.0 — 稳定发布 (2026-09-06)
>
> 本仓库**独立 release**,不绑其他仓的发布节奏;版本号只与本仓 [CHANGELOG](CHANGELOG.md) 对应。
>
> **0.5.0**:🔒 接口认证显式豁免(⚠️ 回环默认行为变更)——loopback + 无 token 时必须显式声明 `--insecure-serve`(或 `EVORULE_INSECURE_SERVE=1`)才允许无认证启动,否则拒绝启动并给三选一自诊断指引;旧 0.4.x 的回环隐式无认证豁免取消(非 loopback 的 fail-closed 硬拒不放松)。体验包启动脚本/文档同步显式声明,行为不变、声明显式化。市场接口注释与实际认证语义对齐。
>
> **0.4.2**:发版链补全——方案 A 全自动分发包流水线(双平台零依赖包:server.exe + rule-serve 0.3.1 配套 + web 静态 + 规则 + 启动脚本 + 中文说明,gitee/github 双 Release 自动回传);Docker 镜像 smoke 修复(fail-closed 安全防护与镜像默认配置冲突定因,smoke 显式传临时 token)。server 运行时行为零变更。
>
> **0.4.1**:核心引擎依赖 0.4.0 → 0.4.1;`GET /api/sessions/:id/diff` 版本不可达由"空 diff"改为 `400 BAD_REQUEST`;宪法文件更名 `core_eval.json` → `server_eval.json`(启动期旧名兼容检测,不静默回退)。
>
> **0.4.0**:核心引擎升级(单会话长跑 O(n²) 性能缺陷修复,实测 10000 命令会话 51s 全程平坦;⚠️ WAL 事实格式升级单向);**平台用户体系与统一认证**(bootstrap 首启 / 登录 / 用户 / 角色 / 权限点,业务 API 双凭据中间件);审计档案只读 API(从 WAL 重建历史会话审计链);插件清单三级配置(`--plugins`);`physics-services` / `indicator-services` 两个确定性原生插件;模板市场 / 服务端 PDF 导出 / 执行侧知识通道 API 族;负载演练与性能基准三件套;AGPL + 商业双许可体系。
>
> **版本策略**:生态内各仓版本号**独立发展、独立发布**,互不强求一致——核心仓(evorule)技术定位稳定、几乎不改动,本仓与其他各仓快速演进。
>
> 本仓库**不是** EvoRule 的核心引擎 —— 核心引擎以 `evorule-tcb` / `evorule-reactor` / `evorule-governance` 形式发布到 crates.io。本仓的定位是**框架的官方 HTTP server 实现** + server 配套的 lib(auth / io_handlers / metrics / hot_reload / debug_control / semantic_invariants / time_machine / rule_tools / rule_schema / workspace)。
>
> **使用风险自负**。issue / PR 欢迎,但不保证响应时间。

---

## 一句话定位

**EvoRule Server = 把 evorule 核心跑成 HTTP 服务。**

核心引擎提供 `execute_transition` 纯函数 + 反应器运行时;本仓提供 HTTP 入口、Session 管理、审计流、Prometheus 指标、认证与用户体系、I/O handler 编排、调试控制、模板市场与导出。

**适合谁用**:

- 想把 evorule 集成进现有系统的**集成商 / SRE**
- 需要远程 session 管理的**规则工程师**
- 需要审计流 SSE 接口、审计档案与因果追溯的**审计员 / 合规官**
- 需要多用户 / 角色权限管理的**平台管理员**
- 想要 devops 友好(Docker / Prometheus / Grafana / OpenAPI / Swagger UI)的**运维**

---

## 架构

```
┌─────────────────────────────────────────────────────────────┐
│                  evorule-server 进程 (单端口 18080)            │
├─────────────────────────────────────────────────────────────┤
│  axum HTTP (18080) + /metrics 端点 (Prometheus 抓取)          │
├─────────────────────────────────────────────────────────────┤
│  Session API      Audit SSE        Debug API     Metrics     │
│  /api/sessions/…  /api/sessions/{id}/events   /api/sessions/{id}/debug/… │
├─────────────────────────────────────────────────────────────┤
│  evorule-server (本仓)  ← 编排 + 路由 + 状态                  │
│  ├── api/server        session / 审计 / 时间机器 / 调试 路由  │
│  ├── api/platform_auth 平台用户体系 + 统一认证中间件(双凭据)   │
│  ├── api/permissions   权限点注册表 + 提交/评审流              │
│  ├── api/bundles       规则包导入(6 项校验+原子落盘)           │
│  ├── api/marketplace   模板市场(CRUD + 在线编辑 + 下载)        │
│  ├── api/pdf_export    服务端 PDF 导出(中文字体子集嵌入)       │
│  ├── api/knowledge     执行侧数据资产只读通道                  │
│  ├── api/openapi       OpenAPI 单一真相源                      │
│  ├── api/hit_stats     规则命中统计聚合器 + 查询面             │
│  ├── core/io_handlers     DB / HTTP / Memory 适配器           │
│  ├── core/auth            Bearer + 速率限制 + 恒定时间比较    │
│  ├── core/metrics         Prometheus 指标(7 个核心 metric)    │
│  ├── core/hot_reload      rules/ 目录监控 + 零停机重载       │
│  ├── core/debug_control   pause / resume / step / inspect    │
│  ├── core/semantic_invariants  规则一致性自检                  │
│  ├── core/time_machine    rewind / diff / fork                │
│  ├── core/rule_tools      规则脚手架 + 校验                   │
│  ├── core/rule_schema     规则 Schema 门禁                    │
│  ├── core/plugin-kit      插件路由机制件(插件为薄壳具名委托)   │
│  ├── core/workspace       多租户工作空间 + 规则元数据管理     │
│  └── plugins/              业务服务插件(demo / physics / indicator) │
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
- publish 状态:`evorule-server` 与 `core/*` 内部 lib 均 `publish = false`(应用层,不进 crates.io)

---

## 快速开始

### 1. 编译

```bash
git clone https://gitee.com/evorule/evorule-server.git
cd evorule-server
cargo build --release
```

### 2. 启动

**最小启动(基础 session / 审计 / 时间机器 / 规则热重载)**——默认监听 `0.0.0.0:18080`,数据存 `./data/`:

```bash
./target/release/evorule-server
```

> ⚠️ **启用 `call_external` / `call_service`(角色 1/3 外部服务调用)必须显式挂载服务注册表**,否则 `ServiceRegistry` 为空,任何 `service_name` 调用都会返回 `unknown service_name`:
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

### 4. 平台认证(多用户场景)

单机 / 内嵌场景可跳过此节(不配置认证 = 全开放,仅限开发)。生产部署推荐启用平台用户体系:

```bash
# 首启创建管理员(仅当平台无用户时可用,否则 409)
curl -X POST http://localhost:18080/api/platform/auth/bootstrap \
  -H "Content-Type: application/json" \
  -d '{"username":"admin","password":"<至少8位>","display_name":"管理员"}'

# 登录获取会话 token
curl -X POST http://localhost:18080/api/platform/auth/login \
  -H "Content-Type: application/json" \
  -d '{"username":"admin","password":"<密码>"}'
```

业务 API 由统一认证中间件保护,**双凭据**:静态 Bearer token(`--auth-token`)或平台会话 token 任一均可;统一 401 语义。`bootstrap` / `login` / `auth/status` 公开,其余平台端点需平台 token。演示场景可开 `--demo-auth`(体验包默认开,生产建议关闭)。

完整路由(OpenAPI 收录 **84 条**,另有工作空间路由族)见 `GET /api/openapi.json`;Swagger UI 需 `--openapi-ui` 显式开启(`GET /api/docs`)。

---

## API 概览

> 下表为人工梳理的主要端点;**单一真相源是 `GET /api/openapi.json`**(84 条路径)。

### 健康与元信息

| 路径 | 方法 | 说明 |
| --- | --- | --- |
| `/api/health` | GET | 健康检查(响应含插件挂载事实) |
| `/api/health/liveness` | GET | 存活检查(始终 200) |
| `/api/health/readiness` | GET | 就绪检查(退出期间 503) |
| `/api/openapi.json` | GET | OpenAPI 文档(单一真相源) |

### Session 与执行

| 路径 | 方法 | 说明 |
| --- | --- | --- |
| `/api/sessions` | POST/GET | 创建 / 列出 session |
| `/api/sessions/{id}` | GET/DELETE | 详情 / 关闭 |
| `/api/sessions/{id}/command` | POST | 提交命令 |
| `/api/sessions/{id}/io_response` | POST | 回应外部 I/O 请求 |
| `/api/sessions/{id}/payload` | POST | 更新 payload |
| `/api/sessions/{id}/state` | GET | 当前状态快照 |
| `/api/sessions/{id}/snapshot` | GET | 完整快照 |
| `/api/sessions/{id}/events` | GET (SSE) | 事件流(含心跳) |
| `/api/sessions/{id}/finished` | GET | 会话是否已终结 |
| `/api/sessions/reap` | POST | 清理已终结会话 |
| `/api/command` ` /api/payload` ` /api/state` ` /api/audit` | POST/GET | 单反应器模式便捷端点(向后兼容,免 session id) |

### 时间机器

| 路径 | 方法 | 说明 |
| --- | --- | --- |
| `/api/sessions/{id}/rewind` | GET | 时间回溯(`?version=N`) |
| `/api/sessions/{id}/diff` | GET | 时点对比(`?a=N&b=M`;版本不可达 = 400) |
| `/api/sessions/{id}/history` | GET | 版本历史 |
| `/api/sessions/{id}/replay` | GET | 重放 |
| `/api/sessions/fork/{parent_id}` | POST | 时点分支(`?version=N`) |
| `/api/sessions/from/{parent_id}` | POST | 从父会话派生 |

### 调试控制

| 路径 | 方法 | 说明 |
| --- | --- | --- |
| `/api/sessions/{id}/debug/phase` | GET | 当前 phase |
| `/api/sessions/{id}/debug/queue` | GET | 队列状态 |
| `/api/sessions/{id}/debug/pending_io` | GET | 待处理 I/O |
| `/api/sessions/{id}/pending_io_count` | GET | 待处理 I/O 计数 |
| `/api/sessions/{id}/step` | GET | 单步执行 |
| `/api/sessions/{id}/interrupt` | POST | 中断反应器 |
| `/api/sessions/{id}/abort` | POST | 强制中止(**默认 404**,需 `--allow-abort` 显式开启) |
| `/api/sessions/{id}/invariants` | GET | 语义不变量自检结果 |
| `/api/sessions/{id}/causal_depth` | GET | 因果深度 |

### 审计

| 路径 | 方法 | 说明 |
| --- | --- | --- |
| `/api/sessions/{id}/audit` | GET | 审计报告(支持 `include_content`) |
| `/api/sessions/{id}/audit/verify` | GET | 验证审计链完整性 |
| `/api/sessions/{id}/audit/export` | GET | 导出审计链 JSON(另有 `/compressed`) |
| `/api/sessions/{id}/audit/import` | POST | 导入审计链(另有 `/compressed`) |
| `/api/sessions/{id}/audit/auto_verify` | GET | 自动验证状态 |
| `/api/sessions/{id}/audit/causal/{fact_id}` | GET | 因果链追溯 |
| `/api/audit-archive/sessions` | GET | 审计档案:从 WAL 重建历史会话列表(只读) |
| `/api/audit-archive/sessions/{id}/audit` | GET | 审计档案:历史会话审计链 |
| `/api/audit/platform-events` | GET | 平台认证事件报表(只读派生事实链) |

### 事实与共享事实

| 路径 | 方法 | 说明 |
| --- | --- | --- |
| `/api/sessions/{id}/facts` | GET | 按前缀查询会话事实 |
| `/api/shared/facts` | GET | 共享事实查询 |
| `/api/shared/facts/version` | GET | 共享事实版本 |
| `/api/shared/facts/rollup` | POST | 共享事实汇总 |
| `/api/shared/facts/{fact_id}/source` ` /used_by` | GET | 事实来源 / 消费方追溯 |
| `/api/sessions/{id}/used_at_startup` | GET | 启动期使用的事实 |

### 平台认证 / 用户 / 角色(0.4.0)

| 路径 | 方法 | 说明 |
| --- | --- | --- |
| `/api/platform/auth/bootstrap` | POST | 首启创建管理员(已有用户则 409) |
| `/api/platform/auth/login` / `logout` | POST | 登录 / 登出 |
| `/api/platform/auth/me` / `status` | GET | 当前用户 / 认证状态 |
| `/api/platform/auth/change-password` | POST | 改密 |
| `/api/platform/users` ` /users/{username}` | GET/POST/PATCH/DELETE | 用户管理 |
| `/api/platform/roles` ` /roles/{name}` | GET/POST/… | 角色管理 |
| `/api/platform/permissions` | GET | 权限点注册表 |

### 权限 / 规则包 / 规则

| 路径 | 方法 | 说明 |
| --- | --- | --- |
| `/api/permissions` | GET/POST | 权限列表 / 创建 |
| `/api/permissions/evaluate` | POST | 权限评估 |
| `/api/permissions/{id}/submit` ` /review` | POST | 提交 / 评审 |
| `/api/bundles/import` | POST | 导入规则包(6 项校验+原子落盘) |
| `/api/bundles/import/dry-run` | POST | 导入预检(只校验不落盘) |
| `/api/bundles/active` | GET | 当前激活规则包 |
| `/api/bundles/imports` | GET | 导入历史 |
| `/api/rules` | GET | 当前规则集 |
| `/api/rules/validate` | POST | 规则校验 |
| /api/rules/reload | POST | 热重载 |
| `/api/rules/hit-stats` | GET | 规则命中统计清单(`filter=all/hit/zero`,`version` 指定版本) |
| `/api/rules/hit-stats/{rule_key}` | GET | 单规则跨版本命中切片(`rule_key`=`{index}@{source}`) |
ule_key={index}@{source}) |

### 规则命中统计

运行时命中信号:聚合器消费审计链的命中归因事实,按 `规则集版本 × 来源 × 规则下标` 聚合。

- 存储为进程内存态,**重启后计数归零**;审计链 WAL 是全量权威源
- 规则集内容或来源任一变化即产生新版本号,reload 后历史版本切片保留(上限 8 个)
- **零命中清单**(`filter=zero`)即死规则候选——静默通过规则清剿的数据源

Prometheus 指标(`/metrics`):

| 指标 | 类型 | 标签 | 含义 |
| --- | --- | --- | --- |
| `evorule_rule_hits_total` | counter | `rule`(下标) / `source`(`core_eval` 或相对路径) / `version` | 规则结构命中次数(直接指令执行成功 / branch 所选分支存在且非空 / io_request 产生信号) |
| `evorule_rules_zero_hits` | gauge | — | 当前版本零命中规则数 |

示例 PromQL:

```promql
# 活跃规则命中速率(按来源)
rate(evorule_rule_hits_total[5m])
# 当前版本死规则数(零命中 gauge)
evorule_rules_zero_hits
```

> 注:从未命中的规则在 `evorule_rule_hits_total` 中**没有时间序列**(counter 仅在命中时创建),完整死规则清单走查询端点 `filter=zero`。

容量口径:审计链每命令 +1 条命中归因事实(记录性,不推进版本号),单条常态 <1KB;WAL 轮换沿用 `--wal-max-size-mb`。

### 数据与服务

| 路径 | 方法 | 说明 |
| --- | --- | --- |
| `/api/knowledge` | GET | 数据集列表(执行侧只读通道) |
| `/api/knowledge/{ds}/entries` ` /{entry_id}` | GET | 条目查询 |
| `/api/services` | GET | 已绑定服务列表(native / plugin / registry 三来源,含参数契约) |
| `/api/export/pdf` | POST | 服务端 PDF 导出(纯 Rust 文本型,中文字体子集嵌入;body 上限 32MB) |
| `/api/marketplace/templates` | GET/POST | 模板市场:列表 / 上传 |
| `/api/marketplace/templates/{id}` | GET/PATCH/DELETE | 模板详情 / 在线编辑 / 删除 |
| `/api/marketplace/templates/{id}/download` | GET | 模板下载 |
| `/api/workspaces` 族 | GET/POST/PATCH/DELETE | 多租户工作空间 + 成员 + 规则 CRUD / 版本 / 激活 / 沙盒 |
| `/metrics` | GET | Prometheus 指标(需 `--metrics-auth` 时带 Bearer) |

### 深入阅读

- [实战集成指南](docs/INTEGRATION_GUIDE.md) — I/O handler 架构、session 生命周期、审计链完整使用、规则编写实战要点、本地开发环境搭建
- [踩坑记录与避坑指南](docs/PITFALLS.md) — 15 个实际集成中遇到的坑(I/O 超时 / SSRF / HTTP 头语义 / 规则 path / TCB 类型限制等),每个含现象、根因、修复方案

---

## 认证与用户体系

两层机制,可单独或组合使用:

| 层 | 启用方式 | 凭据 | 适用 |
| --- | --- | --- | --- |
| 静态 token | `--auth-token <t>` | Bearer token | 单管理员 / 服务间调用 |
| 平台用户体系 | bootstrap 首启即启用 | 平台会话 token | 多用户 / 角色 / 审计需求 |

- **双凭据**:启用后业务 API 接受静态 token **或** 平台会话 token;统一 401 语义
- 受信服务管道:`--service-token`(service 身份,可写受保护域 `stable.llm` / `stable.system`)
- 认证事件全部落审计事实链,可经 `/api/audit/platform-events` 报表查询
- `/metrics` 可独立要求认证(`--metrics-auth`)

---

## 配置

配置加载优先级:**CLI 参数 > 环境变量(前缀 `EVORULE_`)> JSON 配置文件 > 内置默认值**。

### 环境变量 / CLI 参数

| 变量                      | CLI 参数            | 默认                         | 说明                                    |
| ------------------------- | ------------------- | ---------------------------- | --------------------------------------- |
| `EVORULE_CONFIG`          | `--config`          | (无)                         | JSON 配置文件路径                       |
| `EVORULE_ADDR`            | `--addr`            | `0.0.0.0:18080`              | 监听地址                                |
| `EVORULE_AUTH_TOKEN`      | `--auth-token`      | (空)                         | Bearer token(留空 = 关闭认证,仅 dev) |
| `EVORULE_SERVICE_TOKEN`   | `--service-token`   | (空)                         | 受信服务管道 token(service 身份,可写受保护域 `stable.llm`/`stable.system`;仅认证启用时生效) |
| `EVORULE_CORE_EVAL`       | `--core-eval`       | `./resources/server_eval.json` | 宪法文件路径(不可热重载)              |
| `EVORULE_RULES_DIR`       | `--rules-dir`       | `./rules`                    | 业务规则目录(热重载监听)              |
| `EVORULE_DB_PATH`         | `--db-path`         | `./data/evorule.db`          | SQLite 数据库路径                       |
| `EVORULE_MEMORY_DIR`      | `--memory-dir`      | `./data/memory`              | Memory handler 存储目录                 |
| `EVORULE_MAX_ROUNDS`      | `--max-rounds`      | `1000`                       | 反应器最大指令执行步数                  |
| `EVORULE_LOG_LEVEL`       | `--log-level`       | `info`                       | tracing 级别                            |
| `EVORULE_LOG_FORMAT`      | `--log-format`      | `plain`                      | 日志格式(`plain` / `json`)            |
| `EVORULE_LOG_FILE`        | `--log-file`        | (空)                         | 日志文件路径(不设则仅输出 stderr)     |
| `EVORULE_WAL_DIR`         | `--wal-dir`         | (空)                         | WAL 目录(指定后启用持久化;未配置时启动有数据风险警示) |
| `EVORULE_WAL_FSYNC`       | `--wal-fsync`       | `false`                      | 每次 WAL 写入后 fsync                   |
| `EVORULE_WAL_MAX_SIZE_MB` | `--wal-max-size-mb` | `100`                        | 单个 WAL 文件最大大小(0 = 不轮换)     |
| `EVORULE_AUTO_VERIFY`     | `--auto-verify`     | `false`                      | 审计链实时验证                          |
| `EVORULE_NO_RATE_LIMIT`   | `--no-rate-limit`   | `false`                      | 禁用速率限制(仅 benchmark)            |
| `EVORULE_ALLOWED_ORIGINS` | `--allowed-origins` | (空) | CORS 允许的 Origin 列表(逗号分隔;空 = 仅同源;`*` = 全放行,仅开发) |
| `EVORULE_WORKSPACE_DB`    | `--workspace-db`    | `./data/workspace.db`        | Workspace 元数据库路径(独立于业务 db_path) |
| `EVORULE_LOG_MAX_DAYS`    | `--log-max-days`    | `7`                          | 日志文件保留天数                  |
| `EVORULE_LOG_MAX_SIZE_MB` | `--log-max-size-mb` | `1024`                       | 日志目录最大占用空间(MB)        |
| `EVORULE_AUTO_VERIFY_THRESHOLD` | `--auto-verify-threshold` | `1000`            | 审计条目数超过此值时跳过验证(0 = 不限制) |
| `EVORULE_AUTO_VERIFY_INTERVAL` | `--auto-verify-interval` | `1`               | 每 N 次 audit_new 验证一次        |
| `EVORULE_SERVICE_REGISTRY` | `--service-registry` | (空)                        | service_name→URL 映射文件(call_service/call_external 用);**不配置则注册表为空,所有外部服务调用报 `unknown service_name`** |
| `EVORULE_PLUGINS`         | `--plugins`         | (空)                         | 插件清单文件(未配置 = 进程内原生插件全部启用;见[插件清单](#插件清单)) |
| `EVORULE_STATEMENT_WHITELIST` | `--statement-whitelist` | (空)                  | SQL 语句模板白名单文件(未设置则 QUERY_DB 全部返回错误) |
| `EVORULE_ALLOW_LOOPBACK`  | `--allow-loopback`  | `false`                      | 允许 HTTP handler 访问 loopback 地址(仅本地开发, 生产禁用) |
| `EVORULE_METRICS_AUTH`    | `--metrics-auth`    | `false`                      | 启用 /metrics 端点认证(需 Bearer token) |
| `EVORULE_OPENAPI_UI`      | `--openapi-ui`      | `false`                      | 挂载 Swagger UI(`GET /api/docs`;`/api/openapi.json` 始终可用) |
| `EVORULE_ALLOW_ABORT`     | `--allow-abort`     | `false`                      | 启用强制中止会话端点(`POST /api/sessions/{id}/abort`, 默认 404) |
| `EVORULE_DEMO_AUTH`       | `--demo-auth`       | `false`                      | 演示登录入口开关(体验包默认开;生产建议关) |
| `EVORULE_WEB_DIR`         | `--web-dir`         | (空)                         | 静态前端托管目录(SPA 回退 index.html;不设则不托管) |

### JSON 配置文件

```bash
evorule-server --config evorule.json
```

```json
{
  "server": { "addr": "0.0.0.0:18080", "max_rounds": 1000 },
  "auth": { "token": "<your-bearer-token>" },
  "paths": {
    "core_eval": "./resources/server_eval.json",
    "rules_dir": "./rules",
    "db_path": "./data/evorule.db",
    "memory_dir": "./data/memory",
    "wal_dir": "./data/wal",
    "plugins": "./plugin_manifest.json"
  },
  "log": { "level": "info", "format": "json", "file": "./logs/evorule.log" }
}
```

文件不存在或解析失败时降级为纯 CLI/环境变量启动(仅 warn 日志,不报错)。

### 插件清单

插件支持**部署期启用/裁剪**:通过清单文件声明各插件启用集,改清单 + 重启即生效(不做运行时热启停——运行时热变更与确定性审计链的兼容性未论证)。清单支持两类条目:

- **builtin 条目**——进程内原生插件(`plugins/` 下各 crate,如 `demo-services`、`physics-services`、`indicator-services`):`enabled` + 可选 `services` 子集;
- **external 条目**——外部插件包(独立进程 + 自持数据 + `plugin.json` 清单,任意语言实现,装入/拔出零宿主代码改动):`enabled` + `manifest` 指向插件包清单;规范与开发指引见[《插件开发指南》](docs/PLUGIN_GUIDE.md)。

```bash
evorule-server --plugins ./plugin_manifest.json
```

清单文件形态(多插件,键 = 插件 id;builtin 条目 `services` 省略 = 该插件全部服务启用,显式列出 = 子集启用,未列出的插件全启;external 条目 `manifest` = 插件包 plugin.json 路径,相对清单文件所在目录解析):

```json
{
  "plugins": {
    "demo-services": {
      "enabled": true,
      "services": ["inverse_kinematics_solver", "llm_advisor", "robot_move_joints",
                   "shadow_ik_solver", "sampling_service", "rule_sandbox", "config_persist"]
    },
    "physics-services": {
      "enabled": true,
      "services": ["physics_simulate", "physics_energy", "physics_grav_band"]
    },
    "indicator-services": {
      "enabled": true,
      "services": ["indicator_sma", "indicator_ema", "indicator_macd", "indicator_rsi"]
    },
    "finance-config": {
      "enabled": true,
      "manifest": "plugins/finance-config/plugin.json"
    }
  }
}
```

语义约定:

| 清单写法 | 行为 |
|---|---|
| 未配置 `--plugins`(缺省) | 全部原生插件/服务启用;外部插件包不装载(显式安装语义)——存量部署零迁移 |
| `enabled: true` + `services` 省略 | 该插件全部服务启用(builtin) |
| `enabled: true` + `services` 列出子集 | 仅启用列出的服务;未启用服务名回落 HTTP 注册表(`--service-registry`) |
| `enabled: true` + `manifest` 指向 plugin.json | 装入外部插件包,声明服务派生为路由条目(与注册表条目同管道 HTTP 回落) |
| `enabled: false` | 不挂载该插件,`call_service`/`call_external` 直连 HTTP 注册表 |

校验为 **fail-fast**(启动期拒绝,不静默忽略):清单文件不可读、JSON 非法、未知插件 id、`services` 为空、服务名未注册/重复声明,均报错退出并附自诊断指引(合法服务名清单、修复路径)。external 条目另有三拒绝校验:plugin.json 不可读/JSON 非法/id 漂移/空服务集/base_url 非 http(s) 拒绝装载,服务名与内置/注册表/其他外部包冲突拒绝装载。

**运行可见性**:`GET /api/health` 响应含 `plugins` 节,如实呈现启动期挂载事实——

```json
{
  "success": true,
  "message": "ok",
  "plugins": {
    "demo-services": { "enabled": true, "services": ["config_persist"] },
    "physics-services": { "enabled": true, "services": ["physics_energy"] },
    "indicator-services": { "enabled": true, "services": ["indicator_sma"] },
    "finance-config": { "enabled": true, "external": true, "services": ["finance_config_get", "finance_config_set"] }
  }
}
```

**新增原生插件/服务** = 新建(或在既有)插件 crate 的 `NATIVE_SERVICES` 声明表追加服务项 + 在 `src/main.rs` 的 `PLUGIN_DEFS` 登记表登记声明表指针(清单解析/挂载链/健康可见性机制代码零改动;路由器机制件由 [`core/plugin-kit`](core/plugin-kit) 公共 crate 提供,插件为薄壳具名委托)——部署方按需在清单中启用;详见 [plugins/demo-services/README.md](plugins/demo-services/README.md)、[plugins/physics-services/README.md](plugins/physics-services/README.md)、[plugins/indicator-services/README.md](plugins/indicator-services/README.md)。

**新增外部插件包** = 实现独立 HTTP 服务进程 + 编写 plugin.json + 清单登记一行(零宿主代码改动、零重编,任意语言可实现);规范/调用契约/管理面/装卸操作详见[《插件开发指南》](docs/PLUGIN_GUIDE.md)。未打包为插件的既有 HTTP 服务经 `--service-registry` 声明文件直接绑定接入。

---

## 部署

### 升级与兼容

- **升级顺序**:本仓与核心引擎 crate(`evorule-tcb` / `evorule-reactor`)的审计链事实格式同步演进——升级本仓时**必须同时升级核心引擎依赖**,勿混用新旧版本组合。
- **WAL 向前兼容性**:审计链对新事实类型的反序列化策略为**显式拒绝**(fail-fast)——旧版进程读取含新事实类型的 WAL 会报 `InvalidFact(unknown fact type: ...)` 并拒绝启动,**不会静默丢弃或跳过**;遇到该错误的处置 = 将进程升级到与 WAL 写入方同代或更新的版本。
- 升级前建议备份 `data/`(WAL 与数据库);命中归因事实为记录性数据,升级后统计计数从零重新聚合(历史全量以审计链为准)。

### Docker(推荐)

```bash
docker build -t evorule-server:0.5.0 .
docker run -d --name evorule-server -p 18080:18080 -v $(pwd)/data:/data -e EVORULE_AUTH_TOKEN=<your-secret> evorule-server:0.5.0
```

### 二进制

```bash
cargo build --release
./target/release/evorule-server
```

### 性能基准(参考,实测时点数据)

| 场景                | 吞吐              | 基准代码 |
| ------------------- | ----------------- | -------- |
| 单 session 顺序命令 | 5000 cmd/s        | `evorule-server/examples/bench_determinism.rs` |
| 多 session 并发     | 800 cmd/s/session | `evorule-server/examples/bench_throughput.rs` |
| 100k 命令长 session | 1.2 GB WAL        | `evorule-server/examples/bench_long_session.rs` |

负载演练脚本见 `scripts/load-drill.ps1`。0.4.0 修复单会话长跑 O(n²) 缺陷后,10000 命令会话 51s 全程平坦(修复前同规模推算需数十小时)。

---

## 已知限制 / 路线图

| 项                                       | 状态     | 说明                                  |
| ---------------------------------------- | -------- | ------------------------------------- |
| `cargo build --release` 编译时间         | ~3-4 min | cold build                            |
| 启动时间(冷启动)                         | ~2s      | 含 WAL 校验                           |
| 平台用户体系 + 统一认证(双凭据)          | ✅       | bootstrap / 登录 / 用户 / 角色 / 权限点;统一 401 |
| 审计档案(WAL 重建历史会话,只读)          | ✅       | `/api/audit-archive/*` + 平台事件报表 |
| 插件部署期启用/裁剪(`--plugins` 三级配置) | ✅       | fail-fast 校验 + `/api/health` 挂载事实 |
| 确定性原生插件(physics / indicator)      | ✅       | 辛积分器物理内核;pandas 逐位对齐技术指标 |
| 模板市场 / 服务端 PDF 导出 / 知识通道    | ✅       | marketplace CRUD+在线编辑;PDF 中文字体子集;`/api/knowledge` 只读 |
| OpenAPI 单一真相源                       | ✅       | `GET /api/openapi.json`(84 条);Swagger UI 需 `--openapi-ui` |
| 多租户工作空间 (`core/workspace`)        | ✅       | 工作空间 / 成员 / 规则 CRUD + 版本 + 激活 |
| 输入净化 (Prompt 注入防御)               | ✅       | `InputSanitizer` 公共服务, 静默改写   |
| API 版本化 (`/api/v1/` 锁定)             | ❌       | 1.0 之前不承诺                        |
| 多反应器协作原语                         | ❌       | 路线图                                |
| 插件运行时热启停                         | ❌       | 与确定性审计链的兼容性未论证,只做部署期配置 |
| 第三方安全审计                           | ❌       | 1.0 之前不做                          |
| 集群模式 (cluster/)                      | ❌       | 已弃用,见 commit 历史                 |

当前以本节"已知限制 / 路线图"表格为准。

---

## 依赖关系

本仓依赖以下 crates.io 包(核心引擎):

- `evorule-tcb` — 纯函数执行 + 类型安全
- `evorule-reactor` — 反应式 runtime + 哈希链 WAL
- `evorule-governance` — SessionManager + Auditor + time_machine

本仓**独立发布**,不绑核心仓的发布节奏。

---

## 贡献

见 [CONTRIBUTING.md](CONTRIBUTING.md)。

> Issue 与 PR 请提交到 [Gitee](https://gitee.com/evorule/evorule-server)。

---

## 许可证

EvoRule Server 采用 **AGPL + 商业授权双轨许可**(与[核心仓](https://gitee.com/evorule/evorule)一致):

- **代码(本仓所有 Rust 代码)**:AGPL-3.0-or-later(见 [LICENSE](LICENSE));闭源商业/白标场景提供**商业许可**,详见 [DUAL_LICENSE.md](DUAL_LICENSE.md) / [COMMERCIAL_LICENSE.md](COMMERCIAL_LICENSE.md);政府/学术界/非营利可申请免费豁免,见 [FREE_COMMERCIAL_LICENSE.md](FREE_COMMERCIAL_LICENSE.md)
- **文档**:`docs/` 下文档以 CC-BY-4.0 发布,本 README 顶部为 AGPL 头部
- **宪法(server 业务规则集)**:`resources/server_eval.json` 采用 CC0 1.0 公共领域(0.4.1 前旧名 `core_eval.json`;与核心仓宪法原则职责不同、独立演进)

---

## 联系方式

- 邮箱:<evorulelab@gmail.com>
- Gitee:[@evorule](https://gitee.com/evorule)

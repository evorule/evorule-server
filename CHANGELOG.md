<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: AGPL-3.0-or-later
-->

# EvoRule Server 更新日志

所有对 EvoRule Server 仓的重大更改都将记录在此文件中。

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.0.0/) v1.0,
本项目遵循 [语义化版本控制](https://semver.org/lang/zh-CN/) v2.0。

徽章说明:

- 🆕 新增
- 🔄 变更
- 🐛 修复
- 🗑 弃用
- ⚠️ Breaking Change
- 🔒 安全
- 🧪 测试
- ✅ 向后兼容
- 📚 文档

---

## [Unreleased]

（待发布内容将在版本发布时移入对应版本段）

---

## [0.3.0] - 2026-08-26

### ⚠️ Breaking Changes

- **`audit_report()` 返回值变更** — `evorule-server/src/api/server.rs` `GovernanceApi::audit_report()` 从 `String` 改为 `Result<String, serde_json::Error>`，不再静默退化为 `"{}"`（同步 evorule v0.3.2）
- **`GET /api/audit` handler 返回值变更** — `get_audit()` 从 `Json<serde_json::Value>` 改为 `Result<Json<Value>, StatusCode>`，序列化失败时返回 500

### 🆕 新增

- **`core/rule_schema` crate** — 规则 Schema 门禁（线1 防御层），`/api/rules/validate` 提交校验的权威基准。含 3 个 JSON Schema 文件（rule_set / _meta / _shared v1.0）和 Rust 验证库（19KB）
- **`/api/bundles` 规则包 API** — `evorule-server/src/api/bundles.rs`（31KB）：规则包导入、列出活跃包、导入历史、原子落盘（`land_bundle_atomically`）、回滚（`rollback_bundle_moves`）、陈旧目录清理
- **`/api/permissions` 权限 API** — `evorule-server/src/api/permissions.rs`（10KB）：权限管理端点
- **`plugins/demo-services` 插件示例** — Rust 原生业务服务实现（复合路由：原生优先，HTTP 回落），Phase 1 yuanze-demos，7 个原生服务（ik_solver / llm_advisor / robot_move / rule_sandbox / sampling / shadow_validate / config_persist）
- **`rules/bundles/` 规则包示例** — `bundle-ds-yuanze-01-v3`（15 条规则：审计告警/压缩/计算/演进扫描/生成补丁/热加载/机器人移动/安全回滚/采样决策/沙盒验证/影子验证/精度验证）+ `b_guard_shell_risky` 安全规则包
- **服务注册 API** — `list_services_handler` + `BoundServiceInfo` 结构体，列出已绑定服务及其元数据（名称/来源/版本/描述）
- **`service_registry.json`** — 服务注册配置文件
- **`scripts/check_schema_sync.py`** — Schema 同步检查脚本，确保规则 Schema 与代码一致
- **`evorule-bundle` 依赖** — 快照包共享校验（T2：36 号集成契约；6 项校验链 + 逐条 Schema 门禁 + 原子落盘），version 0.2.0

### 🔄 变更

- **核心库依赖 crates.io** — evorule-tcb / reactor / governance 统一使用 crates.io v0.3.2（含 permission 模块 / io_context 数据型），evorule-bundle v0.2.0；发布时移除本地 `[patch.crates-io]` path 覆盖
- **workspace members 新增** — 根 `Cargo.toml` 新增 `core/rule_schema` 和 `plugins/demo-services`
- **`evorule-server/Cargo.toml` 新增依赖** — `evorule-rule-schema`（path）、`evorule-bundle`（0.2.0）、`evorule-demo-services`（path）
- **`resources/core_eval.json` 同步更新** — 同步 evorule v0.3.2 宪法变更

### 🐛 修复

- **元指令白名单修正同步** — `increment` / `noop` transform 类型被拒绝（之前误混入白名单导致假阳性），测试用例重命名为 `test_validate_increment_transform_type_rejected` / `test_validate_noop_transform_rejected`
- **set 非法 operation 提升为 error** — 从 warn 不阻断改为 rejected 阻断，测试用例重命名为 `test_validate_invalid_operation_rejected`
- **workspace 模块多项修复** — `publish_service.rs`（发布队列状态机修复）、`rule_translate.rs`（规则翻译边界修复）、`sandbox_service.rs`（沙盒编排修复）、`rolling_session.rs`（滚动 session 修复）、`session_bridge.rs`（会话桥接修复）、`workspace_service.rs` / `rule_meta_service.rs` / `models.rs` / `db.rs` / `lib.rs`
- **io_handlers 多项修复** — `db_handler.rs`（SQL 注入防护增强）、`http_handler.rs`（SSRF 防护增强）、`memory_handler.rs`（key 长度校验）、`service_registry.rs`（URL scheme 校验）、`lib.rs`
- **hot_reload 修复** — `loader.rs`（规则加载器修复）、`lib.rs`（删除事件语义说明）
- **rule_tools 修复** — `validator.rs`（校验器修复）、`lib.rs`、`bin/evorule-rule-tools.rs`
- **集成测试更新** — `alignment_test.rs` / `integration_test.rs` 同步 API 变更

### 📚 文档

- **10 个缺失文档的 crate 新增 README** — `core/workspace` / `evorule-server` / `core/io_handlers` / `core/auth` / `core/debug_control` / `core/hot_reload` / `core/metrics` / `core/rule_tools` / `core/semantic_invariants` / `core/time_machine`（此前 12 个 crate 中 10 个完全无文档）
- **`core/rule_schema/README.md`** — 规则 Schema 门禁完整说明
- **`plugins/demo-services/README.md`** — 7 个原生服务说明 + 复合路由设计
- **`docs/INTEGRATION_GUIDE.md`** — meta 指令 4→6 种、新增 rule_schema 校验说明、新增 §6 规则包 API、§7 权限 API
- **`GATE_REFERENCE.md`** — 新增 §3.4 rule_schema Schema 完整性门禁、§3.5 plugins 门控、更新 crate 列表
- **`docs/PITFALLS.md`** — 新增坑 20-23（audit_report 返回值变更、元指令白名单修正、patch.crates-io 发布注意、规则包原子回滚）
- **`docs/RELEASE_PROCESS.md`** — 检查项 7→8 项（新增 check_schema_sync.py）、新增 [patch.crates-io] 段检测、子 crate 数量 9→11
- **`README.md` / `DOCS_INDEX.md` / `CHANGELOG.md`** — 同步更新

---

## [0.2.0] - 2026-08-19

> **本版本实际打 tag 日期: 2026-08-19**
>
> 2026-08-10 起 CHANGELOG 段已预写但未实际打 tag, 期间累积了实际 release
> 内容 (workspace 模块 + OpenAPI 单一真相源 + InputSanitizer + 核心库 0.3.1
> 升级 + gitee URL 迁移), 2026-08-19 一次打 tag.

### 🔒 安全

- **B1: HttpHandler 禁用 HTTP 重定向跟随（SSRF 绕过防护）**
  - `core/io_handlers/src/http_handler.rs` `build_client()` 加 `.redirect(reqwest::redirect::Policy::none())`
  - 旧实现 reqwest 默认跟随最多 10 次重定向，SSRF 防护只校验原始 URL 的 DNS 解析结果，
    重定向后的目标 IP 不再校验。攻击者可配置公网 URL → 302 → 169.254.169.254（云元数据）绕过 SSRF 防护
  - 禁用后 3xx 响应作为 Err 返回上层，由调用方决定处理方式（行业最佳实践）
- **B2: `POST /api/rules/reload` 移入认证保护**
  - `evorule-server/src/api/server.rs` 将 reload 路由从 `public_routes` 移到 `protected_routes`
  - 旧实现该端点无认证，攻击者可反复触发规则重载造成 DoS，或当 rules_dir 可写时注入恶意规则
  - 现在需要 `Authorization: Bearer <token>` 头，无认证返回 401
- **B3: 无认证 + 非 loopback 地址时 fail-closed 拒绝启动**
  - `evorule-server/src/main.rs` 无 token 且绑定非 loopback 地址时 `error!` + `exit(1)`
  - 旧实现仅 `warn!` 不阻止启动，公网部署时若用户漏看日志，所有 session 数据完全暴露
  - loopback 地址（127.0.0.1 / [::1]）仍允许无认证启动供本地开发；地址解析失败视为非 loopback（安全侧失败）

### 🆕 新增

- **`core/workspace` crate 首次纳入版本控制** — 多租户工作空间 + 规则元数据管理（P10 基础设施层），含 14 个源文件：
  - `rule_translate.rs`（36KB）：BusinessRule ↔ evorule 核心 6 域类型双向翻译引擎
  - `api.rs`（36KB）：Workspace HTTP API 端点（规则 CRUD / 版本管理 / 审计链 / 沙盒）
  - `db.rs`（96KB）：SQLite 持久化层（工作区 / 规则元数据 / 沙盒报告）
  - `workspace_service.rs` / `rule_meta_service.rs` / `publish_service.rs` / `sandbox_service.rs` / `verdict_service.rs` / `rolling_session.rs` / `session_bridge.rs` / `session_switched.rs` / `mock_io_responder.rs` / `test_report.rs` / `models.rs` / `error.rs` / `lib.rs`
  - 依赖：rusqlite (bundled) + serde + blake3 + chrono + ulid + axum 0.8 + tokio
- **Workspace API 端点** — `evorule-server/src/api/server.rs` 新增 +355 行：workspace 路由组（创建/列举/删除工作区、规则 CRUD、版本管理、审计链拉取、沙盒试运行）
- **Workspace CLI 参数** — `evorule-server/src/main.rs` 新增 +138 行：`--workspace-db` / `--workspace-root` / `--enable-workspace-api` 等启动参数
- **Workspace 集成测试** — `evorule-server/tests/session_integration_test.rs` 新增 +54 行：workspace API 端到端测试
- **OpenAPI 单一真相源 (P2-1)** — `evorule-server/src/api/openapi.rs` (新, 179 行):
  `utoipa::OpenApi` derive 聚合 server 全部 handler, 运行时经
  `GET /api/openapi.json` 导出 OpenAPI 3.1 规范, 前端通过
  `openapi-typescript` 自动生成类型 (杜绝手写 schema 与代码漂移).
  Swagger UI 端点 `GET /api/docs` 通过 `--openapi-ui` 显式开启
  (默认关闭避免生产暴露接口面)
- **强制中止会话端点** — `evorule-server/src/api/server.rs`:
  `POST /api/sessions/{id}/abort` 端点 (014 合法 API #4) 通过 `--allow-abort`
  CLI 参数显式开启, 默认 404 (双保险: 即使认证通过也需显式开启)
- **InputSanitizer 第一层输入净化 (Phase 1)** — `evorule-server/src/input_sanitizer.rs`
  (来自 feature/agent-prompt-impl 分支合并, 749 行 + clippy 修复): Prompt 注入
  防御公共服务, 静默改写 "ignore previous instructions" / "you are now" /
  "system: ..." / "act as admin" 等常见攻击模式, 18 类正则规则, 19 个单测覆盖
- **--openapi-ui / --allow-abort CLI 参数** — `evorule-server/src/main.rs`:
  两个破坏性/暴露性端点的双保险开关, 默认关闭

### 🔄 变更

- **核心库 0.2.1 → 0.3.1** — `evorule-server/Cargo.toml` / `core/io_handlers/Cargo.toml`:
  evorule-tcb / evorule-reactor / evorule-governance 三项核心依赖从 0.2.1 升到 0.3.1
  (核心仓 v0.3.x 已 cargo publish 到 crates.io)
- **新增 utoipa + utoipa-swagger-ui 依赖** — `evorule-server/Cargo.toml`:
  引入 OpenAPI 单一真相源 (`utoipa = "5"` + `utoipa-swagger-ui = "9"`)
- **核心 workspace 加 utoipa 依赖** — `core/workspace/Cargo.toml`: 标注模型
  用于 OpenAPI 导出 (`utoipa = { version = "5", features = ["axum_extras", "chrono"] }`)
- **移除 `[patch.crates-io]` 段** — 根 `Cargo.toml` 之前为本地开发覆盖 evorule-*
  路径的 `[patch.crates-io]` 段移除, release 用户不再误用本地路径
- **内部 crate 版本号统一 workspace 继承** — 9 个内部 crate（auth / debug_control / hot_reload / io_handlers / metrics / rule_tools / semantic_invariants / time_machine / evorule-server）的 `version = "0.1.0"` 改为 `version.workspace = true`，统一继承 workspace.package.version = 0.2.0，以后 bump 一处即可
- **workspace Cargo.toml 注册新成员** — 根 `Cargo.toml` `[workspace].members` 新增 `core/workspace`
- **gitee 仓 owner 迁移** — `evo-rule-lab` → `evorule`:
  - 本仓 (`evorule-server`): `evo-rule-lab/evorule-server` → `evorule/evorule-server` (11 处)
  - 核心仓 (`evorule`): `evo-rule-lab/evorule` → `evorule/evorule` (5 个 L1 文档 10 处)
  - 组织页: `gitee.com/evo-rule-lab` → `gitee.com/evorule` (README + NOTICE 2 处)
  - GitHub 镜像 workflow (`.github/workflows/mirror.yml`) 镜像源 URL 同步

### 🐛 修复

- **N1: AuthConfig 过滤空字符串 token** — `evorule-server/src/auth.rs` `new()` 过滤空 token，防止空 Bearer token 通过认证（`ct_eq("", "")` 返回 true）
- **N2: ServiceRegistry 校验 URL scheme** — `core/io_handlers/src/service_registry.rs` `parse_service_entry()` 解析时校验 scheme 为 http/https，拒绝 file:///data:// 等
- **N3: http_requests_total 指标接入中间件** — `evorule-server/src/api/server.rs` 添加 `http_metrics_middleware`，用 `normalize_path_for_metrics` 把数字段归一化为 `{id}` 防止 Prometheus 基数爆炸
- **N4: hot_reload 支持 auth_token 配置** — `core/hot_reload/src/config.rs` 增加 `auth_token` 字段，`create_session`/`send_rules` 注入 `Authorization: Bearer` 头；bin 加 `--auth-token` CLI 参数
- **N5: HttpHandler::new_dev_allow_loopback 保留不改** — 评估后跳过：evorule-io-handlers 是 `publish = false` 内部 crate，main.rs 的 `--allow-loopback` 已有"生产环境永远不要启用"文档警告
- **N6: MemoryHandler 限制 key 长度** — `core/io_handlers/src/memory_handler.rs` `execute()` 检查 key ≤ 255 字节，防止超长 key 触发 OS 文件名错误
- **gte/gt 域类型翻译** — `core/workspace/src/rule_translate.rs`：evorule 核心仅支持 eq/lt/exists/instruction/all/not 6 域类型，server 端将 gte 翻译为 `not(lt)`、gt 翻译为 `not(all([lt,eq]))`，并实现对称回译（not(lt)→gte、not(all([lt,eq]))→gt），确保 onboarding 创建的 gte/gt 规则通过 G4 校验
- **action_set 角色丢失** — `core/workspace/src/rule_translate.rs` `translate_to_transform`：未正确处理 `action_set` 中的 value 字段，导致动作角色（role）信息丢失。修复后 value 字段正确包含 role 信息
- **G5 校验白名单遗漏** — `core/workspace/src/rule_translate.rs` + `evorule-console/src/lib/validators/ruleValidator.ts`：`__exec__.result.notify` 不在 G5 白名单，新增 `__exec__.result.*` 路径前缀，支持执行结果引用
- **params.path vs params.attr 不一致** — `core/workspace/src/rule_translate.rs`：`translate_to_transform` 生成 `params.path`，而 evorule core `exec_set` 读取 `params.attr`，导致 set 动作静默失败。统一为 `params.attr`

### ✅ 向后兼容

- **S1: hot_reload 删除事件语义说明** — `core/hot_reload/src/lib.rs` 检测到 `ChangeType::Remove` 时输出 `warn!` 日志，明确告知"hot_reload 仅支持增量添加规则，删除文件不会从 server 移除已有规则，如需清除旧规则请重启 session"。旧实现删除文件时静默无提示，用户误以为规则已被移除
- **S2: /metrics 端点可选认证** — `evorule-server/src/main.rs` 新增 `--metrics-auth` / `EVORULE_METRICS_AUTH` CLI 参数；`api/server.rs` `GovernanceServer` 新增 `metrics_requires_auth` 字段，独立构建 `metrics_router`，启用时挂载 `auth_middleware`。默认关闭（Prometheus scraper 通常不带 token），启用后 `/metrics` 也需 `Authorization: Bearer <token>` 头
- **S3: CORS 通配符 origin 检测** — `evorule-server/src/api/server.rs` `build_router()` 检测 `allowed_origins` 包含 `"*"` 时输出 `warn!`，提示"CORS 规范禁止通配符 + credentials 组合，浏览器会拒绝此响应，请使用精确 Origin 列表替代"
- **S4: time_machine 版本间隙测试覆盖** — `core/time_machine/src/lib.rs` 新增 9 个测试覆盖版本间隙（version gap）场景：首条记录前间隙、Command 被忽略产生间隙、ST 与 IoResponse 间间隙、多间隙全返回 None、间隙边界返回 Some、local_diff 间隙版本退化为空对象、build_version_tree 稀疏版本 total_versions 正确性、build_batch_diff 跨间隙配对

### 🧪 测试

- **CI 验证全绿** — 实际打 tag 时: `cargo check --workspace --all-targets` 0 error,
  `cargo clippy --workspace --all-targets -- -D warnings` 0 warning, `cargo fmt --all
  -- --check` 通过, `cargo test --workspace --all-features` 32 个测试组全 ok

---

## [0.1.0] - 2026-07-30

**evorule-server 仓首次建立** — 本仓独立 release。
本仓从应用层迁出 9 个 server 配套 crate,作为 EvoRule 框架的官方 server 实现。

### 🆕 新增

- **新仓建立** — evorule-server 仓 git init,主分支 `main`
- **Cargo workspace 顶层配置**
  - members: `core/{auth, debug_control, hot_reload, io_handlers, metrics, rule_tools, semantic_invariants, time_machine}` + `evorule-server`
  - workspace.package: `version = "0.1.0"` / `edition = "2021"` / `license = "AGPL-3.0-or-later"` / `authors = ["EvoRule Project"]` / `repository = "https://gitee.com/evorule/evorule-server"` / `rust-version = "1.74"`
  - workspace.lints: `unwrap_used/expect_used/panic/panic_in_result_fn` deny + `cognitive_complexity/too_many_lines/type_complexity/module_inception` warn
  - workspace.lints.rust: `unexpected_cfgs` warn (兼容 kani cfg)
- **物理迁入 9 个 crate** (从应用层迁入)
  - `core/auth` (799 KB, 0 publish, 仅本仓内)
  - `core/debug_control` (566 KB, 0 publish)
  - `core/hot_reload` (1.6 MB, 0 publish)
  - `core/io_handlers` (1.2 MB, 0 publish, evorule-server 唯一依赖)
  - `core/metrics` (1.3 MB, 0 publish, Prometheus 实现)
  - `core/rule_tools` (366 KB, 0 publish, 规则脚手架)
  - `core/semantic_invariants` (566 KB, 0 publish)
  - `core/time_machine` (1.3 MB, 0 publish, rewind/diff/fork)
  - `evorule-server` (6.2 MB 源码, 主 bin, 0 publish, 约 50 条路由)
- **新仓门面文件**
  - `README.md` (9.6 KB) — 介绍 evorule-server 仓的定位、架构、快速开始、API 概览、配置、部署、路线图
  - `CHANGELOG.md` (本文档)
  - `SECURITY.md` (简版,引用标准安全流程)
  - `CONTRIBUTING.md` (简版)
  - `LICENSE` (AGPL-3.0-or-later)
  - `.gitignore` (680 B,覆盖 Rust/IDE/数据文件)

### 🔄 变更

- **Path 依赖调整**
  - `evorule-server/Cargo.toml` `evorule-io-handlers` path: `../io_handlers` → `core/io_handlers`
  - 核心引擎依赖使用 `version = "0.1.0"` 声明 (crates.io 兼容)
- **evorule-server 仓元数据**
  - `description`: "EvoRule 框架官方 HTTP server 实现"
  - `repository`: `https://gitee.com/evorule/evorule-server`
  - 9 个 core/* lib 的 metadata 同步调整

### 🗑 弃用

- **cluster/ 不迁入** — 原 `core/cluster/` 已弃用,新仓不引入

### 📚 文档

- 本仓独立 release，不绑核心仓发布节奏

### ✅ 向后兼容

- evorule-server/Cargo.toml 的 `repository` / `description` 已改为本仓 URL
- 9 个 core/* lib 的 Cargo.toml metadata 已补齐（description + publish=false）
- CI 配置(`.gitee-ci/` / `.github/`)已迁移
- Dockerfile / scripts/build-docker.ps1 已迁移
- cargo check / cargo test / cargo clippy 全跑通（447 passed, 0 failed, clippy -D warnings 0 errors）

---

## 历史

本仓代码最初位于应用层(2026-07-30 之前)。
2026-07-30 之后,所有 commit 都在本仓。

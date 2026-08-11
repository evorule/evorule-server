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
- 🛠 建议级改进

---

## [0.2.0] - 2026-08-10

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

### 🐛 修复

- **N1: AuthConfig 过滤空字符串 token** — `evorule-server/src/auth.rs` `new()` 过滤空 token，防止空 Bearer token 通过认证（`ct_eq("", "")` 返回 true）
- **N2: ServiceRegistry 校验 URL scheme** — `core/io_handlers/src/service_registry.rs` `parse_service_entry()` 解析时校验 scheme 为 http/https，拒绝 file:///data:// 等
- **N3: http_requests_total 指标接入中间件** — `evorule-server/src/api/server.rs` 添加 `http_metrics_middleware`，用 `normalize_path_for_metrics` 把数字段归一化为 `{id}` 防止 Prometheus 基数爆炸
- **N4: hot_reload 支持 auth_token 配置** — `core/hot_reload/src/config.rs` 增加 `auth_token` 字段，`create_session`/`send_rules` 注入 `Authorization: Bearer` 头；bin 加 `--auth-token` CLI 参数
- **N5: HttpHandler::new_dev_allow_loopback 保留不改** — 评估后跳过：evorule-io-handlers 是 `publish = false` 内部 crate，main.rs 的 `--allow-loopback` 已有"生产环境永远不要启用"文档警告
- **N6: MemoryHandler 限制 key 长度** — `core/io_handlers/src/memory_handler.rs` `execute()` 检查 key ≤ 255 字节，防止超长 key 触发 OS 文件名错误

### 🛠 建议级改进 (S1-S4)

- **S1: hot_reload 删除事件语义说明** — `core/hot_reload/src/lib.rs` 检测到 `ChangeType::Remove` 时输出 `warn!` 日志，明确告知"hot_reload 仅支持增量添加规则，删除文件不会从 server 移除已有规则，如需清除旧规则请重启 session"。旧实现删除文件时静默无提示，用户误以为规则已被移除
- **S2: /metrics 端点可选认证** — `evorule-server/src/main.rs` 新增 `--metrics-auth` / `EVORULE_METRICS_AUTH` CLI 参数；`api/server.rs` `GovernanceServer` 新增 `metrics_requires_auth` 字段，独立构建 `metrics_router`，启用时挂载 `auth_middleware`。默认关闭（Prometheus scraper 通常不带 token），启用后 `/metrics` 也需 `Authorization: Bearer <token>` 头
- **S3: CORS 通配符 origin 检测** — `evorule-server/src/api/server.rs` `build_router()` 检测 `allowed_origins` 包含 `"*"` 时输出 `warn!`，提示"CORS 规范禁止通配符 + credentials 组合，浏览器会拒绝此响应，请使用精确 Origin 列表替代"
- **S4: time_machine 版本间隙测试覆盖** — `core/time_machine/src/lib.rs` 新增 9 个测试覆盖版本间隙（version gap）场景：首条记录前间隙、Command 被忽略产生间隙、ST 与 IoResponse 间间隙、多间隙全返回 None、间隙边界返回 Some、local_diff 间隙版本退化为空对象、build_version_tree 稀疏版本 total_versions 正确性、build_batch_diff 跨间隙配对

### 🆕 新增（v0.2.0 里程碑）

- **`core/workspace` crate 首次纳入版本控制** — 多租户工作空间 + 规则元数据管理（P10 基础设施层），含 14 个源文件：
  - `rule_translate.rs`（36KB）：BusinessRule ↔ evorule 核心 6 域类型双向翻译引擎
  - `api.rs`（36KB）：Workspace HTTP API 端点（规则 CRUD / 版本管理 / 审计链 / 沙盒）
  - `db.rs`（96KB）：SQLite 持久化层（工作区 / 规则元数据 / 沙盒报告）
  - `workspace_service.rs` / `rule_meta_service.rs` / `publish_service.rs` / `sandbox_service.rs` / `verdict_service.rs` / `rolling_session.rs` / `session_bridge.rs` / `session_switched.rs` / `mock_io_responder.rs` / `test_report.rs` / `models.rs` / `error.rs` / `lib.rs`
  - 依赖：rusqlite (bundled) + serde + blake3 + chrono + ulid + axum 0.8 + tokio
- **Workspace API 端点** — `evorule-server/src/api/server.rs` 新增 +355 行：workspace 路由组（创建/列举/删除工作区、规则 CRUD、版本管理、审计链拉取、沙盒试运行）
- **Workspace CLI 参数** — `evorule-server/src/main.rs` 新增 +138 行：`--workspace-db` / `--workspace-root` / `--enable-workspace-api` 等启动参数
- **Workspace 集成测试** — `evorule-server/tests/session_integration_test.rs` 新增 +54 行：workspace API 端到端测试

### 🐛 修复（v0.2.0 本次会话）

- **gte/gt 域类型翻译** — `core/workspace/src/rule_translate.rs`：evorule 核心仅支持 eq/lt/exists/instruction/all/not 6 域类型，server 端将 gte 翻译为 `not(lt)`、gt 翻译为 `not(all([lt,eq]))`，并实现对称回译（not(lt)→gte、not(all([lt,eq]))→gt），确保 onboarding 创建的 gte/gt 规则通过 G4 校验
- **action_set 角色丢失** — `core/workspace/src/rule_translate.rs` `translate_to_transform`：未正确处理 `action_set` 中的 value 字段，导致动作角色（role）信息丢失。修复后 value 字段正确包含 role 信息
- **G5 校验白名单遗漏** — `core/workspace/src/rule_translate.rs` + `evorule-console/src/lib/validators/ruleValidator.ts`：`__exec__.result.notify` 不在 G5 白名单，新增 `__exec__.result.*` 路径前缀，支持执行结果引用
- **params.path vs params.attr 不一致** — `core/workspace/src/rule_translate.rs`：`translate_to_transform` 生成 `params.path`，而 evorule core `exec_set` 读取 `params.attr`，导致 set 动作静默失败。统一为 `params.attr`

### 🔄 变更（v0.2.0 依赖同步）

- **核心库依赖版本保持 0.2.1** — `evorule-server/Cargo.toml`：evorule-tcb / evorule-reactor / evorule-governance 三项依赖版本号保持 0.2.1（crates.io 上最新）。核心仓 v0.2.2 已 git tag + push，但尚未 `cargo publish` 到 crates.io，待核心仓 v0.2.2 publish 后单独 bump 依赖版本
- **内部 crate 版本号统一 workspace 继承** — 9 个内部 crate（auth / debug_control / hot_reload / io_handlers / metrics / rule_tools / semantic_invariants / time_machine / evorule-server）的 `version = "0.1.0"` 改为 `version.workspace = true`，统一继承 workspace.package.version = 0.2.0，以后 bump 一处即可
- **workspace Cargo.toml 注册新成员** — 根 `Cargo.toml` `[workspace].members` 新增 `core/workspace`

## [0.1.0] - 2026-07-30

**evorule-server 仓首次建立** — 本仓独立 release。
本仓从应用层迁出 9 个 server 配套 crate,作为 EvoRule 框架的官方 server 实现。

### 🆕 新增

- **新仓建立** — evorule-server 仓 git init,主分支 `main`
- **Cargo workspace 顶层配置**
  - members: `core/{auth, debug_control, hot_reload, io_handlers, metrics, rule_tools, semantic_invariants, time_machine}` + `evorule-server`
  - workspace.package: `version = "0.1.0"` / `edition = "2021"` / `license = "AGPL-3.0-or-later"` / `authors = ["EvoRule Project"]` / `repository = "https://gitee.com/evo-rule-lab/evorule-server"` / `rust-version = "1.74"`
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

- **Path 依赖调整**
  - `evorule-server/Cargo.toml` `evorule-io-handlers` path: `../io_handlers` → `core/io_handlers`
  - 核心引擎依赖使用 `version = "0.1.0"` 声明 (crates.io 兼容)

- **新仓门面文件**
  - `README.md` (9.6 KB) — 介绍 evorule-server 仓的定位、架构、快速开始、API 概览、配置、部署、路线图
  - `CHANGELOG.md` (本文档)
  - `SECURITY.md` (简版,引用标准安全流程)
  - `CONTRIBUTING.md` (简版)
  - `LICENSE` (AGPL-3.0-or-later)
  - `.gitignore` (680 B,覆盖 Rust/IDE/数据文件)

### 🔄 变更

- **evorule-server 仓元数据**
  - `description`: "EvoRule 框架官方 HTTP server 实现"
  - `repository`: `https://gitee.com/evo-rule-lab/evorule-server`
  - 9 个 core/* lib 的 metadata 同步调整

### 🗑 弃用

- **cluster/ 不迁入** — 原 `core/cluster/` 已弃用,新仓不引入

### 📚 文档

- 本仓独立 release，不绑核心仓发布节奏

### ⏳ 已知问题（截至 2026-08-01 全部已解决，保留作历史记录）

- ✅ evorule-server/Cargo.toml 的 `repository` / `description` 已改为本仓 URL
- ✅ 9 个 core/* lib 的 Cargo.toml metadata 已补齐（description + publish=false）
- ✅ CI 配置(`.gitee-ci/` / `.github/`)已迁移
- ✅ Dockerfile / scripts/build-docker.ps1 已迁移
- ✅ cargo check / cargo test / cargo clippy 全跑通（447 passed, 0 failed, clippy -D warnings 0 errors）

---

## 历史

本仓代码最初位于应用层(2026-07-30 之前)。
2026-07-30 之后,所有 commit 都在本仓。

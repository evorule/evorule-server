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

---

## [Unreleased]

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

## [0.1.0] - 2026-07-30

**evorule-server 仓首次建立** — 走神 9 决策:evorule 仓必须独立 release,evorule-server 仓也必须独立 release。
本仓从 evorule-application 仓的 `core/` 物理迁出 9 个 server 配套 crate,作为 EvoRule 框架的官方 server 实现。

### 🆕 新增

- **新仓建立** — evorule-server 仓 git init,主分支 `main`
- **Cargo workspace 顶层配置**
  - members: `core/{auth, debug_control, hot_reload, io_handlers, metrics, rule_tools, semantic_invariants, time_machine}` + `evorule-server`
  - workspace.package: `version = "0.1.0"` / `edition = "2021"` / `license = "AGPL-3.0-or-later"` / `authors = ["EvoRule Project"]` / `repository = "https://gitee.com/evo-rule-lab/evorule-server"` / `rust-version = "1.74"`
  - workspace.lints: `unwrap_used/expect_used/panic/panic_in_result_fn` deny + `cognitive_complexity/too_many_lines/type_complexity/module_inception` warn
  - workspace.lints.rust: `unexpected_cfgs` warn (兼容 kani cfg)

- **物理迁入 9 个 crate** (从 evorule-application 仓的 core/ cp 过来,**原仓文件保留**作为思路)
  - `core/auth` (799 KB, 0 publish, 仅本仓内)
  - `core/debug_control` (566 KB, 0 publish)
  - `core/hot_reload` (1.6 MB, 0 publish)
  - `core/io_handlers` (1.2 MB, 0 publish, evorule-server 唯一依赖)
  - `core/metrics` (1.3 MB, 0 publish, Prometheus 实现)
  - `core/rule_tools` (366 KB, 0 publish, 规则脚手架)
  - `core/semantic_invariants` (566 KB, 0 publish)
  - `core/time_machine` (1.3 MB, 0 publish, rewind/diff/fork)
  - `evorule-server` (6.2 MB 源码, 主 bin, 0 publish, 约 50 条路由)<!-- 勘误:原文"100+ 路由"与实际不符,实际 47 条路由,2026-07-31 订正 -->

- **Path 依赖调整**(新仓 vs 原 evorule-application 仓路径深度变化)
  - `evorule-server/Cargo.toml` 4 个 evorule-* path: `../../../evorule/...` → `../../evorule/...` (从 4 层深变 4 层深,但跨仓更直接)
  - `evorule-server/Cargo.toml` `evorule-io-handlers` path: `../io_handlers` → `core/io_handlers`
  - `core/io_handlers/Cargo.toml` 2 个 evorule-* path: 不变 (3 层深,刚好够)
  - 所有跨仓引用仍用 `path + version = "0.1.0"` 双声明 (本地开发 + crates.io 兼容)

- **完整备份原 evorule-application 仓** — 本地备份目录 `evorule-application-backup-20260730/` (15.3 GB / 47K items)
  - 留作"思路"参考,后续 application 仓瘦身方案基于此 backup 制定
  - target/ 构建产物独立备份到本地目录 `.evorule-server-build-backup-20260730/` (14 GB,9 个 target/)

- **新仓门面文件**
  - `README.md` (9.6 KB) — 介绍 evorule-server 仓的定位、架构、快速开始、API 概览、配置、部署、路线图
  - `CHANGELOG.md` (本文档)
  - `SECURITY.md` (简版,引用 evorule 主仓流程)
  - `CONTRIBUTING.md` (简版)
  - `LICENSE` (AGPL-3.0-or-later)
  - `.gitignore` (680 B,覆盖 Rust/IDE/数据文件)

### 🔄 变更

- **evorule-server 仓元数据**
  - `description` 待调整: 原 "EvoRule 独立二进制服务入口(应用层)" → 新 "EvoRule 框架官方 HTTP server 实现" (TODO)
  - `repository` 待调整: 原 `https://gitee.com/evo-rule-lab/evorule` → 新 `https://gitee.com/evo-rule-lab/evorule-server` (TODO)
  - 9 个 core/* lib 的 metadata 同步调整 (TODO)

### 🗑 弃用

- **cluster/ 不迁入** — 原 evorule-application 仓的 `core/cluster/` 已弃用(H5 之后),留原仓,新仓不引入

### 📚 文档

- 走神 9 决策记录:evorule 仓 + evorule-server 仓 + evorule-application 仓 三仓独立 release
- 走神 10 决策记录:放弃自定 release-blocker 22 项,改用 crates.io 10 项行业惯例
- 分仓规划见内部私有文档（不对外发布）

### ⏳ 已知问题（截至 2026-08-01 全部已解决，保留作历史记录）

- ✅ evorule-server/Cargo.toml 的 `repository` / `description` 已改为本仓 URL
- ✅ 9 个 core/* lib 的 Cargo.toml metadata 已补齐（description + publish=false）
- ✅ CI 配置(`.gitee-ci/` / `.github/`)已迁移
- ✅ Dockerfile / scripts/build-docker.ps1 已迁移
- ✅ cargo check / cargo test / cargo clippy 全跑通（447 passed, 0 failed, clippy -D warnings 0 errors）

---

## 历史

本仓所有历史均在 evorule-application 仓的 `core/` 下的 `core/*` 与 `evorule-server/` 目录中(2026-07-30 之前)。
2026-07-30 之后,所有 commit 都在本仓。

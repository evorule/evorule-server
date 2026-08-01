<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: AGPL-3.0-or-later
-->

# EvoRule Server 路线图

> **最后更新**:2026-08-01
> **当前版本**:v0.1.0(内部基线)
> **版本策略**:见 [VERSION_STRATEGY.md](VERSION_STRATEGY.md)

---

## 概述

evorule-server 是 EvoRule 框架的官方 HTTP server 实现。本路线图描述从内部基线到生产就绪的演进路径。

**核心原则**:本仓**不绑** evorule 主仓的发布节奏,独立 release,独立版本号(走神 9)。

---

## v0.1.x — 内部基线期(当前)

**目标**:建立可运行的 server 基线,覆盖核心 API + 安全防护。

### ✅ 已完成

| 里程碑 | 状态 | 说明 |
|---|---|---|
| 仓初始建立 | ✅ v0.1.0 | 9 个 server 配套 crate 从 evorule-application 迁出,workspace 建立 |
| 三层合规门禁 | ✅ | L1 build.rs 字面量扫描 + L1' `#![forbid(unsafe_code)]` + L2 Clippy workspace lints |
| B1-B3 阻塞级安全修复 | ✅ | SSRF redirect 禁用 / reload 认证保护 / fail-closed 启动 |
| N1-N6 非阻塞改进 | ✅ | 空token过滤 / scheme校验 / metrics接入 / hot_reload认证 / key长度限制 |
| S1-S4 建议级改进 | ✅ | 删除事件语义 / metrics可选认证 / CORS通配符检测 / 版本间隙测试 |

### 🔲 待办

| 里程碑 | 优先级 | 说明 |
|---|---|---|
| P1 安全修复(H6-H9) | HIGH | SSRF DNS rebinding / SQL 注入深化 / CORS 精确配置 / DB URL 注入 — 公网部署前必修 |
| 集成测试完善 | MEDIUM | `evorule-server/tests/` 覆盖核心 API 全路径(mock LLM + mock I/O) |
| Crate 级 README | MEDIUM | 9 个 server 配套 crate 各补 README.md(用途 / API / 依赖) |
| Docker 镜像优化 | LOW | 多阶段构建 + 非 root 用户 + 健康检查 |

---

## v0.2.0 — 第一批用户反馈

**目标**:接收早期用户反馈,补齐实用功能。

**预计周期**:v0.1.x 后 6-8 周

### 计划项

| 里程碑 | 说明 |
|---|---|
| 配置热重载增强 | 支持运行时修改 auth_token / rate_limit / CORS 白名单(不重启) |
| SSE 重连恢复 | 客户端断线后从 `Last-Event-ID` 恢复事件流 |
| 时间机器增强 | fork API(从历史状态分叉新 session)+ replay 可视化数据 |
| 调试 API 完善 | `/debug/*` 端点独立端口控制 + 默认关闭生产环境 |
| 指标增强 | HTTP 延迟分位数(p50/p95/p99)+ session 生命周期指标 |
| 审计链导出 | 支持导出完整审计链为 JSON / CSV(合规场景) |

---

## v0.3.0 — 可运维性

**目标**:支持生产环境运维操作。

### 计划项

| 里程碑 | 说明 |
|---|---|
| WAL 持久化增强 | WAL 文件轮换策略完善(P03)+ 崩溃恢复测试 |
| 优雅退出增强 | 连接 draining + in-flight 请求追踪 |
| 健康检查完善 | readiness 探针区分"启动中"vs"运行中"vs"退出中" |
| 日志轮转 | `tracing-appender` 配置 + 日志保留策略验证 |
| 配置验证 | 启动时校验所有配置项(路径存在 / 权限 / 格式),fail-fast |

---

## v1.0.0 — 生产就绪

**目标**:API 稳定承诺 + 第三方安全审计。

**前置条件**(见 [VERSION_STRATEGY.md §4.4](VERSION_STRATEGY.md)):

- [ ] HTTP API 路径稳定(不再有 breaking change)
- [ ] P1 安全修复全部完成(H6-H9)
- [ ] 第三方安全审计通过(见 VERSION_STRATEGY §4.5)
- [ ] 集成测试覆盖率 ≥ 80%
- [ ] 文档完整(所有 crate README + API 参考文档)
- [ ] Docker 镜像生产级(非 root + 最小镜像 + 健康检查)
- [ ] 至少 1 个真实部署案例

---

## 版本节奏建议

| 阶段 | 频率 | 说明 |
|---|---|---|
| v0.1.x | 2-4 周/版本 | 内部基线期,频繁小修 |
| v0.2.0 | 6-8 周 | 第一批用户反馈后,加实用功能 |
| v0.x.0 → v1.0.0 | 3-6 个月 | API 锁定 + 安全审计 + 文档完善 |
| v1.0.0 之后 | 6-8 周/版本 | 正式 release,严格 semver |

---

## 与 evorule 主仓的协调

| 场景 | 流程 |
|---|---|
| 主仓发布新版 | 本仓更新 `Cargo.toml` 的 `evorule-*` 版本号 → `cargo test` → commit + tag |
| 本仓独立发版 | 不需要等主仓,本仓自行 release(只要依赖的 `evorule-*` 版本已发布到 crates.io) |
| 跨仓 breaking change | 主仓先发 MAJOR → 本仓适配 → 本仓发 MAJOR |

---

*本路线图反映当前计划,可能根据用户反馈和实际情况调整。*

<!--
  Copyright 2026 EvoRule Project
  SPDX-License-Identifier: AGPL-3.0-or-later
-->

# evorule-debug-control

**调试 API 控制 —— pause / resume / step / inspect**

> **crate 类型**: 内部 lib（`publish = false`）
> **引入版本**: v0.1.0

---

## 定位

提供反应器调试控制 API，用于开发和测试环境下的单步执行、状态检查、断点调试。

**生产环境应禁用调试 API**（通过 `--enable-debug-api` 标志控制，默认关闭）。

## 公开类型

- `DebugControlService` — 调试控制服务
- `DebugStatus` — 调试状态（paused / running / last_version）
- `StepRequest` — 单步执行请求
- `StepResponse` — 单步执行响应
- `WatchQuery` — 状态观察查询

## 主要功能

- `is_paused()` / `set_paused()` — 暂停/恢复反应器执行
- `get_last_version()` / `set_last_version()` — 获取/设置最后执行的版本号
- `build_router()` — 构建 axum 调试 API 路由

## 安全约束

- 调试 API 默认禁用，需显式启用
- 生产环境启用调试 API 会在启动日志中输出警告
- `#![forbid(unsafe_code)]`（C4）

## 相关文档

- [INTEGRATION_GUIDE.md](../../docs/INTEGRATION_GUIDE.md) — 集成指南
- [GATE_REFERENCE.md](../../GATE_REFERENCE.md) — 门控参考

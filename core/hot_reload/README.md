<!--
  Copyright 2026 EvoRule Project
  SPDX-License-Identifier: AGPL-3.0-or-later
-->

# evorule-hot-reload

**规则热重载 —— 目录监控 + 零停机规则更新**

> **crate 类型**: 内部 lib（`publish = false`）
> **引入版本**: v0.3.0

---

## 定位

监控规则目录的文件变化，自动加载新规则到运行中的反应器，无需重启服务。

## 模块结构

- `config` — 热重载配置（规则目录路径、轮询间隔、auth_token）
- `loader` — 规则加载器（读取 JSON 文件、校验、转换为核心格式）
- `watcher` — 文件系统监控（基于 `notify` crate 或轮询）

## 公开类型

- `HotReloadService` — 热重载服务
- `config::HotReloadConfig` — 热重载配置

## 主要功能

- `config()` — 获取/设置热重载配置
- `build_router()` — 构建 axum 热重载 API 路由（手动触发重载）

## 重要行为

- **仅支持增量添加**（S1）：检测到文件删除时输出 `warn!` 日志，明确告知"hot_reload 仅支持增量添加规则，删除文件不会从 server 移除已有规则，如需清除旧规则请重启 session"
- **auth_token 支持**（N4）：配置增加 `auth_token` 字段，`create_session`/`send_rules` 注入 `Authorization: Bearer` 头；bin 加 `--auth-token` CLI 参数

## 安全约束

- `#![forbid(unsafe_code)]`（C4）
- 规则加载前通过 `core/rule_schema` 做 JSON Schema 校验

## 相关文档

- [PITFALLS.md](../../docs/PITFALLS.md) 坑 1 — hot_reload 相关踩坑
- [GATE_REFERENCE.md](../../GATE_REFERENCE.md) — 门控参考

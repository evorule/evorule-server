<!--
  Copyright 2026 EvoRule Project
  SPDX-License-Identifier: AGPL-3.0-or-later
-->

# evorule-hot-reload

**规则热重载 —— 目录监控 + 零停机规则更新**

> **crate 类型**: 内部 lib（`publish = false`）
> **引入版本**: 0.3.0

---

## 定位

监控规则目录的文件变化，触发 evorule-server 规则重载（`POST /api/rules/reload`），
使规则**对新会话生效**，无需重启服务。已存在会话保持不变（TCB 不可变语义）。

## 模块结构

- `config` — 热重载配置（规则目录路径、轮询间隔、auth_token）
- `loader` — 规则加载器（读取 JSON 文件、校验、转换为核心格式；供 `/status` `/rules` 统计展示）
- `watcher` — 文件系统监控（基于 `notify` crate 或轮询）

## 公开类型

- `HotReloadService` — 热重载服务
- `config::HotReloadConfig` — 热重载配置

## 主要功能

- `trigger_reload` — 触发服务端 `POST /api/rules/reload`，返回重载前后规则总数
- `reload_rules` — 校验规则目录存在后触发服务端 reload（手动/自动共用）
- `build_router` — 构建 axum 热重载 API 路由（`GET /status`、`POST /reload`、`GET /rules`）

## 重要行为

- **使用约束**：`rules_dir` 必须指向 evorule-server 的 `--rules-dir`（服务端 reload
  从该目录合并加载规则），否则监听到的变化不会反映到服务端规则集
- **目录为准全量重载**：重载语义与 `POST /api/rules/reload` 一致——规则对新会话生效；
  删除规则文件并重载后，该规则对新会话移除（旧版「仅支持增量添加」限制随通路语义
  修正一并消除）
- **空目录允许重载**：用于清空业务规则（仅保留 TCB 宪法）
- **auth_token 支持**：设置后所有发往 evorule-server 的请求携带 `Authorization: Bearer` 头；
  bin 加 `--auth-token` CLI 参数

## 安全约束

- `#![forbid(unsafe_code)]`（C4）
- 服务端 reload 侧对规则文件做 JSON Schema 校验（无效文件跳过，不破坏现有规则）

## 相关文档

- [PITFALLS.md](../../docs/PITFALLS.md) 坑 1 — hot_reload 相关踩坑
- [GATE_REFERENCE.md](../../GATE_REFERENCE.md) — 门控参考

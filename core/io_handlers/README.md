<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: AGPL-3.0-or-later

  This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
-->

# evorule-io-handlers

**I/O Handler 实现 —— DB / HTTP / Memory / ServiceRegistry**

> **crate 类型**: 内部 lib（`publish = false`，不进 crates.io）
> **引入版本**: v0.1.0（从 evorule-governance 迁出，H5 迁移）
> **依赖 trait**: `evorule-reactor::IoHandler`

---

## 定位

此 crate 从 `evorule-governance/src/io_handlers/` 迁出，属于**应用层（策略）**。

依赖 `evorule-reactor` 的 `IoHandler` trait，不依赖 `evorule-governance`，避免循环依赖（evorule-governance 的 IoDispatcher 框架是机制，留核心）。

---

## 模块结构

| 模块 | 大小 | 实现 | 说明 |
|------|------|------|------|
| `db_handler` | 34KB | `sqlx` (SQLite) | 数据库 I/O，支持语句白名单（`StatementWhitelist`）防止 SQL 注入 |
| `http_handler` | 28KB | `reqwest` | HTTP 请求 I/O，支持 GET/POST/PUT/PATCH/DELETE/HEAD |
| `memory_handler` | 11KB | `tokio::fs` | 文件系统键值存储 I/O，key 长度限制 ≤ 255 字节（N6） |
| `service_registry` | 20KB | `serde_json` | 服务注册中心，从 `service_registry.json` 加载服务配置，校验 URL scheme（N2） |

---

## 安全特性

### SSRF 防护（B1）
- `HttpHandler::build_client()` 加 `.redirect(reqwest::redirect::Policy::none())`
- 禁用 HTTP 重定向跟随，防止 SSRF 绕过（公网 URL → 302 → 169.254.169.254 云元数据）
- 3xx 响应作为 Err 返回上层，由调用方决定处理方式

### Loopback 防护
- 默认禁止访问 loopback 地址（127.0.0.1 / [::1]）
- `--allow-loopback` CLI 参数可启用（生产环境永远不要启用）
- `HttpHandler::new_dev_allow_loopback()` 仅供开发使用

### URL Scheme 校验（N2）
- `ServiceRegistryHandler::parse_service_entry()` 校验 scheme 为 http/https
- 拒绝 `file:///`、`data://` 等危险 scheme

### SQL 注入防护
- `DbHandler` 使用参数化查询
- `StatementWhitelist` 白名单机制，只允许预定义的 SQL 语句执行
- `WhitelistedDbHandler` 包装器强制白名单校验

### Key 长度限制（N6）
- `MemoryHandler::execute()` 检查 key ≤ 255 字节
- 防止超长 key 触发 OS 文件名错误

---

## ServiceRegistry 机制

`service_registry.json` 配置文件格式：

```json
{
  "services": [
    {
      "name": "ik_solver",
      "url": "http://localhost:5101/solve",
      "method": "POST",
      "timeout_ms": 5000
    }
  ]
}
```

- `service_name` 可引用 payload 路径：`"__exec__.instruction.params.service_name"`
- `args` 可引用 payload 路径：`"__exec__.payload._ik_args"`

---

## 公开类型

- `DbHandler` / `StatementEntry` / `StatementWhitelist` / `WhitelistedDbHandler`
- `HttpHandler`
- `MemoryHandler`
- `ServiceRegistryHandler` / `ServiceEntry`

---

## 安全约束

- `#![forbid(unsafe_code)]`（C4）
- build.rs 4 模式字节子串扫描（S1：debug_assert/unwrap/expect/panic）
- 与 evorule-server (bin) 同一组 4 模式，保证两个安全最敏感的 crate 不会走偏

---

## 相关文档

- [INTEGRATION_GUIDE.md](../../docs/INTEGRATION_GUIDE.md) §1 — I/O Handler 架构
- [PITFALLS.md](../../docs/PITFALLS.md) 坑 4-6 — I/O 处理相关踩坑
- [GATE_REFERENCE.md](../../GATE_REFERENCE.md) §3.2 — 本 crate 的 build.rs 门控

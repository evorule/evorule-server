<!--
  Copyright 2026 EvoRule Project
  SPDX-License-Identifier: AGPL-3.0-or-later
-->

# evorule-metrics

**Prometheus 指标 —— 7 个核心 metric + 会话级指标**

> **crate 类型**: 内部 lib（`publish = false`）
> **引入版本**: v0.1.0

---

## 定位

提供 Prometheus 格式的指标收集和导出，用于 evorule-server 的可观测性。

## 公开类型

- `MetricsService` — 指标服务（收集/导出/路由构建）
- `SessionMetrics` — 会话级指标（命令数、I/O 数、错误数、延迟分布）
- `ServerMetrics` — 服务级指标（活跃 session 数、HTTP 请求数、速率限制触发数）

## 7 个核心 metric

1. `evorule_commands_total` — 已处理命令总数
2. `evorule_io_requests_total` — I/O 请求总数
3. `evorule_io_errors_total` — I/O 错误总数
4. `evorule_sessions_active` — 活跃 session 数
5. `evorule_http_requests_total` — HTTP 请求总数（按路径/状态码分类）
6. `evorule_rate_limits_triggered_total` — 速率限制触发总数
7. `evorule_command_duration_seconds` — 命令执行延迟分布（histogram）

## 主要功能

- `get_metrics_text()` — 获取 Prometheus 文本格式指标
- `get_session_metrics(session_id)` — 获取指定 session 的指标
- `get_server_metrics()` — 获取服务级指标
- `build_router()` — 构建 `/metrics` 端点路由

## 安全特性

- **指标基数防护**（N3）：`normalize_path_for_metrics` 把数字段归一化为 `{id}`，防止 Prometheus 基数爆炸
- **/metrics 可选认证**（S2）：`--metrics-auth` / `EVORULE_METRICS_AUTH` CLI 参数控制是否需要认证，默认关闭（Prometheus scraper 通常不带 token）

## 相关文档

- [README.md](../../README.md) — 指标配置说明
- [GATE_REFERENCE.md](../../GATE_REFERENCE.md) — 门控参考

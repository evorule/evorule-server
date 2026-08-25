<!--
  Copyright 2026 EvoRule Project
  SPDX-License-Identifier: AGPL-3.0-or-later
-->

# evorule-rule-tools

**规则脚手架工具 —— 校验 + 安全检查 + CLI 工具**

> **crate 类型**: 内部 lib + bin（`publish = false`）
> **引入版本**: v0.1.0

---

## 定位

提供规则文件的校验、安全检查和命令行工具，用于规则开发和 CI/CD 流水线。

## 模块结构

- `validator` — 规则校验器（JSON Schema 校验 + 语义检查）
- `safety` — 安全检查器（危险模式检测、SSRF 防护检查、注入防护检查）

## 公开类型

- `validator::RuleValidator` — 规则校验器
- `safety::SafetyChecker` — 安全检查器
- `build_router()` — 构建 `/api/rules/validate` 端点路由

## 主要功能

- **JSON Schema 校验**：基于 `core/rule_schema` 的固化 Schema 做结构校验
- **语义检查**：元指令类型白名单、域类型白名单、必填字段检查、路径格式检查
- **安全检查**：危险模式检测（如硬编码密钥、SQL 注入模式、SSRF 风险 URL）
- **CLI 工具**：`evorule-rule-tools` 二进制，支持命令行校验规则文件

## CLI 用法

```bash
# 校验单个规则文件
evorule-rule-tools validate path/to/rule.json

# 校验整个目录
evorule-rule-tools validate path/to/rules/ --recursive

# 安全检查
evorule-rule-tools safety path/to/rule.json
```

## 安全约束

- `#![forbid(unsafe_code)]`（C4）
- 校验器本身禁止 panic，所有错误路径返回 `Result`

## 相关文档

- [INTEGRATION_GUIDE.md](../../docs/INTEGRATION_GUIDE.md) §4 — 规则编写实战要点
- [GATE_REFERENCE.md](../../GATE_REFERENCE.md) — 门控参考

<!--
  Copyright 2026 EvoRule Project
  SPDX-License-Identifier: AGPL-3.0-or-later
-->

# evorule-semantic-invariants

**语义不变量验证 —— 规则一致性自检 + 违规检测**

> **crate 类型**: 内部 lib（`publish = false`）
> **引入版本**: 0.3.0

---

## 定位

提供规则集的语义不变量验证服务，用于检测规则之间的冲突、冗余和违反业务约束的情况。

## 公开类型

- `SemanticInvariantService` — 语义不变量服务
- `InvariantRule` — 不变量规则（条件 + 操作符 + 期望值）
- `InvariantSet` — 不变量集合（多条规则组合）
- `Violation` — 违规记录（规则 ID、违规描述、严重程度）
- `CheckResult` — 检查结果（通过/失败 + 违规列表）
- `ViolationStats` — 违规统计（按严重程度分类）
- `Severity` — 严重程度枚举（info/warning/error/critical）
- `Operator` — 操作符枚举（eq/ne/lt/gt/contains/exists）

## 主要功能

- `add_rules()` — 添加不变量规则
- `get_rules()` — 获取所有不变量规则
- `remove_rule()` — 移除指定规则
- `get_violations()` — 获取违规列表
- `get_stats()` — 获取违规统计
- `build_router()` — 构建语义不变量 API 路由

## 应用场景

- **规则冲突检测**：检测两条规则是否会产生矛盾的输出
- **业务约束验证**：验证规则集是否满足业务约束（如"订单金额不能为负"）
- **合规检查**：验证规则集是否符合合规要求（如"必须记录审计日志"）
- **CI/CD 集成**：在规则发布前自动运行语义不变量检查

## 安全约束

- `#![forbid(unsafe_code)]`（C4）
- 检查器本身禁止 panic，所有错误路径返回 `Result`

## 相关文档

- [GATE_REFERENCE.md](../../GATE_REFERENCE.md) — 门控参考

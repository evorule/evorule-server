<!--
  Copyright 2026 EvoRule Project
  SPDX-License-Identifier: AGPL-3.0-or-later
-->

# evorule-time-machine

**时间机器 —— rewind / diff / fork / replay（可审计的时间回溯）**

> **crate 类型**: 内部 lib（`publish = false`）
> **引入版本**: 0.3.0

---

## 定位

提供基于事实日志（Fact Log）的时间回溯能力，支持回滚到任意历史版本、对比两个版本的状态差异、从历史版本分支创建新会话、以及重放历史指令序列。

## 公开类型

- `HistoryEntry` — 历史记录条目（fact_id、logical_time、payload 快照）
- `RewindSnapshot` — 回溯快照（指定版本的完整状态）
- `PayloadDiff` — payload 差异（变更字段列表 + 变更前后值）
- `VersionTreeNode` — 版本树节点（用于分支可视化）
- `VersionTreeResponse` — 版本树响应
- `BatchDiffEntry` — 批量差异条目
- `BatchDiffResponse` — 批量差异响应
- `ReplayStep` — 重放步骤
- `ReplayPlanResponse` — 重放计划响应

## 主要功能

- `local_rewind(version)` — 回溯到指定版本（不修改原始日志，创建只读快照）
- `local_diff(version_a, version_b)` — 对比两个版本的 payload 差异
- `build_version_tree()` — 构建版本树（用于分支可视化）
- `build_batch_diff(versions)` — 批量对比多个版本
- `build_replay_plan(from_version, to_version)` — 构建重放计划（从版本 A 到版本 B 的指令序列）

## 重要特性

- **只读回溯**：回溯操作不修改原始事实日志，保证审计链不可篡改
- **版本间隙处理**（S4）：正确处理版本间隙（Command 被忽略产生间隙、ST 与 IoResponse 间间隙），间隙边界返回 `Some`，间隙内返回 `None`
- **稀疏版本树**：支持稀疏版本树的 `total_versions` 正确性验证
- **9 个测试覆盖**：首条记录前间隙、Command 被忽略产生间隙、ST 与 IoResponse 间间隙、多间隙全返回 None、间隙边界返回 Some、local_diff 间隙版本退化为空对象、build_version_tree 稀疏版本正确性、build_batch_diff 跨间隙配对

## 安全约束

- `#![forbid(unsafe_code)]`（C4）
- 回溯操作是只读的，不会修改原始事实日志

## 相关文档

- [INTEGRATION_GUIDE.md](../../docs/INTEGRATION_GUIDE.md) §3 — 审计链完整使用
- [PITFALLS.md](../../docs/PITFALLS.md) — 时间机器相关踩坑
- [GATE_REFERENCE.md](../../GATE_REFERENCE.md) — 门控参考

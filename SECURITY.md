<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: AGPL-3.0-or-later
-->

# EvoRule Server 安全漏洞报告政策

**最后更新**: 2026-07-30

本仓(`evorule-server`)的安全漏洞报告流程**与 [evorule 主仓](https://gitee.com/evo-rule-lab/evorule)共用**。
详细流程见主仓 [`SECURITY.md`](https://gitee.com/evo-rule-lab/evorule/blob/main/SECURITY.md)。

---

## 📧 联系方式

- **邮箱**: <evorulelab@gmail.com>(主题加 `[SECURITY][evorule-server]`)
- **Gitee 私信**: 维护者(@evorulelab)

---

## ⚠️ Supported Versions / 支持的版本

| 版本 | 支持状态 | 说明 |
|---|---|---|
| `v0.1.0` | ✅ Supported | 内部基线阶段(2026-07-30 之后) |
| `evorule-application/core/*` 旧位置 | ❌ EOL | 已迁出,无 Gitee 撤回成本 |

---

## 报告内容

请在报告中包含:

1. 漏洞类型和描述
2. 复现步骤(环境、命令、输入数据)
3. 潜在影响评估(认证绕过?数据泄露?拒绝服务?)
4. 已知绕过方案
5. 是否已公开披露

---

## 修复承诺

- **Critical / High**:60 天内修
- **Medium / Low**:推迟到下一个 minor 版本
- **安全公告**:修完后 30 天内公开披露(经协调)

---

## 本仓特有的安全关注点

| 关注点 | 说明 |
|---|---|
| HTTP 路由 100+ 条 | 重点审 `src/api/server.rs` |
| Bearer token 认证 | `core/auth` + `evorule-server/src/auth.rs`,看是否所有敏感路由都走认证 |
| 时间机器(rewind/diff/fork) | `core/time_machine`,防越权回滚 |
| 调试 API (`/debug/*`) | 默认不暴露?需要单独端口? |
| Prometheus 指标泄露 | `/metrics` 端点是否暴露内部细节 |
| 速率限制 | `tower_governor` 配置,默认 100 RPS 是否合适 |

---

_本仓与 evorule 主仓共用开发节奏,安全公告同步发布。_

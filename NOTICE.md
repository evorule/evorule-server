<!--
  Copyright 2026 EvoRule Project

  This program is free software: you can redistribute it and/or modify
  it under the terms of the GNU Affero General Public License as published by
  the Free Software Foundation, either version 3 of the License, or
  (at your option) any later version.

  This program is distributed in the hope that it will be useful,
  but WITHOUT ANY WARRANTY; without even the implied warranty of
  MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
  GNU Affero General Public License for more details.

  You should have received a copy of the GNU Affero General Public License
  along with this program.  If not, see <https://www.gnu.org/licenses/>.

  SPDX-License-Identifier: AGPL-3.0-or-later
-->

# EvoRule Server — 声明

**版权所有 (c) 2026 EvoRule Project**

本项目（`evorule-server`）是 EvoRule 框架的官方 HTTP server 实现，包含：
- `evorule-server` 独立二进制（axum HTTP + SSE + Session 管理）
- 9 个 server 配套 crate（`core/auth`、`core/io_handlers`、`core/metrics`、`core/hot_reload`、`core/debug_control`、`core/semantic_invariants`、`core/time_machine`、`core/rule_tools`、`core/metrics`）

## 协议

| 资产 | 协议 | 说明 |
|---|---|---|
| **本仓所有代码** | AGPL-3.0-or-later | 详见 [LICENSE](LICENSE) |
| **`resources/core_eval.json`** | **CC0 1.0 公共领域** | EvoRule 宪法（解释器规范）——从 [evorule 主仓](https://gitee.com/evo-rule-lab/evorule)同步，任何人可自由使用 |

## 依赖说明

本仓依赖 evorule 核心三 crate（均为 AGPL-3.0-or-later）：

| 依赖 | 来源 | 说明 |
|---|---|---|
| `evorule-tcb` | [evorule 主仓](https://gitee.com/evo-rule-lab/evorule) | TCB 基础层（JSON 状态机 / 路径解析 / 域评估） |
| `evorule-reactor` | 同上 | 反应器层（主循环 / Fact 日志 / WAL） |
| `evorule-governance` | 同上 | 治理层机制（会话管理 / 审计链 / IoDispatcher 框架） |

本地开发通过 `path` 依赖引用兄弟仓 `../evorule/`；发布时通过 `crates.io` 拉取。

## 第三方依赖

主要第三方依赖（完整列表见 `Cargo.lock`）：

| 依赖 | 协议 | 用途 |
|---|---|---|
| `axum` | MIT | HTTP 框架 |
| `tokio` | MIT | 异步运行时 |
| `reqwest` | MIT/Apache-2.0 | HTTP 客户端（HttpHandler / time_machine） |
| `rusqlite` | MIT | SQLite 绑定（DbHandler） |
| `notify` | CC0-1.0 | 文件系统监听（hot_reload） |
| `tower` / `tower-governor` | MIT | 中间件 / 速率限制 |
| `prometheus` | Apache-2.0 | 指标收集 |
| `serde` / `serde_json` | MIT/Apache-2.0 | 序列化 |
| `clap` | MIT/Apache-2.0 | CLI 参数解析 |
| `tracing` | MIT | 结构化日志 |

## 联系信息

- **项目**: EvoRule Server — 官方 HTTP server 实现
- **作者**: EvoRule Project
- **邮箱**: <evorulelab@gmail.com>
- **组织**: [EvoRule Lab](https://gitee.com/evo-rule-lab)
- **Gitee**: <https://gitee.com/evo-rule-lab/evorule-server>

<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: AGPL-3.0-or-later
-->

# 贡献指南

欢迎贡献 EvoRule Server!在提交 PR 之前,请先阅读本指南。

---

## 行为准则

本项目采用 [Contributor Covenant](https://www.contributor-covenant.org/) v2.1 行为准则。
见 [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md)。

---

## 开发流程

### 1. Fork & Clone

```bash
git clone https://gitee.com/<your-fork>/evorule-server.git
cd evorule-server
```

### 2. 编译验证

```bash
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
```

### 3. 提交 PR

- 提交前确保 `cargo build` / `cargo test` / `cargo clippy` 全部 0 warning / 0 error
- 写新代码时遵守 `.clippy.toml` + `Cargo.toml [workspace.lints]`
- 在 PR 描述中说明:动机 / 实现方式 / 测试覆盖 / 风险评估

---

## 代码风格

- 遵循 `rustfmt` 默认风格
- 提交前 `cargo fmt --all`
- 命名:`snake_case`(变量/函数)/ `PascalCase`(类型)/ `SCREAMING_SNAKE_CASE`(常量)
- 公开 API 必须有 `///` 文档注释
- **禁止 `unsafe`**:本仓 `#![forbid(unsafe_code)]`
- **禁止 panic-prone 模式**:`unwrap()` / `expect()` / 索引越界 在 `clippy` 中 deny
- 错误处理用 `Result<T, E>` + `thiserror` 自定义错误类型

---

## 架构原则

1. **本仓不改 evorule 核心** —— 所有核心变更走核心仓的 release 流程
2. **本仓独立 release**,独立版本号
3. **核心 vs server 配套的边界**
   - 核心 (在核心仓):`evorule-tcb` / `evorule-reactor` / `evorule-governance`
   - server 配套 (在本仓):`core/auth` / `core/io_handlers` / `core/metrics` / `core/hot_reload` / `core/debug_control` / `core/semantic_invariants` / `core/time_machine` / `core/rule_tools`
   - 决策问题:某项改动"放核心"还是"放 server 配套"?参考核心仓的 `STRATEGIC_DIRECTION.md`

4. **新增依赖**:必须先在 PR 中说明为什么这个依赖必要,以及不依赖的方案为何不可行

---

## 测试要求

| 类型 | 要求 |
|---|---|
| 单元测试 | 每个 lib 至少 1 个 happy path + 1 个 error path |
| 集成测试 | 在 `evorule-server/tests/` 下,覆盖核心 API |
| 基准 | 在 `evorule-server/examples/` 下,新增功能必须有对应 bench |
| 文档 | 新公开 API 必须更新 `docs/` 下对应文档 |

---

## 提交信息格式

```
<type>(<scope>): <subject>

<body>

<footer>
```

type: `feat` / `fix` / `docs` / `refactor` / `test` / `bench` / `chore`
scope: 模块名,如 `core/auth` / `evorule-server` / `docs`

示例:

```
feat(core/time-machine): add fork API for branching at historical state

Allow users to fork a session at any historical state, creating
a new session with the same facts but independent evolution.

Closes #123
```

---

## 协议

提交 PR 即表示您同意按 AGPL-3.0-or-later 协议贡献代码。
本仓**不接收**任何"贡献即视为放弃权利"的协议 —— 您的版权仍然属于您。

---

## 联系方式

- Gitee Issue:本仓 Issues
- 邮箱:<evorulelab@gmail.com>

---

_感谢您的贡献!_

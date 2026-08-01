<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: AGPL-3.0-or-later
-->

# AGENTS.md

> 面向 AI agent / 人类工程师的"工作规则"。
> 本文件是 evorule-server 仓的"宪法"——所有改动前请先读一遍。
> evorule-server 仓是 **evorule 框架的官方 HTTP server 实现**(走神 9 决策),不绑 evorule-application 仓。

---

## 〇、本仓定位

**evorule-server 仓 = 框架的官方 server 实现 + 9 个 server 配套 lib**。

- **本仓不能做**: 修改 evorule 核心(那在 evorule 主仓);做应用层 web app(那在 evorule-application 仓);做 AI agent 编排(那在 evo-agent 仓)
- **本仓能做的事**: axum HTTP server、具体 I/O handler、Auth、Metrics、Hot-reload、Debug control、Time machine、Rule tools、Semantic invariants

**与主仓的关系**:通过 crates.io 依赖 evorule-tcb/reactor/governance(`version = "0.1.1"`)。用户 clone 后自动从 crates.io 拉取;本地开发时取消注释 workspace 顶层 `Cargo.toml` 的 `[patch.crates-io]` 段用兄弟仓 path 覆盖。

---

## 一、3 条硬规则(数字编号,跟 evorule 主仓 narrative 风格对齐,不用 G 编号)

> **关于编号**:evorule 主仓用 G 编号(目前只有 G8 门控),本仓**不用 G 编号**,改用数字编号"硬规则 1/2/3",避免跨仓 G 编号含义混淆。G 编号在主仓的 LLM/agent narrative 中有特殊含义,本仓不参与。

### 硬规则 1:核心 vs server 配套的边界

- **核心**(在 evorule 主仓 https://gitee.com/evo-rule-lab/evorule):`evorule-tcb` / `evorule-reactor` / `evorule-governance` / `evorule-cli`
- **server 配套**(在本仓):`evorule-server` (bin) + `core/*` 9 个 lib
- **判断问题**:某项改动该放哪?参考 evorule 主仓的 `STRATEGIC_DIRECTION.md` 〇节

**这条规则是关键**——走神 6 精神:evorule 改的少,application 是扩展空间。**server 仓是 framework 配套,不是 application**。

### 硬规则 2:不要在本仓加"应用层"功能

- ❌ 不加新的 web app(放 evorule-application 仓)
- ❌ 不加 AI agent 能力(放 evo-agent 仓)
- ❌ 不加 IDE 工具(放 evorule-application 仓)
- ❌ 不加 portal 之类的 UI(放 evorule-application 仓)
- ✅ 可以加新的 server 配套 lib(在 `core/` 下)
- ✅ 可以加新的 HTTP 路由(在 `evorule-server/src/api/`)
- ✅ 可以加新的 I/O handler(在 `core/io_handlers/src/handlers/`)

### 硬规则 3:依赖走 crates.io,本地开发用 [patch.crates-io]

```toml
# evorule-server/Cargo.toml (示例)
[dependencies]
evorule-tcb = { version = "0.1.1" }   # 不写 path,走 crates.io
evorule-reactor = { version = "0.1.1", features = ["persistence"] }
evorule-governance = { version = "0.1.1", features = ["persistence"] }

# 顶层 Cargo.toml
[patch.crates-io]
evorule-tcb = { path = "../evorule/evorule-tcb" }
evorule-reactor = { path = "../evorule/evorule-reactor" }
evorule-governance = { path = "../evorule/evorule-governance" }
```

**不要在子 crate 的 `Cargo.toml` 里写 `path = "../evorule/..."`!**那会让用户 clone 下来编译失败。

---

## 二、代码风格(命名 / 不写 unsafe / 不写 panic-prone)

### 命名约定

- 目录:`snake_case`(`core/io_handlers` / `core/time_machine`)
- Crate 名:`kebab-case`(`evorule-io-handlers` / `evorule-time-machine`)
- 文件:`snake_case.rs`
- 类型:`PascalCase` / 函数:`snake_case` / 常量:`SCREAMING_SNAKE_CASE`

### 不写 `unsafe`

- `#![forbid(unsafe_code)]` 在每个 lib/bin 顶部声明
- clippy `unsafe_code` = deny

### 不写 panic-prone 模式

- ❌ `unwrap()` / `expect()` / 索引越界 —— clippy `unwrap_used` / `expect_used` / `panic` = deny
- ✅ 错误处理用 `Result<T, E>` + `thiserror` 自定义错误类型
- ✅ 不可避免的 panic 用 `unreachable!` / `todo!`(但要注释理由)

---

## 三、测试要求

| 类型 | 要求 |
|---|---|
| 单元测试 | 每个 lib 至少 1 个 happy path + 1 个 error path |
| 集成测试 | 在 `evorule-server/tests/` 下,覆盖核心 API |
| 基准 | 在 `evorule-server/examples/` 下,新增功能必须有对应 bench |
| 文档 | 新公开 API 必须更新 `README.md` / `docs/` |

**当前测试覆盖**:17+28+26+73+27+55+52+43+76+28+6+10+6 = 447 passed(core/* lib 321 + evorule-server bin 104 + 集成测试 22)。**所有 lib 均已有测试覆盖**(2026-08-01 验证)。

---

## 四、CI / CD 纪律

- **必须通过**:`cargo build --workspace` + `cargo test --workspace` + `cargo clippy --workspace --all-targets`
- **不要在子 crate 目录**:`cd core/auth && cargo test` 这种,会绕过 workspace 共享 lock,导致版本漂移
- **CI 假设**:兄弟仓 `evorule/` 存在(本地开发用,相对路径 `../evorule`)/ crates.io 拉到(用户 clone)
- **Docker 假设**:build context 是 evorule-server 仓根,不是 evorule-application 仓

---

## 五、跨仓协议(走神 9 精神的执行)

| 仓 | 关系 |
|---|---|
| evorule 主仓 (https://gitee.com/evo-rule-lab/evorule) | 本仓**依赖**它的 3 个 lib(crates.io,本地 patch) |
| evorule-application 仓 (https://gitee.com/evo-rule-lab/evorule-application) | 本仓**独立 release**,不绑 application 仓 |
| evo-agent 仓 (https://gitee.com/evo-rule-lab/evo-agent) | 本仓**独立 release**,agent 可以调本仓 HTTP API |

**绝对不交叉**:
- 业务逻辑 → evorule-application 仓
- AI 能力 → evo-agent 仓
- 框架/原语 → evorule 主仓
- HTTP server 入口 + 配套 → 本仓

---

## 六、什么时候该问 / 什么时候该做

**直接做**(不需要问):
- 改 core/* lib 的内部实现
- 改 evorule-server 的路由
- 改文档 / CHANGELOG
- 修 bug(已经有 issue 描述的)
- 加单元测试

**先问**:
- 加新 crate(在 core/ 下还是另起仓)
- 加新 feature flag
- 改 workspace.lints(可能影响所有 crate)
- 改 Dockerfile(影响部署)
- 改 [patch.crates-io] 路径

**绝不**:
- 改 evorule 核心代码(那是 evorule 主仓的事)
- 删任何文件(用户约束,即使 backup 里有)
- 改 CHANGELOG 已发布版本段的**语义**(新增 / 变更 / 修复的分类、已发布功能描述只增不改,保留历史痕迹)

  **例外(必须订正)**:历史段中的**事实性错误**(数字 / 路径 / 文件名 / 链接等与实际不符)允许并应当订正 —— 可在原条目内直接修正,或追加"勘误:"说明。**语义保留 ≠ 错误保留**。

---

## 七、版本号与发布

- 当前版本:`v0.1.0`(2026-07-30,首次建立)
- 协议:AGPL-3.0-or-later(代码) + CC0-1.0(`resources/core_eval.json`)
- 发布方式:**Git 仓独立 release**(不绑主仓版本号,不绑 application 仓版本号)
- crates.io 状态:**不上 crates.io**(本仓是 server 应用层,跟随主仓 evorule-tcb/reactor/governance 即可)

---

## 八、致谢

- evorule 核心: `https://gitee.com/evo-rule-lab/evorule`
- 文档: 内部私有设计文档（不对外发布）
- 仓库元信息: `Cargo.toml` + `README.md` + `CHANGELOG.md`

---

_本文件是 evorule-server 仓的"宪法",所有改动前请先读一遍。_

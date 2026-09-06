<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: AGPL-3.0-or-later

  This file is part of EvoRule Server, licensed under GNU Affero General Public License v3 or later.
-->

# EvoRule Server 文档总索引

> **最后更新**:2026-09-01
> **版本对齐**:与 `Cargo.toml` 中 `version = "0.1.0"` 同步

---

## 一、入门必读

| 文档 | 用途 | 一句话说明 |
|:---|:---|:---|
| [README.md](README.md) | 项目总览 | evorule-server 是什么、快速开始、架构概览、API 概览 — **新用户首读** |
| [CONTRIBUTING.md](CONTRIBUTING.md) | 贡献指南 | 如何提交 issue / PR / 编译 / 测试 / 提 PR 检查清单 — **贡献者首读** |
| [SECURITY.md](SECURITY.md) | 安全报告 | 漏洞披露流程 + 安全联系人 + 本仓特有安全关注点 |

---

## 二、项目级正式文档

### 2.1 版本、路线、承诺

| 文档 | 主题 | 说明 |
|:---|:---|:---|
| [VERSION_STRATEGY.md](VERSION_STRATEGY.md) | 版本策略 | 语义化版本规则、发布清单 |
| [CHANGELOG.md](CHANGELOG.md) | 更新日志 | Keep a Changelog v1.0 格式;每版所有重大变更(B1-B3 / N1-N6 / S1-S4) |
| [GATE_REFERENCE.md](GATE_REFERENCE.md) | 门控参考 | build.rs 编译时门禁 + Clippy workspace lints + 豁免索引 |

### 2.2 法律、协议、贡献

| 文档 | 主题 | 说明 |
|:---|:---|:---|
| [LICENSE](LICENSE) | AGPL-3.0 主协议 | 本仓 AGPL-3.0-or-later 协议全文 |
| [NOTICE.md](NOTICE.md) | 声明文件 | 版权声明 + 协议分离 + 第三方依赖许可 |
| [AUTHORS.md](AUTHORS.md) | 作者列表 | 核心贡献者名单 |
| [TRADEMARK.md](TRADEMARK.md) | 商标政策 | "EvoRule" / "EvoRule Server" / "元则" / "则灵" 商标使用规范 |
| [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) | 行为准则 | 贡献者行为守则(Contributor Covenant 2.1) |

---

## 三、开发文档（`docs/` 目录）

| 文档 | 说明 |
|:---|:---|
| [docs/INTEGRATION_GUIDE.md](docs/INTEGRATION_GUIDE.md) | 集成指南 — 如何将 evorule-server 集成到应用中 |
| [docs/PITFALLS.md](docs/PITFALLS.md) | 已知坑 — 开发/部署中遇到的陷阱及解决方案 |
| [docs/PITFALLS.json](docs/PITFALLS.json) | 已知坑(机器可读格式) — 供工具消费的结构化版本 |
| [docs/RELEASE_PROCESS.md](docs/RELEASE_PROCESS.md) | 发布流程 — 发布前就绪检查、打 tag、Docker 镜像构建、回滚流程 |

---

## 四、配置文件

| 文件 | 说明 |
|:---|:---|
| [Cargo.toml](Cargo.toml) | Workspace 根配置(members / lints / profile / patch) |
| [.clippy.toml](.clippy.toml) | Clippy 阈值配置(`too-many-lines-threshold = 150`) |
| [.markdownlint.json](.markdownlint.json) | Markdown lint 规则配置 |
| [Dockerfile](Dockerfile) | Docker 容器构建配置 |
| [.dockerignore](.dockerignore) | Docker 构建排除规则 |
| [.gitignore](.gitignore) | Git 忽略规则 |

---

## 五、Crate 结构概览

```
evorule-server/
├── core/
│   ├── auth/              # Bearer token 认证(N1: 空token过滤)
│   ├── debug_control/     # 调试 API 控制
│   ├── hot_reload/        # 规则热重载(S1: 删除事件语义 / N4: auth_token)
│   ├── io_handlers/       # I/O handler 实现(N2: scheme校验 / N6: key长度 / B1: SSRF redirect)
│   ├── metrics/           # Prometheus 指标实现
│   ├── plugin-kit/        # 插件机制公共件(NativeService trait/声明项/泛型过滤路由器,三插件归一)
│   ├── rule_schema/       # 规则 Schema 门禁 (0.3.0 新增, /api/rules/validate 权威基准)
│   ├── rule_tools/        # 规则脚手架工具
│   ├── semantic_invariants/ # 语义不变量验证
│   ├── time_machine/      # 时间机器(S4: 版本间隙测试)
│   └── workspace/         # 多租户工作空间 + 规则元数据管理 (0.3.0 新增)
├── evorule-server/        # 主 bin(axum HTTP + SSE + Session 管理)
│   └── src/
│       ├── api/server.rs  # HTTP 路由 + 中间件(B2: reload认证 / S2: metrics可选认证 / S3: CORS通配符检测)
│       ├── api/bundles.rs # 规则包 API (0.3.0 新增: 导入/列出/回滚)
│       ├── api/permissions.rs # 权限 API (0.3.0 新增)
│       ├── api/openapi.rs # OpenAPI 单一真相源 (0.3.0 新增)
│       ├── auth.rs        # 认证逻辑(N1)
│       ├── main.rs        # 启动入口(B3: fail-closed启动)
│       └── metrics_impl.rs # Prometheus 指标收集(N3: 指标基数防护)
├── plugins/               # 进程内原生插件 (0.3.0 新增;UV-035 起多插件登记)
│   ├── demo-services/     # Rust 原生业务服务示例(复合路由: 原生优先, HTTP回落)
│   ├── physics-services/  # 确定性物理仿真插件(UV-035, vendored rpsm-core 内核)
│   └── indicator-services/ # 确定性金融技术指标插件(UV-037, pandas 语义逐位对齐 Rust 重写)
├── rules/                 # 规则包目录
│   └── bundles/           # 规则包示例 (bundle-ds-yuanze-01-v3 等)
├── resources/
│   └── server_eval.json   # EvoRule 宪法·server 业务规则集(CC0-1.0;0.4.1 前旧名 core_eval.json)
└── docs/                  # 开发文档
```

---

## 六、文档维护规则

1. **加新 L1 公开文档必登索引**:新根目录 md 或 `docs/**` md 创建时,必须同步在本 DOCS_INDEX 登记
2. **版本号单一真相源**:所有文档写死的版本号字符串必须与 `Cargo.toml` 顶层 `version` 一致(CHANGELOG 历史段除外)
3. **文档被取代必标废弃**:新版文档生效时,旧版顶部加 `[已废弃]` 横幅,注明"被 `<新文件名>` 于 `<日期>` 取代"
4. **文档范围**:本仓文档只覆盖 server 仓范围;核心机制文档(TCB/REACTOR/GOVERNANCE SPEC)见核心仓 DOCS_INDEX

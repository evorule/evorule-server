<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: AGPL-3.0-or-later

  This file is part of EvoRule Server, licensed under GNU Affero General Public License v3 or later.
-->

# EvoRule Server 发布流程

> **文档性质**：evorule-server 仓的发布操作手册（流程通用，以首次发布为例）。
> **适用范围**：evorule-server 仓（evorule-server binary + core/\* 子 crate）。
> **前置文档**：[VERSION_STRATEGY.md](../VERSION_STRATEGY.md) 版本策略。

## 各仓独立发布原则

- **各仓独立发布**，不追求生态版本同步 bump。
- 本仓文档**只管好自己仓的真实情况**，诚实说明。
- 如依赖其他仓，最多说明"依赖哪个仓哪个版本"，不谈论其内部结构、运行方式或发布情况。
- 发布流程只覆盖本仓的验证、打 tag、推送。上游核心 crate 的版本同步由各仓自行管理。
- **分支策略**：`main` 为发布分支（tag 从 main 打），`dev/wip` 为开发分支。发布前将 dev/wip 合并到 main。
- 本仓所有 crate `publish = false`，**不上传 crates.io**。发布物为 git tag + Docker 镜像 + 二进制产物。

---

## 0. 前置条件

发布执行人需具备：

- Gitee 源仓库的 push 权限
- 本地 Rust 工具链（stable，与 CI 一致）
- PowerShell 7（`pwsh`）或 Windows PowerShell 5.1（脚本已兼容两者，含 UTF-8 BOM）
- Docker（用于构建和测试镜像）
- `cargo-audit`（可选，用于归档安全审计报告）

## 1. 发布前就绪检查

### 1.1 代码验证

```bash
# 1. 全量测试
cargo test --workspace

# 2. Lint（deny warnings，强门禁）
cargo clippy --workspace --all-targets -- -D warnings

# 3. 格式化检查
cargo fmt --all -- --check

# 4. Release 构建
cargo build --release --workspace
```

以上 4 项必须全部通过（exit 0）。如有失败，**停止发布**，修复后重新执行。

> **门禁系统说明**：`cargo build` 和 `cargo test` 会自动触发 `core/io_handlers/build.rs` 和 `evorule-server/build.rs` 的 L1 编译时字面量门禁。门禁在 Rust 编译器之前执行，违规代码无法进入编译阶段。**禁止设置 `EVORULE_SKIP_GATE=1` 环境变量绕过门禁**——§1.2 的 `validate-all.ps1` 会检测该变量，如设置则发布检查失败。详见 [GATE_REFERENCE.md](../GATE_REFERENCE.md)。

### 1.2 版本与文档治理验证（一站式）

```powershell
# 发布前就绪检查模式：跳过 tag 检查（此时 tag 尚未创建），允许 CHANGELOG 有 [未发布] 段
pwsh scripts/validate-all.ps1 -PreRelease
```

此命令一次性运行 **8 项检查**：

| #   | 检查项                    | 检查内容                                                                                                       |
| --- | ------------------------- | -------------------------------------------------------------------------------------------------------------- |
| 0   | **gate-bypass-check**     | 检测 `EVORULE_SKIP_GATE` 环境变量——如设置则 FAIL（门禁被绕过，禁止发布）                                       |
| 1   | `validate-version.ps1`    | workspace + 11 个子 crate 版本号一致性（SemVer 2.0 + MAJOR 一致 + FULL version 一致）+ L1 文档版本号通用扫描  |
| 2   | `validate-changelog.ps1`  | CHANGELOG 首段版本号 == Cargo.toml + 当前版本段存在 + 中文 `## [未发布]` 匹配                                  |
| 3   | `validate-license.ps1`    | LICENSE 含 AGPL + 所有 .rs 文件 SPDX 头                                                                        |
| 4   | `validate-cargolock.ps1`  | Cargo.lock 策略（binary workspace 必须提交仓根 Cargo.lock）                                                    |
| 5   | `validate-release.ps1`    | tag 格式校验（`-SkipTagCheck` 跳过 tag 存在性，发布前用）+ **`[patch.crates-io]` 段检测**（发布前必须移除）  |
| 6   | **`check_doc_safety.py`** | 文档安全 + 交叉引用完整性 + 基调合规（7 类规则，见下）                                                         |
| 7   | **`check_schema_sync.py`** | 跨仓 Schema 同步检查（`core/rule_schema/schemas/` 与 `evorule-system-rules` 仓一致性）                        |

`check_doc_safety.py` 检查 7 类规则：

- R-门控1：staged 文件不含 `wendang/` 路径（仓内私有文档不发布）
- R3 引用合规：L1 公开文档无私有集合路径泄露
- L1 不提 L2/L3：L1 不链接到仓内私有文档目录
- **R-兄弟仓零谈论**：L1 不谈论兄弟仓内部（依赖声明/指引除外）
- **R-agent身份零泄露**：L1 不泄露 AI agent 身份表述（产品概念除外）
- L1 交叉引用完整性：md 链接指向的仓内文件存在
- DOCS_INDEX 索引存在性

以上全部通过（exit 0）才可继续。如 `check_doc_safety.py` 报告 R-兄弟仓/R-agent 违规，需清理文档后重跑。

## 2. 归档 cargo audit 报告（可选）

```bash
# 生成 JSON + 文本格式审计报告
mkdir -p audit-report-v0.4.2
cargo audit --json > audit-report-v0.4.2/cargo-audit.json
cargo audit > audit-report-v0.4.2/cargo-audit.txt

# 归档到本地私有目录（不 commit，不发布）
# 具体路径由 Release Manager 本地确定，统一不进入 git 与发布包
```

> 报告归档至内部目录，不进入 git history。

## 3. 确认文档状态

### 3.1 CHANGELOG.md

- 确认当前版本章节完整，包含所有变更
- 填入实际发布日期：`## [0.2.0] - 2026-XX-XX`
- 确认无 `## [未发布]` 段（发布时未发布段应转为版本段或清空）
- 确认遵循 [Keep a Changelog](https://keepachangelog.com/) v1.0 格式
- 历史段只保留本仓事实，不谈论其他仓

### 3.2 README.md

- 确认版本号与 Cargo.toml 一致
- 确认"使用风险自负"声明存在
- 确认 API 稳定性诚实声明（"1.0 之前不承诺"）

## 4. 创建 Git Tag

```bash
# 1. 确认工作区干净
git status  # 必须无未提交变更

# 2. 确认版本号
grep '^version' Cargo.toml  # workspace.package.version = "0.4.1"

# 3. 创建带注释的 annotated tag
git tag -a v0.4.2 -m "EvoRule Server v0.4.2

规则引擎服务端首个独立稳定 release：
- evorule-server: HTTP API 服务（认证 + 规则热重载 + 时间机器 + OpenAPI 单一真相源）
- core/*: I/O 处理器、调试控制、指标采集、语义不变量、workspace 多租户等

详见 CHANGELOG.md。"
```

## 5. 推送到 Gitee（源仓库）

```bash
# 推送 main 分支 + tag
git push origin main --tags
```

确认 Gitee CI（`.gitee-ci/validate.yml`）在 tag 上通过：

- validate-pr（文档安全 + 版本一致性）✅
- lint（fmt + clippy）✅
- test（cargo test）✅
- build（release 构建）✅
- docker（镜像构建 + 冒烟测试）✅

## 6. 构建 Docker 镜像

```powershell
# 构建本地 Docker 镜像
pwsh scripts/build-docker.ps1

# 冒烟测试（确认镜像可启动且健康检查通过）
docker run --rm -d -p 18080:18080 --name evorule-smoke `
  -e EVORULE_ADDR=127.0.0.1:18080 evorule-server:latest
Start-Sleep -Seconds 5
curl -sf http://127.0.0.1:18080/api/health
docker stop evorule-smoke
```

> Docker 镜像推送到 registry 的流程待 registry 配置后补充。当前 CI 中的 `build-docker` job 仅构建不推送。

## 7. 同步到 GitHub（镜像仓）

> **暂缓**：GitHub 镜像仓尚未配置，本节留作以后稳定了再执行。当前仅发布到 Gitee。

```bash
# 推送 main 分支 + tag
git push github main --tags
```

确认 GitHub CI 在 tag 上通过：

- `ci.yml`：lint + docs-check + test + build + build-docker ✅

## 8. 创建 Release

> **暂缓**：与 §7 同步，GitHub 镜像仓配置后执行。

在 Gitee/GitHub 的 Releases 页面创建 Release：

1. **Tag**: 选择刚推送的 `v0.4.2`
2. **Title**: `EvoRule Server v0.4.2`
3. **Body**: 从 `CHANGELOG.md` 的当前版本章节提取
4. **附加产物**（可选）：Linux x86_64 二进制（`target/release/evorule-server`）、Docker 镜像 tar

## 9. 发布后验证

```powershell
# 1. 发布后验证模式（严格）：tag 必须存在，CHANGELOG 无 [未发布]
pwsh scripts/validate-all.ps1
```

此命令会用默认严格模式运行 7 项检查（gate-bypass + 5 validate 脚本 + check_doc_safety）。`validate-release.ps1` 会检查 tag `v0.4.2` 存在且无更大 tag。

```bash
# 2. 确认 tag 在仓库存在
git tag -l v0.4.2                    # 本地
git ls-remote --tags origin v0.4.2   # Gitee
# GitHub: 暂缓（镜像仓未配置）

# 3. 确认 CI 全绿
# Gitee: 访问 Gitee CI 页面确认 validate.yml 通过
# GitHub: 暂缓（镜像仓未配置）

# 4. 确认 cargo audit 报告已归档（如执行了 §2）
ls -la audit-report-v0.4.2/
```

## 10. 发布后事项

- [ ] 归档本次发布的所有 CI 日志链接
- [ ] 确认 Docker 镜像可用（如执行了 §6）

---

## 附录：紧急回滚流程

如果发布后发现严重问题需要回滚：

```bash
# 1. 删除 Release（如已创建）
# Gitee: 在 Releases 页面手动删除
# GitHub: gh release delete v0.4.2 --yes（暂缓）

# 2. 在仓库删除 tag
git tag -d v0.4.2                          # 本地
git push origin :refs/tags/v0.4.2          # Gitee
# git push github :refs/tags/v0.4.2        # GitHub（暂缓）

# 3. 在 README.md 标注"v0.4.2 已撤回，原因：XXX"
# 4. 修复后以 v0.4.2 重新发布（不覆盖已撤回的 v0.4.2）
```

> **注意**：撤回 tag 是最后手段。如果问题属于非阻塞缺陷，可在不撤回 tag 的前提下发布下一个 patch 版本。仅当源码本身有严重缺陷（如编译失败、数据损坏、安全漏洞）时才撤回。

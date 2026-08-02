<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: AGPL-3.0-or-later
-->

# VERSION_STRATEGY.md

**evorule-server 仓版本策略**

**最后更新**: 2026-07-30
**配套**: [CHANGELOG.md](CHANGELOG.md)

---

## 〇、版本号约定

本仓遵循 [语义化版本控制](https://semver.org/lang/zh-CN/) v2.0:

```
MAJOR.MINOR.PATCH
```

- **MAJOR**: 破坏性变更(HTTP API 路径变更、crate 改名、依赖核心引擎 MAJOR 升级)
- **MINOR**: 新功能(新 HTTP 路由、新 lib、新 feature flag)
- **PATCH**: bug 修复、性能优化、文档更新

**预发布标签**:`v0.1.0-alpha.1` / `v0.1.0-beta.1`(发布前测试)

---

## 一、版本独立性

本仓**独立发布**，不绑核心引擎或其他仓的发布节奏。

核心引擎升级时,本仓只需要更新 `Cargo.toml` 的 `evorule-*` 版本号,无需同步发布。

---

## 二、依赖版本

### 2.1 核心依赖(通过 crates.io)

```toml
# evorule-server/Cargo.toml
[dependencies]
evorule-tcb = { version = "0.1.1" }                         # 跟随核心引擎
evorule-reactor = { version = "0.1.1", features = ["persistence"] }
evorule-governance = { version = "0.1.1", features = ["persistence"] }
```

**升级流程**:
1. 核心引擎先发新版(`v0.1.1` → crates.io)
2. 等 1 天(让 crates.io 索引更新)
3. 本仓改 `Cargo.toml` 的 `version = "0.1.1"`
4. 跑 `cargo test --workspace`,通过后 commit + tag

### 2.2 本地开发 patch

```toml
# 顶层 Cargo.toml
[patch.crates-io]
evorule-tcb = { path = "../evorule/evorule-tcb" }
evorule-reactor = { path = "../evorule/evorule-reactor" }
evorule-governance = { path = "../evorule/evorule-governance" }
```

**这个 patch 段应该 commit**(简化本地开发),**用户 clone 时不影响**(Cargo 自动忽略不存在的 path)。

### 2.3 其他依赖(直接用 crates.io)

- `axum` / `tokio` / `tower` / `prometheus` / `serde` / `clap` 等
- 跟随 crates.io 主流版本,不锁死
- 升级前跑 `cargo test --workspace` 验证

---

## 三、发布清单(checklist)

每个版本发布前,确认以下都 ✅:

- [ ] `cargo build --workspace` 0 error
- [ ] `cargo test --workspace` 0 failed
- [ ] `cargo clippy --workspace --all-targets` 0 error
- [ ] `cargo fmt --all -- --check` 通过
- [ ] `CHANGELOG.md` 加新版本段(🆕/🔄/🐛/🔒)
- [ ] `README.md` 的"已知限制"段更新
- [ ] `Cargo.toml` 版本号更新
- [ ] `git tag vX.Y.Z` + `git push --tags`
- [ ] Gitee Release 描述复制 `CHANGELOG.md` 的 [X.Y.Z] 段
- [ ] Docker image 重新构建并测试(如果有)

---

## 四、版本节奏建议(参考)

| 阶段 | 频率 | 说明 |
|---|---|---|
| v0.1.x | 2-4 周/版本 | 内部基线期,频繁小修 |
| v0.2.0 | 6-8 周 | 第一批用户反馈后,加实用功能 |
| v0.x.0 → v1.0.0 | 3-6 个月 | API 锁定 + 安全审计 + 文档完善 |
| v1.0.0 之后 | 6-8 周/版本 | 正式 release,严格 semver |

**当前阶段**:v0.1.0(内部基线)

---

## 五、Git tag 命名

- `vX.Y.Z` —— 正式发布
- `vX.Y.Z-alpha.N` —— 内部测试
- `vX.Y.Z-beta.N` —— 公开测试
- 不用 `v0.1.0-rc.1` 这种(我们没 RC 阶段)

---

## 六、安全版本

- **alpha 阶段**(当前):Critical/High 漏洞 60 天内修;Medium/Low 推迟到下个版本
- **1.0.0 之后**:Critical 7 天;High 30 天;Medium 90 天;Low 下个 release

详见 [SECURITY.md](SECURITY.md)

---

_本文件是 evorule-server 仓的版本策略参考,具体节奏以实际需要为准。_

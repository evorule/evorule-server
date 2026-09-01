<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: AGPL-3.0-or-later

  This file is part of EvoRule Server, licensed under GNU Affero General Public License v3 or later.
-->

# GATE_REFERENCE (evorule-server 跨模块门控索引)

> **适用范围**: evorule-server (bin) + core/* 9 个配套 lib
> **协议**: AGPL-3.0-or-later
> **状态**: 权威 (build.rs 编译时门禁 + #![forbid(unsafe_code)] + clippy workspace lints)
> **与核心仓的关系**: 本仓用 **S 编号** (Server 专属), 不参与核心仓的 T/G/F 编号体系

---

## 一、三层门控总览

| 层 | 机制 | 强度 | 实施位置 |
| --- | --- | --- | --- |
| **L1 编译时字面量门禁** | `build.rs` 字节子串扫描 | 高 | `evorule-server/build.rs` + `core/io_handlers/build.rs` |
| **L1' 编译器级强制** | `#![forbid(unsafe_code)]` | 最高 | 每个 crate 的 `src/lib.rs` / `src/main.rs` 顶部 |
| **L2 编译时 lint** | clippy workspace lints | 中 | 根 `Cargo.toml` `[workspace.lints]` + 各 crate `[lints]` |
| **L3 评审** | code review (PR review) | 高 | 人工 |

**协作关系**:
- L1 挡 **panic-prone 字面量** (`.unwrap(` / `.expect(` / `panic!(` / `debug_assert!` 在生产代码)
- L1' 挡 **unsafe** (编译器级 forbid, 不可被 allow 覆盖, 比 build.rs 字面量扫描更强)
- L2 挡 **结构违规** (认知复杂度 > 25 / 函数 > 100 行 / panic-prone 语义分析双保险)
- L3 挡 **语义违规** (业务逻辑 / API 设计 / 安全编码 / 跨文件调用图)
- 三层 **独立兜底**: L1 漏了 L2 拦, L2 漏了 L3 拦

---

## 二、与核心仓门控的关键差异

核心仓的核心约束是 **确定性** (TCB 禁 I/O/async/thread/rand/time/float/HashMap)。
evorule-server 仓是 HTTP server 应用层, **不需要确定性约束**, 但需要 **安全约束**。

| 约束 | evorule 核心 (T/G/F 编号) | evorule-server (S 编号) | 原因 |
| --- | --- | --- | --- |
| panic-prone (G1/F11) | ❌ L1 + L2 双保险 | ❌ **L1 + L2 双保险** (S1) | 跨仓一致 |
| unsafe (G2/T10) | ❌ L1 字面量 deny | ❌ **L1' forbid** (更强) | server 仓用 forbid |
| async/tokio (T14) | ❌ tier0 禁止 | ✅ **必需** | axum HTTP server |
| I/O (T4) | ❌ tier0 禁止 | ✅ **必需** | DB/HTTP/Memory handler |
| HashMap (T8) | ❌ tier0 禁止 | ✅ **必需** | session 管理 |
| SystemTime (T5) | ❌ tier0 禁止 | ✅ **必需** | 超时/日志/审计 |
| 控制流硬编码 (G8) | ❌ tier1/tier2 禁止 | N/A | server 不处理规则指令 |
| 业务术语 (S5.2) | ❌ tier1/tier2 禁止 | N/A | server 不处理规则指令 |

**结论**: evorule-server 的 build.rs **只守 S1 (panic-prone)**, 不扫描 async/tokio/IO/HashMap 等确定性约束模式。

---

## 三、build.rs 模式索引

### 3.1 evorule-server (bin) — 4 模式 (S1)

实施文件: `evorule-server/build.rs` (递归扫描 `src/**/*.rs`, 含 `src/api/*.rs`)

| 编号 | 模式 (字节子串) | 门控含义 | 与 evorule 核心的关系 |
| --- | --- | --- | --- |
| S1-debug_assert | `debug_assert!` | panic-prone (debug 断言) | = F11 / T11 |
| S1-unwrap | `.unwrap(` | panic-prone (unwrap) | = F11 / T9 |
| S1-expect | `.expect(` | panic-prone (expect) | = F11 / T9 |
| S1-panic | `panic!(` | panic-prone (panic 宏) | = F11 |

### 3.2 core/io_handlers (lib) — 4 模式 (S1, 跟 bin 相同)

实施文件: `core/io_handlers/build.rs` (递归扫描 `src/**/*.rs`)

**有意重复**: evorule-server (bin) 和 core/io_handlers 用同一组 4 模式, 保证两个安全最敏感的 crate 不会走偏。与核心仓 tier1/tier2 "有意重复 14 模式" 的设计哲学一致。

### 3.3 其余 core/* lib — 靠 L1' + L2 + L3

`core/{auth, debug_control, hot_reload, metrics, rule_tools, semantic_invariants, time_machine, workspace}` 不加 build.rs, 原因:
- 这些 lib 的 panic-prone 由 **L2 clippy deny** 守 (unwrap_used/expect_used/panic = deny)
- unsafe 由 **L1' #![forbid(unsafe_code)]** 守
- 安全敏感度低于 bin (直接面向网络) 和 io_handlers (直接操作 SQL/HTTP)

如未来某个 lib 安全敏感度提升 (如 auth 加密相关), 可按需追加 build.rs。

### 3.4 core/rule_schema (lib) — Schema 完整性门禁 (0.3.0 新增)

实施文件: `core/rule_schema/build.rs` (4KB, 非字节子串扫描, 是 JSON Schema 完整性校验)

**门控内容**:
- 三个内嵌 schema 文件必须是合法 JSON: `schemas/rule_set/v1.0.json` / `schemas/_meta/v1.0.json` / `schemas/_shared/v1.0.json`
- 跨文件 `$ref` 的 `$id` 自洽: rule_set `$id` = `https://evorule.org/schemas/rule_set/v1.0.json`, meta `$id` = `.../_meta/v1.0.json`, shared `$id` = `.../_shared/v1.0.json`
- rule_set 的 `allOf[0]` 必须指向 meta, `transform.items` 必须指向 `shared#/$defs/transform_rule`
- 把"schema 损坏"从运行时问题提前到构建期问题（与转译器同一纪律）

**C5 纪律**: build.rs 本身禁止 unwrap/expect/panic（deny 级 lint），所有失败路径统一以 `Err(String)` 返回，由 `main` 转非零退出码令构建失败。

### 3.5 plugins/demo-services (lib) — 靠 L1' + L2 + L3 (0.3.0 新增)

`plugins/demo-services` 不加 build.rs，原因同 §3.3：panic-prone 由 clippy deny 守，unsafe 由 `#![forbid(unsafe_code)]` 守。作为插件示例 crate，安全敏感度低于核心 bin 和 io_handlers。

### 3.6 豁免机制

- `strip_test_mod()`: 剥离 `#[cfg(test)] mod tests { ... }` 块, 不扫描测试代码
- 注释行豁免: `//` 开头的行 (含 `///`、`//!`) 不扫描
- `EVORULE_SKIP_GATE=1`: 紧急跳过, 编译时输出 `cargo:warning` 提醒

---

## 四、#![forbid(unsafe_code)] 索引 (L1')

每个 crate 的 `src/lib.rs` 或 `src/main.rs` 顶部必须声明 `#![forbid(unsafe_code)]`。

**forbid vs deny**:
- `forbid` 不可被 `allow` 覆盖 (比 `deny` 更强)
- `deny` 可以被内层 `#[allow(unsafe_code)]` 覆盖
- evorule-server 仓选择 `forbid`, 确保绝对零 unsafe

**当前声明状态** (2026-08-01 验证):

| crate | 声明位置 | 状态 |
| --- | --- | --- |
| evorule-server (bin) | `src/main.rs:26`, `src/lib.rs:13` | ✅ |
| core/io_handlers | `src/lib.rs:18`, `db_handler.rs:4`, `http_handler.rs:4`, `memory_handler.rs:4`, `service_registry.rs:4` | ✅ |
| core/auth | `src/lib.rs` | ✅ (代码风格要求) |
| core/debug_control | `src/lib.rs` | ✅ |
| core/hot_reload | `src/lib.rs` | ✅ |
| core/metrics | `src/lib.rs` | ✅ |
| core/rule_tools | `src/lib.rs` | ✅ |
| core/semantic_invariants | `src/lib.rs` | ✅ |
| core/time_machine | `src/lib.rs` | ✅ |

---

## 五、clippy workspace lints 配置 (L2)

### 5.1 根 `Cargo.toml` 配置

```toml
[workspace.lints.rust]
# kani cfg 由 kani 工具链注入, Cargo 不识别, 显式声明以抑制 warning
unexpected_cfgs = { level = "warn", check-cfg = ['cfg(kani)'] }

[workspace.lints.clippy]
# G1: panic-prone (build.rs L1 已守, clippy L2 双保险)
unwrap_used = "deny"
expect_used = "deny"
panic = "deny"
panic_in_result_fn = "deny"
# F8: 嵌套复杂度 (守 cognitive_complexity, 阈值 25)
cognitive_complexity = { level = "warn", priority = -1 }
# F9: 函数长度 (守 too_many_lines, 阈值 100)
too_many_lines = { level = "warn", priority = -1 }
# F10 部分: 类型复杂度
type_complexity = { level = "warn", priority = -1 }
# F6 部分: 模块导入图
module_inception = "warn"
# 通用代码质量 (warn 级, 不阻断 CI)
all = { level = "warn", priority = -1 }
```

### 5.2 各 crate 启用

`evorule-server/Cargo.toml`, `core/*/Cargo.toml`:

```toml
[lints]
workspace = true
```

### 5.3 L1 + L1' + L2 协作

| 规则类别 | L1 (build.rs) | L1' (forbid) | L2 (clippy) |
| --- | --- | --- | --- |
| G1 (panic-prone) | S1 字面量 deny | — | **deny** (双保险) |
| G2 (unsafe) | — (不扫描) | **forbid** (最强) | — (forbid 已守) |
| F8 (认知复杂度) | L1 不能查 | — | **warn** |
| F9 (函数长度) | L1 不能查 | — | **warn** |

**关键**: deny 类 (`unwrap`/`expect`/`panic`) 由 L1 + L2 双保险; unsafe 由 L1' forbid 强制; warn 类 (认知复杂度/函数长度) 由 L2 静态分析 + L3 review 兜底。

---

## 六、跨模块门控图

```
   根 Cargo.toml (L2 clippy 集中配置, 9 lints)
   GATE_REFERENCE.md (本文档, 跨模块索引)
                              |
        +---------------------+---------------------+
        |                     |                     |
   evorule-server          core/io_handlers     其余 8 个 core/* lib
   (bin, 面向网络)          (SQL/HTTP, 安全敏感)   (auth/metrics/...)
        |                     |                     |
   build.rs (L1)          build.rs (L1)          (无 build.rs)
   4 模式 (S1)            4 模式 (S1, 相同)       靠 L1' + L2 + L3
        |                     |                     |
   #![forbid(unsafe)]     #![forbid(unsafe)]     #![forbid(unsafe)]
   (L1' 编译器强制)        (L1')                  (L1')
        |                     |                     |
   [lints] workspace     [lints] workspace     [lints] workspace
   (L2 clippy 继承根)     (L2)                  (L2)
        |                     |                     |
   code review (L3)       code review (L3)       code review (L3)
```

**S1 跨 2 crate** = panic-prone (L1 + L2 双保险)
**forbid 跨 10 crate** = unsafe (L1' 编译器级强制)
**F8-F9 跨 10 crate** = 复杂度 (L2 守)

---

## 七、豁免索引

### 7.1 tests/ + examples/ 文件级豁免

测试代码 + examples 是 Cargo 演示代码, 允许 panic/expect (L1 build.rs 已守 panic-prone 关键路径, clippy 对 tests/ 默认宽松)。

- `evorule-server/tests/integration_test.rs`
- `evorule-server/tests/session_integration_test.rs`
- `evorule-server/tests/fault_recovery_test.rs`
- `evorule-server/examples/bench_*.rs` (3 个)

### 7.2 src/ mod tests 豁免

src/ 内 `#[cfg(test)] mod <ident> { ... }` 块是测试代码, build.rs 的 `strip_test_mod()` 自动剥离 (支持任意 mod 名: `tests` / `whitelist_tests` / `ssrf_tests` 等, 只要被 `#[cfg(test)]` 修饰就整体剥离), 不需要额外 `#[allow]`。

**实现要点** (2026-08-01 修复):
- `skip_to_mod_tests` 匹配紧跟 `#[cfg(test)]` 的任意 `mod <ident>` (不向后搜索, 避免跨代码误匹配远处的 mod)
- `match_brace` 感知原始字符串 `r#"..."#` (不感知会导致花括号计数错乱)
- 拼接只追加闭合 `}` (`src[close_idx..close_idx+1]`), 不追加 `src[close_idx..]` 全部 —— 文件有多个 `#[cfg(test)] mod` 时, 后者会把后续测试模块体重复追加, 导致测试代码被当作生产代码误报

部分文件的 test mod 顶部加了 `#![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]` (跟核心仓一致), 用于 clippy L2 豁免:
- `evorule-server/src/metrics_impl.rs:254-256`
- (其余文件按需添加)

### 7.3 src/ 函数级 cognitive_complexity / too_many_lines 豁免

按"重构成本/收益"权衡, 大型 dispatch / 拆函数影响接口稳定性的生产函数:

| 文件:行 | 函数 | 复杂度/行数 | 理由 |
| --- | --- | --- | --- |
| `evorule-server/src/api/server.rs` | `pub fn build_router` | 103/100 | axum Router 多 route, 已加 `#[allow(clippy::cognitive_complexity)]` |
| `evorule-server/src/api/server.rs` | `fn load_merged_transforms_from_fs` | 已拆分子函数 | 已重构降低复杂度 |
| `evorule-server/build.rs` + `core/io_handlers/build.rs` | `fn match_brace` | 26/25, 109/100 | 单状态机, 6 状态变量 (depth/in_str/in_char/in_line_c/in_block_c/i) 分支间共享, 拆分子函数需传全部状态; 原始字符串 `r#"..."#` 识别是必要分支 (不感知会导致花括号计数错乱)。已加 `#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]` |

每处豁免都配注释说明理由 (deny 类永不豁免 / warn 类按"成本/收益"权衡)。

---

## 八、发布门禁脚本

`scripts/_cargo_gate.ps1` — 发布前集成门禁, 跑 3 项:

1. `cargo test --workspace --locked`
2. `cargo build --workspace --release --locked`
3. `cargo clippy --workspace --locked --all-targets -- -D warnings`

**与核心仓的区别**: 不跑 `cargo package --list` (evorule-server 仓 `publish = false`, 不上 crates.io)。

**build.rs L1 门禁** 随 `cargo build` / `cargo test` / `cargo clippy` 自动执行 — 如果 build.rs 检测到 S1 违规, 编译会失败, 门禁脚本也会失败。

---

## 九、相关文件

- `evorule-server/build.rs` (L1 字面量门禁, 4 模式 S1)
- `core/io_handlers/build.rs` (L1 字面量门禁, 4 模式 S1, 跟 bin 相同)
- `Cargo.toml` (根 `[workspace.lints]` 集中配置)
- `evorule-server/Cargo.toml` + `core/*/Cargo.toml` (各 crate `[lints] workspace = true`)
- `scripts/_cargo_gate.ps1` (发布门禁脚本)
- 代码风格约束: 不写 unsafe / 不写 panic-prone (见本文档 §一/§二)
- 核心仓 `GATE_REFERENCE.md` (T/G/F 编号体系, 本仓 S 编号的源头)

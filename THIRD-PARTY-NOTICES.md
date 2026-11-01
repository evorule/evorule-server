<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: AGPL-3.0-or-later
-->

# THIRD-PARTY-NOTICES — evorule-server 第三方组件使用声明

> 本文件是 evorule-server 仓的第三方组件使用声明，为生态级总声明（主仓 evorule 仓同名文件）的**单仓子集**。
> 首次生成：2026-10-26 · 依赖记录 503 条（cargo 501 + npm 2）· 许可证 18 类正文全部逐字归档。

---

## 〇、一句话结论

**本仓依赖许可证面无禁止级风险；分发义务随五件套（LICENSE + NOTICE.md + 本文件 + LICENSES/third-party/ + SOURCE.md）随包履行，缺件由 dist 检查阻断。**

| 维度 | 结论 |
|---|---|
| 禁止级许可证 | **0 项** |
| 许可证类别 | 18 类（见第二节聚合表），正文归档 **18/18** |
| 依赖记录 | 503 条（cargo 501 / npm 2） |
| 门禁 | license-gate（deny.toml 四闸，复用主仓 workflow 钉 SHA）+ Doc safety 引用完整性 |

## 一、依赖面总览

| 生态 | 记录数 | 说明 |
|---|---|---|
| cargo | 501 | workspace 全部 crate 的传递依赖（基线：生态清单 2026-10-24 扫描） |
| npm | 2 | 自有插件包 `evorule-codegraph-bridge`（AGPL-3.0-or-later，private）+ vendored 上游 `@colbymchenry/codegraph`（MIT，dist 入仓） |

> 注：`plugins/codegraph-bridge/package.json` 另声明 5 个 npm 包（ignore / jsonc-parser / picomatch / tree-sitter-wasms / web-tree-sitter），实测 `server.mjs` 零 import、vendored dist 自包含——不进分发包义务面，故不计入本清单；若未来被实际消费须先收录再分发。

## 二、许可证聚合表

复合表达式（如 `MIT OR Apache-2.0`）按其可选分支拆分统计，同一依赖可能计入多行。

| 许可证 | 依赖记录数 | 义务要点 | 正文归档 |
|---|---|---|---|
| `MIT` | 452 | 保留版权声明与许可证全文，随二进制分发。 | ✅ |
| `Apache-2.0` | 364 | 保留版权声明、许可证全文与变更说明；上游若有 NOTICE 须一并附带（§4(d)）。 | ✅ |
| `Unicode-3.0` | 19 | 保留版权声明与许可文本。 | ✅ |
| `Zlib` | 11 | 保留版权声明与许可文本。 | ✅ |
| `ISC` | 8 | 保留版权声明与许可文本。 | ✅ |
| `BSD-3-Clause` | 7 | 保留版权声明、条款与免责声明。 | ✅ |
| `Unlicense` | 7 | 无义务。 | ✅ |
| `Apache-2.0-WITH-LLVM-exception` | 6 | 保留版权声明与许可正文。 | ✅ |
| `BSD-2-Clause` | 3 | 保留版权声明、条款与免责声明。 | ✅ |
| `BSL-1.0` | 3 | 宽松许可：保留版权声明与正文。 | ✅ |
| `CC0-1.0` | 3 | 公共领域贡献，无义务。 | ✅ |
| `AGPL-3.0-or-later` | 2 | 网络 copyleft：以网络提供服务时须向使用者提供对应源码（§13）。 | ✅ |
| `CDLA-Permissive-2.0` | 2 | 宽松许可：保留版权声明与正文。 | ✅ |
| `LGPL-2.1-or-later` | 2 | 弱 copyleft：本生态统一选择 MIT/Apache 分支，不触发 LGPL 条款。 | ✅ |
| `MIT-0` | 2 | 保留版权声明与许可证全文，随二进制分发。 | ✅ |
| `0BSD` | 1 | 无附加义务。 | ✅ |
| `BSD-1-Clause` | 1 | 保留版权声明与许可正文。 | ✅ |
| `MPL-2.0` | 1 | 文件级 copyleft：被修改的 MPL 文件须以 MPL 公开；分发时保留声明与正文。 | ✅ |

机器可读全量清单见 [`LICENSES/third-party/manifest.json`](LICENSES/third-party/manifest.json)（本仓 503 条，含每条的许可标识与来源）。

## 三、需特别标注的组件

- **`MPL-2.0` — colored**：文件级 copyleft。当前为原样依赖，义务为零；一旦修改其源码并分发该修改文件，须以 MPL-2.0 公开该文件。
- **`CDLA-Permissive-2.0` — webpki-roots（0.26.11 / 1.0.9 双版本）**：宽松许可，仅要求保留声明；根证书内容本身由各 CA 分发，其内容的独立授权不由本仓库承担。
- **`LGPL-2.1-or-later` — r-efi（5.3.0 / 6.0.0）**：三许可表达式（`MIT OR Apache-2.0 OR LGPL-2.1-or-later`），本生态统一选择 MIT/Apache 分支，不触发 LGPL 条款；正文归档仅为表达式分支完整性。
- **`AGPL-3.0-or-later` — evorule-discipline / evorule-hash**：生态自有 crate（git 钉版依赖），非第三方组件；列出仅为清单完整性。
- **`@colbymchenry/codegraph`（vendored）**：上游 MIT；构建产物 dist 入仓（`plugins/codegraph-bridge/vendor/codegraph/`，MIT 正文随附），同版钉 0.9.4，可复现可溯源。
- **`BSD-1-Clause` — fiat-crypto**：三许可表达式分支，正文按分支归档。
- **wasm-host（独立 workspace，随桌面安装包分发）**：`plugins/wasm-host/` 为独立 cargo workspace，随桌面安装包以独立 exe（+ plugin.json / trusted-keys.json / wasm 模块目录）分发；其依赖树（Cargo.lock 186 条，其中 wasmtime 运行时族 27 条，主导许可证 `Apache-2.0-WITH-LLVM-exception`，该类别正文已归档）**尚未并入本声明计数与生态 manifest**——独立 workspace 不在既有扫描面，清单收录与逐包正文归档已登记为后续批。

## 四、归档与复现

- **正文归档**：[`LICENSES/third-party/`](LICENSES/third-party/) 按 SPDX 标识建目录，正文逐字复制自上游（cargo registry cache / npm registry tarball / OSI 与 gnu.org 补取件带 SOURCE.txt 说明来源）。
- **复现方法**：取主仓生态清单 `LICENSES/third-party/manifest.json`，按 `dependencies[].repo == "evorule-server"` 筛选 cargo 子集；npm 面按仓内实测修正（见 `_meta.npm_note`）。
- **基线时效**：cargo 版本号随 Cargo.lock 演进，以下轮生态重扫刷新；许可证集合层面不受补丁升版影响。

## 五、义务边界

1. **AGPL §13 源码提供**：以网络服务形式交付本软件者，须向其用户提供对应源码；入口见 [SOURCE.md](SOURCE.md)。
2. **第三方义务不因商业许可而免除**：持有商业许可（[COMMERCIAL_LICENSE.md](COMMERCIAL_LICENSE.md)）的用户，其使用与分发不再受 AGPL 源码提供与 copyleft 传染约束，但第三方组件的归属义务（版权声明与许可证正文）对两条授权路径同样成立。
3. **分发随附**：分发本仓产物时，须随附五件套与本声明；打包口径见 [SOURCE.md](SOURCE.md)。

## 六、联系信息

- **项目**: EvoRule — 反应式执行引擎
- **邮箱**: <evorulelab@gmail.com>
- **仓库**: <https://github.com/evorule/evorule-server>

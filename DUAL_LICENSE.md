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

# EvoRule 许可选择指南（三选项）

**版本**: 2.0
**生效日期**: 2026-09-08
**适用范围**: EvoRule 生态全集 —— `evorule`、`evorule-server`（自托管）、`evorule-rule`、`evorule-system-rules`、`evo-agent`、`evorule-agent`、`evorule-console-cloud` 的 console 查看器部分等 **A 类开源仓**。闭源运营层（cloud / server 托管变体）属 B 类，另行商用协议。

---

## 一、概述

EvoRule 以 **AGPL-3.0-or-later** 为基础许可证，并为需要闭源使用的用户额外提供两条授权通道：

| 通道 | 名称 | 费用 | 适合谁 |
|---|---|---|---|
| **A** | AGPL-3.0-or-later（开源合规路线） | 免费 | 愿意开源修改版、个人、内部使用 |
| **B** | 免费商业豁免（FCL, Free Commercial License） | 免费 | 符合资格的实体（见下），需闭源嵌入/分发 |
| **C** | 付费商业许可（Commercial License） | 付费 | 不符合 FCL 资格、需买断 copyleft 义务的实体 |

**核心原则**：

- 代码对所有人（含大厂）开放；
- 合规使用（含内部使用、含网络服务且开源修改版）**永久免费**；
- 闭源使用（嵌入闭源产品、运营闭源 SaaS）是**收费商品**，通过 FCL（优先免费）或 Commercial（付费）授权。

`core_eval.json`（EvoRule 宪法）采用 **CC0 1.0 公共领域**，独立于代码协议（任何人可自由实现兼容引擎）。

---

## 二、你是谁 → 走哪条路

| 你的身份 / 场景 | 推荐通道 | 费用 | 主要义务 |
|---|---|---|---|
| 个人、自由职业者、开源项目 | **A. AGPL** | 免费 | 修改并对外提供网络服务的版本也须 AGPL 开源 |
| 企业年营收 **< $10M**（母公司与全部关联公司合并计算） | **B. FCL** | 免费 | self-attest 声明制；保留版权声明、附带本许可、不移除审计链合规标记 |
| 政府机关 / 事业单位 | **B. FCL** | 免费 | 同上 |
| 高校 / 科研院所 | **B. FCL** | 免费 | 同上 |
| 非营利组织 | **B. FCL** | 免费 | 同上 |
| 愿意把修改版开源的任何实体 | **A. AGPL** | 免费 | 同 AGPL 义务 |
| 企业年营收 **≥ $10M** 且不愿开源的其他实体 | **C. Commercial** | 付费 | 联系 <evorulelab@gmail.com> 获取协议 |

> **B2B2B 场景说明**：你是软件二次开发商，把 EvoRule 嵌入你的产品/服务交付给终端用户 ——
> - 若**你自身**符合 FCL 资格，可走 FCL 交付；
> - 你的**终端用户**是否需 Commercial，由终端用户自身身份决定（详见 FCL / Commercial 文本）；
> - EvoRule Project 保留对**未签约厂商的终端用户**与**直接上门的终端用户**的付费直接服务权（转化阀）。

---

## 三、AGPL 路线（A）

详见 [LICENSE](LICENSE)。只要你遵守 AGPL（含 §13 网络条款：通过网络提供服务时必须开源你的修改版），使用**永久免费**，包括：

- 内部使用（不对外部提供服务）
- 个人学习研究
- 教育用途
- 非营利公益项目
- 任何愿意开源修改版的商业 / 非商业使用

---

## 四、免费商业豁免（B / FCL）

详见 [FREE_COMMERCIAL_LICENSE.md](FREE_COMMERCIAL_LICENSE.md)。符合条件的实体**无需付费**即可闭源使用、修改、嵌入、分发 EvoRule，并免 AGPL 传染与网络条款义务。采用 **self-attest 声明制**，无许可密钥、无强制遥测。

---

## 五、付费商业许可（C / Commercial）

详见 [COMMERCIAL_LICENSE.md](COMMERCIAL_LICENSE.md)。**买断** AGPL §4-6 与 §13 的全部 copyleft 义务，闭源自由使用。联系 <evorulelab@gmail.com>。

---

## 六、协议分离（关键）

| 资产 | 协议 | 说明 |
|---|---|---|
| **代码**（Rust） | AGPL-3.0-or-later | copyleft，保护 EvoRule 当前实现，阻止"白嫖 fork 后卖闭源 SaaS" |
| **`core_eval.json`**（宪法） | **CC0 1.0 公共领域** | 解释器规范，任何人都可自由实现兼容引擎，无需保留版权声明 |

这把"标准"和"实现"分开，类似 HTTP 规范（W3C 公共）vs Apache HTTP Server（版权）。

---

## 七、常见问题

### Q1: 我可以在公司内部使用 AGPL 版本吗？

**A**: 可以。内部使用（不对外提供服务）走 AGPL 即可，**零义务**。若你通过修改版对外提供网络服务，则需按 AGPL §13 开源修改版，或改走 FCL / Commercial。

### Q2: FCL 与 Commercial 的区别？

**A**: FCL 仅对符合资格的实体免费（门槛见 FCL 文本）；不符合资格的实体须购买 Commercial。两者都授予闭源使用权。

### Q3: 我可以从 AGPL 升级到商业许可吗？

**A**: 随时可以。联系 <evorulelab@gmail.com>。

### Q4: 商业许可是永久还是订阅？

**A**: 提供永久与订阅两种，细节见 [COMMERCIAL_LICENSE.md](COMMERCIAL_LICENSE.md)。

### Q5: 我可以基于 `core_eval.json` 实现自己的 EvoRule 引擎吗？

**A**: **可以**，这是 CC0 公共领域的目的。你的实现只需自选许可证（AGPL / 商业 / 闭源），但不是 "EvoRule"，只是 "EvoRule 兼容"。

### Q6: 自由职业者 / 个人开发者需要商业许可吗？

**A**: 不需要。个人使用、学习、内部工具，AGPL 即可。若把基于 EvoRule 的服务**卖给客户**，才需 FCL / Commercial。

### Q7: 大厂可以免费用吗？

**A**: 可以走 **AGPL**（开源修改版并对外服务）。若大厂要**闭源嵌入 / 运营**，须购买 **Commercial**（FCL 仅对 <$10M 等合格实体免费）。

---

## 八、联系信息

- **商业许可咨询**: <evorulelab@gmail.com>
- **组织**: [EvoRule Lab](https://gitee.com/evorule)
- **Gitee**: <https://gitee.com/evorule/evorule-server>

---

## 九、法律声明

本文档**不构成法律建议**。如有法律疑问，请咨询专业律师。

EvoRule 的知识产权归 EvoRule Project 所有。

---

## 版本历史

| 版本 | 日期 | 变更说明 |
|---|---|---|
| 1.0 | 2026-07-19 | 初版 |
| 2.0 | 2026-09-08 | C1 三选项重写：$10M 门槛（含关联合并）、FCL 免费闭源豁免、B2B2B 说明；删除 $1M-$10M 分层收费表与"内部使用建议买商业许可"表述 |

---

**最后更新**: 2026-09-08
**文档版本**: 2.0


---

## 十、EvoRule-Server 托管变体说明（footnote）

`evorule-server` 的**自托管核心**属于 **A 类开源**（AGPL-3.0-or-later，纳入本三文本架构），允许任何人自托管部署（单租户、由用户自行运维），始终适用本仓库 AGPL 与三文本。

若以**多租户 SaaS 运营变体**对外提供服务（即 DEC-2026-001 D-001-08 所述 B 类托管变体），该运营层不在本仓库 AGPL 文本覆盖范围内，须另行签署商用协议（Commercial License 或 FCL 资格评估）。托管运营与自托管部署的边界以"是否由 EvoRule Project 集中多租户运营"划分。

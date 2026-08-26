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

# EvoRule Server 双重许可说明

**版本**: 1.0
**生效日期**: 2026-08-26
**适用范围**: EvoRule Server 及 server 配套库(`core/auth`、`core/io_handlers`、`core/metrics`、`core/hot_reload`、`core/debug_control`、`core/semantic_invariants`、`core/time_machine`、`core/rule_tools`、`core/rule_schema`、`core/workspace`、`plugins/demo-services`、`evorule-server`)

---

## 概述

EvoRule Server 采用**双轨许可模式**,为不同用户提供灵活选择:

1. **AGPL-3.0-or-later 开源许可** — 免费使用,适合开源项目和个人开发者
2. **商业许可** — 付费使用,适合企业闭源产品和商业应用

核心引擎以 crates.io 形式发布(`evorule-tcb` / `evorule-reactor` / `evorule-governance`),其双许可由[核心仓](https://gitee.com/evorule/evorule)的 `DUAL_LICENSE.md` 管辖;本文档管辖 **Server 仓本身及 server 配套库**。

---

## AGPL-3.0-or-later 开源许可

### 适用场景

- ✅ 开源项目(必须同样采用 AGPL-3.0 或兼容许可证)
- ✅ 个人学习和研究
- ✅ 内部工具(不对外提供服务)
- ✅ 教育用途
- ✅ 非营利公益项目

### 主要义务

根据 AGPL-3.0-or-later 许可证,如果您:

- 修改了 EvoRule Server 代码
- 通过网络向用户提供服务

则您必须:

- 公开您的源代码(包括修改部分)
- 提供获取源代码的方式
- 保留原始版权声明和许可证

### 限制

- ❌ 不能将 EvoRule Server 用于闭源商业产品
- ❌ 不能在 SaaS 服务中使用而不公开源代码
- ❌ 不能移除或修改版权声明

---

## 商业许可

### 适用场景

- ✅ 企业闭源产品
- ✅ SaaS 服务(无需公开源代码)
- ✅ 商业软件集成
- ✅ 专有系统开发
- ✅ **白标改名部署**(将 EvoRule Server 改名为您的产品名后向第三方提供的)
- ✅ 需要技术支持和 SLA 保障

### 主要优势

- **无需公开源代码** — 您可以在闭源产品中使用
- **无 AGPL 传染性** — 您的代码不受 AGPL 约束
- **商业友好** — 适合企业级应用
- **法律保护** — 获得明确的商业使用授权
- **技术支持** — 可选的技术支持和咨询服务

### 定价方案

| 方案 | 价格 | 适用对象 |
|---|---|---|
| 初创企业 | 联系询价 | 年收入 < $1M 的公司 |
| 中小企业 | 联系询价 | 年收入 $1M-$10M 的公司 |
| 大型企业 | 联系询价 | 年收入 > $10M 的公司 |
| 教育机构 | 优惠价格 | 学校和科研机构 |
| 非营利组织 | **免费**(申请) | 见 [FREE_COMMERCIAL_LICENSE.md](FREE_COMMERCIAL_LICENSE.md) |

**联系方式**: <evorulelab@gmail.com>

---

## 协议分离(关键)

EvoRule Server 的**代码 / 文档 / 宪法**采用**不同协议**:

| 资产 | 协议 | 说明 |
|---|---|---|
| EvoRule Server 代码(Rust) | AGPL-3.0-or-later / 商业许可 | copyleft,保护当前实现 |
| **`docs/` 下文档** | **CC-BY-4.0** | 文档自由引用,须署名 |
| **`resources/core_eval.json`(宪法)** | **CC0 1.0 公共领域** | 解释器规范(自核心仓同步),任何人都可自由实现 |

---

## 白标授权边界(Server 特有) ⚠️

因 Server 仓面向"为第三方提供服务"的商业模式,特别明确以下边界:

1. **为第三方提供白标 EvoRule Server 服务**,或将 EvoRule Server **改名改制品后交付/售卖**给客户并收费 → **需商业授权**。
2. **服务公司(SI / ISV)** 基于 EvoRule Server 为客户搭建系统,若:
   - 系统**完全免费**交付给终端用户,且**不转售软件许可/不按次计费** → 视为内部工具,可用 AGPL;
   - 系统**向客户收取软件授权费、订阅费或按使用量计费** → **需商业授权**。
3. 单纯**帮助你自己的客户内部部署**(不涉及转售 EvoRule 本身) → 参照第 2 条判断,收费点若仅限人工服务则可维持 AGPL;一旦按软件/服务授权收费则需商业许可。

> 上述边界与[核心仓](https://gitee.com/evorule/evorule)双许可一致,仅针对 Server 的应用形态补充说明。

---

## 常见问题

### Q1: 我可以在公司内部使用 AGPL 版本吗?

**A**: 可以。如果您的内部工具不对外部用户提供服务,可以使用 AGPL 版本而无需公开代码。但如果通过 Web 界面向员工提供服务,从严格的 AGPL 解释角度,可能需要公开代码。建议企业内部使用选择商业许可以避免法律风险。

### Q2: 商业许可是否包含技术支持?

**A**: 基础商业许可不包含技术支持,但可以购买额外的支持套餐。详情请咨询销售团队。

### Q3: 我可以从 AGPL 升级到商业许可吗?

**A**: 可以。您可以随时从 AGPL 切换到商业许可,只需联系销售团队即可。

### Q4: 商业许可是永久的还是订阅制?

**A**: 我们提供两种选项:

- **永久许可** — 一次性付费,永久使用该版本
- **订阅许可** — 年费制,包含所有更新和技术支持

### Q5: EvoRule Server 和核心引擎的许可关系是什么?

**A**: Server 依赖的 `evorule-tcb / reactor / governance` 以 crates.io 发布,遵循核心仓双许可;Server 仓自身代码遵循本文档。两者都属于 EvoRule Project 的 AGPL + 商业双授权体系。

### Q6: 我想给客户部署一套不限量的 rule server,需要商业许可吗?

**A**: 如果这套部署对客户是赠送的、且您不向客户转售软件/按次收费,可维持 AGPL。一旦对客户**按软件授权、订阅或用量收费**,需要商业许可。见上文"白标授权边界"。

---

## 联系方式

- **销售咨询**: <evorulelab@gmail.com>
- **技术支持**: <evorulelab@gmail.com>(同邮箱)
- **Gitee 组织**: <https://gitee.com/evorule>

---

## 法律声明

本文档**不构成法律建议**。如有法律疑问,请咨询专业律师。

EvoRule Server 的知识产权归 EvoRule Project 所有。

---

## 版本历史

| 版本 | 日期 | 变更说明 |
|---|---|---|
| 1.0 | 2026-08-26 | 初版,对齐核心仓双许可体系,针对 Server 应用形态补充白标授权边界 |

---

**最后更新**: 2026-08-26
**文档版本**: 1.0
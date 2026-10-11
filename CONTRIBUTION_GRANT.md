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

# EvoRule 社区贡献授权（Contribution Grant, 通道 D）

**版本**: 1.0
**生效日期**: 2026-10-11
**签发人**: Mr. DAMU ZHENG（EvoRule Project 创始人、唯一版权人）

---

## 一、目的

开源社区与学术科研生态（评测基准、研究框架、学术会议 artifact、教育项目等）常要求
贡献物以宽松开源协议（如 Apache-2.0）进入其代码仓。EvoRule 基础许可证为
AGPL-3.0-or-later，两者的**协议传染性不兼容**：AGPL 源码或其编译产物进入 Apache-2.0
仓会阻断该仓的协议合规。

本通道由版权人**主动签发再授权**，使 EvoRule 贡献物可以宽松协议进入合格的目标社区，
**同时 EvoRule 主仓与其生态的 AGPL 主线完全不变**。这不是给单个项目特权，而是生态级
机制：凡符合本许可资格的目标，均按同一程序获得同类授权。

## 二、资格（目标社区须满足其一）

1. **开源社区项目**：以公认开源协议（OSI 认定）发布、公开运作的社区项目（含评测基准、
   开发者工具、公益基础设施）；
2. **学术科研项目**：高校、科研院所主导或深度参与的研究项目（含学术会议 artifact、
   期刊附属代码、竞赛评测框架）；
3. **教育与非营利项目**：教育机构课程项目、非政府非营利组织项目。

**排除**：不符合上述资格的商业实体（含年营收 ≥ ¥1 亿企业）不适用本通道；其需求由
[FCL](FREE_COMMERCIAL_LICENSE.md) 或 [商业许可](COMMERCIAL_LICENSE.md) 承接。

## 三、授权物（Grant Object）

一次签发针对一份**明确列明的贡献物清单**。清单中的每一项称为「授权物」，包括：

1. **贡献物本体**：由 EvoRule Project 向目标社区提交的代码、文档、规则集、构建产物；
2. **内含生态代码子集**：贡献物编译产物中静态或动态包含的 EvoRule 生态源码子集
  （如 wasm 二进制内含的 `evorule-tcb` / `evorule-reactor` / `evorule-governance`
   编译产物）——**此口径为签发完整性所必需**：仅授权贡献物本体而漏掉其内含的上游
   代码，会在目标仓合规审查中留下协议断链；
3. **不含**：未列入清单的一切（EvoRule 全仓其余部分、商标、"EvoRule" 名号的
   商品化使用——后者见 [TRADEMARK.md](TRADEMARK.md)）。

## 四、授予

对每份签发的授权物清单，版权人授予目标社区一项**全球、非独占、免版税、永久、
不可撤销**的许可：将该授权物以 **Apache-2.0**（或版权人事先认可的更宽松协议，
如 MIT）再许可、分发、修改与再分发。

- **主仓不变原则**：本授予是 EvoRule 侧的局部再授权；EvoRule 主仓及全部生态仓的
  AGPL-3.0-or-later 许可证**不受影响、不降级、不废止**；
- **不可撤销性边界**：授予不可撤销，但依第六节义务的重大违约可终止**后续**授权
  （已依 Apache-2.0 分发的副本不受影响——与 Apache-2.0 §4 一致）；
- `core_eval.json`（宪法）本身为 CC0 1.0 公共领域，任何人可自由实现兼容引擎，
  无需本通道。

## 五、义务

被授权社区（及其下游分发者）须：

1. **署名保留**：在授权物的分发中保留 EvoRule 版权声明与 NOTICE 署名（Apache-2.0
   §4 本有此要求，此处重申）；
2. **源指针**：授权物中保留指向 EvoRule 主仓的源代码指针（源码 URL 或 commit 锚），
   使下游使用者可溯源；
3. **非背书声明**：不得以 EvoRule 名义暗示 EvoRule Project 背书目标社区或其观点
   （商标使用守 [TRADEMARK.md](TRADEMARK.md)——描述性使用与兼容性声明允许）；
4. **范围诚实**：不得宣称授权范围超出签发清单（例如把「wasm 判卷组件」说成
  「EvoRule 全仓已 Apache 化」）。

## 六、终止

- 一般违约：收到书面通知 15 日内纠正，授权继续；
- 重大违约（移除版权声明并拒绝恢复 / 恶意伪造授权范围声明）：该份签发终止，
  但终止前已依 Apache-2.0 分发的副本不受影响；
- 本通道整体停用仅影响**后续签发**，已签发的授权物授权不变。

## 七、签发程序（Grant Procedure)

1. **清单起草**：贡献物清单（逐文件/逐产物列明+BLAKE3 或 commit 锚定版本）；
2. **版权人签发**：唯一版权人（Mr. DAMU ZHENG）签字批准——单签即生效
  （依据 [AUTHORS.md](AUTHORS.md)：唯一作者；[GOVERNANCE.md](GOVERNANCE.md) §一：
   创始人对许可证拥有最终拍板权）；
3. **落锚**：签发记录（清单+日期+签字）存于本仓 `docs/grants/` 目录，随主仓
   git 历史锚定；目标社区提交包附授权书副本。

## 八、与既有通道的关系

| 通道 | 给谁 | 给什么 | 生态主线 |
|---|---|---|---|
| A. AGPL | 任何人 | 开源使用，修改版须开源 | 基础 |
| B. FCL | 合格实体（政府/高校/科研/非营利/中小企业/个人） | 免 AGPL 义务的闭源使用+分发 | 免费豁免 |
| C. Commercial | 不合格实体（大厂） | 付费买断 copyleft 义务 | 付费 |
| **D. Contribution Grant** | **开源社区/学术科研/教育非营利** | **贡献物以宽松协议（Apache-2.0）进入其仓** | **局部再授权** |

## 九、法律声明

本文档不构成法律建议。EvoRule 的知识产权归 EvoRule Project 所有。

---

**签发人签字**：__________________　Mr. DAMU ZHENG
**日期**：2026 年 10 月 11 日

## 版本历史

| 版本 | 日期 | 变更说明 |
|---|---|---|
| 1.0 | 2026-10-11 | 首建：通道 D 社区贡献授权（首例签发对象=ALE 可验证判卷组件） |

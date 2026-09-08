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

# EvoRule 免费商业豁免许可（FCL, Free Commercial License）

**版本**: 2.0
**生效日期**: 2026-09-08
**适用范围**: EvoRule 生态 A 类开源仓（见 [DUAL_LICENSE.md](DUAL_LICENSE.md)）

---

## 一、目的

EvoRule 基础许可证为 **AGPL-3.0-or-later**。为支持个人、中小企业、公共部门与非营利活动，EvoRule Project 向符合资格的实体**免费**授予一份**商业豁免**：在免于 AGPL 传染义务的前提下闭源使用、修改、嵌入、分发 EvoRule。本许可**不收取授权费**。

---

## 二、资格（满足任一即可）

1. **自然人个人**（含自由职业者）；
2. **企业年营收 < 1,000 万美元（USD $10,000,000）**，计算口径为**母公司与全部关联公司合并计算**（堵拆壳白嫖）；
3. **政府机关与事业单位**；
4. **高校与科研院所**；
5. **非政府非营利组织**。

不满足上述任一条件（典型：年营收 ≥ $10M 的其他企业 / 组织）须购买 [商业许可](COMMERCIAL_LICENSE.md)。

---

## 三、授予

在资格成立且遵守第四节义务的前提下，授予被授权方一项**全球、非独占、免版税、不可撤销**的许可，使其免于 AGPL-3.0 §4-6（分发 copyleft）与 §13（网络条款）义务，并有权：

- 将 EvoRule 修改、嵌入、链接进被授权方专有代码，形成**闭源组合作品**，而该组合作品无需适用 AGPL；
- 通过内部网络或互联网提供基于 EvoRule 修改版的服务，而无需公开服务端代码；
- 在授权范围内**无限量复制与部署**。

本许可可再许可给**同一法律实体内部**使用，但**不可转让**给第三方实体。

---

## 四、组合作品边界（明确写入）

- **构成组合作品（触发本豁免的收费锚点）**：库级链接、进程内嵌入、静态 / 动态链接 —— 典型如嵌入式固件、边缘设备运行时。此类闭源分发须依本 FCL 或商业许可。
- **明确排除（不触发 copyleft，无需本许可）**：独立进程之间通过**公开网络 API（HTTP / gRPC 等）**调用 EvoRule 服务。此情形视为**分离作品**，AGPL §13 的"对应源"义务仅及于 EvoRule 服务本身（其已开源），**不影响调用方代码**。

---

## 五、义务

- 保留 EvoRule 原版权声明、免责声明及本授权声明；
- 再分发副本时**附带本许可文本**；
- **不得移除或隐藏** EvoRule 审计链合规标记（用于事后合规取证）；
- 商标使用仍须遵守 [TRADEMARK.md](TRADEMARK.md)。

---

## 六、声明制（self-attest）

本豁免采用**声明制**：**无需事先审批、无需许可密钥、无强制遥测**。被授权方自行判断资格并**留存一份自我声明记录**（实体类型、营收区间或公共部门身份）。

EvoRule Project 保留在合理怀疑时要求被授权方确认资格的权利；如资格不实，授权按第七节终止。

---

## 七、期限与终止

- 资格持续满足且未违约，授权持续有效。
- **一般违约**：收到书面违约通知后 **15 日（治愈期）** 内纠正，授权继续；逾期未纠正，授权自动终止，被授权方须停止使用并销毁相关副本。
- **故意或重复违约**：经一次违约纠正后再次实质性违约，授权**永久终止**。
- **专利诉讼**：被授权方就 EvoRule 对版权人或其被许可人提起专利侵权诉讼，本许可（含专利授权）**即时终止**。
- 终止不影响终止前已依 AGPL 分发的副本。

---

## 八、常见问题（FAQ）

### Q1: 我已经在 AGPL 下使用 EvoRule，还需要 FCL 吗？

**A**: 若你属于合格实体且希望**免于公开自己修改的代码**，则需 FCL。若遵循 AGPL 开源所有修改，则不需要。

### Q2: FCL 可以给多家关联机构使用吗？

**A**: 同一法律实体内可再许可；独立法人实体须各自符合资格（关联公司合并计算营收，但各自使用仍须自身合格）。

### Q3: 商业企业能否通过捐赠获得 FCL？

**A**: 不能。商业企业无论是否捐赠，年营收 ≥ $10M 即须购买商业许可；FCL 仅限上述合格实体。

### Q4: 开源项目可以使用 EvoRule 吗？

**A**: 可以，直接用 AGPL-3.0-or-later。开源项目不需要 FCL，但其代码也须 AGPL 兼容。

### Q5: 我的机构未来转为营利性，授权会终止吗？

**A**: 会。资格变更（如公立改制为营利）须书面通知 EvoRule Project，原 FCL 终止；可改购商业许可。

---

## 九、联系与版本

**疑问咨询**: <evorulelab@gmail.com>（主题加 `[Free Commercial License]`）

---

## 版本历史

| 版本 | 日期 | 变更说明 |
|---|---|---|
| 1.0 | 2026-07-19 | 初版（表单审批制） |
| 2.0 | 2026-09-08 | 改为声明制（无 Key / 无遥测）；明确组合作品边界（网络 API 排除条款）；合并营收门槛 $10M（含关联公司）；15 日治愈期 / 专利诉讼即时终止 |

---

**本政策最终解释权归 EvoRule Project 所有。本文档不构成法律建议，正式使用前建议咨询专业律师。**

<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: AGPL-3.0-or-later
-->

# EvoRule Server — 源码提供入口

本项目以 **AGPL-3.0-or-later** 授权。根据 AGPL-3.0 第 13 条（§13），以网络服务形式交付本软件者，须向其用户提供对应的完整源码。本文件是 evorule-server 分发物（win64.zip / linux64.tar.gz 零依赖分发包、Docker 镜像）随附的**源码提供入口**：任何拿到分发物的人，都能凭本文件定位并校验与该分发物对应的源码。

## 当前版本指纹

| 字段 | 值 |
|---|---|
| 版本 | v0.9.2 |
| 发布 tag | `v0.9.2` |
| 源码归档（Gitee） | <https://gitee.com/evorule/evorule-server/repository/archive/v0.9.2.zip> |
| 源码归档（GitHub 镜像） | <https://github.com/evorule/evorule-server/archive/refs/tags/v0.9.2.tar.gz> |
| 源码 tarball SHA256 | 见发布 Release 说明的 `sha256-checks.txt` |

> 每次发版时更新本表；打包流程须以**本次发布对应的值**覆盖后随包分发，禁止携带未填写的占位值。

## 获取与校验方式

1. **本仓**：<https://gitee.com/evorule/evorule-server>（权威源；GitHub 为镜像）
2. **按 tag 取归档**：上表任一归档链接，或 `git clone` 后 `git checkout <tag>`
3. **完整性校验**：下载归档后计算 SHA256，与发布 Release 说明中 `sha256-checks.txt` 记录值比对，一致即证明与发布产物同源

## 分发物随附规范

本仓分发物打包时须在包内根目录随附五件套：

| 文件 | 作用 |
|---|---|
| `LICENSE` | AGPL-3.0-or-later 正文 |
| `NOTICE.md` | 版权声明、协议选择指引、第三方组件指向 |
| `THIRD-PARTY-NOTICES.md` | 第三方组件归属义务总声明（本仓单仓子集） |
| `LICENSES/third-party/` | 第三方许可证正文逐字归档 |
| `SOURCE.md` | 本文件——§13 源码提供入口（按次发布填写指纹表） |

打包口径与核对工具见根仓 evorule 仓 `scripts/check-dist-compliance.ps1`；第三方声明维护与复现 SOP 见 [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md)。

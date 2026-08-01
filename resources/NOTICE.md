<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: CC0-1.0

  本目录下的 core_eval.json 与 evorule 主仓的 evorule-tcb/core_eval.json
  保持同步（手动同步，主仓发布后需要更新本仓副本）。
  CC0 协议让任何人都可以自由使用、修改、转发。
-->

# evorule/resources/ 目录说明

## 包含内容

- `core_eval.json` —— EvoRule 核心的"宪法",定义 3.5 个元指令(set/push/branch/io_request) + 6 个基本域(eq/lt/gt/exists/instruction/all)。

## 协议

- `core_eval.json` 文件本身:**CC0-1.0**(公有领域),任何人可自由使用。
- 来源:`evorule/evorule-tcb/core_eval.json`(主仓)。

## 同步策略

- 主仓发布新版本后,本目录需要同步更新。
- 推荐:在主仓 CI 里加 step 自动同步到 evorule-server 仓(后续 Phase)。
- 当前:手动 cp + commit。

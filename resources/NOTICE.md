<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: CC0-1.0

  本目录下的 core_eval.json 与核心仓的 evorule-tcb/core_eval.json 同源:
  基础最小评估集来自核心仓;自本仓 v0.4.1 起含本仓消费面专属的会话桥接
  规则(call_external/call_service),与核心副本为设计内分叉,版本号与内容
  均不要求一致(见本仓 CHANGELOG v0.4.1/v0.4.2)。
  CC0 协议让任何人都可以自由使用、修改、转发。
-->

# evorule/resources/ 目录说明

## 包含内容

- `core_eval.json` —— EvoRule 核心的"宪法",定义 3.5 个元指令(set/push/branch/io_request) + 6 个基本域(eq/lt/gt/exists/instruction/all)。

## 协议

- `core_eval.json` 文件本身:**CC0-1.0**(公有领域),任何人可自由使用。
- 来源:`evorule/evorule-tcb/core_eval.json`(核心仓)。

## 同步策略

- 两份宪法为**同源分叉**:本副本 = 核心最小评估集 + 本仓消费面会话桥接规则(自 v0.4.1)。
- 核心仓最小评估集发生变更时,应人工将变更合入本副本(桥接规则自持,不受影响)。
- 版本号各自独立演进,不要求一致。

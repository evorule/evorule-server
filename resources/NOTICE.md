<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: CC0-1.0

  本目录下的 server_eval.json(v0.4.1 前旧名 core_eval.json,UV-044)与核心仓的
  evorule-tcb/core_eval.json 为同源分叉:基础最小评估集来自核心仓;自本仓 v0.4.1
  起含本仓消费面专属的会话桥接规则(call_external/call_service),与核心副本为
  设计内分叉,版本号与内容均不要求一致(见本仓 CHANGELOG v0.4.1/v0.4.2)。
  CC0 协议让任何人都可以自由使用、修改、转发。
-->

# evorule/resources/ 目录说明

## 包含内容

- `server_eval.json` —— EvoRule server 业务规则集("宪法"):最小评估集(元指令 set/push/branch/io_request/collect/merge + 域类型 eq/lt/gt/exists/instruction/all/has_fields/not/any)+ 会话桥接规则(call_external/call_service 单发桥接,自 v0.4.1)。

## 协议

- `server_eval.json` 文件本身:**CC0-1.0**(公有领域),任何人可自由使用。
- 来源(基础最小评估集):`evorule/evorule-tcb/core_eval.json`(核心仓)。

## 演进策略(UV-043 职责模型 + UV-044 更名)

- 两份文件职责不同:核心仓 = 宪法原则(TCB 自评估最小集);本仓 = server 业务规则集(最小集 + 消费面桥接规则)。
- **独立演进**:本副本 = 核心最小评估集 + 本仓消费面会话桥接规则(自 v0.4.1);核心仓最小评估集发生变更时,由维护者按需人工评估是否合入本副本(桥接规则自持,不受影响)。
- 版本号各自独立演进,不要求一致。
- 更名记录:v0.4.1 起 `core_eval.json` → `server_eval.json`(UV-044,与核心仓宪法原则在文件名层面区分)。

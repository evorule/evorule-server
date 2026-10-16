<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: AGPL-3.0-or-later

  This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
-->

# evorule-tcb

> EvoRule 三层架构的 Tier 0 可信计算基 (Trusted Computing Base) ——零依赖、`no_std` 兼容的纯计算内核。

- **版本**:v0.6.0
- **定位**:纯函数 + 确定性 + 永不 panic
- **外部依赖**:0（`Cargo.toml` `[dependencies]` 为空；`Cargo.lock` 中仅 dev-dependencies（proptest/criterion，测试与基准用，不进发布产物），运行时零第三方 crate）
- **测试**:`cargo test` 全量 PASS / 0 failed（2026-09-14 v0.6.0 全量回归，CI 常驻）
- **Clippy**:零警告(`deny(unwrap_used/expect_used/indexing_slicing/panic)`)
- **build.rs 编译时门禁**:24 个禁用模式 (T 编号 + F11 panic-prone 组) + BOM 检测 编译期强制,PASSED
- **协议**:AGPL-3.0-or-later(代码) + CC0-1.0(`core_eval.json` 公共领域)

> **Kani 形式化验证**:34 个 `#[kani::proof]` 分 A/B 两档(v0.6.0 随规则清理退役 P15/P16/P17,原 37 个)——A 档 14 个 v0.6.0 重跑(2026-09-14,`25c0cc0`)全 PASS 并入 CI 闸门;B 档 20 个实测 600s/3600s 超时,判定当前不可运行(proptest 间接覆盖)。详见 [`TCB_SPEC.md` §六](TCB_SPEC.md#六形式化验证-kani-proof) 与 [`docs/KANI.md`](docs/KANI.md);状态唯一权威:[`verification/STATUS.md`](../verification/STATUS.md)。

> 本 crate 属于 [EvoRule](https://gitee.com/evorule) 生态:[主仓](https://gitee.com/evorule/evorule) ｜ [在线控制台 Demo](https://evorule.github.io/evorule-console-cloud/) ｜ [evorule-server（应用层）](https://gitee.com/evorule/evorule-server)

---

## 一、项目定位

`evorule-tcb` 是 EvoRule 三层架构的最底层:

```text
┌─────────────────────────────────────────────┐
│  evorule-governance  治理层（I/O 订阅者/审计/API） │
├─────────────────────────────────────────────┤
│  evorule-reactor     反应式执行器（Fact/MPSC/主循环）│
├─────────────────────────────────────────────┤
│  evorule-tcb         纯计算内核（本项目）          │
└─────────────────────────────────────────────┘
```

**职责**：执行一步状态转换——读取当前指令 + 业务状态 + 队列，依据 `core_eval.json` 编译出的 transform 规则列表，产生新状态或 I/O 请求信号。

**不承担**：I/O 执行、并发调度、持久化、网络通信。这些职责由上层 evorule-reactor / evorule-governance 承担。

### 设计原则

| 原则       | 实现                                                                                            |
| ---------- | ----------------------------------------------------------------------------------------------- |
| 零依赖     | `#![no_std]` + `extern crate alloc`；`Cargo.toml` 中无任何外部 crate                            |
| 纯函数     | 所有公开函数无副作用；相同输入 → 相同输出                                                       |
| 确定性     | `BTreeMap` 保证迭代顺序；`Vec` 顺序明确；无 `Float` 类型                                        |
| 永不 panic | 路径解析返回 `Option`；`checked_add/checked_sub`；递归深度限制；Clippy lint 强制禁用 panic 模式 |
| 可审计     | 整数为 `i64`；无 `unsafe`；无浮点；`core_eval` 运行时结构校验（规则数上限 / 路径解析 / 未知指令报错；`core_eval.json` 由装载层在运行时读入并解析后传入，TCB 无编译期校验） |

### 快速开始（3 行）

```rust
use evorule_tcb::{execute_transition, JsonValue};

let core_eval = vec![];                  // 空宪法 => 不做任何转换
let instr = JsonValue::object_from_pairs(&[("type", JsonValue::string("increment")), ("params", JsonValue::object_from_pairs(&[("attr", JsonValue::string("x")), ("delta", JsonValue::Integer(1))]))]);
let payload = JsonValue::object_from_pairs(&[("x", JsonValue::Integer(5))]);
let queue = vec![];
let result = execute_transition(&core_eval, &instr, &payload, &queue).unwrap();
// → State { payload: {"x": 6}, queue: [] }（如果 core_eval 正确映射了 increment）
```

---

## 二、核心概念

### 2.1 元指令（Meta Instructions）

执行器识别 **5 种元指令** —— 一切业务语义均由 `core_eval.json` 通过组合这些元指令实现：

| 元指令       | 作用                                                 | 修改状态 | 说明 |
| ------------ | ---------------------------------------------------- | -------- | ---- |
| `set`        | 修改 payload 中某字段（支持 `set`/`add`/`sub` 操作，attr 支持数组索引） | 是       | 原子计算 |
| `push`       | 将指令列表推入 queue 前端                            | 是       | FIFO 队列语义 |
| `branch`     | 按域条件执行 `on_true` 或 `on_false` 子指令列表      | 视子指令 | 控制流 |
| `io_request` | 产生 I/O 请求信号（不修改任何状态）                  | 否       | TCB → 反应器跨界协议（计 0.5 个原语） |
| `enforce`    | L2 元规则强制阻断（halt 语义）                       | 否       | TCB 自进化预留 |

注：dispatch **没有** `noop` 分支——未知元指令类型返回 `TcbError::UnknownMetaInstruction`。「未识别指令变 noop」是 `core_eval.json` 层的兜底：最后一条 `all([])` 规则匹配一切未识别指令，其 transform 为空操作。`noop` 作为**业务指令**（队列中的空操作指令）由该兜底规则经 `push` 产生。

**注**：5 种元指令中 `io_request` 计 0.5 个物理原语（不修改任何状态，只产生对外信号），合计 **4.5 物理原语**。v0.6.0 随规则清理退役 `collect` / `merge`（架构事故中由应用层误下沉至机制层的 LLM ReAct 编排能力），机制层回归最小指令集；`enforce` 为 L2 元规则强制阻断（自进化预留）。

**设计要点**：

- **`io_request` 是"半个"元指令**：它**不修改任何状态**，只产生一个 `MetaInstructionResult::IoRequired { io_type, params }` signal，让上层反应器(evorule-reactor)去执行 I/O，然后把结果注入 `payload.__io_results__.{io_type}`，重新执行 `core_eval.json` 走"消费结果"分支（见 [§2.6 I/O 双路径机制](#26-io-双路径机制)）。
- **不能任意新增真元指令**：TCB_SPEC 约束"指令集有限性 = 确定性来源"。`io_request` 作为"半个"被允许，因为它**不影响 TCB 内部状态**(payload / queue 不变)，只产生对外信号——TCB 的纯函数语义被完整保留。

### 2.2 域类型（Domain Types）

`branch` 元指令使用域类型判断条件。基本域类型 7 种：

| 类型          | 含义                           |
| ------------- | ------------------------------ |
| `eq`          | 路径值 == 目标值               |
| `lt`          | 路径值 < 目标值（仅整数）      |
| `exists`      | 路径存在性检查                 |
| `instruction` | 当前指令类型匹配               |
| `all`         | 所有子域为真（**空列表为真**） |
| `not`         | 子域取反                       |
| `has_fields`  | 路径对象包含指定字段集（v0.3.1 新增） |

派生域类型（由 `core_eval.json` 组合基本域实现）：

| 派生 | 定义                         |
| ---- | ---------------------------- |
| `gt` | `all([not(lt), not(eq)])`    |
| `ne` | `not(eq)`                    |
| `ge` | `not(lt)`                    |
| `le` | `not(gt)`                    |
| `or` | `not(all([not(a), not(b)]))` |

### 2.3 `core_eval.json` 宪法

`core_eval.json` 是 TCB 的"宪法"（v0.3.1）——所有业务指令的语义通过它映射到元指令组合，**修改业务语义无需改 TCB 代码**。

例如 `increment` 业务指令的映射：

```json
{
  "type": "branch",
  "params": {
    "domain": { "type": "instruction", "instruction_type": "increment" },
    "on_true": [
      {
        "type": "set",
        "params": {
          "attr": "__exec__.instruction.params.attr",
          "operation": "add",
          "value": "__exec__.instruction.params.delta"
        }
      }
    ]
  }
}
```

**兜底规则**：最后一条规则用 `all([])`（空列表为真）匹配所有未识别指令，作为 noop 处理。

### 2.4 路径引用

任何以 `__` 开头的字符串都会被自动解析为状态路径。例如：

- `__exec__.payload.x` → 当前 payload 的 `x` 字段
- `__exec__.instruction.params.attr` → 当前指令的 `attr` 参数
- `__exec__.payload.__io_results__.call_external` → 按 io_type 隔离的 I/O 结果

`exec_set` 的 `attr` 与 `value`、`exec_push` 的 `instructions` 数组元素、`exec_io_request` 的参数均支持路径引用。

> **注意（v0.3.1）**：`set` 的 `attr` 引用 payload 内以 `__` 开头的字段时必须写显式前缀（如 `__exec__.payload.__io_results__.call_external`），否则会被误判为状态根路径。

**`set` 的 `attr` 支持数组索引写入**（如 `items[0].done`），路径语法与 domain 读取路径一致。索引写入语义显式优先：目标数组**必须已存在**（缺失报错，不隐式创建——数组长度无法从索引推断）；索引越界报错（禁止稀疏数组与隐式追加，追加须由 `push` 显式完成）；中间对象段缺失时仍自动创建空对象（auto-vivification，与既有行为一致）。

### 2.5 I/O 信号传播

I/O 请求**不在顶层检查指令类型**，而是通过 `core_eval.json` 中的 `io_request` 元指令触发信号，沿调用链传播：

```text
core_eval.json (transform 列表)
↓
execute_transition (迭代 transform)
↓
execute_meta_instruction (处理 5 种元指令)
↓
exec_io_request → 返回 MetaInstructionResult::IoRequired
↓
exec_branch 传播 IoRequired（立即返回，不继续执行后续子指令）
↓
execute_transition 检测 IoRequired → 返回 TransitionResult::IoRequired
```

这确保 `core_eval.json` **完全控制 I/O 映射**——新增 I/O 类型只需修改 JSON，无需改 TCB 代码。

### 2.6 I/O 双路径机制

I/O 指令映射采用**双路径模式**——通过 `exists(__exec__.payload.__io_results__.{io_type})` 域条件区分首次执行和恢复执行：

```text
首次执行（**__io_results__.{io_type}** 不存在）：
instruction(call_external) → branch(exists=false) → io_request → IoRequired

恢复执行（**__io_results__.{io_type}** 已注入）：
instruction(call_external) → branch(exists=true) → set(llm_response, 结果) → State
```

**业务字段命名约定**（v0.3.1 仅保留以下两种 I/O 类型）：

| I/O 类型        | 业务字段名       |
| --------------- | ---------------- |
| `call_external` | `llm_response`   |
| `call_service`  | `service_result` |

> **重要**：I/O 结果**按类型隔离**存放于 `__io_results__.{io_type}`；消费后以 JSON `null` 清除（`exists` 将 `null` 视为不存在），防止残留旧结果被后续不同 I/O 指令错误消费。v0.3.1 已撤销 `query_db` / `http_get` / `save_memory` 内置指令——这些能力由应用层通过 `call_service` + `service_name` 路由实现。

### 2.7 io_request 参数可选性（显式声明）

`exec_io_request` 的参数键名以 `?` 后缀**显式声明可选性**：

- `"messages"`（无 `?`）：**必选参数**。路径引用解析失败立即报错 `PathResolutionFailed`——拼写错误的路径在根因处暴露，不会被静默吞掉。
- `"tools?"`（带 `?`）：**可选参数**。路径引用解析失败时跳过该参数（业务性缺省）；解析成功时请求参数使用去掉 `?` 的键名（`"tools"`）。

```json
{
  "type": "io_request",
  "params": {
    "io_type": "call_external",
    "messages": "__exec__.instruction.params.messages",
    "tools?": "__exec__.instruction.params.tools"
  }
}
```

上例中 `messages` 必须存在（否则报错），`tools` 在业务指令未提供时静默省略（如纯聊天场景）。这样 `call_external` 的 `temperature?`/`max_tokens?` 等可选参数在业务指令未提供时不会导致 I/O 请求失败，而必选参数的拼写错误会被立即捕获。

### 2.8 I/O 单轮触发/消费（v0.6.0 现行；原「ReAct 循环」节）

LLM/工具**多轮编排属应用层职责**（v0.6.0 随规则清理退役 `collect`/`merge`），机制层保留 `io_request` 单轮触发/消费语义：

```text
call_external  → io_request(LLM) → 恢复：set llm_response（单轮终态）
call_service   → io_request(工具) → 恢复：set service_result（单轮终态）
```

- **多轮编排**：循环终止、工具扇出等编排逻辑由应用层 runner / tool_registry 实现，不在机制层。
- **`{{tools}}` 模板**：`call_external` 消费结果时将 tools 持久化到 `payload.tools`，应用层可经 `{{tools}}` 引用。
- **历史**：v0.3.1 曾以 `collect`/`merge` 在机制层内嵌完整 ReAct 循环（`react_iteration < 10` 约束终止），v0.6.0 退役。

---

## 三、模块结构

```text
evorule-tcb/
├── Cargo.toml      # 零依赖配置 + std feature
├── build.rs        # 编译时门禁（24 禁用模式 + BOM 检测）
├── core_eval.json  # TCB 宪法（业务指令 → 元指令映射，含 I/O 双路径）
├── src/
│   ├── lib.rs      # 模块声明 + lint 配置 + 公开 API 重导出
│   ├── value.rs    # JsonValue：确定性 JSON 数据模型
│   ├── path.rs     # 路径解析（点号 + 数组索引 + 转义）
│   ├── domain.rs   # 域类型评估器（7 基本类型 + has_fields + 递归深度限制）
│   ├── executor.rs # 元指令执行器（set/push/branch/io_request/enforce）
│   ├── transition.rs # execute_transition：状态转换入口（含约束前置门：顶层 enforce 先于一切 transform/IO 求值）
│   ├── discipline.rs # L2 规则集形态纪律：core_eval.json 判定条款编译期嵌入（include_str!）+ SSOT 漂移防线测试
│   └── error.rs    # TcbError（11 个变体，std feature 下实现 std::error::Error）
└── tests/
    └── integration_test.rs  # 外部 crate 视角集成测试
```

### 模块依赖关系

```text
value.rs （基础类型）
↑
error.rs （错误类型）
↑
path.rs （路径解析，依赖 value）
↑
domain.rs （域评估，依赖 path/value）
↑
executor.rs （元指令执行器，依赖 domain/path/value）
↑
transition.rs （状态转换，依赖 executor/path/value）
```

### 模块说明

#### [value.rs](src/value.rs) — JsonValue

- 6 个变体：`Null` / `Bool` / `Integer(i64)` / `String` / `Array` / `Object(BTreeMap)`
- 手动实现 `PartialEq` / `Eq` / `Ord` 以保证跨语言一致性
- 不含 `Float` 类型（避免浮点比较不确定性）
- `Object` 使用 `BTreeMap` 保证确定性迭代顺序
- 提供 `object_from_pairs`、`array`、`string`、`empty_array` 等构造辅助函数

#### [path.rs](src/path.rs) — 路径解析

- 路径语法：`segment *( "." segment )`，segment = `identifier [ "[" index "]" ]`
- 转义支持：`\.` 转义字段名中的点，`\[` 转义字段名中的方括号
- `resolve_path` 返回 `Option<&JsonValue>`（永不 panic）
- `resolve_path_mut` 提供可变引用访问（用于 `exec_set`）

#### [domain.rs](src/domain.rs) — 域评估

- 7 种基本域类型：`eq` / `lt` / `exists` / `instruction` / `all` / `not` / `has_fields`
- `MAX_DOMAIN_DEPTH = 64` 防止无限递归
- 派生域类型（`gt`/`ne`/`ge`/`le`/`or`）由 `core_eval.json` 组合实现

#### [executor.rs](src/executor.rs) — 元指令执行器

- `execute_meta_instruction`：元指令分发入口（5 种）
- `MetaInstructionResult` 枚举：`State(JsonValue)` | `IoRequired { io_type, params }`
- 5 个 `exec_*` 私有函数分别处理 5 种元指令
- `resolve_path_or_literal`：统一的路径引用解析辅助函数
- `resolve_instructions_list`：递归解析 `instructions` 数组中的路径引用元素
- `MAX_BRANCH_DEPTH = 64` 防止 branch 嵌套过深
- `MAX_TOTAL_META_INSTRUCTIONS = 1024` 单次转换元指令执行总数上限（宽度防线，超限返回 `TooManyExecutedInstructions`）

#### [transition.rs](src/transition.rs) — 状态转换入口

- `execute_transition`：唯一公开的状态转换 API
- 构建 `__exec__` 上下文（包含 `instruction`/`payload`/`queue`）
- 迭代 `core_eval` transform 列表，检测 `IoRequired` 信号
- `MAX_TRANSFORM_RULES = 64`：`core_eval` 规则数上限（SPEC T6 终止性保证）
- `TransitionResult` 枚举：`State { new_payload, new_queue }` | `IoRequired { io_type, params }`

#### [error.rs](src/error.rs) — 错误类型

- `TcbError`：11 个结构化变体，全部携带诊断上下文（见 [§五 公开 API](#五公开-api)）
- `std` feature 下实现 `std::error::Error`

---

## 四、快速开始

### 4.1 编译与测试

```bash
# 编译
cargo build

# 运行全部测试（222 单元 + 5 determinism_proptest + 26 proptest_props + 18 集成 + 18 doc）
cargo test

# Clippy 检查（零警告）
cargo clippy --all-targets -- -D warnings

# 启用 std feature（为 TcbError 实现 std::error::Error）
cargo check --features std
```

### 4.2 最小调用示例

```rust
use evorule_tcb::{execute_transition, JsonValue, TransitionResult};
use alloc::vec::Vec;

// 1. 构造 core_eval transform 列表（实际由 core_eval.json 编译生成）
let core_eval: Vec<JsonValue> = vec![
    JsonValue::object_from_pairs(&[
        ("type", JsonValue::string("branch")),
        ("params", JsonValue::object_from_pairs(&[
            ("domain", JsonValue::object_from_pairs(&[
                ("type", JsonValue::string("instruction")),
                ("instruction_type", JsonValue::string("increment")),
            ])),
            ("on_true", JsonValue::array(vec![
                JsonValue::object_from_pairs(&[
                    ("type", JsonValue::string("set")),
                    ("params", JsonValue::object_from_pairs(&[
                        ("attr", JsonValue::string("x")),
                        ("operation", JsonValue::string("add")),
                        ("value", JsonValue::string("__exec__.instruction.params.delta")),
                    ])),
                ]),
            ])),
        ])),
    ]),
];

// 2. 构造当前指令、payload、queue
let instruction = JsonValue::object_from_pairs(&[
    ("type", JsonValue::string("increment")),
    ("params", JsonValue::object_from_pairs(&[
        ("attr", JsonValue::string("x")),
        ("delta", JsonValue::Integer(5)),
    ])),
]);
let payload = JsonValue::object_from_pairs(&[("x", JsonValue::Integer(10))]);
let queue: Vec<JsonValue> = vec![];

// 3. 执行状态转换
let result = execute_transition(&core_eval, &instruction, &payload, &queue).unwrap();

match result {
    TransitionResult::State { new_payload, new_queue } => {
        // new_payload.x == 15
        assert_eq!(new_payload.get("x"), Some(&JsonValue::Integer(15)));
    }
    TransitionResult::IoRequired { io_type, params } => {
        // 由上层反应器执行 I/O
    }
}
```

### 4.3 I/O 请求示例

`core_eval.json` 中 `call_external` 映射采用**双路径**机制——通过 `exists(__io_results__.call_external)` 区分首次执行和恢复执行：

```json
{
  "type": "branch",
  "params": {
    "domain": { "type": "instruction", "instruction_type": "call_external" },
    "on_true": [
      {
        "type": "branch",
        "params": {
          "domain": {
            "type": "exists",
            "path": "__exec__.payload.__io_results__.call_external"
          },
          "on_true": [
            {
              "type": "set",
              "params": {
                "attr": "llm_response",
                "operation": "set",
                "value": "__exec__.payload.__io_results__.call_external"
              }
            }
          ],
          "on_false": [
            {
              "type": "io_request",
              "params": {
                "io_type": "call_external",
                "messages": "__exec__.instruction.params.messages"
              }
            }
          ]
        }
      }
    ]
  }
}
```

```rust
// 首次执行：__io_results__.call_external 不存在 → 走 on_false → IoRequired
let result = execute_transition(&core_eval, &call_external_instr, &payload, &queue).unwrap();

match result {
    TransitionResult::IoRequired { io_type, params } => {
        assert_eq!(io_type, "call_external");
        // params.messages 已从 __exec__.instruction.params.messages 解析为实际值
        // 反应器收到此信号后：
        //   1. 调用真实 LLM 获取结果
        //   2. 注入 payload.__io_results__.call_external = result
        //   3. 将原指令 push_front 到队列前端重新执行
        //   4. 再次 execute_transition：exists 为真 → 走 on_true → set llm_response
        //   5. 消费后反应器以 null 清除 __io_results__.call_external（防止残留影响后续 I/O）
    }
    _ => panic!("expected IoRequired"),
}
```

---

## 五、公开 API

仅公开 3 个核心类型 + 1 个入口函数 + 2 个常量：

| API                                                    | 说明                                  |
| ------------------------------------------------------ | ------------------------------------- |
| `JsonValue`                                            | 确定性 JSON 数据模型                  |
| `TcbError`                                             | 错误类型（11 个变体，永不 panic）     |
| `execute_transition(core_eval, instr, payload, queue)` | 状态转换入口函数                      |
| `TransitionResult`                                     | 转换结果：`State` 或 `IoRequired`     |
| `MAX_TRANSFORM_RULES`                                  | `core_eval` 规则数上限（64，SPEC T6） |
| `MAX_TOTAL_META_INSTRUCTIONS`                          | 单次转换元指令执行总数上限（1024，宽度防线） |

### `TcbError` 变体

```rust
pub enum TcbError {
    MissingField { field: String },              // 缺少必需字段
    UnknownMetaInstruction { meta_type: String },// 未知元指令类型
    UnknownDomainType { domain_type: String },   // 未知域类型（不在 7 种基本域之内）
    UnknownOperation { operation: String },      // 未知 set 操作
    InvalidState { reason: String },             // 状态结构异常
    InvalidType { expected: &'static str, actual: &'static str, context: String }, // 类型不匹配
    PathResolutionFailed { path: String, reason: String }, // 路径解析失败
    NestingTooDeep { limit: usize },             // branch 嵌套超 64 层
    IntegerOverflow { operation: String, left: i64, right: i64 }, // 整数运算溢出
    TooManyTransformRules { limit: usize, actual: usize }, // core_eval 规则数超 64 条（SPEC T6）
    TooManyExecutedInstructions { limit: usize }, // 单次转换元指令执行总数超 1024（宽度防线）
}
```

启用 `std` feature 后，`TcbError` 实现 `std::error::Error`。

---

## 六、`core_eval.json` 使用说明

### 6.1 结构

```json
{
  "rule_id": "core.eval",
  "version": "0.3.1",
  "description": "TCB 宪法 - 完整 ReAct 循环支持（v0.3.1 修复同轮重复 push）",
  "metadata": { ... },
  "transform": [ ... ]
}
```

`transform` 是一个有序的元指令列表（通常为 `branch`），TCB 按顺序执行，遇到 `IoRequired` 立即返回。

### 6.2 当前已映射的业务指令

| 类别     | 业务指令                                                                   |
| -------- | -------------------------------------------------------------------------- |
| 原子计算 | `increment` / `decrement` / `set`                                          |
| 控制流   | `sequence` / `conditional` / `while_loop`                                  |
| I/O 单轮 | `call_external` / `call_service`（io_request 触发/消费；v0.6.0 起 collect/merge 退役，多轮编排归应用层） |
| 兜底     | 任何未匹配指令（`all([])` 规则）→ noop                                     |

> **v0.3.1 撤销**：`query_db` / `http_get` / `save_memory` 不再是内置指令类型（宪法未定义对应 transform 规则，提交会落入 `all([])` 兜底变为 noop）。应用层应用 `call_service` + `service_name` 实现这些能力。

### 6.3 扩展新业务指令

无需改 TCB 代码，只需在 `core_eval.json` 的 `transform` 数组中追加规则。I/O 类指令**必须使用双路径模式**（通过 `exists(__io_results__.{io_type})` 区分首次请求和结果消费），且每条 transform 规则中**最多包含一个 `io_request` 元指令**（`__io_results__` 按 io_type 隔离，多个会冲突）。

```json
{
  "type": "branch",
  "params": {
    "domain": { "type": "instruction", "instruction_type": "send_email" },
    "on_true": [
      {
        "type": "branch",
        "params": {
          "domain": {
            "type": "exists",
            "path": "__exec__.payload.__io_results__.send_email"
          },
          "on_true": [
            {
              "type": "set",
              "params": {
                "attr": "email_result",
                "operation": "set",
                "value": "__exec__.payload.__io_results__.send_email"
              }
            }
          ],
          "on_false": [
            {
              "type": "io_request",
              "params": {
                "io_type": "send_email",
                "to": "__exec__.instruction.params.to",
                "subject": "__exec__.instruction.params.subject"
              }
            }
          ]
        }
      }
    ]
  }
}
```

> **注意**：若不使用双路径模式，I/O 结果会被注入到 `payload.__io_results__.send_email` 但永远不会被消费为业务字段，且残留的 `__io_results__` 会导致后续不同的 I/O 指令错误消费旧结果。

---

## 七、安全契约

### 7.1 永不 panic

三重保障：

1. **类型系统**：路径解析返回 `Option`，整数运算使用 `checked_*`
2. **递归深度限制**：`domain.rs` `MAX_DOMAIN_DEPTH=64`；`executor.rs` `MAX_BRANCH_DEPTH=64`；`executor.rs` `MAX_TOTAL_META_INSTRUCTIONS=1024`（执行总数宽度防线）
3. **Clippy lint**：`deny(unwrap_used/expect_used/indexing_slicing/panic)`

测试模块通过 `#![allow(clippy::unwrap_used)]` 等局部放宽以保持测试可读性。

### 7.2 整数安全

`set` 元指令的 `add` / `sub` 操作使用 `i64::checked_add` / `checked_sub`，溢出时返回 `TcbError::IntegerOverflow` 而非 panic。

### 7.3 无 `unsafe`

`#![forbid(unsafe_code)]` 在 `lib.rs` 顶层声明。

### 7.4 确定性

- `Object` 使用 `BTreeMap`（按键排序迭代）
- 无 `Float` 类型（避免浮点比较的不确定性）
- 所有迭代顺序明确
- 同一输入执行 10 次结果完全一致（集成测试 `test_determinism_repeated_calls` 验证）

---

## 八、源码审计

`evorule-tcb` 的源码审计通过以下方式进行:

1. **build.rs 编译时门禁** — 24 个禁用模式 (T 编号 + F11 panic-prone 组) 强制,
   详见 [§九 build.rs 门禁](#九-buildrs-编译时门禁) 与 [`TCB_SPEC.md`](TCB_SPEC.md)。
2. **属性测试** — 确定性专项测试（同一输入重复执行一致性、状态隔离、I/O 结果清除）已纳入
   [`tests/integration_test.rs`](tests/integration_test.rs)。
3. **第三方安全审计** — 留待 1.0.0 公开版（0.x 阶段不做）。

---

## 九、build.rs 编译时门禁

### 9.1 24 个禁用模式

[build.rs](build.rs) 扫描 `src/**/*.rs`（测试模块自动剥离后扫描），禁止以下破坏确定性的构造：

| 规则          | 禁止模式                                              | 破坏点                       |
| ------------- | ----------------------------------------------------- | ---------------------------- |
| T8 (哈希容器) | `HashMap`, `HashSet`                                  | 迭代顺序非确定               |
| T9/F11 (panic)| `.unwrap(`, `.expect(`, `panic!(`                     | 可 panic                    |
| T11 (assert)  | `debug_assert!`                                       | 可 panic（发布构建消失）     |
| T10 (unsafe)  | `unsafe`                                              | 内存非确定行为               |
| T12 (浮点)    | `f32`, `f64`, `Float`                                 | 跨平台非确定                 |
| T5 (系统时间) | `SystemTime`, `Instant`                               | 依赖当前时间                 |
| T6 (随机数)   | `rand::`, `random()`                                  | 非确定                       |
| T4 (I/O)      | `std::fs::`, `std::net::`, `std::io::`, `File::open`, `std::process::` | 依赖外部环境 |
| T14 (线程异步)| `std::thread`, `tokio::`, `async`, `await`, `spawn(`  | 并发非确定                   |

- **测试模块剥离**：T8/T9/F11-panic 是 test-tolerant（`#[cfg(test)] mod <ident>` 任意命名测试模块内允许，TCB-2026-35）；T10/T11 等其余标签在所有位置强制。
- **注释跳过**：`//` 行注释内的模式被跳过；行内注释（代码部分含模式）仍被拦截。

### 9.2 BOM 检测

源码文件不得以 UTF-8 BOM (U+FEFF) 开头。编辑器引入 BOM 会遮蔽首行 `//` 前缀，使注释跳过失效。门禁检测到 BOM 时：剥离 BOM 保证后续扫描正确，同时将 BOM 记为 `BOM-detected` 违规强制移除。

### 9.3 紧急跳过（不推荐）

```bash
EVORULE_SKIP_GATE=1 cargo build       # 阀值仅 1/true 生效 (0/空/其他值 = 门禁照常执行, fail-closed)
EVORULE_SKIP_REASON="原因"            # 跳过理由登记 (未登记将出 warning)
```

跳过必须临时且有书面理由，永不永久禁用。任何违规立即构建失败（exit 1），违规行号 + 标签全打印。

---

## 十、遗留事项与路线图

### 10.1 非阻塞遗留事项

| 编号 | 事项                        | 状态      | 说明                                                                                                                |
| ---- | --------------------------- | --------- | ------------------------------------------------------------------------------------------------------------------- |
| N-01 | `MAX_TRANSFORM_RULES` 限制  | ✅ 已完成 | `execute_transition` 入口检查 `core_eval.len() ≤ 64`,超限返回 `TcbError::TooManyTransformRules`(SPEC T6 终止性保证) |
| N-02 | Kani 形式化验证重建         | 🟡 历史 PASS | v0.3.1 以「结构化符号输入 + `KIdSet`/`KIdMap`」重建为 34 个 `#[kani::proof]`(P1-P21 旧编号),后增至 37 个并改行 A/B 两档;v0.6.0 随规则清理退役 P15/P16/P17(37→34)——A 档 14 个 v0.6.0 重跑(2026-09-14,`25c0cc0`)全 PASS 并入 CI 闸门,B 档 20 个实测超时判定当前不可运行(proptest 间接覆盖);详情见 §6.3 与 `docs/KANI.md`,状态唯一权威见 [`verification/STATUS.md`](../verification/STATUS.md) |

### 10.2 后续 Tier 路线

`TransitionResult::IoRequired` 已为 evorule-reactor 准备就绪:

1. **evorule-reactor** — 反应式执行器
   - 消费 `TransitionResult::IoRequired` → 产生 `IoRequest` 事实并
     缓存原指令 → 收到 `IoResponse` 后注入 `__io_results__.{io_type}` →
     将原指令 `push_front` 到队列前端重新执行 →
     `exists` 为真 → `set` 消费结果 → 以 null 清除 `__io_results__.{io_type}`
2. **evorule-governance** — 治理层
   - I/O 订阅者机制 / 审计日志 / HTTP API

详细设计见根 [`README.md`](../README.md) 三层架构章节。

---

## 十一、设计文档参考

实际权威源:

- `evorule-tcb/TCB_SPEC.md` — 本模块 redline 规则 (T1-T14)
- `evorule-tcb/DETERMINISM_REPORT.md` — 确定性保障报告（零运行时依赖 / no_std / 24 模式门禁）
- `evorule-reactor/REACTOR_SPEC.md` — 反应器机制-策略分离
- `evorule-governance/GOVERNANCE_SPEC.md` — 治理层机制-策略分离
- 项目级文档总索引: [`DOCS_INDEX.md`](../DOCS_INDEX.md)（所有 L1 公开文档的唯一入口）
- 项目级架构总览: [`README.md`](../README.md)（三层架构 + 快速开始）

---

## 十二、许可证

**代码**:`AGPL-3.0-or-later`(见 [`LICENSE`](LICENSE) 文件,完整协议文本)。

**`core_eval.json` 宪法**:`CC0-1.0 Universal`(公共领域),
任何人都可自由实现兼容的 EvoRule 引擎,无需保留版权声明。
`core_eval.json` 文件内的 `metadata.public_domain_notice` 字段
有详细说明。

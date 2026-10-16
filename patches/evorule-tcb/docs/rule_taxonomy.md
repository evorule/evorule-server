# EvoRule 规则分类体系

> **版本**: v1.0
> **最后更新**: 2026-08-18
> **目标**: 明确区分不同层次的规则，消除术语混淆

---

## 1. 问题背景

EvoRule 生态中存在多种规则，但都模糊地称为"规则"：

- `core_eval.json` 中的 transform 规则
- `rules/*.json` 中的业务规则
- Rust 代码中的硬编码规则
- 运行时动态生成的规则

这种模糊带来的问题：
1. 设计时容易混淆不同类型规则的职责边界
2. 实施时容易错误地将项目方规则当作系统规则处理
3. 维护时难以理解规则的来源、用途和修改权限
4. 检测逻辑中无法正确区分"规则结构定义了指令过滤"和"规则实际处理了当前指令"

---

## 2. 规则分类体系

### 2.1 四层规则架构

```
┌─────────────────────────────────────────────────────────────┐
│                    应用层 (evorule-server)                    │
│  ┌─────────────────────────────────────────────────────┐    │
│  │  项目方规则 (User Rules)                              │    │
│  │  - 文件: rules/*.json                               │    │
│  │  - 可热重载                                         │    │
│  │  - 定义: 具体业务如何响应指令                        │    │
│  └─────────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────────┘
                              │ 指令 + payload
                              ▼
┌─────────────────────────────────────────────────────────────┐
│              内核层 (evorule-tcb + evorule-reactor)           │
│  ┌─────────────────────────────────────────────────────┐    │
│  │  业务规则 (Business Rules)                          │    │
│  │  - 运行时动态生成                                   │    │
│  │  - 来源: 项目方规则 + 指令参数 + payload               │    │
│  │  - 作用: 动态决策 + 数据驱动                        │    │
│  └─────────────────────────────────────────────────────┘    │
│                              │                              │
│  ┌─────────────────────────────────────────────────────┐    │
│  │  宪法规则 (Constitution Rules)                      │    │
│  │  - 文件: core_eval.json                             │    │
│  │  - 不可热重载（TCB 启动时加载）                      │    │
│  │  - 定义: 内核支持的基础指令处理逻辑                  │    │
│  │  - 示例: increment, set, while_loop 等              │    │
│  └─────────────────────────────────────────────────────┘    │
│                              │                              │
│  ┌─────────────────────────────────────────────────────┐    │
│  │  系统规则 (System Rules)                            │    │
│  │  - 硬编码在 Rust 代码中                              │    │
│  │  - 不可配置                                         │    │
│  │  - 定义: TCB 的运行时约束和检测逻辑                 │    │
│  │  - 示例: noop 豁免、max_transform_rules、Ignored 检测│    │
│  └─────────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────────┘
```

### 2.2 各层规则详细定义

#### 第一层：系统规则 (System Rules)

**定义**：
- 硬编码在 TCB Rust 代码中的运行时约束和检测逻辑
- 不可配置，随 TCB 版本变化
- 不属于"数据"，而是"行为约束"

**示例**：
```rust
// 系统规则 1: noop 指令豁免（不报告 Ignored）
// transition.rs 中的检测逻辑

// 系统规则 2: max_transform_rules 约束（每条 transform 最多 N 条规则）
// transition.rs 中的校验逻辑

// 系统规则 3: Ignored 检测逻辑
// transition.rs 中的 has_matching_business_rule 检测
```

**修改权限**：仅 TCB 核心开发者可修改

**当前代码位置**：
- `transition.rs` - `execute_transition()` 函数中的约束逻辑
- `transition.rs` - `has_matching_business_rule` 检测逻辑

---

#### 第二层：宪法规则 (Constitution Rules)

**定义**：
- 存储在 `core_eval.json` 中的 JSON 规则
- TCB 启动时加载，运行时不可热重载
- 定义内核支持的所有基础指令处理逻辑
- 具有系统约束力，项目方必须遵守

**当前定义的指令**：
| 指令类型 | 处理逻辑 | 说明 |
|---------|---------|------|
| `increment` | set(attr, "add", delta) | 原子累加 |
| `decrement` | set(attr, "sub", delta) | 原子递减 |
| `set` | set(attr, "set", value) | 覆盖赋值 |
| `sequence` | push(instructions) | 顺序执行 |
| `conditional` | branch(domain, then, else) | 条件分支 |
| `while_loop` | branch(condition, [body, self]) | 循环执行 |
| `call_external` | branch(exists, ...) / io_request | 外部调用 (ReAct) |
| `call_service` | branch(exists, ...) / io_request | 服务调用 (ReAct；v0.6.0 起多轮编排归应用层) |
| `all([])` | noop (空操作) | 兜底规则 |

**修改权限**：TCB 核心开发者可修改（升级版本时）

**当前文件位置**：
- `evorule-tcb/core_eval.json` - `transform` 数组

---

#### 第三层：业务规则 (Business Rules)

**定义**：
- 运行时动态生成的规则
- 由项目方规则 + 指令参数 + payload 组合而成
- 动态决策，数据驱动
- TCB 执行这些规则，不关心它们的来源

**示例**：
```json
// 运行时动态生成的业务规则（由应用层生成后提交给 TCB）
{
  "type": "branch",
  "params": {
    "domain": { "type": "instruction", "instruction_type": "set" },
    "on_true": [
      {
        "type": "set",
        "params": {
          "attr": "counter",
          "operation": "add",
          "value": 5
        }
      }
    ]
  }
}
```

**修改权限**：应用层代码生成，项目方规则控制其结构

**当前涉及代码**：
- `evorule-reactor/src/reactor.rs` - 接收业务规则并提交给 TCB
- 应用层 - 生成业务规则的逻辑

---

#### 第四层：项目方规则 (User Rules)

**定义**：
- 存储在 `rules/*.json` 中的 JSON 规则
- 可热重载，运行时可修改
- 定义具体业务如何响应指令
- 面向业务分析师，无需编程知识

**示例**：
```json
// rules/counter.json
{
  "rules": [
    {
      "when": { "type": "command", "instruction_type": "set" },
      "do": [
        {
          "type": "set",
          "operation": "set",
          "attr": "${params.attr}",
          "value": "${params.value}"
        }
      ]
    }
  ]
}
```

**修改权限**：业务分析师可修改

**当前文件位置**：
- `evorule-server/resources/rules/*.json`

---

## 3. 术语规范

### 3.1 术语对照表

| 术语 | 英文 | 中文 | 使用场景 |
|------|------|------|---------|
| 系统规则 | System Rules | 系统规则 | 讨论 TCB 硬编码行为时使用 |
| 宪法规则 | Constitution Rules | 宪法规则 | 讨论 core_eval.json 时使用 |
| 业务规则 | Business Rules | 业务规则 | 讨论运行时动态规则时使用 |
| 项目方规则 | User Rules | 项目方规则 | 讨论 rules/*.json 时使用 |
| 指令 | Instruction | 指令 | 讨论 TCB 接收的命令时使用 |
| 转换 | Transition | 转换 | 讨论 execute_transition() 时使用 |

### 3.2 命名建议

#### 代码变量命名

```rust
// ❌ 不清晰的命名
let rule = ...;           // 哪个层的规则？
let business_rule = ...;  // 不准确，可能是宪法规则

// ✅ 清晰的命名
let const_rule = ...;     // 宪法规则
let sys_rule = ...;       // 系统规则（但通常不存在于规则变量中）
let runtime_rule = ...;   // 运行时业务规则
let user_rule = ...;      // 项目方规则
```

#### 函数命名

```rust
// ❌ 不清晰的命名
fn has_matching_business_rule(...) -> bool  // 实际检查的是宪法规则

// ✅ 清晰的命名
fn has_matching_constitution_rule(...) -> bool  // 明确检查宪法规则
fn has_side_effect(...) -> bool  // 检查是否有实际副作用
```

### 3.3 文档命名

在文档和注释中明确标识规则层次：

```markdown
## 规则分类

### 宪法规则 (Constitution Rules)
- 文件: core_eval.json
- 说明: 定义内核支持的基础指令处理

### 业务规则 (Business Rules)
- 来源: 运行时动态生成
- 说明: 由项目方规则 + 指令参数 + payload 组合而成
```

---

## 4. 问题诊断与解决方案

### 4.1 当前问题

#### 问题 1：`has_matching_business_rule` 命名不准确

```rust
// transition.rs#L168-L192
let has_matching_business_rule = core_eval.iter().any(|rule| {
    // 这里检查的是 core_eval 中的规则 = 宪法规则
    // 不是 "业务规则"
    ...
});
```

**问题**：变量名为 `business_rule`，但实际检查的是宪法规则。这导致：
- B4 场景：动态 domain 规则（lt）被视为"业务规则"，即使它不匹配当前指令
- B5 场景：直接规则（set）被视为"业务规则"，不区分指令类型

#### 问题 2：检测逻辑混淆了两个维度

当前检测逻辑将两个独立问题混在一起：
1. **规则是否匹配当前指令类型** → 检查宪法规则的 domain
2. **执行是否有实际副作用** → 检查 payload/queue 是否变化

这两个维度应该独立判断：
- 规则匹配 + 无副作用 → 合法空操作（如 while_loop condition=false）
- 规则不匹配 + 有副作用 → 不可能发生
- 规则不匹配 + 无副作用 → 报告 Ignored

### 4.2 解决方案

#### 方案 1：重命名 + 拆分检测逻辑（已实施 ✅）

```rust
// 第一步：检查是否存在匹配当前指令的宪法规则
let has_matching_constitution_rule = core_eval.iter().any(|rule| {
    match domain_type {
        "instruction" => {
            // 精确匹配 instruction_type
            return expected_type == instruction_type;
        }
        "all" => false, // all 兜底不是业务规则
        _ => {
            // 动态 domain（eq, lt, gt, ge, not, exists, has_fields 等）
            // 视为通用规则，可处理任何指令
            return true;
        }
    }
    // 直接规则（如 set, increment），检查 rule_type 是否匹配 instruction_type
    if matches!(rule_type, "set" | "increment" | ...) {
        return rule_type == instruction_type;
    }
});

// 第二步：执行转换后检查副作用
let has_side_effect = new_payload != payload || new_queue != queue;

// 第三步：组合判断
if !has_matching_constitution_rule && !has_side_effect {
    // 规则不匹配且无副作用 → 报告 Ignored
    return Ok(TransitionResult::Ignored { ... });
}

// 其他情况都返回 State（包括规则匹配但无副作用的合法空操作）
Ok(TransitionResult::State { new_payload, new_queue })
```

#### 方案 2：已解决的问题分析

| 场景 | 规则类型 | 旧判定 | 新判定 | 说明 |
|------|---------|--------|--------|------|
| B4: lt 规则 + while_loop 指令 | 动态 domain | ✅ 匹配 | ✅ 匹配 | 动态 domain 视为通用规则 |
| B5: set 规则 + while_loop 指令 | 直接规则 | ✅ 匹配 | ❌ 不匹配 | 直接规则检查 rule_type |
| B5b: set 规则 + set 指令 | 直接规则 | ✅ 匹配 | ✅ 匹配 | 直接规则精确匹配 |
| E2: 规则匹配 + on_true 为空 | 宪法规则 | ❌ 可能误判 | ✅ 返回 State | 分离副作用检测 |

**已解决的问题**：
1. ✅ 变量名从 `has_matching_business_rule` 改为 `has_matching_constitution_rule`
2. ✅ 动态 domain 规则视为通用规则（可处理任何指令）
3. ✅ 直接规则检查 `rule_type == instruction_type`
4. ✅ 分离"规则匹配"和"副作用检测"两个判断

---

## 5. 后续实施计划

### 阶段 1：文档规范（已完成 ✅）
- [x] 创建规则分类体系文档 `rule_taxonomy.md`
- [x] 明确系统规则、宪法规则、业务规则、项目方规则四层分类
- [x] 定义术语规范和命名建议

### 阶段 2：代码重构（已完成 ✅）
- [x] 重命名 `has_matching_business_rule` → `has_matching_constitution_rule`
- [x] 改进动态 domain 规则：视为通用规则，可处理任何指令
- [x] 改进直接规则：检查 `rule_type == instruction_type`
- [x] 分离"规则匹配"和"副作用检测"逻辑
- [x] 添加 `has_side_effect` 变量

### 阶段 3：测试更新（已完成 ✅）
- [x] 更新 B4 场景测试：验证动态 domain 规则作为通用规则
- [x] 更新 B5 场景测试：验证直接规则的精确匹配
- [x] 添加 `test_set_direct_rule_matches_set_instruction` 测试
- [x] 所有测试通过（220 单元测试 + 5 proptest + 21 集成测试）

### 阶段 4：文档完善（进行中）
- [ ] 在 core_eval.json 中明确标识"宪法规则"
- [x] 在代码注释中明确规则层次
- [x] 创建术语对照表（本文件第 3 节）

### 阶段 5：代码审计（可选）
- [ ] 审计 evorule-reactor 中 `StepOutcome` 枚举的命名
- [ ] 审计 evorule-server 中业务规则的命名规范
- [ ] 添加代码文档注释，明确规则层次

---

## 6. 相关文件

### 核心定义
- [README.md](../../README.md) - 项目关键概念定义
- [core_eval.json](../core_eval.json) - 宪法规则定义

### 代码实现
- [transition.rs](../src/transition.rs) - TCB 转换逻辑
- [pure.rs](../../evorule-reactor/src/pure.rs) - Reactor 纯函数
- [reactor.rs](../../evorule-reactor/src/reactor.rs) - Reactor 主循环

### 设计规范
- [TCB_SPEC.md](../TCB_SPEC.md) - TCB 系统规范
- [CLI_SPEC.md](../../evorule-cli/CLI_SPEC.md) - CLI 设计规范

---

## 7. 总结

当前 evorule 的规则体系存在分类模糊的问题，导致：
1. 设计时混淆规则的职责边界
2. 实施时出现检测逻辑的误判（B4/B5 场景）
3. 维护时难以理解规则的来源和修改权限

通过本分类体系，明确了：
1. **系统规则**：硬编码行为，不可配置
2. **宪法规则**：core_eval.json 定义的基础指令处理
3. **业务规则**：运行时动态生成的规则
4. **项目方规则**：rules/*.json 定义的业务响应

建议按"文档规范 → 代码重构 → 测试更新"的顺序实施，将规则分类清晰化。

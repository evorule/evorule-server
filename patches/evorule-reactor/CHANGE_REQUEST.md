# 变更审查表 (Change Request)

## 1. 基本信息

| 字段 | 值 |
|------|------|
| **变更 ID** | CR-20260902-001 |
| **变更标题** | 未知 IoResponse 显式入链：Error fact 记录替代静默忽略（回归验证 A1，处置方案：Error fact） |
| **提交人** | EvoRule Team |
| **提交日期** | 2026-09-02 |
| **审查状态** | 已批准 |

## 2. 变更层级判定（必填）

### 2.1 变更层级声明

**本次变更属于**: ✅ **机制层 (Mechanism)**

### 2.2 判定理由

```
本变更修复 IoResponse 异常路径的可审计性缺口，不触及任何业务语义：
- 状态层拒绝消费 unknown/stale request_id 的 IoResponse 时，原仅 tracing
  告警——IoResponse 事实本身已入链（command 通道先 emit 再 handle），但
  链上无异常标记，审计重放无法自解释地对账"响应事实 vs 拒绝消费"
- 现发射 Fact::Error（duplicate / late-after-timeout / forged 三种成因
  提示），与超时路径既有 Error-fact 机制同构；Error 为可恢复事实，
  不影响会话可用性
- handle_fact 签名扩展（facts_log/event_tx/id_gen），均为既有内部机制
  句柄的传递，无新外部依赖
```

### 2.3 机制层判定标准检查

**✅ 机制层变更的特征**:
- [x] 提供通用基础设施能力（异常路径审计自解释）
- [x] 不包含任何特定业务语义
- [x] 可被任何业务场景无差别复用

## 3. 变更分类

- **变更类型**: B - 机制扩展
- **影响模块**: evorule-reactor/src/reactor.rs、tests/integration_test.rs

## 4. 变更详情

### 3.1 变更理由

回归验证 A1：确定性执行引擎的审计链必须自解释。unknown IoResponse 是
安全敏感事件（重复/超时迟到/伪造），静默忽略让审计重放看到"有响应事实
但状态未消费"却无异常标记，破坏"链上事实可逐条对账"的核心承诺。

### 3.2 变更范围

- reactor.rs：`handle_fact` 签名扩展（+facts_log/event_tx/id_gen）；
  IoResponse unknown 路径发射 `Fact::Error` 后返回
- lib.rs：hash 根导出补 `chain_step`（governance auditor 消除内联
  blake3 重复实现所需，B1 配套）
- tests/integration_test.rs：`test_unknown_io_response_ignored` 改名
  `test_unknown_io_response_records_error_fact`，断言 Error fact 入链

### 3.3 破坏性分析

- `handle_fact` 为私有关联函数，签名扩展无外部影响
- 行为变化：unknown IoResponse 现在产生 Error fact（链上新增事实）——
  消费方若对 Error fact 计数敏感会观察到新增条目；这正是修复目的
  （配套 cli 侧同批变更：Error fact → 退出码 3）

### 3.4 影响评估

- 全 workspace 测试须绿；既有超时 Error-fact 语义不变
- Error 为可恢复事实：会话继续可用，不触发恢复/终止路径

### 3.5 测试计划

- [x] `test_unknown_io_response_records_error_fact`：unknown IoResponse → Error fact 入链
- [x] 全 workspace `cargo test` 回归（含 recover 往返、WAL 轮换）
- [x] 变更治理门禁 + 策略层检测 PASSED

### 3.6 回滚方案

git revert 本提交即恢复静默忽略形态；与 governance/cli 侧 CR-20260902-001
同批实施，回滚需同批处理。

## 5. 审查清单

### 层级审查
- [x] 变更层级声明为"机制层"
- [x] 判定理由充分
- [x] 代码中无策略层反模式

### 技术审查
- [x] 代码符合 REACTOR_SPEC.md 规范
- [x] 单元测试通过

### 架构审查
- [x] 变更符合规则分类体系
- [x] 符合"机制不染指控制流"原则

---

## 附 · 历史变更归档

### CR-20260901-001（已批准）：单会话长跑 O(n²) 缺陷修复：Fact::Stable 瘦身 + WAL 旧格式容错 + FactsLog 增量迭代接口（回归验证）

> 归档说明：原 CR 整表置于顶层至 2026-09-02（CR-20260902-001 置顶），完整内容见 git 历史。

| 字段 | 值 |
|------|------|
| **变更 ID** | CR-20260901-001 |
| **变更标题** | 单会话长跑 O(n²) 缺陷修复：Fact::Stable 瘦身 + WAL 旧格式容错 + FactsLog 增量迭代接口（回归验证） |
| **提交人** | EvoRule Team |
| **提交日期** | 2026-09-01 |
| **审查状态** | 已批准 |

Fact::Stable 由 final_snapshot（全量 payload 快照）瘦身为 version: u64
（恢复路径从不读取快照，纯冗余；状态本体由最近一条 StateTransition 确定）；
wal.rs 反序列化对旧格式（≤0.3.x 含 final_snapshot）容错；facts_log.rs 新增
for_each_fact_from 锁内零 clone 增量遍历供 tier2 审计增量化。⚠️ 审计链哈希
输入与 WAL 磁盘格式变更（0.x MINOR 承载，新代码可读旧格式，升级单向）。
回滚：git revert 提交 7da4045。

### CR-20260831-001（已批准）：存储层 trait 抽象：FactWalStore 后端契约 + MemoryWalStore 内存后端（回归验证）

> 归档说明：原 CR 整表收录于 2026-09-01（CR-20260901-001 置顶），完整内容见 git 历史。

| 字段 | 值 |
|------|------|
| **变更 ID** | CR-20260831-001 |
| **变更标题** | 存储层 trait 抽象：FactWalStore 后端契约 + MemoryWalStore 内存后端（回归验证） |
| **提交人** | EvoRule Team |
| **提交日期** | 2026-08-31 |
| **审查状态** | 已批准 |

机制层变更：FactsLog 的 WAL 持久化层提供可替换后端契约（FactWalStore trait +
MemoryWalStore 内存后端），哈希链计算、版本推进、恢复重放逻辑全部不动，
消费方公开 API 零改动。回滚：git revert。

---

### CR-20260830-001（已批准）：build.rs 门禁状态机生命周期撇号判别修复（strip_test_mod 误报消除）

> 归档说明：原 CR 整表收录于 2026-08-31，完整内容见 git 历史。

| 字段 | 值 |
|------|------|
| **变更 ID** | CR-20260830-001 |
| **变更标题** | build.rs 门禁状态机生命周期撇号判别修复 |
| **提交人** | EvoRule Team |
| **提交日期** | 2026-08-30 |
| **审查状态** | 已批准 |

机制层变更：strip_test_mod/find_inline_lbrace/match_brace 状态机在撇号处新增
char_lit_starts() 判别（字符字面量 vs 生命周期），修复 'static 被误判为字符态
致 tests 模块不剥离、门禁全量误报；五仓同一份实现同步修复。回滚：git revert。

---

### CR-20260820-002（已批准）：添加变更治理门禁机制和策略层检测

> 归档说明：原 CR 整表收录于 2026-08-30，完整内容见 git 历史。

| 字段 | 值 |
|------|------|
| **变更 ID** | CR-20260820-002 |
| **变更标题** | 添加变更治理门禁机制和策略层检测 |
| **提交人** | EvoRule Team |
| **提交日期** | 2026-08-20 |
| **审查状态** | 已批准 |

机制层变更：为 build.rs 添加 CHANGE_REQUEST.md 验证逻辑和策略层反模式检测，提供通用的变更治理基础设施；可被任何机制层代码复用，不含业务语义。回滚：删除 build.rs 中的验证代码即可。

## CR-20260920-001 0.6.1 版本收口发布（跨仓 CR）

| 字段 | 值 |
|------|------|
| **变更 ID** | CR-20260920-001 |
| **变更标题** | 0.6.1 版本收口发布：workspace 版本 bump、CHANGELOG 转正、文档版本锚点同步、release.yml publish-crates 幂等加固 |
| **提交人** | EvoRule Team |
| **提交日期** | 2026-09-20 |
| **审查状态** | 已批准 |

**本次变更属于**: ✅ **非机制层（发布配置/文档）**

判定理由：零 src/ 变更——仅版本号 bump（workspace 统一继承）、CHANGELOG 段转正、文档版本锚点更新（含历史版本字面量中性化改写，事实保留、未新增门禁豁免）、release.yml publish-crates 加固（secret 缺位显式跳过 + 已发布幂等跳过，不改变发布语义）。

测试计划：cargo fmt/clippy/test --workspace 全绿；validate-version / validate-release(-SkipTagCheck) PASSED；scan_public_face 阻断 40 项＝在册存量零新增。

---

> 注意：这是机制层变更，后续每次修改都需要更新此文件。

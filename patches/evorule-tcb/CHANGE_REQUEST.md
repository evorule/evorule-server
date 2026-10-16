# 变更审查表 (Change Request)

## 1. 基本信息

| 字段 | 值 |
|------|------|
| **变更 ID** | CR-20260914-001 |
| **变更标题** | collect/merge 元指令退役（v0.6.0 破坏性变更）——LLM ReAct 多轮编排职责归应用层，机制层回归单轮 io_request 语义 |
| **提交人** | EvoRule Team |
| **提交日期** | 2026-09-14 |
| **审查状态** | 已批准 |

## 2. 变更层级判定（必填）

### 2.1 变更层级声明

**本次变更属于**: ✅ **机制层 (Mechanism)**

### 2.2 判定理由

```
审查批准依据：2026-09-14 经确认按 v1.1 清理计划实施，
三项关键决策（reactive_researcher 示例删除 / 宪法应用层化 /
canary 收敛后执行）均采纳推荐方案。
collect/merge 系 2026 年拆分 evo-agent run.rs 时被意外下沉至 TCB 的
应用层能力（LLM 多轮工具扇出与结果回环编排），非设计功能，与「evorule
是纯净计算基、不引入 LLM 能力」的核心原则冲突。enforce 为计划内原语
（TCB 自进化预留）保留。本变更删除 exec_collect / exec_merge /
substitute_template 及指令分发分支，META_INSTRUCTION_TYPES 收窄为
5 种（branch/set/push/io_request/enforce）；多轮编排语义由应用层
runner / tool_registry 承担，机制层仅保留 io_request 单轮触发/消费。
```

### 3.1 变更理由

事故残留原语跨仓扩散（schema/前端/宪法按「功能完整性」附带实现），
久拖不清理将持续误导消费方按 7 种原语设计规则；本方案为经批准
的彻底清理方案。

### 3.2 变更范围

- src/executor.rs（exec_collect/exec_merge 删除、分发分支收窄、
  META_INSTRUCTION_TYPES 5 种）/ src/transition.rs（规则匹配白名单
  收窄）/ src/domain.rs / src/path.rs（substitute_template 移除与
  注释口径）
- tests/integration_test.rs（ReAct 循环测试改单轮口径）；
  tests/kani/kani_proofs.rs（P15/P16/P17 删除，37→34 = A14+B20）；
  tests/kani/model.rs（any_instruction %6→%4）
- 本仓 CHANGE_REQUEST.md 本条目；版本 0.5.0→0.6.0（workspace 统一）

### 3.3 破坏性分析

**破坏性变更（Breaking）**：规则 JSON 使用 `"type": "collect"` /
`"type": "merge"` 将被 schema 枚举与引擎白名单双重拒绝（fail-fast
加载即拒，不静默忽略）。迁移方式：多轮工具编排改由应用层 runner /
tool_registry 实现。

### 3.4 影响评估

- 跨仓同步（随同批实施）：governance（VALID_TRANSFORM_TYPES 4 种）/
  cli（SSOT 断言）/ schema `_shared/v1.0.json` 双副本 / console·
  console-cloud 前端白名单与 LLM 提示词 / evo-agent 宪法单轮化 /
  server INTEGRATION_GUIDE 口径 / registry E 族 5 种收尾
- Kani：proof 34 个（删 P15/P16/P17）；kani.yml 同步 14A+20B；
  A 档 14 + reactor 4 于清理后基线重跑，B 档 20 个
  状态不变（❌）
- 规则资产：30 个规则文件零 collect/merge 引用（Step 9 验证），演示
  规则执行结果与清理前一致（确定性）

### 3.5 测试计划

- [x] `cargo build/test/clippy --workspace` 全绿（Step 8，
      2026-09-14）
- [x] 30 个规则文件全部加载 + 16 条演示规则确定性验证（Step 9）
- [ ] A 档 14 + reactor 4 WSL Kani 重跑 14/14 + 4/4 PASS（Step 10，
      随本提交执行）
- [ ] 全生态 grep 残留复检（Step 10 收尾，历史文档/CHANGELOG 豁免）

### 3.6 回滚方案

git revert 本提交即恢复 collect/merge 原语与 0.5.0 版本；schema /
前端 / 宪法 / registry 同批回滚。

## 1. 基本信息

| 字段 | 值 |
|------|------|
| **变更 ID** | CR-20260913-004 |
| **变更标题** | B 档 23 个超时 proof 四梯队攻坚：CR-003 载体修订（回退 impl 级 cfg 覆写，改 proof 层 stub）+ harness 结构自检 + 模型偏差登记簿治理 |
| **提交人** | EvoRule Team |
| **提交日期** | 2026-09-13 |
| **审查状态** | 已批准 |

## 2. 变更层级判定（必填）

### 2.1 变更层级声明

**本次变更属于**: ✅ **机制层 (Mechanism)**

### 2.2 判定理由

```
CR-001（KaniMap）+ CR-002（递归有界化）+ CR-003（eq/clone 模型化）
消解三层根因后，B 档 proof 攻坚进入 proof 结构层面：单点修复模式
（修生产代码让原 proof 收敛）已被 round 1-12 探针证伪——三层任留
一层即 150s+ 超时，乘积结构必须拆解。本变更确立四梯队结构：
- Tier 0 可行性门（T0-1~T0-7 短实验：solver/stubbing/contracts
  可用性、算术与 clone 数据流审计、unwind 敏感度、构造层复审）
- Tier 1 零侵入优化（unwind 精确化、harness 构造迁移 owned 变体）
- Tier 2 Kani 机制层（proof 层 stub、contracts 组合验证）
- Tier 3 属性分流（深度类→TLA+、溢出类→类型系统门禁、
  已具体化输入→集成测试）
- Tier 4 治理（模型偏差登记簿 + 爆炸半径映射 + 补偿合规检查）
配套纪律：B 档 harness 结构自检断言硬前置；总预算 ≤60 次验证运行
× ≤600s；每阶段 kill criteria，任意检查点可降级收尾。
载体决策依据 ADR-0002（模型弱化载体：impl 级 cfg → proof 层 stub）。
```

### 3.1 变更理由

B 档 23 个 proof 覆盖 P0-1/2/4/5/7/8 六个 P0 属性，实测 600s/3600s
全超时（STATUS.md 附录 B）；三层根因修复后仍超时证明乘积爆炸结构
需 proof 结构层拆解，而非继续单点修生产代码。

### 3.2 变更范围

- **CR-20260913-003 载体修订**（本文件该 CR 修订记录节）：value.rs
  两处 impl 级 cfg(kani) 覆写（Clone/PartialEq）回退，模型化改由
  proof 层 `#[kani::stub(...)]` 承载
- tests/kani/kani_proofs.rs：B 档 harness 加结构自检断言（W3-1，
  Phase 1 前落地）；探针代码（rounds 2-12）迁出生产 proof 文件
  （Batch 1 完成迁出与 harness 对齐）
- KaniMap/ObjectMap 配套适配（CR-001 存储后端的调用面跟随，编译
  必需）：executor.rs（含 mem::take 全值移出修复——cfg(kani) no-op
  Drop 下部分移出被拒）、lib.rs（ObjectMap 顶层 re-export）、
  path.rs / domain.rs / determinism_proptest.rs（测试面 ObjectMap
  采纳——kani CI 以 `--tests` 编译 lib test 与集成测试目标，HEAD 版
  BTreeMap 构造在 kani 下 E0308）、tests/kani/model.rs、reactor 侧
  仅测试/ffi 便利采纳（非 kani 编译面，随在途改动另行入库）
- kani.yml：stub 化 proof 批次与 `-Z stubbing` 参数（Phase 2）
- verification/STATUS.md：属性表新增"模型偏差"列（Phase 1 落地）
- cfg(kani) 偏差登记簿（K 系）：新增"爆炸半径"列与补偿合规规则
  （爆炸半径全调用点映射 / 证据-实现绑定 / 补偿不得引用受该模型
  影响的 proof——机器检查草案）

### 3.3 破坏性分析

无。生产构建（cfg(not(kani))）零改动；A 档 14 个 proof 不经 eq/clone
容器路径，回退后须重跑确认（Batch 1 门禁：14/14 绿才提交）。

### 3.4 影响评估

- A 档证据 SHA 绑定随 value.rs 回退重核：Batch 1 在 WSL 重跑 A 档
  14 个，新 SHA 证据补档（P0-3/P0-6 证据列同批更新）
- reactor 4 个闸门 proof：stub 载体下零影响（爆炸半径限于声明
  stub 的 harness）；cfg 回退场景下需重新论证（强制）
- 登记簿 K-4/K-5 补偿：stub 载体下挂 A 档
  `verify_partial_eq_never_panics`（真实现侧证明）成立；
  cfg 回退场景下降级为测试级补偿并显式标注

### 3.5 测试计划

- [x] Batch 1：value.rs 回退 + cargo test 全绿 + A 档 14 个 WSL
      重跑全绿（新 SHA 证据）+ check_doc_safety/check_status_sync 过
      （2026-09-13 实测通过；新 SHA 证据随本批归档提交落盘）
- [x] Phase 0：T0-1~T0-7 可行性矩阵定稿（每项 go/no-go 有实测/审计依据；
      2026-09-13 完成，结论摘要见 §3.7 追记）
- [ ] Phase 1：P8 系 9 个 proof × ≤3 配置（基线/精确 unwind/owned
      迁移），单 proof 中位数 ≤300s 为过关线
- [ ] Phase 2：stub 试点 2 个（eq 族 + 元指令族）≤600s；批量 ≤20 次
- [ ] Phase 3+4：分流落地 + 全文档对齐 + release-gate 演练
- [ ] 全程：验证运行预算 ≤60 次，触顶即降级收尾；各阶段停止
      准则——Phase 1 单 proof 三配置中位数 >300s 移交后续阶段路由，
      Phase 2 试点 >600s 该属性族移交 TLA+/降级，TLA+ 试跑 >3600s
      维持 N_MAX=2 如实标注

### 3.6 回滚方案

分批提交（Batch 1-5），每批原子对应代码+证据+状态+披露；任一批次
失败 revert 该批即可，前批成果（如 A 档新证据）不受影响。整体降级
路径：预算触顶或 Phase kill → 按收尾选项执行（B 档如实标 ❌，
根因档案与偏差登记簿定稿归档）。

### 3.7 Phase 0（T0-1~T0-7）可行性验证结论追记（2026-09-13 实测定稿）

| 项 | 结论 | 依据 |
|---|---|---|
| T0-1 求解器选项 | `--solver` 选项存在（bitwuzla/cadical/cvc5/kissat/minisat/z3/bin=，**默认 CaDiCaL**）；kissat 对 eq proof 单点试跑 600s 超时无改善 → **Tier 1.2 裁撤** | 实测（600s） |
| T0-2 stubbing | `-Z stubbing` 最小 stub proof 实跑 PASS → **Tier 2.1 stub 默认路径确认**（cfg 回退预案解除待命） | 实测 |
| T0-3 contracts | `requires`/`ensures`/`proof_for_contract` 实跑 PASS；`stub_verified` 编译期报错（`Failed to find contract closure`）→ **组合验证改两步独立**（stub 自带 contract + `proof_for_contract`），P19-P21 主路径维持 | 实测 |
| T0-4 算术审计 | set/add/sub 路径全 `checked_add`/`checked_sub` + `IntegerOverflow` 传播，无裸算术（仅 usize 良性位点）→ **P13 可走 checked-ops 门禁卸载** | 代码审计（executor.rs） |
| T0-5 clone 内容依赖 | evaluate_eq 子树内 eq 结果与 clone 内容仅流入 never-panic 的 `PartialEq` 与分支控制流 → **clone stub 固定值分支成立（限该子树）** | 代码审计（domain.rs/executor.rs） |
| T0-6 unwind 敏感度 | `"payload.x"`→`"p.x"` + `unwind(24)`→`unwind(6)` 单点对比仍 600s 超时 → **Tier 1.1 单独无效**；harness 完全具体输入仍爆炸，证实结构层建模成本（String/Cow/Vec/分配器）主导——三层根因模型外残余因素（入根因档案） | 实测（600s）+ 代码审计（model.rs 无符号输入） |
| T0-7 构造层复审 | F1 中毒路径（impl 级 cfg Clone 覆写经 `object_from_pairs` 清空嵌套复合值）已随 CR-20260913-003 修订消除；现存 `#[cfg(kani)]` 属性 12 处（value.rs 11 + domain.rs 1）全部属 CR-001/CR-002 批准载体，非克隆/构造路径 → **B 档重跑前置门解除** | 代码审计（value.rs/model.rs/domain.rs） |

**方案收缩汇总**：Tier 1.2 裁撤；Tier 1.1 降级为 W3-3/W3-4 配套动作（eq 族按 kill criteria 以首个数据点提前路由 Phase 2 stub 试点）；Tier 2.3 组合验证两步化。Phase 1 计划不变（W3-1 结构自检断言先行）。

### 3.8 Phase 1 W3-1 执行记录（结构自检断言，2026-09-14 定稿）

**落地内容**（`tests/kani/kani_proofs.rs` 单文件，proof 源码变更）：

1. **7 个结构自检助手**：`shape_field`（键存在+返回引用）/ `shape_str`（字符串相等）/ `shape_str_in`（集合哨兵）/ `shape_array`（定长数组）/ `shape_bool` / `shape_payload_leaf`（payload 叶子定位）/ `shape_full_state` 与 `shape_concrete_exec_state`（两族 state 哨兵）。全部为具体值相等断言——不引入符号分支、不改变被证属性解空间，只拦截构造退化导致的假验证。
2. **23 个 B 档 harness 全部接线**（含 13 处构造根接线修复：原草稿将 `payload` 键误作构造根传入 `shape_payload_leaf`，按各 harness 实际构造根改接）。
3. **canary 本地验证（本地验证手段，验证后移除）**：
   - 正向 ✅：`c1`（shape_field/shape_str/shape_str_in，76.9s）、`c3`（shape_payload_leaf × single_key_exec_state，32.5s）、`c2b`（shape_array 裸数组最小载体，**0.55s**）；
   - 反向 ✅：`degraded_fails_loudly`（F1 类退化构造）13.9s 于预期断言点精确响亮失败（unwind 8 下验证，其失败为键缺失 panic，与 unwind 取值无关）——假通过防线成立；
   - **构造墙发现（移交 W3-3/W3-4）**：全具体构造在 CBMC 0.67 下随构造复杂度非线性恶化——`c2`（2 键 map+1 元数组，断言逻辑与 c2b 完全相同）300s 不收敛 vs `c2b`（裸数组）0.55s；`c4a`（`concrete_exec_state` 纯构造、零断言）120s 不收敛；`c5`（3 层嵌套镜像 state）约 250s 被终止。**构造成本本身（String/Cow/Vec/分配器建模）是膨胀源，与断言无关**（T0-6 结论在微型尺度复现）。`shape_full_state`/`shape_concrete_exec_state` 两复合哨兵的载体构造受同一构造墙限制而无法独立实跑，由「原语已验证 + 具体相等断言 + 反向防线」支撑，完整实跑验证随 W3-3/W3-4 闭环。**连带影响**：P9/P10（构造 `concrete_exec_state`）Phase 1 直跑将撞同一构造墙，须 W3-3 owned 迁移或 W4-1 stub 路线先解除。
   - **调试插曲（两层根因，W3-2 要求更新）**：整体 canary 于 unwind(8) 两次 600s 不收敛曾疑似求解器问题；`--debug` 探针 + 拆分定位还原真因：① `unwind(8) < memcmp 字节循环深度`（键/值串如 `"payload.x"` 9 字符需约 10 次展开）→ unwinding 断言失败毒化公式（920 项检查 919 项 undetermined）→ 求解器无限研磨（非求解器问题）；② 修至 unwind(24) 后露出上述构造墙。**W3-2 配套要求据此更新**：B 档 harness 的 unwind 必须 > 其形状断言最长字符串的 memcmp 深度（字节数+2），否则 unwinding 断言假失败。

**证据处理**：kani_proofs.rs 变更触发 A 档 14 proof 证据 SHA 绑定重核，同协议重跑 14/14 PASS，新旧证据分档管理；STATUS.md 证据列/快照同批更新。

**W3-1 结论：结构自检硬前置达成**。

### 3.9 W3-2 unwind 静态盘点（2026-09-14，零运行零代码变更）

**口径与方法**：W3-1 调试插曲确立「`#[kani::unwind]` 属性隐含开启该 harness 的 unwinding assertions（canary 毒化实证），无属性 = 默认 unwind 10 且断言关闭 = 静默截断（验证不完备）」。据此对 23 个 B 档 harness 静态盘点 memcmp/memcpy 成功路径深度 = max(harness 块内值路径字面量, 经 inline 引入的 model.rs 构造字面量) + 2；排除中文 panic 消息（仅失败路径执行）与 shape 助手首参（what 标签，仅入 panic 消息）。A 档 14 个已实证豁免（unwinding assertions 开且 14/14 PASS，展开充分性由 PASS 自证）。

**盘点结论**（合规 7 / 不合规 16；`cur` = 现有属性值，无属性记 10）：

| harness | cur | need | 建议 unwind | 备注 |
| --- | --- | --- | --- | --- |
| evaluate_domain eq / lt / exists | 24 | 13 | 24 ✔ | |
| evaluate_domain not | 24 | 19 | 24 ✔ | |
| evaluate_domain has_fields | 24 | 15 | 24 ✔ | |
| exec_enforce_never_panics / _deterministic | 10(无属性) | 2 | 补属性，W3-4 校准 | 字符串口径不适用（符号输入型），unwind 需按符号结构实跑校准 |
| evaluate_domain instruction | 24 | 25 | 32 | 差 1（`domain.instruction_type` 23B） |
| evaluate_domain all | 4 | 14 | 16 | 历史遗留错误值 |
| evaluate_domain deterministic | 10(无属性) | 13 | 16 | |
| domain_depth_limit | 12 | 13 | 16 | 差 1 |
| has_fields_empty_array | 10(无属性) | 35 | 48 | |
| execute_meta_instruction | 10(无属性) | 13 | 16 | |
| exec_set_arithmetic_safe | 10(无属性) | 18 | 24 | |
| branch_depth_limit | 10(无属性) | 25 | 32 | |
| collect_safe_with_after | 10(无属性) | 30 | 32 | |
| merge_safe | 10(无属性) | 32 | 48 | |
| substitute_template | 10(无属性) | 25 | 32 | |
| io_request_safe | 10(无属性) | 32 | 48 | |
| exec_enforce_halt_semantics | 10(无属性) | 20 | 24 | |
| execute_transition | 10(无属性) | 18 | 24 | |
| transform_rules_limit | 10(无属性) | 18 | 24 | |
| react_io_required | 10(无属性) | 47 | 64 | model::react_core_eval 路径 45B 主导 |

**处置决策**：本次**零代码变更**。16 个不合规项的 unwind 校准登记为 **W3-4 前置配套**——W3-4 本来就以 unwind 为配置维度逐 harness 实跑（≤3 配置），届时按上表建议值起步、以 unwinding assertions 反馈逐个收敛，一次改完 proof 源码 + 一次 A 档证据重跑，避免本次单独变更触发第二次证据重跑。

### 3.10 Phase 1 W3-3 执行记录（owned 构造迁移，2026-09-14）

**落地内容**（Tier 5.3 先行项：构造层与 Clone 实现解绑）：

1. `model.rs`：`obj()` 由 `object_from_pairs(&[(&str, JsonValue)])`（引用对 + 内部深克隆）改为 `object_from_pairs_owned(Vec<(&str, JsonValue)>)`（值 move 进 ObjectMap，零深克隆），可见性 pub(crate)。
2. `kani_proofs.rs`：23 个 B 档 harness 构造调用点适配（`obj(&[...])` → `obj(vec![...])`），迁移后旧构造形态零残留（grep 实证：`obj(&[` / `object_from_pairs(&[` 0 命中；owned 形态 96 处落位）。
3. 编译验收：A 档最快 proof `verify_partial_eq_never_panics` 迁移后实跑 **1.0s PASS**（同编译单元完整性确认，534 断言 0 失败）。

**证据处理**：model.rs/kani_proofs.rs 变更触发 A 档 14 proof 证据 SHA 绑定重核，同协议重跑 14/14 PASS，新旧证据分档管理；STATUS.md 证据列/快照同批更新。

**W3-4 前置就绪**：16 项 unwind 校准表（§3.9）+ owned 构造两要素齐备；W3-4 按配置维度（精确 unwind + owned）首跑 P8 系 9 个，eq 族首数据点按 kill criteria 提前路由 Phase 2 stub 试点。

### 3.11 Phase 1 W3-4 执行记录（精确 unwind 校准落地 + eq 族首数据点路由，2026-09-14）

**落地内容**（提交 `627330a`，一次改完 proof 源码纪律）：

1. `kani_proofs.rs` 4 处 unwind 按 §3.9 建议值校准：instruction 24→32 / all 4→16 / deterministic 补 16 / domain_depth_limit 12→16。
2. W3-3 临时 canary（c2-clone/c2-owned）删除清理（"迁移验证后删除，不入库"承诺收口）。
3. eq 族首数据点：`verify_evaluate_domain_eq_never_panics`（unwind 24 + owned 构造）**600s 超时**（kill criteria 触发）→ eq 族路由 **Phase 2 stub 试点**（ADR-0002 载体）。构造墙主因判断强化：String/KaniMap 建模开销，owned 迁移仅解 clone 分量。

**证据处理**：unwind 属性变更触发 A 档 14 proof 证据重核，于 `627330a` 重跑 14/14 PASS（0.3~4.0s，WSL Kani 0.67.0），新旧证据分档管理；STATUS.md（快照/P0-3/P0-6 证据列/维护区）同批更新。

**其余 8 个 P8 系首轮处置（暂缓独立实跑，合并至清理后基线）**：CR-20260914-001（collect/merge 清理）即将变更同批 proof 源码（`any_instruction` %6→%4、P15/P16/P17 删除、shape 构造同步），预跑数据点将随源码变更立即失去对象意义；且 eq 数据点已对同构造墙 regime 定性（全族同核构造路径）。其测量合并至该清理 Step 10 全量 proof 重跑（清理后基线）与 Phase 2 stub 试点一并执行，避免重复已知超时路径的无效测量。

## 4. CR-20260913-003 修订记录（2026-09-13，随 CR-20260913-004 生效）

**修订**：实施载体由 impl 级 `cfg(kani)` 双实现改为 proof 层
`#[kani::stub(...)]`（依据 [ADR-0002](../docs/adr/ADR-0002-B档proof模型载体与stub化验证策略.md)）。

**修订理由（事实）**：载体复核确认 impl 级覆写在 kani 构建下影响
全部调用点（含 harness 构造层 `object_from_pairs` 内的 `v.clone()`，
嵌套复合输入在深度 1 处被清空——proof 实际验证退化输入）；且真实
trait impl 从 kani 构建产物中消失，使"真实现侧证明补偿"的登记
（如 eq 模型挂 A 档 `verify_partial_eq_never_panics`）引用到被覆写
的模型本身，补偿不可成立。

**修订动作**：value.rs 两处 impl 级 cfg(kani) 覆写（Clone/PartialEq）
回退；原模型语义（容器变体长度比较/空容器拷贝、标量保真）保留为
stub 模型设计输入，由 proof 层声明承载。原 CR 文本（下文）保留不删，
其中 §3.2/§3.4/§3.5 所述 impl 级实现方式以本修订记录为准。

**原批准内容如下**（未改动）：

---

## 1. 基本信息

| 字段 | 值 |
|------|------|
| **变更 ID** | CR-20260913-003 |
| **变更标题** | JsonValue PartialEq/Clone Kani 模型化：容器变体长度比较/空容器拷贝，消除 eq 分支 CBMC 编码爆炸（B 档 proof 第三层根因） |
| **提交人** | EvoRule Team |
| **提交日期** | 2026-09-13 |
| **审查状态** | 已批准（2026-09-13 载体修订：见上方修订记录） |

## 2. 变更层级判定（必填）

### 2.1 变更层级声明

**本次变更属于**: ✅ **机制层 (Mechanism)**

### 2.2 判定理由

```
round 9 三变体探针（同一 evaluate_eq 副本，各 stub 一个嫌疑，
其余保真，unwind 24）实测全部 150s 超时：
- probe3a_noclone（去 clone，真 == + 真 resolve）
- probe3b_noresolve（去 resolve，真 clone + 真 ==）
- probe3c_nocmp（去 ==，真 clone + 真 resolve）
两两交集只剩 == 与 clone 各自独立爆炸：
- 手写 PartialEq 的 Array 分支 `Vec ==`（符号长度按 unwind 上限
  展开，元素递归 JsonValue 比较）
- 手写 PartialEq 的 Object 分支 `iter`/`get` 逐键循环 + 递归 `!=`
- derive(Clone) 的 Vec/ObjectMap 递归拷贝（同符号长度展开）
对照：probe2_q（全 stub 分支）35s PASS；A 档 P1/P2 对具体小值
的 ==/cmp 9~28s PASS（常量传播限制路径，无符号容器展开）。
本变更将 PartialEq/Clone 改为 impl 级 cfg 双实现：
- cfg(not(kani))：原手写 PartialEq 逐字保留；Clone 手写全变体
  深拷贝（与 derive 等价）
- cfg(kani)：容器变体 == 只比长度、clone 返回空容器；标量变体
  （Null/Bool/Integer/String-Cow）保持真比较/真拷贝
```

### 3.1 变更理由

CR-001（KaniMap）+ CR-002（递归树有界化）落地后 eq proof 仍超时
（MAX_DOMAIN_DEPTH=1 实测 300s），第三层根因为 eq 分支内部的
`==`/`clone` 符号容器编码爆炸。

### 3.2 变更范围

- value.rs：`impl PartialEq`（impl 级 cfg 双实现）；`#[derive(Debug, Clone)]`
  改 `#[derive(Debug)]` + 手写 `impl Clone`（impl 级 cfg 双实现）

### 3.3 破坏性分析

无。生产构建（not(kani)）PartialEq 原实现逐字保留，手写 Clone 与
derive 语义等价（全变体深拷贝）；kani 构建的模型语义在下方披露。

### 3.4 影响评估

- 属性安全性：B 档 proof 对求值结果均 `let _ =`（never-panic），
  不断言值语义；A 档 P1（PartialEq 永不 panic）同样 `let _ =`
- reactor proof 断言全为标量（usize/bool），无 JsonValue == 依赖
- kani 下 eq 域值语义弱化（容器变体只比长度）如实披露；
  值语义由 cargo test（非 kani 构建）与 differential 测试覆盖

### 3.5 测试计划

- 全 workspace `cargo test` 回归（生产 Clone/PartialEq 等价性）
- cfg(kani) `cargo check` 编译验证
- eq 系 proof Kani 实测（验证编码爆炸消除）

## 1. 基本信息

| 字段 | 值 |
|------|------|
| **变更 ID** | CR-20260913-002 |
| **变更标题** | 域评估递归树有界化：cfg(kani) 下 MAX_DOMAIN_DEPTH 64→4，消除 CBMC 递归调用树指数编码（B 档 proof 第二层根因） |
| **提交人** | EvoRule Team |
| **提交日期** | 2026-09-13 |
| **审查状态** | 已批准 |

## 2. 变更层级判定（必填）

### 2.1 变更层级声明

**本次变更属于**: ✅ **机制层 (Mechanism)**

### 2.2 判定理由

```
Kani 0.67 CBMC 后端对 evaluate_domain_inner 的 all/not 分支递归
调用自身做无条件调用树编码（每层扇出 2）：unwind 24 = 2^24 实例，
150s 超时且与运行时输入无关。2026-09-13 六轮二分探针定位：
- probe2_q（同逻辑去递归版，all/not 换非递归 stub）35s PASS（unwind 24）
- probe2_o（真递归版，相同 unwind）151s TIMEOUT
- 全内联等价物 / 本地函数跨调用（含 ok_or_else 闭包 + ? 传播
  + Result 跨函数返回）均 13-23s PASS
本变更在 cfg(kani) 下将 MAX_DOMAIN_DEPTH 从 64 降为 4：
- 深度保护分支（depth > MAX_DOMAIN_DEPTH 返回 Err(NestingTooDeep)）
  使递归树有界终止（2^5 实例），无需 unwind 截断
- 生产构建（cfg(not(kani))）保持 64 零改动
```

### 3.1 变更理由

B 档 23 个 proof 中域评估系列（P8a-P8g/P9/P10）在 KaniMap
（CR-20260913-001）落地后仍超时——存在独立于 BTreeMap 的第二层
根因：递归调用树指数编码。

### 3.2 变更范围

- domain.rs：`MAX_DOMAIN_DEPTH` 条件编译（`not(kani)`=64 / `kani`=4），
  文档注明 Kani 验证模型语义（「深度限制为 4 的域评估器」完备验证）
- tests/kani/kani_proofs.rs：P10 适配（构造 `MAX_DOMAIN_DEPTH+1` 层 not、
  unwind 70→12、常量引用替代硬编码 65、import MAX_DOMAIN_DEPTH）

### 3.3 破坏性分析

无。生产构建 `MAX_DOMAIN_DEPTH=64` 零改动；kani 构建下深度限制
变化已在 proof 属性语义中如实披露。

### 3.4 影响评估

- 全 workspace `cargo test` 回归须绿（生产路径无改动）
- kani 构建下 src 单元测试仅编译不运行（65 层构造测试无影响）
- P10 属性语义更新：验证深度保护分支在 5 层到达且不 panic

### 3.5 测试计划

- [x] 代表性 proof 探针定位（六轮二分，根因 100% 确认）
- [ ] `cargo check`/`cargo test` 生产构建回归
- [ ] 代表性 B 档 proof Kani 实测（eq + P10）
- [ ] 23 个 B 档 proof 全量重跑 + A 档 14 个 + reactor 4 个回归

### 3.6 回滚方案

git revert 本提交即恢复 MAX_DOMAIN_DEPTH=64 单值定义（B 档域评估
proof 回到不可运行状态，无其他副作用）。

---

## 附 · 历史变更归档

### CR-20260913-001（已批准）：ObjectMap Kani 后端切换：cfg(kani) 下 KaniMap（有序 Vec）替代 BTreeMap，消除 23 个 B 档 proof 状态爆炸

> 归档说明：原 CR 整表置顶至 2026-09-13（CR-20260913-002 置顶），与 002 同批次提交入库，完整内容见 git 历史。要点：kani 构建下 ObjectMap=KaniMap（有序 Vec 模拟 BTreeMap，条目键字典序不变，API 兼容），JsonValue/KaniMap 增 cfg(kani) no-op Drop；生产构建零改动。
### CR-20260902-001（已批准）：元指令类型白名单 SSOT 化：META_INSTRUCTION_TYPES 常量导出 + 漂移防线（回归验证 C2）

> 归档说明：原 CR 整表置于顶层至 2026-09-13（CR-20260913-001 置顶），完整内容见 git 历史。

### CR-20260830-001（已批准）：build.rs 门禁状态机生命周期撇号判别修复（strip_test_mod 误报消除）

> 归档说明：原 CR 整表置于顶层至 2026-09-02（CR-20260902-001 置顶），完整内容见 git 历史。

| 字段 | 值 |
|------|------|
| **变更 ID** | CR-20260830-001 |
| **变更标题** | build.rs 门禁状态机生命周期撇号判别修复（strip_test_mod 误报消除） |
| **提交人** | EvoRule Team |
| **提交日期** | 2026-08-30 |
| **审查状态** | 已批准 |

本变更只修改 build.rs 门禁自身实现，不触及任何 src/ 执行语义：
strip_test_mod/find_inline_lbrace/match_brace 状态机在撇号处新增
char_lit_starts() 判别（字符字面量 vs 生命周期），新增 skip_lifetime() 跳过；
修复前 'static 等生命周期撇号被误判为字符态开头，令 tests 模块整体不被剥离、
门禁对测试代码全量误报。五仓同一份实现同步修复。回滚：git revert。

### CR-20260827-001（已批准）：core_eval.json v0.4.0：ReAct 应用剧本整体迁出至消费方（T8 最小化专项）

> 归档说明：原 CR 整表收录于 2026-08-30，完整内容见 git 历史。

| 字段 | 值 |
|------|------|
| **变更 ID** | CR-20260827-001 |
| **变更标题** | core_eval.json v0.4.0：ReAct 应用剧本整体迁出至消费方（T8 最小化专项） |
| **提交人** | EvoRule Team |
| **提交日期** | 2026-08-27 |
| **审查状态** | 已批准 |

资产层变更：core_eval.json 移除三条 ReAct 循环 transform 规则（v0.3.1 → 0.4.0），剧本迁至消费方自持运行宪法（app.evoagent.agent v0.4.0），核心仓回归最小引擎自评估集；机制层代码零改动。回滚：git revert 即恢复 v0.3.1。

### CR-20260820-002（已批准）：添加变更治理门禁机制和策略层检测

> 归档说明：原文件整表收录于 2026-08-27，内容未改动。

| 字段 | 值 |
|------|------|
| **变更 ID** | CR-20260820-002 |
| **变更标题** | 添加变更治理门禁机制和策略层检测 |
| **提交人** | EvoRule Team |
| **提交日期** | 2026-08-20 |
| **审查状态** | 已批准 |

本次变更提供通用的变更治理基础设施：CHANGE_REQUEST.md 验证是通用的审查流程管理能力；策略层反模式检测是通用的代码质量保障能力；这些能力可被任何机制层代码复用；不包含任何特定业务语义；定义的是"怎么做"的通用方式，而非"做什么"的业务规则。属机制层变更，影响 build.rs 与本文件自身。回滚方案：删除 build.rs 中的变更治理验证代码和策略检测代码即可。

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

## CR-20260928-001 core_eval.json v0.5.0：tool_trace 工具轨迹记录规则（立项批复）

| 字段 | 值 |
|------|------|
| **变更 ID** | CR-20260928-001 |
| **变更标题** | core_eval.json v0.5.0：新增 tool_trace 规则——应用层工具调用轨迹的事实记录通道 |
| **提交人** | EvoRule Team |
| **提交日期** | 2026-09-28 |
| **审查状态** | 已批准（用户批复：宪法策略增补属低代价系统增强，必要时可添加；物理原语零新增） |

**本次变更属于**: ✅ **资产层变更（core_eval.json）+ 测试补充，机制语义零改动**

判定理由：core_eval.json transform 增补 1 条 branch(tool_trace) 规则（与既有 set 处理规则完全同构，仅组合既有 set 动作/branch 控制流/instruction 域函数——元指令与域函数零新增，不触及物理原语）；version 0.4.0→0.5.0；metadata description/provenance/constraints 随动。transition.rs 仅新增 3 个测试（写入/累积/enforce 拦截），机制代码零改动。背景：工具调用不过引擎=治理面盲区，tool_trace 使应用层工具轨迹经 StateTransition 事实落审计链，规则面可对 instruction_type=tool_trace 精确求值（事前检测）；违规轨迹经约束门 Halted→Violation 留痕，两类留痕互补。确定性/可回放/审计链红线不触及：轨迹数据只经 set 进入 payload，哈希链与状态机语义零改动。

测试计划：cargo fmt/clippy/test 全绿（231 单元测试含 3 新增）；build.rs 策略层检测/字面量门禁 PASSED；下游 reactor/governance 门禁 PASSED。回滚：git revert 即恢复 v0.4.0。

## CR-20260929-001 core_eval.json v0.6.0：tool_trace 规则迁出至部署面规则集（项目方裁定）

| 字段 | 值 |
|------|------|
| **变更 ID** | CR-20260929-001 |
| **变更标题** | core_eval.json v0.6.0：tool_trace 规则迁出至部署面规则集（server_eval.json 自持） |
| **提交人** | EvoRule Team |
| **提交日期** | 2026-09-29 |
| **审查状态** | 已批准（用户裁定：v0.5.0 的 tool_trace 增补属应用层策略规则越界入宪法，按双文件职责模型应落部署面；同批裁定部署面规则集拆分独立 id org.evorule.server.eval） |

**本次变更属于**: ✅ **资产层变更（core_eval.json），机制语义零改动**

判定理由：transform 移除 1 条 branch(tool_trace) 规则（9→8 条），本宪法回归纯计算原则集——应用层策略规则不再进入宪法（双文件职责模型 2026-09-02 定调，2026-09-29 经项目方改判强化）；version 0.5.0→0.6.0 单调递增；metadata description/provenance/constraints 随动（含 tool_trace 撤销兼容说明——在本宪法单独使用时提交 tool_trace 将 Ignored→Error 事实显式警告，与 query_db 等同款口径）。部署面 server_eval.json 已自持 tool_trace 规则（2026-09-28 同步），运行面零影响；executor 代码层 tool_trace 指令类型注册属语言规范能力，保留不动。确定性/可回放/审计链红线不触及：规则集数据变更不进入哈希链。

测试计划：cargo fmt/clippy/test --workspace 全绿；公开面扫描零阻断。回滚：git revert 即恢复 v0.5.0。

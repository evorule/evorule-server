<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: CC-BY-4.0

  This documentation is licensed under Creative Commons Attribution 4.0 International.
-->

# evorule-server 实战集成指南

> **本文档补充 [README.md](../README.md) 的快速开始**，深入到 I/O handler 架构、session 生命周期、审计链完整使用、规则编写实战等真实业务集成场景。
>
> 如果 README 是"能跑起来"，本文档是"跑好真实业务"。
>
> 阅读前建议先浏览 [README.md](../README.md) 了解架构概览和配置参数。
> 避坑指南见 [PITFALLS.md](PITFALLS.md)。

---

## 目录

- [一、I/O Handler 架构](#一io-handler-架构)
- [二、Session 完整生命周期](#二session-完整生命周期)
- [三、审计链完整使用](#三审计链完整使用)
- [四、规则编写实战要点](#四规则编写实战要点)
- [五、本地开发环境搭建](#五本地开发环境搭建)
- [八、插件清单（部署期启用/裁剪）](#八插件清单部署期启用裁剪)

---

## 一、I/O Handler 架构

### 1.1 为什么需要 I/O Handler

evorule 核心是**确定性执行引擎**——给定相同输入，产出相同输出。但真实业务需要调用外部服务（数据库、HTTP API、LLM）。这些 I/O 操作是非确定性的（网络延迟、服务状态）。

evorule 的解决方案：**把 I/O 从执行链路中抽离**。反应器执行到 `io_request` 指令时，不直接发请求，而是产生一个 `IoRequest` 事件挂起等待。外部 `IoSubscriber` 监听事件、执行实际 I/O、回写 `IoResponse`，反应器收到响应后继续执行。

```
反应器                      IoSubscriber                 外部服务
  │                             │                           │
  │── IoRequest 事件 ──────────→│                           │
  │   (io_type, service_name,   │── HTTP POST ─────────────→│
  │    args)                    │←── HTTP Response ─────────│
  │                             │                           │
  │←── IoResponse 回写 ─────────│                           │
  │   (__io_result__)           │                           │
  │                             │                           │
  │── 继续执行后续指令 ──→      │                           │
```

这样**执行链路是确定性的**（规则逻辑），**I/O 是异步的**（handler 执行），两者解耦。

### 1.2 IoDispatcher 与 IoSubscriber

```
IoDispatcher（共享，Clone）
  │── handlers: HashMap<IoType, Arc<dyn IoHandler>>
  │    │── "http"         → HttpHandler
  │    │── "call_service" → ServiceRegistryHandler
  │    │── "db"           → DbHandler
  │    └── "memory"       → MemoryHandler
  │
  └── 被 N 个 IoSubscriber clone 共享
       │
       ├── Session 1 的 IoSubscriber（per-session spawn）
       ├── Session 2 的 IoSubscriber
       └── ...
```

- **IoDispatcher**：handler 注册表，`IoType` → `IoHandler` 映射。`#[derive(Clone)]`，内部全是 `Arc`，clone 开销极低
- **IoSubscriber**：**per-session** 运行，监听单个 session 的 event 通道，收到 `IoRequest` 事件时查 dispatcher 找对应 handler 执行，回写 `IoResponse`

### 1.3 ServiceRegistry 机制

`call_service` 是最常用的 `io_type`。它通过 `service_registry.json` 把服务名映射到 HTTP 端点：

```json
// service_registry.json
{
  "services": {
    "inverse_kinematics_solver": {
      "url": "http://127.0.0.1:5101/solve",
      "method": "POST",
      "timeout_ms": 10000
    },
    "robot_move": {
      "url": "http://127.0.0.1:5102/move",
      "method": "POST"
    },
    "llm_advisor": {
      "url": "http://127.0.0.1:5103/advise",
      "method": "POST"
    }
  }
}
```

规则中 `io_request` 指定 `service_name`，`ServiceRegistryHandler` 查注册表找到 URL，用 `HttpHandler` 发请求，**自动解析 JSON 响应为结构化 `JsonValue`**（见 PITFALLS 坑 4）。

### 1.4 I/O 两阶段协议

规则编写 I/O 时必须遵循两阶段协议：

```
阶段 1：发起 I/O
  规则执行 io_request → 反应器检查 payload.__io_result__ 是否存在
  → 不存在 → 产生 IoRequest 事件 → 反应器挂起等待

阶段 2：消费结果
  IoSubscriber 处理请求 → 回写 __io_result__ → 反应器恢复
  → 规则再次执行 → 检查 __io_result__ 存在 → 消费结果 → 清除 __io_result__
  → 继续后续指令
```

**关键**：规则不需要手动处理两阶段——`io_request` 指令内部自动处理。但理解这个协议有助于排查 I/O 超时问题（见 PITFALLS 排查方法论）。

---

## 二、Session 完整生命周期

### 2.1 创建 Session

```bash
POST /api/sessions
# 无需请求体
```

```json
// 响应
{"session_id": 1, "message": "Session created"}
```

> **D-S1 对齐(2026-08-03)**：实际响应只含 `session_id` 与 `message`，无 `created_at`/`max_rounds`（此前的文档字段是臆造的）。

创建后，server 会**自动为该 session spawn IoSubscriber**（前提是 `SessionApi` 注入了 dispatcher，见 PITFALLS 坑 1）。

### 2.2 提交命令

```bash
POST /api/sessions/1/command
Content-Type: application/json
```

```json
{
  "instruction": {
    "type": "sequence",
    "instructions": [
      {"type": "set", "params": {"attr": "counter", "operation": "set", "value": 1}},
      {"type": "io_request", "params": {
        "io_type": "call_service",
        "service_name": "ik_solver",
        "args": "__exec__.payload._ik_args"
      }}
    ]
  }
}
```

**注意**：请求体必须是 `{"instruction": {...}}` 包装，不是裸指令（见 PITFALLS 坑 10）。

命令提交后**立即返回**（异步执行），需要轮询 `/state` 或订阅 SSE `/events` 等待结果。

### 2.3 查询状态

```bash
GET /api/sessions/1/state
```

```json
{
  "payload": {
    "counter": 1,
    "service_result": {"converged": true, "joint_positions": ["0.46", "-2.22"]}
  },
  "queue": [],
  "version": 12,
  "reactor": {
    "phase": "stable",
    "causal_depth": 4,
    "structural_invariant_violations": 0,
    "pending_io_count": 0,
    "current_step": 2
  }
}
```

> **D-S2 对齐(2026-08-03)**：实际响应含 `payload` / `queue` / `version` / `reactor` 四字段。`phase` 不是顶层字段，而是嵌套在 `reactor` 子对象中（值来自 `ReactorPhase::as_str`，全小写：`idle` / `draining` / `executing` / `awaiting_io` / `stable` / `error`）。此前的文档把 `phase` 写在顶层且大写为 `Stable`，与实现不符。

业务数据在 `state["payload"]` 下（见 PITFALLS 坑 12）。客户端轮询示例：

```python
def wait_for_field(client, session_id, field_path, timeout=30.0):
    """轮询直到 payload 中指定字段出现"""
    deadline = time.time + timeout
    while time.time < deadline:
        state = client.get_state(session_id)
        payload = state.get("payload", {})
        if _resolve_path(payload, field_path) is not None:
            return payload
        time.sleep(0.1)
    raise TimeoutError(f"field '{field_path}' not appeared in {timeout}s")
```

### 2.4 销毁 Session

```bash
DELETE /api/sessions/1
```

销毁后：
- 反应器的 `command_tx` 被丢弃 → 反应器优雅退出
- IoSubscriber 因 event 通道关闭而自动退出
- WAL 数据保留（如果配置了 `--wal-dir`）

Session 有 30 分钟 TTL，无活动自动过期（后台 reaper 每 5 分钟清理）。

---

## 三、审计链完整使用

Session 模式下每个会话有**独立的审计器**和**独立哈希链**（见 PITFALLS 坑 11）。共 7 个审计 API：

### 3.1 查询审计报告

```bash
GET /api/sessions/1/audit
```

```json
{
  "session_id": 1,
  "fact_count": 12,
  "last_hash": "5b9b94a266d7aae0...",
  "verified": true,
  "entries": [
    {"fact_id": 30000, "fact_type": "Command", "logical_time": 1,
     "prev_hash": "genesis", "content_hash": "e4d8ee2e..."},
    {"fact_id": 1, "fact_type": "StateTransition", "logical_time": 2, ...},
    {"fact_id": 2, "fact_type": "IoRequest", "logical_time": 3, ...},
    {"fact_id": 10000, "fact_type": "IoResponse", "logical_time": 4, ...},
    ...
  ]
}
```

每条记录的 `prev_hash` 指向前一条的 `content_hash`，从 `genesis` 起形成不可篡改链。

### 3.2 验证完整性

```bash
GET /api/sessions/1/audit/verify
```

```json
{"verified": true, "session_id": 1, "fact_count": 12, "last_hash": "..."}
```

校验整条哈希链前后衔接。`verified: true` 表示审计链未被篡改。

### 3.3 因果链追溯

```bash
GET /api/sessions/1/audit/causal/10000
```

追溯 `IoResponse(fact_id=10000)` 的因果链：

```json
{
  "session_id": 1,
  "fact_id": 10000,
  "chain_length": 3,
  "chain": [
    {"fact_id": 10000, "fact_type": "IoResponse", "logical_time": 4, "cause": 2},
    {"fact_id": 2, "fact_type": "IoRequest", "logical_time": 3, "cause": 30000},
    {"fact_id": 30000, "fact_type": "Command", "logical_time": 1, "cause": null}
  ]
}
```

`cause` 字段层层回溯到根因（`cause: null` 的用户 Command）。用于根因分析、合规追溯。

**注意**：`cause` 记录的是**逻辑因果**，不是时序相邻——IoRequest 的 cause 直接指向 Command，跳过中间的 StateTransition。

### 3.4 导出与导入

```bash
# 导出 JSON
GET /api/sessions/1/audit/export
# → {"version":"1.0","entry_count":12,"last_hash":"...","entries":[...]}

# 导出 gzip（体积约 JSON 的 40-45%）
GET /api/sessions/1/audit/export/compressed
# → 二进制 gzip 数据（Content-Type: application/gzip）

# 导入 JSON（破坏性：覆盖目标 session 审计链）
POST /api/sessions/2/audit/import
Content-Type: application/json
{...导出的 JSON...}
# → {"imported": true, "verify_ok": true, "status": "ok"}

# 导入 gzip（破坏性）
POST /api/sessions/3/audit/import/compressed
Content-Type: application/gzip
<gzip 二进制>
# → {"imported": true, "verify_ok": true, "status": "ok", "format": "gzip"}
```

**用途**：跨实例迁移、离线分析、备份恢复。导入后自动 `verify` 校验完整性。

> ⚠️ 导入是**破坏性操作**，会覆盖目标 session 的审计链。建议先导出备份。

---

## 四、规则编写实战要点

### 4.1 meta 指令

evorule TCB 有 **6 种合法 meta 指令**（0.3.2 起，之前为 4 种）：

| 指令 | 用途 | 示例 |
|------|------|------|
| `set` | 写 payload 字段 | `{"type":"set","params":{"attr":"x","operation":"set","value":1}}` |
| `push` | 入队业务指令 | `{"type":"push","params":{"instruction":{...}}}` |
| `branch` | 条件分支 | `{"type":"branch","params":{"domain":{...},"on_true":[...],"on_false":[...]}}` |
| `io_request` | 发起 I/O | `{"type":"io_request","params":{"io_type":"call_service",...}}` |
| `collect` | 遍历数组生成多条指令（多工具扇出） | `{"type":"collect","params":{"from":"__exec__.payload.items","template":{...}}}` |
| `merge` | 将工具结果合并进消息历史 | `{"type":"merge","params":{"messages":"__exec__.payload.history","tool_result":"__exec__.payload._io_results.call_service"}}` |

**任何其他 `type` 都不是 meta 指令**，会被当作业务指令 push 到队列。

> **0.3.2 重要变更**: `noop` / `increment` / `decrement` 是**业务指令层**类型（队列中的指令），不是 meta 指令。之前的文档误将它们列为 meta 指令，导致 `core/rule_schema` 校验出现假阳性/假阴性。`/api/rules/validate` 现在会明确拒绝 transform 规则中出现这些类型。

> **规则 Schema 校验**: 提交规则前建议先通过 `POST /api/rules/validate` 校验，该端点使用 `core/rule_schema` crate 的 JSON Schema（`rule_set/v1.0.json` + `_meta/v1.0.json` + `_shared/v1.0.json`）做权威校验，比 evorule TCB 内部校验更早发现问题。

### 4.2 io_request 参数结构

`call_service` 类型的固定参数：

```json
{
  "type": "io_request",
  "params": {
    "io_type": "call_service",
    "service_name": "ik_solver",
    "args": {"target_pose": {"x": "0.5"}, "solver_type": "LMA"}
  }
}
```

- `io_type`：`call_service` / `http` / `db` / `memory`
- `service_name`：`service_registry.json` 中注册的服务名（可引用 payload 路径：`"__exec__.instruction.params.service_name"`）
- `args`：请求体参数（可引用 payload 路径：`"__exec__.payload._ik_args"`，见 PITFALLS 坑 7）

### 4.3 domain path 必须带 payload. 前缀

```json
// 正确
{"type": "eq", "path": "payload.service_result.converged_ok", "value": true}

// 错误（查找 __exec__.service_result，不存在）
{"type": "eq", "path": "service_result.converged_ok", "value": true}
```

详见 PITFALLS 坑 8。

### 4.4 conditional 条件判断

```json
{
  "type": "conditional",
  "condition": {
    "type": "not",
    "inner": {"type": "eq", "path": "payload.pending_alert", "value": null}
  },
  "on_true": [/* 值非 null 时执行 */],
  "on_false": [/* 值为 null 时执行 */]
}
```

**不要用 `exists` 判断非 null**（见 PITFALLS 坑 9）：
- `exists(path)` = 路径存在（含 null 值）
- `not(eq(path, null))` = 值非 null

### 4.5 浮点处理

evorule TCB 无 Float 变体（见 PITFALLS 坑 13）。浮点数以字符串透传：

```json
// 外部服务返回
{"residual": 0.000486, "joint_positions": [0.468, -2.227]}

// evorule payload 中（自动转字符串）
{"residual": "0.000486", "joint_positions": ["0.468", "-2.227"]}
```

**设计原则**：浮点比较和运算放在外部服务中，evorule 只处理布尔结果：

```
外部服务：residual < tolerance → converged_ok: true/false
evorule 规则：eq(payload.service_result.converged_ok, true)
```

### 4.6 domain 类型完整参考

evorule-tcb 只支持 **6 种** domain 类型（`domain.rs:evaluate_domain_inner`）：

| 类型 | 参数 | 语义 | 限制 |
|------|------|------|------|
| `eq` | `path`, `value` | 路径值 == 目标值 | 支持任意 JsonValue（Integer/String/Bool/Null/Object/Array） |
| `lt` | `path`, `value` | 路径值 < 目标值 | **只支持 Integer**（`as_i64`），浮点/字符串返回 false |
| `exists` | `path` | 路径存在 | 含 null 值（路径存在但值为 null 也返回 true） |
| `instruction` | `instruction_type` | 匹配当前指令的 type | 用于 transform 规则的条件匹配 |
| `all` | `inner`（数组） | 所有子域为真 | 空列表 = true；AND 语义 |
| `not` | `inner`（单个） | 子域取反 | NOT 语义 |

**没有的类型**：`or` / `gt` / `gte` / `lte` / `neq` / `any`

**`or` 的实现**（德摩根定律 `or(A,B) = not(all(not(A), not(B)))`）：

```json
{
  "type": "not",
  "inner": {
    "type": "all",
    "inner": [
      { "type": "not", "inner": A },
      { "type": "not", "inner": B }
    ]
  }
}
```

实际示例（`or(diff_exceeded, has_alert)`，见 PITFALLS 坑 16）：

```json
{
  "type": "not",
  "inner": {
    "type": "all",
    "inner": [
      { "type": "not", "inner": { "type": "eq", "path": "payload.shadow_result.diff_exceeded", "value": true } },
      { "type": "eq", "path": "payload.audit.pending_alert", "value": null }
    ]
  }
}
```

**浮点比较的限制**：

`lt` 只支持 Integer 比较（`as_i64`），浮点字符串如 `"0.001"` 会被拒绝返回 false。这是 Kani 形式化验证的约束——避免浮点不确定性破坏确定性保证。浮点比较必须外部化到 I/O 服务中（见 §4.5）。

**eq/lt value 不支持路径引用**：

`set` 的 value 通过 `resolve_path_or_literal` 解析（`__` 开头自动解析为路径引用），但 `eq`/`lt` 的 value 是**字面量比较**（直接取值，不做路径解析）。这意味着 `eq(path, "__exec__.xxx")` 会比较路径值和字符串 `"__exec__.xxx"`，永远 false。

需要"比较两个路径的值"时：
1. 用 `set+sub` 计算差值，`eq(diff, 0)` 判断（仅 Integer）
2. 或外部化到 I/O 服务返回布尔值（见 PITFALLS 坑 17）

### 4.7 set 的幂等性与收敛

`set` 指令对**相同值不产生 StateTransition**（值未变化时跳过）。这保证了含 I/O 两阶段的规则在反应器重新评估时不会无限循环：

```
第一次执行 on_true：
  set service_result = safe_fallback     → 值变化 → StateTransition
  set audit.rollback_triggered = true    → 值变化 → StateTransition
  io_request → __io_result__ 不存在 → 发起请求 → 反应器挂起

IoResponse 回写后反应器恢复，重新评估 transform 规则：
  set service_result = safe_fallback     → 值相同 → 无 StateTransition ✓
  set audit.rollback_triggered = true    → 值相同 → 无 StateTransition ✓
  branch exists(__io_result__) = true → 消费结果 → set audit.alert → 收敛 ✓
```

这个特性让 on_true 分支中可以同时包含 set 操作和嵌套 I/O 两阶段（branch + exists），**无需手动去重**——重复执行的 set 不会触发新的执行轮次，反应器最终收敛到 `stable`。

---

## 五、本地开发环境搭建

### 5.1 启动 evorule-server

```powershell
cargo run --bin evorule-server -- `
    --addr 127.0.0.1:18080 `
    --rules-dir .\rules `
    --service-registry .\service_registry.json `
    --wal-dir .\wal `
    --auto-verify `
    --allow-loopback
```

**关键标志**：
- `--allow-loopback`：放行 SSRF 防护，允许调用 127.0.0.1 上的本地服务（见 PITFALLS 坑 5）
- `--auto-verify`：审计链实时验证，便于开发期发现问题
- `--wal-dir`：启用 WAL 持久化（不指定则纯内存）

### 5.2 配置 service_registry.json

```json
{
  "services": {
    "my_service": {
      "url": "http://127.0.0.1:5100/endpoint",
      "method": "POST",
      "timeout_ms": 10000
    }
  }
}
```

### 5.3 启动外部服务

以 Python FastAPI 为例：

```python
from fastapi import FastAPI
from pydantic import BaseModel

app = FastAPI

class SolveRequest(BaseModel):
    target_pose: dict
    solver_type: str = "LMA"

@app.post("/solve")
async def solve(req: SolveRequest):
    # 业务逻辑
    return {"converged": True, "joint_positions": [0.1, 0.2], "residual": 0.001}

# 启动：uvicorn ik_solver_service:app --port 5101
```

### 5.4 验证集成

```bash
# 1. 健康检查
curl http://127.0.0.1:18080/api/health

# 2. 创建 session
curl -X POST http://127.0.0.1:18080/api/sessions

# 3. 提交含 io_request 的指令
curl -X POST http://127.0.0.1:18080/api/sessions/1/command \
  -H "Content-Type: application/json" \
  -d '{"instruction":{"type":"sequence","instructions":[
    {"type":"set","params":{"attr":"_args","operation":"set","value":{"target_pose":{"x":"0.5"}}}},
    {"type":"io_request","params":{"io_type":"call_service","service_name":"my_service","args":"__exec__.payload._args"}}
  ]}}'

# 4. 查询结果
curl http://127.0.0.1:18080/api/sessions/1/state

# 5. 查看审计链
curl http://127.0.0.1:18080/api/sessions/1/audit
```

### 5.5 排查 I/O 超时

如果 IoRequest 60 秒超时，按顺序检查：

1. **server 日志**有没有 `IoSubscriber 已为 session 启动`？（没有 = 坑 1）
2. **service_registry.json** 路径对不对？服务名拼写对不对？
3. **--allow-loopback** 加了没？（没加 = 坑 5）
4. **外部服务** curl 能直接访问吗？
5. **__io_result__** 出现在 payload 了吗？（没出现 = IoSubscriber 没回写）

---

## 六、规则包（Bundles）API (0.3.0 新增)

规则包是一组相关规则的集合，支持原子导入、版本管理和回滚。规则包以目录形式存在，包含 `bundle_manifest.json`（元数据）和多条规则 JSON 文件。

### 6.1 导入规则包

```bash
POST /api/bundles
Content-Type: multipart/form-data
file=@bundle-ds-yuanze-01-v3.zip
```

或直接指定目录路径（本地开发）：

```bash
POST /api/bundles
Content-Type: application/json
{"path": "/path/to/bundle-ds-yuanze-01-v3"}
```

导入流程：
1. 校验 `bundle_manifest.json` 格式和必填字段
2. 逐条校验规则文件通过 `core/rule_schema` JSON Schema 门禁
3. 原子落盘（`land_bundle_atomically`）：全部成功或全部回滚
4. 检测同数据集的陈旧目录，自动清理

```json
{"imported": true, "bundle_id": "bundle-ds-yuanze-01-v3", "rule_count": 15, "status": "ok"}
```

### 6.2 列出活跃规则包

```bash
GET /api/bundles
```

```json
{
  "bundles": [
    {"id": "bundle-ds-yuanze-01-v3", "name": "原子规则演示包 v3", "version": "1.0.0", "rule_count": 15, "imported_at": "2026-08-26T10:00:00Z"},
    {"id": "b_guard_shell_risky", "name": "Shell 风险防护", "version": "1.0.0", "rule_count": 1, "imported_at": "2026-08-26T11:00:00Z"}
  ]
}
```

### 6.3 查看导入历史

```bash
GET /api/bundles/imports
```

返回所有导入记录，含成功/失败状态、失败原因、回滚记录。

### 6.4 规则包目录结构

```
bundle-ds-yuanze-01-v3/
├── bundle_manifest.json      # 元数据（id/name/version/author/description）
├── rule-audit-alert.json     # 规则文件
├── rule-audit-compactor.json
├── rule-compute-ik.json
└── ... (最多 64 条规则，受 MAX_TRANSFORM_RULES 限制)
```

> **注意**: 规则包导入使用 `evorule-bundle` crate 的 6 项校验链 + 逐条 Schema 门禁 + 原子落盘机制（36 号集成契约）。任何一条规则校验失败，整个包导入回滚，不会部分生效。

---

## 七、权限（Permissions）API (0.3.0 新增)

权限 API 基于 `evorule-governance` 的 `permission` 模块，提供机制层权限原语。具体权限策略由应用层注入。

### 7.1 权限模型

- **PermissionTable**: 权限表，存储所有权限条目
- **PermissionEntry**: 单条权限（subject/resource/action/effect）
- **Verdict**: 判定结果（Allow/Deny/NotApplicable）
- **ConditionEvaluator**: 条件评估器
- **DefaultPolicy**: 默认策略（Allow/Deny）

### 7.2 查询权限

```bash
GET /api/permissions?subject=user:alice&resource=session:1&action=read
```

```json
{"verdict": "Allow", "matched_rule": "rule-123", "reason": "用户是 session 所有者"}
```

### 7.3 管理权限条目

```bash
# 列出所有权限条目
GET /api/permissions/entries

# 新增权限条目
POST /api/permissions/entries
{"subject": "user:bob", "resource": "session:*", "action": "read", "effect": "Allow"}

# 删除权限条目
DELETE /api/permissions/entries/{entry_id}
```

> **注意**: 权限 API 是机制层原语，不包含具体业务角色定义。应用层（如 evorule-console）负责将业务角色（admin/editor/viewer）映射为具体的 PermissionEntry。

---

## 八、插件清单（部署期启用/裁剪）

集成方/部署方可通过插件清单声明进程内原生插件的启用集（白标部署、最小化部署面场景），**改清单 + 重启即生效**：

```bash
# 全量启用（缺省，不传 --plugins 即可，存量零迁移）
evorule-server --addr 0.0.0.0:18080

# 子集启用：只暴露 demo 的 config_persist + physics 的 physics_energy + indicator 的 indicator_sma
echo '{ "plugins": { "demo-services": { "enabled": true, "services": ["config_persist"] }, "physics-services": { "enabled": true, "services": ["physics_energy"] }, "indicator-services": { "enabled": true, "services": ["indicator_sma"] } } }' > plugin_manifest.json
evorule-server --addr 0.0.0.0:18080 --plugins plugin_manifest.json

# 全部停用：call_service/call_external 直连 HTTP 注册表
echo '{ "plugins": { "demo-services": { "enabled": false }, "physics-services": { "enabled": false }, "indicator-services": { "enabled": false } } }' > plugin_manifest.json
```

要点：

1. **校验 fail-fast**：未知名/重复名/空集/未知插件 id 均启动期报错退出（含合法服务名与自诊断指引），不静默忽略。
2. **回落语义**：未启用的服务名沿插件挂载链（声明序）逐层回落，链尾直连 `--service-registry` HTTP 注册表，与进程外服务同路径——已发布规则不受裁剪影响，只是执行路径从原生变为 HTTP（如实报错 `unknown service_name` 当注册表也未配置时）。
3. **可见性**：`GET /api/health` 的 `plugins` 节按插件 id 逐一呈现实际挂载服务名集，供运维探活/对账。
4. **进程外能力不走本清单**：一律经 `service_registry.json` 声明接入（见 §1.3）。

完整语义与新增原生插件/服务指引见 [README「插件清单」](../README.md#插件清单)、[plugins/demo-services/README.md](../plugins/demo-services/README.md)、[plugins/physics-services/README.md](../plugins/physics-services/README.md) 与 [plugins/indicator-services/README.md](../plugins/indicator-services/README.md)。

---

## 参考

- [README.md](../README.md) — 架构概览、配置参数、API 简表
- [PITFALLS.md](PITFALLS.md) — 15 个踩坑记录与避坑指南
- [CONTRIBUTING.md](../CONTRIBUTING.md) — 贡献指南与架构原则
- 源码 `evorule-server/src/api/server.rs` — 约 50 条路由的完整定义
- 源码 `core/io_handlers/src/` — HttpHandler / ServiceRegistryHandler / DbHandler / MemoryHandler 实现

---

_本文档由 2026-07-31 的 yuanze-demos 集成工作总结而成。_

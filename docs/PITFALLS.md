<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: CC-BY-4.0

  This documentation is licensed under Creative Commons Attribution 4.0 International.
-->

# evorule-server 踩坑记录与避坑指南

> **本文档记录在实际集成 evorule-server 时踩过的所有坑**，供后续开发者参考。
>
> 每个坑包含：触发场景 → 现象 → 根因 → 修复方案 → 避坑要点。
>
> 文档生成日期：2026-07-31　|　对应版本：evorule-server 0.1.0

---

## 快速避坑清单（TL;DR）

给赶时间的开发者的一页纸速查：

| #   | 坑                                 | 一句话避坑                                                                                               |
| --- | ---------------------------------- | -------------------------------------------------------------------------------------------------------- |
| 1   | SessionApi 未注入 dispatcher       | `SessionApi::new(...).with_dispatcher(dispatcher.clone())` 必须调用                                      |
| 2   | 多 session 的 IoSubscriber         | 每个 session 创建时都要 spawn 自己的 IoSubscriber                                                        |
| 3   | IoDispatcher 不可 Clone            | `#[derive(Clone)]`（内部全是 Arc，天然可共享）                                                           |
| 4   | ServiceRegistry 返回字符串         | `call_service` 的响应需手动 `serde_json::from_str` 解析                                                  |
| 5   | SSRF 阻止 loopback                 | 本地开发加 `--allow-loopback`，生产**永远不要**                                                          |
| 6   | Content-Encoding 语义冲突          | `application/gzip` 实体**不要**再设 `Content-Encoding: gzip`                                             |
| 7   | io_request 参数未包装              | 先用 `set` 在 payload 构造 args 对象，再 `args` 引用                                                     |
| 8   | 规则 path 缺前缀                   | domain path 要写 `payload.xxx`，不是 `xxx`                                                               |
| 9   | exists ≠ 非 null                   | 判断非 null 用 `not(eq(path, null))`，不要用 `exists`                                                    |
| 10  | 命令格式 422                       | 请求体是 `{"instruction": {...}}`，不是裸指令                                                            |
| 11  | 审计端点混淆                       | session 模式用 `/api/sessions/{id}/audit`，不是 `/api/audit`                                             |
| 12  | payload 嵌套层                     | 业务数据在 `state["payload"]` 下，不是 state 顶层                                                        |
| 13  | TCB 无 Float                       | 浮点以字符串透传，比较时需在外部服务处理                                                                 |
| 14  | PowerShell GBK 乱码                | 用 `pwsh` 或 `chcp 65001`，复杂脚本写 `.ps1` 用 `-File`                                                  |
| 15  | PowerShell 命令长度                | 长 Python 命令用脚本文件，不用 `python -c`                                                               |
| 16  | or/gt domain 不存在                | evorule 只有 6 种 domain；`or` 用德摩根定律；`gt` 无法实现，`lt` 只支持 Integer                          |
| 17  | eq value 不支持路径引用            | `set` 的 value 用 `resolve_path_or_literal` 解析路径；`eq`/`lt` 的 value 是字面量，不做路径解析          |
| 18  | branch+exists 不清除 **io_result** | 同 session 多次提交同一类型 I/O 指令会消费旧结果；需每次创建新 session                                   |
| 19  | io_request 之后的指令不执行        | io_request 返回 IoRequired 信号后立即传播，后续指令不执行；需用 I/O 两阶段模式包装                       |
| 20  | set 业务指令 operation 被忽略      | core_eval 硬编码 operation='set'，add/sub 被静默忽略；用 increment/decrement 指令代替                    |
| 21  | set value 引用路径不存在           | transform 规则 set value 引用 `__exec__.payload.X`，X 不存在时 transition 静默回滚；确保前置指令已设置 X |

---

## 一、I/O 处理（5 个坑）

### 坑 1：SessionApi 未注入 dispatcher → session I/O 60s 超时

**触发场景**：session 模式下提交含 `io_request` 的指令。

**现象**：指令提交后反应器卡住，60 秒后 IoRequest 超时，payload 中 `__io_result__` 始终不出现。

**根因**：`main.rs` 中创建了 `IoDispatcher` 并用于启动全局 `IoSubscriber`，但构造 `SessionApi` 时**没有调用 `.with_dispatcher()`**。`SessionApi` 的 `dispatcher` 字段保持 `None`，导致 `create_session` handler 中不会为新 session spawn IoSubscriber，session 的 IoRequest 无人处理。

**修复**：

```rust
// main.rs — 关键：clone 一份给 SessionApi
let session_dispatcher = dispatcher.clone();  // IoDispatcher 已 derive(Clone)

let session_api = SessionApi::new_with_full_config(...)
    .with_dispatcher(session_dispatcher);  // ← 必须调用
```

**避坑要点**：`dispatcher` 被 `IoSubscriber::new(dispatcher)` move 走后，SessionApi 就拿不到了。务必在 move 之前 clone，或让 dispatcher 通过 `Arc` 共享。`SessionApi.dispatcher` 为 `None` 时 session 的 IoRequest 会静默超时——**没有 warning 日志**，极难排查。

---

### 坑 2：多 session 模式下 IoSubscriber 需要 per-session spawn

**触发场景**：多个并发 session 同时提交含 I/O 的指令。

**现象**：只有第一个 session 的 I/O 能正常处理，后续 session 的 IoRequest 超时。

**根因**：`IoSubscriber` 绑定单个 session 的 `event_rx`（`tokio::broadcast::Receiver`），不是全局的。一个 IoSubscriber 只能服务一个 session。最初以为全局 spawn 一个 IoSubscriber 就够了，实际上每个 session 有独立的 event 通道。

**修复**：在 `create_session` handler 中为**每个新 session** spawn 独立的 IoSubscriber：

```rust
// server.rs create_session handler
if let Some(ref dispatcher) = api.dispatcher {
    let sessions = api.sessions.lock().await;
    if let Some(session) = sessions.get_session(id) {
        let event_rx = session.event_tx.subscribe();
        let command_tx = session.command_tx.clone();
        let subscriber = IoSubscriber::new(dispatcher.clone())
            .with_metrics(metrics.clone());
        tokio::spawn(async move {
            if let Err(e) = subscriber.run(event_rx, command_tx).await {
                tracing::error!(session_id = id, error = %e, "IoSubscriber 异常退出");
            }
        });
    }
}
```

**避坑要点**：`IoSubscriber` 是 **per-session** 的，不是全局单例。session 销毁时其 IoSubscriber 会因 event_rx 关闭而自动退出。

---

### 坑 3：IoDispatcher 需要 derive(Clone)

**触发场景**：坑 1 和坑 2 的前置条件——多个 session 需要共享同一个 dispatcher。

**现象**：`dispatcher.clone()` 编译失败，`IoDispatcher` 没有 `Clone` 实现。

**根因**：`IoDispatcher` struct 未标注 `#[derive(Clone)]`。

**修复**：

```rust
// evorule-governance/src/io_dispatcher.rs
#[derive(Clone)]  // ← 添加
pub struct IoDispatcher {
    handlers: HashMap<IoType, Arc<dyn IoHandler>>,
    // Arc<dyn IoHandler> 天然可 Clone，所以 derive 安全
}
```

**避坑要点**：`IoDispatcher` 的设计意图就是**共享不可变**——handler 注册后不再修改，多个 session 的 IoSubscriber 各持一份 clone 共享底层 handler。`Arc` 内部共享，clone 开销极低。

---

### 坑 4：ServiceRegistryHandler 不解析 JSON 响应

**触发场景**：通过 `io_request` 的 `call_service` 类型调用外部 HTTP 服务，服务返回 JSON。

**现象**：`__io_result__` 是 HTTP 响应体的**原始字符串**，而非结构化 JSON。规则中取 `__io_result__.joint_positions` 取不到值，因为它是字符串不是对象。

**根因**：`service_registry.rs` 的 `execute` 方法直接返回 `HttpHandler` 的 `JsonValue::String`（HTTP 响应体文本），没有尝试解析 JSON。

**修复**：在 `execute` 中对 HTTP 响应体尝试 `serde_json::from_str`，若顶层为 Object/Array 则递归转换为 evorule 的 `JsonValue`：

```rust
// service_registry.rs execute 方法
let raw = http_handler.execute(&http_params).await?;
// 尝试解析 JSON
if let JsonValue::String(s) = &raw {
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s) {
        return Ok(serde_to_json_value(parsed));
    }
}
Ok(raw)
```

新增 `serde_to_json_value` 递归转换函数。**注意**：`evorule-tcb` 的 `JsonValue` **没有 Float 变体**（见坑 13），浮点数需转为字符串。

**避坑要点**：`HttpHandler` 只负责发请求返回字符串，**不关心响应体格式**。上层 handler（如 ServiceRegistry）需要根据自己的语义解析响应。如果直接用 `io_type: "http"` 调 HttpHandler，拿到的也是字符串。

---

### 坑 18：branch+exists 不清除 **io_result**，同 session 多次提交消费旧结果

**触发场景**：同一 session 中多次提交同一类型的 I/O 指令（如多次 `sampling_decider`），使用 `branch + exists(__io_result__)` 模式手动实现 I/O 两阶段。

**现象**：第二次提交同一类型指令时，`exists(__io_result__)` 仍为 true（上次的结果残留），直接走 on_true 消费旧结果，**不会发起新的 I/O 请求**。外部服务未被调用，payload 中的结果停留在上次的值。

**根因**：evorule 有两种 I/O 两阶段模式，对 `__io_result__` 的清除行为不同：

| 模式                       | `__io_result__` 清除 | 后续指令引用结果    | 同 session 多次提交 |
| -------------------------- | -------------------- | ------------------- | ------------------- |
| `io_request` 自动两阶段    | ✅ 消费后自动清除    | ❌ 清除后无法引用   | ✅ 每次发起新请求   |
| `branch+exists` 手动两阶段 | ❌ 不清除            | ✅ on_true 中可引用 | ❌ 第二次消费旧结果 |

`branch+exists` 模式用 `exists(__io_result__)` 判断 I/O 状态：

- 不存在 → on_false → io_request 发起请求
- 存在 → on_true → set 转存结果

但 on_true 中**没有清除 `__io_result__`**——evorule 没有 delete 操作，`set __io_result__ = null` 不会让 `exists` 返回 false（见坑 9：exists 对 null 值返回 true）。下次新指令提交时，`__io_result__` 仍在，直接走 on_true 消费旧结果。

**修复方案**：

1. **每次创建新 session**（推荐）：每个 session 有独立 payload，`__io_result__` 不跨 session 残留。外部服务的状态（如计数器）全局共享。

   ```python
   for i in range(6):
       sid = client.create_session()
       client.send_command(sid, {"type": "sampling_decider", ...})
       # __io_result__ 在新 session 中不存在，会发起新 I/O
   ```

2. **用 `io_request` 自动两阶段**（如果不需要在后续指令中引用 I/O 结果）：io_request 消费后自动清除 `__io_result__`，下次指令提交时会发起新请求。但消费后结果不再可用。

**避坑要点**：

- `branch+exists` 模式适合"每 session 单次 I/O"的场景（如 compute_ik、shadow_validate）
- 需要"同 session 多次提交同一类型 I/O 指令"时，**每次创建新 session**
- 两种模式的选择：需要引用结果 → `branch+exists`（每 session 单次）；不需要引用 → `io_request` 自动（可多次）
- `set __io_result__ = null` **不能**清除 `__io_result__`（exists 对 null 返回 true）

---

### 坑 19：io_request 之后的指令不会执行（需用 I/O 两阶段模式包装）

**触发场景**：规则中 `io_request` 之后直接写 `set` / `save_memory` 等指令，期望它们在 io_request 完成后执行。

**现象**：io_request 之后的指令**永远不会执行**——payload 中看不到预期的状态变更，审计日志中也没有记录。

**根因**：`io_request` 的语义是"信号传播"——它返回 `MetaInstructionResult::IoRequired`，执行器收到这个信号后**立即停止执行当前指令列表**，将信号传播给反应器。后续指令不会在同一次提交中执行。

```rust
// executor.rs exec_branch — 遇到 IoRequired 立即返回
match execute_meta_instruction(instr, state, depth + 1) {
    Ok(MetaInstructionResult::IoRequired { io_type, params }) => {
        // 立即返回 IoRequired，不执行后续指令
        return Ok(MetaInstructionResult::IoRequired { io_type, params });
    }
    Ok(MetaInstructionResult::State(s)) => state = s,  // 只有 State 才继续
    Err(e) => return Err(e),
}
```

**错误示例**（014.json 原始版本）：

```json
{
  "type": "branch",
  "params": {
    "domain": {
      "type": "eq",
      "path": "payload.audit.sandbox_verdict.passed",
      "value": true
    },
    "on_true": [
      {
        "type": "io_request",
        "params": {
          "io_type": "call_service",
          "service_name": "config_persist",
          "args": "..."
        }
      },
      {
        "type": "save_memory",
        "params": { "key": "audit.evolution_history", "value": "..." }
      }
    ]
  }
}
```

`save_memory`（以及任何 io_request 之后的指令）永远不会执行。

**修复**：用 I/O 两阶段模式包装——先检查 `__io_result__` 是否存在，存在则消费结果并执行后续操作，不存在则发起 io_request：

```json
{
  "type": "branch",
  "params": {
    "domain": { "type": "exists", "path": "__exec__.payload.__io_result__" },
    "on_true": [
      // I/O 完成后的操作放这里（第二次提交时执行）
      {
        "type": "set",
        "params": {
          "attr": "audit.evolution_history.last_patch",
          "operation": "set",
          "value": "__exec__.payload.audit.generated_patch_raw"
        }
      }
    ],
    "on_false": [
      // io_request 必须是 on_false 的最后一条指令（第一次提交时执行）
      {
        "type": "io_request",
        "params": {
          "io_type": "call_service",
          "service_name": "config_persist",
          "args": "..."
        }
      }
    ]
  }
}
```

**避坑要点**：

- `io_request` 必须是其所在指令列表的**最后一条指令**——之后的指令不会执行
- 需要 io_request 之后执行的操作，放在 `on_true` 分支（`__io_result__` 存在时执行）
- 这是 I/O 两阶段模式的核心设计：第一次提交发起请求，第二次提交消费结果
- 与坑 18 配合理解：两阶段模式 + 每 session 单次 I/O = 安全的 I/O 处理

---

## 二、HTTP/网络（2 个坑）

### 坑 5：SSRF 防护阻止 loopback 地址

**触发场景**：本地开发时，规则通过 `io_request` 调用同机 `127.0.0.1` 上的 FastAPI / mock 服务。

**现象**：IoRequest 返回错误 `SSRF blocked: loopback address 127.0.0.1 is not allowed`。

**根因**：`HttpHandler` 内置 SSRF 防护，默认拒绝 loopback（127.0.0.0/8, ::1）和私有 IP（10.x, 172.16-31.x, 192.168.x）。这是生产安全要求，但阻碍本地开发。

**修复**：新增 `--allow-loopback` CLI 标志和 `new_dev_allow_loopback()` 构造函数：

```rust
// http_handler.rs
pub fn new_dev_allow_loopback() -> Self {
    // ... 同 new()，但 allow_loopback: true
}

// main.rs
let http = Arc::new(if cfg.allow_loopback {
    warn!("🔓 --allow-loopback 已启用：SSRF 防护放行 loopback（仅限本地开发！）");
    HttpHandler::new_dev_allow_loopback()
} else {
    HttpHandler::new()
});
```

**避坑要点**：

- `--allow-loopback` **仅限本地开发**，生产环境**永远不要**启用
- SSRF 防护是 P0 安全要求（见 `docs/security/`），放行 loopback 意味着规则可以访问内网服务
- 测试用 `#[cfg(test)]` 的 `new_for_tests()` 不受此影响

---

### 坑 6：Content-Encoding + Content-Type 语义冲突

**触发场景**：调用 `GET /api/sessions/{id}/audit/export/compressed` 导出 gzip 压缩审计链。

**现象**：客户端拿到的 `compressed` 数据 magic bytes 是 `7b0a2020`（`{\n  `，JSON 开头），而非 gzip magic `1f8b`。压缩比 118.7%（比原始 JSON 还大）。用此数据调 `import/compressed` 返回 400 Bad Request。

**根因**：`session_audit_export_compressed` handler 同时设置了两个语义冲突的 HTTP 头：

```rust
// 错误配置
(header::CONTENT_TYPE, "application/gzip"),      // 实体是 gzip 文件
(header::CONTENT_ENCODING, "gzip"),               // 传输编码是 gzip ← 问题所在
```

`Content-Encoding: gzip` 表示**响应体被 gzip 传输编码压缩**，HTTP 客户端（httpx, reqwest, curl 等）会**自动解压**。于是客户端拿到的已经是解压后的 JSON，再当 gzip 导入就失败。

`Content-Type: application/gzip` 表示**实体本身是 gzip 文件**，客户端不应自动解压。两者语义冲突。

**修复**：移除 `Content-Encoding` 头，只保留 `Content-Type: application/gzip` + `Content-Disposition`：

```rust
// 正确配置
Ok((
    StatusCode::OK,
    [
        (header::CONTENT_TYPE, "application/gzip"),
        (
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"audit_chain.json.gz\"",
        ),
        // 不设 Content-Encoding —— 实体是 gzip 文件，不是传输编码
    ],
    compressed,
).into_response())
```

修复后压缩比从 118.7% 恢复到正常的 **42.6%**（3043 bytes JSON → 1296 bytes gzip）。

**避坑要点**：

- `Content-Type: application/gzip` = 实体是 gzip 文件（如下载 .gz 文件）
- `Content-Encoding: gzip` = 传输层压缩（客户端自动解压）
- **两者不能同时用于 gzip 实体**。导出 gzip 文件用前者，压缩传输用后者
- 排查方法：检查响应 magic bytes，gzip 应以 `1f 8b` 开头

---

## 三、规则编写（5 个坑）

### 坑 7：io_request 参数需要 args 字段包装

**触发场景**：规则中用 `io_request` 的 `call_service` 类型调用外部服务，需要传递复杂参数。

**现象**：HTTP 请求体格式不对，外部服务收不到参数或返回 422。

**根因**：`ServiceRegistryHandler` 期望 `params.args` 字段作为请求体，但规则直接在 `io_request.params` 里写参数字段，没有包装成 `args` 对象。

**修复**：先用 `set` 指令在 payload 中构造参数对象，再通过 `args` 字段引用：

```json
// 错误：直接写参数
{
  "type": "io_request",
  "params": {
    "io_type": "call_service",
    "service_name": "ik_solver",
    "target_pose": {"x": 0.5}  // ← 不会被当作 HTTP body
  }
}

// 正确：先 set 构造 args 对象，再引用
// 第一步：set 指令构造 _ik_args
{"type": "set", "params": {"attr": "_ik_args", "operation": "set", "value": {
    "target_pose": {"x": "0.5", "y": "0.3", "z": "0.2"},
    "solver_type": "LMA"
}}}
// 第二步：io_request 引用 args
{
  "type": "io_request",
  "params": {
    "io_type": "call_service",
    "service_name": "ik_solver",
    "args": "__exec__.payload._ik_args"  // ← 引用 payload 中的对象
  }
}
```

**避坑要点**：`io_request` 的 `call_service` 类型有固定参数结构：`io_type` / `service_name` / `args`。业务参数必须放在 `args` 里（可以是 payload 路径引用或内联对象），不能直接铺在 params 顶层。

---

### 坑 8：规则 path 缺少 payload. 前缀

**触发场景**：规则中用 domain 类型（如 `eq` / `lt` / `exists`）判断 payload 中的字段。

**现象**：`eq(service_result.converged_ok, true)` 永远返回 false，即使 payload 中 `service_result.converged_ok` 确实是 true。

**根因**：domain 的 `path` 在 `__exec__` 上下文下查找，而业务数据在 `__exec__.payload` 下。写 `service_result.converged_ok` 实际查找 `__exec__.service_result.converged_ok`（不存在），而非 `__exec__.payload.service_result.converged_ok`。

**修复**：path 必须带 `payload.` 前缀：

```json
// 错误
{"type": "eq", "path": "service_result.converged_ok", "value": true}

// 正确
{"type": "eq", "path": "payload.service_result.converged_ok", "value": true}
```

**避坑要点**：domain path 的根是 `__exec__`，业务数据在 `__exec__.payload` 下，所以所有业务字段路径都要以 `payload.` 开头。`io_request` 的 `service_name` / `args` 引用也同理，要用 `__exec__.payload.xxx` 或 `__exec__.instruction.params.xxx`。

> ⚠️ **隐蔽性**：这个坑曾经导致 `test_precision_failure` "假通过"——path 解析失败 → eq 返回 false → 恰好走了 on_false 分支 → 测试断言碰巧匹配。修复 path 后才发现之前的"通过"是巧合。

---

### 坑 9：conditional exists ≠ 非 null 判断

**触发场景**：规则中用 `conditional` 判断某个字段是否"有值"（非 null）。

**现象**：`pending_alert = null` 时，`exists(path="pending_alert")` 仍返回 true，走了 then 分支误触发告警。

**根因**：`evaluate_exists` 的语义是**"路径存在"**（包括值为 `Null` 的情况），不是**"值非 null"**。`set` 指令把 `pending_alert` 设为 `null` 后，路径仍然存在，只是值为 null。

**修复**：判断"值非 null"用 `not(eq(path, null))`，不要用 `exists`：

```json
// 错误：exists 对 null 值返回 true
{
  "type": "conditional",
  "condition": {"type": "exists", "path": "payload.pending_alert"}
}

// 正确：not + eq 判断非 null
{
  "type": "conditional",
  "condition": {
    "type": "not",
    "inner": {"type": "eq", "path": "payload.pending_alert", "value": null}
  }
}
```

**避坑要点**：

- `exists(path)` = 路径存在（含 null 值）→ 适合判断"字段是否被设置过"
- `not(eq(path, null))` = 值非 null → 适合判断"是否有有效值"
- 两者**不等价**。设计 conditional 条件时要明确区分

---

### 坑 16：`or` / `gt` domain 不存在，`lt` 只支持 Integer

**触发场景**：规则中需要"或"条件判断，或比较两个数值大小（如 `diff > threshold`）。

**现象**：规则中使用 `{"type": "or", ...}` 或 `{"type": "gt", ...}` 时，domain 评估返回 false（未知类型被忽略），条件永远不满足，on_true 分支永远不执行。

**根因**：evorule-tcb 的 `evaluate_domain_inner`（`domain.rs:170`）只识别 **6 种** domain 类型：

| 类型          | 语义             | 限制                                                    |
| ------------- | ---------------- | ------------------------------------------------------- |
| `eq`          | 路径值 == 目标值 | 支持任意 JsonValue                                      |
| `lt`          | 路径值 < 目标值  | **只支持 Integer**（`as_i64()` 转换失败直接返回 false） |
| `exists`      | 路径存在         | 含 null 值                                              |
| `instruction` | 指令类型匹配     | —                                                       |
| `all`         | AND 组合         | `inner` 为数组，空列表 = true                           |
| `not`         | 取反             | `inner` 为单个子域                                      |

没有 `or` / `gt` / `gte` / `lte` / `neq` / `any`。且 `lt` 只比较 Integer（`as_i64()`），浮点字符串如 `"0.001"` 会被拒绝，返回 false。

**修复方案**：

1. **`or(A, B)` 用德摩根定律实现**：

   ```
   or(A, B) = not(all(not(A), not(B)))
   ```

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

   实际示例（`or(diff_exceeded, has_alert)`）：

   ```json
   {
     "type": "not",
     "inner": {
       "type": "all",
       "inner": [
         {
           "type": "not",
           "inner": {
             "type": "eq",
             "path": "payload.shadow_result.diff_exceeded",
             "value": true
           }
         },
         { "type": "eq", "path": "payload.audit.pending_alert", "value": null }
       ]
     }
   }
   ```

2. **`gt(a, b)` 浮点比较无法实现** — `lt` 只支持 Integer。浮点比较必须**外部化到 I/O 服务**中，返回布尔值：

   ```
   外部服务：float(diff_percent) > float(max_diff) → diff_exceeded: true/false
   evorule 规则：eq(payload.shadow_result.diff_exceeded, true)
   ```

3. **`gt(a, b)` Integer 比较**可用 `not(lt(a, b))` 且 `not(eq(a, b))` 组合实现（仅 Integer）。

**避坑要点**：

- evorule 的 domain 类型**故意保持极简**（6 种），复杂条件用 `all` + `not` 组合
- `lt` 的 Integer-only 限制是 Kani 形式化验证的约束（避免浮点不确定性破坏确定性保证）
- 浮点比较**永远不要**在 evorule 内做，外部化到 I/O 服务返回布尔值
- 完整 domain 类型参考见 [INTEGRATION_GUIDE.md §4.6](INTEGRATION_GUIDE.md#46-domain-类型完整参考)

---

### 坑 17：eq 的 value 不支持路径引用（只有 set 的 value 支持）

**触发场景**：规则中需要比较两个路径的值，如 `eq(counter, instruction.params.threshold)` 判断计数器是否达到阈值。

**现象**：`eq` 条件永远返回 false，on_true 分支永远不执行。

**根因**：evorule-tcb 中 `set` 和 `eq`/`lt` 对 `value` 字段的处理方式不同：

| 指令  | value 处理方式                                      | 代码位置        | 路径引用  |
| ----- | --------------------------------------------------- | --------------- | --------- |
| `set` | `resolve_path_or_literal` — `__` 开头自动解析为路径 | executor.rs:204 | ✅ 支持   |
| `eq`  | `domain.get("value")` — 直接取值                    | domain.rs:191   | ❌ 不支持 |
| `lt`  | 同 eq — 直接取值                                    | domain.rs:223   | ❌ 不支持 |

`eq` 的 `actual == target`（domain.rs:215）会比较路径解析后的值（如 `Integer(3)`）和字符串字面量（如 `"__exec__.instruction.params.threshold"`），类型不匹配，永远返回 false。

**修复方案**：

evorule **无法在 domain 中直接比较两个路径的值**。可选方案：

1. **用 set+sub 计算差值，eq 判断差值为 0**（仅 Integer）：

   ```json
   { "type": "set", "params": { "attr": "_diff", "operation": "set", "value": "__exec__.instruction.params.threshold" } },
   { "type": "set", "params": { "attr": "_diff", "operation": "sub", "value": "__exec__.payload.audit.counter" } },
   { "type": "eq", "path": "payload._diff", "value": 0 }
   ```

   ⚠️ 注意：如果被引用的路径不存在，`resolve_path_or_literal` 返回 `PathResolutionFailed` 错误（不是默认 0）。需要先用 `exists` 判断路径是否存在。

2. **外部化到 I/O 服务**：把比较逻辑放在外部服务中，返回布尔值，evorule 用 `eq(result, true)` 判断。
   ```json
   // 外部服务：float(diff_percent) > float(max_diff) → diff_exceeded: true
   {
     "type": "eq",
     "path": "payload.shadow_result.diff_exceeded",
     "value": true
   }
   ```

**避坑要点**：

- `set` 的 value 支持路径引用（`resolve_path_or_literal`），`eq`/`lt` 的 value 是**字面量比较**
- 需要"比较两个路径的值"时，用 set+sub+eq(0) 或外部化到 I/O 服务
- `resolve_path_or_literal` 对不存在的路径返回 `PathResolutionFailed` 错误（executor.rs:142），不是默认值

---

## 四、客户端/集成（3 个坑）

### 坑 10：命令提交需要 {"instruction": {...}} 包装

**触发场景**：通过 HTTP API 向 session 提交指令。

**现象**：`POST /api/sessions/{id}/command` 返回 422 Unprocessable Entity。

**根因**：`CommandRequest` 的 serde 结构要求请求体是 `{"instruction": {...}}`，而客户端直接发送了裸指令 JSON `{"type": "set", ...}`。

**修复**：客户端提交时包装：

```python
# 错误
resp = http.post(f"{url}/api/sessions/{sid}/command",
                 json={"type": "sequence", "instructions": [...]})

# 正确
resp = http.post(f"{url}/api/sessions/{sid}/command",
                 json={"instruction": {"type": "sequence", "instructions": [...]}})
```

**避坑要点**：看 `CommandRequest` 的定义——`#[derive(Deserialize)] struct CommandRequest { instruction: JsonValue }`，字段名 `instruction` 是必需的。PayloadUpdate 类似需要 `{"path": "...", "value": ...}` 包装。

---

### 坑 11：session 审计 vs 全局审计端点混淆

**触发场景**：session 模式下查询审计报告。

**现象**：`GET /api/audit` 返回 `entries: []`（空），但 session 明明执行了多条指令、有审计记录。

**根因**：`/api/audit` 是**单反应器 GovernanceApi** 的全局审计器，与 session 无关。session 模式下每个会话有**独立的审计器**（独立哈希链），审计记录在 `/api/sessions/{id}/audit`。

**修复**：客户端区分两个端点：

```python
# 全局审计（单反应器模式，与 session 无关）
client.get_audit()  # → GET /api/audit

# 会话审计（session 模式，每会话独立哈希链）
client.get_session_audit(session_id)  # → GET /api/sessions/{id}/audit
```

**避坑要点**：session 模式下**所有操作都要带 session_id**。`/api/audit`、`/api/state`、`/api/command` 是单反应器遗留端点，session 模式下不要用。对应的 session 端点是 `/api/sessions/{id}/audit`、`/api/sessions/{id}/state`、`/api/sessions/{id}/command`。

---

### 坑 12：payload 在 state 的嵌套层

**触发场景**：客户端读取 `GET /api/sessions/{id}/state` 的返回值。

**现象**：`state["service_result"]` 取不到值，返回 None / KeyError。

**根因**：`GET /state` 返回的状态结构是 `{payload: {...}, version: N, ...}`，业务数据在 `state["payload"]` 下，不是 state 顶层。

**修复**：

```python
state = client.get_state(session_id)
# 错误
result = state["service_result"]
# 正确
result = state["payload"]["service_result"]
```

**避坑要点**：state 快照结构是 `{payload: {业务数据}, facts_log: [...], version: N}`。业务规则写入的字段都在 `payload` 下。`wait_for_field` 等轮询逻辑也要从 `payload` 下取字段。

---

## 五、TCB 类型系统（1 个坑）

### 坑 13：TCB 无 Float 变体，浮点以字符串透传

**触发场景**：外部服务返回含浮点数的 JSON（如 IK 求解的关节角、残差）。

**现象**：payload 中的浮点数都是字符串：

```json
{
  "joint_positions": ["0.46881175889550014", "-2.2275257894446243", ...],
  "residual": "0.00048668868628454367"
}
```

**根因**：`evorule-tcb` 的 `JsonValue` 枚举**没有 Float 变体**，只有 `Integer / String / Bool / Null / Object / Array`。外部 JSON 中的浮点数在转换为 `JsonValue` 时变成字符串。

**影响**：

- 规则中比较浮点要用字符串比较（仅支持相等比较）
- 数值比较（`lt` / `gt`）对字符串浮点无效
- 浮点运算必须在**外部服务**中完成，evorule 只做透传

**应对方案**：

- 精度判断（如 `residual < tolerance`）放在**外部服务**里做，返回 `converged_ok: true/false` 布尔值
- evorule 规则只判断布尔结果，不直接比较浮点
- 需要保留精度时，外部服务返回字符串，evorule 透传

**避坑要点**：这是 TCB 的**有意设计**——浮点数不确定性会破坏确定性执行保证。不要试图在 evorule 核心加 Float 变体（会违背 TCB 原语扩展谨慎原则）。业务层的浮点处理放在外部 I/O 服务中。

---

## 六、环境/工具链（2 个坑）

### 坑 14：PowerShell 5.1 + 无 BOM UTF-8 = GBK 误读

**触发场景**：Windows PowerShell 5.1 环境下处理含中文的文件或日志。

**现象**：中文乱码，或 `Get-Content -Raw` 读取 UTF-8 文件时中文变乱码。

**根因**：PowerShell 5.1 默认编码是 GBK（Windows-1252），无 BOM 的 UTF-8 文件会被当作 GBK 解码。

**修复**：

- 用 `pwsh`（PowerShell 7+）替代 `powershell`，默认 UTF-8
- 或先 `chcp 65001` 切换代码页
- 加 SPDX 头用 `add-spdx-safe.ps1`，不要用 `Get-Content -Raw`
- 服务器中文日志乱码也是此因

**避坑要点**：CI 脚本和本地开发统一用 `pwsh`。

---

### 坑 15：PowerShell 命令长度限制

**触发场景**：PowerShell 中执行较长的 `python -c "..."` 命令。

**现象**：命令执行失败，报错 `Command too long (32064 chars after encoding, limit: 32000)`。

**根因**：PowerShell 环境变量前缀（PATH 等）+ 命令本身总长度超过 32000 字符编码限制。内联 Python 命令稍长就会触限。

**修复**：长命令写成临时 `.py` 脚本文件执行：

```powershell
# 错误：内联命令太长
python -c "import requests; r=requests.get('...'); print(...)"

# 正确：写脚本文件
python tmp_query.py
```

**避坑要点**：复杂脚本写 `.ps1` 文件再用 `-File` 执行。Python 同理——超过 3-4 行的命令一律写脚本文件。

---

## 七、core_eval 限制与规则依赖（2 个坑）

### 坑 20：core_eval 的 set 业务指令处理器硬编码 operation='set'，忽略 instruction.params.operation

**触发场景**：通过 API 或 sequence/conditional/while_loop body 提交 `set` 业务指令，`params` 中带 `operation: "add"` 或 `"sub"`，期望累加或递减。

**现象**：多次 `set(operation=add, value=1)` 结果始终为 1（最后一次覆盖），而非累加。**无错误、无 warning**，指令正常执行但语义错误。

**根因**：`core_eval.json` 的 `set` 处理器（transform 规则）硬编码 `operation: "set"`，未用 `"__exec__.instruction.params.operation"` 透传：

```json
// core_eval.json set 处理器（简化）
{
  "type": "branch",
  "params": {
    "domain": { "type": "instruction", "instruction_type": "set" },
    "on_true": [
      {
        "type": "set",
        "params": {
          "attr": "__exec__.instruction.params.attr",
          "operation": "set", // ← 硬编码！未透传 instruction.params.operation
          "value": "__exec__.instruction.params.value"
        }
      }
    ]
  }
}
```

无论业务指令的 `operation` 是什么，core_eval 总是执行 `set`（覆盖）操作。

**关键区分**：

- **业务 `set` 指令**（经 core_eval 处理）：`operation` 被忽略，总是覆盖 ❌
- **meta-instruction `set`**（transform 规则中 `on_true`/`on_false` 的直接子节点）：`operation` 有效，`exec_set` 直接执行 `add`/`sub` ✅

**修复方案**：用 `increment`/`decrement` 指令代替 `set(operation=add/sub)`：

```python
# 错误：operation 被忽略，3 次结果为 1
client.send_command(sid, {"type": "set", "params": {"attr": "audit.counter", "operation": "add", "value": 1}})

# 正确：increment 映射为 set(add, delta)，3 次结果为 3
client.send_command(sid, {"type": "increment", "params": {"attr": "audit.counter", "delta": 1}})
```

**避坑要点**：

- core_eval 的 `set` 处理器只支持 `operation='set'`（覆盖）
- `add`/`sub` 操作必须用 `increment`/`decrement` 指令（有独立的 transform 规则）
- transform 规则中的 meta-instruction `set` 不受此限制（直接由 `exec_set` 执行）
- `check_pitfalls.py` 可自动检测此坑（P20）

---

### 坑 21：transform 规则的 set value 引用其他规则设置的 payload 路径，路径不存在时 transition 静默回滚

**触发场景**：transform 规则中 `set` 的 `value` 使用 `__exec__.payload.X` 路径引用，`X` 由其他指令的规则设置（如 `compacted_log` 由 `audit_compact` 设置）。当 `X` 尚未被设置时，条件分支触发该 `set`。

**现象**：transition 报 `PathResolutionFailed` 错误，审计链记录 `Error` fact（**仅 content_hash，无人类可读消息**），状态**静默回滚**到 transition 前的值。在 while_loop 内不触发（前置指令已设置 `X`），只在独立调用或部分调用时暴露。

**根因**：

1. `resolve_path_or_literal`（executor.rs:135-146）对 `__` 开头的字符串调用 `resolve_path`，路径不存在时返回 `TcbError::PathResolutionFailed`
2. `execute_transition`（transition.rs:156-177）遇到 `Err` 不更新 `state.payload`，整个 transition 回滚
3. Error fact 只记录 `content_hash`，不记录错误详情，**极难排查**

**实际案例**：`evolution_scanner` 规则在 `failure_count >= 3` 时设 `evolve_request.context = __exec__.payload.audit.compacted_log`，但独立调用 `evolution_scanner` 时 `compacted_log` 不存在（由 `audit_compact` 指令设置，在 while_loop body 中排在 `evolution_scanner` 之前）：

```
3x evolution_scanner (独立调用):
  第1次: failure_count = 0+1 = 1, check 1>=3? No → OK
  第2次: failure_count = 1+1 = 2, check 2>=3? No → OK
  第3次: failure_count = 2+1 = 3, check 3>=3? Yes → set evolve_request.context = compacted_log
         → PathResolutionFailed! → transition 回滚 → failure_count 保持 2
         → 审计链记录 1 个 Error fact（只有 hash，无消息）
```

**修复方案**：

1. **确保引用的路径在前置指令中已设置**（如 while_loop body 中 `audit_compact` 在 `evolution_scanner` 之前）
2. **用 `branch+exists(path)` 保护引用**，路径存在时才执行 `set`：
   ```json
   {
     "type": "branch",
     "params": {
       "domain": { "type": "exists", "path": "payload.audit.compacted_log" },
       "on_true": [
         {
           "type": "set",
           "params": {
             "attr": "audit.evolve_request.context",
             "value": "__exec__.payload.audit.compacted_log"
           }
         }
       ],
       "on_false": []
     }
   }
   ```
3. **测试独立指令前手动预设依赖字段**：
   ```python
   client.send_command(sid, {"type": "set", "params": {"attr": "audit.compacted_log", "operation": "set", "value": {"init": "test"}}})
   ```

**避坑要点**：

- `set` 的 `value` 路径引用（`__exec__.payload.X`）是**合法的**，但 `X` 必须存在
- 多规则协作场景中存在**隐式执行顺序依赖**——规则 A 依赖规则 B 先设置某个 payload 字段
- 这种依赖在规则文件中**不可见**，只在运行时暴露
- 排查方法：检查审计链中是否有 `Error` fact，如果有，可能是路径解析失败导致的状态回滚
- `check_pitfalls.py` 可检测 `set` 的 `value` 引用 `__exec__.payload.X` 路径（P21，warning）

---

## 附：排查方法论

这次集成过程中总结的排查经验：

### 1. IoRequest 超时排查看链路

```
指令提交 → 反应器展开规则 → IoRequest 产生
    → IoSubscriber 是否收到 event？（查 server 日志 "IoSubscriber 已为 session 启动"）
    → dispatcher 是否注册了对应 IoType？（查 ServiceRegistry 配置）
    → HttpHandler 是否被 SSRF 拦截？（查错误消息 "SSRF blocked"）
    → 外部服务是否可达？（curl 直接测）
    → IoResponse 是否回写？（查 payload __io_result__）
```

### 2. 规则不生效排查

```
规则条件永远 false → 检查 path 是否带 payload. 前缀
conditional 误触发 → 检查 exists vs not(eq(null))
io_request 参数不对 → 检查 args 是否在 payload 构造
服务返回字段取不到 → 检查 __io_result__ 是否被解析为结构化 JSON
```

### 3. 审计链排查

```
审计 entries 为空 → 确认查的是 /api/sessions/{id}/audit 不是 /api/audit
verified=false → 审计链被篡改或导入数据损坏
causal_chain 长度=1 → 追溯的是根因 Command，换 IoResponse 追溯看更深链路
```

---

## 五、升级与发布（4 个坑，0.3.0 新增）

### 坑 20：audit_report() 返回值从 String 改为 Result → 编译错误

**触发场景**：从 v0.2.x 升级到 0.3.0，代码中调用 `api.audit_report().await` 或 `session.audit_report()`。

**现象**：编译错误，`mismatched types: expected struct String, found enum Result<String, serde_json::Error>`。

**根因**：0.3.0 同步 evorule 核心 0.3.2 的 Breaking Change，`auditor.report()`/`auditor.export()` 从 `String` 改为 `Result<String, serde_json::Error>`，不再静默退化为 `"{}"`。`GovernanceApi::audit_report()` 和 `session.audit_report()` 同步变更。

**修复方案**：
```rust
// v0.2.x（旧）
let report: String = api.audit_report().await;

// 0.3.0（新）
let report: String = api.audit_report().await?;  // 传播错误
// 或
let report: String = api.audit_report().await.unwrap_or_else(|e| {
    tracing::error!("审计报告序列化失败: {e}");
    "{}".to_string()
});
```

**避坑要点**：HTTP API `GET /api/sessions/{id}/audit` 的返回格式不变（成功时仍是 JSON），但序列化失败时现在返回 500 而非空对象。客户端需处理 500 响应。

---

### 坑 21：元指令白名单修正 → noop/increment transform 被拒绝

**触发场景**：0.3.0 之前写的规则文件中，transform 规则的 `type` 字段使用了 `noop`、`increment`、`decrement`。

**现象**：`POST /api/rules/validate` 返回校验失败，错误信息类似 `"transform[0].type: noop is not one of [set, push, branch, io_request, collect, merge]"`。

**根因**：0.3.0 同步 evorule 核心 0.3.2 的元指令白名单修正。`noop`/`increment`/`decrement` 是**业务指令层**类型（队列中的指令），不是 meta 指令。之前的文档和校验器误将它们列为 meta 指令，导致假阳性/假阴性。

**修复方案**：
- transform 规则的 `type` 只能是 6 种：`set` / `push` / `branch` / `io_request` / `collect` / `merge`
- `noop` 初始指令是 CLI/应用层构造的业务指令，不是 transform 规则类型
- `increment`/`decrement` 如果需要，应通过 `set` 元指令的 `operation: "add"`/`"sub"` 实现

**避坑要点**：提交规则前先用 `POST /api/rules/validate` 校验，该端点使用 `core/rule_schema` 的 JSON Schema 做权威校验，比 evorule TCB 内部校验更早发现问题。

---

### 坑 22：[patch.crates-io] 段导致发布构建失败

**触发场景**：0.3.0 开发阶段，根 `Cargo.toml` 新增了 `[patch.crates-io]` 段，用本地 path 覆盖 evorule-tcb/reactor/governance/bundle。

**现象**：在其他机器上 clone 仓库后 `cargo build` 失败，报错 `path ../evorule/evorule-tcb does not exist`。或发布 Docker 镜像时构建失败。

**根因**：`[patch.crates-io]` 是**本地开发专用**，依赖本地 `../evorule/` 和 `../evorule-bundle/` 目录存在。这些目录在其他机器或 Docker 构建环境中不存在。

**修复方案**：
- **本地开发**：保留 `[patch.crates-io]` 段，确保 `../evorule/` 和 `../evorule-bundle/` 存在
- **发布前**：必须移除 `[patch.crates-io]` 段，使用 crates.io 上的正式版本
- **Docker 构建**：Dockerfile 中不应包含 `[patch.crates-io]` 段，或在构建前用 `sed` 移除

**避坑要点**：`docs/RELEASE_PROCESS.md` §1.2 的发布前就绪检查会检测 `[patch.crates-io]` 段是否存在。发布前务必运行 `scripts/validate-release.ps1`。

---

### 坑 23：规则包导入失败 → 原子回滚后无任何规则生效

**触发场景**：`POST /api/bundles` 导入规则包，包内含 15 条规则，其中 1 条规则 schema 校验失败。

**现象**：导入返回 `{"imported": false, "status": "rolled_back", "error": "..."}`，检查活跃规则包列表发现该包完全不存在，不是 14 条成功 1 条失败。

**根因**：规则包导入使用 `evorule-bundle` crate 的 6 项校验链 + 逐条 Schema 门禁 + **原子落盘**机制（T2：36 号集成契约）。任何一条规则校验失败，整个包导入回滚，不会部分生效。这是有意设计，避免"半生效"状态导致难以排查的问题。

**修复方案**：
1. 查看返回的 `error` 字段，定位失败的规则文件和具体原因
2. 修复该规则文件（常见原因：meta 指令类型错误 / 必填字段缺失 / domain path 格式错误）
3. 用 `POST /api/rules/validate` 单独校验修复后的规则
4. 重新导入整个包

**避坑要点**：导入前先用 `core/rule_schema` 校验包内所有规则。`scripts/check_schema_sync.py` 可批量校验目录下所有规则文件。

---

## 文档维护

- **更新时机**：每次集成 evorule-server 遇到新坑时，追加到对应分类
- **格式要求**：每个坑包含 触发场景 → 现象 → 根因 → 修复方案（含代码位置） → 避坑要点
- **代码引用**：用 `file:line` 格式，便于 IDE 跳转
- **验证**：修复方案必须是经过实际验证的，不能是理论推测

---

_本文档由 2026-07-31 的 yuanze-demos 集成工作总结而成。对应代码见 `yuanze-demos` 仓和 `evorule-server` 仓。_

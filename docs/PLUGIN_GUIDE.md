# 插件开发指南（外部插件包）

> 适用版本：evorule-server 0.5.0+
> 首个范本：[`plugins/finance-config/`](../plugins/finance-config/)（财务配置键读写，独立服务进程 + 自持存储 + 审批管理面）

---

## 一、插件三通道：你的服务该走哪条路

evorule-server 的服务能力按"来源"分为三通道，对账端点 `GET /api/services` 统一呈现：

| 通道 | `source` 值 | 形态 | 适用场景 |
|------|------------|------|---------|
| 宿主内置 | `native` | Rust crate 编译进 server 二进制（PLUGIN_DEFS 登记表） | 纯计算、轻状态、随宿主发布的核心能力 |
| **外部插件包** | `plugin` | **独立进程 + 自持数据目录 + plugin.json 清单**（本指南） | 任意语言实现、独立演进、可拔插的业务能力 |
| 注册表绑定 | `registry` | `service_registry.json` 显式绑定 URL | 已有 HTTP 服务直接接入，无需打包 |

**选型判断**：需要装入/拔出零宿主改动、非 Rust 实现、或数据需自持 → 外部插件包；只是临时挂一个已有 HTTP 服务 → registry 绑定；随宿主同生共死的核心能力 → native。

---

## 二、插件包形态

一个外部插件包 = 一个目录，自包含三件事：

```
my-plugin/                    ← 插件包根目录（目录名建议 = 插件 id）
  ├─ plugin.json              ← 插件包 SSOT（服务声明 + 参数契约，见 §三）
  ├─ <可执行物>               ← 独立服务进程（exe / 脚本 / 容器，任意语言）
  └─ data/                    ← 自持数据目录（随包装卸，不落宿主 data）
```

- **独立进程**：插件是普通 HTTP 服务进程，由部署侧编排拉起（bat / 脚本 / 手工），server 不负责 spawn/watchdog（进程守护属后续专项，第一版由部署侧自管）。
- **自持数据**：插件数据存自己的目录，装卸不污染宿主。
- **崩溃隔离**：插件进程崩溃只影响自身调用（错误透传 fail-fast），server 存活。

---

## 三、plugin.json 规范（插件包 SSOT）

### 3.1 字段定义

| 字段 | 必填 | 类型 | 说明 |
|------|------|------|------|
| `id` | ✅ | string | 插件包 id，**须与 plugin_manifest.json 条目键一致**（不一致启动期拒绝，防漂移） |
| `version` | ✅ | string | 插件包版本，**独立演进**（与 server 版本无关），透传至对账清单 |
| `description` |  | string | 插件包描述，透传至对账清单与启动日志 |
| `base_url` | ✅ | string | 服务进程根地址（`http://` 或 `https://` 前缀必检）；路由 = `base_url + /services/{name}`；**本地插件用 `127.0.0.1` 需 server 以 `--allow-loopback` 启动** |
| `services` | ✅ | array | 服务声明集（**不得为空**——要停用请在挂载清单置 `enabled:false`） |

`services[]` 每项：

| 字段 | 必填 | 类型 | 说明 |
|------|------|------|------|
| `name` | ✅ | string | 服务名，**全局唯一**（与宿主内置、注册表、其他外部包均不得冲突，冲突启动期拒绝） |
| `sensitive` |  | bool | 敏感标记（缺省 false）。`true` 时 REST 直调 `POST /api/services/{name}/invoke` 返回 403——敏感服务**必须经会话 call_service 走审计与审批链** |
| `description` |  | string | 服务描述，透传至对账清单（LLM 消费方据此理解工具用途） |
| `parameters` |  | object | **参数契约**（OpenAI function parameters 简化子集：type/properties/required）。对账清单透传，LLM 消费方据此生成动态工具 schema，可带参真实调用 |

### 3.2 完整示例（范本 plugins/finance-config）

```json
{
  "id": "finance-config",
  "version": "0.1.0",
  "description": "财务域配置键读写（外部插件包范本）",
  "base_url": "http://127.0.0.1:9110",
  "services": [
    {
      "name": "finance_config_get",
      "sensitive": false,
      "description": "财务配置键读取",
      "parameters": {
        "type": "object",
        "properties": {
          "key": { "type": "string", "description": "配置键，例: config:limits.travel.max_amount" }
        },
        "required": ["key"]
      }
    },
    {
      "name": "finance_config_set",
      "sensitive": true,
      "description": "财务配置键写入（需审批后落库）",
      "parameters": {
        "type": "object",
        "properties": {
          "key": { "type": "string" },
          "new_value": { "description": "新值（任意 JSON）" },
          "reason": { "type": "string" },
          "proposed_by": { "type": "string" }
        },
        "required": ["key", "new_value"]
      }
    }
  ]
}
```

> **参数契约归属原则**：参数 schema 由**服务提供方**（插件包）声明为 SSOT——server 只透传不解释，消费方只消费不定义。native/registry 来源无此声明时消费方降级空 schema。

---

## 四、挂载：plugin_manifest.json 登记一行

```jsonc
// plugin_manifest.json（与进程内插件条目共存）
{
  "plugins": {
    "demo-services":   { "enabled": true, "services": ["config_persist"] },   // builtin 条目（既有）
    "finance-config":  { "enabled": true, "manifest": "plugins/finance-config/plugin.json" }  // external 条目
  }
}
```

- external 条目键 = 插件 id；`manifest` 指向 plugin.json。
- **相对路径基准 = plugin_manifest.json 所在目录**（清单自包含语义：整体挪动/换机部署不破装载）。
- server 启动传 `--plugins <清单路径>`；未配置清单 → 外部插件不装载（显式安装语义，与进程内"缺省全启"相反）。

---

## 五、服务调用契约

### 5.1 你的插件要实现的 HTTP 接口

每个声明服务实现一个端点：

```
POST {base_url}/services/{name}
Header:  X-Source: evorule-server        （server 侧自动附加）
Body:    args 原样 JSON（调用方参数对象）
响应:    结果 JSON（HTTP 响应体即服务结果）
超时:    server 侧 5000ms（派生路由条目固定值）
```

契约与 registry 绑定、invoke 端点完全同构——`args` 序列化为 HTTP body，HTTP 响应即服务结果。**建议**同时实现存活探针：

```
GET /health   →  {"status": "ok", "plugin": "<id>", "version": "<version>"}
```

### 5.2 调用方如何到达你的服务（你无需关心，仅供理解）

```
规则/Agent 发 call_service {service_name, args}
  → server 查派生路由表（与 registry 条目同管道）
  → POST {base_url}/services/{service_name}，body=args
  → 响应即服务结果，io_request/io_response fact 由 server 反应器照常记录
```

**审计不变性**：插件进程外执行不影响 WAL/hash-chain 审计事实链——审计记录的是 server 侧的调用事实，与执行进程位置无关。

---

## 六、管理面规范（涉及人工审批的插件）

需要人工审批的插件（敏感写操作等）应自持管理面，范本见 finance-config：

```
GET  /admin/proposals                    待批提案列表
POST /admin/proposals/{id}/approve       body: {"approver": "..."}   批准落库
POST /admin/proposals/{id}/reject        body: {"approver": "...", "reason": "..."}  拒绝
```

**认证规范**：

- `Authorization: Bearer <token>`；token 经环境变量注入（范本：`FINANCE_PLUGIN_ADMIN_TOKEN`）。
- **未配置 token → 管理面一律 503 拒绝**（fail-fast 不静默裸奔）；token 错误 → 401。
- 审批动作带操作者标识，入插件自持审计（AuditEntry）。

**语义规范**：写路径 = "创建提案 → 人工审批 → 落库"两段式，服务调用本身**只创建提案不落库**（呼应治理哲学：静默处置允许，静默通过禁止）。

---

## 七、装卸操作手册

### 7.1 装入（零宿主代码改动、零重编）

1. 启动插件进程（部署侧编排，范本 `plugins/finance-config/start-finance-config.bat`）；
2. plugin_manifest.json 登记 external 条目（`enabled: true` + `manifest` 路径）；
3. 重启 server（若 base_url 为 127.0.0.1，server 需 `--allow-loopback`）；
4. 验证：`GET /api/services` 出现 `source: "plugin"` 条目；`GET /api/health` 的 `plugins` 节出现该插件（`external: true`）。

### 7.2 拔出

1. plugin_manifest.json 该条目 `enabled: false`（或删除条目）；
2. 重启 server → 对账清单该插件服务消失，call_service 该服务名如实报错（或按回落语义走 registry）；
3. 插件进程可停（自持数据随目录保留）。

### 7.3 启动期三拒绝校验（fail-fast，任一命中即拒绝装载并报错退出）

| # | 校验 | 错误信息要点 |
|---|------|-------------|
| 1 | plugin.json 不可读 / JSON 非法 / id 漂移（条目键 ≠ 声明 id）/ 空服务集 / base_url 非 http(s) | 附自诊断指引，不静默装载 |
| 2 | 服务名冲突（与宿主内置声明表全集、注册表、已装载外部包任一冲突） | 服务名全局唯一，含**停用**插件名亦占用（防回落路径被静默劫持） |
| 3 | 挂载清单本身不可读 / JSON 非法 | 启动报错退出 |

---

## 八、安全与信任模型

- **信任模型与 registry 一致**：运维显式登记即可信（治理动作，非工程动作）。
- **loopback 防护**：`127.0.0.1` / 私有 IP 地址需 server 显式 `--allow-loopback`（SSRF 防护门禁，仅限本地开发；生产插件建议部署内网固定地址）。
- **sensitive 守卫**：声明 `sensitive: true` 的服务，REST 直调一律 403——敏感操作只能经会话链（审计 + 审批门）。
- **最小暴露面**：管理面 token 环境变量强制；缺省拒绝。

---

## 九、任意语言实现要点（最小要求清单）

外部插件包与语言无关，满足以下即可：

1. HTTP 服务：`POST /services/{name}`（body=args JSON，响应=结果 JSON），建议 `GET /health`；
2. 声明：plugin.json 如实描述服务与参数契约；
3. 数据自持：不读写宿主 data 目录；
4. 涉审批：按 §六管理面规范（token 认证 + fail-fast）；
5. 启动脚本（Windows bat）使用**纯 ASCII 编码**（cmd 按 ANSI 代码页预解析，非 ASCII 会乱码破坏命令）。

---

## 相关文档

- [INTEGRATION_GUIDE §八 插件清单](INTEGRATION_GUIDE.md#八插件清单部署期启用裁剪) — 挂载清单与回落语义
- [README「服务能力对账与直调」](../README.md) — 对账/直调端点
- 范本源码：[`plugins/finance-config/`](../plugins/finance-config/)（Rust/axum 实现）

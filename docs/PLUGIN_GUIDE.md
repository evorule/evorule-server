# 插件开发指南

> 适用版本：evorule-server 0.5.0+（声明式 pack 自 0.6.0 起）
> 两类范本：
> - **外部插件包**（进程型）：[`plugins/finance-config/`](../plugins/finance-config/)（财务配置键读写，独立服务进程 + 自持存储 + 审批管理面，§一～§十一）
> - **声明式资产包**（契约 v1）：[`plugins/finance-pack/`](../plugins/finance-pack/)（1 场景 + 2 规则模板，零代码零进程，§十二）

---

## 〇、插件两类形态：先选对路

| 形态 | 提供什么 | 形态特征 | 适用场景 | 章节 |
|------|---------|---------|---------|------|
| **外部插件包** | 服务能力（`call_service` 可调） | 独立进程 + 自持数据 + plugin.json | 任意语言实现、有状态、需审批的业务能力 | §一～§十一 |
| **声明式资产包** | 场景 + 规则模板（表单化生成规则草稿） | 纯 JSON 目录，**零进程零代码** | 把领域知识打包成"选场景 → 填表单 → 出草稿"的低门槛规则生产 | §十二 |

两者可在同一份 plugin_manifest.json 共存登记（§四）；一个插件目录只能是一种形态。

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
// plugin_manifest.json（三种条目形态共存）
{
  "plugins": {
    "demo-services":   { "enabled": true, "services": ["config_persist"] },                      // builtin 条目（进程内）
    "finance-config":  { "enabled": true, "manifest": "plugins/finance-config/plugin.json" },    // external 条目（进程外服务包）
    "finance-pack":    { "enabled": true, "pack": "plugins/finance-pack/pack.json" }             // pack 条目（声明式资产包,§十二）
  }
}
```

- external 条目键 = 插件 id；`manifest` 指向 plugin.json；pack 条目 `pack` 指向 pack.json。
- **`manifest` 与 `pack` 互斥**（同条目同时声明二者启动期拒绝）；`services` 子集形态仅用于 builtin 条目。
- **相对路径基准 = plugin_manifest.json 所在目录**（清单自包含语义：整体挪动/换机部署不破装载）。
- server 启动传 `--plugins <清单路径>`；未配置清单 → 外部插件与 pack 均不装载（显式安装语义，与进程内"缺省全启"相反）。

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

契约与 registry 绑定、invoke 端点完全同构——`args` 序列化为 HTTP body，HTTP 响应即服务结果。**推荐实现**存活探针（server 探活任务按周期探测，随 `/api/health` external 插件节呈现运行时存活状态）：

```
GET /health   →  {"status": "ok", "plugin": "<id>", "version": "<version>"}
```

探活结果三态：

- **online** — 2xx 且响应体 JSON 可解析；
- **offline** — 连接失败 / 超时 / 非 2xx（触发 `plugin_offline` 报警事件，恢复后自动记 `plugin_online` 关警留痕）；
- **no_probe** — `/health` 返回 404/405（插件未实现探针）：**如实呈现、不报警不告噪**，文档引导补齐。

探活周期经 `--plugin-probe-interval` 配置（缺省 30s，0 = 关闭探活）。

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

**统一审批入口（server 审批代理）**：部署侧将同一 admin token 值同时注入插件进程与 server 两侧环境变量（server 侧命名 `EVORULE_PLUGIN_ADMIN_TOKEN__<ID 大写下划线>`，如 `EVORULE_PLUGIN_ADMIN_TOKEN__FINANCE_CONFIG`；密钥零落盘），console 插件审批面即可经 server 代理完成审批，审批入口收敛为单通道：

```
GET  /api/plugins/{id}/admin/proposals                   待批提案列表（原样透传）
POST /api/plugins/{id}/admin/proposals/{pid}/approve     批准（approver 由 server 强制注入登录身份）
POST /api/plugins/{id}/admin/proposals/{pid}/reject      拒绝（reason 保留前端值）
```

- approver 由 server 代理端覆盖为平台认证登录 actor，**不信任前端自报**（防伪造操作者）；审计归属不变（入插件自持审计）。
- server 未配置该插件 token → 代理 503；插件 id 未知/非 external → 404；插件管理面不可达 → 502。
- 代理为增量通道，插件管理面直连端口仍可用（运维兜底路径保留）。

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

1. HTTP 服务：`POST /services/{name}`（body=args JSON，响应=结果 JSON），推荐 `GET /health` 存活探针（未实现 = no_probe，不报警；见 §5.1）；
2. 声明：plugin.json 如实描述服务与参数契约；
3. 数据自持：不读写宿主 data 目录；
4. 涉审批：按 §六管理面规范（token 认证 + fail-fast）；
5. 启动脚本（Windows bat）使用**纯 ASCII 编码**（cmd 按 ANSI 代码页预解析，非 ASCII 会乱码破坏命令）。

---

## 十、部署侧看门狗（插件进程自动恢复，可选）

> 定位：server **不 spawn 不守护**插件进程（§二），插件进程的拉起与守护属部署侧职责。分发包内置部署侧看门狗资产（Windows 版包），实现"offline 报警 → 自动拉起 → online 关警"闭环；不启用不影响任何功能。

### 10.1 资产与形态

| 文件 | 说明 |
|------|------|
| `start-watchdog.bat` | 启动壳（最小化窗口跑 PowerShell；纯 ASCII） |
| `watchdog-plugins.ps1` | 看门狗主体（周期读 `/api/health`，离线自动拉起） |
| `plugins-watchdog.json` | 守护配置（缺省空表 = 不守护任何插件，按需登记） |

配置示例（守护 finance-config）：

```jsonc
{
  "health_url": "http://127.0.0.1:18080/api/health",
  "interval_secs": 10,            // 探测周期
  "offline_threshold": 3,         // 连续 N 个周期 offline 才拉起（防抖）
  "max_restarts_per_hour": 5,     // 每插件每小时自动拉起上限
  "plugins": {
    "finance-config": {
      "command": "plugins\\finance-config\\evorule-finance-config-plugin.exe",
      "args": ["--port", "9110", "--data", "plugins\\finance-config\\data"],
      "working_dir": ".",
      "env": { "FINANCE_PLUGIN_ADMIN_TOKEN": "与 server 侧同值的管理 token" }
    }
  }
}
```

### 10.2 行为语义

- **只对真正离线动作**：`status == "offline"`（连接失败/超时/非 2xx）才计入；`no_probe`（未实现 `/health`，404/405）与未挂载插件**如实跳过、不误动作**；
- **防抖**：连续 `offline_threshold` 个周期 offline 才执行拉起，单次探测抖动不动作；
- **升级（系统独占路径）**：每插件每小时拉起次数达 `max_restarts_per_hour` 后停止拉起，日志输出 `ESCALATION` 升级告警，等人工介入；插件重新被观测到 `online` 后闩锁自动解除（人工修复场景）；
- **留痕分工**：拉起动作写 `data\watchdog.log`（部署侧）；`plugin_offline`/`plugin_online` 报警与关警事件在 server 审计面全量（`/api/audit/platform-events` 可查）——部署侧日志与审计面各司其职，不重复建设。

### 10.3 非 Windows 部署形态

Linux/容器部署无 PowerShell 依赖时，用编排层等价物达到同样效果：

- **systemd**：插件进程做成 unit，`Restart=always` + `RestartSec`；
- **容器编排**：插件容器 `restart: always/unless-stopped`（Docker Compose / Kubernetes 同理）；
- 两者的"拉起次数上限/升级告警"由编排层健康检查（healthcheck + max 失败策略）或外部告警承接，语义与 §10.2 对齐。

---

## 十一、外部应用接入与配额（应用凭据通道）

> 定位：插件包之外,外部应用（如运维脚本、第三方系统、伴生 agent 程序）经 server 调用服务能力时,使用**应用凭据**身份而非静态 token——归因粒度到应用,配额防滥用。

### 11.1 应用凭据签发与使用

1. 管理员在 console「应用凭据」工作台（或 `POST /api/platform/apps`,manage_apps 权限）签发,获得 `app_id` + key 明文（**仅此一次返回**,遗失只能吊销重签,身份不复用）;
2. 应用以 `Authorization: Bearer <key>` 调用业务 API,请求按 `app_id` 落 `app_invoke` 归因事件（`/api/audit/platform-events` 可查）;
3. 吊销即时生效（下一请求即 401）。

### 11.2 配额语义与 429 消费

签发时可设两维配额（缺省不限）,管理面可随时调整（全量覆盖,null=不限,已用量保留）:

| 维度 | 语义 | 超限响应 |
|------|------|---------|
| 速率限制 | per-app 令牌桶（次/秒,容量=速率值） | `429` + `Retry-After: <秒>`（令牌恢复估算）+ `x-quota-dimension: rate` |
| 日配额 | 每日总量上限,固定 UTC 日窗口（零点重置） | `429` + `Retry-After: <秒>`（到 UTC 次日零点）+ `x-quota-dimension: daily` |

**消费端最佳实践**：

- 收到 429 先读 `x-quota-dimension` 区分维度：`rate` = 退避 `Retry-After` 秒后重试即可（瞬时限速）;`daily` = 当日预算已尽,重试无意义,应停止请求或降级到次日（`Retry-After` 为到零点的秒数,通常很长）;
- 429 响应体含 `retry_after_secs` 与 `dimension` 字段（机器可读,与头等价）;
- 超限请求**不消耗日配额预算**,也不落逐条归因（防写放大）;持续超限不静默——聚合报警事件（`app_quota_exceeded`）入审计面,管理员可见;
- 日计数经定期快照持久化,server 重启后从最近快照恢复（崩溃窗口内的计数丢失上限=一个快照周期,如实接受）。

---

## 十二、声明式资产包（Plugin Contract v1）

> 定位：**零进程零代码**的资产包——把领域知识打包成「场景 + 规则模板」，用户在 console 通用表单页选场景字段、填表单值，server 以**纯函数**生成规则 JSON 草稿。草稿不落库，生效仍走既有 Draft→Publish 治理链。
> 契约 SSOT：Plugin Contract v1（MAJOR 不符拒载）；范本：[`plugins/finance-pack/`](../plugins/finance-pack/)。

### 12.1 目录形态

```
finance-pack/                  ← pack 根目录（目录名建议 = pack id）
  ├─ pack.json                 ← pack SSOT（声明 + 资产索引）
  └─ assets/
      ├─ scenes/*.json         ← 场景资产（表单字段下拉来源）
      └─ templates/*.json      ← 规则模板资产（骨架 + 表单声明）
```

### 12.2 pack.json 规范

| 字段 | 必填 | 类型 | 说明 |
|------|------|------|------|
| `id` | ✅ | string | pack id，**须与 plugin_manifest.json 条目键一致**（不一致启动期拒绝，防漂移） |
| `contract_version` | ✅ | string | 契约版本，当前 `1.x`；**MAJOR 不符拒载**（契约演进须升级 pack 或 server） |
| `version` | ✅ | string | pack 版本，独立演进，透传至插件清单 API 与草稿 provenance |
| `description` | ✅ | string | pack 描述，透传至插件清单 API |
| `capabilities` | ✅ | array | 能力声明集（v1 已知集：`assets` / `services` / `flow-compile` / `ai-assist`）；**未知能力拒绝装载**（不静默忽略） |
| `assets` | assets 能力时✅ | object | `scenes` / `templates` 两个字符串数组：显式相对路径或 `*.json` glob（相对 pack.json 所在目录；glob 无匹配文件即拒载） |

**fail-fast 清单**（任一命中即拒绝装载并报错退出）：pack.json 不可读 / JSON 非法 / 顶层必须是 object / **未知顶层字段**（v1 字段钉死，新字段须走契约演进）/ id 漂移 / contract_version MAJOR 不符 / 未知能力 / 声明 assets 能力但缺 assets 节（或有 assets 节未声明能力）/ 场景或模板 id 重复 / glob 无匹配。

### 12.3 场景资产（scene）

给模板表单提供「字段下拉」来源，用户不记字段名。示例（范本 `assets/scenes/expense.json`）：

```jsonc
{
  "scene_id": "expense",
  "display_name": { "zh": "报销场景", "en": "Expense" },   // 双语展示对象,R5 纯展示数据
  "description": "...",                                     // 可选
  "business_objects": [
    {
      "object_id": "expense_form",
      "display_name": { "zh": "报销单" },
      "fields": [
        // path = 状态路径,声明了 path 的字段才可用于模板 .path 锁定(见 12.5)
        { "field_id": "amount", "display_name": { "zh": "报销金额" }, "type": "number", "unit": "元",
          "path": "__exec__.payload.amount" },
        { "field_id": "dept", "display_name": { "zh": "申请部门" }, "type": "enum",
          "options": ["sales", "hr", "finance"], "path": "__exec__.payload.dept" }
      ]
    }
  ]
}
```

- 字段 `type` 取 §12.4 控件词表**减 `scene_field`**（scene_field 仅用于模板表单）；enum 必须带非空 `options`。
- 资产字段由契约 v1 钉死（未知顶层字段 / 未知业务对象字段 / 未知字段键均拒载）。

### 12.4 控件词表（v1 固定枚举；新增 = 契约 v2 事件）

| type | 表单呈现 | 生成值类型 |
|------|---------|-----------|
| `text` / `textarea` | 文本框 / 多行文本框 | 非空字符串 |
| `number` / `currency` | 数字输入 | 数值 |
| `date` | 日期输入 | 非空字符串 |
| `boolean` | 复选框 | 布尔 |
| `enum` | 下拉（必须带非空 `options`） | 字符串（越界生成期报错） |
| `scene_field` | 场景字段下拉（来源 = 场景中**声明了 path** 的字段） | 字段 id（仅可作 `{{form.X.path}}` 使用） |

`params_form[]` 每项：`field_id`（模板内唯一）/ `display_name`（双语对象）/ `type` / `required`（缺省 false）/ `default` / `options`（enum）/ `scene_ref`（scene_field 必填，参数级或模板级任一声明）。**required 语义：表单值与 default 都缺失才报错**（default 的职责就是填充缺失值）。

### 12.5 规则模板资产（rule template）与生成语言

```jsonc
{
  "template_id": "amount_threshold_approval",
  "display_name": { "zh": "金额阈值审批" },
  "scene_ref": "expense",
  "params_form": [ /* 12.4 形态 */ ],
  "rule_draft_skeleton": {
    "id": "{{pack}}.{{template}}",
    "version": 1,
    "description": "{{form.threshold}} 以上需 {{form.approver}} 审批",
    "transform": [
      { "type": "branch", "params": {
        "domain": { "type": "lt", "path": "{{form.amount_field.path}}", "value": "{{form.threshold}}" },
        "on_true": [],
        "on_false": [
          { "type": "io_request", "params": {
            "io_type": "call_external",
            "prompt": "{{form.threshold}} 以上需 {{form.approver}} 审批",
            "role": "{{form.approver}}" } }
        ] } }
    ]
  }
}
```

**生成语言 `{{...}}`（v1 故意极小，无逻辑无表达式）**：

| 占位符 | 求值 | 类型规则 |
|---|---|---|
| `{{form.X}}` | 表单值 | 按 X 的控件 type 定型：number/currency → 数值字面量；enum/text/... → 字符串；整槽替换保留 JSON 类型，嵌入字符串则文本化 |
| `{{form.X.path}}` | 场景字段状态路径 | **仅 scene_field 可用**；取值域锁定为「场景中声明了 path 的字段」，非自由字符串 |
| `{{pack}}` / `{{template}}` | pack id / 模板 id | 字符串 |
| 其他任何 `{{...}}` | **装载期即拒绝**（fail-fast，不静默替换） | — |

**四条红线**（装载期与生成期双重校验）：

| 红线 | 内容 | 校验点 |
|------|------|--------|
| R1 确定性 | 同（模板字节, 表单值）→ 字节级同输出；零随机/零时钟/零 IO | 随仓门禁测试 `finance_pack_reference_impl_loads_and_generates` |
| R2 结构不可达 | 用户值只落**值位**（value/prompt/role/description 等），永远填不进结构键；`.path` 取值域锁定 | scene_field 裸用（不带 `.path`）拒载；非 scene_field 用 `.path` 拒载；**键内占位符拒载** |
| R3 draft-only | 生成不落库不进治理状态；草稿生效必须经用户确认走既有 Draft→Publish 链 | generate 端点纯内存返回 |
| R5 locale 纯展示 | display_name 双语字段仅为展示数据，不进事实/命令 | 装载期校验 {zh,en} 字符串对象 |

### 12.6 API 面（受认证保护，与业务 API 同门禁）

```
GET  /api/plugins                                             已装载 pack 清单（含资产计数）
GET  /api/plugins/{pack_id}/assets/{kind}                     资产只读面,kind ∈ scenes|templates（其他 404）
POST /api/plugins/templates/{pack_id}/{template_id}/generate  草稿生成纯函数面
```

- generate 请求体 = 表单值对象（`{"field_id": 值, ...}`）；响应 = `{"rule_draft": {...}, "provenance": {"pack", "pack_version", "template", "contract_version"}}`。
- 校验失败 → **400 显式错误**（含 fail-fast 文案，不静默降级）；未知 pack / 未知模板 → 404。
- console 消费入口：工作空间页「插件模板」→ `/workspace/templates` 通用表单（模板列表 → 表单 → 生成 → 预览/复制 JSON）。

### 12.7 装卸操作手册

1. **装入**：按 §12.1 备好目录 → plugin_manifest.json 登记 pack 条目（§四）→ 重启 server → 验证：启动日志出现 `插件契约 pack: {id} 装载`；`GET /api/plugins` 出现该 pack。
2. **拔出**：清单条目 `enabled: false`（或删除条目）→ 重启 server → 插件清单该 pack 消失（不涉及服务对账与回落语义）。
3. **升级**：改资产 JSON → 重启即生效（启动期读盘，运行期只读）；改字段形态前先核对契约版本（MAJOR 不符拒载）。

### 12.8 与外部插件包的关系

- 二者**互不替代**：pack 不提供可调用服务（无进程），外部插件包不提供表单化模板；领域能力既有服务又有模板时，登记两个条目各司其职。
- 模板骨架中的 `io_request` 运行时仍经会话链调用服务（call_external），**装载期不校验服务名存在性**（草稿期纯函数、执行期 fail-fast 显式报错）。

---

## 相关文档

- [INTEGRATION_GUIDE §八 插件清单](INTEGRATION_GUIDE.md#八插件清单部署期启用裁剪) — 挂载清单与回落语义
- [README「服务能力对账与直调」](../README.md) — 对账/直调端点
- 范本源码：[`plugins/finance-config/`](../plugins/finance-config/)（Rust/axum 实现）
- 声明式 pack 范本：[`plugins/finance-pack/`](../plugins/finance-pack/)（纯 JSON 资产）

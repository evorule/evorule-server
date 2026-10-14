# evorule 部署基线（生产）

安全路由与网关 SG-4 商品面基线批交付物。本文档是 evorule 对外部署的安全基线：拓扑与暴露面（G-3）、认证 posture、TLS/限流、插件 admin token 命名规约（G-4）、插件完整性策略（G-6）、白名单变更窗口与值守调度（运维条款）。配套反代配方见 `deploy/caddy/`（Caddyfile 生产配方＋Caddyfile.dev 本机验证配方）。

适用形态：单机自托管（D-1 拍板先行形态）；K8s ingress 形态为推广中期扩展，本文条款按组件对应折算。

---

## 1. 拓扑与暴露面

### 1.1 标准拓扑

```
外部应用/用户
   │  （仅 443/80，TLS 终结于反代）
   ▼
Caddy 反代（唯一对外暴露面，deploy/caddy/Caddyfile）
   │  loopback
   ▼
evorule-server  127.0.0.1:18080   ← 唯一必经点（认证/路由/白名单/enforce/审计链）
   ├─ loopback：ai-plugin      127.0.0.1:9130   （LLM 执行体，凭据服务端托管）
   ├─ loopback：wasm-host      127.0.0.1:9140   （插件宿主）
   └─ loopback：evo-agent serve 127.0.0.1:8081  （只连 server，不直连 provider）
```

### 1.2 暴露面纪律

- **server 显式绑定 loopback**：`--addr 127.0.0.1:18080`。警告：server 未显式指定 `--addr` 时的缺省值为 `0.0.0.0:18080`（全接口监听）——生产禁止裸用缺省值。
- **反代是唯一对外面**：除 443/80 外，任何组件端口不得对外放行；防火墙仅放行 80/443，并对 18080 显式入站拒绝作纵深防御（命令示例见 `deploy/caddy/README.md`）。
- **外部应用禁止直通后端**：一切流量必经反代→server，不允许外部应用直连 agent/wasm-host/ai-plugin。
- **组件间互信面**：server↔插件/agent 均走 loopback＋token（见 §2/§4），不跨网段明文传输凭据。

## 2. 认证 posture

### 2.1 server

- 生产必须配置 `EVORULE_AUTH_TOKEN`（Bearer 认证）——**优先环境变量或 JSON 配置文件**，不用 CLI 旗标（进程列表可见参数）。
- 需要 service 身份（可写受保护域 `shared.*.stable.llm.*` / `stable.system.*`）的管道另配 `EVORULE_SERVICE_TOKEN`（与 user token 凭据分层的独立值）。
- **`--insecure-serve`（显式豁免认证）生产禁用**：该旗标仅限回归验证场景（loopback＋无 token 时须显式声明才能无认证启动；非 loopback 绑定下即使声明也硬拒）。
- 生产推荐同时启用 `/metrics` 认证（`--metrics-auth`）与 CORS 白名单收敛（`EVORULE_ALLOWED_ORIGINS` 精确列出允许的前端源，禁止 `*`）。

### 2.2 evo-agent serve 面

- agent 缺省 fail-closed：无任何鉴权配置时不再静默进入无认证模式，按绑定面分流（仅 loopback 降级＋显著告警，非 loopback 拒绝启动）。
- 生产配置 `--auth-token`（或 `EVO_AGENT_AUTH__TOKENS`）后启动；**`--no-auth` 为本地开发豁免旗标**（告警但不拦），生产禁用。
- agent 凭据归边：agent 侧不持有 LLM provider 密钥；LLM 出口经 server（ai-plugin 执行体）。

### 2.3 启动姿势示例（生产）

```powershell
$env:EVORULE_ADDR = "127.0.0.1:18080"
$env:EVORULE_AUTH_TOKEN = "<强随机 token>"          # 部署侧密管注入，零落盘
$env:EVORULE_SERVICE_TOKEN = "<另一个强随机 token>"  # 需要 service 管道时
evorule-server.exe --rules-dir .\rules --service-registry .\resources\service_registry.json
```

## 3. TLS 与限流

- **TLS 终结于反代**：Caddy 自动 HTTPS（配方与证书形态见 `deploy/caddy/README.md`「证书签发」）；反代到 server 为 loopback 明文，属可接受范围（不出机）。
- **限流双面**：
  - 执法主面＝server 内建限流，缺省 200 req/s（burst 200）——**`--no-rate-limit` 生产禁用**（仅 benchmark 用）。
  - 可选增强＝Caddy rate_limit 插件（入口粗粒度防刷），构建与配方见 `deploy/caddy/README.md`。
- 安全响应头（HSTS/nosniff/DENY/no-referrer）由反代配方统一注入，见 `deploy/caddy/Caddyfile`。

## 4. 插件 admin token 命名规约（G-4）

外部插件管理面 token 的 env 名由插件 id 折叠生成：`EVORULE_PLUGIN_ADMIN_TOKEN__<ID 大写,非字母数字折叠为下划线>`（如 `finance-config` → `EVORULE_PLUGIN_ADMIN_TOKEN__FINANCE_CONFIG`）。

- **命名规约（部署侧事前规避）**：插件 id 仅用小写字母/数字/连字符，确保任意两个 id 折叠后互异——`finance-config` 与 `finance_config` 会撞同一 env 名。
- **撞号行为（server 侧执法兜底）**：装载期 fail-fast 拒绝启动（不同 id 折叠映射到同一 env 名时），自诊断指引随错误输出；此检测覆盖唯一 admin token 装载路径（外部插件清单；声明式 pack 形态无 admin token 面）。
- **token 配置纪律**：同一 token 值以 env 同时配给插件进程与 server 两侧（密钥零落盘）；server 启动日志仅呈现配置与否，不呈现值；未配置时审批代理对该插件 503（fail-fast），插件管理面直连端口不受影响。

## 5. 插件完整性（G-6）

wasm-host 插件验签三档：`WASM_HOST_SIGN_POLICY` ＝ `warn | enforce | off`（缺省 warn，迁移期过渡）。

- **生产部署基线：必须 `WASM_HOST_SIGN_POLICY=enforce`**，即分发三件套（分发清单＋ed25519 签名＋信任锚）必须存在且全量校验通过，否则模块拒载（fail-fast）。
- 信任锚装配：`WASM_HOST_TRUSTED_KEYS` 指向受信公钥表；三件套生成用 `wasm-host sign`（生成流程见 WASM_UDF_GUIDE / 插件发布指南）。
- warn 档语义（残余风险声明）：材料缺失放行、材料存在仍全量校验（篡改信号不因 warn 放行）——迁移期专用，生产禁用。
- off 档：完全跳过验签，仅限本地调试，生产禁用。
- 裁断留痕（2026-10-14 SG-4 批）：代码缺省保持 warn（迁移期存量兼容），生产强制性由本基线承载；推广期若存量迁移完成，可评估把代码缺省收敛为 enforce。

## 6. 白名单变更窗口与值守调度（运维条款）

适用资产（静态装载、变更须重启生效）：ServiceRegistry 服务白名单（`--service-registry`）、SQL 模板白名单（`--statement-whitelist`）、model allowlist（ai-plugin `model_allowlist_path` 指向的规则资产）、路由/治理规则集（`--rules-dir`）。

- **装载语义**：以上资产在进程启动/重载时装载，运行期只读——**改动资产文件后须重启 server（或走滚动会话 reload 流程）才生效**；不生效的直接改文件行为是无效操作。
- **变更窗口约定**：
  1. 参赛线/长跑任务在跑期间，**禁止**上述资产的任何变更与重启操作；
  2. 变更一律安排在窗口期（任务空档/错峰），执行人＝值守在位的运维或开发本人，禁止无人值守时改动；
  3. 变更前三件套：备份现行资产（回滚件）→ 写明变更内容与预期 → 变更后重启并按 §8 清单抽验；
  4. 回滚预案＝恢复备份文件＋重启（分钟级）；白名单收紧操作先在验证实例（照 §8 照配部署）过一遍再上生产。
- **service_token/插件 token 等凭据轮换**同窗口纪律：轮换涉及双端（server＋插件/agent）同步改 env，须重启生效，安排在同一窗口一次完成。

## 7. 高危旗标清单（生产禁用判据）

| 旗标 | 语义 | 生产判据 |
|---|---|---|
| server `--insecure-serve` | 显式豁免认证（仅 loopback 有效） | 禁用；仅回归验证场景 |
| server `--no-rate-limit` | 完全跳过限流层 | 禁用；仅 benchmark |
| server `--allow-loopback` | HttpHandler 允许访问 loopback 目标 | 禁用（SSRF 面豁口；本地开发专用） |
| server `--allow-abort` | 强制中止端点族 | 缺省关；确需时收紧到运维网段并记值 |
| server `--allow-interventions` | 高危干预端点族 | 缺省关；同上 |
| server `--openapi-ui` | 挂载 Swagger UI | 对外部署禁用（面纹暴露） |
| server `--web-dir` | server 直接托管静态前端 | 与反代形态二选一；用反代时不启用 |
| agent `--no-auth` | serve 面无认证豁免 | 禁用；仅本地开发 |
| wasm-host `WASM_HOST_SIGN_POLICY=off/warn` | 跳过/放宽验签 | 生产 enforce（§5） |

## 8. 照配验收清单（部署后逐项自检）

按本基线完成部署后，依次验证（全部应通过；任一不过＝部署未达基线）：

| # | 验收项 | 方法 | 预期 |
|---|---|---|---|
| 1 | TLS 生效 | `curl -k https://<域名>/api/health`（生产证书形态免 -k） | 200，证书链有效 |
| 2 | 反代连通 | 同上 | 命中 server /api/health |
| 3 | 入站认证 | 无 token 请求受保护端点 | 401/403 显式拒 |
| 4 | 出口白名单 | 白名单外模型请求经 server→ai-plugin | 502 显式拒＋服务端日志 WARN |
| 5 | 限流执法 | 脚本连发 >200 req/s | 超额请求 429 |
| 6 | SSE 流式 | 订阅 `/api/sessions/{id}/events` | 事件即时到达（不缓冲） |
| 7 | 插件完整性 | enforce 态装载未签名模块 | 拒载＋fail-fast 日志 |
| 8 | admin token 撞号 | 清单内放两个折叠撞号 id | 启动期拒绝装载 |
| 9 | 暴露面 | 外部主机扫描 18080/8081/9130/9140 | 全部不可达（仅 443/80 可达） |
| 10 | 凭据落盘检查 | 检查配置文件/日志 | 零 token/密钥明文 |

本机验证形态：`deploy/caddy/Caddyfile.dev`（自签＋全 loopback，对端 28080）即本清单的照配演练床。

---

## 附：本文档与威胁建模对照

| 基线章节 | 收口缺口 |
|---|---|
| §1 拓扑与暴露面、§3 TLS/限流 | G-3 对外暴露基线缺文档 |
| §4 插件 admin token 命名规约 | G-4 token env 撞号（部署面） |
| §5 插件完整性 | G-6 warn 缺省（生产 enforce 基线） |
| §6 变更窗口与值守调度 | 评估档 F-5（G-10 无热载的运维兜底） |
| §7 高危旗标、§8 照配验收 | 各旗标/判据汇总落点 |

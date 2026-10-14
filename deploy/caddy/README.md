# Caddy 反向代理部署指南（商品面基线）

安全路由与网关 SG-4 商品面基线批交付物。evorule 的安全裁决面（认证/白名单/enforce/审计链）全部在 evorule-server 内，本目录配方只承载商品面执行（TLS 终结/转发/连接管理），不含任何 evorule 语义，可整体替换为 nginx 或其他反代。完整部署基线（绑定面/认证 posture/插件完整性/变更窗口运维条款）见 `docs/deploy-baseline.md`。

## 目录

| 文件 | 用途 |
|---|---|
| `Caddyfile` | 生产配方：自动 HTTPS＋反代＋安全响应头＋SSE 流式透传＋可选限流 |
| `Caddyfile.dev` | 本机验证配方：自签证书、全 loopback（照配验收/J-SG4 同款形态） |

## 安装 Caddy

单机自托管推荐 Caddy（自动 HTTPS，免证书手工运维）。

- Windows：从 https://caddyserver.com/download 下载 `caddy_windows_amd64.exe`，放到本目录（或 PATH 内）。
- 验证：`caddy version`。
- Caddy 核心不含限流模块；需要反代层限流时用 xcaddy 构建（见下节），未构建时限流执法主面＝server 内建 200 req/s。

## 证书签发

- 公网标准形态：`Caddyfile` 自动 ACME 签发与续期，要求 80/443 公网可达、域名 A 记录指向本机。
- 无公网 80/443 环境（云安全组仅放 443 等）：改用 DNS challenge——在站点块加 `tls { dns <provider> <...> }`，需用含对应 DNS 插件的 Caddy 构建（xcaddy 同法）。
- 本机验证：`Caddyfile.dev` 用 `tls internal` 自签（本地 CA 根证书落 `./data/caddy/pki`，信任该根后浏览器/curl 可免 `-k`）。

## 限流插件构建（可选增强）

```powershell
# 需 Go 1.21+；产物 caddy.exe 替换 PATH 内的 caddy
xcaddy build --with github.com/mholt/caddy-ratelimit
```

启用方式见 `Caddyfile` 内注释块（按客户端 IP 对 /api/* 限速）。两层限流分工：Caddy 层＝入口粗粒度（防滥用刷量）；server 内建 200 req/s＝业务执法面（`--no-rate-limit` 生产禁用）。

## 防火墙放行（Windows 示例）

生产仅放行 80/443；server/agent/wasm-host/ai-plugin 全部 loopback，不对外放行：

```powershell
New-NetFirewallRule -DisplayName "evorule-gateway-http"  -Direction Inbound -Protocol TCP -LocalPort 80  -Action Allow
New-NetFirewallRule -DisplayName "evorule-gateway-https" -Direction Inbound -Protocol TCP -LocalPort 443 -Action Allow
# 显式拒绝外部直连后端（纵深防御，缺省未放行即已隔离）
New-NetFirewallRule -DisplayName "evorule-backend-deny" -Direction Inbound -Protocol TCP -LocalPort 18080 -RemoteAddress Any -Action Block
```

## 启动与运维

```powershell
# 校验配方
caddy validate --config Caddyfile

# 前台运行（观察日志）
caddy run --config Caddyfile

# 后台服务化（推荐 NSSM / WinSW 注册为系统服务，开机自启+崩溃拉起）
nssm install evorule-caddy "C:\path\to\caddy.exe" "run --config C:\path\to\Caddyfile"
nssm set evorule-caddy AppDirectory C:\path\to\caddy
nssm start evorule-caddy
```

- 证书续期：ACME 自动续期内建，无需值守；`tls internal` 形态同样自动。
- 日志轮转：配方内 `roll_size`/`roll_keep` 已配置。
- 重载：`caddy reload --config Caddyfile`（不断连）。
- 版本升级：替换二进制后重启服务即可，证书与数据不迁移。

## 照配验收（摘要）

部署完成后按 `docs/deploy-baseline.md` §8 清单逐项自检：TLS 探活、无 token 401、白名单外模型 502、限流 429、SSE 流式不缓冲。本机验证形态直接用 `Caddyfile.dev`（对端 28080）。

# SPDX-License-Identifier: AGPL-3.0-or-later
# start-resident-serve.ps1 — evorule-server 常驻实例启动脚本（唯一权威启动入口）
#
# 【为什么必须有这个脚本】（2026-09-29 caller_role 接线批教训，登记册有同步条目）：
#   常驻 serve 的启动参数此前只散落在会话记忆/对话中，无本地固化位。
#   caller_role 接线批为验证宪法桥接（call-service → 127.0.0.1:9100）新增了
#   --allow-loopback 参数并仅在设计文档留痕——下次会话若按记忆里的
#   "原参数"手工拼启动，桥接 dispatch 将被 SSRF 防线拦截（错误应答
#   丢弃分支），排查成本高且未必能定位到根因。
#   纪律：重启常驻 serve 一律用本脚本，禁止手工拼参数。
#   参数变更须先改本脚本（含理由注释）再执行。
#
# 【--allow-loopback 参数变更留痕】（2026-09-29 caller_role 接线批）：
#   本地开发约定（main.rs L786 注释语义，与已开的 --insecure-serve 同族）：
#   service_registry.json 的 call-service 指向 127.0.0.1:9100（echo 验证服务），
#   无此参则桥接 dispatch 被 SSRF 防线拦（http_handler.rs L266），宪法桥 io
#   往返不可用。生产部署严禁加此参（SSRF 防线全额生效）。
#
# 用法：
#   pwsh -NoProfile -File scripts\start-resident-serve.ps1          # 启动（若已占用则拒绝）
#   pwsh -NoProfile -File scripts\start-resident-serve.ps1 -Force   # 先停旧实例再启动
#
# 前置依赖：
#   - echo 验证服务（echo_server.py @127.0.0.1:9100）需在运行（宪法桥/桥接探针依赖）
#   - 新参数生效需先用 cargo build --release 构建新二进制（脚本不负责构建）
param(
    [switch]$Force
)

$ErrorActionPreference = 'Stop'

$Exe = Join-Path $PSScriptRoot '..\target\release\evorule-server.exe'
$Port = 18080

if (-not (Test-Path $Exe)) {
    Write-Error "release 二进制不存在: $Exe —— 先跑 cargo build --release"
}

# 已有实例处理：默认拒绝重复启动；-Force 时先停旧
$existing = Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue
if ($existing) {
    $oldPid = ($existing | Select-Object -First 1).OwningProcess
    if (-not $Force) {
        Write-Error "端口 $Port 已被 PID $oldPid 占用——如需重启用 -Force（先停旧再启动）"
    }
    Write-Host "停止旧实例 PID $oldPid ..."
    Stop-Process -Id $oldPid -Force
    Start-Sleep -Milliseconds 800
}

# ── 权威启动参数（变更须同步更新本注释块与设计档/登记册留痕）──
$args = @(
    "--addr", "127.0.0.1:18080",
    "--insecure-serve",                                                  # 本地回环无认证（开发形态）
    "--rules-dir", "D:\evorule-server\data\agent-governance\rules",
    "--db-path", "D:\evorule-server\data\agent-governance\evorule.db",
    "--workspace-db", "D:\evorule-server\data\agent-governance\workspace.db",
    "--core-eval", "D:\evorule-server\resources\server_eval.json",
    "--no-rate-limit",
    "--service-registry", "D:\evorule-server\service_registry.json",
    "--io-warn-timeout-secs", "1800",
    "--io-error-timeout-secs", "3600",
    "--allow-loopback",                                                  # caller_role 接线批新增：放行 127.0.0.1:9100 桥接 dispatch（见头部留痕；生产禁用）
    "--log-file", "D:\evorule-server\data\agent-governance\logs\server.log"
)

$p = Start-Process -FilePath $Exe -ArgumentList $args -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 4

$health = Invoke-RestMethod -Uri "http://127.0.0.1:$Port/api/health" -TimeoutSec 8
if ($health.success) {
    Write-Host "OK: 常驻 serve 已启动 PID=$($p.Id) @127.0.0.1:$Port (guard_assembled=$($health.guard_assembled))"
    exit 0
} else {
    Write-Error "启动后 health 检查未通过: $($health | ConvertTo-Json -Compress)"
}

# SPDX-License-Identifier: AGPL-3.0-or-later
# start-resident-serve.ps1 — evorule-server 常驻实例启动脚本（通用开发辅助）
#
# 【适用范围说明】（2026-09-29 更新）：
#   本脚本面向「其他开发环境/首次部署」的通用启动辅助。
#   若你的环境使用 evo-agent ops 运维脚本族（scripts/ops/watchdog-check.ps1
#   计划任务看门狗 + ops.local.json 配置），则本机常驻实例的启动参数权威源
#   为 ops.local.json 的 services.evorule-server.args，重启一律交给看门狗
#   或 ops 族的 start-evorule-server.ps1 —— 两处参数表需人工保持一致，
#   变更时同步修改（参数语义见下方注释块）。
#
# 【--allow-loopback 参数变更留痕】（2026-09-29 caller_role 接线批）：
#   本地开发约定（main.rs L786 注释语义，与已开的 --insecure-serve 同族）：
#   service_registry.json 的 call-service 指向 127.0.0.1:9100（echo 验证服务），
#   无此参则桥接 dispatch 被 SSRF 防线拦（http_handler.rs L266），宪法桥 io
#   往返不可用。生产部署严禁加此参（SSRF 防线全额生效）。
#
# 【--wal-dir 参数变更留痕】（2026-09-29 审计链持久化修复批）：
#   会话审计链（Command/IoRequest/IoResponse/判定/payload_update 事实）此前
#   为纯内存模式（会话 TTL 1800s 过期即失、重启清零）。--wal-dir 指向全新
#   目录（不复用旧 data\wal，避免启动时 WAL 恢复复活陈旧共享事实）。
#   生产部署同样应配置 --wal-dir（或 EVORULE_WAL_DIR / paths.wal_dir）。
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
    "--wal-dir", "D:\evorule-server\data\wal-live",                      # 审计链持久化修复批新增：会话事实 WAL 落盘（见头部留痕；勿指向含旧 WAL 的目录）
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

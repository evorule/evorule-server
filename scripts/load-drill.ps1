# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 EvoRule Project
<#
.SYNOPSIS
    UV-032 W3 负载演练:多用户并发长时运行(server 真实 API)

.DESCRIPTION
    N 个并发用户循环执行 建会话→K 条命令→读审计档案,持续指定时长。
    产出:每用户 会话数/命令数/错误数/命令时延统计(p50/p95/max)。
    用法:
      powershell -ExecutionPolicy Bypass -File scripts\load-drill.ps1 -Minutes 30 -Users 3
#>
[CmdletBinding()]
param(
    [int]$Minutes = 30,
    [int]$Users = 3,
    [int]$Port = 18290,
    [string]$ServerExe = "D:\evorule-server\target\release\evorule-server.exe",
    [string]$RepoRoot = "D:\evorule-server"
)

$ErrorActionPreference = 'Stop'
$tmp = Join-Path $env:TEMP "uv032-drill-$(Get-Random)"
New-Item -ItemType Directory -Path "$tmp\wal" -Force | Out-Null

# ---- 拉起被测 server(独立端口+独立数据目录,不碰开发环境) ----
# 注:不传 --web-dir(演练无需静态托管)与 --plugins(2026-09-01 对齐当前 CLI,清单可选且仓内无该文件)
$args = @('--addr', "127.0.0.1:$Port",
    '--rules-dir', 'rules',
    '--service-registry', 'service_registry.json',
    '--wal-dir', "$tmp\wal")
$p = Start-Process -FilePath $ServerExe -ArgumentList $args -WorkingDirectory $RepoRoot `
    -WindowStyle Hidden -RedirectStandardOutput "$tmp\out.log" -RedirectStandardError "$tmp\err.log" -PassThru

$ready = $false
for ($i = 0; $i -lt 60; $i++) {
    try {
        $r = Invoke-WebRequest -Uri "http://127.0.0.1:$Port/api/health" -UseBasicParsing -TimeoutSec 2
        if ($r.StatusCode -eq 200) { $ready = $true; break }
    } catch { Start-Sleep -Milliseconds 500 }
}
if (-not $ready) { Write-Host "[FAIL] server 未就绪,见 $tmp\out.log"; exit 1 }
Write-Host "[OK] server 就绪(:$Port,wal=$tmp\wal);演练 $Users 用户 x $Minutes 分钟"

$api = "http://127.0.0.1:$Port"
$deadline = (Get-Date).AddMinutes($Minutes)

# ---- 每用户一个 job,循环到 deadline ----
$jobs = @()
for ($u = 1; $u -le $Users; $u++) {
    $jobs += Start-Job -Name "drill-$u" -ArgumentList $api, $deadline, $u -ScriptBlock {
        param($api, $deadline, $u)
        $sessions = 0; $cmds = 0; $errors = 0
        $lat = New-Object System.Collections.Generic.List[double]
        while ((Get-Date) -lt $deadline) {
            try {
                $resp = Invoke-RestMethod -Uri "$api/api/sessions" -Method Post -Body '{}' -ContentType 'application/json' -TimeoutSec 10
                $sid = if ($resp.session_id) { $resp.session_id } elseif ($resp.session_new) { $resp.session_new } else { $resp.id }
                $sessions++
                for ($k = 1; $k -le 5; $k++) {
                    if ((Get-Date) -ge $deadline) { break }
                    $body = '{"instruction":{"type":"set","params":{"attr":"drill_u' + $u + '_k' + $k + '","operation":"set","value":' + $k + '}}}'
                    $sw = [Diagnostics.Stopwatch]::StartNew()
                    $null = Invoke-RestMethod -Uri "$api/api/sessions/$sid/command" -Method Post -Body $body -ContentType 'application/json' -TimeoutSec 10
                    $sw.Stop()
                    $lat.Add($sw.Elapsed.TotalMilliseconds)
                    $cmds++
                }
                # 周期性读审计档案(只读面也纳入负载)
                if ($sessions % 10 -eq 0) {
                    $null = Invoke-RestMethod -Uri "$api/api/audit-archive/sessions" -TimeoutSec 10
                }
            } catch { $errors++ ; Start-Sleep -Milliseconds 200 }
        }
        $sorted = $lat | Sort-Object
        $p50 = if ($sorted.Count -gt 0) { $sorted[[int][math]::Floor($sorted.Count * 0.5)] } else { 0 }
        $p95 = if ($sorted.Count -gt 0) { $sorted[[int][math]::Floor($sorted.Count * 0.95)] } else { 0 }
        $max = if ($sorted.Count -gt 0) { $sorted[$sorted.Count - 1] } else { 0 }
        [pscustomobject]@{ user = $u; sessions = $sessions; commands = $cmds; errors = $errors; p50_ms = [math]::Round($p50, 1); p95_ms = [math]::Round($p95, 1); max_ms = [math]::Round($max, 1) }
    }
}

# ---- 等待并汇总 ----
$total = [pscustomobject]@{ sessions = 0; commands = 0; errors = 0 }
foreach ($j in $jobs) {
    $r = Receive-Job -Job $j -Wait
    Remove-Job -Job $j
    Write-Host ("  用户{0}: 会话 {1} | 命令 {2} | 错误 {3} | 命令时延 p50={4}ms p95={5}ms max={6}ms" -f $r.user, $r.sessions, $r.commands, $r.errors, $r.p50_ms, $r.p95_ms, $r.max_ms)
    $total.sessions += $r.sessions; $total.commands += $r.commands; $total.errors += $r.errors
}

# ---- 收尾断言与结构健康复核 ----
try {
    $arch = Invoke-RestMethod -Uri "$api/api/audit-archive/sessions" -TimeoutSec 15
    Write-Host ("[OK] 演练后审计档案会话数: {0}" -f $arch.sessions.Count)
} catch { Write-Host "[WARN] 审计档案读取失败: $($_.Exception.Message)" }

try {
    $m = Invoke-WebRequest -Uri "$api/metrics" -UseBasicParsing -TimeoutSec 10
    $viol = ($m.Content -split "`n" | Where-Object { $_ -match 'structural_invariant_violations' })
    Write-Host ("[OK] 结构不变量指标: {0}" -f ($viol -join '; ').Trim())
} catch { Write-Host "[WARN] /metrics 读取失败: $($_.Exception.Message)" }

Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
Write-Host ("===== 演练汇总: 会话 {0} | 命令 {1} | 错误 {2} =====" -f $total.sessions, $total.commands, $total.errors)
Write-Host ("数据目录保留供排查: $tmp")

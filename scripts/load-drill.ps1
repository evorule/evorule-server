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
# 防呆(2026-09-01 第四轮教训):本脚本拉起的进程若绑定失败(AddrInUse,见 err.log),
# 健康检查会误连**遗留的旧 server**——30 分钟演练数据全部作废(打错对象:
# 旧 server 自带历史泄漏会话,快速撞满 1000 上限引发 429 风暴,纯属假阳性)。
# 故就绪后必须校验端口监听者就是本脚本拉起的 pid,否则硬失败退出。
if ($p.HasExited) {
    Write-Host "[FAIL] 拉起的 server 进程已退出(疑似端口被占,见 $tmp\err.log),健康检查命中的是遗留实例——中止,不产生无效数据"
    exit 1
}
$listener = Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue |
    Where-Object { $_.OwningProcess -eq $p.Id }
if (-not $listener) {
    Write-Host "[FAIL] 端口 $Port 的监听者不是本脚本拉起的进程(pid=$($p.Id)),疑似遗留 server——请先清理后重跑"
    Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
    exit 1
}
Write-Host "[OK] server 就绪(:$Port,pid=$($p.Id),wal=$tmp\wal);演练 $Users 用户 x $Minutes 分钟"

$api = "http://127.0.0.1:$Port"
$deadline = (Get-Date).AddMinutes($Minutes)

# ---- 每用户一个 job,循环到 deadline ----
$jobs = @()
for ($u = 1; $u -le $Users; $u++) {
    $jobs += Start-Job -Name "drill-$u" -ArgumentList $api, $deadline, $u -ScriptBlock {
        param($api, $deadline, $u)
        $sessions = 0; $cmds = 0; $errors = 0; $closed = 0
        # 错误分类统计:定位错误来源(限流 429/客户端 4xx/服务端 5xx/超时与传输)
        $err429 = 0; $err4xx = 0; $err5xx = 0; $errNet = 0
        $lat = New-Object System.Collections.Generic.List[double]
        while ((Get-Date) -lt $deadline) {
            $sid = $null
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
                    # 模拟真实用户操作节奏(50ms/命令):第三轮教训——全速循环瞬时
                    # 速率远超 200 req/s 限流,429 刷屏淹没健康指标。3 用户带间隔
                    # 后 ≈12 req/s,远低于限额,429 应趋零,错误率才是有效健康信号。
                    Start-Sleep -Milliseconds 50
                }
                # 周期性读审计档案(只读面也纳入负载)
                if ($sessions % 10 -eq 0) {
                    $null = Invoke-RestMethod -Uri "$api/api/audit-archive/sessions" -TimeoutSec 10
                }
                # 会话轮换:用完即关(DELETE),模拟真实多用户生命周期。
                # UV-032 首轮教训:不关会话 ~12 分钟耗尽 max=1000 活跃会话上限,
                # 其余时间全部是被上限拒绝的无效错误。
                $null = Invoke-RestMethod -Uri "$api/api/sessions/$sid" -Method Delete -TimeoutSec 10
                $closed++
                $sid = $null
            } catch {
                $errors++
                # 错误归因(第二轮教训:25000 错误不可归因=无效数据)
                $code = try { [int]$_.Exception.Response.StatusCode } catch { 0 }
                if ($code -eq 429) { $err429++ } elseif ($code -ge 500) { $err5xx++ } elseif ($code -ge 400) { $err4xx++ } else { $errNet++ }
                # 泄漏防护:catch 路径若已建会话必须尽力关闭。
                # (第二轮教训:命令异常进 catch 时不关会话,234 个泄漏累积撞满
                #  1000 上限,后续创建全部失败,错误被上限拒绝淹没)
                if ($sid) {
                    try { $null = Invoke-RestMethod -Uri "$api/api/sessions/$sid" -Method Delete -TimeoutSec 10; $closed++ } catch { }
                }
                Start-Sleep -Milliseconds 200
            }
        }
        $sorted = $lat | Sort-Object
        $p50 = if ($sorted.Count -gt 0) { $sorted[[int][math]::Floor($sorted.Count * 0.5)] } else { 0 }
        $p95 = if ($sorted.Count -gt 0) { $sorted[[int][math]::Floor($sorted.Count * 0.95)] } else { 0 }
        $max = if ($sorted.Count -gt 0) { $sorted[$sorted.Count - 1] } else { 0 }
        [pscustomobject]@{ user = $u; sessions = $sessions; commands = $cmds; closed = $closed; errors = $errors;
            err429 = $err429; err4xx = $err4xx; err5xx = $err5xx; errNet = $errNet;
            p50_ms = [math]::Round($p50, 1); p95_ms = [math]::Round($p95, 1); max_ms = [math]::Round($max, 1) }
    }
}

# ---- 等待并汇总 ----
$total = [pscustomobject]@{ sessions = 0; commands = 0; errors = 0; err429 = 0; err4xx = 0; err5xx = 0; errNet = 0 }
foreach ($j in $jobs) {
    $r = Receive-Job -Job $j -Wait
    Remove-Job -Job $j
    Write-Host ("  用户{0}: 会话 {1} | 命令 {2} | 关闭 {3} | 错误 {4} (429:{5} 4xx:{6} 5xx:{7} 网络:{8}) | 命令时延 p50={9}ms p95={10}ms max={11}ms" -f $r.user, $r.sessions, $r.commands, $r.closed, $r.errors, $r.err429, $r.err4xx, $r.err5xx, $r.errNet, $r.p50_ms, $r.p95_ms, $r.max_ms)
    $total.sessions += $r.sessions; $total.commands += $r.commands; $total.errors += $r.errors
    $total.err429 += $r.err429; $total.err4xx += $r.err4xx; $total.err5xx += $r.err5xx; $total.errNet += $r.errNet
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
Write-Host ("===== 演练汇总: 会话 {0} | 命令 {1} | 错误 {2} (429:{3} 4xx:{4} 5xx:{5} 网络:{6}) =====" -f $total.sessions, $total.commands, $total.errors, $total.err429, $total.err4xx, $total.err5xx, $total.errNet)
Write-Host ("数据目录保留供排查: $tmp")

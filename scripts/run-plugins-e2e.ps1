# 插件清单端到端验收脚本（后端插件清单化，15 号实施计划 W4）
#
# 验收链路：plugin_manifest.json（--plugins）→ 启动期 fail-fast 校验 →
#   挂载语义（全启/子集/停用）→ /api/health plugins 节如实呈现。
#
# 覆盖：
#   1. demo-services 单测（with_enabled 过滤/拒绝语义 + 原生服务实现）
#   2. main.rs 清单解析单测（三级配置/缺省全启/未知插件 id/空集）
#   3. tests/plugins_e2e.rs 组件级 E2E（子集命中/未启用如实报错/声明序确定性）
#   4. 真实二进制四场景：
#      A 缺省启动（无清单）→ health 显示全部 7 服务
#      B 子集清单        → health 仅显示启用子集
#      C 停用清单        → health enabled=false
#      D 非法清单（未知名）→ 启动 fail-fast 退出 + 自诊断指引
#
# 用法：
#   pwsh ./scripts/run-plugins-e2e.ps1
#
# 退出码 0 = 验收通过；非 0 = 存在失败项（如实退出，不静默）。

[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repoRoot

$failures = 0

Write-Host "=== [1/4] demo-services 单测 ===" -ForegroundColor Cyan
cargo test -p evorule-demo-services --lib
if ($LASTEXITCODE -ne 0) { $failures++; Write-Host "[FAIL] demo-services 单测未通过" -ForegroundColor Red }
else { Write-Host "[OK] demo-services 单测通过" -ForegroundColor Green }

Write-Host "=== [2/4] 插件清单解析单测（main.rs）===" -ForegroundColor Cyan
cargo test -p evorule-server --bin evorule-server plugin_
if ($LASTEXITCODE -ne 0) { $failures++; Write-Host "[FAIL] 清单解析单测未通过" -ForegroundColor Red }
else { Write-Host "[OK] 清单解析单测通过" -ForegroundColor Green }

Write-Host "=== [3/4] 组件级端到端（tests/plugins_e2e.rs）===" -ForegroundColor Cyan
cargo test -p evorule-server --test plugins_e2e
if ($LASTEXITCODE -ne 0) { $failures++; Write-Host "[FAIL] plugins_e2e 未通过" -ForegroundColor Red }
else { Write-Host "[OK] plugins_e2e 通过" -ForegroundColor Green }

# ---- [4/4] 真实二进制四场景 ----
Write-Host "=== [4/4] 真实二进制场景（构建 debug 版）===" -ForegroundColor Cyan
cargo build -p evorule-server --bin evorule-server
if ($LASTEXITCODE -ne 0) { $failures++; Write-Host "[FAIL] evorule-server 构建失败" -ForegroundColor Red }
else {
    $exe = Join-Path $repoRoot "target\debug\evorule-server.exe"
    $tmp = Join-Path $env:TEMP "evorule-plugins-e2e-$(Get-Random)"
    New-Item -ItemType Directory -Path (Join-Path $tmp "rules") -Force | Out-Null
    $coreEval = Join-Path $repoRoot "resources\core_eval.json"

    # 三份清单（子集/停用/非法未知名——sampling 为真实原生服务的近似拼写,
    # 实名是 sampling_service,故意触发 with_enabled 未知名 fail-fast）
    $subsetM = Join-Path $tmp "subset.json"
    $offM    = Join-Path $tmp "off.json"
    $badM    = Join-Path $tmp "bad.json"
    '{ "plugins": { "demo-services": { "enabled": true, "services": ["config_persist"] } } }' |
        Set-Content -Path $subsetM -Encoding UTF8
    '{ "plugins": { "demo-services": { "enabled": false } } }' |
        Set-Content -Path $offM -Encoding UTF8
    '{ "plugins": { "demo-services": { "enabled": true, "services": ["sampling"] } } }' |
        Set-Content -Path $badM -Encoding UTF8

    # 等待 /api/health 就绪（最长 60s）
    function Wait-Health([int]$Port) {
        $deadline = (Get-Date).AddSeconds(60)
        while ((Get-Date) -lt $deadline) {
            try {
                $h = Invoke-RestMethod -Uri "http://127.0.0.1:$Port/api/health" -TimeoutSec 2
                if ($h.success) { return $h }
            } catch { Start-Sleep -Milliseconds 500 }
        }
        return $null
    }

    function Assert-HealthPlugins($h, [string]$Scenario, [bool]$Enabled, $Services) {
        if ($null -eq $h) {
            Write-Host "[FAIL] $Scenario : /api/health 60s 内未就绪" -ForegroundColor Red
            $script:failures++
            return
        }
        $p = $h.plugins
        if ($null -eq $p -or $null -eq $p.'demo-services') {
            Write-Host "[FAIL] $Scenario : health 响应缺 plugins.demo-services 节: $($h | ConvertTo-Json -Depth 5)" -ForegroundColor Red
            $script:failures++
            return
        }
        $d = $p.'demo-services'
        if ([bool]$d.enabled -ne $Enabled) {
            Write-Host "[FAIL] $Scenario : enabled 应为 $Enabled,实际 $($d.enabled)" -ForegroundColor Red
            $script:failures++
            return
        }
        if ($null -ne $Services) {
            $actual = @($d.services)
            if ($actual.Count -ne @($Services).Count) {
                Write-Host "[FAIL] $Scenario : services 数应 $($Services.Count),实际 $($actual.Count)" -ForegroundColor Red
                $script:failures++
                return
            }
            foreach ($s in $Services) {
                if ($actual -notcontains $s) {
                    Write-Host "[FAIL] $Scenario : services 缺 '$s'" -ForegroundColor Red
                    $script:failures++
                    return
                }
            }
        }
        elseif ($null -ne $d.services) {
            Write-Host "[FAIL] $Scenario : 停用时不应输出 services 节" -ForegroundColor Red
            $script:failures++
            return
        }
        Write-Host "[OK] $Scenario : health plugins 节符合预期" -ForegroundColor Green
    }

    function Start-Server([int]$Port, [string]$Manifest) {
        $args = @(
            "--addr", "127.0.0.1:$Port",
            "--rules-dir", (Join-Path $tmp "rules"),
            "--core-eval", $coreEval
        )
        if ($Manifest -ne "") { $args += @("--plugins", $Manifest) }
        $out = Join-Path $tmp "server-$Port.out.log"
        $err = Join-Path $tmp "server-$Port.err.log"
        return Start-Process -FilePath $exe -ArgumentList $args -WorkingDirectory $tmp `
            -WindowStyle Hidden -PassThru -RedirectStandardOutput $out -RedirectStandardError $err
    }

    # 场景 A：缺省启动（无清单）= 全部 7 服务
    $procA = Start-Server 18091 ""
    try {
        $hA = Wait-Health 18091
        Assert-HealthPlugins $hA "场景A 缺省全启" $true @(
            "inverse_kinematics_solver", "robot_move_joints", "llm_advisor",
            "shadow_ik_solver", "sampling_service", "rule_sandbox", "config_persist"
        )
    } finally { if (-not $procA.HasExited) { Stop-Process -Id $procA.Id -Force } }

    # 场景 B：子集清单
    $procB = Start-Server 18092 $subsetM
    try {
        $hB = Wait-Health 18092
        Assert-HealthPlugins $hB "场景B 子集启用" $true @("config_persist")
    } finally { if (-not $procB.HasExited) { Stop-Process -Id $procB.Id -Force } }

    # 场景 C：停用清单
    $procC = Start-Server 18093 $offM
    try {
        $hC = Wait-Health 18093
        Assert-HealthPlugins $hC "场景C 插件停用" $false $null
    } finally { if (-not $procC.HasExited) { Stop-Process -Id $procC.Id -Force } }

    # 场景 D：非法清单 → 启动 fail-fast（退出码非 0 + 自诊断指引）
    $errD = Join-Path $tmp "server-failfast.err.log"
    $procD = Start-Process -FilePath $exe `
        -ArgumentList @("--addr", "127.0.0.1:18094", "--rules-dir", (Join-Path $tmp "rules"),
                        "--core-eval", $coreEval, "--plugins", $badM) `
        -WorkingDirectory $tmp -WindowStyle Hidden -PassThru `
        -RedirectStandardOutput (Join-Path $tmp "server-failfast.out.log") -RedirectStandardError $errD
    $deadline = (Get-Date).AddSeconds(30)
    while (-not $procD.HasExited -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 300 }
    if (-not $procD.HasExited) {
        Stop-Process -Id $procD.Id -Force
        Write-Host "[FAIL] 场景D 非法清单: server 未在 30s 内 fail-fast 退出" -ForegroundColor Red
        $failures++
    }
    elseif ($procD.ExitCode -eq 0) {
        Write-Host "[FAIL] 场景D 非法清单: 退出码 0（应非 0 fail-fast）" -ForegroundColor Red
        $failures++
    }
    else {
        $errText = (Get-Content $errD -Raw -ErrorAction SilentlyContinue) + (Get-Content (Join-Path $tmp "server-failfast.out.log") -Raw -ErrorAction SilentlyContinue)
        if ($errText -match "自诊断指引" -and $errText -match "合法服务名") {
            Write-Host "[OK] 场景D 非法清单: 启动 fail-fast（退出码 $($procD.ExitCode)）+ 自诊断指引" -ForegroundColor Green
        } else {
            Write-Host "[FAIL] 场景D: 错误输出缺自诊断指引/合法服务名: $errText" -ForegroundColor Red
            $failures++
        }
    }

    # 清理临时目录（本脚本自建产物）
    Start-Sleep -Milliseconds 500
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}

if ($failures -gt 0) {
    Write-Host "`n插件清单端到端验收失败：$failures 项未通过" -ForegroundColor Red
    exit 1
}
Write-Host "`n插件清单端到端验收全部通过" -ForegroundColor Green

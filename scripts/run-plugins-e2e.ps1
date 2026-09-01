# 插件清单端到端验收脚本（后端插件清单化，15 号实施计划 W4；UV-035 泛化至双插件；
# UV-037 泛化至三插件）
#
# 验收链路：plugin_manifest.json（--plugins）→ 启动期 fail-fast 校验 →
#   挂载语义（全启/子集/停用）→ /api/health plugins 节如实呈现。
#
# 覆盖：
#   1. demo-services / physics-services / indicator-services 单测
#      （with_enabled 过滤/拒绝语义 + 原生服务实现）
#   2. main.rs 清单解析单测（三级配置/缺省全启/未知插件 id/空集）
#   3. tests/plugins_e2e.rs 组件级 E2E（子集命中/未启用如实报错/声明序确定性
#      + UV-035 双插件链 + UV-037 三插件链：原生命中/穿透回落/链序正确性）
#   4. 真实二进制五场景：
#      A 缺省启动（无清单）→ health 显示 demo 7 + physics 3 + indicator 4 服务（三插件全启）
#      B 三插件子集清单    → health 仅显示各插件启用子集
#      C 三插件停用清单    → health 三 enabled=false
#      D 混合清单（demo 停用 + physics 子集 + indicator 子集）→ 插件间互不干扰
#      E 非法清单（physics 未知名）→ 启动 fail-fast 退出 + 自诊断指引
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

Write-Host "=== [1/5] demo-services / physics-services / indicator-services 单测 ===" -ForegroundColor Cyan
cargo test -p evorule-demo-services --lib
if ($LASTEXITCODE -ne 0) { $failures++; Write-Host "[FAIL] demo-services 单测未通过" -ForegroundColor Red }
else { Write-Host "[OK] demo-services 单测通过" -ForegroundColor Green }
cargo test -p evorule-physics-services --lib
if ($LASTEXITCODE -ne 0) { $failures++; Write-Host "[FAIL] physics-services 单测未通过" -ForegroundColor Red }
else { Write-Host "[OK] physics-services 单测通过" -ForegroundColor Green }
cargo test -p evorule-indicator-services --lib
if ($LASTEXITCODE -ne 0) { $failures++; Write-Host "[FAIL] indicator-services 单测未通过" -ForegroundColor Red }
else { Write-Host "[OK] indicator-services 单测通过" -ForegroundColor Green }

Write-Host "=== [2/5] 插件清单解析单测（main.rs）===" -ForegroundColor Cyan
cargo test -p evorule-server --bin evorule-server plugin_
if ($LASTEXITCODE -ne 0) { $failures++; Write-Host "[FAIL] 清单解析单测未通过" -ForegroundColor Red }
else { Write-Host "[OK] 清单解析单测通过" -ForegroundColor Green }

Write-Host "=== [3/5] 组件级端到端（tests/plugins_e2e.rs，含 UV-035 双插件链 + UV-037 三插件链）===" -ForegroundColor Cyan
cargo test -p evorule-server --test plugins_e2e
if ($LASTEXITCODE -ne 0) { $failures++; Write-Host "[FAIL] plugins_e2e 未通过" -ForegroundColor Red }
else { Write-Host "[OK] plugins_e2e 通过" -ForegroundColor Green }

# ---- [4/5] 构建真实二进制 ----
Write-Host "=== [4/5] 真实二进制场景（构建 debug 版）===" -ForegroundColor Cyan
cargo build -p evorule-server --bin evorule-server
if ($LASTEXITCODE -ne 0) { $failures++; Write-Host "[FAIL] evorule-server 构建失败" -ForegroundColor Red }
else {
    $exe = Join-Path $repoRoot "target\debug\evorule-server.exe"
    $tmp = Join-Path $env:TEMP "evorule-plugins-e2e-$(Get-Random)"
    New-Item -ItemType Directory -Path (Join-Path $tmp "rules") -Force | Out-Null
    $coreEval = Join-Path $repoRoot "resources\core_eval.json"

    # 四份清单（三插件子集/三插件停用/混合/非法未知名——sampling 为 demo 真实
    # 原生服务的近似拼写,实名是 sampling_service;physics 场景用 physics_energy
    # 缺尾 e 的 physics_energ,故意触发 with_enabled 未知名 fail-fast）
    $subsetM = Join-Path $tmp "subset.json"
    $offM    = Join-Path $tmp "off.json"
    $mixM    = Join-Path $tmp "mix.json"
    $badM    = Join-Path $tmp "bad.json"
    '{ "plugins": { "demo-services": { "enabled": true, "services": ["config_persist"] }, "physics-services": { "enabled": true, "services": ["physics_energy"] }, "indicator-services": { "enabled": true, "services": ["indicator_sma"] } } }' |
        Set-Content -Path $subsetM -Encoding UTF8
    '{ "plugins": { "demo-services": { "enabled": false }, "physics-services": { "enabled": false }, "indicator-services": { "enabled": false } } }' |
        Set-Content -Path $offM -Encoding UTF8
    '{ "plugins": { "demo-services": { "enabled": false }, "physics-services": { "enabled": true, "services": ["physics_simulate"] }, "indicator-services": { "enabled": true, "services": ["indicator_sma", "indicator_rsi"] } } }' |
        Set-Content -Path $mixM -Encoding UTF8
    '{ "plugins": { "demo-services": { "enabled": false }, "physics-services": { "enabled": true, "services": ["physics_energ"] }, "indicator-services": { "enabled": false } } }' |
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

    # 校验指定插件节（enabled/services;Services=$null 时要求停用且无 services 节）
    function Assert-HealthPlugins($h, [string]$Scenario, [string]$PluginId, [bool]$Enabled, $Services) {
        if ($null -eq $h) {
            Write-Host "[FAIL] $Scenario : /api/health 60s 内未就绪" -ForegroundColor Red
            $script:failures++
            return
        }
        $p = $h.plugins
        if ($null -eq $p -or $null -eq $p.$PluginId) {
            Write-Host "[FAIL] $Scenario : health 响应缺 plugins.$PluginId 节: $($h | ConvertTo-Json -Depth 5)" -ForegroundColor Red
            $script:failures++
            return
        }
        $d = $p.$PluginId
        if ([bool]$d.enabled -ne $Enabled) {
            Write-Host "[FAIL] $Scenario : $PluginId enabled 应为 $Enabled,实际 $($d.enabled)" -ForegroundColor Red
            $script:failures++
            return
        }
        if ($null -ne $Services) {
            $actual = @($d.services)
            if ($actual.Count -ne @($Services).Count) {
                Write-Host "[FAIL] $Scenario : $PluginId services 数应 $($Services.Count),实际 $($actual.Count)" -ForegroundColor Red
                $script:failures++
                return
            }
            foreach ($s in $Services) {
                if ($actual -notcontains $s) {
                    Write-Host "[FAIL] $Scenario : $PluginId services 缺 '$s'" -ForegroundColor Red
                    $script:failures++
                    return
                }
            }
        }
        elseif ($null -ne $d.services) {
            Write-Host "[FAIL] $Scenario : $PluginId 停用时不应输出 services 节" -ForegroundColor Red
            $script:failures++
            return
        }
        Write-Host "[OK] $Scenario : health plugins.$PluginId 节符合预期" -ForegroundColor Green
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

    # 场景 A：缺省启动（无清单）= demo 全部 7 + physics 全部 3 + indicator 全部 4 服务
    $procA = Start-Server 18091 ""
    try {
        $hA = Wait-Health 18091
        Assert-HealthPlugins $hA "场景A 缺省全启" "demo-services" $true @(
            "inverse_kinematics_solver", "robot_move_joints", "llm_advisor",
            "shadow_ik_solver", "sampling_service", "rule_sandbox", "config_persist"
        )
        Assert-HealthPlugins $hA "场景A 缺省全启" "physics-services" $true @(
            "physics_simulate", "physics_energy", "physics_grav_band"
        )
        Assert-HealthPlugins $hA "场景A 缺省全启" "indicator-services" $true @(
            "indicator_sma", "indicator_ema", "indicator_macd", "indicator_rsi"
        )
    } finally { if (-not $procA.HasExited) { Stop-Process -Id $procA.Id -Force } }

    # 场景 B：三插件子集清单
    $procB = Start-Server 18092 $subsetM
    try {
        $hB = Wait-Health 18092
        Assert-HealthPlugins $hB "场景B 三插件子集" "demo-services" $true @("config_persist")
        Assert-HealthPlugins $hB "场景B 三插件子集" "physics-services" $true @("physics_energy")
        Assert-HealthPlugins $hB "场景B 三插件子集" "indicator-services" $true @("indicator_sma")
    } finally { if (-not $procB.HasExited) { Stop-Process -Id $procB.Id -Force } }

    # 场景 C：三插件停用清单
    $procC = Start-Server 18093 $offM
    try {
        $hC = Wait-Health 18093
        Assert-HealthPlugins $hC "场景C 三插件停用" "demo-services" $false $null
        Assert-HealthPlugins $hC "场景C 三插件停用" "physics-services" $false $null
        Assert-HealthPlugins $hC "场景C 三插件停用" "indicator-services" $false $null
    } finally { if (-not $procC.HasExited) { Stop-Process -Id $procC.Id -Force } }

    # 场景 D：混合清单（demo 停用 + physics 子集 + indicator 子集）→ 插件间互不干扰
    $procD = Start-Server 18094 $mixM
    try {
        $hD = Wait-Health 18094
        Assert-HealthPlugins $hD "场景D 混合清单" "demo-services" $false $null
        Assert-HealthPlugins $hD "场景D 混合清单" "physics-services" $true @("physics_simulate")
        Assert-HealthPlugins $hD "场景D 混合清单" "indicator-services" $true @("indicator_sma", "indicator_rsi")
    } finally { if (-not $procD.HasExited) { Stop-Process -Id $procD.Id -Force } }

    # 场景 E：非法清单 → 启动 fail-fast（退出码非 0 + 自诊断指引）
    $errE = Join-Path $tmp "server-failfast.err.log"
    $procE = Start-Process -FilePath $exe `
        -ArgumentList @("--addr", "127.0.0.1:18095", "--rules-dir", (Join-Path $tmp "rules"),
                        "--core-eval", $coreEval, "--plugins", $badM) `
        -WorkingDirectory $tmp -WindowStyle Hidden -PassThru `
        -RedirectStandardOutput (Join-Path $tmp "server-failfast.out.log") -RedirectStandardError $errE
    $deadline = (Get-Date).AddSeconds(30)
    while (-not $procE.HasExited -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 300 }
    if (-not $procE.HasExited) {
        Stop-Process -Id $procE.Id -Force
        Write-Host "[FAIL] 场景E 非法清单: server 未在 30s 内 fail-fast 退出" -ForegroundColor Red
        $failures++
    }
    elseif ($procE.ExitCode -eq 0) {
        Write-Host "[FAIL] 场景E 非法清单: 退出码 0（应非 0 fail-fast）" -ForegroundColor Red
        $failures++
    }
    else {
        $errText = (Get-Content $errE -Raw -ErrorAction SilentlyContinue) + (Get-Content (Join-Path $tmp "server-failfast.out.log") -Raw -ErrorAction SilentlyContinue)
        if ($errText -match "自诊断指引" -and $errText -match "合法服务名") {
            Write-Host "[OK] 场景E 非法清单: 启动 fail-fast（退出码 $($procE.ExitCode)）+ 自诊断指引" -ForegroundColor Green
        } else {
            Write-Host "[FAIL] 场景E: 错误输出缺自诊断指引/合法服务名: $errText" -ForegroundColor Red
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

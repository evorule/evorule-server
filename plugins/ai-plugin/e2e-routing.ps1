# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 EvoRule Project
# This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
# ai-plugin 多 provider 路由脚本级 e2e（独立于 e2e.ps1：不需要 evorule-server，
# 仅插件进程 + 双 mock 上游）。
# 断言:
#   ① 路由模式 /llm/models: 清单=白名单∩已装载路由, 未路由模型随 note 显式可见
#   ② 按模型路由: mock-a → 上游1, mock-b → 上游2, 各用各的 provider 凭据
#   ③ default_params 打底且请求体同键字段优先
#   ④ 边界: 未路由模型/白名单外模型/缺 model 字段均显式拒, 无静默回落
#   ⑤ 遗留单端点模式回归: 不配 llm_providers 时行为与既有形态一致
# 用法: powershell -ExecutionPolicy Bypass -File e2e-routing.ps1
# 退出码: 0=全过 1=失败

$ErrorActionPreference = "Stop"

$PluginExe = Join-Path $PSScriptRoot "target\debug\evorule-ai-plugin.exe"
$PluginPort = 19130
$PluginPortLegacy = 19131
$Up1Port = 19191
$Up2Port = 19192
$KeyEnvA = "EVORULE_AI_PLUGIN_E2E_KEY_A"
$KeyEnvB = "EVORULE_AI_PLUGIN_E2E_KEY_B"

$Failures = New-Object System.Collections.Generic.List[string]

function Assert-True($Cond, $Name) {
    if ($Cond) { Write-Host "  [PASS] $Name" }
    else { Write-Host "  [FAIL] $Name"; $script:Failures.Add($Name) | Out-Null }
}

function Write-Utf8NoBom($Path, $Text) {
    [System.IO.File]::WriteAllText($Path, $Text, [System.Text.UTF8Encoding]::new($false))
}

# ---------- 环境预检 ----------
if (-not (Test-Path $PluginExe)) { Write-Host "ai-plugin 二进制缺失: $PluginExe（先在本目录 cargo build）"; exit 1 }
foreach ($p in @($PluginPort, $PluginPortLegacy, $Up1Port, $Up2Port)) {
    $used = Get-NetTCPConnection -LocalPort $p -State Listen -ErrorAction SilentlyContinue
    if ($used) { Write-Host "端口 $p 已被占用，e2e 无法启动"; exit 1 }
}

$Tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("ai-plugin-route-e2e-" + [guid]::NewGuid().ToString("N").Substring(0, 8))
New-Item -ItemType Directory -Path $Tmp | Out-Null

$up1Job = $null; $up2Job = $null; $p1 = $null; $p2 = $null
try {
    # ---------- 1. mock 上游 1：回显 temperature 与 Authorization（验证路由+缺省参数+专属凭据） ----------
    $up1Job = Start-Job -ArgumentList $Up1Port -ScriptBlock {
        param($port)
        $l = [System.Net.HttpListener]::new()
        $l.Prefixes.Add("http://127.0.0.1:$port/")
        $l.Start()
        while ($l.IsListening) {
            $ctx = $l.GetContext()
            try {
                $raw = [System.IO.StreamReader]::new($ctx.Request.InputStream).ReadToEnd()
                $req = $raw | ConvertFrom-Json
                $auth = $ctx.Request.Headers["Authorization"]
                $body = "{`"choices`":[{`"message`":{`"content`":`"up1;t=$($req.temperature);auth=$auth`"}}]}"
                $buf = [System.Text.Encoding]::UTF8.GetBytes($body)
                $ctx.Response.ContentType = "application/json"
                $ctx.Response.StatusCode = 200
                $ctx.Response.OutputStream.Write($buf, 0, $buf.Length)
                $ctx.Response.OutputStream.Close()
            } catch { try { $ctx.Response.Abort() } catch {} }
        }
    }

    # ---------- 2. mock 上游 2：固定回复（区分路由命中） ----------
    $up2Job = Start-Job -ArgumentList $Up2Port -ScriptBlock {
        param($port)
        $l = [System.Net.HttpListener]::new()
        $l.Prefixes.Add("http://127.0.0.1:$port/")
        $l.Start()
        while ($l.IsListening) {
            $ctx = $l.GetContext()
            try {
                $null = [System.IO.StreamReader]::new($ctx.Request.InputStream).ReadToEnd()
                $body = '{"choices":[{"message":{"content":"up2"}}]}'
                $buf = [System.Text.Encoding]::UTF8.GetBytes($body)
                $ctx.Response.ContentType = "application/json"
                $ctx.Response.StatusCode = 200
                $ctx.Response.OutputStream.Write($buf, 0, $buf.Length)
                $ctx.Response.OutputStream.Close()
            } catch { try { $ctx.Response.Abort() } catch {} }
        }
    }
    Start-Sleep -Milliseconds 800

    # ---------- 3. 白名单物料（4 模型：2 个有路由, 1 个 provider 缺凭据, 1 个无 provider） ----------
    $materialPath = Join-Path $Tmp "00_constraint_model_allowlist.json"
    $material = @{
        "`$schema" = "https://evorule.org/schemas/rule_set/v1.0.json"
        kind = "rule_set"
        id = "com.evorule.constraint.model_allowlist"
        version = "1.0.0"
        metadata = @{ tier = "constraint"; title = "e2e 路由物料" }
        transform = @(@{
            type = "enforce"
            params = @{
                domain = @{
                    type = "all"
                    inner = @(
                        @{ type = "instruction"; instruction_type = "call_external" },
                        @{ type = "not"; inner = @{
                            type = "in"
                            path = "instruction.params.model"
                            values = @("mock-a", "mock-b", "mock-c", "orphan-d")
                            on_missing = "error"
                        } }
                    )
                }
                reason = "e2e"
            }
        })
    } | ConvertTo-Json -Depth 12
    Write-Utf8NoBom $materialPath $material

    # ---------- 4. 插件配置（路由模式）+ 启动 ----------
    $cfgPath = Join-Path $Tmp "ai-plugin.json"
    $cfg = @{
        listen_addr = "127.0.0.1:$PluginPort"
        server_base_url = "http://127.0.0.1:1"
        llm_endpoint = "http://127.0.0.1:1/v1"
        llm_api_key = "legacy-unused"
        llm_model = "legacy-model"
        model_allowlist_path = $materialPath
        llm_providers = @(
            @{
                name = "up1"
                execute_url = "http://127.0.0.1:$Up1Port/v1/chat"
                api_key_env = $KeyEnvA
                models = @("mock-a")
                default_params = @{ temperature = 0.9 }
            },
            @{
                name = "up2"
                execute_url = "http://127.0.0.1:$Up2Port/v1/chat"
                api_key_env = $KeyEnvB
                models = @("mock-b")
            }
        )
    } | ConvertTo-Json -Depth 8
    Write-Utf8NoBom $cfgPath $cfg

    # 凭据只走 env（provider 专属）；mock-c 的 provider（up3）故意不给 key → 装载期拒绝
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $PluginExe
    $psi.Arguments = "--config `"$cfgPath`""
    $psi.WorkingDirectory = $Tmp
    $psi.UseShellExecute = $false
    $psi.RedirectStandardError = $true
    $psi.RedirectStandardOutput = $true
    $psi.EnvironmentVariables[$KeyEnvA] = "e2e-key-a"
    $psi.EnvironmentVariables[$KeyEnvB] = "e2e-key-b"
    $p1 = [System.Diagnostics.Process]::Start($psi)
    Start-Sleep -Seconds 2
    if ($p1.HasExited) { Write-Host "ai-plugin 提前退出: $($p1.StandardError.ReadToEnd())"; exit 1 }
    $base = "http://127.0.0.1:$PluginPort"

    # ---------- 5. 断言 ----------
    $models = Invoke-RestMethod -Uri "$base/llm/models" -Method Get
    Assert-True ($models.ready -eq $true) "路由模式清单 ready=true"
    Assert-True (($models.models | Sort-Object) -join "," -eq "mock-a,mock-b") "清单=白名单∩已装载路由 (got: $($models.models -join ','))"
    Assert-True ($models.note -match "mock-c" -and $models.note -match "orphan-d") "未路由模型随 note 显式可见 (got: $($models.note))"

    $r = Invoke-RestMethod -Uri "$base/llm/execute" -Method Post -ContentType "application/json" `
        -Body '{"model":"mock-a","messages":[{"role":"user","content":"hi"}]}'
    Assert-True ($r.content -match "^up1;t=0\.9;auth=Bearer e2e-key-a$") "按模型路由+缺省参数打底+provider 专属凭据 (got: $($r.content))"

    $r = Invoke-RestMethod -Uri "$base/llm/execute" -Method Post -ContentType "application/json" `
        -Body '{"model":"mock-a","messages":[],"temperature":0.1}'
    Assert-True ($r.content -match "^up1;t=0\.1;") "请求体同键字段优先 (got: $($r.content))"

    $r = Invoke-RestMethod -Uri "$base/llm/execute" -Method Post -ContentType "application/json" `
        -Body '{"model":"mock-b","messages":[]}'
    Assert-True ($r.content -eq "up2") "第二 provider 路由命中 (got: $($r.content))"

    try {
        $null = Invoke-RestMethod -Uri "$base/llm/execute" -Method Post -ContentType "application/json" `
            -Body '{"model":"mock-c","messages":[]}'
        Assert-True $false "缺凭据 provider 的模型应显式拒绝"
    } catch {
        $msg = $_.ErrorDetails.Message
        Assert-True ($_.Exception.Response.StatusCode.value__ -eq 502 -and $msg -match "provider") "缺凭据 provider 的模型显式拒绝 (got: $($_.Exception.Response.StatusCode.value__) $msg)"
    }

    try {
        $null = Invoke-RestMethod -Uri "$base/llm/execute" -Method Post -ContentType "application/json" `
            -Body '{"model":"orphan-d","messages":[]}'
        Assert-True $false "无 provider 的白名单模型应显式拒绝"
    } catch {
        $msg = $_.ErrorDetails.Message
        Assert-True ($msg -match "provider") "无 provider 的白名单模型显式拒绝 (got: $msg)"
    }

    try {
        $null = Invoke-RestMethod -Uri "$base/llm/execute" -Method Post -ContentType "application/json" `
            -Body '{"model":"evil-model","messages":[]}'
        Assert-True $false "白名单外模型应显式拒绝"
    } catch {
        $msg = $_.ErrorDetails.Message
        Assert-True ($msg -match "白名单") "白名单外模型显式拒绝 (got: $msg)"
    }

    try {
        $null = Invoke-RestMethod -Uri "$base/llm/execute" -Method Post -ContentType "application/json" `
            -Body '{"messages":[]}'
        Assert-True $false "缺 model 字段应显式拒绝"
    } catch {
        $msg = $_.ErrorDetails.Message
        Assert-True ($msg -match "model") "缺 model 字段显式拒绝 (got: $msg)"
    }

    # ---------- 6. 遗留单端点模式回归（不配 llm_providers） ----------
    $cfgLegacy = Join-Path $Tmp "ai-plugin-legacy.json"
    $cfgL = @{
        listen_addr = "127.0.0.1:$PluginPortLegacy"
        server_base_url = "http://127.0.0.1:1"
        llm_endpoint = "http://127.0.0.1:1/v1"
        llm_api_key = "legacy-key"
        llm_model = "legacy-model"
        llm_execute_url = "http://127.0.0.1:$Up2Port/v1/chat"
        model_allowlist_path = $materialPath
    } | ConvertTo-Json -Depth 4
    Write-Utf8NoBom $cfgLegacy $cfgL
    $psi2 = New-Object System.Diagnostics.ProcessStartInfo
    $psi2.FileName = $PluginExe
    $psi2.Arguments = "--config `"$cfgLegacy`""
    $psi2.WorkingDirectory = $Tmp
    $psi2.UseShellExecute = $false
    $psi2.RedirectStandardError = $true
    $psi2.RedirectStandardOutput = $true
    $p2 = [System.Diagnostics.Process]::Start($psi2)
    Start-Sleep -Seconds 2
    if ($p2.HasExited) { Write-Host "legacy ai-plugin 提前退出: $($p2.StandardError.ReadToEnd())"; exit 1 }
    $baseL = "http://127.0.0.1:$PluginPortLegacy"

    $modelsL = Invoke-RestMethod -Uri "$baseL/llm/models" -Method Get
    Assert-True (($modelsL.models | Sort-Object) -join "," -eq "mock-a,mock-b,mock-c,orphan-d") "遗留模式清单=白名单全量 (got: $($modelsL.models -join ','))"
    $rL = Invoke-RestMethod -Uri "$baseL/llm/execute" -Method Post -ContentType "application/json" `
        -Body '{"model":"mock-a","messages":[]}'
    Assert-True ($rL.content -eq "up2") "遗留模式单端点行为不变 (got: $($rL.content))"

    # ---------- 结果 ----------
    if ($Failures.Count -gt 0) {
        Write-Host "`n路由 e2e 失败 $($Failures.Count) 项"; exit 1
    }
    Write-Host "`n路由 e2e 全过"
    exit 0
} finally {
    foreach ($proc in @($p1, $p2)) {
        if ($proc -and -not $proc.HasExited) { try { $proc.Kill() } catch {} }
    }
    foreach ($job in @($up1Job, $up2Job)) {
        if ($job) { Stop-Job $job -ErrorAction SilentlyContinue; Remove-Job $job -Force -ErrorAction SilentlyContinue }
    }
    Remove-Item $Tmp -Recurse -Force -ErrorAction SilentlyContinue
}

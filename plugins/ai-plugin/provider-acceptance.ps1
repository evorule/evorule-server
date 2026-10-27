# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 EvoRule Project
# This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
# provider 接入自动验收（模型准入白名单扩表前的黄金回合门）：
#   对给定 provider 端点+key 起隔离插件实例，逐模型跑三轮回+白名单拒绝轮，
#   输出 PASS/FAIL 矩阵。全过才允许把模型写入白名单物料（准入=安全面）。
# 用法:
#   powershell -ExecutionPolicy Bypass -File provider-acceptance.ps1 `
#     -Endpoint "https://open.bigmodel.cn/api/coding/paas/v4/chat/completions" `
#     -KeyFile "C:\path\to\key.txt" -Models "glm-4.6,glm-4.5-air" [-ProviderName zhipu] [-Port 19150]
# 退出码: 0=全过 1=有失败（任何 FAIL 的模型都不得入表）
# 依赖: ai-plugin exe（先 cargo build --release 或 -Debug）

param(
    [Parameter(Mandatory = $true)][string]$Endpoint,
    [string]$KeyFile,
    [string]$Key,
    [Parameter(Mandatory = $true)][string]$Models,
    [string]$ProviderName = "acceptance",
    [string]$PluginExe = "",
    [int]$Port = 19150
)

$ErrorActionPreference = "Stop"

# 无 BOM UTF-8 写入（PS5.1 兼容；payload 全 ASCII，注释中文靠 BOM 保护）
function Write-Utf8NoBom($Path, $Text) {
    [System.IO.File]::WriteAllText($Path, $Text, [System.Text.UTF8Encoding]::new($false))
}

# ---------- 凭据二选一 ----------
if (-not $Key -or $Key.Trim().Length -eq 0) {
    if (-not $KeyFile -or -not (Test-Path $KeyFile)) { Write-Host "需要 -Key 或可读的 -KeyFile"; exit 1 }
    $Key = (Get-Content $KeyFile -Raw).Trim()
}
if ($Key.Length -lt 8) { Write-Host "key 过短，疑似误读"; exit 1 }

# ---------- 插件 exe 定位 ----------
if (-not $PluginExe -or -not (Test-Path $PluginExe)) {
    foreach ($c in @((Join-Path $PSScriptRoot "target\release\evorule-ai-plugin.exe"),
                     (Join-Path $PSScriptRoot "target\debug\evorule-ai-plugin.exe"))) {
        if (Test-Path $c) { $PluginExe = $c; break }
    }
}
if (-not (Test-Path $PluginExe)) { Write-Host "ai-plugin exe 缺失（先 cargo build --release）"; exit 1 }

$modelList = $Models.Split(",") | ForEach-Object { $_.Trim() } | Where-Object { $_ }
if ($modelList.Count -eq 0) { Write-Host "-Models 为空"; exit 1 }
$used = Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue
if ($used) { Write-Host "端口 $Port 已被占用"; exit 1 }

$Failures = New-Object System.Collections.Generic.List[string]
function Assert-True($Cond, $Name) {
    if ($Cond) { Write-Host "  [PASS] $Name" }
    else { Write-Host "  [FAIL] $Name"; $script:Failures.Add($Name) | Out-Null }
}

$Tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("prov-accept-" + [guid]::NewGuid().ToString("N").Substring(0, 8))
New-Item -ItemType Directory -Path $Tmp | Out-Null
$proc = $null
try {
    # ---------- 临时物料：待验收模型 + 白名单拒绝测试哨兵 ----------
    $values = @($modelList) + "acceptance-guard-dummy"
    $valuesJson = ($values | ForEach-Object { '"' + $_ + '"' }) -join ","
    $material = @"
{
  "`$schema": "https://evorule.org/schemas/rule_set/v1.0.json",
  "kind": "rule_set",
  "id": "com.evorule.constraint.model_allowlist",
  "version": "1.0.0",
  "metadata": { "tier": "constraint", "title": "provider acceptance temp" },
  "transform": [ { "type": "enforce", "params": { "domain": { "type": "all", "inner": [
    { "type": "instruction", "instruction_type": "call_external" },
    { "type": "not", "inner": { "type": "in", "path": "instruction.params.model", "values": [$valuesJson], "on_missing": "error" } }
  ] }, "reason": "acceptance temp" } } ]
}
"@
    Write-Utf8NoBom (Join-Path $Tmp "material.json") $material

    # ---------- 临时插件配置（凭据走配置兜底字段，隔离端口） ----------
    $cfgObj = [ordered]@{
        listen_addr          = "127.0.0.1:$Port"
        server_base_url      = "http://127.0.0.1:1"
        llm_endpoint         = "http://127.0.0.1:1/v1"
        llm_api_key          = "acceptance-legacy-unused"
        llm_model            = $modelList[0]
        model_allowlist_path = (Join-Path $Tmp "material.json")
        llm_providers        = @(@{
            name         = $ProviderName
            execute_url  = $Endpoint
            api_key_env  = "EVORULE_AI_PLUGIN_ACCEPTANCE_KEY"
            api_key      = $Key
            models       = $modelList
        })
    }
    Write-Utf8NoBom (Join-Path $Tmp "plugin.json") ($cfgObj | ConvertTo-Json -Depth 6)

    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $PluginExe
    $psi.Arguments = "--config `"$Tmp\plugin.json`""
    $psi.WorkingDirectory = $Tmp
    $psi.UseShellExecute = $false
    $psi.RedirectStandardError = $true
    $proc = [System.Diagnostics.Process]::Start($psi)
    Start-Sleep -Seconds 2
    if ($proc.HasExited) { Write-Host "插件提前退出: $($proc.StandardError.ReadToEnd())"; exit 1 }
    $base = "http://127.0.0.1:$Port"

    function Post($body) {
        Invoke-RestMethod -Uri "$base/llm/execute" -Method Post -ContentType "application/json" `
            -Body $body -TimeoutSec 120
    }

    foreach ($m in $modelList) {
        Write-Host "模型 $m :"
        # ① 非流式四键
        $b1 = @{ model = $m; messages = @(@{ role = "user"; content = "Reply with exactly two characters: OK" }) } | ConvertTo-Json -Depth 5
        try {
            $r = Post $b1
            Assert-True ($r.finish_reason -eq "stop" -and $r.content -and $r.content.Length -ge 2) "非流式四键 (finish=$($r.finish_reason) len=$($r.content.Length))"
        } catch { Assert-True $false "非流式四键 (异常: $($_.Exception.Message))" }
        # ② tool_calls 往返（参数解析聚合）
        $b2 = @{
            model    = $m
            messages = @(@{ role = "user"; content = "What is the weather in Paris? Use the tool." })
            tools    = @(@{ type = "function"; function = @{
                name = "get_weather"; description = "Get current weather for a city"
                parameters = @{ type = "object"; properties = @{ city = @{ type = "string" } }; required = @("city") }
            } })
        } | ConvertTo-Json -Depth 8
        try {
            $r2 = Post $b2
            $ok = $r2.finish_reason -eq "tool_calls" -and $r2.tool_calls -and $r2.tool_calls[0].tool_name -and $r2.tool_calls[0].args
            Assert-True $ok "tool_calls 往返 (finish=$($r2.finish_reason) tool=$($r2.tool_calls[0].tool_name))"
        } catch { Assert-True $false "tool_calls 往返 (异常: $($_.Exception.Message))" }
        # ③ 真流式帧下行（PS5.1 Invoke-WebRequest 对 SSE 缓冲行为不稳，用 curl.exe）
        $b3 = @{ model = $m; messages = @(@{ role = "user"; content = "Count from 1 to 5, comma separated" }); stream = $true } | ConvertTo-Json -Depth 5
        try {
            $b3File = Join-Path $Tmp "stream-body.json"
            Write-Utf8NoBom $b3File $b3
            $txt = (& curl.exe -s -m 120 -X POST "$base/llm/execute-stream" -H "Content-Type: application/json" --data-binary "@$b3File") -join "`n"
            $frameCount = ($txt -split "`n" | Where-Object { $_ -like "data:*" -and $_ -ne "data: [DONE]" }).Count
            $hasDone = $txt -match '"type":\s*"done"' -and $txt -match "data: \[DONE\]"
            Assert-True ($frameCount -gt 0 -and $hasDone) "真流式帧下行 (frames=$frameCount sentinel=$hasDone)"
        } catch { Assert-True $false "真流式帧下行 (异常: $($_.Exception.Message))" }
    }

    # ④ 白名单拒绝哨兵（执法面在场性）
    try {
        $null = Post (@{ model = "acceptance-guard-dummy"; messages = @() } | ConvertTo-Json -Depth 3)
        Assert-True $false "白名单拒绝哨兵（不该放行）"
    } catch {
        $msg = ""
        try { $msg = $_.ErrorDetails.Message } catch {}
        Assert-True ($msg -match "白名单") "白名单拒绝哨兵 (got: $msg)"
    }

    # ---------- 结果 ----------
    Write-Host ""
    if ($Failures.Count -gt 0) {
        Write-Host "验收失败 $($Failures.Count) 项 —— 失败模型不得写入白名单物料"
        exit 1
    }
    Write-Host "验收全过：$($modelList -join ', ') 可入白名单物料"
    exit 0
} finally {
    if ($proc -and -not $proc.HasExited) { try { $proc.Kill() } catch {} }
    Remove-Item $Tmp -Recurse -Force -ErrorAction SilentlyContinue
}

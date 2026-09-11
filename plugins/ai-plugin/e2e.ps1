# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 EvoRule Project
# ai-plugin 脚本级 e2e（UV-172 DoD-①②）：真实 evorule-server + 真实 ai-plugin 进程 + mock LLM。
# 断言:
#   ① REST invoke 链路 200，返回含 reply 与 session_id
#   ② sidecar 会话审计链含两事实: call_external 命令(prompt 全文) + io_response(结果全文)
#   ③ 失败路径: mock LLM 不可达 → invoke 显式 502，无静默
# 用法: powershell -ExecutionPolicy Bypass -File e2e.ps1
# 退出码: 0=全过 1=失败

$ErrorActionPreference = "Stop"

$ServerExe = Join-Path $PSScriptRoot "..\..\target\debug\evorule-server.exe"
$PluginExe = Join-Path $PSScriptRoot "target\debug\evorule-ai-plugin.exe"
# server 宪法/规则目录按相对路径 ./resources ./rules 解析 → CWD 必须是仓库根
$ServerCwd = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$ServerUrl = "http://127.0.0.1:18080"
$MockLlmPort = 19199

$Failures = New-Object System.Collections.Generic.List[string]

function Assert-True($Cond, $Name) {
    if ($Cond) { Write-Host "  [PASS] $Name" }
    else { Write-Host "  [FAIL] $Name"; $script:Failures.Add($Name) | Out-Null }
}

# 无 BOM UTF-8 写入（PS5.1 的 Set-Content -Encoding UTF8 带 BOM，server/plugin 的
# serde_json 解析器拒绝 BOM → fail-fast exit 1，e2e 环境永远起不来）
function Write-Utf8NoBom($Path, $Text) {
    [System.IO.File]::WriteAllText($Path, $Text, [System.Text.UTF8Encoding]::new($false))
}

# ---------- 环境预检 ----------
if (-not (Test-Path $ServerExe)) { Write-Host "server 二进制缺失: $ServerExe（先 cargo build -p evorule-server --bin evorule-server）"; exit 1 }
if (-not (Test-Path $PluginExe)) { Write-Host "ai-plugin 二进制缺失: $PluginExe（先在本目录 cargo build）"; exit 1 }
foreach ($p in @(18080, 9130, $MockLlmPort)) {
    $used = Get-NetTCPConnection -LocalPort $p -State Listen -ErrorAction SilentlyContinue
    if ($used) { Write-Host "端口 $p 已被占用，e2e 无法启动"; exit 1 }
}

$Tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("ai-plugin-e2e-" + [guid]::NewGuid().ToString("N").Substring(0, 8))
New-Item -ItemType Directory -Path $Tmp | Out-Null

$serverProc = $null; $pluginProc = $null; $mockJob = $null
try {
    # ---------- 1. mock LLM（HttpListener, OpenAI 兼容 /chat/completions） ----------
    $mockJob = Start-Job -ArgumentList $MockLlmPort -ScriptBlock {
        param($port)
        $l = [System.Net.HttpListener]::new()
        $l.Prefixes.Add("http://127.0.0.1:$port/")
        $l.Start()
        while ($l.IsListening) {
            $ctx = $l.GetContext()
            try {
                $null = [System.IO.StreamReader]::new($ctx.Request.InputStream).ReadToEnd()
                $body = '{"choices":[{"message":{"content":"e2e mock reply"}}]}'
                $buf = [System.Text.Encoding]::UTF8.GetBytes($body)
                $ctx.Response.ContentType = "application/json"
                $ctx.Response.StatusCode = 200
                $ctx.Response.OutputStream.Write($buf, 0, $buf.Length)
                $ctx.Response.OutputStream.Close()
            } catch { try { $ctx.Response.Abort() } catch {} }
        }
    }

    # ---------- 2. ai-plugin 配置 + 启动 ----------
    $pluginCfg = Join-Path $Tmp "ai-plugin.json"
    @{
        listen_addr      = "127.0.0.1:9130"
        server_base_url  = $ServerUrl
        llm_endpoint     = "http://127.0.0.1:$MockLlmPort/v1"
        llm_api_key      = "e2e-mock-key"
        llm_model        = "e2e-model"
        llm_temperature  = 0.2
        llm_timeout_ms   = 30000
    } | ConvertTo-Json | ForEach-Object { Write-Utf8NoBom $pluginCfg $_ }
    $pluginProc = Start-Process -FilePath $PluginExe -ArgumentList @("--config", $pluginCfg) -PassThru -WindowStyle Hidden

    # ---------- 3. 临时插件清单（ai-plugin enabled=true，不动仓库清单） ----------
    $manifest = Join-Path $Tmp "plugin_manifest.json"
    @{
        plugins = @{
            "ai-plugin" = @{ enabled = $true; manifest = (Resolve-Path (Join-Path $PSScriptRoot "plugin.json")).Path -replace "\\", "/" }
        }
    } | ConvertTo-Json -Depth 5 | ForEach-Object { Write-Utf8NoBom $manifest $_ }
    $serverProc = Start-Process -FilePath $ServerExe `
        -ArgumentList @("--addr", "127.0.0.1:18080", "--insecure-serve", "--allow-loopback", "--wal-dir", (Join-Path $Tmp "wal"), "--plugins", $manifest) `
        -WorkingDirectory $ServerCwd -PassThru -WindowStyle Hidden

    # ---------- 4. 就绪等待 ----------
    $ready = $false
    foreach ($i in 1..40) {
        Start-Sleep -Milliseconds 500
        try {
            $h = Invoke-RestMethod -Uri "$ServerUrl/api/health" -TimeoutSec 2 -ErrorAction Stop
            $p = Invoke-RestMethod -Uri "http://127.0.0.1:9130/health" -TimeoutSec 2 -ErrorAction Stop
            if ($h -and $p.ok) { $ready = $true; break }
        } catch {}
    }
    Assert-True $ready "环境就绪（server /health + ai-plugin /health）"
    if (-not $ready) { throw "环境未就绪，中止" }

    # ---------- 5. REST invoke 全链路（DoD-①） ----------
    $invokeBody = @{ messages = @(@{ role = "user"; content = "e2e probe 写一条阈值规则" }) } | ConvertTo-Json -Depth 5
    # PS5.1 对字符串 body 按 ISO-8859-1 编码（中文入口即毁）→ 必须显式 UTF-8 字节
    $raw = Invoke-WebRequest -Uri "$ServerUrl/api/services/ai_plugin_chat/invoke" -Method Post `
        -ContentType "application/json" -Body ([System.Text.Encoding]::UTF8.GetBytes($invokeBody)) `
        -TimeoutSec 120 -UseBasicParsing
    Assert-True ($raw.StatusCode -eq 200) "invoke 返回 200"
    # invoke 经 HttpHandler 返回体为字符串包裹（机制层语义），双层解析
    $inner = ($raw.Content | ConvertFrom-Json)
    if ($inner -is [string]) { $payload = $inner | ConvertFrom-Json } else { $payload = $inner }
    Assert-True ($payload.reply -eq "e2e mock reply") "返回 reply 全文"
    $sid = [int]$payload.session_id
    Assert-True ($sid -gt 0) "返回审计会话 id（session_id=$sid）"

    # ---------- 6. sidecar 审计链两事实（DoD-②） ----------
    # 插件按一次性 sidecar 语义在回路完成后 DELETE 会话 → 活跃审计端点 404；
    # 审计链经 --wal-dir 落盘，从只读档案端点重建（含 content_json 全文）。
    # PS5.1 对无 charset 的 UTF-8 响应按 ISO-8859-1 解码、ConvertTo-Json 又会把
    # 非 ASCII 转成 \uXXXX → 中文断言必失败；故用 curl.exe 落盘 + 显式 UTF-8 读。
    $auditFile = Join-Path $Tmp "audit.json"
    $null = & curl.exe -s -o $auditFile -w "%{response_code}" "$ServerUrl/api/audit-archive/sessions/$sid/audit`?include_content=true"
    $factJson = Get-Content $auditFile -Raw -Encoding UTF8
    Assert-True ($factJson -match "call_external") "审计链含 call_external 命令事实"
    Assert-True ($factJson -match "e2e probe 写一条阈值规则") "审计链含 prompt 全文（命令事实）"
    Assert-True ($factJson -match "executor") "审计链含 executor 通道协调位"
    Assert-True ($factJson -match "e2e mock reply") "审计链含 io_response 结果全文"
    Assert-True ($factJson -match "IoRequest") "审计链含 IoRequest 事实"
} catch {
    Write-Host "  [FAIL] e2e 执行异常: $($_.Exception.Message)"
    $Failures.Add("e2e 执行异常") | Out-Null
} finally {
    foreach ($p in @($serverProc, $pluginProc)) {
        if ($p -and -not $p.HasExited) { try { $p.Kill(); $p.WaitForExit(5000) | Out-Null } catch {} }
    }
    if ($mockJob) { Stop-Job $mockJob -ErrorAction SilentlyContinue; Remove-Job $mockJob -Force -ErrorAction SilentlyContinue }
    Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
}

# ---------- 7. 失败路径（DoD-② e2e 版: mock LLM 不可达 → 502 显式报错） ----------
# 独立环境: 只起 server+plugin, LLM 端点指向无人监听端口
$Tmp2 = Join-Path ([System.IO.Path]::GetTempPath()) ("ai-plugin-e2e2-" + [guid]::NewGuid().ToString("N").Substring(0, 8))
New-Item -ItemType Directory -Path $Tmp2 | Out-Null
$serverProc2 = $null; $pluginProc2 = $null
try {
    $pluginCfg2 = Join-Path $Tmp2 "ai-plugin.json"
    @{
        listen_addr     = "127.0.0.1:9130"
        server_base_url = $ServerUrl
        llm_endpoint    = "http://127.0.0.1:19298/v1"
        llm_api_key     = "e2e-mock-key"
        llm_model       = "e2e-model"
        llm_timeout_ms  = 5000
    } | ConvertTo-Json | ForEach-Object { Write-Utf8NoBom $pluginCfg2 $_ }
    $pluginProc2 = Start-Process -FilePath $PluginExe -ArgumentList @("--config", $pluginCfg2) -PassThru -WindowStyle Hidden

    $manifest2 = Join-Path $Tmp2 "plugin_manifest.json"
    @{
        plugins = @{
            "ai-plugin" = @{ enabled = $true; manifest = (Resolve-Path (Join-Path $PSScriptRoot "plugin.json")).Path -replace "\\", "/" }
        }
    } | ConvertTo-Json -Depth 5 | ForEach-Object { Write-Utf8NoBom $manifest2 $_ }
    $serverProc2 = Start-Process -FilePath $ServerExe `
        -ArgumentList @("--addr", "127.0.0.1:18080", "--insecure-serve", "--allow-loopback", "--wal-dir", (Join-Path $Tmp2 "wal"), "--plugins", $manifest2) `
        -WorkingDirectory $ServerCwd -PassThru -WindowStyle Hidden

    $ready2 = $false
    foreach ($i in 1..40) {
        Start-Sleep -Milliseconds 500
        try {
            $null = Invoke-RestMethod -Uri "$ServerUrl/api/health" -TimeoutSec 2 -ErrorAction Stop
            $null = Invoke-RestMethod -Uri "http://127.0.0.1:9130/health" -TimeoutSec 2 -ErrorAction Stop
            $ready2 = $true; break
        } catch {}
    }
    Assert-True $ready2 "失败路径环境就绪"
    if (-not $ready2) { throw "失败路径环境未就绪" }

    $body2 = '{"messages":[{"role":"user","content":"e2e failure probe"}]}'
    $bodyFile2 = Join-Path $Tmp2 "invoke-body.json"
    Write-Utf8NoBom $bodyFile2 $body2
    # PS5.1 的 Invoke-WebRequest 错误响应流读取为空（quirk）→ 用 curl.exe 取错误体
    $curlOut2 = Join-Path $Tmp2 "curl-out.txt"
    $status = & curl.exe -s -o $curlOut2 -w "%{response_code}" -X POST -H "Content-Type: application/json" --data "@$bodyFile2" "$ServerUrl/api/services/ai_plugin_chat/invoke"
    $errText = if (Test-Path $curlOut2) { Get-Content $curlOut2 -Raw } else { "" }
    Assert-True ($status -eq 502) "LLM 不可达 → invoke 显式 502（无静默）"
    Assert-True ($errText -match "LLM") "错误消息含 LLM 归因"
} catch {
    Write-Host "  [FAIL] 失败路径执行异常: $($_.Exception.Message)"
    $Failures.Add("失败路径执行异常") | Out-Null
} finally {
    foreach ($p in @($serverProc2, $pluginProc2)) {
        if ($p -and -not $p.HasExited) { try { $p.Kill(); $p.WaitForExit(5000) | Out-Null } catch {} }
    }
    Remove-Item -Recurse -Force $Tmp2 -ErrorAction SilentlyContinue
}

Write-Host ""
if ($Failures.Count -eq 0) {
    Write-Host "ai-plugin e2e: 全部通过"
    exit 0
} else {
    Write-Host "ai-plugin e2e: $($Failures.Count) 项失败"
    exit 1
}

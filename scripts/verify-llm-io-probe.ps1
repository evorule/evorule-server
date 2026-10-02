# verify-llm-io-probe.ps1 — 会话主体声明通路探针 (Windows PowerShell)
# 用法: powershell -ExecutionPolicy Bypass -File scripts\verify-llm-io-probe.ps1 [-BaseUrl http://127.0.0.1:18080]
# 依赖: 已启动的 evorule-server；llm 放行条目应已 Active（未配置先跑
#       configure-llm-io-permission.ps1，新部署由启动种子自动写入）
#
# 验证内容（与 configure-llm-io-permission.ps1 配套成对）:
#   1. 放行条目 default-llm-allow-io 处于 Active
#   2. 声明 llm 的会话：命令事实注入 __meta__.caller_role=llm（审计链取证）
#   3. 未声明会话：命令事实无 __meta__（fail-closed 注入面反向对照）
#
# 范围说明: 本探针验证「声明→注入→审计归因」链路。call_service 端到端放行
# （权限门 Allow → 服务执行 → service_result 落链）依赖会话装载含 io_request
# 指令的规则包，属规则装配面，不在本脚本范围。
#
# 幂等：每次运行新建临时会话（内存态，重启即消），可重复执行。

param(
    [string]$BaseUrl = "http://127.0.0.1:18080"
)

$ErrorActionPreference = "Stop"

function Invoke-Api($Method, $Uri, $Body = $null) {
    $params = @{ Uri = $Uri; Method = $Method; TimeoutSec = 15; ContentType = "application/json" }
    if ($null -ne $Body) { $params["Body"] = $Body }
    Invoke-RestMethod @params
}

function Get-CommandMeta($BaseUrl, $roleDeclared) {
    # 新建会话（按需声明）→ 发 tick 指令 → 取审计链首条 Command 事实的 instruction
    $body = if ($roleDeclared) { '{"caller_role":"llm"}' } else { '{}' }
    $s = Invoke-Api Post "$BaseUrl/api/sessions" $body
    $sid = $s.session_id
    Invoke-Api Post "$BaseUrl/api/sessions/$sid/command" '{"instruction":{"type":"tick"}}' | Out-Null
    Start-Sleep -Seconds 1
    $a = Invoke-Api Get "$BaseUrl/api/sessions/$sid/audit?include_content=true"
    $cmd = $a.entries | Where-Object { $_.content_json.type -eq 'Command' } | Select-Object -First 1
    return @{ SessionId = $sid; Instruction = $cmd.content_json.instruction }
}

Write-Host "[llm-probe] EvoRule 会话主体声明通路探针 -> $BaseUrl"

# 1. 放行条目状态
$list = Invoke-Api Get "$BaseUrl/api/permissions"
$entry = $list.entries | Where-Object { $_.id -eq "default-llm-allow-io" } | Select-Object -First 1
if ($null -eq $entry -or $entry.state -ne "active") {
    Write-Host "[llm-probe] 错误: default-llm-allow-io 不在 Active 态（先跑 configure-llm-io-permission.ps1）" -ForegroundColor Red
    exit 1
}
Write-Host "[llm-probe] 1/3 放行条目 Active ✓"

# 2. 声明 llm 会话：__meta__.caller_role=llm 注入
$decl = Get-CommandMeta $BaseUrl $true
$metaRole = $null
if ($decl.Instruction -and $decl.Instruction.__meta__) { $metaRole = $decl.Instruction.__meta__.caller_role }
if ($metaRole -ne "llm") {
    Write-Host "[llm-probe] 错误: 声明 llm 会话的命令事实未注入 __meta__.caller_role=llm（实得: $($decl.Instruction | ConvertTo-Json -Compress))" -ForegroundColor Red
    exit 1
}
Write-Host "[llm-probe] 2/3 声明 llm 会话 __meta__.caller_role=llm 注入 ✓ (session=$($decl.SessionId))"

# 3. 未声明会话反向对照：无 __meta__ 注入
$plain = Get-CommandMeta $BaseUrl $false
$hasMeta = $plain.Instruction -and $plain.Instruction.__meta__
if ($hasMeta) {
    Write-Host "[llm-probe] 错误: 未声明会话不应有 __meta__ 注入（fail-closed 注入面被破坏）" -ForegroundColor Red
    exit 1
}
Write-Host "[llm-probe] 3/3 未声明会话无注入（fail-closed 反向对照）✓ (session=$($plain.SessionId))"

Write-Host "[llm-probe] 完成: 声明通路 3/3 通过 — 声明→注入→审计归因链路正常"
exit 0

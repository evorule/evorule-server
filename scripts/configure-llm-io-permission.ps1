# configure-llm-io-permission.ps1 — LLM 会话 I/O 放行条目配置脚本 (Windows PowerShell)
# 用法: powershell -ExecutionPolicy Bypass -File scripts\configure-llm-io-permission.ps1 [-BaseUrl http://127.0.0.1:18080] [-Token <bearer>]
# 依赖: 已启动的 evorule-server
#
# 行为 (幂等，可重复执行):
#   1. 探测 server 在线 (GET /api/health)
#   2. 查询权限条目 default-llm-allow-io (GET /api/permissions)
#      - 已存在且 Active -> 直接通过
#      - 不存在          -> POST /api/permissions 新建 (Draft)
#                           -> POST .../submit (Draft -> Candidate)
#                           -> POST .../review {"approve":true} (Candidate -> Active)
#      - 存在但未生效    -> 按当前状态补 submit/review
#   3. 输出终态校验结果
#
# 背景: 默认策略下 llm 主体的 I/O 一律 Deny (fail-closed)。声明 caller_role=llm
# 的会话 (agent 后端 / LLM 审计桥 / ai-plugin) 如需 call_service，须由管理员
# 显式放行。本条目只放 io:call_service (最小放大面)，落共享事实 WAL 持久，
# 重启不丢。详见 docs/INTEGRATION_GUIDE.md §7.3。

param(
    [string]$BaseUrl = "http://127.0.0.1:18080",
    [string]$Token = ""
)

$ErrorActionPreference = "Stop"
$ENTRY_ID = "default-llm-allow-io"

$headers = @{}
if ($Token -ne "") { $headers["Authorization"] = "Bearer $Token" }
function Invoke-Api($Method, $Uri, $Body = $null) {
    $params = @{ Uri = $Uri; Method = $Method; Headers = $headers; TimeoutSec = 10; ContentType = "application/json" }
    if ($null -ne $Body) { $params["Body"] = ([System.Text.Encoding]::UTF8.GetBytes(($Body | ConvertTo-Json -Depth 12 -Compress))) }
    Invoke-RestMethod @params
}

Write-Host "[llm-perm] EvoRule LLM I/O permission configurator -> $BaseUrl"

# 1. 探测 server
try {
    Invoke-Api Get "$BaseUrl/api/health" | Out-Null
    Write-Host "[llm-perm] server 在线"
} catch {
    Write-Host "[llm-perm] 错误: 无法连接 $BaseUrl/api/health ，请先启动 evorule-server。" -ForegroundColor Red
    exit 1
}

# 2. 查询条目现状
$existing = $null
try {
    $list = Invoke-Api Get "$BaseUrl/api/permissions"
    $existing = $list.entries | Where-Object { $_.id -eq $ENTRY_ID } | Select-Object -First 1
} catch {
    Write-Host "[llm-perm] 错误: 查询权限条目失败: $($_.Exception.Message)" -ForegroundColor Red
    exit 1
}

if ($null -ne $existing -and $existing.state -eq "active") {
    Write-Host "[llm-perm] 条目 $ENTRY_ID 已 Active，无需变更 (幂等通过)"
    exit 0
}

if ($null -eq $existing) {
    # 2a. 新建 (服务端强制 Draft)
    $entry = @{
        id       = $ENTRY_ID
        subject  = @{ subject_type = "user"; id = "llm" }
        resource = @{ resource_type = "io_action"; path = "io:call_service" }
        action   = "*"
        effect   = "allow"
    }
    try {
        Invoke-Api Post "$BaseUrl/api/permissions" $entry | Out-Null
        Write-Host "[llm-perm] 已新建条目 $ENTRY_ID (Draft)"
    } catch {
        Write-Host "[llm-perm] 错误: 新建条目失败: $($_.Exception.Message)" -ForegroundColor Red
        exit 1
    }
    $state = "draft"
} else {
    $state = $existing.state
    Write-Host "[llm-perm] 条目 $ENTRY_ID 已存在，状态 = $state，补齐审批流"
}

# 2b. 按状态补 submit / review
try {
    if ($state -eq "draft") {
        Invoke-Api Post "$BaseUrl/api/permissions/$ENTRY_ID/submit" | Out-Null
        Write-Host "[llm-perm] 已提交审批 (Candidate)"
    }
    Invoke-Api Post "$BaseUrl/api/permissions/$ENTRY_ID/review" @{ approve = $true } | Out-Null
    Write-Host "[llm-perm] 已裁决生效 (Active)"
} catch {
    Write-Host "[llm-perm] 错误: 审批流推进失败: $($_.Exception.Message)" -ForegroundColor Red
    exit 1
}

# 3. 终态校验
$list = Invoke-Api Get "$BaseUrl/api/permissions"
$final = $list.entries | Where-Object { $_.id -eq $ENTRY_ID } | Select-Object -First 1
if ($null -ne $final -and $final.state -eq "active") {
    Write-Host "[llm-perm] 完成: $ENTRY_ID 已 Active — llm 声明会话的 call_service 放行生效"
    exit 0
}
Write-Host "[llm-perm] 错误: 终态非 Active (state=$($final.state))，请人工检查" -ForegroundColor Red
exit 1

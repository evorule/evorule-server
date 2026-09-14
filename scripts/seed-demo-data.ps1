# seed-demo-data.ps1 — EvoRule 演示数据播种脚本 (Windows PowerShell)
# 用法: powershell -ExecutionPolicy Bypass -File seed-demo-data.ps1
# 依赖: 已启动的 evorule-server (默认 127.0.0.1:18080)
#
# 行为:
#   1. 探测 server 是否在运行 (GET /api/state)
#   2. 新建会话 POST /api/sessions
#   3. 遍历 data/seed/*.json:
#      - 含 "path" 字段的记录 -> POST /api/sessions/{id}/payload  (患者档案等参考数据)
#      - 含 "instruction_type" 字段的记录 -> POST /api/sessions/{id}/command (业务指令, 触发规则)

$ErrorActionPreference = "Stop"
$BASE = "http://127.0.0.1:18080"
$SEED_DIR = Split-Path -Parent $MyInvocation.MyCommand.Definition
$SEED_DIR = Join-Path (Split-Path -Parent $SEED_DIR) "data\seed"

Write-Host "[seed] EvoRule demo data seeder -> $BASE"

# 1. 探测 server
try {
    $health = Invoke-RestMethod -Uri "$BASE/api/state" -Method Get -TimeoutSec 3
    Write-Host "[seed] server 在线 (payload version=$($health.version))"
} catch {
    Write-Host "[seed] 错误: 无法连接 $BASE/api/state ，请先启动 evorule-server (端口 18080)。" -ForegroundColor Red
    exit 1
}

# 2. 新建会话
try {
    $sess = Invoke-RestMethod -Uri "$BASE/api/sessions" -Method Post -ContentType "application/json" -Body "{}"
    $SID = $sess.session_id
    Write-Host "[seed] 新建会话 session_id = $SID"
} catch {
    Write-Host "[seed] 错误: 建会话失败: $($_.Exception.Message)" -ForegroundColor Red
    exit 1
}

$files = @("finance_expenses.json", "finance_invoices.json", "medical_patients.json", "medical_prescriptions.json")
$total = 0; $ok = 0

foreach ($f in $files) {
    $p = Join-Path $SEED_DIR $f
    if (-not (Test-Path $p)) { Write-Host "[seed] 跳过缺失文件: $p"; continue }
    $records = Get-Content -Raw -Path $p -Encoding UTF8 | ConvertFrom-Json
    Write-Host "[seed] == $f : $($records.Count) 条 =="
    foreach ($r in $records) {
        $total++
        try {
            if ($r.PSObject.Properties.Name -contains "path") {
                # 参考数据: 直接写 payload
                $body = @{ path = $r.path; value = $r.value } | ConvertTo-Json -Depth 12 -Compress
                Invoke-RestMethod -Uri "$BASE/api/sessions/$SID/payload" -Method Post -ContentType "application/json; charset=utf-8" -Body ([System.Text.Encoding]::UTF8.GetBytes($body)) | Out-Null
                Write-Host "   [payload] $($r.path)"
            } elseif ($r.PSObject.Properties.Name -contains "instruction_type") {
                # 业务指令: 提交后异步执行
                $cmd = @{ instruction = @{ type = $r.instruction_type; params = $r.params } } | ConvertTo-Json -Depth 12 -Compress
                $resp = Invoke-RestMethod -Uri "$BASE/api/sessions/$SID/command" -Method Post -ContentType "application/json; charset=utf-8" -Body ([System.Text.Encoding]::UTF8.GetBytes($cmd))
                Write-Host "   [command] $($r.instruction_type) -> fact_id=$($resp.fact_id)  (期望: $($r.expect))"
                Start-Sleep -Milliseconds 150
            } else {
                Write-Host "   [skip] 记录既无 path 也无 instruction_type" -ForegroundColor Yellow
                continue
            }
            $ok++
        } catch {
            Write-Host "   [error] $($_.Exception.Message)" -ForegroundColor Red
        }
    }
}

Start-Sleep -Milliseconds 500
Write-Host "[seed] 完成: $ok / $total 条写入。"
Write-Host "[seed] 查看结果: GET $BASE/api/sessions/$SID/state"

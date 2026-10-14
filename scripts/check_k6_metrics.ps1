#!/usr/bin/env pwsh
# check_k6_metrics.ps1 —— [CON-K6-001]/[CON-L4-004] 遵循成本度量双指标
#
# 指标1 金样例覆盖率: 宪法仓 tools/check_golden_coverage.py（目标 100%，退化=红灯）
# 指标2 报错向导覆盖率: evorule-rule-tools four_segment 模板测试通过率
#        （四段式报文缺任一段=红灯，[CON-K2-001]/[CON-L1-002] 口径）
#
# 宪法仓定位: 环境变量 EVORULE_RULES_SRC 优先，其次兄弟目录 evorule-system-rules。
# 退出码: 0 = 双指标全绿; 1 = 任一指标退化; 2 = 环境不完整（宪法仓不可得等）。

$ErrorActionPreference = 'Stop'
$serverRoot = Split-Path -Parent $PSScriptRoot

$coverageScript = Join-Path 'tools' 'check_golden_coverage.py'
$rulesRoot = $env:EVORULE_RULES_SRC
if (-not $rulesRoot) {
    $candidate = Join-Path (Split-Path -Parent $serverRoot) 'evorule-system-rules'
    if (Test-Path (Join-Path $candidate $coverageScript)) { $rulesRoot = $candidate }
}
if (-not $rulesRoot -or -not (Test-Path (Join-Path $rulesRoot $coverageScript))) {
    Write-Host "[FAIL] 找不到 evorule-system-rules 宪法仓（K6 双指标是闭环件）。设 EVORULE_RULES_SRC 或将仓置于兄弟目录" -ForegroundColor Red
    exit 2
}

$failed = @()

# === 指标1: 金样例覆盖率 ===
Write-Host "`n>>> [K6 metric 1] golden coverage <<<" -ForegroundColor Magenta
& python (Join-Path $rulesRoot $coverageScript)
if ($LASTEXITCODE -ne 0) { $failed += 'golden-coverage' }

# === 指标2: 报错向导覆盖率 ===
Write-Host "`n>>> [K6 metric 2] wizard (four-segment) coverage <<<" -ForegroundColor Magenta
Push-Location $serverRoot
try {
    & cargo test -p evorule-rule-tools --lib four_segment
    if ($LASTEXITCODE -ne 0) { $failed += 'wizard-coverage' }
} finally {
    Pop-Location
}

# === SUMMARY ===
if ($failed.Count -gt 0) {
    Write-Host "K6 METRICS: FAILED ($($failed -join ', ')) — 任一指标退化即红灯 [CON-L4-004]" -ForegroundColor Red
    exit 1
}
Write-Host "K6 METRICS: ALL GREEN（金样例覆盖率 100% + 报错向导覆盖率 100%）[CON-K6-001]" -ForegroundColor Green
exit 0

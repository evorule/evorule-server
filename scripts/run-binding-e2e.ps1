# 三层绑定执行侧闭环端到端验收脚本（，14 号实施计划）
#
# 验收链路：治理侧声明（data_dependencies）→ 条目绑定（data_source_binding）
#   → 执行侧 service_registry 绑定 → 规则 io_request 真实 HTTP 命中。
#
# 覆盖：
#   1. tests/binding_e2e.rs 正向用例（三层绑定全链命中 + 落盘/往返断言）
#   2. tests/binding_e2e.rs 负向用例（绑定缺失 → 自诊断指引）
#   3. core/io_handlers 单测（ServiceRegistry 解析/门禁口径）
#
# 用法：
#   pwsh ./scripts/run-binding-e2e.ps1              # 全量验收
#   pwsh ./scripts/run-binding-e2e.ps1 -SkipUnit    # 仅端到端
#
# 退出码 0 = 验收通过；非 0 = 存在失败项（如实退出，不静默）。

[CmdletBinding()]
param(
    [switch]$SkipUnit
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repoRoot

$failures = 0

Write-Host "=== [1/3] 三层绑定端到端（tests/binding_e2e.rs）===" -ForegroundColor Cyan
cargo test -p evorule-server --test binding_e2e -- --nocapture
if ($LASTEXITCODE -ne 0) { $failures++; Write-Host "[FAIL] binding_e2e 未通过" -ForegroundColor Red }
else { Write-Host "[OK] binding_e2e 通过" -ForegroundColor Green }

if (-not $SkipUnit) {
    Write-Host "=== [2/3] service_registry 单测（core/io_handlers）===" -ForegroundColor Cyan
    cargo test -p evorule-io-handlers --lib service_registry
    if ($LASTEXITCODE -ne 0) { $failures++; Write-Host "[FAIL] service_registry 单测未通过" -ForegroundColor Red }
    else { Write-Host "[OK] service_registry 单测通过" -ForegroundColor Green }

    Write-Host "=== [3/3] Q12 回归（治理发布→执行侧导入全链不回归）===" -ForegroundColor Cyan
    cargo test -p evorule-server --test q12_data_asset_e2e
    if ($LASTEXITCODE -ne 0) { $failures++; Write-Host "[FAIL] q12_data_asset_e2e 未通过" -ForegroundColor Red }
    else { Write-Host "[OK] q12_data_asset_e2e 通过" -ForegroundColor Green }
}

if ($failures -gt 0) {
    Write-Host "`n验收失败：$failures 项未通过" -ForegroundColor Red
    exit 1
}
Write-Host "`n三层绑定端到端验收全部通过" -ForegroundColor Green

#!/usr/bin/env pwsh
# validate-all.ps1
# One-shot runner for all VERSION_STRATEGY validation scripts
#
# 用法:
#   pwsh scripts/validate-all.ps1              # 发布后验证(严格模式): tag 必须存在, 无 [Unreleased]
#   pwsh scripts/validate-all.ps1 -PreRelease  # 发布前就绪检查: 跳过 tag 检查, 允许 [Unreleased]
#   pwsh scripts/validate-all.ps1 -AllowUnreleased -SkipTagCheck  # 等价于 -PreRelease
#
# 时序:
#   1. 发布前:  pwsh scripts/validate-all.ps1 -PreRelease   (代码/文档/CHANGELOG 就绪, tag 未打)
#   2. 打 tag:  git tag v0.1.1
#   3. 发布后:  pwsh scripts/validate-all.ps1               (tag 存在且与版本一致)
#
# 包含 8 项检查:
#   0. 门禁绕过检测 (EVORULE_SKIP_GATE 环境变量)
#   1-5. validate-version / validate-changelog / validate-license / validate-cargolock / validate-release
#   6. check_doc_safety.py (文档安全 + 交叉引用完整性 + 基调合规)
#   7. check_schema_sync.py (内嵌 Schema 与 evorule-system-rules 源仓一致性)

[CmdletBinding()]
param(
    [switch]$AllowUnreleased,
    [switch]$SkipTagCheck,
    [switch]$PreRelease
)

# -PreRelease = -AllowUnreleased + -SkipTagCheck(发布前就绪检查一站式)
if ($PreRelease) {
    $AllowUnreleased = $true
    $SkipTagCheck = $true
}

$mode = if ($PreRelease) { "PRE-RELEASE (就绪检查: 跳过 tag, 允许 [Unreleased])" } else { "RELEASE (发布验证: tag 必须存在, 无 [Unreleased])" }
Write-Host "=== Validate-All Mode: $mode ===" -ForegroundColor Cyan

$failed = @()

# === 0. 门禁绕过检测 ===
# build.rs L1 门禁在 cargo build 时隐式运行, 但 EVORULE_SKIP_GATE=1 可无声绕过。
# 发布流程必须确保门禁活跃, 否则违规模式可能进入发布包。
Write-Host "`n>>> [gate-bypass-check] <<<" -ForegroundColor Magenta
if (Test-Path "env:EVORULE_SKIP_GATE") {
    Write-Host "[FAIL] EVORULE_SKIP_GATE is set — build.rs L1 门禁被绕过, 禁止发布" -ForegroundColor Red
    Write-Host "       如需解除: Remove-Item env:EVORULE_SKIP_GATE" -ForegroundColor DarkGray
    $failed += 'gate-bypass-check'
} else {
    Write-Host "[OK]   EVORULE_SKIP_GATE not set — build.rs L1 门禁活跃" -ForegroundColor Green
}

# === 1-5. validate-*.ps1 五件套 ===
$scripts = @(
    @{ Name = 'validate-version';   File = 'validate-version.ps1' }
    @{ Name = 'validate-changelog'; File = 'validate-changelog.ps1' }
    @{ Name = 'validate-license';   File = 'validate-license.ps1' }
    @{ Name = 'validate-cargolock'; File = 'validate-cargolock.ps1' }
    @{ Name = 'validate-release';   File = 'validate-release.ps1' }
)

foreach ($s in $scripts) {
    Write-Host "`n>>> [$($s.Name)] <<<" -ForegroundColor Magenta
    $scriptPath = "$PSScriptRoot\$($s.File)"
    # 用哈希表 splatting 传递命名参数(PS 5.1 下数组 splatting 传 switch 会报 positional parameter 错误)
    $scriptParams = @{}
    if ($s.Name -eq 'validate-changelog' -and $AllowUnreleased) {
        $scriptParams['AllowUnreleased'] = $true
    }
    if ($s.Name -eq 'validate-release' -and $SkipTagCheck) {
        $scriptParams['SkipTagCheck'] = $true
    }
    & $scriptPath @scriptParams
    if ($LASTEXITCODE -ne 0) {
        $failed += $s.Name
    }
}

# === 6. check_doc_safety.py (文档安全 + 交叉引用完整性 + 基调合规) ===
Write-Host "`n>>> [check_doc_safety] <<<" -ForegroundColor Magenta
$repoRoot = Split-Path -Parent $PSScriptRoot
$docSafetyScript = Join-Path $repoRoot "scripts\check_doc_safety.py"

# CI 环境无 staged 文件, 用 --skip-git 跳过 R-门控1 staged 检查
# 本地发布前检查也用 --skip-git (staged 检查由发布流程人工保证)
$docSafetyArgs = @("--skip-git")
& python $docSafetyScript @docSafetyArgs
if ($LASTEXITCODE -ne 0) {
    $failed += 'check_doc_safety'
}

# === 7. check_schema_sync.py (内嵌 Schema 与源仓一致性) ===
Write-Host "`n>>> [check_schema_sync] <<<" -ForegroundColor Magenta
$schemaSyncScript = Join-Path $repoRoot "scripts\check_schema_sync.py"
& python $schemaSyncScript
if ($LASTEXITCODE -ne 0) {
    $failed += 'check_schema_sync'
}

# === SUMMARY ===
Write-Host "`n=========== SUMMARY ===========" -ForegroundColor Cyan
$totalChecks = 1 + $scripts.Count + 2  # gate-bypass + 5 scripts + doc-safety + schema-sync
if ($failed.Count -gt 0) {
    Write-Host "FAILED: $($failed -join ', ')" -ForegroundColor Red
    exit 1
}
Write-Host "ALL $totalChecks CHECKS PASSED (gate-bypass + 5 validate scripts + doc-safety + schema-sync)" -ForegroundColor Green
exit 0

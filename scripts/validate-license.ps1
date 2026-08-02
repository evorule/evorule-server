#!/usr/bin/env pwsh
# validate-license.ps1
# Check: LICENSE file + AGPL identifier + .rs SPDX header
# 各仓独立发布:仅校验本仓(evorule-server)
# Exit code: 0 = pass, 1 = fail

[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot

$failed = $false
Write-Host "`n=== License Validation ===" -ForegroundColor Cyan

# 1. LICENSE file + AGPL content(各仓独立发布:仅校验本仓)
$licenseChecks = @(
    @{ Name = 'evorule-server'; Path = "$repoRoot\LICENSE" }
)
foreach ($p in $licenseChecks) {
    if (-not (Test-Path $p.Path)) {
        Write-Host "[FAIL] $($p.Name) : LICENSE not found at $($p.Path)" -ForegroundColor Red
        $failed = $true
        continue
    }
    $content = Get-Content $p.Path -Raw
    if ($content -match 'GNU Affero General Public License|AGPL-3.0') {
        Write-Host "[OK]   $($p.Name) : LICENSE contains AGPL" -ForegroundColor Green
    } else {
        Write-Host "[FAIL] $($p.Name) : LICENSE does not contain AGPL" -ForegroundColor Red
        $failed = $true
    }
}

# 2. .rs files SPDX header(各仓独立发布:仅校验本仓 src)
$rsDirs = @(
    "$repoRoot\core\auth\src",
    "$repoRoot\core\debug_control\src",
    "$repoRoot\core\hot_reload\src",
    "$repoRoot\core\io_handlers\src",
    "$repoRoot\core\metrics\src",
    "$repoRoot\core\rule_tools\src",
    "$repoRoot\core\semantic_invariants\src",
    "$repoRoot\core\time_machine\src",
    "$repoRoot\evorule-server\src"
)
$totalRs = 0
$withSpdx = 0
$missingFiles = @()
foreach ($dir in $rsDirs) {
    if (-not (Test-Path $dir)) { continue }
    $rsFiles = Get-ChildItem -Path $dir -Filter "*.rs" -Recurse
    foreach ($f in $rsFiles) {
        $totalRs++
        $head = Get-Content $f.FullName -TotalCount 12 -ErrorAction SilentlyContinue
        if ($head -join "`n" -match 'SPDX-License-Identifier:\s*AGPL-3.0') {
            $withSpdx++
        } else {
            $missingFiles += $f.FullName.Replace($repoRoot, '.')
        }
    }
}
if ($totalRs -eq 0) {
    Write-Host "[WARN] No .rs files found" -ForegroundColor Yellow
} elseif ($withSpdx -eq $totalRs) {
    Write-Host "[OK]   All $totalRs .rs files have SPDX header" -ForegroundColor Green
} else {
    $missing = $totalRs - $withSpdx
    Write-Host "[FAIL] $missing / $totalRs .rs files missing SPDX header" -ForegroundColor Red
    foreach ($f in $missingFiles | Select-Object -First 5) {
        Write-Host "       - $f" -ForegroundColor DarkRed
    }
    if ($missingFiles.Count -gt 5) {
        Write-Host "       ... and $($missingFiles.Count - 5) more" -ForegroundColor DarkRed
    }
    $failed = $true
}

if ($failed) { Write-Host "`n[RESULT] FAILED" -ForegroundColor Red; exit 1 }
Write-Host "`n[RESULT] PASSED" -ForegroundColor Green
exit 0

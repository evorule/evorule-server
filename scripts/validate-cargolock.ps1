#!/usr/bin/env pwsh
# validate-cargolock.ps1
# VERSION_STRATEGY.md 8
# Check: binary projects commit Cargo.lock, lib projects do not
# 各仓独立发布:仅校验本仓 crate,不查兄弟仓
# evorule-server workspace 共享仓根 Cargo.lock;evorule-server 是 binary crate,必须提交 Cargo.lock
# Exit code: 0 = pass, 1 = fail

[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot

$failed = $false
Write-Host "`n=== Cargo.lock Policy Validation (8) ===" -ForegroundColor Cyan

# workspace 共享仓根 Cargo.lock;只要有一个 binary crate,就必须提交
# evorule-server 有 [[bin]],workspace 为 binary workspace
$hasBinary = $true  # evorule-server 是 binary crate

$lockPath = Join-Path $repoRoot "Cargo.lock"
$giPath = Join-Path $repoRoot ".gitignore"

$lockExists = Test-Path $lockPath

# 检查 .gitignore 是否排除了「顶层」Cargo.lock
# evorule-server 的 .gitignore 用 **/Cargo.lock + !/Cargo.lock 模式:
#   **/Cargo.lock  排除所有(含子目录)
#   !/Cargo.lock   保留顶层
# 顶层被忽略 = .gitignore 中存在裸 /Cargo.lock 排除行(无 ! 前缀)
$giIgnoresTopLevelLock = $false
if (Test-Path $giPath) {
    $giLines = Get-Content $giPath
    foreach ($line in $giLines) {
        $trimmed = $line.Trim()
        # 跳过注释和空行
        if ($trimmed -eq '' -or $trimmed.StartsWith('#')) { continue }
        # 裸 /Cargo.lock 或 Cargo.lock(顶层)排除行
        if ($trimmed -match '^/Cargo\.lock\s*$' -or $trimmed -match '^Cargo\.lock\s*$') {
            $giIgnoresTopLevelLock = $true
        }
        # !/Cargo.lock 显式保留(覆盖前面的排除)
        if ($trimmed -match '^!/Cargo\.lock\s*$') {
            $giIgnoresTopLevelLock = $false
        }
    }
}

if ($hasBinary) {
    # binary workspace: 仓根 Cargo.lock 必须存在且不被 .gitignore 排除
    if ($lockExists) {
        Write-Host "[OK]   workspace (binary) : Cargo.lock exists at $repoRoot" -ForegroundColor Green
    } else {
        Write-Host "[FAIL] workspace (binary) : Cargo.lock not found at $repoRoot (must commit for binary project)" -ForegroundColor Red
        $failed = $true
    }
    if ($giIgnoresTopLevelLock) {
        Write-Host "[FAIL] workspace (binary) : .gitignore excludes top-level Cargo.lock (must commit)" -ForegroundColor Red
        $failed = $true
    } else {
        Write-Host "[OK]   workspace (binary) : .gitignore does NOT exclude top-level Cargo.lock" -ForegroundColor Green
    }
} else {
    if ($giIgnoresTopLevelLock) {
        Write-Host "[OK]   workspace (lib only) : .gitignore excludes Cargo.lock" -ForegroundColor Green
    } else {
        Write-Host "[INFO] workspace (lib only) : .gitignore does NOT exclude Cargo.lock (optional)" -ForegroundColor Cyan
    }
}

if ($failed) { Write-Host "`n[RESULT] FAILED" -ForegroundColor Red; exit 1 }
Write-Host "`n[RESULT] PASSED" -ForegroundColor Green
exit 0

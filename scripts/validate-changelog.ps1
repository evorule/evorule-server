#!/usr/bin/env pwsh
# validate-changelog.ps1
# VERSION_STRATEGY.md 4.5
# Check: CHANGELOG has section for current version + release-mode has no [Unreleased]/[未发布]
# 各仓独立发布:只校验本仓(evorule-server),不查兄弟仓
# Exit code: 0 = pass, 1 = fail

[CmdletBinding()]
param(
    [switch]$AllowUnreleased
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot

# 各仓独立发布:仅校验本仓(evorule-server workspace)
# UV-100: 路径必须用 Join-Path 拼接 —— 字符串内插 "$repoRoot\CHANGELOG.md"
# 在 Linux pwsh 下 Test-Path 兼容 \ 放行,但 .NET ReadAllText 严格不认 \ →
# FileNotFoundException(CI docs-check job 挂因);Join-Path 产平台正确分隔符
$projects = [ordered]@{}
$projects['evorule-server'] = @{ Version = (Join-Path $repoRoot 'Cargo.toml'); Changelog = (Join-Path $repoRoot 'CHANGELOG.md') }

function Read-Version {
    param([string]$Path)
    if (-not (Test-Path $Path)) { return $null }
    $content = Get-Content $Path -Raw
    # Multiline-aware: find first version = "..." on any line
    if ($content -match '(?m)^\s*version\s*=\s*"([^"]+)"') { return $Matches[1] }
    if ($content -match '(?m)"version"\s*:\s*"([^"]+)"') { return $Matches[1] }
    return $null
}

$failed = $false
Write-Host "`n=== Changelog Validation ===" -ForegroundColor Cyan

foreach ($name in $projects.Keys) {
    $p = $projects[$name]
    $version = Read-Version $p.Version
    if (-not $version) {
        Write-Host "[SKIP] $name : version not found at $($p.Version)" -ForegroundColor Yellow
        continue
    }
    if (-not (Test-Path $p.Changelog)) {
        Write-Host "[FAIL] $name : CHANGELOG not found at $($p.Changelog)" -ForegroundColor Red
        $failed = $true
        continue
    }

    # 用 UTF-8 读取避免中文乱码(PowerShell 5.1 + 无 BOM UTF-8 = GBK 误读)
    $content = [System.IO.File]::ReadAllText($p.Changelog, [System.Text.Encoding]::UTF8)

    # 4.5: every version has its own section
    $sectionPattern = "##\s*\[$([regex]::Escape($version))\]"
    if ($content -match $sectionPattern) {
        Write-Host "[OK]   $name : CHANGELOG has '## [$version]'" -ForegroundColor Green
    } else {
        Write-Host "[FAIL] $name : CHANGELOG missing '## [$version]'" -ForegroundColor Red
        $failed = $true
    }

    # 首段一致性 — CHANGELOG 第一个版本号段 ## [X.Y.Z] 的 X.Y.Z 必须 == Cargo.toml version
    # 自动跳过 ## [Unreleased] / ## [未发布](它们不匹配 \d+\.\d+\.\d+)
    # 拦住"Cargo.toml 改了版本但 CHANGELOG 还停在旧版本段"的问题
    $firstSectionMatch = [regex]::Match($content, '##\s*\[(\d+\.\d+\.\d+)\]')
    if ($firstSectionMatch.Success) {
        $firstVersion = $firstSectionMatch.Groups[1].Value
        if ($firstVersion -ne $version) {
            Write-Host "[FAIL] $name : CHANGELOG 首段为 '## [$firstVersion]' != Cargo.toml version ($version) — 版本号已 bump 但 CHANGELOG 未同步" -ForegroundColor Red
            $failed = $true
        } else {
            Write-Host "[OK]   $name : CHANGELOG 首段 '## [$firstVersion]' == Cargo.toml version" -ForegroundColor Green
        }
    } else {
        Write-Host "[WARN] $name : CHANGELOG 未找到任何 '## [X.Y.Z]' 版本号段" -ForegroundColor Yellow
    }

    # 4.5: release-mode forbids [Unreleased] / [未发布]
    if (-not $AllowUnreleased) {
        $hasUnreleased = $false
        if ($content -match '##\s*\[Unreleased\]') {
            Write-Host "[FAIL] $name : CHANGELOG has '## [Unreleased]' (release-mode forbidden)" -ForegroundColor Red
            $hasUnreleased = $true
            $failed = $true
        }
        if ($content -match '##\s*\[未发布\]') {
            Write-Host "[FAIL] $name : CHANGELOG has '## [未发布]' (release-mode forbidden)" -ForegroundColor Red
            $hasUnreleased = $true
            $failed = $true
        }
        if (-not $hasUnreleased) {
            Write-Host "[OK]   $name : CHANGELOG has no [Unreleased]/[未发布] section (release-mode clean)" -ForegroundColor Green
        }
    } else {
        Write-Host "[INFO] $name : -AllowUnreleased set, skipping [Unreleased]/[未发布] check (dev mode)" -ForegroundColor DarkGray
    }
}

if ($failed) { Write-Host "`n[RESULT] FAILED" -ForegroundColor Red; exit 1 }
Write-Host "`n[RESULT] PASSED" -ForegroundColor Green
exit 0

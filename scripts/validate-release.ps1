#!/usr/bin/env pwsh
# validate-release.ps1
# VERSION_STRATEGY.md 4.5, 10.1
# Release-time check: git tag matches + pre-release identifier + tag format
# 各仓独立发布:只校验本仓 git tag,不查兄弟仓
# Exit code: 0 = pass, 1 = fail
#
# 时序说明:
#   - 发布前就绪检查:用 -SkipTagCheck 跳过 tag 存在性检查(此时 tag 尚未创建)
#   - 发布后验证(默认):tag 必须存在,且与 Cargo.toml version 一致

[CmdletBinding()]
param(
    [switch]$SkipTagCheck
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot

$semverPattern = '^(\d+)\.(\d+)\.(\d+)(?:-([a-z]+)\.(\d+))?$'
$tagPattern = '^v\d+\.\d+\.\d+(-[a-z]+\.\d+)?$'

$failed = $false
Write-Host "`n=== Release Validation ===" -ForegroundColor Cyan

Push-Location $repoRoot
try {
    # 1. Current version
    $currentVersion = (Get-Content "Cargo.toml" | Where-Object { $_ -match '^version\s*=' } | Select-Object -First 1) -replace '.*"([^"]+)".*', '$1'
    Write-Host "[INFO] Current version: $currentVersion" -ForegroundColor Cyan

    if ($currentVersion -notmatch $semverPattern) {
        Write-Host "[FAIL] Current version '$currentVersion' is not valid SemVer" -ForegroundColor Red
        exit 1
    }
    $preRelease = $Matches[4]
    if ($preRelease) {
        Write-Host "[INFO] Pre-release identifier: $preRelease" -ForegroundColor Cyan
    }

    # 2. git tag match — tag 不存在 FAIL(默认);-SkipTagCheck 跳过(发布前就绪检查用)
    if (-not $SkipTagCheck) {
        $tagName = "v$currentVersion"
        $tagExists = git tag --list $tagName
        if ($tagExists) {
            Write-Host "[OK]   git tag '$tagName' exists" -ForegroundColor Green
        } else {
            Write-Host "[FAIL] git tag '$tagName' does not exist — 发布验证要求 tag 已创建(用 'git tag $tagName' 创建;发布前就绪检查用 -SkipTagCheck 跳过)" -ForegroundColor Red
            $failed = $true
        }

        # 不应有比当前版本更大的已发布 tag(说明 Cargo.toml 版本号落后于已发布 tag)
        # 拦住"打了 v0.1.2 tag 但 Cargo.toml 还停在 0.1.1"的问题
        $cm = [int]$Matches[1]; $cn = [int]$Matches[2]; $cp = [int]$Matches[3]
        $allVersionTags = @(git tag --list 'v*' 2>$null | Where-Object { $_ -match '^v\d+\.\d+\.\d+$' })
        $futureTags = @()
        foreach ($t in $allVersionTags) {
            if ($t -match '^v(\d+)\.(\d+)\.(\d+)$') {
                $tm = [int]$Matches[1]; $tn = [int]$Matches[2]; $tp = [int]$Matches[3]
                $tagGreater = $false
                if ($tm -gt $cm) { $tagGreater = $true }
                elseif ($tm -eq $cm -and $tn -gt $cn) { $tagGreater = $true }
                elseif ($tm -eq $cm -and $tn -eq $cn -and $tp -gt $cp) { $tagGreater = $true }
                if ($tagGreater) { $futureTags += $t }
            }
        }
        if ($futureTags.Count -gt 0) {
            Write-Host "[FAIL] 存在比 Cargo.toml version ($cm.$cn.$cp) 更大的已发布 tag: $($futureTags -join ', ') — Cargo.toml 版本号落后于已发布 tag" -ForegroundColor Red
            $failed = $true
        } else {
            Write-Host "[OK]   无比当前版本更大的已发布 tag(版本号未落后)" -ForegroundColor Green
        }
    } else {
        Write-Host "[INFO] -SkipTagCheck set, skipping tag existence/consistency check (pre-release readiness)" -ForegroundColor DarkGray
    }

    # 3. Version tags format check (only check 'v'-prefixed tags)
    # Non-version tags (e.g. checkpoint-*) are development markers, not releases
    $allTags = @(git tag --list 2>$null)
    if ($allTags.Count -eq 0) {
        Write-Host "[INFO] No git tags yet" -ForegroundColor Cyan
    } else {
        $versionTags = @($allTags | Where-Object { $_ -match '^v' })
        $nonVersionTags = @($allTags | Where-Object { $_ -notmatch '^v' })
        if ($nonVersionTags.Count -gt 0) {
            Write-Host "[INFO] Non-version tags (ignored): $($nonVersionTags -join ', ')" -ForegroundColor DarkGray
        }
        if ($versionTags.Count -eq 0) {
            Write-Host "[INFO] No version tags (v*) yet" -ForegroundColor Cyan
        } else {
            $badTags = @($versionTags | Where-Object { $_ -notmatch $tagPattern })
            if ($badTags.Count -gt 0) {
                Write-Host "[FAIL] Bad version tag format: $($badTags -join ', ')" -ForegroundColor Red
                $failed = $true
            } else {
                Write-Host "[OK]   All $($versionTags.Count) version tags have 'vX.Y.Z' format" -ForegroundColor Green
            }
        }
    }

    # 4. [patch.crates-io] 段检测 —— 发布前必须移除（RELEASE_PROCESS.md §1.2 第 5 项）
    # 本地开发 path 覆盖（evorule-tcb/reactor/governance/bundle）不得进入发布包；
    # 用户 clone 后 path 不存在会静默回退到 crates.io 版本，但 evorule-bundle 等从未发布，
    # 会导致构建失败。发布前必须移除 patch 段，依赖全部来自 crates.io。
    $cargoToml = Get-Content "Cargo.toml" -Raw
    if ($cargoToml -match '\[patch\.crates-io\]') {
        Write-Host "[FAIL] Cargo.toml 含 [patch.crates-io] 段 — 本地开发 path 覆盖不得进入发布包, 发布前必须移除该段" -ForegroundColor Red
        $failed = $true
    } else {
        Write-Host "[OK]   Cargo.toml 无 [patch.crates-io] 段（发布依赖全部来自 crates.io）" -ForegroundColor Green
    }

    # 5. Status
    if (-not $preRelease) {
        Write-Host "[INFO] Stable version, ready for release" -ForegroundColor Cyan
    } else {
        Write-Host "[INFO] Pre-release, do NOT publish to crates.io/npm/PyPI until stable" -ForegroundColor Yellow
    }
} finally {
    Pop-Location
}

if ($failed) { Write-Host "`n[RESULT] FAILED" -ForegroundColor Red; exit 1 }
Write-Host "`n[RESULT] PASSED" -ForegroundColor Green
exit 0

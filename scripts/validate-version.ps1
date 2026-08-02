#!/usr/bin/env pwsh
# validate-version.ps1
# VERSION_STRATEGY.md 4.5
# Check: workspace + 本仓内部 crate 版本号一致性(SemVer 2.0 + MAJOR 一致 + FULL version 一致)
# 各仓独立发布:仅校验本仓(evorule-server),不查兄弟仓
# Exit code: 0 = pass, 1 = fail

param(
    [switch]$Quiet
)

$ErrorActionPreference = 'Stop'
trap {
    Write-Host ("TRAP EXCEPTION: " + $_.Exception.GetType().FullName + " :: " + $_.Exception.Message)
    if ($_.InvocationInfo) {
        Write-Host ("TRAP INVOCATION: Line=" + $_.InvocationInfo.ScriptLineNumber + " :: " + $_.InvocationInfo.Line)
    }
    Write-Host ("TRAP STACK: " + $_.ScriptStackTrace)
    exit 99
}

$repoRoot = Split-Path -Parent $PSScriptRoot
$semverPattern = '^(\d+)\.(\d+)\.(\d+)(?:-([a-z]+)\.(\d+))?$'

function Get-TomlVersion {
    param([Parameter(Mandatory=$false)][AllowNull()][string]$Path)
    if ([string]::IsNullOrEmpty($Path)) { return $null }
    if (-not (Test-Path -LiteralPath $Path)) { return $null }
    $lines = Get-Content -LiteralPath $Path
    # 优先识别显式 version = "X.Y.Z"
    $line = $lines | Where-Object { $_ -match '^\s*version\s*=\s*"' } | Select-Object -First 1
    if ($line -and $line -match '"([^"]+)"') { return $Matches[1] }
    # 识别 version.workspace = true(继承 workspace 版本,视为与 workspace 一致)
    $wsLine = $lines | Where-Object { $_ -match '^\s*version\.workspace\s*=\s*true' } | Select-Object -First 1
    if ($wsLine) { return '::workspace::' }
    return $null
}

# 各仓独立发布:仅校验本仓(evorule-server workspace + 本仓内部 crate),不查兄弟仓
$projects = [ordered]@{}
$projects['evorule-server-workspace']    = (Join-Path $repoRoot "Cargo.toml")
$projects['evorule-auth']                = (Join-Path $repoRoot "core\auth\Cargo.toml")
$projects['evorule-debug-control']       = (Join-Path $repoRoot "core\debug_control\Cargo.toml")
$projects['evorule-hot-reload']          = (Join-Path $repoRoot "core\hot_reload\Cargo.toml")
$projects['evorule-io-handlers']         = (Join-Path $repoRoot "core\io_handlers\Cargo.toml")
$projects['evorule-metrics']             = (Join-Path $repoRoot "core\metrics\Cargo.toml")
$projects['evorule-rule-tools']          = (Join-Path $repoRoot "core\rule_tools\Cargo.toml")
$projects['evorule-semantic-invariants'] = (Join-Path $repoRoot "core\semantic_invariants\Cargo.toml")
$projects['evorule-time-machine']        = (Join-Path $repoRoot "core\time_machine\Cargo.toml")
$projects['evorule-server']              = (Join-Path $repoRoot "evorule-server\Cargo.toml")

$versions = [ordered]@{}
$failed = $false
if (-not $Quiet) { Write-Host "`n=== Version Validation ===" -ForegroundColor Cyan }
foreach ($name in $projects.Keys) {
    $p = $projects[$name]
    $v = Get-TomlVersion $p
    $versions[$name] = $v
    if ($null -eq $v) {
        Write-Host "[SKIP] $name : version not found (file may not exist)" -ForegroundColor Yellow
        continue
    }
    if ($v -eq '::workspace::') {
        Write-Host "[OK]   $name : workspace inherited (version.workspace = true)" -ForegroundColor Green
        continue
    }
    if ($v -notmatch $semverPattern) {
        Write-Host "[FAIL] $name : '$v' is not valid SemVer 2.0" -ForegroundColor Red
        $failed = $true
    } else {
        Write-Host "[OK]   $name : $v" -ForegroundColor Green
    }
}

$canonicalVersion = $versions['evorule-server-workspace']

# 本仓内部 crate 必须与 workspace 完全一致(FULL version 比较,含 PATCH)
$internalCrates = @(
    'evorule-auth','evorule-debug-control','evorule-hot-reload','evorule-io-handlers',
    'evorule-metrics','evorule-rule-tools','evorule-semantic-invariants','evorule-time-machine',
    'evorule-server'
)
if ($canonicalVersion -and $canonicalVersion -match $semverPattern) {
    Write-Host ""
    foreach ($name in $internalCrates) {
        $v = $versions[$name]
        if ($v) {
            if ($v -eq '::workspace::') {
                Write-Host "[OK]   $name : workspace inherited (== workspace $canonicalVersion)" -ForegroundColor Green
            } elseif ($v -ne $canonicalVersion) {
                Write-Host "[FAIL] $name ($v) != workspace ($canonicalVersion) — 本仓内部 crate 必须与 workspace 版本完全一致(含 PATCH)" -ForegroundColor Red
                $failed = $true
            } else {
                Write-Host "[OK]   $name ($v) == workspace" -ForegroundColor Green
            }
        }
    }
}

# MAJOR 一致性(本仓所有项目)
$parsedMajors = @()
foreach ($name in $versions.Keys) {
    $v = $versions[$name]
    if ($v -and $v -match $semverPattern) { $parsedMajors += [int]$Matches[1] }
}
$uniqueMajors = $parsedMajors | Sort-Object -Unique
if ($uniqueMajors.Count -gt 1) {
    Write-Host "`n[FAIL] MAJOR mismatch: $($uniqueMajors -join ', ')" -ForegroundColor Red
    $failed = $true
} elseif ($uniqueMajors.Count -eq 1) {
    Write-Host "`n[OK]   All projects share MAJOR = $($uniqueMajors[0])" -ForegroundColor Green
}

# === 通用 L1 文档版本号扫描 ===
# 扫描所有 L1 .md 中的 v\d+\.\d+\.\d+ 字面量,与 Cargo.toml canonical 不一致即 FAIL
# 白名单:
#   - CHANGELOG.md(历史段含多个版本)
#   - 废弃文档(顶部 [已废弃] 横幅)
#   - 审计/威胁模型文档(文件名含 AUDIT/THREAT_MODEL,审计版本与代码版本独立)
#   - 未来版本(>canonical,如路线图 0.2.0/1.0.0)
#   - canonical 自身
# 注:\b 保证 _v0.1.0.md(文件名引用)不被误匹配
if ($canonicalVersion -and $canonicalVersion -match '^(\d+)\.(\d+)\.(\d+)$') {
    $cm = [int]$Matches[1]; $cn = [int]$Matches[2]; $cp = [int]$Matches[3]
    if (-not $Quiet) { Write-Host "`n=== L1 Document Version Scan (canonical = v$canonicalVersion) ===" -ForegroundColor Cyan }

    $l1Files = @()
    $l1Files += Get-ChildItem -LiteralPath $repoRoot -Filter *.md -File -ErrorAction SilentlyContinue
    $docsDir = Join-Path $repoRoot "docs"
    if (Test-Path -LiteralPath $docsDir) {
        $l1Files += Get-ChildItem -LiteralPath $docsDir -Recurse -Filter *.md -File -ErrorAction SilentlyContinue
    }
    $crateSubPaths = @(
        'core\auth','core\debug_control','core\hot_reload','core\io_handlers',
        'core\metrics','core\rule_tools','core\semantic_invariants','core\time_machine',
        'evorule-server'
    )
    foreach ($sub in $crateSubPaths) {
        $crateDir = Join-Path $repoRoot $sub
        if (Test-Path -LiteralPath $crateDir) {
            $l1Files += Get-ChildItem -LiteralPath $crateDir -Filter *.md -File -ErrorAction SilentlyContinue
        }
    }

    # 匹配 v\d+\.\d+\.\d+ 但排除文件名引用(如 SECURITY_AUDIT_v0.1.0.md 中的 _v0.1.0)
    # 负向后行断言: v 前面不能是字母或下划线(否则是文件名/标识符的一部分)
    $versionLiteralPattern = '(?<![a-zA-Z_])v(\d+\.\d+\.\d+)\b'
    $scanFailed = $false
    foreach ($f in $l1Files) {
        $relName = $f.FullName.Substring($repoRoot.Length + 1)
        # CHANGELOG 白名单:根 CHANGELOG.md + 子 crate CHANGELOG.md(历史段含多版本,合法)
        if ($relName -match 'CHANGELOG\.md$') { continue }
        $content = [System.IO.File]::ReadAllText($f.FullName, [System.Text.Encoding]::UTF8)
        # 废弃文档跳过
        $headLen = [Math]::Min(2000, $content.Length)
        if ($content.Substring(0, $headLen) -match '\[已废弃\]') { continue }
        # 审计/威胁模型文档跳过(版本绑定审计批次)
        # SECURITY.md 含版本支持表(历史边界声明如 < v0.1.0,合法)
        if ($relName -match 'AUDIT|THREAT_MODEL|^SECURITY\.md$') { continue }

        $seen = @{}
        foreach ($m in [regex]::Matches($content, $versionLiteralPattern)) {
            $ver = $m.Groups[1].Value
            if ($seen.ContainsKey($ver)) { continue }
            $seen[$ver] = $true
            if ($ver -eq $canonicalVersion) { continue }
            # 未来版本允许(路线图/版本语义表/回滚流程的下一个版本)
            if ($ver -match '^(\d+)\.(\d+)\.(\d+)$') {
                $fm = [int]$Matches[1]; $fn = [int]$Matches[2]; $fp = [int]$Matches[3]
                $isFuture = ($fm -gt $cm) -or
                            ($fm -eq $cm -and $fn -gt $cn) -or
                            ($fm -eq $cm -and $fn -eq $cn -and $fp -gt $cp)
                if ($isFuture) { continue }
            }
            Write-Host "[FAIL] $relName contains 'v$ver' (expected v$canonicalVersion or future version)" -ForegroundColor Red
            $failed = $true; $scanFailed = $true
        }
    }
    if (-not $scanFailed) {
        Write-Host "[OK]   L1 docs contain no stale version literals (CHANGELOG/废弃/审计/未来版本 白名单)" -ForegroundColor Green
    }
}

if ($failed) {
    Write-Host "`n[RESULT] FAILED" -ForegroundColor Red
    exit 1
}
Write-Host "`n[RESULT] PASSED" -ForegroundColor Green
exit 0

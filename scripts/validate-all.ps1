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
# 包含 10 项检查:
#   0. 门禁绕过检测 (EVORULE_SKIP_GATE 环境变量)
#   1-5. validate-version / validate-changelog / validate-license / validate-cargolock / validate-release
#   6. check_doc_safety.py (文档安全 + 交叉引用完整性 + 基调合规)
#   7. check_schema_sync.py (内嵌 Schema 与 evorule-system-rules 源仓一致性)
#   8. scan_constitution_parity.py (宪法语义手写提取禁令)
#   9. conformance 环测试 (金样例装载档, 宪法仓兄弟目录在时执行)
#   10. check_k6_metrics.ps1 (K6 双指标: 金样例覆盖率 + 报错向导覆盖率)

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

# === 8. scan_constitution_parity.py (宪法语义手写提取禁令, [CON-L4-001]/[CON-L4-002]) ===
# 模式表: P1 transform 直取 / P2 硬编码指令白名单 / P3 手写 serde_to_tcb;
# 豁免清单 scripts/allowed_exceptions.conf (显式登记: 理由|责任人|复审期);
# 命中未豁免 = exit 1 阻断 (CI 红灯), 注释关闭=后门被禁
Write-Host "`n>>> [scan_constitution_parity] <<<" -ForegroundColor Magenta
$parityScript = Join-Path $repoRoot "scripts\scan_constitution_parity.py"
& python $parityScript
if ($LASTEXITCODE -ne 0) {
    $failed += 'scan_constitution_parity'
}

# === 9. conformance 环测试 ([CON-R-005]/[CON-L4-003]) ===
# 金样例装载档合法集全过/非法集全拒（真装载路径闭环件）。
# 宪法仓兄弟目录在时执行，不在时 [SKIP]（CI 侧由显式 checkout/clone 保证在场）。
Write-Host "`n>>> [conformance] <<<" -ForegroundColor Magenta
$rulesCandidate = Join-Path (Split-Path -Parent $repoRoot) "evorule-system-rules"
$rulesAnchor = Join-Path (Join-Path $rulesCandidate "schemas") (Join-Path "rule_set" "v1.0.json")
if (-not (Test-Path $rulesAnchor)) {
    if ($env:EVORULE_RULES_SRC -and (Test-Path (Join-Path (Join-Path $env:EVORULE_RULES_SRC "schemas") (Join-Path "rule_set" "v1.0.json")))) {
        $rulesCandidate = $env:EVORULE_RULES_SRC
        $rulesAnchor = Join-Path (Join-Path $rulesCandidate "schemas") (Join-Path "rule_set" "v1.0.json")
    }
}
if (Test-Path $rulesAnchor) {
    Push-Location $repoRoot
    try {
        & cargo test -p evorule-rule-loader --test conformance
        if ($LASTEXITCODE -ne 0) { $failed += 'conformance' }
    } finally {
        Pop-Location
    }
} else {
    Write-Host "[SKIP] 宪法仓不可得($rulesCandidate)，跳过 conformance 环测试（CI 由 checkout 保证在场）" -ForegroundColor DarkGray
}

# === 10. K6 双指标 ([CON-K6-001]/[CON-L4-004]) ===
# 金样例覆盖率 + 报错向导覆盖率（四段式模板测试），任一退化=红灯
Write-Host "`n>>> [k6-metrics] <<<" -ForegroundColor Magenta
if (Test-Path $rulesAnchor) {
    & pwsh -NoProfile -File (Join-Path $repoRoot "scripts\check_k6_metrics.ps1")
    if ($LASTEXITCODE -ne 0) {
        $failed += 'k6-metrics'
    }
} else {
    Write-Host "[SKIP] 宪法仓不可得，跳过 K6 双指标（CI 由 checkout 保证在场）" -ForegroundColor DarkGray
}

# === SUMMARY ===
Write-Host "`n=========== SUMMARY ===========" -ForegroundColor Cyan
$totalChecks = 1 + $scripts.Count + 5  # gate-bypass + 5 scripts + doc-safety + schema-sync + constitution-parity + conformance + k6-metrics
if ($failed.Count -gt 0) {
    Write-Host "FAILED: $($failed -join ', ')" -ForegroundColor Red
    exit 1
}
Write-Host "ALL $totalChecks CHECKS PASSED (gate-bypass + 5 validate scripts + doc-safety + schema-sync + constitution-parity + conformance + k6-metrics)" -ForegroundColor Green
exit 0

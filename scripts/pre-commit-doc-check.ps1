<#
.SYNOPSIS
    EvoRule pre-commit 文档同步检查钩子
.DESCRIPTION
    根据 git 暂存区的源码变更，自动提示需要同步更新的文档文件。
    这是提示性钩子，不会阻断 commit（exit code 始终为 0）。
    详细映射表见 docs/CODE_DOC_MAP.md
.NOTES
    安装方式:
      方式一（推荐，全局生效）:
        git config core.hooksPath .githooks
        然后将本脚本复制为 .githooks/pre-commit（无扩展名）
      方式二（单仓生效）:
        复制本脚本到 .git/hooks/pre-commit（无扩展名）
    手动运行:
      powershell -ExecutionPolicy Bypass -File scripts/pre-commit-doc-check.ps1
#>

$ErrorActionPreference = "SilentlyContinue"

# 设置 UTF-8 输出编码（改善 git hook / bash 调用场景下的中文显示）
try {
    [Console]::OutputEncoding = [System.Text.Encoding]::UTF8
    $OutputEncoding = [System.Text.Encoding]::UTF8
} catch {}

# --- 获取暂存区文件列表 ---
$stagedFiles = git diff --cached --name-only 2>$null
if (-not $stagedFiles) {
    Write-Host "[doc-check] 暂存区为空，跳过文档检查。" -ForegroundColor Gray
    exit 0
}

# --- 按类型分类 ---
$sourceFiles = @()
$docFiles = @()
$configFiles = @()
foreach ($f in $stagedFiles) {
    if ($f -match '\.(rs|json)$') { $sourceFiles += $f }
    elseif ($f -match '\.(md|markdown)$') { $docFiles += $f }
    elseif ($f -match '(Cargo\.toml|build\.rs|\.githooks|scripts/)') { $configFiles += $f }
}

if ($sourceFiles.Count -eq 0 -and $configFiles.Count -eq 0) {
    Write-Host "[doc-check] 本次暂存无源码/配置变更，跳过文档检查。" -ForegroundColor Gray
    exit 0
}

# --- 定义 crate -> 必查文档映射 ---
$crateDocs = @{
    "evorule-tcb" = @(
        "evorule-tcb/TCB_SPEC.md",
        "evorule-tcb/README.md",
        "CHANGELOG.md"
    )
    "evorule-reactor" = @(
        "evorule-reactor/REACTOR_SPEC.md",
        "evorule-reactor/README.md",
        "CHANGELOG.md"
    )
    "evorule-governance" = @(
        "evorule-governance/GOVERNANCE_SPEC.md",
        "evorule-governance/README.md",
        "CHANGELOG.md"
    )
    "evorule-cli" = @(
        "evorule-cli/CLI_SPEC.md",
        "evorule-cli/README.md",
        "evorule-cli/CHANGELOG.md",
        "CHANGELOG.md"
    )
}

# --- 定义特殊文件 -> 额外文档映射 ---
$specialFileDocs = @{
    "build.rs" = @("GATE_REFERENCE.md")
    "core_eval.json" = @(
        "evorule-tcb/TCB_SPEC.md",
        "docs/tutorial/02-ReAct循环示例.md",
        "docs/tutorial/03-写一条业务规则.md",
        "README.md"
    )
    "src/transition.rs" = @("docs/tutorial/01-五分钟跑通-core-eval.md")
    "src/domain.rs" = @("docs/tutorial/03-写一条业务规则.md")
    "src/reactor.rs" = @("docs/tutorial/02-ReAct循环示例.md")
    "src/fact.rs" = @("evorule-governance/GOVERNANCE_SPEC.md")
    "src/facts_log.rs" = @("evorule-governance/GOVERNANCE_SPEC.md")
    "src/io_handler.rs" = @("evorule-governance/GOVERNANCE_SPEC.md")
    "src/io_dispatcher.rs" = @("evorule-governance/GOVERNANCE_SPEC.md")
    "src/hash.rs" = @("evorule-governance/GOVERNANCE_SPEC.md")
    "src/auditor.rs" = @("evorule-reactor/REACTOR_SPEC.md")
    "tests/kani/" = @("evorule-tcb/verification/kani-formal-verification-design.md")
    "verification/kani_proofs.rs" = @("evorule-reactor/docs/KANI.md")
}

# --- 收集需要检查的文档 ---
$docsToCheck = New-Object System.Collections.Generic.HashSet[string]
$affectedCrates = New-Object System.Collections.Generic.HashSet[string]

foreach ($f in $stagedFiles) {
    # 检测 crate
    foreach ($crate in $crateDocs.Keys) {
        if ($f -like "$crate/*") {
            [void]$affectedCrates.Add($crate)
            foreach ($d in $crateDocs[$crate]) { [void]$docsToCheck.Add($d) }
        }
    }
    # 检测特殊文件
    foreach ($pattern in $specialFileDocs.Keys) {
        if ($f -like "*$pattern*") {
            foreach ($d in $specialFileDocs[$pattern]) { [void]$docsToCheck.Add($d) }
        }
    }
}

# build.rs 变更影响所有 crate 的 SPEC 门禁章节
if ($stagedFiles -like "*build.rs") {
    [void]$docsToCheck.Add("GATE_REFERENCE.md")
    foreach ($crate in $crateDocs.Keys) {
        $specName = if ($crate -eq "evorule-cli") { "CLI_SPEC.md" } else { ($crate -replace "evorule-","").ToUpper() + "_SPEC.md" }
        [void]$docsToCheck.Add("$crate/$specName")
    }
}

if ($docsToCheck.Count -eq 0) {
    Write-Host "[doc-check] 未识别到需要同步文档的源码变更。" -ForegroundColor Gray
    exit 0
}

# --- 输出结果 ---
Write-Host ""
Write-Host "========================================" -ForegroundColor Cyan
Write-Host "  EvoRule 文档同步检查 (pre-commit)" -ForegroundColor Cyan
Write-Host "========================================" -ForegroundColor Cyan
Write-Host ""

Write-Host "本次暂存的源码/配置文件 ($($sourceFiles.Count + $configFiles.Count)):" -ForegroundColor White
foreach ($f in ($sourceFiles + $configFiles)) {
    Write-Host "  - $f" -ForegroundColor Gray
}
Write-Host ""

if ($affectedCrates.Count -gt 0) {
    Write-Host "受影响的 crate: $($affectedCrates -join ', ')" -ForegroundColor Yellow
    Write-Host ""
}

Write-Host "建议检查/更新的文档 ($($docsToCheck.Count)):" -ForegroundColor White
$missingDocs = @()
foreach ($d in ($docsToCheck | Sort-Object)) {
    $isStaged = $docFiles -contains $d
    $exists = Test-Path $d
    if ($isStaged) {
        Write-Host "  [x] $d" -ForegroundColor Green
    } elseif ($exists) {
        Write-Host "  [ ] $d  (未暂存)" -ForegroundColor Yellow
        $missingDocs += $d
    } else {
        Write-Host "  [!] $d  (文件不存在，可能需要创建)" -ForegroundColor Red
    }
}
Write-Host ""

# --- 提示 ---
if ($missingDocs.Count -gt 0) {
    Write-Host "注意: 以上 $($missingDocs.Count) 个文档未在本次暂存中。" -ForegroundColor Yellow
    Write-Host "      如果本次变更影响了这些文档，请 git add 后再 commit。" -ForegroundColor Yellow
    Write-Host "      如果确认无需更新，可忽略此提示（本钩子不阻断 commit）。" -ForegroundColor Gray
} else {
    Write-Host "所有建议文档均已暂存，文档同步检查通过。" -ForegroundColor Green
}

Write-Host ""
Write-Host "详细映射表: docs/CODE_DOC_MAP.md" -ForegroundColor Cyan
Write-Host "手动运行:   powershell -File scripts/pre-commit-doc-check.ps1" -ForegroundColor Cyan
Write-Host "========================================" -ForegroundColor Cyan
Write-Host ""

# 提示性钩子，始终 exit 0
exit 0

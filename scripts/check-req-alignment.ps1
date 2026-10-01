# Workspace 成员依赖版本一致性门禁：全 workspace Cargo.toml 中 evorule-*
# registry req 的主线段必须与 workspace.version 主线段一致，漂移即退出码 1。
# 背景：patch/git 依赖形态会把 req 重定向到 git 源，漂移在本地 solve 时不可见，
# patch 退役瞬间才暴露（registry 无匹配版本）。本脚本供 CI 与发版预检共用，
# 自包含（无跨仓脚本依赖）。
# 用法：pwsh -NoProfile -File scripts/check-req-alignment.ps1 [-Repo <repo>]
param([string]$Repo = (Split-Path $PSScriptRoot -Parent))
$ErrorActionPreference = 'Stop'

$wsCargo = Join-Path $Repo 'Cargo.toml'
if (-not (Test-Path $wsCargo)) { Write-Output "ERROR: no Cargo.toml at $Repo"; exit 2 }

$wsVer = $null
$wsVerFull = $null
foreach ($line in Get-Content $wsCargo) {
    if ($line -match '^\s*version\s*=\s*"(\d+\.\d+)\.\d+"') { $wsVer = $Matches[1] }
    if ($line -match '^\s*version\s*=\s*"(\d+\.\d+\.\d+)"') { $wsVerFull = $Matches[1]; break }
}
if (-not $wsVer) { Write-Output 'ERROR: workspace version not found'; exit 2 }
$major = ($wsVer -split '\.')[0]

$bad = @()
foreach ($cargo in (Get-ChildItem $Repo -Recurse -Filter Cargo.toml)) {
    if ($cargo.FullName -match '\\target\\|\\fuzz\\') { continue }
    foreach ($line in Get-Content $cargo.FullName) {
        if ($line -match '^\s*evorule-(tcb|reactor|governance|discipline)\s*=\s*\{\s*version\s*=\s*"(\d+)\.') {
            if ($Matches[2] -ne $major) {
                $bad += ('{0}: {1}' -f $cargo.FullName.Substring($Repo.Length + 1), $line.Trim())
            }
        }
    }
}

# README 版本徽章随动（双语多 badge 全查）
$readme = Join-Path $Repo 'README.md'
if (Test-Path $readme) {
    foreach ($line in Get-Content $readme) {
        if ($line -match 'img\.shields\.io/badge/version-(\d+\.\d+\.\d+)-') {
            if ($Matches[1] -ne $wsVerFull) {
                $bad += ("README.md version badge = {0} (expected {1})" -f $Matches[1], $wsVerFull)
            }
        }
    }
}

if ($bad.Count -gt 0) {
    Write-Output ("FAIL: workspace={0} but {1} drift(s):" -f $wsVer, $bad.Count)
    $bad | ForEach-Object { Write-Output "  $_" }
    exit 1
}
Write-Output "PASS: evorule-* req aligned to $major.x; README badge = $wsVerFull"
exit 0

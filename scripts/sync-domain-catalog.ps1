# 发射形态契约（exit-code 穿透铁律）：
#   必须 `pwsh -NoProfile -File scripts/sync-domain-catalog.ps1`。
#   禁用 `-Command "& { & script }"` 包装——调用操作符会吞掉脚本内 exit N，
#   pwsh 进程恒 rc=0，漂移守卫失败被静默放行。
# 域目录声明同步脚本
#
# SSOT : evorule-rule/src/model/domain_catalog.v1.json（治理侧域目录）
# 目的 : core/law_schema/src/domain_catalog.embedded.json
#        （include_str! 构建期嵌入；criticality 经 law_schema 公开面单源复用，
#        不持第二副本）
#
# 流程 = 节奏强制：复制 → 字节核验 → 双侧守卫测试
#   治理侧守卫：rule 仓域目录聚合守卫（cargo test domain_catalog）
#   执行侧守卫：law_schema 全量测试（域校验面 + golden 跨域引用边）
#
# 何时运行：域目录 SSOT 变更之后（先改 rule 仓 SSOT，跑本脚本同步嵌入副本；
#   双仓成对提交——先合入 rule 侧 SSOT，再合入本仓嵌入副本，与原生服务
#   声明同步的合并顺序同律）。
#
# 用法：
#   pwsh ./scripts/sync-domain-catalog.ps1                        # 默认按兄弟目录推导(检出根下 evorule-rule)
#   pwsh ./scripts/sync-domain-catalog.ps1 -RepoRule E:\evorule-rule
#   pwsh ./scripts/sync-domain-catalog.ps1 -Verify                # 校验模式（CI 用）
#
# -Verify 模式：不复制、不跑测试，仅对比 SSOT 与嵌入副本是否逐字节一致
#   （SHA256 对比，零 cargo 依赖）；任何漂移即退出码 1（fail-closed）。
#   用途：CI 漂移检查——SSOT 改动而忘记跑同步脚本时，此模式红灯拦截。
#
# 退出码 0 = 同步完成且双侧守卫全绿；非 0 = 存在失败项（如实退出）。

[CmdletBinding()]
param(
    [string]$RepoRule = "",
    [switch]$Verify
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
if (-not $RepoRule) { $RepoRule = Join-Path (Split-Path -Parent $repoRoot) 'evorule-rule' }

$ssot = Join-Path $RepoRule "src\model\domain_catalog.v1.json"
$embedded = Join-Path $repoRoot "core\law_schema\src\domain_catalog.embedded.json"

if ($Verify) {
    if (-not (Test-Path $ssot)) { Write-Host "[FAIL] 治理侧 SSOT 不存在: $ssot" -ForegroundColor Red; exit 1 }
    if (-not (Test-Path $embedded)) { Write-Host "[FAIL] 嵌入副本不存在: $embedded（SSOT 已改而未同步？）" -ForegroundColor Red; exit 1 }
    $hA = (Get-FileHash $ssot -Algorithm SHA256).Hash
    $hB = (Get-FileHash $embedded -Algorithm SHA256).Hash
    if ($hA -ne $hB) {
        Write-Host "[FAIL] 域目录嵌入副本与 SSOT 漂移（执行侧域校验将失真）— 请运行本脚本（无 -Verify）重新同步" -ForegroundColor Red
        exit 1
    }
    Write-Host "[OK] 域目录嵌入副本与 SSOT 逐字节一致" -ForegroundColor Green
    exit 0
}

if (-not (Test-Path (Join-Path $RepoRule "Cargo.toml"))) { Write-Host "[FAIL] 治理仓不存在: $RepoRule" -ForegroundColor Red; exit 1 }
if (-not (Test-Path $ssot)) { Write-Host "[FAIL] 治理侧 SSOT 不存在: $ssot" -ForegroundColor Red; exit 1 }

# ① 复制（只复制，不移动）+ ② 字节核验（复制后必须逐字节一致）
Copy-Item $ssot $embedded -Force
# 破 cargo 陈旧 fingerprint：Copy-Item 会连 LastWriteTime 一起复制，嵌入副本 mtime
# 回退到 SSOT 的旧时间戳 → include_str! 依赖不重编 → 守卫假绿。显式戳新时间。
(Get-Item $embedded).LastWriteTime = Get-Date
$lenA = (Get-Item $ssot).Length
$lenB = (Get-Item $embedded).Length
if ($lenA -ne $lenB) { Write-Host "[FAIL] 复制核验失败: SSOT=$lenA 副本=$lenB 字节不一致" -ForegroundColor Red; exit 1 }
Write-Host "[OK] 域目录嵌入副本已同步并核验（$lenA 字节）" -ForegroundColor Green

# ③ 双侧守卫
$failures = 0

Write-Host "=== [1/2] 治理侧守卫（rule 仓域目录聚合 schema/唯一性）===" -ForegroundColor Cyan
Push-Location $RepoRule
try {
    # 零匹配自检：cargo 对「过滤无测试」不报错、退出码恒 0——必须解析输出
    # 确认至少跑了守卫测试，否则守卫形式上绿、实质零覆盖。
    $output = cargo test domain_catalog 2>&1
    $output | ForEach-Object { Write-Host $_ }
    if ($LASTEXITCODE -ne 0) {
        $failures++
        Write-Host "[FAIL] 治理侧守卫未通过" -ForegroundColor Red
    } elseif ($output -match "running 0 tests") {
        $failures++
        Write-Host "[FAIL] 治理侧守卫过滤器零匹配——形式上绿、实质零覆盖" -ForegroundColor Red
    } else {
        Write-Host "[OK] 治理侧守卫通过" -ForegroundColor Green
    }
} finally { Pop-Location }

Write-Host "=== [2/2] 执行侧守卫（law_schema 域校验面全量）===" -ForegroundColor Cyan
Push-Location $repoRoot
try {
    cargo test -p evorule-law-schema
    $rc = $LASTEXITCODE
    if ($rc -ne 0) { $failures++; Write-Host "[FAIL] 执行侧守卫未通过 (exit=$rc)" -ForegroundColor Red }
    else { Write-Host "[OK] 执行侧守卫通过" -ForegroundColor Green }
} finally { Pop-Location }

if ($failures -gt 0) {
    Write-Host "`n同步未完成：$failures 项失败 — 守卫面需跟进修正" -ForegroundColor Red
    exit 1
}
Write-Host "`n域目录声明同步完成（SSOT → 嵌入副本，双侧守卫全绿）" -ForegroundColor Green

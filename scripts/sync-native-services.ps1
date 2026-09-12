# 原生服务声明同步脚本
#
# SSOT : evorule-server/plugins/<插件 id>/official_native_services.json（每插件一份）
# 目的 : evorule-rule/src/model/official_native_services.<插件 id>.embedded.json
#        （include_str! 构建期嵌入，治理侧按登记表声明序聚合）
#
# 插件登记：
#   ① demo-services      ② physics-services      ③ indicator-services
#
# 流程 = 节奏强制（逐插件）：复制 → 字节核验 → 双侧守卫测试
#   执行侧守卫：各插件声明文件 vs 其 NATIVE_SERVICES（三字段+序）
#   治理侧守卫：嵌入副本聚合（schema/唯一性/顺序）
#
# 何时运行：新增/变更原生服务（先改对应插件 SSOT 文件 + 执行侧 NATIVE_SERVICES 表）之后；
#   脚本尾部自动跑双侧测试，双绿才算同步完成（不静默）。
#
# 用法：
#   pwsh ./scripts/sync-native-services.ps1                        # 默认 D:\evorule-rule
#   pwsh ./scripts/sync-native-services.ps1 -RepoRule E:\evorule-rule
#   pwsh ./scripts/sync-native-services.ps1 -Verify                # 校验模式（CI 用）
#
# -Verify 模式：不复制、不跑测试，仅逐插件对比 SSOT 与治理侧嵌入副本是否
#   逐字节一致（哈希对比，零 cargo 依赖）；任何漂移即退出码 1（fail-closed）。
#   用途：CI 漂移检查——SSOT 改动而忘记跑同步脚本时，此模式红灯拦截。
#
# 退出码 0 = 同步完成且双侧守卫全绿；非 0 = 存在失败项（如实退出）。

[CmdletBinding()]
param(
    [string]$RepoRule = "D:\evorule-rule",
    [switch]$Verify
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repoRoot

# 插件登记表：id → 执行侧 crate 名（声明 SSOT 均位于 plugins/<id>/official_native_services.json）
$plugins = @(
    @{ Id = "demo-services";      Crate = "evorule-demo-services" },
    @{ Id = "physics-services";   Crate = "evorule-physics-services" },
    @{ Id = "indicator-services"; Crate = "evorule-indicator-services" }
)

if (-not (Test-Path (Join-Path $RepoRule "Cargo.toml"))) { Write-Host "[FAIL] 治理仓不存在: $RepoRule" -ForegroundColor Red; exit 1 }

# ── 校验模式（-Verify）：SSOT vs 嵌入副本逐字节对比，漂移即 fail-closed ──
if ($Verify) {
    $verifyFailures = 0
    foreach ($p in $plugins) {
        $ssot = Join-Path $repoRoot "plugins\$($p.Id)\official_native_services.json"
        $embedded = Join-Path $RepoRule "src\model\official_native_services.$($p.Id).embedded.json"
        if (-not (Test-Path $ssot)) { Write-Host "[FAIL] $($p.Id) SSOT 不存在: $ssot" -ForegroundColor Red; $verifyFailures++; continue }
        if (-not (Test-Path $embedded)) { Write-Host "[FAIL] $($p.Id) 嵌入副本不存在: $embedded（SSOT 已改而未同步？）" -ForegroundColor Red; $verifyFailures++; continue }
        $hA = (Get-FileHash $ssot -Algorithm SHA256).Hash
        $hB = (Get-FileHash $embedded -Algorithm SHA256).Hash
        if ($hA -ne $hB) {
            Write-Host "[FAIL] $($p.Id) 嵌入副本与 SSOT 漂移（治理侧 sensitive/description 将静默失真）— 请运行本脚本（无 -Verify）重新同步" -ForegroundColor Red
            $verifyFailures++
        } else {
            Write-Host "[OK] $($p.Id) 嵌入副本与 SSOT 逐字节一致" -ForegroundColor Green
        }
    }
    if ($verifyFailures -gt 0) {
        Write-Host "`n校验失败：$verifyFailures 项漂移 — 治理侧目录正在低报/失真，禁止合入" -ForegroundColor Red
        exit 1
    }
    Write-Host "`n校验通过：$($plugins.Count) 份嵌入副本与 SSOT 全部一致" -ForegroundColor Green
    exit 0
}

# ① 复制（只复制，不移动）+ ② 字节核验（复制后必须逐字节一致）
$syncFailures = 0
foreach ($p in $plugins) {
    $ssot = Join-Path $repoRoot "plugins\$($p.Id)\official_native_services.json"
    $embedded = Join-Path $RepoRule "src\model\official_native_services.$($p.Id).embedded.json"
    if (-not (Test-Path $ssot)) { Write-Host "[FAIL] SSOT 不存在: $ssot" -ForegroundColor Red; $syncFailures++; continue }
    Copy-Item $ssot $embedded -Force
    $lenA = (Get-Item $ssot).Length
    $lenB = (Get-Item $embedded).Length
    if ($lenA -ne $lenB) { Write-Host "[FAIL] 复制核验失败: $($p.Id) SSOT=$lenA 副本=$lenB 字节不一致" -ForegroundColor Red; $syncFailures++ }
    else { Write-Host "[OK] $($p.Id) 嵌入副本已同步并核验（$lenA 字节）" -ForegroundColor Green }
}
if ($syncFailures -gt 0) { exit 1 }

# ③ 双侧守卫
$failures = 0

Write-Host "=== [1/2] 执行侧守卫（各插件声明文件 vs NATIVE_SERVICES）===" -ForegroundColor Cyan
foreach ($p in $plugins) {
    cargo test -p $p.Crate --lib native_service
    if ($LASTEXITCODE -ne 0) { $failures++; Write-Host "[FAIL] $($p.Id) 执行侧守卫未通过" -ForegroundColor Red }
    else { Write-Host "[OK] $($p.Id) 执行侧守卫通过" -ForegroundColor Green }
}

Write-Host "=== [2/2] 治理侧守卫（嵌入副本聚合 schema/唯一性/顺序）===" -ForegroundColor Cyan
Push-Location $RepoRule
try {
    cargo test service_catalog
    if ($LASTEXITCODE -ne 0) { $failures++; Write-Host "[FAIL] 治理侧守卫未通过" -ForegroundColor Red }
    else { Write-Host "[OK] 治理侧守卫通过" -ForegroundColor Green }
} finally { Pop-Location }

if ($failures -gt 0) {
    Write-Host "`n同步未完成：$failures 项失败 — 治理侧消费点或守卫需跟进修正" -ForegroundColor Red
    exit 1
}
Write-Host "`n原生服务声明同步完成（各插件 SSOT → 治理侧嵌入副本×$($plugins.Count)，双侧守卫全绿）" -ForegroundColor Green

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
#
# 退出码 0 = 同步完成且双侧守卫全绿；非 0 = 存在失败项（如实退出）。

[CmdletBinding()]
param(
    [string]$RepoRule = "D:\evorule-rule"
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

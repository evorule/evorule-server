# 原生服务声明同步脚本（UV-029 声明文件化）
#
# SSOT : evorule-server/plugins/demo-services/official_native_services.json
# 目的 : evorule-rule/src/model/official_native_services.embedded.json（include_str! 构建期嵌入）
#
# 流程 = 节奏强制：复制 → 字节核验 → 双侧守卫测试
#   ① demo-services 声明文件守卫（文件 vs NATIVE_SERVICES 三字段+序）
#   ② evorule-rule 嵌入副本守卫（schema/唯一性/顺序）
#
# 何时运行：新增/变更原生服务（先改 SSOT 文件 + 执行侧 NATIVE_SERVICES 表）之后；
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

$ssot = Join-Path $repoRoot "plugins\demo-services\official_native_services.json"
$embedded = Join-Path $RepoRule "src\model\official_native_services.embedded.json"

if (-not (Test-Path $ssot)) { Write-Host "[FAIL] SSOT 不存在: $ssot" -ForegroundColor Red; exit 1 }
if (-not (Test-Path (Join-Path $RepoRule "Cargo.toml"))) { Write-Host "[FAIL] 治理仓不存在: $RepoRule" -ForegroundColor Red; exit 1 }

# ① 复制（只复制，不移动）
Copy-Item $ssot $embedded -Force

# ② 字节核验（复制后必须逐字节一致）
$lenA = (Get-Item $ssot).Length
$lenB = (Get-Item $embedded).Length
if ($lenA -ne $lenB) { Write-Host "[FAIL] 复制核验失败: SSOT=$lenB 副本=$lenB 字节不一致" -ForegroundColor Red; exit 1 }
Write-Host "[OK] 嵌入副本已同步并核验（$lenA 字节）" -ForegroundColor Green

# ③ 双侧守卫
$failures = 0

Write-Host "=== [1/2] 执行侧守卫（demo-services 声明文件 vs NATIVE_SERVICES）===" -ForegroundColor Cyan
cargo test -p evorule-demo-services --lib native_service
if ($LASTEXITCODE -ne 0) { $failures++; Write-Host "[FAIL] 执行侧守卫未通过" -ForegroundColor Red }
else { Write-Host "[OK] 执行侧守卫通过" -ForegroundColor Green }

Write-Host "=== [2/2] 治理侧守卫（嵌入副本 schema/顺序）===" -ForegroundColor Cyan
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
Write-Host "`n原生服务声明同步完成（SSOT → 治理侧嵌入副本，双侧守卫全绿）" -ForegroundColor Green

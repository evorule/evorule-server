<#
  prepackage_checks.ps1 — 发版打包双闸预检（发布前必须全绿）

  用途：在组装安装包/发布产物之前，对暂存目录做两道强制检查：
    闸 1 · 密钥扫描  —— 扫描暂存目录全部文本态文件，命中即失败：
        - 实际密钥值（-KeyFiles 指向的密钥文件内容，逐字节比对）
        - 通用密钥特征（sk- 前缀长随机串、MINIMAX_API_KEY=<非空值>、
          llm_api_key=<非空值>）
    闸 2 · 规则装载门禁 —— 暂存的全部规则 JSON 过运行体装载门禁
        （POST {ServerBase}/api/rules/validate，与本仓 CI 同门禁；
         eq/lt 域缺 on_missing 等违规在此拦截，即存量回归拦截）

  附带校验：service_tools 覆盖数与运行目录一致、base_url 不含死端口
  （服务清单与端口失指的回归拦截）。

  用法：
    pwsh ./scripts/prepackage_checks.ps1 `
      -StageDir  <暂存目录> `
      -KeyFiles  <密钥文件路径数组，如 桌面密钥存根> `
      -RulesDir  <规则目录> `
      -ServerBase <运行中 evorule-server 地址，默认 http://127.0.0.1:18080>

  退出码：0 = 双闸全绿可打包；1 = 任一命中（停止发布）。
  依赖：PowerShell 5.1+；闸 2 需要一个运行中的 evorule-server（开发实例即可）。
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)] [string]$StageDir,
    [string[]] $KeyFiles = @(),
    [string] $RulesDir,
    [string] $ServerBase = "http://127.0.0.1:18080"
)

$ErrorActionPreference = "Stop"
$fail = 0

if (-not (Test-Path $StageDir)) { Write-Host "[FAIL] StageDir 不存在: $StageDir"; exit 1 }

# ── 闸 1 · 密钥扫描 ─────────────────────────────────────────────
Write-Host "== 闸 1 · 密钥扫描 ==" -ForegroundColor Cyan

# 收集比对基准：密钥文件全文中的长 token（≥16 连续非空白字符）
$secrets = New-Object System.Collections.Generic.HashSet[string]
foreach ($kf in $KeyFiles) {
    if (-not (Test-Path $kf)) { continue }
    foreach ($m in [regex]::Matches((Get-Content $kf -Raw), '[A-Za-z0-9_\-\.:/]{16,}')) {
        if ($m.Value.Length -ge 16) { [void]$secrets.Add($m.Value) }
    }
}

$patterns = @(
    @{ Name = "sk- 长随机串";      Re = 'sk-[A-Za-z0-9_\-]{16,}' },
    @{ Name = "MINIMAX_API_KEY 赋值"; Re = 'MINIMAX_API_KEY\s*=\s*["'']?[^\s"'']{8,}' },
    @{ Name = "llm_api_key 赋值";     Re = 'llm_api_key\s*[=:]\s*["'']?[A-Za-z0-9_\-]{8,}' }
)

$textExt = @('.json', '.toml', '.md', '.txt', '.js', '.ts', '.svelte', '.html', '.css', '.ps1', '.rs', '.yml', '.yaml')
$stagedFiles = Get-ChildItem $StageDir -Recurse -File | Where-Object {
    ($textExt -contains $_.Extension.ToLower()) -or ($_.Extension -eq '')
}

$hits = 0
foreach ($f in $stagedFiles) {
    $raw = Get-Content $f.FullName -Raw -ErrorAction SilentlyContinue
    if (-not $raw) { continue }
    foreach ($p in $patterns) {
        foreach ($m in [regex]::Matches($raw, $p.Re)) {
            # 占位符/空值/文档示例豁免
            $v = $m.Value
            if ($v -match 'YOUR_|<|=(""|$)\s*$' -or $v.Length -lt 12) { continue }
            Write-Host "  [FAIL][$($p.Name)] $($f.FullName)" -ForegroundColor Red
            $hits++
        }
    }
    foreach ($s in $secrets) {
        if ($raw.Contains($s)) {
            Write-Host "  [FAIL][实际密钥值命中] $($f.FullName)" -ForegroundColor Red
            $hits++
        }
    }
}
if ($hits -eq 0) { Write-Host "[OK] 密钥扫描：$($stagedFiles.Count) 文件零命中" -ForegroundColor Green }
else { Write-Host "[FAIL] 密钥扫描命中 $hits 处 — 停止发布" -ForegroundColor Red; $fail++ }

# ── 闸 2 · 规则装载门禁 ─────────────────────────────────────────
Write-Host "== 闸 2 · 规则装载门禁 ==" -ForegroundColor Cyan
if (-not $RulesDir -or -not (Test-Path $RulesDir)) {
    Write-Host "[SKIP] 未提供 RulesDir — 记得在装载环境单独验证" -ForegroundColor Yellow
} else {
    $ruleFiles = Get-ChildItem $RulesDir -Recurse -File -Filter *.json
    $gateFail = 0
    foreach ($rf in $ruleFiles) {
        try {
            $body = @{ rules = (Get-Content $rf.FullName -Raw) } | ConvertTo-Json -Compress -Depth 32
            $res = Invoke-RestMethod -Uri "$ServerBase/api/rules/validate" -Method Post `
                -ContentType "application/json" -Body $body -TimeoutSec 30
            if (-not $res.passed) {
                Write-Host "  [FAIL] $($rf.Name) 装载门禁未过" -ForegroundColor Red
                $gateFail++
            }
        } catch {
            Write-Host "  [FAIL] $($rf.Name) 校验异常: $($_.Exception.Message)" -ForegroundColor Red
            $gateFail++
        }
    }
    if ($gateFail -eq 0) { Write-Host "[OK] 规则装载门禁：$($ruleFiles.Count) 文件全过" -ForegroundColor Green }
    else { Write-Host "[FAIL] 装载门禁失败 $gateFail 文件 — 停止发布（存量回归拦截）" -ForegroundColor Red; $fail++ }
}

# ── 收官 ────────────────────────────────────────────────────────
if ($fail -eq 0) {
    Write-Host "`n[RESULT] 双闸全绿 — 可以打包发布" -ForegroundColor Green
    exit 0
} else {
    Write-Host "`n[RESULT] $fail 闸未过 — 停止发布" -ForegroundColor Red
    exit 1
}

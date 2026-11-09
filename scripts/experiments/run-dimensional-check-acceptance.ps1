# 量纲一致性通道验收脚本（双通道 + 双跑 SHA256）
#
# 验收链路（确定性测量面 + 立法面裁决的真机端到端）：
#   通道 1 · 律式注册量纲校验（law_host 装配面）：
#     - 注入「长度 + 单价（米/币种混加）」律式集 → 装配拒收，报因含两侧量纲向量；
#     - 注入「跨币种换算」律式集（币种*汇率，异币种同货币维）→ 装配通过；
#     - 结论经 /api/companion/status law_sets 装配投影逐集断言。
#   通道 2 · dimension_check 立法指令（服务总线契约）：
#     - 会话指令 dimension_check {lhs, rhs, target} → 规则层经 call_service 桥
#       发射确定性原生服务 → 信封 consistent 经 eq 裁决 → payload.target 落标签；
#     - 断言：米/币种混加参数 → violation；跨币种同维参数 → pass。
#   双跑：上述两通道完整执行两次，归一化结果 JSON 逐字节 SHA256 全等
#   （确定性锚：同输入必同输出）。
#
# 用法：
#   pwsh ./scripts/experiments/run-dimensional-check-acceptance.ps1
#
# 退出码 0 = 全部断言通过且双跑 SHA256 全等；非 0 = 存在失败项（如实退出）。
# 说明：实验实例使用独立端口与独立 WAL 目录，不触碰常驻实例；结束后清理进程。

[CmdletBinding()]
param(
    [int]$Port = 18082
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
Set-Location $repoRoot

$failures = 0
$Exe = Join-Path $repoRoot "target\release\evorule-server.exe"
if (-not (Test-Path $Exe)) { Write-Error "release 二进制不存在: $Exe —— 先跑 cargo build --release" }

function Invoke-Measurement {
    <# 单次完整测量：起实例 → 通道 1 装配断言 → 通道 2 立法断言 → 收集归一化结果 → 停实例 #>
    $expRoot = Join-Path $repoRoot "data\experiments\dimensional-run"
    $lawDir = Join-Path $expRoot "law-sets"
    $walDir = Join-Path $expRoot "wal"
    Remove-Item -Recurse -Force $expRoot -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force -Path $lawDir | Out-Null
    New-Item -ItemType Directory -Force -Path $walDir | Out-Null

    # ---- 实验律式集（通道 1 注入料） ----
    # 集 A：长度与单价混加（跨量纲相加，量纲违例）——预期装配拒收
    @{ '$schema' = "https://evorule.org/schemas/law_set/v1.0.json"
       "kind" = "law_set"; "id" = "mixed-add-violation"; "version" = "1.0.0"
       "metadata" = @{ "title" = "mixed-add violation sample" }
       "laws" = @(@{
           "id" = "mixed_add"; "formula" = "total = length + price"
           "symbols" = @{ "total" = "total amount"; "length" = "length"; "price" = "unit price" }
           "si_units" = @{ "total" = "CNY"; "length" = "m"; "price" = "CNY*m^-1" }
       }) } | ConvertTo-Json -Depth 6 | Set-Content -Encoding UTF8 (Join-Path $lawDir "a-mixed-add.json")

    # 集 B：跨币种换算律（金额 * 汇率 = 换算金额；异币种同货币维，量纲一致）——预期装配通过
    @{ '$schema' = "https://evorule.org/schemas/law_set/v1.0.json"
       "kind" = "law_set"; "id" = "currency-convert"; "version" = "1.0.0"
       "metadata" = @{ "title" = "currency conversion sample" }
       "laws" = @(@{
           "id" = "usd_convert"; "formula" = "total_usd = amount_cny * rate"
           "symbols" = @{ "total_usd" = "amount in usd"; "amount_cny" = "amount in cny"; "rate" = "exchange rate" }
           "si_units" = @{ "total_usd" = "USD"; "amount_cny" = "CNY"; "rate" = "USD*CNY^-1" }
       }) } | ConvertTo-Json -Depth 6 | Set-Content -Encoding UTF8 (Join-Path $lawDir "b-convert.json")

    # ---- 起独立实验实例 ----
    $procArgs = @(
        "--addr", "127.0.0.1:$Port",
        "--profile", "development",
        "--insecure-serve",
        "--db-path", (Join-Path $expRoot "evorule.db"),
        "--workspace-db", (Join-Path $expRoot "workspace.db"),
        "--core-eval", (Join-Path $repoRoot "resources\server_eval.json"),
        "--no-rate-limit",
        "--allow-loopback",
        "--law-sets-dir", $lawDir,
        "--wal-dir", $walDir,
        "--log-file", (Join-Path $expRoot "server.log")
    )
    $proc = Start-Process -FilePath $Exe -ArgumentList $procArgs -PassThru -WindowStyle Hidden
    try {
        $ready = $false
        for ($i = 0; $i -lt 30; $i++) {
            Start-Sleep -Milliseconds 800
            try {
                $health = Invoke-RestMethod -Uri "http://127.0.0.1:$Port/api/health" -TimeoutSec 4
                if ($health.success) { $ready = $true; break }
            } catch {}
        }
        if (-not $ready) { throw "实验实例 60s 内未就绪（端口 $Port）" }

        $result = [ordered]@{}

        # ---- 权限引导（审计链前置）：放行 call_service I/O 出口 ----
        # 动态权限表默认 deny（fail-closed）；实验为本地 loopback 实例，
        # 覆盖式置 Active 一条 any→io:call_service allow 条目（生产走审批流）。
        $permBody = @{
            "id" = "exp-dim-check-io-allow"
            "subject" = @{ "subject_type" = "any"; "id" = "" }
            "resource" = @{ "resource_type" = "io_action"; "path" = "io:call_service" }
            "action" = "*"
            "effect" = "allow"
            "state" = "active"
        } | ConvertTo-Json -Depth 5 -Compress
        Invoke-RestMethod -Method Post -Uri "http://127.0.0.1:$Port/api/permissions" `
            -ContentType "application/json" -Body $permBody -TimeoutSec 8 | Out-Null
        Invoke-RestMethod -Method Post -Uri "http://127.0.0.1:$Port/api/permissions/exp-dim-check-io-allow/submit" `
            -TimeoutSec 8 | Out-Null
        Invoke-RestMethod -Method Post -Uri "http://127.0.0.1:$Port/api/permissions/exp-dim-check-io-allow/review" `
            -ContentType "application/json" -Body '{"approve": true}' -TimeoutSec 8 | Out-Null

        # ---- 通道 1：law_set 装配投影断言 ----
        $status = Invoke-RestMethod -Uri "http://127.0.0.1:$Port/api/companion/status" -TimeoutSec 8
        $sets = @($status.law_sets)
        $mixed = $sets | Where-Object { $_.dataset_id -eq "a-mixed-add" }
        $conv  = $sets | Where-Object { $_.dataset_id -eq "b-convert" }
        if (-not $mixed) { throw "装配投影缺 a-mixed-add（law_sets=$($sets | ConvertTo-Json -Compress))" }
        if (-not $conv)  { throw "装配投影缺 b-convert" }
        if ($mixed.valid -ne $false) { throw "混加律式集应被拒收，实际 valid=$($mixed.valid)" }
        $mixedErrs = ($mixed.errors -join " | ")
        if ($mixedErrs -notmatch "量纲不一致") { throw "拒收报因应含量纲不一致: $mixedErrs" }
        if ($mixedErrs -notmatch "currency")   { throw "拒收报因应含货币向量 currency: $mixedErrs" }
        if ($mixedErrs -notmatch "(^|[^\w])m([^\w]|$)") { throw "拒收报因应含长度向量 m: $mixedErrs" }
        if ($conv.valid -ne $true) { throw "换算律式集应装配通过，实际 valid=$($conv.valid) errors=$($conv.errors -join ' | ')" }

        $result.channel1 = [ordered]@{
            mixed_add_rejected = $true
            mixed_add_error = $mixedErrs
            convert_valid = $true
            convert_digest = $conv.content_digest
        }

        # ---- 通道 2：dimension_check 立法指令链（完整审计链四步）----
        # 步骤① 会话指令发射 → 受理并落 pending IoRequest（调用链发射直证）
        # 步骤② 确定性原生服务直调（/api/services/{name}/invoke，测量面真机直证）
        # 步骤③ 信封经 io_response 回喂（审计链回喂，裁决面入料）
        # 步骤④ 规则层 eq 裁决 → payload.target 落标签（裁决真机直证）
        function Invoke-DimensionCheck($lhs, $rhs, $target) {
            $s = Invoke-RestMethod -Method Post -Uri "http://127.0.0.1:$Port/api/sessions" `
                 -ContentType "application/json" -Body '{}' -TimeoutSec 8
            $sid = $s.session_id
            if (-not $sid) { throw "会话创建响应缺 session_id: $($s | ConvertTo-Json -Compress)" }
            $instr = @{ "instruction" = @{ "type" = "dimension_check"
                        "params" = @{ "lhs" = $lhs; "rhs" = $rhs; "target" = $target } } } |
                      ConvertTo-Json -Depth 6 -Compress
            $resp = Invoke-RestMethod -Method Post `
                     -Uri "http://127.0.0.1:$Port/api/sessions/$sid/command`?wait=true" `
                     -ContentType "application/json" -Body $instr -TimeoutSec 30
            if ($resp.accepted -ne $true) { throw "指令未受理: $($resp | ConvertTo-Json -Compress)" }
            $reqId = $resp.pending_request_id
            if (-not $reqId) { throw "IoRequest 未发射（应含 pending_request_id）" }

            $invoke = Invoke-RestMethod -Method Post `
                      -Uri "http://127.0.0.1:$Port/api/services/dimensional_check/invoke" `
                      -ContentType "application/json" `
                      -Body (@{ "lhs" = $lhs; "rhs" = $rhs } | ConvertTo-Json -Compress) -TimeoutSec 15
            $inner = $invoke.result
            if ($null -eq $inner) { $inner = $invoke }
            $consistent = $inner.consistent
            if ($null -eq $consistent) { throw "服务信封缺 consistent 字段: $($invoke | ConvertTo-Json -Compress)" }
            if ($inner.status -ne "determined") { throw "服务信封 status 应为 determined: $($invoke | ConvertTo-Json -Compress)" }

            $ioBody = @{ "request_id" = $reqId; "result" = $inner; "error" = $null } |
                       ConvertTo-Json -Depth 8 -Compress
            Invoke-RestMethod -Method Post `
                -Uri "http://127.0.0.1:$Port/api/sessions/$sid/io_response" `
                -ContentType "application/json" -Body $ioBody -TimeoutSec 15 | Out-Null

            $state = Invoke-RestMethod -Uri "http://127.0.0.1:$Port/api/sessions/$sid/state" -TimeoutSec 8
            $payload = $state.payload
            if ($null -eq $payload) { $payload = $state.state.payload }
            [ordered]@{ session = $sid; verdict = $payload.$target; consistent = $consistent; payload = $payload }
        }

        # 违例参数：单价（币种/米）与长度（米）——预期 consistent=false → violation
        $r1 = Invoke-DimensionCheck "CNY*m^-1" "m" "verdict"
        if ($r1.consistent -ne $false) { throw "混加参数测量面应判 consistent=false，实际=$($r1.consistent)" }
        if ($r1.verdict -ne "violation") { throw "混加参数应判 violation，实际 verdict=$($r1.verdict) payload=$($r1.payload | ConvertTo-Json -Compress)" }
        # 合法参数：跨币种同货币维——预期 consistent=true → pass
        $r2 = Invoke-DimensionCheck "USD" "yuan" "verdict"
        if ($r2.consistent -ne $true) { throw "同维参数测量面应判 consistent=true，实际=$($r2.consistent)" }
        if ($r2.verdict -ne "pass") { throw "同维参数应判 pass，实际 verdict=$($r2.verdict) payload=$($r2.payload | ConvertTo-Json -Compress)" }

        $result.channel2 = [ordered]@{
            mixed_params_verdict = $r1.verdict
            mixed_params_consistent = $r1.consistent
            same_dim_verdict = $r2.verdict
            same_dim_consistent = $r2.consistent
        }

        # ---- 归一化结果（可哈希：仅保留断言相关字段，剔除时变会话 id） ----
        $canonical = @{
            channel1_mixed_error = [string]$result.channel1.mixed_add_error
            channel1_convert_digest = [string]$result.channel1.convert_digest
            channel2_mixed_verdict = [string]$result.channel2.mixed_params_verdict
            channel2_same_dim_verdict = [string]$result.channel2.same_dim_verdict
        } | ConvertTo-Json -Compress
        $sha = [System.Security.Cryptography.SHA256]::Create()
        $hash = ([System.BitConverter]::ToString($sha.ComputeHash([System.Text.Encoding]::UTF8.GetBytes($canonical)))).Replace("-", "").ToLowerInvariant()
        return @{ result = $result; canonical = $canonical; sha256 = $hash }
    } finally {
        if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
        Start-Sleep -Milliseconds 600
    }
}

# ---- 双跑 ----
Write-Host "=== 测量跑 1 ===" -ForegroundColor Cyan
$run1 = Invoke-Measurement
Write-Host "run1 sha256 = $($run1.sha256)"
Write-Host "=== 测量跑 2 ===" -ForegroundColor Cyan
$run2 = Invoke-Measurement
Write-Host "run2 sha256 = $($run2.sha256)"

if ($run1.sha256 -ne $run2.sha256) {
    $failures++
    Write-Host "[FAIL] 双跑 SHA256 不全等 — run1=$($run1.sha256) run2=$($run2.sha256)" -ForegroundColor Red
    Write-Host "run1 canonical: $($run1.canonical)"
    Write-Host "run2 canonical: $($run2.canonical)"
} else {
    Write-Host "[OK] 双跑 SHA256 字节全等" -ForegroundColor Green
}

if ($failures -eq 0) {
    Write-Host "`n验收通过：律式注册拒收 + 换算律通过 + dimension_check 裁决链 + 双跑 SHA256 全等" -ForegroundColor Green
} else {
    Write-Host "`n验收存在 $failures 项失败" -ForegroundColor Red
}
exit $failures

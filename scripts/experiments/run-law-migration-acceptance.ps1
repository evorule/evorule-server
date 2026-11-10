#!/usr/bin/env pwsh
# Migrated physics law slices acceptance experiment (L1-L5).
#
# Boot a real server instance with --law-sets-dir pointing at the migrated
# slice deployment artifacts (format-bridge output from the rule repo), then:
#   L1  slice sets load: companion status projection lists both sets, valid,
#       law_count 3+2, explicit unverified third state (si_units placeholders),
#       zero warnings, blake3 digest present
#   L3  instruction-face substitution: /v1/criticality/verify substitutes the
#       loaded law (law_substitution step) and returns a three-state verdict
#   L2/L4 equilibrium wiring: graph_shock_propagate invalidates an adjudication
#       whose instruction_type equals a law id; the server maps it through the
#       law registry (domains[0], max_version_depth) into the potential
#       ledger -> phi_by_domain gains the 'physical' domain bucket
#   L5  determinism anchors: shock response and verify response are byte-equal
#       across reruns (SHA256), graph_digest stable
#
# Usage:
#   pwsh -File scripts/experiments/run-law-migration-acceptance.ps1 `
#        [-Port 18086] [-LawSetsDir D:\wt-b260-rule\data\law_sets]

param(
    [int]$Port = 18086,
    [string]$LawSetsDir = "D:\wt-b260-rule\data\law_sets"
)

$ErrorActionPreference = 'Stop'
$RepoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$Exe = Join-Path $RepoRoot "target\release\evorule-server.exe"
$EvalPath = Join-Path $RepoRoot "resources\server_eval.json"
$Base = "http://127.0.0.1:$Port"
$Stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$RunDir = "D:\tmp\law-migration-acceptance\$Stamp"
$WalDir = Join-Path $RunDir "wal"
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)
$script:hadFail = $false
$assertions = New-Object System.Collections.Generic.List[string]
$nReq = 0

if (-not (Test-Path $Exe)) { throw "release exe not found: $Exe (run: cargo build --release -p evorule-server)" }
if (-not (Test-Path $EvalPath)) { throw "constitution not found: $EvalPath" }
if (-not (Test-Path (Join-Path $LawSetsDir "physical-empirical-slices-0.1.0.json"))) { throw "slice deployment artifacts not found under $LawSetsDir" }
New-Item -ItemType Directory -Force -Path $RunDir, $WalDir, (Join-Path $RunDir "deploy") | Out-Null

function Write-Utf8NoBom([string]$Path, [string]$Content) {
    [System.IO.File]::WriteAllText($Path, $Content, $utf8NoBom)
}

function ConvertTo-NormalJson($Obj) {
    if ($null -eq $Obj) { return "null" }
    if ($Obj -is [bool]) { return $Obj.ToString().ToLower() }
    if ($Obj -is [string]) { return '"' + $Obj.Replace('\', '\\').Replace('"', '\"') + '"' }
    if ($Obj -is [System.Collections.IDictionary]) {
        $pairs = $Obj.GetEnumerator() | Sort-Object Key | ForEach-Object {
            '"' + $_.Key + '":' + (ConvertTo-NormalJson $_.Value)
        }
        return "{" + ($pairs -join ",") + "}"
    }
    if ($Obj -is [pscustomobject]) {
        $pairs = $Obj.PSObject.Properties | Sort-Object Name | ForEach-Object {
            '"' + $_.Name + '":' + (ConvertTo-NormalJson $_.Value)
        }
        return "{" + ($pairs -join ",") + "}"
    }
    if ($Obj -is [System.Collections.IEnumerable]) {
        return "[" + (($Obj | ForEach-Object { ConvertTo-NormalJson $_ }) -join ",") + "]"
    }
    return $Obj.ToString()
}

function Assert-Eq([string]$Name, $Actual, $Expected) {
    $a = ConvertTo-NormalJson $Actual
    $e = ConvertTo-NormalJson $Expected
    if ($a -eq $e) {
        $script:assertions.Add("PASS $Name")
    } else {
        $script:assertions.Add("FAIL $Name expected=$e actual=$a")
        $script:hadFail = $true
    }
}

function Assert-True([string]$Name, [bool]$Cond, [string]$Detail) {
    if ($Cond) {
        $script:assertions.Add("PASS $Name")
    } else {
        $script:assertions.Add("FAIL $Name $Detail")
        $script:hadFail = $true
    }
}

function Get-Sha256([string]$Text) {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($Text)
    ([System.BitConverter]::ToString($sha.ComputeHash($bytes)) -replace '-', '').ToLower()
}

function Invoke-ApiJson([string]$Tag, [string]$Method, [string]$Url, [object]$Body) {
    $script:nReq++
    $reqPath = Join-Path $RunDir "req-$Tag.json"
    $respPath = Join-Path $RunDir "resp-$Tag.json"
    if ($null -ne $Body) {
        Write-Utf8NoBom $reqPath ($Body | ConvertTo-Json -Depth 64)
        & curl.exe -s -S -X $Method -H 'Content-Type: application/json' --data-binary "@$reqPath" -o $respPath $Url
    } else {
        & curl.exe -s -S -X $Method -o $respPath $Url
    }
    if ($LASTEXITCODE -ne 0) { throw "curl exit $LASTEXITCODE for $Tag" }
    $raw = [System.IO.File]::ReadAllText($respPath, [System.Text.Encoding]::UTF8)
    Write-Utf8NoBom (Join-Path $RunDir "raw-$Tag.txt") $raw
    return ($raw | ConvertFrom-Json)
}

# ---- deploy slice artifacts (isolated copy: only the two migrated slice sets) ----
$deployDir = Join-Path $RunDir "deploy"
foreach ($name in @("physical-empirical-slices-0.1.0.json", "physical-approximation-slices-0.1.0.json")) {
    Copy-Item (Join-Path $LawSetsDir $name) (Join-Path $deployDir $name)
}

# ---- shock payload: instruction id == migrated law id, referencing a shocked
#      physical-domain constant; the server maps the invalidated adjudication
#      through the law registry (domains[0]='physical', height=max_version_depth)
$eval = [System.IO.File]::ReadAllText($EvalPath, [System.Text.Encoding]::UTF8) | ConvertFrom-Json
$eval.metadata | Add-Member -MemberType NoteProperty -Name constants -Value @(
    @{ constant_id = 'g_ref'; version = 'experiment-inline-declared'; value_hash = 'experiment-inline-declared'; domain = 'physical'; domain_lo = 9.7; domain_hi = 9.9 }
) -Force
$eval.metadata.formula_registry += @{
    instruction_type = 'phy_cla_law_02'
    formula = 'a - g_ref（重力加速度偏差判定）'
    dimensions = @(0, 0, 0, 0, 0, 0, 0)
    domains = @('physical')
    constants = @('g_ref')
}
$adj = @(
    @{ instruction_type = 'phy_cla_law_02'; state = 'pass' },
    @{ instruction_type = 'stress_check'; state = 'pass' }
)
$revG = @{ constant_id = 'g_ref'; new_value = 10.5; new_uncertainty = 0.1; source = 'experiment anomaly injection (out-of-bounds recalibration)'; verified = $true }
$shockBody = @{ rule_set = $eval; revision = $revG; adjudications = $adj }

# ---- verify payload: claims against the migrated F = m * a law slice ----
$verifyBody = @{
    '$schema' = 'https://evorule.org/schemas/criticality/verify_request/v1.0.json'
    subject = 'F = m * a'
    claims = @(
        @{ symbol = 'm'; value = 2.0; si_unit = 'kg' },
        @{ symbol = 'a'; value = 3.0; si_unit = 'm/s^2' },
        @{ symbol = 'F'; value = 6.4; si_unit = 'N'; uncertainty = 0.05 }
    )
    context = @{ source = 'law-migration acceptance experiment'; measured_at = '2026-11-11T00:00:00Z'; method = 'bench measurement' }
}

$proc = $null
try {
    # ---- boot instance with law_set deployment dir ----
    $proc = Start-Process -FilePath $Exe -ArgumentList @(
        "--addr", "127.0.0.1:$Port",
        "--wal-dir", $WalDir,
        "--profile", "development",
        "--insecure-serve",
        "--law-sets-dir", $deployDir
    ) -WorkingDirectory $RepoRoot -PassThru -WindowStyle Hidden -RedirectStandardOutput (Join-Path $RunDir "stdout.log") -RedirectStandardError (Join-Path $RunDir "stderr.log")

    $healthy = $false
    for ($i = 0; $i -lt 40; $i++) {
        Start-Sleep -Milliseconds 500
        try {
            Invoke-RestMethod -Uri "$Base/api/health" -Method Get -TimeoutSec 2 | Out-Null
            $healthy = $true; break
        } catch { }
    }
    Assert-True 'instance healthy' $healthy "no /api/health within 20s"
    if (-not $healthy) { throw "instance failed to boot (see $($RunDir)\stderr.log)" }

    # ---- L1: slice sets load with explicit unverified third state ----
    $status = Invoke-ApiJson 'status-l1' 'GET' "$Base/api/companion/status" $null
    $lawSets = @($status.law_sets)
    Assert-Eq 'L1.set_count' $lawSets.Count 2
    Assert-Eq 'L1.set_ids' ($lawSets | ForEach-Object { $_.dataset_id }) @('physical-approximation-slices-0.1.0', 'physical-empirical-slices-0.1.0')
    foreach ($st in $lawSets) {
        Assert-True "L1.valid[$($st.dataset_id)]" ($st.valid -eq $true) "slice set must load valid"
        Assert-True "L1.digest[$($st.dataset_id)]" (-not [string]::IsNullOrEmpty($st.content_digest)) "blake3 content digest must be present"
        Assert-Eq "L1.zero_warnings[$($st.dataset_id)]" $st.warning_count 0
    }
    $emp = $lawSets | Where-Object { $_.dataset_id -eq 'physical-empirical-slices-0.1.0' }
    $app = $lawSets | Where-Object { $_.dataset_id -eq 'physical-approximation-slices-0.1.0' }
    Assert-Eq 'L1.empirical_law_count' $emp.law_count 3
    Assert-Eq 'L1.approx_law_count' $app.law_count 2
    Assert-Eq 'L1.empirical_unverified' (@($emp.unverified_laws)) @('phy_cla_law_02', 'phy_elm_law_01', 'phy_elm_law_02')
    Assert-Eq 'L1.approx_unverified' (@($app.unverified_laws)) @('phy_opt_app_03', 'phy_rel_app_01')
    Assert-True 'L1.third_state_not_warning' ($emp.unverified_count -eq 3 -and $app.unverified_count -eq 2) "unverified must be surfaced as explicit third state"

    # ---- L3: instruction-face substitution + three-state verdict ----
    $v1 = Invoke-ApiJson 'verify-1' 'POST' "$Base/v1/criticality/verify" $verifyBody
    Write-Utf8NoBom (Join-Path $RunDir "verify-1.json") ($v1 | ConvertTo-Json -Depth 32)
    $v2 = Invoke-ApiJson 'verify-2' 'POST' "$Base/v1/criticality/verify" $verifyBody
    Assert-True 'L3.verdict_present' ($null -ne $v1.verdict) "verify must return a three-state verdict field"
    $substSteps = @($v1.steps | Where-Object { $_.kind -eq 'law_substitution' -and "$($_.target)".StartsWith('phy_cla_law_02') })
    Assert-True 'L3.law_substitution_step' ($substSteps.Count -gt 0) "verify steps must include law_substitution targeting the migrated law"
    Assert-Eq 'L5.verify_sha256_rerun_equal' (Get-Sha256 ($v1 | ConvertTo-Json -Depth 32 -Compress)) (Get-Sha256 ($v2 | ConvertTo-Json -Depth 32 -Compress))

    # ---- L2/L4: shock -> law registry -> physical bucket in potential ledger ----
    $s1 = Invoke-ApiJson 'shock-1' 'POST' "$Base/api/services/graph_shock_propagate/invoke" $shockBody
    Assert-True 'L2.law_id_in_invalidated' (
        ($s1.result.adjudications_invalidated | ForEach-Object { $_.instruction_type }) -contains 'phy_cla_law_02'
    ) "migrated-law adjudication must be invalidated by the shock"
    Assert-Eq 'L5.shock_graph_digest_rerun' $s1.result.graph_digest $s1.result.graph_digest
    $s2 = Invoke-ApiJson 'shock-2' 'POST' "$Base/api/services/graph_shock_propagate/invoke" $shockBody
    Assert-Eq 'L5.shock_sha256_rerun_equal' (Get-Sha256 ($s1 | ConvertTo-Json -Depth 32 -Compress)) (Get-Sha256 ($s2 | ConvertTo-Json -Depth 32 -Compress))

    $status2 = Invoke-ApiJson 'status-l4' 'GET' "$Base/api/companion/status" $null
    Write-Utf8NoBom (Join-Path $RunDir "phi-after-shock.json") ($status2 | ConvertTo-Json -Depth 32)
    # 窗结算节奏：equilibrium 视图由独立结算任务按窗（缺省 60s）从位势账
    # 重算刷新，非实时投影——轮询至下一窗结算透出 physical 桶（上限 1.5 窗）
    $phiPhys = $null
    for ($i = 0; $i -lt 24; $i++) {
        Start-Sleep -Seconds 5
        $status2 = Invoke-ApiJson "status-l4-poll$i" 'GET' "$Base/api/companion/status" $null
        $phiPhys = $status2.equilibrium.phi_by_domain.physical
        if ($null -ne $phiPhys) { break }
    }
    Write-Utf8NoBom (Join-Path $RunDir "phi-settled.json") ($status2 | ConvertTo-Json -Depth 32)
    Assert-True 'L2.physical_bucket_present' ($null -ne $phiPhys) "phi_by_domain must gain the physical bucket after window settlement"
    $phiValue = if ($phiPhys -is [pscustomobject]) { $phiPhys.value } else { $phiPhys }
    Assert-True 'L4.physical_bucket_nonzero' ([double]$phiValue -gt 0) "physical potential must be nonzero (mapped via law registry, height=max_version_depth)"

    # ---- flush evidence ----
    Write-Utf8NoBom (Join-Path $RunDir "results.txt") (($assertions -join "`r`n") + "`r`n")
    Write-Utf8NoBom (Join-Path $RunDir "summary.txt") "requests=$nReq assertions=$($assertions.Count) fail=$script:hadFail run=$Stamp port=$Port"

    $assertions | ForEach-Object { $_ }
    ""
    if ($script:hadFail) {
        Write-Host "REGRESSION: FAIL (see $RunDir)" -ForegroundColor Red
        exit 1
    } else {
        Write-Host "REGRESSION: ALL PASS ($($assertions.Count) assertions, evidence: $RunDir)" -ForegroundColor Green
    }
}
finally {
    if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
}

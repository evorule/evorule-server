# Shock-propagation real-machine regression driver (graph_shock_propagate, three injections).
# ASCII only, PS 5.1 compatible. Approved design decisions: scripts/experiments/ in git, full hard
# assertions, local acceptance surface (not in CI).
#
# Usage: .\run-shock3-regression.ps1 [-Port 18183]
# Prereq: release exe built at target\release\evorule-server.exe (repo root):
#         cargo build --release -p evorule-server
# Asserts: injection 1 zero-radius + unreferenced_constants names k_B
# (G1 closure); injection 2 domain-inside depth1 reference closure no propagation;
# injection 3 domain-outside depth2 storm (3 affected / 3 regime / 3 adjudications
# reverse-order, stress_check survives, dedup domain); determinism anchors (graph_digest
# equal across runs, input_digest all distinct, injection-3 rerun SHA256 byte-equal,
# provenance.deterministic).

param(
    [int]$Port = 18183
)

$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)   # scripts\experiments -> repo root
$Exe      = Join-Path $RepoRoot "target\release\evorule-server.exe"
$EvalPath = Join-Path $RepoRoot "resources\server_eval.json"
$Base     = "http://127.0.0.1:$Port"
$Stamp    = (Get-Date).ToUniversalTime().ToString('yyyyMMdd-HHmmss')
$RunDir   = Join-Path $RepoRoot "data\experiment\run-shock3-$Stamp"
$WalDir   = Join-Path $RunDir "wal"
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)

if (-not (Test-Path $Exe)) { throw "release exe not found: $Exe (run: cargo build --release -p evorule-server)" }
if (-not (Test-Path $EvalPath)) { throw "constitution not found: $EvalPath" }
New-Item -ItemType Directory -Force -Path $RunDir, $WalDir | Out-Null

$assertions = New-Object System.Collections.Generic.List[string]
$samples    = New-Object System.Collections.Generic.List[string]
$nReq = 0

function Write-Utf8NoBom([string]$Path, [string]$Content) {
    [System.IO.File]::WriteAllText($Path, $Content, $utf8NoBom)
}

function ConvertTo-NormalJson($Obj) {
    # canonical key-sorted JSON so object equality is key-order independent
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

# ---- deploy payloads (server_eval v0.12.0 + augmentation, deterministic regeneration) ----
$eval = [System.IO.File]::ReadAllText($EvalPath, [System.Text.Encoding]::UTF8) | ConvertFrom-Json
$consts = @(
    @{ constant_id = 'alpha_Al6061'; version = 'design-baseline-2026'; value_hash = 'experiment-inline-declared'; domain_lo = 0.00002; domain_hi = 0.000026 },
    @{ constant_id = 'k_B'; version = 'codata-2014'; value_hash = 'experiment-inline-declared' },
    @{ constant_id = 'G'; version = 'codata-2018'; value_hash = 'experiment-inline-declared' },
    @{ constant_id = 'h'; version = 'si-2019'; value_hash = 'experiment-inline-declared' }
)
$eval.metadata | Add-Member -MemberType NoteProperty -Name constants -Value $consts -Force
$reg = $eval.metadata.formula_registry | Where-Object { $_.instruction_type -eq 'thermal_growth_check' }
if (-not $reg) { throw "thermal_growth_check not found in formula_registry" }
$reg | Add-Member -MemberType NoteProperty -Name constants -Value @('alpha_Al6061') -Force
$adj = @(
    @{ instruction_type = 'thermal_growth_check'; state = 'pass' },
    @{ instruction_type = 'clearance_check'; state = 'pass' },
    @{ instruction_type = 'tolerance_stack_check'; state = 'pass' },
    @{ instruction_type = 'stress_check'; state = 'pass' }
)
$rev1 = @{ constant_id = 'k_B'; new_value = 1.380649e-23; new_uncertainty = 0.0; source = 'CODATA 2018 / SI-2019 redefinition (exact)'; verified = $true }
$rev2 = @{ constant_id = 'alpha_Al6061'; new_value = 0.0000242; new_uncertainty = 0.0000002; source = 'post-anomaly recalibration (design review)'; verified = $true }
$rev3 = @{ constant_id = 'alpha_Al6061'; new_value = 0.0000285; new_uncertainty = 0.0000003; source = 'post-anomaly recalibration (unvalidated vendor datapoint)'; verified = $true }

$payloads = @(
    @{ tag = '1-kb';       body = @{ rule_set = $eval; revision = $rev1; adjudications = $adj } },
    @{ tag = '2-alpha-in'; body = @{ rule_set = $eval; revision = $rev2; adjudications = $adj } },
    @{ tag = '3-alpha-out'; body = @{ rule_set = $eval; revision = $rev3; adjudications = $adj } }
)

$proc = $null
$script:hadFail = $false
try {
    # ---- boot instance ----
    $proc = Start-Process -FilePath $Exe -ArgumentList @(
        "--addr", "127.0.0.1:$Port",
        "--wal-dir", $WalDir,
        "--profile", "development",
        "--insecure-serve"
    ) -PassThru -WindowStyle Hidden -RedirectStandardOutput (Join-Path $RunDir "stdout.log") -RedirectStandardError (Join-Path $RunDir "stderr.log")

    $healthy = $false
    for ($i = 0; $i -lt 40; $i++) {
        Start-Sleep -Milliseconds 500
        try {
            Invoke-RestMethod -Uri "$Base/api/health" -Method Get -TimeoutSec 2 | Out-Null
            $healthy = $true; break
        } catch { }
    }
    Assert-True 'instance healthy' $healthy "no /api/health within 20s"

    function Invoke-Shock([string]$Tag, [object]$Body) {
        $script:nReq++
        # rule_set carries CJK text; PS 5.1 Invoke-RestMethod tears non-ASCII in the
        # HTTP body (invalid unicode code point at server). File + curl channel is
        # the mandated form for CJK payloads (session-gate hard rule 8).
        $reqPath = Join-Path $RunDir "req-$Tag.json"
        Write-Utf8NoBom $reqPath ($Body | ConvertTo-Json -Depth 64)
        $respPath = Join-Path $RunDir "resp-$Tag.json"
        & curl.exe -s -S -X POST -H 'Content-Type: application/json' --data-binary "@$reqPath" -o $respPath "$Base/api/services/graph_shock_propagate/invoke"
        if ($LASTEXITCODE -ne 0) { throw "curl exit $LASTEXITCODE for $Tag" }
        $raw = [System.IO.File]::ReadAllText($respPath, [System.Text.Encoding]::UTF8)
        return ($raw | ConvertFrom-Json)
    }

    function Get-Sha256([string]$Text) {
        $sha = [System.Security.Cryptography.SHA256]::Create()
        $bytes = [System.Text.Encoding]::UTF8.GetBytes($Text)
        ([System.BitConverter]::ToString($sha.ComputeHash($bytes)) -replace '-', '').ToLower()
    }

    # ---- three injections ----
    $r1 = Invoke-Shock '1-kb'        $payloads[0].body
    $r2 = Invoke-Shock '2-alpha-in'  $payloads[1].body
    $r3 = Invoke-Shock '3-alpha-out' $payloads[2].body
    $r3b = Invoke-Shock '3-alpha-out-rerun' $payloads[2].body

    # ---- injection 1: zero radius + G1 closure (unreferenced named) ----
    Assert-Eq 'inj1.shocked_constant' $r1.result.shocked_constant 'k_B'
    Assert-Eq 'inj1.affected_instructions' $r1.result.affected_instructions @()
    Assert-Eq 'inj1.regime_invalidated' $r1.result.regime_invalidated @()
    Assert-Eq 'inj1.propagation_depth' $r1.result.propagation_depth 0
    Assert-True 'inj1.unreferenced_names_kB' (($r1.result.unreferenced_constants | ForEach-Object { $_ }) -contains 'k_B') "unreferenced_constants=$($r1.result.unreferenced_constants -join ',')"

    # ---- injection 2: domain-inside, reference closure depth1, no propagation ----
    Assert-Eq 'inj2.shocked_constant' $r2.result.shocked_constant 'alpha_Al6061'
    Assert-Eq 'inj2.affected' $r2.result.affected_instructions @(@{ depth = 1; instruction_type = 'thermal_growth_check'; via = 'reference' })
    Assert-Eq 'inj2.regime_invalidated' $r2.result.regime_invalidated @()
    Assert-Eq 'inj2.domains_touched' $r2.result.domains_touched @()
    Assert-Eq 'inj2.propagation_depth' $r2.result.propagation_depth 1

    # ---- injection 3: domain-outside storm ----
    Assert-Eq 'inj3.affected' $r3.result.affected_instructions @(
        @{ depth = 2; instruction_type = 'clearance_check'; via = 'coupling' },
        @{ depth = 1; instruction_type = 'thermal_growth_check'; via = 'reference' },
        @{ depth = 2; instruction_type = 'tolerance_stack_check'; via = 'coupling' }
    )
    Assert-Eq 'inj3.regime_invalidated' $r3.result.regime_invalidated @('clearance_check','thermal_growth_check','tolerance_stack_check')
    Assert-Eq 'inj3.adjudications_top3' ($r3.result.adjudications_invalidated | Select-Object -First 3) @(
        @{ instruction_type = 'thermal_growth_check'; state = 'pass' },
        @{ instruction_type = 'clearance_check'; state = 'pass' },
        @{ instruction_type = 'tolerance_stack_check'; state = 'pass' }
    )
    Assert-True 'inj3.stress_check_survives' (-not (($r3.result.adjudications_invalidated | ForEach-Object { $_.instruction_type }) -contains 'stress_check')) "control adjudication must survive"
    Assert-Eq 'inj3.domains_touched' $r3.result.domains_touched @(,@(1,0,0,0,0,0,0))
    Assert-Eq 'inj3.propagation_depth' $r3.result.propagation_depth 2

    # ---- determinism anchors ----
    Assert-True 'anchor.provenance_deterministic' ($r1.provenance.deterministic -eq $true -and $r2.provenance.deterministic -eq $true -and $r3.provenance.deterministic -eq $true) "provenance.deterministic must be true"
    Assert-Eq 'anchor.graph_digest_equal_3runs' @($r1.result.graph_digest, $r2.result.graph_digest, $r3.result.graph_digest) @($r1.result.graph_digest, $r1.result.graph_digest, $r1.result.graph_digest)
    $digests = @($r1.result.input_digest, $r2.result.input_digest, $r3.result.input_digest)
    Assert-True 'anchor.input_digest_all_distinct' (($digests[0] -ne $digests[1]) -and ($digests[1] -ne $digests[2]) -and ($digests[0] -ne $digests[2])) "tamper must change input digest: $($digests -join ',')"
    $shaA = Get-Sha256 (($r3 | ConvertTo-Json -Depth 16 -Compress))
    $shaB = Get-Sha256 (($r3b | ConvertTo-Json -Depth 16 -Compress))
    Assert-True 'anchor.rerun_sha256_byte_equal' ($shaA -eq $shaB) "injection-3 rerun byte-equal: $shaA vs $shaB"

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

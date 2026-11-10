# Cross-layer real-machine acceptance driver: law_set finiteness/monotone-form
# registration gate + graph_shock_propagate version guard / readjudication
# feedback + equilibrium phi-potential accounting and stall alarm surface.
#
# Approved design decisions: scripts/experiments/ in git, full hard assertions,
# local acceptance surface (not in CI). Contains CJK assertion substrings ->
# file must stay UTF-8 with BOM (PowerShell parser rule for non-ASCII scripts).
#
# Usage: .\run-crosslayer-acceptance.ps1 [-Port 18085]
# Prereq: release exe built at target\release\evorule-server.exe (repo root):
#         cargo build --release -p evorule-server
#
# Chain per group:
#   A registration gate: law_set deployment dir scanned at boot with per-set
#     isolation (surface: /api/companion/status law_sets projection). Negative
#     variants - missing max_version_depth, missing monotone_form,
#     monotone_form=false, unary-minus formula without time_anchor - must be
#     rejected with the mandated reasons; the same formula with time_anchor and
#     a plain monotone law must load valid.
#   B version guard: revision.version equal to the declared constant version is
#     rejected (monotone version-algebra reason); a different value passes,
#     echoes new_version, and always carries the pending_readjudication key.
#   E determinism anchors: provenance.deterministic, graph_digest stability
#     (incl. duplicated cross-edge dedup), input_digest pairwise distinct,
#     rerun response byte-equal (SHA256 over raw response bytes).
#   C phi-potential lifecycle: out-of-bounds revision -> shock pending
#     readjudication mapped through the law registry (domains[0] +
#     max_version_depth) into phi_by_domain / pending_readjudication; verify
#     reports drive readjudication advances (law_review proposal entry ids and
#     law_substitution skip-step targets); phi strictly decreases across window
#     settlements to zero; the domain phase reaches absorbing after the
#     16-entry domain alert tail ages out with a certified streak >= minimum.
#   D stall alarm: a fresh pending that is never readjudicated holds phi equal
#     across >= 2 windows -> domain enters stall_domains, absorbing is vetoed,
#     and the one-shot alarm lands in the global window alert tail.
#
# Wait strategy: window seconds = 1 via the evaluator's documented env knob
# (EVORULE_EQUILIBRIUM_WINDOW_SECS); certified streak minimum = 2 via
# EVORULE_EQUILIBRIUM_CERTIFIED_STREAK_MIN; equilibrium surface polled and
# gated on observed snapshot values (no fixed sleeps for critical
# transitions); the absorbing assertion polls until the domain alert tail
# ages out (16 windows worst case).

param(
    [int]$Port = 18085
)

$ErrorActionPreference = 'Stop'

$RepoRoot  = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)   # scripts\experiments -> repo root
$Exe       = Join-Path $RepoRoot "target\release\evorule-server.exe"
$EvalPath  = Join-Path $RepoRoot "resources\server_eval.json"
$Base      = "http://127.0.0.1:$Port"
$Stamp     = (Get-Date).ToUniversalTime().ToString('yyyyMMdd-HHmmss')
$RunDir    = Join-Path $RepoRoot "data\experiment\run-crosslayer-$Stamp"
$WalDir    = Join-Path $RunDir "wal"
$LawDir    = Join-Path $RunDir "law-sets"
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)
$WindowSecs = 1   # equilibrium window seconds (env knob of the evaluator)
$StreakMin  = 2   # certified streak minimum for the absorbing phase label

if (-not (Test-Path $Exe)) { throw "release exe not found: $Exe (run: cargo build --release -p evorule-server)" }
if (-not (Test-Path $EvalPath)) { throw "constitution not found: $EvalPath" }

# idempotent rerun guard: fail fast when a residual instance already serves the port
$portBusy = $false
try {
    Invoke-RestMethod -Uri "$Base/api/health" -Method Get -TimeoutSec 2 | Out-Null
    $portBusy = $true
} catch { }
if ($portBusy) { throw "port $Port already serves an evorule-server instance; stop the residual process before rerunning" }

New-Item -ItemType Directory -Force -Path $RunDir, $WalDir, $LawDir | Out-Null

# window cadence via the evaluator's documented env knobs (restored in finally)
$prevWindowEnv = $env:EVORULE_EQUILIBRIUM_WINDOW_SECS
$prevStreakEnv = $env:EVORULE_EQUILIBRIUM_CERTIFIED_STREAK_MIN
$env:EVORULE_EQUILIBRIUM_WINDOW_SECS = "$WindowSecs"
$env:EVORULE_EQUILIBRIUM_CERTIFIED_STREAK_MIN = "$StreakMin"

$assertions = New-Object System.Collections.Generic.List[string]
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

# ---- law_set deployment payloads (one file per dataset; per-set isolation) ----
$schemaLawSet = 'https://evorule.org/schemas/law_set/v1.0.json'

function Write-LawSetFile([string]$Name, [object]$Doc) {
    Write-Utf8NoBom (Join-Path $LawDir "$Name.json") ($Doc | ConvertTo-Json -Depth 16)
}

# negative: max_version_depth missing -> schema-layer rejection
Write-LawSetFile 'neg-missing-depth' @{
    '$schema' = $schemaLawSet; kind = 'law_set'; id = 'neg-missing-depth'; version = '1.0.0'
    laws = @(@{
        id = 'missing_depth_law'; formula = 'F = m * a'
        symbols = @{ F = 'net force'; m = 'mass'; a = 'acceleration' }
        si_units = @{ F = 'newton'; m = 'kilogram'; a = 'm*s^-2' }
        monotone_form = $true
    })
}

# negative: monotone_form missing -> schema-layer rejection
Write-LawSetFile 'neg-missing-monotone' @{
    '$schema' = $schemaLawSet; kind = 'law_set'; id = 'neg-missing-monotone'; version = '1.0.0'
    laws = @(@{
        id = 'missing_monotone_law'; formula = 'F = m * a'
        symbols = @{ F = 'net force'; m = 'mass'; a = 'acceleration' }
        si_units = @{ F = 'newton'; m = 'kilogram'; a = 'm*s^-2' }
        max_version_depth = 1
    })
}

# negative: monotone_form=false -> structural rejection (finiteness rewrite required)
Write-LawSetFile 'neg-false-monotone' @{
    '$schema' = $schemaLawSet; kind = 'law_set'; id = 'neg-false-monotone'; version = '1.0.0'
    laws = @(@{
        id = 'nonmonotone_law'; formula = 'F = m * a'
        symbols = @{ F = 'net force'; m = 'mass'; a = 'acceleration' }
        si_units = @{ F = 'newton'; m = 'kilogram'; a = 'm*s^-2' }
        max_version_depth = 1
        monotone_form = $false
    })
}

# negative: unary-minus formula without time_anchor -> negation-construct rejection
Write-LawSetFile 'neg-unary-minus' @{
    '$schema' = $schemaLawSet; kind = 'law_set'; id = 'neg-unary-minus'; version = '1.0.0'
    laws = @(@{
        id = 'unary_temporal_law'; formula = 'v = -a * t'
        symbols = @{ v = 'velocity'; a = 'acceleration'; t = 'elapsed time' }
        si_units = @{ v = 'm*s^-1'; a = 'm*s^-2'; t = 's' }
        max_version_depth = 1
        monotone_form = $true
    })
}

# positive: same formula, time-anchored rewrite declared -> accepted
Write-LawSetFile 'pos-anchored' @{
    '$schema' = $schemaLawSet; kind = 'law_set'; id = 'pos-anchored'; version = '1.0.0'
    laws = @(@{
        id = 'anchored_temporal_law'; formula = 'v = -a * t'
        symbols = @{ v = 'velocity'; a = 'acceleration'; t = 'elapsed time' }
        si_units = @{ v = 'm*s^-1'; a = 'm*s^-2'; t = 's' }
        max_version_depth = 1
        monotone_form = $true
        time_anchor = 'anchored at t0 = 0 (time-anchored rewrite)'
    })
}

# positive: plain monotone law (ideal gas shape) -> accepted
Write-LawSetFile 'pos-monotone' @{
    '$schema' = $schemaLawSet; kind = 'law_set'; id = 'pos-monotone'; version = '1.0.0'
    laws = @(@{
        id = 'monotone_gas_law'; formula = 'P * V = n * R * T'
        symbols = @{ P = 'pressure'; V = 'volume'; n = 'amount of substance'; R = 'ideal gas constant'; T = 'absolute temperature' }
        si_units = @{ P = 'Pa'; V = 'm^3'; n = 'mol'; R = 'J*mol^-1*K^-1'; T = 'K' }
        max_version_depth = 1
        monotone_form = $true
    })
}

# functional law registry for phi accounting: instruction_type -> law id mapping
Write-LawSetFile 'engineering' @{
    '$schema' = $schemaLawSet; kind = 'law_set'; id = 'engineering-laws'; version = '1.0.0'
    laws = @(@{
        id = 'thermal_growth_check'; formula = 'F = m * a'
        symbols = @{ F = 'net force'; m = 'mass'; a = 'acceleration' }
        si_units = @{ F = 'newton'; m = 'kilogram'; a = 'm*s^-2' }
        domains = @('engineering')
        max_version_depth = 2
        monotone_form = $true
    })
}
Write-LawSetFile 'economic' @{
    '$schema' = $schemaLawSet; kind = 'law_set'; id = 'economic-laws'; version = '1.0.0'
    laws = @(
        @{
            id = 'demand_elasticity_check'; formula = 'd = q * p'
            symbols = @{ d = 'demand'; q = 'quantity'; p = 'price' }
            si_units = @{ d = 'm*s'; q = 'm'; p = 's' }
            domains = @('economic')
            max_version_depth = 1
            monotone_form = $true
        },
        @{
            id = 'unit_margin_check'; formula = 'u = 2 * w'
            symbols = @{ u = 'margin'; w = 'unit cost' }
            si_units = @{ u = 'second'; w = 'second' }
            domains = @('economic')
            max_version_depth = 1
            monotone_form = $true
        }
    )
}

# ---- deploy payloads (constitution + registry augmentation, deterministic regeneration) ----
$eval = [System.IO.File]::ReadAllText($EvalPath, [System.Text.Encoding]::UTF8) | ConvertFrom-Json
$consts = @(
    @{ constant_id = 'alpha_Al6061'; version = 'design-baseline-2026'; value_hash = 'experiment-inline-declared'; domain = 'engineering'; domain_lo = 0.00002; domain_hi = 0.000026 },
    @{ constant_id = 'k_B'; version = 'codata-2014'; value_hash = 'experiment-inline-declared'; domain = 'physical'; domain_lo = 1e-23; domain_hi = 1.3e-23 },
    @{ constant_id = 'G'; version = 'codata-2018'; value_hash = 'experiment-inline-declared'; domain = 'physical' },
    @{ constant_id = 'h'; version = 'si-2019'; value_hash = 'experiment-inline-declared'; domain = 'physical' }
)
$eval.metadata | Add-Member -MemberType NoteProperty -Name constants -Value $consts -Force

$reg = $eval.metadata.formula_registry | Where-Object { $_.instruction_type -eq 'thermal_growth_check' }
if (-not $reg) { throw "thermal_growth_check not found in formula_registry" }
$reg | Add-Member -MemberType NoteProperty -Name constants -Value @('alpha_Al6061', 'k_B') -Force
$reg | Add-Member -MemberType NoteProperty -Name domains -Value @('engineering') -Force
foreach ($t in @('clearance_check', 'tolerance_stack_check')) {
    $r = $eval.metadata.formula_registry | Where-Object { $_.instruction_type -eq $t }
    $r | Add-Member -MemberType NoteProperty -Name domains -Value @('engineering') -Force
}
$eval.metadata.formula_registry += @{
    instruction_type = 'demand_elasticity_check'
    formula = '|dp*q - p0*q0| (demand elasticity deviation)'
    dimensions = @(0, 0, 0, 0, 0, 0, 0)
    domains = @('economic')
}
$eval.metadata | Add-Member -MemberType NoteProperty -Name cross_domain_edges -Value @(
    @{ from = 'thermal_growth_check'; to = 'demand_elasticity_check' }
) -Force

# adjudication lists: E/B shocks leave the law registry untouched (no phi impact);
# C shock maps exactly the thermal law; D shock maps the demand law on top
$adjNoLaw    = @(
    @{ instruction_type = 'clearance_check'; state = 'pass' },
    @{ instruction_type = 'tolerance_stack_check'; state = 'pass' },
    @{ instruction_type = 'stress_check'; state = 'pass' }
)
$adjThermal  = @(@{ instruction_type = 'thermal_growth_check'; state = 'pass' })
$adjAllFour  = @(
    @{ instruction_type = 'thermal_growth_check'; state = 'pass' },
    @{ instruction_type = 'clearance_check'; state = 'pass' },
    @{ instruction_type = 'tolerance_stack_check'; state = 'pass' },
    @{ instruction_type = 'demand_elasticity_check'; state = 'pass' }
)

$revKbSame      = @{ constant_id = 'k_B'; new_value = 1.380649e-23; new_uncertainty = 0.0; source = 'CODATA 2018 / SI-2019 redefinition (exact)'; verified = $true; version = 'codata-2014' }
$revKbNext      = @{ constant_id = 'k_B'; new_value = 1.380649e-23; new_uncertainty = 0.0; source = 'CODATA 2018 / SI-2019 redefinition (exact)'; verified = $true; version = 'codata-2019' }
$revKb          = @{ constant_id = 'k_B'; new_value = 1.380649e-23; new_uncertainty = 0.0; source = 'CODATA 2018 / SI-2019 redefinition (exact)'; verified = $true }
$revAlphaIn     = @{ constant_id = 'alpha_Al6061'; new_value = 0.0000242; new_uncertainty = 0.0000002; source = 'post-anomaly recalibration (design review)'; verified = $true }
$revAlphaOut    = @{ constant_id = 'alpha_Al6061'; new_value = 0.0000285; new_uncertainty = 0.0000003; source = 'post-anomaly recalibration (unvalidated vendor datapoint)'; verified = $true }

# duplicate cross edge variant (dedup determinism anchor)
$evalDup = ($eval | ConvertTo-Json -Depth 64) | ConvertFrom-Json
$evalDup.metadata | Add-Member -MemberType NoteProperty -Name cross_domain_edges -Value @(
    @{ from = 'thermal_growth_check'; to = 'demand_elasticity_check' },
    @{ from = 'thermal_growth_check'; to = 'demand_elasticity_check' }
) -Force

$ctx = @{ source = 'crosslayer-acceptance-driver'; measured_at = '2026-01-01T00:00:00Z' }
$vThermalViolate = @{
    '$schema' = 'https://evorule.org/schemas/criticality/verify_request/v1.0.json'
    subject = 'acceptance-thermal-claim'
    claims = @(
        @{ symbol = 'F'; value = 10; si_unit = 'newton'; uncertainty = 0.1 },
        @{ symbol = 'm'; value = 2; si_unit = 'kilogram' },
        @{ symbol = 'a'; value = 4; si_unit = 'm*s^-2' }
    )
    context = $ctx
}
$vThermalSkip = @{
    '$schema' = 'https://evorule.org/schemas/criticality/verify_request/v1.0.json'
    subject = 'acceptance-thermal-claim'
    claims = @(
        @{ symbol = 'F'; value = 10; si_unit = 'newton' },
        @{ symbol = 'm'; value = 2; si_unit = 'kilogram' },
        @{ symbol = 'a'; value = 4; si_unit = 'm*s^-2' }
    )
    context = $ctx
}
$vEconomicViolate = @{
    '$schema' = 'https://evorule.org/schemas/criticality/verify_request/v1.0.json'
    subject = 'acceptance-economic-claim'
    claims = @(
        @{ symbol = 'u'; value = 100; si_unit = 'second'; uncertainty = 0.1 },
        @{ symbol = 'w'; value = 1; si_unit = 'second' }
    )
    context = $ctx
}

$proc = $null
$script:hadFail = $false
try {
    # ---- boot instance (CWD = repo root so the constitution path resolves) ----
    $proc = Start-Process -FilePath $Exe -ArgumentList @(
        "--addr", "127.0.0.1:$Port",
        "--wal-dir", $WalDir,
        "--law-sets-dir", $LawDir,
        "--profile", "development",
        "--insecure-serve"
    ) -PassThru -WindowStyle Hidden -WorkingDirectory $RepoRoot -RedirectStandardOutput (Join-Path $RunDir "stdout.log") -RedirectStandardError (Join-Path $RunDir "stderr.log")

    $healthy = $false
    for ($i = 0; $i -lt 40; $i++) {
        Start-Sleep -Milliseconds 500
        try {
            Invoke-RestMethod -Uri "$Base/api/health" -Method Get -TimeoutSec 2 | Out-Null
            $healthy = $true; break
        } catch { }
    }
    Assert-True 'H1.instance_healthy' $healthy "no /api/health within 20s"

    function Invoke-Api([string]$Route, [string]$Tag, [object]$Body) {
        $script:nReq++
        # file + curl channel: CJK-safe request bodies and UTF-8-exact response bytes
        $reqPath = Join-Path $RunDir "req-$Tag.json"
        Write-Utf8NoBom $reqPath ($Body | ConvertTo-Json -Depth 64)
        $respPath = Join-Path $RunDir "resp-$Tag.json"
        $prev = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        try {
            $code = & curl.exe -s -S -X POST -H 'Content-Type: application/json' --data-binary "@$reqPath" -o $respPath -w '%{http_code}' "$Base$route"
            $exit = $LASTEXITCODE
        } finally {
            $ErrorActionPreference = $prev
        }
        if ($exit -ne 0) { throw "curl exit $exit for $Tag" }
        $raw = [System.IO.File]::ReadAllText($respPath, [System.Text.Encoding]::UTF8)
        return @{ status = [int]$code; raw = $raw; body = ($raw | ConvertFrom-Json) }
    }

    function Get-Status {
        $script:nReq++
        $respPath = Join-Path $RunDir "status-latest.json"
        $prev = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        try {
            & curl.exe -s -S -o $respPath "$Base/api/companion/status" | Out-Null
            $exit = $LASTEXITCODE
        } finally {
            $ErrorActionPreference = $prev
        }
        if ($exit -ne 0) { throw "curl exit $exit reading companion status" }
        $raw = [System.IO.File]::ReadAllText($respPath, [System.Text.Encoding]::UTF8)
        return ($raw | ConvertFrom-Json)
    }

    function Get-Sha256Of([string]$Path) {
        $sha = [System.Security.Cryptography.SHA256]::Create()
        ([System.BitConverter]::ToString($sha.ComputeHash([System.IO.File]::ReadAllBytes($Path))) -replace '-', '').ToLower()
    }

    function LawSetStatusOf($Status, [string]$DatasetId) {
        $Status.law_sets | Where-Object { $_.dataset_id -eq $DatasetId }
    }

    function PhiOf($Status, [string]$Domain) {
        $phi = $Status.equilibrium.phi_by_domain
        if ($null -eq $phi) { return $null }
        $prop = $phi.PSObject.Properties[$Domain]
        if ($null -eq $prop) { return $null }
        return $prop.Value
    }

    function PhaseOf($Status, [string]$Domain) {
        $phases = $Status.equilibrium.domain_phases
        if ($null -eq $phases) { return $null }
        $prop = $phases.PSObject.Properties[$Domain]
        if ($null -eq $prop) { return $null }
        return $prop.Value
    }

    function Wait-EqView([scriptblock]$Probe, [int]$TimeoutSecs) {
        # poll the companion status surface until the probe passes; returns
        # @{ hit; snapshot } with the last snapshot for assert diagnostics
        $deadline = (Get-Date).AddSeconds($TimeoutSecs)
        $last = $null
        $hit = $false
        while ($true) {
            $last = Get-Status
            if (& $Probe $last) { $hit = $true; break }
            if ((Get-Date) -gt $deadline) { break }
            Start-Sleep -Milliseconds 300
        }
        return @{ hit = $hit; snapshot = $last }
    }

    function EqDetail($Snapshot) {
        ($Snapshot.equilibrium | ConvertTo-Json -Depth 6 -Compress)
    }

    # ---- group A: law_set registration gate (deployment-surface projection) ----
    $statusA = Get-Status

    $negA = LawSetStatusOf $statusA 'neg-missing-depth'
    Assert-True 'A1a.missing_depth_rejected' ($null -ne $negA -and -not $negA.valid) "set must be projected with valid=false"
    $reasonA = if ($negA) { ($negA.errors -join ' | ') } else { '' }
    Assert-True 'A1b.reason_mentions_field' ($reasonA.Contains('max_version_depth')) "rejection reason must name the missing field: $reasonA"
    Assert-True 'A1c.reason_mentions_laws' ($reasonA.Contains('laws')) "rejection reason must point at the laws collection: $reasonA"

    $negB = LawSetStatusOf $statusA 'neg-missing-monotone'
    Assert-True 'A2a.missing_monotone_rejected' ($null -ne $negB -and -not $negB.valid) "set must be projected with valid=false"
    $reasonB = if ($negB) { ($negB.errors -join ' | ') } else { '' }
    Assert-True 'A2b.reason_mentions_field' ($reasonB.Contains('monotone_form')) "rejection reason must name the missing field: $reasonB"

    $negC = LawSetStatusOf $statusA 'neg-false-monotone'
    Assert-True 'A3a.false_monotone_rejected' ($null -ne $negC -and -not $negC.valid) "set must be projected with valid=false"
    $reasonC = if ($negC) { ($negC.errors -join ' | ') } else { '' }
    Assert-True 'A3b.reason_nonmonotone' ($reasonC.Contains('非单调形态律式拒收')) "rejection reason must carry the non-monotone mandate: $reasonC"
    Assert-True 'A3c.reason_mentions_anchor' ($reasonC.Contains('time_anchor')) "rejection reason must point at the anchor escape hatch: $reasonC"

    $negD = LawSetStatusOf $statusA 'neg-unary-minus'
    Assert-True 'A4a.unary_minus_rejected' ($null -ne $negD -and -not $negD.valid) "set must be projected with valid=false"
    $reasonD = if ($negD) { ($negD.errors -join ' | ') } else { '' }
    Assert-True 'A4b.reason_negation_construct' ($reasonD.Contains('否定类构造')) "rejection reason must name the negation construct: $reasonD"
    Assert-True 'A4c.reason_kind_unary_minus' ($reasonD.Contains('unary_minus')) "rejection reason must classify the construct kind: $reasonD"
    Assert-True 'A4d.reason_mentions_anchor' ($reasonD.Contains('time_anchor')) "rejection reason must point at the anchor escape hatch: $reasonD"
    Assert-True 'A4e.reason_snippet' ($reasonD.Contains('-a')) "rejection reason must quote the offending snippet: $reasonD"

    foreach ($pair in @(
        @{ id = 'pos-anchored'; count = 1 },
        @{ id = 'pos-monotone'; count = 1 },
        @{ id = 'engineering'; count = 1 },
        @{ id = 'economic'; count = 2 }
    )) {
        $pos = LawSetStatusOf $statusA $pair.id
        Assert-True "A.pos_$($pair.id)_valid" ($null -ne $pos -and $pos.valid) "accepted set must be projected with valid=true"
        Assert-Eq "A.pos_$($pair.id)_law_count" ($pos.law_count) ($pair.count)
        Assert-True "A.pos_$($pair.id)_digest" ($null -ne $pos.content_digest -and $pos.content_digest.Length -gt 0) "accepted set must carry a content digest"
    }

    # ---- group B: revision version guard (monotone version algebra) ----
    $bSame = Invoke-Api '/api/services/graph_shock_propagate/invoke' 'b-same-version' @{ rule_set = $eval; revision = $revKbSame; adjudications = $adjNoLaw }
    Assert-True 'B1.same_version_rejected' ($bSame.status -eq 400) "same-version revision must be rejected with 400, got $($bSame.status)"
    Assert-True 'B2.reason_monotone_version' ($bSame.raw.Contains('版本代数须单调推进')) "rejection reason must carry the monotone version-algebra mandate"
    Assert-True 'B3.reason_echoes_proposed' ($bSame.raw.Contains('codata-2014')) "rejection reason must echo the proposed version"

    $bNext = Invoke-Api '/api/services/graph_shock_propagate/invoke' 'b-new-version' @{ rule_set = $eval; revision = $revKbNext; adjudications = $adjNoLaw }
    Assert-True 'B4.different_version_accepted' ($bNext.status -eq 200) "distinct version must pass, got $($bNext.status)"
    Assert-Eq 'B5.new_version_echoed' $bNext.body.result.new_version 'codata-2019'
    Assert-Eq 'B6.pending_entries' $bNext.body.result.pending_readjudication @(
        @{ instruction_type = 'clearance_check'; depth = 2; via = 'coupling' },
        @{ instruction_type = 'tolerance_stack_check'; depth = 2; via = 'coupling' }
    )
    Assert-True 'B7.invalidated_sets_nonempty' (($bNext.body.result.regime_invalidated.Count -ge 3) -and ($bNext.body.result.adjudications_invalidated.Count -ge 2)) "invalidated regime/adjudication sets must be non-empty"
    Assert-True 'B8.provenance_deterministic' ($bNext.body.provenance.deterministic -eq $true) "provenance must flag deterministic"

    # ---- group E: determinism anchors (before the phi lifecycle keeps ledger clean) ----
    $eKb       = Invoke-Api '/api/services/graph_shock_propagate/invoke' 'e-kb'          @{ rule_set = $eval; revision = $revKb;       adjudications = $adjNoLaw }
    $eKbRerun  = Invoke-Api '/api/services/graph_shock_propagate/invoke' 'e-kb-rerun'    @{ rule_set = $eval; revision = $revKb;       adjudications = $adjNoLaw }
    $eAlphaIn  = Invoke-Api '/api/services/graph_shock_propagate/invoke' 'e-alpha-in'    @{ rule_set = $eval; revision = $revAlphaIn;  adjudications = $adjNoLaw }
    $eAlphaOut = Invoke-Api '/api/services/graph_shock_propagate/invoke' 'e-alpha-out'   @{ rule_set = $eval; revision = $revAlphaOut; adjudications = $adjNoLaw }
    $eDupEdge  = Invoke-Api '/api/services/graph_shock_propagate/invoke' 'e-kb-dup-edge' @{ rule_set = $evalDup; revision = $revKb;    adjudications = $adjNoLaw }

    foreach ($r in @($eKb, $eKbRerun, $eAlphaIn, $eAlphaOut, $eDupEdge)) {
        Assert-True 'E1.provenance_deterministic' ($r.body.provenance.deterministic -eq $true) "every shock response must flag deterministic"
    }

    $gdB      = $bNext.body.result.graph_digest
    $gdKb     = $eKb.body.result.graph_digest
    $gdIn     = $eAlphaIn.body.result.graph_digest
    $gdOut    = $eAlphaOut.body.result.graph_digest
    $gdDup    = $eDupEdge.body.result.graph_digest
    Assert-True 'E2.graph_digest_stable' (($gdB -eq $gdKb) -and ($gdKb -eq $gdIn) -and ($gdIn -eq $gdOut) -and ($gdOut -eq $gdDup)) "graph digest must stay equal across bodies and duplicated edges: $gdB|$gdKb|$gdIn|$gdOut|$gdDup"

    $idKb  = $eKb.body.result.input_digest
    $idIn  = $eAlphaIn.body.result.input_digest
    $idOut = $eAlphaOut.body.result.input_digest
    Assert-True 'E3.input_digest_pairwise_distinct' (($idKb -ne $idIn) -and ($idIn -ne $idOut) -and ($idKb -ne $idOut)) "distinct revisions must change input digest: $idKb|$idIn|$idOut"

    Assert-True 'E4.rerun_sha256_byte_equal' ((Get-Sha256Of (Join-Path $RunDir 'resp-e-kb.json')) -eq (Get-Sha256Of (Join-Path $RunDir 'resp-e-kb-rerun.json'))) "rerun response bytes must be identical"

    Assert-Eq 'E5.alpha_in_short_circuit' $eAlphaIn.body.result.affected_instructions @(
        @{ depth = 1; instruction_type = 'thermal_growth_check'; via = 'reference' }
    )
    Assert-Eq 'E5.alpha_in_invalidated_empty' ($eAlphaIn.body.result.regime_invalidated) @()
    Assert-Eq 'E5.alpha_in_pending_empty' ($eAlphaIn.body.result.pending_readjudication) @()
    Assert-True 'E5.alpha_in_depth' ($eAlphaIn.body.result.propagation_depth -eq 1) "in-bounds revision must short-circuit at depth 1"

    Assert-Eq 'E6.alpha_out_affected' $eAlphaOut.body.result.affected_instructions @(
        @{ depth = 2; instruction_type = 'clearance_check'; via = 'coupling' },
        @{ depth = 2; instruction_type = 'demand_elasticity_check'; via = 'cross_domain' },
        @{ depth = 1; instruction_type = 'thermal_growth_check'; via = 'reference' },
        @{ depth = 2; instruction_type = 'tolerance_stack_check'; via = 'coupling' }
    )
    Assert-Eq 'E6.alpha_out_knowledge_domains' $eAlphaOut.body.result.knowledge_domains_touched @('economic', 'engineering')
    Assert-True 'E6.alpha_out_cross_depth' ($eAlphaOut.body.result.cross_domain_depth -eq 2) "cross-domain edge must be traversed at depth 2"

    # ---- group C: phi-potential lifecycle (shock -> verify-driven drain -> absorbing) ----
    $cShock = Invoke-Api '/api/services/graph_shock_propagate/invoke' 'c-shock' @{ rule_set = $eval; revision = $revAlphaOut; adjudications = $adjThermal }
    Assert-True 'C1.shock_accepted' ($cShock.status -eq 200) "shock must pass, got $($cShock.status)"
    Assert-Eq 'C1.pending_exact' $cShock.body.result.pending_readjudication @(
        @{ instruction_type = 'thermal_growth_check'; depth = 1; via = 'reference' }
    )

    $snapC2 = Wait-EqView { param($st) (PhiOf $st 'engineering') -eq 2 } 15
    Assert-True 'C2.phi_engineering_two' ($snapC2.hit) "phi_by_domain.engineering never reached 2; last equilibrium=$(EqDetail $snapC2.snapshot)"
    Assert-Eq 'C3.pending_total_two' $snapC2.snapshot.equilibrium.pending_readjudication 2

    $v1 = Invoke-Api '/v1/criticality/verify' 'c-verify-violate' $vThermalViolate
    Assert-True 'C4.verify_contradiction' (($v1.status -eq 200) -and ($v1.body.verdict -eq 'contradiction')) "violating claim must produce contradiction, got $($v1.status)/$($v1.body.verdict)"
    $v1review = $v1.body.proposals | Where-Object { $_.kind -eq 'law_review' -and $_.entry_id -eq 'thermal_growth_check' }
    Assert-True 'C5.law_review_proposal' (($null -ne $v1review) -and ($v1review[0].dataset_id -eq 'engineering')) "violated law must emit a law_review proposal for the registry law"

    $snapC6 = Wait-EqView { param($st) (PhiOf $st 'engineering') -eq 1 } 15
    Assert-True 'C6.phi_strict_decrease' ($snapC6.hit) "phi must strictly decrease 2 -> 1 across window settlements; last equilibrium=$(EqDetail $snapC6.snapshot)"

    $v2 = Invoke-Api '/v1/criticality/verify' 'c-verify-skip' $vThermalSkip
    Assert-True 'C7.skip_verdict_stable' (($v2.status -eq 200) -and ($v2.body.verdict -eq 'stable')) "uncertainty-less claim must verify stable, got $($v2.status)/$($v2.body.verdict)"
    $v2skip = $v2.body.steps | Where-Object { $_.kind -eq 'law_substitution' -and $_.target -eq 'thermal_growth_check' }
    Assert-True 'C7.skip_step_target' (($null -ne $v2skip) -and ($v2skip[0].outcome -eq 'skipped')) "uncertainty-less substitution must emit a skipped law_substitution step targeting the law"

    $snapC8 = Wait-EqView { param($st) ($null -eq (PhiOf $st 'engineering')) -and (($st.equilibrium.pending_readjudication -eq 0) -or ($null -eq $st.equilibrium.pending_readjudication)) } 15
    Assert-True 'C8.phi_drained' ($snapC8.hit) "phi must drain to zero (absent) with pending total 0; last equilibrium=$(EqDetail $snapC8.snapshot)"

    $v3 = Invoke-Api '/v1/criticality/verify' 'c-verify-streak' $vThermalSkip
    Assert-True 'C9.streak_verdict_stable' (($v3.status -eq 200) -and ($v3.body.verdict -eq 'stable')) "streak-building verify must stay stable"

    $snapC10 = Wait-EqView { param($st) (PhaseOf $st 'engineering') -eq 'absorbing' } 60
    Assert-True 'C10.domain_absorbing' ($snapC10.hit) "engineering domain must reach absorbing after the alert tail ages out; last equilibrium=$(EqDetail $snapC10.snapshot)"
    Assert-Eq 'C11.no_stall_at_rest' $snapC10.snapshot.equilibrium.stall_domains @()

    # ---- group D: stall alarm (pending held, never readjudicated) ----
    $dShock = Invoke-Api '/api/services/graph_shock_propagate/invoke' 'd-shock' @{ rule_set = $eval; revision = $revAlphaOut; adjudications = $adjAllFour }
    Assert-True 'D1.shock_accepted' ($dShock.status -eq 200) "shock must pass, got $($dShock.status)"
    Assert-Eq 'D1.pending_exact' $dShock.body.result.pending_readjudication @(
        @{ instruction_type = 'clearance_check'; depth = 2; via = 'coupling' },
        @{ instruction_type = 'demand_elasticity_check'; depth = 2; via = 'cross_domain' },
        @{ instruction_type = 'thermal_growth_check'; depth = 1; via = 'reference' },
        @{ instruction_type = 'tolerance_stack_check'; depth = 2; via = 'coupling' }
    )

    $vd0 = Invoke-Api '/v1/criticality/verify' 'd-verify-alert-seed' $vEconomicViolate
    Assert-True 'D2.economic_contradiction' (($vd0.status -eq 200) -and ($vd0.body.verdict -eq 'contradiction')) "economic violating claim must produce contradiction, got $($vd0.status)/$($vd0.body.verdict)"
    $vd0review = $vd0.body.proposals | Where-Object { $_.kind -eq 'law_review' -and $_.dataset_id -eq 'economic' }
    Assert-True 'D2.economic_alert_attribution' (($null -ne $vd0review) -and ($vd0review[0].entry_id -eq 'unit_margin_check')) "alert must attribute to the economic dataset via the margin law proposal"

    $snapD3 = Wait-EqView { param($st) (($st.equilibrium.stall_domains) -contains 'economic') } 15
    Assert-True 'D3.stall_domain_reported' ($snapD3.hit) "economic must enter stall_domains after 2 held windows; last equilibrium=$(EqDetail $snapD3.snapshot)"
    $dPhase = PhaseOf $snapD3.snapshot 'economic'
    Assert-True 'D4.phase_not_absorbing' (($null -ne $dPhase) -and ($dPhase -ne 'absorbing')) "stalled domain must not be absorbing, got phase=$dPhase"
    Assert-Eq 'D5.pending_total_one' $snapD3.snapshot.equilibrium.pending_readjudication 1
    Assert-Eq 'D5.phi_economic_one' (PhiOf $snapD3.snapshot 'economic') 1

    $snapD6 = Wait-EqView { param($st) (($st.equilibrium.window_alert_tail | Where-Object { $_ -gt 0 } | Measure-Object).Count -gt 0) } 10
    Assert-True 'D6.stall_alarm_in_tail' ($snapD6.hit) "stall alarm must land in the window alert tail; last equilibrium=$(EqDetail $snapD6.snapshot)"

    # ---- group F: determinism manifest (byte-level comparison across full runs) ----
    $manifest = [ordered]@{}
    Get-ChildItem $RunDir -Filter 'resp-*.json' | Sort-Object Name | ForEach-Object {
        $tag = $_.Name.Substring(5, $_.Name.Length - 10)
        $manifest[$tag] = Get-Sha256Of $_.FullName
    }
    Write-Utf8NoBom (Join-Path $RunDir "determinism-manifest.json") (($manifest | ConvertTo-Json -Depth 4) + "`r`n")
    Assert-True 'F1.determinism_manifest_written' ($manifest.Count -ge 9) "manifest must cover all deterministic invoke/verify responses"

    # ---- flush evidence ----
    Write-Utf8NoBom (Join-Path $RunDir "results.txt") (($assertions -join "`r`n") + "`r`n")
    Write-Utf8NoBom (Join-Path $RunDir "summary.txt") "requests=$nReq assertions=$($assertions.Count) fail=$script:hadFail run=$Stamp port=$Port window_secs=$WindowSecs streak_min=$StreakMin"

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
    if ($null -ne $prevWindowEnv) { $env:EVORULE_EQUILIBRIUM_WINDOW_SECS = $prevWindowEnv } else { Remove-Item Env:EVORULE_EQUILIBRIUM_WINDOW_SECS -ErrorAction SilentlyContinue }
    if ($null -ne $prevStreakEnv) { $env:EVORULE_EQUILIBRIUM_CERTIFIED_STREAK_MIN = $prevStreakEnv } else { Remove-Item Env:EVORULE_EQUILIBRIUM_CERTIFIED_STREAK_MIN -ErrorAction SilentlyContinue }
}

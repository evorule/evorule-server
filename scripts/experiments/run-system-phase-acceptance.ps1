# System-phase real-machine acceptance driver: one cross-layer perturbation
# (data-layer measured-value update) whose per-layer phase machines
# (capability / rules / data) must each traverse crisis -> relaxing ->
# absorbing in strict order without backtracking, with staggered onsets,
# while the global composite surface converges: phi_total drains to zero,
# system_phase reaches absorbing, system_balanced turns true.
#
# Observation surface: the companion status endpoint's equilibrium block
# carries four system-phase keys (layer_phases map over capability/rules/data,
# system_phase, system_balanced, phi_total) added alongside the existing
# domain/phase/phi projection. Composite semantics asserted at every sampled
# step: any layer crisis => system crisis; all layers absorbing with
# phi_total == 0 => absorbing; otherwise relaxing.
#
# Chain per round:
#   1 baseline: surface keys present, values in the mandated label domain,
#     composite consistent, phi_total zero.
#   2 determinism anchors: shock provenance.deterministic, graph_digest
#     stability across bodies, input_digest pairwise distinct across distinct
#     revisions, rerun response byte-equality (SHA256 over raw bytes).
#   3 perturbation: out-of-bounds measured-value revision maps exactly one
#     law into the pending ledger (phi 2) -> data layer crisis first.
#   4 readjudication: contradicting verify (numeric sigma) emits a law_review
#     proposal -> readjudication advances (phi 1) and the law-domain alert
#     lands -> rules layer crisis.
#   5 unattributed alert window: a held-potential stall alarm (global-only
#     attribution) plus a cross-dataset contradicting verify (proposals from
#     two datasets => unattributed alert bucket) drive the capability-layer
#     crisis last; the cross verify also completes the drain (phi 0).
#   6 convergence: alert tails age out (relaxing while the certified streak
#     is below minimum), two stable verifies build the streak, every layer
#     reaches absorbing, system_phase absorbing, system_balanced true.
#   7 negative path: a verify request with a malformed sigma param (object
#     shape) must be rejected with 422 and its error response must carry no
#     system-phase surface keys (instruction-level assertion scope: the
#     observation surface itself has no per-scenario view, so the rejected
#     instruction response is the assertable carrier), and the status surface
#     must remain intact and unperturbed afterwards.
#
# Wait strategy: window seconds = 1 via the evaluator's documented env knob
# (EVORULE_EQUILIBRIUM_WINDOW_SECS); certified streak minimum = 2 via
# EVORULE_EQUILIBRIUM_CERTIFIED_STREAK_MIN; every stimulus is gated on an
# observed snapshot value (no fixed sleeps for critical transitions); the
# convergence gate polls until the alert tails age out.
#
# Dual-run regime: the full experiment executes twice with fresh instances;
# each round writes a normalized deterministic observation file (RLE step
# list of the tracked tuple + per-layer distinct sequences + first
# crisis/absorbing step indices + phi sequence; no timestamps, no wall-clock
# data). The two files must be SHA256 byte-equal.
#
# Stagger fallback: absorbing-step strict stagger (pairwise distinct) is the
# default assertion; pass -RelaxAbsorbingStagger only when the real machine
# proves strict staggering impossible (shared streak gate) to switch that
# assertion to the recording form (each layer's absorbing step recorded and
# the stagger matrix reported). Crisis-onset stagger stays strict either way.
#
# Usage: .\run-system-phase-acceptance.ps1 [-Port 18086] [-RelaxAbsorbingStagger]
# Prereq: release exe built at target\release\evorule-server.exe (repo root)
#         with the system-phase observation keys deployed:
#         cargo build --release -p evorule-server

param(
    [int]$Port = 18086,
    [switch]$RelaxAbsorbingStagger
)

$ErrorActionPreference = 'Stop'

$RepoRoot  = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)   # scripts\experiments -> repo root
$Exe       = Join-Path $RepoRoot "target\release\evorule-server.exe"
$EvalPath  = Join-Path $RepoRoot "resources\server_eval.json"
$Base      = "http://127.0.0.1:$Port"
$Stamp     = (Get-Date).ToUniversalTime().ToString('yyyyMMdd-HHmmss')
$RunRoot   = Join-Path $RepoRoot "data\experiment\run-system-phase-$Stamp"
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)
$WindowSecs = 1   # equilibrium window seconds (env knob of the evaluator)
$StreakMin  = 2   # certified streak minimum for the absorbing phase label
$PollMs     = 250 # observation polling cadence (well under one window)

if (-not (Test-Path $Exe)) { throw "release exe not found: $Exe (run: cargo build --release -p evorule-server)" }
if (-not (Test-Path $EvalPath)) { throw "constitution not found: $EvalPath" }

# idempotent rerun guard: fail fast when a residual instance already serves the port
$portBusy = $false
try {
    Invoke-RestMethod -Uri "$Base/api/health" -Method Get -TimeoutSec 2 | Out-Null
    $portBusy = $true
} catch { }
if ($portBusy) { throw "port $Port already serves an evorule-server instance; stop the residual process before rerunning" }

New-Item -ItemType Directory -Force -Path $RunRoot | Out-Null

# window cadence via the evaluator's documented env knobs (restored in finally)
$prevWindowEnv = $env:EVORULE_EQUILIBRIUM_WINDOW_SECS
$prevStreakEnv = $env:EVORULE_EQUILIBRIUM_CERTIFIED_STREAK_MIN
$env:EVORULE_EQUILIBRIUM_WINDOW_SECS = "$WindowSecs"
$env:EVORULE_EQUILIBRIUM_CERTIFIED_STREAK_MIN = "$StreakMin"

$assertions = New-Object System.Collections.Generic.List[string]
$script:hadFail = $false
$script:tracker = $null

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

function Get-Sha256Of([string]$Path) {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    ([System.BitConverter]::ToString($sha.ComputeHash([System.IO.File]::ReadAllBytes($Path))) -replace '-', '').ToLower()
}

function Get-Rle([object[]]$Values) {
    $out = @()
    foreach ($v in $Values) {
        if (($out.Count -eq 0) -or ($out[$out.Count - 1] -ne $v)) { $out += $v }
    }
    return $out
}

function Get-CompositeExpectation($Step) {
    # mandated composite semantics: any layer crisis => crisis; all three
    # layers absorbing with phi_total == 0 => absorbing; otherwise relaxing
    $layers = @($Step.capability, $Step.rules, $Step.data)
    $anyCrisis = $false
    $allAbsorbing = $true
    foreach ($l in $layers) {
        if ($l -eq 'crisis') { $anyCrisis = $true }
        if ($l -ne 'absorbing') { $allAbsorbing = $false }
    }
    if ($anyCrisis) { return 'crisis' }
    if ($allAbsorbing -and ($Step.phi_total -eq 0)) { return 'absorbing' }
    return 'relaxing'
}

function First-StepIndex([string]$Layer, [string]$Value) {
    $steps = $script:tracker.Steps
    for ($i = 0; $i -lt $steps.Count; $i++) {
        if ($steps[$i].$Layer -eq $Value) { return $i }
    }
    return $null
}

function Test-LayerSequenceStrict([string]$Layer) {
    # strict traversal from the first crisis step: a contiguous crisis run,
    # then a contiguous relaxing run (must exist), then absorbing from the
    # first absorbing step on with no backtracking; steps before the first
    # crisis (pre-perturbation baseline) are not constrained
    $steps = $script:tracker.Steps
    $c = First-StepIndex $Layer 'crisis'
    if ($null -eq $c) { return @{ ok = $false; why = "no crisis step observed (steps=$($steps.Count))" } }
    $a = $null
    for ($i = $c; $i -lt $steps.Count; $i++) {
        if ($steps[$i].$Layer -eq 'absorbing') { $a = $i; break }
    }
    if ($null -eq $a) { return @{ ok = $false; why = "no absorbing step after crisis at step $c" } }
    $sawRelaxing = $false
    for ($i = $c; $i -lt $a; $i++) {
        $v = $steps[$i].$Layer
        if ($v -eq 'crisis') {
            if ($sawRelaxing) { return @{ ok = $false; why = "crisis backtrack at step $i after relaxing" } }
        } elseif ($v -eq 'relaxing') {
            $sawRelaxing = $true
        } else {
            return @{ ok = $false; why = "unexpected value '$v' at step $i between crisis and absorbing" }
        }
    }
    if (-not $sawRelaxing) { return @{ ok = $false; why = "no relaxing step between crisis (step $c) and absorbing (step $a)" } }
    for ($i = $a; $i -lt $steps.Count; $i++) {
        if ($steps[$i].$Layer -ne 'absorbing') {
            return @{ ok = $false; why = "backtrack to '$($steps[$i].$Layer)' at step $i after absorbing (step $a)" }
        }
    }
    return @{ ok = $true; why = "crisis@$c relaxing-absorbing@$a strict" }
}

# ---- request payloads (constant across rounds; per-round request files) ----
$ctx = @{ source = 'system-phase-acceptance-driver'; measured_at = '2026-01-01T00:00:00Z' }
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
$vCrossViolate = @{
    '$schema' = 'https://evorule.org/schemas/criticality/verify_request/v1.0.json'
    subject = 'acceptance-crosslayer-claim'
    claims = @(
        @{ symbol = 'F'; value = 10; si_unit = 'newton'; uncertainty = 0.1 },
        @{ symbol = 'm'; value = 2; si_unit = 'kilogram' },
        @{ symbol = 'a'; value = 4; si_unit = 'm*s^-2' },
        @{ symbol = 'u'; value = 100; si_unit = 'second'; uncertainty = 0.1 },
        @{ symbol = 'w'; value = 1; si_unit = 'second' }
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
$vBadUncertainty = @{
    '$schema' = 'https://evorule.org/schemas/criticality/verify_request/v1.0.json'
    subject = 'acceptance-malformed-claim'
    claims = @(
        @{ symbol = 'F'; value = 10; si_unit = 'newton'; uncertainty = @{ sigma = 0.1 } },
        @{ symbol = 'm'; value = 2; si_unit = 'kilogram' },
        @{ symbol = 'a'; value = 4; si_unit = 'm*s^-2' }
    )
    context = $ctx
}

function Invoke-Round([int]$RoundNo, [string]$Tag) {
    $RunDir  = Join-Path $RunRoot "r$RoundNo"
    $WalDir  = Join-Path $RunDir "wal"
    $LawDir  = Join-Path $RunDir "law-sets"
    New-Item -ItemType Directory -Force -Path $RunDir, $WalDir, $LawDir | Out-Null

    # ---- law_set deployment payloads (functional registry, per-round isolation) ----
    $schemaLawSet = 'https://evorule.org/schemas/law_set/v1.0.json'
    Write-Utf8NoBom (Join-Path $LawDir 'engineering.json') (@{
        '$schema' = $schemaLawSet; kind = 'law_set'; id = 'engineering-laws'; version = '1.0.0'
        laws = @(@{
            id = 'thermal_growth_check'; formula = 'F = m * a'
            symbols = @{ F = 'net force'; m = 'mass'; a = 'acceleration' }
            si_units = @{ F = 'newton'; m = 'kilogram'; a = 'm*s^-2' }
            domains = @('engineering')
            max_version_depth = 2
            monotone_form = $true
        })
    } | ConvertTo-Json -Depth 16)
    Write-Utf8NoBom (Join-Path $LawDir 'economic.json') (@{
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
    } | ConvertTo-Json -Depth 16)

    # ---- deploy payloads (constitution + registry augmentation, fresh per round) ----
    $eval = [System.IO.File]::ReadAllText($EvalPath, [System.Text.Encoding]::UTF8) | ConvertFrom-Json
    $consts = @(
        @{ constant_id = 'alpha_Al6061'; version = 'design-baseline-2026'; value_hash = 'experiment-inline-declared'; domain = 'engineering'; domain_lo = 0.00002; domain_hi = 0.000026 }
    )
    $eval.metadata | Add-Member -MemberType NoteProperty -Name constants -Value $consts -Force
    $reg = $eval.metadata.formula_registry | Where-Object { $_.instruction_type -eq 'thermal_growth_check' }
    if (-not $reg) { throw "thermal_growth_check not found in formula_registry" }
    $reg | Add-Member -MemberType NoteProperty -Name constants -Value @('alpha_Al6061') -Force
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

    $adjThermal  = @(@{ instruction_type = 'thermal_growth_check'; state = 'pass' })
    $revAlphaOut = @{ constant_id = 'alpha_Al6061'; new_value = 0.0000285; new_uncertainty = 0.0000003; source = 'post-anomaly recalibration (unvalidated vendor datapoint)'; verified = $true }
    $revAlphaIn  = @{ constant_id = 'alpha_Al6061'; new_value = 0.0000242; new_uncertainty = 0.0000002; source = 'post-anomaly recalibration (design review)'; verified = $true }

    $proc = $null
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
        Assert-True "$Tag.H1.instance_healthy" $healthy "no /api/health within 20s"

        function Invoke-Api([string]$Route, [string]$ReqTag, [object]$Body) {
            # file + curl channel: exact UTF-8 request bodies and response bytes
            $reqPath = Join-Path $RunDir "req-$ReqTag.json"
            Write-Utf8NoBom $reqPath ($Body | ConvertTo-Json -Depth 64)
            $respPath = Join-Path $RunDir "resp-$ReqTag.json"
            $prev = $ErrorActionPreference
            $ErrorActionPreference = 'Continue'
            try {
                $code = & curl.exe -s -S -X POST -H 'Content-Type: application/json' --data-binary "@$reqPath" -o $respPath -w '%{http_code}' "$Base$route"
                $exit = $LASTEXITCODE
            } finally {
                $ErrorActionPreference = $prev
            }
            if ($exit -ne 0) { throw "curl exit $exit for $ReqTag" }
            $raw = [System.IO.File]::ReadAllText($respPath, [System.Text.Encoding]::UTF8)
            return @{ status = [int]$code; raw = $raw; body = ($raw | ConvertFrom-Json) }
        }

        function Get-Status {
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
            # session-level attribution can surface an empty-name domain key
            # in domain_phases/phi_by_domain; only hashtable parsing accepts it
            return ($raw | ConvertFrom-Json -AsHashTable)
        }

        function Step-Track($Status) {
            # run-length encode the tracked system-phase tuple: a new step is
            # recorded only when any tracked value changes between polls;
            # warm-up samples with any tracked key absent (pre-first-settlement
            # default view) are skipped so the step list is boot-race free
            $eq = $Status.equilibrium
            if ($null -eq $eq) { return }
            $lp = $eq.layer_phases
            if ($null -eq $lp) { return }
            $tuple = [ordered]@{
                capability      = $lp.capability
                rules           = $lp.rules
                data            = $lp.data
                system_phase    = $eq.system_phase
                system_balanced = $eq.system_balanced
                phi_total       = $eq.phi_total
            }
            foreach ($v in $tuple.Values) {
                if ($null -eq $v) { return }
            }
            $norm = ConvertTo-NormalJson $tuple
            if (($null -eq $script:tracker.Last) -or ($script:tracker.Last -ne $norm)) {
                $script:tracker.Last = $norm
                $script:tracker.Steps.Add($tuple)
            }
        }

        function Wait-PhaseStep([scriptblock]$Probe, [int]$TimeoutSecs) {
            # poll the status surface, tracking every observed state, until the
            # probe passes; returns @{ hit; snapshot } for assert diagnostics
            $deadline = (Get-Date).AddSeconds($TimeoutSecs)
            $last = $null
            $hit = $false
            while ($true) {
                $last = Get-Status
                Step-Track $last
                if (& $Probe $last) { $hit = $true; break }
                if ((Get-Date) -gt $deadline) { break }
                Start-Sleep -Milliseconds $PollMs
            }
            return @{ hit = $hit; snapshot = $last }
        }

        function LayerPhaseOf($Status, [string]$Layer) {
            $eq = $Status.equilibrium
            if ($null -eq $eq) { return $null }
            $lp = $eq.layer_phases
            if ($null -eq $lp) { return $null }
            if ($lp -is [System.Collections.IDictionary]) {
                if (-not $lp.Contains($Layer)) { return $null }
                return $lp[$Layer]
            }
            $prop = $lp.PSObject.Properties[$Layer]
            if ($null -eq $prop) { return $null }
            return $prop.Value
        }

        function EqDetail($Snapshot) {
            ($Snapshot.equilibrium | ConvertTo-Json -Depth 6 -Compress)
        }

        # ---- reset the per-round tracker ----
        $script:tracker = @{
            Steps = New-Object System.Collections.Generic.List[object]
            Last  = $null
        }

        # ---- baseline: surface present, valid, consistent, phi drained ----
        # gate on the first settled window (alert_free_windows advanced) so
        # both rounds start tracking from the same post-settlement state
        $baseGate = Wait-PhaseStep { param($st) (
                ($null -ne $st.equilibrium.layer_phases) -and
                ($null -ne $st.equilibrium.phi_total) -and
                ($st.equilibrium.alert_free_windows -ge 1)
            ) } 15
        $baseSt = $baseGate.snapshot
        $lpKeys = @()
        if ($null -ne $baseSt.equilibrium.layer_phases) {
            $lpObj = $baseSt.equilibrium.layer_phases
            if ($lpObj -is [System.Collections.IDictionary]) {
                $lpKeys = @($lpObj.Keys | Sort-Object)
            } else {
                $lpKeys = @($lpObj.PSObject.Properties.Name | Sort-Object)
            }
        }
        Assert-Eq "$Tag.P1.layer_phases_keys_exact" $lpKeys @('capability', 'data', 'rules')
        $lpVals = @()
        foreach ($k in @('capability', 'rules', 'data')) {
            $lpVals += (LayerPhaseOf $baseSt $k)
        }
        $lpValid = $true
        foreach ($v in $lpVals) {
            if (($null -eq $v) -or ($v -notin @('crisis', 'relaxing', 'absorbing'))) { $lpValid = $false }
        }
        Assert-True "$Tag.P2.layer_phase_values_valid" $lpValid "layer_phases values must be crisis/relaxing/absorbing, got $($lpVals -join ',')"
        $spValid = ($null -ne $baseSt.equilibrium.system_phase) -and ($baseSt.equilibrium.system_phase -in @('crisis', 'relaxing', 'absorbing'))
        Assert-True "$Tag.P3.system_phase_value_valid" $spValid "system_phase must be a phase label, got $($baseSt.equilibrium.system_phase)"
        Assert-True "$Tag.P4.system_balanced_is_bool" ($baseSt.equilibrium.system_balanced -is [bool]) "system_balanced must be a bool, got $($baseSt.equilibrium.system_balanced)"
        Assert-Eq "$Tag.P5.baseline_phi_total_zero" $baseSt.equilibrium.phi_total 0
        $baseTuple = [ordered]@{
            capability = (LayerPhaseOf $baseSt 'capability'); rules = (LayerPhaseOf $baseSt 'rules'); data = (LayerPhaseOf $baseSt 'data')
            system_phase = $baseSt.equilibrium.system_phase; system_balanced = $baseSt.equilibrium.system_balanced; phi_total = $baseSt.equilibrium.phi_total
        }
        Assert-Eq "$Tag.P6.baseline_composite_consistent" (Get-CompositeExpectation $baseTuple) $baseSt.equilibrium.system_phase

        # ---- determinism anchors (before the perturbation keeps ledger clean) ----
        $xOut   = Invoke-Api '/api/services/graph_shock_propagate/invoke' "$Tag-x-out"      @{ rule_set = $eval; revision = $revAlphaOut; adjudications = $adjThermal }
        $xOutRe = Invoke-Api '/api/services/graph_shock_propagate/invoke' "$Tag-x-out-rerun" @{ rule_set = $eval; revision = $revAlphaOut; adjudications = $adjThermal }
        $xIn    = Invoke-Api '/api/services/graph_shock_propagate/invoke' "$Tag-x-in"       @{ rule_set = $eval; revision = $revAlphaIn;  adjudications = $adjThermal }
        $xInRe  = Invoke-Api '/api/services/graph_shock_propagate/invoke' "$Tag-x-in-rerun"  @{ rule_set = $eval; revision = $revAlphaIn;  adjudications = $adjThermal }

        Assert-True "$Tag.D1.shock_provenance_deterministic" ((($xOut.body.provenance.deterministic -eq $true) -and ($xIn.body.provenance.deterministic -eq $true))) "every shock response must flag deterministic"
        Assert-True "$Tag.D2.graph_digest_stable" ($xOut.body.result.graph_digest -eq $xIn.body.result.graph_digest) "graph digest must stay equal across revisions: $($xOut.body.result.graph_digest)|$($xIn.body.result.graph_digest)"
        Assert-True "$Tag.D3.input_digest_pairwise_distinct" ($xOut.body.result.input_digest -ne $xIn.body.result.input_digest) "distinct revisions must change input digest"
        Assert-True "$Tag.D4.shock_rerun_bytes_equal" ((Get-Sha256Of (Join-Path $RunDir "resp-$Tag-x-out.json")) -eq (Get-Sha256Of (Join-Path $RunDir "resp-$Tag-x-out-rerun.json"))) "identical shock rerun response bytes must match"
        Assert-True "$Tag.D5.probe_rerun_bytes_equal" ((Get-Sha256Of (Join-Path $RunDir "resp-$Tag-x-in.json")) -eq (Get-Sha256Of (Join-Path $RunDir "resp-$Tag-x-in-rerun.json"))) "identical probe rerun response bytes must match"

        # ---- perturbation: out-of-bounds measured-value revision -> phi 2 ----
        Assert-True "$Tag.X1.shock_accepted" ($xOut.status -eq 200) "perturbing shock must pass, got $($xOut.status)"
        Assert-Eq "$Tag.X2.shock_pending_exact" $xOut.body.result.pending_readjudication @(
            @{ instruction_type = 'thermal_growth_check'; depth = 1; via = 'reference' }
        )

        # gate A: first post-shock settlement (phi 2 published) - paces the
        # readjudicating verify strictly between the first and second
        # settlements so the stall episode starts only after it
        $gateA = Wait-PhaseStep { param($st) $st.equilibrium.phi_total -eq 2 } 15
        Assert-True "$Tag.L1.data_crisis_observed" ((First-StepIndex 'data' 'crisis') -ne $null) "data layer must observe crisis after the perturbation; last equilibrium=$(EqDetail $gateA.snapshot)"

        # ---- readjudication: contradicting verify -> rules layer crisis ----
        $v1 = Invoke-Api '/v1/criticality/verify' "$Tag-verify-thermal" $vThermalViolate
        Assert-True "$Tag.X3.verify_contradiction" (($v1.status -eq 200) -and ($v1.body.verdict -eq 'contradiction')) "violating claim must produce contradiction, got $($v1.status)/$($v1.body.verdict)"

        $gateB = Wait-PhaseStep { param($st) (LayerPhaseOf $st 'rules') -eq 'crisis' } 15
        Assert-True "$Tag.L3.rules_crisis_observed" ($gateB.hit -and ((First-StepIndex 'rules' 'crisis') -ne $null)) "rules layer must observe crisis with the readjudication; last equilibrium=$(EqDetail $gateB.snapshot)"

        # ---- unattributed alert window: held potential stalls (global-only
        # alarm) or the cross-dataset contradiction's unattributed bucket
        # drives the capability layer crisis last ----
        $holdC = Wait-PhaseStep { param($st) (LayerPhaseOf $st 'capability') -eq 'crisis' } 8
        # cross-dataset contradicting verify: proposals from two datasets =>
        # unattributed alert bucket; its thermal law_review completes the
        # drain (phi 1 -> 0)
        $vM = Invoke-Api '/v1/criticality/verify' "$Tag-verify-cross" $vCrossViolate
        Assert-True "$Tag.X4.cross_verify_contradiction" (($vM.status -eq 200) -and ($vM.body.verdict -eq 'contradiction')) "cross-dataset violating claim must produce contradiction, got $($vM.status)/$($vM.body.verdict)"

        $gateM = Wait-PhaseStep { param($st) $st.equilibrium.phi_total -eq 0 } 10
        Assert-True "$Tag.L5.capability_crisis_observed" ((First-StepIndex 'capability' 'crisis') -ne $null) "capability layer must observe crisis in the alert-window stage; last equilibrium=$(EqDetail $gateM.snapshot)"

        # ---- convergence: alert tails age out, streak builds, all absorb ----
        $gateD = Wait-PhaseStep { param($st) (@($st.equilibrium.window_alert_tail | Where-Object { $_ -gt 0 }).Count -eq 0) } 35
        Assert-True "$Tag.G0.alert_tails_aged_out" ($gateD.hit) "global alert tail must drain before the streak-building verifies; last equilibrium=$(EqDetail $gateD.snapshot)"

        $null = Invoke-Api '/v1/criticality/verify' "$Tag-verify-skip-1" $vThermalSkip
        $null = Invoke-Api '/v1/criticality/verify' "$Tag-verify-skip-2" $vThermalSkip

        $gateF = Wait-PhaseStep { param($st) (
                ($st.equilibrium.system_phase -eq 'absorbing') -and
                ($st.equilibrium.system_balanced -eq $true) -and
                ($st.equilibrium.phi_total -eq 0) -and
                ((LayerPhaseOf $st 'capability') -eq 'absorbing') -and
                ((LayerPhaseOf $st 'rules') -eq 'absorbing') -and
                ((LayerPhaseOf $st 'data') -eq 'absorbing')
            ) } 60
        Assert-True "$Tag.G1.system_converged" ($gateF.hit) "system surface must converge to absorbing/balanced/phi-0 with all layers absorbing; last equilibrium=$(EqDetail $gateF.snapshot)"

        # ---- per-layer strict sequences ----
        $steps = $script:tracker.Steps
        $seqData = Test-LayerSequenceStrict 'data'
        Assert-True "$Tag.L2.data_phase_sequence_strict" $seqData.ok "data layer must traverse crisis->relaxing->absorbing strictly: $($seqData.why)"
        $seqRules = Test-LayerSequenceStrict 'rules'
        Assert-True "$Tag.L4.rules_phase_sequence_strict" $seqRules.ok "rules layer must traverse crisis->relaxing->absorbing strictly: $($seqRules.why)"
        $seqCap = Test-LayerSequenceStrict 'capability'
        Assert-True "$Tag.L6.capability_phase_sequence_strict" $seqCap.ok "capability layer must traverse crisis->relaxing->absorbing strictly: $($seqCap.why)"

        # ---- stagger matrix (absorbing index scoped at/after first crisis,
        # same scope as the per-layer sequence test) ----
        $fc = [ordered]@{ capability = (First-StepIndex 'capability' 'crisis'); rules = (First-StepIndex 'rules' 'crisis'); data = (First-StepIndex 'data' 'crisis') }
        $fa = [ordered]@{}
        foreach ($l in @('capability', 'rules', 'data')) {
            $a = $null
            if ($null -ne $fc[$l]) {
                for ($i = $fc[$l]; $i -lt $steps.Count; $i++) {
                    if ($steps[$i].$l -eq 'absorbing') { $a = $i; break }
                }
            }
            $fa[$l] = $a
        }
        Assert-True "$Tag.S1.crisis_onset_stagger_strict" (($null -ne $fc.data) -and ($null -ne $fc.rules) -and ($null -ne $fc.capability) -and ($fc.data -lt $fc.rules) -and ($fc.rules -lt $fc.capability)) "crisis onsets must stagger data < rules < capability (step indices): data=$($fc.data) rules=$($fc.rules) capability=$($fc.capability)"
        Assert-True "$Tag.S2.absorbing_step_order" (($null -ne $fa.data) -and ($null -ne $fa.rules) -and ($null -ne $fa.capability) -and ($fa.capability -ge $fa.rules) -and ($fa.rules -ge $fa.data)) "absorbing steps must order capability >= rules >= data: data=$($fa.data) rules=$($fa.rules) capability=$($fa.capability)"
        if ($RelaxAbsorbingStagger) {
            Assert-True "$Tag.S3.absorbing_step_stagger_recorded" (($null -ne $fa.data) -and ($null -ne $fa.rules) -and ($null -ne $fa.capability)) "relaxed mode: absorbing steps recorded, stagger matrix data=$($fa.data) rules=$($fa.rules) capability=$($fa.capability)"
        } else {
            Assert-True "$Tag.S3.absorbing_step_stagger_strict" (($fa.data -ne $fa.rules) -and ($fa.rules -ne $fa.capability)) "absorbing steps must be pairwise distinct: data=$($fa.data) rules=$($fa.rules) capability=$($fa.capability) (use -RelaxAbsorbingStagger only if the real machine proves strict staggering impossible)"
        }

        # ---- global composite accounting ----
        $phiVals = @($steps | ForEach-Object { $_.phi_total })
        $onset = -1
        for ($i = 0; $i -lt $phiVals.Count; $i++) {
            if ($phiVals[$i] -is [int] -or $phiVals[$i] -is [long] -or $phiVals[$i] -is [int64]) {
                if ($phiVals[$i] -gt 0) { $onset = $i; break }
            }
        }
        $phiMono = ($onset -ge 0)
        $phiOffender = ''
        if ($phiMono) {
            for ($i = $onset; $i -lt $phiVals.Count; $i++) {
                if (-not ($phiVals[$i] -is [int] -or $phiVals[$i] -is [long] -or $phiVals[$i] -is [int64])) { $phiMono = $false; $phiOffender = "step $i non-integer phi=$($phiVals[$i])"; break }
                if (($i -gt $onset) -and ($phiVals[$i] -gt $phiVals[$i - 1])) { $phiMono = $false; $phiOffender = "step $i phi=$($phiVals[$i]) > step $($i-1) phi=$($phiVals[$i-1])"; break }
            }
        } else {
            $phiOffender = "phi_total never positive (values=$($phiVals -join ','))"
        }
        Assert-True "$Tag.G2.phi_total_nonincreasing_after_onset" $phiMono "phi_total must be non-increasing from its first positive step: $phiOffender"
        Assert-Eq "$Tag.G3.phi_total_final_zero" $gateF.snapshot.equilibrium.phi_total 0
        Assert-Eq "$Tag.G4.system_phase_final_absorbing" $gateF.snapshot.equilibrium.system_phase 'absorbing'
        Assert-Eq "$Tag.G5.system_balanced_final_true" $gateF.snapshot.equilibrium.system_balanced $true
        Assert-Eq "$Tag.G6.layers_all_absorbing_final" @((LayerPhaseOf $gateF.snapshot 'capability'), (LayerPhaseOf $gateF.snapshot 'rules'), (LayerPhaseOf $gateF.snapshot 'data')) @('absorbing', 'absorbing', 'absorbing')

        $compBad = 0
        $compFirst = ''
        for ($i = 0; $i -lt $steps.Count; $i++) {
            $expect = Get-CompositeExpectation $steps[$i]
            if ($steps[$i].system_phase -ne $expect) {
                if ($compBad -eq 0) { $compFirst = "step $i expected=$expect actual=$($steps[$i].system_phase) tuple=$(ConvertTo-NormalJson $steps[$i])" }
                $compBad++
            }
        }
        Assert-True "$Tag.G7.composite_consistent_every_step" ($compBad -eq 0) "composite semantics must hold at every sampled step ($($steps.Count) steps): first violation $compFirst"

        # ---- negative path: malformed sigma param -> 422, no phase surface ----
        $vBad = Invoke-Api '/v1/criticality/verify' "$Tag-verify-bad-param" $vBadUncertainty
        Assert-Eq "$Tag.N1.malformed_param_rejected_422" $vBad.status 422
        Assert-True "$Tag.N2.error_response_without_system_phase_keys" (($vBad.raw -notmatch 'system_phase') -and ($vBad.raw -notmatch 'layer_phases') -and ($vBad.raw -notmatch 'system_balanced') -and ($vBad.raw -notmatch 'phi_total')) "rejected instruction response must carry no system-phase surface keys (instruction-level scope: the observation surface has no per-scenario view; the error response is the assertable carrier)"
        $postNeg = Wait-PhaseStep { param($st) ($null -ne $st.equilibrium.layer_phases) -and ($null -ne $st.equilibrium.system_phase) } 10
        Assert-True "$Tag.N3.surface_keys_present_after_negative" ($postNeg.hit) "status surface must keep the system-phase keys after the rejected instruction"
        Assert-Eq "$Tag.N4.no_spurious_perturbation_after_negative" @((LayerPhaseOf $postNeg.snapshot 'capability'), (LayerPhaseOf $postNeg.snapshot 'rules'), (LayerPhaseOf $postNeg.snapshot 'data'), $postNeg.snapshot.equilibrium.phi_total) @('absorbing', 'absorbing', 'absorbing', 0)

        # ---- normalized deterministic observations file ----
        $layerSeqs = [ordered]@{
            capability = @((Get-Rle @($steps | ForEach-Object { $_.capability })))
            rules      = @((Get-Rle @($steps | ForEach-Object { $_.rules })))
            data       = @((Get-Rle @($steps | ForEach-Object { $_.data })))
        }
        $phiSeq = @((Get-Rle $phiVals))
        $obs = [ordered]@{
            format               = 'system-phase-acceptance-observations-v1'
            window_secs          = $WindowSecs
            certified_streak_min = $StreakMin
            step_count           = $steps.Count
            steps                = $steps
            layer_sequences      = $layerSeqs
            first_crisis_step    = $fc
            first_absorbing_step = $fa
            phi_sequence         = $phiSeq
        }
        $obsPath = Join-Path $RunDir "system-phase-observations.json"
        Write-Utf8NoBom $obsPath (($obs | ConvertTo-Json -Depth 8) + "`r`n")
        return @{ ObsPath = $obsPath; Steps = $steps.Count }
    }
    finally {
        if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
        # belt-and-braces: free the port before the next round boots
        Start-Sleep -Milliseconds 500
    }
}

try {
    $r1 = Invoke-Round 1 'r1'
    $r2 = Invoke-Round 2 'r2'

    Assert-True 'F1.round1_observations_written' (($r1.Steps -ge 5) -and (Test-Path $r1.ObsPath)) "round 1 must record a non-trivial normalized observation file (steps=$($r1.Steps))"
    Assert-True 'F2.round2_observations_written' (($r2.Steps -ge 5) -and (Test-Path $r2.ObsPath)) "round 2 must record a non-trivial normalized observation file (steps=$($r2.Steps))"
    Assert-True 'F3.dual_run_observations_sha256_equal' ((Get-Sha256Of $r1.ObsPath) -eq (Get-Sha256Of $r2.ObsPath)) "normalized observation files across the two full rounds must be SHA256 byte-equal"

    # ---- flush evidence ----
    Write-Utf8NoBom (Join-Path $RunRoot "results.txt") (($assertions -join "`r`n") + "`r`n")
    Write-Utf8NoBom (Join-Path $RunRoot "summary.txt") "assertions=$($assertions.Count) fail=$script:hadFail run=$Stamp port=$Port window_secs=$WindowSecs streak_min=$StreakMin relax_absorbing_stagger=$($RelaxAbsorbingStagger.IsPresent)"

    $assertions | ForEach-Object { $_ }
    ""
    if ($script:hadFail) {
        Write-Host "REGRESSION: FAIL (see $RunRoot)" -ForegroundColor Red
        exit 1
    } else {
        Write-Host "REGRESSION: ALL PASS ($($assertions.Count) assertions, evidence: $RunRoot)" -ForegroundColor Green
    }
}
finally {
    if ($null -ne $prevWindowEnv) { $env:EVORULE_EQUILIBRIUM_WINDOW_SECS = $prevWindowEnv } else { Remove-Item Env:EVORULE_EQUILIBRIUM_WINDOW_SECS -ErrorAction SilentlyContinue }
    if ($null -ne $prevStreakEnv) { $env:EVORULE_EQUILIBRIUM_CERTIFIED_STREAK_MIN = $prevStreakEnv } else { Remove-Item Env:EVORULE_EQUILIBRIUM_CERTIFIED_STREAK_MIN -ErrorAction SilentlyContinue }
}

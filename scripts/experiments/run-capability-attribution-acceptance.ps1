# Capability-layer attribution acceptance (alert-source split, dual-run).
#
# Proves on a live instance that the alert-source attribution split holds:
# the capability layer is driven by its own unattributed account only
# (record_alert free-function path + empty-domain path), while domain-
# attributed alerts feed the rules layer exclusively. The status surface
# carries the alert_attribution block {capability, domains, total} whose
# invariant capability + sum(domains) == total must hold at every settled
# window, and the domains map never contains the empty-name pseudo-domain
# key (the pre-split global attribution bucket is gone).
#
# Chain per round:
#   1 baseline: equilibrium surface present with the attribution block
#     (keys exact, all-zero), invariant holds, no pseudo-domain key.
#   2 determinism anchors: one out-of-bounds unvalidated revision (opens
#     the readjudication ledger, deduped across identical reruns) plus one
#     in-bounds verified revision (distinct anchor probe): shock
#     provenance.deterministic, graph_digest stability across revisions,
#     input_digest pairwise distinct across distinct revisions, rerun
#     response byte-equality (SHA256 over raw bytes).
#   3 domain-attributed alert: single-law contradicting verify (numeric
#     sigma) -> law-domain alert lands in the engineering domain account:
#     rules layer crisis while the capability layer stays non-crisis and
#     that window's attribution shows domains.engineering >= 1 with
#     capability == 0. The readjudication ledger is already open (phi 2
#     from the anchors' out-of-bounds shock); the verify's law_review
#     proposal completes one chain (phi 1) -> data layer crisis.
#   4 unattributed alert: cross-dataset contradicting verify fired right
#     after the rules-crisis observation (two law domains => no single
#     domain attribution) -> the capability account only: capability layer
#     crisis with that window's attribution showing capability >= 1 and an
#     empty domains map; the cross verify's thermal law_review also
#     completes the drain (phi 1 -> 0). A held-potential stall alarm (2
#     stalled windows, unattributed path) may coalesce into the same
#     capability-crisis window; every such window stays domain-free.
#   5 convergence: global alert tail ages out, two skip verifies build the
#     certified streak, every layer reaches absorbing, phi 0, system
#     balanced; composite semantics consistent at every sampled step.
#   6 negative path: a verify request with a malformed sigma param (object
#     shape) must be rejected with 422 and its error response must carry no
#     attribution or phase surface keys; the status surface stays intact
#     and unperturbed afterwards.
#
# Wait strategy: window seconds = 1 via the evaluator's documented env knob
# (EVORULE_EQUILIBRIUM_WINDOW_SECS); certified streak minimum = 2 via
# EVORULE_EQUILIBRIUM_CERTIFIED_STREAK_MIN; every stimulus is gated on an
# observed snapshot value (no fixed sleeps for critical transitions); the
# cross verify is paced immediately after the rules-crisis observation so
# capability-crisis windows stay contiguous (no relaxing backtracking).
#
# Dual-run regime: the full experiment executes twice with fresh instances;
# each round writes a normalized deterministic observation file (RLE step
# list of the tracked tuple including per-window attribution, per-layer
# distinct sequences, first crisis/absorbing step indices, attribution
# landmark indices, phi sequence; no timestamps, no wall-clock data). The
# two files must be SHA256 byte-equal.
#
# Usage: .\run-capability-attribution-acceptance.ps1 [-Port 18087]
# Prereq: release exe built at target\release\evorule-server.exe (repo root)
#         with the attribution observation keys deployed:
#         cargo build --release -p evorule-server
# Run under pwsh (ConvertFrom-Json -AsHashtable requires PowerShell 7+).

param(
    [int]$Port = 18087
)

$ErrorActionPreference = 'Stop'

$RepoRoot  = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)   # scripts\experiments -> repo root
$Exe       = Join-Path $RepoRoot "target\release\evorule-server.exe"
$EvalPath  = Join-Path $RepoRoot "resources\server_eval.json"
$Base      = "http://127.0.0.1:$Port"
$Stamp     = (Get-Date).ToUniversalTime().ToString('yyyyMMdd-HHmmss')
$RunRoot   = Join-Path $RepoRoot "data\experiment\run-capability-attribution-$Stamp"
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)
$WindowSecs = 2   # equilibrium window seconds (env knob of the evaluator; 2s keeps the single-window attribution snapshot safely above HTTP latency)
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

function First-StepIndex([string]$Layer, [string]$Value) {
    $steps = $script:tracker.Steps
    for ($i = 0; $i -lt $steps.Count; $i++) {
        if ($steps[$i].$Layer -eq $Value) { return $i }
    }
    return $null
}

function Find-StepIndex([scriptblock]$Probe) {
    $steps = $script:tracker.Steps
    for ($i = 0; $i -lt $steps.Count; $i++) {
        if (& $Probe $steps[$i]) { return $i }
    }
    return $null
}

function Get-DomainSum($Step) {
    $sum = 0
    foreach ($v in $Step.att_domains.Values) { $sum += [long]$v }
    return $sum
}

# ---- request payloads (constant across rounds; per-round request files) ----
$ctx = @{ source = 'capability-attribution-acceptance-driver'; measured_at = '2026-01-01T00:00:00Z' }
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
    # the out-of-bounds unvalidated revision opens the readjudication ledger
    # (proven by the system-phase acceptance run: identical shock revisions
    # dedup, phi lands at 2); the in-bounds verified revision keeps a second
    # distinct anchor probe; the contradiction verifies then emit the
    # law_review proposals that complete the chains (phi 2 -> 1 -> 0)
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

    $adjThermal = @(@{ instruction_type = 'thermal_growth_check'; state = 'pass' })
    $revOut = @{ constant_id = 'alpha_Al6061'; new_value = 0.0000285; new_uncertainty = 0.0000003; source = 'post-anomaly recalibration (unvalidated vendor datapoint)'; verified = $true }
    $revIn  = @{ constant_id = 'alpha_Al6061'; new_value = 0.0000242; new_uncertainty = 0.0000002; source = 'post-anomaly recalibration (design review)'; verified = $true }

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
            # only hashtable parsing accepts every JSON object shape the
            # observation surface may carry (empty-name keys included)
            return ($raw | ConvertFrom-Json -AsHashtable)
        }

        function Step-Track($Status) {
            # run-length encode the tracked tuple: a new step is recorded only
            # when any tracked value changes between polls; warm-up samples
            # with any tracked key absent (pre-first-settlement default view)
            # are skipped so the step list is boot-race free
            $eq = $Status.equilibrium
            if ($null -eq $eq) { return }
            $lp = $eq.layer_phases
            if ($null -eq $lp) { return }
            $att = $eq.alert_attribution
            if ($null -eq $att) { return }
            $attDomains = [ordered]@{}
            foreach ($k in ($att.domains.Keys | Sort-Object)) { $attDomains[$k] = $att.domains[$k] }
            $tuple = [ordered]@{
                capability      = $lp.capability
                rules           = $lp.rules
                data            = $lp.data
                system_phase    = $eq.system_phase
                system_balanced = $eq.system_balanced
                phi_total       = $eq.phi_total
                att_capability  = $att.capability
                att_domains     = $attDomains
                att_total       = $att.total
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

        # ---- reset the per-round tracker ----
        $script:tracker = @{
            Steps = New-Object System.Collections.Generic.List[object]
            Last  = $null
        }

        # ---- baseline: attribution block present, exact shape, zeroed ----
        # gate on the first settled window so both rounds start tracking from
        # the same post-settlement state
        $baseGate = Wait-PhaseStep { param($st) (
                ($null -ne $st.equilibrium.layer_phases) -and
                ($null -ne $st.equilibrium.alert_attribution) -and
                ($st.equilibrium.alert_free_windows -ge 1)
            ) } 15
        $baseSt = $baseGate.snapshot
        $att = $baseSt.equilibrium.alert_attribution
        $attKeys = @($att.Keys | Sort-Object)
        Assert-Eq "$Tag.B1.attribution_keys_exact" $attKeys @('capability', 'domains', 'total')
        Assert-Eq "$Tag.B2.attribution_baseline_zero" @($att.capability, @($att.domains.Keys).Count, $att.total) @(0, 0, 0)
        Assert-True "$Tag.B3.no_pseudo_domain_key_baseline" (-not (@($att.domains.Keys) -contains '')) "baseline attribution domains must not contain the empty-name pseudo-domain key"

        # ---- determinism anchors (out-of-bounds revision also opens the ledger) ----
        $xOut   = Invoke-Api '/api/services/graph_shock_propagate/invoke' "$Tag-x-out"       @{ rule_set = $eval; revision = $revOut; adjudications = $adjThermal }
        $xOutRe = Invoke-Api '/api/services/graph_shock_propagate/invoke' "$Tag-x-out-rerun" @{ rule_set = $eval; revision = $revOut; adjudications = $adjThermal }
        $xIn    = Invoke-Api '/api/services/graph_shock_propagate/invoke' "$Tag-x-in"        @{ rule_set = $eval; revision = $revIn;  adjudications = $adjThermal }
        $xInRe  = Invoke-Api '/api/services/graph_shock_propagate/invoke' "$Tag-x-in-rerun"  @{ rule_set = $eval; revision = $revIn;  adjudications = $adjThermal }

        Assert-True "$Tag.D1.shock_provenance_deterministic" ((($xOut.body.provenance.deterministic -eq $true) -and ($xIn.body.provenance.deterministic -eq $true))) "every shock response must flag deterministic"
        Assert-True "$Tag.D2.graph_digest_stable" ($xOut.body.result.graph_digest -eq $xIn.body.result.graph_digest) "graph digest must stay equal across revisions: $($xOut.body.result.graph_digest)|$($xIn.body.result.graph_digest)"
        Assert-True "$Tag.D3.input_digest_pairwise_distinct" ($xOut.body.result.input_digest -ne $xIn.body.result.input_digest) "distinct revisions must change input digest"
        Assert-True "$Tag.D4.shock_rerun_bytes_equal" ((Get-Sha256Of (Join-Path $RunDir "resp-$Tag-x-out.json")) -eq (Get-Sha256Of (Join-Path $RunDir "resp-$Tag-x-out-rerun.json"))) "identical shock rerun response bytes must match"
        Assert-True "$Tag.D5.probe_rerun_bytes_equal" ((Get-Sha256Of (Join-Path $RunDir "resp-$Tag-x-in.json")) -eq (Get-Sha256Of (Join-Path $RunDir "resp-$Tag-x-in-rerun.json"))) "identical probe rerun response bytes must match"

        # ledger gate: the out-of-bounds shock maps the thermal law into the
        # pending readjudication ledger (phi 2, deduped across identical reruns)
        $gateA = Wait-PhaseStep { param($st) $st.equilibrium.phi_total -eq 2 } 15
        Assert-True "$Tag.GA.ledger_opened_by_shock" ($gateA.hit) "out-of-bounds shock must open the pending ledger at phi 2; last equilibrium=$(EqDetail $gateA.snapshot)"

        # ---- domain-attributed alert: single-law contradiction -> rules crisis ----
        $v1 = Invoke-Api '/v1/criticality/verify' "$Tag-verify-thermal" $vThermalViolate
        Assert-True "$Tag.X1.verify_contradiction" (($v1.status -eq 200) -and ($v1.body.verdict -eq 'contradiction')) "violating claim must produce contradiction, got $($v1.status)/$($v1.body.verdict)"

        $gateB = Wait-PhaseStep { param($st) (LayerPhaseOf $st 'rules') -eq 'crisis' } 15
        Assert-True "$Tag.L1.rules_crisis_observed" ($gateB.hit -and ((First-StepIndex 'rules' 'crisis') -ne $null)) "rules layer must observe crisis with the domain-attributed alert; last equilibrium=$(EqDetail $gateB.snapshot)"

        # independence face A: the domain-attributed alert window shows the
        # engineering domain account charged and the capability account clean,
        # with the capability layer NOT in crisis at that same step
        $idxA = Find-StepIndex { param($s) (
                ($s.rules -eq 'crisis') -and
                ($s.capability -ne 'crisis') -and
                ($s.att_capability -eq 0) -and
                (([long]$s.att_domains['engineering']) -ge 1) -and
                ((Get-DomainSum $s) -eq [long]$s.att_total)
            ) }
        Assert-True "$Tag.A1.domain_alert_attributed_exclusively" ($null -ne $idxA) "a settled step must show rules=crisis with domains.engineering>=1, capability account 0, capability layer non-crisis (steps=$($script:tracker.Steps.Count), last=$(EqDetail $gateB.snapshot))"

        # ---- unattributed alert: cross-dataset contradiction -> capability only ----
        # paced immediately after the rules-crisis observation so capability-
        # crisis windows stay contiguous (no relaxing backtracking); the cross
        # verify's thermal law_review also completes the drain (phi 1 -> 0)
        $vM = Invoke-Api '/v1/criticality/verify' "$Tag-verify-cross" $vCrossViolate
        Assert-True "$Tag.X2.cross_verify_contradiction" (($vM.status -eq 200) -and ($vM.body.verdict -eq 'contradiction')) "cross-dataset violating claim must produce contradiction, got $($vM.status)/$($vM.body.verdict)"

        $gateM = Wait-PhaseStep { param($st) (LayerPhaseOf $st 'capability') -eq 'crisis' } 10
        Assert-True "$Tag.L2.capability_crisis_observed" ((First-StepIndex 'capability' 'crisis') -ne $null) "capability layer must observe crisis in the unattributed alert window; last equilibrium=$(EqDetail $gateM.snapshot)"

        # independence face B: an unattributed alert window shows the capability
        # account charged and the domains map empty (no pseudo-domain bucket)
        $idxB = Find-StepIndex { param($s) (
                ($s.capability -eq 'crisis') -and
                ($s.att_capability -ge 1) -and
                (@($s.att_domains.Keys).Count -eq 0) -and
                (([long]$s.att_capability + (Get-DomainSum $s)) -eq [long]$s.att_total)
            ) }
        Assert-True "$Tag.A2.unattributed_alert_attributed_exclusively" ($null -ne $idxB) "a settled step must show capability=crisis with att.capability>=1 and an empty domains map (steps=$($script:tracker.Steps.Count), last=$(EqDetail $gateM.snapshot))"

        $gatePhi = Wait-PhaseStep { param($st) $st.equilibrium.phi_total -eq 0 } 10
        Assert-True "$Tag.L3.ledger_drained_after_cross_verify" ($gatePhi.hit) "the cross verify must complete the readjudication drain (phi 0); last equilibrium=$(EqDetail $gatePhi.snapshot)"

        # ---- attribution accounting over the whole tracked run ----
        $steps = $script:tracker.Steps
        $invBad = 0; $invFirst = ''; $pseudoBad = $false
        for ($i = 0; $i -lt $steps.Count; $i++) {
            $s = $steps[$i]
            if (([long]$s.att_capability + (Get-DomainSum $s)) -ne [long]$s.att_total) {
                if ($invBad -eq 0) { $invFirst = "step $i cap=$($s.att_capability) domains=$(ConvertTo-NormalJson $s.att_domains) total=$($s.att_total)" }
                $invBad++
            }
            if (@($s.att_domains.Keys) -contains '') { $pseudoBad = $true }
        }
        Assert-True "$Tag.A3.attribution_invariant_every_step" ($invBad -eq 0) "capability + sum(domains) == total must hold at every sampled step ($($steps.Count) steps): first violation $invFirst"
        Assert-True "$Tag.A4.no_pseudo_domain_key_any_step" (-not $pseudoBad) "the domains map must never contain the empty-name pseudo-domain key"

        # onset ordering: the capability crisis must start strictly after the
        # rules crisis in this scenario (the first unattributed alert can only
        # land after the domain-attributed alert window)
        $fc = [ordered]@{ capability = (First-StepIndex 'capability' 'crisis'); rules = (First-StepIndex 'rules' 'crisis'); data = (First-StepIndex 'data' 'crisis') }
        Assert-True "$Tag.S1.rules_onset_before_capability_onset" (($null -ne $fc.rules) -and ($null -ne $fc.capability) -and ($fc.rules -lt $fc.capability)) "rules crisis onset must precede capability crisis onset (attribution split): rules=$($fc.rules) capability=$($fc.capability)"
        Assert-True "$Tag.S2.data_onset_with_rules" (($null -ne $fc.data) -and ($fc.data -le $fc.rules)) "data layer (phi progress face) must enter crisis no later than rules (readjudication ledger opens with the shock, before the first alert)"

        # ---- convergence: alert tails age out, streak builds, all absorb ----
        $gateD = Wait-PhaseStep { param($st) (@($st.equilibrium.window_alert_tail | Where-Object { $_ -gt 0 }).Count -eq 0) } 90
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
            ) } 120
        Assert-True "$Tag.G1.system_converged" ($gateF.hit) "system surface must converge to absorbing/balanced/phi-0 with all layers absorbing; last equilibrium=$(EqDetail $gateF.snapshot)"

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

        # ---- negative path: malformed sigma param -> 422, no surface keys ----
        $vBad = Invoke-Api '/v1/criticality/verify' "$Tag-verify-bad-param" $vBadUncertainty
        Assert-Eq "$Tag.N1.malformed_param_rejected_422" $vBad.status 422
        Assert-True "$Tag.N2.error_response_without_surface_keys" (($vBad.raw -notmatch 'alert_attribution') -and ($vBad.raw -notmatch 'system_phase') -and ($vBad.raw -notmatch 'layer_phases') -and ($vBad.raw -notmatch 'phi_total')) "rejected instruction response must carry no attribution or phase surface keys (instruction-level scope: the observation surface has no per-scenario view; the error response is the assertable carrier)"
        $postNeg = Wait-PhaseStep { param($st) ($null -ne $st.equilibrium.layer_phases) -and ($null -ne $st.equilibrium.system_phase) } 10
        Assert-True "$Tag.N3.surface_keys_present_after_negative" ($postNeg.hit) "status surface must keep the attribution and phase keys after the rejected instruction"
        Assert-Eq "$Tag.N4.no_spurious_perturbation_after_negative" @((LayerPhaseOf $postNeg.snapshot 'capability'), (LayerPhaseOf $postNeg.snapshot 'rules'), (LayerPhaseOf $postNeg.snapshot 'data'), $postNeg.snapshot.equilibrium.phi_total) @('absorbing', 'absorbing', 'absorbing', 0)

        # ---- normalized deterministic observations file ----
        $layerSeqs = [ordered]@{
            capability = @((Get-Rle @($steps | ForEach-Object { $_.capability })))
            rules      = @((Get-Rle @($steps | ForEach-Object { $_.rules })))
            data       = @((Get-Rle @($steps | ForEach-Object { $_.data })))
        }
        $phiSeq = @((Get-Rle $phiVals))
        $attCapSeq = @((Get-Rle @($steps | ForEach-Object { $_.att_capability })))
        $attTotalSeq = @((Get-Rle @($steps | ForEach-Object { $_.att_total })))
        $domainKeys = New-Object System.Collections.Generic.SortedSet[string]
        foreach ($s in $steps) { foreach ($k in $s.att_domains.Keys) { $domainKeys.Add([string]$k) | Out-Null } }
        $obs = [ordered]@{
            format               = 'capability-attribution-acceptance-observations-v1'
            window_secs          = $WindowSecs
            certified_streak_min = $StreakMin
            step_count           = $steps.Count
            steps                = $steps
            layer_sequences      = $layerSeqs
            attribution_cap_sequence = $attCapSeq
            attribution_total_sequence = $attTotalSeq
            attribution_domain_keys  = @($domainKeys)
            first_crisis_step    = $fc
            first_domain_attributed_step = $idxA
            first_unattributed_step = $idxB
            phi_sequence         = $phiSeq
        }
        $obsPath = Join-Path $RunDir "capability-attribution-observations.json"
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
    Write-Utf8NoBom (Join-Path $RunRoot "summary.txt") "assertions=$($assertions.Count) fail=$script:hadFail run=$Stamp port=$Port window_secs=$WindowSecs streak_min=$StreakMin"

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

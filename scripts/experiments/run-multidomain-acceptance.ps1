# Cross-domain shock-propagation real-machine acceptance driver (graph_shock_propagate,
# multidomain v1: explicit cross-domain edges + per-domain equilibrium phases backend).
# ASCII only, PS 5.1 compatible. Approved design decisions: scripts/experiments/ in git,
# full hard assertions, local acceptance surface (not in CI).
#
# Usage: .\run-multidomain-acceptance.ps1 [-Port 18084]
# Prereq: release exe built at target\release\evorule-server.exe (repo root):
#         cargo build --release -p evorule-server
#
# Chain: k_B revision (physical domain) -> thermal_growth_check (engineering, reference,
# depth1) -> demand_elasticity_check (economic, explicit cross_domain edge, depth2).
# Asserts: injection 1 full three-domain storm (coupling + cross_domain mixed BFS);
# injection 2 domain-inside in-bounds revision -> empty invalidated set -> frontier
# short-circuit, NO cross-domain propagation (regime gate unchanged); injection 3
# engineering out-of-bounds storm (same mixed shape); adjudication reversal keeps list
# order across domains (proposition reversal covers cross-domain chains, control
# stress_check survives); new output keys cross_domain_depth / knowledge_domains_touched;
# dedup of duplicated cross_domain_edges keeps graph_digest stable; determinism anchors
# (graph_digest equal across runs, input_digest pairwise distinct, injection-3 rerun
# SHA256 byte-equal, provenance.deterministic).

param(
    [int]$Port = 18084
)

$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)   # scripts\experiments -> repo root
$Exe      = Join-Path $RepoRoot "target\release\evorule-server.exe"
$EvalPath = Join-Path $RepoRoot "resources\server_eval.json"
$Base     = "http://127.0.0.1:$Port"
$Stamp    = (Get-Date).ToUniversalTime().ToString('yyyyMMdd-HHmmss')
$RunDir   = Join-Path $RepoRoot "data\experiment\run-multidomain-$Stamp"
$WalDir   = Join-Path $RunDir "wal"
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)

if (-not (Test-Path $Exe)) { throw "release exe not found: $Exe (run: cargo build --release -p evorule-server)" }
if (-not (Test-Path $EvalPath)) { throw "constitution not found: $EvalPath" }
New-Item -ItemType Directory -Force -Path $RunDir, $WalDir | Out-Null

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

# ---- deploy payloads (server_eval + multidomain augmentation, deterministic regeneration) ----
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
    formula = '|Δp·q − p₀·q₀|（需求弹性偏差）'
    dimensions = @(0, 0, 0, 0, 0, 0, 0)
    domains = @('economic')
}
$eval.metadata | Add-Member -MemberType NoteProperty -Name cross_domain_edges -Value @(
    @{ from = 'thermal_growth_check'; to = 'demand_elasticity_check' }
) -Force

$adj = @(
    @{ instruction_type = 'thermal_growth_check'; state = 'pass' },
    @{ instruction_type = 'clearance_check'; state = 'pass' },
    @{ instruction_type = 'tolerance_stack_check'; state = 'pass' },
    @{ instruction_type = 'demand_elasticity_check'; state = 'pass' },
    @{ instruction_type = 'stress_check'; state = 'pass' }
)
$revKb      = @{ constant_id = 'k_B'; new_value = 1.380649e-23; new_uncertainty = 0.0; source = 'CODATA 2018 / SI-2019 redefinition (exact)'; verified = $true }
$revAlphaIn = @{ constant_id = 'alpha_Al6061'; new_value = 0.0000242; new_uncertainty = 0.0000002; source = 'post-anomaly recalibration (design review)'; verified = $true }
$revAlphaOut = @{ constant_id = 'alpha_Al6061'; new_value = 0.0000285; new_uncertainty = 0.0000003; source = 'post-anomaly recalibration (unvalidated vendor datapoint)'; verified = $true }

# duplicate cross edge variant (dedup determinism anchor)
$evalDup = ($eval | ConvertTo-Json -Depth 64) | ConvertFrom-Json
$evalDup.metadata | Add-Member -MemberType NoteProperty -Name cross_domain_edges -Value @(
    @{ from = 'thermal_growth_check'; to = 'demand_elasticity_check' },
    @{ from = 'thermal_growth_check'; to = 'demand_elasticity_check' }
) -Force

$payloads = @(
    @{ tag = '1-kb-physical';  body = @{ rule_set = $eval; revision = $revKb; adjudications = $adj } },
    @{ tag = '2-alpha-inside'; body = @{ rule_set = $eval; revision = $revAlphaIn; adjudications = $adj } },
    @{ tag = '3-alpha-outside'; body = @{ rule_set = $eval; revision = $revAlphaOut; adjudications = $adj } },
    @{ tag = '5-kb-dup-edge';  body = @{ rule_set = $evalDup; revision = $revKb; adjudications = $adj } }
)

# expected domain-vector pair (all-zero economic demand + axis-0 engineering chain)
$expDims = @($null, $null)
$expDims[0] = @(0, 0, 0, 0, 0, 0, 0)
$expDims[1] = @(1, 0, 0, 0, 0, 0, 0)

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

    # ---- injections ----
    $r1  = Invoke-Shock '1-kb-physical'   $payloads[0].body
    $r2  = Invoke-Shock '2-alpha-inside'  $payloads[1].body
    $r3  = Invoke-Shock '3-alpha-outside' $payloads[2].body
    $r3b = Invoke-Shock '3-alpha-outside-rerun' $payloads[2].body
    $r5  = Invoke-Shock '5-kb-dup-edge'   $payloads[3].body

    # ---- injection 1: physical-domain k_B revision -> three-domain storm ----
    Assert-Eq 'inj1.shocked_constant' $r1.result.shocked_constant 'k_B'
    Assert-Eq 'inj1.affected' $r1.result.affected_instructions @(
        @{ depth = 2; instruction_type = 'clearance_check'; via = 'coupling' },
        @{ depth = 2; instruction_type = 'demand_elasticity_check'; via = 'cross_domain' },
        @{ depth = 1; instruction_type = 'thermal_growth_check'; via = 'reference' },
        @{ depth = 2; instruction_type = 'tolerance_stack_check'; via = 'coupling' }
    )
    Assert-Eq 'inj1.regime_invalidated' $r1.result.regime_invalidated @('clearance_check','demand_elasticity_check','thermal_growth_check','tolerance_stack_check')
    Assert-Eq 'inj1.adjudications_order_preserved' ($r1.result.adjudications_invalidated | Select-Object -First 4) @(
        @{ instruction_type = 'thermal_growth_check'; state = 'pass' },
        @{ instruction_type = 'clearance_check'; state = 'pass' },
        @{ instruction_type = 'tolerance_stack_check'; state = 'pass' },
        @{ instruction_type = 'demand_elasticity_check'; state = 'pass' }
    )
    Assert-True 'inj1.stress_check_survives' (-not (($r1.result.adjudications_invalidated | ForEach-Object { $_.instruction_type }) -contains 'stress_check')) "control adjudication must survive"
    Assert-Eq 'inj1.domains_touched' $r1.result.domains_touched $expDims
    Assert-Eq 'inj1.knowledge_domains_touched' $r1.result.knowledge_domains_touched @('economic','engineering')
    Assert-Eq 'inj1.cross_domain_depth' $r1.result.cross_domain_depth 2
    Assert-Eq 'inj1.propagation_depth' $r1.result.propagation_depth 2

    # ---- injection 2: engineering in-bounds revision -> empty invalidated, no propagation ----
    Assert-Eq 'inj2.shocked_constant' $r2.result.shocked_constant 'alpha_Al6061'
    Assert-Eq 'inj2.affected' $r2.result.affected_instructions @(@{ depth = 1; instruction_type = 'thermal_growth_check'; via = 'reference' })
    Assert-Eq 'inj2.regime_invalidated' $r2.result.regime_invalidated @()
    Assert-Eq 'inj2.adjudications_invalidated' $r2.result.adjudications_invalidated @()
    Assert-Eq 'inj2.domains_touched' $r2.result.domains_touched @()
    Assert-Eq 'inj2.knowledge_domains_touched' $r2.result.knowledge_domains_touched @()
    Assert-Eq 'inj2.cross_domain_depth' $r2.result.cross_domain_depth 0
    Assert-Eq 'inj2.propagation_depth' $r2.result.propagation_depth 1

    # ---- injection 3: engineering out-of-bounds storm (same mixed shape) ----
    Assert-Eq 'inj3.shocked_constant' $r3.result.shocked_constant 'alpha_Al6061'
    Assert-Eq 'inj3.affected' $r3.result.affected_instructions @(
        @{ depth = 2; instruction_type = 'clearance_check'; via = 'coupling' },
        @{ depth = 2; instruction_type = 'demand_elasticity_check'; via = 'cross_domain' },
        @{ depth = 1; instruction_type = 'thermal_growth_check'; via = 'reference' },
        @{ depth = 2; instruction_type = 'tolerance_stack_check'; via = 'coupling' }
    )
    Assert-Eq 'inj3.regime_invalidated' $r3.result.regime_invalidated @('clearance_check','demand_elasticity_check','thermal_growth_check','tolerance_stack_check')
    Assert-Eq 'inj3.adjudications_order_preserved' ($r3.result.adjudications_invalidated | Select-Object -First 4) @(
        @{ instruction_type = 'thermal_growth_check'; state = 'pass' },
        @{ instruction_type = 'clearance_check'; state = 'pass' },
        @{ instruction_type = 'tolerance_stack_check'; state = 'pass' },
        @{ instruction_type = 'demand_elasticity_check'; state = 'pass' }
    )
    Assert-True 'inj3.stress_check_survives' (-not (($r3.result.adjudications_invalidated | ForEach-Object { $_.instruction_type }) -contains 'stress_check')) "control adjudication must survive"
    Assert-Eq 'inj3.domains_touched' $r3.result.domains_touched $expDims
    Assert-Eq 'inj3.knowledge_domains_touched' $r3.result.knowledge_domains_touched @('economic','engineering')
    Assert-Eq 'inj3.cross_domain_depth' $r3.result.cross_domain_depth 2
    Assert-Eq 'inj3.propagation_depth' $r3.result.propagation_depth 2

    # ---- dedup anchor: duplicated cross edge keeps graph_digest stable ----
    Assert-Eq 'anchor.dup_edge_graph_digest_equal' $r5.result.graph_digest $r1.result.graph_digest

    # ---- determinism anchors ----
    Assert-True 'anchor.provenance_deterministic' (
        $r1.provenance.deterministic -eq $true -and $r2.provenance.deterministic -eq $true -and
        $r3.provenance.deterministic -eq $true -and $r5.provenance.deterministic -eq $true
    ) "provenance.deterministic must be true"
    Assert-Eq 'anchor.graph_digest_equal_5runs' @($r1.result.graph_digest, $r2.result.graph_digest, $r3.result.graph_digest, $r3b.result.graph_digest, $r5.result.graph_digest) @($r1.result.graph_digest, $r1.result.graph_digest, $r1.result.graph_digest, $r1.result.graph_digest, $r1.result.graph_digest)
    $digests = @($r1.result.input_digest, $r2.result.input_digest, $r3.result.input_digest)
    Assert-True 'anchor.input_digest_pairwise_distinct' (($digests[0] -ne $digests[1]) -and ($digests[1] -ne $digests[2]) -and ($digests[0] -ne $digests[2])) "revisions must change input digest: $($digests -join ',')"
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

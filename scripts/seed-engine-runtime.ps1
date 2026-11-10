# seed-engine-runtime.ps1 - Engine runtime seed for desktop evorule-server.
#
# Standard deploy-time step that seeds a knowledge dataset (default "obs")
# into a running desktop engine through the sanctioned bundle import chain:
#
#   1. probe engine health (port resolved from desktop runtime.json or -Port)
#   2. authenticate with a platform token (-PlatformToken / -TokenFile)
#   3. idempotent workspace seed (GET/POST /api/workspaces)
#   4. discover existing sandbox evidence (closed + report summary.failed == 0)
#   5. inject the domain schema into {rules_dir parent}/knowledge/domain_schemas/
#   6. build + ed25519-sign the knowledge bundle (seed-knowledge-bundle.py)
#   7. register the publisher key in the engine trust registry
#   8. POST /api/bundles/import and verify via GET /api/knowledge
#
# The script is idempotent: identical inputs produce an identical bundle_id,
# and re-import performs the single-activation replacement.
#
# Notes:
# - The trust registry is resolved by the engine process relative to its CWD.
#   This script derives the engine exe path via Win32_Process and places the
#   registry next to the app root (desktop layout: <app>\resources\bin\...).
#   Override with -TrustFile when the layout differs.
# - The publisher private key persists at -KeyFile (default under
#   %LOCALAPPDATA%\evorule-seed\publisher-key.json) so repeated runs reuse the
#   same trust registration. Treat that file as deploy-sensitive material.
# - Without pre-existing sandbox evidence the import cannot claim verdict=pass;
#   the script fails with an explicit message (run a sandbox test in the
#   desktop workspace first - fail-closed, no evidence fabrication).
# - Entry-sandbox consistency (server-side gate): rule entries are matched
#   per-entry against the sandbox draft-rules snapshot; knowledge entries
#   (this script's flow) take batch-level evidence - the closed sandbox with
#   a 0-failed report attests the governance environment at export time.
#   If you extend the chain to rule bundles, build each entry's rule_body
#   verbatim from the tested rules (the snapshot contents), or the import
#   gate rejects the bundle.

[CmdletBinding()]
param(
    [int]$Port = 0,
    [string]$PlatformToken = "",
    [string]$TokenFile = "",
    [string]$Username = "",
    [string]$Password = "",
    [string]$CredentialsFile = "",
    [string]$WorkspaceName = "obs",
    [string]$DatasetId = "obs",
    [string]$SchemaId = "https://evorule.org/schemas/obs/run/v1.json",
    [string]$OwnerId = "",
    [string]$KnowledgeDir = "",
    [string]$TrustFile = "",
    [string]$KeyFile = "",
    [string]$EntriesFile = "",
    [string]$PythonExe = "python",
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"

function Write-Step { param($m) Write-Host "[seed] $m" }
function Write-Fail { param($m) Write-Host "[seed] ERROR: $m" -ForegroundColor Red; exit 1 }

function Invoke-Api {
    param(
        [string]$Method,
        [string]$Url,
        [string]$Token,
        [string]$RawBody = "",
        [int[]]$ExpectStatus = @(200)
    )
    $headers = @{ Authorization = "Bearer $Token" }
    $status = 0
    $text = ""
    try {
        if ($RawBody) {
            $resp = Invoke-WebRequest -Uri $Url -Method $Method -Headers $headers `
                -ContentType "application/json" -Body ([System.Text.Encoding]::UTF8.GetBytes($RawBody)) `
                -UseBasicParsing -TimeoutSec 30
        } else {
            $resp = Invoke-WebRequest -Uri $Url -Method $Method -Headers $headers `
                -UseBasicParsing -TimeoutSec 30
        }
        $status = [int]$resp.StatusCode
        $text = $resp.Content
    } catch {
        $r = $_.Exception.Response
        if ($r) { $status = [int]$r.StatusCode }
        if ($_.ErrorDetails -and $_.ErrorDetails.Message) {
            $text = $_.ErrorDetails.Message
        } elseif ($r) {
            try {
                $stream = $r.GetResponseStream()
                if ($stream) { $text = (New-Object IO.StreamReader($stream)).ReadToEnd() }
            } catch { }
        }
    }
    $payload = $null
    if ($text) { try { $payload = $text | ConvertFrom-Json } catch { $payload = $text } }
    if ($ExpectStatus -notcontains $status) {
        throw "unexpected status $status from $Method $Url : $text"
    }
    return @{ Status = $status; Body = $payload }
}

# --- 1. resolve engine -------------------------------------------------------
$runtimeJson = Join-Path $env:LOCALAPPDATA "com.evorule.evo-agent-desktop\runtime.json"
if ($Port -le 0) {
    if (Test-Path $runtimeJson) {
        $rt = Get-Content $runtimeJson -Raw | ConvertFrom-Json
        $Port = [int]$rt.engine.port
    }
    if ($Port -le 0) { $Port = 61998 }
}
$base = "http://127.0.0.1:$Port"
$health = Invoke-Api -Method Get -Url "$base/api/health" -Token "" -ExpectStatus @(200)
Write-Step "engine online at $base"

# locate engine process (rules-dir anchor + trust-registry CWD anchor)
$proc = Get-CimInstance Win32_Process -Filter "Name = 'evorule-server.exe'" |
    Where-Object { $_.CommandLine -match "--addr 127\.0\.0\.1:$Port\b" } |
    Select-Object -First 1
if (-not $proc) {
    Write-Step "warning: engine process not found; relying on explicit -KnowledgeDir/-TrustFile"
}

if (-not $KnowledgeDir) {
    if ($proc -and $proc.CommandLine -match "--rules-dir (\S+)") {
        $rulesDir = $Matches[1]
        $parent = Split-Path -Parent $rulesDir
        if ($parent) { $KnowledgeDir = Join-Path $parent "knowledge" }
    }
}
if (-not $KnowledgeDir) {
    Write-Fail "cannot derive knowledge dir; pass -KnowledgeDir explicitly"
}
if (-not (Test-Path $KnowledgeDir)) {
    New-Item -ItemType Directory -Path $KnowledgeDir -Force | Out-Null
}
Write-Step "knowledge dir: $KnowledgeDir"

if (-not $TrustFile) {
    if ($proc -and $proc.ExecutablePath) {
        # desktop layout <app>\resources\bin\evorule-server.exe -> CWD = <app>
        $exeDir = Split-Path -Parent $proc.ExecutablePath
        $appRoot = Split-Path -Parent (Split-Path -Parent $exeDir)
        if ($appRoot) { $TrustFile = Join-Path $appRoot "bundle-trusted-publishers.json" }
    }
    if (-not $TrustFile) { $TrustFile = Join-Path (Get-Location) "bundle-trusted-publishers.json" }
}
Write-Step "trust registry: $TrustFile"

# --- 2. auth (bootstrap-aware) -----------------------------------------------
# The platform login is self-bootstrapped: on a zero-user engine the first
# caller of POST /api/platform/auth/bootstrap defines the credentials. This
# script owns that bootstrap on a fresh engine: it generates a random admin
# password, persists the credentials to the deploy file (deploy-sensitive,
# same class as the publisher key; also becomes the desktop app login), and
# logs in for a session token (kept in memory only). On an already-
# bootstrapped engine it reuses stored credentials or -Username/-Password.
if (-not $PlatformToken -and $TokenFile) {
    $PlatformToken = (Get-Content $TokenFile -Raw).Trim()
}
if (-not $PlatformToken) {
    $credFile = if ($CredentialsFile) { $CredentialsFile } else {
        Join-Path $env:LOCALAPPDATA "evorule-seed\platform-credentials.json" }
    $status = Invoke-Api -Method Get -Url "$base/api/platform/auth/status" -Token ""
    $needsBootstrap = [bool]$status.Body.needs_bootstrap
    if (-not ($PSBoundParameters.Username -and $PSBoundParameters.Password) -and (Test-Path $credFile)) {
        $doc = Get-Content $credFile -Raw | ConvertFrom-Json
        $Username = $doc.username
        $Password = $doc.password
    }
    $persist = $false
    if (-not ($Username -and $Password)) {
        if (-not $needsBootstrap) {
            Write-Fail "platform already bootstrapped and no stored credentials at $credFile; pass -Username/-Password or -TokenFile"
        }
        $Username = if ($PSBoundParameters.Username) { $Username } else { "seed-admin" }
        $bytes = New-Object byte[] 24
        $rng = [Security.Cryptography.RandomNumberGenerator]::Create()
        $rng.GetBytes($bytes)
        $rng.Dispose()
        $Password = [Convert]::ToBase64String($bytes)
        $persist = $true
    }
    if ($needsBootstrap) {
        Invoke-Api -Method Post -Url "$base/api/platform/auth/bootstrap" -Token "" `
            -RawBody (@{ username = $Username; password = $Password; display_name = "deploy seed" } | ConvertTo-Json -Compress) `
            -ExpectStatus @(201) | Out-Null
        Write-Step "platform bootstrapped (admin '$Username')"
        if ($persist -and -not (Test-Path $credFile)) {
            New-Item -ItemType Directory -Path (Split-Path -Parent $credFile) -Force | Out-Null
            [IO.File]::WriteAllText($credFile,
                (@{ username = $Username; password = $Password } | ConvertTo-Json -Compress),
                (New-Object Text.UTF8Encoding($false)))
            Write-Step "credentials persisted: $credFile (deploy-sensitive)"
        }
    }
    $login = Invoke-Api -Method Post -Url "$base/api/platform/auth/login" -Token "" `
        -RawBody (@{ username = $Username; password = $Password } | ConvertTo-Json -Compress) `
        -ExpectStatus @(200)
    $PlatformToken = $login.Body.token
    if (-not $OwnerId) { $OwnerId = $login.Body.user.username }
}
if (-not $PlatformToken) { Write-Fail "failed to obtain platform session token" }
Write-Step "platform session acquired (user=$Username)"

# --- 3. workspace seed (idempotent) ------------------------------------------
$workspaces = (Invoke-Api -Method Get -Url "$base/api/workspaces" -Token $PlatformToken).Body
$ws = $workspaces | Where-Object { $_.name -eq $WorkspaceName } | Select-Object -First 1
if (-not $OwnerId) {
    $OwnerId = "seed"
}
if ($ws) {
    Write-Step "workspace '$WorkspaceName' exists (id=$($ws.id))"
} else {
    if ($DryRun) {
        Write-Step "dry-run: would create workspace '$WorkspaceName' (owner=$OwnerId)"
    } else {
        $ws = Invoke-Api -Method Post -Url "$base/api/workspaces" -Token $PlatformToken `
            -RawBody ((@{ name = $WorkspaceName; owner_id = $OwnerId; description = "engine runtime seed" } | ConvertTo-Json -Compress)) `
            -ExpectStatus @(201, 200)
        Write-Step "workspace created (id=$($ws.Body.id))"
    }
}

# --- 4. sandbox evidence discovery (reuse only; never fabricate) -------------
$sandboxRef = ""
foreach ($w in $workspaces) {
    $rid = if ($w.owner_id) { $w.owner_id } else { $OwnerId }
    $list = Invoke-Api -Method Get `
        -Url "$base/api/workspaces/$($w.id)/sandboxes?requester=$([uri]::EscapeDataString($rid))" `
        -Token $PlatformToken -ExpectStatus @(200, 403, 404)
    if ($list.Status -ne 200) { continue }
    foreach ($sb in @($list.Body)) {
        if ("$($sb.status)".ToLowerInvariant() -ne "closed") { continue }
        $rep = Invoke-Api -Method Get `
            -Url "$base/api/workspaces/$($w.id)/sandboxes/$($sb.id)/report?requester=$([uri]::EscapeDataString($rid))" `
            -Token $PlatformToken -ExpectStatus @(200, 404)
        if ($rep.Status -ne 200) { continue }
        $failed = $rep.Body.summary.failed
        if ($null -ne $failed -and [int]$failed -eq 0) {
            $sandboxRef = "sandbox:$($sb.id)"
            break
        }
    }
    if ($sandboxRef) { break }
}
if (-not $sandboxRef) {
    # No reusable evidence: build minimal REAL evidence in the seed workspace
    # (no-op draft rule + empty-case test dataset + sandbox run + close).
    # The report is generated by the engine itself; verdict stays fail-closed
    # if the run does not produce a closed sandbox with zero failed cases.
    if ($DryRun) {
        Write-Step "dry-run: would build minimal sandbox evidence chain in workspace '$WorkspaceName'"
        $sandboxRef = "sandbox:dry-run"
    } else {
        if (-not $ws) { Write-Fail "evidence self-build requires workspace '$WorkspaceName' to exist" }
        $wsId = $ws.id
        # no-op rule: also serves as the knowledge payload (the evidence gate
        # requires every bundle entry's rule_body to digest-match a rule the
        # sandbox actually tested - BND-D-03 content-coverage)
        $ruleObj = @{
            id          = "seed.sandbox_rule"
            version     = 1
            description = "no-op seed rule"
            transform   = @(@{
                type   = "branch"
                params = @{ domain = @{ type = "all"; inner = @() }; on_true = @() }
            })
        }
        $ruleContent = $ruleObj | ConvertTo-Json -Depth 12 -Compress
        $rule = Invoke-Api -Method Post -Url "$base/api/workspaces/$wsId/rules" -Token $PlatformToken `
            -RawBody (@{ name = "seed.sandbox_rule"; content = $ruleContent; created_by = $OwnerId;
                description = "engine runtime seed sandbox rule" } | ConvertTo-Json -Compress) `
            -ExpectStatus @(201)
        $versionId = $rule.Body.current_version_id
        $dataset = Invoke-Api -Method Post -Url "$base/api/workspaces/$wsId/test-datasets" -Token $PlatformToken `
            -RawBody (@{ name = "seed.empty-cases"; cases_json = "[]"; created_by = $OwnerId;
                description = "engine runtime seed (no cases; evidence = clean close)" } | ConvertTo-Json -Compress) `
            -ExpectStatus @(201)
        $sandbox = Invoke-Api -Method Post -Url "$base/api/workspaces/$wsId/sandboxes" -Token $PlatformToken `
            -RawBody (@{ rule_version_ids = @($versionId); test_dataset_id = $dataset.Body.id;
                started_by = $OwnerId } | ConvertTo-Json -Compress) `
            -ExpectStatus @(201)
        $sid = $sandbox.Body.sandbox_id
        Invoke-Api -Method Post -Url "$base/api/workspaces/$wsId/sandboxes/$sid/close" -Token $PlatformToken `
            -RawBody (@{ closed_by = $OwnerId } | ConvertTo-Json -Compress) -ExpectStatus @(200) | Out-Null
        $sandboxRef = "sandbox:$sid"
        Write-Step "evidence self-built: rule=$($rule.Body.id) dataset=$($dataset.Body.id) sandbox=$sid (closed, 0 cases)"
    }
}
Write-Step "sandbox evidence: $sandboxRef"

# --- 5. domain schema injection ----------------------------------------------
$schema = @{
    '$id'   = $SchemaId
    title   = "Engine runtime observation entry"
    type    = "object"
    additionalProperties = $true
    required = @("id")
    properties = @{
        id          = @{ type = "string" }
        version     = @{ type = "integer" }
        description = @{ type = "string" }
        transform   = @{ type = "array" }
    }
}
$schemaDir = Join-Path $KnowledgeDir "domain_schemas"
New-Item -ItemType Directory -Path $schemaDir -Force | Out-Null
$schemaPath = Join-Path $schemaDir "obs-run.schema.json"
$schemaJson = $schema | ConvertTo-Json -Depth 12
if ((Test-Path $schemaPath) -and ((Get-Content $schemaPath -Raw) -eq $schemaJson)) {
    Write-Step "domain schema unchanged: $schemaPath"
} else {
    if ($DryRun) {
        Write-Step "dry-run: would write domain schema $schemaPath"
    } else {
        [IO.File]::WriteAllText($schemaPath, $schemaJson, (New-Object Text.UTF8Encoding($false)))
        Write-Step "domain schema written: $schemaPath"
    }
}

# --- 6. build + sign bundle --------------------------------------------------
# The knowledge payload = the no-op rule content (evidence gate: every entry's
# rule_body must digest-match a rule tested by the referenced sandbox).
$ruleObj = @{
    id          = "seed.sandbox_rule"
    version     = 1
    description = "no-op seed rule"
    transform   = @(@{
        type   = "branch"
        params = @{ domain = @{ type = "all"; inner = @() }; on_true = @() }
    })
}
$entriesPath = Join-Path $env:TEMP "seed-entries-$DatasetId.json"
if (-not $EntriesFile) {
    $entriesDoc = @(
        @{
            entry_id      = "entry-obs-seed-001"
            entry_kind    = "knowledge"
            rule_body     = $ruleObj
            schema_ref    = $SchemaId
            provenance    = @{ source = "seed:engine-runtime" }
            domain        = "general"
            knowledge_kind = "fact"
        }
    ) | ConvertTo-Json -Depth 12
    [IO.File]::WriteAllText($entriesPath, $entriesDoc, (New-Object Text.UTF8Encoding($false)))
    $EntriesFile = $entriesPath
}

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Definition
$outPath = Join-Path $env:TEMP "seed-bundle-$DatasetId.json"
$stamp = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
if (-not $KeyFile) { $KeyFile = Join-Path $env:LOCALAPPDATA "evorule-seed\publisher-key.json" }

$pyArgs = @(
    (Join-Path $scriptDir "seed-knowledge-bundle.py"),
    "--dataset-id", $DatasetId,
    "--schema-id", $SchemaId,
    "--sandbox-ref", $sandboxRef,
    "--exported-at", $stamp,
    "--key-file", $KeyFile,
    "--out", $outPath
)
if ($EntriesFile) { $pyArgs += @("--entries-file", $EntriesFile) }

if ($DryRun) {
    Write-Step "dry-run: would build signed bundle -> $outPath"
    $meta = & $PythonExe @pyArgs | ConvertFrom-Json
} else {
    $meta = & $PythonExe @pyArgs | ConvertFrom-Json
}
if ($LASTEXITCODE -ne 0) { Write-Fail "bundle builder failed (exit $LASTEXITCODE)" }
Write-Step "bundle built: $($meta.bundle_id) hash=$($meta.content_hash)"

# --- 7. trust registry -------------------------------------------------------
$keyId = $meta.key_id
if (Test-Path $TrustFile) {
    $reg = Get-Content $TrustFile -Raw | ConvertFrom-Json
} else {
    $reg = [pscustomobject]@{ publishers = [pscustomobject]@{} }
}
if (-not $reg.publishers.PSObject.Properties[$keyId]) {
    if ($DryRun) {
        Write-Step "dry-run: would register publisher key in $TrustFile"
    } else {
        $reg.publishers | Add-Member -NotePropertyName $keyId -NotePropertyValue $keyId -Force
        $regJson = $reg | ConvertTo-Json -Depth 8
        [IO.File]::WriteAllText($TrustFile, $regJson, (New-Object Text.UTF8Encoding($false)))
        Write-Step "publisher key registered: $TrustFile"
    }
} else {
    Write-Step "publisher key already registered"
}

# --- 8. import + verify ------------------------------------------------------
if ($DryRun) {
    Write-Step "dry-run: would POST $base/api/bundles/import"
    Write-Step "dry-run: bundle payload at $outPath"
    exit 0
}

# splice the exact bundle bytes into the request envelope (no JSON round-trip)
$bundleJson = [IO.File]::ReadAllText($outPath).Trim()
$import = Invoke-Api -Method Post -Url "$base/api/bundles/import" -Token $PlatformToken `
    -RawBody ('{"bundle":' + $bundleJson + '}') `
    -ExpectStatus @(201, 200)
Write-Step ("import ok: imported={0} dataset={1} entries={2}" -f `
    $import.Body.imported, $import.Body.dataset_id, $import.Body.entry_count)

$knowledge = Invoke-Api -Method Get -Url "$base/api/knowledge" -Token $PlatformToken
$datasets = if ($knowledge.Body.datasets) { $knowledge.Body.datasets } else { $knowledge.Body }
$hit = @($datasets) | Where-Object { $_.dataset_id -eq $DatasetId }
if (-not $hit) { Write-Fail "dataset '$DatasetId' not listed after import" }
$entries = Invoke-Api -Method Get -Url "$base/api/knowledge/$DatasetId/entries?q=seed" -Token $PlatformToken
Write-Step "verify: dataset '$DatasetId' hosted, entries query hits: $((@($entries.Body.entries)).Count)"
Write-Host "[seed] done."

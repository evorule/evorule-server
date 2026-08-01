# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 EvoRule Project
<#
.SYNOPSIS
    evorule-server Docker image local build script

.DESCRIPTION
    Constructs Docker build context (containing both evorule-application/core/ and
    evorule/ necessary files), then runs docker build.

    This script shares the same build context layout as the CI workflow
    (.github/workflows/ci.yml build-docker job).

.PARAMETER Tag
    Image tag, default evorule-server:local

.PARAMETER EvorulePath
    Local path to evorule core repo, default ../evorule (relative to repo root)

.PARAMETER NoCache
    Disable Docker build cache

.EXAMPLE
    .\build-docker.ps1
    .\build-docker.ps1 -Tag evorule-server:v0.1.0
    .\build-docker.ps1 -EvorulePath ..\evorule -NoCache
#>
[CmdletBinding()]
param(
    [string]$Tag = "evorule-server:local",
    [string]$EvorulePath = "",
    [switch]$NoCache
)

$ErrorActionPreference = "Stop"

# 1. Resolve paths
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$AppRoot = Split-Path -Parent $ScriptDir
$ParentDir = Split-Path -Parent $AppRoot

if ([string]::IsNullOrEmpty($EvorulePath)) {
    $EvorulePath = Join-Path $ParentDir "evorule"
}

Write-Host "=== evorule-application: $AppRoot ===" -ForegroundColor Cyan
Write-Host "=== evorule core:        $EvorulePath ===" -ForegroundColor Cyan

# 2. Validate evorule core exists
$coreFiles = @(
    "evorule-tcb\Cargo.toml",
    "evorule-reactor\Cargo.toml",
    "evorule-governance\Cargo.toml",
    "evorule-tcb\core_eval.json",
    "rules",
    "Cargo.toml",
    "Cargo.lock"
)
foreach ($f in $coreFiles) {
    $p = Join-Path $EvorulePath $f
    if (-not (Test-Path $p)) {
        Write-Error "evorule core file missing: $p`nVerify -EvorulePath, or clone evorule repo to $ParentDir\evorule\"
        exit 1
    }
}

# 3. Validate evorule-application files
$appFiles = @(
    "Dockerfile",
    ".dockerignore",
    "core\evorule-server\Cargo.toml",
    "core\io_handlers\Cargo.toml"
)
foreach ($f in $appFiles) {
    $p = Join-Path $AppRoot $f
    if (-not (Test-Path $p)) {
        Write-Error "evorule-application file missing: $p"
        exit 1
    }
}

# 4. Validate Docker available
if (-not (Get-Command docker -ErrorAction SilentlyContinue)) {
    Write-Error "docker command not available, install Docker Desktop or Docker Engine first"
    exit 1
}

# 5. Construct temporary build context
$BuildCtx = Join-Path $env:TEMP "evorule-docker-build-$(Get-Random)"
Write-Host "=== Build context: $BuildCtx ===" -ForegroundColor Cyan

New-Item -ItemType Directory -Path "$BuildCtx\evorule-application" -Force | Out-Null
New-Item -ItemType Directory -Path "$BuildCtx\evorule" -Force | Out-Null

# 5.1 Copy evorule-application necessary files (core/, Dockerfile, .dockerignore)
Copy-Item -Path (Join-Path $AppRoot "Dockerfile") -Destination $BuildCtx -Force
Copy-Item -Path (Join-Path $AppRoot ".dockerignore") -Destination $BuildCtx -Force
Copy-Item -Path (Join-Path $AppRoot "core") -Destination "$BuildCtx\evorule-application\" -Recurse -Force

# 5.2 Copy evorule core necessary files (tier0/tier1/tier2 + rules + Cargo.toml/lock)
$coreCopyItems = @(
    "evorule-tcb",
    "evorule-reactor",
    "evorule-governance",
    "rules",
    "Cargo.toml",
    "Cargo.lock"
)
foreach ($item in $coreCopyItems) {
    $src = Join-Path $EvorulePath $item
    Copy-Item -Path $src -Destination "$BuildCtx\evorule\" -Recurse -Force
}

# 5.3 Clean target/ dirs in build context (avoid bloating context)
Get-ChildItem -Path $BuildCtx -Recurse -Directory -Filter "target" | Remove-Item -Recurse -Force

Write-Host "=== Build context structure ===" -ForegroundColor Cyan
Get-ChildItem $BuildCtx -Recurse -Depth 2 | Select-Object FullName

# 6. Build image
Write-Host "=== Building Docker image: $Tag ===" -ForegroundColor Green

$buildArgs = @("build", "-t", $Tag, "-f", "$BuildCtx\Dockerfile")
if ($NoCache) {
    $buildArgs += "--no-cache"
}
$buildArgs += $BuildCtx

Write-Host "docker $($buildArgs -join ' ')" -ForegroundColor DarkGray
& docker @buildArgs
$buildExit = $LASTEXITCODE

# 7. Cleanup build context
Write-Host "=== Cleaning build context ===" -ForegroundColor Cyan
Remove-Item -Recurse -Force $BuildCtx -ErrorAction SilentlyContinue

if ($buildExit -ne 0) {
    Write-Error "Docker build failed (exit code: $buildExit)"
    exit $buildExit
}

Write-Host "=== Build success: $Tag ===" -ForegroundColor Green
Write-Host ""
Write-Host "Run example:" -ForegroundColor Cyan
Write-Host "  docker run --rm -p 18080:18080 -v `${PWD}/data:/data $Tag"
Write-Host "  docker run --rm -p 18080:18080 -e EVORULE_ADDR=127.0.0.1:18080 $Tag"

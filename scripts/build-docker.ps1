# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 EvoRule Project
<#
.SYNOPSIS
    evorule-server Docker image local build script

.DESCRIPTION
    直接在仓根目录运行 docker build,使用独立仓 Dockerfile。
    evorule 核心 (evorule-tcb/reactor/governance) 从 crates.io 拉取,
    无需本地核心仓源码。

    Dockerfile 使用 BuildKit cache mount,需要 DOCKER_BUILDKIT=1。

.PARAMETER Tag
    Image tag, default evorule-server:local

.PARAMETER NoCache
    Disable Docker build cache

.EXAMPLE
    .\build-docker.ps1
    .\build-docker.ps1 -Tag evorule-server:v0.1.0
    .\build-docker.ps1 -NoCache
#>
[CmdletBinding()]
param(
    [string]$Tag = "evorule-server:local",
    [switch]$NoCache
)

$ErrorActionPreference = "Stop"

# 1. Resolve repo root
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = Split-Path -Parent $ScriptDir

Write-Host "=== Repo root: $RepoRoot ===" -ForegroundColor Cyan

# 2. Validate Dockerfile exists
$dockerfile = Join-Path $RepoRoot "Dockerfile"
if (-not (Test-Path $dockerfile)) {
    Write-Error "Dockerfile not found: $dockerfile"
    exit 1
}

# 3. Validate Docker available
if (-not (Get-Command docker -ErrorAction SilentlyContinue)) {
    Write-Error "docker command not available, install Docker Desktop or Docker Engine first"
    exit 1
}

# 4. Build image (BuildKit required for --mount=type=cache in Dockerfile)
$env:DOCKER_BUILDKIT = "1"

Write-Host "=== Building Docker image: $Tag ===" -ForegroundColor Green

$buildArgs = @("build", "-t", $Tag)
if ($NoCache) {
    $buildArgs += "--no-cache"
}
$buildArgs += $RepoRoot

Write-Host "docker $($buildArgs -join ' ')" -ForegroundColor DarkGray
& docker @buildArgs
$buildExit = $LASTEXITCODE

if ($buildExit -ne 0) {
    Write-Error "Docker build failed (exit code: $buildExit)"
    exit $buildExit
}

Write-Host "=== Build success: $Tag ===" -ForegroundColor Green
Write-Host ""
Write-Host "Run example:" -ForegroundColor Cyan
Write-Host "  docker run --rm -p 18080:18080 -v `${PWD}/data:/data $Tag"
Write-Host "  docker run --rm -p 18080:18080 -e EVORULE_ADDR=127.0.0.1:18080 $Tag"

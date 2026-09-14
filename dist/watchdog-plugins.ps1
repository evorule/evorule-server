# watchdog-plugins.ps1 - Deployment-side watchdog for external plugin processes.
# Watches the public GET /api/health external-plugin liveness section and
# restarts configured plugin processes when they stay offline.
#
# Semantics (58 plan W1; UV-182 batches A/C/D/F):
#   - acts only on plugins present in /api/health with status == "offline"
#   - "unauthorized" (401/403 credentials/config problem) is surfaced loudly but
#     NEVER acted on: the process is alive, restarting does not help (batch A)
#   - no_probe / absent plugins are reported as-is and never acted on
#   - server health fetch failures tolerate N consecutive bad cycles (default 3,
#     health_fail_threshold) before skipping action cycles; every failure logs
#     WARN, threshold-and-beyond logs ERROR; PS 5.1 non-JSON responses collapse
#     to an empty map and count the same way (batch C)
#   - debounce: restart only after N consecutive offline cycles (offline_threshold)
#   - restart budget: max_restarts_per_hour per plugin; BOTH successful and
#     FAILED start attempts consume budget so a misconfigured command converges
#     to the escalation latch instead of retrying forever (batch D)
#   - over budget the watchdog stops attempting and emits an ESCALATION log line
#     (system-only path)
#   - the escalation latch clears automatically once the plugin is seen online
#   - optional self-guard (batch H / legacy L3): -InstallSelfGuard registers a
#     current-user ONLOGON scheduled task (hidden) so the watchdog itself comes
#     back after reboot; -UninstallSelfGuard removes the task
#   - actions append to data\watchdog.log; the log rotates at 5 MB into
#     watchdog.log.old (single generation) (batch F)
#   - the audit-facing trail stays in the server (plugin_offline /
#     plugin_unauthorized / plugin_online platform events)
#
# Config: plugins-watchdog.json next to this script (see README-STARTUP.txt).
# ASCII-only on purpose: avoid codepage-sensitive output on any host.

param(
    [string]$ConfigPath = "",
    # UV-182 batch H (legacy L3): optional self-guard management. The watchdog
    # is a plain process - if it dies, nothing revives it. The self-guard is a
    # current-user scheduled task that starts it (hidden) at every logon.
    [switch]$InstallSelfGuard,
    [switch]$UninstallSelfGuard
)

$ErrorActionPreference = "Continue"

if ($ConfigPath -eq "") {
    $ConfigPath = Join-Path $PSScriptRoot "plugins-watchdog.json"
}
$logPath = Join-Path $PSScriptRoot "data\watchdog.log"

function Write-Log([string]$Level, [string]$Msg) {
    $line = "{0} [watchdog] {1} {2}" -f (Get-Date -Format "yyyy-MM-dd HH:mm:ss"), $Level, $Msg
    Write-Host $line
    try {
        $dir = Split-Path $logPath -Parent
        if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
        # UV-182 batch F: size-cap rotation - at 5 MB move to .old (single
        # generation; the current file is recreated on next append)
        if ((Test-Path $logPath) -and ((Get-Item $logPath).Length -ge 5242880)) {
            Move-Item -Path $logPath -Destination ($logPath + ".old") -Force
            Write-Host ("{0} [watchdog] INFO watchdog.log rotated to watchdog.log.old (5 MB cap)" -f (Get-Date -Format "yyyy-MM-dd HH:mm:ss"))
        }
        Add-Content -Path $logPath -Value $line -Encoding ASCII
    } catch {}
}

# UV-182 batch H: self-guard management. Handled BEFORE the single-instance
# mutex so registration works while another instance is already running.
# schtasks quoting: inner quotes are pre-escaped as \" for the native call
# (PowerShell 5.1 native argument passing).
$taskName = "evorule-plugin-watchdog"
if ($InstallSelfGuard -or $UninstallSelfGuard) {
    if ($UninstallSelfGuard) {
        schtasks /Delete /TN $taskName /F | Out-Null
        Write-Log "INFO" ("self-guard: scheduled task '{0}' deleted (or not present)" -f $taskName)
    }
    if ($InstallSelfGuard) {
        $self = Join-Path $PSScriptRoot "watchdog-plugins.ps1"
        $action = 'powershell -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File \"' + $self + '\"'
        schtasks /Create /F /TN $taskName /SC ONLOGON /RL LIMITED /TR $action | Out-Null
        if ($LASTEXITCODE -eq 0) {
            Write-Log "INFO" ("self-guard: scheduled task '{0}' registered (hidden, current user, at logon); watchdog survives reboot" -f $taskName)
        } else {
            Write-Log "ERROR" ("self-guard: task registration failed (schtasks exit {0}); watchdog stays manual" -f $LASTEXITCODE)
        }
    }
    exit 0
}

# single-instance guard (per logon session)
$script:mutex = New-Object System.Threading.Mutex($false, "evorule-plugin-watchdog")
if (-not $script:mutex.WaitOne(0)) {
    Write-Log "WARN" "another watchdog instance is already running; exit"
    exit 0
}

if (-not (Test-Path $ConfigPath)) {
    Write-Log "ERROR" ("config not found: " + $ConfigPath)
    exit 1
}
try {
    $cfg = Get-Content $ConfigPath -Raw -Encoding UTF8 | ConvertFrom-Json
} catch {
    Write-Log "ERROR" ("config parse failed: " + $_.Exception.Message)
    exit 1
}

$plugins = @{}
if ($cfg.plugins) {
    $cfg.plugins.PSObject.Properties | ForEach-Object { $plugins[$_.Name] = $_.Value }
}
if ($plugins.Count -eq 0) {
    Write-Log "INFO" "no plugins configured in plugins-watchdog.json; nothing to watch; exit"
    exit 0
}

$interval     = if ($cfg.interval_secs)          { [int]$cfg.interval_secs }          else { 10 }
$threshold    = if ($cfg.offline_threshold)      { [int]$cfg.offline_threshold }      else { 3 }
$maxRestarts  = if ($cfg.max_restarts_per_hour)  { [int]$cfg.max_restarts_per_hour }  else { 5 }
$fetchTimeout = if ($cfg.fetch_timeout_secs)     { [int]$cfg.fetch_timeout_secs }     else { 5 }
$healthUrl    = if ($cfg.health_url)             { $cfg.health_url }                  else { "http://127.0.0.1:18080/api/health" }
# UV-182 batch C: consecutive bad health cycles tolerated before the watchdog
# stops acting (server restart window / transient glitch); default 3
$failTolerance = if ($cfg.health_fail_threshold) { [int]$cfg.health_fail_threshold }  else { 3 }

Write-Log "INFO" ("watchdog start: url={0} interval={1}s threshold={2} max_restarts/h={3} fail_tolerance={4} plugins=[{5}]" -f $healthUrl, $interval, $threshold, $maxRestarts, $failTolerance, (($plugins.Keys | Sort-Object) -join ","))

# per-plugin state: consecutive offline counter, restart timestamps, escalation latch
$offlineCount = @{}
$restarts     = @{}
$latched      = @{}
$plugins.Keys | ForEach-Object {
    $offlineCount[$_] = 0
    $restarts[$_]     = New-Object System.Collections.Generic.List[datetime]
    $latched[$_]      = $false
}

function Get-StatusMap([string]$Url, [int]$TimeoutSec) {
    try {
        $h = Invoke-RestMethod -Uri $Url -TimeoutSec $TimeoutSec
        $map = @{}
        if ($h.plugins) {
            $h.plugins.PSObject.Properties | ForEach-Object {
                $map[$_.Name] = $_.Value.status   # $null when no_probe (no liveness status)
            }
        }
        return ,$map
    } catch {
        return $null
    }
}

function Start-PluginProcess($Id, $P) {
    $cmd = $P.command
    if (-not $cmd) { Write-Log "ERROR" ("{0}: config missing 'command'; cannot start" -f $Id); return $false }
    $full = $null
    if (Test-Path $cmd) { $full = $cmd }
    elseif (Test-Path (Join-Path $PSScriptRoot $cmd)) { $full = Join-Path $PSScriptRoot $cmd }
    else { Write-Log "ERROR" ("{0}: command not found: {1}" -f $Id, $cmd); return $false }
    $wd = $PSScriptRoot
    if ($P.working_dir) { $wd = Join-Path $PSScriptRoot $P.working_dir }
    $envSaved = @{}
    if ($P.env) {
        $P.env.PSObject.Properties | ForEach-Object {
            $envSaved[$_.Name] = [System.Environment]::GetEnvironmentVariable($_.Name)
            [System.Environment]::SetEnvironmentVariable($_.Name, $_.Value)
        }
    }
    try {
        $argLine = ""
        if ($P.args) { $argLine = ($P.args | ForEach-Object { '"' + ($_ -replace '"', '\"') + '"' }) -join " " }
        Start-Process -FilePath $full -ArgumentList $argLine -WorkingDirectory $wd -WindowStyle Hidden | Out-Null
        Write-Log "INFO" ("{0}: process started ({1} {2})" -f $Id, $full, $argLine)
        return $true
    } catch {
        Write-Log "ERROR" ("{0}: start failed: {1}" -f $Id, $_.Exception.Message)
        return $false
    } finally {
        $envSaved.Keys | ForEach-Object {
            [System.Environment]::SetEnvironmentVariable($_, $envSaved[$_])
        }
    }
}

# UV-182 batch C: consecutive bad health cycle counter (unreachable OR empty
# map - PS 5.1 non-JSON responses collapse to an empty map and count the same)
$healthFails = 0

while ($true) {
    $map = Get-StatusMap $healthUrl $fetchTimeout
    if ($null -eq $map -or $map.Count -eq 0) {
        # UV-182 batch C: tolerate up to health_fail_threshold bad cycles
        # (server restart window / transient glitch) before giving up acting;
        # every failure logs WARN, threshold-and-beyond escalates to ERROR
        $healthFails = $healthFails + 1
        if ($healthFails -ge $failTolerance) {
            Write-Log "ERROR" ("server health bad {0}/{1} (unreachable or empty); skip cycle (no action); HUMAN ATTENTION if persistent" -f $healthFails, $failTolerance)
        } else {
            Write-Log "WARN" ("server health bad {0}/{1} (unreachable or empty); skip cycle (no action)" -f $healthFails, $failTolerance)
        }
    } else {
        if ($healthFails -gt 0) { Write-Log "INFO" ("server health recovered after {0} bad cycle(s); fail counter reset" -f $healthFails) }
        $healthFails = 0
        foreach ($id in $plugins.Keys) {
            $status = $null
            if ($map.ContainsKey($id)) { $status = $map[$id] }
            if ($null -eq $status) {
                # absent from health or no_probe: as-is, never acted on
                if ($offlineCount[$id] -ne 0) { Write-Log "INFO" ("{0}: status no_probe/absent; no action (as-is)" -f $id) }
                $offlineCount[$id] = 0
                continue
            }
            if ($status -eq "online") {
                if ($offlineCount[$id] -gt 0) { Write-Log "INFO" ("{0}: back online; counter reset" -f $id) }
                if ($latched[$id]) { $latched[$id] = $false; Write-Log "INFO" ("{0}: escalation latch cleared (online observed)" -f $id) }
                $offlineCount[$id] = 0
                continue
            }
            if ($status -eq "unauthorized") {
                # UV-182 batch A alignment: 401/403 = credentials/config problem;
                # the process is alive, restarting would not help - surface
                # loudly, never act
                if ($offlineCount[$id] -ne 0) { Write-Log "WARN" ("{0}: status unauthorized (credentials/config problem); no action (restart would not help); check token env config" -f $id) }
                $offlineCount[$id] = 0
                continue
            }
            if ($status -ne "offline") { continue }   # unknown future states: no action
            $offlineCount[$id] = $offlineCount[$id] + 1
            if ($offlineCount[$id] -lt $threshold) {
                Write-Log "INFO" ("{0}: offline {1}/{2}" -f $id, $offlineCount[$id], $threshold)
                continue
            }
            if ($latched[$id]) {
                Write-Log "WARN" ("{0}: offline and escalation-latched; NOT restarting (budget exhausted); human action required" -f $id)
                continue
            }
            $now = Get-Date
            $recent = @($restarts[$id] | Where-Object { $_ -gt $now.AddHours(-1) })
            $restarts[$id] = New-Object System.Collections.Generic.List[datetime]
            $recent | ForEach-Object { $restarts[$id].Add($_) }
            if ($recent.Count -ge $maxRestarts) {
                $latched[$id] = $true
                Write-Log "ESCALATION" ("{0}: restart budget exhausted ({1}/hour); watchdog gives up; HUMAN ACTION REQUIRED" -f $id, $recent.Count)
                continue
            }
            Write-Log "WARN" ("{0}: offline {1}/{2}; attempting restart #{3}" -f $id, $offlineCount[$id], $threshold, ($recent.Count + 1))
            if (Start-PluginProcess $id $plugins[$id]) {
                $restarts[$id].Add($now)
                $offlineCount[$id] = 0
            } else {
                # UV-182 batch D: a FAILED start also consumes the restart
                # budget so a misconfigured command converges to the escalation
                # latch instead of retrying forever every cycle; the offline
                # counter is NOT reset (the process is still down)
                $restarts[$id].Add($now)
                Write-Log "WARN" ("{0}: start attempt FAILED; consumed restart budget ({1}/hour); latch re-evaluated next cycle" -f $id, ($recent.Count + 1))
            }
        }
    }
    Start-Sleep -Seconds $interval
}

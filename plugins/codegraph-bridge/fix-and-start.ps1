$nssm = Join-Path $PSScriptRoot "nssm\win64\nssm.exe"
$svc  = "EvoRuleCodeGraphBridge"
$work = $PSScriptRoot

& $nssm stop $svc 2>$null
Start-Sleep 1
& $nssm set $svc AppDirectory $work
& $nssm set $svc AppStdout  "$work\bridge.out.log"
& $nssm set $svc AppStderr  "$work\bridge.err.log"
& $nssm set $svc AppExit Default Restart
& $nssm set $svc AppRestartDelay 5000
"AppDirectory now:"
& $nssm get $svc AppDirectory
"starting..."
& $nssm start $svc
Start-Sleep 4
& $nssm status $svc

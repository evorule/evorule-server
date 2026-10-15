# 以管理员身份运行此脚本
# 注册 codegraph-bridge 为 Windows 服务（开机自启）
$ErrorActionPreference = "Stop"
$nssm = Join-Path $PSScriptRoot "nssm\win64\nssm.exe"
$svc  = "EvoRuleCodeGraphBridge"
$node = "D:\nodejs\node-v22.11.0-win-x64\node.exe"
$workdir = $PSScriptRoot

# 已存在则先移除
& $nssm stop $svc 2>$null
& $nssm remove $svc confirm 2>$null

& $nssm install $svc $node "server.mjs"
& $nssm set $svc AppDirectory $workdir
& $nssm set $svc DisplayName "EvoRule CodeGraph Bridge"
& $nssm set $svc Description "HTTP bridge exposing codegraph as evorule external plugin on 127.0.0.1:9150"
& $nssm set $svc Start SERVICE_AUTO_START
& $nssm set $svc AppStdout  "$workdir\bridge.out.log"
& $nssm set $svc AppStderr  "$workdir\bridge.err.log"
& $nssm set $svc AppExit Default Restart
& $nssm set $svc AppRestartDelay 5000

& $nssm start $svc
Start-Sleep 3
& $nssm status $svc
Write-Host "`nDone. Check: curl http://127.0.0.1:9150/health" -ForegroundColor Green

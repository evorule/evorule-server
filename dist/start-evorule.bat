@echo off
rem ASCII-only on purpose: cmd batch parser is codepage-sensitive with non-ASCII text.
rem Chinese instructions live in README-STARTUP.txt (open with Notepad).
cd /d "%~dp0"
if not exist data mkdir data
echo ================================================
echo   evorule demo   ^|   http://localhost:18080
echo ================================================
echo.
rem --- Pre-flight: detect busy ports 18080 and 18081 before starting ---
set PORT_IN_USE=0
netstat -ano | findstr ":18080 " | findstr "LISTENING" >nul 2>nul
if not errorlevel 1 set PORT_IN_USE=1
netstat -ano | findstr ":18081 " | findstr "LISTENING" >nul 2>nul
if not errorlevel 1 set PORT_IN_USE=1
if "%PORT_IN_USE%"=="1" (
  powershell -NoProfile -Command "[System.Reflection.Assembly]::LoadWithPartialName('System.Windows.Forms') | Out-Null; [System.Windows.Forms.MessageBox]::Show('Port 18080 or 18081 is already in use by another program. Close that program (or change the port in this script), then restart evorule.','evorule - Port in use','OK','Warning')"
  exit /b 1
)
echo.
echo [1/3] Starting governance service (evorule-rule-serve, port 18081)...
rem --- Governance credentials: generated once per machine into data\ (gitignored),
rem --- never hardcoded in the repository. Delete the file to regenerate.
if not exist "data\governance-credentials.env" (
  powershell -NoProfile -Command "$s='evorule-' + [guid]::NewGuid().ToString('N').Substring(0,24); $p=[guid]::NewGuid().ToString('N').Substring(0,16); Set-Content -Path 'data\governance-credentials.env' -Value ('SECRET=' + $s + [Environment]::NewLine + 'ADMIN_PASSWORD=' + $p) -Encoding Ascii"
)
set GOV_SECRET=
set GOV_ADMIN_PASSWORD=
for /f "usebackq tokens=1,* delims==" %%a in (`powershell -NoProfile -Command "Get-Content 'data\governance-credentials.env'"`) do (
  if /i "%%a"=="SECRET" set GOV_SECRET=%%b
  if /i "%%a"=="ADMIN_PASSWORD" set GOV_ADMIN_PASSWORD=%%b
)
if "%GOV_SECRET%"=="" (
  echo [!] Failed to prepare governance credentials ^(data\governance-credentials.env^).
  pause
  exit /b 1
)
echo     Governance admin credentials: data\governance-credentials.env ^(keep private^)
start "evorule-rule" /min cmd /c "evorule-rule-serve.exe --db .\data\rule.db --port 18081 --secret %GOV_SECRET% --admin-user admin --admin-password %GOV_ADMIN_PASSWORD% --allowed-origins http://localhost:18080,http://127.0.0.1:18080 2>>data\rule-serve-stderr.log"
echo [2/3] Starting main service (evorule-server, port 18080)...
rem --- Registry: prefer service_registry.local.json (deployment-injected credentials,
rem --- gitignored); falls back to the tracked template (empty credential values).
set SVC_REG=service_registry.json
if exist "service_registry.local.json" set SVC_REG=service_registry.local.json
start "evorule-server" /min cmd /c "evorule-server.exe --addr 127.0.0.1:18080 --web-dir web --rules-dir rules --service-registry %SVC_REG% --core-eval resources\server_eval.json --wal-dir .\data\wal --wal-fsync --plugins plugin_manifest.json --workspace-db .\data\workspace.db 2>>data\server-stderr.log"
rem [Optional] ai-plugin: start automatically only when its config file exists
if exist "plugins\ai-plugin\ai-plugin.json" (
  echo [+] ai-plugin config found, starting ai-plugin (port 9130)...
  start "evorule-ai-plugin" /min cmd /c "plugins\ai-plugin\evorule-ai-plugin.exe --config plugins\ai-plugin\ai-plugin.json 2>>data\ai-plugin-stderr.log"
)
echo [3/3] Waiting for main service on port 18080...
set PROBE_OK=0
for /l %%i in (1,1,8) do (
  curl -s -o nul http://127.0.0.1:18080 >nul 2>nul
  if not errorlevel 1 set PROBE_OK=1
  timeout /t 1 /nobreak >nul
)
if not "%PROBE_OK%"=="1" (
  powershell -NoProfile -Command "[System.Reflection.Assembly]::LoadWithPartialName('System.Windows.Forms') | Out-Null; [System.Windows.Forms.MessageBox]::Show('The evorule main service failed to start within about 8 seconds. The error details are in the log file, which will now be opened in Notepad.','evorule - Start failed','OK','Error')"
  start notepad data\server-stderr.log
  exit /b 1
)
echo Main service is ready.
start "" "http://localhost:18080"
echo.
echo Done. If the browser did not open, visit http://localhost:18080
echo If a service fails to start, check data\server-stderr.log or
echo data\rule-serve-stderr.log for the error message.
echo To stop: close BOTH minimized windows ("evorule-server" and "evorule-rule") in the taskbar.
echo (For Chinese instructions, open README-STARTUP.txt)
echo.
pause

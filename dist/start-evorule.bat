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
start "evorule-rule" /min cmd /c "evorule-rule-serve.exe --db .\data\rule.db --port 18081 --secret evorule-demo-secret-2026 --admin-user admin --admin-password evorule-demo --allowed-origins http://localhost:18080,http://127.0.0.1:18080 2>>data\rule-serve-stderr.log"
echo [2/3] Starting main service (evorule-server, port 18080)...
start "evorule-server" /min cmd /c "evorule-server.exe --addr 127.0.0.1:18080 --web-dir web --rules-dir rules --service-registry service_registry.json --core-eval resources\server_eval.json --wal-dir .\data\wal --wal-fsync --plugins plugin_manifest.json --workspace-db .\data\workspace.db --insecure-serve 2>>data\server-stderr.log"
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

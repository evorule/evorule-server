@echo off
rem ASCII-only on purpose: cmd batch parser is codepage-sensitive with non-ASCII text.
rem Chinese instructions live in README-STARTUP.txt (open with Notepad).
cd /d "%~dp0"
if not exist data mkdir data
echo ================================================
echo   evorule demo   ^|   http://localhost:18080
echo ================================================
echo.
echo [1/3] Starting governance service (evorule-rule-serve, port 18081)...
start "evorule-rule" /min cmd /c "evorule-rule-serve.exe --db .\data\rule.db --port 18081 --secret evorule-demo-secret-2026 --admin-user admin --admin-password evorule-demo --allowed-origins http://localhost:18080,http://127.0.0.1:18080 2>>data\rule-serve-stderr.log"
echo [2/3] Starting main service (evorule-server, port 18080)...
start "evorule-server" /min cmd /c "evorule-server.exe --addr 127.0.0.1:18080 --web-dir web --rules-dir rules --service-registry service_registry.json --core-eval resources\server_eval.json --wal-dir .\data\wal --wal-fsync --plugins plugin_manifest.json --workspace-db .\data\workspace.db --insecure-serve 2>>data\server-stderr.log"
timeout /t 2 /nobreak >nul
echo [3/3] Opening browser...
start "" "http://localhost:18080"
echo.
echo Done. If the browser did not open, visit http://localhost:18080
echo If a service fails to start, check data\server-stderr.log or
echo data\rule-serve-stderr.log for the error message.
echo To stop: close BOTH minimized windows ("evorule-server" and "evorule-rule") in the taskbar.
echo (For Chinese instructions, open README-STARTUP.txt)
echo.
pause

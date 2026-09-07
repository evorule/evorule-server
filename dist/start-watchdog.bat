@echo off
rem ASCII-only on purpose: cmd batch parser is codepage-sensitive with non-ASCII text.
rem Optional deployment-side watchdog for external plugin processes.
rem Configure plugins-watchdog.json FIRST (see README-STARTUP.txt, section
rem "Plugin watchdog"). With an empty plugins map the watchdog exits at once.
rem Chinese instructions live in README-STARTUP.txt.
cd /d "%~dp0"
if not exist data mkdir data
echo Starting plugin watchdog (minimized window)...
start "evorule-watchdog" /min powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0watchdog-plugins.ps1"
echo Watchdog started in a minimized window ("evorule-watchdog").
echo Log: data\watchdog.log
echo To stop: close the minimized "evorule-watchdog" window.
echo.
pause

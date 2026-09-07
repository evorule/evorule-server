@echo off
rem Start finance-config external plugin service (pure ASCII per repo hard constraint)
rem Admin API requires env FINANCE_PLUGIN_ADMIN_TOKEN before starting.
rem Usage: start-finance-config.bat [port] [data_dir]
setlocal
set PORT=%1
if "%PORT%"=="" set PORT=9110
set DATA=%2
if "%DATA%"=="" set DATA=%~dp0data

echo [finance-config] starting on 127.0.0.1:%PORT% data=%DATA%
if "%FINANCE_PLUGIN_ADMIN_TOKEN%"=="" (
    echo [finance-config] WARNING: FINANCE_PLUGIN_ADMIN_TOKEN not set - admin API will return 503
)

pushd "%~dp0"
cargo run --quiet --release -- --port %PORT% --data "%DATA%"
set RC=%ERRORLEVEL%
popd
echo [finance-config] exited with code %RC%
exit /b %RC%

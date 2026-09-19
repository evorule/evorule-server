#!/bin/sh
# start-watchdog.sh - Deployment-side plugin watchdog launcher (Linux package;
# 回归验证 legacy L1). Optional: only needed if you want external plugin
# processes auto-restarted. Requires python3; without it use systemd
# Restart=always instead (see README-STARTUP.txt).
cd "$(dirname "$0")" || exit 1
mkdir -p data
if ! command -v python3 >/dev/null 2>&1; then
    echo "python3 not found: the Linux plugin watchdog needs python3."
    echo "Alternative: systemd Restart=always (see README-STARTUP.txt)."
    exit 1
fi
if [ -f data/watchdog.pid ] && kill -0 "$(cat data/watchdog.pid)" 2>/dev/null; then
    echo "Plugin watchdog already running (pid $(cat data/watchdog.pid))."
    exit 0
fi
echo "Starting plugin watchdog in background (log: data/watchdog.log)..."
nohup python3 "$(pwd)/watchdog-plugins.py" >> data/watchdog.log 2>&1 &
echo $! > data/watchdog.pid
echo "Watchdog started (pid $(cat data/watchdog.pid))."
echo "Stop: kill \$(cat data/watchdog.pid)"
echo "Configure plugins first: plugins-watchdog.json (see README-STARTUP.txt)."

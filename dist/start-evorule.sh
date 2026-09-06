#!/usr/bin/env bash
# evorule 体验版一键启动(Linux) — 与 start-evorule.bat 同构
# 中文说明见 README-STARTUP.txt
set -e
cd "$(dirname "$0")"
mkdir -p data
echo "================================================"
echo "  evorule demo   |   http://localhost:18080"
echo "================================================"
echo
echo "[1/3] Starting governance service (evorule-rule-serve, port 18081)..."
nohup ./evorule-rule-serve \
  --db ./data/rule.db --port 18081 \
  --secret evorule-demo-secret-2026 \
  --admin-user admin --admin-password evorule-demo \
  --allowed-origins http://localhost:18080,http://127.0.0.1:18080 \
  2>>data/rule-serve-stderr.log &
echo "[2/3] Starting main service (evorule-server, port 18080)..."
nohup ./evorule-server \
  --addr 127.0.0.1:18080 --web-dir web --rules-dir rules \
  --service-registry service_registry.json \
  --core-eval resources/server_eval.json \
  --wal-dir ./data/wal --wal-fsync \
  2>>data/server-stderr.log &
sleep 2
echo "[3/3] Done. Visit http://localhost:18080"
echo "If a service fails to start, check data/server-stderr.log or"
echo "data/rule-serve-stderr.log for the error message."
echo "To stop: pkill -f evorule-server ; pkill -f evorule-rule-serve"

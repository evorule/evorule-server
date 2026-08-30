#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# 标准开发启动脚本 —— 让 call_external / call_service 真正可用。
#
# call_external 不通的根因：evorule-server 默认不加载 service_registry.json
# （--service-registry 为空是「不暴露外部服务」的有意安全默认）。README 的
# 「快速开始」命令漏了该参数，导致按默认启动后 echo_svc 等 service_name
# 查不到，运行时报 unknown service_name。
#
# 本脚本补全所有必需参数：
#   --service-registry  挂载 service_registry.json（service_name→URL 映射）
#   --allow-loopback    放行 127.0.0.1（SSRF 防护默认拦截本机地址，否则调不通 echo_svc）
#   --rules-dir         ./rules（含 10_role13_demo.json 等规则）
#   --wal-dir           WAL 持久化
#   --auto-verify       审计链实时验证
#
# 用法：
#   ./dev-start.sh          # 前台运行，Ctrl+C 退出
#   ./dev-start.sh &        # 后台运行
set -euo pipefail
cd "$(dirname "$0")"

BIN="target/debug/evorule-server"
if [ ! -x "$BIN" ]; then
  BIN="target/release/evorule-server"
fi
if [ ! -x "$BIN" ]; then
  echo "ERROR: 找不到编译产物，请先 cargo build：" >&2
  echo "  cargo build --bin evorule-server" >&2
  exit 1
fi

if [ ! -f ./service_registry.json ]; then
  echo "WARN: ./service_registry.json 不存在，call_external 将不可用" >&2
fi

exec "$BIN" \
  --addr 127.0.0.1:18080 \
  --rules-dir ./rules \
  --service-registry ./service_registry.json \
  --wal-dir ./data/wal \
  --auto-verify \
  --allow-loopback

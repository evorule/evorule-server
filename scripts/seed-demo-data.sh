#!/usr/bin/env bash
# seed-demo-data.sh — EvoRule 演示数据播种脚本 (Linux/macOS bash)
# 用法: bash seed-demo-data.sh
# 依赖: 已启动的 evorule-server (默认 127.0.0.1:18080), 以及 curl / python3
set -u

BASE="${BASE:-http://127.0.0.1:18080}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SEED_DIR="$(cd "$SCRIPT_DIR/../data/seed" && pwd)"

echo "[seed] EvoRule demo data seeder -> $BASE"

# 1. 探测 server
if ! curl -sf -m 3 "$BASE/api/state" >/dev/null; then
  echo "[seed] 错误: 无法连接 $BASE/api/state ，请先启动 evorule-server (端口 18080)。" >&2
  exit 1
fi
echo "[seed] server 在线"

# 2. 新建会话
SID="$(curl -sf -X POST -H 'Content-Type: application/json' -d '{}' "$BASE/api/sessions" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["session_id"])')"
echo "[seed] 新建会话 session_id = $SID"

files="finance_expenses.json finance_invoices.json medical_patients.json medical_prescriptions.json"
total=0; ok=0

for f in $files; do
  p="$SEED_DIR/$f"
  if [ ! -f "$p" ]; then echo "[seed] 跳过缺失文件: $p"; continue; fi
  n="$(python3 -c 'import json,sys;print(len(json.load(open(sys.argv[1],encoding="utf-8"))))' "$p")"
  echo "[seed] == $f : $n 条 =="

  # 用 python 逐条构造 body 并 curl
  python3 - "$p" "$BASE" "$SID" <<'PY' | while IFS= read -r line; do echo "$line"; done
import json, sys, urllib.request, time
path_file, base, sid = sys.argv[1], sys.argv[2], sys.argv[3]
recs = json.load(open(path_file, encoding="utf-8"))
def post(url, obj):
    data = json.dumps(obj).encode("utf-8")
    req = urllib.request.Request(url, data=data, headers={"Content-Type":"application/json"}, method="POST")
    return json.load(urllib.request.urlopen(req, timeout=5))
for r in recs:
    try:
        if "path" in r:
            post(f"{base}/api/sessions/{sid}/payload", {"path": r["path"], "value": r["value"]})
            print(f"   [payload] {r['path']}")
        elif "instruction_type" in r:
            resp = post(f"{base}/api/sessions/{sid}/command",
                        {"instruction": {"type": r["instruction_type"], "params": r["params"]}})
            print(f"   [command] {r['instruction_type']} -> fact_id={resp.get('fact_id')}  (期望: {r.get('expect','')})")
            time.sleep(0.15)
    except Exception as e:
        print(f"   [error] {e}", file=sys.stderr)
PY
done

sleep 0.5
echo "[seed] 完成。查看结果: GET $BASE/api/sessions/$SID/state"

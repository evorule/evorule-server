#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 EvoRule Project
"""UDF 调用开销基准（三通道对照，端到端延迟 P50/P95/P99）

回答 ADR-0001 R8 挂账：「HTTP 往返 + 序列化开销（UDF 高频小调用场景）到底多少」。

通道：
  A server REST invoke —— 全链：鉴权→registry 路由→HTTP 转发→wasm-host→UDF→回程
  B wasm-host 直调     —— 跳过 server 跳板：client→wasm-host→UDF
  C server /api/health —— 纯 server HTTP 基线（无 UDF 无转发）

R8 答案 = A - B（server 跳板+二次序列化开销）与 A - C（全链 UDF 执行+转发净成本）。

前置：部署态双进程（server:18080 release/debug 均可 + wasm-host:9140）。
用法：python3 bench_udf_invoke.py [-n 200] [-w 20]
输出：三通道延迟统计 + 差值分解结论（stdout）；退出码恒 0（基准非门禁）。
"""

from __future__ import annotations

import argparse
import sys
import json
import statistics
import time
import urllib.request

_OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))

SERVER = "http://127.0.0.1:18080"
HOST = "http://127.0.0.1:9140"
UDF = "udf_finance_tax_calc"
PAYLOAD = json.dumps({"amount_cents": 100000, "rate_bp": 600}).encode()


def timed(method: str, url: str, data: bytes | None = None) -> float:
    req = urllib.request.Request(url, data=data, method=method,
                                 headers={"Content-Type": "application/json"})
    t0 = time.perf_counter()
    with _OPENER.open(req, timeout=30):
        pass
    return (time.perf_counter() - t0) * 1000.0


def bench(name: str, method: str, url: str, data: bytes | None, n: int, warm: int) -> dict:
    for _ in range(warm):
        timed(method, url, data)
    samples = [timed(method, url, data) for _ in range(n)]
    samples.sort()

    def pct(p: float) -> float:
        idx = min(len(samples) - 1, max(0, round(p / 100 * len(samples)) - 1))
        return samples[idx]

    stat = {
        "name": name,
        "n": n,
        "mean": round(statistics.fmean(samples), 3),
        "p50": round(pct(50), 3),
        "p95": round(pct(95), 3),
        "p99": round(pct(99), 3),
        "max": round(samples[-1], 3),
    }
    print(f"  {name}: mean={stat['mean']}ms p50={stat['p50']}ms "
          f"p95={stat['p95']}ms p99={stat['p99']}ms max={stat['max']}ms")
    return stat


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("-n", type=int, default=200, help="测量轮数（默认 200）")
    ap.add_argument("-w", type=int, default=20, help="预热轮数（默认 20）")
    ap.add_argument("--skip-server", action="store_true", help="跳过 A 通道（server REST invoke）")
    args = ap.parse_args()

    # REST invoke 请求体契约=裸 args（wasm-host 顶层读参；{args:...} 包装形态为调用方误用）
    invoke_body = json.dumps({"amount_cents": 100000, "rate_bp": 600}).encode()

    print(f"== UDF 调用开销基准（n={args.n}, warm={args.w}，本地环回）==")
    skip_server = "--skip-server" in sys.argv
    a = None
    if not skip_server:
        a = bench("A server REST invoke（全链）", "POST",
                  f"{SERVER}/api/services/{UDF}/invoke", invoke_body, args.n, args.w)
    else:
        print("  A 通道跳过（--skip-server）")
    b = bench("B wasm-host 直调（跳过 server）", "POST",
              f"{HOST}/services/{UDF}", PAYLOAD, args.n, args.w)
    c = bench("C server /api/health 基线", "GET", f"{SERVER}/api/health", None, args.n, args.w)

    if a:
        hop = round(a["p50"] - b["p50"], 3)
        total = round(a["p50"] - c["p50"], 3)
        print()
        print("== R8 差值分解（p50 口径）==")
        print(f"  server 跳板+二次序列化开销 (A-B): {hop} ms")
        print(f"  全链 UDF 净成本           (A-C): {total} ms")
        print(f"  结论: 单次 UDF 调用端到端 p50 = {a['p50']} ms；"
              f"server 代理跳板增量 {hop} ms——"
              + ("高频小调用场景可接受（<5ms），R8 可闭环"
                 if hop < 5 else
                 "增量显著，高频场景建议评估进程内通道（另立评估）"))
        return 0
    print()
    print("== R8 部分结论（B/C 口径）==")
    print(f"  进程外通道（wasm-host HTTP 直调）p50 = {b['p50']} ms")
    print(f"  纯 server HTTP 基线           p50 = {c['p50']} ms")
    print("  （A 通道 server REST invoke 因 body 契约断裂暂缺——登记册 O 项修复后补测）")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

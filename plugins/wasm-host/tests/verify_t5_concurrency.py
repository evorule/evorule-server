#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 EvoRule Project
"""T5 并发正确性端到端验证（阶段 2 · T5）

真实起进程（wasm-host + evorule-server），N 路并发直调 UDF，断言：

  A. 每路入参唯一（线程号派生），响应与该路期望严格一致 —— 路与路之间
     无串扰、无交叉污染（任一路收到别路的参数即失败）；
  B. 同路多次重复调用结果完全一致 —— UDF 确定性（同入参必同出参）；
  C. 全部并发跑完后 host /health 与 server /api/health 仍 200，
     且再单发一次正常请求仍 200 —— 并发未击穿宿主。

入参取整设计：amount_cents = (线程号+1)*10000，乘以任意 rate_bp 后
必被 10000 整除，tax_cents 期望值无取整歧义。

前置：
  ① server 二进制（在 evorule-server crate 内 `cargo build`）；
  ② wasm-host 二进制（在 plugins/wasm-host 内 `cargo build`）；
  ③ plugins/wasm/ 下有示例 .wasm；plugin_manifest.json 中 wasm-host enabled=true。

用法：
    python verify_t5_concurrency.py [--threads 16] [--iters 8] [--keep]

退出码：0 = 全过；1 = 有失败。
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

# 代理环境变量会劫持 127.0.0.1 请求（本机沙箱存在 HTTP_PROXY），显式绕过
_OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))

HERE = Path(__file__).resolve().parent   # plugins/wasm-host/tests
HOST_ROOT = HERE.parent                          # plugins/wasm-host
REPO = HOST_ROOT.parent.parent                   # 仓根（plugin_manifest.json / rules / resources 所在）
# workspace 共享 target 目录在仓根（.cargo/config.toml 重定向），非 crate 子目录
SERVER_EXE = REPO / "target" / "debug" / (
    "evorule-server.exe" if os.name == "nt" else "evorule-server"
)
HOST_EXE = HOST_ROOT / "target" / "debug" / (
    "evorule-wasm-host.exe" if os.name == "nt" else "evorule-wasm-host"
)

SERVER_PORT = 18097
HOST_PORT = 9140
UDF = "udf_finance_tax_calc"

_failures: list[str] = []


def check(ok: bool, name: str, detail: str = "") -> None:
    tag = "[PASS]" if ok else "[FAIL]"
    line = f"  {tag} {name}"
    if detail:
        line += f" {detail}"
    print(line, flush=True)
    if not ok:
        _failures.append(name)


def http(url: str, method: str = "GET", body: dict | None = None, timeout: float = 15.0):
    req = urllib.request.Request(
        url,
        data=json.dumps(body).encode() if body is not None else None,
        headers={"Content-Type": "application/json"} if body is not None else {},
        method=method,
    )
    t0 = time.time()
    try:
        with _OPENER.open(req, timeout=timeout) as r:
            return r.status, r.read().decode("utf-8", "replace"), time.time() - t0
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", "replace"), time.time() - t0
    except Exception as e:  # 连接层异常（拒连/超时）也作为结果回传
        return 0, f"{{\"exception\": \"{e}\"}}", time.time() - t0


def kill_port(port: int) -> None:
    """清掉占用端口的遗留进程（Windows：netstat + taskkill）"""
    if os.name != "nt":
        subprocess.run(["pkill", "-f", f".*{port}.*"], capture_output=True)
        return
    r = subprocess.run(["netstat", "-ano"], capture_output=True, text=True)
    pids: set[str] = set()
    for line in r.stdout.splitlines():
        if f"127.0.0.1:{port} " in line and "LISTENING" in line:
            pid = line.split()[-1]
            if pid.isdigit() and pid != "0":
                pids.add(pid)
    for pid in pids:
        subprocess.run(["taskkill", "/F", "/PID", pid], capture_output=True)
    if pids:
        time.sleep(1)


def _clean_env() -> dict:
    env = dict(os.environ)
    for k in ("HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"):
        env.pop(k, None)
    return env


def start_server(work: Path) -> tuple[subprocess.Popen, Path]:
    """起 server（临时数据目录），cwd=仓根（清单/规则/宪法相对路径）。
    日志落文件而非管道，PIPE 缓冲填满会阻塞整个进程。"""
    env = _clean_env()
    env.update(
        {
            "EVORULE_ADDR": f"127.0.0.1:{SERVER_PORT}",
            "EVORULE_DB_PATH": str(work / "evorule.db"),
            "EVORULE_WORKSPACE_DB": str(work / "workspace.db"),
            "EVORULE_MEMORY_DIR": str(work / "memory"),
            "EVORULE_WAL_DIR": str(work / "wal"),
        }
    )
    log_path = work / "server.log"
    log_f = open(log_path, "w", encoding="utf-8", errors="replace")
    proc = subprocess.Popen(
        [
            str(SERVER_EXE),
            "--insecure-serve",
            "--allow-loopback",
            # 清单装载是显式选择：不传 --plugins 则 external 插件全不挂载
            "--plugins", "plugin_manifest.json",
        ],
        cwd=str(REPO),
        env=env,
        stdout=log_f,
        stderr=subprocess.STDOUT,
    )
    return proc, log_path


def start_host(work: Path) -> subprocess.Popen:
    """起 wasm-host（默认 9140，UDF 目录 = plugins/wasm），日志同样落文件"""
    env = _clean_env()
    env["WASM_HOST_ADDR"] = f"127.0.0.1:{HOST_PORT}"
    env["WASM_HOST_DIR"] = str(REPO / "plugins" / "wasm")
    log_f = open(work / "host.log", "w", encoding="utf-8", errors="replace")
    return subprocess.Popen(
        [str(HOST_EXE)],
        cwd=str(HOST_ROOT),
        env=env,
        stdout=log_f,
        stderr=subprocess.STDOUT,
    )


def wait_health(port: int, path: str = "/health", timeout: float = 90.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with _OPENER.open(f"http://127.0.0.1:{port}{path}", timeout=2) as r:
                if r.status == 200:
                    return True
        except Exception:
            time.sleep(0.3)
    return False


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--threads", type=int, default=16, help="并发线程数")
    ap.add_argument("--iters", type=int, default=8, help="每线程重复调用次数")
    ap.add_argument("--keep", action="store_true", help="保留临时目录（排查用）")
    args = ap.parse_args()

    missing = [
        (p, n)
        for p, n in (
            (SERVER_EXE, "server 二进制（先在 evorule-server/ 内 cargo build）"),
            (HOST_EXE, "wasm-host 二进制（先在 plugins/wasm-host 内 cargo build）"),
            (REPO / "plugin_manifest.json", "plugin_manifest.json"),
            (REPO / "plugins" / "wasm", "plugins/wasm（UDF 目录）"),
        )
        if not p.exists()
    ]
    if missing:
        for p, n in missing:
            print(f"缺失 {n}: {p}")
        return 1

    kill_port(SERVER_PORT)
    kill_port(HOST_PORT)

    work = Path(tempfile.mkdtemp(prefix="evorule-t5-"))
    server: subprocess.Popen | None = None
    host: subprocess.Popen | None = None
    try:
        print(
            f"T5 并发正确性端到端验证（阶段 2 · T5）"
            f"— {args.threads} 路 x {args.iters} 次 = {args.threads * args.iters} 请求"
        )

        host = start_host(work)
        if not wait_health(HOST_PORT, timeout=15):
            check(False, "wasm-host 就绪", "15s 内 /health 未 2xx")
            return 1
        server, _server_log = start_server(work)
        if not wait_health(SERVER_PORT, "/api/health", timeout=90):
            check(False, "evorule-server 就绪", "90s 内 /api/health 未 2xx")
            return 1
        check(True, f"就绪（host 9140 + server {SERVER_PORT}）")

        # ---- 并发压测：每路入参唯一，期望值由线程号派生 ----
        # amount_cents = (tid+1)*10000 保证乘以任意 rate_bp 后被 10000 整除，
        # tax_cents 期望值无取整歧义；rate_bp 各路不同 → 期望值各路唯一，
        # 任一路拿到别路响应即期望不匹配 → 串扰可检出
        results: list[list[str]] = [[] for _ in range(args.threads)]
        barriers: list[threading.Barrier] = []

        def worker(tid: int) -> None:
            amount = (tid + 1) * 10000
            rate = 100 + tid * 7
            expect_tax = (tid + 1) * rate
            expect_total = amount + expect_tax
            body = {"amount_cents": amount, "rate_bp": rate}
            url = f"http://127.0.0.1:{SERVER_PORT}/api/services/{UDF}/invoke"
            errs: list[str] = []
            barrier.wait()  # 所有线程就位后同时放行，最大化并发重叠
            for i in range(args.iters):
                status, resp, _ = http(url, "POST", body)
                if status != 200:
                    errs.append(f"iter{i} status={status} {resp[:80]}")
                    continue
                try:
                    value = json.loads(resp).get("value", {})
                except Exception as e:
                    errs.append(f"iter{i} 解析失败: {e}")
                    continue
                # 无串扰 + 确定性：值必须逐字段严格一致
                if (
                    value.get("amount_cents") != amount
                    or value.get("rate_bp") != rate
                    or value.get("tax_cents") != expect_tax
                    or value.get("total_cents") != expect_total
                ):
                    errs.append(f"iter{i} 期望 tax={expect_tax} total={expect_total}, 实得 {value}")
            results[tid] = errs

        barrier = threading.Barrier(args.threads)
        t0 = time.time()
        threads = [threading.Thread(target=worker, args=(t,)) for t in range(args.threads)]
        for th in threads:
            th.start()
        for th in threads:
            th.join()
        elapsed = time.time() - t0

        bad = [f"路{t}:{results[t][0]}" for t in range(args.threads) if results[t]]
        check(
            not bad,
            f"{args.threads} 路并发 x {args.iters} 次全部正确（无串扰 + 确定性）",
            f"{elapsed:.2f}s, 失败路数 {len(bad)}" + (f" 首错 {bad[0][:160]}" if bad else ""),
        )

        # ---- 并发后存活断言 ----
        print("\n[并发后] 宿主与 server 存活 + 链路仍通")
        try:
            with _OPENER.open(f"http://127.0.0.1:{HOST_PORT}/health", timeout=5) as r:
                host_ok = r.status == 200
        except Exception:
            host_ok = False
        check(host_ok, "并发后 wasm-host /health 仍 200（宿主未被击穿）")

        try:
            with _OPENER.open(
                f"http://127.0.0.1:{SERVER_PORT}/api/health", timeout=5
            ) as r:
                server_ok = r.status == 200
        except Exception:
            server_ok = False
        check(server_ok, "并发后 server /api/health 仍 200")

        status, resp, _ = http(
            f"http://127.0.0.1:{SERVER_PORT}/api/services/{UDF}/invoke",
            "POST",
            {"amount_cents": 100000, "rate_bp": 600},
        )
        ok_final = (
            status == 200
            and json.loads(resp).get("value", {}).get("tax_cents") == 6000
        )
        check(ok_final, "并发后单发一次正常请求仍 200 且值正确", f"status={status}")
    finally:
        if server is not None:
            server.kill()
        if host is not None:
            host.kill()
        if args.keep:
            print(f"\n临时目录保留: {work}")
        else:
            shutil.rmtree(work, ignore_errors=True)

    print()
    if _failures:
        print(f"结果: 失败 {len(_failures)} 项 → {_failures}")
        return 1
    print("结果: 全部通过")
    return 0


if __name__ == "__main__":
    sys.exit(main())

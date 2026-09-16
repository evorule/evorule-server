#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 EvoRule Project
"""T6 敏感服务直调守卫端到端实证（77 号阶段 2 · T6）

临时把 plugin.json 中 udf_finance_tax_calc 的 sensitive 置 true，真实起进程
（wasm-host + evorule-server），断言：

  A. 直调 sensitive=true 服务 → 403 + 「禁止 REST 直调」文案
     —— 敏感守卫拦截直调（直调 = 静默绕过会话审批链）；
  B. GET /api/services 对账清单中该服务 sensitive=true —— 声明透传对账清单；
  C. 对照组：同插件 sensitive=false 的另一服务直调仍 200
     —— 403 确因敏感守卫触发，非链路断裂。

验毕自动**字节级还原** plugin.json（finally 保证，即使中途失败也还原）。

前置：
  ① server 二进制（在 evorule-server crate 内 `cargo build`）；
  ② wasm-host 二进制（在 plugins/wasm-host 内 `cargo build`）；
  ③ plugins/wasm/ 下有示例 .wasm；plugin_manifest.json 中 wasm-host enabled=true。

用法：
    python verify_t6_sensitive_guard.py [--keep]

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
PLUGIN_JSON = HOST_ROOT / "plugin.json"

SERVER_PORT = 18096
HOST_PORT = 9140
UDF_SENSITIVE = "udf_finance_tax_calc"        # 临时置 sensitive=true 的服务
UDF_CONTROL = "udf_finance_tax_calc_as"       # 对照组（保持 sensitive=false）
ARGS = {"amount_cents": 100000, "rate_bp": 600}

_failures: list[str] = []


def check(ok: bool, name: str, detail: str = "") -> None:
    tag = "[PASS]" if ok else "[FAIL]"
    line = f"  {tag} {name}"
    if detail:
        line += f" — {detail}"
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
    try:
        with _OPENER.open(req, timeout=timeout) as r:
            return r.status, r.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", "replace")


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


def start_server(work: Path) -> subprocess.Popen:
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
    log_f = open(work / "server.log", "w", encoding="utf-8", errors="replace")
    return subprocess.Popen(
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


def flip_sensitive_true() -> None:
    """把 UDF_SENSITIVE 的 sensitive 置 true（写入前原始字节已备份）"""
    doc = json.loads(PLUGIN_JSON.read_text(encoding="utf-8"))
    hit = [s for s in doc["services"] if s["name"] == UDF_SENSITIVE]
    if len(hit) != 1:
        raise RuntimeError(f"plugin.json 中 {UDF_SENSITIVE} 声明数 = {len(hit)}，预期 1")
    hit[0]["sensitive"] = True
    PLUGIN_JSON.write_text(
        json.dumps(doc, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--keep", action="store_true", help="保留临时目录（排查用）")
    args = ap.parse_args()

    missing = [
        (p, n)
        for p, n in (
            (SERVER_EXE, "server 二进制（先在 evorule-server/ 内 cargo build）"),
            (HOST_EXE, "wasm-host 二进制（先在 plugins/wasm-host 内 cargo build）"),
            (PLUGIN_JSON, "plugins/wasm-host/plugin.json"),
            (REPO / "plugins" / "wasm", "plugins/wasm（UDF 目录）"),
        )
        if not p.exists()
    ]
    if missing:
        for p, n in missing:
            print(f"缺失 {n}: {p}")
        return 1

    original = PLUGIN_JSON.read_bytes()  # 字节级备份（finally 还原）
    kill_port(SERVER_PORT)
    kill_port(HOST_PORT)

    work = Path(tempfile.mkdtemp(prefix="evorule-t6-"))
    server: subprocess.Popen | None = None
    host: subprocess.Popen | None = None
    try:
        print("T6 敏感服务直调守卫端到端实证（77 号阶段 2 · T6）")

        flip_sensitive_true()
        host = start_host(work)
        if not wait_health(HOST_PORT, timeout=15):
            check(False, "wasm-host 就绪", "15s 内 /health 未 2xx")
            return 1
        server = start_server(work)
        if not wait_health(SERVER_PORT, "/api/health", timeout=90):
            check(False, "evorule-server 就绪", "90s 内 /api/health 未 2xx")
            return 1
        check(True, "就绪（sensitive=true 已临时生效，验毕字节级还原）")

        # ---- A：直调 sensitive 服务 → 403 ----
        status, body = http(
            f"http://127.0.0.1:{SERVER_PORT}/api/services/{UDF_SENSITIVE}/invoke",
            "POST",
            ARGS,
        )
        check(
            status == 403,
            f"直调 {UDF_SENSITIVE}（sensitive=true）→ 403 Forbidden",
            f"status={status}",
        )
        check(
            "禁止 REST 直调" in body,
            "错误文案点名守卫语义（禁止直调，须走会话审计链）",
            body[:140],
        )

        # ---- B：对账清单透传 sensitive 声明 ----
        status, body = http(f"http://127.0.0.1:{SERVER_PORT}/api/services")
        svc = {}
        if status == 200:
            svc = {s["name"]: s for s in json.loads(body)}
        check(
            svc.get(UDF_SENSITIVE, {}).get("sensitive") is True,
            "GET /api/services 对账清单该服务 sensitive=true（声明透传）",
            json.dumps(svc.get(UDF_SENSITIVE, {}), ensure_ascii=False)[:140],
        )

        # ---- C：对照组 —— 非 sensitive 服务直调仍通 ----
        status, body = http(
            f"http://127.0.0.1:{SERVER_PORT}/api/services/{UDF_CONTROL}/invoke",
            "POST",
            ARGS,
        )
        ok_ctrl = (
            status == 200
            and json.loads(body).get("value", {}).get("tax_cents") == 6000
        )
        check(
            ok_ctrl,
            f"对照组 {UDF_CONTROL}（sensitive=false）直调仍 200 且值正确",
            f"status={status}",
        )
    finally:
        if server is not None:
            server.kill()
        if host is not None:
            host.kill()
        PLUGIN_JSON.write_bytes(original)  # 字节级还原（无条件执行）
        restored = PLUGIN_JSON.read_bytes() == original
        print(
            "\nplugin.json 已字节级还原"
            if restored
            else "\n[ERROR] plugin.json 还原校验失败——请手动 git checkout 恢复"
        )
        if args.keep:
            print(f"临时目录保留: {work}")
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

#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 EvoRule Project
"""C6.4 装配可观测端到端实证（GUARD_ASSEMBLED 运行时断言）

真实起进程（evorule-server），断言装配信号在运行时可被外部观测：

  轮 A（首启观测）：
      /api/health 200 且 `guard_assembled` 字段存在、类型为 bool、值 == true；
      server.log 含启动装配日志「入口守卫（PermissionGate）已装配」——
      字段值与实际装配动作相关（信号诚实性）。
  轮 B（重启观测）：换全新数据目录再起一次进程，guard_assembled 再次 == true
      且装配日志再次出现——逐次启动均置位，非一次性/陈旧状态。

说明：`guard_assembled == false` 分支在当前构建**不可达**——装配为无条件
（main 完成全部 I/O 分发路径注入后必调 `mark_guard_assembled`，fail-closed
设计使然），负向（信号存在性与类型）由编译级验证覆盖（见 STATUS C6.4
编译级证据）；本脚本实证正向运行时观测。

前置：server 二进制（在 evorule-server crate 内 `cargo build`）。

用法：
    python verify_c64_guard_assembled.py [--keep]

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
REPO = HERE.parent.parent.parent         # 仓根（workspace 共享 target 所在）
SERVER_EXE = REPO / "target" / "debug" / (
    "evorule-server.exe" if os.name == "nt" else "evorule-server"
)

SERVER_PORT = 18099
GUARD_LOG_MARK = "入口守卫（PermissionGate）已装配"

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


def start_server(work: Path, round_tag: str) -> subprocess.Popen:
    """起 server（全新数据目录），cwd=仓根。日志落文件而非管道，
    PIPE 缓冲填满会阻塞整个进程。"""
    env = _clean_env()
    env.update(
        {
            "EVORULE_ADDR": f"127.0.0.1:{SERVER_PORT}",
            "EVORULE_DB_PATH": str(work / f"evorule-{round_tag}.db"),
            "EVORULE_WORKSPACE_DB": str(work / f"workspace-{round_tag}.db"),
            "EVORULE_MEMORY_DIR": str(work / f"memory-{round_tag}"),
            "EVORULE_WAL_DIR": str(work / f"wal-{round_tag}"),
        }
    )
    log_f = open(work / f"server-{round_tag}.log", "w", encoding="utf-8", errors="replace")
    return subprocess.Popen(
        [str(SERVER_EXE), "--insecure-serve", "--allow-loopback"],
        cwd=str(REPO),
        env=env,
        stdout=log_f,
        stderr=subprocess.STDOUT,
    )


def wait_health(port: int, path: str = "/api/health", timeout: float = 90.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with _OPENER.open(f"http://127.0.0.1:{port}{path}", timeout=2) as r:
                if r.status == 200:
                    return True
        except Exception:
            time.sleep(0.3)
    return False


def assert_round(work: Path, round_tag: str) -> None:
    """单轮断言：health.guard_assembled 存在/bool/true + 启动日志装配行"""
    server = start_server(work, round_tag)
    try:
        if not wait_health(SERVER_PORT, timeout=90):
            check(False, f"[轮 {round_tag}] evorule-server 就绪", "90s 内 /api/health 未 2xx")
            return

        status, body = http(f"http://127.0.0.1:{SERVER_PORT}/api/health")
        health = json.loads(body) if status == 200 else {}

        # 信号存在性 + 类型（运行时版；false 分支编译级覆盖，见模块 docstring）
        has_field = "guard_assembled" in health
        check(has_field, f"[轮 {round_tag}] /api/health 含 guard_assembled 字段（信号存在）")
        check(
            has_field and isinstance(health["guard_assembled"], bool),
            f"[轮 {round_tag}] guard_assembled 类型为 bool（类型保证）",
            f"实际类型={type(health.get('guard_assembled')).__name__}",
        )
        check(
            has_field and health["guard_assembled"] is True,
            f"[轮 {round_tag}] guard_assembled == true（C6.4 核心：运行时观测成立）",
            f"实际值={health.get('guard_assembled')}",
        )

        # 信号诚实性：字段为 true 的同时，启动日志应有真实装配动作留痕
        time.sleep(1.0)  # 等启动日志刷盘
        log_text = (work / f"server-{round_tag}.log").read_text(
            encoding="utf-8", errors="replace"
        )
        check(
            GUARD_LOG_MARK in log_text,
            f"[轮 {round_tag}] server.log 含启动装配日志（信号与实际装配相关）",
        )
    finally:
        server.kill()
        # 等端口释放，供下一轮绑定
        time.sleep(1.0)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--keep", action="store_true", help="保留临时目录（排查用）")
    args = ap.parse_args()

    if not SERVER_EXE.exists():
        print(f"缺失 server 二进制（先在 evorule-server/ 内 cargo build）: {SERVER_EXE}")
        return 1

    kill_port(SERVER_PORT)

    work = Path(tempfile.mkdtemp(prefix="evorule-c64-"))
    try:
        print("C6.4 装配可观测端到端实证（GUARD_ASSEMBLED 运行时断言）")
        print("\n[轮 A] 首启观测：/api/health.guard_assembled == true + 装配日志")
        assert_round(work, "a")
        print("\n[轮 B] 重启观测（全新数据目录）：逐次启动均置位，非陈旧状态")
        assert_round(work, "b")
    finally:
        kill_port(SERVER_PORT)
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

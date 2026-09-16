#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 EvoRule Project
"""V6 离线语义端到端验证（77 号阶段 2 · V6 / T8 探活复用）

全部真实起进程（wasm-host + evorule-server），断言四条：

  A. host 在线（对照组）→ POST /api/services/{udf}/invoke 200 —— 证明链路本身通，
     后续失败确实归因于 host 离线而非链路断裂；
  B. 停掉 host → 同一调用得到 **502 + 「上游连接失败（服务不可达）」**，秒级返回
     —— 非 hang、非 panic、非等待超时（错误脱敏口径，原始细节只进服务端日志）；
  C. 探活翻转 → GET /api/health 的 plugins["wasm-host"].liveness.status = "offline"
     （57 号探活任务自动发现，无需人工干预）；
  D. server 日志出现 plugin_offline 报警（error! 自诊断 + platform.event 入链留痕）。

前置：
  ① server 二进制（在 evorule-server crate 内 `cargo build`）；
  ② plugins/wasm/ 下有示例 .wasm；
  ③ plugin_manifest.json 中 wasm-host enabled=true（仓根默认即满足）。

用法：
    python verify_v6_offline.py [--keep]

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

# 端口取非常用高位，避免与开发实例冲突；9140 是 plugin.json 声明的 base_url 端口，
# host 必须监听它（server 按声明路由），只能先清场再用。
SERVER_PORT = 18098
HOST_PORT = 9140
UDF = "udf_finance_tax_calc"
# 整数定点契约（TCB JsonValue 无 Float）：amount_cents=1000.00 元，rate_bp=6%
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


def http(url: str, method: str = "GET", body: dict | None = None, timeout: float = 10.0):
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
    """起 server（临时数据目录 + 2s 探活周期），cwd=仓根（清单/规则/宪法相对路径）。
    日志落文件而非管道——server 运行期日志量大，PIPE 缓冲填满会阻塞整个进程。"""
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
            "--plugin-probe-interval", "2",
            # 清单装载是显式选择：不传 --plugins 则 external 插件全不挂载
            # （对齐 09-15 实测运行形态），探活任务也不会启动
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

    work = Path(tempfile.mkdtemp(prefix="evorule-v6-"))
    server: subprocess.Popen | None = None
    host: subprocess.Popen | None = None
    server_log: Path | None = None
    try:
        print("V6 离线语义端到端验证（77 号阶段 2 · V6 / T8）")

        # ---- 场景 A：host 在线（对照组）----
        print("\n[场景 A] host 在线 → invoke 直调 200（对照组）")
        host = start_host(work)
        if not wait_health(HOST_PORT, timeout=15):
            check(False, "wasm-host 就绪", "15s 内 /health 未 2xx")
            return 1
        server, server_log = start_server(work)
        if not wait_health(SERVER_PORT, "/api/health", timeout=90):
            check(False, "evorule-server 就绪", "90s 内 /api/health 未 2xx")
            return 1
        check(True, "evorule-server 就绪（临时数据目录 + 2s 探活周期）")

        status, body, elapsed = http(
            f"http://127.0.0.1:{SERVER_PORT}/api/services/{UDF}/invoke", "POST", ARGS
        )
        check(status == 200, "host 在线 → invoke 200", f"status={status} {body[:100]}")

        # ---- 场景 B：停 host → 502 + 明确连接失败语义 ----
        print("\n[场景 B] 停掉 host → invoke 得明确连接失败错误（非 hang/panic）")
        host.kill()
        host.wait(timeout=10)
        host = None
        status, body, elapsed = http(
            f"http://127.0.0.1:{SERVER_PORT}/api/services/{UDF}/invoke", "POST", ARGS
        )
        check(
            status == 502,
            "host 离线 → 502 Bad Gateway（错误脱敏口径）",
            f"status={status}",
        )
        check(
            "连接失败" in body,
            "错误语义 = 连接失败（服务不可达），非笼统 500",
            body[:140],
        )
        check(
            "9140" not in body and "127.0.0.1" not in body,
            "错误响应不泄漏内网拓扑（脱敏纪律）",
            "拓扑零泄漏" if status == 502 else body[:140],
        )
        check(
            elapsed < 10.0,
            "失败即时返回（非 hang / 非等待超时）",
            f"{elapsed:.2f}s（请求超时 10s）",
        )

        # ---- 场景 C：探活翻转 → /api/health plugins 节 offline ----
        # 结构（merge_liveness_into_plugins，平铺进插件节点）：
        # plugins["wasm-host"] = {enabled, external, services, status, last_probe,
        #                         last_ok?, last_error?}
        print("\n[场景 C] /api/health plugins 节如实呈现 offline")
        deadline = time.time() + 30
        node: dict | None = None
        while time.time() < deadline:
            try:
                with _OPENER.open(
                    f"http://127.0.0.1:{SERVER_PORT}/api/health", timeout=3
                ) as r:
                    plugins = json.loads(r.read().decode()).get("plugins", {})
                    lv = plugins.get("wasm-host") or {}
                    if lv.get("status") == "offline":
                        node = lv
                        break
            except Exception:
                pass
            time.sleep(1)
        check(
            node is not None,
            "探活 2 周期内翻转 → plugins[wasm-host].status=offline",
            json.dumps(node, ensure_ascii=False)[:160] if node else "30s 内未翻转",
        )
        if node:
            check(
                "连接失败" in (node.get("last_error") or ""),
                "last_error 点名连接失败",
                (node.get("last_error") or "")[:140],
            )

        # ---- 场景 D：plugin_offline 报警留痕 ----
        # 控制台 = error! 中文自诊断；platform.event.plugin_offline 事件入 WAL
        # （shared_facts.wal，append-only 审计链），不打印事件名到控制台。
        print("\n[场景 D] plugin_offline 报警（error! 自诊断 + platform.event 入链）")
        server.terminate()
        try:
            server.wait(timeout=10)
        except subprocess.TimeoutExpired:
            server.kill()
        server = None
        log = server_log.read_text(encoding="utf-8", errors="replace") if server_log else ""
        wal_path = work / "wal" / "shared_facts.wal"
        wal = wal_path.read_text(encoding="utf-8", errors="replace") if wal_path.exists() else ""
        check(
            "插件离线: wasm-host" in log,
            "error! 自诊断点名 wasm-host（附排障指引）",
            "命中" if "插件离线: wasm-host" in log else "未见自诊断",
        )
        check(
            "plugin_offline" in wal,
            "platform.event.plugin_offline 事件已入 WAL 审计链",
            "命中" if "plugin_offline" in wal else "未见入链事件",
        )
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

#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 EvoRule Project
"""历史批次 auto_discover 端到端实证（WASM 服务自动发现 · 路线 A）

临时改写 plugin.json（auto_discover 开启/策略表形态），真实起进程
（wasm-host + evorule-server），按 设计文档 §四 验收判据分三轮断言：

  轮 A（F5 失败语义）：auto_discover=true + 空 strategy 表 + **不启 host**
      → server 正常启动不崩；增量服务不注册；server.log 有显式「拉取失败」
      告警（不静默）。
  轮 B（F1+F2+S1/S3）：起 host 后重启 server；策略表仅声明
      udf_finance_tax_calc 且 sensitive=true
      → host GET /services 自报实载清单；host /health 对账 200（实载超出
      策略表合法，undeclared 留痕）；server 自动发现合入未声明服务，
      plugin.json 零改动即可直调 200（F1）；策略表 sensitive=true 覆盖
      保留 → 直调 403（F2）。
  轮 C（F3 存量零迁移）：字节级还原 plugin.json（静态声明制、无
      auto_discover）后重启 → 两服务照常注册、直调 200（既有行为逐字节不变）。

验毕自动**字节级还原** plugin.json（finally 保证，即使中途失败也还原）。

前置：
  ① server 二进制（在 evorule-server crate 内 `cargo build`）；
  ② wasm-host 二进制（在 plugins/wasm-host 内 `cargo build`）；
  ③ plugins/wasm/ 下有示例 .wasm；plugin_manifest.json 中 wasm-host enabled=true。

用法：
    python verify_v79_auto_discover.py [--keep]

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

SERVER_PORT = 18097
HOST_PORT = 9140
UDF_POLICY = "udf_finance_tax_calc"        # 轮 B 策略表声明 + sensitive=true（F2）
UDF_DISCOVERED = "udf_finance_tax_calc_as"  # 轮 B 不声明 → 自动发现接管（F1）
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


def start_server(work: Path, round_tag: str) -> subprocess.Popen:
    """起 server（临时数据目录），cwd=仓根（清单/规则/宪法相对路径）。
    日志落文件而非管道，PIPE 缓冲填满会阻塞整个进程。"""
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


def start_host(work: Path, round_tag: str) -> subprocess.Popen:
    """起 wasm-host（默认 9140，UDF 目录 = plugins/wasm），日志同样落文件"""
    env = _clean_env()
    env["WASM_HOST_ADDR"] = f"127.0.0.1:{HOST_PORT}"
    env["WASM_HOST_DIR"] = str(REPO / "plugins" / "wasm")
    log_f = open(work / f"host-{round_tag}.log", "w", encoding="utf-8", errors="replace")
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


def write_plugin_json(doc: dict) -> None:
    PLUGIN_JSON.write_text(
        json.dumps(doc, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )


def ad_doc(doc_version: int, services: list) -> dict:
    """auto_discover 临时 plugin.json（id/contract 与正式清单一致，防 id 漂移拒载）"""
    return {
        "id": "wasm-host",
        "contract_version": "1.2",
        "version": "0.1.0",
        "description": f"历史批次 auto_discover 端到端验证临时清单（轮 {doc_version}，验毕字节级还原）",
        "base_url": f"http://127.0.0.1:{HOST_PORT}",
        "auto_discover": True,
        "services": services,
    }


def service_map() -> dict:
    status, body = http(f"http://127.0.0.1:{SERVER_PORT}/api/services")
    if status != 200:
        return {}
    return {s["name"]: s for s in json.loads(body)}


def invoke(name: str):
    return http(
        f"http://127.0.0.1:{SERVER_PORT}/api/services/{name}/invoke", "POST", ARGS
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

    work = Path(tempfile.mkdtemp(prefix="evorule-v79-"))
    server: subprocess.Popen | None = None
    host: subprocess.Popen | None = None
    try:
        print("历史批次 auto_discover 端到端实证（WASM 服务自动发现）")

        # ============ 轮 A：F5 失败语义（host 未起 + auto_discover 开启）============
        print("\n[轮 A] F5：host 未起时 auto_discover 拉取失败 → 显式告警、不崩、不静默")
        write_plugin_json(ad_doc("A", []))
        server = start_server(work, "a")
        if not wait_health(SERVER_PORT, "/api/health", timeout=90):
            check(False, "evorule-server 就绪（不崩）", "90s 内 /api/health 未 2xx")
            return 1
        check(True, "server 正常启动（发现失败不崩 server）")

        svc = service_map()
        check(
            UDF_DISCOVERED not in svc and UDF_POLICY not in svc,
            "拉取失败 → 自动发现增量不注册（对账清单无 UDF 服务）",
            f"清单服务数={len(svc)}",
        )

        time.sleep(1.0)  # 等启动日志刷盘
        log_text = (work / "server-a.log").read_text(encoding="utf-8", errors="replace")
        check(
            "auto_discover 拉取失败" in log_text,
            "server.log 有显式「auto_discover 拉取失败」告警（不静默）",
        )

        server.kill()
        server = None

        # ============ 轮 B：F1 自动发现 + F2 策略覆盖 + S1/S3 自报与对账 ============
        print("\n[轮 B] F1+F2：auto_discover 开启，策略表仅声明 1 服务且 sensitive=true")
        write_plugin_json(
            ad_doc(
                "B",
                [
                    {
                        "name": UDF_POLICY,
                        "sensitive": True,
                        "description": "历史批次验证：策略表覆盖（sensitive=true 应保留并 403）",
                    }
                ],
            )
        )
        host = start_host(work, "b")
        if not wait_health(HOST_PORT, timeout=15):
            check(False, "wasm-host 就绪", "15s 内 /health 未 2xx")
            return 1

        # S1：host GET /services 实载清单自报
        status, body = http(f"http://127.0.0.1:{HOST_PORT}/services")
        services_doc = {}
        ok_s1 = False
        if status == 200:
            services_doc = json.loads(body)
            names = services_doc.get("services", [])
            ok_s1 = (
                services_doc.get("count") == 2
                and UDF_POLICY in names
                and UDF_DISCOVERED in names
            )
        check(ok_s1, "host GET /services 自报实载清单（2 个 UDF，S1）", body[:120])

        # S3：对账 Ok（实载超出策略表合法，undeclared 留痕）
        status, body = http(f"http://127.0.0.1:{HOST_PORT}/health")
        rec = json.loads(body).get("reconciliation", {}) if status == 200 else {}
        check(
            rec.get("state") == "ok"
            and rec.get("auto_discover") is True
            and UDF_DISCOVERED in rec.get("undeclared", []),
            "host /health 对账 200：实载超出策略表合法 + undeclared 留痕（S3）",
            json.dumps(rec, ensure_ascii=False)[:160],
        )

        server = start_server(work, "b")
        if not wait_health(SERVER_PORT, "/api/health", timeout=90):
            check(False, "evorule-server 就绪", "90s 内 /api/health 未 2xx")
            return 1

        # F1：plugin.json 零改动可调（自动发现合入）
        svc = service_map()
        check(
            UDF_DISCOVERED in svc,
            "未声明的 UDF 被自动发现合入对账清单（F1 身份接管）",
            f"清单含 {UDF_DISCOVERED}: {UDF_DISCOVERED in svc}",
        )
        check(
            svc.get(UDF_POLICY, {}).get("sensitive") is True,
            "策略表 sensitive=true 透传对账清单（F2 前置）",
            json.dumps(svc.get(UDF_POLICY, {}), ensure_ascii=False)[:120],
        )

        status, body = invoke(UDF_DISCOVERED)
        ok_f1 = (
            status == 200
            and json.loads(body).get("value", {}).get("tax_cents") == 6000
        )
        check(
            ok_f1,
            f"直调 {UDF_DISCOVERED}（默认策略）→ 200 且值正确（F1：放 .wasm + 重启即可用）",
            f"status={status}",
        )

        # F2：策略覆盖生效（敏感守卫不被自动化吞掉）
        status, body = invoke(UDF_POLICY)
        check(
            status == 403 and "禁止 REST 直调" in body,
            f"直调 {UDF_POLICY}（策略表 sensitive=true）→ 403（F2 策略覆盖仍生效）",
            f"status={status}",
        )

        server.kill()
        server = None
        host.kill()
        host = None

        # ============ 轮 C：F3 存量零迁移（还原静态声明制）============
        print("\n[轮 C] F3：还原静态声明制 plugin.json → 存量行为不变")
        PLUGIN_JSON.write_bytes(original)
        host = start_host(work, "c")
        if not wait_health(HOST_PORT, timeout=15):
            check(False, "wasm-host 就绪", "15s 内 /health 未 2xx")
            return 1
        status, body = http(f"http://127.0.0.1:{HOST_PORT}/health")
        rec = json.loads(body).get("reconciliation", {}) if status == 200 else {}
        check(
            rec.get("state") == "ok" and rec.get("auto_discover") is False,
            "host /health 对账 200 且 auto_discover=false（还原成功）",
        )
        server = start_server(work, "c")
        if not wait_health(SERVER_PORT, "/api/health", timeout=90):
            check(False, "evorule-server 就绪", "90s 内 /api/health 未 2xx")
            return 1
        svc = service_map()
        # 注：sensitive=false 时对账清单 skip 序列化（键缺失），故按「非 true」判定
        check(
            svc.get(UDF_POLICY, {}).get("sensitive") is not True
            and svc.get(UDF_DISCOVERED, {}).get("sensitive") is not True,
            "两 UDF 按静态声明注册且非敏感（F3 存量行为不变）",
        )
        status, body = invoke(UDF_POLICY)
        check(
            status == 200 and json.loads(body).get("value", {}).get("tax_cents") == 6000,
            f"直调 {UDF_POLICY} → 200（静态制下非敏感可直调）",
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

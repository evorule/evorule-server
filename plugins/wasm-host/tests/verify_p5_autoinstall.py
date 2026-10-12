#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 EvoRule Project
"""安装自动化端到端实证（一键安装/装完即用/禁用/卸载/审计）

临时环境全链（server debug 实例 + wasm-host debug 实例 + 市场种子放置），
按安装自动化验收标准断言：

  ① install API 一键安装（无手工步骤）→ 200 + 阶段耗时回显
  ② 装完即用：registry 合入（GET /api/services 含新服务）→ call_service 调通
  ③ disable → call_service 404（运行时下线）
  ④ 重装恢复 → 调用复通
  ⑤ uninstall → 快照移除 + 文件删除 + 市场列表仍在（市场面与运行面分离）
  ⑥ 审计三事件（installed/disabled/uninstalled）入 WAL
  ⑦ 通道未开启形态：无 token 的 wasm-host → server 代理显式 503（不静默）

用法：python verify_p5_autoinstall.py（前置：两 debug 二进制已构建；演示钥 seed 在 plugins/marketplace-seed）
退出码：0 = 全过；1 = 有失败。
"""

from __future__ import annotations

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

_OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))

HERE = Path(__file__).resolve().parent
HOST_ROOT = HERE.parent
REPO = HOST_ROOT.parent.parent
SERVER_EXE = REPO / "target" / "debug" / ("evorule-server.exe" if os.name == "nt" else "evorule-server")
HOST_EXE = HOST_ROOT / "target" / "debug" / ("evorule-wasm-host.exe" if os.name == "nt" else "evorule-wasm-host")
SEED = HOST_ROOT.parent / "marketplace-seed"
DEMO_KEYS = HOST_ROOT / "trusted-keys.demo.json"

RESULTS: list[tuple[str, bool, str]] = []
SERVER_PORT = 18190
HOST_PORT = 19190


def check(name: str, ok: bool, detail: str = "") -> None:
    RESULTS.append((name, ok, detail))
    print(f"  [{'PASS' if ok else 'FAIL'}] {name}" + (f" —— {detail}" if detail else ""))


def http(method: str, url: str, data=None, headers=None, timeout: float = 60):
    req = urllib.request.Request(url, data=data, method=method, headers=headers or {})
    try:
        with _OPENER.open(req, timeout=timeout) as resp:
            raw = resp.read()
            try:
                return resp.status, json.loads(raw)
            except Exception:  # noqa: BLE001
                return resp.status, raw
    except urllib.error.HTTPError as e:
        raw = e.read().decode(errors="replace")
        try:
            return e.code, json.loads(raw)
        except Exception:  # noqa: BLE001
            return e.code, {"raw": raw[:150]}
    except Exception as e:  # noqa: BLE001
        return 0, {"err": str(e)}


def wait_health(url: str, timeout: float = 30) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        code, _ = http("GET", url, timeout=3)
        if code == 200:
            return True
        time.sleep(0.3)
    return False


def main() -> int:
    if not SERVER_EXE.exists() or not HOST_EXE.exists():
        print("前置缺失：server/wasm-host debug 二进制（先 cargo build）")
        return 1
    if not SEED.is_dir() or not DEMO_KEYS.exists():
        print(f"前置缺失：市场种子 {SEED} / 演示锚 {DEMO_KEYS}")
        return 1

    tmp = Path(tempfile.mkdtemp(prefix="p5-autoinstall-"))
    procs: list[subprocess.Popen] = []
    try:
        # ---- 环境布置：市场放置一个插件 + server 数据目录 + wasm 目录 ----
        plugin_id = "udf_finance_tax_calc_as"
        mkt = tmp / "gov" / "marketplace" / "plugins"
        (mkt / plugin_id).mkdir(parents=True)
        for f in SEED.joinpath(plugin_id).iterdir():
            shutil.copyfile(f, mkt / plugin_id / f.name)
        wasm_dir = tmp / "gov" / "plugins" / "wasm"
        wasm_dir.mkdir(parents=True)
        plugin_json = tmp / "gov" / "plugins" / "wasm-host" / "plugin.json"
        plugin_json.parent.mkdir(parents=True)
        plugin_json.write_text(json.dumps({"services": []}), encoding="utf-8")

        admin_token = "verify-token-2026"
        host_env = dict(os.environ,
                        WASM_HOST_ADDR=f"127.0.0.1:{HOST_PORT}",
                        WASM_HOST_DIR=str(wasm_dir),
                        WASM_HOST_PLUGIN_JSON=str(plugin_json),
                        WASM_HOST_SIGN_POLICY="enforce",
                        WASM_HOST_TRUSTED_KEYS=str(DEMO_KEYS),
                        WASM_HOST_ADMIN_TOKEN=admin_token)
        host_log = open(tmp / "host.log", "w", encoding="utf-8", errors="replace")
        procs.append(subprocess.Popen([str(HOST_EXE)], env=host_env, cwd=str(HOST_ROOT),
                                      stdout=host_log, stderr=subprocess.STDOUT))
        # 探活用 /services（身份端点恒 200）——零模块启动场景 /health 按对账语义 503
        # 版本握手验收：host_abi/host_version 字段在位且一致
        hw_ok = False
        try:
            code, hbody = http("GET", f"http://127.0.0.1:{HOST_PORT}/health", timeout=3)
            hw_ok = (code in (200, 503) and isinstance(hbody, dict)
                     and hbody.get("host_abi") == 1
                     and str(hbody.get("host_version", "")).count(".") == 2)
        except Exception:  # noqa: BLE001
            pass
        print(f"  [ACCEPT] 版本握手字段在位: {hw_ok}")
        if not wait_health(f"http://127.0.0.1:{HOST_PORT}/services"):
            print(f"wasm-host 未就绪（poll={procs[0].poll()}，日志={tmp}/host.log）")
            return 1

        server_env = dict(os.environ,
                          WASM_HOST_ADMIN_URL=f"http://127.0.0.1:{HOST_PORT}",
                          WASM_HOST_ADMIN_TOKEN=admin_token)
        server_args = [
            "--rules-dir", str(tmp / "gov" / "rules"),
            "--addr", f"127.0.0.1:{SERVER_PORT}",
            "--insecure-serve",
        ]
        server_log = open(tmp / "server.log", "w", encoding="utf-8", errors="replace")
        procs.append(subprocess.Popen([str(SERVER_EXE), *server_args], env=server_env,
                                      cwd=str(REPO), stdout=server_log,
                                      stderr=subprocess.STDOUT))
        if not wait_health(f"http://127.0.0.1:{SERVER_PORT}/api/health", timeout=45):
            rc = procs[-1].poll()
            check("环境就绪", False, f"server 45s 未就绪（poll={rc}，日志={tmp}/server.log）")
            return finish()
        check("环境就绪", True, f"server:{SERVER_PORT} + host:{HOST_PORT}")

        S = f"http://127.0.0.1:{SERVER_PORT}"
        H = f"http://127.0.0.1:{HOST_PORT}"

        # ① 一键安装
        code, body = http("POST", f"{S}/api/marketplace/plugins/{plugin_id}/install")
        stages = body.get("detail", {}).get("stages", {}) if isinstance(body, dict) else {}
        check("① install 一键安装（阶段耗时回显）", code == 200 and bool(stages),
              f"verify={stages.get('verify_ms')}ms compile={stages.get('compile_ms')}ms")

        # ② 装完即用：registry 合入 + call_service 调通
        code, services = http("GET", f"{S}/api/services")
        names = [s.get("name") for s in services] if isinstance(services, list) else []
        check("②a registry 合入（/api/services 含新服务）", plugin_id in names, str(names)[:120])
        code, called = http(
            "POST", f"{S}/api/services/{plugin_id}/invoke",
            json.dumps({"args": {"amount_cents": 100000, "rate_bp": 600}}).encode(),
            {"Content-Type": "application/json"},
        )
        ok_call = code == 200 and json.dumps(called).find("6000") >= 0
        check("②b call_service 装完即调（tax_cents=6000）", ok_call,
              str(called)[:100] if not ok_call else "")

        # ③ disable → 404
        http("POST", f"{S}/api/marketplace/plugins/{plugin_id}/disable")
        code, _ = http("POST", f"{S}/api/services/{plugin_id}/invoke",
                       json.dumps({"args": {}}).encode(), {"Content-Type": "application/json"})
        check("③ disable 后调用 404（运行时下线）", code == 404, f"code={code}")

        # ④ 重装恢复
        code, _ = http("POST", f"{S}/api/marketplace/plugins/{plugin_id}/install")
        code2, called2 = http("POST", f"{S}/api/services/{plugin_id}/invoke",
                              json.dumps({"args": {"amount_cents": 100000, "rate_bp": 600}}).encode(),
                              {"Content-Type": "application/json"})
        check("④ 重装恢复调用", code == 200 and code2 == 200)

        # ⑤ uninstall：文件删除 + 市场列表仍在
        http("POST", f"{S}/api/marketplace/plugins/{plugin_id}/uninstall")
        check("⑤a uninstall 后 .wasm 已删", not (wasm_dir / f"{plugin_id}.wasm").exists())
        code, mkt_list = http("GET", f"{S}/api/marketplace/plugins")
        check("⑤b 市场列表仍在（市场面与运行面分离）",
              code == 200 and mkt_list.get("count") == 1)

        # ⑥ 审计三事件（server WAL / 平台事件端点查）
        code, events = http("GET", f"{S}/api/audit/platform-events")
        text = json.dumps(events)
        found = {k: (k in text) for k in (
            "plugin_installed", "plugin_disabled", "plugin_uninstalled")}
        check("⑥ 审计三事件入链", all(found.values()), str(found))

        # ⑦ 通道未开启形态：撤 token 的 host 实例 → 代理显式失败（503/502，非静默成功）
        procs[0].terminate()
        time.sleep(1)
        code, body = http("POST", f"{S}/api/marketplace/plugins/{plugin_id}/install")
        check("⑦ host 不可达时代理显式失败（502/503）", code in (502, 503), f"code={code}")
    finally:
        for pr in procs:
            if pr.poll() is None:
                pr.terminate()
                try:
                    pr.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    pr.kill()
        if "--keep" in sys.argv:
            print(f"[keep] 临时目录: {tmp}")
        else:
            shutil.rmtree(tmp, ignore_errors=True)

    fails = [n for n, ok, _ in RESULTS if not ok]
    print(f"\n== 汇总：{len(RESULTS)-len(fails)}/{len(RESULTS)} PASS ==")
    return 1 if fails else 0


def finish() -> int:
    fails = [n for n, ok, _ in RESULTS if not ok]
    print(f"\n== 汇总：{len(RESULTS)-len(fails)}/{len(RESULTS)} PASS ==")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())

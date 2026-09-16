#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 EvoRule Project
"""WASM 沙箱负面测试（77 号阶段 2 · T4 / V3）

把「零能力」「fuel 中断」「内存上限」从**设计声明**变成**可复现实证**。

断言：
  ① 含 WASI import 的模块 → 宿主加载期拒绝（fail-fast，非运行时才发现）；
     错误信息须显式点名缺失的 import，而不是笼统失败。
  ② 死循环模块 → 被 fuel 中断，返回 422 且错误含 fuel 提示；
     响应耗时远小于超时上限（证明是中断而非等待超时）。
  ③ 中断之后宿主仍 2xx 存活（死循环未拖垮进程，也未泄漏实例状态）。
  ④ 内存超限模块（企图分配 64 MiB > 16 MiB 上限）→ ResourceLimiter 拦截，
     返回 422 结构化错误（alloc 失败语义），宿主存活而非 host OOM。

用法：
    python run_negative_tests.py [--keep]

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

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent  # plugins/wasm-host
EXE = ROOT / "target" / "debug" / (
    "evorule-wasm-host.exe" if os.name == "nt" else "evorule-wasm-host"
)
TARGET = "wasm32-unknown-unknown"

FIXTURES = {
    "negative_wasi_import": HERE / "wasi-import",
    "negative_infinite_loop": HERE / "infinite-loop",
    "negative_memory_hog": HERE / "memory-hog",
}

_failures: list[str] = []


def check(ok: bool, name: str, detail: str = "") -> None:
    tag = "[PASS]" if ok else "[FAIL]"
    line = f"  {tag} {name}"
    if detail:
        line += f" — {detail}"
    print(line, flush=True)
    if not ok:
        _failures.append(name)


def build_fixtures() -> dict[str, Path]:
    """编译两个负面 fixture，返回 服务名 → .wasm 路径"""
    out: dict[str, Path] = {}
    for name, pkg in FIXTURES.items():
        print(f"  编译 fixture {name} ...", flush=True)
        r = subprocess.run(
            ["cargo", "build", "--release", "--target", TARGET],
            cwd=pkg,
            capture_output=True,
            text=True,
        )
        if r.returncode != 0:
            print(r.stderr[-2000:])
            raise SystemExit(f"fixture 编译失败: {name}")
        wasm = (
            pkg
            / "target"
            / TARGET
            / "release"
            / f"{name}.wasm"
        )
        if not wasm.exists():
            raise SystemExit(f"未找到编译产物: {wasm}")
        out[name] = wasm
    return out


def stage(wasm_by_name: dict[str, Path], into: Path, names: list[str]) -> Path:
    into.mkdir(parents=True, exist_ok=True)
    for n in names:
        shutil.copy2(wasm_by_name[n], into / f"{n}.wasm")
    return into


def start_host(wasm_dir: Path, port: int) -> subprocess.Popen:
    env = dict(os.environ)
    env["WASM_HOST_ADDR"] = f"127.0.0.1:{port}"
    env["WASM_HOST_DIR"] = str(wasm_dir)
    env.pop("HTTP_PROXY", None)
    env.pop("HTTPS_PROXY", None)
    env.pop("http_proxy", None)
    env.pop("https_proxy", None)
    return subprocess.Popen(
        [str(EXE)],
        cwd=str(ROOT),
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        errors="replace",
    )


def wait_health(port: int, timeout: float = 15.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with _OPENER.open(f"http://127.0.0.1:{port}/health", timeout=1) as r:
                if r.status == 200:
                    return True
        except Exception:
            time.sleep(0.2)
    return False


def post(port: int, name: str, body: dict, timeout: float) -> tuple[int, str, float]:
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}/services/{name}",
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    t0 = time.time()
    try:
        with _OPENER.open(req, timeout=timeout) as r:
            return r.status, r.read().decode("utf-8", "replace"), time.time() - t0
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", "replace"), time.time() - t0


# --------------------------------------------------------------------------
# 场景 ①：含 WASI import 的模块必须在加载期被拒
# --------------------------------------------------------------------------
def scenario_wasi_import(wasm: dict[str, Path], work: Path) -> None:
    print("\n[场景 ①] 含 WASI import 的模块 → 加载期拒绝")
    d = stage(wasm, work / "wasi", ["negative_wasi_import"])
    proc = start_host(d, 19141)
    try:
        code = proc.wait(timeout=30)
    except subprocess.TimeoutExpired:
        proc.kill()
        check(False, "宿主拒绝启动", "宿主未退出（超时）——模块竟然加载成功了")
        return
    stderr = proc.stderr.read() if proc.stderr else ""
    stdout = proc.stdout.read() if proc.stdout else ""
    blob = stderr + stdout

    check(code != 0, "宿主拒绝启动（fail-fast）", f"exit={code}")
    check(
        "negative_wasi_import" in blob or "加载 UDF" in blob,
        "错误信息点名失败模块",
        blob.strip().splitlines()[-1][:160] if blob.strip() else "(无输出)",
    )
    check(
        "wasi_snapshot_preview1" in blob or "unknown import" in blob.lower()
        or "fd_write" in blob,
        "错误信息点名缺失的 import（可诊断）",
        "wasi_snapshot_preview1 / fd_write" if (
            "wasi_snapshot_preview1" in blob or "fd_write" in blob
        ) else blob.strip().splitlines()[-1][:160] if blob.strip() else "(无输出)",
    )


# --------------------------------------------------------------------------
# 场景 ②③：死循环必须被 fuel 中断，且宿主存活
# --------------------------------------------------------------------------
def scenario_infinite_loop(wasm: dict[str, Path], work: Path) -> None:
    print("\n[场景 ②③] 死循环 UDF → fuel 中断 + 宿主存活")
    d = stage(wasm, work / "loop", ["negative_infinite_loop"])
    proc = start_host(d, 19142)
    try:
        if not wait_health(19142):
            check(False, "宿主就绪", "15s 内 /health 未 2xx（合法模块本应正常加载）")
            return
        check(True, "宿主就绪（合法模块正常加载）")

        status, body, elapsed = post(19142, "negative_infinite_loop", {}, timeout=60)
        check(status == 422, "死循环被中断 → 422", f"status={status}")
        check(
            "fuel" in body.lower(),
            "错误原因含 fuel 提示",
            body[:160],
        )
        # 默认 fuel 预算 1e8，正常机器上应在秒级内耗尽；用 10s 作宽松上界，
        # 关键区别是"中断"（快、有明确原因）而非"等到超时"（60s 无输出）
        check(
            elapsed < 10.0,
            "中断耗时远小于请求超时（非等待超时）",
            f"{elapsed:.2f}s（请求超时 60s）",
        )

        # ② 之后宿主仍存活
        try:
            with _OPENER.open("http://127.0.0.1:19142/health", timeout=3) as r:
                alive = r.status == 200
                info = json.loads(r.read().decode())
        except Exception as e:  # pragma: no cover
            alive, info = False, {"error": str(e)}
        check(alive, "死循环后宿主仍存活", f"/health={info.get('status')}")
    finally:
        proc.kill()
        proc.wait(timeout=10)


# --------------------------------------------------------------------------
# 场景 ④：内存超限必须被 ResourceLimiter 拦截（alloc 失败，非 host OOM）
# --------------------------------------------------------------------------
def scenario_memory_limit(wasm: dict[str, Path], work: Path) -> None:
    print("\n[场景 ④] 内存超限 UDF → alloc 失败（422 结构化错误），宿主存活")
    d = stage(wasm, work / "mem", ["negative_memory_hog"])
    proc = start_host(d, 19143)
    try:
        if not wait_health(19143):
            check(False, "宿主就绪", "15s 内 /health 未 2xx（合法模块本应正常加载）")
            return
        check(True, "宿主就绪（合法模块正常加载）")

        status, body, elapsed = post(19143, "negative_memory_hog", {}, timeout=60)
        check(
            status == 422,
            "内存超限 → 422 结构化错误（非 500/连接断/挂起）",
            f"status={status} body={body[:120]}",
        )
        blob = body.lower()
        check(
            "unreachable" in blob or "memory" in blob or "trap" in blob or "alloc" in blob,
            "错误语义可诊断（trap/alloc/memory）",
            body[:160],
        )
        check(
            elapsed < 10.0,
            "失败即时返回（非等待超时）",
            f"{elapsed:.2f}s（请求超时 60s）",
        )

        # ④ 之后宿主仍存活 —— 64 MiB 的请求没有真实吃进宿主内存
        try:
            with _OPENER.open("http://127.0.0.1:19143/health", timeout=3) as r:
                alive = r.status == 200
                info = json.loads(r.read().decode())
        except Exception as e:  # pragma: no cover
            alive, info = False, {"error": str(e)}
        check(alive, "内存超限后宿主仍存活（非 host OOM）", f"/health={info.get('status')}")
    finally:
        proc.kill()
        proc.wait(timeout=10)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--keep", action="store_true", help="保留临时目录（排查用）")
    args = ap.parse_args()

    if not EXE.exists():
        print(f"宿主二进制缺失: {EXE}\n先执行: cargo build（在 plugins/wasm-host）")
        return 1

    print("WASM 沙箱负面测试（77 号阶段 2 · T4 / V3）")
    wasm = build_fixtures()

    work = Path(tempfile.mkdtemp(prefix="evorule-wasm-negative-"))
    try:
        scenario_wasi_import(wasm, work)
        scenario_infinite_loop(wasm, work)
        scenario_memory_limit(wasm, work)
    finally:
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

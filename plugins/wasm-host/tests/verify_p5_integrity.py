#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 EvoRule Project
"""分发完整性端到端实证（per-module 清单 + blake3 + ed25519 分离签名）

全程在**临时目录**进行（复制一个在役 .wasm 作 fixture），零现网污染、零 server 依赖。
按立项方案验收判据分三轮：

  轮 A（CLI 签发 + 离线验证）：keygen → sign → verify(enforce) 全过；
      篡改 .wasm 单字节 → verify FAIL（内容哈希不符）；
      篡改清单 → verify FAIL（验签失败）；非信任锚公钥 → FAIL（未知来源）。
  轮 B（运行时策略矩阵，真实起 host 进程）：
      B1 enforce + 三件套齐全   → 启动成功，/health integrity=verified；
      B2 enforce + 缺签名       → 启动失败（fail-fast，stderr 点名"无分发清单与签名"）；
      B3 enforce + 坏签名       → 启动失败（验签失败拒载）；
      B4 warn   + 无材料        → 启动成功，integrity=accepted_unverified；
      B5 warn   + 坏签名        → 启动失败（材料存在即全量校验——篡改信号不因 warn 放行）；
      B6 warn   + 齐全但信任锚未知 key_id → 启动失败（拒绝未知来源）。
  轮 C（off 回归）：策略 off + 无材料 → 启动成功（等价既有行为），integrity=unchecked。

用法：
    python verify_p5_integrity.py [--keep]
前置：wasm-host 二进制（plugins/wasm-host 内 cargo build）+ plugins/wasm/ 下任一在役 .wasm。
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
HOST_ROOT = HERE.parent                  # plugins/wasm-host
HOST_EXE = HOST_ROOT / "target" / "debug" / (
    "evorule-wasm-host.exe" if os.name == "nt" else "evorule-wasm-host"
)
# 在役最小 .wasm（AssemblyScript 示例 2.4KB）作 fixture 源
FIXTURE_SOURCES = [
    Path(__import__("os").environ.get("WASM_HOST_DIR", HOST_ROOT.parent / "wasm")),
]

RESULTS: list[tuple[str, bool, str]] = []


def check(name: str, ok: bool, detail: str = "") -> None:
    RESULTS.append((name, ok, detail))
    print(f"  [{'PASS' if ok else 'FAIL'}] {name}" + (f" —— {detail}" if detail else ""))


def http_get(url: str, timeout: float = 5.0):
    req = urllib.request.Request(url)
    try:
        with _OPENER.open(req, timeout=timeout) as resp:
            return resp.status, json.loads(resp.read().decode("utf-8", errors="replace"))
    except urllib.error.HTTPError as e:
        return e.code, {}
    except Exception as e:  # noqa: BLE001
        return 0, {"err": str(e)}


def run_cli(args: list[str], env_extra: dict[str, str] | None = None):
    env = dict(os.environ)
    if env_extra:
        env.update(env_extra)
    return subprocess.run(
        [str(HOST_EXE), *args], capture_output=True, text=True, timeout=60, env=env
    )


def try_start_host(wasm_dir: Path, policy: str, keys: Path | None, port: str):
    """起 host 进程（临时端口），返回 (proc, probe)——probe=None 表示进程启动即退（fail-fast）。"""
    env = dict(os.environ)
    env.update({
        "WASM_HOST_ADDR": f"127.0.0.1:{port}",
        "WASM_HOST_DIR": str(wasm_dir),
        "WASM_HOST_SIGN_POLICY": policy,
        "RUST_LOG": "info",
    })
    if keys is not None:
        env["WASM_HOST_TRUSTED_KEYS"] = str(keys)
    else:
        env.pop("WASM_HOST_TRUSTED_KEYS", None)
    proc = subprocess.Popen(
        [str(HOST_EXE)],
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        errors="replace",
        cwd=str(HOST_ROOT),
    )
    # 起进程探测：1.2s 内端口通 = 服务模式成功；进程退出 = fail-fast
    for _ in range(24):
        if proc.poll() is not None:
            return proc, None
        code, _ = http_get(f"http://127.0.0.1:{port}/health", timeout=1.0)
        if code == 200:
            return proc, True
        time.sleep(0.05)
    return proc, None


def stop(proc: subprocess.Popen) -> str:
    if proc.poll() is None:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=5)
    err = ""
    if proc.stderr is not None:
        try:
            err = proc.stderr.read()
        except Exception:  # noqa: BLE001
            err = ""
    return err


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--keep", action="store_true", help="保留临时目录（排查用）")
    args = parser.parse_args()

    if not HOST_EXE.exists():
        print(f"未找到 wasm-host 二进制: {HOST_EXE}（先 cargo build）")
        return 1

    fixture = None
    for base in FIXTURE_SOURCES:
        if base.is_dir():
            candidates = sorted(base.glob("*.wasm"), key=lambda p: p.stat().st_size)
            if candidates:
                fixture = candidates[0]
                break
    if fixture is None:
        print("plugins/wasm/ 下无在役 .wasm 可作 fixture")
        return 1
    print(f"fixture: {fixture.name}（{fixture.stat().st_size} 字节）\n")

    tmp = Path(tempfile.mkdtemp(prefix="p5-integrity-"))
    print(f"临时目录: {tmp}\n")
    try:
        wasm_dir = tmp / "wasm"
        wasm_dir.mkdir()
        demo = wasm_dir / "demo.wasm"
        shutil.copyfile(fixture, demo)

        # ---------- 轮 A：CLI 签发 + 离线验证 ----------
        print("== 轮 A：CLI 签发 + 离线验证 ==")
        r = run_cli(["keygen"])
        lines = r.stdout.split()
        seed = lines[1] if len(lines) > 1 else ""
        pk = lines[-1] if lines else ""
        check("A1 keygen 产出种子与公钥", r.returncode == 0 and len(seed) == 64 and len(pk) == 64)

        other = run_cli(["keygen"])
        other_pk = other.stdout.split()[-1]

        keys = tmp / "trusted-keys.json"
        keys.write_text(json.dumps({
            "keys": [
                {"key_id": "test-official", "public_key": pk, "note": "轮 A/B 正锚", "enabled": True},
                {"key_id": "test-disabled", "public_key": other_pk, "note": "禁用锚", "enabled": False},
            ]
        }), encoding="utf-8")

        r = run_cli(["sign", "--wasm", str(demo), "--version", "1.0.0",
                     "--key", seed, "--key-id", "test-official", "--notes", "p5 e2e"])
        check("A2 sign 产出清单+签名", r.returncode == 0 and (wasm_dir / "demo.plugin-module.json").exists()
              and (wasm_dir / "demo.wasm.sig").exists(), r.stdout.strip() or r.stderr.strip())

        r = run_cli(["verify", "--dir", str(wasm_dir), "--keys", str(keys), "--policy", "enforce"])
        check("A3 verify(enforce) 全过", r.returncode == 0 and "PASS demo" in r.stdout)

        # 篡改 .wasm 单字节 → 内容哈希不符
        raw = bytearray(demo.read_bytes())
        raw[-1] ^= 0xFF
        demo.write_bytes(bytes(raw))
        r = run_cli(["verify", "--dir", str(wasm_dir), "--keys", str(keys), "--policy", "enforce"])
        check("A4 篡改 .wasm → FAIL（哈希不符）", r.returncode == 1 and "FAIL demo" in r.stdout)
        r2 = run_cli(["sign", "--wasm", str(demo), "--version", "1.0.0",
                      "--key", seed, "--key-id", "test-official"])
        check("A5 重签后恢复一致", r2.returncode == 0)

        # 篡改清单（换版本字段）→ 验签失败
        manifest = json.loads((wasm_dir / "demo.plugin-module.json").read_text(encoding="utf-8"))
        manifest["version"] = "9.9.9"
        (wasm_dir / "demo.plugin-module.json").write_text(
            json.dumps(manifest, indent=2), encoding="utf-8")
        r = run_cli(["verify", "--dir", str(wasm_dir), "--keys", str(keys), "--policy", "enforce"])
        check("A6 篡改清单 → FAIL（验签失败）", r.returncode == 1 and "FAIL demo" in r.stdout)
        # 还原清单（重签覆盖）
        run_cli(["sign", "--wasm", str(demo), "--version", "1.0.0",
                 "--key", seed, "--key-id", "test-official"])

        # 非信任锚 key_id
        run_cli(["sign", "--wasm", str(demo), "--version", "1.0.0",
                 "--key", other.stdout.split()[1], "--key-id", "unknown-key"])
        r = run_cli(["verify", "--dir", str(wasm_dir), "--keys", str(keys), "--policy", "enforce"])
        check("A7 未知来源签名 → FAIL", r.returncode == 1 and "FAIL demo" in r.stdout)
        run_cli(["sign", "--wasm", str(demo), "--version", "1.0.0",
                 "--key", seed, "--key-id", "test-official"])

        # ---------- 轮 B：运行时策略矩阵 ----------
        print("\n== 轮 B：运行时策略矩阵（真实起进程） ==")
        port = "19146"

        proc, up = try_start_host(wasm_dir, "enforce", keys, port)
        body = http_get(f"http://127.0.0.1:{port}/health")[1] if up else {}
        state = (body.get("integrity") or {}).get("demo", {}).get("state")
        check("B1 enforce+三件套 → 启动成功 integrity=verified",
              proc.poll() is None and up is not None and state == "verified", f"state={state}")
        stop(proc)

        (wasm_dir / "demo.wasm.sig").unlink()
        proc, up = try_start_host(wasm_dir, "enforce", keys, port)
        err = stop(proc)
        check("B2 enforce+缺签名 → 拒启动", up is None and "有清单无签名" in (err or ""),
              (err or "").strip().splitlines()[-1] if err else "")
        run_cli(["sign", "--wasm", str(demo), "--version", "1.0.0",
                 "--key", seed, "--key-id", "test-official"])

        sig_doc = json.loads((wasm_dir / "demo.wasm.sig").read_text(encoding="utf-8"))
        flipped = list(sig_doc["signature"])
        flipped[0] = "e" if flipped[0] != "e" else "f"
        sig_doc["signature"] = "".join(flipped)
        (wasm_dir / "demo.wasm.sig").write_text(json.dumps(sig_doc, indent=2), encoding="utf-8")
        proc, up = try_start_host(wasm_dir, "enforce", keys, port)
        err = stop(proc)
        check("B3 enforce+坏签名 → 拒启动", up is None, (err or "").strip().splitlines()[-1] if err else "")
        run_cli(["sign", "--wasm", str(demo), "--version", "1.0.0",
                 "--key", seed, "--key-id", "test-official"])

        (wasm_dir / "demo.plugin-module.json").unlink()
        (wasm_dir / "demo.wasm.sig").unlink()
        proc, up = try_start_host(wasm_dir, "warn", keys, port)
        body = http_get(f"http://127.0.0.1:{port}/health")[1] if up else {}
        state = (body.get("integrity") or {}).get("demo", {}).get("state")
        check("B4 warn+无材料 → 放行 integrity=accepted_unverified",
              proc.poll() is None and up is not None and state == "accepted_unverified",
              f"state={state}")
        stop(proc)

        (wasm_dir / "demo.plugin-module.json").write_text(
            json.dumps({"module": "demo", "version": "1.0.0", "abi": 1, "blake3": "blake3:00",
                        "purity": "zero-import"}, indent=2), encoding="utf-8")
        sig_doc["key_id"] = "test-official"
        (wasm_dir / "demo.wasm.sig").write_text(json.dumps(sig_doc, indent=2), encoding="utf-8")
        proc, up = try_start_host(wasm_dir, "warn", keys, port)
        err = stop(proc)
        check("B5 warn+坏材料 → 拒启动（存在即全量校验）",
              up is None and ("不符" in (err or "") or "验签失败" in (err or "")),
              (err or "").strip().splitlines()[-1] if err else "")
        (wasm_dir / "demo.plugin-module.json").unlink()
        (wasm_dir / "demo.wasm.sig").unlink()

        # 未知 key_id 的完整三件套（B6）
        run_cli(["sign", "--wasm", str(demo), "--version", "1.0.0",
                 "--key", other.stdout.split()[1], "--key-id", "ghost-key"])
        proc, up = try_start_host(wasm_dir, "warn", keys, port)
        err = stop(proc)
        check("B6 warn+未知 key_id → 拒启动", up is None and "不在信任锚" in (err or ""),
              (err or "").strip().splitlines()[-1] if err else "")
        run_cli(["sign", "--wasm", str(demo), "--version", "1.0.0",
                 "--key", seed, "--key-id", "test-official"])

        # 禁用锚（enabled=false）视同未知
        keys_disabled = tmp / "trusted-disabled.json"
        keys_disabled.write_text(json.dumps({
            "keys": [{"key_id": "test-official", "public_key": pk, "enabled": False}]
        }), encoding="utf-8")
        proc, up = try_start_host(wasm_dir, "warn", keys_disabled, port)
        err = stop(proc)
        check("B7 禁用锚 → 拒启动", up is None and "不在信任锚" in (err or ""),
              (err or "").strip().splitlines()[-1] if err else "")

        # ---------- 轮 C：off 回归 ----------
        print("\n== 轮 C：off 回归（等价既有行为） ==")
        for p in (wasm_dir / "demo.plugin-module.json", wasm_dir / "demo.wasm.sig"):
            if p.exists():
                p.unlink()
        proc, up = try_start_host(wasm_dir, "off", None, port)
        body = http_get(f"http://127.0.0.1:{port}/health")[1] if up else {}
        state = (body.get("integrity") or {}).get("demo", {}).get("state")
        check("C1 off+无材料 → 放行 integrity=unchecked",
              proc.poll() is None and up is not None and state == "unchecked", f"state={state}")
        stop(proc)
    finally:
        if args.keep:
            print(f"\n[keep] 临时目录保留: {tmp}")
        else:
            shutil.rmtree(tmp, ignore_errors=True)

    fails = [n for n, ok, _ in RESULTS if not ok]
    print(f"\n== 汇总：{len(RESULTS) - len(fails)}/{len(RESULTS)} PASS ==")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())

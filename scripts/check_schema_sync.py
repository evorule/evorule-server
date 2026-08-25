#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 EvoRule Project
# This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
"""
evorule-server 内嵌 Schema 与 evorule-system-rules 源仓的一致性守卫
====================================================================

evorule-system-rules 是 Schema 的单一事实来源（SSOT）。evorule-server 在
`core/rule_schema/schemas/` 内嵌三份副本供运行时 jsonschema 校验使用。
本脚本守护两份副本与源仓逐字节一致，防止「改了源 schema 但没同步 server」的漂移。

用法:
    python check_schema_sync.py [--source DIR] [--sync] [--json]

参数:
    --source DIR  evorule-system-rules 仓根目录
                 （默认: 自动探测 ../evorule-system-rules，或环境变量 EVORULE_RULES_SRC）
    --sync       把源仓 schema 拷贝覆盖到本仓（维护操作用，需人工确认）
    --json       以 JSON 输出

退出码:
    0 = 一致（或 --sync 后一致）
    1 = 存在漂移
    2 = 脚本自身错误（源仓不存在等）

校验的三份文件:
    schemas/rule_set/v1.0.json
    schemas/_shared/v1.0.json
    schemas/_meta/v1.0.json
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import sys
from pathlib import Path

# 需要同步的文件（相对各自仓根的路径）
PAIRS = [
    ("schemas/rule_set/v1.0.json", "core/rule_schema/schemas/rule_set/v1.0.json"),
    ("schemas/_shared/v1.0.json", "core/rule_schema/schemas/_shared/v1.0.json"),
    ("schemas/_meta/v1.0.json", "core/rule_schema/schemas/_meta/v1.0.json"),
]


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(65536), b""):
            h.update(chunk)
    return h.hexdigest()


def locate_source(explicit: str | None) -> Path | None:
    """定位 evorule-system-rules 源仓根目录"""
    candidates = []
    if explicit:
        candidates.append(Path(explicit))
    env = os.environ.get("EVORULE_RULES_SRC")
    if env:
        candidates.append(Path(env))
    # 本仓的兄弟目录（本脚本在 evorule-server/scripts 下）
    this_server_root = Path(__file__).resolve().parent.parent
    candidates.append(this_server_root.parent / "evorule-system-rules")

    for c in candidates:
        if (c / "schemas" / "rule_set" / "v1.0.json").exists():
            return c
    return None


def main() -> int:
    parser = argparse.ArgumentParser(description="Schema 跨仓一致性守卫")
    parser.add_argument("--source", default=None, help="evorule-system-rules 仓根目录")
    parser.add_argument("--sync", action="store_true", help="把源仓 schema 拷贝覆盖到本仓")
    parser.add_argument("--json", action="store_true", dest="as_json", help="以 JSON 输出")
    args = parser.parse_args()

    server_root = Path(__file__).resolve().parent.parent
    source_root = locate_source(args.source)
    if source_root is None:
        print("[ERROR] 找不到 evorule-system-rules 源仓。用 --source 指定，或设置 EVORULE_RULES_SRC", file=sys.stderr)
        return 2

    results = []
    all_ok = True
    for src_rel, dst_rel in PAIRS:
        src = source_root / src_rel
        dst = server_root / dst_rel
        if not src.exists():
            print(f"[ERROR] 源仓缺少 {src_rel}: {src}", file=sys.stderr)
            return 2
        if not dst.exists():
            results.append({"file": src_rel, "status": "missing", "src_sha": sha256(src), "dst_sha": None})
            all_ok = False
            continue
        s1, s2 = sha256(src), sha256(dst)
        if s1 != s2:
            results.append({"file": src_rel, "status": "drift", "src_sha": s1, "dst_sha": s2})
            all_ok = False
        else:
            results.append({"file": src_rel, "status": "ok", "src_sha": s1, "dst_sha": s2})

    if args.sync:
        if not all_ok:
            for r in results:
                if r["status"] != "ok":
                    src_rel, dst_rel = PAIRS[[x["file"] for x in results].index(r["file"])]
                    src, dst = source_root / src_rel, server_root / dst_rel
                    dst.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copyfile(src, dst)
                    r["status"] = "synced"
            all_ok = True
            print(f"[SYNC] 已从 {source_root} 同步到 {server_root}")
        else:
            print("[SYNC] 已一致，无需同步")

    if args.as_json:
        print(json.dumps({
            "source": str(source_root),
            "consistent": all_ok,
            "files": results,
        }, ensure_ascii=False, indent=2))
    else:
        for r in results:
            mark = {"ok": "[OK]", "drift": "[DRIFT]", "missing": "[MISSING]", "synced": "[SYNCED]"}[r["status"]]
            print(f"  {mark} {r['file']}")
            if r["status"] == "drift":
                print(f"        src={r['src_sha'][:12]}  dst={r['dst_sha'][:12]}")
        if all_ok:
            print(f"ALL SCHEMA FILES CONSISTENT with {source_root}")
        else:
            print(f"[FAIL] Schema 漂移: 请用 python scripts/check_schema_sync.py --sync 同步")

    return 0 if all_ok else 1


if __name__ == "__main__":
    sys.exit(main())

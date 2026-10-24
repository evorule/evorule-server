#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 EvoRule Project Authors
"""evorule hash-bypass scanner (v1.0, 2026-10-24).

哈希口径纪律（B 族）：BLAKE3 内容哈希只准经 evorule-hash 单一真相源，
禁止任何旁路。本扫描器拦截两类绕行：
  H1  任意 Cargo.toml 中直连 blake3 依赖行（含 [patch] 换源写法）
  H2  .rs 源码中的 `use blake3` / `blake3::` 引用
仅标准库。棘轮语义与 public_face_scan.py 同型：
  hash_bypass_scan.py                    全量扫描；有命中即 exit 1
  hash_bypass_scan.py --dump             打印当前命中（baseline 行）
  hash_bypass_scan.py --ratchet FILE     命中未入 baseline 即 exit 1
"""
import hashlib
import os
import re
import sys

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

SELF_EXEMPT = {
    os.path.join("scripts", "hash_bypass_scan.py"),
    os.path.join("scripts", "hash-bypass-baseline.txt"),
}

SKIP_DIRS = {".git", "node_modules", "__pycache__", "plan", ".codegraph", ".obsidian"}

# (pattern_id, filename matcher, per-line regex)
RULES = [
    ("H1", lambda name: name == "Cargo.toml", re.compile(r"^\s*blake3\s*=")),
    ("H2", lambda name: name.endswith(".rs"), re.compile(r"\buse\s+blake3\b|\bblake3\s*::")),
]


def scan_tree():
    """Yield baseline-form lines: pid|relpath|md5(line)."""
    hits = []
    for dirpath, dirnames, filenames in os.walk(REPO_ROOT):
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS and not d.startswith("target")]
        for name in filenames:
            full = os.path.join(dirpath, name)
            rel = os.path.relpath(full, REPO_ROOT).replace(os.sep, "/")
            if rel in SELF_EXEMPT:
                continue
            for pid, matcher, rx in RULES:
                if not matcher(name):
                    continue
                try:
                    with open(full, "r", encoding="utf-8", errors="replace") as f:
                        for line in f:
                            if rx.search(line):
                                digest = hashlib.md5(line.strip().encode("utf-8", "replace")).hexdigest()
                                hits.append("%s|%s|%s" % (pid, rel, digest))
                except OSError:
                    continue
    return sorted(set(hits))


def load_baseline(path):
    if not os.path.exists(path):
        return set()
    with open(path, "r", encoding="utf-8") as f:
        return {ln.strip() for ln in f if ln.strip() and not ln.startswith("#")}


def main(argv):
    hits = scan_tree()
    if "--dump" in argv:
        for ln in hits:
            print(ln)
        return 0
    if "--ratchet" in argv:
        i = argv.index("--ratchet")
        base = load_baseline(os.path.join(REPO_ROOT, argv[i + 1]))
        new = [h for h in hits if h not in base]
        stale = [h for h in sorted(base) if h not in set(hits)]
        if new:
            print("NEW hash-bypass violations (not in baseline):")
            for h in new:
                print("  " + h)
            print("Fix them, or shrink-then-freeze via --dump if intentional.")
            return 1
        if stale:
            print("info: %d baseline entries no longer present; consider shrinking baseline." % len(stale))
        return 0
    if hits:
        print("hash-bypass violations: %d" % len(hits))
        for h in hits:
            print("  " + h)
        return 1
    print("hash-bypass: clean")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

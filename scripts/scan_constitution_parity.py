#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# =============================================================================
# scan_constitution_parity.py — 宪法语义手写提取禁令扫描器（[CON-L4-001]/[CON-L4-002]）
#
# 定位：消灭「第二装载路径」的类别级常设手段。扫描各仓 *.rs 中绕开
#       rule_loader / rule_schema API 的手写宪法语义实现：
#         P1 transform 直取：`get("transform")` 剥壳语义（唯一合法点=rule_loader）
#         P2 指令形态白名单：`== "command"` 等硬编码判型（须引宪法 $def 常量）
#         P3 手写 serde_to_tcb：第二份转换实现（唯一实现=rule_loader::serde_to_tcb）
#
# 豁免纪律：命中默认 exit 1；确属合法者入 allowed_exceptions.conf
#           （每条含路径:行号锚+理由+责任人+复审期），禁止注释关闭。
#
# 用法：
#   python scripts/scan_constitution_parity.py            # 扫本仓
#   python scripts/scan_constitution_parity.py --root P   # 扫指定仓
#
# 退出码：0=绿灯（含豁免后零命中）；1=存在未豁免命中；2=环境错误
# =============================================================================

import argparse
import re
import sys
from pathlib import Path

# ── 模式表（可扩展；每条含 id/正则/说明）──
PATTERNS = [
    {
        "id": "P1-transform-direct",
        "regex": re.compile(r'\.get\(\s*"transform"\s*\)'),
        "desc": "transform 直取（剥壳语义绕开 rule_loader 唯一装载通道）",
    },
    {
        "id": "P2-instruction-whitelist",
        "regex": re.compile(r'==\s*"command"|type\s*==\s*"(set|branch|push|io_request|enforce)"'),
        "desc": "硬编码指令形态白名单判型（须引宪法 $def 常量或 rule_schema API）",
    },
    {
        "id": "P3-serde-to-tcb-rewrite",
        "regex": re.compile(r'fn\s+serde_to_tcb\s*\('),
        "desc": "手写第二份 serde_to_tcb（唯一实现=rule_loader；use 导入不受限）",
    },
]

# 扫描根（默认脚本所在仓）
DEFAULT_ROOT = Path(__file__).resolve().parent.parent
EXEMPT_FILE = DEFAULT_ROOT / "scripts" / "allowed_exceptions.conf"

# 结构性豁免（扫描器内置：唯一实现点与测试面天然合法，无需逐条登记）
STRUCTURAL_EXEMPTS = {
    # P1：唯一装载通道 crate 自身（剥壳语义的正主）
    "core/rule_loader/src/lib.rs": {"P1-transform-direct", "P3-serde-to-tcb-rewrite"},
    # P2：tier 门 enforce 限定是治理门语义（宪法 tier_gate 条文自带），非白名单判型
    "core/rule_loader/src/lib.rs#P2": set(),
}

SKIP_DIRS = {"target", "node_modules", ".git", "patches"}


def load_exemptions():
    """解析 allowed_exceptions.conf：path:line 锚 或 path 通配；# 开头为注释"""
    exemptions = []  # [(path_substr, pattern_id, reason)]
    if not EXEMPT_FILE.exists():
        return exemptions
    for raw in EXEMPT_FILE.read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        # 格式: <path-substr> | <pattern-id|*> | <理由+责任人+复审期>
        parts = [p.strip() for p in line.split("|")]
        if len(parts) >= 3:
            exemptions.append((parts[0], parts[1], "|".join(parts[2:])))
    return exemptions


def is_exempt(rel_path, pattern_id, line_no, exemptions):
    # 结构性豁免（文件级）
    for key, pats in STRUCTURAL_EXEMPTS.items():
        if key.endswith("#P2"):
            continue
        if rel_path.replace("\\", "/").endswith(key) and (
            pattern_id in pats or "*" in pats
        ):
            return f"structural:{key}"
    # 显式豁免清单（路径子串+模式匹配；行级锚优先精确）
    for path_sub, pat, reason in exemptions:
        if path_sub in rel_path.replace("\\", "/") and pat in (pattern_id, "*"):
            return f"conf:{reason[:60]}"
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", default=str(DEFAULT_ROOT))
    args = ap.parse_args()
    root = Path(args.root)

    if not root.exists():
        print(f"[ENV-ERROR] root not found: {root}", file=sys.stderr)
        return 2

    exemptions = load_exemptions()
    hits = []
    scanned = 0
    for rs_file in root.rglob("*.rs"):
        rel = str(rs_file.relative_to(root))
        if any(skip in rs_file.parts for skip in SKIP_DIRS):
            continue
        scanned += 1
        try:
            text = rs_file.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        for pat in PATTERNS:
            for m in pat["regex"].finditer(text):
                line_no = text.count("\n", 0, m.start()) + 1
                # 取该行文本判断是否注释（粗判：行首Strip后以 // 或 /// 开头 → 文档注释不算）
                line_text = text.splitlines()[line_no - 1].strip()
                if line_text.startswith("//"):
                    continue
                ex = is_exempt(rel, pat["id"], line_no, exemptions)
                hits.append((rel, line_no, pat["id"], pat["desc"], ex))

    unexempted = [h for h in hits if h[4] is None]
    exempted = [h for h in hits if h[4] is not None]

    print(f"=== constitution-parity scan: {scanned} files scanned ===")
    for rel, ln, pid, desc, ex in exempted:
        print(f"[EXEMPT] {rel}:{ln} {pid} ({ex})")
    for rel, ln, pid, desc, _ in unexempted:
        print(f"[HIT]    {rel}:{ln} {pid} — {desc}")

    if unexempted:
        print(f"\nFAIL: {len(unexempted)} unexempted hit(s); "
              f"{len(exempted)} exempted. Fix or register in allowed_exceptions.conf")
        return 1
    print(f"\nOK: 0 unexempted hits ({len(exempted)} exempted)")
    return 0


if __name__ == "__main__":
    sys.exit(main())

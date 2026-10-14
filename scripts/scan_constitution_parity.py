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
# 豁免纪律：命中默认 exit 1；确属合法者入 allowed_exceptions.conf 以
#           **行锚定**（path:lineStart-lineEnd 或 path:line）登记——行级锚保证
#           只放行已审查的具体行，文件未来新增漂移照样 HIT（红灯演练实证：
#           文件级豁免曾把注入漂移静默吞掉，2026-10-14 修复为行锚定）；
#           禁止注释关闭。条目格式：
#             <path:line[-lineEnd]> | <pattern-id|*> | <理由 | 责任人 | 复审期>
#           行锚漂移容忍：锚行±0 精确；锚失配（如重构移行）= 退化为 HIT，
#           须重新登记（防豁免烂尾=每条豁免的复审期约束）。
#
# 用法：
#   python scripts/scan_constitution_parity.py            # 扫本仓
#   python scripts/scan_constitution_parity.py --root P   # 扫指定仓
#
# 退出码：0=绿灯（含豁免后零命中）；1=存在未豁免命中；2=环境错误
# =============================================================================

import argparse
import os
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

# 结构性豁免（扫描器内置：唯一实现点与测试面天然合法，无需逐条登记）。
# 仅此一个 crate 级豁免——rule_loader 是唯一装载通道本体。
STRUCTURAL_EXEMPTS = {
    "core/rule_loader/src/lib.rs": {"P1-transform-direct", "P3-serde-to-tcb-rewrite"},
}

SKIP_DIRS = {"target", "node_modules", ".git", "patches"}


def parse_anchor(anchor):
    """'path:12' -> (path, 12, 12); 'path:12-30' -> (path, 12, 30)"""
    if ":" not in anchor:
        return anchor, None, None  # 无行锚=文件级（仅结构性豁免同待遇，登记时须理由强化）
    path, _, lines = anchor.rpartition(":")
    m = re.match(r"^(\d+)(?:-(\d+))?$", lines)
    if not m:
        return anchor, None, None
    start = int(m.group(1))
    end = int(m.group(2)) if m.group(2) else start
    return path, start, end


def load_exemptions():
    """解析 allowed_exceptions.conf：行锚条目优先；# 开头为注释"""
    exemptions = []  # [(path, line_start, line_end, pattern_id, reason)]
    if not EXEMPT_FILE.exists():
        return exemptions
    for raw in EXEMPT_FILE.read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        parts = [p.strip() for p in line.split("|")]
        if len(parts) >= 3:
            path, ls, le = parse_anchor(parts[0])
            exemptions.append((path, ls, le, parts[1], "|".join(parts[2:])))
    return exemptions


def is_exempt(rel_path, pattern_id, line_no, exemptions):
    rel_norm = rel_path.replace("\\", "/")
    # 结构性豁免（rule_loader crate 本体=唯一实现点）
    for key, pats in STRUCTURAL_EXEMPTS.items():
        if rel_norm.endswith(key) and pattern_id in pats:
            return f"structural:{key}"
    # 行锚豁免：路径匹配 + 行号落锚段 + 模式匹配；无行锚条目=文件级（从严：仍放行
    # 但输出标记 file-scope，复审期须补行锚）
    for path, ls, le, pat, reason in exemptions:
        if path in rel_norm or rel_norm.endswith(path):
            if pat in (pattern_id, "*"):
                if ls is None:
                    return f"file-scope:{reason[:50]}"
                if ls <= line_no <= le:
                    return f"line:{ls}-{le}:{reason[:40]}"
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
    for dirpath, dirnames, filenames in os.walk(root):
        # 目录枚举层剪枝（O-459）：SKIP_DIRS 在遍历时整目录剔除，不深入
        # target/ 等大规模构建目录（原 rglob 语义=全枚举后逐文件过滤，
        # cargo 构建期间枚举本身即达分钟级）
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
        for fn in filenames:
            if not fn.endswith(".rs"):
                continue
            rs_file = Path(dirpath) / fn
            rel = str(rs_file.relative_to(root))
            scanned += 1
            try:
                text = rs_file.read_text(encoding="utf-8", errors="replace")
            except OSError:
                continue
            lines = text.splitlines()
            for pat in PATTERNS:
                for m in pat["regex"].finditer(text):
                    line_no = text.count("\n", 0, m.start()) + 1
                    line_text = lines[line_no - 1].strip() if line_no <= len(lines) else ""
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

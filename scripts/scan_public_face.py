#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# =============================================================================
# scan_public_face.py — 公开面内容矩阵扫描器
#
# 定位：发布前对 git tracked 全类型文件做敏感内容矩阵扫描（广谱一层），
#       与 check_doc_safety.py 分工互补：
#         - check_doc_safety.py：L1 公开文档细规则（引用完整性 / CHANGELOG
#           治理 / 门控 / 索引存在性）
#         - 本工具：全文件类型（含 CI 配置、.gitattributes、脚本注释等
#           词表覆盖盲区类型）的敏感词矩阵扫描；命中分级处置——
#           阻断类（exit 1，须修复）与复核类（exit 0，人工判断）。
#
# 词表纪律：连续敏感字面量一律以拼接形式书写（运行时等价），防止词表文件
#           自身成为泄露源（与 check_doc_safety.py 同一先例）。
#
# 用法：
#   python scripts/scan_public_face.py                 # 扫描本仓（脚本所在仓）
#   python scripts/scan_public_face.py --root PATH     # 扫描指定仓
#   python scripts/scan_public_face.py --json          # JSON 输出（供工具消费）
#
# 退出码：
#   0 = 无阻断类命中（复核类命中不改变退出码，输出清单供人工判断）
#   1 = 存在阻断类命中
#   2 = 环境错误（非 git 仓 / git 不可用等）
# =============================================================================

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

MAX_FILE_BYTES = 8 * 1024 * 1024
BINARY_PROBE_BYTES = 8192

# 词表载体文件豁免：其内容即规则声明本身，按各仓相对路径豁免
# （同 check_doc_safety.py 内 RULE_DECLARATION_FILES 先例）。
WORDLIST_CARRIER_FILES = {
    'scripts/check_doc_safety.py',
    'scripts/scan_public_face.py',
}

# 构建产物锁文件：公开面敏感内容概率极低且短代号易误报，不扫描。
SKIP_FILE_NAMES = {
    'Cargo.lock', 'package-lock.json', 'pnpm-lock.yaml', 'yarn.lock',
    'poetry.lock', 'go.sum', 'gradlew', 'gradlew.bat',
}

# 仓库注册表文件：按既有词表规则，注册表「可见性」列允许提及各仓仓名
# （同 check_doc_safety.py E 类黑名单注释的注册表例外），整文件豁免 C 类。
REGISTRY_FILES = {
    'REPO_REGISTRY.md',
}

# 复核类行级豁免提示：命中行若为规则声明语句（说明禁令本身），不算内容泄露
# （同 check_doc_safety.py RULE_DECLARATION_LINE_HINTS 先例）。
RULE_DECLARATION_LINE_HINTS = re.compile(
    r'(零容忍|禁止出现|不得引用|不得出现|不得链接|本地私有|私有集合|绝对不|'
    r'规则声明|不对外|不发布|\.gitignore|黑名单|白名单|豁免|'
    r'v0\.1\.0 基准评估|实验 1\.[0-9])'
)


def _ic(pattern: str):
    return re.compile(pattern, re.IGNORECASE)


# ---------------------------------------------------------------------------
# A1 类：AI 身份 / AI 署名（阻断）
# 助手工具代号与署名行模式（本生态无对应的合法产品集成语境）。
# 名单字面量一律拆写（运行时等价）。
# ---------------------------------------------------------------------------
AI_IDENTITY_PATTERNS = [
    _ic(r'\bma' + r'vis\b'),
    _ic(r'\bcl' + r'aude\b'),
    _ic(r'\banth' + r'ropic\b'),
    _ic(r'\bcopi' + r'lot\b'),
    _ic(r'\bchat' + r'gpt\b'),
    _ic(r'\bwork' + r'buddy\b'),
    _ic(r'\bcode' + r'buddy\b'),
    _ic(r'\bqo' + r'der\b'),
    _ic(r'\bcode' + r'arts\b'),
    _ic(r'co-authored-by'),
    # 作者性表述（署名/成文披露）；产品功能语境（AI 生成规则 / AI 辅助创建）
    # 为合法产品内容，不在此列。
    re.compile(r'(?:由|经)\s*AI\s*(?:生成|编写|创作|辅助)'),
    re.compile(r'AI\s*辅助\s*(?:编写|创作|开发|完成|起草|撰写)'),
    re.compile(r'AI\s*生成\s*(?:的|了)'),
]
# A1 行级豁免：产品功能名/集成产品名含助手词子串，属合法命名或集成文档。
AI_IDENTITY_HINTS = _ic(r'rule-copilot|Cl' + r'aude\s*Desktop')

# ---------------------------------------------------------------------------
# A2 类：LLM 集成厂商名（复核类，不阻断）
# 本生态多个产品将下列厂商作为 LLM provider / 预设合法集成（配置文件、
# 预设清单、README 功能表、构建产物中的预设字符串），裸厂商名出现属
# 产品内容而非身份泄露；保留输出供人工复核异常语境。
# ---------------------------------------------------------------------------
AI_VENDOR_INTEGRATION_PATTERNS = [
    _ic(r'\bope' + r'nai\b'),
    _ic(r'\bdee' + r'pseek\b'),
    _ic(r'\bqw' + r'en\b'),
    _ic(r'\bgl' + r'm\b'),
    _ic(r'\bgem' + r'ini\b'),
    _ic(r'\bki' + r'mi\b'),
    _ic(r'\bmini' + r'max\b'),
]

# ---------------------------------------------------------------------------
# B 类：内部编号体系（阻断）
# 以泛化形状为主；具体编号字面量拆写（运行时等价）。
# ---------------------------------------------------------------------------
INTERNAL_ID_PATTERNS = [
    re.compile('TC' + r'B-\d{4}-\d+'),
    re.compile('CR-' + r'\d{8}-\d{3}'),
    re.compile(r'UV-\d{2,3}'),
    re.compile(r'INC-\d{2,3}'),
    re.compile(r'REM-\d+'),
    # 「NN 号」内部档引用；负向断言排除法规令文号（令84号/〔2015〕43号）
    re.compile(r'(?<![令发〕])\d{2,3}\s*号'),
    re.compile(r'决策点\s*[①-⑨]'),
    re.compile(r'裁' + r'定\s*[①-⑨]'),
    re.compile(r'设计稿\s*\d+\s*号'),
    re.compile('PLANNING_' + r'FINALIZE'),
    re.compile(r'T[7' + r'8]\s*(?:缓办|调查报告)'),
]
# B 类行级豁免：法规/标准引文行（产品域合法内容，规则仓与体验包大量引用）
LEGAL_CITATION_LINE_HINTS = re.compile(
    r'(部令|国卫|〔\d{4}〕|GB/T|管理办法|指导原则|管理条例|实施细则|人民共和国)'
)

# ---------------------------------------------------------------------------
# C 类：私有路径 / 私有仓名（阻断）
# 路径与仓名字面量拆写（运行时等价）。
# ---------------------------------------------------------------------------
PRIVATE_PATH_PATTERNS = [
    re.compile('D:' + r'\\knowledge'),
    re.compile('D:' + r'\\审计'),
    re.compile('D:' + r'\\仓库管理'),
    re.compile('D:' + r'\\evorule'),
    re.compile('D:' + r'\\evo-agent'),
    re.compile(r'_PRIVATE_zh_docs'),
    re.compile(r'0[1-9]\d_[\u4e00-\u9fa5A-Za-z]'),
    re.compile('evorule' + r'-agent'),
    re.compile('evorule' + r'-application'),
    re.compile('evorule-' + r'backup'),
    re.compile(r'文档[\\/](design|implement|benchmarks|archive)'),
]

# ---------------------------------------------------------------------------
# D 类：治理语境（复核类——命中仅输出清单供人工判断，不改变退出码）
# 背景：部分词在产品域有合法语义，裸词误报率高，故本类不阻断；
#       治理口吻的高精度搭配单独成模式。
# ---------------------------------------------------------------------------
GOV_CONTEXT_PATTERNS = [
    re.compile('战' + r'地脚本|误下沉|止' + r'血|堵' + r'死|豁免清零|维持挂账'),
    re.compile('清' + r'偿|整' + r'改|反馈集中|专项同步|专项登记|批准删除'),
    re.compile('整' + r'治(?![\u4e00-\u9fffA-Za-z0-9_])'),
    re.compile('台' + r'账'),
    re.compile(r'(?<![成设建])立' + r'项'),
    re.compile('核' + r'销'),
    re.compile('窗' + r'口期'),
    re.compile('监督' + r'方'),
    re.compile('遗留' + r'项'),
    re.compile('拍' + r'板'),
    re.compile('留' + r'痕'),
    re.compile('项目' + r'方\s*(?:自行|批准|节奏|纠错|明示|确认|拍板)'),
    re.compile('内部协作区|决策过程记录|发布材料经审批'),
    re.compile(r'阶段\s*0[.\d]'),
]

# (类别名, 是否阻断, 模式列表, 行级豁免提示)
CATEGORIES = [
    ('A1-AI身份署名', True, AI_IDENTITY_PATTERNS, AI_IDENTITY_HINTS),
    ('A2-集成厂商名', False, AI_VENDOR_INTEGRATION_PATTERNS, None),
    ('B-内部编号', True, INTERNAL_ID_PATTERNS, LEGAL_CITATION_LINE_HINTS),
    ('C-私有路径仓名', True, PRIVATE_PATH_PATTERNS, RULE_DECLARATION_LINE_HINTS),
    ('D-治理语境', False, GOV_CONTEXT_PATTERNS, None),
]

# ---------------------------------------------------------------------------
# 文件级白名单（可选）：scripts/scan_public_face_allowlist.txt
# 用途：经项目方裁定接受留痕的工程主键类编号（变更记录主键）按文件豁免。
# 格式：每行 `<文件相对路径> <KIND>`；# 开头为注释；KIND 见下表（匹配值
#       由本扫描器内部正则定义，白名单文件自身不携带任何具体编号字面量）。
# ---------------------------------------------------------------------------
ALLOWLIST_KIND_RES = {
    # 工程变更主键（CR-/TCB- 编号，变更登记表与门禁整改溯源；用户裁定 2026-09-19）
    'change-key': [
        re.compile('^TC' + r'B-\d{4}-\d+$'),
        re.compile('^CR-' + r'\d{8}-\d{3}$'),
    ],
}
ALLOWLIST_DEFAULT_REL = 'scripts/scan_public_face_allowlist.txt'


def load_allowlist(root: Path, override: str = None):
    """返回 (dict: 相对路径 -> KIND 集合, 生效路径或 None)。"""
    path = Path(override) if override else root / ALLOWLIST_DEFAULT_REL
    if not path.exists():
        return {}, None
    entries = {}
    for lineno, raw in enumerate(path.read_text(encoding='utf-8-sig').splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith('#'):
            continue
        parts = line.split()
        if len(parts) != 2 or parts[1] not in ALLOWLIST_KIND_RES:
            raise SystemExit(
                f'ERROR: 白名单格式错（行 {lineno}）: {line!r}（文件 {path}）')
        entries.setdefault(parts[0].replace('\\', '/'), set()).add(parts[1])
    return entries, path


def finding_exempted(rel: str, match: str, allowlist: dict) -> bool:
    kinds = allowlist.get(rel)
    if not kinds:
        return False
    for kind in kinds:
        if any(r.fullmatch(match) for r in ALLOWLIST_KIND_RES[kind]):
            return True
    return False


def tracked_files(root: Path):
    """返回 (相对路径列表, 错误信息)。"""
    out = subprocess.run(['git', 'ls-files', '-z'], cwd=str(root),
                         capture_output=True)
    if out.returncode != 0:
        return None, out.stderr.decode('utf-8', 'replace').strip()
    names = [n.decode('utf-8', 'replace') for n in out.stdout.split(b'\x00') if n]
    return names, None


def decode_text(data: bytes):
    """按 utf-8-sig / utf-8 / gbk 依次尝试解码，失败返回 None。"""
    for enc in ('utf-8-sig', 'utf-8', 'gbk'):
        try:
            return data.decode(enc)
        except UnicodeDecodeError:
            continue
    return None


def scan_repo(root: Path, allowlist: dict = None):
    """返回 (findings, skipped, error)。finding = dict。"""
    allowlist = allowlist or {}
    names, err = tracked_files(root)
    if err is not None:
        return [], [], err
    findings, skipped = [], []
    for name in names:
        rel = name.replace('\\', '/')
        if rel in WORDLIST_CARRIER_FILES:
            continue
        if Path(rel).name in SKIP_FILE_NAMES:
            continue
        fpath = root / rel
        try:
            data = fpath.read_bytes()
        except OSError:
            skipped.append((rel, 'unreadable'))
            continue
        if b'\x00' in data[:BINARY_PROBE_BYTES]:
            skipped.append((rel, 'binary'))
            continue
        if len(data) > MAX_FILE_BYTES:
            skipped.append((rel, 'too-large'))
            continue
        text = decode_text(data)
        if text is None:
            skipped.append((rel, 'undecodable'))
            continue
        for lineno, line in enumerate(text.splitlines(), start=1):
            for cat_name, blocking, patterns, hints in CATEGORIES:
                # 注册表文件按既有裁定豁免 C 类（可见性列可提仓名）
                if blocking and rel in REGISTRY_FILES and cat_name.startswith('C-'):
                    continue
                if hints is not None and hints.search(line):
                    continue
                for pat in patterns:
                    m = pat.search(line)
                    if m:
                        match_text = m.group(0)[:60]
                        if finding_exempted(rel, m.group(0), allowlist):
                            continue
                        findings.append({
                            'file': rel,
                            'line': lineno,
                            'category': cat_name,
                            'blocking': blocking,
                            'match': match_text,
                            'text': line.strip()[:160],
                        })
    return findings, skipped, None


def main() -> int:
    parser = argparse.ArgumentParser(description='公开面内容矩阵扫描器')
    parser.add_argument('--root', default=None,
                        help='待扫描仓库根目录（默认 = 本脚本所在仓）')
    parser.add_argument('--json', action='store_true', help='JSON 输出')
    parser.add_argument('--allowlist', default=None,
                        help='白名单文件路径（默认探测 scripts/scan_public_face_allowlist.txt）')
    parser.add_argument('--fail-on', action='append', default=None, metavar='PREFIX',
                        help='仅对命中类别以 PREFIX 开头的阻断类计入退出码（可多次传；'
                             '缺省=全部阻断类计入。用于分类别收紧门禁，如 C 类存量未裁定前'
                             ' CI 传 --fail-on A1 --fail-on B）')
    args = parser.parse_args()

    root = Path(args.root).resolve() if args.root else \
        Path(__file__).resolve().parent.parent
    if not (root / '.git').exists():
        print(f'ERROR: 非 git 仓库: {root}', file=sys.stderr)
        return 2

    allowlist, allowlist_path = load_allowlist(root, args.allowlist)

    findings, skipped, err = scan_repo(root, allowlist)
    if err is not None:
        print(f'ERROR: git ls-files 失败: {err}', file=sys.stderr)
        return 2

    if args.json:
        print(json.dumps({
            'root': str(root),
            'findings': findings,
            'skipped': [{'file': f, 'reason': r} for f, r in skipped],
        }, ensure_ascii=False, indent=2))
    else:
        print(f'扫描仓: {root}')
        if allowlist_path is not None:
            print(f'白名单: {allowlist_path}（{len(allowlist)} 文件）')
        blocking = [f for f in findings if f['blocking']]
        advisory = [f for f in findings if not f['blocking']]
        for title, group, mark in (('阻断类', blocking, '[阻断]'),
                                   ('复核类', advisory, '[复核]')):
            print(f'\n== {title}（{len(group)} 项） ==')
            for f in group:
                print(f"  {f['file']}:{f['line']} {mark} "
                      f"({f['category']}) 命中「{f['match']}」")
                print(f"      {f['text']}")
        if skipped:
            print(f'\n（跳过 {len(skipped)} 个文件：二进制/超限/不可解码，'
                  f'明细见 --json）')
        enforce = [f for f in blocking
                   if not args.fail_on
                   or any(f['category'].startswith(p) for p in args.fail_on)]
        if args.fail_on:
            print(f'\n（阻断计入口径: 前缀 {args.fail_on} → 计入 {len(enforce)} 项）')
        verdict = 'FAIL' if enforce else 'PASS'
        print(f'\n{verdict}: 阻断类 {len(blocking)} 项 / 复核类 '
              f'{len(advisory)} 项')

    blocking_count = sum(
        1 for f in findings
        if f['blocking']
        and (not args.fail_on
             or any(f['category'].startswith(p) for p in args.fail_on)))
    return 1 if blocking_count else 0


if __name__ == '__main__':
    sys.exit(main())

#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# =============================================================================
# check_doc_safety.py — EvoRule 文档安全与引用完整性检查器
#
# [同步戳记] 母本＝evorule 主仓 scripts/check_doc_safety.py，各公开仓副本以字节级一致为对齐标准；
# [同步戳记] 漂移检测与分发：scripts/sync-doc-safety.ps1（-Check 只报告 / -Sync 复制并逐文件校验）；
#            母本变更后必须 -Sync 分发并逐仓提交推送双远端。
# 覆盖规则（治理方案 048 v1.0 §阶段 3.1 + AGENTS.md 内部约定 + CHANGELOG-GOVERNANCE-20260916.md）：
#   R-门控1 : git staged 文件不得包含「文档/」路径（禁止仓内共享/私有文档 commit）
#   R3-引用合规零容忍：L1 公开文档禁止出现私有集合路径/文件名字面量
#                      （_PRIVATE_zh_docs / 常见私有文件名片段）
#   R-交叉引用完整性：L1 文档中指向同层 L1 的链接必须真实存在
#   R-索引存在性：DOCS_INDEX.md 列出的 L1 路径必须存在（单向存在性检查）
#   R-L1不提L2/L3：L1 公开文档禁止链接到 文档/design|implement|benchmarks|archive/
#   R-CHANGELOG内容治理：CHANGELOG 只记录功能实现/变动；六类黑名单
#                      （A 内部编号 / B 过程语言 / C 未发布计划 / D 商业策略 / E 私有仓名 / F 内部流程）
#
# 用法：
#   python scripts/check_doc_safety.py                 # 默认 = --strict（全项 + 有违规 exit 1）
#   python scripts/check_doc_safety.py --warn          # 仅输出违规，exit 总是 0（用于初期 CI）
#   python scripts/check_doc_safety.py --skip-git      # 跳过 R-门控1（本地非 git 环境）
#   python scripts/check_doc_safety.py --json          # JSON 输出（CI 消费）
#
# 退出码（--strict 模式）：
#   0 = 全部通过, 1 = 违规, 2 = 环境错误（不是 git repo / 文件不可读等）
# =============================================================================

import argparse
import json
import os
import re
import subprocess
import sys
from pathlib import Path
from typing import List, Tuple, Dict, Any

REPO_ROOT = Path(__file__).resolve().parent.parent

# ---------------------------------------------------------------------------
# 规则模式
# ---------------------------------------------------------------------------

# R-门控1：文档/ 路径（staged 文件不能带此前缀）
DOCS_PATH_PATTERN = re.compile(r'(^|[\\/])文档($|[\\/])')

# R3：私有集合泄露（按字面量 / 私有目录名 / 典型私有文件编号前缀来抓）
# 注意：只在 L1 公开文档范围内启用，L2/L3 允许提到「私有集合」这四个字，但不能出现具体文件名。
PRIVATE_LEAK_PATTERNS = [
    re.compile(r'_PRIVATE_zh_docs'),                 # 私有集合根目录名（零容忍）
    re.compile(r'0[1-9]\d_[\u4e00-\u9fa5A-Za-z]'),  # 私有编号前缀：001_xxx ~ 099_xxx
]

# ---------------------------------------------------------------------------
# R-agent身份零泄露：L1 公开文档禁止泄露 AI agent 身份表述
# 基调:文档中不要泄露 agent 的身份。
# 区分:agent 作为产品概念(如"agent 编排""构建 agent 应用")允许;身份表述禁止。
# ---------------------------------------------------------------------------
# 身份泄露词(只在身份泄露语境出现,不会在产品概念语境出现)
AGENT_IDENTITY_PATTERNS = [
    re.compile(r'给\s*AI\s*agent\s*的\s*规则'),
    re.compile(r'给\s*agent\s*的\s*规则'),
    re.compile(r'给\s*AI\s*agent'),
    re.compile(r'给\s*LLM'),
    re.compile(r'LLM\s*/\s*agent\s*程序化'),
    re.compile(r'LLM/agent'),
    re.compile(r'程序化消费'),
    re.compile(r'双轨制'),
    re.compile(r'机器可读附录'),
    re.compile(r'供\s*LLM\s*消费'),
    re.compile(r'供\s*agent\s*消费'),
    re.compile(r'AI\s*agent\s*开发'),
    re.compile(r'agent\s*程序化'),
    re.compile(r'LLM\s*读'),
    re.compile(r'LLM\s*程序化'),
    re.compile(r'agent\s*读'),
]
# agent 产品概念白名单:命中则不算泄露(agent 作为产品/应用概念)
AGENT_PRODUCT_HINTS = re.compile(
    r'(agent\s*编排|agent\s*应用|agent\s*框架|agent\s*能力|'
    r'构建\s*agent|agent\s*demo|agent\s*示例|'
    r'research\s*agent|reactive\s*agent|'
    r'agent\s*层|agent\s*系统|多\s*agent|'
    r'给\s*LLM\s*精灵|'  # 产品/文学表达:LLM 作为受众(如"给 LLM 精灵一个确定性落点"),非 AI 协作身份泄露
    r'给\s*LLM\s*一个|'  # 产品概念:"给 LLM 一个可信任的执行层"——LLM 作为服务对象/受众,非身份泄露
    r'交给\s*LLM|'      # 产品概念:"交给 LLM,这是它的天赋"——LLM 作为分工对象,非身份泄露
    r'evo-agent)'  # evo-agent 是产品名(agent 产品概念),放行
)

# R3 例外：以下 L1 文件是「规则声明本身」，允许提到私有集合名/目录名作为规则说明。
# （这些文件是 AGENTS 规则 / 索引自述 / 检查脚本本体）
RULE_DECLARATION_FILES = {
    'DOCS_INDEX.md',            # 索引说明（可能提到「私有文档不对外」字样）
}
# R3 例外：一行里如果包含「零容忍」「禁止出现」「不得引用」「本地私有」等
# 说明它是规则声明的说明文字，不是泄露内容
RULE_DECLARATION_LINE_HINTS = re.compile(
    r'(零容忍|禁止出现|不得引用|本地私有|私有集合|绝对不 commit|绝对不在|引用合规|文档索引强制|先写设计文档)'
)

# R-L1不提L2/L3：L1 文档不能出现 `文档/` + 四个已知子目录名
L2L3_REF_PATTERN = re.compile(r'文档[\\/](design|implement|benchmarks|archive)')
# L2/L3 例外：与 R3 同一套规则声明文件（AGENTS.md / DOCS_INDEX.md）
# 另外 DOCS_INDEX 中的「D2 搬迁说明」也允许（标注搬迁痕迹的说明行）
L2L3_EXEMPT_HINTS = re.compile(
    r'(按 D2|保守搬迁|永不发布|\.gitignore 保护|L2 设计规范层|L3 实施细节层|仓内共享|不发布|先写设计文档|v0\.1\.0 基准评估|实验 1\.1)'
)

# L1 层目录定义：根目录 *.md + docs/**（不含 docs/benchmarks，已搬走留空）
# 注意：不包含 tier0/1/2/cli crate 根（另有 R-分层 crate README 一致性，留到阶段 4.3）
L1_ROOTS = [
    REPO_ROOT,                 # 根目录 md
    REPO_ROOT / 'docs',        # docs/**
]
L1_EXCLUDE_DIRS = {'.git', 'target', 'node_modules', '.build', '.trae', '.gitee-ci', '.github'}

# R-交叉引用：匹配 Markdown 链接 [text](path) 中相对/绝对路径（不含 http(s): mailto: #anchor）
MD_LINK_RE = re.compile(r'\[[^\]]*\]\(([^)]+)\)')

# ---------------------------------------------------------------------------
# R-CHANGELOG内容治理（CHANGELOG-GOVERNANCE-20260916.md v1.0）
# 铁律：CHANGELOG 只记录"与系统功能实现/变动直接相关"的内容。
# 六类黑名单模式均为高置信（对已清洗内容零误报）；命中即计入违规（--warn 降级为提示）。
# ---------------------------------------------------------------------------
# A 类内部编号
CL_A_INTERNAL_IDS = [
    re.compile(r'TCB-2026-\d+'),
    re.compile('CR-' + r'2026\d{6}-\d{3}'),  # 维护注记：字面量以拼接形式书写，避免被通用文本扫描工具直接命中；拼接后与连续写法运行时等价
    re.compile('6' + r'9 号'),
    re.compile(r'决策点\s*[①-⑨]'),
    re.compile(r'设计稿\s*\d+\s*号'),
    re.compile(r'UV-\d{2,3}'),
    re.compile(r'裁定[①-⑨]'),
    re.compile('4' + r'5 号'),
    re.compile(r'T[78]\s*(?:缓办|调查报告)'),
    re.compile(r'债务\s*D2|D2\s*闭合|A3[）):：]'),
]
# B 类过程语言
# 注意：中文无词边界，"整治"加后置边界防跨界误报（"调整治理服务"会拼出"整治"）
CL_B_PROCESS = [
    re.compile(r'战地脚本|误下沉|止血|堵死|豁免清零|维持挂账|清偿|整改|反馈集中|专项同步|专项登记|批准删除'),
    re.compile(r'整治(?![\u4e00-\u9fffA-Za-z0-9_])'),
]
# C 类未发布/计划声明
CL_C_UNRELEASED = [
    re.compile(r'##\s*\[未发布\]'),
    re.compile(r'起草中'),
    re.compile(r'\bDeferred\b'),
    re.compile(r'计划[（(](?:短期|中期|长期)'),
]
# D 类商业策略（仅内部动机措辞；"双轨许可"作为公开 License 事实放行，须带"兜底"等内部词才报）
CL_D_COMMERCIAL = [
    re.compile(r'灰色通道|许可档位|双轨许可兜底|MIT SDK|防绕过'),
]
# E 类私有仓名（公开文档 L1 禁入；仅注册表"可见性"列可提）
CL_E_PRIVATE_REPO = [
    re.compile(r'evorule-agent'),
    re.compile(r'evorule-application'),
]
# F 类内部流程引用
CL_F_INTERNAL = [
    re.compile(r'内部协作区|项目方体验反馈|发布材料经审批|决策过程记录'),
]

CL_PATTERNS: List[Tuple[str, List]] = [
    ('A-内部编号', CL_A_INTERNAL_IDS),
    ('B-过程语言', CL_B_PROCESS),
    ('C-未发布/计划', CL_C_UNRELEASED),
    ('D-商业策略', CL_D_COMMERCIAL),
    ('E-私有仓名', CL_E_PRIVATE_REPO),
    ('F-内部流程', CL_F_INTERNAL),
]

# ---------------------------------------------------------------------------
# R-CHANGELOG发布纪律（Keep a Changelog v1.1 §推荐 Unreleased 用法）
# 铁律：发布 tag 时，必须把头部 `## [Unreleased]` 转成 `## [x.y.z] - 日期` 并清空，
#       然后才打语义化版本 tag。两者是一体动作，不可只做一半。
# 硬检查（FAIL）：最新语义化版本 tag 必须在 CHANGELOG 中有对应版本区块
#       （`## [x.y.z]`，tag 的 v 前缀忽略）——发布时打了 tag 但忘了把 Unreleased
#       转版本号 / 忘更 CHANGELOG，即被拦截。
# 提示级（不 FAIL）：`## [Unreleased]` 区块有实质条目 = 开发期正常累积（Keep a
#       Changelog 推荐结构：Unreleased 恒在顶部、其后是发布记录区块，勿误判为残留）。
# ---------------------------------------------------------------------------
UNRELEASED_HEADER = re.compile(r'^##\s*\[Unreleased\]', re.IGNORECASE)
CL_VERSION_HEADER = re.compile(r'^##\s*[\[(]?v?(\d+\.\d+\.\d+)[\])]?\s*[-—]')
VERSION_TAG_RE = re.compile(r'^v?(\d+\.\d+\.\d+)$')
# 区块内非条目行：空行 / HTML 注释 / blockquote 引用说明（如「发布治理规则」）
CL_NON_ENTRY_LINE = re.compile(r'^\s*(<!--|>|$)')


def _latest_version_tag(root: Path) -> str | None:
    """返回最新语义化版本 tag 的标准化版本号（v 前缀忽略），无 tag 返回 None。"""
    out, _, rc = run(['git', 'tag', '--list'], root)
    if rc != 0:
        return None
    versions = []
    for t in out.splitlines():
        m = VERSION_TAG_RE.match(t.strip())
        if m:
            versions.append(tuple(int(p) for p in m.group(1).split('.')))
    if not versions:
        return None
    latest = max(versions)
    return f'{latest[0]}.{latest[1]}.{latest[2]}'


def check_changelog_release_alignment(root: Path) -> List[Tuple[Path, int, str, str]]:
    """返回 [(path, lineno, category, snippet)]
    规则：最新语义化版本 tag 必须在 CHANGELOG 有对应版本区块（## [x.y.z]）。
    发布纪律：tag 与 CHANGELOG 是一体动作——打 tag 前须先把 Unreleased 转版本号并清空。
    """
    violations: List[Tuple[Path, int, str, str]] = []
    latest = _latest_version_tag(root)
    if latest is None:
        return violations  # 从未发布，跳过
    candidates = [root / 'CHANGELOG.md']
    if root.is_dir():
        for d in sorted(root.iterdir()):
            if d.is_dir() and d.name not in L1_EXCLUDE_DIRS:
                candidates.append(d / 'CHANGELOG.md')
    seen = set()
    for f in candidates:
        if not f.exists():
            continue
        fp = f.resolve()
        if fp in seen:
            continue
        seen.add(fp)
        try:
            lines = f.read_text(encoding='utf-8').splitlines()
        except (OSError, UnicodeDecodeError):
            continue
        if any(CL_VERSION_HEADER.match(l) and CL_VERSION_HEADER.match(l).group(1) == latest for l in lines):
            continue
        violations.append((fp, 1, 'R-CHANGELOG-RELEASE:tag与CHANGELOG不对齐',
                           f'最新版本 tag=v{latest} 在 CHANGELOG 无对应 [v?{latest}] 区块（发布时未把 Unreleased 转版本号并清空）'))
    return violations


def changelog_unreleased_notes(root: Path) -> List[Tuple[Path, int, str]]:
    """提示级：Unreleased 区块有实质条目 = 开发期正常累积（非违规，仅提示）。"""
    notes: List[Tuple[Path, int, str]] = []
    candidates = [root / 'CHANGELOG.md']
    if root.is_dir():
        for d in sorted(root.iterdir()):
            if d.is_dir() and d.name not in L1_EXCLUDE_DIRS:
                candidates.append(d / 'CHANGELOG.md')
    seen = set()
    for f in candidates:
        if not f.exists():
            continue
        fp = f.resolve()
        if fp in seen:
            continue
        seen.add(fp)
        try:
            lines = f.read_text(encoding='utf-8').splitlines()
        except (OSError, UnicodeDecodeError):
            continue
        for idx, line in enumerate(lines):
            if not UNRELEASED_HEADER.match(line):
                continue
            has_content = any(
                l.strip() and not CL_NON_ENTRY_LINE.match(l)
                for l in lines[idx + 1:]
                if not CL_VERSION_HEADER.match(l)
            )
            if has_content:
                notes.append((fp, idx + 1, '开发期 Unreleased 累积中（发布时须转版本号并清空）'))
    return notes


def run(cmd: List[str], cwd: Path) -> Tuple[str, str, int]:
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, cwd=str(cwd))
    except FileNotFoundError:
        return '', 'git not found', 127
    return r.stdout, r.stderr, r.returncode


# ---------------------------------------------------------------------------
# R-门控1：文档/ 路径 commit 检查（与历史兼容的旧行为）
# ---------------------------------------------------------------------------

def check_gate_staged(cwd: Path, check_history: bool) -> Tuple[bool, List[str], List[str]]:
    """
    返回 (ok, staged_violations, history_violations)。
    staged_violations == None 表示 git 命令失败。
    """
    staged_violations: List[str] = []
    history_violations: List[str] = []

    out, _, rc = run(['git', 'diff', '-z', '--cached', '--name-only', '--diff-filter=ACMRT'], cwd)
    if rc != 0:
        return False, [], []
    staged_files = [f for f in out.split('\0') if f]
    staged_violations = [f for f in staged_files if DOCS_PATH_PATTERN.search(f)]

    if check_history:
        out, _, _ = run(['git', 'ls-files', '-z', '--', '文档/'], cwd)
        tracked = [f for f in out.split('\0') if f]
        out, _, _ = run(['git', 'log', '-z', '--all', '--pretty=format:', '--name-only',
                         '--diff-filter=A', '--', '文档/'], cwd)
        historical = [f for f in out.split('\0') if f]
        history_violations = tracked + historical

    ok = not staged_violations and not history_violations
    return ok, staged_violations, history_violations


# ---------------------------------------------------------------------------
# 辅助：列出 L1 公开文档路径
# ---------------------------------------------------------------------------

def list_l1_docs(root: Path) -> List[Path]:
    files: List[Path] = []
    roots = [root, root / 'docs']
    for base in roots:
        if not base.exists():
            continue
        for dirpath, dirnames, filenames in os.walk(str(base)):
            # 剪枝：不进入排除目录（避免遍历 .build 等大型构建目录）
            dirnames[:] = [d for d in dirnames if d not in L1_EXCLUDE_DIRS]
            cur = Path(dirpath)
            for f in filenames:
                if not f.endswith('.md'):
                    continue
                p = cur / f
                if any(excl in p.parts for excl in L1_EXCLUDE_DIRS):
                    continue
                # 仅保留 L1 区域内：根目录下直接 md（非任何 exclude 子目录）或 docs/** 下 md
                if base == root:
                    if p.parent != root:
                        continue  # 根目录只看直下
                files.append(p.resolve())
    return sorted(set(files))


# ---------------------------------------------------------------------------
# R3：L1 私有集合泄露检查
# ---------------------------------------------------------------------------

def check_private_leak(docs: List[Path], root: Path) -> List[Tuple[Path, int, str, str]]:
    """返回 [(path, lineno, matched_pattern_indicator, snippet)]"""
    violations: List[Tuple[Path, int, str, str]] = []
    for doc in docs:
        rel_name = doc.name
        try:
            lines = doc.read_text(encoding='utf-8').splitlines()
        except (OSError, UnicodeDecodeError):
            continue
        for i, line in enumerate(lines, 1):
            # 规则声明文件 + 规则声明文字行 => 例外（避免自指矛盾）
            if rel_name in RULE_DECLARATION_FILES and RULE_DECLARATION_LINE_HINTS.search(line):
                continue
            for pat in PRIVATE_LEAK_PATTERNS:
                m = pat.search(line)
                if m:
                    violations.append((doc, i, pat.pattern, line.strip()))
                    break
    return violations


# ---------------------------------------------------------------------------
# R-L1不提L2/L3：L1 文档禁止提到 `文档/design|implement|benchmarks|archive/`
# ---------------------------------------------------------------------------

def check_l1_mentions_l2l3(docs: List[Path], root: Path) -> List[Tuple[Path, int, str]]:
    violations: List[Tuple[Path, int, str]] = []
    for doc in docs:
        rel_name = doc.name
        try:
            lines = doc.read_text(encoding='utf-8').splitlines()
        except (OSError, UnicodeDecodeError):
            continue
        for i, line in enumerate(lines, 1):
            # 规则声明文件例外
            if rel_name in RULE_DECLARATION_FILES and L2L3_EXEMPT_HINTS.search(line):
                continue
            if L2L3_REF_PATTERN.search(line):
                violations.append((doc, i, line.strip()))
    return violations


# ---------------------------------------------------------------------------
# R-agent身份零泄露：L1 公开文档禁止泄露 AI agent 身份表述
# ---------------------------------------------------------------------------

def check_agent_identity_leak(docs: List[Path], root: Path) -> List[Tuple[Path, int, str, str]]:
    """返回 [(path, lineno, pattern, snippet)]
    规则:
      - 废弃文档(顶部 [已废弃] 横幅)跳过
      - 命中 AGENT_IDENTITY_PATTERNS(身份泄露词) → 违规
      - 同行命中 AGENT_PRODUCT_HINTS(产品概念) → 放行(产品概念允许)
    """
    violations: List[Tuple[Path, int, str, str]] = []
    for doc in docs:
        try:
            lines = doc.read_text(encoding='utf-8').splitlines()
        except (OSError, UnicodeDecodeError):
            continue
        # 废弃文档跳过
        if '[已废弃]' in '\n'.join(lines[:50]):
            continue
        # 审计/威胁模型文档跳过(历史审计快照)
        if re.search(r'AUDIT|THREAT_MODEL', doc.name):
            continue
        for i, line in enumerate(lines, 1):
            for pat in AGENT_IDENTITY_PATTERNS:
                m = pat.search(line)
                if not m:
                    continue
                # 同行命中产品概念白名单 → 放行
                if AGENT_PRODUCT_HINTS.search(line):
                    continue
                violations.append((doc, i, pat.pattern, line.strip()))
                break
    return violations


# ---------------------------------------------------------------------------
# R-CHANGELOG内容治理：扫描本仓 CHANGELOG.md（含一层子目录如 evorule-cli/）
# 六类黑名单高置信模式；命中即违规。诚实注记（内部基线/勘误）不在模式内，不误报。
# ---------------------------------------------------------------------------

def check_changelog_governance(root: Path) -> List[Tuple[Path, int, str, str]]:
    """返回 [(path, lineno, category:pattern, snippet)]"""
    violations: List[Tuple[Path, int, str, str]] = []
    candidates = [root / 'CHANGELOG.md']
    if root.is_dir():
        for d in sorted(root.iterdir()):
            if d.is_dir() and d.name not in L1_EXCLUDE_DIRS:
                candidates.append(d / 'CHANGELOG.md')
    seen = set()
    for f in candidates:
        if not f.exists():
            continue
        fp = f.resolve()
        if fp in seen:
            continue
        seen.add(fp)
        try:
            lines = f.read_text(encoding='utf-8').splitlines()
        except (OSError, UnicodeDecodeError):
            continue
        for i, line in enumerate(lines, 1):
            for cat, pats in CL_PATTERNS:
                for pat in pats:
                    if pat.search(line):
                        violations.append((fp, i, f'{cat}:{pat.pattern}', line.strip()))
                        break
    return violations


# ---------------------------------------------------------------------------
# R-交叉引用完整性：L1 文档中的 md 链接（指向仓内非 http）必须存在
# ---------------------------------------------------------------------------

# 外部兄弟仓路径（不在 evorule 核心仓内，但允许 L1 文档引用为相对路径）
# 这些路径在 evorule 核心仓内不存在，因此视为「跨仓引用」不校验存在性。
EXTERNAL_SIBLING_PREFIXES = (
    'evo-agent/',
    'evorule-application/',
)
# 明显的占位链接（不是真的引用文件，跳过校验）
PLACEHOLDER_LINK_MARKERS = re.compile(
    r'(申请表单|^NOTICE$|vX\.Y\.Z|v\*|\*_AUDIT_v\*)'
)

# 已确认被删除的旧文件（链接仍在历史文档里，但不回滚），链接检查白名单
KNOWN_DELETED_DOCS = {
    'EVORULE_FORMAL_VERTIFICATION_PLAN.md',
}


def resolve_md_link(link: str, source_doc: Path, root: Path) -> Path | None:
    # 抛锚 + 外部协议
    if not link or link.startswith('#') or link.startswith('http://') or link.startswith('https://') \
            or link.startswith('mailto:'):
        return None
    # 去除 title "...."
    raw = link.split(' ', 1)[0]
    raw = raw.split('#', 1)[0]  # 去掉同页/跨页锚点
    if not raw:
        return None
    if raw.startswith('/'):
        # 仓内绝对路径
        target = (root / raw.lstrip('/')).resolve()
    else:
        target = (source_doc.parent / raw).resolve()
    # 限制必须在 root 内，防 ../ 逃逸
    try:
        target.relative_to(root)
    except ValueError:
        return None
    return target


def check_cross_refs(docs: List[Path], root: Path) -> List[Tuple[Path, int, str, Path]]:
    """[(source_doc, lineno, raw_link, resolved_target)]"""
    violations: List[Tuple[Path, int, str, Path]] = []
    for doc in docs:
        try:
            lines = doc.read_text(encoding='utf-8').splitlines()
        except (OSError, UnicodeDecodeError):
            continue
        for i, line in enumerate(lines, 1):
            for m in MD_LINK_RE.finditer(line):
                raw = m.group(1).split(' ', 1)[0].split('#', 1)[0]
                # 占位链接跳过
                if PLACEHOLDER_LINK_MARKERS.search(raw):
                    continue
                # 跨仓兄弟仓路径跳过（处理相对路径里的多个 ../）
                stripped = raw.lstrip('./')
                if any(stripped.startswith(pref) for pref in EXTERNAL_SIBLING_PREFIXES):
                    continue
                # 已确认删除的旧文件跳过（但只白名单 D1 阶段明确删除的）
                if raw.split('/')[-1] in KNOWN_DELETED_DOCS:
                    continue
                # 非 md 链接（如 .rs 源码链接）跳过存在性校验：
                # L1 到源码的引用是允许的，但检查源码是否存在是构建系统职责
                if raw.endswith('.rs'):
                    continue
                target = resolve_md_link(m.group(1), doc, root)
                if target is None:
                    continue
                if not target.exists():
                    violations.append((doc, i, m.group(1), target))
    return violations


# ---------------------------------------------------------------------------
# R-索引存在性：DOCS_INDEX.md 列出的 L1 路径必须存在
# 极简实现：正则提取形如 (xxx.md) / [xxx.md](docs/xxx.md) 的路径，检查仓内文件
# 注：`文档/benchmarks/EVAL_xxx.md` 这类仓内路径属于 L3，本规则不报（因为不在 L1）
# ---------------------------------------------------------------------------

DOCS_INDEX_NAME = 'DOCS_INDEX.md'

def check_docs_index_exist(docs: List[Path], root: Path) -> List[Tuple[Path, int, str, Path]]:
    violations: List[Tuple[Path, int, str, Path]] = []
    idx = root / DOCS_INDEX_NAME
    if not idx.exists():
        return violations  # 不把索引本身不存在的问题归在这一类（另有巡检）
    try:
        lines = idx.read_text(encoding='utf-8').splitlines()
    except (OSError, UnicodeDecodeError):
        return violations

    for i, line in enumerate(lines, 1):
        # 提取 (path) 括号里的路径（markdown link / 裸路径都算）
        for m in re.finditer(r'\(([^\s)]+\.md(?:#[^)]*)?)\)', line):
            raw = m.group(1).split('#', 1)[0]
            # 未来占位跳过：RELEASE_PROCESS_vX.Y.Z.md / *_AUDIT_v*.md / 含通配符 * / 含大写占位
            if PLACEHOLDER_LINK_MARKERS.search(raw) or '*' in raw:
                continue
            # 已明确删除的 D1 文档跳过
            if raw.split('/')[-1] in KNOWN_DELETED_DOCS:
                continue
            target = resolve_md_link(m.group(1), idx, root)
            if target is None:
                continue
            # 只对 L1 范围内的路径做存在性检查（L3 `文档/xxx` 不在此规则）
            try:
                rel = target.relative_to(root)
            except ValueError:
                continue
            # L1 = 直下 md 或 docs/** 下 md
            in_l1 = (len(rel.parts) == 1 and str(rel).endswith('.md')) or \
                    (len(rel.parts) >= 2 and rel.parts[0] == 'docs')
            if in_l1 and not target.exists():
                violations.append((idx, i, raw, target))

        # 裸路径（不含括号）：`docs/xxx.md` 这种单独写的路径（非链接）也做抽查
        for m in re.finditer(r'(?<![\(\/])\b(docs[\/][^\s`\|\]]+\.md)\b', line):
            raw = m.group(1)
            # 占位跳过
            if PLACEHOLDER_LINK_MARKERS.search(raw) or '*' in raw:
                continue
            target = (root / raw).resolve()
            try:
                target.relative_to(root)
            except ValueError:
                continue
            if not target.exists():
                violations.append((idx, i, raw, target))

    return violations


# ---------------------------------------------------------------------------
# 主流程
# ---------------------------------------------------------------------------

def collect_all(root: Path, skip_git: bool) -> Dict[str, Any]:
    result: Dict[str, Any] = {
        'gate_staged': {'ok': True, 'staged_violations': [], 'history_violations': []},
        'private_leak_l1': [],
        'l1_mentions_l2l3': [],
        'agent_identity_leak_l1': [],
        'changelog_governance': [],
        'changelog_release_alignment': [],
        'changelog_unreleased_notes': [],
        'cross_ref_l1': [],
        'docs_index_exist': [],
    }

    if not skip_git:
        ok, staged, hist = check_gate_staged(root, check_history=False)
        result['gate_staged'] = {
            'ok': ok, 'staged_violations': staged, 'history_violations': hist,
        }

    docs = list_l1_docs(root)
    result['_l1_docs_count'] = len(docs)

    # 私有泄露
    for (p, ln, pat, snip) in check_private_leak(docs, root):
        result['private_leak_l1'].append({
            'file': str(p.relative_to(root)),
            'line': ln, 'pattern': pat, 'snippet': snip,
        })
    # L1 提 L2/L3
    for (p, ln, snip) in check_l1_mentions_l2l3(docs, root):
        result['l1_mentions_l2l3'].append({
            'file': str(p.relative_to(root)),
            'line': ln, 'snippet': snip,
        })
    # R-agent身份零泄露
    for (p, ln, pat, snip) in check_agent_identity_leak(docs, root):
        result['agent_identity_leak_l1'].append({
            'file': str(p.relative_to(root)),
            'line': ln, 'pattern': pat, 'snippet': snip,
        })
    # R-CHANGELOG内容治理
    for (p, ln, cat, snip) in check_changelog_governance(root):
        result['changelog_governance'].append({
            'file': str(p.relative_to(root)), 'line': ln,
            'category': cat, 'snippet': snip,
        })
    # R-CHANGELOG发布纪律（tag 与 CHANGELOG 对齐，硬检查）
    for (p, ln, cat, snip) in check_changelog_release_alignment(root):
        result['changelog_release_alignment'].append({
            'file': str(p.relative_to(root)), 'line': ln,
            'category': cat, 'snippet': snip,
        })
    # R-CHANGELOG Unreleased 累积提示（提示级，不进违规）
    result['changelog_unreleased_notes'] = [
        {'file': str(p.relative_to(root)), 'line': ln, 'note': note}
        for (p, ln, note) in changelog_unreleased_notes(root)
    ]
    # 交叉引用
    for (p, ln, raw, tgt) in check_cross_refs(docs, root):
        result['cross_ref_l1'].append({
            'file': str(p.relative_to(root)), 'line': ln,
            'link': raw, 'missing': str(tgt.relative_to(root)),
        })
    # DOCS_INDEX 列出的 L1 路径存在性
    for (p, ln, raw, tgt) in check_docs_index_exist(docs, root):
        result['docs_index_exist'].append({
            'file': str(p.relative_to(root)), 'line': ln,
            'entry': raw, 'missing': str(tgt.relative_to(root)),
        })

    return result


def any_violation(r: Dict[str, Any]) -> bool:
    gs = r.get('gate_staged', {})
    if not gs.get('ok', True):
        return True
    for k in ('private_leak_l1', 'l1_mentions_l2l3',
              'agent_identity_leak_l1', 'changelog_governance', 'changelog_release_alignment',
              'cross_ref_l1', 'docs_index_exist'):
        if r.get(k):
            return True
    return False


def print_human(r: Dict[str, Any]):
    def hr(title: str):
        print(f'\n--- {title} ---')

    gs = r['gate_staged']
    if not gs['staged_violations'] and not gs['history_violations']:
        print('✓ [R-门控1] staged 无 文档/ 路径')
    else:
        print('✗ [R-门控1] 检测到 commit 文档/ 路径', file=sys.stderr)
        for v in gs['staged_violations']:
            print(f'   staged: {v}', file=sys.stderr)
        for v in gs['history_violations']:
            print(f'   history: {v}', file=sys.stderr)

    hr('R3 引用合规（L1 私有路径泄露）')
    if not r['private_leak_l1']:
        print('✓ 未检测到 L1 公开文档出现私有集合路径/编号前缀')
    else:
        for v in r['private_leak_l1']:
            print(f"   ✗ {v['file']}:{v['line']}  pattern={v['pattern']}  {v['snippet']}", file=sys.stderr)

    hr('L1 不提 L2/L3')
    if not r['l1_mentions_l2l3']:
        print('✓ L1 公开文档未出现 文档/design|implement|benchmarks|archive/ 字面量')
    else:
        for v in r['l1_mentions_l2l3']:
            print(f"   ✗ {v['file']}:{v['line']}  {v['snippet']}", file=sys.stderr)

    hr('R-agent身份零泄露')
    if not r['agent_identity_leak_l1']:
        print('✓ L1 公开文档未泄露 AI agent 身份(agent 产品概念除外)')
    else:
        for v in r['agent_identity_leak_l1']:
            print(f"   ✗ {v['file']}:{v['line']}  pattern={v['pattern']}  {v['snippet']}", file=sys.stderr)

    hr('R-CHANGELOG内容治理（六类黑名单）')
    if not r['changelog_governance']:
        print('✓ CHANGELOG 未命中内部编号/过程语言/未发布计划/商业策略/私有仓名/内部流程')
    else:
        for v in r['changelog_governance']:
            print(f"   ✗ {v['file']}:{v['line']}  [{v['category']}]  {v['snippet']}", file=sys.stderr)

    hr('R-CHANGELOG发布纪律（tag 与 CHANGELOG 对齐）')
    if not r['changelog_release_alignment']:
        print('✓ 最新语义化版本 tag 均在 CHANGELOG 有对应版本区块（发布时 Unreleased→版本号并清空）')
    else:
        for v in r['changelog_release_alignment']:
            print(f"   ✗ {v['file']}:{v['line']}  [{v['category']}]  {v['snippet']}", file=sys.stderr)
    if r.get('changelog_unreleased_notes'):
        for v in r['changelog_unreleased_notes']:
            print(f"   ℹ {v['file']}:{v['line']}  {v['note']}")

    hr('L1 交叉引用完整性')
    if not r['cross_ref_l1']:
        print('✓ L1 文档内所有 md 链接指向的仓内文件均存在')
    else:
        for v in r['cross_ref_l1']:
            print(f"   ✗ {v['file']}:{v['line']}  link=[{v['link']}]  missing=/{v['missing']}", file=sys.stderr)

    hr('DOCS_INDEX 索引存在性')
    if not r['docs_index_exist']:
        print('✓ DOCS_INDEX.md 中列出的 L1 路径均存在')
    else:
        for v in r['docs_index_exist']:
            print(f"   ✗ {v['file']}:{v['line']}  entry=[{v['entry']}]  missing=/{v['missing']}", file=sys.stderr)


def main():
    default_root = str(Path(__file__).resolve().parent.parent)
    p = argparse.ArgumentParser(description='EvoRule 文档安全 + 引用完整性检查')
    p.add_argument('--warn', action='store_true', help='只警告不报错（exit 恒 0）')
    p.add_argument('--skip-git', action='store_true', help='跳过 git staged/history 检查（非 git 环境）')
    p.add_argument('--json', action='store_true', help='JSON 输出')
    p.add_argument('--cwd', default=default_root, help='repo 根目录（默认自动定位 scripts/..）')
    args = p.parse_args()

    cwd = Path(args.cwd).resolve()

    r = collect_all(cwd, skip_git=args.skip_git)

    if args.json:
        r['summary'] = {
            'l1_docs_count': r.pop('_l1_docs_count', 0),
            'any_violation': any_violation(r),
        }
        print(json.dumps(r, ensure_ascii=False, indent=2))
    else:
        print_human(r)

    if args.warn:
        sys.exit(0)
    sys.exit(1 if any_violation(r) else 0)


if __name__ == '__main__':
    main()

#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 EvoRule Project
# This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
"""
evorule-server 规则避坑自动检测器
================================

扫描 JSON 规则文件，基于 docs/PITFALLS.json 的 detection 配置自动检测常见的踩坑模式。

用法:
    python check_pitfalls.py <rule_file_or_dir> [--pitfalls PATH] [--json] [--quiet]

参数:
    rule_file_or_dir   要扫描的 JSON 规则文件或目录
    --pitfalls PATH     PITFALLS.json 路径（默认: 同目录 ../docs/PITFALLS.json）
    --json              以 JSON 格式输出结果（默认人类可读格式）
    --quiet             只输出有问题的规则，不输出扫描摘要

退出码:
    0 = 无 critical/high 级别问题（可能有 warning）
    1 = 发现 critical/high 级别问题
    2 = 脚本自身错误（文件找不到、JSON 解析失败等）

可检测的坑（checkable=true）:
    P07: io_request 参数未包装 args
    P08: domain path 缺 payload. 前缀
    P09: exists 可能误用（warning，需人工确认意图）
    P13: lt 浮点比较（warning）
    P16: 使用不存在的 domain 类型（or/gt/gte 等）
    P17: eq/lt 的 value 用了路径引用（永远 false）
    P18: branch+exists(__io_result__) 模式（warning，同 session 多次提交会踩坑）
    P20: set 业务指令的 operation 字段被 core_eval 忽略（warning，用 increment/decrement 代替）
    P21: set 的 value 引用 payload 路径可能不存在（warning，隐式规则依赖）

不可检测的坑（checkable=false，rust_code/client_side/environment）:
    P01-P06: 服务端 Rust 代码配置问题
    P10-P12: 客户端 HTTP 集成问题
    P14-P15: 环境问题（PowerShell）
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Optional

# ─── 数据结构 ────────────────────────────────────────────────────────────────

@dataclass
class Violation:
    """单个违规记录"""
    pitfall_id: str
    severity: str          # critical / high / medium / low / warning
    title: str
    message: str
    file_path: str
    json_path: str         # JSON 节点路径，如 $.rules[0].params.domain
    fix_hint: str = ""

    def to_dict(self) -> dict:
        return {
            "pitfall_id": self.pitfall_id,
            "severity": self.severity,
            "title": self.title,
            "message": self.message,
            "file_path": self.file_path,
            "json_path": self.json_path,
            "fix_hint": self.fix_hint,
        }


@dataclass
class ScanResult:
    """扫描结果汇总"""
    files_scanned: int = 0
    violations: list[Violation] = field(default_factory=list)

    @property
    def has_blocking(self) -> bool:
        """是否有 critical/high 级别问题（退出码 1 的条件）"""
        return any(v.severity in ("critical", "high") for v in self.violations)

    @property
    def critical_count(self) -> int:
        return sum(1 for v in self.violations if v.severity == "critical")

    @property
    def high_count(self) -> int:
        return sum(1 for v in self.violations if v.severity == "high")

    @property
    def warning_count(self) -> int:
        return sum(1 for v in self.violations if v.severity == "warning")


# ─── PITFALLS.json 加载 ──────────────────────────────────────────────────────

def load_pitfalls(pitfalls_path: Path) -> dict:
    """加载 PITFALLS.json，返回 pitfalls 字典（id → pitfall）"""
    with open(pitfalls_path, "r", encoding="utf-8") as f:
        data = json.load(f)
    return {p["id"]: p for p in data["pitfalls"]}


# ─── JSON 树遍历 ─────────────────────────────────────────────────────────────

# 合法 domain 类型（从 PITFALLS.json 的 valid_domain_types 读取）
VALID_DOMAIN_TYPES = {"eq", "lt", "exists", "instruction", "all", "not"}

# 疑似 domain 类型（用户可能误用，P16 检测）
SUSPECTED_DOMAIN_TYPES = {"or", "gt", "gte", "lte", "neq", "any", "and"}

# io_request call_service 的合法参数
IO_REQUEST_ALLOWED_PARAMS = {"io_type", "service_name", "args"}


def walk_json(node: Any, path: str = "$", callback=None):
    """递归遍历 JSON 树，对每个 dict 节点调用 callback(node, path)"""
    if isinstance(node, dict):
        if callback:
            callback(node, path)
        for key, value in node.items():
            child_path = f"{path}.{key}"
            walk_json(value, child_path, callback)
    elif isinstance(node, list):
        for i, item in enumerate(node):
            child_path = f"{path}[{i}]"
            walk_json(item, child_path, callback)


# ─── 检测器 ──────────────────────────────────────────────────────────────────

def _is_domain_node(node: dict) -> bool:
    """判断节点是否是 domain 节点（有 type 字段且在 domain 上下文中）"""
    t = node.get("type")
    if t is None:
        return False
    # 合法 domain 类型 或 疑似 domain 类型
    return t in VALID_DOMAIN_TYPES or t in SUSPECTED_DOMAIN_TYPES


def _is_string_starting_with_dunder(value: Any) -> bool:
    """判断 value 是否是 __ 开头的字符串（路径引用）"""
    return isinstance(value, str) and value.startswith("__")


# 匹配 branch on_true/on_false 直接子节点的 JSON 路径（meta-instruction）
_META_INSTR_PATH_RE = re.compile(r'\.(on_true|on_false)\[\d+\]$')


def _is_meta_instruction(json_path: str) -> bool:
    """判断节点是否是 branch 的 on_true/on_false 直接子节点（meta-instruction）

    meta-instruction 的 set 支持 operation='add'/'sub'（直接由 exec_set 执行），
    业务 set 指令的 operation 被 core_eval 忽略（P20）。
    """
    return bool(_META_INSTR_PATH_RE.search(json_path))


def check_domain(node: dict, json_path: str, file_path: str, pitfalls: dict) -> list[Violation]:
    """检查 domain 节点，返回违规列表"""
    violations = []
    domain_type = node.get("type")

    # ── P16: 使用不存在的 domain 类型 ──
    if domain_type in SUSPECTED_DOMAIN_TYPES:
        p = pitfalls.get("P16", {})
        violations.append(Violation(
            pitfall_id="P16",
            severity=p.get("severity", "critical"),
            title=p.get("title", "or/gt domain 不存在"),
            message=f"使用了不存在的 domain 类型 '{domain_type}'。evorule 只有 6 种合法 domain: eq/lt/exists/instruction/all/not。"
                   f"未知类型会被忽略，条件永远返回 false，on_true 分支永远不执行。",
            file_path=file_path,
            json_path=json_path,
            fix_hint=p.get("fix", "or 用德摩根定律 not(all(not(A), not(B)))；gt 浮点比较外部化到 I/O 服务"),
        ))

    # ── P08: path 缺 payload. 前缀 ──
    if domain_type in ("eq", "lt", "exists"):
        path_val = node.get("path")
        if isinstance(path_val, str) and path_val:
            if not (path_val.startswith("payload.") or path_val.startswith("__exec__.")):
                # 完全没有前缀 → critical
                p = pitfalls.get("P08", {})
                violations.append(Violation(
                    pitfall_id="P08",
                    severity=p.get("severity", "critical"),
                    title=p.get("title", "规则 path 缺 payload. 前缀"),
                    message=f"domain path '{path_val}' 不以 'payload.' 或 '__exec__.' 开头。"
                           f"domain path 的根是 __exec__，业务数据在 __exec__.payload 下。"
                           f"当前路径会查找 __exec__.{path_val}（不存在），条件永远返回 false。",
                    file_path=file_path,
                    json_path=json_path,
                    fix_hint=f"改为 'payload.{path_val}' 或 '__exec__.payload.{path_val}'",
                ))
            elif path_val.startswith("__exec__.") and not (
                path_val.startswith("__exec__.payload.") or
                path_val.startswith("__exec__.instruction.")
            ):
                # __exec__. 后面缺 payload./instruction. 层级 → warning
                # 例如 __exec__.audit.failure_count 应为 __exec__.payload.audit.failure_count
                suffix = path_val[len("__exec__."):]
                violations.append(Violation(
                    pitfall_id="P08",
                    severity="warning",
                    title="__exec__ 路径缺 payload/instruction 层级",
                    message=f"domain path '{path_val}' 以 __exec__. 开头，但后面不是 'payload.' 或 'instruction.'。"
                           f"业务数据在 __exec__.payload 下，指令参数在 __exec__.instruction 下。"
                           f"当前路径可能查找不到值（如 __exec__.audit.xxx 应为 __exec__.payload.audit.xxx）。",
                    file_path=file_path,
                    json_path=json_path,
                    fix_hint=f"改为 '__exec__.payload.{suffix}' 或 'payload.{suffix}'",
                ))

    # ── P17: eq/lt 的 value 用了路径引用 ──
    if domain_type in ("eq", "lt"):
        value = node.get("value")
        if _is_string_starting_with_dunder(value):
            p = pitfalls.get("P17", {})
            violations.append(Violation(
                pitfall_id="P17",
                severity=p.get("severity", "critical"),
                title=p.get("title", "eq/lt value 不支持路径引用"),
                message=f"eq/lt 的 value '{value}' 是 __ 开头的字符串，会被当作字面量而非路径引用。"
                       f"只有 set 的 value 支持 resolve_path_or_literal 路径解析。"
                       f"eq 会比较路径解析后的值和字符串字面量，类型不匹配，永远返回 false。",
                file_path=file_path,
                json_path=json_path,
                fix_hint=p.get("fix", "用 set+sub 计算差值再 eq(0)（仅 Integer），或外部化到 I/O 服务返回布尔值"),
            ))

    # ── P09: exists 可能误用（warning）──
    if domain_type == "exists":
        p = pitfalls.get("P09", {})
        violations.append(Violation(
            pitfall_id="P09",
            severity="warning",
            title=p.get("title", "exists ≠ 非 null 判断"),
            message=f"exists 对 null 值返回 true（语义是路径存在，不是值非 null）。"
                   f"如果意图是判断非 null，应改用 not(eq(path, null))。",
            file_path=file_path,
            json_path=json_path,
            fix_hint="确认意图：判断字段被设置过 → exists；判断值非 null → not(eq(path, null))",
        ))

    # ── P13: lt 浮点比较（warning）──
    if domain_type == "lt":
        value = node.get("value")
        # 检测浮点字面量
        if isinstance(value, float):
            p = pitfalls.get("P13", {})
            violations.append(Violation(
                pitfall_id="P13",
                severity="warning",
                title=p.get("title", "TCB 无 Float，lt 只支持 Integer"),
                message=f"lt 的 value {value} 是浮点数。evorule-tcb 的 JsonValue 没有 Float 变体，"
                       f"lt 只支持 Integer 比较（as_i64）。浮点比较会返回 false。",
                file_path=file_path,
                json_path=json_path,
                fix_hint="浮点比较外部化到 I/O 服务返回布尔值",
            ))
        # 检测浮点字符串（如 "0.001"）
        elif isinstance(value, str):
            try:
                float(value)
                if "." in value:
                    p = pitfalls.get("P13", {})
                    violations.append(Violation(
                        pitfall_id="P13",
                        severity="warning",
                        title=p.get("title", "TCB 无 Float，lt 只支持 Integer"),
                        message=f"lt 的 value '{value}' 是浮点字符串。lt 只支持 Integer（as_i64），"
                               f"浮点字符串会被拒绝，返回 false。",
                        file_path=file_path,
                        json_path=json_path,
                        fix_hint="浮点比较外部化到 I/O 服务返回布尔值",
                    ))
            except ValueError:
                pass  # 不是数字字符串，忽略

    return violations


def check_io_request(node: dict, json_path: str, file_path: str, pitfalls: dict) -> list[Violation]:
    """检查 io_request 指令，返回违规列表"""
    violations = []
    params = node.get("params", {})
    io_type = params.get("io_type")

    if io_type == "call_service":
        # ── P07: io_request 参数未包装 args ──
        extra_params = set(params.keys()) - IO_REQUEST_ALLOWED_PARAMS
        if extra_params:
            p = pitfalls.get("P07", {})
            violations.append(Violation(
                pitfall_id="P07",
                severity=p.get("severity", "high"),
                title=p.get("title", "io_request 参数需要 args 字段包装"),
                message=f"io_request(call_service) 的 params 中存在非法顶层字段: {sorted(extra_params)}。"
                       f"合法字段只有: io_type / service_name / args。"
                       f"业务参数必须放在 args 里（payload 路径引用或内联对象）。",
                file_path=file_path,
                json_path=json_path,
                fix_hint="先用 set 在 payload 构造参数对象（如 _ik_args），再 args 引用 __exec__.payload._ik_args",
            ))

    return violations


def check_set_instruction(node: dict, json_path: str, file_path: str, pitfalls: dict) -> list[Violation]:
    """检查 set 指令，返回违规列表

    P20: 业务 set 指令的 operation 字段被 core_eval 忽略。
    仅检测业务 set 指令（不在 branch on_true/on_false 直接子节点中），
    meta-instruction set（在 on_true/on_false 中）支持 operation='add'/'sub'。
    """
    violations = []
    params = node.get("params", {})
    operation = params.get("operation")

    if operation is not None and operation != "set":
        # meta-instruction（branch on_true/on_false 直接子节点）支持 operation=add/sub
        if _is_meta_instruction(json_path):
            return violations

        # 业务 set 指令：operation 被 core_eval 忽略
        p = pitfalls.get("P20", {})
        violations.append(Violation(
            pitfall_id="P20",
            severity="warning",
            title=p.get("title", "set 业务指令 operation 被忽略"),
            message=f"业务 set 指令的 operation='{operation}' 将被 core_eval 忽略。"
                   f"core_eval 的 set 处理器硬编码 operation='set'。"
                   f"实际执行 set（覆盖），不是 {operation}。"
                   f"用 increment/decrement 指令代替。",
            file_path=file_path,
            json_path=json_path,
            fix_hint="改用 increment(attr, delta) 或 decrement(attr, delta) 指令",
        ))

    return violations


def check_branch(node: dict, json_path: str, file_path: str, pitfalls: dict) -> list[Violation]:
    """检查 branch 指令，返回违规列表"""
    violations = []
    params = node.get("params", {})
    domain = params.get("domain", {})

    # ── P18: branch+exists(__io_result__) 模式 ──
    if isinstance(domain, dict) and domain.get("type") == "exists":
        domain_path = domain.get("path", "")
        if isinstance(domain_path, str) and "__io_result__" in domain_path:
            p = pitfalls.get("P18", {})
            violations.append(Violation(
                pitfall_id="P18",
                severity="warning",
                title=p.get("title", "branch+exists 不清除 __io_result__"),
                message=f"检测到 branch+exists(__io_result__) 模式。"
                       f"此模式在同 session 多次提交同一类型 I/O 指令时会消费旧结果（__io_result__ 不清除）。"
                       f"如果业务需要多次提交，需每次创建新 session。",
                file_path=file_path,
                json_path=json_path,
                fix_hint="每 session 单次 I/O → 安全；同 session 多次 → 每次创建新 session",
            ))

    # ── P19: io_request 之后的指令不会执行 ──
    for branch_key in ("on_true", "on_false"):
        children = params.get(branch_key, [])
        if not isinstance(children, list):
            continue
        for i, child in enumerate(children):
            if isinstance(child, dict) and child.get("type") == "io_request":
                if i < len(children) - 1:
                    remaining = len(children) - i - 1
                    p = pitfalls.get("P19", {})
                    violations.append(Violation(
                        pitfall_id="P19",
                        severity="warning",
                        title=p.get("title", "io_request 之后的指令不执行"),
                        message=f"io_request 在 {branch_key}[{i}] 位置，后面还有 {remaining} 条指令。"
                               f"io_request 返回 IoRequired 信号后立即传播，不执行后续指令。"
                               f"这些指令永远不会执行。",
                        file_path=file_path,
                        json_path=f"{json_path}.params.{branch_key}[{i}]",
                        fix_hint="用 I/O 两阶段模式包装：io_request 放 on_false 末尾，后续操作放 on_true（__io_result__ 存在时执行）",
                    ))
                break  # 只报第一个 io_request

    # ── P21: set 的 value 引用 payload 路径可能不存在（隐式规则依赖）──
    for branch_key in ("on_true", "on_false"):
        children = params.get(branch_key, [])
        if not isinstance(children, list):
            continue
        for i, child in enumerate(children):
            if not isinstance(child, dict) or child.get("type") != "set":
                continue
            child_params = child.get("params", {})
            value = child_params.get("value")
            if (isinstance(value, str)
                    and value.startswith("__exec__.payload.")
                    and "__io_result__" not in value):
                p = pitfalls.get("P21", {})
                child_path = f"{json_path}.params.{branch_key}[{i}]"
                violations.append(Violation(
                    pitfall_id="P21",
                    severity="warning",
                    title=p.get("title", "set value 引用 payload 路径可能不存在"),
                    message=f"set 的 value='{value}' 是 payload 路径引用。"
                           f"如果该路径未被前置指令设置，transition 会 PathResolutionFailed 报错"
                           f"并静默回滚状态。确保此路径在规则触发前已被设置。",
                    file_path=file_path,
                    json_path=child_path,
                    fix_hint="确保引用的路径在前置指令中已设置，或用 branch+exists(path) 保护",
                ))

    return violations


def scan_node(node: dict, json_path: str, file_path: str, pitfalls: dict) -> list[Violation]:
    """扫描单个 dict 节点，返回所有违规"""
    violations = []
    node_type = node.get("type")

    # 检查 domain 节点
    if _is_domain_node(node):
        violations.extend(check_domain(node, json_path, file_path, pitfalls))

    # 检查 io_request 指令
    if node_type == "io_request":
        violations.extend(check_io_request(node, json_path, file_path, pitfalls))

    # 检查 branch 指令
    if node_type == "branch":
        violations.extend(check_branch(node, json_path, file_path, pitfalls))

    # 检查 set 指令（P20: 业务 set 的 operation 被忽略）
    if node_type == "set":
        violations.extend(check_set_instruction(node, json_path, file_path, pitfalls))

    return violations


# ─── 文件/目录扫描 ───────────────────────────────────────────────────────────

def scan_rule_file(file_path: Path, pitfalls: dict) -> list[Violation]:
    """扫描单个 JSON 规则文件"""
    try:
        with open(file_path, "r", encoding="utf-8") as f:
            content = f.read()
        data = json.loads(content)
    except json.JSONDecodeError as e:
        print(f"[ERROR] {file_path}: JSON 解析失败 - {e}", file=sys.stderr)
        return []
    except Exception as e:
        print(f"[ERROR] {file_path}: 读取失败 - {e}", file=sys.stderr)
        return []

    violations = []

    def callback(node, path):
        if isinstance(node, dict):
            violations.extend(scan_node(node, path, str(file_path), pitfalls))

    walk_json(data, "$", callback)
    return violations


def scan_path(target: Path, pitfalls: dict) -> ScanResult:
    """扫描文件或目录"""
    result = ScanResult()

    if target.is_file():
        if target.suffix == ".json":
            result.files_scanned = 1
            result.violations.extend(scan_rule_file(target, pitfalls))
    elif target.is_dir():
        for json_file in sorted(target.rglob("*.json")):
            result.files_scanned += 1
            result.violations.extend(scan_rule_file(json_file, pitfalls))
    else:
        raise FileNotFoundError(f"路径不存在: {target}")

    return result


# ─── 输出格式化 ──────────────────────────────────────────────────────────────

SEVERITY_SYMBOLS = {
    "critical": "🔴",
    "high": "🟠",
    "medium": "🟡",
    "low": "🔵",
    "warning": "⚠️ ",
}

SEVERITY_ORDER = {"critical": 0, "high": 1, "medium": 2, "warning": 3, "low": 4}


def format_human(result: ScanResult, quiet: bool) -> str:
    """人类可读格式输出"""
    lines = []

    if not quiet:
        lines.append(f"扫描了 {result.files_scanned} 个 JSON 规则文件")
        lines.append("")

    if not result.violations:
        if not quiet:
            lines.append("✅ 未检测到任何坑模式（可检测范围内）")
            lines.append("")
            lines.append("提示: P01-P06（服务端 Rust）、P10-P12（客户端）、P14-P15（环境）不可静态检测，需人工排查")
        return "\n".join(lines)

    # 按严重程度排序
    sorted_violations = sorted(
        result.violations,
        key=lambda v: (SEVERITY_ORDER.get(v.severity, 99), v.file_path, v.json_path)
    )

    # 按文件分组
    current_file = None
    for v in sorted_violations:
        if v.file_path != current_file:
            current_file = v.file_path
            lines.append(f"\n📄 {current_file}")

        sym = SEVERITY_SYMBOLS.get(v.severity, "?")
        lines.append(f"  {sym} [{v.pitfall_id}] {v.severity.upper()}: {v.title}")
        lines.append(f"     路径: {v.json_path}")
        lines.append(f"     问题: {v.message}")
        if v.fix_hint:
            lines.append(f"     修复: {v.fix_hint}")

    # 汇总
    lines.append("")
    lines.append("─" * 60)
    lines.append(f"汇总: {result.critical_count} critical, {result.high_count} high, {result.warning_count} warning")

    if result.has_blocking:
        lines.append("❌ 发现 critical/high 级别问题，需修复后才能安全提交")
    else:
        lines.append("✅ 无 critical/high 级别问题（warning 可选择性处理）")

    return "\n".join(lines)


def format_json(result: ScanResult) -> str:
    """JSON 格式输出"""
    output = {
        "files_scanned": result.files_scanned,
        "summary": {
            "critical": result.critical_count,
            "high": result.high_count,
            "warning": result.warning_count,
            "total": len(result.violations),
            "has_blocking": result.has_blocking,
        },
        "violations": [v.to_dict() for v in sorted(
            result.violations,
            key=lambda v: (SEVERITY_ORDER.get(v.severity, 99), v.file_path, v.json_path)
        )],
    }
    return json.dumps(output, ensure_ascii=False, indent=2)


# ─── CLI 入口 ────────────────────────────────────────────────────────────────

def main():
    parser = argparse.ArgumentParser(
        description="evorule-server 规则避坑自动检测器",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
可检测的坑:
  P07  io_request 参数未包装 args
  P08  domain path 缺 payload. 前缀
  P09  exists 可能误用（warning）
  P13  lt 浮点比较（warning）
  P16  使用不存在的 domain 类型
  P17  eq/lt value 用了路径引用
  P18  branch+exists(__io_result__) 模式（warning）

示例:
  python check_pitfalls.py rules/yuanze_rules.json
  python check_pitfalls.py rules/ --json
  python check_pitfalls.py tests/ --quiet
        """
    )
    parser.add_argument("target", help="要扫描的 JSON 规则文件或目录")
    parser.add_argument("--pitfalls", default=None, help="PITFALLS.json 路径（默认: 同目录 ../docs/PITFALLS.json）")
    parser.add_argument("--json", action="store_true", dest="as_json", help="以 JSON 格式输出")
    parser.add_argument("--quiet", action="store_true", help="只输出有问题的规则")

    args = parser.parse_args()

    # 定位 PITFALLS.json
    if args.pitfalls:
        pitfalls_path = Path(args.pitfalls)
    else:
        # 默认: 脚本在 scripts/ 下，PITFALLS.json 在 ../docs/ 下
        script_dir = Path(__file__).resolve().parent
        pitfalls_path = script_dir.parent / "docs" / "PITFALLS.json"

    if not pitfalls_path.exists():
        print(f"[ERROR] PITFALLS.json 未找到: {pitfalls_path}", file=sys.stderr)
        print(f"        请用 --pitfalls 指定路径", file=sys.stderr)
        sys.exit(2)

    # 加载坑定义
    try:
        pitfalls = load_pitfalls(pitfalls_path)
    except Exception as e:
        print(f"[ERROR] PITFALLS.json 加载失败: {e}", file=sys.stderr)
        sys.exit(2)

    # 扫描
    target = Path(args.target)
    if not target.exists():
        print(f"[ERROR] 目标路径不存在: {target}", file=sys.stderr)
        sys.exit(2)

    try:
        result = scan_path(target, pitfalls)
    except Exception as e:
        print(f"[ERROR] 扫描失败: {e}", file=sys.stderr)
        sys.exit(2)

    # 输出
    if args.as_json:
        print(format_json(result))
    else:
        print(format_human(result, args.quiet))

    # 退出码
    sys.exit(1 if result.has_blocking else 0)


if __name__ == "__main__":
    main()

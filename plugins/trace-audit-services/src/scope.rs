// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! M4 面：写类工具路径集合 vs 任务范围根的区间判定（范围外写入检出）。
//!
//! # 判据正本
//! 离线分析器工具 `m4_deliverable_drift`（同输入同输出对账口径）：
//! - `written_paths` = `ToolCall` 事件中写类工具（值域可扩展，缺省五件）
//!   携带的写路径（`args` 的 `path`/`file`/`target` 三键按序取首个非空字符串，
//!   与正本 Python `or` 链的短路语义一致）；
//! - `out_of_scope` = 给了 scope 根时，写路径归一后不在根区间内的路径；
//!   归一失败（正本 OSError/ValueError：嵌 NUL 等非法形态）一律判范围外。
//!
//! # 服务面诚实边界（与正本的显式差口，非漂移）
//! - **零 FS / 零 CWD**：正本 `Path.resolve()` 依赖进程 CWD（相对路径与
//!   每盘符 CWD 解析）；服务面为纯文本词典法归一——相对写路径按显式
//!   `base_dir` 入参解析（缺省 = scope 根；对账样本场景 CWD == scope，
//!   宿主 CWD ≠ scope 时须显式传 `base_dir`）；**每盘符相对形态**
//!   （`C:foo` 无根）正本依赖进程每盘符 CWD，服务面不可判定 → 写路径
//!   判范围外、scope 入参直接拒解（fail-visible），两侧口径差口如实声明；
//! - **不解析符号链接**：正本 `resolve()` 会沿真实文件系统折叠链接，
//!   服务面只做词典法归一（journal 写路径为执行体落账字面量，非链接形态）；
//! - **大小写口径**：盘符/UNC 形态按 Windows 语义大小写不敏感（与正本
//!   `WindowsPath` 一致），POSIX 绝对形态大小写敏感（与 `POSIXPath` 一致）；
//! - 事件数组资源守卫同家族口径（超限 fail-visible 拒解）。

use evorule_tcb::JsonValue;

/// 写类工具缺省值域（与判据正本 WRITE_TOOLS 逐名一致；正本注明写型工具
/// 名值域可按 agent 定义扩展，服务面经 `write_tools` 入参扩展）。
pub const DEFAULT_WRITE_TOOLS: &[&str] = &[
    "file_write",
    "file_edit",
    "apply_patch",
    "write_file",
    "edit_file",
];

// ============================================================================
// 词典法路径模型（零 FS/零 CWD）
// ============================================================================

/// 路径形态：归一后组件列表 + 前缀种类。
#[derive(Debug, Clone, PartialEq, Eq)]
enum LexKind {
    /// 盘符绝对：`X:/a/b`（大小写不敏感比对）
    Drive(String),
    /// UNC：`//server/share/a`（大小写不敏感比对）
    Unc,
    /// POSIX 绝对：`/a/b`（大小写敏感比对）
    PosixAbs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LexPath {
    kind: LexKind,
    comps: Vec<String>,
}

impl LexPath {
    /// 前缀包含判定（正本 `is_relative_to` 语义：相等亦算内）。
    fn is_within(&self, root: &LexPath) -> bool {
        // 形态不同（盘符 vs POSIX vs UNC）= 不同命名空间，必不在根内
        if !match (&self.kind, &root.kind) {
            (LexKind::Drive(a), LexKind::Drive(b)) => a.eq_ignore_ascii_case(b),
            (LexKind::Unc, LexKind::Unc) | (LexKind::PosixAbs, LexKind::PosixAbs) => true,
            _ => false,
        } {
            return false;
        }
        if self.comps.len() < root.comps.len() {
            return false;
        }
        let case_insensitive = !matches!(self.kind, LexKind::PosixAbs);
        self.comps.iter().zip(root.comps.iter()).all(|(a, b)| {
            if case_insensitive {
                a.eq_ignore_ascii_case(b)
            } else {
                a == b
            }
        })
    }
}

/// 词典法归一（零 FS）：分隔符统一、剥扩展长度前缀、解析 `.`/`..`。
/// `base` 供相对路径拼接。返回 None = 无法词典解析（裸 NUL/每盘符相对
/// 形态等，调用侧按正本 OSError/ValueError 口径判范围外/拒解）。
fn lex_normalize(raw: &str, base: Option<&LexPath>) -> Option<LexPath> {
    if raw.contains('\0') {
        return None; // 正本 ValueError
    }
    let mut s = raw.replace('\\', "/");
    // 剥 Windows 扩展长度前缀（正本 pathlib resolve 输出剥 `\\?\`）
    if let Some(rest) = s.strip_prefix("//?/UNC/") {
        s = format!("//{rest}");
    } else if let Some(rest) = s.strip_prefix("//?/") {
        s = rest.to_string();
    }
    if let Some(body) = s.strip_prefix("//") {
        // UNC：`//server/share` 两段为前缀锚
        let mut it = body.splitn(3, '/');
        let (host, share) = match (it.next(), it.next()) {
            (Some(h), Some(sh)) if !h.is_empty() && !sh.is_empty() => (h, sh),
            _ => return None,
        };
        let mut comps = vec![host.to_string(), share.to_string()];
        comps.extend(lex_rel_comps(it.next().unwrap_or("")));
        return Some(LexPath {
            kind: LexKind::Unc,
            comps,
        });
    }
    if let Some(rest) = s.strip_prefix('/') {
        // POSIX 绝对（`..` 在根处停留：正本 Path('/..').resolve() == '/'）
        return Some(LexPath {
            kind: LexKind::PosixAbs,
            comps: lex_rel_comps(rest),
        });
    }
    // 盘符形态：`X:/...` / `X:` / `X:foo`（盘符限单字母，ntpath 口径）
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        let drive = s[..1].to_ascii_uppercase();
        if let Some(rest) = s[2..].strip_prefix('/') {
            return Some(LexPath {
                kind: LexKind::Drive(drive),
                comps: lex_rel_comps(rest),
            });
        }
        if s.len() == 2 {
            return Some(LexPath {
                kind: LexKind::Drive(drive),
                comps: vec![],
            });
        }
        // `X:foo` 每盘符相对：正本依赖进程每盘符 CWD，服务面不可判定
        return None;
    }
    // 相对路径：拼 base 后解析（`..` 直接作用在 base 组件上）
    let base = base?;
    let mut comps = base.comps.clone();
    apply_rel_comps(&mut comps, &s);
    Some(LexPath {
        kind: base.kind.clone(),
        comps,
    })
}

/// 相对组件解析：`.` 跳过、`..` 上弹（根处停留：弹空列表=停留，与正本
/// resolve 的根/相对底语义一致）、空段折叠。
fn apply_rel_comps(comps: &mut Vec<String>, rest: &str) {
    for seg in rest.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                comps.pop();
            }
            other => comps.push(other.to_string()),
        }
    }
}

fn lex_rel_comps(rest: &str) -> Vec<String> {
    let mut comps = Vec::new();
    apply_rel_comps(&mut comps, rest);
    comps
}

// ============================================================================
// 判据实现（正本 m4_deliverable_drift 同输入同输出）
// ============================================================================

/// 正本口径取写路径：`path`/`file`/`target` 三键按序取首个非空字符串
///（Python `or` 链短路语义：空串与非字符串都跳过）。
fn pick_write_path(args_obj: &JsonValue) -> Option<String> {
    for key in ["path", "file", "target"] {
        if let Some(JsonValue::String(s)) = args_obj.get(key) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// 解析 write_tools 入参（缺省五件；值域可按 agent 定义扩展，正本口径）。
fn parse_write_tools(args: &JsonValue) -> Result<Vec<String>, String> {
    match args.get("write_tools") {
        None | Some(JsonValue::Null) => Ok(DEFAULT_WRITE_TOOLS
            .iter()
            .map(|s| (*s).to_string())
            .collect()),
        Some(JsonValue::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                match it.as_str() {
                    Some(s) if !s.is_empty() => out.push(s.to_string()),
                    _ => {
                        return Err("write_tools 须为非空字符串数组。自诊断指引:写型工具名值域\
                             可按 agent 定义扩展,缺省 = file_write/file_edit/apply_patch/\
                             write_file/edit_file"
                            .to_string());
                    }
                }
            }
            Ok(out)
        }
        Some(_) => Err("write_tools 须为字符串数组。自诊断指引:缺省五件写型工具,\
                        扩展值域传数组覆盖"
            .to_string()),
    }
}

/// 解析 scope/base_dir 入参：scope 缺省 None（不检出只收集）；base_dir
/// 供相对写路径解析，缺省 = scope 根。全部要求词典可解析的绝对形态
///（相对 scope 须带 base_dir 拼接；每盘符相对形态拒解）。
struct ScopeResolved {
    root: Option<LexPath>,
    base: Option<LexPath>,
}

fn resolve_scope_args(args: &JsonValue) -> Result<ScopeResolved, String> {
    let base_dir_raw = match args.get("base_dir") {
        None | Some(JsonValue::Null) => None,
        Some(JsonValue::String(s)) => Some(s.to_string()),
        Some(_) => {
            return Err("base_dir 须为字符串。自诊断指引:相对写路径的解析基准目录,\
                        传绝对路径;缺省 = scope 根"
                .to_string())
        }
    };
    let scope_raw = match args.get("scope") {
        None | Some(JsonValue::Null) => None,
        Some(JsonValue::String(s)) => Some(s.to_string()),
        Some(_) => {
            return Err("scope 须为字符串。自诊断指引:任务范围根目录,传绝对路径;\
                        缺省 None = 只收集写路径不检出"
                .to_string())
        }
    };
    // base_dir 必须词典可解析为绝对形态
    let base = match &base_dir_raw {
        Some(b) => Some(lex_normalize(b, None).ok_or_else(|| {
            format!(
                "base_dir 不可解析为绝对路径: {b}。自诊断指引:服务面零 CWD,相对路径\
                 无法凭空解析;须传绝对形态（含盘符/POSIX 根/UNC）"
            )
        })?),
        None => None,
    };
    let root = match &scope_raw {
        None => None,
        Some(sc) => Some(lex_normalize(sc, base.as_ref()).ok_or_else(|| {
            format!(
                "scope 不可解析为绝对路径: {sc}。自诊断指引:①相对 scope 须同时传\
                 绝对 base_dir 供拼接;②每盘符相对形态（如 C:foo）正本依赖进程每盘符\
                 CWD,服务面拒解（诚实化差口,见服务文档）"
            )
        })?),
    };
    // base 缺省 = scope 根
    let base = base.or_else(|| root.clone());
    Ok(ScopeResolved { root, base })
}

/// M4 判据出参（正本 `m4_deliverable_drift` 同键集同形态）。
pub fn out_of_scope_writes_json(
    events: &[JsonValue],
    args: &JsonValue,
) -> Result<JsonValue, String> {
    let write_tools = parse_write_tools(args)?;
    let resolved = resolve_scope_args(args)?;
    let mut written_paths: Vec<JsonValue> = Vec::new();
    let mut out_of_scope: Vec<JsonValue> = Vec::new();
    for ev in events {
        let is_tool_call = ev.get("type").and_then(JsonValue::as_str) == Some("ToolCall");
        if !is_tool_call {
            continue;
        }
        let name = ev.get("name").and_then(JsonValue::as_str).unwrap_or("?");
        if !write_tools.iter().any(|t| t == name) {
            continue;
        }
        // args 可能在事件顶层 `args` 键,也可能同名字段散布(正本 ev.get("args"))
        let Some(args_obj) = ev.get("args") else {
            continue;
        };
        let Some(p) = pick_write_path(args_obj) else {
            continue;
        };
        written_paths.push(JsonValue::string(&p));
        if let Some(root) = &resolved.root {
            let resolved_path = lex_normalize(&p, resolved.base.as_ref());
            let inside = resolved_path
                .as_ref()
                .map(|lp| lp.is_within(root))
                .unwrap_or(false); // 归一失败 = 正本 OSError/ValueError 判范围外
            if !inside {
                out_of_scope.push(JsonValue::string(&p));
            }
        }
    }
    Ok(JsonValue::object_from_pairs(&[
        ("written_paths", JsonValue::Array(written_paths)),
        ("out_of_scope", JsonValue::Array(out_of_scope)),
    ]))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn s(v: &str) -> JsonValue {
        JsonValue::string(v)
    }

    /// 构造写类 ToolCall 事件。
    fn write_call(tool: &str, path: &str) -> JsonValue {
        JsonValue::object_from_pairs(&[
            ("type", s("ToolCall")),
            ("name", s(tool)),
            ("args", JsonValue::object_from_pairs(&[("path", s(path))])),
        ])
    }

    fn run(events: Vec<JsonValue>, extra: &[(&str, JsonValue)]) -> JsonValue {
        let args = JsonValue::object_from_pairs(extra);
        out_of_scope_writes_json(&events, &args).expect("判据计算应成功")
    }

    // ---- 正本口径：收集与三键 or 链 ----

    #[test]
    fn test_collect_paths_and_default_tools() {
        let events = vec![
            write_call("file_write", r"D:\ws\a.txt"),
            write_call("shell_exec", r"D:\ws\b.txt"), // 非写类工具 → 不收集
            JsonValue::object_from_pairs(&[
                ("type", s("ToolCall")),
                ("name", s("write_file")),
                (
                    "args",
                    JsonValue::object_from_pairs(&[("file", s("c.txt"))]),
                ), // path 缺 → file
            ]),
            JsonValue::object_from_pairs(&[
                ("type", s("ToolCall")),
                ("name", s("edit_file")),
                (
                    "args",
                    JsonValue::object_from_pairs(&[("path", s("")), ("target", s("d.txt"))]),
                ), // 空串跳过 → target
            ]),
            write_call("read", r"D:\ws\e.txt"), // 只读工具 → 不收集
        ];
        let out = run(events, &[]);
        let paths: Vec<String> = out
            .get("written_paths")
            .and_then(JsonValue::as_array)
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(paths, vec![r"D:\ws\a.txt", "c.txt", "d.txt"]);
    }

    #[test]
    fn test_non_string_and_missing_args_skipped() {
        let events = vec![
            JsonValue::object_from_pairs(&[
                ("type", s("ToolCall")),
                ("name", s("file_write")),
                (
                    "args",
                    JsonValue::object_from_pairs(&[("path", JsonValue::Integer(7))]),
                ), // 非字符串 → 跳过
            ]),
            JsonValue::object_from_pairs(&[
                ("type", s("ToolCall")),
                ("name", s("file_write")),
                ("args", JsonValue::object_from_pairs(&[])), // 无路径键 → 跳过
            ]),
        ];
        let out = run(events, &[]);
        assert_eq!(
            out.get("written_paths")
                .and_then(JsonValue::as_array)
                .unwrap()
                .len(),
            0
        );
    }

    // ---- 区间判定 ----

    #[test]
    fn test_out_of_scope_detection() {
        let events = vec![
            write_call("file_write", r"D:\ws\in.txt"),
            write_call("file_write", r"D:\other\out.txt"),
            write_call("file_write", r"D:\ws\sub\deep.txt"),
        ];
        let out = run(events, &[("scope", s(r"D:\ws"))]);
        let oos: Vec<String> = out
            .get("out_of_scope")
            .and_then(JsonValue::as_array)
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(oos, vec![r"D:\other\out.txt"]);
    }

    /// 相对路径按 base_dir 解析;`..` 穿越出根 = 范围外(词典法,与正本
    /// resolve 后 is_relative_to 判定一致)。
    #[test]
    fn test_relative_paths_via_base_dir_and_dotdot_escape() {
        let events = vec![
            write_call("file_write", "in.txt"),
            write_call("file_write", r"sub\in2.txt"),
            write_call("file_write", r"..\..\escape.txt"),
        ];
        let out = run(
            events,
            &[("scope", s(r"D:\ws")), ("base_dir", s(r"D:\ws\run1"))],
        );
        let oos: Vec<String> = out
            .get("out_of_scope")
            .and_then(JsonValue::as_array)
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(oos, vec![r"..\..\escape.txt"]);
    }

    /// base_dir 缺省 = scope 根（对账样本场景 CWD == scope 的等价形态）。
    #[test]
    fn test_base_dir_defaults_to_scope_root() {
        let events = vec![write_call("file_write", "in.txt")];
        let out = run(events, &[("scope", s(r"D:\ws"))]);
        assert_eq!(
            out.get("out_of_scope")
                .and_then(JsonValue::as_array)
                .unwrap()
                .len(),
            0
        );
    }

    /// 盘符形态大小写不敏感（WindowsPath 语义）。
    #[test]
    fn test_drive_case_insensitive() {
        let events = vec![write_call("file_write", r"D:\WS\In.TXT")];
        let out = run(events, &[("scope", s(r"d:\ws"))]);
        assert_eq!(
            out.get("out_of_scope")
                .and_then(JsonValue::as_array)
                .unwrap()
                .len(),
            0
        );
    }

    /// 路径恰等于 scope 根 = 在内（is_relative_to 相等亦真）。
    #[test]
    fn test_path_equal_to_root_is_inside() {
        let events = vec![write_call("file_write", r"D:\ws")];
        let out = run(events, &[("scope", s(r"D:\ws\"))]);
        assert_eq!(
            out.get("out_of_scope")
                .and_then(JsonValue::as_array)
                .unwrap()
                .len(),
            0
        );
    }

    /// 归一失败（嵌 NUL = 正本 ValueError 口径）判范围外。
    #[test]
    fn test_unresolvable_path_judged_out_of_scope() {
        let events = vec![write_call("file_write", "bad\0path.txt")];
        let out = run(events, &[("scope", s(r"D:\ws"))]);
        let oos = out
            .get("out_of_scope")
            .and_then(JsonValue::as_array)
            .unwrap();
        assert_eq!(oos.len(), 1);
    }

    /// 前缀相邻目录不算内：`D:\ws2` 不是 `D:\ws` 的子树（组件级比对,
    /// 非字符串前缀）。
    #[test]
    fn test_sibling_dir_with_common_prefix_not_inside() {
        let events = vec![write_call("file_write", r"D:\ws2\x.txt")];
        let out = run(events, &[("scope", s(r"D:\ws"))]);
        assert_eq!(
            out.get("out_of_scope")
                .and_then(JsonValue::as_array)
                .unwrap()
                .len(),
            1
        );
    }

    // ---- 入参面 ----

    #[test]
    fn test_scope_absent_collects_only() {
        let events = vec![write_call("file_write", r"D:\anywhere\x.txt")];
        let out = run(events, &[]);
        assert_eq!(
            out.get("written_paths")
                .and_then(JsonValue::as_array)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            out.get("out_of_scope")
                .and_then(JsonValue::as_array)
                .unwrap()
                .len(),
            0,
            "scope 缺省 None = 只收集不检出（正本口径）"
        );
    }

    #[test]
    fn test_write_tools_extension() {
        let events = vec![write_call("custom_save", r"D:\ws\a.bin")];
        let out = run(
            events,
            &[
                ("scope", s(r"D:\ws")),
                ("write_tools", JsonValue::array(vec![s("custom_save")])),
            ],
        );
        assert_eq!(
            out.get("written_paths")
                .and_then(JsonValue::as_array)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn test_relative_scope_without_base_dir_fails_visible() {
        let events = vec![write_call("file_write", "a.txt")];
        let err = out_of_scope_writes_json(
            &events,
            &JsonValue::object_from_pairs(&[("scope", s("rel/dir"))]),
        )
        .expect_err("相对 scope 无 base_dir 必须拒解");
        assert!(err.contains("scope"), "错误应含指引: {err}");
    }

    #[test]
    fn test_relative_base_dir_fails_visible() {
        let events = vec![write_call("file_write", "a.txt")];
        let err = out_of_scope_writes_json(
            &events,
            &JsonValue::object_from_pairs(&[("scope", s(r"D:\ws")), ("base_dir", s("rel/dir"))]),
        )
        .expect_err("相对 base_dir 必须拒解");
        assert!(err.contains("base_dir"), "错误应含指引: {err}");
    }

    #[test]
    fn test_write_tools_bad_shape_fails_visible() {
        let events = vec![write_call("file_write", "a.txt")];
        let err = out_of_scope_writes_json(
            &events,
            &JsonValue::object_from_pairs(&[
                ("scope", s(r"D:\ws")),
                ("write_tools", JsonValue::array(vec![JsonValue::Integer(1)])),
            ]),
        )
        .expect_err("write_tools 非字符串数组必须拒解");
        assert!(err.contains("write_tools"), "错误应含指引: {err}");
    }

    // ---- 正本形态锚：键集与 m4_deliverable_drift 输出逐键一致 ----

    #[test]
    fn test_key_set_matches_reference() {
        let out = run(vec![write_call("file_write", "a.txt")], &[]);
        let mut keys = out
            .as_object()
            .map(|o| o.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        keys.sort();
        assert_eq!(keys, vec!["out_of_scope", "written_paths"]);
    }
}

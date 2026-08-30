// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule Server, licensed under GNU Affero General Public License v3 or later.
//! evorule-server compile-time gate (S1: panic-prone)
//!
//! 强制执行 S1：server 主代码路径不能使用 panic-prone 构造。
//!
//! evorule-server 是 evorule 框架的官方 HTTP server 实现。
//! 生产代码路径的 panic 会导致整个 server 进程崩溃，影响所有 session。
//! 必须在此被编译期拦截。
//!
//! # 扫描范围
//! 递归扫描 `src/**/*.rs`（含 `src/api/*.rs` 等子模块）。
//! 模块化后单文件扫描会漏掉子模块后门，故必须递归。
//!
//! # 禁止的模式
//! - S1: `debug_assert!`、`.unwrap(`、`.expect(`、`panic!(`（panic-prone 构造）
//!
//! # 豁免
//! - `#[cfg(test)] mod tests { ... }` 测试模块体（通过 `strip_test_mod` 剥离）
//! - `//` 开头的注释行（含 `///`、`//!`）
//!
//! # unsafe 守护
//! unsafe 不由本 build.rs 扫描，而是由每个源文件顶部的 `#![forbid(unsafe_code)]`
//! 编译器级强制守护（forbid 比 build.rs 字面量扫描更强，不可被 allow 覆盖）。
//!
//! # 与 evorule 核心门控的区别
//! - evorule 核心 (tcb/reactor/governance/cli) 的 build.rs 扫描 T/G/F 系列模式
//!   （含 async/tokio/IO/HashMap 等确定性约束）
//! - 本 build.rs 用 **S 系列编号**，只守 panic-prone（S1）
//! - **不扫描** async/tokio/std::fs/std::net/HashMap/SystemTime 等
//!   （server 仓必需这些，与核心的确定性约束不同）
//! - S1 = F11 = G1 (panic-prone)，是跨仓一致的安全约束
//!
//! # 紧急跳过
//! ```bash
//! EVORULE_SKIP_GATE=1 cargo build
//! ```
//! 跳过必须临时且有书面理由，永不永久禁用。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// 禁止模式：(标签, 字节子串)
///
/// S1 needle 匹配 panic-prone 构造，与 evorule 核心的 F11 模式一致。
const FORBIDDEN: &[(&str, &str)] = &[
    // S1: 主代码路径禁止 panic-prone 构造
    ("S1-debug_assert", "debug_assert!"),
    ("S1-unwrap", ".unwrap("),
    ("S1-expect", ".expect("),
    ("S1-panic", "panic!("),
];

fn main() -> ExitCode {
    let crate_name = std::env::var("CARGO_PKG_NAME").unwrap_or_else(|_| "unknown".into());

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");

    if std::env::var("EVORULE_SKIP_GATE").is_ok() {
        println!("cargo:warning={crate_name} compile-time gate SKIPPED via EVORULE_SKIP_GATE");
        return ExitCode::SUCCESS;
    }

    let manifest_dir = match std::env::var("CARGO_MANIFEST_DIR") {
        Ok(s) => PathBuf::from(s),
        Err(_) => {
            eprintln!("build.rs: CARGO_MANIFEST_DIR not set");
            return ExitCode::FAILURE;
        }
    };
    let src_dir = manifest_dir.join("src");
    if !src_dir.exists() {
        eprintln!("build.rs: src/ not found at {}", src_dir.display());
        return ExitCode::FAILURE;
    }

    println!("cargo:rerun-if-changed={}", src_dir.display());

    // 递归收集 src/**/*.rs
    let mut rs_files: Vec<PathBuf> = Vec::new();
    collect_rs_files(&src_dir, &mut rs_files);
    for f in &rs_files {
        println!("cargo:rerun-if-changed={}", f.display());
    }

    if rs_files.is_empty() {
        eprintln!("==== {crate_name} compile-time gate FAILED ====");
        eprintln!("No .rs files found under {}", src_dir.display());
        return ExitCode::FAILURE;
    }

    let mut violations: Vec<(PathBuf, String, String)> = Vec::new();
    for path in &rs_files {
        let raw = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("build.rs: cannot read {}: {e}", path.display());
                return ExitCode::FAILURE;
            }
        };

        // 先剥离 #[cfg(test)] mod tests { ... } 体，使测试内的 .unwrap()/expect() 不误报
        let content = strip_test_mod(&raw);

        for (label, needle) in FORBIDDEN {
            for (lineno, line) in content.lines().enumerate() {
                // 豁免注释行（含 ///、//!、//）
                let trimmed = line.trim_start();
                if trimmed.starts_with("//") {
                    continue;
                }
                if line.contains(needle) {
                    violations.push((
                        path.clone(),
                        label.to_string(),
                        format!("L{}: {}", lineno + 1, line.trim()),
                    ));
                }
            }
        }
    }

    if violations.is_empty() {
        // Gate passed silently — success is the default expected state, not a warning.
        // SKIP path still emits cargo:warning (skipping a security gate is noteworthy).
        // FAILURE path uses eprintln! (loud, visible on build failure).
        // Gate execution is verifiable by build success (gate failure → build failure).
        return ExitCode::SUCCESS;
    }

    eprintln!();
    eprintln!("==== {crate_name} compile-time gate FAILED ====");
    eprintln!("{} violation(s):", violations.len());
    for (path, label, detail) in &violations {
        eprintln!("  [{}] {}: {}", label, path.display(), detail);
    }
    eprintln!();
    eprintln!("违规类型: S1=panic-prone构造 (unwrap/expect/panic/debug_assert)");
    eprintln!("unsafe 守护: 由源文件顶部 #![forbid(unsafe_code)] 编译器级强制");
    eprintln!("紧急跳过: EVORULE_SKIP_GATE=1 cargo build (须有书面理由)");
    ExitCode::FAILURE
}

/// 递归收集目录下所有 `.rs` 文件
///
/// 按路径字典序排序，保证扫描顺序确定性（便于复现违规报告）。
fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// 从源码中剥离 `#[cfg(test)] mod <ident> { ... }` 块体。
///
/// 通过花括号计数（感知字符串/字符/注释/原始字符串），使测试内的 S1 模式
/// 不触发误报。算法与 evorule-cli/build.rs 一致，便于跨仓审计。
///
/// 支持任意 mod 名（`tests` / `whitelist_tests` / `ssrf_tests` 等），
/// 只要被 `#[cfg(test)]` 修饰就整体剥离。
///
/// **限制**：不剥离 `#[cfg(test)] fn` / `#[cfg(test)] impl` 等非 mod 项；
/// 这类项内部的 panic-prone 构造仍会被扫描（当前 io_handlers 中唯一的
/// `#[cfg(test)] fn new_for_tests()` 内部无 unwrap，不受影响）。
fn strip_test_mod(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < bytes.len() {
        let rel = src[i..].find("#[cfg(test)]");
        if let Some(attr_pos) = rel {
            let abs_pos = i + attr_pos;
            if let Some(mod_offset) = skip_to_mod_tests(&src[abs_pos..]) {
                let mod_abs = abs_pos + mod_offset;
                if let Some(rel_brace) = find_inline_lbrace(&src[mod_abs..]) {
                    let open_idx = mod_abs + rel_brace;
                    if let Some(close_idx) = match_brace(src, open_idx) {
                        // 保留 i..open_idx+1（含 `#[cfg(test)] mod xxx {`）和闭合 `}`，
                        // 剥离中间的测试体。
                        // 只追加 close_idx 处的单个 `}`，不追加 `src[close_idx..]` 全部 ——
                        // 否则文件里有多个 #[cfg(test)] mod 时，第一次剥离会把后续测试模块
                        // 的整个体追加到 out（然后第二次迭代又追加一次），导致测试体被保留，
                        // 26 处 .unwrap( 被当作生产代码误报（实为真测试代码）。
                        out.push_str(&src[i..open_idx + 1]);
                        out.push_str(&src[close_idx..close_idx + 1]);
                        i = close_idx + 1;
                        continue;
                    }
                }
            }
        }
        let ch = match std::str::from_utf8(&bytes[i..]) {
            Ok(s) => s.chars().next().unwrap_or('\u{FFFD}'),
            Err(_) => '\u{FFFD}',
        };
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn skip_to_mod_tests(src: &str) -> Option<usize> {
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            return None;
        }
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        if bytes[i] == b'#' && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;
            let mut depth = 1;
            while i < bytes.len() && depth > 0 {
                if bytes[i] == b'[' {
                    depth += 1;
                }
                if bytes[i] == b']' && depth > 0 {
                    depth -= 1;
                }
                i += 1;
            }
            continue;
        }
        // 必须紧跟 `mod <ident>`（支持任意 mod 名：tests / whitelist_tests / ssrf_tests 等）。
        // 不向后搜索：曾因 `find("mod tests")` 跨过中间代码误匹配远处的 mod tests，
        // 导致 `#[cfg(test)] mod whitelist_tests { ... }` 体被当作生产代码扫描（44 处误报）。
        if src[i..].starts_with("mod") {
            // mod 后必须是空白（排除 modern/modifier 等标识符的一部分）
            if let Some(c) = src[i + 3..].chars().next() {
                if c.is_whitespace() {
                    return Some(i);
                }
            }
        }
        return None;
    }
    None
}

/// `'` 处判别：字符字面量（`'x'` / `'\n'` / `'\''`）还是生命周期（`'a` / `'static` / `'_`）。
///
/// 判别规则（Rust 语法保证无歧义）：
/// - `'` 后跟 `\` → 转义字符字面量；
/// - `'` 后跟单字符且再下一位是 `'` → 单字符字面量；
/// - 其余（`'ident`）→ 生命周期/标签。
///
/// 合法源码不存在 `'ab'`（多字符字面量非法），故该判别不会误判。
/// 不判别的后果（Q12 实测教训）：`fn f() -> &'static str {` 的 `'static` 进入
/// 字符态后吞掉直到下一个 `'` 之间的所有 `{}`，令 match_brace 永不闭合、
/// tests 模块整体不被剥离，门禁对全文件测试代码全量误报。
fn char_lit_starts(bytes: &[u8], i: usize) -> bool {
    match bytes.get(i + 1) {
        Some(b'\\') => true,
        Some(_) => bytes.get(i + 2) == Some(&b'\''),
        None => false,
    }
}

/// 生命周期跳过：从 `'` 起越过标识符字符（`'static` / `'a` / `'_`），停在非 ident 处。
fn skip_lifetime(bytes: &[u8], mut i: usize) -> usize {
    i += 1; // 越过 '
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    i
}

/// 查找下一个不在注释/字符串内的 `{`，遇到 `;` 返回 None（`mod tests;` 无体）。
fn find_inline_lbrace(src: &str) -> Option<usize> {
    let bytes = src.as_bytes();
    let mut i = 0;
    let mut in_line_c = false;
    let mut in_block_c = false;
    let mut in_str = false;
    let mut in_char = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_line_c {
            if b == b'\n' {
                in_line_c = false;
            }
            i += 1;
            continue;
        }
        if in_block_c {
            if b == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                in_block_c = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if in_str {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if in_char {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == b'\'' {
                in_char = false;
            }
            i += 1;
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            in_line_c = true;
            i += 2;
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            in_block_c = true;
            i += 2;
            continue;
        }
        if b == b'"' {
            in_str = true;
            i += 1;
            continue;
        }
        if b == b'\'' {
            if char_lit_starts(bytes, i) {
                in_char = true;
                i += 1;
            } else {
                // 生命周期/标签（`'a` / `'static` / `'outer:`）：不进入字符态，
                // 跳过标识符——否则字符态误吞后续 `{}`（见 char_lit_starts 文档）
                i = skip_lifetime(bytes, i);
            }
            continue;
        }
        if b == b'{' {
            return Some(i);
        }
        if b == b';' {
            return None;
        }
        i += 1;
    }
    None
}

/// 为 `{` at `open_idx` 找匹配的 `}`（感知字符串/注释/原始字符串）。
///
/// # Clippy 豁免
/// - `too_many_lines` / `cognitive_complexity`：本函数是单状态机，6 个状态变量
///   (depth / in_str / in_char / in_line_c / in_block_c / i) 在分支间共享，拆分子
///   函数需传全部状态，反而降低可读性。原始字符串 `r#"..."#` 识别是状态机的必要
///   分支（不感知会导致花括号计数错乱），无法独立。详见 GATE_REFERENCE.md §六。
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
fn match_brace(src: &str, open_idx: usize) -> Option<usize> {
    let bytes = src.as_bytes();
    if bytes[open_idx] != b'{' {
        return None;
    }
    let mut depth: i32 = 0;
    let mut i = open_idx;
    let mut in_line_c = false;
    let mut in_block_c = false;
    let mut in_str = false;
    let mut in_char = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_line_c {
            if b == b'\n' {
                in_line_c = false;
            }
            i += 1;
            continue;
        }
        if in_block_c {
            if b == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                in_block_c = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if in_str {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if in_char {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == b'\'' {
                in_char = false;
            }
            i += 1;
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            in_line_c = true;
            i += 2;
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            in_block_c = true;
            i += 2;
            continue;
        }
        // 原始字符串 r#"..."# / r"..." / r##"..."## (不感知此语法会导致花括号计数错乱)
        if b == b'r' && i + 1 < bytes.len() {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] == b'#' {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'"' {
                let hash_count = j - i - 1;
                i = j + 1;
                while i < bytes.len() {
                    if bytes[i] == b'"' {
                        let mut m = 0;
                        while m < hash_count && i + 1 + m < bytes.len() && bytes[i + 1 + m] == b'#'
                        {
                            m += 1;
                        }
                        if m == hash_count {
                            i += 1 + hash_count;
                            break;
                        }
                    }
                    i += 1;
                }
                continue;
            }
        }
        if b == b'"' {
            in_str = true;
            i += 1;
            continue;
        }
        if b == b'\'' {
            if char_lit_starts(bytes, i) {
                in_char = true;
                i += 1;
            } else {
                // 生命周期/标签（`<'a>` / `&'static str` / `'outer:`）：不进入字符态，
                // 跳过标识符——否则字符态误吞后续 `{}` 致 match_brace 永不闭合、
                // tests 模块整体不被剥离（Q12 实测：'static 令门禁全量误报）
                i = skip_lifetime(bytes, i);
            }
            continue;
        }
        if b == b'{' {
            depth += 1;
        }
        if b == b'}' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn test_collect_rs_files_finds_main() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let src_dir = Path::new(manifest_dir).join("src");
        let mut files = Vec::new();
        collect_rs_files(&src_dir, &mut files);
        assert!(
            files
                .iter()
                .any(|f| f.ends_with("main.rs") || f.ends_with("lib.rs")),
            "collect_rs_files should find src/main.rs or src/lib.rs, got: {:?}",
            files
        );
    }

    #[test]
    fn test_strip_test_mod_removes_test_body() {
        let src = r#"
fn prod() { let x = something.unwrap(); }
#[cfg(test)]
mod tests {
    fn helper() { let x = something.unwrap(); }
}
"#;
        let stripped = strip_test_mod(src);
        // 测试模块体被剥离后，.unwrap( 只剩 prod() 中的 1 处
        assert_eq!(
            stripped.matches(".unwrap(").count(),
            1,
            "strip_test_mod should remove test mod body, got: {:?}",
            stripped
        );
    }

    #[test]
    fn test_match_brace_balanced() {
        let src = "fn f() { let x = { let y = 1; y }; x }";
        let open = src.find('{').unwrap();
        let close = match_brace(src, open).unwrap();
        // 最外层 { 对应最后一个 }
        assert_eq!(src.chars().nth(close), Some('}'));
    }

    /// Q12 实测教训回归：生命周期撇号（位于被扫描花括号**之后**、且其后无恢复撇号）
    /// 不得令字符态吞掉闭合 `}`——旧行为曾致 match_brace 永不闭合 → tests 体不被剥离
    /// → 门禁全量误报
    #[test]
    fn test_match_brace_ignores_lifetime() {
        // 注意：'static 必须在首个 { 之后且其后整段无撇号，才能复现旧缺陷
        let src = "fn outer() { let mk: fn() -> &'static str; }";
        let open = src.find('{').unwrap();
        let close = match_brace(src, open).unwrap();
        assert_eq!(src.chars().nth(close), Some('}'));
    }

    /// 字符字面量中的花括号/引号仍被正确跳过（判别不得矫枉过正）
    #[test]
    fn test_match_brace_ignores_char_and_string_braces() {
        let src = "fn f() { let c = '{'; let s = \"}\"; let esc = '\\''; }";
        let open = src.find('{').unwrap();
        let close = match_brace(src, open).unwrap();
        assert_eq!(src.chars().nth(close), Some('}'));
    }

    /// 含生命周期撇号的 tests 体（撇号后无恢复撇号）仍须被完整剥离
    #[test]
    fn test_strip_survives_lifetime_apostrophe() {
        // 撇号在 tests 体内、其后整个文件无撇号 → 旧行为 match_brace 永不闭合
        let src = concat!(
            "#[cfg(test)]\n",
            "mod tests {\n",
            "    fn schema() -> &'static str { \"x\" }\n",
            "    fn helper() { let x = something.unwrap(); }\n",
            "}\n",
        );
        let stripped = strip_test_mod(src);
        assert_eq!(
            stripped.matches(".unwrap(").count(),
            0,
            "生命周期撇号后的 tests 体须被剥离, got: {stripped:?}"
        );
    }

    /// 字符字面量判别的三个分支：单字符 / 转义 / 生命周期
    #[test]
    fn test_char_lit_vs_lifetime() {
        let bytes = b"fn f() -> &'static str { let c = 'x'; let e = '\\n'; }";
        // 'static：' 后是 s，s 后不是 ' → 生命周期
        let apos = bytes.iter().position(|&b| b == b'\'').unwrap();
        assert!(!char_lit_starts(bytes, apos));
        // 'x'：' 后是 x，x 后是 ' → 字符字面量
        let apos2 = bytes.iter().rposition(|&b| b == b'\'').unwrap() - 3;
        assert_eq!(bytes[apos2], b'\'');
        assert!(char_lit_starts(bytes, apos2));
    }
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! evorule-governance compile-time gate (L1 字面量门禁)
//!
//! 强制执行 GOVERNANCE_SPEC.md 的 G7/G8 + G1(F11) + §5.2 规则。
//! 跨模块设计见 ../../GATE_REFERENCE.md §四(跨模块门控图)+ §五(SPEC 章节编号映射)。
//!
//! **与 evorule-reactor/build.rs 保持同一组 14 模式** (3 G8 + 4 F11 + 7 S5.2,
//! 无 T10) + fact.rs 豁免, 这是有意的双层一致 (避免 tier1/tier2 走偏);
//! 与 evorule-tcb (24 模式) 为有意差异, 见根 GATE_REFERENCE §一。
//!
//! # 扫描的 14 个模式
//!
//! | 规则          | 模式                                                           | 数量 |
//! |---------------|----------------------------------------------------------------|------|
//! | G7/G8 (控制流)| `"conditional"`, `"while_loop"`, `"sequence"`                  | 3    |
//! | G1/F11 (panic)| `debug_assert!`, `.unwrap(`, `.expect(`, `panic!(`             | 4    |
//! | §5.2 (业务术语)| `"math_rule"`, `"physics_rule"`, `"summarize"`, 等             | 7    |
//!
//! # 豁免
//!
//! - `#[cfg(test)] mod <ident>` 任意命名测试模块
//! - 注释 (`//`, `///`, `//!`, `/* */`)
//! - `src/fact.rs` (G8/§5.2 模式) — IoType/ControlFlowType 枚举映射的唯一真值来源
//!
//! 匹配口径: 每行同时按原文与去空白文本匹配 (`x.unwrap ()` 等插空写法同样
//! 拦截); 注释/属性行判定仍用原文。
//!
//! # 策略层反模式检测
//!
//! 除 L1 字面量门禁外, 另执行策略层反模式检测 (P1-P4, 无阀常开):
//! - 与 evorule-tcb/build.rs、evorule-reactor/build.rs 保持同一份实现 (内联副本)
//!
//! # 紧急跳过
//!
//! ```bash
//! EVORULE_SKIP_GATE=1 cargo build       # 跳过 L1 字面量门禁
//! EVORULE_SKIP_REASON="原因"            # 跳过理由登记 (未登记将出 warning)
//! ```
//! 阀值仅 `1`/`true` 生效 (`0`/空/其他值 = 门禁照常执行, fail-closed)。
//! 跳过必须临时且有书面理由, 永不永久禁用。
//!
//! # 门禁定位（诚实边界）
//!
//! 本文件全部门禁是工程质量自查纪律（机制-策略分离、确定性红线），不是
//! 对抗主动攻击者的安全边界；策略层检测无阀常开。CR 变更自查
//! （CHANGE_REQUEST.md 字段清单）已移出公开仓，由本地 git pre-commit hook
//! 承接——它从来不是防伪造审查机制（既定裁定）。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// 禁止模式: (标签, 字节子串)
///
/// G8 needle 含双引号边界, 精确匹配字符串字面量, 不误报注释中
/// 无引号的单词 (如 `// conditional 指令`)。
/// G1/F11 needle 匹配 panic-prone 构造。
/// §5.2 needle 匹配业务术语字符串字面量。
const FORBIDDEN: &[(&str, &str)] = &[
    // G7/G8: 控制流指令名不得出现在 Rust 字符串中
    ("G8-conditional", "\"conditional\""),
    ("G8-while_loop", "\"while_loop\""),
    ("G8-sequence", "\"sequence\""),
    // G1/F11: 非测试代码禁止 panic-prone 构造
    ("F11-debug_assert", "debug_assert!"),
    ("F11-unwrap", ".unwrap("),
    ("F11-expect", ".expect("),
    ("F11-panic", "panic!("),
    // §5.2: 业务术语不得硬编码在 Rust 中
    ("S5.2-math_rule", "\"math_rule\""),
    ("S5.2-physics_rule", "\"physics_rule\""),
    ("S5.2-summarize", "\"summarize\""),
    ("S5.2-admin", "\"admin\""),
    ("S5.2-teacher", "\"teacher\""),
    ("S5.2-call_external", "\"call_external\""),
    ("S5.2-call_service", "\"call_service\""),
];

/// 跳过类环境变量解析（fail-closed 阀值语义）。
///
/// 仅 `1` / `true`（trim 后、大小写不敏感）视为请求跳过；其余任何值
/// （`0`、空串、乱值）一律不跳过——门禁照常执行，并发出 warning 提示
/// 该值被忽略。旧实现 `is_ok()` 把 `=0`/空值也当跳过，属意外 fail-open。
///
/// 附带跳过理由登记（EVORULE_SKIP_REASON）：跳过生效时若未设置非空理由，
/// 追加 warning——「大声原则」：任何跳过都必须可追溯。
fn skip_requested(var: &str) -> bool {
    match std::env::var(var) {
        Ok(v) => {
            let t = v.trim().to_ascii_lowercase();
            if t == "1" || t == "true" {
                match std::env::var("EVORULE_SKIP_REASON") {
                    Ok(r) if !r.trim().is_empty() => {
                        println!("cargo:warning={var} skip reason: {r}");
                    }
                    _ => {
                        println!(
                            "cargo:warning={var} 已跳过但未登记理由 (EVORULE_SKIP_REASON)——跳过须有书面理由"
                        );
                    }
                }
                true
            } else {
                println!("cargo:warning={var}={v} 非肯定值 (仅 1/true 生效)，门禁照常执行");
                false
            }
        }
        Err(_) => false,
    }
}

/// 去空白对照文本（整改确立）：`x.unwrap ()` 等插空写法在纯原文
/// 子串匹配下漏检，故每行额外生成去全部空白文本参与匹配。注释/属性行
/// 判定仍用原文。代价：字符串字面量内凑巧去空白命中的极小概率误报——
/// 符合门禁「宁可误报不可漏报」哲学。
fn squeeze_ws(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for c in line.chars() {
        if !c.is_whitespace() {
            out.push(c);
        }
    }
    out
}

/// 同行块注释剥离（整改确立）。
///
/// 假阳性背景：注释判定只认 `//` 开头，行中块注释区段内的示例文字
/// （如 `/* conditional */`）会让 L1 误报。本函数把同行**自闭合**的
/// 块注释区段（含嵌套 `/* /* */ */`）从匹配文本中剥离。
///
/// 字符串感知（防漏报）：`"..."`、`b"..."`、原始字符串 `r"..."` /
/// `r#"..."#` / `r##"..."##` 内的 `/*` 不是注释起点——否则字符串内伪
/// `/*` 会把后续真代码吃进伪注释区一起剥掉，构成漏报面（违背「宁可
/// 误报不可漏报」）。常规字符串内 `\"` 转义跳过；原始字符串按定义
/// 不处理转义。字符字面量 `'x'` 不设独立状态：`'/​*'` 形态在合法 Rust
/// 中不存在（char 只装一个标量），代价只是 `'"';` 这类写法让本行
/// 「未闭合」而保守降级——误报方向，安全。
///
/// fail-closed 边界：`/*` 无同行 `*/`、字符串未在本行闭合——一律放弃
/// 剥离、整行按原文匹配；跨行块注释首行同样整行保留。剥离器自身不
/// 允许成为漏报面。字符串内容原样保留参与匹配（字符串内命中是设计
/// 接受的误报）。
///
/// 字节级扫描安全：UTF-8 多字节序列所有连续字节 > 0x7F，不会与 ASCII
/// 的 `/ * " ' # b r` 混淆。
fn strip_inline_block_comments(line: &str) -> String {
    let b = line.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0usize;
    while i < b.len() {
        // 字符串类起点（顺序敏感：br# 先于 b" / r# 判定；起点序列内
        // 计数前导 # 数，闭合时要求同数 # 配平）
        let mut hashes = 0usize;
        let mut raw = false;
        let mut start_len = 0usize;
        if b[i] == b'"' {
            start_len = 1;
        } else if b[i] == b'b' && i + 1 < b.len() && b[i + 1] == b'"' {
            start_len = 2;
        } else if b[i] == b'r' && i + 1 < b.len() && b[i + 1] == b'"' {
            start_len = 2;
            raw = true;
        } else if b[i] == b'r' && i + 1 < b.len() && b[i + 1] == b'#'
            || b[i] == b'b' && i + 2 < b.len() && b[i + 1] == b'r' && b[i + 2] == b'#'
        {
            let mut j = i + if b[i] == b'b' { 2 } else { 1 };
            while j < b.len() && b[j] == b'#' {
                hashes += 1;
                j += 1;
            }
            if j < b.len() && b[j] == b'"' {
                raw = true;
                start_len = j - i + 1;
            }
        }
        if start_len > 0 {
            // 扫到本行闭合：常规串处理 `\"` 转义，原始串不处理转义
            let mut j = i + start_len;
            let mut closed = false;
            while j < b.len() {
                if !raw && b[j] == b'\\' {
                    j += 2;
                    continue;
                }
                if b[j] == b'"' {
                    let mut k = j + 1;
                    let mut h = 0usize;
                    while k < b.len() && b[k] == b'#' {
                        h += 1;
                        k += 1;
                    }
                    if h == hashes {
                        closed = true;
                        j = k;
                        break;
                    }
                }
                j += 1;
            }
            if !closed {
                return line.to_string(); // 本行未闭合 → 保守整行
            }
            out.extend_from_slice(&b[i..j]); // 字符串内容原样保留
            i = j;
            continue;
        }
        // 块注释起点：嵌套计数，同行配平才剥离
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            let mut depth = 1usize;
            let mut j = i + 2;
            while j < b.len() {
                if b[j] == b'/' && j + 1 < b.len() && b[j + 1] == b'*' {
                    depth += 1;
                    j += 2;
                } else if b[j] == b'*' && j + 1 < b.len() && b[j + 1] == b'/' {
                    depth -= 1;
                    j += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    j += 1;
                }
            }
            if depth != 0 {
                return line.to_string(); // 跨行块注释 → 保守整行
            }
            out.push(b' '); // 剥离区段以一个空格占位，防相邻 token 粘连
            i = j;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| line.to_string())
}

/// 裸词命中判定（整改确立）。
///
/// `async` / `await` 作为裸词 needle 在普通子串匹配下会被
/// "asynchronous" / "awaiting" 等英文单词误命中（假阳性）。命中后检查
/// 前后字符均非 ASCII 字母——真正使用 async/await 关键字时两侧是
/// 空白/符号/标点（含 `.await` 后缀调用形式）。去空白口径下 `async fn`
/// 合并为 `asyncfn`，词界检查自然拒绝该合并词——但原文口径已保证
/// `async fn` 照常命中，两口径协同无漏报。其余 needle 走普通子串匹配。
fn bare_word_hit(hay: &str, needle: &str) -> bool {
    if needle != "async" && needle != "await" {
        return hay.contains(needle);
    }
    let hb = hay.as_bytes();
    let mut from = 0usize;
    while let Some(pos) = hay[from..].find(needle) {
        let p = from + pos;
        let before_ok = p == 0 || !hb[p - 1].is_ascii_alphabetic();
        let end = p + needle.len();
        let after_ok = end >= hb.len() || !hb[end].is_ascii_alphabetic();
        if before_ok && after_ok {
            return true;
        }
        from = p + 1;
    }
    false
}

/// 从源码中剥离 `#[cfg(test)] mod tests { ... }` 块体。
///
/// 通过花括号计数 (感知字符串/字符/注释), 使测试内的字面量门禁模式
/// 不触发误报。
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
                        // 只补回闭合花括号 (整改暴露的存量缺陷修复):
                        // 旧实现把 src[close_idx..] 整段尾部原样压入输出后又从
                        // close_idx+1 重扫一遍——单测试模块时尾部无违规内容故从未
                        // 暴露; 一文件含多个 cfg(test) 模块时 (如 executor.rs 的
                        // executor_ssot_tests + tests), 第一个模块的尾部拷贝把未
                        // 剥离的后续测试模块体带进扫描输出 (92 条误报)。
                        out.push_str(&src[i..=open_idx]);
                        out.push('}');
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
        // #[cfg(test)] 属性/注释跳过后必须紧跟 `mod <ident>` (整改加固
        // 附带加固): 旧实现用 "mod tests" 字面子串定位——`mod executor_ssot_tests`
        // 等非 tests 命名测试模块不被剥 (测试内模式暴露 → 误报), `mod tests_foo`
        // 反而借前缀命中被误吞 (漏报面)。cfg(test) 限定的模块本就不进生产构建,
        // 剥离任意命名测试模块是安全豁免。
        if src[i..].starts_with("mod") {
            let after = i + 3;
            if after < bytes.len() && (bytes[after] as char).is_whitespace() {
                let mut j = after + 1;
                loop {
                    while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                        j += 1;
                    }
                    if j + 1 < bytes.len() && bytes[j] == b'/' && bytes[j + 1] == b'/' {
                        while j < bytes.len() && bytes[j] != b'\n' {
                            j += 1;
                        }
                        continue;
                    }
                    break;
                }
                let s = j;
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    j += 1;
                }
                if j > s {
                    return Some(i); // `mod <ident>` 命中; 无体形式由 find_inline_lbrace 兜住
                }
            }
            return None; // `mod` 后非空白/非标识符 (如 `modular`) → 保守不剥
        }
        return None; // 属性后非 mod item → 保守不剥
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
/// 不判别的后果：`fn f() -> &'static str {` 的 `'static` 进入字符态后吞掉
/// 直到下一个 `'` 之间的所有 `{}`，令 match_brace 永不闭合、tests 模块
/// 整体不被剥离，门禁对全文件测试代码全量误报。
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

/// 查找下一个不在注释/字符串内的 `{`, 遇到 `;` 返回 None (`mod tests;` 无体)。
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

/// 为 `{` at `open_idx` 找匹配的 `}` (感知字符串/注释)。
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

/// 递归遍历目录, 收集所有 .rs 文件路径。
fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(it) => it,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

fn main() -> ExitCode {
    let crate_name = std::env::var("CARGO_PKG_NAME").unwrap_or_else(|_| "unknown".into());

    // 策略层反模式检测 (无阀常开, 既定裁定): 机制-策略分离是
    // 设计不变量, 不设旁路阀。CR 自查 (CHANGE_REQUEST.md 校验) 已移出公开仓
    // build.rs——EVORULE_SKIP_CR_GATE 随之移除, 自查由本地 git pre-commit hook
    // 承接 (hook 源落本地工具区, 不随仓库/发布公开)。
    if let Err(e) = detect_strategy_patterns(&crate_name) {
        eprintln!("{}", e);
        return ExitCode::FAILURE;
    }

    if skip_requested("EVORULE_SKIP_GATE") {
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

    let mut files = Vec::new();
    collect_rs_files(&src_dir, &mut files);

    let mut violations: Vec<(PathBuf, String, String)> = Vec::new();

    for path in &files {
        let raw = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("build.rs: cannot read {}: {e}", path.display());
                return ExitCode::FAILURE;
            }
        };

        for (label, needle) in FORBIDDEN {
            // 所有模式 test-tolerant: 测试中可构造这些指令做 fixture
            let content = strip_test_mod(&raw);

            for (lineno, line) in content.lines().enumerate() {
                // 豁免注释行 (含 ///、//!、//)
                let trimmed = line.trim_start();
                if trimmed.starts_with("//") {
                    continue;
                }
                // 豁免 fact.rs 中的 IoType/ControlFlowType 字符串映射
                // (§5.2 和 G8 的唯一真值来源, 必须在此集中定义)
                // 注意: 此豁免必须与 evorule-reactor/build.rs 保持一致 (双层一致性)
                let is_fact_rs = path.file_name().and_then(|s| s.to_str()) == Some("fact.rs");
                if (label.starts_with("S5.2") || label.starts_with("G8")) && is_fact_rs {
                    continue;
                }
                // 同行块注释剥离: `/* conditional */` 等
                // 注释示例文字不再误报; 字符串感知防漏报, 跨行未闭合保守整行。
                let stripped = strip_inline_block_comments(line);
                let squeezed = squeeze_ws(&stripped);
                // 裸词词界判定 (整改): async/await 不再被
                // "asynchronous"/"awaiting" 等英文单词误命中。
                if bare_word_hit(&stripped, needle) || bare_word_hit(&squeezed, needle) {
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
        // 大声原则: 通过也要可见——零输出的通过无法与
        // "build.rs 被删/被短路/未执行" 区分。cargo:warning 每次构建常态
        // 可见, 与 SKIPPED 输出及 L1b 策略层检测 PASSED 输出口径一致;
        // 文件数/模式数动态取值, 模式清单变更不产生假数字。
        println!(
            "cargo:warning={} 字面量门禁 PASSED - 已扫 {} 文件 x {} 模式, 0 违规",
            crate_name,
            files.len(),
            FORBIDDEN.len()
        );
        return ExitCode::SUCCESS;
    }

    eprintln!();
    eprintln!("==== {crate_name} compile-time gate FAILED ====");
    eprintln!("{} violation(s):", violations.len());
    for (path, label, detail) in &violations {
        eprintln!("  [{}] {}: {}", label, path.display(), detail);
    }
    eprintln!();
    eprintln!("违规类型: G8=控制流指令字面量 | F11=panic-prone构造 | §5.2=业务术语硬编码");
    eprintln!("紧急跳过: EVORULE_SKIP_GATE=1 cargo build (须有书面理由)");
    ExitCode::FAILURE
}

// ===== 策略模式检测器 (Strategy Pattern Detector) =====
//
// 与 evorule-tcb/build.rs、evorule-reactor/build.rs、evorule-cli/build.rs 保持同一份实现 (内联副本)。
// 任何对检测逻辑的修改必须四仓同步, 防止核心模块的机制层边界走偏。

/// 策略层反模式定义
///
/// 这些模式在机制层（evorule 仓）中是禁止的，因为它们代表策略层（应用层）逻辑
struct StrategyPattern<'a> {
    label: &'static str,
    patterns: &'a [&'static str],
    description: &'static str,
}

/// 策略层反模式检测列表
const STRATEGY_PATTERNS: &[StrategyPattern<'static>] = &[
    // P1: 业务领域关键字 - 机制层不应包含特定业务领域的逻辑
    // 注意：使用更具体的模式，避免误报
    StrategyPattern {
        label: "P1-business-domain",
        patterns: &[
            // 医疗领域
            "\"hospital\"",
            "\"medical\"",
            "\"patient\"",
            "\"clinic\"",
            // 金融领域
            "\"finance\"",
            "\"bank_\"",
            "\"investment\"",
            "\"loan_\"",
            // 法律领域
            "\"lawyer\"",
            "\"court_case\"",
            "\"legal_document\"",
            // 保险领域
            "\"insurance\"",
            "\"policy_number\"",
            "\"premium_amount\"",
            // 制造业
            "\"manufacturing\"",
            "\"production_line\"",
            // 电商领域
            "\"ecommerce\"",
            "\"order_item\"",
            "\"payment_method\"",
        ],
        description: "机制层包含特定业务领域关键字，策略层逻辑必须在应用层仓实现",
    },
    // P2: 控制流指令硬编码 - 检查是否实现了控制流逻辑（不是引用名称）
    // 机制层可以引用控制流类型名称作为数据模型，但不应实现控制流逻辑
    StrategyPattern {
        label: "P2-control-flow-hardcode",
        patterns: &[
            // 控制流实现逻辑（而非引用）
            "execute_conditional",
            "execute_while_loop",
            "execute_sequence",
            "handle_conditional",
            "handle_while_loop",
            "handle_sequence",
            "process_conditional",
            "process_while_loop",
            "process_sequence",
        ],
        description: "控制流指令的执行逻辑应在 core_eval.json 中定义，不应在 Rust 代码中实现",
    },
    // P3: 业务操作硬编码 - 机制层不应包含特定业务操作
    StrategyPattern {
        label: "P3-business-operation",
        patterns: &[
            "calculate_fee",
            "calculate_tax",
            "calculate_discount",
            "validate_insurance",
            "process_claim",
            "approve_loan",
            "check_credit",
            "verify_identity",
            "assess_risk",
            "generate_invoice",
            "create_order",
            "process_payment",
            "update_inventory",
            "ship_product",
            "receive_goods",
            "hire_employee",
            "pay_salary",
            "calculate_bonus",
        ],
        description: "机制层包含特定业务操作，操作逻辑应在应用层实现",
    },
    // P4: 业务规则名硬编码 - 机制层不应包含特定业务规则名
    StrategyPattern {
        label: "P4-business-rule-name",
        patterns: &[
            "hipaa_rule",
            "gdpr_rule",
            "pci_rule",
            "sox_rule",
            "compliance_check",
            "audit_rule",
            "regulatory_check",
            "kpl_rule",
            "aml_rule",
            "kyc_rule",
        ],
        description: "机制层包含特定业务规则名，规则定义应在应用层实现",
    },
];

/// 扫描 src/ 目录中的策略层反模式
///
/// # 返回
///
/// - Ok(()) 未检测到策略层反模式
/// - Err(String) 检测到策略层反模式，包含详细违规信息
fn detect_strategy_patterns(crate_name: &str) -> Result<(), String> {
    // 获取 manifest 目录
    let manifest_dir = match std::env::var("CARGO_MANIFEST_DIR") {
        Ok(s) => PathBuf::from(s),
        Err(_) => {
            return Err("build.rs: CARGO_MANIFEST_DIR not set".to_string());
        }
    };

    let src_dir = manifest_dir.join("src");
    if !src_dir.exists() {
        return Ok(()); // src 目录不存在时跳过检测
    }

    // 收集所有 .rs 文件
    let mut rs_files = Vec::new();
    collect_rs_files_for_strategy(&src_dir, &mut rs_files);

    let mut violations: Vec<(PathBuf, String, String)> = Vec::new();

    for path in &rs_files {
        let content = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue,
        };

        // 全文件扫描 (测试豁免已撤): 测试代码同为机制层, 须守同一
        // 纪律; 旧「剥离 mod tests 再扫」既留注释伪装/`mod tests_foo` 误吞等
        // 绕过面, 又给策略层留测试区藏身处——自查机制不给自己留豁免。
        for pattern_def in STRATEGY_PATTERNS {
            for pattern in pattern_def.patterns {
                if content.contains(pattern) {
                    violations.push((
                        path.clone(),
                        pattern_def.label.to_string(),
                        format!(
                            "发现策略层反模式 \"{}\": {} (模式: {})",
                            pattern, pattern_def.description, pattern_def.label
                        ),
                    ));
                }
            }
        }
    }

    if !violations.is_empty() {
        let violation_details: Vec<String> = violations
            .iter()
            .map(|(path, label, detail)| format!("  [{}] {}: {}", label, path.display(), detail))
            .collect();

        return Err(format!(
            "==== {} 策略层检测 FAILED ====\n\
             \n\
             检测到 {} 处策略层反模式。evorule 仓是机制层，不允许包含策略层代码。\n\
             策略层逻辑必须在应用层仓实现。\n\
             \n\
             违规详情:\n\
             {}\n\
             \n\
             判定标准:\n\
             - P1: 业务领域关键字（hospital/finance/legal 等）\n\
             - P2: 控制流指令硬编码（conditional/while_loop/sequence 应在 core_eval.json）\n\
             - P3: 业务操作硬编码（calculate_fee/process_claim 等）\n\
             - P4: 业务规则名硬编码（hipaa_rule/gdpr_rule 等）\n\
             \n\
             若确认此变更是机制层变更，请检查代码中是否意外引入了策略层概念。",
            crate_name,
            violations.len(),
            violation_details.join("\n")
        ));
    }

    // 验证通过
    println!(
        "cargo:warning={} 策略层检测 PASSED - 未发现策略层反模式",
        crate_name
    );
    Ok(())
}

/// 收集目录下所有 .rs 文件（用于策略检测）
fn collect_rs_files_for_strategy(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect_rs_files_for_strategy(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn test_match_brace_ignores_lifetime() {
        // 注意：'static 必须在首个 { 之后且其后整段无撇号，才能复现旧缺陷
        let src = "fn outer() { let mk: fn() -> &'static str; }";
        let open = src.find('{').unwrap();
        let close = match_brace(src, open).unwrap();
        assert_eq!(src.chars().nth(close), Some('}'));
    }

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

    #[test]
    fn test_strip_multiple_test_mods() {
        // 一文件多个 cfg(test) 模块 (tcb 仓 executor.rs 实况): 旧实现把第一个
        // 模块的尾部整段原样压入输出又重扫一遍, 后续测试模块体未被剥离; 且
        // 模块间生产代码被重复计入输出。
        let src = concat!(
            "fn prod() { let _x = 1; }\n",
            "#[cfg(test)]\n",
            "mod ssot_tests {\n",
            "    fn a() { something.unwrap(); }\n",
            "}\n",
            "fn prod2() { let _y = 2; }\n",
            "#[cfg(test)]\n",
            "mod tests {\n",
            "    fn b() { other.unwrap(); panic!(\"x\"); }\n",
            "}\n",
        );
        let stripped = strip_test_mod(src);
        assert_eq!(
            stripped.matches(".unwrap(").count(),
            0,
            "两个测试模块体都须被剥离, got: {:?}",
            stripped
        );
        assert_eq!(
            stripped.matches("panic!(").count(),
            0,
            "非 tests 命名的测试模块体同样剥离, got: {:?}",
            stripped
        );
        assert!(
            stripped.contains("fn prod()") && stripped.contains("fn prod2()"),
            "生产代码不得被误吞, got: {:?}",
            stripped
        );
        assert_eq!(
            stripped.matches("fn prod2()").count(),
            1,
            "模块间生产代码不得因尾部重复压入被计入两次, got: {:?}",
            stripped
        );
    }

    #[test]
    fn test_char_lit_starts_discrimination() {
        // 转义字符字面量
        assert!(char_lit_starts(b"let c = '\\n';", 8));
        // 单字符字面量
        assert!(char_lit_starts(b"let c = 'x';", 8));
        // 生命周期
        assert!(!char_lit_starts(b"fn f() -> &'static str {", 12));
        assert!(!char_lit_starts(b"fn f<'a>(x: &'a u8) {}", 5));
    }

    #[test]
    fn test_squeeze_ws_catches_whitespace_bypass() {
        // 插空写法原文未命中、去空白命中 → 拦截
        let bypass = "let x = v.unwrap ();";
        assert!(!bypass.contains(".unwrap("));
        assert!(squeeze_ws(bypass).contains(".unwrap("));
        assert!(squeeze_ws("let m: Hash Map<u8, u8>;").contains("HashMap"));
        // 原文直命中路径不受影响; 合法代码不误伤
        assert!("let x = v.unwrap();".contains(".unwrap("));
        assert!(!squeeze_ws("let x = v.unwrap_or(1);").contains(".unwrap("));
    }

    #[test]
    fn test_strip_inline_block_comments() {
        // 同行自闭合块注释 → 剥离, 注释内示例文字不再误报
        let s = strip_inline_block_comments("let x = 1; /* conditional */ let y = 2;");
        assert!(!s.contains("conditional"), "got: {}", s);
        assert!(s.contains("let y = 2;"));
        // 嵌套块注释配平剥离
        let s = strip_inline_block_comments("fn f() {} /* a /* b */ c */ fn g() {}");
        assert!(!s.contains(" /* "), "got: {}", s);
        // 字符串感知: 字符串内 /* 不是注释起点 (防伪起点吃真代码 → 漏报)
        let s = strip_inline_block_comments("let s = \"a/*b\"; v.unwrap();");
        assert!(s.contains("v.unwrap();"), "got: {}", s);
        // 原始字符串 r#...# 内 /* 不起注释
        let s = strip_inline_block_comments("let s = r#\"/*\"#; v.unwrap();");
        assert!(s.contains("v.unwrap();"), "got: {}", s);
        // 未闭合字符串 → 整行保守 (fail-closed)
        assert_eq!(
            strip_inline_block_comments("let s = \"unterminated /* x"),
            "let s = \"unterminated /* x"
        );
        // 跨行块注释首行 → 整行保守 (fail-closed)
        assert_eq!(
            strip_inline_block_comments("fn f() { /* tail comment"),
            "fn f() { /* tail comment"
        );
    }

    #[test]
    fn test_bare_word_hit() {
        // 裸词词界; 本仓 FORBIDDEN 无裸词模式, 仍验证语义
        assert!(!bare_word_hit("this is asynchronous work", "async"));
        assert!(bare_word_hit("async fn f() {}", "async"));
        assert!(!bare_word_hit("asyncfn", "async"));
        assert!(bare_word_hit("x.unwrap(", ".unwrap("));
        assert!(!bare_word_hit("x.unwrap_or(1)", ".unwrap("));
    }
}

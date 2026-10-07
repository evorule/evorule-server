// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 模板渲染引擎（确定性纯函数）。
//!
//! # 语法 v1（最小集，与 pack 模板 `{{}}` 家族对齐）
//! - `{{path}}`：变量替换；path 为点分路径（`a.b.c`），段 = `[A-Za-z_][A-Za-z0-9_]*`；
//! - `{% if path %}…{% else %}…{% endif %}`：条件分支；判定 = 路径解析成功且非 null
//!   且非布尔假（`0`/`""`/`[]` 均为真）；**路径缺失按假处理**（存在性分支正是其用途）；
//! - `{% for x in path %}…{% endfor %}`：对数组迭代；循环变量 `x` 在体内可见，
//!   元素为对象时 `{{x.field}}` 直接取字段；循环内仍可访问 context 根；
//!   同名时内层循环变量遮蔽外层与根（解析序：最内层循环作用域 → context 根）；
//! - 标签内首尾空白忽略（`{{ name }}` ≡ `{{name}}`）。
//!
//! # 确定性边界（诚实声明）
//! - 渲染 = 纯函数（上下文+模板→输出）：无时钟、无随机、无 IO；同输入逐字节同输出；
//! - 值字符串化：String 原样 / Integer 十进制 / Bool `true|false` /
//!   Array|Object 紧凑 JSON 形态 / **Null 拒绝**；
//! - `{{path}}` 路径缺失或解析为 null = fail-fast 显式报错（不静默空串回退）；
//!   `{% if %}` 的缺失按假处理是唯一例外（见上）；
//! - `for` 的 path 缺失或非数组 = fail-fast（不静默空迭代）；
//! - v1 无过滤器、无字面转义（含 `{{`/`{%` 的字面文本无法表达）、无行首空白控制——
//!   需要时以未来版本显式扩展，本引擎不静默宽容；
//! - 数组下标访问不在 v1 语法内（数组内容用 `for` 消费）。
//!
//! # 执行预算（确定性 fail-fast，非静默截断）
//! 模板字节数 / 输出字节数 / 循环总迭代数均有上限，超出如实报错。

use evorule_tcb::JsonValue;

/// 模板源文本字节数上限（1 MiB）
pub const MAX_TEMPLATE_BYTES: usize = 1024 * 1024;
/// 渲染产物字节数上限（8 MiB；超出 fail-fast，非静默截断）
pub const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
/// 循环总迭代数上限（跨全部 for 标签累计；放大器防护）
pub const MAX_LOOP_ITERATIONS: usize = 100_000;

// ============================================================================
// 产出形态
// ============================================================================

/// 产出形态（由调用方声明；`format=json` 时渲染产物额外做合法性校验）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Text,
    Markdown,
    Json,
}

impl Format {
    /// 解析产出形态声明（fail-fast，词表封闭）
    pub fn parse(s: &str) -> Result<Format, String> {
        match s {
            "text" => Ok(Format::Text),
            "markdown" => Ok(Format::Markdown),
            "json" => Ok(Format::Json),
            other => Err(format!(
                "format '{other}' 非法 — 允许值: text | markdown | json（词表封闭，不静默回退）"
            )),
        }
    }

    /// 形态的规范字符串（响应回显用）
    pub fn as_str(&self) -> &'static str {
        match self {
            Format::Text => "text",
            Format::Markdown => "markdown",
            Format::Json => "json",
        }
    }
}

/// 渲染产物：声明的形态 + 渲染文本（确定性纯函数输出）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub format: Format,
    pub content: String,
}

// ============================================================================
// AST
// ============================================================================

/// 诊断位置（1 基行列，按字符计）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pos {
    line: usize,
    col: usize,
}

impl Pos {
    fn describe(&self) -> String {
        format!("第 {} 行第 {} 列", self.line, self.col)
    }
}

/// 模板 AST 节点（解析一次，渲染任意次）
#[derive(Debug)]
enum Node {
    Text(String),
    Subst {
        path: String,
        pos: Pos,
    },
    If {
        cond_path: String,
        then_body: Vec<Node>,
        else_body: Vec<Node>,
    },
    For {
        var: String,
        path: String,
        pos: Pos,
        body: Vec<Node>,
    },
}

// ============================================================================
// 解析（语法 → AST；结构性错误全部在解析期 fail-fast）
// ============================================================================

/// 解析模板源文本为 AST。语法错误带 1 基行列位置。
fn parse(src: &str) -> Result<Vec<Node>, String> {
    if src.len() > MAX_TEMPLATE_BYTES {
        return Err(format!(
            "模板 {} 字节超出上限 {MAX_TEMPLATE_BYTES} — 执行预算保护，请拆分模板",
            src.len()
        ));
    }
    let mut parser = Parser { src, pos: 0 };
    let (nodes, term) = parser.parse_seq(&[])?;
    debug_assert!(term.is_none(), "顶层无收尾标签，parse_seq 不会返回 term");
    Ok(nodes)
}

struct Parser<'a> {
    src: &'a str,
    pos: usize,
}

/// 收尾标签（解析递归出口）：标签词 + 起始位置
struct Terminator {
    word: String,
    #[allow(dead_code)] // 保留起始位置供未来级联诊断；当前仅 word 参与出口判定
    pos: Pos,
}

impl<'a> Parser<'a> {
    /// 字节偏移 → 1 基（行, 列），按字符计
    fn pos_of(&self, offset: usize) -> Pos {
        let offset = offset.min(self.src.len());
        let before = &self.src.as_bytes()[..offset];
        let line = 1 + before.iter().filter(|&&b| b == b'\n').count();
        let line_start = before
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |i| i + 1);
        let col = self.src[line_start..offset].chars().count() + 1;
        Pos { line, col }
    }

    /// 解析节点序列，直到遇到 `terminators` 中的收尾标签（消费该标签后返回），
    /// 或源文本结束（terminators 为空 = 顶层）。
    fn parse_seq(
        &mut self,
        terminators: &[&str],
    ) -> Result<(Vec<Node>, Option<Terminator>), String> {
        let mut nodes: Vec<Node> = Vec::new();
        loop {
            let Some((open, open_offset)) = self.next_open() else {
                let rest = &self.src[self.pos..];
                if !rest.is_empty() {
                    nodes.push(Node::Text(rest.to_string()));
                }
                self.pos = self.src.len();
                if terminators.is_empty() {
                    return Ok((nodes, None));
                }
                return Err(format!(
                    "模板缺少收尾标签 — 需要其中之一: [{}]（文本结束前未闭合，不静默放行）",
                    terminators.join(" / ")
                ));
            };
            let open_pos = self.pos_of(open_offset);
            if open_offset > self.pos {
                nodes.push(Node::Text(self.src[self.pos..open_offset].to_string()));
            }
            let (close, close_offset) = self.find_close(open, open_offset)?;
            let inner = self.src[open_offset + open.len()..close_offset].trim();
            self.pos = close_offset + close.len();
            if open == "{{" {
                // 变量替换标签（`{{}}` 不是控制流，与收尾词表无涉）
                validate_path(inner)
                    .map_err(|e| format!("{{{{ 变量路径非法（{}）: {e}", open_pos.describe()))?;
                nodes.push(Node::Subst {
                    path: inner.to_string(),
                    pos: open_pos,
                });
                continue;
            }
            // `{% %}` 控制流标签
            let (word, rest) = split_tag_word(inner);
            if terminators.contains(&word) {
                return Ok((
                    nodes,
                    Some(Terminator {
                        word: word.to_string(),
                        pos: open_pos,
                    }),
                ));
            }
            match word {
                "if" => {
                    validate_path(rest).map_err(|e| {
                        format!("{{% if %}} 条件路径非法（{}）: {e}", open_pos.describe())
                    })?;
                    let (then_body, term) = self.parse_seq(&["else", "endif"])?;
                    let term = term.ok_or_else(|| {
                        format!(
                            "{{% if {rest} %}}（{}）缺少 {{% endif %}} 收尾",
                            open_pos.describe()
                        )
                    })?;
                    let else_body = if term.word == "else" {
                        let (body, _term2) = self.parse_seq(&["endif"])?;
                        body
                    } else {
                        Vec::new()
                    };
                    nodes.push(Node::If {
                        cond_path: rest.to_string(),
                        then_body,
                        else_body,
                    });
                }
                "for" => {
                    let (var, path) = parse_for_header(rest).map_err(|e| {
                        format!("{{% for %}} 头部非法（{}）: {e}", open_pos.describe())
                    })?;
                    let (body, _term) = self.parse_seq(&["endfor"])?;
                    nodes.push(Node::For {
                        var,
                        path,
                        pos: open_pos,
                        body,
                    });
                }
                "else" | "endif" | "endfor" => {
                    return Err(format!(
                        "多余的 {{% {word} %}} 标签（{}）— 无匹配的开启块（不静默忽略）",
                        open_pos.describe()
                    ));
                }
                other => {
                    return Err(format!(
                        "未知的 {{% {other} %}} 标签（{}）— 支持的标签: \
                         {{% if path %}} / {{% else %}} / {{% endif %}} / \
                         {{% for x in path %}} / {{% endfor %}}",
                        open_pos.describe()
                    ));
                }
            }
        }
    }

    /// 找下一个标签开启（`{{` 或 `{%`），返回（开启符, 字节偏移）。
    fn next_open(&self) -> Option<(&'static str, usize)> {
        let a = self.src[self.pos..]
            .find("{{")
            .map(|i| ("{{", self.pos + i));
        let b = self.src[self.pos..]
            .find("{%")
            .map(|i| ("{%", self.pos + i));
        // 元组序 = (开启符, 偏移)：比较的是偏移，不是开启符字符串
        match (a, b) {
            (Some((_, oa)), Some((_, ob))) => {
                if oa <= ob {
                    a
                } else {
                    b
                }
            }
            (Some(_), None) => a,
            (None, Some(_)) => b,
            (None, None) => None,
        }
    }

    /// 找与开启符匹配的关闭符；关闭前出现标签开启（嵌套标签）→ 显式报错。
    fn find_close(&self, open: &str, open_offset: usize) -> Result<(&'static str, usize), String> {
        let close: &'static str = if open == "{{" { "}}" } else { "%}" };
        let search_from = open_offset + open.len();
        let body = &self.src[search_from..];
        let close_idx = body.find(close).ok_or_else(|| {
            format!(
                "标签未闭合: '{open}'（{}）— 缺少 '{close}'",
                self.pos_of(open_offset).describe()
            )
        })?;
        let nested = body[..close_idx]
            .find('{')
            .filter(|i| body[*i..].starts_with("{{") || body[*i..].starts_with("{%"));
        if let Some(ni) = nested {
            return Err(format!(
                "标签嵌套: '{open}'（{}）体内在 {} 又出现标签开启 — 标签不可嵌套，请拆分表达式",
                self.pos_of(open_offset).describe(),
                self.pos_of(search_from + ni).describe()
            ));
        }
        Ok((close, search_from + close_idx))
    }
}

/// 标签内文拆出（首词, 其余）：`"if user.vip"` → `("if", "user.vip")`。
fn split_tag_word(inner: &str) -> (&str, &str) {
    match inner.find(char::is_whitespace) {
        Some(i) => (&inner[..i], inner[i..].trim()),
        None => (inner, ""),
    }
}

/// 路径合法性：点分段，每段 = `[A-Za-z_][A-Za-z0-9_]*`，非空。
fn validate_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("路径为空 — 形如 a.b.c".to_string());
    }
    for seg in path.split('.') {
        let mut chars = seg.chars();
        let ok_first = chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
        let ok_rest = chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
        if seg.is_empty() || !ok_first || !ok_rest {
            return Err(format!(
                "路径段 '{seg}' 非法 — 每段须为 [A-Za-z_][A-Za-z0-9_]*（下标访问不在 v1 语法内）"
            ));
        }
    }
    Ok(())
}

/// `for` 头部解析：`for <var> in <path>`（var 为单段名，不含点）。
fn parse_for_header(rest: &str) -> Result<(String, String), String> {
    let parts: Vec<&str> = rest.split_whitespace().collect();
    if parts.len() != 3 || parts[1] != "in" {
        return Err(format!(
            "语法须为 'for <var> in <path>'，得到 '{rest}' — var 为单段名（不含点），in 为关键字"
        ));
    }
    let (var, path) = (parts[0], parts[2]);
    if var.contains('.') || validate_path(var).is_err() {
        return Err(format!(
            "循环变量 '{var}' 须为单段名 [A-Za-z_][A-Za-z0-9_]*"
        ));
    }
    validate_path(path)?;
    Ok((var.to_string(), path.to_string()))
}

// ============================================================================
// 渲染（AST + context → 输出；纯函数，同输入逐字节同输出）
// ============================================================================

/// 解析并渲染模板（缺省形态 text）。
pub fn render(src: &str, context: &JsonValue) -> Result<String, String> {
    render_checked(src, context, Format::Text).map(|r| r.content)
}

/// 解析并渲染模板，按声明形态做产出校验（`json` = 产物必须可解析为 JSON）。
pub fn render_checked(src: &str, context: &JsonValue, format: Format) -> Result<Rendered, String> {
    let nodes = parse(src)?;
    let mut out = String::new();
    let mut iters = 0usize;
    let mut scope: Vec<(&str, &JsonValue)> = Vec::new();
    walk(&nodes, context, &mut scope, &mut out, &mut iters)?;
    if format == Format::Json {
        if let Err(e) = serde_json::from_str::<serde_json::Value>(&out) {
            let excerpt: String = out.chars().take(200).collect();
            return Err(format!(
                "format=json 但渲染产物不是合法 JSON: {e} — \
                 字符串值含引号/特殊字符时请勿直接拼入 JSON 文本（v1 无自动转义）；产物前 200 字符: {excerpt}"
            ));
        }
    }
    Ok(Rendered {
        format,
        content: out,
    })
}

/// 路径解析：最内层循环作用域优先，再查 context 根；逐段对象字段取值。
fn resolve<'v>(
    path: &str,
    root: &'v JsonValue,
    scope: &[(&str, &'v JsonValue)],
) -> Option<&'v JsonValue> {
    let mut segs = path.split('.');
    let first = segs.next()?;
    let mut cur: &JsonValue = match scope.iter().rev().find(|(k, _)| *k == first) {
        Some((_, v)) => v,
        None => root.get(first)?,
    };
    for seg in segs {
        cur = cur.get(seg)?;
    }
    Some(cur)
}

/// 条件真值：解析成功且非 null 且非布尔假（`0`/`""`/`[]` 为真）。
fn truthy(v: &JsonValue) -> bool {
    !v.is_null() && v.as_bool() != Some(false)
}

/// 值字符串化：String 原样 / Integer 十进制 / Bool true|false /
/// Array|Object 紧凑 JSON 形态 / Null 拒绝（fail-fast）。
fn stringify(v: &JsonValue) -> Result<String, String> {
    match v {
        JsonValue::String(s) => Ok(s.to_string()),
        JsonValue::Integer(i) => Ok(i.to_string()),
        JsonValue::Bool(b) => Ok(b.to_string()),
        JsonValue::Array(_) | JsonValue::Object(_) => Ok(format!("{v}")),
        JsonValue::Null => {
            Err("路径解析为 null — 替换点不接受 null（确定性引擎不容忍未定义值）".to_string())
        }
    }
}

/// `nodes` 借出循环变量名（`'a`），`root` 借出循环元素（`'b`）——
/// scope 栈经 `&mut Vec` 混存两者，须显式命名以通过不变性检查。
fn walk<'a, 'b>(
    nodes: &'a [Node],
    root: &'b JsonValue,
    scope: &mut Vec<(&'a str, &'b JsonValue)>,
    out: &mut String,
    iters: &mut usize,
) -> Result<(), String> {
    for node in nodes {
        match node {
            Node::Text(t) => push(out, t)?,
            Node::Subst { path, pos } => {
                let v = resolve(path, root, scope).ok_or_else(|| {
                    format!(
                        "变量未找到: '{path}'（{}）— 替换点要求路径在 context 中存在且非 null \
                        （v1 无静默空串回退）",
                        pos.describe()
                    )
                })?;
                let s = stringify(v)?;
                push(out, &s)?;
            }
            Node::If {
                cond_path,
                then_body,
                else_body,
            } => {
                let branch: &[Node] = match resolve(cond_path, root, scope) {
                    // 路径缺失 = 假（存在性分支语义，唯一的缺失宽容点）
                    None => else_body,
                    Some(v) if truthy(v) => then_body,
                    Some(_) => else_body,
                };
                walk(branch, root, scope, out, iters)?;
            }
            Node::For {
                var,
                path,
                pos,
                body,
            } => {
                let arr = resolve(path, root, scope)
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| {
                        format!(
                            "for 迭代源 '{path}'（{}）不是数组（缺失或非数组均 fail-fast，不静默空迭代）",
                            pos.describe()
                        )
                    })?;
                for element in arr {
                    *iters += 1;
                    if *iters > MAX_LOOP_ITERATIONS {
                        return Err(format!(
                            "循环总迭代数超出上限 {MAX_LOOP_ITERATIONS} — \
                             执行预算保护（放大器防护，fail-fast）"
                        ));
                    }
                    scope.push((var.as_str(), element));
                    let walked = walk(body, root, scope, out, iters);
                    scope.pop();
                    walked?;
                }
            }
        }
    }
    Ok(())
}

fn push(out: &mut String, chunk: &str) -> Result<(), String> {
    if out.len() + chunk.len() > MAX_OUTPUT_BYTES {
        return Err(format!(
            "渲染产物超出上限 {MAX_OUTPUT_BYTES} 字节 — 执行预算保护（fail-fast，非静默截断）"
        ));
    }
    out.push_str(chunk);
    Ok(())
}

// ============================================================================
// 测试（逐字节断言 = 确定性验收口径）
// ============================================================================

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn ctx(pairs: &[(&str, JsonValue)]) -> JsonValue {
        JsonValue::object_from_pairs(pairs)
    }

    fn r(tpl: &str, context: &JsonValue) -> String {
        render(tpl, context).unwrap()
    }

    // ===== 文本直通 =====

    #[test]
    fn test_plain_text_passthrough() {
        assert_eq!(r("没有任何标签的纯文本", &ctx(&[])), "没有任何标签的纯文本");
        assert_eq!(r("", &ctx(&[])), "", "空模板渲染为空串（合法）");
    }

    // ===== 变量替换与值字符串化 =====

    #[test]
    fn test_substitution_scalar_stringification() {
        let c = ctx(&[
            ("name", JsonValue::string("张三")),
            ("age", JsonValue::Integer(30)),
            ("vip", JsonValue::Bool(true)),
            ("off", JsonValue::Bool(false)),
        ]);
        assert_eq!(r("你好, {{name}}!", &c), "你好, 张三!");
        // 标签内首尾空白忽略
        assert_eq!(
            r("{{ name }}/{{age}}/{{vip}}/{{off}}", &c),
            "张三/30/true/false"
        );
    }

    #[test]
    fn test_substitution_array_object_json_shape() {
        let c = ctx(&[
            (
                "tags",
                JsonValue::Array(vec![JsonValue::string("a"), JsonValue::Integer(1)]),
            ),
            (
                "meta",
                ctx(&[("k", JsonValue::string("v")), ("n", JsonValue::Integer(2))]),
            ),
        ]);
        // Array/Object 以紧凑 JSON 形态字符串化（确定性；键序 = 排序序）
        assert_eq!(r("{{tags}}", &c), "[\"a\", 1]");
        assert_eq!(r("{{meta}}", &c), "{\"k\": \"v\", \"n\": 2}");
    }

    #[test]
    fn test_substitution_dotted_path() {
        let c = ctx(&[(
            "user",
            ctx(&[("profile", ctx(&[("city", JsonValue::string("上海"))]))]),
        )]);
        assert_eq!(r("{{user.profile.city}}", &c), "上海");
    }

    #[test]
    fn test_substitution_missing_path_fails_fast() {
        let err = render("hi {{nope}}", &ctx(&[])).unwrap_err();
        assert!(err.contains("nope"), "{err}");
        assert!(err.contains("变量未找到"), "{err}");
        // 位置诊断
        assert!(err.contains("第 1 行"), "{err}");
    }

    #[test]
    fn test_substitution_null_fails_fast() {
        let c = ctx(&[("n", JsonValue::Null)]);
        let err = render("x={{n}}", &c).unwrap_err();
        assert!(err.contains("null"), "{err}");
    }

    #[test]
    fn test_loop_var_shadowing_and_root_access() {
        // 循环变量遮蔽同名根键；体内仍可访问根
        let c = ctx(&[
            ("x", JsonValue::string("root")),
            (
                "items",
                JsonValue::Array(vec![JsonValue::Integer(1), JsonValue::Integer(2)]),
            ),
        ]);
        assert_eq!(r("{% for x in items %}[{{x}}]{% endfor %}", &c), "[1][2]");
        assert_eq!(
            r("{{x}}{% for x in items %}{{x}}{% endfor %}{{x}}", &c),
            "root12root"
        );
    }

    // ===== if / else =====

    #[test]
    fn test_if_else_branching() {
        let c = ctx(&[
            ("vip", JsonValue::Bool(true)),
            ("off", JsonValue::Bool(false)),
        ]);
        assert_eq!(r("{% if vip %}A{% else %}B{% endif %}", &c), "A");
        assert_eq!(r("{% if off %}A{% else %}B{% endif %}", &c), "B");
    }

    #[test]
    fn test_if_missing_path_is_falsy() {
        // 存在性分支：路径缺失按假处理（if 的唯一宽容点，契约级）
        let c = ctx(&[]);
        assert_eq!(r("{% if any.thing %}A{% else %}B{% endif %}", &c), "B");
        // 0 / 空串 / 空数组 均为真（非 null 且非布尔假）
        let c2 = ctx(&[
            ("zero", JsonValue::Integer(0)),
            ("empty", JsonValue::string("")),
            ("arr", JsonValue::Array(vec![])),
        ]);
        assert_eq!(
            r(
                "{% if zero %}T{% endif %}{% if empty %}T{% endif %}{% if arr %}T{% endif %}",
                &c2
            ),
            "TTT"
        );
    }

    #[test]
    fn test_if_without_else() {
        let c = ctx(&[("on", JsonValue::Bool(true))]);
        assert_eq!(r("x{% if on %}+{% endif %}", &c), "x+");
        let c2 = ctx(&[]);
        assert_eq!(r("x{% if on %}+{% endif %}", &c2), "x");
    }

    // ===== for =====

    #[test]
    fn test_for_basic_and_field_access() {
        let c = ctx(&[(
            "orders",
            JsonValue::Array(vec![
                ctx(&[
                    ("id", JsonValue::Integer(1)),
                    ("total", JsonValue::Integer(30)),
                ]),
                ctx(&[
                    ("id", JsonValue::Integer(2)),
                    ("total", JsonValue::Integer(45)),
                ]),
            ]),
        )]);
        let tpl = "{% for o in orders %}| {{o.id}} | {{o.total}} |\n{% endfor %}";
        assert_eq!(r(tpl, &c), "| 1 | 30 |\n| 2 | 45 |\n");
    }

    #[test]
    fn test_for_scalar_elements_and_nested_loops() {
        let c = ctx(&[(
            "rows",
            JsonValue::Array(vec![
                JsonValue::Array(vec![JsonValue::Integer(1), JsonValue::Integer(2)]),
                JsonValue::Array(vec![JsonValue::Integer(3), JsonValue::Integer(4)]),
            ]),
        )]);
        let tpl = "{% for row in rows %}{% for cell in row %}({{cell}}){% endfor %};{% endfor %}";
        assert_eq!(r(tpl, &c), "(1)(2);(3)(4);");
    }

    #[test]
    fn test_for_missing_or_non_array_fails_fast() {
        let err = render("{% for x in nope %}y{% endfor %}", &ctx(&[])).unwrap_err();
        assert!(err.contains("nope") && err.contains("不是数组"), "{err}");
        let c = ctx(&[("s", JsonValue::string("str"))]);
        let err = render("{% for x in s %}y{% endfor %}", &c).unwrap_err();
        assert!(err.contains("不是数组"), "{err}");
    }

    // ===== 语法错误（解析期 fail-fast）=====

    #[test]
    fn test_parse_errors_unclosed_and_mismatched() {
        // 未闭合变量标签
        let err = render("a {{ b", &ctx(&[])).unwrap_err();
        assert!(err.contains("未闭合"), "{err}");
        // 未闭合控制块
        let err = render("{% if a %}b", &ctx(&[])).unwrap_err();
        assert!(err.contains("endif"), "{err}");
        // 多余 endif
        let err = render("a{% endif %}", &ctx(&[])).unwrap_err();
        assert!(err.contains("多余"), "{err}");
        // 嵌套标签
        let err = render("{{ a {{ b }} }}", &ctx(&[])).unwrap_err();
        assert!(err.contains("嵌套"), "{err}");
    }

    #[test]
    fn test_parse_errors_bad_paths_and_unknown_tag() {
        let err = render("{{1abc}}", &ctx(&[])).unwrap_err();
        assert!(err.contains("非法"), "{err}");
        let err = render("{% if %}x{% endif %}", &ctx(&[])).unwrap_err();
        assert!(err.contains("路径非法") || err.contains("为空"), "{err}");
        let err = render("{% for x of items %}x{% endfor %}", &ctx(&[])).unwrap_err();
        assert!(err.contains("for"), "{err}");
        let err = render("{% while x %}y{% endwhile %}", &ctx(&[])).unwrap_err();
        assert!(err.contains("未知"), "{err}");
        // 循环变量不允许点分
        let c = ctx(&[("items", JsonValue::Array(vec![]))]);
        let err = render("{% for a.b in items %}x{% endfor %}", &c).unwrap_err();
        assert!(err.contains("单段名"), "{err}");
    }

    // ===== 执行预算 =====

    #[test]
    fn test_loop_iteration_budget() {
        let n = MAX_LOOP_ITERATIONS as i64 + 1;
        let items = JsonValue::Array((0..n).map(JsonValue::Integer).collect());
        let c = ctx(&[("items", items)]);
        let err = render("{% for x in items %}x{% endfor %}", &c).unwrap_err();
        assert!(err.contains("上限"), "{err}");
    }

    // ===== 产出形态 =====

    #[test]
    fn test_format_parse_and_json_validation() {
        assert!(matches!(Format::parse("text"), Ok(Format::Text)));
        assert!(matches!(Format::parse("markdown"), Ok(Format::Markdown)));
        assert!(matches!(Format::parse("json"), Ok(Format::Json)));
        assert!(Format::parse("yaml").is_err());

        // 合法 JSON 产物
        let c = ctx(&[
            ("name", JsonValue::string("张三")),
            ("vip", JsonValue::Bool(true)),
        ]);
        let tpl = "{\"customer\": \"{{name}}\", \"vip\": {{vip}}}";
        let out = render_checked(tpl, &c, Format::Json).unwrap();
        assert_eq!(out.format, Format::Json);
        assert_eq!(out.content, "{\"customer\": \"张三\", \"vip\": true}");

        // 非法 JSON 产物 → fail-fast（v1 无自动转义，含引号字符串拼 JSON 必炸且明确报错）
        let c2 = ctx(&[
            ("name", JsonValue::string("a\"b")),
            ("vip", JsonValue::Bool(true)),
        ]);
        let err = render_checked(tpl, &c2, Format::Json).unwrap_err();
        assert!(err.contains("不是合法 JSON"), "{err}");
        // 同一模板 text 形态不受 JSON 校验约束（如实产出）
        assert_eq!(
            render_checked(tpl, &c2, Format::Text).unwrap().content,
            "{\"customer\": \"a\"b\", \"vip\": true}"
        );
    }

    // ===== 确定性：同输入逐字节同输出 + 端到端黄金值 =====

    #[test]
    fn test_deterministic_repeat_calls() {
        let c = ctx(&[("u", ctx(&[("name", JsonValue::string("李四"))]))]);
        let tpl = "# 单 {{u.name}}\n{% if u.name %}有名字{% endif %}";
        let a = render_checked(tpl, &c, Format::Markdown).unwrap();
        let b = render_checked(tpl, &c, Format::Markdown).unwrap();
        assert_eq!(a, b, "同输入必须逐字节同输出");
    }

    #[test]
    fn test_golden_markdown_statement_byte_exact() {
        let c = ctx(&[(
            "user",
            ctx(&[
                ("name", JsonValue::string("张三")),
                ("vip", JsonValue::Bool(true)),
                (
                    "orders",
                    JsonValue::Array(vec![
                        ctx(&[
                            ("id", JsonValue::Integer(1)),
                            ("total", JsonValue::Integer(30)),
                        ]),
                        ctx(&[
                            ("id", JsonValue::Integer(2)),
                            ("total", JsonValue::Integer(45)),
                        ]),
                    ]),
                ),
            ]),
        )]);
        let tpl =
            "# 对账单 {{user.name}}\n\n{% if user.vip %}VIP 客户{% else %}普通客户{% endif %}\n\n\
                   | 订单 | 金额 |\n|---|---|\n\
                   {% for o in user.orders %}| {{o.id}} | {{o.total}} |\n{% endfor %}";
        let expected =
            "# 对账单 张三\n\nVIP 客户\n\n| 订单 | 金额 |\n|---|---|\n| 1 | 30 |\n| 2 | 45 |\n";
        assert_eq!(r(tpl, &c), expected);
    }

    #[test]
    fn test_cjk_values_byte_fidelity_and_ascii_path_rule() {
        // 中文值逐字节保真
        let c = ctx(&[("title", JsonValue::string("季度报告"))]);
        assert_eq!(r("报告: {{title}}", &c), "报告: 季度报告");
        // 路径段仅限 ASCII（中文键经值承载，不进路径语法）
        let err = render("{{ 标题 }}", &ctx(&[])).unwrap_err();
        assert!(err.contains("非法"), "{err}");
    }
}

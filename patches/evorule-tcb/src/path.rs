// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 路径解析 - 支持点号分隔 + 数组索引，永不 panic
//!
//! # 路径语法 (ABNF)
//! ```text
//! path           = segment *( "." segment )
//! segment        = identifier [ "[" index "]" ]
//! identifier     = 1*(ALPHA / DIGIT / "_" / "-")
//! index          = 1*DIGIT
//! ```
//!
//! # 转义规则
//! - 字段名中的 `.` 需用 `\.` 转义
//! - 字段名中的 `[` 需用 `\[` 转义
//!
//! # 示例
//! - `__exec__.payload.items[0].value` → 访问数组第 0 个元素的 value 字段
//! - `data\.version` → 字段名为 `data.version`
//!
//! # 保证
//! - 永不 panic
//! - 任何解析失败返回 `None`

use crate::value::JsonValue;
use alloc::string::String;
use alloc::vec::Vec;

/// 解析路径，返回值的引用（若存在）
///
/// # 示例
///
/// ```
/// extern crate alloc;
/// use evorule_tcb::JsonValue;
/// use evorule_tcb::path::resolve_path;
/// use alloc::collections::BTreeMap;
///
/// // 构造嵌套状态
/// let mut inner = BTreeMap::new();
/// inner.insert("value".to_string(), JsonValue::Integer(42));
/// let mut mid = BTreeMap::new();
/// mid.insert("inner".to_string(), JsonValue::object(inner));
/// let state = JsonValue::object(mid);
///
/// // 路径解析
/// assert_eq!(resolve_path(&state, "inner.value").and_then(|v| v.as_i64()), Some(42));
/// assert_eq!(resolve_path(&state, "missing"), None);
/// assert_eq!(resolve_path(&state, ""), None);
/// ```
pub fn resolve_path<'a>(state: &'a JsonValue, path: &str) -> Option<&'a JsonValue> {
    if path.is_empty() {
        return None;
    }

    // 检查尾部点号（如 "x."）
    if path.ends_with('.') {
        return None;
    }

    let segments = parse_path_segments(path)?;
    let mut current = state;

    for seg in &segments {
        match seg {
            PathSegment::Field(field) => {
                current = match current {
                    JsonValue::Object(map) => map.get(field.as_str())?,
                    _ => return None,
                };
            }
            PathSegment::Index(field, idx) => {
                // 先访问字段（若存在），再访问索引
                let target = if let Some(f) = field {
                    match current {
                        JsonValue::Object(map) => map.get(f.as_str())?,
                        _ => return None,
                    }
                } else {
                    current
                };
                current = match target {
                    JsonValue::Array(arr) => arr.get(*idx)?,
                    _ => return None,
                };
            }
        }
    }

    Some(current)
}

/// 解析路径，返回值的可变引用（若存在）
pub fn resolve_path_mut<'a>(state: &'a mut JsonValue, path: &str) -> Option<&'a mut JsonValue> {
    if path.is_empty() {
        return None;
    }

    if path.ends_with('.') {
        return None;
    }

    let segments = parse_path_segments(path)?;
    resolve_path_mut_inner(state, &segments)
}

/// 解析相对 `__exec__` 上下文的路径（统一路径约定，v0.3.2 起）
///
/// domain 的 `path` 使用本函数（`__exec__` 相对路径为全引擎统一约定）：
///
/// - `__exec__.` 开头：strip 前缀后从 `__exec__` 节点解析（绝对路径兼容写法）
/// - 其他写法：自动补全 `__exec__.` 前缀解析（相对路径，如 `payload.x`、
///   `instruction.type`、`queue[0].type`）
///
/// 解析失败返回 `None`（调用方决定求值语义：domain 视为 false，
/// 状态修改类指令转为显式 `PathResolutionFailed`）。
pub(crate) fn resolve_exec_path<'a>(state: &'a JsonValue, path: &str) -> Option<&'a JsonValue> {
    let stripped = path.strip_prefix("__exec__.").unwrap_or(path);
    let exec = state.get("__exec__")?;
    resolve_path(exec, stripped)
}

/// 内部递归实现（可变路径解析）
fn resolve_path_mut_inner<'a>(
    state: &'a mut JsonValue,
    segments: &[PathSegment],
) -> Option<&'a mut JsonValue> {
    // 空段列表：返回当前状态（递归终止）
    if segments.is_empty() {
        return Some(state);
    }

    // 使用 split_first 避免 indexing_slicing（永不 panic）
    let (first, rest) = segments.split_first()?;

    match first {
        PathSegment::Field(field) => {
            let obj = state.as_object_mut()?;
            let value = obj.get_mut(field.as_str())?;
            resolve_path_mut_inner(value, rest)
        }
        PathSegment::Index(field, idx) => {
            let target = if let Some(f) = field {
                let obj = state.as_object_mut()?;
                obj.get_mut(f.as_str())?
            } else {
                state
            };
            let arr = target.as_array_mut()?;
            let value = arr.get_mut(*idx)?;
            resolve_path_mut_inner(value, rest)
        }
    }
}

/// 路径段（解析后的中间表示）
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PathSegment {
    /// 字段访问（如 `payload`）
    Field(String),
    /// 索引访问（如 `items[0]` 或 `[0]`，field 为 None 时表示纯索引）
    Index(Option<String>, usize),
}

/// 解析路径字符串为段列表
///
/// 语法约束：
/// - 点号分隔符前后必须有字段名，唯一例外是 `]` 之后的**第一个**点号
///   （允许 `items[0].name` 与 `data.[0]` 两种等价写法）；
/// - 索引段后的连续点号（如 `items[0]..name`）非法。
pub(crate) fn parse_path_segments(path: &str) -> Option<Vec<PathSegment>> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut chars = path.chars().peekable();
    let mut escaped = false;
    // 上一个处理的字符是否为索引段的 ']'：
    // 仅其后的第一个点号允许 current 为空（消耗后立即复位，拒绝连续点号）
    let mut just_closed_index = false;

    while let Some(c) = chars.next() {
        if escaped {
            // 转义字符：原样保留
            current.push(c);
            just_closed_index = false;
            escaped = false;
            continue;
        }

        if c == '\\' {
            escaped = true;
            continue;
        }

        if c == '.' {
            // 点号分隔符：空 current 仅在刚闭合索引段时合法
            let after_index = just_closed_index;
            just_closed_index = false;
            if current.is_empty() && !after_index {
                // 真正的空段（如 "x..y"、".x" 或 "x[0]..y"），非法
                return None;
            }
            if !current.is_empty() {
                segments.push(PathSegment::Field(core::mem::take(&mut current)));
            }
            continue;
        }

        if c == '[' {
            // 索引开始
            let field = if current.is_empty() {
                None
            } else {
                Some(core::mem::take(&mut current))
            };
            // 解析索引直到 ']'，验证闭合括号存在
            let mut idx_str = String::new();
            let mut found_close = false;
            for ic in chars.by_ref() {
                if ic == ']' {
                    found_close = true;
                    break;
                }
                idx_str.push(ic);
            }
            if !found_close {
                // 缺少闭合 ']'（如 "[0" 或 "items[0"），非法
                return None;
            }
            if idx_str.is_empty() {
                return None;
            }
            let idx: usize = idx_str.parse().ok()?;
            segments.push(PathSegment::Index(field, idx));
            just_closed_index = true;
            // ']' 后必须是 '.', '[', 或字符串结束（拒绝 "[0]abc" 等非法拼接）
            match chars.peek() {
                None | Some('.' | '[') => {}
                _ => return None,
            }
            continue;
        }

        // 普通字符
        current.push(c);
        just_closed_index = false;
    }

    // 处理末尾
    if escaped {
        // 转义未结束（如 "x\. "），非法
        return None;
    }
    // 以点号结尾的路径（如 "x."）属于非法语法：点号是分隔符，必须有后续段。
    // 此处 current 必为空（点号在前面已被消耗并清空 current），但 segments 可能非空，
    // 若不检查会被静默接受为前缀路径，掩盖拼写错误。
    if current.is_empty() && !just_closed_index {
        return None;
    }
    if !current.is_empty() {
        segments.push(PathSegment::Field(current));
    }

    if segments.is_empty() {
        return None;
    }

    Some(segments)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    #![allow(clippy::panic)]
    #![allow(clippy::indexing_slicing)]

    use super::*;
    use crate::value::{JsonValue, ObjectMap};
    use alloc::string::ToString;
    use alloc::vec;

    // ===== 基础路径解析测试 =====

    #[test]
    fn test_resolve_path_simple() {
        let mut map = ObjectMap::new();
        map.insert("x".to_string(), JsonValue::Integer(42));
        let state = JsonValue::Object(map);

        assert_eq!(resolve_path(&state, "x"), Some(&JsonValue::Integer(42)));
        assert_eq!(resolve_path(&state, "y"), None);
        assert_eq!(resolve_path(&state, ""), None);
        assert_eq!(resolve_path(&state, "x."), None);
    }

    #[test]
    fn test_resolve_path_nested() {
        let mut inner = ObjectMap::new();
        inner.insert("value".to_string(), JsonValue::Integer(99));
        let mut outer = ObjectMap::new();
        outer.insert("data".to_string(), JsonValue::Object(inner));
        let state = JsonValue::Object(outer);

        assert_eq!(
            resolve_path(&state, "data.value"),
            Some(&JsonValue::Integer(99))
        );
        assert_eq!(resolve_path(&state, "data.missing"), None);
    }

    #[test]
    fn test_resolve_path_array_index() {
        let arr = JsonValue::array(vec![
            JsonValue::Integer(10),
            JsonValue::Integer(20),
            JsonValue::Integer(30),
        ]);
        let state = arr;

        assert_eq!(resolve_path(&state, "[0]"), Some(&JsonValue::Integer(10)));
        assert_eq!(resolve_path(&state, "[2]"), Some(&JsonValue::Integer(30)));
        assert_eq!(resolve_path(&state, "[5]"), None);
        assert_eq!(resolve_path(&state, "[-1]"), None);
    }

    #[test]
    fn test_resolve_path_mixed() {
        let mut item1 = ObjectMap::new();
        item1.insert("name".to_string(), JsonValue::string("alpha"));
        let mut item2 = ObjectMap::new();
        item2.insert("name".to_string(), JsonValue::string("beta"));
        let items = JsonValue::array(vec![JsonValue::Object(item1), JsonValue::Object(item2)]);
        let mut outer = ObjectMap::new();
        outer.insert("items".to_string(), items);
        let state = JsonValue::Object(outer);

        assert_eq!(
            resolve_path(&state, "items[0].name"),
            Some(&JsonValue::string("alpha"))
        );
        assert_eq!(
            resolve_path(&state, "items[1].name"),
            Some(&JsonValue::string("beta"))
        );
        assert_eq!(resolve_path(&state, "items[2].name"), None);
    }

    #[test]
    fn test_resolve_path_escaped() {
        let mut map = ObjectMap::new();
        map.insert("data.version".to_string(), JsonValue::Integer(42));
        let state = JsonValue::Object(map);

        assert_eq!(
            resolve_path(&state, "data\\.version"),
            Some(&JsonValue::Integer(42))
        );
        assert_eq!(resolve_path(&state, "data.version"), None);
    }

    // ===== resolve_path_mut 测试 =====

    #[test]
    fn test_resolve_path_mut_basic() {
        let mut map = ObjectMap::new();
        map.insert("x".to_string(), JsonValue::Integer(42));
        let mut state = JsonValue::Object(map);

        if let Some(value) = resolve_path_mut(&mut state, "x") {
            *value = JsonValue::Integer(100);
        }
        assert_eq!(resolve_path(&state, "x"), Some(&JsonValue::Integer(100)));
    }

    #[test]
    fn test_resolve_path_mut_nested() {
        let mut inner = ObjectMap::new();
        inner.insert("value".to_string(), JsonValue::Integer(1));
        let mut outer = ObjectMap::new();
        outer.insert("data".to_string(), JsonValue::Object(inner));
        let mut state = JsonValue::Object(outer);

        if let Some(value) = resolve_path_mut(&mut state, "data.value") {
            *value = JsonValue::Integer(99);
        }
        assert_eq!(
            resolve_path(&state, "data.value"),
            Some(&JsonValue::Integer(99))
        );
    }

    #[test]
    fn test_resolve_path_mut_array_element() {
        let mut state = JsonValue::array(vec![
            JsonValue::Integer(10),
            JsonValue::Integer(20),
            JsonValue::Integer(30),
        ]);

        if let Some(value) = resolve_path_mut(&mut state, "[1]") {
            *value = JsonValue::Integer(200);
        }
        assert_eq!(resolve_path(&state, "[1]"), Some(&JsonValue::Integer(200)));
    }

    #[test]
    fn test_resolve_path_mut_nonexistent_returns_none() {
        let mut map = ObjectMap::new();
        map.insert("x".to_string(), JsonValue::Integer(42));
        let mut state = JsonValue::Object(map);

        assert!(resolve_path_mut(&mut state, "y").is_none());
        assert!(resolve_path_mut(&mut state, "").is_none());
        assert!(resolve_path_mut(&mut state, "x.").is_none());
    }

    // ===== 边界情况测试 =====

    #[test]
    fn test_resolve_path_special_chars_in_field_name() {
        let mut map = ObjectMap::new();
        map.insert("field_name".to_string(), JsonValue::Integer(1));
        map.insert("field-name".to_string(), JsonValue::Integer(2));
        map.insert("__exec__".to_string(), JsonValue::Integer(3));
        let state = JsonValue::Object(map);

        assert_eq!(
            resolve_path(&state, "field_name"),
            Some(&JsonValue::Integer(1))
        );
        assert_eq!(
            resolve_path(&state, "field-name"),
            Some(&JsonValue::Integer(2))
        );
        assert_eq!(
            resolve_path(&state, "__exec__"),
            Some(&JsonValue::Integer(3))
        );
    }

    #[test]
    fn test_resolve_path_empty_index_returns_none() {
        let state = JsonValue::array(vec![JsonValue::Integer(1)]);
        assert_eq!(resolve_path(&state, "[]"), None);
    }

    #[test]
    fn test_resolve_path_invalid_index_returns_none() {
        let state = JsonValue::array(vec![JsonValue::Integer(1)]);
        assert_eq!(resolve_path(&state, "[abc]"), None);
    }

    #[test]
    fn test_resolve_path_double_dot_returns_none() {
        let mut inner = ObjectMap::new();
        inner.insert("y".to_string(), JsonValue::Integer(1));
        let mut map = ObjectMap::new();
        map.insert("x".to_string(), JsonValue::Object(inner));
        let state = JsonValue::Object(map);

        assert_eq!(resolve_path(&state, "x..y"), None);
    }

    #[test]
    fn test_resolve_path_consecutive_dots_after_index_returns_none() {
        // L1 回归：索引段后仅允许一个点号，连续点号非法
        let mut inner = ObjectMap::new();
        inner.insert("name".to_string(), JsonValue::string("alpha"));
        let items = JsonValue::array(vec![JsonValue::Object(inner)]);
        let mut outer = ObjectMap::new();
        outer.insert("items".to_string(), items);
        let state = JsonValue::Object(outer);

        assert_eq!(resolve_path(&state, "items[0]..name"), None);
        assert_eq!(resolve_path(&state, "items[0]...name"), None);
        // 纯索引开头同理
        let arr = JsonValue::array(vec![JsonValue::array(vec![JsonValue::Integer(1)])]);
        assert_eq!(resolve_path(&arr, "[0]..x"), None);
        // 合法对照：单点号仍可用
        assert_eq!(
            resolve_path(&state, "items[0].name"),
            Some(&JsonValue::string("alpha"))
        );
    }

    #[test]
    fn test_resolve_path_leading_dot_returns_none() {
        let mut map = ObjectMap::new();
        map.insert("x".to_string(), JsonValue::Integer(1));
        let state = JsonValue::Object(map);

        assert_eq!(resolve_path(&state, ".x"), None);
    }

    #[test]
    fn test_resolve_path_on_non_object_returns_none() {
        let state = JsonValue::Integer(42);
        assert_eq!(resolve_path(&state, "x"), None);
        assert_eq!(resolve_path(&state, "[0]"), None);
    }

    #[test]
    fn test_resolve_path_on_non_array_returns_none() {
        let mut map = ObjectMap::new();
        map.insert("x".to_string(), JsonValue::Integer(1));
        let state = JsonValue::Object(map);

        assert_eq!(resolve_path(&state, "[0]"), None);
    }

    #[test]
    fn test_resolve_path_escaped_bracket() {
        let mut map = ObjectMap::new();
        map.insert("data[0]".to_string(), JsonValue::Integer(42));
        let state = JsonValue::Object(map);

        assert_eq!(
            resolve_path(&state, "data\\[0]"),
            Some(&JsonValue::Integer(42))
        );
    }

    #[test]
    fn test_resolve_path_missing_closing_bracket_returns_none() {
        let state = JsonValue::array(vec![JsonValue::Integer(1)]);
        assert_eq!(resolve_path(&state, "[0"), None);
    }

    #[test]
    fn test_resolve_path_missing_closing_bracket_with_field_returns_none() {
        let mut inner = ObjectMap::new();
        inner.insert("name".to_string(), JsonValue::string("alpha"));
        let items = JsonValue::array(vec![JsonValue::Object(inner)]);
        let mut outer = ObjectMap::new();
        outer.insert("items".to_string(), items);
        let state = JsonValue::Object(outer);

        assert_eq!(resolve_path(&state, "items[0"), None);
    }

    #[test]
    fn test_resolve_path_no_separator_after_bracket_returns_none() {
        let state = JsonValue::array(vec![JsonValue::Integer(1)]);
        assert_eq!(resolve_path(&state, "[0]abc"), None);
    }

    #[test]
    fn test_resolve_path_no_separator_after_bracket_with_field_returns_none() {
        let mut inner = ObjectMap::new();
        inner.insert("name".to_string(), JsonValue::string("alpha"));
        let items = JsonValue::array(vec![JsonValue::Object(inner)]);
        let mut outer = ObjectMap::new();
        outer.insert("items".to_string(), items);
        let state = JsonValue::Object(outer);

        assert_eq!(resolve_path(&state, "items[0]name"), None);
    }

    #[test]
    fn test_resolve_path_nested_array_indexing() {
        let row0 = JsonValue::array(vec![JsonValue::Integer(10), JsonValue::Integer(20)]);
        let row1 = JsonValue::array(vec![JsonValue::Integer(30), JsonValue::Integer(40)]);
        let matrix = JsonValue::array(vec![row0, row1]);
        let mut outer = ObjectMap::new();
        outer.insert("matrix".to_string(), matrix);
        let state = JsonValue::Object(outer);

        assert_eq!(
            resolve_path(&state, "matrix[0][1]"),
            Some(&JsonValue::Integer(20))
        );
        assert_eq!(
            resolve_path(&state, "matrix[1][0]"),
            Some(&JsonValue::Integer(30))
        );
    }

    #[test]
    fn test_resolve_path_dot_then_bracket_works() {
        let arr = JsonValue::array(vec![JsonValue::Integer(10), JsonValue::Integer(20)]);
        let mut outer = ObjectMap::new();
        outer.insert("data".to_string(), arr);
        let state = JsonValue::Object(outer);

        assert_eq!(
            resolve_path(&state, "data.[0]"),
            Some(&JsonValue::Integer(10))
        );
        assert_eq!(
            resolve_path(&state, "data[0]"),
            Some(&JsonValue::Integer(10))
        );
    }

    #[test]
    fn test_resolve_path_index_with_field_on_non_object_returns_none() {
        let state = JsonValue::array(vec![JsonValue::Integer(42)]);
        assert_eq!(resolve_path(&state, "a[0]"), None);
    }

    #[test]
    fn test_resolve_path_mut_index_with_field_on_non_object_returns_none() {
        let mut state = JsonValue::array(vec![JsonValue::Integer(42)]);
        assert_eq!(resolve_path_mut(&mut state, "a[0]"), None);
    }

    #[test]
    fn test_resolve_path_mut_index_with_missing_field_on_object_returns_none() {
        let mut state = JsonValue::Object(ObjectMap::new());
        assert_eq!(resolve_path_mut(&mut state, "a[0]"), None);
    }

    #[test]
    fn test_resolve_path_trailing_backslash_returns_none() {
        let state = JsonValue::Object(ObjectMap::new());
        assert_eq!(resolve_path(&state, "x\\"), None);
    }

    #[test]
    fn test_resolve_path_only_backslash_returns_none() {
        let state = JsonValue::Object(ObjectMap::new());
        assert_eq!(resolve_path(&state, "\\"), None);
    }

    #[test]
    fn test_parse_path_segments_empty_returns_none() {
        assert_eq!(parse_path_segments(""), None);
    }

    // ===== 额外测试：点号后跟索引 =====

    #[test]
    fn test_resolve_path_dot_before_bracket_works() {
        let arr = JsonValue::array(vec![JsonValue::Integer(10), JsonValue::Integer(20)]);
        let mut outer = ObjectMap::new();
        outer.insert("data".to_string(), arr);
        let state = JsonValue::Object(outer);

        assert_eq!(
            resolve_path(&state, "data.[0]"),
            Some(&JsonValue::Integer(10))
        );
        assert_eq!(
            resolve_path(&state, "data.[1]"),
            Some(&JsonValue::Integer(20))
        );
    }
}

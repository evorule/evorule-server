<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: AGPL-3.0-or-later

  This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
-->

# evorule-rule-schema

**规则 Schema 门禁 —— 基于固化的 evorule-system-rules v1.0 Schema 的确定性校验**

> **crate 类型**: 内部 lib（`publish = false`，不进 crates.io）
> **引入版本**: v0.3.0
> **防御层级**: 线1（规则进入引擎前的结构校验）

---

## 定位

TCB（`evorule-tcb`）不设防：只保证确定性执行，不保证用户规则正确性。

**本 crate 是 evorule-server 侧的防御层**：在任何规则进入引擎前，用固化 Schema 拦截结构非法的规则并给出明确提示。

Schema 源文件：`schemas/` 下三个文件（`rule_set` / `_meta` / `_shared`），与 `evorule-system-rules` 仓保持同步（跨仓一致性由 `scripts/check_schema_sync.py` 守护）。

---

## 两种校验模式

### 1. `validate_rule_set`

完整 `rule_set` 文档（5 标注字段 + `transform[]`），对应：
- `rule_translate` 输出
- `hot_reload` 加载
- API 提交的完整规则文件

### 2. `validate_transform_list`

裸 `transform` 数组（引擎 native 结构），对应：
- server 从规则文件抽取 transform 数组
- 裸数组入参

---

## Schema 文件结构

```
schemas/
├── rule_set/v1.0.json    # 完整规则集 Schema（allOf 引用 meta + shared）
├── _meta/v1.0.json       # 元数据字段 Schema（id/name/version/author/description）
└── _shared/v1.0.json     # 共享定义 Schema（transform_rule / domain / meta_instruction 等）
```

**$id 自洽**：
- `rule_set`: `https://evorule.org/schemas/rule_set/v1.0.json`
- `_meta`: `https://evorule.org/schemas/_meta/v1.0.json`
- `_shared`: `https://evorule.org/schemas/_shared/v1.0.json`

---

## 构建期门禁（build.rs）

`build.rs` 在编译时校验：
1. 三个 schema 文件必须是合法 JSON
2. 跨文件 `$ref` 的 `$id` 自洽（不漂移）
3. `rule_set` 的 `allOf[0]` 必须指向 `meta`
4. `transform.items` 必须指向 `shared#/$defs/transform_rule`

把"schema 损坏"从运行时问题提前到构建期问题（与转译器同一纪律）。

**C5 纪律**: build.rs 本身禁止 `unwrap`/`expect`/`panic`（deny 级 lint），所有失败路径统一以 `Err(String)` 返回。

---

## 正确性保证

- 元指令/域类型/必填参数等全部由 Schema 表达（SSOT），不维护手写常量表
- 构建期由 `build.rs` 校验三文件合法性 + `$id` 自洽
- 运行期缓存校验器（`once_cell::sync::Lazy`）
- 最大 transform 规则数 = 64（与 TCB `MAX_TRANSFORM_RULES` 一致；schema `maxItems` 亦约束）
- 真实文件合规性由 `evorule-system-rules` `_verify_schemas.py` 闭环验证

---

## 校验结果

```rust
pub struct SchemaReport {
    pub valid: bool,           // 是否通过
    pub mode: &'static str,    // 校验模式：rule_set / transform_list
    pub errors: Vec<String>,   // 错误列表（每条含实例路径 + 具体原因）
}
```

错误信息包含 JSON Pointer 实例路径（如 `/transform/0/params/attr`），供上层显示明确提示。

---

## 依赖

- `serde` + `serde_json` — JSON 序列化
- `once_cell` — 校验器静态缓存（MSRV 1.74，std `LazyLock` 需 1.80）
- `jsonschema` 0.21 — JSON Schema 2020-12 校验器（`Draft202012`）

---

## 相关文件

- [GATE_REFERENCE.md](../../GATE_REFERENCE.md) §3.4 — 本 crate 的 build.rs 门禁说明
- [INTEGRATION_GUIDE.md](../../docs/INTEGRATION_GUIDE.md) §4.1 — 规则校验使用说明
- `scripts/check_schema_sync.py` — 跨仓 Schema 同步检查脚本

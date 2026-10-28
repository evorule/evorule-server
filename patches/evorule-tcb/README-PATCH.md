# vendored patch：evorule-tcb 0.10.0 + in 域

## 是什么

`[patch.crates-io]` 路径补丁（根 Cargo.toml）：以本目录的 evorule-tcb 替换
crates.io 注册表实例（全局单实例替换，无双实例类型冲突；evorule-reactor /
evorule-governance 同机制先例）。基线 = 主仓 v0.10.0（crates.io 已发布版）
逐文件拷贝（2026-10-28 换基重铸：0.8.4 registry 基线 + 累积随行差异 → 整体
重铸为主仓 0.10.0 基线，版本号声明 0.10.0）。

## 为什么

会话模型准入白名单需要多模型集合表达——部署方按自身接入策略圈定
模型清单，同一规则须同时放行集合内任一模型。现有 DSL 域类型仅 eq/lt，
多条 enforce 为 AND 语义无法表达「或」集合。新增 `in` 域类型（集合成员判定）。
governance 升 0.10.0 后其依赖链（reactor ^0.10 → tcb）要求 tcb 0.10.0，
旧 0.8.4 基线与注册表 0.10.0 版本互斥（[patch] 全局替换下解析必炸），故整体换基重铸。

## 差异面（相对基线 0.10.0，全部为增量、无既有行为改动）

| 文件 | 改动 |
|---|---|
| `src/domain.rs` | ① dispatch 加 `"in"` 分支；② 新增 `evaluate_in_outcome`（三态：含→Sat/不含→Unsat/路径缺→按 on_missing；空集→Unsat；字符串元素命中根段点形态→Missing(ValueLiteralAmbiguous)，复用 `is_root_segment_dot_path` 谓词零镜像）；③ 模块 doc 与 evaluate_domain doc 域清单 7→8；④ tests 增 in 组 10 件（sat/unsat/整数成员/空集 Unsat/缺省 unsat 投影/on_missing=error 拒执行/values 缺失 Err/values 非数组 Err/根段点元素歧义/确定性幂等） |

语义裁定（design-19 R-2/R-3，2026-10-16 用户批准）：
- values 元素 = 纯字面量（不做 `__` 引用解析）
- 空集运行时 Unsat（数学语义）；装载面另行拒收空集（fail-fast）
- on_missing 语义与 eq/lt 完全复用（error/unsat 两值 + 缺省兼容 unsat）

旧 0.8.4 基线差异面中已与主仓 0.10.0 同形的项（`JsonValue::U64` 全域通道、
`json_type_name` U64 分支、executor.rs 算术词汇表 doc）随换基吸收进基线，
不再列差异面。

## vendored 化机械差异（非语义）

- workspace 继承字面量化（version/edition/license/authors/repository 等显式声明）
- 尾部空 `[workspace]` 表（脱离 server 工作区，先例同）
- 去 `[lints] workspace = true` 继承（lib.rs 内 `#![deny(...)]` 内层属性兜底）
- 移除打包工件（.cargo-ok / .cargo_vcs_info.json / Cargo.toml.orig）
- 注释/文档内内部设计档编号标记清除（公开面扫描 B 类阻断项；删编号留正文，
  零语义，主仓同类清洗先例）

## 测试边界

- server 工作区 `cargo test` **不含**本补丁的测试（[patch] 包不是 workspace 成员）。
  在本目录独立跑：`cargo test --lib`（自带空 workspace 表）。
- server 侧消费面另有门禁测试（server.rs domain 门禁组 + rule_loader 门禁 +
  plugin_packs 词表 + ai-plugin 白名单解析）覆盖 in 域装载与收集语义。
- 换基基线（2026-10-28 实测）：**lib 274 passed / 0 failed**（含 in 组 10 件 +
  u64 通道 4 件）。

## 删除条件（硬性）

主仓发布版本**含同形 in 域 API** 时：

1. 核对 API 形态一致（签名/语义对照本文件差异面表）；
2. 删根 Cargo.toml `[patch.crates-io]` 中 `evorule-tcb` 行；
3. 删本目录；
4. `cargo update -p evorule-tcb` 拉注册表版；
5. 全量测试绿后随批提交。

届时若主仓 API 形态与本补丁有出入，以主仓为准改造 server 侧接线后再删。

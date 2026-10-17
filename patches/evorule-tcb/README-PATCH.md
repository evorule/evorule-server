# vendored patch：evorule-tcb 0.8.4 + in 域

## 是什么

`[patch.crates-io]` 路径补丁（根 Cargo.toml）：以本目录的 evorule-tcb 替换
crates.io 注册表实例（全局单实例替换，无双实例类型冲突；evorule-reactor /
evorule-governance 同机制先例）。基线 = crates.io `evorule-tcb 0.8.4`
（registry 缓存逐文件拷贝，机械零漂移）。

## 为什么

会话模型准入白名单需要多模型集合表达（MiniMax-M2.5 / MiniMax-M3 /
GLM-5.3-Flash）。现有 DSL 域类型仅 eq/lt，多条 enforce 为 AND 语义无法
表达「或」集合。新增 `in` 域类型（集合成员判定）。主仓 0.8.4 为已发布版，
等主仓后续版本发布前以仓内补丁先行——不经主仓发版即可解锁白名单规则改写。

## 差异面（相对基线 0.8.4，全部为增量、无既有行为改动）

| 文件 | 改动 |
|---|---|
| `src/domain.rs` | ① dispatch 加 `"in"` 分支；② 新增 `evaluate_in_outcome`（三态：含→Sat/不含→Unsat/路径缺→按 on_missing；空集→Unsat；字符串元素命中根段点形态→Missing(ValueLiteralAmbiguous)，复用 `is_root_segment_dot_path` 谓词零镜像）；③ 模块 doc 与 evaluate_domain doc 域清单 7→8；④ tests 增 in 组 10 件（sat/unsat/整数成员/空集 Unsat/缺省 unsat 投影/on_missing=error 拒执行/values 缺失 Err/values 非数组 Err/根段点元素歧义/确定性幂等） |

语义裁定（design-19 R-2/R-3，2026-10-16 用户批准）：
- values 元素 = 纯字面量（不做 `__` 引用解析）
- 空集运行时 Unsat（数学语义）；装载面另行拒收空集（fail-fast）
- on_missing 语义与 eq/lt 完全复用（error/unsat 两值 + 缺省兼容 unsat）

## vendored 化机械差异（非语义）

- 基线为 registry normalized Cargo.toml（字段已字面量化，无需去 workspace 继承）
- 尾部空 `[workspace]` 表（脱离 server 工作区，先例同）
- 移除打包工件（.cargo-ok / .cargo_vcs_info.json / Cargo.toml.orig / Cargo.lock）
- 注释/文档内内部批次编号标记清除（公开面扫描 B 类阻断项；删编号留正文，
  零语义，主仓同类清洗先例）；CHANGE_REQUEST.md 同口径不入公开仓（主仓惯例，
  本地留档，build.rs CR 门禁已移出公开仓无编译依赖）

## 测试边界

- server 工作区 `cargo test` **不含**本补丁的测试（[patch] 包不是 workspace 成员）。
  在本目录独立跑：`cargo test --lib`（自带空 workspace 表）。
- server 侧消费面另有门禁测试（server.rs domain 门禁组 + rule_loader 门禁 +
  plugin_packs 词表 + ai-plugin 白名单解析）覆盖 in 域装载与收集语义。

## 删除条件（硬性）

主仓发布版本**含同形 in 域 API** 时：

1. 核对 API 形态一致（签名/语义对照本文件差异面表）；
2. 删根 Cargo.toml `[patch.crates-io]` 中 `evorule-tcb` 行；
3. 删本目录；
4. `cargo update -p evorule-tcb` 拉注册表版；
5. 全量测试绿后随批提交。

届时若主仓 API 形态与本补丁有出入，以主仓为准改造 server 侧接线后再删。

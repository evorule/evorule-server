# evorule-governance vendored patch — 差异面说明

## 基线与来源

- **基线**：crates.io `evorule-governance 0.8.5`（主仓 workspace 0.8.5 同源）
- **本副本**：主仓 commit `a2e14cf`（安全路由与网关 SG-2 批：I/O 路由策略资产化——
  静态收紧层与 ask 无人值守回退）
- **同步纪律**：主仓 governance 后续变更合入本批消费面时，须同步本副本并在下表登记

## 相对 0.8.5 的 API 差异面（SG-2 批）

| 变更 | 文件 | 消费方 |
|---|---|---|
| 新增 `RoutePolicy`（caller_role × io_type × target → allow/deny/ask 三元决策表，纯函数判定） | `src/permission/route_policy.rs`（新） | `route_policy_loader.rs` |
| `PermissionGate::with_route_policy` / `check_routed`（裁决序：静态 deny 终局、ask 无人值守回退 deny、ask+human 与 allow 落动态表；静态层无放行权） | `src/permission/gate.rs` | server 三处 gate 装配点 |
| `IoSubscriber` deny 回写 error 串拼 route 段（`route=deny` / `route=ask-fallback`，确定性构造） | `src/io_subscriber.rs` | J-SG2 判据（负向可区分） |
| `permission::mod` 导出 RoutePolicy 族 | `src/permission/mod.rs` | — |

## 相对基线 `0a4d592` 的全量文件级差异（2026-10-24 实测复核，11 文件）

> 行数为相对基线 `0a4d592`（批 H）的增/删（行尾规范化后 diff）。
> **同步纪律执行现状**：此前本表只登记 SG-2 四件，漏登下列 7 文件与能力族，本次补全。

| 文件 | ±行 | 内容 |
|---|---|---|
| `session.rs` | +499/−143 | **G6 规则集锚定族**（`with_ruleset_version`/`set_ruleset_version`/`append_ruleset_anchor`/`ruleset_version_anchored_into_chain`）+ **rehydrate 族**（`rehydrate_session_from_wal` + `auditor_from_config` + 4 测试）+ **tick 步进族**（`tick_instruction`/`submit_tick`/`wait_version_stable`）；消费方=server checkpoint/规则集锚定/会话恢复 |
| `auditor.rs` | +184/−2 | **锚点持久化族**（`persist_anchor`/`load_anchor_log`/`with_anchor_persisted`/`with_anchor_impl`）+ import_compressed 解压限额与口径 doc |
| `shared_facts_log.rs` | +14/−0 | recover 前置链三查 + 共享账本 ed25519 锚点链（载荷含 metadata 摘要）+ 锚点持久化入 metadata |
| `io_subscriber.rs` | +5/−109 | deny 回写 route 段；大幅收缩为 SG-2 后形态 |
| `permission/gate.rs` | +9/−34 | SG-2 静态层接线（`check_routed` 裁决序） |
| `circuit_breaker.rs` | +4/−10 | 收敛 |
| `permission/route_policy.rs` | +3/−12 | 微调 |
| `anchor.rs` | +15/−2 | 锚点链配合 G6 |
| `signing.rs` | +0/−9 | 移除 9 行 |
| `metrics.rs` | +2/−2 | 微调 |
| `rule_validation.rs` | +3/−1 | 微调+注释指正（业务语义校验层在 server 侧 core/semantic_invariants） |

**主仓有而本补丁无（退役条件，非本补丁缺陷）**：时钟缝族（`with_clock`/`reap_expired_at`/`max_session_id_from_names`）——主仓 `0a4d592` 后新增；本补丁退役前提=主仓先吸收 G6 锚定/rehydrate/tick 三族，否则删补丁即丢 server 在用能力。`verify_deep` 两侧签名同形（主仓已吸收先例）。

## vendored 化机械差异（非语义）

- `[package]` workspace 继承字段固定为 0.8.5 对应值
- `[lints] workspace = true` 移除（独立 crate 不继承消费方 lints）
- `evorule-tcb` / `evorule-reactor` path 依赖改纯 version（reactor 经消费方
  `[patch.crates-io]` 解析到 vendored 0.8.4，与 ^0.8.3 兼容）

## 删除条件

主仓发布 ≥0.8.6 且含同形 API → 删 server 仓 `[patch.crates-io]` 中
`evorule-governance` 行 + 本目录，依赖回纯 version，`cargo update` 验证一致。

## 基线升级记录（2026-10-21，A-2 核对单首战）

**evorule-governance：基线升级至内核 main `0a4d592`（批 H）**
- 同步面：`shared_facts_log.rs`（recover 前置链三查+共享账本 ed25519 锚点链[载荷含 metadata 摘要]+锚点持久化入 metadata）、`auditor.rs`（import_compressed 解压限额+口径 doc）
- **重放 server 专属改动**：`rotate_wal`（干预原语 wal.force_rotate 通路——server checkpoint 端点消费）
- 测试门：patch workspace 234/0（--features persistence）；主 workspace 1608/0
- 哈希等价：跨仓黄金断言（主仓 tests/golden_hash_cross_repo.rs）持续在役
- 触发依据：批 H 为行为级变更（recover 语义+WAL 锚定）——不升级则跨仓链不互证窗口期开启

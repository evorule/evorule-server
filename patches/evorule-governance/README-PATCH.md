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

## vendored 化机械差异（非语义）

- `[package]` workspace 继承字段固定为 0.8.5 对应值
- `[lints] workspace = true` 移除（独立 crate 不继承消费方 lints）
- `evorule-tcb` / `evorule-reactor` path 依赖改纯 version（reactor 经消费方
  `[patch.crates-io]` 解析到 vendored 0.8.4，与 ^0.8.3 兼容）

## 删除条件

主仓发布 ≥0.8.6 且含同形 API → 删 server 仓 `[patch.crates-io]` 中
`evorule-governance` 行 + 本目录，依赖回纯 version，`cargo update` 验证一致。

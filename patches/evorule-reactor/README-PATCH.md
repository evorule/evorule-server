# vendored patch：evorule-reactor 0.10.0 + 干预原语

## 是什么

`[patch.crates-io]` 路径补丁（根 Cargo.toml）：以本目录的 evorule-reactor 替换
crates.io 注册表实例（全局单实例替换，无双实例类型冲突；0.6.1/0.7 线同机制先例）。
基线 = 主仓 v0.10.0（crates.io 已发布版）逐文件拷贝（2026-10-28 换基重铸：
0.8.4 旧基线 + 累积随行差异 → 整体重铸为主仓 0.10.0 基线，版本号声明 0.10.0）。

## 为什么

会话级干预端点族（invariants/scan、io/cancel_all）需要三个原语：
`check_invariants` 提 pub、`cancel_all_pending` 新原语（+快照 pending 清单 +
不变式扫描槽）。governance 升 0.10.0 后其依赖 reactor ^0.10，旧 0.8.4 基线
与注册表 0.10.0 版本互斥（[patch] 全局替换下解析必炸），故整体换基重铸。

## 差异面（相对基线 0.10.0，全部为增量、无既有行为改动）

| 文件 | 改动 |
|---|---|
| `src/pure.rs` | `check_invariants` `pub(crate)` → `pub`（可见性提升） |
| `src/reactor.rs` | ① `ReactorStateSnapshot` 增 `pending_io: Vec<PendingIoEntry>`（每迭代头随快照刷新，空表零分配）；② `Reactor`/`ReactorBuilder`/`ReactorHandle` 增 `cancel_all_flag` 共享标志 + `cancel_all_pending()`；主循环迭代头 0.55 位消费（逐 pending 请求 emit `Fact::Error` "cancelled by external request" + `force_remove_io_request`，镜像超时路径，不发 Stable）；③ 增 `InvariantScanReport` 共享报告 + `latest_invariants` 槽（每迭代头由 `run_invariant_check` 返回值刷新）+ `ReactorHandle::latest_invariant_scan()`；`run_invariant_check` 改为返回违规明细（既有计数+日志行为不变） |
| `src/lib.rs` | 导出 `InvariantScanReport` |
| `Cargo.toml` | workspace 继承字面量化；`evorule-tcb` req "0.10.0" 无 path；新增 `evorule-audit-chain = { version = "0.10.0" }`（主仓 SC3-a 方案乙随迁：fact.rs/hash.rs 已为 audit-chain 薄转发层）；去 `[lints]` 继承；尾部空 `[workspace]` 表（脱离 server 工作区）；`[[test]] ssot_fact_contract_gate` 契约闸门声明 + `[patch.crates-io]` tcb 重定向（仅独立自测态生效，patch 不嵌套） |
| 测试 | 增 `cancel_all_pending_clears_requests_and_emits_errors`（真实 pending 全链：快照清单可见→取消→清零→Error 入链→扫描干净）；增 `tests/ssot_fact_contract_gate.rs`（Fact 契约黄金向量自证闸门 + 重生成器，见「测试边界」）；增 `tests/fixtures/ssot_fact_contract_golden.json`（冻结黄金向量：11 样本 stable_json + fact_hash，双仓共享对账物） |

governance 不打补丁：`Session.handle` / `Session.facts_log` 均为 pub，server 直接触达。

旧 0.8.4 基线差异面中已与主仓 0.10.0 同形的项（`WalWriter::rotate` 提 pub、
`FactsLog::rotate_wal`、u64 全域通道、M-1 WAL 头格式戳、时钟缝 `runtime` 模块、
`channel_local`）随换基吸收进基线，不再列差异面。

## 测试边界

- server 工作区 `cargo test` **不含**本补丁的测试（[patch] 包不是 workspace 成员）。
  在本目录独立跑：`cargo test --lib --features persistence`（自带空 workspace 表 +
  独立 lock；--features persistence 与根仓消费态同构——workspace 内 governance 等
  成员开启 reactor/persistence，feature unification 后本补丁持久化面在役，WAL/
  rotate 族测试可达。不带该 feature 时 `FactsLog::recover` 等门控 API 不存在，
  facts_log 测试编译失败属预期，非缺陷）。
- **Fact 契约黄金向量闸门**（在役）：`tests/ssot_fact_contract_gate.rs`
  断言副本 live 哈希契约 == 冻结向量 `tests/fixtures/ssot_fact_contract_golden.json`
  （11 样本：9 Fact 变体 + u64::MAX-1 哨兵 id + u64 值通道；断言面仅
  `fact_to_stable_json` / `fact_hash`，无 feature 门控）。对向闸门在
  evorule-integration-tests（`include_str!` 跨仓读同一向量，SSOT 侧
  `evorule-audit-chain` live 复算对账）——**双向棘轮**：任一侧漂移即红，
  共同契约物变更须在本目录重生成向量并双仓同批提交留痕。本仓 CI 的
  `vendored-reactor-gate` job 强阻塞。
- 重生成器（契约有意演进时显式执行）：
  `cargo test --test ssot_fact_contract_gate regenerate_golden_fixture -- --ignored --nocapture`
- 换基基线（2026-10-28 实测）：**lib 187 passed / 0 failed**（--features
  persistence）+ 契约黄金向量闸 `fact_contract_matches_frozen_golden_fixture` 绿。
- server 侧消费端点另有集成测试（`intervention_*` 5 件）覆盖门控/语义/404 面。

## 删除条件（硬性）

主仓发布含同形 API 的版本（`cancel_all_pending` / `InvariantScanReport` /
`pending_io` 快照面 / `check_invariants` pub）时：

1. 核对 API 形态一致（签名/语义对照本文件差异面表）；
2. 删根 Cargo.toml `[patch.crates-io]` 段；
3. 删本目录；
4. `cargo update -p evorule-reactor` 拉注册表版；
5. 全量测试绿后随批提交。

届时若主仓 API 形态与本补丁有出入，以主仓为准改造 server 侧接线后再删。

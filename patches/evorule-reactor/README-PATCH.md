# vendored patch：evorule-reactor 0.8.4 + 干预原语

## 是什么

`[patch.crates-io]` 路径补丁（根 Cargo.toml）：以本目录的 evorule-reactor 替换
crates.io 注册表实例（全局单实例替换，无双实例类型冲突；0.6.1/0.7 线同机制先例）。
基线 = 主仓 v0.8.4（`e489a66`，crates.io 已发布版）逐文件拷贝。

## 为什么

会话级干预端点族（invariants/scan、io/cancel_all、wal/force_rotate）需要三个
原语：`check_invariants` 提 pub、`WalWriter::rotate` 提 pub + FactsLog 透传、
`cancel_all_pending` 新原语（+快照 pending 清单 + 不变式扫描槽）。主仓 0.8.4
为 CI 门禁补丁版（"No API changes"），等下一版发布前以仓内补丁先行——不经
主仓发版即可解锁端点接线。

## 差异面（相对基线 0.8.4，全部为增量、无既有行为改动）

| 文件 | 改动 |
|---|---|
| `src/pure.rs` | `check_invariants` `pub(crate)` → `pub`（可见性提升） |
| `src/wal.rs` | `WalWriter::rotate` 提 `pub`；`FactWalStore` trait 增 `rotate`（默认无操作返回 `Ok(false)`）；`WalWriter` 实现（委托具体方法，返回 `Ok(true)`）；v0.9.0 同步另增：`tcb_to_serde` 补 `JsonValue::U64` 分支（serde Number 无损承载）+ `serde_to_tcb` 回程超 i64 范围 u64 由转字符串改为 `U64` 变体无损承载（WAL 展示面往返保形，回放重算 fact_hash 不失配；小值 U64 经 serde 往返退化为 Integer 属 serde_json Number 无 tag 固有约束，数值无损） |
| `src/facts_log.rs` | 增 `FactsLog::rotate_wal()`（persistence 门控；写锁透传 trait rotate；错误映射 `WalError` → `FactsLogError::WalError`） |
| `src/fact.rs` | v0.9.0 主仓同步（u64 全域无损）：`to_json` 17 处 `as i64` cast → `JsonValue::u64()`（展示面 FactId/cause/request_id/version/index/rule_index 高位值不再回绕为负；哈希输入 `fact_to_stable_json` 不经本函数，哈希口径零扰动）+ doc u64 无损注记 + 哨兵 id 无损回归测试 |
| `src/hash.rs` | v0.9.0 主仓同步：`tcb_to_serde` 补 `JsonValue::U64` 分支（serde Number 无损承载，不经 i64 cast）+ u64 无损回归测试 |
| `src/reactor.rs` | ① `ReactorStateSnapshot` 增 `pending_io: Vec<PendingIoEntry>`（每迭代头随快照刷新，空表零分配）；② `Reactor`/`ReactorBuilder`/`ReactorHandle` 增 `cancel_all_flag` 共享标志 + `cancel_all_pending()`；主循环迭代头 0.55 位消费（逐 pending 请求 emit `Fact::Error` "cancelled by external request" + `force_remove_io_request`，镜像超时路径，不发 Stable）；③ 增 `InvariantScanReport` 共享报告 + `latest_invariants` 槽（每迭代头由 `run_invariant_check` 返回值刷新）+ `ReactorHandle::latest_invariant_scan()`；`run_invariant_check` 改为返回违规明细（既有计数+日志行为不变） |
| `src/lib.rs` | 导出 `InvariantScanReport` |
| `Cargo.toml` | workspace 继承字面量化；`evorule-tcb` 去 path 保留 version；去 `[lints]` 继承；尾部空 `[workspace]` 表（脱离 server 工作区）；v0.9.0 同步另增：`[[test]] ssot_fact_contract_gate` 契约闸门声明 + `[patch.crates-io]` tcb 重定向（仅独立自测态生效，patch 不嵌套） |
| 测试 | 增 `cancel_all_pending_clears_requests_and_emits_errors`（真实 pending 全链：快照清单可见→取消→清零→Error 入链→扫描干净）；增 `tests/ssot_fact_contract_gate.rs`（Fact 契约黄金向量自证闸门 + 重生成器，见「测试边界」）；增 `tests/fixtures/ssot_fact_contract_golden.json`（冻结黄金向量：11 样本 stable_json + fact_hash，双仓共享对账物） |

governance 不打补丁：`Session.handle` / `Session.facts_log` 均为 pub，server 直接触达。

## 测试边界

- server 工作区 `cargo test` **不含**本补丁的测试（[patch] 包不是 workspace 成员）。
  在本目录独立跑：`cargo test --lib`（自带空 workspace 表 + 独立 lock）。
- **独立自测恢复（跨仓契约闸批次）**：
  - tcb 源：本目录 `[patch.crates-io]` 将 `evorule-tcb` 重定向至仓内 vendored
    副本 `../evorule-tcb`（0.8.4 + u64 全域通道；注册表 0.8.4 缺 `U64` 变体
    编译不通过）。被根仓 `[patch.crates-io]` 以本目录为补丁源消费时本节不
    生效（patch 不嵌套），两态互不干扰。
  - **Fact 契约黄金向量闸门**（v0.9.0 起在役）：`tests/ssot_fact_contract_gate.rs`
    断言副本 live 哈希契约 == 冻结向量 `tests/fixtures/ssot_fact_contract_golden.json`
    （11 样本：9 Fact 变体 + u64::MAX-1 哨兵 id + u64 值通道；断言面仅
    `fact_to_stable_json` / `fact_hash`，无 feature 门控）。对向闸门在
    evorule-integration-tests（`include_str!` 跨仓读同一向量，SSOT 侧
    `evorule-audit-chain` live 复算对账）——**双向棘轮**：任一侧漂移即红，
    共同契约物变更须在本目录重生成向量并双仓同批提交留痕。本仓 CI 的
    `vendored-reactor-gate` job 强阻塞。
  - 重生成器（契约有意演进时显式执行）：
    `cargo test --test ssot_fact_contract_gate regenerate_golden_fixture -- --ignored --nocapture`
- 独立全量基线（v0.8.4 时代，历史留痕）：**140 passed / 3 failed**——3 件失败
  （`pure::tests::test_next_step_*`）为环境性缺夹具（读 sibling 路径
  `../evorule-tcb/core_eval.json`，仅主仓布局存在），非补丁引入；主仓内同测试
  基线绿。新增测试 1 件绿。
- server 侧消费端点另有集成测试（`intervention_*` 5 件）覆盖门控/语义/404 面。

## 删除条件（硬性）

主仓发布 **≥0.8.5 且含同形 API** 时：

1. 核对 API 形态一致（签名/语义对照本文件差异面表）；
2. 删根 Cargo.toml `[patch.crates-io]` 段；
3. 删本目录；
4. `cargo update -p evorule-reactor` 拉注册表版；
5. 全量测试绿后随批提交。

届时若主仓 API 形态与本补丁有出入，以主仓为准改造 server 侧接线后再删。

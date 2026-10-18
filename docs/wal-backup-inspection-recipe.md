# WAL 备份与巡检配方（P1 W-2）

三通路 WAL（会话 / 单反应器 / 共享事实）的备份与巡检操作配方。所有端点、
指标、CLI 旗标均对应 evorule-server 已实现行为；本文不引入新机制。

前置条件：

- 服务端已配置 `--wal-dir`（或 `EVORULE_WAL_DIR` / 配置文件 `paths.wal_dir`）。
  未配置时三通路纯内存，重启即全部丢失，本文不适用。
- 备份操作者持有平台管理员凭据（checkpoint 为人工治理动作，自动化凭据
  一律 403）。

## 1. 备份配方

### 1.1 一致性锚点：先 checkpoint 再拷贝

```text
POST /api/admin/wal/checkpoint
Authorization: Bearer <平台管理员 token>
```

checkpoint 对三通路各执行一次纯追加式 WAL 分片切换（崩溃安全，不影响链
内容与可回放性）。拷贝前执行的意义：分片边界干净——正在写的活动分片
停止增长，后续写入落在新分片，备份集是一份自洽的静态快照。

响应为逐项上报（HTTP 恒 200）：

```json
{
  "sessions":     { "total": 0, "rotated": 0, "in_memory": 0, "failed": 0, "items": [] },
  "reactor":      { "rotated": true },
  "shared_facts": { "rotated": true }
}
```

判读：`rotated:false` = 该通路纯内存/无可轮换后端（非失败）；带 `error`
的项为轮换失败（partial failure，其余通路不受牵连）。存在 error 项时应
先排障（见 §3）再备份，避免把故障状态固化进备份集。

单会话的手工轮换（非全通道）走 `POST /api/sessions/{id}/wal/rotate`。

### 1.2 拷贝范围

checkpoint 后对 `wal_dir` 全量拷贝：

| 文件 | 角色 |
| --- | --- |
| `*.wal` 及分片 `*.N.wal`（含 `session_*.wal*`、`governance_single_reactor.wal*`、`shared_facts.wal*`） | 审计链本体，权威数据源 |
| `shared_facts_meta.json` | 共享事实辅助索引（fact_sources / next_fact_id / rollup 标记等） |

要点：

- **必须整目录拷贝**：WAL 恢复按序读全部分片，缺分片即断链。
- metadata 缺失不致命（重启后 recover 以 WAL 为准重建事实），但会损失
  消费关系 / rollup 标记等治理元数据——照常拷贝，不挑拣。
- 拷贝目标为异地存储（另一主机 / 对象存储）。备份集保留多副本时按
  checkpoint 时间点命名，保证每份副本内部分片集合自洽。
- 频率建议：随 checkpoint 节奏执行（如每日低峰一次）；RPO 要求更高时
  加密频次。活动分片在两次 checkpoint 之间的增量不在备份集内，属于
  已接受的丢失窗口。

### 1.3 恢复

将备份集整目录回填 `wal_dir` 后重启服务即可：

- WAL 存在则 `recover` 重放续链（绝不 truncate）；不存在则新建。
- 共享事实经 `shared_facts.wal` + `shared_facts_meta.json` 完整恢复。
- 重启后建议以 `--verify-wal-on-start` 启动一次，确认恢复副本全链
  校验通过（`/api/health` 的 `wal.verify` 节应为 `ok`，见 §2.1）。

## 2. 巡检配方

### 2.1 health 巡检（GET /api/health）

`wal_dir` 已配置且观察面已注册时，响应含 `wal` 节：

```json
{
  "wal": {
    "checkpoint": { "total": 0, "failed": 0, "last_success_at_ms": 0 },
    "storage":    { "files": 0, "total_bytes": 0, "free_bytes": 0, "sampled_at_ms": 0 },
    "verify":     { "reactor": "skipped", "shared_facts": "skipped" }
  }
}
```

- `checkpoint.failed`：存在通路轮换失败的 checkpoint 次数，应恒为 0。
- `storage.free_bytes`：分区剩余空间（后台 60s 轻采样；获取失败省略，
  不造数）。
- `verify`：启动期全链校验位（`--verify-wal-on-start` 开启才有 ok/failed，
  否则 skipped）。**巡检期望生产部署恒开该旗标**；值 `failed` 表示启动
  时已发现链损坏（服务不拒启——取证保护现场），须立即人工介入。

### 2.2 Prometheus 指标

| 指标 | 含义 | 巡检口径 |
| --- | --- | --- |
| `evorule_wal_rotate_total{channel,result}` | 轮换调用计数 | `result="error"` 序列出现即有轮换失败 |
| `evorule_wal_checkpoint_last_success_timestamp_seconds` | 最近全成功 checkpoint 时刻 | 距今超过巡检周期即 checkpoint 失效 |
| `evorule_wal_file_bytes{file}` | 文件大小（shared_facts / reactor / sessions 有界标签） | 增长速率异常（暴涨/停滞）需关注 |
| `evorule_wal_free_bytes` | 分区剩余空间 | 低于水位线告警（衔接 §3） |

### 2.3 告警建议

| 条件 | 级别 | 动作 |
| --- | --- | --- |
| `rotate_total{result="error"}` 增长 | 高 | 按 §3 排障 |
| `checkpoint` `failed` > 0 或 `last_success` 超龄 | 高 | 按 §3 排障 |
| `free_bytes` 低于水位（建议按 WAL 日增量的 3-5 倍设定） | 中 | 扩容/清理，防磁盘满 |
| `/api/health` `wal.verify` = `failed` | 高 | 立即停写、保全现场、人工核查 |
| `verify` 长期为 `skipped` | 低 | 补开 `--verify-wal-on-start` |

## 3. 磁盘满应对

症状链（阈值机制，SSOT=WAL 写入连续失败终止阈值 3）：

1. WAL 写失败错误携带连续计数（`#N`）；
2. 连续失败达 3 次后错误升级，携带「磁盘 / 权限」自助诊断指导，并触发
   升级回调（反应器侧据此终止会话并发射 Error fact）；
3. `evorule_wal_rotate_total{result="error"}` 与 health `checkpoint.failed`
   同步可观测。

处置：清理/扩容磁盘分区 → 无需人工干预数据，写入自动恢复（此前失败
的调用方需自行重试）。磁盘满演练已由集成测试覆盖（`wal_disk_full_drill_*`）。

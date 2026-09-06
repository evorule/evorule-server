# 版本与升级兼容策略(精简版)

> 完整版与证据:evorule-project knowledge 仓 ops/版本与升级兼容策略.md;本文件为 server 侧落地同步版(UV-031)。
> 原则:升级前必冷备+演练(scripts/acceptance-backup-restore.ps1);恢复失败拒绝启动是特性不是缺陷。

## 数据层兼容契约

| 层 | 机制 | 契约 |
|---|---|---|
| WAL(data\wal\,JSON-lines+BLAKE3 行 hash) | 无格式版本头,兼容性取决于 Fact JSON 序列化 | 同 minor 版本保证可恢复;跨 minor 升级前冷备,恢复失败→拒绝启动,处置见知识库 runbook §4.1 |
| SQLite(evorule.db/workspace.db) | 幂等建表+ALTER 前滚 | 新版开旧库自动补结构;不支持降级 |
| 宪法(resources/server_eval.json,v0.4.x;0.4.1 前旧名 core_eval.json) | 启动期 fail-fast 校验关键指令规则(call_external 等)+ 旧名兼容检测给迁移指引(UV-044) | 缺规则拒绝启动+三步自诊断;剧本自持于消费方,宪法版本演进记录于文件 metadata |

## 破坏性变更惯例

- 0.x 阶段 minor 位即大版本(0.4→0.5 可能破坏性);patch 只做修复
- 破坏性变更须:CHANGELOG 顶部 `**BREAKING**` 标记 + 迁移步骤 + 配套版本声明 + 重跑演练脚本

## 升级操作清单

1. 冷备 data\(含 wal 整目录原子复制)
2. 部署新二进制(同 minor 可直接替换;跨 minor 先核对 CHANGELOG BREAKING)
3. 启动并断言:/api/health 200 → /api/audit-archive/sessions 历史会话 fact_count 与备份一致
4. 异常处置:恢复失败按错误指引操作;回退需连同备份的 data 一并回滚

## 已知缺口

- WAL 建议加 magic+version header(版本不匹配如实报告)——待排期
- 数据库 schema 版本号缺失(降级无检测)——待排期

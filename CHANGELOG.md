<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: AGPL-3.0-or-later
-->

# EvoRule Server 更新日志

所有对 EvoRule Server 仓的重大更改都将记录在此文件中。

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.0.0/) v1.0,
本项目遵循 [语义化版本控制](https://semver.org/lang/zh-CN/) v2.0。

徽章说明:

- 🆕 新增
- 🔄 变更
- 🐛 修复
- 🗑 弃用
- ⚠️ Breaking Change
- 🔒 安全
- 🧪 测试
- ✅ 向后兼容
- 📚 文档

---

## [0.4.1] - 2026-09-02

### 🔄 变更

- **核心引擎依赖 evorule-tcb / evorule-reactor / evorule-governance 0.4.0 → 0.4.1（UV-046 核心仓 P0 处置）**

### ⚠️ Breaking Change

- **`GET /api/sessions/:id/diff` 版本不可达返回 400** — 随核心仓 `time_machine::diff` 显式化（UV-046 B8b：不再静默回退空 payload），`a`/`b` 版本不可达时由"空 diff"改为 `400 BAD_REQUEST`（与 rewind 端点同语义）

### 🧪 测试

- 新增 `test_session_diff_unreachable_version`（不可达版本 → 400 断言）

---

## [0.4.0] - 2026-09-02

### ⚠️ Breaking Changes

- **核心引擎依赖 evorule-tcb / evorule-reactor / evorule-governance 0.3.2 → 0.4.0（单会话长跑 O(n²) 性能缺陷修复）** — 缺陷（Stable 事实内嵌全量快照 + 审计全量 clone，长驻会话每命令耗时线性恶化）发现于本版发布前的实战负载检验，未影响任何已发布版本。修复后 10000 命令会话 51s 全程平坦（修复前同规模推算数十小时）。随带 **WAL 事实格式变更**：新代码可读取 ≤0.3.x 旧格式 WAL（final_snapshot 容错 + version_before 兜底），**旧代码不可读取新格式（升级单向；如需回滚二进制须丢弃新格式 WAL）**；审计链哈希输入随 Stable 事实序列化形态变化
- **SSE Stable 事件形态** — `final_snapshot`（全量快照）字段移除，改为 `version`（稳定版本号）；状态本体经会话 snapshot API / 最近一条 StateTransition 获取，信息零丢失

### 🆕 新增

- **平台用户体系与统一认证（UV-017）** — bootstrap 首启/登录/登出/个人信息/改密 + 用户管理/角色管理/权限点注册表 API + 业务 API 统一认证中间件（双凭据 + 401 语义统一）
- **审计档案只读 API `GET /api/audit-archive`（UV-016）** — 从 WAL 重建历史会话审计链；**平台认证事件报表 `GET /api/audit/platform-events`（UV-018）** — 只读派生 platform.event.* 事实链
- **运维开关** — `--demo-auth` 演示登录入口开关（UV-020，auth/status 公开下发，体验包默认开/生产可关）、`--web-dir` 静态前端托管（SPA 回退 index.html）、未配置 `--wal-dir` 启动数据风险警示（UV-019）
- **插件清单三级配置 `--plugins`（UV-030）** — 命令行/env/file 三级 + fail-fast 校验 + 启动日志挂载快照 + `/api/health` plugins 节运行时挂载事实
- **第二个进程内原生插件 `plugins/physics-services`(UV-035 泛化验证)** — vendored rpsm-core v0.1.0 确定性物理内核(辛积分器,编译期锁定常量,同平台同输入逐位一致)+ 3 个无状态原生服务(`physics_simulate`/`physics_energy`/`physics_grav_band`,浮点字符串化,NaN/Inf 显式拒绝,数量/步数预算上限)+ 插件本地声明 SSOT `official_native_services.json`
- **rpsm 内核测试套移植（UV-036）** — 14 个集成测试 65 用例（守恒/旋转/碰撞/常量锁定/BLAKE3 双跑哈希确定性），测试逻辑相对原内核逐行保真，移植边界逐条注记
- **第三个进程内原生插件 `plugins/indicator-services`(UV-037 泛化验证)** — 确定性金融技术指标 4 个无状态原生服务(`indicator_sma`/`indicator_ema`/`indicator_macd`/`indicator_rsi`):Python 参考实现(pandas)语义逐位对齐的 Rust 重写——SMA 逐行移植 pandas `roll_mean` Kahan 补偿滚动和与产物修正,EMA/MACD 按 `ewm(span, adjust=False)` 递推对齐,RSI 按 Wilder `alpha=1/N` 对齐(含 diff 首位 NaN 占位种子与 min_periods 屏蔽期);黄金值由 pandas 3.0.5 实算生成逐位断言;浮点字符串化,NaN/Inf 显式拒绝,warmup null 契约 + 插件本地声明 SSOT
- **插件挂载机制泛化(UV-035)** — `main.rs` 单插件专属装配退役,引入 `PluginDef`/`PLUGIN_DEFS` 进程内插件登记表:新增插件 = 登记表追加一项(id + 服务名清单 + 路由构造子),清单解析(All/Subset/Off)/挂载链(声明序逐插件承接回落链尾)/`/api/health` plugins 节多键呈现,机制代码零改动
- **E2E 双/三插件验收(UV-035/037)** — `tests/plugins_e2e.rs` 生产同构插件链用例(原生命中/穿透回落诚实报错/声明序锁定/链首命中互不干扰);`scripts/run-plugins-e2e.ps1` 真实二进制场景(全启/子集/停用/混合清单互不干扰/非法清单 fail-fast)
- **原生服务声明文件化（UV-029）** — `official_native_services.json` 为 SSOT（三字段+序）+ `sync-native-services.ps1` 同步脚本（复制→字节核验→双侧守卫）
- **发布链闭环** — 发布审批通过即校验落盘 rules_dir + `bundles/import` 防篡改 400 闸门 + 发布队列 HTTP 层端到端测试
- **三层绑定端到端（B2）** — `binding_e2e` 集成测试（治理声明→条目绑定→service_registry→io_request 真实命中）+ 绑定缺失错误自诊断指引；**B5** 数据集级 push 事件 schema 声明执行侧适配
- **执行侧数据资产通道（Q12）** — `/api/knowledge` 三端点 + 落盘 manifest 携带 domain/tags
- **service_registry 加载期 schema 门禁（C9）** — rule_schema SSOT 接入
- **审计增强** — stable 域写入凭据分层（service token 身份准入）、审计报告按需注入完整 Fact 内容（include_content）、审计指标与审计条目 API
- **SharedFactsLog 恢复失败拒绝启动（fail-fast，AUDIT-A1b）**
- **宪法 `resources/core_eval.json` v0.3.1 → v0.4.2** — T8 同步核心仓最小评估集（ReAct 应用剧本迁出至消费方自持）+ 补回 call_external/call_service 会话桥接指令规则（v0.4.1，HTTP 会话为平台消费面无法自持剧本）+ call_service 触发域 service_name 门禁（v0.4.2，兼容 bundle 落地规则硬编码路由）+ server 启动期校验宪法含 call_external 规则否则拒绝启动并给自诊断指引
- **运维件（UV-031）** — 备份/恢复演练脚本（四场景 19 断言：备份→清空→恢复→审计档案回放 / WAL 损坏三级处置）+ VERSION_STRATEGY 精简落地版（WAL/SQLite/宪法三层兼容契约）
- **实战检验件（UV-032）** — 负载演练脚本 `load-drill.ps1`（会话生命周期闭环 + 错误分类统计 + 用户节奏 + 端口监听者 pid 防呆）+ bench 三件（determinism/throughput/long_session）性能现实适配
- **AGPL + 商业双许可体系** — 新增 `DUAL_LICENSE.md`(双轨许可说明 + Server 特有白标授权边界)、`COMMERCIAL_LICENSE.md`(商业许可协议模板)、`FREE_COMMERCIAL_LICENSE.md`(政府/学术界/非营利免费豁免)、`CLA-individual.md`(个人贡献者许可,赋能双许可可执行);对齐 evorule 核心仓双许可体系
- **`CONTRIBUTING.md` 补充双许可声明与 CLA 必要性** — `协议` 扩为 `协议与 CLA`

### 🔄 变更

- **核心依赖走 crates.io 0.4.0** — evorule-tcb / reactor / governance 0.4.0、evorule-bundle 0.3.0、evorule-hash 0.1.3；发布时移除本地 `[patch.crates-io]` path 覆盖
- **插件 NativeService 抽象上提 `core/plugin-kit`(等价重构)** — 三插件 lib.rs 中逐行同构的机制段(`NativeService` trait/`NativeServiceDef` 声明项/过滤路由器:new + with_enabled 三拒绝 + 声明序查找 + HTTP 回落,≈90 行×3)归一为公共 crate 单份维护;三插件改薄壳具名委托(对外 API 逐名不变,既有测试零改动语义通过),`main.rs` `PluginDef` 直引声明表指针 + `mount_router` 单点挂载(6 个逐插件包装构造子退役),新增插件登记成本 = 声明表指针一项;三拒绝语义与错误文案逐字节不变,真实二进制五场景健康节/子集/fail-fast 输出逐项一致
- **T8 测试夹具属地化** — `integration_test.rs` / `fault_recovery_test.rs` / `session_integration_test.rs` 不再跨仓读取 `evorule/evorule-tcb/core_eval.json`,统一改读本仓 `resources/core_eval.json`;机制层验证所需的 call_service 等指令规则以内联应用剧本形态附加(属地原则:运行宪法由消费方自持)
- **自写 blake3 全部收口 evorule-hash crate** — 因果链 API 暴露锚口径统一
- **CORS 默认行为** — 未配置 `--allowed-origins` 时默认放行本机 loopback Origin（localhost/127.0.0.1/[::1] 任意端口），生产部署请显式配置白名单
- **build.rs 门禁状态机生命周期撇号判别修复（CR-20260830-001）** — char_lit_starts/skip_lifetime 判别分流消除 tests 模块剥离失效导致的门禁全量误报（与核心四仓同步）
- **`core/workspace` 补 `publish = false`** — 与其余 11 个 workspace 成员一致(依赖 path crate,保持闭包,不进 crates.io)
- **README / NOTICE 许可证段落改双轨声明** — 指向新增双许可文件,明确代码(AGPL/双许可)、文档(CC-BY-4.0)、宪法(CC0-1.0)分层

### 🐛 修复

- **限流令牌桶语义修正** — per_sec 参数此前按 burst/per_sec 公式误报（日志显示"1 req/s"误导排障），实际持续速率 = per_sec req/s（实测 135+ req/s 持续零 429，行为本就正确）；启动日志改直接打印 per_sec
- **guard_shell_risky v0.2.0 补 has_fields 守卫条件** — 消除无条件 io_request 对业务的阻断
- **IoSubscriber 挂载 LLM 审计形态跳过谓词** — 保障审计桥外部应答权（LLM 审计桥 call_external 回路不被订阅者吞答）

### 🔒 安全

- **升级依赖修复 RUSTSEC 漏洞** — prometheus 0.13→0.14(移除 protobuf 2.28.0, RUSTSEC-2024-0437)、sqlx 0.8→0.8.1(RUSTSEC-2024-0363)、rusqlite 0.31→0.32(解除与 sqlx 的 libsqlite3-sys 冲突)、h2 0.4.15→0.4.19(RUSTSEC-2026-0258)
- **暂存待评估** — `rsa`(RUSTSEC-2023-0071, 无可修复版本)与 `paste`(未维护告警),当前无升级路径

### 🧪 测试

- **总验收 E2E** — 治理发布→LLM 草稿→gate two→执行侧直跑→审计回放全链（含凭据扫描兜底负向）
- **workspace 全量回归** — 60 测试二进制全绿；clippy 1.97 门禁清零

---

## [0.3.0] - 2026-08-26

### ⚠️ Breaking Changes

- **`audit_report()` 返回值变更** — `evorule-server/src/api/server.rs` `GovernanceApi::audit_report()` 从 `String` 改为 `Result<String, serde_json::Error>`，不再静默退化为 `"{}"`（同步 evorule v0.3.2）
- **`GET /api/audit` handler 返回值变更** — `get_audit()` 从 `Json<serde_json::Value>` 改为 `Result<Json<Value>, StatusCode>`，序列化失败时返回 500

### 🆕 新增

- **`core/rule_schema` crate** — 规则 Schema 门禁（线1 防御层），`/api/rules/validate` 提交校验的权威基准。含 3 个 JSON Schema 文件（rule_set / _meta / _shared v1.0）和 Rust 验证库（19KB）
- **`/api/bundles` 规则包 API** — `evorule-server/src/api/bundles.rs`（31KB）：规则包导入、列出活跃包、导入历史、原子落盘（`land_bundle_atomically`）、回滚（`rollback_bundle_moves`）、陈旧目录清理
- **`/api/permissions` 权限 API** — `evorule-server/src/api/permissions.rs`（10KB）：权限管理端点
- **`plugins/demo-services` 插件示例** — Rust 原生业务服务实现（复合路由：原生优先，HTTP 回落），Phase 1 yuanze-demos，7 个原生服务（ik_solver / llm_advisor / robot_move / rule_sandbox / sampling / shadow_validate / config_persist）
- **`rules/bundles/` 规则包示例** — `bundle-ds-yuanze-01-v3`（15 条规则：审计告警/压缩/计算/演进扫描/生成补丁/热加载/机器人移动/安全回滚/采样决策/沙盒验证/影子验证/精度验证）+ `b_guard_shell_risky` 安全规则包
- **服务注册 API** — `list_services_handler` + `BoundServiceInfo` 结构体，列出已绑定服务及其元数据（名称/来源/版本/描述）
- **`service_registry.json`** — 服务注册配置文件
- **`scripts/check_schema_sync.py`** — Schema 同步检查脚本，确保规则 Schema 与代码一致
- **`evorule-bundle` 依赖** — 快照包共享校验（T2：36 号集成契约；6 项校验链 + 逐条 Schema 门禁 + 原子落盘），version 0.2.0

### 🔄 变更

- **核心库依赖 crates.io** — evorule-tcb / reactor / governance 统一使用 crates.io v0.3.2（含 permission 模块 / io_context 数据型），evorule-bundle v0.2.0；发布时移除本地 `[patch.crates-io]` path 覆盖
- **workspace members 新增** — 根 `Cargo.toml` 新增 `core/rule_schema` 和 `plugins/demo-services`
- **`evorule-server/Cargo.toml` 新增依赖** — `evorule-rule-schema`（path）、`evorule-bundle`（0.2.0）、`evorule-demo-services`（path）
- **`resources/core_eval.json` 同步更新** — 同步 evorule v0.3.2 宪法变更

### 🐛 修复

- **元指令白名单修正同步** — `increment` / `noop` transform 类型被拒绝（之前误混入白名单导致假阳性），测试用例重命名为 `test_validate_increment_transform_type_rejected` / `test_validate_noop_transform_rejected`
- **set 非法 operation 提升为 error** — 从 warn 不阻断改为 rejected 阻断，测试用例重命名为 `test_validate_invalid_operation_rejected`
- **workspace 模块多项修复** — `publish_service.rs`（发布队列状态机修复）、`rule_translate.rs`（规则翻译边界修复）、`sandbox_service.rs`（沙盒编排修复）、`rolling_session.rs`（滚动 session 修复）、`session_bridge.rs`（会话桥接修复）、`workspace_service.rs` / `rule_meta_service.rs` / `models.rs` / `db.rs` / `lib.rs`
- **io_handlers 多项修复** — `db_handler.rs`（SQL 注入防护增强）、`http_handler.rs`（SSRF 防护增强）、`memory_handler.rs`（key 长度校验）、`service_registry.rs`（URL scheme 校验）、`lib.rs`
- **hot_reload 修复** — `loader.rs`（规则加载器修复）、`lib.rs`（删除事件语义说明）
- **rule_tools 修复** — `validator.rs`（校验器修复）、`lib.rs`、`bin/evorule-rule-tools.rs`
- **集成测试更新** — `alignment_test.rs` / `integration_test.rs` 同步 API 变更

### 📚 文档

- **10 个缺失文档的 crate 新增 README** — `core/workspace` / `evorule-server` / `core/io_handlers` / `core/auth` / `core/debug_control` / `core/hot_reload` / `core/metrics` / `core/rule_tools` / `core/semantic_invariants` / `core/time_machine`（此前 12 个 crate 中 10 个完全无文档）
- **`core/rule_schema/README.md`** — 规则 Schema 门禁完整说明
- **`plugins/demo-services/README.md`** — 7 个原生服务说明 + 复合路由设计
- **`docs/INTEGRATION_GUIDE.md`** — meta 指令 4→6 种、新增 rule_schema 校验说明、新增 §6 规则包 API、§7 权限 API
- **`GATE_REFERENCE.md`** — 新增 §3.4 rule_schema Schema 完整性门禁、§3.5 plugins 门控、更新 crate 列表
- **`docs/PITFALLS.md`** — 新增坑 20-23（audit_report 返回值变更、元指令白名单修正、patch.crates-io 发布注意、规则包原子回滚）
- **`docs/RELEASE_PROCESS.md`** — 检查项 7→8 项（新增 check_schema_sync.py）、新增 [patch.crates-io] 段检测、子 crate 数量 9→11
- **`README.md` / `DOCS_INDEX.md` / `CHANGELOG.md`** — 同步更新

---

## [0.2.0] - 2026-08-19

> **本版本实际打 tag 日期: 2026-08-19**
>
> 2026-08-10 起 CHANGELOG 段已预写但未实际打 tag, 期间累积了实际 release
> 内容 (workspace 模块 + OpenAPI 单一真相源 + InputSanitizer + 核心库 0.3.1
> 升级 + gitee URL 迁移), 2026-08-19 一次打 tag.

### 🔒 安全

- **B1: HttpHandler 禁用 HTTP 重定向跟随（SSRF 绕过防护）**
  - `core/io_handlers/src/http_handler.rs` `build_client()` 加 `.redirect(reqwest::redirect::Policy::none())`
  - 旧实现 reqwest 默认跟随最多 10 次重定向，SSRF 防护只校验原始 URL 的 DNS 解析结果，
    重定向后的目标 IP 不再校验。攻击者可配置公网 URL → 302 → 169.254.169.254（云元数据）绕过 SSRF 防护
  - 禁用后 3xx 响应作为 Err 返回上层，由调用方决定处理方式（行业最佳实践）
- **B2: `POST /api/rules/reload` 移入认证保护**
  - `evorule-server/src/api/server.rs` 将 reload 路由从 `public_routes` 移到 `protected_routes`
  - 旧实现该端点无认证，攻击者可反复触发规则重载造成 DoS，或当 rules_dir 可写时注入恶意规则
  - 现在需要 `Authorization: Bearer <token>` 头，无认证返回 401
- **B3: 无认证 + 非 loopback 地址时 fail-closed 拒绝启动**
  - `evorule-server/src/main.rs` 无 token 且绑定非 loopback 地址时 `error!` + `exit(1)`
  - 旧实现仅 `warn!` 不阻止启动，公网部署时若用户漏看日志，所有 session 数据完全暴露
  - loopback 地址（127.0.0.1 / [::1]）仍允许无认证启动供本地开发；地址解析失败视为非 loopback（安全侧失败）

### 🆕 新增

- **`core/workspace` crate 首次纳入版本控制** — 多租户工作空间 + 规则元数据管理（P10 基础设施层），含 14 个源文件：
  - `rule_translate.rs`（36KB）：BusinessRule ↔ evorule 核心 6 域类型双向翻译引擎
  - `api.rs`（36KB）：Workspace HTTP API 端点（规则 CRUD / 版本管理 / 审计链 / 沙盒）
  - `db.rs`（96KB）：SQLite 持久化层（工作区 / 规则元数据 / 沙盒报告）
  - `workspace_service.rs` / `rule_meta_service.rs` / `publish_service.rs` / `sandbox_service.rs` / `verdict_service.rs` / `rolling_session.rs` / `session_bridge.rs` / `session_switched.rs` / `mock_io_responder.rs` / `test_report.rs` / `models.rs` / `error.rs` / `lib.rs`
  - 依赖：rusqlite (bundled) + serde + blake3 + chrono + ulid + axum 0.8 + tokio
- **Workspace API 端点** — `evorule-server/src/api/server.rs` 新增 +355 行：workspace 路由组（创建/列举/删除工作区、规则 CRUD、版本管理、审计链拉取、沙盒试运行）
- **Workspace CLI 参数** — `evorule-server/src/main.rs` 新增 +138 行：`--workspace-db` / `--workspace-root` / `--enable-workspace-api` 等启动参数
- **Workspace 集成测试** — `evorule-server/tests/session_integration_test.rs` 新增 +54 行：workspace API 端到端测试
- **OpenAPI 单一真相源 (P2-1)** — `evorule-server/src/api/openapi.rs` (新, 179 行):
  `utoipa::OpenApi` derive 聚合 server 全部 handler, 运行时经
  `GET /api/openapi.json` 导出 OpenAPI 3.1 规范, 前端通过
  `openapi-typescript` 自动生成类型 (杜绝手写 schema 与代码漂移).
  Swagger UI 端点 `GET /api/docs` 通过 `--openapi-ui` 显式开启
  (默认关闭避免生产暴露接口面)
- **强制中止会话端点** — `evorule-server/src/api/server.rs`:
  `POST /api/sessions/{id}/abort` 端点 (014 合法 API #4) 通过 `--allow-abort`
  CLI 参数显式开启, 默认 404 (双保险: 即使认证通过也需显式开启)
- **InputSanitizer 第一层输入净化 (Phase 1)** — `evorule-server/src/input_sanitizer.rs`
  (来自 feature/agent-prompt-impl 分支合并, 749 行 + clippy 修复): Prompt 注入
  防御公共服务, 静默改写 "ignore previous instructions" / "you are now" /
  "system: ..." / "act as admin" 等常见攻击模式, 18 类正则规则, 19 个单测覆盖
- **--openapi-ui / --allow-abort CLI 参数** — `evorule-server/src/main.rs`:
  两个破坏性/暴露性端点的双保险开关, 默认关闭

### 🔄 变更

- **核心库 0.2.1 → 0.3.1** — `evorule-server/Cargo.toml` / `core/io_handlers/Cargo.toml`:
  evorule-tcb / evorule-reactor / evorule-governance 三项核心依赖从 0.2.1 升到 0.3.1
  (核心仓 v0.3.x 已 cargo publish 到 crates.io)
- **新增 utoipa + utoipa-swagger-ui 依赖** — `evorule-server/Cargo.toml`:
  引入 OpenAPI 单一真相源 (`utoipa = "5"` + `utoipa-swagger-ui = "9"`)
- **核心 workspace 加 utoipa 依赖** — `core/workspace/Cargo.toml`: 标注模型
  用于 OpenAPI 导出 (`utoipa = { version = "5", features = ["axum_extras", "chrono"] }`)
- **移除 `[patch.crates-io]` 段** — 根 `Cargo.toml` 之前为本地开发覆盖 evorule-*
  路径的 `[patch.crates-io]` 段移除, release 用户不再误用本地路径
- **内部 crate 版本号统一 workspace 继承** — 9 个内部 crate（auth / debug_control / hot_reload / io_handlers / metrics / rule_tools / semantic_invariants / time_machine / evorule-server）的 `version = "0.1.0"` 改为 `version.workspace = true`，统一继承 workspace.package.version = 0.2.0，以后 bump 一处即可
- **workspace Cargo.toml 注册新成员** — 根 `Cargo.toml` `[workspace].members` 新增 `core/workspace`
- **gitee 仓 owner 迁移** — `evo-rule-lab` → `evorule`:
  - 本仓 (`evorule-server`): `evo-rule-lab/evorule-server` → `evorule/evorule-server` (11 处)
  - 核心仓 (`evorule`): `evo-rule-lab/evorule` → `evorule/evorule` (5 个 L1 文档 10 处)
  - 组织页: `gitee.com/evo-rule-lab` → `gitee.com/evorule` (README + NOTICE 2 处)
  - GitHub 镜像 workflow (`.github/workflows/mirror.yml`) 镜像源 URL 同步

### 🐛 修复

- **N1: AuthConfig 过滤空字符串 token** — `evorule-server/src/auth.rs` `new()` 过滤空 token，防止空 Bearer token 通过认证（`ct_eq("", "")` 返回 true）
- **N2: ServiceRegistry 校验 URL scheme** — `core/io_handlers/src/service_registry.rs` `parse_service_entry()` 解析时校验 scheme 为 http/https，拒绝 file:///data:// 等
- **N3: http_requests_total 指标接入中间件** — `evorule-server/src/api/server.rs` 添加 `http_metrics_middleware`，用 `normalize_path_for_metrics` 把数字段归一化为 `{id}` 防止 Prometheus 基数爆炸
- **N4: hot_reload 支持 auth_token 配置** — `core/hot_reload/src/config.rs` 增加 `auth_token` 字段，`create_session`/`send_rules` 注入 `Authorization: Bearer` 头；bin 加 `--auth-token` CLI 参数
- **N5: HttpHandler::new_dev_allow_loopback 保留不改** — 评估后跳过：evorule-io-handlers 是 `publish = false` 内部 crate，main.rs 的 `--allow-loopback` 已有"生产环境永远不要启用"文档警告
- **N6: MemoryHandler 限制 key 长度** — `core/io_handlers/src/memory_handler.rs` `execute()` 检查 key ≤ 255 字节，防止超长 key 触发 OS 文件名错误
- **gte/gt 域类型翻译** — `core/workspace/src/rule_translate.rs`：evorule 核心仅支持 eq/lt/exists/instruction/all/not 6 域类型，server 端将 gte 翻译为 `not(lt)`、gt 翻译为 `not(all([lt,eq]))`，并实现对称回译（not(lt)→gte、not(all([lt,eq]))→gt），确保 onboarding 创建的 gte/gt 规则通过 G4 校验
- **action_set 角色丢失** — `core/workspace/src/rule_translate.rs` `translate_to_transform`：未正确处理 `action_set` 中的 value 字段，导致动作角色（role）信息丢失。修复后 value 字段正确包含 role 信息
- **G5 校验白名单遗漏** — `core/workspace/src/rule_translate.rs` + `evorule-console/src/lib/validators/ruleValidator.ts`：`__exec__.result.notify` 不在 G5 白名单，新增 `__exec__.result.*` 路径前缀，支持执行结果引用
- **params.path vs params.attr 不一致** — `core/workspace/src/rule_translate.rs`：`translate_to_transform` 生成 `params.path`，而 evorule core `exec_set` 读取 `params.attr`，导致 set 动作静默失败。统一为 `params.attr`

### ✅ 向后兼容

- **S1: hot_reload 删除事件语义说明** — `core/hot_reload/src/lib.rs` 检测到 `ChangeType::Remove` 时输出 `warn!` 日志，明确告知"hot_reload 仅支持增量添加规则，删除文件不会从 server 移除已有规则，如需清除旧规则请重启 session"。旧实现删除文件时静默无提示，用户误以为规则已被移除
- **S2: /metrics 端点可选认证** — `evorule-server/src/main.rs` 新增 `--metrics-auth` / `EVORULE_METRICS_AUTH` CLI 参数；`api/server.rs` `GovernanceServer` 新增 `metrics_requires_auth` 字段，独立构建 `metrics_router`，启用时挂载 `auth_middleware`。默认关闭（Prometheus scraper 通常不带 token），启用后 `/metrics` 也需 `Authorization: Bearer <token>` 头
- **S3: CORS 通配符 origin 检测** — `evorule-server/src/api/server.rs` `build_router()` 检测 `allowed_origins` 包含 `"*"` 时输出 `warn!`，提示"CORS 规范禁止通配符 + credentials 组合，浏览器会拒绝此响应，请使用精确 Origin 列表替代"
- **S4: time_machine 版本间隙测试覆盖** — `core/time_machine/src/lib.rs` 新增 9 个测试覆盖版本间隙（version gap）场景：首条记录前间隙、Command 被忽略产生间隙、ST 与 IoResponse 间间隙、多间隙全返回 None、间隙边界返回 Some、local_diff 间隙版本退化为空对象、build_version_tree 稀疏版本 total_versions 正确性、build_batch_diff 跨间隙配对

### 🧪 测试

- **CI 验证全绿** — 实际打 tag 时: `cargo check --workspace --all-targets` 0 error,
  `cargo clippy --workspace --all-targets -- -D warnings` 0 warning, `cargo fmt --all
  -- --check` 通过, `cargo test --workspace --all-features` 32 个测试组全 ok

---

## [0.1.0] - 2026-07-30

**evorule-server 仓首次建立** — 本仓独立 release。
本仓从应用层迁出 9 个 server 配套 crate,作为 EvoRule 框架的官方 server 实现。

### 🆕 新增

- **新仓建立** — evorule-server 仓 git init,主分支 `main`
- **Cargo workspace 顶层配置**
  - members: `core/{auth, debug_control, hot_reload, io_handlers, metrics, rule_tools, semantic_invariants, time_machine}` + `evorule-server`
  - workspace.package: `version = "0.1.0"` / `edition = "2021"` / `license = "AGPL-3.0-or-later"` / `authors = ["EvoRule Project"]` / `repository = "https://gitee.com/evorule/evorule-server"` / `rust-version = "1.74"`
  - workspace.lints: `unwrap_used/expect_used/panic/panic_in_result_fn` deny + `cognitive_complexity/too_many_lines/type_complexity/module_inception` warn
  - workspace.lints.rust: `unexpected_cfgs` warn (兼容 kani cfg)
- **物理迁入 9 个 crate** (从应用层迁入)
  - `core/auth` (799 KB, 0 publish, 仅本仓内)
  - `core/debug_control` (566 KB, 0 publish)
  - `core/hot_reload` (1.6 MB, 0 publish)
  - `core/io_handlers` (1.2 MB, 0 publish, evorule-server 唯一依赖)
  - `core/metrics` (1.3 MB, 0 publish, Prometheus 实现)
  - `core/rule_tools` (366 KB, 0 publish, 规则脚手架)
  - `core/semantic_invariants` (566 KB, 0 publish)
  - `core/time_machine` (1.3 MB, 0 publish, rewind/diff/fork)
  - `evorule-server` (6.2 MB 源码, 主 bin, 0 publish, 约 50 条路由)
- **新仓门面文件**
  - `README.md` (9.6 KB) — 介绍 evorule-server 仓的定位、架构、快速开始、API 概览、配置、部署、路线图
  - `CHANGELOG.md` (本文档)
  - `SECURITY.md` (简版,引用标准安全流程)
  - `CONTRIBUTING.md` (简版)
  - `LICENSE` (AGPL-3.0-or-later)
  - `.gitignore` (680 B,覆盖 Rust/IDE/数据文件)

### 🔄 变更

- **Path 依赖调整**
  - `evorule-server/Cargo.toml` `evorule-io-handlers` path: `../io_handlers` → `core/io_handlers`
  - 核心引擎依赖使用 `version = "0.1.0"` 声明 (crates.io 兼容)
- **evorule-server 仓元数据**
  - `description`: "EvoRule 框架官方 HTTP server 实现"
  - `repository`: `https://gitee.com/evorule/evorule-server`
  - 9 个 core/* lib 的 metadata 同步调整

### 🗑 弃用

- **cluster/ 不迁入** — 原 `core/cluster/` 已弃用,新仓不引入

### 📚 文档

- 本仓独立 release，不绑核心仓发布节奏

### ✅ 向后兼容

- evorule-server/Cargo.toml 的 `repository` / `description` 已改为本仓 URL
- 9 个 core/* lib 的 Cargo.toml metadata 已补齐（description + publish=false）
- CI 配置(`.gitee-ci/` / `.github/`)已迁移
- Dockerfile / scripts/build-docker.ps1 已迁移
- cargo check / cargo test / cargo clippy 全跑通（447 passed, 0 failed, clippy -D warnings 0 errors）

---

## 历史

本仓代码最初位于应用层(2026-07-30 之前)。
2026-07-30 之后,所有 commit 都在本仓。

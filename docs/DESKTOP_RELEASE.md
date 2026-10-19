# evo-agent-desktop 发版流程（内部邀请 Beta）

> **文档性质**：evo-agent-desktop 安装包的发布操作手册（固化版，2026-10-19）。
> **发布形态**：安装包（exe）发布到 Gitee/GitHub Releases，内部邀请试用反馈；
> **尚非公开推广**——公开推广另有物料与渠道决策（见推广物料档）。
> **发布前双闸预检是强制步骤**，任何一闸不过 = 停止发布。

---

## 0. 密钥纪律（先读）

1. 构建机与发布产物**不得包含任何模型密钥**。用户密钥由用户在自己的
   桌面应用设置中录入（存储于用户本机）；构建者自己的密钥只存在于
   个人文件（如 `桌面\wenjian\`），永远不在打包收集路径上；
2. `plugins/ai-plugin/config.json` 与仓根 `ai-plugin.json` 已入
   `.gitignore`（0807a9c）——执行体配置可含端点，密钥走
   `EVORULE_AI_PLUGIN_LLM_API_KEY` 环境变量（零落盘形态）；
3. 提交前扫一遍 `git diff --staged | grep -i 'sk-\|api_key'`（人工兜底）。

## 1. 组件矩阵（安装包必须携带）

| 组件 | 来源 | 说明 |
|---|---|---|
| `evorule-server.exe`（引擎） | server 仓 release 构建 | 18 家族 115 服务 + band_check 立法 |
| `evo-agent.exe`（serve/CLI） | evo-agent 仓 release 构建 | 工具面含 112 service tools |
| `rules/`（含 bundles） | server 仓 `rules/` | **须过装载门禁**（O-525 修复随包） |
| `resources/server_eval.json` | server 仓 `resources/` | v0.7.0 含 band_check 立法 |
| `resources/service_registry.json` | server 仓根 | |
| `agents/*.json` + `agents/skills/` | evo-agent 仓 | general 含 evorule-deterministic-compute 技能注册 |
| `web/dist`（工作台前端） | evo-agent 仓 web 构建产物 | |
| `evo-agent.toml` 模板 | evo-agent 仓（本地文件） | **须含 115 项 service_tools + base_url 指向本机引擎端口**（O-526/527 修复随包） |
| `skills/evorule-deterministic-compute/` | server 仓 `skills/` | 可选独立分发 |

## 2. 构建步骤

```powershell
# 1) 引擎（server 仓）
cd D:\evorule-server; cargo build --release -p evorule-server

# 2) serve/CLI（evo-agent 仓）
cd D:\evo-agent; cargo build --release -p evo-agent

# 3) 工作台前端
cd D:\evo-agent\web; npm run build
```

## 3. 双闸预检（强制）

```powershell
pwsh D:\evorule-server\scripts\prepackage_checks.ps1 `
  -StageDir  <打包暂存目录> `
  -KeyFiles  @("C:\Users\<你>\Desktop\wenjian\minimax.txt") `
  -RulesDir  D:\evorule-server\rules `
  -ServerBase http://127.0.0.1:18080
```

- **闸 1 密钥扫描**：暂存目录全部文本文件扫实际密钥值 + 通用特征
  （sk- 串 / MINIMAX_API_KEY= / llm_api_key=）；命中即停止发布；
- **闸 2 装载门禁**：暂存 rules 全部 JSON 过运行体装载门禁
  （`/api/rules/validate`，与 CI 同门禁）——拦截 eq/lt 缺 on_missing
  类回归（O-525 场景）；
- 前置：本机需有一个运行中的 evorule-server（开发实例即可）。

## 4. 打包

- 按 0.3.0 既定打包方式产出安装包（NSIS/installer 流程不变）；
- 打包完成后**复跑闸 1**（对最终安装包做一次密钥扫描，二进制按
  字节扫描）——双保险。

## 5. 发布到 Gitee/GitHub（内部邀请 Beta）

1. Gitee Release 页新建（tag：`evo-agent-desktop-v0.3.1` 形态）；
2. 上传安装包 + SHA256 校验和文件；
3. GitHub Releases 镜像同步（仓库为镜像，Release 手动同步）；
4. Release 说明模板：
   - 本版本新增（能力摘要：确定性计算服务家族/期间届日/比例分配等）；
   - **首次启动**：在设置中录入你的模型 API 密钥（仅存本机）；
   - 内部邀请性质说明 + 反馈渠道（Gitee issue）；
5. **可见性**：Gitee 仓若为公开，Release 即公开可下载——内部邀请阶段
   如需限流，可用 Gitee 私有仓+成员可见 Release，或直接点对点分发给
   邀请名单。

## 6. 已知问题门（发布说明须如实携带）

| 登记号 | 问题 | 发布说明口径 |
|---|---|---|
| O-528 | 发版迁移丢用户 localStorage 密钥 | 已在设置重录即恢复；迁移逻辑修复在途 |
| O-526/527 | 旧版 service_tools 不含计算家族/base_url 死端口 | 本版已修复 |
| O-529 | 自动更新器端点未配置 | 本版暂无自动更新，请手动下载 |

## 7. 发布后

- 邀请名单内逐一点对点通知（内部邀请，不做公开推广）；
- 反馈进 T8+ 拉动区登记；
- 引擎/serve 日志在用户机 `%LOCALAPPDATA%\com.evorule.evo-agent-desktop\logs\`，
  反馈问题时请用户附上（日志无密钥明文，已扫描验证）。

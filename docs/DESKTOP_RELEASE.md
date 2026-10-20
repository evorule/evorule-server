# evo-agent-desktop 发版流程（内部邀请 Beta）

> **已让位（2026-10-20）**：发版操作唯一正本 = evo-agent 仓 `desktop/RELEASE.md`
> （生产级差距批D 入仓；本档仅存历史参考，双闸预检节仍有效——预检脚本
> `scripts/prepackage_checks.ps1` 在本仓，发布前照跑）。发布渠道口径以正本
> 为准（GitHub Releases 单通道+自更新）。
>
> **文档性质**：evo-agent-desktop 安装包的发布操作手册（固化版）。
> **发布形态**：安装包（exe）发布到 Gitee/GitHub Releases，内部邀请试用
> 反馈；尚非公开推广——公开推广另有物料与渠道决策。
> **发布前双闸预检是强制步骤**，任何一闸不过 = 停止发布。

---

## 0. 密钥纪律（先读）

1. 构建机与发布产物**不得包含任何模型密钥**。用户密钥由用户在自己的
   桌面应用设置中录入（存储于用户本机）；构建者自己的密钥只存在于
   个人文件，永远不在打包收集路径上；
2. 执行体配置文件已入 `.gitignore`——执行体配置可含端点，密钥走
   环境变量（零落盘形态）；
3. 提交前人工兜底：`git diff --staged` 目视扫一遍。

## 1. 组件矩阵（安装包必须携带）

| 组件 | 来源 | 说明 |
|---|---|---|
| 引擎可执行文件 | server 仓 release 构建 | 全部原生服务家族 + 会话模型准入立法 |
| serve/CLI 可执行文件 | evo-agent 仓 release 构建 | 工具面含全部 service tools |
| 规则目录（含 bundles） | server 仓规则目录 | **须过装载门禁**（存量回归修复随包） |
| 立法文件与服务注册表 | server 仓资源目录 | 含区间判定立法与服务端点注册 |
| 工作台前端构建产物 | evo-agent 仓 web 构建 | |
| agent 定义与技能目录 | evo-agent 仓 | 默认 agent 含确定性计算技能注册 |
| 执行体配置模板 | evo-agent 仓 | **须含全部 service tools + 执行体端口指向本机引擎** |

## 2. 构建步骤

```powershell
# 1) 引擎（server 仓根）
cargo build --release -p evorule-server

# 2) serve/CLI（evo-agent 仓根）
cargo build --release -p evo-agent

# 3) 工作台前端
cd <evo-agent 仓根>\web; npm run build
```

## 3. 双闸预检（强制）

```powershell
pwsh <server 仓根>\scripts\prepackage_checks.ps1 `
  -StageDir  <打包暂存目录> `
  -KeyFiles  @(<构建者密钥存根文件路径>) `
  -RulesDir  <server 仓根>\rules `
  -ServerBase http://127.0.0.1:18080
```

- **闸 1 密钥扫描**：暂存目录全部文本文件扫实际密钥值 + 通用特征
  （sk- 串 / 模型密钥赋值 / 执行体密钥赋值）；命中即停止发布；
- **闸 2 装载门禁**：暂存规则全部 JSON 过运行体装载门禁
  （与 CI 同门禁）——拦截判定域缺显式缺失声明的存量回归；
- 前置：本机需有一个运行中的 evorule-server（开发实例即可）。

## 4. 打包

- 按既定安装器流程产出安装包；
- 打包完成后**复跑闸 1**（对最终安装包做一次字节级密钥扫描）——双保险。

## 5. 发布到 Gitee/GitHub（内部邀请 Beta）

1. Gitee Release 页新建（tag：`evo-agent-desktop-v<语义化版本>` 形态）；
2. 上传安装包 + SHA256 校验和文件 + **latest.json（更新器喂料，随构建三件套产出）**；
3. GitHub Releases 镜像同步（**latest.json 必传**——更新器端点 =
   GitHub Releases 稳定 latest 资产 URL，缺件即全体用户更新检查失败；
   latest.json 内 url 已按本版 tag permalink 钉版，勿改）；
4. Release 说明模板：
   - 本版本新增（能力摘要：确定性计算服务家族、期间届日、比例分配等）；
   - **首次启动**：把你的模型 API 密钥单行写入
     `%APPDATA%\evo-agent-desktop\llm-key.txt`（仅存本机，不进浏览器界面）；
   - 内部邀请性质说明 + 反馈渠道（Gitee issue）；
5. **可见性**：Gitee 仓为公开时 Release 即公开可下载——内部邀请阶段
   如需限流，用点对点分发给邀请名单，或调整仓库可见性。

## 6. 已知问题门（发布说明须如实携带）

| 来源 | 问题 | 发布说明口径 |
|---|---|---|
| 已知问题 | 旧版 service_tools 不含计算家族/执行体端口失指 | 本版已修复（发版模板入库+SSOT 守卫） |
| 已知问题 | 自动更新器端点未配置 | 本版已配置（GitHub Releases latest）；不可达时手动下载 Gitee Release |

## 7. 发布后

- 邀请名单内逐一点对点通知（内部邀请，不做公开推广）；
- 反馈统一走 Gitee issue 登记跟踪；
- 引擎/serve 日志在用户机应用数据目录 `logs\` 下，反馈问题时请用户
  附上（日志无密钥明文，已扫描验证）。

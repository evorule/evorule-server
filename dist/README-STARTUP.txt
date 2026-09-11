evorule 体验版(单机一键启动) v0.6.0
====================================

环境要求
--------
- Windows 10/11 64 位
- 无需安装任何运行时(Node/Rust/数据库都不需要)

启动
----
1. 解压本压缩包到任意目录(路径建议不含空格)
2. 双击 start-evorule.bat
3. 浏览器自动打开 http://localhost:18080 即可使用

退出
----
关闭任务栏上最小化的两个窗口("evorule-server" 和 "evorule-rule")即可。
两个服务互相独立,一个失败不影响另一个;全部关闭后再双击
start-evorule.bat 可重新启动。

启动失败排查
------------
启动脚本会在启动前自动检测端口是否被占用:若 18080 或 18081
已被其他程序占用,会弹出提示框并停止启动,此时请关闭占用端口的
程序后重试。若某个服务仍然启动失败,查看 data\ 目录下的
server-stderr.log / rule-serve-stderr.log,常见原因:
- 端口被占用(改 bat 中对应端口,浏览器地址同步修改)
- 杀毒软件拦截(见下方常见问题)

体验治理视图(规则资产库,可选)
------------------------------
主界面之外的「治理」页连接本地的规则资产治理服务
(evorule-rule,端口 18081,启动脚本已自动拉起)。首次使用:

1. 进入「治理」页,连接地址保持默认 http://127.0.0.1:18081
2. 登录体验账号:用户名 admin / 密码 evorule-demo
3. 即可浏览数据集、5 态生命周期、审批发布等治理功能
(仅限本机体验包默认凭据;正式部署必须更换密码)

治理服务说明:体验包为治理服务指定了固定演示密钥
(--secret evorule-demo-secret-2026);正式部署时请更换,
或不传 --secret 让服务首次启动自动随机生成并持久化
(数据目录下 jwt_secret.key,重启自动复用)。

体验 AI 助手(可选)
------------------
不配置 LLM 也能浏览全部界面与规则工作台;若要体验 AI 助手
(规则草稿生成/规则解释/对话问答),请准备一个 OpenAI 兼容的
API Key,在页面右上角「设置 → LLM 配置」中填写:

- API 端点(如 https://api.openai.com/v1/chat/completions)
- API Key
- 模型名(如 gpt-4o-mini)

Key 只保存在你本机浏览器中,不会上传到任何第三方。

体验服务调用(可选,离线可跑)
------------------------------
本包内置「工具调用」演示:规则通过 call_service 指令调用
server 内置的原生服务(进程内确定性执行,不需要联网、
不需要任何外部服务)。

1. 打开「执行台」(或规则试运行入口),提交 call_service 指令
2. 引擎自动完成:命令 → 调用内置服务 → 求解结果写回会话
3. 打开「审计」页可看到本次调用的完整审计链
   (请求与结果全文入链)

内置服务还包括 rule_sandbox(规则沙箱)等;服务声明见
service_registry.json,可自行扩展为真实 HTTP 服务端点。

插件看门狗(可选)
----------------
普通用户无需运行本功能,直接忽略即可;默认分发包不加载任何插件看门狗。
外部插件(如 finance-config)是独立进程,主服务不负责拉起;
若希望插件进程崩溃后自动恢复,可启用部署侧看门狗:

1. 编辑 plugins-watchdog.json,在 plugins 中登记要守护的插件,
   例如(启用 finance-config):
   "plugins": {
     "finance-config": {
       "command": "plugins\\finance-config\\evorule-finance-config-plugin.exe",
       "args": ["--port", "9110", "--data", "plugins\\finance-config\\data"],
       "env": { "FINANCE_PLUGIN_ADMIN_TOKEN": "换成你的管理token" }
     }
   }
2. 双击 start-watchdog.bat(最小化窗口运行)
3. 看门狗周期读取主服务 /api/health 的插件存活状态:
   - 插件离线连续超过阈值(缺省 3 个周期)才自动拉起(防抖)
   - 每小时自动拉起次数有上限(缺省 5 次),超限后停止拉起并在
     日志中输出 ESCALATION 升级告警,需要人工介入
   - 未实现 /health 探针的插件如实跳过,不误动作
4. 日志见 data\watchdog.log;关闭看门狗:关闭最小化的
   "evorule-watchdog" 窗口

不启用看门狗完全不影响主服务运行;Linux 部署可用 systemd
(Restart=always)或容器编排的自动重启策略达到同等效果。

数据与隐私
----------
- 一切都在本机运行:服务只监听 127.0.0.1(仅本机可访问)
- 规则/工作区/治理数据持久保存在 data\ 下的 SQLite 库,重启不丢
- 会话与审计链实时写入 data\wal\(fsync 落盘,断电不丢、事后可取证);
  注意:重启后历史会话不在会话列表显示(运行状态在内存),审计链文件
  已保留供取证
- 删除整个 data 目录即可完全重置
- AI 助手的每次调用都会写入可回放的审计链(会话存续期间在「审计」页查看)

目录说明
--------
- start-evorule.bat        一键启动脚本(Windows)
- start-evorule.sh         一键启动脚本(Linux 版包内)
- start-watchdog.bat       插件看门狗启动脚本(可选,Windows 版包内)
- watchdog-plugins.ps1     看门狗主体(读 /api/health,离线自动拉起插件)
- plugins-watchdog.json    看门狗配置(缺省不守护任何插件,按需登记)
- evorule-server.exe       主服务(evorule-server v0.6.0,运行时 :18080)
- evorule-rule-serve.exe   治理服务(evorule-rule v0.3.1,规则资产库 :18081)
- web\                     前端页面(evorule-console-cloud)
- rules\                   运行规则集(业务场景演示规则)
- resources\               引擎业务规则集(server_eval.json,含会话桥接规则)
- service_registry.json    服务声明(call_service 的 service_name→服务映射)
- data\                    首次启动后生成:wal\ 为会话/审计链 WAL,
                           rule.db 为治理服务库,其余为规则/工作区库文件
- sha256-checks.txt        二进制完整性校验(sha256)

常见问题
--------
Q: 端口 18080 或 18081 被占用怎么办?
A: 编辑 start-evorule.bat,把对应端口改成其他值(如 18081→18082),
   浏览器地址与治理页连接地址也相应修改。

Q: 浏览器没自动打开?
A: 手动访问 http://localhost:18080

Q: 杀毒软件拦截?
A: 本包未做数字签名,部分杀软可能提示未知发布者;选择"仍要运行"即可,
   或将解压目录加入白名单。

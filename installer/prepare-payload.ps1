# evorule-setup 安装器载荷填充脚本
# 将分发包内容汇集到 installer/payload/（build.rs 会整树内嵌进单文件安装器）。
# 来源与 release.yml win64 打包步骤保持一致：
#   - evorule-server.exe        <- 本仓 target/release
#   - evorule-rule-serve.exe    <- evorule-rule 仓 target/release（与 RULE_SERVE_VERSION 配套）
#   - web/                      <- evorule-console-cloud build/（adapter-static 产物）
#   - rules/                    <- console-cloud assets/evorule-rules/*.json + 本仓 rules/10_role13_demo.json
#   - resources/server_eval.json / service_registry.json / dist 启动脚本与说明
# 防御性排除 data/、logs/、*.log（build.rs 亦有同样排除）。
param(
    [string]$ServerRepo = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path,
    [string]$RuleRepo = "D:\evorule-rule",
    [string]$ConsoleRepo = "D:\evorule-console-cloud"
)
$ErrorActionPreference = "Stop"

$payload = Join-Path $PSScriptRoot "payload"
if (Test-Path $payload) { Remove-Item $payload -Recurse -Force }
New-Item $payload -ItemType Directory -Force | Out-Null

function RequireFile($path) {
    if (-not (Test-Path $path)) { throw "缺少产物: $path" }
    return $path
}

# 1. 二进制
Copy-Item (RequireFile (Join-Path $ServerRepo "target\release\evorule-server.exe")) $payload
Copy-Item (RequireFile (Join-Path $RuleRepo "target\release\evorule-rule-serve.exe")) $payload

# 2. web 静态产物
Copy-Item (RequireFile (Join-Path $ConsoleRepo "build")) (Join-Path $payload "web") -Recurse

# 3. 业务场景规则
New-Item (Join-Path $payload "rules") -ItemType Directory -Force | Out-Null
Copy-Item (Join-Path $ConsoleRepo "assets\evorule-rules\*.json") (Join-Path $payload "rules")
Copy-Item (RequireFile (Join-Path $ServerRepo "rules\10_role13_demo.json")) (Join-Path $payload "rules")

# 4. 引擎资源与服务声明
New-Item (Join-Path $payload "resources") -ItemType Directory -Force | Out-Null
Copy-Item (RequireFile (Join-Path $ServerRepo "resources\server_eval.json")) (Join-Path $payload "resources")
Copy-Item (RequireFile (Join-Path $ServerRepo "service_registry.json")) $payload

# 5. 启动脚本与说明（与 dist/ 一致）
$dist = Join-Path $ServerRepo "dist"
foreach ($f in @("start-evorule.bat","start-evorule.sh","start-watchdog.bat","watchdog-plugins.ps1","plugins-watchdog.json","README-STARTUP.txt")) {
    Copy-Item (RequireFile (Join-Path $dist $f)) $payload
}

# 6. 插件清单：写中性清单（不含任何外部插件条目）
#    注意 dist/plugin_manifest.json 是开发机专用（引用 ../plugins/finance-config），
#    官方 zip 也不打包它；包内必须自带合法清单，否则 server 对 --plugins
#    fail-fast 拒启（v0.5.2 win64 zip 即因此主服务起不来）。
#    空条目清单 = 内建插件全启 + 零外部插件（见 main.rs load_plugin_mounts）。
Set-Content -Path (Join-Path $payload "plugin_manifest.json") -Value '{ "plugins": {} }' -Encoding ascii

$count = (Get-ChildItem $payload -Recurse -File).Count
$size = [math]::Round((Get-ChildItem $payload -Recurse -File | Measure-Object Length -Sum).Sum / 1MB, 1)
Write-Host "payload 就绪: $count 个文件, $size MB"

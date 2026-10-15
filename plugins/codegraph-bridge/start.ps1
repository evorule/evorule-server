# codegraph-bridge 启动脚本
# 用法: powershell -ExecutionPolicy Bypass -File start.ps1
# 依赖: Node >=20, 已在本目录 npm install
$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot
$env:CODEGRAPH_BRIDGE_HOST = "127.0.0.1"
$env:CODEGRAPH_BRIDGE_PORT = "9150"
node server.mjs

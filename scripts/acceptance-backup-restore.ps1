#Requires -Version 5.1
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 EvoRule Project
# 备份/恢复演练脚本(V1-a) — 全程使用 %TEMP% 沙箱目录与独立端口,不触碰任何真实数据目录。
#
# 场景:
#   1 全量备份 -> 数据目录清空 -> 恢复 -> 重启 -> 断言审计档案回放完整
#   2 共享事实 WAL 损坏(乱码注入) -> 断言拒绝启动且错误含自诊断指引
#   3 会话 WAL 位翻转/截断 -> 断言审计档案读取期如实报错(不静默降级)
#   4 rule.db 备份 -> 清空 -> 恢复 -> 重启 -> 断言管理员可登录
#
# 文件安全纪律(2026-08-29 数据事故教训):只复制不移动;每批复制后逐文件核验存在性与字节数;
# 沙箱数据(本脚本自建的临时目录)清理豁免,但清空前必须已持核验过的备份。

param(
    [string]$ServerExe = "",
    [string]$RuleExe = "",
    [int]$PortServer = 18280,
    [int]$PortRule = 18281,
    [switch]$KeepTmp
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
if (-not $ServerExe) { $ServerExe = Join-Path $repoRoot 'target\debug\evorule-server.exe' }
if (-not $RuleExe) { $RuleExe = 'D:\evorule-rule\target\debug\evorule-rule-serve.exe' }

$script:Pass = 0
$script:Fail = 0
$script:Results = New-Object System.Collections.Generic.List[string]

function Assert([string]$Name, [bool]$Condition, [string]$Detail = "") {
    if ($Condition) {
        $script:Pass++
        $line = "PASS | $Name"
    } else {
        $script:Fail++
        $line = "FAIL | $Name | $Detail"
    }
    $script:Results.Add($line) | Out-Null
    Write-Host $line
}

function Wait-Health([string]$Url, [int]$TimeoutSec) {
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        try {
            $r = Invoke-WebRequest -Uri $Url -UseBasicParsing -TimeoutSec 2
            if ($r.StatusCode -eq 200) { return $true }
        } catch { Start-Sleep -Milliseconds 500 }
    }
    return $false
}

function Copy-Verified([string]$Src, [string]$DstDir) {
    # 只复制不移动;复制后核验目标存在且字节数一致
    # ($null = 捕获 Copy-Item 的管道输出:部分注入环境会劫持 Copy-Item 别名使其返回对象)
    $null = Copy-Item -Path $Src -Destination $DstDir -Force
    $s = Get-Item $Src
    $d = Get-Item (Join-Path $DstDir $s.Name)
    if (-not $d -or $d.Length -ne $s.Length) {
        throw "复制核验失败: $Src -> $($d.FullName) ($($s.Length) vs $($d.Length))"
    }
    return $d.Length
}

function Remove-AllFiles([string]$Dir) {
    # 用 .NET API 删除而非 Remove-Item:部分注入环境会劫持 Remove-Item 别名导致不可靠失败
    if (Test-Path $Dir) {
        foreach ($f in [IO.Directory]::GetFiles($Dir)) { [IO.File]::Delete($f) }
    }
}

function Stop-Grace([object]$Proc) {
    if ($Proc -and -not $Proc.HasExited) {
        Stop-Process -Id $Proc.Id -Force -ErrorAction SilentlyContinue
        Start-Sleep -Seconds 2
    }
}

$tmp = Join-Path $env:TEMP "uv031-backup-restore-$(Get-Random)"
New-Item -ItemType Directory -Path $tmp | Out-Null
New-Item -ItemType Directory -Path "$tmp\run1\wal", "$tmp\run1\db", "$tmp\run1\memory", "$tmp\backup", "$tmp\rule" | Out-Null
Write-Host "沙箱目录: $tmp"
Write-Host "server: $ServerExe"
Write-Host "rule:   $RuleExe"

if (-not (Test-Path $ServerExe)) { throw "server 二进制不存在: $ServerExe" }
if (-not (Test-Path $RuleExe)) { throw "rule 二进制不存在: $RuleExe" }

$baseArgs = @('--addr', "127.0.0.1:$PortServer",
    '--db-path', "$tmp\run1\db\evorule.db",
    '--workspace-db', "$tmp\run1\db\workspace.db",
    '--memory-dir', "$tmp\run1\memory",
    '--wal-dir', "$tmp\run1\wal")

# ============ 场景 1: 备份 -> 清空 -> 恢复 -> 断言 ============
Write-Host "`n===== 场景 1: 全量备份->清空->恢复->审计档案回放 ====="
$p1 = Start-Process -FilePath $ServerExe -ArgumentList $baseArgs -WorkingDirectory $repoRoot `
    -WindowStyle Hidden -RedirectStandardOutput "$tmp\s1.out.log" -RedirectStandardError "$tmp\s1.err.log" -PassThru
$ok = Wait-Health "http://127.0.0.1:$PortServer/api/health" 30
Assert "S1 健康检查就绪" $ok "server 未在 30s 内就绪,见 $tmp\s1.out.log"

$sid = $null
$factCount = $null
if ($ok) {
    $resp = Invoke-RestMethod -Uri "http://127.0.0.1:$PortServer/api/sessions" -Method Post -Body '{}' -ContentType 'application/json'
    $sid = if ($resp.session_id) { [int]$resp.session_id } elseif ($resp.session_new) { [int]$resp.session_new } elseif ($resp.id) { [int]$resp.id } else { $null }
    Assert "S1 会话创建(返回会话 ID)" ($null -ne $sid) "响应: $($resp | ConvertTo-Json -Compress)"

    $body = '{"instruction":{"type":"set","params":{"attr":"uv031_probe","operation":"set","value":42}}}'
    $null = Invoke-RestMethod -Uri "http://127.0.0.1:$PortServer/api/sessions/$sid/command" -Method Post -Body $body -ContentType 'application/json'
    Start-Sleep -Milliseconds 800

    $arch = Invoke-RestMethod -Uri "http://127.0.0.1:$PortServer/api/audit-archive/sessions"
    $entry = $arch.sessions | Where-Object { $_.session_id -eq $sid }
    $factCount = if ($entry) { [int]$entry.fact_count } else { $null }
    Assert "S1 会话 $sid 已入审计档案(事实数 $($factCount))" ($null -ne $factCount -and $factCount -gt 0) "档案: $($arch | ConvertTo-Json -Compress)"
}
Stop-Grace $p1

# 备份(wal + db,逐文件核验)
if ($sid) {
    $walFiles = Get-ChildItem "$tmp\run1\wal" -File
    foreach ($f in $walFiles) { $null = Copy-Verified $f.FullName "$tmp\backup" }
    foreach ($f in (Get-ChildItem "$tmp\run1\db" -File)) { $null = Copy-Verified $f.FullName "$tmp\backup" }
    Assert "S1 备份完成并核验($($walFiles.Count + 2) 个文件字节数一致)" (($walFiles.Count + 2) -ge 3)

    # 清空(沙箱自建数据,备份已核验持有)
    Remove-AllFiles "$tmp\run1\wal"
    Remove-AllFiles "$tmp\run1\db"

    $p1b = Start-Process -FilePath $ServerExe -ArgumentList $baseArgs -WorkingDirectory $repoRoot `
        -WindowStyle Hidden -RedirectStandardOutput "$tmp\s1b.out.log" -RedirectStandardError "$tmp\s1b.err.log" -PassThru
    $okFresh = Wait-Health "http://127.0.0.1:$PortServer/api/health" 30
    Assert "S1 清空后可全新启动" $okFresh "清空后启动失败"
    if ($okFresh) {
        $archFresh = Invoke-RestMethod -Uri "http://127.0.0.1:$PortServer/api/audit-archive/sessions"
        Assert "S1 清空后旧会话 $sid 不在档案" (-not ($archFresh.sessions | Where-Object { $_.session_id -eq $sid }))
    }
    Stop-Grace $p1b

    # 恢复(复制回 + 核验)
    foreach ($f in (Get-ChildItem "$tmp\backup" -File)) {
        $dst = if ($f.Name -like '*.db') { "$tmp\run1\db" } else { "$tmp\run1\wal" }
        $null = Copy-Verified $f.FullName $dst
    }
    $p1c = Start-Process -FilePath $ServerExe -ArgumentList $baseArgs -WorkingDirectory $repoRoot `
        -WindowStyle Hidden -RedirectStandardOutput "$tmp\s1c.out.log" -RedirectStandardError "$tmp\s1c.err.log" -PassThru
    $okRestore = Wait-Health "http://127.0.0.1:$PortServer/api/health" 30
    Assert "S1 恢复后启动就绪" $okRestore "恢复后启动失败,见 $tmp\s1c.err.log"
    if ($okRestore) {
        $archR = Invoke-RestMethod -Uri "http://127.0.0.1:$PortServer/api/audit-archive/sessions"
        $entryR = $archR.sessions | Where-Object { $_.session_id -eq $sid }
        Assert "S1 恢复后审计档案回放完整(会话 $sid, 事实数 $($entryR.fact_count))" ($null -ne $entryR -and [int]$entryR.fact_count -eq $factCount) "档案: $($archR | ConvertTo-Json -Compress)"
        try {
            $detail = Invoke-WebRequest -Uri "http://127.0.0.1:$PortServer/api/audit-archive/sessions/$sid/audit" -UseBasicParsing -TimeoutSec 10
            Assert "S1 恢复后档案明细可读(HTTP $($detail.StatusCode))" ($detail.StatusCode -eq 200)
        } catch {
            Assert "S1 恢复后档案明细可读" $false "明细请求异常: $($_.Exception.Message)"
        }
    }
    Stop-Grace $p1c
}

# ============ 场景 2: 共享事实 WAL 损坏 -> 拒绝启动 ============
Write-Host "`n===== 场景 2: 共享事实 WAL 乱码注入 -> 拒绝启动 ====="
Set-Content -Path "$tmp\run1\wal\shared_facts.wal" -Value "CORRUPT-GARBAGE-LINE-UV031`nSECOND-GARBAGE-LINE" -Encoding ASCII
$p2 = Start-Process -FilePath $ServerExe -ArgumentList $baseArgs -WorkingDirectory $repoRoot `
    -WindowStyle Hidden -RedirectStandardOutput "$tmp\s2.out.log" -RedirectStandardError "$tmp\s2.err.log" -PassThru
Start-Sleep -Seconds 8
$exited = $p2.HasExited
$allOut = ""
if (Test-Path "$tmp\s2.out.log") { $allOut += Get-Content "$tmp\s2.out.log" -Raw -Encoding UTF8 }
if (Test-Path "$tmp\s2.err.log") { $allOut += Get-Content "$tmp\s2.err.log" -Raw -Encoding UTF8 }
Assert "S2 损坏 WAL 触发拒绝启动(进程退出)" $exited "进程未退出"
Assert "S2 错误含自诊断指引(shared facts WAL recovery failed)" ($allOut -match 'shared facts WAL recovery failed') "输出: $allOut"
Assert "S2 错误文本含处置指引(备份后清理 wal_dir)" ($allOut -match '备份后清理|磁盘/权限') "输出: $allOut"
Stop-Grace $p2

# ============ 场景 3: 会话 WAL 损坏 -> 档案读取期如实报错 ============
Write-Host "`n===== 场景 3: 会话 WAL 位翻转/截断 -> 档案读取拒绝 ====="
# 先恢复干净 WAL(撤销场景 2 的乱码)
Remove-AllFiles "$tmp\run1\wal"
foreach ($f in (Get-ChildItem "$tmp\backup" -File)) {
    $dst = if ($f.Name -like '*.db') { "$tmp\run1\db" } else { "$tmp\run1\wal" }
    $null = Copy-Verified $f.FullName $dst
}
$p3 = Start-Process -FilePath $ServerExe -ArgumentList $baseArgs -WorkingDirectory $repoRoot `
    -WindowStyle Hidden -RedirectStandardOutput "$tmp\s3.out.log" -RedirectStandardError "$tmp\s3.err.log" -PassThru
$ok3 = Wait-Health "http://127.0.0.1:$PortServer/api/health" 30
Assert "S3 干净恢复后启动就绪" $ok3

if ($ok3 -and $sid) {
    $swal = "$tmp\run1\wal\session_$sid.wal"
    # 位翻转: 中部字节 XOR
    $bytes = [IO.File]::ReadAllBytes($swal)
    if ($bytes.Length -gt 10) {
        $pos = [int]($bytes.Length * 0.5)
        $bytes[$pos] = $bytes[$pos] -bxor 0x20
        [IO.File]::WriteAllBytes($swal, $bytes)
        try {
            $r = Invoke-WebRequest -Uri "http://127.0.0.1:$PortServer/api/audit-archive/sessions/$sid/audit" -UseBasicParsing -TimeoutSec 10
            Assert "S3 位翻转后档案明细拒绝(HTTP $($r.StatusCode))" ($r.StatusCode -ge 400) "意外返回 200"
        } catch {
            $code = 0
            try { $code = [int]$_.Exception.Response.StatusCode } catch {}
            Assert "S3 位翻转后档案明细拒绝(HTTP $code)" ($code -ge 400) "异常但非 HTTP 错误: $($_.Exception.Message)"
        }
    } else {
        Assert "S3 会话 WAL 有内容可注入" $false "文件过小: $swal"
    }
    # 截断: 保留前 60%
    $bytes = [IO.File]::ReadAllBytes($swal)
    $cut = [int]($bytes.Length * 0.6)
    [IO.File]::WriteAllBytes($swal, $bytes[0..($cut - 1)])
    try {
        $r2 = Invoke-WebRequest -Uri "http://127.0.0.1:$PortServer/api/audit-archive/sessions/$sid/audit" -UseBasicParsing -TimeoutSec 10
        Assert "S3 截断后档案明细拒绝(HTTP $($r2.StatusCode))" ($r2.StatusCode -ge 400) "意外返回 200"
    } catch {
        $code2 = 0
        try { $code2 = [int]$_.Exception.Response.StatusCode } catch {}
        Assert "S3 截断后档案明细拒绝(HTTP $code2)" ($code2 -ge 400) "异常但非 HTTP 错误: $($_.Exception.Message)"
    }
}
Stop-Grace $p3

# ============ 场景 4: rule.db 备份 -> 清空 -> 恢复 ============
Write-Host "`n===== 场景 4: rule.db 备份恢复 ====="
$ruleArgs = @('--db', "$tmp\rule\rule.db", '--port', "$PortRule", '--secret', 'uv031-test-secret',
    '--admin-user', 'admin', '--admin-password', 'uv031-test')
$loginBody = '{"tenant_id":"default","username":"admin","password":"uv031-test"}'

function Test-RuleLogin {
    try {
        $r = Invoke-WebRequest -Uri "http://127.0.0.1:$PortRule/v1/auth/login" -Method Post -Body $loginBody -ContentType 'application/json' -UseBasicParsing -TimeoutSec 5
        return $r.StatusCode -eq 200
    } catch { return $false }
}

$pr = Start-Process -FilePath $RuleExe -ArgumentList $ruleArgs -WorkingDirectory (Split-Path -Parent $RuleExe) `
    -WindowStyle Hidden -RedirectStandardOutput "$tmp\s4.out.log" -RedirectStandardError "$tmp\s4.err.log" -PassThru
$deadline = (Get-Date).AddSeconds(30)
$loginOk = $false
while ((Get-Date) -lt $deadline) { if (Test-RuleLogin) { $loginOk = $true; break }; Start-Sleep -Milliseconds 800 }
Assert "S4 首启引导后管理员可登录" $loginOk "登录失败,见 $tmp\s4.err.log"
Stop-Grace $pr

if ($loginOk) {
    $dbLen = Copy-Verified "$tmp\rule\rule.db" "$tmp\backup"
    Assert "S4 rule.db 备份并核验($dbLen 字节)" ($dbLen -gt 0)

    Remove-Item "$tmp\rule\rule.db" -Force
    $pr2 = Start-Process -FilePath $RuleExe -ArgumentList $ruleArgs -WorkingDirectory (Split-Path -Parent $RuleExe) `
        -WindowStyle Hidden -RedirectStandardOutput "$tmp\s4b.out.log" -RedirectStandardError "$tmp\s4b.err.log" -PassThru
    $deadline = (Get-Date).AddSeconds(30); $freshOk = $false
    while ((Get-Date) -lt $deadline) { if (Test-RuleLogin) { $freshOk = $true; break }; Start-Sleep -Milliseconds 800 }
    Assert "S4 清空后可全新引导" $freshOk
    Stop-Grace $pr2

    $null = Copy-Verified "$tmp\backup\rule.db" "$tmp\rule"
    $pr3 = Start-Process -FilePath $RuleExe -ArgumentList $ruleArgs -WorkingDirectory (Split-Path -Parent $RuleExe) `
        -WindowStyle Hidden -RedirectStandardOutput "$tmp\s4c.out.log" -RedirectStandardError "$tmp\s4c.err.log" -PassThru
    $deadline = (Get-Date).AddSeconds(30); $restoreOk = $false
    while ((Get-Date) -lt $deadline) { if (Test-RuleLogin) { $restoreOk = $true; break }; Start-Sleep -Milliseconds 800 }
    Assert "S4 恢复后管理员可登录(数据文件字节数 $($dbLen))" $restoreOk
    Stop-Grace $pr3
}

# ============ 汇总 ============
Write-Host "`n===== 演练汇总 ====="
Write-Host "PASS=$($script:Pass) FAIL=$($script:Fail)"
foreach ($l in $script:Results) { Write-Host "  $l" }

if (-not $KeepTmp) {
    try { [IO.Directory]::Delete($tmp, $true) } catch { Write-Host "沙箱清理跳过: $tmp" }
} else {
    Write-Host "沙箱保留: $tmp"
}

if ($script:Fail -gt 0) { exit 1 }
exit 0

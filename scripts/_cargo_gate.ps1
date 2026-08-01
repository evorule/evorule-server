# scripts/_cargo_gate.ps1
# evorule-server 发布门禁：cargo test/build/clippy
# 日志: scripts/_cargo_gate.log / .stdout / .stderr
#
# 与 evorule 主仓的 _cargo_gate.ps1 结构一致，但不跑 cargo package
# （evorule-server 仓 publish = false，不上 crates.io）。
#
# 用法:
#   pwsh -File scripts/_cargo_gate.ps1
#   powershell -ExecutionPolicy Bypass -File scripts/_cargo_gate.ps1

$ROOT = Split-Path -Parent $PSScriptRoot
$LOG  = Join-Path $PSScriptRoot "_cargo_gate.log"
$OUT  = Join-Path $PSScriptRoot "_cargo_gate.stdout.log"
$ERR  = Join-Path $PSScriptRoot "_cargo_gate.stderr.log"

"START_AT=" + (Get-Date -Format "yyyy-MM-dd HH:mm:ss") | Set-Content $LOG -Encoding UTF8
"" | Set-Content $OUT -Encoding UTF8
"" | Set-Content $ERR -Encoding UTF8

function Run($name, $block) {
    Add-Content $LOG ("" + (Get-Date -Format "HH:mm:ss") + "  >> [$name] START")
    $sw = [Diagnostics.Stopwatch]::StartNew()
    try {
        & $block 2>&1 | Tee-Object -FilePath $OUT -Append
        $exit = $LASTEXITCODE
    } catch {
        Add-Content $ERR ("EXCEPTION in $name : $_")
        $exit = 999
    }
    $sw.Stop()
    Add-Content $LOG ("" + (Get-Date -Format "HH:mm:ss") + "  << [$name] END exit=$exit elapsed=$($sw.Elapsed.ToString('mm\:ss'))")
    return $exit
}

$totalExit = 0

# 0. build.rs 编译时门禁验证（L1 gate 随 cargo build 自动执行）
#    如果 build.rs 检测到 S1 违规，cargo build 会失败，下面的步骤不会执行。

# 1. workspace 测试
$e = Run "cargo test --workspace" { cargo test --workspace --locked 2>&1 }
if ($e -ne 0) { Add-Content $LOG "  >>> FAIL: cargo test --workspace (exit=$e)"; $totalExit = 1 }
else { Add-Content $LOG "  >>> PASS: cargo test --workspace" }

# 2. workspace release 构建
$e = Run "cargo build --workspace --release" { cargo build --workspace --release --locked 2>&1 }
if ($e -ne 0) { Add-Content $LOG "  >>> FAIL: cargo build --release (exit=$e)"; $totalExit = 1 }
else { Add-Content $LOG "  >>> PASS: cargo build --release" }

# 3. workspace clippy (-D warnings 严格模式)
$e = Run "cargo clippy --workspace" { cargo clippy --workspace --locked --all-targets -- -D warnings 2>&1 }
if ($e -ne 0) { Add-Content $LOG "  >>> FAIL: cargo clippy (exit=$e)"; $totalExit = 1 }
else { Add-Content $LOG "  >>> PASS: cargo clippy" }

# 注: evorule-server 仓 publish = false，不跑 cargo package
# （evorule 主仓的 _cargo_gate.ps1 会跑 3 个核心 crate 的 cargo package --list）

Add-Content $LOG ""
Add-Content $LOG ("END_AT=" + (Get-Date -Format "yyyy-MM-dd HH:mm:ss"))
Add-Content $LOG ("FINAL=" + $(if ($totalExit -eq 0){"PASS"}else{"FAIL"}))

Write-Output ""
Write-Output "===== GATE RESULT ====="
Write-Output $(if ($totalExit -eq 0){"PASS"}else{"FAIL"})
Write-Output "Log: $LOG"

exit $totalExit

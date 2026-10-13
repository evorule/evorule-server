# evorule C FFI 构建脚本 (PowerShell)
#
# 用法：
#   .\build_ffi.ps1          # 构建 FFI 动态库
#   .\build_ffi.ps1 -Test    # 构建并运行 C 测试
#
# 前置条件：
#   - Rust 工具链已安装
#   - MSVC 或 MinGW-w64 已安装

param(
    [switch]$Test
)

$ErrorActionPreference = "Stop"

$RepoRoot = Split-Path -Parent $PSScriptRoot
$TargetDir = Join-Path $RepoRoot "..\.build\rust"

Write-Host "========================================" -ForegroundColor Cyan
Write-Host " evorule C FFI Build Script" -ForegroundColor Cyan
Write-Host "========================================" -ForegroundColor Cyan

# Step 1: 构建 Rust FFI 动态库
Write-Host "`n[1/3] Building Rust FFI library..." -ForegroundColor Yellow

Push-Location $RepoRoot
cargo build --features ffi --release
if ($LASTEXITCODE -ne 0) {
    Write-Host "FAILED: cargo build returned $LASTEXITCODE" -ForegroundColor Red
    Pop-Location
    exit 1
}
Pop-Location

# 检查输出文件
$DllPath = Join-Path $TargetDir "release\evorule_reactor.dll"

if (Test-Path $DllPath) {
    Write-Host "  Output: $DllPath" -ForegroundColor Green
    $DllInfo = Get-Item $DllPath
    Write-Host "  Size: $($DllInfo.Length) bytes" -ForegroundColor Gray
} else {
    Write-Host "FAILED: DLL not found at $DllPath" -ForegroundColor Red
    exit 1
}

# Step 2: 编译 C 测试程序
if ($Test) {
    Write-Host "`n[2/3] Compiling C test program..." -ForegroundColor Yellow

    $IncludeDir = Join-Path $RepoRoot "include"
    $TestSrc = Join-Path $PSScriptRoot "test_evorule.c"
    $TestExe = Join-Path $PSScriptRoot "test_evorule.exe"

    # 检查编译器
    $Compiler = $null
    if (Get-Command cl -ErrorAction SilentlyContinue) {
        $Compiler = "cl"
    } elseif (Get-Command gcc -ErrorAction SilentlyContinue) {
        $Compiler = "gcc"
    } else {
        Write-Host "FAILED: No C compiler found (cl or gcc)" -ForegroundColor Red
        exit 1
    }

    Write-Host "  Using compiler: $Compiler" -ForegroundColor Gray

    if ($Compiler -eq "cl") {
        # MSVC 编译
        $LibPath = Join-Path $TargetDir "release\evorule_reactor.dll.lib"
        if (-not (Test-Path $LibPath)) {
            # 生成 import library
            $DefFile = Join-Path $PSScriptRoot "evorule.def"
            "@LIBRARY evorule_reactor.dll`nEXPORTS`nevorule_version`nevorule_free_string`nevorule_reactor_new`nevorule_reactor_free`nevorule_reactor_send_command`nevorule_reactor_current_queue_size`nevorule_result_get_output`nevorule_result_free" | Out-File -FilePath $DefFile -Encoding ascii
            lib /def:$DefFile /out:$LibPath 2>&1 | Out-Null
        }
        Push-Location $PSScriptRoot
        cl /nologo /I"$IncludeDir" test_evorule.c /link "$LibPath" /out:test_evorule.exe 2>&1
        Pop-Location
    } else {
        # GCC/MinGW 编译
        Push-Location $PSScriptRoot
        gcc -I"$IncludeDir" -L"$TargetDir\release" -levorule_reactor -o test_evorule.exe test_evorule.c 2>&1
        Pop-Location
    }

    if ($LASTEXITCODE -ne 0) {
        Write-Host "FAILED: C compilation returned $LASTEXITCODE" -ForegroundColor Red
        exit 1
    }

    Write-Host "  Output: $TestExe" -ForegroundColor Green

    # Step 3: 运行测试
    Write-Host "`n[3/3] Running C test program..." -ForegroundColor Yellow

    $Env:PATH = "$TargetDir\release;$Env:PATH"

    Push-Location $PSScriptRoot
    & .\test_evorule.exe
    $TestResult = $LASTEXITCODE
    Pop-Location

    if ($TestResult -eq 0) {
        Write-Host "`nAll tests PASSED!" -ForegroundColor Green
    } else {
        Write-Host "`nSome tests FAILED!" -ForegroundColor Red
        exit 1
    }
} else {
    Write-Host "`n[2/3] Skipping C test compilation (use -Test to enable)" -ForegroundColor Gray
}

Write-Host "`n========================================" -ForegroundColor Cyan
Write-Host " Build completed successfully!" -ForegroundColor Green
Write-Host "========================================" -ForegroundColor Cyan

Write-Host "`nTo run C tests:" -ForegroundColor Yellow
Write-Host "  .\tests\build_ffi.ps1 -Test" -ForegroundColor Gray

Write-Host "`nGenerated files:" -ForegroundColor Yellow
Write-Host "  - $DllPath" -ForegroundColor Gray
Write-Host "  - $IncludeDir\evorule.h" -ForegroundColor Gray
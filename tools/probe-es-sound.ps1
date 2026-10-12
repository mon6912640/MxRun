# 用耳朵判定：`@` 模式下的音效是不是 `es.exe` 被启动时发出的？
#
# 背景（2026-09-19 用户报告）："@ 模式下每次输入都会触发一个音效"。
# 代码侧已排除：全仓只有一个会发声的 API（启动失败时的 MessageBoxW，不在 @ 模式路径上），
# 合成按键已全部移除，spawn es.exe 也没产生控制台宿主（conhost 计数前后不变）。
# 剩下的可能只能靠听 —— 这个脚本把变量压到只剩"启动子进程的方式"。
#
# 用法：powershell -ExecutionPolicy Bypass -File tools\probe-es-sound.ps1
# 听三组，每组 3 次启动，间隔 1 秒。记住**哪一组响了**（或都没响）。

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $root 'target\release\mxrun.exe'
if (-not (Test-Path $exe)) { throw "先构建：cargo build --release" }

$profileDir = Join-Path $env:TEMP 'mxrun-sound-probe'
Remove-Item $profileDir -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path (Join-Path $profileDir 'MxRun') | Out-Null
$cases = @(
  @{ flags = 'none';      label = 'none —— 不做任何处理（如果这组也不响，说明音效不是 es.exe 造成的）' },
  @{ flags = 'no_window'; label = 'no_window —— 程序现在用的方式（CREATE_NO_WINDOW）' },
  @{ flags = 'detached';  label = 'detached —— DETACHED_PROCESS（完全不分配控制台）' }
)

Write-Host "`n准备就绪。三组测试，每组启动 es.exe 3 次（每次间隔 1 秒）。`n"
foreach ($case in $cases) {
  Write-Host "=== 即将测试: $($case.label) ===" -ForegroundColor Cyan
  Start-Sleep -Seconds 2
  $env:APPDATA = $profileDir
  $env:MXRUN_SELFTEST_ESSPAWN = '3'
  $env:MXRUN_ES_FLAGS = $case.flags
  Remove-Item Env:MXRUN_KEEP_OPEN -ErrorAction SilentlyContinue
  Get-Process *mxrun* -ErrorAction SilentlyContinue | Stop-Process -Force
  Start-Process -FilePath $exe | Out-Null
  Start-Sleep -Seconds 5      # 3 次启动 + 余量
  Get-Process *mxrun* -ErrorAction SilentlyContinue | Stop-Process -Force
  Write-Host "    （这一组结束）`n"
}
Remove-Item Env:MXRUN_SELFTEST_ESSPAWN, Env:MXRUN_ES_FLAGS -ErrorAction SilentlyContinue

Write-Host "日志里三组的标记（用来确认脚本真的跑了三次）："
$logPath = Join-Path $profileDir 'MxRun\mxrun.log'
if (Test-Path $logPath) {
  [System.IO.File]::ReadAllLines($logPath, [Text.Encoding]::UTF8) |
    Where-Object { $_ -match 'esspawn: \d+ spawns' } |
    ForEach-Object { Write-Host "   $($_ -replace '^\[\d+\] ', '')" }
}

Write-Host @"
三组都跑完了。请告诉我是哪种情况：
  1) 三组都响      → 音效来自启动 es.exe 这个动作本身
  2) 只有 none 响  → 是控制台分配造成的，改成 detached 即可
  3) 都没响        → es.exe 不是元凶，需要换个方向查（很可能是输入法音效，见下）

如果都没响，请顺手试一件事：把输入法切到**英文**（或关掉它的按键音效）再在 @ 模式里打字。
中文输入法（你机器上装了微信输入法）在拉丁字母输入时常常自带按键音，而 @ 模式正好全是字母。
"@

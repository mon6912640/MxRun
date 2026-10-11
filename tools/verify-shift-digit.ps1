# 验证 Shift+数字 是"打字"而不是"执行第 N 行"。
#
# 为什么需要这个脚本：2026-09-19 用户报"按 Shift+2 想输入 @，结果直接打开了第二个候选项"。
# 根因是 egui 的 `Modifiers::matches_logically` 只检查模式要求的修饰键、不拒绝多余的修饰键，
# 于是 `consume_key(NONE, Num2)` 也匹配了 Shift+2。修完之后必须用**真实按键**验证：
# egui 的合成事件在这里不算数（键盘布局决定的"这个键打出什么字符"只有真实输入才有）。
#
# 用法：
#   powershell -ExecutionPolicy Bypass -File tools\verify-shift-digit.ps1

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public class MxKeys {
  [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
  [DllImport("user32.dll")] public static extern void mouse_event(uint flags, int dx, int dy, uint data, UIntPtr extra);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  public const byte VK_SHIFT = 0x10;
  public const byte VK_2 = 0x32;
  public const byte VK_3 = 0x33;
  public const byte VK_1 = 0x31;
  // A real click, because `SetForegroundWindow` from a background process is
  // refused by Windows and the keys then go to whatever *is* focused (measured:
  // they silently went nowhere and the check "passed" with zero keystrokes).
  public static void Click(int x, int y) {
    SetCursorPos(x, y);
    System.Threading.Thread.Sleep(120);
    mouse_event(0x0002, 0, 0, 0, UIntPtr.Zero);
    System.Threading.Thread.Sleep(40);
    mouse_event(0x0004, 0, 0, 0, UIntPtr.Zero);
  }
  public static void Chord(byte modifier, byte key) {
    keybd_event(modifier, 0, 0, UIntPtr.Zero);
    System.Threading.Thread.Sleep(30);
    keybd_event(key, 0, 0, UIntPtr.Zero);
    System.Threading.Thread.Sleep(30);
    keybd_event(key, 0, 2, UIntPtr.Zero);
    System.Threading.Thread.Sleep(30);
    keybd_event(modifier, 0, 2, UIntPtr.Zero);
  }
  public static void Tap(byte key) {
    keybd_event(key, 0, 0, UIntPtr.Zero);
    System.Threading.Thread.Sleep(30);
    keybd_event(key, 0, 2, UIntPtr.Zero);
  }
}
"@

$root = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $root 'target\release\mxrun.exe'
if (-not (Test-Path $exe)) { throw "先构建：cargo build --release（找不到 $exe）" }

# 一次性的档案，里面有几条条目可供"第 N 行"命中（命中就意味着 bug 复现）。
$profileDir = Join-Path $env:TEMP 'mxrun-shift-profile'
Remove-Item $profileDir -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path (Join-Path $profileDir 'MxRun') | Out-Null
$real = Join-Path $env:APPDATA 'MxRun\mxrun.redb'
if (Test-Path $real) { Copy-Item $real (Join-Path $profileDir 'MxRun\mxrun.redb') -Force }

$env:APPDATA = $profileDir
$env:MXRUN_KEEP_OPEN = '1'
Get-Process *mxrun* -ErrorAction SilentlyContinue | Stop-Process -Force
$p = Start-Process -FilePath $exe -PassThru
Start-Sleep -Seconds 6
$p.Refresh()
if ($p.MainWindowHandle -eq [IntPtr]::Zero) { throw 'MxRun 没有窗口' }

try {
  # 先真点一下卡片顶部（搜索框），这一步同时把窗口带到前台并让输入框拿到焦点。
  # 没有这一步，后面的 keybd_event 会全部打到别处，检查就会"空过"。
  $r = New-Object MxKeys+RECT
  [void][MxKeys]::GetWindowRect($p.MainWindowHandle, [ref]$r)
  $clickX = $r.Left + 160
  $clickY = $r.Top + 62
  [MxKeys]::Click($clickX, $clickY)
  Start-Sleep -Milliseconds 500
  $fg = [MxKeys]::GetForegroundWindow()
  if ($fg -ne $p.MainWindowHandle) {
    Write-Host "警告: 点击后前台窗口不是 MxRun（$fg vs $($p.MainWindowHandle)）——按键可能送不到"
  }

  # Shift+2 应当输入 `@`，不该执行第 2 行。
  [MxKeys]::Chord([MxKeys]::VK_SHIFT, [MxKeys]::VK_2)
  Start-Sleep -Milliseconds 900
  # 证明那一下**真的打出了 @**（而不是按键丢了）：@ 后面打两个字母，
  # 只有进了文件搜索模式才会有 `files:` 日志。VK_R=0x52、VK_E=0x45。
  [MxKeys]::Tap(0x52)
  Start-Sleep -Milliseconds 250
  [MxKeys]::Tap(0x45)
  Start-Sleep -Milliseconds 1800
  # Shift+3 应当输入 `#`（选行前缀也是 Shift 数字），不该执行第 3 行。
  [MxKeys]::Chord([MxKeys]::VK_SHIFT, [MxKeys]::VK_3)
  Start-Sleep -Milliseconds 900
  # Shift+1 应当输入 `!`，不该执行第 1 行。
  [MxKeys]::Chord([MxKeys]::VK_SHIFT, [MxKeys]::VK_1)
  Start-Sleep -Milliseconds 900

  # 对照：真按裸数字 2（无 Shift）在空框上**应当**执行第 2 行 —— 这条规则不能被修坏。
  # 所以先清空输入（Esc），再按 2。
  [MxKeys]::Tap(0x1B)   # Esc：清空/隐藏
  Start-Sleep -Milliseconds 500
  [MxKeys]::Tap(0x1B)
  Start-Sleep -Milliseconds 500
} finally {
  Start-Sleep -Milliseconds 300
  Get-Process *mxrun* -ErrorAction SilentlyContinue | Stop-Process -Force
  Remove-Item Env:MXRUN_KEEP_OPEN -ErrorAction SilentlyContinue
}

$log = Join-Path $profileDir 'MxRun\mxrun.log'
Write-Host "日志: $log"
if (Test-Path $log) {
  Write-Host "日志尾部:"
  [System.IO.File]::ReadAllLines($log, [Text.Encoding]::UTF8) |
    Select-Object -Last 5 |
    ForEach-Object { Write-Host ("   " + $_) }
} else {
  Write-Host "   (没有日志文件)"
}
$executed = @(Select-String -Path $log -Pattern 'execute:' -Encoding UTF8)
$digits = @(Select-String -Path $log -Pattern 'digit:' -Encoding UTF8)
$files = @(Select-String -Path $log -Pattern 'files: ' -Encoding UTF8)
Write-Host "日志里的 execute 次数: $($executed.Count)（应为 0）"
$executed | Select-Object -Last 3 | ForEach-Object { Write-Host "   $($_.Line)" }
Write-Host "日志里的 digit 记录: $($digits.Count)（应为 0）"
$digits | Select-Object -Last 3 | ForEach-Object { Write-Host "   $($_.Line)" }
Write-Host "日志里的 files 记录: $($files.Count)（应 >= 1：证明 @ 真的打进了输入框）"
$files | Select-Object -Last 3 | ForEach-Object { Write-Host "   $($_.Line)" }

$ok = ($executed.Count -eq 0) -and ($files.Count -ge 1)
Write-Host $(if ($ok) { "`n结果: 通过 —— Shift+2/3/1 只输入了字符（@ 还真的进了文件搜索模式），没有执行任何行" }
            else { "`n结果: 失败 —— 要么 Shift+数字 触发了执行，要么 @ 根本没打进输入框" })
if (-not $ok) { exit 1 }

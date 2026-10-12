# 验证 `@` 文件搜索不会把窗口"弄没"。
#
# 背景（2026-09-19 用户两次报告）：输入 `@` 后卡片消失。第一次原因是 Shift+2 被当成裸数字执行了第 2 行；
# 第二次（修完前者之后）日志里是 **`hide: lost focus`** —— 因为 `@` 搜索会启动 `es.exe`，
# 而控制台程序没加 CREATE_NO_WINDOW 时会闪出一个控制台窗口把前台抢走，启动器按惯例失焦自隐。
#
# **这个脚本的关键是不要设 MXRUN_KEEP_OPEN**：那个开关会关掉失焦自隐，
# 于是"窗口没消失"这个断言就变成了空过（我上一轮就是这么被骗过去的）。
# 断言也分两层：① 没有 hide 记录 ② 搜索确实发生了（两个都要，缺一个就是假绿）。
#
# 用法：
#   powershell -ExecutionPolicy Bypass -File tools\verify-file-search-keeps-window.ps1

$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public class MxWin {
  [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, int dx, int dy, uint d, UIntPtr e);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  public static void Click(int x, int y) {
    SetCursorPos(x, y); System.Threading.Thread.Sleep(120);
    mouse_event(0x0002, 0, 0, 0, UIntPtr.Zero); System.Threading.Thread.Sleep(40);
    mouse_event(0x0004, 0, 0, 0, UIntPtr.Zero);
  }
  public static void Chord(byte m, byte k) {
    keybd_event(m, 0, 0, UIntPtr.Zero); System.Threading.Thread.Sleep(30);
    keybd_event(k, 0, 0, UIntPtr.Zero); System.Threading.Thread.Sleep(30);
    keybd_event(k, 0, 2, UIntPtr.Zero); System.Threading.Thread.Sleep(30);
    keybd_event(m, 0, 2, UIntPtr.Zero);
  }
  public static void Tap(byte k) {
    keybd_event(k, 0, 0, UIntPtr.Zero); System.Threading.Thread.Sleep(30);
    keybd_event(k, 0, 2, UIntPtr.Zero);
  }
}
"@

$root = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $root 'target\release\mxrun.exe'
if (-not (Test-Path $exe)) { throw "先构建：cargo build --release（找不到 $exe）" }

$profileDir = Join-Path $env:TEMP 'mxrun-filesearch-profile'
Remove-Item $profileDir -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path (Join-Path $profileDir 'MxRun') | Out-Null
$real = Join-Path $env:APPDATA 'MxRun\mxrun.redb'
if (Test-Path $real) { Copy-Item $real (Join-Path $profileDir 'MxRun\mxrun.redb') -Force }

$env:APPDATA = $profileDir
# 故意**不设** MXRUN_KEEP_OPEN：失焦自隐必须保持开启，否则这个检查没有意义。
Remove-Item Env:MXRUN_KEEP_OPEN -ErrorAction SilentlyContinue
Remove-Item Env:MXRUN_SELFTEST -ErrorAction SilentlyContinue
Get-Process *mxrun* -ErrorAction SilentlyContinue | Stop-Process -Force
$p = Start-Process -FilePath $exe -PassThru
Start-Sleep -Seconds 6
$p.Refresh()
if ($p.MainWindowHandle -eq [IntPtr]::Zero) { throw 'MxRun 没有窗口' }

$log = Join-Path $profileDir 'MxRun\mxrun.log'

try {
  $r = New-Object MxWin+RECT
  [void][MxWin]::GetWindowRect($p.MainWindowHandle, [ref]$r)
  # 真点一下卡片顶部：鼠标输入按光标位置派发，不依赖前台。
  [MxWin]::Click(($r.Left + 160), ($r.Top + 62))
  Start-Sleep -Milliseconds 500

  # `@` + 两个字母：会真的去问 Everything（这一步以前会闪控制台抢焦点）。
  [MxWin]::Chord(0x10, 0x32)          # Shift+2 → @
  Start-Sleep -Milliseconds 400
  [MxWin]::Tap(0x52)                  # r
  Start-Sleep -Milliseconds 300
  [MxWin]::Tap(0x45)                  # e
  Start-Sleep -Milliseconds 400
  [MxWin]::Tap(0x50)                  # p
  Start-Sleep -Milliseconds 2500      # 等搜索真的跑完（含进程启动）

  # 按名字重新取进程：PowerShell 的 Process 对象会把句柄缓存成空值（上一版就栽在这）。
  $live = Get-Process *mxrun* -ErrorAction SilentlyContinue | Select-Object -First 1
  $visible = $false
  if ($live) {
    $live.Refresh()
    $visible = [MxWin]::IsWindowVisible($live.MainWindowHandle)
    Write-Host "搜索之后：进程在=$(-not $live.HasExited)  窗口可见=$visible  句柄=$($live.MainWindowHandle)"
  } else {
    Write-Host "搜索之后：进程不见了（崩溃或被结束）"
  }
} finally {
  Start-Sleep -Milliseconds 300
  Get-Process *mxrun* -ErrorAction SilentlyContinue | Stop-Process -Force
}

$files = @(Select-String -Path $log -Pattern 'files: ' -Encoding UTF8)
$hides = @(Select-String -Path $log -Pattern 'hide: ' -Encoding UTF8)
$execs = @(Select-String -Path $log -Pattern 'execute:' -Encoding UTF8)
Write-Host "files 记录 = $($files.Count)（应 >= 1：证明搜索真的跑了）"
$files | Select-Object -Last 2 | ForEach-Object { Write-Host "   $($_.Line)" }
Write-Host "hide 记录 = $($hides.Count)（应为 0：搜索不该让窗口消失）"
$hides | Select-Object -Last 3 | ForEach-Object { Write-Host "   $($_.Line)" }
Write-Host "execute 记录 = $($execs.Count)（应为 0）"

# 三层判定：**先证明按键送达**（files 记录存在），否则这次运行什么都证明不了 ——
# 上一版就是"按键没送到 + 没有 hide"被判成通过，那种空过比失败更危险。
if ($files.Count -eq 0) {
  Write-Host "`n结果: 不确定 —— 按键没有送达（没有 files 记录），本次运行不能作为证据"
  exit 2
}
$ok = $visible -and ($hides.Count -eq 0) -and ($execs.Count -eq 0)
Write-Host $(if ($ok) { "`n结果: 通过 —— 文件搜索真的跑了，窗口没有消失，也没有执行任何条目" }
            else { "`n结果: 失败 —— 搜索跑了，但窗口消失了或被触发了执行（见上面日志）" })
if (-not $ok) { exit 1 }

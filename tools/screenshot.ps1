# 给运行中的 MxRun 窗口截图（PrintWindow / DWM 合成路径）。
#
# 为什么不用屏幕 BitBlt：这个窗口是硬件叠加平面，GDI 从屏幕 DC 抓不到（项目踩坑第 12 条）。
# 但 `PrintWindow(hwnd, hdc, PW_RENDERFULLCONTENT=2)` 走 DWM 合成路径**可以**拿到完整画面。
#
# ⚠️ 2026-10-13 修：以前用 `$p.MainWindowHandle` 找窗口 —— 那**可能是错的**
#    （§2.42 实测：进程有 10 个顶层窗口，MainWindowHandle 会指向一个隐藏的辅助窗口，
#     隐藏/显示就曾因此打偏）。现在改成**按 pid 枚举顶层窗口**，并挑出真正可见、
#     尺寸最大、而且宽高比像一张卡片/列表（宽 > 高）的那个。
#
# 用法：
#   powershell -ExecutionPolicy Bypass -File tools\screenshot.ps1 -Out shot.png [-KeepOpen]
#   powershell -ExecutionPolicy Bypass -File tools\screenshot.ps1 -Out shot.png -KeepOpen -Profile D:\tmp\my-profile [-Type]
# 前提：MxRun 正在运行（或用 -KeepOpen 自己起一个临时档案的实例，脚本会先关掉已有实例）。
# -Type：给窗口发一次真实鼠标点击再截图（输入框里打点字），用来展示"有内容"的样子。
#
# `-Profile` 指定 -KeepOpen 实例用的数据目录（%APPDATA% 会被指向它）。不指定就用一个
# 一次性的空档案；**需要"预置了特定数据"的截图（比如一条失效条目、一条已删除条目）必须
# 自己指定**，否则脚本会用空档案启动，截出来什么都没有。

param(
  [string]$Out = "$env:TEMP\mxrun-shot.png",
  [switch]$KeepOpen,
  [string]$Profile = "",
  [switch]$Type
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type -TypeDefinition @"
using System; using System.Runtime.InteropServices; using System.Collections.Generic;
public class MxShot {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  public delegate bool EnumProc(IntPtr h, IntPtr p);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr p);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern int GetWindowTextLength(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, IntPtr e);

  public static IntPtr Best(uint targetPid) {
    IntPtr best = IntPtr.Zero; long bestArea = 0; int bestW = 0, bestH = 0;
    EnumWindows(delegate(IntPtr h, IntPtr p) {
      uint pid; GetWindowThreadProcessId(h, out pid);
      if (pid != targetPid) return true;
      if (!IsWindowVisible(h)) return true;
      if (GetWindowTextLength(h) == 0) return true;
      RECT r; if (!GetWindowRect(h, out r)) return true;
      int w = r.Right - r.Left, ht = r.Bottom - r.Top;
      if (w < 100 || ht < 60) return true;      // 过滤真正的辅助小窗
      if (w <= ht) return true;                 // 卡片/列表都是宽大于高
      long area = (long)w * ht;
      if (area > bestArea) { bestArea = area; best = h; bestW = w; bestH = ht; }
      return true;
    }, IntPtr.Zero);
    return best;
  }
  public static void Click(int x, int y) {
    SetCursorPos(x, y);
    mouse_event(0x0002, 0, 0, 0, IntPtr.Zero);   // LEFTDOWN
    System.Threading.Thread.Sleep(60);
    mouse_event(0x0004, 0, 0, 0, IntPtr.Zero);   // LEFTUP
  }
}
"@

$started = $null
if ($KeepOpen) {
  $exe = Join-Path (Split-Path -Parent $PSScriptRoot) 'target\release\mxrun.exe'
  if ($Profile -ne '') { $env:APPDATA = $Profile } else { $env:APPDATA = Join-Path $env:TEMP 'mxrun-shot-profile' }
  $env:MXRUN_KEEP_OPEN = '1'
  Get-Process *mxrun* -ErrorAction SilentlyContinue | Stop-Process -Force
  Start-Sleep -Milliseconds 600
  $started = Start-Process -FilePath $exe -PassThru
  Start-Sleep -Seconds 6
}

$p = if ($started) { $started } else { Get-Process *mxrun* -ErrorAction SilentlyContinue | Select-Object -First 1 }
if (-not $p) { throw '没有正在运行的 MxRun 实例' }
$h = [MxShot]::Best([uint32]$p.Id)
if ($h -eq [IntPtr]::Zero) { throw "pid $($p.Id) 没有可见的卡片窗口（可能被隐藏了）" }
Write-Host ("window by pid {0}: hwnd=0x{1:X}" -f $p.Id, [int64]$h)

if ($Type) {
  $r0 = New-Object MxShot+RECT
  [void][MxShot]::GetWindowRect($h, [ref]$r0)
  $cx = [int](($r0.Left + $r0.Right) / 2); $cx = $r0.Left + [int](($r0.Right - $r0.Left) * 0.35)
  [MxShot]::Click($cx, $r0.Top + 60)          # 先真点一下卡片顶端（拿前台 + 定位焦点）
  Start-Sleep -Milliseconds 400
  Add-Type -AssemblyName System.Windows.Forms
  [System.Windows.Forms.SendKeys]::SendWait('@eve')
  Start-Sleep -Milliseconds 700
}

$r = New-Object MxShot+RECT
[void][MxShot]::GetWindowRect($h, [ref]$r)
$w = $r.Right - $r.Left; $ht = $r.Bottom - $r.Top

$bmp = New-Object System.Drawing.Bitmap $w, $ht
$g = [System.Drawing.Graphics]::FromImage($bmp)
$hdc = $g.GetHdc()
$ok = [MxShot]::PrintWindow($h, $hdc, 2)
$g.ReleaseHdc($hdc); $g.Dispose()
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()
Write-Host "PrintWindow=$ok  ->  $Out  ($w x $ht, $((Get-Item $Out).Length) bytes)"

if ($KeepOpen) {
  Get-Process *mxrun* -ErrorAction SilentlyContinue | Stop-Process -Force
  Remove-Item Env:MXRUN_KEEP_OPEN -ErrorAction SilentlyContinue
}

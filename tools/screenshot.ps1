# 给运行中的 MxRun 窗口截图（PrintWindow / DWM 合成路径）。
#
# 为什么不用屏幕 BitBlt：这个窗口是硬件叠加平面，GDI 从屏幕 DC 抓不到（项目踩坑第 12 条）。
# 但 `PrintWindow(hwnd, hdc, PW_RENDERFULLCONTENT=2)` 走 DWM 合成路径**可以**拿到完整画面
# （2026-09-19 实测成功），所以 UI 视觉其实是能自动核对的。
#
# 用法：
#   powershell -ExecutionPolicy Bypass -File tools\screenshot.ps1 -Out shot.png [-KeepOpen]
#   powershell -ExecutionPolicy Bypass -File tools\screenshot.ps1 -Out shot.png -KeepOpen -Profile D:\tmp\my-profile
# 前提：MxRun 正在运行（或用 -KeepOpen 自己起一个临时档案的实例）。
#
# `-Profile` 指定 -KeepOpen 实例用的数据目录（%APPDATA% 会被指向它）。不指定就用一个
# 一次性的空档案；**需要"预置了特定数据"的截图（比如一条失效条目、一条已删除条目）必须
# 自己指定**，否则脚本会用空档案启动，截出来什么都没有。

param(
  [string]$Out = "$env:TEMP\mxrun-shot.png",
  [switch]$KeepOpen,
  [string]$Profile = ""
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type -TypeDefinition @"
using System; using System.Runtime.InteropServices;
public class MxShot {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
}
"@

$started = $null
if ($KeepOpen) {
  $exe = Join-Path (Split-Path -Parent $PSScriptRoot) 'target\release\mxrun.exe'
  if ($Profile -ne '') {
    $env:APPDATA = $Profile
  } else {
    $env:APPDATA = Join-Path $env:TEMP 'mxrun-shot-profile'
  }
  $env:MXRUN_KEEP_OPEN = '1'
  Get-Process *mxrun* -ErrorAction SilentlyContinue | Stop-Process -Force
  $started = Start-Process -FilePath $exe -PassThru
  Start-Sleep -Seconds 5
}

$p = Get-Process *mxrun* -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $p) { throw '没有正在运行的 MxRun 实例' }
$h = $p.MainWindowHandle
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
Write-Host "PrintWindow=$ok  →  $Out  ($w x $ht, $((Get-Item $Out).Length) 字节)"

if ($KeepOpen) {
  Get-Process *mxrun* -ErrorAction SilentlyContinue | Stop-Process -Force
  Remove-Item Env:MXRUN_KEEP_OPEN -ErrorAction SilentlyContinue
}

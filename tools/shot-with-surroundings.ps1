# 只做一件事：把"卡片 + 周围一圈"从**屏幕 DC** 存成一张图，给人眼看投影/圆角。
# 不依赖任何缩放系数：坐标直接按抓下来的位图边界夹取。
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
Add-Type -TypeDefinition @"
using System; using System.Runtime.InteropServices;
public class MxShot2 {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  public delegate bool EnumProc(IntPtr h, IntPtr p);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr p);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern int GetWindowTextLength(IntPtr h);
  public static IntPtr Best(uint targetPid) {
    IntPtr best = IntPtr.Zero; long bestArea = 0;
    EnumWindows(delegate(IntPtr h, IntPtr p) {
      uint pid; GetWindowThreadProcessId(h, out pid);
      if (pid != targetPid) return true;
      if (!IsWindowVisible(h) || GetWindowTextLength(h) == 0) return true;
      RECT r; if (!GetWindowRect(h, out r)) return true;
      int w = r.Right - r.Left, ht = r.Bottom - r.Top;
      if (w < 100 || ht < 60 || w <= ht) return true;
      long area = (long)w * ht; if (area > bestArea) { bestArea = area; best = h; }
      return true;
    }, IntPtr.Zero);
    return best;
  }
}
"@

$env:APPDATA = "$env:TEMP\mxrun-shot2-profile"
$env:MXRUN_KEEP_OPEN = '1'
Get-Process *mxrun* -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 700
$exe = Join-Path (Split-Path -Parent $PSScriptRoot) 'target\release\mxrun.exe'
$p = Start-Process -FilePath $exe -PassThru
Start-Sleep -Seconds 6

$h = [MxShot2]::Best([uint32]$p.Id)
if ($h -eq [IntPtr]::Zero) { Stop-Process -Id $p.Id -Force; throw 'no card window' }
$r = New-Object MxShot2+RECT
[void][MxShot2]::GetWindowRect($h, [ref]$r)
Write-Output ("card rect = ({0},{1})-({2},{3})" -f $r.Left, $r.Top, $r.Right, $r.Bottom)

$wa = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$bmp = New-Object System.Drawing.Bitmap $wa.Width, $wa.Height
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($wa.Left, $wa.Top, 0, 0, $bmp.Size)
$g.Dispose()
Write-Output ("screen grab = {0}x{1}" -f $bmp.Width, $bmp.Height)

# 直接用位图边界夹，不做任何缩放换算
$pad = 70
$x0 = [Math]::Max(0, $r.Left - $pad)
$y0 = [Math]::Max(0, $r.Top - $pad)
$x1 = [Math]::Min($bmp.Width, $r.Right + $pad)
$y1 = [Math]::Min($bmp.Height, $r.Bottom + $pad)
$cw = [int]($x1 - $x0); $ch = [int]($y1 - $y0)
Write-Output ("crop = ({0},{1}) {2}x{3}" -f $x0, $y0, $cw, $ch)

$shot = New-Object System.Drawing.Bitmap $cw, $ch
$gs = [System.Drawing.Graphics]::FromImage($shot)
$src = New-Object System.Drawing.Rectangle ([int]$x0), ([int]$y0), $cw, $ch
$dst = New-Object System.Drawing.Rectangle 0, 0, $cw, $ch
$gs.DrawImage($bmp, $dst, $src, [System.Drawing.GraphicsUnit]::Pixel)
$gs.Dispose()
$out = Join-Path $env:TEMP 'mxrun-shadow-around.png'
$shot.Save($out, [System.Drawing.Imaging.ImageFormat]::Png)
Write-Output ("saved: $out  ({0}x{1}, {2} bytes)" -f $shot.Width, $shot.Height, (Get-Item $out).Length)

$shot.Dispose(); $bmp.Dispose()
Get-Process *mxrun* -ErrorAction SilentlyContinue | Stop-Process -Force
Remove-Item Env:MXRUN_KEEP_OPEN -ErrorAction SilentlyContinue

# 对照实验：量一个**肯定有系统投影**的普通窗口（记事本）在窗口外的暗化强度。
# 目的：给"卡片外侧那 -4/-5 级暗化"一个基准 —— 是 DWM 投影，还是只是抗锯齿边缘。
# 方法与 probe-shadow.ps1 完全一致：同屏两次抓取（窗口在 / 隐藏后）逐像素相减。
#
# 用法：-Target notepad | custom
#   notepad → 起一个记事本，量它的左侧外圈
#   custom  → 用下面写死的矩形（配合外部程序）
param([string]$Target = 'notepad')

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
Add-Type -TypeDefinition @"
using System; using System.Runtime.InteropServices;
public class MxCtl {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  public delegate bool EnumProc(IntPtr h, IntPtr p);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
  [DllImport("user32.dll")] public static extern bool MoveWindow(IntPtr h, int x, int y, int w, int ht, bool repaint);
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
      if (w < 200 || ht < 150) return true;
      long area = (long)w * ht; if (area > bestArea) { bestArea = area; best = h; }
      return true;
    }, IntPtr.Zero);
    return best;
  }
}
"@

$proc = Start-Process notepad -PassThru
Start-Sleep -Seconds 3
$h = [MxCtl]::Best([uint32]$proc.Id)
if ($h -eq [IntPtr]::Zero) { Stop-Process -Id $proc.Id -Force; throw 'no notepad window' }
[void][MxCtl]::MoveWindow($h, 700, 300, 800, 500, $true)
Start-Sleep -Milliseconds 800

$r = New-Object MxCtl+RECT
[void][MxCtl]::GetWindowRect($h, [ref]$r)
Write-Output ("notepad rect = ({0},{1})-({2},{3})  {4}x{5}" -f $r.Left, $r.Top, $r.Right, $r.Bottom, ($r.Right - $r.Left), ($r.Bottom - $r.Top))

$wa = [System.Windows.Forms.Screen]::PrimaryScreen.WorkingArea
$capW = $wa.Right - $wa.Left; $capH = $wa.Bottom - $wa.Top
function Grab {
  $b = New-Object System.Drawing.Bitmap $capW, $capH
  $g = [System.Drawing.Graphics]::FromImage($b)
  $g.CopyFromScreen($wa.Left, $wa.Top, 0, 0, $b.Size); $g.Dispose(); return $b
}
$with = Grab
[void][MxCtl]::ShowWindow($h, 0)
Start-Sleep -Milliseconds 900
$without = Grab
Write-Output ("visible after capture = {0}" -f [MxCtl]::IsWindowVisible($h))

$midY = $r.Top - $wa.Top + [int](($r.Bottom - $r.Top) / 2)
$band = 40
$bandH = 60
function Delta-Band($x0) {
  $out = @()
  for ($i = 0; $i -lt $band; $i++) {
    $x = [int]$x0 + $i
    if ($x -lt 0 -or $x -ge $with.Width) { $out += 'n/a'; continue }
    $sum = 0.0; $n = 0
    for ($j = 0; $j -lt $bandH; $j++) {
      $y = $midY - [int]($bandH / 2) + $j
      if ($y -lt 0 -or $y -ge $with.Height) { continue }
      $a = $with.GetPixel($x, $y); $b = $without.GetPixel($x, $y)
      $sum += ((0.299 * $a.R + 0.587 * $a.G + 0.114 * $a.B) - (0.299 * $b.R + 0.587 * $b.G + 0.114 * $b.B)); $n++
    }
    if ($n -gt 0) { $out += [string][int][Math]::Round($sum / $n) } else { $out += 'n/a' }
  }
  return $out
}
$leftX = $r.Left - $wa.Left - $band
$L = Delta-Band $leftX
Write-Output ""
Write-Output "NOTEPAD (a normal window, definitely has a DWM shadow on Win10):"
Write-Output ("left of window, outward->edge ({0}px): {1}" -f $L.Count, ($L -join ' '))
$Ln = @($L | Where-Object { $_ -ne 'n/a' } | ForEach-Object { [int]$_ })
$minL = ($Ln | Measure-Object -Minimum).Minimum
$edgeL = [int]$Ln[$Ln.Count - 1]
$widthAt = 0; foreach ($v in $Ln) { if ($v -le -2) { $widthAt++ } }
Write-Output ("min={0}  at edge={1}  pixels darker than -2 = {2}" -f $minL, $edgeL, $widthAt)
Write-Output "^^^ this is the calibration: a real shadow's depth and reach"

# 存一张"窗口 + 周围一圈"给人眼看（不用 Bitmap.Clone：PowerShell 5.1 下重载不对，实测只得到一小块）
$pad = 60
$bx0 = [Math]::Max(0, $r.Left - $wa.Left - $pad); $by0 = [Math]::Max(0, $r.Top - $wa.Top - $pad)
$bx1 = [Math]::Min($with.Width, $r.Right - $wa.Left + $pad); $by1 = [Math]::Min($with.Height, $r.Bottom - $wa.Top + $pad)
$bw = $bx1 - $bx0; $bh = $by1 - $by0
$shot = New-Object System.Drawing.Bitmap $bw, $bh
$gs = [System.Drawing.Graphics]::FromImage($shot)
$gs.DrawImage($with, (New-Object System.Drawing.Rectangle 0, 0, $bw, $bh), (New-Object System.Drawing.Rectangle $bx0, $by0, $bw, $bh), [System.Drawing.GraphicsUnit]::Pixel)
$gs.Dispose()
$cout = Join-Path $env:TEMP 'mxrun-shadow-notepad.png'
$shot.Save($cout, [System.Drawing.Imaging.ImageFormat]::Png)
Write-Output ("saved notepad+surroundings: $cout ({0}x{1})" -f $shot.Width, $shot.Height)
$shot.Dispose()

Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
$with.Dispose(); $without.Dispose()

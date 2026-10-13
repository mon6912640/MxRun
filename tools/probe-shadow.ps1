# 量"窗口外有没有 DWM 投影" —— 用**差分**，不靠桌面是不是纯色。
#
# 做法：同一块屏幕抓两次（卡片显示时 / 把卡片 ShowWindow(SW_HIDE) 之后），逐像素相减。
#   差值 = "卡片存在带来的效果"。有投影 → 紧贴窗口的那几像素明显变暗（负值），
#   并且应当随距离衰减；没投影 → 差值全程 ≈ 0（桌面本来就长那样）。
#
# ⚠️ 从**屏幕 DC** 抓：踩坑第 12 条说屏幕 BitBlt 抓不到**窗口内容**（硬件叠加平面），
#    但窗口**外面**（桌面/阴影）不在那个平面上，所以对"量投影"这条路有效。
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
Add-Type -TypeDefinition @"
using System; using System.Runtime.InteropServices;
public class MxShadow {
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

$prof = "$env:TEMP\mxrun-shadow-profile"
$env:APPDATA = $prof
$env:MXRUN_KEEP_OPEN = '1'
Get-Process *mxrun* -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 700
$exe = Join-Path (Split-Path -Parent $PSScriptRoot) 'target\release\mxrun.exe'
$p = Start-Process -FilePath $exe -PassThru
Start-Sleep -Seconds 6

$h = [MxShadow]::Best([uint32]$p.Id)
if ($h -eq [IntPtr]::Zero) { throw 'no card window' }
$r = New-Object MxShadow+RECT
[void][MxShadow]::GetWindowRect($h, [ref]$r)
$w = $r.Right - $r.Left; $ht = $r.Bottom - $r.Top
Write-Output ("card rect = ({0},{1})-({2},{3})  {4}x{5}  visible={6}" -f $r.Left, $r.Top, $r.Right, $r.Bottom, $w, $ht, [MxShadow]::IsWindowVisible($h))

$wa = [System.Windows.Forms.Screen]::PrimaryScreen.WorkingArea
$capW = $wa.Right - $wa.Left; $capH = $wa.Bottom - $wa.Top

function Grab {
  $b = New-Object System.Drawing.Bitmap $capW, $capH
  $g = [System.Drawing.Graphics]::FromImage($b)
  $g.CopyFromScreen($wa.Left, $wa.Top, 0, 0, $b.Size)
  $g.Dispose()
  return $b
}

$withCard = Grab
Write-Output "captured #1 (card visible)"
[void][MxShadow]::ShowWindow($h, 0)          # SW_HIDE
Start-Sleep -Milliseconds 900
$without = Grab
Write-Output ("captured #2 (card hidden)  visible={0}" -f [MxShadow]::IsWindowVisible($h))
$r2 = New-Object MxShadow+RECT
[void][MxShadow]::GetWindowRect($h, [ref]$r2)
Write-Output ("rect after hide = ({0},{1})-({2},{3})" -f $r2.Left, $r2.Top, $r2.Right, $r2.Bottom)

$scale = $withCard.Width / [double]$capW
$midY = $r.Top - $wa.Top + [int]($ht / 2)
$band = [int](16 * $scale); if ($band -lt 8) { $band = 8 }
$bandH = [int](40 * $scale)

function Delta-Band($x0) {
  $out = @()
  for ($i = 0; $i -lt $band; $i++) {
    $x = [int]$x0 + $i
    if ($x -lt 0 -or $x -ge $withCard.Width) { $out += 'n/a'; continue }
    $sum = 0.0; $n = 0
    for ($j = 0; $j -lt $bandH; $j++) {
      $y = $midY - [int]($bandH / 2) + $j
      if ($y -lt 0 -or $y -ge $withCard.Height) { continue }
      $a = $withCard.GetPixel($x, $y); $b = $without.GetPixel($x, $y)
      $ga = 0.299 * $a.R + 0.587 * $a.G + 0.114 * $a.B
      $gb = 0.299 * $b.R + 0.587 * $b.G + 0.114 * $b.B
      $sum += ($ga - $gb); $n++
    }
    if ($n -gt 0) { $out += [string][int][Math]::Round($sum / $n) } else { $out += 'n/a' }
  }
  return $out
}

Write-Output ""
Write-Output "delta = luminance(card visible) - luminance(hidden), in grey levels"
Write-Output "        (negative = darker where the card is; a shadow shows as a ramp next to the edge)"
Write-Output ""
$leftX = $r.Left - $wa.Left - $band
$rightX = $r.Right - $wa.Left
$L = Delta-Band $leftX
$R = Delta-Band $rightX
Write-Output ("left  of card, outward->edge ({0}px): {1}" -f $L.Count, ($L -join ' '))
Write-Output ("right of card, edge->outward ({0}px): {1}" -f $R.Count, ($R -join ' '))

$Ln = @($L | Where-Object { $_ -ne 'n/a' } | ForEach-Object { [int]$_ })
$Rn = @($R | Where-Object { $_ -ne 'n/a' } | ForEach-Object { [int]$_ })
$minL = ($Ln | Measure-Object -Minimum).Minimum
$minR = ($Rn | Measure-Object -Minimum).Minimum
$edgeL = [int]$Ln[$Ln.Count - 1]
$edgeR = [int]$Rn[0]
Write-Output ""
Write-Output ("left : min={0}   value at the edge={1}   (a shadow needs the darkest value AT the edge)" -f $minL, $edgeL)
Write-Output ("right: min={0}   value at the edge={1}" -f $minR, $edgeR)

$verdict = if (($edgeL -le -4 -or $edgeR -le -4) -and ($minL -le -3 -or $minR -le -3)) {
  'SHADOW PRESENT: darkening concentrated at the window edge'
} else {
  'NO SHADOW: no edge-localised darkening outside the window'
}
Write-Output "VERDICT -> $verdict"

$pad = [int](50 * $scale)
$cx0 = [Math]::Max(0, $r.Left - $wa.Left - $pad); $cy0 = [Math]::Max(0, $r.Top - $wa.Top - $pad)
$cx1 = [Math]::Min($withCard.Width, $r.Right - $wa.Left + $pad); $cy1 = [Math]::Min($withCard.Height, $r.Bottom - $wa.Top + $pad)
# 不用 Bitmap.Clone(rect, format)：PowerShell 5.1 下它不按预期重载（实测只得到 50x50 一小块），
# 改成显式 DrawImage 重画，尺寸自己说了算。
$cw = $cx1 - $cx0; $ch = $cy1 - $cy0
$around = New-Object System.Drawing.Bitmap $cw, $ch
$ga = [System.Drawing.Graphics]::FromImage($around)
$ga.DrawImage($withCard, (New-Object System.Drawing.Rectangle 0, 0, $cw, $ch), (New-Object System.Drawing.Rectangle $cx0, $cy0, $cw, $ch), [System.Drawing.GraphicsUnit]::Pixel)
$ga.Dispose()
$out = Join-Path $env:TEMP 'mxrun-shadow-around.png'
$around.Save($out, [System.Drawing.Imaging.ImageFormat]::Png)
Write-Output ("saved card+surroundings: $out  ({0}x{1})" -f $around.Width, $around.Height)
$around.Dispose(); $withCard.Dispose(); $without.Dispose()

Get-Process *mxrun* -ErrorAction SilentlyContinue | Stop-Process -Force
Remove-Item Env:MXRUN_KEEP_OPEN -ErrorAction SilentlyContinue

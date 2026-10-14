# 核对行右键菜单的实际渲染：真实鼠标右键打开菜单 + PrintWindow 截图。
#
# 为什么必须真鼠标：egui 的合成事件测不了"右键 → 弹菜单 → 点菜单项"这条链路，
# 而菜单项是**按行动态生成**的（`row_menu`），只有真跑一遍才看得到它到底列了什么。
#
# 截两张：
#   ① 普通条目行（有命令行、有文件）→ 应有 编辑/删除/打开所在目录/文件属性/复制命令行
#   ② `@` 模式的文件行           → 同上（@ 行是瞬时条目，走的是同一套菜单）
#
# 用法：powershell -ExecutionPolicy Bypass -File tools\verify-row-menu.ps1
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
# 内联 C# 里用到 System.Drawing 时必须显式引用它（Add-Type 不会自动带上）
Add-Type -ReferencedAssemblies 'System.Drawing' -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public class MxMenu {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  public delegate bool EnumProc(IntPtr h, IntPtr p);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr p);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern int GetWindowTextLength(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, int dx, int dy, uint d, UIntPtr e);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);

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
  public static void Move(int x, int y) { SetCursorPos(x, y); System.Threading.Thread.Sleep(120); }
  public static void Click(uint down, uint up) {
    mouse_event(down, 0, 0, 0, UIntPtr.Zero); System.Threading.Thread.Sleep(50);
    mouse_event(up, 0, 0, 0, UIntPtr.Zero);
  }
  public static string Shot(IntPtr h, string path) {
    RECT r; if (!GetWindowRect(h, out r)) return "no rect";
    int w = r.Right - r.Left, ht = r.Bottom - r.Top;
    var bmp = new System.Drawing.Bitmap(w, ht);
    var g = System.Drawing.Graphics.FromImage(bmp);
    IntPtr hdc = g.GetHdc();
    bool ok = PrintWindow(h, hdc, 2);
    g.ReleaseHdc(hdc); g.Dispose();
    bmp.Save(path, System.Drawing.Imaging.ImageFormat.Png);
    bmp.Dispose();
    return string.Format("PrintWindow={0} {1}x{2} -> {3}", ok, w, ht, path);
  }
}
"@

$root = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $root 'target\release\mxrun.exe'
if (-not (Test-Path $exe)) { throw "先构建：cargo build --release" }

$profileDir = Join-Path $env:TEMP 'mxrun-menu-profile'
Remove-Item $profileDir -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path (Join-Path $profileDir 'MxRun') | Out-Null
$real = Join-Path $env:APPDATA 'MxRun\mxrun.redb'
if (Test-Path $real) { Copy-Item $real (Join-Path $profileDir 'MxRun\mxrun.redb') -Force }
$env:APPDATA = $profileDir
$env:MXRUN_KEEP_OPEN = '1'          # 别让它因为失焦自隐（截图时必须一直显示）
Remove-Item Env:MXRUN_SELFTEST -ErrorAction SilentlyContinue

Get-Process *mxrun* -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 800
$p = Start-Process -FilePath $exe -PassThru
Start-Sleep -Seconds 6
$h = [MxMenu]::Best([uint32]$p.Id)
if ($h -eq [IntPtr]::Zero) { Get-Process *mxrun* | Stop-Process -Force; throw '没有可见卡片窗口' }
[void][MxMenu]::SetForegroundWindow($h)
Start-Sleep -Milliseconds 400

$r = New-Object MxMenu+RECT
[void][MxMenu]::GetWindowRect($h, [ref]$r)
Write-Host "窗口 ($($r.Left),$($r.Top))-($($r.Right),$($r.Bottom))"

$out1 = Join-Path $env:TEMP 'mxrun-menu-item.png'
$out2 = Join-Path $env:TEMP 'mxrun-menu-filerow.png'

try {
  # ---- ① 普通条目行（第 1 行的文字区域） ----
  [MxMenu]::Move($r.Left + 260, $r.Top + 103)
  [MxMenu]::Click(0x0008, 0x0010)          # RIGHTDOWN / RIGHTUP
  Start-Sleep -Milliseconds 800
  Write-Host ([MxMenu]::Shot($h, $out1))
  [MxMenu]::Click(0x0002, 0x0004)          # 左键点一下关掉菜单
  Start-Sleep -Milliseconds 300

  # ---- ② `@` 模式的文件行 ----
  # 用剪贴板粘贴而不是逐字敲：不经过输入法，也不会因为"按了 '@' 却触发裸数字"之类走偏
  Set-Clipboard -Value '@ever'
  [MxMenu]::Move($r.Left + 260, $r.Top + 62)
  [MxMenu]::Click(0x0002, 0x0004)          # 真点一下搜索框（焦点）
  Start-Sleep -Milliseconds 300
  Add-Type -AssemblyName System.Windows.Forms
  [System.Windows.Forms.SendKeys]::SendWait('^v')
  Start-Sleep -Seconds 3
  [MxMenu]::Move($r.Left + 260, $r.Top + 103)
  [MxMenu]::Click(0x0008, 0x0010)
  Start-Sleep -Milliseconds 900
  Write-Host ([MxMenu]::Shot($h, $out2))
} finally {
  Start-Sleep -Milliseconds 300
  Get-Process *mxrun* -ErrorAction SilentlyContinue | Stop-Process -Force
  Remove-Item Env:MXRUN_KEEP_OPEN -ErrorAction SilentlyContinue
}
Write-Host "`n两张图：`n  $out1`n  $out2"

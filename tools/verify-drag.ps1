# MxRun 拖拽验证：用真实鼠标输入拖窗口，然后核对窗口有没有跟着走。
#
# 为什么需要它：egui 的窗口拖动是把窗口交给系统自己的移动循环
# （`SendMessageW(WM_NCLBUTTONDOWN, HTCAPTION)`，见 src/main.rs 的 `drag_window`），
# 那个调用会**阻塞到真实鼠标松开**——所以 egui 的合成事件测不了（会把进程卡死），
# 必须用 `mouse_event`/`SetCursorPos` 发**真实输入**，系统的移动循环才会正常结束。
#
# 用法（在仓库根目录）：
#   powershell -ExecutionPolicy Bypass -File tools\verify-drag.ps1
#
# 会在临时目录里用**独立档案**跑，不碰你的真实数据；测试完自动清理。

$ErrorActionPreference = 'Stop'
$exe = Join-Path (Split-Path -Parent $PSScriptRoot) 'target\release\mxrun.exe'
if (-not (Test-Path $exe)) { throw "先构建：cargo build --release（找不到 $exe）" }

Add-Type -TypeDefinition @"
using System; using System.Runtime.InteropServices;
public class MxDrag {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, UIntPtr e);
  public static void Drag(int x1, int y1, int x2, int y2, int steps) {
    SetCursorPos(x1, y1); System.Threading.Thread.Sleep(150);
    mouse_event(0x0002, 0, 0, 0, UIntPtr.Zero);          // LEFTDOWN
    System.Threading.Thread.Sleep(220);
    for (int i = 1; i <= steps; i++) {
      SetCursorPos(x1 + (x2 - x1) * i / steps, y1 + (y2 - y1) * i / steps);
      System.Threading.Thread.Sleep(70);
    }
    System.Threading.Thread.Sleep(150);
    mouse_event(0x0004, 0, 0, 0, UIntPtr.Zero);          // LEFTUP
  }
}
"@

$profileDir = Join-Path $env:TEMP 'mxrun-verify-drag'
Remove-Item $profileDir -Recurse -Force -ErrorAction SilentlyContinue
$env:APPDATA = $profileDir
$env:MXRUN_KEEP_OPEN = '1'          # 不失焦隐藏，否则窗口会自己躲起来
Get-Process *mxrun* -ErrorAction SilentlyContinue | Stop-Process -Force
$cursor = New-Object MxDrag+POINT
[void][MxDrag]::GetCursorPos([ref]$cursor)   # 用完把光标还回原处

function Get-Rect([IntPtr]$h) { $r = New-Object MxDrag+RECT; [void][MxDrag]::GetWindowRect($h, [ref]$r); $r }

try {
  Start-Process -FilePath $exe | Out-Null
  Start-Sleep -Seconds 5
  $p = Get-Process *mxrun* | Select-Object -First 1
  $h = $p.MainWindowHandle
  $r = Get-Rect $h
  Write-Host "窗口起始 = $($r.Left),$($r.Top)"

  # 连续三次拖动，覆盖卡片上三个不同的抓取点。
  # **必须连着拖**：修好之前是"隔一次失败"——模态移动循环吞掉了鼠标松开事件，
  # egui 一直以为指针还按着，于是下一次拖动被忽略。所以"连续三次都成功"
  # 正是那条回归的测试。
  $spots = @(@(200, 22, '标题条'), @(150, 220, '列表行'), @(300, 370, '状态栏'))
  $pass = 0
  foreach ($s in $spots) {
    # 机器忙的时候（比如刚跑完 cargo build）一次拖动可能来不及被系统处理，
    # 于是失败一次就重试一次——**偶发失败的自检比没有自检更糟**：它会让人
    # 开始忽略红色输出。只有重试也失败才算真失败。
    $moved = $false
    $note = ''
    for ($attempt = 1; $attempt -le 2 -and -not $moved; $attempt++) {
      [MxDrag]::Drag($r.Left + $s[0], $r.Top + $s[1], $r.Left + $s[0] + 40, $r.Top + $s[1] + 20, 5)
      Start-Sleep -Milliseconds 800
      $r2 = Get-Rect $h
      $moved = ($r2.Left -ne $r.Left) -or ($r2.Top -ne $r.Top)
      if ($moved) {
        $note = if ($attempt -eq 1) { '' } else { '（重试后成功）' }
      } elseif ($attempt -eq 1) {
        Start-Sleep -Milliseconds 600   # 让机器喘口气再试
      }
    }
    if ($moved) { $pass++ }
    Write-Host ("  {0} 拖{1}: 位移 {2},{3} {4}" -f $(if ($moved) { '[OK]  ' } else { '[FAIL]' }), $s[2], ($r2.Left - $r.Left), ($r2.Top - $r.Top), $note)
    $r = $r2
  }

  $saved = @(Select-String -Path (Join-Path $profileDir 'MxRun\mxrun.log') -Pattern 'window: position saved' -Encoding UTF8)
  Write-Host "日志里的位置存档: $($saved.Count) 条"
  $saved | Select-Object -Last 3 | ForEach-Object { Write-Host "   $($_.Line)" }

  $ok = ($pass -eq $spots.Count) -and ($saved.Count -ge $spots.Count)
  Write-Host $(if ($ok) { "`n结果: 通过 —— 连续 $pass 次拖动都生效" } else { "`n结果: 失败 —— 通过 $pass / $($spots.Count)" })
  if (-not $ok) { exit 1 }
}
finally {
  [void][MxDrag]::SetCursorPos($cursor.X, $cursor.Y)
  Get-Process *mxrun* -ErrorAction SilentlyContinue | Stop-Process -Force
  Remove-Item Env:MXRUN_KEEP_OPEN -ErrorAction SilentlyContinue
  Remove-Item $profileDir -Recurse -Force -ErrorAction SilentlyContinue
}

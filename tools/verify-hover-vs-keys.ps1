# MxRun 交互验证：鼠标停在候选区时，键盘上下键还灵不灵。
#
# 背景：列表曾经"悬停即选中"，鼠标只要停在候选区，↑↓ 每动一次就被鼠标抢回选中项，
# 键盘看起来就是坏的（用户 2026-09-19 报告）。原版 AltRun 的列表**根本没有鼠标移动处理**：
# 选中只由键盘、左键点击、右键（先模拟左键选中再弹菜单）改变。
#
# 这个脚本用真实鼠标/键盘输入复现那个场景：
#   点一下第 1 行（选中它，鼠标就停在那一行不动）→ 按 ↓ ↓ → 按回车
#   期望执行**第 3 行**。若悬停仍在抢选中，回车上执行的是第 1 行。
#
# 用法（仓库根目录）：
#   powershell -ExecutionPolicy Bypass -File tools\verify-hover-vs-keys.ps1

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $root 'target\release\mxrun.exe'
if (-not (Test-Path $exe)) { throw "先构建：cargo build --release（找不到 $exe）" }

Add-Type -TypeDefinition @"
using System; using System.Runtime.InteropServices;
public class MxInput {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, UIntPtr e);
  [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
  public static void Click(int x, int y) {
    SetCursorPos(x, y); System.Threading.Thread.Sleep(150);
    mouse_event(0x0002, 0, 0, 0, UIntPtr.Zero); System.Threading.Thread.Sleep(80);
    mouse_event(0x0004, 0, 0, 0, UIntPtr.Zero);
  }
  public static void Key(byte vk) {
    keybd_event(vk, 0, 0, UIntPtr.Zero); System.Threading.Thread.Sleep(60);
    keybd_event(vk, 0, 2, UIntPtr.Zero); System.Threading.Thread.Sleep(120);
  }
}
"@

$profileDir = Join-Path $env:TEMP 'mxrun-verify-hover'
$listPath = Join-Path $env:TEMP 'mxrun-verify-hover-list.txt'
Remove-Item $profileDir -Recurse -Force -ErrorAction SilentlyContinue
# 四条互不相干的条目，命令只写一个临时文件（安全、可判定）
$lines = @()
foreach ($n in 1..4) {
  $letter = [char](64 + $n)                       # A B C D
  $lines += ("F5       ||item$n                        |Item $letter                     |cmd /c echo $letter > `"%TEMP%\mxrun-hover-$letter.txt`"")
}
# 导入器要 ANSI/UTF-8 都能读的纯 ASCII 内容，这里全 ASCII
[System.IO.File]::WriteAllLines($listPath, $lines, (New-Object System.Text.UTF8Encoding($false)))

$env:APPDATA = $profileDir
$env:ALTRUN_LIST = $null
$env:MXRUN_ALTRUN_LIST = $listPath
Get-Process *mxrun* -ErrorAction SilentlyContinue | Stop-Process -Force
Remove-Item "$env:TEMP\mxrun-hover-*.txt" -Force -ErrorAction SilentlyContinue

$cursor = New-Object MxInput+POINT
[void][MxInput]::GetCursorPos([ref]$cursor)

try {
  Start-Process -FilePath $exe -ArgumentList '--import' | Out-Null
  Start-Sleep -Seconds 3
  Get-Process *mxrun* -ErrorAction SilentlyContinue | Stop-Process -Force
  Remove-Item Env:MXRUN_ALTRUN_LIST

  $env:MXRUN_KEEP_OPEN = '1'
  Start-Process -FilePath $exe | Out-Null
  Start-Sleep -Seconds 5
  $p = Get-Process *mxrun* | Select-Object -First 1
  $h = $p.MainWindowHandle
  $r = New-Object MxInput+RECT
  [void][MxInput]::GetWindowRect($h, [ref]$r)

  # 第 1 行大概在 +125，第 3 行大概在 +225（行高约 50）
  Write-Host "点击第 1 行（鼠标随后就停在那一行）"
  [MxInput]::Click($r.Left + 200, $r.Top + 125)
  Start-Sleep -Milliseconds 400
  Write-Host "按 ↓ ↓（期望选中移到第 3 行 = Item C）"
  [MxInput]::Key(0x28); [MxInput]::Key(0x28)
  Start-Sleep -Milliseconds 300
  Write-Host "按回车"
  [MxInput]::Key(0x0D)
  Start-Sleep -Seconds 2

  $made = @(Get-ChildItem $env:TEMP -Filter 'mxrun-hover-*.txt' -ErrorAction SilentlyContinue |
            Select-Object -ExpandProperty Name | Sort-Object)
  Write-Host "执行结果写出的文件: $(if ($made) { $made -join ', ' } else { '(无)' })"

  $ok = ($made.Count -eq 1) -and ($made[0] -eq 'mxrun-hover-C.txt')
  Write-Host $(if ($ok) { "`n结果: 通过 —— 鼠标停在第 1 行上时，↓↓ 仍然把选中带到了第 3 行" }
               else { "`n结果: 失败 —— 期望只有 mxrun-hover-C.txt（第 3 行），实际 $($made -join ', ')" })
  if (-not $ok) { exit 1 }
}
finally {
  [void][MxInput]::SetCursorPos($cursor.X, $cursor.Y)
  Get-Process *mxrun* -ErrorAction SilentlyContinue | Stop-Process -Force
  Remove-Item Env:MXRUN_KEEP_OPEN -ErrorAction SilentlyContinue
  Remove-Item $profileDir -Recurse -Force -ErrorAction SilentlyContinue
  Remove-Item $listPath -Force -ErrorAction SilentlyContinue
  Remove-Item "$env:TEMP\mxrun-hover-*.txt" -Force -ErrorAction SilentlyContinue
}

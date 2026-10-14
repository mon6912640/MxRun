# 复现/验证：`@` 模式打字快时状态栏卡在「搜索中…」。
#
# 机理（读代码 + 实测）：搜索在飞时又敲了字符 → 答案回来发现「答案的查询 != 当前输入」
# → 结果被丢弃，但**没有人给"当前输入"重新起防抖**，于是 es.exe 再也没被调用，
# 状态栏一直「搜索中…」，直到再敲/删一个字符才恢复。
#
# 输入法（IME）排除：本机是 zh-Hans-CN + 微软拼音，`keybd_event` 的真实按键**会先过 IME**
# （2026-10-13 实测打出过 `token'look` 这种多余字符，那种"证据"不能用）。
# 所以这里用 **SendInput + KEYEVENTF_UNICODE** 注入 Unicode 字符：这条路径不经过 IME，
# 注入什么就是什么。代价是它验的是"文本进入 → 触发搜索"的时序，不是键盘映射本身
# （键盘映射另有 tools\verify-shift-digit.ps1 等脚本负责）。
#
# 判据（不靠截图，靠日志）：
#   ① `files: sending "X"` —— 最终查询有没有被**发出去**
#   ② `files: "X" -> N hits` —— 最终查询有没有被**应用**（这个只有应用时才写）
#   ③ 落进输入框的查询必须**逐字等于**我们注入的（否则就是输入法/焦点在捣乱，本次不算数）
#
# 用法：
#   powershell -ExecutionPolicy Bypass -File tools\verify-fast-typing-status.ps1 [-GapMs 70] [-Exe <path>]

param(
  [int]$GapMs = 70,
  [string]$Exe = ""
)

$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public class MxType2 {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  [StructLayout(LayoutKind.Sequential)] public struct KEYBDINPUT {
    public ushort wVk; public ushort wScan; public uint dwFlags; public uint time; public IntPtr dwExtraInfo;
  }
  [StructLayout(LayoutKind.Sequential)] public struct INPUT {
    public uint type; public KEYBDINPUT ki; public int pad1; public int pad2;
  }
  [DllImport("user32.dll")] public static extern uint SendInput(uint n, INPUT[] inputs, int size);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, int dx, int dy, uint d, UIntPtr e);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  const uint KEYEVENTF_UNICODE = 0x0004, KEYEVENTF_KEYUP = 0x0002;

  /// 注入一个 Unicode 字符：不经过 IME（wVk=0，只给 wScan 一个 UTF-16 码元）
  public static void UnicodeChar(char c) {
    INPUT[] a = new INPUT[2];
    a[0].type = 1; a[0].ki.wVk = 0; a[0].ki.wScan = (ushort)c; a[0].ki.dwFlags = KEYEVENTF_UNICODE;
    a[1].type = 1; a[1].ki.wVk = 0; a[1].ki.wScan = (ushort)c; a[1].ki.dwFlags = KEYEVENTF_UNICODE | KEYEVENTF_KEYUP;
    SendInput(2, a, Marshal.SizeOf(typeof(INPUT)));
  }
  public static void Click(int x, int y) {
    SetCursorPos(x, y); System.Threading.Thread.Sleep(120);
    mouse_event(0x0002, 0, 0, 0, UIntPtr.Zero); System.Threading.Thread.Sleep(40);
    mouse_event(0x0004, 0, 0, 0, UIntPtr.Zero);
  }
}
"@

$root = Split-Path -Parent $PSScriptRoot
if ($Exe -eq '') { $Exe = Join-Path $root 'target\release\mxrun.exe' }
if (-not (Test-Path $Exe)) { throw "找不到 exe：$Exe" }
Write-Host "被测程序: $Exe  （构建时间 $((Get-Item $Exe).LastWriteTime)）"

# 便携模式的 exe 把数据写到 **exe 旁边的 data\**，根本不理 %APPDATA%
# （2026-10-13 实测：拿便携版当被测程序时读不到日志，因为它写在别处）。
$exeDir = Split-Path $Exe -Parent
if (Test-Path (Join-Path $exeDir 'portable.txt')) {
  $log = Join-Path $exeDir 'data\mxrun.log'
  Write-Host "便携模式：日志 = $log（不碰 %APPDATA%）"
} else {
  $profileDir = Join-Path $env:TEMP 'mxrun-fasttype-profile'
  Remove-Item $profileDir -Recurse -Force -ErrorAction SilentlyContinue
  New-Item -ItemType Directory -Force -Path (Join-Path $profileDir 'MxRun') | Out-Null
  $real = Join-Path $env:APPDATA 'MxRun\mxrun.redb'
  if (Test-Path $real) { Copy-Item $real (Join-Path $profileDir 'MxRun\mxrun.redb') -Force }
  $env:APPDATA = $profileDir
  $log = Join-Path $profileDir 'MxRun\mxrun.log'
}

Remove-Item Env:MXRUN_SELFTEST -ErrorAction SilentlyContinue
Get-Process *mxrun* -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 800
$p = Start-Process -FilePath $Exe -PassThru
Start-Sleep -Seconds 6

Remove-Item $log -Force -ErrorAction SilentlyContinue
$query = 'tokenlook'          # 已知能搜到东西（用户日志里有 8 hits）

try {
  $r = New-Object MxType2+RECT
  [void][MxType2]::GetWindowRect($p.MainWindowHandle, [ref]$r)
  [MxType2]::Click(($r.Left + 160), ($r.Top + 62))     # 真点一下卡片顶部：鼠标按位置派发，不依赖前台
  Start-Sleep -Milliseconds 500

  [MxType2]::UnicodeChar('@')
  Start-Sleep -Milliseconds 250
  $typed = '@'
  foreach ($ch in $query.ToCharArray()) {
    [MxType2]::UnicodeChar($ch)
    $typed += $ch
    Start-Sleep -Milliseconds $GapMs
  }
  Write-Host "已注入 $typed（间隔 $GapMs ms，低于 120ms 防抖），等 4 秒让搜索落地…"
  Start-Sleep -Seconds 4

  $live = Get-Process *mxrun* -ErrorAction SilentlyContinue | Select-Object -First 1
  if ($live) { $live.Refresh(); Write-Host "窗口可见=$([MxType2]::IsWindowVisible($live.MainWindowHandle))" }
} finally {
  Start-Sleep -Milliseconds 300
  Get-Process *mxrun* -ErrorAction SilentlyContinue | Stop-Process -Force
}

if (-not (Test-Path $log)) { Write-Host "`n结果: 不确定 —— 没有日志"; exit 2 }
$all = @([System.IO.File]::ReadAllLines($log, [Text.Encoding]::UTF8))
$files = @($all | Where-Object { $_ -match 'files: ' })
Write-Host "`n--- 本次运行的 files: / status: 记录 ---"
@($all | Where-Object { $_ -match 'files: |status: ' }) | ForEach-Object { Write-Host "   $_" }

# 输入法/焦点校验：落进输入框的查询必须是**我们注入文本的前缀**。
# 注意：中间查询也被应用是正常行为（间隔久了每次按键都会真的搜一次），判据只看**最后一个**。
$seen = @()
foreach ($l in $files) { if ($l -match 'files: "([^"]*)"') { $seen += $Matches[1] } }
if ($seen.Count -eq 0) {
  Write-Host "`n结果: 不确定 —— 没有任何查询记录（按键/文本没送达）"
  exit 2
}
$bad = @($seen | Where-Object { -not $query.StartsWith($_) })
if ($bad.Count -gt 0) {
  Write-Host "`n结果: 不确定 —— 日志里的查询不是注入文本的前缀（输入法/焦点在捣乱）：$($bad -join ', ')"
  exit 2
}

$sent    = @($files | Where-Object { $_ -match "files: sending `"$([regex]::Escape($query))`"" }).Count
$applied = @($files | Where-Object { $_ -match "files: `"$([regex]::Escape($query))`" -> " }).Count
Write-Host "`n最终查询 [$query]：发送 $sent 次，应用 $applied 次   （中间查询 $($seen.Count) 个）"
if ($sent -ge 1 -and $applied -ge 1) {
  Write-Host "结果: 通过 —— 最后一次输入确实被问了 Everything 并落了结果"
  exit 0
} else {
  Write-Host "结果: 失败 —— 输入停在 [$query]，但发送=$sent 应用=$applied（这就是「一直显示搜索中」）"
  exit 1
}

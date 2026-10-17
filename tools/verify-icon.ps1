# 校验 exe 里的图标资源真的存在、且各个尺寸都能渲染出来。
#
# 为什么需要它：`build.rs` 只是把 `resources/mxrun.rc` 交给 windres，失败时只发一条
# `cargo:warning`（不阻塞构建）。于是"图标没进 exe"这件事有可能是静默的——本脚本把
# 它变回硬失败：
#
#   1. icon/mxrun.ico 里应该有 16/20/24/32/40/48/64/96/128/256 十个尺寸；
#   2. exe 里恰好有 1 个图标组（ExtractIconExW 的返回值）；
#   3. 用 LoadLibraryEx(LOAD_LIBRARY_AS_DATAFILE) + LoadImageW 按 16/32/48/256 取
#      图标都能成功、尺寸正确、且不是纯色块（图案确实渲染出来了）；
#   4. 传 -Png 时把这几张导成一张对照图，供人眼核对。
#
# 用法：tools\verify-icon.ps1 [-Exe target\release\mxrun.exe] [-Ico icon\mxrun.ico] [-Png out.png]
param(
    [string]$Exe = "target\release\mxrun.exe",
    [string]$Ico = "icon\mxrun.ico",
    [string]$Png = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
Push-Location $root

Add-Type -AssemblyName System.Drawing
if (-not ("MxIconProbe" -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class MxIconProbe {
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern IntPtr LoadLibraryExW(string path, IntPtr file, uint flags);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool FreeLibrary(IntPtr module);
    [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern IntPtr LoadImageW(IntPtr inst, IntPtr name, uint type, int cx, int cy, uint load);
    [DllImport("user32.dll", SetLastError = true)]
    public static extern bool DestroyIcon(IntPtr icon);
    [DllImport("shell32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern uint ExtractIconExW(string file, int index, IntPtr[] large, IntPtr[] small, uint count);
}
'@
}

$failures = New-Object System.Collections.Generic.List[string]
function Bad($msg) { $script:failures.Add($msg); Write-Host "  FAIL  $msg" -ForegroundColor Red }
function Okay($msg) { Write-Host "  ok    $msg" -ForegroundColor Green }

# --- 1) .ico 的尺寸表 -------------------------------------------------------
Write-Host "`n[1] $Ico"
$ico = (Resolve-Path $Ico).Path
$bytes = [IO.File]::ReadAllBytes($ico)
$count = [BitConverter]::ToUInt16($bytes, 4)
$sizes = @()
for ($i = 0; $i -lt $count; $i++) {
    $o = 6 + $i * 16
    $w = [int]$bytes[$o]
    if ($w -eq 0) { $w = 256 }
    $sizes += $w
}
$want = @(16, 20, 24, 32, 40, 48, 64, 96, 128, 256)
if (($sizes -join ',') -eq ($want -join ',')) {
    Okay "尺寸齐全：$($sizes -join ', ')"
} else {
    Bad "尺寸表不对：得到 $($sizes -join ','), 期望 $($want -join ',')"
}

# --- 2) exe 里有几个图标组 --------------------------------------------------
Write-Host "`n[2] $Exe"
$exe = (Resolve-Path $Exe).Path
$groups = [MxIconProbe]::ExtractIconExW($exe, -1, $null, $null, 0)
if ($groups -eq 1) { Okay "图标组数量：1" } else { Bad "图标组数量：$groups（期望 1）" }

# --- 3) 逐尺寸加载渲染 ------------------------------------------------------
$mod = [MxIconProbe]::LoadLibraryExW($exe, [IntPtr]::Zero, 0x2)   # LOAD_LIBRARY_AS_DATAFILE
if ($mod -eq [IntPtr]::Zero) {
    Write-Host "LoadLibraryExW 失败，无法读取 exe 资源" -ForegroundColor Red
    Pop-Location
    exit 1
}

$rendered = @{}
foreach ($size in 16, 32, 48, 256) {
    $h = [MxIconProbe]::LoadImageW($mod, [IntPtr]1, 1, $size, $size, 0)   # IMAGE_ICON
    if ($h -eq [IntPtr]::Zero) {
        Bad "${size}px 加载失败（资源 1 不是图标组？）"
        continue
    }
    $icon = [System.Drawing.Icon]::FromHandle($h)
    $bmp = $icon.ToBitmap()
    $rendered[$size] = $bmp
    if ($bmp.Width -ne $size -or $bmp.Height -ne $size) {
        Bad "${size}px 尺寸不对：$($bmp.Width)x$($bmp.Height)"
    } else {
        $colors = @{}
        for ($y = 0; $y -lt $bmp.Height; $y += 2) {
            for ($x = 0; $x -lt $bmp.Width; $x += 2) {
                $colors[$bmp.GetPixel($x, $y).ToArgb()] = 1
            }
        }
        if ($colors.Count -ge 8) {
            Okay "${size}px 渲染成功（$($colors.Count) 种颜色）"
        } else {
            Bad "${size}px 渲染成纯色块（只有 $($colors.Count) 种颜色）"
        }
    }
    [void][MxIconProbe]::DestroyIcon($h)
}
[void][MxIconProbe]::FreeLibrary($mod)

# --- 4) 人眼对照图 ----------------------------------------------------------
if ($Png -ne "") {
    $pad = 14
    $width = 12
    foreach ($s in 16, 32, 48, 256) { if ($rendered.ContainsKey($s)) { $width += $s + $pad } }
    $sheet = New-Object System.Drawing.Bitmap $width, 300
    $g = [System.Drawing.Graphics]::FromImage($sheet)
    $g.Clear([System.Drawing.Color]::FromArgb(245, 245, 245))
    $x = 6
    $font = New-Object System.Drawing.Font("Consolas", 9)
    foreach ($s in 16, 32, 48, 256) {
        if (-not $rendered.ContainsKey($s)) { continue }
        $bmp = $rendered[$s]
        $g.DrawImage($bmp, $x, 10, $bmp.Width, $bmp.Height)
        $g.DrawString("${s}px", $font, [System.Drawing.Brushes]::Red, $x, 10 + $bmp.Height + 4)
        $x += $s + $pad
    }
    $g.Dispose()
    $out = $Png
    if (-not [IO.Path]::IsPathRooted($out)) { $out = Join-Path (Get-Location).Path $out }
    $sheet.Save($out, [System.Drawing.Imaging.ImageFormat]::Png)
    $sheet.Dispose()
    Write-Host "  对照图：$out"
}

Pop-Location
Write-Host ""
if ($failures.Count -gt 0) {
    Write-Host "图标校验失败（$($failures.Count) 项）" -ForegroundColor Red
    exit 1
}
Write-Host "图标校验通过" -ForegroundColor Green

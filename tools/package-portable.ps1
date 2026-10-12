# 打一个可以直接发给朋友的便携包。
#
# 产物：dist\MxRun-v<版本>-portable.zip
#   内容（解压即用，数据跟着文件夹走）：
#     mxrun.exe        主程序
#     portable.txt     声明便携模式 → 数据写到同目录的 data\ 里（删除它则用 %APPDATA%）
#     上手说明.txt      给朋友看的三步 + 常见问题
#     LICENSE          许可证
#     版本.txt         版本与构建时间（"我到底在跑哪一版"这个问题靠它回答）
#
# 用法：
#   powershell -ExecutionPolicy Bypass -File tools\package-portable.ps1
#   powershell -ExecutionPolicy Bypass -File tools\package-portable.ps1 -SkipBuild

param(
  [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$cargo = "$env:USERPROFILE\.cargo\bin\cargo.exe"
if (-not (Test-Path $cargo)) { $cargo = 'cargo' }
$env:PATH = "D:\work\mingw64\bin;$env:USERPROFILE\.cargo\bin;$env:PATH"

if (-not $SkipBuild) {
  Write-Host '先跑测试，再构建 release…' -ForegroundColor Cyan
  Get-Process *mxrun* -ErrorAction SilentlyContinue | Stop-Process -Force   # exe 被占用会 os error 5
  & $cargo test 2>&1 | Select-String -Pattern 'test result:' | ForEach-Object { Write-Host "   $_" }
  if ($LASTEXITCODE -ne 0) { throw '测试没全过，不打包（先修好再发）' }
  & $cargo build --release
  if ($LASTEXITCODE -ne 0) { throw '构建失败' }
}

$exe = Join-Path $root 'target\release\mxrun.exe'
if (-not (Test-Path $exe)) { throw "找不到 $exe" }

# 版本号从 Cargo.toml 读，别手写（手写迟早和代码不一致）
$version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
$built = (Get-Item $exe).LastWriteTime.ToString('yyyy-MM-dd HH:mm')
$sizeMB = [math]::Round((Get-Item $exe).Length / 1MB, 2)

$stage = Join-Path $root "dist\MxRun-v$version"
$zip = Join-Path $root "dist\MxRun-v$version-portable.zip"
Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item $zip -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $stage | Out-Null

Copy-Item $exe (Join-Path $stage 'mxrun.exe')
# 便携模式声明文件：内容无关紧要，存在即可
Set-Content -Path (Join-Path $stage 'portable.txt') -Value '便携模式：数据保存在同目录的 data\ 文件夹里' -Encoding UTF8
if (Test-Path (Join-Path $root 'LICENSE')) { Copy-Item (Join-Path $root 'LICENSE') $stage }

Set-Content -Path (Join-Path $stage '版本.txt') -Encoding UTF8 -Value @"
MxRun v$version
构建时间：$built
可执行文件大小：$sizeMB MB
打包时间：$(Get-Date -Format 'yyyy-MM-dd HH:mm')

程序启动后，日志里也会写一行 "startup: exe built $built"，
设置页顶部显示同一个时间 —— 用来确认"我现在跑的是哪一版"。
"@

Set-Content -Path (Join-Path $stage '上手说明.txt') -Encoding UTF8 -Value @"
MxRun v$version —— 三步开始用
=====================================

1) 把整个文件夹放到一个**固定**位置，例如 D:\Tools\MxRun
   （数据就存在这个文件夹里；之后别随意挪动 —— 挪了也能用，
     但右键菜单和开机自启会在下次启动时自动改写到新路径）

2) 双击 mxrun.exe。窗口出来后按 Esc 隐藏，之后用 Alt+F1 呼出。
   右下角托盘图标右键 → 设置 / 快捷项管理 / 退出。

3) 想让它开机自启：托盘 → 设置 → 开机自启（写在 HKCU，不需要管理员）。


常用操作
-------------------------------------
Alt+F1          呼出 / 隐藏
打字            搜索（支持拼音、首字母）
Enter / 空格     执行选中的那一项
Ctrl+数字       直接执行第 N 项（空框时单个数字也行）
#3              先打字筛出列表，再用 #3 选第 3 项
=100/8          计算器模式
@报告           搜索整机文件（需要 Everything 的 es.exe，见下）
Esc             清空 / 隐藏
F2              编辑选中项；Delete 两次删除
Ctrl+L          最近用过的列表


@ 文件搜索要装 Everything
-------------------------------------
@ 模式调用 Everything 的命令行工具 es.exe。请自行安装：
  1) 装 Everything（voidtools.com）
  2) 下载它的命令行工具 es.exe，放到 PATH 里，
     或在 MxRun 设置 → 文件搜索 里指定 es.exe 的路径
没装也能用，只是 @ 模式会提示"找不到 es.exe"。


常见问题
-------------------------------------
Q: 第一次打开被 Windows 拦了（"Windows 已保护你的电脑"）？
A: 这是未签名程序的正常提示。点"更多信息" → "仍要运行"。
   杀软误报也一样，加信任即可。

Q: 数据在哪？
A: 便携模式（本包默认）= 程序旁边的 data\ 文件夹。
   删掉 portable.txt 就用 %APPDATA%\MxRun（那种模式数据不跟着走）。
   两种模式别混用，否则会有两份数据。

Q: 怎么彻底卸载？
A: 托盘 → 设置里关掉"发送到菜单 / 右键菜单 / 开机自启"，
   再删掉整个文件夹即可（没有安装程序，注册表里只留上面三项，都已关掉）。

Q: 怎么升级？
A: 直接替换 mxrun.exe（或解压新版覆盖）。数据库会自动迁移，
   每次启动还会自动备份最近 10 份到 data\backups\。
"@

Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip -CompressionLevel Optimal

$hash = (Get-FileHash $zip -Algorithm SHA256).Hash
Set-Content -Path "$zip.sha256" -Encoding ASCII -Value "$hash  $(Split-Path $zip -Leaf)"

Write-Host ''
Write-Host '打包完成：' -ForegroundColor Green
Get-ChildItem $stage | ForEach-Object { Write-Host ("   {0,-16} {1,8:N1} KB" -f $_.Name, ($_.Length / 1KB)) }
Write-Host ''
Write-Host "   → $zip  ($([math]::Round((Get-Item $zip).Length / 1MB, 2)) MB)"
Write-Host "   → SHA256: $hash"
Write-Host ''
Write-Host '发给朋友：把 zip 给他，让他解压到固定目录后双击 mxrun.exe 即可。' -ForegroundColor Cyan

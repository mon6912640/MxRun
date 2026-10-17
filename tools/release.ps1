# 一条命令发版：改版本号 -> 测试/构建/打包 -> 写发布说明 -> 提交 -> 打标签 -> 推送 -> 建 GitHub Release
#
# 用法（在哪个目录跑都行，脚本自己定位仓库根）：
#   powershell -ExecutionPolicy Bypass -File tools\release.ps1                # 补丁号 +1（0.1.6 -> 0.1.7），先问一句
#   powershell -ExecutionPolicy Bypass -File tools\release.ps1 -Minor         # 次版本号 +1（0.1.6 -> 0.2.0）
#   powershell -ExecutionPolicy Bypass -File tools\release.ps1 -Version 0.1.7 # 指定版本
#   powershell -ExecutionPolicy Bypass -File tools\release.ps1 -DryRun        # 只打印计划，不动仓库、不打包、不关进程
#   ... 再加 -Yes 跳过确认
#
# 前置条件（脚本逐条检查，不满足就停）：
#   * 当前在 master 且工作区干净（发版必须从"已提交的状态"出发）
#   * 上一次打标签之后**有**新提交（没东西可发就别发）
#   * gh（GitHub CLI）已安装并 `gh auth login` 过 —— 用来建 Release 并上传 zip
#
# 设计取舍：
#   * 打包复用 tools\package-portable.ps1（测试/构建/图标校验的唯一入口），不在这里重写一套
#   * 发布说明自动写进 docs\releases\v<版本>.md，并作为发布提交的正文 —— 一处内容两处用
#   * **所有原生命令都走 Invoke-Native**：PowerShell 5.1 在 `$ErrorActionPreference='Stop'`
#     下会把子进程写到 stderr 的每一行当致命错误（git push 正常也会往 stderr 写 "To https://…"），
#     于是 `git push 2>&1 | …` 会在"已提交、已打标签"之后把脚本打断 —— 打包脚本踩过同一个坑
#   * 每个不可逆动作失败时都说清"做到哪一步、剩下怎么补"
param(
    [string]$Version,
    [switch]$Minor,
    [switch]$Yes,
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root

function Step($m) { Write-Host "`n== $m" -ForegroundColor Cyan }
function Info($m) { Write-Host "   $m" }
function Die($m) {
    Write-Host "`n✗ $m" -ForegroundColor Red
    Pop-Location
    exit 1
}

# 跑一个原生命令，绝不让它的 stderr 变成 PowerShell 的终止错误。
# 返回 { Code, Text }；-ShowLast 时把最后两行输出打出来（进度感）。
function Invoke-Native {
    param(
        [Parameter(Mandatory = $true)][string]$Exe,
        [string[]]$ArgList = @(),
        [switch]$ShowLast
    )
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $text = (& $Exe @ArgList 2>&1 | Out-String)
        $code = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $prev
    }
    if ($ShowLast -and $text.Trim()) {
        $text.Trim() -split "`r?`n" | Where-Object { $_.Trim() } |
            Select-Object -Last 2 | ForEach-Object { Info $_.Trim() }
    }
    [pscustomobject]@{ Code = $code; Text = $text }
}

try {
    # ---------- 0) 找到 gh ----------
    $gh = (Get-Command gh -ErrorAction SilentlyContinue | Select-Object -First 1).Source
    if (-not $gh) {
        # PATH 还是安装之前的时候（已开着的终端/服务进程），按默认安装位置再找一次
        $candidate = Join-Path $env:ProgramFiles 'GitHub CLI\gh.exe'
        if (Test-Path $candidate) { $gh = $candidate }
    }
    if (-not $gh) { Die '找不到 gh（GitHub CLI）。先装一个：winget install --id GitHub.cli -e' }

    # ---------- 1) 前置检查 ----------
    Step '前置检查'

    $st = Invoke-Native 'git' @('status', '--porcelain')
    if ($st.Text.Trim()) {
        $st.Text.Trim() -split "`r?`n" | Select-Object -First 8 | ForEach-Object { Write-Host "   $_" -ForegroundColor DarkGray }
        Die '工作区不干净：发版前先提交或还原上面这些改动'
    }
    Info '工作区干净'

    $branch = (Invoke-Native 'git' @('rev-parse', '--abbrev-ref', 'HEAD')).Text.Trim()
    if ($branch -ne 'master') { Die "当前分支是 $branch —— 发版请在 master 上进行" }
    Info '分支 master'

    $authOut = (Invoke-Native $gh @('auth', 'status')).Text
    if ($authOut -notmatch 'Logged in to') { Die 'gh 未登录：先执行 gh auth login' }
    $account = ($authOut | Select-String -Pattern 'account (\S+)' | Select-Object -First 1).Matches[0].Groups[1].Value
    Info "gh 已登录：$account"

    $running = @(Get-Process *mxrun* -ErrorAction SilentlyContinue)
    if ($running.Count -gt 0) {
        # 只提示，不关：干跑、以及人还没确认的时候不动别人的程序（打包前才真的关）
        Info "有 $($running.Count) 个实例在跑（真发版会在打包前关掉，否则 exe 被锁）"
    }

    # ---------- 2) 算出版本号 ----------
    Step '版本号'
    $toml = Join-Path $root 'Cargo.toml'
    $cur = (Select-String -Path $toml -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
    $parts = $cur -split '\.'
    if ($Version) {
        if ($Version -notmatch '^\d+\.\d+\.\d+$') { Die "-Version 要给 x.y.z（收到 $Version）" }
        $next = $Version
    }
    elseif ($Minor) {
        $next = '{0}.{1}.0' -f [int]$parts[0], ([int]$parts[1] + 1)
    }
    else {
        $next = '{0}.{1}.{2}' -f [int]$parts[0], [int]$parts[1], ([int]$parts[2] + 1)
    }
    Info "当前 $cur  ->  本次 $next"

    if ((Invoke-Native 'git' @('rev-parse', '-q', '--verify', "refs/tags/v$next")).Code -eq 0) {
        Die "标签 v$next 已存在，换一个版本号"
    }

    $describe = Invoke-Native 'git' @('describe', '--tags', '--abbrev=0', '--match', 'v*')
    $lastTag = if ($describe.Code -eq 0 -and $describe.Text.Trim()) { $describe.Text.Trim() } else { $null }
    if ($lastTag) {
        $log = Invoke-Native 'git' @('log', '--oneline', "$lastTag..HEAD")
        Info "上一版 $lastTag"
    }
    else {
        $log = Invoke-Native 'git' @('log', '--oneline', 'HEAD')
        Info '还没有任何标签，本次按首个版本发'
    }
    $commitLines = @($log.Text -split "`r?`n" | Where-Object { $_.Trim() })
    if ($commitLines.Count -eq 0) { Die '自上一个标签以来没有任何提交，没东西可发' }
    Info "自上一版以来 $($commitLines.Count) 个提交"

    # ---------- 3) 发布说明的内容（DryRun 也能先看） ----------
    $notesDir = Join-Path $root 'docs\releases'
    $notesPath = Join-Path $notesDir "v$next.md"
    $zipPath = Join-Path $root "dist\MxRun-v$next-portable.zip"

    $bullets = $commitLines | Select-Object -First 40 | ForEach-Object {
        ' - ' + ($_ -replace '^[0-9a-f]{7,}\s+', '')
    }
    $plan = @(
        "写发布说明    docs\releases\v$next.md"
        '打包          tools\package-portable.ps1（测试 + 构建 + 图标校验 + 压缩）'
        "提交          `"发布 v$next`"（含 Cargo.toml / Cargo.lock / 说明）"
        "打标签        v$next"
        "推送          git push origin master && git push origin v$next"
        "建 Release    gh release create v$next <zip> <sha256> --notes-file docs\releases\v$next.md"
    )

    if ($DryRun) {
        Step '计划（-DryRun：不动仓库、不打包、不关进程）'
        $plan | ForEach-Object { Info $_ }
        Step '将写进发布说明的内容'
        $bullets | ForEach-Object { Write-Host "   $_" -ForegroundColor DarkGray }
        Write-Host "`n干跑结束：什么也没改。" -ForegroundColor Yellow
        Pop-Location
        exit 0
    }

    if (-not $Yes) {
        $prevLabel = if ($lastTag) { $lastTag } else { '无' }
        Write-Host "`n即将发布 v$next（上一版 $prevLabel）：" -ForegroundColor Yellow
        $plan | ForEach-Object { Write-Host "   $_" }
        $ans = Read-Host '确认？(y/N)'
        if ($ans -notmatch '^(y|yes)$') { Write-Host '已取消，什么也没改。'; Pop-Location; exit 0 }
    }

    # ---------- 4) 改版本号 + 写发布说明 ----------
    Step '改版本号、写发布说明'
    $raw = [IO.File]::ReadAllText($toml, [Text.Encoding]::UTF8)
    $raw = $raw -replace ('version = "' + [regex]::Escape($cur) + '"'), ('version = "' + $next + '"')
    [IO.File]::WriteAllText($toml, $raw, (New-Object Text.UTF8Encoding($false)))
    New-Item -ItemType Directory -Force $notesDir | Out-Null

    $notes = @()
    $notes += "# MxRun v$next"
    $notes += ''
    $notes += '## 下载与安装'
    $notes += ''
    $notes += '1. 下载 `MxRun-v' + $next + '-portable.zip`'
    $notes += '2. 解压到**固定目录**（里面已有 `portable.txt`：数据写在同目录 `data\`，整个文件夹可搬）'
    $notes += '3. 双击 `mxrun.exe`；「设置」里可开关开机自启（写 HKCU，不需要管理员）'
    $notes += ''
    $notes += '## 本版内容'
    $notes += ''
    $notes += $bullets
    $notes += ''
    $notes += '## 校验'
    $notes += ''
    $notes += '```'
    $notes += 'SHA256  <打包后填>'
    $notes += '```'
    [IO.File]::WriteAllText($notesPath, (($notes -join "`n") + "`n"), (New-Object Text.UTF8Encoding($false)))
    Info "版本号 -> $next；说明 -> docs\releases\v$next.md"

    # ---------- 5) 打包（测试 + 构建 + 图标校验 + zip） ----------
    Step '打包（先跑测试和真实构建，几分钟）'
    # 这一步真的需要关实例：运行中的 exe 被锁，cargo 会报 os error 5
    Get-Process *mxrun* -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 600
    $pkg = Invoke-Native 'powershell' @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', (Join-Path $PSScriptRoot 'package-portable.ps1'))
    if ($pkg.Code -ne 0) { Die '打包失败 —— 仓库停在"版本号已改、未提交"的状态：修好后重跑本脚本' }
    if (-not (Test-Path $zipPath)) { Die "打包脚本没产出 $zipPath" }
    $sha = (Get-FileHash $zipPath -Algorithm SHA256).Hash
    $sizeMB = [math]::Round((Get-Item $zipPath).Length / 1MB, 2)
    Info "zip $sizeMB MB  SHA256 $sha"

    $notesText = [IO.File]::ReadAllText($notesPath, [Text.Encoding]::UTF8)
    $notesText = $notesText -replace 'SHA256  <打包后填>', ('SHA256  ' + $sha)
    [IO.File]::WriteAllText($notesPath, $notesText, (New-Object Text.UTF8Encoding($false)))

    # ---------- 6) 提交 + 标签 ----------
    Step '提交与标签'
    $msgFile = Join-Path $env:TEMP "mxrun-release-$next.txt"
    $body = (($notes -join "`n") -replace ('^# MxRun v' + [regex]::Escape($next) + "`n+"), '')
    $body = $body -replace 'SHA256  <打包后填>', ('SHA256  ' + $sha)
    [IO.File]::WriteAllText($msgFile, ("发布 v$next`n`n" + $body + "`n"), (New-Object Text.UTF8Encoding($false)))

    $null = Invoke-Native 'git' @('add', '-A')
    $commitRes = Invoke-Native 'git' @('commit', '-F', $msgFile) -ShowLast
    if ($commitRes.Code -ne 0) { Die '提交失败（钩子拦下了？看上面的输出）' }
    $commit = (Invoke-Native 'git' @('rev-parse', '--short', 'HEAD')).Text.Trim()
    Info "提交 $commit"

    if ((Invoke-Native 'git' @('tag', "v$next")).Code -ne 0) { Die "打标签失败：版本号已提交（$commit），手动 git tag v$next" }
    Info "标签 v$next"

    # ---------- 7) 推送 ----------
    Step '推送'
    $push1 = Invoke-Native 'git' @('push', 'origin', 'master') -ShowLast
    if ($push1.Code -ne 0) {
        Die "master 推送失败。本地已提交 $commit 并打好标签 v$next；修好网络/凭据后手动：git push origin master; git push origin v$next"
    }
    $push2 = Invoke-Native 'git' @('push', 'origin', "v$next") -ShowLast
    if ($push2.Code -ne 0) { Die "标签推送失败：master 已推上去，补一次 git push origin v$next" }

    # ---------- 8) 建 Release 并上传 zip ----------
    Step '建 GitHub Release'
    $rel = Invoke-Native $gh @(
        'release', 'create', "v$next", $zipPath, "$zipPath.sha256",
        '--title', "MxRun v$next", '--notes-file', $notesPath, '--verify-tag'
    ) -ShowLast
    if ($rel.Code -ne 0) {
        Die "Release 创建失败。代码与标签都已推送，手动补：gh release create v$next `"$zipPath`" --title `"MxRun v$next`" --notes-file `"$notesPath`""
    }

    Step '完成'
    $view = Invoke-Native $gh @('release', 'view', "v$next", '--json', 'url')
    $url = ''
    if ($view.Code -eq 0 -and $view.Text.Trim()) {
        try { $url = ($view.Text | ConvertFrom-Json).url } catch { $url = '' }
    }
    Write-Host "`n✓ v$next 已发布：$url" -ForegroundColor Green
    Write-Host "   提交 $commit · 标签 v$next · zip $sizeMB MB · SHA256 $sha"
    Write-Host "   发布说明：docs\releases\v$next.md`n"
}
finally {
    Pop-Location
}

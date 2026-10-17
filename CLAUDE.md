# MxRun

Windows 键盘快速启动器，Rust + egui 重写 AltRun。

## 先读这个

**`docs/开发进度.md`** —— 当前进度、已验证的事实、待办顺序、待决策项、踩坑记录。
开工前读它，能省掉大量重新探索。

## 两条容易走偏的前提

1. **对齐目标是 Delphi 原版 AltRun**（用户日常在用的那版，源码在 `<AltRun 源码目录>`），
   **不是** `docs/ALTRun.ahk`（另一作者的 AHK 重写版，交互模型不同，且**不随仓库分发**）。
   权威规格见 `docs/AltRun交互规格.md`。
2. **核心要求只有两条：现代美观 + 性能**。其余（功能完整性、扩展性、插件生态）都要让位。

## 本机环境

**个人路径与数据（AltRun 源码位置、构建工具链、真实清单）都在 `local/机器环境.md`** ——
该目录被 `.gitignore` 排除，仓库里的文档一律用 `<占位符>` 表示，别把真实路径写回文档。

提交前有个 `.githooks/pre-commit` 会拦截含个人信息的提交（拦截词在
`local/personal-patterns.txt`，同样是本机文件）。换机器 clone 后需要执行一次：

```bash
git config core.hooksPath .githooks
```

## 构建

```bash
export PATH="<MinGW-w64>/bin:$HOME/.cargo/bin:$PATH"   # 真实路径见 local/机器环境.md
tasklist | grep -i mxrun      # 有实例在跑就先关掉，否则 exe 被锁 → cargo 报 os error 5
cargo build --release         # → target/release/mxrun.exe
cargo test                    # 单元测试（模型/迁移/搜索/执行器/导入器）
```

构建后**核对 `target/release/mxrun.exe` 的时间戳**——`tail` 会吞掉构建错误，容易误判成功。

## 约定

- **发版由用户说了算（2026-10-17 明确要求）**：平时改完只 `cargo build --release` 自测 + 提交，
  **不动 `Cargo.toml` 的 `version`、不跑 `tools/package-portable.ps1`、不产 `dist/` 的 zip**。
  用户说"发版 / 打个包"时才做，一次跳号、一次交付（`version` 只在那一刻改）。
  真发版走 `tools/release.ps1`：改版本 → 测试/构建/打包 → 写 `docs/releases/v<版本>.md` → 提交 →
  打标签 → 推送 → `gh release create` 传 zip；先 `-DryRun` 看计划，确认后去掉它再加 `-Yes`。
  理由：改一次发一版会让版本号爆炸，他要攒一波再发。开发中想区分构建，看设置页的
  `构建时间`（`exe_build_stamp`）就够了，不需要版本号
- `mxrun.log`（`%APPDATA%\MxRun\`）是排查主力：`wake:` 呼出延迟、`hide:` 隐藏原因、`prompt:` 参数输入、`execute:` 执行记录。加功能时顺手往里加事件
- 判断窗口可见性用 Win32 `IsWindowVisible`，不要用 `Get-Process` 的 `MainWindowHandle`
- egui 空闲时不跑 `ui()`，轮询式检查必须配 `ctx.request_repaint_after(...)`
- 键盘/鼠标交互**能**自动化验证，但要发**真实输入**：`keybd_event` / `mouse_event` + `SetCursorPos`。
  egui 的合成事件不行——拖动窗口会把控制权交给系统模态循环，合成事件会让它永远等不到鼠标松开。
  现成脚本：`tools/verify-drag.ps1`（连续拖三处）、`tools/verify-hover-vs-keys.ps1`（鼠标压着候选区时 ↑↓ 是否还好用）。
  窗口必须在前台；`MXRUN_KEEP_OPEN=1` 可防止它失焦自隐
- **UI 视觉也能自动核对**：`tools/screenshot.ps1` 用 `PrintWindow(PW_RENDERFULLCONTENT)` 走 DWM 合成路径截图
  （2026-09-19 实测可行）。⚠️ 但**从屏幕 DC 做 BitBlt 抓不到**这个窗口（硬件叠加平面，踩坑第 12 条）——区别在路径
- **改源码只用编辑工具，别用 PowerShell 文本管道**：这里的 shell 是 Windows PowerShell 5.1，
  `Get-Content`/`Set-Content` 默认按 ANSI 走，会把中文和破折号整片改成 `?`（2026-09-15 真踩过，
  文件当场不是合法 UTF-8）。读日志要显式 `[Text.Encoding]::UTF8`；**仓库里的 `.ps1` 一律存成 UTF-8 with BOM**，
  否则 PowerShell 5.1 按 GBK 解析它（2026-09-19 又踩一次）
- 需要输入的条目：执行器返回 `NeedsInput` → UI 打开参数提示（顶框切换式）。参数历史存在 redb 的
  `param:` 键下，按条目分开记、上限 50
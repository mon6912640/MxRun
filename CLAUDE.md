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

- `mxrun.log`（`%APPDATA%\MxRun\`）是排查主力：`wake:` 呼出延迟、`hide:` 隐藏原因、`prompt:` 参数输入、`execute:` 执行记录。加功能时顺手往里加事件
- 判断窗口可见性用 Win32 `IsWindowVisible`，不要用 `Get-Process` 的 `MainWindowHandle`
- egui 空闲时不跑 `ui()`，轮询式检查必须配 `ctx.request_repaint_after(...)`
- 键盘交互**不能靠自动化测试验证**（SendKeys 只送前台窗口、WM_CHAR 进不了 egui），需要人工确认；
  自动化环境里也用 GDI 截不到这个窗口（硬件叠加平面），**UI 视觉只能人眼看**
- **改源码只用编辑工具，别用 PowerShell 文本管道**：这里的 shell 是 Windows PowerShell 5.1，
  `Get-Content`/`Set-Content` 默认按 ANSI 走，会把中文和破折号整片改成 `?`（2026-09-15 真踩过，
  文件当场不是合法 UTF-8）。读日志要显式 `[Text.Encoding]::UTF8`
- 需要输入的条目：执行器返回 `NeedsInput` → UI 打开参数提示（顶框切换式）。参数历史存在 redb 的
  `param:` 键下，按条目分开记、上限 50
# MxRun

现代化的 Windows 键盘快速启动器 —— AltRun 的精神续作，用 Rust 重写。

## 当前状态：MVP v0.1

已实现：

- **全局热键呼出 / 隐藏**（默认 `Alt+F1`，可在设置中自定义，保存后立即生效、无需重启；被占用时启动自动回退并提示）
- **系统托盘**：蓝色圆点图标，右键菜单（显示/隐藏、新建条目、设置、退出），双击呼出
- **设置界面**（托盘菜单进入）：热键捕获式录入（点击后按下新组合键）+ 右键集成开关
- **加项通道**：右键 → 发送到 → MxRun、文件/目录右键菜单、`MxRun.exe "<路径>"`、把文件拖到 exe 上、托盘"新建条目…"，或在搜索框里输入一个没有匹配结果的词直接回车。**全程只弹一个小确认框**（关键字 / 名称 / 命令行已预填），主窗口不会跳出来打扰
- **便携**：exe 旁边放一个空的 `portable.txt`，数据库/备份/日志就都存到同目录的 `data\` 里，整个文件夹拷到任何机器上都能用；换位置后**跑一次会自动把右键菜单重新指向当前副本**（`docs/开发进度.md` §2.12）
- **F2 编辑当前条目**（与原版 AltRun 一致；设置因此移到托盘菜单，原版也在那里）
- **参数输入**（P1-1）：选中带 `*` 的条目 → 回车 → 顶框变成参数框，列表位置显示该条目用过的历史参数（↑↓ 选、单击填入）；三种编码与 `{p}`/`%p`/`{%c}` 替换规则照原版
- 无边框、半透明圆角深色窗口，启动/呼出时自动居中（Esc 隐藏）
- **nucleo 打分式模糊搜索**：连续/词首匹配加权，命中字符橙色高亮
- **拼音搜索**：支持全拼与首字母缩写（如 `jsj` 匹配"计算器"）
- **Frecency 智能排序**：频率 × 14 天半衰期时间衰减，替代 AltRun 的单调计数
- **redb 事务存储**（ACID）：崩溃不丢数据；每次启动自动滚动备份最近 10 份到 `%APPDATA%\MxRun\backups\`
- **真实系统图标**：结果行显示 exe / 目录 / 文件 / 快捷方式的系统图标（`IShellItemImageFactory`，按 DPI 取对应像素数），取不到时回退 emoji
- **内置计算器**：直接输入数学表达式（如 `1+2*3`）
- 键盘全操作：↑/↓ 选择，Enter 执行，鼠标悬停/点击亦可
- **对齐 AltRun 的三条核心手感**（明细见 `docs/AltRun交互规格.md`）：
  - **空格 = 执行当前选中项**（空输入时执行第 1 项，与 AltRun 一致）。代价：空格不能再作为搜索词的一部分，多词查询不再可输入——AltRun 本身也不做分词
  - **失焦立即隐藏**：点到别的窗口即消失（用 `GetForegroundWindow` 轮询，250ms 一次；不用 egui 的 ViewportInfo，见踩坑记录）
  - **Esc 二级语义**：输入非空先清空、空输入才隐藏

内置演示命令：计算器、记事本、cmd、资源管理器、Bing、GitHub。

## 架构要点（踩坑记录）

- **事件线程**：全局热键 / 托盘事件在独立后台线程轮询（eframe 窗口隐藏后事件循环休眠，`App::ui` 内轮询会永久失效）
- **窗口显隐**：直接 Win32（`ShowWindow` / `SetWindowPos` / `PostMessage(WM_CLOSE)`），不走 egui `send_viewport_cmd`（eframe 0.36 后台线程发的视口命令会被静默丢弃）；可见性用自有 `AtomicBool` 跟踪（egui 0.36 `ViewportInfo::visible()` 恒为 None，不可信）
- **窗口句柄查找**：按进程 PID + 窗口标题 "MxRun" 过滤——winit 会创建名为 `Winit Thread Event Target` 的可见辅助窗口，仅按"可见顶层窗口"过滤会抓错句柄。但**反过来额外要求 `IsWindowVisible` 同样是坑**：首帧时 winit 还没显示窗口，句柄查不到，于是启动时的居中/置顶静默失效（热键呼出反倒正常，因为那时句柄已被缓存）。句柄查找不能依赖窗口可见性
- **单实例**：redb 对数据文件持独占锁，重复启动会因拿不到锁而失败；而二进制带 `windows_subsystem = "windows"`，启动期 panic 用户完全看不见（双击第二次 = 毫无反应）。现用进程级命名互斥体 `CreateMutexW("MxRun.SingleInstance.v1")` 在 `main` 最前面拦截，提示后干净退出。互斥体句柄常驻 static 不释放——一旦 drop 就等于退出单实例保护，进程结束（含崩溃）由系统回收
- **启动失败可见化**：`MxRunApp::new` 返回 `Result<Self, String>`，数据库/托盘/全局热键任一初始化失败都弹 Win32 `MessageBoxW` 说明原因并 `exit(1)`，不再 `.expect()` 裸崩（没有控制台可打印）

## 开发环境

- Rust `stable-x86_64-pc-windows-gnu`（rustup 管理，位于 `~/.cargo/bin`）
- WinLibs MinGW-w64（提供 `dlltool.exe`/`gcc.exe`，
  GNU 工具链链接 windows-\* crate 时需要）

构建前设置 PATH（Git Bash，路径按自己机器替换）：

```bash
export PATH="<MinGW-w64>/bin:$HOME/.cargo/bin:$PATH"
cargo build            # 调试版
cargo build --release  # 发布版 → target/release/mxrun.exe（约 13 MB）
```

## 数据位置

`%APPDATA%\MxRun\`

- `mxrun.redb` — 命令与 frecency 数据库
- `backups/` — 滚动备份（10 份）

## 性能实测

release 构建，3440×1440，单实例（2026-09-13）：

| 指标 | 实测 | 设计预算 |
|---|---|---|
| 热键按下 → 首帧渲染 | **1.8–5.9 ms** | ≤ 50 ms ✓ |
| 私有提交（常驻，与显隐无关） | **157 MB** | — |
| 工作集（窗口显示中 / 隐藏 6 秒） | 182 / 183 MB | ≤ 30 MB（仅隐藏态口径）✗ |
| 工作集（隐藏数小时后被系统 trim） | ~11 MB | ✓ |
| 线程 / 句柄 | 27 / 493 | — |

渲染后端定为 **Glow(OpenGL)**：同一二进制只换后端对比，wgpu 为 464 MB 提交 / 55 线程 / 881 句柄，画面逐像素一致，故取 glow。`MXRUN_RENDERER=wgpu` 可切回（换 GPU/驱动出问题时的退路）。

**读数陷阱**：早先"常驻 11 MB"只是系统长时间空闲后 **trim 工作集**的结果，不是进程真实占用——该值会随系统内存压力波动，真实占用看"私有提交"列（157 MB 恒定）。做性能对比时务必两个口径都记。

## 路线图

- [ ] 快捷项 GUI 管理（增删改，存储层 `add_command`/`delete_command` 已就绪）
- [ ] Fallback 兜底：无匹配时网页搜索 / Everything IPC 全盘搜索
- [x] 文件图标提取与显示（见上"真实系统图标"；URL 命令暂无文件图标，回退 emoji，
      要不要做 favicon 未定）
- [ ] 拖拽文件到窗口直接创建快捷项
- [ ] `>` 命令面板模式
- [ ] Mica 毛玻璃背景（DWM）、呼出动画
      —— **可行性已验证**：`MXRUN_BACKDROP=acrylic|mica|off` + `MXRUN_CARD_ALPHA=0-255`
      （默认 off）。DWM backdrop 管线在 egui/glow 的**透明无边框置顶**窗口上工作正常，
      一条 `DwmSetWindowAttribute(DWMWA_SYSTEMBACKDROP_TYPE, ...)` 即可，无需换路线。
      但要注意材质选择：**Acrylic(=3/DWMSBT_TRANSIENTWINDOW) 才是毛玻璃**——它模糊背后一切
      （含其他窗口），实测把背后的资源管理器糊成柔和色块；**Mica(=2/DWMSBT_MAINWINDOW)
      只采样壁纸、刻意不显示背后窗口**，在深色壁纸区域表现为一块死平暗色，不适合做启动器材质。
      待做：材质与圆角/卡片透明度的视觉配方（材质会填满整个窗口矩形，现有"靠透明角做圆角"
      的做法会失效）、明暗主题联动
- [ ] 受控目录索引 + UWP 应用索引

设计蓝图见工作区文档《现代启动器设计方案.md》。

## 关于隐私

仓库里**不含**任何本机路径、用户名或软件清单：文档统一用 `<占位符>`，机器相关信息放在
被忽略的 `local/` 目录。提交前 `.githooks/pre-commit` 会扫描暂存内容，命中个人信息直接拒绝提交
（clone 后需执行一次 `git config core.hooksPath .githooks`）。

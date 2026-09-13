# AltRun 实现原理与技术方案调研

> 调研日期：2026-09-13
> 调研对象：AltRun 原版（ET 民工，Delphi/Pascal）与现役开源重写版（zhugecaomao/ALTRun，AutoHotkey v2）

---

## 1. AltRun 是什么

AltRun 是一款 Windows 平台的键盘快速启动器（launcher），通过全局热键呼出一个输入框，输入关键字即可过滤并启动程序、文件、目录、网址或任意命令行。默认主热键 `Alt+R` 也是软件名字的由来。

它有两个主要版本：

| 版本 | 作者 | 技术栈 | 状态 |
|---|---|---|---|
| 原版 AltRun（1.46.x） | ET 民工（etworker） | Delphi / Pascal，闭源分发（曾托管于 Google Code） | 2011 年后停止更新 |
| **ALTRun（AHK 重写版）** | 诸葛草帽（zhugecaomao） | AutoHotkey v2，单文件脚本，GPL v3 开源 | 活跃维护中（2025–2026 年仍持续更新） |

原版停更后，zhugecaomao 用 AutoHotkey 完整重写了 AltRun，并融合了 RunZ、Listary、Everything、Total Commander 等工具的能力，这是目前社区实际使用的版本。本报告的实现原理分析以其开源源码（`ALTRun.ahk`，约 3400 行）为主要依据。 [GitHub - zhugecaomao/ALTRun](https://github.com/zhugecaomao/ALTRun "citation"), [善用佳软 - ALTRun 评测](https://xbeta.info/altrun.htm "citation"), [ALTRun 作者主页](https://zhugecaomao.jimdofree.com/altrun/ "citation")

---

## 2. 核心设计哲学：不建全盘索引

AltRun 与 Launchy、Wox、PowerToys Run 等启动器最根本的区别是：**它只搜索"用户自定义的快捷项列表"，而不是扫描开始菜单或全盘文件**。

- 快捷项通过拖拽、SendTo（发送到）菜单、命令管理器手动添加，或由可控的目录索引生成；
- 搜索时只在一个几百到几千条的内存列表上做正则匹配，因此响应是即时的、内存占用 < 10 MB、单文件绿色便携、不写注册表。 [Freewaregenius - AltRun review](https://freewaregenius.com/altrun-lightweight-application-launcher/ "citation"), [MajorGeeks - ALTRun](https://www.majorgeeks.com/files/details/altrun.html "citation")

这是它"轻量高效"的根本原因：**用数据规模的控制换取性能，而不是用复杂的索引引擎对抗数据规模**。

---

## 3. 整体架构（AHK 版）

整个程序是一个约 150 KB 的单文件 AutoHotkey v2 脚本（`ALTRun.ahk`），用 Ahk2Exe 编译为独立 exe。启动时的自执行段（auto-execute section）按以下顺序初始化：

```
LoadConfig()      ; 读取/创建 INI 配置
LoadCommands()    ; 加载全部命令到内存，构建搜索索引
LoadHistory()     ; 加载历史记录
UpdateSendTo()    ; 维护"发送到"菜单快捷方式
UpdateStartup()   ; 维护开机自启快捷方式
UpdateStartMenu() ; 维护开始菜单项
SetTrayMenu()     ; 系统托盘菜单
SetMainGUI()      ; 创建主界面（输入框 + ListView + 状态栏）
RegisterHotkey()  ; 注册全局热键和上下文热键
Listary()         ; 启动类 Listary 快速切换目录监控
Plugins()         ; 加载插件
AutoCheckUpdate() ; 自动检查更新
```

架构上是一个典型的**单进程事件驱动 GUI 程序**：AHK 运行时自带消息循环，所有功能（热键、GUI 事件、定时器、窗口监控）都注册为回调，无独立索引服务、无后台数据库进程。

### 3.1 数据存储：纯 INI 文件

所有状态持久化在一个 INI 文件中（`IniRead`/`IniWrite`），主要 section：

- `[DefaultCommand]` / `[UserCommand]` — 内置与用户命令；
- `[Index]` — 目录索引生成的命令；
- `[Fallback]` — 搜索无结果时的兜底命令（新建命令、Everything 搜索、Google 搜索等）；
- `[History]` / `[Usage]` — 历史记录与按日使用统计；
- `[Config]` — 全部配置项。

**命令行格式**：`类型 | 路径 | 描述=Rank`，例如 `File | C:\Apps\Everything.exe | 文件搜索=12`。`=` 后的整数就是该命令的优先级权重（SmartRank 分值），解析时用正则 `s)^(.*)=(\d+)\s*$` 从行尾拆分，允许命令文本本身含 `=`。

---

## 4. 关键机制实现原理

### 4.1 全局热键与窗口呼出

- 用 AHK 的 `Hotkey` 命令（底层是 Win32 `RegisterHotKey` / 键盘钩子）注册双全局热键（默认 `Alt+Space` 和 `Alt+R`），回调 `ToggleWindow` 切换主窗口显隐；
- 用 `HotIfWinActive("ahk_id " MainGUI.Hwnd)` 注册**上下文热键**——仅当主窗口激活时，Tab/方向键/F1~F4/Ctrl+N 等按键被重映射为导航与管理操作，窗口隐藏后这些热键自动失效，不污染全局按键；
- 呼出时自动切换到英文输入法（`SwitchToEnglishIME`），保证输入的是匹配关键字而非中文候选；
- 无标题栏窗口通过 `OnMessage(0x201, ...)` 拦截 `WM_LBUTTONDOWN`，回发 `WM_NCLBUTTONDOWN + HTCAPTION` 消息欺骗系统进入标题栏拖动；透明度用 `WinSetTransparent`，圆角用 DWM 的 `DwmSetWindowAttribute`（`SetWindowCorner`）；失焦自动隐藏通过给所有控件注册 `LoseFocus` 事件轮询判断实现。

### 4.2 搜索匹配引擎（核心）

匹配分**离线建索引**与**在线匹配**两步：

**建索引（LoadCommands）**：加载全部命令后，为每条命令预计算一个"可搜索文本"（`g_CMDINDEX`）：
- 默认取"文件名 + 描述"（`MatchPath` 开启时用完整路径）；
- 若开启拼音匹配，再追加中文的**拼音首字母**（见 4.3）；
- 所有命令按 Rank 用 `Sort(rankRows, "R N")` 数值逆序排序——**排序在加载时一次完成，搜索时零排序开销**。

**在线匹配（SearchCommand + BuildFuzzyPattern）**：输入框 `Change` 事件实时触发，把用户输入编译成正则：
- 无空格：转义正则元字符后直接作为子串模式（即"任意位置匹配"）；
- 有空格：按空格分词，各词转义后用 `.*` 连接（`token1.*token2`），实现**保序模糊匹配**；
- 若开启"仅从开头匹配"，模式前加 `^` 锚定（`g_RUNTIME["RegEx"] = "imS)^"` 或 `"imS)"`）；
- 然后对预计算索引数组逐条 `RegExMatch`，命中即取，**达到列表行数上限立即 break**——这是它快的第二个原因：短列表 + 预编译文本 + 提前终止。

```autohotkey
for cmdIndex, searchableText in g_CMDINDEX {
    if RegExMatch(searchableText, regexPattern) {
        g_MATCHED.Push(g_COMMANDS[cmdIndex])
        if g_MATCHED.Length >= listLimit
            break
    }
}
```

与同类软件对比：TypeAndRun 只支持首字母匹配、Executor 按词首匹配、Launchy 模糊匹配效率差，AltRun 的"任意位置关键字匹配 + 可选锚定 + 空格分词保序"在命中率和效率之间取得了很好的平衡。 [善用佳软 - ALTRun 评测](https://xbeta.info/altrun.htm "citation")

**兜底机制（Fallback）**：输入以 `+`、空格、`>` 开头，或搜索无结果时，不回空列表，而是显示 Fallback 命令组——新建命令、用 Everything 全盘搜索当前关键字、Google/Bing 搜索、计算器、以 AHK Run 执行原始命令等。这把"查不到"变成了"换个方式继续查"，是重要的体验设计。

**计算器**：输入纯数学表达式（`+-*/^()` 和数字）且无命令命中时，用 `Eval()` 求值——白名单字符校验 + 递归消括号 + 按优先级（幂 → 乘除 → 加减）正则逐项折叠归约，自实现了 60 行的安全表达式求值器，不依赖 `eval` 式动态执行。

### 4.3 中文拼音首字母匹配

经典的无库实现：内置一张 **GBK 码位区间 → 拼音首字母** 的静态表（如 `[-20319,-20284,"A"]` 对应"啊~芭"），把中文字符用 `StrPut(char, buf, "CP936")` 转成 GBK 双字节码，查表得到首字母。建索引时把首字母串拼进可搜索文本，搜索"pinyin"式缩写（如 `wj` 匹配"文件"）就退化成普通子串匹配，搜索路径完全复用。

### 4.4 SmartRank 智能排序

- 每条命令的 INI 值就是一个整数权重，初始为 1；
- 每次执行该命令，`UpdateRank()` 把权重 +1 写回 INI 并重载内存缓存（也可用 `Ctrl + +/-` 手动调权）；
- 加载时按权重降序排序，因此**越常用的命令在同等匹配下排越靠前**，实现"用得越多越靠前"的自适应排序，无需任何复杂的频率衰减算法。

### 4.5 命令执行分发

`RunCommand()` 按命令类型分发：

| 类型 | 执行方式 |
|---|---|
| `DIR` | `OpenDir()`，优先用配置的文件管理器（如 Total Commander）打开，否则 Explorer |
| `FUNC` | 直接调用脚本内置函数（`%cmdPath%()` 动态调用），实现"内置功能命令"（新建命令、Everything 搜索、Google 搜索、关机……） |
| 其他（File/CMD/URL/App） | AHK `Run()`，底层即 `ShellExecute`，URL 交给默认浏览器，UWP 应用走 `shell:AppsFolder\AppID` |

参数机制：命令行中的占位符（如原版的 `%p`）在执行前由 `ParseArg()` 替换为用户在关键字后输入的参数，支持"关键字 + 空格 + 参数"的两段式输入（例如 `gmailto someone@gmail.com` 直接打开 Gmail 写信）。 [Freewaregenius - AltRun review](https://freewaregenius.com/altrun-lightweight-application-launcher/ "citation")

### 4.6 受控目录索引（Reindex）

虽然不做全盘索引，但提供可控的目录索引：
- 按配置递归扫描指定目录、指定扩展名（`Loop Files ... "R"`），支持最大深度（数路径分隔符）和排除正则；
- UWP/商店应用通过调用 PowerShell `Get-StartApps` 导出 CSV 再解析，转成 `shell:AppsFolder\AppID` 命令；
- 扫描结果攒成一个大字符串**一次性 `IniWrite`**，避免逐条写 INI 造成的 IO 放大和云同步目录（如 OneDrive）报错——一个值得注意的性能细节。

### 4.7 类 Listary 快速切换目录

- 用 `GroupAdd` 定义窗口组：文件管理器（Explorer/TC）、打开/保存对话框、排除窗口；
- 主循环 `WinWaitActive` + 轮询监控活动窗口，判定当前窗口是文件对话框（按控件类名 `SysListView32` 等和标题特征双重识别）后，注册仅在该对话框生效的热键（如 `Ctrl+G`），按下即读取 Total Commander/Explorer 当前路径并写回对话框的路径栏；
- 另起一个 `SetTimer` 在对话框标题上显示快捷键提示。这是对 Listary 核心功能的约 150 行复刻。

### 4.8 其他集成

- **Everything**：本质是把搜索词作为命令行参数转发（`Everything.exe -s "关键字"`），零耦合；
- **SendTo 菜单**：安装时向系统 SendTo 目录写入指向自身 exe 的 `.lnk`，右键"发送到"即把任意文件登记为 AltRun 命令；
- **单文件资源**：默认背景图以 Base64 内嵌在源码里，运行时 `CryptStringToBinary` 解码到临时目录，保证单 exe 分发。

---

## 5. 技术方案总结

| 维度 | 方案 |
|---|---|
| 语言/运行时 | AutoHotkey v2（原 Delphi），Ahk2Exe 编译为单 exe |
| 数据模型 | 快捷项列表（类型 \| 路径 \| 描述 = 权重），INI 持久化 |
| 搜索策略 | 预计算可搜索文本（文件名+描述+拼音首字母）→ 空格分词 `.*` 连接的正则 → 顺序扫描 + 命中上限提前终止 |
| 排序 | 使用频率计数权重，加载时一次降序排序（SmartRank） |
| 中文支持 | GBK 码位区间表映射拼音首字母 |
| 热键 | 全局热键（RegisterHotKey）+ 窗口上下文热键（HotIfWinActive）双轨 |
| 扩展性 | FUNC 类型内置函数命令 + 外部工具转发（Everything/TC/Listary 复刻）+ SendTo 集成 |
| 性能哲学 | 控制数据规模（用户自定义列表）而非对抗规模；加载时重活做完，搜索路径上只做正则扫描 |

**可借鉴点**：
1. "不索引全盘"的产品取舍——对启动器而言，用户真正常用的入口不超过几百个；
2. 把排序、拼音展开等重计算全部前移到加载阶段，搜索热路径极简；
3. Fallback 兜底列表把"搜索失败"转化为另一种操作入口；
4. INI 攒批一次写入避免 IO 放大；
5. AHK 的上下文热键（HotIf）是低成本实现"模态按键"的范例。

**局限**：单线程消息循环，索引量大时加载和重载有感知；拼音只支持首字母且依赖 GBK 码表（生僻字可能失配）；表达式计算器等为正则归约实现，能力有限；依赖 Windows 专属 API 与 AHK 运行时，不可移植。

---

## 6. 参考资料

1. [GitHub - zhugecaomao/ALTRun（AHK 开源重写版）](https://github.com/zhugecaomao/ALTRun "citation")
2. [善用佳软 - 神逸之作：国产快速启动软件神品 ALTRun](https://xbeta.info/altrun.htm "citation")
3. [Freewaregenius - AltRun: lightweight application launcher](https://freewaregenius.com/altrun-lightweight-application-launcher/ "citation")
4. [MajorGeeks - ALTRun](https://www.majorgeeks.com/files/details/altrun.html "citation")
5. [ALTRun 作者主页（诸葛草帽）](https://zhugecaomao.jimdofree.com/altrun/ "citation")
6. 源码分析：`ALTRun.ahk`（zhugecaomao 的 AHK 版，本机留存副本；**他人代码，不随本仓库分发**）

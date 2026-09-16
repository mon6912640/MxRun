# AltRun 交互规格（从 Delphi 源码提取）

> 提取日期：2026-09-13
> 权威来源：**2014 年 Delphi 原版 AltRun 源码**（用户日常在用的那一版；本机路径见 `local/机器环境.md`，不随仓库分发）
> 交叉参照：另一作者的 AHK 重写版 `ALTRun.ahk`（**不是**对齐目标，差异见 §12；该文件是他人代码，**不随仓库分发**，本机留存副本）
> 行号为 `iconv -f GBK -t UTF-8` 后的编号，与原文件一一对应

## 总览

单窗口、无标题栏、常驻置顶的键盘启动器。全局热键切换显隐（**本机是 `Alt+1`**，见 §2 校正），窗口出现时焦点永远在唯一的多行过滤框里。**输入即过滤**：每敲一个字符重算结果列表（最多 10 行）并自动选中第 1 行。**回车执行当前选中项**（空格、中键、双击、`Alt/Ctrl+数字`、`;`、`'` 都能执行）。带参数的快捷项走**两段式**：选出项 → 弹出独立参数对话框 → 输入参数 → 回车。占位符在真实命令行里被替换。窗口**失去焦点立即隐藏**，另有 `HideDelay` 秒无操作自动隐藏。

哲学：**关键字即一切，回车即确认**——把 Show-Then-Do 压成一次热键 + 两个按键。

### 三条必须先看的校正

1. **`HotKey=Alt + R` 是死键。** 代码只读 `HotKey1`/`HotKey2`（`Unit/untALTRunOption.pas:47-48`、`:746-747`），裸 `HotKey` 从未被读取。真正的第一热键是 `HotKey1=Alt + 1`（配置界面里叫 *Primary HotKey*，`untALTRunOption.pas:1001`）。
   **实测复核（2026-09-13，对运行中的 ALTRun.exe）**：按 `Alt+R` 窗口无反应；按 `Alt+1` 窗口出现（标题 `ALTRun`），再按一次隐藏——确认为开关式切换。
2. **没有"关键字 + 空格 + 参数"的行内参数输入。** 空格是"执行"触发键（§1.2），参数一律走 `frmParam` 对话框。
3. **结果列表是单列 `TListBox`，不是表格。** 序号是行文本的前两个字符，不是独立列。AHK 版的 4 列 ListView 属于另一作者的设计。

---

## 1. 按键映射

按键处理有两层：窗体级 `FormKeyDown`（`KeyPreview = True`，`Form/frmALTRun.dfm:720`、`:726`）先于焦点控件执行；`Key := VK_NONAME` 即吞掉该键。

### 1.1 通用（窗体级）

| 键 | 行为 | 出处 |
|---|---|---|
| Enter | 执行当前选中项（窗口先隐藏）；无结果则弹"无此项,添加它?" | `frmALTRun.pas:2296-2302`、`:662-752` |
| Esc | 输入框非空 → 清空；输入框为空 → 隐藏窗口 | `frmALTRun.pas:2255-2265` |
| ↑ / ↓ | 上/下一项，**首尾循环**，同步标题栏与命令行显示 | `frmALTRun.pas:2175-2213` |
| F1 | 关于窗口 | `frmALTRun.pas:2215-2219` |
| F2 | 编辑当前项 | `frmALTRun.pas:2221-2225` |
| Insert | 新建快捷项 | `frmALTRun.pas:2227-2231` |
| Delete | 删除当前项（带确认框） | `frmALTRun.pas:2233-2241`、`:559-582` |
| Ctrl+D | 打开当前项所在文件夹（无目录则静默不动作） | `frmALTRun.pas:1610-1623`、`DirAvailable :1256-1319` |
| Ctrl+C | 当前项命令行复制到剪贴板，右下角显示 ★ 250ms | `frmALTRun.pas:1625-1636`、`:548-556` |
| Ctrl+L | 用"最近执行列表"（≤10 项）替换结果列表 | `frmALTRun.pas:1638-1645`、`:2640-2686` |
| Tab | = ↓（`actDown` 的 SecondaryShortCut） | `frmALTRun.dfm:8640-8644`、`frmALTRun.pas:584-613` |
| Shift+Tab | = ↑ | `frmALTRun.dfm:8634-8639`、`frmALTRun.pas:1155-1184` |
| Alt+S | 快捷项管理器（窗体级，非全局） | `frmALTRun.dfm:8587-8592` |
| Alt+C | 配置窗口（窗体级） | `frmALTRun.dfm:8594-8599` |
| PageUp/PageDown | `PostMessage` 给列表原生翻页，**焦点不转移**，标题栏/命令行不更新 | `frmALTRun.pas:1533-1547` |
| Ctrl+0..9 / Alt+0..9（含小键盘） | 直接执行第 N 项 | `frmALTRun.pas:1562-1585` |
| `;` | 执行第 2 项 | `frmALTRun.pas:1587-1609` |
| `'` | 执行第 3 项 | `frmALTRun.pas:1587-1609` |
| 0..9（无修饰） | 过滤后列表为空时，执行"上一次结果列表"的第 N 项 | `frmALTRun.pas:1416-1442` |
| **Space** | 见 §1.2 | `frmALTRun.pas:1453-1468` |
| 鼠标中键（搜索框/标题/背景/列表） | 执行当前项 | `:1356-1363`、`:2442-2453`、`:2465-2477`、`:2529-2540` |
| 滚轮 | 上/下一项（仅 `m_IsTop` 时） | `:1761-1827` |
| 关闭按钮 / Alt+F4 | 非 Debug 模式 → 仅隐藏并清空输入，**不退出** | `frmALTRun.pas:1786-1802` |
| Home/End/Backspace/F3–F12 | **未找到**绑定，走编辑框原生行为 | — |

### 1.2 Space 的确切语义

每次输入变化后重建列表；若重建后**列表为空**且文本末字符是空格，就执行"空格前高亮的那一项"（`frmALTRun.pas:1453-1468`）。

- 脚本里**没有任何 ShortCut 含空格**（`ShortCutList.txt` 全表），所以输入空格在 Regex 模式下必然无匹配 → 列表必空 → **Space 恒等于"执行当前高亮项"**。
- 输入框**为空**时按空格同样成立：`m_LastShortCutCmdIndex` 由 `GetLastCmdList`（`:2350-2377`）置为 0 → **执行第 1 项**。本机 `FavoriteList.txt` 第 1 行是 `(30 空格)|我的电脑`，正是这条路径把空关键字写进收藏表的实证（`untShortCutMan.pas:2073`）。
- 第二参数 `KeyWord` = 空格前的文本，**不会**成为执行参数，只用于写 `FavoriteList.txt`（`untShortCutMan.pas:2071-2077`）。

### 1.3 焦点在结果列表时

- Enter：本控件不处理（`case 13: ;`，`:2494`），由窗体级 `FormKeyPress` 执行。
- PageUp/PageDown：放行给原生列表。
- **其它任何键**：`PostMessage(edtShortCut.Handle, WM_KEYDOWN, Key, 0)` + 搜索框 `SetFocus` —— "任何输入都弹回搜索框"（`:2496-2512`）。
- 单击 = 选中（`dfm:3361`）；双击 = 执行（`dfm:3362`）；左键无动作；右键 = 先模拟左键选中再弹 `pmList`（`:2525-2539`）。

### 1.4 焦点在命令行时

该控件 `ReadOnly = True`、`TabStop = False`，只能靠鼠标点击获得焦点（`dfm:3376-3400`）。

- `FormKeyDown` 第一句 `if edtCommandLine.Focused then Exit`（`:2143-2144`）→ **窗体级所有按键失效**。
- Enter / PageUp / PageDown 放行 → Enter 冒泡到 `FormKeyPress` 执行。
- 其它任何键：`PostMessage` 给搜索框 + 转移焦点（`:1331-1355`）。
- 右键挂 `pmCommandLine`，但 `OnPopup = pmListPopup` 且**没有任何菜单项** → 实际是空菜单（`:2575-2579`、`dfm:8977-8982`）。

---

## 2. 热键模型

| 组件 | INI 键 | 默认 | 本机值 | 行为 |
|---|---|---|---|---|
| `hkmHotkey1` | `HotKey1` | `Alt + R` | **`Alt + 1`** | 切换显隐 |
| `hkmHotkey2` | `HotKey2` | `Pause` | `     `（5 空格） | 切换显隐，**与 hotkey1 同码同义** |
| `hkmHotkey3` | `LastItemHotKey` | `ALT+L` | `ALT+L` | **直接执行"最近一次使用"的项，不显示窗口** |

- 三者都在**全局**注册（Win32 `RegisterHotKey` 注册到 `Application.Handle`，`3rdUnit/HotKeyManager/HotKeyManager.pas:191`、`:499`），不依赖窗口是否激活。注册点：`:1194`、`:1212`、`:2081`。
- 所谓"主/副"只是配置界面的 *Primary / Secondary HotKey* 措辞（`untALTRunOption.pas:1001-1002`），**运行时语义完全一致**。两者绑定同一个 `OnHotKeyPressed`（`dfm:8651-8655`、`:8977-8981`）。
- 热键按下时先抓一组"前台上下文"（`frmALTRun.pas:2419-2430`）：`Param[0] := Clipboard.AsUnicodeText`、`Param[1] := 前台窗口句柄`、`Param[2] := 前台窗口标题`、`Param[3] := 前台窗口类名`。然后 `if m_IsShow then actHideExecute else actShowExecute`。
- `hkmHotkey3`：抓同样的 Param[0..3]，取"最近项列表"第 1 项**直接执行，不显示窗口**（`frmALTRun.pas:2380-2409`）。源码注释：`//TODO: 暂时以ALT+L作为调用最近一次快捷项的热键`（`:2080`）。
- **不存在"仅窗口激活时生效"的上下文热键**：Tab/Shift+Tab/Alt+S/Alt+C/F1/F2/Insert/Delete/数字/`;`/`'` 全部是窗体/控件事件处理器，只有窗口激活时可用；没有为它们做 `RegisterHotKey`，也没有 `GetAsyncKeyState` 轮询。
- 禁用次级热键：`HotKey2` 为空串或等于 `resVoidHotKey`（默认 `'NONE'`，`untALTRunOption.pas:999`）时静默不注册（`:1218-1228`）。本机值是 5 个空格——既非空串也非 `'NONE'`，`TextToHotKey` 返回 0 → 会走"弹警告框并把 `HotKeyStr2` 置空"的分支（`:1220-1225`）。
- 打开配置窗口前先 `hkmHotkey1.ClearHotKeys; hkmHotkey2.ClearHotKeys` 避免注册冲突（`:265-266`）。

---

## 3. 参数机制

### 3.1 流程（两段式，**弹对话框**）

1. 输入关键字 → 列表过滤 → 自动选中第 1 项（`:1473-1478`）。
2. Enter / Space / Ctrl+数字 / Alt+数字 / `;` / `'` / 双击 / 中键 → `ShortCutMan.Execute(item, keyword)`（`:731`）。
3. `ParamType = ptNone` → 直接执行，占位符**不替换**（`untShortCutMan.pas:736-744`）。
4. 否则检查命令行里有没有"自动取参"标记（`{%c}`/`{%wd}`/`{%wt}`/`{%wc}`）；都没有 → **弹 `ParamForm.ShowModal`**，取消则本次执行作废（`:745-780`）。
5. 拿到参数后按类型编码，交给工作线程做占位符替换与 `ShellExecute`（`:781-795`、`:159-330`）。

### 3.2 占位符全集

定义于 `Unit/untALTRunOption.pas:27-39`：

| 常量 | 字面值 | 含义 |
|---|---|---|
| `NEW_PARAM_FLAG` | `{%p}` | 参数（新语法，**优先于 `%p`**） |
| `PARAM_FLAG` | `%p` | 参数（旧语法） |
| `CLIPBOARD_FLAG` | `{%c}` | 剪贴板文本 |
| `FOREGROUND_WINDOW_ID_FLAG` | `{%wd}` | 前台窗口句柄（十进制） |
| `FOREGROUND_WINDOW_TEXT_FLAG` | `{%wt}` | 前台窗口标题 |
| `FOREGROUND_WINDOW_CLASS_FLAG` | `{%wc}` | 前台窗口类名 |
| `SHOW_MAX_FLAG` | `@+` | 命令行首前缀：最大化启动 |
| `SHOW_MIN_FLAG` | `@-` | 命令行首前缀：最小化启动 |
| `SHOW_HIDE_FLAG` | `@` | 命令行首前缀：隐藏启动 |

规则：
- **前缀顺序敏感**：必须先判 `@+`/`@-` 再判 `@`，代码用 `if/else if` 保证（`untShortCutMan.pas:172-186`）。
- **多个占位符只替换第一个命中的**（同一串 `if/else if` 链，`:194-261`）。
- **`{%c}`/`{%wd}`/`{%wt}`/`{%wc}` 只在 `ParamType <> ptNone` 时才被替换**（`:736-744`）。
- 裸 `%c` **不支持**（只有 `{%c}`）；`frmShortCut` 的提示文字仍写旧语法 `"%c"`，属过期文案（`Form/frmShortCut.dfm:106-108`）。
- 环境变量 `%VAR%` 执行前展开（`untShortCutMan.pas:1746-1756`）。
- 参数编码：`ptNoEncoding` 原样；`ptURLQuery` → `%XX`；`ptUTF8Query` → 先 `Utf8Encode` 再 `%XX`；空格转 `+`，`A-Za-z*@._-` 不转义（`Unit/untUtilities.pas:1117-1150`）。
- **没有** `%1`/`%2`/`{clip}`/`{date}` 之类的其它 token。

### 3.3 参数历史（`ParamHistory.txt`）

- 路径 = exe 同目录（`Form/frmParam.pas:26`、`:190`）。
- 写：`Format('%-10d|%-30s%', [Rank, Param])`（`:366-367`）。
- 读：按 `|` 切分 → rank = 使用次数 → 去重 → 读满 `ParamHistoryLimit` 即停（`:252-326`）。
- 上限处理（默认 50，`untALTRunOption.pas:757`）：读满即停；新增时若已满，**从后向前删掉 rank 最小的**，新参数插到**第一位**；已存在则 rank+1（`frmParam.pas:81-108`）；写时只写前 limit 条（`:359-362`）。
- 生命周期：`FormCreate → LoadParamHistory`（`:191`）、`FormDestroy → SaveParamHistory`（`:196`）。

### 3.4 `frmParam` 对话框

- 可编辑下拉框 `cbbParam: TComboBoxEx`（`AutoCompleteOptions = [acoAutoSuggest, acoAutoAppend]`）+ `Default = True` 的确定按钮（`Form/frmParam.dfm:30-53`）。
- `WS_POPUP + WS_EX_TOPMOST`、`WndParent = GetDesktopWindow`、`poScreenCenter`（`frmParam.pas:169-179`、`dfm:13`）。标题 = 当前快捷项的 `Name`（`untShortCutMan.pas:766`）。
- Enter = 确定（`:129-134`、`:226-238`）；Esc = 先清空文本、空后再取消（`:208-222`）；`HideDelay` 秒无操作自动取消（`:328-338`、`:384-388`）；显示时 `SetFocus`（`:249`）。
- 源码注释：因 `TComboBox` 对中文有 Bug 而改用 `TComboBoxEx`（`frmParam.pas:1-2`）。

---

## 4. 剪贴板变量

- 语法 **`{%c}`**（`untALTRunOption.pas:29`）。
- **取值时机 = 呼出热键被按下的瞬间**，不是执行瞬间：`ShortCutMan.Param[0] := Clipboard.AsUnicodeText`（`frmALTRun.pas:2419`、`:2387`）。
- 替换条件：`ParamType <> ptNone` **且**命令行含 `{%c}`；命中后 `cmdobj.Param := m_Param[0]`，**不弹参数框**（`untShortCutMan.pas:745-748`）。
- 同一机制产出 `{%wd}`/`{%wt}`/`{%wc}`，存 `Param[1..3]`；`m_Param` 是 `array[0..5]`（`untShortCutMan.pas:73`）。
- 本机实用例（`ShortCutList.txt:38-41`）：`cb` = `http://www.baidu.com/s?wd={%c}`、`cg` = Google、`ShowOnly` = `@.\WinCtl.exe ShowOnly {%wd}`。

---

## 5. Fallback 兜底

**Delphi 版没有任何兜底机制**——全树 grep `Fallback` / 搜索引擎常量均 0 命中。用户感知的"搜索兜底"其实就是**一批普通快捷项**，靠关键字匹配出现：

| ShortCut | ParamType | 命令行 | 作用 |
|---|---|---|---|
| `b` | URL_Query | `http://www.baidu.com/s?wd=` | 百度搜索（弹参数框） |
| `g` | UTF8_Query | `http://www.google.com/search?q=` | Google 搜索 |
| `s` | URL_Query | `http://mp3.sogou.com/music.so?query=` | 搜狗 MP3 |
| `zd` | URL_Query | `http://zhidao.baidu.com/q?...&word=` | 百度知道 |
| `v` | UTF8_Query | `http://www.verycd.com/search/folders?kw=` | VeryCD |
| `y` | UTF8_Query | `http://search.yahoo.com/search?p=` | Yahoo |
| `r` | No_Encoding | `{%p}` | "运行"对话框 |
| `cb` | URL_Query | `http://www.baidu.com/s?wd={%c}` | 剪贴板搜百度（**不弹框**） |
| `cg` | UTF8_Query | `http://www.google.com/search?q={%c}` | 剪贴板搜 Google |

（`ShortCutList.txt:23-42`；出厂模板 `untShortCutMan.pas:1447-1464`。）

唯一的"无结果"分支：按 Enter 且列表为空 → 弹 `无此项 "%s", 添加它?`，确定则用输入文本新建快捷项（`frmALTRun.pas:748-752`、`:220-228`、`untShortCutMan.pas:799-884`）。空格路径不会走到这里。

近亲功能：`Ctrl+L` 用"最近执行的 ≤10 项"替换列表（非兜底，是历史回访）。

---

## 6. 焦点与隐藏

- **失焦立即隐藏**：`evtMainDeactivate` 里所有"延迟再隐藏"的分支都被注释掉，实际只剩 `evtMainMinimize(Sender); edtShortCut.Text := '';`（`frmALTRun.pas:1698-1705`）。现行代码不用任何阈值。
- **无操作自动隐藏**：`RestartHideTimer(Delay)` → `tmrHide.Interval := Delay * 1000`（`:2628-2638`）→ 隐藏 + 清空（`:2737-2743`）。
  - **`HideDelay` 单位 = 秒**，默认 15，本机 15。仅当 `m_IsShow = True` 时生效。
  - 重启时机：`FormActivate`、`evtMainActivate`、`FormKeyDown`、若干 MouseActivate、`actShowExecute`、`actShortCutExecute` 结束（`:1852`、`:1691-1696`、`:2180`、`:1688`、`:2522`、`:2462`、`:1127`、`:966`）。
- **执行命令后总是先隐藏**：`actExecuteExecute` 第 3 行就 `evtMainMinimize(Self)`（`:674`），与 `ExitWhenExecute` 无关。`ExitWhenExecute = 1` 时额外启 `tmrExit` → `Application.Terminate`（`:745-746`、`:2701-2707`）。本机 = 0。
- `actHideExecute` = 隐藏 + 清空输入（`:755-762`）。
- **`frmAutoHide` 是一条渐隐提示条**（不是"自动隐藏"窗体）：`AlphaBlendValue` 每 tick 减 10，降到 0 就 `mrOk`（`Form/frmAutoHide.pas:36-53`）。**唯一用途**：首次单击托盘图标时提示"最好通过热键 %s 来显示"（`frmALTRun.pas:2543-2572`）。
- `m_IsTop` 标志：打开模态子窗口前后置 `False`/`True`，抑制 `tmrHide` 与滚轮处理（`:156`、`:940-966`、`:363-365`）。

---

## 7. 结果列表

- 控件是**单列 `TListBox`**：`BorderStyle = bsNone`、`ItemHeight = 16`、`PopupMenu = pmList`（`Form/frmALTRun.dfm:3339-3366`）。**无列结构。**
- **行文本 = 序号标记(2 字符) + `ListFormat` 渲染串**：
  - `ListFormat` 默认 `%-25s| %s`，三选一：`%-25s| %s` / `%s (%s)` / `%s [%s]`（`untALTRunOption.pas:659-663`、`:798`）；参数是 `[ShortCut, Name]`（`untShortCutMan.pas:1030`）。
  - 序号标记由 `Format(' %d|%s', [...])` 或 `Format('*%d|%s', [...])` 生成（`:1058-1081`）。
  - **`*` 前缀 = 该项带参数（执行时会弹参数框）**，空格 = 无参数。
  - 第 11 项起序号位变成两个空格（`:1078-1079`）。
- **序号基准**由 `IndexFrom0to9` 决定（默认 False）：
  - `0`（**本机值**）→ 显示 `1,2,...,9,0`，第 10 项显示 `0`；映射 `(i+1) mod 10`。
  - `1` → 显示 `0..9`。
- **数字键提示**：`edtHint` 覆盖在搜索框右侧（右对齐），条件为 `ShowOperationHint` 且选中行第 2 字符是数字且输入长度 < 10 → 显示"按下 ALT+%s 或 CTRL+%s 执行快捷项"（`:1657-1665`、`:1188-1195`）。
- **选中项随输入变化**：每次 `edtShortCutChange` 结束都 `lstShortCut.ItemIndex := 0`，并同步 `lblShortCut.Caption := Name`（能打开文件夹时包 `[...]`）与 `edtCommandLine.Text := 'CMD=' + CommandLine`（`:1473-1478`、`:1321-1329`、`:843-855`）。
- **行数上限**：`ShowTopTen = 1`（本机）时只放前 10 项（`:1393-1400`）；否则放全部匹配。控件高度固定 `10 * ItemHeight`（`:1101`）。
- 窗口高度：`ShowCommandLine = 1` → 250px；否则 230px（`:1104-1107`）。

---

## 8. 搜索行为

实现集中在 `TShortCutMan.FilterKeyWord`（`Unit/untShortCutMan.pas:925-1095`）。

- **匹配字段只有 `ShortCut`（关键字）**，不匹配 `Name`，也不匹配 `CommandLine`（`:1001`、`:1013`）。大小写不敏感（`:953`）。
- **长度预筛**：`if Length(KeyWord) > Length(Item.ShortCut) then Continue`（`:991`）。
- **`Regex = 1`（本机值，变量名 `EnableRegex`）**：关键字先做通配符转换——把已有 `.*` 换成占位、`*` → `.*`、`?` → `.`（`:958-981`）；再用 `TRegExpr.Exec` 做一次**非锚定**搜索，`MatchPos[0]` 即命中位置（`:995-1010`）。
- **`MatchAnywhere`**（默认 True，本机 1）：`MatchPos[0] > 1` 就 `Continue`（`:1005-1006`）；非 Regex 路径同理（`:1016`）。即 **= 0 时要求从头匹配**，没有第三个开关。
- **没有空格分词**。空格是普通字符参与匹配，不存在多 token `.*` 连接。输入恰好是单个空格时直接返回空结果（`:941-942`）。
- **没有拼音匹配**：全树 grep 无拼音表/首字母索引。
- 两条隐式行为：
  1. **评分 ≤ 0 的命中会被整条丢弃**：`Rank = 1024 + Freq*4 - matchPos*128 - (len(ShortCut)-len(KeyWord))*16`，且只有 `Rank > 0` 才加入结果（`:1021-1036`）。命中位置越靠后越容易被丢掉。
  2. **`Item.Rank` 先被当作"命中位置"用，再被覆写成"评分"**（`:1004`、`:1015`、`:1026`）。
- `RememberFavouratMatch = 1` 时：若 `FavoriteList.txt` 里存在 `小写关键字 → Name` 映射且命中当前结果，把该项**移到第 1 位**（`:1044-1054`）。写入点在每次带关键字执行时（`:2073`）。本机 = 0，但 `FavoriteList.txt` 已有 4.6KB 数据（历史开过）。配置界面文案 `Remember Last ShortCut Match` 与实现语义不符（`untALTRunOption.pas:1036`）。

---

## 9. 排序

- **没有 `SmartRank` 开关**（全树 grep 零命中）。两套硬编码公式：
- **加载时排序**（`untShortCutMan.pas:1894-1929`）：
  `Rank = Freq*1000 - (4*(c1-'A') + 2*(c2-'A') + 1*(c3-'A')) - Length(ShortCut)`，然后 `QuickSort` 降序。
  含义：**使用频率主导，前 3 个字母越靠前越优先，同频时短名优先**。
- **过滤时评分**（每次输入重算，`:1026`）：`1024 + Freq*4 - matchPos*128 - (len差)*16`。
- **同分不确定**：`SelectPivot` 用 `Randomize` + `Random` 选支点（`:1830-1836`）→ 同 Rank 项顺序每次可能不同。
- **`Freq` 读写时机（关键）**：
  - 读（+1）：`Inc(ShortCutItem.Freq)`，在 `Execute(ShortCutItem, KeyWord)` 重载里（`:2071-2077`）。所有执行路径都走它。
  - **写（落盘）不是每次执行都发生**：只有 `SaveShortCutList` 被调用时才写回 `ShortCutList.txt`。调用点：程序退出（`frmALTRun.pas:248`）、配置窗口确定（`:431`）、编辑项确定（`:652`）、管理器确定（`:951`）、删除项（`untShortCutMan.pas:1549`）、新增/覆盖（`:466`、`:479`）。
  - 即**运行期间累积的 Freq 只有退出或进过上述对话框后才落盘；崩溃/强杀会丢**。
- **行格式**：`Format('F%-8d|%-20s|%-30s|%-30s|%s', [Freq, ParamType, ShortCut, Name, CommandLine])`（`:1874-1893`）。解析时 `F` 开头才认为是新格式；超过 8 位数字按 10000 处理（`:1947-2061`、具体 `:2000-2015`）。
- **`ShowTopTen` 只影响"显示几行"，不影响排序**。
- **"最近使用"是另一套数据**：`AddLatestShortCutItem` 把执行过的项移到 `m_LatestList` 首位，上限 `MAX_LATEST_NUM = 10`（`:27`、`:489-505`）；退出时索引串写入 INI `[DEBUG] LatestList`（`frmALTRun.pas:237-238` → `untALTRunOption.pas:889`）；启动时还原（`:1099`、`untShortCutMan.pas:1839-1867`）。本机 `LatestList=0, 59`。
- 管理器里 Freq 藏在 `TListItem.Data`，**拖拽重排不复制 `Data` → 拖一下就丢 Freq**（`frmShortCutMan.pas:627-653`、`untShortCutMan.pas:905`、`:1344`）。

---

## 10. 窗口行为

- **无标题栏**：`BorderStyle = bsNone`（`dfm:4`）；`FormStyle = fsStayOnTop`（`dfm:11`）。
- **拖动**：在背景图 `imgBackground` 或标题栏 `lblShortCut` 上左键按下 → `ReleaseCapture; SendMessage(Handle, WM_SYSCOMMAND, SC_DRAGMOVE, 0)`（`frmALTRun.pas:2442-2453`、`:2465-2477`）。中键在同位置 = 执行当前项。
- **透明度**：首次显示时加 `WS_EX_LAYERED`，`SetLayeredWindowAttributes(Handle, AlphaColor, Alpha, LWA_ALPHA or LWA_COLORKEY)`（`:1086-1093`）。`AlphaColor` 是**颜色键**（默认 `clBlack`，本机 128）；`Alpha` 才是整体不透明度（默认 240，本机 240）。
- **圆角**：`SetWindowRgn(Handle, CreateRoundRectRgn(0,0,Width,Height,RoundBorderRadius,RoundBorderRadius), True)`（`:1096`）。`RoundBorderRadius` 默认 12，本机 12。
- **皮肤**：`ShowSkin = 1` 时从 exe 目录加载 `BGFileName`（默认 `BG.jpg`）到 `imgBackground`；文件不存在则先把内置图片写成该文件（`:1039-1043`、`untALTRunOption.pas:788`）。`BGFileName` 不可通过 UI 修改。
- **尺寸**：宽度用 `FormWidth`（默认 420，本机 420），只在"首次显示 + WinTop/WinLeft 非 0"的分支里应用（`:1031`）；高度固定 250/230。**不可拖拽缩放**。
- **位置记忆**：`WinTop`/`WinLeft`。首次显示时若 `(WinTop <= 0) or (WinLeft <= 0)` 则 `poScreenCenter`，否则用记录值（`:1024-1031`）；显示结束后与退出时写回（`:1131-1132`、`:237-238`）。
- **标题栏按钮**：三个 `TSpeedButton`（管理器/配置/关闭），可见性由 `ShowShortCutButton`/`ShowConfigButton`/`ShowCloseButton` 决定，并按可见性重排标题栏（`:1046-1084`）。**本机三者全为 0 → 全隐藏**。
- **提示行 `edtHint`**：与搜索框重叠，空输入时从 `HintList[0..24]` **随机**挑一条显示；输入 1–5 字符时显示"回车/空格执行""Ctrl+D 打开目录""回车添加"之一；≥6 字符则隐藏（`:2581-2617`、`untALTRunOption.pas:645-660`）。
- **弹出音**：`PlayPopupNotify = 1` 时播放 exe 目录的 `Popup.wav`（`:1136-1152`）。

---

## 11. 其它用户可感知的交互

- **单实例**：`IsRunningInstance('ALTRUN_MUTEX')` → 已运行则给自己的窗口 `SendMessage(WM_ALTRUN_SHOW_WINDOW)` 后 `Halt(1)`（`frmALTRun.pas:2029-2036`；处理见 `:2761-2767`）。
- **开机自启**（两处都做）：注册表 `HKCU\...\Run` 写 `ALTRun = exe 全路径`（`untUtilities.pas:115-146`）+ 公共启动文件夹快捷方式 `ALTRun.lnk`（`:148-170`）。`AutoRun` 默认 True，本机 1。
- **SendTo 集成**：在 `CSIDL_SENDTO` 建 `ALTRun.lnk`（`untUtilities.pas:335-343`）。从"发送到"调用时 `ParamStr(1)` = 文件路径：主程序在运行 → `SendMessage(WM_SETTEXT, 1, FileName)` 让主程序 `AddFileShortCut`；未运行 → 自己加载后添加再退出（`frmALTRun.pas:1946-2027`、`:2770-2785`）。`AddFileShortCut` 会**弹 `frmShortCut` 预填三个框让你确认**（`untShortCutMan.pas:377-475`）。
  - **只取第一个文件**：代码只读 `ParamStr(1)`，多选时其余参数被忽略。
  - **入口分支**（`frmALTRun.pas:1949-2027`）：先排除 `Restart`/`Clean` 两个特殊标志，再把窗口标题改成 `ALTRun - Add ShortCut`（`:1974`）。
    - **已有实例在跑** → `HandleID`（INI `[DEBUG]` 缓存的窗口句柄，本机 `67572`）→ `IsWindow` 校验 → `SendMessage(WM_SETTEXT, 1, 路径)`；接收端 `:2767-2777` 用 `WParam = 1` 甄别来源后调 `AddFileShortCut`。`SendMessage` **同步阻塞**——第二个进程要等弹窗流程走完才退出；**主窗口不会显示**（只有 `WM_ALTRUN_SHOW_WINDOW` 才显示）。
    - **没有实例在跑** → 自己 `LoadShortCutList` → `AddFileShortCut` → `Application.Terminate`，主界面全程不出现（一次性加项工具）。此分支在单实例互斥体检查（`:2029`）**之前**就 `Exit`，等于**不走单实例保护**。
  - **预填规则**（`ExtractShortCutItemFromFileName`，`untShortCutMan.pas:799-882`）：`.lnk` 先 `ResolveLink` 解真实目标，**两种例外保留 lnk 本身**（解出为空 / 解出 `%windir%\Installer\{...}` 的 MSI 图标 exe）；**关键字 = 文件名去扩展名**（目录不切扩展名；`http://` 取主机名；Chrome `--app=` 型 lnk 另走分支）；**名称 = 关键字**（同一值）；**命令行 = 完整路径**；**参数类型固定 `rgParam.ItemIndex := 0`（无参数）** → 从"发送到"进来的项**永远不带参数**。
  - **对话框 `frmShortCut`**：模态 + `fsStayOnTop` + `poScreenCenter` + `bsDialog`（494×257，标题"快捷项"，不可缩放）；`FormShow` 先 `SetForegroundWindow`，**焦点落在"快捷项"（关键字）输入框**（`frmShortCut.pas:258-262`）；"确定"是默认按钮（回车确认 / Esc 取消）。
  - **点确定后的连锁**（`untShortCutMan.pas:416-482`）：① 关键字或命令行**任一为空** → 弹"空行已添加."并降级成**空行分隔项**（`scBlank`，三字段清空）；② 命令行含 ALTRun 自身目录 → 弹"使用相对路径替换绝对路径吗？这样便于在U盘上使用"（本例几乎不触发）；③ **关键字重名** → 弹"同名快捷项已存在 [关键字 (名称) = 命令行]，是否替换?"（**只按关键字查重**——全字段严格查重那段已被注释掉，`:447-455`）；④ 通过 → `AppendShortCutItem`（`m_ShortCutList.Add`，**追加到列表末尾**、无 Freq）→ `SaveShortCutList` → `LoadShortCutList`，**运行中的实例立刻可搜到**；⑤ 取消 → 不写任何东西。
- **shell 右键菜单：源码里有，但是死代码（从未生效）**。`untUtilities.pas:300-333` 的 `AddMeToShortCutMenu` 写 `HKEY_CLASSES_ROOT\*\shell\Add To ALTRun\command` = `"<exe>" "%1"`；只注册 `*\shell`（所有文件），**没有** `Directory`/`Folder`/`Drive`。
  三条实测证据（2026-09-13 复核）：① 全源码树 `AddMeToShortCutMenu` 只有声明（`:31`）与定义（`:300`）两处，**零调用点**；② 本机注册表 `HKCR\*\shell` 与 `HKCU\Software\Classes\*\shell` 下均无 `Add To ALTRun`；③ 全树无 `.reg`/安装脚本（`Bin\` 里只有 `Clean.bat`）。
  **失效原因（推断，非实证）**：直接写 `HKEY_CLASSES_ROOT` 在 Vista+ 非管理员下会被 UAC 拦，而该函数用 `except Exit` 吞掉异常 → 静默失败；且配置界面只有 AutoRun 与 SendTo 两个勾选框，没有暴露它的开关（`untALTRunOption.pas:51`、`:170` 两个 KEY 就是全部）。
  **给实现者的结论**：这是"半成品遗留"，**别把它当成原版已有的功能去对齐**；MxRun 要做就注册到 `HKCU\Software\Classes\*\shell`（免管理员），不要照抄 HKCR 写法。
- **清理**：`ParamStr(1) = 'Clean'`（`Clean.bat`）→ 确认后取消自启 + 移出 SendTo + 退出（`:1962-1972`）。
- **重启**：`ParamStr(1) = 'Restart'` → `Sleep(2000)` + `ShellExecute(自己,'Restart')` + 关闭（`:1953-1957`、`:2620-2626`）。
- **托盘**：`ntfMain`（CoolTrayIcon），`OnClick = OnDblClick` → **单击即切换显隐**；首次单击弹渐隐提示（`dfm:8553-8556`、`:2543-2572`）。由 `ShowTrayIcon` 控制（本机 1）。
- **托盘右键菜单**（顺序）：显示 / 快捷项管理 / 配置 / 关于 / 退出（`dfm:8558-8573`），其中管理/配置带 `Alt+S`/`Alt+C` 快捷键。
- **列表右键菜单**：添加 / 编辑 / 删除 / `-` / 打开所在目录（最后一项仅在可定位目录时可见，`dfm:8661-8676`、`:2575-2579`）。
- **新建快捷项入口**：Insert 键 / 无结果时回车确认 / 右键"添加" / 管理器"添加" → 都落到 `frmShortCut` 对话框（另有第五条入口：SendTo / 命令行传入文件路径，见上）。`frmShortCut` 支持三种填路径方式：`File` 按钮、`Dir` 按钮、**从资源管理器拖文件进来**（仅取第 1 个，`frmShortCut.pas:61-107`、`:109-141`、`:255`、`:264-301`）。
  - 校验（仅 OK 时）：Name 不能含半角逗号；选"无参数"却在命令行写了占位符 → 询问是否改选参数类型（`:155-187`）。
  - `btnTest` 用当前值拼一个临时项执行（**不累加 Freq、不写 FavoriteList**，`:189-218`）。
- **快捷项管理器 `frmShortCutMan`**：4 列 `ShortCut`(100)/`Name`(100)/`Param Type`(100)/`Command Line`(400)，列宽持久化到 INI（`dfm:76-92`、`frmShortCutMan.pas:584-585`、`:613-614`）；`RowSelect`、`GridLines`、**无复选框列、无序号列**、不支持点列头排序。`Param Type` 列是字符串 `''`/`No_Encoding`/`URL_Query`/`UTF8_Query`。键盘：F2 编辑、Insert 添加、Delete 删除、双击编辑。工具栏：添加/编辑/删除/路径转换/校验/分隔/帮助/关闭/取消，**无快捷键**。
  - **已知缺陷**：拖拽重排丢 Freq；右键菜单项名与动作**相反**（`mniCut → actAdd`、`mniInsert → actEdit`，`dfm:666-674`）；"编辑时不动内容直接确定"会因重复检测把自己算作重复而**静默丢弃修改**（`:246-252`、`:511-532`）；"帮助"按钮是调试残留；新建行插在选中行**之前**；列表可内联改名且不走重复检测。
  - 确定/取消**对话框自己不落盘**，由调用方处理；但窗体几何与列宽在 `FormDestroy` 里**无条件写 INI**，"取消"挡不住。
  - **"校验"** 逐项跑 `IsValidCommandLine`，把无效项送进 `frmInvalid` 让用户勾选（默认全勾）后删除。
  - **"路径转换"**：`MB_YESNOCANCEL` —— YES = 绝对→相对（命令行含 exe 目录且不在首字符时才处理，替换成字面量 `.`），NO = 相对→绝对，CANCEL 退出；只改控件不落盘、无撤销（`:277-325`）。
- **执行计数彩蛋**：每执行满 10000 次弹"恭喜！您已经运行了 %d 次快捷项！"（`untShortCutMan.pas:2093-2099`）；本机 `ShortCutRunCount = 11593`。
- **主窗口不接受文件拖放**（`DragAcceptFiles` 只出现在 `frmShortCut.pas:255` 与 `frmShortCutMan.pas:571`）。
- **`frmInvalid`**：仅由管理器"校验"按钮打开，4 列 + 复选框，右键 = 全选/全不选/测试。

---

## 12. 与 AHK 版的显著差异（两边都已验证）

> **重要**：AHK 重写版（`ALTRun.ahk`，本机留存副本、不随仓库分发）不是对齐目标。下列差异说明为什么"按调研文档对齐"会跑偏。

| # | 维度 | Delphi 原版（权威） | AHK 重写版 |
|---|---|---|---|
| 1 | 参数输入 | 独立参数对话框 | 内联"关键字<空格>参数"，`ParseArg` 解析 |
| 2 | 占位符 | 六种（`%p`/`{%p}`/`{%c}`/`{%wd}`/`{%wt}`/`{%wc}`） | **一个都没有** |
| 3 | 剪贴板变量 | `{%c}`，热键按下瞬间取值 | 无 `{%c}` |
| 4 | 兜底 | **无** | 有（`+`/空格/`>` 前缀 + 无结果整表兜底） |
| 5 | 结果列表 | 单列 `TListBox`，序号写在行文本前两位 | 4 列 ListView |
| 6 | 序号基准 | 由 `IndexFrom0to9` 决定 | 恒从 1 开始 |
| 7 | 空格 | 恒等于"执行当前项" | 默认只输入空格，需开 `SpaceToRun` |
| 8 | 数字键 | Ctrl/Alt+数字执行；无修饰数字=上一次列表第 N 项；`;`/`'` = 第 2/3 项 | Alt+数字运行、Ctrl+数字仅选中 |
| 9 | Freq 落盘 | 只在退出/对话框确定时写回 | 每次执行成功即写 INI 并全量重载 |
| 10 | Top10 / 历史 | `ShowTopTen` 限 10 行 + `Ctrl+L` 最近列表 | 无 Top10 |
| 11 | 窗口位置 | 记忆 `WinTop`/`WinLeft` | 每次居中 |
| 12 | 匹配算法 | `*`→`.*`、`?`→`.`，**不做分词** | 折叠空白 + 多 token `.*` 连接（保序） |
| 13 | 命令行显示 | 底部常驻只读行 `CMD=...` | 状态栏显示命令片段 |
| 14 | 中键执行 | 默认开启 | 默认关闭 |
| 15 | 拼音匹配 | **没有** | 有 |
| 16 | 主窗拖放 | 无 | 无 |

---

## 附：给实现者的高风险清单

1. **`HotKey` 是死键**，生效的是 `HotKey1`（本机 `Alt+1`）——已实测复核。
2. **空输入框按空格/回车 = 执行第 1 项**，并会写一条空关键字进 `FavoriteList.txt`。
3. **评分 ≤ 0 的命中被静默丢弃**（命中位置越靠后越容易消失）。
4. **同分项顺序不确定**（快排随机支点）。
5. **`Freq` 只在退出/对话框确定时落盘**，崩溃即丢。
6. **`{%c}`/`{%wd}`/`{%wt}`/`{%wc}` 只在 `ParamType <> ptNone` 时替换**。
7. **多个占位符只替换第一个命中的**。
8. **`@+`/`@-`/`@` 必须按此顺序判定**。
9. 管理器拖拽丢 Freq、右键菜单名与动作相反、编辑不改动会被自身判重。
10. **改 `AlphaColor`/`Alpha`/`RoundBorderRadius`/`FormWidth`/`Lang` 会触发进程重启**，其余设置热生效。
11. `ShowShortCutButton` 默认 True，但本机为 0（三个按钮全隐藏）。
12. `LatestList` 存在 `[DEBUG]` 节（语义上属于用户数据）。
13. **`AddMeToShortCutMenu`（shell 右键菜单）是死代码**——零调用点、注册表无痕、无安装脚本，**别当已有功能去对齐**（详见 §11）。原版真正在用的加项通道是 **SendTo**（活的，`SendTo\ALTRun.lnk` 实测存在）。

---

## 附：MxRun 对齐分档

### ① 必须补（用户日常在用、MxRun 没有）

> **进度（2026-09-15）**：已经补上的有 —— **空格 = 执行当前项**、**参数机制**（形态按用户拍板改成顶框切换式，
> 不是弹框；见 `开发进度.md` §2.10）、**失焦立即隐藏**、**Esc 二级语义**、**`*` 前缀**、
> **单实例唤醒**（第二个实例静默交接后退出，见 §2.11）、**右键集成：SendTo + shell 菜单**（§2.11）、
> **无结果回车 → 添加它**（§2.11）、**F2 编辑**（§2.11）。
> **还没补的**：序号、Ctrl/Alt+数字与 `;`/`'` 直接执行第 N 项、窗体级快捷键里除 F2 以外的那些
> （Insert/Delete/Ctrl+D/Ctrl+C/Ctrl+L/Tab/首尾循环）、列表点击语义（单击选中 + 双击/中键执行）、
> 窗口拖动、位置记忆、托盘单击、`{%c}` 在热键按下瞬间取值（现在是执行瞬间）。

| 交互 | 说明 |
|---|---|
| **空格 = 执行当前项** | 空输入按空格 = 执行第 1 项。这是 AltRun 最核心的手感 |
| **参数对话框 + 六个占位符** | 本机 72 条中约 14 条依赖（`{%p}` 2、`{%c}` 2、`{%wd}` 2、搜索引擎 8）。**是弹框，不是行内** |
| **失焦立即隐藏** | 点到别处即消失 |
| **Esc 二级语义** | 输入非空 → 清空；输入为空 → 才隐藏 |
| **序号 + `*` 前缀** | 序号写在前两位；`*` = 带参数（会弹框） |
| **Ctrl/Alt+数字**、`;`=第2项、`'`=第3项 | 直接执行第 N 项 |
| **窗体级快捷键** | F2 编辑 / Insert 新建 / Delete 删除 / Ctrl+D 打开目录 / Ctrl+C 复制命令行 / Ctrl+L 最近列表 / Tab=↓、Shift+Tab=↑ / ↑↓ 首尾循环 |
| **列表点击语义** | 单击 = 选中、双击 = 执行、中键 = 执行（MxRun 现为悬停即选中、单击即执行） |
| **窗口拖动** | `ReleaseCapture + SC_DRAGMOVE` |
| **位置记忆**（WinTop/WinLeft） | |
| **单实例唤醒** | 给已有窗口发自定义消息后自己退出（比弹框提示好） |
| **托盘单击**切换显隐 | MxRun 现为双击 |
| **无结果回车 → "添加它?"** | 快捷项增长的主入口 |
| **右键集成：SendTo + shell 菜单** | 用户日常在用的加项通道（本机 `SendTo\ALTRun.lnk` 实测存在、`[Config] AddToSendTo=1`），MxRun **一条入口都没有**。原版的 shell 菜单部分**是死代码**，别照抄 `HKCR` 写法 → 用 `HKCU\Software\Classes\*\shell`（免管理员） |
| `{%c}` 在**热键按下瞬间**取剪贴板 | 不是执行瞬间 |

### ② 故意不照搬（MxRun 已更好，或那边是缺陷）

- **排序**：AltRun 会静默丢弃低分命中，且同分顺序随机 → MxRun 的 frecency + nucleo 更准且确定。
- **匹配范围**：AltRun 只匹配关键字 → MxRun 匹配更多（但也说明需要 alias 字段来精确对齐"打 b 出百度"）。
- **拼音**：Delphi 版没有，MxRun 有 → 保留。
- **统计落盘**：AltRun 崩溃即丢 → redb 事务是正解（印证设计文档"治丢数据"的判断）。
- **皮肤**：`BG.jpg` + 颜色键透明 → 应改用 Acrylic（见 README 的 E2 结论）。
- 管理器的那些已知缺陷（拖拽丢 Freq、菜单名与动作相反等）一律不照搬。

### ③ 待定（需用户拍板）

呼出键默认值（`Alt+1` vs `Alt+F1`）；副热键要不要；命令行常驻显示；空输入 Top10 上限；序号基准；列表行数上限 10；执行后是否退程序；弹出音；"执行满 10000 次"彩蛋要不要致敬。

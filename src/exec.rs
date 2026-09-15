//! Executor (P0-2): turns a stored [`Action`] into something that actually
//! happens.
//!
//! The file is split in two on purpose:
//!
//! - **Pure helpers** — argument encoding, placeholder insertion, `%VAR%`
//!   expansion, relative-path detection. No OS calls, unit tested.
//! - **The OS layer** — `ShellExecuteW`, `CreateProcessW`, the clipboard, the
//!   foreground window, and the built-in verbs. Thin, reached only from
//!   [`run`].
//!
//! ## Why two launch primitives
//!
//! `Open` goes through `ShellExecuteW`, because that is the only call that
//! understands documents, folders, URLs, protocols, `.msc`/`.cpl` and CLSID
//! paths — none of which are executables. `Run` goes through
//! `CreateProcessW`, because a command line with arguments is *not* a file
//! name: AltRun handed the whole string to `ShellExecute` and only worked
//! because its `WinExec` fallback caught the failure (see
//! `untShortCutMan.pas:341-350`). Splitting by effect removes that trap.
//!
//! ## Arguments
//!
//! [`ArgSource::Prompt`] takes its value from the inline prompt (P1-1): the UI
//! collects the text and passes it to [`run_with_arg`]. Called through plain
//! [`run`] there is nobody to ask, so it yields [`Outcome::NeedsInput`] — which
//! is exactly the signal that makes the UI open the prompt. The other sources
//! (clipboard, foreground window) need no typing and work either way.
//!
//! Legacy `{%c}` placeholders are handled too: AltRun's clipboard items are
//! stored as `http://…?wd={%c}`, so `Replace` mode fills in whichever marker
//! the string actually contains.

use crate::store::{
    Action, ArgSource, ArgSpec, BuiltinVerb, Effect, Encoder, InsertMode, Item, LaunchMode,
};
use std::sync::Mutex;
use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND, LPARAM, WPARAM};
use windows::Win32::System::DataExchange::{
    CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
};
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::System::Threading::{
    CreateProcessW, PROCESS_INFORMATION, STARTF_USESHOWWINDOW, STARTUPINFOW,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowW, GetClassNameW, GetForegroundWindow, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, SendMessageW, SetForegroundWindow,
    ShowWindow, SW_HIDE, SW_MINIMIZE, SW_RESTORE, SW_SHOWMAXIMIZED, SW_SHOWMINIMIZED,
    SW_SHOWNORMAL, WM_COMMAND, SHOW_WINDOW_CMD,
};
use windows::core::{BOOL, PCWSTR, PWSTR};

/// Placeholder that receives the argument, in both syntaxes found in the wild.
pub const MARKER_NEW: &str = "{p}";
pub const MARKER_OLD: &str = "%p";
/// AltRun's clipboard marker: replaced with the clipboard text.
pub const MARKER_CLIPBOARD: &str = "{%c}";

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Started,
    /// The action needs a value nobody supplied yet: the UI answers by opening
    /// the parameter prompt, then re-runs it through [`run_with_arg`].
    NeedsInput,
    Failed(String),
}

// ---------------------------------------------------------------------------
// Pure helpers (unit tested below)
// ---------------------------------------------------------------------------

/// Encode an argument the way a search engine expects it.
///
/// Both encoders produce UTF-8 percent-encoding: AltRun's `URL_Query` and
/// `UTF8_Query` differed only because Delphi encoded the former from ANSI
/// bytes. Modern engines take UTF-8, so the split is kept for data
/// compatibility, not behaviour.
pub fn encode(value: &str, enc: Encoder) -> String {
    match enc {
        Encoder::Raw => value.to_string(),
        Encoder::UrlQuery | Encoder::Utf8Percent => percent_encode(value),
    }
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.as_bytes() {
        match *b {
            b' ' => out.push('+'),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'@' | b'.' | b'_' | b'-' => {
                out.push(*b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Put the argument into the target string.
///
/// `Replace` fills every placeholder the string contains (`{p}`, `%p`,
/// `{%c}`). AltRun replaced only the *first* one — deliberately not copied:
/// see `docs/命令模型设计.md` §4.4.
pub fn apply_insert(target: &str, value: Option<&str>, mode: InsertMode) -> String {
    let Some(value) = value else {
        return target.to_string();
    };
    match mode {
        InsertMode::None => target.to_string(),
        InsertMode::Replace => {
            let mut out = target.to_string();
            for marker in [MARKER_CLIPBOARD, MARKER_NEW, MARKER_OLD] {
                if out.contains(marker) {
                    out = out.replace(marker, value);
                }
            }
            out
        }
        // How AltRun's search engines work: prefix + encoded query, no marker.
        InsertMode::Append => format!("{target}{value}"),
    }
}

/// Expand `%VAR%` the way the shell does: unknown names are left untouched.
pub fn expand_env(input: &str) -> String {
    if !input.contains('%') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '%' {
            if let Some(close) = chars[i + 1..].iter().position(|c| *c == '%') {
                let name: String = chars[i + 1..i + 1 + close].iter().collect();
                if !name.is_empty() {
                    // std::env::var is case-insensitive on Windows, matching the shell.
                    if let Ok(value) = std::env::var(&name) {
                        out.push_str(&value);
                        i += close + 2;
                        continue;
                    }
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// AltRun resolved `.\`/`..\` against the launcher's own directory
/// (`untShortCutMan.pas:187-189`).
pub fn wants_launcher_dir(command: &str) -> bool {
    command.contains(".\\") || command.contains("..\\")
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---------------------------------------------------------------------------
// Foreground window snapshot
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
pub struct ForegroundInfo {
    pub hwnd: isize,
    pub title: String,
    pub class: String,
}

static FOREGROUND: Mutex<ForegroundInfo> = Mutex::new(ForegroundInfo {
    hwnd: 0,
    title: String::new(),
    class: String::new(),
});

/// The window the last `HideForegroundWindow` put away.
///
/// Restore has to use *this*, not the current snapshot: by the time the user
/// summons the launcher again the foreground window is whatever they clicked
/// on since. AltRun's bundled `WinCtl.exe UnHide` took no argument for the same
/// reason — it remembered internally.
static LAST_HIDDEN: Mutex<isize> = Mutex::new(0);

fn window_text(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn window_class(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// Capture the window the user was in. Must run **before** the launcher takes
/// focus, otherwise "the foreground window" is MxRun itself — AltRun took this
/// snapshot at hotkey-press time for the same reason (交互规格 §4).
pub fn remember_foreground() {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        return;
    }
    let info = ForegroundInfo {
        hwnd: hwnd.0 as isize,
        title: window_text(hwnd),
        class: window_class(hwnd),
    };
    if let Ok(mut slot) = FOREGROUND.lock() {
        *slot = info;
    }
}

fn foreground() -> ForegroundInfo {
    FOREGROUND.lock().map(|g| g.clone()).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Clipboard
// ---------------------------------------------------------------------------

/// `CF_UNICODETEXT` — spelled out so the `Win32_System_Ole` feature stays off.
const CF_UNICODETEXT: u32 = 13;

/// The clipboard is a shared, singly-owned resource: another process may hold
/// it for a moment, so opening it is retried briefly instead of failing.
fn open_clipboard_with_retry() -> bool {
    for _ in 0..10 {
        if unsafe { OpenClipboard(None) }.is_ok() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}

pub fn clipboard_text() -> Option<String> {
    unsafe {
        if !open_clipboard_with_retry() {
            return None;
        }
        let text = (|| -> Option<String> {
            if IsClipboardFormatAvailable(CF_UNICODETEXT).is_err() {
                return None;
            }
            let handle = GetClipboardData(CF_UNICODETEXT).ok()?;
            let ptr = GlobalLock(HGLOBAL(handle.0));
            if ptr.is_null() {
                return None;
            }
            let mut len = 0usize;
            let wide_ptr = ptr as *const u16;
            while *wide_ptr.add(len) != 0 {
                len += 1;
            }
            let slice = std::slice::from_raw_parts(wide_ptr, len);
            let out = String::from_utf16_lossy(slice);
            let _ = GlobalUnlock(HGLOBAL(handle.0));
            Some(out)
        })();
        let _ = CloseClipboard();
        text
    }
}

fn copy_to_clipboard(text: &str) -> Outcome {
    use windows::Win32::System::DataExchange::{EmptyClipboard, SetClipboardData};
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalUnlock as Unlock, GMEM_MOVEABLE};

    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = wide.len() * 2;
    unsafe {
        let handle = match GlobalAlloc(GMEM_MOVEABLE, bytes) {
            Ok(h) => h,
            Err(e) => return Outcome::Failed(format!("GlobalAlloc 失败：{e}")),
        };
        let ptr = GlobalLock(handle);
        if ptr.is_null() {
            return Outcome::Failed("GlobalLock 失败".into());
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr() as *const u8, ptr as *mut u8, bytes);
        let _ = Unlock(handle);
        if !open_clipboard_with_retry() {
            return Outcome::Failed("无法打开剪贴板".into());
        }
        let _ = EmptyClipboard();
        if SetClipboardData(CF_UNICODETEXT, Some(HANDLE(handle.0))).is_err() {
            let _ = CloseClipboard();
            return Outcome::Failed("写入剪贴板失败".into());
        }
        let _ = CloseClipboard();
    }
    Outcome::Started
}

// ---------------------------------------------------------------------------
// OS layer
// ---------------------------------------------------------------------------

fn show_flag(mode: LaunchMode) -> i32 {
    match mode {
        LaunchMode::Normal => SW_SHOWNORMAL.0,
        LaunchMode::Maximized => SW_SHOWMAXIMIZED.0,
        LaunchMode::Minimized => SW_SHOWMINIMIZED.0,
        LaunchMode::Hidden => SW_HIDE.0,
    }
}

/// Resolve the argument value, or report that the user must type one.
///
/// `typed` is the text from the prompt, and is only consulted for
/// [`ArgSource::Prompt`] — a clipboard or foreground-window item takes its
/// value from the system even while the user is looking at a prompt.
fn arg_value(arg: &ArgSpec, typed: Option<&str>) -> Result<Option<String>, Outcome> {
    let raw = match arg.source {
        ArgSource::None => return Ok(None),
        ArgSource::Prompt => match typed {
            Some(text) if !text.is_empty() => text.to_string(),
            // No prompt, or an empty one: the caller shows the prompt instead.
            _ => return Err(Outcome::NeedsInput),
        },
        ArgSource::Clipboard => clipboard_text().unwrap_or_default(),
        ArgSource::ForegroundId => {
            let fg = foreground();
            if fg.hwnd == 0 {
                return Err(Outcome::Failed("没有记录到前台窗口".into()));
            }
            fg.hwnd.to_string()
        }
        ArgSource::ForegroundTitle => foreground().title,
        ArgSource::ForegroundClass => foreground().class,
    };
    Ok(Some(encode(&raw, arg.encode)))
}

/// Build the final command string for an item: insert the argument, expand
/// environment variables. Returns the command plus an optional working
/// directory for relative commands.
pub fn build_command(
    target: &str,
    arg: &ArgSpec,
    typed: Option<&str>,
) -> Result<(String, Option<String>), Outcome> {
    let value = arg_value(arg, typed)?;
    let inserted = apply_insert(target, value.as_deref(), arg.insert);
    let command = expand_env(&inserted);
    let workdir = if wants_launcher_dir(&command) {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_string_lossy().into_owned()))
    } else {
        None
    };
    Ok((command, workdir))
}

fn shell_execute(target: &str, workdir: Option<&str>, mode: LaunchMode) -> Outcome {
    let op = wide("open");
    let file = wide(target);
    let dir = workdir.map(wide);
    let r = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(op.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            dir.as_ref().map_or(PCWSTR::null(), |d| PCWSTR(d.as_ptr())),
            SHOW_WINDOW_CMD(show_flag(mode)),
        )
    };
    // ShellExecuteW returns a value <= 32 on failure (no Result in the binding).
    if r.0 as isize > 32 {
        Outcome::Started
    } else {
        Outcome::Failed(format!("ShellExecute 失败（代码 {}）", r.0 as isize))
    }
}

fn create_process(command: &str, workdir: Option<&str>, mode: LaunchMode) -> Outcome {
    let mut cmdline: Vec<u16> = command.encode_utf16().chain(std::iter::once(0)).collect();
    let dir = workdir.map(wide);
    let si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        dwFlags: STARTF_USESHOWWINDOW,
        wShowWindow: show_flag(mode) as u16,
        ..Default::default()
    };
    let mut pi = PROCESS_INFORMATION::default();
    let r = unsafe {
        CreateProcessW(
            PCWSTR::null(),
            Some(PWSTR(cmdline.as_mut_ptr())),
            None,
            None,
            false,
            Default::default(),
            None,
            dir.as_ref().map_or(PCWSTR::null(), |d| PCWSTR(d.as_ptr())),
            &si,
            &mut pi,
        )
    };
    match r {
        Ok(()) => {
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(pi.hProcess);
                let _ = windows::Win32::Foundation::CloseHandle(pi.hThread);
            }
            Outcome::Started
        }
        Err(e) => Outcome::Failed(format!("启动进程失败：{e}")),
    }
}

/// `MIN_ALL` (0x1A3 = 419) sent to the shell tray window — the documented
/// "show desktop" / minimize-everything toggle, and what AltRun's bundled
/// `WinCtl.exe MinAll` did (`untShortCutMan.pas:1430`).
fn minimize_all() -> Outcome {
    const MIN_ALL: usize = 419;
    let class = wide("Shell_TrayWnd");
    let tray = match unsafe { FindWindowW(PCWSTR(class.as_ptr()), PCWSTR::null()) } {
        Ok(h) if !h.0.is_null() => h,
        _ => return Outcome::Failed("找不到任务栏窗口（Shell_TrayWnd）".into()),
    };
    unsafe { SendMessageW(tray, WM_COMMAND, Some(WPARAM(MIN_ALL)), Some(LPARAM(0))) };
    Outcome::Started
}

fn with_foreground_window<F: FnOnce(HWND)>(f: F) -> Outcome {
    let fg = foreground();
    if fg.hwnd == 0 {
        return Outcome::Failed("没有记录到前台窗口（需要在呼出前捕获）".into());
    }
    f(HWND(fg.hwnd as *mut core::ffi::c_void));
    Outcome::Started
}

/// Context handed to the `EnumWindows` callback for [`hide_others`].
struct HideOthersCtx {
    keep: isize,
    own_pid: u32,
    hidden: usize,
}

unsafe extern "system" fn hide_others_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = unsafe { &mut *(lparam.0 as *mut HideOthersCtx) };
    let raw = hwnd.0 as isize;
    // Leave the window the user wants to keep, plus anything invisible or
    // untitled (tool windows, IME helpers, the shell's own plumbing).
    if raw == ctx.keep || !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return BOOL(1);
    }
    if unsafe { GetWindowTextLengthW(hwnd) } == 0 {
        return BOOL(1);
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == ctx.own_pid {
        return BOOL(1);
    }
    // Minimize rather than hide: AltRun's WinCtl probably hid them outright,
    // but a minimized window can always be brought back from the taskbar,
    // whereas a hidden one is easy to lose.
    unsafe {
        let _ = ShowWindow(hwnd, SW_MINIMIZE);
    }
    ctx.hidden += 1;
    BOOL(1)
}

/// AltRun's `ShowOnly`: keep the remembered front window, minimize the rest.
fn hide_others() -> Outcome {
    let fg = foreground();
    if fg.hwnd == 0 {
        return Outcome::Failed("没有记录到前台窗口（需要在呼出前捕获）".into());
    }
    let mut ctx = HideOthersCtx { keep: fg.hwnd, own_pid: std::process::id(), hidden: 0 };
    unsafe {
        let _ = EnumWindows(Some(hide_others_cb), LPARAM(&mut ctx as *mut HideOthersCtx as isize));
    }
    if ctx.hidden == 0 {
        Outcome::Failed("没有其它可最小化的窗口".into())
    } else {
        Outcome::Started
    }
}

fn run_builtin(verb: BuiltinVerb, typed: Option<&str>) -> Outcome {
    match verb {
        BuiltinVerb::MinimizeAll | BuiltinVerb::ShowDesktop => minimize_all(),
        BuiltinVerb::HideOthers => hide_others(),
        BuiltinVerb::HideForegroundWindow => with_foreground_window(|hwnd| unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
            // Remember it so "restore" can find it again.
            if let Ok(mut slot) = LAST_HIDDEN.lock() {
                *slot = hwnd.0 as isize;
            }
        }),
        BuiltinVerb::ShowForegroundWindow => {
            let remembered = LAST_HIDDEN.lock().map(|v| *v).unwrap_or(0);
            if remembered != 0 {
                let hwnd = HWND(remembered as *mut core::ffi::c_void);
                unsafe {
                    // SW_RESTORE also brings back a minimized window.
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                    let _ = SetForegroundWindow(hwnd);
                }
                Outcome::Started
            } else {
                // Nothing was hidden yet: fall back to the snapshot.
                with_foreground_window(|hwnd| unsafe {
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                    let _ = SetForegroundWindow(hwnd);
                })
            }
        }
        BuiltinVerb::Shutdown => create_process("shutdown /s /t 5", None, LaunchMode::Hidden),
        BuiltinVerb::Reboot => create_process("shutdown /r /t 5", None, LaunchMode::Hidden),
        // AltRun's 「运行」: what the user typed *is* the command line. It is
        // the manual equivalent of a fallback — the user picks this item first
        // (see docs/命令模型设计.md §6.4). LaunchMode is forced to Normal: the
        // item's own `Hidden` would make the launched program invisible, and
        // for a "run what I type" command that is never what was meant.
        BuiltinVerb::RunInput => match typed_line(typed) {
            Some(line) => create_process(line, None, LaunchMode::Normal),
            None => Outcome::NeedsInput,
        },
    }
}

/// The command line to run for [`BuiltinVerb::RunInput`], if the user typed
/// one. Whitespace-only input counts as "nothing typed" — running it would just
/// flash an empty console.
fn typed_line(typed: Option<&str>) -> Option<&str> {
    typed.map(str::trim).filter(|s| !s.is_empty())
}

fn reveal(path: &str) -> Outcome {
    let op = wide("open");
    let file = wide("explorer.exe");
    let params = wide(&format!("/select,\"{}\"", expand_env(path)));
    let r = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(op.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR(params.as_ptr()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if r.0 as isize > 32 {
        Outcome::Started
    } else {
        Outcome::Failed(format!("无法定位：{path}"))
    }
}

/// Run one action. Pure dispatch — all OS work happens in the helpers above.
///
/// Items whose [`ArgSpec`] asks the user for a value cannot be run through
/// here: they return [`Outcome::NeedsInput`], the UI opens the prompt, and the
/// retry comes back through [`run_with_arg`].
pub fn run(item: &Item, action: &Action) -> Outcome {
    run_with_arg(item, action, None)
}

/// Same, with the text the user typed into the parameter prompt.
pub fn run_with_arg(item: &Item, action: &Action, typed: Option<&str>) -> Outcome {
    match &action.effect {
        Effect::Open { target } => match build_command(target, &item.arg, typed) {
            Ok((cmd, dir)) => shell_execute(&cmd, dir.as_deref(), item.launch),
            Err(outcome) => outcome,
        },
        Effect::Run { line } => match build_command(line, &item.arg, typed) {
            Ok((cmd, dir)) => create_process(&cmd, dir.as_deref(), item.launch),
            Err(outcome) => outcome,
        },
        Effect::Builtin { verb } => run_builtin(*verb, typed),
        Effect::Reveal { path } => reveal(path),
        Effect::Copy { text } => copy_to_clipboard(&expand_env(text)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_encoding_is_untouched() {
        assert_eq!(encode("hello world", Encoder::Raw), "hello world");
    }

    #[test]
    fn query_encoding_matches_the_alt_run_rules() {
        // Space becomes +, unreserved characters survive.
        assert_eq!(encode("a b", Encoder::UrlQuery), "a+b");
        assert_eq!(encode("a-b_c.d*e@f", Encoder::UrlQuery), "a-b_c.d*e@f");
        // UTF-8 percent encoding for everything else (including CJK).
        assert_eq!(encode("中", Encoder::Utf8Percent), "%E4%B8%AD");
        assert_eq!(encode("a/b?c=d", Encoder::UrlQuery), "a%2Fb%3Fc%3Dd");
    }

    #[test]
    fn insert_replaces_every_marker_syntax() {
        // The new syntax.
        assert_eq!(apply_insert("cmd /k {p}", Some("ping x"), InsertMode::Replace), "cmd /k ping x");
        // The old syntax still present in imported data.
        assert_eq!(apply_insert("cmd /k %p", Some("ping x"), InsertMode::Replace), "cmd /k ping x");
        // AltRun's clipboard form.
        assert_eq!(
            apply_insert("http://www.baidu.com/s?wd={%c}", Some("rust"), InsertMode::Replace),
            "http://www.baidu.com/s?wd=rust"
        );
        // AltRun replaced only the first marker; we replace all of them.
        assert_eq!(apply_insert("{p}-{p}", Some("x"), InsertMode::Replace), "x-x");
    }

    #[test]
    fn append_mode_is_the_search_engine_mechanism() {
        // http://www.baidu.com/s?wd= + encoded query
        assert_eq!(
            apply_insert("http://www.baidu.com/s?wd=", Some("rust+lang"), InsertMode::Append),
            "http://www.baidu.com/s?wd=rust+lang"
        );
        // No argument -> nothing is appended.
        assert_eq!(apply_insert("calc.exe", None, InsertMode::Append), "calc.exe");
    }

    #[test]
    fn env_expansion_leaves_unknown_names_alone() {
        // Use a variable Windows always defines, so the test needs no mutation
        // (`set_var` is unsafe in edition 2024 and would race other tests).
        let windir = std::env::var("WINDIR").expect("WINDIR is always set on Windows");
        assert_eq!(expand_env("%WINDIR%\\x"), format!("{windir}\\x"));
        // Lookup is case-insensitive, like the shell's.
        assert_eq!(expand_env("%windir%"), windir);
        // Unknown names survive verbatim (ExpandEnvironmentStringsW behaviour).
        assert_eq!(
            expand_env("%MXRUN_DEFINITELY_MISSING%\\x"),
            "%MXRUN_DEFINITELY_MISSING%\\x"
        );
        assert_eq!(expand_env("no percent here"), "no percent here");
    }

    #[test]
    fn relative_commands_get_the_launcher_dir() {
        assert!(wants_launcher_dir(r".\ALTRun.ini"));
        assert!(wants_launcher_dir(r"@.\WinCtl.exe MinAll"));
        assert!(wants_launcher_dir(r"..\tool.exe"));
        assert!(!wants_launcher_dir(r"C:\Windows\notepad.exe"));
    }

    /// The full path an imported AltRun row takes: `cmd /k %p` needs input, so
    /// it must report NeedsInput rather than silently running the marker.
    #[test]
    fn prompt_source_reports_needs_input() {
        let arg = ArgSpec { source: ArgSource::Prompt, encode: Encoder::Raw, insert: InsertMode::Replace };
        assert_eq!(build_command("cmd /k {p}", &arg, None), Err(Outcome::NeedsInput));
        // An empty prompt is the same as no prompt: asking again beats running
        // `cmd /k ` with nothing after it.
        assert_eq!(build_command("cmd /k {p}", &arg, Some("")), Err(Outcome::NeedsInput));
    }

    /// Once the prompt hands the text over, it lands in the marker.
    #[test]
    fn prompt_text_fills_the_marker() {
        let arg = ArgSpec { source: ArgSource::Prompt, encode: Encoder::Raw, insert: InsertMode::Replace };
        let (cmd, dir) = build_command("cmd /k {p}", &arg, Some("ping 127.0.0.1")).unwrap();
        assert_eq!(cmd, "cmd /k ping 127.0.0.1");
        assert!(dir.is_none());
    }

    /// The search-template shape: no marker, the encoded query is appended.
    #[test]
    fn prompt_text_is_encoded_for_search_engines() {
        let arg = ArgSpec {
            source: ArgSource::Prompt,
            encode: Encoder::UrlQuery,
            insert: InsertMode::Append,
        };
        let (cmd, _) = build_command("http://www.baidu.com/s?wd=", &arg, Some("rust 教程")).unwrap();
        assert_eq!(cmd, "http://www.baidu.com/s?wd=rust+%E6%95%99%E7%A8%8B");
    }

    /// `运行` runs exactly what was typed — no encoding, no marker.
    #[test]
    fn typed_line_trims_and_rejects_blank() {
        assert_eq!(typed_line(Some("  notepad  ")), Some("notepad"));
        assert_eq!(typed_line(Some("   ")), None);
        assert_eq!(typed_line(None), None);
    }

    #[test]
    fn run_input_without_text_asks_for_it() {
        assert_eq!(run_builtin(BuiltinVerb::RunInput, None), Outcome::NeedsInput);
        assert_eq!(run_builtin(BuiltinVerb::RunInput, Some("  ")), Outcome::NeedsInput);
    }

    /// A clipboard search engine needs no typing: clipboard -> encode -> append.
    #[test]
    fn clipboard_search_builds_without_input() {
        let arg = ArgSpec {
            source: ArgSource::Clipboard,
            encode: Encoder::Utf8Percent,
            insert: InsertMode::Replace,
        };
        // Clipboard is empty in a test run, so only the shape is asserted.
        let (cmd, dir) = build_command("http://x/?q={%c}", &arg, None).expect("no input needed");
        assert!(cmd.starts_with("http://x/?q="));
        assert!(dir.is_none());
    }

    /// End-to-end: the typed parameter really reaches a real process. The
    /// command writes it to a temp file, so the assertion is on the *side
    /// effect*, not on our own string handling — this is the test that would
    /// catch "the prompt collected text but never passed it on".
    #[test]
    fn typed_parameter_reaches_a_real_process() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let out = std::env::temp_dir().join(format!("mxrun-param-{nanos}.txt"));
        let _ = std::fs::remove_file(&out);

        let item = Item {
            id: "test-param".into(),
            title: "参数端到端".into(),
            subtitle: String::new(),
            keywords: Vec::new(),
            actions: vec![Action::run(format!("cmd /c echo {{p}} > \"{}\"", out.display()))],
            arg: ArgSpec {
                source: ArgSource::Prompt,
                encode: Encoder::Raw,
                insert: InsertMode::Replace,
            },
            launch: LaunchMode::Hidden,
            source: Default::default(),
            health: Default::default(),
        };
        let action = item.default_action().cloned().unwrap();

        // Without the prompt text it must refuse...
        assert_eq!(run(&item, &action), Outcome::NeedsInput);
        // ...and with it, write the file.
        assert_eq!(
            run_with_arg(&item, &action, Some("hello-param")),
            Outcome::Started
        );

        // The child runs asynchronously; give it a moment.
        let mut body = None;
        for _ in 0..50 {
            if let Ok(text) = std::fs::read_to_string(&out) {
                body = Some(text);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let _ = std::fs::remove_file(&out);
        let body = body.expect("the parameterised command never produced its file");
        assert_eq!(body.trim(), "hello-param");
    }

    /// End-to-end: the Run path really starts a process. `cmd /c exit` is
    /// side-effect free, and Hidden keeps a console from flashing.
    #[test]
    fn run_path_starts_a_real_process() {
        assert_eq!(
            create_process("cmd /c exit 0", None, LaunchMode::Hidden),
            Outcome::Started
        );
    }

    /// End-to-end: the Open path reports the shell's failure code instead of
    /// pretending it worked (the v1 code called `open::that` and only logged).
    #[test]
    fn open_path_reports_shell_failure() {
        match shell_execute("mxrun-no-such-target-9f3a", None, LaunchMode::Hidden) {
            Outcome::Failed(why) => assert!(why.contains("ShellExecute"), "{why}"),
            other => panic!("junk target must fail, got {other:?}"),
        }
    }

    /// The whole chain for an AltRun-style window action: the item needs no
    /// typing, and its {%wd} comes from the captured foreground window.
    #[test]
    fn foreground_verb_without_snapshot_fails_cleanly() {
        // No snapshot was taken in a test process, so this must not panic.
        match run_builtin(BuiltinVerb::HideForegroundWindow, None) {
            Outcome::Failed(_) | Outcome::Started => {}
            other => panic!("unexpected outcome {other:?}"),
        }
    }

    // ---- helpers for the tests that touch real windows --------------------

    fn pid_of_window(hwnd: isize) -> Option<u32> {
        use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(HWND(hwnd as *mut core::ffi::c_void), Some(&mut pid)) };
        (pid != 0).then_some(pid)
    }

    fn image_name_of_pid(pid: u32) -> Option<String> {
        use windows::Win32::System::Threading::{
            OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        };
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let r = unsafe { QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len) };
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(handle) };
        r.ok()?;
        let full = String::from_utf16_lossy(&buf[..len as usize]);
        full.rsplit(['\\', '/']).next().map(|s| s.to_lowercase())
    }

    fn kill_pid(pid: u32) {
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
        if let Ok(handle) = unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) } {
            unsafe {
                let _ = TerminateProcess(handle, 0);
                let _ = windows::Win32::Foundation::CloseHandle(handle);
            }
        }
    }

    fn is_visible(hwnd: isize) -> bool {
        unsafe {
            windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(HWND(
                hwnd as *mut core::ffi::c_void,
            ))
        }
        .as_bool()
    }

    /// End-to-end over a **real window**, using the verbs themselves: start
    /// notepad through our own Run path, snapshot it the way the app does on
    /// show, then hide and re-show it and ask Win32 whether that happened.
    ///
    /// Skipped — never failed — when notepad does not reach the foreground:
    /// hiding whatever *is* in front (the terminal, say) would be an
    /// unpleasant surprise.
    #[test]
    fn window_verbs_hide_and_show_a_real_window() {
        if create_process("notepad.exe", None, LaunchMode::Normal) != Outcome::Started {
            eprintln!("skip: could not start notepad");
            return;
        }

        let mut pid = None;
        for _ in 0..25 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if let Some(p) = pid_of_window(unsafe { GetForegroundWindow() }.0 as isize) {
                if image_name_of_pid(p).as_deref() == Some("notepad.exe") {
                    pid = Some(p);
                    break;
                }
            }
        }
        let Some(pid) = pid else {
            eprintln!("skip: notepad never reached the foreground (nothing was hidden)");
            return;
        };

        // What show_window() does before the launcher takes focus.
        remember_foreground();
        let info = foreground();
        assert!(info.hwnd != 0, "snapshot taken");
        assert_eq!(pid_of_window(info.hwnd), Some(pid), "snapshot is notepad");

        assert_eq!(run_builtin(BuiltinVerb::HideForegroundWindow, None), Outcome::Started);
        assert!(!is_visible(info.hwnd), "notepad must be hidden");
        // Restore must find it through the "last hidden" memory, not through a
        // fresh snapshot — that is the whole point of the remembered handle.
        assert_eq!(
            LAST_HIDDEN.lock().map(|v| *v).unwrap_or(0),
            info.hwnd,
            "the hidden window must be remembered"
        );
        assert_eq!(run_builtin(BuiltinVerb::ShowForegroundWindow, None), Outcome::Started);
        assert!(is_visible(info.hwnd), "notepad must be visible again");

        kill_pid(pid);
    }

    /// The `{%c}` path, end to end: write through our own copy action, read
    /// back through the clipboard reader the executor uses.
    ///
    /// Skipped when the clipboard holds no text, so a copied image is never
    /// thrown away by a test run.
    #[test]
    fn clipboard_round_trip() {
        let Some(saved) = clipboard_text() else {
            eprintln!("skip: clipboard holds no text to restore");
            return;
        };
        let probe = "mxrun-clipboard-test-中文-42";
        assert_eq!(copy_to_clipboard(probe), Outcome::Started);
        assert_eq!(clipboard_text().as_deref(), Some(probe));
        // Put the user's clipboard back.
        assert_eq!(copy_to_clipboard(&saved), Outcome::Started);
        assert_eq!(clipboard_text().as_deref(), Some(saved.as_str()));
    }
}

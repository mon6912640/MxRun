#![windows_subsystem = "windows"]

//! MxRun MVP - a modern keyboard launcher.
//!
//! Core loop: global hotkey (default Alt+F1, configurable) toggles a
//! borderless, translucent, rounded egui window. Keystrokes are fuzzy-matched
//! with nucleo (scored, not just filtered), ranked with frecency, and rendered
//! with matched characters highlighted. Enter executes, Esc hides, F2 opens
//! the in-window settings view.
//!
//! Event architecture: global hotkey / tray / tray-menu events are polled on
//! a DEDICATED BACKGROUND THREAD (not inside `App::ui`), because eframe's
//! event loop sleeps while the window is hidden — polling inside `ui()`
//! would silently stop and the window could never be re-shown. The thread
//! pushes viewport commands into a cloned `egui::Context` (plus an
//! `Arc<AtomicBool>` to request the settings view) and calls
//! `request_repaint` to wake the UI.

mod exec;
mod import;
mod store;

use eframe::egui;
use egui::{Color32, FontId, Key, RichText, TextFormat, text::LayoutJob};
use global_hotkey::{
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
    hotkey::{Code, HotKey, Modifiers},
};
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{CaseMatching, Normalization, Pattern},
};
use pinyin::ToPinyin;
use std::os::windows::ffi::OsStrExt;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use store::{Action, BuiltinVerb, Effect, Item, PARAM_HISTORY_LIMIT, Store, frecency_bonus};
use tray_icon::{
    TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuItem},
};

const MAX_ROWS: usize = 8;
const DEFAULT_HOTKEY: &str = "Alt+F1";

/// How much of the best score a row needs to stay on screen.
///
/// With a realistic list a short query fuzzy-matches almost everything, because
/// nucleo happily finds subsequences inside the pinyin expansion ("dos" matches
/// "Win**d**ows" → d-o-s). The real hit still wins by an order of magnitude, so
/// the long tail is dropped rather than displayed. Measured with the sample
/// profile: `dos` scored 1088 for the intended row and 16–63 for the noise.
const SCORE_GATE: f64 = 0.15;

/// Append a timestamped line to %APPDATA%\MxRun\mxrun.log (debug aid).
fn log_line(msg: &str) {
    let base = std::env::var("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    let path = base.join("MxRun").join("mxrun.log");
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| {
            use std::io::Write;
            writeln!(f, "[{ts}] {msg}")
        });
}

// ---------- startup guards ----------
//
// A launcher is started by double-clicking, so "launch it again" is a normal
// user action rather than a developer mistake. It must never be a silent
// death: this binary is built with `windows_subsystem = "windows"`, so a
// panic at startup prints to a stderr nobody owns — the user just sees
// nothing happen.

/// Process-wide mutex guarding the database file.
const INSTANCE_MUTEX: &str = "MxRun.SingleInstance.v1";

/// Keeps the instance mutex alive for the whole process (see below).
static INSTANCE_HANDLE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

/// Take the single-instance lock; `false` means another MxRun already owns it.
///
/// Required because redb opens the database with an exclusive file lock: a
/// second process cannot open it, and it used to die on `.expect()` with
/// "另一个程序已锁定文件的一部分". The handle is deliberately kept in a
/// static — dropping it would destroy the mutex while we are still running.
/// The OS releases it when we exit, crashes included.
fn acquire_single_instance() -> bool {
    let name: Vec<u16> = INSTANCE_MUTEX
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        // CreateMutexW leaves the last error untouched on success, so clear it
        // first — otherwise we might read a stale ERROR_ALREADY_EXISTS.
        SetLastError(ERROR_SUCCESS);
        match CreateMutexW(None, true, PCWSTR(name.as_ptr())) {
            Ok(handle) => {
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    false
                } else {
                    let _ = INSTANCE_HANDLE.set(handle.0 as usize);
                    true
                }
            }
            Err(e) => {
                // Can't even create a mutex — don't refuse to start over it.
                log_line(&format!("single-instance mutex unavailable: {e}"));
                true
            }
        }
    }
}

/// Show a blocking Win32 dialog. The only way to tell the user anything
/// before the egui window exists (there is no console to print to).
fn message_box(text: &str, title: &str, is_error: bool) {
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let text = wide(text);
    let title = wide(title);
    let icon = if is_error {
        MB_ICONERROR
    } else {
        MB_ICONINFORMATION
    };
    unsafe {
        let _ = MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | icon | MB_SETFOREGROUND | MB_TOPMOST,
        );
    }
}

/// Run `--import` and report through a dialog plus the log.
fn run_import(cli: import::CliImport) {
    log_line(&format!(
        "import: start path={} curated={}",
        cli.path.display(),
        cli.curated
    ));
    let mut store = match Store::open() {
        Ok(s) => s,
        Err(e) => {
            log_line(&format!("import: cannot open store: {e}"));
            message_box(&format!("无法打开数据文件：{e}"), "MxRun 导入", true);
            return;
        }
    };
    for w in &store.warnings {
        log_line(&format!("store: {w}"));
    }
    match import::import_file(&mut store, &cli.path, cli.curated) {
        Ok(report) => {
            for note in &report.notes {
                log_line(&format!("import: {note}"));
            }
            log_line(&format!(
                "import: lines={} commands={} imported={} needs_input={} builtin={} removed_seed={} skipped(sep={} unwanted={} unsupported={})",
                report.lines,
                report.commands,
                report.imported,
                report.needs_input,
                report.builtin,
                report.removed_seed,
                report.skipped_separator,
                report.skipped_unwanted,
                report.skipped_unsupported
            ));
            message_box(
                &format!(
                    "{}\n来源：{}",
                    report.summary(),
                    cli.path.display()
                ),
                "MxRun 导入完成",
                false,
            );
        }
        Err(e) => {
            log_line(&format!("import: failed: {e}"));
            message_box(
                &format!("导入失败：{e}\n\n来源：{}", cli.path.display()),
                "MxRun 导入",
                true,
            );
        }
    }
}

fn main() -> eframe::Result<()> {
    // Anchor the clock the wake probe and the icon timings both read.
    PROC_START.get_or_init(std::time::Instant::now);
    if !acquire_single_instance() {
        log_line("startup: another instance is already running, exiting");
        message_box(
            "MxRun 已经在运行。\n\n请到系统托盘找到它（双击图标呼出），或按你自己设置的呼出快捷键。",
            "MxRun",
            false,
        );
        return Ok(());
    }

    // Import mode runs headless: no window, a summary dialog, everything also
    // written to mxrun.log (a GUI-subsystem binary has no console to print to).
    if let Some(cli) = import::parse_args(std::env::args().skip(1)) {
        run_import(cli);
        return Ok(());
    }

    // Pay the one-off shell-imaging initialisation (~145 ms, measured) while
    // eframe is still creating its window and GL context. Our own code does not
    // run until that finishes, so without this the cost lands *after* the first
    // frame and the list shows emoji for ~200 ms before the real icons arrive.
    std::thread::spawn(|| {
        com_init_thread();
        let _ = icon_pixels(std::path::Path::new("cmd.exe"), 16);
    });

    // Renderer: Glow (OpenGL) by default, chosen by measurement (release build,
    // same binary, only the backend differing — see the E1 notes in README):
    //   glow: 157 MB private commit, 27 threads, 493 handles
    //   wgpu: 464 MB private commit, 55 threads, 881 handles
    // Output was pixel-identical and the hotkey->first-frame latency the same
    // (1.8-5.9 ms). MXRUN_RENDERER=wgpu is the escape hatch if OpenGL misbehaves
    // on another GPU/driver.
    let (renderer, renderer_name) = match std::env::var("MXRUN_RENDERER").as_deref() {
        Ok("wgpu") => (eframe::Renderer::Wgpu, "wgpu"),
        _ => (eframe::Renderer::Glow, "glow"),
    };
    log_line(&format!(
        "renderer={renderer_name} (MXRUN_RENDERER={:?})",
        std::env::var("MXRUN_RENDERER")
    ));

    let options = eframe::NativeOptions {
        renderer,
        viewport: egui::ViewportBuilder::default()
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_inner_size([680.0, 400.0])
            .with_min_inner_size([480.0, 200.0]),
        ..Default::default()
    };
    eframe::run_native(
        "MxRun",
        options,
        Box::new(|cc| match MxRunApp::new(cc) {
            Ok(app) => Ok(Box::new(app)),
            Err(msg) => {
                log_line(&format!("startup failed: {msg}"));
                message_box(
                    &format!("MxRun 启动失败：\n\n{msg}\n\n数据目录：%APPDATA%\\MxRun"),
                    "MxRun 启动失败",
                    true,
                );
                std::process::exit(1);
            }
        }),
    )
}

// ---------- hotkey parsing / capturing ----------

/// Parse a human string like "Alt+F1" / "Ctrl+Shift+K" into a HotKey.
fn parse_hotkey(s: &str) -> Option<HotKey> {
    let mut mods = Modifiers::empty();
    let mut key: Option<Code> = None;
    for part in s.split('+') {
        match part.trim().to_ascii_lowercase().as_str() {
            "alt" => mods |= Modifiers::ALT,
            "ctrl" | "control" => mods |= Modifiers::CONTROL,
            "shift" => mods |= Modifiers::SHIFT,
            "win" | "super" | "meta" => mods |= Modifiers::SUPER,
            k => key = Some(parse_key_code(k)?),
        }
    }
    let mods = if mods.is_empty() { None } else { Some(mods) };
    Some(HotKey::new(mods, key?))
}

fn parse_key_code(k: &str) -> Option<Code> {
    let k = k.trim().to_ascii_lowercase();
    if k.len() == 1 {
        let c = k.chars().next()?;
        if c.is_ascii_alphabetic() {
            // Code::KeyA .. KeyZ
            return Some(match c {
                'a' => Code::KeyA, 'b' => Code::KeyB, 'c' => Code::KeyC, 'd' => Code::KeyD,
                'e' => Code::KeyE, 'f' => Code::KeyF, 'g' => Code::KeyG, 'h' => Code::KeyH,
                'i' => Code::KeyI, 'j' => Code::KeyJ, 'k' => Code::KeyK, 'l' => Code::KeyL,
                'm' => Code::KeyM, 'n' => Code::KeyN, 'o' => Code::KeyO, 'p' => Code::KeyP,
                'q' => Code::KeyQ, 'r' => Code::KeyR, 's' => Code::KeyS, 't' => Code::KeyT,
                'u' => Code::KeyU, 'v' => Code::KeyV, 'w' => Code::KeyW, 'x' => Code::KeyX,
                'y' => Code::KeyY, 'z' => Code::KeyZ,
                _ => unreachable!(),
            });
        }
        if c.is_ascii_digit() {
            return Some(match c {
                '0' => Code::Digit0, '1' => Code::Digit1, '2' => Code::Digit2,
                '3' => Code::Digit3, '4' => Code::Digit4, '5' => Code::Digit5,
                '6' => Code::Digit6, '7' => Code::Digit7, '8' => Code::Digit8,
                '9' => Code::Digit9,
                _ => unreachable!(),
            });
        }
    }
    match k.as_str() {
        "f1" => Some(Code::F1), "f2" => Some(Code::F2), "f3" => Some(Code::F3),
        "f4" => Some(Code::F4), "f5" => Some(Code::F5), "f6" => Some(Code::F6),
        "f7" => Some(Code::F7), "f8" => Some(Code::F8), "f9" => Some(Code::F9),
        "f10" => Some(Code::F10), "f11" => Some(Code::F11), "f12" => Some(Code::F12),
        "space" => Some(Code::Space),
        "tab" => Some(Code::Tab),
        _ => None,
    }
}

/// egui key -> display name table for hotkey capture.
const CAPTURE_KEYS: &[(Key, &str)] = &[
    (Key::F1, "F1"), (Key::F2, "F2"), (Key::F3, "F3"), (Key::F4, "F4"),
    (Key::F5, "F5"), (Key::F6, "F6"), (Key::F7, "F7"), (Key::F8, "F8"),
    (Key::F9, "F9"), (Key::F10, "F10"), (Key::F11, "F11"), (Key::F12, "F12"),
    (Key::A, "A"), (Key::B, "B"), (Key::C, "C"), (Key::D, "D"), (Key::E, "E"),
    (Key::F, "F"), (Key::G, "G"), (Key::H, "H"), (Key::I, "I"), (Key::J, "J"),
    (Key::K, "K"), (Key::L, "L"), (Key::M, "M"), (Key::N, "N"), (Key::O, "O"),
    (Key::P, "P"), (Key::Q, "Q"), (Key::R, "R"), (Key::S, "S"), (Key::T, "T"),
    (Key::U, "U"), (Key::V, "V"), (Key::W, "W"), (Key::X, "X"), (Key::Y, "Y"),
    (Key::Z, "Z"),
    (Key::Num0, "0"), (Key::Num1, "1"), (Key::Num2, "2"), (Key::Num3, "3"),
    (Key::Num4, "4"), (Key::Num5, "5"), (Key::Num6, "6"), (Key::Num7, "7"),
    (Key::Num8, "8"), (Key::Num9, "9"),
    (Key::Space, "Space"), (Key::Tab, "Tab"),
];

/// Capture the next key combo from egui input. Returns "Alt+F1"-style text.
fn capture_hotkey(ctx: &egui::Context) -> Option<String> {
    let mods = ctx.input(|i| i.modifiers);
    for (key, name) in CAPTURE_KEYS {
        if ctx.input(|i| i.key_pressed(*key)) {
            let mut s = String::new();
            if mods.ctrl {
                s.push_str("Ctrl+");
            }
            if mods.alt {
                s.push_str("Alt+");
            }
            if mods.shift {
                s.push_str("Shift+");
            }
            s.push_str(name);
            return Some(s);
        }
    }
    None
}

// ---------- window show / hide helpers (callable from any thread) ----------
//
// Visibility is tracked in our own atomic, NOT via egui's ViewportInfo:
// `ViewportInfo::visible()` proved unreliable in egui 0.36 (always None).
//
// Show/hide/close use DIRECT WIN32 CALLS (ShowWindow / SetWindowPos /
// PostMessage) on our own HWND. We deliberately do NOT use
// `Context::send_viewport_cmd` for visibility: in eframe 0.36 viewport
// commands issued from a background thread (and even from deferred UI
// frames) were silently dropped, producing the "hotkey can show but never
// hide" bug. Win32 ShowWindow is thread-safe and deterministic.

use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_SUCCESS, GetLastError, HWND, LPARAM, SetLastError, WPARAM,
};
use windows::Win32::Graphics::Dwm::{DWMWINDOWATTRIBUTE, DwmSetWindowAttribute};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, PCWSTR};

static WINDOW_VISIBLE: AtomicBool = AtomicBool::new(true);
static MAIN_HWND: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

// --- E1a wake-latency probe (temporary instrumentation) ---
//
// A launcher's real hot path is "hidden -> visible", not cold start: the user
// presses the hotkey dozens of times a day. This measures how long that takes,
// from the moment we decide to show the window to the first frame egui
// actually renders — i.e. how long eframe takes to wake from its sleep.
static PROC_START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
static WAKE_T0_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Microseconds since the process started. `main` anchors the clock so both the
/// wake probe and the icon timings share one origin (lazily initialising it here
/// would make whichever caller ran first read 0).
fn us_since_start() -> u64 {
    PROC_START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_micros() as u64
}

fn window_visible() -> bool {
    WINDOW_VISIBLE.load(Ordering::Relaxed)
}

struct EnumCtx {
    pid: u32,
    found: Option<HWND>,
}

unsafe extern "system" fn enum_windows_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = unsafe { &mut *(lparam.0 as *mut EnumCtx) };
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    // Our process owns several top-level windows (winit's "Thread Event
    // Target" helper is ALSO visible and owns no title) — the main window is
    // the one titled "MxRun".
    //
    // The title check alone is the discriminator. Do NOT also require
    // IsWindowVisible: that filter made main_hwnd() fail exactly when it was
    // needed — on the first frame (winit has not shown the window yet, so the
    // initial centering silently did nothing) and while the window is hidden
    // (the handle survives only because of the MAIN_HWND cache).
    if pid == ctx.pid {
        let mut text = [0u16; 256];
        let len = unsafe { GetWindowTextW(hwnd, &mut text) } as usize;
        let title = String::from_utf16_lossy(&text[..len.min(256)]);
        if title == "MxRun" {
            ctx.found = Some(hwnd);
            return BOOL(0); // stop enumerating
        }
    }
    BOOL(1)
}

/// Locate (and cache) our main window handle.
fn main_hwnd() -> Option<HWND> {
    if let Some(h) = MAIN_HWND.get() {
        return Some(HWND(*h as _));
    }
    let mut ctx = EnumCtx {
        pid: std::process::id(),
        found: None,
    };
    unsafe {
        let _ = EnumWindows(
            Some(enum_windows_cb),
            LPARAM(&mut ctx as *mut EnumCtx as isize),
        );
    }
    ctx.found.map(|h| {
        let _ = MAIN_HWND.set(h.0 as usize);
        h
    })
}

fn show_window(ctx: &egui::Context) {
    // Snapshot the window the user is in *before* we take focus: the
    // window-control actions (and {%wd}/{%wt}/{%wc}) refer to that window, not
    // to MxRun itself. AltRun captured the same values at hotkey-press time
    // (docs/AltRun交互规格.md §4).
    exec::remember_foreground();
    WINDOW_VISIBLE.store(true, Ordering::Relaxed);
    match main_hwnd() {
        Some(hwnd) => unsafe {
            // Center on the primary monitor at 1/3 height.
            let sw = GetSystemMetrics(SM_CXSCREEN);
            let sh = GetSystemMetrics(SM_CYSCREEN);
            let mut rect = std::mem::zeroed();
            let _ = GetWindowRect(hwnd, &mut rect);
            let ww = rect.right - rect.left;
            let wh = rect.bottom - rect.top;
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                (sw - ww) / 2,
                (sh - wh) / 3,
                0,
                0,
                SWP_NOSIZE | SWP_SHOWWINDOW,
            );
            let _ = SetForegroundWindow(hwnd);
        },
        // Should not happen now that the handle lookup no longer requires the
        // window to be visible, but if it does the show is a silent no-op —
        // worth a line in the log.
        None => log_line("show: main window handle not found"),
    }
    ctx.request_repaint();
}

fn hide_window(_ctx: &egui::Context) {
    WINDOW_VISIBLE.store(false, Ordering::Relaxed);
    if let Some(hwnd) = main_hwnd() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

// --- E2 spike: native system backdrop (Mica) ---

/// Ask DWM to paint the Win11 system material behind this window.
/// `MXRUN_BACKDROP=mica|acrylic` (or a raw DWMSBT_* number). Win11 22H2+.
///
/// mica (2) samples the *wallpaper* and deliberately does NOT show the windows
/// behind — so "nothing behind changed" is not evidence either way. acrylic (3)
/// blurs everything behind, which makes it the useful discriminator for
/// whether the backdrop pipeline works on this window at all.
fn apply_backdrop(hwnd: HWND, mode: &str) {
    // DWMWA_SYSTEMBACKDROP_TYPE = 38 (not in older SDK headers; raw value).
    const DWMWA_SYSTEMBACKDROP_TYPE: i32 = 38;
    let value: i32 = match mode {
        "mica" => 2,     // DWMSBT_MAINWINDOW
        "acrylic" => 3,  // DWMSBT_TRANSIENTWINDOW
        other => match other.parse::<i32>() {
            Ok(v) if v > 0 => v,
            _ => return,
        },
    };
    let r = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWINDOWATTRIBUTE(DWMWA_SYSTEMBACKDROP_TYPE),
            &value as *const i32 as *const core::ffi::c_void,
            std::mem::size_of::<i32>() as u32,
        )
    };
    log_line(&format!(
        "backdrop: DwmSetWindowAttribute(SYSTEMBACKDROP_TYPE={value}) -> {r:?}"
    ));
}

/// AltRun behaviour: the window hides the moment it loses focus
/// (docs/AltRun交互规格.md §6). Ask Win32 for the foreground window directly
/// rather than trusting egui's ViewportInfo — egui 0.36's `visible()` is always
/// None (see the README pitfall notes), and `focused` is no more trustworthy.
/// Anything owned by this process counts as "still ours", so an open dialog or
/// an IME window does not dismiss the launcher.
fn foreground_is_ours() -> bool {
    let fg = unsafe { GetForegroundWindow() };
    if fg.0.is_null() {
        return true; // no foreground window mid-switch: don't misread it as a blur
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(fg, Some(&mut pid)) };
    pid == std::process::id()
}

fn toggle_window(ctx: &egui::Context) {
    if window_visible() {
        hide_window(ctx);
    } else {
        WAKE_T0_US.store(us_since_start(), Ordering::Relaxed); // E1a probe
        show_window(ctx);
    }
}

/// Ask winit to close the window (clean exit) from any thread.
fn close_window() {
    if let Some(hwnd) = main_hwnd() {
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

// ---------- E3: system icon extraction ----------

/// Icon size on screen, in points. The bitmap is fetched at
/// `ICON_POINTS * pixels_per_point` so it stays sharp on scaled displays.
const ICON_POINTS: f32 = 24.0;
//
// Icons are fetched ONCE per command while building the index (a few hundred
// commands at most) and kept as egui textures, so the search hot path only
// does a hash lookup — the same "do the expensive work at load time" rule the
// AltRun research pinned down. `SHGetFileInfoW` would be simpler but caps at
// the legacy 16/32px sizes and a DPI-scaled display then shows a blurry icon;
// IShellItemImageFactory hands back a 32-bit premultiplied bitmap at whatever
// size we ask for.

/// The shell APIs need COM on this thread; calling it twice is harmless.
fn com_init_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    });
}

/// COM is per-thread, so a worker thread must initialise its own apartment —
/// the process-wide `Once` above would (correctly) skip it there.
fn com_init_thread() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
}

/// Turn a launcher command's target into a path the shell can produce an icon
/// for. Handles the shapes that appear in real command lists: a bare name
/// ("calc.exe"), a quoted path with arguments, an unquoted path with
/// arguments. Returns None for things with no file behind them (URLs).
/// Resolve a bare command name against PATH the way the shell would,
/// honouring PATHEXT. Imported AltRun rows carry bare names with no extension
/// (`nslookup`, `mspaint`) and command lines whose head is a bare name
/// (`cmd /k %p`), neither of which is a path.
fn resolve_in_path(name: &str) -> Option<std::path::PathBuf> {
    if name.is_empty() || name.contains(['\\', '/']) {
        return None;
    }
    let path = std::env::var_os("PATH")?;
    let exts: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
        .split(';')
        .map(|e| e.trim().to_lowercase())
        .filter(|e| !e.is_empty())
        .collect();
    for dir in std::env::split_paths(&path) {
        let direct = dir.join(name);
        if direct.is_file() {
            return Some(direct);
        }
        for ext in &exts {
            let cand = dir.join(format!("{name}{ext}"));
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

fn icon_source(target: &str) -> Option<std::path::PathBuf> {
    let t = target.trim();
    if t.is_empty() {
        return None;
    }
    // "C:\path with spaces\app.exe" -arg
    if let Some(rest) = t.strip_prefix('"') {
        let end = rest.find('"')?;
        let p = std::path::PathBuf::from(&rest[..end]);
        return p.exists().then_some(p);
    }
    // Exact path, or path + arguments: try the whole string first so paths with
    // spaces survive, then the part before the first space.
    let whole = std::path::Path::new(t);
    if whole.exists() {
        return Some(whole.to_path_buf());
    }
    if let Some((head, _)) = t.split_once(' ') {
        let p = std::path::Path::new(head);
        if p.exists() {
            return Some(p.to_path_buf());
        }
        // `cmd /k %p` — the head is a bare command name, not a path.
        if let Some(found) = resolve_in_path(head) {
            return Some(found);
        }
    }
    resolve_in_path(t)
}

/// Read the shell's icon for `path` at `px` pixels square as premultiplied
/// RGBA. Kept separate from the texture upload so it can be exercised without
/// a GUI (see the tests at the bottom of this file).
fn icon_pixels(path: &std::path::Path, px: u32) -> Option<(usize, usize, Vec<u8>)> {
    use windows::Win32::Graphics::Gdi::{BITMAP, DeleteObject, GetObjectW, HBITMAP};
    use windows::Win32::UI::Shell::{
        IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_ICONONLY,
    };

    com_init_once();
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let item: IShellItemImageFactory =
            SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).ok()?;
        let hbmp: HBITMAP = item
            .GetImage(
                windows::Win32::Foundation::SIZE {
                    cx: px as i32,
                    cy: px as i32,
                },
                SIIGBF_ICONONLY,
            )
            .ok()?;

        let mut bm = BITMAP::default();
        let got = GetObjectW(
            hbmp.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut _ as *mut core::ffi::c_void),
        );
        if got == 0 || bm.bmBits.is_null() || bm.bmWidth <= 0 || bm.bmHeight <= 0 {
            let _ = DeleteObject(hbmp.into());
            return None;
        }
        let (w, h) = (bm.bmWidth as usize, bm.bmHeight as usize);
        let stride = bm.bmWidthBytes as usize;
        // A negative height means a top-down DIB; the shell returns those, and
        // GetObjectW reports the height as-is, so take the magnitude.
        let src = bm.bmBits as *const u8;
        let mut rgba = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                let o = y * stride + x * 4;
                // Premultiplied BGRA -> RGBA, same premultiplication.
                rgba.push(*src.add(o + 2));
                rgba.push(*src.add(o + 1));
                rgba.push(*src.add(o));
                rgba.push(*src.add(o + 3));
            }
        }
        let _ = DeleteObject(hbmp.into());
        Some((w, h, rgba))
    }
}

/// Fetch `path`'s icon and upload it as a texture drawn at `logical_px` points.
/// Only called from the UI thread, with pixels prepared by the icon worker.
fn upload_icon(
    ctx: &egui::Context,
    key: &str,
    (w, h, rgba): (usize, usize, Vec<u8>),
) -> egui::TextureHandle {
    let image = egui::ColorImage::from_rgba_premultiplied([w, h], &rgba);
    ctx.load_texture(format!("icon:{key}"), image, egui::TextureOptions::LINEAR)
}

/// Shell icons, fetched on demand and kept in a bounded cache.
///
/// Prewarming every icon before the first frame cost ~0.6 s with 59 items and
/// scales linearly, so the window used to appear late and would only get worse
/// as the list grows. Now the row renderer *requests* what it is about to draw;
/// a worker thread does the shell call; the texture is uploaded when the pixels
/// arrive. Two debts paid at once: no startup stall, and a cache that cannot
/// grow without bound (which matters for file-search results later).
struct Icons {
    map: std::collections::HashMap<String, egui::TextureHandle>,
    order: std::collections::VecDeque<String>,
    /// Targets already asked for, so a row does not queue the same fetch twice.
    pending: std::collections::HashSet<String>,
    /// Targets that produced nothing (URLs, builtin verbs) — never ask again.
    missing: std::collections::HashSet<String>,
    request: std::sync::mpsc::Sender<String>,
    done: std::sync::mpsc::Receiver<(String, Option<(usize, usize, Vec<u8>)>)>,
    hits: u64,
    misses: u64,
    /// One log line telling us how long the first icon took to arrive.
    first_logged: bool,
    /// …and one telling us the queue has drained.
    drained_logged: bool,
}

/// How many icons to keep. Visible rows are at most 8, so a few hundred covers
/// everything a session realistically scrolls through.
const ICON_CACHE_CAP: usize = 256;

impl Icons {
    fn new(ctx: &egui::Context) -> Self {
        let (request, jobs) = std::sync::mpsc::channel::<String>();
        let (results, done) = std::sync::mpsc::channel();
        spawn_icon_worker(ctx.clone(), jobs, results);
        Self {
            map: std::collections::HashMap::new(),
            order: std::collections::VecDeque::new(),
            pending: std::collections::HashSet::new(),
            missing: std::collections::HashSet::new(),
            request,
            done,
            hits: 0,
            misses: 0,
            first_logged: false,
            drained_logged: false,
        }
    }

    /// The texture for `key`, queueing a fetch the first time it is asked for.
    fn get(&mut self, key: &str) -> Option<egui::TextureHandle> {
        if let Some(tex) = self.map.get(key) {
            self.hits += 1;
            return Some(tex.clone());
        }
        self.misses += 1;
        if !self.pending.contains(key) && !self.missing.contains(key) {
            if self.request.send(key.to_string()).is_ok() {
                self.pending.insert(key.to_string());
            }
        }
        None
    }

    /// Upload whatever the worker finished since the last frame.
    fn drain(&mut self, ctx: &egui::Context) {
        while let Ok((key, pixels)) = self.done.try_recv() {
            self.pending.remove(&key);
            match pixels {
                Some(px) => {
                    let tex = upload_icon(ctx, &key, px);
                    self.map.insert(key.clone(), tex);
                    self.order.push_back(key);
                    while self.order.len() > ICON_CACHE_CAP {
                        if let Some(old) = self.order.pop_front() {
                            self.map.remove(&old);
                        }
                    }
                    if !self.first_logged {
                        self.first_logged = true;
                        log_line(&format!(
                            "icons: first ready at {}us (cache={})",
                            us_since_start(),
                            self.len()
                        ));
                    }
                }
                None => {
                    // URLs and builtin verbs have no shell icon: remember that,
                    // so every repaint does not re-queue them.
                    self.missing.insert(key);
                }
            }
        }
        // One line per session telling us the worker has caught up — the cheap
        // way to see that on-demand loading really delivered.
        if self.first_logged && !self.drained_logged && self.pending.is_empty() {
            self.drained_logged = true;
            log_line(&format!(
                "icons: worker idle at {}us (cache={} missing={})",
                us_since_start(),
                self.map.len(),
                self.missing.len()
            ));
        }
    }

    fn len(&self) -> usize {
        self.map.len()
    }
}

/// One thread, one COM apartment, shell calls only — never blocks the UI.
fn spawn_icon_worker(
    ctx: egui::Context,
    jobs: std::sync::mpsc::Receiver<String>,
    results: std::sync::mpsc::Sender<(String, Option<(usize, usize, Vec<u8>)>)>,
) {
    std::thread::spawn(move || {
        com_init_thread();
        let px = (ICON_POINTS * ctx.pixels_per_point()).ceil() as u32;
        let mut first = true;
        while let Ok(target) = jobs.recv() {
            let started = std::time::Instant::now();
            // Targets carry %VAR% as often as not (`%WINDIR%`, `%PROGRAMFILES%`).
            let expanded = exec::expand_env(&target);
            let pixels = icon_source(&expanded).and_then(|src| icon_pixels(&src, px));
            if first {
                // The very first shell icon call is what costs: it initialises
                // the shell imaging machinery. Worth knowing when tuning start-up.
                first = false;
                log_line(&format!(
                    "icons: worker first fetch took {}ms (at {}us)",
                    started.elapsed().as_millis(),
                    us_since_start()
                ));
            }
            if results.send((target, pixels)).is_err() {
                break; // UI gone
            }
            // Wake the UI so the icon appears as soon as it exists.
            ctx.request_repaint();
        }
    });
}

// ---------- search data structures ----------

/// A command enriched for searching: pinyin-expanded haystack + frecency.
struct Indexed {
    item: Item,
    /// title + subtitle + keywords + full pinyin + pinyin initials (lowercased)
    searchable: String,
    /// char length of `item.title` (highlight boundary)
    title_chars: usize,
    /// lowercased trigger words, precomputed for the keyword weighting
    keywords_lower: Vec<String>,
    frec_bonus: f64,
    /// How many times this item has been launched (shown in the row).
    launches: u32,
}

struct Scored {
    idx: usize,
    score: f64,
    /// char indices into `item.title` to highlight (may be empty)
    hit_indices: Vec<u32>,
}

/// Score one indexed item against an already-parsed pattern.
///
/// Kept as a free function so the ranking rules (including trigger-word
/// weighting) can be unit-tested without an egui context.
fn rank_item(
    idx: usize,
    it: &Indexed,
    query: &str,
    pattern: &Pattern,
    matcher: &mut Matcher,
    buf: &mut Vec<char>,
    idx_buf: &mut Vec<u32>,
) -> Option<Scored> {
    // Match the display title for highlighting, and the pinyin-expanded
    // haystack for reach; take the better score.
    buf.clear();
    idx_buf.clear();
    let title_score = pattern.indices(Utf32Str::new(&it.item.title, buf), matcher, idx_buf);
    let hits = if title_score.is_some() { idx_buf.clone() } else { Vec::new() };

    buf.clear();
    let hay_score = pattern.score(Utf32Str::new(&it.searchable, buf), matcher);

    let (base, hits) = match (title_score, hay_score) {
        (Some(s), hs) => {
            let s = s as f64;
            let alt = hs.map(|x| x as f64 * 0.9).unwrap_or(0.0);
            if s >= alt { (s, hits) } else { (alt, Vec::new()) }
        }
        (None, Some(s)) => (s as f64 * 0.9, Vec::new()),
        (None, None) => return None,
    };

    // Trigger words outrank fuzzy title hits: typing `stea` must put Steam
    // first even when other titles fuzzy-match just as well.
    let q = query.trim().to_lowercase();
    let keyword_bonus = it
        .keywords_lower
        .iter()
        .map(|k| {
            if *k == q {
                1000.0
            } else if k.starts_with(&q) {
                400.0
            } else if k.contains(&q) {
                200.0
            } else {
                0.0
            }
        })
        .fold(0.0, f64::max);

    Some(Scored {
        idx,
        score: base + keyword_bonus + it.frec_bonus,
        hit_indices: hits,
    })
}

/// Drop the long tail of weak fuzzy matches (see [`SCORE_GATE`]). Expects a
/// sorted list; a zero best score means nothing matched strongly, so nothing is
/// dropped.
fn apply_score_gate(results: &mut Vec<Scored>) {
    let Some(best) = results.first().map(|s| s.score) else {
        return;
    };
    if best <= 0.0 {
        return;
    }
    let floor = best * SCORE_GATE;
    results.retain(|s| s.score >= floor);
}

/// Right-hand launch counter: `None` when the item has never run, so unused
/// rows stay clean instead of showing a wall of zeroes.
fn launch_label(count: u32) -> Option<String> {
    match count {
        0 => None,
        1 => Some("启动 1 次".to_string()),
        n => Some(format!("启动 {n} 次")),
    }
}

/// Second line of a row. Falls back to what the item will actually run, so
/// imported rows (which carry no description) are not half empty.
fn row_detail(item: &Item) -> String {
    if !item.subtitle.trim().is_empty() {
        return item.subtitle.clone();
    }
    let target = item.icon_target().unwrap_or_default();
    truncate_chars(target, 64)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Parameter prompt (P1-1)
// ---------------------------------------------------------------------------

/// An open parameter prompt.
///
/// AltRun popped a modal dialog here (`frmParam`: a combobox holding the
/// parameter history). MxRun keeps the two stages — pick the command, then say
/// what to give it — but not the second window: the top box *becomes* the
/// parameter field and the list becomes the suggestion list, so the window
/// never changes size and nothing steals focus (see `docs/开发进度.md` §4).
struct Prompt {
    /// The command being launched, cloned: while the prompt is open the result
    /// list is no longer what is on screen, so an index into it would be a trap.
    item: Item,
    action: Action,
    /// Parameter history for this item, `(value, uses)`, already sorted by use
    /// count. Loaded once when the prompt opens, so typing never touches the
    /// database.
    history: Vec<(String, u32)>,
    /// Indices into `history` matching what is typed right now.
    suggestions: Vec<usize>,
    /// Highlighted suggestion (Up/Down). `None` = "use what I typed".
    cursor: Option<usize>,
    /// What the user typed.
    text: String,
    /// True when the text *is* the command line ([`BuiltinVerb::RunInput`]).
    run_line: bool,
}

impl Prompt {
    fn new(item: Item, action: Action, history: Vec<(String, u32)>) -> Self {
        let run_line = matches!(
            action.effect,
            Effect::Builtin { verb: BuiltinVerb::RunInput }
        );
        let mut prompt = Self {
            item,
            action,
            history,
            suggestions: Vec::new(),
            cursor: None,
            text: String::new(),
            run_line,
        };
        prompt.refilter();
        prompt
    }

    /// Recompute the suggestions after the text changed. Typing also drops the
    /// highlight: it means "I want my own text, not a remembered one".
    fn refilter(&mut self) {
        self.suggestions = filter_history(&self.history, &self.text);
        self.cursor = None;
    }

    /// What Enter will use: the highlighted suggestion, else the typed text.
    fn value(&self) -> String {
        match self.cursor.and_then(|i| self.suggestions.get(i)) {
            Some(&h) => self.history[h].0.clone(),
            None => self.text.trim().to_string(),
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.suggestions.is_empty() {
            self.cursor = None;
            return;
        }
        let last = self.suggestions.len() as isize - 1;
        self.cursor = Some(match self.cursor {
            None => 0,
            Some(i) => (i as isize + delta).clamp(0, last) as usize,
        });
    }

    fn empty_hint(&self) -> &'static str {
        if self.run_line {
            "输入要运行的命令（Esc 返回）"
        } else {
            "请输入参数（Esc 返回）"
        }
    }
}

/// Indices of the history entries containing `query`, case-insensitively.
/// An empty query keeps everything (the store already sorted by use count).
fn filter_history(history: &[(String, u32)], query: &str) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    history
        .iter()
        .enumerate()
        .filter(|(_, (value, _))| q.is_empty() || value.to_lowercase().contains(&q))
        .map(|(i, _)| i)
        .collect()
}

struct MxRunApp {
    store: Store,
    items: Vec<Indexed>,
    /// Shell icons for the rows being drawn: fetched on demand by a worker
    /// thread, cached with a cap (see [`Icons`]).
    icons: Icons,
    matcher: Matcher,
    input: String,
    results: Vec<Scored>,
    /// Open parameter prompt (P1-1); replaces the search box while it lives.
    prompt: Option<Prompt>,
    calc_result: Option<String>,
    selected: usize,
    /// E2 spike: card opacity, `MXRUN_CARD_ALPHA` (0-255, default 235).
    card_alpha: u8,
    want_focus: bool,
    centered: bool,
    prev_visible: bool,
    /// When the window was last shown — focus-loss hiding waits out a short
    /// grace period so a failed SetForegroundWindow cannot cause a show/hide
    /// flicker.
    shown_at: Option<std::time::Instant>,
    // settings view
    view_settings: bool,
    open_settings: Arc<AtomicBool>,
    capturing_hotkey: bool,
    pending_hotkey: Option<String>,
    hotkey_str: String,
    hotkey_manager: GlobalHotKeyManager,
    _tray: TrayIcon,
    status: String,
    /// One-off log of when the first frame rendered (cold-start breakdown).
    first_frame_logged: bool,
}

impl MxRunApp {
    /// Build the app. Errors are returned as user-facing messages: startup
    /// failures are reported by a dialog in `main`, never by a panic (see the
    /// note on `windows_subsystem` above).
    fn new(cc: &eframe::CreationContext<'_>) -> Result<Self, String> {
        install_cjk_font(&cc.egui_ctx);

        let store = Store::open().map_err(|e| {
            format!(
                "无法打开数据文件 mxrun.redb：{e}\n\
                 （可能仍有另一个 MxRun 实例在运行，或文件被其他程序占用）"
            )
        })?;

        // Migration / load problems are logged instead of vanishing (v1 dropped
        // unparseable rows without a word).
        for w in &store.warnings {
            log_line(&format!("store: {w}"));
        }

        // --- global hotkey (from config, default Alt+F1) ---
        let mut hotkey_str = store
            .get_config("hotkey")
            .filter(|s| parse_hotkey(s).is_some())
            .unwrap_or_else(|| DEFAULT_HOTKEY.to_string());
        let manager =
            GlobalHotKeyManager::new().map_err(|e| format!("无法初始化全局热键：{e}"))?;
        let mut startup_note: Option<String> = None;
        let registered = parse_hotkey(&hotkey_str)
            .map(|hk| manager.register(hk).is_ok())
            .unwrap_or(false);
        log_line(&format!("startup: hotkey={hotkey_str} registered={registered}"));
        if !registered {
            // The configured hotkey is taken by another program (or invalid):
            // fall back to the default instead of leaving no way to summon.
            startup_note = Some(format!(
                "快捷键 {hotkey_str} 注册失败（可能被其他程序占用），已回退为 {DEFAULT_HOTKEY}"
            ));
            hotkey_str = DEFAULT_HOTKEY.to_string();
            if let Some(hk) = parse_hotkey(&hotkey_str) {
                let _ = manager.register(hk);
            }
        }

        // --- system tray ---
        let menu = Menu::new();
        let toggle_item = MenuItem::new("显示 / 隐藏  MxRun", true, None);
        let settings_item = MenuItem::new("设置", true, None);
        let quit_item = MenuItem::new("退出 MxRun", true, None);
        let _ = menu.append(&toggle_item);
        let _ = menu.append(&settings_item);
        let _ = menu.append(&quit_item);
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("MxRun")
            .with_icon(build_tray_icon())
            .build()
            .map_err(|e| format!("无法创建托盘图标：{e}"))?;

        // --- background event thread: hotkey + tray events ---
        // Must NOT live inside App::ui(): eframe sleeps while the window is
        // hidden, so ui() would stop running and the window could never be
        // re-shown (the original "hidden = dead" bug).
        let ctx2 = cc.egui_ctx.clone();
        let toggle_id = toggle_item.id().clone();
        let settings_id = settings_item.id().clone();
        let quit_id = quit_item.id().clone();
        let open_settings = Arc::new(AtomicBool::new(false));
        let open_settings2 = open_settings.clone();
        std::thread::spawn(move || loop {
            let mut wake = false;

            for ev in GlobalHotKeyEvent::receiver().try_iter() {
                if ev.state == HotKeyState::Pressed {
                    toggle_window(&ctx2);
                    wake = true;
                }
            }
            for ev in TrayIconEvent::receiver().try_iter() {
                if let TrayIconEvent::DoubleClick { .. } = ev {
                    show_window(&ctx2);
                    wake = true;
                }
            }
            for ev in MenuEvent::receiver().try_iter() {
                if ev.id == toggle_id {
                    toggle_window(&ctx2);
                } else if ev.id == settings_id {
                    open_settings2.store(true, Ordering::Relaxed);
                    show_window(&ctx2);
                } else if ev.id == quit_id {
                    close_window();
                }
                wake = true;
            }

            if wake {
                ctx2.request_repaint();
            }
            std::thread::sleep(Duration::from_millis(50));
        });

        let mut app = Self {
            items: Vec::new(),
            icons: Icons::new(&cc.egui_ctx),
            matcher: Matcher::new(Config::DEFAULT),
            input: String::new(),
            results: Vec::new(),
            prompt: None,
            calc_result: None,
            selected: 0,
            card_alpha: std::env::var("MXRUN_CARD_ALPHA")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(235),
            want_focus: true,
            centered: false,
            prev_visible: true,
            // Start the grace period at launch: the window is shown on the first
            // frame, and focus-loss hiding must be armed for that first show too
            // (leaving it None here meant the check stayed dormant until the
            // window had been hidden and re-shown at least once).
            shown_at: Some(std::time::Instant::now()),
            view_settings: false,
            open_settings,
            capturing_hotkey: false,
            pending_hotkey: None,
            status: String::new(),
            first_frame_logged: false,
            store,
            hotkey_manager: manager,
            hotkey_str,
            _tray: tray,
        };
        app.reset_status();
        if let Some(note) = startup_note {
            app.status = note;
        }
        app.rebuild_index(&cc.egui_ctx);
        app.refresh_search();

        // Queue the icons for the rows the first frame will draw, *before* that
        // frame exists. The first shell icon call costs ~200 ms (it initialises
        // the shell imaging machinery), and eframe spends a similar amount of
        // time on its own init — overlapping the two means the icons are usually
        // ready by the time the window appears instead of popping in after it.
        app.prefetch_visible_icons();

        // --- debug hook: drive the search pipeline without keyboard input ---
        //
        // `MXRUN_SELFTEST=<query>` fills the search box at startup, runs the
        // real search, and logs what the list will show. `MXRUN_SELFTEST_EXEC=1`
        // additionally runs the first row (only safe for items that refuse to
        // run, e.g. ones needing input). It must run *after* the index is built.
        //
        // Why: keys cannot be delivered to the window from an automation
        // context (SendKeys targets whatever window has focus — see CLAUDE.md
        // pitfall 3), so this is how the search → list → execute path gets
        // verified without a human at the keyboard.
        if let Ok(query) = std::env::var("MXRUN_SELFTEST") {
            app.input = query.clone();
            app.refresh_search();
            log_line(&format!("selftest: query={query:?} rows={}", app.results.len()));
            for (row, sc) in app.results.iter().enumerate() {
                let it = &app.items[sc.idx];
                let (category, needs_input) = match it.item.default_action() {
                    Some(a) => (a.effect.label(), it.item.wants_input()),
                    None => ("条目", false),
                };
                log_line(&format!(
                    "selftest: row{row} title={:?} category={category} needs_input={needs_input} launches={} score={:.1}",
                    it.item.title, it.launches, sc.score
                ));
            }
            if app.results.is_empty() {
                log_line("selftest: no matches");
            }
            if std::env::var("MXRUN_SELFTEST_EXEC").is_ok() {
                let ctx = cc.egui_ctx.clone();
                log_line("selftest: executing first row");
                app.execute_selected(&ctx);
                log_line(&format!("selftest: status={:?}", app.status));
                // `MXRUN_SELFTEST_PARAM` drives the parameter prompt as well:
                // the only way to exercise "typed parameter -> real command"
                // with no keyboard. It **does** run the command for real.
                if let Ok(param) = std::env::var("MXRUN_SELFTEST_PARAM") {
                    match app.prompt.as_mut() {
                        Some(p) => {
                            p.text = param;
                            p.refilter();
                            log_line(&format!(
                                "selftest: prompt open, history={} suggestions={}",
                                p.history.len(),
                                p.suggestions.len()
                            ));
                        }
                        None => log_line("selftest: MXRUN_SELFTEST_PARAM set, but no prompt opened"),
                    }
                    app.finish_prompt(&ctx);
                    log_line(&format!("selftest: after parameter status={:?}", app.status));
                }
            }
        }
        Ok(app)
    }

    fn reset_status(&mut self) {
        self.status = format!(
            "{} 呼出 / 隐藏 · Enter/空格 执行 · Esc 清空或隐藏 · F2 设置",
            self.hotkey_str
        );
    }

    /// Apply a new hotkey string: re-register live, persist to config.
    fn apply_hotkey(&mut self, new_hotkey: &str) {
        let Some(new_hk) = parse_hotkey(new_hotkey) else {
            self.status = format!("无效快捷键：{new_hotkey}");
            return;
        };
        let old = parse_hotkey(&self.hotkey_str);
        if let Some(old_hk) = old {
            let _ = self.hotkey_manager.unregister(old_hk);
        }
        match self.hotkey_manager.register(new_hk) {
            Ok(_) => {
                self.hotkey_str = new_hotkey.to_string();
                self.store.set_config("hotkey", new_hotkey);
                self.reset_status();
                self.status = format!("快捷键已更新为 {new_hotkey}，立即生效");
            }
            Err(e) => {
                // Roll back to the old hotkey.
                if let Some(old_hk) = old {
                    let _ = self.hotkey_manager.register(old_hk);
                }
                self.status = format!("注册失败（可能被占用）：{e}");
            }
        }
    }

    fn rebuild_index(&mut self, _ctx: &egui::Context) {
        let t0 = std::time::Instant::now();
        let items = self.store.load_items().unwrap_or_default();
        self.items = items
            .into_iter()
            .map(|item| {
                let base = item.haystack();
                let mut full = String::new();
                let mut initials = String::new();
                for ch in base.chars() {
                    let mut buf = [0u8; 4];
                    let s: &str = ch.encode_utf8(&mut buf);
                    if let Some(py) = s.to_pinyin().next().flatten() {
                        let plain = py.plain();
                        full.push_str(plain);
                        full.push(' ');
                        if let Some(c) = plain.chars().next() {
                            initials.push(c);
                        }
                    }
                }
                let searchable = if initials.is_empty() {
                    base.to_lowercase()
                } else {
                    format!("{} {} {}", base.to_lowercase(), full, initials)
                };
                let frec = self.store.get_frecency(&item.id);
                Indexed {
                    title_chars: item.title.chars().count(),
                    keywords_lower: item
                        .keywords
                        .iter()
                        .map(|k| k.trim().to_lowercase())
                        .filter(|k| !k.is_empty())
                        .collect(),
                    item,
                    searchable,
                    frec_bonus: frecency_bonus(&frec),
                    launches: frec.count,
                }
            })
            .collect();

        // Icons are no longer fetched here: warming all of them before the first
        // frame cost ~0.6 s at 59 items and grows linearly with the list. They
        // are now requested by the row renderer and prepared on a worker thread
        // (see `Icons`), so startup only pays for the index itself.
        log_line(&format!(
            "index: {} items built in {}ms",
            self.items.len(),
            t0.elapsed().as_millis()
        ));
    }

    /// Pull one item's statistics back out of the store.
    ///
    /// `bump_frecency` writes to the database, but the in-memory index holds its
    /// own copy — without this the launch count and the ranking weight stayed
    /// stale until the next restart.
    fn refresh_frecency(&mut self, id: &str) {
        let frec = self.store.get_frecency(id);
        if let Some(indexed) = self.items.iter_mut().find(|i| i.item.id == id) {
            indexed.launches = frec.count;
            indexed.frec_bonus = frecency_bonus(&frec);
        }
    }

    /// Ask the icon worker for the icons of whatever the list currently shows.
    fn prefetch_visible_icons(&mut self) {
        let targets: Vec<String> = self
            .results
            .iter()
            .filter_map(|sc| self.items[sc.idx].item.icon_target().map(str::to_string))
            .collect();
        for target in targets {
            let _ = self.icons.get(&target);
        }
    }

    fn refresh_search(&mut self) {
        self.results.clear();
        self.calc_result = None;
        let query = self.input.trim();
        if query.is_empty() {
            // Idle: show everything ranked by frecency.
            for (idx, it) in self.items.iter().enumerate() {
                self.results.push(Scored {
                    idx,
                    score: it.frec_bonus,
                    hit_indices: Vec::new(),
                });
            }
        } else {
            let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
            let mut buf: Vec<char> = Vec::new();
            let mut idx_buf: Vec<u32> = Vec::new();
            for (idx, it) in self.items.iter().enumerate() {
                if let Some(sc) = rank_item(
                    idx,
                    it,
                    query,
                    &pattern,
                    &mut self.matcher,
                    &mut buf,
                    &mut idx_buf,
                ) {
                    self.results.push(sc);
                }
            }

            // Calculator: pure math expression.
            if query.chars().all(|c| "0123456789+-*/%^(). ".contains(c))
                && query.chars().any(|c| "+-*/%^".contains(c))
            {
                if let Ok(v) = evalexpr::eval(query) {
                    if let Ok(n) = v.as_number() {
                        self.calc_result = Some(format!("{n}"));
                    }
                }
            }
        }
        self.results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if !query.is_empty() {
            apply_score_gate(&mut self.results);
        }
        self.results.truncate(MAX_ROWS);
        self.selected = 0;
    }

    fn execute_selected(&mut self, ctx: &egui::Context) {
        let Some(sc) = self.results.get(self.selected) else {
            return;
        };
        let item = self.items[sc.idx].item.clone();
        let Some(action) = item.default_action().cloned() else {
            return;
        };

        let effect = action.effect.clone();
        match exec::run(&item, &action) {
            exec::Outcome::Started => {
                self.store.bump_frecency(&item.id);
                self.refresh_frecency(&item.id);
                log_line(&format!("execute: {} ({effect:?})", item.title));
                self.status = format!("已启动：{}", item.title);
                self.input.clear();
                self.refresh_search();
                hide_window(ctx);
            }
            // The item wants a typed argument: open the parameter prompt rather
            // than reporting a dead end (P1-1).
            exec::Outcome::NeedsInput => {
                self.open_prompt(item, action);
            }
            exec::Outcome::Failed(why) => {
                log_line(&format!("execute: failed: {} -> {why}", item.title));
                self.status = format!("执行失败：{} —— {why}", item.title);
            }
        }
    }

    /// Open the parameter prompt for an item (`NeedsInput` came back from the
    /// executor). AltRun's `frmParam`, inline.
    fn open_prompt(&mut self, item: Item, action: Action) {
        let history = self.store.param_history(&item.id, PARAM_HISTORY_LIMIT);
        log_line(&format!(
            "prompt: open for {:?} (history={})",
            item.title,
            history.len()
        ));
        self.prompt = Some(Prompt::new(item, action, history));
        // The top box is now the parameter field: take the keys.
        self.want_focus = true;
    }

    /// Enter in the parameter field: run the command with what was picked.
    fn finish_prompt(&mut self, ctx: &egui::Context) {
        let Some(prompt) = self.prompt.take() else {
            return;
        };
        let value = prompt.value();
        if value.is_empty() {
            // Nothing typed: ask again instead of running `cmd /k ` (AltRun's
            // dialog refused too).
            self.status = prompt.empty_hint().to_string();
            self.prompt = Some(prompt);
            self.want_focus = true;
            return;
        }

        let id = prompt.item.id.clone();
        let title = prompt.item.title.clone();
        match exec::run_with_arg(&prompt.item, &prompt.action, Some(&value)) {
            exec::Outcome::Started => {
                self.store.bump_frecency(&id);
                // Remember it, so the next run can pick it instead of typing.
                self.store.bump_param(&id, &value);
                self.refresh_frecency(&id);
                // The parameter itself is not logged: the history in the db is
                // the feature, a plaintext log of it is not.
                log_line(&format!(
                    "execute: {title} (+parameter, {} chars)",
                    value.chars().count()
                ));
                self.status = format!("已启动：{title}");
                self.input.clear();
                self.refresh_search();
                hide_window(ctx);
            }
            // Unreachable — a value was supplied — but never close silently.
            exec::Outcome::NeedsInput => {
                self.status = prompt.empty_hint().to_string();
                self.prompt = Some(prompt);
            }
            exec::Outcome::Failed(why) => {
                log_line(&format!("execute: failed: {title} -> {why}"));
                self.status = format!("执行失败：{title} —— {why}");
                // Keep the text: the usual cause is a typo in it.
                self.prompt = Some(prompt);
                self.want_focus = true;
            }
        }
    }

    /// Keys while the parameter prompt is open.
    ///
    /// Every key handled here is *consumed* so the text box never sees it:
    /// Enter would otherwise surrender focus, and Esc would revert the text
    /// behind our back. Returns true when the prompt is done with this frame
    /// (the caller stops rendering, exactly like Enter on a result row).
    fn handle_prompt_keys(&mut self, ctx: &egui::Context) -> bool {
        // Esc, two stages (same shape as the main box and AltRun's dialog):
        // clear what was typed first, leave the prompt only when it is empty.
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape)) {
            if self.prompt.as_ref().is_some_and(|p| p.text.is_empty()) {
                log_line("prompt: cancelled");
                self.prompt = None;
                self.reset_status();
                self.want_focus = true;
            } else if let Some(p) = self.prompt.as_mut() {
                p.text.clear();
                p.refilter();
            }
            return false;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Enter)) {
            self.finish_prompt(ctx);
            return true;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::ArrowDown)) {
            if let Some(p) = self.prompt.as_mut() {
                p.move_cursor(1);
            }
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::ArrowUp)) {
            if let Some(p) = self.prompt.as_mut() {
                p.move_cursor(-1);
            }
        }
        // Space is deliberately *not* consumed here: unlike the main box (where
        // it means "run the selected item"), inside a parameter it is just a
        // space.
        false
    }

    fn name_layout_job(&self, sc: &Scored) -> LayoutJob {
        let item = &self.items[sc.idx];
        let name = &item.item.title;
        let mut job = LayoutJob::default();
        let font = FontId::proportional(17.0);
        let normal = Color32::from_gray(225);
        let hit = Color32::from_rgb(255, 190, 80);

        // Convert char indices to byte ranges.
        let char_to_byte: Vec<usize> = name.char_indices().map(|(b, _)| b).collect();
        let mut pos = 0usize; // char position
        let mut hits = sc.hit_indices.clone();
        hits.sort_unstable();
        hits.dedup();
        for h in hits {
            let h = h as usize;
            if h >= item.title_chars || h < pos {
                continue;
            }
            if h > pos {
                job.append(
                    &name[char_to_byte[pos]..char_to_byte[h]],
                    0.0,
                    TextFormat::simple(font.clone(), normal),
                );
            }
            let end = if h + 1 < item.title_chars {
                char_to_byte[h + 1]
            } else {
                name.len()
            };
            job.append(
                &name[char_to_byte[h]..end],
                0.0,
                TextFormat::simple(font.clone(), hit),
            );
            pos = h + 1;
        }
        if pos < item.title_chars {
            job.append(
                &name[char_to_byte[pos]..],
                0.0,
                TextFormat::simple(font, normal),
            );
        }
        job
    }

    // ---------- settings view ----------

    fn render_settings(&mut self, ui: &mut egui::Ui) {
        ui.heading(RichText::new("⚙ 设置").size(20.0));
        ui.add_space(12.0);

        // --- hotkey row ---
        ui.horizontal(|ui| {
            ui.label(RichText::new("呼出快捷键").size(15.0));
            ui.add_space(12.0);

            let display = if self.capturing_hotkey {
                "请按下新的快捷键…  (Esc 取消)".to_string()
            } else {
                self.pending_hotkey
                    .clone()
                    .unwrap_or_else(|| self.hotkey_str.clone())
            };
            ui.label(
                RichText::new(display)
                    .size(15.0)
                    .color(Color32::from_rgb(255, 190, 80)),
            );
        });
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            if !self.capturing_hotkey {
                if ui.button("重新设置（点击后按下新组合键）").clicked() {
                    self.capturing_hotkey = true;
                    self.pending_hotkey = None;
                }
                if let Some(pending) = self.pending_hotkey.clone() {
                    if ui.button("保存").clicked() {
                        self.apply_hotkey(&pending);
                        self.pending_hotkey = None;
                    }
                    if ui.button("取消").clicked() {
                        self.pending_hotkey = None;
                    }
                }
            }
        });

        // capture input while listening
        if self.capturing_hotkey {
            if ui.ctx().input(|i| i.key_pressed(Key::Escape)) {
                self.capturing_hotkey = false;
            } else if let Some(captured) = capture_hotkey(ui.ctx()) {
                self.capturing_hotkey = false;
                self.pending_hotkey = Some(captured);
            }
            ui.ctx().request_repaint_after(Duration::from_millis(50));
        }

        ui.add_space(16.0);
        ui.separator();
        ui.add_space(8.0);
        ui.label(
            RichText::new("支持修饰键组合：Alt / Ctrl / Shift + F1-F12、字母、数字、Space、Tab\n\
                           保存后立即生效，无需重启；如组合键被其他程序占用会提示注册失败。")
                .size(12.0)
                .color(Color32::from_gray(140)),
        );

        ui.add_space(16.0);
        if ui.button("返回 (Esc / F2)").clicked() {
            self.view_settings = false;
            self.capturing_hotkey = false;
            self.pending_hotkey = None;
            self.reset_status();
        }
    }
}

impl eframe::App for MxRunApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Rgba::TRANSPARENT.to_array()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Pick up any icons the worker finished since the last frame.
        self.icons.drain(&ctx);

        // One line saying when the window was actually up: cold start is now
        // dominated by eframe/GL/font initialisation, not by our data.
        if !self.first_frame_logged {
            self.first_frame_logged = true;
            log_line(&format!("frame: first at {}us", us_since_start()));
        }

        // E1a probe: first frame after a show request -> report the wake latency.
        let t0 = WAKE_T0_US.swap(0, Ordering::Relaxed);
        if t0 != 0 {
            log_line(&format!(
                "wake: {}us (hotkey -> first ui frame)",
                us_since_start().saturating_sub(t0)
            ));
        }

        // Center the window on the very first frame (initial launch).
        if !self.centered {
            self.centered = true;
            show_window(&ctx);
            if let Some(hwnd) = main_hwnd() {
                apply_backdrop(hwnd, &std::env::var("MXRUN_BACKDROP").unwrap_or_default());
            }
        }

        // Tray menu asked for the settings view.
        if self.open_settings.swap(false, Ordering::Relaxed) {
            self.view_settings = true;
        }

        // Re-focus the input box whenever the window transitions hidden -> visible
        // (the background thread can't touch app state).
        let visible = window_visible();
        if visible && !self.prev_visible {
            self.want_focus = true;
            self.shown_at = Some(std::time::Instant::now());
        }
        if !visible && self.prev_visible {
            // Just got hidden — by the hotkey, Esc, or focus loss. AltRun clears
            // the box on hide too (actHideExecute), so the next summon starts
            // from a clean slate instead of a stale query.
            self.input.clear();
            self.selected = 0;
            // An open prompt dies with the window as well: coming back to a
            // half-typed parameter for a command the user has moved on from
            // would be worse than starting over.
            if self.prompt.take().is_some() {
                log_line("prompt: dropped with the window");
            }
            self.refresh_search();
        }
        self.prev_visible = visible;

        // Focus-loss detection needs a frame to run in, and egui idles when
        // there is nothing to redraw — without this the check below simply
        // never executes while the window sits open and untouched. Ask for a
        // periodic wake-up only while visible (hidden costs nothing); 250ms is
        // short enough that dismissal still feels instant to a human.
        if visible {
            ctx.request_repaint_after(Duration::from_millis(250));
        }

        // AltRun: losing focus hides immediately (docs/AltRun交互规格.md §6).
        // Skipped while the settings view is open — that state lives in this
        // window, unlike AltRun's separate modal dialogs — and for a moment
        // after showing, in case SetForegroundWindow has not landed yet.
        //
        // `MXRUN_KEEP_OPEN=1` disables this hiding entirely so the window can be
        // screenshotted from an automation context: a process started in the
        // background cannot take the foreground, so the window used to vanish
        // after ~300 ms and no picture of the UI could be taken (see
        // docs/开发进度.md §7 pitfall 12).
        let keep_open = std::env::var("MXRUN_KEEP_OPEN").is_ok();
        if visible
            && !keep_open
            && !self.view_settings
            && self
                .shown_at
                .is_some_and(|t| t.elapsed() > Duration::from_millis(300))
            && !foreground_is_ours()
        {
            log_line("hide: lost focus");
            hide_window(&ctx);
            return;
        }

        // Global key handling (order matters: settings capture eats keys first,
        // then the parameter prompt, which owns the keyboard while it is open).
        if self.view_settings {
            if !self.capturing_hotkey
                && (ctx.input(|i| i.key_pressed(Key::Escape))
                    || ctx.input(|i| i.key_pressed(Key::F2)))
            {
                self.view_settings = false;
                self.pending_hotkey = None;
                self.reset_status();
            }
        } else if self.prompt.is_some() {
            if self.handle_prompt_keys(&ctx) {
                return;
            }
        } else {
            // Esc, two stages (AltRun §1.1): clear the box first, dismiss only
            // when there is nothing left to clear.
            if ctx.input(|i| i.key_pressed(Key::Escape)) {
                if self.input.is_empty() {
                    hide_window(&ctx);
                    return;
                }
                self.input.clear();
                self.selected = 0;
                self.refresh_search();
            }
            // Space runs the selected item (AltRun §1.2). consume_key takes the
            // keystroke away from the text box so a space never reaches the
            // query — AltRun does the same, and it has no multi-word search
            // either. Trade-off: two-word queries are no longer typeable.
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Space)) {
                self.execute_selected(&ctx);
                return;
            }
            if ctx.input(|i| i.key_pressed(Key::F2)) {
                self.view_settings = true;
            }
            if ctx.input(|i| i.key_pressed(Key::Enter)) {
                self.execute_selected(&ctx);
                return;
            }
            if ctx.input(|i| i.key_pressed(Key::ArrowDown)) && !self.results.is_empty() {
                self.selected = (self.selected + 1) % self.results.len().max(1);
            }
            if ctx.input(|i| i.key_pressed(Key::ArrowUp)) && !self.results.is_empty() {
                self.selected =
                    (self.selected + self.results.len() - 1) % self.results.len().max(1);
            }
        }

        let panel = egui::Frame::new()
            .fill(Color32::from_rgba_unmultiplied(26, 27, 34, self.card_alpha))
            .corner_radius(egui::CornerRadius::same(14))
            .inner_margin(egui::Margin::same(18))
            .stroke(egui::Stroke::new(1.0, Color32::from_white_alpha(24)));

        egui::CentralPanel::default()
            .frame(panel)
            .show(ui, |ui| {
                if self.view_settings {
                    self.render_settings(ui);
                } else {
                    self.render_search(ui, &ctx);
                }
            });
    }
}

impl MxRunApp {
    fn render_search(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // An open parameter prompt takes over both the top box and the list.
        if self.prompt.is_some() {
            self.render_prompt(ui);
            return;
        }

        // --- Search box ---
        let edit = egui::TextEdit::singleline(&mut self.input)
            .font(FontId::proportional(22.0))
            .hint_text("输入以搜索命令、拼音、网址…")
            .desired_width(f32::INFINITY)
            .frame(egui::Frame::default());
        let resp = ui.add(edit);
        if self.want_focus {
            resp.request_focus();
            self.want_focus = false;
        }
        if resp.changed() {
            self.refresh_search();
        }

        ui.add_space(6.0);
        ui.separator();
        ui.add_space(6.0);

        // --- Calculator result ---
        if let Some(result) = &self.calc_result {
            ui.label(
                RichText::new(format!("= {result}"))
                    .size(20.0)
                    .color(Color32::from_rgb(140, 220, 140)),
            );
            ui.add_space(4.0);
        }

        // --- Result list ---
        let mut clicked: Option<usize> = None;
        for row in 0..self.results.len() {
            let sc = &self.results[row];
            let item = &self.items[sc.idx];
            let selected = row == self.selected;
            let bg = if selected {
                Color32::from_white_alpha(18)
            } else {
                Color32::TRANSPARENT
            };
            let job = self.name_layout_job(sc);
            // Category label + emoji are derived from the default action's
            // effect (there is no stored "kind" any more).
            let (emoji, category) = match item.item.default_action() {
                Some(a) => (a.effect.icon(), a.effect.label()),
                None => ("•", "条目"),
            };
            let desc = row_detail(&item.item);
            // Ask for the icon of this row; the worker delivers it a frame or
            // two later and the emoji stands in until then.
            let icon = match item.item.icon_target() {
                Some(target) => self.icons.get(target),
                None => None,
            };
            let launches = launch_label(item.launches);
            let frame = egui::Frame::new()
                .fill(bg)
                .corner_radius(egui::CornerRadius::same(8))
                .inner_margin(egui::Margin::symmetric(10, 7));
            let inner = frame.show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    match &icon {
                        // Real shell icon at the display resolution (the texture
                        // is fetched at 24 * pixels_per_point physical pixels and
                        // drawn at 24 points, so it stays sharp when scaled).
                        Some(tex) => {
                            ui.add(
                                egui::Image::new(tex).fit_to_exact_size(egui::vec2(24.0, 24.0)),
                            );
                        }
                        None => {
                            ui.label(RichText::new(emoji).size(17.0));
                        }
                    }
                    ui.vertical(|ui| {
                        ui.label(job);
                        // Items that need a typed argument are marked with `*`:
                        // AltRun's own convention for "this one asks for input",
                        // and here it warns that Enter will open the parameter
                        // prompt instead of running straight away.
                        let needs_input = if item.item.wants_input() { "  ·  需要输入 *" } else { "" };
                        ui.label(
                            RichText::new(format!("{}  ·  {}{}", category, desc, needs_input))
                                .size(12.0)
                                .color(Color32::from_gray(140)),
                        );
                    });
                    // Launch counter, flush right — this is what fills the wide
                    // empty side of the row.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        match &launches {
                            Some(text) => {
                                ui.label(
                                    RichText::new(text)
                                        .size(11.0)
                                        .color(Color32::from_gray(120)),
                                );
                            }
                            None => {
                                // Never launched: say so quietly rather than
                                // leaving the column looking broken.
                                ui.label(
                                    RichText::new("未启动")
                                        .size(11.0)
                                        .color(Color32::from_gray(78)),
                                );
                            }
                        }
                    });
                });
            });
            let rect = inner.response.rect;
            let resp = ui.interact(rect, ui.id().with(row), egui::Sense::click());
            if resp.hovered() {
                self.selected = row;
            }
            if resp.clicked() {
                clicked = Some(row);
            }
        }
        if let Some(row) = clicked {
            self.selected = row;
            self.execute_selected(ctx);
        }

        if self.results.is_empty() && self.calc_result.is_none() {
            ui.label(
                RichText::new("无匹配结果 · 网页搜索等兜底功能将在后续版本加入")
                    .size(13.0)
                    .color(Color32::from_gray(130)),
            );
        }

        // --- Status bar ---
        ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
            ui.label(
                RichText::new(&self.status)
                    .size(11.0)
                    .color(Color32::from_gray(110)),
            );
        });
    }

    /// The parameter prompt, drawn where the search box and the command list
    /// normally are: a chip naming the command, the parameter field, and what
    /// this item was given before.
    fn render_prompt(&mut self, ui: &mut egui::Ui) {
        // Split the borrows up front: the closures below need the prompt and
        // the focus flag at once, and nothing else from `self`.
        let want_focus = &mut self.want_focus;
        let prompt = self.prompt.as_mut().expect("caller checked");

        let accent = Color32::from_rgb(255, 190, 80);

        // --- top box: "which command is asking" + what was typed ---
        ui.horizontal(|ui| {
            egui::Frame::new()
                .fill(Color32::from_rgba_unmultiplied(255, 190, 80, 26))
                .corner_radius(egui::CornerRadius::same(7))
                .inner_margin(egui::Margin::symmetric(9, 4))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(format!(
                            "{} {}",
                            prompt.action.effect.icon(),
                            truncate_chars(&prompt.item.title, 24)
                        ))
                        .size(14.0)
                        .color(accent),
                    );
                });
            let edit = egui::TextEdit::singleline(&mut prompt.text)
                .font(FontId::proportional(20.0))
                .hint_text(if prompt.run_line { "输入要运行的命令…" } else { "输入参数…" })
                .desired_width(f32::INFINITY)
                .frame(egui::Frame::default());
            let resp = ui.add(edit);
            if *want_focus {
                resp.request_focus();
                *want_focus = false;
            }
            if resp.changed() {
                prompt.refilter();
            }
        });

        ui.add_space(6.0);
        ui.separator();
        ui.add_space(6.0);

        // --- suggestion list: the parameters used with this item before ---
        let mut picked: Option<usize> = None;
        if prompt.suggestions.is_empty() {
            let msg = if prompt.history.is_empty() {
                "还没有历史参数 · 输入后回车即可"
            } else {
                "没有匹配的历史参数"
            };
            ui.label(RichText::new(msg).size(13.0).color(Color32::from_gray(130)));
        }
        for row in 0..prompt.suggestions.len().min(MAX_ROWS) {
            let h = prompt.suggestions[row];
            let value = prompt.history[h].0.clone();
            let uses = prompt.history[h].1;
            let selected = prompt.cursor == Some(row);
            let frame = egui::Frame::new()
                .fill(if selected {
                    Color32::from_rgba_unmultiplied(255, 190, 80, 30)
                } else {
                    Color32::TRANSPARENT
                })
                .corner_radius(egui::CornerRadius::same(8))
                .inner_margin(egui::Margin::symmetric(10, 5));
            let inner = frame.show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(truncate_chars(&value, 60))
                            .size(14.0)
                            .color(if selected { accent } else { Color32::from_gray(225) }),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!("用过 {uses} 次"))
                                .size(11.0)
                                .color(Color32::from_gray(120)),
                        );
                    });
                });
            });
            let resp = ui.interact(
                inner.response.rect,
                ui.id().with(("prompt-row", row)),
                egui::Sense::click(),
            );
            if resp.hovered() {
                prompt.cursor = Some(row);
            }
            if resp.clicked() {
                picked = Some(row);
            }
        }
        // A click fills the field instead of running straight away — a stored
        // parameter is often the right one only *after* an edit, and filling is
        // what AltRun's combobox did.
        if let Some(row) = picked {
            let h = prompt.suggestions[row];
            prompt.text = prompt.history[h].0.clone();
            prompt.cursor = Some(row);
            *want_focus = true;
        }

        // --- hint line (this is the status bar while a prompt is open) ---
        let hint = if prompt.cursor.is_some() {
            format!("回车执行「{}」 · Esc 返回", truncate_chars(&prompt.value(), 40))
        } else if prompt.history.is_empty() {
            "回车执行 · Esc 返回".to_string()
        } else {
            "↑↓ 选历史参数 · 回车执行 · Esc 返回".to_string()
        };
        ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
            ui.label(RichText::new(hint).size(11.0).color(Color32::from_gray(110)));
        });
    }
}

/// Load a CJK font (Microsoft YaHei) so Chinese text doesn't render as tofu.
/// egui's bundled fonts cover Latin only — without this every Chinese
/// character shows as an empty box.
fn install_cjk_font(ctx: &egui::Context) {
    const CANDIDATES: [&str; 3] = [
        "C:/Windows/Fonts/msyh.ttc",
        "C:/Windows/Fonts/simhei.ttf",
        "C:/Windows/Fonts/simsun.ttc",
    ];
    for path in CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert(
                "cjk".to_owned(),
                std::sync::Arc::new(egui::FontData::from_owned(bytes)),
            );
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts
                    .families
                    .entry(family)
                    .or_default()
                    .push("cjk".to_owned());
            }
            ctx.set_fonts(fonts);
            return;
        }
    }
    eprintln!("warning: no CJK font found, Chinese text will not render");
}

/// Generate a simple 32x32 tray icon (blue disc) without shipping an asset.
fn build_tray_icon() -> tray_icon::Icon {
    let (w, h) = (32u32, 32u32);
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let (dx, dy) = (x - 16, y - 16);
            if dx * dx + dy * dy <= 13 * 13 {
                let i = ((y as u32 * w + x as u32) * 4) as usize;
                rgba[i] = 90; // R
                rgba[i + 1] = 140; // G
                rgba[i + 2] = 255; // B
                rgba[i + 3] = 255; // A
            }
        }
    }
    tray_icon::Icon::from_rgba(rgba, w, h).expect("invalid tray icon")
}

#[cfg(test)]
mod tests {
    use super::*;
    use store::{Action, Item, seed_items};

    /// Build an indexed item the way `rebuild_index` does, without egui.
    fn index_for(item: Item) -> Indexed {
        let base = item.haystack();
        Indexed {
            title_chars: item.title.chars().count(),
            keywords_lower: item.keywords.iter().map(|k| k.to_lowercase()).collect(),
            searchable: base.to_lowercase(),
            item,
            frec_bonus: 0.0,
            launches: 0,
        }
    }

    /// The launch counter is what fills the right-hand side of a row.
    #[test]
    fn launch_label_hides_zero_and_counts_the_rest() {
        assert_eq!(launch_label(0), None, "unused rows stay clean");
        assert_eq!(launch_label(1).as_deref(), Some("启动 1 次"));
        assert_eq!(launch_label(42).as_deref(), Some("启动 42 次"));
    }

    /// Imported rows carry no description, so the second line falls back to
    /// what the item actually runs — otherwise half the row is blank.
    #[test]
    fn row_detail_falls_back_to_the_target() {
        let mut item = Item {
            title: "Steam".into(),
            actions: vec![Action::run(r"C:\Program Files\Steam\Steam.exe -tcp")],
            ..Default::default()
        };
        assert_eq!(row_detail(&item), r"C:\Program Files\Steam\Steam.exe -tcp");
        item.subtitle = "游戏平台".into();
        assert_eq!(row_detail(&item), "游戏平台", "an explicit subtitle wins");

        let long = Item {
            title: "x".into(),
            actions: vec![Action::open("D".repeat(200))],
            ..Default::default()
        };
        assert!(
            row_detail(&long).chars().count() <= 64,
            "long targets are truncated so the row cannot grow sideways"
        );
    }

    fn score(items: &[Indexed], query: &str) -> Vec<(String, f64)> {
        let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut buf = Vec::new();
        let mut idx_buf = Vec::new();
        let mut out: Vec<(String, f64)> = items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| {
                rank_item(i, it, query, &pattern, &mut matcher, &mut buf, &mut idx_buf)
            })
            .map(|sc| (items[sc.idx].item.title.clone(), sc.score))
            .collect();
        out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        out
    }

    /// A keyword hit must outrank a fuzzy title hit — this is what makes the
    /// imported trigger words (`stea`, `myip`) behave like AltRun's.
    #[test]
    fn exact_keyword_beats_fuzzy_title() {
        let steam = Item {
            id: "a".into(),
            title: "Steam 客户端".into(),
            keywords: vec!["stea".into()],
            actions: vec![Action::run("steam.exe")],
            ..Default::default()
        };
        // A title that fuzzy-matches "stea" just as well, but has no keyword.
        let other = Item {
            id: "b".into(),
            title: "steamcmd 工具".into(),
            actions: vec![Action::run("steamcmd.exe")],
            ..Default::default()
        };
        let idx = vec![index_for(steam), index_for(other)];
        let ranked = score(&idx, "stea");
        assert_eq!(ranked[0].0, "Steam 客户端", "keyword match must win: {ranked:?}");
        assert!(ranked[0].1 > 1000.0, "bonus applied: {ranked:?}");
    }

    /// A keyword that is not in the title at all must still be reachable —
    /// this is the `myip` / `b` case from the imported AltRun list.
    #[test]
    fn keyword_only_match_is_reachable() {
        let item = Item {
            id: "c".into(),
            title: "我的IP地址".into(),
            keywords: vec!["myip".into()],
            actions: vec![Action::run("nslookup")],
            ..Default::default()
        };
        let ranked = score(&[index_for(item)], "myip");
        assert_eq!(ranked.len(), 1, "trigger word must be searchable");
    }

    /// Prefix/contains tiers keep their order: exact > prefix > contains.
    #[test]
    fn keyword_tiers_rank_in_order() {
        let mk = |id: &str, title: &str, kw: &str| {
            index_for(Item {
                id: id.into(),
                title: title.into(),
                keywords: vec![kw.into()],
                actions: vec![Action::run("x.exe")],
                ..Default::default()
            })
        };
        let idx = vec![mk("1", "包含的", "xxabxx"), mk("2", "前缀的", "abxx"), mk("3", "精确的", "ab")];
        let ranked = score(&idx, "ab");
        let titles: Vec<&str> = ranked.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(titles, vec!["精确的", "前缀的", "包含的"], "got {ranked:?}");
    }

    /// The demo items must be findable by the keyword a user would type.
    #[test]
    fn seed_items_are_searchable_by_keyword() {        let idx: Vec<Indexed> = seed_items().into_iter().map(index_for).collect();
        for (query, expected) in [("calc", "计算器"), ("notepad", "记事本"), ("bing", "Bing")] {
            let ranked = score(&idx, query);
            assert_eq!(ranked[0].0, expected, "query {query} -> {ranked:?}");
        }
    }

    /// The icon path has to cope with every shape a command target takes.
    #[test]
    fn icon_source_handles_command_shapes() {
        // Bare name resolved through PATH (how the bundled demo commands look).
        assert!(icon_source("cmd.exe").is_some(), "bare name via PATH");
        // Absolute path with arguments, quoted.
        let quoted = icon_source(r#""C:\Windows\System32\notepad.exe" -foo"#);
        assert!(quoted.is_some(), "quoted path with arguments");
        assert!(quoted.unwrap().ends_with("notepad.exe"));
        // A directory.
        assert!(icon_source(r"C:\Windows").is_some(), "directory");
        // URLs and junk have no icon behind them.
        assert!(icon_source("https://github.com").is_none(), "url");
        assert!(icon_source("").is_none(), "empty");
        assert!(icon_source(r"C:\definitely\missing\nope.exe").is_none());
        // Bare names without an extension still resolve (PATHEXT), and so do
        // environment variables once expanded — both shapes come straight out
        // of the AltRun list (`nslookup`, `%WINDIR%`).
        assert!(icon_source("nslookup").is_some(), "bare name, no extension");
        assert!(icon_source("mspaint").is_some(), "bare name, no extension");
        assert!(
            icon_source("cmd /k {p}").is_some(),
            "command line whose head is a bare name"
        );
        assert!(
            icon_source(r"shutdown /s /t 5").is_some(),
            "system tool with arguments"
        );
        let windir = exec::expand_env("%WINDIR%");
        assert!(icon_source(&windir).is_some(), "expanded %WINDIR%");
    }

    /// The shell must actually hand back pixels for the item kinds a launcher
    /// shows. (x86/x64 System32 is redirected for 32-bit builds, which these
    /// are not; the paths below are the 64-bit ones.)
    #[test]
    fn icon_pixels_works_for_real_targets() {
        // Own .bat rather than a system script: which ones ship varies by
        // Windows build and localization.
        let bat = std::env::temp_dir().join("mxrun-icon-test.bat");
        std::fs::write(&bat, "@echo off\r\necho mxrun icon test\r\n").expect("write test .bat");

        let mut required = vec![
            ("exe/abs", r"C:\Windows\System32\notepad.exe".to_string()),
            ("exe/PATH", "cmd.exe".to_string()),
            ("dir", r"C:\Windows".to_string()),
            ("bat", bat.to_string_lossy().into_owned()),
        ];
        // Optional, because the harness has to create it: a real .lnk.
        if let Some(p) = std::env::var_os("MXRUN_TEST_LNK") {
            required.push(("lnk", p.to_string_lossy().into_owned()));
        }

        let mut checked = Vec::new();
        for (label, target) in &required {
            let src = icon_source(target).unwrap_or_else(|| panic!("{label}: no source"));
            let (w, h, rgba) = icon_pixels(&src, 48)
                .unwrap_or_else(|| panic!("{label}: shell returned no image for {src:?}"));
            assert_eq!(rgba.len(), w * h * 4, "{label}: pixel buffer size");
            assert_eq!((w, h), (48, 48), "{label}: requested size not honoured");
            // An all-transparent icon means we read the wrong memory.
            assert!(
                rgba.chunks(4).any(|p| p[3] > 0),
                "{label}: icon is entirely transparent"
            );
            checked.push(*label);
        }
        println!("verified icon types: {checked:?}");
    }

    // ---- parameter prompt (P1-1) ------------------------------------------

    fn history(entries: &[(&str, u32)]) -> Vec<(String, u32)> {
        entries.iter().map(|(v, n)| (v.to_string(), *n)).collect()
    }

    fn prompt_with(entries: &[(&str, u32)], run_line: bool) -> Prompt {
        let item = Item {
            id: "p".into(),
            title: "百度搜索".into(),
            keywords: vec!["b".into()],
            actions: vec![Action::open("http://www.baidu.com/s?wd=")],
            ..Default::default()
        };
        let mut action = item.default_action().cloned().unwrap();
        if run_line {
            action.effect = Effect::Builtin { verb: BuiltinVerb::RunInput };
        }
        Prompt::new(item, action, history(entries))
    }

    #[test]
    fn history_filter_is_case_insensitive_and_keeps_order() {
        let h = history(&[("Rust 教程", 3), ("egui notes", 2), ("rust weekly", 1)]);
        assert_eq!(filter_history(&h, ""), vec![0, 1, 2], "empty query: all of it");
        assert_eq!(filter_history(&h, "  "), vec![0, 1, 2], "blank query too");
        assert_eq!(filter_history(&h, "rust"), vec![0, 2], "order = use count");
        assert_eq!(filter_history(&h, "RUST"), vec![0, 2], "case-insensitive");
        assert_eq!(filter_history(&h, "gui"), vec![1], "substring, not prefix");
        assert!(filter_history(&h, "没有这个").is_empty());
    }

    /// Enter uses the typed text until a suggestion is highlighted.
    #[test]
    fn prompt_value_prefers_the_highlighted_suggestion() {
        let mut p = prompt_with(&[("rust", 5), ("egui", 1)], false);
        assert_eq!(p.value(), "", "nothing typed yet");

        p.text = "  my own query  ".into();
        p.refilter();
        assert_eq!(p.value(), "my own query", "typed text is trimmed");
        assert_eq!(p.suggestions.len(), 0, "no history matches");

        // Down highlights the first suggestion of the list, and that is what
        // Enter then runs; the typed text is left alone.
        p.text.clear();
        p.refilter();
        p.move_cursor(1);
        assert_eq!(p.cursor, Some(0));
        assert_eq!(p.value(), "rust");
        p.move_cursor(1);
        assert_eq!(p.value(), "egui");
        // Clamped, not wrapped: holding Down must not jump back to the top.
        p.move_cursor(1);
        assert_eq!(p.value(), "egui");

        // Typing again drops the highlight.
        p.text = "ru".into();
        p.refilter();
        assert_eq!(p.cursor, None);
        assert_eq!(p.suggestions, vec![0]);
        assert_eq!(p.value(), "ru");
    }

    #[test]
    fn prompt_cursor_stays_none_without_history() {
        let mut p = prompt_with(&[], false);
        p.move_cursor(1);
        assert_eq!(p.cursor, None);
        assert_eq!(p.value(), "");
        assert!(p.history.is_empty());
        assert_eq!(p.empty_hint(), "请输入参数（Esc 返回）");
    }

    /// `运行` takes the text as the command line itself, and says so.
    #[test]
    fn run_input_prompt_is_recognised() {
        let p = prompt_with(&[], true);
        assert!(p.run_line);
        assert_eq!(p.empty_hint(), "输入要运行的命令（Esc 返回）");
        let q = prompt_with(&[], false);
        assert!(!q.run_line, "a search template is not a command line");
    }
}

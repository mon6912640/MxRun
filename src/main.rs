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

mod discover;
mod exec;
mod import;
mod integrate;
mod ipc;
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
    Arc, Mutex,
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

/// The launcher's own size, and the size of the small add/edit card.
///
/// AltRun showed the add dialog as a separate little window and deliberately
/// kept the launcher itself off screen (`docs/AltRun交互规格.md` §11). MxRun
/// uses the same window reshaped instead of a second one: same "a small box
/// appears, you confirm, it goes away" experience, without a second window to
/// keep in sync (focus, z-order, backdrop, and `main_hwnd` all stay as they
/// are). Documented as a deliberate deviation.
const LAUNCHER_SIZE: [f32; 2] = [680.0, 400.0];
const ADD_SIZE: [f32; 2] = [540.0, 300.0];
/// The manager is wider on purpose: command lines are long, and AltRun's own
/// manager gave the command column 400px for the same reason.
const MANAGER_SIZE: [f32; 2] = [880.0, 560.0];
/// Settings grew past the launcher's height (hotkey + discovery + integration),
/// and a settings page that hides its own buttons is worse than a taller card.
/// It scrolls too, so nothing can fall off the bottom again.
const SETTINGS_SIZE: [f32; 2] = [680.0, 560.0];

// ---------- palette ----------
//
// The card's colours live in *two* tables, picked at runtime. They used to be
// scattered as `from_gray(78)` … `from_gray(150)` in a couple of dozen places —
// seven greys with no hierarchy, which is what made the card look muddy. Three
// tiers now, plus the accent, and a light twin for the light theme.
//
// `pal()` is a plain atomic read: the theme changes about once in a session, and
// threading a `&Palette` through every closure that draws a row was not worth
// the borrow friction.
struct Palette {
    /// Card background. Opaque on purpose: translucency hurt reading (2026-09-19).
    card: Color32,
    /// 1px hairline around the card; scaled by the summon animation.
    stroke_alpha: f32,
    /// Primary text: item titles, headings.
    text: Color32,
    /// Secondary text: the detail line, notes, hints that must stay readable.
    text_dim: Color32,
    /// Tertiary text: status line, launch counts, the "未启动" placeholder.
    text_faint: Color32,
    /// Matched characters and the keyword column.
    accent: Color32,
    /// Success / enabled state.
    ok: Color32,
    /// Delete confirmation.
    danger: Color32,
    /// Selected row: a quiet wash plus a 3px accent bar on the left.
    selected_bg: Color32,
    /// Hovered row: fainter still, and it never moves the selection.
    hover_bg: Color32,
    /// The drag grip on the title strip.
    grip: Color32,
    /// The mode chip's background.
    chip_bg: Color32,
    /// Scroll bar handle (idle / hovered).
    scroll: Color32,
    scroll_hover: Color32,
}

const DARK: Palette = Palette {
    card: Color32::from_rgb(26, 27, 34),
    stroke_alpha: 24.0,
    text: Color32::from_gray(235),
    text_dim: Color32::from_gray(155),
    text_faint: Color32::from_gray(105),
    accent: Color32::from_rgb(255, 190, 80),
    ok: Color32::from_rgb(140, 220, 140),
    danger: Color32::from_rgb(255, 140, 120),
    selected_bg: Color32::from_rgba_premultiplied(15, 15, 15, 15),
    hover_bg: Color32::from_rgba_premultiplied(8, 8, 8, 8),
    grip: Color32::from_gray(74),
    chip_bg: Color32::from_rgba_premultiplied(10, 10, 10, 10),
    scroll: Color32::from_gray(52),
    scroll_hover: Color32::from_gray(78),
};

const LIGHT: Palette = Palette {
    // Slightly cool white, so it does not glare like pure #FFF under a lamp.
    card: Color32::from_rgb(250, 250, 252),
    stroke_alpha: 26.0,
    text: Color32::from_gray(24),
    text_dim: Color32::from_gray(96),
    text_faint: Color32::from_gray(140),
    // The dark theme's amber is unreadable on white; this is the same hue,
    // darkened until it passes on a light background.
    accent: Color32::from_rgb(176, 106, 0),
    ok: Color32::from_rgb(20, 128, 64),
    danger: Color32::from_rgb(196, 40, 40),
    selected_bg: Color32::from_rgba_premultiplied(14, 14, 14, 14),
    hover_bg: Color32::from_rgba_premultiplied(7, 7, 7, 7),
    grip: Color32::from_gray(198),
    chip_bg: Color32::from_rgba_premultiplied(10, 10, 10, 10),
    scroll: Color32::from_gray(198),
    scroll_hover: Color32::from_gray(168),
};

/// Config key for the theme choice (`system` / `dark` / `light`).
const THEME_KEY: &str = "theme";

/// 0 = dark, 1 = light. See [`pal`].
static THEME_LIGHT: AtomicBool = AtomicBool::new(false);

fn pal() -> &'static Palette {
    if THEME_LIGHT.load(Ordering::Relaxed) {
        &LIGHT
    } else {
        &DARK
    }
}

fn theme_is_light() -> bool {
    THEME_LIGHT.load(Ordering::Relaxed)
}

/// The user's theme choice — `system` (default) / `dark` / `light`, stored in
/// the config table under `theme`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ThemeChoice {
    System,
    Dark,
    Light,
}

impl ThemeChoice {
    fn as_str(self) -> &'static str {
        match self {
            ThemeChoice::System => "system",
            ThemeChoice::Dark => "dark",
            ThemeChoice::Light => "light",
        }
    }

    fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "dark" => ThemeChoice::Dark,
            "light" => ThemeChoice::Light,
            _ => ThemeChoice::System,
        }
    }

    fn label(self) -> &'static str {
        match self {
            ThemeChoice::System => "跟随系统",
            ThemeChoice::Dark => "深色",
            ThemeChoice::Light => "浅色",
        }
    }
}

/// Is Windows itself in light mode?
///
/// `AppsUseLightTheme` under the personalisation key; a missing value counts as
/// dark, which is the theme this card was designed against.
fn system_theme_is_light() -> bool {
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, KEY_READ, REG_DWORD, RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
    };
    let subkey: Vec<u16> = "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize\0"
        .encode_utf16()
        .collect();
    let name: Vec<u16> = "AppsUseLightTheme\0".encode_utf16().collect();
    unsafe {
        let mut key = Default::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(subkey.as_ptr()),
            Some(0),
            KEY_READ,
            &mut key,
        )
        .is_err()
        {
            return false;
        }
        let mut value: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        let mut kind = Default::default();
        let ok = RegQueryValueExW(
            key,
            windows::core::PCWSTR(name.as_ptr()),
            None,
            Some(&mut kind),
            Some(&mut value as *mut u32 as *mut u8),
            Some(&mut size),
        )
        .is_ok()
            && kind == REG_DWORD;
        let _ = RegCloseKey(key);
        ok && value != 0
    }
}

/// Resolve a choice into the active palette. `MXRUN_THEME` (`system|dark|light`)
/// wins over the stored setting — that is how the screenshots and quick A/B
/// runs drive it.
fn apply_theme_choice(choice: ThemeChoice) {
    let choice = match std::env::var("MXRUN_THEME") {
        Ok(v) if !v.trim().is_empty() => ThemeChoice::parse(&v),
        _ => choice,
    };
    // Remember what Windows itself says, so the settings page can describe the
    // *system* rather than whatever override happens to be in force.
    let system = system_theme_is_light();
    SYSTEM_LIGHT.store(system, Ordering::Relaxed);
    let light = match choice {
        ThemeChoice::Dark => false,
        ThemeChoice::Light => true,
        ThemeChoice::System => system,
    };
    THEME_LIGHT.store(light, Ordering::Relaxed);
}

/// What Windows reported the last time we looked.
static SYSTEM_LIGHT: AtomicBool = AtomicBool::new(false);

/// Row height driver: the icon, and the space before it.
const ICON_SIZE: f32 = 26.0;

/// Summon animation: fade in over this long while rising this far.
///
/// Kept deliberately short — a launcher's animation budget is stricter than any
/// other app's (the design note says "跟手但不拖沓"). `MXRUN_ANIM_MS=0` turns it
/// off entirely for anyone who wants the AltRun snap.
const ANIM_MS_DEFAULT: f32 = 100.0;
const ANIM_RISE: f32 = 4.0;

/// Paths this instance was started with (before eframe owns the process).
static PENDING_PATHS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

/// Config key for the auto-discovery switch (absent = on).
const DISCOVERY_KEY: &str = "discovery";

/// How much of the best score a row needs to stay on screen.///
/// With a realistic list a short query fuzzy-matches almost everything, because
/// nucleo happily finds subsequences inside the pinyin expansion ("dos" matches
/// "Win**d**ows" → d-o-s). The real hit still wins by an order of magnitude, so
/// the long tail is dropped rather than displayed. Measured with the sample
/// profile: `dos` scored 1088 for the intended row and 16–63 for the noise.
const SCORE_GATE: f64 = 0.15;

/// Append a timestamped line to `<data dir>\mxrun.log` (debug aid).
///
/// The data dir is `%APPDATA%\MxRun`, or `data\` beside the exe in portable
/// mode — the log follows the database so a portable folder stays
/// self-contained.
fn log_line(msg: &str) {
    let path = store::default_data_dir().join("mxrun.log");
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
                "import: lines={} commands={} imported={} needs_input={} builtin={} removed_seed={} skipped(sep={} unwanted={} unsupported={} deleted={})",
                report.lines,
                report.commands,
                report.imported,
                report.needs_input,
                report.builtin,
                report.removed_seed,
                report.skipped_separator,
                report.skipped_unwanted,
                report.skipped_unsupported,
                report.skipped_deleted
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

/// Install or remove the two Explorer entry points from the command line.
///
/// The same operations are buttons in the settings page; having them as
/// switches too means a portable copy can register itself, and it is the only
/// way to verify the registry/shortcut work without clicking.
fn run_integration(install: bool) -> bool {
    let mut lines: Vec<String> = Vec::new();
    let mut failed = false;

    let sendto = if install {
        integrate::install_sendto().map(|p| format!("已加入「发送到」：{}", p.display()))
    } else {
        integrate::uninstall_sendto().map(|()| "已从「发送到」移除".to_string())
    };
    match sendto {
        Ok(msg) => lines.push(msg),
        Err(e) => {
            lines.push(format!("发送到：{e}"));
            failed = true;
        }
    }

    let menu = if install {
        integrate::install_shell_menu().map(|n| format!("已注册 {n} 处右键菜单（文件 / 目录 / 目录空白处）"))
    } else {
        integrate::uninstall_shell_menu().map(|()| "已移除右键菜单".to_string())
    };
    match menu {
        Ok(msg) => lines.push(msg),
        Err(e) => {
            lines.push(format!("右键菜单：{e}"));
            failed = true;
        }
    }

    for line in &lines {
        log_line(&format!("integration: {line}"));
    }
    // Answering through the command line counts as answering: a scripted setup
    // should not leave a question pending for the next interactive start.
    integrate::mark_integration_asked();
    message_box(
        &format!(
            "{}\n\n之后：右键一个文件/文件夹 → 发送到 → MxRun（或右键菜单）。\n\
             MxRun 只会弹出一个小确认框，主窗口不会出现。",
            lines.join("\n")
        ),
        if install { "MxRun 右键集成" } else { "MxRun 移除右键集成" },
        failed,
    );
    true
}

fn main() -> eframe::Result<()> {
    // Anchor the clock the wake probe and the icon timings both read.
    PROC_START.get_or_init(std::time::Instant::now);

    let args: Vec<String> = std::env::args().skip(1).collect();

    // Import mode runs headless and is checked *before* the single-instance
    // guard on purpose: it is a maintenance command, and answering "MxRun is
    // already running" to `--import` would be useless. redb's own file lock
    // already prevents two writers, and the failure is reported in the dialog.
    if let Some(cli) = import::parse_args(args.iter().cloned()) {
        run_import(cli);
        return Ok(());
    }

    if args.iter().any(|a| a == "--install-integration") {
        run_integration(true);
        return Ok(());
    }
    if args.iter().any(|a| a == "--uninstall-integration") {
        run_integration(false);
        return Ok(());
    }

    // Anything else on the command line is a path the user pointed at us:
    // "发送到 → MxRun", the shell context menu, or a drop on the exe.
    let paths: Vec<String> = args
        .iter()
        .filter(|a| !a.starts_with("--"))
        .map(|a| a.trim().trim_matches('"').to_string())
        .filter(|a| !a.is_empty())
        .collect();

    if !acquire_single_instance() {
        // Someone else is already running: hand the paths over and leave.
        // No dialog — "不打扰" is the whole point of this interaction
        // (docs/AltRun交互规格.md §11), and AltRun's own "already running"
        // message box was the thing users had to click away.
        match ipc::send(&paths) {
            Ok(file) => log_line(&format!(
                "handoff: sent {} path(s) to the running instance ({})",
                paths.len(),
                file.display()
            )),
            Err(e) => {
                log_line(&format!("handoff: failed: {e}"));
                message_box(
                    &format!("MxRun 已经在运行，但无法把路径交给它：{e}"),
                    "MxRun",
                    true,
                );
            }
        }
        return Ok(());
    }

    // First instance with paths: start straight in the add dialog. The viewport
    // is created at the dialog's size so the small card never flashes at
    // launcher size first.
    let start_with_paths = !paths.is_empty();
    if start_with_paths {
        let _ = PENDING_PATHS.set(paths);
    }
    let start_size = if start_with_paths {
        ADD_SIZE
    } else {
        LAUNCHER_SIZE
    };

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
            // Explicit: the taskbar / Alt+Tab entry. It already read "MxRun"
            // (eframe defaults to the crate name), but that would silently
            // follow a rename of the binary.
            .with_title("MxRun")
            .with_always_on_top()
            .with_inner_size(start_size)
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
                    &format!(
                        "MxRun 启动失败：\n\n{msg}\n\n数据目录：{}",
                        store::default_data_dir().display()
                    ),
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
    show_window_sized(ctx, None);
}

/// Show the window, optionally at a given size.
///
/// The size matters for the hand-off path: the add card must be small from its
/// very first frame. The window's own size is otherwise owned by the app (see
/// `MxRunApp::apply_window_size`), which is why `None` keeps it untouched.
fn show_window_sized(ctx: &egui::Context, size: Option<[f32; 2]>) {
    // Snapshot the window the user is in *before* we take focus: the
    // window-control actions (and {%wd}/{%wt}/{%wc}) refer to that window, not
    // to MxRun itself. AltRun captured the same values at hotkey-press time
    // (docs/AltRun交互规格.md §4).
    //
    // Not for the add hand-off: there the user is in Explorer, and clobbering
    // the snapshot would make "恢复窗口" bring back Explorer instead of the app
    // they were actually working in.
    if size.is_none() {
        exec::remember_foreground();
    }
    WINDOW_VISIBLE.store(true, Ordering::Relaxed);
    match main_hwnd() {
        Some(hwnd) => unsafe {
            let mut rect = std::mem::zeroed();
            let _ = GetWindowRect(hwnd, &mut rect);
            let current = [
                (rect.right - rect.left) as f32,
                (rect.bottom - rect.top) as f32,
            ];
            let [ww, wh] = size.unwrap_or(current);

            // Where to put it: the position the user dragged it to last time
            // (AltRun's WinTop/WinLeft), clamped so a monitor change cannot
            // leave the card off-screen; otherwise centred at 1/3 height.
            let (x, y) = place_for([ww, wh]);
            if remembered_position().is_some() {
                log_line(&format!("window: placed at {x},{y} (remembered)"));
            }
            let flags = if size.is_some() {
                SWP_SHOWWINDOW
            } else {
                SWP_NOSIZE | SWP_SHOWWINDOW
            };
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                x,
                y,
                ww as i32,
                wh as i32,
                flags,
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

/// Where the user last dragged the card (AltRun's WinTop/WinLeft).///
/// Kept in a process-wide slot rather than read from the store on every show:
/// `show_window` is called from the hotkey thread, which does not own the
/// database. The app fills it in at startup and after every drag.
static WINDOW_POS: std::sync::Mutex<Option<(i32, i32)>> = std::sync::Mutex::new(None);

fn remembered_position() -> Option<(i32, i32)> {
    WINDOW_POS.lock().ok().and_then(|p| *p)
}

fn set_remembered_position(pos: Option<(i32, i32)>) {
    if let Ok(mut slot) = WINDOW_POS.lock() {
        *slot = pos;
    }
}

/// The stored position (AltRun's `WinTop`/`WinLeft`), read from the config.
fn stored_position(store: &Store) -> Option<(i32, i32)> {
    let x = store.get_config("win_x")?.parse::<i32>().ok()?;
    let y = store.get_config("win_y")?.parse::<i32>().ok()?;
    Some((x, y))
}

/// Where a window of this size should go: the remembered spot if there is one,
/// otherwise centred at 1/3 height. Always clamped into the screen, so a
/// remembered position from a monitor that is no longer attached cannot leave
/// the card invisible.
fn place_for(size: [f32; 2]) -> (i32, i32) {
    let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    let (w, h) = (size[0] as i32, size[1] as i32);
    match remembered_position() {
        Some((x, y)) => (x.clamp(0, (sw - w).max(0)), y.clamp(0, (sh - h).max(0))),
        None => ((sw - w) / 2, (sh - h) / 3),
    }
}

/// Where the card is right now, straight from the window manager.
fn current_position() -> Option<(i32, i32)> {
    let hwnd = main_hwnd()?;
    let mut rect = unsafe { std::mem::zeroed() };
    unsafe { GetWindowRect(hwnd, &mut rect) }.ok()?;
    Some((rect.left, rect.top))
}

/// Hand the window to the OS and let it run its own move loop, the way every
/// Win32 app does (`ReleaseCapture` + `WM_NCLBUTTONDOWN` with `HTCAPTION`).
///
/// The call blocks until the drag ends, which is exactly what makes remembering
/// the position easy: when it returns, the new position is final.
fn drag_window(hwnd: HWND) {
    use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
    unsafe {
        let _ = ReleaseCapture();
        let _ = SendMessageW(
            hwnd,
            WM_NCLBUTTONDOWN,
            Some(WPARAM(HTCAPTION as usize)),
            Some(LPARAM(0)),
        );
        // The modal move loop underneath that call pumps its own messages and
        // swallows the mouse ones: winit never sees the button come up, so egui
        // keeps believing the pointer is down and **ignores the next drag**
        // (measured with synthesized input 2026-09-19: every second drag did
        // nothing — which is exactly what "I can't drag it" felt like). Post
        // the release ourselves so the input state catches up.
        let _ = PostMessageW(Some(hwnd), WM_LBUTTONUP, WPARAM(0), LPARAM(0));
    }
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

/// Did DWM accept our request for native rounded corners? (Logged; the card's
/// own radius no longer depends on it — see `CARD_RADIUS`.)
static DWM_ROUNDED: AtomicBool = AtomicBool::new(false);

/// Our own corner radius.
///
/// **Deliberately the same as Win11's `DWMWCP_ROUND`** (8px): the two arcs then
/// coincide instead of leaving a crescent of desktop between them — which is
/// exactly what "the wallpaper bleeds into the corner" was (user, 2026-09-19).
/// Painting it even when DWM is doing the clipping also means we cannot end up
/// with square corners if DWM decides not to round a transparent window.
const CARD_RADIUS: u8 = 8;

/// Ask DWM for its own rounded corners (Win11 22H2+) and remember the answer.
///
/// On Win11 this is also what gets the window its drop shadow (which a
/// PrintWindow capture cannot show, so it needs a human eye).
///
/// `MXRUN_ROUND=0` turns it off (the corners then come from `CARD_RADIUS`).
fn apply_round_corners(hwnd: HWND) {
    if std::env::var("MXRUN_ROUND").is_ok_and(|v| v == "0") {
        log_line("corners: disabled by MXRUN_ROUND=0");
        return;
    }
    // DWMWA_WINDOW_CORNER_PREFERENCE = 33, DWMWCP_ROUND = 2 (Win11 22H2+).
    const DWMWA_WINDOW_CORNER_PREFERENCE: i32 = 33;
    const DWMWCP_ROUND: i32 = 2;
    let r = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWINDOWATTRIBUTE(DWMWA_WINDOW_CORNER_PREFERENCE),
            &DWMWCP_ROUND as *const i32 as *const core::ffi::c_void,
            std::mem::size_of::<i32>() as u32,
        )
    };
    let ok = r.is_ok();
    DWM_ROUNDED.store(ok, Ordering::Relaxed);
    log_line(&format!(
        "corners: DwmSetWindowAttribute(CORNER_PREFERENCE=ROUND) -> {r:?} (dwm_owns_corners={ok})"
    ));
}

/// How long the summon animation runs; `MXRUN_ANIM_MS=0` disables it.
fn anim_ms() -> f32 {
    std::env::var("MXRUN_ANIM_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(ANIM_MS_DEFAULT)
}

/// The window's current size in points — used to re-place it during the rise.
fn applied_size_of(hwnd: HWND) -> [f32; 2] {
    let mut rect = unsafe { std::mem::zeroed() };
    unsafe { GetWindowRect(hwnd, &mut rect) }.ok();
    [
        (rect.right - rect.left) as f32,
        (rect.bottom - rect.top) as f32,
    ]
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
/// Set by the tray's 退出 just before it closes the window, so the close handler
/// knows this one is meant (see the `close_requested` check in `App::ui`).
static QUITTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn close_window() {
    QUITTING.store(true, Ordering::Relaxed);
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
    // Shell namespace paths have no file behind them but the shell still knows
    // how to image them — this is how a UWP app's logo is obtained
    // (`shell:AppsFolder\<AUMID>`). Passing them through unchanged is the whole
    // trick; `exists()` would always say no.
    if t.len() > 6 && t[..6].eq_ignore_ascii_case("shell:") {
        return Some(std::path::PathBuf::from(t));
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
        // Debug: UWP app entries (`shell:AppsFolder\<AUMID>`) are the one kind of
        // target with no file behind it, so say out loud how far the shell gets
        // with one.
        let app_entry = path.to_string_lossy().starts_with("shell:AppsFolder");
        let created: windows::core::Result<IShellItemImageFactory> =
            SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None);
        if app_entry && created.is_err() {
            // Worth a line: the app entry failed to resolve, so this row will
            // keep its emoji placeholder.
            crate::log_line(&format!(
                "icons: app entry {:?} not resolvable ({:?})",
                path,
                created.as_ref().err().map(|e| e.code().0)
            ));
        }
        let item = created.ok()?;
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
    /// Unix seconds of the last launch — the `Ctrl+L` recent list sorts on it.
    last_used: u64,
}

struct Scored {
    idx: usize,
    score: f64,
    /// The query hits a *token start* in the searchable text (`chrome` for
    /// `chr`, `calc` for `c`) rather than being scattered through it
    /// (`c`…`o`…`c`…`o`…`s` in "Cocos Dashboard"). Short queries only accept
    /// these; see [`MIN_FUZZY_CHARS`].
    strong: bool,
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
        strong: is_token_start(&it.searchable, &q),
        hit_indices: hits,
    })
}

/// Does `query` sit at the start of some word in `haystack`?
///
/// The searchable text is lowercase with the fields — and the per-syllable
/// pinyin — separated by spaces, so "a word start" is exactly what a user means
/// by "starts with": `chr` is a word start in "chrome", not in "cocos dashboard".
/// Cheap (a handful of comparisons per item) and pure enough to test.
fn is_token_start(haystack: &str, query: &str) -> bool {
    if query.is_empty() {
        return false;
    }
    if haystack.starts_with(query) {
        return true;
    }
    haystack
        .match_indices(' ')
        .any(|(i, _)| haystack[i + 1..].starts_with(query))
}

/// Queries this short only accept token-start matches. One or two letters is a
/// "find me something starting with this" gesture; letting them fuzzy-match
/// everywhere is what filled the list with noise (125 of 198 items matched `e`,
/// 2026-09-19). Measured with the diagnostic: at 3, `e` drops from eight rows to
/// six all-real ones and `ch`/`py` lose their scattered filler, while the
/// hand-written one- and two-letter keywords (`b`, `g`, `zd`) still match as
/// token starts and stay.
const MIN_FUZZY_CHARS: usize = 3;

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
    // A UWP app's target is its AUMID (`shell:AppsFolder\Microsoft.Windows…!App`),
    // which is machine noise in a row. Say what it is instead.
    if target.starts_with("shell:AppsFolder\\") {
        return "Windows 应用".to_string();
    }
    truncate_chars(target, 64)
}

/// Build the searchable index for a list of items.
///
/// Free function (not a method) so the ranking can be measured and tested
/// without a window: the diagnostic below drives it directly on a copy of a real
/// database, which is how the "noise" question gets answered with numbers rather
/// than taste.
fn index_items(items: Vec<Item>, store: &mut Store) -> Vec<Indexed> {
    items
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
            let frec = store.get_frecency(&item.id);
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
                last_used: frec.last_used,
            }
        })
        .collect()
}

/// Rank `items` for `query` exactly the way the launcher does, and return the
/// top rows with their scores. Used by the diagnostic and the tests — the point
/// is that it cannot drift from what the UI does.
#[cfg(test)]
fn rank_all(items: &[Indexed], query: &str, limit: usize) -> Vec<(String, f64, bool)> {
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buf: Vec<char> = Vec::new();
    let mut idx_buf: Vec<u32> = Vec::new();
    let mut scored: Vec<Scored> = Vec::new();
    for (idx, it) in items.iter().enumerate() {
        if let Some(sc) = rank_item(
            idx,
            it,
            query,
            &pattern,
            &mut matcher,
            &mut buf,
            &mut idx_buf,
        ) {
            scored.push(sc);
        }
    }
    let before_gate = scored.len();
    apply_score_gate(&mut scored);
    // Mirror the launcher: short queries drop scattered matches.
    if query.chars().count() < MIN_FUZZY_CHARS {
        scored.retain(|sc| sc.strong);
    }
    scored.sort_by(|a, b| b.score.total_cmp(&a.score));
    let _ = before_gate;
    scored
        .into_iter()
        .take(limit)
        .map(|sc| {
            (
                items[sc.idx].item.title.clone(),
                sc.score,
                sc.strong,
            )
        })
        .collect()
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

// ---------------------------------------------------------------------------
// Add / edit card (the way items get in)
// ---------------------------------------------------------------------------

/// Where the card came from — it decides what happens when it closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddOrigin {
    /// Right-click / 发送到 / `MxRun.exe "<path>"`. The launcher was never on
    /// screen, so confirming or cancelling leaves nothing behind (AltRun's
    /// "不打扰", `docs/AltRun交互规格.md` §11).
    External,
    /// Opened from the launcher itself (F2): closing returns to the list.
    Launcher,
}

/// The command line behind an action, when it has one. `None` for the effects
/// that are not a command (builtin verbs, clipboard, reveal) — those cannot be
/// meaningfully edited as text.
fn action_command(action: &Action) -> Option<&str> {
    match &action.effect {
        Effect::Open { target } => Some(target),
        Effect::Run { line } => Some(line),
        _ => None,
    }
}

/// The add / edit card: the same three fields AltRun's `frmShortCut`
/// pre-filled (关键字 / 名称 / 命令行), in the same window instead of a second
/// one.
struct AddForm {
    origin: AddOrigin,
    /// The item being changed (F2). `None` = creating a new one.
    editing: Option<Item>,
    /// The path this came in as. Identity for re-adds: same file, same row.
    source_path: String,
    /// Paths still waiting behind this one (a multi-file right-click).
    queue: Vec<String>,
    keyword: String,
    title: String,
    command: String,
    /// The first Enter on a colliding keyword only warns; the second replaces.
    overwrite_asked: bool,
    /// Why the last attempt was refused.
    error: String,
}

impl AddForm {
    /// A new item derived from a path (the right-click / 发送到 flow).
    fn from_path(path: &str, queue: Vec<String>) -> Self {
        let item = integrate::item_from_path(path);
        let command = item
            .default_action()
            .and_then(action_command)
            .unwrap_or_default()
            .to_string();
        Self {
            origin: AddOrigin::External,
            editing: None,
            source_path: path.to_string(),
            queue,
            keyword: item.keywords.first().cloned().unwrap_or(item.title.clone()),
            title: item.title.clone(),
            command,
            overwrite_asked: false,
            error: String::new(),
        }
    }

    /// An empty card, seeded with the query that matched nothing — AltRun's
    /// other way in ("无此项 "%s", 添加它?", `frmALTRun.pas:748-752`).
    fn blank(seed: &str) -> Self {
        Self {
            origin: AddOrigin::Launcher,
            editing: None,
            source_path: String::new(),
            queue: Vec::new(),
            keyword: seed.to_string(),
            title: seed.to_string(),
            command: String::new(),
            overwrite_asked: false,
            error: String::new(),
        }
    }

    /// The same card, filled in from an existing item (F2 编辑). `None` when the
    /// item has no editable command line (builtin verb, clipboard copy…).
    fn from_item(item: Item) -> Option<Self> {
        let command = action_command(item.default_action()?)?.to_string();
        Some(Self {
            origin: AddOrigin::Launcher,
            source_path: item.source.external_id.clone(),
            keyword: item
                .keywords
                .first()
                .cloned()
                .unwrap_or_else(|| item.title.clone()),
            title: item.title.clone(),
            command,
            editing: Some(item),
            queue: Vec::new(),
            overwrite_asked: false,
            error: String::new(),
        })
    }

    fn is_editing(&self) -> bool {
        self.editing.is_some()
    }

    /// "添加" or "保存", depending on what the card is doing.
    fn confirm_label(&self) -> &'static str {
        if self.is_editing() { "保存" } else { "添加" }
    }

    fn heading(&self) -> &'static str {
        if self.is_editing() { "✎ 编辑条目" } else { "＋ 添加条目" }
    }
}

/// What a row's context menu asked for (a manager row or a result row).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuAction {
    Edit,
    Delete,
    Reveal,
}

/// The manager's "where did this come from" filter. Auto-discovered items are
/// the ones worth reviewing in bulk, so they get their own switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceFilter {
    /// Hand-made, imported, and the demo rows.
    Curated,
    /// Found by the Start Menu scan.
    Discovered,
}

impl SourceFilter {
    fn matches(self, provider: &str) -> bool {
        match self {
            SourceFilter::Discovered => discover::is_discovered_provider(provider),
            SourceFilter::Curated => !discover::is_discovered_provider(provider),
        }
    }
}

/// A short note that shows itself for a couple of seconds and then takes the
/// window away with it.
///
/// Used for the one case where the add flow leaves something behind that the
/// user did not ask for: MxRun was **not** running, so the right-click started
/// it, and after the item is stored the process stays resident in the tray with
/// the hotkey registered (`docs/开发进度.md` §2.11). AltRun simply exited there;
/// the user asked for "stay resident, but say so".
struct Toast {
    /// When it should disappear.
    until: std::time::Instant,
    title: String,
    body: String,
}

/// How far back the `Ctrl+L` recent list looks.
const RECENT_WINDOW_SECS: u64 = 7 * 24 * 60 * 60;

/// Was this item used inside the recent window? Never-used items (`last_used`
/// 0) are not "recent" — AltRun's LatestList only ever held real launches.
///
/// A timestamp in the *future* counts as recent on purpose: clocks jump
/// backwards (NTP, DST, a manual fix), and that must not empty the list of
/// things the user just launched.
fn is_recent(last_used: u64, now: u64) -> bool {
    last_used > 0 && now.saturating_sub(last_used) <= RECENT_WINDOW_SECS
}

/// Which row a digit key asks for, given the modifiers that are held.
///
/// **A bare digit is never a command.** Digits are how you search for the items
/// that have them in their name — `360安全卫士`, `7-Zip`, `1password` — and a
/// launcher that swallowed `7` would make those unreachable. AltRun drew the
/// same line: its number keys only fire with `Alt` or `Ctrl`
/// (`frmALTRun.pas:1566`).
///
/// The tenth row is `0`, exactly as AltRun printed it in its index column.
fn digit_row(key: Key, ctrl: bool, alt: bool) -> Option<usize> {
    use egui::Key::*;
    if !ctrl && !alt {
        return None;
    }
    Some(match key {
        Num1 => 0,
        Num2 => 1,
        Num3 => 2,
        Num4 => 3,
        Num5 => 4,
        Num6 => 5,
        Num7 => 6,
        Num8 => 7,
        Num9 => 8,
        Num0 => 9,
        _ => return None,
    })
}

/// A bare digit runs a row **only while the search box is empty**.
///
/// With text in the box, digits are search text — `python 3.12`, `3.12`, `win
/// 11` all have to be typeable. The empty-box half is the same idea as AltRun's
/// "space with an empty box runs the first item": with nothing typed, a key is
/// a command; once you are typing, it is input.
///
/// Cost, measured before choosing it: no item in the real lists starts with a
/// digit (0 of 175 Start Menu shortcuts, 0 of 72 curated keywords), so nothing
/// becomes unreachable — and a digit-leading query can still be found by any
/// of its later characters ("zip" finds 7-Zip).
fn bare_digit_row(key: Key, input_is_empty: bool) -> Option<usize> {
    if !input_is_empty {
        return None;
    }
    digit_row(key, true, false)
}

/// `;` runs the second row and `'` the third — AltRun's own shortcuts
/// (`frmALTRun.pas:1588-1594`), kept as they were. The price is that neither
/// character can be typed into the search box; AltRun's keywords were always
/// plain identifiers, and so are the user's.
const SEMICOLON_ROW: usize = 1;
const QUOTE_ROW: usize = 2;

/// Which row a `Ctrl`/`Alt` + digit asks for, reading the keyboard. Our own
/// keys are *consumed* so the digits never reach the search box.
fn consume_digit(ctx: &egui::Context) -> Option<usize> {
    use egui::Key::*;
    const DIGITS: [Key; 10] = [Num0, Num1, Num2, Num3, Num4, Num5, Num6, Num7, Num8, Num9];
    for key in DIGITS {
        for modifiers in [egui::Modifiers::CTRL, egui::Modifiers::ALT] {
            if ctx.input_mut(|i| i.consume_key(modifiers, key))
                && let Some(row) = digit_row(key, modifiers.ctrl, modifiers.alt)
            {
                return Some(row);
            }
        }
    }
    None
}

/// The same for a bare digit with an empty search box (`bare_digit_row`).
fn consume_bare_digit(ctx: &egui::Context, input_is_empty: bool) -> Option<usize> {
    if !input_is_empty {
        return None;
    }
    use egui::Key::*;
    const DIGITS: [Key; 10] = [Num0, Num1, Num2, Num3, Num4, Num5, Num6, Num7, Num8, Num9];
    for key in DIGITS {
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, key))
            && let Some(row) = bare_digit_row(key, input_is_empty)
        {
            return Some(row);
        }
    }
    None
}

/// The calculator's mode prefix: `=1+2`.
///
/// It used to be implicit (any query that looked like an expression got
/// evaluated), which the bare-digit rules then broke — from an empty box the
/// leading `1` of `1+2` runs row 1 instead of being typed. A prefix fixes that
/// and gives the calculator a discoverable way in. `#` (row selector) works the
/// same way, and `@` (file search) will join them.
const CALC_PREFIX: char = '=';

/// Is the box in calculator mode? Leading space is forgiven, nothing else.
fn is_calc_mode(input: &str) -> bool {
    input.trim_start().starts_with(CALC_PREFIX)
}

/// The expression to evaluate: everything after the prefix.
fn calc_expression(input: &str) -> &str {
    input.trim().strip_prefix(CALC_PREFIX).unwrap_or("").trim()
}

/// Evaluate a calculator expression, formatted the way the list shows it.
///
/// `evalexpr` does *integer* arithmetic when both sides are integers, so
/// `100/8` came out as `12` — which is not what a calculator should say. When
/// the expression divides, the literals are turned into floats first
/// (`100/8` → `100.0/8.0` → `12.5`). Everything else keeps exact integer math,
/// which also stops `2^10` from printing as `1024.0000000000002`.
fn eval_calc(expr: &str) -> Option<String> {
    if expr.is_empty() {
        return None;
    }
    let source = if expr.contains('/') {
        floatify(expr)
    } else {
        expr.to_string()
    };
    let value = evalexpr::eval(&source).ok()?;
    let n = value.as_number().ok()?;
    // `1/0` is `inf` once the literals are floats; a calculator says "no"
    // rather than printing `inf`. Same for NaN.
    if !n.is_finite() {
        return None;
    }
    Some(format_number(n, expr.contains('/') || expr.contains('.')))
}

/// Rewrite numeric literals as floats: `100/8` → `100.0/8.0`.
fn floatify(expr: &str) -> String {
    let mut out = String::with_capacity(expr.len() + 8);
    let mut digits = String::new();
    for c in expr.chars() {
        if c.is_ascii_digit() || c == '.' {
            digits.push(c);
            continue;
        }
        push_literal(&mut digits, &mut out);
        out.push(c);
    }
    push_literal(&mut digits, &mut out);
    out
}

fn push_literal(digits: &mut String, out: &mut String) {
    if digits.is_empty() {
        return;
    }
    out.push_str(digits);
    if !digits.contains('.') {
        out.push_str(".0");
    }
    digits.clear();
}

/// Print a number the way a person would: `3`, not `3.0`; `0.3`, not
/// `0.30000000000000004`.
fn format_number(n: f64, float_math: bool) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        return format!("{}", n as i64);
    }
    if !float_math {
        return format!("{n}");
    }
    let trimmed = format!("{n:.10}");
    let trimmed = trimmed.trim_end_matches('0').trim_end_matches('.');
    // Rounding to ten decimals must not turn a tiny non-zero number into `0`.
    if trimmed.is_empty() || trimmed == "-" || trimmed.parse::<f64>() == Ok(0.0) {
        format!("{n}")
    } else {
        trimmed.to_string()
    }
}

/// What a trailing digit in the search box means.
///
/// AltRun's rule was "a trailing character that kills the search is a command,
/// not input" (`frmALTRun.pas:1411-1470`): digit = the Nth row of the list you
/// were just looking at, space = the row you had highlighted. We keep the idea
/// but adapt the trigger to a fuzzy matcher, which almost never returns nothing
/// (measured: even `zz` returns eight rows) — so "the digit changed nothing"
/// counts as well as "the digit killed the list".
#[derive(Debug, PartialEq, Eq)]
enum DigitVerdict {
    /// Run this zero-based row of the *previous* list.
    Run(usize),
    /// Not a digit at the end — nothing to decide.
    NotADigit,
    /// The prefix is not what the previous list was built from: the input
    /// changed by more than one appended character (paste, delete, a click).
    NotAnAppend,
    /// The digit is part of a name that has digits in it (`pycharm64`, `x64`,
    /// `Python 3.12`): the user is *typing* a number, not picking a row.
    PrefixHasDigit,
    /// The digit is doing real filtering (`b` → `b3` went 8 rows → 2).
    DigitFilters,
    /// The previous list did not reach that row (AltRun's second guard).
    NoSuchRow,
    /// There was no previous list to point at.
    NoPreviousList,
}

/// Decide what a trailing digit means. Pure, so the rules can be tested without
/// a keyboard (which Windows makes impossible to automate here).
///
/// - `input` — the search box as it is now
/// - `prev_input` — the search box as it was one keystroke ago
/// - `prev_rows` — how many rows that list showed
/// - `cur_rows` — how many rows the list shows now
/// - `same_as_prev` — the current rows are the very same items as before
fn digit_verdict(
    input: &str,
    prev_input: &str,
    prev_rows: usize,
    cur_rows: usize,
    same_as_prev: bool,
) -> DigitVerdict {
    let Some(digit) = input.chars().last().and_then(|c| c.to_digit(10)) else {
        return DigitVerdict::NotADigit;
    };
    // The digit has to be the one character that was just appended, otherwise
    // the "previous list" is not the list this digit was typed against.
    let prefix = &input[..input.len() - 1];
    if prefix != prev_input {
        return DigitVerdict::NotAnAppend;
    }
    if prefix.is_empty() {
        // The empty box is its own rule (`consume_bare_digit`): the list on
        // screen is the one to count rows in.
        return DigitVerdict::NoPreviousList;
    }
    // Safety belt the original did not need: in a fuzzy matcher the digit the
    // user is *typing* usually does not change anything either (`pycharm` →
    // `pycharm6` still matches `pycharm64`), and 19 of 177 titles here contain
    // digits. A prefix that already has one is a name being typed.
    if prefix.chars().any(|c| c.is_ascii_digit()) {
        return DigitVerdict::PrefixHasDigit;
    }
    if prev_rows == 0 {
        return DigitVerdict::NoPreviousList;
    }
    // AltRun: the digit killed the search. Ours: or it changed nothing at all.
    if cur_rows > 0 && !same_as_prev {
        return DigitVerdict::DigitFilters;
    }
    // `0` is the tenth row, exactly as AltRun's index column printed it.
    let row = if digit == 0 { 10 } else { digit as usize };
    if prev_rows < row {
        return DigitVerdict::NoSuchRow;
    }
    DigitVerdict::Run(row - 1)
}

/// The explicit selector: `b#3` = the third row of the results for `b`, `#3` =
/// the third row of the whole list. Returns the query to search for and, once a
/// digit follows the `#`, the zero-based row it names.
///
/// `#` is a *mode marker*, not a key binding: both characters are still typed
/// normally, we just stop treating everything from `#` on as search text. The
/// cost is that `#` cannot start a query (nothing in the real lists does).
fn split_selector(input: &str) -> (&str, Option<usize>) {
    let Some((query, sel)) = input.split_once('#') else {
        return (input, None);
    };
    // `#` alone, or `#x`: the selector is being typed (or was a typo). The query
    // half still drives the list, which is what makes `b#3` readable while you
    // type it.
    let digits: String = sel.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() != sel.len() {
        return (query, None);
    }
    match digits.parse::<usize>() {
        // 1-based on screen; `0` is the tenth row, like everywhere else here.
        Ok(0) => (query, Some(9)),
        Ok(n) => (query, Some(n - 1)),
        Err(_) => (query, None),
    }
}

/// The list as it was one keystroke ago — AltRun's `m_LastShortCutPointerList`
/// (`frmALTRun.pas:2350`), which is what its digit and space commands point at.
///
/// Kept by item **id**, not by index: deleting an item shifts every index after
/// it, and firing at the wrong row is exactly the kind of bug that is invisible
/// until it launches something embarrassing.
#[derive(Clone, Default)]
struct ListSnapshot {
    /// The search box's text when this list was built.
    input: String,
    /// Visible rows, in display order.
    ids: Vec<String>,
}

/// What the last keystroke decided, for the log.
impl DigitVerdict {
    fn reason(&self) -> &'static str {
        match self {
            DigitVerdict::Run(_) => "run",
            DigitVerdict::NotADigit => "not-a-digit",
            DigitVerdict::NotAnAppend => "not-an-append",
            DigitVerdict::PrefixHasDigit => "prefix-has-digit",
            DigitVerdict::DigitFilters => "digit-filters",
            DigitVerdict::NoSuchRow => "no-such-row",
            DigitVerdict::NoPreviousList => "no-previous-list",
        }
    }
}

/// Where the last discovery run ended up, shared with the UI thread.
#[derive(Default)]
struct DiscoveryState {
    /// `None` until a scan finishes.
    output: Option<discover::ScanOutput>,
    /// Set while a scan is running, so a second one is not started.
    running: bool,
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
    /// Card opacity, `MXRUN_CARD_ALPHA` (0-255). **Default 255 = opaque**: a
    /// translucent card let the desktop bleed through and made the list tiring
    /// to read (reported 2026-09-19). 235 was the old look.
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
    /// Tray menu asked for a blank add card.
    open_new: Arc<AtomicBool>,
    /// Paths handed over by another process (fill → add dialog).
    pending_adds: Arc<Mutex<Vec<String>>>,
    /// Open add / edit card. Replaces the whole window content while it lives.
    add: Option<AddForm>,
    /// Self-dismissing note (see [`Toast`]).
    toast: Option<Toast>,
    /// First-run question: "shall MxRun join the right-click menu?"
    ask_integration: bool,
    /// Manager view (`快捷项管理`): every item, filterable, with edit/delete.
    manage: bool,
    /// Tray menu asked for the manager.
    open_manage: Arc<AtomicBool>,
    /// Id armed by the first `Delete` — the confirmation, without a modal.
    pending_delete: Option<String>,
    /// Manager filter: `None` = everything, otherwise one side of the list.
    manage_source: Option<SourceFilter>,
    /// Auto-discovery (P0-4): shared with the scan thread.
    discovery: Arc<Mutex<DiscoveryState>>,
    /// Scan once the first frame is on screen; cleared afterwards.
    discovery_pending: Option<bool>, // Some(force)
    /// Last discovery outcome, for the settings page.
    discovery_note: String,
    /// `Ctrl+L`: show only recently used items (AltRun's 最近列表).
    recent_only: bool,
    /// The box holds a `=` expression: no list, just a result.
    calc_mode: bool,
    /// The user's theme choice (`theme` in the config table).
    theme: ThemeChoice,
    /// When the current summon animation started (`None` = settled).
    anim: Option<std::time::Instant>,
    /// Animation progress 0..1, and the window offset that goes with it.
    anim_t: f32,
    anim_dy: f32,
    /// One-shot flag for the `MXRUN_SELFTEST_RECTS` bookkeeping.
    logged_drag_rect: bool,
    /// Which row the pointer was over last frame — only for the faint hover
    /// tint. It deliberately does **not** move the selection.
    hover_row: Option<usize>,
    /// The list one keystroke ago (`ListSnapshot`) — what a trailing digit
    /// points at.
    prev_list: ListSnapshot,
    /// Set when the selection moved by keyboard, so the list can scroll it back
    /// into view (the launcher list scrolls now that eight rows no longer fit).
    scroll_selected: bool,
    /// This process was started *by* an add request (right-click → 发送到 while
    /// nothing was running). Only then does the "MxRun is now resident" note
    /// make sense.
    cold_start_add: bool,
    /// The note is shown once per process, not once per queued file.
    add_note_shown: bool,
    /// Window size the app last applied, so it only resizes on a mode change.
    applied_size: [f32; 2],
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

        // Keep the Explorer entries pointing at *this* copy. A portable folder
        // that moved (or a USB stick with a new drive letter) would otherwise
        // leave menu items aiming at a path that no longer exists. Anything the
        // user never registered stays untouched — see `repair_if_registered`.
        for note in integrate::repair_if_registered() {
            log_line(&format!("integration: {note}"));
        }
        log_line(&format!(
            "startup: data_dir={} portable={} integration(sendto={} menu={})",
            store::default_data_dir().display(),
            store::is_portable(),
            integrate::sendto_installed(),
            integrate::shell_menu_installed()
        ));

        // A machine whose Explorer has never heard of MxRun gets asked once
        // (see `integrate::mark_integration_asked`). Skipped when the window is
        // opening for a right-click add — the user is busy with a card — and
        // when a script is driving us.
        let started_for_add = PENDING_PATHS.get().is_some_and(|p| !p.is_empty());
        let ask_integration = !started_for_add
            && !integrate::any_registered()
            && !integrate::integration_asked()
            && std::env::var("MXRUN_SELFTEST").is_err();
        if ask_integration {
            log_line("integration: nothing registered on this machine, asking");
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
        // Order follows AltRun's own menu (显示 / 快捷项管理 / 配置 / 退出).
        let menu = Menu::new();
        let toggle_item = MenuItem::new("显示 / 隐藏  MxRun", true, None);
        let manage_item = MenuItem::new("快捷项管理…", true, None);
        let new_item = MenuItem::new("新建条目…", true, None);
        let settings_item = MenuItem::new("设置", true, None);
        let quit_item = MenuItem::new("退出 MxRun", true, None);
        let _ = menu.append(&toggle_item);
        let _ = menu.append(&manage_item);
        let _ = menu.append(&new_item);
        let _ = menu.append(&settings_item);
        let _ = menu.append(&quit_item);
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("MxRun")
            .with_icon(build_tray_icon())
            .build()
            .map_err(|e| format!("无法创建托盘图标：{e}"))?;

        // --- background event thread: hotkey + tray events + the hand-off inbox ---
        // Must NOT live inside App::ui(): eframe sleeps while the window is
        // hidden, so ui() would stop running and the window could never be
        // re-shown (the original "hidden = dead" bug). The inbox is polled here
        // for the same reason — a request that arrives while the launcher is
        // hidden has to be able to bring it back.
        let ctx2 = cc.egui_ctx.clone();
        let toggle_id = toggle_item.id().clone();
        let manage_id = manage_item.id().clone();
        let new_id = new_item.id().clone();
        let settings_id = settings_item.id().clone();
        let quit_id = quit_item.id().clone();
        let open_settings = Arc::new(AtomicBool::new(false));
        let open_settings2 = open_settings.clone();
        let open_manage = Arc::new(AtomicBool::new(false));
        let open_manage2 = open_manage.clone();
        let open_new = Arc::new(AtomicBool::new(false));
        let open_new2 = open_new.clone();
        let pending_adds: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let pending_adds2 = pending_adds.clone();
        std::thread::spawn(move || {
            let mut tick: u32 = 0;
            // Single click toggles the window (AltRun's behaviour), and a double
            // click must not toggle it twice — the second click inside this
            // window is swallowed.
            let mut last_tray_click: Option<std::time::Instant> = None;
            loop {
                let mut wake = false;

                for ev in GlobalHotKeyEvent::receiver().try_iter() {
                    if ev.state == HotKeyState::Pressed {
                        toggle_window(&ctx2);
                        wake = true;
                    }
                }
                for ev in TrayIconEvent::receiver().try_iter() {
                    if let TrayIconEvent::Click {
                        button: tray_icon::MouseButton::Left,
                        button_state: tray_icon::MouseButtonState::Up,
                        ..
                    } = ev
                    {
                        let now = std::time::Instant::now();
                        let fresh = last_tray_click
                            .is_none_or(|t| now.duration_since(t) > Duration::from_millis(350));
                        if fresh {
                            last_tray_click = Some(now);
                            toggle_window(&ctx2);
                            wake = true;
                        }
                    }
                }
                for ev in MenuEvent::receiver().try_iter() {
                    if ev.id == toggle_id {
                        toggle_window(&ctx2);
                    } else if ev.id == manage_id {
                        open_manage2.store(true, Ordering::Relaxed);
                        show_window_sized(&ctx2, Some(MANAGER_SIZE));
                    } else if ev.id == new_id {
                        open_new2.store(true, Ordering::Relaxed);
                        show_window_sized(&ctx2, Some(ADD_SIZE));
                    } else if ev.id == settings_id {
                        open_settings2.store(true, Ordering::Relaxed);
                        show_window(&ctx2);
                    } else if ev.id == quit_id {
                        close_window();
                    }
                    wake = true;
                }

                // Another process asked us to add paths (or just to wake up).
                // Every other tick (100 ms) is plenty for a hand-off and keeps
                // an idle launcher from hammering the filesystem.
                tick = tick.wrapping_add(1);
                if tick % 2 == 0 {
                    for request in ipc::drain() {
                        match request {
                            ipc::Request::Add(paths) => {
                                log_line(&format!("handoff: received {} path(s)", paths.len()));
                                if let Ok(mut queue) = pending_adds2.lock() {
                                    queue.extend(paths);
                                }
                                // Show at dialog size *before* the frame is
                                // drawn: the card must never appear at launcher
                                // size first and then jump.
                                show_window_sized(&ctx2, Some(ADD_SIZE));
                            }
                            ipc::Request::Wake => show_window(&ctx2),
                        }
                        wake = true;
                    }
                }

                if wake {
                    ctx2.request_repaint();
                }
                std::thread::sleep(Duration::from_millis(50));
            }
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
            theme: ThemeChoice::parse(
                &store
                    .get_config(THEME_KEY)
                    .unwrap_or_else(|| ThemeChoice::System.as_str().to_string()),
            ),
            card_alpha: std::env::var("MXRUN_CARD_ALPHA")
                .ok()
                .and_then(|v| v.parse().ok())
                // Opaque by default: at 235 the desktop showed through the card
                // and reading the list got tiring (reported 2026-09-19). The env
                // var stays as the knob for the visual work later on.
                .unwrap_or(255),
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
            open_new,
            pending_adds,
            add: None,
            toast: None,
            ask_integration,
            manage: false,
            open_manage,
            pending_delete: None,
            manage_source: None,
            discovery: Arc::new(Mutex::new(DiscoveryState::default())),
            // Auto-discovery is on by default (user's call, 2026-09-17); a
            // stored "0" turns it off. The first scan is deferred to the first
            // frame so it can never sit in front of startup.
            discovery_pending: (store.get_config(DISCOVERY_KEY).as_deref() != Some("0"))
                .then_some(false),
            discovery_note: String::new(),
            recent_only: false,
            calc_mode: false,
            anim: None,
            anim_t: 1.0,
            anim_dy: 0.0,
            logged_drag_rect: false,
            hover_row: None,
            prev_list: ListSnapshot::default(),
            scroll_selected: false,
            cold_start_add: false,
            add_note_shown: false,
            applied_size: LAUNCHER_SIZE,
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

        // Where the card was last left (AltRun's WinTop/WinLeft): the hotkey
        // thread needs it, and it does not own the database, so hand it over
        // through the process-wide slot.
        let pos = stored_position(&app.store);
        set_remembered_position(pos);

        // Debug hook: answer the first-run question without a keyboard
        // (`MXRUN_SELFTEST_INTEGRATION=yes|no`) — pressing the card's button is
        // the only other way, and keys cannot be delivered from a script.
        if app.ask_integration {
            if let Ok(answer) = std::env::var("MXRUN_SELFTEST_INTEGRATION") {
                app.answer_integration(&cc.egui_ctx, answer.eq_ignore_ascii_case("yes"));
                log_line(&format!(
                    "selftest: answered the integration question {answer:?} -> {}",
                    app.status
                ));
            }
        }

        // Debug hook: force a discovery scan at startup regardless of the cache
        // (`MXRUN_SELFTEST_DISCOVER=force`), so the "deleted items stay deleted"
        // rule can be verified without clicking 现在扫一次.
        if let Ok(mode) = std::env::var("MXRUN_SELFTEST_DISCOVER") {
            app.discovery_pending = Some(mode == "force");
        }

        // Debug hook: type a string one character at a time through the same
        // code path a keyboard drives (`MXRUN_SELFTEST_TYPE=b#3`). Windows will
        // not deliver keys to a background window, so this is the only way to
        // verify the digit rules from a script — **it really runs items**, so
        // point it at a scratch profile only. Several specs can be chained with
        // `|` (`al2|lab1|q55`); the box is reset between them.
        if let Ok(spec) = std::env::var("MXRUN_SELFTEST_TYPE") {
            for spec in spec.split('|') {
                app.input.clear();
                app.prev_list = ListSnapshot::default();
                app.refresh_search();
                log_line(&format!("type: --- spec {spec:?} ---"));
                for ch in spec.chars() {
                    app.input.push(ch);
                    app.refresh_search();
                    app.after_query_change(&cc.egui_ctx);
                    log_line(&format!(
                        "type: {:?} -> {} rows | {} | calc={:?}",
                        app.input,
                        app.results.len(),
                        app.status,
                        app.calc_result
                    ));
                }
            }
        }

        // Debug hook: run row N the way a digit key does
        // (`MXRUN_SELFTEST_ROW=3` is what pressing `3` on an empty box does).
        // **It really runs the item**, so only point it at a scratch profile.
        if let Ok(spec) = std::env::var("MXRUN_SELFTEST_ROW")
            && let Ok(row) = spec.trim().parse::<usize>()
        {
            let title = app
                .results
                .get(row.saturating_sub(1))
                .map(|sc| app.items[sc.idx].item.title.clone());
            log_line(&format!(
                "selftest: row {row} -> {:?} ({} rows)",
                title,
                app.results.len()
            ));
            app.run_index(row.saturating_sub(1), &cc.egui_ctx);
            log_line(&format!("selftest: after row {row}: {}", app.status));
        }

        // Debug hook: open the settings view at startup
        // (`MXRUN_SELFTEST_SETTINGS=1`) — the only way to screenshot it without
        // clicking the tray menu.
        if std::env::var("MXRUN_SELFTEST_SETTINGS").is_ok() {
            app.open_settings.store(true, Ordering::Relaxed);
        }

        // Debug hook: switch to the recent list at startup
        // (`MXRUN_SELFTEST_RECENT=1`), the only way to check it from a script.
        if std::env::var("MXRUN_SELFTEST_RECENT").is_ok() {
            app.toggle_recent();
        }

        // Debug hook: place the card at a fixed spot and remember it, which is
        // what a real drag does (`MXRUN_SELFTEST_WINPOS=x,y`). The next start
        // must come back to that spot — the only way to verify the position
        // memory without a mouse.
        if let Ok(spec) = std::env::var("MXRUN_SELFTEST_WINPOS")
            && let Some((x, y)) = spec.split_once(',').and_then(|(x, y)| {
                Some((x.trim().parse::<i32>().ok()?, y.trim().parse::<i32>().ok()?))
            })
        {
            app.store.set_config("win_x", &x.to_string());
            app.store.set_config("win_y", &y.to_string());
            set_remembered_position(Some((x, y)));
            log_line(&format!("window: position saved at {x},{y} (selftest)"));
        }

        // Debug hook: drive the manager and the delete flow without a keyboard
        // (`MXRUN_SELFTEST_MANAGE=1` opens it, `MXRUN_SELFTEST_DELETE=1` then
        // deletes the selected row — twice, the way the confirmation works).
        // **It really deletes**, so only ever point it at a scratch profile.
        if std::env::var("MXRUN_SELFTEST_MANAGE").is_ok() {
            // `MXRUN_SELFTEST_SOURCE=discovered|curated` narrows the manager
            // first, so a scripted delete can target an auto-discovered row.
            match std::env::var("MXRUN_SELFTEST_SOURCE").as_deref() {
                Ok("discovered") => app.manage_source = Some(SourceFilter::Discovered),
                Ok("curated") => app.manage_source = Some(SourceFilter::Curated),
                _ => {}
            }
            app.enter_manage();
            log_line(&format!(
                "selftest: manage rows={} items={}",
                app.results.len(),
                app.items.len()
            ));
            if std::env::var("MXRUN_SELFTEST_DELETE").is_ok() && !app.results.is_empty() {
                let victim = app.results[app.selected].idx;
                let (id, title) = (
                    app.items[victim].item.id.clone(),
                    app.items[victim].item.title.clone(),
                );
                log_line(&format!("selftest: deleting {title:?} (id={id})"));
                let ctx = cc.egui_ctx.clone();
                app.request_delete(&ctx);
                log_line(&format!("selftest: armed -> {}", app.status));
                app.request_delete(&ctx);
                log_line(&format!("selftest: after delete -> {}", app.status));
                log_line(&format!(
                    "selftest: items now={} still_present={} tombstone={}",
                    app.items.len(),
                    app.items.iter().any(|i| i.item.id == id),
                    app.store
                        .deleted_sources()
                        .iter()
                        .any(|(p, ext, _)| *ext == id || format!("{p}:{ext}") == id)
                ));
            }
        }

        // Started with paths (right-click → 发送到, shell menu, or a drop on the
        // exe): open the card before the first frame, so the window is *born*
        // as the small card.
        if let Some(paths) = PENDING_PATHS.get() {
            app.cold_start_add = true;
            app.open_add(paths.clone());
            app.selftest_confirm_add(&cc.egui_ctx);
        }

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
                // `MXRUN_SELFTEST_EDIT=1` drives the F2 path: open the card on
                // the first row, change the command line, save. **It really
                // edits the database**, so only ever point it at a scratch
                // profile. Verifies the part the add path does not cover: the
                // id survives (and with it the launch count).
                if std::env::var("MXRUN_SELFTEST_EDIT").is_ok() {
                    if let Some(sc) = app.results.first() {
                        let before = app.items[sc.idx].item.clone();
                        log_line(&format!(
                            "selftest: editing {:?} (id={}, launches={})",
                            before.title, before.id, app.items[sc.idx].launches
                        ));
                        app.open_edit();
                        if let Some(form) = app.add.as_mut() {
                            form.command = format!("{} --mxrun-selftest", form.command);
                            log_line(&format!("selftest: card command={:?}", form.command));
                        } else {
                            log_line("selftest: card did not open (not an editable effect)");
                        }
                        for attempt in 1..=2 {
                            app.confirm_add(&ctx);
                            match app.add.as_ref() {
                                Some(form) => {
                                    log_line(&format!("selftest: card still open: {}", form.error))
                                }
                                None => break,
                            }
                            let _ = attempt;
                        }
                        let after = app
                            .items
                            .iter()
                            .find(|i| i.item.id == before.id)
                            .map(|i| i.item.clone());
                        match after {
                            Some(item) => log_line(&format!(
                                "selftest: after edit id={} title={:?} command={:?}",
                                item.id,
                                item.title,
                                action_command(item.default_action().unwrap_or(&item.actions[0]))
                            )),
                            None => log_line("selftest: edited item vanished"),
                        }
                        log_line(&format!("selftest: edit status={:?}", app.status));
                    }
                }
            }
        }
        Ok(app)
    }

    fn reset_status(&mut self) {
        self.status = format!(
            "{} 呼出 / 隐藏 · Enter/空格 执行 · Ctrl+数字 执行第 N 项 · F2 编辑 · 设置见托盘菜单",
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
        self.items = index_items(items, &mut self.store);

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

    /// What a keystroke in the search box means beyond "search".
    ///
    /// Called after `refresh_search`, so `self.results` is the list as the user
    /// sees it right now. Order matters: the explicit `#3` wins over the
    /// implicit trailing-digit rule, so `b#3` can never be read as "row 3 of
    /// whatever `b#` matches".
    fn after_query_change(&mut self, ctx: &egui::Context) {
        // 1) Explicit selector: `#3`, `b#3`.
        if self.input.contains('#') {
            let row = split_selector(self.input.trim()).1;
            if let Some(row) = row {
                self.run_selector(row, ctx);
                return;
            }
            // Mid-typing (`b#`): the list is already the right one, just say so.
            self.status = format!(
                "输入序号执行对应项（1..{}，0 = 第 10 项）· 再按一次退格可返回搜索",
                self.results.len()
            );
            self.snapshot_list();
            return;
        }

        // 2) AltRun's rule, adapted: a digit that did not help the search is a
        //    command.
        let prev = self.prev_list.clone();
        let same_as_prev = !prev.ids.is_empty()
            && prev.ids.len() == self.results.len()
            && prev.ids.iter().zip(&self.results).all(|(id, sc)| {
                self.items.get(sc.idx).is_some_and(|it| &it.item.id == id)
            });
        let verdict = digit_verdict(
            &self.input,
            &prev.input,
            prev.ids.len(),
            self.results.len(),
            same_as_prev,
        );
        match verdict {
            DigitVerdict::Run(row) => {
                let id = prev.ids.get(row).cloned();
                let title = id
                    .as_ref()
                    .and_then(|id| self.items.iter().find(|i| &i.item.id == id))
                    .map(|i| i.item.title.clone());
                log_line(&format!(
                    "digit: {:?} -> run row {} of {:?} ({} rows) [{:?}]",
                    self.input,
                    row + 1,
                    prev.input,
                    prev.ids.len(),
                    title
                ));
                if let Some(id) = id {
                    self.run_item_id(&id, ctx);
                }
                self.prev_list = ListSnapshot::default();
                return;
            }
            DigitVerdict::NotADigit | DigitVerdict::NotAnAppend => {}
            other => {
                // Only worth a line when a digit was actually involved.
                if self.input.chars().last().is_some_and(|c| c.is_ascii_digit()) {
                    log_line(&format!(
                        "digit: {:?} -> keep input ({}; prev={:?} {} rows, now {} rows)",
                        self.input,
                        other.reason(),
                        prev.input,
                        prev.ids.len(),
                        self.results.len()
                    ));
                }
            }
        }
        self.snapshot_list();
    }

    /// Remember the list on screen, for the next keystroke's trailing digit.
    fn snapshot_list(&mut self) {
        let ids = self
            .results
            .iter()
            .filter_map(|sc| self.items.get(sc.idx).map(|it| it.item.id.clone()))
            .collect();
        self.prev_list = ListSnapshot {
            input: self.input.clone(),
            ids,
        };
    }

    /// `#N`: run the Nth row of what the list shows for the query before `#`.
    ///
    /// The current `self.results` are already that list — `refresh_search` uses
    /// the text before `#` as the query.
    fn run_selector(&mut self, row: usize, ctx: &egui::Context) {
        if row >= self.results.len() {
            self.status = format!("没有第 {} 项（当前 {} 项）", row + 1, self.results.len());
            log_line(&format!(
                "selector: {:?} -> no row {}",
                self.input,
                row + 1
            ));
            return;
        }
        let id = self.items[self.results[row].idx].item.id.clone();
        log_line(&format!(
            "selector: {:?} -> run row {} ({} rows shown)",
            self.input,
            row + 1,
            self.results.len()
        ));
        self.input.clear();
        self.run_item_id(&id, ctx);
        self.prev_list = ListSnapshot::default();
    }

    /// Run an item by id — used by the digit rules, which point at a row of a
    /// list that may have been rebuilt (or emptied) since.
    fn run_item_id(&mut self, id: &str, ctx: &egui::Context) {
        if let Some(idx) = self.items.iter().position(|it| it.item.id == id) {
            self.execute_index(idx, ctx);
        }
    }

    /// Advance the summon animation one frame.
    ///
    /// Only ever runs for ~100ms after a summon, and asks for the frames it
    /// needs itself — the launcher must stay "no work while idle" (the wake
    /// latency budget is measured from the hotkey to the first frame, and this
    /// runs *after* that frame has already been painted).
    fn step_anim(&mut self, ctx: &egui::Context) {
        let Some(t0) = self.anim else {
            return;
        };
        let ms = anim_ms();
        let raw = (t0.elapsed().as_secs_f32() * 1000.0 / ms.max(1.0)).clamp(0.0, 1.0);
        // Ease-out cubic: quick off the mark, settles gently.
        let eased = 1.0 - (1.0 - raw).powi(3);
        self.anim_t = eased;
        self.anim_dy = (1.0 - eased) * ANIM_RISE;
        if raw >= 1.0 {
            self.anim = None;
            self.anim_dy = 0.0;
        } else {
            ctx.request_repaint();
        }
        // The rise: move the window while it fades, then land exactly on the
        // spot `show_window_sized` chose.
        if let (Some(hwnd), true) = (main_hwnd(), self.anim_dy > 0.3) {
            let size = applied_size_of(hwnd);
            let (x, y) = place_for(size);
            let target = y + self.anim_dy.round() as i32;
            if std::env::var_os("MXRUN_SELFTEST_RECTS").is_some() {
                log_line(&format!(
                    "anim: t={:.2} dy={:.1} -> y {target} (settles at {y})",
                    self.anim_t, self.anim_dy
                ));
            }
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    x,
                    target,
                    size[0] as i32,
                    size[1] as i32,
                    SWP_NOACTIVATE,
                );
            }
        }
    }

    /// A quiet chip naming the current input mode, so a vanished list (the
    /// calculator) or a filtered one (the recent list) never looks like a bug.
    fn mode_chip(&self) -> Option<(&'static str, Color32)> {
        if self.calc_mode {
            return Some(("计算器", pal().ok));
        }
        if self.recent_only {
            return Some(("最近列表", pal().accent));
        }
        if self.input.contains('#') {
            return Some(("按序号选行", pal().accent));
        }
        None
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
            indexed.last_used = frec.last_used;
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
        // `=1+2`: the box is a calculator, so there is no list at all — and no
        // search, no digit rules, nothing that could hijack the expression.
        self.calc_mode = is_calc_mode(&self.input);
        if self.calc_mode {
            let expr = calc_expression(&self.input);
            self.calc_result = eval_calc(expr);
            self.status = if self.calc_result.is_some() {
                "计算器：回车不做任何事，Esc 清空".to_string()
            } else {
                "输入算式，例如 =1+2（支持 + - * / % ^ 与括号）".to_string()
            };
            return;
        }
        // `b#3`: everything from `#` on is a row selector, not search text, so
        // the list keeps showing what you are choosing from while you type it.
        let query = split_selector(self.input.trim()).0;
        let manage = self.manage;
        let filter = self.manage_source;
        let recent_only = self.recent_only;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if recent_only {
            // `Ctrl+L`: AltRun's 最近列表 — what has actually been used lately,
            // most recent first. Nothing used in the window means an empty list,
            // which is honest (and the status line says what the list is).
            for (idx, it) in self.items.iter().enumerate() {
                if is_recent(it.last_used, now) {
                    self.results.push(Scored {
                        idx,
                        score: it.last_used as f64,
                        // Not a search hit at all: this list is "what you used
                        // lately", so every row is as strong as every other.
                        strong: true,
                        hit_indices: Vec::new(),
                    });
                }
            }
        } else if query.is_empty() && manage {
            // Manager, no filter: every item there is, most used first — the
            // ones that were never launched pile up at the bottom, which is
            // exactly what you go looking for when tidying up.
            for (idx, it) in self.items.iter().enumerate() {
                if filter.is_some_and(|f| !f.matches(&it.item.source.provider)) {
                    continue;
                }
                self.results.push(Scored {
                    idx,
                    score: it.launches as f64,
                    strong: true,
                    hit_indices: Vec::new(),
                });
            }
        } else if query.is_empty() {
            // Idle: show everything ranked by frecency.
            for (idx, it) in self.items.iter().enumerate() {
                self.results.push(Scored {
                    idx,
                    score: it.frec_bonus,
                    strong: true,
                    hit_indices: Vec::new(),
                });
            }
        } else {
            let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
            let mut buf: Vec<char> = Vec::new();
            let mut idx_buf: Vec<u32> = Vec::new();
            for (idx, it) in self.items.iter().enumerate() {
                if filter.is_some_and(|f| !f.matches(&it.item.source.provider)) {
                    continue;
                }
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
            // (The calculator used to live here, evaluating any query that
            // looked like an expression. It moved behind the `=` prefix — see
            // `is_calc_mode` — because the bare-digit rules ate the leading
            // number of `1+2` from an empty box.)
        }
        // The sorting below has to know what "most recent" means for Ctrl+L.
        if recent_only {
            self.results.sort_by(|a, b| {
                self.items[b.idx]
                    .last_used
                    .cmp(&self.items[a.idx].last_used)
                    .then_with(|| self.items[a.idx].item.title.cmp(&self.items[b.idx].item.title))
            });
        } else if manage && query.is_empty() {
            // Same ordering as the score above, with the title as the tiebreak
            // so the list does not shuffle between frames.
            self.results.sort_by(|a, b| {
                let (la, lb) = (self.items[a.idx].launches, self.items[b.idx].launches);
                lb.cmp(&la)
                    .then_with(|| self.items[a.idx].item.title.cmp(&self.items[b.idx].item.title))
            });
        } else {
            self.results.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
        // The manager is the place where seeing *everything* is the point: no
        // score gate and no eight-row cap, it scrolls instead.
        if !query.is_empty() && !manage && !recent_only {
            // Short queries mean "something *starting* with this": a one-letter
            // query fuzzy-matches most of a 200-item list (`e` hit 125 of them),
            // and the padding below the real matches is the noise a user
            // notices. Longer queries keep the fuzzy reach.
            if query.chars().count() < MIN_FUZZY_CHARS {
                self.results.retain(|sc| sc.strong);
            }
            apply_score_gate(&mut self.results);
        }
        if !manage {
            self.results.truncate(MAX_ROWS);
        }
        self.selected = self.selected.min(self.results.len().saturating_sub(1));
    }

    fn execute_selected(&mut self, ctx: &egui::Context) {
        let Some(sc) = self.results.get(self.selected) else {
            return;
        };
        self.execute_index(sc.idx, ctx);
    }

    /// Run the item at `self.items[idx]`.
    ///
    /// Split out from `execute_selected` because the digit rules point at a row
    /// of the list from one keystroke ago — a list that the current search may
    /// not contain any more (the digit usually kills it).
    fn execute_index(&mut self, idx: usize, ctx: &egui::Context) {
        let Some(indexed) = self.items.get(idx) else {
            return;
        };
        // Debug hook: decide everything, launch nothing
        // (`MXRUN_SELFTEST_DRYRUN=1`). Lets the digit rules be measured against
        // a real list without starting the programs in it.
        if std::env::var_os("MXRUN_SELFTEST_DRYRUN").is_some() {
            log_line(&format!("dryrun: would execute {:?}", indexed.item.title));
            self.input.clear();
            return;
        }
        let item = indexed.item.clone();
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

    // ---------- add / edit card ----------

    /// Open the card for a batch of paths (right-click, 发送到, `MxRun.exe "…"`).
    fn open_add(&mut self, mut paths: Vec<String>) {
        if paths.is_empty() {
            return;
        }
        paths.retain(|p| !p.trim().is_empty());
        if paths.is_empty() {
            return;
        }
        let first = paths.remove(0);
        log_line(&format!("add: open for {:?} (queue={})", first, paths.len()));
        self.add = Some(AddForm::from_path(&first, paths));
        self.want_focus = true;
    }

    /// Open the card on the selected item (F2).
    fn open_edit(&mut self) {
        let Some(sc) = self.results.get(self.selected) else {
            return;
        };
        let item = self.items[sc.idx].item.clone();
        match AddForm::from_item(item) {
            Some(form) => {
                log_line(&format!("edit: open for {:?}", form.title));
                self.add = Some(form);
            }
            // Builtin verbs, clipboard copies and reveals have no command line
            // to edit — say so instead of showing an empty box.
            None => {
                self.status = format!("「{}」是内建动作，暂时不能在界面里编辑", self.items[sc.idx].item.title);
            }
        }
    }

    /// Take over any paths another process handed us.
    fn receive_pending_adds(&mut self, ctx: &egui::Context) {
        let incoming: Vec<String> = match self.pending_adds.lock() {
            Ok(mut queue) => std::mem::take(&mut *queue),
            Err(_) => return,
        };
        if incoming.is_empty() {
            return;
        }
        let opened = self.add.is_none();
        match self.add.as_mut() {
            // Already showing a card: queue behind it rather than losing them.
            Some(form) => form.queue.extend(incoming),
            None => self.open_add(incoming),
        }
        // Debug hook (`MXRUN_SELFTEST_ADD=1`): confirm the card without a
        // keyboard. This is the only way to verify the whole right-click path
        // (hand-off → card → database) from a script — keys cannot be delivered
        // to the window (CLAUDE.md pitfall 3).
        if opened {
            self.selftest_confirm_add(ctx);
        }
    }

    /// Walk the card through "press Enter" without a keyboard
    /// (`MXRUN_SELFTEST_ADD=1`). Twice, because that is what a human does: the
    /// first Enter on a colliding keyword only warns, the second replaces.
    fn selftest_confirm_add(&mut self, ctx: &egui::Context) {
        if std::env::var("MXRUN_SELFTEST_ADD").is_err() || self.add.is_none() {
            return;
        }
        for attempt in 1..=2 {
            log_line(&format!("selftest: confirming the add card (attempt {attempt})"));
            self.confirm_add(ctx);
            match self.add.as_ref() {
                Some(form) => log_line(&format!("selftest: card still open: {}", form.error)),
                None => break,
            }
        }
        log_line(&format!("selftest: after add status={:?}", self.status));
    }

    /// Enter on the card: create the item, or save the edit.
    fn confirm_add(&mut self, ctx: &egui::Context) {
        // Snapshot the card first: deciding whether the keyword collides needs
        // `&self`, and holding the `&mut self.add` borrow across that call is
        // exactly the kind of aliasing Rust refuses (correctly).
        let Some(form) = self.add.as_ref() else {
            return;
        };
        let keyword = form.keyword.trim().to_string();
        let mut title = form.title.trim().to_string();
        let command = form.command.trim().to_string();
        let editing = form.editing.clone();
        let source_path = form.source_path.clone();
        let overwrite_asked = form.overwrite_asked;

        // AltRun refused an empty keyword or command line by silently turning
        // the row into a blank separator; here it is simply refused
        // (docs/AltRun交互规格.md §11, "两个坑别照抄" ①).
        if keyword.is_empty() || command.is_empty() {
            self.set_add_error("关键字和命令行都不能为空");
            return;
        }
        if title.is_empty() {
            title = keyword.clone();
        }

        // Colliding keyword: warn once, replace on the second Enter. AltRun
        // asked with a modal; this is the same question without the modal.
        let editing_id = editing.as_ref().map(|i| i.id.clone());
        let clash = self.find_by_keyword(&keyword, editing_id.as_deref());
        if let Some(other) = clash.clone() {
            if !overwrite_asked {
                self.set_add_error(&format!(
                    "关键字「{keyword}」已被「{}」使用 —— 再按一次回车将覆盖它",
                    other.title
                ));
                if let Some(form) = self.add.as_mut() {
                    form.overwrite_asked = true;
                }
                return;
            }
        }

        // What gets written: the edited item, the collided item, or a new one.
        let Some(mut item) = editing
            .clone()
            .or_else(|| clash.clone())
            .or_else(|| Some(integrate::item_from_path(&source_path)))
        else {
            return;
        };
        let verb = if editing.is_some() {
            "updated"
        } else if clash.is_some() {
            "replaced"
        } else {
            "created"
        };

        item.title = title;
        if item.keywords.is_empty() {
            item.keywords.push(keyword.clone());
        } else {
            // Only the first trigger word is on the card; the rest (imported
            // rows can carry several) are left alone.
            item.keywords[0] = keyword.clone();
        }
        if let Some(action) = item.actions.first_mut() {
            match &mut action.effect {
                Effect::Open { target } => *target = command.clone(),
                Effect::Run { line } => *line = command.clone(),
                _ => {
                    // Nothing to edit in a builtin verb: store the text as a
                    // plain "open" action instead.
                    action.effect = Effect::Open { target: command.clone() };
                }
            }
        } else {
            item.actions = vec![Action::open(&command)];
        }

        // `upsert_from_source` keeps the id (and with it the launch count) when
        // the same source comes back; an edited row keeps its own id outright.
        //
        // Adding something on purpose also lifts an earlier tombstone: "delete"
        // means "do not bring this back by yourself", not "never again".
        self.store
            .clear_deleted(&item.source.provider, &item.source.external_id);
        let saved = if item.source.external_id.is_empty() {
            self.store.upsert_item(&item).map(|()| Some(item.id.clone()))
        } else {
            self.store.upsert_from_source(item)
        };
        match saved {
            Ok(Some(_)) => {
                log_line(&format!(
                    "add: {verb} {keyword:?} -> {}",
                    truncate_chars(&command, 120)
                ));
                self.status = match verb {
                    "updated" => format!("已保存：{keyword}"),
                    "replaced" => format!("已覆盖：{keyword}"),
                    _ => format!("已添加：{keyword}"),
                };
                self.finish_add(ctx);
            }
            // Cannot happen: the tombstone was cleared just above. Say so
            // rather than closing the card as if it had saved something.
            Ok(None) => {
                log_line("add: refused by a tombstone that should have been cleared");
                self.set_add_error("这条之前被删过，仍处于删除状态");
            }
            Err(e) => {
                log_line(&format!("add: failed: {e}"));
                self.set_add_error(&format!("保存失败：{e}"));
            }
        }
    }

    fn set_add_error(&mut self, message: &str) {
        if let Some(form) = self.add.as_mut() {
            form.error = message.to_string();
        }
    }

    // ---------- delete / insert / reveal ----------

    /// `Delete` on the selected row.
    ///
    /// AltRun asked with a modal confirmation (`frmALTRun.pas:2233-2241`); the
    /// question is the same here, the modal is not: the first press arms it, the
    /// second one deletes, and any other key disarms. Same shape as the "keyword
    /// already in use" warning on the add card.
    fn request_delete(&mut self, ctx: &egui::Context) {
        let Some(sc) = self.results.get(self.selected) else {
            return;
        };
        let item = self.items[sc.idx].item.clone();

        if self.pending_delete.as_deref() != Some(item.id.as_str()) {
            self.pending_delete = Some(item.id.clone());
            self.status = format!("再按一次 Delete 删除「{}」（其它按键取消）", item.title);
            log_line(&format!("delete: confirm? {:?}", item.title));
            return;
        }

        self.pending_delete = None;
        match self.store.delete_item(&item) {
            Ok(()) => {
                log_line(&format!(
                    "delete: removed {:?} (id={}, provider={})",
                    item.title, item.id, item.source.provider
                ));
                self.status = format!("已删除：{}", item.title);
                self.rebuild_index(ctx);
                self.refresh_search();
            }
            Err(e) => {
                log_line(&format!("delete: failed: {e}"));
                self.status = format!("删除失败：{e}");
            }
        }
    }

    /// `Insert`: a blank card, the same one the right-click path uses.
    fn request_new(&mut self) {
        log_line("add: open blank card (Insert)");
        self.add = Some(AddForm::blank(""));
        self.want_focus = true;
    }

    /// Remember where the card sits now (called after the user drags it).
    fn save_position(&mut self) {
        let Some((x, y)) = current_position() else {
            return;
        };
        self.store.set_config("win_x", &x.to_string());
        self.store.set_config("win_y", &y.to_string());
        set_remembered_position(Some((x, y)));
        log_line(&format!("window: position saved at {x},{y}"));
    }

    /// `Ctrl/Alt+N`, `;`, `'`: run the Nth row directly (AltRun's fastest way
    /// in once you know your list).
    fn run_index(&mut self, row: usize, ctx: &egui::Context) {
        if row >= self.results.len() {
            // Out of range is silent: the number of rows changes as you type,
            // and a scolding status line would be noise.
            return;
        }
        self.selected = row;
        self.scroll_selected = true;
        self.execute_selected(ctx);
    }

    /// `Ctrl+C`: put the selected item's command line on the clipboard.
    fn copy_command(&mut self) {
        let Some(sc) = self.results.get(self.selected) else {
            return;
        };
        let item = self.items[sc.idx].item.clone();
        let Some(command) = item.default_action().and_then(action_command) else {
            self.status = format!("「{}」是内建动作，没有命令行可复制", item.title);
            return;
        };
        let action = Action::copy(command);
        match exec::run(&item, &action) {
            exec::Outcome::Started => {
                log_line(&format!("copy: command line of {:?}", item.title));
                self.status = format!("已复制命令行：{}", truncate_chars(command, 60));
            }
            exec::Outcome::NeedsInput => {}
            exec::Outcome::Failed(why) => {
                self.status = format!("复制失败：{why}");
            }
        }
    }

    /// `Ctrl+L`: show only what has been used recently (AltRun's 最近列表),
    /// most recent first. Pressing it again goes back to the normal list.
    fn toggle_recent(&mut self) {
        self.recent_only = !self.recent_only;
        self.selected = 0;
        self.refresh_search();
        if self.recent_only {
            self.status = "最近列表：只看最近 7 天用过的（再按 Ctrl+L 返回）".to_string();
        } else {
            self.reset_status();
        }
        log_line(&format!(
            "list: recent_only={} rows={}",
            self.recent_only,
            self.results.len()
        ));
    }

    /// `Ctrl+D`: open the folder the selected item lives in, with the file
    /// selected — AltRun's "打开所在目录".
    fn reveal_selected(&mut self) {
        let Some(sc) = self.results.get(self.selected) else {
            return;
        };
        let item = self.items[sc.idx].item.clone();
        let Some(target) = item.icon_target() else {
            self.status = format!("「{}」没有可定位的文件", item.title);
            return;
        };
        let Some(path) = icon_source(&target) else {
            self.status = format!("「{}」没有可定位的文件", item.title);
            return;
        };
        if !path.exists() {
            self.status = format!("找不到：{}", path.display());
            return;
        }
        let action = Action::reveal(path.to_string_lossy().into_owned());
        match exec::run(&item, &action) {
            exec::Outcome::Started => {
                log_line(&format!("reveal: {} -> {}", item.title, path.display()));
                self.status = format!("已在资源管理器中定位：{}", path.display());
            }
            exec::Outcome::NeedsInput => {}
            exec::Outcome::Failed(why) => {
                log_line(&format!("reveal: failed: {why}"));
                self.status = format!("定位失败：{why}");
            }
        }
    }

    /// Leave the card: next queued path, or close according to where it came
    /// from.
    fn finish_add(&mut self, ctx: &egui::Context) {
        let origin = self.add.as_ref().map(|f| f.origin);
        // Multi-file right-click: the rest of the batch follows the same card.
        if let Some(form) = self.add.as_mut() {
            if !form.queue.is_empty() {
                let next = form.queue.remove(0);
                let rest = std::mem::take(&mut form.queue);
                self.add = Some(AddForm::from_path(&next, rest));
                self.want_focus = true;
                return;
            }
        }
        self.add = None;
        // The list changed: rebuild so the new item is searchable right away
        // (AltRun reloaded its list for the same reason).
        self.rebuild_index(ctx);
        self.refresh_search();
        match origin {
            Some(AddOrigin::External) => {
                // Nothing usually stays on screen: the user was in Explorer,
                // not here. The exception is the one case where something *does*
                // stay behind — this process, which the add request itself
                // started and which now sits in the tray with the hotkey
                // registered. Say so, once, then get out of the way.
                if self.cold_start_add && !self.add_note_shown {
                    self.add_note_shown = true;
                    log_line("add: noting that the launcher stays resident");
                    self.toast = Some(Toast {
                        until: std::time::Instant::now() + Duration::from_millis(2600),
                        title: self.status.clone(),
                        body: format!(
                            "MxRun 已在托盘运行（{} 呼出 · 右键托盘图标可退出）",
                            self.hotkey_str
                        ),
                    });
                    self.want_focus = false;
                    self.apply_window_size(ctx);
                } else {
                    log_line("add: done, hiding");
                    self.apply_window_size(ctx);
                    hide_window(ctx);
                }
            }
            Some(AddOrigin::Launcher) | None => {
                self.apply_window_size(ctx);
                self.want_focus = true;
            }
        }
    }

    fn cancel_add(&mut self, ctx: &egui::Context) {
        let origin = self.add.as_ref().map(|f| f.origin);
        let queued = self.add.as_ref().map(|f| f.queue.len()).unwrap_or(0);
        log_line(&format!("add: cancelled (queued={queued})"));
        self.add = None;
        match origin {
            Some(AddOrigin::External) => {
                self.apply_window_size(ctx);
                hide_window(ctx);
            }
            _ => {
                self.apply_window_size(ctx);
                self.want_focus = true;
                self.reset_status();
            }
        }
    }

    /// The first item (other than `except_id`) that already answers to
    /// `keyword`.
    fn find_by_keyword(&self, keyword: &str, except_id: Option<&str>) -> Option<Item> {
        let kw = keyword.to_lowercase();
        self.items
            .iter()
            .find(|i| {
                i.keywords_lower.iter().any(|k| *k == kw)
                    && Some(i.item.id.as_str()) != except_id
            })
            .map(|i| i.item.clone())
    }

    /// Resize the window when the mode changes (launcher vs the small card).
    /// The hand-off path sizes the window itself before showing it, so this is
    /// usually a no-op — but it is what puts the launcher back to full size
    /// after a right-click add.
    fn apply_window_size(&mut self, ctx: &egui::Context) {
        let want = if self.add.is_some() || self.toast.is_some() || self.ask_integration {
            ADD_SIZE
        } else if self.manage {
            MANAGER_SIZE
        } else if self.view_settings {
            SETTINGS_SIZE
        } else {
            LAUNCHER_SIZE
        };
        if want == self.applied_size {
            return;
        }
        self.applied_size = want;
        let Some(hwnd) = main_hwnd() else {
            return;
        };
        // A mode change resizes the card but must not move it back to the
        // middle: once the user has dragged it somewhere, that is where it
        // lives (clamped, in case the size change would hang it off-screen).
        let (x, y) = place_for(want);
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                x,
                y,
                want[0] as i32,
                want[1] as i32,
                SWP_NOACTIVATE,
            );
        }
        ctx.request_repaint();
    }

    /// Keys while the add / edit card is open.
    ///
    /// Returns true when the card is done with this frame. Space is not
    /// consumed (it is text here), and Tab/Shift+Tab are left to egui, which
    /// already moves focus between the fields.
    fn handle_add_keys(&mut self, ctx: &egui::Context) -> bool {
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape)) {
            self.cancel_add(ctx);
            return true;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Enter)) {
            self.confirm_add(ctx);
            return true;
        }
        false
    }

    /// Enter the manager (tray menu → 快捷项管理, AltRun's `frmShortCutMan`).
    ///
    /// Same window, a different list: everything instead of the top eight, no
    /// score gate, a scrollbar instead of a cap. The filter box is the familiar
    /// search box, so narrowing 59 rows works the way everything else does.
    fn enter_manage(&mut self) {
        if self.manage {
            return;
        }
        log_line(&format!("manage: open ({} items)", self.items.len()));
        self.manage = true;
        self.input.clear();
        self.pending_delete = None;
        self.selected = 0;
        self.refresh_search();
        self.want_focus = true;
        self.status = format!(
            "快捷项管理 · 共 {} 条 · 回车/F2 编辑 · Delete 删除 · Insert 新建 · Ctrl+D 定位 · Esc 返回",
            self.items.len()
        );
    }

    fn leave_manage(&mut self) {
        log_line("manage: close");
        self.manage = false;
        self.input.clear();
        self.pending_delete = None;
        self.selected = 0;
        self.refresh_search();
        self.reset_status();
        self.want_focus = true;
    }

    /// Keys while the manager is open. AltRun's manager keyboard, kept as it
    /// was: F2 编辑 / Insert 添加 / Delete 删除 / 双击编辑 — with Enter doing
    /// what double-click does, since there is no "run" here.
    fn handle_manage_keys(&mut self, ctx: &egui::Context) -> bool {
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape)) {
            self.leave_manage();
            return true;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Delete)) {
            self.request_delete(ctx);
            return false;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Insert)) {
            self.request_new();
            return true;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::F2))
            || ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Enter))
        {
            self.open_edit();
            return true;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, Key::D)) {
            self.reveal_selected();
            return false;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::ArrowDown)) {
            if !self.results.is_empty() {
                self.selected = (self.selected + 1).min(self.results.len() - 1);
            }
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::ArrowUp)) {
            self.selected = self.selected.saturating_sub(1);
        }
        false
    }

    /// Start the auto-discovery scan on a background thread, if one is due.
    ///
    /// Deliberately called from the first `ui()` frame: the scan costs ~300 ms
    /// of COM work on the first run, and the cold-start budget is 255 ms — it
    /// must never be in front of the window. The thread owns its own `Store`
    /// handle (redb allows one writer at a time, so it only writes at the very
    /// end, in a single transaction).
    fn spawn_discovery(
        &mut self,
        force: bool,
        input: discover::ScanInput,
        state: Arc<Mutex<DiscoveryState>>,
    ) {
        {
            let mut guard = match state.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            if guard.running {
                return;
            }
            guard.running = true;
        }
        log_line(&format!("discover: scan started (force={force})"));
        std::thread::spawn(move || {
            // Filesystem + COM only (see `discover::ScanInput`): the database
            // belongs to the UI thread.
            let output = discover::collect(&input, force);
            let mut guard = match state.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            guard.running = false;
            guard.output = Some(output);
        });
    }

    /// Pick up a finished scan: write what it found (this thread owns the
    /// store), rebuild the index and say so in the status line.
    ///
    /// No toast on purpose: a scan can finish at any moment, and stealing the
    /// window while somebody is typing would be worse than a sentence they read
    /// the next time they look.
    fn receive_discovery(&mut self, ctx: &egui::Context) {
        let output = {
            let mut guard = match self.discovery.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            guard.output.take()
        };
        let Some(output) = output else {
            return;
        };
        let report = discover::apply(&mut self.store, output);
        log_line(&format!("discover: {}", report.summary()));
        self.discovery_note = report.human();
        if report.added > 0 {
            self.rebuild_index(ctx);
            self.refresh_search();
            self.status = format!("{} · 可在设置里关闭，或在快捷项管理里清理", self.discovery_note);
        }
    }

    /// Get rid of the note and the window together.
    fn dismiss_toast(&mut self, ctx: &egui::Context) {
        if self.toast.take().is_some() {
            self.apply_window_size(ctx);
            hide_window(ctx);
        }
    }

    /// Answer the first-run question and act on it.
    ///
    /// Either answer marks the machine as asked: nagging on every start would be
    /// worse than never offering, and the settings page can still turn the
    /// entries on later.
    fn answer_integration(&mut self, ctx: &egui::Context, yes: bool) {
        self.ask_integration = false;
        integrate::mark_integration_asked();
        if yes {
            let sendto = integrate::install_sendto();
            let menu = integrate::install_shell_menu();
            self.status = match (&sendto, &menu) {
                (Ok(_), Ok(n)) => format!("已加入右键菜单（{n} 处）和「发送到」"),
                (Err(e), _) | (_, Err(e)) => format!("注册失败：{e}"),
            };
            log_line(&format!("integration: first-run answer=yes, status={:?}", self.status));
        } else {
            self.status = "好的，随时可以在设置里加入右键菜单".to_string();
            log_line("integration: first-run answer=no");
        }
        self.apply_window_size(ctx);
        self.want_focus = true;
    }

    /// Keys while the parameter prompt is open.
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
        let normal = pal().text;
        let hit = pal().accent;

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
        // Scrollable: the page keeps growing, and a settings screen whose own
        // "返回" button is below the window edge is a bug (reported 2026-09-17).
        // The wheel and the bar scroll it; dragging scrolls nothing — on a
        // frameless card, dragging anywhere should move the window (see the drag
        // handle in `eframe::App::ui`), which is what AltRun did too.
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .scroll_source(egui::scroll_area::ScrollSource {
                drag: egui::scroll_area::DragScroll::Never,
                ..Default::default()
            })
            .show(ui, |ui| self.render_settings_body(ui));
    }

    fn render_settings_body(&mut self, ui: &mut egui::Ui) {
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
                    .color(pal().accent),
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
                .color(pal().text_dim),
        );

        ui.add_space(16.0);
        ui.separator();
        ui.add_space(8.0);
        self.render_theme_settings(ui);

        ui.add_space(16.0);
        ui.separator();
        ui.add_space(8.0);
        self.render_discovery_settings(ui);

        ui.add_space(16.0);
        ui.separator();
        ui.add_space(8.0);
        self.render_integration(ui);

        ui.add_space(16.0);
        if ui.button("返回 (Esc)").clicked() {
            self.view_settings = false;
            self.capturing_hotkey = false;
            self.pending_hotkey = None;
            self.reset_status();
        }
    }

    /// Auto-discovery (P0-4) in the settings page.
    /// 外观：主题选择（跟随系统 / 深色 / 浅色）。
    fn render_theme_settings(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("外观").size(15.0));
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let mut picked = self.theme;
            for choice in [ThemeChoice::System, ThemeChoice::Dark, ThemeChoice::Light] {
                if ui
                    .selectable_label(self.theme == choice, RichText::new(choice.label()).size(13.0))
                    .clicked()
                {
                    picked = choice;
                }
            }
            if picked != self.theme {
                self.theme = picked;
                self.store.set_config(THEME_KEY, picked.as_str());
                // Apply right away so the click is visibly instant — including
                // the choice's effect on egui's own widgets.
                apply_theme_choice(self.theme);
                apply_card_style(ui.ctx());
                log_line(&format!("theme: {}", picked.as_str()));
                // The selectable labels keep egui's *old* visuals for the rest
                // of this frame; one more frame paints the new theme.
                ui.ctx().request_repaint();
            }
        });
        ui.add_space(4.0);
        let hint = match self.theme {
            ThemeChoice::System => {
                if SYSTEM_LIGHT.load(Ordering::Relaxed) {
                    "跟随系统（系统当前为浅色）"
                } else {
                    "跟随系统（系统当前为深色）"
                }
            }
            ThemeChoice::Dark => "固定深色",
            ThemeChoice::Light => "固定浅色",
        };
        ui.label(RichText::new(hint).size(12.0).color(pal().text_dim));
    }

    /// 自动发现：开关 + 现在扫一次 + 上次扫描结果。
    fn render_discovery_settings(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("自动发现").size(15.0));
        ui.add_space(6.0);

        let mut on = self.discovery_pending.is_some()
            || self.store.get_config(DISCOVERY_KEY).as_deref() != Some("0");
        let running = self
            .discovery
            .lock()
            .map(|g| g.running)
            .unwrap_or(false);

        if ui
            .checkbox(&mut on, RichText::new("自动扫描开始菜单（新装的软件自动进列表）").size(13.0))
            .changed()
        {
            self.store.set_config(DISCOVERY_KEY, if on { "1" } else { "0" });
            self.discovery_pending = on.then_some(false);
            self.status = if on {
                "已开启自动发现，稍后会扫一次".into()
            } else {
                "已关闭自动发现（已发现的条目仍留在列表里）".to_string()
            };
            log_line(&format!("discover: switched {on}"));
        }

        ui.horizontal(|ui| {
            if ui.button("现在扫一次").clicked() {
                self.discovery_pending = Some(true);
                self.status = "正在扫描开始菜单…".into();
            }
            if running {
                ui.label(RichText::new("扫描中…").size(12.0).color(pal().text_dim));
            }
        });

        let note = if self.discovery_note.is_empty() {
            "还没扫过。扫描在后台进行，不影响呼出和搜索速度。".to_string()
        } else {
            self.discovery_note.clone()
        };
        ui.label(RichText::new(note).size(12.0).color(pal().text_dim));
        ui.label(
            RichText::new(
                "只扫开始菜单里的程序（跳过卸载程序、帮助文档和网页快捷方式），\
                 按真实程序路径去重——你自己手工加过的条目不会被覆盖。\
                 不想要的可以在「快捷项管理」里删掉，删过的不会再被扫回来。",
            )
            .size(12.0)
            .color(pal().text_dim),
        );
    }

    /// The two ways to get things *into* MxRun from Explorer.
    ///
    /// AltRun had only the SendTo shortcut (its shell-menu code was never
    /// called from anywhere — `docs/AltRun交互规格.md` §11). Both are opt-in
    /// buttons rather than something done behind the user's back at startup:
    /// writing to the registry and to the SendTo folder is visible surgery on
    /// their system.
    fn render_integration(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("右键集成").size(15.0));
        ui.add_space(6.0);

        let sendto = integrate::sendto_installed();
        let menu = integrate::shell_menu_installed();

        ui.horizontal(|ui| {
            ui.label(
                RichText::new(if sendto { "✓ 已加入「发送到」菜单" } else { "· 未加入「发送到」菜单" })
                    .size(13.0)
                    .color(if sendto { pal().ok } else { pal().text_dim }),
            );
            if ui.button(if sendto { "移除" } else { "加入" }).clicked() {
                let outcome = if sendto {
                    integrate::uninstall_sendto()
                } else {
                    integrate::install_sendto().map(|_| ())
                };
                self.status = match outcome {
                    Ok(()) => {
                        log_line(&format!("integration: sendto now installed={}", !sendto));
                        // Touching the switch is an answer to the first-run
                        // question, whichever way it went.
                        integrate::mark_integration_asked();
                        if sendto { "已从「发送到」菜单移除".into() } else { "已加入「发送到」菜单".to_string() }
                    }
                    Err(e) => {
                        log_line(&format!("integration: sendto failed: {e}"));
                        format!("操作失败：{e}")
                    }
                };
            }
        });

        ui.horizontal(|ui| {
            ui.label(
                RichText::new(if menu { "✓ 已注册文件和目录右键菜单" } else { "· 未注册右键菜单" })
                    .size(13.0)
                    .color(if menu { pal().ok } else { pal().text_dim }),
            );
            if ui.button(if menu { "移除" } else { "注册" }).clicked() {
                let outcome = if menu {
                    integrate::uninstall_shell_menu().map(|_| 0)
                } else {
                    integrate::install_shell_menu()
                };
                self.status = match outcome {
                    Ok(n) => {
                        log_line(&format!("integration: shell menu now installed={}", !menu));
                        integrate::mark_integration_asked();
                        if menu {
                            "已移除右键菜单".into()
                        } else {
                            format!("已注册 {n} 处右键菜单（文件 / 目录 / 目录空白处）")
                        }
                    }
                    Err(e) => {
                        log_line(&format!("integration: shell menu failed: {e}"));
                        format!("注册失败：{e}")
                    }
                };
            }
        });

        ui.add_space(6.0);
        ui.label(
            RichText::new("两种方式都只会调起一个小确认框，主窗口不会弹出来打扰你。\n\
                           右键菜单写在 HKCU 下，不需要管理员权限。")
                .size(12.0)
                .color(pal().text_dim),
        );
    }
}

/// Card-wide styling: a scroll bar that does not shout.
///
/// The list is keyboard-driven most of the time, so the bar is only a hint that
/// there is more — egui's default is wide and bright enough to pull the eye away
/// from the rows. (eframe 0.36 has no `style` hook on `App`, so this runs once
/// per frame from `ui`; it is a handful of field writes.)
fn apply_card_style(ctx: &egui::Context) {
    // egui's own widgets (real text fields, buttons, checkboxes, scroll bars)
    // need matching visuals — the palette only covers what we draw by hand.
    ctx.set_visuals(if theme_is_light() {
        egui::Visuals::light()
    } else {
        egui::Visuals::dark()
    });
    let p = pal();
    ctx.all_styles_mut(|style| {
        style.spacing.scroll.bar_width = 6.0;
        style.spacing.scroll.floating = true;
        style.visuals.widgets.inactive.bg_fill = p.scroll;
        style.visuals.widgets.hovered.bg_fill = p.scroll_hover;
        style.visuals.widgets.active.bg_fill = p.scroll_hover;
        // Text fields sit on the card, which we paint ourselves: without this
        // egui would put its own (much darker) field background behind the
        // search box and the card's fill would be covered up.
        style.visuals.extreme_bg_color = Color32::TRANSPARENT;
    });
}

impl eframe::App for MxRunApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Rgba::TRANSPARENT.to_array()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // Follow the system theme: re-read it on every summon (cheap registry
        // read, no idle polling — the launcher must stay asleep while hidden).
        apply_theme_choice(self.theme);
        apply_card_style(&ctx);
        self.step_anim(&ctx);

        // Alt+F4 / the system close button hide the launcher instead of exiting
        // it — AltRun's own behaviour (docs/AltRun交互规格.md), and the only
        // sane one for a resident launcher: exiting takes the tray icon and the
        // global hotkey with it, so the user has no way back short of finding
        // the exe. The tray's 退出 sets QUITTING and closes for real.
        if ctx.input(|i| i.viewport().close_requested()) && !QUITTING.load(Ordering::Relaxed) {
            log_line("hide: close requested (Alt+F4) -> hiding, not exiting");
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            hide_window(&ctx);
            return;
        }

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
                apply_round_corners(hwnd);
            }
            // Cold start gets the same fade-in as a summon: the window is not
            // transitioning from hidden here, so the block below would miss it.
            self.anim = (anim_ms() > 0.0).then(std::time::Instant::now);
        }

        // Tray menu asked for the settings view.
        if self.open_settings.swap(false, Ordering::Relaxed) {
            self.view_settings = true;
        }
        // Tray menu asked for a blank add card.
        if self.open_new.swap(false, Ordering::Relaxed) {
            self.add = Some(AddForm::blank(""));
            self.want_focus = true;
        }
        // Tray menu asked for the manager.
        if self.open_manage.swap(false, Ordering::Relaxed) {
            self.enter_manage();
        }

        // Paths handed over by another process (right-click → 发送到, shell menu,
        // or a second launch with a path).
        self.receive_pending_adds(&ctx);

        // A finished auto-discovery scan changed the list.
        self.receive_discovery(&ctx);

        // Auto-discovery starts once the window is actually up (see
        // `spawn_discovery`): never in front of the first frame.
        if self.first_frame_logged
            && let Some(force) = self.discovery_pending.take()
        {
            // The snapshot the worker needs is read here, on the thread that
            // owns the store: one load_items plus one range query.
            let input = discover::read_input(&mut self.store);
            self.spawn_discovery(force, input, self.discovery.clone());
        }

        // The card is a different-sized window; keep it in step with the mode.
        self.apply_window_size(&ctx);

        // The after-add note dismisses itself (and takes the window with it).
        if let Some(toast) = self.toast.as_ref() {
            let left = toast.until.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                log_line("add: note elapsed, hiding");
                self.dismiss_toast(&ctx);
                return;
            }
            // Wake up exactly when it is due, instead of relying on the 250 ms
            // focus poll.
            ctx.request_repaint_after(left);
        }

        // Re-focus the input box whenever the window transitions hidden -> visible
        // (the background thread can't touch app state).
        let visible = window_visible();
        if visible && !self.prev_visible {
            self.want_focus = true;
            self.shown_at = Some(std::time::Instant::now());
            // Start the summon animation (fade + a small rise).
            self.anim = (anim_ms() > 0.0).then(std::time::Instant::now);
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
            // Same for the add / edit card, and it matters more here: a card is
            // usually a right-click the user has already walked away from, and
            // leaving it armed means the next hotkey press greets them with a
            // half-filled form instead of the launcher. The pre-fill is free to
            // recreate — just right-click the file again.
            if self.add.take().is_some() {
                log_line("add: dropped with the window");
                self.apply_window_size(&ctx);
            }
            // The note goes with the window too (it was about the window).
            self.toast = None;
            // An unanswered first-run question is not lost, just postponed: the
            // marker is only written when the user actually answers, so the next
            // start asks again.
            if self.ask_integration {
                log_line("integration: first-run question postponed (window hidden)");
                self.ask_integration = false;
                self.apply_window_size(&ctx);
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
        // then the add card, then the parameter prompt — each owns the keyboard
        // while it is open).
        if self.view_settings {
            if !self.capturing_hotkey
                && (ctx.input(|i| i.key_pressed(Key::Escape))
                    || ctx.input(|i| i.key_pressed(Key::F2)))
            {
                self.view_settings = false;
                self.pending_hotkey = None;
                self.reset_status();
            }
        } else if self.ask_integration {
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Enter)) {
                self.answer_integration(&ctx, true);
                return;
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape)) {
                self.answer_integration(&ctx, false);
                return;
            }
        } else if self.add.is_some() {
            if self.handle_add_keys(&ctx) {
                return;
            }
        } else if self.toast.is_some() {
            // Esc (or Enter) gets rid of the note early; it is only a note.
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape))
                || ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Enter))
            {
                self.dismiss_toast(&ctx);
                return;
            }
        } else if self.manage {
            if self.handle_manage_keys(&ctx) {
                return;
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
                // egui surrenders a text box's focus on Escape, so without this
                // the next keystroke went nowhere (reported 2026-09-19).
                self.want_focus = true;
                ctx.request_repaint();
            }
            // Space runs the selected item (AltRun §1.2). consume_key takes the
            // keystroke away from the text box so a space never reaches the
            // query — AltRun does the same, and it has no multi-word search
            // either. Trade-off: two-word queries are no longer typeable.
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Space)) {
                self.execute_selected(&ctx);
                return;
            }
            // F2 edits the selected item — AltRun's own binding
            // (docs/AltRun交互规格.md §1). The settings page it used to open
            // lives in the tray menu now, which is where AltRun kept its 配置
            // dialog as well.
            if ctx.input(|i| i.key_pressed(Key::F2)) {
                self.open_edit();
            }
            // Insert = 新建快捷项 (AltRun §1), Delete = 删除当前项 with a
            // confirmation, Ctrl+D = 打开所在目录 (our Reveal action).
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Insert)) {
                self.request_new();
                return;
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Delete)) {
                self.request_delete(&ctx);
                return;
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, Key::D)) {
                self.reveal_selected();
                return;
            }
            if ctx.input(|i| i.key_pressed(Key::Enter)) {
                // A `=` expression is not an item to add and has nothing to run.
                if self.calc_mode {
                    return;
                }
                // Nothing matched: offer to add it, the way AltRun did
                // ("无此项 "%s", 添加它?"). This is the launcher's own way in,
                // next to the right-click one.
                if self.results.is_empty() && !self.input.trim().is_empty() {
                    let seed = self.input.trim().to_string();
                    log_line(&format!("add: open for unmatched query {seed:?}"));
                    self.add = Some(AddForm::blank(&seed));
                    self.want_focus = true;
                    return;
                }
                self.execute_selected(&ctx);
                return;
            }
            if ctx.input(|i| i.key_pressed(Key::ArrowDown)) && !self.results.is_empty() {
                self.selected = (self.selected + 1) % self.results.len().max(1);
                self.scroll_selected = true;
            }
            if ctx.input(|i| i.key_pressed(Key::ArrowUp)) && !self.results.is_empty() {
                self.selected =
                    (self.selected + self.results.len() - 1) % self.results.len().max(1);
                self.scroll_selected = true;
            }
            // Tab / Shift+Tab walk the list like ↓ / ↑ (AltRun's binding).
            // Only here in the launcher: inside the cards Tab is how you move
            // between the fields, so it must not be eaten.
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Tab))
                && !self.results.is_empty()
            {
                self.selected = (self.selected + 1) % self.results.len().max(1);
                self.scroll_selected = true;
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::SHIFT, Key::Tab))
                && !self.results.is_empty()
            {
                self.selected =
                    (self.selected + self.results.len() - 1) % self.results.len().max(1);
                self.scroll_selected = true;
            }
            // Ctrl/Alt+digit runs the Nth row; `;` and `'` are AltRun's own
            // shortcuts for the second and third (they sit right next to the
            // numbers on the keyboard it was written for).
            if let Some(row) = consume_digit(&ctx) {
                self.run_index(row, &ctx);
                return;
            }
            // A bare digit does the same **while the box is empty** — summon,
            // see your list, press 3. Typing anything turns digits back into
            // text (`bare_digit_row`).
            if let Some(row) = consume_bare_digit(&ctx, self.input.trim().is_empty()) {
                self.run_index(row, &ctx);
                return;
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Semicolon)) {
                self.run_index(SEMICOLON_ROW, &ctx);
                return;
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Quote)) {
                self.run_index(QUOTE_ROW, &ctx);
                return;
            }
            // Ctrl+C copies the selected item's command line, Ctrl+L switches
            // to the recently used list (both from AltRun's key table).
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, Key::C)) {
                self.copy_command();
                return;
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, Key::L)) {
                self.toggle_recent();
                return;
            }
            // Safety net: whatever the box lost its focus to (a click on a row,
            // a context menu, an Escape egui handled before we could claim it),
            // the next thing you type still goes into the search box. Without
            // this the launcher looked frozen until you clicked the box — the
            // exact complaint that came in on 2026-09-19.
            if ctx.memory(|m| m.focused()).is_none() && !ctx.any_popup_open() {
                let typed: String = ctx.input(|i| {
                    i.events
                        .iter()
                        .filter_map(|e| match e {
                            egui::Event::Text(t) => Some(t.as_str()),
                            _ => None,
                        })
                        .collect()
                });
                if !typed.is_empty() && !typed.chars().all(char::is_control) {
                    log_line(&format!("focus: recovered typed text {typed:?}"));
                    self.input.push_str(&typed);
                    self.selected = 0;
                    self.refresh_search();
                    self.after_query_change(&ctx);
                }
                self.want_focus = true;
            }
        }

        let panel = egui::Frame::new()
            .fill(Color32::from_rgba_unmultiplied(
                pal().card.r(),
                pal().card.g(),
                pal().card.b(),
                (self.card_alpha as f32 * self.anim_t) as u8,
            ))
            .corner_radius(egui::CornerRadius::same(CARD_RADIUS))
            .inner_margin(egui::Margin::same(18))
            // The hairline fades with everything else, or an empty card would
            // still show its outline at the start of the fade.
            .stroke(egui::Stroke::new(
                1.0,
                Color32::from_rgba_premultiplied(
                    (pal().stroke_alpha * self.anim_t) as u8,
                    (pal().stroke_alpha * self.anim_t) as u8,
                    (pal().stroke_alpha * self.anim_t) as u8,
                    (pal().stroke_alpha * self.anim_t) as u8,
                ),
            ));

        egui::CentralPanel::default()
            .frame(panel)
            .show(ui, |ui| {
                // Fade the *contents* too: the frame's own alpha is handled
                // above, and `set_opacity` covers everything drawn inside.
                ui.set_opacity(self.anim_t);
                // Dragging the card moves the window: the OS runs its own move
                // loop (`drag_window`), which is why the position can be saved
                // the moment the call returns. Registered before the views so
                // that text fields and buttons — added later and therefore on
                // top — keep their own drag behaviour (text selection).
                //
                // The rect is the *whole window*, not `ui.max_rect()`: with the
                // card now full of content (search box, a list that fills the
                // card, the status strip), the frame's 18px margin was the only
                // thing left to grab, which read as "there is nowhere to drag"
                // (reported 2026-09-19).
                let bg = ui.interact(
                    ctx.content_rect(),
                    ui.id().with("card-drag"),
                    egui::Sense::drag(),
                );
                // Rect bookkeeping for the "where can I drag?" question: the
                // handle must cover the whole card, not just the content area.
                if std::env::var_os("MXRUN_SELFTEST_RECTS").is_some() && !self.logged_drag_rect {
                    self.logged_drag_rect = true;
                    log_line(&format!(
                        "layout: drag handle {:?}, window {:?}, content clip {:?}",
                        bg.rect,
                        ctx.content_rect(),
                        ui.clip_rect()
                    ));
                }
                if bg.drag_started()
                    && let Some(hwnd) = main_hwnd()
                {
                    drag_window(hwnd);
                    self.save_position();
                }

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
        // A note that is on its way out owns the window for its last moments.
        if self.toast.is_some() {
            self.render_toast(ui);
            return;
        }
        // The first-run question (a machine that has never seen MxRun).
        if self.ask_integration {
            self.render_integration_ask(ui);
            return;
        }
        // The add / edit card takes over the whole window while it is open.
        if self.add.is_some() {
            self.render_add(ui);
            return;
        }
        // The manager: the same window, showing everything instead of results.
        if self.manage {
            self.render_manager(ui);
            return;
        }
        // An open parameter prompt takes over both the top box and the list.
        if self.prompt.is_some() {
            self.render_prompt(ui);
            return;
        }

        // --- Title strip ---
        //
        // The card is frameless, so this is its title bar: the app name, with a
        // small grip right behind it, like a nameplate with a handle. (It was
        // centred first, then flush right — both read as a stray underline,
        // because nothing anchored them; next to the name it looks deliberate.)
        // The strip is one *row*: the name, the grip on the card's centre line,
        // and the mode chip at the right end. (Wrapping it in `horizontal` is
        // load-bearing: a bare `with_layout` here would claim the whole rest of
        // the card and push the search box to the bottom — seen and fixed
        // 2026-09-19.) The grip sits at the card's centre because that is where
        // the user asked for it: flush right looked stray, and next to the name
        // read like punctuation.
        const GRIP_W: f32 = 26.0;
        ui.horizontal(|ui| {
            let title = ui.label(
                RichText::new("MxRun")
                    .size(12.0)
                    .color(pal().text_dim),
            );
            let grip = egui::Rect::from_center_size(
                egui::pos2(ui.max_rect().center().x, title.rect.center().y),
                egui::vec2(GRIP_W, 3.0),
            );
            ui.painter()
                .rect_filled(grip, egui::CornerRadius::same(2), pal().grip);
            // Right side: the mode chip (only while a mode is on).
            if let Some((label, color)) = self.mode_chip() {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    egui::Frame::new()
                        .fill(pal().chip_bg)
                        .corner_radius(egui::CornerRadius::same(4))
                        .inner_margin(egui::Margin::symmetric(6, 1))
                        .show(ui, |ui| {
                            ui.label(RichText::new(label).size(11.0).color(color));
                        });
                });
            }
        });
        ui.add_space(3.0);
        // --- Search box ---
        //
        // `event_filter`: egui's focus navigation reads the raw events at the
        // *start* of a frame, before this widget exists — with the stock filter
        // it takes Tab (moves focus to the next widget) and Escape (surrenders
        // focus) for itself, so the box silently stopped accepting input until
        // you clicked it again (reported 2026-09-19, and it bit after every Esc).
        // Claiming them here keeps both keys ours: Tab walks the list, Escape
        // clears the box.
        let edit = egui::TextEdit::singleline(&mut self.input)
            .font(FontId::proportional(22.0))
            .hint_text("输入以搜索命令、拼音、网址…")
            .desired_width(f32::INFINITY)
            .event_filter(egui::EventFilter {
                horizontal_arrows: true,
                vertical_arrows: true,
                tab: true,
                escape: true,
                ..Default::default()
            })
            .frame(egui::Frame::default());
        let resp = ui.add(edit);
        if self.want_focus {
            resp.request_focus();
            self.want_focus = false;
        }
        if resp.changed() {
            self.refresh_search();
            self.after_query_change(&ctx);
        }

        ui.add_space(6.0);
        ui.separator();
        ui.add_space(6.0);

        // Reserve the status line first, so the list below can never draw over
        // it (see `render_status_bar`).
        let status_rect = self.render_status_bar(ui);

        // --- Calculator result ---
        if self.calc_mode && self.calc_result.is_none() {
            // A `=` mode with nothing to show: say what it wants rather than
            // falling through to "no matches".
            ui.label(
                RichText::new("输入算式，例如 =1+2")
                    .size(15.0)
                    .color(pal().text_dim),
            );
            ui.add_space(4.0);
        }
        if let Some(result) = &self.calc_result {
            ui.label(
                RichText::new(format!("= {result}"))
                    .size(20.0)
                    .color(pal().ok),
            );
            ui.add_space(4.0);
        }
        // --- Result list ---
        //
        // The empty-state message goes *before* the scroll area: with
        // `auto_shrink(false)` the area takes every pixel left, so anything
        // drawn after it would be pushed off the card.
        if self.results.is_empty() && self.calc_result.is_none() && !self.calc_mode {
            ui.label(
                RichText::new("无匹配结果 · 网页搜索等兜底功能将在后续版本加入")
                    .size(13.0)
                    .color(pal().text_dim),
            );
            return;
        }
        //
        // Scrollable: eight rows at the current row height are more than a
        // 400px card holds, so the last one or two used to be cut off with no
        // way to reach them (the list only ever scrolls when the selection is
        // moved, which `scroll_selected` tracks).
        let mut clicked: Option<usize> = None;
        let mut run_now: Option<usize> = None;
        let mut menu_action: Option<(usize, MenuAction)> = None;
        let mut drag_now = false;
        let scroll_selected = std::mem::take(&mut self.scroll_selected);
        // Hover is drawn from the previous frame's state: the background has to
        // be painted before the row's `Response` exists. One frame of lag on a
        // faint tint is invisible.
        let hover_row = self.hover_row;
        let mut hovered_next = None;
        let list_area = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            // Wheel and scroll bar scroll it; dragging scrolls nothing — on a
            // frameless card, dragging anywhere should move the window (see the
            // drag handle in eframe::App::ui), which is what AltRun did too.
            .scroll_source(egui::scroll_area::ScrollSource {
                drag: egui::scroll_area::DragScroll::Never,
                ..Default::default()
            })
            .show(ui, |ui| {
        for row in 0..self.results.len() {
            let sc = &self.results[row];
            let item = &self.items[sc.idx];
            let selected = row == self.selected;
            let bg = if selected {
                pal().selected_bg
            } else if hover_row == Some(row) {
                // Much fainter than the selection: the pointer says "clickable",
                // it does not say "this is what Enter will run".
                pal().hover_bg
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
                    // The index, the way AltRun printed it in the first column —
                    // just quieter, so it does not fight the icon and the title.
                    // `Ctrl/Alt + N` runs this row; `;` and `'` are the second
                    // and third.
                    ui.add_sized(
                        [14.0, 20.0],
                        egui::Label::new(
                            RichText::new(format!("{}", row + 1))
                                .size(11.0)
                                .color(if selected { pal().text_dim } else { pal().text_faint }),
                        ),
                    );
                    match &icon {
                        // Real shell icon at the display resolution (the texture
                        // is fetched at ICON_SIZE * pixels_per_point physical
                        // pixels and drawn at ICON_SIZE points, so it stays sharp
                        // when scaled).
                        Some(tex) => {
                            ui.add(
                                egui::Image::new(tex)
                                    .fit_to_exact_size(egui::vec2(ICON_SIZE, ICON_SIZE)),
                            );
                        }
                        None => {
                            ui.label(RichText::new(emoji).size(ICON_SIZE - 7.0));
                        }
                    }
                    ui.add_space(4.0);
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
                                .color(pal().text_dim),
                        );
                    });
                    // Launch counter, flush right — this is what fills the wide
                    // empty side of the row.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        match &launches {
                            Some(text) => {
                                ui.label(RichText::new(text).size(11.0).color(pal().text_dim));
                            }
                            None => {
                                // Never launched: say so quietly rather than
                                // leaving the column looking broken.
                                ui.label(
                                    RichText::new("未启动").size(11.0).color(pal().text_faint),
                                );
                            }
                        }
                    });
                });
            });
            let rect = inner.response.rect;
            // The selection is a quiet wash plus a short accent bar on the left —
            // one glance at the bar says which row Enter will run, which a
            // slightly-lighter rectangle never did.
            if selected {
                let bar = egui::Rect::from_min_size(
                    egui::pos2(rect.left() + 1.0, rect.top() + 8.0),
                    egui::vec2(3.0, rect.height() - 16.0),
                );
                ui.painter()
                    .rect_filled(bar, egui::CornerRadius::same(2), pal().accent);
            }
            // A hair of air between rows: they used to butt right up against
            // each other, which read as a table rather than a list.
            ui.add_space(2.0);
            // `click_and_drag`: a click still selects, a drag moves the window —
            // the list is the biggest piece of the card, and AltRun let you drag
            // by its list background too. egui only calls it a drag past its own
            // threshold, so selection and the double click are unaffected.
            let resp = ui.interact(rect, ui.id().with(row), egui::Sense::click_and_drag());
            // **Hover does not select.** AltRun's list had no mouse-move handler
            // at all: the selection moved only with the keyboard, a left click, or
            // a right click (which fakes a left one first). Selecting on hover
            // meant a mouse resting anywhere over the list stole the selection
            // back after every ↑↓ — the arrow keys looked broken (reported
            // 2026-09-19). A faint tint is left as the click affordance.
            if resp.hovered() {
                hovered_next = Some(row);
            }
            if resp.drag_started() {
                drag_now = true;
            }
            // Keep the selection on screen when ↑↓/Tab/digits move it.
            if selected && scroll_selected {
                resp.scroll_to_me(Some(egui::Align::Center));
            }
            // AltRun's list semantics (docs/AltRun交互规格.md §1): a single
            // click only *selects*, and a double click (or the middle button)
            // runs it. Clicking to run was a mis-click waiting to happen once
            // the list grew past a hundred entries.
            if resp.clicked() {
                clicked = Some(row);
            }
            if resp.double_clicked() || resp.clicked_by(egui::PointerButton::Middle) {
                run_now = Some(row);
            }
            // AltRun's list had a right-click menu (添加 / 编辑 / 删除 /
            // 打开所在目录). Add stays on Insert and the tray; the three that
            // act on *this* row are here.
            resp.context_menu(|ui| {
                if ui.button("编辑 (F2)").clicked() {
                    menu_action = Some((row, MenuAction::Edit));
                    ui.close();
                }
                if ui.button("删除 (Delete)").clicked() {
                    menu_action = Some((row, MenuAction::Delete));
                    ui.close();
                }
                if ui.button("打开所在目录 (Ctrl+D)").clicked() {
                    menu_action = Some((row, MenuAction::Reveal));
                    ui.close();
                }
            });
        }
        });
        self.check_status_overlap(status_rect, list_area.inner_rect);
        self.hover_row = hovered_next;
        if drag_now && let Some(hwnd) = main_hwnd() {
            drag_window(hwnd);
            self.save_position();
        }
        if let Some(row) = clicked {
            // Select only — running is the double click below.
            self.selected = row;
        }
        if let Some(row) = run_now {
            self.selected = row;
            self.execute_selected(ctx);
        }
        if let Some((row, action)) = menu_action {
            self.selected = row;
            match action {
                MenuAction::Edit => self.open_edit(),
                MenuAction::Delete => {
                    let ctx = ctx.clone();
                    self.request_delete(&ctx);
                }
                MenuAction::Reveal => self.reveal_selected(),
            }
        }

        if self.results.is_empty() && self.calc_result.is_none() && !self.calc_mode {
            ui.label(
                RichText::new("无匹配结果 · 网页搜索等兜底功能将在后续版本加入")
                    .size(13.0)
                    .color(pal().text_dim),
            );
        }
    }

    /// The status line, as a bottom panel of the card.
    ///
    /// It has to be laid out **before** the list. A `bottom_up` layout placed
    /// after a `ScrollArea` gets no space left — the scroll area has already
    /// taken the whole card — so the text was drawn on top of the last row
    /// (the overlap in the 2026-09-19 screenshot).
    fn render_status_bar(&mut self, ui: &mut egui::Ui) -> egui::Rect {
        let status = self.status.clone();
        // egui 0.36 unified the panel types: this used to be `TopBottomPanel`,
        // and `show_inside` is now `show`.
        let rect = egui::Panel::bottom("mxrun-status")
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                ui.add_space(2.0);
                ui.label(
                    RichText::new(status)
                        .size(11.0)
                        .color(pal().text_faint),
                );
            })
            .response
            .rect;
        // The panel registers its own `click_and_drag` on its resize edge and
        // eats the press there, which made the status strip the one dead spot on
        // the card (measured: a drag at its top edge did nothing). Lay a drag
        // area on top of it — later widgets win — so the whole card drags.
        let drag = ui.interact(
            rect,
            ui.id().with("status-drag"),
            egui::Sense::drag(),
        );
        if drag.drag_started()
            && let Some(hwnd) = main_hwnd()
        {
            drag_window(hwnd);
            self.save_position();
        }
        rect
    }

    /// Complain in the log if the list is drawn over the status line — the
    /// overlap a user found in a screenshot on 2026-09-19. Cheap (two rects),
    /// and it turns a "looks wrong" report into a line of evidence.
    fn check_status_overlap(&self, status: egui::Rect, list: egui::Rect) {
        // A shared edge is not an overlap — the scroll area and the panel meet
        // at exactly the same y, and `Rect::intersects` counts that. Only a
        // real intrusion (more than a pixel) is worth reporting.
        let intrudes = list.bottom() - status.top() > 1.0;
        if intrudes {
            log_line(&format!(
                "layout: list ({:.0}..{:.0}) overlaps the status bar ({:.0}..{:.0})",
                list.top(),
                list.bottom(),
                status.top(),
                status.bottom()
            ));
        } else if std::env::var_os("MXRUN_SELFTEST_RECTS").is_some() {
            log_line(&format!(
                "layout: list {:.0}..{:.0}, status {:.0}..{:.0} (no overlap)",
                list.top(),
                list.bottom(),
                status.top(),
                status.bottom()
            ));
        }
    }

    /// The short note that follows an add which left MxRun resident.
    fn render_toast(&mut self, ui: &mut egui::Ui) {
        let Some(toast) = self.toast.as_ref() else {
            return;
        };
        let green = pal().ok;
        ui.add_space(18.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new(&toast.title).size(19.0).color(green));
            ui.add_space(10.0);
            ui.label(
                RichText::new(&toast.body)
                    .size(13.0)
                    .color(pal().text_dim),
            );
        });
        ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
            ui.label(
                RichText::new("这条提示 2 秒后自己消失")
                    .size(11.0)
                    .color(pal().text_faint),
            );
        });
    }

    /// The manager: AltRun's `frmShortCutMan`, as a view of the same window.
    ///
    /// Columns follow the original's four (ShortCut / Name / Param Type /
    /// Command Line), adapted to this model: keyword, name, kind + command line,
    /// launch count. Deliberately **not** copied from the original: drag
    /// reordering (it dropped the frequency data), inline renaming (it bypassed
    /// duplicate detection) and the checkbox column it never had.
    fn render_manager(&mut self, ui: &mut egui::Ui) {
        // --- filter box (the same search box, with a different hint) ---
        let edit = egui::TextEdit::singleline(&mut self.input)
            .font(FontId::proportional(17.0))
            .hint_text(format!("筛选 {} 条条目…", self.items.len()))
            .desired_width(f32::INFINITY)
            .frame(egui::Frame::default());
        let resp = ui.add(edit);
        if self.want_focus {
            resp.request_focus();
            self.want_focus = false;
        }
        if resp.changed() {
            self.selected = 0;
            self.pending_delete = None;
            self.refresh_search();
        }

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("共 {} 条", self.results.len()))
                    .size(12.0)
                    .color(pal().text_dim),
            );
            ui.add_space(10.0);
            // Which half of the list to look at. Auto-discovered entries are
            // the ones worth reviewing in bulk, so they get their own switch.
            let mut picked = self.manage_source;
            for (label, value) in [
                ("全部", None),
                ("手工 / 导入", Some(SourceFilter::Curated)),
                ("自动发现", Some(SourceFilter::Discovered)),
            ] {
                if ui
                    .selectable_label(self.manage_source == value, RichText::new(label).size(12.0))
                    .clicked()
                {
                    picked = value;
                }
            }
            if picked != self.manage_source {
                self.manage_source = picked;
                self.selected = 0;
                self.refresh_search();
            }
            if let Some(id) = self.pending_delete.clone() {
                if let Some(it) = self.items.iter().find(|i| i.item.id == id) {
                    ui.label(
                        RichText::new(format!("再按一次 Delete 删除「{}」", it.item.title))
                            .size(12.0)
                            .color(pal().danger),
                    );
                }
            }
        });
        ui.separator();

        // --- rows ---
        let mut clicked: Option<usize> = None;
        let mut double: Option<usize> = None;
        let mut menu_action: Option<MenuAction> = None;
        let armed = self.pending_delete.clone();
        let selected = self.selected;
        // The keyboard arrows move this; hover no longer does.
        let new_selected = selected;
        let mut drag_now = false;
        let hover_row = self.hover_row;
        let mut hovered_next = None;

        // Same as the launcher: the status line reserves its space before the
        // scroll area, otherwise the rows draw over it.
        ui.add_space(4.0);
        let status_rect = self.render_status_bar(ui);

        // A filter can empty the manager; say so before the scroll area takes
        // the rest of the card (a label after it would be pushed off).
        if self.results.is_empty() {
            ui.label(
                RichText::new("没有匹配的条目")
                    .size(13.0)
                    .color(pal().text_dim),
            );
            return;
        }

        let manager_area = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            // Wheel and scroll bar scroll it; dragging scrolls nothing — on a
            // frameless card, dragging anywhere should move the window (see the
            // drag handle in eframe::App::ui), which is what AltRun did too.
            .scroll_source(egui::scroll_area::ScrollSource {
                drag: egui::scroll_area::DragScroll::Never,
                ..Default::default()
            })
            .show(ui, |ui| {
                for row in 0..self.results.len() {
                    let sc = &self.results[row];
                    let indexed = &self.items[sc.idx];
                    let item = &indexed.item;
                    let is_armed = armed.as_deref() == Some(item.id.as_str());
                    let is_selected = row == selected;
                    let bg = if is_armed {
                        Color32::from_rgba_unmultiplied(200, 60, 50, 60)
                    } else if is_selected {
                        pal().selected_bg
                    } else if hover_row == Some(row) {
                        pal().hover_bg
                    } else {
                        Color32::TRANSPARENT
                    };
                    let (emoji, category) = match item.default_action() {
                        Some(a) => (a.effect.icon(), a.effect.label()),
                        None => ("•", "条目"),
                    };
                    let keyword = item.keywords.first().cloned().unwrap_or_default();
                    // Same wording as the launcher's rows: a UWP app's target is
                    // an AUMID, which is machine noise in a table (`row_detail`
                    // says "Windows 应用" for those).
                    let detail = truncate_chars(&row_detail(item), 70);
                    let launches = launch_label(indexed.launches).unwrap_or_else(|| "未启动".into());

                    let frame = egui::Frame::new()
                        .fill(bg)
                        .corner_radius(egui::CornerRadius::same(6))
                        .inner_margin(egui::Margin::symmetric(8, 5));
                    let inner = frame.show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(emoji).size(14.0));
                            // keyword, the column AltRun put first
                            ui.add_sized(
                                [110.0, 18.0],
                                egui::Label::new(
                                    RichText::new(truncate_chars(&keyword, 12))
                                        .size(13.0)
                                        .color(pal().accent),
                                )
                                .truncate(),
                            );
                            ui.add_sized(
                                [170.0, 18.0],
                                egui::Label::new(RichText::new(truncate_chars(&item.title, 18)).size(13.0))
                                    .truncate(),
                            );
                            ui.label(
                                RichText::new(format!("{category} · {detail}"))
                                    .size(11.0)
                                    .color(pal().text_dim),
                            );
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(
                                    RichText::new(launches)
                                        .size(11.0)
                                        .color(pal().text_faint),
                                );
                            });
                        });
                    });
                    let rect = inner.response.rect;
                    // The same selection cue as the launcher: the accent bar says
                    // which row Enter / F2 acts on. (The armed-for-delete row is
                    // already red, so it does not need one.)
                    if is_selected && !is_armed {
                        let bar = egui::Rect::from_min_size(
                            egui::pos2(rect.left() + 1.0, rect.top() + 7.0),
                            egui::vec2(3.0, rect.height() - 14.0),
                        );
                        ui.painter()
                            .rect_filled(bar, egui::CornerRadius::same(2), pal().accent);
                    }
                    // Same as the launcher's rows: click selects, drag moves the
                    // window (the manager fills the card too) — and hover does
                    // **not** select, or the arrow keys would fight the pointer.
                    let resp = ui.interact(
                        rect,
                        ui.id().with(("manage-row", row)),
                        egui::Sense::click_and_drag(),
                    );
                    if resp.hovered() {
                        hovered_next = Some(row);
                    }
                    if resp.drag_started() {
                        drag_now = true;
                    }
                    if resp.clicked() {
                        clicked = Some(row);
                    }
                    if resp.double_clicked() {
                        double = Some(row);
                    }
                    resp.context_menu(|ui| {
                        if ui.button("编辑 (F2)").clicked() {
                            menu_action = Some(MenuAction::Edit);
                            ui.close();
                        }
                        if ui.button("删除 (Delete)").clicked() {
                            menu_action = Some(MenuAction::Delete);
                            ui.close();
                        }
                        if ui.button("打开所在目录 (Ctrl+D)").clicked() {
                            menu_action = Some(MenuAction::Reveal);
                            ui.close();
                        }
                    });
                }
            });

        if new_selected != selected {
            self.selected = new_selected;
        }
        if let Some(row) = clicked {
            self.selected = row;
        }
        if let Some(row) = double {
            // AltRun: double-click edits (there is no "run" in the manager).
            self.selected = row;
            self.open_edit();
        }
        if let Some(action) = menu_action {
            match action {
                MenuAction::Edit => self.open_edit(),
                MenuAction::Delete => {
                    let ctx = ui.ctx().clone();
                    self.request_delete(&ctx);
                }
                MenuAction::Reveal => self.reveal_selected(),
            }
        }
        self.check_status_overlap(status_rect, manager_area.inner_rect);
        self.hover_row = hovered_next;
        if drag_now && let Some(hwnd) = main_hwnd() {
            drag_window(hwnd);
            self.save_position();
        }
    }

    /// The first-run question: MxRun has never been registered on this machine.
    /// A card rather than a system dialog: it uses the app's own language, and
    /// Esc ("不用了") is as easy to reach as Enter.
    fn render_integration_ask(&mut self, ui: &mut egui::Ui) {
        let mut answer: Option<bool> = None;
        ui.add_space(10.0);
        ui.label(RichText::new("要把 MxRun 加进右键菜单吗？").size(19.0));
        ui.add_space(10.0);
        for line in [
            "· 右键 → 发送到 → MxRun",
            "· 文件 / 文件夹右键 → 用 MxRun 添加(&M)",
            "",
            "加进去之后，右键只会弹一张小卡片（关键字、名称、命令行已预填），",
            "主窗口不会跳出来打扰你。写在 HKCU 下，不需要管理员权限，",
            "随时能在设置里移除。",
        ] {
            ui.label(
                RichText::new(line)
                    .size(13.0)
                    .color(if line.is_empty() { pal().text_faint } else { pal().text_dim }),
            );
        }
        ui.with_layout(egui::Layout::bottom_up(egui::Align::RIGHT), |ui| {
            ui.horizontal(|ui| {
                if ui.button(RichText::new("加入").size(14.0)).clicked() {
                    answer = Some(true);
                }
                if ui.button(RichText::new("不用了").size(14.0)).clicked() {
                    answer = Some(false);
                }
            });
        });
        if let Some(yes) = answer {
            let ctx = ui.ctx().clone();
            self.answer_integration(&ctx, yes);
        }
    }

    /// The add / edit card. Three fields, the buttons, and a line saying what
    /// is going to happen.
    fn render_add(&mut self, ui: &mut egui::Ui) {
        // Split the borrows: the closures need the form and the focus flag.
        let want_focus = &mut self.want_focus;
        let Some(form) = self.add.as_mut() else {
            return;
        };
        let mut confirm = false;
        let mut cancel = false;

        ui.horizontal(|ui| {
            ui.heading(RichText::new(form.heading()).size(19.0));
            if form.queue.len() > 0 {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("还有 {} 个待添加", form.queue.len()))
                            .size(12.0)
                            .color(pal().text_dim),
                    );
                });
            }
        });
        ui.add_space(10.0);

        egui::Grid::new("mxrun-add-fields")
            .num_columns(2)
            .spacing([10.0, 8.0])
            .show(ui, |ui| {
                let field = |ui: &mut egui::Ui, text: &mut String, hint: &str| {
                    ui.add(
                        egui::TextEdit::singleline(text)
                            .font(FontId::proportional(15.0))
                            .hint_text(hint)
                            .desired_width(408.0),
                    )
                };
                ui.label(RichText::new("关键字").size(14.0));
                let keyword = field(ui, &mut form.keyword, "输入什么能搜到它");
                if *want_focus {
                    keyword.request_focus();
                    *want_focus = false;
                }
                // Editing the keyword after a collision warning means the user
                // is fixing it, not confirming the overwrite.
                if keyword.changed() {
                    form.overwrite_asked = false;
                    form.error.clear();
                }
                ui.end_row();

                ui.label(RichText::new("名称").size(14.0));
                let title = field(ui, &mut form.title, "列表里显示的名字");
                if title.changed() {
                    form.error.clear();
                }
                ui.end_row();

                ui.label(RichText::new("命令行").size(14.0));
                let command = field(ui, &mut form.command, "要打开或运行什么");
                if command.changed() {
                    form.error.clear();
                }
                ui.end_row();
            });

        ui.add_space(8.0);
        // What the card is about. For a right-click add the source path is the
        // context the user just came from.
        let note = if form.is_editing() {
            "回车保存 · Esc 取消 · Tab 换行".to_string()
        } else if form.source_path.is_empty() {
            "回车添加 · Esc 取消".to_string()
        } else {
            format!("来自 {} · 回车添加 · Esc 取消", truncate_chars(&form.source_path, 46))
        };
        ui.label(RichText::new(note).size(11.0).color(pal().text_faint));
        if !form.error.is_empty() {
            ui.label(
                RichText::new(&form.error)
                    .size(12.0)
                    .color(pal().accent),
            );
        }

        // Buttons, flush right (Enter/Esc do the same thing).
        ui.with_layout(egui::Layout::bottom_up(egui::Align::RIGHT), |ui| {
            ui.horizontal(|ui| {
                if ui.button(RichText::new(form.confirm_label()).size(14.0)).clicked() {
                    confirm = true;
                }
                if ui.button(RichText::new("取消").size(14.0)).clicked() {
                    cancel = true;
                }
            });
        });

        // The borrow of `form` ends here, so the store work can run.
        if confirm {
            let ctx = ui.ctx().clone();
            self.confirm_add(&ctx);
        } else if cancel {
            let ctx = ui.ctx().clone();
            self.cancel_add(&ctx);
        }
    }

    /// The parameter prompt, drawn where the search box and the command list
    /// normally are: a chip naming the command, the parameter field, and what
    /// this item was given before.
    fn render_prompt(&mut self, ui: &mut egui::Ui) {
        // Split the borrows up front: the closures below need the prompt and
        // the focus flag at once, and nothing else from `self`.
        let want_focus = &mut self.want_focus;
        let prompt = self.prompt.as_mut().expect("caller checked");

        let accent = pal().accent;

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
            ui.label(RichText::new(msg).size(13.0).color(pal().text_dim));
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
                            .color(if selected { accent } else { pal().text }),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!("用过 {uses} 次"))
                                .size(11.0)
                                .color(pal().text_faint),
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
            ui.label(RichText::new(hint).size(11.0).color(pal().text_faint));
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
            last_used: 0,
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
    /// Diagnostic, not a test: rank a fixed query list against a *copy* of a real
    /// database and print the rows. This is how the "search noise" question gets
    /// answered with numbers.
    ///
    /// ```text
    /// mkdir %TEMP%\diag  &  copy %APPDATA%\MxRun\mxrun.redb %TEMP%\diag\
    /// set MXRUN_DIAG_DIR=%TEMP%\diag
    /// cargo test -- --ignored --nocapture diag_ranking
    /// ```
    #[test]
    #[ignore = "diagnostic: needs MXRUN_DIAG_DIR pointing at a copied data directory"]
    fn diag_ranking() {
        let Ok(dir) = std::env::var("MXRUN_DIAG_DIR") else {
            eprintln!("set MXRUN_DIAG_DIR to a directory holding a copied mxrun.redb");
            return;
        };
        let mut store = Store::open_at(std::path::PathBuf::from(&dir)).expect("open copy");
        let items = index_items(store.load_items().unwrap_or_default(), &mut store);
        let queries: Vec<String> = std::env::var("MXRUN_DIAG_QUERIES")
            .map(|s| s.split(',').map(|q| q.trim().to_string()).collect())
            .unwrap_or_else(|_| {
                ["c", "a", "e", "s", "ch", "chr", "ch", "计算", "jsj", "py", "st", "se"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect()
            });
        println!("\n=== {} items ===", items.len());
        // `MXRUN_DIAG_DUPES=1` answers a different question: which items share a
        // title (the shape that showed two identical `Settings` rows).
        if std::env::var_os("MXRUN_DIAG_DUPES").is_some() {
            let mut by_title: std::collections::HashMap<String, Vec<&Indexed>> =
                std::collections::HashMap::new();
            for it in &items {
                by_title
                    .entry(it.item.title.trim().to_lowercase())
                    .or_default()
                    .push(it);
            }
            let mut dupes: Vec<_> = by_title.iter().filter(|(_, v)| v.len() > 1).collect();
            dupes.sort_by_key(|(t, _)| t.clone());
            println!("\n-- titles appearing more than once: {} --", dupes.len());
            for (title, group) in dupes {
                println!("{title:?}");
                for it in group {
                    println!(
                        "    id={:<44} provider={:<10} target={}",
                        truncate_chars(&it.item.id, 42),
                        it.item.source.provider,
                        truncate_chars(it.item.icon_target().unwrap_or("(none)"), 56)
                    );
                }
            }
            return;
        }
        for q in queries {
            let raw = {
                let pattern = Pattern::parse(&q, CaseMatching::Ignore, Normalization::Smart);
                let mut matcher = Matcher::new(Config::DEFAULT);
                let mut buf: Vec<char> = Vec::new();
                let mut idx: Vec<u32> = Vec::new();
                items
                    .iter()
                    .enumerate()
                    .filter(|(i, it)| {
                        rank_item(*i, it, &q, &pattern, &mut matcher, &mut buf, &mut idx).is_some()
                    })
                    .count()
            };
            let strong = {
                let toks: Vec<&str> = vec![];
                let _ = toks;
                let pattern = Pattern::parse(&q, CaseMatching::Ignore, Normalization::Smart);
                let mut matcher = Matcher::new(Config::DEFAULT);
                let mut buf: Vec<char> = Vec::new();
                let mut idx: Vec<u32> = Vec::new();
                items
                    .iter()
                    .enumerate()
                    .filter(|(i, it)| {
                        rank_item(*i, it, &q, &pattern, &mut matcher, &mut buf, &mut idx)
                            .is_some_and(|sc| sc.strong)
                    })
                    .count()
            };
            let shown = rank_all(&items, &q, 8);
            println!(
                "\nquery {q:?}  (matched {raw}, token-start {strong}, shown {})",
                shown.len()
            );
            for (i, (title, score, sc_strong)) in shown.iter().enumerate() {
                println!(
                    "   {}. {:<28} {score:>8.1} {:<8} {}",
                    i + 1,
                    truncate_chars(title, 26),
                    if *sc_strong { "start" } else { "fuzzy" },
                    target_of(&items, title)
                );
            }
        }
    }

    /// Which file an item points at (for the ranking diagnostic).
    fn target_of(items: &[Indexed], title: &str) -> String {
        items
            .iter()
            .find(|it| it.item.title == title)
            .and_then(|it| it.item.icon_target())
            .map(|t| truncate_chars(t, 58))
            .unwrap_or_default()
    }

    /// The short-query rule rests on one predicate, so pin it down.
    #[test]
    fn token_start_rule() {
        let hay = "character map 系统应用 charmap";
        assert!(is_token_start(hay, "c"));
        assert!(is_token_start(hay, "ch"));
        assert!(is_token_start(hay, "cha"), "prefix of the first word");
        // Not a word start — and this is the case the rule exists for:
        // "character" is c-h-a-r, so `chr` is a *scattered* match. It still
        // scores as a fuzzy hit, it just is not a "starts with".
        assert!(!is_token_start(hay, "chr"));
        assert!(is_token_start(hay, "map"), "prefix of a later word");
        assert!(is_token_start(hay, "系统"), "a CJK title is one word");
        assert!(!is_token_start(hay, "ar"), "inside a word is not a word start");
        assert!(!is_token_start(hay, "racter"));
        assert!(!is_token_start(hay, ""), "an empty query matches nothing");
        // Pinyin syllables are separate tokens, so initials keep working:
        // `计算器` becomes "计算器 calc jisuanqi jsq", where `j` is a word start.
        let pinyin = "计算器 calc jisuanqi jsq";
        assert!(is_token_start(pinyin, "j"));
        assert!(is_token_start(pinyin, "js"));
        assert!(is_token_start(pinyin, "calc"));
        assert!(!is_token_start(pinyin, "sj"));
    }

    /// The "no match → add it" rule and the UWP dedupe share one idea: names are
    /// compared case- and whitespace-insensitively.
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

    // ---- add / edit card --------------------------------------------------

    /// A dropped path arrives with all three fields filled in.
    #[test]
    fn add_card_prefills_from_the_path() {
        let form = AddForm::from_path(r"C:\Tools\My App.exe", vec![]);
        assert_eq!(form.keyword, "My App");
        assert_eq!(form.title, "My App");
        assert_eq!(form.command, r"C:\Tools\My App.exe");
        assert_eq!(form.origin, AddOrigin::External);
        assert!(!form.is_editing());
        assert_eq!(form.confirm_label(), "添加");
    }

    /// F2 fills the same card from the item, and remembers what it is editing.
    #[test]
    fn add_card_edits_an_existing_item() {
        let item = Item {
            id: "x".into(),
            title: "GitHub".into(),
            // Imported rows can carry several trigger words; only the first is
            // on the card, the rest must survive (the confirm path keeps them).
            keywords: vec!["gh".into(), "github".into()],
            actions: vec![Action::run(r"C:\Tools\gh.exe --fast")],
            source: store::Source { provider: "manual".into(), external_id: "c:\\tools\\gh.exe".into() },
            ..Default::default()
        };
        let form = AddForm::from_item(item.clone()).expect("a command line is editable");
        assert_eq!(form.keyword, "gh");
        assert_eq!(form.title, "GitHub");
        assert_eq!(form.command, r"C:\Tools\gh.exe --fast");
        assert_eq!(form.origin, AddOrigin::Launcher);
        assert!(form.is_editing());
        assert_eq!(form.confirm_label(), "保存");
        assert_eq!(form.editing.as_ref().map(|i| i.keywords.len()), Some(2));
    }

    /// Items whose effect is not a command line cannot be edited here — better
    /// to say so than to show an empty box that would overwrite the verb.
    #[test]
    fn add_card_refuses_builtin_effects() {
        let item = Item {
            id: "x".into(),
            title: "显示桌面".into(),
            actions: vec![Action { label: "默认".into(), effect: Effect::Builtin { verb: BuiltinVerb::MinimizeAll } }],
            ..Default::default()
        };
        assert!(AddForm::from_item(item).is_none());
    }

    /// The "no match → add it" entry seeds the card with what was typed.
    #[test]
    fn blank_card_seeds_the_query() {
        let form = AddForm::blank("my thing");
        assert_eq!(form.keyword, "my thing");
        assert_eq!(form.title, "my thing");
        assert!(form.command.is_empty(), "the user supplies the command");
        assert_eq!(form.origin, AddOrigin::Launcher);
        assert!(form.source_path.is_empty(), "no source path to show");
    }

    // ---- keyboard flow (P1: AltRun's key table) ---------------------------

    /// The rule that keeps digit-named items reachable: bare digits are search
    /// text, never a command…
    #[test]
    fn bare_digits_stay_search_text() {
        for key in [Key::Num1, Key::Num7, Key::Num0] {
            assert_eq!(digit_row(key, false, false), None, "{key:?} must be typeable");
        }
        // …and with a modifier they pick a row, 1-based on screen, 0-based here.
        assert_eq!(digit_row(Key::Num1, true, false), Some(0));
        assert_eq!(digit_row(Key::Num1, false, true), Some(0), "Alt works too");
        assert_eq!(digit_row(Key::Num9, true, false), Some(8));
        // The tenth row is `0`, exactly as AltRun's index column printed it.
        assert_eq!(digit_row(Key::Num0, true, false), Some(9));
        // Anything that is not a digit is not our business.
        assert_eq!(digit_row(Key::A, true, false), None);
        assert_eq!(digit_row(Key::F2, true, false), None);
    }

    /// …except with an empty box, where a bare digit runs the row it names.
    #[test]
    fn bare_digits_run_a_row_only_on_an_empty_box() {
        assert_eq!(bare_digit_row(Key::Num3, true), Some(2), "empty box: run row 3");
        assert_eq!(bare_digit_row(Key::Num1, true), Some(0));
        assert_eq!(bare_digit_row(Key::Num0, true), Some(9));
        // The moment there is text in the box, digits belong to the query.
        for key in [Key::Num1, Key::Num3, Key::Num0] {
            assert_eq!(bare_digit_row(key, false), None, "{key:?} must be typeable");
        }
        assert_eq!(bare_digit_row(Key::A, true), None);
        assert_eq!(bare_digit_row(Key::Enter, true), None);
    }

    // ---- AltRun's trailing-digit rule, adapted to a fuzzy matcher ---------

    /// The happy paths: the digit did not help the search, so it is a command.
    #[test]
    fn trailing_digit_runs_a_row_of_the_previous_list() {
        // `steam` showed 5 rows and `steam3` matches nothing (AltRun's case).
        assert_eq!(
            digit_verdict("steam3", "steam", 5, 0, false),
            DigitVerdict::Run(2)
        );
        // Or the digit changed nothing at all — what a fuzzy matcher needs,
        // because it rarely returns zero (`pycharm6` still matches `pycharm64`).
        assert_eq!(
            digit_verdict("pycharm6", "pycharm", 8, 8, true),
            DigitVerdict::Run(5)
        );
        // `0` is the tenth row, as AltRun's index column printed it.
        assert_eq!(
            digit_verdict("steam0", "steam", 10, 0, false),
            DigitVerdict::Run(9)
        );
    }

    /// …and every guard that keeps the rule from eating a number the user is
    /// typing.
    #[test]
    fn digit_rules_keep_their_guardrails() {
        // The digit is filtering for real (`b` 8 rows → `b3` 2 rows).
        assert_eq!(
            digit_verdict("b3", "b", 8, 2, false),
            DigitVerdict::DigitFilters
        );
        // A name with digits in it: the prefix already carries one, so the user
        // is typing `pycharm64`, not picking row 4.
        assert_eq!(
            digit_verdict("pycharm64", "pycharm6", 1, 1, true),
            DigitVerdict::PrefixHasDigit
        );
        // AltRun's second guard: the previous list never reached that row.
        assert_eq!(
            digit_verdict("godot4", "godot", 2, 0, false),
            DigitVerdict::NoSuchRow
        );
        // The empty box is its own rule (`consume_bare_digit`).
        assert_eq!(
            digit_verdict("3", "", 8, 8, true),
            DigitVerdict::NoPreviousList
        );
        // The character was not appended to what the previous list was built
        // from (paste, backspace, a click elsewhere).
        assert_eq!(
            digit_verdict("steam3", "stea", 5, 0, false),
            DigitVerdict::NotAnAppend
        );
        // No digit involved.
        assert_eq!(
            digit_verdict("steam", "stea", 8, 5, false),
            DigitVerdict::NotADigit
        );
        // The previous query matched nothing, so there is no list to point at.
        assert_eq!(
            digit_verdict("zz3", "zz", 0, 0, false),
            DigitVerdict::NoPreviousList
        );
    }

    /// `=N` — the calculator, behind its own prefix.
    #[test]
    fn calculator_prefix() {
        assert!(is_calc_mode("=1+2"));
        assert!(is_calc_mode("  = 1 + 2"), "leading space is forgiven");
        assert!(!is_calc_mode("1+2"), "no prefix, no calculator");
        assert!(is_calc_mode("=x"), "still the calculator, just a bad expression");
        assert!(!is_calc_mode(""));

        assert_eq!(calc_expression("=1+2"), "1+2");
        assert_eq!(calc_expression("= 1 + 2 "), "1 + 2");
        assert_eq!(calc_expression("="), "");
        assert_eq!(calc_expression("1+2"), "", "not in calc mode");

        // The whole point: expressions that start with a digit work now, which
        // the bare-digit rules made impossible without a prefix.
        assert_eq!(eval_calc("1+2").as_deref(), Some("3"));
        assert_eq!(eval_calc("(1+2)*3").as_deref(), Some("9"));
        assert_eq!(eval_calc("2^10").as_deref(), Some("1024"));
        assert_eq!(eval_calc("7%4").as_deref(), Some("3"));
        assert_eq!(eval_calc(""), None, "an empty expression is not a number");
        assert_eq!(eval_calc("hello"), None);
        assert_eq!(eval_calc("1/0"), None, "division by zero has no answer");
        // Division must not be integer division (`100/8` was `12`).
        assert_eq!(eval_calc("100/8").as_deref(), Some("12.5"));
        assert_eq!(eval_calc("10/4").as_deref(), Some("2.5"));
        assert_eq!(eval_calc("8/2").as_deref(), Some("4"), "no trailing .0");
        // Float noise is trimmed, tiny values are not rounded away.
        assert_eq!(eval_calc("0.1+0.2").as_deref(), Some("0.3"));
        assert_eq!(eval_calc("1e-15").as_deref(), Some("0.000000000000001"));
    }

    /// `#N` — the explicit way, which works no matter what the search does.
    #[test]
    fn explicit_row_selector() {
        assert_eq!(split_selector("b#3"), ("b", Some(2)));
        assert_eq!(split_selector("#3"), ("", Some(2)), "the whole list");
        assert_eq!(split_selector("#1"), ("", Some(0)));
        assert_eq!(split_selector("#0"), ("", Some(9)), "0 is the tenth row");
        assert_eq!(split_selector("#12"), ("", Some(11)), "two digits are fine");
        // Still typing it: the list stays put, nothing fires.
        assert_eq!(split_selector("b#"), ("b", None));
        assert_eq!(split_selector("b#x"), ("b", None));
        assert_eq!(split_selector("a#2b"), ("a", None), "not a pure number");
        // No selector at all: the search box is just a search box.
        assert_eq!(split_selector("plain"), ("plain", None));
        assert_eq!(split_selector(""), ("", None));
    }

    /// `;` and `'` are AltRun's second and third rows.
    #[test]
    fn punctuation_rows_match_the_original() {
        assert_eq!(SEMICOLON_ROW, 1, "`;` is the 2nd row on screen");
        assert_eq!(QUOTE_ROW, 2, "`'` is the 3rd row on screen");
    }

    /// `Ctrl+L`: "recently used" means *used*, inside a week.
    #[test]
    fn recent_list_window() {
        let now = 1_800_000_000u64;
        assert!(is_recent(now, now), "just launched");
        assert!(is_recent(now - RECENT_WINDOW_SECS, now), "at the edge");
        assert!(!is_recent(now - RECENT_WINDOW_SECS - 1, now), "one second too old");
        assert!(!is_recent(0, now), "never launched is not recent");
        // A clock that jumped backwards (NTP, DST, a manual fix) must not hide
        // what was just used: a "future" timestamp stays recent.
        assert!(is_recent(now + 10_000, now));
    }
}

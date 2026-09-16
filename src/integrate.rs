//! Outside world → MxRun: turning a path into a command, and registering MxRun
//! in Windows so there is a way to reach it from outside at all.
//!
//! Two halves:
//!
//! * [`item_from_path`] — a file, folder or URL the user dropped on us (via
//!   SendTo, the shell context menu, or `MxRun.exe "<path>"`) becomes an
//!   [`Item`]. The pre-fill rules follow AltRun's `frmShortCut`
//!   (`untShortCutMan.pas:799-882`): `.lnk` resolves to its real target,
//!   the keyword is the file name without extension (a folder keeps its dots),
//!   and the command line is the full path.
//! * [`install_sendto`] / [`install_shell_menu`] — the two entry points. AltRun
//!   had only the SendTo shortcut (its shell-menu code was dead — §11 of the
//!   spec); MxRun writes both, and the shell menu goes to `HKCU`, which needs
//!   no administrator.

use crate::store::{Action, ArgSpec, Health, Item, LaunchMode, Source};
use std::path::{Path, PathBuf};
use windows::Win32::Foundation::WIN32_ERROR;
use windows::core::{Interface, PCWSTR};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, CoInitializeEx, COINIT_APARTMENTTHREADED, IPersistFile,
    STGM_READ,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegDeleteTreeW, RegOpenKeyExW, RegSetValueExW,
};
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};

/// Provider name for everything the user adds by hand or from outside.
/// Distinct from `seed` / `legacy` / `shortcutlist`, so a re-import never
/// overwrites a hand-made item.
pub const PROVIDER: &str = "manual";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The registry helpers return a bare Win32 error code rather than a `Result`.
fn check(status: WIN32_ERROR, what: &str) -> Result<(), String> {
    if status.0 == 0 {
        Ok(())
    } else {
        Err(format!("{what} 失败：Win32 错误码 {}", status.0))
    }
}

/// COM is per-thread: every thread that touches the shell link APIs needs its
/// own apartment (the process-wide init in `main` only covers the main thread).
fn ensure_com() {
    use std::cell::OnceCell;
    thread_local! {
        static ONCE: OnceCell<()> = const { OnceCell::new() };
    }
    ONCE.with(|once| {
        once.get_or_init(|| unsafe {
            // Already-initialised threads report S_FALSE / RPC_E_CHANGED_MODE;
            // both are fine to ignore.
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        });
    });
}

// ---------------------------------------------------------------------------
// path -> item
// ---------------------------------------------------------------------------

/// Build the item MxRun would create for `raw`, before the user edits it.
///
/// The item carries `provider = "manual"` and the (lower-cased) full path as
/// its `external_id`, so adding the same file twice updates one row instead of
/// piling up duplicates — and keeps the launch counter attached to it.
pub fn item_from_path(raw: &str) -> Item {
    let cleaned = raw.trim().trim_matches('"').trim();
    let target = resolve_target(cleaned);
    let title = keyword_for(cleaned, &target);

    Item {
        id: String::new(), // filled in by `Store::upsert_from_source`
        title: title.clone(),
        subtitle: String::new(),
        keywords: vec![title],
        actions: vec![Action::open(&target)],
        arg: ArgSpec::default(),
        launch: LaunchMode::Normal,
        source: Source {
            provider: PROVIDER.to_string(),
            external_id: cleaned.to_lowercase(),
        },
        health: Health::Unknown,
    }
}

/// The string that goes into the command line: `.lnk` files are followed to
/// their target, everything else is used as given.
fn resolve_target(raw: &str) -> String {
    let path = Path::new(raw);
    let is_lnk = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("lnk"));
    if !is_lnk {
        return raw.to_string();
    }
    match resolve_lnk(path) {
        // AltRun kept two kinds of shortcut as-is: ones that resolve to nothing
        // and ones whose target is an MSI icon cache exe (launching *that* is
        // useless, the .lnk is the thing that works).
        Some(t) if !t.as_os_str().is_empty() && !is_msi_icon(&t) => t.to_string_lossy().into_owned(),
        _ => raw.to_string(),
    }
}

fn is_msi_icon(path: &Path) -> bool {
    let Some(windir) = std::env::var_os("WINDIR") else {
        return false;
    };
    let prefix = Path::new(&windir).join("Installer");
    path.to_string_lossy()
        .to_lowercase()
        .starts_with(&prefix.to_string_lossy().to_lowercase())
}

/// The pre-filled keyword: file name without extension, folder name as-is,
/// host name for a URL.
fn keyword_for(raw: &str, target: &str) -> String {
    if let Some(host) = url_host(raw) {
        return host;
    }
    let path = Path::new(target);
    // A folder keeps its dots ("node.js" must not become "node").
    let name = if path.is_dir() {
        path.file_name()
    } else {
        path.file_stem().or_else(|| path.file_name())
    };
    match name {
        Some(n) if !n.is_empty() => n.to_string_lossy().into_owned(),
        // No name at all (a drive root, a bare "::" CLSID): keep something
        // usable rather than an empty keyword the dialog would reject.
        _ => target.trim_end_matches(['\\', '/']).to_string(),
    }
}

/// `http://www.example.com/x` -> `example.com`. Returns None when the string is
/// a path, not a URL.
fn url_host(raw: &str) -> Option<String> {
    let (scheme, rest) = raw.split_once("://")?;
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
    {
        return None;
    }
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or(host); // strip user:pass@
    let host = host.split(':').next().unwrap_or(host); // strip :port
    let host = host.strip_prefix("www.").unwrap_or(host);
    (!host.is_empty()).then(|| host.to_string())
}

/// Follow a `.lnk` to the file it points at. `None` when it cannot be read.
pub fn resolve_lnk(path: &Path) -> Option<PathBuf> {
    ensure_com();
    let link: IShellLinkW = unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }.ok()?;
    let file: IPersistFile = link.cast().ok()?;
    let path_w = wide(&path.to_string_lossy());
    unsafe { file.Load(PCWSTR(path_w.as_ptr()), STGM_READ).ok()? };

    let mut buf = [0u16; 1024];
    unsafe { link.GetPath(&mut buf, std::ptr::null_mut(), 0).ok()? };
    let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    if len == 0 {
        return None;
    }
    Some(PathBuf::from(String::from_utf16_lossy(&buf[..len])))
}

/// Write a shortcut, so `SendTo` (and anything else shell-based) can point here.
pub fn create_lnk(target: &Path, lnk: &Path, icon: Option<&Path>) -> Result<(), String> {
    ensure_com();
    let link: IShellLinkW = unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }
        .map_err(|e| format!("创建 ShellLink 失败：{e}"))?;
    let target_w = wide(&target.to_string_lossy());
    unsafe { link.SetPath(PCWSTR(target_w.as_ptr())) }
        .map_err(|e| format!("SetPath 失败：{e}"))?;
    // Working directory = the exe's own folder, so relative things still work.
    if let Some(dir) = target.parent() {
        let dir_w = wide(&dir.to_string_lossy());
        let _ = unsafe { link.SetWorkingDirectory(PCWSTR(dir_w.as_ptr())) };
    }
    if let Some(icon) = icon.or(Some(target)) {
        let icon_w = wide(&icon.to_string_lossy());
        let _ = unsafe { link.SetIconLocation(PCWSTR(icon_w.as_ptr()), 0) };
    }
    let file: IPersistFile = link.cast().map_err(|e| format!("IPersistFile 失败：{e}"))?;
    let lnk_w = wide(&lnk.to_string_lossy());
    unsafe { file.Save(PCWSTR(lnk_w.as_ptr()), true) }
        .map_err(|e| format!("写入快捷方式失败：{e}"))
}

// ---------------------------------------------------------------------------
// entry point 1: "发送到" (SendTo)
// ---------------------------------------------------------------------------

pub fn sendto_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|a| {
        PathBuf::from(a)
            .join("Microsoft")
            .join("Windows")
            .join("SendTo")
    })
}

pub fn sendto_lnk() -> Option<PathBuf> {
    Some(sendto_dir()?.join("MxRun.lnk"))
}

pub fn sendto_installed() -> bool {
    sendto_lnk().is_some_and(|p| p.exists())
}

/// Put "MxRun" into the right-click → 发送到 menu.
///
/// No arguments on the shortcut: Windows appends the selected files itself,
/// which is exactly how AltRun's `SendTo\ALTRun.lnk` worked (spec §11).
pub fn install_sendto() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("找不到自身路径：{e}"))?;
    let lnk = sendto_lnk().ok_or("找不到 %APPDATA%\\Microsoft\\Windows\\SendTo")?;
    create_lnk(&exe, &lnk, Some(&exe))?;
    Ok(lnk)
}

pub fn uninstall_sendto() -> Result<(), String> {
    let lnk = sendto_lnk().ok_or("找不到 SendTo 目录")?;
    match std::fs::remove_file(&lnk) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("删除 {} 失败：{e}", lnk.display())),
    }
}

// ---------------------------------------------------------------------------
// entry point 2: shell context menu (HKCU — no administrator needed)
// ---------------------------------------------------------------------------

/// Menu text. AltRun's dead code used "Add To ALTRun"; `&M` gives it a
/// keyboard accelerator the way the original's other menu items had one.
pub const MENU_LABEL: &str = "用 MxRun 添加(&M)";

/// `(registry key, what to pass on the command line)`.
///
/// Three places, because right-clicking a file, a folder and empty folder space
/// are three different registry keys in Windows — AltRun wrote `HKCR\*\shell`
/// only, and never called it.
fn shell_menu_entries() -> [(&'static str, &'static str); 3] {
    [
        (r"Software\Classes\*\shell\MxRun", "%1"),
        (r"Software\Classes\Directory\shell\MxRun", "%1"),
        (r"Software\Classes\Directory\Background\shell\MxRun", "%V"),
    ]
}

pub fn shell_menu_installed() -> bool {
    let mut hkey = HKEY::default();
    let sub = wide(r"Software\Classes\*\shell\MxRun");
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(sub.as_ptr()),
            None,
            KEY_READ,
            &mut hkey,
        )
    };
    if status.0 == 0 {
        let _ = unsafe { RegCloseKey(hkey) };
        return true;
    }
    false
}

pub fn install_shell_menu() -> Result<usize, String> {
    let exe = std::env::current_exe().map_err(|e| format!("找不到自身路径：{e}"))?;
    let exe = exe.to_string_lossy().into_owned();
    let mut done = 0;
    for (key, arg) in shell_menu_entries() {
        write_reg_string(key, None, MENU_LABEL)?;
        write_reg_string(key, Some("Icon"), &exe)?;
        write_reg_string(&format!("{key}\\command"), None, &format!("\"{exe}\" \"{arg}\""))?;
        done += 1;
    }
    Ok(done)
}

pub fn uninstall_shell_menu() -> Result<(), String> {
    for (key, _) in shell_menu_entries() {
        let sub = wide(key);
        // Deleting a key that is not there is not an error here.
        let _ = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(sub.as_ptr())) };
    }
    Ok(())
}

fn write_reg_string(key: &str, name: Option<&str>, value: &str) -> Result<(), String> {
    let sub = wide(key);
    let mut hkey = HKEY::default();
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(sub.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hkey,
            None,
        )
    };
    check(status, &format!("创建注册表项 {key}"))?;

    let name_w = name.map(wide);
    let name_ptr = name_w.as_ref().map_or(PCWSTR::null(), |w| PCWSTR(w.as_ptr()));
    // REG_SZ is UTF-16 including the terminating null, as bytes.
    let bytes: Vec<u8> = value
        .encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect();
    let status = unsafe { RegSetValueExW(hkey, name_ptr, None, REG_SZ, Some(&bytes)) };
    let _ = unsafe { RegCloseKey(hkey) };
    check(status, &format!("写入注册表值 {key}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("mxrun-add-{tag}-{nanos}"));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn file_becomes_a_command_named_after_it() {
        let dir = temp_dir("file");
        let file = dir.join("Some App.exe");
        std::fs::write(&file, b"x").unwrap();

        let item = item_from_path(&file.to_string_lossy());
        assert_eq!(item.title, "Some App", "extension is dropped");
        assert_eq!(item.keywords, vec!["Some App"]);
        assert_eq!(item.source.provider, PROVIDER);
        assert!(item.source.external_id.ends_with("some app.exe"), "id is the path");
        match &item.default_action().unwrap().effect {
            crate::store::Effect::Open { target } => {
                assert_eq!(target, &file.to_string_lossy().to_string())
            }
            other => panic!("a dropped file opens, got {other:?}"),
        }
        assert!(!item.wants_input(), "a dropped file needs no typing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A folder keeps the dots in its name — only files lose an extension.
    #[test]
    fn folder_keyword_keeps_its_dots() {
        let dir = temp_dir("folder");
        let folder = dir.join("node.js");
        std::fs::create_dir_all(&folder).unwrap();
        assert_eq!(item_from_path(&folder.to_string_lossy()).title, "node.js");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn urls_are_named_after_the_host() {
        let item = item_from_path("https://www.example.com/some/page?q=1");
        assert_eq!(item.title, "example.com");
        assert_eq!(item.keywords, vec!["example.com"]);
        // A URL is not a file: it must stay verbatim in the command line.
        match &item.default_action().unwrap().effect {
            crate::store::Effect::Open { target } => assert!(target.starts_with("https://www.")),
            other => panic!("unexpected effect {other:?}"),
        }
    }

    #[test]
    fn quotes_and_spaces_from_the_shell_are_stripped() {
        let item = item_from_path("  \"C:\\Program Files\\Thing\\thing.exe\"  ");
        assert_eq!(item.title, "thing");
        assert_eq!(item.source.external_id, r"c:\program files\thing\thing.exe");
    }

    /// A `.lnk` is followed to its target; one that points nowhere stays itself.
    #[test]
    fn shortcuts_resolve_to_their_target() {
        let dir = temp_dir("lnk");
        let target = dir.join("real-target.exe");
        std::fs::write(&target, b"x").unwrap();
        let lnk = dir.join("My Shortcut.lnk");
        create_lnk(&target, &lnk, None).expect("create shortcut");

        assert_eq!(resolve_lnk(&lnk).as_deref(), Some(target.as_path()));
        let item = item_from_path(&lnk.to_string_lossy());
        assert_eq!(item.title, "real-target", "named after the target, not the lnk");
        match &item.default_action().unwrap().effect {
            crate::store::Effect::Open { target: t } => {
                assert_eq!(t, &target.to_string_lossy().to_string())
            }
            other => panic!("unexpected effect {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_shortcut_is_still_addable() {
        let dir = temp_dir("broken");
        let lnk = dir.join("Ghost.lnk");
        std::fs::write(&lnk, b"not really a shortcut").unwrap();
        let item = item_from_path(&lnk.to_string_lossy());
        assert_eq!(item.title, "Ghost", "falls back to the lnk's own name");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Adding the same path twice must not look like two different files
    /// (`external_id` decides identity, and it ignores case).
    #[test]
    fn identity_is_the_path_case_insensitively() {
        let a = item_from_path(r"C:\Tools\App.exe");
        let b = item_from_path(r"c:\tools\app.exe");
        assert_eq!(a.source, b.source);
    }
}

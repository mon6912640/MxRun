//! App auto-discovery (P0-4): the Start Menu is the list of what is installed.
//!
//! Measured on the machine this was written for: 193 shortcuts (175 `.lnk` +
//! 18 `.url`) in the two Start Menu folders, and resolving one `.lnk` to its
//! target costs ~1.8 ms (a COM round trip). So a full scan is ~300 ms — nothing
//! on a background thread, but far too much to put in front of the first frame.
//! Hence:
//!
//! * the scan runs **after** the first frame, on its own thread;
//! * it is **incremental**: each shortcut's `(mtime, size)` is remembered, and
//!   an unchanged file is never resolved again (a repeat start costs ~5 ms);
//! * it never touches an item that already exists — including the user's 59
//!   hand-curated ones — and it refuses to resurrect anything the user deleted.
//!
//! Deduplication is by **resolved target path**, not by name (the design doc's
//! warning): the user's `chrome` entry and a scanned "Google Chrome" are the
//! same `chrome.exe`, and both must not end up in the list.
//!
//! UWP apps live in `shell:AppsFolder` instead (43 of them here, and the
//! enumeration costs ~670 ms because it is a superset containing the desktop
//! apps again). They are deliberately **out of scope** for now — see
//! `docs/开发进度.md` §2.14.

use crate::integrate;
use crate::store::{Action, ArgSpec, Health, Item, LaunchMode, Source, Store};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Provider name for everything found on disk.
pub const PROVIDER: &str = "startmenu";

/// Bump when the filtering rules change: the cache key includes it, so a new
/// rule set re-examines the shortcuts instead of trusting old decisions.
const SCAN_GEN: u32 = 1;

/// Names that are never what somebody wants in a launcher, in three flavours
/// because vendors name things differently:
///
/// * **prefix** terms: an uninstaller is "Uninstall X", "UninstallFoo",
///   "UninstallMyth.Cool" — the trailing side is anybody's guess, the leading
///   side is not;
/// * **substring** terms: CJK has no word boundaries, and "卸载百度网盘" is junk
///   wherever the marker sits;
/// * **word** terms: "Ryzen Master Help Guide" is junk while "HelpDesk Pro" is
///   a real application, so these need boundaries on both sides. Plurals are
///   listed explicitly — no stemming, no surprises.
const JUNK_PREFIXES: &[&str] = &["uninstall", "unins", "remove", "卸载", "反安装"];

const JUNK_SUBSTRINGS: &[&str] = &[
    "卸载", "反安装", "帮助", "说明", "文档", "官网", "主页", "更新", "升级", "修复", "补丁",
    "许可", "激活", "关于", "新闻",
];

const JUNK_WORDS: &[&str] = &[
    "help", "readme", "documentation", "docs", "manual", "manuals",
    "website", "homepage", "online",
    "update", "updates", "upgrade", "upgrades", "repair", "patch", "patches",
    "license", "licence", "licenses", "eula",
    "about", "news", "changelog", "notes",
];

/// What one scan did, for the log, the status line and the settings page.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScanReport {
    /// Shortcut files examined (not the cached ones).
    pub seen: usize,
    /// Unchanged since the last scan, skipped without a COM call.
    pub cached: usize,
    pub skipped_junk: usize,
    pub skipped_web: usize,
    pub skipped_missing: usize,
    /// Already in the list (the user's own entry, or an earlier scan).
    pub existing: usize,
    /// Removed by the user before — a tombstone keeps them out.
    pub tombstoned: usize,
    pub added: usize,
    /// How long the scan took, in milliseconds.
    pub millis: u128,
}

impl ScanReport {
    /// One line for the log.
    pub fn summary(&self) -> String {
        format!(
            "seen={} cached={} added={} existing={} tombstoned={} skipped(junk={} web={} missing={}) in {}ms",
            self.seen,
            self.cached,
            self.added,
            self.existing,
            self.tombstoned,
            self.skipped_junk,
            self.skipped_web,
            self.skipped_missing,
            self.millis
        )
    }

    /// Wording for the status line / settings page.
    pub fn human(&self) -> String {
        if self.added == 0 {
            // This used to print "现有 N 个", which read as "the list has N
            // items" — it is really how many the scan already knew about, so a
            // cached re-scan showed "没有新应用（现有 2 个）" on a 157-item list
            // (spotted in a screenshot, 2026-09-17). Say what was examined.
            format!(
                "已扫描开始菜单：没有新应用（这次检查了 {} 个快捷方式，{} 个没变化，用时 {}ms）",
                self.seen, self.cached, self.millis
            )
        } else {
            format!(
                "自动发现 {} 个应用（跳过 {} 个卸载/帮助类、{} 个网页快捷方式）",
                self.added, self.skipped_junk, self.skipped_web
            )
        }
    }
}

/// The two Start Menu folders: the user's own and the machine-wide one.
///
/// Order matters: the user's shortcuts win, so a name they created themselves
/// takes precedence over the vendor's.
pub fn start_menu_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(appdata) = std::env::var_os("APPDATA") {
        roots.push(
            PathBuf::from(appdata)
                .join("Microsoft")
                .join("Windows")
                .join("Start Menu")
                .join("Programs"),
        );
    }
    if let Some(programdata) = std::env::var_os("ProgramData") {
        roots.push(
            PathBuf::from(programdata)
                .join("Microsoft")
                .join("Windows")
                .join("Start Menu")
                .join("Programs"),
        );
    }
    roots
}

/// Everything a scan needs from the database, read up front on the UI thread.
///
/// The scan itself cannot open the store: redb keeps an exclusive lock on the
/// file, and a second `Store` in the worker thread fails with
/// "another program has locked a portion of the file" (measured, 2026-09-17).
/// So the worker does filesystem and COM work only, and the UI thread applies
/// the result — which is also the only thread allowed to write.
#[derive(Default)]
pub struct ScanInput {
    /// Target paths already claimed by items in the list (the dedupe key).
    pub known: std::collections::HashSet<String>,
    /// `scan:` cache entries: shortcut path -> `gen:mtime:size`.
    pub cache: std::collections::HashMap<String, String>,
}

/// What a scan produced: items to add, cache entries to store, what happened.
#[derive(Default)]
pub struct ScanOutput {
    pub items: Vec<Item>,
    pub cache_updates: Vec<(String, String)>,
    pub report: ScanReport,
}

/// Read what the scan needs. Cheap: one `load_items` plus one range query.
pub fn read_input(store: &mut Store) -> ScanInput {
    ScanInput {
        known: known_targets(store),
        cache: store.scan_cache(),
    }
}

/// Scan the Start Menu: pure filesystem + COM work, no database access.
pub fn collect(input: &ScanInput, force: bool) -> ScanOutput {
    let started = std::time::Instant::now();
    let mut out = ScanOutput::default();
    let mut seen_targets = input.known.clone();
    let mut fresh: Vec<Item> = Vec::new();

    for root in start_menu_roots() {
        walk(&root, &mut |path| {
            let name = match path.file_stem() {
                Some(n) => n.to_string_lossy().into_owned(),
                None => return,
            };
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();

            // Web shortcuts are out of scope by decision (2026-09-17): this
            // launcher opens files and apps, and a Start Menu full of vendor
            // links is exactly the noise auto-discovery is criticised for.
            if ext == "url" {
                out.report.skipped_web += 1;
                return;
            }
            if ext != "lnk" {
                return;
            }

            // Cheap checks before the 1.8 ms COM call.
            if is_junk_name(&name) {
                out.report.skipped_junk += 1;
                remember(&mut out, &path);
                return;
            }
            if !force && unchanged(input, &path) {
                out.report.cached += 1;
                return;
            }

            out.report.seen += 1;
            remember(&mut out, &path);

            let Some(target) = integrate::resolve_lnk(&path) else {
                out.report.skipped_missing += 1;
                return;
            };
            if !target.is_file() || !is_launchable(&target) {
                out.report.skipped_missing += 1;
                return;
            }
            let key = normalize(&target);
            if !seen_targets.insert(key) {
                // The same executable is already in the list — either the
                // user's own entry or another shortcut for it.
                out.report.existing += 1;
                return;
            }

            fresh.push(item_for(&name, &target, &path));
        });
    }

    out.report.millis = started.elapsed().as_millis();
    out.items = fresh;
    out
}

/// Store what the scan found. Runs on the thread that owns the `Store`.
pub fn apply(store: &mut Store, mut out: ScanOutput) -> ScanReport {
    store.set_meta_many(&out.cache_updates);
    match store.insert_scanned(std::mem::take(&mut out.items)) {
        Ok((added, existing, tombstoned)) => {
            out.report.added = added;
            out.report.existing += existing;
            out.report.tombstoned = tombstoned;
        }
        Err(e) => {
            crate::log_line(&format!("discover: writing failed: {e}"));
        }
    }
    out.report
}

/// Scan and store in one call — what a caller that already owns a `Store` uses
/// (the tests, and any future "rescan" button that runs on this thread).
#[allow(dead_code)]
pub fn scan(store: &mut Store, force: bool) -> ScanReport {
    let input = read_input(store);
    let out = collect(&input, force);
    apply(store, out)
}

/// Every path that is already covered by an item in the list.
///
/// `pub(crate)` for the tests: the dedupe rule is the part that must not rot.
pub(crate) fn known_targets(store: &mut Store) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    let items = store.load_items().unwrap_or_default();
    for item in items {
        // Only the launch target matters; an item that opens a URL or runs a
        // builtin verb has no path to collide with.
        if let Some(action) = item.default_action()
            && let Some(target) = crate::action_command(action)
        {
            if target.contains("://") || target.starts_with("::") {
                continue;
            }
            if let Some(path) = crate::icon_source(target) {
                set.insert(normalize(&path));
            }
        }
    }
    set
}

/// The item a Start Menu shortcut becomes.
fn item_for(name: &str, target: &Path, lnk: &Path) -> Item {
    let command = target.to_string_lossy().into_owned();
    Item {
        id: String::new(), // filled in by the store: `startmenu:<path>`
        title: name.to_string(),
        subtitle: String::new(),
        // AltRun's rule for anything that comes in from outside: the keyword is
        // the name, and the name is the keyword.
        keywords: vec![name.to_string()],
        actions: vec![Action::open(&command)],
        arg: ArgSpec::default(),
        launch: LaunchMode::Normal,
        source: Source {
            provider: PROVIDER.to_string(),
            external_id: lnk.to_string_lossy().to_lowercase(),
        },
        health: Health::Unknown,
    }
}

/// Is this something a launcher should offer? Documents, drivers and archives
/// that a vendor dropped in the Start Menu are not.
fn is_launchable(target: &Path) -> bool {
    const OK: &[&str] = &["exe", "com", "bat", "cmd", "ps1", "lnk", "msc", "cpl", "jar"];
    target
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .is_some_and(|e| OK.contains(&e.as_str()))
}

/// Names nobody wants in a launcher: uninstallers, help files, vendor links.
fn is_junk_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    // All-hex names are internal ids that leaked into the menu.
    if lower.len() >= 8 && lower.chars().all(|c| c.is_ascii_hexdigit()) {
        return true;
    }
    if JUNK_PREFIXES.iter().any(|t| lower.starts_with(t)) {
        return true;
    }
    if JUNK_SUBSTRINGS.iter().any(|t| lower.contains(t)) {
        return true;
    }
    JUNK_WORDS.iter().any(|t| contains_word(&lower, t))
}

/// Does `name` contain `term` as a whole word ("help" but not "HelpDesk")?
fn contains_word(name: &str, term: &str) -> bool {
    let mut from = 0;
    while let Some(pos) = name[from..].find(term) {
        let start = from + pos;
        let end = start + term.len();
        let left_ok = start == 0
            || !name[..start]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric());
        let right_ok = end == name.len()
            || !name[end..].chars().next().is_some_and(|c| c.is_alphanumeric());
        if left_ok && right_ok {
            return true;
        }
        from = end;
    }
    false
}

/// Comparable form of a path: expanded, unquoted, lower-cased.
///
/// Case is irrelevant on Windows and `%WINDIR%` must not make the same file
/// look like two — the user's imported `myip` points at `nslookup` while the
/// Start Menu shortcut points at `C:\Windows\System32\nslookup.exe`.
pub(crate) fn normalize(path: &Path) -> String {
    let expanded = crate::exec::expand_env(&path.to_string_lossy());
    expanded
        .trim()
        .trim_matches('"')
        .replace('/', "\\")
        .to_lowercase()
}

/// Walk a directory tree, calling `visit` for every file. Missing roots are
/// fine (a fresh profile has no Start Menu folder yet).
fn walk(dir: &Path, visit: &mut impl FnMut(PathBuf)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, visit);
        } else if path.is_file() {
            visit(path);
        }
    }
}

/// Did this file change since we last looked at it?
fn unchanged(input: &ScanInput, path: &Path) -> bool {
    let Some(stamp) = stamp_of(path) else {
        return false;
    };
    input
        .cache
        .get(&cache_key(path))
        .is_some_and(|v| v == &format!("{SCAN_GEN}:{stamp}"))
}

/// Note the file's stamp so the next scan can skip it without a COM call.
fn remember(out: &mut ScanOutput, path: &Path) {
    if let Some(stamp) = stamp_of(path) {
        out.cache_updates
            .push((cache_key(path), format!("{SCAN_GEN}:{stamp}")));
    }
}

fn stamp_of(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(format!("{mtime}:{}", meta.len()))
}

fn cache_key(path: &Path) -> String {
    format!("scan:{}", path.to_string_lossy().to_lowercase())
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
        let dir = std::env::temp_dir().join(format!("mxrun-disc-{tag}-{nanos}"));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn junk_names_are_recognised_without_eating_real_apps() {
        for name in [
            "Uninstall PixPin",
            "卸载百度网盘",
            "UninstallMyth.Cool",   // vendor style: no separator at all
            "Ryzen Master Help Guide",
            "Website",
            "Python 3.12 Manuals (64-bit)",
            "Check for Updates",
            "关于我们",
            "Release Notes",
            "308046B0AF4A39CB",     // an internal id that leaked into the menu
            "Remove Foo",
            "Foo 更新程序",
        ] {
            assert!(is_junk_name(name), "{name} should be filtered");
        }
        for name in [
            "Google Chrome",
            "Visual Studio Code",
            "HelpDesk Pro",  // "help" inside a word
            "UpdateHub",     // "update" inside a word
            "Notesmith",     // "notes" inside a word
            "Steam",
            "Node.js",
            "Godot Engine",
            "Everything",
        ] {
            assert!(!is_junk_name(name), "{name} should survive");
        }
    }

    #[test]
    fn paths_compare_case_and_variable_insensitively() {
        assert_eq!(
            normalize(Path::new(r"C:\Windows\System32\Notepad.EXE")),
            normalize(Path::new(r"c:\windows\system32\notepad.exe"))
        );
        assert_eq!(
            normalize(Path::new(r"C:/Windows/notepad.exe")),
            normalize(Path::new(r"C:\Windows\notepad.exe"))
        );
        // `%WINDIR%` expands, so an imported item and a scanned shortcut agree.
        let windir = std::env::var("WINDIR").expect("WINDIR is always set");
        assert_eq!(
            normalize(Path::new(r"%WINDIR%\notepad.exe")),
            normalize(&PathBuf::from(windir).join("notepad.exe"))
        );
    }

    #[test]
    fn only_launchable_files_count() {
        assert!(is_launchable(Path::new(r"C:\x\app.exe")));
        assert!(is_launchable(Path::new(r"C:\x\tool.bat")));
        assert!(!is_launchable(Path::new(r"C:\x\readme.txt")));
        assert!(!is_launchable(Path::new(r"C:\x\guide.chm")));
        assert!(!is_launchable(Path::new(r"C:\x\data.zip")));
    }

    /// The shortcut a scan creates carries the source that makes re-scans
    /// idempotent and keeps the frecency counter out of it.
    #[test]
    fn items_carry_a_stable_source() {
        let item = item_for("Google Chrome", Path::new(r"C:\Apps\chrome.exe"), Path::new(r"C:\Menu\Chrome.lnk"));
        assert_eq!(item.title, "Google Chrome");
        assert_eq!(item.keywords, vec!["Google Chrome"]);
        assert_eq!(item.source.provider, PROVIDER);
        assert_eq!(item.source.external_id, r"c:\menu\chrome.lnk");
        assert!(!item.wants_input());
        assert_eq!(item.arg, ArgSpec::default(), "no argument, no encoding");
    }

    /// A scan of a folder tree finds the shortcuts and ignores everything else.
    /// The walk is pure filesystem work, so it can be tested for real.
    #[test]
    fn walking_finds_shortcuts_and_descends_into_folders() {
        let dir = temp_dir("walk");
        std::fs::create_dir_all(dir.join("Sub Folder")).unwrap();
        std::fs::write(dir.join("A.lnk"), b"x").unwrap();
        std::fs::write(dir.join("Sub Folder").join("B.lnk"), b"x").unwrap();
        std::fs::write(dir.join("Sub Folder").join("notes.txt"), b"x").unwrap();

        let mut found: Vec<String> = Vec::new();
        walk(&dir, &mut |p| found.push(p.file_name().unwrap().to_string_lossy().into_owned()));
        found.sort();
        assert_eq!(found, vec!["A.lnk", "B.lnk", "notes.txt"]);

        // A missing root is not an error (fresh profile).
        walk(&dir.join("nope"), &mut |_| panic!("nothing to visit"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The scan cache is what keeps repeat starts cheap: same file, same stamp,
    /// no COM call.
    #[test]
    fn repeat_scans_skip_unchanged_files() {
        let dir = temp_dir("stamp");
        let file = dir.join("App.lnk");
        std::fs::write(&file, b"first").unwrap();

        let mut input = ScanInput::default();
        assert!(!unchanged(&input, &file), "never looked at before");

        let mut out = ScanOutput::default();
        remember(&mut out, &file);
        assert_eq!(out.cache_updates.len(), 1);
        for (k, v) in out.cache_updates {
            input.cache.insert(k, v);
        }
        assert!(unchanged(&input, &file), "same size and time");

        // Touching the file (new content, new size) invalidates it.
        std::fs::write(&file, b"second, longer").unwrap();
        assert!(!unchanged(&input, &file), "content changed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The dedupe set is built from real items, by resolved path — the rule the
    /// design doc insists on.
    #[test]
    fn known_targets_come_from_the_items_paths() {
        let dir = temp_dir("known");
        let mut store = Store::open_at(dir.join("data")).unwrap();
        let exe = dir.join("Some App.exe");
        std::fs::write(&exe, b"x").unwrap();

        let item = Item {
            id: "manual:x".into(),
            title: "my thing".into(),
            keywords: vec!["mt".into()],
            actions: vec![Action::open(exe.to_string_lossy().into_owned())],
            source: Source { provider: "manual".into(), external_id: "x".into() },
            ..Default::default()
        };
        store.upsert_item(&item).unwrap();

        let known = known_targets(&mut store);
        assert!(known.contains(&normalize(&exe)), "the path is claimed: {known:?}");
        // A URL item claims nothing (there is no file to collide with).
        let url = Item {
            id: "manual:u".into(),
            title: "bing".into(),
            actions: vec![Action::open("https://www.bing.com")],
            source: Source { provider: "manual".into(), external_id: "u".into() },
            ..Default::default()
        };
        store.upsert_item(&url).unwrap();
        assert!(!known_targets(&mut store).iter().any(|k| k.contains("bing")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

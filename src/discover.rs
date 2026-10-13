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
//! UWP apps live in `shell:AppsFolder`: they have no shortcut on disk and no
//! `.exe` to point at, so they are enumerated separately and launched through
//! their AUMID (`shell:AppsFolder\<AUMID>`). Measured here: 217 entries in
//! ~670 ms, of which 43 are UWP (the rest are the desktop apps again — the
//! folder is a superset). The `!` in the AUMID
//! (`Microsoft.WindowsCalculator_8wekyb3d8bbwe!App`) is what tells them apart.

use crate::integrate;
use crate::store::{Action, ArgSpec, Health, Item, LaunchMode, Source, Store};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Provider name for everything found on disk.
pub const PROVIDER: &str = "startmenu";

/// Provider name for UWP / Store apps.
pub const UWP_PROVIDER: &str = "uwp";

/// The shell namespace folder that lists every launchable app entry.
const APPS_FOLDER: &str = "shell:AppsFolder";

/// Prefix a UWP item's launch target carries.
const UWP_TARGET_PREFIX: &str = "shell:AppsFolder\\";

/// Is this provider one of ours (something a scan put in the list)?
///
/// The manager's "自动发现" filter and the bulk delete both key on this, so it
/// must know about **every** discovery source — forgetting UWP here would file
/// 41 scanned apps under "手工 / 导入".
pub fn is_discovered_provider(provider: &str) -> bool {
    provider == PROVIDER || provider == UWP_PROVIDER
}

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
    /// System / maintenance tools the scan refuses to add (see is_system_tool).
    pub skipped_system: usize,
    /// Already in the list (the user's own entry, or an earlier scan).
    pub existing: usize,
    /// Removed by the user before — a tombstone keeps them out.
    pub tombstoned: usize,
    pub added: usize,
    /// UWP entries the shell reported (before filtering).
    pub uwp_seen: usize,
    /// UWP apps already in the list by name.
    pub uwp_existing: usize,
    /// How long the AppsFolder enumeration took, in milliseconds.
    pub uwp_millis: u128,
    /// Duplicate UWP entries dropped because a desktop item had the same name.
    pub removed_duplicates: usize,
    /// Stored system tools dropped because today's rules filter them out.
    pub removed_filtered: usize,
    /// Ambiguously named entries that got a folder hint appended.
    pub renamed_ambiguous: usize,
    /// How long the scan took, in milliseconds.
    pub millis: u128,
}

impl ScanReport {
    /// One line for the log.
    pub fn summary(&self) -> String {
        format!(
            "seen={} cached={} added={} existing={} tombstoned={} dropped(dupes={} filtered={}) renamed={} skipped(junk={} web={} missing={} system={}) uwp(seen={} existing={} in {}ms) in {}ms",
            self.seen,
            self.cached,
            self.added,
            self.existing,
            self.tombstoned,
            self.removed_duplicates,
            self.removed_filtered,
            self.renamed_ambiguous,
            self.skipped_junk,
            self.skipped_web,
            self.skipped_missing,
            self.skipped_system,
            self.uwp_seen,
            self.uwp_existing,
            self.uwp_millis,
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
                "自动发现 {} 个应用（开始菜单 {} + 系统应用 {}；跳过 {} 个卸载/帮助类、{} 个网页快捷方式）",
                self.added,
                self.added.saturating_sub(self.uwp_added()),
                self.uwp_added(),
                self.skipped_junk,
                self.skipped_web
            )
        }
    }

    /// How many of the added items came from `shell:AppsFolder`.
    fn uwp_added(&self) -> usize {
        // `added` counts both passes; the settings page only needs a rough split,
        // and the exact number is in the log line.
        self.uwp_seen.saturating_sub(self.uwp_existing)
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
    /// Titles already in the list — the dedupe key for UWP entries, which have
    /// no path to compare.
    pub titles: Vec<String>,
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
    let titles = store
        .load_items()
        .unwrap_or_default()
        .into_iter()
        .map(|item| item.title)
        .collect();
    ScanInput {
        known: known_targets(store),
        titles,
        cache: store.scan_cache(),
    }
}

/// One entry from `shell:AppsFolder`: the name a user sees, and the parsing name
/// (which is an AUMID for UWP apps and a plain path for desktop ones).
struct AppEntry {
    name: String,
    parsing: String,
}

/// Is this parsing name a UWP / Store app?
///
/// UWP AUMIDs are `PackageFamilyName!ApplicationId`; desktop entries in the same
/// folder are ordinary paths. Pure, so the rule can be tested.
fn is_uwp_aumid(parsing: &str) -> bool {
    parsing.contains('!') && !parsing.contains('\\') && !parsing.contains('/')
}

/// Enumerate `shell:AppsFolder`. COM work only — no database, no UI.
fn enumerate_apps_folder() -> Vec<AppEntry> {
    use windows::Win32::UI::Shell::{
        BHID_EnumItems, IEnumShellItems, IShellItem, SHCreateItemFromParsingName,
        SIGDN_DESKTOPABSOLUTEPARSING, SIGDN_NORMALDISPLAY,
    };
    use windows::core::PCWSTR;

    crate::integrate::ensure_com();
    let wide: Vec<u16> = APPS_FOLDER.encode_utf16().chain(std::iter::once(0)).collect();
    let mut out = Vec::new();
    unsafe {
        let Ok(folder) = SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(wide.as_ptr()), None)
        else {
            crate::log_line("discover: shell:AppsFolder could not be opened");
            return out;
        };
        let Ok(enumerator) = folder.BindToHandler::<_, IEnumShellItems>(None, &BHID_EnumItems)
        else {
            crate::log_line("discover: shell:AppsFolder could not be enumerated");
            return out;
        };
        loop {
            let mut slot: [Option<IShellItem>; 1] = [None];
            let mut fetched = 0u32;
            if enumerator.Next(&mut slot, Some(&mut fetched)).is_err() || fetched == 0 {
                break;
            }
            let Some(item) = slot[0].take() else { break };
            let (Ok(name), Ok(parsing)) = (
                item.GetDisplayName(SIGDN_NORMALDISPLAY),
                item.GetDisplayName(SIGDN_DESKTOPABSOLUTEPARSING),
            ) else {
                continue;
            };
            let (Ok(name), Ok(parsing)) = (name.to_string(), parsing.to_string()) else {
                continue;
            };
            if name.trim().is_empty() || parsing.trim().is_empty() {
                continue;
            }
            out.push(AppEntry { name, parsing });
        }
    }
    out
}

/// The item a UWP app becomes: it launches through its AUMID, and the icon comes
/// from the same shell path (the icon worker resolves anything through
/// `SHCreateItemFromParsingName`, which handles app entries too).
fn uwp_item_for(name: &str, aumid: &str) -> Item {
    let command = format!("{UWP_TARGET_PREFIX}{aumid}");
    Item {
        id: String::new(), // filled in by the store: `uwp:<aumid>`
        title: name.to_string(),
        subtitle: String::new(),
        keywords: vec![name.to_string()],
        actions: vec![Action::open(&command)],
        arg: ArgSpec::default(),
        launch: LaunchMode::Normal,
        // A UWP app is launched by AUMID through the shell: it has no working
        // directory of its own.
        working_dir: None,
        source: Source {
            provider: UWP_PROVIDER.to_string(),
            external_id: aumid.to_lowercase(),
        },
        health: Health::Unknown,
    }
}

/// Was this UWP app already in the list by *name*?
///
/// UWP entries cannot be deduped by path — there is no path, only an AUMID — but
/// the same app often exists already: either the user typed it in, or the Start
/// Menu scan found the `.exe` shim that launches it (`calc` → 计算器). Comparing
/// names is the heuristic the design doc called for.
fn title_taken(name: &str, taken: &std::collections::HashSet<String>) -> bool {
    taken.contains(&normalize_name(name))
}

fn normalize_name(name: &str) -> String {
    name.trim().to_lowercase()
}

/// Remove UWP duplicates that a **non**-UWP item already covers by name.
///
/// The scan no longer creates these (the UWP pass now knows the names the Start
/// Menu pass just found), but lists written before that fix still hold them —
/// the ranking diagnostic showed two `Settings` rows. Deleting **without** a
/// tombstone is deliberate: if the user later removes the desktop entry, the app
/// should be discoverable again on the next scan.
pub fn cleanup_duplicate_titles(store: &mut Store) -> usize {
    let items = store.load_items().unwrap_or_default();
    let mut desktop_titles: std::collections::HashSet<String> = std::collections::HashSet::new();
    for item in &items {
        if item.source.provider != UWP_PROVIDER {
            desktop_titles.insert(normalize_name(&item.title));
        }
    }
    let mut removed = 0;
    for item in &items {
        if item.source.provider == UWP_PROVIDER && desktop_titles.contains(&normalize_name(&item.title))
        {
            crate::log_line(&format!(
                "discover: dropping duplicate UWP entry {:?} (a desktop entry has the same name)",
                item.title
            ));
            if store.delete_item_by_id(&item.id).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

/// Windows tools people actually launch from a launcher.
///
/// Everything else in `%WINDIR%\System32` / `SysWOW64` is a console, a debugger
/// or a vendor diagnostic that nobody types into a launcher: the scan used to
/// add `Application Verifier`, `Event Viewer`, `services`, `Steps Recorder`,
/// `System Information`, `ODBC Data Sources`, `Windows App Cert Kit` — and they
/// took the *top* of short queries (see the ranking diagnostic in
/// `docs/开发进度.md` §2.29). An allow-list is the manageable direction: a
/// blacklist of Windows' own tools never ends.
///
/// A user who does want one of them adds it by hand; manual entries are never
/// filtered.
const WINDOWS_TOOLS_TO_KEEP: &[&str] = &[
    "cmd",
    "powershell",
    "pwsh",
    "wt",
    "notepad",
    "calc",
    "mspaint",
    "explorer",
    "control",
    "taskmgr",
    "snippingtool",
    "osk",
    "magnify",
    "charmap",
    "dxdiag",
];

/// Should the scan skip this target as a system / maintenance tool?
///
/// Three rules, all measured against a real Start Menu (§2.29):
/// * `.msc` / `.cpl` — console snap-ins and control-panel applets;
/// * anything under `\Windows\Installer\` — MSI cache leftovers that only
///   trigger a repair;
/// * `System32` / `SysWOW64` files that are not on [`WINDOWS_TOOLS_TO_KEEP`].
///
/// Pure (path in, decision out), so the rules can be tested.
fn is_system_tool(target: &Path) -> bool {
    let lower = target.to_string_lossy().to_lowercase();
    let ext = target
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if ext == "msc" || ext == "cpl" {
        return true;
    }
    if lower.contains(r"\windows\installer\") {
        return true;
    }
    // Windows Kits is the SDK: App Cert Kit, Performance Analyzer, gflags and
    // friends. Same allow-list treatment.
    let in_windows_dirs = lower.contains(r"\windows\system32\")
        || lower.contains(r"\windows\syswow64\")
        || lower.contains(r"\windows\winsxs\")
        || lower.contains(r"\windows kits\");
    if !in_windows_dirs {
        return false;
    }
    let stem = target
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    !WINDOWS_TOOLS_TO_KEEP.contains(&stem.as_str())
}

/// Drop discovered items that today's rules would never have added.
///
/// The filter above only protects *new* scans; lists written before it keep
/// their `Application Verifier`, `Event Viewer`, `services.msc` and friends —
/// and those are exactly the rows that were taking the top of short queries.
/// This is the self-healing half.
///
/// Two guard rails: only items a scan put there (`startmenu` — a manual or
/// imported entry is never touched), and only ones **never launched** — if the
/// user actually uses one of them from MxRun, it stays.
pub fn cleanup_filtered_discovered(store: &mut Store) -> usize {
    let items = store.load_items().unwrap_or_default();
    let mut removed = 0;
    for item in &items {
        if item.source.provider != PROVIDER {
            continue;
        }
        if store.get_frecency(&item.id).count > 0 {
            continue;
        }
        let Some(target) = item.default_action().and_then(crate::action_command) else {
            continue;
        };
        if !is_system_tool(Path::new(&target)) {
            continue;
        }
        crate::log_line(&format!(
            "discover: dropping {:?} — a system tool today's rules filter out",
            item.title
        ));
        if store.delete_item_by_id(&item.id).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Give discovered items that share a name a hint of where they came from.
///
/// `Settings` is the real case (2026-09-19): TortoiseGit and TortoiseSVN both
/// ship a shortcut by that name, so the manager showed two identical rows. The
/// Start Menu folder a shortcut lives in is the natural disambiguator, and it is
/// recoverable from the stored `external_id` (the `.lnk` path), so this repairs
/// existing lists too.
///
/// Idempotent: a title that already carries a hint is left alone, so running it
/// after every scan cannot grow `Settings · TortoiseGit · TortoiseGit`.
pub fn disambiguate_titles(store: &mut Store) -> usize {
    let items = store.load_items().unwrap_or_default();
    let mut groups: std::collections::HashMap<String, Vec<&Item>> = std::collections::HashMap::new();
    for item in &items {
        if item.source.provider == PROVIDER {
            groups
                .entry(normalize_name(&item.title))
                .or_default()
                .push(item);
        }
    }
    let mut renamed = 0usize;
    for (_, group) in groups.iter().filter(|(_, g)| g.len() > 1) {
        for item in group {
            if item.title.contains(" · ") {
                continue; // already disambiguated by an earlier run
            }
            let Some(hint) = folder_hint(&item.source.external_id) else {
                continue;
            };
            if normalize_name(&hint) == normalize_name(&item.title) {
                continue; // the folder says nothing new
            }
            let mut updated: Item = (*item).clone();
            updated.title = format!("{} · {}", item.title.trim(), hint);
            if store.upsert_item(&updated).is_ok() {
                renamed += 1;
                crate::log_line(&format!(
                    "discover: {:?} -> {:?} (name was ambiguous)",
                    item.title, updated.title
                ));
            }
        }
    }
    renamed
}

/// The Start Menu folder a `.lnk` path sits in, as a disambiguation hint.
fn folder_hint(lnk_path: &str) -> Option<String> {
    let path = Path::new(lnk_path);
    let folder = path.parent()?.file_name()?.to_string_lossy().into_owned();
    // Generic containers say nothing useful.
    const GENERIC: &[&str] = &[
        "programs",
        "start menu",
        "programdata",
        "microsoft",
        "windows",
    ];
    if GENERIC.contains(&folder.to_lowercase().as_str()) {
        return None;
    }
    Some(folder)
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
            if is_system_tool(&target) {
                out.report.skipped_system += 1;
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

    // --- UWP / Store apps (shell:AppsFolder) ---
    //
    // A second pass with its own provider tag: no shortcut on disk, no `.exe`,
    // launched by AUMID. Dedupe is by name here (there is no path to compare) —
    // the same app usually already exists as the user's own entry or as the
    // `.exe` shim the Start Menu scan found.
    let uwp_started = std::time::Instant::now();
    // Names to avoid: what the list already has, **plus what this same scan just
    // found on the Start Menu**. Missing the second half put two `Settings` rows
    // in the list — one from a shortcut, one from the Store (spotted in the
    // ranking diagnostic, 2026-09-19).
    let mut titles: std::collections::HashSet<String> = input
        .titles
        .iter()
        .map(|t| normalize_name(t))
        .chain(fresh.iter().map(|item| normalize_name(&item.title)))
        .collect();
    for entry in enumerate_apps_folder() {
        if !is_uwp_aumid(&entry.parsing) {
            continue;
        }
        out.report.uwp_seen += 1;
        if is_junk_name(&entry.name) {
            out.report.skipped_junk += 1;
            continue;
        }
        if title_taken(&entry.name, &titles) {
            out.report.uwp_existing += 1;
            continue;
        }
        titles.insert(normalize_name(&entry.name));
        fresh.push(uwp_item_for(&entry.name, &entry.parsing));
    }
    out.report.uwp_millis = uwp_started.elapsed().as_millis();

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
    out.report.removed_duplicates = cleanup_duplicate_titles(store);
    out.report.removed_filtered += cleanup_filtered_discovered(store);
    out.report.renamed_ambiguous = disambiguate_titles(store);
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
    // The command line, not the bare exe: a Start Menu shortcut may carry
    // arguments (`chrome.exe --app=…`), and AltRun keeps them
    // (`untUtilities.pas:881-887`). `target` stays the *path*, because that is
    // what identity, dedupe and the system-tool filter are about.
    let command = integrate::resolve_lnk_command(lnk)
        .unwrap_or_else(|| target.to_string_lossy().into_owned());
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
        // A Start Menu shortcut can set "start in" too, and a Start Menu entry
        // is a `.lnk` like any other (`Item::working_dir`).
        working_dir: integrate::lnk_working_dir(lnk),
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

    /// UWP entries: what counts as one, what an item looks like, and the name
    /// dedupe that keeps a second copy of the same app out of the list.
    #[test]
    fn uwp_rules() {
        // Every discovery source must answer yes here, or the manager's
        // "自动发现" filter files scanned apps under "手工 / 导入".
        assert!(is_discovered_provider(PROVIDER));
        assert!(is_discovered_provider(UWP_PROVIDER));
        assert!(!is_discovered_provider("manual"));
        assert!(!is_discovered_provider("legacy"));

        // UWP AUMIDs are `Family!App`; desktop entries in the same folder are
        // ordinary paths and must not be mistaken for apps.
        assert!(is_uwp_aumid("Microsoft.WindowsCalculator_8wekyb3d8bbwe!App"));
        assert!(is_uwp_aumid("Microsoft.WindowsStore_8wekyb3d8bbwe!App"));
        assert!(!is_uwp_aumid(r"C:\Program Files\Google\Chrome\chrome.exe"));
        assert!(!is_uwp_aumid("notepad"));

        let item = uwp_item_for("计算器", "Microsoft.WindowsCalculator_8wekyb3d8bbwe!App");
        assert_eq!(item.title, "计算器");
        assert_eq!(item.keywords, vec!["计算器".to_string()]);
        assert_eq!(item.source.provider, UWP_PROVIDER);
        // The store fills `id` from the source; the external id is the AUMID,
        // lowercased so a case change cannot produce a second item.
        assert_eq!(
            item.source.external_id,
            "microsoft.windowscalculator_8wekyb3d8bbwe!app"
        );
        assert_eq!(
            item.default_action()
                .and_then(crate::action_command)
                .as_deref(),
            Some(r"shell:AppsFolder\Microsoft.WindowsCalculator_8wekyb3d8bbwe!App"),
            "UWP apps launch through their AUMID"
        );

        // Name dedupe: whatever the list already has by title is not added again
        // (the user's own `计算器`, or the `calc.exe` shim the menu scan found).
        let mut taken = std::collections::HashSet::new();
        taken.insert(normalize_name(" 计算器 "));
        assert!(title_taken("计算器", &taken));
        assert!(title_taken("  计算器 ", &taken));
        assert!(!title_taken("照片", &taken));
    }

    /// A UWP entry that a desktop item already covers by name gets dropped — and
    /// dropped *without* a tombstone, so it can come back if the user removes the
    /// desktop entry later.
    #[test]
    fn duplicate_titles_are_cleaned_up() {
        let dir = temp_dir("dup-cleanup");
        let mut store = Store::open_at(dir.clone()).expect("store");

        let mut desktop = uwp_item_for("Settings", "ignored");
        desktop.id = "startmenu:test-settings".to_string();
        desktop.source.provider = PROVIDER.to_string();
        desktop.source.external_id = r"c:\windows\settings.lnk".to_string();
        desktop.title = "Settings".to_string();
        let mut duplicate = uwp_item_for("Settings", "Microsoft.Windows.Settings_abc!App");
        duplicate.id = "uwp:test-settings".to_string();
        let mut keeper = uwp_item_for("照片", "Microsoft.Windows.Photos_abc!App");
        keeper.id = "uwp:test-photos".to_string();
        store.upsert_item(&desktop).expect("insert desktop");
        store.upsert_item(&duplicate).expect("insert uwp dup");
        store.upsert_item(&keeper).expect("insert uwp keeper");

        assert_eq!(cleanup_duplicate_titles(&mut store), 1, "one duplicate");
        let left: Vec<String> = store
            .load_items()
            .unwrap()
            .into_iter()
            .map(|i| i.title)
            .collect();
        assert!(left.contains(&"Settings".to_string()), "the desktop one stays");
        assert!(left.contains(&"照片".to_string()), "an unrelated UWP app stays");
        assert!(
            !store.is_deleted(&duplicate.source.provider, &duplicate.source.external_id),
            "no tombstone: it may return"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// System / maintenance tools stay out of the list; the ones people actually
    /// launch stay in.
    #[test]
    fn system_tools_are_filtered() {
        let p = |s: &str| PathBuf::from(s);
        // Consoles and control-panel applets.
        assert!(is_system_tool(&p(r"C:\WINDOWS\system32\eventvwr.msc")));
        assert!(is_system_tool(&p(r"C:\WINDOWS\system32\services.msc")));
        assert!(is_system_tool(&p(r"C:\WINDOWS\system32\joy.cpl")));
        // MSI cache leftovers: launching one only triggers a repair.
        assert!(is_system_tool(&p(
            r"C:\WINDOWS\Installer\{BC1FA5CF-A36F-4C61-9638-09D0B431B00D}\foo.exe"
        )));
        // Windows' own directories: only the allow-list survives.
        assert!(is_system_tool(&p(r"C:\Windows\System32\appverif.exe")));
        assert!(is_system_tool(&p(r"C:\Windows\SysWOW64\AMDBugReportTool.exe")));
        assert!(is_system_tool(&p(r"C:\WINDOWS\system32\psr.exe")));
        assert!(is_system_tool(&p(r"C:\WINDOWS\system32\msinfo32.exe")));
        assert!(!is_system_tool(&p(r"C:\Windows\System32\cmd.exe")));
        assert!(!is_system_tool(&p(r"C:\Windows\System32\notepad.exe")));
        assert!(!is_system_tool(&p(r"C:\Windows\System32\calc.exe")));
        assert!(!is_system_tool(&p(r"C:\Windows\explorer.exe")));
        // Everything outside Windows' directories is untouched — this rule must
        // never eat a normal application.
        assert!(!is_system_tool(&p(
            r"C:\Program Files\Google\Chrome\chrome.exe"
        )));
        assert!(!is_system_tool(&p(
            r"C:\Program Files\AMD\CNext\RadeonSoftware.exe"
        )));
        assert!(!is_system_tool(&p(r"S:\games\Steam\Steam.exe")));
        assert!(!is_system_tool(&p(r"R:\apps\AltRun\Bin\AltRun.exe")));
    }

    /// Two apps with the same name (TortoiseGit's and TortoiseSVN's `Settings`)
    /// get told apart by the Start Menu folder they came from — and running the
    /// pass twice must not keep appending to the name.
    #[test]
    fn ambiguous_names_get_a_folder_hint() {
        let dir = temp_dir("disambiguate");
        let mut store = Store::open_at(dir.clone()).expect("store");

        let mk = |id: &str, lnk: &str| {
            let mut item = uwp_item_for("Settings", "ignored");
            item.id = id.to_string();
            item.source.provider = PROVIDER.to_string();
            item.source.external_id = lnk.to_string();
            item
        };
        let a = mk("startmenu:a", r"R:\Menu\TortoiseGit\Settings.lnk");
        let b = mk("startmenu:b", r"R:\Menu\TortoiseSVN\Settings.lnk");
        let lone = {
            let mut item = mk("startmenu:c", r"R:\Menu\TortoiseGit\Steam.lnk");
            item.title = "Steam".to_string();
            item
        };
        store.upsert_item(&a).unwrap();
        store.upsert_item(&b).unwrap();
        store.upsert_item(&lone).unwrap();

        assert_eq!(disambiguate_titles(&mut store), 2, "both rows are ambiguous");
        let titles: Vec<String> = store
            .load_items()
            .unwrap()
            .into_iter()
            .map(|i| i.title)
            .collect();
        assert!(titles.contains(&"Settings · TortoiseGit".to_string()));
        assert!(titles.contains(&"Settings · TortoiseSVN".to_string()));
        assert!(
            titles.contains(&"Steam".to_string()),
            "an unambiguous name is left alone"
        );

        // Second run: nothing left to do, and no `· TortoiseGit · TortoiseGit`.
        assert_eq!(disambiguate_titles(&mut store), 0);
        assert!(
            store
                .load_items()
                .unwrap()
                .iter()
                .all(|i| !i.title.contains(" · TortoiseGit · "))
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// The folder hint must not fire on the generic containers, or every scanned
    /// item in `Programs` would get a pointless suffix.
    #[test]
    fn folder_hint_ignores_generic_folders() {
        assert_eq!(
            folder_hint(r"R:\roaming\Microsoft\Windows\Start Menu\Programs\Steam.lnk"),
            None
        );
        assert_eq!(
            folder_hint(r"C:\ProgramData\Microsoft\Windows\Start Menu\Programs\TortoiseGit\Settings.lnk"),
            Some("TortoiseGit".to_string())
        );
        assert_eq!(folder_hint(""), None);
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

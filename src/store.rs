//! MxRun storage layer: redb (ACID embedded KV) + rolling backups.
//!
//! Design notes (vs AltRun's INI file):
//! - All writes go through redb write transactions -> crash at any point
//!   never leaves a half-written file.
//! - On every launch, the db file is copied into `backups/` (rolling 10)
//!   BEFORE the database is opened, so a corrupted db can always be rolled
//!   back manually.
//! - Everything can be exported to human-readable JSON.
//!
//! ## Item model (P0-1)
//!
//! The model follows `docs/命令模型设计.md`: classification runs along two
//! orthogonal axes instead of AltRun's flat "command line + param type" tuple.
//!
//! - **Axis 1 — effect** (closed set): what the program can actually *do*.
//!   `Open` / `Run` / `Builtin` / `Copy` / `Reveal`. New capabilities add a
//!   verb, not a type.
//! - **Axis 2 — source** (open set): where the item came from
//!   (`provider` + `external_id`). Importers and query providers hang off
//!   this, so adding one never touches the core enums.
//!
//! An item carries a *list* of actions (Enter runs the first); parameters are
//! an attribute of the action, not a type of item.
//!
//! ## Migration safety
//!
//! Every struct/enum here is `#[serde(default)]`, so rows written by an older
//! build still deserialize. On top of that, `open_at` migrates v1 rows
//! (`Command`) into `Item`s, driven by the `schema_version` meta key. **Nothing
//! is ever dropped silently**: every row that cannot be parsed is recorded in
//! `Store::warnings` so `main` can write it to `mxrun.log`.

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const COMMANDS: TableDefinition<&str, &str> = TableDefinition::new("commands");
const META: TableDefinition<&str, &str> = TableDefinition::new("meta");

/// Written into the meta table; absent means "v1 (legacy `Command` rows)".
const SCHEMA_VERSION_KEY: &str = "schema_version";
const SCHEMA_VERSION: u32 = 2;

// ---------------------------------------------------------------------------
// The model
// ---------------------------------------------------------------------------

/// One selectable result. Static items live in the db; dynamic ones (e.g.
/// Everything results) only exist for the current query.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Item {
    /// Stable id. **Frecency is keyed by it — migration must never change it.**
    pub id: String,
    pub title: String,
    pub subtitle: String,
    /// Trigger words; the first one is the primary keyword.
    pub keywords: Vec<String>,
    /// At least one; the first is the default action (what Enter runs).
    pub actions: Vec<Action>,
    pub arg: ArgSpec,
    pub launch: LaunchMode,
    pub source: Source,
    pub health: Health,
}

impl Item {
    pub fn default_action(&self) -> Option<&Action> {
        self.actions.first()
    }

    /// The string the shell icon is resolved from (used by the icon pipeline).
    pub fn icon_target(&self) -> Option<&str> {
        match &self.default_action()?.effect {
            Effect::Open { target } => Some(target),
            Effect::Run { line } => Some(line),
            Effect::Reveal { path } => Some(path),
            Effect::Builtin { .. } | Effect::Copy { .. } => None,
        }
    }

    /// Everything that should be matchable by the fuzzy matcher.
    pub fn haystack(&self) -> String {
        let mut s = String::with_capacity(self.title.len() + self.subtitle.len() + 16);
        s.push_str(&self.title);
        if !self.subtitle.is_empty() {
            s.push(' ');
            s.push_str(&self.subtitle);
        }
        for kw in &self.keywords {
            s.push(' ');
            s.push_str(kw);
        }
        s
    }

    /// True when this item needs the user to *type* something before it can
    /// run — drives the `*` marker. Clipboard and foreground-window sources
    /// need no typing, so they must not be marked.
    #[allow(dead_code)]
    pub fn wants_input(&self) -> bool {
        self.arg.source == ArgSource::Prompt
    }
}

/// An action: an item may carry several (Enter = the first).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Action {
    pub label: String,
    pub effect: Effect,
}

impl Action {
    pub fn open(target: impl Into<String>) -> Self {
        Self { label: "打开".into(), effect: Effect::Open { target: target.into() } }
    }
    pub fn run(line: impl Into<String>) -> Self {
        Self { label: "运行".into(), effect: Effect::Run { line: line.into() } }
    }
    /// Reserved: file-search results and the CRUD screen attach this (P1+).
    #[allow(dead_code)]
    pub fn reveal(path: impl Into<String>) -> Self {
        Self { label: "打开所在目录".into(), effect: Effect::Reveal { path: path.into() } }
    }
    /// Reserved: used by file-search results and the CRUD screen (P1+).
    #[allow(dead_code)]
    pub fn copy(text: impl Into<String>) -> Self {
        Self { label: "复制".into(), effect: Effect::Copy { text: text.into() } }
    }
    /// Reserved: the window-control / power verbs land in P0-2.
    #[allow(dead_code)]
    pub fn builtin(verb: BuiltinVerb) -> Self {
        Self { label: verb.label().into(), effect: Effect::Builtin { verb } }
    }
}

/// Axis 1: what actually happens. Closed set — extend with verbs, not types.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Effect {
    /// Hand a target to the shell: program, file, folder, URL, protocol,
    /// CLSID, `shell:` path — Windows itself does not distinguish these.
    Open { target: String },
    /// Run a command line (may carry arguments, expansion of `{p}`, etc).
    Run { line: String },
    /// Done in-process by MxRun itself.
    Builtin { verb: BuiltinVerb },
    /// Put text on the clipboard.
    Copy { text: String },
    /// Reveal a path in Explorer.
    Reveal { path: String },
    // Reserved seam: `Custom { provider, payload }` for plugins/scripts.
}

impl Effect {
    /// Short category label shown in the result row.
    pub fn label(&self) -> &'static str {
        match self {
            Effect::Open { target } => {
                let t = target.trim().to_ascii_lowercase();
                if t.starts_with("http://") || t.starts_with("https://") {
                    "网址"
                } else if t.starts_with("::") || t.starts_with("shell:") {
                    "系统"
                } else if Path::new(target.trim_matches('"')).is_dir() {
                    "目录"
                } else {
                    "应用"
                }
            }
            Effect::Run { .. } => "命令",
            Effect::Builtin { .. } => "动作",
            Effect::Copy { .. } => "复制",
            Effect::Reveal { .. } => "定位",
        }
    }

    /// Emoji fallback when the shell gives us no icon.
    pub fn icon(&self) -> &'static str {
        match self {
            Effect::Open { .. } => "🚀",
            Effect::Run { .. } => "⌨",
            Effect::Builtin { .. } => "⚙",
            Effect::Copy { .. } => "📋",
            Effect::Reveal { .. } => "📂",
        }
    }
}

/// In-process verbs (AltRun needed a bundled `WinCtl.exe` for these).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinVerb {
    MinimizeAll,
    ShowDesktop,
    HideForegroundWindow,
    ShowForegroundWindow,
    /// AltRun's `ShowOnly`: keep the front window, get the rest out of the way.
    HideOthers,
    Shutdown,
    Reboot,
    /// AltRun's 「运行」: execute whatever the user typed (also the seam for a
    /// future "no match -> run it" fallback).
    RunInput,
}

impl BuiltinVerb {
    /// Reserved: shown in the result list once the verbs are implemented (P0-2).
    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            BuiltinVerb::MinimizeAll => "最小化全部窗口",
            BuiltinVerb::ShowDesktop => "显示桌面",
            BuiltinVerb::HideForegroundWindow => "隐藏当前窗口",
            BuiltinVerb::ShowForegroundWindow => "恢复窗口",
            BuiltinVerb::HideOthers => "只显示当前窗口",
            BuiltinVerb::Shutdown => "关机",
            BuiltinVerb::Reboot => "重启",
            BuiltinVerb::RunInput => "运行",
        }
    }
}

/// How the target is shown when launched (replaces AltRun's `@+`/`@-`/`@`).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LaunchMode {
    #[default]
    Normal,
    Maximized,
    Minimized,
    Hidden,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    #[default]
    Unknown,
    Ok,
    Broken,
}

/// Argument specification: three independent facts.
///
/// AltRun collapsed all of this into one 4-valued `param_type` enum that also
/// gated variable substitution (which is why its window-control items had to
/// pretend to "take a parameter").
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct ArgSpec {
    /// Where the value comes from.
    pub source: ArgSource,
    /// How it is encoded before insertion.
    pub encode: Encoder,
    /// How it lands in the command.
    pub insert: InsertMode,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArgSource {
    #[default]
    None,
    /// Ask the user (inline prompt; AltRun popped `frmParam`).
    Prompt,
    Clipboard,
    ForegroundId,
    ForegroundTitle,
    ForegroundClass,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Encoder {
    #[default]
    Raw,
    UrlQuery,
    Utf8Percent,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InsertMode {
    #[default]
    None,
    /// Replace the `{p}` marker in the target/line.
    Replace,
    /// Append to the end (how AltRun's search engines work).
    Append,
}

/// Axis 2: provenance. Makes imports idempotent and keeps sources apart.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq, Hash)]
#[serde(default)]
pub struct Source {
    /// "seed" | "legacy" | "manual" | "shortcutlist" | "startmenu" | "everything" | ...
    pub provider: String,
    /// Unique within that provider: a line number, a .lnk path, a full path…
    pub external_id: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default)]
pub struct Frecency {
    pub count: u32,
    pub last_used: u64,
}

/// v1 row shape, kept only for migration.
#[derive(Deserialize)]
struct LegacyCommand {
    id: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    desc: String,
}

impl LegacyCommand {
    /// v1 -> v2. The id is preserved because frecency is keyed by it.
    fn into_item(self) -> Item {
        let LegacyCommand { id, kind, path, name, desc } = self;
        // v1 stored "run this command" as kind=cmd, everything else was a
        // shell target (file/dir/url/CLSID).
        let action = if kind == "cmd" {
            Action::run(path.clone())
        } else {
            Action::open(path.clone())
        };
        let arg = if path.contains("{p}") {
            ArgSpec { source: ArgSource::Prompt, encode: Encoder::Raw, insert: InsertMode::Replace }
        } else {
            ArgSpec::default()
        };
        Item {
            source: Source { provider: "legacy".into(), external_id: id.clone() },
            id,
            title: name,
            subtitle: desc,
            keywords: Vec::new(),
            actions: vec![action],
            arg,
            launch: LaunchMode::Normal,
            health: Health::Unknown,
        }
    }
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

pub struct Store {
    db: Database,
    /// Data directory (backups / exports live here). Read by `export_json`.
    #[allow(dead_code)]
    pub data_dir: PathBuf,
    /// Problems hit while opening/migrating/loading. `main` logs these: the
    /// old code dropped unparseable rows without a word.
    pub warnings: Vec<String>,
}

/// Meta key holding one parameter's use count for one item.
///
/// Item ids contain `:` themselves (`shortcutlist:line:7`), which is harmless:
/// the prefix is matched whole, never split.
fn param_key(item_id: &str, value: &str) -> String {
    format!("param:{item_id}:{value}")
}

/// Prefix of the discovery scan cache (`scan:<shortcut path>`), so the whole
/// thing can be read back in one range query.
const SCAN_PREFIX: &str = "scan:";

/// Meta key remembering that the user deleted a source.///
/// Keyed by `(provider, external_id)` rather than by id, because that is what
/// an import can recognise — the id it would have used is gone with the row.
fn gone_key(provider: &str, external_id: &str) -> String {
    format!("gone:{provider}:{external_id}")
}

/// How many parameters are remembered per item. AltRun's `ParamHistoryLimit`
/// default was 50 — same number, but per item instead of global.
pub const PARAM_HISTORY_LIMIT: usize = 50;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Marker file that switches the whole app into portable mode: put an empty
/// `portable.txt` next to `mxrun.exe` and everything — database, backups, log,
/// the hand-off inbox — lives in a `data/` folder beside the exe instead of
/// `%APPDATA%`.
///
/// That is what makes the folder copyable: the settings (and with them "the
/// right-click entries are mine") travel with it, and nothing is left behind on
/// the machine you happened to run it from.
pub const PORTABLE_MARKER: &str = "portable.txt";

/// Where this run keeps its data: the portable folder if the marker is there,
/// otherwise `%APPDATA%\MxRun`.
pub fn default_data_dir() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from));
    data_dir_for(exe_dir.as_deref(), std::env::var_os("APPDATA").as_deref())
}

/// The rule above, as a pure function — so it can be tested without writing
/// markers next to the test executable.
fn data_dir_for(exe_dir: Option<&Path>, appdata: Option<&std::ffi::OsStr>) -> PathBuf {
    if let Some(dir) = exe_dir {
        if dir.join(PORTABLE_MARKER).exists() {
            return dir.join("data");
        }
    }
    appdata
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("MxRun")
}

/// True when the app is running from a portable folder (see
/// [`PORTABLE_MARKER`]). Used for the startup line in the log.
pub fn is_portable() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join(PORTABLE_MARKER).exists()))
        .unwrap_or(false)
}

impl Store {
    /// Open the database under `%APPDATA%\MxRun`, backing up any existing file
    /// first and migrating old rows if needed.
    pub fn open() -> Result<Self, Box<dyn std::error::Error>> {
        Self::open_at(default_data_dir())
    }

    /// Same, at an explicit directory — so tests can use a temp dir.
    pub fn open_at(data_dir: PathBuf) -> Result<Self, Box<dyn std::error::Error>> {
        fs::create_dir_all(&data_dir)?;
        let db_path = data_dir.join("mxrun.redb");

        if db_path.exists() {
            Self::rotate_backup(&data_dir, &db_path)?;
        }

        let db = Database::create(&db_path)?;
        // Ensure tables exist.
        let tx = db.begin_write()?;
        {
            let _ = tx.open_table(COMMANDS)?;
            let _ = tx.open_table(META)?;
        }
        tx.commit()?;

        let mut store = Self { db, data_dir, warnings: Vec::new() };
        store.migrate_if_needed()?;

        if store.load_items()?.is_empty() {
            store.seed_defaults()?;
        }
        Ok(store)
    }

    fn rotate_backup(data_dir: &Path, db_path: &Path) -> std::io::Result<()> {
        let backup_dir = data_dir.join("backups");
        fs::create_dir_all(&backup_dir)?;

        let stamp = {
            let t = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!("{:06x}-{:08x}", t / 1_000_000, t % 1_000_000)
        };
        fs::copy(db_path, backup_dir.join(format!("mxrun-{stamp}.redb")))?;

        // Keep only the newest 10 backups.
        let mut files: Vec<_> = fs::read_dir(&backup_dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "redb"))
            .collect();
        files.sort();
        while files.len() > 10 {
            let old = files.remove(0);
            let _ = fs::remove_file(old);
        }
        Ok(())
    }

    // ----- meta helpers -------------------------------------------------

    fn read_meta(&self, key: &str) -> Option<String> {
        let tx = self.db.begin_read().ok()?;
        let table = tx.open_table(META).ok()?;
        let v = table.get(key).ok()??;
        Some(v.value().to_string())
    }

    fn write_meta(&self, key: &str, value: &str) {
        if let Ok(tx) = self.db.begin_write() {
            {
                if let Ok(mut table) = tx.open_table(META) {
                    let _ = table.insert(key, value);
                }
            }
            let _ = tx.commit();
        }
    }

    fn remove_meta(&self, key: &str) {
        if let Ok(tx) = self.db.begin_write() {
            {
                if let Ok(mut table) = tx.open_table(META) {
                    let _ = table.remove(key);
                }
            }
            let _ = tx.commit();
        }
    }

    // ----- migration ----------------------------------------------------

    /// Runs the v1 -> v2 conversion once, driven by the `schema_version` key.
    ///
    /// Rows are converted only here (never during a normal load), because a v1
    /// row *does* deserialize into an `Item` once every field has a default —
    /// it would silently turn into an empty item.
    fn migrate_if_needed(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let current = self
            .read_meta(SCHEMA_VERSION_KEY)
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(1);
        if current >= SCHEMA_VERSION {
            return Ok(());
        }

        let mut converted: Vec<(String, String)> = Vec::new();
        let mut broken: Vec<String> = Vec::new();
        {
            let tx = self.db.begin_read()?;
            let table = tx.open_table(COMMANDS)?;
            for row in table.iter()? {
                let (k, v) = row?;
                let id = k.value().to_string();
                let raw = v.value().to_string();
                if id.is_empty() {
                    continue;
                }
                match serde_json::from_str::<LegacyCommand>(&raw) {
                    Ok(old) => converted.push((id, serde_json::to_string(&old.into_item())?)),
                    // Keep the row untouched: never destroy data we cannot read.
                    Err(_) => broken.push(id.clone()),
                }
            }
        }

        if !converted.is_empty() {
            let tx = self.db.begin_write()?;
            {
                let mut table = tx.open_table(COMMANDS)?;
                for (id, json) in &converted {
                    table.insert(id.as_str(), json.as_str())?;
                }
            }
            tx.commit()?;
            self.warnings
                .push(format!("migrate: v{current} -> v{SCHEMA_VERSION}, {} 条已转换", converted.len()));
        }
        for id in &broken {
            self.warnings
                .push(format!("migrate: 记录 id={id} 无法解析为旧结构，已保留原样（未删除）"));
        }

        self.write_meta(SCHEMA_VERSION_KEY, &SCHEMA_VERSION.to_string());
        Ok(())
    }

    fn seed_defaults(&self) -> Result<(), Box<dyn std::error::Error>> {
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(COMMANDS)?;
            for item in seed_items() {
                table.insert(item.id.as_str(), serde_json::to_string(&item)?.as_str())?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ----- item CRUD ----------------------------------------------------

    /// Load every item. Unreadable rows are reported, never dropped silently.
    pub fn load_items(&mut self) -> Result<Vec<Item>, Box<dyn std::error::Error>> {
        let mut out = Vec::new();
        let mut warnings = Vec::new();
        {
            let tx = self.db.begin_read()?;
            let table = tx.open_table(COMMANDS)?;
            for row in table.iter()? {
                let (k, v) = row?;
                let id = k.value().to_string();
                let raw = v.value();
                match serde_json::from_str::<Item>(raw) {
                    Ok(item) if !item.actions.is_empty() => out.push(item),
                    Ok(_) => warnings.push(format!(
                        "load: 跳过无动作的记录 id={id}（结构不完整）"
                    )),
                    Err(err) => {
                        // Defensive: a row that migration did not reach but is
                        // still in the v1 shape.
                        if let Ok(old) = serde_json::from_str::<LegacyCommand>(raw) {
                            out.push(old.into_item());
                        } else {
                            warnings.push(format!("load: 跳过无法解析的记录 id={id}（{err}）"));
                        }
                    }
                }
            }
        }
        self.warnings.append(&mut warnings);
        Ok(out)
    }

    /// Insert or replace one item by its id.
    /// Reserved for the CRUD screen; `upsert_from_source` builds on it.
    #[allow(dead_code)]
    pub fn upsert_item(&self, item: &Item) -> Result<(), Box<dyn std::error::Error>> {
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(COMMANDS)?;
            table.insert(item.id.as_str(), serde_json::to_string(item)?.as_str())?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Import seam: store an item coming from a provider, keyed by
    /// `(provider, external_id)` so re-importing updates instead of
    /// duplicating — and so the local frecency (keyed by id) survives.
    ///
    /// Returns the id the item ended up under, or `None` when the user deleted
    /// this source before: a deleted item must not come back the next time the
    /// same list is imported (see [`Store::delete_item`]).
    pub fn upsert_from_source(&mut self, mut item: Item) -> Result<Option<String>, Box<dyn std::error::Error>> {
        if item.source.provider.is_empty() {
            return Err("upsert_from_source: 缺少 provider".into());
        }
        if self.is_deleted(&item.source.provider, &item.source.external_id) {
            return Ok(None);
        }
        if !item.source.external_id.is_empty() {
            for existing in self.load_items()? {
                if existing.source == item.source {
                    item.id = existing.id; // keep the id -> keep the frecency
                    break;
                }
            }
        }
        if item.id.is_empty() {
            item.id = format!("{}:{}", item.source.provider, item.source.external_id);
        }
        self.upsert_item(&item)?;
        Ok(Some(item.id))
    }

    /// Remove an item, and remember that the user did not want it.
    ///
    /// Two things happen beyond dropping the row:
    ///
    /// * its frecency and parameter history go too — they are keyed by id, and
    ///   a later item that happens to reuse the id (a re-import of the same
    ///   line, say) must not inherit the counts of the thing the user threw
    ///   away;
    /// * a **tombstone** is written for the item's `(provider, external_id)`,
    ///   so importing the same source again does not resurrect it. Without
    ///   that, "delete" would be a lie for anything that came from a list:
    ///   the next `--import` would put it straight back.
    ///
    /// Used by the manager and by `Delete` on a row.
    pub fn delete_item(&mut self, item: &Item) -> Result<(), Box<dyn std::error::Error>> {
        {
            let tx = self.db.begin_write()?;
            {
                let mut table = tx.open_table(COMMANDS)?;
                table.remove(item.id.as_str())?;
            }
            tx.commit()?;
        }

        // No source to remember (a hand-made item that was never tied to a
        // path): nothing to block, and nothing can bring it back anyway.
        if !item.source.provider.is_empty() {
            self.mark_deleted(item)?;
        }

        self.remove_meta(&format!("frec:{}", item.id));
        for (value, _) in self.param_entries(&item.id) {
            self.remove_meta(&param_key(&item.id, &value));
        }
        Ok(())
    }

    /// Remove an item by id without leaving a tombstone (dropping the demo
    /// items on import — those are not "something the user deleted").
    pub fn delete_item_by_id(&self, id: &str) -> Result<(), Box<dyn std::error::Error>> {
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(COMMANDS)?;
            table.remove(id)?;
        }
        tx.commit()?;
        Ok(())
    }

    // ----- tombstones ---------------------------------------------------

    /// A raw meta value. The discovery scan reads its cache in bulk instead
    /// (`scan_cache`), so this is only used for one-off lookups.
    #[allow(dead_code)]
    pub fn meta(&self, key: &str) -> Option<String> {
        self.read_meta(key)
    }

    #[allow(dead_code)]
    pub fn set_meta(&self, key: &str, value: &str) {
        self.write_meta(key, value);
    }

    /// Every `scan:` entry in one read transaction.
    ///
    /// The discovery cache is read wholesale: one transaction for ~200 keys,
    /// instead of one per key (which would be ~200 file-level reads on the UI
    /// thread while a frame is being drawn).
    pub fn scan_cache(&self) -> std::collections::HashMap<String, String> {
        let mut out = std::collections::HashMap::new();
        let mut scan = || -> Option<()> {
            let tx = self.db.begin_read().ok()?;
            let table = tx.open_table(META).ok()?;
            for row in table.range(SCAN_PREFIX..).ok()? {
                let (k, v) = row.ok()?;
                let key = k.value();
                if !key.starts_with(SCAN_PREFIX) {
                    break;
                }
                out.insert(key.to_string(), v.value().to_string());
            }
            Some(())
        };
        let _ = scan();
        out
    }

    /// Write many meta values in one transaction (the scan cache again).
    pub fn set_meta_many(&self, pairs: &[(String, String)]) {
        if pairs.is_empty() {
            return;
        }
        if let Ok(tx) = self.db.begin_write() {
            {
                if let Ok(mut table) = tx.open_table(META) {
                    for (key, value) in pairs {
                        let _ = table.insert(key.as_str(), value.as_str());
                    }
                }
            }
            let _ = tx.commit();
        }
    }

    /// Add items that came from a scan, in **one** transaction.
    ///
    /// Unlike [`Store::upsert_from_source`] this never touches an item that is
    /// already there: the user may have renamed it, changed its keyword, or
    /// edited its command line, and a periodic scan must not undo that.
    ///
    /// Returns `(inserted, existing, tombstoned)`.
    pub fn insert_scanned(
        &mut self,
        items: Vec<Item>,
    ) -> Result<(usize, usize, usize), Box<dyn std::error::Error>> {
        let known: std::collections::HashSet<Source> = self
            .load_items()?
            .into_iter()
            .map(|i| i.source)
            .collect();

        let mut fresh: Vec<Item> = Vec::new();
        let (mut existing, mut tombstoned) = (0usize, 0usize);
        for mut item in items {
            if self.is_deleted(&item.source.provider, &item.source.external_id) {
                tombstoned += 1;
                continue;
            }
            if known.contains(&item.source) {
                existing += 1;
                continue;
            }
            if item.id.is_empty() {
                item.id = format!("{}:{}", item.source.provider, item.source.external_id);
            }
            fresh.push(item);
        }
        if fresh.is_empty() {
            return Ok((0, existing, tombstoned));
        }

        let inserted = fresh.len();
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(COMMANDS)?;
            for item in &fresh {
                table.insert(item.id.as_str(), serde_json::to_string(item)?.as_str())?;
            }
        }
        tx.commit()?;
        Ok((inserted, existing, tombstoned))
    }

    /// Was this source deleted by the user? Imports ask before re-adding.
    pub fn is_deleted(&self, provider: &str, external_id: &str) -> bool {        if provider.is_empty() || external_id.is_empty() {
            return false;
        }
        self.read_meta(&gone_key(provider, external_id)).is_some()
    }

    /// Forget a tombstone. Done when the user adds the same thing again on
    /// purpose: an import must not resurrect what was deleted, but a deliberate
    /// add is a change of mind, not a resurrection.
    pub fn clear_deleted(&self, provider: &str, external_id: &str) {
        if provider.is_empty() || external_id.is_empty() {
            return;
        }
        self.remove_meta(&gone_key(provider, external_id));
    }

    fn mark_deleted(&self, item: &Item) -> Result<(), Box<dyn std::error::Error>> {
        let record = serde_json::json!({
            "title": item.title,
            "at": now_secs(),
        });
        self.write_meta(
            &gone_key(&item.source.provider, &item.source.external_id),
            &record.to_string(),
        );
        Ok(())
    }

    /// Everything the user has deleted, newest first — the raw material for a
    /// future "已删除" list / undo.
    #[allow(dead_code)] // exercised by the tests; a "已删除 / 撤销" view will read it
    pub fn deleted_sources(&self) -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        let mut scan = || -> Option<()> {
            let tx = self.db.begin_read().ok()?;
            let table = tx.open_table(META).ok()?;
            for row in table.range("gone:"..).ok()? {
                let (k, v) = row.ok()?;
                let Some(rest) = k.value().strip_prefix("gone:") else {
                    break;
                };
                let (provider, external_id) = rest.split_once(':').unwrap_or((rest, ""));
                let title = serde_json::from_str::<serde_json::Value>(v.value())
                    .ok()
                    .and_then(|j| j.get("title").and_then(|t| t.as_str()).map(String::from))
                    .unwrap_or_default();
                out.push((provider.to_string(), external_id.to_string(), title));
            }
            Some(())
        };
        let _ = scan();
        out
    }

    // ----- frecency -----------------------------------------------------

    pub fn get_frecency(&self, id: &str) -> Frecency {
        (|| -> Option<Frecency> {
            let tx = self.db.begin_read().ok()?;
            let table = tx.open_table(META).ok()?;
            let v = table.get(format!("frec:{id}").as_str()).ok()??;
            serde_json::from_str(v.value()).ok()
        })()
        .unwrap_or_default()
    }

    /// Record one use of an item (single small transaction).
    pub fn bump_frecency(&self, id: &str) {
        let mut f = self.get_frecency(id);
        f.count = f.count.saturating_add(1);
        f.last_used = now_secs();
        if let Ok(json) = serde_json::to_string(&f) {
            self.write_meta(&format!("frec:{id}"), &json);
        }
    }

    // ----- parameter history -------------------------------------------

    /// Remember one parameter the user typed for `item_id`.
    ///
    /// AltRun kept these in `ParamHistory.txt` next to the exe (rank = use
    /// count, newest first, capped at `ParamHistoryLimit`). Keeping them in the
    /// same meta table means no new file to manage, and the ranking can be
    /// per item instead of one global list — "the wiki page I always open" and
    /// "the search I always run" no longer share slots.
    pub fn bump_param(&self, item_id: &str, value: &str) {
        let value = value.trim();
        // Nothing typed: recording it would put an empty row at the top of the
        // suggestions forever.
        if value.is_empty() {
            return;
        }
        let key = param_key(item_id, value);
        let count = self
            .read_meta(&key)
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0)
            .saturating_add(1);
        self.write_meta(&key, &count.to_string());
        self.trim_param_history(item_id);
    }

    /// The parameters used with one item, `(value, uses)`, most used first.
    /// `limit` is applied after sorting, so it means "the top N".
    pub fn param_history(&self, item_id: &str, limit: usize) -> Vec<(String, u32)> {
        let mut entries = self.param_entries(item_id);
        entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        entries.truncate(limit);
        entries
    }

    /// `(value, count)` for one item, in key order.
    fn param_entries(&self, item_id: &str) -> Vec<(String, u32)> {
        let prefix = format!("param:{item_id}:");
        let mut out: Vec<(String, u32)> = Vec::new();
        let mut scan = || -> Option<()> {
            let tx = self.db.begin_read().ok()?;
            let table = tx.open_table(META).ok()?;
            for row in table.range(prefix.as_str()..).ok()? {
                let (k, v) = row.ok()?;
                // Keys come back sorted, so the first one that does not carry
                // the prefix ends this item's slice.
                let Some(value) = k.value().strip_prefix(prefix.as_str()) else {
                    break;
                };
                out.push((value.to_string(), v.value().parse::<u32>().unwrap_or(0)));
            }
            Some(())
        };
        let _ = scan();
        out
    }

    /// Keep the history bounded (AltRun's limit was 50): drop the least used.
    fn trim_param_history(&self, item_id: &str) {
        let mut entries = self.param_entries(item_id);
        if entries.len() <= PARAM_HISTORY_LIMIT {
            return;
        }
        entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        for (value, _) in entries.into_iter().skip(PARAM_HISTORY_LIMIT) {
            self.remove_meta(&param_key(item_id, &value));
        }
    }

    // ----- config -------------------------------------------------------

    /// Read a config value (meta table, `cfg:` prefix).
    pub fn get_config(&self, key: &str) -> Option<String> {
        self.read_meta(&format!("cfg:{key}"))
    }

    /// Write a config value.
    pub fn set_config(&self, key: &str, value: &str) {
        self.write_meta(&format!("cfg:{key}"), value);
    }

    /// Export every item to a human-readable JSON file next to the db.
    /// Kept for the CRUD screen / debugging — no caller yet.
    #[allow(dead_code)]
    pub fn export_json(&mut self) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let items = self.load_items()?;
        let path = self.data_dir.join("mxrun-export.json");
        fs::write(&path, serde_json::to_string_pretty(&items)?)?;
        Ok(path)
    }
}

/// The demo items created for a fresh database. Replaces v1's `seed_defaults`;
/// keywords are filled in so they are reachable by typing, not only by title.
pub fn seed_items() -> Vec<Item> {
    let mk = |id: &str, title: &str, subtitle: &str, kw: &[&str], action: Action| Item {
        id: id.to_string(),
        title: title.to_string(),
        subtitle: subtitle.to_string(),
        keywords: kw.iter().map(|s| s.to_string()).collect(),
        actions: vec![action],
        arg: ArgSpec::default(),
        launch: LaunchMode::Normal,
        source: Source { provider: "seed".into(), external_id: id.to_string() },
        health: Health::Unknown,
    };
    vec![
        mk("seed-0", "计算器", "Windows 计算器", &["calc"], Action::run("calc.exe")),
        mk("seed-1", "记事本", "Windows 记事本", &["notepad"], Action::run("notepad.exe")),
        mk("seed-2", "命令提示符", "cmd 终端", &["cmd"], Action::run("cmd.exe")),
        mk("seed-3", "文件资源管理器", "Windows Explorer", &["explorer"], Action::run("explorer.exe")),
        mk("seed-4", "Bing", "搜索引擎", &["bing"], Action::open("https://www.bing.com")),
        mk("seed-5", "GitHub", "代码托管平台", &["github"], Action::open("https://github.com")),
    ]
}

/// Frecency bonus: frequency with ~14-day half-life decay.
/// Returns a value roughly in 0..100 to be added on top of the match score.
pub fn frecency_bonus(f: &Frecency) -> f64 {
    if f.count == 0 {
        return 0.0;
    }
    let age_days = now_secs().saturating_sub(f.last_used) as f64 / 86_400.0;
    let decay = 0.5f64.powf(age_days / 14.0);
    (f.count as f64).ln_1p() * 20.0 * decay
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("mxrun-test-{tag}-{nanos}"));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn legacy_json(id: &str, kind: &str, path: &str, name: &str, desc: &str) -> String {
        // Windows paths are full of backslashes: they must be escaped to be
        // valid JSON.
        let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        format!(
            r#"{{"id":"{}","kind":"{}","path":"{}","name":"{}","desc":"{}"}}"#,
            esc(id),
            esc(kind),
            esc(path),
            esc(name),
            esc(desc)
        )
    }

    /// New shape must survive a serde round trip with every field populated.
    #[test]
    fn item_roundtrip_keeps_every_field() {
        let item = Item {
            id: "x1".into(),
            title: "Dos窗口".into(),
            subtitle: "命令行".into(),
            keywords: vec!["cmd".into(), "dos".into()],
            actions: vec![
                Action::run("cmd /k {p}"),
                Action::reveal(r"C:\Windows\System32"),
            ],
            arg: ArgSpec {
                source: ArgSource::Prompt,
                encode: Encoder::Raw,
                insert: InsertMode::Replace,
            },
            launch: LaunchMode::Maximized,
            source: Source { provider: "shortcutlist".into(), external_id: "line:7".into() },
            health: Health::Ok,
        };
        let json = serde_json::to_string(&item).unwrap();
        let back: Item = serde_json::from_str(&json).unwrap();
        assert_eq!(item, back);
        assert_eq!(back.default_action().unwrap().label, "运行");
        assert!(back.wants_input());
        assert_eq!(back.icon_target(), Some("cmd /k {p}"));
    }

    /// A v1 row is turned into an Item — and keeps its id, because frecency is
    /// keyed by id.
    #[test]
    fn legacy_row_migrates_and_keeps_id() {
        let item: Item = serde_json::from_str::<LegacyCommand>(&legacy_json(
            "seed-2", "cmd", "cmd /k {p}", "Dos窗口", "终端",
        ))
        .unwrap()
        .into_item();

        assert_eq!(item.id, "seed-2", "id must survive: frecency is keyed by it");
        assert_eq!(item.title, "Dos窗口");
        assert_eq!(item.subtitle, "终端");
        assert_eq!(item.source.provider, "legacy");
        match &item.default_action().unwrap().effect {
            Effect::Run { line } => assert_eq!(line, "cmd /k {p}"),
            other => panic!("kind=cmd should migrate to Run, got {other:?}"),
        }
        // {p} in a v1 row implies "ask the user, no encoding, replace in place".
        assert_eq!(item.arg.source, ArgSource::Prompt);
        assert_eq!(item.arg.insert, InsertMode::Replace);
    }

    #[test]
    fn legacy_url_opens_and_plain_kind_opens() {
        let url: Item = serde_json::from_str::<LegacyCommand>(&legacy_json(
            "seed-4", "url", "https://github.com", "GitHub", "",
        ))
        .unwrap()
        .into_item();
        assert!(matches!(url.default_action().unwrap().effect, Effect::Open { .. }));
        assert_eq!(url.default_action().unwrap().effect.label(), "网址");

        let dir: Item = serde_json::from_str::<LegacyCommand>(&legacy_json(
            "seed-9", "dir", r"C:\Windows", "Windows", "",
        ))
        .unwrap()
        .into_item();
        assert_eq!(dir.default_action().unwrap().effect.label(), "目录");
    }

    /// Opening a real v1 database migrates it, keeps the rows, and records the
    /// schema version.
    #[test]
    fn open_migrates_v1_database_in_place() {
        let dir = temp_dir("migrate");
        fs::create_dir_all(&dir).unwrap();

        // Build a v1-shaped database by hand: rows + no schema_version key.
        {
            let db = Database::create(dir.join("mxrun.redb")).unwrap();
            let tx = db.begin_write().unwrap();
            {
                let mut t = tx.open_table(COMMANDS).unwrap();
                t.insert("seed-0", legacy_json("seed-0", "cmd", "calc.exe", "计算器", "Windows 计算器").as_str())
                    .unwrap();
                t.insert("u-1", legacy_json("u-1", "file", r"C:\Projects\demo", "demo", "").as_str())
                    .unwrap();
                t.insert("bad", "{not json at all").unwrap();
            }
            tx.commit().unwrap();
        }

        let mut store = Store::open_at(dir.clone()).unwrap();

        // Freqency written for seed-0 must still line up with the same id.
        store.bump_frecency("seed-0");
        assert_eq!(store.get_frecency("seed-0").count, 1);

        let items = store.load_items().unwrap();
        assert_eq!(items.len(), 2, "both readable rows survive");
        assert!(items.iter().any(|i| i.title == "计算器"));
        assert!(items.iter().any(|i| i.title == "demo"));

        // The unreadable row is reported, not silently dropped.
        assert!(
            store.warnings.iter().any(|w| w.contains("bad")),
            "unparseable row must be reported: {:?}",
            store.warnings
        );
        assert_eq!(store.get_config("schema_version"), None, "config lives under cfg:");
        assert_eq!(
            store.read_meta(SCHEMA_VERSION_KEY).as_deref(),
            Some("2"),
            "migration must stamp the schema version"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Deleting an item takes its statistics with it, and remembers the source
    /// so a later import cannot quietly put it back.
    #[test]
    fn delete_removes_stats_and_leaves_a_tombstone() {
        let dir = temp_dir("delete");
        let mut store = Store::open_at(dir.clone()).unwrap();

        let mk = |title: &str| Item {
            id: String::new(),
            title: title.into(),
            keywords: vec!["steam".into()],
            actions: vec![Action::run("steam.exe")],
            source: Source { provider: "shortcutlist".into(), external_id: "line:43".into() },
            ..Default::default()
        };
        let id = store.upsert_from_source(mk("Steam")).unwrap().expect("first add");
        store.bump_frecency(&id);
        store.bump_frecency(&id);
        store.bump_param(&id, "rust");
        assert_eq!(store.get_frecency(&id).count, 2);
        assert_eq!(store.param_history(&id, 10).len(), 1);

        let item = store
            .load_items()
            .unwrap()
            .into_iter()
            .find(|i| i.id == id)
            .expect("item is there");
        store.delete_item(&item).unwrap();

        // Gone from the list…
        assert!(store.load_items().unwrap().iter().all(|i| i.id != id));
        // …and its bookkeeping is gone with it, so nothing inherits the counts.
        assert_eq!(store.get_frecency(&id).count, 0);
        assert!(store.param_history(&id, 10).is_empty());

        // The tombstone keeps the next import from resurrecting it…
        assert!(store.is_deleted("shortcutlist", "line:43"));
        assert_eq!(store.upsert_from_source(mk("Steam")).unwrap(), None, "not re-added");
        assert!(store.load_items().unwrap().iter().all(|i| i.title != "Steam"));

        // The deleted list is available for a future "undo".
        let deleted = store.deleted_sources();
        assert_eq!(deleted.len(), 1);
        assert_eq!(deleted[0].0, "shortcutlist");
        assert_eq!(deleted[0].1, "line:43");
        assert_eq!(deleted[0].2, "Steam", "title recorded");

        // …until the user adds the same thing on purpose, which forgets it.
        store.clear_deleted("shortcutlist", "line:43");
        assert!(!store.is_deleted("shortcutlist", "line:43"));
        assert!(store.upsert_from_source(mk("Steam")).unwrap().is_some());
        assert!(store.deleted_sources().is_empty(), "tombstone forgotten");

        let _ = fs::remove_dir_all(&dir);
    }

    /// Dropping the demo items is not a user deletion — nothing is remembered,
    /// so a fresh database still gets its demo rows.
    #[test]
    fn dropping_demo_items_leaves_no_tombstone() {
        let dir = temp_dir("demo-drop");
        let mut store = Store::open_at(dir.clone()).unwrap();
        let demo = store.load_items().unwrap();
        assert!(!demo.is_empty());
        for item in &demo {
            store.delete_item_by_id(&item.id).unwrap();
        }
        assert!(store.deleted_sources().is_empty());
        assert!(!store.is_deleted("seed", "seed-0"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// Re-opening an already migrated database must not re-seed or duplicate.
    #[test]
    fn reopening_is_idempotent() {        let dir = temp_dir("reopen");
        let mut store = Store::open_at(dir.clone()).unwrap();
        let first = store.load_items().unwrap().len();
        assert_eq!(first, seed_items().len(), "fresh db gets the demo items");
        drop(store);

        let mut store = Store::open_at(dir.clone()).unwrap();
        assert_eq!(store.load_items().unwrap().len(), first, "no duplicates");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Re-importing from a provider updates in place instead of duplicating.
    #[test]
    fn upsert_from_source_is_idempotent_and_keeps_id() {
        let dir = temp_dir("upsert");
        let mut store = Store::open_at(dir.clone()).unwrap();

        let mk = |title: &str| Item {
            id: "whatever".into(),
            title: title.into(),
            keywords: vec!["steam".into()],
            actions: vec![Action::run(r"C:\Program Files\Steam\Steam.exe -tcp")],
            source: Source { provider: "shortcutlist".into(), external_id: "line:43".into() },
            ..Default::default()
        };

        let id1 = store.upsert_from_source(mk("Steam")).unwrap().expect("not deleted");
        store.bump_frecency(&id1);
        let id2 = store.upsert_from_source(mk("Steam 客户端")).unwrap().expect("not deleted");

        assert_eq!(id1, id2, "same source -> same id, so frecency survives");
        assert_eq!(store.get_frecency(&id1).count, 1);
        let items = store.load_items().unwrap();
        let steam: Vec<_> = items.iter().filter(|i| i.source.provider == "shortcutlist").collect();
        assert_eq!(steam.len(), 1, "no duplicate row");
        assert_eq!(steam[0].title, "Steam 客户端", "second import wins");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Parameter history: per item, most used first, blanks ignored.
    #[test]
    fn param_history_ranks_per_item() {
        let dir = temp_dir("param-history");
        let store = Store::open_at(dir.clone()).unwrap();

        // Only the values matter here; the counts are asserted separately.
        let values = |item: &str, limit: usize| -> Vec<String> {
            store.param_history(item, limit).into_iter().map(|(v, _)| v).collect()
        };

        store.bump_param("a", "rust");
        store.bump_param("a", "rust");
        store.bump_param("a", "egui");
        store.bump_param("b", "另一个条目的参数");

        assert_eq!(values("a", 10), vec!["rust", "egui"]);
        assert_eq!(values("b", 10), vec!["另一个条目的参数"]);
        assert!(values("c", 10).is_empty(), "unknown item: no history");
        assert_eq!(values("a", 1), vec!["rust"], "limit = top N");
        assert_eq!(
            store.param_history("a", 10)[0].1,
            2,
            "the use count is kept, not just the value"
        );

        // Whitespace-only input is not a parameter.
        store.bump_param("a", "   ");
        assert_eq!(values("a", 10).len(), 2);

        // Values are recorded as typed, spaces and all (they get encoded later).
        store.bump_param("a", "两 个 词");
        assert!(values("a", 10).contains(&"两 个 词".to_string()));

        let _ = fs::remove_dir_all(&dir);
    }

    /// The history stays bounded, and it drops the least used first.
    #[test]
    fn param_history_is_capped_by_least_used() {        let dir = temp_dir("param-cap");
        let store = Store::open_at(dir.clone()).unwrap();

        store.bump_param("a", "keep");
        store.bump_param("a", "keep");
        for i in 0..(PARAM_HISTORY_LIMIT + 5) {
            store.bump_param("a", &format!("p{i:03}"));
        }

        let hist = store.param_history("a", PARAM_HISTORY_LIMIT * 2);
        assert_eq!(hist.len(), PARAM_HISTORY_LIMIT, "cap enforced");
        assert_eq!(hist[0].0, "keep", "the most used one is not evicted");
        assert_eq!(hist[0].1, 2, "and it kept its count");

        // Non-evicted entries are still readable, and "keep" appears once.
        let top = store.param_history("a", 10);
        assert_eq!(top.len(), 10);
        assert_eq!(
            store.param_history("a", PARAM_HISTORY_LIMIT).iter().filter(|(v, _)| v == "keep").count(),
            1
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// Portable mode: a `portable.txt` next to the exe moves everything into
    /// `data/` there; without it the per-user `%APPDATA%\MxRun` is used.
    #[test]
    fn portable_marker_moves_the_data_next_to_the_exe() {
        let dir = temp_dir("portable");
        fs::create_dir_all(&dir).unwrap();

        // No marker: the per-user roaming profile wins. (A made-up drive letter
        // keeps this from looking like anybody's real profile path — the
        // pre-commit hook rightly refuses to see those in the repository.)
        assert_eq!(
            data_dir_for(Some(&dir), Some(std::ffi::OsStr::new(r"R:\roaming"))),
            PathBuf::from(r"R:\roaming").join("MxRun")
        );

        // Marker present: data lives beside the exe…
        fs::write(dir.join(PORTABLE_MARKER), b"").unwrap();
        assert_eq!(data_dir_for(Some(&dir), Some(std::ffi::OsStr::new(r"R:\roaming"))), dir.join("data"));

        // …even when %APPDATA% is not set at all (a stripped-down environment).
        assert_eq!(data_dir_for(Some(&dir), None), dir.join("data"));

        // No exe directory to speak of (can't happen for a real process, but
        // the fallback must not panic).
        assert_eq!(
            data_dir_for(None, Some(std::ffi::OsStr::new(r"R:\roaming"))),
            PathBuf::from(r"R:\roaming").join("MxRun")
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// Manual-check helper (not part of the normal run):
    ///
    /// ```text
    /// cargo test -- --ignored --nocapture seed_profile_for_manual_check
    /// APPDATA=<printed dir>  target\release\mxrun.exe
    /// ```
    ///
    /// Keyboard interaction cannot be automated (CLAUDE.md pitfall list), so
    /// this prepares a throwaway profile holding AltRun-shaped items that
    /// exercise every P0-2 execution path, and prints where it is.
    #[test]
    #[ignore = "manual check helper: seeds a profile to try the executor by hand"]
    fn seed_profile_for_manual_check() {
        let dir = std::env::temp_dir().join("mxrun-p02-profile").join("MxRun");
        let _ = fs::remove_dir_all(dir.parent().unwrap());
        let mut store = Store::open_at(dir.clone()).unwrap();

        let mk = |title: &str,
                  kw: &str,
                  effect: crate::store::Effect,
                  arg: ArgSpec,
                  provider: &str,
                  ext: &str| Item {
            id: String::new(),
            title: title.into(),
            subtitle: String::new(),
            keywords: vec![kw.into()],
            actions: vec![Action { label: "默认".into(), effect }],
            arg,
            launch: LaunchMode::Normal,
            source: Source { provider: provider.into(), external_id: ext.into() },
            health: Health::Unknown,
        };
        let none = ArgSpec::default();
        let items = vec![
            // Env expansion + a system folder.
            mk("Windows 目录", "windir", crate::store::Effect::Open { target: "%WINDIR%".into() }, none, "manual", "m1"),
            // A whole command line, no argument.
            mk("我的IP地址", "myip", crate::store::Effect::Run { line: "nslookup".into() }, none, "manual", "m2"),
            // Argument required -> must show `*` and refuse to run.
            mk(
                "需要输入的示例",
                "ask",
                crate::store::Effect::Run { line: "cmd /k {p}".into() },
                ArgSpec { source: ArgSource::Prompt, encode: Encoder::Raw, insert: InsertMode::Replace },
                "manual",
                "m3",
            ),
            // Clipboard argument, no typing: baidu search of the clipboard.
            mk(
                "百度 搜索剪贴板",
                "cb",
                crate::store::Effect::Open { target: "http://www.baidu.com/s?wd={%c}".into() },
                ArgSpec { source: ArgSource::Clipboard, encode: Encoder::Utf8Percent, insert: InsertMode::Replace },
                "manual",
                "m4",
            ),
            // Builtin verbs: window control and show-desktop.
            mk("隐藏当前窗口", "hide", crate::store::Effect::Builtin { verb: BuiltinVerb::HideForegroundWindow }, none, "manual", "m5"),
            mk("恢复窗口", "unhide", crate::store::Effect::Builtin { verb: BuiltinVerb::ShowForegroundWindow }, none, "manual", "m8"),
            mk("显示桌面", "desk", crate::store::Effect::Builtin { verb: BuiltinVerb::MinimizeAll }, none, "manual", "m6"),
            // App with arguments (the shape that used to fail silently).
            mk(
                "记事本(带参数)",
                "npp",
                crate::store::Effect::Run { line: "notepad.exe".into() },
                none,
                "manual",
                "m7",
            ),
            // P1-1 end to end: the typed parameter must reach a real process.
            // It writes what it got to a temp file — that file is the evidence,
            // and `MXRUN_SELFTEST_PARAM` can drive the whole thing with no
            // keyboard. Hidden keeps the console from flashing.
            {
                let mut it = mk(
                    "参数写入文件",
                    "p11",
                    crate::store::Effect::Run {
                        line: r#"cmd /c echo {p} > "%TEMP%\mxrun-p11-param.txt""#.into(),
                    },
                    ArgSpec {
                        source: ArgSource::Prompt,
                        encode: Encoder::Raw,
                        insert: InsertMode::Replace,
                    },
                    "manual",
                    "m9",
                );
                it.launch = LaunchMode::Hidden;
                it
            },
        ];
        for item in items {
            store.upsert_from_source(item).unwrap();
        }
        println!("\n样本档案已就绪：{}", dir.display());
        println!("启动：  $env:APPDATA='{}'; .\\target\\release\\mxrun.exe", dir.parent().unwrap().display());
        println!("可试：windir / myip / ask(回车应变出参数输入框) / cb / hide→unhide / desk / npp");
        println!("      p11 输入任意文字 → 写入 %TEMP%\\mxrun-p11-param.txt（可无键盘验收）\n");
    }
}

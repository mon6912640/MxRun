//! File search for `@` mode: Everything's `es.exe` behind a small, testable API,
//! with anything's `ae.exe` as the fallback when `es.exe` is missing or fails.
//!
//! `ae.exe` (anything's CLI, measured 2026-10-20) speaks the same query
//! language for everything `@` mode sends — one argv per term, `!term`
//! negation, a term containing `\` matches the full path — with three
//! differences the argument builders account for:
//!
//! * it writes **UTF-8** when piped (`decode_console` tries UTF-8 first);
//! * `--` ends the switches, so a `-`-leading word needs no regex detour;
//! * its complaints go to **stderr** as `ae: …`, exit 1 (usage) or 2 (no index).
//!
//! Measured facts about `es.exe` (2026-09-19, `docs/开发进度.md` §2.31):
//!
//! * `es.exe` writes **GBK** (codepage 936) to its stdout — `b2 e2 ca d4` is
//!   "测试" — and `-utf8-bom` is accepted but **ignored** by 1.1.0.30.
//! * A query that starts with `-` is read as a switch ("Error 6: Unknown
//!   switch.", exit 6), so those go through `-r` (regex) with the pattern
//!   escaped, which takes the text as an argument.
//! * **No match is exit code 0** with empty output, so the exit code only tells
//!   errors apart from results.
//! * A warm query costs ~38-60 ms, so the UI debounces and drops stale answers
//!   rather than cancelling (a fresh process cannot be cancelled mid-flight).
//!
//! The module deliberately does no UI and no database work: it turns a query
//! into paths, filters the noise, and ranks what is left.

use std::path::{Path, PathBuf};

/// One search result. `modified` is Unix seconds (`0` when unavailable).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileHit {
    pub path: String,
    pub is_dir: bool,
    pub modified: u64,
}

/// Folders nobody means to launch: build trees, caches, OS internals.
///
/// The user asked for these to be excluded by default **and editable in
/// settings** (2026-09-19), so this is only the starting list; the live list
/// comes from the config (`file_exclude`).
pub const DEFAULT_EXCLUSIONS: &[&str] = &[
    r"\node_modules\",
    r"\.git\",
    r"\.svn\",
    r"\__pycache__\",
    r"\.venv\",
    r"\.cache\",
    r"\appdata\local\temp\",
    r"\appdata\local\packages\",
    r"\windows\prefetch\",
    r"\windows\winsxs\",
    r"\windows\installer\",
    r"\windows\softwaredistribution\",
    r"\$recycle.bin\",
    r"\system volume information\",
    r"\.thumbnails\",
];

/// How much bigger than the display cap the candidate set is. Noise filtering
/// and re-ranking both throw rows away, so asking for only `limit` would leave
/// the list short for no reason.
const OVERFETCH: usize = 4;

/// Hard cap on what we ask Everything for, so a huge display cap cannot ask for
/// a million rows.
const MAX_FETCH: usize = 400;

/// Give up on `es.exe` after this long (Everything not running, a huge index,
/// an instance that never answers).
const TIMEOUT_MS: u64 = 2_000;

/// `ae.exe` gets a little longer: with anything not running it loads the saved
/// index file itself before answering.
const AE_TIMEOUT_MS: u64 = 3_000;

/// After `es.exe` fails, ask `ae.exe` first for this long. Without it a hung
/// Everything would cost the full [`TIMEOUT_MS`] on **every** keystroke before
/// the fallback got its turn.
const ES_COOLDOWN_MS: u64 = 30_000;

/// Which command-line search tool answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    /// Everything's `es.exe` — the preferred one.
    Es,
    /// anything's `ae.exe` — the fallback.
    Ae,
}

impl Engine {
    pub fn exe_name(self) -> &'static str {
        match self {
            Engine::Es => "es.exe",
            Engine::Ae => "ae.exe",
        }
    }

    fn timeout_ms(self) -> u64 {
        match self {
            Engine::Es => TIMEOUT_MS,
            Engine::Ae => AE_TIMEOUT_MS,
        }
    }

    /// The program behind the CLI, for "is it running?" hints.
    fn app_name(self) -> &'static str {
        match self {
            Engine::Es => "Everything",
            Engine::Ae => "anything",
        }
    }
}

/// The search CLIs found on this machine. `es.exe` is asked first; `ae.exe`
/// answers when `es.exe` is missing or fails (user request, 2026-10-20).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchTools {
    pub es: Option<PathBuf>,
    pub ae: Option<PathBuf>,
}

impl SearchTools {
    /// Discover both tools; `es_cfg` / `ae_cfg` are the paths set in settings.
    pub fn discover(es_cfg: Option<&str>, ae_cfg: Option<&str>) -> Self {
        SearchTools {
            es: discover_engine(Engine::Es, es_cfg),
            ae: discover_engine(Engine::Ae, ae_cfg),
        }
    }

    /// Is there anything at all to search with?
    pub fn any(&self) -> bool {
        self.es.is_some() || self.ae.is_some()
    }

    pub fn path(&self, engine: Engine) -> Option<&PathBuf> {
        match engine {
            Engine::Es => self.es.as_ref(),
            Engine::Ae => self.ae.as_ref(),
        }
    }

    pub fn set(&mut self, engine: Engine, path: Option<PathBuf>) {
        match engine {
            Engine::Es => self.es = path,
            Engine::Ae => self.ae = path,
        }
    }
}

/// Discover one tool from its configured path.
pub fn discover_engine(engine: Engine, configured: Option<&str>) -> Option<PathBuf> {
    match engine {
        Engine::Es => discover_es(configured),
        Engine::Ae => discover_ae(configured),
    }
}

/// Find `es.exe`: the configured path first, then `PATH`, then the usual spots.
///
/// Pure filesystem work; returns `None` when Everything's CLI is not installed,
/// which the UI reports as "装/指定 es.exe" rather than showing an empty list.
pub fn discover_es(configured: Option<&str>) -> Option<PathBuf> {
    let mut guesses: Vec<PathBuf> = Vec::new();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let base = PathBuf::from(local);
        guesses.push(base.join("bin").join("es.exe"));
        guesses.push(base.join("Programs").join("es").join("es.exe"));
    }
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        let base = PathBuf::from(profile);
        guesses.push(base.join(".local").join("bin").join("es.exe"));
        guesses.push(base.join("scoop").join("shims").join("es.exe"));
    }
    for cand in [
        r"C:\Program Files\Everything\es.exe",
        r"C:\Program Files (x86)\Everything\es.exe",
    ] {
        guesses.push(PathBuf::from(cand));
    }
    discover_tool("es.exe", "MXRUN_ES_PATH", configured, guesses)
}

/// Find `ae.exe`: the configured path, `PATH`, next to MxRun itself, the
/// usual spots — and last, next to a **running** `anything.exe`, because
/// anything is a portable app that rarely lives anywhere predictable, and ae.exe
/// ships in the same folder.
pub fn discover_ae(configured: Option<&str>) -> Option<PathBuf> {
    let mut guesses: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        guesses.push(dir.join("ae.exe"));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let base = PathBuf::from(local);
        guesses.push(base.join("bin").join("ae.exe"));
        guesses.push(base.join("Programs").join("anything").join("ae.exe"));
    }
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        let base = PathBuf::from(profile);
        guesses.push(base.join(".local").join("bin").join("ae.exe"));
        guesses.push(base.join("scoop").join("shims").join("ae.exe"));
    }
    discover_tool("ae.exe", "MXRUN_AE_PATH", configured, guesses).or_else(|| {
        // An env override that names nothing means "no ae.exe", on purpose.
        if std::env::var_os("MXRUN_AE_PATH").is_some() {
            return None;
        }
        running_exe_dir("anything.exe")
            .map(|dir| dir.join("ae.exe"))
            .filter(|p| p.is_file())
    })
}

/// The shared lookup order: env override, configured path (file or folder),
/// `PATH`, then `guesses`.
fn discover_tool(exe: &str, env_var: &str, configured: Option<&str>, guesses: Vec<PathBuf>) -> Option<PathBuf> {
    // A test/power-user override, in the spirit of the other `MXRUN_*` switches.
    if let Some(from_env) = std::env::var_os(env_var) {
        let path = PathBuf::from(from_env);
        // Deliberately *not* falling through to auto-detection: the point of the
        // override is to be able to say "there is no es.exe here" and watch the
        // error path behave.
        return path.is_file().then_some(path);
    }
    if let Some(p) = configured.map(str::trim).filter(|p| !p.is_empty()) {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
        // Also accept a directory, because that is what people copy out of a
        // folder window.
        let inside = path.join(exe);
        if inside.is_file() {
            return Some(inside);
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let cand = dir.join(exe);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    guesses.into_iter().find(|p| p.is_file())
}

/// Folder of the first running process whose image is named `name`.
///
/// One Toolhelp snapshot (a few ms), only reached when every cheaper lookup
/// came up empty.
fn running_exe_dir(name: &str) -> Option<PathBuf> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };
    use windows::core::PWSTR;
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = None;
        let mut more = Process32FirstW(snap, &mut entry).is_ok();
        while more {
            let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            let exe = String::from_utf16_lossy(&entry.szExeFile[..len]);
            if exe.eq_ignore_ascii_case(name) {
                if let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, entry.th32ProcessID) {
                    let mut buf = [0u16; 1024];
                    let mut size = buf.len() as u32;
                    let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut size).is_ok();
                    let _ = CloseHandle(process);
                    if ok {
                        let image = PathBuf::from(String::from_utf16_lossy(&buf[..size as usize]));
                        found = image.parent().map(Path::to_path_buf);
                        if found.is_some() {
                            break;
                        }
                    }
                }
            }
            more = Process32NextW(snap, &mut entry).is_ok();
        }
        let _ = CloseHandle(snap);
        found
    }
}

/// Decode what a console program wrote.
///
/// `es.exe` emits the ANSI codepage; a future build might emit UTF-8. Trying
/// UTF-8 first is free and correct in that case, and the Windows conversion
/// (no new dependency) handles today's GBK output.
pub fn decode_console(bytes: &[u8]) -> String {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_string();
    }
    for cp in [windows_cp_ansi(), windows_cp_oem()] {
        if let Some(text) = decode_with_codepage(bytes, cp) {
            return text;
        }
    }
    String::from_utf8_lossy(bytes).into_owned()
}

fn windows_cp_ansi() -> u32 {
    use windows::Win32::Globalization::CP_ACP;
    CP_ACP
}

fn windows_cp_oem() -> u32 {
    use windows::Win32::Globalization::CP_OEMCP;
    CP_OEMCP
}

fn decode_with_codepage(bytes: &[u8], codepage: u32) -> Option<String> {
    use windows::Win32::Globalization::{MULTI_BYTE_TO_WIDE_CHAR_FLAGS, MultiByteToWideChar};
    if bytes.is_empty() {
        return Some(String::new());
    }
    unsafe {
        let needed = MultiByteToWideChar(codepage, MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0), bytes, None);
        if needed <= 0 {
            return None;
        }
        let mut wide = vec![0u16; needed as usize];
        let wrote = MultiByteToWideChar(codepage, MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0), bytes, Some(&mut wide));
        if wrote <= 0 {
            return None;
        }
        wide.truncate(wrote as usize);
        Some(String::from_utf16_lossy(&wide))
    }
}

/// Everything-side negation terms for the configured exclusions.
///
/// Two rules come from driving `es.exe` by hand (2026-10-17):
///
/// * each term must be its **own argv** — one argument containing spaces is taken
///   as a single literal phrase (`es "temp !x"` returns 0 rows, while
///   `es temp "!x"` filters properly);
/// * a term that itself contains a space kills the whole query
///   (`!\system volume information\` → 0 rows even next to other negations), so
///   those fragments are left to the client-side filter.
///
/// Regex mode (`-r`, used for `-`-leading queries) cannot carry `!term` at all.
pub fn query_exclusions(exclusions: &[String], on: bool) -> Vec<String> {
    if !on {
        return Vec::new();
    }
    exclusions
        .iter()
        .map(|e| e.trim())
        .filter(|e| !e.is_empty() && !e.contains(' '))
        .map(|e| format!("!{e}"))
        .collect()
}

/// Split a query into the search terms `es.exe` wants.
///
/// `es.exe` takes the search text as **several arguments** and reads a *single*
/// argument containing spaces as one literal phrase — measured 2026-10-17:
///
/// ```text
/// es "report template"   -> 0 rows                     (what Command::arg sends)
/// es  report template    -> the rows one would expect
/// ```
///
/// Rust's `Command::arg` always quotes an argument that contains spaces, so
/// passing the query as one string silently broke **every multi-word search** in
/// `@` mode. Quotes the user types keep a group together (Everything's phrase
/// syntax); unbalanced quotes are forgiven.
pub fn split_query_terms(query: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for ch in query.trim().chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                if !quoted && !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Is this path something the user almost certainly did not mean?
///
/// Matching is case-insensitive substring work on the whole path, because the
/// exclusions are path fragments (`\node_modules\`) rather than names.
pub fn is_noise(path: &str, exclusions: &[String]) -> bool {
    if exclusions.is_empty() {
        return false;
    }
    let lower = path.to_lowercase();
    exclusions
        .iter()
        .map(|e| e.trim().to_lowercase())
        .filter(|e| !e.is_empty())
        .any(|e| lower.contains(&e))
}

/// Parse `es.exe` output into hits.
///
/// Lines are full paths, `\r\n` separated. A trailing separator means a folder
/// (Everything reports folders with one), and a path that still exists is
/// stat'ed once for its modification time — that is what makes "newest first"
/// possible without trusting Everything's own ordering.
pub fn parse_hits(output: &str) -> Vec<FileHit> {
    let mut out = Vec::new();
    for line in output.lines() {
        let raw = line.trim_end_matches(['\r', '\n']).trim();
        if raw.is_empty() {
            continue;
        }
        let path = Path::new(raw);
        if !path.exists() {
            continue; // deleted between the index and the query
        }
        out.push(FileHit {
            path: raw.to_string(),
            is_dir: path.is_dir(),
            modified: modified_secs(path),
        });
    }
    out
}

fn modified_secs(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Rank hits the way the design note promised: name match quality first, then
/// shallower paths, then newer files, then ones this launcher has opened before.
///
/// `opened` maps a path to how often MxRun opened it (`frec:file:` counts).
pub fn rank(hits: Vec<FileHit>, query: &str, opened: &std::collections::HashMap<String, u32>) -> Vec<FileHit> {
    let q = query.trim().to_lowercase();
    let mut scored: Vec<(f64, FileHit)> = hits
        .into_iter()
        .map(|hit| {
            let score = hit_score(&hit, &q, opened);
            (score, hit)
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| a.1.path.to_lowercase().cmp(&b.1.path.to_lowercase()))
    });
    scored.into_iter().map(|(_, hit)| hit).collect()
}

fn hit_score(hit: &FileHit, query: &str, opened: &std::collections::HashMap<String, u32>) -> f64 {
    let name = Path::new(&hit.path)
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let mut score = if query.is_empty() {
        0.0
    } else if name.starts_with(query) {
        1000.0
    } else if name
        .split([' ', '-', '_', '.', '(', '['])
        .any(|word| word.starts_with(query))
    {
        600.0
    } else if name.contains(query) {
        300.0
    } else {
        // Matched somewhere in the path (Everything searched the name, but a
        // regex or `path:` query can land here).
        100.0
    };
    // Shallower is better: 8 points per level, floored so a deep path is not
    // punished into oblivion.
    let depth = hit.path.matches('\\').count() as f64;
    score += (60.0 - depth * 8.0).max(0.0);
    // Newer is better, on a 30-day ramp (100 → 0).
    if hit.modified > 0 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let age_days = now.saturating_sub(hit.modified) as f64 / 86_400.0;
        score += (100.0 - age_days * (100.0 / 30.0)).max(0.0);
    }
    // Files opened from MxRun before: a strong, personal signal.
    if let Some(count) = opened.get(&hit.path) {
        score += 250.0 + (*count as f64).min(20.0) * 25.0;
    }
    score
}

/// Unix milliseconds until which `es.exe` waits behind `ae.exe` (see
/// [`ES_COOLDOWN_MS`]). `0` = no cooldown.
static ES_COOLDOWN_UNTIL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The order to ask the tools in: `es.exe` first, unless it failed recently
/// and `ae.exe` is there to stand in for it.
fn engine_order(tools: &SearchTools, es_cooling: bool) -> Vec<Engine> {
    let mut order: Vec<Engine> = [Engine::Es, Engine::Ae]
        .into_iter()
        .filter(|e| tools.path(*e).is_some())
        .collect();
    if es_cooling && order.len() == 2 {
        order.reverse();
    }
    order
}

/// Command-line arguments for `es.exe`.
fn es_args(query: &str, terms: &[String], fetch: usize, exclusions: &[String], exclusions_on: bool) -> Vec<String> {
    let mut args: Vec<String> = vec!["-n".into(), fetch.to_string()];
    // Newest first as the *candidate* order: we re-rank anyway, and recency is
    // the better prior when a query matches thousands of files.
    args.extend(["-sort".into(), "date-modified".into(), "-sort-descending".into()]);
    if terms.iter().any(|t| t.starts_with('-')) {
        // A dash-leading word would be read as a switch ("Error 6: Unknown
        // switch"), so take the regex route, where the text is the switch's
        // argument. Regex mode cannot carry `!term` exclusions or AND terms —
        // the client-side filter still applies.
        args.push("-r".into());
        args.push(regex_escape(query));
    } else {
        // One argv per term: see `split_query_terms`.
        args.extend(terms.iter().cloned());
        // Push the noise filter into Everything as well, so the row limit applies
        // to **clean** results. Client-side filtering alone is too late: for
        // `temp` the newest 56 matches were all `$RECYCLE.BIN\*.temp-archive`,
        // every one of them was dropped afterwards, and the list came back empty
        // while Everything held thousands of usable rows (user report, 2026-10-17).
        args.extend(query_exclusions(exclusions, exclusions_on));
    }
    args
}

/// Command-line arguments for `ae.exe`. `--` ends the switches, so dashed
/// words are plain terms and the exclusions always apply.
fn ae_args(terms: &[String], fetch: usize, exclusions: &[String], exclusions_on: bool) -> Vec<String> {
    let mut args: Vec<String> = vec!["-n".into(), fetch.to_string(), "-sort-date".into(), "-desc".into(), "--".into()];
    args.extend(terms.iter().cloned());
    args.extend(query_exclusions(exclusions, exclusions_on));
    args
}

/// What a finished tool run printed.
struct ToolOutput {
    stdout: String,
    stderr: String,
}

/// Run one search CLI with a timeout. `Err` is a human-readable reason.
fn run_tool(engine: Engine, exe: &Path, args: &[String]) -> Result<ToolOutput, String> {
    let name = engine.exe_name();
    let mut cmd = std::process::Command::new(exe);
    // **No console at all.** `es.exe` is a console-subsystem binary (PE
    // subsystem 3, verified), so a plain spawn allocates a console: invisible with
    // `CREATE_NO_WINDOW`, but still there. A user reported "every keystroke in `@`
    // mode plays a sound" (2026-09-19) and, although the three-way ear test
    // (`tools/probe-es-sound.ps1`) could not reproduce it with *any* flag, the
    // console is the only `@`-specific side effect we control — so it goes.
    // `DETACHED_PROCESS` gives the child no console at all; the pipes we pass
    // explicitly keep stdout working. `ae.exe` is a console binary too.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(DETACHED_PROCESS);
    }
    cmd.args(args);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.stdin(std::process::Stdio::null());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("无法启动 {name}（{}）：{e}", exe.display()))?;

    // Read both pipes on their own threads: a child that fills a buffer while we
    // poll for the timeout would otherwise deadlock.
    fn drain<R: std::io::Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut out) = pipe {
                let _ = out.read_to_end(&mut buf);
            }
            buf
        })
    }
    let out_reader = drain(child.stdout.take());
    let err_reader = drain(child.stderr.take());

    let timeout = engine.timeout_ms();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = out_reader.join();
                    let _ = err_reader.join();
                    return Err(format!("{name} 超时（{timeout}ms）——{} 是否在运行？", engine.app_name()));
                }
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
            Err(e) => {
                let _ = out_reader.join();
                let _ = err_reader.join();
                return Err(format!("等待 {name} 失败：{e}"));
            }
        }
    };

    let stdout = decode_console(&out_reader.join().unwrap_or_default());
    let stderr = decode_console(&err_reader.join().unwrap_or_default());
    let code = status.code().unwrap_or(-1);
    if code != 0 {
        return Err(format!("{name} 返回 {code}：{}", failure_reason(engine, &stdout, &stderr)));
    }
    Ok(ToolOutput { stdout, stderr })
}

/// The line that explains a failed run: `es.exe` prints its complaints on
/// stdout (`Error 8: …`), `ae.exe` on stderr (`ae: …`).
fn failure_reason(engine: Engine, stdout: &str, stderr: &str) -> String {
    let line = match engine {
        Engine::Es => stdout.lines().find(|l| l.contains("Error")),
        Engine::Ae => stderr
            .lines()
            .map(|l| l.trim())
            .find(|l| !l.is_empty())
            .map(|l| l.strip_prefix("ae:").unwrap_or(l)),
    };
    line.map(str::trim)
        .filter(|l| !l.is_empty())
        .unwrap_or("(无说明)")
        .to_string()
}

/// Ask Everything (or anything). Blocking; the caller runs it off the UI thread.
///
/// `es.exe` goes first; when it is missing or fails, `ae.exe` answers. Returns
/// the hits plus which tool produced them, or `Err` with a human-readable
/// reason when no tool is there, or every one failed or took too long — the UI
/// shows that instead of an empty list, because "nothing found" and "the search
/// is broken" must not look alike.
pub fn search(
    tools: &SearchTools,
    query: &str,
    limit: usize,
    exclusions: &[String],
    exclusions_on: bool,
    opened: &std::collections::HashMap<String, u32>,
) -> Result<(Vec<FileHit>, Engine), String> {
    let query = query.trim();
    if query.is_empty() {
        // Empty query: Everything would list the whole index. The UI shows
        // "recently opened" instead, so nothing to do here.
        return Ok((Vec::new(), Engine::Es));
    }
    let terms = split_query_terms(query);
    if terms.is_empty() {
        return Ok((Vec::new(), Engine::Es));
    }
    let fetch = limit.saturating_mul(OVERFETCH).clamp(1, MAX_FETCH);

    let es_cooling = ES_COOLDOWN_UNTIL.load(std::sync::atomic::Ordering::Relaxed) > now_ms();
    let order = engine_order(tools, es_cooling);
    if order.is_empty() {
        return Err("找不到 es.exe 或 ae.exe".to_string());
    }
    let mut errors: Vec<String> = Vec::new();
    for engine in order {
        let Some(exe) = tools.path(engine) else { continue };
        let args = match engine {
            Engine::Es => es_args(query, &terms, fetch, exclusions, exclusions_on),
            Engine::Ae => ae_args(&terms, fetch, exclusions, exclusions_on),
        };
        match run_tool(engine, exe, &args) {
            Ok(out) => {
                if engine == Engine::Es {
                    ES_COOLDOWN_UNTIL.store(0, std::sync::atomic::Ordering::Relaxed);
                }
                let hits = parse_hits(&out.stdout);
                let filtered: Vec<FileHit> = if exclusions_on {
                    hits.into_iter()
                        .filter(|h| !is_noise(&h.path, exclusions))
                        .collect()
                } else {
                    hits
                };
                let ranked = rank(filtered, query, opened).into_iter().take(limit).collect();
                return Ok((ranked, engine));
            }
            Err(why) => {
                if engine == Engine::Es {
                    ES_COOLDOWN_UNTIL.store(now_ms() + ES_COOLDOWN_MS, std::sync::atomic::Ordering::Relaxed);
                }
                errors.push(why);
            }
        }
    }
    Err(errors.join("；"))
}

/// Check that a tool really answers a search, for the settings page's
/// 测试 button: "the file exists" is not "the search works" (Everything not
/// running is the classic case). `Ok` carries a one-line verdict.
pub fn probe(engine: Engine, exe: &Path) -> Result<String, String> {
    let args: Vec<String> = match engine {
        Engine::Es => vec!["-n".into(), "1".into(), "windows".into()],
        Engine::Ae => vec!["-n".into(), "1".into(), "--".into(), "windows".into()],
    };
    let t0 = std::time::Instant::now();
    let out = run_tool(engine, exe, &args)?;
    let ms = t0.elapsed().as_millis();
    // ae.exe answers from the saved index file when anything is not running,
    // and says so on stderr. Usable, but no longer live.
    if engine == Engine::Ae && out.stderr.contains("没有运行") {
        return Ok(format!("可用（{ms}ms）· anything 没在运行，结果来自上次保存的索引，建议让 anything 常驻"));
    }
    Ok(format!("可用（{ms}ms）"))
}

/// Spawn `es.exe` once, for the "does spawning it make a sound?" A/B.
///
/// `flags` selects the creation flags so the question can be answered by ear:
/// `no_window` (what the app uses), `detached` (`DETACHED_PROCESS`, no console at
/// all) or `none` (inherit — the case that should be loudest, which is how the
/// person listening knows the test is working).
pub fn probe_spawn(flags: &str) -> std::io::Result<()> {
    // Spawn `es.exe` **directly**: routing through `cmd /c` would put a second
    // console program into the experiment and the ear could not tell which one
    // made the noise.
    let Some(es) = discover_es(None) else {
        return Err(std::io::Error::other("es.exe not found"));
    };
    let mut cmd = std::process::Command::new(es);
    cmd.arg("-n").arg("1").arg("ever");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let code = match flags {
            "detached" => 0x0000_0008, // DETACHED_PROCESS: no console at all
            "none" => 0,               // inherit: the loud case, proves the test runs
            _ => 0x0800_0000,          // CREATE_NO_WINDOW: what the app uses
        };
        cmd.creation_flags(code);
    }
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    let mut child = cmd.spawn()?;
    let _ = child.wait();
    Ok(())
}

/// Escape a literal query for Everything's regex mode.
fn regex_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for ch in text.chars() {
        if "\\^$.*+?()[]{}|".contains(ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_bytes_decode_from_gbk() {
        // "测试" in GBK, wrapped in an ASCII path, exactly as es.exe emits it.
        let mut bytes = br"D:\tmp\".to_vec();
        bytes.extend_from_slice(&[0xb2, 0xe2, 0xca, 0xd4]);
        bytes.extend_from_slice(b".txt\r\n");
        let text = decode_console(&bytes);
        assert_eq!(text, "D:\\tmp\\测试.txt\r\n");

        // UTF-8 (a future es.exe, or our own test data) must pass through.
        let utf8 = "D:\\tmp\\测试.txt\r\n".as_bytes();
        assert_eq!(decode_console(utf8), "D:\\tmp\\测试.txt\r\n");

        assert_eq!(decode_console(b""), "");
    }

    /// Multi-word queries must reach `es.exe` as **separate arguments**; sent as
    /// one they are read as a literal phrase and match nothing (measured
    /// 2026-10-17: `es "report template"` → 0 rows, `es report template` → hits).
    #[test]
    fn query_terms_are_split_so_multi_word_searches_work() {
        assert_eq!(split_query_terms("temp"), vec!["temp"]);
        assert_eq!(
            split_query_terms("  report   template "),
            vec!["report", "template"]
        );
        // Quotes the user typed keep a group together (Everything's phrase form).
        assert_eq!(
            split_query_terms("\"final report\" template"),
            vec!["final report", "template"]
        );
        // Unbalanced quotes are forgiven rather than dropping the word.
        assert_eq!(split_query_terms("\"unbalanced"), vec!["unbalanced"]);
        assert!(split_query_terms("   ").is_empty());
    }

    /// The exclusions have to reach Everything as separate `!term` arguments.
    /// Both "rules" below were measured against `es.exe` directly (2026-10-17):
    /// a space inside one argument makes it a literal phrase, and a space inside
    /// the *term* makes the whole query match nothing.
    #[test]
    fn exclusions_become_negation_terms_where_es_accepts_them() {
        let ex: Vec<String> = vec![
            "\\node_modules\\".to_string(),
            "   ".to_string(),
            String::new(),
            "\\system volume information\\".to_string(),
            "\\appdata\\local\\temp\\".to_string(),
        ];
        assert_eq!(
            query_exclusions(&ex, true),
            vec![
                "!\\node_modules\\".to_string(),
                "!\\appdata\\local\\temp\\".to_string(),
            ],
            "empty fragments and the one with spaces stay client-side only"
        );
        assert!(query_exclusions(&ex, false).is_empty(), "filter off = no terms");
        // The app must never hand es.exe a term it answers by returning nothing.
        assert!(query_exclusions(&ex, true).iter().all(|t| !t.contains(' ')));
    }

    #[test]
    fn noise_filter_matches_path_fragments() {
        let ex: Vec<String> = DEFAULT_EXCLUSIONS.iter().map(|s| s.to_string()).collect();
        assert!(is_noise(r"C:\p\node_modules\react\index.js", &ex));
        assert!(is_noise(r"D:\repo\.git\config", &ex));
        assert!(is_noise(r"R:\roaming\AppData\Local\Temp\a.txt", &ex));
        assert!(is_noise(r"C:\Windows\WinSxS\manifests\x.manifest", &ex));
        // Real files are never noise.
        assert!(!is_noise(r"D:\work\MxRun\README.md", &ex));
        assert!(!is_noise(r"S:\games\Steam\Steam.exe", &ex));
        // An empty list turns the filter off, and case does not matter.
        assert!(!is_noise(r"C:\p\node_modules\x", &[]));
        assert!(is_noise(r"c:\P\NODE_MODULES\x", &vec![r"\node_modules\".into()]));
    }

    #[test]
    fn parse_hits_needs_files_that_exist() {
        let dir = std::env::temp_dir();
        let real = dir.join("mxrun-filesearch-test.txt");
        std::fs::write(&real, b"x").expect("write");
        let text = format!(
            "{}\r\n{}\r\n\r\n",
            real.display(),
            dir.join("mxrun-definitely-missing.txt").display()
        );
        let hits = parse_hits(&text);
        assert_eq!(hits.len(), 1, "a path that is gone is dropped");
        assert_eq!(hits[0].path, real.to_string_lossy());
        assert!(!hits[0].is_dir);
        assert!(hits[0].modified > 0);

        // Directories are recognised as such.
        let hits = parse_hits(&format!("{}\r\n", dir.display()));
        assert_eq!(hits.len(), 1);
        assert!(hits[0].is_dir);

        let _ = std::fs::remove_file(&real);
    }

    #[test]
    fn ranking_prefers_name_matches_then_shallow_then_recent() {
        let opened = std::collections::HashMap::new();
        let hits = vec![
            FileHit {
                path: r"D:\a\b\c\d\report-notes.md".into(),
                is_dir: false,
                modified: 1_700_000_000,
            },
            FileHit {
                path: r"D:\report.md".into(),
                is_dir: false,
                modified: 1_700_000_000,
            },
            FileHit {
                path: r"D:\x\old-report.md".into(),
                is_dir: false,
                modified: 1_700_000_000,
            },
        ];
        let ranked = rank(hits, "report", &opened);
        // Precedence as designed: a name *starting* with the query (1000) beats a
        // word start after a dash (600), and depth only shuffles within a tier —
        // so the deep `report-notes.md` still comes before the shallow
        // `old-report.md`, even though it lives five folders deeper.
        assert_eq!(ranked[0].path, r"D:\report.md");
        assert_eq!(ranked[1].path, r"D:\a\b\c\d\report-notes.md");
        assert_eq!(ranked[2].path, r"D:\x\old-report.md");
    }

    #[test]
    fn opening_a_file_before_lifts_it() {
        let hits = vec![
            FileHit {
                path: r"D:\one\notes.md".into(),
                is_dir: false,
                modified: 1_700_000_000,
            },
            FileHit {
                path: r"D:\two\notes.md".into(),
                is_dir: false,
                modified: 1_700_000_000,
            },
        ];
        let mut opened = std::collections::HashMap::new();
        opened.insert(r"D:\two\notes.md".to_string(), 3u32);
        let ranked = rank(hits, "notes", &opened);
        assert_eq!(ranked[0].path, r"D:\two\notes.md");
    }

    #[test]
    fn regex_escape_leaves_a_literal_query_alone() {
        // Only regex metacharacters get escaped — a dash is literal outside a
        // character class, so `plain-name` stays readable.
        assert_eq!(regex_escape("plain-name.txt"), r"plain-name\.txt");
        assert_eq!(regex_escape("a(b)[c]"), r"a\(b\)\[c\]");
        assert_eq!(regex_escape(""), "");
    }

    /// es.exe is asked first; after it failed, ae.exe goes first for a while —
    /// but es.exe is still tried if ae.exe fails too.
    #[test]
    fn es_goes_first_unless_it_failed_recently() {
        let both = SearchTools {
            es: Some(PathBuf::from("es.exe")),
            ae: Some(PathBuf::from("ae.exe")),
        };
        assert_eq!(engine_order(&both, false), vec![Engine::Es, Engine::Ae]);
        assert_eq!(engine_order(&both, true), vec![Engine::Ae, Engine::Es]);
        let es_only = SearchTools { es: both.es.clone(), ae: None };
        assert_eq!(engine_order(&es_only, true), vec![Engine::Es], "nothing to stand in");
        let ae_only = SearchTools { es: None, ae: both.ae.clone() };
        assert_eq!(engine_order(&ae_only, false), vec![Engine::Ae]);
        assert!(engine_order(&SearchTools::default(), false).is_empty());
        assert!(!SearchTools::default().any());
    }

    /// ae.exe takes `--` and then the same terms and `!exclusions` es.exe gets
    /// (verified against the real ae.exe 0.2.0 on 2026-10-20), so a dashed word
    /// needs no regex detour there and the exclusions still apply.
    #[test]
    fn ae_gets_terms_after_a_double_dash() {
        let ex = vec!["\\node_modules\\".to_string()];
        let terms = split_query_terms("-foo bar");
        assert_eq!(
            ae_args(&terms, 40, &ex, true),
            vec!["-n", "40", "-sort-date", "-desc", "--", "-foo", "bar", "!\\node_modules\\"]
        );
        // es.exe still takes the regex route for the same query, without `!terms`.
        assert_eq!(
            es_args("-foo bar", &terms, 40, &ex, true),
            vec!["-n", "40", "-sort", "date-modified", "-sort-descending", "-r", "-foo bar"]
        );
        let plain = split_query_terms("report");
        assert_eq!(
            es_args("report", &plain, 40, &ex, true),
            vec!["-n", "40", "-sort", "date-modified", "-sort-descending", "report", "!\\node_modules\\"]
        );
    }

    #[test]
    fn failure_reasons_come_from_the_right_stream() {
        assert_eq!(
            failure_reason(Engine::Es, "Error 8: Everything IPC not found.\r\n", ""),
            "Error 8: Everything IPC not found."
        );
        assert_eq!(
            failure_reason(Engine::Ae, "", "\r\nae: anything 没有运行，且无法读取索引文件\r\n"),
            "anything 没有运行，且无法读取索引文件"
        );
        assert_eq!(failure_reason(Engine::Ae, "Error x", ""), "(无说明)");
    }

    /// The whole fallback against real binaries: an "es.exe" that fails, then
    /// `ae.exe` answering. Needs `MXRUN_TEST_AE` (a working ae.exe) and
    /// `MXRUN_TEST_QUERY` (a name it can find).
    #[test]
    #[ignore = "needs a real ae.exe: set MXRUN_TEST_AE and MXRUN_TEST_QUERY"]
    fn live_fallback_to_ae() {
        let (Ok(ae), Ok(query)) = (std::env::var("MXRUN_TEST_AE"), std::env::var("MXRUN_TEST_QUERY")) else {
            eprintln!("set MXRUN_TEST_AE and MXRUN_TEST_QUERY");
            return;
        };
        // `where.exe` exits non-zero for these arguments: a stand-in broken es.exe.
        let broken_es = PathBuf::from(r"C:\Windows\System32\where.exe");
        let tools = SearchTools { es: Some(broken_es.clone()), ae: Some(PathBuf::from(&ae)) };
        let opened = std::collections::HashMap::new();
        let (hits, engine) = search(&tools, &query, 10, &[], false, &opened).expect("ae.exe should answer");
        eprintln!("{} hits via {:?}: {:?}", hits.len(), engine, hits.iter().map(|h| &h.path).collect::<Vec<_>>());
        assert_eq!(engine, Engine::Ae);
        assert!(!hits.is_empty());
        // Both broken: the error names both tools.
        let none = SearchTools { es: Some(broken_es.clone()), ae: Some(broken_es) };
        ES_COOLDOWN_UNTIL.store(0, std::sync::atomic::Ordering::Relaxed);
        let err = search(&none, &query, 10, &[], false, &opened).unwrap_err();
        eprintln!("both broken: {err}");
        assert!(err.contains("es.exe") && err.contains("ae.exe"));
        eprintln!("probe ae: {:?}", probe(Engine::Ae, Path::new(&ae)));
        eprintln!("discover_ae(None): {:?}", discover_ae(None));
    }

    /// A leading dash is a switch to `es.exe` ("Error 6: Unknown switch.", exit
    /// 6) — but as the **value** of `-r` it is taken literally, which was
    /// verified against the real binary on 2026-09-19: `es.exe -r "-foo"` exits
    /// 0 and searches for `-foo`. That is why dashed queries go through `-r`.
    #[test]
    fn a_dashed_query_is_not_mistaken_for_a_switch() {
        let q = "-foo";
        assert!(q.starts_with('-'));
        assert_eq!(regex_escape(q), "-foo", "no escaping needed for the dash");
        // The real protection is *where* the text goes: after `-r`.
        assert_eq!(regex_escape("a-b"), "a-b");
    }
}

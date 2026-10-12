//! File search for `@` mode: Everything's `es.exe` behind a small, testable API.
//!
//! Measured facts this module is built on (2026-09-19, `docs/开发进度.md` §2.31):
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

/// Find `es.exe`: the configured path first, then `PATH`, then the usual spots.
///
/// Pure filesystem work; returns `None` when Everything's CLI is not installed,
/// which the UI reports as "装/指定 es.exe" rather than showing an empty list.
pub fn discover_es(configured: Option<&str>) -> Option<PathBuf> {
    // A test/power-user override, in the spirit of the other `MXRUN_*` switches.
    if let Some(from_env) = std::env::var_os("MXRUN_ES_PATH") {
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
        let inside = path.join("es.exe");
        if inside.is_file() {
            return Some(inside);
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let cand = dir.join("es.exe");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
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
    guesses.into_iter().find(|p| p.is_file())
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

/// Ask Everything. Blocking; the caller runs it off the UI thread.
///
/// Returns `Err` with a human-readable reason when the tool is missing, when it
/// fails, or when it takes too long — the UI shows that instead of an empty
/// list, because "nothing found" and "the search is broken" must not look alike.
pub fn search(
    es: &Path,
    query: &str,
    limit: usize,
    exclusions: &[String],
    exclusions_on: bool,
    opened: &std::collections::HashMap<String, u32>,
) -> Result<Vec<FileHit>, String> {
    let fetch = limit.saturating_mul(OVERFETCH).clamp(1, MAX_FETCH);
    let mut cmd = std::process::Command::new(es);
    // **No console window.** A GUI process that spawns a console program gets a
    // console allocated for it, and that flash takes the foreground — the
    // launcher hides itself on focus loss, so searching made the whole card
    // vanish (user report, 2026-09-19: "输入一个 @ 后窗口就关闭了"; the log line
    // was `hide: lost focus`, not an execute).
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.arg("-n").arg(fetch.to_string());
    // Newest first as the *candidate* order: we re-rank anyway, and recency is
    // the better prior when a query matches thousands of files.
    cmd.arg("-sort").arg("date-modified").arg("-sort-descending");
    let query = query.trim();
    if query.is_empty() {
        // Empty query: Everything would list the whole index. The UI shows
        // "recently opened" instead, so nothing to do here.
        return Ok(Vec::new());
    }
    if query.starts_with('-') {
        // A leading dash is read as a switch ("Error 6: Unknown switch"), so
        // take the regex route, where the text is the switch's argument.
        cmd.arg("-r").arg(regex_escape(query));
    } else {
        cmd.arg(query);
    }
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.stdin(std::process::Stdio::null());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("无法启动 es.exe（{}）：{e}", es.display()))?;

    // Read the pipe on its own thread: a child that fills the buffer while we
    // poll for the timeout would otherwise deadlock.
    let stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut out) = stdout {
            use std::io::Read;
            let _ = out.read_to_end(&mut buf);
        }
        buf
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(TIMEOUT_MS);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader.join();
                    return Err(format!("es.exe 超时（{}ms）——Everything 是否在运行？", TIMEOUT_MS));
                }
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
            Err(e) => {
                let _ = reader.join();
                return Err(format!("等待 es.exe 失败：{e}"));
            }
        }
    };

    let bytes = reader.join().unwrap_or_default();
    let code = status.code().unwrap_or(-1);
    let text = decode_console(&bytes);
    if code != 0 {
        // es.exe prints its complaints on stdout, not stderr.
        let first = text
            .lines()
            .find(|l| l.contains("Error"))
            .unwrap_or("(无说明)")
            .trim()
            .to_string();
        return Err(format!("es.exe 返回 {code}：{first}"));
    }

    let hits = parse_hits(&text);
    let filtered: Vec<FileHit> = if exclusions_on {
        hits.into_iter()
            .filter(|h| !is_noise(&h.path, exclusions))
            .collect()
    } else {
        hits
    };
    Ok(rank(filtered, query, opened).into_iter().take(limit).collect())
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

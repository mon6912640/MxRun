//! Single-instance hand-off: the "right-click → add" channel.
//!
//! AltRun did this with a window message: a second `ALTRun.exe "<path>"` posted
//! `WM_SETTEXT` to the running instance's window. MxRun cannot receive an
//! arbitrary window message (the event loop belongs to winit, and eframe only
//! hands us egui events), so the hand-off goes through a directory instead:
//!
//! * the second instance writes one small file per request and **exits
//!   silently** — no dialog, nothing on screen. That is the soul of this
//!   interaction (`docs/AltRun交互规格.md` §11: "不打扰"), and it also fixes
//!   AltRun's flaw of letting two instances race on the same list file;
//! * the running instance's background thread drains the directory (it already
//!   ticks every 50 ms for the hotkey and tray) and opens the add dialog.
//!
//! Requests are written to a temp name and renamed into place, so a drain can
//! never read a half-written file. An **empty** request means "just wake up" —
//! what AltRun did instead of telling the user "already running".

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// What another process asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Show the launcher window (the user started the exe again).
    Wake,
    /// Open the add dialog for these paths.
    Add(Vec<String>),
}

/// `%APPDATA%\MxRun\inbox` — next to the database, so the whole app state
/// lives in one directory (and `APPDATA` can be pointed at a scratch dir for
/// tests and experiments).
pub fn inbox_dir() -> PathBuf {
    crate::store::default_data_dir().join("inbox")
}

/// Ask the running instance to do something. Returns the file that was written.
pub fn send(paths: &[String]) -> std::io::Result<PathBuf> {
    send_to(&inbox_dir(), paths)
}

/// Same, into an explicit directory (tests).
pub fn send_to(dir: &Path, paths: &[String]) -> std::io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    // The pid keeps two simultaneous launches from overwriting each other.
    let name = format!("req-{stamp}-{}.txt", std::process::id());
    let tmp = dir.join(format!("{name}.tmp"));
    let final_path = dir.join(&name);

    // A newline would split one path into two on the receiving side.
    let clean: Vec<&str> = paths
        .iter()
        .map(|p| p.trim())
        .filter(|p| !p.is_empty() && !p.contains('\n'))
        .collect();
    fs::write(&tmp, clean.join("\n"))?;
    fs::rename(&tmp, &final_path)?;
    Ok(final_path)
}

/// Take everything that is pending, newest last. Files are removed as they are
/// read; a file that cannot be read stays for the next tick rather than
/// vanishing silently.
pub fn drain() -> Vec<Request> {
    drain_from(&inbox_dir())
}

/// Same, from an explicit directory (tests).
pub fn drain_from(dir: &Path) -> Vec<Request> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "txt"))
        .collect();
    // Names start with the timestamp, so this is also chronological order.
    files.sort();

    let mut out = Vec::new();
    for file in files {
        let Ok(bytes) = fs::read(&file) else {
            continue; // transient (locked / racing): try again next tick
        };
        let body = String::from_utf8_lossy(&bytes);
        let paths: Vec<String> = body
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        out.push(if paths.is_empty() {
            Request::Wake
        } else {
            Request::Add(paths)
        });
        let _ = fs::remove_file(&file);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("mxrun-ipc-{tag}-{nanos}"));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn add_request_round_trips() {
        let dir = temp_dir("add");
        let paths = vec![r"C:\some\file.txt".to_string(), r"D:\另一 个\目录".to_string()];
        send_to(&dir, &paths).unwrap();

        let got = drain_from(&dir);
        assert_eq!(got, vec![Request::Add(paths.clone())]);
        // Draining twice must not replay the request...
        assert!(drain_from(&dir).is_empty(), "requests are consumed once");
        // ...and the inbox is left clean.
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0, "no leftovers");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Starting the exe again (no paths) means "wake up", not "add nothing".
    #[test]
    fn empty_request_means_wake() {
        let dir = temp_dir("wake");
        send_to(&dir, &[]).unwrap();
        assert_eq!(drain_from(&dir), vec![Request::Wake]);

        // Whitespace-only args are the same thing as no args.
        send_to(&dir, &["   ".to_string()]).unwrap();
        assert_eq!(drain_from(&dir), vec![Request::Wake]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Several files dropped on the exe at once arrive as one request, and two
    /// simultaneous launches do not overwrite each other.
    #[test]
    fn requests_accumulate_and_keep_order() {
        let dir = temp_dir("multi");
        send_to(&dir, &["a".to_string()]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        send_to(&dir, &["b".to_string(), "c".to_string()]).unwrap();

        let got = drain_from(&dir);
        assert_eq!(got.len(), 2, "each launch leaves its own request");
        let mut paths = Vec::new();
        for req in got {
            match req {
                Request::Add(p) => paths.extend(p),
                Request::Wake => panic!("both launches carried a path"),
            }
        }
        assert_eq!(paths, vec!["a", "b", "c"]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A missing inbox is not an error — the first launch of a fresh profile
    /// has none.
    #[test]
    fn draining_a_missing_inbox_is_harmless() {
        let dir = temp_dir("missing");
        assert!(drain_from(&dir).is_empty());
        assert!(!dir.exists());
    }

    /// Garbage in the inbox must not wedge the channel.
    #[test]
    fn junk_files_are_ignored_and_cleared() {
        let dir = temp_dir("junk");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("garbage.bin"), [0xff, 0xfe, 0x00]).unwrap();
        fs::write(dir.join("req-1-2.txt"), [0x41, 0xff, 0x0a, 0x42]).unwrap();

        // The .bin is not ours to touch; the .txt is read lossily.
        assert_eq!(drain_from(&dir), vec![Request::Add(vec!["A\u{fffd}".into(), "B".into()])]);
        let left: Vec<_> = fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).collect();
        assert_eq!(left.len(), 1, "only the foreign file remains");
        assert_eq!(left[0].file_name(), "garbage.bin");
        let _ = fs::remove_dir_all(&dir);
    }
}

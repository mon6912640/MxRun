//! P0-3: import AltRun's `ShortCutList.txt` into the MxRun store.
//!
//! Shape of the source (docs/AltRun交互规格.md, and the file itself):
//!
//! ```text
//! F权重 | 编码列 | 关键字 | 名称 | 命令行
//! F116  |        | Computer | 我的电脑 | ::{20D04FE0-3AEA-1069-A2D8-08002B30309D}
//! F6    |No_Encoding| cmd   | Dos窗口  | cmd /k %p
//! ```
//!
//! Three things this deliberately does **not** copy:
//!
//! 1. **The `F<n>` weights.** They are the author's shipped defaults, not the
//!    user's usage (verified against the template in `untShortCutMan.pas`; see
//!    `docs/命令模型设计.md` §6.1.1). Importing them would freeze a decade-old
//!    ranking into frecency.
//! 2. **`@.\WinCtl.exe …`.** That helper ships with AltRun; the same four
//!    window actions are built-in verbs here.
//! 3. **The separator lines.** AltRun never displayed them either (§6.6).
//!
//! Everything is idempotent: rows are keyed by `provider + "line:N"`, so
//! re-running updates in place and keeps each item's frecency.

use crate::store::{
    Action, ArgSource, ArgSpec, BuiltinVerb, Effect, Encoder, Health, InsertMode, Item, LaunchMode,
    Source, Store,
};
use std::path::Path;

/// Environment variable pointing at the AltRun list on this machine, so no
/// machine-specific path is baked into the repository.
pub const LIST_ENV: &str = "MXRUN_ALTRUN_LIST";

/// Default import source: `$MXRUN_ALTRUN_LIST`, else `ShortCutList.txt` in the
/// working directory. A path on the command line overrides both.
pub fn default_list_path() -> std::path::PathBuf {
    std::env::var_os(LIST_ENV)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("ShortCutList.txt"))
}

/// Curated skip list: entries that no longer make sense on a modern Windows,
/// point at AltRun itself, or belong to dead services. `--import-all` overrides
/// it. Reasoning per entry: `docs/命令模型设计.md` §6.3.
pub const SKIP_KEYWORDS: &[&str] = &[
    "ie",                 // IE retired in 2022
    "InternetConnection", // IE settings page
    "AddRemoveProgram",   // moved into Settings on Win11
    "SystemProperty",     // ditto
    "Windows",            // opening %WINDIR% from a launcher is pointless
    "System32",
    "ProgramFiles",
    "Config",  // .\ALTRun.ini — AltRun's own config file
    "Upgrade", // 2009 blog link, long dead
    "s",       // 搜狗MP3 — service gone
    "v",       // VeryCD — site gone
    "y",       // Yahoo — alive, but never used (its F value was a seed)
    // Author's template entry: a cmd prompt that first pops "what do you want
    // to run?". The user never uses it (2026-09-13).
    "cmd",
];

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub lines: usize,
    pub commands: usize,
    pub imported: usize,
    pub skipped_separator: usize,
    pub skipped_unwanted: usize,
    pub skipped_unsupported: usize,
    pub needs_input: usize,
    pub builtin: usize,
    /// Demo items removed because real data replaced them.
    pub removed_seed: usize,
    pub notes: Vec<String>,
}

impl Report {
    /// Human-readable summary, shown in a dialog (a GUI binary has no console)
    /// and written to the log.
    pub fn summary(&self) -> String {
        let mut s = format!(
            "读取 {} 行，其中命令 {} 条\n\n导入 {} 条",
            self.lines, self.commands, self.imported
        );
        if self.needs_input > 0 {
            s.push_str(&format!("\n· 其中 {} 条需要输入参数（列表显示 *）", self.needs_input));
        }
        if self.builtin > 0 {
            s.push_str(&format!("\n· 其中 {} 条改用内建实现（不依赖外部小工具）", self.builtin));
        }
        s.push_str(&format!(
            "\n\n跳过：分隔线 {} 条、无用条目 {} 条",
            self.skipped_separator, self.skipped_unwanted
        ));
        if self.skipped_unsupported > 0 {
            s.push_str(&format!("、暂不支持 {} 条", self.skipped_unsupported));
        }
        if self.removed_seed > 0 {
            s.push_str(&format!(
                "\n\n已移除 {} 条演示数据（真实数据进来了，它们只会造成重复）",
                self.removed_seed
            ));
        }
        if !self.notes.is_empty() {
            s.push_str("\n\n说明：\n");
            for n in self.notes.iter().take(8) {
                s.push_str(&format!("· {n}\n"));
            }
        }
        s
    }
}

/// One row of the source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawRow {
    pub freq: Option<u32>,
    pub param_type: String,
    pub keyword: String,
    pub name: String,
    pub command: String,
}

/// What to do with a row.
#[derive(Debug, PartialEq)]
pub enum Decision {
    Import(Box<Item>),
    Separator,
    Unwanted(String),
    Unsupported(String),
}

fn is_param_type(field: &str) -> bool {
    matches!(
        field.trim().to_ascii_lowercase().as_str(),
        "" | "no_encoding" | "url_query" | "utf8_query"
    )
}

/// Split one source row. Old files have no `F<n>` column; the parameter-type
/// column may be empty — both are tolerated (the Delphi parser does the same).
pub fn split_row(line: &str) -> Option<RawRow> {
    let parts: Vec<&str> = line.splitn(5, '|').map(str::trim).collect();
    if parts.len() < 4 {
        return None;
    }
    let mut idx = 0;
    let freq = match parts[0].strip_prefix('F').and_then(|v| v.trim().parse().ok()) {
        Some(n) => {
            idx = 1;
            Some(n)
        }
        None => None,
    };
    let param_type = if is_param_type(parts[idx]) {
        let p = parts[idx].to_string();
        idx += 1;
        p
    } else {
        String::new()
    };
    if parts.len() - idx < 3 {
        return None;
    }
    Some(RawRow {
        freq,
        param_type,
        keyword: parts[idx].to_string(),
        name: parts[idx + 1].to_string(),
        command: parts[idx + 2].to_string(),
    })
}

/// First token of a command line (honouring a leading quote) plus the rest.
fn first_token(command: &str) -> (&str, &str) {
    let cmd = command.trim();
    if let Some(rest) = cmd.strip_prefix('"') {
        return match rest.find('"') {
            Some(end) => (&rest[..end], rest[end + 1..].trim()),
            None => (rest, ""),
        };
    }
    match cmd.split_once(' ') {
        Some((head, rest)) => (head, rest.trim()),
        None => (cmd, ""),
    }
}

/// Which effect runs this command line?
///
/// `Open` hands the string to the shell (documents, folders, URLs, protocols,
/// CLSIDs); `Run` starts a process (a program plus its arguments). The split is
/// the whole reason `cmd /k %p` used to fail silently.
pub fn effect_for(command: &str) -> Effect {
    let lower = command.trim().to_ascii_lowercase();
    if lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("::")
        || lower.starts_with("shell:")
        || lower.starts_with("ms-settings:")
    {
        return Effect::Open { target: command.trim().to_string() };
    }
    let (_, args) = first_token(command);
    if args.is_empty() {
        // A lone target: program, folder, document or bare command name.
        Effect::Open { target: command.trim().to_string() }
    } else {
        Effect::Run { line: command.trim().to_string() }
    }
}

/// Derive the argument specification from the source row.
fn argspec_for(row: &RawRow) -> ArgSpec {
    let cmd = &row.command;
    let source = if cmd.contains("{%c}") {
        ArgSource::Clipboard
    } else if cmd.contains("{%wd}") {
        ArgSource::ForegroundId
    } else if cmd.contains("{%wt}") {
        ArgSource::ForegroundTitle
    } else if cmd.contains("{%wc}") {
        ArgSource::ForegroundClass
    } else if row.param_type.trim().is_empty() {
        ArgSource::None
    } else {
        // AltRun popped its parameter dialog for these.
        ArgSource::Prompt
    };
    let encode = match row.param_type.trim().to_ascii_lowercase().as_str() {
        "url_query" => Encoder::UrlQuery,
        "utf8_query" => Encoder::Utf8Percent,
        _ => Encoder::Raw,
    };
    // A placeholder in the line means "fill it in"; otherwise the value is
    // appended — which is exactly how AltRun's search engines work.
    let insert = if source == ArgSource::None {
        InsertMode::None
    } else if cmd.contains("{p}") || cmd.contains("%p") || cmd.contains("{%c}") || cmd.contains("{%w")
    {
        InsertMode::Replace
    } else {
        InsertMode::Append
    };
    ArgSpec { source, encode, insert }
}

/// AltRun's `WinCtl.exe` window actions, rebuilt as in-process verbs.
fn builtin_for(command: &str) -> Option<BuiltinVerb> {
    if !command.to_ascii_lowercase().contains("winctl.exe") {
        return None;
    }
    let lower = command.to_ascii_lowercase();
    if lower.contains("minall") {
        Some(BuiltinVerb::MinimizeAll)
    } else if lower.contains("showonly") {
        Some(BuiltinVerb::HideOthers)
    } else if lower.contains("unhide") {
        Some(BuiltinVerb::ShowForegroundWindow)
    } else if lower.contains("hide") {
        Some(BuiltinVerb::HideForegroundWindow)
    } else {
        None
    }
}

/// Strip AltRun's `@+` / `@-` / `@` launch prefix (order matters: check the two
/// two-character forms first, exactly as the original does).
fn split_launch_prefix(command: &str) -> (LaunchMode, &str) {
    let t = command.trim();
    if let Some(rest) = t.strip_prefix("@+") {
        (LaunchMode::Maximized, rest.trim())
    } else if let Some(rest) = t.strip_prefix("@-") {
        (LaunchMode::Minimized, rest.trim())
    } else if let Some(rest) = t.strip_prefix('@') {
        (LaunchMode::Hidden, rest.trim())
    } else {
        (LaunchMode::Normal, t)
    }
}

/// Turn one row into an item, or explain why it is skipped.
pub fn to_decision(row: &RawRow, line_no: usize, curated: bool) -> Decision {
    if row.keyword.is_empty() && row.command.is_empty() {
        return Decision::Separator;
    }
    if curated && SKIP_KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(&row.keyword)) {
        return Decision::Unwanted(format!("{}（{}）", row.name, row.keyword));
    }

    let (launch, bare) = split_launch_prefix(&row.command);
    let mut args = argspec_for(row);

    // Window control used to be an external helper; make it a builtin verb.
    // The `@` prefix those rows carry only meant "don't flash the helper's own
    // window" — meaningless once the action runs in-process.
    let mut launch = launch;
    let effect = if bare.to_ascii_lowercase().contains("winctl.exe") {
        match builtin_for(bare) {
            Some(verb) => {
                args = ArgSpec::default();
                launch = LaunchMode::Normal;
                Effect::Builtin { verb }
            }
            None => {
                return Decision::Unsupported(format!("{}（未知的窗口动作）", row.name));
            }
        }
    } else {
        effect_for(bare)
    };

    let title = if row.name.trim().is_empty() {
        row.keyword.clone()
    } else {
        row.name.clone()
    };
    Decision::Import(Box::new(Item {
        id: String::new(), // upsert_from_source derives it from the source key
        title,
        subtitle: String::new(),
        keywords: if row.keyword.trim().is_empty() {
            Vec::new()
        } else {
            vec![row.keyword.clone()]
        },
        actions: vec![Action { label: "默认".into(), effect }],
        arg: args,
        launch,
        source: Source {
            provider: "shortcutlist".into(),
            external_id: format!("line:{line_no}"),
        },
        health: Health::Unknown,
    }))
}

/// Decode the file. `ShortCutList.txt` is GBK on a Chinese Windows (the Delphi
/// build wrote the ANSI code page); UTF-8 files are accepted too.
pub fn decode(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    decode_code_page(bytes).unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned())
}

#[cfg(windows)]
fn decode_code_page(bytes: &[u8]) -> Option<String> {
    use windows::Win32::Globalization::{CP_ACP, MultiByteToWideChar};
    unsafe {
        let len = MultiByteToWideChar(CP_ACP, Default::default(), bytes, None);
        if len <= 0 {
            return None;
        }
        let mut buf = vec![0u16; len as usize];
        let n = MultiByteToWideChar(CP_ACP, Default::default(), bytes, Some(&mut buf));
        if n <= 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..n as usize]))
    }
}

#[cfg(not(windows))]
fn decode_code_page(_bytes: &[u8]) -> Option<String> {
    None
}

/// Import a whole file. `curated` applies [`SKIP_KEYWORDS`].
pub fn import_file(
    store: &mut Store,
    path: &Path,
    curated: bool,
) -> Result<Report, Box<dyn std::error::Error>> {
    let bytes = std::fs::read(path)?;
    let text = decode(&bytes);
    let mut report = Report::default();

    for (idx, line) in text.lines().enumerate() {
        let line_no = idx + 1;
        if line.trim().is_empty() {
            report.lines += 1;
            report.skipped_separator += 1;
            continue;
        }
        let Some(row) = split_row(line) else {
            report.lines += 1;
            report.notes.push(format!("第 {line_no} 行无法解析，已跳过"));
            continue;
        };
        report.lines += 1;
        report.commands += 1;
        match to_decision(&row, line_no, curated) {
            Decision::Import(item) => {
                if item.wants_input() {
                    report.needs_input += 1;
                }
                if matches!(
                    item.default_action().map(|a| &a.effect),
                    Some(Effect::Builtin { .. })
                ) {
                    report.builtin += 1;
                }
                store.upsert_from_source(*item)?;
                report.imported += 1;
            }
            Decision::Separator => report.skipped_separator += 1,
            Decision::Unwanted(what) => {
                report.skipped_unwanted += 1;
                report.notes.push(format!("跳过无用条目：{what}"));
            }
            Decision::Unsupported(what) => {
                report.skipped_unsupported += 1;
                report.notes.push(format!("跳过暂不支持的条目：{what}"));
            }
        }
    }

    // The demo items only exist so a fresh database is not empty. Once real
    // data is in they are duplicates ("计算器" beside the imported "Calc"), so
    // they go — but only when the import actually produced something.
    if report.imported > 0 {
        report.removed_seed = remove_seed_items(store)?;
    }
    Ok(report)
}

/// Providers that only ever hold placeholder data.
///
/// `seed` is what a fresh database gets today; `legacy` is what v1's six demo
/// rows migrate into — v1 had no way to add anything else (`add_command` was
/// never called), so a `legacy` item is always demo data too.
const DEMO_PROVIDERS: &[&str] = &["seed", "legacy"];

/// Delete the demo items. Returns how many were removed.
fn remove_seed_items(store: &mut Store) -> Result<usize, Box<dyn std::error::Error>> {
    let ids: Vec<String> = store
        .load_items()?
        .into_iter()
        .filter(|i| DEMO_PROVIDERS.contains(&i.source.provider.as_str()))
        .map(|i| i.id)
        .collect();
    for id in &ids {
        store.delete_item(id)?;
    }
    Ok(ids.len())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliImport {
    pub path: std::path::PathBuf,
    /// Apply [`SKIP_KEYWORDS`] (default). `--import-all` turns it off.
    pub curated: bool,
}

/// Recognise the import switches. Accepts an explicit path after the flag,
/// otherwise the AltRun install on this machine.
pub fn parse_args(args: impl Iterator<Item = String>) -> Option<CliImport> {
    let args: Vec<String> = args.collect();
    let (flag, curated) = args.iter().enumerate().find_map(|(i, a)| match a.as_str() {
        "--import" => Some((i, true)),
        "--import-all" => Some((i, false)),
        _ => None,
    })?;
    let path = args
        .get(flag + 1)
        .filter(|a| !a.starts_with("--"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(default_list_path);
    Some(CliImport { path, curated })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_the_import_switches() {
        let args = |v: &[&str]| -> Vec<String> { v.iter().map(|s| s.to_string()).collect() };
        assert!(parse_args(args(&[]).into_iter()).is_none());
        let curated = parse_args(args(&["--import"]).into_iter()).unwrap();
        assert!(curated.curated);
        assert_eq!(curated.path, default_list_path());

        let all = parse_args(args(&["--import-all"]).into_iter()).unwrap();
        assert!(!all.curated);

        let explicit = parse_args(args(&["--import", r"D:\tmp\other.txt"]).into_iter()).unwrap();
        assert!(explicit.curated);
        assert_eq!(explicit.path, std::path::PathBuf::from(r"D:\tmp\other.txt"));
    }

    fn row(line: &str) -> RawRow {
        split_row(line).unwrap_or_else(|| panic!("cannot parse {line:?}"))
    }

    #[test]
    fn parses_a_modern_row() {
        let r = row("F116     |                    |Computer                      |我的电脑                      |::{20D04FE0-3AEA-1069-A2D8-08002B30309D}");
        assert_eq!(r.freq, Some(116));
        assert_eq!(r.param_type, "");
        assert_eq!(r.keyword, "Computer");
        assert_eq!(r.name, "我的电脑");
        assert!(r.command.starts_with("::{20D04FE0"));
    }

    #[test]
    fn parses_a_row_with_a_param_type() {
        let r = row("F6       |No_Encoding         |cmd                           |Dos窗口                       |cmd /k %p");
        assert_eq!(r.freq, Some(6));
        assert_eq!(r.param_type, "No_Encoding");
        assert_eq!(r.keyword, "cmd");
        assert_eq!(r.command, "cmd /k %p");
    }

    #[test]
    fn keeps_pipes_inside_the_command() {
        // AltRun re-joins the tail rather than splitting it; a URL may contain |.
        let r = row("F0       |                    |x                             |y                             |http://a/?q=1|2");
        assert_eq!(r.command, "http://a/?q=1|2");
    }

    #[test]
    fn separators_are_skipped_not_imported() {
        let r = row("         |                    |                              |                              |");
        assert_eq!(to_decision(&r, 17, true), Decision::Separator);
    }

    #[test]
    fn curated_mode_drops_the_agreed_skip_list() {
        let ie = row("F8       |                    |ie                            |IE浏览器                      |iexplore.exe");
        assert!(matches!(to_decision(&ie, 4, true), Decision::Unwanted(_)));
        // --import-all keeps everything.
        assert!(matches!(to_decision(&ie, 4, false), Decision::Import(_)));
    }

    #[test]
    fn window_control_becomes_a_builtin_verb() {
        let cases = [
            ("@.\\WinCtl.exe MinAll", BuiltinVerb::MinimizeAll),
            ("@.\\WinCtl.exe ShowOnly {%wd}", BuiltinVerb::HideOthers),
            ("@.\\WinCtl.exe Hide {%wd}", BuiltinVerb::HideForegroundWindow),
            ("@.\\WinCtl.exe UnHide", BuiltinVerb::ShowForegroundWindow),
        ];
        for (command, expected) in cases {
            let line = format!("F0       |No_Encoding         |k                             |n                             |{command}");
            let Decision::Import(item) = to_decision(&row(&line), 1, true) else {
                panic!("{command} should import");
            };
            match &item.default_action().unwrap().effect {
                Effect::Builtin { verb } => assert_eq!(*verb, expected, "{command}"),
                other => panic!("{command} -> {other:?}"),
            }
            // The @ prefix must not leak into the item.
            assert_eq!(item.launch, LaunchMode::Normal);
            assert!(!item.wants_input(), "{command} needs no input");
        }
    }

    #[test]
    fn search_engines_append_and_clipboard_ones_replace() {
        // 百度: prefix + appended query, URL encoded (no placeholder at all).
        let b = row("F12      |URL_Query           |b                             |百度 搜索引擎                 |http://www.baidu.com/s?wd=");
        let Decision::Import(baidu) = to_decision(&b, 23, true) else {
            panic!("baidu should import");
        };
        assert_eq!(baidu.arg.source, ArgSource::Prompt);
        assert_eq!(baidu.arg.encode, Encoder::UrlQuery);
        assert_eq!(baidu.arg.insert, InsertMode::Append);
        assert!(baidu.wants_input());
        assert!(matches!(baidu.default_action().unwrap().effect, Effect::Open { .. }));

        // 搜剪贴板: no typing at all, the {%c} marker is replaced in place.
        let cb = row("F12      |URL_Query           |cb                            |百度 搜索剪贴板               |http://www.baidu.com/s?wd={%c}");
        let Decision::Import(clip) = to_decision(&cb, 38, true) else {
            panic!("cb should import");
        };
        assert_eq!(clip.arg.source, ArgSource::Clipboard);
        assert_eq!(clip.arg.insert, InsertMode::Replace);
        assert_eq!(clip.arg.encode, Encoder::UrlQuery);
        assert!(!clip.wants_input(), "clipboard search needs no typing");
    }

    #[test]
    fn commands_with_arguments_run_instead_of_open() {
        // The shape that used to fail silently through open::that. (`cmd` is on
        // the curated skip list now, so this asks for the uncurated decision —
        // the point here is the effect derivation, not the curation.)
        let dos = row("F6       |No_Encoding         |cmd                           |Dos窗口                       |cmd /k %p");
        let Decision::Import(item) = to_decision(&dos, 7, false) else {
            panic!("cmd should import when not curated");
        };
        match &item.default_action().unwrap().effect {
            Effect::Run { line } => assert_eq!(line, "cmd /k %p"),
            other => panic!("expected Run, got {other:?}"),
        }
        assert_eq!(item.arg.source, ArgSource::Prompt);
        assert_eq!(item.arg.insert, InsertMode::Replace);
        assert!(item.wants_input());

        // A lone program name opens (the shell resolves it).
        let nslookup = row("F0       |                    |myip                          |我的IP地址                    |nslookup");
        let Decision::Import(item) = to_decision(&nslookup, 12, true) else {
            panic!("myip should import");
        };
        assert!(matches!(item.default_action().unwrap().effect, Effect::Open { .. }));
        assert!(!item.wants_input());
    }

    #[test]
    fn quoted_apps_with_arguments_run() {
        // Shape under test: a quoted interpreter path followed by a script —
        // a very common entry in imported lists.
        let script = row("F4       |                    |py                            |PythonScript                  |C:\\Tools\\python\\pythonw.exe \"C:\\Tools\\agent\\launch.pyw\"");
        let Decision::Import(item) = to_decision(&script, 68, true) else {
            panic!("interpreter + script should import");
        };
        assert!(matches!(item.default_action().unwrap().effect, Effect::Run { .. }));
    }

    #[test]
    fn launch_prefix_is_parsed_not_kept() {
        let line = "F0       |                    |k                             |n                             |@+calc.exe";
        let Decision::Import(item) = to_decision(&row(line), 1, true) else {
            panic!("should import");
        };
        assert_eq!(item.launch, LaunchMode::Maximized);
        match &item.default_action().unwrap().effect {
            Effect::Open { target } => assert_eq!(target, "calc.exe"),
            other => panic!("{other:?}"),
        }
    }

    /// The real file, when this machine has it (point `$MXRUN_ALTRUN_LIST` at
    /// it): counts and the two shapes that matter most (`cmd /k %p` and the
    /// clipboard search).
    #[test]
    fn imports_the_real_alt_run_list() {
        let path = default_list_path();
        if !path.exists() {
            eprintln!("skip: {} not present ($MXRUN_ALTRUN_LIST)", path.display());
            return;
        }
        let text = decode(&std::fs::read(path).unwrap());
        let report = scan(&text, true);
        // Rust's `lines()` drops the trailing newline, so this is 72 commands
        // plus the 4 real separators from the author's template.
        assert_eq!(report.lines, 76, "line count");
        assert_eq!(report.commands, 72, "command count");
        assert_eq!(report.skipped_separator, 4, "the four blank separator rows");
        assert_eq!(report.imported, 59, "curated import size");
        assert_eq!(report.skipped_unwanted, 13, "the agreed skip list");
        assert_eq!(report.needs_input, 4, "r, b, g, zd need typing");
        assert_eq!(report.builtin, 4, "the four WinCtl actions");
    }

    /// Importing real data must clear the demo items, or the list ends up with
    /// "计算器" next to the imported "Calc". Covers both the demo provider of a
    /// fresh database and the `legacy` provider v1 rows migrate into.
    #[test]
    fn importing_removes_the_demo_items() {
        let dir = std::env::temp_dir().join(format!("mxrun-imp-seed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = Store::open_at(dir.clone()).unwrap();
        assert_eq!(store.load_items().unwrap().len(), 6, "fresh db is seeded");

        // Make it look like the user's database: v1 rows migrate with the
        // `legacy` provider, not `seed`.
        for mut item in store.load_items().unwrap() {
            item.source.provider = "legacy".into();
            store.upsert_item(&item).unwrap();
        }

        let file = dir.join("list.txt");
        std::fs::write(
            &file,
            "F12      |URL_Query           |b                             |百度 搜索引擎                 |http://www.baidu.com/s?wd=\n",
        )
        .unwrap();
        let report = import_file(&mut store, &file, true).unwrap();
        assert_eq!(report.imported, 1);
        assert_eq!(report.removed_seed, 6, "demo items are dropped");

        let items = store.load_items().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].source.provider, "shortcutlist");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Temporary diagnostic: dump what the real file turns into.
    #[test]
    #[ignore = "diagnostic dump"]
    fn dump_imported_items() {
        let path = default_list_path();
        let text = decode(&std::fs::read(path).unwrap());
        for (idx, line) in text.lines().enumerate() {
            let Some(row) = split_row(line) else { continue };
            let Decision::Import(item) = to_decision(&row, idx + 1, true) else { continue };
            if idx < 15 {
                println!(
                    "line{} title={:?} keywords={:?} effect={:?}",
                    idx + 1,
                    item.title,
                    item.keywords,
                    item.default_action().unwrap().effect
                );
            }
        }
    }

    /// Same walk as `import_file` without touching a store.
    fn scan(text: &str, curated: bool) -> Report {
        let mut r = Report::default();
        for (idx, line) in text.lines().enumerate() {
            r.lines += 1;
            if line.trim().is_empty() {
                r.skipped_separator += 1;
                continue;
            }
            let Some(row) = split_row(line) else { continue };
            r.commands += 1;
            match to_decision(&row, idx + 1, curated) {
                Decision::Import(item) => {
                    r.imported += 1;
                    if item.wants_input() {
                        r.needs_input += 1;
                    }
                    if matches!(
                        item.default_action().map(|a| &a.effect),
                        Some(Effect::Builtin { .. })
                    ) {
                        r.builtin += 1;
                    }
                }
                Decision::Separator => r.skipped_separator += 1,
                Decision::Unwanted(_) => r.skipped_unwanted += 1,
                Decision::Unsupported(_) => r.skipped_unsupported += 1,
            }
        }
        r
    }
}

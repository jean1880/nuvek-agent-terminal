//! The external editor: its configuration shape, the presets, the argv template, the grammar of
//! a `path:line:column` location, and the confinement of the file to open.
//!
//! Like [`crate::difftool`], a tool is a name and an argv template. Placeholders are substituted
//! per ELEMENT of the argv and the result is handed to a process as an argv: no shell ever sees
//! it, so a path with spaces, quotes or `$(…)` in it is just a path. The program is fixed in the
//! configuration, so repository content, agent text and link text never choose what runs.
//!
//! | Placeholder | Becomes |
//! |---|---|
//! | `{file}` | the absolute, canonical path of the file (required) |
//! | `{line}` | the 1-based line; 1 when none is known |
//! | `{column}` | the 1-based column; 1 when none is known |
//! | `{dir}` | the absolute folder the file is opened from |
//!
//! `{{` and `}}` are literal braces; any other `{name}` is rejected when the tool is saved.
//!
//! `{file}` is absolute, so it never starts with `-` and can never be read as an option.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::difftool::{pieces_with, Piece};

/// A configured external editor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditorTool {
    /// What the buttons call it ("Open in Zed").
    pub name: String,
    /// The program, then its arguments, with placeholders.
    pub argv: Vec<String>,
    /// Whether the editor runs in a terminal. Kept for a later slice; the UI never sets it.
    #[serde(default)]
    pub terminal: bool,
}

/// The placeholders, in documentation order.
pub const PLACEHOLDERS: [&str; 4] = ["file", "line", "column", "dir"];

/// A ready-made editor the Settings dropdown offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub name: &'static str,
    pub argv: &'static [&'static str],
}

/// The presets, in dropdown order (the dropdown adds "Custom" after them). Guarded by
/// `presets_are_valid_and_found_by_argv`.
pub const PRESETS: [Preset; 4] = [
    Preset {
        name: "VS Code",
        argv: &["code", "-g", "{file}:{line}:{column}"],
    },
    Preset {
        name: "Zed",
        argv: &["zed", "{file}:{line}:{column}"],
    },
    Preset {
        name: "JetBrains",
        argv: &["idea", "--line", "{line}", "--column", "{column}", "{file}"],
    },
    Preset {
        name: "GNOME Text Editor",
        argv: &["gnome-text-editor", "{file}+{line}:{column}"],
    },
];

impl Preset {
    pub fn tool(&self) -> EditorTool {
        EditorTool {
            name: self.name.to_owned(),
            argv: self.argv.iter().map(|s| (*s).to_owned()).collect(),
            terminal: false,
        }
    }

    /// The program that must be installed for this preset to work.
    pub fn program(&self) -> &'static str {
        self.argv[0]
    }

    /// Whether the program is installed. `locate` is the app's `SystemProbe::locate`; only an
    /// absolute answer counts.
    pub fn is_installed(&self, locate: impl Fn(&str) -> Option<String>) -> bool {
        locate(self.program()).is_some_and(|p| Path::new(&p).is_absolute())
    }
}

/// The index of the preset whose argv is exactly `tool`'s, if any (the dropdown's selection).
pub fn preset_index(tool: &EditorTool) -> Option<usize> {
    PRESETS.iter().position(|p| {
        p.argv
            .iter()
            .copied()
            .eq(tool.argv.iter().map(String::as_str))
    })
}

/// Checks a tool as it is typed: a name, a program that is not a placeholder and does not start
/// with `-`, only known placeholders, and `{file}` somewhere after the program.
pub fn validate(tool: &EditorTool) -> Result<(), String> {
    if tool.name.trim().is_empty() {
        return Err("Give the editor a name".to_owned());
    }
    let Some(program) = tool.argv.first().filter(|p| !p.trim().is_empty()) else {
        return Err("Name the program to run".to_owned());
    };
    if program.starts_with('-') {
        return Err("The program cannot start with “-”".to_owned());
    }
    if pieces_with(program, &PLACEHOLDERS)?
        .iter()
        .any(|p| matches!(p, Piece::Holder(_)))
    {
        return Err("The program itself cannot be a placeholder".to_owned());
    }
    let mut names_a_file = false;
    for element in &tool.argv[1..] {
        for piece in pieces_with(element, &PLACEHOLDERS)? {
            names_a_file |= piece == Piece::Holder("file");
        }
    }
    if !names_a_file {
        return Err("Use {file} so the editor knows what to open".to_owned());
    }
    Ok(())
}

/// What the placeholders become.
#[derive(Debug, Clone)]
pub struct Values {
    pub file: OsString,
    pub line: u32,
    pub column: u32,
    pub dir: OsString,
}

/// The argv for `tool` with its placeholders filled, element by element. The program is
/// returned as given. Refuses a `{file}` or `{dir}` that is not absolute.
pub fn substitute(tool: &EditorTool, values: &Values) -> Result<Vec<OsString>, String> {
    validate(tool)?;
    for (name, v) in [("file", &values.file), ("dir", &values.dir)] {
        if !Path::new(v).is_absolute() {
            return Err(format!("{{{name}}} is not an absolute path"));
        }
    }
    let (line, column) = (
        values.line.max(1).to_string(),
        values.column.max(1).to_string(),
    );
    let mut out = Vec::with_capacity(tool.argv.len());
    for (i, element) in tool.argv.iter().enumerate() {
        if i == 0 {
            out.push(OsString::from(element));
            continue;
        }
        let mut arg = OsString::new();
        for piece in pieces_with(element, &PLACEHOLDERS)? {
            match piece {
                Piece::Text(t) => arg.push(t),
                Piece::Holder("file") => arg.push(&values.file),
                Piece::Holder("line") => arg.push(&line),
                Piece::Holder("column") => arg.push(&column),
                Piece::Holder(_) => arg.push(&values.dir),
            }
        }
        out.push(arg);
    }
    Ok(out)
}

/// The longest location text read.
pub const MAX_LOCATION_BYTES: usize = 4096;
/// Line numbers are clamped to this.
pub const MAX_LINE: u32 = 10_000_000;
/// Column numbers are clamped to this.
pub const MAX_COLUMN: u32 = 100_000;

/// A path with the line and column its text named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub path: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

/// Reads an all-digit number: 0 becomes 1, and a huge one is clamped to `max`.
fn number(text: &str, max: u32) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value = text.parse::<u64>().unwrap_or(u64::MAX);
    Some(value.clamp(1, u64::from(max)) as u32)
}

/// Splits a trailing `:digits` off `text`. Not when what is left is empty or one letter, which
/// is a Windows drive (`C:12`), not a path with a line.
fn split_number(text: &str) -> Option<(&str, &str)> {
    let (head, tail) = text.rsplit_once(':')?;
    let drive = head.len() == 1 && head.bytes().all(|b| b.is_ascii_alphabetic());
    (!head.is_empty() && !drive && !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()))
        .then_some((head, tail))
}

/// Reads `path`, `path:12`, `path:12:3` or `path#L12`. Bounded: text over
/// [`MAX_LOCATION_BYTES`], an empty path, or a control character is `None`. A zero line or
/// column is 1, and a huge one is clamped. Anything else after the path stays in the path.
pub fn parse_location(text: &str) -> Option<Location> {
    let text = text.trim();
    if text.is_empty() || text.len() > MAX_LOCATION_BYTES || text.chars().any(char::is_control) {
        return None;
    }
    let location = |path: &str, line: Option<u32>, column: Option<u32>| {
        (!path.is_empty()).then(|| Location {
            path: path.to_owned(),
            line,
            column,
        })
    };
    if let Some((path, fragment)) = text.rsplit_once("#L") {
        if let Some(line) = number(fragment, MAX_LINE) {
            return location(path, Some(line), None);
        }
    }
    if let Some((rest, last)) = split_number(text) {
        if let Some((path, line)) = split_number(rest) {
            return location(path, number(line, MAX_LINE), number(last, MAX_COLUMN));
        }
        return location(rest, number(last, MAX_LINE), None);
    }
    location(text, None, None)
}

/// Whether `text` starts with a URI scheme (`https:`, `mailto:`) rather than a path with a line
/// (`a.rs:12`, `a.rs:12:3`): a scheme is a letter then letters, digits, `+`, `.` or `-`, and
/// what follows its colon is not all line and column numbers. One letter is a Windows drive.
fn has_scheme(text: &str) -> bool {
    let Some((scheme, rest)) = text.split_once(':') else {
        return false;
    };
    let mut chars = scheme.chars();
    let is_scheme = scheme.len() > 1
        && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'));
    let numbers = !rest.is_empty()
        && rest
            .split(':')
            .all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    is_scheme && !numbers
}

/// Decodes `%XX` escapes. `None` for a bad escape or text that is not UTF-8 once decoded.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The file a link names, when it names one: a path (`src/a.rs`, `/abs/a.rs`), with an optional
/// line and column (`src/a.rs:12`, `src/a.rs:12:3`, `src/a.rs#L12`), or a `file://` URL on this
/// machine (percent escapes decoded). A link with any other scheme (`https:`, `mailto:`), an
/// in-page anchor (`#top`), a protocol-relative `//host` link, or text [`parse_location`] refuses
/// is `None`, and so stays an ordinary link. The text is untrusted: it is only ever parsed into a
/// [`Location`], which [`resolve_target`] then confines; it never reaches a command line whole.
pub fn file_link(href: &str) -> Option<Location> {
    let href = href.trim();
    if href.is_empty() || href.len() > MAX_LOCATION_BYTES || href.chars().any(char::is_control) {
        return None;
    }
    if let Some(rest) = href.strip_prefix("file://") {
        let path = rest.strip_prefix("localhost").unwrap_or(rest);
        if !path.starts_with('/') {
            return None; // another machine's file
        }
        let decoded = percent_decode(path)?;
        let location = parse_location(&decoded)?;
        return Path::new(&location.path).is_absolute().then_some(location);
    }
    if href.starts_with('#') || href.starts_with("//") || has_scheme(href) {
        return None;
    }
    parse_location(href)
}

/// Whether inline code text (`` `src/a.rs:12` ``) is a file location worth making a link of. It
/// is stricter than [`file_link`], since code spans hold every kind of text: no spaces, and
/// either a line number or a `/` in the path, and a last component with an extension that has a
/// letter in it (so `a.rs:12` and `src/lib.rs` are files, `localhost:8080` and `and/or` are not).
pub fn code_location(text: &str) -> Option<Location> {
    if text.chars().any(char::is_whitespace) || text.contains("://") {
        return None;
    }
    let location = file_link(text)?;
    let name = location.path.rsplit('/').next().unwrap_or_default();
    let has_extension = name.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty() && ext.chars().any(|c| c.is_ascii_alphabetic())
    });
    (has_extension && (location.line.is_some() || location.path.contains('/'))).then_some(location)
}

/// The file and line a read's tool input names, for the card's "Open" button: the path from
/// `file_path`, `AbsolutePath` or `Path`, and the line from `line`, `offset`, `StartLine` or
/// `start_line`. A line of 0 is 1; a huge one is clamped. `None` when no file is named: a bare
/// `path` is where a search or a listing looked, a folder more often than a file.
pub fn read_location(input: &serde_json::Value) -> Option<Location> {
    let text = |key: &str| {
        input
            .get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.trim().is_empty())
    };
    let path = ["file_path", "AbsolutePath", "Path"]
        .into_iter()
        .find_map(text)?;
    if path.len() > MAX_LOCATION_BYTES || path.chars().any(char::is_control) {
        return None;
    }
    let line = ["line", "offset", "StartLine", "start_line"]
        .into_iter()
        .find_map(|key| input.get(key).and_then(serde_json::Value::as_u64))
        .map(|n| n.clamp(1, u64::from(MAX_LINE)) as u32);
    Some(Location {
        path: path.to_owned(),
        line,
        column: None,
    })
}

/// The file to open, resolved and judged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The canonical absolute path (symlinks followed).
    pub abs: PathBuf,
    /// Whether it is inside the canonical folder.
    pub inside: bool,
    /// Whether a name on its way is one an edit or read always asks about
    /// ([`agent_core::sensitive`]).
    pub sensitive: bool,
}

/// Resolves `raw` (absolute, or relative to `folder`) to the file it leads to. Follows every
/// symlink, so a link that leads out of the folder is outside. The file must exist and be a
/// regular file. Needs no git. Blocking.
pub fn resolve_target(folder: &Path, raw: &str) -> Result<Target, String> {
    if raw.is_empty() || raw.len() > MAX_LOCATION_BYTES || raw.contains('\0') {
        return Err("That is not a file path".to_owned());
    }
    let root = folder
        .canonicalize()
        .map_err(|e| format!("Could not resolve the thread's folder: {e}"))?;
    let abs = root.join(raw).canonicalize().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => "The file does not exist".to_owned(),
        _ => format!("Could not resolve the file: {e}"),
    })?;
    let meta = std::fs::metadata(&abs).map_err(|e| format!("Could not read the file: {e}"))?;
    if !meta.is_file() {
        return Err("That is not a regular file".to_owned());
    }
    let inside = abs.starts_with(&root);
    // Inside, the folder's own place in the filesystem is no business of the list; outside,
    // every component of the whole path counts.
    let seen = if inside {
        abs.strip_prefix(&root).unwrap_or(&abs)
    } else {
        &abs
    };
    let names: Vec<String> = seen
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(n) => Some(n.to_string_lossy().to_lowercase()),
            _ => None,
        })
        .collect();
    Ok(Target {
        abs,
        inside,
        sensitive: agent_core::sensitive::names_sensitive(&names),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(argv: &[&str]) -> EditorTool {
        EditorTool {
            name: "T".into(),
            argv: argv.iter().map(|s| (*s).to_owned()).collect(),
            terminal: false,
        }
    }

    fn values() -> Values {
        Values {
            file: "/home/me/my repo/src/a b.rs".into(),
            line: 42,
            column: 7,
            dir: "/home/me/my repo".into(),
        }
    }

    #[test]
    fn presets_are_valid_and_found_by_argv() {
        for (i, p) in PRESETS.iter().enumerate() {
            validate(&p.tool()).unwrap_or_else(|e| panic!("{}: {e}", p.name));
            assert_eq!(preset_index(&p.tool()), Some(i));
            assert!(!p.tool().terminal);
        }
        assert_eq!(preset_index(&tool(&["code", "{file}"])), None);
        let names: Vec<_> = PRESETS.iter().map(|p| p.name).collect();
        assert_eq!(names, ["VS Code", "Zed", "JetBrains", "GNOME Text Editor"]);
    }

    #[test]
    fn every_placeholder_is_known_and_filled() {
        for name in PLACEHOLDERS {
            let t = tool(&["ed", &format!("{{{name}}}"), "{file}"]);
            assert!(validate(&t).is_ok(), "{name}");
        }
        let argv = substitute(
            &tool(&["ed", "{file}", "{line}", "{column}", "{dir}"]),
            &values(),
        )
        .expect("ok");
        assert_eq!(argv[2], OsString::from("42"));
        assert_eq!(argv[3], OsString::from("7"));
        assert_eq!(argv[4], OsString::from("/home/me/my repo"));
    }

    #[test]
    fn placeholders_fill_per_element_and_spaces_stay_one_argument() {
        let argv = substitute(&PRESETS[0].tool(), &values()).expect("ok");
        assert_eq!(argv.len(), 3);
        assert_eq!(argv[0], OsString::from("code"));
        assert_eq!(argv[2], OsString::from("/home/me/my repo/src/a b.rs:42:7"));
        let argv = substitute(&PRESETS[2].tool(), &values()).expect("ok");
        assert_eq!(argv.len(), 6);
        assert_eq!(argv[5], OsString::from("/home/me/my repo/src/a b.rs"));
        // A zero line or column reads as 1.
        let v = Values {
            line: 0,
            column: 0,
            ..values()
        };
        let argv = substitute(&PRESETS[1].tool(), &v).expect("ok");
        assert_eq!(argv[1], OsString::from("/home/me/my repo/src/a b.rs:1:1"));
    }

    #[test]
    fn metacharacters_are_plain_text() {
        let v = Values {
            file: "/w/$(touch pwned);`x` && y|z\n'\".rs".into(),
            ..values()
        };
        let argv = substitute(&PRESETS[0].tool(), &v).expect("ok");
        assert_eq!(argv.len(), 3, "a hostile path never splits");
        assert_eq!(
            argv[2],
            OsString::from("/w/$(touch pwned);`x` && y|z\n'\".rs:42:7")
        );
        // A relative file or dir is refused rather than read as an option.
        let bad = Values {
            file: "-rf".into(),
            ..values()
        };
        assert!(substitute(&PRESETS[0].tool(), &bad).is_err());
        let bad = Values {
            dir: "rel".into(),
            ..values()
        };
        assert!(substitute(&PRESETS[0].tool(), &bad).is_err());
    }

    #[test]
    fn validate_rejects_unknown_placeholders_a_placeholder_program_and_no_file() {
        for bad in [
            &["code", "{fiel}"][..],
            &["code", "{file", "x"],
            &["code", "file}"],
            &["code", "{}"],
            &["code", "-g"],
            &["code"],
            &["{file}", "x"],
            &["{dir}ed", "{file}"],
            &["-x", "{file}"],
            &[""],
            &[],
        ] {
            assert!(validate(&tool(bad)).is_err(), "{bad:?}");
        }
        // The old tool's placeholders are not this one's.
        assert!(validate(&tool(&["ed", "{new}", "{file}"])).is_err());
        assert!(validate(&tool(&["ed", "{{literal}}", "{file}"])).is_ok());
        let mut nameless = tool(&["ed", "{file}"]);
        nameless.name = " ".into();
        assert!(validate(&nameless).is_err());
    }

    #[test]
    fn parse_location_grammar() {
        let loc = |p: &str, l: Option<u32>, c: Option<u32>| {
            Some(Location {
                path: p.to_owned(),
                line: l,
                column: c,
            })
        };
        assert_eq!(parse_location("src/a.rs"), loc("src/a.rs", None, None));
        assert_eq!(
            parse_location("src/a.rs:12"),
            loc("src/a.rs", Some(12), None)
        );
        assert_eq!(
            parse_location("src/a.rs:12:3"),
            loc("src/a.rs", Some(12), Some(3))
        );
        assert_eq!(
            parse_location("src/a.rs#L12"),
            loc("src/a.rs", Some(12), None)
        );
        assert_eq!(
            parse_location("  src/a.rs:5 "),
            loc("src/a.rs", Some(5), None)
        );
        // Zero becomes 1; huge numbers are clamped.
        assert_eq!(parse_location("a.rs:0:0"), loc("a.rs", Some(1), Some(1)));
        assert_eq!(parse_location("a.rs#L0"), loc("a.rs", Some(1), None));
        assert_eq!(
            parse_location("a.rs:99999999999999999999:99999999999"),
            loc("a.rs", Some(MAX_LINE), Some(MAX_COLUMN))
        );
        // A Windows drive is a drive, not a line; a path on one still takes a line.
        assert_eq!(parse_location("C:12"), loc("C:12", None, None));
        assert_eq!(
            parse_location("C:\\a\\b.rs"),
            loc("C:\\a\\b.rs", None, None)
        );
        assert_eq!(
            parse_location("C:\\a\\b.rs:7"),
            loc("C:\\a\\b.rs", Some(7), None)
        );
        // Not a number after the colon: all path.
        assert_eq!(parse_location("a.rs:x"), loc("a.rs:x", None, None));
        assert_eq!(parse_location("a.rs:12:x"), loc("a.rs:12:x", None, None));
        assert_eq!(parse_location("a.rs#Lx"), loc("a.rs#Lx", None, None));
        // Nothing, no path, a control character, or too much text.
        for bad in ["", "   ", "a\n.rs:3", "a\0b"] {
            assert_eq!(parse_location(bad), None, "{bad:?}");
        }
        assert_eq!(parse_location(&"x".repeat(MAX_LOCATION_BYTES + 1)), None);
        assert!(parse_location(&"x".repeat(MAX_LOCATION_BYTES)).is_some());
    }

    fn at(path: &str, line: Option<u32>, column: Option<u32>) -> Option<Location> {
        Some(Location {
            path: path.to_owned(),
            line,
            column,
        })
    }

    #[test]
    fn file_links_are_paths_and_file_urls_and_every_other_scheme_is_not() {
        assert_eq!(file_link("src/a.rs"), at("src/a.rs", None, None));
        assert_eq!(file_link("src/a.rs:12"), at("src/a.rs", Some(12), None));
        assert_eq!(file_link("a.rs:12:3"), at("a.rs", Some(12), Some(3)));
        assert_eq!(file_link("/w/a.rs#L7"), at("/w/a.rs", Some(7), None));
        assert_eq!(file_link("../up.rs"), at("../up.rs", None, None));
        assert_eq!(
            file_link("file:///w/my%20dir/a.rs:4"),
            at("/w/my dir/a.rs", Some(4), None)
        );
        assert_eq!(
            file_link("file://localhost/w/a.rs"),
            at("/w/a.rs", None, None)
        );
        for not_a_file in [
            "https://example.com/a.rs:12",
            "http://example.com",
            "mailto:someone@example.com",
            "javascript:alert(1)",
            "ssh:host",
            "#section",
            "//example.com/a.rs",
            "file://example.com/w/a.rs",
            "file:relative.rs",
            "file:///w/%zz.rs",
            "file:///w/%ff.rs",
            "",
            "a\nb.rs",
        ] {
            assert_eq!(file_link(not_a_file), None, "{not_a_file:?}");
        }
        assert_eq!(file_link(&"x".repeat(MAX_LOCATION_BYTES + 1)), None);
    }

    #[test]
    fn only_code_that_names_a_file_becomes_a_link() {
        assert_eq!(code_location("src/a.rs:12"), at("src/a.rs", Some(12), None));
        assert_eq!(code_location("a.rs:12:3"), at("a.rs", Some(12), Some(3)));
        assert_eq!(code_location("src/lib.rs"), at("src/lib.rs", None, None));
        assert_eq!(
            code_location("Makefile.am:3"),
            at("Makefile.am", Some(3), None)
        );
        for not_a_file in [
            "Cargo.toml",
            "localhost:8080",
            "127.0.0.1:8080",
            "and/or",
            "a.rs:12 b.rs",
            "https://example.com/a.rs",
            ".gitignore:2",
            "foo()",
            "x = 1",
        ] {
            assert_eq!(code_location(not_a_file), None, "{not_a_file:?}");
        }
    }

    #[test]
    fn a_reads_input_names_its_file_and_line() {
        use serde_json::json;
        assert_eq!(
            read_location(&json!({"file_path": "src/a.rs", "offset": 40, "limit": 10})),
            at("src/a.rs", Some(40), None)
        );
        assert_eq!(
            read_location(&json!({"AbsolutePath": "/w/a.rs", "StartLine": 0})),
            at("/w/a.rs", Some(1), None)
        );
        assert_eq!(
            read_location(&json!({"Path": "/w/a.rs", "start_line": 99_999_999_999u64})),
            at("/w/a.rs", Some(MAX_LINE), None)
        );
        // A search's or a listing's `path` is where it looked, not a file to open.
        assert_eq!(read_location(&json!({"path": "src"})), None);
        assert_eq!(read_location(&json!({"pattern": "x", "path": "src"})), None);
        assert_eq!(read_location(&json!({"file_path": ""})), None);
        assert_eq!(read_location(&json!({"file_path": 3})), None);
        assert_eq!(read_location(&json!({"file_path": "a\u{0}b"})), None);
        assert_eq!(read_location(&serde_json::Value::Null), None);
    }

    #[test]
    fn resolve_target_confines_symlinks_and_reports_outside_and_sensitive() {
        use std::os::unix::fs::symlink;
        let work = tempfile::tempdir().expect("work");
        let elsewhere = tempfile::tempdir().expect("elsewhere");
        let root = work.path().canonicalize().expect("canon");
        std::fs::create_dir_all(root.join("src")).expect("src");
        std::fs::create_dir_all(root.join(".git/hooks")).expect("git");
        std::fs::write(root.join("src/a b.rs"), "x").expect("file");
        std::fs::write(root.join(".git/hooks/pre-commit"), "x").expect("hook");
        std::fs::write(root.join("AGENTS.md"), "x").expect("agents");
        let outside = elsewhere
            .path()
            .canonicalize()
            .expect("canon")
            .join("o.txt");
        std::fs::write(&outside, "x").expect("outside");
        symlink(&outside, root.join("leaves")).expect("link");
        symlink(root.join("src/a b.rs"), root.join("stays")).expect("link");
        symlink(root.join(".git/hooks/pre-commit"), root.join("innocent")).expect("link");

        // Inside: relative and absolute, spaces and all.
        let t = resolve_target(&root, "src/a b.rs").expect("inside");
        assert_eq!(t.abs, root.join("src/a b.rs"));
        assert!(t.inside && !t.sensitive);
        let t = resolve_target(&root, root.join("src/a b.rs").to_str().expect("utf8"))
            .expect("absolute");
        assert!(t.inside);
        // `..` that comes back in is inside, one that leaves is outside.
        assert!(
            resolve_target(&root, "src/../src/a b.rs")
                .expect("ok")
                .inside
        );
        let t = resolve_target(&root, outside.to_str().expect("utf8")).expect("outside");
        assert!(!t.inside);
        // A link that stays is inside and resolves to its target; one that leaves is outside.
        let t = resolve_target(&root, "stays").expect("stays");
        assert!(t.inside);
        assert_eq!(t.abs, root.join("src/a b.rs"));
        let t = resolve_target(&root, "leaves").expect("leaves");
        assert!(!t.inside);
        assert_eq!(t.abs, outside);
        // Sensitive: by its own name, and by where an innocent-looking link leads.
        assert!(resolve_target(&root, "AGENTS.md").expect("ok").sensitive);
        assert!(
            resolve_target(&root, ".git/hooks/pre-commit")
                .expect("ok")
                .sensitive
        );
        let t = resolve_target(&root, "innocent").expect("ok");
        assert!(t.inside && t.sensitive);
        // Missing, a directory, and junk are errors.
        assert!(resolve_target(&root, "nope.rs").is_err());
        assert!(resolve_target(&root, "src").is_err());
        assert!(resolve_target(&root, "").is_err());
        assert!(resolve_target(&root, "a\0b").is_err());
        assert!(resolve_target(Path::new("/nonexistent/folder"), "a").is_err());
    }
}

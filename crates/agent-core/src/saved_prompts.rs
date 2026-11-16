//! Saved prompts: named prompt bodies the composer's `/` typeahead expands into the draft.
//!
//! Pure: no GTK, no I/O. The app reads the prompts from its settings and the values (the thread's
//! folder, a file, a selection) from its widgets, and hands both here. Expanding never sends
//! anything: the result goes into the composer, where the user reads and edits it.
//!
//! A body may hold placeholders: `{file}`, `{branch}`, `{selection}` and `{folder}`. `{{` and `}}`
//! are literal braces, and any other `{word}` stays as it is. A placeholder with no value is left
//! in place, never blanked, and reported so the composer can select it for the user to fill.
//!
//! Every value is redacted ([`crate::redact::redact`]) as it is given to [`Values`], before it
//! is cut to a cap, so no text reaches a draft through a placeholder unmasked. The body itself
//! is the user's own words and is kept exactly.

use std::ops::Range;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::commands::{Builtin, CompletionItem, CompletionKind};
use crate::redact::redact;

/// The most saved prompts that are offered; any past it are reported and ignored.
pub const MAX_PROMPTS: usize = 200;
/// The longest body, in bytes.
pub const MAX_BODY_BYTES: usize = 16 * 1024;
/// The longest name, in characters.
pub const MAX_NAME_CHARS: usize = 48;
/// The most a `{selection}` puts in a draft, in bytes (after redaction).
pub const SELECTION_MAX_BYTES: usize = 8 * 1024;
/// The longest description the typeahead shows (the body's first line), in characters.
const DESCRIPTION_CHARS: usize = 80;

/// One saved prompt as the settings file keeps it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedPrompt {
    /// A stable identity for an editor; may be empty in a hand-written file.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// What follows the `/`. Compared without regard to case.
    pub name: String,
    /// The text the prompt expands to.
    pub body: String,
    /// Offered only in threads whose folder is this one or inside it. `None`: everywhere.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------------------------

/// Why `name` cannot name a saved prompt, if it cannot. `builtins` are the app's own commands:
/// a name or alias of one is refused, since a saved prompt never stands in for a built-in.
pub fn validate_name(name: &str, builtins: &[Builtin]) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("A saved prompt needs a name".to_owned());
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(format!(
            "A name is at most {MAX_NAME_CHARS} characters long"
        ));
    }
    if !name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
    {
        return Err("A name starts with a letter or a digit".to_owned());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("A name holds only letters, digits, - and _".to_owned());
    }
    let lower = name.to_ascii_lowercase();
    if let Some(b) = builtins
        .iter()
        .find(|b| b.name == lower || b.aliases.contains(&lower.as_str()))
    {
        return Err(format!("/{lower} is the app's own command (/{})", b.name));
    }
    Ok(())
}

/// Why `body` cannot be a saved prompt's text, if it cannot.
pub fn validate_body(body: &str) -> Result<(), String> {
    if body.trim().is_empty() {
        return Err("A saved prompt needs some text".to_owned());
    }
    if body.len() > MAX_BODY_BYTES {
        return Err(format!("The text is over {} KB", MAX_BODY_BYTES / 1024));
    }
    Ok(())
}

/// Why `folder` cannot limit a prompt, if it cannot. It must be an absolute path (a leading
/// `~/` is the caller's to expand before this is asked).
pub fn validate_folder(folder: &str) -> Result<(), String> {
    if folder.trim().is_empty() || !folder.starts_with('/') {
        return Err("The folder must be an absolute path".to_owned());
    }
    Ok(())
}

/// Each prompt's problem, in order: `None` for one that is offered. A later prompt with the
/// same name and folder as an earlier one is the duplicate, and prompts past [`MAX_PROMPTS`]
/// are refused. `expand` turns a stored folder into an absolute path (`~/` expanded).
pub fn problems(
    prompts: &[SavedPrompt],
    builtins: &[Builtin],
    expand: impl Fn(&str) -> String,
) -> Vec<Option<String>> {
    let mut seen: Vec<(String, Option<PathBuf>)> = Vec::new();
    prompts
        .iter()
        .enumerate()
        .map(|(i, p)| {
            if i >= MAX_PROMPTS {
                return Some(format!(
                    "Only the first {MAX_PROMPTS} saved prompts are used"
                ));
            }
            let folder = p.folder.as_deref().map(&expand);
            let checked = validate_name(&p.name, builtins)
                .and_then(|()| validate_body(&p.body))
                .and_then(|()| folder.as_deref().map_or(Ok(()), validate_folder));
            if let Err(why) = checked {
                return Some(why);
            }
            // The folder by its path components, so `/a/b` and `/a/b/` are one folder.
            let key = (
                p.name.trim().to_ascii_lowercase(),
                folder.map(|f| Path::new(&f).components().collect::<PathBuf>()),
            );
            if seen.contains(&key) {
                return Some(format!("/{} is already a saved prompt", key.0));
            }
            seen.push(key);
            None
        })
        .collect()
}

/// Whether a prompt limited to `folder` is offered in a thread in `dir`: `dir` is `folder` or
/// inside it, compared by whole path components (so `/a/b` does not cover `/a/bc`).
pub fn scope_matches(folder: &Path, dir: &Path) -> bool {
    folder.is_absolute() && dir.starts_with(folder)
}

/// The prompts offered in a thread in `dir`, in the order the typeahead lists them: every valid
/// one in scope, a folder's own before a global one, and of several with one name only the
/// first (so a folder's prompt stands in for the global one of its name). `dir` is `None` when
/// the thread has no folder: only global prompts apply.
pub fn offered<'a>(
    prompts: &'a [SavedPrompt],
    builtins: &[Builtin],
    dir: Option<&Path>,
    expand: impl Fn(&str) -> String,
) -> Vec<&'a SavedPrompt> {
    let problems = problems(prompts, builtins, &expand);
    let usable = prompts
        .iter()
        .zip(problems)
        .filter_map(|(p, problem)| problem.is_none().then_some(p));
    let (mut scoped, mut global): (Vec<&SavedPrompt>, Vec<&SavedPrompt>) = (Vec::new(), Vec::new());
    for p in usable {
        match p.folder.as_deref() {
            None => global.push(p),
            Some(folder) => {
                let folder = expand(folder);
                if dir.is_some_and(|d| scope_matches(Path::new(&folder), d)) {
                    scoped.push(p);
                }
            }
        }
    }
    // The deepest folder first, so the nearest scope wins a name.
    scoped.sort_by_key(|p| {
        std::cmp::Reverse(
            p.folder
                .as_deref()
                .map_or(0, |f| Path::new(&expand(f)).components().count()),
        )
    });
    let mut out: Vec<&SavedPrompt> = Vec::new();
    for p in scoped.into_iter().chain(global) {
        let name = p.name.trim().to_ascii_lowercase();
        if !out
            .iter()
            .any(|o| o.name.trim().eq_ignore_ascii_case(&name))
        {
            out.push(p);
        }
    }
    out
}

/// A prompt as a typeahead row: its name, and its body's first non-blank line as the
/// description.
pub fn completion_item(prompt: &SavedPrompt) -> CompletionItem {
    let name = prompt.name.trim().to_ascii_lowercase();
    let first = prompt
        .body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let mut description: String = first.chars().take(DESCRIPTION_CHARS).collect();
    if first.chars().count() > DESCRIPTION_CHARS {
        description.push('…');
    }
    CompletionItem {
        kind: CompletionKind::Saved,
        insert_text: format!("/{name} "),
        name,
        description,
        hint: None,
    }
}

// ---------------------------------------------------------------------------------------------
// Placeholders
// ---------------------------------------------------------------------------------------------

/// A value a body can ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placeholder {
    /// The first `@path` in the draft, else the file in focus in the diff panel.
    File,
    /// The current branch of the thread's folder.
    Branch,
    /// The text selected in the thread's transcript, else in the composer.
    Selection,
    /// The thread's folder.
    Folder,
}

impl Placeholder {
    /// Every placeholder, in the order the documentation lists them.
    pub const ALL: [Placeholder; 4] = [
        Placeholder::File,
        Placeholder::Branch,
        Placeholder::Selection,
        Placeholder::Folder,
    ];

    /// The word between the braces.
    pub fn name(self) -> &'static str {
        match self {
            Placeholder::File => "file",
            Placeholder::Branch => "branch",
            Placeholder::Selection => "selection",
            Placeholder::Folder => "folder",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.name() == name)
    }
}

/// A piece of a parsed body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// Literal text, escapes already undone.
    Text(String),
    Placeholder(Placeholder),
}

/// `body` as literal text and placeholders. `{{` is `{` and `}}` is `}`; a `{word}` that names
/// no placeholder, and a brace that opens nothing, stay literal.
pub fn parse(body: &str) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    let mut text = String::new();
    let mut rest = body;
    while let Some(c) = rest.chars().next() {
        if let Some(after) = rest.strip_prefix("{{") {
            text.push('{');
            rest = after;
            continue;
        }
        if let Some(after) = rest.strip_prefix("}}") {
            text.push('}');
            rest = after;
            continue;
        }
        if c == '{' {
            let inner = &rest[1..];
            if let Some(end) = inner.find('}') {
                if let Some(p) = Placeholder::from_name(&inner[..end]) {
                    if !text.is_empty() {
                        out.push(Segment::Text(std::mem::take(&mut text)));
                    }
                    out.push(Segment::Placeholder(p));
                    rest = &inner[end + 1..];
                    continue;
                }
            }
        }
        text.push(c);
        rest = &rest[c.len_utf8()..];
    }
    if !text.is_empty() {
        out.push(Segment::Text(text));
    }
    out
}

/// The values placeholders are filled with. Each is redacted when it is set, so nothing given
/// here reaches a draft unmasked; a blank value counts as none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Values {
    file: Option<String>,
    branch: Option<String>,
    selection: Option<String>,
    folder: Option<String>,
    selection_cut: bool,
    selection_dropped: bool,
}

impl Values {
    pub fn with_file(mut self, file: &str) -> Self {
        self.file = prepared(file);
        self
    }

    pub fn with_branch(mut self, branch: &str) -> Self {
        self.branch = prepared(branch);
        self
    }

    pub fn with_folder(mut self, folder: &str) -> Self {
        self.folder = prepared(folder);
        self
    }

    /// The selection, redacted and then cut to [`SELECTION_MAX_BYTES`] (see [`cap_selection`]).
    pub fn with_selection(mut self, raw: &str) -> Self {
        let (text, cut) = cap_selection(raw, SELECTION_MAX_BYTES);
        let kept = !text.trim().is_empty();
        self.selection_cut = cut && kept;
        // Cut to nothing (one unbroken run past the bound): not filled, but the caller is told.
        self.selection_dropped = cut && !kept;
        self.selection = kept.then_some(text);
        self
    }

    /// The value for `p`, if there is one.
    pub fn get(&self, p: Placeholder) -> Option<&str> {
        match p {
            Placeholder::File => self.file.as_deref(),
            Placeholder::Branch => self.branch.as_deref(),
            Placeholder::Selection => self.selection.as_deref(),
            Placeholder::Folder => self.folder.as_deref(),
        }
    }
}

fn prepared(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| redact(value))
}

/// A placeholder left in the text for the user to fill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unresolved {
    pub placeholder: Placeholder,
    /// Byte range of `{name}` in [`Resolved::text`].
    pub range: Range<usize>,
}

/// A body with its placeholders filled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub text: String,
    /// Byte offset for the cursor: the first unresolved placeholder, else the end.
    pub cursor: usize,
    /// Placeholders with no value, in order, still written as `{name}`.
    pub unresolved: Vec<Unresolved>,
    /// Whether a `{selection}` was filled with a selection cut to its cap.
    pub selection_cut: bool,
    /// Whether a `{selection}` was left unfilled because nothing of the selection could be kept
    /// whole: one run with no blank or line break, past four times the cap.
    pub selection_dropped: bool,
}

/// Fills `segments` from `values`. A value goes in as it is: it is never scanned for
/// placeholders again. One with no value is written back as `{name}` and reported.
pub fn resolve(segments: &[Segment], values: &Values) -> Resolved {
    let mut text = String::new();
    let mut unresolved = Vec::new();
    let mut selection_cut = false;
    let mut selection_dropped = false;
    for segment in segments {
        match segment {
            Segment::Text(t) => text.push_str(t),
            Segment::Placeholder(p) => match values.get(*p) {
                Some(value) => {
                    text.push_str(value);
                    selection_cut |= *p == Placeholder::Selection && values.selection_cut;
                }
                None => {
                    selection_dropped |= *p == Placeholder::Selection && values.selection_dropped;
                    let start = text.len();
                    text.push('{');
                    text.push_str(p.name());
                    text.push('}');
                    unresolved.push(Unresolved {
                        placeholder: *p,
                        range: start..text.len(),
                    });
                }
            },
        }
    }
    let cursor = unresolved.first().map_or(text.len(), |u| u.range.start);
    Resolved {
        text,
        cursor,
        unresolved,
        selection_cut,
        selection_dropped,
    }
}

/// [`parse`] then [`resolve`].
pub fn expand(body: &str, values: &Values) -> Resolved {
    resolve(&parse(body), values)
}

/// Whether `body` asks for `p` at all (so a caller can skip reading a value nobody uses).
pub fn uses(body: &str, p: Placeholder) -> bool {
    parse(body)
        .iter()
        .any(|s| matches!(s, Segment::Placeholder(q) if *q == p))
}

/// The first `@path` mention in `draft`: an `@` at the start or after a blank, followed by a
/// path, without the punctuation a sentence puts after it.
pub fn first_file_mention(draft: &str) -> Option<String> {
    draft.split_whitespace().find_map(|word| {
        let path = word
            .strip_prefix('@')?
            .trim_end_matches(['.', ',', ';', ':', '!', '?', ')']);
        (!path.is_empty()).then(|| path.to_owned())
    })
}

/// `raw` as a selection may go into a draft: redacted, and its head kept to at most `cap` bytes.
/// Returns the text and whether anything was cut.
///
/// Redaction sees whole lines: before it runs, text past four times the cap is dropped back to
/// the last line break (or, on one long line, the last blank), so no secret is cut in half
/// where its pattern could no longer be recognised. The cap then cuts the redacted text, at a
/// character boundary and never inside a mask.
pub fn cap_selection(raw: &str, cap: usize) -> (String, bool) {
    let mut cut = false;
    let bound = cap.saturating_mul(4);
    let mut kept = raw;
    if kept.len() > bound {
        cut = true;
        let head = &kept[..floor_boundary(kept, bound)];
        let end = head
            .rfind('\n')
            .or_else(|| head.rfind(char::is_whitespace))
            .unwrap_or(0);
        kept = &head[..end];
    }
    let redacted = redact(kept);
    let mut text = redacted.as_str();
    if text.len() > cap {
        cut = true;
        let end = before_mask(text, floor_boundary(text, cap));
        text = &text[..end];
    }
    (text.trim_end().to_owned(), cut)
}

/// The largest character boundary of `s` at or before `at`.
fn floor_boundary(s: &str, at: usize) -> usize {
    let mut at = at.min(s.len());
    while !s.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// `end`, moved back to the start of a mask it would cut (`****` and up to four more characters
/// on its line, or a PEM marker), so a kept head never ends with half a mask.
fn before_mask(text: &str, end: usize) -> usize {
    const PEM: &str = "[REDACTED PEM BLOCK]";
    let window = end.saturating_sub(PEM.len() + 8);
    let mut snapped = end;
    for (start, _) in text[..end].char_indices().filter(|(i, _)| *i >= window) {
        let rest = &text[start..];
        let mask_end = if let Some(after) = rest.strip_prefix("****") {
            let tail: usize = after
                .chars()
                .take(4)
                .take_while(|c| *c != '\n' && !c.is_whitespace())
                .map(char::len_utf8)
                .sum();
            start + 4 + tail
        } else if rest.starts_with(PEM) {
            start + PEM.len()
        } else {
            continue;
        };
        if start < end && end < mask_end {
            snapped = snapped.min(start);
        }
    }
    snapped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::builtins;

    fn prompt(name: &str, body: &str, folder: Option<&str>) -> SavedPrompt {
        SavedPrompt {
            id: String::new(),
            name: name.to_owned(),
            body: body.to_owned(),
            folder: folder.map(str::to_owned),
        }
    }

    fn same(s: &str) -> String {
        s.to_owned()
    }

    #[test]
    fn parse_finds_each_placeholder_and_keeps_escaped_braces() {
        assert_eq!(
            parse("Review {file} on {branch}: {selection} in {folder}"),
            vec![
                Segment::Text("Review ".into()),
                Segment::Placeholder(Placeholder::File),
                Segment::Text(" on ".into()),
                Segment::Placeholder(Placeholder::Branch),
                Segment::Text(": ".into()),
                Segment::Placeholder(Placeholder::Selection),
                Segment::Text(" in ".into()),
                Segment::Placeholder(Placeholder::Folder),
            ]
        );
        // Escaped braces are literal, and so is a word that names no placeholder.
        assert_eq!(
            parse("{{x}} {{file}} {nope} {File} { file } {"),
            vec![Segment::Text("{x} {file} {nope} {File} { file } {".into())]
        );
        assert_eq!(parse("a }b"), vec![Segment::Text("a }b".into())]);
        assert_eq!(parse(""), Vec::<Segment>::new());
        // Multibyte text around a placeholder.
        assert_eq!(
            parse("é{file}日"),
            vec![
                Segment::Text("é".into()),
                Segment::Placeholder(Placeholder::File),
                Segment::Text("日".into()),
            ]
        );
    }

    #[test]
    fn resolve_leaves_unresolved_placeholders_in_place_and_reports_the_first() {
        let values = Values::default().with_folder("/w/app");
        let r = expand("In {folder}, check {file} and {branch}.", &values);
        assert_eq!(r.text, "In /w/app, check {file} and {branch}.");
        let first = &r.unresolved[0];
        assert_eq!(first.placeholder, Placeholder::File);
        assert_eq!(&r.text[first.range.clone()], "{file}");
        assert_eq!(r.cursor, first.range.start);
        assert_eq!(r.unresolved[1].placeholder, Placeholder::Branch);
        assert_eq!(&r.text[r.unresolved[1].range.clone()], "{branch}");
        // Everything filled: the cursor goes to the end and nothing is reported.
        let r = expand("{folder}", &values);
        assert_eq!((r.text.as_str(), r.cursor), ("/w/app", 6));
        assert!(r.unresolved.is_empty());
        // A blank value is no value: nothing is silently blanked.
        let r = expand("{file}", &Values::default().with_file("   "));
        assert_eq!(r.text, "{file}");
    }

    #[test]
    fn resolve_does_not_rescan_inserted_values() {
        let values = Values::default()
            .with_selection("see {branch} and {{x}}")
            .with_branch("main");
        let r = expand("{selection} / {branch}", &values);
        assert_eq!(r.text, "see {branch} and {{x}} / main");
        assert!(r.unresolved.is_empty());
    }

    #[test]
    fn every_value_is_redacted_before_it_reaches_the_text() {
        let secret = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        let values = Values::default()
            .with_selection(&format!("token {secret}"))
            .with_file(&format!("notes/{secret}.txt"))
            .with_branch(&format!("fix/{secret}"))
            .with_folder(&format!("/w/{secret}"));
        let r = expand("{selection}|{file}|{branch}|{folder}", &values);
        assert!(!r.text.contains(secret), "{}", r.text);
        assert!(!r.text.contains("ghp_abcd"), "{}", r.text);
        assert_eq!(r.text.matches("****6789").count(), 4, "{}", r.text);
        // The body is the user's own text and is kept as written.
        let r = expand(&format!("{secret} {{folder}}"), &Values::default());
        assert!(r.text.starts_with(secret));
    }

    #[test]
    fn a_selection_is_capped_at_8_kb_after_redaction() {
        // Under the cap: kept whole, not reported as cut.
        let small = "line one\nline two";
        let r = expand("{selection}", &Values::default().with_selection(small));
        assert_eq!(r.text, small);
        assert!(!r.selection_cut);

        // Over it: the head is kept, within the cap, and the cut is reported.
        let big = "word ".repeat(4_000);
        let r = expand("Q: {selection}", &Values::default().with_selection(&big));
        assert!(r.selection_cut);
        assert!(r.text.len() <= "Q: ".len() + SELECTION_MAX_BYTES);
        assert!(r.text.starts_with("Q: word word"));

        // A cut never splits a character.
        let wide = "日".repeat(SELECTION_MAX_BYTES);
        let (text, cut) = cap_selection(&wide, SELECTION_MAX_BYTES);
        assert!(cut);
        assert!(text.len() <= SELECTION_MAX_BYTES && text.chars().all(|c| c == '日'));

        // One unbroken run past the bound keeps nothing: left unfilled, and said to be.
        let run = "x".repeat(SELECTION_MAX_BYTES * 4 + 1);
        assert_eq!(
            cap_selection(&run, SELECTION_MAX_BYTES),
            (String::new(), true)
        );
        let r = expand("Q: {selection}", &Values::default().with_selection(&run));
        assert_eq!(r.text, "Q: {selection}");
        assert!(r.selection_dropped && !r.selection_cut);
        let r = expand("{folder}", &Values::default().with_selection(&run));
        assert!(!r.selection_dropped);
        let r = expand("{selection}", &Values::default().with_selection(small));
        assert!(!r.selection_dropped);

        // Not asked for: a cut selection is not reported.
        let r = expand("{folder}", &Values::default().with_selection(&big));
        assert!(!r.selection_cut);
    }

    #[test]
    fn a_capped_selection_never_keeps_part_of_a_secret() {
        let secret = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        // A secret straddling the cap: masked first, and the mask is not cut in half.
        let filler = "a".repeat(SELECTION_MAX_BYTES - 6);
        let raw = format!("{filler} {secret} tail");
        let (text, cut) = cap_selection(&raw, SELECTION_MAX_BYTES);
        assert!(cut);
        assert!(!text.contains("ghp_"), "{}", &text[text.len() - 20..]);
        assert!(!text.ends_with('*'), "{}", &text[text.len() - 20..]);

        // A secret past the raw bound on one long line: dropped back to a blank, never kept as
        // a fragment redaction could no longer recognise.
        let pad = "b".repeat(SELECTION_MAX_BYTES * 4 - 20);
        let raw = format!("{pad} {secret}");
        let (text, cut) = cap_selection(&raw, SELECTION_MAX_BYTES);
        assert!(cut);
        assert!(!text.contains("ghp_"));

        // The same past a line break: the partial line goes.
        let lines = format!("{}\nkey {secret}", "c".repeat(SELECTION_MAX_BYTES * 4 - 10));
        let (text, _) = cap_selection(&lines, SELECTION_MAX_BYTES);
        assert!(!text.contains("ghp_"));
    }

    #[test]
    fn validate_name_refuses_builtins_aliases_duplicates_and_whitespace() {
        let b = builtins();
        assert!(validate_name("review", b).is_ok());
        assert!(validate_name("fix-bug_2", b).is_ok());
        for bad in [
            "",
            "  ",
            "two words",
            "tab\tname",
            "-lead",
            "_lead",
            "slash/name",
            "é",
            "/review",
        ] {
            assert!(validate_name(bad, b).is_err(), "{bad:?}");
        }
        assert!(validate_name(&"x".repeat(MAX_NAME_CHARS + 1), b).is_err());
        // A built-in's name or alias, in any case.
        for taken in ["model", "MODEL", "compress", "new", "Help"] {
            let err = validate_name(taken, b).expect_err(taken);
            assert!(err.contains("app's own command"), "{err}");
        }
        // Duplicates are the later row's problem, by name and folder, regardless of case.
        let prompts = [
            prompt("review", "a", None),
            prompt("Review", "b", None),
            prompt("review", "c", Some("/w")),
            prompt("model", "d", None),
            prompt("empty", "  ", None),
            prompt("rel", "e", Some("w/rel")),
        ];
        let p = problems(&prompts, b, same);
        assert!(p[0].is_none() && p[2].is_none(), "{p:?}");
        assert!(
            p[1].as_deref().is_some_and(|e| e.contains("already")),
            "{p:?}"
        );
        assert!(p[3].is_some() && p[4].is_some() && p[5].is_some(), "{p:?}");
        // One folder however it is written: a trailing slash, a `.`, or `~/` once expanded.
        let folders = [
            prompt("fix", "a", Some("/a/b")),
            prompt("fix", "b", Some("/a/b/")),
            prompt("FIX", "c", Some("/a/./b")),
            prompt("fix", "d", Some("~/b")),
            prompt("fix", "e", Some("/a/bc")),
        ];
        let p = problems(&folders, b, |f| f.replacen('~', "/a", 1));
        assert!(p[0].is_none() && p[4].is_none(), "{p:?}");
        assert!(p[1..4].iter().all(Option::is_some), "{p:?}");
        assert!(validate_body(&"x".repeat(MAX_BODY_BYTES + 1)).is_err());
        // Past the count cap.
        let many: Vec<SavedPrompt> = (0..=MAX_PROMPTS)
            .map(|i| prompt(&format!("p{i}"), "x", None))
            .collect();
        let p = problems(&many, b, same);
        assert!(p[..MAX_PROMPTS].iter().all(Option::is_none));
        assert!(p[MAX_PROMPTS].is_some());
    }

    #[test]
    fn scope_matches_a_folder_and_its_children_but_not_a_sibling_prefix() {
        let folder = Path::new("/a/b");
        assert!(scope_matches(folder, Path::new("/a/b")));
        assert!(scope_matches(folder, Path::new("/a/b/c/d")));
        assert!(!scope_matches(folder, Path::new("/a/bc")));
        assert!(!scope_matches(folder, Path::new("/a")));
        assert!(!scope_matches(Path::new("a/b"), Path::new("a/b")));
    }

    #[test]
    fn a_folders_prompt_comes_first_and_stands_in_for_the_global_one() {
        let prompts = [
            prompt("review", "global", None),
            prompt("review", "here", Some("/w/app")),
            prompt("review", "nearer", Some("/w/app/sub")),
            prompt("notes", "elsewhere", Some("/other")),
            prompt("tidy", "global tidy", None),
            prompt("model", "never", None),
        ];
        let body = |dir: Option<&str>| -> Vec<&str> {
            offered(&prompts, builtins(), dir.map(Path::new), same)
                .iter()
                .map(|p| p.body.as_str())
                .collect()
        };
        assert_eq!(body(Some("/w/app/sub/x")), ["nearer", "global tidy"]);
        assert_eq!(body(Some("/w/app")), ["here", "global tidy"]);
        assert_eq!(body(Some("/w")), ["global", "global tidy"]);
        assert_eq!(body(None), ["global", "global tidy"]);
        // `~/` folders are the caller's to expand.
        let home = [prompt("h", "home", Some("~/w"))];
        let expanded = offered(&home, builtins(), Some(Path::new("/home/u/w")), |f| {
            f.replacen('~', "/home/u", 1)
        });
        assert_eq!(expanded.len(), 1);
    }

    #[test]
    fn a_completion_item_names_the_prompt_and_its_first_line() {
        let item = completion_item(&prompt("Review", "\n  Look at {file}\nmore", None));
        assert_eq!(item.kind, CompletionKind::Saved);
        assert_eq!(item.name, "review");
        assert_eq!(item.description, "Look at {file}");
        assert_eq!(item.insert_text, "/review ");
        let long = completion_item(&prompt("l", &"x".repeat(200), None));
        assert_eq!(long.description.chars().count(), DESCRIPTION_CHARS + 1);
    }

    #[test]
    fn the_first_file_mention_is_the_file() {
        assert_eq!(
            first_file_mention("/review look at @src/main.rs, then @b.rs").as_deref(),
            Some("src/main.rs")
        );
        assert_eq!(first_file_mention("mail a@b.c and @ alone"), None);
        assert_eq!(first_file_mention("(@x.rs)").as_deref(), None);
        assert_eq!(
            first_file_mention("see @README.md.").as_deref(),
            Some("README.md")
        );
    }

    #[test]
    fn uses_reports_only_real_placeholders() {
        assert!(uses("x {branch}", Placeholder::Branch));
        assert!(!uses("x {{branch}}", Placeholder::Branch));
        assert!(!uses("x {selection}", Placeholder::Branch));
    }

    #[test]
    fn a_saved_prompt_round_trips_and_omits_empty_fields() {
        let p = prompt("review", "Look", None);
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, r#"{"name":"review","body":"Look"}"#);
        let back: SavedPrompt = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
        let scoped: SavedPrompt =
            serde_json::from_str(r#"{"id":"1","name":"n","body":"b","folder":"/w"}"#).unwrap();
        assert_eq!(scoped.folder.as_deref(), Some("/w"));
    }
}

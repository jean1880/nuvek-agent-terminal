//! Pure logic for the diff panel: which two trees to compare, how to read
//! `git diff --numstat -z`, how to colour each line of a unified diff, and how
//! to cap a diff too large to show.
//!
//! Nothing here runs git; `crate::git` does, and the window renders.

pub use agent_core::review::{Located, Side};

/// What the panel compares against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiffBase {
    /// HEAD against the working tree as it is now (untracked files included).
    #[default]
    Uncommitted,
    /// Target branch (main/master) against the working tree as it is now.
    TargetBranch,
    /// The tab's latest checkpoint against the one before it.
    LastTurn,
    /// Where the tab's first checkpoint started against the working tree now.
    ThisTab,
}

impl DiffBase {
    /// Dropdown order. Guarded by `all_lists_every_base_in_dropdown_order`.
    pub const ALL: [DiffBase; 4] = [
        DiffBase::Uncommitted,
        DiffBase::TargetBranch,
        DiffBase::LastTurn,
        DiffBase::ThisTab,
    ];

    pub fn label(self) -> &'static str {
        match self {
            DiffBase::Uncommitted => "Uncommitted",
            DiffBase::TargetBranch => "Target branch",
            DiffBase::LastTurn => "Last turn",
            DiffBase::ThisTab => "This tab",
        }
    }

    /// The name a review draft keeps for the base it was made on (`review_drafts.diff_base`).
    pub fn key(self) -> &'static str {
        match self {
            DiffBase::Uncommitted => "uncommitted",
            DiffBase::TargetBranch => "target_branch",
            DiffBase::LastTurn => "last_turn",
            DiffBase::ThisTab => "this_tab",
        }
    }
}

/// A checkpoint as the panel needs it: its commit and where it started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointCommit {
    pub commit: String,
    /// `None` for a checkpoint taken before the repository's first commit.
    pub parent: Option<String>,
}

/// Why a base cannot be shown yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    NoCheckpoints,
    NoTargetBranch,
}

/// The two tree-ishes to diff for `base`.
///
/// `checkpoints` are the tab's, oldest first; `current` is a fresh snapshot
/// tree of the working tree; `empty_tree` stands in for a missing parent (an
/// unborn HEAD), since it differs between SHA-1 and SHA-256 repositories and
/// must be asked of git rather than hard-coded.
pub fn diff_range(
    base: DiffBase,
    head: Option<&str>,
    checkpoints: &[CheckpointCommit],
    current: &str,
    empty_tree: &str,
    target_branch: Option<&str>,
) -> Result<(String, String), Unavailable> {
    let or_empty = |rev: Option<&str>| rev.unwrap_or(empty_tree).to_string();
    match base {
        DiffBase::Uncommitted => Ok((or_empty(head), current.to_string())),
        DiffBase::TargetBranch => {
            let target = target_branch.ok_or(Unavailable::NoTargetBranch)?;
            Ok((target.to_string(), current.to_string()))
        }
        DiffBase::LastTurn => {
            let last = checkpoints.last().ok_or(Unavailable::NoCheckpoints)?;
            Ok((or_empty(last.parent.as_deref()), last.commit.clone()))
        }
        DiffBase::ThisTab => {
            let first = checkpoints.first().ok_or(Unavailable::NoCheckpoints)?;
            Ok((or_empty(first.parent.as_deref()), current.to_string()))
        }
    }
}

/// One file's line counts from `git diff --numstat`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStat {
    pub path: String,
    /// Set for a rename or copy.
    pub old_path: Option<String>,
    /// `None` for a binary file, which git counts as `-`.
    pub added: Option<u64>,
    pub deleted: Option<u64>,
}

/// Parses `git diff --numstat -z`.
///
/// A plain entry is `added\tdeleted\tpath\0`. A rename leaves the path empty
/// and follows with two more NUL-terminated fields: `added\tdeleted\t\0old\0new\0`.
pub fn parse_numstat(bytes: &[u8]) -> Vec<FileStat> {
    let mut fields = bytes.split(|b| *b == 0);
    let mut stats = Vec::new();
    while let Some(field) = fields.next() {
        if field.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(field);
        let mut parts = text.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let count = |n: &str| n.parse::<u64>().ok();
        let (path, old_path) = if path.is_empty() {
            let old = fields
                .next()
                .map(|f| String::from_utf8_lossy(f).to_string());
            let new = fields
                .next()
                .map(|f| String::from_utf8_lossy(f).to_string());
            match (old, new) {
                (Some(old), Some(new)) => (new, Some(old)),
                _ => continue,
            }
        } else {
            (path.to_string(), None)
        };
        stats.push(FileStat {
            path,
            old_path,
            added: count(added),
            deleted: count(deleted),
        });
    }
    stats
}

/// Totals for the panel header: files, lines added, lines deleted.
pub fn totals(stats: &[FileStat]) -> (usize, u64, u64) {
    let added = stats.iter().filter_map(|s| s.added).sum();
    let deleted = stats.iter().filter_map(|s| s.deleted).sum();
    (stats.len(), added, deleted)
}

/// The panel header's one-line summary, e.g. `3 files, +12 −4`.
pub fn summary(stats: &[FileStat]) -> String {
    let (files, added, deleted) = totals(stats);
    let noun = if files == 1 { "file" } else { "files" };
    format!("{files} {noun}, +{added} −{deleted}")
}

/// One row of the file list, e.g. `src/a.rs  +3 −1`, `old → new  +0 −0`, or
/// `logo.png  binary`.
pub fn file_row(stat: &FileStat) -> String {
    let name = match &stat.old_path {
        Some(old) => format!("{old} → {}", stat.path),
        None => stat.path.clone(),
    };
    match (stat.added, stat.deleted) {
        (Some(added), Some(deleted)) => format!("{name}  +{added} −{deleted}"),
        _ => format!("{name}  binary"),
    }
}

/// The last component of a repository-relative path (`src/a.rs` → `a.rs`).
pub fn file_name(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

/// The folder part of a repository-relative path (`src/a.rs` → `src`); `""` at the root.
pub fn folder_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(folder, _)| folder)
}

/// A file's row under its folder header: its name, then `+3 −1` or `binary`. A rename names the
/// old path in full (it may have come from another folder): `old/a.rs → b.rs  +0 −0`.
pub fn file_label(stat: &FileStat) -> String {
    let name = match &stat.old_path {
        Some(old) => format!("{old} → {}", file_name(&stat.path)),
        None => file_name(&stat.path).to_owned(),
    };
    match (stat.added, stat.deleted) {
        (Some(added), Some(deleted)) => format!("{name}  +{added} −{deleted}"),
        _ => format!("{name}  binary"),
    }
}

/// The files of a diff grouped for the panel: `(folder, indices into stats)`. Folders sorted by
/// path, the repository root (`""`) first; files sorted by name within each folder.
pub fn group_by_folder(stats: &[FileStat]) -> Vec<(String, Vec<usize>)> {
    let mut groups: std::collections::BTreeMap<&str, Vec<usize>> =
        std::collections::BTreeMap::new();
    for (i, stat) in stats.iter().enumerate() {
        groups.entry(folder_of(&stat.path)).or_default().push(i);
    }
    groups
        .into_iter()
        .map(|(folder, mut files)| {
            files.sort_by(|a, b| file_name(&stats[*a].path).cmp(file_name(&stats[*b].path)));
            (folder.to_owned(), files)
        })
        .collect()
}

/// A unified diff cut at each `diff --git` header: one chunk per file, in git's order, which is
/// the order `--numstat` lists them in (so chunk `i` is `stats[i]`'s). A diff cut short by
/// [`truncate`] has fewer chunks than files. Text before the first header (git writes none) is
/// dropped.
pub fn split_by_file(diff: &str) -> Vec<&str> {
    let mut starts = Vec::new();
    let mut offset = 0;
    for line in diff.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            starts.push(offset);
        }
        offset += line.len();
    }
    starts
        .iter()
        .enumerate()
        .map(|(i, &start)| &diff[start..starts.get(i + 1).copied().unwrap_or(diff.len())])
        .collect()
}

/// How a line of a unified diff is coloured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// `diff --git …`, where each file starts.
    File,
    /// `index`, `---`, `+++`, mode and rename lines between a file header and
    /// its first hunk.
    Meta,
    Hunk,
    Added,
    Removed,
    Context,
}

/// Classifies each line of a unified diff.
///
/// Stateful rather than per-line: a removed line whose text starts with `--`
/// reads `--- …`, exactly like a file's old-path line. Only lines before a
/// file's first hunk are meta.
pub fn classify(diff: &str) -> Vec<LineKind> {
    let mut in_header = false;
    diff.lines()
        .map(|line| {
            if line.starts_with("diff --git ") {
                in_header = true;
                LineKind::File
            } else if line.starts_with("@@") {
                in_header = false;
                LineKind::Hunk
            } else if in_header {
                LineKind::Meta
            } else if line.starts_with('+') {
                LineKind::Added
            } else if line.starts_with('-') {
                LineKind::Removed
            } else if line.starts_with('\\') {
                // "\ No newline at end of file"
                LineKind::Meta
            } else {
                LineKind::Context
            }
        })
        .collect()
}

/// One line of a unified diff with its place in the old and new file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineRef {
    /// The 1-based line in the old file: context and removed lines only.
    pub old: Option<u32>,
    /// The 1-based line in the new file: context and added lines only.
    pub new: Option<u32>,
    /// What [`classify`] makes of the line, so a row agrees with what the panel shows.
    pub kind: LineKind,
}

/// A hunk header's `(start, count)` for the old and the new side.
type HunkRanges = ((u32, u32), (u32, u32));

/// Reads a hunk header, `@@ -a[,b] +c[,d] @@ …`. An omitted count is 1, as git writes it.
/// Anything else, a combined-diff `@@@` header included, is `None`.
fn parse_hunk_header(line: &str) -> Option<HunkRanges> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let (new, _) = rest.split_once(" @@")?;
    let range = |range: &str| -> Option<(u32, u32)> {
        match range.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((range.parse().ok()?, 1)),
        }
    };
    Some((range(old)?, range(new)?))
}

/// Where the next line of a hunk lands, and how many lines of it each side still owes.
struct Cursor {
    old: u32,
    new: u32,
    old_left: u32,
    new_left: u32,
}

/// Maps each text line of a file's unified diff (the chunk [`split_by_file`] gives, header
/// included) to its old and new line numbers, one entry per line in order, so entry `i` is row
/// `i` of [`classify`].
///
/// The numbers start at each `@@ -a,b +c,d @@` header (1-based; an omitted count means 1). A
/// context line advances both counters, an added line only the new one, a removed line only the
/// old one, and each uses up one of the header's lines on its side. Once a hunk's counts are
/// spent, the rows after it (up to the next `@@`) carry `None`, though their `kind` stays what
/// [`classify`] says. File, meta and hunk lines, and `\ No newline`, carry `None`. A hunk whose
/// header cannot be read, and any line before a first hunk, carry `None` too: never a guess. A
/// chunk cut short mid-hunk keeps the numbers it has. The arithmetic saturates.
pub fn parse_hunks(chunk: &str) -> Vec<LineRef> {
    let mut cursor: Option<Cursor> = None;
    let kinds = classify(chunk);
    chunk
        .lines()
        .zip(kinds)
        .map(|(line, kind)| {
            let (mut old, mut new) = (None, None);
            match kind {
                LineKind::File => cursor = None,
                LineKind::Hunk => {
                    cursor = parse_hunk_header(line).map(|((o, ol), (n, nl))| Cursor {
                        old: o,
                        new: n,
                        old_left: ol,
                        new_left: nl,
                    });
                }
                LineKind::Meta => {}
                LineKind::Context | LineKind::Added | LineKind::Removed => {
                    if let Some(c) = cursor.as_mut().filter(|c| c.old_left > 0 || c.new_left > 0) {
                        if kind != LineKind::Added {
                            old = Some(c.old);
                            c.old = c.old.saturating_add(1);
                            c.old_left = c.old_left.saturating_sub(1);
                        }
                        if kind != LineKind::Removed {
                            new = Some(c.new);
                            c.new = c.new.saturating_add(1);
                            c.new_left = c.new_left.saturating_sub(1);
                        }
                    }
                }
            }
            LineRef { old, new, kind }
        })
        .collect()
}

/// The new-file line a row of the chunk stands for, or `None` for a row past the end, outside a
/// hunk, or in a hunk with no new-side line (a deleted file).
///
/// A context or added row is its own new line. A removed row is the nearest following new-side
/// line of the same hunk, else the nearest preceding one. A `\ No newline` row, or any other
/// row of a hunk with no number, is the nearest preceding new-side line, else the nearest
/// following one. A hunk row is the hunk's first new-side line. A file row, and meta rows
/// before the first hunk, are `None`.
pub fn new_line_at(chunk: &str, row: usize) -> Option<u32> {
    let refs = parse_hunks(chunk);
    let at = refs.get(row)?;
    if at.new.is_some() {
        return at.new;
    }
    let is_edge = |r: &LineRef| matches!(r.kind, LineKind::Hunk | LineKind::File);
    let start = match at.kind {
        LineKind::File => return None,
        LineKind::Hunk => row,
        _ => refs[..row].iter().rposition(is_edge)?,
    };
    if refs[start].kind != LineKind::Hunk {
        return None;
    }
    // The hunk runs from its `@@` row to just before the next hunk or file row.
    let end = refs[start + 1..]
        .iter()
        .position(is_edge)
        .map_or(refs.len(), |p| start + 1 + p);
    let following = || refs[row + 1..end].iter().find_map(|r| r.new);
    let preceding = || refs[start..row].iter().rev().find_map(|r| r.new);
    match at.kind {
        LineKind::Removed => following().or_else(preceding),
        LineKind::Hunk => refs[start..end].iter().find_map(|r| r.new),
        _ => preceding().or_else(following),
    }
}

/// The new-file line the first hunk of `diff` starts at (the line an editor should open on), or
/// `None` when the diff has no hunk or its first hunk has no new-side line.
pub fn first_hunk_line(diff: &str) -> Option<u32> {
    let row = classify(diff).iter().position(|k| *k == LineKind::Hunk)?;
    new_line_at(diff, row)
}

/// One unified-diff hunk extracted from a file chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk {
    pub index: usize,
    /// The `@@ -a,b +c,d @@ ...` header line (without trailing newline).
    pub header: String,
    /// Raw hunk lines after the header (including trailing newlines if present).
    pub body: String,
    pub new_start: u32,
    pub new_count: u32,
}

/// Splits a single file's unified diff `chunk` into its file header block and individual hunks.
///
/// Returns `None` if `chunk` has no `--- `/`+++ ` headers or no valid `@@` hunks (for example,
/// binary files or mode-only changes).
pub fn split_hunks(chunk: &str) -> Option<(String, Vec<DiffHunk>)> {
    let kinds = classify(chunk);
    let mut header_block = String::new();
    let mut hunks: Vec<DiffHunk> = Vec::new();
    let mut in_hunk = false;
    let mut has_old = false;
    let mut has_new = false;

    for (raw_line, kind) in chunk.split_inclusive('\n').zip(kinds) {
        let trimmed = raw_line.trim_end_matches(&['\r', '\n'][..]);
        match kind {
            LineKind::Hunk => {
                let (_, (new_start, new_count)) = parse_hunk_header(trimmed)?;
                in_hunk = true;
                let idx = hunks.len();
                hunks.push(DiffHunk {
                    index: idx,
                    header: trimmed.to_owned(),
                    body: String::new(),
                    new_start,
                    new_count,
                });
            }
            _ if !in_hunk => {
                if trimmed.starts_with("--- ") {
                    has_old = true;
                } else if trimmed.starts_with("+++ ") {
                    has_new = true;
                }
                header_block.push_str(raw_line);
                if !raw_line.ends_with('\n') {
                    header_block.push('\n');
                }
            }
            _ => {
                if let Some(current) = hunks.last_mut() {
                    current.body.push_str(raw_line);
                    if !raw_line.ends_with('\n') {
                        current.body.push('\n');
                    }
                }
            }
        }
    }

    if hunks.is_empty() || !has_old || !has_new {
        return None;
    }
    Some((header_block, hunks))
}

/// Builds a valid unified-diff patch containing only the hunks at `selected` indices of `chunk`.
pub fn build_hunk_patch(chunk: &str, selected: &[usize]) -> Option<String> {
    if selected.is_empty() {
        return None;
    }
    let (file_header, hunks) = split_hunks(chunk)?;
    let mut wanted = std::collections::BTreeSet::new();
    for &idx in selected {
        if idx >= hunks.len() {
            return None;
        }
        wanted.insert(idx);
    }
    let mut patch = file_header;
    for idx in wanted {
        let hunk = &hunks[idx];
        patch.push_str(&hunk.header);
        patch.push('\n');
        patch.push_str(&hunk.body);
    }
    Some(patch)
}

/// The lines of a file's diff a review comment is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentSpan {
    pub side: Side,
    /// 1-based, inclusive, on `side`.
    pub start: u32,
    pub end: u32,
    /// The first and last row of the chunk the comment covers (both changed or context lines).
    pub rows: (usize, usize),
    /// The changed and context lines in those rows, prefix kept, in order.
    pub quoted: Vec<String>,
}

/// Why rows of a diff cannot take a comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanRefusal {
    /// Only file, meta or hunk header lines were chosen.
    NoLines,
    /// More changed or context lines than one comment may quote.
    TooMany(usize),
    /// A card's diff of several files: the lines chosen are in more than one.
    AcrossFiles,
    /// The chosen lines are in a hunk with no line numbers (an agent's `@@ edit @@` fragment).
    Unnumbered,
}

impl SpanRefusal {
    /// What the panel says.
    pub fn reason(self) -> String {
        match self {
            SpanRefusal::NoLines => "Select changed or context lines".to_owned(),
            SpanRefusal::TooMany(n) => format!(
                "Select at most {} lines for one comment ({n} chosen)",
                agent_core::review::SELECTION_MAX_LINES
            ),
            SpanRefusal::AcrossFiles => "Select lines of one file for one comment".to_owned(),
            SpanRefusal::Unnumbered => "These lines are the agent's own edit, with no line \
                                        numbers in the file: comment on them in the Changes panel"
                .to_owned(),
        }
    }
}

/// One file of a file-change card's diff: the path its header names and the rows it runs over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardFile {
    /// As the `+++ b/…` header names it (the `--- a/…` one for a deleted file), `a/`/`b/` off.
    pub path: String,
    /// Its first row (its first header line) and the row after its last.
    pub rows: std::ops::Range<usize>,
}

/// The path a `--- a/x` or `+++ b/x` header line names, without its prefix or a trailing
/// timestamp; `None` for `/dev/null` or no path.
fn header_path(line: &str, marker: &str, prefix: &str) -> Option<String> {
    let rest = line.strip_prefix(marker)?;
    let rest = rest.split('\t').next().unwrap_or(rest);
    if rest == "/dev/null" || rest.is_empty() {
        return None;
    }
    Some(rest.strip_prefix(prefix).unwrap_or(rest).to_owned())
}

/// The files of a file-change card's diff, in order. A file starts at a `diff --git` line, or
/// at a `--- ` line followed by a `+++ ` line that is not inside a hunk (the shape the app's own
/// differ writes, one file after another). Rows before the first file, and a file whose headers
/// name no path, belong to no file.
pub fn card_files(text: &str) -> Vec<CardFile> {
    let lines: Vec<&str> = text.lines().collect();
    let refs = parse_hunks(text);
    let numbered = |i: usize| {
        refs.get(i)
            .is_some_and(|r| r.old.is_some() || r.new.is_some())
    };
    let mut starts: Vec<usize> = Vec::new();
    // Inside a `diff --git` header, before its first hunk: its `---`/`+++` start nothing.
    let mut in_git_header = false;
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with("diff --git ") {
            starts.push(i);
            in_git_header = true;
        } else if line.starts_with("@@") {
            in_git_header = false;
        } else if !in_git_header
            && line.starts_with("--- ")
            && lines.get(i + 1).is_some_and(|l| l.starts_with("+++ "))
            && !numbered(i)
            && !numbered(i + 1)
        {
            starts.push(i);
        }
    }
    starts
        .iter()
        .enumerate()
        .filter_map(|(n, &start)| {
            let end = starts.get(n + 1).copied().unwrap_or(lines.len());
            // The header ends at the first hunk.
            let header = lines[start..end]
                .iter()
                .take_while(|l| !l.starts_with("@@"));
            let (mut new, mut old) = (None, None);
            for line in header {
                new = new.or_else(|| header_path(line, "+++ ", "b/"));
                old = old.or_else(|| header_path(line, "--- ", "a/"));
            }
            Some(CardFile {
                path: new.or(old)?,
                rows: start..end,
            })
        })
        .collect()
}

/// A comment's place on a file-change card: the file and its span, the span's rows counted in
/// the card's whole text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardSpan {
    pub path: String,
    pub span: CommentSpan,
}

/// The comment rows `first_row..=last_row` of a file-change card's `text` would make. As
/// [`comment_span`] does in the file's own part of the text, which the rows must all be in
/// ([`SpanRefusal::AcrossFiles`] else; header rows at either end do not count). Rows in a hunk
/// with no numbers are [`SpanRefusal::Unnumbered`], never a guess.
pub fn card_comment_span(
    text: &str,
    first_row: usize,
    last_row: usize,
) -> Result<CardSpan, SpanRefusal> {
    let (lo, hi) = (first_row.min(last_row), first_row.max(last_row));
    let files = card_files(text);
    let refs = parse_hunks(text);
    let lines: Vec<&str> = text.lines().collect();
    // Only the rows a comment can take decide which file is meant: changed or context rows of a
    // file, below a hunk header of that file (so never the file's own `---`/`+++` rows).
    let mut commentable = vec![false; lines.len()];
    for file in &files {
        let mut in_hunk = false;
        for i in file.rows.clone() {
            if lines[i].starts_with("@@") {
                in_hunk = true;
            } else if in_hunk {
                commentable[i] = refs.get(i).is_some_and(|r| {
                    matches!(
                        r.kind,
                        LineKind::Added | LineKind::Removed | LineKind::Context
                    )
                });
            }
        }
    }
    let chosen: Vec<usize> = (lo..=hi.min(lines.len().saturating_sub(1)))
        .filter(|&i| commentable.get(i).copied().unwrap_or(false))
        .collect();
    let (Some(&first), Some(&last)) = (chosen.first(), chosen.last()) else {
        return Err(SpanRefusal::NoLines);
    };
    let file = files
        .iter()
        .find(|f| f.rows.contains(&first))
        .ok_or(SpanRefusal::NoLines)?;
    if !file.rows.contains(&last) {
        return Err(SpanRefusal::AcrossFiles);
    }
    if chosen
        .iter()
        .all(|&i| refs[i].old.is_none() && refs[i].new.is_none())
    {
        return Err(SpanRefusal::Unnumbered);
    }
    let chunk: String = lines[file.rows.clone()]
        .iter()
        .map(|l| format!("{l}\n"))
        .collect();
    let base = file.rows.start;
    let mut span = comment_span(&chunk, first - base, last - base)?;
    span.rows = (span.rows.0 + base, span.rows.1 + base);
    Ok(CardSpan {
        path: file.path.clone(),
        span,
    })
}

/// Whether a row is a changed or context line with a place in a file.
fn is_content(r: &LineRef) -> bool {
    matches!(
        r.kind,
        LineKind::Added | LineKind::Removed | LineKind::Context
    ) && (r.old.is_some() || r.new.is_some())
}

/// The side and range of the content rows `rows` of `refs`: the new side when any of them has a
/// new line (context or added), else the old side (only removed lines).
fn span_of(refs: &[LineRef], rows: &[usize]) -> Option<(Side, u32, u32)> {
    let numbers = |pick: fn(&LineRef) -> Option<u32>| -> Option<(u32, u32)> {
        let mut it = rows.iter().filter_map(|&i| pick(&refs[i]));
        let first = it.next()?;
        Some(it.fold((first, first), |(lo, hi), n| (lo.min(n), hi.max(n))))
    };
    if let Some((start, end)) = numbers(|r| r.new) {
        return Some((Side::New, start, end));
    }
    numbers(|r| r.old).map(|(start, end)| (Side::Old, start, end))
}

/// The comment rows `first_row..=last_row` of `chunk` (a file's diff, as [`split_by_file`] cuts
/// it) would make: file, meta and hunk rows are left out, and the range is counted on the new
/// side unless only removed lines were chosen. Rows past the end are ignored.
pub fn comment_span(
    chunk: &str,
    first_row: usize,
    last_row: usize,
) -> Result<CommentSpan, SpanRefusal> {
    let refs = parse_hunks(chunk);
    let (lo, hi) = (first_row.min(last_row), first_row.max(last_row));
    let rows: Vec<usize> = (lo..=hi.min(refs.len().saturating_sub(1)))
        .filter(|&i| i < refs.len() && is_content(&refs[i]))
        .collect();
    let (Some(&first), Some(&last)) = (rows.first(), rows.last()) else {
        return Err(SpanRefusal::NoLines);
    };
    if rows.len() > agent_core::review::SELECTION_MAX_LINES {
        return Err(SpanRefusal::TooMany(rows.len()));
    }
    let (side, start, end) = span_of(&refs, &rows).ok_or(SpanRefusal::NoLines)?;
    let lines: Vec<&str> = chunk.lines().collect();
    Ok(CommentSpan {
        side,
        start,
        end,
        rows: (first, last),
        quoted: rows.iter().map(|&i| lines[i].to_owned()).collect(),
    })
}

/// Where a comment made earlier sits in `chunk`, the file's diff as it is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    pub located: Located,
    /// Its span in this diff, unless it is [`Located::Gone`].
    pub span: Option<CommentSpan>,
}

/// Finds a comment's `quoted` lines in `chunk`. A run of content rows whose lines are exactly
/// `quoted` (a quote is compared as kept: redacted, so a row is also compared redacted) and
/// whose range is the recorded `side`, `start` and `end` is [`Located::Exact`]. Else one such
/// run elsewhere, and only one, is [`Located::Moved`] with its own range. None, or several
/// (which one was meant cannot be told), is [`Located::Gone`]: never a guess.
pub fn anchor(chunk: &str, side: Side, start: u32, end: u32, quoted: &[String]) -> Anchor {
    let gone = Anchor {
        located: Located::Gone,
        span: None,
    };
    if quoted.is_empty() {
        return gone;
    }
    let refs = parse_hunks(chunk);
    let lines: Vec<&str> = chunk.lines().collect();
    let content: Vec<usize> = (0..refs.len().min(lines.len()))
        .filter(|&i| is_content(&refs[i]))
        .collect();
    // Each row redacted at most once, and only when its text is not already the quote's.
    let mut redacted: Vec<Option<String>> = vec![None; lines.len()];
    let mut same = |row: usize, want: &str| {
        let line = lines[row];
        if line == want {
            return true;
        }
        redacted[row]
            .get_or_insert_with(|| agent_core::redact::redact(line))
            .as_str()
            == want
    };
    let mut found: Vec<&[usize]> = Vec::new();
    if content.len() >= quoted.len() {
        for window in content.windows(quoted.len()) {
            if window
                .iter()
                .zip(quoted)
                .all(|(&row, want)| same(row, want))
            {
                found.push(window);
            }
        }
    }
    let to_span = |rows: &[usize]| -> Option<CommentSpan> {
        let (side, start, end) = span_of(&refs, rows)?;
        Some(CommentSpan {
            side,
            start,
            end,
            rows: (*rows.first()?, *rows.last()?),
            quoted: quoted.to_vec(),
        })
    };
    let spans: Vec<CommentSpan> = found.iter().filter_map(|rows| to_span(rows)).collect();
    if let Some(exact) = spans
        .iter()
        .find(|s| s.side == side && s.start == start && s.end == end)
    {
        return Anchor {
            located: Located::Exact,
            span: Some(exact.clone()),
        };
    }
    match spans.as_slice() {
        [only] => Anchor {
            located: Located::Moved,
            span: Some(only.clone()),
        },
        _ => gone,
    }
}

/// At most this much diff text is put in the panel.
pub const MAX_DIFF_BYTES: usize = 1024 * 1024;
/// At most this many lines of diff text are put in the panel.
pub const MAX_DIFF_LINES: usize = 20_000;

/// Cuts `diff` to the first `max_lines` lines and `max_bytes` bytes, never
/// splitting a line or a character. Returns the kept text and how many lines
/// were left out.
pub fn truncate(diff: &str, max_bytes: usize, max_lines: usize) -> (&str, usize) {
    let mut end = 0;
    let mut kept = 0;
    for line in diff.split_inclusive('\n') {
        if kept == max_lines || end + line.len() > max_bytes {
            break;
        }
        end += line.len();
        kept += 1;
    }
    let total = diff.split_inclusive('\n').count();
    (&diff[..end], total - kept)
}

fn extract_candidate_paths(
    val: &serde_json::Value,
    paths: &mut std::collections::BTreeSet<std::path::PathBuf>,
) {
    for edit in crate::editdiff::preview_from_input(val) {
        paths.insert(std::path::PathBuf::from(edit.path));
    }
    const KEYS: &[&str] = &[
        "file_path",
        "FilePath",
        "path",
        "Path",
        "file",
        "TargetFile",
        "target_file",
        "AbsolutePath",
        "absolute_path",
        "Cwd",
        "cwd",
        "directory",
        "dir",
    ];
    for k in KEYS {
        if let Some(s) = val.get(*k).and_then(|v| v.as_str()) {
            if !s.trim().is_empty() {
                paths.insert(std::path::PathBuf::from(s));
            }
        }
    }
    if let Some(args) = val
        .get("Arguments")
        .or_else(|| val.get("arguments"))
        .or_else(|| val.get("args"))
    {
        extract_candidate_paths(args, paths);
    }
    if let Some(cmd) = val
        .get("CommandLine")
        .or_else(|| val.get("command"))
        .or_else(|| val.get("cmd"))
        .and_then(|v| v.as_str())
    {
        let parts: Vec<&str> = cmd.split_whitespace().collect();
        for i in 0..parts.len() {
            if parts[i] == "-C" && i + 1 < parts.len() {
                let clean = parts[i + 1].trim_matches(|c| c == '"' || c == '\'');
                paths.insert(std::path::PathBuf::from(clean));
            }
        }
    }
}

/// Extracts all candidate file paths touched or modified by the thread's events.
pub fn touched_paths_from_envelopes(
    envelopes: &[agent_core::event::Envelope],
) -> Vec<std::path::PathBuf> {
    let mut paths = std::collections::BTreeSet::new();
    for env in envelopes {
        match &env.event {
            agent_core::event::Event::SessionStarted { cwd: Some(c), .. } => {
                paths.insert(std::path::PathBuf::from(c));
            }
            agent_core::event::Event::ItemStarted {
                input: Some(val), ..
            } => {
                extract_candidate_paths(val, &mut paths);
            }
            agent_core::event::Event::ApprovalRequested { input, .. } => {
                extract_candidate_paths(input, &mut paths);
            }
            _ => {}
        }
    }
    paths.into_iter().collect()
}

/// Discovers the enclosing git repository root for the touched file paths, if any.
pub fn discover_enclosing_repo(paths: &[std::path::PathBuf]) -> Option<std::path::PathBuf> {
    for path in paths {
        let check_dir = if path.is_file() {
            path.parent()
        } else {
            Some(path.as_path())
        };
        if let Some(dir) = check_dir {
            if let Ok(Some(repo)) = crate::git::discover(dir) {
                return Some(repo.toplevel);
            }
        }
        if path.is_relative() {
            if let Ok(curr) = std::env::current_dir() {
                let joined = curr.join(path);
                let check_dir = if joined.is_file() {
                    joined.parent()
                } else {
                    Some(joined.as_path())
                };
                if let Some(dir) = check_dir {
                    if let Ok(Some(repo)) = crate::git::discover(dir) {
                        return Some(repo.toplevel);
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(path: &str) -> FileStat {
        FileStat {
            path: path.to_owned(),
            old_path: None,
            added: Some(1),
            deleted: Some(0),
        }
    }

    /// Folders sorted with the root first, files by name inside each, indices back into `stats`.
    #[test]
    fn files_group_under_their_folder() {
        let stats = [
            stat("src/window/diff_panel.rs"),
            stat("Cargo.toml"),
            stat("src/main.rs"),
            stat("src/window/imp.rs"),
            stat("README.md"),
            stat("src/config.rs"),
        ];
        let groups = group_by_folder(&stats);
        let named: Vec<(&str, Vec<&str>)> = groups
            .iter()
            .map(|(folder, files)| {
                (
                    folder.as_str(),
                    files.iter().map(|i| file_name(&stats[*i].path)).collect(),
                )
            })
            .collect();
        assert_eq!(
            named,
            [
                ("", vec!["Cargo.toml", "README.md"]),
                ("src", vec!["config.rs", "main.rs"]),
                ("src/window", vec!["diff_panel.rs", "imp.rs"]),
            ]
        );
        assert!(group_by_folder(&[]).is_empty());
    }

    #[test]
    fn file_labels_name_the_file_under_its_folder() {
        assert_eq!(file_label(&stat("src/a.rs")), "a.rs  +1 −0");
        let renamed = FileStat {
            path: "src/b.rs".into(),
            old_path: Some("old/a.rs".into()),
            added: Some(0),
            deleted: Some(0),
        };
        assert_eq!(file_label(&renamed), "old/a.rs → b.rs  +0 −0");
        let binary = FileStat {
            added: None,
            deleted: None,
            ..stat("logo.png")
        };
        assert_eq!(file_label(&binary), "logo.png  binary");
    }

    /// One chunk per `diff --git` header, each starting at its header, nothing lost between; a
    /// removed line that reads like a header inside a hunk does not split.
    #[test]
    fn diffs_split_at_each_file_header() {
        let a = "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-x\n+y\n";
        let b = "diff --git a/b b/b\nnew file mode 100644\n--- /dev/null\n+++ b/b\n@@ -0,0 +1 @@\n+diff --git not a header\n";
        let text = format!("{a}{b}");
        let chunks = split_by_file(&text);
        assert_eq!(chunks, [a, b]);
        assert_eq!(chunks.concat(), text);
        assert!(split_by_file("").is_empty());
        let no_newline = "diff --git a/a b/a\n@@ -1 +1 @@\n-x\n+y";
        assert_eq!(split_by_file(no_newline), [no_newline]);
    }

    #[test]
    fn all_lists_every_base_in_dropdown_order() {
        // Adding a variant must fail to compile here until ALL is updated.
        for (i, base) in DiffBase::ALL.iter().enumerate() {
            let expected = match base {
                DiffBase::Uncommitted => 0,
                DiffBase::TargetBranch => 1,
                DiffBase::LastTurn => 2,
                DiffBase::ThisTab => 3,
            };
            assert_eq!(i, expected);
        }
    }

    fn cp(commit: &str, parent: Option<&str>) -> CheckpointCommit {
        CheckpointCommit {
            commit: commit.into(),
            parent: parent.map(Into::into),
        }
    }

    #[test]
    fn ranges_follow_the_base() {
        let cps = [cp("c1", Some("head")), cp("c2", Some("c1"))];
        let range = |base, head, cps: &[CheckpointCommit]| {
            diff_range(base, head, cps, "now", "E", Some("target"))
        };
        assert_eq!(
            range(DiffBase::Uncommitted, Some("head"), &cps),
            Ok(("head".into(), "now".into()))
        );
        assert_eq!(
            range(DiffBase::TargetBranch, Some("head"), &cps),
            Ok(("target".into(), "now".into()))
        );
        assert_eq!(
            range(DiffBase::LastTurn, Some("head"), &cps),
            Ok(("c1".into(), "c2".into()))
        );
        assert_eq!(
            range(DiffBase::ThisTab, Some("head"), &cps),
            Ok(("head".into(), "now".into()))
        );
    }

    #[test]
    fn a_missing_parent_or_head_diffs_against_the_empty_tree() {
        let cps = [cp("c1", None)];
        let range = |base, head| diff_range(base, head, &cps, "now", "E", Some("target"));
        assert_eq!(
            range(DiffBase::Uncommitted, None),
            Ok(("E".into(), "now".into()))
        );
        assert_eq!(
            range(DiffBase::TargetBranch, None),
            Ok(("target".into(), "now".into()))
        );
        assert_eq!(
            range(DiffBase::LastTurn, None),
            Ok(("E".into(), "c1".into()))
        );
        assert_eq!(
            range(DiffBase::ThisTab, None),
            Ok(("E".into(), "now".into()))
        );
    }

    #[test]
    fn checkpoint_bases_need_a_checkpoint() {
        for base in [DiffBase::LastTurn, DiffBase::ThisTab] {
            assert_eq!(
                diff_range(base, Some("h"), &[], "now", "E", None),
                Err(Unavailable::NoCheckpoints)
            );
        }
        assert_eq!(
            diff_range(DiffBase::TargetBranch, Some("h"), &[], "now", "E", None),
            Err(Unavailable::NoTargetBranch)
        );
    }

    #[test]
    fn numstat_reads_plain_binary_and_renamed_entries() {
        let raw = b"3\t1\tsrc/a.rs\0-\t-\tlogo.png\x000\t0\t\0old name.txt\0new name.txt\0";
        assert_eq!(
            parse_numstat(raw),
            vec![
                FileStat {
                    path: "src/a.rs".into(),
                    old_path: None,
                    added: Some(3),
                    deleted: Some(1),
                },
                FileStat {
                    path: "logo.png".into(),
                    old_path: None,
                    added: None,
                    deleted: None,
                },
                FileStat {
                    path: "new name.txt".into(),
                    old_path: Some("old name.txt".into()),
                    added: Some(0),
                    deleted: Some(0),
                },
            ]
        );
        assert_eq!(totals(&parse_numstat(raw)), (3, 3, 1));
        assert!(parse_numstat(b"").is_empty());
    }

    #[test]
    fn summary_and_rows_read_naturally() {
        let raw = b"3\t1\tsrc/a.rs\0-\t-\tlogo.png\x000\t0\t\0old.txt\0new.txt\0";
        let stats = parse_numstat(raw);
        assert_eq!(summary(&stats), "3 files, +3 −1");
        assert_eq!(summary(&stats[..1]), "1 file, +3 −1");
        assert_eq!(file_row(&stats[0]), "src/a.rs  +3 −1");
        assert_eq!(file_row(&stats[1]), "logo.png  binary");
        assert_eq!(file_row(&stats[2]), "old.txt → new.txt  +0 −0");
    }

    #[test]
    fn a_removed_line_that_looks_like_a_header_is_still_removed() {
        let diff = "diff --git a/x b/x\nindex 1..2 100644\n--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n--- not a header\n+++ nor this\n ctx\n\\ No newline at end of file\n";
        assert_eq!(
            classify(diff),
            vec![
                LineKind::File,
                LineKind::Meta,
                LineKind::Meta,
                LineKind::Meta,
                LineKind::Hunk,
                LineKind::Removed,
                LineKind::Added,
                LineKind::Context,
                LineKind::Meta,
            ]
        );
    }

    #[test]
    fn truncation_keeps_whole_lines_and_counts_the_rest() {
        let diff = "a\nbb\nccc\n";
        assert_eq!(truncate(diff, 100, 100), (diff, 0));
        assert_eq!(truncate(diff, 100, 2), ("a\nbb\n", 1));
        assert_eq!(truncate(diff, 6, 100), ("a\nbb\n", 1));
        // A multi-byte line that does not fit is dropped whole, never split.
        let wide = "é\n日本語\n";
        assert_eq!(truncate(wide, 5, 100), ("é\n", 1));
    }

    #[test]
    fn touched_paths_extracted_from_events() {
        use agent_core::event::{Envelope, Event, ItemKind};
        let env1 = Envelope::new(Event::ItemStarted {
            kind: ItemKind::FileChange,
            title: "edit".into(),
            input: Some(serde_json::json!({"file_path": "src/foo.rs"})),
            parent: None,
        });
        let env2 = Envelope::new(Event::ApprovalRequested {
            tool: "write_to_file".into(),
            title: None,
            input: serde_json::json!({"TargetFile": "/path/to/bar.rs"}),
            reason: None,
            options: vec![],
            response: agent_core::event::ResponseCapability::Live,
            remembers: None,
        });
        let paths = touched_paths_from_envelopes(&[env1, env2]);
        assert_eq!(
            paths,
            vec![
                std::path::PathBuf::from("/path/to/bar.rs"),
                std::path::PathBuf::from("src/foo.rs")
            ]
        );
    }

    fn lr(old: Option<u32>, new: Option<u32>, kind: LineKind) -> LineRef {
        LineRef { old, new, kind }
    }

    const HEAD: &str = "diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n";

    #[test]
    fn the_first_hunk_line_is_where_an_editor_opens() {
        let chunk = format!("{HEAD}@@ -10,2 +12,3 @@\n a\n+b\n c\n@@ -40 +50 @@\n-x\n+y\n");
        assert_eq!(first_hunk_line(&chunk), Some(12));
        assert_eq!(first_hunk_line(HEAD), None);
        assert_eq!(first_hunk_line(""), None);
    }

    #[test]
    fn parse_hunks_numbers_two_hunks() {
        let chunk =
            format!("{HEAD}@@ -1,3 +1,3 @@ fn a\n a\n-b\n+B\n c\n@@ -10,2 +10,3 @@\n x\n+y\n z\n");
        use LineKind::*;
        assert_eq!(
            parse_hunks(&chunk),
            vec![
                lr(None, None, File),
                lr(None, None, Meta),
                lr(None, None, Meta),
                lr(None, None, Meta),
                lr(None, None, Hunk),
                lr(Some(1), Some(1), Context),
                lr(Some(2), None, Removed),
                lr(None, Some(2), Added),
                lr(Some(3), Some(3), Context),
                lr(None, None, Hunk),
                lr(Some(10), Some(10), Context),
                lr(None, Some(11), Added),
                lr(Some(11), Some(12), Context),
            ]
        );
    }

    #[test]
    fn parse_hunks_added_only_and_removed_only() {
        use LineKind::*;
        assert_eq!(
            parse_hunks("@@ -4,0 +5,2 @@\n+a\n+b\n"),
            vec![
                lr(None, None, Hunk),
                lr(None, Some(5), Added),
                lr(None, Some(6), Added),
            ]
        );
        assert_eq!(
            parse_hunks("@@ -4,2 +3,0 @@\n-a\n-b\n"),
            vec![
                lr(None, None, Hunk),
                lr(Some(4), None, Removed),
                lr(Some(5), None, Removed),
            ]
        );
    }

    #[test]
    fn parse_hunks_new_and_deleted_file() {
        use LineKind::*;
        assert_eq!(
            parse_hunks("@@ -0,0 +1,3 @@\n+a\n+b\n+c\n"),
            vec![
                lr(None, None, Hunk),
                lr(None, Some(1), Added),
                lr(None, Some(2), Added),
                lr(None, Some(3), Added),
            ]
        );
        assert_eq!(
            parse_hunks("@@ -1,2 +0,0 @@\n-a\n-b\n"),
            vec![
                lr(None, None, Hunk),
                lr(Some(1), None, Removed),
                lr(Some(2), None, Removed),
            ]
        );
    }

    #[test]
    fn parse_hunks_omitted_counts_mean_one() {
        use LineKind::*;
        assert_eq!(
            parse_hunks("@@ -3 +3 @@\n-a\n+b\n"),
            vec![
                lr(None, None, Hunk),
                lr(Some(3), None, Removed),
                lr(None, Some(3), Added),
            ]
        );
    }

    #[test]
    fn parse_hunks_removed_line_that_looks_like_a_header() {
        use LineKind::*;
        let chunk = format!("{HEAD}@@ -1,2 +1 @@\n--- x\n y\n");
        let refs = parse_hunks(&chunk);
        assert_eq!(refs[5], lr(Some(1), None, Removed));
        assert_eq!(refs[6], lr(Some(2), Some(1), Context));
    }

    #[test]
    fn parse_hunks_no_newline_marker_has_no_numbers() {
        use LineKind::*;
        assert_eq!(
            parse_hunks("@@ -1 +1 @@\n-a\n\\ No newline at end of file\n+b\n"),
            vec![
                lr(None, None, Hunk),
                lr(Some(1), None, Removed),
                lr(None, None, Meta),
                lr(None, Some(1), Added),
            ]
        );
    }

    #[test]
    fn parse_hunks_malformed_header_gives_no_numbers() {
        use LineKind::*;
        for header in [
            "@@ -x +1 @@",
            "@@ -1,2 +1,y @@",
            "@@@ -1 -1 +1 @@@",
            "@@ -1 +1",
            "@@",
        ] {
            let chunk = format!("{header}\n a\n-b\n+c\n");
            assert_eq!(
                parse_hunks(&chunk),
                vec![
                    lr(None, None, Hunk),
                    lr(None, None, Context),
                    lr(None, None, Removed),
                    lr(None, None, Added),
                ],
                "{header}"
            );
        }
        // Text before any hunk is not numbered either, and a good hunk after a bad one recovers.
        assert_eq!(
            parse_hunks(" a\n@@ -x @@\n b\n@@ -2 +2 @@\n c\n"),
            vec![
                lr(None, None, Context),
                lr(None, None, Hunk),
                lr(None, None, Context),
                lr(None, None, Hunk),
                lr(Some(2), Some(2), Context),
            ]
        );
    }

    #[test]
    fn parse_hunks_saturates() {
        let refs = parse_hunks("@@ -4294967295,3 +4294967295,3 @@\n a\n b\n c\n");
        assert_eq!(refs[1].new, Some(u32::MAX));
        assert_eq!(refs[2].new, Some(u32::MAX));
        assert_eq!(refs[3].old, Some(u32::MAX));
    }

    #[test]
    fn new_line_at_maps_removed_header_and_out_of_range_rows() {
        // Rows: 0 file, 1-3 meta, 4 hunk, 5 context, 6 removed, 7 removed, 8 added,
        // 9 removed (last in its hunk), 10 hunk, 11 removed, 12 meta.
        let chunk = format!(
            "{HEAD}@@ -1,5 +1,3 @@\n a\n-b\n-c\n+D\n-e\n@@ -20,1 +18,0 @@\n-z\n\\ No newline at end of file\n"
        );
        assert_eq!(new_line_at(&chunk, 5), Some(1));
        // A removed row takes the next new-side line.
        assert_eq!(new_line_at(&chunk, 6), Some(2));
        assert_eq!(new_line_at(&chunk, 7), Some(2));
        assert_eq!(new_line_at(&chunk, 8), Some(2));
        // With none after it, the one before.
        assert_eq!(new_line_at(&chunk, 9), Some(2));
        // Header, hunk and meta rows.
        assert_eq!(new_line_at(&chunk, 0), None);
        assert_eq!(new_line_at(&chunk, 2), None);
        assert_eq!(new_line_at(&chunk, 4), Some(1));
        // A hunk with no new side has nothing to give.
        assert_eq!(new_line_at(&chunk, 10), None);
        assert_eq!(new_line_at(&chunk, 11), None);
        assert_eq!(new_line_at(&chunk, 12), None);
        // Past the end.
        assert_eq!(new_line_at(&chunk, 13), None);
        assert_eq!(new_line_at("", 0), None);
    }

    #[test]
    fn new_line_at_marker_row_is_the_nearest_preceding_line_else_the_following() {
        let chunk = "@@ -7,1 +9,2 @@\n-a\n+b\n+c\n\\ No newline at end of file\n";
        assert_eq!(new_line_at(chunk, 4), Some(10));
        let chunk = "@@ -3,1 +3,1 @@\n\\ odd\n a\n";
        assert_eq!(new_line_at(chunk, 1), Some(3));
    }

    #[test]
    fn parse_hunks_stops_numbering_after_a_complete_hunk() {
        use LineKind::*;
        assert_eq!(
            parse_hunks("@@ -1,2 +1,2 @@\n a\n b\n c\n-d\n+e\n@@ -9 +9 @@\n-x\n+y\n"),
            vec![
                lr(None, None, Hunk),
                lr(Some(1), Some(1), Context),
                lr(Some(2), Some(2), Context),
                lr(None, None, Context),
                lr(None, None, Removed),
                lr(None, None, Added),
                lr(None, None, Hunk),
                lr(Some(9), None, Removed),
                lr(None, Some(9), Added),
            ]
        );
    }

    #[test]
    fn parse_hunks_keeps_numbers_when_the_chunk_ends_mid_hunk() {
        use LineKind::*;
        assert_eq!(
            parse_hunks("@@ -5,4 +5,5 @@\n a\n-b\n+B\n"),
            vec![
                lr(None, None, Hunk),
                lr(Some(5), Some(5), Context),
                lr(Some(6), None, Removed),
                lr(None, Some(6), Added),
            ]
        );
        assert_eq!(new_line_at("@@ -5,4 +5,5 @@\n a\n-b\n", 2), Some(5));
    }

    #[test]
    fn new_line_at_a_stray_row_after_a_hunk_takes_the_preceding_line() {
        assert_eq!(new_line_at("@@ -0,0 +4 @@\n+a\n stray\n", 2), Some(4));
    }

    /// A file's diff: header rows 0-3, hunk row 4, then rows 5-9.
    const CHUNK: &str = "diff --git a/f.rs b/f.rs\nindex 1..2 100644\n--- a/f.rs\n+++ b/f.rs\n\
                         @@ -10,4 +10,3 @@\n keep\n-old one\n-old two\n+new one\n tail\n";

    fn quoted(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn a_comment_span_counts_on_the_new_side_unless_only_removed_lines_are_chosen() {
        // Context, removed and added: the new side, from the context and added lines.
        let span = comment_span(CHUNK, 5, 8).expect("lines");
        assert_eq!((span.side, span.start, span.end), (Side::New, 10, 11));
        assert_eq!(span.rows, (5, 8));
        assert_eq!(
            span.quoted,
            quoted(&[" keep", "-old one", "-old two", "+new one"])
        );
        // Only removed lines: the old side.
        let span = comment_span(CHUNK, 6, 7).expect("lines");
        assert_eq!((span.side, span.start, span.end), (Side::Old, 11, 12));
        // Rows given backwards are the same rows.
        assert_eq!(comment_span(CHUNK, 7, 6), comment_span(CHUNK, 6, 7));
        // Header rows around the lines are left out of the span.
        let span = comment_span(CHUNK, 0, 5).expect("lines");
        assert_eq!((span.start, span.end, span.rows), (10, 10, (5, 5)));
        assert_eq!(span.quoted, quoted(&[" keep"]));
        // A row past the end is ignored.
        assert_eq!(comment_span(CHUNK, 9, 400).map(|s| s.rows), Ok((9, 9)));
    }

    #[test]
    fn header_rows_alone_take_no_comment_and_a_huge_selection_is_refused() {
        assert_eq!(comment_span(CHUNK, 0, 4), Err(SpanRefusal::NoLines));
        assert_eq!(comment_span("", 0, 0), Err(SpanRefusal::NoLines));
        let big: String = std::iter::once("@@ -0,0 +1,401 @@\n".to_owned())
            .chain((0..401).map(|i| format!("+l{i}\n")))
            .collect();
        assert_eq!(comment_span(&big, 0, 401), Err(SpanRefusal::TooMany(401)));
        assert!(comment_span(&big, 0, 400).is_ok());
        assert!(SpanRefusal::TooMany(401).reason().contains("401"));
        assert_eq!(
            SpanRefusal::NoLines.reason(),
            "Select changed or context lines"
        );
    }

    #[test]
    fn a_comment_is_anchored_where_it_was_moved_or_gone() {
        let span = comment_span(CHUNK, 6, 8).expect("lines");
        let here = anchor(CHUNK, span.side, span.start, span.end, &span.quoted);
        assert_eq!(here.located, Located::Exact);
        assert_eq!(here.span.as_ref().map(|s| s.rows), Some((6, 8)));

        // Three lines inserted above: the same lines, found once, renumbered.
        let shifted = CHUNK.replace("@@ -10,4 +10,3 @@", "@@ -13,4 +13,3 @@");
        let moved = anchor(&shifted, span.side, span.start, span.end, &span.quoted);
        assert_eq!(moved.located, Located::Moved);
        let s = moved.span.expect("span");
        assert_eq!(
            (s.side, s.start, s.end, s.rows),
            (Side::New, 14, 14, (6, 8))
        );

        // The lines changed: gone, with no span.
        let changed = CHUNK.replace("+new one", "+newer one");
        let gone = anchor(&changed, span.side, span.start, span.end, &span.quoted);
        assert_eq!(
            gone,
            Anchor {
                located: Located::Gone,
                span: None
            }
        );

        // Found twice, and neither at its recorded lines: which was meant cannot be told.
        let twice = "@@ -1,2 +1,2 @@\n a\n+x\n@@ -20,2 +20,2 @@\n a\n+x\n";
        let q = quoted(&[" a", "+x"]);
        assert_eq!(anchor(twice, Side::New, 5, 6, &q).located, Located::Gone);
        // Twice, but once at its recorded lines: that one.
        let at = anchor(twice, Side::New, 20, 21, &q);
        assert_eq!(at.located, Located::Exact);
        assert_eq!(at.span.map(|s| s.rows), Some((4, 5)));
        assert_eq!(anchor(CHUNK, Side::New, 1, 1, &[]).located, Located::Gone);
    }

    #[test]
    fn a_redacted_quote_still_finds_its_line() {
        let token = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        let chunk = format!("@@ -1 +1 @@\n+let t = \"{token}\";\n");
        let kept = vec![agent_core::redact::redact(&format!(
            "+let t = \"{token}\";"
        ))];
        assert_ne!(
            kept[0],
            format!("+let t = \"{token}\";"),
            "the quote was masked"
        );
        let found = anchor(&chunk, Side::New, 1, 1, &kept);
        assert_eq!(found.located, Located::Exact);
    }

    #[test]
    fn every_base_has_its_own_key() {
        let keys: std::collections::HashSet<_> = DiffBase::ALL.iter().map(|b| b.key()).collect();
        assert_eq!(keys.len(), DiffBase::ALL.len());
        assert!(!keys.contains(agent_core::review::CARD_BASE));
    }

    /// Two files as the app's differ writes a card's diff: headers, then hunks, one after the
    /// other, no `diff --git` line.
    const CARD: &str = "--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n\
                        --- /dev/null\n+++ b/new.rs\n@@ -0,0 +1,2 @@\n+x\n+y\n";

    #[test]
    fn a_card_diff_is_cut_into_its_files_by_their_headers() {
        let files = card_files(CARD);
        assert_eq!(
            files,
            vec![
                CardFile {
                    path: "src/a.rs".into(),
                    rows: 0..7
                },
                CardFile {
                    path: "new.rs".into(),
                    rows: 7..12
                },
            ]
        );
        // A deleted file is named by its old header; `diff --git` headers start a file too, and
        // their own `---`/`+++` lines start nothing more.
        let git = "diff --git a/old.rs b/old.rs\ndeleted file mode 100644\n--- a/old.rs\n\
                   +++ /dev/null\n@@ -1 +0,0 @@\n-gone\n";
        assert_eq!(
            card_files(git),
            vec![CardFile {
                path: "old.rs".into(),
                rows: 0..6
            }]
        );
        // A removed line that reads `--- ` inside a hunk is a line, not a header.
        let inside = "--- a/x\n+++ b/x\n@@ -1,2 +1,0 @@\n--- y\n-+++ z\n";
        assert_eq!(card_files(inside).len(), 1);
        assert!(card_files("# a.rs: binary file, not shown\n").is_empty());
    }

    #[test]
    fn a_card_comment_names_its_file_and_counts_rows_in_the_whole_card() {
        let second = card_comment_span(CARD, 10, 11).expect("lines");
        assert_eq!(second.path, "new.rs");
        assert_eq!(
            (second.span.side, second.span.start, second.span.end),
            (Side::New, 1, 2)
        );
        assert_eq!(second.span.rows, (10, 11), "rows of the whole card");
        assert_eq!(second.span.quoted, quoted(&["+x", "+y"]));
        let first = card_comment_span(CARD, 4, 4).expect("a removed line");
        assert_eq!(
            (first.path.as_str(), first.span.side, first.span.start),
            ("src/a.rs", Side::Old, 2)
        );
        // The next file's headers at the end of a selection do not count; its lines do.
        assert_eq!(
            card_comment_span(CARD, 5, 8).map(|s| s.span.rows),
            Ok((5, 6))
        );
        assert_eq!(
            card_comment_span(CARD, 5, 10),
            Err(SpanRefusal::AcrossFiles)
        );
        assert_eq!(card_comment_span(CARD, 0, 2), Err(SpanRefusal::NoLines));
        assert_eq!(card_comment_span("", 0, 0), Err(SpanRefusal::NoLines));
        // The agent's own fragment has no numbers: refused, never guessed.
        let fragment = "--- a/x.rs\n+++ b/x.rs\n@@ edit @@\n-old\n+new\n";
        assert_eq!(
            card_comment_span(fragment, 3, 4),
            Err(SpanRefusal::Unnumbered)
        );
        for refusal in [SpanRefusal::AcrossFiles, SpanRefusal::Unnumbered] {
            assert!(!refusal.reason().is_empty());
        }
        // A comment made on a card is found again in the panel's diff of the same file.
        let panel = "diff --git a/new.rs b/new.rs\nnew file mode 100644\n--- /dev/null\n\
                     +++ b/new.rs\n@@ -0,0 +1,2 @@\n+x\n+y\n";
        let s = &second.span;
        assert_eq!(
            anchor(panel, s.side, s.start, s.end, &s.quoted).located,
            Located::Exact
        );
    }

    #[test]
    fn split_hunks_and_build_hunk_patch_select_requested_hunks() {
        let chunk = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,4 @@ fn first()
 a
+b
 c
 d
@@ -20,3 +21,2 @@ fn second()
 x
-y
 z
";
        let (header, hunks) = split_hunks(chunk).expect("two hunks");
        assert!(header.contains("--- a/src/lib.rs"));
        assert!(header.contains("+++ b/src/lib.rs"));
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].new_start, 1);
        assert_eq!(hunks[1].new_start, 21);

        let only_second = build_hunk_patch(chunk, &[1]).expect("patch");
        assert!(only_second.contains("--- a/src/lib.rs"));
        assert!(!only_second.contains("fn first()"));
        assert!(only_second.contains("@@ -20,3 +21,2 @@ fn second()"));
        assert!(only_second.contains("-y"));

        assert_eq!(build_hunk_patch(chunk, &[]), None);
        assert_eq!(build_hunk_patch(chunk, &[2]), None);
        assert_eq!(split_hunks("Binary files a/x and b/x differ\n"), None);
    }
}

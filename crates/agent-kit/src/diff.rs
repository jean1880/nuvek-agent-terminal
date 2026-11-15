//! Pure logic for the diff panel: which two trees to compare, how to read
//! `git diff --numstat -z`, how to colour each line of a unified diff, and how
//! to cap a diff too large to show.
//!
//! Nothing here runs git; `crate::git` does, and the window renders.

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
}

//! A fork's baseline: the source's working tree when the fork was made, so each side of a
//! comparison can show what it changed since then.
//!
//! [`take_fork_baseline`] snapshots the folder exactly as a turn checkpoint does
//! ([`snapshot_tree`]: through a private index, skipping secret-looking, oversized and nested
//! untracked files, never touching the user's index, HEAD, working tree or stash) and records
//! the tree as a commit under the checkpoint identity. Once the fork's thread is stored,
//! [`keep_fork_baseline`] points `refs/agent-terminal/fork/<thread>` at it, created with an
//! empty old value so nothing is overwritten; until then the commit is only unreferenced, which
//! git keeps for weeks. The ref is never pruned by age ([`super::refs_to_prune`]) and goes with
//! its thread ([`drop_fork_baseline`]), or with every other `refs/agent-terminal/*` ref at
//! uninstall.
//!
//! [`fork_changes`] is one side of a comparison: a fresh snapshot of the side's folder diffed
//! against the baseline tree, tree to tree ([`super::diff_trees`]: git's own diff, no external
//! diff or textconv driver, capped as the diff panel is).
//!
//! Everything here shells out to git and blocks: call it off the main thread.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use agent_core::fork_compare::is_object_id;

use super::{
    commit_tree, create_ref, delete_ref, diff_trees, discover, git_output, git_raw, is_secret_path,
    snapshot_tree, RepoInfo, ScratchIndex, Skipped, SnapshotOutcome, TabDiff,
    MAX_SKIPPED_IN_MESSAGE, QUERY_TIMEOUT_SECS, REF_ROOT, SNAPSHOT_CONFIG, SNAPSHOT_TIMEOUT_SECS,
};

/// Laying a snapshot into a new worktree writes the whole tree: far beyond a query's budget.
const LAY_IN_TIMEOUT_SECS: u64 = 120;

/// Where fork baselines live: `refs/agent-terminal/fork/<thread>`, under [`REF_ROOT`].
pub const FORK_REF_ROOT: &str = "refs/agent-terminal/fork";

/// Most characters of a thread id a ref is named for (a thread id is a UUID).
const MAX_THREAD_ID: usize = 64;

/// Each snapshot's private index is its own: comparisons and forks may run at once.
static SNAPSHOTS: AtomicU64 = AtomicU64::new(0);

/// The baseline ref of fork thread `thread`; `None` for an id that could not safely name a ref
/// (anything but ASCII letters, digits and inner `-`).
pub fn fork_ref(thread: &str) -> Option<String> {
    let fits = !thread.is_empty()
        && thread.len() <= MAX_THREAD_ID
        && !thread.starts_with('-')
        && thread
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-');
    fits.then(|| format!("{FORK_REF_ROOT}/{thread}"))
}

/// Whether `refname` is a fork baseline (spared by checkpoint pruning).
pub fn is_fork_ref(refname: &str) -> bool {
    refname
        .strip_prefix(FORK_REF_ROOT)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// A baseline taken at a fork ([`take_fork_baseline`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkBaseline {
    /// The working tree it was taken in, where its ref goes.
    pub toplevel: PathBuf,
    pub tree: String,
    /// The commit recording `tree` (on top of HEAD, under the checkpoint identity).
    pub commit: String,
    /// Untracked files left out, as a checkpoint leaves them out.
    pub skipped: Vec<Skipped>,
}

/// What [`take_fork_baseline`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaselineOutcome {
    /// The folder is not in a repository (or git is not installed).
    NotRepo,
    /// The index was locked (most likely the agent, mid-commit): no baseline.
    Busy,
    Taken(ForkBaseline),
}

fn baseline_message(skipped: &[Skipped]) -> String {
    let mut message = "fork baseline".to_owned();
    if !skipped.is_empty() {
        message.push_str("\n\nNot captured:\n");
        for entry in skipped.iter().take(MAX_SKIPPED_IN_MESSAGE) {
            message.push_str(&format!("- {entry}\n"));
        }
        if skipped.len() > MAX_SKIPPED_IN_MESSAGE {
            message.push_str(&format!(
                "- … {} more\n",
                skipped.len() - MAX_SKIPPED_IN_MESSAGE
            ));
        }
    }
    message
}

fn snapshot_tag(what: &str) -> String {
    format!(
        "{what}-{}-{}",
        std::process::id(),
        SNAPSHOTS.fetch_add(1, Ordering::Relaxed)
    )
}

/// Snapshots the working tree `dir` is in and records it as a commit, naming no ref yet
/// ([`keep_fork_baseline`] does, once the fork's thread exists). Leaves the user's index, HEAD,
/// working tree and stash exactly as they were.
pub fn take_fork_baseline(dir: &Path) -> Result<BaselineOutcome, String> {
    let Some(repo) = discover(dir)? else {
        return Ok(BaselineOutcome::NotRepo);
    };
    let snapshot = match snapshot_tree(&repo, &snapshot_tag("fork"))? {
        SnapshotOutcome::Busy => return Ok(BaselineOutcome::Busy),
        SnapshotOutcome::Taken(snapshot) => snapshot,
    };
    let commit = commit_tree(
        &repo,
        &snapshot.tree,
        repo.head.as_deref(),
        &baseline_message(&snapshot.skipped),
    )?;
    Ok(BaselineOutcome::Taken(ForkBaseline {
        toplevel: repo.toplevel,
        tree: snapshot.tree,
        commit,
        skipped: snapshot.skipped,
    }))
}

/// Points fork `thread`'s baseline ref at `commit` in the repository `dir` is in. Refuses if
/// the ref already exists (an empty old value): nothing is overwritten.
pub fn keep_fork_baseline(dir: &Path, thread: &str, commit: &str) -> Result<(), String> {
    let refname = fork_ref(thread).ok_or("This thread's id cannot name a git ref")?;
    if !is_object_id(commit) {
        return Err("The fork's starting point is not a git object id".to_owned());
    }
    let repo = discover(dir)?.ok_or("The folder is no longer in a git repository")?;
    create_ref(&repo, &refname, commit)
}

/// What `refname` points at, if it exists.
fn resolve(repo: &RepoInfo, refname: &str) -> Result<Option<String>, String> {
    let out = git_output(
        &repo.toplevel,
        [
            "rev-parse",
            "-q",
            "--verify",
            &format!("{refname}^{{commit}}"),
        ],
        &[],
        QUERY_TIMEOUT_SECS,
    )?;
    let id = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    Ok((out.status.success() && !id.is_empty()).then_some(id))
}

/// Deletes fork `thread`'s baseline ref in the repository `dir` is in, only while it still
/// points at `commit` (the one the fork recorded): a ref someone else moved is left alone.
/// `Ok(false)` when there was nothing of the fork's to delete.
pub fn drop_fork_baseline(dir: &Path, thread: &str, commit: &str) -> Result<bool, String> {
    let Some(refname) = fork_ref(thread) else {
        return Ok(false);
    };
    if !dir.is_dir() {
        return Ok(false);
    }
    let Some(repo) = discover(dir)? else {
        return Ok(false);
    };
    if resolve(&repo, &refname)?.as_deref() != Some(commit) {
        return Ok(false);
    }
    debug_assert!(refname.starts_with(&format!("{REF_ROOT}/")));
    delete_ref(&repo, &refname)?;
    Ok(true)
}

/// The paths of snapshot `tree` that a fork's worktree takes from HEAD instead: names that look
/// like secrets ([`is_secret_path`]: a tracked `.env` is captured by the snapshot's `add -u`), and
/// what the source's own index marks assume-unchanged or skip-worktree (its content is the user's
/// to keep out of git's sight). `source` is the source's toplevel, `worktree` the fork's.
fn head_only_paths(source: &Path, worktree: &Path, tree: &str) -> Result<Vec<OsString>, String> {
    let mut paths: BTreeSet<Vec<u8>> = BTreeSet::new();
    let listed = git_raw(
        worktree,
        ["ls-tree", "-r", "-z", "--name-only", tree],
        &[],
        SNAPSHOT_TIMEOUT_SECS,
    )?;
    for path in listed.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        if is_secret_path(Path::new(OsStr::from_bytes(path))) {
            paths.insert(path.to_vec());
        }
    }
    // `ls-files -v`: a lowercase tag is assume-unchanged, `S` skip-worktree.
    let flagged = git_raw(
        source,
        SNAPSHOT_CONFIG
            .iter()
            .copied()
            .chain(["ls-files", "-v", "-z"]),
        &[],
        SNAPSHOT_TIMEOUT_SECS,
    )?;
    for entry in flagged.split(|b| *b == 0) {
        if entry.len() > 2
            && entry[1] == b' '
            && (entry[0].is_ascii_lowercase() || entry[0] == b'S')
        {
            paths.insert(entry[2..].to_vec());
        }
    }
    Ok(paths
        .into_iter()
        .map(|p| OsStr::from_bytes(&p).to_os_string())
        .collect())
}

/// One entry of `ls-tree -z` (`<mode> <type> <oid>\t<path>`) as the mode and object id of a file,
/// a link or a submodule (gitlink) at exactly `path`, its bytes compared whole; `None` for
/// anything else.
fn tree_entry(entry: &[u8], path: &[u8]) -> Option<(String, String)> {
    let tab = entry.iter().position(|b| *b == b'\t')?;
    let (meta, name) = (&entry[..tab], &entry[tab + 1..]);
    if name != path {
        return None;
    }
    let meta = std::str::from_utf8(meta).ok()?;
    match meta.split(' ').collect::<Vec<_>>().as_slice() {
        [mode, "blob", oid] | [mode @ "160000", "commit", oid] if is_object_id(oid) => {
            Some(((*mode).to_owned(), (*oid).to_owned()))
        }
        _ => None,
    }
}

/// HEAD's entry for `path` in the repository of `dir` ([`tree_entry`]).
fn head_entry(dir: &Path, path: &OsStr) -> Result<Option<(String, String)>, String> {
    let args: [&OsStr; 6] = [
        OsStr::new("--literal-pathspecs"),
        OsStr::new("ls-tree"),
        OsStr::new("-z"),
        OsStr::new("HEAD"),
        OsStr::new("--"),
        path,
    ];
    let out = git_raw(dir, args, &[], SNAPSHOT_TIMEOUT_SECS)?;
    Ok(out
        .split(|b| *b == 0)
        .find_map(|entry| tree_entry(entry, path.as_bytes())))
}

/// What [`lay_in_fork_worktree`] laid in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaidIn {
    /// The tree laid in: the snapshot, with HEAD's content at the paths that take it.
    pub tree: String,
    /// That tree recorded as a commit on top of HEAD (the checkpoint identity, on no branch):
    /// the fork's baseline, which its ref keeps and its changes are compared against.
    pub commit: String,
    /// The paths that took HEAD's content (or were left out where HEAD has none).
    pub from_head: Vec<String>,
}

/// Lays snapshot `tree` (a fork's baseline of `source`) into `worktree`, a fresh worktree whose
/// branch is at the source's HEAD, as uncommitted changes only: its working tree becomes the
/// snapshot (modified, deleted and new files), while its index and branch stay at HEAD, so
/// nothing the app made ever lands on a branch. The paths [`head_only_paths`] names keep HEAD's
/// content (or are left out when HEAD has none), so a secret-looking or hidden-from-git file is
/// never copied. The tree laid in is recorded as the fork's baseline ([`LaidIn::commit`]), so
/// the fork's changes are compared against exactly what it started with and the baseline ref
/// keeps no such file either. Touches only the new worktree. Its error says how far it got.
/// Blocking.
pub fn lay_in_fork_worktree(source: &Path, worktree: &Path, tree: &str) -> Result<LaidIn, String> {
    let not_copied = |e: String| format!("the thread's uncommitted work could not be copied: {e}");
    if !is_object_id(tree) {
        return Err(not_copied(
            "the fork's starting point is not a git object id".to_owned(),
        ));
    }
    let source = discover(source)
        .map_err(not_copied)?
        .ok_or_else(|| not_copied("the folder is no longer in a git repository".to_owned()))?;
    let fork = discover(worktree)
        .map_err(not_copied)?
        .ok_or_else(|| not_copied("the new worktree is not a git working tree".to_owned()))?;
    let head_only = head_only_paths(&source.toplevel, &fork.toplevel, tree).map_err(not_copied)?;
    let dir = fork.toplevel.as_path();
    // The tree to lay in, made in a private index of the fork's own git directory.
    let laid = if head_only.is_empty() {
        tree.to_owned()
    } else {
        let work = fork.git_dir.join("agent-terminal");
        std::fs::create_dir_all(&work)
            .map_err(|err| not_copied(format!("could not create {}: {err}", work.display())))?;
        let scratch = ScratchIndex(work.join(format!("index-{}", snapshot_tag("forkwt"))));
        let env = [("GIT_INDEX_FILE", scratch.0.as_os_str())];
        let private = |args: &[&OsStr], timeout: u64| {
            git_raw(
                dir,
                SNAPSHOT_CONFIG
                    .iter()
                    .map(OsStr::new)
                    .chain(args.iter().copied()),
                &env,
                timeout,
            )
        };
        private(
            &[OsStr::new("read-tree"), OsStr::new(tree)],
            LAY_IN_TIMEOUT_SECS,
        )
        .map_err(not_copied)?;
        for path in &head_only {
            match head_entry(dir, path).map_err(not_copied)? {
                Some((mode, oid)) => private(
                    &[
                        OsStr::new("update-index"),
                        OsStr::new("--add"),
                        OsStr::new("--replace"),
                        OsStr::new("--cacheinfo"),
                        OsStr::new(&mode),
                        OsStr::new(&oid),
                        path,
                    ],
                    SNAPSHOT_TIMEOUT_SECS,
                ),
                None => private(
                    &[
                        OsStr::new("update-index"),
                        OsStr::new("--force-remove"),
                        OsStr::new("--"),
                        path,
                    ],
                    SNAPSHOT_TIMEOUT_SECS,
                ),
            }
            .map_err(not_copied)?;
        }
        let written =
            private(&[OsStr::new("write-tree")], LAY_IN_TIMEOUT_SECS).map_err(not_copied)?;
        String::from_utf8_lossy(&written).trim().to_owned()
    };
    let commit = commit_tree(&fork, &laid, fork.head.as_deref(), &baseline_message(&[]))
        .map_err(not_copied)?;
    // The working tree to the snapshot, then the index back to HEAD: the work is uncommitted.
    git_raw(
        dir,
        ["read-tree", "-u", "--reset", &laid],
        &[],
        LAY_IN_TIMEOUT_SECS,
    )
    .map_err(|e| format!("copying the thread's uncommitted work stopped part-way: {e}"))?;
    git_raw(dir, ["reset", "-q"], &[], LAY_IN_TIMEOUT_SECS).map_err(|e| {
        format!(
            "the thread's uncommitted work was copied, but the new worktree's index could not \
             be set back to its branch's commit, so the work may show as staged: {e}"
        )
    })?;
    Ok(LaidIn {
        tree: laid,
        commit,
        from_head: head_only
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
    })
}

/// One side of a fork comparison ([`fork_changes`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForkChanges {
    /// The folder is not in a repository (or git is not installed).
    NotRepo,
    /// Why there is nothing to show.
    Unavailable(String),
    /// The side's working tree now, against the baseline.
    Ready {
        /// The working tree the side is in: two sides in one are one folder's changes.
        toplevel: PathBuf,
        diff: TabDiff,
    },
}

/// What changed in the working tree `dir` is in since `base_tree` (a fork's baseline): a fresh
/// snapshot (as [`take_fork_baseline`] takes one, touching nothing of the user's) diffed tree to
/// tree against it.
pub fn fork_changes(dir: &Path, base_tree: Option<&str>) -> Result<ForkChanges, String> {
    if !dir.is_dir() {
        return Ok(ForkChanges::Unavailable(
            "The thread's folder no longer exists".to_owned(),
        ));
    }
    let Some(repo) = discover(dir)? else {
        return Ok(ForkChanges::NotRepo);
    };
    let Some(base) = base_tree.filter(|b| is_object_id(b)) else {
        return Ok(ForkChanges::Unavailable(
            "No starting point was recorded when this fork was made: git was busy then, the \
             folder was not in a repository, checkpoints were switched off, or the fork was \
             made by an earlier version"
                .to_owned(),
        ));
    };
    let present = git_output(
        &repo.toplevel,
        ["cat-file", "-e", &format!("{base}^{{tree}}")],
        &[],
        QUERY_TIMEOUT_SECS,
    )?;
    if !present.status.success() {
        return Ok(ForkChanges::Unavailable(
            "The fork's starting point is not in this folder's repository".to_owned(),
        ));
    }
    let now = match snapshot_tree(&repo, &snapshot_tag("compare"))? {
        SnapshotOutcome::Taken(snapshot) => snapshot.tree,
        SnapshotOutcome::Busy => {
            return Ok(ForkChanges::Unavailable(
                "Git is busy in this repository; refresh in a moment".to_owned(),
            ))
        }
    };
    let diff = diff_trees(&repo, base, &now)?;
    Ok(ForkChanges::Ready {
        toplevel: repo.toplevel,
        diff,
    })
}

#[cfg(test)]
mod tests {
    use super::super::{is_secret_path, list_refs, refs_to_prune, RefEntry};
    use super::*;
    use crate::test_support::{git as sh, git_or_skip, init_repo_at};
    use std::fs;

    #[test]
    fn a_fork_ref_names_only_a_safe_thread_id_under_the_app_namespace() {
        let id = "0f8fad5b-d9cb-469f-a165-70867728950e";
        let name = fork_ref(id).expect("a uuid names a ref");
        assert_eq!(name, format!("refs/agent-terminal/fork/{id}"));
        // Uninstall removes everything under the app's root.
        assert!(name.starts_with(&format!("{REF_ROOT}/")));
        assert!(is_fork_ref(&name));
        for bad in [
            "",
            "-x",
            "a/b",
            "a..b",
            "a b",
            "x.lock",
            "é",
            &"a".repeat(65),
        ] {
            assert_eq!(fork_ref(bad), None, "{bad:?}");
        }
        assert!(!is_fork_ref("refs/agent-terminal/forked/x"));
        assert!(!is_fork_ref("refs/agent-terminal/1/0001"));
    }

    #[test]
    fn checkpoint_pruning_never_takes_a_baseline() {
        let entry = |refname: &str, time| RefEntry {
            refname: refname.to_owned(),
            commit: "c".into(),
            tree: "t".into(),
            time,
            parent: None,
        };
        let entries = [
            entry("refs/agent-terminal/fork/abc", 1),
            entry("refs/agent-terminal/1/0001", 1),
        ];
        assert_eq!(
            refs_to_prune(&entries, 1, 50, 1_000),
            ["refs/agent-terminal/1/0001"],
            "an expired checkpoint goes; a baseline as old stays"
        );
    }

    /// A repository with a commit, and the state a baseline must leave alone.
    fn repo() -> Option<tempfile::TempDir> {
        if !git_or_skip() {
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        init_repo_at(
            dir.path(),
            &[
                ("a.txt", b"one\n"),
                ("b.txt", b"keep\n"),
                (".gitignore", b""),
            ],
        );
        Some(dir)
    }

    fn user_state(dir: &Path) -> (Vec<u8>, String, String, String, String) {
        let mut files: Vec<String> = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(&d).unwrap() {
                let p = e.unwrap().path();
                if p.file_name().is_some_and(|n| n == ".git") {
                    continue;
                }
                if p.is_dir() {
                    stack.push(p);
                } else {
                    files.push(format!("{}:{:?}", p.display(), fs::read(&p).unwrap()));
                }
            }
        }
        files.sort();
        (
            fs::read(dir.join(".git/index")).unwrap(),
            sh(dir, &["rev-parse", "HEAD"]),
            sh(dir, &["symbolic-ref", "HEAD"]),
            sh(dir, &["stash", "list"]),
            files.join("\n"),
        )
    }

    fn taken(outcome: BaselineOutcome) -> ForkBaseline {
        match outcome {
            BaselineOutcome::Taken(b) => b,
            other => panic!("expected a baseline, got {other:?}"),
        }
    }

    fn ready(outcome: ForkChanges) -> (PathBuf, TabDiff) {
        match outcome {
            ForkChanges::Ready { toplevel, diff } => (toplevel, diff),
            other => panic!("expected changes, got {other:?}"),
        }
    }

    fn paths(diff: &TabDiff) -> Vec<&str> {
        diff.stats.iter().map(|s| s.path.as_str()).collect()
    }

    #[test]
    fn fork_baseline_leaves_head_index_and_stash_alone() {
        let Some(tmp) = repo() else { return };
        let dir = tmp.path();
        // Staged, unstaged and untracked work, and a stash entry of the user's.
        fs::write(dir.join("b.txt"), "stashed\n").unwrap();
        sh(dir, &["stash", "push", "-q", "-m", "mine"]);
        fs::write(dir.join("a.txt"), "staged\n").unwrap();
        sh(dir, &["add", "a.txt"]);
        fs::write(dir.join("a.txt"), "staged then edited\n").unwrap();
        fs::write(dir.join("new.txt"), "untracked\n").unwrap();
        let before = user_state(dir);
        let head = sh(dir, &["rev-parse", "HEAD"]);

        let baseline = taken(take_fork_baseline(dir).unwrap());
        assert_eq!(
            user_state(dir),
            before,
            "the baseline touched the user's state"
        );
        assert_eq!(
            sh(
                dir,
                &["cat-file", "-p", &format!("{}:a.txt", baseline.tree)]
            ),
            "staged then edited",
            "the working tree as it is, not the index"
        );
        assert_eq!(
            sh(
                dir,
                &["cat-file", "-p", &format!("{}:new.txt", baseline.tree)]
            ),
            "untracked"
        );
        assert_eq!(
            sh(dir, &["rev-parse", &format!("{}^", baseline.commit)]),
            head,
            "recorded on top of HEAD"
        );
        assert_eq!(
            sh(
                dir,
                &["rev-parse", &format!("{}^{{tree}}", baseline.commit)]
            ),
            baseline.tree
        );
        assert_eq!(
            sh(dir, &["for-each-ref", "refs/agent-terminal"]),
            "",
            "no ref before the fork's thread exists"
        );
        // The private index is gone.
        let work = dir.join(".git/agent-terminal");
        assert!(fs::read_dir(&work).map_or(true, |mut d| d.next().is_none()));

        let thread = "0f8fad5b-d9cb-469f-a165-70867728950e";
        keep_fork_baseline(dir, thread, &baseline.commit).unwrap();
        assert_eq!(
            user_state(dir),
            before,
            "keeping it touched the user's state"
        );
        let refname = fork_ref(thread).unwrap();
        assert_eq!(sh(dir, &["rev-parse", &refname]), baseline.commit);
        // Created with an empty old value: a second fork's ref of that name is refused.
        fs::write(dir.join("other.txt"), "other\n").unwrap();
        let other = taken(take_fork_baseline(dir).unwrap());
        fs::remove_file(dir.join("other.txt")).unwrap();
        assert!(keep_fork_baseline(dir, thread, &other.commit).is_err());
        assert_eq!(sh(dir, &["rev-parse", &refname]), baseline.commit);
        // HEAD and the branch never moved; the stash is the user's one entry.
        assert_eq!(sh(dir, &["rev-parse", "HEAD"]), head);
        assert_eq!(sh(dir, &["stash", "list"]).lines().count(), 1);
        // It is a checkpoint-namespace ref that the checkpoint listing sees but never prunes.
        let listed = list_refs(&discover(dir).unwrap().unwrap()).unwrap();
        assert!(listed.iter().any(|e| e.refname == refname));
        assert!(refs_to_prune(&listed, 0, 0, u64::MAX).is_empty());

        // Deleted with its thread, and only while it is still the fork's.
        assert!(!drop_fork_baseline(dir, thread, &other.commit).unwrap());
        assert_eq!(sh(dir, &["rev-parse", &refname]), baseline.commit);
        assert!(drop_fork_baseline(dir, thread, &baseline.commit).unwrap());
        assert_eq!(sh(dir, &["for-each-ref", "refs/agent-terminal"]), "");
        assert!(!drop_fork_baseline(dir, thread, &baseline.commit).unwrap());
        assert_eq!(user_state(dir), before);
    }

    #[test]
    fn fork_baseline_skips_secret_untracked_files() {
        let Some(tmp) = repo() else { return };
        let dir = tmp.path();
        fs::write(dir.join(".env"), "TOKEN=x\n").unwrap();
        fs::write(dir.join("tls.key"), "key\n").unwrap();
        fs::write(dir.join("notes.txt"), "fine\n").unwrap();
        let baseline = taken(take_fork_baseline(dir).unwrap());
        let files = sh(dir, &["ls-tree", "-r", "--name-only", &baseline.tree]);
        assert!(files.lines().any(|f| f == "notes.txt"), "{files}");
        for skipped_name in [".env", "tls.key"] {
            assert!(is_secret_path(Path::new(skipped_name)));
            assert!(
                !files.lines().any(|f| f == skipped_name),
                "{skipped_name} in {files}"
            );
            assert!(baseline.skipped.iter().any(|s| s.path == skipped_name));
        }
        // And the diff since the fork never brings it in either.
        fs::write(dir.join("later.pem"), "pem\n").unwrap();
        let (_, diff) = ready(fork_changes(dir, Some(&baseline.tree)).unwrap());
        assert!(diff.stats.is_empty(), "{:?}", paths(&diff));
    }

    #[test]
    fn a_busy_repository_or_none_gives_no_baseline() {
        let Some(tmp) = repo() else { return };
        fs::write(tmp.path().join(".git/index.lock"), "").unwrap();
        assert_eq!(
            take_fork_baseline(tmp.path()).unwrap(),
            BaselineOutcome::Busy
        );
        let plain = tempfile::tempdir().unwrap();
        assert_eq!(
            take_fork_baseline(plain.path()).unwrap(),
            BaselineOutcome::NotRepo
        );
        assert_eq!(
            fork_changes(plain.path(), None).unwrap(),
            ForkChanges::NotRepo
        );
    }

    #[test]
    fn changes_since_the_fork_are_tree_to_tree_and_driver_free() {
        let Some(tmp) = repo() else { return };
        let dir = tmp.path();
        fs::write(dir.join("a.txt"), "before the fork\n").unwrap();
        let baseline = taken(take_fork_baseline(dir).unwrap());
        // An external diff and a textconv driver that would leave a mark if they ran.
        let mark = dir.join(".git/driver-ran");
        let script = format!("touch '{}'; echo DRIVER", mark.display());
        sh(dir, &["config", "diff.external", &script]);
        sh(dir, &["config", "diff.marked.textconv", &script]);
        fs::write(dir.join(".gitattributes"), "*.txt diff=marked\n").unwrap();
        sh(dir, &["add", ".gitattributes"]);
        sh(dir, &["commit", "-q", "-m", "attributes"]);

        fs::write(dir.join("a.txt"), "after the fork\n").unwrap();
        fs::write(dir.join("c.txt"), "new\n").unwrap();
        let before = user_state(dir);
        let (toplevel, diff) = ready(fork_changes(dir, Some(&baseline.tree)).unwrap());
        assert_eq!(
            user_state(dir),
            before,
            "reading the changes touched the user's state"
        );
        assert_eq!(toplevel, discover(dir).unwrap().unwrap().toplevel);
        // What was there at the fork (a.txt's first edit) is not counted; the commit made
        // since is (its .gitattributes), being part of the tree now.
        assert_eq!(paths(&diff), [".gitattributes", "a.txt", "c.txt"]);
        assert!(diff.text.contains("-before the fork"), "{}", diff.text);
        assert!(diff.text.contains("+after the fork"), "{}", diff.text);
        assert!(!diff.text.contains("DRIVER"), "{}", diff.text);
        assert!(!mark.exists(), "an external diff or textconv driver ran");
        assert_eq!(diff.from, baseline.tree);
        assert_eq!(
            (diff.undo_to.as_deref(), diff.to_checkpoint.as_deref()),
            (None, None)
        );
        assert!(!diff.too_large);
        // A starting point that is not in the repository says so; none says why.
        let missing = "1".repeat(40);
        assert!(matches!(
            fork_changes(dir, Some(&missing)).unwrap(),
            ForkChanges::Unavailable(why) if why.contains("not in this folder's repository")
        ));
        assert!(matches!(
            fork_changes(dir, None).unwrap(),
            ForkChanges::Unavailable(why) if why.contains("No starting point")
        ));
        assert!(matches!(
            fork_changes(&dir.join("gone"), Some(&baseline.tree)).unwrap(),
            ForkChanges::Unavailable(why) if why.contains("no longer exists")
        ));
    }

    #[test]
    fn a_large_diff_is_capped_like_the_panels() {
        let Some(tmp) = repo() else { return };
        let dir = tmp.path();
        let baseline = taken(take_fork_baseline(dir).unwrap());
        let big: String = (0..crate::diff::MAX_DIFF_LINES + 50)
            .map(|i| format!("line {i}\n"))
            .collect();
        fs::write(dir.join("big.txt"), big).unwrap();
        let (_, diff) = ready(fork_changes(dir, Some(&baseline.tree)).unwrap());
        assert_eq!(paths(&diff), ["big.txt"]);
        assert!(diff.omitted_lines > 0, "the text is cut");
        assert!(diff.text.lines().count() <= crate::diff::MAX_DIFF_LINES);
    }

    #[test]
    fn compare_trees_across_two_worktrees() {
        let Some(tmp) = repo() else { return };
        let dir = tmp.path();
        fs::write(dir.join("a.txt"), "shared start\n").unwrap();
        let baseline = taken(take_fork_baseline(dir).unwrap());
        let parent = tempfile::tempdir().unwrap();
        let wt = parent.path().join("wt");
        sh(
            dir,
            &["worktree", "add", "-q", "-b", "side", wt.to_str().unwrap()],
        );
        keep_fork_baseline(&wt, "abc-1", &baseline.commit).unwrap();
        // Refs are shared: the main checkout sees the worktree's baseline.
        assert_eq!(
            sh(dir, &["rev-parse", &fork_ref("abc-1").unwrap()]),
            baseline.commit
        );
        fs::write(dir.join("main.txt"), "main side\n").unwrap();
        fs::write(wt.join("side.txt"), "worktree side\n").unwrap();
        let (main_top, main) = ready(fork_changes(dir, Some(&baseline.tree)).unwrap());
        let (wt_top, side) = ready(fork_changes(&wt, Some(&baseline.tree)).unwrap());
        assert_ne!(main_top, wt_top, "two folders, told apart");
        assert_eq!(paths(&main), ["main.txt"]);
        // The worktree started at HEAD, without the main checkout's uncommitted edit.
        assert_eq!(paths(&side), ["a.txt", "side.txt"]);
        assert!(drop_fork_baseline(dir, "abc-1", &baseline.commit).unwrap());
        assert!(!drop_fork_baseline(&dir.join("missing"), "abc-1", &baseline.commit).unwrap());
    }
}

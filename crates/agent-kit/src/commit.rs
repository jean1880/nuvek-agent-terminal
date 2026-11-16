//! A commit the user confirms in the diff panel: the chosen paths, committed in one step through
//! the user's own git, so their hooks, signing and identity apply.
//!
//! This is the one place the app writes the user's index or HEAD. It never uses a private index
//! (`GIT_INDEX_FILE`) and shares nothing with checkpoints: `snapshot_tree`, `take_checkpoint` and
//! `restore` are untouched, and so is `refs/agent-terminal/*`.
//!
//! The paths are staged (`git add -A -- <paths>`) and then committed with `git commit -m <message>
//! -- <paths>`, whose `--only` meaning commits exactly those paths: whatever else the user staged
//! stays staged and is not committed. Every pathspec is literal (`--literal-pathspecs`), so a
//! file named `*` or `:(exclude)x` names only itself. Nothing is ever skipped, amended or forced:
//! [`commit_args`] and [`add_args`] never carry `--no-verify`, `-n`, `--amend`, `--allow-empty`,
//! `-a` or `--no-gpg-sign`.
//!
//! Before anything is staged, the commit checks that the branch and HEAD are the ones the dialog
//! showed, and refuses a chosen path that is now a folder (a tracked file replaced by one), which
//! `add -A` would take whole, files the panel never listed included. Each step runs in a process
//! group of its own, so a timeout ends the hooks and helpers git started too.
//!
//! Blocking: run everything here off the main thread.

use crate::git::{
    failure_message, git_output, git_output_in_group, git_raw, RepoInfo, QUERY_TIMEOUT_SECS,
};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

/// Timeout for each step of a commit, longer than a query's because the user's hooks run.
pub const COMMIT_TIMEOUT_SECS: u64 = 120;

/// At most this many paths go in one commit from the panel, and at most [`MAX_PATH_BYTES`] of
/// them, so the command line stays well inside the system's argument limit.
pub const MAX_PATHS: usize = 1_000;
/// See [`MAX_PATHS`].
pub const MAX_PATH_BYTES: usize = 64 * 1024;

/// An operation git has left half done, during which the panel offers no commit: finishing it
/// is the user's, in a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InProgress {
    Merge,
    Rebase,
    CherryPick,
    Revert,
}

impl InProgress {
    /// Every operation, in the order they are looked for.
    pub const ALL: [InProgress; 4] = [
        InProgress::Merge,
        InProgress::Rebase,
        InProgress::CherryPick,
        InProgress::Revert,
    ];

    /// Why there is no commit from the panel, as a sentence.
    pub fn reason(self) -> &'static str {
        match self {
            InProgress::Merge => "A merge is in progress: finish or abort it in a terminal",
            InProgress::Rebase => "A rebase is in progress: finish or abort it in a terminal",
            InProgress::CherryPick => {
                "A cherry-pick is in progress: finish or abort it in a terminal"
            }
            InProgress::Revert => "A revert is in progress: finish or abort it in a terminal",
        }
    }

    /// The files in a worktree's git directory that mark the operation.
    fn markers(self) -> &'static [&'static str] {
        match self {
            InProgress::Merge => &["MERGE_HEAD"],
            InProgress::Rebase => &["rebase-merge", "rebase-apply"],
            InProgress::CherryPick => &["CHERRY_PICK_HEAD"],
            InProgress::Revert => &["REVERT_HEAD"],
        }
    }
}

/// The operation `git_dir` (a worktree's own git directory) is in the middle of, if any.
pub fn in_progress_op(git_dir: &Path) -> Option<InProgress> {
    InProgress::ALL.into_iter().find(|op| {
        op.markers()
            .iter()
            .any(|marker| std::fs::symlink_metadata(git_dir.join(marker)).is_ok())
    })
}

/// The operation `dir`'s repository (its worktree) is in the middle of, if any. `Ok(None)` too
/// when `dir` is in no repository.
pub fn in_progress(dir: &Path) -> Result<Option<InProgress>, String> {
    Ok(crate::git::discover(dir)?.and_then(|repo| in_progress_op(&repo.git_dir)))
}

/// What the commit dialog shows about the repository before anything is committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitContext {
    /// Where the commit runs: the paths are relative to it.
    pub toplevel: PathBuf,
    /// The branch checked out; `None` on a detached HEAD.
    pub branch: Option<String>,
    /// The commit HEAD points at; `None` before the first.
    pub head: Option<String>,
    /// No commit yet: this is the first.
    pub unborn: bool,
    /// The branch is the repository's default one (its remote's `HEAD`, else `main` or `master`).
    pub on_default_branch: bool,
    pub in_progress: Option<InProgress>,
    /// Paths the user staged themselves that are not among the chosen ones: they stay staged
    /// and are not part of this commit.
    pub staged_elsewhere: Vec<String>,
}

impl CommitContext {
    /// A warning for committing here, when one is due: on a detached HEAD, or on the default
    /// branch. Neither is refused.
    pub fn branch_warning(&self) -> Option<String> {
        match &self.branch {
            None => Some(
                "HEAD is detached: the commit is on no branch, and switching away leaves it \
                 behind unless a branch is made for it"
                    .to_owned(),
            ),
            Some(branch) if self.on_default_branch => Some(format!(
                "{branch} is the default branch: this commits straight onto it"
            )),
            Some(_) => None,
        }
    }

    /// What the commit must still find when it runs: the branch and HEAD shown here.
    pub fn expected(&self) -> Expected {
        Expected {
            branch: self.branch.clone(),
            head: self.head.clone(),
        }
    }
}

/// The branch and HEAD the user saw in the dialog. A commit that finds either moved does
/// nothing: what the dialog warned about (or did not) would no longer be true.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expected {
    pub branch: Option<String>,
    pub head: Option<String>,
}

/// Reads what committing `chosen` in `dir`'s repository would involve. Reads only.
pub fn commit_context(dir: &Path, chosen: &[String]) -> Result<CommitContext, String> {
    context_with(dir, chosen, &[])
}

fn context_with(
    dir: &Path,
    chosen: &[String],
    env: &[(&str, &OsStr)],
) -> Result<CommitContext, String> {
    let repo = repository(dir)?;
    let top = repo.toplevel.as_path();
    let status = git_raw(
        top,
        ["status", "--porcelain=v2", "-z", "--untracked-files=no"],
        env,
        QUERY_TIMEOUT_SECS,
    )?;
    let chosen: BTreeSet<&str> = chosen.iter().map(String::as_str).collect();
    let staged_elsewhere = staged_paths(&status)
        .into_iter()
        .filter(|(path, orig)| {
            !chosen.contains(path.as_str())
                && orig.as_deref().is_none_or(|orig| !chosen.contains(orig))
        })
        .map(|(path, _)| path)
        .collect();
    let on_default_branch = match &repo.branch {
        Some(branch) => default_branch(top, env).as_deref() == Some(branch.as_str()),
        None => false,
    };
    Ok(CommitContext {
        toplevel: repo.toplevel.clone(),
        branch: repo.branch.clone(),
        head: repo.head.clone(),
        unborn: repo.head.is_none(),
        on_default_branch,
        in_progress: in_progress_op(&repo.git_dir),
        staged_elsewhere,
    })
}

fn repository(dir: &Path) -> Result<RepoInfo, String> {
    crate::git::discover(dir)?.ok_or_else(|| "This folder is not inside a git repository".into())
}

/// The repository's default branch, by its short name: the branch its `origin` remote's `HEAD`
/// names, else `main` or `master`, whichever exists here.
fn default_branch(top: &Path, env: &[(&str, &OsStr)]) -> Option<String> {
    let line = |args: &[&str]| {
        git_output(top, args, env, QUERY_TIMEOUT_SECS)
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
            .filter(|s| !s.is_empty())
    };
    if let Some(name) = line(&["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]) {
        if let Some(branch) = name.strip_prefix("origin/") {
            return Some(branch.to_owned());
        }
    }
    ["main", "master"]
        .into_iter()
        .map(str::to_owned)
        .find(|b| line(&["rev-parse", "--verify", "-q", &format!("refs/heads/{b}")]).is_some())
}

/// The paths `git status --porcelain=v2 -z` reports as staged (their index differs from HEAD),
/// each with the path it was renamed from, if it was.
pub fn staged_paths(status: &[u8]) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    let mut records = status.split(|b| *b == 0).filter(|r| !r.is_empty());
    while let Some(record) = records.next() {
        let text = String::from_utf8_lossy(record);
        let (kind, fields) = match text.as_bytes().first() {
            // `1 XY sub mH mI mW hH hI path`
            Some(b'1') => (1, 9),
            // `2 XY sub mH mI mW hH hI Xscore path`, then the original path as its own record.
            Some(b'2') => (2, 10),
            // `u XY sub m1 m2 m3 mW h1 h2 h3 path`: unmerged, staged by definition.
            Some(b'u') => (3, 11),
            _ => continue,
        };
        let parts: Vec<&str> = text.splitn(fields, ' ').collect();
        let orig = (kind == 2)
            .then(|| {
                records
                    .next()
                    .map(|r| String::from_utf8_lossy(r).into_owned())
            })
            .flatten();
        let (Some(xy), Some(path)) = (parts.get(1), parts.get(fields - 1)) else {
            continue;
        };
        let staged = kind == 3 || !xy.starts_with('.');
        if staged && !path.is_empty() {
            out.push(((*path).to_owned(), orig));
        }
    }
    out
}

/// A commit to make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRequest {
    /// The message, exactly as the user typed it.
    pub message: String,
    /// Repository-relative paths, as the diff named them.
    pub paths: Vec<String>,
    /// The branch and HEAD the dialog showed.
    pub expected: Expected,
}

/// A commit that was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Committed {
    /// The new commit, abbreviated.
    pub short: String,
}

/// Whether a path, as the diff named it, can be committed from the panel. A name that is not
/// valid UTF-8 reached the panel decoded lossily (with U+FFFD) and would name another file.
pub fn committable_path(path: &str) -> bool {
    let p = Path::new(path);
    !path.is_empty()
        && !p.is_absolute()
        && p.components().all(|c| matches!(c, Component::Normal(_)))
        && !path
            .chars()
            .any(|c| c.is_control() || c == char::REPLACEMENT_CHARACTER)
}

/// Refuses paths git should never see: none, too many, or one [`committable_path`] refuses.
pub fn validate_paths(paths: &[String]) -> Result<(), String> {
    if paths.is_empty() {
        return Err("Choose at least one file to commit".into());
    }
    let bytes: usize = paths.iter().map(|p| p.len() + 1).sum();
    if paths.len() > MAX_PATHS || bytes > MAX_PATH_BYTES {
        return Err(format!(
            "Too many files to commit from here ({}); choose fewer, or commit in a terminal",
            paths.len()
        ));
    }
    match paths.iter().find(|p| !committable_path(p)) {
        Some(path) => Err(format!(
            "Not a path that can be committed from here: {path}"
        )),
        None => Ok(()),
    }
}

/// Refuses a request git should never see: an empty message, or paths [`validate_paths`]
/// refuses.
pub fn validate(request: &CommitRequest) -> Result<(), String> {
    if request.message.trim().is_empty() {
        return Err("Write a commit message first".into());
    }
    validate_paths(&request.paths)
}

/// Refuses a chosen path that is a folder on disk, unless it is a submodule (a gitlink in the
/// index). Such a path is a tracked file replaced by a folder: `git add -A` would take it for a
/// folder and stage everything in it, files the panel never listed included. Nothing is staged.
fn refuse_folders(top: &Path, paths: &[String], env: &[(&str, &OsStr)]) -> Result<(), String> {
    for path in paths {
        let is_dir = std::fs::symlink_metadata(top.join(path)).is_ok_and(|m| m.is_dir());
        if !is_dir {
            continue;
        }
        let listed = git_raw(
            top,
            ["--literal-pathspecs", "ls-files", "-s", "-z", "--", path],
            env,
            QUERY_TIMEOUT_SECS,
        )?;
        let suffix = format!("\t{path}");
        let gitlink = listed
            .split(|b| *b == 0)
            .any(|record| record.starts_with(b"160000 ") && record.ends_with(suffix.as_bytes()));
        if !gitlink {
            return Err(format!(
                "{path} was a file and is now a folder. Nothing was staged or committed: commit \
                 this change in a terminal."
            ));
        }
    }
    Ok(())
}

/// Refuses a commit whose branch or HEAD moved since the dialog showed them.
fn check_expected(repo: &RepoInfo, expected: &Expected) -> Result<(), String> {
    if repo.branch != expected.branch || repo.head != expected.head {
        let now = repo.branch.as_deref().unwrap_or("a detached HEAD");
        return Err(format!(
            "The branch or its latest commit changed since the dialog opened (now on {now}). \
             Nothing was staged or committed: open Commit… again to see where it would go."
        ));
    }
    Ok(())
}

/// The arguments of the `git add` that stages the chosen paths: every change to them, new and
/// deleted files included, and nothing else.
pub fn add_args(paths: &[String]) -> Vec<String> {
    let mut args: Vec<String> = ["--literal-pathspecs", "add", "-A", "--"]
        .map(str::to_owned)
        .to_vec();
    args.extend(paths.iter().cloned());
    args
}

/// Where the message sits in [`commit_args`]: everything before it and after it depends on the
/// paths alone.
pub const MESSAGE_ARG: usize = 3;

/// The arguments of the `git commit` that commits exactly the chosen paths. The message is one
/// `-m` value, so whatever it says (a leading `-` included) is never read as an option.
pub fn commit_args(request: &CommitRequest) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--literal-pathspecs".to_owned(),
        "commit".to_owned(),
        "-m".to_owned(),
        request.message.clone(),
        "--".to_owned(),
    ];
    args.extend(request.paths.iter().cloned());
    args
}

/// Commits `request` in `dir`'s repository: stages the chosen paths, then commits them alone.
/// The user's hooks and signing run; a hook that fails leaves HEAD where it was (the paths stay
/// staged). Git's message on failure, with anything secret-shaped in it masked.
pub fn commit_paths(dir: &Path, request: &CommitRequest) -> Result<Committed, String> {
    commit_with(dir, request, &[], COMMIT_TIMEOUT_SECS)
}

fn commit_with(
    dir: &Path,
    request: &CommitRequest,
    env: &[(&str, &OsStr)],
    timeout_secs: u64,
) -> Result<Committed, String> {
    validate(request)?;
    let repo = repository(dir)?;
    if let Some(op) = in_progress_op(&repo.git_dir) {
        return Err(op.reason().to_owned());
    }
    check_expected(&repo, &request.expected)?;
    let top = repo.toplevel.as_path();
    refuse_folders(top, &request.paths, env)?;
    // Each step in a process group of its own: a timeout kills the hooks and signing helpers
    // git started, not git alone.
    let step = |args: Vec<String>| -> Result<(), String> {
        let out = git_output_in_group(top, &args, env, timeout_secs)
            .map_err(|why| failure(&why, timeout_secs))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(failure(&failure_message(&out.stderr), timeout_secs))
        }
    };
    step(add_args(&request.paths))?;
    step(commit_args(request)).map_err(|why| format!("{why}\n\n{STILL_STAGED}"))?;
    let short = git_raw(
        top,
        ["rev-parse", "--short", "HEAD"],
        env,
        QUERY_TIMEOUT_SECS,
    )
    .map(|out| String::from_utf8_lossy(&out).trim().to_owned())
    .unwrap_or_default();
    Ok(Committed { short })
}

/// Added to a failure after the files were staged: what the user's index now holds.
pub const STILL_STAGED: &str = "Nothing was committed. The chosen files stay staged as they are \
     on disk, replacing any part of them you had staged before; `git restore --staged -- <file>` \
     unstages one.";

/// What the user is told when a step fails: git's (and a hook's) own words, masked, with what a
/// timeout means.
fn failure(why: &str, timeout_secs: u64) -> String {
    let why = agent_core::redact::redact(why);
    if why.contains(&format!("did not finish within {timeout_secs}s")) {
        format!(
            "{why}\n\nIt was stopped, with the hooks and helpers it started. If .git/index.lock \
             is left behind, remove it once no git is running. Hooks and signing that ask for \
             input cannot be answered from here: commit in a terminal instead."
        )
    } else {
        why
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(message: &str, paths: &[&str]) -> CommitRequest {
        CommitRequest {
            message: message.to_owned(),
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
            expected: Expected {
                branch: None,
                head: None,
            },
        }
    }

    const FORBIDDEN: [&str; 8] = [
        "--no-verify",
        "-n",
        "--amend",
        "--allow-empty",
        "-a",
        "--all",
        "--no-gpg-sign",
        "--force",
    ];

    #[test]
    fn commit_args_never_skip_hooks_or_amend() {
        for req in [
            request("One line", &["a.rs"]),
            request("Subject\n\nBody one.\n\nBody two.", &["a.rs", "b/c.rs"]),
            request("-n", &["-a", "--amend", "--no-verify"]),
        ] {
            let args = commit_args(&req);
            assert_eq!(args[MESSAGE_ARG], req.message);
            // Every argument before `--` is an option of ours, except the message after `-m`.
            let split = args.iter().position(|a| a == "--").unwrap_or(args.len());
            let options: Vec<&String> = args[..split]
                .iter()
                .enumerate()
                .filter(|(i, _)| *i == 0 || args[i - 1] != "-m")
                .map(|(_, a)| a)
                .collect();
            for arg in &options {
                assert!(!FORBIDDEN.contains(&arg.as_str()), "{arg} in {args:?}");
            }
            assert_eq!(
                options,
                ["--literal-pathspecs", "commit", "-m"]
                    .iter()
                    .collect::<Vec<_>>()
            );
            assert_eq!(&args[split + 1..], req.paths.as_slice(), "paths follow --");
            let add = add_args(&req.paths);
            assert_eq!(&add[..4], ["--literal-pathspecs", "add", "-A", "--"]);
            assert_eq!(&add[4..], req.paths.as_slice());
        }
    }

    #[test]
    fn requests_git_should_never_see_are_refused() {
        assert!(validate(&request("Fix", &["a.rs", "-odd", "src/*"])).is_ok());
        assert!(validate(&request("  \n", &["a.rs"])).is_err());
        assert!(validate(&request("Fix", &[])).is_err());
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../../x",
            "./a",
            "a\nb",
            "caf\u{FFFD}.txt",
        ] {
            assert!(validate(&request("Fix", &[bad])).is_err(), "{bad:?}");
            assert!(!committable_path(bad), "{bad:?}");
        }
        let many: Vec<String> = (0..=MAX_PATHS).map(|i| format!("f{i}")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        assert!(validate(&request("Fix", &refs)).is_err());
    }

    #[test]
    fn commit_refused_during_merge_or_rebase() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(in_progress_op(dir.path()), None);
        for op in InProgress::ALL {
            // Exhaustive: a new operation fails to compile until it is listed and worded.
            match op {
                InProgress::Merge
                | InProgress::Rebase
                | InProgress::CherryPick
                | InProgress::Revert => {}
            }
            assert!(op.reason().ends_with("in a terminal"));
            for marker in op.markers() {
                let path = dir.path().join(marker);
                std::fs::write(&path, b"x").unwrap();
                assert_eq!(in_progress_op(dir.path()), Some(op), "{marker}");
                std::fs::remove_file(&path).unwrap();
            }
        }
    }

    #[test]
    fn staged_paths_reads_porcelain_v2() {
        let status = b"1 M. N... 100644 100644 100644 aaa bbb staged.rs\0\
                       1 .M N... 100644 100644 100644 aaa bbb unstaged.rs\0\
                       2 R. N... 100644 100644 100644 aaa bbb R100 new name.rs\0old.rs\0\
                       u UU N... 100644 100644 100644 100644 a b c conflict.rs\0\
                       # branch.oid abc\0";
        assert_eq!(
            staged_paths(status),
            vec![
                ("staged.rs".to_owned(), None),
                ("new name.rs".to_owned(), Some("old.rs".to_owned())),
                ("conflict.rs".to_owned(), None),
            ]
        );
    }

    #[test]
    fn a_detached_head_or_the_default_branch_is_warned_about_never_refused() {
        let mut ctx = CommitContext {
            toplevel: PathBuf::from("/r"),
            branch: Some("feat/x".into()),
            head: Some("abc".into()),
            unborn: false,
            on_default_branch: false,
            in_progress: None,
            staged_elsewhere: Vec::new(),
        };
        assert_eq!(ctx.branch_warning(), None);
        ctx.on_default_branch = true;
        ctx.branch = Some("main".into());
        assert!(ctx
            .branch_warning()
            .is_some_and(|w| w.starts_with("main is the default branch")));
        ctx.branch = None;
        assert!(ctx
            .branch_warning()
            .is_some_and(|w| w.starts_with("HEAD is detached")));
    }

    #[test]
    fn timeout_reports_possible_running_hook() {
        let message = failure("git did not finish within 120s", 120);
        assert!(message.contains("stopped, with the hooks and helpers it started"));
        assert!(message.contains(".git/index.lock"));
        assert_eq!(failure("fatal: nope", 120), "fatal: nope");
        let masked = failure("hook said ghp_abcdefghijklmnopqrstuvwxyz0123456789", 120);
        assert!(!masked.contains("ghp_abcdefghijklmnopqrstuvwxyz0123456789"));
    }

    /// Against real repositories, with a hermetic config. Skipped when git is not installed.
    mod repo {
        use super::*;
        use crate::test_support::{git as sh, git_or_skip, git_output as sh_output, init_repo_at};
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        /// Neither the user's global nor the system config reaches the commit; the repository's
        /// own (identity, no signing) does.
        const ENV: [(&str, &str); 2] = [
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
        ];

        fn env() -> Vec<(&'static str, &'static OsStr)> {
            ENV.iter().map(|(k, v)| (*k, OsStr::new(v))).collect()
        }

        fn identity(dir: &Path) {
            sh(dir, &["config", "user.name", "Tester"]);
            sh(dir, &["config", "user.email", "tester@example.com"]);
            sh(dir, &["config", "commit.gpgsign", "false"]);
        }

        fn new_repo() -> Option<tempfile::TempDir> {
            if !git_or_skip() {
                return None;
            }
            let dir = tempfile::tempdir().unwrap();
            init_repo_at(dir.path(), &[("a.txt", b"one\n"), ("b.txt", b"two\n")]);
            identity(dir.path());
            Some(dir)
        }

        /// A request as the dialog builds it: expecting the branch and HEAD it read.
        fn asked(dir: &Path, message: &str, paths: &[&str]) -> CommitRequest {
            let mut req = request(message, paths);
            req.expected = context_with(dir, &req.paths, &env()).unwrap().expected();
            req
        }

        fn commit(dir: &Path, message: &str, paths: &[&str]) -> Result<Committed, String> {
            commit_with(
                dir,
                &asked(dir, message, paths),
                &env(),
                COMMIT_TIMEOUT_SECS,
            )
        }

        /// Whether process `pid` is gone (or only a zombie waiting to be reaped).
        fn gone(pid: &str) -> bool {
            match fs::read_to_string(format!("/proc/{}/stat", pid.trim())) {
                Err(_) => true,
                // `pid (comm) state …`: the state follows the last `)`.
                Ok(stat) => stat
                    .rsplit_once(')')
                    .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
            }
        }

        fn hook(dir: &Path, name: &str, body: &str) {
            let hooks = dir.join(".git/hooks");
            fs::create_dir_all(&hooks).unwrap();
            let path = hooks.join(name);
            fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }

        fn committed_files(dir: &Path) -> Vec<String> {
            sh(dir, &["show", "--name-only", "--format=", "HEAD"])
                .lines()
                .map(str::to_owned)
                .collect()
        }

        fn staged(dir: &Path) -> String {
            sh(dir, &["diff", "--cached", "--name-only"])
        }

        #[test]
        fn commit_only_includes_the_chosen_paths() {
            let Some(tmp) = new_repo() else { return };
            let dir = tmp.path();
            fs::write(dir.join("a.txt"), "changed\n").unwrap();
            fs::write(dir.join("b.txt"), "staged by hand\n").unwrap();
            fs::write(dir.join("new.txt"), "new\n").unwrap();
            fs::write(dir.join("*"), "star\n").unwrap();
            sh(dir, &["add", "b.txt"]);
            let ctx = context_with(dir, &["a.txt".into(), "new.txt".into()], &env()).unwrap();
            assert_eq!(ctx.staged_elsewhere, vec!["b.txt".to_owned()]);

            let done = commit(dir, "Change a, add new", &["a.txt", "new.txt"]).unwrap();
            assert!(!done.short.is_empty());
            assert_eq!(committed_files(dir), vec!["a.txt", "new.txt"]);
            // What the user staged elsewhere stays staged, and uncommitted.
            assert_eq!(staged(dir), "b.txt");
            assert_eq!(sh(dir, &["show", "HEAD:b.txt"]), "two");
            // A literal pathspec: the file named `*` is not everything.
            fs::write(dir.join("a.txt"), "again\n").unwrap();
            commit(dir, "Star only", &["*"]).unwrap();
            assert_eq!(committed_files(dir), vec!["*"]);
            assert_eq!(staged(dir), "b.txt");
            assert_eq!(
                sh(dir, &["log", "-1", "--format=%s%n%an"]),
                "Star only\nTester"
            );
        }

        #[test]
        fn commit_creates_a_commit_with_hooks_running() {
            let Some(tmp) = new_repo() else { return };
            let dir = tmp.path();
            hook(
                dir,
                "pre-commit",
                "echo ran > \"$(git rev-parse --git-dir)/hook-ran\"",
            );
            fs::write(dir.join("a.txt"), "changed\n").unwrap();
            let before = sh(dir, &["rev-parse", "HEAD"]);
            commit(dir, "Subject\n\nBody line.", &["a.txt"]).unwrap();
            assert!(dir.join(".git/hook-ran").exists(), "the hook ran");
            assert_eq!(sh(dir, &["rev-parse", "HEAD~1"]), before);
            assert_eq!(
                sh(dir, &["log", "-1", "--format=%B"]),
                "Subject\n\nBody line."
            );
        }

        #[test]
        fn a_failing_hook_leaves_nothing_committed_and_files_staged() {
            let Some(tmp) = new_repo() else { return };
            let dir = tmp.path();
            hook(
                dir,
                "pre-commit",
                "echo 'lint failed: fix a.txt' >&2\nexit 1",
            );
            fs::write(dir.join("a.txt"), "changed\n").unwrap();
            let before = sh(dir, &["rev-parse", "HEAD"]);
            let err = commit(dir, "Will fail", &["a.txt"]).unwrap_err();
            assert!(err.contains("lint failed: fix a.txt"), "{err}");
            assert!(
                err.ends_with(STILL_STAGED),
                "the dialog says what is staged: {err}"
            );
            assert_eq!(sh(dir, &["rev-parse", "HEAD"]), before);
            assert_eq!(staged(dir), "a.txt");
        }

        #[test]
        fn commit_leaves_checkpoints_alone() {
            let Some(tmp) = new_repo() else { return };
            let dir = tmp.path();
            fs::write(dir.join("a.txt"), "turn\n").unwrap();
            let repo = crate::git::discover(dir).unwrap().unwrap();
            crate::git::take_checkpoint(&repo, 7, "Claude").unwrap();
            let refs = || {
                sh(
                    dir,
                    &[
                        "for-each-ref",
                        "--format=%(refname) %(objectname)",
                        "refs/agent-terminal/",
                    ],
                )
            };
            let before = refs();
            assert!(!before.is_empty());
            commit(dir, "Commit the turn", &["a.txt"]).unwrap();
            assert_eq!(refs(), before);
            let diff = crate::git::tab_diff(dir, 7, crate::diff::DiffBase::LastTurn).unwrap();
            assert!(
                matches!(diff, crate::git::DiffOutcome::Ready(_)),
                "{diff:?}"
            );
        }

        #[test]
        fn commit_runs_at_the_toplevel_from_a_subfolder() {
            let Some(tmp) = new_repo() else { return };
            let dir = tmp.path();
            fs::create_dir_all(dir.join("sub")).unwrap();
            fs::write(dir.join("sub/x.txt"), "x\n").unwrap();
            commit(&dir.join("sub"), "Add x", &["sub/x.txt"]).unwrap();
            assert_eq!(committed_files(dir), vec!["sub/x.txt"]);
        }

        #[test]
        fn commit_in_a_linked_worktree() {
            let Some(tmp) = new_repo() else { return };
            let dir = tmp.path();
            let parent = tempfile::tempdir().unwrap();
            let wt = parent.path().join("wt");
            sh(
                dir,
                &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
            );
            fs::write(wt.join("a.txt"), "in the worktree\n").unwrap();
            let ctx = context_with(&wt, &["a.txt".into()], &env()).unwrap();
            assert_eq!(ctx.branch.as_deref(), Some("feat"));
            assert_eq!(ctx.branch_warning(), None);
            commit(&wt, "From the worktree", &["a.txt"]).unwrap();
            assert_eq!(
                sh(dir, &["log", "-1", "--format=%s", "feat"]),
                "From the worktree"
            );
            assert_eq!(sh(dir, &["log", "-1", "--format=%s", "main"]), "init");
        }

        #[test]
        fn commit_with_unborn_head() {
            if !git_or_skip() {
                return;
            }
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path();
            sh(dir, &["init", "-q"]);
            identity(dir);
            fs::write(dir.join("first.txt"), "1\n").unwrap();
            fs::write(dir.join("later.txt"), "2\n").unwrap();
            let ctx = context_with(dir, &["first.txt".into()], &env()).unwrap();
            assert!(ctx.unborn);
            commit(dir, "First", &["first.txt"]).unwrap();
            assert_eq!(committed_files(dir), vec!["first.txt"]);
            assert!(sh(dir, &["status", "--porcelain"]).contains("?? later.txt"));
        }

        #[test]
        fn the_default_branch_and_a_detached_head_are_reported() {
            let Some(tmp) = new_repo() else { return };
            let dir = tmp.path();
            let ctx = context_with(dir, &[], &env()).unwrap();
            assert_eq!(ctx.branch.as_deref(), Some("main"));
            assert!(ctx.on_default_branch);
            assert!(ctx.branch_warning().is_some());
            sh(dir, &["checkout", "-q", "--detach"]);
            let ctx = context_with(dir, &[], &env()).unwrap();
            assert_eq!(ctx.branch, None);
            assert!(ctx.branch_warning().is_some_and(|w| w.contains("detached")));
            // Allowed with the warning: the commit is made.
            fs::write(dir.join("a.txt"), "detached\n").unwrap();
            commit(dir, "On a detached HEAD", &["a.txt"]).unwrap();
            assert_eq!(sh(dir, &["log", "-1", "--format=%s"]), "On a detached HEAD");
        }

        #[test]
        fn a_merge_in_progress_refuses_the_commit() {
            let Some(tmp) = new_repo() else { return };
            let dir = tmp.path();
            let head = sh(dir, &["rev-parse", "HEAD"]);
            fs::write(dir.join(".git/MERGE_HEAD"), format!("{head}\n")).unwrap();
            fs::write(dir.join("a.txt"), "x\n").unwrap();
            let err = commit(dir, "No", &["a.txt"]).unwrap_err();
            assert_eq!(err, InProgress::Merge.reason());
            assert_eq!(sh(dir, &["rev-parse", "HEAD"]), head);
            assert_eq!(staged(dir), "", "nothing was staged either");
        }

        #[test]
        fn missing_identity_is_reported_not_substituted() {
            if !git_or_skip() {
                return;
            }
            // An identity from the environment would be used, as git would; this test needs none.
            if [
                "GIT_AUTHOR_NAME",
                "GIT_AUTHOR_EMAIL",
                "GIT_COMMITTER_NAME",
                "GIT_COMMITTER_EMAIL",
                "EMAIL",
            ]
            .iter()
            .any(|v| std::env::var_os(v).is_some())
            {
                eprintln!("an identity is set in the environment; skipping");
                return;
            }
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path();
            init_repo_at(dir, &[("a.txt", b"one\n")]);
            sh(dir, &["config", "user.useConfigOnly", "true"]);
            fs::write(dir.join("a.txt"), "changed\n").unwrap();
            let before = sh(dir, &["rev-parse", "HEAD"]);
            let err = commit(dir, "Nobody", &["a.txt"]).unwrap_err();
            // git's own refusal: useConfigOnly forbids guessing an identity.
            assert!(err.contains("auto-detection is disabled"), "{err}");
            assert_eq!(sh(dir, &["rev-parse", "HEAD"]), before);
            let log = sh_output(dir, &["log", "--format=%ae"]);
            assert!(!String::from_utf8_lossy(&log.stdout).contains("agent-terminal@localhost"));
        }

        #[test]
        fn a_slow_hook_times_out_with_the_hook_explained() {
            let Some(tmp) = new_repo() else { return };
            let dir = tmp.path();
            // The hook and a helper it starts, each recording its pid.
            hook(
                dir,
                "pre-commit",
                "echo $$ > .git/hook.pid\nsleep 30 &\necho $! > .git/helper.pid\nwait",
            );
            fs::write(dir.join("a.txt"), "slow\n").unwrap();
            let err = commit_with(dir, &asked(dir, "Slow", &["a.txt"]), &env(), 1).unwrap_err();
            assert!(
                err.contains("stopped, with the hooks and helpers it started"),
                "{err}"
            );
            let pids: Vec<String> = ["hook.pid", "helper.pid"]
                .iter()
                .map(|f| fs::read_to_string(dir.join(".git").join(f)).unwrap())
                .collect();
            let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !pids.iter().all(|p| gone(p)) && std::time::Instant::now() < until {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            for pid in &pids {
                assert!(gone(pid), "process {} outlived the timeout", pid.trim());
            }
        }

        #[test]
        fn a_deletion_and_a_rename_are_committed_whole() {
            let Some(tmp) = new_repo() else { return };
            let dir = tmp.path();
            fs::remove_file(dir.join("a.txt")).unwrap();
            commit(dir, "Remove a", &["a.txt"]).unwrap();
            assert_eq!(committed_files(dir), vec!["a.txt"]);
            assert!(sh_output(dir, &["cat-file", "-e", "HEAD:a.txt"])
                .status
                .code()
                .is_some_and(|c| c != 0));

            // A rename as the panel lists it: the new path with the old one.
            fs::rename(dir.join("b.txt"), dir.join("c.txt")).unwrap();
            fs::write(dir.join("other.txt"), "left out\n").unwrap();
            commit(dir, "Rename b", &["c.txt", "b.txt"]).unwrap();
            assert_eq!(
                sh(dir, &["show", "-M", "--name-status", "--format=", "HEAD"]),
                "R100\tb.txt\tc.txt"
            );
            assert!(sh(dir, &["status", "--porcelain"]).contains("?? other.txt"));
        }

        #[test]
        fn a_file_replaced_by_a_folder_is_refused_before_anything_is_staged() {
            let Some(tmp) = new_repo() else { return };
            let dir = tmp.path();
            fs::remove_file(dir.join("a.txt")).unwrap();
            fs::create_dir(dir.join("a.txt")).unwrap();
            fs::write(dir.join("a.txt/.env"), "TOKEN=x\n").unwrap();
            fs::write(dir.join("a.txt/unticked.rs"), "x\n").unwrap();
            let head = sh(dir, &["rev-parse", "HEAD"]);
            let err = commit(dir, "Folder", &["a.txt"]).unwrap_err();
            assert!(
                err.contains("a.txt was a file and is now a folder"),
                "{err}"
            );
            assert_eq!(staged(dir), "", "nothing under the folder was staged");
            assert_eq!(sh(dir, &["rev-parse", "HEAD"]), head);

            // The same as a rename's old path.
            fs::write(dir.join("b2.txt"), "two\n").unwrap();
            fs::remove_file(dir.join("b.txt")).unwrap();
            fs::create_dir(dir.join("b.txt")).unwrap();
            fs::write(dir.join("b.txt/.env"), "TOKEN=y\n").unwrap();
            let err = commit(dir, "Rename", &["b2.txt", "b.txt"]).unwrap_err();
            assert!(
                err.contains("b.txt was a file and is now a folder"),
                "{err}"
            );
            assert_eq!(staged(dir), "");
        }

        #[test]
        fn a_moved_branch_or_head_refuses_the_commit() {
            let Some(tmp) = new_repo() else { return };
            let dir = tmp.path();
            sh(dir, &["checkout", "-q", "-b", "feat"]);
            fs::write(dir.join("a.txt"), "changed\n").unwrap();
            // The dialog was opened on feat; the user then switched to main.
            let req = asked(dir, "Moved", &["a.txt"]);
            sh(dir, &["checkout", "-q", "main"]);
            let err = commit_with(dir, &req, &env(), COMMIT_TIMEOUT_SECS).unwrap_err();
            assert!(err.contains("changed since the dialog opened"), "{err}");
            assert!(err.contains("now on main"), "{err}");
            assert_eq!(staged(dir), "", "nothing was staged");
            // Or a commit landed meanwhile on the same branch.
            let req = asked(dir, "Moved", &["a.txt"]);
            fs::write(dir.join("b.txt"), "elsewhere\n").unwrap();
            sh(dir, &["commit", "-q", "-am", "meanwhile"]);
            fs::write(dir.join("a.txt"), "changed again\n").unwrap();
            assert!(commit_with(dir, &req, &env(), COMMIT_TIMEOUT_SECS)
                .unwrap_err()
                .contains("changed since the dialog opened"));
        }

        #[test]
        fn a_first_commit_leaves_another_staged_file_staged() {
            if !git_or_skip() {
                return;
            }
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path();
            sh(dir, &["init", "-q"]);
            identity(dir);
            fs::write(dir.join("first.txt"), "1\n").unwrap();
            fs::write(dir.join("mine.txt"), "staged by hand\n").unwrap();
            sh(dir, &["add", "mine.txt"]);
            let ctx = context_with(dir, &["first.txt".into()], &env()).unwrap();
            assert!(ctx.unborn);
            assert_eq!(ctx.staged_elsewhere, vec!["mine.txt".to_owned()]);
            commit(dir, "First", &["first.txt"]).unwrap();
            assert_eq!(committed_files(dir), vec!["first.txt"]);
            assert_eq!(staged(dir), "mine.txt");
        }
    }
}

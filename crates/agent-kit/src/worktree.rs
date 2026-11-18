//! New Tab in Worktree: where a new worktree goes, whether a branch name will
//! do, and the git calls that create and remove one.
//!
//! Nothing here is ever forced: a worktree is removed only when it is clean,
//! with `git worktree remove` and no `--force`, and its branch is never
//! deleted. The git calls block; run them off the main thread.

use crate::git::{git_output, git_raw, QUERY_TIMEOUT_SECS};
use std::path::{Path, PathBuf};

/// A worktree a tab was opened in. Kept beside its thread in the store (as JSON), so a
/// reopened worktree thread still knows it is one.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: String,
    /// The main working tree, which owns the worktree.
    pub main_toplevel: PathBuf,
    /// The branch the worktree was started from, which "Merge into …" merges back into. `None`
    /// for a worktree started from a commit or a tag, or made before it was recorded: the main
    /// checkout's branch is used then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
}

/// A branch name as a directory name: `feat/x` becomes `feat-x`.
pub fn branch_dir_name(branch: &str) -> String {
    branch.trim().replace('/', "-")
}

/// Where a worktree for `branch` goes.
///
/// By default it is a hidden sibling of the main working tree,
/// `<parent>/.<repo>.worktrees/<branch>`. Hidden, because anything that
/// indexes every project directory beside the repository would otherwise
/// index the worktree as a second copy of it. `root`, when configured
/// (already `~`-expanded), replaces that with `<root>/<repo>/<branch>`.
pub fn default_path(main_toplevel: &Path, branch: &str, root: Option<&Path>) -> PathBuf {
    let repo = main_toplevel
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string());
    let branch = branch_dir_name(branch);
    match root {
        Some(root) => root.join(&repo).join(branch),
        None => main_toplevel
            .parent()
            .unwrap_or(main_toplevel)
            .join(format!(".{repo}.worktrees"))
            .join(branch),
    }
}

/// A problem with a name that can be told without asking git. A leading `-`
/// would reach git as an option.
pub fn precheck_name(name: &str, what: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(format!("Enter a {what}"));
    }
    if name.starts_with('-') {
        return Err(format!("A {what} cannot start with '-'"));
    }
    // A name is shown in dialogs and commands: one that can hide or reorder text there is no
    // name to show for what will run.
    if agent_core::text::has_control_or_bidi(name, &[]) {
        return Err(format!(
            "A {what} cannot hold control, direction or invisible characters"
        ));
    }
    Ok(())
}

/// The main working tree of the repository `dir` is in: the directory that
/// holds the common `.git`, whichever worktree `dir` itself is in.
pub fn main_toplevel(dir: &Path) -> Result<PathBuf, String> {
    let out = git_raw(
        dir,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        &[],
        QUERY_TIMEOUT_SECS,
    )?;
    let common = PathBuf::from(String::from_utf8_lossy(&out).trim());
    // Only an ordinary `<tree>/.git` says where the main tree is. A
    // submodule's (`.git/modules/x`), a separate or a bare git directory
    // does not, and guessing would put worktrees inside another `.git`.
    if common.file_name() != Some(std::ffi::OsStr::new(".git")) {
        return Err(format!(
            "This repository keeps its git directory at {}, which New Tab in \
             Worktree does not support (submodules and separate git directories)",
            common.display()
        ));
    }
    common
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("Unexpected git directory {}", common.display()))
}

/// The branch `dir` has checked out, if any.
pub fn current_branch(dir: &Path) -> Option<String> {
    let out = git_output(
        dir,
        ["symbolic-ref", "-q", "--short", "HEAD"],
        &[],
        QUERY_TIMEOUT_SECS,
    )
    .ok()?;
    let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !branch.is_empty()).then_some(branch)
}

/// What a folder's HEAD is, as [`head_branch`] reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadBranch {
    /// The branch checked out (an unborn one included).
    Branch(String),
    /// HEAD names a commit, not a branch.
    Detached,
    /// The folder is in no repository.
    NotRepository,
    /// git could not be asked, or gave no answer that says.
    Unknown,
}

/// The branch `dir` has checked out, or why there is none. Blocking (one `git symbolic-ref`,
/// through [`git_output`]: redirecting variables removed, no optional locks); call it off the
/// main thread.
pub fn head_branch(dir: &Path) -> HeadBranch {
    let Ok(out) = git_output(
        dir,
        ["symbolic-ref", "-q", "--short", "HEAD"],
        &[],
        QUERY_TIMEOUT_SECS,
    ) else {
        return HeadBranch::Unknown;
    };
    let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
    match out.status.code() {
        Some(0) if !branch.is_empty() => HeadBranch::Branch(branch),
        // `-q`: a HEAD that is not a symbolic ref exits 1, silently.
        Some(1) => HeadBranch::Detached,
        // Outside any repository git dies with 128 ("not a git repository").
        Some(128) => HeadBranch::NotRepository,
        _ => HeadBranch::Unknown,
    }
}

/// Whether `branch` is a valid, unused name for a new branch.
pub fn check_new_branch(repo: &Path, branch: &str) -> Result<(), String> {
    precheck_name(branch, "branch name")?;
    let branch = branch.trim();
    let valid = git_output(
        repo,
        ["check-ref-format", "--branch", branch],
        &[],
        QUERY_TIMEOUT_SECS,
    )?;
    if !valid.status.success() {
        return Err(format!("'{branch}' is not a valid branch name"));
    }
    let exists = git_output(
        repo,
        [
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
        &[],
        QUERY_TIMEOUT_SECS,
    )?;
    if exists.status.success() {
        return Err(format!("A branch named '{branch}' already exists"));
    }
    Ok(())
}

/// The commit `base` names, so the worktree starts from exactly what was
/// checked.
pub fn resolve_base(repo: &Path, base: &str) -> Result<String, String> {
    precheck_name(base, "base")?;
    let out = git_output(
        repo,
        [
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{}^{{commit}}", base.trim()),
        ],
        &[],
        QUERY_TIMEOUT_SECS,
    )?;
    let commit = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || commit.is_empty() {
        return Err(format!(
            "'{}' is not a commit in this repository",
            base.trim()
        ));
    }
    Ok(commit)
}

/// The local branch a worktree started from `start` (as typed: `HEAD`, a branch, a tag or a
/// commit) is based on: `start` itself when it names a local branch, the main checkout's branch
/// for `HEAD`, and `None` otherwise.
pub fn base_branch(repo: &Path, start: &str) -> Option<String> {
    let start = start.trim();
    if start == "HEAD" {
        return current_branch(repo);
    }
    if precheck_name(start, "base").is_err() {
        return None;
    }
    let out = git_output(
        repo,
        [
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{start}"),
        ],
        &[],
        QUERY_TIMEOUT_SECS,
    )
    .ok()?;
    out.status.success().then(|| start.to_owned())
}

/// Creates `branch` at `base` (a commit id from [`resolve_base`]) in a new
/// worktree at `path`, which must not exist yet.
pub fn add(main_toplevel: &Path, branch: &str, path: &Path, base: &str) -> Result<(), String> {
    if path.exists() {
        return Err(format!("{} already exists", path.display()));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Could not create {}: {err}", parent.display()))?;
    }
    let path_arg = path.as_os_str().to_os_string();
    let args: Vec<std::ffi::OsString> = ["worktree", "add", "-b", branch.trim(), "--"]
        .iter()
        .map(std::ffi::OsString::from)
        .chain([path_arg, base.into()])
        .collect();
    git_raw(main_toplevel, args, &[], WORKTREE_TIMEOUT_SECS)
        .map(|_| ())
        .map_err(with_prune_hint)
}

/// Creating or removing a worktree writes or deletes a whole tree and may run
/// a checkout hook, far beyond a query's budget. Killing git part-way would
/// leave a half-made worktree, so this is generous.
const WORKTREE_TIMEOUT_SECS: u64 = 120;

/// Adds what to do when git was stopped part-way, which can leave a
/// half-made worktree registered.
fn with_prune_hint(err: String) -> String {
    if err.contains("did not finish within") {
        format!(
            "{err}\n\nIt may have left a partial worktree behind. `git worktree list` \
             shows it, and `git worktree prune` clears a registration whose folder \
             is gone."
        )
    } else {
        err
    }
}

/// Whether the worktree at `path` holds nothing that removing it would lose:
/// nothing uncommitted, nothing untracked, and no ignored files either.
/// `git worktree remove` refuses the first two itself, but deletes ignored
/// files — a `node_modules`, a `.env`, build output — without asking, and a
/// fresh worktree has none, so any there were made in it. `Err` when git
/// cannot say, which is never taken as clean.
pub fn is_clean(path: &Path) -> Result<bool, String> {
    let status = git_raw(
        path,
        ["status", "--porcelain", "--untracked-files=normal"],
        &[],
        QUERY_TIMEOUT_SECS,
    )?;
    if !status.is_empty() {
        return Ok(false);
    }
    let ignored = git_raw(
        path,
        [
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
        ],
        &[],
        QUERY_TIMEOUT_SECS,
    )?;
    Ok(ignored.is_empty())
}

/// Whether `info` is still a worktree of its main checkout, on its branch: what a worktree read
/// back from the store must be before its removal is offered (it may have been removed, moved
/// or switched to another branch by hand since). `Err` when git cannot say, which is never
/// taken as registered.
pub fn is_registered(info: &WorktreeInfo) -> Result<bool, String> {
    let out = git_raw(
        &info.main_toplevel,
        ["worktree", "list", "--porcelain"],
        &[],
        QUERY_TIMEOUT_SECS,
    )?;
    let want = info
        .path
        .canonicalize()
        .unwrap_or_else(|_| info.path.clone());
    Ok(listed_worktree(
        &String::from_utf8_lossy(&out),
        &want,
        info.branch.trim(),
    ))
}

/// Whether `git worktree list --porcelain` output lists `path` with `branch` checked out.
fn listed_worktree(porcelain: &str, path: &Path, branch: &str) -> bool {
    let wanted_ref = format!("refs/heads/{branch}");
    porcelain.split("\n\n").any(|record| {
        let mut here = false;
        let mut on_branch = false;
        for line in record.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                let p = Path::new(p);
                here = p == path || p.canonicalize().is_ok_and(|c| c == path);
            } else if let Some(b) = line.strip_prefix("branch ") {
                on_branch = b == wanted_ref;
            }
        }
        here && on_branch
    })
}

/// Why removing the worktree `info` would not be offered, or `None` when it may be: it must still
/// be registered on its branch ([`is_registered`]) and hold nothing that removing it would lose
/// ([`is_clean`]). A question git cannot answer is a reason too, never taken as clean. Whether a
/// tab or another thread is in it is the caller's to ask.
pub fn removal_blocker(info: &WorktreeInfo) -> Option<String> {
    match is_registered(info) {
        Ok(true) => {}
        Ok(false) => return Some("it is no longer a worktree on its branch".to_owned()),
        Err(e) => return Some(format!("git could not say whether it is a worktree: {e}")),
    }
    match is_clean(&info.path) {
        Ok(true) => None,
        Ok(false) => Some(
            "it holds uncommitted, untracked or ignored files, which removing it would lose"
                .to_owned(),
        ),
        Err(e) => Some(format!("git could not say whether it is clean: {e}")),
    }
}

/// Where a fork in its own worktree goes ([`plan_fork`]), shown before it is made
/// ([`add_fork`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkPlan {
    /// The main working tree of the source's repository.
    pub main_toplevel: PathBuf,
    /// `fork/<slug>-<n>`, a branch that does not exist yet.
    pub branch: String,
    /// Where the worktree goes ([`default_path`]), not there yet.
    pub path: PathBuf,
    /// The branch the source has checked out, which "Merge into …" merges back into.
    pub base: Option<String>,
}

/// Most numbered names [`plan_fork`] tries before giving up.
const MAX_FORK_NAMES: u32 = 99;

/// Where a fork of the thread in `source` would go in a worktree of its own: the first
/// `fork/<slug>-<n>` (`slug` from [`agent_core::fork_compare::branch_slug`]) that names no branch
/// and whose folder (under `root`, else beside the repository) does not exist. Read-only.
pub fn plan_fork(source: &Path, slug: &str, root: Option<&Path>) -> Result<ForkPlan, String> {
    let main = main_toplevel(source)?;
    let slug = agent_core::fork_compare::branch_slug(slug);
    for n in 1..=MAX_FORK_NAMES {
        let branch = format!("fork/{slug}-{n}");
        let path = default_path(&main, &branch, root);
        if path.exists() || check_new_branch(&main, &branch).is_err() {
            continue;
        }
        return Ok(ForkPlan {
            base: current_branch(source),
            main_toplevel: main,
            branch,
            path,
        });
    }
    Err(format!(
        "Every branch name from fork/{slug}-1 to fork/{slug}-{MAX_FORK_NAMES} is taken"
    ))
}

/// A fork's worktree, made by [`add_fork`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkWorktree {
    pub info: WorktreeInfo,
    /// The snapshot of the source's folder the worktree's branch starts at: the fork's baseline.
    pub baseline: crate::git::ForkBaseline,
    /// The fork's folder: the worktree, or the same folder inside it the source was in.
    pub dir: PathBuf,
}

/// Makes the worktree `plan` names for a fork of the thread in `source`: a snapshot of the
/// source's working tree as a fork's baseline is taken ([`crate::git::take_fork_baseline`]:
/// tracked changes and untracked files, less secret-looking, oversized and nested ones; the
/// user's index, HEAD, working tree and stash untouched), then [`add`] puts `plan.branch` at that
/// commit in a new worktree, so the fork starts with the source's uncommitted work. The branch
/// is checked again first. Blocking.
pub fn add_fork(source: &Path, plan: &ForkPlan) -> Result<ForkWorktree, String> {
    check_new_branch(&plan.main_toplevel, &plan.branch)?;
    let baseline = match crate::git::take_fork_baseline(source)? {
        crate::git::BaselineOutcome::Taken(baseline) => baseline,
        crate::git::BaselineOutcome::Busy => {
            return Err(
                "Git is busy in this repository (its index is locked); try again in a \
                        moment"
                    .to_owned(),
            )
        }
        crate::git::BaselineOutcome::NotRepo => {
            return Err("The thread's folder is not in a git repository".to_owned())
        }
    };
    add(
        &plan.main_toplevel,
        &plan.branch,
        &plan.path,
        &baseline.commit,
    )?;
    // The fork works where the source did: the same folder inside the new worktree, when the
    // snapshot has it.
    let inner = source
        .canonicalize()
        .ok()
        .and_then(|s| {
            let top = baseline.toplevel.canonicalize().ok()?;
            s.strip_prefix(top).ok().map(Path::to_path_buf)
        })
        .filter(|rel| !rel.as_os_str().is_empty());
    let dir = match inner.map(|rel| plan.path.join(rel)) {
        Some(dir) if dir.is_dir() => dir,
        _ => plan.path.clone(),
    };
    Ok(ForkWorktree {
        info: WorktreeInfo {
            path: plan.path.clone(),
            branch: plan.branch.clone(),
            main_toplevel: plan.main_toplevel.clone(),
            base: plan.base.clone(),
        },
        baseline,
        dir,
    })
}

/// Removes the worktree at `path`. Never forced: git refuses a dirty one.
/// The branch is kept.
pub fn remove(main_toplevel: &Path, path: &Path) -> Result<(), String> {
    let args: Vec<std::ffi::OsString> = vec![
        "worktree".into(),
        "remove".into(),
        "--".into(),
        path.as_os_str().to_os_string(),
    ];
    git_raw(main_toplevel, args, &[], WORKTREE_TIMEOUT_SECS)
        .map(|_| ())
        .map_err(with_prune_hint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_slashes_become_dashes() {
        assert_eq!(branch_dir_name("feat/diff-panel"), "feat-diff-panel");
        assert_eq!(branch_dir_name(" fix "), "fix");
    }

    #[test]
    fn the_default_is_a_hidden_sibling_and_a_root_overrides_it() {
        let top = Path::new("/home/u/git/agent-terminal");
        assert_eq!(
            default_path(top, "feat/x", None),
            PathBuf::from("/home/u/git/.agent-terminal.worktrees/feat-x")
        );
        assert_eq!(
            default_path(top, "feat/x", Some(Path::new("/srv/wt"))),
            PathBuf::from("/srv/wt/agent-terminal/feat-x")
        );
    }

    #[test]
    fn names_that_would_read_as_options_are_refused() {
        assert!(precheck_name("-f", "branch name").is_err());
        assert!(precheck_name("  ", "branch name").is_err());
        assert!(precheck_name("feat/x", "branch name").is_ok());
        for hidden in ["feat\u{202E}x", "a\nb", "a\u{200B}b", "x\u{7}"] {
            assert!(precheck_name(hidden, "branch name").is_err(), "{hidden:?}");
        }
    }

    #[test]
    fn a_worktree_is_listed_only_at_its_path_and_on_its_branch() {
        let porcelain = "worktree /r/main\nHEAD abc\nbranch refs/heads/main\n\n\
                         worktree /r/.main.worktrees/feat-x\nHEAD def\nbranch refs/heads/feat/x\n\n\
                         worktree /r/detached\nHEAD 123\ndetached\n";
        let path = Path::new("/r/.main.worktrees/feat-x");
        assert!(listed_worktree(porcelain, path, "feat/x"));
        assert!(
            !listed_worktree(porcelain, path, "feat/y"),
            "another branch"
        );
        assert!(!listed_worktree(porcelain, Path::new("/r/gone"), "feat/x"));
        assert!(!listed_worktree(porcelain, Path::new("/r/detached"), "x"));
    }

    #[test]
    fn worktree_info_round_trips_as_json() {
        let info = WorktreeInfo {
            path: PathBuf::from("/r/.main.worktrees/feat-x"),
            branch: "feat/x".into(),
            main_toplevel: PathBuf::from("/r/main"),
            base: Some("main".into()),
        };
        let json = serde_json::to_string(&info).unwrap();
        assert_eq!(serde_json::from_str::<WorktreeInfo>(&json).unwrap(), info);
    }

    #[test]
    fn worktree_info_without_base_round_trips() {
        // As stored before the base was recorded.
        let json = r#"{"path":"/r/wt","branch":"feat/x","main_toplevel":"/r/main"}"#;
        let info: WorktreeInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.base, None);
        assert_eq!(
            serde_json::to_string(&info).unwrap(),
            json,
            "no base, no key"
        );
    }

    /// `<tmp>/work/main` with a commit and work in progress of every kind: a staged edit edited
    /// again, an unstaged edit, a deleted tracked file, an untracked file, an untracked secret, a
    /// subfolder, and a stash entry of the user's.
    fn repo_with_work() -> Option<(tempfile::TempDir, PathBuf)> {
        use crate::test_support::{git as sh, git_or_skip, init_repo_at};
        use std::fs;
        if !git_or_skip() {
            return None;
        }
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("work/main");
        fs::create_dir_all(&main).unwrap();
        init_repo_at(
            &main,
            &[
                ("a.txt", b"a\n"),
                ("b.txt", b"b\n"),
                ("gone.txt", b"gone\n"),
                ("sub/c.txt", b"c\n"),
                (".gitignore", b"build/\n"),
            ],
        );
        let main = main.canonicalize().unwrap();
        fs::write(main.join("b.txt"), "stashed\n").unwrap();
        sh(&main, &["stash", "push", "-q", "-m", "mine"]);
        fs::write(main.join("a.txt"), "staged\n").unwrap();
        sh(&main, &["add", "a.txt"]);
        fs::write(main.join("a.txt"), "staged then edited\n").unwrap();
        fs::write(main.join("sub/c.txt"), "unstaged\n").unwrap();
        fs::remove_file(main.join("gone.txt")).unwrap();
        fs::write(main.join("new.txt"), "untracked\n").unwrap();
        fs::write(main.join(".env"), "TOKEN=x\n").unwrap();
        fs::create_dir_all(main.join("build")).unwrap();
        fs::write(main.join("build/out.bin"), "ignored\n").unwrap();
        Some((tmp, main))
    }

    #[test]
    fn fork_worktree_starts_with_uncommitted_work() {
        use crate::test_support::git as sh;
        use std::fs;
        let Some((_tmp, main)) = repo_with_work() else {
            return;
        };
        let head = sh(&main, &["rev-parse", "HEAD"]);
        let source = main.join("sub");
        let plan = plan_fork(&source, "Try the --other way", None).unwrap();
        assert_eq!(plan.branch, "fork/try-the-other-way-1");
        assert_eq!(plan.path, default_path(&main, &plan.branch, None));
        assert_eq!(plan.base.as_deref(), Some("main"));
        assert!(!plan.path.exists(), "planning makes nothing");
        let fork = add_fork(&source, &plan).unwrap();
        let wt = &fork.info.path;
        assert_eq!(current_branch(wt).as_deref(), Some(plan.branch.as_str()));
        assert_eq!(fork.dir, wt.join("sub"), "the folder the source was in");
        assert_eq!(fork.info.base.as_deref(), Some("main"));
        assert_eq!(is_registered(&fork.info), Ok(true));
        // The source's uncommitted work, as its working tree had it.
        let read = |p: &str| fs::read_to_string(wt.join(p)).ok();
        assert_eq!(read("a.txt").as_deref(), Some("staged then edited\n"));
        assert_eq!(read("sub/c.txt").as_deref(), Some("unstaged\n"));
        assert_eq!(read("new.txt").as_deref(), Some("untracked\n"));
        assert_eq!(read("b.txt").as_deref(), Some("b\n"), "never the stash");
        assert_eq!(read("gone.txt"), None, "a deletion is carried too");
        // Never a secret-looking file, nor an ignored one.
        assert_eq!(read(".env"), None);
        assert!(fork.baseline.skipped.iter().any(|s| s.path == ".env"));
        assert!(!wt.join("build").exists());
        // The branch starts at the baseline, recorded on top of the source's HEAD.
        assert_eq!(sh(wt, &["rev-parse", "HEAD"]), fork.baseline.commit);
        assert_eq!(
            sh(wt, &["rev-parse", &format!("{}^", fork.baseline.commit)]),
            head
        );
        // The fork's baseline ref goes under the app's namespace only.
        let thread = "0f8fad5b-d9cb-469f-a165-70867728950e";
        crate::git::keep_fork_baseline(&fork.dir, thread, &fork.baseline.commit).unwrap();
        let refs = sh(&main, &["for-each-ref", "--format=%(refname)"]);
        let ours: Vec<&str> = refs
            .lines()
            .filter(|r| !r.starts_with("refs/heads/") && *r != "refs/stash")
            .collect();
        assert_eq!(ours, [format!("refs/agent-terminal/fork/{thread}")]);
        // The next plan for the same title takes the next number.
        let next = plan_fork(&main, "Try the --other way", None).unwrap();
        assert_eq!(next.branch, "fork/try-the-other-way-2");
        // A fresh fork's worktree is clean (its work is the branch's first commit), so removal
        // could be offered; once it is worked in, never.
        assert_eq!(removal_blocker(&fork.info), None);
        fs::write(wt.join("later.txt"), "fork's own\n").unwrap();
        assert!(removal_blocker(&fork.info).is_some_and(|why| why.contains("uncommitted")));
        // A plan made stale by a branch of that name since is refused before anything is made.
        sh(&main, &["branch", &next.branch]);
        assert!(add_fork(&main, &next).is_err());
        assert!(!next.path.exists());
    }

    #[test]
    fn fork_worktree_leaves_the_users_index_head_and_stash_alone() {
        use crate::test_support::git as sh;
        use std::fs;
        let Some((_tmp, main)) = repo_with_work() else {
            return;
        };
        let state = || {
            (
                fs::read(main.join(".git/index")).unwrap(),
                sh(&main, &["rev-parse", "HEAD"]),
                sh(&main, &["symbolic-ref", "HEAD"]),
                sh(&main, &["stash", "list"]),
                // (Not `git status`, which may refresh the index itself.)
                fs::read_to_string(main.join("a.txt")).unwrap(),
                fs::read_to_string(main.join("sub/c.txt")).unwrap(),
                main.join("gone.txt").exists(),
                main.join(".env").exists(),
            )
        };
        let before = state();
        let plan = plan_fork(&main, "fork", None).unwrap();
        let fork = add_fork(&main, &plan).unwrap();
        assert_eq!(
            state(),
            before,
            "forking into a worktree touched the user's state"
        );
        assert_eq!(sh(&main, &["stash", "list"]).lines().count(), 1);
        // Removing it, clean, keeps the branch and still leaves the user's state alone.
        assert_eq!(removal_blocker(&fork.info), None);
        remove(&main, &fork.info.path).unwrap();
        assert!(
            check_new_branch(&main, &plan.branch).is_err(),
            "the branch is kept"
        );
        assert_eq!(state(), before);
        // A source not in a repository has no plan.
        let plain = tempfile::tempdir().unwrap();
        assert!(plan_fork(plain.path(), "x", None).is_err());
    }

    mod repo {
        use super::super::*;
        use crate::test_support::{git as sh, git_or_skip, init_repo_at};
        use std::fs;

        /// `<tmp>/work/main` with one commit, so worktrees land in `<tmp>/work`.
        fn new_repo() -> Option<(tempfile::TempDir, PathBuf)> {
            if !git_or_skip() {
                return None;
            }
            let tmp = tempfile::tempdir().unwrap();
            let main = tmp.path().join("work/main");
            fs::create_dir_all(&main).unwrap();
            init_repo_at(&main, &[("a.txt", b"a\n"), (".gitignore", b"build/\n")]);
            Some((tmp, main))
        }

        #[test]
        fn head_branch_names_the_branch_or_says_why_there_is_none() {
            let Some((tmp, main)) = new_repo() else {
                return;
            };
            let branch = sh(&main, &["symbolic-ref", "--short", "HEAD"]);
            assert_eq!(head_branch(&main), HeadBranch::Branch(branch));
            // From a folder inside the repository too.
            fs::create_dir_all(main.join("sub")).unwrap();
            assert!(matches!(
                head_branch(&main.join("sub")),
                HeadBranch::Branch(_)
            ));
            sh(&main, &["checkout", "-q", "--detach"]);
            assert_eq!(head_branch(&main), HeadBranch::Detached);
            // A plain folder: no repository (the temp root is outside any).
            let plain = tmp.path().join("plain");
            fs::create_dir_all(&plain).unwrap();
            assert_eq!(head_branch(&plain), HeadBranch::NotRepository);
            // A folder that is not there cannot be asked.
            assert_eq!(head_branch(&tmp.path().join("gone")), HeadBranch::Unknown);
        }

        #[test]
        fn a_worktree_is_created_on_a_new_branch_and_only_removed_when_clean() {
            let Some((_tmp, main)) = new_repo() else {
                return;
            };
            // The main tree's canonical path, as git reports it.
            let main = main.canonicalize().unwrap();
            assert!(check_new_branch(&main, "feat/x").is_ok());
            assert!(check_new_branch(&main, "main").is_err(), "existing branch");
            assert!(check_new_branch(&main, "bad..name").is_err());
            assert!(check_new_branch(&main, "-b").is_err(), "reads as an option");
            assert!(resolve_base(&main, "nope").is_err());
            assert!(resolve_base(&main, "--all").is_err(), "reads as an option");

            let base = resolve_base(&main, "main").unwrap();
            let path = default_path(&main, "feat/x", None);
            add(&main, "feat/x", &path, &base).unwrap();
            assert_eq!(current_branch(&path).as_deref(), Some("feat/x"));
            let info = WorktreeInfo {
                path: path.clone(),
                branch: "feat/x".into(),
                main_toplevel: main.clone(),
                base: base_branch(&main, "main"),
            };
            assert_eq!(info.base.as_deref(), Some("main"));
            assert_eq!(base_branch(&main, "HEAD").as_deref(), Some("main"));
            assert_eq!(base_branch(&main, &base), None, "a commit is no branch");
            assert_eq!(base_branch(&main, "-main"), None);
            assert_eq!(is_registered(&info), Ok(true));
            let other = WorktreeInfo {
                branch: "feat/other".into(),
                ..info.clone()
            };
            assert_eq!(is_registered(&other), Ok(false), "not on that branch");
            // Seen from inside the worktree, the main tree is still found.
            assert_eq!(main_toplevel(&path).unwrap(), main);
            assert!(add(&main, "feat/y", &path, &base).is_err(), "path exists");

            fs::write(path.join("dirty.txt"), "x\n").unwrap();
            assert_eq!(is_clean(&path), Ok(false));
            assert!(remove(&main, &path).is_err(), "a dirty worktree is kept");
            assert!(path.exists());

            fs::remove_file(path.join("dirty.txt")).unwrap();
            // Ignored files are not dirty to git, but removing the worktree
            // would delete them, so they keep it.
            fs::create_dir_all(path.join("build")).unwrap();
            fs::write(path.join("build/out.bin"), "made here\n").unwrap();
            assert_eq!(is_clean(&path), Ok(false), "ignored files keep it");
            fs::remove_dir_all(path.join("build")).unwrap();

            assert_eq!(is_clean(&path), Ok(true));
            remove(&main, &path).unwrap();
            assert!(!path.exists());
            assert_eq!(is_registered(&info), Ok(false), "removed");
            // The branch survives its worktree.
            assert!(check_new_branch(&main, "feat/x").is_err());
        }

        #[test]
        fn a_separate_git_dir_is_refused_rather_than_guessed() {
            if !crate::git::git_installed() {
                return;
            }
            let tmp = tempfile::tempdir().unwrap();
            let tree = tmp.path().join("tree");
            let git_dir = tmp.path().join("elsewhere.git");
            fs::create_dir_all(&tree).unwrap();
            sh(
                &tree,
                &[
                    "init",
                    "-q",
                    "--separate-git-dir",
                    git_dir.to_str().unwrap(),
                ],
            );
            let err = main_toplevel(&tree).unwrap_err();
            assert!(err.contains("does not support"), "{err}");
        }

        #[test]
        fn a_timeout_explains_how_to_clear_a_partial_worktree() {
            assert!(with_prune_hint("git did not finish within 120s".into())
                .contains("git worktree prune"));
            assert_eq!(with_prune_hint("fatal: other".into()), "fatal: other");
        }
    }
}

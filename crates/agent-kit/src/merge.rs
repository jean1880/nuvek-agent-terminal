//! Merge back from a worktree thread: the worktree's branch merged into its base branch, in the
//! checkout that has the base checked out, after a preview the user confirms.
//!
//! With the diff panel's commit (`commit`), this is the only place the app writes the user's
//! branch, index or working tree, and it shares nothing with checkpoints. [`preflight`] only
//! reads: it records the base checkout, both tips, how far each side is ahead, a conflict
//! prediction (`git merge-tree --write-tree`, which writes objects only) and every reason the
//! merge cannot run. [`commands`] is the one list both the preview and [`merge`] use, so what is
//! shown is what runs.
//!
//! [`merge`] reads everything again first and refuses if either tip, the base branch or the
//! state of either checkout moved since the preview, then merges the exact commit the preview
//! read. It never forces, never skips a hook and never picks a side (`--force`, `--no-verify`,
//! `-X`, `-s` are never passed). Exit 0 is checked too: only a new commit whose parents are the
//! two shown counts as merged. A merge that stops part-way is undone with `git merge --abort`,
//! or, when git stopped before recording it (HEAD unmoved), `git reset --merge` to the base's
//! own commit; both are safe because the base checkout was proven clean. One that cannot be
//! undone (a left `index.lock` included) is said to be left as it is, with what to run. The
//! branch and the worktree are never deleted.
//!
//! Each step runs in a process group of its own, so a timeout ends the hooks git started too.
//!
//! Blocking: run everything here off the main thread.

use crate::commit::{in_progress_op, InProgress};
use crate::git::{
    discover, failure_message, git_output, git_output_in_group, git_raw, QUERY_TIMEOUT_SECS,
};
use crate::worktree::{is_registered, precheck_name, WorktreeInfo};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Timeout for the merge and for undoing it: the user's hooks run.
pub const MERGE_TIMEOUT_SECS: u64 = 120;

/// The hooks a merge commit can run, in the order git runs them.
pub const MERGE_HOOKS: [&str; 4] = [
    "pre-merge-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-merge",
];

/// What `git merge-tree --write-tree` says merging would do. Three states, never two: a
/// prediction git could not make is never "no conflicts".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prediction {
    Clean,
    /// The files that would conflict.
    Conflicts(Vec<String>),
    /// This git cannot predict (older than 2.38), or the prediction failed.
    Unknown,
}

/// Why the merge cannot run now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The worktree has uncommitted or untracked files: they would not be merged.
    WorktreeChanges,
    /// The base checkout has a detached HEAD.
    BaseDetached,
    /// The base checkout has another branch checked out.
    BaseElsewhere(String),
    /// The base checkout has uncommitted changes to tracked files.
    BaseChanges,
    /// The base checkout is in the middle of a merge, rebase, cherry-pick or revert.
    BaseInProgress(InProgress),
    /// The branch has no commit the base does not have.
    NothingToMerge,
    /// Merging would conflict in these files.
    Conflicts(Vec<String>),
}

impl Refusal {
    /// The refusal as a sentence, naming the branches and the base checkout.
    pub fn reason(&self, plan: &MergePlan) -> String {
        let (base, branch, dir) = (&plan.base, &plan.branch, plan.base_dir.display());
        match self {
            Refusal::WorktreeChanges => "The worktree has uncommitted or untracked files. Commit \
                 them first: only committed work is merged, and nothing is committed or stashed \
                 for you."
                .to_owned(),
            Refusal::BaseDetached => {
                format!("{dir} has a detached HEAD. Check out {base} there first.")
            }
            Refusal::BaseElsewhere(other) => {
                format!("{dir} has {other} checked out, not {base}. Check out {base} there first.")
            }
            Refusal::BaseChanges => format!(
                "{dir} has uncommitted changes. Commit or stash them there first: a merge must \
                 start from a clean checkout so that it can always be undone."
            ),
            Refusal::BaseInProgress(op) => format!("In {dir}: {}", op.reason()),
            Refusal::NothingToMerge => {
                format!("{base} already has every commit of {branch}: nothing to merge.")
            }
            Refusal::Conflicts(files) => format!(
                "Merging {branch} into {base} would conflict in {}. Bring {base} into {branch} \
                 in the worktree and resolve it there first.",
                file_list(files)
            ),
        }
    }
}

/// At most this many conflicting files are named; the rest are counted.
const LISTED_CONFLICTS: usize = 8;

fn file_list(files: &[String]) -> String {
    let mut named: Vec<&str> = files
        .iter()
        .take(LISTED_CONFLICTS)
        .map(String::as_str)
        .collect();
    let more = files.len().saturating_sub(LISTED_CONFLICTS);
    let more_words = format!("{more} more");
    if more > 0 {
        named.push(&more_words);
    }
    match named.as_slice() {
        [] => "some files".to_owned(),
        [one] => (*one).to_owned(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// What merging a worktree's branch back would do, read before anything runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergePlan {
    /// The worktree whose branch is merged.
    pub info: WorktreeInfo,
    /// The worktree's branch.
    pub branch: String,
    /// The branch it is merged into.
    pub base: String,
    /// Where the merge runs: the checkout that has `base` checked out.
    pub base_dir: PathBuf,
    /// The base's commit when the plan was read.
    pub base_tip: String,
    /// The branch's commit when the plan was read.
    pub branch_tip: String,
    /// Commits on the branch that the base does not have.
    pub ahead: u32,
    /// Commits on the base that the branch does not have.
    pub behind: u32,
    pub prediction: Prediction,
    /// The merge hooks that will run in the base checkout.
    pub hooks: Vec<String>,
    /// Every linked worktree of the repository (this one included), as git lists them: a folder
    /// inside one of them is not in the base checkout even when it is inside its folder.
    pub worktrees: Vec<PathBuf>,
    /// Every reason the merge cannot run; empty when it can.
    pub refusals: Vec<Refusal>,
}

/// The linked worktrees `git worktree list --porcelain` lists: every one after the first (the
/// main working tree).
pub fn linked_worktrees(porcelain: &str) -> Vec<PathBuf> {
    porcelain
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .skip(1)
        .map(PathBuf::from)
        .collect()
}

impl MergePlan {
    /// Whether nothing stops the merge.
    pub fn ready(&self) -> bool {
        self.refusals.is_empty()
    }
}

/// One git command, and the directory it runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitStep {
    pub dir: PathBuf,
    pub args: Vec<String>,
}

impl GitStep {
    /// The step as the preview shows it: the directory, then the command as a POSIX shell would
    /// read it back.
    pub fn shown(&self) -> String {
        format!(
            "In {}:\n{}",
            self.dir.display(),
            agent_core::commit_message::shown_command(&self.args)
        )
    }
}

/// The arguments of the merge of `tip`, the commit the preview read as `branch`'s, into `base`:
/// always a merge commit with the message git itself gives a branch merge (one `-m` value), and
/// the exact commit after `--`, so no name is ever read as an option, no tag of the same name and
/// no commit made on the branch since the preview is merged in its place.
///
/// Configuration that would make the merge something other than what the preview shows is
/// overridden for this one command: `branch.<base>.mergeOptions` (a `--no-commit`,
/// `--squash` or `--ff-only` there), `merge.ff` (already overridden by `--no-ff`) and
/// `merge.log` (which would append a log to the shown message). Hooks and signing are not
/// touched: they run as at any `git merge`.
pub fn merge_args(base: &str, branch: &str, tip: &str) -> Vec<String> {
    vec![
        "-c".to_owned(),
        format!("branch.{base}.mergeOptions="),
        "-c".to_owned(),
        "merge.ff=false".to_owned(),
        "-c".to_owned(),
        "merge.log=false".to_owned(),
        "merge".to_owned(),
        "--no-ff".to_owned(),
        "--no-edit".to_owned(),
        "-m".to_owned(),
        format!("Merge branch '{branch}'"),
        "--".to_owned(),
        tip.to_owned(),
    ]
}

/// The arguments of the undo of a merge that stopped part-way.
pub fn abort_args() -> Vec<String> {
    ["merge", "--abort"].map(str::to_owned).to_vec()
}

/// The commands the merge runs, in order: what the preview shows, and what [`merge`] runs.
pub fn commands(plan: &MergePlan) -> Vec<GitStep> {
    vec![GitStep {
        dir: plan.base_dir.clone(),
        args: merge_args(&plan.base, &plan.branch, &plan.branch_tip),
    }]
}

/// Reads what merging `info`'s branch into its base would do. Reads only. `Err` when there is
/// nothing to plan against: the names cannot be passed to git, the worktree is no longer on
/// its branch, or a branch is gone.
pub fn preflight(info: &WorktreeInfo) -> Result<MergePlan, String> {
    preflight_with(info, &[])
}

fn preflight_with(info: &WorktreeInfo, env: &[(&str, &OsStr)]) -> Result<MergePlan, String> {
    let branch = info.branch.trim().to_owned();
    precheck_name(&branch, "branch name")?;
    let main = info.main_toplevel.as_path();
    let base = match info.base.as_deref().map(str::trim) {
        Some(base) if !base.is_empty() => base.to_owned(),
        // A worktree made before its base was recorded: the main checkout's branch.
        _ => crate::worktree::current_branch(main).ok_or_else(|| {
            format!(
                "{} has a detached HEAD, so there is no branch to merge {branch} into",
                main.display()
            )
        })?,
    };
    precheck_name(&base, "base branch")?;
    if base == branch {
        return Err(format!("{branch} is the base branch itself"));
    }
    if !is_registered(info)? {
        return Err(format!(
            "{} is no longer a worktree on {branch}",
            info.path.display()
        ));
    }
    let branch_tip = tip(main, &branch, env)?;
    let base_tip = tip(main, &base, env)?;
    let mut refusals = Vec::new();

    let worktree_status = git_raw(
        &info.path,
        ["status", "--porcelain", "--untracked-files=normal"],
        env,
        QUERY_TIMEOUT_SECS,
    )?;
    if !worktree_status.is_empty() {
        refusals.push(Refusal::WorktreeChanges);
    }

    let base_dir = main.to_path_buf();
    let repo = discover(&base_dir)?
        .ok_or_else(|| format!("{} is not a git repository", base_dir.display()))?;
    match repo.branch.as_deref() {
        None => refusals.push(Refusal::BaseDetached),
        Some(on) if on != base => refusals.push(Refusal::BaseElsewhere(on.to_owned())),
        Some(_) => {}
    }
    if let Some(op) = in_progress_op(&repo.git_dir) {
        refusals.push(Refusal::BaseInProgress(op));
    }
    let base_status = git_raw(
        &base_dir,
        ["status", "--porcelain", "--untracked-files=no"],
        env,
        QUERY_TIMEOUT_SECS,
    )?;
    if !base_status.is_empty() {
        refusals.push(Refusal::BaseChanges);
    }

    let counts = git_raw(
        main,
        [
            "rev-list",
            "--left-right",
            "--count",
            &format!("{base_tip}...{branch_tip}"),
        ],
        env,
        QUERY_TIMEOUT_SECS,
    )?;
    let (behind, ahead) =
        read_counts(&counts).ok_or_else(|| "git rev-list gave an unexpected answer".to_owned())?;
    let prediction = if ahead == 0 {
        refusals.push(Refusal::NothingToMerge);
        Prediction::Clean
    } else {
        let prediction = predict(main, &base_tip, &branch_tip, env);
        if let Prediction::Conflicts(files) = &prediction {
            refusals.push(Refusal::Conflicts(files.clone()));
        }
        prediction
    };
    let hooks = hooks(&base_dir, env);
    let worktrees = git_raw(
        main,
        ["worktree", "list", "--porcelain"],
        env,
        QUERY_TIMEOUT_SECS,
    )
    .map(|out| linked_worktrees(&String::from_utf8_lossy(&out)))?;
    Ok(MergePlan {
        info: info.clone(),
        branch,
        base,
        base_dir,
        base_tip,
        branch_tip,
        ahead,
        behind,
        prediction,
        hooks,
        worktrees,
        refusals,
    })
}

/// The commit local branch `name` points at.
fn tip(repo: &Path, name: &str, env: &[(&str, &OsStr)]) -> Result<String, String> {
    let out = git_output(
        repo,
        [
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{name}^{{commit}}"),
        ],
        env,
        QUERY_TIMEOUT_SECS,
    )?;
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if out.status.success() && !sha.is_empty() {
        Ok(sha)
    } else {
        Err(format!("There is no branch named {name} any more"))
    }
}

/// `rev-list --left-right --count` output: the left count, then the right.
fn read_counts(out: &[u8]) -> Option<(u32, u32)> {
    let text = String::from_utf8_lossy(out);
    let mut parts = text.split_whitespace();
    let left = parts.next()?.parse().ok()?;
    let right = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((left, right))
}

fn predict(repo: &Path, base_tip: &str, branch_tip: &str, env: &[(&str, &OsStr)]) -> Prediction {
    match git_output(
        repo,
        [
            "merge-tree",
            "--write-tree",
            "--name-only",
            "--no-messages",
            "-z",
            base_tip,
            branch_tip,
        ],
        env,
        QUERY_TIMEOUT_SECS,
    ) {
        Ok(out) => read_prediction(out.status.code(), &out.stdout),
        Err(_) => Prediction::Unknown,
    }
}

/// Reads `git merge-tree --write-tree --name-only --no-messages -z`: exit 0 is a clean merge, 1
/// one with conflicts (the tree, then each conflicted path, NUL-terminated, then an empty
/// field). Anything else, an older git's usage error included, is [`Prediction::Unknown`].
pub fn read_prediction(code: Option<i32>, stdout: &[u8]) -> Prediction {
    match code {
        Some(0) => Prediction::Clean,
        Some(1) => {
            let mut files: Vec<String> = Vec::new();
            for field in stdout.split(|b| *b == 0).skip(1) {
                if field.is_empty() {
                    break;
                }
                let name = String::from_utf8_lossy(field).into_owned();
                if !files.contains(&name) {
                    files.push(name);
                }
            }
            if files.is_empty() {
                Prediction::Unknown
            } else {
                Prediction::Conflicts(files)
            }
        }
        _ => Prediction::Unknown,
    }
}

/// The merge hooks installed for `dir`'s repository (`core.hooksPath` honoured).
fn hooks(dir: &Path, env: &[(&str, &OsStr)]) -> Vec<String> {
    use std::os::unix::fs::PermissionsExt;
    let Ok(out) = git_raw(
        dir,
        ["rev-parse", "--git-path", "hooks"],
        env,
        QUERY_TIMEOUT_SECS,
    ) else {
        return Vec::new();
    };
    let path = PathBuf::from(String::from_utf8_lossy(&out).trim());
    let hooks = if path.is_absolute() {
        path
    } else {
        dir.join(path)
    };
    MERGE_HOOKS
        .iter()
        .filter(|name| {
            std::fs::metadata(hooks.join(name))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
        .map(|name| (*name).to_owned())
        .collect()
}

/// A merge that was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merged {
    /// The base's commit before the merge, abbreviated.
    pub before: String,
    /// The merge commit, abbreviated.
    pub after: String,
}

/// Merges `plan`'s branch into its base, after reading everything again: a plan with a refusal,
/// or one whose tips, base branch or checkouts moved since it was read, does nothing. Git's (and
/// a hook's) message on failure, masked, saying whether the merge was undone.
pub fn merge(plan: &MergePlan) -> Result<Merged, String> {
    merge_with(plan, &[], MERGE_TIMEOUT_SECS)
}

fn merge_with(
    plan: &MergePlan,
    env: &[(&str, &OsStr)],
    timeout_secs: u64,
) -> Result<Merged, String> {
    if let Some(refusal) = plan.refusals.first() {
        return Err(refusal.reason(plan));
    }
    let now = preflight_with(&plan.info, env).map_err(|why| {
        format!(
            "{}\n\nNothing was merged.",
            agent_core::redact::redact(&why)
        )
    })?;
    if now.base != plan.base
        || now.base_dir != plan.base_dir
        || now.base_tip != plan.base_tip
        || now.branch_tip != plan.branch_tip
    {
        return Err(agent_core::redact::redact(&format!(
            "{} or {} changed since the preview. Nothing was merged: open the merge again to \
             see what it would do now.",
            plan.base, plan.branch
        )));
    }
    if let Some(refusal) = now.refusals.first() {
        return Err(format!(
            "{}\n\nNothing was merged.",
            agent_core::redact::redact(&refusal.reason(&now))
        ));
    }
    execute(plan, env, timeout_secs)
}

/// Runs [`commands`], undoing the merge if it stops part-way.
fn execute(plan: &MergePlan, env: &[(&str, &OsStr)], timeout_secs: u64) -> Result<Merged, String> {
    for step in commands(plan) {
        let ran = git_output_in_group(&step.dir, &step.args, env, timeout_secs);
        let why = match ran {
            Ok(out) if out.status.success() => continue,
            Ok(out) => said(&out),
            Err(why) => why,
        };
        let why = agent_core::redact::redact(&why);
        let timed_out = why.contains(&format!("did not finish within {timeout_secs}s"));
        let mut message = why;
        if timed_out {
            message.push_str(
                "\n\nIt was stopped, with the hooks and helpers it started. Hooks and signing \
                 that ask for input (a passphrase prompt, for one) cannot be answered from here: \
                 merge in a terminal instead.",
            );
        }
        message.push_str("\n\n");
        message.push_str(&recover(plan, env, timeout_secs));
        return Err(message);
    }
    // Exit 0 is not proof of a merge commit: configuration or a hook can stop short of one
    // (`--no-commit` from somewhere, a merge left in progress) or make something else.
    if let Err(why) = check_merged(plan, env) {
        return Err(format!("{why}\n\n{}", recover(plan, env, timeout_secs)));
    }
    let after = git_raw(
        &plan.base_dir,
        ["rev-parse", "--short", "HEAD"],
        env,
        QUERY_TIMEOUT_SECS,
    )
    .map(|out| String::from_utf8_lossy(&out).trim().to_owned())
    .unwrap_or_default();
    Ok(Merged {
        before: short(&plan.base_tip),
        after,
    })
}

/// Whether the base checkout now holds exactly the merge the preview showed: nothing in
/// progress, still on the base, and HEAD a new commit whose parents are the base's old commit
/// and the merged one.
fn check_merged(plan: &MergePlan, env: &[(&str, &OsStr)]) -> Result<(), String> {
    let not = |what: &str| {
        Err(format!(
            "git reported success, but {what}, so this is not the merge the preview showed."
        ))
    };
    let Some(repo) = discover(&plan.base_dir).ok().flatten() else {
        return not("the base checkout could not be read back");
    };
    if in_progress_op(&repo.git_dir).is_some() {
        return not("a merge is still in progress, uncommitted");
    }
    if repo.branch.as_deref() != Some(plan.base.as_str()) {
        return not("the base checkout is no longer on its branch");
    }
    if repo.head.as_deref() == Some(plan.base_tip.as_str()) {
        return not("no merge commit was made");
    }
    let parents = git_raw(
        &plan.base_dir,
        ["rev-list", "--parents", "-n", "1", "HEAD"],
        env,
        QUERY_TIMEOUT_SECS,
    )
    .map(|out| {
        String::from_utf8_lossy(&out)
            .split_whitespace()
            .skip(1)
            .map(str::to_owned)
            .collect::<Vec<_>>()
    })
    .unwrap_or_default();
    if parents != [plan.base_tip.clone(), plan.branch_tip.clone()] {
        return not("the new commit is not a merge of the two commits shown");
    }
    Ok(())
}

/// Whether a git in `git_dir` left its index lock behind (killed mid-write, for one). With
/// `GIT_OPTIONAL_LOCKS=0`, `git status` succeeds regardless, so this is looked for itself.
fn index_locked(git_dir: &Path) -> bool {
    std::fs::symlink_metadata(git_dir.join("index.lock")).is_ok()
}

/// What a failed git said: a merge reports its conflicts on stdout, a hook its refusal on stderr.
fn said(out: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stdout = stdout.trim();
    let stderr = failure_message(&out.stderr);
    match (
        stdout.is_empty(),
        out.stderr.iter().all(u8::is_ascii_whitespace),
    ) {
        (true, _) => stderr,
        (false, true) => stdout.to_owned(),
        (false, false) => format!("{stdout}\n{stderr}"),
    }
}

fn short(sha: &str) -> String {
    sha.chars().take(12).collect()
}

/// The arguments of the undo of a merge git stopped before recording (no `MERGE_HEAD`, HEAD
/// not moved, the merged files left in the index and tree): back to the base's own commit,
/// keeping anything the merge did not touch. Used only when HEAD is still that commit, so no
/// branch moves.
pub fn unrecorded_undo_args(base_tip: &str) -> Vec<String> {
    vec![
        "reset".to_owned(),
        "--merge".to_owned(),
        base_tip.to_owned(),
    ]
}

/// How the base checkout stands after a failed merge.
struct Standing {
    /// HEAD is the base's old commit, on the base branch.
    at_base: bool,
    merging: bool,
    locked: bool,
    /// No tracked file differs from HEAD.
    clean: bool,
}

fn standing(plan: &MergePlan, env: &[(&str, &OsStr)]) -> Option<Standing> {
    let repo = discover(&plan.base_dir).ok().flatten()?;
    let clean = git_raw(
        &plan.base_dir,
        ["status", "--porcelain", "--untracked-files=no"],
        env,
        QUERY_TIMEOUT_SECS,
    )
    .ok()?
    .is_empty();
    Some(Standing {
        at_base: repo.head.as_deref() == Some(plan.base_tip.as_str())
            && repo.branch.as_deref() == Some(plan.base.as_str()),
        merging: in_progress_op(&repo.git_dir) == Some(InProgress::Merge),
        locked: index_locked(&repo.git_dir),
        clean,
    })
}

/// After a merge that failed: undoes it with `git merge --abort` when git left one in progress,
/// or, when git stopped before recording it with HEAD unmoved, with `git reset --merge` to the
/// base's own commit (safe because the checkout was proven clean before the merge). Then says
/// whether the base checkout is exactly as it was. Nothing else is ever run.
fn recover(plan: &MergePlan, env: &[(&str, &OsStr)], timeout_secs: u64) -> String {
    let dir = plan.base_dir.display();
    let base = &plan.base;
    let before = short(&plan.base_tip);
    let locked_words = format!(
        "A git was stopped while it held {dir}'s index: .git/index.lock is left behind, so the \
         checkout cannot be trusted to be as it was ({base} was at {before}). Once no git is \
         running there, remove .git/index.lock, then check `git status`; `git merge --abort` \
         undoes a merge in progress."
    );
    let Some(now) = standing(plan, env) else {
        return format!(
            "{dir} could not be read after the merge ({base} was at {before}). Check `git \
             status` there; `git merge --abort` undoes a merge in progress."
        );
    };
    // A lock left behind by a git that was stopped: an undo would only fail on it, and the
    // checkout cannot be read as settled until the user removes it.
    if now.locked {
        return locked_words;
    }
    let undo = if now.merging {
        Some(("git merge --abort".to_owned(), abort_args()))
    } else if now.at_base && !now.clean {
        let args = unrecorded_undo_args(&plan.base_tip);
        Some((agent_core::commit_message::shown_command(&args), args))
    } else {
        None
    };
    let mut undone = None;
    if let Some((shown, args)) = &undo {
        let ran = git_output_in_group(&plan.base_dir, args, env, timeout_secs);
        undone = Some(match ran {
            Ok(out) if out.status.success() => Ok(shown.clone()),
            Ok(out) => Err(failure_message(&out.stderr)),
            Err(why) => Err(why),
        });
    }
    let after = standing(plan, env);
    if after.as_ref().is_some_and(|s| s.locked) {
        return locked_words;
    }
    let restored = after.is_some_and(|s| s.at_base && !s.merging && s.clean);
    match (undone, restored) {
        (Some(Ok(shown)), true) => {
            format!("The merge was undone with `{shown}`: {base} is at {before} again, as it was.")
        }
        (None, true) => format!("Nothing was merged: {base} is still at {before}."),
        (Some(Err(why)), _) => format!(
            "The merge could not be undone ({}). It is left as git left it in {dir}: run `git \
             merge --abort` there for a merge in progress, or check `git status`.",
            agent_core::redact::redact(&why)
        ),
        (Some(Ok(_)), false) | (None, false) => format!(
            "{dir} is not as it was before the merge ({base} was at {before}). Check `git status` \
             there; `git merge --abort` undoes a merge in progress."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORBIDDEN: [&str; 12] = [
        "--force",
        "-f",
        "--no-verify",
        "-n",
        "-X",
        "--strategy-option",
        "-s",
        "--strategy",
        "--allow-unrelated-histories",
        "--squash",
        "--ff-only",
        "--no-gpg-sign",
    ];

    fn info(branch: &str, base: Option<&str>) -> WorktreeInfo {
        WorktreeInfo {
            path: PathBuf::from("/r/.main.worktrees/feat"),
            branch: branch.into(),
            main_toplevel: PathBuf::from("/r/main"),
            base: base.map(str::to_owned),
        }
    }

    fn plan(branch: &str) -> MergePlan {
        MergePlan {
            info: info(branch, Some("main")),
            branch: branch.into(),
            base: "main".into(),
            base_dir: PathBuf::from("/r/main"),
            base_tip: "a".repeat(40),
            branch_tip: "b".repeat(40),
            ahead: 1,
            behind: 0,
            prediction: Prediction::Clean,
            hooks: Vec::new(),
            worktrees: Vec::new(),
            refusals: Vec::new(),
        }
    }

    #[test]
    fn linked_worktrees_are_every_listed_one_but_the_main() {
        let porcelain = "worktree /r/main\nHEAD abc\nbranch refs/heads/main\n\n\
                         worktree /r/main/nested\nHEAD def\nbranch refs/heads/x\n\n\
                         worktree /r/.main.worktrees/feat\nHEAD 123\ndetached\n";
        assert_eq!(
            linked_worktrees(porcelain),
            vec![
                PathBuf::from("/r/main/nested"),
                PathBuf::from("/r/.main.worktrees/feat")
            ]
        );
    }

    #[test]
    fn the_merge_is_never_forced_and_the_branch_follows_dashdash() {
        let tip = "b".repeat(40);
        for branch in ["feat/x", "-f", "--no-verify"] {
            let args = merge_args("main", branch, &tip);
            let split = args.iter().position(|a| a == "--").unwrap();
            // Every argument before `--` is an option of ours, except the value after `-c`
            // (a setting) or `-m` (the message).
            let options: Vec<&str> = args[..split]
                .iter()
                .enumerate()
                .filter(|(i, _)| *i == 0 || !["-m", "-c"].contains(&args[i - 1].as_str()))
                .map(|(_, a)| a.as_str())
                .collect();
            for arg in &options {
                assert!(!FORBIDDEN.contains(arg), "{arg} in {args:?}");
            }
            assert_eq!(
                options,
                ["-c", "-c", "-c", "merge", "--no-ff", "--no-edit", "-m"]
            );
            assert_eq!(args[10], format!("Merge branch '{branch}'"));
            // The exact commit the preview read, never a name.
            assert_eq!(&args[split + 1..], std::slice::from_ref(&tip));
        }
        // The settings that would change what the preview shows are overridden, and only those.
        let args = merge_args("main", "feat/x", &tip);
        assert_eq!(
            [&args[1], &args[3], &args[5]],
            [
                "branch.main.mergeOptions=",
                "merge.ff=false",
                "merge.log=false"
            ]
        );
        assert_eq!(abort_args(), ["merge", "--abort"]);
        // The one reset: back to the base's own commit, keeping what the merge did not touch.
        assert_eq!(
            unrecorded_undo_args(&tip),
            ["reset", "--merge", tip.as_str()]
        );
    }

    #[test]
    fn the_preview_shows_what_runs_and_where() {
        let p = plan("feat/x");
        let steps = commands(&p);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].dir, p.base_dir);
        assert_eq!(steps[0].args, merge_args("main", "feat/x", &p.branch_tip));
        assert_eq!(
            steps[0].shown(),
            format!(
                "In /r/main:\ngit -c branch.main.mergeOptions= -c merge.ff=false -c \
                 merge.log=false merge --no-ff --no-edit -m 'Merge branch '\\''feat/x'\\''' -- {}",
                p.branch_tip
            )
        );
    }

    #[test]
    fn a_prediction_git_cannot_make_is_unknown_never_clean() {
        assert_eq!(read_prediction(Some(0), b"abc\0"), Prediction::Clean);
        assert_eq!(
            read_prediction(Some(1), b"abc\0a.txt\0b dir/c.txt\0a.txt\0\0"),
            Prediction::Conflicts(vec!["a.txt".into(), "b dir/c.txt".into()])
        );
        // An older git: `merge-tree --write-tree` is a usage error.
        assert_eq!(
            read_prediction(Some(129), b"usage: git merge-tree"),
            Prediction::Unknown
        );
        assert_eq!(read_prediction(None, b""), Prediction::Unknown);
        assert_eq!(read_prediction(Some(1), b"abc\0\0"), Prediction::Unknown);
    }

    #[test]
    fn counts_are_read_left_then_right() {
        assert_eq!(read_counts(b"2\t3\n"), Some((2, 3)));
        assert_eq!(read_counts(b"2\n"), None);
        assert_eq!(read_counts(b"x\t1\n"), None);
    }

    #[test]
    fn every_refusal_says_why_and_what_to_do() {
        let p = plan("feat/x");
        let reasons: Vec<String> = [
            Refusal::WorktreeChanges,
            Refusal::BaseDetached,
            Refusal::BaseElsewhere("other".into()),
            Refusal::BaseChanges,
            Refusal::BaseInProgress(InProgress::Rebase),
            Refusal::NothingToMerge,
            Refusal::Conflicts((0..10).map(|i| format!("f{i}")).collect()),
        ]
        .iter()
        .map(|r| {
            // Exhaustive: a new refusal fails to compile until it is worded.
            match r {
                Refusal::WorktreeChanges
                | Refusal::BaseDetached
                | Refusal::BaseElsewhere(_)
                | Refusal::BaseChanges
                | Refusal::BaseInProgress(_)
                | Refusal::NothingToMerge
                | Refusal::Conflicts(_) => {}
            }
            r.reason(&p)
        })
        .collect();
        assert!(reasons[0].contains("Commit them first"));
        assert!(reasons[2].contains("has other checked out, not main"));
        assert!(reasons[4].contains("A rebase is in progress"));
        assert!(reasons[6].contains("f0, f1, f2, f3, f4, f5, f6, f7 and 2 more"));
        assert_eq!(file_list(&["a".into()]), "a");
        assert_eq!(file_list(&["a".into(), "b".into()]), "a and b");
    }

    #[test]
    fn a_plan_with_a_refusal_runs_nothing() {
        let mut p = plan("feat/x");
        p.refusals.push(Refusal::BaseChanges);
        assert!(!p.ready());
        let err = merge_with(&p, &[], 1).unwrap_err();
        assert_eq!(err, Refusal::BaseChanges.reason(&p));
    }

    /// Against real repositories, with a hermetic config. Skipped when git is not installed.
    mod repo {
        use super::*;
        use crate::test_support::{git as sh, git_or_skip, git_output as sh_output, init_repo_at};
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        const ENV: [(&str, &str); 2] = [
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
        ];

        fn env() -> Vec<(&'static str, &'static OsStr)> {
            ENV.iter().map(|(k, v)| (*k, OsStr::new(v))).collect()
        }

        /// `<tmp>/main` on `main`, and a worktree on `feat` with one commit of its own.
        struct Fixture {
            tmp: tempfile::TempDir,
            main: PathBuf,
            wt: PathBuf,
            info: WorktreeInfo,
        }

        fn fixture() -> Option<Fixture> {
            if !git_or_skip() {
                return None;
            }
            let tmp = tempfile::tempdir().unwrap();
            let main = tmp.path().join("main");
            fs::create_dir_all(&main).unwrap();
            init_repo_at(&main, &[("a.txt", b"one\ntwo\nthree\n"), ("b.txt", b"b\n")]);
            let main = main.canonicalize().unwrap();
            sh(&main, &["config", "user.name", "Tester"]);
            sh(&main, &["config", "user.email", "tester@example.com"]);
            sh(&main, &["config", "commit.gpgsign", "false"]);
            let wt = tmp.path().join("wt");
            sh(
                &main,
                &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
            );
            let wt = wt.canonicalize().unwrap();
            fs::write(wt.join("new.txt"), "from the worktree\n").unwrap();
            sh(&wt, &["add", "new.txt"]);
            sh(&wt, &["commit", "-q", "-m", "work in the worktree"]);
            let info = WorktreeInfo {
                path: wt.clone(),
                branch: "feat".into(),
                main_toplevel: main.clone(),
                base: Some("main".into()),
            };
            Some(Fixture {
                tmp,
                main,
                wt,
                info,
            })
        }

        fn head(dir: &Path) -> String {
            sh(dir, &["rev-parse", "HEAD"])
        }

        fn plan_of(f: &Fixture) -> MergePlan {
            preflight_with(&f.info, &env()).unwrap()
        }

        fn run(p: &MergePlan) -> Result<Merged, String> {
            merge_with(p, &env(), MERGE_TIMEOUT_SECS)
        }

        fn hook(main: &Path, name: &str, body: &str) {
            let hooks = main.join(".git/hooks");
            fs::create_dir_all(&hooks).unwrap();
            let path = hooks.join(name);
            fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }

        /// HEAD, the index and the tracked tree of `dir`, to compare before and after.
        fn state(dir: &Path) -> (String, String, String) {
            (
                head(dir),
                sh(dir, &["ls-files", "-s"]),
                sh(dir, &["status", "--porcelain", "--untracked-files=all"]),
            )
        }

        fn branch_exists(dir: &Path, name: &str) -> bool {
            sh_output(
                dir,
                &[
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{name}"),
                ],
            )
            .status
            .success()
        }

        fn conflict_on_base(f: &Fixture) {
            fs::write(f.wt.join("a.txt"), "one\nTWO from feat\nthree\n").unwrap();
            sh(&f.wt, &["commit", "-q", "-am", "feat edits a"]);
            fs::write(f.main.join("a.txt"), "one\nTWO from main\nthree\n").unwrap();
            sh(&f.main, &["commit", "-q", "-am", "main edits a"]);
        }

        #[test]
        fn a_clean_branch_is_merged_with_a_merge_commit_and_the_branch_is_kept() {
            let Some(f) = fixture() else { return };
            let before = head(&f.main);
            let p = plan_of(&f);
            assert!(p.ready(), "{:?}", p.refusals);
            assert_eq!((p.ahead, p.behind), (1, 0));
            assert_eq!(p.prediction, Prediction::Clean);
            assert_eq!(p.base_dir, f.main);
            assert_eq!(p.worktrees, vec![f.wt.clone()]);
            let done = run(&p).unwrap();
            assert!(before.starts_with(&done.before));
            // A merge commit even where a fast-forward was possible.
            assert_eq!(sh(&f.main, &["rev-parse", "HEAD^1"]), before);
            assert_eq!(sh(&f.main, &["rev-parse", "HEAD^2"]), p.branch_tip);
            assert_eq!(
                sh(&f.main, &["log", "-1", "--format=%s"]),
                "Merge branch 'feat'"
            );
            assert!(f.main.join("new.txt").exists());
            // Never deleted: the branch, and its worktree.
            assert!(branch_exists(&f.main, "feat"));
            assert!(f.wt.exists());
            assert_eq!(crate::worktree::is_registered(&f.info), Ok(true));
            // And the removal mandate still holds: a dirty worktree is refused.
            fs::write(f.wt.join("dirty.txt"), "x\n").unwrap();
            assert!(crate::worktree::remove(&f.main, &f.wt).is_err());
            assert!(f.wt.exists());
        }

        #[test]
        fn a_tag_named_like_the_branch_is_never_merged_in_its_place() {
            let Some(f) = fixture() else { return };
            // A tag `feat` on the base's commit: by name alone git would take the tag.
            sh(&f.main, &["tag", "feat", "main"]);
            let p = plan_of(&f);
            run(&p).unwrap();
            assert_eq!(sh(&f.main, &["rev-parse", "HEAD^2"]), p.branch_tip);
            assert!(f.main.join("new.txt").exists());
        }

        #[test]
        fn a_merge_after_the_base_moved_on_is_a_true_merge() {
            let Some(f) = fixture() else { return };
            fs::write(f.main.join("b.txt"), "changed on main\n").unwrap();
            sh(&f.main, &["commit", "-q", "-am", "main moves"]);
            let p = plan_of(&f);
            assert_eq!((p.ahead, p.behind), (1, 1));
            assert!(p.ready());
            run(&p).unwrap();
            assert_eq!(
                fs::read_to_string(f.main.join("b.txt")).unwrap(),
                "changed on main\n"
            );
            assert!(f.main.join("new.txt").exists());
        }

        #[test]
        fn a_worktree_without_a_recorded_base_merges_into_the_main_checkouts_branch() {
            let Some(f) = fixture() else { return };
            let info = WorktreeInfo {
                base: None,
                ..f.info.clone()
            };
            let p = preflight_with(&info, &env()).unwrap();
            assert_eq!(p.base, "main");
            assert!(p.ready());
        }

        #[test]
        fn refuses_a_dirty_worktree_and_a_dirty_base_and_changes_nothing() {
            let Some(f) = fixture() else { return };
            fs::write(f.wt.join("untracked.txt"), "x\n").unwrap();
            let p = plan_of(&f);
            assert_eq!(p.refusals, vec![Refusal::WorktreeChanges]);
            fs::remove_file(f.wt.join("untracked.txt")).unwrap();

            fs::write(f.main.join("b.txt"), "uncommitted\n").unwrap();
            let before = state(&f.main);
            let p = plan_of(&f);
            assert_eq!(p.refusals, vec![Refusal::BaseChanges]);
            assert!(run(&p).is_err());
            assert_eq!(state(&f.main), before);
            // An untracked file in the base checkout is not in the way of the plan.
            sh(&f.main, &["checkout", "-q", "--", "b.txt"]);
            fs::write(f.main.join("mine.txt"), "x\n").unwrap();
            assert!(plan_of(&f).ready());
        }

        #[test]
        fn detached_or_mid_merge_base_is_refused() {
            let Some(f) = fixture() else { return };
            sh(&f.main, &["checkout", "-q", "--detach"]);
            assert_eq!(plan_of(&f).refusals, vec![Refusal::BaseDetached]);
            sh(&f.main, &["checkout", "-q", "-b", "other"]);
            assert_eq!(
                plan_of(&f).refusals,
                vec![Refusal::BaseElsewhere("other".into())]
            );
            sh(&f.main, &["checkout", "-q", "main"]);
            let tip_now = head(&f.main);
            fs::write(f.main.join(".git/MERGE_HEAD"), format!("{tip_now}\n")).unwrap();
            let p = plan_of(&f);
            assert_eq!(p.refusals, vec![Refusal::BaseInProgress(InProgress::Merge)]);
            assert!(run(&p).is_err());
            assert_eq!(head(&f.main), tip_now);
        }

        #[test]
        fn refuses_when_base_or_branch_moved_since_preview() {
            let Some(f) = fixture() else { return };
            let p = plan_of(&f);
            fs::write(f.main.join("b.txt"), "meanwhile\n").unwrap();
            sh(&f.main, &["commit", "-q", "-am", "meanwhile on main"]);
            let before = state(&f.main);
            let err = run(&p).unwrap_err();
            assert!(err.contains("changed since the preview"), "{err}");
            assert_eq!(state(&f.main), before);

            let p = plan_of(&f);
            fs::write(f.wt.join("more.txt"), "more\n").unwrap();
            sh(&f.wt, &["add", "more.txt"]);
            sh(&f.wt, &["commit", "-q", "-m", "more"]);
            assert!(run(&p).unwrap_err().contains("changed since the preview"));
            assert_eq!(state(&f.main), before);

            // The main checkout switched branch, on a worktree with no recorded base.
            let info = WorktreeInfo {
                base: None,
                ..f.info.clone()
            };
            let p = preflight_with(&info, &env()).unwrap();
            sh(&f.main, &["checkout", "-q", "-b", "elsewhere"]);
            assert!(merge_with(&p, &env(), MERGE_TIMEOUT_SECS)
                .unwrap_err()
                .contains("changed since the preview"));
        }

        #[test]
        fn predicts_a_conflict_without_touching_anything_and_refuses_it() {
            let Some(f) = fixture() else { return };
            conflict_on_base(&f);
            let before = (state(&f.main), state(&f.wt));
            let p = plan_of(&f);
            assert_eq!(p.prediction, Prediction::Conflicts(vec!["a.txt".into()]));
            assert_eq!(p.refusals, vec![Refusal::Conflicts(vec!["a.txt".into()])]);
            assert_eq!((state(&f.main), state(&f.wt)), before);
            assert!(run(&p).is_err());
            assert_eq!((state(&f.main), state(&f.wt)), before);
        }

        #[test]
        fn a_conflicted_merge_is_aborted_to_the_exact_prior_state() {
            let Some(f) = fixture() else { return };
            conflict_on_base(&f);
            fs::write(f.main.join("mine.txt"), "untracked, kept\n").unwrap();
            let before = state(&f.main);
            // As if the prediction had missed it: run the steps themselves.
            let mut p = plan_of(&f);
            p.refusals.clear();
            let err = execute(&p, &env(), MERGE_TIMEOUT_SECS).unwrap_err();
            assert!(err.contains("CONFLICT"), "{err}");
            assert!(err.contains("undone with `git merge --abort`"), "{err}");
            assert_eq!(state(&f.main), before);
            assert_eq!(in_progress_op(&f.main.join(".git")), None);
            assert_eq!(
                fs::read_to_string(f.main.join("mine.txt")).unwrap(),
                "untracked, kept\n"
            );
        }

        #[test]
        fn hooks_run_and_a_failing_one_leaves_the_base_as_it_was() {
            let Some(f) = fixture() else { return };
            hook(
                &f.main,
                "post-merge",
                "echo ran > \"$(git rev-parse --git-dir)/post-merge-ran\"",
            );
            let p = plan_of(&f);
            assert_eq!(p.hooks, vec!["post-merge".to_owned()]);
            run(&p).unwrap();
            assert!(f.main.join(".git/post-merge-ran").exists(), "the hook ran");

            // Another commit to merge, and a hook that refuses it.
            fs::write(f.wt.join("next.txt"), "next\n").unwrap();
            sh(&f.wt, &["add", "next.txt"]);
            sh(&f.wt, &["commit", "-q", "-m", "next"]);
            hook(
                &f.main,
                "pre-merge-commit",
                "echo 'checks failed: fix next.txt' >&2\nexit 1",
            );
            let before = state(&f.main);
            let p = plan_of(&f);
            assert!(p.hooks.contains(&"pre-merge-commit".to_owned()));
            let err = run(&p).unwrap_err();
            assert!(err.contains("checks failed: fix next.txt"), "{err}");
            assert_eq!(state(&f.main), before, "{err}");
            assert_eq!(in_progress_op(&f.main.join(".git")), None, "{err}");
        }

        /// Whether process `pid` is gone (or only a zombie waiting to be reaped).
        fn gone(pid: &str) -> bool {
            match fs::read_to_string(format!("/proc/{}/stat", pid.trim())) {
                Err(_) => true,
                Ok(stat) => stat
                    .rsplit_once(')')
                    .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
            }
        }

        #[test]
        fn a_slow_hook_times_out_and_its_group_is_killed() {
            let Some(f) = fixture() else { return };
            let marks = f.tmp.path().join("marks");
            fs::create_dir_all(&marks).unwrap();
            hook(
                &f.main,
                "pre-merge-commit",
                &format!(
                    "echo $$ > '{m}/hook.pid'\nsleep 30 &\necho $! > '{m}/helper.pid'\nwait",
                    m = marks.display()
                ),
            );
            let before = state(&f.main);
            let p = plan_of(&f);
            let err = merge_with(&p, &env(), 1).unwrap_err();
            assert!(
                err.contains("stopped, with the hooks and helpers it started"),
                "{err}"
            );
            assert!(err.contains("cannot be answered from here"), "{err}");
            let pids: Vec<String> = ["hook.pid", "helper.pid"]
                .iter()
                .map(|n| fs::read_to_string(marks.join(n)).unwrap())
                .collect();
            let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !pids.iter().all(|p| gone(p)) && std::time::Instant::now() < until {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            for pid in &pids {
                assert!(gone(pid), "process {} outlived the timeout", pid.trim());
            }
            // The hook runs before git records the merge, so the merged files sit in the index
            // and tree with HEAD unmoved: undone back to the base's commit, exactly.
            assert!(
                err.contains(&format!("undone with `git reset --merge {}`", p.base_tip)),
                "{err}"
            );
            assert_eq!(state(&f.main), before, "{err}");
            assert_eq!(in_progress_op(&f.main.join(".git")), None);
        }

        #[test]
        fn configured_merge_options_never_change_what_runs() {
            let Some(f) = fixture() else { return };
            // Each of these would make `git merge` stop short of the merge commit shown, or
            // change its message.
            sh(
                &f.main,
                &["config", "branch.main.mergeOptions", "--no-commit"],
            );
            sh(&f.main, &["config", "merge.ff", "only"]);
            sh(&f.main, &["config", "merge.log", "true"]);
            let before = head(&f.main);
            let p = plan_of(&f);
            run(&p).unwrap();
            assert_eq!(in_progress_op(&f.main.join(".git")), None);
            assert_eq!(sh(&f.main, &["rev-parse", "HEAD^1"]), before);
            assert_eq!(sh(&f.main, &["rev-parse", "HEAD^2"]), p.branch_tip);
            assert_eq!(
                sh(&f.main, &["log", "-1", "--format=%B"]),
                "Merge branch 'feat'",
                "the message shown, with no log appended"
            );
        }

        #[test]
        fn a_merge_that_reports_success_without_committing_is_undone_and_reported() {
            let Some(f) = fixture() else { return };
            let before = state(&f.main);
            let p = plan_of(&f);
            // What a git that exits 0 short of the commit leaves behind.
            sh(
                &f.main,
                &["merge", "-q", "--no-ff", "--no-commit", &p.branch_tip],
            );
            assert_eq!(
                in_progress_op(&f.main.join(".git")),
                Some(InProgress::Merge)
            );
            let why = check_merged(&p, &env()).unwrap_err();
            assert!(why.contains("still in progress"), "{why}");
            let said = recover(&p, &env(), MERGE_TIMEOUT_SECS);
            assert!(said.contains("undone with `git merge --abort`"), "{said}");
            assert_eq!(state(&f.main), before);
            // And a commit that is not the merge shown is never taken for it.
            fs::write(f.main.join("b.txt"), "other\n").unwrap();
            sh(&f.main, &["commit", "-q", "-am", "not a merge"]);
            assert!(check_merged(&p, &env())
                .unwrap_err()
                .contains("not a merge of the two commits shown"));
        }

        #[test]
        fn a_left_index_lock_is_never_taken_for_a_restored_checkout() {
            let Some(f) = fixture() else { return };
            let p = plan_of(&f);
            fs::write(f.main.join(".git/index.lock"), b"").unwrap();
            let said = recover(&p, &env(), MERGE_TIMEOUT_SECS);
            assert!(said.contains(".git/index.lock is left behind"), "{said}");
            assert!(!said.contains("Nothing was merged"), "{said}");
        }

        #[test]
        fn the_commit_shown_is_the_one_merged_even_if_the_branch_moves() {
            let Some(f) = fixture() else { return };
            let p = plan_of(&f);
            // A commit lands on the branch after the preview, past the re-check: the merge
            // still takes only the commit that was shown.
            fs::write(f.wt.join("late.txt"), "late\n").unwrap();
            sh(&f.wt, &["add", "late.txt"]);
            sh(&f.wt, &["commit", "-q", "-m", "late"]);
            execute(&p, &env(), MERGE_TIMEOUT_SECS).unwrap();
            assert_eq!(sh(&f.main, &["rev-parse", "HEAD^2"]), p.branch_tip);
            assert!(!f.main.join("late.txt").exists());
        }

        #[test]
        fn names_starting_with_dash_are_refused_before_git() {
            let Some(f) = fixture() else { return };
            let dashed = WorktreeInfo {
                branch: "-feat".into(),
                ..f.info.clone()
            };
            assert!(preflight_with(&dashed, &env())
                .unwrap_err()
                .contains("cannot start with '-'"));
            let dashed_base = WorktreeInfo {
                base: Some("--all".into()),
                ..f.info.clone()
            };
            assert!(preflight_with(&dashed_base, &env())
                .unwrap_err()
                .contains("cannot start with '-'"));
            let same = WorktreeInfo {
                base: Some("feat".into()),
                ..f.info.clone()
            };
            assert!(preflight_with(&same, &env()).is_err());
        }

        #[test]
        fn nothing_to_merge_is_refused() {
            let Some(f) = fixture() else { return };
            run(&plan_of(&f)).unwrap();
            assert_eq!(plan_of(&f).refusals, vec![Refusal::NothingToMerge]);
        }

        #[test]
        fn the_merge_leaves_checkpoints_alone() {
            let Some(f) = fixture() else { return };
            fs::write(f.main.join("mine.txt"), "untracked\n").unwrap();
            let repo = crate::git::discover(&f.main).unwrap().unwrap();
            crate::git::take_checkpoint(&repo, 9, "Claude").unwrap();
            let refs = || {
                sh(
                    &f.main,
                    &[
                        "for-each-ref",
                        "--format=%(refname) %(objectname)",
                        "refs/agent-terminal/",
                    ],
                )
            };
            let before = refs();
            assert!(!before.is_empty());
            run(&plan_of(&f)).unwrap();
            assert_eq!(refs(), before);
        }
    }
}

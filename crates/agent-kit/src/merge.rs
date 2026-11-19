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
//! two shown counts as merged. A merge git recorded as in progress (`MERGE_HEAD`) that stops
//! part-way is undone with `git merge --abort`, and with nothing else: in every other failure the
//! checkout is left exactly as git left it (nothing can tell what git wrote from changes the user
//! made meanwhile), and the message says what state it is in and what the user can run. The
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

/// Hooks that run during a squash merge and commit in the base checkout.
pub const SQUASH_HOOKS: [&str; 5] = [
    "pre-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-commit",
    "post-merge",
];

/// Hooks that run in the worktree during `git rebase`.
pub const REBASE_WORKTREE_HOOKS: [&str; 7] = [
    "pre-rebase",
    "post-checkout",
    "pre-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-commit",
    "post-rewrite",
];

/// Hooks that run in the base checkout during `git merge --ff-only`.
pub const REBASE_BASE_HOOKS: [&str; 1] = ["post-merge"];

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

/// How a worktree's branch is brought back into its base checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeMethod {
    /// Merge commit preserving branch history (`git merge --no-ff`).
    #[default]
    Merge,
    /// Squash branch commits into a single commit on base (`git merge --squash` + `git commit`).
    Squash,
    /// Rebase branch commits onto base tip in the worktree, then fast-forward base (`git merge --ff-only`).
    Rebase,
}

impl MergeMethod {
    /// Every supported method in display order.
    pub const ALL: [MergeMethod; 3] =
        [MergeMethod::Merge, MergeMethod::Squash, MergeMethod::Rebase];

    pub fn label(self) -> &'static str {
        match self {
            MergeMethod::Merge => "Merge commit",
            MergeMethod::Squash => "Squash and commit",
            MergeMethod::Rebase => "Rebase and fast-forward",
        }
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
    /// Default commit message offered when squashing commits.
    pub squash_message: String,
    /// Commits on the branch that the base does not have.
    pub ahead: u32,
    /// Commits on the base that the branch does not have.
    pub behind: u32,
    pub prediction: Prediction,
    /// The merge hooks that will run in the base checkout.
    pub hooks: Vec<String>,
    /// The folder they run from: a merge that finds another (`core.hooksPath` changed) does
    /// nothing.
    pub hooks_dir: Option<PathBuf>,
    /// Commit hooks in the base checkout that will run for a squash commit.
    pub squash_hooks: Vec<String>,
    /// Hooks in the worktree (`info.path`) that will run during `git rebase`.
    pub rebase_worktree_hooks: Vec<String>,
    /// The worktree's own hooks folder when rebasing.
    pub rebase_worktree_hooks_dir: Option<PathBuf>,
    /// Hooks in the base checkout that will run during `git merge --ff-only`.
    pub rebase_base_hooks: Vec<String>,
    /// Every linked worktree of the repository (this one included), as git lists them: a folder
    /// inside one of them is not in the base checkout even when it is inside its folder.
    pub worktrees: Vec<PathBuf>,
    /// Whether `branch` still has a registered worktree at `info.path` (false when merging a
    /// branch whose clean worktree was already removed, e.g. a closed delegated worker).
    pub has_worktree: bool,
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

/// One git command, the settings it runs with, and the directory it runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitStep {
    pub dir: PathBuf,
    pub args: Vec<String>,
    /// Settings for this one command (key, value), passed through git's configuration
    /// environment ([`config_env`]), which takes any key, `=` in a branch name included.
    pub config: Vec<(String, String)>,
}

impl GitStep {
    /// The step as the preview shows it: the directory, the settings it runs with, then the
    /// command as a POSIX shell would read it back.
    pub fn shown(&self) -> String {
        let mut text = format!("In {}:\n", self.dir.display());
        if !self.config.is_empty() {
            let settings: Vec<String> = self
                .config
                .iter()
                .map(|(key, value)| {
                    if value.is_empty() {
                        format!("{key} (unset)")
                    } else {
                        format!("{key}={value}")
                    }
                })
                .collect();
            text.push_str(&format!("with {}:\n", settings.join(", ")));
        }
        text.push_str(&agent_core::commit_message::shown_command(&self.args));
        text
    }

    /// The environment that gives git this step's settings.
    pub fn env(&self) -> Vec<(String, String)> {
        config_env(&self.config)
    }
}

/// `settings` as git's configuration environment: `GIT_CONFIG_COUNT`, then a
/// `GIT_CONFIG_KEY_<n>`/`GIT_CONFIG_VALUE_<n>` pair each. These override every configuration
/// file but are themselves overridden by `git -c` options inherited in `GIT_CONFIG_PARAMETERS`,
/// so that is set empty: nothing from the app's own environment changes what was shown.
pub fn config_env(settings: &[(String, String)]) -> Vec<(String, String)> {
    let mut env = vec![
        ("GIT_CONFIG_PARAMETERS".to_owned(), String::new()),
        ("GIT_CONFIG_COUNT".to_owned(), settings.len().to_string()),
    ];
    for (n, (key, value)) in settings.iter().enumerate() {
        env.push((format!("GIT_CONFIG_KEY_{n}"), key.clone()));
        env.push((format!("GIT_CONFIG_VALUE_{n}"), value.clone()));
    }
    env
}

/// The settings that would make the merge something other than what the preview shows, turned
/// off for that one command: `branch.<base>.mergeOptions` (a `--no-commit`, `--squash`, `-s`
/// there), `merge.ff` (already overridden by `--no-ff`), `merge.log` (which would append a log
/// to the shown message) and `merge.autoStash`. Hooks and signing are not touched: they run as
/// at any `git merge`.
pub fn merge_config(base: &str) -> Vec<(String, String)> {
    vec![
        (format!("branch.{base}.mergeOptions"), String::new()),
        ("merge.ff".to_owned(), "false".to_owned()),
        ("merge.log".to_owned(), "false".to_owned()),
        ("merge.autoStash".to_owned(), "false".to_owned()),
    ]
}

/// The arguments of the merge of `tip`, the commit the preview read as `branch`'s: always a
/// merge commit with the message git itself gives a branch merge (one `-m` value), and the exact
/// commit after `--`, so no name is ever read as an option, no tag of the same name and no
/// commit made on the branch since the preview is merged in its place.
pub fn merge_args(branch: &str, tip: &str) -> Vec<String> {
    vec![
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
        args: merge_args(&plan.branch, &plan.branch_tip),
        config: merge_config(&plan.base),
    }]
}

/// Config overrides for `git merge --squash`: ignores branch merge options, forces `merge.ff=true`
/// so a user's `merge.ff=false` or `merge.ff=only` never breaks `--squash`, and disables log/stash.
pub fn squash_config(base: &str) -> Vec<(String, String)> {
    [
        (format!("branch.{base}.mergeOptions"), ""),
        ("merge.ff".to_owned(), "true"),
        ("merge.log".to_owned(), "false"),
        ("merge.autoStash".to_owned(), "false"),
    ]
    .into_iter()
    .map(|(k, v)| (k, v.to_owned()))
    .collect()
}

/// Config overrides for `git merge --ff-only`: ignores branch merge options and disables auto-stash,
/// while requiring fast-forward instead of `--no-ff`.
pub fn ff_only_config(base: &str) -> Vec<(String, String)> {
    [
        (format!("branch.{base}.mergeOptions"), ""),
        ("merge.ff".to_owned(), "only"),
        ("merge.autoStash".to_owned(), "false"),
    ]
    .into_iter()
    .map(|(k, v)| (k, v.to_owned()))
    .collect()
}

/// Config overrides for `git rebase` inside the worktree.
pub fn rebase_config() -> Vec<(String, String)> {
    [
        ("rebase.autoStash", "false"),
        ("rebase.updateRefs", "false"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect()
}

/// Arguments for `git merge --squash`.
pub fn squash_args(tip: &str) -> Vec<String> {
    vec![
        "merge".to_owned(),
        "--squash".to_owned(),
        "--".to_owned(),
        tip.to_owned(),
    ]
}

/// Arguments for committing a staged squash.
pub fn squash_commit_args(message: &str) -> Vec<String> {
    vec!["commit".to_owned(), "-m".to_owned(), message.to_owned()]
}

/// Arguments for rebasing the worktree branch onto `base_tip`.
pub fn rebase_args(base_tip: &str) -> Vec<String> {
    vec!["rebase".to_owned(), "--".to_owned(), base_tip.to_owned()]
}

/// Arguments for fast-forwarding `base` to `tip`.
pub fn ff_only_args(tip: &str) -> Vec<String> {
    vec![
        "merge".to_owned(),
        "--ff-only".to_owned(),
        "--".to_owned(),
        tip.to_owned(),
    ]
}

/// The commands [`merge_with_method`] runs for `method`. For `Rebase`, the second step takes the
/// commit SHA produced by step 1, shown here as `<resulting rebased commit>`.
pub fn commands_for(
    plan: &MergePlan,
    method: MergeMethod,
    squash_message: Option<&str>,
) -> Vec<GitStep> {
    match method {
        MergeMethod::Merge => commands(plan),
        MergeMethod::Squash => {
            let msg = squash_message
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(plan.squash_message.as_str());
            let redacted = agent_core::redact::redact(msg);
            vec![
                GitStep {
                    dir: plan.base_dir.clone(),
                    args: squash_args(&plan.branch_tip),
                    config: squash_config(&plan.base),
                },
                GitStep {
                    dir: plan.base_dir.clone(),
                    args: squash_commit_args(&redacted),
                    config: Vec::new(),
                },
            ]
        }
        MergeMethod::Rebase => vec![
            GitStep {
                dir: plan.info.path.clone(),
                args: rebase_args(&plan.base_tip),
                config: rebase_config(),
            },
            GitStep {
                dir: plan.base_dir.clone(),
                args: ff_only_args("<resulting rebased commit>"),
                config: ff_only_config(&plan.base),
            },
        ],
    }
}

/// Reads what merging `info`'s branch into its base would do. Reads only. `Err` when there is
/// nothing to plan against: the names cannot be passed to git, the worktree is no longer on
/// its branch, or a branch is gone.
pub fn preflight(info: &WorktreeInfo) -> Result<MergePlan, String> {
    preflight_with(info, &[])
}

/// Preflight for merging `branch` into `base` (or `main_toplevel`'s current branch when `None`),
/// using `worktree` when it is still registered and falling back to branch-only preflight if the
/// worktree was already cleaned up.
pub fn preflight_branch(
    main_toplevel: &Path,
    branch: &str,
    base: Option<&str>,
    worktree: Option<&Path>,
) -> Result<MergePlan, String> {
    let info = WorktreeInfo {
        path: worktree.unwrap_or(main_toplevel).to_path_buf(),
        branch: branch.to_owned(),
        main_toplevel: main_toplevel.to_path_buf(),
        base: base.map(str::to_owned),
    };
    if worktree.is_some() && is_registered(&info).unwrap_or(false) {
        preflight_with(&info, &[])
    } else {
        preflight_branch_with(&info, &[])
    }
}

fn preflight_with(info: &WorktreeInfo, env: &[(&str, &OsStr)]) -> Result<MergePlan, String> {
    preflight_core(info, true, env)
}

fn preflight_branch_with(info: &WorktreeInfo, env: &[(&str, &OsStr)]) -> Result<MergePlan, String> {
    preflight_core(info, false, env)
}

fn preflight_core(
    info: &WorktreeInfo,
    require_worktree: bool,
    env: &[(&str, &OsStr)],
) -> Result<MergePlan, String> {
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
    let has_worktree = if require_worktree {
        if !is_registered(info)? {
            return Err(format!(
                "{} is no longer a worktree on {branch}",
                info.path.display()
            ));
        }
        true
    } else {
        false
    };
    let branch_tip = tip(main, &branch, env)?;
    let base_tip = tip(main, &base, env)?;
    let mut refusals = Vec::new();

    if has_worktree {
        let worktree_status = git_raw(
            &info.path,
            ["status", "--porcelain", "--untracked-files=normal"],
            env,
            QUERY_TIMEOUT_SECS,
        )?;
        if !worktree_status.is_empty() {
            refusals.push(Refusal::WorktreeChanges);
        }
    }

    let base_dir = match crate::worktree::checkout_of(main, &base)? {
        Some(dir) => dir,
        None => main.to_path_buf(),
    };
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
    let squash_message = default_squash_message(main, &base_tip, &branch_tip, &branch, env);
    let (hooks_dir, hooks) = hooks_for_dir(&base_dir, &MERGE_HOOKS, env);
    let (_, squash_hooks) = hooks_for_dir(&base_dir, &SQUASH_HOOKS, env);
    let (rebase_worktree_hooks_dir, rebase_worktree_hooks) = if has_worktree {
        hooks_for_dir(&info.path, &REBASE_WORKTREE_HOOKS, env)
    } else {
        (None, Vec::new())
    };
    let (_, rebase_base_hooks) = hooks_for_dir(&base_dir, &REBASE_BASE_HOOKS, env);
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
        squash_message,
        ahead,
        behind,
        prediction,
        hooks_dir,
        hooks,
        squash_hooks,
        rebase_worktree_hooks,
        rebase_worktree_hooks_dir,
        rebase_base_hooks,
        worktrees,
        has_worktree,
        refusals,
    })
}

fn default_squash_message(
    repo: &Path,
    base_tip: &str,
    branch_tip: &str,
    branch: &str,
    env: &[(&str, &OsStr)],
) -> String {
    let fallback = format!("Squash merge branch '{branch}'");
    let Ok(out) = git_output(
        repo,
        [
            "log",
            "--format=%s",
            "--no-merges",
            "--max-count=20",
            &format!("{base_tip}..{branch_tip}"),
        ],
        env,
        QUERY_TIMEOUT_SECS,
    ) else {
        return fallback;
    };
    if !out.status.success() {
        return fallback;
    }
    let subjects: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| format!("- {}", agent_core::redact::redact(l)))
        .collect();
    if subjects.is_empty() {
        fallback
    } else if subjects.len() == 1 {
        subjects[0].trim_start_matches("- ").to_owned()
    } else {
        format!("{fallback}\n\n{}", subjects.join("\n"))
    }
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

/// The folder `dir`'s repository runs hooks from (`core.hooksPath` honoured), and the executable
/// hooks among `names` installed there.
fn hooks_for_dir(
    dir: &Path,
    names: &[&str],
    env: &[(&str, &OsStr)],
) -> (Option<PathBuf>, Vec<String>) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(out) = git_raw(
        dir,
        ["rev-parse", "--git-path", "hooks"],
        env,
        QUERY_TIMEOUT_SECS,
    ) else {
        return (None, Vec::new());
    };
    let path = PathBuf::from(String::from_utf8_lossy(&out).trim());
    let hooks = if path.is_absolute() {
        path
    } else {
        dir.join(path)
    };
    let found = names
        .iter()
        .filter(|name| {
            std::fs::metadata(hooks.join(name))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
        .map(|name| (*name).to_owned())
        .collect();
    (Some(hooks), found)
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

/// Executes `method` (`Merge`, `Squash`, or `Rebase`) on `plan` after re-validating every tip,
/// branch, checkout and hook against when the preview was read.
pub fn merge_with_method(
    plan: &MergePlan,
    method: MergeMethod,
    squash_message: Option<&str>,
) -> Result<Merged, String> {
    merge_method_with(plan, method, squash_message, &[], MERGE_TIMEOUT_SECS)
}

fn merge_with(
    plan: &MergePlan,
    env: &[(&str, &OsStr)],
    timeout_secs: u64,
) -> Result<Merged, String> {
    merge_method_with(plan, MergeMethod::Merge, None, env, timeout_secs)
}

fn merge_method_with(
    plan: &MergePlan,
    method: MergeMethod,
    squash_message: Option<&str>,
    env: &[(&str, &OsStr)],
    timeout_secs: u64,
) -> Result<Merged, String> {
    if let Some(refusal) = plan.refusals.first() {
        return Err(refusal.reason(plan));
    }
    let trimmed_squash = squash_message.map(str::trim).unwrap_or("");
    if method == MergeMethod::Squash {
        if trimmed_squash.is_empty() {
            return Err(
                "Write a commit message for the squash commit first.\n\nNothing was merged."
                    .to_owned(),
            );
        }
        if trimmed_squash.contains('\0') {
            return Err(
                "The commit message cannot contain NUL bytes.\n\nNothing was merged.".to_owned(),
            );
        }
        if agent_core::redact::redact(trimmed_squash) != trimmed_squash {
            return Err(
                "The squash commit message looks like it contains a secret or token.\n\nNothing was merged."
                    .to_owned(),
            );
        }
    }
    if !plan.has_worktree && method == MergeMethod::Rebase {
        return Err(
            "The branch has no active worktree to rebase in; choose Merge commit or Squash and \
             commit.\n\nNothing was merged."
                .to_owned(),
        );
    }
    let now = if plan.has_worktree {
        preflight_with(&plan.info, env)
    } else {
        preflight_branch_with(&plan.info, env)
    }
    .map_err(|why| {
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
    let hooks_changed = match method {
        MergeMethod::Merge => now.hooks != plan.hooks || now.hooks_dir != plan.hooks_dir,
        MergeMethod::Squash => {
            now.squash_hooks != plan.squash_hooks || now.hooks_dir != plan.hooks_dir
        }
        MergeMethod::Rebase => {
            now.rebase_worktree_hooks != plan.rebase_worktree_hooks
                || now.rebase_worktree_hooks_dir != plan.rebase_worktree_hooks_dir
                || now.rebase_base_hooks != plan.rebase_base_hooks
                || now.hooks_dir != plan.hooks_dir
        }
    };
    if hooks_changed {
        return Err(
            "The merge hooks that would run changed since the preview (one added or removed, \
             or core.hooksPath changed). Nothing was merged: open the merge again to see which \
             run now."
                .to_owned(),
        );
    }
    if let Some(refusal) = now.refusals.first() {
        return Err(format!(
            "{}\n\nNothing was merged.",
            agent_core::redact::redact(&refusal.reason(&now))
        ));
    }
    match method {
        MergeMethod::Merge => execute(plan, env, timeout_secs),
        MergeMethod::Squash => execute_squash(plan, trimmed_squash, env, timeout_secs),
        MergeMethod::Rebase => execute_rebase(plan, env, timeout_secs),
    }
}

fn run_step(step: &GitStep, env: &[(&str, &OsStr)], timeout_secs: u64) -> Result<(), String> {
    let settings = step.env();
    let mut step_env: Vec<(&str, &OsStr)> = env.to_vec();
    step_env.extend(
        settings
            .iter()
            .map(|(k, v)| (k.as_str(), OsStr::new(v.as_str()))),
    );
    let ran = git_output_in_group(&step.dir, &step.args, &step_env, timeout_secs);
    let why = match ran {
        Ok(out) if out.status.success() => return Ok(()),
        Ok(out) => said(&out),
        Err(why) => why,
    };
    let why = agent_core::redact::redact(&why);
    let mut message = why;
    if message.contains(&format!("did not finish within {timeout_secs}s")) {
        message.push_str(
            "\n\nIt was stopped, with the hooks and helpers it started. Hooks and signing \
             that ask for input (a passphrase prompt, for one) cannot be answered from here: \
             merge in a terminal instead.",
        );
    }
    Err(message)
}

fn execute_squash(
    plan: &MergePlan,
    message: &str,
    env: &[(&str, &OsStr)],
    timeout_secs: u64,
) -> Result<Merged, String> {
    if discover(&plan.base_dir)
        .ok()
        .flatten()
        .is_some_and(|r| r.git_dir.join("SQUASH_MSG").exists())
    {
        return Err(
            "A squash commit message (.git/SQUASH_MSG) is already present in the base checkout. \
             Nothing was merged."
                .to_owned(),
        );
    }
    let steps = commands_for(plan, MergeMethod::Squash, Some(message));
    if let Err(err) = run_step(&steps[0], env, timeout_secs) {
        return Err(format!(
            "{err}\n\n{}",
            recover_squash(plan, None, env, timeout_secs)
        ));
    }
    let expected_tree = git_raw(&plan.base_dir, ["write-tree"], env, QUERY_TIMEOUT_SECS)
        .ok()
        .map(|out| String::from_utf8_lossy(&out).trim().to_owned())
        .filter(|s| !s.is_empty());
    if let Err(err) = run_step(&steps[1], env, timeout_secs) {
        return Err(format!(
            "{err}\n\n{}",
            recover_squash(plan, expected_tree.as_deref(), env, timeout_secs)
        ));
    }
    if let Err(why) = check_squash_merged(plan, env) {
        return Err(format!(
            "{why}\n\n{}",
            recover_squash(plan, expected_tree.as_deref(), env, timeout_secs)
        ));
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

fn check_squash_merged(plan: &MergePlan, env: &[(&str, &OsStr)]) -> Result<(), String> {
    let not = |what: &str| {
        Err(format!(
            "git reported success, but {what}, so this is not the squash commit the preview showed."
        ))
    };
    let Some(repo) = discover(&plan.base_dir).ok().flatten() else {
        return not("the base checkout could not be read back");
    };
    if in_progress_op(&repo.git_dir).is_some() {
        return not("an operation is still in progress");
    }
    if repo.branch.as_deref() != Some(plan.base.as_str()) {
        return not("the base checkout is no longer on its branch");
    }
    if repo.head.as_deref() == Some(plan.base_tip.as_str()) {
        return not("no squash commit was made");
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
    if parents != [plan.base_tip.clone()] {
        return not("the new commit is not a single-parent squash on the base commit shown");
    }
    Ok(())
}

fn recover_squash(
    plan: &MergePlan,
    expected_tree: Option<&str>,
    env: &[(&str, &OsStr)],
    timeout_secs: u64,
) -> String {
    let dir = plan.base_dir.display();
    let base = &plan.base;
    let before = short(&plan.base_tip);
    let Some(repo) = discover(&plan.base_dir).ok().flatten() else {
        return format!(
            "{dir} could not be read after the squash ({base} was at {before}). {}",
            left_as_is(&plan.base_dir)
        );
    };
    if index_locked(&repo.git_dir) {
        return format!(
            "A git was stopped while it held {dir}'s index: .git/index.lock is left behind, so the \
             checkout cannot be trusted to be as it was ({base} was at {before}). Once no git is \
             running there, remove .git/index.lock and check `git status`."
        );
    }
    if repo.head.as_deref() == Some(plan.base_tip.as_str())
        && repo.branch.as_deref() == Some(plan.base.as_str())
    {
        let squash_msg = repo.git_dir.join("SQUASH_MSG");
        let safe_to_reset = if !squash_msg.exists() {
            false
        } else if let Some(expected) = expected_tree {
            // Step 1 finished with staged tree `expected`: only reset if the index tree still
            // equals `expected` AND there are no unstaged tracked edits made meanwhile.
            let tree_now = git_raw(&plan.base_dir, ["write-tree"], env, QUERY_TIMEOUT_SECS)
                .ok()
                .map(|out| String::from_utf8_lossy(&out).trim().to_owned());
            let unstaged_clean = git_raw(
                &plan.base_dir,
                ["diff", "--name-only"],
                env,
                QUERY_TIMEOUT_SECS,
            )
            .is_ok_and(|out| out.is_empty());
            tree_now.as_deref() == Some(expected) && unstaged_clean
        } else {
            // Step 1 itself stopped (conflict/abort) and created SQUASH_MSG: only reset when it
            // actually wrote unmerged conflict entries in the index.
            git_raw(
                &plan.base_dir,
                ["ls-files", "--unmerged"],
                env,
                QUERY_TIMEOUT_SECS,
            )
            .is_ok_and(|out| !out.is_empty())
        };
        if !safe_to_reset {
            return format!(
                "{dir} still has its HEAD on {base} at {before}; nothing was reset automatically. {}",
                left_as_is(&plan.base_dir)
            );
        }
        let reset = git_output_in_group(&plan.base_dir, ["reset", "--merge"], env, timeout_secs);
        let _ = std::fs::remove_file(&squash_msg);
        if matches!(reset, Ok(out) if out.status.success())
            && standing(plan, env).is_some_and(|s| s.at_base && !s.locked && s.changed.is_empty())
        {
            return format!(
                "Nothing was committed: undone with `git reset --merge`. {dir} is back on \
                 {base} at {before}."
            );
        }
    }
    format!(
        "{dir} could not be automatically reset after the failed squash ({base} was at {before}). {}",
        left_as_is(&plan.base_dir)
    )
}

fn execute_rebase(
    plan: &MergePlan,
    env: &[(&str, &OsStr)],
    timeout_secs: u64,
) -> Result<Merged, String> {
    let Some(wt_repo) = discover(&plan.info.path).ok().flatten() else {
        return Err(
            "The worktree could not be read before rebasing. Nothing was merged.".to_owned(),
        );
    };
    if let Some(op) = in_progress_op(&wt_repo.git_dir) {
        return Err(format!("{}\n\nNothing was merged.", op.reason()));
    }
    if wt_repo.branch.as_deref() != Some(plan.branch.as_str())
        || wt_repo.head.as_deref() != Some(plan.branch_tip.as_str())
    {
        return Err(format!(
            "{} changed since the preview. Nothing was merged.",
            plan.branch
        ));
    }

    let rebase_step = GitStep {
        dir: plan.info.path.clone(),
        args: rebase_args(&plan.base_tip),
        config: rebase_config(),
    };
    if let Err(err) = run_step(&rebase_step, env, timeout_secs) {
        return Err(format!(
            "{err}\n\n{}",
            recover_rebase_worktree(plan, env, timeout_secs)
        ));
    }

    let Some(wt_after) = discover(&plan.info.path).ok().flatten() else {
        return Err(
            "The worktree could not be read after rebasing. The base checkout was not changed."
                .to_owned(),
        );
    };
    if in_progress_op(&wt_after.git_dir).is_some()
        || wt_after.branch.as_deref() != Some(plan.branch.as_str())
    {
        return Err(format!(
            "The rebase stopped before finishing, so {} was not fast-forwarded. Check `git status` in {}.",
            plan.base,
            plan.info.path.display()
        ));
    }
    let Some(rebased_tip) = wt_after.head else {
        return Err(
            "The worktree has no HEAD after rebase. The base checkout was not changed.".to_owned(),
        );
    };
    let ancestor = git_output(
        &plan.info.path,
        ["merge-base", "--is-ancestor", &plan.base_tip, &rebased_tip],
        env,
        QUERY_TIMEOUT_SECS,
    );
    if !matches!(ancestor, Ok(out) if out.status.success()) {
        return Err(format!(
            "Rebased {} is not based on {} ({}). {} was not moved.",
            plan.branch,
            plan.base,
            short(&plan.base_tip),
            plan.base
        ));
    }

    // Re-validate base checkout immediately before fast-forwarding: another process may have
    // switched branch, committed, dirtied files, or changed hooks while rebase ran.
    let Some(base_now) = discover(&plan.base_dir).ok().flatten() else {
        return Err(format!(
            "{} was rebased in {}, while {} could not be read back; nothing was fast-forwarded.",
            plan.branch,
            plan.info.path.display(),
            plan.base
        ));
    };
    let base_clean = git_raw(
        &plan.base_dir,
        ["status", "--porcelain", "--untracked-files=no"],
        env,
        QUERY_TIMEOUT_SECS,
    )
    .is_ok_and(|out| out.is_empty());
    let (base_hooks_dir_now, base_hooks_now) =
        hooks_for_dir(&plan.base_dir, &REBASE_BASE_HOOKS, env);
    if in_progress_op(&base_now.git_dir).is_some()
        || base_now.branch.as_deref() != Some(plan.base.as_str())
        || base_now.head.as_deref() != Some(plan.base_tip.as_str())
        || !base_clean
        || base_hooks_dir_now != plan.hooks_dir
        || base_hooks_now != plan.rebase_base_hooks
    {
        return Err(format!(
            "{} or {}'s state changed while rebasing. {} was rebased in {}, while {} was not moved.",
            plan.base,
            plan.base_dir.display(),
            plan.branch,
            plan.info.path.display(),
            plan.base
        ));
    }

    let ff_step = GitStep {
        dir: plan.base_dir.clone(),
        args: ff_only_args(&rebased_tip),
        config: ff_only_config(&plan.base),
    };
    if let Err(err) = run_step(&ff_step, env, timeout_secs) {
        let base_state = match discover(&plan.base_dir).ok().flatten() {
            Some(repo)
                if repo.branch.as_deref() == Some(plan.base.as_str())
                    && repo.head.as_deref() == Some(rebased_tip.as_str()) =>
            {
                format!(
                    "{} DID advance to {} (likely a post-merge hook failed or timed out).",
                    plan.base,
                    short(&rebased_tip)
                )
            }
            Some(repo)
                if repo.branch.as_deref() == Some(plan.base.as_str())
                    && repo.head.as_deref() == Some(plan.base_tip.as_str()) =>
            {
                format!(
                    "{} stayed untouched at {}.",
                    plan.base,
                    short(&plan.base_tip)
                )
            }
            Some(repo) => format!(
                "{} is now at {} (unexpected state — check git status in {}).",
                plan.base_dir.display(),
                repo.head
                    .as_deref()
                    .map(short)
                    .unwrap_or_else(|| "detached".into()),
                plan.base_dir.display()
            ),
            None => format!(
                "{} could not be verified after the fast-forward failed.",
                plan.base_dir.display()
            ),
        };
        return Err(format!(
            "{err}\n\n{} was rebased in {}. {base_state}",
            plan.branch,
            plan.info.path.display(),
        ));
    }
    check_rebase_merged(plan, &rebased_tip, env)
        .map_err(|why| format!("{why}\n\n{}", left_as_is(&plan.base_dir)))?;
    Ok(Merged {
        before: short(&plan.base_tip),
        after: short(&rebased_tip),
    })
}

fn check_rebase_merged(
    plan: &MergePlan,
    rebased_tip: &str,
    _env: &[(&str, &OsStr)],
) -> Result<(), String> {
    let not = |what: &str| {
        Err(format!(
            "git reported success, but {what}, so the fast-forward did not complete as shown."
        ))
    };
    let Some(repo) = discover(&plan.base_dir).ok().flatten() else {
        return not("the base checkout could not be read back");
    };
    if in_progress_op(&repo.git_dir).is_some() {
        return not("an operation is still in progress in the base checkout");
    }
    if repo.branch.as_deref() != Some(plan.base.as_str()) {
        return not("the base checkout is no longer on its branch");
    }
    if repo.head.as_deref() != Some(rebased_tip) {
        return not("the base branch did not advance to the rebased commit");
    }
    Ok(())
}

fn recover_rebase_worktree(plan: &MergePlan, env: &[(&str, &OsStr)], timeout_secs: u64) -> String {
    let wt_dir = plan.info.path.display();
    let Some(repo) = discover(&plan.info.path).ok().flatten() else {
        return format!(
            "{wt_dir} could not be read after the rebase failed. {} was not touched.",
            plan.base
        );
    };
    if index_locked(&repo.git_dir) {
        return format!(
            "A git was stopped while holding {wt_dir}'s index: remove .git/index.lock once git \
             exits, then run `git rebase --abort`. {} was not touched.",
            plan.base
        );
    }
    if in_progress_op(&repo.git_dir) == Some(InProgress::Rebase) {
        let abort = git_output_in_group(&plan.info.path, ["rebase", "--abort"], env, timeout_secs);
        if matches!(abort, Ok(out) if out.status.success()) {
            return format!(
                "Rebase aborted with `git rebase --abort`. {wt_dir} is back at {}, and {} was not touched.",
                short(&plan.branch_tip),
                plan.base
            );
        }
    }
    format!(
        "The rebase stopped without completing; {} was not touched. {}",
        plan.base,
        left_as_is(&plan.info.path)
    )
}

/// Runs [`commands`], undoing the merge if it stops part-way.
fn execute(plan: &MergePlan, env: &[(&str, &OsStr)], timeout_secs: u64) -> Result<Merged, String> {
    for step in commands(plan) {
        let settings = step.env();
        let mut step_env: Vec<(&str, &OsStr)> = env.to_vec();
        step_env.extend(
            settings
                .iter()
                .map(|(k, v)| (k.as_str(), OsStr::new(v.as_str()))),
        );
        let ran = git_output_in_group(&step.dir, &step.args, &step_env, timeout_secs);
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

/// How the base checkout stands after a failed merge.
struct Standing {
    /// HEAD is the base's old commit, on the base branch.
    at_base: bool,
    merging: bool,
    locked: bool,
    /// The tracked files that differ from HEAD (staged or not), as `git status` names them.
    changed: Vec<String>,
}

fn standing(plan: &MergePlan, env: &[(&str, &OsStr)]) -> Option<Standing> {
    let repo = discover(&plan.base_dir).ok().flatten()?;
    let status = git_raw(
        &plan.base_dir,
        ["status", "--porcelain", "--untracked-files=no"],
        env,
        QUERY_TIMEOUT_SECS,
    )
    .ok()?;
    let changed = String::from_utf8_lossy(&status)
        .lines()
        .filter_map(|line| line.get(3..))
        .map(str::to_owned)
        .collect();
    Some(Standing {
        at_base: repo.head.as_deref() == Some(plan.base_tip.as_str())
            && repo.branch.as_deref() == Some(plan.base.as_str()),
        merging: in_progress_op(&repo.git_dir) == Some(InProgress::Merge),
        locked: index_locked(&repo.git_dir),
        changed,
    })
}

/// The commands the user may run in `dir` when the app leaves a checkout as git left it:
/// what it shows, and what puts the tracked files back (said to discard changes).
fn left_as_is(dir: &Path) -> String {
    let git = |args: &[&str]| {
        let mut all = vec!["-C".to_owned(), dir.to_string_lossy().into_owned()];
        all.extend(args.iter().map(|a| (*a).to_owned()));
        agent_core::commit_message::shown_command(&all)
    };
    format!(
        "`{}` shows what is there. If none of it is yours, `{}` puts every tracked file back as \
         the base's commit has it: it discards every uncommitted change to them, staged or not, \
         yours included.",
        git(&["status"]),
        git(&["restore", "--staged", "--worktree", "--", "."])
    )
}

/// After a merge that failed: undoes it with `git merge --abort`, and only when git recorded a
/// merge in progress (`MERGE_HEAD`). In every other case the checkout is left exactly as git
/// left it, since nothing can tell what git wrote from changes of the user's made meanwhile: it
/// says what state it is in and what the user can run. Says whether the base checkout is
/// exactly as it was.
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
            "{dir} could not be read after the merge ({base} was at {before}). {}",
            left_as_is(&plan.base_dir)
        );
    };
    // A lock left behind by a git that was stopped: an undo would only fail on it, and the
    // checkout cannot be read as settled until the user removes it.
    if now.locked {
        return locked_words;
    }
    if now.merging {
        let undo = git_output_in_group(&plan.base_dir, abort_args(), env, timeout_secs);
        let undone = match undo {
            Ok(out) if out.status.success() => Ok(()),
            Ok(out) => Err(failure_message(&out.stderr)),
            Err(why) => Err(why),
        };
        let after = standing(plan, env);
        if after.as_ref().is_some_and(|s| s.locked) {
            return locked_words;
        }
        return match (undone, after) {
            (Ok(()), Some(s)) if s.at_base && !s.merging && s.changed.is_empty() => format!(
                "The merge was undone with `git merge --abort`: {base} is at {before} again, as \
                 it was."
            ),
            (Err(why), _) => format!(
                "The merge could not be undone ({}). It is left in progress in {dir}: run `git \
                 merge --abort` there, or resolve it and commit.",
                agent_core::redact::redact(&why)
            ),
            (Ok(()), _) => format!(
                "The merge was undone with `git merge --abort`, but {dir} is still not as it was \
                 ({base} was at {before}). {}",
                left_as_is(&plan.base_dir)
            ),
        };
    }
    match (now.at_base, now.changed.is_empty()) {
        (true, true) => format!("Nothing was merged: {base} is still at {before}."),
        (true, false) => {
            let mut files: Vec<String> = now
                .changed
                .iter()
                .take(8)
                .map(|f| agent_core::redact::redact(f))
                .collect();
            if now.changed.len() > 8 {
                files.push(format!("{} more", now.changed.len() - 8));
            }
            format!(
                "Nothing was committed and {base} is still at {before}, but git did not record a \
                 merge it could undo, and {dir} has tracked files that differ from it: {}. They \
                 are left exactly as they are, since the app cannot tell what git wrote from \
                 changes of yours. {}",
                files.join(", "),
                left_as_is(&plan.base_dir)
            )
        }
        (false, _) => format!(
            "{dir} is not as it was before the merge ({base} was at {before}), and nothing was \
             undone. {}",
            left_as_is(&plan.base_dir)
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
            squash_message: "feat/x".into(),
            ahead: 1,
            behind: 0,
            prediction: Prediction::Clean,
            hooks: Vec::new(),
            hooks_dir: None,
            squash_hooks: Vec::new(),
            rebase_worktree_hooks: Vec::new(),
            rebase_worktree_hooks_dir: None,
            rebase_base_hooks: Vec::new(),
            worktrees: Vec::new(),
            has_worktree: true,
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
            let args = merge_args(branch, &tip);
            let split = args.iter().position(|a| a == "--").unwrap();
            // Every argument before `--` is an option of ours, except the message after `-m`.
            let options: Vec<&str> = args[..split]
                .iter()
                .enumerate()
                .filter(|(i, _)| *i == 0 || args[i - 1] != "-m")
                .map(|(_, a)| a.as_str())
                .collect();
            for arg in &options {
                assert!(!FORBIDDEN.contains(arg), "{arg} in {args:?}");
            }
            assert_eq!(options, ["merge", "--no-ff", "--no-edit", "-m"]);
            assert_eq!(args[4], format!("Merge branch '{branch}'"));
            // The exact commit the preview read, never a name.
            assert_eq!(&args[split + 1..], std::slice::from_ref(&tip));
        }
        assert_eq!(abort_args(), ["merge", "--abort"]);
    }

    #[test]
    fn the_overrides_go_through_the_config_environment_whatever_the_branch_name() {
        // A base named with `=`: a `-c` option would split its key there.
        let config = merge_config("a=b");
        assert_eq!(
            config,
            [
                ("branch.a=b.mergeOptions", ""),
                ("merge.ff", "false"),
                ("merge.log", "false"),
                ("merge.autoStash", "false"),
            ]
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
        );
        let env = config_env(&config);
        assert_eq!(env[0], ("GIT_CONFIG_PARAMETERS".into(), String::new()));
        assert_eq!(env[1], ("GIT_CONFIG_COUNT".into(), "4".into()));
        assert_eq!(
            env[2],
            ("GIT_CONFIG_KEY_0".into(), "branch.a=b.mergeOptions".into())
        );
        assert_eq!(env[3], ("GIT_CONFIG_VALUE_0".into(), String::new()));
        assert_eq!(env.len(), 2 + 2 * 4);
    }

    #[test]
    fn the_preview_shows_what_runs_and_where() {
        let p = plan("feat/x");
        let steps = commands(&p);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].dir, p.base_dir);
        assert_eq!(steps[0].args, merge_args("feat/x", &p.branch_tip));
        assert_eq!(steps[0].config, merge_config("main"));
        assert_eq!(
            steps[0].shown(),
            format!(
                "In /r/main:\nwith branch.main.mergeOptions (unset), merge.ff=false, \
                 merge.log=false, merge.autoStash=false:\ngit merge --no-ff --no-edit -m \
                 'Merge branch '\\''feat/x'\\''' -- {}",
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
            // The hook runs before git records the merge: no MERGE_HEAD, HEAD unmoved, and the
            // merged files in the index and tree. Nothing is undone (the app cannot tell them
            // from the user's own changes); the message says so and what the user can run.
            assert_eq!(head(&f.main), before.0, "HEAD never moved");
            assert_eq!(in_progress_op(&f.main.join(".git")), None);
            assert!(err.contains("left exactly as they are"), "{err}");
            assert!(err.contains("new.txt"), "the files are named: {err}");
            assert!(
                err.contains("restore --staged --worktree -- .")
                    && err.contains("discards every uncommitted change"),
                "{err}"
            );
            assert!(err.contains("status`"), "{err}");
            assert!(!err.contains("reset"), "{err}");
        }

        #[test]
        fn a_change_staged_after_the_last_check_is_never_lost() {
            let Some(f) = fixture() else { return };
            let p = plan_of(&f);
            // The user stages a change between the re-check and git starting.
            fs::write(f.main.join("b.txt"), "mine, staged\n").unwrap();
            sh(&f.main, &["add", "b.txt"]);
            let before = state(&f.main);
            let result = execute(&p, &env(), MERGE_TIMEOUT_SECS);
            // Whatever git did, the user's change is still there and still staged.
            assert_eq!(
                fs::read_to_string(f.main.join("b.txt")).unwrap(),
                "mine, staged\n",
                "{result:?}"
            );
            match result {
                Err(err) => {
                    assert_eq!(state(&f.main), before, "{err}");
                    assert!(!err.contains("undone"), "nothing was undone: {err}");
                }
                Ok(_) => {
                    // A merge git made with it is checked like any other: exactly the two
                    // parents shown.
                    assert_eq!(sh(&f.main, &["rev-parse", "HEAD^2"]), p.branch_tip);
                }
            }
        }

        #[test]
        fn mergeoptions_of_a_base_named_with_an_equals_sign_are_turned_off() {
            let Some(f) = fixture() else { return };
            sh(&f.main, &["branch", "-m", "main", "a=b"]);
            // `-s ours` would make a merge commit that takes nothing from the branch.
            sh(&f.main, &["config", "branch.a=b.mergeOptions", "-s ours"]);
            let info = WorktreeInfo {
                base: Some("a=b".into()),
                ..f.info.clone()
            };
            let p = preflight_with(&info, &env()).unwrap();
            assert!(p.ready(), "{:?}", p.refusals);
            merge_with(&p, &env(), MERGE_TIMEOUT_SECS).unwrap();
            assert!(f.main.join("new.txt").exists(), "the branch's work came in");
            assert_eq!(sh(&f.main, &["rev-parse", "HEAD^2"]), p.branch_tip);
        }

        #[test]
        fn hooks_changed_since_the_preview_refuse_the_merge() {
            let Some(f) = fixture() else { return };
            let p = plan_of(&f);
            hook(&f.main, "post-merge", "true");
            let before = state(&f.main);
            let err = run(&p).unwrap_err();
            assert!(err.contains("merge hooks that would run changed"), "{err}");
            assert_eq!(state(&f.main), before);

            let p = plan_of(&f);
            let elsewhere = f.tmp.path().join("other-hooks");
            fs::create_dir_all(&elsewhere).unwrap();
            sh(
                &f.main,
                &["config", "core.hooksPath", elsewhere.to_str().unwrap()],
            );
            assert!(run(&p)
                .unwrap_err()
                .contains("hooks that would run changed"));
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

        #[test]
        fn base_checked_out_in_linked_worktree_is_found_without_refusal() {
            let Some(f) = fixture() else { return };
            let other_base = f.wt.parent().unwrap().join("main-wt");
            // Move main checkout away from `main` and check `main` out in a linked worktree.
            sh(&f.main, &["checkout", "-q", "-b", "other"]);
            sh(
                &f.main,
                &[
                    "worktree",
                    "add",
                    "--",
                    &other_base.to_string_lossy(),
                    "main",
                ],
            );
            let p = plan_of(&f);
            assert!(p.refusals.is_empty(), "{:?}", p.refusals);
            assert_eq!(
                p.base_dir.canonicalize().unwrap(),
                other_base.canonicalize().unwrap()
            );
            merge_method_with(&p, MergeMethod::Merge, None, &env(), MERGE_TIMEOUT_SECS).unwrap();
            assert!(other_base.join("a.txt").exists());
        }

        #[test]
        fn squash_merges_into_single_parent_commit_with_chosen_message() {
            let Some(f) = fixture() else { return };
            fs::write(f.wt.join("second.txt"), "two\n").unwrap();
            sh(&f.wt, &["add", "second.txt"]);
            sh(&f.wt, &["commit", "-q", "-m", "second change"]);
            let p = plan_of(&f);
            assert!(
                p.squash_message.contains("second change"),
                "{}",
                p.squash_message
            );

            let steps = commands_for(&p, MergeMethod::Squash, Some("feat: combined"));
            assert_eq!(steps.len(), 2);
            assert_eq!(steps[0].args, squash_args(&p.branch_tip));
            assert_eq!(steps[1].args, squash_commit_args("feat: combined"));

            let merged = merge_method_with(
                &p,
                MergeMethod::Squash,
                Some("feat: combined"),
                &env(),
                MERGE_TIMEOUT_SECS,
            )
            .unwrap();
            assert_ne!(merged.before, merged.after);
            let parents = sh(&f.main, &["rev-list", "--parents", "-n", "1", "HEAD"]);
            let parts: Vec<&str> = parents.split_whitespace().collect();
            assert_eq!(parts.len(), 2, "single parent expected: {parents}");
            assert_eq!(parts[1], p.base_tip);
            assert_eq!(sh(&f.main, &["log", "-1", "--format=%s"]), "feat: combined");
            assert!(f.main.join("second.txt").exists());
        }

        #[test]
        fn conflicted_squash_resets_to_exact_prior_state_and_keeps_untracked_files() {
            let Some(f) = fixture() else { return };
            conflict_on_base(&f);
            fs::write(f.main.join("mine.txt"), "untracked\n").unwrap();
            let before = state(&f.main);
            let mut p = plan_of(&f);
            p.refusals.clear();
            let err = execute_squash(&p, "feat: squash", &env(), MERGE_TIMEOUT_SECS).unwrap_err();
            assert!(err.contains("git reset --merge"), "{err}");
            assert_eq!(state(&f.main), before);
            assert!(!f.main.join(".git/SQUASH_MSG").exists());
            assert_eq!(
                fs::read_to_string(f.main.join("mine.txt")).unwrap(),
                "untracked\n"
            );
        }

        #[test]
        fn rebase_replays_commits_and_fast_forwards_base() {
            let Some(f) = fixture() else { return };
            fs::write(f.main.join("base.txt"), "upstream\n").unwrap();
            sh(&f.main, &["add", "base.txt"]);
            sh(&f.main, &["commit", "-q", "-m", "upstream"]);

            let p = plan_of(&f);
            let steps = commands_for(&p, MergeMethod::Rebase, None);
            assert_eq!(steps.len(), 2);
            assert_eq!(steps[0].dir, p.info.path);
            assert_eq!(steps[1].dir, p.base_dir);
            assert!(steps[1].args.contains(&"--ff-only".to_owned()));
            assert!(!steps[1]
                .config
                .iter()
                .any(|(k, v)| k == "merge.ff" && v == "false"));

            let merged =
                merge_method_with(&p, MergeMethod::Rebase, None, &env(), MERGE_TIMEOUT_SECS)
                    .unwrap();
            assert_eq!(head(&f.main), head(&f.wt));
            assert_eq!(short(&head(&f.main)), merged.after);
            let parents = sh(&f.main, &["rev-list", "--parents", "-n", "1", "HEAD"]);
            let parts: Vec<&str> = parents.split_whitespace().collect();
            assert_eq!(
                parts.len(),
                2,
                "fast-forward commit must not be a merge commit"
            );
            assert!(f.main.join("a.txt").exists());
            assert!(f.main.join("base.txt").exists());
        }

        #[test]
        fn conflicted_rebase_aborts_worktree_leaving_base_untouched() {
            let Some(f) = fixture() else { return };
            conflict_on_base(&f);
            let base_before = state(&f.main);
            let wt_before = state(&f.wt);
            let mut p = plan_of(&f);
            p.refusals.clear();
            let err = execute_rebase(&p, &env(), MERGE_TIMEOUT_SECS).unwrap_err();
            assert!(err.contains("rebase --abort"), "{err}");
            assert_eq!(state(&f.main), base_before);
            assert_eq!(state(&f.wt), wt_before);
            assert_eq!(in_progress_op(&f.wt.join(".git")), None);
        }

        #[test]
        fn failed_squash_never_discards_concurrently_staged_user_file() {
            let Some(f) = fixture() else { return };
            let p = plan_of(&f);
            // User creates and stages `new.txt` (touched by `feat`) AFTER preflight:
            // `git merge --squash` refuses without creating SQUASH_MSG, so recovery must leave
            // the staged user file completely intact.
            fs::write(f.main.join("new.txt"), "important\n").unwrap();
            sh(&f.main, &["add", "new.txt"]);
            let err = execute_squash(&p, "feat: squash", &env(), MERGE_TIMEOUT_SECS).unwrap_err();
            assert!(!err.contains("undone with `git reset --merge`"), "{err}");
            assert_eq!(
                fs::read_to_string(f.main.join("new.txt")).unwrap(),
                "important\n"
            );
            let status = sh(&f.main, &["status", "--porcelain"]);
            assert!(status.contains("A  new.txt"), "{status}");
        }

        #[test]
        fn squash_neutralizes_repo_merge_ff_false_and_only_and_rejects_secrets() {
            let Some(f) = fixture() else { return };
            sh(&f.main, &["config", "merge.ff", "false"]);
            let p = plan_of(&f);
            let refusal = merge_method_with(
                &p,
                MergeMethod::Squash,
                Some("token ghp_1234567890abcdefghijklmnopqrstuvwx"),
                &env(),
                MERGE_TIMEOUT_SECS,
            )
            .unwrap_err();
            assert!(refusal.contains("secret or token"));

            merge_method_with(
                &p,
                MergeMethod::Squash,
                Some("feat: works under merge.ff=false"),
                &env(),
                MERGE_TIMEOUT_SECS,
            )
            .unwrap();
            assert_eq!(
                sh(&f.main, &["log", "-1", "--format=%s"]),
                "feat: works under merge.ff=false"
            );
        }

        #[test]
        fn rebase_detects_worktree_hookspath_and_reports_post_merge_timeout_truthfully() {
            let Some(f) = fixture() else { return };
            // Worktree-specific hooksPath overrides repo hooks during rebase.
            let wt_hooks = f.tmp.path().join("wt-hooks");
            fs::create_dir_all(&wt_hooks).unwrap();
            let post_checkout = wt_hooks.join("post-checkout");
            fs::write(&post_checkout, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&post_checkout, fs::Permissions::from_mode(0o755)).unwrap();
            sh(&f.wt, &["config", "extensions.worktreeConfig", "true"]);
            sh(
                &f.wt,
                &[
                    "config",
                    "--worktree",
                    "core.hooksPath",
                    &wt_hooks.to_string_lossy(),
                ],
            );
            let p = plan_of(&f);
            assert!(
                p.rebase_worktree_hooks
                    .contains(&"post-checkout".to_owned()),
                "{:?}",
                p.rebase_worktree_hooks
            );
            assert_eq!(
                p.rebase_worktree_hooks_dir
                    .as_ref()
                    .and_then(|d| d.canonicalize().ok()),
                wt_hooks.canonicalize().ok()
            );

            // A slow post-merge hook in base_dir times out AFTER fast-forward already moved HEAD:
            // error must report that base DID advance, never falsely claim it stayed untouched.
            hook(&f.main, "post-merge", "sleep 5\n");
            let p = plan_of(&f);
            let err = merge_method_with(&p, MergeMethod::Rebase, None, &env(), 1).unwrap_err();
            assert!(!err.contains("stayed untouched"), "{err}");
            assert!(err.contains("DID advance"), "{err}");
            assert_eq!(head(&f.main), head(&f.wt));
        }

        #[test]
        fn failed_commit_hook_never_discards_concurrently_staged_user_edit() {
            let Some(f) = fixture() else { return };
            // Install a pre-commit hook that edits & stages an unrelated file (`b.txt`) AFTER
            // `git merge --squash` has already succeeded and created SQUASH_MSG, then aborts commit.
            let p = plan_of(&f);
            hook(
                &f.main,
                "pre-commit",
                "echo 'concurrent user edit' > b.txt\ngit add b.txt\nexit 1\n",
            );
            let err = execute_squash(&p, "feat: squash", &env(), MERGE_TIMEOUT_SECS).unwrap_err();
            assert!(
                err.contains("nothing was reset automatically"),
                "concurrent staged edit should stop reset --merge: {err}"
            );
            assert_eq!(
                fs::read_to_string(f.main.join("b.txt")).unwrap(),
                "concurrent user edit\n"
            );
            let status = sh(&f.main, &["status", "--porcelain"]);
            assert!(status.contains("M  b.txt"), "{status}");
        }

        #[test]
        fn preflight_branch_merges_after_clean_worktree_removal() {
            let Some(f) = fixture() else { return };
            // Commit on the feature branch, then remove its clean worktree while keeping branch.
            crate::worktree::remove(&f.main, &f.wt).unwrap();
            assert!(!f.wt.exists());
            // Regular preflight requires the worktree, whereas preflight_branch falls back.
            assert!(preflight_with(&f.info, &env()).is_err());
            let p = preflight_branch_with(&f.info, &env()).unwrap();
            assert!(!p.has_worktree);
            assert!(p.refusals.is_empty());
            assert!(p.ahead >= 1);
            // Rebase is refused without a checked-out worktree, whereas Merge succeeds.
            let rebase_err =
                merge_method_with(&p, MergeMethod::Rebase, None, &env(), MERGE_TIMEOUT_SECS)
                    .unwrap_err();
            assert!(rebase_err.contains("no active worktree"), "{rebase_err}");
            let done = merge_method_with(&p, MergeMethod::Merge, None, &env(), MERGE_TIMEOUT_SECS)
                .unwrap();
            assert!(!done.after.is_empty());
            assert!(branch_exists(&f.main, &f.info.branch));
        }
    }
}

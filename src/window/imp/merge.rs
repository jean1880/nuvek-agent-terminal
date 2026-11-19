//! "Merge into …" in a worktree tab's diff panel: the worktree's branch merged back into the
//! branch it started from, in the main checkout, after a dialog that shows what was read and the
//! exact command, in the directory it runs in.
//!
//! The reading and the merge are `agent_kit::merge`, run off the main thread. The window adds
//! what only it knows: the tabs open, and the delegated sub-agents alive, in the base checkout
//! and in this worktree. One in the base checkout that is working (or about to: starting, a
//! prompt or a switch waiting) refuses the merge, since it would rewrite files under a running
//! agent; an idle one is a warning. Anything else open in this worktree refuses it too: it could
//! commit to the branch being merged. The branch and the worktree are never deleted: the success
//! dialog says how to delete the branch, and closing the thread offers the usual clean-only
//! removal of the worktree.

use adw::prelude::*;
use adw::subclass::prelude::*;
use agent_core::commit_message::shown_command;
use agent_core::redact::redact;
use agent_kit::merge::{MergePlan, Merged, Prediction};
use gtk4::glib;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

use super::threads::{off_main, store_job};
use super::{present_message_over, AgentTerminalWindow, CHECKPOINT_QUIET};
use crate::chat::ChatBackend;

/// An open tab, or a live delegated sub-agent, as read on the main thread: where it works and
/// whether it is working. Placed against the plan's folders off the main thread ([`blockers`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Seen {
    pub title: String,
    pub dir: PathBuf,
    /// The worktree its tab was opened in, if any.
    pub worktree: Option<PathBuf>,
    pub busy: bool,
    /// A delegated sub-agent of this very thread: closing it is not the user's to do from here,
    /// so idle in this worktree it is only a warning.
    pub own_child: bool,
}

/// Something in the base checkout or this worktree, as the dialog names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BaseTab {
    pub title: String,
    /// It is working, or about to: a thread's turn, background work, a question, a start, a
    /// prompt or a switch waiting; a terminal (a page or a thread's drawer) that has shown
    /// output just now or runs a job of its own; a sub-agent running.
    pub busy: bool,
    pub own_child: bool,
}

/// Where each [`Seen`] works, against the plan: in the base checkout, in this worktree, or
/// elsewhere (another worktree, nested in the base checkout's folder or not, counts as
/// elsewhere). Paths are compared resolved (links followed), and as given when they cannot be.
pub(super) fn place(plan: &MergePlan, seen: &[Seen]) -> (Vec<BaseTab>, Vec<BaseTab>) {
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let base = canon(&plan.base_dir);
    let here = canon(&plan.info.path);
    let others: Vec<PathBuf> = plan
        .worktrees
        .iter()
        .map(|w| canon(w))
        .filter(|w| *w != here && *w != base)
        .collect();
    let (mut in_base, mut in_here) = (Vec::new(), Vec::new());
    for s in seen {
        let dir = canon(&s.dir);
        let worktree = s.worktree.as_deref().map(canon);
        let tab = BaseTab {
            title: s.title.clone(),
            busy: s.busy,
            own_child: s.own_child,
        };
        if dir.starts_with(&here) || worktree.as_ref() == Some(&here) {
            in_here.push(tab);
        } else if worktree.is_some_and(|w| w != base) || others.iter().any(|o| dir.starts_with(o)) {
            continue;
        } else if dir.starts_with(&base) {
            in_base.push(tab);
        }
    }
    (in_base, in_here)
}

/// What stops the merge and what to know first, from what works in the base checkout and in this
/// worktree, and whether this tab itself is working.
pub(super) fn tab_lines(
    base_dir: &Path,
    in_base: &[BaseTab],
    in_here: &[BaseTab],
    own_busy: bool,
) -> TabLines {
    let dir = base_dir.display();
    let mut lines = TabLines::default();
    if own_busy {
        lines.refusals.push(
            "This thread's agent is working. Wait for its turn to end, or stop it, first: it may \
             be changing the branch."
                .to_owned(),
        );
    }
    for tab in in_here {
        match (tab.own_child, tab.busy) {
            (true, true) => lines.refusals.push(format!(
                "{} of this thread is working in this worktree. Wait for it to finish first: it \
                 could commit to the branch while it is merged.",
                tab.title
            )),
            (true, false) => lines.warnings.push(format!(
                "{} of this thread is idle in this worktree: work given to it while the merge runs \
                 could commit to the branch.",
                tab.title
            )),
            (false, _) => lines.refusals.push(format!(
                "“{}” is also open in this worktree. Close it first: it could commit to the \
                 branch while it is merged.",
                tab.title
            )),
        }
    }
    for tab in in_base {
        if tab.busy {
            lines.refusals.push(format!(
                "“{}” is working in {dir}. Wait for it to finish, or stop it, first: a merge \
                 rewrites the files it is working on.",
                tab.title
            ));
        } else {
            lines.warnings.push(format!(
                "“{}” is open in {dir}: the merge changes the files it sees.",
                tab.title
            ));
        }
    }
    lines
}

/// [`place`], then [`tab_lines`]: everything open that stops the merge or should be known first.
/// Reads the file system: run off the main thread.
pub(super) fn blockers(plan: &MergePlan, seen: &[Seen], own_busy: bool) -> TabLines {
    let (in_base, in_here) = place(plan, seen);
    tab_lines(&plan.base_dir, &in_base, &in_here, own_busy)
}

/// What a press of Merge says instead of merging when something now stops it (a turn started,
/// or a tab opened, while the dialog was up).
pub(super) fn click_refusal(lines: &TabLines) -> Option<String> {
    lines
        .refusals
        .first()
        .map(|why| format!("{why}\n\nNothing was merged."))
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct TabLines {
    pub refusals: Vec<String>,
    pub warnings: Vec<String>,
}

fn commits(n: u32) -> String {
    if n == 1 {
        "1 commit".to_owned()
    } else {
        format!("{n} commits")
    }
}

/// What the dialog says the merge involves: how far each side is ahead, the prediction, the
/// hooks and the base's commit now.
pub(super) fn plan_lines(plan: &MergePlan) -> Vec<String> {
    let (base, branch) = (&plan.base, &plan.branch);
    let mut lines = Vec::new();
    let mut ahead = format!("{branch} has {} that {base} does not.", commits(plan.ahead));
    if plan.behind > 0 {
        ahead.push_str(&format!(
            " {base} has {} that {branch} does not.",
            commits(plan.behind)
        ));
    }
    lines.push(ahead);
    match &plan.prediction {
        Prediction::Clean if plan.ahead > 0 => lines.push("git predicts no conflicts.".to_owned()),
        Prediction::Clean | Prediction::Conflicts(_) => {}
        Prediction::Unknown => lines.push(
            "This git cannot predict conflicts (that needs git 2.38 or newer). A merge that \
             conflicts is undone."
                .to_owned(),
        ),
    }
    match plan.hooks.as_slice() {
        [] => {}
        [one] => lines.push(format!("Your {one} hook runs, as at any git merge.")),
        hooks => lines.push(format!(
            "Your {} hooks run, as at any git merge.",
            hooks.join(", ")
        )),
    }
    lines.push(format!(
        "{base} is at {} now.",
        plan.base_tip.chars().take(12).collect::<String>()
    ));
    lines
}

/// The commands the merge runs, each with the directory it runs in: built from the very list
/// the merge runs.
#[cfg(test)]
pub(super) fn shown_steps(plan: &MergePlan) -> String {
    shown_steps_for(plan, agent_kit::merge::MergeMethod::Merge, None)
}

/// The exact command lines shown in the preview for `method`.
pub(super) fn shown_steps_for(
    plan: &MergePlan,
    method: agent_kit::merge::MergeMethod,
    squash_message: Option<&str>,
) -> String {
    agent_kit::merge::commands_for(plan, method, squash_message)
        .iter()
        .map(agent_kit::merge::GitStep::shown)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The command that deletes the branch, for the user to run once its worktree is removed. The
/// app never runs it.
pub(super) fn delete_branch_command(plan: &MergePlan) -> String {
    shown_command(&[
        "-C".to_owned(),
        plan.base_dir.to_string_lossy().into_owned(),
        "branch".to_owned(),
        "-D".to_owned(),
        "--".to_owned(),
        plan.branch.clone(),
    ])
}

/// The success dialog's words, before and after the delete command (shown on its own, in a
/// label that can be selected and copied).
pub(super) fn merged_words(plan: &MergePlan, done: &Merged) -> (String, String) {
    (
        format!(
            "{base} moved from {before} to {after} in {dir}.\n\nThe branch {branch} is kept: the \
             app never deletes a branch. Once its worktree is removed, this deletes it:",
            base = plan.base,
            before = done.before,
            after = if done.after.is_empty() {
                "the merge"
            } else {
                &done.after
            },
            dir = plan.base_dir.display(),
            branch = plan.branch,
        ),
        "Closing the thread offers to remove the worktree when nothing in it is uncommitted."
            .to_owned(),
    )
}

/// The method and optional commit message chosen in [`merge_dialog_with`].
struct MergeChoice {
    method: agent_kit::merge::MergeMethod,
    squash_message: Option<String>,
}

/// Options configuring extra follow-up actions in the merge preview dialog.
#[derive(Debug, Clone, Copy)]
pub(super) struct MergeDialogOptions {
    pub initial: agent_kit::merge::MergeMethod,
    pub can_ask_agent: bool,
    pub can_commit_first: bool,
}

/// Builds the conflict-resolution prompt sent to the worktree thread's agent.
pub(super) fn conflict_handoff_prompt(plan: &MergePlan) -> String {
    let files: Vec<String> = plan
        .refusals
        .iter()
        .find_map(|r| match r {
            agent_kit::merge::Refusal::Conflicts(files) => Some(files.clone()),
            _ => None,
        })
        .unwrap_or_default();
    let list = if files.is_empty() {
        "the conflicting files".to_owned()
    } else {
        files
            .iter()
            .take(12)
            .map(|f| redact(f))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let merge_cmd = shown_command(&["merge".to_owned(), format!("refs/heads/{}", plan.base)]);
    redact(&format!(
        "Merging `{}` into `{}` would conflict in: {list}.\n\n\
         Please run `{merge_cmd}` in this worktree, resolve any conflicts, and commit so `{}` \
         can merge cleanly into `{}`.",
        plan.branch, plan.base, plan.branch, plan.base
    ))
}

/// The merge dialog's parts the flow and its test read back.
pub(super) struct MergeDialog {
    pub dialog: adw::AlertDialog,
    pub method_row: adw::ComboRow,
    pub squash_buffer: gtk4::TextBuffer,
    pub methods: Vec<agent_kit::merge::MergeMethod>,
    #[cfg_attr(not(test), expect(dead_code, reason = "read by the dialog test"))]
    pub commands: gtk4::Label,
    #[cfg_attr(not(test), expect(dead_code, reason = "read by the dialog test"))]
    pub refusals: Vec<String>,
}

fn method_note(method: agent_kit::merge::MergeMethod, branch: &str) -> String {
    match method {
        agent_kit::merge::MergeMethod::Merge => format!(
            "The merge is never forced and skips no hook, and it takes exactly the commit shown. If \
             it stops part-way with a merge in progress (a conflict, say) it is undone with git \
             merge --abort; anything else git leaves is left as it is and said, never undone for \
             you. The branch {branch} and its worktree are kept."
        ),
        agent_kit::merge::MergeMethod::Squash => format!(
            "The squash commits the combined diff onto the base branch with no second parent and \
             skips no hook. If it stops part-way it is undone with git reset --merge. The branch \
             {branch} and its worktree are kept."
        ),
        agent_kit::merge::MergeMethod::Rebase => format!(
            "First rebases {branch} onto the base commit in this worktree (git rebase --abort \
             undoes a conflict), then fast-forwards the base checkout. The branch {branch} and its \
             worktree are kept."
        ),
    }
}

/// Builds the dialog for `plan`, with what the tabs in the base checkout add. Merge is offered
/// only when nothing refuses it; Cancel is the default and what a press outside answers.
#[cfg(test)]
pub(super) fn merge_dialog(plan: &MergePlan, tabs: &TabLines) -> MergeDialog {
    merge_dialog_with(plan, tabs, agent_kit::merge::MergeMethod::Merge)
}

/// [`merge_dialog`] starting on `initial`.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "test helper wrapping merge_dialog_full")
)]
pub(super) fn merge_dialog_with(
    plan: &MergePlan,
    tabs: &TabLines,
    initial: agent_kit::merge::MergeMethod,
) -> MergeDialog {
    merge_dialog_full(
        plan,
        tabs,
        MergeDialogOptions {
            initial,
            can_ask_agent: false,
            can_commit_first: false,
        },
    )
}

/// [`merge_dialog`] with full follow-up action support (`Ask Agent` on predicted conflicts,
/// `Commit First…` on dirty worktree, and branch-only method filtering when `has_worktree` is false).
pub(super) fn merge_dialog_full(
    plan: &MergePlan,
    tabs: &TabLines,
    options: MergeDialogOptions,
) -> MergeDialog {
    use agent_kit::merge::{MergeMethod, Refusal};
    let initial = options.initial;
    let dialog = adw::AlertDialog::new(
        Some(&format!("Merge {} into {}?", plan.branch, plan.base)),
        None,
    );
    let column = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    let refusals: Vec<String> = plan
        .refusals
        .iter()
        .map(|r| redact(&r.reason(plan)))
        .chain(tabs.refusals.iter().cloned())
        .collect();
    for line in &refusals {
        let label = wrapped(line);
        label.add_css_class("error");
        column.append(&label);
    }
    for line in &tabs.warnings {
        let label = wrapped(line);
        label.add_css_class("warning");
        column.append(&label);
    }
    for line in plan_lines(plan) {
        let label = wrapped(&line);
        label.add_css_class("dim-label");
        column.append(&label);
    }

    let methods: Vec<MergeMethod> = if plan.has_worktree {
        MergeMethod::ALL.to_vec()
    } else {
        vec![MergeMethod::Merge, MergeMethod::Squash]
    };
    let labels: Vec<&str> = methods.iter().map(|m| m.label()).collect();
    let model = gtk4::StringList::new(&labels);
    let initial_idx = methods.iter().position(|m| *m == initial).unwrap_or(0);
    let active_initial = methods[initial_idx];
    let method_row = adw::ComboRow::builder()
        .title("Method")
        .model(&model)
        .selected(initial_idx as u32)
        .build();
    let method_list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    method_list.append(&method_row);
    column.append(&method_list);

    let squash_box = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    let squash_label = wrapped("Commit message");
    squash_label.add_css_class("heading");
    squash_box.append(&squash_label);
    let squash_buffer = gtk4::TextBuffer::new(None);
    squash_buffer.set_text(&plan.squash_message);
    let squash_view = gtk4::TextView::builder()
        .buffer(&squash_buffer)
        .wrap_mode(gtk4::WrapMode::WordChar)
        .accepts_tab(false)
        .top_margin(6)
        .bottom_margin(6)
        .left_margin(8)
        .right_margin(8)
        .build();
    squash_view.update_property(&[gtk4::accessible::Property::Label("Squash commit message")]);
    let squash_scroll = gtk4::ScrolledWindow::builder()
        .child(&squash_view)
        .min_content_height(72)
        .max_content_height(140)
        .has_frame(true)
        .build();
    squash_box.append(&squash_scroll);
    let secret_warning = wrapped("Commit message looks like it contains a secret or token.");
    secret_warning.add_css_class("error");
    let initial_check = agent_core::commit_message::check(&plan.squash_message);
    secret_warning.set_visible(active_initial == MergeMethod::Squash && initial_check.looks_secret);
    squash_box.append(&secret_warning);
    squash_box.set_visible(active_initial == MergeMethod::Squash);
    column.append(&squash_box);

    let method_hooks_label = wrapped("");
    method_hooks_label.add_css_class("dim-label");
    let method_hooks_text = |method: MergeMethod| -> String {
        let hooks: Vec<&str> = match method {
            MergeMethod::Merge => Vec::new(),
            MergeMethod::Squash => plan.squash_hooks.iter().map(String::as_str).collect(),
            MergeMethod::Rebase => plan
                .rebase_worktree_hooks
                .iter()
                .chain(plan.rebase_base_hooks.iter())
                .map(String::as_str)
                .collect(),
        };
        match hooks.as_slice() {
            [] => String::new(),
            [one] => format!("Your {one} hook also runs."),
            many => format!("Your {} hooks also run.", many.join(", ")),
        }
    };
    let initial_hooks = method_hooks_text(active_initial);
    method_hooks_label.set_text(&initial_hooks);
    method_hooks_label.set_visible(!initial_hooks.is_empty());
    column.append(&method_hooks_label);

    let heading = wrapped("Runs, in this order:");
    heading.add_css_class("heading");
    column.append(&heading);
    let initial_msg =
        (active_initial == MergeMethod::Squash).then_some(plan.squash_message.as_str());
    let commands = wrapped(&shown_steps_for(plan, active_initial, initial_msg));
    commands.add_css_class("monospace");
    commands.set_selectable(true);
    column.append(&commands);
    let note = wrapped(&method_note(active_initial, &plan.branch));
    note.add_css_class("dim-label");
    column.append(&note);
    dialog.set_extra_child(Some(&column));

    dialog.add_response("cancel", "Cancel");
    let has_conflicts = plan
        .refusals
        .iter()
        .any(|r| matches!(r, Refusal::Conflicts(_)));
    let has_dirty_worktree = plan
        .refusals
        .iter()
        .any(|r| matches!(r, Refusal::WorktreeChanges));
    let has_other_plan_refusal = plan
        .refusals
        .iter()
        .any(|r| !matches!(r, Refusal::Conflicts(_)));
    if options.can_commit_first && has_dirty_worktree {
        dialog.add_response("commit-first", "Commit First…");
    }
    if options.can_ask_agent
        && has_conflicts
        && !has_dirty_worktree
        && !has_other_plan_refusal
        && tabs.refusals.is_empty()
    {
        dialog.add_response("ask-agent", &format!("Ask Agent to Bring in {}", plan.base));
    }
    dialog.add_response("merge", "Merge");
    dialog.set_response_appearance("merge", adw::ResponseAppearance::Suggested);
    let can_merge = refusals.is_empty()
        && (active_initial != MergeMethod::Squash
            || (!initial_check.empty && !initial_check.looks_secret));
    dialog.set_response_enabled("merge", can_merge);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    let has_refusals = !refusals.is_empty();
    let sync_methods = methods.clone();
    let sync = {
        let plan = plan.clone();
        let dialog = dialog.downgrade();
        let method_row = method_row.downgrade();
        let squash_box = squash_box.downgrade();
        let squash_buffer = squash_buffer.downgrade();
        let secret_warning = secret_warning.downgrade();
        let method_hooks_label = method_hooks_label.downgrade();
        let commands = commands.downgrade();
        let note = note.downgrade();
        std::rc::Rc::new(move || {
            let (
                Some(dialog),
                Some(method_row),
                Some(squash_box),
                Some(squash_buffer),
                Some(secret_warning),
                Some(method_hooks_label),
                Some(commands),
                Some(note),
            ) = (
                dialog.upgrade(),
                method_row.upgrade(),
                squash_box.upgrade(),
                squash_buffer.upgrade(),
                secret_warning.upgrade(),
                method_hooks_label.upgrade(),
                commands.upgrade(),
                note.upgrade(),
            )
            else {
                return;
            };
            let method = sync_methods
                .get(method_row.selected() as usize)
                .copied()
                .unwrap_or_default();
            squash_box.set_visible(method == MergeMethod::Squash);
            let (start, end) = squash_buffer.bounds();
            let msg = squash_buffer.text(&start, &end, false).to_string();
            let check = agent_core::commit_message::check(&msg);
            secret_warning.set_visible(method == MergeMethod::Squash && check.looks_secret);
            let hook_list: Vec<&str> = match method {
                MergeMethod::Merge => Vec::new(),
                MergeMethod::Squash => plan.squash_hooks.iter().map(String::as_str).collect(),
                MergeMethod::Rebase => plan
                    .rebase_worktree_hooks
                    .iter()
                    .chain(plan.rebase_base_hooks.iter())
                    .map(String::as_str)
                    .collect(),
            };
            let hook_text = match hook_list.as_slice() {
                [] => String::new(),
                [one] => format!("Your {one} hook also runs."),
                many => format!("Your {} hooks also run.", many.join(", ")),
            };
            method_hooks_label.set_text(&hook_text);
            method_hooks_label.set_visible(!hook_text.is_empty());
            commands.set_text(&shown_steps_for(
                &plan,
                method,
                (method == MergeMethod::Squash).then_some(msg.as_str()),
            ));
            note.set_text(&method_note(method, &plan.branch));
            let label = match method {
                MergeMethod::Merge => "Merge",
                MergeMethod::Squash => "Squash and Commit",
                MergeMethod::Rebase => "Rebase and Merge",
            };
            dialog.set_response_label("merge", label);
            let message_ok = method != MergeMethod::Squash || (!check.empty && !check.looks_secret);
            dialog.set_response_enabled("merge", !has_refusals && message_ok);
        })
    };
    let sync_method = sync.clone();
    method_row.connect_selected_notify(move |_| sync_method());
    squash_buffer.connect_changed(move |_| sync());
    if active_initial != MergeMethod::Merge {
        let label = match active_initial {
            MergeMethod::Merge => "Merge",
            MergeMethod::Squash => "Squash and Commit",
            MergeMethod::Rebase => "Rebase and Merge",
        };
        dialog.set_response_label("merge", label);
    }

    MergeDialog {
        dialog,
        method_row,
        squash_buffer,
        methods,
        commands,
        refusals,
    }
}

fn wrapped(text: &str) -> gtk4::Label {
    gtk4::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk4::pango::WrapMode::WordChar)
        .build()
}

/// Whether `tab`'s terminal (a terminal page, or a thread's drawer) is at work: it changed on
/// screen just now, or a job other than its shell holds the terminal. A quiet build, a `sleep`
/// before a commit or an editor shows nothing for a while, but owns the foreground.
fn terminal_busy(tab: &super::TabState) -> bool {
    let recent = tab
        .terminal_output
        .get()
        .is_some_and(|at| at.elapsed() < CHECKPOINT_QUIET);
    recent || super::terminal_runs_a_job(&tab.terminal)
}

/// Whether the terminal `fd` is the controlling terminal of has a foreground process group other
/// than its session leader's (the shell VTE started): a job the shell is running. Two `ioctl`s,
/// which never block. Unknown (either fails) counts as no job.
#[cfg_attr(
    not(feature = "terminal"),
    expect(dead_code, reason = "a build without terminals has no pty to ask")
)]
pub(super) fn foreground_job(fd: &impl std::os::fd::AsRawFd) -> bool {
    let fd = fd.as_raw_fd();
    // SAFETY: both only read the terminal's process group and session; `fd` is open for the
    // duration of the call (borrowed from its owner).
    let (group, session) = unsafe { (libc::tcgetpgrp(fd), libc::tcgetsid(fd)) };
    group > 0 && session > 0 && group != session
}

/// Target whose branch is being merged into its base checkout.
enum MergeTarget {
    WorktreeTab {
        panel_root: gtk4::Box,
        page: adw::TabPage,
        panel: Box<crate::window::diff_panel::DiffPanel>,
        info: agent_kit::worktree::WorktreeInfo,
    },
    ThreadMenu {
        thread: String,
        info: agent_kit::worktree::WorktreeInfo,
    },
    WorkerBranch {
        parent_thread: String,
        lease: crate::delegation::WorktreeLease,
    },
}

#[derive(Clone)]
enum PreflightSpec {
    Worktree(agent_kit::worktree::WorktreeInfo),
    Branch(agent_kit::worktree::WorktreeInfo),
    Lease(crate::delegation::WorktreeLease),
}

impl MergeTarget {
    fn spec(&self) -> PreflightSpec {
        match self {
            Self::WorktreeTab { info, .. } => PreflightSpec::Worktree(info.clone()),
            Self::ThreadMenu { info, .. } => PreflightSpec::Branch(info.clone()),
            Self::WorkerBranch { lease, .. } => PreflightSpec::Lease(lease.clone()),
        }
    }
}

impl PreflightSpec {
    fn preflight(&self) -> Result<MergePlan, String> {
        match self {
            Self::Worktree(info) => agent_kit::merge::preflight(info),
            Self::Branch(info) => agent_kit::merge::preflight_branch(
                &info.main_toplevel,
                &info.branch,
                info.base.as_deref(),
                Some(&info.path),
            ),
            Self::Lease(lease) => agent_kit::merge::preflight_branch(
                &lease.source_repo,
                &lease.branch,
                None,
                Some(&lease.worktree),
            ),
        }
    }
}

impl AgentTerminalWindow {
    /// Every open tab other than the one whose panel is `own` (in every window) and every live
    /// delegated sub-agent, with where it works and whether it is working; and whether the
    /// tab whose panel is `own` is working itself. Read on the main thread, placed off it.
    fn seen_for_merge(
        &self,
        own: Option<&gtk4::Box>,
        own_thread_arg: Option<&str>,
    ) -> (Vec<Seen>, bool) {
        let found = std::cell::RefCell::new(Vec::new());
        let own_busy = std::cell::Cell::new(false);
        let own_thread = std::cell::RefCell::new(own_thread_arg.map(str::to_owned));
        self.for_each_window(|window| {
            for tab in window.tabs.borrow().iter() {
                if tab.remote.is_some() {
                    continue;
                }
                // A thread is busy while it holds its work; a terminal (a page, or a thread's
                // drawer) while it has just shown output or runs a job of its own.
                let busy = terminal_busy(tab) || tab.chat.as_ref().is_some_and(|c| c.holds_merge());
                let is_own_panel = own.is_some_and(|p| tab.diff_panel.root == *p);
                let is_own_thread = own_thread_arg
                    .is_some_and(|tid| tab.chat.as_ref().is_some_and(|c| c.thread == tid));
                if is_own_panel || is_own_thread {
                    own_busy.set(busy);
                    if own_thread.borrow().is_none() {
                        *own_thread.borrow_mut() = tab.chat.as_ref().map(|c| c.thread.clone());
                    }
                    continue;
                }
                found.borrow_mut().push(Seen {
                    title: tab.page.title().to_string(),
                    dir: PathBuf::from(&tab.dir),
                    worktree: tab.worktree.as_ref().map(|w| w.path.clone()),
                    busy,
                    own_child: false,
                });
            }
        });
        let mut found = found.into_inner();
        if let Some(supervisor) = crate::delegation::supervisor() {
            let own_thread = own_thread.into_inner();
            for (name, cwd, working, own_child) in supervisor.live_workers(own_thread.as_deref()) {
                if cwd.as_os_str().is_empty() {
                    continue;
                }
                found.push(Seen {
                    title: format!("Sub-agent {name}"),
                    dir: cwd,
                    worktree: None,
                    busy: working,
                    own_child,
                });
            }
        }
        (found, own_busy.get())
    }

    /// Where the outcome of a merge from the panel at `panel_root` is shown: over the panel while
    /// its tab is open, else over this window.
    fn merge_result_parent(
        &self,
        panel_root: &gtk4::Box,
        panel_parent: &gtk4::Widget,
    ) -> gtk4::Widget {
        if self.tab_by_panel(panel_root, |_| ()).is_some() && panel_parent.root().is_some() {
            panel_parent.clone()
        } else {
            self.obj().clone().upcast()
        }
    }

    /// "Merge into …" in the diff panel behind `panel_root`: reads the plan and what is open
    /// off the main thread, then shows it with the exact command, unless the tab closed
    /// meanwhile. The panel's button stays off until the dialog is up or the reason it is not
    /// has been said.
    pub(super) fn merge_from_panel(&self, panel_root: &gtk4::Box) {
        let Some((page, panel, worktree)) = self.tab_by_panel(panel_root, |t| {
            (t.page.clone(), t.diff_panel.clone(), t.worktree.clone())
        }) else {
            return;
        };
        let Some(info) = worktree else {
            panel.set_merge_preparing(false);
            return;
        };
        self.present_merge_flow(MergeTarget::WorktreeTab {
            panel_root: panel_root.clone(),
            page,
            panel: Box::new(panel),
            info,
        });
    }

    /// "Merge into <base>…" from a sidebar thread row's context menu.
    pub(super) fn merge_worktree_thread(&self, thread: &str) {
        let obj = self.obj().downgrade();
        let thread = thread.to_owned();
        glib::MainContext::default().spawn_local(async move {
            let info = store_job({
                let thread = thread.clone();
                move |store| {
                    store
                        .thread_attributes(&thread)
                        .ok()
                        .and_then(|a| a.worktree)
                }
            })
            .await
            .unwrap_or(None);
            let Some(obj) = obj.upgrade() else { return };
            let imp = obj.imp();
            let Some(info) = info else {
                let parent: gtk4::Widget = obj.clone().upcast();
                present_message_over(
                    &parent,
                    "Can't Merge Worktree",
                    "This thread is not attached to a worktree.",
                );
                return;
            };
            imp.present_merge_flow(MergeTarget::ThreadMenu { thread, info });
        });
    }

    /// "Merge Branch…" from a delegated worker inspector card (`isolated_worktree`).
    pub(super) fn merge_worker_branch(&self, parent_thread: String, worker_id: String) {
        let parent_widget: gtk4::Widget = self.obj().clone().upcast();
        let wid = match agent_core::delegation::WorkerId::parse(&worker_id) {
            Ok(id) => id,
            Err(why) => {
                present_message_over(
                    &parent_widget,
                    "Can't Merge Worker Branch",
                    &redact(&why.to_string()),
                );
                return;
            }
        };
        let Some(lease) = crate::delegation::supervisor().and_then(|s| s.child_lease(&wid)) else {
            present_message_over(
                &parent_widget,
                "Can't Merge Worker Branch",
                "This sub-agent does not have an isolated worktree branch.",
            );
            return;
        };
        self.present_merge_flow(MergeTarget::WorkerBranch {
            parent_thread,
            lease,
        });
    }

    fn present_merge_flow(&self, target: MergeTarget) {
        let obj = self.obj().downgrade();
        glib::MainContext::default().spawn_local(async move {
            let done_preparing = |target: &MergeTarget| {
                if let MergeTarget::WorktreeTab { panel, .. } = target {
                    panel.set_merge_preparing(false);
                }
            };
            let read = {
                let spec = target.spec();
                off_main("reading the merge", move || spec.preflight()).await
            };
            let Some(obj) = obj.upgrade() else {
                done_preparing(&target);
                return;
            };
            let imp = obj.imp();
            let still_open = match &target {
                MergeTarget::WorktreeTab { panel_root, .. } => {
                    imp.tab_by_panel(panel_root, |_| ()).is_some()
                }
                MergeTarget::ThreadMenu { thread, .. } => imp.summary_of(thread).is_some(),
                MergeTarget::WorkerBranch { .. } => true,
            };
            if !still_open {
                done_preparing(&target);
                return;
            }
            let parent_widget = |target: &MergeTarget| -> gtk4::Widget {
                match target {
                    MergeTarget::WorktreeTab {
                        panel_root, panel, ..
                    } => imp.merge_result_parent(panel_root, &panel.dialog_parent()),
                    MergeTarget::ThreadMenu { .. } | MergeTarget::WorkerBranch { .. } => {
                        obj.clone().upcast()
                    }
                }
            };
            let err_title = match &target {
                MergeTarget::WorkerBranch { .. } => "Can't Merge Worker Branch",
                MergeTarget::WorktreeTab { .. } | MergeTarget::ThreadMenu { .. } => {
                    "Can't Merge This Worktree"
                }
            };
            let plan = match read {
                Ok(plan) => plan,
                Err(why) => {
                    done_preparing(&target);
                    present_message_over(&parent_widget(&target), err_title, &redact(&why));
                    return;
                }
            };
            let (seen, own_busy) = match &target {
                MergeTarget::WorktreeTab { panel_root, .. } => {
                    imp.seen_for_merge(Some(panel_root), None)
                }
                MergeTarget::ThreadMenu { thread, .. } => imp.seen_for_merge(None, Some(thread)),
                MergeTarget::WorkerBranch { .. } => imp.seen_for_merge(None, None),
            };
            let lines = {
                let plan = plan.clone();
                off_main("reading what is open", move || {
                    Ok::<_, String>(blockers(&plan, &seen, own_busy))
                })
                .await
            };
            let Ok(lines) = lines else {
                done_preparing(&target);
                return;
            };
            let still_open = match &target {
                MergeTarget::WorktreeTab { panel_root, .. } => {
                    imp.tab_by_panel(panel_root, |_| ()).is_some()
                }
                MergeTarget::ThreadMenu { thread, .. } => imp.summary_of(thread).is_some(),
                MergeTarget::WorkerBranch { .. } => true,
            };
            if !still_open {
                done_preparing(&target);
                return;
            }
            let parent = parent_widget(&target);
            let initial = imp.config.borrow().default_merge_method;
            let thread_for_actions = match &target {
                MergeTarget::WorktreeTab { panel_root, .. } => imp
                    .tab_by_panel(panel_root, |t| t.chat.as_ref().map(|c| c.thread.clone()))
                    .flatten(),
                MergeTarget::ThreadMenu { thread, .. } => Some(thread.clone()),
                MergeTarget::WorkerBranch { .. } => None,
            };
            let options = MergeDialogOptions {
                initial,
                can_ask_agent: thread_for_actions.is_some(),
                can_commit_first: !matches!(target, MergeTarget::WorkerBranch { .. }),
            };
            let parts = merge_dialog_full(&plan, &lines, options);
            parts.dialog.present(Some(&parent));
            done_preparing(&target);

            let obj = obj.downgrade();
            let method_row = parts.method_row;
            let squash_buffer = parts.squash_buffer;
            let methods = parts.methods;
            let target_ref = std::rc::Rc::new(target);
            parts.dialog.connect_response(None, move |_, response| {
                let Some(obj) = obj.upgrade() else { return };
                match response {
                    "merge" => {
                        let method = methods
                            .get(method_row.selected() as usize)
                            .copied()
                            .unwrap_or_default();
                        let msg = if method == agent_kit::merge::MergeMethod::Squash {
                            let (start, end) = squash_buffer.bounds();
                            Some(squash_buffer.text(&start, &end, false).to_string())
                        } else {
                            None
                        };
                        obj.imp().run_merge(
                            &target_ref,
                            &parent,
                            plan.clone(),
                            MergeChoice {
                                method,
                                squash_message: msg,
                            },
                        );
                    }
                    "ask-agent" => {
                        if let Some(ref thread_id) = thread_for_actions {
                            obj.imp().ask_agent_to_resolve_conflict(
                                &target_ref,
                                thread_id,
                                plan.clone(),
                                &parent,
                            );
                        }
                    }
                    "commit-first" => {
                        obj.imp()
                            .open_worktree_for_commit(&target_ref, thread_for_actions.as_deref());
                    }
                    _ => {}
                }
            });
        });
    }

    fn ask_agent_to_resolve_conflict(
        &self,
        target: &MergeTarget,
        thread_id: &str,
        plan: MergePlan,
        parent: &gtk4::Widget,
    ) {
        let (seen, own_busy) = match target {
            MergeTarget::WorktreeTab { panel_root, .. } => {
                self.seen_for_merge(Some(panel_root), None)
            }
            MergeTarget::ThreadMenu { thread, .. } => self.seen_for_merge(None, Some(thread)),
            MergeTarget::WorkerBranch { .. } => return,
        };
        let spec = target.spec();
        let panel_root = match target {
            MergeTarget::WorktreeTab { panel_root, .. } => Some(panel_root.clone()),
            _ => None,
        };
        let obj = self.obj().downgrade();
        let thread_id = thread_id.to_owned();
        let parent = parent.clone();
        glib::MainContext::default().spawn_local(async move {
            let recheck = {
                let plan = plan.clone();
                off_main("rechecking worktree before asking agent", move || {
                    let now = spec.preflight()?;
                    let lines = blockers(&now, &seen, own_busy);
                    if now.info.path != plan.info.path
                        || now.has_worktree != plan.has_worktree
                        || now.branch != plan.branch
                        || now.base != plan.base
                        || now.branch_tip != plan.branch_tip
                        || now.base_tip != plan.base_tip
                    {
                        return Err(redact(&format!(
                            "{} or {} changed since the preview. Open the merge dialog again \
                             before asking the agent.",
                            plan.branch, plan.base
                        )));
                    }
                    if let Some(why) = lines.refusals.first() {
                        return Err(why.clone());
                    }
                    if let Some(other) = now
                        .refusals
                        .iter()
                        .find(|r| !matches!(r, agent_kit::merge::Refusal::Conflicts(_)))
                    {
                        return Err(redact(&other.reason(&now)));
                    }
                    if !now
                        .refusals
                        .iter()
                        .any(|r| matches!(r, agent_kit::merge::Refusal::Conflicts(_)))
                    {
                        return Err(
                            "No conflicts remain to resolve. Open the merge dialog again."
                                .to_owned(),
                        );
                    }
                    Ok(conflict_handoff_prompt(&now))
                })
                .await
            };
            let Some(obj) = obj.upgrade() else { return };
            let imp = obj.imp();
            let shown_over = match panel_root.as_ref() {
                Some(root) => imp.merge_result_parent(root, &parent),
                None => parent.clone(),
            };
            let prompt = match recheck {
                Ok(p) => p,
                Err(why) => {
                    present_message_over(&shown_over, "Can't Ask Agent Right Now", &why);
                    return;
                }
            };
            let Some(page) = imp.open_thread(&thread_id, true) else {
                return;
            };
            let owner = super::threads::thread_owner(&thread_id).unwrap_or_else(|| obj.clone());
            if let Some(view) = owner
                .imp()
                .with_page_tab(&page, |t| t.chat.as_ref().and_then(|c| c.view.clone()))
                .flatten()
            {
                if let Err(why) = view.submit_review(&prompt) {
                    present_message_over(&shown_over, "Could Not Ask Agent", &why);
                }
                return;
            }
            // Opening a previously closed thread builds its view and session asynchronously.
            let owner_w = owner.downgrade();
            for _ in 0..80 {
                glib::timeout_future(std::time::Duration::from_millis(25)).await;
                let Some(owner) = owner_w.upgrade() else {
                    return;
                };
                let ready = owner
                    .imp()
                    .with_page_tab(&page, |t| {
                        t.chat.as_ref().and_then(|c| {
                            let view = c.view.clone()?;
                            let ready = c.slot.get().is_some_and(|s| s.send_refusal().is_none());
                            Some((view, ready))
                        })
                    })
                    .flatten();
                if let Some((view, true)) = ready {
                    if let Err(why) = view.submit_review(&prompt) {
                        present_message_over(&shown_over, "Could Not Ask Agent", &why);
                    }
                    return;
                }
            }
            if let Some(owner) = owner_w.upgrade() {
                if let Some(view) = owner
                    .imp()
                    .with_page_tab(&page, |t| t.chat.as_ref().and_then(|c| c.view.clone()))
                    .flatten()
                {
                    if let Err(why) = view.submit_review(&prompt) {
                        present_message_over(&shown_over, "Could Not Ask Agent", &why);
                    }
                }
            }
        });
    }

    fn open_worktree_for_commit(&self, target: &MergeTarget, thread_id: Option<&str>) {
        let (owner, page, panel) = match target {
            MergeTarget::WorktreeTab { page, panel, .. } => {
                (self.obj().clone(), page.clone(), (**panel).clone())
            }
            MergeTarget::ThreadMenu { .. } => {
                let Some(tid) = thread_id else { return };
                let Some(page) = self.open_thread(tid, true) else {
                    return;
                };
                let owner = super::threads::thread_owner(tid).unwrap_or_else(|| self.obj().clone());
                let Some(panel) = owner.imp().with_page_tab(&page, |t| t.diff_panel.clone()) else {
                    return;
                };
                (owner, page, panel)
            }
            MergeTarget::WorkerBranch { .. } => return,
        };
        panel.select_uncommitted();
        if panel.is_detached() {
            panel.present();
        } else if !panel.root.is_visible() {
            panel.set_shown(true);
            owner.imp().config.borrow_mut().diff_panel_visible = true;
            owner.imp().schedule_config_save();
        }
        owner.imp().refresh_diff(&page);
    }

    /// Runs `choice` on `plan` once what is open is checked again: a turn may have
    /// started, or a tab opened, while the dialog was up.
    fn run_merge(
        &self,
        target: &MergeTarget,
        parent: &gtk4::Widget,
        plan: MergePlan,
        choice: MergeChoice,
    ) {
        let MergeChoice {
            method,
            squash_message,
        } = choice;
        let (seen, own_busy) = match target {
            MergeTarget::WorktreeTab { panel_root, .. } => {
                self.seen_for_merge(Some(panel_root), None)
            }
            MergeTarget::ThreadMenu { thread, .. } => self.seen_for_merge(None, Some(thread)),
            MergeTarget::WorkerBranch { .. } => self.seen_for_merge(None, None),
        };
        enum After {
            WorktreeTab {
                panel_root: gtk4::Box,
                page: adw::TabPage,
                panel: Box<crate::window::diff_panel::DiffPanel>,
            },
            Thread {
                thread: String,
                offer_close: bool,
            },
        }
        let after = match target {
            MergeTarget::WorktreeTab {
                panel_root,
                page,
                panel,
                ..
            } => After::WorktreeTab {
                panel_root: panel_root.clone(),
                page: page.clone(),
                panel: panel.clone(),
            },
            MergeTarget::ThreadMenu { thread, .. } => After::Thread {
                thread: thread.clone(),
                offer_close: true,
            },
            MergeTarget::WorkerBranch { parent_thread, .. } => After::Thread {
                thread: parent_thread.clone(),
                offer_close: false,
            },
        };
        let parent = parent.clone();
        let obj = self.obj().downgrade();
        glib::MainContext::default().spawn_local(async move {
            if let After::WorktreeTab { panel, .. } = &after {
                panel.set_merging(true);
            }
            let done = {
                let plan = plan.clone();
                off_main("the merge", move || {
                    // Refused before git runs at all: something started while the dialog was up.
                    let refused = click_refusal(&blockers(&plan, &seen, own_busy));
                    Ok(match refused {
                        Some(why) => Err(Err(why)),
                        None => agent_kit::merge::merge_with_method(
                            &plan,
                            method,
                            squash_message.as_deref(),
                        )
                        .map_err(Ok),
                    })
                })
                .await
            };
            if let After::WorktreeTab { panel, .. } = &after {
                panel.set_merging(false);
            }
            let Some(obj) = obj.upgrade() else { return };
            let imp = obj.imp();
            let shown_over = match &after {
                After::WorktreeTab { panel_root, .. } => {
                    imp.merge_result_parent(panel_root, &parent)
                }
                After::Thread { .. } => parent.clone(),
            };
            // `Err(Err(_))`: refused at the click; `Err(Ok(_))`: git's failure.
            let done: Result<Merged, Result<String, String>> =
                done.unwrap_or_else(|why| Err(Ok(why)));
            match done {
                Ok(done) => {
                    info!("merged a worktree's branch into its base");
                    match &after {
                        After::WorktreeTab { page, .. } => {
                            imp.merged(Some(page), &shown_over, &plan, &done);
                        }
                        After::Thread {
                            thread,
                            offer_close,
                        } => {
                            let page = offer_close.then(|| imp.page_of_thread(thread)).flatten();
                            imp.merged(page.as_ref(), &shown_over, &plan, &done);
                        }
                    }
                }
                Err(Err(why)) => {
                    present_message_over(&shown_over, "Can't Merge Right Now", &why);
                }
                Err(Ok(why)) => {
                    warn!("a merge from a worktree did not go through");
                    present_message_over(&shown_over, "Could Not Merge", &why);
                }
            }
            match &after {
                After::WorktreeTab { page, .. } => imp.refresh_diff(page),
                After::Thread { thread, .. } => {
                    if let Some(page) = imp.page_of_thread(thread) {
                        imp.refresh_diff(&page);
                    }
                }
            }
        });
    }

    /// Says the merge is made, how to delete the branch, and optionally offers to close `page`.
    fn merged(
        &self,
        page: Option<&adw::TabPage>,
        parent: &gtk4::Widget,
        plan: &MergePlan,
        done: &Merged,
    ) {
        let dialog = merged_dialog_with(plan, done, page.is_some());
        dialog.present(Some(parent));
        let Some(page) = page else { return };
        let obj = self.obj().downgrade();
        let page = page.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "close" {
                return;
            }
            let Some(obj) = obj.upgrade() else { return };
            let imp = obj.imp();
            // Only a page still in this window: it may have been dragged to another meanwhile.
            if imp.with_page_tab(&page, |_| ()).is_none() {
                return;
            }
            let view = imp.tab_view.borrow().clone();
            if let Some(view) = view {
                view.close_page(&page);
            }
        });
    }
}

/// The success dialog: what moved, and the command that deletes the kept branch in a label that
/// can be selected and copied. Done is the default and what a close answers.
#[cfg(test)]
pub(super) fn merged_dialog(plan: &MergePlan, done: &Merged) -> adw::AlertDialog {
    merged_dialog_with(plan, done, true)
}

pub(super) fn merged_dialog_with(
    plan: &MergePlan,
    done: &Merged,
    offer_close: bool,
) -> adw::AlertDialog {
    let (before, after) = merged_words(plan, done);
    let dialog = adw::AlertDialog::new(
        Some(&format!("Merged {} into {}", plan.branch, plan.base)),
        None,
    );
    let column = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    column.append(&wrapped(&before));
    let command = wrapped(&delete_branch_command(plan));
    command.add_css_class("monospace");
    command.add_css_class("merge-delete-command");
    command.set_selectable(true);
    column.append(&command);
    if offer_close {
        let note = wrapped(&after);
        note.add_css_class("dim-label");
        column.append(&note);
    }
    dialog.set_extra_child(Some(&column));
    dialog.add_response("done", "Done");
    if offer_close {
        dialog.add_response("close", "Close Thread");
    }
    dialog.set_default_response(Some("done"));
    dialog.set_close_response("done");
    dialog
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use agent_kit::merge::Refusal;
    use agent_kit::worktree::WorktreeInfo;
    use std::path::PathBuf;

    pub(crate) fn plan() -> MergePlan {
        MergePlan {
            info: WorktreeInfo {
                path: PathBuf::from("/r/.main.worktrees/feat-x"),
                branch: "feat/x".into(),
                main_toplevel: PathBuf::from("/r/main"),
                base: Some("main".into()),
            },
            branch: "feat/x".into(),
            base: "main".into(),
            base_dir: PathBuf::from("/r/main"),
            base_tip: "0123456789abcdef0123456789abcdef01234567".into(),
            branch_tip: "f".repeat(40),
            squash_message: "feat/x summary".into(),
            ahead: 3,
            behind: 1,
            prediction: Prediction::Clean,
            hooks: Vec::new(),
            hooks_dir: None,
            squash_hooks: Vec::new(),
            rebase_worktree_hooks: Vec::new(),
            rebase_worktree_hooks_dir: None,
            rebase_base_hooks: Vec::new(),
            worktrees: vec![PathBuf::from("/r/.main.worktrees/feat-x")],
            has_worktree: true,
            refusals: Vec::new(),
        }
    }

    fn seen(title: &str, dir: &Path, worktree: Option<&Path>, busy: bool) -> Seen {
        Seen {
            title: title.into(),
            dir: dir.to_path_buf(),
            worktree: worktree.map(Path::to_path_buf),
            busy,
            own_child: false,
        }
    }

    /// A base checkout with a worktree nested inside its folder, this worktree beside it, and a
    /// link to the base checkout, on disk so paths resolve.
    struct Folders {
        _tmp: tempfile::TempDir,
        base: PathBuf,
        nested: PathBuf,
        here: PathBuf,
        link: PathBuf,
        plan: MergePlan,
    }

    fn folders() -> Folders {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let base = root.join("main");
        let nested = base.join(".wt/other");
        let here = root.join(".main.worktrees/feat-x");
        for dir in [&base.join("src"), &nested, &here] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let link = root.join("link-to-main");
        std::os::unix::fs::symlink(&base, &link).unwrap();
        let mut plan = plan();
        plan.base_dir = base.clone();
        plan.info.main_toplevel = base.clone();
        plan.info.path = here.clone();
        plan.worktrees = vec![nested.clone(), here.clone()];
        Folders {
            _tmp: tmp,
            base,
            nested,
            here,
            link,
            plan,
        }
    }

    #[test]
    fn what_is_in_the_base_is_found_through_links_and_not_in_nested_worktrees() {
        let f = folders();
        let (in_base, in_here) = place(
            &f.plan,
            &[
                seen("Via a link", &f.link.join("src"), None, false),
                seen("In base", &f.base, None, true),
                seen("Nested worktree", &f.nested, None, true),
                seen(
                    "Nested by its tab",
                    &f.base.join(".wt"),
                    Some(&f.nested),
                    true,
                ),
                seen("Elsewhere", Path::new("/nowhere/at/all"), None, true),
                seen("Here too", &f.here, Some(&f.here), false),
            ],
        );
        let titles = |tabs: &[BaseTab]| tabs.iter().map(|t| t.title.clone()).collect::<Vec<_>>();
        assert_eq!(titles(&in_base), ["Via a link", "In base"]);
        assert_eq!(titles(&in_here), ["Here too"]);
    }

    #[test]
    fn this_threads_own_sub_agents_here_refuse_only_while_working() {
        let f = folders();
        let child = |busy| Seen {
            own_child: true,
            ..seen("Sub-agent w1", &f.here, None, busy)
        };
        let idle = blockers(&f.plan, &[child(false)], false);
        assert!(idle.refusals.is_empty(), "{idle:?}");
        assert!(idle.warnings[0].starts_with("Sub-agent w1 of this thread is idle"));
        let working = blockers(&f.plan, &[child(true)], false);
        assert!(working.refusals[0].starts_with("Sub-agent w1 of this thread is working"));
        // Another tree's sub-agent there is no different from another tab.
        let other = blockers(
            &f.plan,
            &[seen("Sub-agent w2", &f.here, None, false)],
            false,
        );
        assert_eq!(other.refusals.len(), 1);
    }

    #[test]
    fn a_terminal_with_no_job_of_its_own_is_not_busy() {
        // Not a terminal at all: unknown, which counts as no job.
        let file = tempfile::tempfile().unwrap();
        assert!(!foreground_job(&file));
        // A terminal nobody's session controls: no job either.
        let (mut leader, mut follower) = (0, 0);
        // SAFETY: openpty writes the two descriptors it opens; the rest are null (defaults).
        let opened = unsafe {
            libc::openpty(
                &mut leader,
                &mut follower,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert_eq!(opened, 0);
        // SAFETY: openpty returned these two as open and owned by us.
        let (leader, follower) = unsafe {
            use std::os::fd::FromRawFd;
            (
                std::os::fd::OwnedFd::from_raw_fd(leader),
                std::os::fd::OwnedFd::from_raw_fd(follower),
            )
        };
        assert!(!foreground_job(&leader));

        // A shell on it running a job in the foreground, as a terminal page's or drawer's
        // would: busy while the job runs, idle once the shell has the terminal back.
        use std::os::unix::process::CommandExt;
        let stdio = || std::process::Stdio::from(follower.try_clone().unwrap());
        let mut shell = std::process::Command::new("sh");
        shell
            .args(["-c", "set -m; sleep 5"])
            .stdin(stdio())
            .stdout(stdio())
            .stderr(stdio());
        // SAFETY: only async-signal-safe calls between fork and exec: a new session, and the
        // terminal on stdin made its controlling terminal.
        unsafe {
            shell.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = shell.spawn().unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut seen_busy = false;
        while std::time::Instant::now() < until && !seen_busy {
            seen_busy = foreground_job(&leader);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // The whole session goes: the job's own group, then the shell.
        use std::os::fd::AsRawFd;
        // SAFETY: reads the terminal's foreground group, then signals that group alone.
        unsafe {
            let job = libc::tcgetpgrp(leader.as_raw_fd());
            if job > 0 && job as u32 != child.id() {
                libc::kill(-job, libc::SIGKILL);
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            seen_busy,
            "a foreground job other than the shell reads busy"
        );
        drop(follower);
    }

    #[test]
    fn another_tab_in_this_worktree_or_this_thread_working_refuses() {
        let f = folders();
        let lines = blockers(&f.plan, &[seen("Second", &f.here, None, false)], false);
        assert_eq!(lines.refusals.len(), 1, "{lines:?}");
        assert!(lines.refusals[0].starts_with("“Second” is also open in this worktree"));
        let lines = blockers(&f.plan, &[], true);
        assert!(lines.refusals[0].starts_with("This thread's agent is working"));
        assert_eq!(blockers(&f.plan, &[], false), TabLines::default());
    }

    #[test]
    fn the_click_checks_again_and_a_turn_started_meanwhile_refuses() {
        let f = folders();
        // Idle when the dialog opened: a warning, and Merge is offered.
        let idle = [seen("Parser", &f.base, None, false)];
        assert_eq!(click_refusal(&blockers(&f.plan, &idle, false)), None);
        // Working by the click: refused, saying nothing was merged.
        let working = [seen("Parser", &f.base, None, true)];
        let why = click_refusal(&blockers(&f.plan, &working, false)).unwrap();
        assert!(why.starts_with("“Parser” is working in"), "{why}");
        assert!(why.ends_with("Nothing was merged."), "{why}");
        // And this thread having started a turn.
        assert!(click_refusal(&blockers(&f.plan, &[], true)).is_some());
    }

    #[test]
    fn a_busy_tab_in_the_base_refuses_and_an_idle_one_warns() {
        let tabs = [
            BaseTab {
                title: "Fix the parser".into(),
                busy: true,
                own_child: false,
            },
            BaseTab {
                title: "Notes".into(),
                busy: false,
                own_child: false,
            },
        ];
        let lines = tab_lines(Path::new("/r/main"), &tabs, &[], false);
        assert_eq!(lines.refusals.len(), 1);
        assert!(lines.refusals[0].starts_with("“Fix the parser” is working in /r/main"));
        assert_eq!(lines.warnings.len(), 1);
        assert!(lines.warnings[0].starts_with("“Notes” is open in /r/main"));
        let own = tab_lines(Path::new("/r/main"), &[], &[], true);
        assert!(own.refusals[0].starts_with("This thread's agent is working"));
        assert_eq!(
            tab_lines(Path::new("/r/main"), &[], &[], false),
            TabLines::default()
        );
    }

    #[test]
    fn the_dialog_says_how_far_each_side_is_and_what_runs() {
        let mut p = plan();
        let lines = plan_lines(&p);
        assert_eq!(
            lines[0],
            "feat/x has 3 commits that main does not. main has 1 commit that feat/x does not."
        );
        assert_eq!(lines[1], "git predicts no conflicts.");
        assert_eq!(lines.last().unwrap(), "main is at 0123456789ab now.");
        p.prediction = Prediction::Unknown;
        p.hooks = vec!["pre-merge-commit".into(), "post-merge".into()];
        p.behind = 0;
        let lines = plan_lines(&p);
        assert_eq!(lines[0], "feat/x has 3 commits that main does not.");
        assert!(lines[1].contains("cannot predict conflicts"));
        assert_eq!(
            lines[2],
            "Your pre-merge-commit, post-merge hooks run, as at any git merge."
        );
        // Exactly the list the merge runs.
        let steps = agent_kit::merge::commands(&p);
        assert_eq!(
            shown_steps(&p),
            steps
                .iter()
                .map(agent_kit::merge::GitStep::shown)
                .collect::<Vec<_>>()
                .join("\n")
        );
        // The settings it runs with are shown with it, and the command is the argument list.
        assert!(shown_steps(&p)
            .starts_with("In /r/main:\nwith branch.main.mergeOptions (unset), merge.ff=false"));
        assert!(shown_steps(&p).ends_with(&shown_command(&steps[0].args)));
    }

    #[test]
    fn success_explains_the_kept_branch_and_gives_the_delete_command() {
        let p = plan();
        let (before, after) = merged_words(
            &p,
            &Merged {
                before: "0123456789ab".into(),
                after: "fedcba987654".into(),
            },
        );
        assert!(before.starts_with("main moved from 0123456789ab to fedcba987654 in /r/main."));
        assert!(before.contains("The branch feat/x is kept"));
        assert!(after.starts_with("Closing the thread offers to remove the worktree"));
        assert_eq!(
            delete_branch_command(&p),
            "git -C /r/main branch -D -- feat/x"
        );
        let mut odd = plan();
        odd.base_dir = PathBuf::from("/r/my repo");
        assert_eq!(
            delete_branch_command(&odd),
            "git -C '/r/my repo' branch -D -- feat/x"
        );
    }

    /// GTK: run from the window's construction test. Merge is offered only with nothing
    /// refusing it, Cancel is the default, and the commands are the ones that run.
    pub(crate) fn the_merge_dialog_refuses_what_it_must() {
        let p = plan();
        let parts = merge_dialog(&p, &TabLines::default());
        assert!(parts.dialog.is_response_enabled("merge"));
        assert_eq!(parts.dialog.default_response().as_deref(), Some("cancel"));
        assert_eq!(parts.dialog.close_response().as_str(), "cancel");
        assert_eq!(parts.dialog.response_label("merge").as_str(), "Merge");
        assert_eq!(parts.commands.text().as_str(), shown_steps(&p));
        assert!(parts.refusals.is_empty());

        let mut dirty = plan();
        dirty.refusals = vec![Refusal::BaseChanges];
        let parts = merge_dialog(&dirty, &TabLines::default());
        assert!(!parts.dialog.is_response_enabled("merge"));
        assert_eq!(parts.refusals, vec![Refusal::BaseChanges.reason(&dirty)]);

        let busy = tab_lines(
            &p.base_dir,
            &[BaseTab {
                title: "Busy".into(),
                busy: true,
                own_child: false,
            }],
            &[],
            false,
        );
        let parts = merge_dialog(&p, &busy);
        assert!(
            !parts.dialog.is_response_enabled("merge"),
            "a busy tab refuses"
        );

        // The success dialog's delete command is its own selectable, monospace label.
        let done = merged_dialog(
            &p,
            &Merged {
                before: "0123456789ab".into(),
                after: "fedcba987654".into(),
            },
        );
        assert_eq!(done.default_response().as_deref(), Some("done"));
        assert_eq!(done.close_response().as_str(), "done");
        let command = done
            .extra_child()
            .and_then(|c| crate::testutil::find_by_class::<gtk4::Label>(&c, "merge-delete-command"))
            .expect("the delete command's label");
        assert!(command.is_selectable());
        assert!(command.has_css_class("monospace"));
        assert_eq!(command.text().as_str(), delete_branch_command(&p));

        // Switching methods updates preview steps, button label, and blank-squash guard.
        let parts = merge_dialog_with(
            &p,
            &TabLines::default(),
            agent_kit::merge::MergeMethod::Squash,
        );
        assert_eq!(
            parts.dialog.response_label("merge").as_str(),
            "Squash and Commit"
        );
        assert!(parts.commands.text().as_str().contains("--squash"));
        assert!(parts.dialog.is_response_enabled("merge"));
        parts.squash_buffer.set_text("   ");
        assert!(!parts.dialog.is_response_enabled("merge"));
        parts.method_row.set_selected(2);
        assert_eq!(
            parts.dialog.response_label("merge").as_str(),
            "Rebase and Merge"
        );
        assert!(parts.commands.text().as_str().contains("git rebase"));
        assert!(parts.dialog.is_response_enabled("merge"));

        // Follow-up options: Ask Agent on predicted conflicts (clean worktree), Commit First on
        // dirty worktree, and branch-only filtering hides Rebase.
        let mut conflicted = plan();
        conflicted.refusals = vec![Refusal::Conflicts(vec!["src/main.rs".into()])];
        let parts = merge_dialog_full(
            &conflicted,
            &TabLines::default(),
            MergeDialogOptions {
                initial: agent_kit::merge::MergeMethod::Merge,
                can_ask_agent: true,
                can_commit_first: true,
            },
        );
        assert!(!parts.dialog.is_response_enabled("merge"));
        assert!(parts.dialog.has_response("ask-agent"));
        assert!(!parts.dialog.has_response("commit-first"));
        let prompt = conflict_handoff_prompt(&conflicted);
        assert!(prompt.contains("src/main.rs"));
        assert!(prompt.contains("git merge refs/heads/main"), "{prompt}");

        // Tab blockers or other plan refusals suppress Ask Agent.
        let blocked_parts = merge_dialog_full(
            &conflicted,
            &busy,
            MergeDialogOptions {
                initial: agent_kit::merge::MergeMethod::Merge,
                can_ask_agent: true,
                can_commit_first: true,
            },
        );
        assert!(!blocked_parts.dialog.has_response("ask-agent"));
        let mut odd_base = conflicted.clone();
        odd_base.base = "release candidate".into();
        assert!(
            conflict_handoff_prompt(&odd_base).contains("git merge 'refs/heads/release candidate'")
        );

        let mut dirty_wt = plan();
        dirty_wt.refusals = vec![
            Refusal::WorktreeChanges,
            Refusal::Conflicts(vec!["a.rs".into()]),
        ];
        let parts = merge_dialog_full(
            &dirty_wt,
            &TabLines::default(),
            MergeDialogOptions {
                initial: agent_kit::merge::MergeMethod::Merge,
                can_ask_agent: true,
                can_commit_first: true,
            },
        );
        assert!(parts.dialog.has_response("commit-first"));
        assert!(!parts.dialog.has_response("ask-agent"));

        let mut branch_only = plan();
        branch_only.has_worktree = false;
        let parts = merge_dialog_full(
            &branch_only,
            &TabLines::default(),
            MergeDialogOptions {
                initial: agent_kit::merge::MergeMethod::Rebase,
                can_ask_agent: false,
                can_commit_first: false,
            },
        );
        assert_eq!(
            parts.methods,
            vec![
                agent_kit::merge::MergeMethod::Merge,
                agent_kit::merge::MergeMethod::Squash
            ]
        );
        assert!(!parts.dialog.has_response("ask-agent"));
        assert!(!parts.dialog.has_response("commit-first"));
        assert!(!merged_dialog_with(
            &branch_only,
            &Merged {
                before: "0123456789ab".into(),
                after: "fedcba987654".into(),
            },
            false,
        )
        .has_response("close"));
    }
}

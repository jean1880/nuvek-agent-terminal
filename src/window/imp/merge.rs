//! "Merge into …" in a worktree tab's diff panel: the worktree's branch merged back into the
//! branch it started from, in the main checkout, after a dialog that shows what was read and the
//! exact command, in the directory it runs in.
//!
//! The reading and the merge are `agent_kit::merge`, run off the main thread. The window adds
//! what only it knows: the tabs open in the base checkout. A tab there that is working refuses
//! the merge (it would rewrite files under a running agent); an idle one is a warning. The branch
//! and the worktree are never deleted: the success dialog says how to delete the branch, and
//! closing the thread offers the usual clean-only removal of the worktree.

use adw::prelude::*;
use adw::subclass::prelude::*;
use agent_core::commit_message::shown_command;
use agent_core::redact::redact;
use agent_kit::merge::{MergePlan, Merged, Prediction};
use gtk4::glib;
use std::path::Path;
use tracing::{info, warn};

use super::threads::off_main;
use super::{present_message_over, AgentTerminalWindow, CHECKPOINT_QUIET};

/// An open tab in the base checkout, as the dialog names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BaseTab {
    pub title: String,
    /// Its agent is working (a thread's turn, background work or a question), or a terminal
    /// page has shown output just now.
    pub busy: bool,
}

/// What stops the merge and what to know first, from the tabs open in the base checkout and
/// whether this tab itself is working.
pub(super) fn tab_lines(base_dir: &Path, tabs: &[BaseTab], own_busy: bool) -> TabLines {
    let dir = base_dir.display();
    let mut lines = TabLines::default();
    if own_busy {
        lines.refusals.push(
            "This thread's agent is working. Wait for its turn to end, or stop it, first: it may \
             be changing the branch."
                .to_owned(),
        );
    }
    for tab in tabs {
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
pub(super) fn shown_steps(plan: &MergePlan) -> String {
    agent_kit::merge::commands(plan)
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

/// The success dialog's words.
pub(super) fn merged_words(plan: &MergePlan, done: &Merged) -> String {
    format!(
        "{base} moved from {before} to {after} in {dir}.\n\nThe branch {branch} is kept: the app \
         never deletes a branch. Once its worktree is removed, this deletes it:\n\n{command}\n\n\
         Closing the thread offers to remove the worktree when nothing in it is uncommitted.",
        base = plan.base,
        before = done.before,
        after = if done.after.is_empty() {
            "the merge"
        } else {
            &done.after
        },
        dir = plan.base_dir.display(),
        branch = plan.branch,
        command = delete_branch_command(plan),
    )
}

/// The merge dialog's parts the flow and its test read back.
pub(super) struct MergeDialog {
    pub dialog: adw::AlertDialog,
    #[cfg_attr(not(test), expect(dead_code, reason = "read by the dialog test"))]
    pub commands: gtk4::Label,
    #[cfg_attr(not(test), expect(dead_code, reason = "read by the dialog test"))]
    pub refusals: Vec<String>,
}

/// Builds the dialog for `plan`, with what the tabs in the base checkout add. Merge is offered
/// only when nothing refuses it; Cancel is the default and what a press outside answers.
pub(super) fn merge_dialog(plan: &MergePlan, tabs: &TabLines) -> MergeDialog {
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
    let heading = wrapped("Runs, in this order:");
    heading.add_css_class("heading");
    column.append(&heading);
    let commands = wrapped(&shown_steps(plan));
    commands.add_css_class("monospace");
    commands.set_selectable(true);
    column.append(&commands);
    let note = wrapped(&format!(
        "The merge is never forced and skips no hook. If it stops part-way (a conflict, a hook \
         that refuses, a timeout) it is undone with git merge --abort. The branch {} and its \
         worktree are kept.",
        plan.branch
    ));
    note.add_css_class("dim-label");
    column.append(&note);
    dialog.set_extra_child(Some(&column));

    dialog.add_response("cancel", "Cancel");
    dialog.add_response("merge", "Merge");
    dialog.set_response_appearance("merge", adw::ResponseAppearance::Suggested);
    dialog.set_response_enabled("merge", refusals.is_empty());
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    MergeDialog {
        dialog,
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

impl AgentTerminalWindow {
    /// The tabs open in `base_dir`, in every window, leaving out those in the worktree itself.
    fn base_checkout_tabs(&self, base_dir: &Path, worktree: &Path) -> Vec<BaseTab> {
        let found = std::cell::RefCell::new(Vec::new());
        self.for_each_window(|window| {
            for tab in window.tabs.borrow().iter() {
                let dir = Path::new(&tab.dir);
                if tab.remote.is_some() || !dir.starts_with(base_dir) || dir.starts_with(worktree) {
                    continue;
                }
                let busy = match &tab.chat {
                    Some(chat) => chat.busy(),
                    None => tab.last_output.get().elapsed() < CHECKPOINT_QUIET,
                };
                found.borrow_mut().push(BaseTab {
                    title: tab.page.title().to_string(),
                    busy,
                });
            }
        });
        found.into_inner()
    }

    /// Whether the tab whose panel is `panel_root` is working.
    fn panel_tab_busy(&self, panel_root: &gtk4::Box) -> bool {
        self.tab_by_panel(panel_root, |t| match &t.chat {
            Some(chat) => chat.busy(),
            None => t.last_output.get().elapsed() < CHECKPOINT_QUIET,
        })
        .unwrap_or(false)
    }

    /// "Merge into …" in the diff panel behind `panel_root`: reads the plan off the main thread,
    /// then shows it with the exact command. The panel's button stays off until the dialog is up
    /// or the reason it is not has been said.
    pub(super) fn merge_from_panel(&self, panel_root: &gtk4::Box) {
        let Some((page, panel, worktree)) = self.tab_by_panel(panel_root, |t| {
            (t.page.clone(), t.diff_panel.clone(), t.worktree.clone())
        }) else {
            return;
        };
        let parent = panel.dialog_parent();
        let Some(info) = worktree else {
            panel.set_merge_preparing(false);
            return;
        };
        let obj = self.obj().downgrade();
        let root = panel_root.clone();
        glib::MainContext::default().spawn_local(async move {
            let read = {
                let info = info.clone();
                off_main("reading the merge", move || {
                    agent_kit::merge::preflight(&info)
                })
                .await
            };
            panel.set_merge_preparing(false);
            let Some(obj) = obj.upgrade() else { return };
            let plan = match read {
                Ok(plan) => plan,
                Err(why) => {
                    present_message_over(&parent, "Can't Merge This Worktree", &redact(&why));
                    return;
                }
            };
            let imp = obj.imp();
            let tabs = tab_lines(
                &plan.base_dir,
                &imp.base_checkout_tabs(&plan.base_dir, &info.path),
                imp.panel_tab_busy(&root),
            );
            let parts = merge_dialog(&plan, &tabs);
            parts.dialog.present(Some(&parent));
            let obj = obj.downgrade();
            parts.dialog.connect_response(None, move |_, response| {
                if response != "merge" {
                    return;
                }
                let Some(obj) = obj.upgrade() else { return };
                obj.imp()
                    .run_merge(&root, &page, &panel, &parent, plan.clone());
            });
        });
    }

    /// Runs the merge `plan` previewed, once the tabs in the base checkout are checked again: a
    /// turn may have started while the dialog was up.
    fn run_merge(
        &self,
        panel_root: &gtk4::Box,
        page: &adw::TabPage,
        panel: &crate::window::diff_panel::DiffPanel,
        parent: &gtk4::Widget,
        plan: MergePlan,
    ) {
        let tabs = tab_lines(
            &plan.base_dir,
            &self.base_checkout_tabs(&plan.base_dir, &plan.info.path),
            self.panel_tab_busy(panel_root),
        );
        if let Some(why) = tabs.refusals.first() {
            present_message_over(
                parent,
                "Can't Merge Right Now",
                &format!("{why}\n\nNothing was merged."),
            );
            return;
        }
        let (page, panel, parent) = (page.clone(), panel.clone(), parent.clone());
        let obj = self.obj().downgrade();
        glib::MainContext::default().spawn_local(async move {
            panel.set_merging(true);
            let done = {
                let plan = plan.clone();
                off_main("the merge", move || agent_kit::merge::merge(&plan)).await
            };
            panel.set_merging(false);
            let Some(obj) = obj.upgrade() else { return };
            match done {
                Ok(done) => {
                    info!("merged a worktree's branch into its base");
                    obj.imp().merged(&page, &parent, &plan, &done);
                }
                Err(why) => {
                    warn!("a merge from a worktree did not go through");
                    present_message_over(&parent, "Could Not Merge", &why);
                }
            }
            obj.imp().refresh_diff(&page);
        });
    }

    /// Says the merge is made, how to delete the branch, and offers to close the thread (which
    /// offers the worktree's removal as any worktree tab's close does).
    fn merged(&self, page: &adw::TabPage, parent: &gtk4::Widget, plan: &MergePlan, done: &Merged) {
        let dialog = adw::AlertDialog::new(
            Some(&format!("Merged {} into {}", plan.branch, plan.base)),
            Some(&merged_words(plan, done)),
        );
        dialog.add_response("done", "Done");
        dialog.add_response("close", "Close Thread");
        dialog.set_default_response(Some("done"));
        dialog.set_close_response("done");
        dialog.present(Some(parent));
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
            ahead: 3,
            behind: 1,
            prediction: Prediction::Clean,
            hooks: Vec::new(),
            refusals: Vec::new(),
        }
    }

    #[test]
    fn a_busy_tab_in_the_base_refuses_and_an_idle_one_warns() {
        let tabs = [
            BaseTab {
                title: "Fix the parser".into(),
                busy: true,
            },
            BaseTab {
                title: "Notes".into(),
                busy: false,
            },
        ];
        let lines = tab_lines(Path::new("/r/main"), &tabs, false);
        assert_eq!(lines.refusals.len(), 1);
        assert!(lines.refusals[0].starts_with("“Fix the parser” is working in /r/main"));
        assert_eq!(lines.warnings.len(), 1);
        assert!(lines.warnings[0].starts_with("“Notes” is open in /r/main"));
        let own = tab_lines(Path::new("/r/main"), &[], true);
        assert!(own.refusals[0].starts_with("This thread's agent is working"));
        assert_eq!(
            tab_lines(Path::new("/r/main"), &[], false),
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
        assert_eq!(
            shown_steps(&p),
            agent_kit::merge::commands(&p)
                .iter()
                .map(|s| format!("In {}:\n{}", s.dir.display(), shown_command(&s.args)))
                .collect::<Vec<_>>()
                .join("\n")
        );
        assert_eq!(
            shown_steps(&p),
            "In /r/main:\ngit merge --no-ff --no-edit -m 'Merge branch '\\''feat/x'\\''' -- \
             refs/heads/feat/x"
        );
    }

    #[test]
    fn success_explains_the_kept_branch_and_gives_the_delete_command() {
        let p = plan();
        let words = merged_words(
            &p,
            &Merged {
                before: "0123456789ab".into(),
                after: "fedcba987654".into(),
            },
        );
        assert!(words.starts_with("main moved from 0123456789ab to fedcba987654 in /r/main."));
        assert!(words.contains("The branch feat/x is kept"));
        assert!(words.contains("git -C /r/main branch -D -- feat/x"));
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
            }],
            false,
        );
        let parts = merge_dialog(&p, &busy);
        assert!(
            !parts.dialog.is_response_enabled("merge"),
            "a busy tab refuses"
        );
    }
}

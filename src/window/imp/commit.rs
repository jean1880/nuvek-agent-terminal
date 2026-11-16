//! "Commit…" in a diff panel: the ticked files committed in one step through the user's own git,
//! after a dialog that shows the branch, what stays staged, the message and the exact commands.
//!
//! The commit itself is `agent_kit::commit`, run off the main thread. Drafting a message is a
//! prompt put in the thread's composer for the user to review and send: no side request, so no
//! new process, credential or model ever sees the diff.

use adw::prelude::*;
use adw::subclass::prelude::*;
use agent_core::commit_message::{self, MessageCheck};
use agent_kit::commit::{CommitContext, CommitRequest};
use gtk4::glib;
use tracing::{info, warn};

use super::threads::off_main;
use super::{present_message_over, AgentTerminalWindow};
use crate::window::diff_panel::{CommitAsk, DiffPanel};

/// At most this many files are named in the dialog; the rest are counted.
const LISTED_FILES: usize = 12;

/// Every path `ask` commits, renames' old paths included, each once and in the diff's order.
pub(super) fn ask_paths(ask: &CommitAsk) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    for path in ask.files.iter().flat_map(|f| f.paths()) {
        if !paths.iter().any(|p| p == path) {
            paths.push(path.to_owned());
        }
    }
    paths
}

/// The dialog's file list: the chosen files, the first [`LISTED_FILES`] named.
fn file_lines(ask: &CommitAsk) -> String {
    let mut lines: Vec<String> = ask
        .files
        .iter()
        .take(LISTED_FILES)
        .map(|f| match &f.old_path {
            Some(old) => format!("{old} → {}", f.path),
            None => f.path.clone(),
        })
        .collect();
    let more = ask.files.len().saturating_sub(LISTED_FILES);
    if more > 0 {
        lines.push(format!("… and {more} more"));
    }
    lines.join("\n")
}

/// What the dialog says about where the commit goes, what it leaves out and what stays staged.
pub(super) fn context_lines(ctx: &CommitContext, ask: &CommitAsk) -> Vec<String> {
    let mut lines = Vec::new();
    let chosen = ask.files.len();
    lines.push(match (&ctx.branch, ctx.unborn) {
        (Some(branch), true) => format!("The first commit on {branch}."),
        (Some(branch), false) => format!("On {branch}."),
        (None, _) => "On a detached HEAD.".to_owned(),
    });
    if chosen < ask.listed {
        let left = ask.listed - chosen;
        lines.push(format!(
            "{chosen} of {} files; the {left} left unticked stay as they are.",
            ask.listed
        ));
    }
    match ctx.staged_elsewhere.len() {
        0 => {}
        1 => lines.push("1 file you staged yourself stays staged and is not committed.".into()),
        n => lines.push(format!(
            "{n} files you staged yourself stay staged and are not committed."
        )),
    }
    lines.push(
        "Files the app does not show (ignored, secret-looking or too large) are not included."
            .into(),
    );
    lines
}

/// The words under the message box for what [`commit_message::check`] found.
pub(super) fn check_words(check: &MessageCheck) -> String {
    let mut words = Vec::new();
    if check.empty {
        words.push("Write a message to commit.".to_owned());
    }
    if let Some(length) = check.long_subject {
        words.push(format!(
            "The first line is {length} characters; git tools show {} or fewer best.",
            commit_message::SUBJECT_LIMIT
        ));
    }
    if check.looks_secret {
        words.push(
            "This message looks like it contains a secret. Committing it asks once more.".into(),
        );
    }
    words.join(" ")
}

/// The two commands a commit runs, as the confirmation shows them.
pub(super) fn shown_commands(request: &CommitRequest) -> String {
    format!(
        "{}\n{}",
        commit_message::shown_command(&agent_kit::commit::add_args(&request.paths)),
        commit_message::shown_command(&agent_kit::commit::commit_args(request))
    )
}

/// The commit dialog's parts the flow reads back.
pub(super) struct CommitDialog {
    pub dialog: adw::AlertDialog,
    pub message: gtk4::TextView,
    #[cfg_attr(not(test), expect(dead_code, reason = "read by the dialog test"))]
    pub commands: gtk4::Label,
    #[cfg_attr(not(test), expect(dead_code, reason = "read by the dialog test"))]
    pub check: gtk4::Label,
}

/// Builds the dialog for committing `ask` with `ctx`, its message box holding `draft`. Commit is
/// on only while the message is not empty; the commands follow the message as it is typed.
/// `can_draft`: the tab is a thread, so "Draft Message" is offered.
pub(super) fn commit_dialog(
    ctx: &CommitContext,
    ask: &CommitAsk,
    paths: &[String],
    draft: &str,
    can_draft: bool,
) -> CommitDialog {
    let dialog = adw::AlertDialog::new(Some("Commit Changes"), None);
    let column = gtk4::Box::new(gtk4::Orientation::Vertical, 8);

    if let Some(warning) = ctx.branch_warning() {
        let label = wrapped(&warning);
        label.add_css_class("warning");
        column.append(&label);
    }
    for line in context_lines(ctx, ask) {
        let label = wrapped(&line);
        label.add_css_class("dim-label");
        column.append(&label);
    }
    let files = wrapped(&file_lines(ask));
    files.add_css_class("monospace");
    files.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
    files.set_wrap(false);
    column.append(&files);

    let message = gtk4::TextView::builder()
        .wrap_mode(gtk4::WrapMode::WordChar)
        .accepts_tab(false)
        .top_margin(6)
        .bottom_margin(6)
        .left_margin(6)
        .right_margin(6)
        .build();
    message.update_property(&[gtk4::accessible::Property::Label("Commit message")]);
    message.buffer().set_text(draft);
    let scroller = gtk4::ScrolledWindow::builder()
        .child(&message)
        .min_content_height(110)
        .max_content_height(240)
        .propagate_natural_height(true)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .css_classes(["card"])
        .build();
    let heading = wrapped("Message: a subject line, a blank line, then the body.");
    heading.add_css_class("heading");
    column.append(&heading);
    column.append(&scroller);
    let check = wrapped("");
    check.add_css_class("caption");
    column.append(&check);

    let commands = wrapped("");
    commands.add_css_class("monospace");
    commands.set_selectable(true);
    commands.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
    column.append(&commands);
    let note = wrapped("Your git hooks and signing run. Nothing is skipped or amended.");
    note.add_css_class("dim-label");
    column.append(&note);
    dialog.set_extra_child(Some(&column));

    dialog.add_response("cancel", "Cancel");
    if can_draft {
        dialog.add_response("draft", "Draft Message");
    }
    dialog.add_response("commit", "Commit");
    dialog.set_response_appearance("commit", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    let paths = paths.to_vec();
    let update = glib::clone!(
        #[weak]
        dialog,
        #[weak]
        check,
        #[weak]
        commands,
        move |buffer: &gtk4::TextBuffer| {
            let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
            let found = commit_message::check(&text);
            dialog.set_response_enabled("commit", !found.empty);
            let words = check_words(&found);
            check.set_visible(!words.is_empty());
            check.set_text(&words);
            commands.set_text(&shown_commands(&CommitRequest {
                message: text.to_string(),
                paths: paths.clone(),
            }));
        }
    );
    update(&message.buffer());
    message.buffer().connect_changed(update);
    CommitDialog {
        dialog,
        message,
        commands,
        check,
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

fn text_of(view: &gtk4::TextView) -> String {
    let buffer = view.buffer();
    buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), false)
        .to_string()
}

impl AgentTerminalWindow {
    /// "Commit…" in the diff panel behind `panel_root`: reads the repository off the main thread,
    /// then asks with the dialog. A half-done merge or rebase, or a repository that cannot be
    /// read, is said instead.
    pub(super) fn commit_from_panel(&self, panel_root: &gtk4::Box, ask: CommitAsk) {
        let Some((page, panel, view)) = self.tab_by_panel(panel_root, |t| {
            (
                t.page.clone(),
                t.diff_panel.clone(),
                t.chat.as_ref().and_then(|c| c.view.clone()),
            )
        }) else {
            return;
        };
        let parent = panel.dialog_parent();
        let paths = ask_paths(&ask);
        let obj = self.obj().downgrade();
        glib::MainContext::default().spawn_local(async move {
            let read = {
                let (repo, paths) = (ask.repo.clone(), paths.clone());
                off_main("reading the repository", move || {
                    agent_kit::commit::commit_context(&repo, &paths)
                })
                .await
            };
            let Some(obj) = obj.upgrade() else { return };
            let ctx = match read {
                Ok(ctx) => ctx,
                Err(why) => {
                    present_message_over(&parent, "Could Not Read the Repository", &why);
                    return;
                }
            };
            if let Some(op) = ctx.in_progress {
                present_message_over(&parent, "Can't Commit Right Now", op.reason());
                return;
            }
            let parts = commit_dialog(&ctx, &ask, &paths, &panel.commit_draft(), view.is_some());
            let message = parts.message.clone();
            parts.dialog.present(Some(&parent));
            message.grab_focus();
            let obj = obj.downgrade();
            parts.dialog.connect_response(None, move |_, response| {
                let text = text_of(&message);
                // Kept whatever the answer: a commit that works clears it.
                panel.set_commit_draft(&text);
                let Some(obj) = obj.upgrade() else { return };
                match response {
                    "draft" => {
                        if let Some(view) = &view {
                            view.insert_attachment_text(&commit_message::draft_prompt(&paths));
                            obj.imp().show_toast(
                                "A prompt to draft the message is in the thread's composer",
                            );
                        }
                    }
                    "commit" => {
                        let request = CommitRequest {
                            message: text,
                            paths: paths.clone(),
                        };
                        obj.imp().confirm_and_commit(
                            &page,
                            &panel,
                            &parent,
                            ask.repo.clone(),
                            request,
                        );
                    }
                    _ => {}
                }
            });
        });
    }

    /// Commits `request` in `repo`, asking once more first when its message looks like it holds
    /// a secret (it is committed as typed, never masked).
    fn confirm_and_commit(
        &self,
        page: &adw::TabPage,
        panel: &DiffPanel,
        parent: &gtk4::Widget,
        repo: std::path::PathBuf,
        request: CommitRequest,
    ) {
        let (page, panel, parent) = (page.clone(), panel.clone(), parent.clone());
        let obj = self.obj().downgrade();
        glib::MainContext::default().spawn_local(async move {
            if commit_message::check(&request.message).looks_secret {
                let ask = crate::dialogs::confirm_dialog(
                    "Commit a Message That Looks Like a Secret?",
                    "Part of this message looks like a key or token. It is committed exactly as \
                     written, and a commit is hard to take back once pushed.",
                    ("cancel", "Cancel"),
                    ("commit", "Commit Anyway"),
                    adw::ResponseAppearance::Destructive,
                    false,
                );
                if !crate::dialogs::confirmed(&ask, &parent, "commit").await {
                    return;
                }
            }
            panel.set_committing(true);
            let done = off_main("the commit", move || {
                agent_kit::commit::commit_paths(&repo, &request)
            })
            .await;
            panel.set_committing(false);
            let Some(obj) = obj.upgrade() else { return };
            match done {
                Ok(done) => {
                    info!("committed from the diff panel");
                    panel.set_commit_draft("");
                    obj.imp().show_toast(&if done.short.is_empty() {
                        "Committed".to_owned()
                    } else {
                        format!("Committed {}", done.short)
                    });
                }
                Err(why) => {
                    warn!("a commit from the diff panel failed");
                    present_message_over(&parent, "Could Not Commit", &why);
                }
            }
            obj.imp().refresh_diff(&page);
        });
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::window::diff_panel::CommitFile;
    use std::path::PathBuf;

    fn ask(files: &[(&str, Option<&str>)], listed: usize) -> CommitAsk {
        CommitAsk {
            repo: PathBuf::from("/r"),
            files: files
                .iter()
                .map(|(p, o)| CommitFile {
                    path: (*p).to_owned(),
                    old_path: o.map(str::to_owned),
                })
                .collect(),
            listed,
        }
    }

    fn ctx(branch: Option<&str>, staged: &[&str]) -> CommitContext {
        CommitContext {
            toplevel: PathBuf::from("/r"),
            branch: branch.map(str::to_owned),
            unborn: false,
            on_default_branch: branch == Some("main"),
            in_progress: None,
            staged_elsewhere: staged.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn a_rename_commits_both_its_paths_once() {
        let a = ask(
            &[("new.rs", Some("old.rs")), ("a.rs", None), ("old.rs", None)],
            3,
        );
        assert_eq!(ask_paths(&a), vec!["new.rs", "old.rs", "a.rs"]);
        assert_eq!(file_lines(&a), "old.rs → new.rs\na.rs\nold.rs");
    }

    #[test]
    fn the_dialog_says_what_stays_staged_and_what_is_left_out() {
        let a = ask(&[("a.rs", None)], 3);
        let lines = context_lines(&ctx(Some("feat/x"), &["b.rs", "c.rs"]), &a);
        assert_eq!(lines[0], "On feat/x.");
        assert!(lines.iter().any(|l| l.starts_with("1 of 3 files")));
        assert!(lines
            .iter()
            .any(|l| l == "2 files you staged yourself stay staged and are not committed."));
        assert!(lines
            .iter()
            .any(|l| l.starts_with("Files the app does not show")));
        let lines = context_lines(&ctx(None, &[]), &ask(&[("a.rs", None)], 1));
        assert_eq!(lines[0], "On a detached HEAD.");
        assert!(!lines.iter().any(|l| l.contains("staged yourself")));
    }

    #[test]
    fn the_shown_commands_are_the_ones_that_run() {
        let request = CommitRequest {
            message: "Fix it\n\nWhy.".into(),
            paths: vec!["a.rs".into(), "-odd".into()],
        };
        assert_eq!(
            shown_commands(&request),
            "git --literal-pathspecs add -A -- a.rs -odd\n\
             git --literal-pathspecs commit -m 'Fix it\n\nWhy.' -- a.rs -odd"
        );
        assert!(check_words(&commit_message::check("ok")).is_empty());
        assert!(check_words(&commit_message::check("")).contains("Write a message"));
    }

    /// GTK: run from the window's construction test. Commit is off while the message is empty
    /// and the commands follow what is typed; the draft comes back; the branch warning shows.
    pub(crate) fn the_commit_dialog_follows_its_message() {
        let a = ask(&[("a.rs", None)], 1);
        let paths = ask_paths(&a);
        let parts = commit_dialog(&ctx(Some("main"), &[]), &a, &paths, "", true);
        assert!(!parts.dialog.is_response_enabled("commit"));
        assert!(parts.dialog.has_response("draft"));
        assert!(parts.check.text().contains("Write a message"));
        parts.message.buffer().set_text("Fix it");
        assert!(parts.dialog.is_response_enabled("commit"));
        assert_eq!(
            parts.commands.text().as_str(),
            "git --literal-pathspecs add -A -- a.rs\ngit --literal-pathspecs commit -m 'Fix it' -- a.rs"
        );
        assert!(!parts.check.is_visible());

        let parts = commit_dialog(&ctx(None, &[]), &a, &paths, "Kept draft", false);
        assert_eq!(text_of(&parts.message), "Kept draft");
        assert!(parts.dialog.is_response_enabled("commit"));
        assert!(!parts.dialog.has_response("draft"), "no thread, no draft");
    }
}

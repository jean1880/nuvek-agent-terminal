//! "Commit…" in a diff panel: the ticked files committed in one step through the user's own git,
//! after a dialog that shows the branch, what stays staged, the message and the exact commands.
//!
//! The commit itself is `agent_kit::commit`, run off the main thread. Drafting a message is a
//! prompt put in the thread's composer for the user to review and send: no side request, so no
//! new process, credential or model ever sees the diff.

use adw::prelude::*;
use adw::subclass::prelude::*;
use agent_core::commit_message::{self, MessageCheck};
use agent_core::redact::redact;
use agent_kit::commit::{CommitContext, CommitRequest, HunkPatch, PartialCommitRequest};
use gtk4::glib;
use tracing::{info, warn};

use super::threads::{off_main, store_job};
use super::{present_message_over, AgentTerminalWindow};
use crate::window::diff_panel::{CommitAsk, DiffPanel};

/// At most this many files are named in the dialog; the rest are counted.
const LISTED_FILES: usize = 12;
const REPLY_LIMIT: usize = 2_000;
const EVENT_LIMIT: usize = 10_000;

/// Read a bounded suffix and refuse an incomplete or superseded turn.
fn draft_reply(
    store: &agent_kit::store::Store,
    thread: &str,
    baseline: i64,
    prompt: &str,
) -> agent_kit::store::Result<Option<(i64, String)>> {
    use agent_core::event::{Event, TurnState};
    let events = store.events(thread, Some(baseline), EVENT_LIMIT + 1)?;
    if events.len() > EVENT_LIMIT {
        return Ok(None);
    }
    let items = store.transcript_suffix(thread, &events);
    if last_seq(store, thread)? != events.last().map_or(baseline, |(seq, _)| *seq) {
        return Ok(None);
    }
    let Some(user) = items.iter().rev().find(|i| i.message.role == "user") else {
        return Ok(None);
    };
    if user.first_seq <= baseline || user.message.text != prompt {
        return Ok(None);
    }
    let Some(reply) = items.iter().rev().find(|i| i.message.role == "assistant") else {
        return Ok(None);
    };
    if reply.first_seq <= user.first_seq
        || reply.message.status != "completed"
        || reply.message.text.trim().is_empty()
        || reply.message.text.chars().count() > REPLY_LIMIT
    {
        return Ok(None);
    }
    let mut completion = None;
    let mut started = None;
    for (seq, env) in &events {
        if *seq <= user.first_seq {
            continue;
        }
        // A message queued during another turn predates that turn's end. Its next reply may
        // belong to an earlier queued message, so its turn cannot be identified safely here.
        if started.is_none()
            && completion.is_none()
            && matches!(env.event, Event::TurnCompleted { .. })
        {
            return Ok(None);
        }
        match &env.event {
            Event::TurnStarted { .. } => {
                started = Some(*seq);
                completion = None;
            }
            Event::TurnCompleted {
                state: TurnState::Completed,
                error: None,
                ..
            } if started.is_some() => completion = started.take().map(|start| (start, *seq)),
            Event::TurnCompleted { .. } => {
                started = None;
                completion = None;
            }
            _ => {}
        }
    }
    let Some((start, end)) = completion else {
        return Ok(None);
    };
    if end <= reply.first_seq
        || reply.first_seq <= start
        || !events.iter().any(|(seq, env)| {
            *seq == reply.first_seq
                && env.item.as_deref() == Some(&reply.message.item_id)
                && matches!(
                    env.event,
                    Event::ItemStarted {
                        kind: agent_core::event::ItemKind::AssistantMessage,
                        parent: None,
                        ..
                    }
                )
        })
        || !events.iter().any(|(seq, env)| {
            *seq < end
                && env.item.as_deref() == Some(&reply.message.item_id)
                && matches!(
                    env.event,
                    Event::ItemCompleted {
                        status: agent_core::event::ItemStatus::Completed,
                        ..
                    }
                )
        })
        || events
            .iter()
            .any(|(seq, env)| *seq > end && env.item.as_deref() == Some(&reply.message.item_id))
    {
        return Ok(None);
    }
    Ok(Some((reply.first_seq, reply.message.text.clone())))
}

fn last_seq(store: &agent_kit::store::Store, thread: &str) -> agent_kit::store::Result<i64> {
    let after = store.tail_after(thread, 1)?;
    Ok(store
        .events(thread, after, 1)?
        .last()
        .map_or(0, |(seq, _)| *seq))
}

/// Every path `ask` commits (whole files, renames' old paths, and partial-hunk files), each once
/// and in the diff's order.
pub(super) fn ask_paths(ask: &CommitAsk) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    for path in ask
        .files
        .iter()
        .flat_map(|f| f.paths())
        .chain(ask.patches.iter().map(|p| p.path.as_str()))
    {
        if !paths.iter().any(|p| p == path) {
            paths.push(path.to_owned());
        }
    }
    paths
}

/// Whole-file paths `ask` commits, renames' old paths included, each once and in the diff's order.
pub(super) fn whole_paths(ask: &CommitAsk) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    for path in ask.files.iter().flat_map(|f| f.paths()) {
        if !paths.iter().any(|p| p == path) {
            paths.push(path.to_owned());
        }
    }
    paths
}

/// The dialog's file list: the chosen files (whole and partial-hunk), the first [`LISTED_FILES`] named.
fn file_lines(ask: &CommitAsk) -> String {
    let entries = ask
        .files
        .iter()
        .map(|f| match &f.old_path {
            Some(old) => format!("{old} → {}", f.path),
            None => f.path.clone(),
        })
        .chain(
            ask.patches
                .iter()
                .map(|p| format!("{} ({} of {} hunks)", p.path, p.chosen, p.total)),
        );
    let total = ask.files.len() + ask.patches.len();
    let mut lines: Vec<String> = entries.take(LISTED_FILES).collect();
    let more = total.saturating_sub(LISTED_FILES);
    if more > 0 {
        lines.push(format!("… and {more} more"));
    }
    lines.join("\n")
}

/// What the dialog says about where the commit goes, what it leaves out and what stays staged.
pub(super) fn context_lines(ctx: &CommitContext, ask: &CommitAsk) -> Vec<String> {
    let mut lines = Vec::new();
    let chosen = ask.files.len() + ask.patches.len();
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
    if !ask.patches.is_empty() {
        lines.push("Unselected hunks stay unstaged in your working tree.".into());
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

/// The commands a commit runs, as the confirmation shows them, built from the very argument
/// lists the commit runs. Everything but the message depends on the paths alone, so it is
/// written once; each keystroke only quotes the message.
pub(super) struct ShownCommit {
    before_commit: Vec<String>,
    before_message: String,
    after_message: String,
    after_commit: Option<String>,
}

impl ShownCommit {
    pub(super) fn new(whole_paths: &[String], patches: &[HunkPatch]) -> Self {
        use agent_kit::commit::{
            add_args, apply_cached_args, commit_args, index_commit_args, Expected, MESSAGE_ARG,
        };
        if patches.is_empty() {
            let args = commit_args(&CommitRequest {
                message: String::new(),
                paths: whole_paths.to_vec(),
                expected: Expected {
                    branch: None,
                    head: None,
                },
            });
            return Self {
                before_commit: vec![commit_message::shown_command(&add_args(whole_paths))],
                before_message: commit_message::shown_command(&args[..MESSAGE_ARG]),
                after_message: args[MESSAGE_ARG + 1..]
                    .iter()
                    .map(|arg| format!(" {}", commit_message::shown_arg(arg)))
                    .collect(),
                after_commit: None,
            };
        }
        let mut before_commit = vec![format!(
            "{}  # in a temporary GIT_INDEX_FILE",
            commit_message::shown_command(&[
                "--literal-pathspecs".to_owned(),
                "read-tree".to_owned(),
                "HEAD".to_owned(),
            ])
        )];
        if !whole_paths.is_empty() {
            before_commit.push(commit_message::shown_command(&add_args(whole_paths)));
        }
        let apply_cmd = commit_message::shown_command(&apply_cached_args());
        for patch in patches {
            before_commit.push(format!(
                "{apply_cmd}  # {} ({} of {} hunks)",
                patch.path, patch.chosen, patch.total
            ));
        }
        let mut reset_args = vec![
            "--literal-pathspecs".to_owned(),
            "reset".to_owned(),
            "-q".to_owned(),
            "HEAD".to_owned(),
            "--".to_owned(),
        ];
        reset_args.extend(whole_paths.iter().cloned());
        for patch in patches {
            if !whole_paths.iter().any(|p| p == &patch.path) {
                reset_args.push(patch.path.clone());
            }
        }
        let args = index_commit_args("");
        Self {
            before_commit,
            before_message: commit_message::shown_command(&args[..MESSAGE_ARG]),
            after_message: args[MESSAGE_ARG + 1..]
                .iter()
                .map(|arg| format!(" {}", commit_message::shown_arg(arg)))
                .collect(),
            after_commit: Some(commit_message::shown_command(&reset_args)),
        }
    }

    pub(super) fn text(&self, message: &str) -> String {
        let commit_line = format!(
            "{} {}{}",
            self.before_message,
            commit_message::shown_arg(message),
            self.after_message
        );
        let mut parts = self.before_commit.clone();
        parts.push(commit_line);
        if let Some(after) = &self.after_commit {
            parts.push(after.clone());
        }
        parts.join("\n")
    }
}

/// The commit dialog's parts the flow reads back.
pub(super) struct CommitDialog {
    pub dialog: adw::AlertDialog,
    pub message: gtk4::TextView,
    #[cfg_attr(not(test), expect(dead_code, reason = "read by the dialog test"))]
    pub commands: gtk4::Label,
    #[cfg_attr(not(test), expect(dead_code, reason = "read by the dialog test"))]
    pub check: gtk4::Label,
    pub use_reply: gtk4::Button,
    pub reply_hint: gtk4::Label,
}

/// Builds the dialog for committing `ask` with `ctx`, its message box holding `draft`. Commit is
/// on only while the message is not empty; the commands follow the message as it is typed.
/// `can_draft`: the tab is a thread, so "Draft Message" is offered.
pub(super) fn commit_dialog(
    ctx: &CommitContext,
    ask: &CommitAsk,
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

    let use_reply = gtk4::Button::with_label("Use Last Reply");
    use_reply.set_sensitive(false);
    let reply_hint = wrapped("Send the draft prompt and wait for a short completed reply.");
    reply_hint.add_css_class("dim-label");
    if can_draft {
        column.append(&use_reply);
        column.append(&reply_hint);
    }

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

    let whole = whole_paths(ask);
    let shown = ShownCommit::new(&whole, &ask.patches);
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
            commands.set_text(&shown.text(&text));
        }
    );
    update(&message.buffer());
    message.buffer().connect_changed(update);
    CommitDialog {
        dialog,
        message,
        commands,
        check,
        use_reply,
        reply_hint,
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

fn apply_draft_reply(
    message: &gtk4::TextView,
    hint: &gtk4::Label,
    selection_matches: bool,
    typed: &str,
    candidate: &(i64, String),
    found: Option<agent_kit::store::Result<Option<(i64, String)>>>,
) {
    if selection_matches
        && text_of(message) == typed
        && matches!(&found, Some(Ok(Some(reply))) if reply == candidate)
    {
        message.buffer().set_text(&candidate.1);
        hint.set_text("Reply copied; review and edit it before committing.");
    } else {
        hint.set_text("The reply or selection changed, or the thread could not be read. Your message was kept.");
    }
}

type ReplyRead = Option<agent_kit::store::Result<Option<(i64, String)>>>;
type ReplyFuture = std::pin::Pin<Box<dyn std::future::Future<Output = ReplyRead>>>;

fn update_reply_candidate(
    button: &gtk4::Button,
    hint: &gtk4::Label,
    candidate: &std::cell::RefCell<Option<(i64, String)>>,
    selection_matches: bool,
    found: ReplyRead,
) {
    let reply = if selection_matches {
        match found {
            Some(Ok(Some(reply))) => {
                hint.set_text("Copy the completed reply into the message box for review.");
                Some(reply)
            }
            Some(Ok(None)) => {
                hint.set_text("Send the draft prompt and wait for a short completed reply.");
                None
            }
            _ => {
                hint.set_text("Could not read the thread. Retrying; your message was kept.");
                None
            }
        }
    } else {
        hint.set_text("The selection changed. Draft a new message for these files.");
        None
    };
    button.set_sensitive(reply.is_some());
    *candidate.borrow_mut() = reply;
}

fn connect_reply_button(
    button: &gtk4::Button,
    message: &gtk4::TextView,
    hint: &gtk4::Label,
    candidate: std::rc::Rc<std::cell::RefCell<Option<(i64, String)>>>,
    busy: std::rc::Rc<std::cell::Cell<bool>>,
    selection_matches: std::rc::Rc<dyn Fn() -> bool>,
    read: std::rc::Rc<dyn Fn() -> ReplyFuture>,
) {
    let (message, hint) = (message.clone(), hint.clone());
    button.connect_clicked(move |button| {
        let Some(candidate) = candidate.borrow().clone() else {
            return;
        };
        busy.set(true);
        button.set_sensitive(false);
        let (message, hint, selection_matches, read, busy) = (
            message.clone(),
            hint.clone(),
            selection_matches.clone(),
            read.clone(),
            busy.clone(),
        );
        let typed = text_of(&message);
        glib::MainContext::default().spawn_local(async move {
            let found = read().await;
            apply_draft_reply(
                &message,
                &hint,
                selection_matches(),
                &typed,
                &candidate,
                found,
            );
            busy.set(false);
        });
    });
}

impl AgentTerminalWindow {
    /// "Commit…" in the diff panel behind `panel_root`: reads the repository off the main thread,
    /// then asks with the dialog. Too many files, a half-done merge or rebase, or a repository
    /// that cannot be read, is said instead. The panel's button stays off until one or the other.
    pub(super) fn commit_from_panel(&self, panel_root: &gtk4::Box, ask: CommitAsk) {
        let Some((page, panel, view, thread)) = self.tab_by_panel(panel_root, |t| {
            (
                t.page.clone(),
                t.diff_panel.clone(),
                t.chat.as_ref().and_then(|c| c.view.clone()),
                t.chat.as_ref().map(|c| c.thread.clone()),
            )
        }) else {
            return;
        };
        let parent = panel.dialog_parent();
        let paths = ask_paths(&ask);
        if let Err(why) = agent_kit::commit::validate_paths(&paths) {
            panel.set_preparing(false);
            present_message_over(&parent, "Can't Commit These Files Here", &redact(&why));
            return;
        }
        let obj = self.obj().downgrade();
        glib::MainContext::default().spawn_local(async move {
            let read = {
                let (repo, paths) = (ask.repo.clone(), paths.clone());
                off_main("reading the repository", move || {
                    agent_kit::commit::commit_context(&repo, &paths)
                })
                .await
            };
            panel.set_preparing(false);
            let Some(obj) = obj.upgrade() else { return };
            let ctx = match read {
                Ok(ctx) => ctx,
                Err(why) => {
                    present_message_over(&parent, "Could Not Read the Repository", &redact(&why));
                    return;
                }
            };
            if let Some(op) = ctx.in_progress {
                present_message_over(&parent, "Can't Commit Right Now", op.reason());
                return;
            }
            let expected = ctx.expected();
            let parts = commit_dialog(&ctx, &ask, &panel.commit_draft(), view.is_some());
            let whole = whole_paths(&ask);
            let patches = ask.patches.clone();
            let repo = ask.repo.clone();
            let message = parts.message.clone();
            if let (Some(thread), Some(baseline)) = (thread.clone(), panel.draft_request(&ask)) {
                let reply_message = message.clone();
                let button = parts.use_reply.clone();
                let hint = parts.reply_hint.clone();
                let panel_check = panel.clone();
                let ask_check = ask.clone();
                let prompt = commit_message::draft_prompt(&paths);
                let dialog = parts.dialog.downgrade();
                let candidate = std::rc::Rc::new(std::cell::RefCell::new(None));
                let busy = std::rc::Rc::new(std::cell::Cell::new(false));
                let selection = {
                    let (panel, ask) = (panel_check.clone(), ask_check.clone());
                    std::rc::Rc::new(move || panel.draft_request(&ask) == Some(baseline))
                };
                let read = {
                    let (thread, prompt) = (thread.clone(), prompt.clone());
                    std::rc::Rc::new(move || {
                        let (thread, prompt) = (thread.clone(), prompt.clone());
                        Box::pin(async move { store_job(move |s| draft_reply(s, &thread, baseline, &prompt)).await }) as ReplyFuture
                    })
                };
                connect_reply_button(&button, &reply_message, &hint, candidate.clone(), busy.clone(), selection, read);
                glib::MainContext::default().spawn_local(async move {
                    loop {
                        if dialog.upgrade().is_none_or(|d| !d.is_visible()) { return; }
                        if busy.get() {
                            glib::timeout_future_seconds(2).await;
                            continue;
                        }
                        let (thread_read, prompt_read) = (thread.clone(), prompt.clone());
                        let found = store_job(move |s| draft_reply(s, &thread_read, baseline, &prompt_read)).await;
                        if dialog.upgrade().is_none_or(|d| !d.is_visible()) { return; }
                        if busy.get() { continue; }
                        update_reply_candidate(&button, &hint, &candidate, panel_check.draft_request(&ask_check) == Some(baseline), found);
                        glib::timeout_future_seconds(2).await;
                    }
                });
            }
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
                        if let (Some(view), Some(thread)) = (view.clone(), thread.clone()) {
                            panel.clear_draft_request();
                            let (panel, ask, paths, parent) = (panel.clone(), ask.clone(), paths.clone(), parent.clone());
                            let obj = obj.downgrade();
                            glib::MainContext::default().spawn_local(async move {
                                match store_job(move |s| last_seq(s, &thread)).await {
                                    Some(Ok(seq)) => {
                                        panel.set_draft_request(ask, seq);
                                        view.insert_attachment_text(&commit_message::draft_prompt(&paths));
                                        if let Some(obj) = obj.upgrade() { obj.imp().show_toast("A prompt to draft the message is in the thread's composer"); }
                                    }
                                    _ => present_message_over(&parent, "Could Not Read the Thread", "The draft prompt was not inserted. Try again."),
                                }
                            });
                        }
                    }
                    "commit" => {
                        let request = PartialCommitRequest {
                            message: text,
                            paths: whole.clone(),
                            patches: patches.clone(),
                            expected: expected.clone(),
                        };
                        obj.imp()
                            .confirm_and_commit(&page, &panel, &parent, repo.clone(), request);
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
        request: PartialCommitRequest,
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
                agent_kit::commit::commit_partial(&repo, &request)
            })
            .await;
            panel.set_committing(false);
            let Some(obj) = obj.upgrade() else { return };
            match done {
                Ok(done) => {
                    info!("committed from the diff panel");
                    panel.set_commit_draft("");
                    panel.clear_draft_request();
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
    use agent_core::event::{Envelope, Event, ItemKind, ItemStatus, StreamKind, TurnState};
    use std::path::PathBuf;

    #[test]
    fn use_reply_rechecks_store_and_selection_after_click() {
        use std::{cell::Cell, rc::Rc, time::Duration};
        crate::testutil::adw_init().unwrap();
        let store = Rc::new(agent_kit::store::Store::open_in_memory().unwrap());
        let thread = store.create_thread("/tmp", None).unwrap();
        let prompt = commit_message::draft_prompt(&["a.rs".into()]);
        let append = |event: Envelope| store.append_event(&thread, None, &event).unwrap();
        let start = |id: &str, kind| {
            Envelope::new(Event::ItemStarted {
                kind,
                title: String::new(),
                input: None,
                parent: None,
            })
            .item(id)
        };
        let text = |id: &str, value: &str| {
            Envelope::new(Event::ContentSnapshot {
                stream: StreamKind::Assistant,
                text: value.into(),
            })
            .item(id)
        };
        let end = |id: &str| {
            Envelope::new(Event::ItemCompleted {
                status: ItemStatus::Completed,
                output: None,
                error: None,
            })
            .item(id)
        };
        append(start("u", ItemKind::UserMessage));
        append(text("u", &prompt));
        append(end("u"));
        append(Envelope::new(Event::TurnStarted { model: None }));
        append(start("a", ItemKind::AssistantMessage));
        append(text("a", "Fix the issue"));
        append(end("a"));
        append(Envelope::turn_completed(TurnState::Completed, None));
        let candidate = draft_reply(&store, &thread, 0, &prompt).unwrap().unwrap();
        let context = glib::MainContext::default();

        for scenario in ["success", "selection", "typed", "error", "reply"] {
            let button = gtk4::Button::with_label("Use Last Reply");
            let message = gtk4::TextView::new();
            message.buffer().set_text("My draft");
            let hint = gtk4::Label::new(None);
            let selected = Rc::new(Cell::new(true));
            let failing = Rc::new(Cell::new(scenario == "error"));
            let available = Rc::new(std::cell::RefCell::new(Some(candidate.clone())));
            let busy = Rc::new(Cell::new(false));
            let selection = {
                let selected = selected.clone();
                Rc::new(move || selected.get())
            };
            let read = {
                let (store, thread, prompt, failing) = (
                    store.clone(),
                    thread.clone(),
                    prompt.clone(),
                    failing.clone(),
                );
                Rc::new(move || {
                    let (store, thread, prompt, failing) = (
                        store.clone(),
                        thread.clone(),
                        prompt.clone(),
                        failing.clone(),
                    );
                    Box::pin(async move {
                        glib::timeout_future(Duration::from_millis(10)).await;
                        if failing.get() {
                            None
                        } else {
                            Some(draft_reply(&store, &thread, 0, &prompt))
                        }
                    }) as ReplyFuture
                })
            };
            connect_reply_button(
                &button,
                &message,
                &hint,
                available.clone(),
                busy.clone(),
                selection,
                read,
            );
            button.emit_clicked();
            match scenario {
                "selection" => selected.set(false),
                "reply" => {
                    append(text("a", "Changed after the first read"));
                }
                "typed" => message.buffer().set_text("Edited during read"),
                _ => {}
            }
            assert!(crate::testutil::pump_until(&context, 2, || !hint
                .text()
                .is_empty()));
            let expected = match scenario {
                "success" => "Fix the issue",
                "typed" => "Edited during read",
                _ => "My draft",
            };
            assert_eq!(text_of(&message), expected, "{scenario}");
            if scenario == "error" {
                failing.set(false);
                update_reply_candidate(
                    &button,
                    &hint,
                    &available,
                    true,
                    Some(Ok(Some(candidate.clone()))),
                );
                assert!(button.is_sensitive());
                button.emit_clicked();
                assert!(crate::testutil::pump_until(&context, 2, || !busy.get()));
                assert_eq!(
                    text_of(&message),
                    "Fix the issue",
                    "retry after a failed read"
                );
            }
        }
    }

    #[test]
    fn draft_reply_requires_the_latest_completed_matching_turn() {
        let store = agent_kit::store::Store::open_in_memory().unwrap();
        let thread = store.create_thread("/tmp", None).unwrap();
        let append = |event: Envelope| store.append_event(&thread, None, &event).unwrap();
        let prompt = commit_message::draft_prompt(&["a.rs".into()]);
        let start = |id: &str, kind| {
            Envelope::new(Event::ItemStarted {
                kind,
                title: String::new(),
                input: None,
                parent: None,
            })
            .item(id)
        };
        let text = |id: &str, text: &str| {
            Envelope::new(Event::ContentSnapshot {
                stream: StreamKind::Assistant,
                text: text.into(),
            })
            .item(id)
        };
        let end = |id: &str, status| {
            Envelope::new(Event::ItemCompleted {
                status,
                output: None,
                error: None,
            })
            .item(id)
        };
        let turn = |state| {
            Envelope::new(Event::TurnCompleted {
                state,
                usage: None,
                cost_usd: None,
                error: None,
                model: None,
                billing: None,
            })
        };
        append(start("u", ItemKind::UserMessage));
        append(text("u", &prompt));
        append(end("u", ItemStatus::Completed));
        append(Envelope::new(Event::TurnStarted { model: None }));
        append(start("a", ItemKind::AssistantMessage));
        append(text("a", "Fix the issue"));
        assert!(draft_reply(&store, &thread, 0, &prompt).unwrap().is_none());
        append(end("a", ItemStatus::Completed));
        append(turn(TurnState::Interrupted));
        assert!(draft_reply(&store, &thread, 0, &prompt).unwrap().is_none());
        append(Envelope::new(Event::TurnStarted { model: None }));
        append(start("b", ItemKind::AssistantMessage));
        append(text("b", "Fix the issue"));
        append(end("b", ItemStatus::Completed));
        append(turn(TurnState::Completed));
        assert_eq!(
            draft_reply(&store, &thread, 0, &prompt).unwrap().unwrap().1,
            "Fix the issue"
        );
        assert!(draft_reply(&store, &thread, 0, "another prompt")
            .unwrap()
            .is_none());
        let after = last_seq(&store, &thread).unwrap();
        assert!(draft_reply(&store, &thread, after, &prompt)
            .unwrap()
            .is_none());
        append(text("b", &"é".repeat(REPLY_LIMIT + 1)));
        assert!(draft_reply(&store, &thread, 0, &prompt).unwrap().is_none());
        append(start("u2", ItemKind::UserMessage));
        append(text("u2", "Something else"));
        assert!(draft_reply(&store, &thread, 0, &prompt).unwrap().is_none());
        let baseline = last_seq(&store, &thread).unwrap();
        append(start("queued", ItemKind::UserMessage));
        append(text("queued", &prompt));
        append(end("queued", ItemStatus::Completed));
        append(turn(TurnState::Completed));
        append(Envelope::new(Event::TurnStarted { model: None }));
        append(start("earlier", ItemKind::AssistantMessage));
        append(text("earlier", "An earlier queued answer"));
        append(end("earlier", ItemStatus::Completed));
        append(turn(TurnState::Completed));
        assert!(draft_reply(&store, &thread, baseline, &prompt)
            .unwrap()
            .is_none());
    }

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
            patches: Vec::new(),
            listed,
        }
    }

    fn ctx(branch: Option<&str>, staged: &[&str]) -> CommitContext {
        CommitContext {
            toplevel: PathBuf::from("/r"),
            branch: branch.map(str::to_owned),
            head: Some("abc".into()),
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
        use agent_kit::commit::{add_args, apply_cached_args, commit_args, index_commit_args};
        let paths: Vec<String> = vec!["a.rs".into(), "-odd".into(), "it's *".into()];
        let shown = ShownCommit::new(&paths, &[]);
        for message in ["Fix it\n\nWhy.", "-n", "it's $HOME", ""] {
            let request = CommitRequest {
                message: message.into(),
                paths: paths.clone(),
                expected: ctx(None, &[]).expected(),
            };
            // Exactly the argument lists the commit runs, written as a shell would read them.
            assert_eq!(
                shown.text(message),
                format!(
                    "{}\n{}",
                    commit_message::shown_command(&add_args(&request.paths)),
                    commit_message::shown_command(&commit_args(&request))
                ),
                "{message:?}"
            );
        }
        let patches = vec![HunkPatch {
            path: "b.rs".into(),
            patch: "--- a/b.rs\n+++ b/b.rs\n@@ -1 +1 @@\n-a\n+b\n".into(),
            chosen: 1,
            total: 2,
        }];
        let shown_partial = ShownCommit::new(&paths, &patches);
        let text = shown_partial.text("Partial fix");
        assert!(text.contains("read-tree HEAD"));
        assert!(text.contains(&commit_message::shown_command(&add_args(&paths))));
        assert!(text.contains(&commit_message::shown_command(&apply_cached_args())));
        assert!(text.contains("b.rs (1 of 2 hunks)"));
        assert!(
            text.contains(&commit_message::shown_command(&index_commit_args(
                "Partial fix"
            )))
        );
        assert!(text.contains("reset -q HEAD --"));

        assert!(check_words(&commit_message::check("ok")).is_empty());
        assert!(check_words(&commit_message::check("")).contains("Write a message"));
    }

    #[test]
    fn partial_hunk_ask_lists_hunk_counts_and_paths() {
        let mut a = ask(&[("whole.rs", None)], 2);
        a.patches.push(HunkPatch {
            path: "part.rs".into(),
            patch: "p".into(),
            chosen: 2,
            total: 3,
        });
        assert_eq!(whole_paths(&a), vec!["whole.rs"]);
        assert_eq!(ask_paths(&a), vec!["whole.rs", "part.rs"]);
        assert_eq!(file_lines(&a), "whole.rs\npart.rs (2 of 3 hunks)");
        let lines = context_lines(&ctx(Some("feat/x"), &[]), &a);
        assert!(lines
            .iter()
            .any(|l| l == "Unselected hunks stay unstaged in your working tree."));
    }

    /// GTK: run from the window's construction test. Commit is off while the message is empty
    /// and the commands follow what is typed; the draft comes back; the branch warning shows.
    pub(crate) fn the_commit_dialog_follows_its_message() {
        let a = ask(&[("a.rs", None)], 1);
        let paths = whole_paths(&a);
        let parts = commit_dialog(&ctx(Some("main"), &[]), &a, "", true);
        assert!(!parts.dialog.is_response_enabled("commit"));
        assert!(parts.dialog.has_response("draft"));
        assert!(!parts.use_reply.is_sensitive());
        assert!(parts.reply_hint.text().contains("completed reply"));
        let candidate = (1, "Original reply".to_owned());
        parts.message.buffer().set_text("My typed draft");
        apply_draft_reply(
            &parts.message,
            &parts.reply_hint,
            true,
            "My typed draft",
            &candidate,
            Some(Ok(Some((1, "Changed reply".into())))),
        );
        assert_eq!(
            text_of(&parts.message),
            "My typed draft",
            "a changed reply cannot replace a draft"
        );
        apply_draft_reply(
            &parts.message,
            &parts.reply_hint,
            false,
            "My typed draft",
            &candidate,
            Some(Ok(Some(candidate.clone()))),
        );
        assert_eq!(
            text_of(&parts.message),
            "My typed draft",
            "a changed selection cannot replace a draft"
        );
        apply_draft_reply(
            &parts.message,
            &parts.reply_hint,
            true,
            "My typed draft",
            &candidate,
            Some(Ok(Some(candidate.clone()))),
        );
        assert_eq!(text_of(&parts.message), "Original reply");
        parts.message.buffer().set_text("");
        assert!(parts.check.text().contains("Write a message"));
        parts.message.buffer().set_text("Fix it");
        assert!(parts.dialog.is_response_enabled("commit"));
        assert_eq!(
            parts.commands.text().as_str(),
            ShownCommit::new(&paths, &[]).text("Fix it")
        );
        assert!(!parts.check.is_visible());

        let parts = commit_dialog(&ctx(None, &[]), &a, "Kept draft", false);
        assert_eq!(text_of(&parts.message), "Kept draft");
        assert!(parts.dialog.is_response_enabled("commit"));
        assert!(!parts.dialog.has_response("draft"), "no thread, no draft");
    }
}

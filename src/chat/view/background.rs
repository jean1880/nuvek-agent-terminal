//! The background task explorer: a shelf above the composer listing what runs in the background
//! (shell commands, and sub-agents the sub-agent shelf does not already show), and a dialog that
//! follows one shell task's output.
//!
//! A row opens a sub-agent in the sub-agent panel; a shell task opens [`TaskOutputPanel`],
//! which the view fills by polling the agent (`Control::TaskOutput`) while the dialog is open.
//! The agent hands back only the end of the output (Claude: the last 8 KiB), so the panel says
//! when it shows part of it.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::AdwDialogExt;
use agent_core::event::{BackgroundTask, BackgroundTaskKind};
use gtk4::prelude::*;

use super::cards::{label, wrapped_text};
use super::model::task_name;
use super::panels::{follow_dialog, scrolled};
use super::payload::TaskOutput;
use super::shelf::Shelf;
use super::subagents::set_if_changed;

/// What a task is, as its row says.
pub fn kind_text(kind: BackgroundTaskKind) -> &'static str {
    match kind {
        BackgroundTaskKind::Agent => "Sub-agent",
        BackgroundTaskKind::Shell => "Shell command",
        BackgroundTaskKind::Other => "Task",
    }
}

fn kind_icon(kind: BackgroundTaskKind) -> &'static str {
    match kind {
        BackgroundTaskKind::Agent => "agent-subagent-symbolic",
        BackgroundTaskKind::Shell | BackgroundTaskKind::Other => "at-utilities-terminal-symbolic",
    }
}

/// One task as the list shows it; `openable`: selecting it shows something.
#[derive(Debug, Clone, PartialEq)]
pub struct ListedTask {
    pub task: BackgroundTask,
    pub openable: bool,
}

/// The shelf's one-line summary.
pub fn title_text(n: usize) -> String {
    match n {
        1 => "1 task in the background".to_owned(),
        n => format!("{n} tasks in the background"),
    }
}

/// What a task's row says under its name.
fn now_text(listed: &ListedTask) -> &'static str {
    match (listed.openable, listed.task.kind) {
        (true, BackgroundTaskKind::Agent) => "Open to follow this sub-agent",
        (true, _) => "Open to follow its output",
        (false, _) => "Nothing to show",
    }
}

/// The background tasks' shelf above the composer, stacked with the sub-agents' and built from
/// the same parts (`shelf.rs`): a one-line summary, collapsed by default; opened, a row per task
/// that runs, which opens its output (or its sub-agent). Shown while any runs.
pub struct BackgroundShelf {
    shelf: Shelf,
    tasks: RefCell<Vec<ListedTask>>,
}

impl BackgroundShelf {
    /// `open` is called with the task the user picked.
    pub fn new(open: impl Fn(&BackgroundTask) + 'static) -> Rc<Self> {
        let shelf = Shelf::new(
            "at-utilities-terminal-symbolic",
            "what runs in the background",
            None,
        );
        shelf.fold.card.add_css_class("background-shelf");
        let this = Rc::new(Self {
            shelf,
            tasks: RefCell::new(Vec::new()),
        });
        let weak = Rc::downgrade(&this);
        this.shelf.list.connect_row_activated(move |_, row| {
            let Some(this) = weak.upgrade() else { return };
            let picked = usize::try_from(row.index())
                .ok()
                .and_then(|i| this.tasks.borrow().get(i).cloned())
                .filter(|t| t.openable);
            if let Some(picked) = picked {
                open(&picked.task);
            }
        });
        this
    }

    /// The shelf, for the stack above the composer.
    pub fn revealer(&self) -> &gtk4::Revealer {
        &self.shelf.revealer
    }

    /// Shows `tasks`, or hides the shelf when there are none. Rows are rebuilt only when the
    /// list changed.
    pub fn set(&self, tasks: Vec<ListedTask>) {
        self.shelf.show(!tasks.is_empty());
        if tasks.is_empty() || *self.tasks.borrow() == tasks {
            *self.tasks.borrow_mut() = tasks;
            return;
        }
        set_if_changed(&self.shelf.title, &title_text(tasks.len()));
        let rows = self.shelf.rebuild(tasks.len());
        for (row, listed) in rows.iter().zip(&tasks) {
            let name = task_name(&listed.task);
            let kind = kind_text(listed.task.kind);
            let now = now_text(listed);
            row.mark.set_icon_name(Some(kind_icon(listed.task.kind)));
            row.set_running(true, "");
            row.set_text(&name, kind, now);
            row.describe(
                &format!("{name}\n{kind}\n{now}"),
                &name,
                &format!("{kind}; {now}"),
            );
            row.row.set_activatable(listed.openable);
        }
        *self.tasks.borrow_mut() = tasks;
    }

    /// Opens or closes the shelf's list (demo scenes).
    pub(crate) fn set_expanded(&self, expanded: bool) {
        self.shelf.set_expanded(expanded);
    }

    /// Whether the shelf shows (tests).
    #[cfg(test)]
    pub fn shown(&self) -> bool {
        self.shelf.shown()
    }

    /// The listed task names and whether each opens, in order (tests).
    #[cfg(test)]
    pub fn listed(&self) -> Vec<(String, bool)> {
        self.tasks
            .borrow()
            .iter()
            .map(|t| (task_name(&t.task), t.openable))
            .collect()
    }
}

/// One background shell task's output, in a dialog that follows it until closed.
pub struct TaskOutputPanel {
    dialog: adw::Dialog,
    task_id: String,
    status: gtk4::Label,
    note: gtk4::Label,
    buffer: gtk4::TextBuffer,
    /// A read is out: the next poll waits for it.
    in_flight: Cell<bool>,
    /// The task has left the list; one last read, then the poll stops.
    ended: Cell<bool>,
    closed: Cell<bool>,
}

impl TaskOutputPanel {
    pub fn new(task: &BackgroundTask) -> Rc<Self> {
        let body = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
        body.set_margin_top(12);
        body.set_margin_bottom(18);
        body.set_margin_start(18);
        body.set_margin_end(18);
        let title = wrapped_text(&task_name(task), &["subagent-panel-title"]);
        title.set_selectable(true);
        let status = label("Reading output…", &["dim-label"]);
        body.append(&title);
        body.append(&status);
        let buffer = gtk4::TextBuffer::new(None);
        let view = gtk4::TextView::with_buffer(&buffer);
        view.set_editable(false);
        view.set_cursor_visible(false);
        view.set_monospace(true);
        view.set_wrap_mode(gtk4::WrapMode::WordChar);
        view.add_css_class("code-view");
        view.set_top_margin(10);
        view.set_bottom_margin(10);
        view.set_left_margin(12);
        view.set_right_margin(12);
        let scroller = scrolled(&view);
        scroller.add_css_class("code-block");
        // Opens on the newest output and follows it live.
        super::follow::follow_end(&scroller);
        body.append(&scroller);
        let note = label("", &["dim-label", "caption"]);
        note.set_visible(false);
        body.append(&note);
        let dialog = follow_dialog(&task_name(task), 760, 560, &body);
        let this = Rc::new(Self {
            dialog,
            task_id: task.id.clone(),
            status,
            note,
            buffer,
            in_flight: Cell::new(false),
            ended: Cell::new(false),
            closed: Cell::new(false),
        });
        let weak = Rc::downgrade(&this);
        this.dialog.connect_closed(move |_| {
            if let Some(this) = weak.upgrade() {
                this.closed.set(true);
            }
        });
        this
    }

    pub fn dialog(&self) -> &adw::Dialog {
        &self.dialog
    }

    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    pub fn closed(&self) -> bool {
        self.closed.get()
    }

    /// Starts a read unless one is out; false when it should not (one is out, or the panel is
    /// done).
    pub fn begin_read(&self) -> bool {
        if self.closed.get() || self.in_flight.get() {
            return false;
        }
        self.in_flight.set(true);
        true
    }

    /// The task left the background list: the next read is the last.
    pub fn mark_ended(&self) {
        self.ended.set(true);
    }

    pub fn ended(&self) -> bool {
        self.ended.get()
    }

    /// Shows a read's result: the output (following its end unless the user scrolled up), or
    /// why it could not be read.
    pub fn show(&self, result: Result<TaskOutput, String>) {
        self.in_flight.set(false);
        let state = if self.ended.get() {
            "Finished"
        } else {
            "Running · updates every second"
        };
        match result {
            Ok(out) => {
                let shown = {
                    let (start, end) = self.buffer.bounds();
                    self.buffer.text(&start, &end, false).to_string()
                };
                // An ended task whose output file is gone reads as empty: keep what was shown.
                if out.text.is_empty() && !shown.is_empty() {
                    self.status.set_text(state);
                    return;
                }
                let text = plain_text(&out.text);
                // The scroller follows the end itself (`follow::follow_end`). New output is
                // appended rather than the whole text replaced, which would empty the view for a
                // moment and throw a reader who scrolled up back to the end.
                if shown != text {
                    match text.strip_prefix(shown.as_str()) {
                        Some(added) => self.buffer.insert(&mut self.buffer.end_iter(), added),
                        None => self.buffer.set_text(&text),
                    }
                }
                self.status.set_text(if text.is_empty() {
                    if self.ended.get() {
                        "Finished without output"
                    } else {
                        "Running · no output yet"
                    }
                } else {
                    state
                });
                let note = size_note(&out);
                self.note.set_visible(note.is_some());
                self.note.set_text(note.as_deref().unwrap_or_default());
            }
            Err(error) => {
                self.status
                    .set_text(&format!("Could not read the output: {error}"));
            }
        }
    }

    /// The shown text and status (tests).
    #[cfg(test)]
    pub fn shown(&self) -> (String, String) {
        let (start, end) = self.buffer.bounds();
        (
            self.buffer.text(&start, &end, false).to_string(),
            self.status.text().to_string(),
        )
    }
}

/// "Showing the last 8.0 KiB of 120.4 KiB" when the reply holds only the end of the output.
pub fn size_note(out: &TaskOutput) -> Option<String> {
    if !out.truncated {
        return None;
    }
    let kib = |b: usize| format!("{:.1} KiB", b as f64 / 1024.0);
    Some(match out.total_bytes {
        Some(total) => format!(
            "Showing the last {} of {}",
            kib(out.text.len()),
            kib(usize::try_from(total).unwrap_or(usize::MAX))
        ),
        None => format!("Showing the last {}", kib(out.text.len())),
    })
}

/// Terminal output as plain text: escape sequences (colours, cursor moves, titles) dropped, and
/// a line redrawn with carriage returns (a progress bar) kept as its last state.
pub fn plain_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI: parameters and intermediates, then one final byte in @..~.
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            // OSC (titles, hyperlinks): up to BEL or ST (ESC \).
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            // A two-byte sequence (charset selection, keypad mode, ...).
            _ => {}
        }
    }
    out.split('\n')
        .map(|line| {
            let line = line.strip_suffix('\r').unwrap_or(line);
            line.rsplit('\r').next().unwrap_or(line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_drops_escapes_and_keeps_a_redrawn_line_last_state() {
        assert_eq!(
            plain_text("\u{1b}[1;32mok\u{1b}[0m done\r\n"),
            "ok done\n",
            "colours go, CRLF is a newline"
        );
        assert_eq!(
            plain_text("\u{1b}]0;title\u{7}a\u{1b}]8;;http://x\u{1b}\\b"),
            "ab",
            "titles and hyperlinks go"
        );
        assert_eq!(plain_text(" 10%\r 50%\r100%\nnext"), "100%\nnext");
        assert_eq!(plain_text("é ✓ plain"), "é ✓ plain");
        assert_eq!(
            plain_text("cut \u{1b}["),
            "cut ",
            "a cut-off sequence ends the text"
        );
    }

    #[test]
    fn size_note_speaks_only_for_a_cut_output() {
        let mut out = TaskOutput {
            text: "x".repeat(8192),
            total_bytes: Some(123_290),
            truncated: false,
        };
        assert_eq!(size_note(&out), None);
        out.truncated = true;
        assert_eq!(
            size_note(&out).as_deref(),
            Some("Showing the last 8.0 KiB of 120.4 KiB")
        );
        out.total_bytes = None;
        assert_eq!(size_note(&out).as_deref(), Some("Showing the last 8.0 KiB"));
    }
}

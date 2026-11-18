//! Comments on diff lines, gathered into one review for the thread's agent.
//!
//! A file's diff view takes a selection (or, by keyboard, the caret's line): "Comment" (or
//! Ctrl+Alt+M) maps it to file lines (`agent_kit::diff::comment_span`) and opens a note editor.
//! The comments are listed under their file's diff, their lines marked (a background and an
//! indent, so not by colour alone), and counted in the review bar, whose "Send Review" composes
//! them into one message (`agent_core::review::compose`: redacted, fenced, within the hand-off
//! budget) that the window queues to the panel's own thread.
//!
//! Drafts belong to the thread, not the window: the window loads them from the store when the
//! panel is bound to its thread, and every change hands the whole list back to be written
//! (redacted) through the store writer. They are keyed by path, so they survive a refresh and a
//! change of base; each diff shown re-anchors them (`agent_kit::diff::anchor`), and one whose
//! lines are not found is kept and flagged "not in this diff". Comments made on the thread's
//! file-change cards join the same list ([`Review::add_from_card`]).
//!
//! "Send Review" first has the window look for each new-side comment in its working file, off
//! the main thread (`agent_kit::review_check`), the button spinning meanwhile; the message then
//! names the lines where they are now, or says they moved, went, or could not be checked.

use crate::diff::{Anchor, CommentSpan, DiffBase, Located, Side};
use crate::review_popover::{chosen_rows, note_editor, refusal, selection_rect};
use crate::theme::DiffColours;
use adw::prelude::*;
use agent_core::review::{Checked, Composed, ReviewComment};
use agent_kit::review_check::CheckAsk;
use agent_kit::store::ReviewDraft;
use gtk4::glib;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

pub(super) use crate::review_popover::KEYS_NAME;

/// Tag of the lines a comment is on.
pub(super) const TAG_COMMENTED: &str = "commented";
/// How far a commented line is indented beyond the view's own margin: the mark that is not a
/// colour.
const COMMENTED_INDENT: i32 = 14;
/// How opaque the commented lines' background is.
const COMMENTED_ALPHA: f32 = 0.22;

/// What the bar says, for as long as the panel lives, when the stored drafts could not be read.
pub(super) const LOAD_FAILED: &str = "Saved comments could not be read; new ones are not saved";

/// What "Comment" says while it cannot be pressed.
pub(super) const SELECT_FIRST: &str = "Select lines in this file's diff to comment on them";

/// Writes `thread`'s whole draft list (already redacted).
pub(super) type PersistHandler = Rc<dyn Fn(String, Vec<ReviewDraft>)>;
/// Queues a composed review to the thread; `Err` says why it could not.
pub(super) type SendHandler = Rc<dyn Fn(String) -> Result<(), String>>;
/// What a check of the working files answers: one result per ask, in order.
pub(super) type CheckDone = Box<dyn FnOnce(Vec<Checked>)>;
/// Looks for each comment in its working file, off the main thread, and answers on it.
pub(super) type CheckHandler = Rc<dyn Fn(Vec<CheckAsk>, CheckDone)>;

/// A row's Comment button, held weakly (its row owns it), with the row's path.
type RowButton = (glib::WeakRef<gtk4::Button>, String);

/// One comment, with the panel's own id for it.
#[derive(Debug, Clone)]
struct Entry {
    id: u64,
    draft: ReviewDraft,
}

/// A file view built so far: what its marks and comment list are drawn from.
struct FileMarks {
    path: String,
    chunk: Rc<str>,
    view: glib::WeakRef<gtk4::TextView>,
    list: glib::WeakRef<gtk4::Box>,
    /// The box holding both: what the file's popovers are parented to. A box lets go of any
    /// child it still has when it goes (a text view would not, and a popover on one can outlive
    /// it), and lays no popover out.
    column: glib::WeakRef<gtk4::Box>,
}

/// The review's state, shared by the panel and the handlers of its widgets. Cheap to clone:
/// every field is a reference, and widgets are held weakly (the panel's tree owns them).
#[derive(Clone)]
pub(super) struct Review {
    /// The thread drafts belong to; `None`: a tab with no thread, and no commenting.
    thread: Rc<RefCell<Option<String>>>,
    /// The stored drafts have been read (or there were none to read): until then nothing is
    /// written, which would replace them.
    loaded: Rc<Cell<bool>>,
    entries: Rc<RefCell<Vec<Entry>>>,
    next_id: Rc<Cell<u64>>,
    /// Each file's diff on show, by path.
    chunks: Rc<RefCell<HashMap<String, Rc<str>>>>,
    /// The base of the diff on show.
    base: Rc<Cell<DiffBase>>,
    files: Rc<RefCell<Vec<FileMarks>>>,
    /// Every row's Comment button, with its path.
    buttons: Rc<RefCell<Vec<RowButton>>>,
    /// A line the bar shows until the next change: how the last send went.
    notice: Rc<RefCell<Option<String>>>,
    persist: Rc<RefCell<Option<PersistHandler>>>,
    send: Rc<RefCell<Option<SendHandler>>>,
    check: Rc<RefCell<Option<CheckHandler>>>,
    /// The working files are being checked for a send: the button spins and nothing else sends.
    checking: Rc<Cell<bool>>,
    /// The stored drafts could not be read: nothing is saved for the rest of the panel's life,
    /// and the bar says so for as long.
    load_failed: Rc<Cell<bool>>,
    colours: Rc<RefCell<DiffColours>>,
    bar: glib::WeakRef<gtk4::Box>,
    orphans: glib::WeakRef<gtk4::Box>,
    status: glib::WeakRef<gtk4::Label>,
    send_button: glib::WeakRef<gtk4::Button>,
    send_face: glib::WeakRef<gtk4::Stack>,
    spinner: glib::WeakRef<gtk4::Spinner>,
    discard_button: glib::WeakRef<gtk4::Button>,
}

/// `1 comment`, `3 comments`.
pub(crate) fn count_words(n: usize) -> String {
    if n == 1 {
        "1 comment".to_owned()
    } else {
        format!("{n} comments")
    }
}

/// What the bar says for `total` comments of which `gone` are not in the diff on show.
pub(super) fn bar_words(total: usize, gone: usize) -> String {
    let mut words = count_words(total);
    if gone > 0 {
        words.push_str(&format!(" · {gone} not in this diff"));
    }
    words
}

/// A comment's place as its list row says it.
fn place_words(side: Side, start: u32, end: u32) -> String {
    let lines = if start == end {
        format!("Line {start}")
    } else {
        format!("Lines {start}–{end}")
    };
    match side {
        Side::New => lines,
        Side::Old => format!("{lines} (removed)"),
    }
}

/// What the bar says after a send of what the working files showed, when any comment was not
/// at its lines: `Checked against the files: 1 moved, 2 no longer there, 1 not checked`.
pub(super) fn recheck_words(checks: impl Iterator<Item = Checked>) -> Option<String> {
    let (mut moved, mut gone, mut unchecked) = (0, 0, 0);
    for check in checks {
        match check {
            Checked::Exact => {}
            Checked::Moved { .. } => moved += 1,
            Checked::Gone => gone += 1,
            Checked::Unchecked(_) => unchecked += 1,
        }
    }
    let parts: Vec<String> = [
        (moved, "moved"),
        (gone, "no longer there"),
        (unchecked, "not checked"),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, what)| format!("{n} {what}"))
    .collect();
    (!parts.is_empty()).then(|| format!("Checked against the files: {}", parts.join(", ")))
}

/// The current time in epoch milliseconds.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// The review bar's widgets.
pub(super) struct Bar {
    /// The whole region under the diff: the comments on files not in it, then the bar's row.
    pub(super) region: gtk4::Box,
    /// Comments on files the diff on show does not list, each with Edit and Delete.
    pub(super) orphans: gtk4::Box,
    pub(super) status: gtk4::Label,
    pub(super) send: gtk4::Button,
    /// The send button's face: its label, or a spinner while the files are checked.
    pub(super) send_face: gtk4::Stack,
    pub(super) spinner: gtk4::Spinner,
    pub(super) discard: gtk4::Button,
}

/// What the send button says while the working files are checked.
pub(super) const CHECKING: &str = "Checking lines…";

/// The review bar: the comments on files not in the diff, the count, "Send Review" and
/// "Discard".
pub(super) fn bar() -> Bar {
    let status = gtk4::Label::builder()
        .xalign(0.0)
        .hexpand(true)
        .wrap(true)
        .wrap_mode(gtk4::pango::WrapMode::WordChar)
        .css_classes(["dim-label", "caption"])
        .build();
    let busy = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(6)
        .halign(gtk4::Align::Center)
        .build();
    let spinner = gtk4::Spinner::new();
    busy.append(&spinner);
    busy.append(&gtk4::Label::new(Some(CHECKING)));
    let send_face = gtk4::Stack::builder()
        .transition_type(gtk4::StackTransitionType::Crossfade)
        .build();
    send_face.add_named(
        &adw::ButtonContent::builder()
            .icon_name("at-chat-message-new-symbolic")
            .label("Send Review")
            .build(),
        Some("idle"),
    );
    send_face.add_named(&busy, Some("busy"));
    let send = gtk4::Button::builder()
        .child(&send_face)
        .valign(gtk4::Align::Center)
        .tooltip_text(
            "Send every comment to this thread's agent as one message (queued while it works)",
        )
        .css_classes(["pill", "suggested-action", "diff-send-review"])
        .build();
    send.update_property(&[gtk4::accessible::Property::Label("Send review")]);
    let discard = gtk4::Button::builder()
        .label("Discard")
        .valign(gtk4::Align::Center)
        .tooltip_text("Delete every comment of this review")
        .css_classes(["flat", "diff-discard-review"])
        .build();
    let row = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(8)
        .build();
    row.append(&status);
    row.append(&discard);
    row.append(&send);
    let orphans = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(4)
        .visible(false)
        .build();
    orphans.add_css_class("diff-comments");
    orphans.update_property(&[gtk4::accessible::Property::Label(
        "Comments on files not in this diff",
    )]);
    let region = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(6)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(8)
        .margin_end(8)
        .visible(false)
        .build();
    region.add_css_class("diff-review-bar");
    region.append(&orphans);
    region.append(&row);
    Bar {
        region,
        orphans,
        status,
        send,
        send_face,
        spinner,
        discard,
    }
}

impl Review {
    pub(super) fn new(bar: &Bar, colours: Rc<RefCell<DiffColours>>) -> Self {
        Self {
            thread: Rc::new(RefCell::new(None)),
            loaded: Rc::new(Cell::new(false)),
            entries: Rc::new(RefCell::new(Vec::new())),
            next_id: Rc::new(Cell::new(1)),
            chunks: Rc::new(RefCell::new(HashMap::new())),
            base: Rc::new(Cell::new(DiffBase::default())),
            files: Rc::new(RefCell::new(Vec::new())),
            buttons: Rc::new(RefCell::new(Vec::new())),
            notice: Rc::new(RefCell::new(None)),
            persist: Rc::new(RefCell::new(None)),
            send: Rc::new(RefCell::new(None)),
            check: Rc::new(RefCell::new(None)),
            checking: Rc::new(Cell::new(false)),
            load_failed: Rc::new(Cell::new(false)),
            colours,
            bar: bar.region.downgrade(),
            orphans: bar.orphans.downgrade(),
            status: bar.status.downgrade(),
            send_button: bar.send.downgrade(),
            send_face: bar.send_face.downgrade(),
            spinner: bar.spinner.downgrade(),
            discard_button: bar.discard.downgrade(),
        }
    }

    pub(super) fn set_check(&self, handler: CheckHandler) {
        *self.check.borrow_mut() = Some(handler);
    }

    pub(super) fn set_persist(&self, handler: PersistHandler) {
        *self.persist.borrow_mut() = Some(handler);
    }

    pub(super) fn set_send(&self, handler: SendHandler) {
        *self.send.borrow_mut() = Some(handler);
        self.sync_bar();
    }

    /// Binds the review to `thread`. True when that is new (the caller then loads its drafts);
    /// a panel's thread never changes, so a second, different thread is refused.
    pub(super) fn bind(&self, thread: &str) -> bool {
        let fresh = {
            let mut bound = self.thread.borrow_mut();
            match bound.as_deref() {
                Some(_) => false,
                None => {
                    *bound = Some(thread.to_owned());
                    true
                }
            }
        };
        if fresh {
            self.sync_buttons();
            self.sync_bar();
        }
        fresh
    }

    pub(super) fn thread(&self) -> Option<String> {
        self.thread.borrow().clone()
    }

    /// Takes the drafts the store holds for `thread`. Comments made before they arrived are
    /// kept with them, and the merged list is written back.
    pub(super) fn restore(&self, thread: &str, stored: Vec<ReviewDraft>) {
        // After a failed read nothing is written for the panel's life, as the bar says, so a
        // late answer is not taken either.
        if self.thread.borrow().as_deref() != Some(thread)
            || self.loaded.get()
            || self.load_failed.get()
        {
            return;
        }
        self.loaded.set(true);
        let made_here = !self.entries.borrow().is_empty();
        {
            let mut entries = self.entries.borrow_mut();
            for draft in stored {
                let id = self.next_id.get();
                self.next_id.set(id + 1);
                entries.push(Entry { id, draft });
            }
        }
        self.sort();
        if made_here {
            self.save();
        }
        self.render_all();
    }

    /// The store could not be read: the drafts it holds are left alone (nothing is written over
    /// them), and comments made now stay in this panel only.
    pub(super) fn load_failed(&self, thread: &str) {
        if self.thread.borrow().as_deref() == Some(thread) && !self.loaded.get() {
            self.load_failed.set(true);
            self.sync_bar();
        }
    }

    fn sort(&self) {
        self.entries.borrow_mut().sort_by(|a, b| {
            (&a.draft.path, a.draft.start_line, a.draft.created_at, a.id).cmp(&(
                &b.draft.path,
                b.draft.start_line,
                b.draft.created_at,
                b.id,
            ))
        });
    }

    /// Hands the whole list to the window to be written, once the stored one has been read.
    fn save(&self) {
        let (Some(thread), Some(persist)) = (self.thread(), self.persist.borrow().clone()) else {
            return;
        };
        if !self.loaded.get() {
            return;
        }
        let drafts = self.drafts();
        persist(thread, drafts);
    }

    /// The drafts, in order.
    pub(super) fn drafts(&self) -> Vec<ReviewDraft> {
        self.entries
            .borrow()
            .iter()
            .map(|e| e.draft.clone())
            .collect()
    }

    /// After any change: written, marks and lists redrawn, the bar recounted.
    fn changed(&self) {
        self.notice.borrow_mut().take();
        self.sort();
        self.save();
        self.render_all();
    }

    /// Adds a comment on `span` of `path` with `note`, redacted as it is kept.
    pub(super) fn add(&self, path: &str, span: &CommentSpan, note: &str) {
        self.add_on(path, span, note, self.base.get().key());
    }

    /// Adds a comment made on one of the thread's file-change cards (`path` from the
    /// repository's top, as the panel names files). Refused (`None`) with no thread bound yet;
    /// else how many comments the review then holds.
    pub(super) fn add_from_card(
        &self,
        path: &str,
        span: &CommentSpan,
        note: &str,
    ) -> Option<usize> {
        self.thread.borrow().as_ref()?;
        self.add_on(path, span, note, agent_core::review::CARD_BASE);
        Some(self.entries.borrow().len())
    }

    fn add_on(&self, path: &str, span: &CommentSpan, note: &str, base: &str) {
        let draft = ReviewDraft {
            path: path.to_owned(),
            side: span.side,
            start_line: span.start,
            end_line: span.end,
            quoted: span.quoted.clone(),
            note: note.trim().to_owned(),
            diff_base: base.to_owned(),
            created_at: now_ms(),
        }
        .redacted();
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        self.entries.borrow_mut().push(Entry { id, draft });
        self.changed();
    }

    fn edit(&self, id: u64, note: &str) {
        if let Some(entry) = self.entries.borrow_mut().iter_mut().find(|e| e.id == id) {
            entry.draft.note = agent_core::redact::redact(note.trim());
        }
        self.changed();
    }

    fn remove(&self, id: u64) {
        self.entries.borrow_mut().retain(|e| e.id != id);
        self.changed();
    }

    pub(super) fn clear(&self) {
        self.entries.borrow_mut().clear();
        self.changed();
    }

    /// Where `draft` is in the diff on show.
    fn anchor(&self, draft: &ReviewDraft) -> Anchor {
        match self.chunks.borrow().get(&draft.path) {
            Some(chunk) => crate::diff::anchor(
                chunk,
                draft.side,
                draft.start_line,
                draft.end_line,
                &draft.quoted,
            ),
            None => Anchor {
                located: Located::Gone,
                span: None,
            },
        }
    }

    /// The comments as the review sends them: each placed in the diff on show (a moved one with
    /// its new lines), with what the working file said of it when `checks` (by entry id) has it.
    /// When `only` is given, comments added afterwards (not in `only`) are left out.
    fn comments_checked(
        &self,
        checks: &HashMap<u64, Checked>,
        only: Option<&HashSet<u64>>,
    ) -> Vec<(u64, ReviewComment)> {
        self.entries
            .borrow()
            .iter()
            .filter(|e| only.is_none_or(|set| set.contains(&e.id)))
            .map(|e| {
                let d = &e.draft;
                let anchor = self.anchor(d);
                let (side, start, end) = match (&anchor.located, &anchor.span) {
                    (Located::Moved, Some(s)) => (s.side, s.start, s.end),
                    _ => (d.side, d.start_line, d.end_line),
                };
                (
                    e.id,
                    ReviewComment {
                        path: d.path.clone(),
                        side,
                        start,
                        end,
                        quoted: d.quoted.clone(),
                        note: d.note.clone(),
                        located: anchor.located,
                        checked: checks.get(&e.id).copied(),
                    },
                )
            })
            .collect()
    }

    /// The comments as they would be sent with no file checked.
    #[cfg(test)]
    pub(super) fn comments(&self) -> Vec<ReviewComment> {
        self.comments_checked(&HashMap::new(), None)
            .into_iter()
            .map(|(_, c)| c)
            .collect()
    }

    /// "Send Review": with a check handler, first has each new-side comment looked for in its
    /// working file (the button spinning, nothing else sent meanwhile), then sends; without one,
    /// sends at once. Removed lines are never looked for. Comments added while the files are
    /// checked stay for the next review instead of going out unchecked.
    pub(super) fn send_now(&self) {
        if self.checking.get() || self.send.borrow().is_none() || self.entries.borrow().is_empty() {
            return;
        }
        let snapshot: HashSet<u64> = self.entries.borrow().iter().map(|e| e.id).collect();
        let Some(check) = self.check.borrow().clone() else {
            self.deliver(&HashMap::new(), Some(&snapshot));
            return;
        };
        let (ids, asks): (Vec<u64>, Vec<CheckAsk>) = self
            .comments_checked(&HashMap::new(), Some(&snapshot))
            .into_iter()
            .filter(|(_, c)| c.side == Side::New)
            .map(|(id, c)| {
                (
                    id,
                    CheckAsk {
                        path: c.path,
                        start: c.start,
                        end: c.end,
                        quoted: c.quoted,
                    },
                )
            })
            .unzip();
        if asks.is_empty() {
            self.deliver(&HashMap::new(), Some(&snapshot));
            return;
        }
        self.set_checking(true);
        let review = self.clone();
        check(
            asks,
            Box::new(move |found| {
                review.set_checking(false);
                // By id: a comment deleted meanwhile is not sent, one added meanwhile stays for
                // the next review (never sent unchecked), and an answer of the wrong length
                // checks nothing.
                let checks: HashMap<u64, Checked> = if found.len() == ids.len() {
                    ids.into_iter().zip(found).collect()
                } else {
                    HashMap::new()
                };
                review.deliver(&checks, Some(&snapshot));
            }),
        );
    }

    /// Spins the send button (and keeps Send and Discard off) while the files are checked.
    fn set_checking(&self, on: bool) {
        self.checking.set(on);
        if let (Some(face), Some(spinner)) = (self.send_face.upgrade(), self.spinner.upgrade()) {
            face.set_visible_child_name(if on { "busy" } else { "idle" });
            if on {
                spinner.start();
            } else {
                spinner.stop();
            }
        }
        self.sync_bar();
    }

    /// Whether the files are being checked for a send (tests).
    #[cfg(test)]
    pub(super) fn is_checking(&self) -> bool {
        self.checking.get()
            && self
                .send_face
                .upgrade()
                .is_some_and(|f| f.visible_child_name().as_deref() == Some("busy"))
    }

    /// Composes the review, with what the files said of each comment, and hands it to the
    /// window to queue. What went is deleted; comments past the message budget (and any added
    /// while checking) stay for the next review, and the bar says so.
    fn deliver(&self, checks: &HashMap<u64, Checked>, only: Option<&HashSet<u64>>) {
        let Some(send) = self.send.borrow().clone() else {
            return;
        };
        let pairs = self.comments_checked(checks, only);
        let comments: Vec<ReviewComment> = pairs.iter().map(|(_, c)| c.clone()).collect();
        let Composed { text, included } = agent_core::review::compose(&comments);
        if included == 0 {
            return;
        }
        let rechecked = recheck_words(comments[..included].iter().filter_map(|c| c.checked));
        let sent_ids: HashSet<u64> = pairs[..included].iter().map(|(id, _)| *id).collect();
        let not_fit = pairs.len().saturating_sub(included);
        match send(text) {
            Ok(()) => {
                {
                    let mut entries = self.entries.borrow_mut();
                    entries.retain(|e| !sent_ids.contains(&e.id));
                }
                self.changed();
                let mut notice = Vec::new();
                if not_fit > 0 {
                    notice.push(format!(
                        "Sent {}; {} did not fit in one message and {} kept for the next review",
                        count_words(included),
                        count_words(not_fit),
                        if not_fit == 1 { "is" } else { "are" }
                    ));
                }
                notice.extend(rechecked);
                if !notice.is_empty() {
                    *self.notice.borrow_mut() = Some(notice.join(" · "));
                    self.sync_bar();
                }
            }
            Err(why) => {
                *self.notice.borrow_mut() = Some(format!("Could not send the review: {why}"));
                self.sync_bar();
            }
        }
    }

    /// Takes the diff on show: each file's text by path, and its base. File views come later,
    /// as their rows are expanded.
    pub(super) fn show(&self, base: DiffBase, chunks: HashMap<String, Rc<str>>) {
        self.base.set(base);
        *self.chunks.borrow_mut() = chunks;
        self.files.borrow_mut().clear();
        self.buttons.borrow_mut().clear();
        self.sync_bar();
    }

    /// Redraws the bar: hidden with no thread, or with nothing to say.
    pub(super) fn sync_bar(&self) {
        let (Some(bar), Some(status), Some(send), Some(discard)) = (
            self.bar.upgrade(),
            self.status.upgrade(),
            self.send_button.upgrade(),
            self.discard_button.upgrade(),
        ) else {
            return;
        };
        let total = self.entries.borrow().len();
        let notice = self.notice.borrow().clone();
        let failed = self.load_failed.get();
        bar.set_visible(
            self.thread.borrow().is_some() && (total > 0 || notice.is_some() || failed),
        );
        let gone = self
            .entries
            .borrow()
            .iter()
            .filter(|e| self.anchor(&e.draft).located == Located::Gone)
            .count();
        let mut parts = Vec::new();
        if total > 0 {
            parts.push(bar_words(total, gone));
        }
        parts.extend(notice);
        if failed {
            parts.push(LOAD_FAILED.to_owned());
        }
        status.set_text(&parts.join(" · "));
        let checking = self.checking.get();
        send.set_sensitive(total > 0 && self.send.borrow().is_some() && !checking);
        send.set_tooltip_text(Some(if checking {
            "Looking for each comment's lines in the working files"
        } else {
            "Send every comment to this thread's agent as one message (queued while it works)"
        }));
        discard.set_visible(total > 0);
        discard.set_sensitive(!checking);
        self.render_orphans();
    }

    /// Lists, in the bar, the comments on files the diff on show does not list, so each can be
    /// edited or deleted there.
    fn render_orphans(&self) {
        let Some(orphans) = self.orphans.upgrade() else {
            return;
        };
        while let Some(child) = orphans.first_child() {
            orphans.remove(&child);
        }
        let mine: Vec<Entry> = {
            let chunks = self.chunks.borrow();
            self.entries
                .borrow()
                .iter()
                .filter(|e| !chunks.contains_key(&e.draft.path))
                .cloned()
                .collect()
        };
        orphans.set_visible(!mine.is_empty());
        let gone = Anchor {
            located: Located::Gone,
            span: None,
        };
        for entry in &mine {
            orphans.append(&self.comment_row(entry, &gone, &orphans, true));
        }
    }

    /// Shows every row's Comment button while there is a thread.
    fn sync_buttons(&self) {
        let shown = self.thread.borrow().is_some();
        for (button, _) in self.buttons.borrow().iter() {
            if let Some(button) = button.upgrade() {
                button.set_visible(shown);
            }
        }
    }

    pub(super) fn render_all(&self) {
        let paths: Vec<String> = self.files.borrow().iter().map(|f| f.path.clone()).collect();
        for path in paths {
            self.render(&path);
        }
        self.sync_bar();
    }

    /// Recolours the commented lines, as when the theme changes.
    pub(super) fn recolour(&self, view: &gtk4::TextView) {
        if let Some(tag) = view.buffer().tag_table().lookup(TAG_COMMENTED) {
            let hunk = self.colours.borrow().hunk;
            let rgba = gtk4::gdk::RGBA::new(hunk.red(), hunk.green(), hunk.blue(), COMMENTED_ALPHA);
            tag.set_paragraph_background_rgba(Some(&rgba));
        }
    }

    /// Marks `path`'s commented lines in its view and lists its comments under it.
    fn render(&self, path: &str) {
        let Some((view, list, column)) = self.files.borrow().iter().find_map(|f| {
            (f.path == path).then(|| (f.view.upgrade(), f.list.upgrade(), f.column.upgrade()))
        }) else {
            return;
        };
        let mine: Vec<Entry> = self
            .entries
            .borrow()
            .iter()
            .filter(|e| e.draft.path == path)
            .cloned()
            .collect();
        let anchors: Vec<Anchor> = mine.iter().map(|e| self.anchor(&e.draft)).collect();
        if let Some(view) = &view {
            let buffer = view.buffer();
            let (start, end) = buffer.bounds();
            buffer.remove_tag_by_name(TAG_COMMENTED, &start, &end);
            for anchor in &anchors {
                let Some(span) = &anchor.span else { continue };
                let (Ok(first), Ok(last)) =
                    (i32::try_from(span.rows.0), i32::try_from(span.rows.1))
                else {
                    continue;
                };
                if let (Some(from), Some(mut to)) =
                    (buffer.iter_at_line(first), buffer.iter_at_line(last))
                {
                    to.forward_to_line_end();
                    buffer.apply_tag_by_name(TAG_COMMENTED, &from, &to);
                }
            }
        }
        let (Some(list), Some(column)) = (list, column) else {
            return;
        };
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        list.set_visible(!mine.is_empty());
        for (entry, anchor) in mine.iter().zip(&anchors) {
            list.append(&self.comment_row(entry, anchor, &column, false));
        }
    }

    /// One comment under its file's diff (or, `with_path`, in the bar for a file not in it):
    /// where it is, its note, Edit and Delete.
    fn comment_row(
        &self,
        entry: &Entry,
        anchor: &Anchor,
        column: &gtk4::Box,
        with_path: bool,
    ) -> gtk4::Box {
        let d = &entry.draft;
        let place = match (&anchor.located, &anchor.span) {
            (Located::Moved, Some(s)) => place_words(s.side, s.start, s.end),
            (Located::Gone, _) => format!(
                "{} · not in this diff",
                place_words(d.side, d.start_line, d.end_line)
            ),
            _ => place_words(d.side, d.start_line, d.end_line),
        };
        let place = if with_path {
            format!("{} · {place}", d.path)
        } else {
            place
        };
        let heading = gtk4::Label::builder()
            .label(&place)
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk4::pango::WrapMode::WordChar)
            .css_classes(["caption-heading"])
            .build();
        let note = gtk4::Label::builder()
            .label(&d.note)
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk4::pango::WrapMode::WordChar)
            .selectable(true)
            .build();
        let words = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .build();
        words.append(&heading);
        words.append(&note);
        let edit = gtk4::Button::builder()
            .icon_name("at-document-edit-symbolic")
            .tooltip_text("Edit this comment")
            .valign(gtk4::Align::Center)
            .css_classes(["flat", "diff-comment-edit"])
            .build();
        edit.update_property(&[gtk4::accessible::Property::Label(&format!(
            "Edit the comment on {}",
            place.to_lowercase()
        ))]);
        let delete = gtk4::Button::builder()
            .icon_name("at-user-trash-symbolic")
            .tooltip_text("Delete this comment")
            .valign(gtk4::Align::Center)
            .css_classes(["flat", "diff-comment-delete"])
            .build();
        delete.update_property(&[gtk4::accessible::Property::Label(&format!(
            "Delete the comment on {}",
            place.to_lowercase()
        ))]);
        let row = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .spacing(4)
            .build();
        row.add_css_class("diff-comment");
        row.append(&words);
        row.append(&edit);
        row.append(&delete);

        let id = entry.id;
        let review = self.clone();
        let initial = d.note.clone();
        let column = column.downgrade();
        edit.connect_clicked(move |button| {
            let Some(column) = column.upgrade() else {
                return;
            };
            let review = review.clone();
            let rect = button.compute_bounds(&column).map(|b| {
                gtk4::gdk::Rectangle::new(
                    b.x() as i32,
                    b.y() as i32,
                    (b.width() as i32).max(1),
                    (b.height() as i32).max(1),
                )
            });
            note_editor(&column, rect, "Edit Comment", &initial, move |note| {
                review.edit(id, &note)
            });
        });
        let review = self.clone();
        delete.connect_clicked(move |_| {
            // After the click: the button goes with the list it is redrawn into.
            let review = review.clone();
            glib::idle_add_local_once(move || review.remove(id));
        });
        row
    }

    /// The row's Comment button for `path`: hidden without a thread, on only while the file's
    /// view has a selection; `why_not` (no text diff) keeps it off and says why.
    pub(super) fn comment_button(&self, path: &str, why_not: Option<&str>) -> gtk4::Button {
        let button = gtk4::Button::builder()
            .icon_name("at-chat-message-new-symbolic")
            .css_classes(["flat", "diff-comment-button"])
            .valign(gtk4::Align::Center)
            .sensitive(false)
            .visible(self.thread.borrow().is_some())
            .tooltip_text(why_not.unwrap_or(SELECT_FIRST))
            .build();
        button.update_property(&[gtk4::accessible::Property::Label(&format!(
            "Comment on the selected lines of {path}"
        ))]);
        let review = self.clone();
        let owned = path.to_owned();
        button.connect_clicked(move |_| {
            review.comment_on(&owned);
        });
        self.buttons
            .borrow_mut()
            .push((button.downgrade(), path.to_owned()));
        button
    }

    /// The Comment buttons of `path`'s row.
    fn buttons_of(&self, path: &str) -> Vec<gtk4::Button> {
        self.buttons
            .borrow()
            .iter()
            .filter(|(_, p)| p == path)
            .filter_map(|(b, _)| b.upgrade())
            .collect()
    }

    /// Wraps a file's freshly built `view` (of `chunk`) for commenting: its commented-line tag,
    /// a visible caret, Ctrl+Alt+M, the Comment button following its selection, and the list of
    /// its comments under it. Returns what the row shows.
    pub(super) fn decorate(&self, path: &str, chunk: &str, view: &gtk4::TextView) -> gtk4::Widget {
        let buffer = view.buffer();
        if let Some(tag) = buffer.create_tag(Some(TAG_COMMENTED), &[]) {
            tag.set_left_margin(view.left_margin() + COMMENTED_INDENT);
        }
        self.recolour(view);
        let list = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .spacing(4)
            .margin_start(6)
            .margin_end(6)
            .margin_top(4)
            .margin_bottom(4)
            .visible(false)
            .build();
        list.add_css_class("diff-comments");
        list.update_property(&[gtk4::accessible::Property::Label(&format!(
            "Comments on {path}"
        ))]);
        let column = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        column.append(view);
        column.append(&list);
        self.files.borrow_mut().push(FileMarks {
            path: path.to_owned(),
            chunk: Rc::from(chunk),
            view: view.downgrade(),
            list: list.downgrade(),
            column: column.downgrade(),
        });

        // A caret, so lines can be chosen by keyboard (Shift+arrows) in the read-only view.
        view.set_cursor_visible(true);
        // On only while the view is on screen (its row expanded) and has a selection: a popover
        // needs a mapped parent.
        let sync = {
            let review = self.clone();
            let owned = path.to_owned();
            move |view: &gtk4::TextView| {
                let on = view.is_mapped() && view.buffer().has_selection();
                for button in review.buttons_of(&owned) {
                    button.set_sensitive(on);
                    button.set_tooltip_text(Some(if on {
                        "Comment on the selected lines"
                    } else {
                        SELECT_FIRST
                    }));
                }
            }
        };
        let on_select = sync.clone();
        buffer.connect_has_selection_notify(glib::clone!(
            #[weak]
            view,
            move |_| on_select(&view)
        ));
        let on_map = sync.clone();
        view.connect_map(move |view| on_map(view));
        view.connect_unmap(move |view| sync(view));
        let keys = gtk4::EventControllerKey::new();
        keys.set_name(Some(KEYS_NAME));
        let review = self.clone();
        let owned = path.to_owned();
        keys.connect_key_pressed(move |_, key, _, state| {
            if crate::review_popover::is_comment_key(key, state) {
                review.comment_on(&owned);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        view.add_controller(keys);
        self.render(path);
        column.upcast()
    }

    /// Starts a comment on `path`'s selected lines (or the caret's line): refused with the reason
    /// when they are only headers or too many, else the note editor at the selection. Returns
    /// the popover it opened; nothing while the file's view is not on screen.
    pub(super) fn comment_on(&self, path: &str) -> Option<gtk4::Popover> {
        self.thread.borrow().as_ref()?;
        let (chunk, view, column) = self
            .files
            .borrow()
            .iter()
            .find_map(|f| {
                (f.path == path)
                    .then(|| (Rc::clone(&f.chunk), f.view.upgrade(), f.column.upgrade()))
            })
            .and_then(|(c, v, b)| Some((c, v?, b?)))?;
        // A collapsed row's view is off screen: nothing to anchor a popover to.
        if !view.is_mapped() {
            return None;
        }
        let (first, last, from, to) = chosen_rows(&view.buffer())?;
        let rect = selection_rect(&view, &column, &from, &to);
        Some(match crate::diff::comment_span(&chunk, first, last) {
            Err(why) => refusal(&column, rect, &why.reason()),
            Ok(span) => {
                let review = self.clone();
                let owned = path.to_owned();
                let title = format!(
                    "Comment on {}",
                    place_words(span.side, span.start, span.end)
                );
                note_editor(&column, Some(rect), &title, "", move |note| {
                    review.add(&owned, &span, &note);
                })
            }
        })
    }

    /// The comments, their places as the diff on show has them, for tests.
    #[cfg(test)]
    pub(super) fn located(&self) -> Vec<(String, Located, u32, u32)> {
        self.comments()
            .into_iter()
            .map(|c| (c.path, c.located, c.start, c.end))
            .collect()
    }

    /// The bar's words when it is shown (tests).
    #[cfg(test)]
    pub(super) fn bar_view(&self) -> Option<(String, bool)> {
        let (bar, status, send) = (
            self.bar.upgrade()?,
            self.status.upgrade()?,
            self.send_button.upgrade()?,
        );
        bar.is_visible()
            .then(|| (status.text().to_string(), send.is_sensitive()))
    }

    /// The rows of `path`'s view tagged as commented (tests).
    #[cfg(test)]
    pub(super) fn marked_rows(&self, path: &str) -> Vec<i32> {
        let Some(view) = self
            .files
            .borrow()
            .iter()
            .find_map(|f| (f.path == path).then(|| f.view.upgrade()).flatten())
        else {
            return Vec::new();
        };
        let buffer = view.buffer();
        let Some(tag) = buffer.tag_table().lookup(TAG_COMMENTED) else {
            return Vec::new();
        };
        (0..buffer.line_count())
            .filter(|&l| buffer.iter_at_line(l).is_some_and(|i| i.has_tag(&tag)))
            .collect()
    }

    /// `path`'s built view (tests).
    #[cfg(test)]
    pub(super) fn view_of(&self, path: &str) -> Option<gtk4::TextView> {
        self.files
            .borrow()
            .iter()
            .find_map(|f| (f.path == path).then(|| f.view.upgrade()).flatten())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bar_counts_comments_and_those_not_in_the_diff() {
        assert_eq!(bar_words(1, 0), "1 comment");
        assert_eq!(bar_words(3, 0), "3 comments");
        assert_eq!(bar_words(3, 1), "3 comments · 1 not in this diff");
        assert_eq!(place_words(Side::New, 4, 4), "Line 4");
        assert_eq!(place_words(Side::New, 4, 6), "Lines 4–6");
        assert_eq!(place_words(Side::Old, 2, 3), "Lines 2–3 (removed)");
    }
}

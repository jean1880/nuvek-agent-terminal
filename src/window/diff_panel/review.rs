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
//! lines are not found is kept and flagged "not in this diff".

use crate::diff::{Anchor, CommentSpan, DiffBase, Located, Side};
use crate::theme::DiffColours;
use adw::prelude::*;
use agent_core::review::{Composed, ReviewComment};
use agent_kit::store::ReviewDraft;
use gtk4::glib;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

/// Tag of the lines a comment is on.
pub(super) const TAG_COMMENTED: &str = "commented";
/// How far a commented line is indented beyond the view's own margin: the mark that is not a
/// colour.
const COMMENTED_INDENT: i32 = 14;
/// How opaque the commented lines' background is.
const COMMENTED_ALPHA: f32 = 0.22;

/// What the bar says, for as long as the panel lives, when the stored drafts could not be read.
pub(super) const LOAD_FAILED: &str = "Saved comments could not be read; new ones are not saved";

/// The name of the review's own key controllers (a file's view, the note editor).
pub(super) const KEYS_NAME: &str = "diff-review-keys";

/// What "Comment" says while it cannot be pressed.
pub(super) const SELECT_FIRST: &str = "Select lines in this file's diff to comment on them";

/// Writes `thread`'s whole draft list (already redacted).
pub(super) type PersistHandler = Rc<dyn Fn(String, Vec<ReviewDraft>)>;
/// Queues a composed review to the thread; `Err` says why it could not.
pub(super) type SendHandler = Rc<dyn Fn(String) -> Result<(), String>>;

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
    /// The stored drafts could not be read: nothing is saved for the rest of the panel's life,
    /// and the bar says so for as long.
    load_failed: Rc<Cell<bool>>,
    colours: Rc<RefCell<DiffColours>>,
    bar: glib::WeakRef<gtk4::Box>,
    orphans: glib::WeakRef<gtk4::Box>,
    status: glib::WeakRef<gtk4::Label>,
    send_button: glib::WeakRef<gtk4::Button>,
    discard_button: glib::WeakRef<gtk4::Button>,
}

/// `1 comment`, `3 comments`.
pub(super) fn count_words(n: usize) -> String {
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
    pub(super) discard: gtk4::Button,
}

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
    let send = gtk4::Button::builder()
        .child(
            &adw::ButtonContent::builder()
                .icon_name("at-chat-message-new-symbolic")
                .label("Send Review")
                .build(),
        )
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
            load_failed: Rc::new(Cell::new(false)),
            colours,
            bar: bar.region.downgrade(),
            orphans: bar.orphans.downgrade(),
            status: bar.status.downgrade(),
            send_button: bar.send.downgrade(),
            discard_button: bar.discard.downgrade(),
        }
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
        let draft = ReviewDraft {
            path: path.to_owned(),
            side: span.side,
            start_line: span.start,
            end_line: span.end,
            quoted: span.quoted.clone(),
            note: note.trim().to_owned(),
            diff_base: self.base.get().key().to_owned(),
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
    /// its new lines).
    pub(super) fn comments(&self) -> Vec<ReviewComment> {
        self.entries
            .borrow()
            .iter()
            .map(|e| {
                let d = &e.draft;
                let anchor = self.anchor(d);
                let (side, start, end) = match (&anchor.located, &anchor.span) {
                    (Located::Moved, Some(s)) => (s.side, s.start, s.end),
                    _ => (d.side, d.start_line, d.end_line),
                };
                ReviewComment {
                    path: d.path.clone(),
                    side,
                    start,
                    end,
                    quoted: d.quoted.clone(),
                    note: d.note.clone(),
                    located: anchor.located,
                }
            })
            .collect()
    }

    /// Composes the review and hands it to the window to queue. What went is deleted; comments
    /// past the message budget stay for the next review, and the bar says so.
    pub(super) fn send_now(&self) {
        let Some(send) = self.send.borrow().clone() else {
            return;
        };
        let Composed { text, included } = agent_core::review::compose(&self.comments());
        if included == 0 {
            return;
        }
        match send(text) {
            Ok(()) => {
                let left = {
                    let mut entries = self.entries.borrow_mut();
                    let n = included.min(entries.len());
                    entries.drain(..n);
                    entries.len()
                };
                self.changed();
                if left > 0 {
                    *self.notice.borrow_mut() = Some(format!(
                        "Sent {}; {} did not fit in one message and {} kept for the next review",
                        count_words(included),
                        count_words(left),
                        if left == 1 { "is" } else { "are" }
                    ));
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
        send.set_sensitive(total > 0 && self.send.borrow().is_some());
        discard.set_visible(total > 0);
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
            let mods = state & gtk4::accelerator_get_default_mod_mask();
            let wanted = gtk4::gdk::ModifierType::CONTROL_MASK | gtk4::gdk::ModifierType::ALT_MASK;
            if mods == wanted && matches!(key, gtk4::gdk::Key::m | gtk4::gdk::Key::M) {
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
        let buffer = view.buffer();
        let (from, to) = buffer.selection_bounds().unwrap_or_else(|| {
            let caret = buffer.iter_at_mark(&buffer.get_insert());
            (caret, caret)
        });
        let first = from.line();
        // A selection ending at the start of a line does not take that line.
        let last = if to.line() > first && to.line_offset() == 0 {
            to.line() - 1
        } else {
            to.line()
        };
        let rect = selection_rect(&view, &column, &from, &to);
        let (Ok(first), Ok(last)) = (usize::try_from(first), usize::try_from(last)) else {
            return None;
        };
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

/// Where the selection from `from` to `to` is in `view`, in its widget's coordinates.
fn selection_rect(
    view: &gtk4::TextView,
    column: &gtk4::Box,
    from: &gtk4::TextIter,
    to: &gtk4::TextIter,
) -> gtk4::gdk::Rectangle {
    let a = view.iter_location(from);
    let b = view.iter_location(to);
    let (x, y) = view.buffer_to_window_coords(gtk4::TextWindowType::Widget, a.x(), a.y());
    let (_, y2) = view.buffer_to_window_coords(gtk4::TextWindowType::Widget, b.x(), b.y());
    let bottom = y2 + b.height().max(1);
    // In the column's coordinates; the view is its first child, so at worst the same.
    let (dx, dy) = view
        .compute_point(column, &gtk4::graphene::Point::new(0.0, 0.0))
        .map_or((0, 0), |p| (p.x() as i32, p.y() as i32));
    gtk4::gdk::Rectangle::new((x + dx).max(0), y + dy, 1, (bottom - y).max(1))
}

/// A popover on `parent` that is let go of when it closes, or when `parent` is unmapped (the
/// panel redrawn under it).
fn transient_popover(
    parent: &impl IsA<gtk4::Widget>,
    rect: Option<gtk4::gdk::Rectangle>,
) -> gtk4::Popover {
    let popover = gtk4::Popover::new();
    popover.set_parent(parent);
    if let Some(rect) = rect {
        popover.set_pointing_to(Some(&rect));
    }
    let parent = parent.upcast_ref::<gtk4::Widget>().clone();
    let unmapped = parent.connect_unmap(glib::clone!(
        #[weak]
        popover,
        move |_| {
            popover.popdown();
            if popover.parent().is_some() {
                popover.unparent();
            }
        }
    ));
    let handler = RefCell::new(Some(unmapped));
    popover.connect_closed(glib::clone!(
        #[weak]
        parent,
        move |p| {
            if let Some(id) = handler.borrow_mut().take() {
                parent.disconnect(id);
            }
            let p = p.clone();
            glib::idle_add_local_once(move || {
                if p.parent().is_some() {
                    p.unparent();
                }
            });
        }
    ));
    popover
}

/// Says, at the selection, why it takes no comment.
fn refusal(column: &gtk4::Box, rect: gtk4::gdk::Rectangle, why: &str) -> gtk4::Popover {
    let popover = transient_popover(column, Some(rect));
    let label = gtk4::Label::builder()
        .label(why)
        .wrap(true)
        .max_width_chars(40)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    label.add_css_class("diff-comment-refusal");
    popover.set_child(Some(&label));
    popover.popup();
    popover
}

/// The note editor: a multi-line entry in a popover on `parent`. Enter saves, Shift+Enter adds a
/// line, Escape (or a press outside) cancels. Save is off while the note is empty.
fn note_editor(
    parent: &impl IsA<gtk4::Widget>,
    rect: Option<gtk4::gdk::Rectangle>,
    title: &str,
    initial: &str,
    on_save: impl Fn(String) + 'static,
) -> gtk4::Popover {
    let popover = transient_popover(parent, rect);
    popover.add_css_class("diff-comment-editor");
    let heading = gtk4::Label::builder()
        .label(title)
        .xalign(0.0)
        .css_classes(["heading"])
        .build();
    let entry = gtk4::TextView::builder()
        .wrap_mode(gtk4::WrapMode::WordChar)
        .accepts_tab(false)
        .top_margin(6)
        .bottom_margin(6)
        .left_margin(6)
        .right_margin(6)
        .build();
    entry.add_css_class("card");
    entry.buffer().set_text(initial);
    entry.update_property(&[gtk4::accessible::Property::Label("Comment")]);
    let scroller = gtk4::ScrolledWindow::builder()
        .child(&entry)
        .min_content_width(320)
        .min_content_height(80)
        .max_content_height(240)
        .propagate_natural_height(true)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .build();
    let hint = gtk4::Label::builder()
        .label("Enter saves · Shift+Enter adds a line")
        .xalign(0.0)
        .hexpand(true)
        .css_classes(["dim-label", "caption"])
        .build();
    let cancel = gtk4::Button::with_label("Cancel");
    let save = gtk4::Button::builder()
        .label("Save")
        .css_classes(["suggested-action"])
        .sensitive(!initial.trim().is_empty())
        .build();
    let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    buttons.append(&hint);
    buttons.append(&cancel);
    buttons.append(&save);
    let column = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(6)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    column.append(&heading);
    column.append(&scroller);
    column.append(&buttons);
    popover.set_child(Some(&column));

    let on_save = Rc::new(on_save);
    let commit = Rc::new(glib::clone!(
        #[weak]
        popover,
        #[weak]
        entry,
        move || {
            let buffer = entry.buffer();
            let (start, end) = buffer.bounds();
            let note = buffer.text(&start, &end, false).to_string();
            if note.trim().is_empty() {
                return;
            }
            popover.popdown();
            on_save(note);
        }
    ));
    entry.buffer().connect_changed(glib::clone!(
        #[weak]
        save,
        move |buffer| {
            let (start, end) = buffer.bounds();
            save.set_sensitive(!buffer.text(&start, &end, false).trim().is_empty());
        }
    ));
    let keys = gtk4::EventControllerKey::new();
    keys.set_name(Some(KEYS_NAME));
    keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
    let on_enter = Rc::clone(&commit);
    keys.connect_key_pressed(move |_, key, _, state| {
        let enter = matches!(
            key,
            gtk4::gdk::Key::Return | gtk4::gdk::Key::KP_Enter | gtk4::gdk::Key::ISO_Enter
        );
        if enter && !state.contains(gtk4::gdk::ModifierType::SHIFT_MASK) {
            on_enter();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    entry.add_controller(keys);
    let on_click = Rc::clone(&commit);
    save.connect_clicked(move |_| on_click());
    cancel.connect_clicked(glib::clone!(
        #[weak]
        popover,
        move |_| popover.popdown()
    ));
    popover.popup();
    entry.grab_focus();
    popover
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

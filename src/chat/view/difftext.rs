//! A read-only unified diff: a `GtkSourceView` with the `diff` language, monospace, in a frame
//! capped in height, with a line of notes under it (where the diff came from, whether it was cut).
//!
//! Used by the file-change cards ("View diff") and by file-edit approvals, so both show the same
//! thing the same way. Line numbers are deliberately off: a diff's own line numbers are the
//! numbers of the diff text, not of the file, and would mislead.
//!
//! A thread's own file-change cards also take review comments ([`DiffText::allow_comments`]):
//! a "Comment" button in the header (and Ctrl+Alt+M in the view, on the selection or the caret's
//! line) maps the selection to a file and its lines (`agent_kit::diff::card_comment_span`) and
//! opens the note editor; the saved note goes to the thread's review. Approvals, and the cards
//! of sub-agents in the inspector, never take one.

use gtk4::glib;
use gtk4::prelude::*;
use sourceview5::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

/// The tallest the diff frame grows before it scrolls.
const MAX_HEIGHT: i32 = 320;

/// Takes a comment written on the diff: its file and lines, and the note.
pub type CommentHandler = Rc<dyn Fn(agent_kit::diff::CardSpan, String)>;

/// What "Comment" says while there is nothing selected to comment on.
pub const SELECT_FIRST: &str = "Select lines of this diff to comment on them";

/// A diff's comment state, shared with the handlers of its widgets.
#[derive(Default)]
struct Comments {
    /// Set once the diff takes comments at all.
    handler: RefCell<Option<CommentHandler>>,
    /// Why the diff on show takes none (`None`: it does).
    why_not: RefCell<Option<String>>,
    /// The text on show, as its rows are counted.
    text: RefCell<Rc<str>>,
}

pub struct DiffText {
    root: gtk4::Box,
    buffer: sourceview5::Buffer,
    view: sourceview5::View,
    notes: gtk4::Label,
    comment: gtk4::Button,
    comments: Rc<Comments>,
}

impl DiffText {
    pub fn new() -> Self {
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        root.add_css_class("diff-text");

        let buffer = sourceview5::Buffer::new(None);
        buffer.set_highlight_matching_brackets(false);
        if let Some(lang) = sourceview5::LanguageManager::default().language("diff") {
            buffer.set_language(Some(&lang));
            buffer.set_highlight_syntax(true);
        }
        super::cards::apply_scheme(&buffer);

        let view = sourceview5::View::with_buffer(&buffer);
        view.set_editable(false);
        view.set_cursor_visible(false);
        view.set_monospace(true);
        view.set_wrap_mode(gtk4::WrapMode::None);
        view.set_show_line_numbers(false);
        view.add_css_class("code-view");
        view.add_css_class("diff-view");
        view.set_top_margin(8);
        view.set_bottom_margin(8);
        view.set_left_margin(10);
        view.set_right_margin(10);

        let scroller = gtk4::ScrolledWindow::new();
        scroller.set_policy(gtk4::PolicyType::Automatic, gtk4::PolicyType::Automatic);
        scroller.set_propagate_natural_height(true);
        scroller.set_max_content_height(MAX_HEIGHT);
        scroller.set_child(Some(&view));
        // The same frame as a code block: a header with the name and Copy, over the text.
        let frame = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        frame.add_css_class("code-block");
        let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        header.add_css_class("code-header");
        let name = super::cards::label("diff", &["code-lang"]);
        name.set_hexpand(true);
        header.append(&name);
        let comment = gtk4::Button::builder()
            .icon_name("at-chat-message-new-symbolic")
            .css_classes(["flat", "diff-comment-button"])
            .valign(gtk4::Align::Center)
            .sensitive(false)
            .visible(false)
            .tooltip_text(SELECT_FIRST)
            .build();
        comment.update_property(&[gtk4::accessible::Property::Label(
            "Comment on the selected lines of this diff",
        )]);
        header.append(&comment);
        let copy = super::cards::copy_button("Copy diff");
        header.append(&copy);
        super::cards::wire_copy(&copy, {
            let buffer = buffer.downgrade();
            move || {
                buffer.upgrade().map_or_else(String::new, |buffer| {
                    let (start, end) = buffer.bounds();
                    buffer.text(&start, &end, false).to_string()
                })
            }
        });
        frame.append(&header);
        frame.append(&scroller);
        root.append(&frame);

        let notes = super::cards::content_label("", &["diff-notes", "dim-label"]);
        notes.set_visible(false);
        root.append(&notes);

        Self {
            root,
            buffer,
            view,
            notes,
            comment,
            comments: Rc::default(),
        }
    }

    pub fn widget(&self) -> &gtk4::Box {
        &self.root
    }

    /// Shows `text`, with `notes` under it (empty: no note line).
    pub fn set(&self, text: &str, notes: &[String]) {
        *self.comments.text.borrow_mut() = Rc::from(text);
        self.buffer.set_text(text);
        let line = notes.join("  ·  ");
        self.notes.set_text(&line);
        self.notes.set_visible(!line.is_empty());
        self.sync_comment();
    }

    /// Lets the diff take review comments, each handed to `handler`: shows "Comment", gives the
    /// view a caret (lines can be chosen by keyboard, Shift+arrows) and binds Ctrl+Alt+M. Once.
    pub fn allow_comments(&self, handler: CommentHandler) {
        if self.comments.handler.replace(Some(handler)).is_some() {
            return;
        }
        self.comment.set_visible(true);
        self.view.set_cursor_visible(true);
        let sync = {
            let (comments, button) = (Rc::clone(&self.comments), self.comment.downgrade());
            move |view: &sourceview5::View| {
                if let Some(button) = button.upgrade() {
                    sync_button(&comments, view, &button);
                }
            }
        };
        let on_select = sync.clone();
        let weak_view = self.view.downgrade();
        self.buffer.connect_has_selection_notify(move |_| {
            if let Some(view) = weak_view.upgrade() {
                on_select(&view);
            }
        });
        let on_map = sync.clone();
        self.view.connect_map(move |view| on_map(view));
        self.view.connect_unmap(move |view| sync(view));
        let start = {
            let (comments, view, root) = (
                Rc::clone(&self.comments),
                self.view.downgrade(),
                self.root.downgrade(),
            );
            move || {
                if let (Some(view), Some(root)) = (view.upgrade(), root.upgrade()) {
                    comment_on(&comments, &view, &root);
                }
            }
        };
        let on_click = start.clone();
        self.comment.connect_clicked(move |_| on_click());
        let keys = gtk4::EventControllerKey::new();
        keys.set_name(Some(crate::review_popover::KEYS_NAME));
        keys.connect_key_pressed(move |_, key, _, state| {
            if crate::review_popover::is_comment_key(key, state) {
                start();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.view.add_controller(keys);
        self.sync_comment();
    }

    /// Whether the diff on show takes comments: `None` when it does, else why not (said as the
    /// button's tooltip, the button off).
    pub fn set_commentable(&self, why_not: Option<&str>) {
        *self.comments.why_not.borrow_mut() = why_not.map(str::to_owned);
        self.sync_comment();
    }

    fn sync_comment(&self) {
        sync_button(&self.comments, &self.view, &self.comment);
    }

    /// Starts a comment on the selection, as "Comment" does; the popover it opened (tests).
    #[cfg(test)]
    pub fn comment_on(&self) -> Option<gtk4::Popover> {
        comment_on(&self.comments, &self.view, &self.root)
    }

    /// The Comment button: shown, on, and its tooltip (tests).
    #[cfg(test)]
    pub fn comment_button(&self) -> (bool, bool, Option<String>) {
        (
            self.comment.is_visible(),
            self.comment.is_sensitive(),
            self.comment.tooltip_text().map(|t| t.to_string()),
        )
    }

    /// Selects rows `first..=last` (tests).
    #[cfg(test)]
    pub fn select_rows(&self, first: i32, last: i32) {
        if let (Some(from), Some(mut to)) = (
            self.buffer.iter_at_line(first),
            self.buffer.iter_at_line(last),
        ) {
            to.forward_to_line_end();
            self.buffer.select_range(&from, &to);
        }
    }

    /// What is shown (tests).
    #[cfg(test)]
    pub fn text(&self) -> String {
        let (start, end) = self.buffer.bounds();
        self.buffer.text(&start, &end, false).to_string()
    }

    /// The notes line (tests).
    #[cfg(test)]
    pub fn notes(&self) -> String {
        self.notes.text().to_string()
    }

    /// Whether the buffer is highlighted as a diff (tests).
    #[cfg(test)]
    pub fn language(&self) -> Option<String> {
        self.buffer.language().map(|l| l.id().to_string())
    }
}

/// Turns "Comment" on only while the diff takes comments, its view is on screen (a popover
/// needs a mapped parent) and has a selection; its tooltip says why not otherwise.
fn sync_button(comments: &Comments, view: &sourceview5::View, button: &gtk4::Button) {
    if comments.handler.borrow().is_none() {
        return;
    }
    let why_not = comments.why_not.borrow().clone();
    let on = why_not.is_none() && view.is_mapped() && view.buffer().has_selection();
    button.set_sensitive(on);
    button.set_tooltip_text(Some(match (&why_not, on) {
        (Some(why), _) => why.as_str(),
        (None, true) => "Comment on the selected lines",
        (None, false) => SELECT_FIRST,
    }));
}

/// Starts a comment on the selected rows (or the caret's): the reason when they take none, else
/// the note editor at the selection, whose note goes to the handler. Nothing while the diff
/// takes no comments or is off screen.
fn comment_on(
    comments: &Comments,
    view: &sourceview5::View,
    root: &gtk4::Box,
) -> Option<gtk4::Popover> {
    let handler = comments.handler.borrow().clone()?;
    if comments.why_not.borrow().is_some() || !view.is_mapped() {
        return None;
    }
    let text = Rc::clone(&comments.text.borrow());
    let buffer: gtk4::TextBuffer = view.buffer();
    let (first, last, from, to) = crate::review_popover::chosen_rows(&buffer)?;
    let rect = crate::review_popover::selection_rect(view, root, &from, &to);
    Some(
        match agent_kit::diff::card_comment_span(&text, first, last) {
            Err(why) => crate::review_popover::refusal(root, rect, &why.reason()),
            Ok(place) => {
                let lines = if place.span.start == place.span.end {
                    format!("line {}", place.span.start)
                } else {
                    format!("lines {}–{}", place.span.start, place.span.end)
                };
                let removed = match place.span.side {
                    agent_core::review::Side::New => "",
                    agent_core::review::Side::Old => " (removed)",
                };
                let title = format!(
                    "Comment on {}, {lines}{removed}",
                    agent_kit::diff::file_name(&place.path)
                );
                crate::review_popover::note_editor(root, Some(rect), &title, "", move |note| {
                    handler(place.clone(), note);
                })
            }
        },
    )
}

/// The notes under a diff: where it came from, and that it was cut.
pub fn notes_for(
    origin: agent_kit::filediff::Origin,
    omitted_lines: usize,
    clipped: bool,
) -> Vec<String> {
    use agent_kit::filediff::Origin;
    let mut notes = Vec::new();
    match origin {
        Origin::Checkpoint => notes.push("Changes this turn".to_owned()),
        Origin::AgentEdit => notes.push("from the agent's edit".to_owned()),
    }
    if omitted_lines > 0 {
        notes.push(format!(
            "Diff truncated, {omitted_lines} more lines — open externally"
        ));
    }
    if clipped {
        notes.push(
            "Only the first 512 KiB of each side were compared; the change may lie beyond."
                .to_owned(),
        );
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_kit::filediff::Origin;

    #[test]
    fn the_notes_say_where_the_diff_came_from_and_when_it_was_cut() {
        assert_eq!(
            notes_for(Origin::Checkpoint, 0, false),
            ["Changes this turn"]
        );
        assert_eq!(
            notes_for(Origin::AgentEdit, 0, false),
            ["from the agent's edit"]
        );
        let cut = notes_for(Origin::Checkpoint, 12, false);
        assert_eq!(cut.len(), 2);
        assert!(cut[1].starts_with("Diff truncated"), "{cut:?}");
        assert!(cut[1].ends_with("open externally"));
    }

    #[test]
    fn a_clipped_comparison_warns_even_when_display_truncation_hides_the_trailer() {
        let warning =
            "Only the first 512 KiB of each side were compared; the change may lie beyond.";
        for origin in [Origin::Checkpoint, Origin::AgentEdit] {
            for omitted in [0, 12] {
                let notes = notes_for(origin, omitted, true);
                assert!(notes.iter().any(|n| n == warning), "{notes:?}");
                assert!(!notes_for(origin, omitted, false)
                    .iter()
                    .any(|n| n == warning));
            }
        }
    }
}

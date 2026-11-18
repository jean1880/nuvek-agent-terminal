//! The popovers a diff comment is written in, shared by the diff panel and the file-change
//! cards: the note editor, the line that says why a selection takes no comment, and where a
//! selection is. Each popover is parented to a box (never the text view, which would not let go
//! of one it still holds when it goes) and let go of when it closes or that box is unmapped.

use gtk4::glib;
use gtk4::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

/// The name of a comment's own key controllers (a diff view, the note editor).
pub const KEYS_NAME: &str = "diff-review-keys";

/// Whether a key press is Ctrl+Alt+M, the "comment on the selected lines" key.
pub fn is_comment_key(key: gtk4::gdk::Key, state: gtk4::gdk::ModifierType) -> bool {
    let mods = state & gtk4::accelerator_get_default_mod_mask();
    let wanted = gtk4::gdk::ModifierType::CONTROL_MASK | gtk4::gdk::ModifierType::ALT_MASK;
    mods == wanted && matches!(key, gtk4::gdk::Key::m | gtk4::gdk::Key::M)
}

/// The selected rows of `buffer` (or the caret's row), first and last, and the iterators at
/// either end. A selection ending at the start of a row does not take that row.
pub fn chosen_rows(
    buffer: &gtk4::TextBuffer,
) -> Option<(usize, usize, gtk4::TextIter, gtk4::TextIter)> {
    let (from, to) = buffer.selection_bounds().unwrap_or_else(|| {
        let caret = buffer.iter_at_mark(&buffer.get_insert());
        (caret, caret)
    });
    let first = from.line();
    let last = if to.line() > first && to.line_offset() == 0 {
        to.line() - 1
    } else {
        to.line()
    };
    Some((
        usize::try_from(first).ok()?,
        usize::try_from(last).ok()?,
        from,
        to,
    ))
}

/// Where the selection from `from` to `to` is in `view`, in `parent`'s coordinates (`parent`
/// holds the view).
pub fn selection_rect(
    view: &impl IsA<gtk4::TextView>,
    parent: &impl IsA<gtk4::Widget>,
    from: &gtk4::TextIter,
    to: &gtk4::TextIter,
) -> gtk4::gdk::Rectangle {
    let view = view.as_ref();
    let a = view.iter_location(from);
    let b = view.iter_location(to);
    let (x, y) = view.buffer_to_window_coords(gtk4::TextWindowType::Widget, a.x(), a.y());
    let (_, y2) = view.buffer_to_window_coords(gtk4::TextWindowType::Widget, b.x(), b.y());
    let bottom = y2 + b.height().max(1);
    let (dx, dy) = view
        .compute_point(parent, &gtk4::graphene::Point::new(0.0, 0.0))
        .map_or((0, 0), |p| (p.x() as i32, p.y() as i32));
    gtk4::gdk::Rectangle::new((x + dx).max(0), y + dy, 1, (bottom - y).max(1))
}

/// A popover on `parent` that is let go of when it closes, or when `parent` is unmapped (redrawn
/// under it).
pub fn transient_popover(
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
pub fn refusal(
    parent: &impl IsA<gtk4::Widget>,
    rect: gtk4::gdk::Rectangle,
    why: &str,
) -> gtk4::Popover {
    let popover = transient_popover(parent, Some(rect));
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
pub fn note_editor(
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

//! The shelves docked above the composer for work that runs beside the turn: the sub-agents
//! (`subagent_shelf.rs`) and the background tasks (`background.rs`). Both are built from the same
//! parts so they read as one family: a collapsible header with a one-line summary
//! ([`Shelf`]), rows of the same shape ([`ShelfRow`]: a mark, a spinner or how it ended, a name,
//! what it is, and a line saying what it does now), and a [`ShelfStack`] that joins whichever
//! shelves show into one card, divided by a rule, instead of loose cards each with its own frame.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;

use super::cards::label;
use super::subagents::set_if_changed;

/// One shelf: a collapsible card whose header says what it holds, over a list of
/// [`ShelfRow`]s that scrolls inside the card, and an optional action under it.
pub struct Shelf {
    pub revealer: gtk4::Revealer,
    pub title: gtk4::Label,
    pub fold: super::cards::Collapsible,
    pub list: gtk4::ListBox,
}

impl Shelf {
    /// `icon` and `what` head the card ("Show `what`"); `footer`, if any, sits under the list.
    /// Collapsed and hidden to start with.
    pub fn new(icon: &str, what: &'static str, footer: Option<&gtk4::Button>) -> Self {
        let title = label("", &["plan-title", "shelf-title"]);
        let list = gtk4::ListBox::new();
        list.add_css_class("shelf-list");
        list.set_selection_mode(gtk4::SelectionMode::None);
        // A long list scrolls inside the card rather than pushing the composer away.
        let scroller = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vscrollbar_policy(gtk4::PolicyType::Automatic)
            .propagate_natural_height(true)
            .max_content_height(220)
            .child(&list)
            .build();
        let body = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        body.add_css_class("shelf-body");
        body.append(&scroller);
        if let Some(footer) = footer {
            body.append(footer);
        }
        let fold = super::cards::collapsible(icon, &title, &body, false, what);
        fold.card.add_css_class("shelf");
        let revealer = gtk4::Revealer::builder()
            .transition_type(gtk4::RevealerTransitionType::SlideUp)
            .reveal_child(false)
            .child(&fold.card)
            .build();
        Self {
            revealer,
            title,
            fold,
            list,
        }
    }

    /// Shows or hides the shelf; the stack it is in follows.
    pub fn show(&self, shown: bool) {
        if self.revealer.reveals_child() != shown {
            self.revealer.set_reveal_child(shown);
        }
    }

    #[cfg(test)]
    pub fn shown(&self) -> bool {
        self.revealer.reveals_child()
    }

    /// Replaces the rows with `n` fresh ones (the caller fills them).
    pub fn rebuild(&self, n: usize) -> Vec<ShelfRow> {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        (0..n)
            .map(|_| {
                let row = ShelfRow::new();
                self.list.append(&row.row);
                row
            })
            .collect()
    }

    /// Opens or closes the card.
    pub fn set_expanded(&self, expanded: bool) {
        if self.fold.body.reveals_child() != expanded {
            self.fold.toggle.emit_clicked();
        }
    }

    #[cfg(test)]
    pub fn expanded(&self) -> bool {
        self.fold.body.reveals_child()
    }
}

/// One row of a shelf, its labels set in place: `mark` (the agent's brand, or what kind of task
/// it is), a spinner while it runs or `state` once it ended, `name`, `detail` (what it was asked,
/// or what it is) and `now` under them.
pub struct ShelfRow {
    pub row: gtk4::ListBoxRow,
    pub mark: gtk4::Image,
    /// Spins while it runs; hidden once it has ended.
    pub spinner: gtk4::Spinner,
    /// How it ended (`completed`, `failed`…); hidden while it runs.
    pub state: gtk4::Label,
    pub name: gtk4::Label,
    pub detail: gtk4::Label,
    pub now: gtk4::Label,
}

impl ShelfRow {
    fn new() -> Self {
        let b = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        b.add_css_class("shelf-row");
        let top = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        let mark = gtk4::Image::new();
        mark.set_pixel_size(14);
        mark.add_css_class("shelf-mark");
        top.append(&mark);
        let spinner = gtk4::Spinner::builder().spinning(true).build();
        spinner.add_css_class("thread-background");
        top.append(&spinner);
        let state = label("", &["subagent-state", "dim-label"]);
        state.set_visible(false);
        top.append(&state);
        let name = label("", &["subagent-name"]);
        name.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        name.set_max_width_chars(28);
        top.append(&name);
        let detail = label("", &["subagent-task", "dim-label"]);
        detail.set_hexpand(true);
        detail.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        top.append(&detail);
        b.append(&top);
        let now = label("", &["shelf-now"]);
        now.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        b.append(&now);
        let row = gtk4::ListBoxRow::new();
        row.set_child(Some(&b));
        Self {
            row,
            mark,
            spinner,
            state,
            name,
            detail,
            now,
        }
    }

    /// Spins while `running`; otherwise shows `ended` in its place.
    pub fn set_running(&self, running: bool, ended: &str) {
        // The widget's own `visible`, not `is_visible()`: that is false for every row while the
        // card is collapsed, and a finished row would keep spinning once opened.
        if self.spinner.property::<bool>("visible") != running {
            self.spinner.set_visible(running);
            self.spinner.set_spinning(running);
        }
        self.state.set_visible(!running);
        set_if_changed(&self.state, if running { "" } else { ended });
    }

    pub fn set_text(&self, name: &str, detail: &str, now: &str) {
        set_if_changed(&self.name, name);
        set_if_changed(&self.detail, detail);
        set_if_changed(&self.now, now);
    }

    /// The row's tooltip and its accessible name and description.
    pub fn describe(&self, tooltip: &str, name: &str, description: &str) {
        self.row.set_tooltip_text(Some(tooltip));
        self.row.update_property(&[
            gtk4::accessible::Property::Label(name),
            gtk4::accessible::Property::Description(description),
        ]);
    }
}

/// The shelves above the composer, joined into one card: whichever show are stacked edge to edge
/// inside one frame, each after the first divided from the one above by a rule, and the frame
/// goes once none shows (after its closing slide, so nothing jumps).
pub struct ShelfStack {
    pub widget: gtk4::Box,
    shelves: RefCell<Vec<gtk4::Revealer>>,
}

impl ShelfStack {
    pub fn new() -> Rc<Self> {
        let widget = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        widget.add_css_class("shelf-stack");
        // The frame's rounded corners clip the shelves' own headers.
        widget.set_overflow(gtk4::Overflow::Hidden);
        widget.set_visible(false);
        Rc::new(Self {
            widget,
            shelves: RefCell::new(Vec::new()),
        })
    }

    /// Adds `shelf` at the bottom of the stack.
    pub fn push(self: &Rc<Self>, shelf: &gtk4::Revealer) {
        self.widget.append(shelf);
        self.shelves.borrow_mut().push(shelf.clone());
        let weak = Rc::downgrade(self);
        let sync = move |_: &gtk4::Revealer| {
            if let Some(this) = weak.upgrade() {
                this.sync();
            }
        };
        shelf.connect_reveal_child_notify(sync.clone());
        shelf.connect_child_revealed_notify(sync);
        self.sync();
    }

    /// The frame shows while any shelf does (or is still sliding away); the first one showing is
    /// the top, every later one is joined under a rule.
    fn sync(&self) {
        let mut first = true;
        for shelf in self.shelves.borrow().iter() {
            let showing = shelf.reveals_child() || shelf.is_child_revealed();
            // A closed shelf takes no room and is no one's divider.
            shelf.set_visible(showing);
            if showing && !first {
                shelf.add_css_class("shelf-joined");
            } else {
                shelf.remove_css_class("shelf-joined");
            }
            first &= !showing;
        }
        self.widget.set_visible(!first);
    }

    /// Each shelf, top to bottom: whether it shows, and whether it is joined under a rule (tests).
    #[cfg(test)]
    pub fn showing(&self) -> Vec<(bool, bool)> {
        self.shelves
            .borrow()
            .iter()
            .map(|s| (s.is_visible(), s.has_css_class("shelf-joined")))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frame shows only while a shelf does; whichever shelf shows first heads it, and only
    /// the ones under it are joined by a rule, whatever order they come and go in.
    #[test]
    #[ignore = "native GTK; run alone in an isolated display"]
    fn native_shelves_stack_as_one_card() {
        gtk4::init().expect("GTK init");
        let stack = ShelfStack::new();
        let top = Shelf::new("at-utilities-terminal-symbolic", "the top", None);
        let bottom = Shelf::new("at-utilities-terminal-symbolic", "the bottom", None);
        stack.push(&top.revealer);
        stack.push(&bottom.revealer);
        assert!(!stack.widget.is_visible(), "no shelf, no frame");
        assert_eq!(stack.showing(), [(false, false), (false, false)]);

        bottom.show(true);
        assert!(stack.widget.is_visible());
        assert_eq!(
            stack.showing(),
            [(false, false), (true, false)],
            "alone, the bottom shelf heads the frame"
        );
        top.show(true);
        assert_eq!(
            stack.showing(),
            [(true, false), (true, true)],
            "both: the bottom one is joined under the top one"
        );
        top.show(false);
        assert_eq!(stack.showing(), [(false, false), (true, false)]);
        bottom.show(false);
        assert!(
            !stack.widget.is_visible(),
            "the frame goes with the last shelf"
        );
    }
}

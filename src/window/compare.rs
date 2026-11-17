//! The compare window: two to four transcripts side by side, lined up by turn
//! ([`agent_core::compare::table`]). It is a window of its own, never a thread page, and it
//! only reads: nothing in it starts a session or sends anything to an agent.
//!
//! It knows nothing of where its columns come from. A caller opens it ([`CompareWindow::new`]),
//! starts a read with [`CompareWindow::begin`] (a spinner until the first answer), reads the
//! store off the main thread and hands the result to [`CompareWindow::show`]; only the newest
//! read draws. Refresh (the header's button) calls the caller back to read again.
//!
//! What it shows is already scrubbed (the store scrubs every event and title); replies are
//! markdown whose links open nothing but a web or mail address ([`crate::chat::view::guard_links`]).

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use agent_core::compare::{self, Entry, Role, RowKind};
use gtk4::glib;

use super::Generation;

/// One column as the window draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColumnView {
    /// The thread's title.
    pub(crate) heading: String,
    /// The agent and model, or why there is nothing (the thread is gone).
    pub(crate) detail: String,
    pub(crate) entries: Vec<Entry>,
}

/// What [`CompareWindow::show`] draws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompareContent {
    /// Under the window's title: what is compared from where.
    pub(crate) subtitle: String,
    pub(crate) columns: Vec<ColumnView>,
    /// Said in place of the rows when no column has anything.
    pub(crate) empty: String,
}

/// What Refresh calls ([`CompareWindow::connect_refresh`]).
type Refresh = Rc<dyn Fn(&CompareWindow)>;

struct Inner {
    window: adw::Window,
    title: adw::WindowTitle,
    stack: gtk4::Stack,
    refresh: gtk4::Button,
    generation: Generation,
    on_refresh: RefCell<Option<Refresh>>,
}

/// A compare window. Cheap to clone: every clone is the same window.
#[derive(Clone)]
pub(crate) struct CompareWindow(Rc<Inner>);

impl CompareWindow {
    /// A window titled `title`, sized from `parent` and closed with it, showing a spinner until
    /// the first [`Self::show`]. Not yet presented.
    pub(crate) fn new(parent: &gtk4::Window, title: &str) -> Self {
        let heading = adw::WindowTitle::new(title, "");
        let refresh = gtk4::Button::builder()
            .icon_name("at-view-refresh-symbolic")
            .tooltip_text("Read the threads again")
            .build();
        refresh.update_property(&[gtk4::accessible::Property::Label("Refresh the comparison")]);
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&heading));
        header.pack_start(&refresh);
        let stack = gtk4::Stack::new();
        let spinner = gtk4::Spinner::builder()
            .spinning(true)
            .width_request(32)
            .height_request(32)
            .halign(gtk4::Align::Center)
            .valign(gtk4::Align::Center)
            .build();
        spinner.update_property(&[gtk4::accessible::Property::Label("Reading the threads")]);
        stack.add_named(&spinner, Some("loading"));
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&stack));
        let window = adw::Window::builder()
            .title(title)
            .default_width(parent.width().clamp(720, 1280))
            .default_height((parent.height() * 9 / 10).clamp(480, 960))
            .content(&toolbar)
            .build();
        window.add_css_class("compare-window");
        // A dialog over the window closes on a press outside it, as Escape does.
        crate::dialogs::close_dialogs_on_outside_click(&window);
        // The thread window closing takes this one with it.
        let watch = RefCell::new(Some(parent.connect_destroy(glib::clone!(
            #[weak]
            window,
            move |_| window.destroy()
        ))));
        let parent_weak = parent.downgrade();
        window.connect_destroy(move |_| {
            if let (Some(parent), Some(id)) = (parent_weak.upgrade(), watch.borrow_mut().take()) {
                parent.disconnect(id);
            }
        });
        let inner = Rc::new(Inner {
            window,
            title: heading,
            stack,
            refresh,
            generation: Generation::new(),
            on_refresh: RefCell::new(None),
        });
        let weak = Rc::downgrade(&inner);
        inner.refresh.connect_clicked(move |_| {
            let Some(inner) = weak.upgrade() else { return };
            let callback = inner.on_refresh.borrow().clone();
            if let Some(callback) = callback {
                callback(&CompareWindow(inner));
            }
        });
        Self(inner)
    }

    /// The window itself.
    pub(crate) fn window(&self) -> &adw::Window {
        &self.0.window
    }

    /// Calls `refresh` with this window when Refresh is clicked: it should read again
    /// ([`Self::begin`]). Handed the window, so it need not hold one (which would keep the
    /// window alive through its own callback).
    pub(crate) fn connect_refresh(&self, refresh: impl Fn(&CompareWindow) + 'static) {
        *self.0.on_refresh.borrow_mut() = Some(Rc::new(refresh));
    }

    /// Starts a read: its generation, which only [`Self::show`] or [`Self::show_error`] of the
    /// newest read draws. What is shown stays until then; Refresh waits for the answer.
    pub(crate) fn begin(&self) -> u64 {
        self.0.refresh.set_sensitive(false);
        self.0.generation.next()
    }

    /// Draws `content`, unless a newer read has started since `generation`.
    pub(crate) fn show(&self, generation: u64, content: &CompareContent) {
        if !self.0.generation.is_current(generation) {
            return;
        }
        self.0.refresh.set_sensitive(true);
        self.0.title.set_subtitle(&content.subtitle);
        let page = content_page(content);
        self.replace_page(&page);
    }

    /// Says why the read failed, unless a newer read has started since `generation`.
    pub(crate) fn show_error(&self, generation: u64, why: &str) {
        if !self.0.generation.is_current(generation) {
            return;
        }
        self.0.refresh.set_sensitive(true);
        let page = adw::StatusPage::builder()
            .title("Could Not Read the Threads")
            .description(glib::markup_escape_text(why).as_str())
            .build();
        crate::icons::set_status_icon(&page, "at-dialog-error-symbolic");
        self.replace_page(page.upcast_ref());
    }

    fn replace_page(&self, page: &gtk4::Widget) {
        let stack = &self.0.stack;
        if let Some(old) = stack.child_by_name("shown") {
            stack.remove(&old);
        }
        stack.add_named(page, Some("shown"));
        stack.set_visible_child_name("shown");
    }
}

fn label(text: &str, classes: &[&str]) -> gtk4::Label {
    gtk4::Label::builder()
        .label(text)
        .wrap(true)
        .wrap_mode(gtk4::pango::WrapMode::WordChar)
        .xalign(0.0)
        .css_classes(classes.to_vec())
        .build()
}

/// The page for `content`: a heading per column, then the rows lined up by turn.
fn content_page(content: &CompareContent) -> gtk4::Widget {
    let columns: Vec<Vec<Entry>> = content.columns.iter().map(|c| c.entries.clone()).collect();
    let table = compare::table(&columns);
    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    let heads = gtk4::Grid::builder()
        .column_homogeneous(true)
        .column_spacing(18)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(18)
        .margin_end(18)
        .build();
    for (i, column) in content.columns.iter().enumerate() {
        let head = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        head.add_css_class("compare-column-head");
        let title = label(&column.heading, &["title-4"]);
        title.set_wrap(false);
        title.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        title.set_tooltip_text(Some(&column.heading));
        head.append(&title);
        head.append(&label(&column.detail, &["dim-label"]));
        head.append(&label(
            &compare::summary(&compare::totals(&column.entries)),
            &["dim-label", "caption", "compare-summary"],
        ));
        heads.attach(&head, i as i32, 0, 1, 1);
    }
    outer.append(&heads);
    outer.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
    if table.rows.is_empty() {
        let empty = adw::StatusPage::builder()
            .title("Nothing to Compare Yet")
            .description(glib::markup_escape_text(&content.empty).as_str())
            .vexpand(true)
            .build();
        outer.append(&empty);
        return outer.upcast();
    }
    let grid = gtk4::Grid::builder()
        .column_homogeneous(true)
        .column_spacing(18)
        .row_spacing(6)
        .margin_top(12)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();
    let width = content.columns.len().max(1) as i32;
    let mut at = 0;
    for row in &table.rows {
        let heading = label(&row.kind.label(), &["heading", "compare-row-heading"]);
        if at > 0 {
            heading.set_margin_top(12);
        }
        grid.attach(&heading, 0, at, width, 1);
        at += 1;
        for (i, cell) in row.cells.iter().enumerate() {
            grid.attach(&cell_widget(cell, row.kind), i as i32, at, 1, 1);
        }
        at += 1;
    }
    if table.more > 0 {
        let more = label(
            &format!(
                "{} not shown.",
                if table.more == 1 {
                    "1 later turn".to_owned()
                } else {
                    format!("{} later turns", table.more)
                }
            ),
            &["dim-label"],
        );
        more.set_margin_top(12);
        grid.attach(&more, 0, at, width, 1);
    }
    let scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .child(&grid)
        .build();
    outer.append(&scroll);
    outer.upcast()
}

/// One cell: its entries in order, or a dim note when the column has no such turn.
fn cell_widget(cell: &[Entry], kind: RowKind) -> gtk4::Widget {
    let cell_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(6)
        .valign(gtk4::Align::Start)
        .css_classes(["compare-cell"])
        .build();
    if cell.is_empty() {
        let none = match kind {
            RowKind::Earlier => "Nothing before the first message",
            RowKind::Turn(_) => "No such turn",
        };
        cell_box.append(&label(none, &["dim-label", "compare-none"]));
        return cell_box.upcast();
    }
    for entry in cell {
        cell_box.append(&entry_widget(entry));
    }
    cell_box.upcast()
}

/// What an entry's status adds to its line: nothing once it completed.
fn status_note(entry: &Entry) -> Option<&'static str> {
    if entry.open() {
        return Some("running");
    }
    match entry.status.as_str() {
        "failed" => Some("failed"),
        "declined" => Some("declined"),
        "interrupted" => Some("interrupted"),
        _ => None,
    }
}

fn entry_widget(entry: &Entry) -> gtk4::Widget {
    let text = compare::clip(&entry.text);
    match entry.role {
        Role::Prompt => {
            let prompt = label(&text, &["compare-prompt"]);
            prompt.set_selectable(true);
            prompt.set_margin_top(6);
            prompt.set_margin_bottom(6);
            prompt.set_margin_start(8);
            prompt.set_margin_end(8);
            let frame = gtk4::Box::builder()
                .orientation(gtk4::Orientation::Vertical)
                .css_classes(["card"])
                .build();
            frame.append(&prompt);
            frame.upcast()
        }
        Role::Reply => {
            let reply = gtk4::Label::builder()
                .use_markup(true)
                .label(crate::chat::view::markdown::passage_markup(&text))
                .wrap(true)
                .wrap_mode(gtk4::pango::WrapMode::WordChar)
                .xalign(0.0)
                .selectable(true)
                .css_classes(["compare-reply"])
                .build();
            // A path in a reply is no file to open from here.
            crate::chat::view::guard_links(&reply);
            reply.upcast()
        }
        Role::Tool => {
            let line = match status_note(entry) {
                Some(note) => format!("▸ {text} ({note})"),
                None => format!("▸ {text}"),
            };
            let tool = label(&line, &["dim-label", "compare-tool"]);
            tool.set_selectable(true);
            if entry.failed() {
                tool.add_css_class("error");
            }
            tool.upcast()
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn e(role: Role, text: &str, status: &str) -> Entry {
        Entry {
            role,
            text: text.to_owned(),
            status: status.to_owned(),
            seq: 0,
        }
    }

    #[test]
    fn a_tool_line_says_how_it_ended_unless_it_completed() {
        assert_eq!(status_note(&e(Role::Tool, "Run x", "completed")), None);
        assert_eq!(
            status_note(&e(Role::Tool, "Run x", "open")),
            Some("running")
        );
        assert_eq!(
            status_note(&e(Role::Tool, "Run x", "failed")),
            Some("failed")
        );
        assert_eq!(
            status_note(&e(Role::Tool, "Run x", "declined")),
            Some("declined")
        );
    }

    /// The window lines two columns up by turn, says what a short column lacks, draws only the
    /// newest read, and its every button has a name. Run from the window test (GTK is
    /// initialised there).
    pub(crate) fn the_window_lines_columns_up_by_turn(parent: &gtk4::Window) {
        let ctx = glib::MainContext::default();
        let window = CompareWindow::new(parent, "Compare Forks");
        let root: gtk4::Widget = window.window().clone().upcast();
        assert!(
            crate::testutil::find_first::<gtk4::Spinner>(&root).is_some(),
            "a spinner until the first read answers"
        );
        let refreshed = Rc::new(std::cell::Cell::new(0));
        let r = refreshed.clone();
        window.connect_refresh(move |_| r.set(r.get() + 1));
        let content = CompareContent {
            subtitle: "“A” and its fork “B”".into(),
            columns: vec![
                ColumnView {
                    heading: "A".into(),
                    detail: "Claude".into(),
                    entries: vec![
                        e(Role::Prompt, "left one", "completed"),
                        e(Role::Reply, "**left** reply", "completed"),
                        e(Role::Prompt, "left two", "completed"),
                    ],
                },
                ColumnView {
                    heading: "B".into(),
                    detail: "Codex".into(),
                    entries: vec![
                        e(Role::Prompt, "right one", "completed"),
                        e(Role::Tool, "Run cargo test", "failed"),
                    ],
                },
            ],
            empty: "Nothing yet.".into(),
        };
        let stale = window.begin();
        let current = window.begin();
        window.show(stale, &content);
        assert!(
            crate::testutil::find_first::<gtk4::Grid>(&root).is_none(),
            "a superseded read draws nothing"
        );
        window.show(current, &content);
        window.window().present();
        assert!(crate::testutil::pump_until(&ctx, 10, || window
            .window()
            .is_mapped()));
        let texts = crate::testutil::label_texts(&root);
        for expected in [
            "Turn 1",
            "Turn 2",
            "left one",
            "right one",
            "left reply",
            "▸ Run cargo test (failed)",
            "No such turn",
            "1 message · 0 replies · 1 tool call (1 failed)",
        ] {
            assert!(
                texts.iter().any(|t| t == expected),
                "{expected:?} in {texts:#?}"
            );
        }
        // Turn 2 has only the left column; the right one says so, in the same row.
        let cells = crate::testutil::find_all_by_class::<gtk4::Box>(&root, "compare-cell");
        assert_eq!(cells.len(), 4, "two rows of two cells");
        let problems = crate::testutil::button_problems(&root, 24);
        assert!(problems.is_empty(), "{problems:#?}");
        let unlabelled = crate::testutil::icon_only_without_label(&root);
        assert!(unlabelled.is_empty(), "{unlabelled:#?}");
        // Refresh asks the caller to read again, and waits for that read.
        let refresh = crate::testutil::find_all::<gtk4::Button>(&root)
            .into_iter()
            .find(|b| b.icon_name().as_deref() == Some("at-view-refresh-symbolic"))
            .expect("refresh");
        refresh.emit_clicked();
        assert_eq!(refreshed.get(), 1);
        let again = window.begin();
        assert!(!refresh.is_sensitive(), "no second read while one runs");
        window.show_error(again, "the store is busy");
        assert!(refresh.is_sensitive());
        assert!(crate::testutil::label_texts(&root)
            .iter()
            .any(|t| t == "the store is busy"));
        // Nothing in either column: the empty note, never a blank window.
        let empty = CompareContent {
            columns: content
                .columns
                .iter()
                .map(|c| ColumnView {
                    entries: Vec::new(),
                    ..c.clone()
                })
                .collect(),
            ..content.clone()
        };
        let g = window.begin();
        window.show(g, &empty);
        let texts = crate::testutil::label_texts(&root);
        assert!(
            texts.iter().any(|t| t == "Nothing to Compare Yet"),
            "{texts:#?}"
        );
        assert!(
            texts.iter().any(|t| t == "Nothing yet"),
            "each column's summary"
        );
        window.window().close();
        assert!(crate::testutil::pump_until(&ctx, 10, || !window
            .window()
            .is_visible()));
    }
}

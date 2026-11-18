//! The compare window: two to four transcripts side by side, lined up by turn
//! ([`agent_core::compare::table`]). It is a window of its own, never a thread page, and it
//! only reads: nothing in it starts a session or sends anything to an agent.
//!
//! It knows nothing of where its columns come from. A caller opens it ([`CompareWindow::new`]),
//! starts a read with [`CompareWindow::begin`] (a spinner until the first answer), reads the
//! store off the main thread and hands the result to [`CompareWindow::show`]; only the newest
//! read draws. Refresh (the header's button) calls the caller back to read again.
//!
//! A caller with file changes to show starts a read of the Changes tab too
//! ([`CompareWindow::begin_changes`], which shows the tab switcher) and hands each side's diff to
//! [`CompareWindow::show_changes`]: a column per side (or one, standing for sides that share a
//! folder, with a note saying what that means), the paths changed on every side, and each
//! side's files laid out as the diff panel lays them out, read-only
//! ([`super::diff_panel::read_only_files`]). The diffs are read off the main thread by the
//! caller.
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

/// One column of the Changes tab: what changed in a thread's folder since its starting point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChangesState {
    /// The folder is not in a git repository.
    NotRepo,
    /// Why there is nothing to show (no starting point, git busy, the folder gone).
    Unavailable(String),
    /// The diff from the starting point to the folder as it is now.
    Ready(crate::git::TabDiff),
}

/// A column of the Changes tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChangesColumn {
    pub(crate) heading: String,
    pub(crate) state: ChangesState,
}

/// What [`CompareWindow::show_changes`] draws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChangesContent {
    /// Said above the columns: what the diffs can and cannot tell (one folder shared).
    pub(crate) note: Option<String>,
    /// True when the sides share one working folder, so overlapping paths are not "changed on
    /// both sides".
    pub(crate) shared: bool,
    /// One column per side, or a single one standing for several that are one and the same.
    pub(crate) columns: Vec<ChangesColumn>,
}

/// The paths changed in every one of `columns` (each ready, at least two), in the first one's
/// order: where the sides' work overlaps.
pub(crate) fn changed_in_all(columns: &[ChangesColumn]) -> Vec<String> {
    let diffs: Vec<&crate::git::TabDiff> = columns
        .iter()
        .filter_map(|c| match &c.state {
            ChangesState::Ready(diff) => Some(diff),
            _ => None,
        })
        .collect();
    if diffs.len() < 2 || diffs.len() != columns.len() {
        return Vec::new();
    }
    diffs[0]
        .stats
        .iter()
        .map(|s| s.path.clone())
        .filter(|path| {
            diffs[1..]
                .iter()
                .all(|d| d.stats.iter().any(|s| &s.path == path))
        })
        .collect()
}

/// What Refresh calls ([`CompareWindow::connect_refresh`]).
type Refresh = Rc<dyn Fn(&CompareWindow)>;

struct Inner {
    window: adw::Window,
    title: adw::WindowTitle,
    stack: gtk4::Stack,
    /// Switches between the Transcripts and Changes tabs; shown once a Changes read has begun.
    switcher: adw::ViewSwitcher,
    changes: gtk4::Stack,
    changes_generation: Generation,
    refresh: gtk4::Button,
    generation: Generation,
    on_refresh: RefCell<Option<Refresh>>,
    /// The window's own hold on itself, so it lives (and Refresh works) with no caller keeping
    /// it: let go once its `adw::Window` is destroyed (closed, or its parent gone).
    kept: RefCell<Option<CompareWindow>>,
}

/// A compare window. Cheap to clone: every clone is the same window, which stays alive until
/// its `adw::Window` is destroyed, whoever holds a clone.
#[derive(Clone)]
pub(crate) struct CompareWindow(Rc<Inner>);

/// A weak hold on a [`CompareWindow`], for async reads that must not keep a closed window alive.
#[derive(Clone)]
pub(crate) struct WeakCompareWindow(std::rc::Weak<Inner>);

impl WeakCompareWindow {
    pub(crate) fn upgrade(&self) -> Option<CompareWindow> {
        self.0.upgrade().map(CompareWindow)
    }
}

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
        stack.add_named(&spinner("Reading the threads"), Some("loading"));
        let changes = gtk4::Stack::new();
        changes.add_named(&spinner("Reading the file changes"), Some("loading"));
        let tabs = adw::ViewStack::new();
        tabs.add_titled(&stack, Some("transcripts"), "Transcripts");
        tabs.add_titled(&changes, Some("changes"), "Changes");
        let switcher = adw::ViewSwitcher::builder()
            .stack(&tabs)
            .policy(adw::ViewSwitcherPolicy::Wide)
            .visible(false)
            .tooltip_text("Show the transcripts or the file changes")
            .build();
        header.pack_end(&switcher);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&tabs));
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
            switcher,
            changes,
            changes_generation: Generation::new(),
            refresh,
            generation: Generation::new(),
            on_refresh: RefCell::new(None),
            kept: RefCell::new(None),
        });
        *inner.kept.borrow_mut() = Some(Self(inner.clone()));
        // Let go once it is closed (or destroyed with its parent). GTK holds the window
        // through its own signal, so the last hold may go here.
        let release = {
            let weak = Rc::downgrade(&inner);
            move || {
                if let Some(inner) = weak.upgrade() {
                    let kept = inner.kept.borrow_mut().take();
                    drop(inner);
                    drop(kept);
                }
            }
        };
        let on_close = release.clone();
        inner.window.connect_close_request(move |_| {
            on_close();
            glib::Propagation::Proceed
        });
        inner.window.connect_destroy(move |_| release());
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

    /// A weak hold on this window, for async work.
    pub(crate) fn downgrade(&self) -> WeakCompareWindow {
        WeakCompareWindow(Rc::downgrade(&self.0))
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
        replace_shown(&self.0.stack, page);
    }

    /// Starts a read of the Changes tab (showing the tab's switcher from then on): its
    /// generation, which only the newest [`Self::show_changes`] or
    /// [`Self::show_changes_error`] draws.
    pub(crate) fn begin_changes(&self) -> u64 {
        self.0.switcher.set_visible(true);
        self.0.changes_generation.next()
    }

    /// Draws the Changes tab, unless a newer read of it has started since `generation`.
    pub(crate) fn show_changes(&self, generation: u64, content: &ChangesContent) {
        if !self.0.changes_generation.is_current(generation) {
            return;
        }
        replace_shown(&self.0.changes, &changes_page(content));
    }

    /// Says why the Changes tab could not be read, unless a newer read has started.
    pub(crate) fn show_changes_error(&self, generation: u64, why: &str) {
        if !self.0.changes_generation.is_current(generation) {
            return;
        }
        let page = adw::StatusPage::builder()
            .title("Could Not Read the Changes")
            .description(glib::markup_escape_text(why).as_str())
            .build();
        crate::icons::set_status_icon(&page, "at-dialog-error-symbolic");
        replace_shown(&self.0.changes, page.upcast_ref());
    }
}

fn spinner(name: &str) -> gtk4::Spinner {
    let spinner = gtk4::Spinner::builder()
        .spinning(true)
        .width_request(32)
        .height_request(32)
        .halign(gtk4::Align::Center)
        .valign(gtk4::Align::Center)
        .build();
    spinner.update_property(&[gtk4::accessible::Property::Label(name)]);
    spinner
}

/// Puts `page` in `stack` as its shown page, in place of the one before.
fn replace_shown(stack: &gtk4::Stack, page: &gtk4::Widget) {
    if let Some(old) = stack.child_by_name("shown") {
        stack.remove(&old);
    }
    stack.add_named(page, Some("shown"));
    stack.set_visible_child_name("shown");
}

/// The Changes tab for `content`: its note, the files changed on every side, then a column
/// per side with its totals and its files.
fn changes_page(content: &ChangesContent) -> gtk4::Widget {
    let outer = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();
    if let Some(note) = &content.note {
        let note = label(note, &["compare-changes-note", "warning"]);
        outer.append(&note);
    }
    let in_all = if content.shared {
        Vec::new()
    } else {
        changed_in_all(&content.columns)
    };
    if !in_all.is_empty() {
        let both = label(
            &format!("Changed on both sides: {}", in_all.join(", ")),
            &["compare-changed-in-all"],
        );
        both.set_selectable(true);
        outer.append(&both);
    }
    let grid = gtk4::Grid::builder()
        .column_homogeneous(true)
        .column_spacing(18)
        .build();
    for (i, column) in content.columns.iter().enumerate() {
        let col = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .spacing(6)
            .valign(gtk4::Align::Start)
            .css_classes(["compare-changes-column"])
            .build();
        let title = label(&column.heading, &["title-4"]);
        title.set_wrap(false);
        title.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        title.set_tooltip_text(Some(&column.heading));
        col.append(&title);
        match &column.state {
            ChangesState::NotRepo => col.append(&label(
                "This folder is not in a git repository, so there are no file changes to show.",
                &["dim-label"],
            )),
            ChangesState::Unavailable(why) => col.append(&label(why, &["dim-label"])),
            ChangesState::Ready(diff) if diff.stats.is_empty() => {
                col.append(&label("No files changed.", &["dim-label"]))
            }
            ChangesState::Ready(diff) => {
                col.append(&label(
                    &crate::diff::summary(&diff.stats),
                    &["dim-label", "caption", "compare-changes-summary"],
                ));
                col.append(&super::diff_panel::read_only_files(diff));
            }
        }
        grid.attach(&col, i as i32, 0, 1, 1);
    }
    outer.append(&grid);
    gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .child(&outer)
        .build()
        .upcast()
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
        let alive = Rc::downgrade(&window.0);
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
        // The Changes tab: hidden until a read of it begins, then only its newest read draws.
        let switcher =
            crate::testutil::find_first::<adw::ViewSwitcher>(&root).expect("the tab switcher");
        assert!(
            !switcher.is_visible(),
            "no Changes tab without changes to read"
        );
        let stale = window.begin_changes();
        let current = window.begin_changes();
        assert!(switcher.is_visible());
        let text = "diff --git a/src/x.rs b/src/x.rs\n--- a/src/x.rs\n+++ b/src/x.rs\n\
                    @@ -1 +1 @@\n-old\n+new\n";
        let changed = crate::git::TabDiff {
            stats: vec![crate::diff::FileStat {
                path: "src/x.rs".into(),
                old_path: None,
                added: Some(1),
                deleted: Some(1),
            }],
            text: text.into(),
            omitted_lines: 3,
            too_large: false,
            undo_to: None,
            from: "base".into(),
            to_checkpoint: None,
        };
        let changes = ChangesContent {
            note: Some("One folder.".into()),
            shared: true,
            columns: vec![
                ChangesColumn {
                    heading: "A".into(),
                    state: ChangesState::Ready(changed.clone()),
                },
                ChangesColumn {
                    heading: "B".into(),
                    state: ChangesState::Unavailable("Git is busy".into()),
                },
            ],
        };
        window.show_changes(stale, &changes);
        assert!(
            crate::testutil::find_first::<gtk4::Expander>(&root).is_none(),
            "a superseded read of the changes draws nothing"
        );
        window.show_changes(current, &changes);
        let texts = crate::testutil::label_texts(&root);
        for expected in [
            "One folder.",
            "1 file, +1 −1",
            "src/",
            "x.rs  +1 −1",
            "Git is busy",
            "… diff truncated: 3 more lines not shown",
        ] {
            assert!(
                texts.iter().any(|t| t == expected),
                "{expected:?} in {texts:#?}"
            );
        }
        let expander =
            crate::testutil::find_first::<gtk4::Expander>(&root).expect("the file's row");
        assert!(
            expander.child().is_none(),
            "a file's diff is built when first opened"
        );
        expander.set_expanded(true);
        let scroller = expander
            .child()
            .and_downcast::<gtk4::ScrolledWindow>()
            .expect("the file's diff scroller");
        let view = scroller
            .child()
            .and_downcast::<gtk4::TextView>()
            .expect("the file's diff");
        assert!(!view.is_editable());
        let buffer = view.buffer();
        let shown = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
        assert!(shown.contains("+new"), "{shown}");
        // Live theme switch recolours open read-only diff views.
        let before_added = buffer
            .tag_table()
            .lookup("added")
            .and_then(|t| t.foreground_rgba());
        crate::palette::apply(
            crate::config::ThemeChoice::SolarizedLight,
            crate::config::ThemePair::default(),
        );
        let after_added = buffer
            .tag_table()
            .lookup("added")
            .and_then(|t| t.foreground_rgba());
        assert_ne!(before_added, after_added);
        crate::palette::apply(
            crate::config::ThemeChoice::default(),
            crate::config::ThemePair::default(),
        );

        // Two shared-folder columns never say "Changed on both sides".
        let shared_two = ChangesContent {
            note: Some("One folder.".into()),
            shared: true,
            columns: vec![
                ChangesColumn {
                    heading: "A".into(),
                    state: ChangesState::Ready(changed.clone()),
                },
                ChangesColumn {
                    heading: "B".into(),
                    state: ChangesState::Ready(changed.clone()),
                },
            ],
        };
        let g = window.begin_changes();
        window.show_changes(g, &shared_two);
        assert!(
            !crate::testutil::label_texts(&root)
                .iter()
                .any(|t| t.starts_with("Changed on both sides:")),
            "shared folder suppresses 'Changed on both sides'"
        );

        // Both sides changed one path in separate folders: named once, above the columns.
        let both = ChangesContent {
            note: None,
            shared: false,
            columns: vec![
                ChangesColumn {
                    heading: "A".into(),
                    state: ChangesState::Ready(changed.clone()),
                },
                ChangesColumn {
                    heading: "B".into(),
                    state: ChangesState::Ready(changed),
                },
            ],
        };
        assert_eq!(changed_in_all(&both.columns), ["src/x.rs"]);
        let g = window.begin_changes();
        window.show_changes(g, &both);
        assert!(crate::testutil::label_texts(&root)
            .iter()
            .any(|t| t == "Changed on both sides: src/x.rs"));
        let g = window.begin_changes();
        window.show_changes_error(g, "the store is busy");
        assert!(crate::testutil::label_texts(&root)
            .iter()
            .any(|t| t == "Could Not Read the Changes"));
        let problems = crate::testutil::button_problems(&root, 24);
        assert!(problems.is_empty(), "{problems:#?}");
        let unnamed = crate::testutil::icon_only_without_label(&root);
        assert!(unnamed.is_empty(), "{unnamed:#?}");

        // Nobody else holds it: Refresh still reaches the caller while the window is open.
        let shown = window.window().clone();
        drop(window);
        assert!(
            alive.upgrade().is_some(),
            "an open window keeps itself alive"
        );
        refresh.emit_clicked();
        assert_eq!(
            refreshed.get(),
            2,
            "Refresh works with no caller holding the window"
        );
        // Closed, it lets go of itself.
        shown.close();
        assert!(
            crate::testutil::pump_until(&ctx, 10, || alive.upgrade().is_none()),
            "a closed window is freed"
        );
    }
}

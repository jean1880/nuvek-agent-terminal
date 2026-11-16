//! The per-tab diff panel: a view of what changed in the tab's repository — uncommitted, over
//! the last turn, or since the tab opened. It writes nothing itself: on Uncommitted its rows have
//! tick boxes and its "Commit…" bar hands the ticked files to the window, which commits them
//! after the user confirms.
//!
//! The changed files are listed under a header per folder, each file a
//! collapsed row (name and `+3 −1`) that expands to its own diff, so the diffs
//! are separate and explored one at a time. A file's diff view is built the
//! first time it is expanded: a large change set lists instantly.
//!
//! This module only builds and fills widgets. The diff itself comes from
//! [`crate::git::tab_diff`], run off the main thread by the window, and the
//! text logic (classifying, splitting by file, grouping by folder) is in
//! [`crate::diff`].

use crate::diff::{DiffBase, LineKind};
use crate::diff_tool::DiffTools;
use crate::editor_tool::EditorTools;
use crate::git::{DiffOutcome, TabDiff};
use crate::theme::DiffColours;
use adw::prelude::*;
use agent_kit::commit::InProgress;
use agent_kit::github::{PrOutcome, PrState, PushState};
use gtk4::glib;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::rc::Rc;

mod review;

/// Narrowest width the panel remembers being dragged to.
pub const MIN_WIDTH: i32 = 220;

const TAG_ADDED: &str = "added";
const TAG_REMOVED: &str = "removed";
const TAG_HUNK: &str = "hunk";
const TAG_META: &str = "meta";
const TAG_FILE: &str = "file";

/// CSS class of every diff view, styled by [`apply_view_colours`].
const VIEW_CLASS: &str = "agent-diff-view";

thread_local! {
    /// One provider for every panel: the theme is global, so they all share
    /// its colours. GTK objects stay on the main thread, hence thread-local.
    static VIEW_CSS: (gtk4::CssProvider, Cell<bool>) =
        (gtk4::CssProvider::new(), Cell::new(false));
}

/// Paints diff views with the terminal's background and text colours, so
/// the panel reads as part of the terminal whatever the desktop's own light
/// or dark style.
fn apply_view_colours(colours: &DiffColours) {
    let css = format!(
        ".{VIEW_CLASS}, .{VIEW_CLASS} text {{ background-color: {}; color: {}; }}",
        colours.background, colours.text
    );
    VIEW_CSS.with(|(provider, installed)| {
        provider.load_from_data(&css);
        if !installed.get() {
            if let Some(display) = gtk4::gdk::Display::default() {
                gtk4::style_context_add_provider_for_display(
                    &display,
                    provider,
                    gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
                );
                installed.set(true);
            }
        }
    });
}

/// Gives `view`'s line tags the diff colours.
fn colour_tags(view: &gtk4::TextView, colours: &DiffColours) {
    let table = view.buffer().tag_table();
    for (name, colour) in [
        (TAG_ADDED, &colours.added),
        (TAG_REMOVED, &colours.removed),
        (TAG_HUNK, &colours.hunk),
        (TAG_META, &colours.meta),
        (TAG_FILE, &colours.file),
    ] {
        if let Some(tag) = table.lookup(name) {
            tag.set_foreground_rgba(Some(colour));
        }
    }
}

/// A read-only view of one file's diff, each line coloured by its kind.
fn file_view(text: &str, colours: &DiffColours) -> gtk4::TextView {
    let view = gtk4::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk4::WrapMode::None)
        .left_margin(6)
        .right_margin(6)
        .top_margin(4)
        .bottom_margin(4)
        .build();
    // Added, not set through the builder: `css_classes` replaces the list,
    // and with it the `monospace` class that `.monospace(true)` put there.
    view.add_css_class(VIEW_CLASS);
    let buffer = view.buffer();
    for name in [TAG_ADDED, TAG_REMOVED, TAG_HUNK, TAG_META, TAG_FILE] {
        buffer.create_tag(Some(name), &[]);
    }
    if let Some(tag) = buffer.tag_table().lookup(TAG_FILE) {
        tag.set_weight(700);
    }
    colour_tags(&view, colours);
    buffer.set_text(text.strip_suffix('\n').unwrap_or(text));
    for (line, kind) in crate::diff::classify(text).into_iter().enumerate() {
        let tag = match kind {
            LineKind::Added => TAG_ADDED,
            LineKind::Removed => TAG_REMOVED,
            LineKind::Hunk => TAG_HUNK,
            LineKind::Meta => TAG_META,
            LineKind::File => TAG_FILE,
            LineKind::Context => continue,
        };
        let Ok(line) = i32::try_from(line) else {
            break;
        };
        if let Some(start) = buffer.iter_at_line(line) {
            let mut end = start;
            end.forward_to_line_end();
            buffer.apply_tag_by_name(tag, &start, &end);
        }
    }
    view
}

/// What a Ctrl+click on `row` of a file's diff opens: the file at the row's line in the new
/// file ([`crate::diff::new_line_at`]: an added or context row is its own line, a removed row or
/// a hunk header the nearest one). `None` for a row with no line in the new file (the file
/// header, a deleted file, a row past what was shown): such a click opens nothing rather than
/// guessing, and stays a plain click.
fn ctrl_click_edit(text: &str, path: &str, row: usize) -> Option<EditFile> {
    Some(EditFile {
        path: path.to_owned(),
        line: Some(crate::diff::new_line_at(text, row)?),
    })
}

/// Ctrl+click on a row of `view` (the diff `text` of `path`) opens the file in the editor at
/// that row's line; a plain click still selects text. A click on a row with no line is left to
/// the view.
fn open_at_ctrl_click(
    view: &gtk4::TextView,
    text: &str,
    path: &str,
    edit: &Rc<RefCell<Option<EditHandler>>>,
) {
    let click = gtk4::GestureClick::new();
    click.set_button(gtk4::gdk::BUTTON_PRIMARY);
    // Before the view's own gesture, so a claimed Ctrl+click never starts a selection.
    click.set_propagation_phase(gtk4::PropagationPhase::Capture);
    let (text, path, edit) = (text.to_owned(), path.to_owned(), Rc::clone(edit));
    click.connect_pressed(move |gesture, presses, x, y| {
        if presses != 1
            || !gesture
                .current_event_state()
                .contains(gtk4::gdk::ModifierType::CONTROL_MASK)
        {
            return;
        }
        let Some(view) = gesture.widget().and_downcast::<gtk4::TextView>() else {
            return;
        };
        let (bx, by) =
            view.window_to_buffer_coords(gtk4::TextWindowType::Widget, x as i32, y as i32);
        let Some(row) = view
            .iter_at_location(bx, by)
            .and_then(|at| usize::try_from(at.line()).ok())
        else {
            return;
        };
        let (Some(file), Some(handler)) =
            (ctrl_click_edit(&text, &path, row), edit.borrow().clone())
        else {
            return;
        };
        gesture.set_state(gtk4::EventSequenceState::Claimed);
        handler(file);
    });
    view.add_controller(click);
}

/// What the "Open a PR" bar says and offers for a branch's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrWords {
    /// The line beside the button: where the branch would go, and what is missing.
    pub status: String,
    pub button: &'static str,
    /// Whether the button can be pressed.
    pub enabled: bool,
}

/// The bar's words for `outcome` (the result of `agent_kit::github::pr_state`). Three states,
/// never two: ready, not possible here (and why), and could not be read.
pub fn pr_words(outcome: &Result<PrOutcome, String>) -> PrWords {
    let off = |status: String| PrWords {
        status,
        button: "Open a PR",
        enabled: false,
    };
    let state = match outcome {
        Err(why) => return off(format!("Could not read the branch: {why}")),
        Ok(PrOutcome::Unavailable(why)) => return off(why.clone()),
        Ok(PrOutcome::Ready(state)) => state,
    };
    if state.github.is_none() {
        return off(format!(
            "{} is not a GitHub remote, so there is no pull request to open",
            state.remote
        ));
    }
    if state.on_base() {
        return off(format!(
            "On {}: switch to a feature branch to open a pull request",
            state.branch
        ));
    }
    let into = state.base.as_deref().unwrap_or("the default branch");
    let mut status = format!("{} → {into}", state.branch);
    match state.push {
        PushState::NoUpstream => status.push_str(" · not pushed yet"),
        PushState::Ahead(1) => status.push_str(" · 1 commit to push"),
        PushState::Ahead(n) => status.push_str(&format!(" · {n} commits to push")),
        PushState::Unknown => status.push_str(" · may need pushing"),
        PushState::UpToDate => {}
    }
    if state.uncommitted {
        status.push_str(" · uncommitted changes are not included");
    }
    PrWords {
        status,
        button: if state.needs_push() {
            "Push & Open PR"
        } else {
            "Open a PR"
        },
        enabled: true,
    }
}

/// A file the user asked to open in the external diff tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenFile {
    /// Repository-relative, as git named it.
    pub path: String,
    /// The commit or tree the old side is read from.
    pub from: String,
    /// The new side when it is a checkpoint; `None`: the working file.
    pub to_checkpoint: Option<String>,
}

/// A file the user asked to open in the editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditFile {
    /// Repository-relative, as the diff named it.
    pub path: String,
    /// The first changed line in the new file, when the row's diff names one.
    pub line: Option<u32>,
}

/// A file a commit from the panel would include.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitFile {
    /// Repository-relative, as the diff named it.
    pub path: String,
    /// The path it was renamed from: committed with it, so the rename is whole.
    pub old_path: Option<String>,
}

impl CommitFile {
    /// Every path the commit must name for this file.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.path.as_str()).chain(self.old_path.as_deref())
    }
}

/// What "Commit…" hands the window: the repository whose diff is on show, the files chosen in
/// it, and how many were listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitAsk {
    pub repo: PathBuf,
    pub files: Vec<CommitFile>,
    pub listed: usize,
}

/// Why the panel offers no commit right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitOff {
    /// The diff compares checkpoints or a target branch, not the working tree: no bar.
    OtherBase,
    /// Nothing uncommitted is listed (clean, not a repository, or not read yet): no bar.
    NoChanges,
    /// A merge, rebase, cherry-pick or revert is half done.
    InProgress(InProgress),
    /// The push behind "Push & Open PR" is running.
    Pushing,
    /// Every file is unticked.
    NothingChosen,
}

impl CommitOff {
    /// Whether the bar is hidden altogether, rather than shown with its reason.
    pub fn hides_bar(self) -> bool {
        match self {
            CommitOff::OtherBase | CommitOff::NoChanges => true,
            CommitOff::InProgress(_) | CommitOff::Pushing | CommitOff::NothingChosen => false,
        }
    }

    /// The sentence beside the disabled button.
    pub fn reason(self) -> &'static str {
        match self {
            CommitOff::OtherBase => "Commits are made from the Uncommitted view",
            CommitOff::NoChanges => "Nothing to commit",
            CommitOff::InProgress(op) => op.reason(),
            CommitOff::Pushing => "Wait for the push to finish",
            CommitOff::NothingChosen => "Tick the files to commit",
        }
    }
}

/// Why no commit is offered, if one is not: the view on show, how many files it lists and how
/// many are ticked, the operation git has half done, and whether a push runs.
pub fn commit_off(
    uncommitted: bool,
    listed: usize,
    chosen: usize,
    op: Option<InProgress>,
    pushing: bool,
) -> Option<CommitOff> {
    if !uncommitted {
        Some(CommitOff::OtherBase)
    } else if listed == 0 {
        Some(CommitOff::NoChanges)
    } else if let Some(op) = op {
        Some(CommitOff::InProgress(op))
    } else if pushing {
        Some(CommitOff::Pushing)
    } else if chosen == 0 {
        Some(CommitOff::NothingChosen)
    } else {
        None
    }
}

/// Whether a listed file can be ticked for a commit. A name that is not valid UTF-8 reached the
/// panel decoded lossily, and would name some other file to git.
pub fn committable(stat: &crate::diff::FileStat) -> bool {
    agent_kit::commit::committable_path(&stat.path)
        && stat
            .old_path
            .as_deref()
            .is_none_or(agent_kit::commit::committable_path)
}

/// Why a row has no tick box that can be ticked.
pub const UNCOMMITTABLE: &str =
    "This file's name cannot be passed on exactly (it is not valid UTF-8): commit it in a terminal";

/// The tick box of a row that cannot be committed from here: unticked, off, and saying why.
fn uncommittable_tick() -> gtk4::CheckButton {
    let check = gtk4::CheckButton::builder()
        .valign(gtk4::Align::Center)
        .active(false)
        .sensitive(false)
        .tooltip_text(UNCOMMITTABLE)
        .build();
    check.update_property(&[gtk4::accessible::Property::Label(UNCOMMITTABLE)]);
    check
}

/// The words beside a usable Commit button.
pub fn chosen_words(chosen: usize, listed: usize) -> String {
    match (chosen, listed) {
        (1, 1) => "1 file to commit".to_owned(),
        (c, l) if c == l => format!("All {l} files to commit"),
        (c, l) => format!("{c} of {l} files to commit"),
    }
}

type OpenHandler = Rc<dyn Fn(OpenFile)>;
type EditHandler = Rc<dyn Fn(EditFile)>;
type PrHandler = Rc<dyn Fn(PrState)>;
type CommitHandler = Rc<dyn Fn(CommitAsk)>;
/// A row's tick box, held weakly (its row owns it), with the row's path.
type Tick = (glib::WeakRef<gtk4::CheckButton>, String);
type PopHandler = Rc<dyn Fn()>;
/// A popped-out panel's parent window and its destroy handler.
type ParentWatch = (glib::WeakRef<gtk4::Window>, glib::SignalHandlerId);

/// The diff on show: its left side, and its right side when that is a checkpoint.
type Sides = (String, Option<String>);

/// The "Commit…" bar and the files it would commit. Cheap to clone: every field is a reference.
#[derive(Clone)]
struct CommitBar {
    bar: gtk4::Box,
    /// Ticks or unticks every file.
    all: gtk4::CheckButton,
    status: gtk4::Label,
    button: gtk4::Button,
    face: gtk4::Stack,
    spinner: gtk4::Spinner,
    data: CommitData,
}

/// The commit bar's state, shared by the panel and the bar's handlers. Cheap to clone: every
/// field is a reference.
#[derive(Clone)]
struct CommitData {
    /// A commit is running (`set_committing`).
    committing: Rc<Cell<bool>>,
    /// "Commit…" was pressed and its dialog is not up yet (`set_preparing`): a second press
    /// would open a second dialog.
    preparing: Rc<Cell<bool>>,
    /// The files the Uncommitted diff on show lists (empty for another base, or none).
    listed: Rc<RefCell<Vec<CommitFile>>>,
    /// Paths the user unticked. Kept across refreshes, so a file that appears starts ticked
    /// and one the user left out stays out.
    unticked: Rc<RefCell<BTreeSet<String>>>,
    /// Each listed row's tick box (held weakly: its row owns it), with its path.
    ticks: Rc<RefCell<Vec<Tick>>>,
    /// The repository the diff on show was read from.
    repo: Rc<RefCell<Option<PathBuf>>>,
    /// The operation git has half done there, as the last refresh read it.
    op: Rc<Cell<Option<InProgress>>>,
    /// The diff on show is the Uncommitted one.
    uncommitted: Rc<Cell<bool>>,
    /// Set while code moves the tick boxes, so that is not taken for the user's choice.
    syncing: Rc<Cell<bool>>,
    handler: Rc<RefCell<Option<CommitHandler>>>,
    /// The message being written, kept while the dialog is closed; cleared by a commit. Never
    /// stored anywhere else: it goes with the tab.
    draft: Rc<RefCell<String>>,
}

impl CommitData {
    /// The listed files still ticked, in the diff's order.
    fn chosen(&self) -> Vec<CommitFile> {
        let unticked = self.unticked.borrow();
        self.listed
            .borrow()
            .iter()
            .filter(|f| !unticked.contains(&f.path))
            .cloned()
            .collect()
    }
}

impl std::ops::Deref for CommitBar {
    type Target = CommitData;
    fn deref(&self) -> &CommitData {
        &self.data
    }
}

/// A [`CommitBar`] for its own widgets' handlers: the widgets held weakly, so no handler keeps
/// the widget it is connected to alive.
struct WeakCommitBar {
    bar: glib::WeakRef<gtk4::Box>,
    all: glib::WeakRef<gtk4::CheckButton>,
    status: glib::WeakRef<gtk4::Label>,
    button: glib::WeakRef<gtk4::Button>,
    face: glib::WeakRef<gtk4::Stack>,
    spinner: glib::WeakRef<gtk4::Spinner>,
    data: CommitData,
}

impl WeakCommitBar {
    fn upgrade(&self) -> Option<CommitBar> {
        Some(CommitBar {
            bar: self.bar.upgrade()?,
            all: self.all.upgrade()?,
            status: self.status.upgrade()?,
            button: self.button.upgrade()?,
            face: self.face.upgrade()?,
            spinner: self.spinner.upgrade()?,
            data: self.data.clone(),
        })
    }
}

impl CommitBar {
    fn new() -> Self {
        let all = gtk4::CheckButton::builder()
            .valign(gtk4::Align::Center)
            .tooltip_text("Tick or untick every file")
            .build();
        all.update_property(&[gtk4::accessible::Property::Label("Commit every file")]);
        let status = gtk4::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .wrap(true)
            .wrap_mode(gtk4::pango::WrapMode::WordChar)
            .css_classes(["dim-label", "caption"])
            .build();
        let content = adw::ButtonContent::builder()
            .icon_name("at-object-select-symbolic")
            .label("Commit…")
            .build();
        let busy = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .spacing(6)
            .halign(gtk4::Align::Center)
            .build();
        let spinner = gtk4::Spinner::new();
        busy.append(&spinner);
        busy.append(&gtk4::Label::new(Some("Committing…")));
        let face = gtk4::Stack::builder()
            .transition_type(gtk4::StackTransitionType::Crossfade)
            .build();
        face.add_named(&content, Some("idle"));
        face.add_named(&busy, Some("busy"));
        let button = gtk4::Button::builder()
            .child(&face)
            .valign(gtk4::Align::Center)
            .tooltip_text("Commit the ticked files, after showing the command")
            .css_classes(["pill", "diff-commit"])
            .sensitive(false)
            .build();
        let bar = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .spacing(8)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(8)
            .margin_end(8)
            .visible(false)
            .build();
        bar.append(&all);
        bar.append(&status);
        bar.append(&button);
        Self {
            bar,
            all,
            status,
            button,
            face,
            spinner,
            data: CommitData {
                committing: Rc::new(Cell::new(false)),
                preparing: Rc::new(Cell::new(false)),
                listed: Rc::new(RefCell::new(Vec::new())),
                unticked: Rc::new(RefCell::new(BTreeSet::new())),
                ticks: Rc::new(RefCell::new(Vec::new())),
                repo: Rc::new(RefCell::new(None)),
                op: Rc::new(Cell::new(None)),
                uncommitted: Rc::new(Cell::new(false)),
                syncing: Rc::new(Cell::new(false)),
                handler: Rc::new(RefCell::new(None)),
                draft: Rc::new(RefCell::new(String::new())),
            },
        }
    }

    /// The bar for the bar's own signal handlers: its widgets held weakly, so no handler keeps
    /// the widget it is connected to alive.
    fn downgrade(&self) -> WeakCommitBar {
        WeakCommitBar {
            bar: self.bar.downgrade(),
            all: self.all.downgrade(),
            status: self.status.downgrade(),
            button: self.button.downgrade(),
            face: self.face.downgrade(),
            spinner: self.spinner.downgrade(),
            data: self.data.clone(),
        }
    }

    /// Redraws the bar for the diff on show, the ticks and what is running.
    fn sync(&self, pushing: bool) {
        let listed = self.listed.borrow().len();
        let chosen = self.chosen().len();
        let off = commit_off(
            self.uncommitted.get() && self.repo.borrow().is_some(),
            listed,
            chosen,
            self.op.get(),
            pushing,
        );
        let committing = self.committing.get();
        // A commit running keeps its bar, whatever a refresh meanwhile found ("git is busy").
        self.bar
            .set_visible(committing || !off.is_some_and(CommitOff::hides_bar));
        let words = match off {
            _ if committing => "Committing the chosen files…".to_owned(),
            Some(off) => off.reason().to_owned(),
            None => chosen_words(chosen, listed),
        };
        self.status.set_text(&words);
        self.button
            .set_sensitive(off.is_none() && !committing && !self.preparing.get());
        self.syncing.set(true);
        self.all.set_visible(
            !committing && !off.is_some_and(|o| matches!(o, CommitOff::InProgress(_))),
        );
        self.all.set_active(listed > 0 && chosen == listed);
        self.all.set_inconsistent(chosen > 0 && chosen < listed);
        self.syncing.set(false);
    }
}

/// The panel's widgets. Cheap to clone: every field is a reference.
#[derive(Clone)]
pub struct DiffPanel {
    /// The panel's place beside the thread. It stays there, and stays the panel's identity,
    /// while the panel is popped out (then it is hidden and empty).
    pub root: gtk4::Box,
    /// Everything the panel shows: in `root`, or in its own window when popped out.
    pub(in crate::window) content: gtk4::Box,
    /// The window the panel is popped out into, if it is.
    detached: Rc<RefCell<Option<adw::Window>>>,
    /// The parent window's destroy handler while popped out, to drop on docking.
    parent_watch: Rc<RefCell<Option<ParentWatch>>>,
    pop: gtk4::Button,
    pop_handler: Rc<RefCell<Option<PopHandler>>>,
    /// The "Open a PR" bar under the diff.
    pr_bar: gtk4::Box,
    pr_status: gtk4::Label,
    pr_button: gtk4::Button,
    pr_content: adw::ButtonContent,
    pr_face: gtk4::Stack,
    pr_spinner: gtk4::Spinner,
    /// A push for the pull request is running (`set_pushing`).
    pr_pushing: Rc<Cell<bool>>,
    pr_state: Rc<RefCell<Option<PrState>>>,
    pr_handler: Rc<RefCell<Option<PrHandler>>>,
    /// The "Commit…" bar under the diff, above the PR bar.
    commit: CommitBar,
    /// The comments on diff lines and the review bar under the diff, above the commit bar.
    review: review::Review,
    base: gtk4::DropDown,
    /// The base the refresh in flight was started with.
    begun_base: Rc<Cell<DiffBase>>,
    summary: gtk4::Label,
    refresh: gtk4::Button,
    undo: gtk4::Button,
    /// What the Undo button restores to, for the diff on show.
    undo_to: Rc<RefCell<Option<String>>>,
    stack: gtk4::Stack,
    status: adw::StatusPage,
    retry: gtk4::Button,
    /// The folder headers and file rows of the diff on show.
    files: gtk4::Box,
    /// Under the files: why some diffs are not shown (too large, cut short).
    note: gtk4::Label,
    /// Each listed file's diff text, by `stats` index (empty: not shown).
    chunks: Rc<RefCell<Vec<String>>>,
    /// How many file rows are listed.
    file_rows: Rc<Cell<usize>>,
    /// Every file view built so far, recoloured with the theme.
    views: Rc<RefCell<Vec<gtk4::TextView>>>,
    /// The diff colours, for views built after a theme change.
    colours: Rc<RefCell<DiffColours>>,
    /// Bumped by every refresh, so a slow one that finishes after a newer one
    /// cannot overwrite it.
    generation: Rc<super::Generation>,
    /// Called with the file to open when a row's "Open in …" is clicked.
    open_handler: Rc<RefCell<Option<OpenHandler>>>,
    /// Called with the file to open when a row's "Open in ‹editor›" is clicked.
    edit_handler: Rc<RefCell<Option<EditHandler>>>,
    /// The diff on show, for the rows' open buttons.
    shown_sides: Rc<RefCell<Option<Sides>>>,
    /// Whether the saved width has been applied since the panel was last
    /// shown. Until it has, the divider is where GTK put it, not where the
    /// user did, and must not be saved as their choice.
    placed: Rc<Cell<bool>>,
    /// Set while code moves the divider, so that move is not taken for a drag.
    placing: Rc<Cell<bool>>,
    /// The file row the user last expanded or moved focus to: what a saved prompt's `{file}`
    /// falls back to. Kept across a refresh while the file is still listed.
    focused: Rc<RefCell<Option<String>>>,
}

impl DiffPanel {
    pub fn new(colours: &DiffColours) -> Self {
        let labels: Vec<&str> = DiffBase::ALL.iter().map(|b| b.label()).collect();
        let base = gtk4::DropDown::from_strings(&labels);
        base.set_tooltip_text(Some("Compare against"));

        let summary = gtk4::Label::builder()
            .hexpand(true)
            .xalign(0.0)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .css_classes(["dim-label"])
            .build();
        let refresh = gtk4::Button::builder()
            .icon_name("at-view-refresh-symbolic")
            .tooltip_text("Refresh")
            .css_classes(["flat"])
            .build();
        refresh.update_property(&[gtk4::accessible::Property::Label("Refresh changes")]);
        let header = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .spacing(6)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .build();
        // Offered only for a checkpoint base with something to undo; see
        // `show_diff`.
        let undo = gtk4::Button::builder()
            .icon_name("at-edit-undo-symbolic")
            .tooltip_text("Undo these changes…")
            .css_classes(["flat"])
            .visible(false)
            .build();
        undo.update_property(&[gtk4::accessible::Property::Label("Undo these changes")]);
        let pop = gtk4::Button::builder()
            .icon_name(crate::icons::POP_OUT_ICON)
            .css_classes(["flat"])
            .build();
        header.append(&base);
        header.append(&summary);
        header.append(&undo);
        header.append(&refresh);
        header.append(&pop);

        // Under the diff: where the branch would go, and the button that opens its pull request
        // on GitHub (pushing first, when it has to).
        let pr_status = gtk4::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .wrap(true)
            .wrap_mode(gtk4::pango::WrapMode::WordChar)
            .css_classes(["dim-label", "caption"])
            .build();
        let pr_content = adw::ButtonContent::builder()
            .icon_name(crate::icons::PULL_REQUEST_ICON)
            .label("Open a PR")
            .build();
        // The button's face: its label, or a spinner while its push runs (`set_pushing`).
        let pr_busy = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .spacing(6)
            .halign(gtk4::Align::Center)
            .build();
        let pr_spinner = gtk4::Spinner::new();
        pr_busy.append(&pr_spinner);
        pr_busy.append(&gtk4::Label::new(Some("Pushing…")));
        let pr_face = gtk4::Stack::builder()
            .transition_type(gtk4::StackTransitionType::Crossfade)
            .build();
        pr_face.add_named(&pr_content, Some("idle"));
        pr_face.add_named(&pr_busy, Some("busy"));
        let pr_button = gtk4::Button::builder()
            .child(&pr_face)
            .valign(gtk4::Align::Center)
            .css_classes(["pill", "suggested-action", "diff-open-pr"])
            .sensitive(false)
            .build();
        let pr_bar = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .spacing(8)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(8)
            .margin_end(8)
            .visible(false)
            .build();
        pr_bar.append(&pr_status);
        pr_bar.append(&pr_button);

        let retry = gtk4::Button::builder()
            .label("Retry")
            .halign(gtk4::Align::Center)
            .css_classes(["pill"])
            .visible(false)
            .build();
        let status = adw::StatusPage::builder()
            .child(&retry)
            .vexpand(true)
            .build();
        status.add_css_class("compact");

        let files = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .spacing(2)
            .margin_start(6)
            .margin_end(6)
            .margin_top(4)
            .margin_bottom(6)
            .build();
        let note = gtk4::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .visible(false)
            .margin_top(6)
            .css_classes(["dim-label"])
            .build();
        let column = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        column.append(&files);
        column.append(&note);
        let diff_page = gtk4::ScrolledWindow::builder()
            .vexpand(true)
            .child(&column)
            .build();

        let stack = gtk4::Stack::new();
        stack.set_transition_type(gtk4::StackTransitionType::Crossfade);
        stack.set_transition_duration(crate::brand::CROSSFADE_MS);
        stack.add_named(&status, Some("status"));
        stack.add_named(&diff_page, Some("diff"));

        let content = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .vexpand(true)
            .hexpand(true)
            .build();
        content.append(&header);
        content.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
        content.append(&stack);
        let (review_bar, review_status, review_send, review_discard) = review::bar();
        let review_separator = gtk4::Separator::new(gtk4::Orientation::Horizontal);
        review_bar
            .bind_property("visible", &review_separator, "visible")
            .sync_create()
            .build();
        content.append(&review_separator);
        content.append(&review_bar);
        let shared_colours = Rc::new(RefCell::new(colours.clone()));
        let review = review::Review::new(
            &review_bar,
            &review_status,
            &review_send,
            &review_discard,
            Rc::clone(&shared_colours),
        );
        let commit = CommitBar::new();
        let commit_separator = gtk4::Separator::new(gtk4::Orientation::Horizontal);
        commit
            .bar
            .bind_property("visible", &commit_separator, "visible")
            .sync_create()
            .build();
        content.append(&commit_separator);
        content.append(&commit.bar);
        let pr_separator = gtk4::Separator::new(gtk4::Orientation::Horizontal);
        pr_bar
            .bind_property("visible", &pr_separator, "visible")
            .sync_create()
            .build();
        content.append(&pr_separator);
        content.append(&pr_bar);
        // No width request: the paned lets the panel narrow in a small window. MIN_WIDTH is only
        // the floor for a width the user drags it to.
        let root = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .build();
        root.append(&content);

        let panel = Self {
            root,
            content,
            detached: Rc::new(RefCell::new(None)),
            parent_watch: Rc::new(RefCell::new(None)),
            pop,
            pop_handler: Rc::new(RefCell::new(None)),
            pr_bar,
            pr_status,
            pr_button,
            pr_content,
            pr_face,
            pr_spinner,
            pr_pushing: Rc::new(Cell::new(false)),
            pr_state: Rc::new(RefCell::new(None)),
            pr_handler: Rc::new(RefCell::new(None)),
            commit,
            review,
            base,
            begun_base: Rc::new(Cell::new(DiffBase::default())),
            summary,
            refresh,
            undo,
            undo_to: Rc::new(RefCell::new(None)),
            stack,
            status,
            retry,
            files,
            note,
            chunks: Rc::new(RefCell::new(Vec::new())),
            file_rows: Rc::new(Cell::new(0)),
            views: Rc::new(RefCell::new(Vec::new())),
            colours: shared_colours,
            generation: Rc::new(super::Generation::new()),
            open_handler: Rc::new(RefCell::new(None)),
            edit_handler: Rc::new(RefCell::new(None)),
            shown_sides: Rc::new(RefCell::new(None)),
            placed: Rc::new(Cell::new(false)),
            placing: Rc::new(Cell::new(false)),
            focused: Rc::new(RefCell::new(None)),
        };
        panel.set_colours(colours);
        panel.show_pop_state();

        let review = panel.review.clone();
        review_send.connect_clicked(move |_| review.send_now());
        let review = panel.review.clone();
        let parent = panel.content.downgrade();
        review_discard.connect_clicked(move |_| {
            let n = review.drafts().len();
            if n < 2 {
                review.clear();
                return;
            }
            let Some(parent) = parent.upgrade() else {
                return;
            };
            let dialog = adw::AlertDialog::new(
                Some(&format!("Discard {}?", review::count_words(n))),
                Some("The comments are deleted and not sent."),
            );
            dialog.add_responses(&[("cancel", "Cancel"), ("discard", "Discard")]);
            dialog.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel");
            let review = review.clone();
            dialog.connect_response(None, move |_, response| {
                if response == "discard" {
                    review.clear();
                }
            });
            dialog.present(Some(&parent));
        });

        let handler = Rc::clone(&panel.pr_handler);
        let state = Rc::clone(&panel.pr_state);
        panel.pr_button.connect_clicked(move |_| {
            let (Some(handler), Some(state)) = (handler.borrow().clone(), state.borrow().clone())
            else {
                return;
            };
            handler(state);
        });
        let weak = panel.commit.downgrade();
        let pushing = Rc::clone(&panel.pr_pushing);
        panel.commit.button.connect_clicked(move |_| {
            let Some(bar) = weak.upgrade() else {
                return;
            };
            let (Some(handler), Some(repo)) =
                (bar.handler.borrow().clone(), bar.repo.borrow().clone())
            else {
                return;
            };
            let files = bar.chosen();
            if files.is_empty() || bar.committing.get() || bar.preparing.get() {
                return;
            }
            // Off until the window has its dialog up (or has said why not): `set_preparing`.
            bar.preparing.set(true);
            bar.sync(pushing.get());
            let listed = bar.listed.borrow().len();
            handler(CommitAsk {
                repo,
                files,
                listed,
            });
        });
        let weak = panel.commit.downgrade();
        let pushing = Rc::clone(&panel.pr_pushing);
        panel.commit.all.connect_toggled(move |all| {
            let Some(bar) = weak.upgrade() else {
                return;
            };
            if bar.syncing.get() {
                return;
            }
            // A partly ticked box shows as unticked, so a click on it ticks everything; a click
            // on a fully ticked one unticks everything.
            let tick = all.is_active();
            {
                let mut unticked = bar.unticked.borrow_mut();
                for file in bar.listed.borrow().iter() {
                    if tick {
                        unticked.remove(&file.path);
                    } else {
                        unticked.insert(file.path.clone());
                    }
                }
            }
            bar.syncing.set(true);
            for (check, _) in bar.ticks.borrow().iter() {
                if let Some(check) = check.upgrade() {
                    check.set_active(tick);
                }
            }
            bar.syncing.set(false);
            bar.sync(pushing.get());
        });
        let handler = Rc::clone(&panel.pop_handler);
        panel.pop.connect_clicked(move |_| {
            let handler = handler.borrow().clone();
            if let Some(handler) = handler {
                handler();
            }
        });
        // A breakpoint that un-applies restores the place's visibility, which while popped out
        // would show an empty place beside the thread: keep it hidden.
        let detached = Rc::clone(&panel.detached);
        panel.root.connect_visible_notify(move |root| {
            if root.is_visible() && detached.borrow().is_some() {
                let root = root.downgrade();
                glib::idle_add_local_once(move || {
                    if let Some(root) = root.upgrade() {
                        root.set_visible(false);
                    }
                });
            }
        });
        panel
    }

    /// The pop-out button's icon and words for where the panel is.
    fn show_pop_state(&self) {
        let (icon, words) = if self.is_detached() {
            ("at-sidebar-show-symbolic", "Put back beside the thread")
        } else {
            (crate::icons::POP_OUT_ICON, "Open in its own window")
        };
        self.pop.set_icon_name(icon);
        self.pop.set_tooltip_text(Some(words));
        self.pop
            .update_property(&[gtk4::accessible::Property::Label(words)]);
    }

    /// The file in focus in the panel (the row last expanded or focused), while the panel is on
    /// show and lists it; its path as the panel shows it, from the repository's top.
    pub fn focused_file(&self) -> Option<String> {
        if !self.is_shown() {
            return None;
        }
        self.focused.borrow().clone()
    }

    /// Whether the panel is in its own window.
    pub fn is_detached(&self) -> bool {
        self.detached.borrow().is_some()
    }

    /// Whether the panel is on show: beside the thread, or in its own window.
    pub fn is_shown(&self) -> bool {
        self.root.is_visible() || self.is_detached()
    }

    /// Calls `pop` when the pop-out button is clicked (out, or back in when popped out).
    pub fn connect_pop(&self, pop: impl Fn() + 'static) {
        *self.pop_handler.borrow_mut() = Some(Rc::new(pop));
    }

    /// Moves the panel into a window of its own, titled after `title`, or brings that window to
    /// the front if it is already out. Closing the window (or [`Self::dock`]) puts the panel
    /// back; `parent` closing closes it too.
    pub fn pop_out(&self, parent: &gtk4::Window, title: &str) {
        if let Some(window) = self.detached.borrow().as_ref() {
            window.present();
            return;
        }
        self.root.remove(&self.content);
        let header = adw::HeaderBar::new();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&self.content));
        let window = adw::Window::builder()
            .title(format!("Changes — {title}"))
            .default_width(parent.width().clamp(MIN_WIDTH * 2, 640))
            .default_height((parent.height() * 4 / 5).clamp(480, 900))
            .content(&toolbar)
            .build();
        window.add_css_class("diff-window");
        // A dialog over the popped-out panel closes on a press outside it, as Escape does.
        crate::dialogs::close_dialogs_on_outside_click(&window);
        *self.detached.borrow_mut() = Some(window.clone());
        self.root.set_visible(false);

        let panel = self.clone();
        window.connect_close_request(move |_| {
            // The window is closing itself: take the panel back and let it go.
            panel.take_back();
            glib::Propagation::Proceed
        });
        let panel = self.clone();
        // The thread's window closing takes the panel's window with it.
        let watch = parent.connect_destroy(move |_| panel.dock());
        *self.parent_watch.borrow_mut() = Some((parent.downgrade(), watch));
        self.show_pop_state();
        window.present();
    }

    /// Puts a popped-out panel back beside the thread, shown, and closes its window.
    pub fn dock(&self) {
        if let Some(window) = self.take_back() {
            window.destroy();
        }
    }

    /// Moves the content back beside the thread and returns the (now empty) window it was in.
    fn take_back(&self) -> Option<adw::Window> {
        let window = self.detached.borrow_mut().take()?;
        if let Some((parent, watch)) = self.parent_watch.borrow_mut().take() {
            if let Some(parent) = parent.upgrade() {
                parent.disconnect(watch);
            }
        }
        if let Some(toolbar) = window.content().and_downcast::<adw::ToolbarView>() {
            toolbar.set_content(None::<&gtk4::Widget>);
        }
        self.root.append(&self.content);
        self.show_pop_state();
        self.set_shown(true);
        Some(window)
    }

    /// Brings a popped-out panel's window to the front.
    pub fn present(&self) {
        if let Some(window) = self.detached.borrow().as_ref() {
            window.present();
        }
    }

    /// The widget dialogs about this panel are shown over: in whichever window it is in.
    pub fn dialog_parent(&self) -> gtk4::Widget {
        self.content.clone().upcast()
    }

    /// Calls `open` with the branch's state when "Open a PR" is clicked.
    pub fn connect_open_pr(&self, open: impl Fn(PrState) + 'static) {
        *self.pr_handler.borrow_mut() = Some(Rc::new(open));
    }

    /// Shows the branch's pull-request state from a refresh, unless a newer refresh has started.
    /// `None`: not a repository, and no bar at all.
    pub fn show_pr(&self, generation: u64, outcome: Option<Result<PrOutcome, String>>) {
        if !self.generation.is_current(generation) {
            return;
        }
        let Some(outcome) = outcome else {
            self.pr_bar.set_visible(false);
            *self.pr_state.borrow_mut() = None;
            return;
        };
        let words = pr_words(&outcome);
        self.pr_status.set_text(&words.status);
        self.pr_status.set_tooltip_text(Some(&words.status));
        self.pr_content.set_label(words.button);
        // A refresh landing mid-push (or mid-commit) keeps the button busy: only the end of
        // what runs frees it.
        self.pr_button.set_sensitive(
            words.enabled && !self.pr_pushing.get() && !self.commit.committing.get(),
        );
        self.pr_button.set_tooltip_text(Some(if words.enabled {
            "Open this branch's pull request page on GitHub in the browser"
        } else {
            &words.status
        }));
        *self.pr_state.borrow_mut() = match outcome {
            Ok(PrOutcome::Ready(state)) if words.enabled => Some(state),
            _ => None,
        };
        self.pr_bar.set_visible(true);
    }

    /// While the push behind "Push & Open PR" runs, the button spins and says so, and cannot be
    /// pressed again, whatever refreshes land meanwhile. Its end gives the button back as the
    /// last refresh left it (pressable only with a branch ready to open).
    pub fn set_pushing(&self, pushing: bool) {
        self.pr_pushing.set(pushing);
        self.pr_face
            .set_visible_child_name(if pushing { "busy" } else { "idle" });
        if pushing {
            self.pr_spinner.start();
            self.pr_button.set_sensitive(false);
        } else {
            self.pr_spinner.stop();
            self.pr_button
                .set_sensitive(self.pr_state.borrow().is_some() && !self.commit.committing.get());
        }
        self.commit.sync(pushing);
    }

    /// Binds the panel's comments to its tab's thread: until then none can be made. True the
    /// first time, when the caller loads the thread's saved drafts ([`Self::restore_review`]).
    pub fn bind_review(&self, thread: &str) -> bool {
        self.review.bind(thread)
    }

    /// Takes `thread`'s saved drafts, read from the store.
    pub fn restore_review(&self, thread: &str, drafts: Vec<agent_kit::store::ReviewDraft>) {
        self.review.restore(thread, drafts);
    }

    /// The saved drafts could not be read: nothing is written over them.
    pub fn review_load_failed(&self, thread: &str) {
        self.review.load_failed(thread);
    }

    /// Calls `persist` with the thread and its whole draft list (redacted) after every change.
    pub fn connect_review_persist(
        &self,
        persist: impl Fn(String, Vec<agent_kit::store::ReviewDraft>) + 'static,
    ) {
        self.review.set_persist(Rc::new(persist));
    }

    /// Calls `send` with the composed review when "Send Review" is clicked; `Err` is shown.
    pub fn connect_send_review(&self, send: impl Fn(String) -> Result<(), String> + 'static) {
        self.review.set_send(Rc::new(send));
    }

    /// Calls `commit` with the chosen files when "Commit…" is clicked.
    pub fn connect_commit(&self, commit: impl Fn(CommitAsk) + 'static) {
        *self.commit.handler.borrow_mut() = Some(Rc::new(commit));
    }

    /// Shows, from a refresh, the repository its diff came from (`None`: none) and the operation
    /// git has half done there, unless a newer refresh has started.
    pub fn show_commit(&self, generation: u64, repo: Option<PathBuf>, op: Option<InProgress>) {
        if !self.generation.is_current(generation) {
            return;
        }
        *self.commit.repo.borrow_mut() = repo;
        self.commit.op.set(op);
        self.commit.sync(self.pr_pushing.get());
    }

    /// While a commit runs, its button spins and says so, and neither it nor the PR button can
    /// be pressed, whatever refreshes land meanwhile.
    pub fn set_committing(&self, committing: bool) {
        self.commit.committing.set(committing);
        self.commit
            .face
            .set_visible_child_name(if committing { "busy" } else { "idle" });
        if committing {
            self.commit.spinner.start();
            self.pr_button.set_sensitive(false);
        } else {
            self.commit.spinner.stop();
            self.pr_button
                .set_sensitive(self.pr_state.borrow().is_some() && !self.pr_pushing.get());
        }
        self.commit.sync(self.pr_pushing.get());
    }

    /// Ends (or starts) the wait between a "Commit…" press and its dialog being up: the button
    /// is off meanwhile, so one press opens one dialog.
    pub fn set_preparing(&self, preparing: bool) {
        self.commit.preparing.set(preparing);
        self.commit.sync(self.pr_pushing.get());
    }

    /// The commit message being written in this tab, kept while its dialog is closed.
    pub fn commit_draft(&self) -> String {
        self.commit.draft.borrow().clone()
    }

    /// Keeps (or, with an empty text, clears) the commit message being written.
    pub fn set_commit_draft(&self, text: &str) {
        *self.commit.draft.borrow_mut() = text.to_owned();
    }

    /// The commit bar's words, whether it can be pressed and whether a commit is running, when
    /// it is shown (tests).
    #[cfg(test)]
    pub fn commit_view(&self) -> Option<(String, bool, bool)> {
        self.commit.bar.is_visible().then(|| {
            (
                self.commit.status.text().to_string(),
                self.commit.button.is_sensitive(),
                self.commit.face.visible_child_name().as_deref() == Some("busy"),
            )
        })
    }

    /// Whether the PR button shows a push in flight (tests).
    #[cfg(test)]
    pub fn is_pushing(&self) -> bool {
        self.pr_face.visible_child_name().as_deref() == Some("busy")
    }

    /// The PR bar's status, button label and whether it can be pressed (tests).
    #[cfg(test)]
    pub fn pr_view(&self) -> Option<(String, String, bool)> {
        self.pr_bar.is_visible().then(|| {
            (
                self.pr_status.text().to_string(),
                self.pr_content.label().to_string(),
                self.pr_button.is_sensitive(),
            )
        })
    }

    /// Shows or hides the panel. A shown panel gets its saved width again at
    /// the next [`Self::place`], once the paned has measured the new layout.
    pub fn set_shown(&self, shown: bool) {
        self.placed.set(false);
        self.root.set_visible(shown);
    }

    /// Gives the panel `width` pixels of `paned`, if that has not been done
    /// since it was shown and `paned` has a size to take them from. Called
    /// from the paned's layout notifications until it succeeds.
    pub fn place(&self, paned: &gtk4::Paned, width: i32) {
        // `placing`: set_position re-enters through the position handler.
        if self.placed.get() || self.placing.get() || !self.root.is_visible() || paned.width() <= 0
        {
            return;
        }
        self.placing.set(true);
        paned.set_position((paned.width() - width).max(0));
        self.placing.set(false);
        self.placed.set(true);
    }

    /// The width the user has dragged the panel to, when a position change
    /// is that: not a placement, not a hidden or unplaced panel.
    pub fn dragged_width(&self, paned: &gtk4::Paned) -> Option<i32> {
        if !self.placed.get() || self.placing.get() || !self.root.is_visible() {
            return None;
        }
        let width = paned.width() - paned.position();
        (paned.width() > 0 && width >= MIN_WIDTH).then_some(width)
    }

    /// The base currently chosen in the dropdown.
    pub fn base(&self) -> DiffBase {
        DiffBase::ALL
            .get(self.base.selected() as usize)
            .copied()
            .unwrap_or_default()
    }

    /// Calls `refresh` when the user asks for one: the refresh button, the
    /// retry button, or a change of base.
    pub fn connect_refresh(&self, refresh: impl Fn() + Clone + 'static) {
        let on_click = refresh.clone();
        self.refresh.connect_clicked(move |_| on_click());
        let on_retry = refresh.clone();
        self.retry.connect_clicked(move |_| on_retry());
        self.base.connect_selected_notify(move |_| refresh());
    }

    /// Recolours the diff text, as when the terminal theme changes: every file
    /// view built so far, and those built from now on.
    pub fn set_colours(&self, colours: &DiffColours) {
        apply_view_colours(colours);
        *self.colours.borrow_mut() = colours.clone();
        for view in self.views.borrow().iter() {
            colour_tags(view, colours);
            self.review.recolour(view);
        }
    }

    /// What the panel shows: the stack page, the status title, the summary,
    /// the diff text (every file's, expanded or not, then the note) and how
    /// many files are listed.
    #[cfg(test)]
    pub fn visible_state(&self) -> (String, String, String, String, usize) {
        let mut text = self.chunks.borrow().concat();
        if self.note.is_visible() {
            text.push('\n');
            text.push_str(&self.note.text());
        }
        (
            self.stack
                .visible_child_name()
                .map(|s| s.to_string())
                .unwrap_or_default(),
            self.status.title().to_string(),
            self.summary.text().to_string(),
            text,
            self.file_rows.get(),
        )
    }

    /// The folder headers on show, in order (tests).
    #[cfg(test)]
    pub fn folder_headers(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut child = self.files.first_child();
        while let Some(widget) = child {
            if let Some(label) = widget.downcast_ref::<gtk4::Label>() {
                out.push(label.text().to_string());
            }
            child = widget.next_sibling();
        }
        out
    }

    /// The file rows' expanders, in order (tests).
    #[cfg(test)]
    pub fn file_expanders(&self) -> Vec<gtk4::Expander> {
        let mut out = Vec::new();
        let mut child = self.files.first_child();
        while let Some(widget) = child {
            if let Ok(expander) = widget.clone().downcast::<gtk4::Expander>() {
                out.push(expander);
            }
            child = widget.next_sibling();
        }
        out
    }

    /// Marks a refresh as started and returns its generation, which
    /// [`Self::show`] must be given back.
    pub fn begin(&self) -> u64 {
        let generation = self.generation.next();
        self.begun_base.set(self.base());
        self.summary.set_text("Loading…");
        generation
    }

    /// Shows a finished refresh, unless a newer one has started since.
    pub fn show(&self, generation: u64, result: Result<DiffOutcome, String>) {
        if !self.generation.is_current(generation) {
            return;
        }
        // Only the working tree's own diff can be committed from; the rows say so with a tick.
        self.commit
            .uncommitted
            .set(self.begun_base.get() == DiffBase::Uncommitted);
        self.commit.listed.borrow_mut().clear();
        self.commit.ticks.borrow_mut().clear();
        match result {
            Err(err) => self.show_status("at-dialog-warning-symbolic", "Git failed", &err, true),
            Ok(DiffOutcome::NotRepo) => self.show_status(
                "at-folder-symbolic",
                "Not a git repository",
                "This tab's folder is not inside a repository.",
                false,
            ),
            Ok(DiffOutcome::Unavailable(reason)) => self.show_status(
                "at-document-open-recent-symbolic",
                "Nothing to show yet",
                &reason,
                false,
            ),
            Ok(DiffOutcome::Ready(diff)) if diff.stats.is_empty() => {
                self.show_status("at-object-select-symbolic", "No changes", "", false)
            }
            Ok(DiffOutcome::Ready(diff)) => self.show_diff(&diff),
        }
        self.commit.sync(self.pr_pushing.get());
    }

    fn show_status(&self, icon: &str, title: &str, description: &str, retry: bool) {
        self.focused.borrow_mut().take();
        // No diff on show: every comment is kept, flagged as not in it.
        self.review.show(self.begun_base.get(), HashMap::new());
        self.set_undo(None);
        self.summary.set_text("");
        crate::icons::set_status_icon(&self.status, icon);
        self.status.set_title(title);
        // The description is markup; git's messages are not.
        self.status
            .set_description(Some(&glib::markup_escape_text(description)));
        self.retry.set_visible(retry);
        self.stack.set_visible_child_name("status");
    }

    /// Calls `open` when a file row's "Open in …" is clicked.
    pub fn connect_open(&self, open: impl Fn(OpenFile) + 'static) {
        *self.open_handler.borrow_mut() = Some(Rc::new(open));
    }

    /// Calls `open` when a file row's "Open in ‹editor›" is clicked.
    pub fn connect_open_in_editor(&self, open: impl Fn(EditFile) + 'static) {
        *self.edit_handler.borrow_mut() = Some(Rc::new(open));
    }

    /// A row's "Open in ‹editor›" button. Always on offer: with no editor configured it opens
    /// the system's handler, and the tooltip says that cannot jump to a line. It opens at the
    /// row's first changed line (`line`).
    fn editor_button(&self, path: String, line: Option<u32>) -> gtk4::Button {
        let button = gtk4::Button::builder()
            .icon_name("at-document-edit-symbolic")
            .css_classes(["flat", "diff-open"])
            .valign(gtk4::Align::Center)
            .build();
        button.update_property(&[gtk4::accessible::Property::Label(
            "Open the file in the editor",
        )]);
        let refresh = |button: &gtk4::Button| match EditorTools::shared().get() {
            Some(tool) => button.set_tooltip_text(Some(&format!(
                "{}. Ctrl+click a line of the diff to open the file at it",
                crate::editor_tool::open_label(&tool)
            ))),
            None => button.set_tooltip_text(Some(crate::editor_tool::NO_EDITOR_HINT)),
        };
        refresh(&button);
        let conn = EditorTools::shared().connect_changed(glib::clone!(
            #[weak]
            button,
            move || refresh(&button)
        ));
        button.connect_destroy(move |_| EditorTools::shared().disconnect(conn));

        let handler = Rc::clone(&self.edit_handler);
        button.connect_clicked(move |_| {
            if let Some(handler) = handler.borrow().clone() {
                handler(EditFile {
                    path: path.clone(),
                    line,
                });
            }
        });
        button
    }

    /// A row's "Open in ‹tool›" button: hidden while no tool is configured, and following the
    /// setting when it changes. Renamed files open as the new path (the old side reads empty).
    fn open_button(&self, path: String) -> gtk4::Button {
        let button = gtk4::Button::builder()
            .icon_name(crate::icons::EXTERNAL_ICON)
            .css_classes(["flat", "diff-open"])
            .valign(gtk4::Align::Center)
            .build();
        let refresh = |button: &gtk4::Button| match DiffTools::shared().get() {
            Some(tool) => {
                button.set_visible(true);
                let label = crate::diff_tool::open_label(&tool);
                button.set_tooltip_text(Some(&label));
                button.update_property(&[gtk4::accessible::Property::Label(&label)]);
            }
            None => {
                button.set_visible(false);
                button.set_tooltip_text(Some(crate::diff_tool::NO_TOOL_HINT));
                button.update_property(&[gtk4::accessible::Property::Label(
                    crate::diff_tool::NO_TOOL_HINT,
                )]);
            }
        };
        refresh(&button);
        let conn = DiffTools::shared().connect_changed(glib::clone!(
            #[weak]
            button,
            move || refresh(&button)
        ));
        button.connect_destroy(move |_| DiffTools::shared().disconnect(conn));

        let handler = Rc::clone(&self.open_handler);
        let sides = Rc::clone(&self.shown_sides);
        button.connect_clicked(move |_| {
            let (Some(handler), Some((from, to_checkpoint))) =
                (handler.borrow().clone(), sides.borrow().clone())
            else {
                return;
            };
            handler(OpenFile {
                path: path.clone(),
                from,
                to_checkpoint,
            });
        });
        button
    }

    /// A row's "include in the commit" tick box: ticked unless the user unticked this path.
    fn commit_tick(&self, path: &str) -> gtk4::CheckButton {
        let check = gtk4::CheckButton::builder()
            .valign(gtk4::Align::Center)
            .active(!self.commit.unticked.borrow().contains(path))
            .tooltip_text("Include in the commit")
            .build();
        check.add_css_class("diff-commit-tick");
        check.update_property(&[gtk4::accessible::Property::Label(&format!(
            "Include {path} in the commit"
        ))]);
        let weak = self.commit.downgrade();
        let pushing = Rc::clone(&self.pr_pushing);
        let owned = path.to_owned();
        check.connect_toggled(move |check| {
            let Some(bar) = weak.upgrade() else {
                return;
            };
            if bar.syncing.get() {
                return;
            }
            if check.is_active() {
                bar.unticked.borrow_mut().remove(&owned);
            } else {
                bar.unticked.borrow_mut().insert(owned.clone());
            }
            bar.sync(pushing.get());
        });
        self.commit
            .ticks
            .borrow_mut()
            .push((check.downgrade(), path.to_owned()));
        check
    }

    fn set_undo(&self, to: Option<String>) {
        self.undo.set_visible(to.is_some());
        *self.undo_to.borrow_mut() = to;
    }

    /// Calls `undo` with the revision to restore to when Undo is clicked.
    pub fn connect_undo(&self, undo: impl Fn(String) + 'static) {
        let undo_to = Rc::clone(&self.undo_to);
        self.undo.connect_clicked(move |_| {
            let target = undo_to.borrow().clone();
            if let Some(target) = target {
                undo(target);
            }
        });
    }

    /// Whether Undo is on offer.
    #[cfg(test)]
    pub fn undo_offered(&self) -> bool {
        self.undo.is_visible() && self.undo_to.borrow().is_some()
    }

    fn show_diff(&self, diff: &TabDiff) {
        self.set_undo(diff.undo_to.clone());
        self.summary.set_text(&crate::diff::summary(&diff.stats));
        *self.shown_sides.borrow_mut() = Some((diff.from.clone(), diff.to_checkpoint.clone()));

        while let Some(child) = self.files.first_child() {
            self.files.remove(&child);
        }
        self.views.borrow_mut().clear();
        // A file no longer listed is no longer in focus.
        {
            let mut focused = self.focused.borrow_mut();
            if focused
                .as_ref()
                .is_some_and(|f| !diff.stats.iter().any(|s| &s.path == f))
            {
                *focused = None;
            }
        }

        // Chunk `i` is `stats[i]`'s: git writes `--numstat` and the patch in the same order. A
        // diff cut short has fewer chunks than files; a too-large one has none.
        let chunks: Vec<String> = if diff.too_large {
            Vec::new()
        } else {
            crate::diff::split_by_file(&diff.text)
                .into_iter()
                .map(str::to_owned)
                .collect()
        };
        let mut by_stat = vec![String::new(); diff.stats.len()];
        for (slot, chunk) in by_stat.iter_mut().zip(chunks) {
            *slot = chunk;
        }
        let cut_short = !diff.too_large && diff.omitted_lines > 0;
        // Before the rows are built: their Comment buttons register with the review.
        self.review.show(
            self.begun_base.get(),
            diff.stats
                .iter()
                .zip(&by_stat)
                .filter(|(_, chunk)| !chunk.is_empty())
                .map(|(stat, chunk)| (stat.path.clone(), Rc::from(chunk.as_str())))
                .collect(),
        );

        let mut rows = 0;
        for (folder, files) in crate::diff::group_by_folder(&diff.stats) {
            let heading = if folder.is_empty() {
                "Repository root".to_owned()
            } else {
                format!("{folder}/")
            };
            let header = gtk4::Label::builder()
                .label(heading)
                .xalign(0.0)
                .ellipsize(gtk4::pango::EllipsizeMode::Start)
                .tooltip_text(folder.as_str())
                .margin_top(if rows == 0 { 0 } else { 8 })
                .css_classes(["heading", "dim-label"])
                .build();
            self.files.append(&header);
            for index in files {
                let stat = &diff.stats[index];
                if self.commit.uncommitted.get() && committable(stat) {
                    self.commit.listed.borrow_mut().push(CommitFile {
                        path: stat.path.clone(),
                        old_path: stat.old_path.clone(),
                    });
                }
                let text = by_stat[index].clone();
                let why_hidden = if diff.too_large {
                    Some("Not shown: the diff is too large. Open it in your diff tool.")
                } else if text.is_empty() && cut_short {
                    Some("Not shown: the diff was cut short before this file.")
                } else if text.is_empty() {
                    Some("No text diff for this file.")
                } else {
                    None
                };
                self.files.append(&self.file_row(stat, text, why_hidden));
                rows += 1;
            }
        }
        self.file_rows.set(rows);
        *self.chunks.borrow_mut() = by_stat;

        let note = if diff.too_large {
            let (_, added, deleted) = crate::diff::totals(&diff.stats);
            Some(format!(
                "This diff is too large to show ({} changed lines). The file list above is complete.",
                added + deleted
            ))
        } else if cut_short {
            Some(format!(
                "… diff truncated: {} more lines not shown",
                diff.omitted_lines
            ))
        } else {
            None
        };
        self.note.set_visible(note.is_some());
        self.note.set_text(note.as_deref().unwrap_or_default());
        self.stack.set_visible_child_name("diff");
    }

    /// One changed file: a collapsed row (name, `+3 −1`, "Open in …") that expands to its own
    /// diff, built on first expand. `why_hidden`: shown instead of a diff there is no text for.
    fn file_row(
        &self,
        stat: &crate::diff::FileStat,
        text: String,
        why_hidden: Option<&'static str>,
    ) -> gtk4::Expander {
        let label = gtk4::Label::builder()
            .label(crate::diff::file_label(stat))
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk4::pango::EllipsizeMode::Middle)
            .tooltip_text(stat.path.as_str())
            .build();
        let title = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        if self.commit.uncommitted.get() {
            if committable(stat) {
                title.append(&self.commit_tick(&stat.path));
            } else {
                title.append(&uncommittable_tick());
            }
        }
        title.append(&label);
        // Binary files open fine in a tool that knows them; the button is for every row.
        title.append(&self.open_button(stat.path.clone()));
        title.append(&self.editor_button(stat.path.clone(), crate::diff::first_hunk_line(&text)));
        title.append(&self.review.comment_button(&stat.path, why_hidden));
        let expander = gtk4::Expander::builder()
            .label_widget(&title)
            .margin_start(8)
            .build();
        expander.add_css_class("diff-file");
        // Expanding a row, or moving focus to it, makes its file the one in focus.
        let focus = gtk4::EventControllerFocus::new();
        let (focused, path) = (Rc::clone(&self.focused), stat.path.clone());
        focus.connect_enter(move |_| *focused.borrow_mut() = Some(path.clone()));
        expander.add_controller(focus);
        let (focused, path) = (Rc::clone(&self.focused), stat.path.clone());
        expander.connect_expanded_notify(move |expander| {
            if expander.is_expanded() {
                *focused.borrow_mut() = Some(path.clone());
            }
        });

        let views = Rc::clone(&self.views);
        let colours = Rc::clone(&self.colours);
        let (edit, path) = (Rc::clone(&self.edit_handler), stat.path.clone());
        let review = self.review.clone();
        expander.connect_expanded_notify(move |expander| {
            if !expander.is_expanded() || expander.child().is_some() {
                return;
            }
            let child: gtk4::Widget = match why_hidden {
                Some(why) => gtk4::Label::builder()
                    .label(why)
                    .xalign(0.0)
                    .wrap(true)
                    .margin_start(6)
                    .css_classes(["dim-label"])
                    .build()
                    .upcast(),
                None => {
                    let view = file_view(&text, &colours.borrow());
                    open_at_ctrl_click(&view, &text, &path, &edit);
                    views.borrow_mut().push(view.clone());
                    review.decorate(&path, &text, &view)
                }
            };
            expander.set_child(Some(&child));
        });
        expander
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use agent_kit::github::GithubRepo;

    fn state(push: PushState) -> PrState {
        PrState {
            branch: "feat/x".into(),
            remote: "origin".into(),
            remote_branch: "feat/x".into(),
            base: Some("master".into()),
            github: Some(GithubRepo {
                host: "github.com".into(),
                owner: "o".into(),
                name: "r".into(),
            }),
            push,
            uncommitted: false,
        }
    }

    fn words(state: PrState) -> PrWords {
        pr_words(&Ok(PrOutcome::Ready(state)))
    }

    #[test]
    fn a_ctrl_click_opens_the_rows_new_line_and_never_guesses() {
        // Two hunks, a deletion between: rows are the panel's text lines.
        let text = "diff --git a/s.rs b/s.rs\n--- a/s.rs\n+++ b/s.rs\n\
                    @@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n@@ -10,2 +10,1 @@\n x\n-y\n";
        let line = |row| ctrl_click_edit(text, "s.rs", row).and_then(|f| f.line);
        assert_eq!(line(4), Some(1), "context");
        assert_eq!(line(5), Some(2), "a removed row: the line that replaced it");
        assert_eq!(line(6), Some(2), "added");
        assert_eq!(line(7), Some(3));
        assert_eq!(line(8), Some(10), "a hunk header: its first line");
        assert_eq!(line(10), Some(10), "a trailing removal: the line before it");
        for row in [0, 1, 2, 11, 500] {
            assert_eq!(ctrl_click_edit(text, "s.rs", row), None, "row {row}");
        }
        // A deleted file has no line to open.
        assert_eq!(
            ctrl_click_edit("@@ -1,2 +0,0 @@\n-a\n-b\n", "d.rs", 1),
            None
        );
        // A diff cut short keeps the lines it shows.
        assert_eq!(
            ctrl_click_edit("@@ -5,9 +5,9 @@\n a\n+b\n", "c.rs", 2),
            Some(EditFile {
                path: "c.rs".into(),
                line: Some(6)
            })
        );
    }

    #[test]
    fn the_pr_bar_says_where_the_branch_goes_and_what_it_needs() {
        let ready = words(state(PushState::UpToDate));
        assert_eq!(ready.status, "feat/x → master");
        assert_eq!(ready.button, "Open a PR");
        assert!(ready.enabled);

        let unpushed = words(state(PushState::NoUpstream));
        assert_eq!(unpushed.status, "feat/x → master · not pushed yet");
        assert_eq!(unpushed.button, "Push & Open PR");
        assert_eq!(
            words(state(PushState::Ahead(1))).status,
            "feat/x → master · 1 commit to push"
        );
        assert_eq!(
            words(state(PushState::Ahead(3))).status,
            "feat/x → master · 3 commits to push"
        );

        let mut dirty = state(PushState::UpToDate);
        dirty.uncommitted = true;
        dirty.base = None;
        assert_eq!(
            words(dirty).status,
            "feat/x → the default branch · uncommitted changes are not included"
        );
    }

    #[test]
    fn the_pr_bar_is_off_with_a_reason_and_an_unread_branch_is_its_own_state() {
        let mut on_base = state(PushState::UpToDate);
        on_base.branch = "master".into();
        on_base.remote_branch = "master".into();
        let off = words(on_base);
        assert!(!off.enabled);
        assert!(off.status.starts_with("On master"));

        let mut local = state(PushState::UpToDate);
        local.github = None;
        let off = words(local);
        assert!(!off.enabled);
        assert!(off.status.contains("not a GitHub remote"));

        let off = pr_words(&Ok(PrOutcome::Unavailable("No branch".into())));
        assert_eq!((off.status.as_str(), off.enabled), ("No branch", false));
        let unread = pr_words(&Err("git timed out".into()));
        assert!(!unread.enabled);
        assert_eq!(unread.status, "Could not read the branch: git timed out");
    }

    #[test]
    fn commit_off_names_why_and_which_reasons_hide_the_bar() {
        assert_eq!(commit_off(true, 2, 2, None, false), None);
        assert_eq!(
            commit_off(false, 2, 2, None, false),
            Some(CommitOff::OtherBase)
        );
        assert_eq!(
            commit_off(true, 0, 0, None, false),
            Some(CommitOff::NoChanges)
        );
        assert_eq!(
            commit_off(true, 2, 2, Some(InProgress::Rebase), true),
            Some(CommitOff::InProgress(InProgress::Rebase))
        );
        assert_eq!(commit_off(true, 2, 2, None, true), Some(CommitOff::Pushing));
        assert_eq!(
            commit_off(true, 2, 0, None, false),
            Some(CommitOff::NothingChosen)
        );
        let every = [
            CommitOff::OtherBase,
            CommitOff::NoChanges,
            CommitOff::InProgress(InProgress::Merge),
            CommitOff::Pushing,
            CommitOff::NothingChosen,
        ];
        for off in every {
            // Exhaustive: a new reason fails to compile until it is placed here.
            let hides = match off {
                CommitOff::OtherBase | CommitOff::NoChanges => true,
                CommitOff::InProgress(_) | CommitOff::Pushing | CommitOff::NothingChosen => false,
            };
            assert_eq!(off.hides_bar(), hides, "{off:?}");
            assert!(!off.reason().is_empty());
        }
        assert_eq!(chosen_words(1, 1), "1 file to commit");
        assert_eq!(chosen_words(3, 3), "All 3 files to commit");
        assert_eq!(chosen_words(2, 3), "2 of 3 files to commit");
    }

    fn ready(paths: &[&str]) -> Result<DiffOutcome, String> {
        let numstat: Vec<u8> = paths
            .iter()
            .flat_map(|p| format!("1\t0\t{p}\0").into_bytes())
            .collect();
        Ok(DiffOutcome::Ready(TabDiff {
            stats: crate::diff::parse_numstat(&numstat),
            text: String::new(),
            omitted_lines: 0,
            too_large: false,
            undo_to: None,
            from: "HEAD".into(),
            to_checkpoint: None,
        }))
    }

    /// The tick boxes of the rows on show, with their states.
    fn ticks(panel: &DiffPanel) -> Vec<(String, bool)> {
        panel
            .commit
            .ticks
            .borrow()
            .iter()
            .filter_map(|(check, path)| check.upgrade().map(|c| (path.clone(), c.is_active())))
            .collect()
    }

    /// GTK: run from the window's construction test. The commit bar shows only for the
    /// Uncommitted view of a repository with changes, the ticks survive a refresh, untick-all
    /// turns Commit off, a merge or a push in flight turns it off with the reason, a refresh
    /// mid-commit keeps it busy, and Commit hands over the ticked files.
    pub(crate) fn the_commit_bar_follows_the_view_and_the_ticks() {
        let panel = DiffPanel::new(&crate::theme::Theme::diff_colours(Default::default()));
        let asked = Rc::new(RefCell::new(None));
        let seen = Rc::clone(&asked);
        panel.connect_commit(move |ask| *seen.borrow_mut() = Some(ask));
        let repo = PathBuf::from("/repo");

        let generation = panel.begin();
        panel.show(generation, ready(&["a.rs", "b.rs", "c.rs"]));
        assert_eq!(panel.commit_view(), None, "no repository known yet");
        panel.show_commit(generation, Some(repo.clone()), None);
        assert_eq!(
            panel.commit_view(),
            Some(("All 3 files to commit".to_owned(), true, false))
        );
        assert_eq!(ticks(&panel).len(), 3);

        // Untick one: the count follows, and the choice survives a refresh.
        if let Some(check) = panel.commit.ticks.borrow()[1].0.upgrade() {
            check.set_active(false);
        }
        assert_eq!(
            panel.commit_view().map(|v| v.0),
            Some("2 of 3 files to commit".to_owned())
        );
        let generation = panel.begin();
        panel.show(generation, ready(&["a.rs", "b.rs", "c.rs", "d.rs"]));
        panel.show_commit(generation, Some(repo.clone()), None);
        assert_eq!(
            ticks(&panel),
            vec![
                ("a.rs".to_owned(), true),
                ("b.rs".to_owned(), false),
                ("c.rs".to_owned(), true),
                ("d.rs".to_owned(), true),
            ],
            "a new file starts ticked; the one left out stays out"
        );
        panel.commit.button.emit_clicked();
        let ask = asked
            .borrow_mut()
            .take()
            .expect("Commit hands the files over");
        assert_eq!(ask.repo, repo);
        assert_eq!(ask.listed, 4);
        let paths: Vec<&str> = ask.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["a.rs", "c.rs", "d.rs"]);
        // Until the window has its dialog up, the button is off: one press, one dialog.
        assert_eq!(panel.commit_view().map(|v| v.1), Some(false));
        panel.commit.button.emit_clicked();
        assert!(asked.borrow().is_none(), "a second press opens nothing");
        panel.set_preparing(false);
        assert_eq!(panel.commit_view().map(|v| v.1), Some(true));

        // Untick everything with the bar's own box: Commit is off, and says why.
        panel.commit.all.set_active(true);
        panel.commit.all.set_active(false);
        assert_eq!(
            panel.commit_view(),
            Some((CommitOff::NothingChosen.reason().to_owned(), false, false))
        );
        panel.commit.all.set_active(true);
        assert_eq!(panel.commit_view().map(|v| v.1), Some(true));

        // A merge half done, or a push in flight: shown, but off, with the reason.
        panel.show_commit(generation, Some(repo.clone()), Some(InProgress::Merge));
        assert_eq!(
            panel.commit_view(),
            Some((InProgress::Merge.reason().to_owned(), false, false))
        );
        panel.show_commit(generation, Some(repo.clone()), None);
        panel.set_pushing(true);
        assert_eq!(panel.commit_view().map(|v| v.1), Some(false));
        panel.set_pushing(false);

        // A commit running: busy, and a refresh landing meanwhile keeps it so.
        panel.set_committing(true);
        let generation = panel.begin();
        panel.show(generation, ready(&["a.rs"]));
        panel.show_commit(generation, Some(repo.clone()), None);
        assert_eq!(panel.commit_view().map(|v| (v.1, v.2)), Some((false, true)));
        panel.commit.button.emit_clicked();
        assert!(asked.borrow().is_none(), "no second commit while one runs");
        // A refresh that finds git busy (the commit holds its lock) keeps the bar and its state.
        let generation = panel.begin();
        panel.show(
            generation,
            Ok(DiffOutcome::Unavailable("Git is busy".into())),
        );
        assert_eq!(
            panel.commit_view(),
            Some(("Committing the chosen files…".to_owned(), false, true))
        );
        panel.set_committing(false);
        let generation = panel.begin();
        panel.show(generation, ready(&["a.rs"]));
        panel.show_commit(generation, Some(repo.clone()), None);
        assert_eq!(panel.commit_view().map(|v| (v.1, v.2)), Some((true, false)));

        // A name that is not valid UTF-8 (decoded with U+FFFD) has a tick box that is off, and
        // is never handed over.
        let generation = panel.begin();
        panel.show(generation, ready(&["caf\u{FFFD}.rs", "a.rs"]));
        panel.show_commit(generation, Some(repo.clone()), None);
        assert_eq!(ticks(&panel), vec![("a.rs".to_owned(), true)]);
        assert_eq!(
            panel.commit_view().map(|v| v.0),
            Some("1 file to commit".to_owned())
        );

        // Clean, or another view: no bar.
        let generation = panel.begin();
        panel.show(generation, ready(&[]));
        panel.show_commit(generation, Some(repo.clone()), None);
        assert_eq!(panel.commit_view(), None);
        panel.base.set_selected(2);
        let generation = panel.begin();
        panel.show(generation, ready(&["a.rs"]));
        panel.show_commit(generation, Some(repo), None);
        assert_eq!(panel.commit_view(), None);
        assert!(
            ticks(&panel).is_empty(),
            "no tick boxes outside Uncommitted"
        );

        // The message draft is kept for the tab.
        panel.set_commit_draft("Half written");
        assert_eq!(panel.commit_draft(), "Half written");
    }

    /// A one-file diff of `path` whose hunk starts at `at` (rows: four header rows, the hunk,
    /// then ` keep`, `-old one`, `-old two`, `+<added>`, ` tail`).
    fn one_file(path: &str, at: u32, added: &str) -> Result<DiffOutcome, String> {
        let text = format!(
            "diff --git a/{path} b/{path}\nindex 1..2 100644\n--- a/{path}\n+++ b/{path}\n\
             @@ -{at},4 +{at},3 @@\n keep\n-old one\n-old two\n+{added}\n tail\n"
        );
        Ok(DiffOutcome::Ready(TabDiff {
            stats: crate::diff::parse_numstat(format!("1\t2\t{path}\0").as_bytes()),
            text,
            omitted_lines: 0,
            too_large: false,
            undo_to: None,
            from: "HEAD".into(),
            to_checkpoint: None,
        }))
    }

    /// Writes `note` in an open note editor and presses its Save.
    fn save_note(popover: &gtk4::Popover, note: &str) {
        let root = popover.clone().upcast::<gtk4::Widget>();
        let entry: gtk4::TextView =
            crate::testutil::find_first(&root).expect("the editor has an entry");
        entry.buffer().set_text(note);
        let save = crate::testutil::find_all::<gtk4::Button>(&root)
            .into_iter()
            .find(|b| b.label().as_deref() == Some("Save"))
            .expect("Save");
        assert!(save.is_sensitive());
        save.emit_clicked();
    }

    /// GTK: run from the window's construction test. Comments are made from a selection in a
    /// file's view, refused on header lines, marked, kept redacted and written whole after every
    /// change, restored from the store, re-anchored when the diff moves or changes, and sent as
    /// one composed, fenced message, after which they are gone; a failed send keeps them.
    pub(crate) fn comments_are_kept_anchored_and_sent_as_one_review() {
        let ctx = glib::MainContext::default();
        let panel = DiffPanel::new(&crate::theme::Theme::diff_colours(Default::default()));
        let window = adw::Window::builder()
            .default_width(700)
            .default_height(700)
            .content(&panel.root)
            .build();
        panel.set_shown(true);
        window.present();
        assert!(crate::testutil::pump_until(&ctx, 5, || panel
            .root
            .is_mapped()));

        type Written = Vec<(String, Vec<agent_kit::store::ReviewDraft>)>;
        let saved: Rc<RefCell<Written>> = Rc::default();
        let log = Rc::clone(&saved);
        panel.connect_review_persist(move |thread, drafts| log.borrow_mut().push((thread, drafts)));
        let sent: Rc<RefCell<Vec<String>>> = Rc::default();
        let refuse = Rc::new(Cell::new(false));
        let (log, no) = (Rc::clone(&sent), Rc::clone(&refuse));
        panel.connect_send_review(move |text| {
            if no.get() {
                return Err("the thread is not open".into());
            }
            log.borrow_mut().push(text);
            Ok(())
        });

        // No thread: no Comment button and no bar.
        let generation = panel.begin();
        panel.show(generation, one_file("a.rs", 10, "new one"));
        let button = |panel: &DiffPanel| {
            crate::testutil::find_by_class::<gtk4::Button>(
                panel.files.upcast_ref(),
                "diff-comment-button",
            )
            .expect("the row has a Comment button")
        };
        assert!(!button(&panel).is_visible());
        assert_eq!(panel.review.bar_view(), None);

        // Bound, with one stored comment: it is shown at its lines, and nothing is written back.
        assert!(panel.bind_review("t1"));
        assert!(!panel.bind_review("t2"), "a panel's thread never changes");
        assert!(button(&panel).is_visible());
        let stored = agent_kit::store::ReviewDraft {
            path: "a.rs".into(),
            side: crate::diff::Side::New,
            start_line: 11,
            end_line: 11,
            quoted: vec!["+new one".into()],
            note: "Stored note".into(),
            diff_base: "uncommitted".into(),
            created_at: 1,
        };
        panel.restore_review("t2", vec![stored.clone()]);
        assert_eq!(
            panel.review.bar_view(),
            None,
            "another thread's drafts are not taken"
        );
        panel.restore_review("t1", vec![stored.clone()]);
        assert!(saved.borrow().is_empty());
        assert_eq!(
            panel.review.bar_view(),
            Some(("1 comment".to_owned(), true))
        );

        // Expanding the row builds its view, with the stored comment's line marked.
        panel.file_expanders()[0].set_expanded(true);
        assert_eq!(panel.review.marked_rows("a.rs"), vec![8]);
        let view = panel.review.view_of("a.rs").expect("view");
        assert!(
            view.is_cursor_visible(),
            "a caret, for choosing lines by keyboard"
        );
        assert!(!button(&panel).is_sensitive(), "nothing selected yet");

        // Header lines alone are refused, with the reason.
        let buffer = view.buffer();
        let line = |n: i32| buffer.iter_at_line(n).expect("line");
        buffer.select_range(&line(0), &line(3));
        assert!(button(&panel).is_sensitive());
        let refused = panel.review.comment_on("a.rs").expect("a popover");
        assert_eq!(
            crate::testutil::label_texts(refused.upcast_ref()),
            vec!["Select changed or context lines".to_owned()]
        );
        refused.popdown();

        // Rows 5 and 6 (the selection ends at the start of row 7): a comment with a secret.
        let token = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        buffer.select_range(&line(5), &line(7));
        let editor = panel.review.comment_on("a.rs").expect("the editor");
        save_note(&editor, &format!("Rotate {token} here"));
        {
            let saved = saved.borrow();
            let (thread, drafts) = saved.last().expect("written after the change");
            assert_eq!(thread, "t1");
            assert_eq!(drafts.len(), 2, "the stored comment and the new one, whole");
            let made = drafts
                .iter()
                .find(|d| d.start_line == 10)
                .expect("the new one");
            assert_eq!((made.end_line, made.side), (10, crate::diff::Side::New));
            assert_eq!(made.quoted, vec![" keep".to_owned(), "-old one".to_owned()]);
            assert!(!made.note.contains(token), "kept redacted: {}", made.note);
            assert_eq!(made.diff_base, "uncommitted");
        }
        assert_eq!(panel.review.marked_rows("a.rs"), vec![5, 6, 8]);
        assert_eq!(
            panel.review.bar_view(),
            Some(("2 comments".to_owned(), true))
        );
        assert!(crate::testutil::button_problems(panel.content.upcast_ref(), 24).is_empty());
        assert!(crate::testutil::icon_only_without_label(panel.content.upcast_ref()).is_empty());

        // Three lines inserted above: both comments move with their lines.
        let generation = panel.begin();
        panel.show(generation, one_file("a.rs", 13, "new one"));
        assert_eq!(
            panel.review.located(),
            vec![
                ("a.rs".to_owned(), crate::diff::Located::Moved, 13, 13),
                ("a.rs".to_owned(), crate::diff::Located::Moved, 14, 14),
            ]
        );
        // The added line changed: that comment is not in this diff, but kept.
        let generation = panel.begin();
        panel.show(generation, one_file("a.rs", 13, "newer one"));
        assert_eq!(
            panel.review.bar_view().map(|v| v.0),
            Some("2 comments · 1 not in this diff".to_owned())
        );
        panel.file_expanders()[0].set_expanded(true);
        assert_eq!(panel.review.marked_rows("a.rs"), vec![5, 6]);

        // A send that fails keeps every comment and says why.
        refuse.set(true);
        panel.review.send_now();
        assert_eq!(
            panel.review.bar_view().map(|v| v.0),
            Some("2 comments · 1 not in this diff · Could not send the review: the thread is not open".to_owned())
        );
        assert_eq!(panel.review.drafts().len(), 2);

        // Sent: one message, fenced and redacted; then they are gone, and the store is told.
        refuse.set(false);
        let writes = saved.borrow().len();
        let send = crate::testutil::find_by_class::<gtk4::Button>(
            panel.content.upcast_ref(),
            "diff-send-review",
        )
        .expect("Send Review");
        send.emit_clicked();
        let texts = sent.borrow().clone();
        assert_eq!(texts.len(), 1, "one message for the whole review");
        let text = &texts[0];
        assert!(
            text.starts_with("Review comments on your changes (2)."),
            "{text}"
        );
        assert!(text.contains("1. a.rs, line 13 (new side)\n```\n keep\n-old one\n```"));
        assert!(text.contains("2. a.rs, no longer at these lines (was line 11, new side)"));
        assert!(!text.contains(token));
        assert!(panel.review.drafts().is_empty());
        assert_eq!(saved.borrow().len(), writes + 1);
        assert_eq!(saved.borrow().last().map(|(_, d)| d.len()), Some(0));
        assert_eq!(panel.review.bar_view(), None);

        // Discard: one comment goes without asking.
        let view = panel.review.view_of("a.rs").expect("view");
        let buffer = view.buffer();
        buffer.select_range(
            &buffer.iter_at_line(9).expect("tail"),
            &buffer.iter_at_line(9).expect("tail"),
        );
        // The caret's line, with nothing selected (what Ctrl+Alt+M comments on).
        let editor = panel.review.comment_on("a.rs").expect("the editor");
        save_note(&editor, "Tail");
        assert_eq!(panel.review.drafts().len(), 1);
        crate::testutil::find_by_class::<gtk4::Button>(
            panel.content.upcast_ref(),
            "diff-discard-review",
        )
        .expect("Discard")
        .emit_clicked();
        assert!(panel.review.drafts().is_empty());
        window.close();
    }

    /// GTK: run from the window's construction test. The panel pops out into its own window,
    /// keeps its place (hidden) beside the thread, and comes back shown; the PR bar follows the
    /// branch and hands the clicked state over.
    pub(crate) fn the_panel_pops_out_docks_and_offers_a_pr(parent: &gtk4::Window) {
        let panel = DiffPanel::new(&crate::theme::Theme::diff_colours(Default::default()));
        let slot = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        slot.append(&panel.root);
        panel.set_shown(true);

        panel.pop_out(parent, "My thread");
        assert!(panel.is_detached());
        assert!(panel.is_shown());
        assert!(
            !panel.root.is_visible(),
            "the place beside the thread is hidden"
        );
        assert!(panel.root.first_child().is_none(), "and empty");
        let window = panel
            .content
            .root()
            .and_downcast::<adw::Window>()
            .expect("the content is in its own window");
        assert_eq!(window.title().as_deref(), Some("Changes — My thread"));
        panel.pop_out(parent, "again");
        assert_eq!(
            panel.content.root().and_downcast::<adw::Window>(),
            Some(window.clone()),
            "a second pop-out brings the same window up"
        );

        // Closing the window docks the panel back, shown.
        window.close();
        assert!(!panel.is_detached());
        assert!(panel.root.is_visible());
        assert_eq!(
            panel.root.first_child().as_ref(),
            Some(panel.content.upcast_ref::<gtk4::Widget>())
        );
        panel.pop_out(parent, "My thread");
        panel.dock();
        assert!(!panel.is_detached());

        let clicked = Rc::new(RefCell::new(None));
        let seen = Rc::clone(&clicked);
        panel.connect_open_pr(move |state| *seen.borrow_mut() = Some(state));
        let generation = panel.begin();
        panel.show_pr(
            generation,
            Some(Ok(PrOutcome::Ready(state(PushState::Ahead(2))))),
        );
        assert_eq!(
            panel.pr_view(),
            Some((
                "feat/x → master · 2 commits to push".to_owned(),
                "Push & Open PR".to_owned(),
                true
            ))
        );
        panel.pr_button.emit_clicked();
        assert_eq!(
            clicked.borrow().as_ref().map(|s| s.push.clone()),
            Some(PushState::Ahead(2))
        );
        // The push in flight: the button spins and cannot be pressed twice; the refresh after
        // it draws the bar afresh.
        panel.set_pushing(true);
        assert!(panel.is_pushing());
        assert_eq!(panel.pr_view().map(|v| v.2), Some(false));
        // A refresh landing mid-push (a turn ending, say) leaves it busy: no second push.
        panel.show_pr(
            generation,
            Some(Ok(PrOutcome::Ready(state(PushState::UpToDate)))),
        );
        assert!(panel.is_pushing());
        assert_eq!(panel.pr_view().map(|v| v.2), Some(false));
        // Its end gives the button back as that refresh left it.
        panel.set_pushing(false);
        assert!(!panel.is_pushing());
        assert_eq!(panel.pr_view().map(|v| v.2), Some(true));
        // A stale refresh changes nothing; no repository hides the bar.
        panel.show_pr(generation.wrapping_sub(1), None);
        assert!(panel.pr_view().is_some());
        panel.show_pr(generation, None);
        assert!(panel.pr_view().is_none());
        let generation = panel.begin();
        panel.show_pr(generation, Some(Err("boom".into())));
        assert_eq!(panel.pr_view().map(|v| v.2), Some(false));
    }
}

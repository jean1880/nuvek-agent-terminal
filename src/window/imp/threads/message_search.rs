//! Message search in the sidebar: with its toggle on, what the search entry holds is also looked
//! for in what the threads said (`Store::search`), and the matches are listed under the thread
//! rows, in an "In Messages" section of their own.
//!
//! The search runs [`MESSAGE_SEARCH_DEBOUNCE_MS`] after typing pauses, off the main thread
//! (`store_job`), and only its newest run may draw: every change bumps a generation, and an
//! answer for an older one is dropped. While the start-up backfill is still indexing threads the
//! section says so, and searches again every [`MESSAGE_SEARCH_REFRESH_MS`]. An index that cannot
//! be read is said to be unavailable, never "no match". Opening a hit opens (or shows) its
//! thread and brings the matching item into view; one older than the history the thread replays
//! is said to be so.

use std::cell::{Cell, RefCell};

use super::*;
use crate::window::sidebar_model::{
    hit_role_label, message_hit_rows, message_status, snippet_markup, MessageHitRow,
    MessageSearch as Found, MESSAGE_HITS, MESSAGE_SEARCH_DEBOUNCE_MS, MESSAGE_SEARCH_REFRESH_MS,
};
use agent_kit::store::SearchResults;

/// The message-search toggle's icon (bundled).
const MESSAGES_ICON: &str = "at-chat-message-new-symbolic";
/// What the search entry says while messages are searched too, and while they are not.
const PLACEHOLDER_MESSAGES: &str = "Search threads and messages";
const PLACEHOLDER_THREADS: &str = "Search threads";
/// What opening a hit says when its thread's view does not hold the item.
pub(super) const OLDER_THAN_SHOWN: &str =
    "This message is older than what this thread shows, so it cannot be scrolled to";

/// The widgets and state of the sidebar's "In Messages" section.
pub(in crate::window::imp) struct MessageSearch {
    /// In the search row, beside the entry: searches messages too while active.
    pub(super) toggle: gtk4::ToggleButton,
    /// Under the thread rows; hidden while the toggle is off or nothing is typed.
    pub(super) section: gtk4::Box,
    /// Why nothing is listed, or that the index is still being built.
    status: Label,
    list: gtk4::ListBox,
    /// What `list` shows, in its order.
    rows: RefCell<Vec<MessageHitRow>>,
    /// Bumped by every change of what to search for: only the newest search may draw.
    generation: Cell<u64>,
    /// The search waiting for typing to pause (or the next refresh while indexing).
    timer: RefCell<Option<glib::SourceId>>,
}

impl MessageSearch {
    pub(super) fn new() -> Self {
        let toggle = gtk4::ToggleButton::builder()
            .icon_name(MESSAGES_ICON)
            .tooltip_text("Also Search Messages")
            .css_classes(["flat"])
            .valign(Align::Center)
            .build();
        toggle.update_property(&[gtk4::accessible::Property::Label("Also search messages")]);
        let header = Label::builder()
            .label("In Messages")
            .xalign(0.0)
            .css_classes(["folder-label", "caption-heading"])
            .tooltip_text("What the threads said that matches the search")
            .build();
        let status = Label::builder()
            .xalign(0.0)
            .wrap(true)
            .visible(false)
            .css_classes(["dim-label", "caption", "message-search-status"])
            .accessible_role(gtk4::AccessibleRole::Status)
            .build();
        let list = gtk4::ListBox::builder()
            .selection_mode(gtk4::SelectionMode::None)
            .css_classes(["navigation-sidebar", "message-hits"])
            .visible(false)
            .build();
        list.update_property(&[gtk4::accessible::Property::Label(
            "Messages that match the search",
        )]);
        let section = Box::builder()
            .orientation(Orientation::Vertical)
            .spacing(4)
            .margin_top(8)
            .margin_start(8)
            .margin_end(8)
            .visible(false)
            .css_classes(["message-search"])
            .build();
        section.append(&header);
        section.append(&status);
        section.append(&list);
        Self {
            toggle,
            section,
            status,
            list,
            rows: RefCell::new(Vec::new()),
            generation: Cell::new(0),
            timer: RefCell::new(None),
        }
    }

    /// A new search generation: the waiting search (if any) is cancelled, and any answer still
    /// on its way for an older one will be dropped.
    fn next_generation(&self) -> u64 {
        if let Some(timer) = self.timer.borrow_mut().take() {
            timer.remove();
        }
        let next = self.generation.get().wrapping_add(1);
        self.generation.set(next);
        next
    }

    /// Hides the section and forgets what it listed.
    fn clear(&self) {
        clear_rows(&self.list);
        self.rows.borrow_mut().clear();
        self.list.set_visible(false);
        self.status.set_visible(false);
        self.section.set_visible(false);
    }

    /// Lists `rows` (what was `found`), with the status line for it. The same hits as shown
    /// already (a refresh while indexing that found nothing new) keep their widgets, so a
    /// keyboard user's place in the list is kept.
    fn show(&self, found: Found, rows: Vec<MessageHitRow>, indexing: usize, now: i64, home: &str) {
        let unchanged = self.list.is_visible() && *self.rows.borrow() == rows;
        if !unchanged {
            clear_rows(&self.list);
            for row in &rows {
                self.list.append(&hit_row(row, now, home));
            }
        }
        self.list.set_visible(!rows.is_empty());
        match message_status(found, indexing) {
            Some(text) => {
                self.status.set_text(&text);
                self.status.set_visible(true);
            }
            None => self.status.set_visible(false),
        }
        *self.rows.borrow_mut() = rows;
        self.section.set_visible(true);
    }

    /// The hit at `index` in the list, if any.
    fn hit_at(&self, index: i32) -> Option<MessageHitRow> {
        let index = usize::try_from(index).ok()?;
        self.rows.borrow().get(index).cloned()
    }
}

/// Threads the start-up backfill has still to index (0 once it has ended, or never started).
fn search_indexing() -> usize {
    SEARCH_BACKFILL.with(|b| {
        b.borrow()
            .as_ref()
            .filter(|b| !b.finished())
            .map_or(0, agent_kit::search_backfill::Backfill::remaining)
    })
}

/// One hit's row: its thread's title, "Archived" when it is, its age, and who said what, the
/// matched words emphasised. The thread's folder is its tooltip.
fn hit_row(hit: &MessageHitRow, now: i64, home: &str) -> gtk4::ListBoxRow {
    let top = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();
    top.append(
        &Label::builder()
            .label(&hit.title)
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .css_classes(["thread-title"])
            .build(),
    );
    if hit.archived {
        top.append(
            &Label::builder()
                .label("Archived")
                .css_classes(["dim-label", "caption", "message-hit-archived"])
                .build(),
        );
    }
    top.append(
        &Label::builder()
            .label(relative_time(now, hit.at))
            .css_classes(["thread-age", "dim-label", "caption"])
            .build(),
    );
    let snippet = Label::builder()
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk4::pango::WrapMode::WordChar)
        .lines(2)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(["caption", "message-hit-snippet"])
        .build();
    // The role is a fixed word; the snippet is escaped, only its match markers made markup.
    snippet.set_markup(&format!(
        "{}: {}",
        hit_role_label(&hit.role),
        snippet_markup(&hit.snippet)
    ));
    let column = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(2)
        .css_classes(["message-hit"])
        .build();
    column.append(&top);
    column.append(&snippet);
    let row = gtk4::ListBoxRow::builder()
        .child(&column)
        .tooltip_text(crate::utils::tildify(&hit.folder, home))
        .build();
    row.update_property(&[gtk4::accessible::Property::Label(
        &hit.accessible_label(now),
    )]);
    row
}

/// Brings `item` into view in `view`, or says (over the transcript) that it is older than the
/// history the thread shows.
pub(super) fn jump_to_hit(view: &ChatView, item: &str) {
    if view.show_item(item) {
        return;
    }
    if let Some(overlay) = view.toast_overlay() {
        overlay.add_toast(
            adw::Toast::builder()
                .title(OLDER_THAN_SHOWN)
                .use_markup(false)
                .build(),
        );
    }
}

impl AgentTerminalWindow {
    /// Wires the section into the sidebar: a hit opens where it was said.
    pub(super) fn connect_message_search(&self, search: &MessageSearch) {
        let obj = self.obj();
        search.toggle.connect_toggled(glib::clone!(
            #[weak]
            obj,
            move |_| obj.imp().message_search_changed()
        ));
        search.list.connect_row_activated(glib::clone!(
            #[weak]
            obj,
            move |_, row| {
                let imp = obj.imp();
                let hit = imp
                    .sidebar
                    .borrow()
                    .as_ref()
                    .and_then(|s| s.messages.hit_at(row.index()));
                if let Some(hit) = hit {
                    imp.open_message_hit(&hit.thread, &hit.item);
                }
            }
        ));
    }

    /// What to search for changed (the entry, the toggle, Show Archived): the search runs again
    /// once typing pauses, or the section goes away when there is nothing to search for.
    pub(super) fn message_search_changed(&self) {
        self.arm_message_search(MESSAGE_SEARCH_DEBOUNCE_MS);
    }

    /// Starts a new search generation, run after `delay_ms`; with the toggle off or nothing
    /// typed, hides the section instead.
    fn arm_message_search(&self, delay_ms: u64) -> Option<u64> {
        let sidebar = self.sidebar.borrow().clone()?;
        let search = &sidebar.messages;
        let generation = search.next_generation();
        let on = search.toggle.is_active();
        sidebar.search.set_placeholder_text(Some(if on {
            PLACEHOLDER_MESSAGES
        } else {
            PLACEHOLDER_THREADS
        }));
        if !on || sidebar.search.text().trim().is_empty() {
            search.clear();
            return None;
        }
        let obj = self.obj().downgrade();
        let timer =
            glib::timeout_add_local_once(std::time::Duration::from_millis(delay_ms), move || {
                if let Some(obj) = obj.upgrade() {
                    obj.imp().run_message_search(generation);
                }
            });
        *search.timer.borrow_mut() = Some(timer);
        Some(generation)
    }

    /// Runs search `generation` off the main thread, if it is still the newest; its answer is
    /// drawn by [`Self::message_search_landed`].
    fn run_message_search(&self, generation: u64) {
        let Some(sidebar) = self.sidebar.borrow().clone() else {
            return;
        };
        // It fired: a once source removes itself, so there is nothing left to cancel.
        sidebar.messages.timer.borrow_mut().take();
        if sidebar.messages.generation.get() != generation {
            return;
        }
        let query = sidebar.search.text().trim().to_owned();
        let archived = self.show_archived.get();
        let obj = self.obj().downgrade();
        glib::MainContext::default().spawn_local(async move {
            let found = store_job(move |s| s.search(&query, MESSAGE_HITS, archived)).await;
            if let Some(obj) = obj.upgrade() {
                obj.imp().message_search_landed(generation, found);
            }
        });
    }

    /// Draws what search `generation` found; false (nothing drawn) when a newer search has been
    /// asked for since. `None` is a store read that was not made, shown as unavailable.
    pub(super) fn message_search_landed(
        &self,
        generation: u64,
        found: Option<agent_kit::store::Result<SearchResults>>,
    ) -> bool {
        let Some(sidebar) = self.sidebar.borrow().clone() else {
            return false;
        };
        let search = &sidebar.messages;
        if search.generation.get() != generation {
            return false;
        }
        let (found, rows) = match found {
            Some(Ok(SearchResults::Hits(hits))) => {
                let rows =
                    message_hit_rows(hits, self.summaries.borrow().as_deref().unwrap_or_default());
                (Found::Found(rows.len()), rows)
            }
            Some(Ok(SearchResults::Unavailable)) => (Found::Unavailable, Vec::new()),
            Some(Err(e)) => {
                warn!("Cannot search messages: {e}");
                (Found::Unavailable, Vec::new())
            }
            None => (Found::Unavailable, Vec::new()),
        };
        let indexing = search_indexing();
        let home = env::var("HOME").unwrap_or_default();
        search.show(found, rows, indexing, sidebar_now(), &home);
        // What the backfill indexes meanwhile shows up without another keystroke.
        if indexing > 0 && found != Found::Unavailable {
            self.arm_message_search(MESSAGE_SEARCH_REFRESH_MS);
        }
        true
    }

    /// Opens (or shows) `thread` and brings its item `item` into view: at once when the thread
    /// is built, else once its build has replayed it.
    pub(super) fn open_message_hit(&self, thread: &str, item: &str) {
        if let Some(owner) = thread_owner(thread).filter(|w| w != &*self.obj()) {
            if owner.imp().is_closing() {
                self.show_toast("This thread is closing in another window");
                return;
            }
            owner.imp().open_message_hit(thread, item);
            owner.present();
            return;
        }
        let Some(page) = self.open_thread(thread, true) else {
            return;
        };
        // In the narrow overlay mode, picking a hit is done with the sidebar, as picking a row.
        if let Some(split) = self.split_view.borrow().as_ref() {
            if split.is_collapsed() {
                split.set_show_sidebar(false);
            }
        }
        let item = item.to_owned();
        let built = self
            .with_page_chat_mut(&page, |chat| match &chat.view {
                Some(view) => Some(view.clone()),
                None => {
                    chat.pending_jump = Some(item.clone());
                    None
                }
            })
            .flatten();
        if let Some(view) = built {
            // After the move to the newest messages that showing the page queued.
            glib::idle_add_local_once(move || jump_to_hit(&view, &item));
        }
    }
}

#[cfg(test)]
pub(in crate::window::imp) mod tests {
    use super::*;
    use agent_core::event::{ItemKind, ItemStatus, StreamKind, TurnState};

    /// The sidebar's message search.
    fn messages(window: &crate::window::AgentTerminalWindow) -> Rc<Sidebar> {
        window.imp().sidebar.borrow().clone().expect("a sidebar")
    }

    fn reply(store: &Store, thread: &str, item: &str, text: &str) {
        let events = [
            Envelope::new(Event::ItemStarted {
                kind: ItemKind::AssistantMessage,
                title: String::new(),
                input: None,
                parent: None,
            })
            .item(item),
            Envelope::new(Event::ContentDelta {
                stream: StreamKind::Assistant,
                text: text.to_owned(),
            })
            .item(item),
            Envelope::new(Event::ItemCompleted {
                status: ItemStatus::Completed,
                output: None,
                error: None,
            })
            .item(item),
            Envelope::turn_completed(TurnState::Completed, None),
        ];
        for env in events {
            store.append_event(thread, None, &env).expect("event");
        }
    }

    fn pump_until(what: &str, done: impl Fn() -> bool) {
        assert!(
            crate::testutil::pump_until(&glib::MainContext::default(), 10, done),
            "{what}"
        );
    }

    /// The whole path on the app's (in-memory) store: typing searches once typing pauses, a
    /// newer search drops an older answer, hits are drawn as escaped text, archived hits follow
    /// Show Archived, an unreadable index says so, and a hit opens its thread at the item.
    pub(crate) fn message_search_finds_and_opens_a_hit(
        window: &crate::window::AgentTerminalWindow,
    ) {
        let imp = window.imp();
        let store = app_store();
        let dir = tempfile::tempdir().expect("tmp");
        let cwd = dir.path().to_string_lossy().into_owned();
        let thread = store
            .create_thread(&cwd, Some("Deploy <notes>"))
            .expect("thread");
        reply(
            &store,
            &thread,
            "r1",
            "the <b>rollout</b> & the zebrafish step",
        );
        let archived = store.create_thread(&cwd, None).expect("thread");
        reply(&store, &archived, "r2", "an archived zebrafish");
        store.set_archived(&archived, true).expect("archive");
        for t in [&thread, &archived] {
            store
                .index_thread(t, agent_kit::store::INDEX_BUDGET)
                .expect("index");
        }
        imp.reload_summaries();
        let sidebar = messages(window);
        let search = &sidebar.messages;

        // Off: typing filters threads only, and the section stays hidden.
        sidebar.search.set_text("zebrafish");
        imp.message_search_changed();
        assert!(!search.section.is_visible());
        assert!(search.timer.borrow().is_none(), "nothing to run");

        // On: debounced, then drawn.
        search.toggle.set_active(true);
        assert!(search.timer.borrow().is_some(), "waits for typing to pause");
        assert_eq!(
            sidebar.search.placeholder_text().as_deref(),
            Some(PLACEHOLDER_MESSAGES)
        );
        pump_until("the search ran and drew its hit", || {
            search.rows.borrow().len() == 1
        });
        assert!(search.section.is_visible());
        let hit = search.hit_at(0).expect("a hit");
        assert_eq!(
            (hit.thread.as_str(), hit.item.as_str()),
            (thread.as_str(), "r1")
        );
        assert_eq!(hit.title, "Deploy <notes>");
        let row = search.list.row_at_index(0).expect("a row");
        let column = row.child().expect("column");
        let snippet = column
            .last_child()
            .and_then(|w| w.downcast::<Label>().ok())
            .expect("snippet label");
        // Shown as the thread wrote it: its tags are text, not markup.
        assert!(snippet.text().contains("<b>rollout</b> & the zebrafish"));
        assert!(snippet.label().contains("&lt;b&gt;rollout&lt;/b&gt; &amp;"));
        assert!(snippet.label().contains("<b>zebrafish</b>"));
        assert!(!search.status.is_visible(), "hits say it all");

        // A newer search drops the older answer.
        // (Replacing the text empties it for a moment, which hides the section at once.)
        sidebar.search.set_text("nothingmatchesthis");
        let stale = search.generation.get();
        imp.message_search_changed();
        let old_hit = agent_kit::store::SearchHit {
            thread_id: thread.clone(),
            item_id: "r1".into(),
            first_seq: 1,
            role: "assistant".into(),
            at: 0,
            snippet: "zebrafish".into(),
            rank: -1.0,
            archived: false,
        };
        assert!(!imp.message_search_landed(stale, Some(Ok(SearchResults::Hits(vec![old_hit])))));
        assert!(
            search.rows.borrow().is_empty(),
            "the stale answer drew nothing"
        );
        pump_until("the newer search drew", || {
            search.status.is_visible() && search.status.text() == "No messages match."
        });
        assert!(search.section.is_visible());
        assert!(search.rows.borrow().is_empty());

        // Archived hits only with Show Archived.
        sidebar.search.set_text("zebrafish");
        imp.show_archived.set(true);
        imp.message_search_changed();
        pump_until("the archived hit is listed", || {
            search.rows.borrow().len() == 2
        });
        assert!(search.rows.borrow().iter().any(|r| r.archived));
        imp.show_archived.set(false);
        imp.message_search_changed();
        pump_until("the archived hit left", || search.rows.borrow().len() == 1);

        // An index that cannot be read is unavailable, never "no match".
        let current = search.generation.get();
        assert!(imp.message_search_landed(current, Some(Ok(SearchResults::Unavailable))));
        assert!(search.rows.borrow().is_empty());
        assert!(search.status.text().starts_with("Search is unavailable"));
        let current = search.generation.get();
        assert!(imp.message_search_landed(current, None));
        assert!(search.status.text().starts_with("Search is unavailable"));

        // Opening a hit: the thread opens, and its build brings the item into view.
        imp.message_search_changed();
        pump_until("the hit is back", || search.rows.borrow().len() == 1);
        imp.open_message_hit(&thread, "r1");
        let page = imp.page_of_thread(&thread).expect("the thread opened");
        pump_until("the thread was built", || {
            imp.with_page_chat(&page, |c| c.view.is_some())
                .unwrap_or(false)
        });
        assert_eq!(
            imp.with_page_chat(&page, |c| c.pending_jump.clone()),
            Some(None),
            "the build took the jump"
        );
        let view = imp
            .with_page_chat(&page, |c| c.view.clone())
            .flatten()
            .expect("view");
        // Waiting for its layout, or already placed.
        let target = view.pending_jump_id();
        assert!(
            target.is_none() || target.as_deref() == Some("r1"),
            "{target:?}"
        );
        assert!(view.show_item("r1"), "the view holds the hit's item");
        assert!(!view.show_item("not-in-this-thread"));

        // Opened again, now built: the jump is made at once, after the page's own scroll.
        imp.open_message_hit(&thread, "r1");
        assert_eq!(
            imp.with_page_chat(&page, |c| c.pending_jump.clone()),
            Some(None),
            "a built thread needs no pending jump"
        );

        // Turning the toggle off hides the section.
        search.toggle.set_active(false);
        assert!(!search.section.is_visible());
        assert_eq!(
            sidebar.search.placeholder_text().as_deref(),
            Some(PLACEHOLDER_THREADS)
        );
        sidebar.search.set_text("");
        imp.close_row_now(&RowKey::Thread(thread.clone()));
        pump_until("the page closed", || imp.page_of_thread(&thread).is_none());
        store.delete_thread(&thread).expect("delete");
        store.delete_thread(&archived).expect("delete");
        imp.reload_summaries();
    }

    #[test]
    fn snippet_markup_parses_as_pango_markup() {
        use agent_kit::store::{SNIPPET_CLOSE, SNIPPET_OPEN};
        let snippet =
            format!("<span>&nbsp; {SNIPPET_OPEN}x{SNIPPET_CLOSE} \u{7} \"q\" {SNIPPET_OPEN}");
        let markup = snippet_markup(&snippet);
        let (_, text, _) = gtk4::pango::parse_markup(&markup, '\0').expect("valid markup");
        assert_eq!(text.as_str(), "<span>&nbsp; x   \"q\" ");
    }
}

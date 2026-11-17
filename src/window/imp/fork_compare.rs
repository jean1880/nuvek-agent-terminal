//! Compare with Fork… (the thread menu), `/compare` and the fork toast's Compare: finds the
//! threads a thread can be compared with (its source, its forks, its source's other forks:
//! [`agent_core::fork_compare::pairings`]), asks which when there are several, and opens a
//! [`CompareWindow`] on the pair. Every store read goes through `store_job`, off the main
//! thread; nothing here writes or starts a session.

use agent_core::adapter::Driver;
use agent_core::compare::Side;
use agent_core::fork_compare::{Pairing, Relation};
use agent_kit::store::CompareColumn;

use super::threads::store_job;
use super::*;
use crate::window::compare::{ColumnView, CompareContent, CompareWindow};
use crate::window::sidebar_model::display_title;

/// What a column is called: the thread's title, or that it is gone.
fn column_view(column: CompareColumn) -> ColumnView {
    let Some(summary) = column.summary else {
        return ColumnView {
            heading: "Deleted thread".to_owned(),
            detail: "This thread no longer exists".to_owned(),
            entries: Vec::new(),
        };
    };
    let agent = summary
        .driver
        .as_deref()
        .and_then(Driver::from_key)
        .map(|d| d.info().label.to_owned());
    let model = summary.model.filter(|m| !m.is_empty() && m != "default");
    let detail = match (agent, model) {
        (Some(agent), Some(model)) => format!("{agent} · {model}"),
        (Some(agent), None) => agent,
        (None, Some(model)) => model,
        (None, None) => "Agent unknown".to_owned(),
    };
    ColumnView {
        heading: display_title(&summary.title).to_owned(),
        detail,
        entries: column.entries,
    }
}

/// The window's content for a fork pair read as `columns` (source first, then the fork; or two
/// siblings), compared as `relation` says.
fn fork_content(relation: Relation, columns: Vec<CompareColumn>) -> CompareContent {
    let columns: Vec<ColumnView> = columns.into_iter().map(column_view).collect();
    let name = |i: usize| {
        columns
            .get(i)
            .map(|c| format!("“{}”", c.heading))
            .unwrap_or_default()
    };
    let (subtitle, empty) = match relation {
        Relation::Source | Relation::Fork => (
            format!("{} and its fork {}, since the fork", name(0), name(1)),
            "Neither thread has a message since the fork.",
        ),
        Relation::Sibling => (
            format!("{} and {}, two forks of one thread", name(0), name(1)),
            "Neither fork has a message of its own yet.",
        ),
    };
    CompareContent {
        subtitle,
        columns,
        empty: empty.to_owned(),
    }
}

impl AgentTerminalWindow {
    /// Compare with Fork… and `/compare` on `thread`: opens the comparison at once when there is
    /// one thread to compare it with, asks which when there are several, and says so when there
    /// is none.
    pub(super) fn compare_with_fork(&self, thread: &str) {
        let obj = self.obj().downgrade();
        let thread = thread.to_owned();
        glib::MainContext::default().spawn_local(async move {
            // The titles come with the pairings, read on the worker too: the picker names them.
            let found = store_job({
                let thread = thread.clone();
                move |store| -> agent_kit::store::Result<(String, Vec<(Pairing, String)>)> {
                    let title = |id: &str| -> agent_kit::store::Result<String> {
                        Ok(store
                            .thread_summary(id)?
                            .map(|s| display_title(&s.title).to_owned())
                            .unwrap_or_default())
                    };
                    let own = title(&thread)?;
                    let mut named = Vec::new();
                    for pairing in store.fork_pairings(&thread)? {
                        let other = title(&pairing.other)?;
                        named.push((pairing, other));
                    }
                    Ok((own, named))
                }
            })
            .await;
            let Some(obj) = obj.upgrade() else { return };
            let imp = obj.imp();
            match found {
                Some(Ok((title, pairings))) => match pairings.as_slice() {
                    [] => imp.show_toast(
                        "Nothing to compare with: this thread has no fork, and is no fork",
                    ),
                    [(one, _)] => imp.open_fork_compare(&thread, one.clone()),
                    _ => imp.pick_fork(&thread, &title, pairings),
                },
                Some(Err(e)) => present_message(&obj, "Cannot Compare the Thread", &e.to_string()),
                None => imp.show_toast("Cannot read the thread"),
            }
        });
    }

    /// Asks which of `pairings` (each with its thread's title) to compare `thread`, titled
    /// `title`, with.
    fn pick_fork(&self, thread: &str, title: &str, pairings: Vec<(Pairing, String)>) {
        let dialog = confirm_dialog(
            "Compare with Fork",
            &format!("Choose the thread to read beside “{title}”."),
            ("cancel", "Cancel"),
            ("compare", "Compare"),
            adw::ResponseAppearance::Suggested,
            true,
        );
        let labels: Vec<String> = pairings
            .iter()
            .map(|(p, other)| format!("{other} ({})", p.relation.label()))
            .collect();
        let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
        let choice = adw::ComboRow::builder()
            .title("Thread")
            .model(&gtk4::StringList::new(&labels))
            .build();
        let list = gtk4::ListBox::builder()
            .selection_mode(gtk4::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        list.append(&choice);
        dialog.set_extra_child(Some(&list));
        let thread = thread.to_owned();
        self.ask(dialog, "compare", move |imp, go| {
            if !go {
                return;
            }
            let picked = usize::try_from(choice.selected())
                .ok()
                .and_then(|i| pairings.get(i));
            if let Some((pairing, _)) = picked {
                imp.open_fork_compare(&thread, pairing.clone());
            }
        });
    }

    /// Opens a compare window on `thread` and `pairing`'s other thread, and reads them. The
    /// window keeps itself alive while it is open, so Refresh works with no one holding it.
    pub(super) fn open_fork_compare(&self, thread: &str, pairing: Pairing) {
        let window = CompareWindow::new(self.obj().upcast_ref(), "Compare Forks");
        let sides = pairing.sides(thread);
        let relation = pairing.relation;
        load_fork_compare(&window, sides.clone(), relation);
        window.connect_refresh(move |window| load_fork_compare(window, sides.clone(), relation));
        window.window().present();
    }
}

/// Reads `sides` off the main thread and shows them in `window`, unless a newer read started.
fn load_fork_compare(window: &CompareWindow, sides: [Side; 2], relation: Relation) {
    let generation = window.begin();
    let window = window.clone();
    glib::MainContext::default().spawn_local(async move {
        let read =
            store_job(move |store| store.compare_columns(&sides).map_err(|e| e.to_string())).await;
        match read {
            Some(Ok(columns)) => window.show(generation, &fork_content(relation, columns)),
            Some(Err(why)) => window.show_error(generation, &why),
            None => window.show_error(generation, "The thread store could not be read."),
        }
    });
}

#[cfg(test)]
pub(super) mod tests {
    use agent_core::compare::{Entry, Role};
    use agent_kit::store::ThreadSummary;

    use super::*;

    fn summary(title: &str, driver: Option<&str>, model: Option<&str>) -> ThreadSummary {
        ThreadSummary {
            id: "t".into(),
            title: title.into(),
            cwd: "/w".into(),
            updated_at: 0,
            driver: driver.map(str::to_owned),
            model: model.map(str::to_owned),
            unread: false,
            archived: false,
            pin_rank: None,
            tags: Vec::new(),
        }
    }

    #[test]
    fn columns_name_their_thread_agent_and_model_or_say_it_is_gone() {
        let column = |s: Option<ThreadSummary>| {
            column_view(CompareColumn {
                summary: s,
                entries: Vec::new(),
            })
        };
        let c = column(Some(summary("Plan", Some("claude"), Some("opus"))));
        assert_eq!(
            (c.heading.as_str(), c.detail.as_str()),
            ("Plan", "Claude · opus")
        );
        let c = column(Some(summary("Plan", Some("codex"), Some("default"))));
        assert_eq!(c.detail, "Codex", "the agent's own default names no model");
        let c = column(Some(summary("", None, None)));
        assert_eq!(c.detail, "Agent unknown");
        assert!(
            !c.heading.is_empty(),
            "an untitled thread still has a heading"
        );
        let c = column(None);
        assert_eq!(c.heading, "Deleted thread");
    }

    #[test]
    fn the_subtitle_says_how_the_two_are_related() {
        let columns = || {
            vec![
                CompareColumn {
                    summary: Some(summary("A", Some("claude"), None)),
                    entries: vec![Entry {
                        role: Role::Prompt,
                        text: "hi".into(),
                        status: "completed".into(),
                        seq: 3,
                    }],
                },
                CompareColumn {
                    summary: Some(summary("B", Some("claude"), None)),
                    entries: Vec::new(),
                },
            ]
        };
        let fork = fork_content(Relation::Source, columns());
        assert_eq!(fork.subtitle, "“A” and its fork “B”, since the fork");
        assert_eq!(fork.columns[0].entries.len(), 1);
        assert_eq!(
            fork_content(Relation::Fork, columns()).subtitle,
            fork.subtitle
        );
        let siblings = fork_content(Relation::Sibling, columns());
        assert_eq!(siblings.subtitle, "“A” and “B”, two forks of one thread");
    }

    /// Fork thread records the link through the real fork path, and Compare opens a window on
    /// the pair showing each side's own turns since the fork. Run from the window test, on its
    /// store (no writer: every store job runs in place).
    pub(in crate::window) fn a_fork_is_linked_and_compared_side_by_side(
        window: &crate::window::AgentTerminalWindow,
    ) {
        use agent_core::event::{Envelope, ItemKind, ItemStatus, StreamKind};
        let imp = window.imp();
        let ctx = glib::MainContext::default();
        let store = super::super::threads::app_store();
        let dir = tempfile::tempdir().expect("dir");
        let dir_s = dir.path().to_string_lossy().into_owned();
        let source = store
            .create_thread(&dir_s, Some("Fork source"))
            .expect("source");
        let pt = store
            .add_provider_thread(&source, "claude", "opus")
            .expect("pt");
        store.set_active_provider_thread(&source, &pt).expect("set");
        let reply = |thread: &str, pt: Option<&str>, item: &str, text: &str| {
            for env in [
                Envelope::item_started(item, ItemKind::AssistantMessage, "", None, None),
                Envelope::delta(item, StreamKind::Assistant, text),
                Envelope::item_completed(item, ItemStatus::Completed, None, None),
            ] {
                store.append_event(thread, pt, &env).expect("event");
            }
        };
        store
            .append_user_message(&source, Some(&pt), "before the fork")
            .expect("prompt");
        reply(&source, Some(&pt), "r0", "an answer both share");
        imp.reload_summaries();
        let before: std::collections::HashSet<String> = store
            .list_threads(true)
            .expect("list")
            .into_iter()
            .map(|t| t.id)
            .collect();

        imp.fork_thread(&source, Driver::Claude, Some(dir_s.clone()), false);
        let mut fork = None;
        assert!(
            crate::testutil::pump_until(&ctx, 20, || {
                fork = store
                    .list_threads(true)
                    .ok()
                    .into_iter()
                    .flatten()
                    .map(|t| t.id)
                    .find(|id| !before.contains(id));
                fork.is_some()
            }),
            "the fork is stored"
        );
        let fork = fork.unwrap_or_default();
        let link = store.fork_link(&fork).expect("read").expect("a fork link");
        assert_eq!(link.source_thread, source);
        let last = store
            .events(&source, None, 1000)
            .expect("events")
            .last()
            .map(|(seq, _)| *seq);
        assert_eq!(
            Some(link.fork_seq),
            last,
            "the fork point is the source's last event"
        );
        assert_eq!(
            store.fork_link(&source).expect("read"),
            None,
            "the source is untouched"
        );

        // A compaction takes the same path and is linked to nothing.
        let known: std::collections::HashSet<String> =
            before.iter().cloned().chain([fork.clone()]).collect();
        imp.fork_thread(&source, Driver::Claude, Some(dir_s.clone()), true);
        let mut compacted = None;
        assert!(
            crate::testutil::pump_until(&ctx, 20, || {
                compacted = store
                    .list_threads(true)
                    .ok()
                    .into_iter()
                    .flatten()
                    .map(|t| t.id)
                    .find(|id| !known.contains(id));
                compacted.is_some()
            }),
            "the compaction is stored"
        );
        let compacted = compacted.unwrap_or_default();
        assert_eq!(store.fork_link(&compacted).expect("read"), None);
        assert!(
            store
                .fork_pairings(&source)
                .expect("pairings")
                .iter()
                .all(|p| p.other != compacted),
            "a compaction is never offered for comparison"
        );

        // Each side goes on its own way.
        store
            .append_user_message(&source, Some(&pt), "the source goes on")
            .expect("prompt");
        reply(&source, Some(&pt), "r1", "source's own answer");
        store
            .append_user_message(&fork, None, "the fork tries this")
            .expect("prompt");
        reply(&fork, None, "r2", "fork's own answer");

        let pairings = store.fork_pairings(&fork).expect("pairings");
        assert_eq!(pairings.len(), 1);
        // Nothing holds the window but the window itself, as from the menu or the toast.
        imp.open_fork_compare(&fork, pairings[0].clone());
        let compare = gtk4::Window::list_toplevels()
            .into_iter()
            .filter_map(|w| w.downcast::<gtk4::Window>().ok())
            .find(|w| w.has_css_class("compare-window") && w.is_visible())
            .expect("the compare window");
        let root: gtk4::Widget = compare.clone().upcast();
        assert!(
            crate::testutil::pump_until(&ctx, 20, || {
                crate::testutil::label_texts(&root)
                    .iter()
                    .any(|t| t == "fork's own answer")
            }),
            "the comparison is read and drawn"
        );
        let texts = crate::testutil::label_texts(&root);
        for shown in [
            "the source goes on",
            "source's own answer",
            "the fork tries this",
        ] {
            assert!(texts.iter().any(|t| t == shown), "{shown:?} in {texts:#?}");
        }
        for hidden in ["before the fork", "an answer both share"] {
            assert!(
                !texts.iter().any(|t| t.contains(hidden)),
                "what came before the fork is not compared: {hidden:?}"
            );
        }
        assert_eq!(
            crate::testutil::find_all_by_class::<gtk4::Box>(&root, "compare-cell").len(),
            2,
            "one turn each, side by side"
        );
        let problems = crate::testutil::button_problems(&root, 24);
        assert!(problems.is_empty(), "{problems:#?}");
        // Refresh reads again and draws what came since.
        store
            .append_user_message(&fork, None, "a second try")
            .expect("prompt");
        let refresh = crate::testutil::find_all::<gtk4::Button>(&root)
            .into_iter()
            .find(|b| b.icon_name().as_deref() == Some("at-view-refresh-symbolic"))
            .expect("refresh");
        refresh.emit_clicked();
        assert!(
            crate::testutil::pump_until(&ctx, 20, || {
                crate::testutil::label_texts(&root)
                    .iter()
                    .any(|t| t == "a second try")
            }),
            "Refresh draws the new message"
        );
        compare.close();
        assert!(crate::testutil::pump_until(&ctx, 10, || !compare.is_visible()));

        // Deleting the source leaves the fork's link; there is then nothing to compare it with.
        store.delete_thread(&source).expect("delete");
        assert!(store.fork_link(&fork).expect("read").is_some());
        assert!(store.fork_pairings(&fork).expect("pairings").is_empty());
        imp.reload_summaries();
    }
}

//! Compare with Fork… (the thread menu), `/compare` and the fork toast's Compare: finds the
//! threads a thread can be compared with (its source, its forks, its source's other forks:
//! [`agent_core::fork_compare::pairings`]), asks which when there are several, and opens a
//! [`CompareWindow`] on the pair: the transcripts, and each side's file changes since the fork's
//! baseline. Every store read goes through `store_job` and every git call through `off_main`,
//! off the main thread; nothing here writes the store or starts a session.
//!
//! A fork's baseline is taken here too ([`fork_baseline`], when Fork thread runs), its ref kept
//! once the fork is stored ([`keep_fork_baseline`]) and dropped with the fork
//! ([`drop_fork_baseline`]).

use std::path::{Path, PathBuf};

use agent_core::adapter::Driver;
use agent_core::fork_compare::{Pairing, Relation};
use agent_kit::store::{CompareColumn, ForkColumns, Store};

use super::threads::{off_main, store_job};
use super::*;
use crate::git::{BaselineOutcome, ForkBaseline, ForkChanges};
use crate::window::compare::{
    ChangesColumn, ChangesContent, ChangesState, ColumnView, CompareContent, CompareWindow,
};
use crate::window::sidebar_model::display_title;

/// Said above the file changes of two threads in one folder: what they can and cannot show.
pub(super) const SHARED_FOLDER: &str = "Both threads work in the same folder, so their file \
     changes cannot be told apart: this is everything that changed there since the fork, by \
     either thread or by anyone else.";

/// Takes a fork's baseline of `dir` (the source's folder) off the main thread: `None` when it
/// is not in a repository, git is busy or the snapshot fails (said in the log), and the fork
/// then compares by transcript only.
pub(super) async fn fork_baseline(dir: String) -> Option<ForkBaseline> {
    let taken = off_main("the fork baseline", move || {
        crate::git::take_fork_baseline(Path::new(&dir))
    })
    .await;
    match taken {
        Ok(BaselineOutcome::Taken(baseline)) => {
            if !baseline.skipped.is_empty() {
                info!(
                    "The fork's baseline left out {} untracked path(s)",
                    baseline.skipped.len()
                );
            }
            Some(baseline)
        }
        Ok(BaselineOutcome::NotRepo) => None,
        Ok(BaselineOutcome::Busy) => {
            info!("Git was busy: the fork has no baseline to compare files against");
            None
        }
        Err(e) => {
            warn!("Could not take the fork's baseline: {e}");
            None
        }
    }
}

/// Points fork `thread`'s baseline ref at `commit` in the repository of `dir`, off the main
/// thread, now that the thread is stored.
pub(super) fn keep_fork_baseline(dir: PathBuf, thread: String, commit: String) {
    glib::MainContext::default().spawn_local(async move {
        let kept = off_main("the fork baseline", move || {
            crate::git::keep_fork_baseline(&dir, &thread, &commit)
        })
        .await;
        if let Err(e) = kept {
            warn!("Could not keep the fork's baseline: {e}");
        }
    });
}

/// The folder and baseline commit of fork `thread`, read before it is deleted.
pub(super) fn stored_baseline(store: &Store, thread: &str) -> Option<(String, String)> {
    let commit = store.fork_link(thread).ok().flatten()?.base_commit?;
    let cwd = store.thread_summary(thread).ok().flatten()?.cwd;
    Some((cwd, commit))
}

/// Deletes deleted fork `thread`'s baseline ref in the repository of `dir`, off the main
/// thread, while it still points at `commit`.
pub(super) fn drop_fork_baseline(dir: String, thread: String, commit: String) {
    glib::MainContext::default().spawn_local(async move {
        let dropped = off_main("the fork baseline", move || {
            crate::git::drop_fork_baseline(Path::new(&dir), &thread, &commit)
        })
        .await;
        if let Err(e) = dropped {
            warn!("Could not remove the fork's baseline: {e}");
        }
    });
}

/// Each side's changes since its baseline (blocking), and whether the two are in one folder. Two
/// sides in one folder with one baseline are read once.
fn read_changes(dirs: [Option<String>; 2], bases: [Option<String>; 2]) -> (Vec<ForkChanges>, bool) {
    let side = |dir: Option<&str>, base: Option<&str>| match dir {
        None => ForkChanges::Unavailable("This thread no longer exists".to_owned()),
        Some(dir) => crate::git::fork_changes(Path::new(dir), base)
            .unwrap_or_else(|e| ForkChanges::Unavailable(agent_core::redact::redact(&e))),
    };
    let first = side(dirs[0].as_deref(), bases[0].as_deref());
    let second = if dirs[0] == dirs[1] && bases[0] == bases[1] {
        first.clone()
    } else {
        side(dirs[1].as_deref(), bases[1].as_deref())
    };
    let top = |c: &ForkChanges| match c {
        ForkChanges::Ready { toplevel, .. } => Some(toplevel.clone()),
        _ => None,
    };
    let shared = (dirs[0].is_some() && dirs[0] == dirs[1])
        || (top(&first).is_some() && top(&first) == top(&second));
    (vec![first, second], shared)
}

/// The Changes tab for sides headed `headings`, read as `sides`; `shared` when they are in one
/// folder, which the note says: one column then stands for both when they share a baseline too.
fn changes_content(headings: &[String], sides: Vec<ForkChanges>, shared: bool) -> ChangesContent {
    let state = |c: ForkChanges| match c {
        ForkChanges::NotRepo => ChangesState::NotRepo,
        ForkChanges::Unavailable(why) => ChangesState::Unavailable(why),
        ForkChanges::Ready { diff, .. } => ChangesState::Ready(diff),
    };
    let heading = |i: usize| headings.get(i).cloned().unwrap_or_default();
    let one_baseline = matches!(
        sides.as_slice(),
        [ForkChanges::Ready { diff: a, .. }, ForkChanges::Ready { diff: b, .. }] if a.from == b.from
    );
    let note = shared.then(|| SHARED_FOLDER.to_owned());
    if shared && one_baseline {
        let first = sides.into_iter().next().map(state);
        return ChangesContent {
            note,
            shared,
            columns: first
                .map(|state| ChangesColumn {
                    heading: format!("Shared by “{}” and “{}”", heading(0), heading(1)),
                    state,
                })
                .into_iter()
                .collect(),
        };
    }
    ChangesContent {
        note,
        shared,
        columns: sides
            .into_iter()
            .enumerate()
            .map(|(i, side)| ChangesColumn {
                heading: heading(i),
                state: state(side),
            })
            .collect(),
    }
}

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
        let thread = thread.to_owned();
        load_fork_compare(&window, thread.clone(), pairing.clone());
        window.connect_refresh(move |window| {
            load_fork_compare(window, thread.clone(), pairing.clone())
        });
        window.window().present();
    }
}

/// Reads `thread` and `pairing`'s other thread off the main thread and shows them in `window`:
/// the transcripts from the store, then each side's file changes from git. Only the newest read
/// of each draws; holds `window` weakly across awaits so closing it stops the work.
fn load_fork_compare(window: &CompareWindow, thread: String, pairing: Pairing) {
    let generation = window.begin();
    let changes = window.begin_changes();
    let relation = pairing.relation;
    let weak = window.downgrade();
    glib::MainContext::default().spawn_local(async move {
        let read = store_job(move |store| {
            store
                .fork_columns(&thread, &pairing)
                .map_err(|e| e.to_string())
        })
        .await;
        let Some(window) = weak.upgrade() else { return };
        let ForkColumns { columns, bases } = match read {
            Some(Ok(read)) => read,
            Some(Err(why)) => {
                window.show_error(generation, &why);
                window.show_changes_error(changes, &why);
                return;
            }
            None => {
                let why = "The thread store could not be read.";
                window.show_error(generation, why);
                window.show_changes_error(changes, why);
                return;
            }
        };
        let dirs: Vec<Option<String>> = columns
            .iter()
            .map(|c| c.summary.as_ref().map(|s| s.cwd.clone()))
            .collect();
        let content = fork_content(relation, columns);
        let headings: Vec<String> = content.columns.iter().map(|c| c.heading.clone()).collect();
        window.show(generation, &content);
        drop(window);
        let dirs = [
            dirs.first().cloned().flatten(),
            dirs.get(1).cloned().flatten(),
        ];
        let sides = off_main("the fork comparison", move || Ok(read_changes(dirs, bases))).await;
        let Some(window) = weak.upgrade() else { return };
        match sides {
            Ok((sides, shared)) => {
                window.show_changes(changes, &changes_content(&headings, sides, shared))
            }
            Err(why) => window.show_changes_error(changes, &why),
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

    fn diff(from: &str, paths: &[&str]) -> crate::git::TabDiff {
        crate::git::TabDiff {
            stats: paths
                .iter()
                .map(|p| crate::diff::FileStat {
                    path: (*p).to_owned(),
                    old_path: None,
                    added: Some(1),
                    deleted: Some(0),
                })
                .collect(),
            text: String::new(),
            omitted_lines: 0,
            too_large: false,
            undo_to: None,
            from: from.to_owned(),
            to_checkpoint: None,
        }
    }

    fn ready(top: &str, from: &str, paths: &[&str]) -> ForkChanges {
        ForkChanges::Ready {
            toplevel: PathBuf::from(top),
            diff: diff(from, paths),
        }
    }

    #[test]
    fn two_folders_show_two_columns_and_what_both_changed() {
        let headings = ["A".to_owned(), "B".to_owned()];
        let content = changes_content(
            &headings,
            vec![
                ready("/a", "base", &["x.rs", "y.rs"]),
                ready("/b", "base", &["y.rs", "z.rs"]),
            ],
            false,
        );
        assert_eq!(content.note, None);
        assert_eq!(content.columns.len(), 2);
        assert_eq!(content.columns[1].heading, "B");
        assert_eq!(
            crate::window::compare::changed_in_all(&content.columns),
            ["y.rs"]
        );
        // A side with nothing to show says why, in its own column.
        let content = changes_content(
            &headings,
            vec![
                ready("/a", "base", &["x.rs"]),
                ForkChanges::Unavailable("git is busy".into()),
            ],
            false,
        );
        assert_eq!(
            content.columns[1].state,
            ChangesState::Unavailable("git is busy".into())
        );
        assert!(crate::window::compare::changed_in_all(&content.columns).is_empty());
        let content = changes_content(&headings, vec![ForkChanges::NotRepo; 2], false);
        assert!(content
            .columns
            .iter()
            .all(|c| c.state == ChangesState::NotRepo));
    }

    #[test]
    fn one_folder_is_one_column_that_says_its_changes_cannot_be_told_apart() {
        let headings = ["A".to_owned(), "B".to_owned()];
        let content = changes_content(
            &headings,
            vec![
                ready("/w", "base", &["x.rs"]),
                ready("/w", "base", &["x.rs"]),
            ],
            true,
        );
        assert_eq!(content.note.as_deref(), Some(SHARED_FOLDER));
        assert_eq!(content.columns.len(), 1, "one folder, one diff");
        assert_eq!(content.columns[0].heading, "Shared by “A” and “B”");
        assert!(
            crate::window::compare::changed_in_all(&content.columns).is_empty(),
            "a file is not 'changed on both sides' just because the folder is one"
        );
        // Two siblings forked at different times from one folder: each against its own
        // baseline, still saying the folder is one (`shared: true` suppresses "both sides").
        let content = changes_content(
            &headings,
            vec![ready("/w", "one", &["x.rs"]), ready("/w", "two", &["x.rs"])],
            true,
        );
        assert!(content.shared);
        assert_eq!(content.note.as_deref(), Some(SHARED_FOLDER));
        assert_eq!(content.columns.len(), 2);
    }

    #[test]
    fn a_thread_that_is_gone_or_a_side_in_the_same_folder_is_read_as_such() {
        let plain = tempfile::tempdir().expect("dir");
        let dir = plain.path().to_string_lossy().into_owned();
        let (sides, shared) = read_changes([Some(dir.clone()), None], [None, None]);
        assert!(!shared);
        assert_eq!(
            sides[1],
            ForkChanges::Unavailable("This thread no longer exists".into())
        );
        let (sides, shared) = read_changes([Some(dir.clone()), Some(dir)], [None, None]);
        assert!(shared, "one folder, whether or not it is a repository");
        assert_eq!(sides[0], sides[1]);
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
        assert_eq!(
            (link.base_tree.as_deref(), link.base_commit.as_deref()),
            (None, None),
            "a folder outside a repository has no baseline"
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

    /// Git in `dir` with no user or system configuration but an identity.
    fn git(dir: &Path, args: &[&str]) -> Option<String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    /// A fork made in a repository takes a baseline of its source's folder without touching the
    /// user's index, HEAD or stash, keeps it under its own ref, compares the folder's changes
    /// since then (one folder: one diff, saying it cannot tell the threads apart), and its ref
    /// goes when the fork is deleted. Run from the window test (no store writer).
    pub(in crate::window) fn a_forks_files_are_compared_against_its_baseline(
        window: &crate::window::AgentTerminalWindow,
    ) {
        let imp = window.imp();
        let ctx = glib::MainContext::default();
        let store = super::super::threads::app_store();
        let repo = tempfile::tempdir().expect("dir");
        let dir = repo.path();
        if git(dir, &["init", "-q"]).is_none() {
            eprintln!("git is not available; skipping");
            return;
        }
        std::fs::write(dir.join("a.txt"), "one\n").expect("write");
        std::fs::write(dir.join("b.txt"), "two\n").expect("write");
        git(dir, &["add", "."]).expect("add");
        git(dir, &["commit", "-qm", "first"]).expect("commit");
        // Work in progress before the fork: staged, then edited again, and an untracked secret.
        std::fs::write(dir.join("a.txt"), "staged\n").expect("write");
        git(dir, &["add", "a.txt"]).expect("add");
        std::fs::write(dir.join("a.txt"), "before the fork\n").expect("write");
        std::fs::write(dir.join(".env"), "TOKEN=x\n").expect("write");
        let user_state = || {
            (
                std::fs::read(dir.join(".git/index")).unwrap_or_default(),
                git(dir, &["rev-parse", "HEAD"]),
                git(dir, &["stash", "list"]),
                git(dir, &["status", "--porcelain"]),
            )
        };
        let before_fork = user_state();

        let dir_s = dir.to_string_lossy().into_owned();
        let source = store
            .create_thread(&dir_s, Some("Baseline source"))
            .expect("source");
        let pt = store
            .add_provider_thread(&source, "claude", "opus")
            .expect("pt");
        store.set_active_provider_thread(&source, &pt).expect("set");
        store
            .append_user_message(&source, Some(&pt), "change some files")
            .expect("prompt");
        imp.reload_summaries();
        let known: std::collections::HashSet<String> = store
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
                    .find(|id| !known.contains(id));
                fork.is_some()
            }),
            "the fork is stored"
        );
        let fork = fork.unwrap_or_default();
        let link = store.fork_link(&fork).expect("read").expect("a link");
        let tree = link.base_tree.clone().expect("a baseline tree");
        let commit = link.base_commit.clone().expect("a baseline commit");
        let refname = crate::git::fork_ref(&fork).expect("a ref name");
        assert!(
            crate::testutil::pump_until(&ctx, 20, || {
                git(dir, &["rev-parse", "-q", "--verify", &refname]).as_deref()
                    == Some(commit.as_str())
            }),
            "the baseline is kept under the fork's ref"
        );
        assert_eq!(
            user_state(),
            before_fork,
            "the fork touched the user's git state"
        );
        assert_eq!(
            git(dir, &["cat-file", "-p", &format!("{tree}:a.txt")]).as_deref(),
            Some("before the fork"),
            "the baseline is the working tree as it was"
        );
        assert_eq!(
            git(dir, &["cat-file", "-e", &format!("{tree}:.env")]),
            None,
            "a secret-looking untracked file is never captured"
        );

        // After the fork, one file changes.
        std::fs::write(dir.join("b.txt"), "after the fork\n").expect("write");
        let pairings = store.fork_pairings(&fork).expect("pairings");
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
                    .any(|t| t == SHARED_FOLDER)
            }),
            "the Changes tab says the folder is one"
        );
        let texts = crate::testutil::label_texts(&root);
        assert!(
            texts.iter().any(|t| t == "b.txt  +1 −1"),
            "the change since the fork: {texts:#?}"
        );
        assert!(
            !texts.iter().any(|t| t.starts_with("a.txt")),
            "work from before the fork is not counted: {texts:#?}"
        );
        assert!(texts.iter().any(|t| t == "1 file, +1 −1"));
        assert_eq!(
            crate::testutil::find_all_by_class::<gtk4::Box>(&root, "compare-changes-column").len(),
            1,
            "one folder, one diff"
        );
        let problems = crate::testutil::button_problems(&root, 24);
        assert!(problems.is_empty(), "{problems:#?}");
        assert_eq!(
            user_state().0,
            before_fork.0,
            "reading the diff left the index"
        );
        compare.close();
        assert!(crate::testutil::pump_until(&ctx, 10, || !compare.is_visible()));

        // Deleting the fork removes its baseline ref; the source's folder is otherwise as it was.
        imp.delete_thread_now(&fork);
        assert!(
            crate::testutil::pump_until(&ctx, 20, || git(
                dir,
                &["for-each-ref", "refs/agent-terminal"]
            )
            .is_some_and(|s| s.is_empty())),
            "the baseline went with its fork"
        );
        assert_eq!(git(dir, &["rev-parse", "HEAD"]), before_fork.1);
        assert_eq!(git(dir, &["stash", "list"]), before_fork.2);
        store.delete_thread(&source).expect("delete");
        imp.reload_summaries();
    }
}

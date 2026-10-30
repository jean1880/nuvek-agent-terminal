//! The sticky "sub-agents at work" shelf above the composer, so what the workers do never gets
//! lost in the history. It shows a one-line summary ("2 workers active · 1 finished"), collapsed
//! by default; opened, one row per docked sub-agent with what it is running now (`Run cargo test`,
//! `Read src/config.rs`) or how it ended. A row opens that sub-agent on its own (the explorer's
//! dialog). Built from the shared shelf parts (`shelf.rs`), as the background task shelf is.
//!
//! A sub-agent docks when it is seen running and stays docked after it finishes, until the next
//! prompt is sent or "Clear finished" is pressed; then the shelf slides away once none runs.
//! Sub-agents that finished before the thread was opened (replayed history) never dock: the
//! header's sub-agent button lists those.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use gtk4::prelude::*;

use super::model::{Body, SubagentSummary, ToolStatus, Transcript};
use super::payload;
use super::shelf::{Shelf, ShelfRow};
use super::subagents::{counts, set_if_changed};

/// The shelf's one-line summary: the running ones, then the finished ones still docked.
pub fn title_text(running: usize, finished: usize, unknown: usize) -> String {
    let active = match running {
        1 => "1 worker active".to_owned(),
        n => format!("{n} workers active"),
    };
    let done = match finished {
        1 => "1 worker finished".to_owned(),
        n => format!("{n} workers finished"),
    };
    let known = match (running, finished) {
        (_, 0) => active,
        (0, _) => done,
        (_, n) => format!("{active} · {n} finished"),
    };
    match (running + finished, unknown) {
        (_, 0) => known,
        (0, 1) => "1 worker unknown".to_owned(),
        (0, n) => format!("{n} workers unknown"),
        (_, n) => format!("{known} · {n} unknown"),
    }
}

/// What sub-agent `id` is doing now: its running step, else its latest one (`None`: no step
/// yet), titled as its card is (`Run <command>`, `Read <path>`, …).
pub fn current_step(model: &Transcript, id: &str) -> Option<String> {
    let agent = model.get(id)?;
    // Newest first: the first running step wins; failing that, the newest step of all.
    let mut latest = None;
    let mut running = None;
    for step in agent.children.iter().rev().filter_map(|c| model.get(c)) {
        let Body::Tool(tool) = &step.body else {
            continue;
        };
        latest.get_or_insert(tool);
        if tool.status == ToolStatus::Running {
            running = Some(tool);
            break;
        }
    }
    let step = running.or(latest)?;
    // The step card's own title, so the shelf and the card say the same.
    Some(
        payload::tool_title(
            step.kind,
            &step.title,
            step.input.as_ref(),
            &step.input_text,
        )
        .text(),
    )
}

/// Fills one docked sub-agent's row: its brand, whether it runs or how it ended, its task, and
/// what it does now.
fn fill(row: &ShelfRow, agent: &SubagentSummary, now: Option<&str>) {
    super::subagents::set_brand(&row.mark, agent.driver);
    row.set_running(agent.running(), agent.state_label());
    let activity = agent.activity.as_deref().or(now);
    let now = activity
        .map(|s| format!("Last reported activity: {s}"))
        .unwrap_or_else(|| "Activity unavailable".to_owned());
    row.describe(
        &format!(
            "{}\n{}\n{}\n{now}\nOpen this worker",
            agent.name,
            super::subagents::identity_text(agent),
            agent.task_text()
        ),
        &agent.name,
        &format!("{}; {}; {now}", agent.state_label(), agent.task_text()),
    );
    row.set_text(&agent.name, agent.task_text(), &now);
}

pub struct SubagentShelf {
    shelf: Shelf,
    /// The docked sub-agents behind the rows, in order, and their rows.
    rows: RefCell<Vec<(String, ShelfRow)>>,
    /// Every sub-agent seen running since the last prompt or "Clear finished": what stays docked
    /// after it finishes.
    docked: RefCell<HashSet<String>>,
    /// "Clear finished": shown while a finished sub-agent is docked.
    clear: gtk4::Button,
}

impl SubagentShelf {
    /// `open` is called with a sub-agent's item id when its row is chosen; `cleared` after
    /// "Clear finished" emptied the dock, to redraw it.
    pub fn new(open: impl Fn(&str) + 'static, cleared: impl Fn() + 'static) -> Rc<Self> {
        let clear = gtk4::Button::builder()
            .label("Clear finished")
            .halign(gtk4::Align::End)
            .css_classes(["flat", "shelf-action"])
            .tooltip_text("Take the finished workers off the dock")
            .visible(false)
            .build();
        let shelf = Shelf::new(
            crate::icons::SUBAGENT_ICON,
            "what the sub-agents are doing",
            Some(&clear),
        );
        shelf.fold.card.add_css_class("subagent-shelf");
        let this = Rc::new(Self {
            shelf,
            rows: RefCell::new(Vec::new()),
            docked: RefCell::new(HashSet::new()),
            clear,
        });
        let weak = Rc::downgrade(&this);
        this.clear.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.clear_finished();
                cleared();
            }
        });
        let weak = Rc::downgrade(&this);
        this.shelf.list.connect_row_activated(move |_, row| {
            let Some(this) = weak.upgrade() else { return };
            let id = usize::try_from(row.index())
                .ok()
                .and_then(|i| this.rows.borrow().get(i).map(|(id, _)| id.clone()));
            if let Some(id) = id {
                open(&id);
            }
        });
        this
    }

    /// The shelf, for the stack above the composer.
    pub fn revealer(&self) -> &gtk4::Revealer {
        &self.shelf.revealer
    }

    /// Follows the thread's sub-agents: a row per docked one (every one seen running since the
    /// last prompt or "Clear finished"), with its current step or how it ended; shown while any
    /// is docked. Rows are rebuilt only when the set of docked sub-agents changes.
    pub fn set(&self, model: &Transcript, agents: &[SubagentSummary]) {
        let shown: Vec<&SubagentSummary> = {
            let mut docked = self.docked.borrow_mut();
            docked.extend(agents.iter().filter(|a| a.running()).map(|a| a.id.clone()));
            agents.iter().filter(|a| docked.contains(&a.id)).collect()
        };
        self.shelf.show(!shown.is_empty());
        if shown.is_empty() {
            return;
        }
        let (running, finished, unknown) = counts(shown.iter().copied());
        set_if_changed(&self.shelf.title, &title_text(running, finished, unknown));
        self.clear.set_visible(finished > 0);
        let same = {
            let rows = self.rows.borrow();
            rows.len() == shown.len() && rows.iter().zip(&shown).all(|((id, _), a)| *id == a.id)
        };
        if !same {
            let rows = self.shelf.rebuild(shown.len());
            *self.rows.borrow_mut() = shown.iter().map(|a| a.id.clone()).zip(rows).collect();
        }
        for ((id, row), agent) in self.rows.borrow().iter().zip(&shown) {
            fill(row, agent, current_step(model, id).as_deref());
        }
    }

    /// Takes the finished sub-agents off the dock (a new prompt, or "Clear finished"). Running
    /// ones dock again at the next [`Self::set`]; the caller redraws.
    pub fn clear_finished(&self) {
        self.docked.borrow_mut().clear();
    }

    /// Opens the worker activity disclosure, including deterministic demo scenes.
    pub(crate) fn set_expanded(&self, expanded: bool) {
        self.shelf.set_expanded(expanded);
    }

    /// Whether the shelf shows, whether it is open, and each row's current step (tests).
    #[cfg(test)]
    pub fn state(&self) -> (bool, bool, Vec<String>) {
        (
            self.shelf.shown(),
            self.shelf.expanded(),
            self.rows
                .borrow()
                .iter()
                .map(|(_, r)| r.now.text().to_string())
                .collect(),
        )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use agent_core::adapter::Driver;
    use agent_core::event::{Envelope, ItemKind};

    #[test]
    #[ignore = "native GTK; run alone in an isolated display"]
    fn native_unknown_worker_has_no_clear_finished_button() {
        gtk4::init().expect("GTK init");
        let t = transcript(&[subagent("s1", "Explore")]);
        let shelf = SubagentShelf::new(|_| {}, || {});
        let mut workers = t.subagents();
        shelf.set(&t, &workers);
        workers[0].lifecycle = Some(agent_core::event::WorkerState::Unknown);
        shelf.set(&t, &workers);
        assert_eq!(shelf.shelf.title.text(), "1 worker unknown");
        assert!(!shelf.clear.property::<bool>("visible"));
        assert!(shelf.state().0);
    }

    #[test]
    fn titles_count_the_running_ones() {
        assert_eq!(title_text(1, 0, 0), "1 worker active");
        assert_eq!(title_text(3, 0, 0), "3 workers active");
        assert_eq!(title_text(0, 1, 0), "1 worker finished");
        assert_eq!(title_text(0, 2, 0), "2 workers finished");
        assert_eq!(title_text(1, 2, 0), "1 worker active · 2 finished");
    }

    #[test]
    fn an_unknown_worker_is_not_called_finished() {
        let t = transcript(&[subagent("s1", "Explore")]);
        let mut worker = t.subagents().remove(0);
        worker.lifecycle = Some(agent_core::event::WorkerState::Unknown);
        let (running, finished, unknown) = counts([&worker]);
        assert_eq!((running, finished, unknown), (0, 0, 1));
        assert_eq!(title_text(running, finished, unknown), "1 worker unknown");
        assert_eq!(
            title_text(2, 1, 3),
            "2 workers active · 1 finished · 3 unknown"
        );
    }

    fn start(
        id: &str,
        kind: ItemKind,
        title: &str,
        input: serde_json::Value,
        parent: Option<&str>,
    ) -> Envelope {
        Envelope::item_started(id, kind, title, Some(input), parent)
    }

    fn done(id: &str) -> Envelope {
        Envelope::item_completed(id, agent_core::event::ItemStatus::Completed, None, None)
    }

    fn subagent(id: &str, name: &str) -> Envelope {
        start(
            id,
            ItemKind::Subagent,
            "Task",
            serde_json::json!({"subagent_type": name, "description": "Find the writers"}),
            None,
        )
    }

    fn command(id: &str, parent: &str, cmd: &str) -> Envelope {
        start(
            id,
            ItemKind::Command,
            "Bash",
            serde_json::json!({ "command": cmd }),
            Some(parent),
        )
    }

    fn read(id: &str, parent: &str, path: &str) -> Envelope {
        start(
            id,
            ItemKind::FileRead,
            "Read",
            serde_json::json!({ "file_path": path }),
            Some(parent),
        )
    }

    fn transcript(envs: &[Envelope]) -> Transcript {
        let mut t = Transcript::new();
        for env in envs {
            t.apply(env, Driver::Claude);
        }
        t
    }

    #[test]
    fn current_step_prefers_the_running_one_then_the_latest() {
        // A command still running, then a read that already finished: the command is what the
        // sub-agent is doing.
        let t = transcript(&[
            subagent("s1", "Explore"),
            command("c1", "s1", "cargo test"),
            read("r1", "s1", "src/config.rs"),
            done("r1"),
        ]);
        assert_eq!(current_step(&t, "s1").as_deref(), Some("Run cargo test"));

        // Everything finished: the newest step, a tool other than a command by title + target.
        let t = transcript(&[
            subagent("s1", "Explore"),
            command("c1", "s1", "cargo test"),
            done("c1"),
            read("r1", "s1", "src/config.rs"),
            done("r1"),
        ]);
        assert_eq!(
            current_step(&t, "s1").as_deref(),
            Some("Read src/config.rs")
        );

        // The shelf line is exactly the step card's title (one grammar).
        let step = match &t.get("r1").expect("step").body {
            Body::Tool(tool) => tool.clone(),
            other => panic!("{other:?}"),
        };
        let (action, target) = payload::card_title_and_summary(
            step.kind,
            &step.title,
            step.input.as_ref(),
            &step.input_text,
        );
        assert_eq!(current_step(&t, "s1"), Some(format!("{action} {target}")));

        // No step yet, or no such sub-agent.
        let t = transcript(&[subagent("s1", "Explore")]);
        assert_eq!(current_step(&t, "s1"), None);
        assert_eq!(current_step(&t, "nope"), None);
    }

    /// Shown, collapsed, while a sub-agent runs; a row per running one with its current step,
    /// rebuilt as the set changes; a row opens its sub-agent (and that may update the shelf
    /// re-entrantly); gone once none runs. Needs GTK (run from `chat::view::tests::ui_checks`).
    pub(crate) fn shelf_follows_running_subagents() {
        let opened = Rc::new(RefCell::new(Vec::<String>::new()));
        let shelf_slot: Rc<RefCell<Option<Rc<SubagentShelf>>>> = Rc::default();
        let t = transcript(&[
            subagent("s1", "Explore"),
            command("c1", "s1", "rg write_atomic"),
        ]);
        let cleared = Rc::new(std::cell::Cell::new(0));
        let shelf = SubagentShelf::new(
            {
                let (opened, shelf_slot) = (opened.clone(), shelf_slot.clone());
                let t = transcript(&[subagent("s1", "Explore")]);
                move |id| {
                    opened.borrow_mut().push(id.to_owned());
                    // Opening runs the view's refresh, which updates the shelf from in here.
                    if let Some(shelf) = shelf_slot.borrow().as_ref() {
                        shelf.set(&t, &t.subagents());
                    }
                }
            },
            {
                let cleared = cleared.clone();
                move || cleared.set(cleared.get() + 1)
            },
        );
        *shelf_slot.borrow_mut() = Some(shelf.clone());
        // In a window, as in the app: activating a row moves focus, which needs a root.
        let window = gtk4::Window::new();
        window.set_child(Some(shelf.revealer()));

        shelf.set(&t, &t.subagents());
        assert_eq!(
            shelf.state(),
            (
                true,
                false,
                vec!["Last reported activity: Run rg write_atomic".to_owned()]
            ),
            "shown, collapsed by default, with the running command"
        );
        shelf.shelf.fold.toggle.emit_clicked();
        assert!(shelf.state().1, "one click opens it");
        shelf.shelf.fold.toggle.emit_clicked();
        assert!(!shelf.state().1, "and another closes it");

        let row = shelf.shelf.list.row_at_index(0).expect("a row");
        row.emit_by_name::<()>("activate", &[]);
        assert_eq!(*opened.borrow(), ["s1"], "the row opened its sub-agent");

        let two = transcript(&[
            subagent("s1", "Explore"),
            command("c1", "s1", "rg write_atomic"),
            subagent("s2", "general-purpose"),
        ]);
        shelf.set(&two, &two.subagents());
        assert_eq!(shelf.shelf.title.text(), "2 workers active");
        assert_eq!(
            shelf.state().2,
            [
                "Last reported activity: Run rg write_atomic",
                "Activity unavailable"
            ],
            "a row each, in order"
        );

        let finished: Vec<SubagentSummary> = two
            .subagents()
            .into_iter()
            .map(|mut a| {
                a.status = ToolStatus::Completed;
                a
            })
            .collect();
        // Finished, they stay docked (not lost in the history), with how they ended, and the
        // card offers to clear them.
        shelf.set(&two, &finished);
        assert!(shelf.state().0, "still docked once they finish");
        assert_eq!(shelf.shelf.title.text(), "2 workers finished");
        // The `visible` property itself: `is_visible()` also needs every parent shown, and the
        // card is collapsed here.
        assert!(shelf.clear.property::<bool>("visible"));
        let ended: Vec<String> = shelf
            .rows
            .borrow()
            .iter()
            .map(|(_, r)| r.state.text().to_string())
            .collect();
        assert_eq!(ended, ["completed", "completed"]);
        assert!(shelf
            .rows
            .borrow()
            .iter()
            .all(|(_, r)| !r.spinner.property::<bool>("visible")
                && r.state.property::<bool>("visible")));

        // "Clear finished" takes them off and asks the view to redraw; then the card goes.
        shelf.clear.emit_clicked();
        assert_eq!(cleared.get(), 1);
        shelf.set(&two, &finished);
        assert!(!shelf.state().0, "gone once cleared and none runs");

        // A sub-agent that finished before the thread was shown (replayed history) never docks.
        let fresh = SubagentShelf::new(|_| {}, || {});
        fresh.set(&two, &finished);
        assert!(!fresh.state().0, "replayed finished workers are not docked");

        // Clearing keeps a running one: it docks again at the next update.
        shelf.set(&two, &two.subagents());
        shelf.clear_finished();
        shelf.set(&two, &two.subagents());
        assert_eq!(shelf.shelf.title.text(), "2 workers active");
        shelf_slot.borrow_mut().take();
        window.destroy();
    }
}

//! The thread sidebar's context menu (right-click, the Menu key, long press) and its actions:
//! continue a thread in another agent, switch it in place, rename, archive, delete, open its
//! folder, copy its id; plus the Delete key.
//!
//! The menu is data: [`thread_menu`] builds a model, [`to_gio_menu`] turns it into a `gio::Menu`
//! whose items all activate one window action (`win.thread-menu`) with the encoded
//! [`ThreadAction`] as their target. No closure captures a widget, and a disabled entry points at
//! a disabled action (`win.thread-menu-off`), which GTK draws insensitive.
//!
//! Deleting removes the thread's rows from this app's store and nothing else: an agent's own
//! session files are never touched.

use std::rc::Rc;

use agent_core::adapter::Driver;
use agent_core::export::{ExportFormat, ExportOptions};
use gtk4::gio;

use super::threads::{archive_notice, delete_notice, store_job, NewThread, Sidebar, WriteNotice};
use super::*;
use crate::chat::session::build_handoff;
use crate::chat::ChatBackend;
use crate::model_catalog::ModelCatalog;
use crate::window::sidebar_model::{
    chosen_tags, colour_at, colour_choices, colour_index, display_title, menu_agents, tag_choices,
    thread_menu, MenuAgent, MenuEntry, RowKey, TagColour, ThreadAction, ThreadMenuInput,
};

/// The action every enabled menu item activates, with the encoded [`ThreadAction`] as target.
const ACTION: &str = "win.thread-menu";
/// The never-enabled twin that disabled items point at.
const ACTION_OFF: &str = "win.thread-menu-off";

/// A `gio::Menu` for `entries`: sections as sections, submenus as submenus, items as items. A
/// disabled submenu (nothing to hand over, no agent ready) is a greyed-out item: GTK cannot
/// grey a submenu, and an open one that is empty or all greyed offers nothing.
fn to_gio_menu(entries: &[MenuEntry]) -> gio::Menu {
    let menu = gio::Menu::new();
    for entry in entries {
        match entry {
            MenuEntry::Item {
                label,
                action,
                enabled,
            } => {
                let item = gio::MenuItem::new(Some(label), None);
                item.set_action_and_target_value(
                    Some(if *enabled { ACTION } else { ACTION_OFF }),
                    Some(&action.encode().to_variant()),
                );
                menu.append_item(&item);
            }
            // GTK's PopoverMenu draws no icon on a submenu entry (verified in the preview), so
            // agent submenus are plain labels: the brand icons live in the widgets, not here.
            MenuEntry::Submenu {
                label,
                entries,
                enabled: true,
            } => {
                menu.append_submenu(Some(label), &to_gio_menu(entries));
            }
            MenuEntry::Submenu {
                label,
                enabled: false,
                ..
            } => {
                let item = gio::MenuItem::new(Some(label), None);
                // The disabled action takes a string; no choice is encoded, as none can be made.
                item.set_action_and_target_value(Some(ACTION_OFF), Some(&"".to_variant()));
                menu.append_item(&item);
            }
            MenuEntry::Section(entries) => menu.append_section(None, &to_gio_menu(entries)),
        }
    }
    menu
}

impl AgentTerminalWindow {
    /// Registers the menu's actions on the window. Called once, from the shell's setup.
    pub(super) fn setup_thread_menu_actions(&self) {
        let obj = self.obj();
        let run = gio::SimpleAction::new("thread-menu", Some(&String::static_variant_type()));
        run.connect_activate(glib::clone!(
            #[weak]
            obj,
            move |_, target| {
                let action = target
                    .and_then(|t| t.get::<String>())
                    .and_then(|s| ThreadAction::decode(&s));
                if let Some(action) = action {
                    obj.imp().run_thread_action(action);
                }
            }
        ));
        obj.add_action(&run);
        let off = gio::SimpleAction::new("thread-menu-off", Some(&String::static_variant_type()));
        off.set_enabled(false);
        obj.add_action(&off);
    }

    /// Right-click, long press, the Menu key (and Shift+F10) open the menu of the row they hit;
    /// Delete asks to delete the selected thread.
    pub(super) fn install_thread_menu_triggers(&self, sidebar: &Rc<Sidebar>) {
        let obj = self.obj();
        let list = sidebar.list.clone();

        let key_of_row = |sidebar: &Rc<Sidebar>, row: &gtk4::ListBoxRow| -> Option<RowKey> {
            usize::try_from(row.index())
                .ok()
                .and_then(|i| sidebar.keys.borrow().get(i).cloned().flatten())
        };

        // Pointer: where the click landed, in the list's coordinates.
        let at_point = {
            let weak_sidebar = Rc::downgrade(sidebar);
            glib::clone!(
                #[weak]
                obj,
                #[weak]
                list,
                move |x: f64, y: f64| {
                    let Some(sidebar) = weak_sidebar.upgrade() else {
                        return;
                    };
                    let Some(row) = list.row_at_y(y as i32) else {
                        return;
                    };
                    let Some(RowKey::Thread(thread)) = key_of_row(&sidebar, &row) else {
                        return;
                    };
                    obj.imp().open_thread_menu(thread, &row, Some((x, y)));
                }
            )
        };
        let at_point = Rc::new(at_point);

        let click = gtk4::GestureClick::new();
        click.set_button(3);
        click.connect_pressed({
            let at_point = at_point.clone();
            move |gesture, _, x, y| {
                gesture.set_state(gtk4::EventSequenceState::Claimed);
                at_point(x, y);
            }
        });
        list.add_controller(click);

        let long_press = gtk4::GestureLongPress::new();
        long_press.set_touch_only(true);
        long_press.connect_pressed({
            let at_point = at_point.clone();
            move |gesture, x, y| {
                gesture.set_state(gtk4::EventSequenceState::Claimed);
                at_point(x, y);
            }
        });
        list.add_controller(long_press);

        // Keyboard: the selected row.
        let keys = gtk4::EventControllerKey::new();
        let weak_sidebar = Rc::downgrade(sidebar);
        keys.connect_key_pressed(glib::clone!(
            #[weak]
            obj,
            #[weak]
            list,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, keyval, _, state| {
                use gtk4::gdk::Key;
                let menu_key = keyval == Key::Menu
                    || (keyval == Key::F10 && state.contains(gtk4::gdk::ModifierType::SHIFT_MASK));
                let delete = matches!(keyval, Key::Delete | Key::KP_Delete);
                // Alt+Shift+Up/Down moves a pinned row along the Pinned section.
                let reorder = match keyval {
                    Key::Up => Some(-1i8),
                    Key::Down => Some(1),
                    _ => None,
                }
                .filter(|_| {
                    state.contains(
                        gtk4::gdk::ModifierType::ALT_MASK | gtk4::gdk::ModifierType::SHIFT_MASK,
                    ) && !state.contains(gtk4::gdk::ModifierType::CONTROL_MASK)
                });
                if !menu_key && !delete && reorder.is_none() {
                    return glib::Propagation::Proceed;
                }
                let Some(sidebar) = weak_sidebar.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                let Some(row) = list.selected_row() else {
                    return glib::Propagation::Proceed;
                };
                let Some(RowKey::Thread(thread)) = key_of_row(&sidebar, &row) else {
                    return glib::Propagation::Proceed;
                };
                if let Some(delta) = reorder {
                    // Only a pinned row moves; on any other the keys stay the list's own.
                    if !obj
                        .imp()
                        .summary_of(&thread)
                        .is_some_and(|s| s.pin_rank.is_some())
                    {
                        return glib::Propagation::Proceed;
                    }
                    obj.imp().change_pin(&thread, PinChange::Move(delta));
                } else if delete {
                    obj.imp().confirm_delete(&thread);
                } else {
                    obj.imp().open_thread_menu(thread, &row, None);
                }
                glib::Propagation::Stop
            }
        ));
        list.add_controller(keys);
    }

    /// The agents the menu offers: enabled and installed, each with its catalogue models.
    fn menu_agents(&self) -> Vec<MenuAgent> {
        menu_agents(&ModelCatalog::shared().models(), |d| self.agent_usable(d))
    }

    /// Pops the thread's menu up by `row`: at `at` (the pointer, in the list's coordinates), else
    /// below the row. Whether the thread has any message is read off the main thread first.
    ///
    /// The popover hangs off the list, never the row: the sidebar rebuilds its rows on any state
    /// change (focus moving to the popover is one), and a row finalized under an open menu took
    /// the menu with it.
    pub(super) fn open_thread_menu(
        &self,
        thread: String,
        row: &gtk4::ListBoxRow,
        at: Option<(f64, f64)>,
    ) {
        let Some(list) = self.sidebar.borrow().as_ref().map(|s| s.list.clone()) else {
            return;
        };
        // Where to point, fixed now: the row may be gone by the time the menu opens.
        let rect = match at {
            Some((x, y)) => gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1),
            None => match row.compute_bounds(&list) {
                Some(b) => gtk4::gdk::Rectangle::new(
                    b.x() as i32,
                    b.y() as i32,
                    b.width() as i32,
                    b.height() as i32,
                ),
                None => return,
            },
        };
        let obj = self.obj().downgrade();
        glib::MainContext::default().spawn_local(async move {
            let (has_messages, forks) = store_job({
                let thread = thread.clone();
                move |store| {
                    (
                        store.has_messages(&thread).unwrap_or(false),
                        store
                            .fork_pairings(&thread)
                            .is_ok_and(|pairings| !pairings.is_empty()),
                    )
                }
            })
            .await
            .unwrap_or((false, false));
            let Some(obj) = obj.upgrade() else { return };
            let imp = obj.imp();
            let Some(summary) = imp.summary_of(&thread) else {
                return; // deleted meanwhile
            };
            let input = ThreadMenuInput {
                thread,
                archived: summary.archived,
                pinned: summary.pin_rank.is_some(),
                tagged: !summary.tags.is_empty(),
                has_messages,
                forks,
                agents: imp.menu_agents(),
            };
            let menu = to_gio_menu(&thread_menu(&input));
            let popover = gtk4::PopoverMenu::from_model(Some(&menu));
            popover.set_tooltip_text(Some("Switch changes the agent in this thread and stops its current turn. Continue, Fork and Compact create a new thread with a handoff."));
            popover.set_has_arrow(false);
            popover.set_halign(gtk4::Align::Start);
            popover.set_parent(&list);
            popover.set_pointing_to(Some(&rect));
            // Unparented once closed (not inside the signal itself).
            popover.connect_closed(|popover| {
                let popover = popover.clone();
                glib::idle_add_local_once(move || popover.unparent());
            });
            popover.popup();
        });
    }

    /// Carries out a menu choice.
    pub(super) fn run_thread_action(&self, action: ThreadAction) {
        match action {
            ThreadAction::ContinueIn {
                thread,
                driver,
                model,
                effort,
            } => self.continue_thread_in(thread, driver, model, effort),
            ThreadAction::SwitchTo {
                thread,
                driver,
                model,
                effort,
            } => self.switch_thread_to(&thread, driver, model, effort),
            ThreadAction::Fork(ref thread) | ThreadAction::Compact(ref thread) => {
                let compact = matches!(action, ThreadAction::Compact(_));
                if let Some(summary) = self.summary_of(thread) {
                    if let Some(driver) = self.thread_driver(thread, &summary) {
                        self.fork_thread(thread, driver, Some(summary.cwd), compact);
                    }
                }
            }
            ThreadAction::CompareForks(thread) => self.compare_with_fork(&thread),
            ThreadAction::Rename(thread) => self.prompt_rename(&thread),
            ThreadAction::Pin(thread) => self.change_pin(&thread, PinChange::Pin { at: None }),
            ThreadAction::Unpin(thread) => self.change_pin(&thread, PinChange::Unpin),
            ThreadAction::PinMove(thread, delta) => {
                self.change_pin(&thread, PinChange::Move(delta));
            }
            ThreadAction::EditTags(thread) => self.prompt_tags(&thread),
            ThreadAction::ClearTags(thread) => self.clear_tags(&thread),
            ThreadAction::Archive(thread) => self.set_thread_archived(&thread, true),
            ThreadAction::Unarchive(thread) => self.set_thread_archived(&thread, false),
            ThreadAction::Delete(thread) => self.confirm_delete(&thread),
            ThreadAction::OpenFolder(thread) => self.open_thread_folder(&thread),
            ThreadAction::Export(thread) => self.confirm_export(&thread),
            ThreadAction::ReviewEnvironment(thread) => {
                if let Some(summary) = self.summary_of(&thread) {
                    let driver = self.thread_driver(&thread, &summary);
                    self.confirm_environment_review(summary.cwd, driver);
                }
            }
            ThreadAction::CopyId(thread) => {
                self.obj().clipboard().set_text(&thread);
                self.show_toast("Copied the thread id");
            }
        }
    }

    /// The agent `thread` runs on: its open session's, else the one its `summary` recorded.
    fn thread_driver(
        &self,
        thread: &str,
        summary: &agent_kit::store::ThreadSummary,
    ) -> Option<Driver> {
        self.slot_of(thread).map(|slot| slot.driver()).or_else(|| {
            summary
                .driver
                .as_deref()
                .and_then(crate::window::sidebar_model::parse_driver)
        })
    }

    /// What the sidebar calls the thread.
    pub(super) fn thread_title_of(&self, thread: &str) -> String {
        let open = self
            .with_chat(thread, |c| c.title.clone())
            .filter(|t| !t.is_empty());
        let title = open
            .or_else(|| self.summary_of(thread).map(|s| s.title))
            .unwrap_or_default();
        display_title(&title).to_owned()
    }

    // ---- continue / switch ----

    /// A NEW thread in the same folder on `driver`/`model`/`effort`, whose first message carries a
    /// budgeted, redacted handoff of this thread (the same path Fork uses). This thread is left
    /// as it is.
    fn continue_thread_in(
        &self,
        thread: String,
        driver: Driver,
        model: Option<String>,
        effort: Option<String>,
    ) {
        let Some(summary) = self.summary_of(&thread) else {
            return;
        };
        let title = self.thread_title_of(&thread);
        // The new thread keeps this one's mode; only a thread with no session yet falls back to
        // the profile's.
        let mode = self
            .slot_of(&thread)
            .and_then(|s| s.get())
            .map(|s| s.wanted_mode());
        let obj = self.obj().downgrade();
        glib::MainContext::default().spawn_local(async move {
            let loaded = store_job({
                let thread = thread.clone();
                move |store| store.transcript_messages(&thread)
            })
            .await;
            let Some(obj) = obj.upgrade() else { return };
            let imp = obj.imp();
            let messages = match loaded {
                Some(Ok(messages)) => messages,
                Some(Err(e)) => {
                    present_message(&obj, "Cannot Continue the Thread", &e.to_string());
                    return;
                }
                None => {
                    imp.show_toast("Cannot read the thread");
                    return;
                }
            };
            let handoff = build_handoff(&messages, &format!("thread {thread}"));
            if handoff.carried == 0 {
                imp.show_toast("Nothing to hand over yet: send a message first");
                return;
            }
            imp.create_chat_thread(NewThread {
                dir: Some(summary.cwd),
                model,
                effort,
                title: Some(continued_title(&title, driver)),
                handoff: Some((handoff, format!("“{title}”"))),
                mode,
                ..NewThread::new(driver)
            });
        });
    }

    /// This thread, in place, on `driver`/`model`/`effort` (the header picker's switch). A thread
    /// that is not open is opened first. One with no session is switched as soon as its session
    /// or build allows, or moved to `driver` at once when its own agent cannot start one
    /// ([`Self::switch_in_place_with`]); it is never forked.
    fn switch_thread_to(
        &self,
        thread: &str,
        driver: Driver,
        model: Option<String>,
        effort: Option<String>,
    ) {
        if let Some(owner) = super::threads::thread_owner(thread).filter(|w| w != &*self.obj()) {
            owner.imp().switch_thread_to(thread, driver, model, effort);
            owner.present();
            return;
        }
        let Some(_page) = self.open_thread(thread, true) else {
            return;
        };
        let Some(slot) = self.slot_of(thread) else {
            return;
        };
        if slot.get().is_none() {
            self.switch_in_place_with(thread, driver, model, effort);
            return;
        }
        // Mid-turn, ask first: the switch stops the turn.
        let id = thread.to_owned();
        // Through the window's own switch, as the header's picker goes: a delegation root's record
        // moves with it, so the supervisor never judges the new agent as the old one.
        self.when_not_busy(thread, move |imp| {
            if imp.slot_of(&id).and_then(|s| s.get()).is_some() {
                imp.switch_agent_confirmed(&id, driver, model.clone(), effort.clone(), None);
            }
        });
    }

    // ---- rename ----

    fn prompt_rename(&self, thread: &str) {
        let current = self.thread_title_of(thread);
        let entry = gtk4::Entry::builder()
            .text(&current)
            .activates_default(true)
            .build();
        let dialog = confirm_dialog(
            "Rename Thread",
            "",
            ("cancel", "Cancel"),
            ("rename", "Rename"),
            adw::ResponseAppearance::Suggested,
            true,
        );
        dialog.set_extra_child(Some(&entry));
        let thread = thread.to_owned();
        self.ask(dialog, "rename", move |imp, rename| {
            if !rename {
                return;
            }
            let title = entry.text().trim().to_owned();
            if !title.is_empty() {
                imp.rename_thread_to(&thread, &title);
            }
        });
    }

    fn rename_thread_to(&self, thread: &str, title: &str) {
        let (id, stored) = (thread.to_owned(), title.to_owned());
        self.write_store("rename the thread", move |s| s.rename_thread(&id, &stored));
        self.with_chat_mut(thread, |chat| chat.title = title.to_owned());
        if let Some(page) = self.page_of_thread(thread) {
            page.set_title(title);
            if self.selected_row_key() == Some(RowKey::Thread(thread.to_owned())) {
                if let Some(w) = self.window_title.borrow().as_ref() {
                    w.set_title(title);
                }
            }
        }
        self.refresh_sidebar();
    }

    // ---- pin ----

    /// Pins, unpins or moves a thread through the store writer, then says so with an Undo that
    /// puts it back (an unpinned thread returns to the place it had). Pinning never touches
    /// `updated_at`, so no folder is reordered by it.
    pub(super) fn change_pin(&self, thread: &str, change: PinChange) {
        let (id, undo_id) = (thread.to_owned(), thread.to_owned());
        let what = match change {
            PinChange::Pin { .. } => "pin the thread",
            PinChange::Unpin => "unpin the thread",
            PinChange::Move(_) => "move the pinned thread",
        };
        self.write_store_then(
            what,
            move |s| change.apply(s, &id),
            move |imp, outcome| match outcome {
                // A move that changed nothing (already at that end) has nothing to say or undo.
                Some(Ok(receipt)) if receipt.before == receipt.after && change.is_move() => {}
                Some(Ok(receipt)) => {
                    let undo = change.undo(receipt);
                    imp.action_toast(
                        change.done(),
                        "Undo",
                        10,
                        adw::ToastPriority::Normal,
                        move |imp| imp.change_pin(&undo_id, undo),
                    );
                }
                failed => {
                    let error = failed.and_then(Result::err).map_or_else(
                        || "the thread store did not answer in time".to_owned(),
                        |e| e.to_string(),
                    );
                    present_message(
                        &imp.obj(),
                        "Could Not Change the Pin",
                        &format!("The thread could not be {}: {error}", change.past()),
                    );
                }
            },
        );
    }

    // ---- tags ----

    /// Opens the Tags dialog for `thread` once its tags, every tag a thread holds and their
    /// colours are read, together and off the main thread. A thread gone meanwhile opens none.
    fn prompt_tags(&self, thread: &str) {
        let obj = self.obj().downgrade();
        let thread = thread.to_owned();
        glib::MainContext::default().spawn_local(async move {
            let read = store_job({
                let thread = thread.clone();
                move |store| -> agent_kit::store::Result<_> {
                    let Some(summary) = store.thread_summary(&thread)? else {
                        return Ok(None);
                    };
                    Ok(Some((summary, store.list_tags()?, store.tag_colours()?)))
                }
            })
            .await;
            let Some(obj) = obj.upgrade() else { return };
            match read {
                Some(Ok(Some((summary, known, colours)))) => {
                    obj.imp()
                        .show_tags_dialog(thread, summary, &known, &colours);
                }
                Some(Ok(None)) => {} // deleted meanwhile
                Some(Err(e)) => {
                    warn!("Cannot read the tags: {e}");
                    obj.imp().show_toast("Cannot read the tags");
                }
                None => obj.imp().show_toast("Cannot read the tags"),
            }
        });
    }

    /// The Tags dialog: every tag offered as a row to tick, with its colour, and an entry for new
    /// ones. What would be refused (a bad tag, too many) says why inline and keeps Save off.
    /// Saving writes only the colours whose dropdown was changed, so a colour set meanwhile
    /// elsewhere is kept.
    fn show_tags_dialog(
        &self,
        thread: String,
        summary: agent_kit::store::ThreadSummary,
        known: &[(String, usize)],
        colours: &std::collections::BTreeMap<String, TagColour>,
    ) {
        let current = summary.tags;
        let title = self
            .with_chat(&thread, |c| c.title.clone())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| display_title(&summary.title).to_owned());
        let dialog = confirm_dialog(
            "Thread Tags",
            &format!(
                "Tick the tags of “{title}”, or type new ones separated by spaces or commas. A \
                 thread has at most {} tags.",
                agent_core::thread_tag::MAX_TAGS
            ),
            ("cancel", "Cancel"),
            ("save", "Save"),
            adw::ResponseAppearance::Suggested,
            true,
        );
        let content = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .spacing(12)
            .build();
        let entry = gtk4::Entry::builder()
            .placeholder_text("New tags")
            .activates_default(true)
            .build();
        entry.update_property(&[gtk4::accessible::Property::Label("New tags")]);
        content.append(&entry);
        let problem = gtk4::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .visible(false)
            .css_classes(["error", "caption"])
            .build();
        content.append(&problem);
        let choices = colour_choices();
        // Each row: its tag, its box, its colour dropdown and the colour it was opened with.
        let mut rows: Vec<(String, gtk4::CheckButton, gtk4::DropDown, u32)> = Vec::new();
        let list = gtk4::ListBox::builder()
            .selection_mode(gtk4::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        for (tag, held) in tag_choices(known, &current) {
            let check = gtk4::CheckButton::builder()
                .active(held)
                .valign(gtk4::Align::Center)
                .build();
            check.update_property(&[gtk4::accessible::Property::Label(&format!("#{tag}"))]);
            let colour = gtk4::DropDown::from_strings(&choices);
            let opened_with = colour_index(colours.get(&tag).copied());
            colour.set_selected(opened_with);
            colour.set_valign(gtk4::Align::Center);
            colour.set_tooltip_text(Some("The tag's colour, from the colour scheme"));
            colour.update_property(&[gtk4::accessible::Property::Label(&format!(
                "Colour of #{tag}"
            ))]);
            let count = known.iter().find(|(t, _)| *t == tag).map_or(0, |(_, n)| *n);
            let row = adw::ActionRow::builder()
                .title(format!("#{tag}"))
                .use_markup(false)
                .subtitle(match count {
                    0 | 1 => "1 thread".to_owned(),
                    n => format!("{n} threads"),
                })
                .activatable_widget(&check)
                .build();
            row.add_prefix(&check);
            row.add_suffix(&colour);
            list.append(&row);
            rows.push((tag, check, colour, opened_with));
        }
        if !rows.is_empty() {
            let scroller = gtk4::ScrolledWindow::builder()
                .hscrollbar_policy(gtk4::PolicyType::Never)
                .propagate_natural_height(true)
                .max_content_height(320)
                .child(&list)
                .build();
            content.append(&scroller);
        }
        dialog.set_extra_child(Some(&content));
        let rows = Rc::new(rows);
        let chosen = {
            let (rows, entry) = (rows.clone(), entry.clone());
            move || {
                chosen_tags(
                    rows.iter()
                        .filter(|(_, check, _, _)| check.is_active())
                        .map(|(tag, _, _, _)| tag.as_str()),
                    &entry.text(),
                )
            }
        };
        let chosen = Rc::new(chosen);
        let validate = {
            let (chosen, dialog, problem) = (chosen.clone(), dialog.clone(), problem.clone());
            move || {
                let outcome = chosen();
                if let Err(why) = &outcome {
                    problem.set_text(&why.to_string());
                }
                problem.set_visible(outcome.is_err());
                dialog.set_response_enabled("save", outcome.is_ok());
            }
        };
        let validate = Rc::new(validate);
        entry.connect_changed({
            let validate = validate.clone();
            move |_| validate()
        });
        for (_, check, _, _) in rows.iter() {
            let validate = validate.clone();
            check.connect_toggled(move |_| validate());
        }
        validate();
        self.ask(dialog, "save", move |imp, save| {
            if !save {
                return;
            }
            // Save is off while the tags would be refused; a refusal here is nothing to save.
            let Ok(tags) = chosen() else {
                return;
            };
            let colours = ColourWrite::Set(
                rows.iter()
                    .filter(|(_, _, colour, opened_with)| colour.selected() != *opened_with)
                    .map(|(tag, _, colour, _)| (tag.clone(), colour_at(colour.selected())))
                    .collect(),
            );
            imp.save_tags(&thread, tags, colours, false);
        });
    }

    /// Removes every tag of `thread`, with an Undo that puts them back, with the colours they had
    /// (a tag no other thread has loses its colour with it).
    fn clear_tags(&self, thread: &str) {
        self.save_tags(thread, Vec::new(), ColourWrite::Set(Vec::new()), true);
    }

    /// Writes `thread`'s tags and `colours` through the store writer, in one write. What the
    /// thread had (its tags and their colours) is read in that same write; with `undoable`, a
    /// change from some tags says so with an Undo that writes them back.
    pub(super) fn save_tags(
        &self,
        thread: &str,
        tags: Vec<String>,
        colours: ColourWrite,
        undoable: bool,
    ) {
        let id = thread.to_owned();
        let undo_id = thread.to_owned();
        self.write_store_then(
            "save the thread's tags",
            move |s| s.atomically(|s| colours.write_with_tags(s, &id, &tags)),
            move |imp, outcome| match outcome {
                Some(Ok((before, had))) => {
                    if undoable && !before.is_empty() {
                        imp.action_toast(
                            "Tags cleared",
                            "Undo",
                            10,
                            adw::ToastPriority::Normal,
                            move |imp| {
                                imp.save_tags(
                                    &undo_id,
                                    before.clone(),
                                    ColourWrite::RestoreUnset(had.clone()),
                                    false,
                                );
                            },
                        );
                    }
                }
                failed => {
                    let error = failed.and_then(Result::err).map_or_else(
                        || "the thread store did not answer in time".to_owned(),
                        |e| e.to_string(),
                    );
                    present_message(
                        &imp.obj(),
                        "Could Not Save the Tags",
                        &format!("The thread's tags were not changed: {error}"),
                    );
                }
            },
        );
    }

    // ---- archive / delete ----

    /// Hides (or brings back) a thread. Archiving closes its page; the thread stays in the store
    /// and behind the sidebar's "Show Archived" toggle.
    pub(super) fn set_thread_archived(&self, thread: &str, archived: bool) {
        if let Some(owner) = super::threads::thread_owner(thread).filter(|w| w != &*self.obj()) {
            owner.imp().set_thread_archived(thread, archived);
            return;
        }
        if !archived {
            self.store_archived(thread, false, false);
            return;
        }
        // Archiving ends a running turn and drops an unsent message like closing does: ask
        // first, and do nothing if declined.
        let key = RowKey::Thread(thread.to_owned());
        if let Some(reasons) = self.row_close_reasons(&key) {
            let id = thread.to_owned();
            self.confirm_stop_then(thread, reasons, move |imp| imp.archive_now(&id));
            return;
        }
        self.archive_now(thread);
    }

    /// Archives the thread and closes its page. Its toast says whether that stopped a turn
    /// (busy as the page closes, not when the question was asked).
    fn archive_now(&self, thread: &str) {
        let key = RowKey::Thread(thread.to_owned());
        let stopped = self.row_is_busy(&key);
        self.store_archived(thread, true, stopped);
        // Its page's detach discards nothing: a blank thread archived is never deleted, however
        // the archive's write fares.
        self.close_row_without_discard(thread);
    }

    /// Archives (or brings back) the thread in the store, and says so once the writer has
    /// answered: an archive's toast offers Undo, which brings the thread back into the list
    /// (never its stopped turn).
    fn store_archived(&self, thread: &str, archived: bool, stopped: bool) {
        let id = thread.to_owned();
        let thread = thread.to_owned();
        self.write_store_then(
            if archived {
                "archive the thread"
            } else {
                "restore the thread"
            },
            move |s| s.set_archived(&id, archived),
            move |imp, outcome| {
                let outcome = outcome.map(|r| r.map_err(|e| e.to_string()));
                imp.show_write_notice(archive_notice(archived, stopped, outcome), move |imp| {
                    imp.set_thread_archived(&thread, false);
                });
            },
        );
    }

    /// Shows `notice`: a toast (with Undo running `undo`, when it offers one) or a dialog.
    fn show_write_notice(&self, notice: WriteNotice, undo: impl Fn(&Self) + 'static) {
        match notice {
            WriteNotice::Toast { text, undo: false } => self.show_toast(&text),
            WriteNotice::Toast { text, undo: true } => {
                self.action_toast(&text, "Undo", 10, adw::ToastPriority::Normal, undo);
            }
            WriteNotice::Failure { heading, body } => present_message(&self.obj(), heading, &body),
        }
    }

    /// Asks before deleting: this removes the thread's history from Agent Terminal.
    pub(super) fn confirm_delete(&self, thread: &str) {
        let title = self.thread_title_of(thread);
        let dialog = confirm_dialog(
            "Delete This Thread?",
            &format!(
                "“{title}” and its history in Agent Terminal will be deleted. The agent's own \
                 session files are not touched. This cannot be undone."
            ),
            ("cancel", "Cancel"),
            ("delete", "Delete"),
            adw::ResponseAppearance::Destructive,
            false,
        );
        let thread = thread.to_owned();
        self.ask(dialog, "delete", move |imp, delete| {
            if delete {
                imp.delete_thread_now(&thread);
            }
        });
    }

    /// Closes the thread's page (ending its session) and removes it from the store.
    /// Says so once the writer has answered: "Thread deleted", or what failed.
    pub(super) fn delete_thread_now(&self, thread: &str) {
        // An agent alive in any window (not suspended, not exited), or its live sub-agents:
        // closing its page stops them, whatever the write does.
        let stopped = super::threads::thread_owner(thread)
            .and_then(|w| w.imp().slot_of(thread))
            .and_then(|slot| slot.get())
            .is_some_and(|s| s.status().alive)
            || crate::delegation::supervisor().is_some_and(|s| s.root_has_live_children(thread));
        self.close_row_without_discard(thread);
        let id = thread.to_owned();
        // Already gone (deleted elsewhere meanwhile) is deleted too.
        self.write_store_then(
            "delete the thread",
            move |s| match s.delete_thread(&id) {
                Err(agent_kit::store::StoreError::NotFound(_)) => Ok(()),
                done => done,
            },
            move |imp, outcome| {
                let outcome = outcome.map(|r| r.map_err(|e| e.to_string()));
                if matches!(outcome, Some(Ok(()))) {
                    // Its images go with it, once no other thread refers to them.
                    super::threads::sweep_images();
                }
                imp.show_write_notice(delete_notice(stopped, outcome), |_| {});
            },
        );
    }

    // ---- export ----

    /// Asks what to export (format, reasoning, tool output), then where to save it. A thread that
    /// is gone is a no-op.
    fn confirm_export(&self, thread: &str) {
        if self.summary_of(thread).is_none() {
            return;
        }
        let title = self.thread_title_of(thread);
        let dialog = confirm_dialog(
            "Export Thread",
            "",
            ("cancel", "Cancel"),
            ("export", "Export…"),
            adw::ResponseAppearance::Suggested,
            true,
        );
        dialog.set_body(&format!(
            "Save “{title}” as a file.\n\n{}",
            agent_core::export::REDACTION_NOTE
        ));
        let labels: Vec<&str> = ExportFormat::ALL.iter().map(|f| f.label()).collect();
        let format = adw::ComboRow::builder()
            .title("Format")
            .model(&gtk4::StringList::new(&labels))
            .build();
        let reasoning = adw::SwitchRow::builder()
            .title("Include reasoning")
            .active(false)
            .build();
        let tool_output = adw::SwitchRow::builder()
            .title("Include tool output")
            .active(true)
            .build();
        let list = gtk4::ListBox::builder()
            .selection_mode(gtk4::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        list.append(&format);
        list.append(&reasoning);
        list.append(&tool_output);
        dialog.set_extra_child(Some(&list));
        let thread = thread.to_owned();
        self.ask(dialog, "export", move |imp, export| {
            if !export {
                return;
            }
            let options = ExportOptions {
                format: export_format_at(format.selected()),
                include_reasoning: reasoning.is_active(),
                include_tool_output: tool_output.is_active(),
            };
            imp.pick_export_target(thread, title, options);
        });
    }

    /// Asks where to save, then exports there: the store read on its worker, the render and write
    /// on another thread, so nothing blocks the window.
    fn pick_export_target(&self, thread: String, title: String, options: ExportOptions) {
        let dialog = gtk4::FileDialog::builder()
            .title("Export Thread")
            .accept_label("Export")
            .modal(true)
            .build();
        let today = glib::DateTime::now_local()
            .ok()
            .and_then(|d| d.format("%Y-%m-%d").ok())
            .map(|s| s.to_string())
            .unwrap_or_default();
        dialog.set_initial_name(Some(&export_file_name(&title, &today, options.format)));
        if let Some(docs) =
            glib::user_special_dir(glib::UserDirectory::Documents).filter(|p| p.is_dir())
        {
            dialog.set_initial_folder(Some(&gio::File::for_path(docs)));
        }
        let obj = self.obj().downgrade();
        glib::MainContext::default().spawn_local(async move {
            let Some(window) = obj.upgrade() else { return };
            let Ok(file) = dialog
                .save_future(Some(window.upcast_ref::<gtk4::Window>()))
                .await
            else {
                return; // Cancelled.
            };
            let Some(path) = file.path() else {
                window.imp().show_toast("That place can't be written to");
                return;
            };
            // Progress only when the work is slow.
            let done = Rc::new(std::cell::Cell::new(false));
            glib::timeout_add_local_once(std::time::Duration::from_millis(300), {
                let (done, obj) = (done.clone(), obj.clone());
                move || {
                    if let (false, Some(window)) = (done.get(), obj.upgrade()) {
                        window.imp().show_toast("Exporting…");
                    }
                }
            });
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let outcome = export_to(thread, options, path).await;
            done.set(true);
            let Some(window) = obj.upgrade() else { return };
            match outcome {
                Ok(()) => window.imp().show_toast(&format!("Exported to {name}")),
                Err(why) => window.imp().show_toast(&why),
            }
        });
    }

    // ---- folder ----

    fn open_thread_folder(&self, thread: &str) {
        let Some(summary) = self.summary_of(thread) else {
            return;
        };
        let folder = gio::File::for_path(&summary.cwd);
        if !std::path::Path::new(&summary.cwd).is_dir() {
            self.show_toast("That folder no longer exists");
            return;
        }
        gtk4::FileLauncher::new(Some(&folder)).launch(
            Some(self.obj().upcast_ref::<gtk4::Window>()),
            None::<&gio::Cancellable>,
            |result| {
                if let Err(e) = result {
                    warn!("Cannot open the folder: {e}");
                }
            },
        );
    }
}

/// The format at dropdown position `index`; the first (Markdown) for one out of range.
fn export_format_at(index: u32) -> ExportFormat {
    ExportFormat::ALL
        .get(index as usize)
        .copied()
        .unwrap_or_default()
}

/// The save dialog's first name: `<slug-of-title>-<yyyy-mm-dd>.<ext>`.
fn export_file_name(title: &str, date: &str, format: ExportFormat) -> String {
    let slug = agent_core::export::file_slug(title);
    let ext = format.extension();
    if date.is_empty() {
        format!("{slug}.{ext}")
    } else {
        format!("{slug}-{date}.{ext}")
    }
}

/// Reads `thread` on the store's worker, then renders, redacts and writes it to `path` on
/// another. The error is the text for a toast. Nothing is written when the read fails or the
/// store's writer did not answer.
async fn export_to(
    thread: String,
    options: ExportOptions,
    path: std::path::PathBuf,
) -> Result<(), String> {
    let version = env!("CARGO_PKG_VERSION");
    let source =
        store_job(move |store| agent_kit::export::read_thread(store, &thread, &options, version))
            .await
            .ok_or_else(|| "Cannot read the thread".to_owned())?
            .map_err(|e| e.to_string())?;
    super::threads::off_main("The export", move || {
        agent_kit::export::write_export(&source, &options, &path).map_err(|e| e.to_string())
    })
    .await
}

/// Tags with their colours.
pub(in crate::window) type TagColours = Vec<(String, TagColour)>;

/// The tag colours a tags write sets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::window) enum ColourWrite {
    /// Each of these, as given (`None` clears it): the dropdowns the user changed.
    Set(Vec<(String, Option<TagColour>)>),
    /// Each of these only where its tag has no colour when the write runs: an Undo puts back
    /// what was lost, never over a colour chosen since.
    RestoreUnset(Vec<(String, TagColour)>),
}

impl ColourWrite {
    /// On the writer: sets `thread`'s tags to `tags`, then these colours. Returns the tags the
    /// thread had and the colours those tags had, read in the same write.
    pub(in crate::window) fn write_with_tags(
        &self,
        store: &agent_kit::store::Store,
        thread: &str,
        tags: &[String],
    ) -> agent_kit::store::Result<(Vec<String>, TagColours)> {
        let before = store
            .thread_summary(thread)?
            .ok_or_else(|| agent_kit::store::StoreError::NotFound(thread.to_owned()))?
            .tags;
        let now = store.tag_colours()?;
        let had: Vec<(String, TagColour)> = before
            .iter()
            .filter_map(|t| now.get(t).map(|c| (t.clone(), *c)))
            .collect();
        store.set_tags(thread, tags)?;
        let wanted: Vec<(String, Option<TagColour>)> = match self {
            Self::Set(colours) => colours.clone(),
            Self::RestoreUnset(colours) => {
                let now = store.tag_colours()?;
                colours
                    .iter()
                    .filter(|(t, _)| !now.contains_key(t))
                    .map(|(t, c)| (t.clone(), Some(*c)))
                    .collect()
            }
        };
        if !wanted.is_empty() {
            store.set_tag_colours(&wanted)?;
        }
        Ok((before, had))
    }
}

/// What a pin action does to the Pinned section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::window) enum PinChange {
    /// Pins the thread: at the end of the Pinned section, or back at place `at` (an undo).
    Pin {
        at: Option<i64>,
    },
    Unpin,
    /// Up (negative) or down (positive) the Pinned section.
    Move(i8),
}

/// The place a thread had before a pin change and the one it has now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::window) struct PinReceipt {
    pub before: Option<i64>,
    pub after: Option<i64>,
}

impl PinChange {
    /// The store write.
    pub(in crate::window) fn apply(
        self,
        store: &agent_kit::store::Store,
        thread: &str,
    ) -> agent_kit::store::Result<PinReceipt> {
        let rank = |store: &agent_kit::store::Store| -> agent_kit::store::Result<Option<i64>> {
            Ok(store.thread_summary(thread)?.and_then(|t| t.pin_rank))
        };
        // Read on the writer, in order with the writes: never from the sidebar's cached list.
        let before = rank(store)?;
        match self {
            Self::Pin { at } => {
                store.set_pinned(thread, true)?;
                if let Some(want) = at {
                    // From where the thread is now: it may have been pinned already.
                    if let Some(now) = rank(store)? {
                        store.move_pin(thread, want - now)?;
                    }
                }
            }
            Self::Unpin => store.set_pinned(thread, false)?,
            Self::Move(delta) => store.move_pin(thread, i64::from(delta))?,
        }
        Ok(PinReceipt {
            before,
            after: rank(store)?,
        })
    }

    fn is_move(self) -> bool {
        matches!(self, Self::Move(_))
    }

    /// The change that reverses this one, given what the write found.
    pub(in crate::window) fn undo(self, receipt: PinReceipt) -> Self {
        match self {
            Self::Pin { .. } => Self::Unpin,
            Self::Unpin => Self::Pin { at: receipt.before },
            Self::Move(_) => {
                let back = receipt.before.unwrap_or(0) - receipt.after.unwrap_or(0);
                Self::Move(i8::try_from(back).unwrap_or(if back < 0 { i8::MIN } else { i8::MAX }))
            }
        }
    }

    /// The toast once it has landed.
    fn done(self) -> &'static str {
        match self {
            Self::Pin { .. } => "Thread pinned",
            Self::Unpin => "Thread unpinned",
            Self::Move(d) if d < 0 => "Moved up",
            Self::Move(_) => "Moved down",
        }
    }

    /// What failed, for the failure dialog.
    fn past(self) -> &'static str {
        match self {
            Self::Pin { .. } => "pinned",
            Self::Unpin => "unpinned",
            Self::Move(_) => "moved",
        }
    }
}

/// `<source title> (→ <Agent>)`: the title of a thread continued in another agent.
fn continued_title(source: &str, driver: Driver) -> String {
    format!("{source} (→ {})", driver.info().label)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    use crate::availability::Availability;
    use crate::window::sidebar_model::MenuAgent;

    #[test]
    fn an_undone_unpin_moves_from_where_the_thread_is_now() {
        let store = agent_kit::store::Store::open_in_memory().expect("open");
        let ids: Vec<String> = ["x", "y", "z"]
            .iter()
            .map(|n| store.create_thread("/w", Some(n)).expect("thread"))
            .collect();
        for id in &ids {
            store.set_pinned(id, true).expect("pin");
        }
        // x was re-pinned meanwhile (it is first), and the undo wants it second.
        let receipt = PinChange::Pin { at: Some(1) }
            .apply(&store, &ids[0])
            .expect("apply");
        assert_eq!(receipt.before, Some(0));
        let rank = |id: &str| store.thread_summary(id).unwrap().unwrap().pin_rank;
        assert_eq!(
            [rank(&ids[1]), rank(&ids[0]), rank(&ids[2])],
            [Some(0), Some(1), Some(2)]
        );
    }

    #[test]
    fn a_tags_write_never_reverts_a_colour_set_meanwhile() {
        let store = agent_kit::store::Store::open_in_memory().expect("open");
        let a = store.create_thread("/w", Some("a")).expect("a");
        let b = store.create_thread("/w", Some("b")).expect("b");
        let tags = |v: &[&str]| v.iter().map(|t| (*t).to_owned()).collect::<Vec<_>>();
        store.set_tags(&a, &["review", "ops"]).expect("tag");
        store.set_tags(&b, &["review"]).expect("tag");
        store
            .set_tag_colours(&[
                ("review", Some(TagColour::Warning)),
                ("ops", Some(TagColour::Accent)),
            ])
            .expect("colour");

        // Clear Tags on a reads what it had in the same write.
        let (before, had) = ColourWrite::Set(Vec::new())
            .write_with_tags(&store, &a, &[])
            .expect("clear");
        assert_eq!(before, ["ops", "review"]);
        assert_eq!(
            had,
            [
                ("ops".to_owned(), TagColour::Accent),
                ("review".to_owned(), TagColour::Warning)
            ]
        );
        // Meanwhile another thread's dialog recolours review (b still holds it).
        store
            .set_tag_colours(&[("review", Some(TagColour::Danger))])
            .expect("recolour");
        // The Undo puts back ops (which lost its colour) but never over review's new one.
        ColourWrite::RestoreUnset(had)
            .write_with_tags(&store, &a, &before)
            .expect("undo");
        let colours = store.tag_colours().expect("colours");
        assert_eq!(colours.get("ops"), Some(&TagColour::Accent));
        assert_eq!(colours.get("review"), Some(&TagColour::Danger));
        assert_eq!(store.thread_summary(&a).unwrap().unwrap().tags, before);

        // A dialog's Save writes only the colours it changed.
        ColourWrite::Set(vec![("ops".into(), None)])
            .write_with_tags(&store, &b, &tags(&["review", "ops"]))
            .expect("save");
        let colours = store.tag_colours().expect("colours");
        assert_eq!(colours.get("ops"), None, "the one changed");
        assert_eq!(
            colours.get("review"),
            Some(&TagColour::Danger),
            "left alone"
        );
        assert!(matches!(
            ColourWrite::Set(Vec::new()).write_with_tags(&store, "gone", &[]),
            Err(agent_kit::store::StoreError::NotFound(_))
        ));
    }

    #[test]
    fn a_continued_thread_is_titled_after_its_source_and_the_agent() {
        assert_eq!(
            continued_title("Fix the build", Driver::Codex),
            "Fix the build (→ Codex)"
        );
        assert_eq!(
            continued_title("New thread", Driver::Agy),
            "New thread (→ Antigravity)"
        );
    }

    #[test]
    fn the_save_dialog_starts_with_a_dated_slug() {
        assert_eq!(
            export_file_name("Fix the build!", "2026-11-15", ExportFormat::Markdown),
            "fix-the-build-2026-11-15.md"
        );
        assert_eq!(
            export_file_name("New thread", "2026-11-15", ExportFormat::Json),
            "new-thread-2026-11-15.json"
        );
        assert_eq!(export_file_name("", "", ExportFormat::Json), "thread.json");
    }

    #[test]
    fn the_dropdown_index_picks_its_format() {
        for (i, f) in ExportFormat::ALL.into_iter().enumerate() {
            assert_eq!(export_format_at(i as u32), f);
        }
        assert_eq!(export_format_at(99), ExportFormat::Markdown);
    }

    /// GTK checks, run from the window test (GTK belongs to the one thread that initialised it).
    pub(in crate::window) fn gtk_checks() {
        let input = ThreadMenuInput {
            thread: "t".into(),
            archived: false,
            pinned: false,
            tagged: false,
            has_messages: false,
            forks: false,
            agents: vec![MenuAgent {
                driver: Driver::Claude,
                models: Vec::new(),
            }],
        };
        let menu = to_gio_menu(&thread_menu(&input));
        // Three sections, each a submenu-free group of items or submenus.
        let model: &gio::MenuModel = menu.upcast_ref();
        assert_eq!(model.n_items(), 3);
        let section = model
            .item_link(0, "section")
            .expect("the first section holds Continue in and Switch");
        assert_eq!(
            gio_menu_labels_of(&section),
            [
                "Continue in a new thread with…",
                "Switch agent (stops the current turn)",
                "Fork thread",
                "Compact into a new thread"
            ]
        );
        let housekeeping = model.item_link(1, "section").expect("second section");
        assert_eq!(
            gio_menu_labels_of(&housekeeping),
            ["Rename…", "Pin to Top", "Tags…", "Archive", "Delete…"]
        );
        let links = model.item_link(2, "section").expect("third section");
        assert_eq!(
            gio_menu_labels_of(&links),
            [
                "Open folder",
                "Export…",
                "Review Multi-Agent Environment…",
                "Copy thread id"
            ]
        );
        // Nothing said yet: Export is greyed out; Open folder is not.
        let target = |i: i32| {
            links
                .item_attribute_value(i, "action", None)
                .and_then(|v| v.get::<String>())
        };
        assert_eq!(target(0).as_deref(), Some(ACTION));
        assert_eq!(target(1).as_deref(), Some(ACTION_OFF));
        // With no messages "Continue in" is one greyed-out item, not a submenu to open; Switch
        // still opens.
        let action_of = |model: &gio::MenuModel, index: i32| {
            model
                .item_attribute_value(index, "action", None)
                .and_then(|v| v.get::<String>())
        };
        assert!(section.item_link(0, "submenu").is_none());
        assert_eq!(action_of(&section, 0).as_deref(), Some(ACTION_OFF));
        let switch = section.item_link(1, "submenu").expect("submenu");
        let claude = switch.item_link(0, "submenu").expect("claude");
        let action = claude
            .item_attribute_value(0, "action", None)
            .and_then(|v| v.get::<String>());
        assert_eq!(action.as_deref(), Some(ACTION));
        // The target decodes back to the action.
        let target = claude
            .item_attribute_value(0, "target", None)
            .and_then(|v| v.get::<String>())
            .and_then(|s| ThreadAction::decode(&s));
        assert!(matches!(
            target,
            Some(ThreadAction::SwitchTo {
                driver: Driver::Claude,
                model: None,
                effort: None,
                ..
            })
        ));

        // No agent ready: neither entry opens an empty submenu; both are greyed out, with
        // their labels.
        let none = ThreadMenuInput {
            has_messages: true,
            agents: Vec::new(),
            ..input
        };
        let menu = to_gio_menu(&thread_menu(&none));
        let model: &gio::MenuModel = menu.upcast_ref();
        let section = model.item_link(0, "section").expect("first section");
        assert_eq!(
            gio_menu_labels_of(&section),
            [
                "Continue in a new thread with…",
                "Switch agent (stops the current turn)",
                "Fork thread",
                "Compact into a new thread"
            ]
        );
        for index in 0..2 {
            assert!(section.item_link(index, "submenu").is_none(), "{index}");
            assert_eq!(action_of(&section, index).as_deref(), Some(ACTION_OFF));
        }

        // A fork (or a thread that was forked) offers Compare with Fork… beside Fork thread.
        let forked = ThreadMenuInput {
            forks: true,
            ..none
        };
        let menu = to_gio_menu(&thread_menu(&forked));
        let model: &gio::MenuModel = menu.upcast_ref();
        let section = model.item_link(0, "section").expect("first section");
        assert_eq!(gio_menu_labels_of(&section)[3], "Compare with Fork…");
        assert_eq!(action_of(&section, 3).as_deref(), Some(ACTION));
    }

    /// Each menu choice, carried out on a real window and store, does what its label says and
    /// nothing more. Run from the window test with the shell up (GTK belongs to one thread).
    pub(in crate::window) fn actions_carry_out_each_choice(
        window: &crate::window::AgentTerminalWindow,
    ) {
        use crate::testutil::pump_until;
        use crate::window::imp::threads::app_store;
        let imp = window.imp();
        let ctx = glib::MainContext::default();
        let settle = |cond: &dyn Fn() -> bool| assert!(pump_until(&ctx, 5, cond), "timed out");
        let chat_of = |thread: &str| {
            imp.with_chat(thread, |c| {
                (c.title.clone(), c.driver, c.pending_switch.clone())
            })
        };
        let dir = tempfile::tempdir().expect("tmp");
        let folder = dir.path().to_string_lossy().into_owned();
        let open_thread = || {
            imp.new_chat_thread(Some(Driver::Claude), Some(folder.clone()), None);
            let thread = imp
                .tabs
                .borrow()
                .last()
                .and_then(|t| t.chat.as_ref().map(|c| c.thread.clone()))
                .expect("a thread page");
            imp.reload_summaries();
            thread
        };
        let thread = open_thread();

        // A thread nobody has named is "New thread" everywhere; an unknown one too.
        assert_eq!(imp.thread_title_of(&thread), "New thread");
        assert_eq!(imp.thread_title_of("no-such-thread"), "New thread");

        // Rename: the store, the open page, its tab title and the sidebar row all follow.
        imp.rename_thread_to(&thread, "Fix the build");
        settle(&|| {
            app_store()
                .thread_summary(&thread)
                .ok()
                .flatten()
                .is_some_and(|s| s.title == "Fix the build")
        });
        assert_eq!(
            chat_of(&thread).map(|c| c.0).as_deref(),
            Some("Fix the build")
        );
        assert_eq!(
            imp.page_of_thread(&thread)
                .map(|p| p.title().to_string())
                .as_deref(),
            Some("Fix the build")
        );
        settle(&|| {
            imp.sidebar_rows()
                .iter()
                .any(|r| r.key == RowKey::Thread(thread.clone()) && r.title == "Fix the build")
        });
        // While the page is open its own title wins over a stored one changed elsewhere.
        app_store()
            .rename_thread(&thread, "Changed elsewhere")
            .expect("rename");
        assert_eq!(imp.thread_title_of(&thread), "Fix the build");

        // Copy id: exactly the id reaches the clipboard.
        imp.run_thread_action(ThreadAction::CopyId(thread.clone()));
        let copied = Rc::new(std::cell::RefCell::new(None));
        let sink = copied.clone();
        let clipboard = window.clipboard();
        ctx.spawn_local(async move {
            *sink.borrow_mut() = Some(clipboard.read_text_future().await.ok().flatten());
        });
        settle(&|| copied.borrow().is_some());
        assert_eq!(
            copied
                .borrow()
                .clone()
                .flatten()
                .map(|t| t.to_string())
                .as_deref(),
            Some(thread.as_str())
        );

        // Export of a thread that is gone does nothing (no dialog, no error).
        imp.run_thread_action(ThreadAction::Export("no-such-thread".into()));
        for _ in 0..20 {
            ctx.iteration(false);
        }

        // Continue in, before anything was said: there is nothing to hand over, so no thread
        // is made. Judged by which threads are open, not how many pages: blank pages left by
        // earlier checks close on their own schedule (after their session settles), so a count
        // taken around them is a race.
        let open_threads = || -> std::collections::HashSet<String> {
            imp.tabs
                .borrow()
                .iter()
                .filter_map(|t| t.chat.as_ref().map(|c| c.thread.clone()))
                .collect()
        };
        let before = open_threads();
        imp.run_thread_action(ThreadAction::ContinueIn {
            thread: thread.clone(),
            driver: Driver::Codex,
            model: None,
            effort: None,
        });
        for _ in 0..50 {
            ctx.iteration(false);
        }
        assert!(
            open_threads().is_subset(&before),
            "no handoff, no new thread"
        );

        // With history it opens a NEW Codex thread in the same folder, titled after its
        // source, and leaves the source alone.
        app_store()
            .append_user_message(&thread, None, "make the build pass")
            .expect("message");
        imp.run_thread_action(ThreadAction::ContinueIn {
            thread: thread.clone(),
            driver: Driver::Codex,
            model: None,
            effort: None,
        });
        settle(&|| !open_threads().is_subset(&before));
        {
            let tabs = imp.tabs.borrow();
            let new = tabs
                .iter()
                .find(|t| t.chat.as_ref().is_some_and(|c| !before.contains(&c.thread)))
                .expect("the new page");
            let chat = new.chat.as_ref().expect("a thread page");
            assert_ne!(chat.thread, thread);
            assert_eq!(chat.driver, Driver::Codex);
            assert_eq!(new.dir, folder);
            assert_eq!(chat.title, "Fix the build (→ Codex)");
        }
        assert_eq!(
            chat_of(&thread).map(|c| c.1),
            Some(Driver::Claude),
            "the source is unchanged"
        );

        // The menu itself reflects the thread: with history, Continue in is offered enabled.
        // Folders start collapsed: open the thread's by clicking its header, as a user would.
        let sidebar = imp.sidebar.borrow().clone().expect("sidebar");
        let row_index = || {
            sidebar
                .keys
                .borrow()
                .iter()
                .position(|k| k.as_ref() == Some(&RowKey::Thread(thread.clone())))
        };
        imp.refresh_sidebar();
        if row_index().is_none() {
            let header = crate::testutil::descendants(sidebar.list.upcast_ref())
                .into_iter()
                .find(|w| w.is::<gtk4::ListBoxRow>() && w.widget_name() == folder.as_str())
                .expect("the folder's header");
            let toggle = header
                .first_child()
                .and_then(|line| line.first_child())
                .and_then(|w| w.downcast::<gtk4::Button>().ok())
                .expect("the folder toggle");
            toggle.emit_clicked();
        }
        let index = row_index().expect("the thread's row, once its folder is open");
        let row = sidebar.list.row_at_index(index as i32).expect("row");
        // The menu offers only agents detected as ready; here Claude is, the others are not.
        let availability = crate::availability::AgentAvailability::shared();
        let before = availability.all();
        availability.set(
            Driver::Claude,
            crate::availability::Availability::Ready("/nonexistent/claude".into()),
        );
        imp.open_thread_menu(thread.clone(), &row, Some((4.0, 4.0)));
        let list = imp
            .sidebar
            .borrow()
            .as_ref()
            .map(|s| s.list.clone())
            .expect("list");
        // The thread menu hangs off the list itself (rows' own popovers sit deeper).
        let popover = || {
            let mut child = list.first_child();
            while let Some(widget) = child {
                child = widget.next_sibling();
                if let Ok(menu) = widget.downcast::<gtk4::PopoverMenu>() {
                    return Some(menu);
                }
            }
            None
        };
        settle(&|| popover().is_some());
        // GIO does not bounds-check item indices: count before indexing.
        let link = |model: &gio::MenuModel, index: i32, kind: &str| {
            assert!(
                index < model.n_items(),
                "{kind} {index} of {}",
                model.n_items()
            );
            model.item_link(index, kind).expect(kind)
        };
        let model = popover().and_then(|p| p.menu_model()).expect("menu model");
        assert_eq!(model.n_items(), 3, "the thread menu's three sections");
        let section = link(&model, 0, "section");
        let continue_in = link(&section, 0, "submenu");
        assert_eq!(continue_in.n_items(), 1, "only the ready agent is offered");
        let first_agent = link(&continue_in, 0, "submenu");
        assert!(first_agent.n_items() > 0);
        for (driver, state) in before {
            availability.set(driver, state);
        }
        assert_eq!(
            first_agent
                .item_attribute_value(0, "action", None)
                .and_then(|v| v.get::<String>())
                .as_deref(),
            Some(ACTION),
            "a thread with history can be continued"
        );
        if let Some(p) = popover() {
            p.popdown();
        }

        // Archive: the page closes and the store marks it; Unarchive brings it back.
        imp.run_thread_action(ThreadAction::Archive(thread.clone()));
        assert!(chat_of(&thread).is_none(), "archiving closes the page");
        settle(&|| {
            app_store()
                .thread_summary(&thread)
                .ok()
                .flatten()
                .is_some_and(|s| s.archived)
        });
        imp.run_thread_action(ThreadAction::Unarchive(thread.clone()));
        settle(&|| {
            app_store()
                .thread_summary(&thread)
                .ok()
                .flatten()
                .is_some_and(|s| !s.archived)
        });
        // Closed, its title comes from the store.
        imp.reload_summaries();
        assert_eq!(imp.thread_title_of(&thread), "Changed elsewhere");

        // Switch, for a thread that is not open: it is opened, and the switch waits for its
        // session. A test scan marks every CLI Missing: model the startup interval where the
        // source agent is still being detected, rather than a definitively missing agent.
        let own_before = availability.get(Driver::Claude);
        availability.set(Driver::Claude, Availability::Detecting);
        imp.run_thread_action(ThreadAction::SwitchTo {
            thread: thread.clone(),
            driver: Driver::Codex,
            model: Some("gpt-5".into()),
            effort: None,
        });
        let switched = chat_of(&thread).expect("switching opens the thread");
        assert!(
            imp.slot_of(&thread).and_then(|s| s.get()).is_none(),
            "a reopened thread has no session until it is shown"
        );
        assert_eq!(
            switched.2,
            Some((Driver::Codex, Some("gpt-5".into()), None)),
            "the switch waits for the session"
        );
        settle(&|| {
            imp.with_chat(&thread, |c| c.view.is_some())
                .unwrap_or(false)
        });
        assert_eq!(
            chat_of(&thread).and_then(|c| c.2),
            Some((Driver::Codex, Some("gpt-5".into()), None)),
            "building the view while detecting must keep the parked switch"
        );
        // The scan then finds the thread's own agent missing: the parked switch is not dropped
        // (nor turned into a new thread), the thread itself moves to Codex.
        let threads_before = open_threads();
        availability.set(Driver::Claude, Availability::Missing);
        assert_eq!(
            chat_of(&thread).and_then(|c| c.2),
            None,
            "the parked switch is taken, never left for a later install to apply"
        );
        settle(&|| chat_of(&thread).is_some_and(|c| c.1 == Driver::Codex));
        assert_eq!(
            imp.slot_of(&thread).map(|s| s.driver()),
            Some(Driver::Codex)
        );
        assert!(
            open_threads().is_subset(&threads_before),
            "switching in place never forks"
        );
        let providers = app_store().provider_threads(&thread).expect("providers");
        let active = app_store()
            .active_provider_thread(&thread)
            .expect("active")
            .expect("one is active");
        let active = providers
            .iter()
            .find(|p| p.id == active)
            .expect("the active provider");
        assert_eq!(active.driver, "codex");
        assert_eq!(active.model, "gpt-5");
        assert_eq!(active.native_id, None, "nothing of Claude's is resumed");
        availability.set(Driver::Claude, own_before);

        // Delete: the page closes and the thread leaves the store, its history with it.
        imp.delete_thread_now(&thread);
        assert!(chat_of(&thread).is_none());
        settle(&|| app_store().thread_summary(&thread).ok().flatten().is_none());
        assert!(!app_store().has_messages(&thread).expect("read"));
        // Actions on a thread that is gone do nothing (and do not panic).
        imp.run_thread_action(ThreadAction::OpenFolder(thread.clone()));
        imp.run_thread_action(ThreadAction::ReviewEnvironment(thread.clone()));
        imp.run_thread_action(ThreadAction::ContinueIn {
            thread: thread.clone(),
            driver: Driver::Agy,
            model: None,
            effort: None,
        });
        assert!(chat_of(&thread).is_none());
    }

    fn gio_menu_labels_of(model: &gio::MenuModel) -> Vec<String> {
        (0..model.n_items())
            .filter_map(|i| {
                model
                    .item_attribute_value(i, "label", None)
                    .and_then(|v| v.get::<String>())
            })
            .collect()
    }
}

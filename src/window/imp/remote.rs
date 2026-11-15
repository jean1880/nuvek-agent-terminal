//! Machines reached with the user's own `ssh`: Settings → General → Remote Machines (the list,
//! each machine's Check and Remove) and New Terminal Thread on Another Machine… (the dialog that
//! opens a terminal page there).
//!
//! Nothing here connects on its own. A check runs only when its button is pressed, a page
//! connects only when the user opens it (or presses Reconnect on it), and remote pages are never
//! saved for Reopen Terminal Tabs, so no `ssh` starts at launch.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use agent_core::remote::{self, RemoteCheck};
use gtk4::glib;

use super::{AgentTerminalWindow, RemoteTab};

/// Seconds the whole check may take: ssh's own `ConnectTimeout=10`, plus the remote `true`.
const CHECK_TIMEOUT_SECS: u64 = 20;

/// Whether `alias` answers: `ssh` with [`remote::CHECK_OPTIONS`] running `true` there, never a
/// prompt, through the timeout runner. Blocking: call it off the main thread.
fn check_reachable(alias: &str) -> RemoteCheck {
    let argv = match remote::ssh_check_argv(alias) {
        Ok(argv) => argv,
        Err(why) => return RemoteCheck::Unreachable(why),
    };
    let mut command = std::process::Command::new(&argv[0]);
    command.args(&argv[1..]);
    match agent_kit::exec::run_command(command, "ssh", CHECK_TIMEOUT_SECS) {
        Ok(out) => remote::check_outcome(out.status.code(), &String::from_utf8_lossy(&out.stderr)),
        Err(why) => RemoteCheck::Unreachable(why),
    }
}

/// Shows `problem` (or, with none, `help`) under `row` and marks `entry` invalid or valid, for
/// the eye and for a screen reader.
fn mark_entry(entry: &gtk4::Entry, row: &adw::ActionRow, problem: Option<&str>, help: &str) {
    match problem {
        Some(problem) => {
            entry.add_css_class("error");
            row.set_subtitle(&glib::markup_escape_text(problem));
            entry.update_property(&[gtk4::accessible::Property::Description(problem)]);
            entry.update_state(&[gtk4::accessible::State::Invalid(
                gtk4::AccessibleInvalidState::True,
            )]);
        }
        None => {
            entry.remove_css_class("error");
            row.set_subtitle(&glib::markup_escape_text(help));
            entry.update_property(&[gtk4::accessible::Property::Description("")]);
            entry.update_state(&[gtk4::accessible::State::Invalid(
                gtk4::AccessibleInvalidState::False,
            )]);
        }
    }
}

/// Runs `commit` when `entry` is committed: Enter, or focus leaving it.
fn on_commit(entry: &gtk4::Entry, commit: impl Fn() + 'static) {
    let commit = Rc::new(commit);
    let on_enter = Rc::clone(&commit);
    entry.connect_activate(move |_| on_enter());
    let focus = gtk4::EventControllerFocus::new();
    focus.connect_leave(move |_| commit());
    entry.add_controller(focus);
}

/// A row titled `title` holding one entry.
fn entry_row(
    title: &str,
    help: &str,
    text: &str,
    placeholder: &str,
) -> (adw::ActionRow, gtk4::Entry) {
    let entry = gtk4::Entry::builder()
        .text(text)
        .hexpand(true)
        .valign(gtk4::Align::Center)
        .placeholder_text(placeholder)
        .build();
    entry.update_property(&[gtk4::accessible::Property::Label(title)]);
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(glib::markup_escape_text(help).as_str())
        .build();
    row.add_suffix(&entry);
    (row, entry)
}

const ALIAS_HELP: &str = "A Host from your ssh configuration, a host name, or user@host";
const LABEL_HELP: &str = "How Agent Terminal names it; blank shows the ssh name";

/// The Remote Machines group while Settings is open: its rows, rebuilt when a machine is added
/// or removed, and what each machine's last check said (this session only, never saved).
///
/// Owned only by the add row's handlers, and holding the group and that row weakly, so closing
/// Settings frees it with them.
struct Machines {
    window: glib::WeakRef<crate::window::AgentTerminalWindow>,
    group: glib::WeakRef<adw::PreferencesGroup>,
    rows: RefCell<Vec<adw::ExpanderRow>>,
    add_row: glib::WeakRef<adw::ActionRow>,
    checks: RefCell<HashMap<String, RemoteCheck>>,
}

impl Machines {
    fn window(&self) -> Option<crate::window::AgentTerminalWindow> {
        self.window.upgrade()
    }

    fn check_of(&self, alias: &str) -> RemoteCheck {
        self.checks.borrow().get(alias).cloned().unwrap_or_default()
    }

    /// Draws one row per listed machine, then the add row.
    fn rebuild(self: &Rc<Self>) {
        let (Some(group), Some(add_row), Some(window)) =
            (self.group.upgrade(), self.add_row.upgrade(), self.window())
        else {
            return;
        };
        let old: Vec<adw::ExpanderRow> = self.rows.borrow_mut().drain(..).collect();
        for row in old {
            group.remove(&row);
        }
        group.remove(&add_row);
        let hosts = window.imp().config.borrow().remote_hosts.clone();
        for (index, host) in hosts.iter().enumerate() {
            let row = self.host_row(index, host);
            group.add(&row);
            self.rows.borrow_mut().push(row);
        }
        group.add(&add_row);
    }

    fn host_row(self: &Rc<Self>, index: usize, host: &remote::RemoteHost) -> adw::ExpanderRow {
        let row = adw::ExpanderRow::builder()
            .title(glib::markup_escape_text(host.display_name()).as_str())
            .subtitle(glib::markup_escape_text(&self.check_of(&host.alias).describe()).as_str())
            .build();

        let check = gtk4::Button::builder()
            .label("Check")
            .valign(gtk4::Align::Center)
            .tooltip_text("Try to connect now, without asking for anything")
            .build();
        check.update_property(&[gtk4::accessible::Property::Label(&format!(
            "Check {}",
            host.display_name()
        ))]);
        let remove = gtk4::Button::builder()
            .icon_name("at-user-trash-symbolic")
            .valign(gtk4::Align::Center)
            .tooltip_text("Remove")
            .css_classes(["flat"])
            .build();
        remove.update_property(&[gtk4::accessible::Property::Label(&format!(
            "Remove {}",
            host.display_name()
        ))]);
        row.add_suffix(&check);
        row.add_suffix(&remove);

        let (alias_row, alias_entry) = entry_row(
            "SSH Name",
            ALIAS_HELP,
            &host.alias,
            "host, alias or user@host",
        );
        let (label_row, label_entry) = entry_row("Label", LABEL_HELP, &host.label, &host.alias);
        row.add_row(&alias_row);
        row.add_row(&label_row);
        // The alias this row stands for: a commit arriving after the list changed under it (a
        // focus leaving while rows are rebuilt) finds another machine at `index`, and does
        // nothing.
        let current = Rc::new(RefCell::new(host.alias.clone()));
        let still_here = {
            let current = Rc::clone(&current);
            move |imp: &AgentTerminalWindow| {
                imp.config
                    .borrow()
                    .remote_hosts
                    .get(index)
                    .is_some_and(|h| h.alias == *current.borrow())
            }
        };
        let still_here = Rc::new(still_here);

        // The alias: checked as typed, saved only when committed and valid.
        alias_entry.connect_changed(glib::clone!(
            #[weak]
            alias_row,
            move |entry| {
                let problem = remote::validate_alias(entry.text().trim()).err();
                mark_entry(entry, &alias_row, problem.as_deref(), ALIAS_HELP);
            }
        ));
        let this = Rc::downgrade(self);
        let alias_here = Rc::clone(&still_here);
        let alias_current = Rc::clone(&current);
        on_commit(
            &alias_entry,
            glib::clone!(
                #[weak]
                alias_entry,
                #[weak]
                alias_row,
                #[weak]
                row,
                move || {
                    let Some(this) = this.upgrade() else {
                        return;
                    };
                    let Some(window) = this.window() else {
                        return;
                    };
                    let imp = window.imp();
                    if !alias_here(imp) {
                        return;
                    }
                    let text = alias_entry.text().to_string();
                    let old = alias_current.borrow().clone();
                    let result = imp.config.borrow_mut().set_remote_alias(index, &text);
                    match result {
                        Ok(false) => {}
                        Ok(true) => {
                            // A new name is a new machine as far as the check knows.
                            this.checks.borrow_mut().remove(&old);
                            imp.schedule_config_save();
                            let host = imp.config.borrow().remote_hosts.get(index).cloned();
                            if let Some(host) = host {
                                alias_current.replace(host.alias.clone());
                                row.set_title(&glib::markup_escape_text(host.display_name()));
                                row.set_subtitle(&glib::markup_escape_text(
                                    &RemoteCheck::NotChecked.describe(),
                                ));
                            }
                        }
                        Err(why) => mark_entry(&alias_entry, &alias_row, Some(&why), ALIAS_HELP),
                    }
                }
            ),
        );

        // The label: the same rule.
        label_entry.connect_changed(glib::clone!(
            #[weak]
            label_row,
            move |entry| {
                let problem = remote::validate_label(entry.text().trim()).err();
                mark_entry(entry, &label_row, problem.as_deref(), LABEL_HELP);
            }
        ));
        let this = Rc::downgrade(self);
        let label_here = Rc::clone(&still_here);
        on_commit(
            &label_entry,
            glib::clone!(
                #[weak]
                label_entry,
                #[weak]
                label_row,
                #[weak]
                row,
                move || {
                    let Some(window) = this.upgrade().and_then(|t| t.window()) else {
                        return;
                    };
                    let imp = window.imp();
                    if !label_here(imp) {
                        return;
                    }
                    let result = imp
                        .config
                        .borrow_mut()
                        .set_remote_label(index, &label_entry.text());
                    match result {
                        Ok(false) => {}
                        Ok(true) => {
                            imp.schedule_config_save();
                            let host = imp.config.borrow().remote_hosts.get(index).cloned();
                            if let Some(host) = host {
                                row.set_title(&glib::markup_escape_text(host.display_name()));
                            }
                        }
                        Err(why) => mark_entry(&label_entry, &label_row, Some(&why), LABEL_HELP),
                    }
                }
            ),
        );

        let this = Rc::downgrade(self);
        let check_here = Rc::clone(&still_here);
        let check_current = Rc::clone(&current);
        check.connect_clicked(glib::clone!(
            #[weak]
            row,
            move |button| {
                let Some(this) = this.upgrade() else {
                    return;
                };
                let Some(window) = this.window() else {
                    return;
                };
                if !check_here(window.imp()) {
                    return;
                }
                let alias = check_current.borrow().clone();
                this.start_check(alias, &row, button);
            }
        ));

        let this = Rc::downgrade(self);
        let remove_here = Rc::clone(&still_here);
        remove.connect_clicked(move |_| {
            let Some(this) = this.upgrade() else {
                return;
            };
            let Some(window) = this.window() else {
                return;
            };
            let imp = window.imp();
            if !remove_here(imp) {
                return;
            }
            let removed = {
                let mut config = imp.config.borrow_mut();
                (index < config.remote_hosts.len()).then(|| config.remote_hosts.remove(index))
            };
            if let Some(host) = removed {
                this.checks.borrow_mut().remove(&host.alias);
                imp.schedule_config_save();
                // Rebuilt after this handler returns, since it removes this very button's row.
                let this = Rc::clone(&this);
                glib::idle_add_local_once(move || this.rebuild());
            }
        });
        row
    }

    /// Runs the check for `alias` off the main thread and shows its answer on `row`.
    fn start_check(self: &Rc<Self>, alias: String, row: &adw::ExpanderRow, button: &gtk4::Button) {
        button.set_sensitive(false);
        self.checks
            .borrow_mut()
            .insert(alias.clone(), RemoteCheck::Checking);
        row.set_subtitle(&glib::markup_escape_text(&RemoteCheck::Checking.describe()));
        let this = Rc::downgrade(self);
        let row = row.downgrade();
        let button = button.downgrade();
        glib::MainContext::default().spawn_local(async move {
            let checked = alias.clone();
            let result = gtk4::gio::spawn_blocking(move || check_reachable(&checked))
                .await
                .unwrap_or_else(|_| RemoteCheck::Unreachable("the check failed".to_owned()));
            tracing::info!("Remote machine check: {}", result.describe());
            if let Some(button) = button.upgrade() {
                button.set_sensitive(true);
            }
            let Some(this) = this.upgrade() else {
                return;
            };
            // Kept only while the machine is still listed under that name.
            let listed = this.window().is_some_and(|w| {
                w.imp()
                    .config
                    .borrow()
                    .remote_hosts
                    .iter()
                    .any(|h| h.alias == alias)
            });
            if !listed {
                this.checks.borrow_mut().remove(&alias);
                return;
            }
            if let Some(row) = row.upgrade() {
                row.set_subtitle(&glib::markup_escape_text(&result.describe()));
            }
            this.checks.borrow_mut().insert(alias, result);
        });
    }
}

impl AgentTerminalWindow {
    /// Settings → General → Remote Machines.
    pub(super) fn remote_machines_group(&self) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::builder()
            .title("Remote Machines")
            .description(
                "Machines New Terminal Thread on Another Machine can open a terminal on, with \
                 your own ssh and its configuration. Nothing connects until you open a terminal \
                 there or press Check, and never with port, agent or X11 forwarding.",
            )
            .build();
        let (add_row, add_entry) =
            entry_row("Add a Machine", ALIAS_HELP, "", "host, alias or user@host");
        let add = gtk4::Button::builder()
            .label("Add")
            .valign(gtk4::Align::Center)
            .sensitive(false)
            .build();
        add.update_property(&[gtk4::accessible::Property::Label("Add the machine")]);
        add_row.add_suffix(&add);

        let machines = Rc::new(Machines {
            window: self.obj().downgrade(),
            group: group.downgrade(),
            rows: RefCell::new(Vec::new()),
            add_row: add_row.downgrade(),
            checks: RefCell::new(HashMap::new()),
        });
        machines.rebuild();

        add_entry.connect_changed(glib::clone!(
            #[weak]
            add_row,
            #[weak]
            add,
            move |entry| {
                let text = entry.text();
                // An empty entry is not an error yet, only nothing to add.
                let problem = (!text.trim().is_empty())
                    .then(|| remote::validate_alias(text.trim()).err())
                    .flatten();
                mark_entry(entry, &add_row, problem.as_deref(), ALIAS_HELP);
                add.set_sensitive(!text.trim().is_empty() && problem.is_none());
            }
        ));
        // Added on Enter or the Add button only: leaving the entry keeps the draft.
        let commit = Rc::new(glib::clone!(
            #[weak]
            add_entry,
            #[weak]
            add_row,
            move || {
                let Some(window) = machines.window() else {
                    return;
                };
                let imp = window.imp();
                let text = add_entry.text().to_string();
                if text.trim().is_empty() {
                    return;
                }
                let result = imp.config.borrow_mut().add_remote_host(&text, "");
                match result {
                    Ok(()) => {
                        imp.schedule_config_save();
                        add_entry.set_text("");
                        machines.rebuild();
                    }
                    Err(why) => mark_entry(&add_entry, &add_row, Some(&why), ALIAS_HELP),
                }
            }
        ));
        let on_enter = Rc::clone(&commit);
        add_entry.connect_activate(move |_| on_enter());
        add.connect_clicked(move |_| commit());
        group
    }

    /// New Terminal Thread on Another Machine…: pick a listed machine and a folder there, then
    /// open a terminal page on it. With none listed, says so and offers Settings.
    pub(super) fn show_remote_terminal_dialog(&self) {
        let obj = self.obj();
        let hosts = self.config.borrow().remote_hosts.clone();
        if hosts.is_empty() {
            let dialog = crate::dialogs::confirm_dialog(
                "No Remote Machines",
                "Add a machine in Settings → General → Remote Machines, by the name you reach it \
                 with ssh. Then open a terminal on it from here.",
                ("close", "Close"),
                ("settings", "Open Settings"),
                adw::ResponseAppearance::Suggested,
                true,
            );
            dialog.connect_response(
                Some("settings"),
                glib::clone!(
                    #[weak]
                    obj,
                    move |_, _| obj.imp().show_preferences()
                ),
            );
            dialog.present(Some(&*obj));
            return;
        }

        let dialog = crate::dialogs::confirm_dialog(
            "New Terminal Thread on Another Machine",
            "Opens a terminal there with your own ssh. A first connection may ask, in the \
             terminal, to accept the machine's key or for a password. Checkpoints, changes and \
             hand-offs are not available on another machine.",
            ("cancel", "Cancel"),
            ("open", "Open"),
            adw::ResponseAppearance::Suggested,
            true,
        );
        let names: Vec<String> = hosts
            .iter()
            .map(|h| {
                if h.display_name() == h.alias {
                    h.alias.clone()
                } else {
                    format!("{} ({})", h.display_name(), h.alias)
                }
            })
            .collect();
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let machine = adw::ComboRow::builder()
            .title("Machine")
            .model(&gtk4::StringList::new(&name_refs))
            .build();
        let folder = adw::EntryRow::builder()
            .title("Folder There (~ is your home there)")
            .text("~")
            .activates_default(true)
            .build();
        let rows = gtk4::ListBox::builder()
            .selection_mode(gtk4::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        rows.append(&machine);
        rows.append(&folder);
        dialog.set_extra_child(Some(&rows));

        folder.connect_changed(glib::clone!(
            #[weak]
            dialog,
            move |row| {
                let problem = remote::parse_remote_dir(&row.text()).err();
                match &problem {
                    Some(problem) => {
                        row.add_css_class("error");
                        row.set_tooltip_text(Some(problem));
                        row.upcast_ref::<gtk4::Widget>()
                            .update_property(&[gtk4::accessible::Property::Description(problem)]);
                    }
                    None => {
                        row.remove_css_class("error");
                        row.set_tooltip_text(None);
                        row.upcast_ref::<gtk4::Widget>()
                            .update_property(&[gtk4::accessible::Property::Description("")]);
                    }
                }
                dialog.set_response_enabled("open", problem.is_none());
            }
        ));
        dialog.connect_response(
            Some("open"),
            glib::clone!(
                #[weak]
                obj,
                #[weak]
                machine,
                #[weak]
                folder,
                move |_, _| {
                    let Some(host) = hosts.get(machine.selected() as usize) else {
                        return;
                    };
                    let Ok(dir) = remote::parse_remote_dir(&folder.text()) else {
                        return;
                    };
                    obj.imp().add_remote_terminal_tab(RemoteTab {
                        alias: host.alias.clone(),
                        dir,
                    });
                }
            ),
        );
        dialog.present(Some(&*obj));
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// The Remote Machines group, driven as a user drives it: an alias is saved only when it is
    /// valid and committed, a refusal is shown on the field and saves nothing, and Remove takes
    /// the machine off the list. No check runs. Run from the window test.
    pub(in crate::window) fn the_remote_machines_group_saves_only_valid_aliases(
        window: &crate::window::AgentTerminalWindow,
    ) {
        let imp = window.imp();
        let before = std::mem::take(&mut imp.config.borrow_mut().remote_hosts);
        let group = imp.remote_machines_group();
        let ctx = glib::MainContext::default();
        let entries = || -> Vec<gtk4::Entry> {
            crate::testutil::descendants(group.upcast_ref())
                .into_iter()
                .filter_map(|w| w.downcast::<gtk4::Entry>().ok())
                .collect()
        };
        let hosts = || imp.config.borrow().remote_hosts.clone();
        let commit = |entry: &gtk4::Entry, text: &str| {
            entry.set_text(text);
            entry.emit_by_name::<()>("activate", &[]);
            crate::testutil::settle(&ctx);
        };

        let add = entries().pop().expect("the add entry");
        add.set_text("-oProxy");
        crate::testutil::settle(&ctx);
        assert!(hosts().is_empty(), "typing saves nothing");
        for bad in ["-oProxyCommand=x", "box;id", "a b", "$(id)"] {
            commit(&add, bad);
            assert!(hosts().is_empty(), "{bad:?} was saved");
            assert!(add.has_css_class("error"), "{bad:?} is not shown as wrong");
        }
        commit(&add, "box");
        assert_eq!(
            hosts(),
            vec![agent_core::remote::RemoteHost::new("box", "").unwrap()]
        );
        assert_eq!(add.text(), "", "the add field is cleared");

        // The machine's own row: alias, label, then the add row's entry.
        let fields = entries();
        assert_eq!(fields.len(), 3);
        let alias = fields
            .iter()
            .find(|e| e.text() == "box")
            .cloned()
            .expect("the alias field");
        // The label field's placeholder is the alias it falls back to.
        let label = fields
            .iter()
            .find(|e| e.placeholder_text().as_deref() == Some("box"))
            .cloned()
            .expect("the label field");
        commit(&alias, "-v");
        assert_eq!(
            hosts()[0].alias,
            "box",
            "a refused rename keeps the saved name"
        );
        assert!(alias.has_css_class("error"));
        commit(&alias, "box2");
        assert_eq!(hosts()[0].alias, "box2");
        assert!(!alias.has_css_class("error"));
        commit(&label, "Build box");
        assert_eq!(hosts()[0].label, "Build box");

        // Every icon-only button says what it does.
        let buttons: Vec<gtk4::Button> = crate::testutil::descendants(group.upcast_ref())
            .into_iter()
            .filter_map(|w| w.downcast::<gtk4::Button>().ok())
            .collect();
        let remove = buttons
            .iter()
            .find(|b| b.icon_name().as_deref() == Some("at-user-trash-symbolic"))
            .expect("a remove button");
        remove.emit_clicked();
        crate::testutil::settle(&ctx);
        assert!(hosts().is_empty(), "Remove took it off the list");
        assert_eq!(entries().len(), 1, "only the add row is left");

        imp.config.borrow_mut().remote_hosts = before;
    }
}

//! Machines reached with the user's own `ssh`: Settings → General → Remote Machines (the list,
//! each machine's Check and Remove), New Terminal Thread on Another Machine… (the dialog that
//! opens a terminal page there) and New Thread on Another Machine… (Claude or Codex there).
//!
//! Nothing here connects on its own. A check runs only when its button is pressed, a page
//! connects only when the user opens it (or presses Reconnect on it), a thread there is checked
//! only when the user presses Create and its agent starts only when the user opens it, and
//! neither remote pages nor remote threads are reopened at launch, so no `ssh` starts then.

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
/// prompt, through the timeout runner, in exactly `env` (`KEY=value` entries: the environment a
/// remote page gets, `clear_env` removed). Blocking: call it off the main thread.
fn check_reachable(alias: &str, env: &[String]) -> RemoteCheck {
    let argv = match remote::ssh_check_argv(alias) {
        Ok(argv) => argv,
        Err(why) => return RemoteCheck::Unreachable(why),
    };
    let mut command = std::process::Command::new(&argv[0]);
    command.args(&argv[1..]).env_clear();
    command.envs(env.iter().filter_map(|entry| entry.split_once('=')));
    match agent_kit::exec::run_command(command, "ssh", CHECK_TIMEOUT_SECS) {
        Ok(out) => remote::check_outcome(out.status.code(), &String::from_utf8_lossy(&out.stderr)),
        Err(why) => RemoteCheck::Unreachable(why),
    }
}

/// Shows `problem` (or, with none, `help`) under `row` and marks `entry` invalid or valid, for
/// the eye and for a screen reader.
pub(super) fn mark_entry(
    entry: &gtk4::Entry,
    row: &adw::ActionRow,
    problem: Option<&str>,
    help: &str,
) {
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
pub(super) fn on_commit(entry: &gtk4::Entry, commit: impl Fn() + 'static) {
    let commit = Rc::new(commit);
    let on_enter = Rc::clone(&commit);
    entry.connect_activate(move |_| on_enter());
    let focus = gtk4::EventControllerFocus::new();
    focus.connect_leave(move |_| commit());
    entry.add_controller(focus);
}

/// A row titled `title` holding one entry.
pub(super) fn entry_row(
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

/// Marks a folder row invalid (with `problem`) or valid, for the eye and a screen reader.
fn mark_folder(row: &adw::EntryRow, problem: Option<&str>) {
    match problem {
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
        // The same environment a page on that machine gets.
        let env = self
            .window()
            .map(|w| w.imp().terminal_env())
            .unwrap_or_default();
        let this = Rc::downgrade(self);
        let row = row.downgrade();
        let button = button.downgrade();
        glib::MainContext::default().spawn_local(async move {
            let checked = alias.clone();
            let result = gtk4::gio::spawn_blocking(move || check_reachable(&checked, &env))
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

/// Whether a thread in `folder` may run `driver`: Claude or Codex, on a machine still listed in
/// Settings ([`remote::listed`]). Checked when the thread is made and again at every start.
pub(super) fn remote_thread_allowed(
    config: &crate::config::TerminalConfig,
    folder: &remote::RemoteFolder,
    driver: agent_core::adapter::Driver,
) -> Result<(), String> {
    remote::chat_cli(driver)?;
    remote::listed(&config.remote_hosts, folder.alias())
}

/// The agents a thread on another machine can run, in the dialog's order.
const REMOTE_AGENTS: [agent_core::adapter::Driver; 2] = [
    agent_core::adapter::Driver::Claude,
    agent_core::adapter::Driver::Codex,
];

/// Asks `alias` where `driver`'s CLI is and what `dir` is there, with the user's `ssh` in batch
/// mode (`env`: the environment a remote page gets). Blocking: call it off the main thread.
fn check_remote_thread(
    alias: &str,
    dir: &str,
    driver: agent_core::adapter::Driver,
    env: Vec<String>,
) -> Result<remote::RemoteFolder, agent_kit::remote::RemoteError> {
    use agent_kit::remote::RemoteError;
    let cli = remote::chat_cli(driver).map_err(RemoteError::Refused)?;
    let Some(program) = glib::find_program_in_path("ssh") else {
        return Err(RemoteError::Refused(
            "ssh was not found. Install the OpenSSH client.".to_owned(),
        ));
    };
    let ssh = agent_kit::remote::Ssh {
        program: program.to_string_lossy().into_owned(),
        env,
    };
    agent_kit::remote::probe(&ssh, alias, cli)?;
    let canonical = agent_kit::remote::check_folder(&ssh, alias, dir)?;
    remote::RemoteFolder::new(alias, &canonical).map_err(RemoteError::Refused)
}

impl AgentTerminalWindow {
    /// Creates a thread on another machine in `folder` (already checked there) on `driver`
    /// (else the default agent, when Claude or Codex, else Claude), opened once stored.
    pub(super) fn new_remote_chat_thread(
        &self,
        driver: Option<agent_core::adapter::Driver>,
        folder: remote::RemoteFolder,
    ) {
        let driver = driver
            .or_else(|| self.default_agent())
            .filter(|d| remote::chat_cli(*d).is_ok())
            .unwrap_or(agent_core::adapter::Driver::Claude);
        if let Err(why) = remote_thread_allowed(&self.config.borrow(), &folder, driver) {
            super::present_message(&self.obj(), "Cannot Create a Thread", &why);
            return;
        }
        tracing::info!("New thread on another machine");
        self.store_new_thread(super::threads::NewThread::new(driver), folder.cwd());
    }

    /// Offers, in a toast, a terminal page on `alias` in `dir`: where the user answers what a
    /// batch `ssh` cannot (a host key to accept, a password).
    pub(super) fn offer_remote_terminal(&self, alias: &str, dir: &str) {
        let remote = RemoteTab {
            alias: alias.to_owned(),
            dir: dir.to_owned(),
        };
        self.action_toast(
            &format!("{alias} needs you to accept its key or sign in"),
            "Open a Terminal There",
            15,
            adw::ToastPriority::High,
            move |imp| imp.add_remote_terminal_tab(remote.clone()),
        );
    }

    /// The dialog said when no machine is listed: where to add one.
    fn no_remote_machines(&self) {
        let obj = self.obj();
        let dialog = crate::dialogs::confirm_dialog(
            "No Remote Machines",
            "Add a machine in Settings → General → Remote Machines, by the name you reach it \
             with ssh. Then open a thread on it from here.",
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
    }

    /// New Thread on Another Machine…: a listed machine, a folder there and Claude or Codex.
    /// Create asks the machine (off the main thread, `ssh` in batch mode) where the agent is
    /// and what the folder is canonically, then makes the thread; nothing connects before.
    pub(super) fn show_remote_thread_dialog(&self) {
        let obj = self.obj();
        let hosts = self.config.borrow().remote_hosts.clone();
        if hosts.is_empty() {
            self.no_remote_machines();
            return;
        }
        let dialog = crate::dialogs::confirm_dialog(
            "New Thread on Another Machine",
            "Runs the agent there with your own ssh, never asking for anything: accept a new \
             machine's key or a password in a terminal page there first. Approvals offer Allow \
             once and Deny only; checkpoints, changes, undo and hand-off briefs are not \
             available on another machine.",
            ("cancel", "Cancel"),
            ("create", "Create"),
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
        let agent_names: Vec<&str> = REMOTE_AGENTS.iter().map(|d| d.info().label).collect();
        let agent = adw::ComboRow::builder()
            .title("Agent")
            .model(&gtk4::StringList::new(&agent_names))
            .build();
        if let Some(at) = self
            .default_agent()
            .and_then(|d| REMOTE_AGENTS.iter().position(|r| *r == d))
        {
            agent.set_selected(at as u32);
        }
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
        rows.append(&agent);
        rows.append(&folder);
        dialog.set_extra_child(Some(&rows));
        folder.connect_changed(glib::clone!(
            #[weak]
            dialog,
            move |row| {
                let problem = remote::parse_remote_dir(&row.text()).err();
                mark_folder(row, problem.as_deref());
                dialog.set_response_enabled("create", problem.is_none());
            }
        ));
        dialog.connect_response(
            Some("create"),
            glib::clone!(
                #[weak]
                obj,
                #[weak]
                machine,
                #[weak]
                agent,
                #[weak]
                folder,
                move |_, _| {
                    let Some(host) = hosts.get(machine.selected() as usize) else {
                        return;
                    };
                    let Some(&driver) = REMOTE_AGENTS.get(agent.selected() as usize) else {
                        return;
                    };
                    let Ok(dir) = remote::parse_remote_dir(&folder.text()) else {
                        return;
                    };
                    obj.imp()
                        .create_remote_thread(host.alias.clone(), dir, driver);
                }
            ),
        );
        dialog.present(Some(&*obj));
    }

    /// Checks `dir` and the agent on `alias` off the main thread, then makes the thread, or says
    /// why not (offering a terminal page there when one would get past it).
    fn create_remote_thread(
        &self,
        alias: String,
        dir: String,
        driver: agent_core::adapter::Driver,
    ) {
        if let Err(why) = remote::listed(&self.config.borrow().remote_hosts, &alias) {
            super::present_message(&self.obj(), "Cannot Create a Thread", &why);
            return;
        }
        self.show_toast(&format!("Checking {alias}…"));
        let env = self.terminal_env();
        let obj = self.obj().downgrade();
        glib::MainContext::default().spawn_local(async move {
            let (asked_alias, asked_dir) = (alias.clone(), dir.clone());
            let checked = gtk4::gio::spawn_blocking(move || {
                check_remote_thread(&asked_alias, &asked_dir, driver, env)
            })
            .await
            .unwrap_or_else(|_| {
                Err(agent_kit::remote::RemoteError::Refused(
                    "the check of the other machine failed".to_owned(),
                ))
            });
            let Some(obj) = obj.upgrade() else { return };
            let imp = obj.imp();
            if imp.is_closing() {
                return;
            }
            match checked {
                Ok(folder) => imp.new_remote_chat_thread(Some(driver), folder),
                Err(failure) => {
                    tracing::info!("A thread on another machine could not be made");
                    if failure.needs_terminal() {
                        imp.offer_remote_terminal(&alias, &dir);
                    }
                    super::present_message(
                        &obj,
                        "Cannot Create a Thread",
                        &format!("{alias}: {}", failure.reason()),
                    );
                }
            }
        });
    }

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
            self.no_remote_machines();
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
                mark_folder(row, problem.as_deref());
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

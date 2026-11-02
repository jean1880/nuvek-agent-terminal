//! Managed registration is previewed on a worker and applied only by its explicit dialog.
//!
//! One master switch ("agents can use each other as sub-agents") drives every agent's own
//! "Available for delegation" switch; those stay, as the per-agent opt-out. [`Switches`] keeps
//! them in step.

use super::*;
use agent_kit::delegation_config::{self, Action};
use std::cell::Cell;
use std::path::PathBuf;

/// The master switch, each agent's switch and the registration row, so a change to one shows
/// on the others. `quiet` is set while code moves a switch: its handler then does nothing.
pub(super) struct Switches {
    master: RefCell<Option<glib::WeakRef<adw::SwitchRow>>>,
    agents: RefCell<Vec<(Driver, glib::WeakRef<adw::SwitchRow>)>>,
    /// Each Local server's own switch, by profile id (the rows are rebuilt on an add or a
    /// removal, so a dead one is dropped when the next is added).
    servers: RefCell<Vec<(String, glib::WeakRef<adw::SwitchRow>)>>,
    pub(super) registration_row: glib::WeakRef<adw::ActionRow>,
    dialog: glib::WeakRef<adw::PreferencesDialog>,
    quiet: Cell<bool>,
    /// An Antigravity review is open: the master switch stays where the user put it until the
    /// review ends, instead of flicking off while the registration is not yet saved.
    reviewing: Cell<bool>,
    /// Antigravity was turned off while its review was open: the review's outcome is not saved,
    /// and a registration it applied is offered for removal.
    withdrawn: Cell<bool>,
}

impl Switches {
    fn set_quietly(&self, row: &adw::SwitchRow, on: bool) {
        if row.is_active() != on {
            self.quiet.set(true);
            row.set_active(on);
            self.quiet.set(false);
        }
    }
    fn agent_row(&self, driver: Driver) -> Option<adw::SwitchRow> {
        self.agents
            .borrow()
            .iter()
            .find(|(d, _)| *d == driver)
            .and_then(|(_, row)| row.upgrade())
    }
    /// Registers `driver`'s own switch (built after the master one).
    pub(super) fn add_agent(&self, driver: Driver, row: &adw::SwitchRow) {
        self.agents.borrow_mut().push((driver, row.downgrade()));
    }
    /// Registers a Local server's own switch.
    pub(super) fn add_server(&self, id: &str, row: &adw::SwitchRow) {
        let mut servers = self.servers.borrow_mut();
        servers.retain(|(_, r)| r.upgrade().is_some());
        servers.push((id.to_owned(), row.downgrade()));
    }
    fn server_row(&self, id: &str) -> Option<adw::SwitchRow> {
        self.servers
            .borrow()
            .iter()
            .find(|(i, _)| i == id)
            .and_then(|(_, row)| row.upgrade())
    }
    /// Whether code is moving a switch right now (its handler should do nothing).
    pub(super) fn quiet(&self) -> bool {
        self.quiet.get()
    }
}

/// Something with its own "available for delegation" switch: one of the vendor agents (one
/// profile each) or one Local server (any number, each its own profile).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Unit {
    Agent(Driver),
    Server(String),
}

/// One unit's state for the master switch: its name, whether it is switched off in Settings,
/// and whether it is available for delegation.
pub(super) type UnitState = (String, bool, bool);

/// The master switch's text, and whether it is on: on only when every enabled agent and every
/// enabled Local server is available for delegation. Pure, so the mixed case is testable without
/// GTK.
pub(super) fn master_state(agents: &[UnitState]) -> (bool, String) {
    let enabled: Vec<_> = agents.iter().filter(|(_, disabled, _)| !disabled).collect();
    let on: Vec<&str> = enabled
        .iter()
        .filter(|(_, _, a)| *a)
        .map(|(n, _, _)| n.as_str())
        .collect();
    let off: Vec<&str> = enabled
        .iter()
        .filter(|(_, _, a)| !*a)
        .map(|(n, _, _)| n.as_str())
        .collect();
    let caveat = agent_core::delegation::route::RouteCaveat::CodexNativeWorkersUnsupervised;
    let base = format!(
        "Every enabled agent can hand work to the others and take work from them, in any \
         thread, whichever agent drives it. Each thread's sub-agents are its own. Antigravity \
         needs a one-time registration, reviewed when you turn this on.\nCodex as a sub-agent: \
         {}",
        caveat.explain()
    );
    let all = !enabled.is_empty() && off.is_empty();
    let text = if all || on.is_empty() {
        base
    } else {
        format!(
            "Custom: on for {}; off for {}. Turn this on to include every agent.\n{base}",
            on.join(", "),
            off.join(", ")
        )
    };
    (all, text)
}

impl AgentTerminalWindow {
    /// The delegation state now of each vendor agent and of each Local server (per profile, as
    /// the Local agent has any number): what the master switch counts.
    fn delegation_units(&self) -> Vec<(Unit, UnitState)> {
        let mut out: Vec<(Unit, UnitState)> = Driver::ALL
            .into_iter()
            .filter(|d| !d.is_built_in())
            .map(|d| {
                let p = self.agent_profile_now(d);
                (
                    Unit::Agent(d),
                    (
                        d.info().long_label.to_owned(),
                        p.disabled,
                        p.available_for_delegation,
                    ),
                )
            })
            .collect();
        out.extend(self.config.borrow().local_servers().into_iter().map(|p| {
            (
                Unit::Server(p.id.clone()),
                (p.name.clone(), p.disabled, p.available_for_delegation),
            )
        }));
        out
    }

    fn delegation_agents(&self) -> Vec<UnitState> {
        self.delegation_units()
            .into_iter()
            .map(|(_, s)| s)
            .collect()
    }

    /// Makes the Local server `id` available for delegation or not: saved at once, its switch and
    /// the master one put in step. A server stays refused as a delegation target until the
    /// routes admit the Local agent; the switch records the choice either way.
    pub(super) fn set_server_delegation(&self, id: &str, on: bool, switches: &Rc<Switches>) {
        self.edit_server(id, |p| p.available_for_delegation = on);
        let saved = self
            .config
            .borrow()
            .profile_by_id(id)
            .is_some_and(|p| p.available_for_delegation);
        if let Some(row) = switches.server_row(id) {
            switches.set_quietly(&row, saved);
        }
        self.refresh_master_switch(switches);
    }

    /// Puts the master switch and its text in step with the saved settings. The switch itself
    /// moves on the next idle, never from inside a switch's own change: GObject holds a nested
    /// change's notification until the outer one ends, so it would arrive after `quiet` was
    /// cleared and be taken for the user's click (turning every agent back off).
    pub(super) fn refresh_master_switch(&self, switches: &Rc<Switches>) {
        let Some(master) = switches.master.borrow().as_ref().and_then(|m| m.upgrade()) else {
            return;
        };
        master.set_subtitle(&master_state(&self.delegation_agents()).1);
        let (window, switches) = (self.obj().downgrade(), switches.clone());
        glib::idle_add_local_once(move || {
            let (Some(window), Some(master)) = (
                window.upgrade(),
                switches.master.borrow().as_ref().and_then(|m| m.upgrade()),
            ) else {
                return;
            };
            if switches.reviewing.get() {
                return;
            }
            let (on, text) = master_state(&window.imp().delegation_agents());
            switches.set_quietly(&master, on);
            master.set_subtitle(&text);
        });
    }

    /// Makes `driver` available for delegation or not, as its own switch or the master one
    /// asked. Claude and Codex save at once. Antigravity is saved on only once its
    /// registration review is applied (or found ready); declined or failed, it stays off, and
    /// turning it off offers Remove. Both switches end in step with what was saved.
    pub(super) fn set_agent_delegation(&self, driver: Driver, on: bool, switches: &Rc<Switches>) {
        let settle = {
            let (window, switches) = (self.obj().downgrade(), switches.clone());
            move |on: bool| {
                let Some(window) = window.upgrade() else {
                    return;
                };
                if driver == Driver::Agy {
                    switches.reviewing.set(false);
                }
                if let Some(row) = switches.agent_row(driver) {
                    switches.set_quietly(&row, on);
                }
                window.imp().refresh_master_switch(&switches);
            }
        };
        if driver == Driver::Agy && !on && switches.reviewing.get() {
            // The open review decides nothing now: its end settles Antigravity off.
            switches.withdrawn.set(true);
            self.refresh_master_switch(switches);
            return;
        }
        if driver != Driver::Agy || !on {
            self.edit_agent_profile(driver, |p| p.available_for_delegation = on);
            settle(on);
            if driver == Driver::Agy {
                self.review_delegation_registration(
                    Action::Remove,
                    switches.dialog.clone(),
                    switches.registration_row.clone(),
                    |_| {},
                );
            }
            return;
        }
        switches.reviewing.set(true);
        switches.withdrawn.set(false);
        let window = self.obj().downgrade();
        let review_switches = switches.clone();
        self.review_delegation_registration(
            Action::Configure,
            switches.dialog.clone(),
            switches.registration_row.clone(),
            move |review| {
                let Some(window) = window.upgrade() else {
                    return;
                };
                let switches = review_switches;
                if switches.withdrawn.replace(false) {
                    if matches!(review, Review::Ready) {
                        window.imp().review_delegation_registration(
                            Action::Remove,
                            switches.dialog.clone(),
                            switches.registration_row.clone(),
                            |_| {},
                        );
                    }
                    settle(false);
                    return;
                }
                let ready = matches!(review, Review::Ready);
                if let Review::Failed(error) = &review {
                    window
                        .imp()
                        .show_toast(&format!("Antigravity was not set up: {error}"));
                }
                if ready {
                    window
                        .imp()
                        .edit_agent_profile(driver, |p| p.available_for_delegation = true);
                }
                settle(ready);
            },
        );
    }

    /// "Agents can use each other as sub-agents": turns every enabled agent's delegation on or
    /// off together (Antigravity through its review).
    fn master_delegation_row(&self, switches: &Rc<Switches>) -> adw::SwitchRow {
        let row = adw::SwitchRow::builder()
            .title("Agents can use each other as sub-agents")
            .subtitle_lines(0)
            .build();
        switches.master.replace(Some(row.downgrade()));
        self.refresh_master_switch(switches);
        let obj = self.obj();
        let switches = switches.clone();
        row.connect_active_notify(glib::clone!(
            #[weak]
            obj,
            move |row| {
                if switches.quiet() {
                    return;
                }
                let on = row.is_active();
                let imp = obj.imp();
                for (unit, (_, disabled, available)) in imp.delegation_units() {
                    // Turning on skips disabled agents; turning off includes them, and an
                    // Antigravity still in its review (not yet saved on).
                    let in_review = unit == Unit::Agent(Driver::Agy) && switches.reviewing.get();
                    if (on && disabled) || (available == on && !(in_review && !on)) {
                        continue;
                    }
                    match unit {
                        Unit::Agent(driver) => imp.set_agent_delegation(driver, on, &switches),
                        Unit::Server(id) => imp.set_server_delegation(&id, on, &switches),
                    }
                }
                imp.refresh_master_switch(&switches);
            }
        ));
        row
    }

    /// The delegation group, and the [`Switches`] the agent groups' own switches join.
    pub(super) fn delegation_group(
        &self,
        dialog: &adw::PreferencesDialog,
    ) -> (adw::PreferencesGroup, Rc<Switches>) {
        let group = adw::PreferencesGroup::builder()
            .title("Cross-agent delegation")
            .description(agent_core::delegation::tools::DELEGATION_SCOPE_TEXT)
            .build();
        let row = adw::ActionRow::builder().title("Antigravity registration")
            .subtitle("Checking… Claude and Codex use temporary launch bindings; Antigravity needs a managed registration, set up when it is made available for delegation.")
            .subtitle_lines(0).build();
        let switches = Rc::new(Switches {
            master: RefCell::new(None),
            agents: RefCell::new(Vec::new()),
            servers: RefCell::new(Vec::new()),
            registration_row: row.downgrade(),
            dialog: dialog.downgrade(),
            quiet: Cell::new(false),
            reviewing: Cell::new(false),
            withdrawn: Cell::new(false),
        });
        group.add(&self.master_delegation_row(&switches));
        let runtime = adw::ActionRow::builder().title("Application binding")
            .subtitle(if crate::delegation::supervisor().is_some() {
                "Supervisor available. Each enabled thread receives its own binding when opened."
            } else { "Could not verify the supervisor. Delegation is unavailable until startup completes." })
            .build();
        group.add(&runtime);
        group.add(&row);
        self.check_delegation_registration(row.downgrade());
        for (title, action) in [
            ("Configure / Repair", Some(Action::Configure)),
            ("Remove", Some(Action::Remove)),
            ("Test", None),
        ] {
            let button = gtk4::Button::with_label(title);
            let obj = self.obj();
            let weak_dialog = dialog.downgrade();
            let weak_row = row.downgrade();
            button.connect_clicked(glib::clone!(
                #[weak]
                obj,
                move |_| {
                    obj.imp().preview_delegation_registration(
                        action,
                        weak_dialog.clone(),
                        weak_row.clone(),
                    );
                }
            ));
            let action_row = adw::ActionRow::builder().title(title).build();
            action_row.add_suffix(&button);
            action_row.set_activatable_widget(Some(&button));
            group.add(&action_row);
        }
        let undo = gtk4::Button::with_label("Undo last change");
        let obj = self.obj();
        let weak_row = row.downgrade();
        undo.connect_clicked(glib::clone!(
            #[weak]
            obj,
            move |_| {
                let profile = obj.imp().agent_profile_now(Driver::Agy);
                let clear = obj.imp().config.borrow().clear_env.clone();
                let obj = obj.downgrade();
                let weak_row = weak_row.clone();
                glib::MainContext::default().spawn_local(async move {
                    let result = registration_job(profile, clear, |ctx| {
                        delegation_config::undo(&ctx.home, &ctx.state)
                    })
                    .await
                    .and_then(|undone| undone);
                    let text = match result {
                        Ok(()) => "Registration restored. Reopen enabled threads to use it.".into(),
                        Err(error) => error,
                    };
                    if let Some(row) = weak_row.upgrade() {
                        row.set_subtitle(&text);
                    }
                    if let Some(obj) = obj.upgrade() {
                        obj.imp().show_toast(&text);
                    }
                });
            }
        ));
        let undo_row = adw::ActionRow::builder()
            .title("Restore previous registration")
            .subtitle("Refuses to overwrite settings edited after the last change.")
            .build();
        undo_row.add_suffix(&undo);
        undo_row.set_activatable_widget(Some(&undo));
        group.add(&undo_row);
        (group, switches)
    }

    fn preview_delegation_registration(
        &self,
        action: Option<Action>,
        dialog: glib::WeakRef<adw::PreferencesDialog>,
        row: glib::WeakRef<adw::ActionRow>,
    ) {
        let Some(action) = action else {
            self.test_delegation_registration(row);
            return;
        };
        self.review_delegation_registration(action, dialog, row, |_| {});
    }

    /// The Test button: the strict registration check plus the headless bridge handshake.
    fn test_delegation_registration(&self, row: glib::WeakRef<adw::ActionRow>) {
        let profile = self.agent_profile_now(Driver::Agy);
        let profile_name = profile.name.clone();
        let clear = self.config.borrow().clear_env.clone();
        if let Some(row) = row.upgrade() {
            row.set_subtitle("Checking the selected profile's registration…");
        }
        glib::MainContext::default().spawn_local(async move {
            let result = registration_job(profile, clear, |ctx| {
                let preview = delegation_config::preview(
                    &ctx.home,
                    &ctx.state,
                    &ctx.command,
                    Action::Configure,
                );
                let diagnostic = delegation_config::bridge_diagnostic(
                    &ctx.command,
                    &ctx.launch.env,
                    &ctx.launch.unset,
                );
                (preview, diagnostic)
            })
            .await;
            let text=match result {
                Ok((preview,diagnostic))=>{
                    let registration=registration_status(&preview);
                    let shim=match diagnostic {Ok(())=>"Headless bridge handshake verified in this profile's environment; unbound state expected.".to_owned(),Err(error)=>format!("Headless bridge diagnostic failed: {error}")};
                    format!("Profile: {profile_name}\n{registration}\n{shim}\nLive authenticated thread binding is not tested by this check; open an enabled thread to establish it.")
                }
                Err(error)=>format!("Could not verify this profile's environment: {error}"),
            };
            if let Some(row) = row.upgrade() { row.set_subtitle(&text); }
        });
    }

    /// Fills the registration row with the strict check's verdict, without the bridge
    /// handshake (that starts a process; the Test button runs it).
    pub(super) fn check_delegation_registration(&self, row: glib::WeakRef<adw::ActionRow>) {
        let profile = self.agent_profile_now(Driver::Agy);
        let clear = self.config.borrow().clear_env.clone();
        glib::MainContext::default().spawn_local(async move {
            let text = registration_job(profile, clear, |ctx| {
                registration_status(&delegation_config::preview(
                    &ctx.home,
                    &ctx.state,
                    &ctx.command,
                    Action::Configure,
                ))
            })
            .await
            .unwrap_or_else(|error| error);
            if let Some(row) = row.upgrade() {
                row.set_subtitle(&text);
            }
        });
    }

    /// Previews `action` on the Antigravity profile's registration, listing any permission
    /// repairs, and applies it only from its review dialog. `done` gets the outcome once:
    /// `Ready` when nothing needed changing or the change was applied.
    pub(super) fn review_delegation_registration(
        &self,
        action: Action,
        dialog: glib::WeakRef<adw::PreferencesDialog>,
        row: glib::WeakRef<adw::ActionRow>,
        done: impl FnOnce(Review) + 'static,
    ) {
        let profile = self.agent_profile_now(Driver::Agy);
        let profile_name = profile.name.clone();
        let clear = self.config.borrow().clear_env.clone();
        let weak = self.obj().downgrade();
        if let Some(row) = row.upgrade() {
            row.set_subtitle("Checking the selected profile's registration…");
        }
        glib::MainContext::default().spawn_local(async move {
            let result = registration_job(profile, clear, move |ctx| {
                delegation_config::preview_repairing(&ctx.home, &ctx.state, &ctx.command, action)
            })
            .await
            .and_then(|plan| plan);
            let finish = |row: &glib::WeakRef<adw::ActionRow>, text: &str| {
                if let Some(row) = row.upgrade() {
                    row.set_subtitle(text);
                }
            };
            let plan = match result {
                Ok(plan) if !plan.changed() => {
                    let text = match action {
                        Action::Configure => registration_status(&Ok(plan)),
                        Action::Remove => "No managed registration to remove.".to_owned(),
                    };
                    finish(&row, &text);
                    done(Review::Ready);
                    return;
                }
                Ok(plan) => plan,
                Err(error) => {
                    let text = format!("Could not verify: {error}");
                    finish(&row, &text);
                    done(Review::Failed(text));
                    return;
                }
            };
            let (Some(obj), Some(parent)) = (weak.upgrade(), dialog.upgrade()) else {
                done(Review::Cancelled);
                return;
            };
            let (confirm, answer) = review_dialog(&plan, &profile_name, action);
            confirm.present(Some(&parent));
            if answer.await != Ok(true) {
                finish(&row, "Registration left unchanged.");
                done(Review::Cancelled);
                return;
            }
            let result = blocking_or(
                move || delegation_config::apply(plan),
                Err("Registration worker failed".into()),
            )
            .await;
            let (text, review) = match result {
                Ok(()) => (
                    "Registration saved. Reopen enabled threads to use the new configuration."
                        .to_owned(),
                    Review::Ready,
                ),
                Err(error) => (error.clone(), Review::Failed(error)),
            };
            finish(&row, &text);
            obj.imp().show_toast(&text);
            done(review);
        });
    }
}

// `path` with the home folder written as `~`, for a narrow table column.
use agent_kit::paths::tilde;

/// The review of a registration plan: a Target | Change table, one row per file or folder in
/// the order Apply changes them, then Cancel and Apply. The receiver gets `true` only for
/// Apply; Cancel, Escape and closing the dialog all mean no.
fn review_dialog(
    plan: &delegation_config::Preview,
    profile_name: &str,
    action: Action,
) -> (adw::Dialog, tokio::sync::oneshot::Receiver<bool>) {
    let title = match action {
        Action::Configure => "Set Up Antigravity Delegation",
        Action::Remove => "Remove Antigravity Delegation",
    };
    let dialog = adw::Dialog::builder()
        .title(title)
        .content_width(680)
        .build();
    let intro = gtk4::Label::builder()
        .label(format!(
            "Profile “{profile_name}” · home {}\nApply makes these changes, in this order. \
             Nothing changes until you choose Apply. A folder or file others can write to is \
             unsafe for a registration, so its permission is narrowed first.",
            plan.home().display()
        ))
        .wrap(true)
        .xalign(0.0)
        .css_classes(["dim-label"])
        .build();

    let grid = gtk4::Grid::builder()
        .column_spacing(18)
        .row_spacing(10)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    for (column, heading) in ["Target", "Change"].into_iter().enumerate() {
        let label = gtk4::Label::builder()
            .label(heading)
            .xalign(0.0)
            .css_classes(["heading"])
            .build();
        grid.attach(&label, column as i32, 0, 1, 1);
    }
    for (i, change) in plan.changes().iter().enumerate() {
        let row = 2 * i as i32 + 1;
        grid.attach(
            &gtk4::Separator::new(gtk4::Orientation::Horizontal),
            0,
            row,
            2,
            1,
        );
        let target = gtk4::Label::builder()
            .label(tilde(&change.target, plan.home()))
            .tooltip_text(change.target.display().to_string())
            .xalign(0.0)
            .yalign(0.0)
            .wrap(true)
            .wrap_mode(gtk4::pango::WrapMode::Char)
            .max_width_chars(30)
            .selectable(true)
            .css_classes(["monospace"])
            .build();
        // A path broken across lines must not gain a hyphen that reads as part of its name.
        let no_hyphens = gtk4::pango::AttrList::new();
        no_hyphens.insert(gtk4::pango::AttrInt::new_insert_hyphens(false));
        target.set_attributes(Some(&no_hyphens));
        let what = gtk4::Label::builder()
            .label(&change.change)
            .xalign(0.0)
            .yalign(0.0)
            .wrap(true)
            .hexpand(true)
            .build();
        grid.attach(&target, 0, row + 1, 1, 1);
        grid.attach(&what, 1, row + 1, 1, 1);
    }
    let table = gtk4::Frame::builder().child(&grid).build();
    let scrolled = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(420)
        .child(&table)
        .build();

    let cancel = gtk4::Button::with_label("Cancel");
    let apply = gtk4::Button::builder()
        .label("Apply")
        .css_classes(["suggested-action"])
        .build();
    let buttons = gtk4::Box::builder()
        .spacing(12)
        .halign(gtk4::Align::End)
        .build();
    buttons.append(&cancel);
    buttons.append(&apply);

    let content = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(12)
        .margin_top(6)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();
    content.append(&intro);
    content.append(&scrolled);
    content.append(&buttons);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&content));
    dialog.set_child(Some(&view));
    dialog.set_default_widget(Some(&cancel));
    dialog.set_focus(Some(&cancel));

    let (send, answer) = tokio::sync::oneshot::channel();
    let send = Rc::new(RefCell::new(Some(send)));
    let reply = |send: &Rc<RefCell<Option<tokio::sync::oneshot::Sender<bool>>>>, yes: bool| {
        if let Some(send) = send.borrow_mut().take() {
            let _ = send.send(yes);
        }
    };
    for (button, yes) in [(&cancel, false), (&apply, true)] {
        let (send, dialog) = (send.clone(), dialog.downgrade());
        button.connect_clicked(move |_| {
            reply(&send, yes);
            if let Some(dialog) = dialog.upgrade() {
                dialog.close();
            }
        });
    }
    dialog.connect_closed(move |_| reply(&send, false));
    (dialog, answer)
}

/// How a registration review ended.
pub(super) enum Review {
    /// Nothing needed changing, or the reviewed change was applied.
    Ready,
    /// The user declined the change (or the dialog went away).
    Cancelled,
    /// The registration could not be checked or the change failed; the message says why.
    Failed(String),
}

fn registration_status(preview: &Result<delegation_config::Preview, String>) -> String {
    match preview {
        Ok(plan) if !plan.changed() => "Registration and exact allow rules verified.".to_owned(),
        Ok(_) => {
            "Registration or exact allow rules need setting up: Configure / Repair.".to_owned()
        }
        Err(error) => format!("Registration could not be verified: {error}"),
    }
}

/// What a registration job works on: the Antigravity profile's verified home, the app's
/// delegation state under it, this executable, and the profile's launch environment.
struct RegistrationCtx {
    home: PathBuf,
    state: PathBuf,
    command: PathBuf,
    launch: crate::agent_proc::AgentEnv,
}

/// Resolves `profile`'s [`RegistrationCtx`], then runs `job` on it off the main thread. A
/// context that cannot be resolved is an `Err` saying why; a job that panics, "Registration
/// worker failed".
async fn registration_job<T: Send + 'static>(
    profile: Profile,
    clear: Vec<String>,
    job: impl FnOnce(RegistrationCtx) -> T + Send + 'static,
) -> Result<T, String> {
    let ctx = registration_context(profile, clear).await?;
    gtk4::gio::spawn_blocking(move || job(ctx))
        .await
        .map_err(|_| "Registration worker failed".to_owned())
}

async fn registration_context(
    profile: Profile,
    clear: Vec<String>,
) -> Result<RegistrationCtx, String> {
    let file = profile.env_file.clone();
    let patterns = clear.clone();
    // With the patterns, as the agent's own launch reads it (`resolve_agent`).
    let env =
        gtk4::gio::spawn_blocking(move || match file.filter(|path| !path.trim().is_empty()) {
            Some(path) => crate::utils::load_env_file_for_profile(&path, &patterns),
            None => Ok(Vec::new()),
        })
        .await
        .map_err(|_| "Could not resolve this profile's environment".to_owned())??;
    let launch = super::super::threads::agent_env(env, &clear);
    if agent_core::delegation::agy_guard::argv_redirect(&profile.argv()).is_some()
        || agent_core::delegation::agy_guard::env_redirect(&crate::agent_home::child_env(&launch))
            .is_some()
    {
        return Err("The selected profile redirects Antigravity configuration; managed registration is unavailable.".into());
    }
    let home = crate::agent_home::effective_home(&launch)
        .ok_or("The selected profile has no verified home")?;
    let state = home.join(".config/agent-terminal/delegation");
    let command = std::env::current_exe()
        .map_err(|_| "Cannot locate the installed Agent Terminal executable")?;
    Ok(RegistrationCtx {
        home,
        state,
        command,
        launch,
    })
}

#[cfg(test)]
mod tests {
    use super::{master_state, UnitState};

    fn u(name: &str, disabled: bool, available: bool) -> UnitState {
        (name.to_owned(), disabled, available)
    }

    const CLAUDE: &str = "Claude";
    const CODEX: &str = "Codex";
    const AGY: &str = "Antigravity";

    #[test]
    fn the_master_switch_is_on_only_when_every_enabled_agent_is() {
        // (name, disabled, available)
        assert!(
            master_state(&[
                u(CLAUDE, false, true),
                u(CODEX, false, true),
                u(AGY, false, true)
            ])
            .0
        );
        assert!(
            !master_state(&[
                u(CLAUDE, false, true),
                u(CODEX, false, true),
                u(AGY, false, false)
            ])
            .0
        );
        // A disabled agent neither holds it off nor counts for it.
        assert!(
            master_state(&[
                u(CLAUDE, false, true),
                u(CODEX, false, true),
                u(AGY, true, false)
            ])
            .0
        );
        // Nothing enabled: nothing to turn on.
        assert!(
            !master_state(&[
                u(CLAUDE, true, true),
                u(CODEX, true, false),
                u(AGY, true, false)
            ])
            .0
        );
    }

    #[test]
    fn a_mixed_state_names_who_is_on_and_off_and_the_codex_caveat_always_shows() {
        let (on, text) = master_state(&[
            u(CLAUDE, false, true),
            u(CODEX, false, true),
            u(AGY, false, false),
        ]);
        assert!(!on);
        assert!(
            text.starts_with("Custom: on for Claude, Codex; off for Antigravity."),
            "{text}"
        );
        let caveat = agent_core::delegation::route::RouteCaveat::CodexNativeWorkersUnsupervised;
        for agents in [
            [
                u(CLAUDE, false, true),
                u(CODEX, false, true),
                u(AGY, false, true),
            ],
            [
                u(CLAUDE, false, false),
                u(CODEX, false, false),
                u(AGY, false, false),
            ],
        ] {
            let (_, text) = master_state(&agents);
            assert!(
                !text.starts_with("Custom"),
                "all on or all off is not custom: {text}"
            );
            assert!(text.contains(caveat.explain()));
        }
    }

    /// Local servers count one by one, by name: one server left off keeps the master off and is
    /// named, a switched-off server counts for nothing, and any number of servers can be added.
    #[test]
    fn local_servers_count_per_profile_not_per_agent() {
        let vendors = [
            u(CLAUDE, false, true),
            u(CODEX, false, true),
            u(AGY, false, true),
        ];
        let with = |servers: &[UnitState]| {
            let mut all = vendors.to_vec();
            all.extend_from_slice(servers);
            master_state(&all)
        };
        assert!(with(&[]).0);
        assert!(with(&[u("Ollama", false, true), u("Big card", false, true)]).0);
        let (on, text) = with(&[u("Ollama", false, true), u("Big card", false, false)]);
        assert!(!on, "one server off is not all on");
        assert!(
            text.starts_with(
                "Custom: on for Claude, Codex, Antigravity, Ollama; off for Big card."
            ),
            "{text}"
        );
        // A server switched off in Settings neither holds the master off nor counts for it.
        assert!(with(&[u("Ollama", false, true), u("Spare", true, false)]).0);
        // A newly added server is off: the master shows it as not yet included.
        assert!(!with(&[u("Ollama 2", false, false)]).0);
        // Only servers, none on: nothing custom.
        let (on, text) = master_state(&[u("A", false, false), u("B", false, false)]);
        assert!(!on && !text.starts_with("Custom"), "{text}");
        let (on, _) = master_state(&[u("A", false, true), u("B", false, true)]);
        assert!(on);
    }
}

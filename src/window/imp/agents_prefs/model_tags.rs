//! Settings → Agents → Model Tags: every model of every enabled profile with the tier, tags and
//! best-for note delegation sees, and where they come from. An edit writes an entry for that one
//! model to the user's own tag file (never the shared one, which another tool may overwrite);
//! Reset removes it. A Local server's model is tagged for that server alone unless "Apply to
//! every server" is on, because one model can suit a large card and a small one differently.
//!
//! Every change applies as it is made; file I/O runs off the main thread, and a value that does
//! not validate is refused where it is typed and never written.

use std::cell::{Cell, RefCell};

use agent_core::model_tags::{TagSource, MAX_BEST_FOR_BYTES};
use agent_core::user_tags::{Edit, TIERS};
use gtk4::gio;

use super::*;
use crate::model_catalog::ServerSource;
use crate::model_tags_file::load_now;
use crate::tag_edit::{self, Effective, Target};

const GROUP_HELP: &str = "The tier, tags and best-for note other agents see for each model when \
they choose a sub-agent. What you set here is saved in your own tag file and wins over the shared \
tag file and the built-in tags; Reset puts the model back under them. Agents cannot change these.";
const BEST_FOR_HELP: &str = "A short note on what this model is best for";
const EVERY_SERVER_HELP: &str = "On: the same model on any Local server gets these tags. Off: \
only this server's copy does (the same model can suit a large and a small card differently).";

/// The text under a model's name: its tier, tags and where they come from.
pub(super) fn summary(effective: Option<&Effective>) -> String {
    let Some(e) = effective else {
        return "No tags".to_owned();
    };
    let source = match (e.source, e.scoped) {
        (TagSource::User, true) => "yours, this server",
        (TagSource::User, false) => "yours",
        (TagSource::File, _) => "shared file",
        (TagSource::BuiltIn, _) => "built in",
    };
    let tags = if e.tags.is_empty() {
        "no tags".to_owned()
    } else {
        e.tags.join(", ")
    };
    format!(
        "{} · {tags} · {source}",
        e.tier.as_deref().unwrap_or("no tier")
    )
}

/// Why `text` cannot be a best-for note.
pub(super) fn best_for_problem(text: &str) -> Option<String> {
    (text.trim().len() > MAX_BEST_FOR_BYTES).then(|| format!("At most {MAX_BEST_FOR_BYTES} bytes"))
}

/// The profile an edit of a model of `driver` is scoped to: a Local server's own, unless the
/// edit is for every server; a vendor agent's models are never scoped.
pub(super) fn scope_profile(
    driver: Driver,
    every_server: bool,
    profile_id: &str,
) -> Option<String> {
    (driver.is_built_in() && !every_server).then(|| profile_id.to_owned())
}

/// A model's identity for a write, owned so a worker can hold it.
#[derive(Clone)]
struct Owned {
    id: String,
    driver: Driver,
    profile: Option<String>,
}

impl Owned {
    fn target(&self) -> Target<'_> {
        Target {
            id: &self.id,
            driver: Some(self.driver),
            profile: self.profile.as_deref(),
        }
    }
}

/// One model's row and what it shows.
struct ModelRow {
    window: glib::WeakRef<crate::window::AgentTerminalWindow>,
    id: String,
    driver: Driver,
    profile_id: String,
    every: Cell<bool>,
    /// Set while code moves a widget: its handler then does nothing.
    quiet: Cell<bool>,
    saved_best_for: RefCell<String>,
    row: adw::ExpanderRow,
    tier: adw::ComboRow,
    toggles: Vec<(String, gtk4::ToggleButton)>,
    best_for: adw::EntryRow,
}

impl ModelRow {
    fn owned(&self) -> Owned {
        Owned {
            id: self.id.clone(),
            driver: self.driver,
            profile: scope_profile(self.driver, self.every.get(), &self.profile_id),
        }
    }

    fn files(&self) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
        let window = self.window.upgrade()?;
        let config = window.imp().config.borrow();
        Some((config.model_tags_file(), config.user_model_tags_file()))
    }

    fn toast(&self, text: &str) {
        if let Some(window) = self.window.upgrade() {
            window.imp().show_toast(text);
        }
    }

    /// Shows `e` (the model's effective tags) in the widgets, without writing anything.
    fn show(&self, e: Option<&Effective>) {
        self.quiet.set(true);
        self.row.set_subtitle(&summary(e));
        let tier = e
            .and_then(|e| e.tier.as_deref())
            .and_then(|t| TIERS.iter().position(|x| *x == t))
            .map_or(0, |i| i as u32 + 1);
        self.tier.set_selected(tier);
        for (tag, button) in &self.toggles {
            button.set_active(e.is_some_and(|e| e.tags.contains(tag)));
        }
        let note = e.and_then(|e| e.best_for.clone()).unwrap_or_default();
        if self.best_for.text() != note {
            self.best_for.set_text(&note);
        }
        *self.saved_best_for.borrow_mut() = note;
        self.quiet.set(false);
    }

    /// Reads the model's effective tags again, off the main thread, and shows them.
    fn reload(self: &Rc<Self>) {
        let Some((file, user)) = self.files() else {
            return;
        };
        let (this, who) = (self.clone(), self.owned());
        // The display is the server's own view of the model, whichever scope an edit writes.
        let seen = Owned {
            profile: Some(self.profile_id.clone()),
            ..who
        };
        glib::MainContext::default().spawn_local(async move {
            let found = gio::spawn_blocking(move || {
                let tags = load_now(&file, &user).tags.unwrap_or_default();
                tag_edit::effective(&tags, seen.target())
            })
            .await;
            if let Ok(effective) = found {
                this.show(effective.as_ref());
            }
        });
    }

    /// Writes `edit` for this model, off the main thread; a refusal is said and the widgets go
    /// back to what is saved.
    fn write(self: &Rc<Self>, edit: Edit) {
        let Some((file, user)) = self.files() else {
            return;
        };
        let (this, who) = (self.clone(), self.owned());
        glib::MainContext::default().spawn_local(async move {
            let done = gio::spawn_blocking(move || {
                let tags = load_now(&file, &user).tags.unwrap_or_default();
                tag_edit::set(&user, &tags, who.target(), &edit).map(|_| ())
            })
            .await
            .unwrap_or_else(|_| Err("The tags could not be saved".into()));
            if let Err(why) = done {
                this.toast(&why);
            }
            this.reload();
        });
    }

    /// Removes this model's entry at the scope the row writes to.
    fn reset(self: &Rc<Self>) {
        let Some((_, user)) = self.files() else {
            return;
        };
        let (this, who) = (self.clone(), self.owned());
        glib::MainContext::default().spawn_local(async move {
            let done = gio::spawn_blocking(move || tag_edit::reset(&user, who.target()))
                .await
                .unwrap_or_else(|_| Err("The tags could not be reset".into()));
            match done {
                Ok(false) => this.toast("You have set nothing for this model"),
                Err(why) => this.toast(&why),
                Ok(true) => {}
            }
            this.reload();
        });
    }

    /// Moves the user's entry between "this server" and "every server": written at the new
    /// scope, then removed from the old one.
    fn rescope(self: &Rc<Self>, every: bool) {
        let before = self.owned();
        self.every.set(every);
        let after = self.owned();
        let Some((file, user)) = self.files() else {
            return;
        };
        let this = self.clone();
        glib::MainContext::default().spawn_local(async move {
            let moved = gio::spawn_blocking(move || -> Result<(), String> {
                let tags = load_now(&file, &user).tags.unwrap_or_default();
                let Some(entry) = agent_kit::user_tags_file::load(&user).ok().and_then(|doc| {
                    agent_core::user_tags::find(
                        &doc,
                        &before.id,
                        agent_core::user_tags::Scope {
                            driver: Some(before.driver),
                            profile: before.profile.as_deref(),
                        },
                    )
                }) else {
                    return Ok(());
                };
                let edit = Edit {
                    tier: entry.tier,
                    tags: Some(entry.tags),
                    best_for: entry.best_for,
                };
                tag_edit::set(&user, &tags, after.target(), &edit)?;
                tag_edit::reset(&user, before.target()).map(|_| ())
            })
            .await
            .unwrap_or_else(|_| Err("The tags could not be moved".into()));
            if let Err(why) = moved {
                this.toast(&why);
            }
            this.reload();
        });
    }
}

impl AgentTerminalWindow {
    /// The Model Tags group: a profile per expander, a model per row inside.
    pub(super) fn model_tags_group(
        &self,
        dialog: &adw::PreferencesDialog,
    ) -> adw::PreferencesGroup {
        let obj = self.obj();
        let refresh = gtk4::Button::builder()
            .icon_name("at-view-refresh-symbolic")
            .tooltip_text("List the models again")
            .valign(gtk4::Align::Center)
            .css_classes(["flat"])
            .build();
        let group = adw::PreferencesGroup::builder()
            .title("Model Tags")
            .description(GROUP_HELP)
            .header_suffix(&refresh)
            .build();
        let rows: Rc<RefCell<Vec<gtk4::Widget>>> = Rc::default();
        let generation = Rc::new(crate::window::Generation::new());
        let fill: Rc<dyn Fn()> = Rc::new(glib::clone!(
            #[weak]
            obj,
            #[weak]
            group,
            #[weak]
            dialog,
            #[strong]
            rows,
            #[strong]
            generation,
            move || {
                let current = generation.next();
                let imp = obj.imp();
                let (file, user) = {
                    let config = imp.config.borrow();
                    (config.model_tags_file(), config.user_model_tags_file())
                };
                let listed = imp.model_tag_profiles();
                let (generation, rows) = (generation.clone(), rows.clone());
                glib::MainContext::default().spawn_local(glib::clone!(
                    #[weak]
                    obj,
                    #[weak]
                    group,
                    #[weak]
                    dialog,
                    async move {
                        let loaded = gio::spawn_blocking(move || load_now(&file, &user)).await;
                        if !generation.is_current(current) {
                            return;
                        }
                        let tags = loaded.ok().and_then(|s| s.tags).unwrap_or_default();
                        for row in rows.borrow_mut().drain(..) {
                            group.remove(&row);
                        }
                        let timer = obj.imp().agent_edit_timer(&dialog);
                        if listed.iter().all(|(_, models)| models.is_empty()) {
                            let empty = adw::ActionRow::builder()
                                .title("No models listed yet")
                                .subtitle(
                                    "Models appear once an agent or server has listed them; \
                                     use the refresh button after a scan",
                                )
                                .build();
                            group.add(&empty);
                            rows.borrow_mut().push(empty.upcast());
                        }
                        for (profile, models) in &listed {
                            if models.is_empty() {
                                continue;
                            }
                            let expander = obj
                                .imp()
                                .model_tag_profile_row(profile, models, &tags, &timer, &dialog);
                            group.add(&expander);
                            rows.borrow_mut().push(expander.upcast());
                        }
                    }
                ));
            }
        ));
        fill();
        refresh.connect_clicked({
            let fill = fill.clone();
            move |_| fill()
        });
        group
    }

    /// The enabled profiles that run a chat agent, each with the models its server or vendor
    /// lists now.
    fn model_tag_profiles(&self) -> Vec<(Profile, Vec<agent_core::catalog::CatalogModel>)> {
        let catalog = ModelCatalog::shared();
        self.config
            .borrow()
            .profiles
            .iter()
            .filter(|p| !p.disabled)
            .filter_map(|p| {
                let driver = crate::config::profile_driver(p)?;
                let source = ServerSource::of(p, driver);
                Some((p.clone(), catalog.models_for(driver, source.as_ref())))
            })
            .collect()
    }

    fn model_tag_profile_row(
        &self,
        profile: &Profile,
        models: &[agent_core::catalog::CatalogModel],
        tags: &agent_core::model_tags::ModelTags,
        timer: &Rc<EditDebounce>,
        _dialog: &adw::PreferencesDialog,
    ) -> adw::ExpanderRow {
        let driver = crate::config::profile_driver(profile).unwrap_or(Driver::Claude);
        let expander = adw::ExpanderRow::builder()
            .title(&profile.name)
            .subtitle(format!(
                "{} · {} model{}",
                driver.info().long_label,
                models.len(),
                if models.len() == 1 { "" } else { "s" }
            ))
            .use_markup(false)
            .build();
        for model in models {
            let row = self.model_tag_row(profile, driver, model, tags, timer);
            expander.add_row(&row.row);
        }
        expander
    }

    fn model_tag_row(
        &self,
        profile: &Profile,
        driver: Driver,
        model: &agent_core::catalog::CatalogModel,
        tags: &agent_core::model_tags::ModelTags,
        timer: &Rc<EditDebounce>,
    ) -> Rc<ModelRow> {
        let seen = tag_edit::effective(
            tags,
            Target {
                id: &model.id,
                driver: Some(driver),
                profile: Some(&profile.id),
            },
        );
        let row = adw::ExpanderRow::builder()
            .title(&model.id)
            .use_markup(false)
            .build();

        let mut choices = vec!["No tier"];
        choices.extend(TIERS);
        let tier = adw::ComboRow::builder()
            .title("Tier")
            .subtitle("fast, standard or deep: how much model the task needs")
            .model(&gtk4::StringList::new(&choices))
            .build();
        row.add_row(&tier);

        let flow = gtk4::FlowBox::builder()
            .selection_mode(gtk4::SelectionMode::None)
            .homogeneous(false)
            .min_children_per_line(1)
            .max_children_per_line(6)
            .row_spacing(6)
            .column_spacing(6)
            .margin_top(8)
            .margin_bottom(8)
            .margin_start(12)
            .margin_end(12)
            .build();
        let toggles: Vec<(String, gtk4::ToggleButton)> = tags
            .vocabulary
            .iter()
            .map(|(tag, meaning)| {
                let button = gtk4::ToggleButton::builder()
                    .label(tag)
                    .tooltip_text(meaning)
                    .build();
                flow.append(&button);
                (tag.clone(), button)
            })
            .collect();
        let tags_row = adw::PreferencesRow::builder().title("Tags").build();
        let tags_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        tags_box.append(
            &gtk4::Label::builder()
                .label("Tags")
                .xalign(0.0)
                .margin_top(8)
                .margin_start(12)
                .css_classes(["heading"])
                .build(),
        );
        tags_box.append(&flow);
        tags_row.set_child(Some(&tags_box));
        row.add_row(&tags_row);

        let (best_for, best_error) = validated_entry("Best For", BEST_FOR_HELP, "");
        row.add_row(&best_for);
        row.add_row(&best_error);

        let state = Rc::new(ModelRow {
            window: self.obj().downgrade(),
            id: model.id.clone(),
            driver,
            profile_id: profile.id.clone(),
            every: Cell::new(
                seen.as_ref()
                    .is_some_and(|e| e.source == TagSource::User && !e.scoped),
            ),
            quiet: Cell::new(false),
            saved_best_for: RefCell::default(),
            row: row.clone(),
            tier: tier.clone(),
            toggles,
            best_for: best_for.clone(),
        });
        state.show(seen.as_ref());

        tier.connect_selected_notify(glib::clone!(
            #[strong]
            state,
            move |r| {
                if state.quiet.get() {
                    return;
                }
                match (r.selected() as usize)
                    .checked_sub(1)
                    .and_then(|i| TIERS.get(i))
                {
                    Some(t) => state.write(Edit {
                        tier: Some((*t).to_owned()),
                        ..Edit::default()
                    }),
                    // "No tier" is not a choice to write: Reset clears what you set.
                    None => state.reload(),
                }
            }
        ));
        for (_, button) in &state.toggles {
            button.connect_toggled(glib::clone!(
                #[strong]
                state,
                move |_| {
                    if state.quiet.get() {
                        return;
                    }
                    let chosen: Vec<String> = state
                        .toggles
                        .iter()
                        .filter(|(_, b)| b.is_active())
                        .map(|(t, _)| t.clone())
                        .collect();
                    state.write(Edit {
                        tags: Some(chosen),
                        ..Edit::default()
                    });
                }
            ));
        }
        let (decide_state, apply_state) = (state.clone(), state.clone());
        self.commit_only_entry(
            &best_for,
            &best_error,
            BEST_FOR_HELP,
            timer,
            move |_, text, commit| {
                let saved = decide_state.saved_best_for.borrow().clone();
                let parsed = match best_for_problem(text) {
                    Some(problem) => Err(problem),
                    None => Ok(text.trim().to_owned()),
                };
                crate::config::field_change(parsed, &saved, commit)
            },
            move |_, value: Option<String>| {
                if let Some(note) = value {
                    apply_state.write(Edit {
                        best_for: Some(note),
                        ..Edit::default()
                    });
                }
            },
        );

        if driver.is_built_in() {
            let every = adw::SwitchRow::builder()
                .title("Apply to Every Server")
                .subtitle(EVERY_SERVER_HELP)
                .subtitle_lines(0)
                .active(state.every.get())
                .build();
            every.connect_active_notify(glib::clone!(
                #[strong]
                state,
                move |r| {
                    if r.is_active() != state.every.get() {
                        state.rescope(r.is_active());
                    }
                }
            ));
            row.add_row(&every);
        }

        let reset = gtk4::Button::builder()
            .label("Reset")
            .valign(gtk4::Align::Center)
            .build();
        let reset_row = adw::ActionRow::builder()
            .title("Reset")
            .subtitle("Remove what you set for this model; the shared and built-in tags apply")
            .build();
        reset_row.add_suffix(&reset);
        reset.connect_clicked(glib::clone!(
            #[strong]
            state,
            move |_| state.reset()
        ));
        row.add_row(&reset_row);
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn effective(source: TagSource, scoped: bool) -> Effective {
        Effective {
            tier: Some("fast".into()),
            tags: vec!["code".into(), "bulk".into()],
            best_for: None,
            source,
            scoped,
        }
    }

    #[test]
    fn a_model_row_says_its_tier_tags_and_where_they_came_from() {
        assert_eq!(summary(None), "No tags");
        assert_eq!(
            summary(Some(&effective(TagSource::BuiltIn, false))),
            "fast · code, bulk · built in"
        );
        assert_eq!(
            summary(Some(&effective(TagSource::File, false))),
            "fast · code, bulk · shared file"
        );
        assert_eq!(
            summary(Some(&effective(TagSource::User, true))),
            "fast · code, bulk · yours, this server"
        );
        assert_eq!(
            summary(Some(&effective(TagSource::User, false))),
            "fast · code, bulk · yours"
        );
        let bare = Effective {
            tier: None,
            tags: vec![],
            ..effective(TagSource::User, false)
        };
        assert_eq!(summary(Some(&bare)), "no tier · no tags · yours");
    }

    #[test]
    fn a_best_for_note_over_the_bound_is_refused_where_it_is_typed() {
        assert_eq!(best_for_problem(""), None);
        assert_eq!(best_for_problem(&"x".repeat(MAX_BEST_FOR_BYTES)), None);
        assert!(best_for_problem(&"x".repeat(MAX_BEST_FOR_BYTES + 1)).is_some());
        // Surrounding space does not count: it is trimmed before it is saved.
        assert_eq!(
            best_for_problem(&format!("  {}  ", "x".repeat(MAX_BEST_FOR_BYTES))),
            None
        );
    }

    #[test]
    fn only_a_local_servers_model_is_scoped_to_its_profile() {
        assert_eq!(
            scope_profile(Driver::Local, false, "p-small").as_deref(),
            Some("p-small")
        );
        assert_eq!(scope_profile(Driver::Local, true, "p-small"), None);
        for vendor in [Driver::Claude, Driver::Codex, Driver::Agy] {
            assert_eq!(scope_profile(vendor, false, "p-claude"), None);
        }
    }
}
